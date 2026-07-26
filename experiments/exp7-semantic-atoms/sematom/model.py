"""语义原子信封类型与冻结枚举（议题 21/22/23 定案字段 + 议题 24 宪法）。

宪法边界（议题 24）：形式化认识论，绝不形式化语义。
本模块只允许过程/溯源元数据（谁说的/怎么产生/怎么验证/谁判的关系/何时失效），
content 保持不透明自然语言——本模块对它做的唯一运算是哈希。
"""

from __future__ import annotations

import hashlib
from collections.abc import Iterable
from dataclasses import dataclass
from enum import StrEnum
from typing import NewType

from .errors import ClosedSetViolation

AtomId = NewType("AtomId", str)


class AtomKind(StrEnum):
    CLAIM = "claim"
    TOMBSTONE = "tombstone"
    LINK = "link"
    SPEC = "spec"


class AssertionLevel(StrEnum):
    """议题 21：生产者对自己产出过程的自声明（闭集四值）。"""

    FACT_FROM_TOOL = "FACT_FROM_TOOL"
    INFERENCE = "INFERENCE"
    SPECULATION = "SPECULATION"
    DIRECTIVE = "DIRECTIVE"


class VerificationStatus(StrEnum):
    UNVERIFIED = "unverified"
    VERIFIED = "verified"
    DISPUTED = "disputed"


class VerificationMethod(StrEnum):
    """议题 23 验证独立性四档。"""

    SELF_ATTESTED = "self-attested"
    INDEPENDENT_VERIFIER = "independent-verifier"
    DETERMINISTIC = "deterministic"
    HUMAN = "human"


#: 议题 26 修订 4：verified 只能由独立验证者或确定性 checker 写入；自检不产生 verified。
VERIFIED_WRITERS: frozenset[VerificationMethod] = frozenset(
    {VerificationMethod.INDEPENDENT_VERIFIER, VerificationMethod.DETERMINISTIC}
)


class LinkRelation(StrEnum):
    """议题 22 Q3：闭集五关系（冻结，新增须过四重测试）。"""

    EQUIVALENT = "EQUIVALENT"
    CONTRADICTS = "CONTRADICTS"
    ENTAILS = "ENTAILS"
    SUPPORTS = "SUPPORTS"
    REFINES = "REFINES"


class JudgmentMethod(StrEnum):
    """link atom 判断方法（议题 22：embedding|llm|human）。"""

    EMBEDDING = "embedding"
    LLM = "llm"
    HUMAN = "human"


class Criticality(StrEnum):
    """议题 9 可靠性档位声明（规格内）。"""

    LOW = "low"
    STANDARD = "standard"
    HIGH = "high"
    CRITICAL = "critical"


def parse_enum[E: StrEnum](enum_cls: type[E], raw: str) -> E:
    """边界解析：冻结枚举的集外值在写入边界被拒绝（议题 24 的运行时体现）。"""
    try:
        return enum_cls(raw)
    except ValueError as exc:
        raise ClosedSetViolation(f"{enum_cls.__name__} 冻结枚举拒绝集外值: {raw!r}") from exc


def compute_atom_id(
    kind: AtomKind,
    content: str,
    *,
    target: AtomId | None = None,
    endpoints: tuple[AtomId, AtomId] | None = None,
    relation: str | None = None,
) -> AtomId:
    """议题 21：id = 内容哈希（sha256）。字面去重在此层；语义等价归 link atom（议题 22）。

    link 的 id 含 relation：同一对端点上的矛盾判断是不同原子，共存不裁决（修订 6）。
    """
    h = hashlib.sha256()
    h.update(kind.value.encode())
    h.update(b"\x00")
    h.update(content.encode("utf-8"))  # 哈希是 content 在本层被允许的唯一运算
    h.update(b"\x00")
    if target is not None:
        h.update(target.encode())
    if endpoints is not None:
        h.update(endpoints[0].encode())
        h.update(b"\x00")
        h.update(endpoints[1].encode())
    if relation is not None:
        h.update(relation.encode())
    return AtomId(h.hexdigest())


def compute_chain_hash(atom_id: AtomId, parent_chain_hashes: Iterable[str]) -> str:
    """血缘链完整性：chain_hash = H(排序后的父 chain_hash 序列 + 自身 id)。"""
    h = hashlib.sha256()
    for parent_hash in sorted(parent_chain_hashes):
        h.update(parent_hash.encode())
        h.update(b"\x00")
    h.update(atom_id.encode())
    return h.hexdigest()


@dataclass(frozen=True, slots=True)
class Provenance:
    source: str
    lineage: tuple[AtomId, ...] = ()
    chain_hash: str = ""


@dataclass(frozen=True, slots=True)
class Verification:
    status: VerificationStatus = VerificationStatus.UNVERIFIED
    by: str = ""
    method: VerificationMethod | None = None
    timestamp: float = 0.0


@dataclass(frozen=True, slots=True)
class Validity:
    created: float
    ttl: float | None = None


@dataclass(frozen=True, slots=True)
class Judgment:
    by: str
    method: JudgmentMethod


@dataclass(frozen=True, slots=True)
class AtomView:
    """原子的读出视图（含 verdict 事件折叠后的有效验证状态）。"""

    id: AtomId
    kind: AtomKind
    content: str
    provenance: Provenance
    assertion: AssertionLevel | None
    validity: Validity
    verification: Verification
    signature: str | None = None
    target: AtomId | None = None
    relation: LinkRelation | None = None
    endpoints: tuple[AtomId, AtomId] | None = None
    judgment: Judgment | None = None
