# plugins/dash/scripts/test_dash_renderers.py
import unittest

from dashlib.model import Activity, Model, Task


def model(tasks, activity=None, stalled=None):
    return Model(project="llmos", tasks=tasks,
                 activity=activity or [], stalled=stalled or [],
                 velocity={"commits_7d": 17})


class TestOneline(unittest.TestCase):
    def test_format_with_active_milestone(self):
        # I7:单行带上活跃里程碑 id;▶ 口径按 spec §4 只数 active+stalled(本例 T2)
        from dashlib.model import Milestone
        m = model([Task("T1", "a", "done", source="sdd"),
                   Task("T2", "b", "active", source="sdd"),
                   Task("T3", "c", "pending", source="sdd")],
                  activity=[Activity("agent", "x", "2026-09-13T14:00:00+08:00"),
                            Activity("agent", "y", "2026-09-13T14:00:00+08:00")],
                  stalled=["T9(6h)"])
        m.milestones = [Milestone("W26", title="统一恢复面", state="active",
                                  tasks_done=1, tasks_total=3)]
        from dashlib.render_oneline import render_oneline
        self.assertEqual(render_oneline(m),
                         "[dash] llmos W26 ✓1▶1·1 ⚑1 ·2ag")

    def test_format_without_active_milestone_omits_token(self):
        # I7:无活跃里程碑(全 done/纯 git 仓库)时里程碑 token 整体省略
        from dashlib.model import Milestone
        m = model([Task("T1", "a", "done", source="sdd"),
                   Task("T2", "b", "active", source="sdd"),
                   Task("T3", "c", "pending", source="sdd")],
                  activity=[Activity("agent", "x", "2026-09-13T14:00:00+08:00")],
                  stalled=[])
        m.milestones = [Milestone("W25", title="旧波次", state="done",
                                  tasks_done=7, tasks_total=7)]
        from dashlib.render_oneline import render_oneline
        self.assertEqual(render_oneline(m), "[dash] llmos ✓1▶1·1 ⚑0 ·1ag")


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

    def test_all_done_milestone_neutral_placeholder(self):
        # Minor 8:无 active 也无 planned(全部完成)时用中性"—",
        # 不虚报"进行中"、也不出"下一步"
        from dashlib.model import Milestone
        from dashlib.render_panel import render_panel
        m = model([Task("T1", "a", "done", source="sdd")])
        m.milestones = [Milestone("W27", title="收尾", state="done",
                                  tasks_done=3, tasks_total=3)]
        out = render_panel(m, focus=None, now_iso=NOW)
        plain = out.replace("\x1b[0m", "")
        self.assertIn("—", plain)
        self.assertNotIn("进行中", plain)
        self.assertNotIn("下一步", plain)


class TestHtmlMermaid(unittest.TestCase):
    def test_html_selfcontained(self):
        from dashlib.render_html import render_html
        out = render_html(full_model(), NOW)
        self.assertTrue(out.startswith("<!DOCTYPE html>"))
        self.assertIn("<style>", out)
        self.assertNotIn("http://", out)        # 零外链
        self.assertIn("prefers-color-scheme", out)
        self.assertIn("T9 全仓验证门", out)

    def test_html_escapes_label_markup(self):
        # HTML 快照是唯一标记上下文:label 含 < 必须转义,不得注入活标签
        from dashlib.render_html import render_html
        m = model([Task("T1", "注入 <script>alert(1)</script>", "done", source="sdd")])
        out = render_html(m, NOW)
        self.assertIn("&lt;script&gt;", out)
        self.assertNotIn("<script>", out)

    def test_mermaid_laneless_and_barrier(self):
        from dashlib.render_mermaid import render_mermaid
        out = render_mermaid(full_model())
        self.assertIn('T9["T9 全仓验证门"]:::pending', out)   # 无车道顶层节点带类
        self.assertIn("T6 -.-> T9", out)                      # 屏障虚线
        self.assertIn("subgraph", out)

    def test_inject_idempotent(self):
        import tempfile
        from pathlib import Path
        from dashlib.render_mermaid import inject_mermaid
        with tempfile.TemporaryDirectory() as d:
            ws = Path(d)
            (ws / "progress.md").write_text("# ledger\n", encoding="utf-8")
            inject_mermaid(ws, "```mermaid\nflowchart LR\n```\n")
            first = (ws / "progress.md").read_text(encoding="utf-8")
            inject_mermaid(ws, "```mermaid\nflowchart LR\n```\n")
            self.assertEqual(first, (ws / "progress.md").read_text(encoding="utf-8"))

    def test_inject_backslash_in_block_is_literal(self):
        # Minor 10:注入块含反斜杠(如 \1)时替换必须按字面写入;
        # 字符串替换式 re.sub 会把 \1 当组引用(抛 invalid group reference)
        import tempfile
        from pathlib import Path
        from dashlib.render_mermaid import inject_mermaid
        section = '```mermaid\nflowchart LR\n  A["x\\1y"]\n```'
        with tempfile.TemporaryDirectory() as d:
            ws = Path(d)
            (ws / "progress.md").write_text("# ledger\n", encoding="utf-8")
            inject_mermaid(ws, section)                 # 首次=追加,无替换路径
            inject_mermaid(ws, section)                 # 二次=替换路径,反斜杠按字面
            text = (ws / "progress.md").read_text(encoding="utf-8")
            self.assertIn("x\\1y", text)
            self.assertEqual(text.count("x\\1y"), 1)    # 幂等


if __name__ == "__main__":
    unittest.main()
