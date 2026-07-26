"""不变式：三轨验收（议题 23）+ hard/soft gate 分离（议题 26 修订 3）。"""

from __future__ import annotations

import pytest

from sematom import (
    AtomStore,
    ClosedSetViolation,
    Constraints,
    Criticality,
    DeterministicCriterion,
    HumanCriterion,
    IntentSpec,
    LlmJudgeCriterion,
    MockJudge,
    run_acceptance,
    write_spec,
)
from sematom.spec import read_spec


def _spec(acceptance) -> IntentSpec:
    return IntentSpec(goal="demo", acceptance=tuple(acceptance),
                      constraints=Constraints(), criticality=Criticality.STANDARD,
                      issuer="orchestrator")


def test_deterministic_track_runs_registered_checkers():
    spec = _spec([
        DeterministicCriterion(checker="tool://pytest"),
        DeterministicCriterion(checker="tool://file-exists"),
    ])
    report = run_acceptance(
        spec,
        checkers={"tool://pytest": lambda artifact: artifact.endswith(".py"),
                  "tool://file-exists": lambda artifact: len(artifact) > 0},
        judge=MockJudge(),
        artifact="solution.py",
    )
    assert report.hard_gate_passed
    assert report.escrow_releasable


def test_deterministic_failure_blocks_escrow_even_if_soft_passes():
    """修订 3：soft gate 通过不能替代 hard gate 失败。"""
    spec = _spec([
        DeterministicCriterion(checker="tool://pytest"),
        LlmJudgeCriterion(criterion="摘要忠实"),
    ])
    report = run_acceptance(spec, checkers={"tool://pytest": lambda _: False},
                            judge=MockJudge(pass_rate=1.0), artifact="x")
    assert not report.hard_gate_passed
    assert not report.escrow_releasable
    assert report.soft_outcomes[0].passed is True  # soft 结果仍记录，供信誉/评价


def test_soft_failure_does_not_block_escrow():
    """修订 3 另一面：escrow 默认只绑 hard gate；soft gate 失败不挡结算（显式声明除外）。"""
    spec = _spec([
        DeterministicCriterion(checker="tool://pytest"),
        LlmJudgeCriterion(criterion="文风优雅"),
    ])
    report = run_acceptance(spec, checkers={"tool://pytest": lambda _: True},
                            judge=MockJudge(pass_rate=0.0), artifact="x")
    assert report.escrow_releasable
    assert report.soft_outcomes[0].passed is False


def test_llm_judge_track_uses_configurable_mock():
    passing = MockJudge(pass_rate=1.0, seed=42)
    failing = MockJudge(pass_rate=0.0, seed=42)
    spec = _spec([LlmJudgeCriterion(criterion="判据")])
    r1 = run_acceptance(spec, checkers={}, judge=passing, artifact="a")
    r2 = run_acceptance(spec, checkers={}, judge=failing, artifact="a")
    assert r1.soft_outcomes[0].passed is True
    assert r2.soft_outcomes[0].passed is False


def test_mock_judge_error_rates():
    """误判率可配置：fp=1 时真阴性全被误判为通过；fn=1 时真阳性全被误判为失败。"""
    fp_judge = MockJudge(false_positive_rate=1.0, seed=1)
    fn_judge = MockJudge(false_negative_rate=1.0, seed=1)
    assert fp_judge.judge("c", "a", should_pass=False) is True   # 假阳性
    assert fn_judge.judge("c", "a", should_pass=True) is False   # 假阴性
    clean = MockJudge(false_positive_rate=0.0, false_negative_rate=0.0, seed=1)
    assert clean.judge("c", "a", should_pass=True) is True
    assert clean.judge("c", "a", should_pass=False) is False


def test_human_track_returns_pending():
    spec = _spec([HumanCriterion(criterion="最终权威复核")])
    report = run_acceptance(spec, checkers={}, judge=MockJudge(), artifact="a")
    assert report.outcomes[0].passed is None
    assert "awaiting human" in report.outcomes[0].detail


def test_unregistered_checker_rejected():
    spec = _spec([DeterministicCriterion(checker="tool://nonexistent")])
    with pytest.raises(ClosedSetViolation):
        run_acceptance(spec, checkers={}, judge=MockJudge(), artifact="a")


def test_spec_acceptance_roundtrip(store: AtomStore):
    """规格即原子：三轨 acceptance 序列化入 specs 表，读回逐项等价。"""
    spec = _spec([
        DeterministicCriterion(checker="tool://pytest", description="测试全绿"),
        LlmJudgeCriterion(criterion="摘要忠实于原文"),
        HumanCriterion(criterion="高风险变更人工复核"),
    ])
    spec_id = write_spec(store, spec)
    loaded = read_spec(store, spec_id)
    assert loaded is not None
    assert loaded.goal == "demo"
    assert loaded.acceptance == spec.acceptance
    assert loaded.issuer == "orchestrator"
