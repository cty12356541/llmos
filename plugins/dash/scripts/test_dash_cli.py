# plugins/dash/scripts/test_dash_cli.py
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

DASH = Path(__file__).resolve().parent / "dash"


def run(args, cwd):
    return subprocess.run([sys.executable, str(DASH), *args], cwd=cwd,
                          capture_output=True, text=True)


class TestFocusCli(unittest.TestCase):
    def test_write_read_clear(self):
        with tempfile.TemporaryDirectory() as d:
            r = run(["focus", "T9"], d)
            self.assertEqual(r.returncode, 0, r.stderr)
            data = json.loads((Path(d) / ".dash" / "focus.json").read_text(encoding="utf-8"))
            self.assertEqual(data["target"], "T9")
            self.assertIn("聚焦 T9", r.stdout)
            r = run(["focus", "clear"], d)
            self.assertEqual(r.returncode, 0)
            self.assertFalse((Path(d) / ".dash" / "focus.json").exists())

    def test_focus_respected_by_panel(self):
        with tempfile.TemporaryDirectory() as d:
            run(["focus", "zz"], d)
            r = run(["render", "panel"], d)
            self.assertIn("聚焦 zz", r.stdout)   # 无匹配任务也显示聚焦头

    def test_nonobject_config_warns(self):
        # 顶层非对象(数组/null)即损坏,与解析失败同告警,不得静默降级
        for payload in ("[1, 2, 3]", "null"):
            with self.subTest(payload=payload):
                with tempfile.TemporaryDirectory() as d:
                    (Path(d) / ".dash").mkdir()
                    (Path(d) / ".dash" / "config.json").write_text(payload, encoding="utf-8")
                    r = run(["render", "panel"], d)
                    self.assertEqual(r.returncode, 0, r.stderr)
                    self.assertIn("config 损坏,已用缺省阈值", r.stdout)


if __name__ == "__main__":
    unittest.main()
