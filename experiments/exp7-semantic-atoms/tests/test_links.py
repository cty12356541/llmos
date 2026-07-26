"""LinkAtom 五关系行为（议题 22）：共存不裁决、CONTRADICTS 落地分歧可见、判断带 provenance。"""

from __future__ import annotations

import pytest

from sematom import (
    AssertionLevel,
    AtomStore,
    JudgmentMethod,
    LinkRelation,
    UnknownAtom,
    links_between,
    write_link,
)

S = AssertionLevel.INFERENCE


def _pair(store: AtomStore):
    a = store.write_claim("地球是平的", source="agent-x", assertion=AssertionLevel.SPECULATION)
    b = store.write_claim("地球是球体", source="agent-y", assertion=AssertionLevel.FACT_FROM_TOOL)
    return a, b


def test_all_five_relations_writable(store: AtomStore):
    a, b = _pair(store)
    for relation in LinkRelation:
        link_id = write_link(store, relation, a, b, by="judge-1",
                             method=JudgmentMethod.LLM, source="svc", content=f"{relation} 判断")
        view = store.get(link_id)
        assert view is not None and view.relation is relation
        assert view.endpoints == (a, b)
        assert view.judgment is not None and view.judgment.by == "judge-1"


def test_contradictory_judgments_coexist(store: AtomStore):
    """修订 6：link atom 不自动合并——矛盾判断共存，分歧显式化，裁决归用户态。"""
    a, b = _pair(store)
    write_link(store, LinkRelation.CONTRADICTS, a, b, by="judge-1",
               method=JudgmentMethod.LLM, source="svc-a")
    write_link(store, LinkRelation.EQUIVALENT, a, b, by="judge-2",
               method=JudgmentMethod.EMBEDDING, source="svc-b")
    views = links_between(store, a, b)
    assert {v.relation for v in views} == {LinkRelation.CONTRADICTS, LinkRelation.EQUIVALENT}
    # 两个判断的 provenance 都可查（谁判的、用什么方法）
    assert {v.judgment.method for v in views} == {JudgmentMethod.LLM, JudgmentMethod.EMBEDDING}


def test_link_endpoints_must_exist(store: AtomStore):
    a, _ = _pair(store)
    with pytest.raises(UnknownAtom):
        write_link(store, LinkRelation.SUPPORTS, a, "sha256:ghost",
                   by="j", method=JudgmentMethod.LLM, source="s")


def test_link_lineage_references_endpoints(store: AtomStore):
    """link atom 的 lineage 指向两端点：变换的输入输出关系可显式陈述（议题 22 六）。"""
    a, b = _pair(store)
    link_id = write_link(store, LinkRelation.ENTAILS, a, b,
                         by="j", method=JudgmentMethod.LLM, source="s")
    view = store.get(link_id)
    assert view is not None and set(view.provenance.lineage) == {a, b}
