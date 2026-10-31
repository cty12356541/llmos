"""CI-discovered conformance gate for SABI cross-epoch compatibility diffing."""

from __future__ import annotations

import importlib.util
import io
import json
from pathlib import Path
import sys
from types import ModuleType
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[3]
TOOL_DIR = ROOT / "tools" / "sabi-compat-diff"
DIFF_SCRIPT_PATH = TOOL_DIR / "sabi_compat_diff.py"
DIFF_TESTS_PATH = TOOL_DIR / "test_sabi_compat_diff.py"


def _load_module(module_name: str, file_path: Path) -> ModuleType:
    if not file_path.is_file():
        raise FileNotFoundError(f"missing SABI compat-diff module: {file_path}")
    spec = importlib.util.spec_from_file_location(module_name, file_path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"unable to load module spec from {file_path}")
    module = importlib.util.module_from_spec(spec)
    sys.modules[module_name] = module
    spec.loader.exec_module(module)
    return module


_SCD_MODULE = _load_module("sabi_compat_diff", DIFF_SCRIPT_PATH)
_SCD_TEST_MODULE = _load_module("sabi_compat_diff_tool_tests", DIFF_TESTS_PATH)


class SabiCompatDiffTests(_SCD_TEST_MODULE.SabiCompatDiffTest):
    """Bridges all unit tests from tools/sabi-compat-diff into CI schema discovery."""


class SabiCanonicalBaselineSelfDiffGateTests(unittest.TestCase):
    """Standing self-diff verification against the frozen SABI v1 schema baseline."""

    def test_canonical_v1_manifest_and_proto_self_diff_is_compatible(self) -> None:
        manifest_path = (
            ROOT / "schema" / "nlos" / "sabi" / "v1" / "deprecation-manifest.yaml"
        )
        proto_dir = ROOT / "schema" / "nlos" / "sabi" / "v1"
        self.assertTrue(manifest_path.is_file(), f"missing manifest: {manifest_path}")
        self.assertTrue(proto_dir.is_dir(), f"missing proto dir: {proto_dir}")

        stdout_buf = io.StringIO()
        with patch("sys.stdout", stdout_buf):
            exit_code = _SCD_MODULE.main(
                [
                    str(manifest_path),
                    str(manifest_path),
                    "--baseline-proto-dir",
                    str(proto_dir),
                    "--candidate-proto-dir",
                    str(proto_dir),
                    "--json",
                ]
            )

        self.assertEqual(exit_code, 0)
        payload = json.loads(stdout_buf.getvalue())
        self.assertIs(payload["compatible"], True)
        self.assertEqual(payload["summary"][_SCD_MODULE.BREAKING_VIOLATION], 0)
        self.assertGreaterEqual(payload["summary"][_SCD_MODULE.UNCHANGED], 200)


if __name__ == "__main__":
    unittest.main()
