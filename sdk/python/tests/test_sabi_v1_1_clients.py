"""Unit and real local-socket roundtrip tests for SABI v1.1 Python SDK clients."""

from __future__ import annotations

import asyncio
import hashlib
import secrets
import struct
import sys
import tempfile
import unittest
from pathlib import Path

_SDK_PY = str(Path(__file__).resolve().parents[1])
if _SDK_PY not in sys.path:
    sys.path.insert(0, _SDK_PY)

from nlos_sdk import (  # noqa: E402  (import first to wire gen/python path)
    BLOB_REFERENCE_SCHEMA,
    LLM_GATEWAY_SCHEMA,
    MAX_BLOB_CHUNK_BYTES,
    MAX_BLOB_FRAME_BYTES,
    OPERATION_CONTROL_SCHEMA,
    SABI_V1_CHANNELS,
    TOOL_SANDBOX_SCHEMA,
    WAIT_CONTROL_SCHEMA,
    WAIT_STATE_CANCELLED,
    WAIT_STATE_PENDING,
    WAIT_STATE_WOKEN,
    BlobReferenceClient,
    BlobReferenceError,
    CommonSemanticsError,
    Ed25519Signer,
    IpcError,
    LlmChatTurn,
    LlmGatewayClient,
    LlmGatewayError,
    LlmUsage,
    LocalRpcClient,
    OperationClient,
    OperationControlError,
    OperationLifecycleState,
    OperationReference,
    ReadReferenceResult,
    SabiServiceError,
    ServiceDirectoryClient,
    SessionIdentity,
    ToolSandboxClient,
    TransportConfig,
    WaitClient,
    connect_authenticated,
    default_deadline_monotonic_ns,
    endpoint_channel_binding,
    format_blob_uri,
    parse_blob_uri,
    principal_handshake_message,
    validate_schema_compatibility,
)
from nlos.sabi.v1 import blob_reference_pb2 as _blob_pb  # noqa: E402
from nlos.sabi.v1 import envelope_pb2 as _env_pb  # noqa: E402
from nlos.sabi.v1 import principal_handshake_pb2 as _hs_pb  # noqa: E402
from nlos.sabi.v1 import llm_gateway_pb2 as _llm_pb  # noqa: E402
from nlos.sabi.v1 import operation_control_pb2 as _op_pb  # noqa: E402
from nlos.sabi.v1 import service_directory_pb2 as _dir_pb  # noqa: E402
from nlos.sabi.v1 import tool_sandbox_pb2 as _tool_pb  # noqa: E402
from nlos.sabi.v1 import wait_control_pb2 as _wait_pb  # noqa: E402


def _new_schema(name: str, major: int = 1, minor: int = 0) -> _env_pb.SchemaIdentity:
    return _env_pb.SchemaIdentity(name=name, major=major, minor=minor)


_MUTATION_METHODS = frozenset(
    {
        "register_wait",
        "notify_commits",
        "cancel_wait",
        "cancel",
        "llm_chat",
        "attenuate_budget",
        "record_external_usage",
        "invoke_tool",
        "mint_reference",
        "read_reference",
        "release_reference",
    }
)


def _wrap_response(
    req: _env_pb.ExchangeRequest,
    payload: bytes,
    *,
    error_code: int = _env_pb.SABI_ERROR_CODE_UNSPECIFIED,
    retry: int = _env_pb.RETRY_DIRECTIVE_UNSPECIFIED,
    safe_message: str = "",
    operation: _env_pb.OperationReference | None = None,
    receipts: tuple[bytes, ...] = (),
    schema_major: int = 1,
    critical_extension_ids: tuple[int, ...] = (),
) -> _env_pb.ExchangeResponse:
    resp = _env_pb.ExchangeResponse()
    resp.envelope.schema.CopyFrom(_new_schema("nlos.sabi.Envelope", schema_major, 1))
    resp.envelope.schema.critical_extension_ids.extend(critical_extension_ids)
    resp.envelope.request_id = req.envelope.request_id
    resp.envelope.service = req.envelope.service
    resp.envelope.method = req.envelope.method
    resp.envelope.payload = payload

    resp_ctx = resp.envelope.response_context
    resp_ctx.correlation_id = req.envelope.request_context.correlation_id
    effective_receipts = receipts
    if (
        not effective_receipts
        and operation is None
        and error_code == _env_pb.SABI_ERROR_CODE_UNSPECIFIED
        and req.envelope.method in _MUTATION_METHODS
    ):
        effective_receipts = (
            hashlib.sha256(req.envelope.request_id or b"receipt").digest()[:16],
        )
    if operation is not None:
        resp_ctx.operation.CopyFrom(operation)
    for receipt_id in effective_receipts:
        rec = resp_ctx.receipts.add()
        rec.receipt_id = receipt_id
    if error_code != _env_pb.SABI_ERROR_CODE_UNSPECIFIED:
        resp_ctx.failure.code = error_code
        resp_ctx.failure.retry = retry
        resp_ctx.failure.safe_message = safe_message
    return resp


async def _read_wire_frame(reader: asyncio.StreamReader) -> bytes:
    header = await reader.readexactly(4)
    (length,) = struct.unpack("!I", header)
    return await reader.readexactly(length)


async def _write_wire_frame(writer: asyncio.StreamWriter, body: bytes) -> None:
    writer.write(struct.pack("!I", len(body)) + body)
    await writer.drain()


class _FakeSabiServer:
    """In-process real Unix socket server speaking ADR-0011 handshake + SABI v1.1."""

    def __init__(self, sock_path: Path, expected_signer: Ed25519Signer) -> None:
        self.sock_path = sock_path
        self.expected_signer = expected_signer
        self.server: asyncio.AbstractServer | None = None
        self.connection_count = 0
        self.request_count = 0
        self.waits: dict[bytes, _wait_pb.WaitRecord] = {}
        self.operations: dict[tuple[bytes, int], _op_pb.OperationStatus] = {}
        self.blobs: dict[bytes, tuple[bytes, int, bytes]] = {}
        self.secret_blob = b"BLOB_CANARY_PAYLOAD_SECRET_987654321_DO_NOT_LEAK"
        self.secret_receipt = b"RECEIPT_CANARY_SECRET_BYTES_1234"
        self.behavior_hook: str | None = None
        self._chat_attempts = 0
        self._budget_remaining = 5000

    async def start(self) -> None:
        self.server = await asyncio.start_unix_server(
            self._handle_conn, path=str(self.sock_path)
        )

    async def close(self) -> None:
        if self.server is not None:
            self.server.close()
            await self.server.wait_closed()
            self.server = None

    async def _handle_conn(
        self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter
    ) -> None:
        self.connection_count += 1
        try:
            # 1. ADR-0011 server-first challenge
            nonce = secrets.token_bytes(32)
            binding = endpoint_channel_binding(str(self.sock_path))
            challenge = _hs_pb.PrincipalHandshakeChallenge(
                schema=_new_schema("nlos.sabi.PrincipalHandshake", 1, 0),
                nonce=nonce,
            )
            await _write_wire_frame(writer, challenge.SerializeToString())

            att_raw = await _read_wire_frame(reader)
            att = _hs_pb.PrincipalHandshakeAttestation.FromString(att_raw)
            if bytes(att.nonce) != nonce or bytes(att.channel_binding) != binding:
                return
            msg = principal_handshake_message(
                bytes(att.nonce),
                bytes(att.principal_id),
                bytes(att.channel_binding),
            )
            try:
                from nacl.signing import VerifyKey

                VerifyKey(self.expected_signer.public_key).verify(
                    msg, bytes(att.signature)
                )
            except Exception:
                return

            # 2. Persistent request/response loop
            while True:
                try:
                    req_raw = await _read_wire_frame(reader)
                except asyncio.IncompleteReadError:
                    return
                self.request_count += 1
                req = _env_pb.ExchangeRequest.FromString(req_raw)

                if self.behavior_hook == "stall":
                    await asyncio.sleep(1.5)
                    return
                if self.behavior_hook == "half_header":
                    writer.write(b"\x10\x00")
                    await writer.drain()
                    writer.close()
                    return
                if self.behavior_hook == "half_body":
                    writer.write(struct.pack("!I", 128) + b"partial-frame")
                    await writer.drain()
                    writer.close()
                    return

                resp = self._dispatch(req, bytes(att.principal_id))
                await _write_wire_frame(writer, resp.SerializeToString())
        finally:
            writer.close()
            try:
                await writer.wait_closed()
            except Exception:
                pass

    def _dispatch(
        self, req: _env_pb.ExchangeRequest, caller_principal: bytes
    ) -> _env_pb.ExchangeResponse:
        service = req.envelope.service
        method = req.envelope.method
        payload = bytes(req.envelope.payload)

        if self.behavior_hook == "unknown_major":
            return _wrap_response(req, b"", schema_major=2)
        if self.behavior_hook == "unknown_critical_ext":
            return _wrap_response(req, b"", critical_extension_ids=(77,))

        if service == "service_directory" and method == "negotiate":
            dreq = _dir_pb.NegotiateServiceRequest.FromString(payload)
            transport = (
                dreq.supported_transport_kinds[0]
                if dreq.supported_transport_kinds
                else _dir_pb.LOCAL_TRANSPORT_KIND_UNIX_DOMAIN_SOCKET
            )
            dres = _dir_pb.NegotiateServiceResponse(
                schema=_new_schema("nlos.sabi.ServiceDirectory", 1, 0),
                binding=_dir_pb.ServiceBinding(
                    candidate=_dir_pb.ServiceCandidate(
                        binding_id=hashlib.sha256(
                            dreq.service.encode("utf-8")
                        ).digest()[:16],
                        generation=1,
                        service=dreq.service,
                        version=_dir_pb.ServiceVersion(
                            schema_name=dreq.schema_name,
                            major=dreq.major,
                            minor=dreq.minimum_minor,
                        ),
                        feature_ids=list(dreq.required_feature_ids),
                        transport_kinds=[transport],
                    ),
                    endpoint=_dir_pb.LocalEndpoint(
                        kind=transport,
                        address=str(self.sock_path),
                    ),
                ),
            )
            return _wrap_response(req, dres.SerializeToString())

        if service == "wait_control":
            return self._dispatch_wait(req, method, payload)
        if service == "operation_control":
            return self._dispatch_operation(req, method, payload)
        if service == "llm_gateway":
            return self._dispatch_llm(req, method, payload)
        if service == "tool_sandbox":
            return self._dispatch_tool(req, method, payload)
        if service == "blob_reference":
            return self._dispatch_blob(req, method, payload, caller_principal)

        return _wrap_response(
            req,
            b"",
            error_code=_env_pb.SABI_ERROR_CODE_NOT_SUPPORTED,
            retry=_env_pb.RETRY_DIRECTIVE_DO_NOT_RETRY,
            safe_message=f"unknown service {service}",
        )

    def _dispatch_wait(
        self, req: _env_pb.ExchangeRequest, method: str, payload: bytes
    ) -> _env_pb.ExchangeResponse:
        if method == "register_wait":
            wreq = _wait_pb.RegisterWaitRequest.FromString(payload)
            wait_id = hashlib.sha256(wreq.idempotency_key).digest()[:16]
            replayed = wait_id in self.waits
            if not replayed:
                rec = _wait_pb.WaitRecord(
                    wait_id=wait_id,
                    binding=wreq.binding,
                    channel_id=wreq.channel_id,
                    channel_generation=1,
                    channel_fencing_token=b"\x55" * 16,
                    target_sequence=wreq.target_sequence,
                    state=WAIT_STATE_PENDING,
                    idempotency_key=wreq.idempotency_key,
                    registered_at_ms=wreq.registered_at_ms,
                )
                self.waits[wait_id] = rec
            res = _wait_pb.RegisterWaitResult(
                schema=_new_schema(WAIT_CONTROL_SCHEMA),
                record=self.waits[wait_id],
                replayed=replayed,
            )
            return _wrap_response(req, res.SerializeToString())

        if method == "inspect_wait":
            ireq = _wait_pb.InspectWaitRequest.FromString(payload)
            rec = self.waits.get(bytes(ireq.wait_id))
            if rec is None:
                return _wrap_response(
                    req,
                    b"",
                    error_code=_env_pb.SABI_ERROR_CODE_NOT_FOUND,
                    retry=_env_pb.RETRY_DIRECTIVE_DO_NOT_RETRY,
                    safe_message="wait not found",
                )
            res = _wait_pb.InspectWaitResult(
                schema=_new_schema(WAIT_CONTROL_SCHEMA),
                record=rec,
            )
            return _wrap_response(req, res.SerializeToString())

        if method == "list_waits":
            lreq = _wait_pb.ListWaitsRequest.FromString(payload)
            res = _wait_pb.ListWaitsResult(schema=_new_schema(WAIT_CONTROL_SCHEMA))
            for rec in self.waits.values():
                if not lreq.filter_channel_id or rec.channel_id == lreq.filter_channel_id:
                    res.waits.add().CopyFrom(rec)
            return _wrap_response(req, res.SerializeToString())

        if method == "notify_commits":
            nreq = _wait_pb.NotifyCommitsRequest.FromString(payload)
            res = _wait_pb.WakeReport(schema=_new_schema(WAIT_CONTROL_SCHEMA))
            for rec in self.waits.values():
                if (
                    rec.channel_id == nreq.channel_id
                    and rec.state == WAIT_STATE_PENDING
                    and rec.target_sequence <= nreq.up_to_sequence
                ):
                    rec.state = WAIT_STATE_WOKEN
                    rec.woken_at_ms = nreq.notified_at_ms
                    rec.woken_up_to_sequence = nreq.up_to_sequence
                    res.woken.add().CopyFrom(rec)
            return _wrap_response(req, res.SerializeToString())

        if method == "cancel_wait":
            creq = _wait_pb.CancelWaitRequest.FromString(payload)
            rec = self.waits.get(bytes(creq.wait_id))
            if rec is None:
                return _wrap_response(
                    req,
                    b"",
                    error_code=_env_pb.SABI_ERROR_CODE_NOT_FOUND,
                    retry=_env_pb.RETRY_DIRECTIVE_DO_NOT_RETRY,
                    safe_message="wait not found",
                )
            replayed = rec.state == WAIT_STATE_CANCELLED
            rec.state = WAIT_STATE_CANCELLED
            rec.cancelled_at_ms = creq.cancelled_at_ms
            res = _wait_pb.CancelWaitResult(
                schema=_new_schema(WAIT_CONTROL_SCHEMA),
                record=rec,
                replayed=replayed,
            )
            return _wrap_response(req, res.SerializeToString())

        return _wrap_response(req, b"")

    def _dispatch_operation(
        self, req: _env_pb.ExchangeRequest, method: str, payload: bytes
    ) -> _env_pb.ExchangeResponse:
        if method == "query":
            qreq = _op_pb.QueryOperationRequest.FromString(payload)
            key = (bytes(qreq.operation.operation_id), int(qreq.operation.generation))
            status = self.operations.get(key)
            if status is None:
                status = _op_pb.OperationStatus(
                    schema=_new_schema(OPERATION_CONTROL_SCHEMA),
                    operation=qreq.operation,
                    state=OperationLifecycleState.DISPATCHED,
                    cancel_epoch=1,
                    receipt=_env_pb.ReceiptReference(receipt_id=self.secret_receipt),
                )
                self.operations[key] = status
            return _wrap_response(req, status.SerializeToString())

        if method == "cancel":
            creq = _op_pb.CancelOperationRequest.FromString(payload)
            key = (bytes(creq.operation.operation_id), int(creq.operation.generation))
            cur = self.operations.get(key)
            if cur is None or cur.cancel_epoch != creq.expected_cancel_epoch:
                return _wrap_response(
                    req,
                    b"",
                    error_code=_env_pb.SABI_ERROR_CODE_FAILED_PRECONDITION,
                    retry=_env_pb.RETRY_DIRECTIVE_DO_NOT_RETRY,
                    safe_message="cancel_epoch mismatch",
                )
            cur.state = OperationLifecycleState.CANCEL_REQUESTED
            cur.cancel_epoch += 1
            return _wrap_response(req, cur.SerializeToString())

        return _wrap_response(req, b"")

    def _dispatch_llm(
        self, req: _env_pb.ExchangeRequest, method: str, payload: bytes
    ) -> _env_pb.ExchangeResponse:
        if method == "llm_chat":
            self._chat_attempts += 1
            if self.behavior_hook == "uncertain_once" and self._chat_attempts == 1:
                op_ref = _env_pb.OperationReference(
                    operation_id=hashlib.sha256(
                        req.envelope.request_id or b"op"
                    ).digest()[:16],
                    generation=1,
                )
                return _wrap_response(
                    req,
                    b"",
                    error_code=_env_pb.SABI_ERROR_CODE_UNCERTAIN,
                    retry=_env_pb.RETRY_DIRECTIVE_QUERY_OPERATION_OR_RETRY_SAME_IDEMPOTENCY_KEY,
                    safe_message="upstream gateway timeout",
                    operation=op_ref,
                )
            if self.behavior_hook == "do_not_retry":
                return _wrap_response(
                    req,
                    b"",
                    error_code=_env_pb.SABI_ERROR_CODE_INVALID_ARGUMENT,
                    retry=_env_pb.RETRY_DIRECTIVE_DO_NOT_RETRY,
                    safe_message="bad prompt parameter",
                )
            creq = _llm_pb.LlmChatRequest.FromString(payload)
            self._budget_remaining = max(0, self._budget_remaining - 25)
            res = _llm_pb.LlmChatResult(
                schema=_new_schema(LLM_GATEWAY_SCHEMA),
                model=creq.model,
                content_utf8=f"echo:{creq.turns[-1].content}",
                reasoning_utf8="checked",
                usage=_llm_pb.LlmUsageWire(
                    api_calls=1,
                    prompt_tokens=10,
                    completion_tokens=15,
                    total_tokens=25,
                    cost_microcredits=100,
                ),
            )
            return _wrap_response(req, res.SerializeToString())

        if method == "query_budget":
            res = _llm_pb.QueryBudgetResult(
                schema=_new_schema(LLM_GATEWAY_SCHEMA),
                limit=_llm_pb.LlmUsageWire(api_calls=100, total_tokens=5000),
                consumed=_llm_pb.LlmUsageWire(
                    api_calls=self._chat_attempts,
                    total_tokens=5000 - self._budget_remaining,
                ),
                remaining=_llm_pb.LlmUsageWire(
                    api_calls=100 - self._chat_attempts,
                    total_tokens=self._budget_remaining,
                ),
                external_advisory=_llm_pb.LlmUsageWire(api_calls=2, total_tokens=40),
            )
            return _wrap_response(req, res.SerializeToString())

        if method == "attenuate_budget":
            areq = _llm_pb.AttenuateBudgetRequest.FromString(payload)
            granted_tokens = min(
                int(areq.child_limit.total_tokens), self._budget_remaining
            )
            res = _llm_pb.AttenuateBudgetResult(
                schema=_new_schema(LLM_GATEWAY_SCHEMA),
                granted_limit=_llm_pb.LlmUsageWire(
                    api_calls=int(areq.child_limit.api_calls),
                    total_tokens=granted_tokens,
                ),
                valid_until_ms=areq.valid_until_ms,
            )
            return _wrap_response(req, res.SerializeToString())

        if method == "record_external_usage":
            res = _llm_pb.RecordExternalUsageResult(
                schema=_new_schema(LLM_GATEWAY_SCHEMA),
                replayed=False,
            )
            return _wrap_response(req, res.SerializeToString())

        return _wrap_response(req, b"")

    def _dispatch_tool(
        self, req: _env_pb.ExchangeRequest, method: str, payload: bytes
    ) -> _env_pb.ExchangeResponse:
        if method == "list_tools":
            res = _tool_pb.ListToolsResult(schema=_new_schema(TOOL_SANDBOX_SCHEMA))
            res.tools.add(
                name="fs.write_note",
                summary="Write a reversible note",
                effect_class="COMPENSATABLE",
                compensation_handler="fs.delete_note",
                input_schema_json='{"type":"object"}',
            )
            return _wrap_response(req, res.SerializeToString())

        if method == "invoke_tool":
            ireq = _tool_pb.InvokeToolRequest.FromString(payload)
            res = _tool_pb.InvokeToolResult(
                schema=_new_schema(TOOL_SANDBOX_SCHEMA),
                output_json=f'{{"ok":true,"tool":"{ireq.tool}"}}',
            )
            return _wrap_response(req, res.SerializeToString())

        return _wrap_response(req, b"")

    def _dispatch_blob(
        self,
        req: _env_pb.ExchangeRequest,
        method: str,
        payload: bytes,
        caller_principal: bytes,
    ) -> _env_pb.ExchangeResponse:
        if method == "mint_reference":
            mreq = _blob_pb.MintReferenceRequest.FromString(payload)
            if len(mreq.recipient_principal_id) != 16:
                return _wrap_response(
                    req,
                    b"",
                    error_code=_env_pb.SABI_ERROR_CODE_INVALID_ARGUMENT,
                    retry=_env_pb.RETRY_DIRECTIVE_DO_NOT_RETRY,
                    safe_message="recipient_principal_id must be 16 bytes",
                )
            ref_id = secrets.token_bytes(16)
            self.blobs[ref_id] = (
                bytes(mreq.recipient_principal_id),
                int(mreq.max_chunk_bytes),
                self.secret_blob,
            )
            res = _blob_pb.MintReferenceResult(
                schema=_new_schema(BLOB_REFERENCE_SCHEMA),
                reference_id=ref_id,
                minted_at_ms=1_700_000_000_123,
            )
            return _wrap_response(req, res.SerializeToString())

        if method == "read_reference":
            rreq = _blob_pb.ReadReferenceRequest.FromString(payload)
            entry = self.blobs.get(bytes(rreq.reference_id))
            if entry is None:
                return _wrap_response(
                    req,
                    b"",
                    error_code=_env_pb.SABI_ERROR_CODE_NOT_FOUND,
                    retry=_env_pb.RETRY_DIRECTIVE_DO_NOT_RETRY,
                    safe_message="reference not found",
                )
            recipient, block_bytes, data = entry
            if recipient != caller_principal:
                return _wrap_response(
                    req,
                    b"",
                    error_code=_env_pb.SABI_ERROR_CODE_PERMISSION_DENIED,
                    retry=_env_pb.RETRY_DIRECTIVE_DO_NOT_RETRY,
                    safe_message="holder mismatch",
                )
            if self.behavior_hook == "oversize_blob_resp":
                oversized = _blob_pb.ReadReferenceResult(
                    schema=_new_schema(BLOB_REFERENCE_SCHEMA),
                    chunk=b"X" * (MAX_BLOB_CHUNK_BYTES + 200),
                    offset=0,
                    chunk_digest_sha256=b"\xaa" * 32,
                )
                return _wrap_response(req, oversized.SerializeToString())
            if rreq.offset % block_bytes != 0:
                return _wrap_response(
                    req,
                    b"",
                    error_code=_env_pb.SABI_ERROR_CODE_INVALID_ARGUMENT,
                    retry=_env_pb.RETRY_DIRECTIVE_DO_NOT_RETRY,
                    safe_message="offset not aligned to block_bytes",
                )
            chunk = data[rreq.offset : rreq.offset + min(rreq.length_bytes, block_bytes)]
            digest = hashlib.sha256(chunk).digest()
            if self.behavior_hook == "bad_chunk_digest":
                digest = bytes([digest[0] ^ 0xFF]) + digest[1:]
            res = _blob_pb.ReadReferenceResult(
                schema=_new_schema(BLOB_REFERENCE_SCHEMA),
                chunk=chunk,
                offset=rreq.offset,
                chunk_digest_sha256=digest,
            )
            return _wrap_response(req, res.SerializeToString())

        if method == "release_reference":
            rreq = _blob_pb.ReleaseReferenceRequest.FromString(payload)
            released = self.blobs.pop(bytes(rreq.reference_id), None) is not None
            res = _blob_pb.ReleaseReferenceResult(
                schema=_new_schema(BLOB_REFERENCE_SCHEMA),
                released=released,
            )
            return _wrap_response(req, res.SerializeToString())

        return _wrap_response(req, b"")


class TestChannelRegistryAndExports(unittest.TestCase):
    """Static registry, wheel-independent exports, and compatibility gates."""

    def test_twelve_channels_and_schema_compatibility_gates(self) -> None:
        self.assertEqual(len(SABI_V1_CHANNELS), 12)
        for name in (
            LLM_GATEWAY_SCHEMA,
            TOOL_SANDBOX_SCHEMA,
            BLOB_REFERENCE_SCHEMA,
            WAIT_CONTROL_SCHEMA,
            OPERATION_CONTROL_SCHEMA,
        ):
            self.assertIn(name, SABI_V1_CHANNELS)
            self.assertEqual(SABI_V1_CHANNELS[name].major, 1)
            self.assertEqual(SABI_V1_CHANNELS[name].minor, 0)

        with self.assertRaises(CommonSemanticsError) as unknown_major:
            validate_schema_compatibility(
                _new_schema(LLM_GATEWAY_SCHEMA, 2, 0), LLM_GATEWAY_SCHEMA
            )
        self.assertEqual(unknown_major.exception.code, "E_ABI_VERSION")

        with self.assertRaises(CommonSemanticsError) as crit_nonzero:
            validate_schema_compatibility(
                _env_pb.SchemaIdentity(
                    name=BLOB_REFERENCE_SCHEMA,
                    major=1,
                    minor=0,
                    critical_extension_ids=[11],
                ),
                BLOB_REFERENCE_SCHEMA,
            )
        self.assertEqual(
            crit_nonzero.exception.code, "UNSUPPORTED_CRITICAL_EXTENSION"
        )

        with self.assertRaises(CommonSemanticsError) as crit_zero:
            validate_schema_compatibility(
                _env_pb.SchemaIdentity(
                    name=BLOB_REFERENCE_SCHEMA,
                    major=1,
                    minor=0,
                    critical_extension_ids=[0],
                ),
                BLOB_REFERENCE_SCHEMA,
            )
        self.assertEqual(
            crit_zero.exception.code, "UNSUPPORTED_CRITICAL_EXTENSION"
        )

        with self.assertRaises(CommonSemanticsError) as too_many_ext:
            validate_schema_compatibility(
                _env_pb.SchemaIdentity(
                    name=BLOB_REFERENCE_SCHEMA,
                    major=1,
                    minor=0,
                    non_critical_extension_ids=list(range(1, 34)),
                ),
                BLOB_REFERENCE_SCHEMA,
            )
        self.assertEqual(too_many_ext.exception.code, "E_ABI_VERSION")

    def test_signer_repr_redacts_private_seed(self) -> None:
        seed = b"SEED_CANARY_SECRET_BYTES_1234567"
        signer = Ed25519Signer(seed)
        self.assertEqual(len(signer.sign(b"msg")), 64)
        self.assertNotIn(seed.hex(), repr(signer))
        self.assertNotIn("SEED_CANARY", repr(signer))
        self.assertNotIn("SEED_CANARY", str(signer))

    def test_blob_uri_helpers(self) -> None:
        ref = bytes.fromhex("0123456789abcdef0123456789abcdef")
        uri = format_blob_uri(ref)
        self.assertEqual(uri, "nlos-blob://0123456789abcdef0123456789abcdef")
        self.assertEqual(parse_blob_uri(uri), ref)
        self.assertEqual(
            parse_blob_uri("nlosref://0123456789abcdef0123456789abcdef"), ref
        )
        with self.assertRaises(IpcError):
            parse_blob_uri("http://example.com")
        with self.assertRaises(IpcError):
            parse_blob_uri("nlos-blob://" + "00" * 16)


class TestSabiV11AuthenticatedSocketRoundtrips(unittest.IsolatedAsyncioTestCase):
    """Real Unix-socket tests for all 5 new clients on a persistent connection."""

    async def test_all_five_clients_share_single_authenticated_connection(self) -> None:
        signer = Ed25519Signer(secrets.token_bytes(32))
        principal = hashlib.sha256(b"principal-alpha").digest()[:16]
        with tempfile.TemporaryDirectory(dir="/tmp", prefix="nlos-py-") as tmp:
            sock = Path(tmp) / "daemon.sock"
            server = _FakeSabiServer(sock, signer)
            await server.start()
            try:
                identity = SessionIdentity(
                    principal_id=principal,
                    application_id=b"\x22" * 16,
                    process_id=b"\x33" * 16,
                )
                rpc = await connect_authenticated(
                    str(sock), signer, identity.principal_id
                )
                try:
                    self.assertTrue(rpc.is_usable)
                    dir_client = ServiceDirectoryClient(
                        rpc,
                        principal_id=identity.principal_id,
                        application_id=identity.application_id,
                        process_id=identity.process_id,
                    )
                    wait_client = WaitClient(
                        rpc,
                        principal_id=identity.principal_id,
                        application_id=identity.application_id,
                        process_id=identity.process_id,
                    )
                    op_client = OperationClient(
                        rpc,
                        principal_id=identity.principal_id,
                        application_id=identity.application_id,
                        process_id=identity.process_id,
                    )
                    llm_client = LlmGatewayClient(
                        rpc,
                        principal_id=identity.principal_id,
                        application_id=identity.application_id,
                        process_id=identity.process_id,
                    )
                    tool_client = ToolSandboxClient(
                        rpc,
                        principal_id=identity.principal_id,
                        application_id=identity.application_id,
                        process_id=identity.process_id,
                    )
                    blob_client = BlobReferenceClient(
                        rpc,
                        principal_id=identity.principal_id,
                        application_id=identity.application_id,
                        process_id=identity.process_id,
                    )

                    negotiated = [
                        await dir_client.negotiate(req)
                        for req in (
                            WaitClient.service_requirement(),
                            OperationClient.service_requirement(),
                            LlmGatewayClient.service_requirement(),
                            ToolSandboxClient.service_requirement(),
                            BlobReferenceClient.service_requirement(),
                        )
                    ]
                    self.assertEqual(len(negotiated), 5)

                    binding = secrets.token_bytes(16)
                    channel_id = secrets.token_bytes(16)
                    reg = await wait_client.register_wait(binding, channel_id, 5)
                    self.assertEqual(reg.record.state, WAIT_STATE_PENDING)
                    inspected = await wait_client.inspect_wait(reg.record.wait_id)
                    self.assertEqual(inspected.record.wait_id, reg.record.wait_id)
                    listed = await wait_client.list_waits(channel_id)
                    self.assertEqual(len(listed.waits), 1)
                    wake = await wait_client.notify_commits(channel_id, 7)
                    self.assertEqual(len(wake.woken), 1)
                    self.assertEqual(wake.woken[0].state, WAIT_STATE_WOKEN)
                    cancelled = await wait_client.cancel_wait(reg.record.wait_id)
                    self.assertEqual(cancelled.record.state, WAIT_STATE_CANCELLED)

                    op_ref = OperationReference(secrets.token_bytes(16), 1)
                    status = await op_client.query(op_ref)
                    self.assertEqual(status.state, OperationLifecycleState.DISPATCHED)
                    self.assertNotIn(
                        "RECEIPT_CANARY",
                        repr(status) + str(status),
                    )
                    status2 = await op_client.cancel(
                        op_ref, expected_cancel_epoch=status.cancel_epoch
                    )
                    self.assertEqual(
                        status2.state, OperationLifecycleState.CANCEL_REQUESTED
                    )

                    deadline = default_deadline_monotonic_ns()
                    budget_before = await llm_client.query_budget()
                    chat_res = await llm_client.chat(
                        "local-qwen",
                        [LlmChatTurn(role="user", content="hello")],
                        idempotency_key=secrets.token_bytes(16),
                        deadline_monotonic_ns=deadline,
                    )
                    self.assertEqual(chat_res.content_utf8, "echo:hello")
                    self.assertEqual(chat_res.usage.total_tokens, 25)
                    budget_after = await llm_client.query_budget()
                    self.assertEqual(
                        budget_after.remaining.total_tokens,
                        budget_before.remaining.total_tokens - 25,
                    )
                    att_res = await llm_client.attenuate_budget(
                        LlmUsage(api_calls=5, total_tokens=1000),
                        valid_until_ms=1_800_000_000_000,
                        idempotency_key=secrets.token_bytes(16),
                        deadline_monotonic_ns=deadline,
                    )
                    self.assertEqual(att_res.granted_limit.total_tokens, 1000)
                    ext_res = await llm_client.record_external_usage(
                        "openai",
                        "gpt-test",
                        LlmUsage(api_calls=1, total_tokens=10),
                        idempotency_key=secrets.token_bytes(16),
                        deadline_monotonic_ns=deadline,
                    )
                    self.assertFalse(ext_res.replayed)

                    tools_res = await tool_client.list_tools()
                    self.assertEqual(tools_res.tools[0].name, "fs.write_note")
                    invoke_res = await tool_client.invoke_tool(
                        "fs.write_note",
                        {"title": "test"},
                        idempotency_key=secrets.token_bytes(16),
                        deadline_monotonic_ns=deadline,
                    )
                    self.assertEqual(
                        invoke_res.parsed_output(), {"ok": True, "tool": "fs.write_note"}
                    )

                    file_digest = hashlib.sha256(server.secret_blob).digest()
                    minted = await blob_client.mint_reference(
                        "/safe/handle",
                        file_digest,
                        len(server.secret_blob),
                        max_reads=2,
                        max_chunk_bytes=16,
                        recipient_principal_id=principal,
                    )
                    chunk0 = await blob_client.read_reference(
                        minted.reference_id, offset=0
                    )
                    self.assertEqual(chunk0.chunk, server.secret_blob[:16])
                    self.assertNotIn("BLOB_CANARY", repr(chunk0) + str(chunk0))
                    chunk1 = await blob_client.read_reference(
                        minted.reference_id, offset=16, length_bytes=16
                    )
                    self.assertEqual(chunk1.chunk, server.secret_blob[16:32])
                    rel = await blob_client.release_reference(minted.reference_id)
                    self.assertTrue(rel.released)

                    self.assertTrue(rpc.is_usable)
                    self.assertEqual(server.connection_count, 1)
                    self.assertGreaterEqual(server.request_count, 16)
                finally:
                    await rpc.close()
            finally:
                await server.close()


class TestTransportAndRetryResilience(unittest.IsolatedAsyncioTestCase):
    """Timeouts, half-frame EOF, connection poisoning, reconnect, and retry guards."""

    async def test_timeout_and_half_frame_poison_connection_until_reconnect(self) -> None:
        signer = Ed25519Signer(secrets.token_bytes(32))
        principal = secrets.token_bytes(16)
        with tempfile.TemporaryDirectory(dir="/tmp", prefix="nlos-py-") as tmp:
            sock = Path(tmp) / "daemon.sock"
            server = _FakeSabiServer(sock, signer)
            await server.start()
            try:
                cfg = TransportConfig(
                    connect_timeout=1.0,
                    read_timeout=0.1,
                    write_timeout=1.0,
                )
                for hook_mode, expected_codes in (
                    ("stall", {"TIMEOUT"}),
                    ("half_header", {"EOF", "READ"}),
                    ("half_body", {"EOF", "READ"}),
                ):
                    server.behavior_hook = hook_mode
                    rpc = await connect_authenticated(str(sock), signer, principal, cfg)
                    try:
                        wait_client = WaitClient(rpc, principal_id=principal)
                        with self.assertRaises(IpcError) as err:
                            await wait_client.list_waits()
                        self.assertIn(err.exception.code, expected_codes)
                        self.assertFalse(rpc.is_usable)

                        with self.assertRaises(IpcError) as poisoned_err:
                            await wait_client.list_waits()
                        self.assertEqual(
                            poisoned_err.exception.code, "CONNECTION_UNUSABLE"
                        )
                        self.assertIn("reconnect", str(poisoned_err.exception).lower())
                    finally:
                        await rpc.close()

                # Reconnect cleanly after poison
                server.behavior_hook = None
                async with await WaitClient.connect_authenticated(
                    str(sock), signer=signer, principal_id=principal, config=cfg
                ) as recovered:
                    self.assertTrue(recovered.is_usable)
                    listed = await recovered.list_waits()
                    self.assertEqual(listed.waits, ())
            finally:
                await server.close()

    async def test_unknown_wire_major_and_critical_extension_fail_closed(self) -> None:
        signer = Ed25519Signer(secrets.token_bytes(32))
        principal = secrets.token_bytes(16)
        with tempfile.TemporaryDirectory(dir="/tmp", prefix="nlos-py-") as tmp:
            sock = Path(tmp) / "daemon.sock"
            server = _FakeSabiServer(sock, signer)
            await server.start()
            try:
                for hook in ("unknown_major", "unknown_critical_ext"):
                    server.behavior_hook = hook
                    async with await WaitClient.connect_authenticated(
                        str(sock), signer=signer, principal_id=principal
                    ) as wait_client:
                        with self.assertRaises(IpcError) as err:
                            await wait_client.list_waits()
                        self.assertEqual(err.exception.code, "COMPATIBILITY")
            finally:
                await server.close()

    async def test_uncertain_error_same_idempotency_key_retry_rules(self) -> None:
        signer = Ed25519Signer(secrets.token_bytes(32))
        principal = secrets.token_bytes(16)
        with tempfile.TemporaryDirectory(dir="/tmp", prefix="nlos-py-") as tmp:
            sock = Path(tmp) / "daemon.sock"
            server = _FakeSabiServer(sock, signer)
            server.behavior_hook = "uncertain_once"
            await server.start()
            try:
                async with await LlmGatewayClient.connect_authenticated(
                    str(sock), signer=signer, principal_id=principal
                ) as llm:
                    key1 = secrets.token_bytes(16)
                    key2 = secrets.token_bytes(16)
                    deadline = default_deadline_monotonic_ns()
                    turns = [LlmChatTurn("user", "retry-safe")]

                    with self.assertRaises(LlmGatewayError) as exc_info:
                        await llm.chat(
                            "local-qwen",
                            turns,
                            idempotency_key=key1,
                            deadline_monotonic_ns=deadline,
                        )
                    err = exc_info.exception
                    self.assertIsInstance(err, SabiServiceError)
                    self.assertTrue(err.is_uncertain)
                    self.assertTrue(err.is_retryable)

                    # Different key -> rejected locally before send
                    with self.assertRaises(CommonSemanticsError) as diff_key:
                        await llm.chat(
                            "local-qwen",
                            turns,
                            idempotency_key=key2,
                            deadline_monotonic_ns=deadline,
                            retry_of=err,
                        )
                    self.assertEqual(diff_key.exception.code, "UNSAFE_RETRY")

                    # Same key but changed payload -> rejected locally
                    with self.assertRaises(CommonSemanticsError) as diff_body:
                        await llm.chat(
                            "local-qwen",
                            [LlmChatTurn("user", "mutated-prompt")],
                            idempotency_key=key1,
                            deadline_monotonic_ns=deadline,
                            retry_of=err,
                        )
                    self.assertEqual(diff_body.exception.code, "UNSAFE_RETRY")

                    # Same key + identical payload -> succeeds
                    res = await llm.chat(
                        "local-qwen",
                        turns,
                        idempotency_key=key1,
                        deadline_monotonic_ns=deadline,
                        retry_of=err,
                    )
                    self.assertEqual(res.content_utf8, "echo:retry-safe")

                    # DO_NOT_RETRY error cannot be retried even with identical args
                    server.behavior_hook = "do_not_retry"
                    with self.assertRaises(LlmGatewayError) as dnr:
                        await llm.chat(
                            "local-qwen",
                            turns,
                            idempotency_key=key2,
                            deadline_monotonic_ns=deadline,
                        )
                    self.assertFalse(dnr.exception.is_retryable)
                    with self.assertRaises(CommonSemanticsError) as denied:
                        await llm.chat(
                            "local-qwen",
                            turns,
                            idempotency_key=key2,
                            deadline_monotonic_ns=deadline,
                            retry_of=dnr.exception,
                        )
                    self.assertEqual(denied.exception.code, "UNSAFE_RETRY")
            finally:
                await server.close()


class TestSecurityAndBoundaryChecks(unittest.IsolatedAsyncioTestCase):
    """Fail-closed validation for recipient, alignment, SHA-256, 64KiB cap, and zero-leak."""

    async def test_llm_and_tool_mandatory_fields_and_64k_cap(self) -> None:
        signer = Ed25519Signer(secrets.token_bytes(32))
        principal = secrets.token_bytes(16)
        with tempfile.TemporaryDirectory(dir="/tmp", prefix="nlos-py-") as tmp:
            sock = Path(tmp) / "daemon.sock"
            server = _FakeSabiServer(sock, signer)
            await server.start()
            try:
                async with await LlmGatewayClient.connect_authenticated(
                    str(sock), signer=signer, principal_id=principal
                ) as llm:
                    tool = ToolSandboxClient(llm._target, principal_id=principal)
                    deadline = default_deadline_monotonic_ns()
                    key = secrets.token_bytes(16)

                    # Missing idempotency_key on side-effecting calls
                    for call in (
                        llm.chat(
                            "m", [LlmChatTurn("user", "hi")], deadline_monotonic_ns=deadline
                        ),
                        llm.attenuate_budget(
                            LlmUsage(total_tokens=10), deadline_monotonic_ns=deadline
                        ),
                        llm.record_external_usage(
                            "p", "m", LlmUsage(), deadline_monotonic_ns=deadline
                        ),
                        tool.invoke_tool("t", "{}", deadline_monotonic_ns=deadline),
                    ):
                        with self.assertRaises(CommonSemanticsError) as exc:
                            await call
                        self.assertEqual(exc.exception.code, "MISSING_IDEMPOTENCY_KEY")

                    # Missing deadline on LlmGateway side-effecting & ToolSandbox calls
                    for call in (
                        llm.chat("m", [LlmChatTurn("user", "hi")], idempotency_key=key),
                        llm.attenuate_budget(
                            LlmUsage(total_tokens=10), idempotency_key=key
                        ),
                        llm.record_external_usage(
                            "p", "m", LlmUsage(), idempotency_key=key
                        ),
                        tool.invoke_tool("t", "{}", idempotency_key=key),
                        tool.list_tools(deadline_monotonic_ns=0),
                    ):
                        with self.assertRaises(CommonSemanticsError) as exc:
                            await call
                        self.assertEqual(exc.exception.code, "MISSING_DEADLINE")

                    # 64 KiB frame cap on requests
                    with self.assertRaises(IpcError) as big_llm:
                        await llm.chat(
                            "m",
                            [LlmChatTurn("user", "A" * 65_600)],
                            idempotency_key=key,
                            deadline_monotonic_ns=deadline,
                        )
                    self.assertEqual(big_llm.exception.code, "FRAME_TOO_LARGE")

                    with self.assertRaises(IpcError) as big_tool:
                        await tool.invoke_tool(
                            "t",
                            '{"k":"' + ("B" * 65_600) + '"}',
                            idempotency_key=key,
                            deadline_monotonic_ns=deadline,
                        )
                    self.assertEqual(big_tool.exception.code, "FRAME_TOO_LARGE")
            finally:
                await server.close()

    async def test_blob_boundaries_holder_mismatch_alignment_sha256_zero_leak(
        self,
    ) -> None:
        signer = Ed25519Signer(secrets.token_bytes(32))
        holder_a = secrets.token_bytes(16)
        holder_b = secrets.token_bytes(16)
        while holder_b == holder_a:
            holder_b = secrets.token_bytes(16)

        with tempfile.TemporaryDirectory(dir="/tmp", prefix="nlos-py-") as tmp:
            sock = Path(tmp) / "daemon.sock"
            server = _FakeSabiServer(sock, signer)
            await server.start()
            try:
                async with await BlobReferenceClient.connect_authenticated(
                    str(sock), signer=signer, principal_id=holder_a
                ) as client_a:
                    file_digest = hashlib.sha256(server.secret_blob).digest()

                    # Missing or wrong-length recipient_principal_id rejected
                    with self.assertRaises(CommonSemanticsError) as missing_rec:
                        await client_a.mint_reference("/f", file_digest, 64)
                    self.assertEqual(
                        missing_rec.exception.code, "MISSING_RECIPIENT_PRINCIPAL"
                    )

                    with self.assertRaises(CommonSemanticsError) as short_rec:
                        await client_a.mint_reference(
                            "/f", file_digest, 64, recipient_principal_id=b"\x01" * 15
                        )
                    self.assertEqual(short_rec.exception.code, "INVALID_ID")

                    # Invalid max_chunk_bytes bounds (0 and > 65408)
                    with self.assertRaises(IpcError) as zero_chunk:
                        await client_a.mint_reference(
                            "/f",
                            file_digest,
                            64,
                            max_chunk_bytes=0,
                            recipient_principal_id=holder_a,
                        )
                    self.assertEqual(zero_chunk.exception.code, "INVALID_CHUNK_SIZE")

                    with self.assertRaises(IpcError) as over_chunk:
                        await client_a.mint_reference(
                            "/f",
                            file_digest,
                            100_000,
                            max_chunk_bytes=MAX_BLOB_CHUNK_BYTES + 1,
                            recipient_principal_id=holder_a,
                        )
                    self.assertEqual(over_chunk.exception.code, "FRAME_TOO_LARGE")

                    # Mint for holder_b while connected as holder_a -> holder mismatch rejected
                    minted_for_b = await client_a.mint_reference(
                        "/f",
                        file_digest,
                        len(server.secret_blob),
                        max_reads=3,
                        max_chunk_bytes=16,
                        recipient_principal_id=holder_b,
                    )
                    with self.assertRaises(CommonSemanticsError) as mismatch:
                        await client_a.read_reference(
                            minted_for_b.reference_id, offset=0, length_bytes=16
                        )
                    self.assertEqual(mismatch.exception.code, "HOLDER_MISMATCH")

                    # Unaligned offset check
                    minted_for_a = await client_a.mint_reference(
                        "/f",
                        file_digest,
                        len(server.secret_blob),
                        max_reads=4,
                        max_chunk_bytes=16,
                        recipient_principal_id=holder_a,
                    )
                    with self.assertRaises(IpcError) as unaligned:
                        await client_a.read_reference(
                            minted_for_a.reference_id, offset=7, length_bytes=16
                        )
                    self.assertEqual(unaligned.exception.code, "UNALIGNED_OFFSET")

                    # Corrupted chunk SHA-256 fails closed without leaking secret bytes
                    server.behavior_hook = "bad_chunk_digest"
                    with self.assertRaises(IpcError) as sha_err:
                        await client_a.read_reference(
                            minted_for_a.reference_id, offset=0, length_bytes=16
                        )
                    self.assertEqual(sha_err.exception.code, "INTEGRITY_ERROR")
                    self.assertNotIn("BLOB_CANARY", str(sha_err.exception))
                    self.assertNotIn(
                        server.secret_blob[:16].decode("utf-8"),
                        str(sha_err.exception),
                    )

                    # Server sending > 65536 encoded ReadReferenceResult fails closed
                    server.behavior_hook = "oversize_blob_resp"
                    with self.assertRaises(IpcError) as oversized:
                        await client_a.read_reference(
                            minted_for_a.reference_id, offset=0, length_bytes=16
                        )
                    self.assertEqual(oversized.exception.code, "FRAME_TOO_LARGE")

                    # Direct ReadReferenceResult repr redaction check
                    sample = ReadReferenceResult(
                        chunk=server.secret_blob,
                        offset=0,
                        chunk_digest_sha256=hashlib.sha256(server.secret_blob).digest(),
                    )
                    self.assertNotIn("BLOB_CANARY", repr(sample) + str(sample))
            finally:
                await server.close()


if __name__ == "__main__":
    unittest.main()
