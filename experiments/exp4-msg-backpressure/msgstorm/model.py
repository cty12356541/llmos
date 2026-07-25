"""Core domain model for the exp4 message-backpressure simulation.

Mirrors the llmos kernel semantics from issue 5/11:
- Message channel: direct, ordered (FIFO per inbox), backpressure-blocking.
- Topic: broadcast, fan-out over the subscription relation.
- Sender-prepaid billing: every delivery (one copy per recipient) is charged
  to the sender's credit balance, atomically, before the send proceeds.
"""

from collections import deque
from dataclasses import dataclass
from enum import StrEnum
from typing import NewType

AgentId = NewType("AgentId", int)


class AgentRole(StrEnum):
    """Whether an agent communicates normally or storms the topic."""

    NORMAL = "normal"
    STORM = "storm"


class AgentState(StrEnum):
    """Lifecycle of a sender under mechanical + economic backpressure."""

    ACTIVE = "active"
    BLOCKED = "blocked"  # parked on a full recipient inbox (mechanical)
    BROKE = "broke"  # budget exhausted, waiting for recharge (economic)


@dataclass(frozen=True, slots=True)
class Message:
    """One immutable message; each delivery shares the same value."""

    sender: AgentId
    seq: int
    sent_at: float
    is_storm: bool


@dataclass(frozen=True, slots=True)
class PendingDelivery:
    """A charged delivery parked on a full inbox, waiting for space."""

    recipient: AgentId
    message: Message


@dataclass(frozen=True, slots=True)
class SimConfig:
    """Kernel switches under test: prepaid billing and inbox backpressure."""

    prepaid: bool
    backpressure: bool
    cost_per_delivery: int = 1
    duration_s: float = 200.0
    sample_interval_s: float = 1.0
    recharge_interval_s: float | None = None
    recharge_amount: int = 0


class Inbox:
    """Bounded FIFO buffer modelling the context window.

    Mutable by design: a buffer exists solely to be filled and drained.
    `waiters` holds parked deliveries in arrival order, preserving FIFO
    semantics across backpressure.
    """

    def __init__(self, capacity: int) -> None:
        self.capacity = capacity
        self.messages: deque[Message] = deque()
        self.waiters: deque[PendingDelivery] = deque()

    @property
    def has_space(self) -> bool:
        return len(self.messages) < self.capacity


class Agent:
    """Simulation entity. Mutable by design: credits, inbox and state
    evolve over simulated time; that evolution is the thing being measured."""

    def __init__(
        self,
        agent_id: AgentId,
        role: AgentRole,
        *,
        credits: int,
        send_rate: float,
        process_rate: float,
        inbox_capacity: int,
        peers: tuple[AgentId, ...] = (),
        topic: str | None = None,
        start_s: float = 0.0,
    ) -> None:
        self.id = agent_id
        self.role = role
        self.credits = credits
        self.send_rate = send_rate
        self.process_rate = process_rate
        self.inbox = Inbox(inbox_capacity)
        self.peers = peers
        self.topic = topic
        self.start_s = start_s
        self.state = AgentState.ACTIVE
        self.outstanding = 0  # parked deliveries not yet landed
        self.budget_exhausted_at: float | None = None
        self.blocked_since: float | None = None
        self.blocked_total_s = 0.0
        self.process_scheduled = False
        self._seq = 0

    def next_seq(self) -> int:
        self._seq += 1
        return self._seq
