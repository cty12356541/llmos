"""不变式：血缘图遍历（接触追踪/召回的存储层支撑，议题 9/21）。"""

from __future__ import annotations

import pytest

from sematom import (
    AssertionLevel,
    AtomStore,
    UnknownAtom,
    VerificationStatus,
)

S = AssertionLevel.INFERENCE


def test_lineage_parent_must_exist(store: AtomStore):
    with pytest.raises(UnknownAtom):
        store.write_claim("孤儿断言", source="s", assertion=S, lineage=("sha256:ghost",))


def test_forward_traversal_bfs(store: AtomStore):
    root = store.write_claim("根事实", source="tool", assertion=AssertionLevel.FACT_FROM_TOOL)
    mid = store.write_claim("中间推理", source="s", assertion=S, lineage=(root,))
    leaf1 = store.write_claim("叶一", source="s", assertion=S, lineage=(mid,))
    leaf2 = store.write_claim("叶二", source="s", assertion=S, lineage=(mid,))
    grand = store.write_claim("孙代", source="s", assertion=S, lineage=(leaf1, leaf2))
    desc = store.descendants(root)
    assert set(desc) == {mid, leaf1, leaf2, grand}
    assert desc.index(mid) < desc.index(grand)  # BFS：祖先先于后代


def test_diamond_lineage_visited_once(store: AtomStore):
    a = store.write_claim("a", source="s", assertion=S)
    b = store.write_claim("b", source="s", assertion=S, lineage=(a,))
    c = store.write_claim("c", source="s", assertion=S, lineage=(a,))
    d = store.write_claim("d", source="s", assertion=S, lineage=(b, c))
    assert store.descendants(a).count(d) == 1


def test_recall_marks_descendants_disputed(store: AtomStore):
    """召回 = 沿 lineage 标记后代 disputed（议题 21 3.1：接触追踪的数据结构）。"""
    bad = store.write_claim("幻觉原子", source="s", assertion=S)
    derived = store.write_claim("基于幻觉的推理", source="s", assertion=S, lineage=(bad,))
    clean = store.write_claim("无关原子", source="s", assertion=S)
    hit = store.recall(bad, by="supervisor")
    assert hit == [derived]
    assert store.verification_of(derived).status is VerificationStatus.DISPUTED
    assert store.verification_of(clean).status is VerificationStatus.UNVERIFIED


def test_recall_skips_already_retracted(store: AtomStore):
    bad = store.write_claim("污染源", source="s", assertion=S)
    derived = store.write_claim("后代", source="s", assertion=S, lineage=(bad,))
    store.retract(derived, source="s")
    assert store.recall(bad, by="supervisor") == []
