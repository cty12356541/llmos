"""Shared thin client base for SABI v1.0/v1.1 service channels."""

from __future__ import annotations

import hashlib
import secrets
import time
from typing import Any, Callable, Sequence, TypeVar

from google.protobuf.message import DecodeError, Message

from .common import (
    CommonSemanticsError,
    MethodSemantics,
    _ensure_gen_python_path,
    validate_request_context,
    validate_response_context,
    validate_schema_compatibility,
)
from .local_rpc import IpcError, LocalRpcClient, TransportConfig
from .principal_session import (
    SessionIdentity,
    connect_authenticated as _connect_authenticated_rpc,
)
from .service_directory import ServiceRequirement

_ensure_gen_python_path()

from nlos.sabi.v1 import envelope_pb2 as _env_pb  # noqa: E402

ENVELOPE_SCHEMA = "nlos.sabi.Envelope"
ENVELOPE_MAJOR = 1
ENVELOPE_MINOR = 1
MAX_CONTROL_PAYLOAD_BYTES = 64 * 1024
DEFAULT_CAPABILITY_SLOT = 9
DEFAULT_CAPABILITY_GENERATION = 1
DEFAULT_PROCESS_GENERATION = 1
DEFAULT_DEADLINE_HORIZON_NS = 30_000_000_000

_ID_BYTES = 16
_SHA256_BYTES = 32
_U64_MAX = (1 << 64) - 1

_TClient = TypeVar("_TClient", bound="SabiServiceClientBase")
_TMsg = TypeVar("_TMsg", bound=Message)


def default_deadline_monotonic_ns(
    timeout_ns: int = DEFAULT_DEADLINE_HORIZON_NS,
) -> int:
    """Return a future deadline compatible with both Python monotonic and epoch clocks."""

    if isinstance(timeout_ns, bool) or not isinstance(timeout_ns, int) or timeout_ns <= 0:
        raise IpcError("INVALID_CONFIG", "timeout_ns must be a positive integer")
    return max(time.monotonic_ns(), time.time_ns()) + timeout_ns


def default_wall_ms() -> int:
    """Return a positive millisecond timestamp for non-authoritative request fields."""

    return max(1, time.time_ns() // 1_000_000)


def require_bytes_len(
    field: str,
    value: Any,
    expected_len: int,
    *,
    code: str = "INVALID_ID",
) -> bytes:
    """Validate fixed-length bytes-like value without leaking raw bytes in errors."""

    if not isinstance(value, (bytes, bytearray, memoryview)):
        raise CommonSemanticsError(code, f"{field} must be bytes")
    raw = bytes(value)
    if len(raw) != expected_len:
        raise CommonSemanticsError(
            code,
            f"{field} must contain exactly {expected_len} bytes (got {len(raw)})",
        )
    return raw


def require_uint(
    field: str,
    value: Any,
    *,
    minimum: int = 0,
    maximum: int | None = _U64_MAX,
    code: str = "INVALID_ARGUMENT",
) -> int:
    """Validate a bounded non-boolean integer field."""

    if isinstance(value, bool) or not isinstance(value, int):
        raise IpcError(code, f"{field} must be an integer")
    if value < minimum or (maximum is not None and value > maximum):
        bound = f"{minimum}..={maximum}" if maximum is not None else f">={minimum}"
        raise IpcError(code, f"{field} must be within {bound}")
    return value


def require_bounded_str(
    field: str,
    value: Any,
    *,
    max_bytes: int | None = None,
    allow_empty: bool = False,
) -> str:
    """Validate string field without NUL bytes or raw value leakage in errors."""

    if not isinstance(value, str):
        raise IpcError("INVALID_ARGUMENT", f"{field} must be a string")
    if "\0" in value:
        raise IpcError("INVALID_ARGUMENT", f"{field} must not contain NUL bytes")
    if not allow_empty and not value:
        raise IpcError("INVALID_ARGUMENT", f"{field} must not be empty")
    if max_bytes is not None and len(value.encode("utf-8")) > max_bytes:
        raise IpcError(
            "FRAME_TOO_LARGE",
            f"{field} exceeds {max_bytes} UTF-8 bytes",
        )
    return value


def set_schema_identity(
    identity: _env_pb.SchemaIdentity,
    name: str,
    major: int = 1,
    minor: int = 0,
) -> None:
    """Populate a protobuf SchemaIdentity message."""

    identity.name = name
    identity.major = major
    identity.minor = minor


class SabiServiceError(Exception):
    """Typed SABI failure envelope returned by a service channel."""

    def __init__(
        self,
        service: str,
        code: int,
        retry: int,
        detail: str,
        method: str = "",
        *,
        operation: _env_pb.OperationReference | None = None,
        receipts: tuple[bytes, ...] = (),
        idempotency_key: bytes = b"",
        request_digest_sha256: bytes = b"",
    ) -> None:
        try:
            code_name = _env_pb.SabiErrorCode.Name(code)
        except ValueError:
            code_name = str(code)
        try:
            retry_name = _env_pb.RetryDirective.Name(retry)
        except ValueError:
            retry_name = str(retry)
        super().__init__(
            f"{service}.{method} failed: code={code_name} ({code}), "
            f"retry={retry_name} ({retry}), detail={detail!r}"
        )
        self.service = service
        self.method = method
        self.code = code
        self.retry = retry
        self.detail = detail
        self.operation = operation
        self.receipts = receipts
        self.idempotency_key = bytes(idempotency_key)
        self.request_digest_sha256 = bytes(request_digest_sha256)

    @property
    def safe_message(self) -> str:
        return self.detail

    @property
    def is_uncertain(self) -> bool:
        return self.code in (
            _env_pb.SABI_ERROR_CODE_UNCERTAIN,
            _env_pb.SABI_ERROR_CODE_EFFECT_UNKNOWN,
        )

    @property
    def is_retryable(self) -> bool:
        return self.retry in (
            _env_pb.RETRY_DIRECTIVE_RETRY_SAME_IDEMPOTENCY_KEY,
            _env_pb.RETRY_DIRECTIVE_QUERY_OPERATION_OR_RETRY_SAME_IDEMPOTENCY_KEY,
        )

    @property
    def requires_operation_query(self) -> bool:
        return (
            self.retry
            == _env_pb.RETRY_DIRECTIVE_QUERY_OPERATION_OR_RETRY_SAME_IDEMPOTENCY_KEY
        )


def validate_retry_attempt(
    previous_error: Exception,
    *,
    service: str,
    method: str,
    idempotency_key: bytes,
    request_digest_sha256: bytes,
    side_effecting: bool,
) -> None:
    """Enforce safe same-idempotency-key retry rules against a prior failure."""

    if isinstance(previous_error, SabiServiceError):
        if not previous_error.is_retryable:
            raise CommonSemanticsError(
                "UNSAFE_RETRY",
                "previous failure carries DO_NOT_RETRY directive",
            )
        if (previous_error.service, previous_error.method) != (service, method):
            raise CommonSemanticsError(
                "UNSAFE_RETRY",
                "retry target service or method does not match original request",
            )
        if not side_effecting or not previous_error.idempotency_key:
            raise CommonSemanticsError(
                "UNSAFE_RETRY",
                "idempotent retry requires a side-effecting call with original key",
            )
        if idempotency_key != previous_error.idempotency_key:
            raise CommonSemanticsError(
                "UNSAFE_RETRY",
                "retry must preserve the original idempotency_key",
            )
        if (
            previous_error.request_digest_sha256
            and request_digest_sha256 != previous_error.request_digest_sha256
        ):
            raise CommonSemanticsError(
                "UNSAFE_RETRY",
                "retry payload digest does not match original request",
            )
        return

    if isinstance(previous_error, IpcError):
        if not previous_error.retryable:
            raise CommonSemanticsError(
                "UNSAFE_RETRY",
                f"IPC error {previous_error.code} is non-retryable",
            )
        if previous_error.side_effecting or side_effecting:
            if (previous_error.service, previous_error.method) != (service, method):
                raise CommonSemanticsError(
                    "UNSAFE_RETRY",
                    "retry target service or method does not match original request",
                )
            if not previous_error.idempotency_key:
                raise CommonSemanticsError(
                    "UNSAFE_RETRY",
                    "missing original idempotency_key on failed call",
                )
            if idempotency_key != previous_error.idempotency_key:
                raise CommonSemanticsError(
                    "UNSAFE_RETRY",
                    "retry after uncertain transport error must reuse idempotency_key",
                )
            if (
                previous_error.request_digest_sha256
                and request_digest_sha256 != previous_error.request_digest_sha256
            ):
                raise CommonSemanticsError(
                    "UNSAFE_RETRY",
                    "retry payload digest does not match original request",
                )
        return

    raise CommonSemanticsError(
        "UNSAFE_RETRY",
        "cannot retry unknown or non-SABI error type",
    )


def _normalize_capability_handles(
    capability_slot: int,
    capability_generation: int,
    capability_handles: Sequence[tuple[int, int]] | None,
) -> tuple[tuple[int, int], ...]:
    if capability_handles is None:
        handles = ((capability_slot, capability_generation),)
    else:
        handles = tuple((int(slot), int(gen)) for slot, gen in capability_handles)
    if len(handles) > 64:
        raise CommonSemanticsError(
            "TOO_MANY_CAPABILITIES",
            "too many capability handles",
        )
    seen: set[tuple[int, int]] = set()
    for slot, gen in handles:
        if (
            isinstance(slot, bool)
            or isinstance(gen, bool)
            or slot <= 0
            or gen <= 0
        ):
            raise CommonSemanticsError(
                "INVALID_GENERATION",
                "capability slot and generation must be positive",
            )
        if (slot, gen) in seen:
            raise CommonSemanticsError(
                "DUPLICATE_CAPABILITY",
                "duplicate capability handle",
            )
        seen.add((slot, gen))
    return handles


class SabiServiceClientBase:
    """Shared transport, context validation, retry check, and error mapping."""

    _service_name: str = ""
    _schema_name: str = ""
    _schema_major: int = 1
    _schema_minor: int = 0
    _max_payload_bytes: int = MAX_CONTROL_PAYLOAD_BYTES
    _error_type: type[SabiServiceError] = SabiServiceError

    def __init__(
        self,
        endpoint_or_rpc: str | LocalRpcClient,
        config: TransportConfig | None = None,
        *,
        principal_id: bytes | None = None,
        application_id: bytes | None = None,
        process_id: bytes | None = None,
        process_generation: int = DEFAULT_PROCESS_GENERATION,
        capability_slot: int = DEFAULT_CAPABILITY_SLOT,
        capability_generation: int = DEFAULT_CAPABILITY_GENERATION,
        capability_handles: Sequence[tuple[int, int]] | None = None,
        now_fn: Callable[[], int] | None = None,
    ) -> None:
        if isinstance(endpoint_or_rpc, str):
            if not endpoint_or_rpc:
                raise IpcError("INVALID_CONFIG", "IPC endpoint must not be empty")
        elif not isinstance(endpoint_or_rpc, LocalRpcClient):
            raise IpcError(
                "INVALID_CONFIG",
                "endpoint_or_rpc must be an endpoint path or LocalRpcClient",
            )

        identity = SessionIdentity(
            principal_id=(
                bytes(principal_id)
                if principal_id is not None
                else secrets.token_bytes(_ID_BYTES)
            ),
            application_id=(
                bytes(application_id)
                if application_id is not None
                else secrets.token_bytes(_ID_BYTES)
            ),
            process_id=(
                bytes(process_id)
                if process_id is not None
                else secrets.token_bytes(_ID_BYTES)
            ),
            process_generation=process_generation,
        )
        self._target = endpoint_or_rpc
        self._config = config or TransportConfig()
        self._principal_id = identity.principal_id
        self._application_id = identity.application_id
        self._process_id = identity.process_id
        self._process_generation = identity.process_generation
        self._capability_handles = _normalize_capability_handles(
            capability_slot,
            capability_generation,
            capability_handles,
        )
        self._now_fn = now_fn or time.monotonic_ns

    @property
    def principal_id(self) -> bytes:
        return self._principal_id

    @property
    def is_usable(self) -> bool:
        """Whether underlying persistent connection is usable (always True for one-shot)."""

        if isinstance(self._target, LocalRpcClient):
            return self._target.is_usable
        return True

    async def __aenter__(self: _TClient) -> _TClient:
        return self

    async def __aexit__(self, *_exc: object) -> None:
        await self.close()

    @classmethod
    def service_requirement(
        cls,
        *,
        minimum_minor: int | None = None,
        required_feature_ids: tuple[int, ...] = (),
    ) -> ServiceRequirement:
        """Return the ``ServiceDirectory`` requirement for this service client."""

        return ServiceRequirement(
            service=cls._service_name,
            schema_name=cls._schema_name,
            major=cls._schema_major,
            minimum_minor=(
                cls._schema_minor if minimum_minor is None else minimum_minor
            ),
            required_feature_ids=required_feature_ids,
        )

    @classmethod
    async def connect_authenticated(
        cls: type[_TClient],
        endpoint: str,
        *,
        signer: Any,
        principal_id: bytes,
        config: TransportConfig | None = None,
        application_id: bytes | None = None,
        process_id: bytes | None = None,
        process_generation: int = DEFAULT_PROCESS_GENERATION,
        **kwargs: Any,
    ) -> _TClient:
        """Complete ADR-0011 handshake once and reuse the authenticated connection."""

        rpc = await _connect_authenticated_rpc(
            endpoint, signer, principal_id, config
        )
        try:
            return cls(
                rpc,
                config=config,
                principal_id=bytes(principal_id),
                application_id=(
                    bytes(application_id)
                    if application_id is not None
                    else secrets.token_bytes(_ID_BYTES)
                ),
                process_id=(
                    bytes(process_id)
                    if process_id is not None
                    else secrets.token_bytes(_ID_BYTES)
                ),
                process_generation=process_generation,
                **kwargs,
            )
        except BaseException:
            await rpc.close()
            raise

    @classmethod
    def from_endpoint(
        cls: type[_TClient],
        endpoint: str,
        config: TransportConfig | None = None,
        **kwargs: Any,
    ) -> _TClient:
        """Construct a client opening a fresh local connection per RPC."""

        return cls(endpoint, config=config, **kwargs)

    async def close(self) -> None:
        """Close the persistent connection (idempotent; no-op for one-shot endpoint)."""

        if isinstance(self._target, LocalRpcClient):
            await self._target.close()

    def _new_schema_identity(self) -> _env_pb.SchemaIdentity:
        return _env_pb.SchemaIdentity(
            name=self._schema_name,
            major=self._schema_major,
            minor=self._schema_minor,
        )

    def _parse_result(
        self,
        message_cls: type[_TMsg],
        payload: bytes,
        method: str,
    ) -> _TMsg:
        if len(payload) > self._max_payload_bytes:
            raise IpcError(
                "FRAME_TOO_LARGE",
                f"{self._service_name}.{method} response payload exceeds "
                f"{self._max_payload_bytes} bytes",
            )
        try:
            parsed = message_cls.FromString(payload)
        except DecodeError:
            raise IpcError(
                "COMPATIBILITY",
                f"malformed {self._service_name}.{method} response payload",
            ) from None
        schema = parsed.schema if parsed.HasField("schema") else None  # type: ignore[attr-defined]
        validate_schema_compatibility(schema, self._schema_name)
        return parsed

    async def _call(
        self,
        method: str,
        payload: bytes,
        semantics: MethodSemantics,
        *,
        idempotency_key: bytes | None = None,
        require_explicit_idempotency_key: bool = False,
        deadline_monotonic_ns: int | None = 0,
        require_deadline: bool | None = None,
        now_monotonic_ns: int | None = None,
        correlation_id: bytes | None = None,
        proposal_or_input_digest_sha256: bytes = b"",
        retry_of: Exception | None = None,
    ) -> tuple[bytes, _env_pb.SabiResponseContext]:
        if len(payload) > self._max_payload_bytes:
            raise IpcError(
                "FRAME_TOO_LARGE",
                f"{self._service_name}.{method} request payload has {len(payload)} bytes; "
                f"maximum is {self._max_payload_bytes}",
            )

        resolved_key = b""
        if semantics.side_effecting:
            if idempotency_key is None:
                if require_explicit_idempotency_key or retry_of is not None:
                    raise CommonSemanticsError(
                        "MISSING_IDEMPOTENCY_KEY",
                        f"{self._service_name}.{method} requires an explicit idempotency_key",
                    )
                resolved_key = secrets.token_bytes(_ID_BYTES)
            else:
                if not isinstance(idempotency_key, (bytes, bytearray, memoryview)):
                    raise CommonSemanticsError(
                        "INVALID_ID",
                        "idempotency_key must be bytes",
                    )
                resolved_key = bytes(idempotency_key)
                if not resolved_key:
                    raise CommonSemanticsError(
                        "MISSING_IDEMPOTENCY_KEY",
                        "side-effecting call requires a non-empty idempotency_key",
                    )
        elif idempotency_key is not None:
            resolved_key = require_bytes_len("idempotency_key", idempotency_key, _ID_BYTES)

        must_have_deadline = (
            semantics.long_running if require_deadline is None else require_deadline
        )
        if deadline_monotonic_ns is None:
            resolved_deadline = 0
        else:
            if (
                isinstance(deadline_monotonic_ns, bool)
                or not isinstance(deadline_monotonic_ns, int)
                or deadline_monotonic_ns < 0
            ):
                raise CommonSemanticsError(
                    "INVALID_DEADLINE",
                    "deadline_monotonic_ns must be a non-negative integer",
                )
            resolved_deadline = deadline_monotonic_ns
        if must_have_deadline and resolved_deadline == 0:
            raise CommonSemanticsError(
                "MISSING_DEADLINE",
                f"{self._service_name}.{method} requires a positive deadline_monotonic_ns",
            )

        digest = hashlib.sha256(payload).digest()
        if retry_of is not None:
            validate_retry_attempt(
                retry_of,
                service=self._service_name,
                method=method,
                idempotency_key=resolved_key,
                request_digest_sha256=digest,
                side_effecting=semantics.side_effecting,
            )

        if correlation_id is not None:
            resolved_correlation = require_bytes_len(
                "correlation_id", correlation_id, _ID_BYTES
            )
        elif semantics.side_effecting and len(resolved_key) == _ID_BYTES:
            resolved_correlation = resolved_key
        else:
            resolved_correlation = secrets.token_bytes(_ID_BYTES)

        request = _env_pb.ExchangeRequest()
        set_schema_identity(
            request.envelope.schema,
            ENVELOPE_SCHEMA,
            ENVELOPE_MAJOR,
            ENVELOPE_MINOR,
        )
        request.envelope.request_id = secrets.token_bytes(_ID_BYTES)
        request.envelope.service = self._service_name
        request.envelope.method = method
        request.envelope.payload = payload

        req_ctx = request.envelope.request_context
        req_ctx.caller.principal_id = self._principal_id
        req_ctx.caller.application_id = self._application_id
        req_ctx.caller.process_id = self._process_id
        req_ctx.caller.process_generation = self._process_generation
        req_ctx.correlation_id = resolved_correlation
        if resolved_key:
            req_ctx.idempotency_key = resolved_key
        req_ctx.deadline_monotonic_ns = resolved_deadline
        for slot, gen in self._capability_handles:
            cap = req_ctx.capability_handles.add()
            cap.slot = slot
            cap.generation = gen
        if proposal_or_input_digest_sha256:
            req_ctx.proposal_or_input_digest_sha256 = require_bytes_len(
                "proposal_or_input_digest_sha256",
                proposal_or_input_digest_sha256,
                _SHA256_BYTES,
                code="INVALID_PROPOSAL_DIGEST",
            )

        resolved_now = (
            self._now_fn() if now_monotonic_ns is None else now_monotonic_ns
        )
        validate_request_context(request.envelope, semantics, resolved_now)

        try:
            response = await self._exchange(request)
        except IpcError as error:
            raise error.with_call_context(
                service=self._service_name,
                method=method,
                idempotency_key=resolved_key,
                request_digest_sha256=digest,
                side_effecting=semantics.side_effecting,
            ) from None

        env_schema = (
            response.envelope.schema
            if response.envelope.HasField("schema")
            else None
        )
        validate_schema_compatibility(env_schema, ENVELOPE_SCHEMA)
        if (
            response.envelope.service != self._service_name
            or response.envelope.method != method
        ):
            raise IpcError(
                "COMPATIBILITY",
                f"response service/method mismatch for {self._service_name}.{method}",
            )

        resp_ctx = validate_response_context(response.envelope, semantics)
        if resp_ctx.correlation_id != req_ctx.correlation_id:
            raise IpcError(
                "CORRELATION_ID_MISMATCH",
                "SABI response correlation_id does not match request",
            )
        receipts = tuple(bytes(r.receipt_id) for r in resp_ctx.receipts)
        operation = (
            resp_ctx.operation if resp_ctx.HasField("operation") else None
        )
        if resp_ctx.HasField("failure"):
            failure = resp_ctx.failure
            op_copy = None
            if operation is not None:
                op_copy = _env_pb.OperationReference()
                op_copy.CopyFrom(operation)
            raise self._error_type(
                self._service_name,
                int(failure.code),
                int(failure.retry),
                str(failure.safe_message),
                method=method,
                operation=op_copy,
                receipts=receipts,
                idempotency_key=resolved_key,
                request_digest_sha256=digest,
            )

        resp_payload = bytes(response.envelope.payload)
        if not resp_payload:
            raise IpcError(
                "COMPATIBILITY",
                f"{self._service_name}.{method} response payload is missing",
            )
        if len(resp_payload) > self._max_payload_bytes:
            raise IpcError(
                "FRAME_TOO_LARGE",
                f"{self._service_name}.{method} response payload has "
                f"{len(resp_payload)} bytes; maximum is {self._max_payload_bytes}",
            )
        return resp_payload, resp_ctx

    async def _exchange(
        self,
        request: _env_pb.ExchangeRequest,
    ) -> _env_pb.ExchangeResponse:
        if isinstance(self._target, str):
            client = await LocalRpcClient.connect(self._target, self._config)
            try:
                return await client.exchange(request)
            finally:
                await client.close()
        return await self._target.exchange(request)
