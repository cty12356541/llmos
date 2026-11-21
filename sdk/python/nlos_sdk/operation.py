"""Thin SABI v1.0 client for ``nlos.sabi.OperationControl``."""

from __future__ import annotations

import secrets
from dataclasses import dataclass
from enum import IntEnum
from typing import Any

from ._client_base import (
    DEFAULT_PROCESS_GENERATION,
    SabiServiceClientBase,
    SabiServiceError,
    require_bytes_len,
    require_uint,
)
from .common import MethodSemantics, _ensure_gen_python_path
from .local_rpc import IpcError

_ensure_gen_python_path()

from nlos.sabi.v1 import envelope_pb2 as _env_pb  # noqa: E402
from nlos.sabi.v1 import operation_control_pb2 as _op_pb  # noqa: E402

OPERATION_CONTROL_SCHEMA = "nlos.sabi.OperationControl"
OPERATION_CONTROL_MAJOR = 1
OPERATION_CONTROL_MINOR = 0
OPERATION_CONTROL_SERVICE = "operation_control"

QUERY_OPERATION_METHOD = "query"
CANCEL_OPERATION_METHOD = "cancel"


class OperationLifecycleState(IntEnum):
    """Typed operation lifecycle states defined by ``OperationControl``."""

    UNSPECIFIED = _op_pb.OPERATION_LIFECYCLE_STATE_UNSPECIFIED
    REGISTERED = _op_pb.OPERATION_LIFECYCLE_STATE_REGISTERED
    DISPATCHED = _op_pb.OPERATION_LIFECYCLE_STATE_DISPATCHED
    CANCEL_REQUESTED = _op_pb.OPERATION_LIFECYCLE_STATE_CANCEL_REQUESTED
    COMPLETED = _op_pb.OPERATION_LIFECYCLE_STATE_COMPLETED
    FAILED = _op_pb.OPERATION_LIFECYCLE_STATE_FAILED
    CANCELLED_BEFORE_EFFECT = _op_pb.OPERATION_LIFECYCLE_STATE_CANCELLED_BEFORE_EFFECT
    PARTIAL_EFFECT = _op_pb.OPERATION_LIFECYCLE_STATE_PARTIAL_EFFECT
    EFFECT_UNKNOWN = _op_pb.OPERATION_LIFECYCLE_STATE_EFFECT_UNKNOWN


OPERATION_STATE_UNSPECIFIED = OperationLifecycleState.UNSPECIFIED
OPERATION_STATE_REGISTERED = OperationLifecycleState.REGISTERED
OPERATION_STATE_DISPATCHED = OperationLifecycleState.DISPATCHED
OPERATION_STATE_CANCEL_REQUESTED = OperationLifecycleState.CANCEL_REQUESTED
OPERATION_STATE_COMPLETED = OperationLifecycleState.COMPLETED
OPERATION_STATE_FAILED = OperationLifecycleState.FAILED
OPERATION_STATE_CANCELLED_BEFORE_EFFECT = OperationLifecycleState.CANCELLED_BEFORE_EFFECT
OPERATION_STATE_PARTIAL_EFFECT = OperationLifecycleState.PARTIAL_EFFECT
OPERATION_STATE_EFFECT_UNKNOWN = OperationLifecycleState.EFFECT_UNKNOWN

_QUERY_SEMANTICS = MethodSemantics(side_effecting=False, long_running=False)
_MUTATION_SEMANTICS = MethodSemantics(side_effecting=True, long_running=False)

_ID_BYTES = 16
_VALID_STATES = frozenset(
    int(state)
    for state in OperationLifecycleState
    if state != OperationLifecycleState.UNSPECIFIED
)


@dataclass(frozen=True, slots=True)
class OperationReference:
    """Reference to an asynchronous SABI operation."""

    operation_id: bytes
    generation: int


@dataclass(frozen=True, slots=True)
class QueryOperationRequest:
    """Typed request parameters for ``OperationControl.query``."""

    operation: OperationReference


@dataclass(frozen=True, slots=True)
class CancelOperationRequest:
    """Typed request parameters for ``OperationControl.cancel``."""

    operation: OperationReference
    expected_cancel_epoch: int


@dataclass(frozen=True, slots=True)
class OperationStatus:
    """Lifecycle snapshot returned by ``OperationControl``."""

    operation: OperationReference
    state: OperationLifecycleState
    cancel_epoch: int
    receipt_id: bytes | None = None

    @property
    def is_terminal(self) -> bool:
        return self.state in (
            OperationLifecycleState.COMPLETED,
            OperationLifecycleState.FAILED,
            OperationLifecycleState.CANCELLED_BEFORE_EFFECT,
            OperationLifecycleState.PARTIAL_EFFECT,
            OperationLifecycleState.EFFECT_UNKNOWN,
        )

    def __repr__(self) -> str:
        # Redact raw receipt bytes while retaining operational metadata.
        receipt_desc = f"<{len(self.receipt_id)} bytes>" if self.receipt_id else "None"
        return (
            f"OperationStatus(operation_id_len={len(self.operation.operation_id)}, "
            f"generation={self.operation.generation}, state={self.state.name}, "
            f"cancel_epoch={self.cancel_epoch}, receipt_id={receipt_desc})"
        )


class OperationControlError(SabiServiceError):
    """Typed SABI failure returned by ``OperationControl``."""


def _normalize_operation_ref(
    operation: OperationReference | bytes,
    generation: int | None = None,
) -> OperationReference:
    if isinstance(operation, OperationReference):
        if generation is not None and generation != operation.generation:
            raise IpcError(
                "INVALID_ARGUMENT",
                "generation argument conflicts with OperationReference.generation",
            )
        op_id = require_bytes_len("operation_id", operation.operation_id, _ID_BYTES)
        gen = require_uint("generation", operation.generation, minimum=1)
        return OperationReference(operation_id=op_id, generation=gen)

    if generation is None:
        raise IpcError(
            "INVALID_GENERATION",
            "generation must be provided when operation_id is passed as bytes",
        )
    op_id = require_bytes_len("operation_id", operation, _ID_BYTES)
    gen = require_uint("generation", generation, minimum=1)
    return OperationReference(operation_id=op_id, generation=gen)


def _decode_operation_status(
    raw: _op_pb.OperationStatus,
    method: str,
) -> OperationStatus:
    if not raw.HasField("operation"):
        raise IpcError(
            "COMPATIBILITY",
            f"OperationControl.{method} response is missing operation reference",
        )
    op_id = bytes(raw.operation.operation_id)
    if len(op_id) != _ID_BYTES or raw.operation.generation <= 0:
        raise IpcError(
            "COMPATIBILITY",
            f"OperationControl.{method} returned malformed operation reference",
        )
    if int(raw.state) not in _VALID_STATES:
        raise IpcError(
            "COMPATIBILITY",
            f"OperationControl.{method} returned invalid lifecycle state {raw.state}",
        )
    receipt_id: bytes | None = None
    if raw.HasField("receipt"):
        if not raw.receipt.receipt_id:
            raise IpcError(
                "COMPATIBILITY",
                f"OperationControl.{method} returned empty receipt reference",
            )
        receipt_id = bytes(raw.receipt.receipt_id)

    return OperationStatus(
        operation=OperationReference(
            operation_id=op_id,
            generation=int(raw.operation.generation),
        ),
        state=OperationLifecycleState(int(raw.state)),
        cancel_epoch=int(raw.cancel_epoch),
        receipt_id=receipt_id,
    )


class OperationClient(SabiServiceClientBase):
    """High-level SABI client for ``nlos.sabi.OperationControl`` (``v1.0``)."""

    _service_name = OPERATION_CONTROL_SERVICE
    _schema_name = OPERATION_CONTROL_SCHEMA
    _schema_major = OPERATION_CONTROL_MAJOR
    _schema_minor = OPERATION_CONTROL_MINOR
    _error_type = OperationControlError

    async def query(
        self,
        operation: OperationReference | bytes,
        generation: int | None = None,
        *,
        deadline_monotonic_ns: int = 0,
        correlation_id: bytes | None = None,
        retry_of: Exception | None = None,
    ) -> OperationStatus:
        """Query the durable lifecycle status of an operation."""

        op_ref = _normalize_operation_ref(operation, generation)
        msg = _op_pb.QueryOperationRequest(
            schema=self._new_schema_identity(),
            operation=_env_pb.OperationReference(
                operation_id=op_ref.operation_id,
                generation=op_ref.generation,
            ),
        )
        payload, _ = await self._call(
            QUERY_OPERATION_METHOD,
            msg.SerializeToString(),
            _QUERY_SEMANTICS,
            deadline_monotonic_ns=deadline_monotonic_ns,
            correlation_id=correlation_id,
            retry_of=retry_of,
        )
        res = self._parse_result(
            _op_pb.OperationStatus, payload, QUERY_OPERATION_METHOD
        )
        return _decode_operation_status(res, QUERY_OPERATION_METHOD)

    async def cancel(
        self,
        operation: OperationReference | bytes,
        generation: int | None = None,
        *,
        expected_cancel_epoch: int,
        idempotency_key: bytes | None = None,
        deadline_monotonic_ns: int = 0,
        correlation_id: bytes | None = None,
        retry_of: Exception | None = None,
    ) -> OperationStatus:
        """Request epoch-fenced cancellation of an operation."""

        op_ref = _normalize_operation_ref(operation, generation)
        epoch = require_uint(
            "expected_cancel_epoch", expected_cancel_epoch, minimum=0
        )
        resolved_key = (
            secrets.token_bytes(_ID_BYTES)
            if idempotency_key is None
            else require_bytes_len("idempotency_key", idempotency_key, _ID_BYTES)
        )
        msg = _op_pb.CancelOperationRequest(
            schema=self._new_schema_identity(),
            operation=_env_pb.OperationReference(
                operation_id=op_ref.operation_id,
                generation=op_ref.generation,
            ),
            expected_cancel_epoch=epoch,
        )
        payload, _ = await self._call(
            CANCEL_OPERATION_METHOD,
            msg.SerializeToString(),
            _MUTATION_SEMANTICS,
            idempotency_key=resolved_key,
            deadline_monotonic_ns=deadline_monotonic_ns,
            correlation_id=correlation_id,
            retry_of=retry_of,
        )
        res = self._parse_result(
            _op_pb.OperationStatus, payload, CANCEL_OPERATION_METHOD
        )
        return _decode_operation_status(res, CANCEL_OPERATION_METHOD)

    async def query_operation(
        self,
        operation: OperationReference | bytes,
        generation: int | None = None,
        **kwargs: Any,
    ) -> OperationStatus:
        """Alias for :meth:`query`."""

        return await self.query(operation, generation, **kwargs)

    async def cancel_operation(
        self,
        operation: OperationReference | bytes,
        generation: int | None = None,
        **kwargs: Any,
    ) -> OperationStatus:
        """Alias for :meth:`cancel`."""

        return await self.cancel(operation, generation, **kwargs)


__all__ = [
    "CANCEL_OPERATION_METHOD",
    "CancelOperationRequest",
    "DEFAULT_PROCESS_GENERATION",
    "OPERATION_CONTROL_MAJOR",
    "OPERATION_CONTROL_MINOR",
    "OPERATION_CONTROL_SCHEMA",
    "OPERATION_CONTROL_SERVICE",
    "OPERATION_STATE_CANCELLED_BEFORE_EFFECT",
    "OPERATION_STATE_CANCEL_REQUESTED",
    "OPERATION_STATE_COMPLETED",
    "OPERATION_STATE_DISPATCHED",
    "OPERATION_STATE_EFFECT_UNKNOWN",
    "OPERATION_STATE_FAILED",
    "OPERATION_STATE_PARTIAL_EFFECT",
    "OPERATION_STATE_REGISTERED",
    "OPERATION_STATE_UNSPECIFIED",
    "OperationClient",
    "OperationControlError",
    "OperationLifecycleState",
    "OperationReference",
    "OperationStatus",
    "QUERY_OPERATION_METHOD",
    "QueryOperationRequest",
]
