"""Discrete-event engine implementing the simplified llmos kernel semantics.

Faithful invariants (simplified but honest):
- Atomic deduction: one credit per delivery, charged to the sender before
  the delivery is attempted; a fan-out stops at the first unaffordable copy.
- Backpressure blocking: a full inbox parks the delivery and suspends the
  sender until a receiver drains a slot (FIFO waiters preserve ordering).
- Budget suspension: a sender that cannot afford the next delivery is BROKE
  and stays silent until `recharge` tops it up.
"""

import heapq
import random
from dataclasses import dataclass
from typing import assert_never

from msgstorm.metrics import Metrics, Sample
from msgstorm.model import (
    Agent,
    AgentId,
    AgentRole,
    AgentState,
    Message,
    PendingDelivery,
    SimConfig,
)


@dataclass(frozen=True, slots=True)
class SendAttempt:
    agent: AgentId


@dataclass(frozen=True, slots=True)
class ProcessInbox:
    agent: AgentId


@dataclass(frozen=True, slots=True)
class SampleTick:
    """Periodic metrics sample."""


@dataclass(frozen=True, slots=True)
class RechargeTick:
    """Periodic top-up of every agent's balance."""


Event = SendAttempt | ProcessInbox | SampleTick | RechargeTick


@dataclass(frozen=True, slots=True)
class StormWithoutTopicError(Exception):
    """A storm-role agent was scheduled without a target topic."""

    agent: int

    def __str__(self) -> str:
        return f"storm agent {self.agent} has no topic to publish to"


class Engine:
    """Single-process discrete-event simulator."""

    def __init__(self, config: SimConfig, *, seed: int) -> None:
        self.config = config
        self.rng = random.Random(seed)
        self.agents: dict[AgentId, Agent] = {}
        self.topics: dict[str, list[AgentId]] = {}
        self.metrics = Metrics()
        self.now = 0.0
        self._queue: list[tuple[float, int, Event]] = []
        self._counter = 0

    # --- world construction -------------------------------------------------

    def add_agent(self, agent: Agent) -> None:
        self.agents[agent.id] = agent

    def subscribe(self, topic: str, subscriber: AgentId) -> None:
        self.topics.setdefault(topic, []).append(subscriber)

    # --- public semantic operations (event loop and tests share these) ------

    def try_send(self, agent_id: AgentId) -> None:
        """One synchronous send attempt: charge, then deliver / park / evict."""
        agent = self.agents[agent_id]
        if agent.state is not AgentState.ACTIVE:
            return
        msg = Message(
            sender=agent.id,
            seq=agent.next_seq(),
            sent_at=self.now,
            is_storm=agent.role is AgentRole.STORM,
        )
        self._fan_out(agent, msg, self._recipients(agent))

    def process_one(self, agent_id: AgentId) -> Message | None:
        """Drain one inbox message; wake the oldest parked delivery if any."""
        inbox = self.agents[agent_id].inbox
        if not inbox.messages:
            return None
        msg = inbox.messages.popleft()
        if msg.is_storm:
            self.metrics.processed_storm += 1
        else:
            self.metrics.processed_useful += 1
        if inbox.waiters:
            pending = inbox.waiters.popleft()
            inbox.messages.append(pending.message)
            if pending.message.is_storm:
                self.metrics.delivered_storm += 1
            else:
                self.metrics.delivered_useful += 1
            sender = self.agents[pending.message.sender]
            sender.outstanding -= 1
            if sender.outstanding == 0 and sender.state is AgentState.BLOCKED:
                self._resume_from_block(sender)
        return msg

    def recharge(self, agent_id: AgentId, amount: int) -> None:
        """Top up a balance; a BROKE agent resumes sending."""
        agent = self.agents[agent_id]
        agent.credits += amount
        if agent.state is AgentState.BROKE:
            agent.state = AgentState.ACTIVE
            self._schedule(self.now + self.rng.expovariate(agent.send_rate), SendAttempt(agent.id))

    # --- event loop -----------------------------------------------------------

    def run(self) -> Metrics:
        self._schedule_initial()
        while self._queue:
            t, _, event = heapq.heappop(self._queue)
            if t > self.config.duration_s:
                break
            self.now = t
            match event:
                case SendAttempt(agent=aid):
                    self.try_send(aid)
                    if self.agents[aid].state is AgentState.ACTIVE:
                        self._reschedule_send(aid)
                case ProcessInbox(agent=aid):
                    agent = self.agents[aid]
                    agent.process_scheduled = False
                    self.process_one(aid)
                    if agent.inbox.messages:
                        self._schedule(
                            self.now + self.rng.expovariate(agent.process_rate),
                            ProcessInbox(aid),
                        )
                        agent.process_scheduled = True
                case SampleTick():
                    self._record_sample()
                    self._schedule(self.now + self.config.sample_interval_s, SampleTick())
                case RechargeTick():
                    for aid in self.agents:
                        self.recharge(aid, self.config.recharge_amount)
                    interval = self.config.recharge_interval_s
                    if interval is not None:
                        self._schedule(self.now + interval, RechargeTick())
                case unreachable:
                    assert_never(unreachable)
        self._close_out_blocked_time()
        return self.metrics

    # --- internals ------------------------------------------------------------

    def _recipients(self, agent: Agent) -> list[AgentId]:
        match agent.role:
            case AgentRole.NORMAL:
                return [self.rng.choice(agent.peers)] if agent.peers else []
            case AgentRole.STORM:
                if agent.topic is None:
                    raise StormWithoutTopicError(agent=int(agent.id))
                return list(self.topics[agent.topic])
            case unreachable:
                return assert_never(unreachable)

    def _fan_out(self, agent: Agent, msg: Message, recipients: list[AgentId]) -> None:
        for rid in recipients:
            if self.config.prepaid:
                if agent.credits < self.config.cost_per_delivery:
                    self._exhaust_budget(agent)
                    return
                agent.credits -= self.config.cost_per_delivery
                self.metrics.credits_charged += self.config.cost_per_delivery
            inbox = self.agents[rid].inbox
            if inbox.has_space:
                self._deliver(rid, msg)
            elif self.config.backpressure:
                inbox.waiters.append(PendingDelivery(rid, msg))
                agent.outstanding += 1
            else:
                self._evict_oldest(rid, msg)
        if agent.outstanding > 0:
            self._block(agent)

    def _deliver(self, recipient: AgentId, msg: Message) -> None:
        target = self.agents[recipient]
        target.inbox.messages.append(msg)
        if msg.is_storm:
            self.metrics.delivered_storm += 1
        else:
            self.metrics.delivered_useful += 1
        if not target.process_scheduled:
            self._schedule(
                self.now + self.rng.expovariate(target.process_rate),
                ProcessInbox(recipient),
            )
            target.process_scheduled = True

    def _evict_oldest(self, recipient: AgentId, msg: Message) -> None:
        evicted = self.agents[recipient].inbox.messages.popleft()
        if evicted.is_storm:
            self.metrics.evicted_storm += 1
        else:
            self.metrics.evicted_useful += 1
        self._deliver(recipient, msg)

    def _block(self, agent: Agent) -> None:
        agent.state = AgentState.BLOCKED
        agent.blocked_since = self.now

    def _resume_from_block(self, agent: Agent) -> None:
        agent.state = AgentState.ACTIVE
        if agent.blocked_since is not None:
            agent.blocked_total_s += self.now - agent.blocked_since
            agent.blocked_since = None
        if self.config.prepaid and agent.credits < self.config.cost_per_delivery:
            self._exhaust_budget(agent)
            return
        self._reschedule_send(agent.id)

    def _exhaust_budget(self, agent: Agent) -> None:
        if agent.budget_exhausted_at is None:
            agent.budget_exhausted_at = self.now
        if agent.outstanding > 0:
            self._block(agent)  # still owes parked deliveries; BROKE on resume
        else:
            agent.state = AgentState.BROKE

    def _reschedule_send(self, agent_id: AgentId) -> None:
        agent = self.agents[agent_id]
        self._schedule(self.now + self.rng.expovariate(agent.send_rate), SendAttempt(agent_id))

    def _record_sample(self) -> None:
        normals = [a for a in self.agents.values() if a.role is AgentRole.NORMAL]
        fill = (
            sum(len(a.inbox.messages) for a in normals) / (len(normals) * normals[0].inbox.capacity)
            if normals
            else 0.0
        )
        blocked = sum(1 for a in normals if a.state is AgentState.BLOCKED)
        self.metrics.samples.append(
            Sample(
                t=self.now,
                delivered_storm=self.metrics.delivered_storm,
                delivered_useful=self.metrics.delivered_useful,
                processed_storm=self.metrics.processed_storm,
                processed_useful=self.metrics.processed_useful,
                inbox_fill_mean=fill,
                blocked_normals=blocked,
            )
        )

    def _close_out_blocked_time(self) -> None:
        for agent in self.agents.values():
            if agent.state is AgentState.BLOCKED and agent.blocked_since is not None:
                agent.blocked_total_s += self.now - agent.blocked_since
                agent.blocked_since = None

    def _schedule_initial(self) -> None:
        for agent in self.agents.values():
            if agent.start_s > 0:
                self._schedule(agent.start_s, SendAttempt(agent.id))
            else:
                self._schedule(self.rng.expovariate(agent.send_rate), SendAttempt(agent.id))
        self._schedule(self.config.sample_interval_s, SampleTick())
        if self.config.recharge_interval_s is not None:
            self._schedule(self.config.recharge_interval_s, RechargeTick())

    def _schedule(self, t: float, event: Event) -> None:
        self._counter += 1
        heapq.heappush(self._queue, (t, self._counter, event))
