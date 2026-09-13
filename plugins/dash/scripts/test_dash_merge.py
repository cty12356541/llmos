# plugins/dash/scripts/test_dash_merge.py
import json
import os
import tempfile
import unittest
from datetime import datetime, timedelta
from pathlib import Path

from dashlib.merge import merge
from dashlib.model import Activity, Fragment, Task

NOW = "2026-09-13T15:00:00+08:00"


def frag(source, tasks=None, **kw):
    return Fragment(source=source, tasks=tasks or [], **kw)


class TestMerge(unittest.TestCase):
    def test_deep_source_wins(self):
        t_git = Task(id="T9", label="全仓验证门(旧)", state="pending", source="git")
        t_sdd = Task(id="T9", label="全仓验证门", state="active",
                     since="2026-09-13T08:00:00+08:00", source="sdd")
        m = merge("llmos", [frag("git", [t_git]), frag("sdd", [t_sdd])], [], NOW)
        self.assertEqual(m.tasks[0].label, "全仓验证门")
        self.assertEqual(m.tasks[0].state, "stalled")     # active 7h > 2h

    def test_stalled_summary(self):
        t = Task(id="T9", label="x", state="active",
                 since="2026-09-13T09:00:00+08:00", source="sdd")
        m = merge("llmos", [frag("sdd", [t])], [], NOW)
        self.assertEqual(m.stalled, ["T9(6h)"])

    def test_pending_stays_pending_when_only_git(self):
        t = Task(id="T9", label="x", state="pending", source="git")
        m = merge("llmos", [frag("git", [t])], [], NOW)
        self.assertEqual(m.tasks[0].state, "pending")

    def test_velocity_and_warnings_merge(self):
        m = merge("llmos",
                  [frag("git", velocity={"commits_7d": 17}, warnings=["git 源不可用"]),
                   frag("session", activity=[Activity("agent", "探索", NOW)])],
                  [], NOW)
        self.assertEqual(m.velocity["commits_7d"], 17)
        self.assertEqual(m.warnings, ["git 源不可用"])
        self.assertEqual(len(m.activity), 1)

    def test_barriers_pass_through(self):
        # merge 不得吞/改 barriers:load_sdd 的屏障串原样进模型
        m = merge("llmos", [frag("git")], ["B1"], NOW)
        self.assertEqual(m.barriers, ["B1"])

    def test_sdd_active_task_stalled_via_ledger_mtime(self):
        # R18 端到端:活跃 sdd 任务 since=台账 mtime → 台账 2h 无跃迁即 ⚑ 可达
        from dashlib.adapters import load_sdd
        with tempfile.TemporaryDirectory() as d:
            ws = Path(d) / ".superpowers" / "sdd" / "2026-09-13-W1"
            ws.mkdir(parents=True)
            (ws / "dag.json").write_text(json.dumps(
                {"wave": "W1", "lanes": [], "tasks": {"1": {"label": "x"}},
                 "barriers": []}), encoding="utf-8")
            (ws / "progress.md").write_text("Task 1: dispatched\n", encoding="utf-8")
            mtime = (datetime.fromisoformat(NOW) - timedelta(hours=6)).timestamp()
            os.utime(ws / "progress.md", (mtime, mtime))
            frag, _ = load_sdd(Path(d), NOW)
            m = merge("llmos", [frag], [], NOW)
            self.assertEqual(m.tasks[0].state, "stalled")
            self.assertEqual(m.stalled, ["T1(6h)"])


if __name__ == "__main__":
    unittest.main()
