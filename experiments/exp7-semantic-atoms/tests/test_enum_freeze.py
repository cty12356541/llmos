"""不变式：枚举冻结（议题 24 宪法 Q2）——闭集枚举的集外值在写入边界被拒绝。"""

from __future__ import annotations

import pytest

from sematom import (
    AssertionLevel,
    AtomStore,
    ClosedSetViolation,
    Constraints,
    Criticality,
    DeterministicCriterion,
    IntentSpec,
    LinkRelation,
    write_link,
    write_spec,
)
from sematom.model import parse_enum
from sematom.spec import criterion_parse


def test_relation_enum_has_exactly_five_values():
    assert {r.value for r in LinkRelation} == {
        "EQUIVALENT", "CONTRADICTS", "ENTAILS", "SUPPORTS", "REFINES",
    }


def test_relation_outside_closed_set_rejected(store: AtomStore):
    a = store.write_claim("甲", source="s", assertion=AssertionLevel.INFERENCE)
    b = store.write_claim("乙", source="s", assertion=AssertionLevel.INFERENCE)
    with pytest.raises(ClosedSetViolation):
        # 议题 24 预警清单：PART_OF 是世界关系不是判断关系，直接破墙
        write_link(store, "PART_OF", a, b, by="m", method="llm", source="s")


def test_judgment_method_outside_closed_set_rejected(store: AtomStore):
    a = store.write_claim("甲", source="s", assertion=AssertionLevel.INFERENCE)
    b = store.write_claim("乙", source="s", assertion=AssertionLevel.INFERENCE)
    with pytest.raises(ClosedSetViolation):
        write_link(store, "EQUIVALENT", a, b, by="m", method="vibes", source="s")


def test_assertion_enum_frozen():
    assert {a.value for a in AssertionLevel} == {
        "FACT_FROM_TOOL", "INFERENCE", "SPECULATION", "DIRECTIVE",
    }
    with pytest.raises(ClosedSetViolation):
        parse_enum(AssertionLevel, "TRUTH")  # 内核不判真假（议题 24）


def test_acceptance_method_three_way_closed_set():
    with pytest.raises(ClosedSetViolation):
        criterion_parse({"method": "embedding-similarity", "criterion": "x"})
    with pytest.raises(ClosedSetViolation):
        criterion_parse({"criterion": "x"})  # 缺 method


def test_constraints_unknown_key_rejected():
    with pytest.raises(ClosedSetViolation):
        Constraints.parse({"budget_cap": 100, "topic": "physics"})  # 主题分类是本体（议题 24 禁止）


def test_spec_roundtrip_preserves_closed_fields(store: AtomStore):
    spec = IntentSpec(
        goal="把测试跑绿",
        acceptance=(DeterministicCriterion(checker="tool://pytest"),),
        constraints=Constraints(budget_cap=500, forbidden=("network",)),
        criticality=Criticality.HIGH,
        issuer="orchestrator",
    )
    spec_id = write_spec(store, spec)
    from sematom import read_spec

    loaded = read_spec(store, spec_id)
    assert loaded is not None
    assert loaded.criticality is Criticality.HIGH
    assert loaded.constraints.budget_cap == 500
    assert loaded.constraints.forbidden == ("network",)
