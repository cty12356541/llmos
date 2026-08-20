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
    IpcError,
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
    environment: dict[str, str] | None = None,
) -> tuple[asyncio.subprocess.Process, dict[str, str]]:
    process_environment = os.environ.copy()
    process_environment.pop("NLOS_TAKEOVER_CONTROL_HOLD_AFTER_COMMIT", None)
    if environment is not None:
        process_environment.update(environment)
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
        env=process_environment,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )
    assert process.stdout is not None
    ready = await asyncio.wait_for(process.stdout.readline(), timeout=60)
    if ready.strip() != b"READY":
        assert process.stderr is not None
        diagnostics = await process.stderr.read()
        raise AssertionError(f"{ready!r}: {diagnostics.decode('utf-8', errors='replace')}")
    fixture_line = await asyncio.wait_for(process.stdout.readline(), timeout=60)
    return process, parse_fixture(fixture_line)


async def wait_for_server_line(
    process: asyncio.subprocess.Process, prefix: bytes
) -> bytes:
    assert process.stdout is not None
    line = await asyncio.wait_for(process.stdout.readline(), timeout=60)
    assert line.startswith(prefix), line
    return line


def submit_request(fixture: dict[str, str], request_seed: int = 0xD1) -> ExchangeRequest:
    byte = lambda value: bytes([value & 0xFF]) * 16
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
    request.envelope.request_id = byte(request_seed)
    request.envelope.service = "takeover_control"
    request.envelope.method = "submit_barrier_observation"
    context = request.envelope.request_context
    context.caller.principal_id = byte(request_seed + 3)
    context.caller.application_id = byte(request_seed + 4)
    context.caller.process_id = byte(request_seed + 5)
    context.caller.process_generation = 1
    context.correlation_id = byte(request_seed + 1)
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


async def run_crash_restart() -> None:
    # Keep Unix socket names below SUN_LEN on macOS/Linux even under a long
    # temporary-directory prefix.
    crash_socket = endpoint("pc")
    restart_socket = endpoint("pr")
    authority_path = str(
        Path(tempfile.gettempdir())
        / f"nlos-takeover-{os.getpid()}-{time.time_ns()}-crash.sqlite3"
    )
    identity_path = str(
        Path(tempfile.gettempdir())
        / f"nlos-takeover-{os.getpid()}-{time.time_ns()}-crash-identity"
    )
    process: asyncio.subprocess.Process | None = None
    client: LocalRpcClient | None = None
    try:
        process, fixture = await start_server(
            crash_socket,
            authority_path,
            identity_path,
            {"NLOS_TAKEOVER_CONTROL_HOLD_AFTER_COMMIT": "1"},
        )
        request = submit_request(fixture)
        config = TransportConfig(connect_timeout=2, read_timeout=2, write_timeout=2)
        client = await LocalRpcClient.connect(crash_socket, config)
        in_flight = asyncio.create_task(client.exchange(request))
        await wait_for_server_line(process, b"COMMIT_READY")
        process.kill()
        crash_code = await process.wait()
        assert crash_code != 0
        try:
            await in_flight
        except IpcError:
            pass
        else:
            raise AssertionError("crashed TakeoverControl request unexpectedly returned")
        await client.close()
        client = None

        process, recovered_fixture = await start_server(
            restart_socket, authority_path, identity_path
        )
        assert recovered_fixture == fixture
        recovery_request = submit_request(fixture)
        recovery_client = await LocalRpcClient.connect(restart_socket, config)
        recovered = await recovery_client.exchange(recovery_request)
        assert_success(recovered, recovered_fixture)
        await recovery_client.close()

        replay_client = await LocalRpcClient.connect(restart_socket, config)
        replay = await replay_client.exchange(recovery_request)
        assert_success(replay, recovered_fixture)
        assert recovered.SerializeToString() == replay.SerializeToString()
        await replay_client.close()
        assert await process.wait() == 0
    finally:
        if client is not None:
            await client.close()
        if process is not None and process.returncode is None:
            process.kill()
            await process.wait()
        for path in (
            crash_socket,
            restart_socket,
            authority_path,
            f"{authority_path}-wal",
            f"{authority_path}-shm",
        ):
            try:
                Path(path).unlink()
            except FileNotFoundError:
                pass
        shutil.rmtree(identity_path, ignore_errors=True)


async def run_concurrent_pressure() -> None:
    socket = endpoint("pp")
    authority_path = str(
        Path(tempfile.gettempdir())
        / f"nlos-takeover-{os.getpid()}-{time.time_ns()}-pressure.sqlite3"
    )
    identity_path = str(
        Path(tempfile.gettempdir())
        / f"nlos-takeover-{os.getpid()}-{time.time_ns()}-pressure-identity"
    )
    process: asyncio.subprocess.Process | None = None
    clients: list[LocalRpcClient] = []
    try:
        process, fixture = await start_server(
            socket,
            authority_path,
            identity_path,
            {"NLOS_TAKEOVER_CONTROL_CONNECTIONS": "8"},
        )
        config = TransportConfig(connect_timeout=2, read_timeout=5, write_timeout=5)
        for _ in range(8):
            clients.append(await LocalRpcClient.connect(socket, config))
        responses = await asyncio.gather(
            *(
                client.exchange(submit_request(fixture, 0xD1 + index))
                for index, client in enumerate(clients)
            )
        )
        first_record = BarrierObservationRecord()
        first_record.ParseFromString(responses[0].envelope.payload)
        first_record_wire = first_record.SerializeToString()
        for index, response in enumerate(responses):
            assert_success(response, fixture)
            record = BarrierObservationRecord()
            record.ParseFromString(response.envelope.payload)
            assert record.SerializeToString() == first_record_wire, index
        for client in clients:
            await client.close()
        assert await process.wait() == 0
    finally:
        for client in clients:
            await client.close()
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


async def run_torn_wal_recovery() -> None:
    torn_socket = endpoint("tw")
    restart_socket = endpoint("tr")
    authority_path = str(
        Path(tempfile.gettempdir())
        / f"nlos-takeover-{os.getpid()}-{time.time_ns()}-torn.sqlite3"
    )
    identity_path = str(
        Path(tempfile.gettempdir())
        / f"nlos-takeover-{os.getpid()}-{time.time_ns()}-torn-identity"
    )
    process: asyncio.subprocess.Process | None = None
    client: LocalRpcClient | None = None
    try:
        process, fixture = await start_server(
            torn_socket,
            authority_path,
            identity_path,
            {
                "NLOS_TAKEOVER_CONTROL_HOLD_AFTER_COMMIT": "1",
                "NLOS_TAKEOVER_CONTROL_TRUNCATE_WAL_AFTER_COMMIT": "1",
            },
        )
        request = submit_request(fixture)
        config = TransportConfig(connect_timeout=2, read_timeout=2, write_timeout=2)
        client = await LocalRpcClient.connect(torn_socket, config)
        in_flight = asyncio.create_task(client.exchange(request))
        await wait_for_server_line(process, b"COMMIT_READY")
        await wait_for_server_line(process, b"WAL_TORN_READY")
        process.kill()
        crash_code = await process.wait()
        assert crash_code != 0
        try:
            await in_flight
        except IpcError:
            pass
        else:
            raise AssertionError("torn WAL request unexpectedly returned")
        await client.close()
        client = None
        Path(f"{authority_path}-shm").unlink(missing_ok=True)

        process, recovered_fixture = await start_server(
            restart_socket, authority_path, identity_path
        )
        assert recovered_fixture == fixture
        recovery_client = await LocalRpcClient.connect(restart_socket, config)
        response = await recovery_client.exchange(request)
        assert_success(response, recovered_fixture)
        await recovery_client.close()

        replay_client = await LocalRpcClient.connect(restart_socket, config)
        replay = await replay_client.exchange(request)
        assert_success(replay, recovered_fixture)
        assert response.SerializeToString() == replay.SerializeToString()
        await replay_client.close()
        assert await process.wait() == 0
    finally:
        if client is not None:
            await client.close()
        if process is not None and process.returncode is None:
            process.kill()
            await process.wait()
        for path in (
            torn_socket,
            restart_socket,
            authority_path,
            f"{authority_path}-wal",
            f"{authority_path}-shm",
        ):
            try:
                Path(path).unlink()
            except FileNotFoundError:
                pass
        shutil.rmtree(identity_path, ignore_errors=True)


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

    await run_crash_restart()
    await run_concurrent_pressure()
    await run_torn_wal_recovery()


if __name__ == "__main__":
    asyncio.run(main())
