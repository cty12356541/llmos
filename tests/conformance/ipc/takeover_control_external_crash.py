"""External-timing process-crash conformance for TakeoverControl.

The supervisor is this client process, not the feature-gated server.  It does
not set any server crash hook: a short OS-CSPRNG delay is sampled after the
request is submitted, the child is terminated, and the same authority,
identity, and idempotency key are used after restart.

This proves process-level external termination and replay convergence only.
It does not claim a particular commit phase, power loss, time-window
anti-replay, or production Capability/peer-attestation enforcement.
"""

from __future__ import annotations

import asyncio
import os
import secrets
import shutil
import signal
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

MAX_EXTERNAL_DELAY_MS = 64
HOOK_ENVIRONMENT_KEYS = (
    "NLOS_TAKEOVER_CONTROL_HOLD_BEFORE_COMMIT",
    "NLOS_TAKEOVER_CONTROL_HOLD_AFTER_COMMIT",
    "NLOS_TAKEOVER_CONTROL_TRUNCATE_WAL_AFTER_COMMIT",
    "NLOS_TAKEOVER_CONTROL_RANDOM_CRASH_PHASE",
    "NLOS_TAKEOVER_CONTROL_RANDOM_CRASH_SEED",
    "NLOS_TAKEOVER_CONTROL_CONNECTIONS",
    "NLOS_TAKEOVER_CONTROL_ROUNDS",
)


def parse_trials() -> int:
    if len(sys.argv) == 1:
        return 8
    assert len(sys.argv) == 3 and sys.argv[1] == "--trials", "usage: --trials N"
    trials = int(sys.argv[2])
    assert 0 < trials <= 32
    return trials


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
    socket: str, authority_path: str, identity_path: str
) -> tuple[asyncio.subprocess.Process, dict[str, str]]:
    process_environment = os.environ.copy()
    for key in HOOK_ENVIRONMENT_KEYS:
        process_environment.pop(key, None)
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


def submit_request(fixture: dict[str, str], request_seed: int) -> ExchangeRequest:
    def byte(value: int) -> bytes:
        return bytes([value & 0xFF]) * 16

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


def terminate_process(process: asyncio.subprocess.Process, force: bool) -> str:
    if sys.platform == "win32":
        process.kill()
        return "TerminateProcess"
    if force:
        process.send_signal(signal.SIGKILL)
        return "SIGKILL"
    process.terminate()
    return "SIGTERM"


async def run_trial(trial: int, trials: int) -> None:
    crash_socket = endpoint("ec")
    restart_socket = endpoint("er")
    suffix = f"{os.getpid()}-{time.time_ns()}-{trial}"
    authority_path = str(
        Path(tempfile.gettempdir()) / f"nlos-takeover-{suffix}-external.sqlite3"
    )
    identity_path = str(
        Path(tempfile.gettempdir()) / f"nlos-takeover-{suffix}-external-identity"
    )
    process: asyncio.subprocess.Process | None = None
    client: LocalRpcClient | None = None
    delay_ms = secrets.randbelow(MAX_EXTERNAL_DELAY_MS + 1)
    force = sys.platform != "win32" and trial % 2 == 1
    initial_state = "transport_error"
    try:
        process, fixture = await start_server(crash_socket, authority_path, identity_path)
        request = submit_request(fixture, 0xE1 + trial)
        config = TransportConfig(connect_timeout=2, read_timeout=2, write_timeout=2)
        client = await LocalRpcClient.connect(crash_socket, config)
        initial_exchange = asyncio.create_task(client.exchange(request))
        await asyncio.sleep(delay_ms / 1000)
        termination = terminate_process(process, force)
        crash_code = await process.wait()
        assert crash_code != 0, f"trial {trial} server unexpectedly exited cleanly"
        try:
            initial_response = await initial_exchange
        except IpcError:
            pass
        else:
            assert_success(initial_response, fixture)
            initial_state = "success"
        await client.close()
        client = None

        process, recovered_fixture = await start_server(
            restart_socket, authority_path, identity_path
        )
        assert recovered_fixture == fixture
        recovery_client = await LocalRpcClient.connect(restart_socket, config)
        recovered = await recovery_client.exchange(request)
        assert_success(recovered, recovered_fixture)
        await recovery_client.close()

        replay_client = await LocalRpcClient.connect(restart_socket, config)
        replay = await replay_client.exchange(request)
        assert_success(replay, recovered_fixture)
        assert recovered.SerializeToString() == replay.SerializeToString()
        await replay_client.close()
        assert await process.wait() == 0
        exit_label = f"code:{crash_code}"
        print(
            f"EXTERNAL_CRASH trial={trial + 1}/{trials} delay_ms={delay_ms} "
            f"termination={termination} initial={initial_state} "
            f"exit={exit_label} recovery=success replay=byte_equal",
            flush=True,
        )
    finally:
        if client is not None:
            await client.close()
        if process is not None and process.returncode is None:
            terminate_process(process, force)
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


async def main() -> None:
    trials = parse_trials()
    for trial in range(trials):
        await run_trial(trial, trials)
    print(f"EXTERNAL_CRASH_SUMMARY trials={trials} platform={sys.platform}", flush=True)


if __name__ == "__main__":
    asyncio.run(main())
