# plugins/dash/scripts/test_dash_model.py
import unittest
from datetime import datetime, timedelta, timezone

from dashlib.model import Task, is_stalled, age, display_width


class TestModel(unittest.TestCase):
    def test_stalled_over_threshold(self):
        t = Task(id="T9", label="全仓验证门", state="active",
                 since="2026-09-13T08:00:00+08:00", source="sdd")
        now = "2026-09-13T15:00:00+08:00"  # 7h > 2h
        self.assertTrue(is_stalled(t, now, threshold_h=2.0))

    def test_active_under_threshold(self):
        t = Task(id="T9", label="x", state="active",
                 since="2026-09-13T14:30:00+08:00", source="sdd")
        self.assertFalse(is_stalled(t, "2026-09-13T15:00:00+08:00", 2.0))

    def test_non_active_never_stalled(self):
        t = Task(id="T1", label="x", state="done",
                 since="2026-09-13T08:00:00+08:00", source="sdd")
        self.assertFalse(is_stalled(t, "2026-09-13T15:00:00+08:00", 2.0))

    def test_age_format(self):
        self.assertEqual(age("2026-09-13T14:55:00+08:00", "2026-09-13T15:00:00+08:00"), "5m")
        self.assertEqual(age("2026-09-13T09:00:00+08:00", "2026-09-13T15:00:00+08:00"), "6h")

    def test_display_width_cjk(self):
        self.assertEqual(display_width("台账"), 4)      # 2×2
        self.assertEqual(display_width("ab"), 2)
        self.assertEqual(display_width("✓ T1 表组"), 9)  # ✓=1 空格=1 T1=2 空格=1 表组=4(brief 原写 8,但其自身分项之和为 9,已按公式修正)

    def test_since_missing_not_stalled(self):
        t = Task(id="T2", label="x", state="active", source="session")
        self.assertFalse(is_stalled(t, "2026-09-13T15:00:00+08:00", 2.0))


if __name__ == "__main__":
    unittest.main()
