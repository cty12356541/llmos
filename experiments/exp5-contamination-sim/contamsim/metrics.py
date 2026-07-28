"""Measurement for the exp5 outbreak simulation.

Contamination ground truth is invisible to the defense mechanisms; metrics
are the only place it is read.
"""

from dataclasses import dataclass, field


@dataclass(frozen=True, slots=True)
class Sample:
    """Per-step snapshot of the shared space."""

    t: int
    active: int  # shared atoms not disputed (circulating)
    contaminated_active: int  # circulating AND contaminated (ground truth)
    pending: int  # waiting at the gate
    ratio: float  # contaminated_active / active


@dataclass(frozen=True, slots=True)
class RecallEvent:
    """One descendant marked disputed by a recall wave."""

    detected_at: int  # when the ancestor was judged disputed
    recalled_at: int  # when this descendant was marked
    depth: int  # lineage hops from the detected ancestor


@dataclass(slots=True)
class Metrics:
    """Accumulator mutated by the World during the run."""

    samples: list[Sample] = field(default_factory=list)
    recalls: list[RecallEvent] = field(default_factory=list)
    detections: int = 0  # contaminated atoms correctly judged disputed
    collateral: int = 0  # clean atoms falsely judged disputed


@dataclass(frozen=True, slots=True)
class Summary:
    """One number per question the experiment asks."""

    final_ratio: float
    tail_mean_ratio: float  # mean ratio over the last 20% of steps
    max_ratio: float
    converge_t: int | None  # first t after which ratio stays <= threshold
    recall_coverage: float  # disputed contaminated / all contaminated
    collateral_rate: float  # disputed clean / all clean
