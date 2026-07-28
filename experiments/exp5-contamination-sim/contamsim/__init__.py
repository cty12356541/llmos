"""exp5-contamination-sim: provenance-graph outbreak simulator.

Validates the anti-hallucination-contagion mechanism of llmos issues 9/12
(provenance gating + contact tracing + recall) against an injected
hallucination outbreak. Pure graph simulation: no LLM, no network.
"""

from contamsim.model import (
    AgentId,
    AssertionLevel,
    AtomId,
    Defense,
    LinkRelation,
    SimConfig,
    VerificationStatus,
)

__all__ = [
    "AgentId",
    "AssertionLevel",
    "AtomId",
    "Defense",
    "LinkRelation",
    "SimConfig",
    "VerificationStatus",
]
