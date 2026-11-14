"""ADR-0011 连接级 principal 挑战应答握手——Rust 冻结契约的 Python 客户端镜像。

对位源(逐字节镜像,勿单独演进):
- ``crates/nlos-ipc/src/handshake.rs:144-155`` — 摘要构造
- ``crates/nlos-ipc/src/handshake/transport.rs:70-107`` — 端点通道绑定
- ``crates/nlos-schema/src/lib.rs`` — 长度边界与 schema 身份

本模块只有纯函数;帧 I/O 与会话生命周期在 ``principal_session``。
"""

from __future__ import annotations

import hashlib
import os
import sys
from typing import Any

from google.protobuf.message import DecodeError

from .local_rpc import IpcError, _ensure_gen_python_path

_ensure_gen_python_path()

from nlos.sabi.v1.principal_handshake_pb2 import (  # noqa: E402
    PrincipalHandshakeAttestation,
    PrincipalHandshakeChallenge,
)

#: 握手消息域分隔(handshake.rs:62)
HANDSHAKE_MESSAGE_DOMAIN: bytes = b"llmos/principal-handshake/v1"

#: 端点通道绑定域分隔(transport.rs:72)
CHANNEL_BINDING_DOMAIN: bytes = b"llmos/ipc-channel-binding/v1"

HANDSHAKE_NONCE_BYTES = 32
HANDSHAKE_SIGNATURE_BYTES = 64
HANDSHAKE_PRINCIPAL_ID_BYTES = 16
MAX_HANDSHAKE_CHANNEL_BINDING_BYTES = 256

HANDSHAKE_SCHEMA_NAME = "nlos.sabi.PrincipalHandshake"
HANDSHAKE_SCHEMA_MAJOR = 1
HANDSHAKE_SCHEMA_MINOR = 0


def principal_handshake_message(
    nonce: bytes,
    principal_id: bytes,
    channel_binding: bytes,
) -> bytes:
    """客户端签名、服务端验证的摘要:域 ‖ nonce ‖ principal ‖ binding。

    固定宽度 nonce/principal 在前,变长 binding 收尾,拼接无歧义
    (镜像 handshake.rs:139-155)。
    """

    _require_length("nonce", nonce, HANDSHAKE_NONCE_BYTES)
    _require_length("principal_id", principal_id, HANDSHAKE_PRINCIPAL_ID_BYTES)
    _require_channel_binding(channel_binding)
    return hashlib.sha256(
        HANDSHAKE_MESSAGE_DOMAIN + bytes(nonce) + bytes(principal_id) + bytes(channel_binding)
    ).digest()


def endpoint_path_bytes(endpoint: str, platform: str | None = None) -> bytes:
    """端点路径喂入绑定的字节形态(镜像 transport.rs:92-107)。

    Unix = OS 字符串原始字节;Windows = 管道名 UTF-16 码元按本机字节序。
    同一端点两端永远同 OS,编码不会在同一线上混用。
    """

    resolved = platform if platform is not None else sys.platform
    if resolved == "win32":
        encoding = "utf-16-le" if sys.byteorder == "little" else "utf-16-be"
        return endpoint.encode(encoding)
    return os.fsencode(endpoint)


def endpoint_channel_binding(endpoint: str, platform: str | None = None) -> bytes:
    """两端对端点路径共识的通道绑定:绑定域 ‖ 路径字节的 SHA-256。"""

    return hashlib.sha256(
        CHANNEL_BINDING_DOMAIN + endpoint_path_bytes(endpoint, platform)
    ).digest()


def decode_challenge(wire: bytes) -> PrincipalHandshakeChallenge:
    """解码并校验服务端挑战帧;任何失配抛类型化 ``IpcError``。"""

    challenge = PrincipalHandshakeChallenge()
    try:
        challenge.ParseFromString(wire)
    except DecodeError as error:
        raise IpcError(
            "COMPATIBILITY", "malformed PrincipalHandshakeChallenge"
        ) from error
    if (
        not challenge.HasField("schema")
        or challenge.schema.name != HANDSHAKE_SCHEMA_NAME
        or challenge.schema.major != HANDSHAKE_SCHEMA_MAJOR
    ):
        raise IpcError(
            "COMPATIBILITY",
            f"unexpected handshake schema {challenge.schema.name!r}",
        )
    _require_length("nonce", challenge.nonce, HANDSHAKE_NONCE_BYTES)
    return challenge


def build_attestation(
    principal_id: bytes,
    nonce: bytes,
    channel_binding: bytes,
    signature: bytes,
) -> bytes:
    """构造客户端证明帧(镜像 client_attestation + 长度边界校验)。"""

    _require_length("principal_id", principal_id, HANDSHAKE_PRINCIPAL_ID_BYTES)
    _require_length("nonce", nonce, HANDSHAKE_NONCE_BYTES)
    _require_length("signature", signature, HANDSHAKE_SIGNATURE_BYTES)
    _require_channel_binding(channel_binding)
    attestation = PrincipalHandshakeAttestation()
    attestation.schema.name = HANDSHAKE_SCHEMA_NAME
    attestation.schema.major = HANDSHAKE_SCHEMA_MAJOR
    attestation.schema.minor = HANDSHAKE_SCHEMA_MINOR
    attestation.principal_id = bytes(principal_id)
    attestation.nonce = bytes(nonce)
    attestation.channel_binding = bytes(channel_binding)
    attestation.signature = bytes(signature)
    return attestation.SerializeToString(deterministic=True)


def _require_length(field: str, value: Any, expected: int) -> None:
    actual = len(value)
    if actual != expected:
        raise IpcError(
            "COMPATIBILITY",
            f"handshake {field} has {actual} bytes; exactly {expected} required",
        )


def _require_channel_binding(binding: bytes) -> None:
    actual = len(binding)
    if actual == 0 or actual > MAX_HANDSHAKE_CHANNEL_BINDING_BYTES:
        raise IpcError(
            "COMPATIBILITY",
            "handshake channel binding has "
            f"{actual} bytes; 1..={MAX_HANDSHAKE_CHANNEL_BINDING_BYTES} required",
        )


__all__ = [
    "CHANNEL_BINDING_DOMAIN",
    "HANDSHAKE_MESSAGE_DOMAIN",
    "HANDSHAKE_NONCE_BYTES",
    "HANDSHAKE_PRINCIPAL_ID_BYTES",
    "HANDSHAKE_SCHEMA_MAJOR",
    "HANDSHAKE_SCHEMA_MINOR",
    "HANDSHAKE_SCHEMA_NAME",
    "HANDSHAKE_SIGNATURE_BYTES",
    "MAX_HANDSHAKE_CHANNEL_BINDING_BYTES",
    "build_attestation",
    "decode_challenge",
    "endpoint_channel_binding",
    "endpoint_path_bytes",
    "principal_handshake_message",
]
