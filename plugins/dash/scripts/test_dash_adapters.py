# plugins/dash/scripts/test_dash_adapters.py
import json
import os
import subprocess
import tempfile
import time
import unittest
from datetime import datetime, timedelta
from pathlib import Path

from dashlib.adapters import load_git, load_sdd, load_session
from dashlib.model import Fragment


def write_events(tmp: Path, events):
    p = tmp / "state.jsonl"
    p.write_text("".join(json.dumps(e, ensure_ascii=False) + "\n" for e in events),
                 encoding="utf-8")
    return p


NOW = "2026-09-13T15:00:00+08:00"


def sh(cmd, cwd):
    subprocess.run(cmd, shell=True, cwd=cwd, check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


class TestSessionAdapter(unittest.TestCase):
    def test_todos_become_tasks(self):
        with tempfile.TemporaryDirectory() as d:
            p = write_events(Path(d), [
                {"ts": "2026-09-13T14:50:00+08:00", "kind": "todo", "event": "updated",
                 "session": "s1", "summary": "修面板;写测试"},
                {"ts": "2026-09-13T14:55:00+08:00", "kind": "todo", "event": "updated",
                 "session": "s1", "summary": "写测试"},
            ])
            frag = load_session(p, NOW)
            self.assertEqual(frag.source, "session")
            by_id = {t.id: t for t in frag.tasks}
            self.assertEqual(by_id["todo-s1-0"].label, "修面板")
            self.assertEqual(by_id["todo-s1-0"].state, "done")   # 最新快照无此标签→done
            self.assertEqual(by_id["todo-s1-1"].label, "写测试")
            self.assertEqual(by_id["todo-s1-1"].state, "active")
            self.assertEqual(by_id["todo-s1-0"].since, "2026-09-13T14:50:00+08:00")
            self.assertEqual(by_id["todo-s1-1"].since, "2026-09-13T14:50:00+08:00")

    def test_turn_end_drops_session_agents(self):
        # R19:前台 agent 不活过回合——turn_end 后该会话残余 agent 清落,activity 为空
        with tempfile.TemporaryDirectory() as d:
            p = write_events(Path(d), [
                {"ts": "2026-09-13T14:30:00+08:00", "kind": "agent", "event": "spawned",
                 "session": "s1", "summary": "探索写集"},
                {"ts": "2026-09-13T14:58:00+08:00", "kind": "stop", "event": "turn_end",
                 "session": "s1", "summary": ""},
            ])
            frag = load_session(p, NOW)
            self.assertEqual(frag.activity, [])

    def test_turn_end_scoped_to_own_session(self):
        # R19:清落只针对当事会话——s1 的 turn_end 不得带走 s2 的在跑 agent
        with tempfile.TemporaryDirectory() as d:
            p = write_events(Path(d), [
                {"ts": "2026-09-13T14:30:00+08:00", "kind": "agent", "event": "spawned",
                 "session": "s1", "summary": "A"},
                {"ts": "2026-09-13T14:31:00+08:00", "kind": "agent", "event": "spawned",
                 "session": "s2", "summary": "B"},
                {"ts": "2026-09-13T14:58:00+08:00", "kind": "stop", "event": "turn_end",
                 "session": "s1", "summary": ""},
            ])
            frag = load_session(p, NOW)
            self.assertEqual([a.label for a in frag.activity], ["B"])
            self.assertEqual(frag.activity[0].last_event, "")   # s2 未被 s1 的 turn_end 波及

    def test_completed_agent_dropped(self):
        with tempfile.TemporaryDirectory() as d:
            p = write_events(Path(d), [
                {"ts": "2026-09-13T14:00:00+08:00", "kind": "agent", "event": "spawned",
                 "session": "s1", "summary": "跑测试"},
                {"ts": "2026-09-13T14:10:00+08:00", "kind": "agent", "event": "completed",
                 "session": "s1", "summary": "跑测试"},
            ])
            self.assertEqual(load_session(p, NOW).activity, [])

    def test_agent_completed_empty_summary_pops_oldest(self):
        with tempfile.TemporaryDirectory() as d:
            p = write_events(Path(d), [
                {"ts": "2026-09-13T14:00:00+08:00", "kind": "agent", "event": "spawned",
                 "session": "s1", "summary": "A"},
                {"ts": "2026-09-13T14:05:00+08:00", "kind": "agent", "event": "spawned",
                 "session": "s1", "summary": "B"},
                {"ts": "2026-09-13T14:10:00+08:00", "kind": "agent", "event": "completed",
                 "session": "s1", "summary": ""},
            ])
            frag = load_session(p, NOW)
            self.assertEqual([a.label for a in frag.activity], ["B"])  # FIFO 弹出 A

    def test_empty_todo_snapshot_completes_all(self):
        with tempfile.TemporaryDirectory() as d:
            p = write_events(Path(d), [
                {"ts": "2026-09-13T14:00:00+08:00", "kind": "todo", "event": "updated",
                 "session": "s1", "summary": "甲;乙"},
                {"ts": "2026-09-13T14:20:00+08:00", "kind": "todo", "event": "updated",
                 "session": "s1", "summary": ""},
            ])
            frag = load_session(p, NOW)
            self.assertEqual([t.state for t in frag.tasks], ["done", "done"])
            self.assertEqual({t.since for t in frag.tasks}, {"2026-09-13T14:00:00+08:00"})

    def test_corrupt_lines_skipped(self):
        with tempfile.TemporaryDirectory() as d:
            p = Path(d) / "state.jsonl"
            p.write_text('{"ts":"2026-09-13T14:00:00+08:00","kind":"agent","event":"spawned",'
                         '"session":"s1","summary":"a"}\n{{{{corrupt\n', encoding="utf-8")
            self.assertEqual(len(load_session(p, NOW).activity), 1)

    def test_missing_file_empty_fragment(self):
        frag = load_session(Path("/nonexistent/state.jsonl"), NOW)
        self.assertEqual((frag.tasks, frag.activity, frag.warnings), ([], [], []))

    def test_perf_10k_events_under_100ms(self):
        with tempfile.TemporaryDirectory() as d:
            events = [{"ts": "2026-09-13T10:00:00+08:00", "kind": "stop",
                       "event": "turn_end", "session": f"s{i % 50}", "summary": ""} for i in range(10_000)]
            p = write_events(Path(d), events)
            t0 = time.perf_counter()
            load_session(p, NOW)
            self.assertLess(time.perf_counter() - t0, 0.1)


class TestGitAdapter(unittest.TestCase):
    def test_synthetic_repo(self):
        with tempfile.TemporaryDirectory() as d:
            sh("git init -q && git config user.email t@t && git config user.name t", d)
            (Path(d) / "a.txt").write_text("1")
            sh("git add -A && git commit -qm 'c1'", d)
            sh("git tag v1.0.0", d)
            (Path(d) / "a.txt").write_text("2")
            sh("git add -A && git commit -qm 'c2'", d)
            frag = load_git(Path(d), NOW)
            self.assertEqual(frag.source, "git")
            self.assertEqual(frag.velocity["commits_7d"], 2)
            self.assertEqual(frag.velocity["dirty_files"], 0)
            self.assertEqual([m.id for m in frag.milestones], ["v1.0.0"])
            self.assertEqual(frag.milestones[0].state, "done")

    def test_dirty_files_counted(self):
        with tempfile.TemporaryDirectory() as d:
            sh("git init -q && git config user.email t@t && git config user.name t", d)
            (Path(d) / "a.txt").write_text("1")
            (Path(d) / "b.txt").write_text("2")
            frag = load_git(Path(d), NOW)
            self.assertEqual(frag.velocity["dirty_files"], 2)

    def test_non_repo_degrades(self):
        with tempfile.TemporaryDirectory() as d:
            frag = load_git(Path(d), NOW)
            self.assertEqual(frag.velocity, {})
            self.assertEqual(frag.warnings, ["git 源不可用"])


def make_workspace(tmp: Path, wave="W26", done_all=True):
    ws = tmp / ".superpowers" / "sdd" / f"2026-09-13-{wave}"
    ws.mkdir(parents=True)
    (ws / "dag.json").write_text(json.dumps({
        "wave": wave, "title": "统一恢复面",
        "lanes": [{"id": "a", "name": "semantic-ledger", "tasks": [1]},
                  {"id": "b", "name": "worker-dual", "tasks": [6]}],
        "tasks": {"1": {"label": "schema v42 表组"}, "6": {"label": "cycle 拆分双域"},
                  "9": {"label": "全仓验证门"}},
        "barriers": [{"id": "1", "after": [6], "unlocks": [9]}],
    }, ensure_ascii=False), encoding="utf-8")
    ledger = "# ledger\n"
    ledger += "Task 1: complete (commits aaa..bbb, review clean)\n"
    ledger += ("Task 6: complete\n" if done_all else "Task 6: dispatched\n")
    ledger += "Task 9: complete\n" if done_all else ""
    (ws / "progress.md").write_text(ledger, encoding="utf-8")
    return ws


class TestSddAdapter(unittest.TestCase):
    def test_states_and_lanes(self):
        with tempfile.TemporaryDirectory() as d:
            make_workspace(Path(d), done_all=False)
            frag, barriers = load_sdd(Path(d), NOW)
            by_id = {t.id: t for t in frag.tasks}
            self.assertEqual(by_id["T1"].state, "done")
            self.assertEqual(by_id["T1"].lane, "semantic-ledger")
            self.assertEqual(by_id["T6"].state, "active")
            self.assertIn("dispatched", by_id["T6"].note)
            self.assertEqual(by_id["T9"].lane, "无车道")
            self.assertEqual(by_id["T9"].state, "pending")
            self.assertEqual(barriers, ["屏障 1: T6 → T9"])
            self.assertEqual(frag.milestones[0].id, "W26")
            self.assertEqual(frag.milestones[0].state, "active")

    def test_all_done_milestone(self):
        with tempfile.TemporaryDirectory() as d:
            make_workspace(Path(d), done_all=True)
            frag, _ = load_sdd(Path(d), NOW)
            self.assertEqual(frag.milestones[0].state, "done")
            self.assertEqual(frag.milestones[0].tasks_done, 3)
            # T9 pending→done 需要 ledger 有行;done_all=True 时写了 Task 9: complete

    def test_no_workspace_empty(self):
        with tempfile.TemporaryDirectory() as d:
            frag, barriers = load_sdd(Path(d), NOW)
            self.assertEqual((frag.tasks, frag.milestones, barriers), ([], [], []))
            self.assertEqual(frag.warnings, [])

    def test_corrupt_workspace_skipped(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            good = make_workspace(root, wave="W25", done_all=True)
            bad = root / ".superpowers" / "sdd" / "2026-09-13-W26"
            bad.mkdir()
            (bad / "dag.json").write_text("{ 损坏的 json", encoding="utf-8")
            os.utime(good / "dag.json", (1_000_000_000, 1_000_000_000))
            os.utime(bad / "dag.json", (2_000_000_000, 2_000_000_000))
            frag, barriers = load_sdd(root, NOW)          # 损坏源(且是最新)不白屏
            self.assertEqual(frag.warnings, ["sdd 源不可用"])
            self.assertEqual([t.id for t in frag.tasks], ["T1", "T6", "T9"])  # 出自唯一可解析源
            self.assertEqual(frag.milestones[0].id, "W25")
            self.assertEqual(barriers, ["屏障 1: T6 → T9"])

    def test_two_workspaces_order_by_mtime(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            old = make_workspace(root, wave="W27", done_all=True)
            new = make_workspace(root, wave="W25", done_all=False)
            dag = json.loads((old / "dag.json").read_text(encoding="utf-8"))
            dag["tasks"]["2"] = {"label": "旧波次独有任务"}
            dag["barriers"] = [{"id": "0", "after": [1], "unlocks": [6]}]
            (old / "dag.json").write_text(json.dumps(dag, ensure_ascii=False), encoding="utf-8")
            ledger = (old / "progress.md").read_text(encoding="utf-8")
            (old / "progress.md").write_text(ledger + "Task 2: complete\n", encoding="utf-8")
            # 名字序(W25<W27)与 mtime 序相反:钉死 R12——最新按 mtime 定,不按路径名
            os.utime(old / "dag.json", (1_000_000_000, 1_000_000_000))
            os.utime(new / "dag.json", (2_000_000_000, 2_000_000_000))
            frag, barriers = load_sdd(root, NOW)
            self.assertEqual([m.id for m in frag.milestones], ["W27", "W25"])  # 旧→新累计
            self.assertEqual(frag.milestones[-1].id, "W25")                    # 当前波次在末位
            self.assertEqual(frag.milestones[0].state, "done")
            self.assertEqual(frag.milestones[-1].state, "active")
            self.assertEqual(len(frag.tasks), 3)            # tasks 只出自最新工作区
            self.assertNotIn("旧波次独有任务", [t.label for t in frag.tasks])
            self.assertEqual(barriers, ["屏障 1: T6 → T9"])  # 屏障只出自最新工作区

    def test_corrupt_payload_kinds_skip_workspace(self):
        # C1:非数字任务键(int("abc")→ValueError)与非 UTF-8 dag.json
        # (UnicodeDecodeError)均按损坏源跳过并告警,不得炸掉渲染路径(spec §9)
        def bad_key(ws):
            (ws / "dag.json").write_text(json.dumps(
                {"wave": "W1", "lanes": [], "tasks": {"abc": {"label": "x"}},
                 "barriers": []}), encoding="utf-8")

        def bad_utf8(ws):
            (ws / "dag.json").write_bytes(b"\xff\xfe{}")

        for maker in (bad_key, bad_utf8):
            with self.subTest(maker=maker.__name__):
                with tempfile.TemporaryDirectory() as d:
                    ws = Path(d) / ".superpowers" / "sdd" / "2026-09-13-W1"
                    ws.mkdir(parents=True)
                    maker(ws)
                    frag, barriers = load_sdd(Path(d), NOW)
                    self.assertEqual(frag.warnings, ["sdd 源不可用"])
                    self.assertEqual((frag.tasks, frag.milestones, barriers), ([], [], []))

    def test_active_task_since_is_ledger_mtime(self):
        # R18:活跃任务 since=本工作区 progress.md mtime(「台账 2h 无跃迁→⚑」近似语义)
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            make_workspace(root, done_all=False)     # T6 active(dispatched)
            ledger = root / ".superpowers" / "sdd" / "2026-09-13-W26" / "progress.md"
            mtime = (datetime.fromisoformat(NOW) - timedelta(hours=1)).timestamp()
            os.utime(ledger, (mtime, mtime))
            frag, _ = load_sdd(root, NOW)
            by_id = {t.id: t for t in frag.tasks}
            self.assertEqual(by_id["T6"].since,
                             datetime.fromtimestamp(mtime).astimezone().isoformat(timespec="seconds"))
            self.assertIsNone(by_id["T1"].since)    # done 不带 since
            self.assertIsNone(by_id["T9"].since)    # pending 不带 since

    def test_ledger_missing_degrades_without_since(self):
        # R18 守卫:progress.md 缺失(→ 空台账、全 pending)不炸,since 一律 None
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            make_workspace(root, done_all=False)
            (root / ".superpowers" / "sdd" / "2026-09-13-W26" / "progress.md").unlink()
            frag, _ = load_sdd(root, NOW)
            self.assertEqual(frag.warnings, [])
            self.assertTrue(all(t.since is None for t in frag.tasks))


if __name__ == "__main__":
    unittest.main()
