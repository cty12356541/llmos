"""Ed25519 薄签名层——SDK 唯一的非 protobuf 依赖隔离点。

底层选 PyNaCl(libsodium 绑定;对比与推荐见
``docs/plans/2026-11-08-w82d-sdk-prework.md``)。**选型待 maintainer 签认**:
若换库(如 cryptography),只改本文件,签名输出必须是标准 Ed25519
(RFC 8032,64 字节),与 Rust 侧 ed25519-dalek ``verify_strict`` 兼容。
"""

from __future__ import annotations

from typing import Any

ED25519_SEED_BYTES = 32
ED25519_SIGNATURE_BYTES = 64


class SigningUnavailableError(RuntimeError):
    """PyNaCl 不可用;Ed25519 会话需要 ``pip install pynacl``。"""


def _load_pynacl() -> Any:
    try:
        import nacl.signing
    except ImportError as error:  # pragma: no cover - 环境性分支
        raise SigningUnavailableError(
            "nlos-sdk 的 Ed25519 挑战应答需要 PyNaCl: pip install pynacl"
        ) from error
    return nacl.signing


class Ed25519Signer:
    """持有 32 字节种子,产出标准 Ed25519 签名。"""

    def __init__(self, seed: bytes) -> None:
        seed_bytes = bytes(seed)
        if len(seed_bytes) != ED25519_SEED_BYTES:
            raise ValueError(
                f"Ed25519 seed 必须恰好 {ED25519_SEED_BYTES} 字节,"
                f"实得 {len(seed_bytes)}"
            )
        signing = _load_pynacl()
        self._key = signing.SigningKey(seed_bytes)

    @property
    def public_key(self) -> bytes:
        """32 字节验证公钥。"""

        return bytes(self._key.verify_key)

    def sign(self, message: bytes) -> bytes:
        """对任意字节串签名,返回 64 字节标准 Ed25519 签名。"""

        return bytes(self._key.sign(bytes(message)).signature)


__all__ = [
    "ED25519_SEED_BYTES",
    "ED25519_SIGNATURE_BYTES",
    "Ed25519Signer",
    "SigningUnavailableError",
]
