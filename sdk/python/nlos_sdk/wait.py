"""Thin SABI v1.0 client for ``nlos.sabi.WaitControl``."""

from __future__ import annotations

import secrets
from dataclasses import dataclass
from typing import Any

from ._client_base import (
    DEFAULT_PROCESS_GENERATION,
    SabiServiceClientBase,
    SabiServiceError,
    default_wall_ms,
    require_bytes_len,
    require_uint,
)
from .common import MethodSemantics, _ensure_gen_python_path
from .local_rpc import IpcError

_ensure_gen_python_path()

from nlos.sabi.v1 import wait_control_pb2 as _wait_pb  # noqa: E402

WAIT_CONTROL_SCHEMA = "nlos.sabi.WaitControl"
WAIT_CONTROL_MAJOR = 1
WAIT_CONTROL_MINOR = 0
WAIT_CONTROL_SERVICE = "wait_control"

REGISTER_WAIT_METHOD = "register_wait"
NOTIFY_COMMITS_METHOD = "notify_commits"
CANCEL_WAIT_METHOD = "cancel_wait"
LIST_WAITS_METHOD = "list_waits"
INSPECT_WAIT_METHOD = "inspect_wait"

WAIT_STATE_UNSPECIFIED = _wait_pb.WAIT_STATE_CODE_UNSPECIFIED
WAIT_STATE_PENDING = _wait_pb.WAIT_STATE_CODE_PENDING
WAIT_STATE_WOKEN = _wait_pb.WAIT_STATE_CODE_WOKEN
WAIT_STATE_CANCELLED = _wait_pb.WAIT_STATE_CODE_CANCELLED

_QUERY_SEMANTICS = MethodSemantics(side_effecting=False, long_running=False)
_MUTATION_SEMANTICS = MethodSemantics(side_effecting=True, long_running=False)

_ID_BYTES = 16
_VALID_WAIT_STATES = frozenset(
    {
        WAIT_STATE_PENDING,
        WAIT_STATE_WOKEN,
        WAIT_STATE_CANCELLED,
    }
)


def _require_non_zero_id(field: str, value: Any) -> bytes:
    raw = require_bytes_len(field, value, _ID_BYTES)
    if not any(raw):
        raise IpcError("INVALID_ID", f"{field} must not be all zeros")
    return raw


@dataclass(frozen=True, slots=True)
class WaitRecord:
    """Durable wait registration record returned by ``WaitControl``."""

    wait_id: bytes
    binding: bytes
    channel_id: bytes
    channel_generation: int
    channel_fencing_token: bytes
    target_sequence: int
    state: int
    idempotency_key: bytes
    registered_at_ms: int
    woken_at_ms: int = 0
    woken_up_to_sequence: int = 0
    cancelled_at_ms: int = 0

    @property
    def state_name(self) -> str:
        try:
            return _wait_pb.WaitStateCode.Name(self.state)
        except ValueError:
            return str(self.state)


@dataclass(frozen=True, slots=True)
class RegisterWaitRequest:
    """Typed request parameters for ``WaitControl.register_wait``."""

    binding: bytes
    channel_id: bytes
    target_sequence: int
    idempotency_key: bytes | None = None
    registered_at_ms: int | None = None


@dataclass(frozen=True, slots=True)
class RegisterWaitResult:
    """Outcome of a ``WaitControl.register_wait`` call."""

    record: WaitRecord
    replayed: bool = False


@dataclass(frozen=True, slots=True)
class NotifyCommitsRequest:
    """Typed request parameters for ``WaitControl.notify_commits``."""

    channel_id: bytes
    up_to_sequence: int
    notified_at_ms: int | None = None
    idempotency_key: bytes | None = None


@dataclass(frozen=True, slots=True)
class WakeReport:
    """Outcome of a ``WaitControl.notify_commits`` wake notification."""

    woken: tuple[WaitRecord, ...]


@dataclass(frozen=True, slots=True)
class CancelWaitRequest:
    """Typed request parameters for ``WaitControl.cancel_wait``."""

    wait_id: bytes
    cancelled_at_ms: int | None = None
    idempotency_key: bytes | None = None


@dataclass(frozen=True, slots=True)
class CancelWaitResult:
    """Outcome of a ``WaitControl.cancel_wait`` mutation."""

    record: WaitRecord
    replayed: bool = False


@dataclass(frozen=True, slots=True)
class ListWaitsRequest:
    """Typed request parameters for ``WaitControl.list_waits``."""

    filter_channel_id: bytes = b""


@dataclass(frozen=True, slots=True)
class ListWaitsResult:
    """Outcome of a ``WaitControl.list_waits`` query."""

    waits: tuple[WaitRecord, ...]


@dataclass(frozen=True, slots=True)
class InspectWaitRequest:
    """Typed request parameters for ``WaitControl.inspect_wait``."""

    wait_id: bytes


@dataclass(frozen=True, slots=True)
class InspectWaitResult:
    """Outcome of a ``WaitControl.inspect_wait`` query."""

    record: WaitRecord


class WaitControlError(SabiServiceError):
    """Typed SABI failure returned by ``WaitControl``."""


def _decode_wait_record(raw: _wait_pb.WaitRecord, method: str) -> WaitRecord:
    if raw.state not in _VALID_WAIT_STATES:
        raise IpcError(
            "COMPATIBILITY",
            f"WaitControl.{method} returned invalid WaitStateCode {raw.state}",
        )
    if len(raw.wait_id) != _ID_BYTES or len(raw.channel_id) != _ID_BYTES:
        raise IpcError(
            "COMPATIBILITY",
            f"WaitControl.{method} returned malformed 16-byte wait/channel id",
        )
    if raw.binding and len(raw.binding) != _ID_BYTES:
        raise IpcError(
            "COMPATIBILITY",
            f"WaitControl.{method} returned malformed 16-byte binding id",
        )
    if raw.idempotency_key and len(raw.idempotency_key) != _ID_BYTES:
        raise IpcError(
            "COMPATIBILITY",
            f"WaitControl.{method} returned malformed 16-byte idempotency_key",
        )
    return WaitRecord(
        wait_id=bytes(raw.wait_id),
        binding=bytes(raw.binding),
        channel_id=bytes(raw.channel_id),
        channel_generation=int(raw.channel_generation),
        channel_fencing_token=bytes(raw.channel_fencing_token),
        target_sequence=int(raw.target_sequence),
        state=int(raw.state),
        idempotency_key=bytes(raw.idempotency_key),
        registered_at_ms=int(raw.registered_at_ms),
        woken_at_ms=int(raw.woken_at_ms),
        woken_up_to_sequence=int(raw.woken_up_to_sequence),
        cancelled_at_ms=int(raw.cancelled_at_ms),
    )


class WaitClient(SabiServiceClientBase):
    """High-level SABI client for ``nlos.sabi.WaitControl`` (``v1.0``)."""

    _service_name = WAIT_CONTROL_SERVICE
    _schema_name = WAIT_CONTROL_SCHEMA
    _schema_major = WAIT_CONTROL_MAJOR
    _schema_minor = WAIT_CONTROL_MINOR
    _error_type = WaitControlError

    async def register_wait(
        self,
        binding: bytes,
        channel_id: bytes,
        target_sequence: int,
        *,
        idempotency_key: bytes | None = None,
        registered_at_ms: int | None = None,
        deadline_monotonic_ns: int = 0,
        correlation_id: bytes | None = None,
        retry_of: Exception | None = None,
    ) -> RegisterWaitResult:
        """Register a durable channel-sequence wait."""

        resolved_binding = _require_non_zero_id("binding", binding)
        resolved_channel = _require_non_zero_id("channel_id", channel_id)
        resolved_target = require_uint("target_sequence", target_sequence, minimum=1)
        resolved_ts = (
            default_wall_ms()
            if registered_at_ms is None
            else require_uint("registered_at_ms", registered_at_ms, minimum=1)
        )
        resolved_key = (
            secrets.token_bytes(_ID_BYTES)
            if idempotency_key is None
            else require_bytes_len("idempotency_key", idempotency_key, _ID_BYTES)
        )

        msg = _wait_pb.RegisterWaitRequest(
            schema=self._new_schema_identity(),
            binding=resolved_binding,
            channel_id=resolved_channel,
            target_sequence=resolved_target,
            idempotency_key=resolved_key,
            registered_at_ms=resolved_ts,
        )
        payload, _ = await self._call(
            REGISTER_WAIT_METHOD,
            msg.SerializeToString(),
            _MUTATION_SEMANTICS,
            idempotency_key=resolved_key,
            deadline_monotonic_ns=deadline_monotonic_ns,
            correlation_id=correlation_id,
            retry_of=retry_of,
        )
        res = self._parse_result(
            _wait_pb.RegisterWaitResult, payload, REGISTER_WAIT_METHOD
        )
        if not res.HasField("record"):
            raise IpcError(
                "COMPATIBILITY",
                "WaitControl.register_wait result is missing record",
            )
        return RegisterWaitResult(
            record=_decode_wait_record(res.record, REGISTER_WAIT_METHOD),
            replayed=bool(res.replayed),
        )

    async def notify_commits(
        self,
        channel_id: bytes,
        up_to_sequence: int,
        *,
        notified_at_ms: int | None = None,
        idempotency_key: bytes | None = None,
        deadline_monotonic_ns: int = 0,
        correlation_id: bytes | None = None,
        retry_of: Exception | None = None,
    ) -> WakeReport:
        """Advance durable commit sequence on a channel and collect woken waits."""

        resolved_channel = _require_non_zero_id("channel_id", channel_id)
        resolved_seq = require_uint("up_to_sequence", up_to_sequence, minimum=1)
        resolved_ts = (
            default_wall_ms()
            if notified_at_ms is None
            else require_uint("notified_at_ms", notified_at_ms, minimum=1)
        )
        resolved_key = (
            secrets.token_bytes(_ID_BYTES)
            if idempotency_key is None
            else require_bytes_len("idempotency_key", idempotency_key, _ID_BYTES)
        )

        msg = _wait_pb.NotifyCommitsRequest(
            schema=self._new_schema_identity(),
            channel_id=resolved_channel,
            up_to_sequence=resolved_seq,
            notified_at_ms=resolved_ts,
            idempotency_key=resolved_key,
        )
        payload, _ = await self._call(
            NOTIFY_COMMITS_METHOD,
            msg.SerializeToString(),
            _MUTATION_SEMANTICS,
            idempotency_key=resolved_key,
            deadline_monotonic_ns=deadline_monotonic_ns,
            correlation_id=correlation_id,
            retry_of=retry_of,
        )
        res = self._parse_result(
            _wait_pb.WakeReport, payload, NOTIFY_COMMITS_METHOD
        )
        return WakeReport(
            woken=tuple(
                _decode_wait_record(item, NOTIFY_COMMITS_METHOD) for item in res.woken
            )
        )

    async def cancel_wait(
        self,
        wait_id: bytes,
        *,
        cancelled_at_ms: int | None = None,
        idempotency_key: bytes | None = None,
        deadline_monotonic_ns: int = 0,
        correlation_id: bytes | None = None,
        retry_of: Exception | None = None,
    ) -> CancelWaitResult:
        """Cancel a registered wait idempotently."""

        resolved_wait_id = _require_non_zero_id("wait_id", wait_id)
        resolved_ts = (
            default_wall_ms()
            if cancelled_at_ms is None
            else require_uint("cancelled_at_ms", cancelled_at_ms, minimum=1)
        )
        resolved_key = (
            secrets.token_bytes(_ID_BYTES)
            if idempotency_key is None
            else require_bytes_len("idempotency_key", idempotency_key, _ID_BYTES)
        )

        msg = _wait_pb.CancelWaitRequest(
            schema=self._new_schema_identity(),
            wait_id=resolved_wait_id,
            cancelled_at_ms=resolved_ts,
            idempotency_key=resolved_key,
        )
        payload, _ = await self._call(
            CANCEL_WAIT_METHOD,
            msg.SerializeToString(),
            _MUTATION_SEMANTICS,
            idempotency_key=resolved_key,
            deadline_monotonic_ns=deadline_monotonic_ns,
            correlation_id=correlation_id,
            retry_of=retry_of,
        )
        res = self._parse_result(
            _wait_pb.CancelWaitResult, payload, CANCEL_WAIT_METHOD
        )
        if not res.HasField("record"):
            raise IpcError(
                "COMPATIBILITY",
                "WaitControl.cancel_wait result is missing record",
            )
        return CancelWaitResult(
            record=_decode_wait_record(res.record, CANCEL_WAIT_METHOD),
            replayed=bool(res.replayed),
        )

    async def list_waits(
        self,
        filter_channel_id: bytes | None = None,
        *,
        deadline_monotonic_ns: int = 0,
        correlation_id: bytes | None = None,
        retry_of: Exception | None = None,
    ) -> ListWaitsResult:
        """List registered waits, optionally filtered to one 16-byte channel_id."""

        if filter_channel_id is None or filter_channel_id == b"":
            resolved_filter = b""
        else:
            resolved_filter = require_bytes_len(
                "filter_channel_id", filter_channel_id, _ID_BYTES
            )

        msg = _wait_pb.ListWaitsRequest(
            schema=self._new_schema_identity(),
            filter_channel_id=resolved_filter,
        )
        payload, _ = await self._call(
            LIST_WAITS_METHOD,
            msg.SerializeToString(),
            _QUERY_SEMANTICS,
            deadline_monotonic_ns=deadline_monotonic_ns,
            correlation_id=correlation_id,
            retry_of=retry_of,
        )
        res = self._parse_result(
            _wait_pb.ListWaitsResult, payload, LIST_WAITS_METHOD
        )
        return ListWaitsResult(
            waits=tuple(
                _decode_wait_record(item, LIST_WAITS_METHOD) for item in res.waits
            )
        )

    async def inspect_wait(
        self,
        wait_id: bytes,
        *,
        deadline_monotonic_ns: int = 0,
        correlation_id: bytes | None = None,
        retry_of: Exception | None = None,
    ) -> InspectWaitResult:
        """Inspect a single wait registration by 16-byte ``wait_id``."""

        resolved_wait_id = _require_non_zero_id("wait_id", wait_id)
        msg = _wait_pb.InspectWaitRequest(
            schema=self._new_schema_identity(),
            wait_id=resolved_wait_id,
        )
        payload, _ = await self._call(
            INSPECT_WAIT_METHOD,
            msg.SerializeToString(),
            _QUERY_SEMANTICS,
            deadline_monotonic_ns=deadline_monotonic_ns,
            correlation_id=correlation_id,
            retry_of=retry_of,
        )
        res = self._parse_result(
            _wait_pb.InspectWaitResult, payload, INSPECT_WAIT_METHOD
        )
        if not res.HasField("record"):
            raise IpcError(
                "COMPATIBILITY",
                "WaitControl.inspect_wait result is missing record",
            )
        return InspectWaitResult(
            record=_decode_wait_record(res.record, INSPECT_WAIT_METHOD)
        )

    # Ergonomic aliases matching SABI verb naming.
    async def register(
        self,
        binding: bytes,
        channel_id: bytes,
        target_sequence: int,
        **kwargs: Any,
    ) -> RegisterWaitResult:
        """Alias for :meth:`register_wait`."""

        return await self.register_wait(binding, channel_id, target_sequence, **kwargs)

    async def inspect(self, wait_id: bytes, **kwargs: Any) -> InspectWaitResult:
        """Alias for :meth:`inspect_wait`."""

        return await self.inspect_wait(wait_id, **kwargs)

    async def list(
        self, filter_channel_id: bytes | None = None, **kwargs: Any
    ) -> ListWaitsResult:
        """Alias for :meth:`list_waits`."""

        return await self.list_waits(filter_channel_id, **kwargs)

    async def cancel(self, wait_id: bytes, **kwargs: Any) -> CancelWaitResult:
        """Alias for :meth:`cancel_wait`."""

        return await self.cancel_wait(wait_id, **kwargs)


__all__ = [
    "CancelWaitRequest",
    "CancelWaitResult",
    "DEFAULT_PROCESS_GENERATION",
    "INSPECT_WAIT_METHOD",
    "InspectWaitRequest",
    "InspectWaitResult",
    "LIST_WAITS_METHOD",
    "ListWaitsRequest",
    "ListWaitsResult",
    "NOTIFY_COMMITS_METHOD",
    "NotifyCommitsRequest",
    "REGISTER_WAIT_METHOD",
    "RegisterWaitRequest",
    "RegisterWaitResult",
    "WAIT_CONTROL_MAJOR",
    "WAIT_CONTROL_MINOR",
    "WAIT_CONTROL_SCHEMA",
    "WAIT_CONTROL_SERVICE",
    "WAIT_STATE_CANCELLED",
    "WAIT_STATE_PENDING",
    "WAIT_STATE_UNSPECIFIED",
    "WAIT_STATE_WOKEN",
    "WaitClient",
    "WaitControlError",
    "WaitRecord",
    "WakeReport",
]
