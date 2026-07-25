"""Metrics accumulation for the simulation: counters and periodic samples."""

from dataclasses import dataclass


@dataclass(frozen=True, slots=True)
class Sample:
    """Cumulative metrics snapshot at time t (windowed stats derive from diffs)."""

    t: float
    delivered_storm: int
    delivered_useful: int
    processed_storm: int
    processed_useful: int
    inbox_fill_mean: float
    blocked_normals: int


class Metrics:
    """Mutable accumulator — its sole purpose is to be updated by the engine."""

    def __init__(self) -> None:
        self.delivered_storm = 0
        self.delivered_useful = 0
        self.evicted_storm = 0
        self.evicted_useful = 0
        self.processed_storm = 0
        self.processed_useful = 0
        self.credits_charged = 0
        self.samples: list[Sample] = []
