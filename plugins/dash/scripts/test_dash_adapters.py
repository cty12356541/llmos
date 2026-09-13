# plugins/dash/scripts/test_dash_adapters.py
import json
import subprocess
import tempfile
import time
import unittest
from pathlib import Path

from dashlib.adapters import load_git, load_session
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

    def test_active_agent_activity(self):
        with tempfile.TemporaryDirectory() as d:
            p = write_events(Path(d), [
                {"ts": "2026-09-13T14:30:00+08:00", "kind": "agent", "event": "spawned",
                 "session": "s1", "summary": "探索写集"},
                {"ts": "2026-09-13T14:58:00+08:00", "kind": "stop", "event": "turn_end",
                 "session": "s1", "summary": ""},
            ])
            frag = load_session(p, NOW)
            self.assertEqual(len(frag.activity), 1)
            self.assertEqual(frag.activity[0].label, "探索写集")
            self.assertEqual(frag.activity[0].since, "2026-09-13T14:30:00+08:00")
            self.assertEqual(frag.activity[0].last_event, "turn_end 14:58")

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


if __name__ == "__main__":
    unittest.main()
