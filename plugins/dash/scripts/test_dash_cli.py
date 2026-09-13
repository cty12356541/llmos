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


if __name__ == "__main__":
    unittest.main()
