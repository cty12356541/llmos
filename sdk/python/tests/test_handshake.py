"""handshake 契约镜像的金向量与失败闭合测试。

向量在计划期用 /usr/bin/shasum -a 256 独立计算(非 Python hashlib),
对位 Rust 冻结契约:handshake.rs:144-155(摘要)、transport.rs:85-107(绑定)。
"""

from __future__ import annotations

import sys

import pytest

from nlos_sdk.handshake import (  # noqa: E402  (先导入以挂 gen/python 路径)
    CHANNEL_BINDING_DOMAIN,
    HANDSHAKE_MESSAGE_DOMAIN,
    HANDSHAKE_NONCE_BYTES,
    HANDSHAKE_SCHEMA_MAJOR,
    HANDSHAKE_SCHEMA_MINOR,
    HANDSHAKE_SCHEMA_NAME,
    MAX_HANDSHAKE_CHANNEL_BINDING_BYTES,
    build_attestation,
    decode_challenge,
    endpoint_channel_binding,
    endpoint_path_bytes,
    principal_handshake_message,
)
from nlos_sdk.local_rpc import IpcError  # noqa: E402

from nlos.sabi.v1.principal_handshake_pb2 import (  # noqa: E402
    PrincipalHandshakeAttestation,
    PrincipalHandshakeChallenge,
)

DIGEST_VECTOR = bytes.fromhex(
    "d8137d26b194334870f733a179ed4c93b6ce0a6a7b1039a6b1c7a66a97c91e01"
)
UNIX_BINDING_VECTOR = bytes.fromhex(
    "8f32c84889dc916ffc10d2b2dc9f9a5a4608a447b38dce1947a5fab3496cb8f9"
)
WINDOWS_UTF16LE_VECTOR = bytes.fromhex(
    "5c005c002e005c0070006900700065005c006e006c006f0073002d007400650073007400"
)
WINDOWS_BINDING_VECTOR = bytes.fromhex(
    "e1736841b7a49dc10506ef807f0fa6ca1409a13b7edd2d22b4a8c5890532e067"
)
VECTOR_NONCE = bytes(range(32))
VECTOR_PRINCIPAL = b"\x11" * 16
VECTOR_BINDING = b"\xa5" * 32


def test_domain_constants_mirror_rust() -> None:
    assert HANDSHAKE_MESSAGE_DOMAIN == b"llmos/principal-handshake/v1"
    assert CHANNEL_BINDING_DOMAIN == b"llmos/ipc-channel-binding/v1"
    assert HANDSHAKE_NONCE_BYTES == 32
    assert HANDSHAKE_SCHEMA_NAME == "nlos.sabi.PrincipalHandshake"
    assert (HANDSHAKE_SCHEMA_MAJOR, HANDSHAKE_SCHEMA_MINOR) == (1, 0)


def test_principal_handshake_message_golden_vector() -> None:
    assert (
        principal_handshake_message(VECTOR_NONCE, VECTOR_PRINCIPAL, VECTOR_BINDING)
        == DIGEST_VECTOR
    )


def test_endpoint_channel_binding_unix_vector() -> None:
    assert (
        endpoint_channel_binding("/tmp/nlos-test.sock", platform="darwin")
        == UNIX_BINDING_VECTOR
    )


def test_endpoint_path_bytes_unix_is_raw_os_bytes() -> None:
    assert endpoint_path_bytes("/tmp/nlos-test.sock", platform="darwin") == (
        b"/tmp/nlos-test.sock"
    )


def test_endpoint_path_bytes_windows_is_utf16_native() -> None:
    """Windows 管道名字节 = UTF-16 码元按本机字节序(transport.rs:99-107)。

    未实机核验:本向量按镜像实现与独立预计算字节比对,未在实机 Windows
    服务端对拍(回执标注);镜像源为 Rust 同名函数的逐行翻译。
    """
    assert (
        endpoint_path_bytes(r"\\.\pipe\nlos-test", platform="win32")
        == WINDOWS_UTF16LE_VECTOR
    )


def test_endpoint_channel_binding_windows_vector() -> None:
    # 同上:未实机核验的参数化对拍(镜像 transport.rs 的 UTF-16LE 编码路径)
    assert (
        endpoint_channel_binding(r"\\.\pipe\nlos-test", platform="win32")
        == WINDOWS_BINDING_VECTOR
    )


def test_endpoint_binding_is_32_bytes_for_long_paths() -> None:
    long_unix = "/tmp/" + "x" * 500 + ".sock"
    assert len(endpoint_channel_binding(long_unix, platform="darwin")) == 32


def test_decode_challenge_accepts_valid_wire() -> None:
    challenge = PrincipalHandshakeChallenge()
    challenge.schema.name = HANDSHAKE_SCHEMA_NAME
    challenge.schema.major = HANDSHAKE_SCHEMA_MAJOR
    challenge.schema.minor = HANDSHAKE_SCHEMA_MINOR
    challenge.nonce = VECTOR_NONCE
    decoded = decode_challenge(challenge.SerializeToString(deterministic=True))
    assert decoded.nonce == VECTOR_NONCE


def test_decode_challenge_rejects_garbage() -> None:
    with pytest.raises(IpcError, match="malformed"):
        decode_challenge(b"\xff\xff\xff\xff\xff\xff\xff\xff")


def test_decode_challenge_rejects_wrong_nonce_length() -> None:
    challenge = PrincipalHandshakeChallenge()
    challenge.schema.name = HANDSHAKE_SCHEMA_NAME
    challenge.schema.major = HANDSHAKE_SCHEMA_MAJOR
    challenge.nonce = b"\x00" * 31
    with pytest.raises(IpcError, match="nonce"):
        decode_challenge(challenge.SerializeToString(deterministic=True))


def test_decode_challenge_rejects_wrong_schema() -> None:
    challenge = PrincipalHandshakeChallenge()
    challenge.schema.name = "nlos.sabi.Envelope"
    challenge.schema.major = 1
    challenge.nonce = VECTOR_NONCE
    with pytest.raises(IpcError, match="schema"):
        decode_challenge(challenge.SerializeToString(deterministic=True))


def test_build_attestation_roundtrip_and_schema_identity() -> None:
    signature = b"\x77" * 64
    wire = build_attestation(VECTOR_PRINCIPAL, VECTOR_NONCE, VECTOR_BINDING, signature)
    parsed = PrincipalHandshakeAttestation()
    parsed.ParseFromString(wire)
    assert parsed.principal_id == VECTOR_PRINCIPAL
    assert parsed.nonce == VECTOR_NONCE
    assert parsed.channel_binding == VECTOR_BINDING
    assert parsed.signature == signature
    assert parsed.schema.name == HANDSHAKE_SCHEMA_NAME
    assert parsed.schema.major == HANDSHAKE_SCHEMA_MAJOR
    assert parsed.schema.minor == HANDSHAKE_SCHEMA_MINOR
    assert not parsed.schema.critical_extension_ids


@pytest.mark.parametrize(
    ("field", "value"),
    [
        ("principal_id", b"\x11" * 15),
        ("principal_id", b"\x11" * 17),
        ("nonce", b"\x00" * 31),
        ("nonce", b"\x00" * 33),
        ("channel_binding", b""),
        ("channel_binding", b"\x01" * (MAX_HANDSHAKE_CHANNEL_BINDING_BYTES + 1)),
        ("signature", b"\x77" * 63),
        ("signature", b"\x77" * 65),
    ],
)
def test_build_attestation_rejects_malformed_lengths(
    field: str, value: bytes
) -> None:
    fields = {
        "principal_id": VECTOR_PRINCIPAL,
        "nonce": VECTOR_NONCE,
        "channel_binding": VECTOR_BINDING,
        "signature": b"\x77" * 64,
    }
    fields[field] = value
    with pytest.raises(IpcError):
        build_attestation(
            fields["principal_id"],
            fields["nonce"],
            fields["channel_binding"],
            fields["signature"],
        )


def test_default_platform_matches_sys_platform() -> None:
    # 缺省 platform 取 sys.platform:darwin/linux → unix 字节,win32 → UTF-16
    if sys.platform == "win32":
        expected = WINDOWS_UTF16LE_VECTOR
        endpoint = r"\\.\pipe\nlos-test"
    else:
        expected = b"/tmp/nlos-test.sock"
        endpoint = "/tmp/nlos-test.sock"
    assert endpoint_path_bytes(endpoint) == expected
