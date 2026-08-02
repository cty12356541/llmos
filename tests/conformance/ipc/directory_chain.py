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
    SABI_ERROR_CODE_CONFLICT,
    SABI_ERROR_CODE_UNCERTAIN,
    ExchangeRequest,
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

        assert await server.wait() == 0
    except BaseException:
        server.kill()
        await server.wait()
        raise


if __name__ == "__main__":
    asyncio.run(main())
