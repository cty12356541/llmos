#!/usr/bin/env python3
"""CI gate verifying SABI golden wire corpus immutability and SHA-256 digests."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import re
import subprocess
import sys
from typing import Any

MANIFEST_SCHEMA = "nlos.golden.epoch_manifest.v1"

CANONICAL_V1_0_CHANNELS: tuple[str, ...] = (
    "nlos.sabi.Envelope",
    "nlos.sabi.ServiceDirectory",
    "nlos.sabi.OperationControl",
    "nlos.sabi.SystemControl",
    "nlos.sabi.TakeoverControl",
    "nlos.sabi.WaitControl",
    "nlos.sabi.PrincipalHandshake",
    "nlos.sabi.DriverProvider",
    "nlos.sabi.ChannelControl",
)

EXPECTED_LEGACY_ROOT_COUNT = 7
_EPOCH_HEX_RE = re.compile(r"^[0-9a-f]+\n\Z")
_ROOT_HEX_RE = re.compile(r"^[0-9a-f]+\n?\Z")


def _verify_hex_entry(
    file_path: Path,
    entry: dict[str, Any],
    *,
    require_trailing_newline: bool,
    missing_code: str,
    errors: list[str],
    label: str,
) -> None:
    if not file_path.is_file():
        errors.append(f"{missing_code}: missing golden file {label}")
        return

    raw_bytes = file_path.read_bytes()
    actual_sha256_file = hashlib.sha256(raw_bytes).hexdigest()
    expected_sha256_file = entry.get("sha256_file")
    if actual_sha256_file != expected_sha256_file:
        errors.append(
            f"E_SHA256_FILE_MISMATCH: {label} file sha256 {actual_sha256_file} "
            f"!= manifest {expected_sha256_file}"
        )

    try:
        text = raw_bytes.decode("ascii")
    except UnicodeDecodeError:
        errors.append(f"E_NONCANONICAL_HEX: {label} contains non-ASCII bytes")
        return

    pattern = _EPOCH_HEX_RE if require_trailing_newline else _ROOT_HEX_RE
    stripped = text[:-1] if text.endswith("\n") else text
    if not pattern.match(text) or len(stripped) == 0 or len(stripped) % 2 != 0:
        errors.append(
            f"E_NONCANONICAL_HEX: {label} must be single-line canonical lowercase "
            "even-length hex"
            + (" ending with LF" if require_trailing_newline else "")
        )
        return

    wire_bytes = bytes.fromhex(stripped)
    actual_len = len(wire_bytes)
    expected_len = entry.get("byte_length")
    if actual_len != expected_len:
        errors.append(
            f"E_BYTE_LENGTH_MISMATCH: {label} wire byte_length {actual_len} "
            f"!= manifest {expected_len}"
        )

    actual_sha256_wire = hashlib.sha256(wire_bytes).hexdigest()
    expected_sha256_wire = entry.get("sha256_wire")
    if actual_sha256_wire != expected_sha256_wire:
        errors.append(
            f"E_SHA256_WIRE_MISMATCH: {label} wire sha256 {actual_sha256_wire} "
            f"!= manifest {expected_sha256_wire}"
        )


def _check_git_diff(repo_root: Path, base_ref: str, errors: list[str]) -> None:
    ls_proc = subprocess.run(
        ["git", "ls-tree", "-r", "--name-only", base_ref, "--", "schema/golden"],
        cwd=repo_root,
        capture_output=True,
        text=True,
        check=False,
    )
    if ls_proc.returncode != 0:
        errors.append(
            f"E_GIT_DIFF_BASE_INVALID: git ls-tree failed for {base_ref!r}: "
            f"{ls_proc.stderr.strip()}"
        )
        return

    pre_existing = {
        line.strip()
        for line in ls_proc.stdout.splitlines()
        if line.strip().endswith((".hex", "manifest.json"))
    }

    diff_proc = subprocess.run(
        [
            "git",
            "diff",
            "--name-status",
            "--no-renames",
            base_ref,
            "--",
            "schema/golden",
        ],
        cwd=repo_root,
        capture_output=True,
        text=True,
        check=False,
    )
    if diff_proc.returncode != 0:
        errors.append(
            f"E_GIT_DIFF_FAILED: git diff failed against {base_ref!r}: "
            f"{diff_proc.stderr.strip()}"
        )
        return

    for raw_line in diff_proc.stdout.splitlines():
        line = raw_line.strip()
        if not line:
            continue
        parts = line.split("\t", 1)
        if len(parts) != 2:
            continue
        status, rel_path = parts[0].strip(), parts[1].strip()
        if rel_path in pre_existing and status != "A":
            errors.append(
                f"E_GIT_DIFF_IMMUTABLE_VIOLATION: pre-existing golden artifact "
                f"{rel_path} modified/deleted ({status}) relative to {base_ref}"
            )
        elif rel_path.endswith(".hex") and status in {"M", "D", "T"}:
            errors.append(
                f"E_GIT_DIFF_IMMUTABLE_VIOLATION: golden hex {rel_path} "
                f"has status {status} relative to {base_ref}"
            )


def check_golden_corpus(
    repo_root: Path,
    *,
    check_git_diff_base: str | None = None,
) -> tuple[bool, list[str], dict[str, Any]]:
    """Validate legacy root goldens and all immutable epoch golden corpora."""
    errors: list[str] = []
    summary: dict[str, Any] = {
        "legacy_root_count": 0,
        "epochs": {},
    }

    golden_dir = repo_root / "schema" / "golden"
    epochs_root = golden_dir / "epochs"
    v1_dir = epochs_root / "v1.0"

    if not golden_dir.is_dir() or not epochs_root.is_dir() or not v1_dir.is_dir():
        errors.append(
            "E_GOLDEN_DIR_MISSING: schema/golden and schema/golden/epochs/v1.0 "
            "must exist"
        )
        return False, errors, summary

    epoch_dirs = sorted(p for p in epochs_root.iterdir() if p.is_dir())
    stray_epochs_entries = sorted(
        p.name for p in epochs_root.iterdir() if not p.is_dir()
    )
    for stray in stray_epochs_entries:
        errors.append(
            f"E_UNREGISTERED_GOLDEN_FILE: unexpected file schema/golden/epochs/{stray}"
        )

    legacy_checked = False
    for epoch_dir in epoch_dirs:
        epoch_name = epoch_dir.name
        manifest_path = epoch_dir / "manifest.json"
        if not manifest_path.is_file():
            errors.append(
                f"E_MANIFEST_MISSING: missing schema/golden/epochs/{epoch_name}/manifest.json"
            )
            continue

        try:
            manifest = json.loads(manifest_path.read_text(encoding="utf-8"))
        except json.JSONDecodeError as exc:
            errors.append(
                f"E_MANIFEST_JSON: invalid JSON in epochs/{epoch_name}/manifest.json: {exc}"
            )
            continue

        if manifest.get("schema") != MANIFEST_SCHEMA:
            errors.append(
                f"E_MANIFEST_SCHEMA: epochs/{epoch_name}/manifest.json schema "
                f"{manifest.get('schema')!r} != {MANIFEST_SCHEMA!r}"
            )
        if manifest.get("epoch") != epoch_name:
            errors.append(
                f"E_MANIFEST_EPOCH_MISMATCH: epochs/{epoch_name}/manifest.json "
                f"declares epoch {manifest.get('epoch')!r}"
            )
        if manifest.get("immutable") is not True:
            errors.append(
                f"E_MANIFEST_NOT_IMMUTABLE: epochs/{epoch_name}/manifest.json "
                "must set immutable=true"
            )
        sabi_major = manifest.get("sabi_major")
        if isinstance(sabi_major, bool) or not isinstance(sabi_major, int) or sabi_major <= 0:
            errors.append(
                f"E_MANIFEST_MAJOR: epochs/{epoch_name}/manifest.json "
                f"invalid sabi_major={sabi_major!r}"
            )

        required_channels = manifest.get("required_channels")
        if (
            not isinstance(required_channels, list)
            or not required_channels
            or len(required_channels) != len(set(required_channels))
        ):
            errors.append(
                f"E_CHANNEL_SET_MISMATCH: epochs/{epoch_name}/manifest.json "
                "required_channels must be a non-empty unique list"
            )
            required_channels_set: set[str] = set()
        else:
            required_channels_set = set(required_channels)

        if epoch_name == "v1.0":
            expected_set = set(CANONICAL_V1_0_CHANNELS)
            if required_channels_set != expected_set:
                missing = sorted(expected_set - required_channels_set)
                extra = sorted(required_channels_set - expected_set)
                errors.append(
                    f"E_CHANNEL_SET_MISMATCH: v1.0 required_channels mismatch "
                    f"(missing={missing}, extra={extra})"
                )

            legacy_entries = manifest.get("legacy_root_goldens")
            if not isinstance(legacy_entries, list):
                errors.append(
                    "E_ROOT_MANIFEST_MISSING: v1.0 manifest.json must lock "
                    "legacy_root_goldens"
                )
            else:
                legacy_checked = True
                if len(legacy_entries) != EXPECTED_LEGACY_ROOT_COUNT:
                    errors.append(
                        f"E_ROOT_GOLDEN_COUNT: expected {EXPECTED_LEGACY_ROOT_COUNT} "
                        f"legacy_root_goldens, got {len(legacy_entries)}"
                    )
                registered_root_files: set[str] = set()
                for entry in legacy_entries:
                    rel_path = entry.get("path", "")
                    root_file = repo_root / rel_path
                    if root_file.parent != golden_dir:
                        errors.append(
                            f"E_ROOT_PATH_INVALID: invalid legacy root path {rel_path!r}"
                        )
                        continue
                    registered_root_files.add(root_file.name)
                    _verify_hex_entry(
                        root_file,
                        entry,
                        require_trailing_newline=False,
                        missing_code="E_ROOT_GOLDEN_MISSING",
                        errors=errors,
                        label=rel_path,
                    )

                actual_root_hex = {p.name for p in golden_dir.glob("*.hex")}
                for unregistered in sorted(actual_root_hex - registered_root_files):
                    errors.append(
                        f"E_UNREGISTERED_GOLDEN_FILE: unregistered root golden "
                        f"schema/golden/{unregistered}"
                    )
                summary["legacy_root_count"] = len(registered_root_files)

        vectors = manifest.get("vectors")
        if not isinstance(vectors, list) or not vectors:
            errors.append(
                f"E_VECTORS_EMPTY: epochs/{epoch_name}/manifest.json vectors must be non-empty"
            )
            continue

        registered_epoch_files: set[str] = set()
        covered_channels: set[str] = set()
        handshake_messages: set[str] = set()

        for vec in vectors:
            file_name = vec.get("file", "")
            if not file_name or "/" in file_name or "\\" in file_name:
                errors.append(
                    f"E_VECTOR_FILENAME_INVALID: invalid vector file {file_name!r} "
                    f"in epochs/{epoch_name}"
                )
                continue
            if file_name in registered_epoch_files:
                errors.append(
                    f"E_DUPLICATE_VECTOR_FILE: duplicate vector {file_name} "
                    f"in epochs/{epoch_name}"
                )
            registered_epoch_files.add(file_name)

            channel = vec.get("channel", "")
            covered_channels.add(channel)
            if channel == "nlos.sabi.PrincipalHandshake":
                handshake_messages.add(str(vec.get("message", "")))

            if vec.get("schema_major") != sabi_major:
                errors.append(
                    f"E_VECTOR_MAJOR_MISMATCH: {file_name} schema_major "
                    f"{vec.get('schema_major')!r} != {sabi_major!r}"
                )

            _verify_hex_entry(
                epoch_dir / file_name,
                vec,
                require_trailing_newline=True,
                missing_code="E_EPOCH_GOLDEN_MISSING",
                errors=errors,
                label=f"schema/golden/epochs/{epoch_name}/{file_name}",
            )

        if covered_channels != required_channels_set:
            missing_ch = sorted(required_channels_set - covered_channels)
            extra_ch = sorted(covered_channels - required_channels_set)
            errors.append(
                f"E_CHANNEL_COVERAGE_INCOMPLETE: epochs/{epoch_name} channel "
                f"coverage mismatch (missing={missing_ch}, extra={extra_ch})"
            )

        if epoch_name == "v1.0":
            expected_hs = {
                "nlos.sabi.v1.PrincipalHandshakeChallenge",
                "nlos.sabi.v1.PrincipalHandshakeAttestation",
            }
            if not expected_hs.issubset(handshake_messages):
                errors.append(
                    "E_HANDSHAKE_DUAL_ARM_MISSING: v1.0 PrincipalHandshake must "
                    "cover both Challenge and Attestation messages"
                )

        actual_epoch_entries = {
            p.name for p in epoch_dir.iterdir() if p.name != "manifest.json"
        }
        for stray in sorted(actual_epoch_entries - registered_epoch_files):
            errors.append(
                f"E_UNREGISTERED_GOLDEN_FILE: unregistered file "
                f"schema/golden/epochs/{epoch_name}/{stray}"
            )

        summary["epochs"][epoch_name] = {
            "channel_count": len(covered_channels),
            "vector_count": len(registered_epoch_files),
        }

    if not legacy_checked:
        errors.append(
            "E_ROOT_MANIFEST_MISSING: v1.0 legacy_root_goldens was not verified"
        )

    if check_git_diff_base is not None:
        _check_git_diff(repo_root, check_git_diff_base, errors)

    return len(errors) == 0, errors, summary


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Verify SABI golden wire corpus immutability and SHA-256 manifest."
    )
    parser.add_argument(
        "--root",
        type=Path,
        default=Path(__file__).resolve().parents[1],
        help="Repository root directory (defaults to parent of tools/).",
    )
    parser.add_argument(
        "--check-git-diff",
        dest="check_git_diff_base",
        default=None,
        help="Optional git base ref to verify zero mutation/deletion of pre-existing goldens.",
    )
    args = parser.parse_args(argv)

    ok, errors, summary = check_golden_corpus(
        args.root.resolve(),
        check_git_diff_base=args.check_git_diff_base,
    )
    if not ok:
        for err in errors:
            print(f"[GOLDEN-IMMUTABILITY-FAIL] {err}", file=sys.stderr)
        print(
            f"golden immutability check FAILED with {len(errors)} error(s).",
            file=sys.stderr,
        )
        return 1

    epochs_desc = ", ".join(
        f"{ep}: {info['channel_count']} channels / {info['vector_count']} vectors"
        for ep, info in sorted(summary["epochs"].items())
    )
    diff_note = (
        f", git-diff base={args.check_git_diff_base} OK"
        if args.check_git_diff_base
        else ""
    )
    print(
        f"[GOLDEN-IMMUTABILITY-OK] legacy_root={summary['legacy_root_count']} files; "
        f"epochs=({epochs_desc}){diff_note}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
