"""Storm scenario per llmos issue 11 crack 2: one rogue agent floods the
broadcast topic at high rate while N normal agents exchange direct messages
at low rate. Question: does sender-prepaid make the storm self-limiting?"""

from dataclasses import dataclass
from typing import Final

from msgstorm.engine import Engine
from msgstorm.metrics import Sample
from msgstorm.model import Agent, AgentId, AgentRole, SimConfig

BROADCAST_TOPIC: Final = "broadcast"


@dataclass(frozen=True, slots=True)
class ScenarioParams:
    """Knobs of the storm world; defaults match the main experiment matrix."""

    config: SimConfig
    n_normals: int = 20
    normal_send_rate: float = 1.0  # msg/s, direct to a random peer
    normal_process_rate: float = 20.0  # msg/s drained from the context window
    normal_budget: int = 100_000  # comfortably more than normal traffic needs
    inbox_capacity: int = 100  # context-window analogue
    storm_rate: float = 50.0  # msg/s published to the broadcast topic
    storm_budget: int = 10_000
    storm_start_s: float = 10.0  # let normal traffic establish a baseline


@dataclass(frozen=True, slots=True)
class RunResult:
    """Final metrics of one seeded run."""

    seed: int
    delivered_useful: int
    delivered_storm: int
    evicted_useful: int
    evicted_storm: int
    processed_useful: int
    processed_storm: int
    snr: float  # useful share of what normal agents actually consumed
    storm_capped_at_s: float | None  # budget-exhaustion time of the rogue
    mean_blocked_s_per_normal: float
    credits_charged: int
    samples: tuple[Sample, ...]


def run_scenario(params: ScenarioParams, *, seed: int) -> RunResult:
    """Build the storm world, run it to completion, and collect metrics."""
    engine = Engine(params.config, seed=seed)
    normal_ids = [AgentId(i) for i in range(params.n_normals)]
    for aid in normal_ids:
        engine.add_agent(
            Agent(
                aid,
                AgentRole.NORMAL,
                credits=params.normal_budget,
                send_rate=params.normal_send_rate,
                process_rate=params.normal_process_rate,
                inbox_capacity=params.inbox_capacity,
                peers=tuple(p for p in normal_ids if p != aid),
            )
        )
    rogue_id = AgentId(params.n_normals)
    engine.add_agent(
        Agent(
            rogue_id,
            AgentRole.STORM,
            credits=params.storm_budget,
            send_rate=params.storm_rate,
            process_rate=params.normal_process_rate,
            inbox_capacity=params.inbox_capacity,
            topic=BROADCAST_TOPIC,
            start_s=params.storm_start_s,
        )
    )
    for aid in normal_ids:
        engine.subscribe(BROADCAST_TOPIC, aid)

    metrics = engine.run()

    consumed = metrics.processed_useful + metrics.processed_storm
    normals = [a for a in engine.agents.values() if a.role is AgentRole.NORMAL]
    return RunResult(
        seed=seed,
        delivered_useful=metrics.delivered_useful,
        delivered_storm=metrics.delivered_storm,
        evicted_useful=metrics.evicted_useful,
        evicted_storm=metrics.evicted_storm,
        processed_useful=metrics.processed_useful,
        processed_storm=metrics.processed_storm,
        snr=metrics.processed_useful / consumed if consumed else 1.0,
        storm_capped_at_s=engine.agents[rogue_id].budget_exhausted_at,
        mean_blocked_s_per_normal=sum(a.blocked_total_s for a in normals) / len(normals),
        credits_charged=metrics.credits_charged,
        samples=tuple(metrics.samples),
    )
