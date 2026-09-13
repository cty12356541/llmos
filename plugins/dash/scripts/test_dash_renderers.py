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


NOW = "2026-09-13T15:00:00+08:00"


def full_model():
    from dashlib.model import Milestone
    m = model([
        Task("T1", "schema v42 表组", "done", lane="semantic-ledger", source="sdd"),
        Task("T6", "cycle 拆分双域", "active", lane="worker-dual",
             since="2026-09-13T14:30:00+08:00", source="sdd", note="fix round"),
        Task("T9", "全仓验证门", "pending", lane="无车道", source="sdd"),
    ], activity=[Activity("agent", "探索写集", "2026-09-13T14:55:00+08:00")])
    m.milestones = [Milestone("W25", title="旧波次", state="done", tasks_done=7, tasks_total=7),
                    Milestone("W26", title="统一恢复面", state="active", tasks_done=1, tasks_total=3)]
    m.barriers = ["屏障 1: T6 → T9"]
    return m


class TestPanel(unittest.TestCase):
    def test_sections_and_marks(self):
        from dashlib.render_panel import render_panel
        out = render_panel(full_model(), focus=None, now_iso=NOW)
        plain = out.replace("\x1b[0m", "")
        self.assertIn("在跑 / 健康", plain)
        self.assertIn("探索写集 · 5m", plain)
        self.assertIn("轨迹", plain)
        self.assertIn("W26 统一恢复面", plain)
        # R2 裁定:brief 原句是恒真 no-op,替换为真实断言——fixture 的 W26
        # 里程碑为 active,故"下一步:待启动"行不得出现。
        self.assertNotIn("下一步:待启动", plain)
        self.assertIn("T9 全仓验证门", plain)             # 无车道行可见(回归 T 前置修复)
        self.assertIn("屏障 1: T6 → T9", plain)

    def test_focus_filters(self):
        from dashlib.render_panel import render_panel
        out = render_panel(full_model(), focus="T6", now_iso=NOW)
        plain = out.replace("\x1b[0m", "")
        self.assertIn("聚焦 T6", plain)
        self.assertIn("cycle 拆分双域", plain)
        self.assertIn("其余:", plain)
        self.assertNotIn("schema v42 表组", plain)        # 非焦点任务被压缩


if __name__ == "__main__":
    unittest.main()
