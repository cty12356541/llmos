"""Thin SABI v1.0 client for ``nlos.sabi.BlobReference``."""

from __future__ import annotations

import hashlib
import hmac
import secrets
from dataclasses import dataclass, field
from typing import Any

from ._client_base import (
    DEFAULT_PROCESS_GENERATION,
    MAX_CONTROL_PAYLOAD_BYTES,
    SabiServiceClientBase,
    SabiServiceError,
    default_deadline_monotonic_ns,
    default_wall_ms,
    require_bounded_str,
    require_bytes_len,
    require_uint,
)
from .common import CommonSemanticsError, MethodSemantics, _ensure_gen_python_path
from .local_rpc import IpcError

_ensure_gen_python_path()

from nlos.sabi.v1 import blob_reference_pb2 as _blob_pb  # noqa: E402

BLOB_REFERENCE_SCHEMA = "nlos.sabi.BlobReference"
BLOB_REFERENCE_MAJOR = 1
BLOB_REFERENCE_MINOR = 0
BLOB_REFERENCE_SERVICE = "blob_reference"

MAX_BLOB_CHUNK_BYTES = 65_408
MAX_BLOB_FRAME_BYTES = MAX_CONTROL_PAYLOAD_BYTES
BLOB_URI_SCHEME = "nlos-blob://"

MINT_REFERENCE_METHOD = "mint_reference"
READ_REFERENCE_METHOD = "read_reference"
RELEASE_REFERENCE_METHOD = "release_reference"

_MUTATION_SEMANTICS = MethodSemantics(side_effecting=True, long_running=False)
_LONG_RUNNING_MUTATION_SEMANTICS = MethodSemantics(
    side_effecting=True, long_running=True
)

_ID_BYTES = 16
_SHA256_BYTES = 32
_U32_MAX = (1 << 32) - 1


def _require_non_zero_id(field_name: str, value: Any) -> bytes:
    raw = require_bytes_len(field_name, value, _ID_BYTES)
    if not any(raw):
        raise CommonSemanticsError(
            "INVALID_ID",
            f"{field_name} must not be all zeros",
        )
    return raw


def format_blob_uri(reference_id: bytes) -> str:
    """Format a 16-byte ``BlobReference`` ID as an ``nlos-blob://`` URI."""

    raw = _require_non_zero_id("reference_id", reference_id)
    return f"{BLOB_URI_SCHEME}{raw.hex()}"


def parse_blob_uri(uri: str) -> bytes:
    """Parse an ``nlos-blob://<32-hex>`` or ``nlosref://<32-hex>`` URI strictly."""

    if not isinstance(uri, str) or "\0" in uri:
        raise IpcError("INVALID_ARGUMENT", "blob URI must be a valid string")
    if uri.startswith(BLOB_URI_SCHEME):
        hex_part = uri[len(BLOB_URI_SCHEME) :]
    elif uri.startswith("nlosref://"):
        hex_part = uri[len("nlosref://") :]
    else:
        raise IpcError(
            "INVALID_ARGUMENT",
            "blob URI scheme must be nlos-blob:// or nlosref://",
        )
    if (
        len(hex_part) != _ID_BYTES * 2
        or any(ch not in "0123456789abcdefABCDEF" for ch in hex_part)
    ):
        raise IpcError("INVALID_ARGUMENT", "blob URI must contain 32 hex characters")
    raw = bytes.fromhex(hex_part)
    if not any(raw):
        raise IpcError("INVALID_ID", "blob URI reference_id must not be all zeros")
    return raw


@dataclass(frozen=True, slots=True)
class _TrackedToken:
    block_bytes: int
    recipient_principal_id: bytes


@dataclass(frozen=True, slots=True)
class MintReferenceRequest:
    """Typed request parameters for ``BlobReference.mint_reference``."""

    path_handle: str
    file_digest_sha256: bytes
    size_bytes: int
    max_reads: int
    max_chunk_bytes: int
    valid_until_ms: int
    recipient_principal_id: bytes


@dataclass(frozen=True, slots=True)
class MintReferenceResult:
    """Outcome of ``BlobReference.mint_reference``."""

    reference_id: bytes
    minted_at_ms: int
    recipient_principal_id: bytes = b""
    max_chunk_bytes: int = 0

    def to_uri(self) -> str:
        """Render ``reference_id`` as an ``nlos-blob://`` URI."""

        return format_blob_uri(self.reference_id)

    def __repr__(self) -> str:
        return (
            f"MintReferenceResult(reference_id=<{len(self.reference_id)} bytes>, "
            f"minted_at_ms={self.minted_at_ms}, "
            f"max_chunk_bytes={self.max_chunk_bytes})"
        )


@dataclass(frozen=True, slots=True)
class ReadReferenceRequest:
    """Typed request parameters for ``BlobReference.read_reference``."""

    reference_id: bytes
    offset: int
    length_bytes: int


@dataclass(frozen=True, slots=True)
class ReadReferenceResult:
    """Verified chunk returned by ``BlobReference.read_reference``."""

    chunk: bytes = field(repr=False)
    offset: int
    chunk_digest_sha256: bytes = field(repr=False)

    def __repr__(self) -> str:
        # Never expose raw blob bytes or chunk bytes in repr/log formatting.
        return (
            f"ReadReferenceResult(offset={self.offset}, "
            f"chunk_bytes={len(self.chunk)})"
        )

    def __str__(self) -> str:
        return self.__repr__()


@dataclass(frozen=True, slots=True)
class ReleaseReferenceRequest:
    """Typed request parameters for ``BlobReference.release_reference``."""

    reference_id: bytes


@dataclass(frozen=True, slots=True)
class ReleaseReferenceResult:
    """Outcome of ``BlobReference.release_reference``."""

    released: bool


class BlobReferenceError(SabiServiceError):
    """Typed SABI failure returned by ``BlobReference``."""


class BlobReferenceClient(SabiServiceClientBase):
    """High-level SABI client for ``nlos.sabi.BlobReference`` (``v1.0``)."""

    _service_name = BLOB_REFERENCE_SERVICE
    _schema_name = BLOB_REFERENCE_SCHEMA
    _schema_major = BLOB_REFERENCE_MAJOR
    _schema_minor = BLOB_REFERENCE_MINOR
    _max_payload_bytes = MAX_BLOB_FRAME_BYTES
    _error_type = BlobReferenceError

    def __init__(self, *args: Any, **kwargs: Any) -> None:
        super().__init__(*args, **kwargs)
        self._known_tokens: dict[bytes, _TrackedToken] = {}

    def register_token_metadata(
        self,
        reference_id: bytes,
        *,
        block_bytes: int,
        recipient_principal_id: bytes,
    ) -> None:
        """Record local token metadata so reads verify holder and block alignment."""

        ref = _require_non_zero_id("reference_id", reference_id)
        block = require_uint(
            "block_bytes",
            block_bytes,
            minimum=1,
            maximum=MAX_BLOB_CHUNK_BYTES,
            code="INVALID_CHUNK_SIZE",
        )
        recipient = _require_non_zero_id(
            "recipient_principal_id", recipient_principal_id
        )
        self._known_tokens[ref] = _TrackedToken(
            block_bytes=block,
            recipient_principal_id=recipient,
        )

    async def mint_reference(
        self,
        path_handle: str,
        file_digest_sha256: bytes,
        size_bytes: int,
        *,
        max_reads: int = 1,
        max_chunk_bytes: int = MAX_BLOB_CHUNK_BYTES,
        valid_until_ms: int | None = None,
        recipient_principal_id: bytes | None = None,
        idempotency_key: bytes | None = None,
        deadline_monotonic_ns: int | None = None,
        correlation_id: bytes | None = None,
        retry_of: Exception | None = None,
    ) -> MintReferenceResult:
        """Mint a holder-bound read-only ``BlobReference`` token."""

        if recipient_principal_id is None:
            raise CommonSemanticsError(
                "MISSING_RECIPIENT_PRINCIPAL",
                "BlobReference.mint_reference requires a 16-byte recipient_principal_id",
            )
        resolved_recipient = _require_non_zero_id(
            "recipient_principal_id", recipient_principal_id
        )
        resolved_path = require_bounded_str(
            "path_handle", path_handle, max_bytes=4096
        )
        resolved_digest = require_bytes_len(
            "file_digest_sha256",
            file_digest_sha256,
            _SHA256_BYTES,
            code="INVALID_DIGEST",
        )
        resolved_size = require_uint("size_bytes", size_bytes, minimum=0)
        resolved_max_reads = require_uint(
            "max_reads", max_reads, minimum=1, maximum=_U32_MAX
        )
        if isinstance(max_chunk_bytes, bool) or not isinstance(max_chunk_bytes, int):
            raise IpcError("INVALID_CHUNK_SIZE", "max_chunk_bytes must be an integer")
        if max_chunk_bytes <= 0:
            raise IpcError(
                "INVALID_CHUNK_SIZE",
                "max_chunk_bytes must be at least 1",
            )
        if max_chunk_bytes > MAX_BLOB_CHUNK_BYTES:
            raise IpcError(
                "FRAME_TOO_LARGE",
                f"max_chunk_bytes={max_chunk_bytes} exceeds MAX_BLOB_CHUNK_BYTES "
                f"({MAX_BLOB_CHUNK_BYTES})",
            )

        resolved_valid_until = (
            default_wall_ms() + 60_000
            if valid_until_ms is None
            else require_uint("valid_until_ms", valid_until_ms, minimum=1)
        )
        resolved_key = (
            secrets.token_bytes(_ID_BYTES)
            if idempotency_key is None
            else _require_non_zero_id("idempotency_key", idempotency_key)
        )
        resolved_deadline = (
            default_deadline_monotonic_ns()
            if deadline_monotonic_ns is None
            else deadline_monotonic_ns
        )

        msg = _blob_pb.MintReferenceRequest(
            schema=self._new_schema_identity(),
            path_handle=resolved_path,
            file_digest_sha256=resolved_digest,
            size_bytes=resolved_size,
            max_reads=resolved_max_reads,
            max_chunk_bytes=max_chunk_bytes,
            valid_until_ms=resolved_valid_until,
            recipient_principal_id=resolved_recipient,
        )
        payload, _ = await self._call(
            MINT_REFERENCE_METHOD,
            msg.SerializeToString(),
            _LONG_RUNNING_MUTATION_SEMANTICS,
            idempotency_key=resolved_key,
            deadline_monotonic_ns=resolved_deadline,
            require_deadline=True,
            correlation_id=correlation_id,
            retry_of=retry_of,
        )
        res = self._parse_result(
            _blob_pb.MintReferenceResult, payload, MINT_REFERENCE_METHOD
        )
        ref_id = bytes(res.reference_id)
        if len(ref_id) != _ID_BYTES or not any(ref_id):
            raise IpcError(
                "COMPATIBILITY",
                "BlobReference.mint_reference returned malformed reference_id",
            )
        self._known_tokens[ref_id] = _TrackedToken(
            block_bytes=max_chunk_bytes,
            recipient_principal_id=resolved_recipient,
        )
        return MintReferenceResult(
            reference_id=ref_id,
            minted_at_ms=int(res.minted_at_ms),
            recipient_principal_id=resolved_recipient,
            max_chunk_bytes=max_chunk_bytes,
        )

    async def read_reference(
        self,
        reference_id: bytes,
        offset: int = 0,
        length_bytes: int = MAX_BLOB_CHUNK_BYTES,
        *,
        block_bytes: int | None = None,
        expected_holder_principal_id: bytes | None = None,
        idempotency_key: bytes | None = None,
        deadline_monotonic_ns: int = 0,
        correlation_id: bytes | None = None,
        retry_of: Exception | None = None,
    ) -> ReadReferenceResult:
        """Read and SHA-256 verify one aligned blob chunk within the 64 KiB frame cap."""

        resolved_ref = _require_non_zero_id("reference_id", reference_id)
        resolved_offset = require_uint("offset", offset, minimum=0)
        resolved_length = require_uint(
            "length_bytes",
            length_bytes,
            minimum=1,
            maximum=MAX_BLOB_CHUNK_BYTES,
            code="INVALID_CHUNK_SIZE",
        )

        tracked = self._known_tokens.get(resolved_ref)
        expected_holder = (
            _require_non_zero_id(
                "expected_holder_principal_id", expected_holder_principal_id
            )
            if expected_holder_principal_id is not None
            else (tracked.recipient_principal_id if tracked is not None else None)
        )
        if expected_holder is not None and not hmac.compare_digest(
            self._principal_id, expected_holder
        ):
            raise CommonSemanticsError(
                "HOLDER_MISMATCH",
                "caller principal does not match blob reference recipient_principal_id",
            )

        resolved_block = (
            require_uint(
                "block_bytes",
                block_bytes,
                minimum=1,
                maximum=MAX_BLOB_CHUNK_BYTES,
                code="INVALID_CHUNK_SIZE",
            )
            if block_bytes is not None
            else (tracked.block_bytes if tracked is not None else None)
        )
        if resolved_block is not None and resolved_offset % resolved_block != 0:
            raise IpcError(
                "UNALIGNED_OFFSET",
                f"offset={resolved_offset} must align to block_bytes={resolved_block}",
            )

        resolved_key = (
            secrets.token_bytes(_ID_BYTES)
            if idempotency_key is None
            else _require_non_zero_id("idempotency_key", idempotency_key)
        )
        msg = _blob_pb.ReadReferenceRequest(
            schema=self._new_schema_identity(),
            reference_id=resolved_ref,
            offset=resolved_offset,
            length_bytes=resolved_length,
        )
        payload, _ = await self._call(
            READ_REFERENCE_METHOD,
            msg.SerializeToString(),
            _MUTATION_SEMANTICS,
            idempotency_key=resolved_key,
            deadline_monotonic_ns=deadline_monotonic_ns,
            correlation_id=correlation_id,
            retry_of=retry_of,
        )
        res = self._parse_result(
            _blob_pb.ReadReferenceResult, payload, READ_REFERENCE_METHOD
        )
        chunk = bytes(res.chunk)
        if int(res.offset) != resolved_offset:
            raise IpcError(
                "COMPATIBILITY",
                f"BlobReference.read_reference returned offset={res.offset}, "
                f"expected {resolved_offset}",
            )
        if (
            len(chunk) > MAX_BLOB_CHUNK_BYTES
            or len(chunk) > resolved_length
            or (resolved_block is not None and len(chunk) > resolved_block)
        ):
            raise IpcError(
                "FRAME_TOO_LARGE",
                f"BlobReference.read_reference chunk has {len(chunk)} bytes, "
                f"exceeding permitted bound",
            )
        digest = bytes(res.chunk_digest_sha256)
        if len(digest) != _SHA256_BYTES:
            raise IpcError(
                "COMPATIBILITY",
                "BlobReference.read_reference returned malformed chunk_digest_sha256",
            )
        computed = hashlib.sha256(chunk).digest()
        if not hmac.compare_digest(computed, digest):
            # Never format raw chunk or digest bytes into exception messages.
            raise IpcError(
                "INTEGRITY_ERROR",
                f"blob chunk SHA-256 verification failed at offset={resolved_offset} "
                f"(chunk_bytes={len(chunk)})",
            )
        return ReadReferenceResult(
            chunk=chunk,
            offset=int(res.offset),
            chunk_digest_sha256=digest,
        )

    async def release_reference(
        self,
        reference_id: bytes,
        *,
        idempotency_key: bytes | None = None,
        deadline_monotonic_ns: int = 0,
        correlation_id: bytes | None = None,
        retry_of: Exception | None = None,
    ) -> ReleaseReferenceResult:
        """Explicitly release a previously minted ``BlobReference`` token."""

        resolved_ref = _require_non_zero_id("reference_id", reference_id)
        resolved_key = (
            secrets.token_bytes(_ID_BYTES)
            if idempotency_key is None
            else _require_non_zero_id("idempotency_key", idempotency_key)
        )
        msg = _blob_pb.ReleaseReferenceRequest(
            schema=self._new_schema_identity(),
            reference_id=resolved_ref,
        )
        payload, _ = await self._call(
            RELEASE_REFERENCE_METHOD,
            msg.SerializeToString(),
            _MUTATION_SEMANTICS,
            idempotency_key=resolved_key,
            deadline_monotonic_ns=deadline_monotonic_ns,
            correlation_id=correlation_id,
            retry_of=retry_of,
        )
        res = self._parse_result(
            _blob_pb.ReleaseReferenceResult, payload, RELEASE_REFERENCE_METHOD
        )
        self._known_tokens.pop(resolved_ref, None)
        return ReleaseReferenceResult(released=bool(res.released))


__all__ = [
    "BLOB_REFERENCE_MAJOR",
    "BLOB_REFERENCE_MINOR",
    "BLOB_REFERENCE_SCHEMA",
    "BLOB_REFERENCE_SERVICE",
    "BLOB_URI_SCHEME",
    "BlobReferenceClient",
    "BlobReferenceError",
    "DEFAULT_PROCESS_GENERATION",
    "MAX_BLOB_CHUNK_BYTES",
    "MAX_BLOB_FRAME_BYTES",
    "MINT_REFERENCE_METHOD",
    "MintReferenceRequest",
    "MintReferenceResult",
    "READ_REFERENCE_METHOD",
    "RELEASE_REFERENCE_METHOD",
    "ReadReferenceRequest",
    "ReadReferenceResult",
    "ReleaseReferenceRequest",
    "ReleaseReferenceResult",
    "format_blob_uri",
    "parse_blob_uri",
]
