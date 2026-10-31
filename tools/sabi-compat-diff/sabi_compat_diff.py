#!/usr/bin/env python3
"""Cross-epoch SABI contract evolution and deprecation-manifest compatibility checker.

Compares baseline vs. candidate `deprecation-manifest.yaml` documents (and optional
baseline vs. candidate `.proto` schema directories) using only the Python 3 standard
library. Classifies every contract delta into:
  - UNCHANGED
  - ADDITIVE_SAFE
  - DEPRECATION_NOTICE
  - SUNSET_RETIREMENT
  - BREAKING_VIOLATION

Exit codes:
  0  Candidate is backward-compatible (zero BREAKING_VIOLATION items)
  1  Candidate contains one or more BREAKING_VIOLATION items
  2  CLI usage or file/parse error
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass, field
import json
from pathlib import Path
import re
import sys
from typing import Any

REPORT_SCHEMA = "nlos.sabi.compat_diff.v1"

UNCHANGED = "UNCHANGED"
ADDITIVE_SAFE = "ADDITIVE_SAFE"
DEPRECATION_NOTICE = "DEPRECATION_NOTICE"
SUNSET_RETIREMENT = "SUNSET_RETIREMENT"
BREAKING_VIOLATION = "BREAKING_VIOLATION"

ALL_CLASSIFICATIONS: tuple[str, ...] = (
    UNCHANGED,
    ADDITIVE_SAFE,
    DEPRECATION_NOTICE,
    SUNSET_RETIREMENT,
    BREAKING_VIOLATION,
)

VALID_ENTRY_KINDS: frozenset[str] = frozenset(
    {"MESSAGE", "FIELD", "METHOD", "ENUM_VALUE", "EXTENSION"}
)
VALID_STATES: frozenset[str] = frozenset({"ACTIVE", "DEPRECATED", "SUNSET"})


class ManifestParseError(ValueError):
    """Raised when a deprecation-manifest.yaml or .proto file is syntactically invalid."""


@dataclass(frozen=True)
class ChannelRecord:
    name: str
    major: int
    minor: int
    frozen: bool
    reserved_field_numbers: tuple[int, ...]
    reserved_symbols: tuple[str, ...]


@dataclass(frozen=True)
class EntryRecord:
    channel: str
    symbol: str
    kind: str
    introduced_in_epoch: int
    state: str
    parent_type: str | None = None
    field_number: int | None = None
    deprecated_in_epoch: int | None = None
    earliest_sunset_epoch: int | None = None
    sunset_in_epoch: int | None = None
    replacement: str | None = None
    migration_rule_id: str | None = None
    reason: str | None = None


@dataclass(frozen=True)
class DeprecationManifestDoc:
    manifest_version: int
    current_epoch: int
    min_compatibility_window_epochs: int
    frozen_channels_total: int
    channels: dict[str, ChannelRecord]
    entries: dict[tuple[str, str], EntryRecord]


@dataclass(frozen=True)
class DiffItem:
    classification: str
    scope: str
    target: str
    rule_id: str
    detail: str

    def to_dict(self) -> dict[str, str]:
        return {
            "classification": self.classification,
            "scope": self.scope,
            "target": self.target,
            "rule_id": self.rule_id,
            "detail": self.detail,
        }


@dataclass
class CompatDiffReport:
    baseline_manifest: str
    candidate_manifest: str
    baseline_epoch: int
    candidate_epoch: int
    items: list[DiffItem] = field(default_factory=list)

    @property
    def counts(self) -> dict[str, int]:
        summary = {cls: 0 for cls in ALL_CLASSIFICATIONS}
        for item in self.items:
            summary[item.classification] = summary.get(item.classification, 0) + 1
        return summary

    @property
    def compatible(self) -> bool:
        return self.counts[BREAKING_VIOLATION] == 0

    @property
    def exit_code(self) -> int:
        return 0 if self.compatible else 1

    def to_dict(self) -> dict[str, Any]:
        return {
            "schema": REPORT_SCHEMA,
            "compatible": self.compatible,
            "exit_code": self.exit_code,
            "baseline_manifest": self.baseline_manifest,
            "candidate_manifest": self.candidate_manifest,
            "baseline_epoch": self.baseline_epoch,
            "candidate_epoch": self.candidate_epoch,
            "summary": self.counts,
            "items": [item.to_dict() for item in self.items],
        }

    def render_json(self) -> str:
        return json.dumps(self.to_dict(), indent=2, ensure_ascii=False) + "\n"

    def render_text(self) -> str:
        counts = self.counts
        verdict = "COMPATIBLE" if self.compatible else "BREAKING_REJECTED"
        lines = [
            "NLOS SABI CROSS-EPOCH COMPATIBILITY DIFF REPORT",
            f"verdict: {verdict}",
            f"baseline_manifest: {self.baseline_manifest} (epoch={self.baseline_epoch})",
            f"candidate_manifest: {self.candidate_manifest} (epoch={self.candidate_epoch})",
            (
                "summary: "
                f"unchanged={counts[UNCHANGED]} "
                f"additive_safe={counts[ADDITIVE_SAFE]} "
                f"deprecation_notice={counts[DEPRECATION_NOTICE]} "
                f"sunset_retirement={counts[SUNSET_RETIREMENT]} "
                f"breaking_violation={counts[BREAKING_VIOLATION]}"
            ),
        ]
        for item in self.items:
            if item.classification != UNCHANGED:
                lines.append(
                    f"[{item.classification}] [{item.rule_id}] {item.scope} {item.target}: {item.detail}"
                )
        return "\n".join(lines) + "\n"


def _strip_yaml_comment(raw_line: str) -> str:
    in_quotes = False
    escaped = False
    out: list[str] = []
    for ch in raw_line:
        if escaped:
            out.append(ch)
            escaped = False
            continue
        if ch == "\\" and in_quotes:
            out.append(ch)
            escaped = True
            continue
        if ch == '"':
            in_quotes = not in_quotes
            out.append(ch)
            continue
        if ch == "#" and not in_quotes:
            break
        out.append(ch)
    return "".join(out).rstrip()


def _parse_scalar_str(raw: str, line_no: int) -> str:
    val = raw.strip()
    if len(val) >= 2 and val.startswith('"') and val.endswith('"'):
        try:
            parsed = json.loads(val)
        except json.JSONDecodeError as exc:
            raise ManifestParseError(f"line {line_no}: invalid quoted string {val!r}: {exc}") from exc
        if not isinstance(parsed, str):
            raise ManifestParseError(f"line {line_no}: expected string, got {val!r}")
        return parsed
    if not val or " " in val:
        raise ManifestParseError(f"line {line_no}: invalid unquoted string {val!r}")
    return val


def _parse_scalar_int(raw: str, line_no: int) -> int:
    val = raw.strip()
    if not re.fullmatch(r"[0-9]+", val):
        raise ManifestParseError(f"line {line_no}: expected non-negative integer, got {val!r}")
    return int(val, 10)


def _parse_scalar_bool(raw: str, line_no: int) -> bool:
    val = raw.strip()
    if val == "true":
        return True
    if val == "false":
        return False
    raise ManifestParseError(f"line {line_no}: expected boolean true/false, got {val!r}")


def _parse_inline_int_list(raw: str, line_no: int) -> tuple[int, ...]:
    val = raw.strip()
    if not (val.startswith("[") and val.endswith("]")):
        raise ManifestParseError(f"line {line_no}: expected inline int list [...], got {val!r}")
    inner = val[1:-1].strip()
    if not inner:
        return ()
    return tuple(_parse_scalar_int(part, line_no) for part in inner.split(","))


def _parse_inline_str_list(raw: str, line_no: int) -> tuple[str, ...]:
    val = raw.strip()
    if not (val.startswith("[") and val.endswith("]")):
        raise ManifestParseError(f"line {line_no}: expected inline string list [...], got {val!r}")
    inner = val[1:-1].strip()
    if not inner:
        return ()
    return tuple(_parse_scalar_str(part, line_no) for part in inner.split(","))


def parse_deprecation_manifest_yaml(text: str) -> tuple[DeprecationManifestDoc, list[DiffItem]]:
    """Parses `deprecation-manifest.yaml` and collects structural violations."""
    top_scalars: dict[str, int] = {}
    raw_channels: list[tuple[int, dict[str, str]]] = []
    raw_entries: list[tuple[int, dict[str, str]]] = []
    structural_violations: list[DiffItem] = []

    section: str | None = None
    current_item: dict[str, str] | None = None
    current_item_line = 0

    def flush_item() -> None:
        nonlocal current_item
        if current_item is None:
            return
        if section == "channels":
            raw_channels.append((current_item_line, current_item))
        elif section == "entries":
            raw_entries.append((current_item_line, current_item))
        current_item = None

    for idx, raw_line in enumerate(text.splitlines()):
        line_no = idx + 1
        cleaned = _strip_yaml_comment(raw_line)
        if not cleaned.strip():
            continue
        indent = len(cleaned) - len(cleaned.lstrip(" "))
        stripped = cleaned.strip()

        if indent == 0:
            flush_item()
            if stripped in ("channels:", "entries:"):
                section = stripped[:-1]
                continue
            section = None
            if ":" not in stripped:
                raise ManifestParseError(f"line {line_no}: expected `key: value`")
            key, val = stripped.split(":", 1)
            key = key.strip()
            val = val.strip()
            if key in top_scalars:
                raise ManifestParseError(f"line {line_no}: duplicate top-level field {key!r}")
            top_scalars[key] = _parse_scalar_int(val, line_no)
        else:
            if section not in ("channels", "entries"):
                raise ManifestParseError(f"line {line_no}: unexpected indented line outside list")
            if stripped.startswith("- "):
                flush_item()
                current_item = {}
                current_item_line = line_no
                remainder = stripped[2:].strip()
                if remainder:
                    if ":" not in remainder:
                        raise ManifestParseError(f"line {line_no}: expected `key: value` after `- `")
                    k, v = remainder.split(":", 1)
                    current_item[k.strip()] = v.strip()
            elif stripped == "-":
                flush_item()
                current_item = {}
                current_item_line = line_no
            else:
                if current_item is None or ":" not in stripped:
                    raise ManifestParseError(f"line {line_no}: malformed list item attribute")
                k, v = stripped.split(":", 1)
                attr_key = k.strip()
                if attr_key in current_item:
                    raise ManifestParseError(f"line {line_no}: duplicate attribute {attr_key!r}")
                current_item[attr_key] = v.strip()

    flush_item()

    for req in (
        "manifest_version",
        "current_epoch",
        "min_compatibility_window_epochs",
        "frozen_channels_total",
    ):
        if req not in top_scalars:
            raise ManifestParseError(f"missing required top-level field {req!r}")

    manifest_version = top_scalars["manifest_version"]
    current_epoch = top_scalars["current_epoch"]
    min_window = top_scalars["min_compatibility_window_epochs"]
    frozen_total = top_scalars["frozen_channels_total"]

    if manifest_version != 1:
        structural_violations.append(
            DiffItem(
                classification=BREAKING_VIOLATION,
                scope="manifest",
                target="manifest_version",
                rule_id="COMPAT-DEPRECATE-001",
                detail=f"unsupported manifest_version={manifest_version} (expected 1)",
            )
        )
    if current_epoch < 1:
        structural_violations.append(
            DiffItem(
                classification=BREAKING_VIOLATION,
                scope="manifest",
                target="current_epoch",
                rule_id="COMPAT-VER-001",
                detail=f"current_epoch={current_epoch} must be >= 1",
            )
        )
    if min_window < 2:
        structural_violations.append(
            DiffItem(
                classification=BREAKING_VIOLATION,
                scope="manifest",
                target="min_compatibility_window_epochs",
                rule_id="COMPAT-VER-001",
                detail=f"min_compatibility_window_epochs={min_window} is below statutory minimum 2",
            )
        )

    channels: dict[str, ChannelRecord] = {}
    for line_no, raw_ch in raw_channels:
        for req_ch in ("name", "major", "minor", "frozen"):
            if req_ch not in raw_ch:
                raise ManifestParseError(f"line {line_no}: channel missing {req_ch!r}")
        ch_name = _parse_scalar_str(raw_ch["name"], line_no)
        if ch_name in channels:
            structural_violations.append(
                DiffItem(
                    classification=BREAKING_VIOLATION,
                    scope="channel",
                    target=ch_name,
                    rule_id="COMPAT-DEPRECATE-001",
                    detail="duplicate channel declaration in candidate manifest",
                )
            )
            continue
        res_nums = (
            _parse_inline_int_list(raw_ch["reserved_field_numbers"], line_no)
            if "reserved_field_numbers" in raw_ch
            else ()
        )
        res_syms = (
            _parse_inline_str_list(raw_ch["reserved_symbols"], line_no)
            if "reserved_symbols" in raw_ch
            else ()
        )
        channels[ch_name] = ChannelRecord(
            name=ch_name,
            major=_parse_scalar_int(raw_ch["major"], line_no),
            minor=_parse_scalar_int(raw_ch["minor"], line_no),
            frozen=_parse_scalar_bool(raw_ch["frozen"], line_no),
            reserved_field_numbers=res_nums,
            reserved_symbols=res_syms,
        )

    if frozen_total != len(channels):
        structural_violations.append(
            DiffItem(
                classification=BREAKING_VIOLATION,
                scope="manifest",
                target="frozen_channels_total",
                rule_id="COMPAT-DEPRECATE-001",
                detail=f"frozen_channels_total={frozen_total} != actual channel count {len(channels)}",
            )
        )

    entries: dict[tuple[str, str], EntryRecord] = {}
    for line_no, raw_ent in raw_entries:
        for req_ent in ("channel", "symbol", "kind", "introduced_in_epoch", "state"):
            if req_ent not in raw_ent:
                raise ManifestParseError(f"line {line_no}: entry missing {req_ent!r}")
        ch = _parse_scalar_str(raw_ent["channel"], line_no)
        sym = _parse_scalar_str(raw_ent["symbol"], line_no)
        kind = _parse_scalar_str(raw_ent["kind"], line_no)
        state = _parse_scalar_str(raw_ent["state"], line_no)
        if kind not in VALID_ENTRY_KINDS:
            raise ManifestParseError(f"line {line_no}: unsupported entry kind {kind!r}")
        if state not in VALID_STATES:
            raise ManifestParseError(f"line {line_no}: unsupported entry state {state!r}")
        key = (ch, sym)
        if key in entries:
            structural_violations.append(
                DiffItem(
                    classification=BREAKING_VIOLATION,
                    scope="entry",
                    target=f"{ch}::{sym}",
                    rule_id="COMPAT-DEPRECATE-001",
                    detail="duplicate (channel, symbol) entry in manifest",
                )
            )
            continue
        entries[key] = EntryRecord(
            channel=ch,
            symbol=sym,
            kind=kind,
            introduced_in_epoch=_parse_scalar_int(raw_ent["introduced_in_epoch"], line_no),
            state=state,
            parent_type=(
                _parse_scalar_str(raw_ent["parent_type"], line_no)
                if "parent_type" in raw_ent
                else None
            ),
            field_number=(
                _parse_scalar_int(raw_ent["field_number"], line_no)
                if "field_number" in raw_ent
                else None
            ),
            deprecated_in_epoch=(
                _parse_scalar_int(raw_ent["deprecated_in_epoch"], line_no)
                if "deprecated_in_epoch" in raw_ent
                else None
            ),
            earliest_sunset_epoch=(
                _parse_scalar_int(raw_ent["earliest_sunset_epoch"], line_no)
                if "earliest_sunset_epoch" in raw_ent
                else None
            ),
            sunset_in_epoch=(
                _parse_scalar_int(raw_ent["sunset_in_epoch"], line_no)
                if "sunset_in_epoch" in raw_ent
                else None
            ),
            replacement=(
                _parse_scalar_str(raw_ent["replacement"], line_no)
                if "replacement" in raw_ent
                else None
            ),
            migration_rule_id=(
                _parse_scalar_str(raw_ent["migration_rule_id"], line_no)
                if "migration_rule_id" in raw_ent
                else None
            ),
            reason=(
                _parse_scalar_str(raw_ent["reason"], line_no)
                if "reason" in raw_ent
                else None
            ),
        )

    doc = DeprecationManifestDoc(
        manifest_version=manifest_version,
        current_epoch=current_epoch,
        min_compatibility_window_epochs=min_window,
        frozen_channels_total=frozen_total,
        channels=channels,
        entries=entries,
    )
    return doc, structural_violations


def _validate_candidate_entry_invariants(
    doc: DeprecationManifestDoc,
    entry: EntryRecord,
    effective_reserved_nums: set[int],
    effective_reserved_syms: set[str],
    items: list[DiffItem],
) -> bool:
    """Validates candidate entry lifecycle and tombstone rules. Returns True if clean."""
    target = f"{entry.channel}::{entry.symbol}"
    required_window = max(2, doc.min_compatibility_window_epochs)
    clean = True

    if entry.channel not in doc.channels:
        items.append(
            DiffItem(
                classification=BREAKING_VIOLATION,
                scope="entry",
                target=target,
                rule_id="COMPAT-VER-001",
                detail=f"references undeclared channel {entry.channel!r}",
            )
        )
        return False

    if entry.field_number is not None and entry.field_number <= 0:
        items.append(
            DiffItem(
                classification=BREAKING_VIOLATION,
                scope="entry",
                target=target,
                rule_id="COMPAT-DEPRECATE-001",
                detail=f"invalid protobuf field_number={entry.field_number}",
            )
        )
        clean = False

    if entry.state != "SUNSET" and entry.symbol in effective_reserved_syms:
        items.append(
            DiffItem(
                classification=BREAKING_VIOLATION,
                scope="entry",
                target=target,
                rule_id="COMPAT-DEPRECATE-001",
                detail=f"reuses permanently reserved symbol tombstone {entry.symbol!r}",
            )
        )
        clean = False

    if (
        entry.state != "SUNSET"
        and entry.field_number is not None
        and entry.field_number in effective_reserved_nums
    ):
        items.append(
            DiffItem(
                classification=BREAKING_VIOLATION,
                scope="entry",
                target=target,
                rule_id="COMPAT-DEPRECATE-001",
                detail=f"reuses permanently reserved field_number tombstone {entry.field_number}",
            )
        )
        clean = False

    if entry.state in ("DEPRECATED", "SUNSET"):
        for attr_name, attr_val in (
            ("deprecated_in_epoch", entry.deprecated_in_epoch),
            ("earliest_sunset_epoch", entry.earliest_sunset_epoch),
            ("replacement", entry.replacement),
            ("migration_rule_id", entry.migration_rule_id),
        ):
            if not attr_val:
                items.append(
                    DiffItem(
                        classification=BREAKING_VIOLATION,
                        scope="entry",
                        target=target,
                        rule_id="COMPAT-DEPRECATE-001",
                        detail=f"state={entry.state} requires {attr_name}",
                    )
                )
                clean = False

        if (
            entry.deprecated_in_epoch is not None
            and entry.earliest_sunset_epoch is not None
            and entry.earliest_sunset_epoch < entry.deprecated_in_epoch + required_window
        ):
            items.append(
                DiffItem(
                    classification=BREAKING_VIOLATION,
                    scope="entry",
                    target=target,
                    rule_id="COMPAT-DEPRECATE-001",
                    detail=(
                        f"insufficient deprecation grace window: earliest_sunset_epoch="
                        f"{entry.earliest_sunset_epoch} < deprecated_in_epoch="
                        f"{entry.deprecated_in_epoch} + {required_window}"
                    ),
                )
            )
            clean = False

    if entry.state == "SUNSET":
        if entry.sunset_in_epoch is None:
            items.append(
                DiffItem(
                    classification=BREAKING_VIOLATION,
                    scope="entry",
                    target=target,
                    rule_id="COMPAT-DEPRECATE-001",
                    detail="state=SUNSET requires sunset_in_epoch",
                )
            )
            clean = False
        elif (
            entry.earliest_sunset_epoch is not None
            and (
                entry.sunset_in_epoch < entry.earliest_sunset_epoch
                or doc.current_epoch < entry.sunset_in_epoch
                or doc.current_epoch < entry.earliest_sunset_epoch
            )
        ):
            items.append(
                DiffItem(
                    classification=BREAKING_VIOLATION,
                    scope="entry",
                    target=target,
                    rule_id="COMPAT-DEPRECATE-001",
                    detail=(
                        f"premature SUNSET at sunset_in_epoch={entry.sunset_in_epoch} "
                        f"(current_epoch={doc.current_epoch}, "
                        f"earliest_sunset_epoch={entry.earliest_sunset_epoch})"
                    ),
                )
            )
            clean = False

    return clean


def diff_deprecation_manifests(
    baseline: DeprecationManifestDoc,
    candidate: DeprecationManifestDoc,
    structural_violations: list[DiffItem] | None = None,
    *,
    baseline_label: str = "baseline.yaml",
    candidate_label: str = "candidate.yaml",
) -> CompatDiffReport:
    """Computes cross-epoch compatibility classifications between baseline and candidate."""
    items: list[DiffItem] = list(structural_violations or [])

    if candidate.current_epoch < baseline.current_epoch:
        items.append(
            DiffItem(
                classification=BREAKING_VIOLATION,
                scope="manifest",
                target="current_epoch",
                rule_id="COMPAT-VER-001",
                detail=(
                    f"current_epoch regressed from {baseline.current_epoch} "
                    f"to {candidate.current_epoch}"
                ),
            )
        )

    # 1. Channel-level diff
    all_channel_names = sorted(set(baseline.channels) | set(candidate.channels))
    effective_reserved_nums_by_ch: dict[str, set[int]] = {}
    effective_reserved_syms_by_ch: dict[str, set[str]] = {}

    for ch_name in all_channel_names:
        base_ch = baseline.channels.get(ch_name)
        cand_ch = candidate.channels.get(ch_name)

        if base_ch is not None and cand_ch is None:
            items.append(
                DiffItem(
                    classification=BREAKING_VIOLATION,
                    scope="channel",
                    target=ch_name,
                    rule_id="COMPAT-DEPRECATE-001",
                    detail="historical frozen SABI channel was physically deleted from manifest",
                )
            )
            continue

        assert cand_ch is not None
        effective_reserved_nums_by_ch[ch_name] = set(cand_ch.reserved_field_numbers)
        effective_reserved_syms_by_ch[ch_name] = set(cand_ch.reserved_symbols)

        if base_ch is None:
            if not cand_ch.frozen or cand_ch.major < 1:
                items.append(
                    DiffItem(
                        classification=BREAKING_VIOLATION,
                        scope="channel",
                        target=ch_name,
                        rule_id="COMPAT-VER-001",
                        detail=(
                            f"newly registered channel must have frozen=true and major>=1 "
                            f"(got frozen={cand_ch.frozen}, major={cand_ch.major})"
                        ),
                    )
                )
            else:
                items.append(
                    DiffItem(
                        classification=ADDITIVE_SAFE,
                        scope="channel",
                        target=ch_name,
                        rule_id="COMPAT-VER-001",
                        detail=f"added new frozen SABI channel v{cand_ch.major}.{cand_ch.minor}",
                    )
                )
            continue

        effective_reserved_nums_by_ch[ch_name] |= set(base_ch.reserved_field_numbers)
        effective_reserved_syms_by_ch[ch_name] |= set(base_ch.reserved_symbols)

        ch_broken = False
        if base_ch.frozen and not cand_ch.frozen:
            items.append(
                DiffItem(
                    classification=BREAKING_VIOLATION,
                    scope="channel",
                    target=ch_name,
                    rule_id="COMPAT-VER-001",
                    detail="frozen channel flipped from frozen=true back to frozen=false",
                )
            )
            ch_broken = True
        elif not cand_ch.frozen:
            items.append(
                DiffItem(
                    classification=BREAKING_VIOLATION,
                    scope="channel",
                    target=ch_name,
                    rule_id="COMPAT-VER-001",
                    detail="canonical SABI channel is not marked frozen=true",
                )
            )
            ch_broken = True

        if cand_ch.major < base_ch.major:
            items.append(
                DiffItem(
                    classification=BREAKING_VIOLATION,
                    scope="channel",
                    target=ch_name,
                    rule_id="COMPAT-VER-001",
                    detail=f"channel major version regressed from {base_ch.major} to {cand_ch.major}",
                )
            )
            ch_broken = True
        elif cand_ch.major == base_ch.major and cand_ch.minor < base_ch.minor:
            items.append(
                DiffItem(
                    classification=BREAKING_VIOLATION,
                    scope="channel",
                    target=ch_name,
                    rule_id="COMPAT-VER-001",
                    detail=f"channel minor version regressed from {base_ch.minor} to {cand_ch.minor}",
                )
            )
            ch_broken = True

        dropped_nums = sorted(set(base_ch.reserved_field_numbers) - set(cand_ch.reserved_field_numbers))
        if dropped_nums:
            items.append(
                DiffItem(
                    classification=BREAKING_VIOLATION,
                    scope="channel",
                    target=ch_name,
                    rule_id="COMPAT-DEPRECATE-001",
                    detail=f"removed permanent reserved_field_numbers tombstones: {dropped_nums}",
                )
            )
            ch_broken = True

        dropped_syms = sorted(set(base_ch.reserved_symbols) - set(cand_ch.reserved_symbols))
        if dropped_syms:
            items.append(
                DiffItem(
                    classification=BREAKING_VIOLATION,
                    scope="channel",
                    target=ch_name,
                    rule_id="COMPAT-DEPRECATE-001",
                    detail=f"removed permanent reserved_symbols tombstones: {dropped_syms}",
                )
            )
            ch_broken = True

        if not ch_broken:
            if (cand_ch.major, cand_ch.minor) != (base_ch.major, base_ch.minor) or (
                cand_ch.reserved_field_numbers != base_ch.reserved_field_numbers
                or cand_ch.reserved_symbols != base_ch.reserved_symbols
            ):
                items.append(
                    DiffItem(
                        classification=ADDITIVE_SAFE,
                        scope="channel",
                        target=ch_name,
                        rule_id="COMPAT-VER-001",
                        detail=(
                            f"channel evolved from v{base_ch.major}.{base_ch.minor} "
                            f"to v{cand_ch.major}.{cand_ch.minor}"
                        ),
                    )
                )
            else:
                items.append(
                    DiffItem(
                        classification=UNCHANGED,
                        scope="channel",
                        target=ch_name,
                        rule_id="COMPAT-VER-001",
                        detail=f"frozen channel v{cand_ch.major}.{cand_ch.minor} unchanged",
                    )
                )

    # Also treat baseline SUNSET field slots as permanently reserved against new entries.
    sunset_scoped_fields: set[tuple[str, str | None, int]] = set()
    for base_ent in baseline.entries.values():
        if base_ent.state == "SUNSET" and base_ent.field_number is not None:
            sunset_scoped_fields.add((base_ent.channel, base_ent.parent_type, base_ent.field_number))

    # Detect duplicate field_number collisions within candidate entries on same (channel, parent_type).
    seen_cand_fields: dict[tuple[str, str | None, int], str] = {}
    for cand_ent in candidate.entries.values():
        if cand_ent.field_number is not None:
            slot_key = (cand_ent.channel, cand_ent.parent_type, cand_ent.field_number)
            if slot_key in seen_cand_fields:
                items.append(
                    DiffItem(
                        classification=BREAKING_VIOLATION,
                        scope="entry",
                        target=f"{cand_ent.channel}::{cand_ent.symbol}",
                        rule_id="COMPAT-DEPRECATE-001",
                        detail=(
                            f"field_number={cand_ent.field_number} collides with "
                            f"{seen_cand_fields[slot_key]!r} in parent {cand_ent.parent_type!r}"
                        ),
                    )
                )
            else:
                seen_cand_fields[slot_key] = cand_ent.symbol

    # 2. Entry-level diff
    all_entry_keys = sorted(set(baseline.entries) | set(candidate.entries))
    for entry_key in all_entry_keys:
        ch_name, sym_name = entry_key
        target = f"{ch_name}::{sym_name}"
        base_ent = baseline.entries.get(entry_key)
        cand_ent = candidate.entries.get(entry_key)

        if base_ent is not None and cand_ent is None:
            cand_ch = candidate.channels.get(ch_name)
            tombstoned_in_ch = (
                base_ent.state == "SUNSET"
                and cand_ch is not None
                and sym_name in cand_ch.reserved_symbols
                and (
                    base_ent.field_number is None
                    or base_ent.field_number in cand_ch.reserved_field_numbers
                )
            )
            if tombstoned_in_ch:
                items.append(
                    DiffItem(
                        classification=SUNSET_RETIREMENT,
                        scope="entry",
                        target=target,
                        rule_id="COMPAT-DEPRECATE-001",
                        detail="SUNSET entry archived into permanent channel reserved_* tombstones",
                    )
                )
            else:
                items.append(
                    DiffItem(
                        classification=BREAKING_VIOLATION,
                        scope="entry",
                        target=target,
                        rule_id="COMPAT-DEPRECATE-001",
                        detail=(
                            f"historical {base_ent.state} entry was physically deleted without "
                            "completing SUNSET and recording permanent tombstones"
                        ),
                    )
                )
            continue

        assert cand_ent is not None
        res_nums = effective_reserved_nums_by_ch.get(ch_name, set())
        res_syms = effective_reserved_syms_by_ch.get(ch_name, set())
        entry_clean = _validate_candidate_entry_invariants(
            candidate, cand_ent, res_nums, res_syms, items
        )

        if base_ent is None:
            if (
                cand_ent.field_number is not None
                and (ch_name, cand_ent.parent_type, cand_ent.field_number) in sunset_scoped_fields
            ):
                items.append(
                    DiffItem(
                        classification=BREAKING_VIOLATION,
                        scope="entry",
                        target=target,
                        rule_id="COMPAT-DEPRECATE-001",
                        detail=(
                            f"new entry reuses SUNSET field_number={cand_ent.field_number} "
                            f"on {cand_ent.parent_type!r}"
                        ),
                    )
                )
                continue
            if not entry_clean:
                continue
            if cand_ent.state == "SUNSET":
                items.append(
                    DiffItem(
                        classification=BREAKING_VIOLATION,
                        scope="entry",
                        target=target,
                        rule_id="COMPAT-DEPRECATE-001",
                        detail="newly introduced entry cannot jump directly to SUNSET without prior DEPRECATED grace window",
                    )
                )
            elif cand_ent.state == "DEPRECATED":
                items.append(
                    DiffItem(
                        classification=DEPRECATION_NOTICE,
                        scope="entry",
                        target=target,
                        rule_id="COMPAT-DEPRECATE-001",
                        detail=(
                            f"added entry in DEPRECATED grace window "
                            f"(earliest_sunset_epoch={cand_ent.earliest_sunset_epoch}, "
                            f"replacement={cand_ent.replacement!r})"
                        ),
                    )
                )
            else:
                items.append(
                    DiffItem(
                        classification=ADDITIVE_SAFE,
                        scope="entry",
                        target=target,
                        rule_id="COMPAT-VER-001",
                        detail=f"added new ACTIVE {cand_ent.kind} in epoch {cand_ent.introduced_in_epoch}",
                    )
                )
            continue

        # Existing entry present in both baseline and candidate
        mutated_identity = False
        for attr in ("kind", "parent_type", "field_number", "introduced_in_epoch"):
            if getattr(base_ent, attr) != getattr(cand_ent, attr):
                items.append(
                    DiffItem(
                        classification=BREAKING_VIOLATION,
                        scope="entry",
                        target=target,
                        rule_id="COMPAT-DEPRECATE-001",
                        detail=(
                            f"immutable wire attribute {attr!r} changed from "
                            f"{getattr(base_ent, attr)!r} to {getattr(cand_ent, attr)!r}"
                        ),
                    )
                )
                mutated_identity = True
        if mutated_identity or not entry_clean:
            continue

        transition = (base_ent.state, cand_ent.state)
        if transition == ("ACTIVE", "ACTIVE"):
            items.append(
                DiffItem(
                    classification=UNCHANGED,
                    scope="entry",
                    target=target,
                    rule_id="COMPAT-DEPRECATE-001",
                    detail="ACTIVE contract unchanged",
                )
            )
        elif transition == ("ACTIVE", "DEPRECATED"):
            items.append(
                DiffItem(
                    classification=DEPRECATION_NOTICE,
                    scope="entry",
                    target=target,
                    rule_id="COMPAT-DEPRECATE-001",
                    detail=(
                        f"transitioned ACTIVE -> DEPRECATED at epoch {cand_ent.deprecated_in_epoch} "
                        f"(earliest_sunset_epoch={cand_ent.earliest_sunset_epoch}, "
                        f"replacement={cand_ent.replacement!r}, rule={cand_ent.migration_rule_id!r})"
                    ),
                )
            )
        elif transition == ("ACTIVE", "SUNSET"):
            items.append(
                DiffItem(
                    classification=BREAKING_VIOLATION,
                    scope="entry",
                    target=target,
                    rule_id="COMPAT-DEPRECATE-001",
                    detail="illegal direct ACTIVE -> SUNSET jump bypassing mandatory DEPRECATED grace window",
                )
            )
        elif transition == ("DEPRECATED", "DEPRECATED"):
            if (
                base_ent.earliest_sunset_epoch is not None
                and cand_ent.earliest_sunset_epoch is not None
                and cand_ent.earliest_sunset_epoch < base_ent.earliest_sunset_epoch
            ):
                items.append(
                    DiffItem(
                        classification=BREAKING_VIOLATION,
                        scope="entry",
                        target=target,
                        rule_id="COMPAT-DEPRECATE-001",
                        detail=(
                            f"shortened previously promised earliest_sunset_epoch from "
                            f"{base_ent.earliest_sunset_epoch} to {cand_ent.earliest_sunset_epoch}"
                        ),
                    )
                )
            elif cand_ent != base_ent:
                items.append(
                    DiffItem(
                        classification=DEPRECATION_NOTICE,
                        scope="entry",
                        target=target,
                        rule_id="COMPAT-DEPRECATE-001",
                        detail="updated DEPRECATED guidance metadata within grace window",
                    )
                )
            else:
                items.append(
                    DiffItem(
                        classification=UNCHANGED,
                        scope="entry",
                        target=target,
                        rule_id="COMPAT-DEPRECATE-001",
                        detail="DEPRECATED contract unchanged",
                    )
                )
        elif transition == ("DEPRECATED", "SUNSET"):
            if (
                base_ent.earliest_sunset_epoch is not None
                and cand_ent.sunset_in_epoch is not None
                and cand_ent.sunset_in_epoch < base_ent.earliest_sunset_epoch
            ):
                items.append(
                    DiffItem(
                        classification=BREAKING_VIOLATION,
                        scope="entry",
                        target=target,
                        rule_id="COMPAT-DEPRECATE-001",
                        detail=(
                            f"premature SUNSET at epoch {cand_ent.sunset_in_epoch} before baseline "
                            f"earliest_sunset_epoch={base_ent.earliest_sunset_epoch}"
                        ),
                    )
                )
            else:
                items.append(
                    DiffItem(
                        classification=SUNSET_RETIREMENT,
                        scope="entry",
                        target=target,
                        rule_id="COMPAT-DEPRECATE-001",
                        detail=(
                            f"completed grace window DEPRECATED -> SUNSET at epoch "
                            f"{cand_ent.sunset_in_epoch}"
                        ),
                    )
                )
        elif transition == ("SUNSET", "SUNSET"):
            items.append(
                DiffItem(
                    classification=UNCHANGED,
                    scope="entry",
                    target=target,
                    rule_id="COMPAT-DEPRECATE-001",
                    detail="SUNSET tombstone unchanged",
                )
            )
        else:
            items.append(
                DiffItem(
                    classification=BREAKING_VIOLATION,
                    scope="entry",
                    target=target,
                    rule_id="COMPAT-DEPRECATE-001",
                    detail=f"illegal backward lifecycle state transition {base_ent.state} -> {cand_ent.state}",
                )
            )

    return CompatDiffReport(
        baseline_manifest=baseline_label,
        candidate_manifest=candidate_label,
        baseline_epoch=baseline.current_epoch,
        candidate_epoch=candidate.current_epoch,
        items=items,
    )


# ---------------------------------------------------------------------------
# Optional .proto directory wire-compatibility scanner (pure Python stdlib)
# ---------------------------------------------------------------------------


@dataclass
class ProtoContainer:
    kind: str  # "message" or "enum"
    name: str
    fields_by_number: dict[int, tuple[str, str]] = field(default_factory=dict)  # num -> (type, name)
    reserved_numbers: set[int] = field(default_factory=set)
    reserved_names: set[str] = field(default_factory=set)


_COMMENT_BLOCK_RE = re.compile(r"/\*.*?\*/", re.DOTALL)
_COMMENT_LINE_RE = re.compile(r"//[^\n]*")
_PROTO_FIELD_RE = re.compile(
    r"^(?:(repeated|optional)\s+)?([A-Za-z0-9_.]+)\s+([A-Za-z_][A-Za-z0-9_]*)\s*=\s*([0-9]+)\b"
)
_PROTO_ENUM_VAL_RE = re.compile(r"^([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(-?[0-9]+)\b")


def _parse_reserved_stmt(body: str, nums: set[int], names: set[str]) -> None:
    if '"' in body or "'" in body:
        for match in re.finditer(r"""['"]([^'"]+)['"]""", body):
            names.add(match.group(1))
        return
    for part in body.split(","):
        token = part.strip()
        if not token:
            continue
        if " to " in token:
            low_s, high_s = token.split(" to ", 1)
            low = int(low_s.strip(), 10)
            high = low + 1024 if high_s.strip() == "max" else int(high_s.strip(), 10)
            for num in range(low, min(high, low + 4096) + 1):
                nums.add(num)
        elif token.isdigit():
            nums.add(int(token, 10))


def parse_proto_directory(proto_dir: Path) -> dict[str, ProtoContainer]:
    """Parses top-level message and enum declarations across all `.proto` files in `proto_dir`."""
    containers: dict[str, ProtoContainer] = {}
    for proto_file in sorted(proto_dir.rglob("*.proto")):
        raw = proto_file.read_text(encoding="utf-8")
        no_block = _COMMENT_BLOCK_RE.sub("", raw)
        cleaned = _COMMENT_LINE_RE.sub("", no_block)

        rel_file = proto_file.relative_to(proto_dir).as_posix()
        stack: list[tuple[str, ProtoContainer | None]] = []

        # Tokenize by braces and semicolons
        tokens = re.split(r"([{};])", cleaned)
        pending = ""
        for tok in tokens:
            if tok == "{":
                header = pending.strip()
                pending = ""
                msg_m = re.match(r"^message\s+([A-Za-z_][A-Za-z0-9_]*)$", header)
                enum_m = re.match(r"^enum\s+([A-Za-z_][A-Za-z0-9_]*)$", header)
                oneof_m = re.match(r"^oneof\s+([A-Za-z_][A-Za-z0-9_]*)$", header)
                if msg_m:
                    prefix = ".".join(c.name for kind, c in stack if c is not None and kind == "message")
                    fq_name = f"{rel_file}::{prefix + '.' if prefix else ''}{msg_m.group(1)}"
                    obj = ProtoContainer(kind="message", name=msg_m.group(1))
                    containers[fq_name] = obj
                    stack.append(("message", obj))
                elif enum_m:
                    prefix = ".".join(c.name for kind, c in stack if c is not None and kind == "message")
                    fq_name = f"{rel_file}::{prefix + '.' if prefix else ''}{enum_m.group(1)}"
                    obj = ProtoContainer(kind="enum", name=enum_m.group(1))
                    containers[fq_name] = obj
                    stack.append(("enum", obj))
                elif oneof_m:
                    parent_obj = next((c for kind, c in reversed(stack) if kind == "message"), None)
                    stack.append(("oneof", parent_obj))
                else:
                    stack.append(("other", None))
            elif tok == "}":
                pending = ""
                if stack:
                    stack.pop()
            elif tok == ";":
                stmt = " ".join(pending.strip().split())
                pending = ""
                if not stmt or not stack:
                    continue
                top_kind, top_obj = stack[-1]
                if top_obj is None:
                    continue
                if stmt.startswith("reserved "):
                    _parse_reserved_stmt(
                        stmt[len("reserved ") :], top_obj.reserved_numbers, top_obj.reserved_names
                    )
                elif top_kind in ("message", "oneof") and top_obj.kind == "message":
                    fm = _PROTO_FIELD_RE.match(stmt)
                    if fm:
                        qualifier = fm.group(1) or ""
                        ftype = f"{qualifier + ' ' if qualifier else ''}{fm.group(2)}"
                        fname = fm.group(3)
                        fnum = int(fm.group(4), 10)
                        top_obj.fields_by_number[fnum] = (ftype, fname)
                elif top_kind == "enum" and top_obj.kind == "enum":
                    em = _PROTO_ENUM_VAL_RE.match(stmt)
                    if em:
                        ename = em.group(1)
                        enum_num = int(em.group(2), 10)
                        top_obj.fields_by_number[enum_num] = ("ENUM_VALUE", ename)
            else:
                pending += " " + tok

    return containers


def diff_proto_directories(
    baseline_dir: Path,
    candidate_dir: Path,
    candidate_manifest: DeprecationManifestDoc,
) -> list[DiffItem]:
    """Cross-checks `.proto` wire layouts between baseline and candidate directories."""
    base_map = parse_proto_directory(baseline_dir)
    cand_map = parse_proto_directory(candidate_dir)
    items: list[DiffItem] = []

    sunset_symbols = {
        ent.symbol.split(".")[-1]
        for ent in candidate_manifest.entries.values()
        if ent.state == "SUNSET"
    }
    sunset_field_nums = {
        ent.field_number
        for ent in candidate_manifest.entries.values()
        if ent.state == "SUNSET" and ent.field_number is not None
    }
    for ch in candidate_manifest.channels.values():
        sunset_symbols.update(s.split(".")[-1] for s in ch.reserved_symbols)
        sunset_field_nums.update(ch.reserved_field_numbers)

    for fq_name in sorted(set(base_map) | set(cand_map)):
        b_obj = base_map.get(fq_name)
        c_obj = cand_map.get(fq_name)

        if b_obj is not None and c_obj is None:
            items.append(
                DiffItem(
                    classification=BREAKING_VIOLATION,
                    scope="proto",
                    target=fq_name,
                    rule_id="COMPAT-VER-001",
                    detail=f"deleted {b_obj.kind} {b_obj.name!r} from .proto schema",
                )
            )
            continue

        assert c_obj is not None
        if b_obj is None:
            items.append(
                DiffItem(
                    classification=ADDITIVE_SAFE,
                    scope="proto",
                    target=fq_name,
                    rule_id="COMPAT-VER-001",
                    detail=f"added new .proto {c_obj.kind} {c_obj.name!r}",
                )
            )
            continue

        dropped_res_nums = sorted(b_obj.reserved_numbers - c_obj.reserved_numbers)
        if dropped_res_nums:
            items.append(
                DiffItem(
                    classification=BREAKING_VIOLATION,
                    scope="proto",
                    target=fq_name,
                    rule_id="COMPAT-DEPRECATE-001",
                    detail=f"dropped reserved field numbers {dropped_res_nums}",
                )
            )

        dropped_res_names = sorted(b_obj.reserved_names - c_obj.reserved_names)
        if dropped_res_names:
            items.append(
                DiffItem(
                    classification=BREAKING_VIOLATION,
                    scope="proto",
                    target=fq_name,
                    rule_id="COMPAT-DEPRECATE-001",
                    detail=f"dropped reserved field names {dropped_res_names}",
                )
            )

        effective_res_nums = b_obj.reserved_numbers | c_obj.reserved_numbers
        effective_res_names = b_obj.reserved_names | c_obj.reserved_names

        for fnum, (ctype, cname) in sorted(c_obj.fields_by_number.items()):
            if fnum in effective_res_nums or cname in effective_res_names:
                items.append(
                    DiffItem(
                        classification=BREAKING_VIOLATION,
                        scope="proto",
                        target=f"{fq_name}#{fnum}",
                        rule_id="COMPAT-DEPRECATE-001",
                        detail=f"field {cname!r} (#{fnum}) reuses reserved .proto tombstone",
                    )
                )

        all_nums = sorted(set(b_obj.fields_by_number) | set(c_obj.fields_by_number))
        container_unchanged = True
        for fnum in all_nums:
            b_field = b_obj.fields_by_number.get(fnum)
            c_field = c_obj.fields_by_number.get(fnum)
            field_target = f"{fq_name}#{fnum}"

            if b_field is not None and c_field is None:
                container_unchanged = False
                _, b_name = b_field
                properly_reserved = fnum in c_obj.reserved_numbers and b_name in c_obj.reserved_names
                authorized_sunset = fnum in sunset_field_nums or b_name in sunset_symbols
                if properly_reserved and authorized_sunset:
                    items.append(
                        DiffItem(
                            classification=SUNSET_RETIREMENT,
                            scope="proto",
                            target=field_target,
                            rule_id="COMPAT-DEPRECATE-001",
                            detail=f"retired SUNSET field {b_name!r} (#{fnum}) into `reserved` tombstones",
                        )
                    )
                else:
                    items.append(
                        DiffItem(
                            classification=BREAKING_VIOLATION,
                            scope="proto",
                            target=field_target,
                            rule_id="COMPAT-DEPRECATE-001",
                            detail=(
                                f"removed field {b_name!r} (#{fnum}) without both `reserved` "
                                "tombstones and manifest SUNSET authorization"
                            ),
                        )
                    )
            elif b_field is None and c_field is not None:
                container_unchanged = False
                ctype, cname = c_field
                items.append(
                    DiffItem(
                        classification=ADDITIVE_SAFE,
                        scope="proto",
                        target=field_target,
                        rule_id="COMPAT-VER-001",
                        detail=f"added optional/additive field `{ctype} {cname} = {fnum}`",
                    )
                )
            elif b_field is not None and c_field is not None and b_field != c_field:
                container_unchanged = False
                items.append(
                    DiffItem(
                        classification=BREAKING_VIOLATION,
                        scope="proto",
                        target=field_target,
                        rule_id="COMPAT-VER-001",
                        detail=(
                            f"mutated wire slot #{fnum} from `{b_field[0]} {b_field[1]}` "
                            f"to `{c_field[0]} {c_field[1]}`"
                        ),
                    )
                )

        if container_unchanged:
            items.append(
                DiffItem(
                    classification=UNCHANGED,
                    scope="proto",
                    target=fq_name,
                    rule_id="COMPAT-VER-001",
                    detail=f"{c_obj.kind} {c_obj.name!r} wire layout unchanged",
                )
            )

    return items


def build_arg_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="sabi_compat_diff.py",
        description="Cross-epoch SABI deprecation-manifest and .proto compatibility diff checker.",
    )
    parser.add_argument("baseline_yaml", help="Path to baseline deprecation-manifest.yaml")
    parser.add_argument("candidate_yaml", help="Path to candidate deprecation-manifest.yaml")
    parser.add_argument(
        "--baseline-proto-dir",
        help="Optional directory containing baseline .proto files",
        default=None,
    )
    parser.add_argument(
        "--candidate-proto-dir",
        help="Optional directory containing candidate .proto files",
        default=None,
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="Emit machine-readable RFC 8259 JSON report",
    )
    return parser


def main(argv: list[str] | None = None) -> int:
    parser = build_arg_parser()
    args = parser.parse_args(argv)

    if bool(args.baseline_proto_dir) != bool(args.candidate_proto_dir):
        print(
            "error: --baseline-proto-dir and --candidate-proto-dir must be provided together",
            file=sys.stderr,
        )
        return 2

    baseline_path = Path(args.baseline_yaml)
    candidate_path = Path(args.candidate_yaml)

    try:
        baseline_text = baseline_path.read_text(encoding="utf-8")
        candidate_text = candidate_path.read_text(encoding="utf-8")
        base_doc, base_violations = parse_deprecation_manifest_yaml(baseline_text)
        if base_violations:
            raise ManifestParseError(
                f"baseline manifest itself has structural errors: {base_violations[0].detail}"
            )
        cand_doc, cand_violations = parse_deprecation_manifest_yaml(candidate_text)
    except (OSError, ManifestParseError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2

    report = diff_deprecation_manifests(
        base_doc,
        cand_doc,
        cand_violations,
        baseline_label=baseline_path.as_posix(),
        candidate_label=candidate_path.as_posix(),
    )

    if args.baseline_proto_dir and args.candidate_proto_dir:
        base_proto = Path(args.baseline_proto_dir)
        cand_proto = Path(args.candidate_proto_dir)
        if not base_proto.is_dir() or not cand_proto.is_dir():
            print("error: proto directories must exist and be directories", file=sys.stderr)
            return 2
        report.items.extend(diff_proto_directories(base_proto, cand_proto, cand_doc))

    if args.json:
        sys.stdout.write(report.render_json())
    else:
        sys.stdout.write(report.render_text())

    return report.exit_code


if __name__ == "__main__":
    raise SystemExit(main())
