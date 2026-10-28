"""High-level Python SDK client for ``nlos.sabi.ChannelControl`` (``v1.0``)."""

from __future__ import annotations

import secrets
from dataclasses import dataclass
from typing import Any

from google.protobuf.message import DecodeError

from .local_rpc import (
    IpcError,
    LocalRpcClient,
    TransportConfig,
    _ensure_gen_python_path,
)

_ensure_gen_python_path()

from nlos.sabi.v1 import channel_control_pb2 as _cc_pb  # noqa: E402
from nlos.sabi.v1 import envelope_pb2 as _env_pb  # noqa: E402
from nlos.sabi.v1.envelope_pb2 import (  # noqa: E402
    RETRY_DIRECTIVE_DO_NOT_RETRY,
    RETRY_DIRECTIVE_QUERY_OPERATION_OR_RETRY_SAME_IDEMPOTENCY_KEY,
    RETRY_DIRECTIVE_RETRY_SAME_IDEMPOTENCY_KEY,
    SABI_ERROR_CODE_ABI_VERSION,
    SABI_ERROR_CODE_BUDGET,
    SABI_ERROR_CODE_CANCELLED,
    SABI_ERROR_CODE_CONFLICT,
    SABI_ERROR_CODE_DEADLINE,
    SABI_ERROR_CODE_DRIVER,
    SABI_ERROR_CODE_DURABILITY,
    SABI_ERROR_CODE_EFFECT_UNKNOWN,
    SABI_ERROR_CODE_FENCED,
    SABI_ERROR_CODE_HOST_LOST,
    SABI_ERROR_CODE_INVALID_ARGUMENT,
    SABI_ERROR_CODE_NOT_FOUND,
    SABI_ERROR_CODE_NOT_SUPPORTED,
    SABI_ERROR_CODE_PARTIAL,
    SABI_ERROR_CODE_QUOTA,
    SABI_ERROR_CODE_RETRY,
    SABI_ERROR_CODE_RIGHTS,
    SABI_ERROR_CODE_STATE,
    SABI_ERROR_CODE_UNCERTAIN,
    ExchangeRequest,
    RetryDirective,
    SabiErrorCode,
)

from .common import (  # noqa: E402
    MethodSemantics,
    validate_request_context,
    validate_response_context,
)
from .service_directory import ServiceRequirement  # noqa: E402

ENVELOPE_SCHEMA = "nlos.sabi.Envelope"
ENVELOPE_MAJOR = 1
ENVELOPE_MINOR = 1

CHANNEL_CONTROL_SCHEMA = "nlos.sabi.ChannelControl"
CHANNEL_CONTROL_MAJOR = 1
CHANNEL_CONTROL_MINOR = 0
CHANNEL_CONTROL_SERVICE = "channel"
MAX_CHANNEL_CONTROL_PAYLOAD_BYTES = 64 * 1024

CREATE_CHANNEL_METHOD = "create_channel"
ROTATE_CHANNEL_METHOD = "rotate_channel"
ENQUEUE_METHOD = "enqueue"
RECEIVE_METHOD = "receive"
ACK_METHOD = "ack"
COMPACT_METHOD = "compact"
INSPECT_CHANNEL_METHOD = "inspect_channel"
INSPECT_QUEUE_METHOD = "inspect_queue"
REGISTER_QUEUE_CONSUMPTION_METHOD = "register_queue_consumption"

DEFAULT_CAPABILITY_SLOT = 9
DEFAULT_CAPABILITY_GENERATION = 1
DEFAULT_PRINCIPAL_ID = b"\x31" * 16
DEFAULT_APPLICATION_ID = b"\x32" * 16
DEFAULT_PROCESS_ID = b"\x33" * 16
DEFAULT_PROCESS_GENERATION = 1

QUERY_SEMANTICS = MethodSemantics(side_effecting=False, long_running=False)
MUTATION_SEMANTICS = MethodSemantics(side_effecting=True, long_running=False)


@dataclass(frozen=True, slots=True)
class ChannelRecord:
    """Durable Channel head/generation record returned by ``ChannelControl``."""

    channel_id: bytes
    generation: int
    fencing_token: bytes
    capacity_bytes: int
    policy_digest: bytes
    idempotency_key: bytes
    created_at_ms: int
    updated_at_ms: int = 0
    replayed: bool = False


@dataclass(frozen=True, slots=True)
class QueueEntryRecord:
    """Immutable durable queue entry returned by ``ChannelControl``."""

    channel_id: bytes
    generation: int
    sequence: int
    fencing_token: bytes
    payload: bytes
    payload_len: int
    idempotency_key: bytes
    enqueued_at_ms: int
    acked_at_ms: int | None = None
    replayed: bool = False
    binding: bytes = b""
    fiber_generation: int = 0

    @property
    def payload_bytes(self) -> int:
        return self.payload_len

    @property
    def producer_binding(self) -> bytes:
        return self.binding

    @property
    def producer_fiber_generation(self) -> int:
        return self.fiber_generation


@dataclass(frozen=True, slots=True)
class QueueConsumptionRecord:
    """Immutable durable queue-consumption registration record."""

    channel_id: bytes
    generation: int
    sequence: int
    consumer_fiber_id: bytes
    consumer_fiber_generation: int
    idempotency_key: bytes
    registered_at_ms: int
    replayed: bool = False
    registration_id: bytes = b""

    @property
    def binding(self) -> bytes:
        return self.consumer_fiber_id

    @property
    def fiber_generation(self) -> int:
        return self.consumer_fiber_generation


@dataclass(frozen=True, slots=True)
class QueueState:
    """Verified queue state of a Channel."""

    channel_id: bytes
    generation: int
    capacity_bytes: int
    outstanding_entries: int
    outstanding_bytes: int
    unacked_entries: int
    acked_uncompacted_entries: int
    next_sequence: int
    consume_high_water: int = 0
    trim_high_water: int = 0
    backlog_bytes: int = 0
    retained_bytes: int = 0
    max_sequence: int = 0


@dataclass(frozen=True, slots=True)
class AckResult:
    """Outcome of a ``ChannelControl.ack`` mutation."""

    acked: bool
    channel_id: bytes
    generation: int
    sequence: int
    acked_at_ms: int
    consume_high_water: int = 0
    replayed: bool = False


@dataclass(frozen=True, slots=True)
class CompactResult:
    """Outcome of a ``ChannelControl.compact`` mutation."""

    removed_entries: int
    reclaimed_bytes: int
    queue_state: QueueState
    channel_id: bytes = b""
    trim_high_water: int = 0
    replayed: bool = False


class ChannelControlError(Exception):
    """Typed SABI failure returned by the ``ChannelControl`` service."""

    def __init__(
        self,
        code: int,
        retry: int,
        detail: str,
        method: str = "",
    ) -> None:
        try:
            code_name = SabiErrorCode.Name(code)
        except ValueError:
            code_name = str(code)
        try:
            retry_name = RetryDirective.Name(retry)
        except ValueError:
            retry_name = str(retry)
        super().__init__(
            f"ChannelControl.{method} failed: code={code_name} ({code}), "
            f"retry={retry_name} ({retry}), detail={detail!r}"
        )
        self.code = code
        self.retry = retry
        self.detail = detail
        self.method = method

    @property
    def safe_message(self) -> str:
        return self.detail


class ChannelClient:
    """High-level SABI client for ``nlos.sabi.ChannelControl`` (``v1.0``)."""

    def __init__(
        self,
        endpoint_or_rpc: str | LocalRpcClient,
        config: TransportConfig | None = None,
        *,
        principal_id: bytes = DEFAULT_PRINCIPAL_ID,
        application_id: bytes = DEFAULT_APPLICATION_ID,
        process_id: bytes = DEFAULT_PROCESS_ID,
        process_generation: int = DEFAULT_PROCESS_GENERATION,
        capability_slot: int = DEFAULT_CAPABILITY_SLOT,
        capability_generation: int = DEFAULT_CAPABILITY_GENERATION,
    ) -> None:
        if isinstance(endpoint_or_rpc, str) and not endpoint_or_rpc:
            raise IpcError("INVALID_CONFIG", "IPC endpoint must not be empty")
        self._target = endpoint_or_rpc
        self._config = config or TransportConfig()
        self._principal_id = principal_id
        self._application_id = application_id
        self._process_id = process_id
        self._process_generation = process_generation
        self._capability_slot = capability_slot
        self._capability_generation = capability_generation
        self._fencing_tokens: dict[tuple[bytes, int], bytes] = {}
        self._channel_generations: dict[bytes, int] = {}

    @classmethod
    def from_endpoint(
        cls,
        endpoint: str,
        config: TransportConfig | None = None,
        **kwargs: Any,
    ) -> ChannelClient:
        """Construct a ``ChannelClient`` that opens a fresh local connection per RPC."""

        return cls(endpoint, config=config, **kwargs)

    @staticmethod
    def service_requirement() -> ServiceRequirement:
        """Return the ``ServiceDirectory`` requirement for ``nlos.sabi.ChannelControl``."""

        return ServiceRequirement(
            service=CHANNEL_CONTROL_SERVICE,
            schema_name=CHANNEL_CONTROL_SCHEMA,
            major=CHANNEL_CONTROL_MAJOR,
            minimum_minor=CHANNEL_CONTROL_MINOR,
        )

    async def create_channel(
        self,
        capacity_bytes: int,
        policy_digest: bytes = b"\x00" * 32,
        *,
        idempotency_key: bytes | None = None,
        created_at_ms: int = 0,
    ) -> ChannelRecord:
        """Create or idempotently replay a durable Channel."""

        key = _resolve_idempotency_key(idempotency_key)
        wire_req = _cc_pb.CreateChannelRequestWire()
        _set_channel_schema(wire_req.schema)
        wire_req.capacity_bytes = capacity_bytes
        wire_req.policy_digest = policy_digest
        wire_req.idempotency_key = key
        wire_req.created_at_ms = created_at_ms

        resp_payload = await self._call(
            CREATE_CHANNEL_METHOD,
            wire_req.SerializeToString(deterministic=True),
            MUTATION_SEMANTICS,
            idempotency_key=key,
        )
        wire_res = _parse_result(
            _cc_pb.CreateChannelResultWire,
            resp_payload,
            CREATE_CHANNEL_METHOD,
        )
        if not wire_res.HasField("record"):
            raise IpcError(
                "COMPATIBILITY",
                "CreateChannelResultWire is missing record",
            )
        record = _decode_channel_record(wire_res.record, replayed=bool(wire_res.replayed))
        self._remember_channel(record)
        return record

    async def rotate_channel(
        self,
        channel_id: bytes,
        expected_generation: int,
        expected_fencing_token_or_capacity: bytes | int = b"",
        new_policy_digest: bytes = b"",
        *,
        expected_fencing_token: bytes | None = None,
        idempotency_key: bytes | None = None,
        rotated_at_ms: int = 0,
    ) -> ChannelRecord:
        """Rotate a Channel generation with fencing CAS."""

        _ = new_policy_digest
        resolved_token: bytes
        if expected_fencing_token is not None:
            resolved_token = expected_fencing_token
        elif isinstance(expected_fencing_token_or_capacity, (bytes, bytearray)) and len(
            expected_fencing_token_or_capacity
        ) > 0:
            resolved_token = bytes(expected_fencing_token_or_capacity)
        else:
            cached = self._fencing_tokens.get((bytes(channel_id), expected_generation))
            if cached is not None:
                resolved_token = cached
            elif isinstance(self._target, str):
                inspected = await self.inspect_channel(channel_id)
                resolved_token = inspected.fencing_token
            else:
                resolved_token = b"\x00" * 32

        key = _resolve_idempotency_key(idempotency_key)
        wire_req = _cc_pb.RotateChannelRequestWire()
        _set_channel_schema(wire_req.schema)
        wire_req.channel_id = channel_id
        wire_req.expected_generation = expected_generation
        wire_req.expected_fencing_token = resolved_token
        wire_req.idempotency_key = key
        wire_req.rotated_at_ms = rotated_at_ms

        resp_payload = await self._call(
            ROTATE_CHANNEL_METHOD,
            wire_req.SerializeToString(deterministic=True),
            MUTATION_SEMANTICS,
            idempotency_key=key,
        )
        wire_res = _parse_result(
            _cc_pb.RotateChannelResultWire,
            resp_payload,
            ROTATE_CHANNEL_METHOD,
        )
        if not wire_res.HasField("record"):
            raise IpcError(
                "COMPATIBILITY",
                "RotateChannelResultWire is missing record",
            )
        record = _decode_channel_record(wire_res.record, replayed=bool(wire_res.replayed))
        self._remember_channel(record)
        return record

    async def enqueue(
        self,
        channel_id: bytes,
        expected_generation: int,
        expected_fencing_token: bytes,
        payload: bytes,
        *,
        idempotency_key: bytes | None = None,
        enqueued_at_ms: int = 0,
    ) -> QueueEntryRecord:
        """Append one unregistered payload to the Channel queue."""

        return await self._enqueue_impl(
            channel_id=channel_id,
            expected_generation=expected_generation,
            expected_fencing_token=expected_fencing_token,
            payload=payload,
            producer_binding=b"",
            producer_fiber_generation=0,
            idempotency_key=idempotency_key,
            enqueued_at_ms=enqueued_at_ms,
        )

    async def enqueue_registered(
        self,
        channel_id: bytes,
        expected_generation: int,
        expected_fencing_token: bytes,
        payload: bytes,
        producer_binding: bytes,
        producer_fiber_generation: int,
        *,
        idempotency_key: bytes | None = None,
        enqueued_at_ms: int = 0,
    ) -> QueueEntryRecord:
        """Append one payload bound to a producing ExecutionFiber registration."""

        return await self._enqueue_impl(
            channel_id=channel_id,
            expected_generation=expected_generation,
            expected_fencing_token=expected_fencing_token,
            payload=payload,
            producer_binding=producer_binding,
            producer_fiber_generation=producer_fiber_generation,
            idempotency_key=idempotency_key,
            enqueued_at_ms=enqueued_at_ms,
        )

    async def _enqueue_impl(
        self,
        *,
        channel_id: bytes,
        expected_generation: int,
        expected_fencing_token: bytes,
        payload: bytes,
        producer_binding: bytes,
        producer_fiber_generation: int,
        idempotency_key: bytes | None,
        enqueued_at_ms: int,
    ) -> QueueEntryRecord:
        key = _resolve_idempotency_key(idempotency_key)
        wire_req = _cc_pb.EnqueueChannelRequestWire()
        _set_channel_schema(wire_req.schema)
        wire_req.channel_id = channel_id
        wire_req.expected_generation = expected_generation
        wire_req.expected_fencing_token = expected_fencing_token
        wire_req.payload = payload
        wire_req.idempotency_key = key
        wire_req.enqueued_at_ms = enqueued_at_ms
        wire_req.producer_binding = producer_binding
        wire_req.producer_fiber_generation = producer_fiber_generation

        resp_payload = await self._call(
            ENQUEUE_METHOD,
            wire_req.SerializeToString(deterministic=True),
            MUTATION_SEMANTICS,
            idempotency_key=key,
        )
        wire_res = _parse_result(
            _cc_pb.EnqueueChannelResultWire,
            resp_payload,
            ENQUEUE_METHOD,
        )
        if not wire_res.HasField("record"):
            raise IpcError(
                "COMPATIBILITY",
                "EnqueueChannelResultWire is missing record",
            )
        return _decode_queue_entry_record(
            wire_res.record,
            replayed=bool(wire_res.replayed),
        )

    async def receive(
        self,
        channel_id: bytes,
        limit: int = 16,
    ) -> list[QueueEntryRecord]:
        """Read the unacknowledged delivery window in sequence order up to ``limit``."""

        wire_req = _cc_pb.ReceiveChannelRequestWire()
        _set_channel_schema(wire_req.schema)
        wire_req.channel_id = channel_id
        wire_req.limit = limit

        resp_payload = await self._call(
            RECEIVE_METHOD,
            wire_req.SerializeToString(deterministic=True),
            QUERY_SEMANTICS,
        )
        wire_res = _parse_result(
            _cc_pb.ReceiveChannelResultWire,
            resp_payload,
            RECEIVE_METHOD,
        )
        return [
            _decode_queue_entry_record(entry, replayed=False)
            for entry in wire_res.entries
        ]

    async def register_queue_consumption(
        self,
        channel_id: bytes,
        expected_generation_or_sequence: int,
        sequence_or_binding: int | bytes,
        consumer_fiber_id_or_generation: bytes | int,
        consumer_fiber_generation: int | None = None,
        *,
        idempotency_key: bytes | None = None,
        registered_at_ms: int = 0,
    ) -> QueueConsumptionRecord:
        """Register a durable consuming ExecutionFiber before acknowledging."""

        if isinstance(sequence_or_binding, (bytes, bytearray)):
            expected_generation = self._channel_generations.get(bytes(channel_id), 1)
            sequence = int(expected_generation_or_sequence)
            binding = bytes(sequence_or_binding)
            fiber_generation = int(consumer_fiber_id_or_generation)
        else:
            if not isinstance(consumer_fiber_id_or_generation, (bytes, bytearray)):
                raise IpcError(
                    "INVALID_CONFIG",
                    "consumer_fiber_id must be bytes",
                )
            if consumer_fiber_generation is None:
                raise IpcError(
                    "INVALID_CONFIG",
                    "consumer_fiber_generation is required",
                )
            expected_generation = int(expected_generation_or_sequence)
            sequence = int(sequence_or_binding)
            binding = bytes(consumer_fiber_id_or_generation)
            fiber_generation = int(consumer_fiber_generation)

        key = _resolve_idempotency_key(idempotency_key)
        wire_req = _cc_pb.RegisterQueueConsumptionRequestWire()
        _set_channel_schema(wire_req.schema)
        wire_req.channel_id = channel_id
        wire_req.sequence = sequence
        wire_req.binding = binding
        wire_req.fiber_generation = fiber_generation
        wire_req.idempotency_key = key
        wire_req.registered_at_ms = registered_at_ms

        resp_payload = await self._call(
            REGISTER_QUEUE_CONSUMPTION_METHOD,
            wire_req.SerializeToString(deterministic=True),
            MUTATION_SEMANTICS,
            idempotency_key=key,
        )
        wire_res = _parse_result(
            _cc_pb.RegisterQueueConsumptionResultWire,
            resp_payload,
            REGISTER_QUEUE_CONSUMPTION_METHOD,
        )
        if not wire_res.HasField("record"):
            raise IpcError(
                "COMPATIBILITY",
                "RegisterQueueConsumptionResultWire is missing record",
            )
        rec = wire_res.record
        return QueueConsumptionRecord(
            channel_id=bytes(rec.channel_id),
            generation=expected_generation,
            sequence=int(rec.sequence),
            consumer_fiber_id=bytes(rec.binding),
            consumer_fiber_generation=int(rec.fiber_generation),
            idempotency_key=bytes(rec.idempotency_key),
            registered_at_ms=int(rec.registered_at_ms),
            replayed=bool(wire_res.replayed),
            registration_id=bytes(rec.registration_id),
        )

    async def ack(
        self,
        channel_id: bytes,
        expected_generation_or_sequence: int,
        sequence: int | None = None,
        *,
        acked_at_ms: int = 0,
        idempotency_key: bytes | None = None,
    ) -> AckResult:
        """Advance the Channel consume high-water mark."""

        if sequence is None:
            expected_generation = self._channel_generations.get(bytes(channel_id), 1)
            up_to_sequence = int(expected_generation_or_sequence)
        else:
            expected_generation = int(expected_generation_or_sequence)
            up_to_sequence = int(sequence)

        key = _resolve_idempotency_key(idempotency_key)
        wire_req = _cc_pb.AckChannelRequestWire()
        _set_channel_schema(wire_req.schema)
        wire_req.channel_id = channel_id
        wire_req.up_to_sequence = up_to_sequence
        wire_req.acked_at_ms = acked_at_ms
        wire_req.idempotency_key = key

        resp_payload = await self._call(
            ACK_METHOD,
            wire_req.SerializeToString(deterministic=True),
            MUTATION_SEMANTICS,
            idempotency_key=key,
        )
        wire_res = _parse_result(
            _cc_pb.AckChannelResultWire,
            resp_payload,
            ACK_METHOD,
        )
        replayed = bool(wire_res.replayed)
        high_water = int(wire_res.consume_high_water)
        return AckResult(
            acked=not replayed,
            channel_id=bytes(wire_res.channel_id),
            generation=expected_generation,
            sequence=high_water,
            acked_at_ms=int(wire_res.acked_at_ms),
            consume_high_water=high_water,
            replayed=replayed,
        )

    async def compact(
        self,
        channel_id: bytes,
        expected_generation: int = 1,
        *,
        trim_to_sequence: int | None = None,
        idempotency_key: bytes | None = None,
    ) -> CompactResult:
        """Trim acknowledged queue entries up to the consume high-water mark."""

        before_state: QueueState | None = None
        if isinstance(self._target, str):
            before_state = await self.inspect_queue(channel_id)
            effective_trim = (
                before_state.consume_high_water
                if trim_to_sequence is None
                else int(trim_to_sequence)
            )
        else:
            effective_trim = (
                (1 << 62) if trim_to_sequence is None else int(trim_to_sequence)
            )

        key = _resolve_idempotency_key(idempotency_key)
        wire_req = _cc_pb.CompactChannelRequestWire()
        _set_channel_schema(wire_req.schema)
        wire_req.channel_id = channel_id
        wire_req.trim_to_sequence = effective_trim
        wire_req.idempotency_key = key

        resp_payload = await self._call(
            COMPACT_METHOD,
            wire_req.SerializeToString(deterministic=True),
            MUTATION_SEMANTICS,
            idempotency_key=key,
        )
        wire_res = _parse_result(
            _cc_pb.CompactChannelResultWire,
            resp_payload,
            COMPACT_METHOD,
        )
        replayed = bool(wire_res.replayed)
        trim_hw = int(wire_res.trim_high_water)

        if isinstance(self._target, str) and before_state is not None:
            after_state = await self.inspect_queue(channel_id)
            removed_entries = (
                0 if replayed else max(0, trim_hw - before_state.trim_high_water)
            )
            reclaimed_bytes = (
                0
                if replayed
                else max(0, before_state.retained_bytes - after_state.retained_bytes)
            )
        else:
            removed_entries = 0
            reclaimed_bytes = 0
            after_state = QueueState(
                channel_id=bytes(wire_res.channel_id),
                generation=expected_generation,
                capacity_bytes=0,
                outstanding_entries=0,
                outstanding_bytes=0,
                unacked_entries=0,
                acked_uncompacted_entries=0,
                next_sequence=trim_hw + 1,
                consume_high_water=trim_hw,
                trim_high_water=trim_hw,
                backlog_bytes=0,
                retained_bytes=0,
                max_sequence=trim_hw,
            )

        return CompactResult(
            removed_entries=removed_entries,
            reclaimed_bytes=reclaimed_bytes,
            queue_state=after_state,
            channel_id=bytes(wire_res.channel_id),
            trim_high_water=trim_hw,
            replayed=replayed,
        )

    async def inspect_channel(
        self,
        channel_id: bytes,
    ) -> ChannelRecord:
        """Inspect the current durable Channel generation record."""

        wire_req = _cc_pb.InspectChannelRequestWire()
        _set_channel_schema(wire_req.schema)
        wire_req.channel_id = channel_id

        resp_payload = await self._call(
            INSPECT_CHANNEL_METHOD,
            wire_req.SerializeToString(deterministic=True),
            QUERY_SEMANTICS,
        )
        wire_res = _parse_result(
            _cc_pb.InspectChannelResultWire,
            resp_payload,
            INSPECT_CHANNEL_METHOD,
        )
        if not wire_res.HasField("record"):
            raise IpcError(
                "COMPATIBILITY",
                "InspectChannelResultWire is missing record",
            )
        record = _decode_channel_record(wire_res.record, replayed=False)
        self._remember_channel(record)
        return record

    async def inspect_queue(
        self,
        channel_id: bytes,
    ) -> QueueState:
        """Inspect the verified queue state of a Channel."""

        wire_req = _cc_pb.InspectQueueRequestWire()
        _set_channel_schema(wire_req.schema)
        wire_req.channel_id = channel_id

        resp_payload = await self._call(
            INSPECT_QUEUE_METHOD,
            wire_req.SerializeToString(deterministic=True),
            QUERY_SEMANTICS,
        )
        wire_res = _parse_result(
            _cc_pb.InspectQueueResultWire,
            resp_payload,
            INSPECT_QUEUE_METHOD,
        )
        if not wire_res.HasField("state"):
            raise IpcError(
                "COMPATIBILITY",
                "InspectQueueResultWire is missing state",
            )
        generation = self._channel_generations.get(bytes(channel_id), 1)
        return _decode_queue_state(wire_res.state, generation=generation)

    def _remember_channel(self, record: ChannelRecord) -> None:
        self._fencing_tokens[(record.channel_id, record.generation)] = (
            record.fencing_token
        )
        self._channel_generations[record.channel_id] = record.generation

    async def _call(
        self,
        method: str,
        payload: bytes,
        semantics: MethodSemantics,
        *,
        idempotency_key: bytes = b"",
        correlation_id: bytes | None = None,
    ) -> bytes:
        if len(payload) > MAX_CHANNEL_CONTROL_PAYLOAD_BYTES:
            raise IpcError(
                "FRAME_TOO_LARGE",
                f"ChannelControl.{method} request payload exceeds 64 KiB",
            )

        resolved_correlation = (
            correlation_id
            if correlation_id is not None
            else (
                idempotency_key
                if semantics.side_effecting and len(idempotency_key) == 16
                else secrets.token_bytes(16)
            )
        )

        request = ExchangeRequest()
        request.envelope.schema.name = ENVELOPE_SCHEMA
        request.envelope.schema.major = ENVELOPE_MAJOR
        request.envelope.schema.minor = ENVELOPE_MINOR
        request.envelope.request_id = secrets.token_bytes(16)
        request.envelope.service = CHANNEL_CONTROL_SERVICE
        request.envelope.method = method
        request.envelope.payload = payload

        req_ctx = request.envelope.request_context
        req_ctx.caller.principal_id = self._principal_id
        req_ctx.caller.application_id = self._application_id
        req_ctx.caller.process_id = self._process_id
        req_ctx.caller.process_generation = self._process_generation
        req_ctx.correlation_id = resolved_correlation
        if idempotency_key:
            req_ctx.idempotency_key = idempotency_key
        req_ctx.deadline_monotonic_ns = 0
        cap = req_ctx.capability_handles.add()
        cap.slot = self._capability_slot
        cap.generation = self._capability_generation

        validate_request_context(request.envelope, semantics, 0)

        response = await self._exchange(request)
        resp_ctx = validate_response_context(response.envelope, semantics)
        if resp_ctx.correlation_id != req_ctx.correlation_id:
            raise IpcError(
                "CORRELATION_ID_MISMATCH",
                "SABI response correlation_id does not match request",
            )
        if resp_ctx.HasField("failure"):
            failure = resp_ctx.failure
            raise ChannelControlError(
                code=int(failure.code),
                retry=int(failure.retry),
                detail=str(failure.safe_message),
                method=method,
            )
        resp_payload = bytes(response.envelope.payload)
        if not resp_payload or len(resp_payload) > MAX_CHANNEL_CONTROL_PAYLOAD_BYTES:
            raise IpcError(
                "COMPATIBILITY",
                f"ChannelControl.{method} response payload is missing or oversized",
            )
        return resp_payload

    async def _exchange(
        self,
        request: ExchangeRequest,
    ) -> _env_pb.ExchangeResponse:
        if isinstance(self._target, str):
            client = await LocalRpcClient.connect(self._target, self._config)
            try:
                return await client.exchange(request)
            finally:
                await client.close()
        return await self._target.exchange(request)


def _resolve_idempotency_key(idempotency_key: bytes | None) -> bytes:
    if idempotency_key is None:
        return secrets.token_bytes(16)
    return bytes(idempotency_key)


def _set_channel_schema(identity: _env_pb.SchemaIdentity) -> None:
    identity.name = CHANNEL_CONTROL_SCHEMA
    identity.major = CHANNEL_CONTROL_MAJOR
    identity.minor = CHANNEL_CONTROL_MINOR


def _require_channel_schema(message: Any, method: str) -> None:
    if (
        not message.HasField("schema")
        or message.schema.name != CHANNEL_CONTROL_SCHEMA
        or message.schema.major != CHANNEL_CONTROL_MAJOR
        or message.schema.critical_extension_ids
    ):
        raise IpcError(
            "COMPATIBILITY",
            f"incompatible ChannelControl.{method} response schema identity",
        )


def _parse_result(message_cls: Any, payload: bytes, method: str) -> Any:
    try:
        parsed = message_cls.FromString(payload)
    except DecodeError as error:
        raise IpcError(
            "COMPATIBILITY",
            f"malformed ChannelControl.{method} response payload",
        ) from error
    _require_channel_schema(parsed, method)
    return parsed


def _decode_channel_record(
    wire: _cc_pb.ChannelRecordWire,
    *,
    replayed: bool,
) -> ChannelRecord:
    created_ms = int(wire.created_at_ms)
    return ChannelRecord(
        channel_id=bytes(wire.channel_id),
        generation=int(wire.generation),
        fencing_token=bytes(wire.fencing_token),
        capacity_bytes=int(wire.capacity_bytes),
        policy_digest=bytes(wire.policy_digest),
        idempotency_key=bytes(wire.idempotency_key),
        created_at_ms=created_ms,
        updated_at_ms=created_ms,
        replayed=replayed,
    )


def _decode_queue_entry_record(
    wire: _cc_pb.QueueEntryRecordWire,
    *,
    replayed: bool,
) -> QueueEntryRecord:
    return QueueEntryRecord(
        channel_id=bytes(wire.channel_id),
        generation=int(wire.generation),
        sequence=int(wire.sequence),
        fencing_token=bytes(wire.fencing_token),
        payload=bytes(wire.payload),
        payload_len=int(wire.payload_bytes),
        idempotency_key=bytes(wire.idempotency_key),
        enqueued_at_ms=int(wire.enqueued_at_ms),
        acked_at_ms=None,
        replayed=replayed,
        binding=bytes(wire.binding),
        fiber_generation=int(wire.fiber_generation),
    )


def _decode_queue_state(
    wire: _cc_pb.QueueStateWire,
    *,
    generation: int,
) -> QueueState:
    max_seq = int(wire.max_sequence)
    consume_hw = int(wire.consume_high_water)
    trim_hw = int(wire.trim_high_water)
    backlog = int(wire.backlog_bytes)
    retained = int(wire.retained_bytes)
    unacked_entries = max(0, max_seq - consume_hw)
    acked_uncompacted_entries = max(0, consume_hw - trim_hw)
    outstanding_entries = max(0, max_seq - trim_hw)
    return QueueState(
        channel_id=bytes(wire.channel_id),
        generation=generation,
        capacity_bytes=int(wire.capacity_bytes),
        outstanding_entries=outstanding_entries,
        outstanding_bytes=backlog,
        unacked_entries=unacked_entries,
        acked_uncompacted_entries=acked_uncompacted_entries,
        next_sequence=max_seq + 1,
        consume_high_water=consume_hw,
        trim_high_water=trim_hw,
        backlog_bytes=backlog,
        retained_bytes=retained,
        max_sequence=max_seq,
    )


__all__ = [
    "ACK_METHOD",
    "AckResult",
    "CHANNEL_CONTROL_MAJOR",
    "CHANNEL_CONTROL_MINOR",
    "CHANNEL_CONTROL_SCHEMA",
    "CHANNEL_CONTROL_SERVICE",
    "COMPACT_METHOD",
    "CREATE_CHANNEL_METHOD",
    "ChannelClient",
    "ChannelControlError",
    "ChannelRecord",
    "CompactResult",
    "DEFAULT_APPLICATION_ID",
    "DEFAULT_CAPABILITY_GENERATION",
    "DEFAULT_CAPABILITY_SLOT",
    "DEFAULT_PRINCIPAL_ID",
    "DEFAULT_PROCESS_GENERATION",
    "DEFAULT_PROCESS_ID",
    "ENQUEUE_METHOD",
    "ENVELOPE_MAJOR",
    "ENVELOPE_MINOR",
    "ENVELOPE_SCHEMA",
    "INSPECT_CHANNEL_METHOD",
    "INSPECT_QUEUE_METHOD",
    "MAX_CHANNEL_CONTROL_PAYLOAD_BYTES",
    "MUTATION_SEMANTICS",
    "QUERY_SEMANTICS",
    "QueueConsumptionRecord",
    "QueueEntryRecord",
    "QueueState",
    "RECEIVE_METHOD",
    "REGISTER_QUEUE_CONSUMPTION_METHOD",
    "RETRY_DIRECTIVE_DO_NOT_RETRY",
    "RETRY_DIRECTIVE_QUERY_OPERATION_OR_RETRY_SAME_IDEMPOTENCY_KEY",
    "RETRY_DIRECTIVE_RETRY_SAME_IDEMPOTENCY_KEY",
    "ROTATE_CHANNEL_METHOD",
    "SABI_ERROR_CODE_ABI_VERSION",
    "SABI_ERROR_CODE_BUDGET",
    "SABI_ERROR_CODE_CANCELLED",
    "SABI_ERROR_CODE_CONFLICT",
    "SABI_ERROR_CODE_DEADLINE",
    "SABI_ERROR_CODE_DRIVER",
    "SABI_ERROR_CODE_DURABILITY",
    "SABI_ERROR_CODE_EFFECT_UNKNOWN",
    "SABI_ERROR_CODE_FENCED",
    "SABI_ERROR_CODE_HOST_LOST",
    "SABI_ERROR_CODE_INVALID_ARGUMENT",
    "SABI_ERROR_CODE_NOT_FOUND",
    "SABI_ERROR_CODE_NOT_SUPPORTED",
    "SABI_ERROR_CODE_PARTIAL",
    "SABI_ERROR_CODE_QUOTA",
    "SABI_ERROR_CODE_RETRY",
    "SABI_ERROR_CODE_RIGHTS",
    "SABI_ERROR_CODE_STATE",
    "SABI_ERROR_CODE_UNCERTAIN",
]
