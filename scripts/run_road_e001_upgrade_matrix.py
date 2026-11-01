#!/usr/bin/env python3
"""Stage E E7 (W81-E7A, [ROAD-E-001]) Reproducible Upgrade Matrix Runner.

Mechanically verifies:
1. Pre-run SHA-256 and byte-length snapshot of all 16 committed sealed `N-1`
   baseline files across the 2 third-party multi-component Applications
   (`knowledge-observatory-app`, `field-survey-app`) and 2 third-party userland
   Drivers (`mcp-bridge-driver`, `openai-compat-driver`).
2. Cross-check of every frozen `v1.0.0.nlos-pkg` and `v1.1.0.nlos-pkg` against
   its committed `.v1.0.0.store-seal.txt` governance seal (`artifact = ...` and
   `# artifact = ...` lines) and canonical frozen digest table.
3. Ecosystem `Cargo.toml` manifest purity audit (`[KABI-SVC-001]`,
   `[SABI-CONFORM-001]`, `[DRV-PROTO-001]`): zero private kernel crate
   dependencies in `examples/ecosystem-apps/**` or `examples/ecosystem-drivers/**`,
   and zero internal host SQLite authority crate dependencies in userland drivers.
4. Execution of the detached standalone Cargo upgrade gate workspace:
   `cargo test --manifest-path tests/stage_e_road_e001_upgrade_gate/Cargo.toml`
5. Post-run SHA-256 and byte-length snapshot equality (`pre == post`,
   `zero_recompile_verified == True`): 0 bytes modified, recompiled, or
   re-signed across all 16 sealed `N-1` baseline files.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import shutil
import subprocess
import sys
import tempfile
import time
import unittest
from pathlib import Path
from typing import Any

GATE_MANIFEST_REL = "tests/stage_e_road_e001_upgrade_gate/Cargo.toml"

FROZEN_SAMPLES: tuple[dict[str, Any], ...] = (
    {
        "label": "knowledge-observatory-app",
        "kind": "application",
        "sample_dir": "examples/ecosystem-apps/knowledge-observatory-app",
        "v1_0_0_pkg": (
            "examples/ecosystem-apps/knowledge-observatory-app/sealed/"
            "knowledge-observatory-app.v1.0.0.nlos-pkg"
        ),
        "v1_0_0_sha256": "690c4e296bf3b0bedd56b17592064c04e3eac8defcf14d4d1e988e0daf4602d5",
        "v1_0_0_bytes": 2538,
        "v1_1_0_pkg": (
            "examples/ecosystem-apps/knowledge-observatory-app/sealed/"
            "knowledge-observatory-app.v1.1.0.nlos-pkg"
        ),
        "v1_1_0_sha256": "0bc978a23cc62571503bdd939aef6be76f692b5f2d61e26bb227cb0398191985",
        "v1_1_0_bytes": 2828,
        "certify_json": (
            "examples/ecosystem-apps/knowledge-observatory-app/sealed/"
            "knowledge-observatory-app.v1.0.0.certify.json"
        ),
        "store_seal": (
            "examples/ecosystem-apps/knowledge-observatory-app/sealed/"
            "knowledge-observatory-app.v1.0.0.store-seal.txt"
        ),
    },
    {
        "label": "field-survey-app",
        "kind": "application",
        "sample_dir": "examples/ecosystem-apps/field-survey-app",
        "v1_0_0_pkg": (
            "examples/ecosystem-apps/field-survey-app/sealed/"
            "field-survey-app.v1.0.0.nlos-pkg"
        ),
        "v1_0_0_sha256": "4e529d61239bec8bf289993fd7294991207842fc77620ed7cefa0f19f9d1c4eb",
        "v1_0_0_bytes": 2341,
        "v1_1_0_pkg": (
            "examples/ecosystem-apps/field-survey-app/sealed/"
            "field-survey-app.v1.1.0.nlos-pkg"
        ),
        "v1_1_0_sha256": "59f219d130fd1845c9d307e6e2e799ca11dbf8e1fe3d05b5f663e16d292111c2",
        "v1_1_0_bytes": 2618,
        "certify_json": (
            "examples/ecosystem-apps/field-survey-app/sealed/"
            "field-survey-app.v1.0.0.certify.json"
        ),
        "store_seal": (
            "examples/ecosystem-apps/field-survey-app/sealed/"
            "field-survey-app.v1.0.0.store-seal.txt"
        ),
    },
    {
        "label": "mcp-bridge-driver",
        "kind": "driver",
        "sample_dir": "examples/ecosystem-drivers/mcp-bridge-driver",
        "v1_0_0_pkg": (
            "examples/ecosystem-drivers/mcp-bridge-driver/sealed/"
            "mcp-bridge-driver.v1.0.0.nlos-pkg"
        ),
        "v1_0_0_sha256": "6bff8ec8aeaff6d478eabbb6dbca97e062660fb711b719ff13ec9c67ec1fe83d",
        "v1_0_0_bytes": 2327,
        "v1_1_0_pkg": (
            "examples/ecosystem-drivers/mcp-bridge-driver/sealed/"
            "mcp-bridge-driver.v1.1.0.nlos-pkg"
        ),
        "v1_1_0_sha256": "9a6ec0ebb328a5064b8781a7996f87cffb64dd35b1e6acf2fab8c52881fa525b",
        "v1_1_0_bytes": 2778,
        "certify_json": (
            "examples/ecosystem-drivers/mcp-bridge-driver/sealed/"
            "mcp-bridge-driver.v1.0.0.certify.json"
        ),
        "store_seal": (
            "examples/ecosystem-drivers/mcp-bridge-driver/sealed/"
            "mcp-bridge-driver.v1.0.0.store-seal.txt"
        ),
    },
    {
        "label": "openai-compat-driver",
        "kind": "driver",
        "sample_dir": "examples/ecosystem-drivers/openai-compat-driver",
        "v1_0_0_pkg": (
            "examples/ecosystem-drivers/openai-compat-driver/sealed/"
            "openai-compat-driver.v1.0.0.nlos-pkg"
        ),
        "v1_0_0_sha256": "db0d93d156dd07f6efc937b891dfc97bf9b363aeb0c28081382d22e1e68be0e1",
        "v1_0_0_bytes": 2080,
        "v1_1_0_pkg": (
            "examples/ecosystem-drivers/openai-compat-driver/sealed/"
            "openai-compat-driver.v1.1.0.nlos-pkg"
        ),
        "v1_1_0_sha256": "0467b105904b8b013524c880eef32d33d1fc7fe3f13a661dd6fadfb41940de09",
        "v1_1_0_bytes": 2413,
        "certify_json": (
            "examples/ecosystem-drivers/openai-compat-driver/sealed/"
            "openai-compat-driver.v1.0.0.certify.json"
        ),
        "store_seal": (
            "examples/ecosystem-drivers/openai-compat-driver/sealed/"
            "openai-compat-driver.v1.0.0.store-seal.txt"
        ),
    },
)

FORBIDDEN_KERNEL_CRATES: tuple[str, ...] = (
    "nlos-kernel",
    "nlos-kabi",
    "nlos-hal",
    "nlos-mm",
    "nlos-sched",
    "nlos-arch",
    "nlos-boot",
    "nlos-wasm-sandbox",
    "nlos-compat-shim",
    "nlos-store-registry",
    "nlos-publisher-trust",
    "nlos-certify",
)

FORBIDDEN_DRIVER_HOST_AUTHORITY_CRATES: tuple[str, ...] = (
    "nlos-identity",
    "nlos-capability",
    "nlos-task",
    "nlos-plan",
    "nlos-application",
    "nlos-slice-k",
)

ECOSYSTEM_MANIFESTS: tuple[tuple[str, bool], ...] = (
    ("examples/ecosystem-apps/Cargo.toml", False),
    ("examples/ecosystem-apps/knowledge-observatory-app/Cargo.toml", False),
    ("examples/ecosystem-apps/field-survey-app/Cargo.toml", False),
    ("examples/ecosystem-drivers/Cargo.toml", True),
    ("examples/ecosystem-drivers/ecosystem-driver-kit/Cargo.toml", True),
    ("examples/ecosystem-drivers/mcp-bridge-driver/Cargo.toml", True),
    ("examples/ecosystem-drivers/openai-compat-driver/Cargo.toml", True),
)

TEST_RESULT_RE = re.compile(
    r"test result:\s+(ok|FAILED)\.\s+(\d+)\s+passed;\s+(\d+)\s+failed;\s+(\d+)\s+ignored;"
)


def repo_root() -> Path:
    return Path(__file__).resolve().parent.parent


def all_sealed_relative_paths() -> list[str]:
    paths: list[str] = []
    for sample in FROZEN_SAMPLES:
        paths.append(sample["v1_0_0_pkg"])
        paths.append(sample["v1_1_0_pkg"])
        paths.append(sample["certify_json"])
        paths.append(sample["store_seal"])
    return paths


def sha256_file(path: Path) -> tuple[str, int]:
    data = path.read_bytes()
    return hashlib.sha256(data).hexdigest(), len(data)


def capture_sealed_snapshot(
    root: Path, rel_paths: list[str] | None = None
) -> dict[str, dict[str, Any]]:
    target_paths = rel_paths if rel_paths is not None else all_sealed_relative_paths()
    snapshot: dict[str, dict[str, Any]] = {}
    for rel in target_paths:
        full = root / rel
        if not full.is_file():
            raise FileNotFoundError(f"missing sealed N-1 baseline file: {rel}")
        digest, size_bytes = sha256_file(full)
        snapshot[rel] = {"sha256": digest, "bytes": size_bytes}
    return snapshot


def parse_store_seal_artifacts(seal_text: str) -> list[dict[str, Any]]:
    """Parses both `artifact = ...` and `# artifact = ...` lines in `.store-seal.txt`."""
    entries: list[dict[str, Any]] = []
    for raw_line in seal_text.splitlines():
        line = raw_line.strip()
        if not line:
            continue
        if line.startswith("#"):
            uncommented = line.lstrip("#").strip()
            if uncommented.startswith("artifact =") or uncommented.startswith("artifact="):
                line = uncommented
            else:
                continue
        if not (line.startswith("artifact =") or line.startswith("artifact=")):
            continue
        _, rhs = line.split("=", 1)
        fields: dict[str, str] = {}
        for token in rhs.strip().split():
            if "=" in token:
                k, v = token.split("=", 1)
                fields[k.strip()] = v.strip()
            elif "path" not in fields:
                fields["path"] = token.strip()
        if "path" not in fields or "sha256" not in fields or "bytes" not in fields:
            raise ValueError(f"malformed artifact seal line: {raw_line!r}")
        entries.append(
            {
                "path": fields["path"].replace("\\", "/"),
                "sha256": fields["sha256"].lower(),
                "bytes": int(fields["bytes"]),
                "tree": fields.get("tree", ""),
            }
        )
    if len(entries) < 2:
        raise ValueError(
            f"expected >= 2 artifact lines (v1.0.0 + v1.1.0) in store-seal.txt, found {len(entries)}"
        )
    return entries


def verify_seals_against_snapshot(
    root: Path,
    snapshot: dict[str, dict[str, Any]],
    samples: tuple[dict[str, Any], ...] = FROZEN_SAMPLES,
) -> list[str]:
    """Cross-checks `.store-seal.txt` and frozen digest constants against `snapshot`."""
    errors: list[str] = []
    for sample in samples:
        seal_rel = sample["store_seal"]
        seal_path = root / seal_rel
        if not seal_path.is_file():
            errors.append(f"missing store seal file: {seal_rel}")
            continue
        try:
            parsed_artifacts = parse_store_seal_artifacts(
                seal_path.read_text(encoding="utf-8")
            )
        except Exception as exc:  # noqa: BLE001
            errors.append(f"{seal_rel}: failed to parse seal ({exc})")
            continue

        by_path = {item["path"]: item for item in parsed_artifacts}
        for ver_key, sha_key, bytes_key in (
            ("v1_0_0_pkg", "v1_0_0_sha256", "v1_0_0_bytes"),
            ("v1_1_0_pkg", "v1_1_0_sha256", "v1_1_0_bytes"),
        ):
            pkg_rel = sample[ver_key]
            expected_sha = sample[sha_key]
            expected_bytes = sample[bytes_key]
            snap_entry = snapshot.get(pkg_rel)
            if snap_entry is None:
                errors.append(f"snapshot missing {pkg_rel}")
                continue
            if snap_entry["sha256"] != expected_sha or snap_entry["bytes"] != expected_bytes:
                errors.append(
                    f"frozen baseline mismatch on {pkg_rel}: "
                    f"expected ({expected_sha}, {expected_bytes}B), "
                    f"got ({snap_entry['sha256']}, {snap_entry['bytes']}B)"
                )
            seal_item = by_path.get(pkg_rel)
            if seal_item is None:
                errors.append(f"{seal_rel} missing artifact entry for {pkg_rel}")
                continue
            if (
                seal_item["sha256"] != snap_entry["sha256"]
                or seal_item["bytes"] != snap_entry["bytes"]
            ):
                errors.append(
                    f"{seal_rel} digest mismatch on {pkg_rel}: "
                    f"seal=({seal_item['sha256']}, {seal_item['bytes']}B) != "
                    f"disk=({snap_entry['sha256']}, {snap_entry['bytes']}B)"
                )
    return errors


def verify_zero_drift(
    pre: dict[str, dict[str, Any]], post: dict[str, dict[str, Any]]
) -> list[str]:
    drifted: list[str] = []
    all_keys = sorted(set(pre.keys()) | set(post.keys()))
    for key in all_keys:
        if key not in pre or key not in post:
            drifted.append(key)
            continue
        if pre[key] != post[key]:
            drifted.append(key)
    return drifted


def audit_manifest_text_purity(manifest_text: str, is_driver: bool) -> list[str]:
    violations: list[str] = []
    for line_no, raw_line in enumerate(manifest_text.splitlines(), start=1):
        stripped = raw_line.strip()
        if not stripped or stripped.startswith("#"):
            continue
        for forbidden in FORBIDDEN_KERNEL_CRATES:
            if forbidden in stripped:
                violations.append(
                    f"line {line_no}: forbidden private kernel dependency {forbidden!r}"
                )
        if is_driver:
            for forbidden in FORBIDDEN_DRIVER_HOST_AUTHORITY_CRATES:
                if forbidden in stripped:
                    violations.append(
                        f"line {line_no}: forbidden host authority dependency {forbidden!r} in userland driver"
                    )
    return violations


def audit_workspace_ecosystem_manifests(root: Path) -> list[str]:
    violations: list[str] = []
    for rel_manifest, is_driver in ECOSYSTEM_MANIFESTS:
        manifest_path = root / rel_manifest
        if not manifest_path.is_file():
            violations.append(f"missing ecosystem Cargo.toml: {rel_manifest}")
            continue
        text = manifest_path.read_text(encoding="utf-8")
        for item in audit_manifest_text_purity(text, is_driver=is_driver):
            violations.append(f"{rel_manifest}: {item}")
    return violations


def run_cargo_upgrade_gate(root: Path) -> dict[str, Any]:
    cmd = [
        "cargo",
        "test",
        "--manifest-path",
        GATE_MANIFEST_REL,
    ]
    t0 = time.monotonic()
    proc = subprocess.run(
        cmd,
        cwd=str(root),
        capture_output=True,
        text=True,
        check=False,
    )
    elapsed_ms = int((time.monotonic() - t0) * 1000)
    combined = f"{proc.stdout}\n{proc.stderr}"
    passed_total = 0
    failed_total = 0
    ignored_total = 0
    suites_seen = 0
    for match in TEST_RESULT_RE.finditer(combined):
        suites_seen += 1
        passed_total += int(match.group(2))
        failed_total += int(match.group(3))
        ignored_total += int(match.group(4))
    return {
        "command": " ".join(cmd),
        "returncode": proc.returncode,
        "elapsed_ms": elapsed_ms,
        "suites_seen": suites_seen,
        "passed_tests": passed_total,
        "failed_tests": failed_total,
        "ignored_tests": ignored_total,
        "stdout": proc.stdout,
        "stderr": proc.stderr,
    }


def execute_upgrade_matrix(root: Path) -> tuple[int, dict[str, Any]]:
    pre_snapshot = capture_sealed_snapshot(root)
    pre_seal_errors = verify_seals_against_snapshot(root, pre_snapshot)
    manifest_violations = audit_workspace_ecosystem_manifests(root)
    cargo_outcome = run_cargo_upgrade_gate(root)
    post_snapshot = capture_sealed_snapshot(root)
    drifted_files = verify_zero_drift(pre_snapshot, post_snapshot)
    post_seal_errors = verify_seals_against_snapshot(root, post_snapshot)

    zero_recompile_verified = (
        len(drifted_files) == 0
        and len(pre_seal_errors) == 0
        and len(post_seal_errors) == 0
    )
    cargo_passed = (
        cargo_outcome["returncode"] == 0
        and cargo_outcome["failed_tests"] == 0
        and cargo_outcome["passed_tests"] >= 6
    )
    overall_ok = (
        zero_recompile_verified
        and len(manifest_violations) == 0
        and cargo_passed
    )

    frozen_packages_summary = []
    for sample in FROZEN_SAMPLES:
        for ver_label, pkg_key in (("v1.0.0", "v1_0_0_pkg"), ("v1.1.0", "v1_1_0_pkg")):
            rel = sample[pkg_key]
            frozen_packages_summary.append(
                {
                    "sample": sample["label"],
                    "kind": sample["kind"],
                    "version": ver_label,
                    "path": rel,
                    "pre_sha256": pre_snapshot[rel]["sha256"],
                    "post_sha256": post_snapshot[rel]["sha256"],
                    "bytes": post_snapshot[rel]["bytes"],
                    "unchanged": pre_snapshot[rel] == post_snapshot[rel],
                }
            )

    report: dict[str, Any] = {
        "gate_id": "ROAD-E-001",
        "wave_task": "W81-E7A",
        "status": "PASS" if overall_ok else "FAIL",
        "host_upgrade_transition": "N-1 (epoch=1) -> N (epoch=2)",
        "sealed_baseline_file_count": len(pre_snapshot),
        "frozen_nlos_pkg_count": len(frozen_packages_summary),
        "zero_recompile_verified": zero_recompile_verified,
        "drifted_files": drifted_files,
        "pre_seal_errors": pre_seal_errors,
        "post_seal_errors": post_seal_errors,
        "manifest_purity_violations": manifest_violations,
        "cargo_gate": {
            "command": cargo_outcome["command"],
            "returncode": cargo_outcome["returncode"],
            "elapsed_ms": cargo_outcome["elapsed_ms"],
            "suites_seen": cargo_outcome["suites_seen"],
            "passed_tests": cargo_outcome["passed_tests"],
            "failed_tests": cargo_outcome["failed_tests"],
            "ignored_tests": cargo_outcome["ignored_tests"],
        },
        "frozen_packages": frozen_packages_summary,
    }
    if not cargo_passed:
        report["cargo_stdout_tail"] = cargo_outcome["stdout"][-4000:]
        report["cargo_stderr_tail"] = cargo_outcome["stderr"][-4000:]
    return (0 if overall_ok else 1), report


def render_human_report(report: dict[str, Any]) -> str:
    lines = [
        "========================================================================",
        "NLOS STAGE E [ROAD-E-001] ZERO-RECOMPILE HOST UPGRADE GATE MATRIX",
        "========================================================================",
        f"Status                  : {report['status']}",
        f"Wave / Task             : {report['wave_task']}",
        f"Host Epoch Transition   : {report['host_upgrade_transition']}",
        f"Sealed Baseline Files   : {report['sealed_baseline_file_count']}",
        f"Frozen .nlos-pkg Count  : {report['frozen_nlos_pkg_count']}",
        f"Zero-Recompile Verified : {report['zero_recompile_verified']}",
        f"Manifest Violations     : {len(report['manifest_purity_violations'])}",
        (
            "Cargo Gate              : "
            f"rc={report['cargo_gate']['returncode']}, "
            f"passed={report['cargo_gate']['passed_tests']}, "
            f"failed={report['cargo_gate']['failed_tests']}, "
            f"elapsed_ms={report['cargo_gate']['elapsed_ms']}"
        ),
        "------------------------------------------------------------------------",
        "Frozen N-1 Package Digests (pre == post):",
    ]
    for pkg in report["frozen_packages"]:
        flag = "UNCHANGED" if pkg["unchanged"] else "DRIFTED"
        lines.append(
            f"  [{flag}] {pkg['sample']} {pkg['version']} "
            f"({pkg['bytes']} B) sha256={pkg['post_sha256']}"
        )
    if report["drifted_files"]:
        lines.append(f"Drifted files: {report['drifted_files']}")
    if report["pre_seal_errors"]:
        lines.append(f"Pre-run seal errors: {report['pre_seal_errors']}")
    if report["post_seal_errors"]:
        lines.append(f"Post-run seal errors: {report['post_seal_errors']}")
    if report["manifest_purity_violations"]:
        lines.append(f"Manifest violations: {report['manifest_purity_violations']}")
    if "cargo_stdout_tail" in report:
        lines.append("--- cargo stdout (tail) ---")
        lines.append(report["cargo_stdout_tail"])
    if "cargo_stderr_tail" in report:
        lines.append("--- cargo stderr (tail) ---")
        lines.append(report["cargo_stderr_tail"])
    lines.append("========================================================================")
    return "\n".join(lines)


class RoadE001MatrixSelfTests(unittest.TestCase):
    """Deterministic unit tests for seal verification, tamper detection, and purity audits."""

    def test_live_16_file_seal_cross_check_passes(self) -> None:
        root = repo_root()
        snap = capture_sealed_snapshot(root)
        self.assertEqual(len(snap), 16)
        self.assertEqual(verify_seals_against_snapshot(root, snap), [])
        self.assertEqual(verify_zero_drift(snap, snap), [])

    def test_seal_parser_parses_active_and_commented_v1_1_artifact_lines(self) -> None:
        root = repo_root()
        for sample in FROZEN_SAMPLES:
            seal_text = (root / sample["store_seal"]).read_text(encoding="utf-8")
            parsed = parse_store_seal_artifacts(seal_text)
            paths = {entry["path"]: entry for entry in parsed}
            self.assertIn(sample["v1_0_0_pkg"], paths)
            self.assertIn(sample["v1_1_0_pkg"], paths)
            self.assertEqual(paths[sample["v1_0_0_pkg"]]["sha256"], sample["v1_0_0_sha256"])
            self.assertEqual(paths[sample["v1_1_0_pkg"]]["sha256"], sample["v1_1_0_sha256"])

    def test_one_byte_package_tamper_or_resign_is_detected_on_temp_copy(self) -> None:
        root = repo_root()
        sample = FROZEN_SAMPLES[0]
        sample_paths = [
            sample["v1_0_0_pkg"],
            sample["v1_1_0_pkg"],
            sample["certify_json"],
            sample["store_seal"],
        ]
        with tempfile.TemporaryDirectory(prefix="road-e001-selftest-") as tmp:
            tmp_root = Path(tmp)
            for rel in sample_paths:
                dst = tmp_root / rel
                dst.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(root / rel, dst)

            pre = capture_sealed_snapshot(tmp_root, sample_paths)
            self.assertEqual(
                verify_seals_against_snapshot(tmp_root, pre, samples=(sample,)), []
            )

            # Flip 1 byte in the temp copy of `v1.0.0.nlos-pkg` (simulating silent rebuild/re-sign).
            pkg_file = tmp_root / sample["v1_0_0_pkg"]
            mutated = bytearray(pkg_file.read_bytes())
            mutated[-1] ^= 0x01
            pkg_file.write_bytes(bytes(mutated))

            post = capture_sealed_snapshot(tmp_root, sample_paths)
            drifted = verify_zero_drift(pre, post)
            self.assertEqual(drifted, [sample["v1_0_0_pkg"]])
            seal_errors = verify_seals_against_snapshot(tmp_root, post, samples=(sample,))
            self.assertGreaterEqual(len(seal_errors), 1)

    def test_one_byte_store_seal_tamper_is_detected_on_temp_copy(self) -> None:
        root = repo_root()
        sample = FROZEN_SAMPLES[2]
        sample_paths = [
            sample["v1_0_0_pkg"],
            sample["v1_1_0_pkg"],
            sample["certify_json"],
            sample["store_seal"],
        ]
        with tempfile.TemporaryDirectory(prefix="road-e001-seal-tamper-") as tmp:
            tmp_root = Path(tmp)
            for rel in sample_paths:
                dst = tmp_root / rel
                dst.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(root / rel, dst)

            seal_file = tmp_root / sample["store_seal"]
            original_text = seal_file.read_text(encoding="utf-8")
            replacement_sha = "f" * 64
            tampered_text = original_text.replace(sample["v1_0_0_sha256"], replacement_sha, 1)
            seal_file.write_text(tampered_text, encoding="utf-8")

            snap = capture_sealed_snapshot(tmp_root, sample_paths)
            seal_errors = verify_seals_against_snapshot(tmp_root, snap, samples=(sample,))
            self.assertEqual(len(seal_errors), 1)
            self.assertIn("digest mismatch", seal_errors[0])

    def test_ecosystem_manifest_purity_audit_passes_live_and_catches_kernel_dep(self) -> None:
        root = repo_root()
        self.assertEqual(audit_workspace_ecosystem_manifests(root), [])

        bad_app_toml = '[dependencies]\nnlos-kabi = { path = "../../crates/nlos-kabi" }\n'
        app_violations = audit_manifest_text_purity(bad_app_toml, is_driver=False)
        self.assertEqual(len(app_violations), 1)
        self.assertIn("nlos-kabi", app_violations[0])

        bad_drv_toml = (
            '[dependencies]\nnlos-capability = { path = "../../crates/nlos-capability" }\n'
        )
        drv_violations = audit_manifest_text_purity(bad_drv_toml, is_driver=True)
        self.assertEqual(len(drv_violations), 1)
        self.assertIn("nlos-capability", drv_violations[0])


def run_self_tests() -> int:
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(RoadE001MatrixSelfTests)
    runner = unittest.TextTestRunner(stream=sys.stdout, verbosity=2)
    result = runner.run(suite)
    return 0 if result.wasSuccessful() else 1


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Run the Stage E [ROAD-E-001] Host N-1 -> N zero-recompile upgrade gate matrix."
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="Emit machine-readable JSON summary to stdout.",
    )
    parser.add_argument(
        "--self-test",
        action="store_true",
        help="Run built-in unittest suite for seal verification, tamper detection, and purity audits.",
    )
    args = parser.parse_args(argv)

    if args.self_test:
        return run_self_tests()

    rc, report = execute_upgrade_matrix(repo_root())
    if args.json:
        print(json.dumps(report, indent=2, sort_keys=False))
    else:
        print(render_human_report(report))
    return rc


if __name__ == "__main__":
    sys.exit(main())
