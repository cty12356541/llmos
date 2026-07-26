"""IntentSpec：意图规格——语义计算的类型系统（议题 23 定案 + 议题 26 修订 3/4）。

- 三轨异构验收：deterministic（代码，hard gate）/ llm-judge（模型，soft gate）/ human（soft gate）。
- 修订 3：escrow 释放默认只绑 hard gate；soft gate 结果影响验收评价与信誉（用户态）。
- 修订 4：验收通过要写 verified 时，deterministic 轨用 method=deterministic、
  llm-judge 轨必须以独立验证者身份（method=independent-verifier）经 record_verdict 写入——
  存储层强制，self-attested 不产生 verified。
- 规格即原子（议题 23 Q3）：goal 作为不透明 content 存储，结构字段（acceptance/constraints/
  criticality）入 specs 表；精化链用 REFINES link（links.refinement_chain）。
- 本实验不接真实 LLM：llm-judge 轨用 MockJudge（可配置通过率/误判率，种子可复现）。
"""

from __future__ import annotations

import json
import random
from collections.abc import Callable, Mapping
from dataclasses import dataclass
from typing import Literal, assert_never

from .errors import ClosedSetViolation, UnknownAtom
from .model import AtomId, AtomKind, Criticality, parse_enum
from .store import AtomStore

_SPEC_SCHEMA = """
CREATE TABLE IF NOT EXISTS specs (
  spec_id     TEXT PRIMARY KEY,
  acceptance  TEXT NOT NULL,
  constraints TEXT NOT NULL,
  criticality TEXT NOT NULL
);
"""

# ---- 验收标准：三轨判别联合（议题 23 Q1）----


@dataclass(frozen=True, slots=True)
class DeterministicCriterion:
    """确定性轨：checker 是 tool:// 风格的注册表引用，执行器解析为可调用检查函数。"""

    checker: str
    description: str = ""
    method: Literal["deterministic"] = "deterministic"


@dataclass(frozen=True, slots=True)
class LlmJudgeCriterion:
    """llm-judge 轨：NL 判据由模型判定（本实验为 MockJudge）。"""

    criterion: str
    method: Literal["llm-judge"] = "llm-judge"


@dataclass(frozen=True, slots=True)
class HumanCriterion:
    """human 轨：最高风险走人工；执行器返回 pending（passed=None）。"""

    criterion: str
    method: Literal["human"] = "human"


AcceptanceCriterion = DeterministicCriterion | LlmJudgeCriterion | HumanCriterion

_CONSTRAINT_KEYS = frozenset({"budget_cap", "deadline", "forbidden"})


@dataclass(frozen=True, slots=True)
class Constraints:
    """全结构字段，内核可强制（议题 23：与 Policy/Budget 咬合，不涉及语义）。"""

    budget_cap: int | None = None
    deadline: float | None = None
    forbidden: tuple[str, ...] = ()

    def to_dict(self) -> dict[str, object]:
        return {"budget_cap": self.budget_cap, "deadline": self.deadline,
                "forbidden": list(self.forbidden)}

    @classmethod
    def parse(cls, raw: Mapping[str, object]) -> Constraints:
        unknown = set(raw) - _CONSTRAINT_KEYS
        if unknown:
            raise ClosedSetViolation(f"constraints 闭集键拒绝: {sorted(unknown)}")
        return cls(
            budget_cap=raw.get("budget_cap"),  # type: ignore[arg-type]
            deadline=raw.get("deadline"),  # type: ignore[arg-type]
            forbidden=tuple(raw.get("forbidden", ())),  # type: ignore[arg-type]
        )


@dataclass(frozen=True, slots=True)
class IntentSpec:
    goal: str
    acceptance: tuple[AcceptanceCriterion, ...]
    constraints: Constraints
    criticality: Criticality
    issuer: str
    id: AtomId | None = None


def criterion_to_dict(c: AcceptanceCriterion) -> dict[str, str]:
    match c:
        case DeterministicCriterion(checker=name, description=desc):
            return {"method": "deterministic", "checker": name, "criterion": desc}
        case LlmJudgeCriterion(criterion=text):
            return {"method": "llm-judge", "criterion": text}
        case HumanCriterion(criterion=text):
            return {"method": "human", "criterion": text}
        case _ as unreachable:
            assert_never(unreachable)


def criterion_parse(raw: Mapping[str, str]) -> AcceptanceCriterion:
    """边界解析：method 三选一闭集，集外拒绝（议题 23 陷阱检查：内核只有 method 枚举）。"""
    match raw.get("method"):
        case "deterministic":
            return DeterministicCriterion(checker=raw["checker"],
                                          description=raw.get("criterion", ""))
        case "llm-judge":
            return LlmJudgeCriterion(criterion=raw["criterion"])
        case "human":
            return HumanCriterion(criterion=raw["criterion"])
        case other:
            raise ClosedSetViolation(f"acceptance method 闭集拒绝: {other!r}")


# ---- 规格即原子：存取 ----


def write_spec(store: AtomStore, spec: IntentSpec, *, lineage: tuple[AtomId, ...] = ()) -> AtomId:
    """规格写入信念流：goal 是不透明 content（内核永不解析），结构字段入 specs 表。"""
    store._db.execute(_SPEC_SCHEMA)
    spec_id = store._insert_spec_atom(spec.goal, source=spec.issuer, lineage=lineage)
    store._db.execute(
        "INSERT INTO specs (spec_id, acceptance, constraints, criticality) VALUES (?,?,?,?)",
        (
            spec_id,
            json.dumps([criterion_to_dict(c) for c in spec.acceptance]),
            json.dumps(spec.constraints.to_dict()),
            spec.criticality.value,
        ),
    )
    store._db.commit()
    return spec_id


def read_spec(store: AtomStore, spec_id: AtomId) -> IntentSpec | None:
    view = store.get(spec_id)
    if view is None or view.kind is not AtomKind.SPEC:
        return None
    row = store._db.execute(
        "SELECT acceptance, constraints, criticality FROM specs WHERE spec_id = ?", (spec_id,)
    ).fetchone()
    if row is None:
        raise UnknownAtom(f"spec 结构行缺失: {spec_id}")
    return IntentSpec(
        goal=view.content,
        acceptance=tuple(criterion_parse(c) for c in json.loads(row["acceptance"])),
        constraints=Constraints.parse(json.loads(row["constraints"])),
        criticality=parse_enum(Criticality, row["criticality"]),
        issuer=view.provenance.source,
        id=spec_id,
    )


# ---- 三轨验收执行 ----


class MockJudge:
    """llm-judge 轨 mock 判定器：可配置通过率/误判率，种子可复现（不接真实 LLM）。

    - should_pass=None：无ground truth，按 pass_rate 判定；
    - should_pass 给定：对真阳性按 false_negative_rate 误判，对真阴性按 false_positive_rate 误判。
    """

    def __init__(self, *, pass_rate: float = 1.0, false_positive_rate: float = 0.0,
                 false_negative_rate: float = 0.0, seed: int = 0):
        if not 0.0 <= pass_rate <= 1.0:
            raise ValueError("pass_rate 必须在 [0,1]")
        self._pass_rate = pass_rate
        self._fp = false_positive_rate
        self._fn = false_negative_rate
        self._rng = random.Random(seed)

    def judge(self, criterion: str, artifact: str, *, should_pass: bool | None = None) -> bool:
        _ = (criterion, artifact)  # mock 不读语义，只按概率判定
        if should_pass is None:
            return self._rng.random() < self._pass_rate
        if should_pass:
            return self._rng.random() >= self._fn
        return self._rng.random() < self._fp


@dataclass(frozen=True, slots=True)
class CriterionOutcome:
    criterion: AcceptanceCriterion
    passed: bool | None  # None = human 轨待决
    detail: str = ""


@dataclass(frozen=True, slots=True)
class AcceptanceReport:
    outcomes: tuple[CriterionOutcome, ...]

    @property
    def hard_gate_passed(self) -> bool:
        """hard gate = 全部 deterministic 轨通过（修订 3）。"""
        return all(
            o.passed for o in self.outcomes if isinstance(o.criterion, DeterministicCriterion)
        )

    @property
    def escrow_releasable(self) -> bool:
        """修订 3：escrow 释放默认只绑 hard gate，soft gate 不做金钱结算硬条件。"""
        return self.hard_gate_passed

    @property
    def soft_outcomes(self) -> tuple[CriterionOutcome, ...]:
        """soft gate 结果影响验收评价与信誉（用户态），不进 escrow。"""
        return tuple(
            o for o in self.outcomes if not isinstance(o.criterion, DeterministicCriterion)
        )


def run_acceptance(
    spec: IntentSpec,
    *,
    checkers: Mapping[str, Callable[[str], bool]],
    judge: MockJudge,
    artifact: str,
) -> AcceptanceReport:
    """三轨验收执行器：deterministic 跑注册表检查函数；llm-judge 走 mock；human 返回 pending。"""
    outcomes: list[CriterionOutcome] = []
    for criterion in spec.acceptance:
        match criterion:
            case DeterministicCriterion(checker=name):
                fn = checkers.get(name)
                if fn is None:
                    raise ClosedSetViolation(f"未注册的 deterministic checker: {name!r}")
                outcomes.append(CriterionOutcome(criterion, bool(fn(artifact)),
                                                 f"checker {name}"))
            case LlmJudgeCriterion(criterion=text):
                outcomes.append(CriterionOutcome(criterion, judge.judge(text, artifact),
                                                 "mock llm-judge"))
            case HumanCriterion(criterion=text):
                outcomes.append(CriterionOutcome(criterion, None, f"awaiting human: {text}"))
            case _ as unreachable:
                assert_never(unreachable)
    return AcceptanceReport(tuple(outcomes))
