"""Cross-language TakeoverControl request/response and durable replay."""

from __future__ import annotations

import asyncio
import os
import shutil
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / "sdk" / "python"))
sys.path.insert(0, str(ROOT / "gen" / "python"))

from nlos.sabi.v1.envelope_pb2 import ExchangeRequest, ExchangeResponse  # noqa: E402
from nlos.sabi.v1.takeover_control_pb2 import (  # noqa: E402
    BarrierObservationRecord,
    SubmitBarrierObservationRequest,
)
from nlos_sdk import (  # noqa: E402
    LocalRpcClient,
    MethodSemantics,
    TransportConfig,
    validate_response_context,
)


def endpoint(label: str) -> str:
    unique = f"{os.getpid()}-{time.time_ns()}-{label}"
    if sys.platform == "win32":
        return rf"\\.\pipe\nlos-takeover-{unique}"
    return str(Path(tempfile.gettempdir()) / f"nlos-takeover-{unique}.sock")


def parse_fixture(line: bytes) -> dict[str, str]:
    text = line.decode("utf-8").strip()
    assert text.startswith("FIXTURE "), text
    result: dict[str, str] = {}
    for field in text.removeprefix("FIXTURE ").split():
        key, separator, value = field.partition("=")
        assert separator and key and value, field
        result[key] = value
    return result


def fixture_bytes(fixture: dict[str, str], key: str) -> bytes:
    value = fixture[key]
    result = bytes.fromhex(value)
    assert len(result) > 0, key
    return result


async def start_server(
    socket: str,
    authority_path: str,
    identity_path: str,
) -> tuple[asyncio.subprocess.Process, dict[str, str]]:
    process = await asyncio.create_subprocess_exec(
        "cargo",
        "run",
        "--quiet",
        "-p",
        "nlos-takeover-control",
        "--features",
        "conformance-server",
        "--bin",
        "takeover-control-conformance",
        "--",
        socket,
        authority_path,
        identity_path,
        cwd=ROOT,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )
    assert process.stdout is not None
    ready = await asyncio.wait_for(process.stdout.readline(), timeout=60)
    assert ready.strip() == b"READY", ready
    fixture_line = await asyncio.wait_for(process.stdout.readline(), timeout=60)
    return process, parse_fixture(fixture_line)


def submit_request(fixture: dict[str, str]) -> ExchangeRequest:
    payload = SubmitBarrierObservationRequest()
    payload.schema.name = "nlos.sabi.TakeoverControl"
    payload.schema.major = 1
    payload.schema.minor = 0
    payload.target.takeover_receipt_id = fixture_bytes(fixture, "takeover_receipt_id")
    payload.target.participant_type = int(fixture["participant_type"])
    payload.target.participant_id = fixture_bytes(fixture, "participant_id")
    payload.target.participant_generation = int(fixture["participant_generation"])
    payload.target.admission_receipt_id = fixture_bytes(fixture, "admission_receipt_id")
    payload.evidence.remote_receipt_id = fixture_bytes(fixture, "remote_receipt_id")
    payload.evidence.barrier_digest = fixture_bytes(fixture, "barrier_digest")
    payload.evidence.observed_at_ms = int(fixture["observed_at_ms"])
    payload.signature.signer_principal_id = fixture_bytes(fixture, "signer_principal_id")
    payload.signature.signer_control_domain_id = fixture_bytes(
        fixture, "signer_control_domain_id"
    )
    payload.signature.signer_key_id = fixture_bytes(fixture, "signer_key_id")
    payload.signature.signature = fixture_bytes(fixture, "signature")

    request = ExchangeRequest()
    request.envelope.schema.name = "nlos.sabi.Envelope"
    request.envelope.schema.major = 1
    request.envelope.schema.minor = 1
    request.envelope.request_id = bytes([0xD1]) * 16
    request.envelope.service = "takeover_control"
    request.envelope.method = "submit_barrier_observation"
    context = request.envelope.request_context
    context.caller.principal_id = bytes([0xD4]) * 16
    context.caller.application_id = bytes([0xD5]) * 16
    context.caller.process_id = bytes([0xD6]) * 16
    context.caller.process_generation = 1
    context.correlation_id = bytes([0xD2]) * 16
    context.idempotency_key = bytes([0xD3]) * 16
    context.deadline_monotonic_ns = 1_000
    capability = context.capability_handles.add()
    capability.slot = 5
    capability.generation = 1
    request.envelope.payload = payload.SerializeToString()
    return request


def assert_success(response: ExchangeResponse, fixture: dict[str, str]) -> None:
    context = validate_response_context(
        response.envelope,
        MethodSemantics(side_effecting=True, long_running=False),
    )
    assert not context.HasField("failure")
    assert len(context.receipts) == 1
    record = BarrierObservationRecord()
    record.ParseFromString(response.envelope.payload)
    assert record.signed
    assert record.participant_id == fixture_bytes(fixture, "participant_id")
    assert record.barrier_digest == fixture_bytes(fixture, "barrier_digest")
    assert record.observed_at_ms == int(fixture["observed_at_ms"])
    assert record.signer_principal_id == fixture_bytes(fixture, "signer_principal_id")
    assert record.signer_key_id == fixture_bytes(fixture, "signer_key_id")
    assert record.signer_key_generation == 1
    assert context.receipts[0].receipt_id == record.receipt_id


async def main() -> None:
    socket = endpoint("py")
    authority_path = str(
        Path(tempfile.gettempdir())
        / f"nlos-takeover-{os.getpid()}-{time.time_ns()}.sqlite3"
    )
    identity_path = str(
        Path(tempfile.gettempdir())
        / f"nlos-takeover-{os.getpid()}-{time.time_ns()}-identity"
    )
    process: asyncio.subprocess.Process | None = None
    try:
        process, fixture = await start_server(socket, authority_path, identity_path)
        request = submit_request(fixture)
        config = TransportConfig(connect_timeout=2, read_timeout=2, write_timeout=2)
        client = await LocalRpcClient.connect(socket, config)
        response = await client.exchange(request)
        assert_success(response, fixture)
        await client.close()

        replay_client = await LocalRpcClient.connect(socket, config)
        replay = await replay_client.exchange(request)
        assert_success(replay, fixture)
        assert response.SerializeToString() == replay.SerializeToString()
        await replay_client.close()
        assert await process.wait() == 0
    finally:
        if process is not None and process.returncode is None:
            process.kill()
            await process.wait()
        for path in (
            socket,
            authority_path,
            f"{authority_path}-wal",
            f"{authority_path}-shm",
        ):
            try:
                Path(path).unlink()
            except FileNotFoundError:
                pass
        shutil.rmtree(identity_path, ignore_errors=True)


if __name__ == "__main__":
    asyncio.run(main())
