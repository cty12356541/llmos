#!/usr/bin/env python3
"""render_dag.py 视图层测试(零依赖,stdlib unittest)。

跑法:python3 scripts/test_render_dag.py(或仓库根 python3 -m unittest scripts.test_render_dag)

覆盖两个缺陷的回归:
  1. 无车道任务(tasks 有、lanes 无)必须在终端图与 mermaid 中可见,
     否则头部计数与网格行数矛盾(如 W26 的 ✓10 vs 8 行,活跃时 ▶1 无行可指)
  2. --view 只读模式不得写 dag.md / progress.md(常驻面板与 integrator
     并发写台账的丢账竞态由此消除)
"""
import importlib.util
import re
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent / "render_dag.py"

_spec = importlib.util.spec_from_file_location("render_dag", SCRIPT)
render_dag = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(render_dag)


def strip_ansi(s: str) -> str:
    return re.sub(r"\033\[[0-9;]*m", "", s)


FIXTURE_DAG = {
    "wave": "W-test",
    "title": "测试波次",
    "lanes": [
        {"id": "a", "name": "lane-a", "tasks": [1, 2]},
        {"id": "b", "name": "lane-b", "tasks": [3]},
    ],
    "tasks": {
        "1": {"label": "甲任务"},
        "2": {"label": "乙任务"},
        "3": {"label": "丙任务"},
        "4": {"label": "无车道任务"},
        "5": {"label": "另一无车道"},
    },
    "barriers": [
        {"id": "1", "after": [2, 3], "unlocks": [4, 5]},
    ],
}

# 状态:1=done 2=active(dispatched) 3=done 4=done 5=pending(未出现)
FIXTURE_LEDGER = """# SDD ledger — fixture

Task 1: complete (commits aaa0000..bbb1111, review clean)
Task 2: dispatched to worker
Task 3: complete (commits ccc2222..ddd3333, review clean)
Task 4: complete (commits eee4444..fff5555, review clean)
"""


def fixture_status():
    return {n: render_dag.resolve_status(FIXTURE_LEDGER, n) for n in range(1, 6)}


class TerminalViewTests(unittest.TestCase):
    def test_laneless_tasks_visible(self):
        out = strip_ansi(render_dag.terminal_view(FIXTURE_DAG, fixture_status()))
        self.assertIn("T4 无车道任务", out)
        self.assertIn("T5 另一无车道", out)

    def test_mark_count_matches_task_count(self):
        """头部计数来自全部 tasks,网格标记数必须与之相等,否则计数撒谎。"""
        out = strip_ansi(render_dag.terminal_view(FIXTURE_DAG, fixture_status()))
        marks = re.findall(r"[✓▶·] T\d", out)
        self.assertEqual(len(marks), len(FIXTURE_DAG["tasks"]))


class MermaidViewTests(unittest.TestCase):
    def test_laneless_task_nodes_declared_with_status(self):
        """无车道任务必须作为带状态类的节点声明,而非仅靠屏障边隐式建点。"""
        out = render_dag.mermaid(FIXTURE_DAG, fixture_status())
        self.assertIn('T4["T4 无车道任务"]:::done', out)
        self.assertIn('T5["T5 另一无车道"]:::pending', out)


class OnelineViewTests(unittest.TestCase):
    def test_oneline_covers_all_tasks(self):
        out = render_dag.oneline_view(FIXTURE_DAG, fixture_status())
        self.assertIn("✓3 ▶1 ·1", out)


class ViewModeTests(unittest.TestCase):
    """--view 只读模式:打印终端图,零文件写副作用。"""

    def _make_workspace(self):
        ws = Path(tempfile.mkdtemp(prefix="dag-view-test-"))
        (ws / "dag.json").write_text(
            __import__("json").dumps(FIXTURE_DAG, ensure_ascii=False), encoding="utf-8"
        )
        (ws / "progress.md").write_text(FIXTURE_LEDGER, encoding="utf-8")
        return ws

    def test_view_mode_writes_nothing(self):
        ws = self._make_workspace()
        before = (ws / "progress.md").read_text(encoding="utf-8")
        r = subprocess.run(
            [sys.executable, str(SCRIPT), str(ws), "--view"],
            capture_output=True, text=True,
        )
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertIn("测试波次", r.stdout)
        self.assertFalse((ws / "dag.md").exists(), "--view 不得写 dag.md")
        self.assertEqual(
            (ws / "progress.md").read_text(encoding="utf-8"), before,
            "--view 不得改写 progress.md",
        )

    def test_full_mode_still_writes(self):
        """守门:默认完整模式保持既有注入行为(状态跃迁时用)。"""
        ws = self._make_workspace()
        r = subprocess.run(
            [sys.executable, str(SCRIPT), str(ws)],
            capture_output=True, text=True,
        )
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertTrue((ws / "dag.md").exists())
        self.assertIn("<!-- dag:begin -->", (ws / "progress.md").read_text(encoding="utf-8"))


if __name__ == "__main__":
    unittest.main()
