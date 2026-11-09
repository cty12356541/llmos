"""认证持久会话客户端:握手、单连接顺序多轮、背压、毒化、失败闭合。

进程内测试服务器镜像 Rust 服务端行为(transport.rs 的服务端半 +
serve_one 的单连接多轮顺序服务):发挑战 → 验证明 → 同连接循环 echo。
真实 Rust 服务端(system-control-daemon)的端到端在
``tests/conformance/ipc/authenticated_session.py``。
"""

from __future__ import annotations

import asyncio
import secrets
import sys
import tempfile
from contextlib import asynccontextmanager
from pathlib import Path
from typing import AsyncIterator

import pytest

from nlos_sdk._signing import Ed25519Signer
from nlos_sdk.channel import ChannelClient, ChannelControlError
from nlos_sdk.handshake import (
    build_attestation,
    decode_challenge,
    endpoint_channel_binding,
    principal_handshake_message,
)
from nlos_sdk.local_rpc import IpcError, TransportConfig, _read_frame, _write_frame
from nlos_sdk.principal_session import (
    SessionIdentity,
    connect_authenticated,
)

if sys.platform == "win32":  # pragma: no cover - 本套在 Unix 跑,Windows 由 CI 三平台覆盖
    pytest.skip("进程内 Unix socket 测试服务器仅覆盖非 Windows", allow_module_level=True)

from nlos.sabi.v1.envelope_pb2 import (  # noqa: E402
    RETRY_DIRECTIVE_DO_NOT_RETRY,
    SABI_ERROR_CODE_NOT_FOUND,
    ExchangeRequest,
    ExchangeResponse,
)


class HandshakeServer:
    """镜像 Rust 服务端的进程内挑战应答服务器。

    记录 connections/exchanges/attestations 供断言;可注入
    reject_attestation / garbage_challenge / close_after_round 行为。
    """

    def __init__(
        self,
        *,
        signer: Ed25519Signer,
        rounds: int = 16,
        response_delay: float = 0.0,
        challenge_delay: float = 0.0,
        reject_attestation: bool = False,
        garbage_challenge: bool = False,
        failure_envelopes: bool = False,
    ) -> None:
        self._signer = signer
        self._rounds = rounds
        self._response_delay = response_delay
        self._challenge_delay = challenge_delay
        self._reject_attestation = reject_attestation
        self._garbage_challenge = garbage_challenge
        self._failure_envelopes = failure_envelopes
        self._server: asyncio.AbstractServer | None = None
        self._handlers: set[asyncio.Task[None]] = set()
        self.path: str = ""
        self.connections = 0
        self.exchanges = 0
        self.attestations = 0
        self.rejected = 0
        self.handlers_done = 0

    async def start(self) -> None:
        tmp = Path(tempfile.gettempdir())
        self.path = str(
            tmp / f"nlos-session-test-{secrets.token_hex(8)}.sock"
        )
        self._server = await asyncio.start_unix_server(self._handle, self.path)

    async def stop(self) -> None:
        if self._server is not None:
            self._server.close()
            for handler in self._handlers:
                handler.cancel()
            if self._handlers:
                await asyncio.gather(*self._handlers, return_exceptions=True)
            self._handlers.clear()
            await self._server.wait_closed()
        Path(self.path).unlink(missing_ok=True)

    async def _handle(
        self, reader: asyncio.StreamReader, writer: asyncio.StreamWriter
    ) -> None:
        task = asyncio.current_task()
        if task is not None:
            self._handlers.add(task)
        config = TransportConfig()
        self.connections += 1
        try:
            if self._challenge_delay:
                await asyncio.sleep(self._challenge_delay)
            if self._garbage_challenge:
                writer.write(b"\x00\x00\x00\x04\xff\xff\xff\xff")
                await writer.drain()
                return
            nonce = secrets.token_bytes(32)
            challenge = build_challenge_wire(nonce)
            await _write_frame(writer, challenge, config)
            attestation_wire = await _read_frame(reader, config.maximum_frame_bytes)
            self.attestations += 1
            if not self._verify(attestation_wire, nonce):
                self.rejected += 1
                return
            for _ in range(self._rounds):
                request_wire = await _read_frame(reader, config.maximum_frame_bytes)
                self.exchanges += 1
                if self._response_delay:
                    await asyncio.sleep(self._response_delay)
                response = self._response_for(request_wire)
                await _write_frame(
                    writer, response.SerializeToString(deterministic=True), config
                )
        except (asyncio.IncompleteReadError, ConnectionResetError, OSError):
            return
        finally:
            self.handlers_done += 1
            writer.close()
            try:
                await writer.wait_closed()
            except OSError:
                pass

    def _verify(self, attestation_wire: bytes, nonce: bytes) -> bool:
        try:
            from nlos.sabi.v1.principal_handshake_pb2 import (
                PrincipalHandshakeAttestation,
            )

            attestation = PrincipalHandshakeAttestation()
            attestation.ParseFromString(attestation_wire)
        except Exception:
            return False
        if self._reject_attestation:
            return False
        if attestation.nonce != nonce:
            return False
        if attestation.channel_binding != endpoint_channel_binding(self.path):
            return False
        from nacl.signing import VerifyKey

        try:
            VerifyKey(self._signer.public_key).verify(
                principal_handshake_message(
                    nonce, attestation.principal_id, attestation.channel_binding
                ),
                attestation.signature,
            )
        except Exception:
            return False
        return True

    def _response_for(self, request_wire: bytes) -> ExchangeResponse:
        request = ExchangeRequest()
        request.ParseFromString(request_wire)
        response = ExchangeResponse()
        response.envelope.schema.name = request.envelope.schema.name
        response.envelope.schema.major = request.envelope.schema.major
        response.envelope.schema.minor = request.envelope.schema.minor
        response.envelope.request_id = request.envelope.request_id
        response.envelope.service = request.envelope.service
        response.envelope.method = request.envelope.method
        if self._failure_envelopes:
            context = response.envelope.response_context
            context.correlation_id = request.envelope.request_context.correlation_id
            context.failure.code = SABI_ERROR_CODE_NOT_FOUND
            context.failure.retry = RETRY_DIRECTIVE_DO_NOT_RETRY
            context.failure.safe_message = "harness not-found"
        return response


def build_challenge_wire(nonce: bytes) -> bytes:
    from nlos.sabi.v1.principal_handshake_pb2 import PrincipalHandshakeChallenge
    from nlos_sdk.handshake import HANDSHAKE_SCHEMA_MAJOR, HANDSHAKE_SCHEMA_MINOR, HANDSHAKE_SCHEMA_NAME

    challenge = PrincipalHandshakeChallenge()
    challenge.schema.name = HANDSHAKE_SCHEMA_NAME
    challenge.schema.major = HANDSHAKE_SCHEMA_MAJOR
    challenge.schema.minor = HANDSHAKE_SCHEMA_MINOR
    challenge.nonce = nonce
    return challenge.SerializeToString(deterministic=True)


def build_request(request_id: bytes) -> ExchangeRequest:
    request = ExchangeRequest()
    request.envelope.schema.name = "nlos.sabi.Envelope"
    request.envelope.schema.major = 1
    request.envelope.schema.minor = 1
    request.envelope.request_id = request_id
    request.envelope.service = "channel"
    request.envelope.method = "inspect_channel"
    return request


@asynccontextmanager
async def session_server(**kwargs: object) -> AsyncIterator[HandshakeServer]:
    """在测试自身事件循环上起停测试服务器(避免跨 loop 的 async fixture)。"""

    signer = kwargs.pop("signer", None) or Ed25519Signer(secrets.token_bytes(32))
    server = HandshakeServer(signer=signer, **kwargs)  # type: ignore[arg-type]
    await server.start()
    try:
        yield server
    finally:
        await server.stop()


@pytest.mark.asyncio
async def test_handshake_then_sequential_rounds_on_one_connection() -> None:
    async with session_server(rounds=4) as server:
        signer = server._signer
        rpc = await connect_authenticated(
            server.path, signer, principal_id=b"\x42" * 16
        )
        for round_index in range(3):
            request = build_request(secrets.token_bytes(16))
            response = await rpc.exchange(request)
            assert response.envelope.request_id == request.envelope.request_id
        assert server.connections == 1
        assert server.attestations == 1
        assert server.exchanges == 3
        await rpc.close()


@pytest.mark.asyncio
async def test_concurrent_second_exchange_fails_with_backpressure() -> None:
    async with session_server(rounds=4, response_delay=0.2) as server:
        rpc = await connect_authenticated(
            server.path, server._signer, principal_id=b"\x42" * 16
        )
        first = asyncio.create_task(
            rpc.exchange(build_request(secrets.token_bytes(16)))
        )
        await asyncio.sleep(0.05)
        with pytest.raises(IpcError, match="in-flight"):
            await rpc.exchange(build_request(secrets.token_bytes(16)))
        await first
        await rpc.close()


@pytest.mark.asyncio
async def test_poisoned_after_mid_exchange_disconnect() -> None:
    async with session_server(rounds=1) as server:
        rpc = await connect_authenticated(
            server.path, server._signer, principal_id=b"\x42" * 16
        )
        request = build_request(secrets.token_bytes(16))
        response = await rpc.exchange(request)
        assert response.envelope.request_id == request.envelope.request_id
        # 连接已被服务端关闭:下一轮必须失败闭合并毒化
        with pytest.raises(IpcError):
            await rpc.exchange(build_request(secrets.token_bytes(16)))
        with pytest.raises(IpcError, match="unusable"):
            await rpc.exchange(build_request(secrets.token_bytes(16)))


@pytest.mark.asyncio
async def test_wrong_key_attestation_is_rejected_and_first_exchange_fails() -> None:
    async with session_server(rounds=4) as server:
        stranger = Ed25519Signer(secrets.token_bytes(32))
        rpc = await connect_authenticated(
            server.path, stranger, principal_id=b"\x42" * 16
        )
        with pytest.raises(IpcError):
            await rpc.exchange(build_request(secrets.token_bytes(16)))
        assert server.rejected == 1
        assert server.exchanges == 0


@pytest.mark.asyncio
async def test_garbage_challenge_fails_typed_and_closes() -> None:
    async with session_server(garbage_challenge=True) as server:
        with pytest.raises(IpcError, match="malformed"):
            await connect_authenticated(
                server.path, server._signer, principal_id=b"\x42" * 16
            )
        assert server.attestations == 0


@pytest.mark.asyncio
async def test_channel_client_reuses_authenticated_session_across_calls() -> None:
    async with session_server(rounds=8, failure_envelopes=True) as server:
        principal = b"\x42" * 16
        client = await ChannelClient.connect_authenticated(
            server.path,
            signer=server._signer,
            principal_id=principal,
            config=TransportConfig(read_timeout=2.0),
        )
        try:
            for _ in range(3):
                with pytest.raises(ChannelControlError):
                    await client.inspect_channel(b"\x99" * 16)
        finally:
            await client.close()
        assert server.connections == 1, "三条 RPC 必须复用同一认证连接"
        assert server.exchanges == 3
        assert client._principal_id == principal


@pytest.mark.asyncio
async def test_external_cancellation_mid_exchange_poisons_session() -> None:
    """镜像 lib.rs:445-448 预毒化:外部取消(如 wait_for)后连接必须不可复用。

    回归锚:曾复现为取消后 _usable 仍 True,下一轮读到上一轮的滞留响应,
    幂等重试场景下静默拿到陈旧答案(对抗审查确认项 1/3)。
    """

    async with session_server(rounds=4, response_delay=0.3) as server:
        rpc = await connect_authenticated(
            server.path, server._signer, principal_id=b"\x42" * 16
        )
        with pytest.raises(TimeoutError):
            await asyncio.wait_for(
                rpc.exchange(build_request(secrets.token_bytes(16))),
                timeout=0.05,
            )
        # 取消后:连接必须已毒化,不得把滞留的第一轮响应当第二轮答案
        with pytest.raises(IpcError, match="unusable"):
            await rpc.exchange(build_request(secrets.token_bytes(16)))


@pytest.mark.asyncio
async def test_cancelled_connect_closes_transport_no_leak() -> None:
    """外部取消握手必须关闭传输,不留半开连接(审查确认项 2/5)。"""

    async with session_server(rounds=1, challenge_delay=0.3) as server:
        with pytest.raises(TimeoutError):
            await asyncio.wait_for(
                connect_authenticated(
                    server.path, server._signer, principal_id=b"\x42" * 16
                ),
                timeout=0.05,
            )
        for _ in range(50):
            if server.handlers_done >= 1:
                break
            await asyncio.sleep(0.02)
        assert server.handlers_done == 1, "取消后服务端 handler 必须看到断连退出"


@pytest.mark.asyncio
async def test_connect_authenticated_closes_session_when_construction_fails() -> None:
    """握手成功但会话构造失败必须关连接(审查确认项 4)。"""

    async with session_server(rounds=2) as server:
        with pytest.raises(ValueError):
            await ChannelClient.connect_authenticated(
                server.path,
                signer=server._signer,
                principal_id=b"\x42" * 16,
                application_id=b"wrong-length",
            )
        for _ in range(50):
            if server.handlers_done >= 1:
                break
            await asyncio.sleep(0.02)
        assert server.handlers_done == 1, "构造失败必须关闭已握手的连接"


@pytest.mark.asyncio
async def test_channel_client_close_is_public_and_idempotent() -> None:
    """公开 close()(审查确认项 6):认证会话与明文 target 均可安全关闭。"""

    async with session_server(rounds=2) as server:
        client = await ChannelClient.connect_authenticated(
            server.path, signer=server._signer, principal_id=b"\x42" * 16
        )
        await client.close()
        await client.close()  # 幂等
        with pytest.raises(IpcError, match="unusable"):
            await client._target.exchange(build_request(secrets.token_bytes(16)))
    plain = ChannelClient("some-unused-endpoint")
    await plain.close()  # 明文一次性 target:无持久连接,不抛


def test_session_identity_derives_random_non_principal_ids() -> None:
    identity = SessionIdentity.derive(b"\x42" * 16)
    assert identity.principal_id == b"\x42" * 16
    assert len(identity.application_id) == 16
    assert len(identity.process_id) == 16
    assert identity.process_generation == 1
    other = SessionIdentity.derive(b"\x42" * 16)
    assert identity.application_id != other.application_id
    assert identity.process_id != other.process_id


def test_session_identity_rejects_malformed_ids() -> None:
    with pytest.raises(ValueError):
        SessionIdentity(b"\x42" * 15, b"a" * 16, b"b" * 16)
    with pytest.raises(ValueError):
        SessionIdentity(b"\x42" * 16, b"a" * 16, b"b" * 16, process_generation=0)
