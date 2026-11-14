"""Cross-language Python authenticated session vs the Rust system-control-daemon.

对位 W82-D 件二的机械验收:对现有 Rust 单发服务端(system-control-daemon
``--auth-socket`` 端点,每连接 = 1 次 ADR-0011 握手 + 1 次 exchange)完成
认证握手并跑通认证 exchange。Rust 侧 ``verify_strict`` 通过即证明 Python
的 digest/binding/签名逐字节兼容(任何字节序或域分隔错误都会被拒)。

Cases:
  1/5 认证会话 create_channel(握手 + 1 轮认证 exchange)
  2/5 新认证会话 enqueue(服务端单发:每 case 各自握手)
  3/5 新认证会话 receive + inspect_queue 断言队列状态
  4/5 错误密钥失败闭合:握手无 ack,错钥由首轮 exchange 揭晓被拒
  5/5 plain 端点对照(既有明文路径不回归)

用法: python3 tests/conformance/ipc/authenticated_session.py
"""

from __future__ import annotations

import asyncio
import os
import secrets
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / "sdk" / "python"))

from nlos_sdk import (  # noqa: E402
    ChannelClient,
    ChannelControlError,
    Ed25519Signer,
    IpcError,
    TransportConfig,
)
from nlos_sdk.principal_session import connect_authenticated  # noqa: E402

from nlos.sabi.v1.envelope_pb2 import ExchangeRequest  # noqa: E402

_ENDPOINT_SEQ = 0


def endpoint(label: str) -> str:
    global _ENDPOINT_SEQ
    _ENDPOINT_SEQ += 1
    unique = f"{os.getpid():x}-{time_ns():x}-{_ENDPOINT_SEQ:x}-{label}"
    if sys.platform == "win32":
        return rf"\\.\pipe\nlos-auth-session-{unique}"
    return str(Path(tempfile.gettempdir()) / f"nlos-auth-session-{unique}.sock")


def time_ns() -> int:
    import time

    return time.time_ns()


def binary_path() -> Path:
    binary = ROOT / "target" / "debug" / "system-control-daemon"
    if not binary.exists():
        print("[authenticated_session] daemon binary missing; building…", flush=True)
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


def write_seed_file(root: Path) -> tuple[Path, Ed25519Signer]:
    seed = secrets.token_bytes(32)
    signer = Ed25519Signer(seed)
    seed_file = root / "client.seed"
    seed_file.write_text(seed.hex() + "\n", encoding="utf-8")
    seed_file.chmod(0o600)
    return seed_file, signer


async def start_daemon(
    binary: Path, root_dir: Path, auth_endpoint: str, plain_endpoint: str,
    seed_file: Path,
) -> asyncio.subprocess.Process:
    process = await asyncio.create_subprocess_exec(
        str(binary),
        "--root",
        str(root_dir),
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
        raise AssertionError(f"daemon READY without bootstrapped principal: {text!r}")
    print(f"[authenticated_session] daemon READY principal={principal_hex}", flush=True)
    process._principal_hex = principal_hex  # type: ignore[attr-defined]
    return process


async def stop_daemon(process: asyncio.subprocess.Process) -> None:
    if process.returncode is None:
        process.kill()
        await process.wait()


async def build_probe_request(request_id: bytes) -> ExchangeRequest:
    request = ExchangeRequest()
    request.envelope.schema.name = "nlos.sabi.Envelope"
    request.envelope.schema.major = 1
    request.envelope.schema.minor = 1
    request.envelope.request_id = request_id
    request.envelope.service = "channel"
    request.envelope.method = "inspect_channel"
    return request


async def main() -> None:
    binary = binary_path()
    root_dir = (
        Path(tempfile.gettempdir())
        / f"nlos-auth-conformance-{os.getpid()}-{time_ns()}"
    )
    root_dir.mkdir(parents=True)
    seed_file, signer = write_seed_file(root_dir)
    auth_endpoint = endpoint("auth")
    plain_endpoint = endpoint("plain")

    daemon = await start_daemon(
        binary, root_dir, auth_endpoint, plain_endpoint, seed_file
    )
    principal_id = bytes.fromhex(daemon._principal_hex)  # type: ignore[attr-defined]
    assert len(principal_id) == 16
    config = TransportConfig(connect_timeout=5.0, read_timeout=5.0, write_timeout=5.0)

    try:
        # Case 1/5: authenticated create_channel
        client = await ChannelClient.connect_authenticated(
            auth_endpoint,
            signer=signer,
            principal_id=principal_id,
            config=config,
        )
        create_key = b"\x51" * 16
        policy_digest = b"\x62" * 32
        created = await client.create_channel(
            128,
            policy_digest=policy_digest,
            idempotency_key=create_key,
            created_at_ms=2_000,
        )
        assert not created.replayed
        assert len(created.channel_id) == 16
        assert created.generation == 1
        await client.close()
        print(
            "[authenticated_session] case 1/5 authenticated create_channel PASS "
            f"(channel_id={created.channel_id.hex()}, fencing={created.fencing_token.hex()[:16]}…)",
            flush=True,
        )

        # Case 2/5: fresh authenticated session enqueue(服务端单发 → 各自握手)
        client = await ChannelClient.connect_authenticated(
            auth_endpoint, signer=signer, principal_id=principal_id, config=config
        )
        payload = b"auth-session-payload"
        enq_key = b"\x61" * 16
        entry = await client.enqueue(
            created.channel_id,
            created.generation,
            created.fencing_token,
            payload,
            idempotency_key=enq_key,
            enqueued_at_ms=2_100,
        )
        assert not entry.replayed
        assert entry.sequence == 1
        assert entry.payload == payload
        await client.close()
        print(
            "[authenticated_session] case 2/5 authenticated enqueue PASS "
            f"(seq={entry.sequence}, payload_len={entry.payload_len})",
            flush=True,
        )

        # Case 3/5: fresh authenticated sessions receive + inspect_queue
        # (服务端单发:每连接 1 次握手 + 1 次 exchange,两查询各起一会话)
        client = await ChannelClient.connect_authenticated(
            auth_endpoint, signer=signer, principal_id=principal_id, config=config
        )
        entries = await client.receive(created.channel_id, limit=8)
        await client.close()
        assert len(entries) == 1 and entries[0].payload == payload

        client = await ChannelClient.connect_authenticated(
            auth_endpoint, signer=signer, principal_id=principal_id, config=config
        )
        state = await client.inspect_queue(created.channel_id)
        await client.close()
        assert state.outstanding_entries == 1
        assert state.next_sequence == 2
        print(
            "[authenticated_session] case 3/5 authenticated receive+inspect_queue PASS "
            f"(outstanding={state.outstanding_entries}, next_seq={state.next_sequence})",
            flush=True,
        )

        # Case 4/5: wrong-key attestation fails closed(Rust verify_strict 拒绝)
        stranger = Ed25519Signer(secrets.token_bytes(32))
        rpc = await connect_authenticated(
            auth_endpoint, stranger, principal_id, config=config
        )
        try:
            await rpc.exchange(await build_probe_request(secrets.token_bytes(16)))
        except IpcError as error:
            print(
                "[authenticated_session] case 4/5 wrong-key attestation rejected "
                f"PASS (typed IpcError code={error.code})",
                flush=True,
            )
        else:
            raise AssertionError("wrong-key attestation was accepted by Rust daemon")
        await rpc.close()

        # Case 5/5: plain endpoint control(既有明文路径不回归)
        plain = ChannelClient(plain_endpoint, config=config)
        replayed = await plain.create_channel(
            128,
            policy_digest=policy_digest,
            idempotency_key=create_key,
            created_at_ms=2_000,
        )
        assert replayed.replayed
        assert replayed.channel_id == created.channel_id
        print(
            "[authenticated_session] case 5/5 plain-endpoint replay control PASS",
            flush=True,
        )
    finally:
        await stop_daemon(daemon)
        if sys.platform != "win32":
            for sock in (auth_endpoint, plain_endpoint):
                Path(sock).unlink(missing_ok=True)
        shutil.rmtree(root_dir, ignore_errors=True)

    print("[authenticated_session] ALL 5/5 CASES PASSED", flush=True)


if __name__ == "__main__":
    asyncio.run(main())
