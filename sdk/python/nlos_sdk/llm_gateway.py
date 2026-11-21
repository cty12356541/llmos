"""Thin SABI v1.0 client for ``nlos.sabi.LlmGateway``."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Any, Sequence

from ._client_base import (
    DEFAULT_PROCESS_GENERATION,
    MAX_CONTROL_PAYLOAD_BYTES,
    SabiServiceClientBase,
    SabiServiceError,
    default_wall_ms,
    require_bounded_str,
    require_bytes_len,
    require_uint,
)
from .common import CommonSemanticsError, MethodSemantics, _ensure_gen_python_path
from .local_rpc import IpcError

_ensure_gen_python_path()

from nlos.sabi.v1 import llm_gateway_pb2 as _llm_pb  # noqa: E402

LLM_GATEWAY_SCHEMA = "nlos.sabi.LlmGateway"
LLM_GATEWAY_MAJOR = 1
LLM_GATEWAY_MINOR = 0
LLM_GATEWAY_SERVICE = "llm_gateway"
MAX_LLM_GATEWAY_FRAME_BYTES = MAX_CONTROL_PAYLOAD_BYTES

LLM_CHAT_METHOD = "llm_chat"
QUERY_BUDGET_METHOD = "query_budget"
ATTENUATE_BUDGET_METHOD = "attenuate_budget"
RECORD_EXTERNAL_USAGE_METHOD = "record_external_usage"

_QUERY_SEMANTICS = MethodSemantics(side_effecting=False, long_running=False)
_MUTATION_SEMANTICS = MethodSemantics(side_effecting=True, long_running=False)
_LONG_RUNNING_MUTATION_SEMANTICS = MethodSemantics(
    side_effecting=True, long_running=True
)

_ID_BYTES = 16
_U32_MAX = (1 << 32) - 1


@dataclass(frozen=True, slots=True)
class LlmChatTurn:
    """Single prompt/response turn sent to ``LlmGateway.llm_chat``."""

    role: str
    content: str


@dataclass(frozen=True, slots=True)
class LlmUsage:
    """Normalized token and credit usage counters."""

    api_calls: int = 0
    prompt_tokens: int = 0
    completion_tokens: int = 0
    total_tokens: int = 0
    cost_microcredits: int = 0


@dataclass(frozen=True, slots=True)
class LlmChatRequest:
    """Typed request parameters for ``LlmGateway.llm_chat``."""

    model: str
    turns: tuple[LlmChatTurn, ...]
    max_output_tokens: int = 0
    temperature_milli: int = 0
    session_hint: str = ""


@dataclass(frozen=True, slots=True)
class LlmChatResult:
    """Typed response payload from ``LlmGateway.llm_chat``."""

    model: str
    content_utf8: str
    reasoning_utf8: str = ""
    usage: LlmUsage = LlmUsage()

    def __repr__(self) -> str:
        return (
            f"LlmChatResult(model={self.model!r}, "
            f"content_bytes={len(self.content_utf8.encode('utf-8'))}, "
            f"reasoning_bytes={len(self.reasoning_utf8.encode('utf-8'))}, "
            f"usage={self.usage!r})"
        )


@dataclass(frozen=True, slots=True)
class QueryBudgetRequest:
    """Typed request marker for ``LlmGateway.query_budget``."""


@dataclass(frozen=True, slots=True)
class QueryBudgetResult:
    """Remaining caller budget snapshot returned by ``LlmGateway.query_budget``."""

    limit: LlmUsage
    consumed: LlmUsage
    remaining: LlmUsage
    external_advisory: LlmUsage = LlmUsage()


@dataclass(frozen=True, slots=True)
class AttenuateBudgetRequest:
    """Typed request parameters for ``LlmGateway.attenuate_budget``."""

    child_limit: LlmUsage
    valid_until_ms: int = 0


@dataclass(frozen=True, slots=True)
class AttenuateBudgetResult:
    """Granted sub-budget returned by ``LlmGateway.attenuate_budget``."""

    granted_limit: LlmUsage
    valid_until_ms: int


@dataclass(frozen=True, slots=True)
class RecordExternalUsageRequest:
    """Typed request parameters for ``LlmGateway.record_external_usage``."""

    provider: str
    model: str
    usage: LlmUsage
    recorded_at_ms: int | None = None


@dataclass(frozen=True, slots=True)
class RecordExternalUsageResult:
    """Outcome of ``LlmGateway.record_external_usage``."""

    replayed: bool = False


class LlmGatewayError(SabiServiceError):
    """Typed SABI failure returned by ``LlmGateway``."""


def _normalize_turns(
    turns: Sequence[LlmChatTurn | tuple[str, str]],
) -> tuple[LlmChatTurn, ...]:
    if isinstance(turns, (str, bytes, bytearray)):
        raise IpcError("INVALID_ARGUMENT", "turns must be a sequence of LlmChatTurn")
    normalized: list[LlmChatTurn] = []
    for index, item in enumerate(turns):
        if isinstance(item, LlmChatTurn):
            role, content = item.role, item.content
        elif isinstance(item, tuple) and len(item) == 2:
            role, content = item
        else:
            raise IpcError(
                "INVALID_ARGUMENT",
                f"turns[{index}] must be LlmChatTurn or (role, content)",
            )
        normalized.append(
            LlmChatTurn(
                role=require_bounded_str(
                    f"turns[{index}].role", role, max_bytes=128
                ),
                content=require_bounded_str(
                    f"turns[{index}].content",
                    content,
                    max_bytes=MAX_LLM_GATEWAY_FRAME_BYTES,
                    allow_empty=True,
                ),
            )
        )
    if not normalized:
        raise IpcError("INVALID_ARGUMENT", "turns must contain at least one entry")
    return tuple(normalized)


def _encode_usage(usage: LlmUsage) -> _llm_pb.LlmUsageWire:
    if not isinstance(usage, LlmUsage):
        raise IpcError("INVALID_ARGUMENT", "usage must be an LlmUsage instance")
    return _llm_pb.LlmUsageWire(
        api_calls=require_uint("usage.api_calls", usage.api_calls),
        prompt_tokens=require_uint("usage.prompt_tokens", usage.prompt_tokens),
        completion_tokens=require_uint(
            "usage.completion_tokens", usage.completion_tokens
        ),
        total_tokens=require_uint("usage.total_tokens", usage.total_tokens),
        cost_microcredits=require_uint(
            "usage.cost_microcredits", usage.cost_microcredits
        ),
    )


def _decode_usage(raw: _llm_pb.LlmUsageWire | None) -> LlmUsage:
    if raw is None:
        return LlmUsage()
    return LlmUsage(
        api_calls=int(raw.api_calls),
        prompt_tokens=int(raw.prompt_tokens),
        completion_tokens=int(raw.completion_tokens),
        total_tokens=int(raw.total_tokens),
        cost_microcredits=int(raw.cost_microcredits),
    )


def _validate_explicit_idempotency_key(idempotency_key: bytes | None) -> bytes:
    if idempotency_key is None:
        raise CommonSemanticsError(
            "MISSING_IDEMPOTENCY_KEY",
            "LlmGateway side-effecting call requires an explicit idempotency_key",
        )
    raw = require_bytes_len("idempotency_key", idempotency_key, _ID_BYTES)
    if not any(raw):
        raise CommonSemanticsError(
            "INVALID_ID",
            "idempotency_key must not be all zeros",
        )
    return raw


class LlmGatewayClient(SabiServiceClientBase):
    """High-level SABI client for ``nlos.sabi.LlmGateway`` (``v1.0``)."""

    _service_name = LLM_GATEWAY_SERVICE
    _schema_name = LLM_GATEWAY_SCHEMA
    _schema_major = LLM_GATEWAY_MAJOR
    _schema_minor = LLM_GATEWAY_MINOR
    _max_payload_bytes = MAX_LLM_GATEWAY_FRAME_BYTES
    _error_type = LlmGatewayError

    async def chat(
        self,
        model: str,
        turns: Sequence[LlmChatTurn | tuple[str, str]],
        *,
        max_output_tokens: int = 0,
        temperature_milli: int = 0,
        session_hint: str = "",
        idempotency_key: bytes | None = None,
        deadline_monotonic_ns: int | None = None,
        correlation_id: bytes | None = None,
        proposal_or_input_digest_sha256: bytes = b"",
        retry_of: Exception | None = None,
    ) -> LlmChatResult:
        """Invoke ``LlmGateway.llm_chat`` with explicit idempotency key and deadline."""

        resolved_key = _validate_explicit_idempotency_key(idempotency_key)
        resolved_model = require_bounded_str("model", model, max_bytes=256)
        normalized_turns = _normalize_turns(turns)
        resolved_tokens = require_uint(
            "max_output_tokens", max_output_tokens, minimum=0, maximum=_U32_MAX
        )
        resolved_temp = require_uint(
            "temperature_milli", temperature_milli, minimum=0, maximum=_U32_MAX
        )
        resolved_hint = require_bounded_str(
            "session_hint", session_hint, max_bytes=512, allow_empty=True
        )

        msg = _llm_pb.LlmChatRequest(
            schema=self._new_schema_identity(),
            model=resolved_model,
            turns=[
                _llm_pb.LlmChatTurn(role=t.role, content=t.content)
                for t in normalized_turns
            ],
            max_output_tokens=resolved_tokens,
            temperature_milli=resolved_temp,
            session_hint=resolved_hint,
        )
        payload, _ = await self._call(
            LLM_CHAT_METHOD,
            msg.SerializeToString(),
            _LONG_RUNNING_MUTATION_SEMANTICS,
            idempotency_key=resolved_key,
            require_explicit_idempotency_key=True,
            deadline_monotonic_ns=deadline_monotonic_ns,
            require_deadline=True,
            correlation_id=correlation_id,
            proposal_or_input_digest_sha256=proposal_or_input_digest_sha256,
            retry_of=retry_of,
        )
        res = self._parse_result(_llm_pb.LlmChatResult, payload, LLM_CHAT_METHOD)
        return LlmChatResult(
            model=str(res.model),
            content_utf8=str(res.content_utf8),
            reasoning_utf8=str(res.reasoning_utf8),
            usage=_decode_usage(res.usage if res.HasField("usage") else None),
        )

    async def llm_chat(
        self,
        model: str,
        turns: Sequence[LlmChatTurn | tuple[str, str]],
        **kwargs: Any,
    ) -> LlmChatResult:
        """Alias for :meth:`chat` matching the wire method name."""

        return await self.chat(model, turns, **kwargs)

    async def query_budget(
        self,
        *,
        deadline_monotonic_ns: int = 0,
        correlation_id: bytes | None = None,
        retry_of: Exception | None = None,
    ) -> QueryBudgetResult:
        """Query the caller's remaining gateway credit budget."""

        msg = _llm_pb.QueryBudgetRequest(schema=self._new_schema_identity())
        payload, _ = await self._call(
            QUERY_BUDGET_METHOD,
            msg.SerializeToString(),
            _QUERY_SEMANTICS,
            deadline_monotonic_ns=deadline_monotonic_ns,
            correlation_id=correlation_id,
            retry_of=retry_of,
        )
        res = self._parse_result(
            _llm_pb.QueryBudgetResult, payload, QUERY_BUDGET_METHOD
        )
        return QueryBudgetResult(
            limit=_decode_usage(res.limit if res.HasField("limit") else None),
            consumed=_decode_usage(
                res.consumed if res.HasField("consumed") else None
            ),
            remaining=_decode_usage(
                res.remaining if res.HasField("remaining") else None
            ),
            external_advisory=_decode_usage(
                res.external_advisory if res.HasField("external_advisory") else None
            ),
        )

    async def attenuate_budget(
        self,
        child_limit: LlmUsage,
        *,
        valid_until_ms: int = 0,
        idempotency_key: bytes | None = None,
        deadline_monotonic_ns: int | None = None,
        correlation_id: bytes | None = None,
        retry_of: Exception | None = None,
    ) -> AttenuateBudgetResult:
        """Attenuate caller budget for a child delegation scope."""

        resolved_key = _validate_explicit_idempotency_key(idempotency_key)
        encoded_limit = _encode_usage(child_limit)
        resolved_valid_until = require_uint(
            "valid_until_ms", valid_until_ms, minimum=0
        )

        msg = _llm_pb.AttenuateBudgetRequest(
            schema=self._new_schema_identity(),
            child_limit=encoded_limit,
            valid_until_ms=resolved_valid_until,
        )
        payload, _ = await self._call(
            ATTENUATE_BUDGET_METHOD,
            msg.SerializeToString(),
            _MUTATION_SEMANTICS,
            idempotency_key=resolved_key,
            require_explicit_idempotency_key=True,
            deadline_monotonic_ns=deadline_monotonic_ns,
            require_deadline=True,
            correlation_id=correlation_id,
            retry_of=retry_of,
        )
        res = self._parse_result(
            _llm_pb.AttenuateBudgetResult, payload, ATTENUATE_BUDGET_METHOD
        )
        return AttenuateBudgetResult(
            granted_limit=_decode_usage(
                res.granted_limit if res.HasField("granted_limit") else None
            ),
            valid_until_ms=int(res.valid_until_ms),
        )

    async def record_external_usage(
        self,
        provider: str,
        model: str,
        usage: LlmUsage,
        *,
        recorded_at_ms: int | None = None,
        idempotency_key: bytes | None = None,
        deadline_monotonic_ns: int | None = None,
        correlation_id: bytes | None = None,
        retry_of: Exception | None = None,
    ) -> RecordExternalUsageResult:
        """Record advisory external provider usage idempotently."""

        resolved_key = _validate_explicit_idempotency_key(idempotency_key)
        resolved_provider = require_bounded_str("provider", provider, max_bytes=128)
        resolved_model = require_bounded_str("model", model, max_bytes=256)
        encoded_usage = _encode_usage(usage)
        resolved_ts = (
            default_wall_ms()
            if recorded_at_ms is None
            else require_uint("recorded_at_ms", recorded_at_ms, minimum=1)
        )

        msg = _llm_pb.RecordExternalUsageRequest(
            schema=self._new_schema_identity(),
            provider=resolved_provider,
            model=resolved_model,
            usage=encoded_usage,
            recorded_at_ms=resolved_ts,
        )
        payload, _ = await self._call(
            RECORD_EXTERNAL_USAGE_METHOD,
            msg.SerializeToString(),
            _MUTATION_SEMANTICS,
            idempotency_key=resolved_key,
            require_explicit_idempotency_key=True,
            deadline_monotonic_ns=deadline_monotonic_ns,
            require_deadline=True,
            correlation_id=correlation_id,
            retry_of=retry_of,
        )
        res = self._parse_result(
            _llm_pb.RecordExternalUsageResult,
            payload,
            RECORD_EXTERNAL_USAGE_METHOD,
        )
        return RecordExternalUsageResult(replayed=bool(res.replayed))


__all__ = [
    "ATTENUATE_BUDGET_METHOD",
    "AttenuateBudgetRequest",
    "AttenuateBudgetResult",
    "DEFAULT_PROCESS_GENERATION",
    "LLM_CHAT_METHOD",
    "LLM_GATEWAY_MAJOR",
    "LLM_GATEWAY_MINOR",
    "LLM_GATEWAY_SCHEMA",
    "LLM_GATEWAY_SERVICE",
    "LlmChatRequest",
    "LlmChatResult",
    "LlmChatTurn",
    "LlmGatewayClient",
    "LlmGatewayError",
    "LlmUsage",
    "MAX_LLM_GATEWAY_FRAME_BYTES",
    "QUERY_BUDGET_METHOD",
    "QueryBudgetRequest",
    "QueryBudgetResult",
    "RECORD_EXTERNAL_USAGE_METHOD",
    "RecordExternalUsageRequest",
    "RecordExternalUsageResult",
]
