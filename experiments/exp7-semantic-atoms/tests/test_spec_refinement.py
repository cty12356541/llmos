"""不变式：规格即原子 + REFINES 精化链（议题 23 Q3：规格版本链 = 需求演化链）。"""

from __future__ import annotations

from sematom import (
    AtomKind,
    AtomStore,
    Constraints,
    Criticality,
    DeterministicCriterion,
    IntentSpec,
    JudgmentMethod,
    LinkRelation,
    read_spec,
    refinement_chain,
    write_link,
    write_spec,
)


def _spec(goal: str) -> IntentSpec:
    return IntentSpec(goal=goal,
                      acceptance=(DeterministicCriterion(checker="tool://pytest"),),
                      constraints=Constraints(budget_cap=100),
                      criticality=Criticality.STANDARD, issuer="orchestrator")


def test_spec_stored_as_atom(store: AtomStore):
    spec_id = write_spec(store, _spec("v1 目标"))
    view = store.get(spec_id)
    assert view is not None
    assert view.kind is AtomKind.SPEC
    assert view.content == "v1 目标"  # goal 作为不透明 content
    assert view.provenance.source == "orchestrator"  # 签发者入 provenance


def test_refinement_chain_via_refines_links(store: AtomStore):
    v1 = write_spec(store, _spec("规格 v1"))
    v2 = write_spec(store, _spec("规格 v2"), lineage=(v1,))
    v3 = write_spec(store, _spec("规格 v3"), lineage=(v2,))
    write_link(store, LinkRelation.REFINES, v2, v1,
               by="architect", method=JudgmentMethod.HUMAN, source="orchestrator")
    write_link(store, LinkRelation.REFINES, v3, v2,
               by="architect", method=JudgmentMethod.HUMAN, source="orchestrator")
    assert refinement_chain(store, v1) == [v1, v2, v3]
    assert refinement_chain(store, v2) == [v2, v3]
    assert refinement_chain(store, v3) == [v3]


def test_retracted_link_breaks_chain(store: AtomStore):
    """错误的精化判断用墓碑撤回（议题 22：与撤回错误断言同一套机制）。"""
    v1 = write_spec(store, _spec("v1"))
    v2 = write_spec(store, _spec("v2"))
    link_id = write_link(store, "REFINES", v2, v1,
                         by="architect", method="human", source="orchestrator")
    assert refinement_chain(store, v1) == [v1, v2]
    store.retract(link_id, source="orchestrator", reason="精化关系判错")
    assert refinement_chain(store, v1) == [v1]


def test_spec_retractable_and_disputable(store: AtomStore):
    """垃圾规格可被发现和标记（议题 23 陷阱检查：verified garbage 至少可争议）。"""
    spec_id = write_spec(store, _spec("垃圾规格"))
    store.retract(spec_id, source="reviewer", reason="需求写错")
    assert read_spec(store, spec_id) is None
