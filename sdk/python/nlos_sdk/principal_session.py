"""ADR-0011 认证持久会话:连接级握手 + 单连接顺序多轮的最小正确版。

握手字节与失败顺序逐字节镜像 Rust ``authenticated_connect``
(``crates/nlos-ipc/src/handshake/transport.rs:397-421``):收挑战帧 → 解码 →
按端点路径派生通道绑定 → 摘要 → 签名 → 发证明帧。Rust 服务端在证明后
**没有 ack**:接受与否由首轮 exchange 揭晓,拒绝表现为服务端直接断连。

会话生命周期:证明后把同一 reader/writer 交给 :class:`LocalRpcClient`,
后续多轮顺序复用、并发背压、失败毒化全部继承其语义(镜像
``crates/nlos-ipc/src/lib.rs:398-477`` 的 ``LocalRpcClient``)。

扩展缝(W82-A 未决):五通道同连接多路复用的最终形状未冻结;本模块只做
「认证会话 + 单通道复用」,``connect_authenticated`` 返回可复用的
``LocalRpcClient``,将来 mux 层在其上加通道维度,不破坏本 API。
"""

from __future__ import annotations

import asyncio
import secrets
from dataclasses import dataclass

from ._signing import Ed25519Signer
from .handshake import (
    HANDSHAKE_PRINCIPAL_ID_BYTES,
    build_attestation,
    decode_challenge,
    endpoint_channel_binding,
    principal_handshake_message,
)
from .local_rpc import (
    IpcError,
    LocalRpcClient,
    TransportConfig,
    _open_platform_stream,
    _read_frame,
    _write_frame,
)

_ID_BYTES = 16


@dataclass(frozen=True, slots=True)
class SessionIdentity:
    """会话派生身份:principal 来自握手,其余字段每会话随机派生。

    取代历史占位常量(``DEFAULT_PRINCIPAL_ID`` 等 b"\\x31"*16 字面量),
    不再让全部调用方在线上共享同一身份字节。
    """

    principal_id: bytes
    application_id: bytes
    process_id: bytes
    process_generation: int = 1

    def __post_init__(self) -> None:
        for name in ("principal_id", "application_id", "process_id"):
            value = bytes(getattr(self, name))
            if len(value) != _ID_BYTES:
                raise ValueError(f"{name} must contain exactly {_ID_BYTES} bytes")
        if not isinstance(self.process_generation, int) or self.process_generation <= 0:
            raise ValueError("process_generation must be positive")

    @classmethod
    def derive(cls, principal_id: bytes) -> "SessionIdentity":
        """以握手 principal 为根派生会话身份(application/process 随机)。"""

        return cls(
            principal_id=bytes(principal_id),
            application_id=secrets.token_bytes(_ID_BYTES),
            process_id=secrets.token_bytes(_ID_BYTES),
        )


async def connect_authenticated(
    endpoint: str,
    signer: Ed25519Signer,
    principal_id: bytes,
    config: TransportConfig | None = None,
) -> LocalRpcClient:
    """连接本地端点、应答服务端挑战,返回同连接的持久 ``LocalRpcClient``。

    通道绑定按 ``endpoint`` 派生(镜像 ``endpoint_channel_binding``),服务端
    必须对同一端点路径做同样派生。握手任何一步失败都关闭连接并抛类型化
    ``IpcError``——失败闭合,无半开会话残留。
    """

    if not endpoint:
        raise IpcError("INVALID_CONFIG", "IPC endpoint must not be empty")
    principal = bytes(principal_id)
    if len(principal) != HANDSHAKE_PRINCIPAL_ID_BYTES:
        raise IpcError(
            "INVALID_CONFIG",
            f"principal_id must contain exactly {HANDSHAKE_PRINCIPAL_ID_BYTES} bytes",
        )
    resolved = config or TransportConfig()
    try:
        reader, writer = await asyncio.wait_for(
            _open_platform_stream(endpoint),
            timeout=resolved.connect_timeout,
        )
    except TimeoutError as error:
        raise IpcError("TIMEOUT", "IPC connect timed out") from error
    except OSError as error:
        raise IpcError("CONNECT", "IPC connect failed") from error

    try:
        challenge_wire = await asyncio.wait_for(
            _read_frame(reader, resolved.maximum_frame_bytes),
            timeout=resolved.read_timeout,
        )
        challenge = decode_challenge(challenge_wire)
        binding = endpoint_channel_binding(endpoint)
        digest = principal_handshake_message(challenge.nonce, principal, binding)
        signature = signer.sign(digest)
        attestation = build_attestation(
            principal, challenge.nonce, binding, signature
        )
        await _write_frame(writer, attestation, resolved)
    except IpcError:
        writer.close()
        await _swallow_wait_closed(writer)
        raise
    except TimeoutError as error:
        writer.close()
        await _swallow_wait_closed(writer)
        raise IpcError("TIMEOUT", "handshake timed out") from error
    except OSError as error:
        writer.close()
        await _swallow_wait_closed(writer)
        raise IpcError("READ", "handshake transport failed") from error
    except Exception as error:
        writer.close()
        await _swallow_wait_closed(writer)
        raise IpcError("HANDSHAKE", f"handshake failed: {error}") from error
    return LocalRpcClient(reader, writer, resolved)


async def _swallow_wait_closed(writer: asyncio.StreamWriter) -> None:
    try:
        await writer.wait_closed()
    except OSError:
        pass


__all__ = ["SessionIdentity", "connect_authenticated"]
