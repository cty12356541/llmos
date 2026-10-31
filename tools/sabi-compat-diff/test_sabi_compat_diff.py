#!/usr/bin/env python3
"""Unit and CLI tests for `tools/sabi-compat-diff/sabi_compat_diff.py`."""

from __future__ import annotations

import io
import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

THIS_DIR = Path(__file__).resolve().parent
REPO_ROOT = THIS_DIR.parent.parent
if str(THIS_DIR) not in sys.path:
    sys.path.insert(0, str(THIS_DIR))

import sabi_compat_diff as scd  # noqa: E402


def _write_lf(path: Path, content: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with open(path, "w", encoding="utf-8", newline="\n") as handle:
        handle.write(content)


BASELINE_V1_YAML = """\
manifest_version: 1
current_epoch: 1
min_compatibility_window_epochs: 2
frozen_channels_total: 1

channels:
  - name: "nlos.sabi.ServiceDirectory"
    major: 1
    minor: 0
    frozen: true
    reserved_field_numbers: [90]
    reserved_symbols: ["AncientV0"]

entries:
  - channel: "nlos.sabi.ServiceDirectory"
    symbol: "ResolveServiceRequest"
    kind: "MESSAGE"
    introduced_in_epoch: 1
    state: "ACTIVE"

  - channel: "nlos.sabi.ServiceDirectory"
    symbol: "ResolveServiceRequest.legacy_filter"
    kind: "FIELD"
    parent_type: "ResolveServiceRequest"
    field_number: 8
    introduced_in_epoch: 1
    state: "ACTIVE"
"""


class SabiCompatDiffTest(unittest.TestCase):
    def test_canonical_repo_manifest_and_proto_self_diff_is_unchanged(self) -> None:
        manifest_path = REPO_ROOT / "schema" / "nlos" / "sabi" / "v1" / "deprecation-manifest.yaml"
        proto_dir = REPO_ROOT / "schema" / "nlos" / "sabi" / "v1"

        stdout_buf = io.StringIO()
        with patch("sys.stdout", stdout_buf):
            code = scd.main(
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
        self.assertEqual(code, 0)
        payload = json.loads(stdout_buf.getvalue())
        self.assertTrue(payload["compatible"])
        self.assertEqual(payload["summary"][scd.BREAKING_VIOLATION], 0)
        self.assertGreaterEqual(payload["summary"][scd.UNCHANGED], 9 + 25)

    def test_additive_safe_new_channel_minor_bump_and_new_active_entry(self) -> None:
        candidate_yaml = """\
manifest_version: 1
current_epoch: 1
min_compatibility_window_epochs: 2
frozen_channels_total: 2

channels:
  - name: "nlos.sabi.ServiceDirectory"
    major: 1
    minor: 1
    frozen: true
    reserved_field_numbers: [90]
    reserved_symbols: ["AncientV0"]

  - name: "nlos.sabi.AuditTelemetry"
    major: 1
    minor: 0
    frozen: true
    reserved_field_numbers: []
    reserved_symbols: []

entries:
  - channel: "nlos.sabi.ServiceDirectory"
    symbol: "ResolveServiceRequest"
    kind: "MESSAGE"
    introduced_in_epoch: 1
    state: "ACTIVE"

  - channel: "nlos.sabi.ServiceDirectory"
    symbol: "ResolveServiceRequest.legacy_filter"
    kind: "FIELD"
    parent_type: "ResolveServiceRequest"
    field_number: 8
    introduced_in_epoch: 1
    state: "ACTIVE"

  - channel: "nlos.sabi.ServiceDirectory"
    symbol: "ResolveServiceRequest.capability_selector"
    kind: "FIELD"
    parent_type: "ResolveServiceRequest"
    field_number: 9
    introduced_in_epoch: 1
    state: "ACTIVE"
"""
        base_doc, _ = scd.parse_deprecation_manifest_yaml(BASELINE_V1_YAML)
        cand_doc, cand_err = scd.parse_deprecation_manifest_yaml(candidate_yaml)
        report = scd.diff_deprecation_manifests(base_doc, cand_doc, cand_err)

        self.assertTrue(report.compatible, report.render_text())
        self.assertEqual(report.counts[scd.BREAKING_VIOLATION], 0)
        self.assertEqual(report.counts[scd.ADDITIVE_SAFE], 3)

    def test_valid_active_to_deprecated_and_deprecated_to_sunset_lifecycle(self) -> None:
        epoch_2_deprecated = """\
manifest_version: 1
current_epoch: 2
min_compatibility_window_epochs: 2
frozen_channels_total: 1

channels:
  - name: "nlos.sabi.ServiceDirectory"
    major: 2
    minor: 0
    frozen: true
    reserved_field_numbers: [90]
    reserved_symbols: ["AncientV0"]

entries:
  - channel: "nlos.sabi.ServiceDirectory"
    symbol: "ResolveServiceRequest"
    kind: "MESSAGE"
    introduced_in_epoch: 1
    state: "ACTIVE"

  - channel: "nlos.sabi.ServiceDirectory"
    symbol: "ResolveServiceRequest.legacy_filter"
    kind: "FIELD"
    parent_type: "ResolveServiceRequest"
    field_number: 8
    introduced_in_epoch: 1
    state: "DEPRECATED"
    deprecated_in_epoch: 2
    earliest_sunset_epoch: 4
    replacement: "ResolveServiceRequest.capability_selector"
    migration_rule_id: "MIG-SD-008"
    reason: "replaced by typed capability selector"
"""
        base_doc, _ = scd.parse_deprecation_manifest_yaml(BASELINE_V1_YAML)
        dep_doc, dep_err = scd.parse_deprecation_manifest_yaml(epoch_2_deprecated)
        rep_dep = scd.diff_deprecation_manifests(base_doc, dep_doc, dep_err)
        self.assertTrue(rep_dep.compatible, rep_dep.render_text())
        self.assertEqual(rep_dep.counts[scd.DEPRECATION_NOTICE], 1)

        epoch_4_sunset = """\
manifest_version: 1
current_epoch: 4
min_compatibility_window_epochs: 2
frozen_channels_total: 1

channels:
  - name: "nlos.sabi.ServiceDirectory"
    major: 4
    minor: 0
    frozen: true
    reserved_field_numbers: [8, 90]
    reserved_symbols: ["AncientV0"]

entries:
  - channel: "nlos.sabi.ServiceDirectory"
    symbol: "ResolveServiceRequest"
    kind: "MESSAGE"
    introduced_in_epoch: 1
    state: "ACTIVE"

  - channel: "nlos.sabi.ServiceDirectory"
    symbol: "ResolveServiceRequest.legacy_filter"
    kind: "FIELD"
    parent_type: "ResolveServiceRequest"
    field_number: 8
    introduced_in_epoch: 1
    state: "SUNSET"
    deprecated_in_epoch: 2
    earliest_sunset_epoch: 4
    sunset_in_epoch: 4
    replacement: "ResolveServiceRequest.capability_selector"
    migration_rule_id: "MIG-SD-008"
    reason: "retired at epoch 4"
"""
        sun_doc, sun_err = scd.parse_deprecation_manifest_yaml(epoch_4_sunset)
        rep_sun = scd.diff_deprecation_manifests(dep_doc, sun_doc, sun_err)
        self.assertTrue(rep_sun.compatible, rep_sun.render_text())
        self.assertEqual(rep_sun.counts[scd.SUNSET_RETIREMENT], 1)

        # Archiving the SUNSET entry into permanent channel tombstones in epoch 5 is also safe.
        epoch_5_archived = """\
manifest_version: 1
current_epoch: 5
min_compatibility_window_epochs: 2
frozen_channels_total: 1

channels:
  - name: "nlos.sabi.ServiceDirectory"
    major: 5
    minor: 0
    frozen: true
    reserved_field_numbers: [8, 90]
    reserved_symbols: ["AncientV0", "ResolveServiceRequest.legacy_filter"]

entries:
  - channel: "nlos.sabi.ServiceDirectory"
    symbol: "ResolveServiceRequest"
    kind: "MESSAGE"
    introduced_in_epoch: 1
    state: "ACTIVE"
"""
        arch_doc, arch_err = scd.parse_deprecation_manifest_yaml(epoch_5_archived)
        rep_arch = scd.diff_deprecation_manifests(sun_doc, arch_doc, arch_err)
        self.assertTrue(rep_arch.compatible, rep_arch.render_text())
        self.assertEqual(rep_arch.counts[scd.SUNSET_RETIREMENT], 1)

    def test_blocks_physical_deletion_of_channel_or_active_entry(self) -> None:
        deleted_entry_yaml = """\
manifest_version: 1
current_epoch: 2
min_compatibility_window_epochs: 2
frozen_channels_total: 1

channels:
  - name: "nlos.sabi.ServiceDirectory"
    major: 2
    minor: 0
    frozen: true
    reserved_field_numbers: [90]
    reserved_symbols: ["AncientV0"]

entries:
  - channel: "nlos.sabi.ServiceDirectory"
    symbol: "ResolveServiceRequest"
    kind: "MESSAGE"
    introduced_in_epoch: 1
    state: "ACTIVE"
"""
        base_doc, _ = scd.parse_deprecation_manifest_yaml(BASELINE_V1_YAML)
        cand_doc, cand_err = scd.parse_deprecation_manifest_yaml(deleted_entry_yaml)
        report = scd.diff_deprecation_manifests(base_doc, cand_doc, cand_err)
        self.assertFalse(report.compatible)
        self.assertGreaterEqual(report.counts[scd.BREAKING_VIOLATION], 1)

    def test_blocks_unfreeze_and_version_regressions(self) -> None:
        unfrozen_and_regressed = """\
manifest_version: 1
current_epoch: 1
min_compatibility_window_epochs: 2
frozen_channels_total: 1

channels:
  - name: "nlos.sabi.ServiceDirectory"
    major: 0
    minor: 0
    frozen: false
    reserved_field_numbers: [90]
    reserved_symbols: ["AncientV0"]

entries:
  - channel: "nlos.sabi.ServiceDirectory"
    symbol: "ResolveServiceRequest"
    kind: "MESSAGE"
    introduced_in_epoch: 1
    state: "ACTIVE"

  - channel: "nlos.sabi.ServiceDirectory"
    symbol: "ResolveServiceRequest.legacy_filter"
    kind: "FIELD"
    parent_type: "ResolveServiceRequest"
    field_number: 8
    introduced_in_epoch: 1
    state: "ACTIVE"
"""
        base_doc, _ = scd.parse_deprecation_manifest_yaml(BASELINE_V1_YAML)
        cand_doc, cand_err = scd.parse_deprecation_manifest_yaml(unfrozen_and_regressed)
        report = scd.diff_deprecation_manifests(base_doc, cand_doc, cand_err)
        self.assertFalse(report.compatible)
        self.assertGreaterEqual(report.counts[scd.BREAKING_VIOLATION], 2)

    def test_blocks_insufficient_grace_window_direct_sunset_and_premature_sunset(self) -> None:
        # 1. earliest_sunset_epoch = deprecated_in_epoch + 1 (< +2 minimum grace window)
        short_window_yaml = """\
manifest_version: 1
current_epoch: 2
min_compatibility_window_epochs: 2
frozen_channels_total: 1

channels:
  - name: "nlos.sabi.ServiceDirectory"
    major: 2
    minor: 0
    frozen: true
    reserved_field_numbers: [90]
    reserved_symbols: ["AncientV0"]

entries:
  - channel: "nlos.sabi.ServiceDirectory"
    symbol: "ResolveServiceRequest"
    kind: "MESSAGE"
    introduced_in_epoch: 1
    state: "ACTIVE"

  - channel: "nlos.sabi.ServiceDirectory"
    symbol: "ResolveServiceRequest.legacy_filter"
    kind: "FIELD"
    parent_type: "ResolveServiceRequest"
    field_number: 8
    introduced_in_epoch: 1
    state: "DEPRECATED"
    deprecated_in_epoch: 2
    earliest_sunset_epoch: 3
    replacement: "ResolveServiceRequest.capability_selector"
    migration_rule_id: "MIG-SD-008"
"""
        base_doc, _ = scd.parse_deprecation_manifest_yaml(BASELINE_V1_YAML)
        short_doc, short_err = scd.parse_deprecation_manifest_yaml(short_window_yaml)
        rep_short = scd.diff_deprecation_manifests(base_doc, short_doc, short_err)
        self.assertFalse(rep_short.compatible)

        # 2. Direct ACTIVE -> SUNSET jump
        direct_sunset_yaml = """\
manifest_version: 1
current_epoch: 4
min_compatibility_window_epochs: 2
frozen_channels_total: 1

channels:
  - name: "nlos.sabi.ServiceDirectory"
    major: 4
    minor: 0
    frozen: true
    reserved_field_numbers: [90]
    reserved_symbols: ["AncientV0"]

entries:
  - channel: "nlos.sabi.ServiceDirectory"
    symbol: "ResolveServiceRequest"
    kind: "MESSAGE"
    introduced_in_epoch: 1
    state: "ACTIVE"

  - channel: "nlos.sabi.ServiceDirectory"
    symbol: "ResolveServiceRequest.legacy_filter"
    kind: "FIELD"
    parent_type: "ResolveServiceRequest"
    field_number: 8
    introduced_in_epoch: 1
    state: "SUNSET"
    deprecated_in_epoch: 2
    earliest_sunset_epoch: 4
    sunset_in_epoch: 4
    replacement: "ResolveServiceRequest.capability_selector"
    migration_rule_id: "MIG-SD-008"
"""
        direct_doc, direct_err = scd.parse_deprecation_manifest_yaml(direct_sunset_yaml)
        rep_direct = scd.diff_deprecation_manifests(base_doc, direct_doc, direct_err)
        self.assertFalse(rep_direct.compatible)

    def test_blocks_reserved_slot_or_symbol_reuse_and_tombstone_removal(self) -> None:
        reused_tombstones_yaml = """\
manifest_version: 1
current_epoch: 2
min_compatibility_window_epochs: 2
frozen_channels_total: 1

channels:
  - name: "nlos.sabi.ServiceDirectory"
    major: 2
    minor: 0
    frozen: true
    reserved_field_numbers: []
    reserved_symbols: []

entries:
  - channel: "nlos.sabi.ServiceDirectory"
    symbol: "ResolveServiceRequest"
    kind: "MESSAGE"
    introduced_in_epoch: 1
    state: "ACTIVE"

  - channel: "nlos.sabi.ServiceDirectory"
    symbol: "ResolveServiceRequest.legacy_filter"
    kind: "FIELD"
    parent_type: "ResolveServiceRequest"
    field_number: 8
    introduced_in_epoch: 1
    state: "ACTIVE"

  - channel: "nlos.sabi.ServiceDirectory"
    symbol: "AncientV0"
    kind: "FIELD"
    parent_type: "ResolveServiceRequest"
    field_number: 90
    introduced_in_epoch: 2
    state: "ACTIVE"
"""
        base_doc, _ = scd.parse_deprecation_manifest_yaml(BASELINE_V1_YAML)
        cand_doc, cand_err = scd.parse_deprecation_manifest_yaml(reused_tombstones_yaml)
        report = scd.diff_deprecation_manifests(base_doc, cand_doc, cand_err)
        self.assertFalse(report.compatible)
        # Dropped reserved_field_numbers, dropped reserved_symbols, reused AncientV0, reused 90.
        self.assertGreaterEqual(report.counts[scd.BREAKING_VIOLATION], 4)

    def test_proto_directory_diff_and_cli_exit_codes(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            tmp_root = Path(tmp)
            base_yaml = tmp_root / "base.yaml"
            cand_ok_yaml = tmp_root / "cand_ok.yaml"
            cand_bad_yaml = tmp_root / "cand_bad.yaml"
            base_proto_dir = tmp_root / "proto_base"
            cand_ok_proto_dir = tmp_root / "proto_ok"
            cand_bad_proto_dir = tmp_root / "proto_bad"

            _write_lf(base_yaml, BASELINE_V1_YAML)
            _write_lf(cand_ok_yaml, BASELINE_V1_YAML)
            _write_lf(
                cand_bad_yaml,
                BASELINE_V1_YAML.replace("frozen: true", "frozen: false"),
            )

            _write_lf(
                base_proto_dir / "directory.proto",
                """\
syntax = "proto3";
package nlos.sabi.v1;

message ResolveServiceRequest {
  reserved 90;
  reserved "ancient_v0";
  string service = 1;
  string legacy_filter = 8;
}
""",
            )
            _write_lf(
                cand_ok_proto_dir / "directory.proto",
                """\
syntax = "proto3";
package nlos.sabi.v1;

message ResolveServiceRequest {
  reserved 90;
  reserved "ancient_v0";
  string service = 1;
  string legacy_filter = 8;
  optional string capability_selector = 9;
}
""",
            )
            _write_lf(
                cand_bad_proto_dir / "directory.proto",
                """\
syntax = "proto3";
package nlos.sabi.v1;

message ResolveServiceRequest {
  uint64 service = 1;
  string hijacked_reserved = 90;
}
""",
            )

            ok_out = io.StringIO()
            with patch("sys.stdout", ok_out):
                ok_code = scd.main(
                    [
                        str(base_yaml),
                        str(cand_ok_yaml),
                        "--baseline-proto-dir",
                        str(base_proto_dir),
                        "--candidate-proto-dir",
                        str(cand_ok_proto_dir),
                        "--json",
                    ]
                )
            self.assertEqual(ok_code, 0)
            ok_json = json.loads(ok_out.getvalue())
            self.assertTrue(ok_json["compatible"])
            self.assertEqual(ok_json["summary"][scd.ADDITIVE_SAFE], 1)

            bad_out = io.StringIO()
            with patch("sys.stdout", bad_out):
                bad_code = scd.main(
                    [
                        str(base_yaml),
                        str(cand_bad_yaml),
                        "--baseline-proto-dir",
                        str(base_proto_dir),
                        "--candidate-proto-dir",
                        str(cand_bad_proto_dir),
                        "--json",
                    ]
                )
            self.assertEqual(bad_code, 1)
            bad_json = json.loads(bad_out.getvalue())
            self.assertFalse(bad_json["compatible"])
            self.assertGreaterEqual(bad_json["summary"][scd.BREAKING_VIOLATION], 4)


if __name__ == "__main__":
    unittest.main()
