# plugins/dash/scripts/test_dash_hooks.py
import json
import tempfile
import unittest
from pathlib import Path

from importlib import import_module
import sys
sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "hooks"))
record_event = import_module("record_event")


def hook_payload(tool_name, tool_input=None, tool_response=None):
    return json.dumps({"tool_name": tool_name,
                       "tool_input": tool_input or {},
                       "tool_response": tool_response or {},
                       "session_id": "sess-1", "cwd": "/tmp"})


class TestRecordEvent(unittest.TestCase):
    def test_todo_event_appended(self):
        with tempfile.TemporaryDirectory() as d:
            p = Path(d) / "state.jsonl"
            record_event.main(hook_payload("TodoWrite",
                                           {"todos": [{"content": "修面板", "status": "in_progress"}]}),
                              p)
            lines = p.read_text(encoding="utf-8").strip().splitlines()
            ev = json.loads(lines[-1])
            self.assertEqual(ev["kind"], "todo")
            self.assertEqual(ev["event"], "updated")
            self.assertIn("修面板", ev["summary"])
            self.assertTrue(ev["ts"])

    def test_agent_spawned(self):
        with tempfile.TemporaryDirectory() as d:
            p = Path(d) / "state.jsonl"
            record_event.main(hook_payload("Agent", {"description": "探索写集"}), p)
            ev = json.loads(p.read_text(encoding="utf-8").strip().splitlines()[-1])
            self.assertEqual((ev["kind"], ev["event"]), ("agent", "spawned"))
            self.assertIn("探索写集", ev["summary"])

    def test_stop_event(self):
        with tempfile.TemporaryDirectory() as d:
            p = Path(d) / "state.jsonl"
            record_event.main(hook_payload("Stop"), p)
            ev = json.loads(p.read_text(encoding="utf-8").strip().splitlines()[-1])
            self.assertEqual((ev["kind"], ev["event"]), ("stop", "turn_end"))

    def test_garbage_stdin_never_raises(self):
        with tempfile.TemporaryDirectory() as d:
            p = Path(d) / "state.jsonl"
            record_event.main("not json at all{", p)   # 不得抛异常
            record_event.main("", p)
            self.assertFalse(p.exists())               # 无有效事件不落盘


if __name__ == "__main__":
    unittest.main()
