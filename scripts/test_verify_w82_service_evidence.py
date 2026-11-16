import json
from pathlib import Path
import sys
import tempfile
import unittest

from verify_w82_service_evidence import Evidence, check_escape_matrix, check_openat2_trace


class ServiceEvidenceTests(unittest.TestCase):
    def test_nonzero_exit_is_recorded_and_an_independent_command_can_run(self):
        with tempfile.TemporaryDirectory() as directory:
            output = Path(directory)
            evidence = Evidence(output)
            evidence.run("failed", [sys.executable, "-c", "print('first'); raise SystemExit(7)"])
            evidence.run("next", [sys.executable, "-c", "print('second')"])

            self.assertEqual(evidence.exit_code, 1)
            self.assertEqual((output / "failed.log").read_text().strip(), "first")
            self.assertEqual((output / "next.log").read_text().strip(), "second")
            records = [json.loads(line) for line in (output / "results.jsonl").read_text().splitlines()]
            self.assertEqual([record["returncode"] for record in records], [7, 0])

    def test_skipped_windows_symlink_fixture_fails_evidence_gate(self):
        output = (
            "escape_preopen_leaf=windows_symlink_unavailable os_error=1314\n"
            "escape_preopen_leaf=hard_link\n"
            "escape_preopen_parent=ntfs_junction\n"
        )
        with tempfile.TemporaryDirectory() as directory:
            evidence = Evidence(Path(directory))
            check_escape_matrix(evidence, output, "windows")
            self.assertEqual(evidence.exit_code, 1)

    def test_fallback_needs_an_injected_enosys_from_an_actual_openat2_call(self):
        with tempfile.TemporaryDirectory() as directory:
            evidence = Evidence(Path(directory))
            trace = Path(directory) / "openat2.trace"
            thread_trace = trace.with_suffix(".trace.123")
            unavailable = (
                'openat2(3, "nested/leaf", {resolve=RESOLVE_BENEATH}, 24) '
                '= -1 ENOSYS (Function not implemented)'
            )
            thread_trace.write_text(unavailable + "\n")
            check_openat2_trace(evidence, trace, injected=True)
            self.assertEqual(evidence.exit_code, 1)

            thread_trace.write_text(unavailable + " (INJECTED)\n")
            evidence = Evidence(Path(directory))
            check_openat2_trace(evidence, trace, injected=True)
            self.assertEqual(evidence.exit_code, 0)


if __name__ == "__main__":
    unittest.main()
