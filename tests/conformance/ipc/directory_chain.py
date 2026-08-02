"""Bootstrap through Rust ServiceDirectory, then call its negotiated service."""

from __future__ import annotations

import asyncio
import os
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / "sdk" / "python"))
sys.path.insert(0, str(ROOT / "gen" / "python"))

from nlos.sabi.v1.envelope_pb2 import (  # noqa: E402
    RETRY_DIRECTIVE_DO_NOT_RETRY,
    RETRY_DIRECTIVE_QUERY_OPERATION_OR_RETRY_SAME_IDEMPOTENCY_KEY,
    SABI_ERROR_CODE_CANCELLED,
    SABI_ERROR_CODE_CONFLICT,
    SABI_ERROR_CODE_DEADLINE,
    SABI_ERROR_CODE_EFFECT_UNKNOWN,
    SABI_ERROR_CODE_PARTIAL,
    SABI_ERROR_CODE_UNCERTAIN,
    ExchangeRequest,
)
from nlos.sabi.v1.operation_control_pb2 import (  # noqa: E402
    OPERATION_LIFECYCLE_STATE_CANCEL_REQUESTED,
    OPERATION_LIFECYCLE_STATE_CANCELLED_BEFORE_EFFECT,
    OPERATION_LIFECYCLE_STATE_DISPATCHED,
    OPERATION_LIFECYCLE_STATE_REGISTERED,
    CancelOperationRequest,
    OperationStatus,
    QueryOperationRequest,
)
from nlos_sdk import (  # noqa: E402
    IpcError,
    LocalRpcClient,
    MethodSemantics,
    ServiceDirectoryClient,
    ServiceRequirement,
    TransportConfig,
    validate_response_context,
)


def endpoint(label: str) -> str:
    unique = f"{os.getpid():x}-{time.time_ns() & 0xFFFFFFF:x}-{label[0]}"
    if sys.platform == "win32":
        return rf"\\.\pipe\nlos-directory-{unique}"
    return str(Path(tempfile.gettempdir()) / f"nlos-directory-{unique}.sock")


async def start_server(
    directory_endpoint: str,
    business_endpoint: str,
    authority_path: str,
    phase: str,
) -> asyncio.subprocess.Process:
    process = await asyncio.create_subprocess_exec(
        "cargo",
        "run",
        "--quiet",
        "-p",
        "nlos-ipc",
        "--features",
        "conformance-server",
        "--bin",
        "nlos-directory-chain",
        "--",
        directory_endpoint,
        business_endpoint,
        authority_path,
        phase,
        cwd=ROOT,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )
    assert process.stdout is not None
    ready = await asyncio.wait_for(process.stdout.readline(), timeout=60)
    if ready.strip() != b"READY":
        assert process.stderr is not None
        stderr = await process.stderr.read()
        raise AssertionError(f"directory chain failed before ready: {stderr!r}")
    return process


def business_request(
    request_seed: int,
    correlation_seed: int,
    method: str = "cancel",
    key_seed: int = 6,
    payload: bytes = b"\x04\x05\x06",
) -> ExchangeRequest:
    request = ExchangeRequest()
    request.envelope.schema.name = "nlos.sabi.Envelope"
    request.envelope.schema.major = 1
    request.envelope.schema.minor = 1
    request.envelope.request_id = bytes([request_seed]) * 16
    request.envelope.service = "operation"
    request.envelope.method = method
    request_context = request.envelope.request_context
    request_context.caller.principal_id = bytes([1]) * 16
    request_context.caller.application_id = bytes([2]) * 16
    request_context.caller.process_id = bytes([3]) * 16
    request_context.caller.process_generation = 7
    request_context.correlation_id = bytes([correlation_seed]) * 16
    request_context.idempotency_key = bytes([key_seed]) * 16
    request_context.deadline_monotonic_ns = 123_456
    capability = request_context.capability_handles.add()
    capability.slot = 11
    capability.generation = 2
    request.envelope.payload = payload
    return request


def operation_control_request(
    request_seed: int,
    correlation_seed: int,
    method: str,
    operation_id: bytes,
    generation: int,
    expected_cancel_epoch: int = 0,
) -> ExchangeRequest:
    if method == "query":
        payload = QueryOperationRequest()
    elif method == "cancel":
        payload = CancelOperationRequest()
        payload.expected_cancel_epoch = expected_cancel_epoch
    else:
        raise AssertionError(f"unsupported control method {method}")
    payload.schema.name = "nlos.sabi.OperationControl"
    payload.schema.major = 1
    payload.operation.operation_id = operation_id
    payload.operation.generation = generation

    request = ExchangeRequest()
    request.envelope.schema.name = "nlos.sabi.Envelope"
    request.envelope.schema.major = 1
    request.envelope.schema.minor = 1
    request.envelope.request_id = bytes([request_seed]) * 16
    request.envelope.service = "operation_control"
    request.envelope.method = method
    context = request.envelope.request_context
    context.caller.principal_id = bytes([1]) * 16
    context.caller.application_id = bytes([2]) * 16
    context.caller.process_id = bytes([3]) * 16
    context.caller.process_generation = 7
    context.correlation_id = bytes([correlation_seed]) * 16
    if method == "cancel":
        context.idempotency_key = bytes([0x71]) * 16
    capability = context.capability_handles.add()
    capability.slot = 11
    capability.generation = 2
    request.envelope.payload = payload.SerializeToString()
    return request


def operation_status(payload: bytes) -> OperationStatus:
    status = OperationStatus()
    status.ParseFromString(payload)
    assert status.HasField("operation")
    return status


async def main() -> None:
    directory_endpoint = endpoint("bootstrap")
    business_endpoint = endpoint("business")
    authority_path = str(
        Path(tempfile.gettempdir())
        / f"nlos-directory-{os.getpid()}-{time.time_ns()}-authority.sqlite3"
    )
    server = await start_server(
        directory_endpoint,
        business_endpoint,
        authority_path,
        "commit",
    )
    try:
        transport_config = TransportConfig(
            connect_timeout=2,
            read_timeout=2,
            write_timeout=2,
        )
        connected = await ServiceDirectoryClient.negotiate_and_connect(
            directory_endpoint,
            ServiceRequirement(
                service="operation",
                schema_name="nlos.sabi.Envelope",
                major=1,
                minimum_minor=1,
            ),
            transport_config,
        )
        assert connected.binding.endpoint.address == business_endpoint
        assert connected.binding.candidate.generation == 7

        try:
            await connected.client.exchange(business_request(9, 5))
        except IpcError as error:
            assert error.code == "READ"
        else:
            raise AssertionError("first committed exchange must disconnect")
        await connected.client.close()

        assert await server.wait() == 0
        server = await start_server(
            directory_endpoint,
            business_endpoint,
            authority_path,
            "recover",
        )
        recovered = await ServiceDirectoryClient.negotiate_and_connect(
            directory_endpoint,
            ServiceRequirement(
                service="operation",
                schema_name="nlos.sabi.Envelope",
                major=1,
                minimum_minor=1,
            ),
            transport_config,
        )
        assert recovered.binding.endpoint.address == business_endpoint
        retry_client = recovered.client
        response = await retry_client.exchange(business_request(10, 7))
        assert response.envelope.request_id == bytes([10]) * 16
        assert response.envelope.payload == b"\x04\x05\x06\xd0"
        response_context = validate_response_context(
            response.envelope,
            MethodSemantics(side_effecting=True, long_running=True),
        )
        assert response_context.correlation_id == bytes([7]) * 16
        assert response_context.operation.generation == 1
        assert response_context.receipts[0].receipt_id == bytes([0x99]) * 16
        await retry_client.close()

        conflict_client = await LocalRpcClient.connect(
            business_endpoint,
            transport_config,
        )
        conflict = await conflict_client.exchange(
            business_request(11, 8, "cancel", 6, b"\x04\x05\x07")
        )
        conflict_context = validate_response_context(
            conflict.envelope,
            MethodSemantics(side_effecting=True, long_running=True),
        )
        assert conflict_context.failure.code == SABI_ERROR_CODE_CONFLICT
        assert conflict_context.failure.retry == RETRY_DIRECTIVE_DO_NOT_RETRY
        assert (
            conflict_context.operation.operation_id
            == response_context.operation.operation_id
        )
        await conflict_client.close()

        pending_client = await LocalRpcClient.connect(
            business_endpoint,
            transport_config,
        )
        pending = await pending_client.exchange(
            business_request(12, 9, "pending", 7, b"\x01")
        )
        pending_context = validate_response_context(
            pending.envelope,
            MethodSemantics(side_effecting=True, long_running=True),
        )
        assert pending_context.failure.code == SABI_ERROR_CODE_UNCERTAIN
        assert (
            pending_context.failure.retry
            == RETRY_DIRECTIVE_QUERY_OPERATION_OR_RETRY_SAME_IDEMPOTENCY_KEY
        )
        assert pending_context.HasField("operation")
        await pending_client.close()

        pending_operation = pending_context.operation
        query_pending_client = await LocalRpcClient.connect(
            business_endpoint, transport_config
        )
        queried_pending = await query_pending_client.exchange(
            operation_control_request(
                18,
                15,
                "query",
                pending_operation.operation_id,
                pending_operation.generation,
            )
        )
        validate_response_context(
            queried_pending.envelope,
            MethodSemantics(side_effecting=False, long_running=False),
        )
        queried_pending_status = operation_status(queried_pending.envelope.payload)
        assert queried_pending_status.state == OPERATION_LIFECYCLE_STATE_DISPATCHED
        assert queried_pending_status.cancel_epoch == 0
        await query_pending_client.close()

        cancel_control_client = await LocalRpcClient.connect(
            business_endpoint, transport_config
        )
        cancelled_pending = await cancel_control_client.exchange(
            operation_control_request(
                19,
                16,
                "cancel",
                pending_operation.operation_id,
                pending_operation.generation,
            )
        )
        validate_response_context(
            cancelled_pending.envelope,
            MethodSemantics(side_effecting=True, long_running=False),
        )
        cancelled_pending_status = operation_status(
            cancelled_pending.envelope.payload
        )
        assert (
            cancelled_pending_status.state
            == OPERATION_LIFECYCLE_STATE_CANCEL_REQUESTED
        )
        assert cancelled_pending_status.cancel_epoch == 1
        await cancel_control_client.close()

        cancel_replay_client = await LocalRpcClient.connect(
            business_endpoint, transport_config
        )
        cancel_replay = await cancel_replay_client.exchange(
            operation_control_request(
                20,
                17,
                "cancel",
                pending_operation.operation_id,
                pending_operation.generation,
            )
        )
        cancel_replay_status = operation_status(cancel_replay.envelope.payload)
        assert cancel_replay_status.cancel_epoch == 1
        assert (
            cancel_replay_status.state
            == OPERATION_LIFECYCLE_STATE_CANCEL_REQUESTED
        )
        await cancel_replay_client.close()

        query_cancelled_client = await LocalRpcClient.connect(
            business_endpoint, transport_config
        )
        query_cancelled = await query_cancelled_client.exchange(
            operation_control_request(
                21,
                18,
                "query",
                pending_operation.operation_id,
                pending_operation.generation,
            )
        )
        query_cancelled_status = operation_status(query_cancelled.envelope.payload)
        assert query_cancelled_status.cancel_epoch == 1
        assert (
            query_cancelled_status.state
            == OPERATION_LIFECYCLE_STATE_CANCEL_REQUESTED
        )
        await query_cancelled_client.close()

        worker_deadline_client = await LocalRpcClient.connect(
            business_endpoint, transport_config
        )
        worker_deadline = await worker_deadline_client.exchange(
            business_request(22, 19, "worker_deadline", 12, b"\x06")
        )
        worker_deadline_context = validate_response_context(
            worker_deadline.envelope,
            MethodSemantics(side_effecting=True, long_running=True),
        )
        assert worker_deadline_context.failure.code == SABI_ERROR_CODE_UNCERTAIN
        assert worker_deadline_context.HasField("operation")
        await worker_deadline_client.close()

        worker_operation = worker_deadline_context.operation
        query_queued_client = await LocalRpcClient.connect(
            business_endpoint, transport_config
        )
        query_queued = await query_queued_client.exchange(
            operation_control_request(
                23,
                20,
                "query",
                worker_operation.operation_id,
                worker_operation.generation,
            )
        )
        query_queued_status = operation_status(query_queued.envelope.payload)
        assert query_queued_status.state == OPERATION_LIFECYCLE_STATE_REGISTERED
        assert query_queued_status.cancel_epoch == 0
        await query_queued_client.close()

        await asyncio.sleep(0.65)
        query_deadline_client = await LocalRpcClient.connect(
            business_endpoint, transport_config
        )
        query_deadline = await query_deadline_client.exchange(
            operation_control_request(
                24,
                21,
                "query",
                worker_operation.operation_id,
                worker_operation.generation,
            )
        )
        query_deadline_status = operation_status(query_deadline.envelope.payload)
        assert (
            query_deadline_status.state
            == OPERATION_LIFECYCLE_STATE_CANCELLED_BEFORE_EFFECT
        )
        assert query_deadline_status.cancel_epoch == 1
        assert query_deadline_status.receipt.receipt_id == bytes([0xA7]) * 16
        await query_deadline_client.close()

        deadline_before_client = await LocalRpcClient.connect(
            business_endpoint,
            transport_config,
        )
        deadline_before = await deadline_before_client.exchange(
            business_request(13, 10, "deadline_before_dispatch", 8, b"\x02")
        )
        deadline_before_context = validate_response_context(
            deadline_before.envelope,
            MethodSemantics(side_effecting=True, long_running=True),
        )
        assert deadline_before_context.failure.code == SABI_ERROR_CODE_DEADLINE
        assert (
            deadline_before_context.failure.retry
            == RETRY_DIRECTIVE_DO_NOT_RETRY
        )
        assert deadline_before_context.receipts[0].receipt_id == bytes([0xA1]) * 16
        await deadline_before_client.close()

        deadline_replay_client = await LocalRpcClient.connect(
            business_endpoint,
            transport_config,
        )
        deadline_replay = await deadline_replay_client.exchange(
            business_request(14, 11, "deadline_before_dispatch", 8, b"\x02")
        )
        deadline_replay_context = validate_response_context(
            deadline_replay.envelope,
            MethodSemantics(side_effecting=True, long_running=True),
        )
        assert deadline_replay_context.failure.code == SABI_ERROR_CODE_DEADLINE
        assert (
            deadline_replay_context.operation.operation_id
            == deadline_before_context.operation.operation_id
        )
        assert deadline_replay_context.correlation_id == bytes([11]) * 16
        await deadline_replay_client.close()

        cancel_before_client = await LocalRpcClient.connect(
            business_endpoint,
            transport_config,
        )
        cancel_before = await cancel_before_client.exchange(
            business_request(15, 12, "cancel_before_dispatch", 9, b"\x03")
        )
        cancel_before_context = validate_response_context(
            cancel_before.envelope,
            MethodSemantics(side_effecting=True, long_running=True),
        )
        assert cancel_before_context.failure.code == SABI_ERROR_CODE_CANCELLED
        assert cancel_before_context.failure.retry == RETRY_DIRECTIVE_DO_NOT_RETRY
        assert cancel_before_context.receipts[0].receipt_id == bytes([0xA2]) * 16
        await cancel_before_client.close()

        cancel_after_client = await LocalRpcClient.connect(
            business_endpoint,
            transport_config,
        )
        cancel_after = await cancel_after_client.exchange(
            business_request(16, 13, "cancel_after_dispatch", 10, b"\x04")
        )
        cancel_after_context = validate_response_context(
            cancel_after.envelope,
            MethodSemantics(side_effecting=True, long_running=True),
        )
        assert cancel_after_context.failure.code == SABI_ERROR_CODE_PARTIAL
        assert cancel_after_context.failure.retry == RETRY_DIRECTIVE_DO_NOT_RETRY
        assert cancel_after_context.receipts[0].receipt_id == bytes([0xA4]) * 16
        await cancel_after_client.close()

        deadline_after_client = await LocalRpcClient.connect(
            business_endpoint,
            transport_config,
        )
        deadline_after = await deadline_after_client.exchange(
            business_request(17, 14, "deadline_after_dispatch", 11, b"\x05")
        )
        deadline_after_context = validate_response_context(
            deadline_after.envelope,
            MethodSemantics(side_effecting=True, long_running=True),
        )
        assert (
            deadline_after_context.failure.code
            == SABI_ERROR_CODE_EFFECT_UNKNOWN
        )
        assert (
            deadline_after_context.failure.retry
            == RETRY_DIRECTIVE_QUERY_OPERATION_OR_RETRY_SAME_IDEMPOTENCY_KEY
        )
        assert deadline_after_context.receipts[0].receipt_id == bytes([0xA6]) * 16
        await deadline_after_client.close()

        assert await server.wait() == 0
    except BaseException:
        server.kill()
        await server.wait()
        raise


if __name__ == "__main__":
    asyncio.run(main())
