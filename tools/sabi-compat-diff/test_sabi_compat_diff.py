#!/usr/bin/env python3
"""Unit and subprocess integration tests for `sabi_compat_diff.py` (`W78-E3`)."""

from __future__ import annotations

import json
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

TOOLS_DIR = Path(__file__).resolve().parent
REPO_ROOT = TOOLS_DIR.parents[1]
CANONICAL_SCHEMA_DIR = REPO_ROOT / "schema" / "nlos" / "sabi" / "v1"
SCRIPT_PATH = TOOLS_DIR / "sabi_compat_diff.py"

sys.path.insert(0, str(TOOLS_DIR))
import sabi_compat_diff  # noqa: E402


MINIMAL_PROTO_TEMPLATE = """\
syntax = "proto3";
package nlos.sabi.v1;

enum SampleMode {{
  SAMPLE_MODE_UNSPECIFIED = 0;
  SAMPLE_MODE_FAST = 1;
}}

message SampleRequest {{
  string request_token = 1;
  uint32 timeout_ms = 2;
  SampleMode mode = 3;
{extra_fields}
}}

message SampleResponse {{
  bool accepted = 1;
}}

service SampleChannelService {{
  rpc ExecuteSample(SampleRequest) returns (SampleResponse);
{extra_rpcs}
}}
"""

MINIMAL_MANIFEST_TEMPLATE = """\
manifest_version: 1
current_epoch: {epoch}
min_compatibility_window_epochs: 2
frozen_channels_total: 1
channels:
  - name: nlos.sabi.Sample
    major: {major}
    minor: {minor}
    frozen: {frozen}
    reserved_field_numbers: {reserved_nums}
    reserved_symbols: {reserved_syms}
{entries_block}
"""


def _write_surface(
    target_dir: Path,
    *,
    proto_text: str,
    manifest_text: str,
    proto_name: str = "sample.proto",
) -> tuple[Path, Path]:
    target_dir.mkdir(parents=True, exist_ok=True)
    proto_path = target_dir / proto_name
    manifest_path = target_dir / "deprecation-manifest.yaml"
    proto_path.write_text(proto_text, encoding="utf-8")
    manifest_path.write_text(manifest_text, encoding="utf-8")
    return target_dir, manifest_path


class SabiCompatDiffTests(unittest.TestCase):
    """Positive and negative regression suite for ADR-0014 / ADR-0027 static gates."""

    def test_01_self_check_canonical_sabi_v1_passes(self) -> None:
        report = sabi_compat_diff.run_self_check(CANONICAL_SCHEMA_DIR)
        self.assertTrue(report.passed, msg=f"unexpected errors: {report.errors}")
        self.assertEqual(len(report.errors), 0)
        self.assertEqual(report.summary["baseline_proto_files"], 9)
        self.assertEqual(report.summary["baseline_channels"], 9)
        self.assertGreaterEqual(report.summary["baseline_messages"], 100)

    def test_02_compliant_additive_proto_evolutions_pass(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            base_dir = root / "base"
            cand_dir = root / "cand"
            shutil.copytree(CANONICAL_SCHEMA_DIR, base_dir)
            shutil.copytree(CANONICAL_SCHEMA_DIR, cand_dir)

            envelope_proto = cand_dir / "envelope.proto"
            text = envelope_proto.read_text(encoding="utf-8")
            text += (
                "\nmessage AdditiveAuditHint {\n"
                "  string hint_id = 1;\n"
                "  optional uint64 sequence = 2;\n"
                "}\n"
            )
            envelope_proto.write_text(text, encoding="utf-8")

            report = sabi_compat_diff.compare_contract_surfaces(
                base_dir,
                base_dir / "deprecation-manifest.yaml",
                cand_dir,
                cand_dir / "deprecation-manifest.yaml",
            )
            self.assertTrue(report.passed, msg=f"unexpected errors: {report.errors}")
            self.assertEqual(
                report.summary["candidate_messages"],
                report.summary["baseline_messages"] + 1,
            )

    def test_03_compliant_additive_field_and_rpc_pass(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            base_proto = MINIMAL_PROTO_TEMPLATE.format(extra_fields="", extra_rpcs="")
            base_manifest = MINIMAL_MANIFEST_TEMPLATE.format(
                epoch=1,
                major=1,
                minor=0,
                frozen="true",
                reserved_nums="[]",
                reserved_syms="[]",
                entries_block="entries: []",
            )
            cand_proto = MINIMAL_PROTO_TEMPLATE.format(
                extra_fields="  optional string trace_context = 4;",
                extra_rpcs="  rpc InspectSample(SampleRequest) returns (SampleResponse);",
            )
            cand_manifest = MINIMAL_MANIFEST_TEMPLATE.format(
                epoch=1,
                major=1,
                minor=1,
                frozen="true",
                reserved_nums="[]",
                reserved_syms="[]",
                entries_block="entries: []",
            )
            b_dir, b_man = _write_surface(
                root / "base", proto_text=base_proto, manifest_text=base_manifest
            )
            c_dir, c_man = _write_surface(
                root / "cand", proto_text=cand_proto, manifest_text=cand_manifest
            )

            report = sabi_compat_diff.compare_contract_surfaces(
                b_dir, b_man, c_dir, c_man
            )
            self.assertTrue(report.passed, msg=f"unexpected errors: {report.errors}")
            info_codes = {item.code for item in report.infos}
            self.assertIn("I_ADDITIVE_FIELD", info_codes)
            self.assertIn("I_ADDITIVE_RPC", info_codes)

    def test_04_compliant_active_to_deprecated_transition_passes(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            proto_src = MINIMAL_PROTO_TEMPLATE.format(extra_fields="", extra_rpcs="")
            base_entries = """\
entries:
  - channel: nlos.sabi.Sample
    symbol: SampleRequest.timeout_ms
    kind: FIELD
    parent_type: SampleRequest
    field_number: 2
    introduced_in_epoch: 1
    state: ACTIVE
"""
            cand_entries = """\
entries:
  - channel: nlos.sabi.Sample
    symbol: SampleRequest.timeout_ms
    kind: FIELD
    parent_type: SampleRequest
    field_number: 2
    introduced_in_epoch: 1
    state: DEPRECATED
    deprecated_in_epoch: 2
    earliest_sunset_epoch: 4
    replacement: SampleRequest.deadline_monotonic_ns
    migration_rule_id: MIG-SAMPLE-001
    reason: Monotonic deadline replaces relative timeout_ms
"""
            b_dir, b_man = _write_surface(
                root / "base",
                proto_text=proto_src,
                manifest_text=MINIMAL_MANIFEST_TEMPLATE.format(
                    epoch=1,
                    major=1,
                    minor=0,
                    frozen="true",
                    reserved_nums="[]",
                    reserved_syms="[]",
                    entries_block=base_entries,
                ),
            )
            c_dir, c_man = _write_surface(
                root / "cand",
                proto_text=proto_src,
                manifest_text=MINIMAL_MANIFEST_TEMPLATE.format(
                    epoch=2,
                    major=2,
                    minor=0,
                    frozen="true",
                    reserved_nums="[]",
                    reserved_syms="[]",
                    entries_block=cand_entries,
                ),
            )
            report = sabi_compat_diff.compare_contract_surfaces(
                b_dir, b_man, c_dir, c_man
            )
            self.assertTrue(report.passed, msg=f"unexpected errors: {report.errors}")
            self.assertIn(
                "I_TRANSITION_ACTIVE_TO_DEPRECATED",
                {item.code for item in report.infos},
            )

    def test_05_compliant_full_window_deprecated_to_sunset_with_reserved_passes(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            base_proto = MINIMAL_PROTO_TEMPLATE.format(
                extra_fields="  uint64 deadline_monotonic_ns = 4;",
                extra_rpcs="",
            )
            cand_proto = """\
syntax = "proto3";
package nlos.sabi.v1;

enum SampleMode {
  SAMPLE_MODE_UNSPECIFIED = 0;
  SAMPLE_MODE_FAST = 1;
}

message SampleRequest {
  reserved 2;
  reserved "timeout_ms";
  string request_token = 1;
  SampleMode mode = 3;
  uint64 deadline_monotonic_ns = 4;
}

message SampleResponse {
  bool accepted = 1;
}

service SampleChannelService {
  rpc ExecuteSample(SampleRequest) returns (SampleResponse);
}
"""
            base_entries = """\
entries:
  - channel: nlos.sabi.Sample
    symbol: SampleRequest.timeout_ms
    kind: FIELD
    parent_type: SampleRequest
    field_number: 2
    introduced_in_epoch: 1
    state: DEPRECATED
    deprecated_in_epoch: 2
    earliest_sunset_epoch: 4
    replacement: SampleRequest.deadline_monotonic_ns
    migration_rule_id: MIG-SAMPLE-001
    reason: Monotonic deadline replaces relative timeout_ms
"""
            cand_entries = """\
entries:
  - channel: nlos.sabi.Sample
    symbol: SampleRequest.timeout_ms
    kind: FIELD
    parent_type: SampleRequest
    field_number: 2
    introduced_in_epoch: 1
    state: SUNSET
    deprecated_in_epoch: 2
    earliest_sunset_epoch: 4
    sunset_in_epoch: 4
    replacement: SampleRequest.deadline_monotonic_ns
    migration_rule_id: MIG-SAMPLE-001
    reason: Monotonic deadline replaces relative timeout_ms
"""
            b_dir, b_man = _write_surface(
                root / "base",
                proto_text=base_proto,
                manifest_text=MINIMAL_MANIFEST_TEMPLATE.format(
                    epoch=2,
                    major=2,
                    minor=0,
                    frozen="true",
                    reserved_nums="[]",
                    reserved_syms="[]",
                    entries_block=base_entries,
                ),
            )
            c_dir, c_man = _write_surface(
                root / "cand",
                proto_text=cand_proto,
                manifest_text=MINIMAL_MANIFEST_TEMPLATE.format(
                    epoch=4,
                    major=4,
                    minor=0,
                    frozen="true",
                    reserved_nums="[2]",
                    reserved_syms='["SampleRequest.timeout_ms"]',
                    entries_block=cand_entries,
                ),
            )
            report = sabi_compat_diff.compare_contract_surfaces(
                b_dir, b_man, c_dir, c_man
            )
            self.assertTrue(report.passed, msg=f"unexpected errors: {report.errors}")
            info_codes = {item.code for item in report.infos}
            self.assertIn("I_RETIRED_FIELD_RESERVED", info_codes)
            self.assertIn("I_TRANSITION_DEPRECATED_TO_SUNSET", info_codes)

    def test_06_deleting_message_enum_service_or_rpc_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            base_proto = MINIMAL_PROTO_TEMPLATE.format(
                extra_fields="",
                extra_rpcs="  rpc SecondaryCall(SampleRequest) returns (SampleResponse);",
            )
            cand_proto = """\
syntax = "proto3";
package nlos.sabi.v1;

message SampleRequest {
  string request_token = 1;
  uint32 timeout_ms = 2;
  uint32 mode = 3;
}

service SampleChannelService {
  rpc ExecuteSample(SampleRequest) returns (SampleRequest);
}
"""
            manifest = MINIMAL_MANIFEST_TEMPLATE.format(
                epoch=1,
                major=1,
                minor=0,
                frozen="true",
                reserved_nums="[]",
                reserved_syms="[]",
                entries_block="entries: []",
            )
            b_dir, b_man = _write_surface(
                root / "base", proto_text=base_proto, manifest_text=manifest
            )
            c_dir, c_man = _write_surface(
                root / "cand", proto_text=cand_proto, manifest_text=manifest
            )
            report = sabi_compat_diff.compare_contract_surfaces(
                b_dir, b_man, c_dir, c_man
            )
            self.assertFalse(report.passed)
            err_codes = {item.code for item in report.errors}
            self.assertIn("E_REMOVED_MESSAGE", err_codes)
            self.assertIn("E_REMOVED_ENUM", err_codes)
            self.assertIn("E_REMOVED_RPC", err_codes)
            self.assertIn("E_CHANGED_RPC_SIGNATURE", err_codes)

    def test_07_deleting_field_without_both_numeric_and_name_reserved_fails(
        self,
    ) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            base_proto = MINIMAL_PROTO_TEMPLATE.format(extra_fields="", extra_rpcs="")
            # Candidate reserves number 2 but forgets `reserved "timeout_ms";`
            cand_proto = """\
syntax = "proto3";
package nlos.sabi.v1;

enum SampleMode {
  SAMPLE_MODE_UNSPECIFIED = 0;
  SAMPLE_MODE_FAST = 1;
}

message SampleRequest {
  reserved 2;
  string request_token = 1;
  SampleMode mode = 3;
}

message SampleResponse {
  bool accepted = 1;
}

service SampleChannelService {
  rpc ExecuteSample(SampleRequest) returns (SampleResponse);
}
"""
            manifest = MINIMAL_MANIFEST_TEMPLATE.format(
                epoch=1,
                major=1,
                minor=0,
                frozen="true",
                reserved_nums="[]",
                reserved_syms="[]",
                entries_block="entries: []",
            )
            b_dir, b_man = _write_surface(
                root / "base", proto_text=base_proto, manifest_text=manifest
            )
            c_dir, c_man = _write_surface(
                root / "cand", proto_text=cand_proto, manifest_text=manifest
            )
            report = sabi_compat_diff.compare_contract_surfaces(
                b_dir, b_man, c_dir, c_man
            )
            self.assertFalse(report.passed)
            self.assertIn(
                "E_REMOVED_FIELD_WITHOUT_RESERVED",
                {item.code for item in report.errors},
            )

    def test_08_changing_existing_field_type_or_name_fails(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            base_proto = MINIMAL_PROTO_TEMPLATE.format(extra_fields="", extra_rpcs="")
            cand_proto = """\
syntax = "proto3";
package nlos.sabi.v1;

enum SampleMode {
  SAMPLE_MODE_UNSPECIFIED = 0;
  SAMPLE_MODE_FAST = 1;
}

message SampleRequest {
  bytes renamed_token = 1;
  repeated uint32 timeout_ms = 2;
  SampleMode mode = 3;
}

message SampleResponse {
  bool accepted = 1;
}

service SampleChannelService {
  rpc ExecuteSample(SampleRequest) returns (SampleResponse);
}
"""
            manifest = MINIMAL_MANIFEST_TEMPLATE.format(
                epoch=1,
                major=1,
                minor=0,
                frozen="true",
                reserved_nums="[]",
                reserved_syms="[]",
                entries_block="entries: []",
            )
            b_dir, b_man = _write_surface(
                root / "base", proto_text=base_proto, manifest_text=manifest
            )
            c_dir, c_man = _write_surface(
                root / "cand", proto_text=cand_proto, manifest_text=manifest
            )
            report = sabi_compat_diff.compare_contract_surfaces(
                b_dir, b_man, c_dir, c_man
            )
            self.assertFalse(report.passed)
            err_codes = {item.code for item in report.errors}
            self.assertIn("E_CHANGED_FIELD_NAME", err_codes)
            self.assertIn("E_CHANGED_FIELD_TYPE", err_codes)

    def test_09_reusing_or_dropping_proto_reserved_slot_fails(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            base_proto = MINIMAL_PROTO_TEMPLATE.format(
                extra_fields='  reserved 9;\n  reserved "legacy_field";',
                extra_rpcs="",
            )
            cand_proto = MINIMAL_PROTO_TEMPLATE.format(
                extra_fields="  string legacy_field = 9;",
                extra_rpcs="",
            )
            manifest = MINIMAL_MANIFEST_TEMPLATE.format(
                epoch=1,
                major=1,
                minor=0,
                frozen="true",
                reserved_nums="[]",
                reserved_syms="[]",
                entries_block="entries: []",
            )
            b_dir, b_man = _write_surface(
                root / "base", proto_text=base_proto, manifest_text=manifest
            )
            c_dir, c_man = _write_surface(
                root / "cand", proto_text=cand_proto, manifest_text=manifest
            )
            report = sabi_compat_diff.compare_contract_surfaces(
                b_dir, b_man, c_dir, c_man
            )
            self.assertFalse(report.passed)
            err_codes = {item.code for item in report.errors}
            self.assertIn("E_REMOVED_RESERVED_TOMBSTONE", err_codes)
            self.assertIn("E_RESERVED_SLOT_REUSE", err_codes)

    def test_10_illegal_state_skip_active_to_sunset_fails(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            base_proto = MINIMAL_PROTO_TEMPLATE.format(extra_fields="", extra_rpcs="")
            cand_proto = """\
syntax = "proto3";
package nlos.sabi.v1;

enum SampleMode {
  SAMPLE_MODE_UNSPECIFIED = 0;
  SAMPLE_MODE_FAST = 1;
}

message SampleRequest {
  reserved 2;
  reserved "timeout_ms";
  string request_token = 1;
  SampleMode mode = 3;
}

message SampleResponse {
  bool accepted = 1;
}

service SampleChannelService {
  rpc ExecuteSample(SampleRequest) returns (SampleResponse);
}
"""
            base_entries = """\
entries:
  - channel: nlos.sabi.Sample
    symbol: SampleRequest.timeout_ms
    kind: FIELD
    parent_type: SampleRequest
    field_number: 2
    introduced_in_epoch: 1
    state: ACTIVE
"""
            cand_entries = """\
entries:
  - channel: nlos.sabi.Sample
    symbol: SampleRequest.timeout_ms
    kind: FIELD
    parent_type: SampleRequest
    field_number: 2
    introduced_in_epoch: 1
    state: SUNSET
    deprecated_in_epoch: 2
    earliest_sunset_epoch: 4
    sunset_in_epoch: 4
    replacement: SampleRequest.request_token
    reason: Skipped DEPRECATED in baseline
"""
            b_dir, b_man = _write_surface(
                root / "base",
                proto_text=base_proto,
                manifest_text=MINIMAL_MANIFEST_TEMPLATE.format(
                    epoch=1,
                    major=1,
                    minor=0,
                    frozen="true",
                    reserved_nums="[]",
                    reserved_syms="[]",
                    entries_block=base_entries,
                ),
            )
            c_dir, c_man = _write_surface(
                root / "cand",
                proto_text=cand_proto,
                manifest_text=MINIMAL_MANIFEST_TEMPLATE.format(
                    epoch=4,
                    major=4,
                    minor=0,
                    frozen="true",
                    reserved_nums="[]",
                    reserved_syms="[]",
                    entries_block=cand_entries,
                ),
            )
            report = sabi_compat_diff.compare_contract_surfaces(
                b_dir, b_man, c_dir, c_man
            )
            self.assertFalse(report.passed)
            self.assertIn(
                "E_ILLEGAL_STATE_SKIP_ACTIVE_TO_SUNSET",
                {item.code for item in report.errors},
            )

    def test_11_insufficient_deprecation_window_and_premature_sunset_fail(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            proto_src = MINIMAL_PROTO_TEMPLATE.format(extra_fields="", extra_rpcs="")
            short_window_entries = """\
entries:
  - channel: nlos.sabi.Sample
    symbol: SampleRequest.timeout_ms
    kind: FIELD
    parent_type: SampleRequest
    field_number: 2
    introduced_in_epoch: 1
    state: DEPRECATED
    deprecated_in_epoch: 2
    earliest_sunset_epoch: 3
    replacement: SampleRequest.request_token
    reason: Window is only 1 epoch instead of >= 2
"""
            b_dir, b_man = _write_surface(
                root / "base",
                proto_text=proto_src,
                manifest_text=MINIMAL_MANIFEST_TEMPLATE.format(
                    epoch=1,
                    major=1,
                    minor=0,
                    frozen="true",
                    reserved_nums="[]",
                    reserved_syms="[]",
                    entries_block="entries: []",
                ),
            )
            c_dir, c_man = _write_surface(
                root / "cand",
                proto_text=proto_src,
                manifest_text=MINIMAL_MANIFEST_TEMPLATE.format(
                    epoch=2,
                    major=2,
                    minor=0,
                    frozen="true",
                    reserved_nums="[]",
                    reserved_syms="[]",
                    entries_block=short_window_entries,
                ),
            )
            report = sabi_compat_diff.compare_contract_surfaces(
                b_dir, b_man, c_dir, c_man
            )
            self.assertFalse(report.passed)
            self.assertIn(
                "E_INSUFFICIENT_DEPRECATION_WINDOW",
                {item.code for item in report.errors},
            )

    def test_12_premature_sunset_and_unremoved_sunset_field_fail(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            proto_src = MINIMAL_PROTO_TEMPLATE.format(extra_fields="", extra_rpcs="")
            base_entries = """\
entries:
  - channel: nlos.sabi.Sample
    symbol: SampleRequest.timeout_ms
    kind: FIELD
    parent_type: SampleRequest
    field_number: 2
    introduced_in_epoch: 1
    state: DEPRECATED
    deprecated_in_epoch: 2
    earliest_sunset_epoch: 4
    replacement: SampleRequest.request_token
    reason: Planned sunset at epoch 4
"""
            cand_entries = """\
entries:
  - channel: nlos.sabi.Sample
    symbol: SampleRequest.timeout_ms
    kind: FIELD
    parent_type: SampleRequest
    field_number: 2
    introduced_in_epoch: 1
    state: SUNSET
    deprecated_in_epoch: 2
    earliest_sunset_epoch: 4
    sunset_in_epoch: 3
    replacement: SampleRequest.request_token
    reason: Premature sunset at epoch 3 and field still present in proto
"""
            b_dir, b_man = _write_surface(
                root / "base",
                proto_text=proto_src,
                manifest_text=MINIMAL_MANIFEST_TEMPLATE.format(
                    epoch=2,
                    major=2,
                    minor=0,
                    frozen="true",
                    reserved_nums="[]",
                    reserved_syms="[]",
                    entries_block=base_entries,
                ),
            )
            c_dir, c_man = _write_surface(
                root / "cand",
                proto_text=proto_src,
                manifest_text=MINIMAL_MANIFEST_TEMPLATE.format(
                    epoch=3,
                    major=3,
                    minor=0,
                    frozen="true",
                    reserved_nums="[]",
                    reserved_syms="[]",
                    entries_block=cand_entries,
                ),
            )
            report = sabi_compat_diff.compare_contract_surfaces(
                b_dir, b_man, c_dir, c_man
            )
            self.assertFalse(report.passed)
            err_codes = {item.code for item in report.errors}
            self.assertIn("E_PREMATURE_SUNSET", err_codes)
            self.assertIn("E_SUNSET_FIELD_STILL_PRESENT", err_codes)

    def test_13_manifest_reserved_symbol_or_field_number_reuse_fails(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            base_proto = MINIMAL_PROTO_TEMPLATE.format(extra_fields="", extra_rpcs="")
            cand_proto = MINIMAL_PROTO_TEMPLATE.format(
                extra_fields="  string retired_sym = 77;",
                extra_rpcs="",
            )
            b_dir, b_man = _write_surface(
                root / "base",
                proto_text=base_proto,
                manifest_text=MINIMAL_MANIFEST_TEMPLATE.format(
                    epoch=2,
                    major=2,
                    minor=0,
                    frozen="true",
                    reserved_nums="[77]",
                    reserved_syms='["SampleRequest.retired_sym"]',
                    entries_block="entries: []",
                ),
            )
            cand_entries = """\
entries:
  - channel: nlos.sabi.Sample
    symbol: SampleRequest.retired_sym
    kind: FIELD
    parent_type: SampleRequest
    field_number: 77
    introduced_in_epoch: 2
    state: ACTIVE
"""
            c_dir, c_man = _write_surface(
                root / "cand",
                proto_text=cand_proto,
                manifest_text=MINIMAL_MANIFEST_TEMPLATE.format(
                    epoch=2,
                    major=2,
                    minor=1,
                    frozen="true",
                    reserved_nums="[77]",
                    reserved_syms='["SampleRequest.retired_sym"]',
                    entries_block=cand_entries,
                ),
            )
            report = sabi_compat_diff.compare_contract_surfaces(
                b_dir, b_man, c_dir, c_man
            )
            self.assertFalse(report.passed)
            err_codes = {item.code for item in report.errors}
            self.assertIn("E_MANIFEST_RESERVED_REUSE", err_codes)
            self.assertIn("E_RESERVED_SLOT_REUSE", err_codes)

    def test_14_cli_subprocess_exit_codes_and_deterministic_json(self) -> None:
        run_a = subprocess.run(
            [sys.executable, str(SCRIPT_PATH), "--self-check", "--json"],
            capture_output=True,
            text=True,
            check=False,
        )
        run_b = subprocess.run(
            [sys.executable, str(SCRIPT_PATH), "--self-check", "--json"],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(run_a.returncode, 0, msg=run_a.stderr)
        self.assertEqual(run_a.stdout, run_b.stdout)
        parsed = json.loads(run_a.stdout)
        self.assertTrue(parsed["passed"])
        self.assertEqual(parsed["error_count"], 0)

        with tempfile.TemporaryDirectory() as tmp:
            bad_dir = Path(tmp) / "broken"
            bad_dir.mkdir()
            (bad_dir / "broken.proto").write_text("syntax = 'proto2';\n", encoding="utf-8")
            (bad_dir / "deprecation-manifest.yaml").write_text("", encoding="utf-8")
            bad_run = subprocess.run(
                [
                    sys.executable,
                    str(SCRIPT_PATH),
                    "--self-check",
                    "--schema-dir",
                    str(bad_dir),
                    "--json",
                ],
                capture_output=True,
                text=True,
                check=False,
            )
            self.assertEqual(bad_run.returncode, 2)
            bad_payload = json.loads(bad_run.stdout)
            self.assertFalse(bad_payload["passed"])
            self.assertEqual(bad_payload["findings"][0]["code"], "E_SCHEMA_PARSE_ERROR")


if __name__ == "__main__":
    unittest.main()
