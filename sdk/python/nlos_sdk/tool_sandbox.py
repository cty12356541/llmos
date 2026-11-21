"""Thin SABI v1.0 client for ``nlos.sabi.ToolSandbox``."""

from __future__ import annotations

import json
from dataclasses import dataclass
from typing import Any, Mapping

from ._client_base import (
    DEFAULT_PROCESS_GENERATION,
    MAX_CONTROL_PAYLOAD_BYTES,
    SabiServiceClientBase,
    SabiServiceError,
    default_deadline_monotonic_ns,
    require_bounded_str,
    require_bytes_len,
)
from .common import CommonSemanticsError, MethodSemantics, _ensure_gen_python_path
from .local_rpc import IpcError

_ensure_gen_python_path()

from nlos.sabi.v1 import tool_sandbox_pb2 as _tool_pb  # noqa: E402

TOOL_SANDBOX_SCHEMA = "nlos.sabi.ToolSandbox"
TOOL_SANDBOX_MAJOR = 1
TOOL_SANDBOX_MINOR = 0
TOOL_SANDBOX_SERVICE = "tool_sandbox"
MAX_TOOL_SANDBOX_FRAME_BYTES = MAX_CONTROL_PAYLOAD_BYTES

LIST_TOOLS_METHOD = "list_tools"
INVOKE_TOOL_METHOD = "invoke_tool"

TOOL_EFFECT_READ_ONLY = "READ_ONLY"
TOOL_EFFECT_REVERSIBLE = "REVERSIBLE"
TOOL_EFFECT_COMPENSATABLE = "COMPENSATABLE"
TOOL_EFFECT_IRREVERSIBLE = "IRREVERSIBLE"
VALID_TOOL_EFFECT_CLASSES = frozenset(
    {
        TOOL_EFFECT_READ_ONLY,
        TOOL_EFFECT_REVERSIBLE,
        TOOL_EFFECT_COMPENSATABLE,
        TOOL_EFFECT_IRREVERSIBLE,
    }
)

_LONG_RUNNING_QUERY_SEMANTICS = MethodSemantics(
    side_effecting=False, long_running=True
)
_LONG_RUNNING_MUTATION_SEMANTICS = MethodSemantics(
    side_effecting=True, long_running=True
)

_ID_BYTES = 16


@dataclass(frozen=True, slots=True)
class ToolDescriptor:
    """Descriptor for one tool exposed by ``ToolSandbox``."""

    name: str
    summary: str
    effect_class: str
    compensation_handler: str
    input_schema_json: str


@dataclass(frozen=True, slots=True)
class ListToolsRequest:
    """Typed request marker for ``ToolSandbox.list_tools``."""


@dataclass(frozen=True, slots=True)
class ListToolsResult:
    """Typed response payload from ``ToolSandbox.list_tools``."""

    tools: tuple[ToolDescriptor, ...]


@dataclass(frozen=True, slots=True)
class InvokeToolRequest:
    """Typed request parameters for ``ToolSandbox.invoke_tool``."""

    tool: str
    arguments_json: str = "{}"


@dataclass(frozen=True, slots=True)
class InvokeToolResult:
    """Typed response payload from ``ToolSandbox.invoke_tool``."""

    output_json: str

    def parsed_output(self) -> Any:
        """Decode ``output_json`` into Python JSON values."""

        return json.loads(self.output_json)


class ToolSandboxError(SabiServiceError):
    """Typed SABI failure returned by ``ToolSandbox``."""


def _decode_tool_descriptor(
    raw: _tool_pb.ToolDescriptorWire,
    index: int,
) -> ToolDescriptor:
    name = require_bounded_str(f"tools[{index}].name", str(raw.name), max_bytes=256)
    effect_class = str(raw.effect_class)
    if effect_class not in VALID_TOOL_EFFECT_CLASSES:
        raise IpcError(
            "COMPATIBILITY",
            f"ToolSandbox.list_tools returned invalid effect_class for {name!r}",
        )
    compensation_handler = str(raw.compensation_handler)
    if effect_class == TOOL_EFFECT_COMPENSATABLE and not compensation_handler:
        raise IpcError(
            "COMPATIBILITY",
            f"ToolSandbox.list_tools COMPENSATABLE tool {name!r} missing compensation_handler",
        )
    return ToolDescriptor(
        name=name,
        summary=str(raw.summary),
        effect_class=effect_class,
        compensation_handler=compensation_handler,
        input_schema_json=str(raw.input_schema_json),
    )


def _normalize_arguments_json(arguments: str | Mapping[str, Any]) -> str:
    if isinstance(arguments, Mapping):
        try:
            encoded = json.dumps(
                dict(arguments),
                separators=(",", ":"),
                ensure_ascii=False,
            )
        except (TypeError, ValueError) as exc:
            raise IpcError(
                "INVALID_ARGUMENT",
                f"arguments_json mapping is not JSON-serializable: {exc}",
            ) from None
        return require_bounded_str(
            "arguments_json", encoded, max_bytes=MAX_TOOL_SANDBOX_FRAME_BYTES
        )
    return require_bounded_str(
        "arguments_json", arguments, max_bytes=MAX_TOOL_SANDBOX_FRAME_BYTES
    )


class ToolSandboxClient(SabiServiceClientBase):
    """High-level SABI client for ``nlos.sabi.ToolSandbox`` (``v1.0``)."""

    _service_name = TOOL_SANDBOX_SERVICE
    _schema_name = TOOL_SANDBOX_SCHEMA
    _schema_major = TOOL_SANDBOX_MAJOR
    _schema_minor = TOOL_SANDBOX_MINOR
    _max_payload_bytes = MAX_TOOL_SANDBOX_FRAME_BYTES
    _error_type = ToolSandboxError

    async def list_tools(
        self,
        *,
        deadline_monotonic_ns: int | None = None,
        correlation_id: bytes | None = None,
        retry_of: Exception | None = None,
    ) -> ListToolsResult:
        """List registered sandbox tools and their effect descriptors."""

        resolved_deadline = (
            default_deadline_monotonic_ns()
            if deadline_monotonic_ns is None
            else deadline_monotonic_ns
        )
        msg = _tool_pb.ListToolsRequest(schema=self._new_schema_identity())
        payload, _ = await self._call(
            LIST_TOOLS_METHOD,
            msg.SerializeToString(),
            _LONG_RUNNING_QUERY_SEMANTICS,
            deadline_monotonic_ns=resolved_deadline,
            require_deadline=True,
            correlation_id=correlation_id,
            retry_of=retry_of,
        )
        res = self._parse_result(
            _tool_pb.ListToolsResult, payload, LIST_TOOLS_METHOD
        )
        return ListToolsResult(
            tools=tuple(
                _decode_tool_descriptor(item, idx)
                for idx, item in enumerate(res.tools)
            )
        )

    async def invoke_tool(
        self,
        tool: str,
        arguments_json: str | Mapping[str, Any] = "{}",
        *,
        idempotency_key: bytes | None = None,
        deadline_monotonic_ns: int | None = None,
        correlation_id: bytes | None = None,
        proposal_or_input_digest_sha256: bytes = b"",
        retry_of: Exception | None = None,
    ) -> InvokeToolResult:
        """Invoke a sandboxed tool with explicit idempotency key and deadline."""

        if idempotency_key is None:
            raise CommonSemanticsError(
                "MISSING_IDEMPOTENCY_KEY",
                "ToolSandbox.invoke_tool requires an explicit idempotency_key",
            )
        resolved_key = require_bytes_len("idempotency_key", idempotency_key, _ID_BYTES)
        if not any(resolved_key):
            raise CommonSemanticsError(
                "INVALID_ID",
                "idempotency_key must not be all zeros",
            )

        resolved_tool = require_bounded_str("tool", tool, max_bytes=256)
        resolved_args = _normalize_arguments_json(arguments_json)

        msg = _tool_pb.InvokeToolRequest(
            schema=self._new_schema_identity(),
            tool=resolved_tool,
            arguments_json=resolved_args,
        )
        payload, _ = await self._call(
            INVOKE_TOOL_METHOD,
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
        res = self._parse_result(
            _tool_pb.InvokeToolResult, payload, INVOKE_TOOL_METHOD
        )
        return InvokeToolResult(output_json=str(res.output_json))


__all__ = [
    "DEFAULT_PROCESS_GENERATION",
    "INVOKE_TOOL_METHOD",
    "InvokeToolRequest",
    "InvokeToolResult",
    "LIST_TOOLS_METHOD",
    "ListToolsRequest",
    "ListToolsResult",
    "MAX_TOOL_SANDBOX_FRAME_BYTES",
    "TOOL_EFFECT_COMPENSATABLE",
    "TOOL_EFFECT_IRREVERSIBLE",
    "TOOL_EFFECT_READ_ONLY",
    "TOOL_EFFECT_REVERSIBLE",
    "TOOL_SANDBOX_MAJOR",
    "TOOL_SANDBOX_MINOR",
    "TOOL_SANDBOX_SCHEMA",
    "TOOL_SANDBOX_SERVICE",
    "ToolDescriptor",
    "ToolSandboxClient",
    "ToolSandboxError",
    "VALID_TOOL_EFFECT_CLASSES",
]
