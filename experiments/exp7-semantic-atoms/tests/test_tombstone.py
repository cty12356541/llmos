"""不变式：墓碑撤回（议题 21 Q2 + 议题 26 修订 6：tombstone 优先）。"""

from __future__ import annotations

import pytest

from sematom import (
    AssertionLevel,
    AtomKind,
    AtomStore,
    UnknownAtom,
    VerificationMethod,
    VerificationStatus,
)


def _claim(store: AtomStore, content: str = "待撤回断言") -> object:
    return store.write_claim(content, source="agent-a", assertion=AssertionLevel.SPECULATION)


def test_tombstone_hides_target(store: AtomStore):
    atom_id = _claim(store)
    assert store.get(atom_id) is not None
    store.retract(atom_id, source="agent-a", reason="事后发现来源不可靠")
    assert store.get(atom_id) is None  # tombstone 优先：撤回战胜内容
    assert store.is_retracted(atom_id)


def test_tombstone_is_itself_an_atom(store: AtomStore):
    atom_id = _claim(store)
    tomb_id = store.retract(atom_id, source="agent-a", reason="幻觉")
    tomb = store.get(tomb_id)
    assert tomb is not None
    assert tomb.kind is AtomKind.TOMBSTONE
    assert tomb.target == atom_id
    assert tomb.provenance.lineage == (atom_id,)  # 墓碑与目标的推导关系入血缘


def test_tombstone_survives_later_verdicts(store: AtomStore):
    """撤回后即使再写 verified verdict，查询仍然隐藏（tombstone 优先，防复活）。"""
    atom_id = _claim(store)
    store.retract(atom_id, source="agent-a")
    store.record_verdict(atom_id, VerificationStatus.VERIFIED,
                         by="verifier-x", method=VerificationMethod.DETERMINISTIC)
    assert store.get(atom_id) is None


def test_retract_unknown_target_rejected(store: AtomStore):
    with pytest.raises(UnknownAtom):
        store.retract("sha256:nonexistent", source="agent-a")


def test_expired_listing_excludes_retracted(store: AtomStore, clock):
    atom_id = store.write_claim("短命断言", source="a", assertion=AssertionLevel.INFERENCE,
                                ttl=10.0)
    store.retract(atom_id, source="a")
    clock.advance(20.0)
    assert store.expired() == []
