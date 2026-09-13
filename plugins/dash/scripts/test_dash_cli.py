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


class TestSend(unittest.TestCase):
    def test_compose_prompt(self):
        from dashlib.sendkeys import compose_prompt
        self.assertEqual(compose_prompt("T9", "全仓验证门"),
                         "聚焦 T9(全仓验证门):汇总当前障碍、最近回执与下一步建议")

    def test_compose_prompt_empty_label(self):
        # R17:空 label(默认 ⏎ 路径 "当前波次" 无匹配任务)省略空括号,不出悬垂 ()
        from dashlib.sendkeys import compose_prompt
        self.assertEqual(compose_prompt("T9", ""),
                         "聚焦 T9:汇总当前障碍、最近回执与下一步建议")

    def test_watch_bad_interval_usage(self):
        with tempfile.TemporaryDirectory() as d:
            r = run(["watch", "5s"], d)
            self.assertEqual(r.returncode, 2, r.stdout)
            self.assertIn("用法", r.stderr)
            self.assertNotIn("Traceback", r.stderr)   # 守卫兜住,不裸抛 ValueError

    def test_send_degrades_without_tmux(self):
        import os
        from dashlib.sendkeys import send_to_conversation
        env = dict(os.environ, PATH="/nonexistent")
        # 注:brief 原文 sys.path 插的是 .../dashlib 子目录,`from dashlib.sendkeys`
        # 需要包父目录(scripts/)在路径上,照抄会 ModuleNotFoundError——改为插父目录
        r = subprocess.run([sys.executable, "-c",
                            "import sys;sys.path.insert(0,%r);from dashlib.sendkeys import send_to_conversation;"
                            "print(send_to_conversation('聚焦 T9', None))" % str(Path(__file__).parent)],
                           capture_output=True, text=True, env=env)
        self.assertIn("聚焦 T9", r.stdout)   # 无 tmux → 打印可复制文本,不失败

    def test_watch_once_renders_frame(self):
        with tempfile.TemporaryDirectory() as d:
            r = run(["watch", "--once"], d)
            self.assertEqual(r.returncode, 0, r.stderr)
            self.assertIn("dash", r.stdout.lower())   # 空仓也有友好空态


if __name__ == "__main__":
    unittest.main()
