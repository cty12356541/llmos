"""Minute-scale cross-language TakeoverControl IPC soak.

The bounded conformance matrix intentionally remains fast.  This independent
entry point keeps a configurable set of real Unix-socket/named-pipe clients
connected before each unary call, repeats the same idempotent mutation over
bounded server rounds, and requires every response to carry the same durable
record.  The Rust conformance server performs the final one-row/LocallyCovered
check before it exits.
"""

from __future__ import annotations

import argparse
import asyncio
import math
import os
import shutil
import signal
import sys
import tempfile
import time
from dataclasses import dataclass
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

DEFAULT_DURATION_MS = 60_000
DEFAULT_ROUNDS = 8
DEFAULT_CONNECTIONS = 32


@dataclass(frozen=True, slots=True)
class SoakOptions:
    duration_ms: int
    rounds: int
    connections: int


def endpoint() -> str:
    # Keep the Unix path deliberately short: macOS/Linux sockaddr_un paths
    # have a small SUN_LEN bound, while tmpdir() may already contain a long
    # prefix.
    unique = f"{os.getpid()}-{time.time_ns()}"
    if sys.platform == "win32":
        return rf"\\.\pipe\nlos-takeover-{unique}"
    return str(Path(tempfile.gettempdir()) / f"nlos-takeover-{unique}-ls.sock")


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
    options: SoakOptions,
) -> tuple[asyncio.subprocess.Process, dict[str, str]]:
    process_environment = os.environ.copy()
    process_environment.pop("NLOS_TAKEOVER_CONTROL_HOLD_BEFORE_COMMIT", None)
    process_environment.pop("NLOS_TAKEOVER_CONTROL_HOLD_AFTER_COMMIT", None)
    process_environment.pop("NLOS_TAKEOVER_CONTROL_TRUNCATE_WAL_AFTER_COMMIT", None)
    process_environment.pop("NLOS_TAKEOVER_CONTROL_RANDOM_CRASH_PHASE", None)
    process_environment.pop("NLOS_TAKEOVER_CONTROL_RANDOM_CRASH_SEED", None)
    process_environment["NLOS_TAKEOVER_CONTROL_CONNECTIONS"] = str(options.connections)
    process_environment["NLOS_TAKEOVER_CONTROL_ROUNDS"] = str(options.rounds)
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
    try:
        ready = await asyncio.wait_for(process.stdout.readline(), timeout=60)
        if ready.strip() != b"READY":
            assert process.stderr is not None
            diagnostics = await process.stderr.read()
            raise AssertionError(
                f"{ready!r}: {diagnostics.decode('utf-8', errors='replace')}"
            )
        fixture_line = await asyncio.wait_for(process.stdout.readline(), timeout=60)
        return process, parse_fixture(fixture_line)
    except BaseException:
        if process.returncode is None:
            process.kill()
            await process.wait()
        raise


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


def assert_success(
    response: ExchangeResponse,
    request: ExchangeRequest,
    fixture: dict[str, str],
) -> bytes:
    context = validate_response_context(
        response.envelope,
        MethodSemantics(side_effecting=True, long_running=False),
    )
    assert not context.HasField("failure")
    assert response.envelope.request_id == request.envelope.request_id
    assert context.correlation_id == request.envelope.request_context.correlation_id
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
    # Compare the exact payload bytes returned by the durable handler, not a
    # re-encoded projection that could discard unknown protobuf fields.
    return bytes(response.envelope.payload)


def env_integer(name: str, fallback: int) -> int:
    raw = os.environ.get(name, str(fallback))
    try:
        value = int(raw)
    except ValueError as error:
        raise ValueError(f"{name} must be an integer, got {raw!r}") from error
    return value


def parse_options() -> SoakOptions:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--duration-ms",
        type=int,
        default=env_integer(
            "NLOS_TAKEOVER_CONTROL_LONG_SOAK_DURATION_MS", DEFAULT_DURATION_MS
        ),
    )
    parser.add_argument(
        "--rounds",
        type=int,
        default=env_integer("NLOS_TAKEOVER_CONTROL_LONG_SOAK_ROUNDS", DEFAULT_ROUNDS),
    )
    parser.add_argument(
        "--connections",
        type=int,
        default=env_integer(
            "NLOS_TAKEOVER_CONTROL_LONG_SOAK_CONNECTIONS", DEFAULT_CONNECTIONS
        ),
    )
    arguments = parser.parse_args()
    if not 1 <= arguments.duration_ms <= 2_147_483_647:
        raise ValueError("duration must be within 1..=2^31-1 ms")
    if not 1 <= arguments.rounds <= 8:
        raise ValueError("rounds must be within 1..=8")
    if not 2 <= arguments.connections <= 32:
        raise ValueError("connections must be within 2..=32")
    return SoakOptions(arguments.duration_ms, arguments.rounds, arguments.connections)


async def run(options: SoakOptions) -> None:
    # The conformance server uses the normal bounded transport read timeout;
    # cap the idle connection hold below that timeout and repeat fresh fixture
    # sessions until the requested aggregate pressure duration is reached.
    hold_ms = min(1_000, max(1, math.ceil(options.duration_ms / options.rounds)))
    hold_seconds = hold_ms / 1_000
    config = TransportConfig(
        connect_timeout=5,
        read_timeout=max(5, hold_seconds + 5),
        write_timeout=5,
    )
    pressure_ms = 0
    session_count = 0
    completed_sessions = 0
    started_at = time.monotonic()
    while pressure_ms < options.duration_ms or session_count == 0:
        socket = endpoint()
        stamp = f"{os.getpid()}-{time.time_ns()}-{session_count}"
        authority_path = str(
            Path(tempfile.gettempdir()) / f"nlos-takeover-{stamp}-long-soak.sqlite3"
        )
        identity_path = str(
            Path(tempfile.gettempdir()) / f"nlos-takeover-{stamp}-long-soak-identity"
        )
        process: asyncio.subprocess.Process | None = None
        active_clients: list[LocalRpcClient] = []
        try:
            process, fixture = await start_server(
                socket, authority_path, identity_path, options
            )
            first_record_wire: bytes | None = None
            for round_index in range(options.rounds):
                round_clients = list(
                    await asyncio.gather(
                        *(
                            LocalRpcClient.connect(socket, config)
                            for _ in range(options.connections)
                        )
                    )
                )
                active_clients.extend(round_clients)
                try:
                    # Keep every real endpoint connection open simultaneously.
                    # The server remains in this round until all unary handlers
                    # finish.
                    await asyncio.sleep(hold_seconds)
                    requests = [
                        submit_request(
                            fixture, 0x20 + round_index * options.connections + index
                        )
                        for index in range(options.connections)
                    ]
                    responses = await asyncio.gather(
                        *(
                            client.exchange(request)
                            for client, request in zip(round_clients, requests)
                        )
                    )
                    for index, (request, response) in enumerate(
                        zip(requests, responses)
                    ):
                        record_wire = assert_success(response, request, fixture)
                        if first_record_wire is None:
                            first_record_wire = record_wire
                        else:
                            assert record_wire == first_record_wire, (
                                round_index,
                                index,
                                "durable record differs",
                            )
                        completed_sessions += 1
                finally:
                    for client in round_clients:
                        await client.close()
                        active_clients.remove(client)
            assert process is not None
            exit_code = await process.wait()
            assert exit_code == 0, f"long-soak server failed with exit code {exit_code}"
            pressure_ms += hold_ms * options.rounds
            session_count += 1
        finally:
            for client in active_clients:
                await client.close()
            if process is not None and process.returncode is None:
                if sys.platform == "win32":
                    process.kill()
                else:
                    process.send_signal(signal.SIGKILL)
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

    elapsed_ms = round((time.monotonic() - started_at) * 1_000)
    assert pressure_ms >= options.duration_ms, (
        f"long-soak pressure {pressure_ms}ms is shorter than requested "
        f"{options.duration_ms}ms"
    )
    print(
        f"LONG_SOAK duration_ms={options.duration_ms} elapsed_ms={elapsed_ms} "
        f"pressure_ms={pressure_ms} rounds={options.rounds} "
        f"connections={options.connections} sessions={session_count} "
        f"calls={completed_sessions} durable_rows=1 coverage=LocallyCovered"
    )


if __name__ == "__main__":
    asyncio.run(run(parse_options()))
