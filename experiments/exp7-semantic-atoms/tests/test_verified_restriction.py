"""不变式：verified 写入限制（议题 26 修订 4）+ verdict 事件折叠（议题 24 Q3）。"""

from __future__ import annotations

import pytest

from sematom import (
    AssertionLevel,
    AtomStore,
    ForbiddenVerifiedWrite,
    VerificationMethod,
    VerificationStatus,
)


def _claim(store: AtomStore) -> object:
    return store.write_claim("待验证断言", source="producer",
                             assertion=AssertionLevel.INFERENCE)


def test_deterministic_can_write_verified(store: AtomStore):
    atom_id = _claim(store)
    store.record_verdict(atom_id, VerificationStatus.VERIFIED,
                         by="tool://pytest", method=VerificationMethod.DETERMINISTIC)
    assert store.verification_of(atom_id).status is VerificationStatus.VERIFIED


def test_independent_verifier_can_write_verified(store: AtomStore):
    atom_id = _claim(store)
    store.record_verdict(atom_id, VerificationStatus.VERIFIED,
                         by="verifier-agent-b", method=VerificationMethod.INDEPENDENT_VERIFIER)
    assert store.verification_of(atom_id).status is VerificationStatus.VERIFIED


def test_self_attested_cannot_write_verified(store: AtomStore):
    """生产者自检的 verified 标记无效——防伪造 verified 击穿"只许 verified 进规划"闸门。"""
    atom_id = _claim(store)
    with pytest.raises(ForbiddenVerifiedWrite):
        store.record_verdict(atom_id, VerificationStatus.VERIFIED,
                             by="producer", method=VerificationMethod.SELF_ATTESTED)
    assert store.verification_of(atom_id).status is VerificationStatus.UNVERIFIED


def test_human_cannot_write_verified(store: AtomStore):
    """修订 4 字面：只有 deterministic / independent-verifier 两档能写 verified。"""
    atom_id = _claim(store)
    with pytest.raises(ForbiddenVerifiedWrite):
        store.record_verdict(atom_id, VerificationStatus.VERIFIED,
                             by="operator", method=VerificationMethod.HUMAN)


def test_self_attested_disputed_allowed(store: AtomStore):
    """限制只针对 verified；disputed/unverified 任何方法可写（分歧可见，议题 12）。"""
    atom_id = _claim(store)
    store.record_verdict(atom_id, VerificationStatus.DISPUTED,
                         by="producer", method=VerificationMethod.SELF_ATTESTED)
    assert store.verification_of(atom_id).status is VerificationStatus.DISPUTED


def test_verification_is_event_fold_not_mutation(store: AtomStore, clock):
    """状态机迁移不改写原子：unverified → verified → disputed 是 verdict 事件流的 fold。"""
    atom_id = _claim(store)
    assert store.verification_of(atom_id).status is VerificationStatus.UNVERIFIED
    store.record_verdict(atom_id, VerificationStatus.VERIFIED,
                         by="v1", method=VerificationMethod.DETERMINISTIC)
    clock.advance(1.0)
    store.record_verdict(atom_id, VerificationStatus.DISPUTED,
                         by="v2", method=VerificationMethod.INDEPENDENT_VERIFIER)
    ver = store.verification_of(atom_id)
    assert ver.status is VerificationStatus.DISPUTED  # 最新事件胜出
    assert ver.by == "v2"
    # 原子本体未动：内容仍原样可读
    view = store.get(atom_id)
    assert view is not None and view.content == "待验证断言"
