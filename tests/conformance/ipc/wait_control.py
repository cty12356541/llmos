"""Cross-language WaitControl request/response, durable replay, rejection shapes."""

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

from nlos.sabi.v1 import wait_control_pb2 as wc  # noqa: E402
from nlos.sabi.v1.envelope_pb2 import (  # noqa: E402
    RETRY_DIRECTIVE_DO_NOT_RETRY,
    SABI_ERROR_CODE_CONFLICT,
    SABI_ERROR_CODE_NOT_SUPPORTED,
    SABI_ERROR_CODE_RIGHTS,
    ExchangeRequest,
    ExchangeResponse,
)
from nlos_sdk import (  # noqa: E402
    LocalRpcClient,
    MethodSemantics,
    TransportConfig,
    validate_response_context,
)

CONNECTIONS_ENV = "NLOS_WAIT_CONTROL_CONNECTIONS"
ROUNDS_ENV = "NLOS_WAIT_CONTROL_ROUNDS"
SCENE_ENV = "NLOS_WAIT_CONTROL_SCENE"
CAPABILITY_SLOT = 9
CONFIG = TransportConfig(connect_timeout=2, read_timeout=5, write_timeout=5)


def endpoint(label: str) -> str:
    unique = f"{os.getpid()}-{time.time_ns()}-{label}"
    if sys.platform == "win32":
        return rf"\\.\pipe\nlos-wait-{unique}"
    return str(Path(tempfile.gettempdir()) / f"nlos-wait-{unique}.sock")


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


def filled(seed: int) -> bytes:
    return bytes([seed & 0xFF]) * 16


async def start_server(
    socket: str,
    authority_root: str,
    environment: dict[str, str] | None = None,
) -> tuple[asyncio.subprocess.Process, dict[str, str]]:
    process_environment = os.environ.copy()
    process_environment.pop(CONNECTIONS_ENV, None)
    process_environment.pop(ROUNDS_ENV, None)
    process_environment.pop(SCENE_ENV, None)
    if environment is not None:
        process_environment.update(environment)
    process = await asyncio.create_subprocess_exec(
        "cargo",
        "run",
        "--quiet",
        "-p",
        "nlos-wait-control",
        "--features",
        "conformance-server",
        "--bin",
        "wait-control-conformance",
        "--",
        socket,
        authority_root,
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


async def stop_server(process: asyncio.subprocess.Process) -> None:
    if process.returncode is None:
        process.kill()
        await process.wait()


async def exchange(socket: str, request: ExchangeRequest) -> ExchangeResponse:
    """One connection serves exactly one request, so every exchange opens a
    fresh client exactly like the takeover conformance clients do."""
    client = await LocalRpcClient.connect(socket, CONFIG)
    try:
        return await client.exchange(request)
    finally:
        await client.close()


def build_exchange(
    request_seed: int,
    method: str,
    payload: bytes,
    idempotency_key: bytes = b"",
    service: str = "wait_control",
    capability_slot: int | None = CAPABILITY_SLOT,
) -> ExchangeRequest:
    """`capability_slot=None` sends no capability handle at all."""
    request = ExchangeRequest()
    request.envelope.schema.name = "nlos.sabi.Envelope"
    request.envelope.schema.major = 1
    request.envelope.schema.minor = 1
    request.envelope.request_id = filled(request_seed)
    request.envelope.service = service
    request.envelope.method = method
    context = request.envelope.request_context
    context.caller.principal_id = filled(0x31)
    context.caller.application_id = filled(0x32)
    context.caller.process_id = filled(0x33)
    context.caller.process_generation = 1
    context.correlation_id = filled(request_seed + 1)
    context.idempotency_key = idempotency_key
    context.deadline_monotonic_ns = 1_000
    if capability_slot is not None:
        handle = context.capability_handles.add()
        handle.slot = capability_slot
        handle.generation = 1
    request.envelope.payload = payload
    return request


def payload_schema_identity(identity) -> None:
    identity.name = "nlos.sabi.WaitControl"
    identity.major = 1
    identity.minor = 0


def register_payload(
    channel_id: bytes,
    binding_seed: int,
    target_sequence: int,
    key_seed: int,
) -> bytes:
    payload = wc.RegisterWaitRequest()
    payload_schema_identity(payload.schema)
    payload.binding = filled(binding_seed)
    payload.channel_id = channel_id
    payload.target_sequence = target_sequence
    payload.idempotency_key = filled(key_seed)
    payload.registered_at_ms = 1_000
    return payload.SerializeToString()


def notify_payload(channel_id: bytes, up_to_sequence: int, key_seed: int) -> bytes:
    payload = wc.NotifyCommitsRequest()
    payload_schema_identity(payload.schema)
    payload.channel_id = channel_id
    payload.up_to_sequence = up_to_sequence
    payload.notified_at_ms = 2_000
    payload.idempotency_key = filled(key_seed)
    return payload.SerializeToString()


def cancel_payload(wait_id: bytes, key_seed: int) -> bytes:
    payload = wc.CancelWaitRequest()
    payload_schema_identity(payload.schema)
    payload.wait_id = wait_id
    payload.cancelled_at_ms = 3_000
    payload.idempotency_key = filled(key_seed)
    return payload.SerializeToString()


def list_payload() -> bytes:
    payload = wc.ListWaitsRequest()
    payload_schema_identity(payload.schema)
    return payload.SerializeToString()


def inspect_payload(wait_id: bytes) -> bytes:
    payload = wc.InspectWaitRequest()
    payload_schema_identity(payload.schema)
    payload.wait_id = wait_id
    return payload.SerializeToString()


def assert_success(
    response, request_seed: int, side_effecting: bool
):
    """Asserts the bounded success envelope shape and returns the context."""
    context = validate_response_context(
        response.envelope,
        MethodSemantics(side_effecting=side_effecting, long_running=False),
    )
    assert not context.HasField("failure")
    assert context.correlation_id == filled(request_seed + 1)
    assert response.envelope.request_id == filled(request_seed)
    assert response.envelope.service == "wait_control"
    return context


def assert_failure(response, request, code, retry) -> None:
    """Asserts the bounded failure envelope shape: payload and all evidence
    cleared, request identity retained, typed code/retry, correlation echoed."""
    envelope = response.envelope
    assert len(envelope.payload) == 0
    assert envelope.request_id == request.envelope.request_id
    assert envelope.service == request.envelope.service
    assert envelope.method == request.envelope.method
    context = validate_response_context(
        envelope,
        MethodSemantics(side_effecting=True, long_running=False),
    )
    assert context.HasField("failure")
    assert context.failure.code == code
    assert context.failure.retry == retry
    assert not context.receipts
    assert not context.HasField("operation")
    assert context.correlation_id == request.envelope.request_context.correlation_id


async def run_fresh_scenario(authority_root: str):
    """The canonical `fresh`-scene script: all five methods roundtrip over
    real IPC, every mutation replays durably, and every rejection class
    surfaces the bounded failure shape without touching the registry."""
    socket = endpoint("py-fresh")
    process: asyncio.subprocess.Process | None = None
    try:
        process, fixture = await start_server(
            socket, authority_root, {CONNECTIONS_ENV: "14"}
        )
        channel_id = fixture_bytes(fixture, "channel_id")

        # 1. register_wait crosses real IPC and carries the durable row receipt.
        register1 = build_exchange(
            0xD1,
            "register_wait",
            register_payload(channel_id, 1, 5, 1),
            idempotency_key=filled(1),
        )
        registered = await exchange(socket, register1)
        success_context = assert_success(registered, 0xD1, side_effecting=True)
        result1 = wc.RegisterWaitResult()
        result1.ParseFromString(registered.envelope.payload)
        assert not result1.replayed
        record1 = result1.record
        assert record1.state == wc.WAIT_STATE_CODE_PENDING
        assert record1.target_sequence == 5
        assert record1.binding == filled(1)
        assert record1.channel_id == channel_id
        assert record1.channel_generation == 1
        assert record1.registered_at_ms == 1_000
        assert len(record1.channel_fencing_token) == 32
        assert len(success_context.receipts) == 1
        assert success_context.receipts[0].receipt_id == record1.wait_id

        # 2. The exact same request replays the original durable row.
        register_replay = await exchange(socket, register1)
        assert_success(register_replay, 0xD1, side_effecting=True)
        replay_result1 = wc.RegisterWaitResult()
        replay_result1.ParseFromString(register_replay.envelope.payload)
        assert replay_result1.replayed
        assert replay_result1.record == record1

        # 3. notify_commits up to the target wakes the wait.
        notify1 = build_exchange(
            0xD2,
            "notify_commits",
            notify_payload(channel_id, 5, 9),
            idempotency_key=filled(9),
        )
        notified = await exchange(socket, notify1)
        notify_context = assert_success(notified, 0xD2, side_effecting=True)
        report = wc.WakeReport()
        report.ParseFromString(notified.envelope.payload)
        assert len(report.woken) == 1
        assert report.woken[0].state == wc.WAIT_STATE_CODE_WOKEN
        assert report.woken[0].target_sequence == 5
        assert report.woken[0].woken_up_to_sequence == 5
        assert report.woken[0].woken_at_ms == 2_000
        # The notify receipt is keyed by the request idempotency key.
        assert notify_context.receipts[0].receipt_id == filled(9)

        # 4. The notify replay returns the byte-identical durable report.
        notify_replay = await exchange(socket, notify1)
        assert_success(notify_replay, 0xD2, side_effecting=True)
        assert notified.SerializeToString() == notify_replay.SerializeToString()

        # 5. inspect_wait returns the woken durable row.
        inspect1 = build_exchange(0xD4, "inspect_wait", inspect_payload(record1.wait_id))
        inspected = await exchange(socket, inspect1)
        assert_success(inspected, 0xD4, side_effecting=False)
        inspect_result = wc.InspectWaitResult()
        inspect_result.ParseFromString(inspected.envelope.payload)
        assert inspect_result.record.state == wc.WAIT_STATE_CODE_WOKEN
        assert inspect_result.record.wait_id == record1.wait_id
        assert inspect_result.record.woken_up_to_sequence == 5

        # 6. list_waits enumerates the single woken row.
        list1 = build_exchange(0xD5, "list_waits", list_payload())
        listed = await exchange(socket, list1)
        assert_success(listed, 0xD5, side_effecting=False)
        list_result = wc.ListWaitsResult()
        list_result.ParseFromString(listed.envelope.payload)
        assert len(list_result.waits) == 1
        assert list_result.waits[0].state == wc.WAIT_STATE_CODE_WOKEN

        # 7. A second registration on the same channel.
        register2 = build_exchange(
            0xD6,
            "register_wait",
            register_payload(channel_id, 2, 7, 2),
            idempotency_key=filled(2),
        )
        registered2 = await exchange(socket, register2)
        assert_success(registered2, 0xD6, side_effecting=True)
        result2 = wc.RegisterWaitResult()
        result2.ParseFromString(registered2.envelope.payload)
        assert result2.record.state == wc.WAIT_STATE_CODE_PENDING

        # 8. cancel_wait flips the second wait to CANCELLED.
        cancel1 = build_exchange(
            0xD7,
            "cancel_wait",
            cancel_payload(result2.record.wait_id, 3),
            idempotency_key=filled(3),
        )
        cancelled = await exchange(socket, cancel1)
        cancel_context = assert_success(cancelled, 0xD7, side_effecting=True)
        cancel_result = wc.CancelWaitResult()
        cancel_result.ParseFromString(cancelled.envelope.payload)
        assert not cancel_result.replayed
        assert cancel_result.record.state == wc.WAIT_STATE_CODE_CANCELLED
        assert cancel_result.record.cancelled_at_ms == 3_000
        assert cancel_context.receipts[0].receipt_id == filled(3)

        # 9. The cancellation replays durably.
        cancel_replay = await exchange(socket, cancel1)
        assert_success(cancel_replay, 0xD7, side_effecting=True)
        cancel_replay_result = wc.CancelWaitResult()
        cancel_replay_result.ParseFromString(cancel_replay.envelope.payload)
        assert cancel_replay_result.replayed
        assert cancel_replay_result.record == cancel_result.record

        # 10. Unknown method: bounded NOT_SUPPORTED, payload and evidence cleared.
        unknown_method = build_exchange(0xD8, "frobnicate", b"\xaa\xbb")
        assert_failure(
            await exchange(socket, unknown_method),
            unknown_method,
            SABI_ERROR_CODE_NOT_SUPPORTED,
            RETRY_DIRECTIVE_DO_NOT_RETRY,
        )

        # 11. A foreign service name is equally NOT_SUPPORTED.
        foreign_service = build_exchange(
            0xD9, "list_waits", list_payload(), service="other_service"
        )
        assert_failure(
            await exchange(socket, foreign_service),
            foreign_service,
            SABI_ERROR_CODE_NOT_SUPPORTED,
            RETRY_DIRECTIVE_DO_NOT_RETRY,
        )

        # 12. A wrong capability slot is a policy denial: RIGHTS, no side effect.
        denied = build_exchange(
            0xDA,
            "register_wait",
            register_payload(channel_id, 4, 9, 4),
            idempotency_key=filled(4),
            capability_slot=8,
        )
        assert_failure(
            await exchange(socket, denied),
            denied,
            SABI_ERROR_CODE_RIGHTS,
            RETRY_DIRECTIVE_DO_NOT_RETRY,
        )

        # 13. A payload key rebound against the context key is a CONFLICT.
        mismatched = build_exchange(
            0xDB,
            "register_wait",
            register_payload(channel_id, 5, 9, 5),
            idempotency_key=filled(4),
        )
        assert_failure(
            await exchange(socket, mismatched),
            mismatched,
            SABI_ERROR_CODE_CONFLICT,
            RETRY_DIRECTIVE_DO_NOT_RETRY,
        )

        # 14. The rejections above left no durable trace: exactly the two
        # canonical rows remain, in enumeration order.
        list2 = build_exchange(0xDC, "list_waits", list_payload())
        listed2 = await exchange(socket, list2)
        assert_success(listed2, 0xDC, side_effecting=False)
        list_result2 = wc.ListWaitsResult()
        list_result2.ParseFromString(listed2.envelope.payload)
        assert len(list_result2.waits) == 2
        assert list_result2.waits[0].state == wc.WAIT_STATE_CODE_WOKEN
        assert list_result2.waits[0].target_sequence == 5
        assert list_result2.waits[1].state == wc.WAIT_STATE_CODE_CANCELLED
        assert list_result2.waits[1].target_sequence == 7

        assert await process.wait() == 0
        return register1, record1
    finally:
        if process is not None:
            await stop_server(process)
        try:
            Path(socket).unlink()
        except FileNotFoundError:
            pass


async def run_mixed_scenario() -> None:
    """The `mixed`-scene script: preset rows covering every state are read
    back through list/inspect and the preset pending row is cancelled."""
    socket = endpoint("py-mixed")
    authority_root = (
        Path(tempfile.gettempdir()) / f"nlos-wait-{os.getpid()}-{time.time_ns()}-mixed"
    )
    process: asyncio.subprocess.Process | None = None
    try:
        process, fixture = await start_server(
            socket, str(authority_root), {SCENE_ENV: "mixed", CONNECTIONS_ENV: "5"}
        )

        # 1. list_waits enumerates every preset state in durable order.
        list1 = build_exchange(0xE1, "list_waits", list_payload())
        listed = await exchange(socket, list1)
        assert_success(listed, 0xE1, side_effecting=False)
        list_result = wc.ListWaitsResult()
        list_result.ParseFromString(listed.envelope.payload)
        assert len(list_result.waits) == 3
        assert [record.state for record in list_result.waits] == [
            wc.WAIT_STATE_CODE_CANCELLED,
            wc.WAIT_STATE_CODE_WOKEN,
            wc.WAIT_STATE_CODE_PENDING,
        ]
        assert [record.target_sequence for record in list_result.waits] == [1, 2, 3]
        assert list_result.waits[0].wait_id == fixture_bytes(fixture, "cancelled_wait_id")
        assert list_result.waits[1].wait_id == fixture_bytes(fixture, "woken_wait_id")
        assert list_result.waits[2].wait_id == fixture_bytes(fixture, "pending_wait_id")

        # 2..4. inspect_wait returns each preset row with its durable facts.
        def check_cancelled(record) -> None:
            assert record.cancelled_at_ms == 3_000

        def check_woken(record) -> None:
            assert record.woken_at_ms == 2_000
            assert record.woken_up_to_sequence == 2

        def check_untouched(_record) -> None:
            return None

        inspect_cases = (
            (0xE2, "cancelled_wait_id", wc.WAIT_STATE_CODE_CANCELLED, check_cancelled),
            (0xE3, "woken_wait_id", wc.WAIT_STATE_CODE_WOKEN, check_woken),
            (0xE4, "pending_wait_id", wc.WAIT_STATE_CODE_PENDING, check_untouched),
        )
        for seed, field, state, extra in inspect_cases:
            inspect = build_exchange(seed, "inspect_wait", inspect_payload(fixture_bytes(fixture, field)))
            inspected = await exchange(socket, inspect)
            assert_success(inspected, seed, side_effecting=False)
            inspect_result = wc.InspectWaitResult()
            inspect_result.ParseFromString(inspected.envelope.payload)
            assert inspect_result.record.state == state
            extra(inspect_result.record)

        # 5. The preset pending row is cancellable: this is what lets the
        # server postcheck prove the client script really committed.
        cancel = build_exchange(
            0xE5,
            "cancel_wait",
            cancel_payload(fixture_bytes(fixture, "pending_wait_id"), 0xF1),
            idempotency_key=filled(0xF1),
        )
        cancelled = await exchange(socket, cancel)
        assert_success(cancelled, 0xE5, side_effecting=True)
        cancel_result = wc.CancelWaitResult()
        cancel_result.ParseFromString(cancelled.envelope.payload)
        assert not cancel_result.replayed
        assert cancel_result.record.state == wc.WAIT_STATE_CODE_CANCELLED

        assert await process.wait() == 0
    finally:
        if process is not None:
            await stop_server(process)
        try:
            Path(socket).unlink()
        except FileNotFoundError:
            pass
        shutil.rmtree(authority_root, ignore_errors=True)


async def run_restart_scenario(authority_root: str, register1, record1) -> None:
    """A restarted server on the same authority root replays the original
    registration and still enumerates both canonical rows."""
    socket = endpoint("py-restart")
    process: asyncio.subprocess.Process | None = None
    try:
        process, _fixture = await start_server(socket, authority_root, {CONNECTIONS_ENV: "2"})

        replayed = await exchange(socket, register1)
        assert_success(replayed, 0xD1, side_effecting=True)
        replay_result = wc.RegisterWaitResult()
        replay_result.ParseFromString(replayed.envelope.payload)
        assert replay_result.replayed
        # The replay resolves the same durable row (identical id/binding/target),
        # and its state reflects every later transition: after the fresh script
        # it surfaces as WOKEN, not as the original PENDING snapshot.
        assert replay_result.record.wait_id == record1.wait_id
        assert replay_result.record.binding == record1.binding
        assert replay_result.record.channel_id == record1.channel_id
        assert replay_result.record.target_sequence == record1.target_sequence
        assert replay_result.record.state == wc.WAIT_STATE_CODE_WOKEN
        assert replay_result.record.woken_up_to_sequence == 5

        list1 = build_exchange(0xDD, "list_waits", list_payload())
        listed = await exchange(socket, list1)
        assert_success(listed, 0xDD, side_effecting=False)
        list_result = wc.ListWaitsResult()
        list_result.ParseFromString(listed.envelope.payload)
        assert len(list_result.waits) == 2
        assert list_result.waits[0].state == wc.WAIT_STATE_CODE_WOKEN
        assert list_result.waits[1].state == wc.WAIT_STATE_CODE_CANCELLED

        assert await process.wait() == 0
    finally:
        if process is not None:
            await stop_server(process)
        try:
            Path(socket).unlink()
        except FileNotFoundError:
            pass


async def main() -> None:
    authority_root = (
        Path(tempfile.gettempdir()) / f"nlos-wait-{os.getpid()}-{time.time_ns()}-root"
    )
    try:
        register1, record1 = await run_fresh_scenario(str(authority_root))
        await run_mixed_scenario()
        await run_restart_scenario(str(authority_root), register1, record1)
    finally:
        shutil.rmtree(authority_root, ignore_errors=True)


if __name__ == "__main__":
    asyncio.run(main())
