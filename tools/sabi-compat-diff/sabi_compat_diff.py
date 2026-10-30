#!/usr/bin/env python3
"""Static SABI contract evolution and deprecation-manifest compatibility checker.

Compares a baseline `.proto` directory + `deprecation-manifest.yaml` against a
candidate `.proto` directory + `deprecation-manifest.yaml` (or runs `--self-check`
against `schema/nlos/sabi/v1/`), enforcing `ADR-0014` and `ADR-0027` invariants
with zero external Python dependencies:

1. Rejects removal of existing `.proto` files, `message`, `enum`, `service`, or
   `rpc` declarations, and rejects field/enum-value deletion unless both numeric
   (`reserved <num>;`) and identifier (`reserved "<name>";`) tombstones are
   recorded on the enclosing type.
2. Rejects changing an existing field's type, cardinality, oneof membership, or
   name at the same field number, and rejects reusing or deleting `reserved`
   field/enum/channel tombstone slots.
3. Rejects `deprecation-manifest.yaml` lifecycle violations: `ACTIVE -> SUNSET`
   state skipping, state regressions, `earliest_sunset_epoch < deprecated_in_epoch + 2`,
   premature sunset before `earliest_sunset_epoch`, and `RESERVED` symbol/slot reuse.
4. Permits compliant additive changes: new messages/enums/services, new optional
   `rpc` methods, new fields on fresh unreserved numbers, `ACTIVE -> DEPRECATED`
   with a `>= 2` epoch grace window, and full-window `DEPRECATED -> SUNSET`.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass, field
import json
from pathlib import Path
import re
import sys
from typing import Any

MIN_COMPATIBILITY_WINDOW_EPOCHS = 2
VALID_STATES = frozenset({"ACTIVE", "DEPRECATED", "SUNSET"})
VALID_KINDS = frozenset({"MESSAGE", "FIELD", "METHOD", "ENUM_VALUE", "EXTENSION"})
MAX_PROTO_FIELD_NUMBER = 536_870_911
PROTO_RESERVED_FIRST = 19_000
PROTO_RESERVED_LAST = 19_999


@dataclass(frozen=True)
class CompatFinding:
    """Single deterministic compatibility diagnostic."""

    code: str
    severity: str  # "ERROR" or "INFO"
    location: str
    message: str

    def to_dict(self) -> dict[str, str]:
        return {
            "code": self.code,
            "location": self.location,
            "message": self.message,
            "severity": self.severity,
        }


@dataclass
class NumberRange:
    start: int
    end: int

    def contains(self, value: int) -> bool:
        return self.start <= value <= self.end


@dataclass
class ReservedSet:
    ranges: list[NumberRange] = field(default_factory=list)
    names: set[str] = field(default_factory=set)

    def add_range(self, start: int, end: int) -> None:
        self.ranges.append(NumberRange(start=start, end=end))

    def has_number(self, number: int) -> bool:
        return any(r.contains(number) for r in self.ranges)

    def explicit_numbers_up_to(self, ceiling: int = 65_536) -> set[int]:
        out: set[int] = set()
        for r in self.ranges:
            hi = min(r.end, ceiling)
            if r.start <= hi:
                out.update(range(r.start, hi + 1))
        return out


@dataclass(frozen=True)
class ProtoField:
    name: str
    number: int
    type_name: str
    cardinality: str  # "singular", "optional", "repeated", "map"
    oneof_group: str | None = None


@dataclass
class ProtoMessage:
    full_name: str
    file_name: str
    fields_by_number: dict[int, ProtoField] = field(default_factory=dict)
    fields_by_name: dict[str, ProtoField] = field(default_factory=dict)
    oneof_groups: set[str] = field(default_factory=set)
    reserved: ReservedSet = field(default_factory=ReservedSet)


@dataclass(frozen=True)
class ProtoEnumValue:
    name: str
    number: int


@dataclass
class ProtoEnum:
    full_name: str
    file_name: str
    values_by_number: dict[int, ProtoEnumValue] = field(default_factory=dict)
    values_by_name: dict[str, ProtoEnumValue] = field(default_factory=dict)
    reserved: ReservedSet = field(default_factory=ReservedSet)


@dataclass(frozen=True)
class ProtoRpc:
    name: str
    request_type: str
    response_type: str
    client_streaming: bool
    server_streaming: bool


@dataclass
class ProtoService:
    full_name: str
    file_name: str
    rpcs: dict[str, ProtoRpc] = field(default_factory=dict)


@dataclass
class ProtoCorpus:
    files: set[str] = field(default_factory=set)
    packages_by_file: dict[str, str] = field(default_factory=dict)
    messages: dict[str, ProtoMessage] = field(default_factory=dict)
    enums: dict[str, ProtoEnum] = field(default_factory=dict)
    services: dict[str, ProtoService] = field(default_factory=dict)


@dataclass
class ChannelRecord:
    name: str
    major: int
    minor: int
    frozen: bool
    reserved_field_numbers: list[int] = field(default_factory=list)
    reserved_symbols: list[str] = field(default_factory=list)


@dataclass
class ManifestEntry:
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


@dataclass
class DeprecationManifestDoc:
    manifest_version: int
    current_epoch: int
    min_compatibility_window_epochs: int
    frozen_channels_total: int
    channels: dict[str, ChannelRecord]
    entries: dict[tuple[str, str], ManifestEntry]


class SchemaParseError(Exception):
    """Raised when a `.proto` file or `deprecation-manifest.yaml` is malformed."""


def proto_stem_to_channel_name(file_name: str) -> str:
    stem = Path(file_name).stem
    pascal = "".join(part.capitalize() for part in stem.split("_") if part)
    return f"nlos.sabi.{pascal}"


def _strip_proto_comments(source: str) -> str:
    """Remove `//` and `/* ... */` comments while preserving string literals and newlines."""
    result: list[str] = []
    i = 0
    n = len(source)
    in_string = False
    quote_char = ""
    while i < n:
        ch = source[i]
        if in_string:
            result.append(ch)
            if ch == "\\" and i + 1 < n:
                result.append(source[i + 1])
                i += 2
                continue
            if ch == quote_char:
                in_string = False
            i += 1
            continue

        if ch in ('"', "'"):
            in_string = True
            quote_char = ch
            result.append(ch)
            i += 1
            continue

        if ch == "/" and i + 1 < n and source[i + 1] == "/":
            i += 2
            while i < n and source[i] != "\n":
                i += 1
            continue

        if ch == "/" and i + 1 < n and source[i + 1] == "*":
            i += 2
            while i + 1 < n and not (source[i] == "*" and source[i + 1] == "/"):
                if source[i] == "\n":
                    result.append("\n")
                i += 1
            if i + 1 >= n:
                raise SchemaParseError("unterminated block comment in .proto")
            i += 2
            continue

        result.append(ch)
        i += 1
    return "".join(result)


_TOKEN_RE = re.compile(
    r'"(?:\\.|[^"\\])*"|\'(?:\\.|[^\'\\])*\'|[{}();=,\[\]<>]|[^\s{}();=,\[\]<>]+'
)


def _tokenize_proto(source: str) -> list[str]:
    cleaned = _strip_proto_comments(source)
    return _TOKEN_RE.findall(cleaned)


class _TokenStream:
    def __init__(self, tokens: list[str], file_name: str) -> None:
        self.tokens = tokens
        self.pos = 0
        self.file_name = file_name

    def eof(self) -> bool:
        return self.pos >= len(self.tokens)

    def peek(self, offset: int = 0) -> str | None:
        idx = self.pos + offset
        return self.tokens[idx] if idx < len(self.tokens) else None

    def next_token(self) -> str:
        if self.pos >= len(self.tokens):
            raise SchemaParseError(f"{self.file_name}: unexpected EOF")
        tok = self.tokens[self.pos]
        self.pos += 1
        return tok

    def expect(self, expected: str) -> None:
        got = self.next_token()
        if got != expected:
            raise SchemaParseError(
                f"{self.file_name}: expected {expected!r}, got {got!r}"
            )

    def skip_statement_or_block(self) -> None:
        depth = 0
        while not self.eof():
            tok = self.next_token()
            if tok == "{":
                depth += 1
            elif tok == "}":
                if depth <= 1:
                    return
                depth -= 1
            elif tok == ";" and depth == 0:
                return


def _unquote(literal: str, file_name: str) -> str:
    if len(literal) < 2 or literal[0] not in ('"', "'") or literal[-1] != literal[0]:
        raise SchemaParseError(f"{file_name}: expected quoted string, got {literal!r}")
    return literal[1:-1]


def _parse_field_options_if_any(stream: _TokenStream) -> None:
    if stream.peek() == "[":
        depth = 0
        while not stream.eof():
            tok = stream.next_token()
            if tok == "[":
                depth += 1
            elif tok == "]":
                depth -= 1
                if depth == 0:
                    return
        raise SchemaParseError(f"{stream.file_name}: unterminated field options '['")


def _parse_reserved(stream: _TokenStream, target: ReservedSet) -> None:
    first = stream.peek()
    if first is None:
        raise SchemaParseError(f"{stream.file_name}: empty reserved statement")
    if first.startswith(('"', "'")):
        while True:
            lit = _unquote(stream.next_token(), stream.file_name)
            if not lit:
                raise SchemaParseError(f"{stream.file_name}: empty reserved name")
            target.names.add(lit)
            sep = stream.next_token()
            if sep == ";":
                break
            if sep != ",":
                raise SchemaParseError(
                    f"{stream.file_name}: expected ',' or ';' in reserved names, got {sep!r}"
                )
    else:
        while True:
            start_tok = stream.next_token()
            try:
                start = int(start_tok, 0)
            except ValueError as exc:
                raise SchemaParseError(
                    f"{stream.file_name}: invalid reserved number {start_tok!r}"
                ) from exc
            if stream.peek() == "to":
                stream.next_token()
                end_tok = stream.next_token()
                end = MAX_PROTO_FIELD_NUMBER if end_tok == "max" else int(end_tok, 0)
            else:
                end = start
            if start <= 0 or end < start or end > MAX_PROTO_FIELD_NUMBER:
                raise SchemaParseError(
                    f"{stream.file_name}: invalid reserved range {start}..{end}"
                )
            target.add_range(start, end)
            sep = stream.next_token()
            if sep == ";":
                break
            if sep != ",":
                raise SchemaParseError(
                    f"{stream.file_name}: expected ',' or ';' in reserved numbers, got {sep!r}"
                )


def _register_message_field(
    msg: ProtoMessage,
    field_obj: ProtoField,
    findings: list[CompatFinding],
) -> None:
    loc = f"{msg.file_name}:{msg.full_name}.{field_obj.name}"
    if (
        field_obj.number <= 0
        or field_obj.number > MAX_PROTO_FIELD_NUMBER
        or PROTO_RESERVED_FIRST <= field_obj.number <= PROTO_RESERVED_LAST
    ):
        findings.append(
            CompatFinding(
                code="E_PROTO_INVALID_FIELD_NUMBER",
                severity="ERROR",
                location=loc,
                message=f"field number {field_obj.number} is outside legal Protobuf bounds",
            )
        )
    if field_obj.number in msg.fields_by_number:
        findings.append(
            CompatFinding(
                code="E_PROTO_DUPLICATE_FIELD_NUMBER",
                severity="ERROR",
                location=loc,
                message=f"duplicate field number {field_obj.number} in message {msg.full_name}",
            )
        )
    if field_obj.name in msg.fields_by_name:
        findings.append(
            CompatFinding(
                code="E_PROTO_DUPLICATE_FIELD_NAME",
                severity="ERROR",
                location=loc,
                message=f"duplicate field name {field_obj.name!r} in message {msg.full_name}",
            )
        )
    msg.fields_by_number[field_obj.number] = field_obj
    msg.fields_by_name[field_obj.name] = field_obj


def _parse_field_declaration(
    stream: _TokenStream,
    *,
    oneof_group: str | None,
) -> ProtoField:
    cardinality = "singular"
    first = stream.peek()
    if first in ("optional", "repeated", "required"):
        if oneof_group is not None:
            raise SchemaParseError(
                f"{stream.file_name}: cardinality {first!r} not permitted inside oneof"
            )
        cardinality = stream.next_token()

    if stream.peek() == "map":
        stream.next_token()
        stream.expect("<")
        key_type = stream.next_token()
        stream.expect(",")
        val_type = stream.next_token()
        stream.expect(">")
        type_name = f"map<{key_type},{val_type}>"
        cardinality = "map"
    else:
        type_name = stream.next_token()

    field_name = stream.next_token()
    stream.expect("=")
    num_tok = stream.next_token()
    try:
        field_num = int(num_tok, 0)
    except ValueError as exc:
        raise SchemaParseError(
            f"{stream.file_name}: invalid field number {num_tok!r} on {field_name}"
        ) from exc
    _parse_field_options_if_any(stream)
    stream.expect(";")
    return ProtoField(
        name=field_name,
        number=field_num,
        type_name=type_name,
        cardinality=cardinality,
        oneof_group=oneof_group,
    )


def _parse_enum(
    stream: _TokenStream,
    prefix: str,
    corpus: ProtoCorpus,
    findings: list[CompatFinding],
) -> None:
    name = stream.next_token()
    full_name = f"{prefix}.{name}" if prefix else name
    stream.expect("{")
    enum_obj = ProtoEnum(full_name=full_name, file_name=stream.file_name)
    while not stream.eof():
        tok = stream.peek()
        if tok == "}":
            stream.next_token()
            break
        if tok == ";":
            stream.next_token()
            continue
        if tok == "option":
            stream.next_token()
            stream.skip_statement_or_block()
            continue
        if tok == "reserved":
            stream.next_token()
            _parse_reserved(stream, enum_obj.reserved)
            continue

        val_name = stream.next_token()
        stream.expect("=")
        num_tok = stream.next_token()
        val_num = int(num_tok, 0)
        _parse_field_options_if_any(stream)
        stream.expect(";")
        val = ProtoEnumValue(name=val_name, number=val_num)
        if val_num in enum_obj.values_by_number:
            findings.append(
                CompatFinding(
                    code="E_PROTO_DUPLICATE_ENUM_NUMBER",
                    severity="ERROR",
                    location=f"{stream.file_name}:{full_name}.{val_name}",
                    message=f"duplicate enum number {val_num} in {full_name}",
                )
            )
        enum_obj.values_by_number[val_num] = val
        enum_obj.values_by_name[val_name] = val

    for val in enum_obj.values_by_number.values():
        if enum_obj.reserved.has_number(val.number) or val.name in enum_obj.reserved.names:
            findings.append(
                CompatFinding(
                    code="E_RESERVED_SLOT_REUSE",
                    severity="ERROR",
                    location=f"{stream.file_name}:{full_name}.{val.name}",
                    message=f"enum value {val.name}={val.number} occupies a reserved slot",
                )
            )
    corpus.enums[full_name] = enum_obj


def _parse_message(
    stream: _TokenStream,
    prefix: str,
    corpus: ProtoCorpus,
    findings: list[CompatFinding],
) -> None:
    name = stream.next_token()
    full_name = f"{prefix}.{name}" if prefix else name
    stream.expect("{")
    msg = ProtoMessage(full_name=full_name, file_name=stream.file_name)

    while not stream.eof():
        tok = stream.peek()
        if tok == "}":
            stream.next_token()
            break
        if tok == ";":
            stream.next_token()
            continue
        if tok == "option":
            stream.next_token()
            stream.skip_statement_or_block()
            continue
        if tok == "reserved":
            stream.next_token()
            _parse_reserved(stream, msg.reserved)
            continue
        if tok == "message":
            stream.next_token()
            _parse_message(stream, full_name, corpus, findings)
            continue
        if tok == "enum":
            stream.next_token()
            _parse_enum(stream, full_name, corpus, findings)
            continue
        if tok == "oneof":
            stream.next_token()
            oneof_name = stream.next_token()
            msg.oneof_groups.add(oneof_name)
            stream.expect("{")
            while not stream.eof():
                if stream.peek() == "}":
                    stream.next_token()
                    break
                if stream.peek() == "option":
                    stream.next_token()
                    stream.skip_statement_or_block()
                    continue
                if stream.peek() == ";":
                    stream.next_token()
                    continue
                oneof_field = _parse_field_declaration(stream, oneof_group=oneof_name)
                _register_message_field(msg, oneof_field, findings)
            continue

        field_obj = _parse_field_declaration(stream, oneof_group=None)
        _register_message_field(msg, field_obj, findings)

    for fld in msg.fields_by_number.values():
        if msg.reserved.has_number(fld.number) or fld.name in msg.reserved.names:
            findings.append(
                CompatFinding(
                    code="E_RESERVED_SLOT_REUSE",
                    severity="ERROR",
                    location=f"{stream.file_name}:{full_name}.{fld.name}",
                    message=(
                        f"field {fld.name}={fld.number} in message {full_name} "
                        "reuses a reserved field number or name"
                    ),
                )
            )
    corpus.messages[full_name] = msg


def _parse_service(stream: _TokenStream, corpus: ProtoCorpus) -> None:
    name = stream.next_token()
    stream.expect("{")
    svc = ProtoService(full_name=name, file_name=stream.file_name)
    while not stream.eof():
        tok = stream.peek()
        if tok == "}":
            stream.next_token()
            break
        if tok == ";":
            stream.next_token()
            continue
        if tok == "option":
            stream.next_token()
            stream.skip_statement_or_block()
            continue
        if tok == "rpc":
            stream.next_token()
            rpc_name = stream.next_token()
            stream.expect("(")
            client_stream = False
            if stream.peek() == "stream":
                stream.next_token()
                client_stream = True
            req_type = stream.next_token()
            stream.expect(")")
            stream.expect("returns")
            stream.expect("(")
            server_stream = False
            if stream.peek() == "stream":
                stream.next_token()
                server_stream = True
            resp_type = stream.next_token()
            stream.expect(")")
            if stream.peek() == "{":
                stream.skip_statement_or_block()
            else:
                stream.expect(";")
            svc.rpcs[rpc_name] = ProtoRpc(
                name=rpc_name,
                request_type=req_type,
                response_type=resp_type,
                client_streaming=client_stream,
                server_streaming=server_stream,
            )
            continue
        raise SchemaParseError(
            f"{stream.file_name}: unexpected token {tok!r} in service {name}"
        )
    corpus.services[name] = svc


def parse_proto_directory(proto_dir: Path) -> tuple[ProtoCorpus, list[CompatFinding]]:
    """Parse all `*.proto` files in `proto_dir` into a normalized `ProtoCorpus`."""
    if not proto_dir.is_dir():
        raise SchemaParseError(f"proto directory does not exist: {proto_dir}")

    proto_files = sorted(proto_dir.glob("*.proto"))
    if not proto_files:
        raise SchemaParseError(f"no .proto files found in {proto_dir}")

    corpus = ProtoCorpus()
    findings: list[CompatFinding] = []

    for proto_file in proto_files:
        rel_name = proto_file.name
        corpus.files.add(rel_name)
        tokens = _tokenize_proto(proto_file.read_text(encoding="utf-8"))
        stream = _TokenStream(tokens, rel_name)
        pkg_name = ""
        while not stream.eof():
            tok = stream.next_token()
            if tok == "syntax":
                stream.expect("=")
                syntax_val = _unquote(stream.next_token(), rel_name)
                if syntax_val != "proto3":
                    raise SchemaParseError(
                        f"{rel_name}: unsupported syntax {syntax_val!r}, expected 'proto3'"
                    )
                stream.expect(";")
            elif tok == "package":
                parts: list[str] = []
                while not stream.eof() and stream.peek() != ";":
                    parts.append(stream.next_token())
                stream.expect(";")
                pkg_name = "".join(parts)
            elif tok in ("import", "option"):
                stream.skip_statement_or_block()
            elif tok == "message":
                _parse_message(stream, "", corpus, findings)
            elif tok == "enum":
                _parse_enum(stream, "", corpus, findings)
            elif tok == "service":
                _parse_service(stream, corpus)
            elif tok == ";":
                continue
            else:
                raise SchemaParseError(f"{rel_name}: unexpected top-level token {tok!r}")
        corpus.packages_by_file[rel_name] = pkg_name

    return corpus, findings


def _strip_yaml_comment(raw_line: str, line_no: int) -> str:
    in_quotes = False
    quote_ch = ""
    escaped = False
    for idx, ch in enumerate(raw_line):
        if escaped:
            escaped = False
            continue
        if in_quotes:
            if ch == "\\" and quote_ch == '"':
                escaped = True
            elif ch == quote_ch:
                in_quotes = False
        else:
            if ch in ('"', "'"):
                in_quotes = True
                quote_ch = ch
            elif ch == "#":
                return raw_line[:idx]
    if in_quotes:
        raise SchemaParseError(f"deprecation-manifest line {line_no}: unterminated quote")
    return raw_line


def _parse_yaml_scalar_str(raw: str, line_no: int) -> str:
    val = raw.strip()
    if not val:
        raise SchemaParseError(f"deprecation-manifest line {line_no}: empty string scalar")
    if val[0] in ('"', "'"):
        if len(val) < 2 or val[-1] != val[0]:
            raise SchemaParseError(
                f"deprecation-manifest line {line_no}: malformed quoted scalar {val!r}"
            )
        return val[1:-1]
    return val


def _parse_yaml_optional_str(raw: str, line_no: int) -> str | None:
    val = raw.strip()
    if val in ("", "null", "~"):
        return None
    return _parse_yaml_scalar_str(val, line_no)


def _parse_yaml_int(raw: str, line_no: int, field_name: str) -> int:
    val = raw.strip()
    try:
        return int(val, 10)
    except ValueError as exc:
        raise SchemaParseError(
            f"deprecation-manifest line {line_no}: invalid integer for {field_name}: {val!r}"
        ) from exc


def _parse_yaml_optional_int(raw: str, line_no: int, field_name: str) -> int | None:
    val = raw.strip()
    if val in ("", "null", "~"):
        return None
    return _parse_yaml_int(val, line_no, field_name)


def _parse_yaml_inline_list(raw: str, line_no: int, as_int: bool) -> list[Any]:
    val = raw.strip()
    if not (val.startswith("[") and val.endswith("]")):
        raise SchemaParseError(
            f"deprecation-manifest line {line_no}: expected inline list [...], got {val!r}"
        )
    inner = val[1:-1].strip()
    if not inner:
        return []
    items = [part.strip() for part in inner.split(",")]
    if as_int:
        return [_parse_yaml_int(item, line_no, "list_item") for item in items]
    return [_parse_yaml_scalar_str(item, line_no) for item in items]


def parse_deprecation_manifest_file(
    manifest_path: Path,
) -> tuple[DeprecationManifestDoc, list[CompatFinding]]:
    """Parse and validate `deprecation-manifest.yaml` using pure standard-library Python."""
    if not manifest_path.is_file():
        raise SchemaParseError(f"deprecation manifest does not exist: {manifest_path}")

    text = manifest_path.read_text(encoding="utf-8")
    if "\0" in text:
        raise SchemaParseError(f"{manifest_path}: NUL byte is forbidden")

    root_scalars: dict[str, int] = {}
    raw_channels: list[dict[str, Any]] = []
    raw_entries: list[dict[str, Any]] = []
    section = "root"
    pending_list_key: str | None = None
    pending_list_indent = 0

    for idx, raw_line in enumerate(text.splitlines()):
        line_no = idx + 1
        stripped = _strip_yaml_comment(raw_line, line_no)
        if not stripped.strip():
            continue
        if "\t" in stripped:
            raise SchemaParseError(f"deprecation-manifest line {line_no}: tabs are forbidden")

        indent = len(stripped) - len(stripped.lstrip(" "))
        content = stripped.strip()

        if pending_list_key is not None and indent > pending_list_indent:
            if not content.startswith("-"):
                raise SchemaParseError(
                    f"deprecation-manifest line {line_no}: expected '-' item in {pending_list_key}"
                )
            item_raw = content[1:].strip()
            target_ch = raw_channels[-1]
            if pending_list_key == "reserved_field_numbers":
                target_ch["reserved_field_numbers"].append(
                    _parse_yaml_int(item_raw, line_no, "reserved_field_numbers")
                )
            else:
                target_ch["reserved_symbols"].append(
                    _parse_yaml_scalar_str(item_raw, line_no)
                )
            continue
        pending_list_key = None

        if indent == 0:
            if ":" not in content:
                raise SchemaParseError(f"deprecation-manifest line {line_no}: missing ':'")
            key, val = content.split(":", 1)
            key = key.strip()
            val = val.strip()
            if key in (
                "manifest_version",
                "current_epoch",
                "min_compatibility_window_epochs",
                "frozen_channels_total",
            ):
                root_scalars[key] = _parse_yaml_int(val, line_no, key)
                section = "root"
            elif key == "channels":
                if val == "[]":
                    section = "root"
                elif val == "":
                    section = "channels"
                else:
                    raise SchemaParseError(
                        f"deprecation-manifest line {line_no}: invalid channels header"
                    )
            elif key == "entries":
                if val == "[]":
                    section = "root"
                elif val == "":
                    section = "entries"
                else:
                    raise SchemaParseError(
                        f"deprecation-manifest line {line_no}: invalid entries header"
                    )
            else:
                raise SchemaParseError(
                    f"deprecation-manifest line {line_no}: unknown top-level key {key!r}"
                )
            continue

        if section == "channels":
            if content.startswith("-"):
                raw_channels.append(
                    {
                        "_line": line_no,
                        "reserved_field_numbers": [],
                        "reserved_symbols": [],
                    }
                )
                rem = content[1:].strip()
                if rem:
                    prop_indent = len(stripped) - len(content[1:].lstrip(" "))
                    pending_list_key, pending_list_indent = _apply_channel_kv(
                        raw_channels[-1], rem, prop_indent, line_no
                    )
            else:
                if not raw_channels:
                    raise SchemaParseError(
                        f"deprecation-manifest line {line_no}: channel field before '-'"
                    )
                pending_list_key, pending_list_indent = _apply_channel_kv(
                    raw_channels[-1], content, indent, line_no
                )
        elif section == "entries":
            if content.startswith("-"):
                raw_entries.append({"_line": line_no})
                rem = content[1:].strip()
                if rem:
                    _apply_entry_kv(raw_entries[-1], rem, line_no)
            else:
                if not raw_entries:
                    raise SchemaParseError(
                        f"deprecation-manifest line {line_no}: entry field before '-'"
                    )
                _apply_entry_kv(raw_entries[-1], content, line_no)
        else:
            raise SchemaParseError(
                f"deprecation-manifest line {line_no}: unexpected indented line"
            )

    for req_key in (
        "manifest_version",
        "current_epoch",
        "min_compatibility_window_epochs",
        "frozen_channels_total",
    ):
        if req_key not in root_scalars:
            raise SchemaParseError(f"deprecation-manifest missing top-level {req_key!r}")

    findings: list[CompatFinding] = []
    manifest_version = root_scalars["manifest_version"]
    current_epoch = root_scalars["current_epoch"]
    min_window = root_scalars["min_compatibility_window_epochs"]
    frozen_total = root_scalars["frozen_channels_total"]

    if manifest_version != 1:
        findings.append(
            CompatFinding(
                code="E_MANIFEST_VERSION",
                severity="ERROR",
                location=manifest_path.name,
                message=f"unsupported manifest_version={manifest_version}, expected 1",
            )
        )
    if current_epoch < 1:
        findings.append(
            CompatFinding(
                code="E_INVALID_EPOCH",
                severity="ERROR",
                location=manifest_path.name,
                message=f"current_epoch={current_epoch} must be >= 1",
            )
        )
    if min_window < MIN_COMPATIBILITY_WINDOW_EPOCHS:
        findings.append(
            CompatFinding(
                code="E_COMPAT_WINDOW_TOO_SMALL",
                severity="ERROR",
                location=manifest_path.name,
                message=(
                    f"min_compatibility_window_epochs={min_window} < "
                    f"{MIN_COMPATIBILITY_WINDOW_EPOCHS}"
                ),
            )
        )

    channels: dict[str, ChannelRecord] = {}
    for raw_ch in raw_channels:
        ch_name = raw_ch.get("name")
        if not ch_name:
            raise SchemaParseError(f"channel at line {raw_ch['_line']} missing name")
        if ch_name in channels:
            findings.append(
                CompatFinding(
                    code="E_DUPLICATE_CHANNEL",
                    severity="ERROR",
                    location=f"{manifest_path.name}:{ch_name}",
                    message=f"duplicate channel {ch_name!r}",
                )
            )
        ch_rec = ChannelRecord(
            name=ch_name,
            major=int(raw_ch["major"]),
            minor=int(raw_ch["minor"]),
            frozen=bool(raw_ch["frozen"]),
            reserved_field_numbers=list(raw_ch["reserved_field_numbers"]),
            reserved_symbols=list(raw_ch["reserved_symbols"]),
        )
        if not ch_rec.frozen:
            findings.append(
                CompatFinding(
                    code="E_UNFROZEN_CHANNEL",
                    severity="ERROR",
                    location=f"{manifest_path.name}:{ch_name}",
                    message=f"channel {ch_name} must have frozen=true",
                )
            )
        channels[ch_name] = ch_rec

    if frozen_total != len(raw_channels):
        findings.append(
            CompatFinding(
                code="E_CHANNEL_COUNT_MISMATCH",
                severity="ERROR",
                location=manifest_path.name,
                message=(
                    f"frozen_channels_total={frozen_total} != actual channels={len(raw_channels)}"
                ),
            )
        )

    effective_window = max(min_window, MIN_COMPATIBILITY_WINDOW_EPOCHS)
    entries: dict[tuple[str, str], ManifestEntry] = {}
    sunset_symbols: set[tuple[str, str]] = set()
    sunset_fields: set[tuple[str, str | None, int]] = set()

    for raw_en in raw_entries:
        ch = raw_en.get("channel")
        sym = raw_en.get("symbol")
        kind = raw_en.get("kind")
        state = raw_en.get("state")
        intro = raw_en.get("introduced_in_epoch")
        if not ch or not sym or not kind or not state or intro is None:
            raise SchemaParseError(
                f"entry at line {raw_en['_line']} missing required core fields"
            )
        loc = f"{manifest_path.name}:{ch}:{sym}"
        if ch not in channels:
            findings.append(
                CompatFinding(
                    code="E_UNKNOWN_CHANNEL",
                    severity="ERROR",
                    location=loc,
                    message=f"entry {sym!r} references unregistered channel {ch!r}",
                )
            )
        if kind not in VALID_KINDS:
            findings.append(
                CompatFinding(
                    code="E_INVALID_ENTRY_KIND",
                    severity="ERROR",
                    location=loc,
                    message=f"invalid entry kind {kind!r}",
                )
            )
        if state not in VALID_STATES:
            findings.append(
                CompatFinding(
                    code="E_INVALID_ENTRY_STATE",
                    severity="ERROR",
                    location=loc,
                    message=f"invalid state {state!r}",
                )
            )
        entry_obj = ManifestEntry(
            channel=ch,
            symbol=sym,
            kind=kind,
            introduced_in_epoch=int(intro),
            state=state,
            parent_type=raw_en.get("parent_type"),
            field_number=raw_en.get("field_number"),
            deprecated_in_epoch=raw_en.get("deprecated_in_epoch"),
            earliest_sunset_epoch=raw_en.get("earliest_sunset_epoch"),
            sunset_in_epoch=raw_en.get("sunset_in_epoch"),
            replacement=raw_en.get("replacement"),
            migration_rule_id=raw_en.get("migration_rule_id"),
            reason=raw_en.get("reason"),
        )
        if (ch, sym) in entries:
            findings.append(
                CompatFinding(
                    code="E_DUPLICATE_ENTRY",
                    severity="ERROR",
                    location=loc,
                    message=f"duplicate entry ({ch}, {sym})",
                )
            )
        _validate_single_manifest_entry(
            entry_obj,
            current_epoch=current_epoch,
            effective_window=effective_window,
            location=loc,
            findings=findings,
        )
        if entry_obj.state == "SUNSET":
            sunset_symbols.add((ch, sym))
            if entry_obj.field_number is not None:
                sunset_fields.add((ch, entry_obj.parent_type, entry_obj.field_number))
        entries[(ch, sym)] = entry_obj

    for entry_obj in entries.values():
        if entry_obj.state == "SUNSET":
            continue
        loc = f"{manifest_path.name}:{entry_obj.channel}:{entry_obj.symbol}"
        ch_rec_opt = channels.get(entry_obj.channel)
        if ch_rec_opt is not None:
            if entry_obj.symbol in ch_rec_opt.reserved_symbols:
                findings.append(
                    CompatFinding(
                        code="E_MANIFEST_RESERVED_REUSE",
                        severity="ERROR",
                        location=loc,
                        message=(
                            f"entry {entry_obj.symbol!r} reuses channel reserved_symbols "
                            f"tombstone in {entry_obj.channel}"
                        ),
                    )
                )
            if (
                entry_obj.field_number is not None
                and entry_obj.field_number in ch_rec_opt.reserved_field_numbers
            ):
                findings.append(
                    CompatFinding(
                        code="E_MANIFEST_RESERVED_REUSE",
                        severity="ERROR",
                        location=loc,
                        message=(
                            f"entry {entry_obj.symbol!r} reuses reserved_field_numbers "
                            f"slot {entry_obj.field_number} in {entry_obj.channel}"
                        ),
                    )
                )
        if (entry_obj.channel, entry_obj.symbol) in sunset_symbols:
            findings.append(
                CompatFinding(
                    code="E_MANIFEST_RESERVED_REUSE",
                    severity="ERROR",
                    location=loc,
                    message=f"entry {entry_obj.symbol!r} collides with SUNSET symbol",
                )
            )
        if (
            entry_obj.field_number is not None
            and (entry_obj.channel, entry_obj.parent_type, entry_obj.field_number)
            in sunset_fields
        ):
            findings.append(
                CompatFinding(
                    code="E_MANIFEST_RESERVED_REUSE",
                    severity="ERROR",
                    location=loc,
                    message=(
                        f"entry {entry_obj.symbol!r} reuses SUNSET field_number "
                        f"{entry_obj.field_number}"
                    ),
                )
            )

    doc = DeprecationManifestDoc(
        manifest_version=manifest_version,
        current_epoch=current_epoch,
        min_compatibility_window_epochs=min_window,
        frozen_channels_total=frozen_total,
        channels=channels,
        entries=entries,
    )
    return doc, findings


def _apply_channel_kv(
    target: dict[str, Any],
    content: str,
    indent: int,
    line_no: int,
) -> tuple[str | None, int]:
    if ":" not in content:
        raise SchemaParseError(f"deprecation-manifest line {line_no}: missing ':'")
    key, val = content.split(":", 1)
    key = key.strip()
    val = val.strip()
    if key == "name":
        target["name"] = _parse_yaml_scalar_str(val, line_no)
    elif key in ("major", "minor"):
        target[key] = _parse_yaml_int(val, line_no, key)
    elif key == "frozen":
        if val not in ("true", "false"):
            raise SchemaParseError(
                f"deprecation-manifest line {line_no}: invalid bool {val!r}"
            )
        target["frozen"] = val == "true"
    elif key == "reserved_field_numbers":
        if val == "":
            return ("reserved_field_numbers", indent)
        target["reserved_field_numbers"] = _parse_yaml_inline_list(
            val, line_no, as_int=True
        )
    elif key == "reserved_symbols":
        if val == "":
            return ("reserved_symbols", indent)
        target["reserved_symbols"] = _parse_yaml_inline_list(
            val, line_no, as_int=False
        )
    else:
        raise SchemaParseError(
            f"deprecation-manifest line {line_no}: unknown channel key {key!r}"
        )
    return (None, 0)


def _apply_entry_kv(target: dict[str, Any], content: str, line_no: int) -> None:
    if ":" not in content:
        raise SchemaParseError(f"deprecation-manifest line {line_no}: missing ':'")
    key, val = content.split(":", 1)
    key = key.strip()
    val = val.strip()
    if key in ("channel", "symbol", "kind", "state"):
        target[key] = _parse_yaml_scalar_str(val, line_no)
    elif key in ("parent_type", "replacement", "migration_rule_id", "reason"):
        target[key] = _parse_yaml_optional_str(val, line_no)
    elif key == "introduced_in_epoch":
        target[key] = _parse_yaml_int(val, line_no, key)
    elif key in (
        "field_number",
        "deprecated_in_epoch",
        "earliest_sunset_epoch",
        "sunset_in_epoch",
    ):
        target[key] = _parse_yaml_optional_int(val, line_no, key)
    else:
        raise SchemaParseError(
            f"deprecation-manifest line {line_no}: unknown entry key {key!r}"
        )


def _validate_single_manifest_entry(
    entry: ManifestEntry,
    *,
    current_epoch: int,
    effective_window: int,
    location: str,
    findings: list[CompatFinding],
) -> None:
    if entry.introduced_in_epoch < 1 or entry.introduced_in_epoch > current_epoch:
        findings.append(
            CompatFinding(
                code="E_INVALID_EPOCH",
                severity="ERROR",
                location=location,
                message=(
                    f"introduced_in_epoch={entry.introduced_in_epoch} outside "
                    f"1..={current_epoch}"
                ),
            )
        )
    if entry.state == "ACTIVE":
        if (
            entry.deprecated_in_epoch is not None
            or entry.earliest_sunset_epoch is not None
            or entry.sunset_in_epoch is not None
        ):
            findings.append(
                CompatFinding(
                    code="E_INVALID_STATE_METADATA",
                    severity="ERROR",
                    location=location,
                    message="ACTIVE entry must not set deprecation or sunset epochs",
                )
            )
    elif entry.state in ("DEPRECATED", "SUNSET"):
        if entry.deprecated_in_epoch is None:
            findings.append(
                CompatFinding(
                    code="E_MISSING_DEPRECATION_METADATA",
                    severity="ERROR",
                    location=location,
                    message="missing deprecated_in_epoch",
                )
            )
        if entry.earliest_sunset_epoch is None:
            findings.append(
                CompatFinding(
                    code="E_MISSING_DEPRECATION_METADATA",
                    severity="ERROR",
                    location=location,
                    message="missing earliest_sunset_epoch",
                )
            )
        if not entry.replacement:
            findings.append(
                CompatFinding(
                    code="E_MISSING_DEPRECATION_METADATA",
                    severity="ERROR",
                    location=location,
                    message="missing non-empty replacement symbol",
                )
            )
        if not entry.reason:
            findings.append(
                CompatFinding(
                    code="E_MISSING_DEPRECATION_METADATA",
                    severity="ERROR",
                    location=location,
                    message="missing non-empty deprecation reason",
                )
            )
        if (
            entry.deprecated_in_epoch is not None
            and entry.earliest_sunset_epoch is not None
            and entry.earliest_sunset_epoch
            < entry.deprecated_in_epoch + effective_window
        ):
            findings.append(
                CompatFinding(
                    code="E_INSUFFICIENT_DEPRECATION_WINDOW",
                    severity="ERROR",
                    location=location,
                    message=(
                        f"earliest_sunset_epoch={entry.earliest_sunset_epoch} < "
                        f"deprecated_in_epoch={entry.deprecated_in_epoch} + {effective_window}"
                    ),
                )
            )
        if entry.state == "DEPRECATED" and entry.sunset_in_epoch is not None:
            findings.append(
                CompatFinding(
                    code="E_INVALID_STATE_METADATA",
                    severity="ERROR",
                    location=location,
                    message="DEPRECATED entry must not set sunset_in_epoch",
                )
            )
        if entry.state == "SUNSET":
            if entry.sunset_in_epoch is None:
                findings.append(
                    CompatFinding(
                        code="E_MISSING_DEPRECATION_METADATA",
                        severity="ERROR",
                        location=location,
                        message="SUNSET entry missing sunset_in_epoch",
                    )
                )
            elif (
                entry.earliest_sunset_epoch is not None
                and (
                    entry.sunset_in_epoch < entry.earliest_sunset_epoch
                    or current_epoch < entry.sunset_in_epoch
                )
            ):
                findings.append(
                    CompatFinding(
                        code="E_PREMATURE_SUNSET",
                        severity="ERROR",
                        location=location,
                        message=(
                            f"sunset_in_epoch={entry.sunset_in_epoch} violates "
                            f"earliest_sunset_epoch={entry.earliest_sunset_epoch} / "
                            f"current_epoch={current_epoch}"
                        ),
                    )
                )


def _cross_check_proto_and_manifest(
    corpus: ProtoCorpus,
    manifest: DeprecationManifestDoc,
    label: str,
) -> list[CompatFinding]:
    """Ensure active/deprecated/reserved manifest declarations align with `.proto`."""
    findings: list[CompatFinding] = []
    rpc_Lookup: set[str] = set()
    for svc in corpus.services.values():
        for rpc in svc.rpcs.values():
            rpc_Lookup.add(rpc.name)
            rpc_Lookup.add(f"{svc.full_name}.{rpc.name}")

    # Check channel reserved symbols and field numbers against live proto elements.
    channel_messages: dict[str, list[ProtoMessage]] = {}
    for msg in corpus.messages.values():
        ch_name = proto_stem_to_channel_name(msg.file_name)
        channel_messages.setdefault(ch_name, []).append(msg)

    for ch_name, ch_rec in manifest.channels.items():
        for msg in channel_messages.get(ch_name, []):
            if msg.full_name in ch_rec.reserved_symbols:
                findings.append(
                    CompatFinding(
                        code="E_RESERVED_SLOT_REUSE",
                        severity="ERROR",
                        location=f"{label}:{msg.file_name}:{msg.full_name}",
                        message=(
                            f"message {msg.full_name} is permanently reserved in {ch_name}"
                        ),
                    )
                )
            for fld in msg.fields_by_number.values():
                if (
                    fld.name in ch_rec.reserved_symbols
                    or f"{msg.full_name}.{fld.name}" in ch_rec.reserved_symbols
                ):
                    findings.append(
                        CompatFinding(
                            code="E_RESERVED_SLOT_REUSE",
                            severity="ERROR",
                            location=f"{label}:{msg.file_name}:{msg.full_name}.{fld.name}",
                            message=(
                                f"field {msg.full_name}.{fld.name} reuses channel reserved symbol"
                            ),
                        )
                    )
                if fld.number in ch_rec.reserved_field_numbers:
                    findings.append(
                        CompatFinding(
                            code="E_RESERVED_SLOT_REUSE",
                            severity="ERROR",
                            location=f"{label}:{msg.file_name}:{msg.full_name}.{fld.name}",
                            message=(
                                f"field {msg.full_name}.{fld.name}={fld.number} reuses channel "
                                f"reserved_field_numbers in {ch_name}"
                            ),
                        )
                    )

    for entry in manifest.entries.values():
        loc = f"{label}:{entry.channel}:{entry.symbol}"
        if entry.state in ("ACTIVE", "DEPRECATED"):
            if entry.kind == "MESSAGE" and entry.symbol not in corpus.messages:
                findings.append(
                    CompatFinding(
                        code="E_MANIFEST_PROTO_MISMATCH",
                        severity="ERROR",
                        location=loc,
                        message=(
                            f"{entry.state} MESSAGE {entry.symbol!r} not found in .proto corpus"
                        ),
                    )
                )
            elif entry.kind == "FIELD":
                parent = entry.parent_type or ""
                msg = corpus.messages.get(parent)
                field_name = entry.symbol.split(".")[-1]
                if msg is None or field_name not in msg.fields_by_name:
                    findings.append(
                        CompatFinding(
                            code="E_MANIFEST_PROTO_MISMATCH",
                            severity="ERROR",
                            location=loc,
                            message=(
                                f"{entry.state} FIELD {entry.symbol!r} (parent={parent!r}) "
                                "not found in .proto corpus"
                            ),
                        )
                    )
                elif (
                    entry.field_number is not None
                    and msg.fields_by_name[field_name].number != entry.field_number
                ):
                    findings.append(
                        CompatFinding(
                            code="E_MANIFEST_PROTO_MISMATCH",
                            severity="ERROR",
                            location=loc,
                            message=(
                                f"FIELD {entry.symbol!r} number mismatch between manifest "
                                f"({entry.field_number}) and .proto "
                                f"({msg.fields_by_name[field_name].number})"
                            ),
                        )
                    )
            elif entry.kind == "METHOD" and entry.symbol not in rpc_Lookup:
                findings.append(
                    CompatFinding(
                        code="E_MANIFEST_PROTO_MISMATCH",
                        severity="ERROR",
                        location=loc,
                        message=f"{entry.state} METHOD {entry.symbol!r} not found in .proto services",
                    )
                )
            elif entry.kind == "ENUM_VALUE":
                parent = entry.parent_type or ""
                en = corpus.enums.get(parent)
                val_name = entry.symbol.split(".")[-1]
                if en is None or val_name not in en.values_by_name:
                    findings.append(
                        CompatFinding(
                            code="E_MANIFEST_PROTO_MISMATCH",
                            severity="ERROR",
                            location=loc,
                            message=f"{entry.state} ENUM_VALUE {entry.symbol!r} not found in .proto",
                        )
                    )
        elif entry.state == "SUNSET" and entry.kind == "FIELD" and entry.parent_type:
            msg = corpus.messages.get(entry.parent_type)
            field_name = entry.symbol.split(".")[-1]
            if msg is not None and (
                (entry.field_number is not None and entry.field_number in msg.fields_by_number)
                or field_name in msg.fields_by_name
            ):
                findings.append(
                    CompatFinding(
                        code="E_SUNSET_FIELD_STILL_PRESENT",
                        severity="ERROR",
                        location=loc,
                        message=(
                            f"SUNSET field {entry.symbol!r} must be removed and marked reserved "
                            f"in message {entry.parent_type}"
                        ),
                    )
                )

    return findings


def _diff_proto_corpora(
    base: ProtoCorpus,
    cand: ProtoCorpus,
) -> list[CompatFinding]:
    findings: list[CompatFinding] = []

    for proto_file in sorted(base.files):
        if proto_file not in cand.files:
            findings.append(
                CompatFinding(
                    code="E_REMOVED_PROTO_FILE",
                    severity="ERROR",
                    location=proto_file,
                    message=f"existing .proto file {proto_file!r} was deleted",
                )
            )
        elif base.packages_by_file.get(proto_file) != cand.packages_by_file.get(proto_file):
            findings.append(
                CompatFinding(
                    code="E_CHANGED_PROTO_PACKAGE",
                    severity="ERROR",
                    location=proto_file,
                    message=(
                        f"package changed from {base.packages_by_file.get(proto_file)!r} "
                        f"to {cand.packages_by_file.get(proto_file)!r}"
                    ),
                )
            )

    for msg_name in sorted(base.messages):
        base_msg = base.messages[msg_name]
        cand_msg = cand.messages.get(msg_name)
        if cand_msg is None:
            findings.append(
                CompatFinding(
                    code="E_REMOVED_MESSAGE",
                    severity="ERROR",
                    location=f"{base_msg.file_name}:{msg_name}",
                    message=f"existing message {msg_name!r} was deleted",
                )
            )
            continue

        # Reserved tombstones from baseline cannot be dropped or reused in candidate.
        for base_num in sorted(base_msg.reserved.explicit_numbers_up_to()):
            if not cand_msg.reserved.has_number(base_num):
                findings.append(
                    CompatFinding(
                        code="E_REMOVED_RESERVED_TOMBSTONE",
                        severity="ERROR",
                        location=f"{cand_msg.file_name}:{msg_name}",
                        message=(
                            f"reserved field number {base_num} in {msg_name} was removed"
                        ),
                    )
                )
            if base_num in cand_msg.fields_by_number:
                findings.append(
                    CompatFinding(
                        code="E_RESERVED_SLOT_REUSE",
                        severity="ERROR",
                        location=f"{cand_msg.file_name}:{msg_name}",
                        message=(
                            f"field number {base_num} in {msg_name} was previously reserved"
                        ),
                    )
                )
        for base_rname in sorted(base_msg.reserved.names):
            if base_rname not in cand_msg.reserved.names:
                findings.append(
                    CompatFinding(
                        code="E_REMOVED_RESERVED_TOMBSTONE",
                        severity="ERROR",
                        location=f"{cand_msg.file_name}:{msg_name}",
                        message=f"reserved field name {base_rname!r} in {msg_name} was removed",
                    )
                )
            if base_rname in cand_msg.fields_by_name:
                findings.append(
                    CompatFinding(
                        code="E_RESERVED_SLOT_REUSE",
                        severity="ERROR",
                        location=f"{cand_msg.file_name}:{msg_name}.{base_rname}",
                        message=f"field name {base_rname!r} in {msg_name} was previously reserved",
                    )
                )

        for fld_num in sorted(base_msg.fields_by_number):
            base_fld = base_msg.fields_by_number[fld_num]
            cand_fld = cand_msg.fields_by_number.get(fld_num)
            loc = f"{cand_msg.file_name}:{msg_name}.{base_fld.name}"
            if cand_fld is None:
                has_num = cand_msg.reserved.has_number(fld_num)
                has_name = base_fld.name in cand_msg.reserved.names
                if not (has_num and has_name):
                    findings.append(
                        CompatFinding(
                            code="E_REMOVED_FIELD_WITHOUT_RESERVED",
                            severity="ERROR",
                            location=loc,
                            message=(
                                f"deleted field {msg_name}.{base_fld.name} = {fld_num} "
                                f"without recording both `reserved {fld_num};` and "
                                f"`reserved \"{base_fld.name}\";`"
                            ),
                        )
                    )
                else:
                    findings.append(
                        CompatFinding(
                            code="I_RETIRED_FIELD_RESERVED",
                            severity="INFO",
                            location=loc,
                            message=(
                                f"field {msg_name}.{base_fld.name} = {fld_num} retired with "
                                "numeric and name tombstones"
                            ),
                        )
                    )
                continue

            if cand_fld.name != base_fld.name:
                findings.append(
                    CompatFinding(
                        code="E_CHANGED_FIELD_NAME",
                        severity="ERROR",
                        location=loc,
                        message=(
                            f"field number {fld_num} in {msg_name} renamed from "
                            f"{base_fld.name!r} to {cand_fld.name!r}"
                        ),
                    )
                )
            if (
                cand_fld.type_name != base_fld.type_name
                or cand_fld.cardinality != base_fld.cardinality
                or cand_fld.oneof_group != base_fld.oneof_group
            ):
                findings.append(
                    CompatFinding(
                        code="E_CHANGED_FIELD_TYPE",
                        severity="ERROR",
                        location=loc,
                        message=(
                            f"field {msg_name}.{base_fld.name} = {fld_num} changed signature "
                            f"from ({base_fld.cardinality} {base_fld.type_name}, "
                            f"oneof={base_fld.oneof_group}) to "
                            f"({cand_fld.cardinality} {cand_fld.type_name}, "
                            f"oneof={cand_fld.oneof_group})"
                        ),
                    )
                )

        for fld_num in sorted(cand_msg.fields_by_number):
            if fld_num not in base_msg.fields_by_number:
                cand_fld = cand_msg.fields_by_number[fld_num]
                findings.append(
                    CompatFinding(
                        code="I_ADDITIVE_FIELD",
                        severity="INFO",
                        location=f"{cand_msg.file_name}:{msg_name}.{cand_fld.name}",
                        message=(
                            f"additive field {msg_name}.{cand_fld.name} = {fld_num} "
                            f"({cand_fld.cardinality} {cand_fld.type_name})"
                        ),
                    )
                )

    for enum_name in sorted(base.enums):
        base_en = base.enums[enum_name]
        cand_en = cand.enums.get(enum_name)
        if cand_en is None:
            findings.append(
                CompatFinding(
                    code="E_REMOVED_ENUM",
                    severity="ERROR",
                    location=f"{base_en.file_name}:{enum_name}",
                    message=f"existing enum {enum_name!r} was deleted",
                )
            )
            continue

        for val_num in sorted(base_en.values_by_number):
            base_val = base_en.values_by_number[val_num]
            cand_val = cand_en.values_by_number.get(val_num)
            loc = f"{cand_en.file_name}:{enum_name}.{base_val.name}"
            if cand_val is None:
                if not (
                    cand_en.reserved.has_number(val_num)
                    and base_val.name in cand_en.reserved.names
                ):
                    findings.append(
                        CompatFinding(
                            code="E_REMOVED_ENUM_VALUE_WITHOUT_RESERVED",
                            severity="ERROR",
                            location=loc,
                            message=(
                                f"deleted enum value {enum_name}.{base_val.name} = {val_num} "
                                "without both numeric and name `reserved` tombstones"
                            ),
                        )
                    )
            elif cand_val.name != base_val.name:
                findings.append(
                    CompatFinding(
                        code="E_CHANGED_ENUM_VALUE_NAME",
                        severity="ERROR",
                        location=loc,
                        message=(
                            f"enum value {val_num} in {enum_name} renamed from "
                            f"{base_val.name!r} to {cand_val.name!r}"
                        ),
                    )
                )

    for svc_name in sorted(base.services):
        base_svc = base.services[svc_name]
        cand_svc = cand.services.get(svc_name)
        if cand_svc is None:
            findings.append(
                CompatFinding(
                    code="E_REMOVED_SERVICE",
                    severity="ERROR",
                    location=f"{base_svc.file_name}:{svc_name}",
                    message=f"existing service {svc_name!r} was deleted",
                )
            )
            continue

        for rpc_name in sorted(base_svc.rpcs):
            base_rpc = base_svc.rpcs[rpc_name]
            cand_rpc = cand_svc.rpcs.get(rpc_name)
            loc = f"{cand_svc.file_name}:{svc_name}.{rpc_name}"
            if cand_rpc is None:
                findings.append(
                    CompatFinding(
                        code="E_REMOVED_RPC",
                        severity="ERROR",
                        location=loc,
                        message=f"existing rpc {svc_name}.{rpc_name} was deleted",
                    )
                )
            elif cand_rpc != base_rpc:
                findings.append(
                    CompatFinding(
                        code="E_CHANGED_RPC_SIGNATURE",
                        severity="ERROR",
                        location=loc,
                        message=(
                            f"rpc {svc_name}.{rpc_name} changed signature from "
                            f"{base_rpc} to {cand_rpc}"
                        ),
                    )
                )

        for rpc_name in sorted(cand_svc.rpcs):
            if rpc_name not in base_svc.rpcs:
                findings.append(
                    CompatFinding(
                        code="I_ADDITIVE_RPC",
                        severity="INFO",
                        location=f"{cand_svc.file_name}:{svc_name}.{rpc_name}",
                        message=f"additive rpc {svc_name}.{rpc_name}",
                    )
                )

    return findings


def _diff_deprecation_manifests(
    base: DeprecationManifestDoc,
    cand: DeprecationManifestDoc,
) -> list[CompatFinding]:
    findings: list[CompatFinding] = []
    min_window = max(
        MIN_COMPATIBILITY_WINDOW_EPOCHS,
        base.min_compatibility_window_epochs,
        cand.min_compatibility_window_epochs,
    )

    if cand.current_epoch < base.current_epoch:
        findings.append(
            CompatFinding(
                code="E_EPOCH_REGRESSION",
                severity="ERROR",
                location="deprecation-manifest.yaml:current_epoch",
                message=(
                    f"candidate current_epoch={cand.current_epoch} regressed below "
                    f"baseline current_epoch={base.current_epoch}"
                ),
            )
        )

    for ch_name, base_ch in sorted(base.channels.items()):
        cand_ch = cand.channels.get(ch_name)
        if cand_ch is None:
            findings.append(
                CompatFinding(
                    code="E_CHANNEL_REMOVED_OR_UNFROZEN",
                    severity="ERROR",
                    location=f"deprecation-manifest.yaml:{ch_name}",
                    message=f"frozen channel {ch_name!r} was removed from candidate manifest",
                )
            )
            continue
        if base_ch.frozen and not cand_ch.frozen:
            findings.append(
                CompatFinding(
                    code="E_CHANNEL_REMOVED_OR_UNFROZEN",
                    severity="ERROR",
                    location=f"deprecation-manifest.yaml:{ch_name}",
                    message=f"channel {ch_name!r} flipped frozen true -> false",
                )
            )
        if cand_ch.major == base_ch.major and cand_ch.minor < base_ch.minor:
            findings.append(
                CompatFinding(
                    code="E_CHANNEL_MINOR_REGRESSION",
                    severity="ERROR",
                    location=f"deprecation-manifest.yaml:{ch_name}",
                    message=(
                        f"channel {ch_name!r} minor regressed {base_ch.minor} -> {cand_ch.minor}"
                    ),
                )
            )
        for rnum in base_ch.reserved_field_numbers:
            if rnum not in cand_ch.reserved_field_numbers:
                findings.append(
                    CompatFinding(
                        code="E_REMOVED_RESERVED_TOMBSTONE",
                        severity="ERROR",
                        location=f"deprecation-manifest.yaml:{ch_name}",
                        message=(
                            f"reserved_field_numbers tombstone {rnum} dropped from {ch_name}"
                        ),
                    )
                )
        for rsym in base_ch.reserved_symbols:
            if rsym not in cand_ch.reserved_symbols:
                findings.append(
                    CompatFinding(
                        code="E_REMOVED_RESERVED_TOMBSTONE",
                        severity="ERROR",
                        location=f"deprecation-manifest.yaml:{ch_name}",
                        message=f"reserved_symbols tombstone {rsym!r} dropped from {ch_name}",
                    )
                )

    base_reserved_symbols: set[tuple[str, str]] = set()
    base_reserved_fields: set[tuple[str, str | None, int]] = set()
    for ch_name, base_ch in base.channels.items():
        for rsym in base_ch.reserved_symbols:
            base_reserved_symbols.add((ch_name, rsym))
        for rnum in base_ch.reserved_field_numbers:
            base_reserved_fields.add((ch_name, None, rnum))
    for base_en in base.entries.values():
        if base_en.state == "SUNSET":
            base_reserved_symbols.add((base_en.channel, base_en.symbol))
            if base_en.field_number is not None:
                base_reserved_fields.add(
                    (base_en.channel, base_en.parent_type, base_en.field_number)
                )

    for key, base_en in sorted(base.entries.items()):
        loc = f"deprecation-manifest.yaml:{base_en.channel}:{base_en.symbol}"
        cand_en = cand.entries.get(key)
        if cand_en is None:
            findings.append(
                CompatFinding(
                    code="E_ENTRY_REMOVED",
                    severity="ERROR",
                    location=loc,
                    message=(
                        f"managed entry ({base_en.channel}, {base_en.symbol}) was deleted "
                        "instead of transitioning through DEPRECATED -> SUNSET"
                    ),
                )
            )
            continue

        if (
            cand_en.kind != base_en.kind
            or cand_en.introduced_in_epoch != base_en.introduced_in_epoch
            or cand_en.parent_type != base_en.parent_type
            or cand_en.field_number != base_en.field_number
        ):
            findings.append(
                CompatFinding(
                    code="E_IMMUTABLE_ENTRY_METADATA_CHANGED",
                    severity="ERROR",
                    location=loc,
                    message=(
                        f"immutable identity metadata changed on ({base_en.channel}, "
                        f"{base_en.symbol})"
                    ),
                )
            )

        transition = (base_en.state, cand_en.state)
        if transition == ("ACTIVE", "SUNSET"):
            findings.append(
                CompatFinding(
                    code="E_ILLEGAL_STATE_SKIP_ACTIVE_TO_SUNSET",
                    severity="ERROR",
                    location=loc,
                    message=(
                        f"entry {base_en.symbol!r} jumped ACTIVE -> SUNSET without a "
                        f">={min_window}-epoch DEPRECATED grace window"
                    ),
                )
            )
        elif transition in (
            ("DEPRECATED", "ACTIVE"),
            ("SUNSET", "ACTIVE"),
            ("SUNSET", "DEPRECATED"),
        ):
            findings.append(
                CompatFinding(
                    code="E_ILLEGAL_STATE_REGRESSION",
                    severity="ERROR",
                    location=loc,
                    message=(
                        f"illegal reverse transition {base_en.state} -> {cand_en.state} "
                        f"on {base_en.symbol!r}"
                    ),
                )
            )
        elif transition == ("ACTIVE", "DEPRECATED"):
            findings.append(
                CompatFinding(
                    code="I_TRANSITION_ACTIVE_TO_DEPRECATED",
                    severity="INFO",
                    location=loc,
                    message=(
                        f"legal deprecation {base_en.symbol!r}: "
                        f"deprecated_in_epoch={cand_en.deprecated_in_epoch}, "
                        f"earliest_sunset_epoch={cand_en.earliest_sunset_epoch}"
                    ),
                )
            )
        elif transition == ("DEPRECATED", "SUNSET"):
            if (
                base_en.deprecated_in_epoch is not None
                and cand_en.deprecated_in_epoch != base_en.deprecated_in_epoch
            ):
                findings.append(
                    CompatFinding(
                        code="E_IMMUTABLE_ENTRY_METADATA_CHANGED",
                        severity="ERROR",
                        location=loc,
                        message=(
                            f"entry {base_en.symbol!r} altered deprecated_in_epoch from "
                            f"{base_en.deprecated_in_epoch} to {cand_en.deprecated_in_epoch}"
                        ),
                    )
                )
            req_sunset = max(
                base_en.earliest_sunset_epoch or 0,
                (base_en.deprecated_in_epoch or 0) + min_window,
            )
            if (
                cand.current_epoch < req_sunset
                or cand_en.sunset_in_epoch is None
                or cand_en.sunset_in_epoch < req_sunset
            ):
                findings.append(
                    CompatFinding(
                        code="E_PREMATURE_SUNSET",
                        severity="ERROR",
                        location=loc,
                        message=(
                            f"entry {base_en.symbol!r} cannot transition DEPRECATED -> SUNSET "
                            f"at epoch {cand.current_epoch} (required earliest_sunset_epoch "
                            f">= {req_sunset})"
                        ),
                    )
                )
            else:
                findings.append(
                    CompatFinding(
                        code="I_TRANSITION_DEPRECATED_TO_SUNSET",
                        severity="INFO",
                        location=loc,
                        message=(
                            f"legal full-window sunset {base_en.symbol!r} at epoch "
                            f"{cand_en.sunset_in_epoch}"
                        ),
                    )
                )

    for key, cand_en in sorted(cand.entries.items()):
        if key in base.entries:
            continue
        loc = f"deprecation-manifest.yaml:{cand_en.channel}:{cand_en.symbol}"
        if cand_en.state == "SUNSET":
            findings.append(
                CompatFinding(
                    code="E_ILLEGAL_DIRECT_SUNSET",
                    severity="ERROR",
                    location=loc,
                    message=(
                        f"newly added entry {cand_en.symbol!r} cannot enter manifest "
                        "directly in SUNSET state"
                    ),
                )
            )
        if key in base_reserved_symbols:
            findings.append(
                CompatFinding(
                    code="E_MANIFEST_RESERVED_REUSE",
                    severity="ERROR",
                    location=loc,
                    message=(
                        f"entry {cand_en.symbol!r} reuses baseline RESERVED/SUNSET symbol"
                    ),
                )
            )
        if cand_en.field_number is not None and (
            (cand_en.channel, cand_en.parent_type, cand_en.field_number)
            in base_reserved_fields
            or (cand_en.channel, None, cand_en.field_number) in base_reserved_fields
        ):
            findings.append(
                CompatFinding(
                    code="E_MANIFEST_RESERVED_REUSE",
                    severity="ERROR",
                    location=loc,
                    message=(
                        f"entry {cand_en.symbol!r} reuses baseline RESERVED/SUNSET "
                        f"field_number {cand_en.field_number}"
                    ),
                )
            )

    return findings


@dataclass
class CompatDiffReport:
    passed: bool
    findings: list[CompatFinding]
    summary: dict[str, Any]

    @property
    def errors(self) -> list[CompatFinding]:
        return [f for f in self.findings if f.severity == "ERROR"]

    @property
    def infos(self) -> list[CompatFinding]:
        return [f for f in self.findings if f.severity == "INFO"]

    def to_dict(self) -> dict[str, Any]:
        return {
            "error_count": len(self.errors),
            "findings": [f.to_dict() for f in self.findings],
            "info_count": len(self.infos),
            "passed": self.passed,
            "summary": self.summary,
        }


def compare_contract_surfaces(
    base_proto_dir: Path,
    base_manifest_path: Path,
    cand_proto_dir: Path,
    cand_manifest_path: Path,
) -> CompatDiffReport:
    """Compare baseline and candidate `.proto` + `deprecation-manifest.yaml` surfaces."""
    base_corpus, base_proto_findings = parse_proto_directory(base_proto_dir)
    cand_corpus, cand_proto_findings = parse_proto_directory(cand_proto_dir)
    base_manifest, base_man_findings = parse_deprecation_manifest_file(base_manifest_path)
    cand_manifest, cand_man_findings = parse_deprecation_manifest_file(cand_manifest_path)

    findings: list[CompatFinding] = []
    findings.extend(base_proto_findings)
    findings.extend(cand_proto_findings)
    findings.extend(base_man_findings)
    findings.extend(cand_man_findings)
    findings.extend(_cross_check_proto_and_manifest(base_corpus, base_manifest, "baseline"))
    findings.extend(_cross_check_proto_and_manifest(cand_corpus, cand_manifest, "candidate"))
    findings.extend(_diff_proto_corpora(base_corpus, cand_corpus))
    findings.extend(_diff_deprecation_manifests(base_manifest, cand_manifest))

    # Deduplicate and sort deterministically.
    unique_map: dict[tuple[str, str, str, str], CompatFinding] = {}
    for item in findings:
        unique_map[(item.severity, item.code, item.location, item.message)] = item
    ordered = sorted(
        unique_map.values(),
        key=lambda f: (0 if f.severity == "ERROR" else 1, f.code, f.location, f.message),
    )
    has_errors = any(f.severity == "ERROR" for f in ordered)
    summary = {
        "baseline_channels": len(base_manifest.channels),
        "baseline_epoch": base_manifest.current_epoch,
        "baseline_messages": len(base_corpus.messages),
        "baseline_proto_files": len(base_corpus.files),
        "candidate_channels": len(cand_manifest.channels),
        "candidate_epoch": cand_manifest.current_epoch,
        "candidate_messages": len(cand_corpus.messages),
        "candidate_proto_files": len(cand_corpus.files),
    }
    return CompatDiffReport(passed=not has_errors, findings=ordered, summary=summary)


def run_self_check(schema_dir: Path) -> CompatDiffReport:
    """Self-check canonical `schema/nlos/sabi/v1/` `.proto` corpus and deprecation manifest."""
    manifest_path = schema_dir / "deprecation-manifest.yaml"
    return compare_contract_surfaces(
        base_proto_dir=schema_dir,
        base_manifest_path=manifest_path,
        cand_proto_dir=schema_dir,
        cand_manifest_path=manifest_path,
    )


def _default_schema_dir() -> Path:
    repo_root = Path(__file__).resolve().parents[2]
    return repo_root / "schema" / "nlos" / "sabi" / "v1"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description=(
            "Static SABI .proto and deprecation-manifest.yaml compatibility checker "
            "(ADR-0014 / ADR-0027)."
        )
    )
    parser.add_argument(
        "--self-check",
        action="store_true",
        help="Validate canonical schema/nlos/sabi/v1/ consistency.",
    )
    parser.add_argument(
        "--schema-dir",
        type=Path,
        default=None,
        help="Override schema directory for --self-check (defaults to schema/nlos/sabi/v1).",
    )
    parser.add_argument(
        "--base-dir",
        type=Path,
        default=None,
        help="Baseline directory containing *.proto and deprecation-manifest.yaml.",
    )
    parser.add_argument(
        "--cand-dir",
        type=Path,
        default=None,
        help="Candidate directory containing *.proto and deprecation-manifest.yaml.",
    )
    parser.add_argument(
        "--base-proto-dir",
        type=Path,
        default=None,
        help="Baseline directory containing *.proto files.",
    )
    parser.add_argument(
        "--base-manifest",
        type=Path,
        default=None,
        help="Baseline deprecation-manifest.yaml path.",
    )
    parser.add_argument(
        "--cand-proto-dir",
        type=Path,
        default=None,
        help="Candidate directory containing *.proto files.",
    )
    parser.add_argument(
        "--cand-manifest",
        type=Path,
        default=None,
        help="Candidate deprecation-manifest.yaml path.",
    )
    parser.add_argument(
        "--json",
        action="store_true",
        help="Emit canonical sorted-key JSON output.",
    )
    args = parser.parse_args(argv)

    try:
        if args.self_check:
            target_dir = args.schema_dir or _default_schema_dir()
            report = run_self_check(target_dir)
        else:
            base_proto = args.base_proto_dir or args.base_dir
            base_man = args.base_manifest or (
                args.base_dir / "deprecation-manifest.yaml" if args.base_dir else None
            )
            cand_proto = args.cand_proto_dir or args.cand_dir
            cand_man = args.cand_manifest or (
                args.cand_dir / "deprecation-manifest.yaml" if args.cand_dir else None
            )
            if not (base_proto and base_man and cand_proto and cand_man):
                parser.error(
                    "provide --self-check OR (--base-dir and --cand-dir) OR "
                    "(--base-proto-dir, --base-manifest, --cand-proto-dir, --cand-manifest)"
                )
            report = compare_contract_surfaces(base_proto, base_man, cand_proto, cand_man)
    except SchemaParseError as exc:
        if args.json:
            print(
                json.dumps(
                    {
                        "error_count": 1,
                        "findings": [
                            {
                                "code": "E_SCHEMA_PARSE_ERROR",
                                "location": "<input>",
                                "message": str(exc),
                                "severity": "ERROR",
                            }
                        ],
                        "info_count": 0,
                        "passed": False,
                        "summary": {},
                    },
                    indent=2,
                    sort_keys=True,
                )
            )
        else:
            print(f"FAIL [E_SCHEMA_PARSE_ERROR] {exc}", file=sys.stderr)
        return 2

    if args.json:
        print(json.dumps(report.to_dict(), indent=2, sort_keys=True))
    else:
        for finding in report.findings:
            print(f"[{finding.severity}] {finding.code} ({finding.location}): {finding.message}")
        status = "OK" if report.passed else "FAIL"
        print(
            f"{status}: sabi-compat-diff checked "
            f"proto_files={report.summary['baseline_proto_files']}->{report.summary['candidate_proto_files']} "
            f"messages={report.summary['baseline_messages']}->{report.summary['candidate_messages']} "
            f"channels={report.summary['baseline_channels']}->{report.summary['candidate_channels']} "
            f"epoch={report.summary['baseline_epoch']}->{report.summary['candidate_epoch']} "
            f"errors={len(report.errors)} infos={len(report.infos)}"
        )

    return 0 if report.passed else 3


if __name__ == "__main__":
    sys.exit(main())
