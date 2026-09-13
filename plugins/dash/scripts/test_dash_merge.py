# plugins/dash/scripts/test_dash_merge.py
import unittest

from dashlib.merge import merge
from dashlib.model import Activity, Fragment, Task, Milestone

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


if __name__ == "__main__":
    unittest.main()
