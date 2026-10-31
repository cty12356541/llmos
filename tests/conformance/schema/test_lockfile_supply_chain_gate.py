"""CI-discovered conformance gate for Cargo and npm lockfile supply-chain integrity (RISK-B-09)."""

from __future__ import annotations

import json
from pathlib import Path
import re
import tomllib
from typing import Any
import unittest

ROOT = Path(__file__).resolve().parents[3]
SHA256_HEX_RE = re.compile(r"^[0-9a-f]{64}$")
GIT_REV_SHA_RE = re.compile(r"[0-9a-f]{40}", re.IGNORECASE)
SRI_INTEGRITY_RE = re.compile(r"^sha(?:256|384|512)-[A-Za-z0-9+/=]+(?:\s+sha(?:256|384|512)-[A-Za-z0-9+/=]+)*$")
FORBIDDEN_FLOATING_SPECS = frozenset({"*", "latest", "LATEST", "head", "HEAD", "main", "master", "nightly", "canary", ""})


def audit_cargo_manifest_dependencies(manifest_data: dict[str, Any], label: str) -> list[str]:
    """Returns blocker messages for floating or unpinned git dependencies in a parsed Cargo.toml."""
    violations: list[str] = []

    sections: list[tuple[str, dict[str, Any]]] = []
    workspace = manifest_data.get("workspace")
    if isinstance(workspace, dict):
        ws_deps = workspace.get("dependencies")
        if isinstance(ws_deps, dict):
            sections.append(("workspace.dependencies", ws_deps))

    for key in ("dependencies", "dev-dependencies", "build-dependencies"):
        table = manifest_data.get(key)
        if isinstance(table, dict):
            sections.append((key, table))

    targets = manifest_data.get("target")
    if isinstance(targets, dict):
        for target_cfg, target_body in targets.items():
            if isinstance(target_body, dict):
                for key in ("dependencies", "dev-dependencies", "build-dependencies"):
                    table = target_body.get(key)
                    if isinstance(table, dict):
                        sections.append((f"target.{target_cfg}.{key}", table))

    for section_name, dep_table in sections:
        for dep_name, spec in dep_table.items():
            if isinstance(spec, str):
                cleaned = spec.strip()
                if cleaned in FORBIDDEN_FLOATING_SPECS or "*" in cleaned:
                    violations.append(
                        f"{label} [{section_name}] {dep_name}: forbidden floating version '{spec}'"
                    )
            elif isinstance(spec, dict):
                if spec.get("workspace") is True:
                    continue
                version_val = spec.get("version")
                if isinstance(version_val, str):
                    cleaned = version_val.strip()
                    if cleaned in FORBIDDEN_FLOATING_SPECS or "*" in cleaned:
                        violations.append(
                            f"{label} [{section_name}] {dep_name}: forbidden floating version '{version_val}'"
                        )
                git_url = spec.get("git")
                if isinstance(git_url, str):
                    rev = spec.get("rev")
                    if not isinstance(rev, str) or not GIT_REV_SHA_RE.fullmatch(rev.strip()):
                        violations.append(
                            f"{label} [{section_name}] {dep_name}: git dependency '{git_url}' lacks 40-hex commit 'rev'"
                        )
                elif version_val is None and "path" not in spec:
                    violations.append(
                        f"{label} [{section_name}] {dep_name}: dependency lacks pinned version or local path"
                    )

    return violations


def audit_cargo_lock_data(lock_data: dict[str, Any], label: str) -> tuple[int, list[str]]:
    """Audits parsed Cargo.lock TOML for registry SHA-256 checksums and pinned git commits."""
    violations: list[str] = []
    packages = lock_data.get("package")
    if not isinstance(packages, list) or not packages:
        return 0, [f"{label}: missing or empty [[package]] list"]

    external_count = 0
    for pkg in packages:
        if not isinstance(pkg, dict):
            violations.append(f"{label}: malformed package entry")
            continue
        name = str(pkg.get("name", "<unknown>"))
        version = str(pkg.get("version", "")).strip()
        if not version or version in FORBIDDEN_FLOATING_SPECS or "*" in version:
            violations.append(f"{label} package {name}: invalid or floating version '{version}'")
        source = pkg.get("source")
        if source is None:
            # Workspace local member crate.
            continue
        external_count += 1
        source_str = str(source)
        if source_str.startswith("registry+"):
            checksum = pkg.get("checksum")
            if not isinstance(checksum, str) or not SHA256_HEX_RE.fullmatch(checksum):
                violations.append(
                    f"{label} registry package {name}@{version}: missing or invalid 64-hex SHA-256 checksum"
                )
        elif source_str.startswith("git+"):
            frag = source_str.split("#", 1)[1] if "#" in source_str else ""
            if not GIT_REV_SHA_RE.fullmatch(frag):
                violations.append(
                    f"{label} git package {name}@{version}: source '{source_str}' lacks pinned 40-hex commit hash"
                )

    return external_count, violations


def audit_npm_manifest_and_lockfile(
    manifest_data: dict[str, Any],
    lock_data: dict[str, Any],
    label: str,
) -> tuple[int, list[str]]:
    """Audits parsed package.json and package-lock.json (v2/v3) for floating specs and SRI integrity."""
    violations: list[str] = []

    for section in (
        "dependencies",
        "devDependencies",
        "optionalDependencies",
        "peerDependencies",
    ):
        deps = manifest_data.get(section)
        if not isinstance(deps, dict):
            continue
        for dep_name, raw_spec in deps.items():
            spec = str(raw_spec).strip()
            if spec in FORBIDDEN_FLOATING_SPECS or spec == "x" or spec == "X" or "*" in spec:
                violations.append(
                    f"{label}/package.json [{section}] {dep_name}: forbidden floating version '{raw_spec}'"
                )
            if (spec.startswith("git+") or spec.startswith("github:")) and "#" not in spec:
                violations.append(
                    f"{label}/package.json [{section}] {dep_name}: unpinned git dependency '{raw_spec}'"
                )

    packages = lock_data.get("packages")
    if not isinstance(packages, dict) or "" not in packages:
        return 0, [f"{label}/package-lock.json: missing lockfileVersion 2/3 'packages' map"]

    audited_remote = 0
    for pkg_key, entry in packages.items():
        if pkg_key == "":
            continue
        if not isinstance(entry, dict):
            violations.append(f"{label}/package-lock.json [{pkg_key}]: malformed package object")
            continue
        if entry.get("link") is True:
            continue
        resolved = str(entry.get("resolved", ""))
        if resolved.startswith("file:"):
            continue
        audited_remote += 1
        version = str(entry.get("version", "")).strip()
        if not version or version in FORBIDDEN_FLOATING_SPECS:
            violations.append(
                f"{label}/package-lock.json [{pkg_key}]: missing or floating locked version '{version}'"
            )
        integrity = entry.get("integrity")
        if not isinstance(integrity, str) or not SRI_INTEGRITY_RE.fullmatch(integrity.strip()):
            violations.append(
                f"{label}/package-lock.json [{pkg_key}]: missing or invalid SRI 'integrity' digest"
            )

    return audited_remote, violations


class LockfileSupplyChainConformanceGateTests(unittest.TestCase):
    """Standing CI conformance gate verifying repository Cargo and npm lockfiles (RISK-B-09)."""

    def test_root_and_workspace_cargo_manifests_and_lockfile_are_pinned_and_checksummed(self) -> None:
        root_toml = ROOT / "Cargo.toml"
        root_lock = ROOT / "Cargo.lock"
        self.assertTrue(root_toml.is_file(), f"missing root Cargo.toml: {root_toml}")
        self.assertTrue(root_lock.is_file(), f"missing root Cargo.lock: {root_lock}")

        manifest_paths = [root_toml, *sorted((ROOT / "crates").glob("*/Cargo.toml"))]
        self.assertGreaterEqual(len(manifest_paths), 45, "expected root + >=44 workspace crate manifests")

        all_violations: list[str] = []
        for manifest_path in manifest_paths:
            rel = manifest_path.relative_to(ROOT).as_posix()
            data = tomllib.loads(manifest_path.read_text(encoding="utf-8"))
            all_violations.extend(audit_cargo_manifest_dependencies(data, rel))

        lock_data = tomllib.loads(root_lock.read_text(encoding="utf-8"))
        external_count, lock_violations = audit_cargo_lock_data(lock_data, "Cargo.lock")
        all_violations.extend(lock_violations)

        self.assertEqual(all_violations, [])
        self.assertGreaterEqual(external_count, 100)

    def test_root_and_desktop_npm_manifests_and_lockfiles_are_pinned_and_sri_verified(self) -> None:
        for rel_dir, min_expected in ((".", 35), ("desktop", 70)):
            ws_dir = ROOT if rel_dir == "." else ROOT / rel_dir
            pkg_json = ws_dir / "package.json"
            pkg_lock = ws_dir / "package-lock.json"
            self.assertTrue(pkg_json.is_file(), f"missing {pkg_json}")
            self.assertTrue(pkg_lock.is_file(), f"missing {pkg_lock}")

            manifest_data = json.loads(pkg_json.read_text(encoding="utf-8"))
            lock_data = json.loads(pkg_lock.read_text(encoding="utf-8"))
            audited_count, violations = audit_npm_manifest_and_lockfile(
                manifest_data,
                lock_data,
                rel_dir,
            )
            self.assertEqual(violations, [], f"npm supply-chain violations in {rel_dir}: {violations}")
            self.assertGreaterEqual(audited_count, min_expected)

    def test_rejects_floating_versions_missing_checksums_and_missing_sri_integrity(self) -> None:
        bad_cargo_toml = tomllib.loads(
            """
            [workspace.dependencies]
            wild = "*"
            latest_pkg = { version = "latest" }
            unpinned_git = { git = "https://example.invalid/repo.git", branch = "main" }
            """
        )
        cargo_manifest_violations = audit_cargo_manifest_dependencies(bad_cargo_toml, "bad/Cargo.toml")
        self.assertEqual(len(cargo_manifest_violations), 3)

        bad_cargo_lock = tomllib.loads(
            """
            version = 4

            [[package]]
            name = "missing-hash"
            version = "1.0.0"
            source = "registry+https://github.com/rust-lang/crates.io-index"

            [[package]]
            name = "unpinned-git"
            version = "0.2.0"
            source = "git+https://example.invalid/repo.git?branch=main#not40hex"
            """
        )
        _, cargo_lock_violations = audit_cargo_lock_data(bad_cargo_lock, "bad/Cargo.lock")
        self.assertEqual(len(cargo_lock_violations), 2)

        bad_pkg_json = {
            "dependencies": {"floating-star": "*", "floating-latest": "latest"},
            "devDependencies": {"unpinned-gh": "github:org/repo"},
        }
        bad_pkg_lock = {
            "lockfileVersion": 3,
            "packages": {
                "": {"name": "bad-app", "version": "0.1.0"},
                "node_modules/tampered": {
                    "version": "1.2.3",
                    "resolved": "https://registry.npmjs.org/tampered/-/tampered-1.2.3.tgz",
                },
            },
        }
        _, npm_violations = audit_npm_manifest_and_lockfile(bad_pkg_json, bad_pkg_lock, "bad-npm")
        self.assertEqual(len(npm_violations), 4)


if __name__ == "__main__":
    unittest.main()
