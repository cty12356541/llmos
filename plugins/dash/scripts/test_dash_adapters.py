# plugins/dash/scripts/test_dash_adapters.py
import json
import tempfile
import time
import unittest
from pathlib import Path

from dashlib.adapters import load_session
from dashlib.model import Fragment


def write_events(tmp: Path, events):
    p = tmp / "state.jsonl"
    p.write_text("".join(json.dumps(e, ensure_ascii=False) + "\n" for e in events),
                 encoding="utf-8")
    return p


NOW = "2026-09-13T15:00:00+08:00"


class TestSessionAdapter(unittest.TestCase):
    def test_todos_become_tasks(self):
        with tempfile.TemporaryDirectory() as d:
            p = write_events(Path(d), [
                {"ts": "2026-09-13T14:50:00+08:00", "kind": "todo", "event": "updated",
                 "session": "s1", "summary": "修面板;写测试"},
                {"ts": "2026-09-13T14:55:00+08:00", "kind": "todo", "event": "updated",
                 "session": "s1", "summary": "写测试"},
            ])
            frag = load_session(p, NOW)
            self.assertEqual(frag.source, "session")
            states = {t.id: t.state for t in frag.tasks}
            self.assertEqual(states["todo-s1-0"], "active")
            self.assertEqual(states["todo-s1-1"], "done")   # 第二快照中消失→done
            self.assertEqual(frag.tasks[0].since, "2026-09-13T14:50:00+08:00")

    def test_active_agent_activity(self):
        with tempfile.TemporaryDirectory() as d:
            p = write_events(Path(d), [
                {"ts": "2026-09-13T14:30:00+08:00", "kind": "agent", "event": "spawned",
                 "session": "s1", "summary": "探索写集"},
                {"ts": "2026-09-13T14:58:00+08:00", "kind": "stop", "event": "turn_end",
                 "session": "s1", "summary": ""},
            ])
            frag = load_session(p, NOW)
            self.assertEqual(len(frag.activity), 1)
            self.assertEqual(frag.activity[0].label, "探索写集")
            self.assertEqual(frag.activity[0].since, "2026-09-13T14:30:00+08:00")

    def test_completed_agent_dropped(self):
        with tempfile.TemporaryDirectory() as d:
            p = write_events(Path(d), [
                {"ts": "2026-09-13T14:00:00+08:00", "kind": "agent", "event": "spawned",
                 "session": "s1", "summary": "跑测试"},
                {"ts": "2026-09-13T14:10:00+08:00", "kind": "agent", "event": "completed",
                 "session": "s1", "summary": "跑测试"},
            ])
            self.assertEqual(load_session(p, NOW).activity, [])

    def test_corrupt_lines_skipped(self):
        with tempfile.TemporaryDirectory() as d:
            p = Path(d) / "state.jsonl"
            p.write_text('{"ts":"2026-09-13T14:00:00+08:00","kind":"agent","event":"spawned",'
                         '"session":"s1","summary":"a"}\n{{{{corrupt\n', encoding="utf-8")
            self.assertEqual(len(load_session(p, NOW).activity), 1)

    def test_missing_file_empty_fragment(self):
        frag = load_session(Path("/nonexistent/state.jsonl"), NOW)
        self.assertEqual((frag.tasks, frag.activity, frag.warnings), ([], [], []))

    def test_perf_10k_events_under_100ms(self):
        with tempfile.TemporaryDirectory() as d:
            events = [{"ts": "2026-09-13T10:00:00+08:00", "kind": "stop",
                       "event": "turn_end", "session": f"s{i % 50}", "summary": ""} for i in range(10_000)]
            p = write_events(Path(d), events)
            t0 = time.perf_counter()
            load_session(p, NOW)
            self.assertLess(time.perf_counter() - t0, 0.1)


if __name__ == "__main__":
    unittest.main()
