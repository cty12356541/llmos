#!/usr/bin/env python3
"""Cross-process SABI v1.1 Python SDK <-> Rust daemon conformance suite."""

from __future__ import annotations

import asyncio
import hashlib
import os
import secrets
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
SDK_ROOT = ROOT / "sdk" / "python"
GEN_ROOT = ROOT / "gen" / "python"
for path in (str(SDK_ROOT), str(GEN_ROOT)):
    if path not in sys.path:
        sys.path.insert(0, path)

from nlos.sabi.v1 import envelope_pb2 as _env_pb  # noqa: E402
from nlos.sabi.v1 import service_directory_pb2 as _dir_pb  # noqa: E402
from nlos_sdk import (  # noqa: E402
    WAIT_STATE_CANCELLED,
    WAIT_STATE_PENDING,
    WAIT_STATE_WOKEN,
    BlobReferenceClient,
    BlobReferenceError,
    ChannelClient,
    CommonSemanticsError,
    DirectoryNegotiationError,
    Ed25519Signer,
    IpcError,
    LlmChatTurn,
    LlmGatewayClient,
    LlmGatewayError,
    OperationClient,
    OperationControlError,
    OperationReference,
    SABI_V1_CHANNELS,
    ServiceDirectoryClient,
    SessionIdentity,
    ToolSandboxClient,
    ToolSandboxError,
    TransportConfig,
    WaitClient,
    connect_authenticated,
    default_deadline_monotonic_ns,
)

_SEQ = 0


def _daemon_binary() -> Path:
    exe_suffix = ".exe" if sys.platform == "win32" else ""
    binary = ROOT / "target" / "debug" / f"system-control-daemon{exe_suffix}"
    if not binary.exists():
        print("[sabi_v1_1_python] building system-control-daemon...", flush=True)
        subprocess.run(
            [
                "cargo",
                "build",
                "-p",
                "nlos-system-control",
                "--features",
                "daemon",
                "--bin",
                "system-control-daemon",
            ],
            cwd=ROOT,
            check=True,
        )
    return binary


def _endpoint(root: Path, label: str) -> str:
    global _SEQ
    _SEQ += 1
    if sys.platform == "win32":
        return rf"\\.\pipe\nlos-v11-{os.getpid():x}-{time.time_ns():x}-{_SEQ:x}-{label}"
    return str(root / f"{label}.sock")


def _write_seed_file(root: Path, name: str = "client.seed") -> tuple[Path, Ed25519Signer]:
    seed = secrets.token_bytes(32)
    signer = Ed25519Signer(seed)
    seed_file = root / name
    seed_file.write_text(seed.hex() + "\n", encoding="utf-8")
    seed_file.chmod(0o600)
    return seed_file, signer


async def _start_daemon(
    binary: Path,
    state_dir: Path,
    auth_endpoint: str,
    plain_endpoint: str,
    seed_file: Path,
) -> tuple[asyncio.subprocess.Process, bytes]:
    process = await asyncio.create_subprocess_exec(
        str(binary),
        "--root",
        str(state_dir),
        "--auth-socket",
        auth_endpoint,
        "--plain-socket",
        plain_endpoint,
        "--identity-key-file",
        str(seed_file),
        cwd=ROOT,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )
    assert process.stdout is not None
    try:
        ready = await asyncio.wait_for(process.stdout.readline(), timeout=30)
    except BaseException:
        process.kill()
        await process.wait()
        raise
    text = ready.decode("utf-8", errors="replace").strip()
    if not text.startswith("READY") or "principal=" not in text:
        assert process.stderr is not None
        stderr = await process.stderr.read()
        process.kill()
        await process.wait()
        raise AssertionError(f"daemon failed before READY: {text!r} / {stderr!r}")
    fields = dict(part.split("=", 1) for part in text.split() if "=" in part)
    principal_hex = fields.get("principal")
    if not principal_hex or principal_hex == "none":
        process.kill()
        await process.wait()
        raise AssertionError(f"daemon READY missing principal: {text!r}")
    return process, bytes.fromhex(principal_hex)


async def _stop_daemon(process: asyncio.subprocess.Process) -> None:
    if process.returncode is None:
        process.kill()
        await process.wait()


async def _assert_wrong_key_rejected(
    auth_endpoint: str, principal_id: bytes
) -> None:
    wrong_seed = b"WRONG_SEED_SECRET_CANARY_1234567"
    wrong_signer = Ed25519Signer(wrong_seed)
    rpc = await connect_authenticated(auth_endpoint, wrong_signer, principal_id)
    try:
        wait_client = WaitClient(rpc, principal_id=principal_id)
        try:
            await wait_client.list_waits()
        except IpcError as err:
            assert "WRONG_SEED" not in str(err)
            assert wrong_seed.hex() not in str(err)
            return
        raise AssertionError("wrong-key attestation unexpectedly succeeded")
    finally:
        await rpc.close()


async def _assert_unknown_major_rejected(
    auth_endpoint: str, signer: Ed25519Signer, principal_id: bytes
) -> None:
    rpc = await connect_authenticated(auth_endpoint, signer, principal_id)
    try:
        bad_req = _env_pb.ExchangeRequest()
        bad_req.envelope.schema.name = "nlos.sabi.Envelope"
        bad_req.envelope.schema.major = 2
        bad_req.envelope.schema.minor = 0
        bad_req.envelope.request_id = secrets.token_bytes(16)
        bad_req.envelope.service = "wait_control"
        bad_req.envelope.method = "list_waits"
        bad_req.envelope.request_context.caller.principal_id = principal_id
        bad_req.envelope.request_context.caller.application_id = b"\x22" * 16
        bad_req.envelope.request_context.caller.process_id = b"\x33" * 16
        bad_req.envelope.request_context.caller.process_generation = 1
        bad_req.envelope.request_context.correlation_id = secrets.token_bytes(16)
        try:
            await rpc.exchange(bad_req)
        except IpcError as err:
            assert err.code in {"READ", "EOF", "COMPATIBILITY", "CONNECTION_UNUSABLE"}
            return
        raise AssertionError("unknown envelope major=2 unexpectedly accepted")
    finally:
        await rpc.close()


async def _assert_oversized_frame_rejected(
    auth_endpoint: str, signer: Ed25519Signer, principal_id: bytes
) -> None:
    rpc = await connect_authenticated(
        auth_endpoint,
        signer,
        principal_id,
        TransportConfig(maximum_frame_bytes=1024 * 1024),
    )
    try:
        # Exceed client/server control payload bound (> 64 KiB) via LlmGatewayClient
        llm = LlmGatewayClient(rpc, principal_id=principal_id)
        try:
            await llm.chat(
                "local-qwen",
                [LlmChatTurn("user", "X" * 66_000)],
                idempotency_key=secrets.token_bytes(16),
                deadline_monotonic_ns=default_deadline_monotonic_ns(),
            )
        except IpcError as err:
            assert err.code == "FRAME_TOO_LARGE", err.code
            assert rpc.is_usable
        else:
            raise AssertionError("oversized chat payload was not rejected")

        # Also probe wire oversized ExchangeRequest (> 1 MiB transport frame bound)
        huge = _env_pb.ExchangeRequest()
        huge.envelope.schema.name = "nlos.sabi.Envelope"
        huge.envelope.schema.major = 1
        huge.envelope.schema.minor = 1
        huge.envelope.request_id = secrets.token_bytes(16)
        huge.envelope.service = "wait_control"
        huge.envelope.method = "list_waits"
        huge.envelope.request_context.caller.principal_id = principal_id
        huge.envelope.request_context.caller.application_id = b"\x22" * 16
        huge.envelope.request_context.caller.process_id = b"\x33" * 16
        huge.envelope.request_context.caller.process_generation = 1
        huge.envelope.request_context.correlation_id = secrets.token_bytes(16)
        huge.envelope.payload = b"Z" * (1024 * 1024 + 64)
        try:
            await rpc.exchange(huge)
        except IpcError as err:
            assert err.code == "FRAME_TOO_LARGE", err.code
        else:
            raise AssertionError("oversized 1 MiB wire frame was not rejected")
    finally:
        await rpc.close()


async def main() -> None:
    assert len(SABI_V1_CHANNELS) == 12
    binary = _daemon_binary()

    with tempfile.TemporaryDirectory(dir="/tmp", prefix="nlos-v11-") as tmp_dir:
        tmp_path = Path(tmp_dir)
        state_dir = tmp_path / "state"
        state_dir.mkdir(parents=True, exist_ok=True)
        seed_file, signer = _write_seed_file(tmp_path)
        auth_endpoint = _endpoint(tmp_path, "auth")
        plain_endpoint = _endpoint(tmp_path, "plain")

        daemon, principal_id = await _start_daemon(
            binary, state_dir, auth_endpoint, plain_endpoint, seed_file
        )
        try:
            identity = SessionIdentity(
                principal_id=principal_id,
                application_id=b"\x22" * 16,
                process_id=b"\x33" * 16,
            )

            # Case 1: Single persistent Ed25519 authenticated connection for multi-round SABI calls
            rpc = await connect_authenticated(auth_endpoint, signer, identity.principal_id)
            try:
                common_kwargs = {
                    "principal_id": identity.principal_id,
                    "application_id": identity.application_id,
                    "process_id": identity.process_id,
                    "process_generation": identity.process_generation,
                }
                dir_client = ServiceDirectoryClient(rpc, **common_kwargs)
                chan_client = ChannelClient(rpc, **common_kwargs)
                wait_client = WaitClient(rpc, **common_kwargs)
                op_client = OperationClient(rpc, **common_kwargs)
                llm_client = LlmGatewayClient(rpc, **common_kwargs)
                tool_client = ToolSandboxClient(rpc, **common_kwargs)
                blob_client = BlobReferenceClient(rpc, **common_kwargs)

                # 1a. ServiceDirectory.negotiate (mounted + unmounted)
                for req in (
                    ChannelClient.service_requirement(),
                    WaitClient.service_requirement(),
                    OperationClient.service_requirement(),
                ):
                    binding = await dir_client.negotiate(req)
                    assert binding.candidate.service == req.service
                    assert binding.candidate.version.major == req.major

                try:
                    await dir_client.negotiate(LlmGatewayClient.service_requirement())
                except DirectoryNegotiationError as err:
                    assert err.code == _dir_pb.DIRECTORY_ERROR_CODE_NOT_FOUND, err.code
                else:
                    raise AssertionError(
                        "expected DirectoryNegotiationError for unmounted llm_gateway"
                    )
                print("[1/6] PASS ServiceDirectory.negotiate mounted & unmounted")

                # 1b. ChannelControl over same connection
                chan = await chan_client.create_channel(64 * 1024)
                channel_id = chan.channel_id
                entry1 = await chan_client.enqueue(
                    channel_id, chan.generation, chan.fencing_token, b"sabi-v1.1-msg"
                )
                assert entry1.sequence == 1
                qstate = await chan_client.inspect_queue(channel_id)
                assert qstate.next_sequence == 2
                print("[2/6] PASS ChannelControl persistent multi-round")

                # 1c. WaitControl full lifecycle over same connection
                reg_key = secrets.token_bytes(16)
                binding1 = secrets.token_bytes(16)
                wait1 = await wait_client.register_wait(
                    binding1, channel_id, 2, idempotency_key=reg_key
                )
                assert not wait1.replayed
                assert wait1.record.state == WAIT_STATE_PENDING

                wait1_replay = await wait_client.register_wait(
                    binding1, channel_id, 2, idempotency_key=reg_key
                )
                assert wait1_replay.replayed
                assert wait1_replay.record.wait_id == wait1.record.wait_id

                inspected = await wait_client.inspect_wait(wait1.record.wait_id)
                assert inspected.record.wait_id == wait1.record.wait_id
                listed = await wait_client.list_waits(channel_id)
                assert any(w.wait_id == wait1.record.wait_id for w in listed.waits)

                cancelled = await wait_client.cancel_wait(wait1.record.wait_id)
                assert cancelled.record.state == WAIT_STATE_CANCELLED

                wait2 = await wait_client.register_wait(
                    secrets.token_bytes(16), channel_id, 2
                )
                wake = await wait_client.notify_commits(channel_id, 2)
                assert any(
                    w.wait_id == wait2.record.wait_id and w.state == WAIT_STATE_WOKEN
                    for w in wake.woken
                )
                print("[3/6] PASS WaitControl register/replay/inspect/list/cancel/notify")

                # 1d. OperationControl query/cancel typed NOT_FOUND against live daemon
                missing_op = OperationReference(secrets.token_bytes(16), 1)
                for op_call in (
                    op_client.query(missing_op),
                    op_client.cancel(missing_op, expected_cancel_epoch=0),
                ):
                    try:
                        await op_call
                    except OperationControlError as err:
                        assert err.code == _env_pb.SABI_ERROR_CODE_NOT_FOUND, err.code
                        assert not err.is_retryable
                    else:
                        raise AssertionError("expected OperationControlError NOT_FOUND")
                print("[4/6] PASS OperationControl typed query/cancel closure")

                # 1e. New v1.1 clients wire encoding + typed unmounted closure on daemon
                deadline = default_deadline_monotonic_ns()
                unmounted_checks = [
                    (llm_client.query_budget(), LlmGatewayError),
                    (
                        llm_client.chat(
                            "local-qwen",
                            [LlmChatTurn("user", "ping")],
                            idempotency_key=secrets.token_bytes(16),
                            deadline_monotonic_ns=deadline,
                        ),
                        LlmGatewayError,
                    ),
                    (tool_client.list_tools(), ToolSandboxError),
                    (
                        tool_client.invoke_tool(
                            "fs.noop",
                            "{}",
                            idempotency_key=secrets.token_bytes(16),
                            deadline_monotonic_ns=deadline,
                        ),
                        ToolSandboxError,
                    ),
                    (
                        blob_client.mint_reference(
                            "/tmp/probe.bin",
                            hashlib.sha256(b"probe").digest(),
                            5,
                            max_reads=1,
                            max_chunk_bytes=64,
                            recipient_principal_id=identity.principal_id,
                        ),
                        BlobReferenceError,
                    ),
                ]
                for coro, expected_err in unmounted_checks:
                    try:
                        await coro
                    except expected_err as err:
                        assert err.code in {
                            _env_pb.SABI_ERROR_CODE_NOT_SUPPORTED,
                            _env_pb.SABI_ERROR_CODE_NOT_FOUND,
                        }, err.code
                        assert not err.is_retryable
                    else:
                        raise AssertionError(
                            f"expected typed {expected_err.__name__} for unmounted service"
                        )

                # Verify recipient_principal_id missing fails closed client-side
                try:
                    await blob_client.mint_reference(
                        "/tmp/probe.bin",
                        hashlib.sha256(b"probe").digest(),
                        5,
                    )
                except CommonSemanticsError as err:
                    assert err.code == "MISSING_RECIPIENT_PRINCIPAL"
                else:
                    raise AssertionError("expected MISSING_RECIPIENT_PRINCIPAL")

                assert rpc.is_usable
                print("[5/6] PASS LlmGateway/ToolSandbox/BlobReference unmounted & boundary closure")
            finally:
                await rpc.close()

            # Case 2: Wrong-key handshake, unknown major=2, and oversized frame bounds
            await _assert_wrong_key_rejected(auth_endpoint, principal_id)
            await _assert_unknown_major_rejected(auth_endpoint, signer, principal_id)
            await _assert_oversized_frame_rejected(auth_endpoint, signer, principal_id)
            print("[6/6] PASS wrong-key, unknown-major, and oversize-frame boundaries")
        finally:
            await _stop_daemon(daemon)

    print("ALL 6/6 SABI v1.1 CONFORMANCE CASES PASSED")


if __name__ == "__main__":
    asyncio.run(main())
