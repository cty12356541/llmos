"""CI-discovered conformance gate for Stage E ledger-schema v1.3 coverage and ROAD-E-001 preflight."""

from __future__ import annotations

import importlib.util
from pathlib import Path
import subprocess
import sys
from types import ModuleType
import unittest

ROOT = Path(__file__).resolve().parents[3]
LINT_CLAIMS_PATH = ROOT / "scripts" / "lint_claims.py"
TEST_LINT_CLAIMS_PATH = ROOT / "scripts" / "test_lint_claims.py"
VERIFY_STAGE_E_LEDGER_PATH = ROOT / "tools" / "verify_stage_e_ledger_coverage.py"
ROAD_E001_MATRIX_PATH = ROOT / "scripts" / "run_road_e001_upgrade_matrix.py"


def _load_module(module_name: str, file_path: Path) -> ModuleType:
    if not file_path.is_file():
        raise FileNotFoundError(f"missing required gate script: {file_path}")
    spec = importlib.util.spec_from_file_location(module_name, file_path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"unable to load module spec from {file_path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[module_name] = module
    spec.loader.exec_module(module)
    return module


_LINT_CLAIMS = _load_module("nlos_ci_lint_claims", LINT_CLAIMS_PATH)
_VERIFY_STAGE_E = _load_module("nlos_ci_verify_stage_e_ledger", VERIFY_STAGE_E_LEDGER_PATH)
_ROAD_E001_MATRIX = _load_module("nlos_ci_road_e001_matrix", ROAD_E001_MATRIX_PATH)


class StageELedgerV13StandingGateTests(unittest.TestCase):
    """Standing CI gate for ledger-schema v1.3 bidirectional lint and Stage E strict coverage."""

    def test_lint_claims_and_unit_suite_pass(self) -> None:
        findings, stats = _LINT_CLAIMS.run(ROOT)
        errors = [item for item in findings if item[0] == "ERROR"]
        self.assertEqual(errors, [], f"lint_claims.py reported errors: {errors}")
        self.assertGreaterEqual(sum(stats["claims"].values()), 169)
        self.assertGreaterEqual(sum(stats["risks"].values()), 17)
        self.assertGreaterEqual(stats["index"], 204)
        self.assertEqual(stats["index"], stats["on_disk"])

        proc = subprocess.run(
            [sys.executable, "-X", "utf8", "-m", "unittest", "scripts.test_lint_claims"],
            cwd=str(ROOT),
            capture_output=True,
            text=True,
            encoding="utf-8",
            check=False,
        )
        self.assertEqual(
            proc.returncode,
            0,
            f"scripts.test_lint_claims failed:\nSTDOUT:\n{proc.stdout}\nSTDERR:\n{proc.stderr}",
        )

    def test_verify_stage_e_ledger_coverage_self_test_and_strict_mode(self) -> None:
        self_test_proc = subprocess.run(
            [sys.executable, "-X", "utf8", str(VERIFY_STAGE_E_LEDGER_PATH), "--self-test"],
            cwd=str(ROOT),
            capture_output=True,
            text=True,
            encoding="utf-8",
            check=False,
        )
        self.assertEqual(
            self_test_proc.returncode,
            0,
            f"verify_stage_e_ledger_coverage.py --self-test failed:\n{self_test_proc.stderr}",
        )

        report = _VERIFY_STAGE_E.audit_strict(ROOT)
        self.assertTrue(report["ok"], f"strict Stage E ledger audit failed: {report['errors']}")
        counts = report["counts"]
        self.assertGreaterEqual(counts["total_normative_requirements"], 45)
        self.assertGreaterEqual(counts["work_packages"], 8)
        self.assertGreaterEqual(counts["claims_total"], 169)
        self.assertGreaterEqual(counts["claims_by_stage"].get("E", 0), 53)
        self.assertGreaterEqual(counts["risks_total"], 17)
        self.assertEqual(counts["open_p0_risks"], 0)
        self.assertGreaterEqual(counts["stage_e_evidence_indexed"], 16)
        self.assertEqual(
            counts["stage_e_evidence_indexed"],
            counts["stage_e_evidence_on_disk"],
        )
        self.assertEqual(report["hygiene"]["emoji_violations"], [])
        self.assertEqual(report["hygiene"]["broken_links"], [])


class StageERoadE001PreflightAndSelfTestGateTests(unittest.TestCase):
    """Standing CI gate for ROAD-E-001 N-1 sealed baseline integrity and matrix harness self-tests."""

    def test_road_e001_matrix_self_tests_pass(self) -> None:
        proc = subprocess.run(
            [sys.executable, "-X", "utf8", str(ROAD_E001_MATRIX_PATH), "--self-test"],
            cwd=str(ROOT),
            capture_output=True,
            text=True,
            encoding="utf-8",
            check=False,
        )
        self.assertEqual(
            proc.returncode,
            0,
            f"run_road_e001_upgrade_matrix.py --self-test failed:\nSTDOUT:\n{proc.stdout}\nSTDERR:\n{proc.stderr}",
        )

    def test_sealed_n1_baseline_seals_and_ecosystem_manifest_purity(self) -> None:
        snapshot = _ROAD_E001_MATRIX.capture_sealed_snapshot(ROOT)
        self.assertEqual(len(snapshot), 16)
        seal_errors = _ROAD_E001_MATRIX.verify_seals_against_snapshot(ROOT, snapshot)
        self.assertEqual(seal_errors, [])
        manifest_violations = _ROAD_E001_MATRIX.audit_workspace_ecosystem_manifests(ROOT)
        self.assertEqual(manifest_violations, [])


if __name__ == "__main__":
    unittest.main()
