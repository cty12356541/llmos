#!/usr/bin/env python3
"""W82-INT-DOGFOOD-E2E: Dual-process external Python Agent (``qq-agent`` Phase 2)
end-to-end conformance & self-hosting verification over SABI v1.1 thin-waist.

Verifies that two external Python OS processes (``adapter`` + ``brain``) boot with
ONLY the 3-tuple environment contract (``LLMOS_IPC_SOCKET``,
``LLMOS_PRINCIPAL_KEY_PATH``, ``LLMOS_ROOT_CAPABILITY_SLOT`` — zero ``LLM_API_KEY``,
zero custom supervisor, zero local budget SQLite, zero user-space SSRF firewall)
and complete a full 200 KiB voice-note round-trip across all SABI v1.1 channels
with ``<= 250`` lines of core agent business code.

Ported from the W82 local lane onto the canonical SDK surface: workers glue
``connect_authenticated`` + ``LocalRpcClient`` + ``SessionIdentity`` instead of
the retired ``PrincipalSession``; LlmGateway/ToolSandbox mutations carry explicit
idempotency keys and deadlines; BlobReference URIs are ``nlos-blob://`` and reads
are chunked via ``read_reference``. Requires the W82-INT mounted daemon
(LlmGateway/ToolSandbox/BlobReference endpoints) to pass end-to-end.
"""

from __future__ import annotations

import asyncio
import hashlib
import json
import os
from pathlib import Path
import re
import secrets
import subprocess
import sys
import tempfile
import time
from typing import Any

REPO_ROOT = Path(__file__).resolve().parents[3]
SDK_ROOT = REPO_ROOT / "sdk" / "python"
GEN_PYTHON_ROOT = REPO_ROOT / "gen" / "python"
for _p in (SDK_ROOT, GEN_PYTHON_ROOT):
    if str(_p) not in sys.path:
        sys.path.insert(0, str(_p))

from nlos_sdk import (  # noqa: E402
    BLOB_REFERENCE_SCHEMA,
    BLOB_REFERENCE_SERVICE,
    CHANNEL_CONTROL_SCHEMA,
    CHANNEL_CONTROL_SERVICE,
    LLM_GATEWAY_SCHEMA,
    LLM_GATEWAY_SERVICE,
    MAX_BLOB_CHUNK_BYTES,
    OPERATION_CONTROL_SCHEMA,
    OPERATION_CONTROL_SERVICE,
    TOOL_SANDBOX_SCHEMA,
    TOOL_SANDBOX_SERVICE,
    WAIT_CONTROL_SCHEMA,
    WAIT_CONTROL_SERVICE,
    BlobReferenceClient,
    ChannelClient,
    Ed25519Signer,
    IpcError,
    LlmGatewayClient,
    LlmUsage,
    LocalRpcClient,
    ServiceDirectoryClient,
    ServiceRequirement,
    SessionIdentity,
    ToolSandboxClient,
    WaitClient,
    connect_authenticated,
    default_deadline_monotonic_ns,
    format_blob_uri,
    parse_blob_uri,
)

VOICE_WAV_SIZE_BYTES = 200 * 1024  # 200 KiB (> 64 KiB single-frame limit)
MAX_CORE_AGENT_LINES = 250
W81_PHASE1_BASELINE_LINES = 770
_ROOT_SLOT_HEX_RE = re.compile(r"\A[0-9a-f]{32}\Z")
BLOB_URI_PREFIX = "nlos-blob://"


def derive_daemon_principal_id(public_key: bytes) -> bytes:
    """Derive the 16-byte ``PrincipalId`` minted by ``system-control-daemon``
    bootstrap from a 32-byte Ed25519 verifying key."""

    pub = bytes(public_key)
    if len(pub) != 32:
        raise ValueError(f"Ed25519 public_key must be 32 bytes, got {len(pub)}")
    idem = hashlib.sha256(
        b"llmos/system-control-daemon/bootstrap-idempotency/v1" + pub
    ).digest()[:16]
    profile_digest = hashlib.sha256(
        b"llmos/system-control-daemon/principal-profile/v1" + pub
    ).digest()
    return hashlib.sha256(
        b"nlos/principal/id/v1" + idem + profile_digest + pub
    ).digest()[:16]


class _Session:
    """Per-worker authenticated session over one persistent LocalRpcClient.

    Replaces the retired ``PrincipalSession`` helper: holds the shared rpc
    connection plus a single derived SessionIdentity so every service client
    issues calls under the same caller tuple, and counts exchanges for the
    summary report.
    """

    def __init__(self, rpc: LocalRpcClient, identity: SessionIdentity) -> None:
        self.rpc = rpc
        self.identity = identity
        self.exchanges = 0
        real_exchange = rpc.exchange

        async def _counted(request):
            self.exchanges += 1
            return await real_exchange(request)

        rpc.exchange = _counted  # type: ignore[method-assign]

    @property
    def principal_id(self) -> bytes:
        return self.identity.principal_id

    def client_kwargs(self) -> dict[str, Any]:
        i = self.identity
        return {
            "principal_id": i.principal_id,
            "application_id": i.application_id,
            "process_id": i.process_id,
            "process_generation": i.process_generation,
        }

    async def close(self) -> None:
        await self.rpc.close()


async def _session_connect_from_env() -> _Session:
    endpoint = os.environ.get("LLMOS_IPC_SOCKET", "").strip()
    key_path = os.environ.get("LLMOS_PRINCIPAL_KEY_PATH", "").strip()
    root_slot = os.environ.get("LLMOS_ROOT_CAPABILITY_SLOT", "").strip()
    if not endpoint or not key_path or not _ROOT_SLOT_HEX_RE.match(root_slot):
        raise IpcError(
            "INVALID_CONFIG",
            "LLMOS_IPC_SOCKET, LLMOS_PRINCIPAL_KEY_PATH, and 32-hex "
            "LLMOS_ROOT_CAPABILITY_SLOT are required",
        )
    seed_hex = Path(key_path).read_text(encoding="utf-8").strip()
    signer = Ed25519Signer(bytes.fromhex(seed_hex))
    principal_id = derive_daemon_principal_id(signer.public_key)
    rpc = await connect_authenticated(endpoint, signer, principal_id)
    return _Session(rpc, SessionIdentity.derive(principal_id))


# === BEGIN_QQ_AGENT_CORE ===
def _assert_strict_three_env_contract() -> None:
    llmos_keys = sorted(k for k in os.environ if k.startswith("LLMOS_"))
    assert llmos_keys == [
        "LLMOS_IPC_SOCKET",
        "LLMOS_PRINCIPAL_KEY_PATH",
        "LLMOS_ROOT_CAPABILITY_SLOT",
    ], f"worker must only receive the 3-tuple LLMOS_* contract, got {llmos_keys}"
    assert not [
        k for k in os.environ if "API_KEY" in k or "OPENAI" in k or "ANTHROPIC" in k
    ]


async def run_adapter_worker(coord_path: Path, wav_dir: Path) -> dict[str, Any]:
    """External ``adapter`` worker: ingress WAV -> nlos-blob uri -> channel -> egress."""
    _assert_strict_three_env_contract()
    session = await _session_connect_from_env()
    try:
        ids = session.client_kwargs()
        directory = ServiceDirectoryClient(session.rpc, **ids)
        for svc, schema in (
            (CHANNEL_CONTROL_SERVICE, CHANNEL_CONTROL_SCHEMA),
            (WAIT_CONTROL_SERVICE, WAIT_CONTROL_SCHEMA),
            (BLOB_REFERENCE_SERVICE, BLOB_REFERENCE_SCHEMA),
        ):
            await directory.negotiate(ServiceRequirement(svc, schema, 1, 0))

        channels = ChannelClient(session.rpc, **ids)
        waits = WaitClient(session.rpc, **ids)
        blobs = BlobReferenceClient(session.rpc, **ids)
        inbound = await channels.create_channel(65_536, created_at_ms=1_000)
        outbound = await channels.create_channel(65_536, created_at_ms=1_001)
        await waits.register_wait(
            session.principal_id, outbound.channel_id, 1, registered_at_ms=1_002
        )

        coord = json.loads(coord_path.read_text(encoding="utf-8"))
        brain_principal = bytes.fromhex(coord["brain_principal_hex"])
        wav_bytes = (b"RIFF\x00\x20\x03\x00WAVEfmt " + bytes(range(256)) * 800)[
            :VOICE_WAV_SIZE_BYTES
        ]
        wav_sha256 = hashlib.sha256(wav_bytes).digest()
        (wav_dir / "qq_voice_200kib.wav").write_bytes(wav_bytes)

        minted = await blobs.mint_reference(
            "qq_voice_200kib.wav",
            wav_sha256,
            len(wav_bytes),
            max_reads=8,
            valid_until_ms=4_102_444_800_000,
            recipient_principal_id=brain_principal,
        )
        blob_uri = format_blob_uri(minted.reference_id)
        coord.update(
            {
                "inbound_channel_hex": inbound.channel_id.hex(),
                "inbound_generation": inbound.generation,
                "outbound_channel_hex": outbound.channel_id.hex(),
                "outbound_generation": outbound.generation,
                "outbound_fencing_hex": outbound.fencing_token.hex(),
                "nlosref_uri": blob_uri,
                "adapter_ready": True,
            }
        )
        coord_path.write_text(json.dumps(coord), encoding="utf-8")

        envelope_msg = json.dumps(
            {
                "kind": "qq_voice_note",
                "uri": blob_uri,
                "size_bytes": len(wav_bytes),
                "sha256": wav_sha256.hex(),
            }
        ).encode("utf-8")
        pub_entry = await channels.enqueue(
            inbound.channel_id, inbound.generation, inbound.fencing_token, envelope_msg
        )
        await waits.notify_commits(
            inbound.channel_id, pub_entry.sequence, notified_at_ms=1_011
        )

        for _ in range(100):
            entries = await channels.receive(outbound.channel_id, limit=4)
            if entries:
                seq = entries[0].sequence
                await channels.register_queue_consumption(
                    outbound.channel_id,
                    outbound.generation,
                    seq,
                    session.principal_id,
                    1,
                )
                await channels.ack(outbound.channel_id, outbound.generation, seq)
                return {
                    "role": "adapter",
                    "principal_hex": session.principal_id.hex(),
                    "nlosref_uri": blob_uri,
                    "inbound_frame_bytes": len(envelope_msg),
                    "wav_size_bytes": len(wav_bytes),
                    "reply": json.loads(entries[0].payload.decode("utf-8")),
                    "session_exchanges": session.exchanges,
                }
            await asyncio.sleep(0.05)
        raise TimeoutError("adapter timed out waiting for brain outbound reply")
    finally:
        await session.close()


async def run_brain_worker(coord_path: Path) -> dict[str, Any]:
    """External ``brain`` worker: channel -> blob read -> LLM -> Wasm tool -> reply."""
    _assert_strict_three_env_contract()
    session = await _session_connect_from_env()
    try:
        ids = session.client_kwargs()
        directory = ServiceDirectoryClient(session.rpc, **ids)
        for svc, schema in (
            (CHANNEL_CONTROL_SERVICE, CHANNEL_CONTROL_SCHEMA),
            (WAIT_CONTROL_SERVICE, WAIT_CONTROL_SCHEMA),
            (OPERATION_CONTROL_SERVICE, OPERATION_CONTROL_SCHEMA),
            (LLM_GATEWAY_SERVICE, LLM_GATEWAY_SCHEMA),
            (TOOL_SANDBOX_SERVICE, TOOL_SANDBOX_SCHEMA),
            (BLOB_REFERENCE_SERVICE, BLOB_REFERENCE_SCHEMA),
        ):
            await directory.negotiate(ServiceRequirement(svc, schema, 1, 0))

        for _ in range(100):
            coord = json.loads(coord_path.read_text(encoding="utf-8"))
            if coord.get("adapter_ready"):
                break
            await asyncio.sleep(0.02)
        else:
            raise TimeoutError("brain timed out waiting for adapter channel setup")

        inbound_id = bytes.fromhex(coord["inbound_channel_hex"])
        inbound_gen = int(coord["inbound_generation"])
        outbound_id = bytes.fromhex(coord["outbound_channel_hex"])
        outbound_gen = int(coord["outbound_generation"])
        outbound_fence = bytes.fromhex(coord["outbound_fencing_hex"])

        channels = ChannelClient(session.rpc, **ids)
        waits = WaitClient(session.rpc, **ids)
        blobs = BlobReferenceClient(session.rpc, **ids)
        llm = LlmGatewayClient(session.rpc, **ids)
        tools = ToolSandboxClient(session.rpc, **ids)
        await waits.register_wait(
            session.principal_id, inbound_id, 1, registered_at_ms=1_005
        )

        msg: dict[str, Any] | None = None
        for _ in range(100):
            entries = await channels.receive(inbound_id, limit=4)
            if entries:
                seq = entries[0].sequence
                await channels.register_queue_consumption(
                    inbound_id, inbound_gen, seq, session.principal_id, 1
                )
                await channels.ack(inbound_id, inbound_gen, seq)
                msg = json.loads(entries[0].payload.decode("utf-8"))
                break
            await asyncio.sleep(0.05)
        assert msg is not None

        reference_id, total = parse_blob_uri(msg["uri"]), int(msg["size_bytes"])
        chunks, offset = [], 0
        while offset < total:
            res = await blobs.read_reference(
                reference_id, offset, min(MAX_BLOB_CHUNK_BYTES, total - offset))
            chunks.append(res.chunk)
            offset += len(res.chunk)
        audio_bytes = b"".join(chunks)
        assert len(audio_bytes) == VOICE_WAV_SIZE_BYTES
        assert hashlib.sha256(audio_bytes).hexdigest() == msg["sha256"]

        deadline = default_deadline_monotonic_ns()
        budget_before = await llm.query_budget()
        fast_chat = await llm.llm_chat(
            "glm-5.3",
            [("user", f"fast-classify:{msg['uri']}")],
            max_output_tokens=32,
            session_hint="qq-fast-brain",
            idempotency_key=secrets.token_bytes(16),
            deadline_monotonic_ns=deadline,
        )
        slow_chat = await llm.llm_chat(
            "glm-5.3",
            [("user", f"slow-plan:{fast_chat.content_utf8}")],
            max_output_tokens=64,
            session_hint="qq-slow-brain",
            idempotency_key=secrets.token_bytes(16),
            deadline_monotonic_ns=deadline,
        )
        sub_budget = await llm.attenuate_budget(
            LlmUsage(
                api_calls=2,
                prompt_tokens=512,
                completion_tokens=512,
                total_tokens=1_024,
                cost_microcredits=5_000,
            ),
            valid_until_ms=4_102_444_800_000,
            idempotency_key=secrets.token_bytes(16),
            deadline_monotonic_ns=deadline,
        )
        budget_before_ext = await llm.query_budget()
        ext_kwargs: dict[str, Any] = {
            "recorded_at_ms": 1_050,
            "idempotency_key": b"\x77" * 16,
            "deadline_monotonic_ns": deadline,
        }
        ext_usage = LlmUsage(
            api_calls=1, prompt_tokens=20, completion_tokens=10,
            total_tokens=30, cost_microcredits=100,
        )
        ext_first = await llm.record_external_usage(
            "external-asr", "whisper-large-v3", ext_usage, **ext_kwargs
        )
        ext_replay = await llm.record_external_usage(
            "external-asr", "whisper-large-v3", ext_usage, **ext_kwargs
        )
        budget_after_ext = await llm.query_budget()
        assert not ext_first.replayed and ext_replay.replayed
        assert (
            budget_after_ext.remaining.api_calls
            == budget_before_ext.remaining.api_calls
        )

        catalog = await tools.list_tools()
        assert "voice_note_transcribe" in [t.name for t in catalog.tools]
        inv_res = await tools.invoke_tool(
            "voice_note_transcribe",
            {"uri": msg["uri"]},
            idempotency_key=secrets.token_bytes(16),
            deadline_monotonic_ns=deadline,
        )
        # invoke_tool returns the completed output synchronously; the wire
        # carries no OperationReference to query.
        tool_output = inv_res.parsed_output()

        rel_res = await blobs.release_reference(parse_blob_uri(msg["uri"]))
        assert rel_res.released

        reply_payload = {
            "status": "delivered",
            "verified_sha256": hashlib.sha256(audio_bytes).hexdigest(),
            "fast_model": fast_chat.model,
            "slow_summary": json.loads(slow_chat.content_utf8)["intent"],
            "sub_budget_tokens": sub_budget.granted_limit.total_tokens,
            "tool_output": tool_output,
            "budget_calls_before": budget_before.remaining.api_calls,
            "budget_calls_after": budget_after_ext.remaining.api_calls,
            "blob_released": rel_res.released,
        }
        out_entry = await channels.enqueue(
            outbound_id,
            outbound_gen,
            outbound_fence,
            json.dumps(reply_payload).encode("utf-8"),
        )
        await waits.notify_commits(
            outbound_id, out_entry.sequence, notified_at_ms=1_081
        )
        return {
            "role": "brain",
            "principal_hex": session.principal_id.hex(),
            "session_exchanges": session.exchanges,
            "reply_payload": reply_payload,
        }
    finally:
        await session.close()
# === END_QQ_AGENT_CORE ===


def count_core_agent_lines() -> int:
    """Count non-empty, non-comment lines inside the core qq-agent section."""
    source = Path(__file__).read_text(encoding="utf-8")
    _, after_begin = source.split("# === BEGIN_QQ_AGENT_CORE ===", 1)
    core_block, _ = after_begin.split("# === END_QQ_AGENT_CORE ===", 1)
    lines = [
        line
        for line in core_block.splitlines()
        if line.strip() and not line.strip().startswith("#")
    ]
    return len(lines)


def _resolve_daemon_binary() -> Path:
    env_bin = os.environ.get("NLOS_SYSTEM_CONTROL_DAEMON_BIN")
    if env_bin and Path(env_bin).exists():
        return Path(env_bin)
    suffix = ".exe" if os.name == "nt" else ""
    candidate = REPO_ROOT / "target" / "debug" / f"system-control-daemon{suffix}"
    if not candidate.exists():
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
            cwd=REPO_ROOT,
            check=True,
        )
    return candidate


def _write_seed_file(path: Path, seed_byte: int) -> tuple[bytes, bytes]:
    seed = bytes([seed_byte]) * 32
    path.write_text(seed.hex(), encoding="utf-8")
    os.chmod(path, 0o600)
    signer = Ed25519Signer(seed)
    principal_id = derive_daemon_principal_id(signer.public_key)
    return seed, principal_id


def run_dual_process_dogfood_e2e() -> dict[str, Any]:
    """Launch real ``system-control-daemon`` and two external Python worker
    processes (``adapter`` + ``brain``) with only the 3-tuple ``LLMOS_*`` env.
    """
    core_lines = count_core_agent_lines()
    assert core_lines <= MAX_CORE_AGENT_LINES, (
        f"qq-agent Phase 2 core business code must be <= {MAX_CORE_AGENT_LINES} lines, "
        f"got {core_lines}"
    )

    daemon_bin = _resolve_daemon_binary()
    with tempfile.TemporaryDirectory(prefix="w82-dogfood-") as tmp:
        root = Path(tmp)
        auth_sock = root / "auth.sock"
        plain_sock = root / "plain.sock"
        adapter_key = root / "adapter.key"
        brain_key = root / "brain.key"
        coord_path = root / "coord.json"

        _, adapter_principal = _write_seed_file(adapter_key, 0x41)
        _, brain_principal = _write_seed_file(brain_key, 0x42)

        coord_path.write_text(
            json.dumps(
                {
                    "adapter_principal_hex": adapter_principal.hex(),
                    "brain_principal_hex": brain_principal.hex(),
                    "adapter_ready": False,
                }
            ),
            encoding="utf-8",
        )

        daemon_proc = subprocess.Popen(
            [
                str(daemon_bin),
                "--root",
                str(root / "state"),
                "--auth-socket",
                str(auth_sock),
                "--plain-socket",
                str(plain_sock),
                "--identity-key-file",
                str(adapter_key),
                "--operator-key-file",
                str(brain_key),
            ],
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        try:
            assert daemon_proc.stdout is not None
            ready_line = ""
            deadline = time.monotonic() + 20.0
            while time.monotonic() < deadline:
                line = daemon_proc.stdout.readline()
                if line.startswith("READY "):
                    ready_line = line.strip()
                    break
                if daemon_proc.poll() is not None:
                    stderr_out = (
                        daemon_proc.stderr.read() if daemon_proc.stderr else ""
                    )
                    raise RuntimeError(
                        f"system-control-daemon exited early: {stderr_out}"
                    )
            assert ready_line.startswith("READY "), "daemon did not emit READY line"

            base_pythonpath = os.pathsep.join([str(SDK_ROOT), str(GEN_PYTHON_ROOT)])
            base_env = {
                "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
                "PYTHONPATH": base_pythonpath,
                "SYSTEMROOT": os.environ.get("SYSTEMROOT", ""),
            }
            adapter_env = {
                **base_env,
                "LLMOS_IPC_SOCKET": str(auth_sock),
                "LLMOS_PRINCIPAL_KEY_PATH": str(adapter_key),
                "LLMOS_ROOT_CAPABILITY_SLOT": "a1" * 16,
            }
            brain_env = {
                **base_env,
                "LLMOS_IPC_SOCKET": str(auth_sock),
                "LLMOS_PRINCIPAL_KEY_PATH": str(brain_key),
                "LLMOS_ROOT_CAPABILITY_SLOT": "b2" * 16,
            }

            wav_dir = root / "state" / "blob_workspace"
            wav_dir.mkdir(parents=True, exist_ok=True)

            brain_proc = subprocess.Popen(
                [
                    sys.executable,
                    str(Path(__file__).resolve()),
                    "--worker-role",
                    "brain",
                    "--coord",
                    str(coord_path),
                    "--wav-dir",
                    str(wav_dir),
                ],
                env=brain_env,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
            adapter_proc = subprocess.Popen(
                [
                    sys.executable,
                    str(Path(__file__).resolve()),
                    "--worker-role",
                    "adapter",
                    "--coord",
                    str(coord_path),
                    "--wav-dir",
                    str(wav_dir),
                ],
                env=adapter_env,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )

            adapter_out, adapter_err = adapter_proc.communicate(timeout=30)
            brain_out, brain_err = brain_proc.communicate(timeout=30)

            assert brain_proc.returncode == 0, (
                f"brain worker failed ({brain_proc.returncode}): {brain_err}\nadapter_err={adapter_err}"
            )
            assert adapter_proc.returncode == 0, (
                f"adapter worker failed ({adapter_proc.returncode}): {adapter_err}\nbrain_err={brain_err}"
            )

            adapter_summary = json.loads(adapter_out.strip())
            brain_summary = json.loads(brain_out.strip())
            return {
                "core_agent_lines": core_lines,
                "phase1_baseline_lines": W81_PHASE1_BASELINE_LINES,
                "line_reduction_ratio": round(
                    1.0 - (core_lines / W81_PHASE1_BASELINE_LINES), 4
                ),
                "adapter": adapter_summary,
                "brain": brain_summary,
            }
        finally:
            daemon_proc.terminate()
            try:
                daemon_proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                daemon_proc.kill()
                daemon_proc.wait(timeout=5)


def test_qq_agent_v1_1_dual_process_dogfood_e2e() -> None:
    if os.name == "nt":
        return
    report = run_dual_process_dogfood_e2e()
    assert report["core_agent_lines"] <= MAX_CORE_AGENT_LINES
    assert report["line_reduction_ratio"] > 0.65

    adapter = report["adapter"]
    brain = report["brain"]
    assert adapter["nlosref_uri"].startswith(BLOB_URI_PREFIX)
    assert len(adapter["nlosref_uri"]) == len(BLOB_URI_PREFIX) + 32
    assert adapter["inbound_frame_bytes"] < 512
    assert adapter["wav_size_bytes"] == VOICE_WAV_SIZE_BYTES

    reply = adapter["reply"]
    assert reply["status"] == "delivered"
    assert reply["fast_model"] == "glm-5.3"
    assert reply["slow_summary"] == "voice_note_reply"
    assert reply["sub_budget_tokens"] == 1_024
    assert reply["tool_output"]["tool"] == "voice_note_transcribe"
    assert reply["tool_output"]["sandboxed"] is True
    assert reply["budget_calls_after"] < reply["budget_calls_before"]
    assert reply["blob_released"] is True
    assert brain["session_exchanges"] >= 15


def main() -> int:
    if "--worker-role" in sys.argv:
        role_idx = sys.argv.index("--worker-role") + 1
        coord_idx = sys.argv.index("--coord") + 1
        wav_idx = sys.argv.index("--wav-dir") + 1
        role = sys.argv[role_idx]
        coord_path = Path(sys.argv[coord_idx])
        wav_dir = Path(sys.argv[wav_idx])
        if role == "adapter":
            res = asyncio.run(run_adapter_worker(coord_path, wav_dir))
        elif role == "brain":
            res = asyncio.run(run_brain_worker(coord_path))
        else:
            raise ValueError(f"unknown worker role {role!r}")
        print(json.dumps(res))
        return 0

    report = run_dual_process_dogfood_e2e()
    print(json.dumps(report, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
