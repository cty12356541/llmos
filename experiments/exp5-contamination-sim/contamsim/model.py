"""Domain model for the exp5 contamination-propagation simulation.

Mirrors the llmos issue decisions:
- Issue 21 (semantic atoms): NL content + structured envelope, kernel never
  parses content; atoms immutable, belief state is a fold over the atom stream
  (so verification status lives in the World fold, not on the atom).
- Issue 9 (provenance): source / lineage / assertion level / verification
  status are the four mandatory envelope fields.
- Issue 22 (link atoms): EQUIVALENT/CONTRADICTS/ENTAILS/SUPPORTS/REFINES as a
  closed enum; ENTAILS/SUPPORTS amplify contamination radius.
- Issue 26 revision 4: verified status is written only by an independent
  verifier, never by the producer.
"""

from dataclasses import dataclass
from enum import StrEnum
from typing import NewType

AtomId = NewType("AtomId", int)
AgentId = NewType("AgentId", int)


class AssertionLevel(StrEnum):
    """Assertion level declared by the producer (issue 9/21)."""

    FACT_FROM_TOOL = "fact_from_tool"
    INFERENCE = "inference"
    SPECULATION = "speculation"
    DIRECTIVE = "directive"


class VerificationStatus(StrEnum):
    """Verification state machine values (issue 21)."""

    UNVERIFIED = "unverified"
    VERIFIED = "verified"
    DISPUTED = "disputed"


class LinkRelation(StrEnum):
    """Closed five-relation enum for link atoms (issue 22)."""

    EQUIVALENT = "equivalent"
    CONTRADICTS = "contradicts"
    ENTAILS = "entails"
    SUPPORTS = "supports"
    REFINES = "refines"


@dataclass(frozen=True, slots=True)
class Provenance:
    """Kernel-mandatory provenance metadata (issue 9 Q3)."""

    source: AgentId
    lineage: tuple[AtomId, ...]


@dataclass(frozen=True, slots=True)
class SemanticAtom:
    """Immutable atom envelope. Content is opaque to the kernel."""

    id: AtomId
    content: str
    provenance: Provenance
    assertion: AssertionLevel
    created_at: int


@dataclass(frozen=True, slots=True)
class LinkAtom:
    """A semantic relation assertion between two atoms (issue 22)."""

    id: AtomId
    relation: LinkRelation
    endpoints: tuple[AtomId, AtomId]
    judged_by: AgentId
    created_at: int


class Defense(StrEnum):
    """The four experimental arms: which countermeasures are active."""

    NONE = "none"  # no gating, no tracing
    GATING = "gating"  # only verified atoms enter the shared space
    TRACING = "tracing"  # only contact tracing + recall
    COMBINED = "combined"  # both

    @property
    def gating(self) -> bool:
        return self in (Defense.GATING, Defense.COMBINED)

    @property
    def tracing(self) -> bool:
        return self in (Defense.TRACING, Defense.COMBINED)


@dataclass(frozen=True, slots=True)
class SimConfig:
    """Simulation parameters. error_rate is the llm-judge misjudgment rate."""

    defense: Defense
    error_rate: float
    seed: int
    steps: int = 200
    n_agents: int = 12
    p_produce: float = 0.6
    max_parents: int = 3
    p_link: float = 0.15
    verifier_throughput: int = 10
    audit_pool: int = 100
    recall_delay: int = 2
    n_initial_clean: int = 20
    n_injected: int = 3
