# plugins/dash/scripts/test_dash_renderers.py
import unittest

from dashlib.model import Activity, Model, Task


def model(tasks, activity=None, stalled=None):
    return Model(project="llmos", tasks=tasks,
                 activity=activity or [], stalled=stalled or [],
                 velocity={"commits_7d": 17})


class TestOneline(unittest.TestCase):
    def test_format(self):
        m = model([Task("T1", "a", "done", source="sdd"),
                   Task("T2", "b", "active", source="sdd"),
                   Task("T3", "c", "pending", source="sdd")],
                  activity=[Activity("agent", "x", "2026-09-13T14:00:00+08:00"),
                            Activity("agent", "y", "2026-09-13T14:00:00+08:00")],
                  stalled=["T9(6h)"])
        from dashlib.render_oneline import render_oneline
        # ▶ 口径按 spec §4:只数任务状态 active+stalled(本例 T2);T9 仅是
        # ⚑ 告警标记、无任务记录,不计入 ▶(brief 原字面 ▶2 与 spec 矛盾,已正为 ▶1)。
        self.assertEqual(render_oneline(m),
                         "[dash] llmos ✓1▶1·1 ⚑1 ·2ag")


if __name__ == "__main__":
    unittest.main()
