"""Cross-language Python ChannelClient against the Rust system-control-daemon."""

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

from nlos_sdk import (  # noqa: E402
    CHANNEL_CONTROL_SCHEMA,
    CHANNEL_CONTROL_SERVICE,
    ChannelClient,
    ChannelControlError,
    TransportConfig,
)
from nlos_sdk.channel import (  # noqa: E402
    RETRY_DIRECTIVE_DO_NOT_RETRY,
    SABI_ERROR_CODE_CONFLICT,
    SABI_ERROR_CODE_FENCED,
    SABI_ERROR_CODE_NOT_FOUND,
    SABI_ERROR_CODE_QUOTA,
)

_ENDPOINT_SEQ = 0


def endpoint(label: str) -> str:
    global _ENDPOINT_SEQ
    _ENDPOINT_SEQ += 1
    unique = f"{os.getpid():x}-{time.time_ns():x}-{_ENDPOINT_SEQ:x}-{label}"
    if sys.platform == "win32":
        return rf"\\.\pipe\nlos-cc-daemon-{unique}"
    return str(Path(tempfile.gettempdir()) / f"nlos-cc-daemon-{unique}.sock")


async def start_daemon(
    root_dir: Path,
    auth_endpoint: str,
    plain_endpoint: str,
) -> asyncio.subprocess.Process:
    process = await asyncio.create_subprocess_exec(
        "cargo",
        "run",
        "--quiet",
        "-p",
        "nlos-system-control",
        "--features",
        "daemon",
        "--bin",
        "system-control-daemon",
        "--",
        "--root",
        str(root_dir),
        "--auth-socket",
        auth_endpoint,
        "--plain-socket",
        plain_endpoint,
        cwd=ROOT,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )
    assert process.stdout is not None
    try:
        ready = await asyncio.wait_for(process.stdout.readline(), timeout=120)
    except BaseException:
        process.kill()
        await process.wait()
        raise
    ready_text = ready.decode("utf-8", errors="replace").strip()
    if not ready_text.startswith("READY ") or "plain_socket=" not in ready_text:
        assert process.stderr is not None
        stderr = await process.stderr.read()
        process.kill()
        await process.wait()
        raise AssertionError(
            f"system-control-daemon failed before READY: {ready_text!r} / {stderr!r}"
        )
    return process


async def stop_daemon(process: asyncio.subprocess.Process) -> None:
    if process.returncode is None:
        process.kill()
        await process.wait()


async def main() -> None:
    root_dir = (
        Path(tempfile.gettempdir())
        / f"nlos-cc-conformance-{os.getpid()}-{time.time_ns()}"
    )
    auth_endpoint = endpoint("auth")
    plain_endpoint = endpoint("plain")

    req = ChannelClient.service_requirement()
    assert req.service == CHANNEL_CONTROL_SERVICE
    assert req.schema_name == CHANNEL_CONTROL_SCHEMA
    assert req.major == 1
    assert req.minimum_minor == 0

    daemon = await start_daemon(root_dir, auth_endpoint, plain_endpoint)
    try:
        config = TransportConfig(
            connect_timeout=5.0,
            read_timeout=5.0,
            write_timeout=5.0,
        )
        client = ChannelClient(plain_endpoint, config=config)

        # Case 1/6: create_channel + idempotent replay
        create_key = b"\x11" * 16
        policy_digest = b"\x22" * 32
        created = await client.create_channel(
            64,
            policy_digest=policy_digest,
            idempotency_key=create_key,
            created_at_ms=1_000,
        )
        assert not created.replayed
        assert len(created.channel_id) == 16
        assert created.generation == 1
        assert len(created.fencing_token) == 32
        assert created.capacity_bytes == 64
        assert created.policy_digest == policy_digest
        assert created.idempotency_key == create_key
        assert created.created_at_ms == 1_000

        created_replay = await client.create_channel(
            64,
            policy_digest=policy_digest,
            idempotency_key=create_key,
            created_at_ms=1_000,
        )
        assert created_replay.replayed
        assert created_replay.channel_id == created.channel_id
        assert created_replay.generation == created.generation
        assert created_replay.fencing_token == created.fencing_token
        print(
            "[conformance/channel_control] case 1/6 create_channel + idempotent replay PASS",
            flush=True,
        )

        # Case 2/6: enqueue + enqueue_registered + idempotent enqueue replay
        payload_1 = b"hello-channel-1"
        payload_2 = b"hello-channel-2-registered"
        enq1_key = b"\x21" * 16
        enq2_key = b"\x22" * 16
        producer_binding = b"\x77" * 16

        entry1 = await client.enqueue(
            created.channel_id,
            created.generation,
            created.fencing_token,
            payload_1,
            idempotency_key=enq1_key,
            enqueued_at_ms=1_100,
        )
        assert not entry1.replayed
        assert entry1.channel_id == created.channel_id
        assert entry1.generation == 1
        assert entry1.sequence == 1
        assert entry1.payload == payload_1
        assert entry1.payload_len == len(payload_1)
        assert entry1.binding == b""
        assert entry1.fiber_generation == 0

        entry2 = await client.enqueue_registered(
            created.channel_id,
            created.generation,
            created.fencing_token,
            payload_2,
            producer_binding,
            1,
            idempotency_key=enq2_key,
            enqueued_at_ms=1_200,
        )
        assert not entry2.replayed
        assert entry2.channel_id == created.channel_id
        assert entry2.generation == 1
        assert entry2.sequence == 2
        assert entry2.payload == payload_2
        assert entry2.payload_len == len(payload_2)
        assert entry2.binding == producer_binding
        assert entry2.fiber_generation == 1

        entry1_replay = await client.enqueue(
            created.channel_id,
            created.generation,
            created.fencing_token,
            payload_1,
            idempotency_key=enq1_key,
            enqueued_at_ms=1_100,
        )
        assert entry1_replay.replayed
        assert entry1_replay.sequence == 1
        assert entry1_replay.payload == payload_1
        print(
            "[conformance/channel_control] case 2/6 enqueue + enqueue_registered + replay PASS",
            flush=True,
        )

        # Case 3/6: receive & inspect_queue
        entries = await client.receive(created.channel_id, limit=16)
        assert len(entries) == 2
        assert entries[0].sequence == 1
        assert entries[0].payload == payload_1
        assert entries[1].sequence == 2
        assert entries[1].payload == payload_2

        qstate = await client.inspect_queue(created.channel_id)
        assert qstate.channel_id == created.channel_id
        assert qstate.capacity_bytes == 64
        assert qstate.outstanding_entries == 2
        assert qstate.unacked_entries == 2
        assert qstate.acked_uncompacted_entries == 0
        assert qstate.outstanding_bytes == len(payload_1) + len(payload_2)
        assert qstate.next_sequence == 3
        print(
            "[conformance/channel_control] case 3/6 receive + inspect_queue PASS",
            flush=True,
        )

        # Case 4/6: register_queue_consumption + ack + compact
        consumer_fiber = b"\x88" * 16
        reg_key = b"\x31" * 16
        consumption = await client.register_queue_consumption(
            created.channel_id,
            created.generation,
            1,
            consumer_fiber,
            1,
            idempotency_key=reg_key,
            registered_at_ms=1_300,
        )
        assert not consumption.replayed
        assert consumption.channel_id == created.channel_id
        assert consumption.sequence == 1
        assert consumption.consumer_fiber_id == consumer_fiber
        assert consumption.consumer_fiber_generation == 1
        assert len(consumption.registration_id) == 16

        ack_first = await client.ack(
            created.channel_id,
            created.generation,
            1,
            acked_at_ms=1_400,
        )
        assert ack_first.acked
        assert ack_first.sequence == 1
        assert ack_first.acked_at_ms == 1_400

        ack_second = await client.ack(
            created.channel_id,
            created.generation,
            1,
            acked_at_ms=1_400,
        )
        assert not ack_second.acked
        assert ack_second.replayed
        assert ack_second.sequence == 1

        compacted = await client.compact(
            created.channel_id,
            created.generation,
        )
        assert compacted.removed_entries == 1
        assert compacted.reclaimed_bytes == len(payload_1)
        assert compacted.queue_state.outstanding_entries == 1
        assert compacted.queue_state.unacked_entries == 1
        assert compacted.queue_state.acked_uncompacted_entries == 0
        print(
            "[conformance/channel_control] case 4/6 register_queue_consumption + ack + compact PASS",
            flush=True,
        )

        # Case 5/6: inspect_channel & rotate_channel
        inspected = await client.inspect_channel(created.channel_id)
        assert inspected.channel_id == created.channel_id
        assert inspected.generation == 1
        assert inspected.fencing_token == created.fencing_token

        rotated = await client.rotate_channel(
            created.channel_id,
            1,
            created.fencing_token,
            rotated_at_ms=1_500,
        )
        assert not rotated.replayed
        assert rotated.channel_id == created.channel_id
        assert rotated.generation == 2
        assert rotated.fencing_token != created.fencing_token
        print(
            "[conformance/channel_control] case 5/6 inspect_channel + rotate_channel PASS",
            flush=True,
        )

        # Case 6/6: fail-closed error handling (Fenced, Quota, NotFound, Conflict)
        try:
            await client.enqueue(
                created.channel_id,
                1,
                created.fencing_token,
                b"stale-write",
            )
        except ChannelControlError as error:
            assert error.code == SABI_ERROR_CODE_FENCED
            assert error.retry == RETRY_DIRECTIVE_DO_NOT_RETRY
        else:
            raise AssertionError("stale generation/fencing_token enqueue was accepted")

        try:
            await client.enqueue(
                created.channel_id,
                rotated.generation,
                rotated.fencing_token,
                b"x" * 65,
            )
        except ChannelControlError as error:
            assert error.code == SABI_ERROR_CODE_QUOTA
            assert error.retry == RETRY_DIRECTIVE_DO_NOT_RETRY
        else:
            raise AssertionError("over-capacity enqueue was accepted")

        try:
            await client.inspect_channel(b"\xff" * 16)
        except ChannelControlError as error:
            assert error.code == SABI_ERROR_CODE_NOT_FOUND
            assert error.retry == RETRY_DIRECTIVE_DO_NOT_RETRY
        else:
            raise AssertionError("inspect_channel on unknown channel was accepted")

        try:
            await client.create_channel(
                128,
                policy_digest=policy_digest,
                idempotency_key=create_key,
                created_at_ms=1_000,
            )
        except ChannelControlError as error:
            assert error.code == SABI_ERROR_CODE_CONFLICT
            assert error.retry == RETRY_DIRECTIVE_DO_NOT_RETRY
        else:
            raise AssertionError("conflicting idempotency_key was accepted")

        print(
            "[conformance/channel_control] case 6/6 fail-closed error handling PASS",
            flush=True,
        )
    finally:
        await stop_daemon(daemon)
        if sys.platform != "win32":
            for sock in (auth_endpoint, plain_endpoint):
                try:
                    Path(sock).unlink()
                except FileNotFoundError:
                    pass
        shutil.rmtree(root_dir, ignore_errors=True)

    print("[conformance/channel_control] ALL 6/6 CASES PASSED", flush=True)


if __name__ == "__main__":
    asyncio.run(main())
