"""Given/When/Then tests for the exp4 kernel semantics: prepaid billing,
inbox backpressure blocking, budget exhaustion and recharge recovery."""

from msgstorm.engine import Engine
from msgstorm.model import Agent, AgentId, AgentRole, AgentState, SimConfig

TOPIC = "broadcast"


def _agent(
    aid: int,
    role: AgentRole,
    *,
    credits: int = 0,
    inbox_capacity: int = 10,
    peers: tuple[AgentId, ...] = (),
    topic: str | None = None,
) -> Agent:
    return Agent(
        AgentId(aid),
        role,
        credits=credits,
        send_rate=1.0,
        process_rate=1.0,
        inbox_capacity=inbox_capacity,
        peers=peers,
        topic=topic,
    )


def _engine(*, prepaid: bool = True, backpressure: bool = False, cost: int = 1) -> Engine:
    config = SimConfig(prepaid=prepaid, backpressure=backpressure, cost_per_delivery=cost)
    return Engine(config, seed=1)


def _storm_world(engine: Engine, n_subs: int, budget: int, inbox_capacity: int = 10) -> None:
    """Add n_subs normal agents subscribed to TOPIC plus one storm publisher."""
    for i in range(n_subs):
        engine.add_agent(_agent(i, AgentRole.NORMAL, inbox_capacity=inbox_capacity))
        engine.subscribe(TOPIC, AgentId(i))
    engine.add_agent(_agent(n_subs, AgentRole.STORM, credits=budget, topic=TOPIC))


def test_billing_charges_one_credit_per_delivery() -> None:
    # Given a storm agent with 10 credits and a topic with 3 subscribers
    engine = _engine(prepaid=True)
    _storm_world(engine, n_subs=3, budget=10)
    # When it publishes one message
    engine.try_send(AgentId(3))
    # Then exactly 3 credits are charged and 3 copies are delivered
    assert engine.agents[AgentId(3)].credits == 7
    assert engine.metrics.delivered_storm == 3
    assert engine.metrics.credits_charged == 3
    for i in range(3):
        assert len(engine.agents[AgentId(i)].inbox.messages) == 1


def test_billing_partial_fanout_when_budget_runs_out() -> None:
    # Given a storm agent whose budget (2) is smaller than the fan-out (3)
    engine = _engine(prepaid=True)
    _storm_world(engine, n_subs=3, budget=2)
    # When it publishes
    engine.try_send(AgentId(3))
    # Then only 2 deliveries happen, the third is rejected, and the agent is broke
    assert engine.agents[AgentId(3)].credits == 0
    assert engine.metrics.delivered_storm == 2
    assert engine.agents[AgentId(3)].state is AgentState.BROKE
    assert engine.agents[AgentId(3)].budget_exhausted_at is not None


def test_billing_exact_budget_then_next_send_rejected() -> None:
    # Given a storm agent whose budget exactly covers one fan-out (3)
    engine = _engine(prepaid=True)
    _storm_world(engine, n_subs=3, budget=3)
    # When it publishes once, the send succeeds atomically
    engine.try_send(AgentId(3))
    assert engine.metrics.delivered_storm == 3
    assert engine.agents[AgentId(3)].state is AgentState.ACTIVE
    # When it tries to publish again with an empty balance
    engine.try_send(AgentId(3))
    # Then nothing more is delivered and the agent suspends
    assert engine.metrics.delivered_storm == 3
    assert engine.agents[AgentId(3)].state is AgentState.BROKE


def test_free_mode_never_charges() -> None:
    # Given billing disabled and a storm agent with zero credits
    engine = _engine(prepaid=False)
    _storm_world(engine, n_subs=3, budget=0)
    # When it publishes
    engine.try_send(AgentId(3))
    # Then all deliveries proceed and no credit is charged
    assert engine.metrics.delivered_storm == 3
    assert engine.metrics.credits_charged == 0
    assert engine.agents[AgentId(3)].state is AgentState.ACTIVE


def test_backpressure_blocks_sender_until_receiver_drains() -> None:
    # Given backpressure on, receiver Y with inbox capacity 1, sender X
    engine = _engine(prepaid=True, backpressure=True)
    engine.add_agent(_agent(0, AgentRole.NORMAL, credits=10, peers=(AgentId(1),)))
    engine.add_agent(_agent(1, AgentRole.NORMAL, inbox_capacity=1))
    # When X sends twice without Y processing
    engine.try_send(AgentId(0))
    engine.try_send(AgentId(0))
    # Then the second send is parked and X is blocked
    assert len(engine.agents[AgentId(1)].inbox.messages) == 1
    assert engine.agents[AgentId(0)].state is AgentState.BLOCKED
    assert engine.agents[AgentId(0)].outstanding == 1
    # When Y processes one message
    engine.process_one(AgentId(1))
    # Then the parked delivery lands in the freed slot and X resumes
    assert len(engine.agents[AgentId(1)].inbox.messages) == 1
    assert engine.agents[AgentId(0)].state is AgentState.ACTIVE
    assert engine.agents[AgentId(0)].outstanding == 0


def test_fifo_order_preserved_through_backpressure() -> None:
    # Given capacity 1 and three sends from X to Y (two get parked)
    engine = _engine(prepaid=True, backpressure=True)
    engine.add_agent(_agent(0, AgentRole.NORMAL, credits=10, peers=(AgentId(1),)))
    engine.add_agent(_agent(1, AgentRole.NORMAL, inbox_capacity=1))
    engine.try_send(AgentId(0))  # lands in the inbox
    engine.try_send(AgentId(0))  # parked; X blocks (a blocked sender cannot send more)
    # When Y drains its inbox, re-sending whenever X unblocks
    first = engine.process_one(AgentId(1))  # drains msg 1, parked msg 2 lands, X resumes
    engine.try_send(AgentId(0))  # parked again; X blocks
    rest = [engine.process_one(AgentId(1)) for _ in range(2)]
    # Then messages arrive in send order
    assert [m.seq if m else None for m in [first, *rest]] == [1, 2, 3]


def test_no_backpressure_evicts_oldest() -> None:
    # Given backpressure off and receiver inbox capacity 1
    engine = _engine(prepaid=True, backpressure=False)
    engine.add_agent(_agent(0, AgentRole.NORMAL, credits=10, peers=(AgentId(1),)))
    engine.add_agent(_agent(1, AgentRole.NORMAL, inbox_capacity=1))
    # When X sends twice without Y processing
    engine.try_send(AgentId(0))
    engine.try_send(AgentId(0))
    # Then the inbox holds only the newest message; the oldest was evicted
    inbox = engine.agents[AgentId(1)].inbox.messages
    assert [m.seq for m in inbox] == [2]
    assert engine.metrics.evicted_useful == 1
    assert engine.agents[AgentId(0)].state is AgentState.ACTIVE


def test_budget_exhaustion_caps_storm_in_full_run() -> None:
    # Given a full scenario run with a small storm budget
    from msgstorm.scenario import ScenarioParams, run_scenario

    params = ScenarioParams(
        config=SimConfig(prepaid=True, backpressure=True, duration_s=100.0),
        storm_budget=500,
    )
    # When the simulation runs to completion
    result = run_scenario(params, seed=7)
    # Then storm deliveries are capped at exactly the budget
    assert result.delivered_storm == 500
    assert result.storm_capped_at_s is not None
    # And normal traffic continues after the storm dies
    assert result.delivered_useful > 0


def test_free_storm_grows_past_any_prepaid_cap() -> None:
    # Given the same scenario with billing disabled
    from msgstorm.scenario import ScenarioParams, run_scenario

    params = ScenarioParams(
        config=SimConfig(prepaid=False, backpressure=True, duration_s=100.0),
        storm_budget=500,
    )
    # When the simulation runs
    result = run_scenario(params, seed=7)
    # Then storm volume far exceeds the prepaid cap of 500
    assert result.delivered_storm > 10_000
    assert result.storm_capped_at_s is None


def test_recharge_resumes_sending() -> None:
    # Given a storm agent that has exhausted its budget
    engine = _engine(prepaid=True)
    _storm_world(engine, n_subs=3, budget=1)
    engine.try_send(AgentId(3))
    assert engine.agents[AgentId(3)].state is AgentState.BROKE
    # When it is recharged
    engine.recharge(AgentId(3), 5)
    # Then it is active again and can publish (charged per delivery)
    assert engine.agents[AgentId(3)].state is AgentState.ACTIVE
    engine.try_send(AgentId(3))
    assert engine.metrics.delivered_storm == 1 + 3
    assert engine.agents[AgentId(3)].credits == 2


def test_blocked_on_full_inbox_keeps_budget_charge_per_attempt() -> None:
    # Given backpressure on, prepaid on, and all 2 inboxes full (capacity 1)
    engine = _engine(prepaid=True, backpressure=True)
    _storm_world(engine, n_subs=2, budget=10, inbox_capacity=1)
    storm = AgentId(2)
    engine.try_send(storm)  # fills both inboxes
    # When the storm publishes again, both deliveries park and are charged
    engine.try_send(storm)
    # Then 4 credits were spent and the sender is blocked, not broke
    assert engine.agents[storm].credits == 6
    assert engine.agents[storm].state is AgentState.BLOCKED
    assert engine.metrics.delivered_storm == 2  # only the first fan-out landed
