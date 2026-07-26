"""不变式：内容哈希去重（议题 21：id = 内容哈希，字面去重）。"""

from __future__ import annotations

import hashlib

from sematom import AssertionLevel, AtomKind, AtomStore
from sematom.model import compute_atom_id


def test_same_content_same_id_no_new_row(store: AtomStore):
    first = store.write_claim("同一条断言", source="agent-a",
                              assertion=AssertionLevel.INFERENCE)
    rows_before = store.row_count()
    second = store.write_claim("同一条断言", source="agent-b",  # 不同生产者、同内容
                               assertion=AssertionLevel.FACT_FROM_TOOL)
    assert first == second
    assert store.row_count() == rows_before  # 不产生新行


def test_different_content_different_id(store: AtomStore):
    a = store.write_claim("断言甲", source="s", assertion=AssertionLevel.INFERENCE)
    b = store.write_claim("断言乙", source="s", assertion=AssertionLevel.INFERENCE)
    assert a != b


def test_id_is_sha256_content_hash(store: AtomStore):
    content = "哈希完整性校验"
    atom_id = store.write_claim(content, source="s", assertion=AssertionLevel.INFERENCE)
    expected = compute_atom_id(AtomKind.CLAIM, content)
    assert atom_id == expected
    assert len(atom_id) == len(hashlib.sha256(b"x").hexdigest())


def test_chain_hash_deterministic_and_parent_dependent(store: AtomStore):
    parent = store.write_claim("父断言", source="s", assertion=AssertionLevel.FACT_FROM_TOOL)
    child = store.write_claim("子断言", source="s", assertion=AssertionLevel.INFERENCE,
                              lineage=(parent,))
    parent_view = store.get(parent)
    child_view = store.get(child)
    assert parent_view is not None and child_view is not None
    assert child_view.provenance.chain_hash != parent_view.provenance.chain_hash
    # 同构重放：同内容同父，chain_hash 可复现
    child_replay = store.write_claim("子断言", source="s2", assertion=AssertionLevel.INFERENCE,
                                     lineage=(parent,))
    assert child_replay == child  # 去重后 chain_hash 一致（同一行）


def test_whitespace_difference_is_different_atom(store: AtomStore):
    """字面去重的边界：哈希只覆盖字面相同，语义等价归 link atom（议题 22）。"""
    a = store.write_claim("断言 有空格", source="s", assertion=AssertionLevel.INFERENCE)
    b = store.write_claim("断言有空格", source="s", assertion=AssertionLevel.INFERENCE)
    assert a != b
