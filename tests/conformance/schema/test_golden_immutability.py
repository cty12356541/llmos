"""Unit tests for the SABI golden wire corpus immutability gate."""

from __future__ import annotations

import hashlib
import importlib.util
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[3]
TOOL_PATH = ROOT / "tools" / "check_golden_immutability.py"


def _load_gate_module():
    if not TOOL_PATH.is_file():
        raise FileNotFoundError(f"missing gate script: {TOOL_PATH}")
    spec = importlib.util.spec_from_file_location("check_golden_immutability", TOOL_PATH)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"unable to load module spec from {TOOL_PATH}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class GoldenImmutabilityGateTests(unittest.TestCase):
    """Positive and negative tamper-detection tests for golden wire corpus."""

    def _copy_corpus(self, temp_dir: str) -> Path:
        temp_root = Path(temp_dir)
        shutil.copytree(ROOT / "schema" / "golden", temp_root / "schema" / "golden")
        return temp_root

    def test_clean_corpus_passes(self) -> None:
        gate = _load_gate_module()
        ok, errors, summary = gate.check_golden_corpus(ROOT)
        self.assertTrue(ok, f"expected clean corpus to pass, got errors: {errors}")
        self.assertEqual(errors, [])
        self.assertEqual(summary["legacy_root_count"], 7)
        self.assertEqual(summary["epochs"]["v1.0"]["channel_count"], 9)
        self.assertGreaterEqual(summary["epochs"]["v1.0"]["vector_count"], 11)

    def test_tamper_single_byte_in_hex_fails(self) -> None:
        gate = _load_gate_module()
        with tempfile.TemporaryDirectory() as tmp:
            temp_root = self._copy_corpus(tmp)
            target = (
                temp_root
                / "schema"
                / "golden"
                / "epochs"
                / "v1.0"
                / "nlos.sabi.Envelope-v1.0.hex"
            )
            original = target.read_text(encoding="ascii").strip()
            flipped_first_byte = ("ff" if original[:2] != "ff" else "fe") + original[2:]
            target.write_text(flipped_first_byte + "\n", encoding="ascii")

            ok, errors, _ = gate.check_golden_corpus(temp_root)
            self.assertFalse(ok)
            joined = "\n".join(errors)
            self.assertIn("E_SHA256_WIRE_MISMATCH", joined)
            self.assertIn("E_SHA256_FILE_MISMATCH", joined)

        with tempfile.TemporaryDirectory() as tmp:
            temp_root = self._copy_corpus(tmp)
            root_target = (
                temp_root / "schema" / "golden" / "nlos.sabi.Envelope-v1.hex"
            )
            original = root_target.read_text(encoding="ascii").strip()
            flipped = ("ff" if original[:2] != "ff" else "fe") + original[2:]
            root_target.write_text(flipped + "\n", encoding="ascii")

            ok, errors, _ = gate.check_golden_corpus(temp_root)
            self.assertFalse(ok)
            self.assertIn("E_SHA256_WIRE_MISMATCH", "\n".join(errors))

    def test_noncanonical_uppercase_hex_fails(self) -> None:
        gate = _load_gate_module()
        with tempfile.TemporaryDirectory() as tmp:
            temp_root = self._copy_corpus(tmp)
            target = (
                temp_root
                / "schema"
                / "golden"
                / "epochs"
                / "v1.0"
                / "nlos.sabi.WaitControl.InspectWaitRequest-v1.0.hex"
            )
            original = target.read_text(encoding="ascii")
            target.write_text(original.upper(), encoding="ascii")

            ok, errors, _ = gate.check_golden_corpus(temp_root)
            self.assertFalse(ok)
            self.assertIn("E_NONCANONICAL_HEX", "\n".join(errors))

    def test_delete_golden_file_fails(self) -> None:
        gate = _load_gate_module()
        with tempfile.TemporaryDirectory() as tmp:
            temp_root = self._copy_corpus(tmp)
            (
                temp_root
                / "schema"
                / "golden"
                / "epochs"
                / "v1.0"
                / "nlos.sabi.PrincipalHandshake.Challenge-v1.0.hex"
            ).unlink()

            ok, errors, _ = gate.check_golden_corpus(temp_root)
            self.assertFalse(ok)
            self.assertIn("E_EPOCH_GOLDEN_MISSING", "\n".join(errors))

        with tempfile.TemporaryDirectory() as tmp:
            temp_root = self._copy_corpus(tmp)
            (
                temp_root
                / "schema"
                / "golden"
                / "nlos.sabi.PrincipalHandshake-v1.hex"
            ).unlink()

            ok, errors, _ = gate.check_golden_corpus(temp_root)
            self.assertFalse(ok)
            self.assertIn("E_ROOT_GOLDEN_MISSING", "\n".join(errors))

    def test_unregistered_stray_hex_file_fails(self) -> None:
        gate = _load_gate_module()
        with tempfile.TemporaryDirectory() as tmp:
            temp_root = self._copy_corpus(tmp)
            stray_epoch = (
                temp_root
                / "schema"
                / "golden"
                / "epochs"
                / "v1.0"
                / "nlos.sabi.Unregistered-v1.0.hex"
            )
            stray_epoch.write_text("0a00\n", encoding="ascii")

            ok, errors, _ = gate.check_golden_corpus(temp_root)
            self.assertFalse(ok)
            self.assertIn("E_UNREGISTERED_GOLDEN_FILE", "\n".join(errors))

        with tempfile.TemporaryDirectory() as tmp:
            temp_root = self._copy_corpus(tmp)
            stray_root = (
                temp_root / "schema" / "golden" / "nlos.sabi.StrayRoot-v1.hex"
            )
            stray_root.write_text("0a00\n", encoding="ascii")

            ok, errors, _ = gate.check_golden_corpus(temp_root)
            self.assertFalse(ok)
            self.assertIn("E_UNREGISTERED_GOLDEN_FILE", "\n".join(errors))

    def test_missing_one_of_nine_channels_fails(self) -> None:
        gate = _load_gate_module()
        with tempfile.TemporaryDirectory() as tmp:
            temp_root = self._copy_corpus(tmp)
            epoch_dir = temp_root / "schema" / "golden" / "epochs" / "v1.0"
            manifest_path = epoch_dir / "manifest.json"
            manifest = json.loads(manifest_path.read_text(encoding="utf-8"))

            removed_channel = "nlos.sabi.ChannelControl"
            manifest["required_channels"] = [
                ch for ch in manifest["required_channels"] if ch != removed_channel
            ]
            kept_vectors = []
            for vec in manifest["vectors"]:
                if vec["channel"] == removed_channel:
                    (epoch_dir / vec["file"]).unlink()
                else:
                    kept_vectors.append(vec)
            manifest["vectors"] = kept_vectors
            manifest_path.write_text(
                json.dumps(manifest, indent=2) + "\n", encoding="utf-8"
            )

            ok, errors, _ = gate.check_golden_corpus(temp_root)
            self.assertFalse(ok)
            joined = "\n".join(errors)
            self.assertTrue(
                "E_CHANNEL_SET_MISMATCH" in joined
                or "E_CHANNEL_COVERAGE_INCOMPLETE" in joined,
                f"unexpected errors: {errors}",
            )

    def test_git_diff_rejects_mutation_of_existing_golden_and_allows_new_epoch(
        self,
    ) -> None:
        gate = _load_gate_module()
        with tempfile.TemporaryDirectory() as tmp:
            temp_root = self._copy_corpus(tmp)
            subprocess.run(["git", "init"], cwd=temp_root, check=True, capture_output=True)
            subprocess.run(
                ["git", "config", "user.email", "test@example.com"],
                cwd=temp_root,
                check=True,
                capture_output=True,
            )
            subprocess.run(
                ["git", "config", "user.name", "Test Runner"],
                cwd=temp_root,
                check=True,
                capture_output=True,
            )
            subprocess.run(
                ["git", "add", "schema/golden"],
                cwd=temp_root,
                check=True,
                capture_output=True,
            )
            subprocess.run(
                ["git", "commit", "-m", "baseline"],
                cwd=temp_root,
                check=True,
                capture_output=True,
            )
            base_sha = subprocess.run(
                ["git", "rev-parse", "HEAD"],
                cwd=temp_root,
                check=True,
                capture_output=True,
                text=True,
            ).stdout.strip()

            # Additive new epoch directory is allowed by --check-git-diff.
            v2_dir = temp_root / "schema" / "golden" / "epochs" / "v2.0"
            v2_dir.mkdir(parents=True)
            sample_hex = "0a160a126e6c6f732e736162692e456e76656c6f70651002\n"
            sample_bytes = sample_hex.encode("ascii")
            sample_wire = bytes.fromhex(sample_hex.strip())
            (v2_dir / "nlos.sabi.Envelope-v2.0.hex").write_bytes(sample_bytes)
            v2_manifest = {
                "schema": "nlos.golden.epoch_manifest.v1",
                "epoch": "v2.0",
                "sabi_major": 2,
                "immutable": True,
                "required_channels": ["nlos.sabi.Envelope"],
                "vectors": [
                    {
                        "file": "nlos.sabi.Envelope-v2.0.hex",
                        "channel": "nlos.sabi.Envelope",
                        "message": "nlos.sabi.v1.Envelope",
                        "schema_major": 2,
                        "schema_minor": 0,
                        "byte_length": len(sample_wire),
                        "sha256_wire": hashlib.sha256(sample_wire).hexdigest(),
                        "sha256_file": hashlib.sha256(sample_bytes).hexdigest(),
                    }
                ],
            }
            (v2_dir / "manifest.json").write_text(
                json.dumps(v2_manifest, indent=2) + "\n", encoding="utf-8"
            )
            subprocess.run(
                ["git", "add", "schema/golden/epochs/v2.0"],
                cwd=temp_root,
                check=True,
                capture_output=True,
            )
            ok, errors, _ = gate.check_golden_corpus(
                temp_root, check_git_diff_base=base_sha
            )
            self.assertTrue(ok, f"additive epoch should pass git diff gate: {errors}")

            # Even if an attacker rewrites both an existing .hex AND manifest.json,
            # --check-git-diff catches the mutation of an already-committed golden!
            v1_manifest_path = (
                temp_root / "schema" / "golden" / "epochs" / "v1.0" / "manifest.json"
            )
            v1_manifest = json.loads(v1_manifest_path.read_text(encoding="utf-8"))
            tampered_vec = v1_manifest["vectors"][0]
            tampered_file = (
                temp_root
                / "schema"
                / "golden"
                / "epochs"
                / "v1.0"
                / tampered_vec["file"]
            )
            new_hex = "0a00\n"
            new_file_bytes = new_hex.encode("ascii")
            new_wire_bytes = bytes.fromhex("0a00")
            tampered_file.write_bytes(new_file_bytes)
            tampered_vec["byte_length"] = len(new_wire_bytes)
            tampered_vec["sha256_wire"] = hashlib.sha256(new_wire_bytes).hexdigest()
            tampered_vec["sha256_file"] = hashlib.sha256(new_file_bytes).hexdigest()
            v1_manifest_path.write_text(
                json.dumps(v1_manifest, indent=2) + "\n", encoding="utf-8"
            )

            ok_mut, errors_mut, _ = gate.check_golden_corpus(
                temp_root, check_git_diff_base=base_sha
            )
            self.assertFalse(ok_mut)
            self.assertIn("E_GIT_DIFF_IMMUTABLE_VIOLATION", "\n".join(errors_mut))


if __name__ == "__main__":
    unittest.main()
