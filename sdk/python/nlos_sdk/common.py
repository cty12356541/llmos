"""Fail-closed validators for the candidate common SABI metadata."""

from __future__ import annotations

from dataclasses import dataclass

from nlos.sabi.v1 import envelope_pb2 as _pb

_ID_BYTES = 16
_SHA256_BYTES = 32
_MAX_ACTIVITY_CONTEXT_BYTES = 4 * 1024
_MAX_CAPABILITY_HANDLES = 64
_MAX_RECEIPTS = 64
_MAX_SAFE_MESSAGE_BYTES = 512


@dataclass(frozen=True, slots=True)
class MethodSemantics:
    """Negotiated behavior needed to validate common request requirements."""

    side_effecting: bool = False
    long_running: bool = False


class CommonSemanticsError(Exception):
    """A malformed or unsafe common SABI context."""

    def __init__(self, code: str, message: str) -> None:
        super().__init__(message)
        self.code = code


def validate_request_context(
    envelope: _pb.Envelope,
    semantics: MethodSemantics,
    now_monotonic_ns: int,
) -> _pb.SabiRequestContext:
    """Validate caller, authority, deadline, and idempotency metadata."""

    if envelope.WhichOneof("common_context") != "request_context":
        _fail("MISSING_REQUEST_CONTEXT", "SABI request context is missing")
    context = envelope.request_context
    if not context.HasField("caller"):
        _fail("MISSING_CALLER", "caller identity is missing")
    _require_id("principal_id", context.caller.principal_id)
    _require_id("application_id", context.caller.application_id)
    _require_id("process_id", context.caller.process_id)
    _require_positive("process_generation", context.caller.process_generation)
    _require_id("correlation_id", context.correlation_id)

    if not context.idempotency_key:
        if semantics.side_effecting:
            _fail(
                "MISSING_IDEMPOTENCY_KEY",
                "side-effecting call requires an idempotency key",
            )
    else:
        _require_id("idempotency_key", context.idempotency_key)
    if context.deadline_monotonic_ns == 0:
        if semantics.long_running:
            _fail("MISSING_DEADLINE", "long-running call requires a deadline")
    elif context.deadline_monotonic_ns <= now_monotonic_ns:
        _fail("DEADLINE_EXPIRED", "call deadline has expired")
    if len(context.activity_context) > _MAX_ACTIVITY_CONTEXT_BYTES:
        _fail("ACTIVITY_CONTEXT_TOO_LARGE", "activity context exceeds 4 KiB")
    if context.HasField("task_execution_binding"):
        binding = context.task_execution_binding
        _require_id("task_attempt_id", binding.task_attempt_id)
        _require_positive("task_authority_term", binding.task_authority_term)
        _require_positive(
            "isolation_domain_generation",
            binding.isolation_domain_generation,
        )
    _require_capabilities(
        context.capability_handles,
        context.reservation_handle
        if context.HasField("reservation_handle")
        else None,
    )
    digest_length = len(context.proposal_or_input_digest_sha256)
    if digest_length not in (0, _SHA256_BYTES):
        _fail(
            "INVALID_PROPOSAL_DIGEST",
            "proposal/input digest must be SHA-256 sized",
        )
    return context


def validate_response_context(
    envelope: _pb.Envelope,
    semantics: MethodSemantics,
) -> _pb.SabiResponseContext:
    """Validate common errors and preserve safe retry/reconciliation rules."""

    if envelope.WhichOneof("common_context") != "response_context":
        _fail("MISSING_RESPONSE_CONTEXT", "SABI response context is missing")
    context = envelope.response_context
    _require_id("correlation_id", context.correlation_id)
    if context.HasField("operation"):
        _require_operation(context.operation)
    _require_receipts(context.receipts)
    if (
        semantics.side_effecting
        and not context.HasField("operation")
        and not context.receipts
    ):
        _fail(
            "MISSING_EFFECT_EVIDENCE",
            "mutation response requires Operation or Receipt",
        )
    if not context.HasField("failure"):
        return context

    failure = context.failure
    try:
        _pb.SabiErrorCode.Name(failure.code)
    except ValueError:
        _fail("INVALID_ERROR_CODE", "unknown SABI error code")
    if failure.code == _pb.SABI_ERROR_CODE_UNSPECIFIED:
        _fail("INVALID_ERROR_CODE", "unspecified SABI error code")
    try:
        _pb.RetryDirective.Name(failure.retry)
    except ValueError:
        _fail("INVALID_RETRY_DIRECTIVE", "unknown retry directive")
    if failure.retry == _pb.RETRY_DIRECTIVE_UNSPECIFIED:
        _fail("INVALID_RETRY_DIRECTIVE", "unspecified retry directive")
    if (
        len(failure.safe_message.encode("utf-8")) > _MAX_SAFE_MESSAGE_BYTES
        or "\0" in failure.safe_message
    ):
        _fail(
            "UNSAFE_ERROR_MESSAGE",
            "safe error message is oversized or contains NUL",
        )

    has_operation = context.HasField("operation")
    has_receipt = bool(context.receipts)
    if (
        failure.retry
        == _pb.RETRY_DIRECTIVE_QUERY_OPERATION_OR_RETRY_SAME_IDEMPOTENCY_KEY
        and not has_operation
    ):
        _fail(
            "MISSING_OPERATION",
            "query-operation retry requires an Operation reference",
        )
    if failure.code in (
        _pb.SABI_ERROR_CODE_UNCERTAIN,
        _pb.SABI_ERROR_CODE_EFFECT_UNKNOWN,
    ):
        if (
            failure.retry
            != _pb.RETRY_DIRECTIVE_QUERY_OPERATION_OR_RETRY_SAME_IDEMPOTENCY_KEY
        ):
            _fail(
                "UNSAFE_RETRY",
                "uncertain outcome must preserve the original idempotency key",
            )
    elif (
        failure.code == _pb.SABI_ERROR_CODE_RETRY
        and failure.retry
        != _pb.RETRY_DIRECTIVE_RETRY_SAME_IDEMPOTENCY_KEY
    ):
        _fail(
            "UNSAFE_RETRY",
            "retry outcome must preserve the original idempotency key",
        )
    if failure.code == _pb.SABI_ERROR_CODE_PARTIAL and not has_receipt:
        _fail("MISSING_RECEIPT", "partial outcome requires a Receipt reference")
    return context


def _require_id(field: str, value: bytes) -> None:
    if len(value) != _ID_BYTES:
        _fail("INVALID_ID", f"{field} must contain exactly {_ID_BYTES} bytes")


def _require_positive(field: str, value: int) -> None:
    if isinstance(value, bool) or not isinstance(value, int) or value <= 0:
        _fail("INVALID_GENERATION", f"{field} must be positive")


def _require_capabilities(handles: object, reservation: object | None) -> None:
    if len(handles) > _MAX_CAPABILITY_HANDLES:
        _fail("TOO_MANY_CAPABILITIES", "too many capability handles")
    seen: set[tuple[int, int]] = set()
    values = list(handles)
    if reservation is not None:
        values.append(reservation)
    for handle in values:
        _require_positive("capability_slot", handle.slot)
        _require_positive("capability_generation", handle.generation)
        key = (handle.slot, handle.generation)
        if key in seen:
            _fail("DUPLICATE_CAPABILITY", "duplicate capability handle")
        seen.add(key)


def _require_operation(operation: _pb.OperationReference) -> None:
    _require_id("operation_id", operation.operation_id)
    _require_positive("operation_generation", operation.generation)


def _require_receipts(receipts: object) -> None:
    if len(receipts) > _MAX_RECEIPTS:
        _fail("TOO_MANY_RECEIPTS", "too many receipts")
    seen: set[bytes] = set()
    for receipt in receipts:
        _require_id("receipt_id", receipt.receipt_id)
        if receipt.receipt_id in seen:
            _fail("DUPLICATE_RECEIPT", "duplicate receipt reference")
        seen.add(receipt.receipt_id)


def _fail(code: str, message: str) -> None:
    raise CommonSemanticsError(code, message)
