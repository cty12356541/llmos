"""薄签名层必须产出标准 Ed25519(RFC 8032 §7.1 TEST 1 KAT)。

薄层是 SDK 唯一 import PyNaCl 的位置(选型待 maintainer 签认,见
docs/plans/2026-11-08-w82d-sdk-prework.md);KAT 保证无论底层库选谁,
输出都与 ed25519-dalek verify_strict 兼容的标准签名。
"""

from __future__ import annotations

import pytest

from nlos_sdk._signing import Ed25519Signer

RFC8032_SEED = bytes.fromhex(
    "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60"
)
RFC8032_PUBLIC = bytes.fromhex(
    "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a"
)
RFC8032_SIG_EMPTY = bytes.fromhex(
    "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555"
    "fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b"
)


def test_rfc8032_test1_empty_message() -> None:
    signer = Ed25519Signer(RFC8032_SEED)
    assert signer.sign(b"") == RFC8032_SIG_EMPTY


def test_sign_is_deterministic_64_bytes() -> None:
    signer = Ed25519Signer(RFC8032_SEED)
    first = signer.sign(b"\x00" * 32)
    second = signer.sign(b"\x00" * 32)
    assert first == second
    assert len(first) == 64


def test_public_key_matches_reference() -> None:
    assert Ed25519Signer(RFC8032_SEED).public_key == RFC8032_PUBLIC


def test_seed_shape_is_enforced() -> None:
    with pytest.raises(ValueError):
        Ed25519Signer(b"short")
    with pytest.raises(ValueError):
        Ed25519Signer(b"x" * 33)
