//! W44-RA integration coverage of the durable throttle-decision ledger:
//! normal path, replay/conflict, authority-verified chain, state/fence/
//! timestamp gates, restart read-back, legacy v6-with-data upgrade, the
//! migration tri-state, and DDL immutability of the ledger rows.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use nlos_resource::{
    ActivateReservationRequest, ConsumeReservationRequest, CreateAccountRequest,
    CreateQuoteRequest, FinalizeReservationRequest, QuarantineReservationRequest,
    RecordThrottleDecisionRequest, RegisterDriverRequest, ReservationRecord, ReserveRequest,
    ResourceAuthority, ResourceAuthorityError, ResourceDemand, RotateDriverRequest,
    ThrottleDecisionDecision,
};
use nlos_types::{CallId, IdempotencyKey, OperationId, ReservationId};
use rusqlite::Connection;

static NEXT: AtomicU64 = AtomicU64::new(1);
struct Root(PathBuf);
impl Root {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "nlos-resource-{label}-{}-{nonce}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
    }
    fn path(&self) -> &Path {
        &self.0
    }
    fn database(&self) -> PathBuf {
        self.0.join("resource-authority.db")
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn driver_request(seed: u8) -> RegisterDriverRequest {
    RegisterDriverRequest {
        profile_digest: [seed; 32],
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(1); 16]),
        created_at_ms: 1000,
    }
}
fn account_request(seed: u8, credit: u64) -> CreateAccountRequest {
    CreateAccountRequest {
        initial_credit: credit,
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(2); 16]),
        created_at_ms: 1000,
    }
}
#[allow(clippy::large_types_passed_by_value)] // Keep fixture call sites compact and readable.
fn quote_request(
    seed: u8,
    d: nlos_resource::DriverRecord,
    upper: u64,
    capacity: ResourceDemand,
) -> CreateQuoteRequest {
    CreateQuoteRequest {
        driver_id: d.driver_id,
        driver_generation: d.generation,
        driver_fencing_token: d.fencing_token,
        operation_proposal_digest: [seed.wrapping_add(3); 32],
        pricing_version: [seed.wrapping_add(4); 32],
        upper_bound: upper,
        demand_capacity: capacity,
        valid_until_ms: 10_000,
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(5); 16]),
        created_at_ms: 1000,
    }
}
#[allow(clippy::large_types_passed_by_value)] // Keep fixture call sites compact and readable.
fn reserve_request(
    seed: u8,
    a: nlos_resource::AccountRecord,
    q: nlos_resource::QuoteRecord,
    demand: ResourceDemand,
) -> ReserveRequest {
    ReserveRequest {
        account_id: a.account_id,
        quote_id: q.quote_id,
        call_id: CallId::from_bytes([seed.wrapping_add(6); 16]),
        operation_id: OperationId::from_bytes([seed.wrapping_add(7); 16]),
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(8); 16]),
        demand,
        reserved_at_ms: 2000,
    }
}
fn activate_request(r: &ReservationRecord) -> ActivateReservationRequest {
    ActivateReservationRequest {
        reservation_id: r.reservation_id,
        call_id: r.call_id,
        operation_id: r.operation_id,
        driver_id: r.driver_id,
        driver_generation: r.driver_generation,
        driver_fencing_token: r.driver_fencing_token,
        activation_token: r.activation_token,
        activated_at_ms: 3000,
    }
}

const DEMAND: ResourceDemand = ResourceDemand {
    cpu_shares: 64,
    memory_mib: 512,
    io_weight: 5,
};
const CAPACITY: ResourceDemand = ResourceDemand {
    cpu_shares: 100,
    memory_mib: 1024,
    io_weight: 10,
};

/// Authoritative decision request fixture: `demand_after` is always derived
/// by the crate's own adjustment, so a test only breaks it deliberately.
fn throttle_request(
    seed: u8,
    reservation_id: ReservationId,
    percent: u64,
) -> RecordThrottleDecisionRequest {
    RecordThrottleDecisionRequest {
        reservation_id,
        throttle_percent: percent,
        demand_before: DEMAND,
        demand_after: DEMAND.throttled_to_percent(percent),
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(9); 16]),
        decided_at_ms: 5000,
    }
}

/// One RESERVED reservation declaring [`DEMAND`] under [`CAPACITY`].
fn reserved(authority: &ResourceAuthority, seed: u8) -> ReservationRecord {
    let driver = authority
        .register_driver(driver_request(seed))
        .unwrap()
        .record();
    let account = authority
        .create_account(account_request(seed, 1000))
        .unwrap();
    let quote = authority
        .create_quote(quote_request(seed, driver, 100, CAPACITY))
        .unwrap()
        .record();
    authority
        .reserve(reserve_request(seed, account, quote, DEMAND))
        .unwrap()
        .record()
}

fn user_version(root: &Root) -> i64 {
    let raw = Connection::open(root.database()).unwrap();
    raw.pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap()
}

#[test]
fn throttle_decision_records_replays_and_merges_the_effective_demand() {
    let root = Root::new("throttle-record");
    let authority = ResourceAuthority::open(root.path()).unwrap();
    let reservation = reserved(&authority, 10);

    // Empty ledger: the read face is exactly the declared demand.
    let empty = authority
        .inspect_effective_demand(reservation.reservation_id)
        .unwrap();
    assert_eq!(empty.declared_demand, DEMAND);
    assert_eq!(empty.effective_demand, DEMAND);
    assert_eq!(
        empty.decisions,
        [] as [nlos_resource::ThrottleDecisionReceipt; 0]
    );

    // First link: before = declared demand, after = authoritative 50% scale.
    let request = throttle_request(10, reservation.reservation_id, 50);
    let first = match authority.record_throttle_decision(request) {
        Ok(ThrottleDecisionDecision::Recorded(receipt)) => receipt,
        other => panic!("expected Recorded, got {other:?}"),
    };
    assert_eq!(first.sequence, 1);
    assert_eq!(first.operation_id, reservation.operation_id);
    assert_eq!(first.demand_before, DEMAND);
    assert_eq!(
        first.demand_after,
        ResourceDemand {
            cpu_shares: 32,
            memory_mib: 256,
            io_weight: 2,
        }
    );

    // Exact replay (the timestamp is tolerated, the crate-wide replay
    // convention for identity-excluded timestamps).
    let mut replay = request;
    replay.decided_at_ms = 9_999;
    let replayed = authority.record_throttle_decision(replay).unwrap();
    assert!(matches!(replayed, ThrottleDecisionDecision::Replayed(r) if r == first));

    // Second link continues the chain from the first decision's after.
    let mut second = throttle_request(11, reservation.reservation_id, 50);
    second.demand_before = first.demand_after;
    second.demand_after = first.demand_after.throttled_to_percent(50);
    second.decided_at_ms = 6_000;
    let second_receipt = authority
        .record_throttle_decision(second)
        .unwrap()
        .receipt();
    assert_eq!(second_receipt.sequence, 2);

    // The merged read face folds the whole chain.
    let merged = authority
        .inspect_effective_demand(reservation.reservation_id)
        .unwrap();
    assert_eq!(merged.declared_demand, DEMAND);
    assert_eq!(merged.effective_demand, second_receipt.demand_after);
    assert_eq!(merged.decisions, [first, second_receipt]);

    // The ledger read face returns the same rows in sequence order.
    assert_eq!(
        authority
            .inspect_throttle_decisions(reservation.reservation_id)
            .unwrap(),
        [first, second_receipt]
    );

    // A zero-percent decision collapses the effective demand to zero.
    let mut collapse = throttle_request(12, reservation.reservation_id, 0);
    collapse.demand_before = second_receipt.demand_after;
    collapse.demand_after = ResourceDemand::default();
    collapse.decided_at_ms = 7_000;
    authority.record_throttle_decision(collapse).unwrap();
    assert_eq!(
        authority
            .inspect_effective_demand(reservation.reservation_id)
            .unwrap()
            .effective_demand,
        ResourceDemand::default()
    );
}

#[test]
fn throttle_ledger_leaves_the_existing_reservation_flows_untouched() {
    let root = Root::new("throttle-flows");
    let authority = ResourceAuthority::open(root.path()).unwrap();
    let reservation = reserved(&authority, 20);

    // A decision recorded while RESERVED changes neither the declared row
    // nor the credit hold.
    let decision = authority
        .record_throttle_decision(throttle_request(20, reservation.reservation_id, 50))
        .unwrap()
        .receipt();
    let stored = authority
        .inspect_reservation(reservation.reservation_id)
        .unwrap();
    assert_eq!(stored.demand, DEMAND, "the declared demand stays immutable");
    assert_eq!(stored.state, nlos_resource::ReservationState::Reserved);
    assert_eq!(stored.upper_bound, 100);
    assert_eq!(
        authority
            .inspect_account(reservation.account_id)
            .unwrap()
            .available_credit,
        900,
        "the decision moves no credit"
    );

    // Activation and consumption keep working exactly as before; accounting
    // stays on the reserve upper bound, never on a demand.
    let activation = authority
        .activate(activate_request(&reservation))
        .unwrap()
        .receipt();
    authority
        .consume(ConsumeReservationRequest {
            reservation_id: reservation.reservation_id,
            operation_id: reservation.operation_id,
            activation_receipt_id: activation.receipt_id,
            sequence: 1,
            cumulative_usage: 40,
            consumed_at_ms: 4_001,
        })
        .unwrap();
    assert_eq!(
        authority
            .inspect_effective_demand(reservation.reservation_id)
            .unwrap()
            .effective_demand,
        decision.demand_after,
        "activation/consume do not disturb the merged read face"
    );

    // The chain continues while ACTIVE, from the live effective demand.
    let mut second = throttle_request(21, reservation.reservation_id, 100);
    second.demand_before = decision.demand_after;
    second.demand_after = decision.demand_after;
    second.decided_at_ms = 5_000;
    let second_receipt = authority
        .record_throttle_decision(second)
        .unwrap()
        .receipt();
    assert_eq!(second_receipt.sequence, 2);
    assert_eq!(
        authority
            .inspect_effective_demand(reservation.reservation_id)
            .unwrap()
            .effective_demand,
        decision.demand_after
    );
}

#[test]
fn throttle_replay_conflict_fails_closed_on_any_content_mismatch() {
    let root = Root::new("throttle-conflict");
    let authority = ResourceAuthority::open(root.path()).unwrap();
    let reservation = reserved(&authority, 30);
    let other = reserved(&authority, 31);
    let request = throttle_request(30, reservation.reservation_id, 50);
    authority.record_throttle_decision(request).unwrap();

    let mut percent = request;
    percent.throttle_percent = 25;
    percent.demand_after = DEMAND.throttled_to_percent(25);
    assert!(matches!(
        authority.record_throttle_decision(percent),
        Err(ResourceAuthorityError::IdempotencyConflict)
    ));

    let mut after = request;
    after.demand_after = DEMAND.throttled_to_percent(25);
    assert!(matches!(
        authority.record_throttle_decision(after),
        Err(ResourceAuthorityError::IdempotencyConflict)
    ));

    let mut rebound = request;
    rebound.reservation_id = other.reservation_id;
    assert!(matches!(
        authority.record_throttle_decision(rebound),
        Err(ResourceAuthorityError::IdempotencyConflict)
    ));

    // The conflicting replays leave the durable ledger untouched.
    assert_eq!(
        authority
            .inspect_throttle_decisions(reservation.reservation_id)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        authority
            .inspect_throttle_decisions(other.reservation_id)
            .unwrap(),
        [] as [nlos_resource::ThrottleDecisionReceipt; 0]
    );
}

#[test]
fn throttle_chain_and_adjustment_are_authority_verified() {
    let root = Root::new("throttle-chain");
    let authority = ResourceAuthority::open(root.path()).unwrap();
    let reservation = reserved(&authority, 40);
    let first = authority
        .record_throttle_decision(throttle_request(40, reservation.reservation_id, 50))
        .unwrap()
        .receipt();

    // Stale chain input: before must be the current effective demand.
    let mut stale = throttle_request(41, reservation.reservation_id, 25);
    stale.demand_before = DEMAND;
    stale.demand_after = DEMAND.throttled_to_percent(25);
    assert!(matches!(
        authority.record_throttle_decision(stale),
        Err(ResourceAuthorityError::StaleThrottleDemand {
            expected,
            reported: DEMAND
        }) if expected == first.demand_after
    ));

    // Non-authoritative adjustment: after must be the crate's scaling of
    // before, never a caller-invented demand.
    let mut invented = throttle_request(42, reservation.reservation_id, 25);
    invented.demand_before = first.demand_after;
    invented.demand_after = ResourceDemand {
        cpu_shares: 1,
        memory_mib: 1,
        io_weight: 1,
    };
    assert!(matches!(
        authority.record_throttle_decision(invented),
        Err(ResourceAuthorityError::InvalidThrottleAdjustment {
            expected,
            reported
        }) if expected == first.demand_after.throttled_to_percent(25) && reported != expected
    ));

    assert_eq!(
        authority
            .inspect_throttle_decisions(reservation.reservation_id)
            .unwrap()
            .len(),
        1,
        "rejected links are not persisted"
    );
}

#[test]
fn throttle_percent_domain_and_unknown_reservation_fail_closed() {
    let root = Root::new("throttle-percent");
    let authority = ResourceAuthority::open(root.path()).unwrap();
    let reservation = reserved(&authority, 50);
    let mut wide = throttle_request(50, reservation.reservation_id, 101);
    wide.demand_after = DEMAND; // a >100 percent that "widens back" to identity
    assert!(matches!(
        authority.record_throttle_decision(wide),
        Err(ResourceAuthorityError::InvalidThrottlePercent)
    ));
    let mut huge = throttle_request(50, reservation.reservation_id, u64::MAX);
    huge.demand_after = DEMAND;
    assert!(matches!(
        authority.record_throttle_decision(huge),
        Err(ResourceAuthorityError::InvalidThrottlePercent)
    ));
    assert_eq!(
        authority
            .inspect_throttle_decisions(reservation.reservation_id)
            .unwrap(),
        [] as [nlos_resource::ThrottleDecisionReceipt; 0]
    );
    assert!(matches!(
        authority.record_throttle_decision(throttle_request(
            1,
            ReservationId::from_bytes([0xFF; 16]),
            50
        )),
        Err(ResourceAuthorityError::ReservationNotFound)
    ));
    assert!(matches!(
        authority.inspect_effective_demand(ReservationId::from_bytes([0xEE; 16])),
        Err(ResourceAuthorityError::ReservationNotFound)
    ));
}

#[test]
fn throttle_refuses_terminal_states_and_stale_fence() {
    let root = Root::new("throttle-gates");
    let authority = ResourceAuthority::open(root.path()).unwrap();

    // QUARANTINED: frozen, no new decisions.
    let quarantined = reserved(&authority, 60);
    let activation = authority
        .activate(activate_request(&quarantined))
        .unwrap()
        .receipt();
    authority
        .quarantine(QuarantineReservationRequest {
            reservation_id: quarantined.reservation_id,
            operation_id: quarantined.operation_id,
            activation_receipt_id: activation.receipt_id,
            reason_digest: [0x60; 32],
            quarantined_at_ms: 4_500,
        })
        .unwrap();
    assert!(matches!(
        authority.record_throttle_decision(throttle_request(60, quarantined.reservation_id, 50)),
        Err(ResourceAuthorityError::ReservationQuarantined)
    ));

    // FINALIZED: settled and released, nothing left to throttle.
    let finalized = reserved(&authority, 61);
    let activation = authority
        .activate(activate_request(&finalized))
        .unwrap()
        .receipt();
    authority
        .finalize_reservation(FinalizeReservationRequest {
            reservation_id: finalized.reservation_id,
            operation_id: finalized.operation_id,
            activation_receipt_id: activation.receipt_id,
            effect_closed_proof_digest: [0x61; 32],
            final_seq: 0,
            final_usage: 0,
            finalized_at_ms: 5_000,
        })
        .unwrap();
    assert!(matches!(
        authority.record_throttle_decision(throttle_request(61, finalized.reservation_id, 50)),
        Err(ResourceAuthorityError::ReservationFinalized)
    ));

    // Stale driver fence: like every mutating path, the decision refuses.
    let fenced = reserved(&authority, 62);
    authority
        .rotate_driver(RotateDriverRequest {
            driver_id: fenced.driver_id,
            expected_generation: fenced.driver_generation,
            expected_fencing_token: fenced.driver_fencing_token,
            idempotency_key: IdempotencyKey::from_bytes([0x62; 16]),
            rotated_at_ms: 4_000,
        })
        .unwrap();
    assert!(matches!(
        authority.record_throttle_decision(throttle_request(62, fenced.reservation_id, 50)),
        Err(ResourceAuthorityError::StaleDriver)
    ));
}

#[test]
fn throttle_timestamp_regressions_fail_closed() {
    let root = Root::new("throttle-timestamps");
    let authority = ResourceAuthority::open(root.path()).unwrap();

    // A RESERVED reservation refuses a decision that predates its creation
    // (fixtures create reservations at 2_000).
    let reservation = reserved(&authority, 70);
    let mut early = throttle_request(70, reservation.reservation_id, 50);
    early.decided_at_ms = 1_999;
    assert!(matches!(
        authority.record_throttle_decision(early),
        Err(ResourceAuthorityError::InvalidThrottleTimestamp)
    ));

    // An ACTIVE reservation refuses a decision that predates activation
    // (fixtures activate at 3_000).
    authority.activate(activate_request(&reservation)).unwrap();
    let mut pre_activation = throttle_request(70, reservation.reservation_id, 50);
    pre_activation.decided_at_ms = 2_999;
    assert!(matches!(
        authority.record_throttle_decision(pre_activation),
        Err(ResourceAuthorityError::InvalidThrottleTimestamp)
    ));

    // The chain refuses a decision that predates the latest decision.
    let first = authority
        .record_throttle_decision(throttle_request(71, reservation.reservation_id, 50))
        .unwrap()
        .receipt();
    assert_eq!(first.decided_at_ms, 5_000);
    let mut regressed = throttle_request(72, reservation.reservation_id, 50);
    regressed.demand_before = first.demand_after;
    regressed.demand_after = first.demand_after.throttled_to_percent(50);
    regressed.decided_at_ms = 4_999;
    assert!(matches!(
        authority.record_throttle_decision(regressed),
        Err(ResourceAuthorityError::InvalidThrottleTimestamp)
    ));
    assert_eq!(
        authority
            .inspect_throttle_decisions(reservation.reservation_id)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn throttle_ledger_survives_restart_and_replays_after_reopen() {
    let root = Root::new("throttle-restart");
    let reservation_id = {
        let authority = ResourceAuthority::open(root.path()).unwrap();
        let reservation = reserved(&authority, 80);
        let first = authority
            .record_throttle_decision(throttle_request(80, reservation.reservation_id, 50))
            .unwrap()
            .receipt();
        let mut second = throttle_request(81, reservation.reservation_id, 25);
        second.demand_before = first.demand_after;
        second.demand_after = first.demand_after.throttled_to_percent(25);
        second.decided_at_ms = 6_000;
        authority.record_throttle_decision(second).unwrap();
        reservation.reservation_id
    };

    let reopened = ResourceAuthority::open(root.path()).unwrap();
    let merged = reopened.inspect_effective_demand(reservation_id).unwrap();
    assert_eq!(merged.declared_demand, DEMAND);
    assert_eq!(merged.decisions.len(), 2);
    assert_eq!(merged.decisions[0].sequence, 1);
    assert_eq!(merged.decisions[1].sequence, 2);
    assert_eq!(
        merged.effective_demand,
        DEMAND.throttled_to_percent(50).throttled_to_percent(25)
    );
    assert_eq!(
        reopened.inspect_throttle_decisions(reservation_id).unwrap(),
        merged.decisions
    );

    // The exact retry of a pre-restart decision replays the durable receipt.
    let replay = reopened
        .record_throttle_decision(throttle_request(80, reservation_id, 50))
        .unwrap()
        .receipt();
    assert_eq!(replay, merged.decisions[0]);
    assert_eq!(
        reopened
            .inspect_throttle_decisions(reservation_id)
            .unwrap()
            .len(),
        2,
        "the replay adds no ledger row"
    );
}

/// W40-B lesson applied: the legacy-upgrade fixture carries real v6 data
/// (driver, account, quote, reservation, activation, consumption, decision
/// chain) — an empty database proves nothing about data preservation.
#[test]
fn legacy_v6_database_with_data_upgrades_in_place() {
    let root = Root::new("throttle-migration");
    let (reservation, activation, consumption, decision) = {
        let authority = ResourceAuthority::open(root.path()).unwrap();
        let reservation = reserved(&authority, 90);
        let activation = authority
            .activate(activate_request(&reservation))
            .unwrap()
            .receipt();
        let consumption = authority
            .consume(ConsumeReservationRequest {
                reservation_id: reservation.reservation_id,
                operation_id: reservation.operation_id,
                activation_receipt_id: activation.receipt_id,
                sequence: 1,
                cumulative_usage: 40,
                consumed_at_ms: 4_001,
            })
            .unwrap()
            .receipt();
        let decision = authority
            .record_throttle_decision(throttle_request(90, reservation.reservation_id, 50))
            .unwrap()
            .receipt();
        // Capture the live row after activation/consumption so the
        // post-upgrade comparison covers the full migrated state.
        let live = authority
            .inspect_reservation(reservation.reservation_id)
            .unwrap();
        (live, activation, consumption, decision)
    };
    // Strip the v7 objects to reproduce a legacy v6-shaped database (a v6
    // database by definition carries no throttle ledger; dropping the table
    // removes its empty data and its triggers).
    {
        let raw = Connection::open(root.database()).unwrap();
        raw.execute_batch(
            "DROP TABLE reservation_throttle_decisions;
             PRAGMA user_version = 6;",
        )
        .unwrap();
    }
    assert_eq!(user_version(&root), 6);

    let authority = ResourceAuthority::open(root.path()).unwrap();
    assert_eq!(user_version(&root), 7, "the upgrade stamps v7");
    // All v6 data survived byte-identically.
    assert_eq!(
        authority
            .inspect_reservation(reservation.reservation_id)
            .unwrap(),
        reservation
    );
    assert_eq!(
        authority
            .inspect_activation_receipt(reservation.reservation_id)
            .unwrap(),
        activation
    );
    assert_eq!(
        authority
            .inspect_consumption_receipt(reservation.reservation_id, 1)
            .unwrap(),
        consumption
    );
    assert_eq!(
        authority
            .inspect_account(reservation.account_id)
            .unwrap()
            .available_credit,
        900
    );
    // The migrated ledger starts empty (the v6 database had none), the
    // effective demand falls back to the declared demand, and new decisions
    // are recordable over the migrated rows.
    assert_eq!(
        authority
            .inspect_throttle_decisions(reservation.reservation_id)
            .unwrap(),
        [] as [nlos_resource::ThrottleDecisionReceipt; 0]
    );
    assert_eq!(
        authority
            .inspect_effective_demand(reservation.reservation_id)
            .unwrap()
            .effective_demand,
        reservation.demand
    );
    // The pre-downgrade decision re-records over the migrated rows with the
    // same content-derived identity (the v6 database had no ledger row to
    // preserve, so the chain restarts at sequence 1).
    let recorded = authority
        .record_throttle_decision(throttle_request(90, reservation.reservation_id, 50))
        .unwrap()
        .receipt();
    assert_eq!(recorded.sequence, 1);
    assert_eq!(recorded.receipt_id, decision.receipt_id);
    assert_eq!(
        authority
            .inspect_effective_demand(reservation.reservation_id)
            .unwrap()
            .effective_demand,
        DEMAND.throttled_to_percent(50)
    );
}

#[test]
fn complete_unstamped_throttle_schema_reopens_by_stamping_v7() {
    let root = Root::new("throttle-restamp");
    let reservation_id = {
        let authority = ResourceAuthority::open(root.path()).unwrap();
        let reservation = reserved(&authority, 92);
        authority
            .record_throttle_decision(throttle_request(92, reservation.reservation_id, 50))
            .unwrap();
        reservation.reservation_id
    };
    {
        let raw = Connection::open(root.database()).unwrap();
        raw.execute_batch("PRAGMA user_version = 6;").unwrap();
    }

    // Complete schema + unstamped: the reopen only stamps, never rebuilds,
    // so the surviving ledger rows stay byte-identical.
    let authority = ResourceAuthority::open(root.path()).unwrap();
    assert_eq!(user_version(&root), 7);
    let decisions = authority
        .inspect_throttle_decisions(reservation_id)
        .unwrap();
    assert_eq!(decisions.len(), 1);
    assert_eq!(
        authority
            .inspect_effective_demand(reservation_id)
            .unwrap()
            .effective_demand,
        DEMAND.throttled_to_percent(50)
    );

    // A second reopen takes the complete-state fast path.
    drop(authority);
    let reopened = ResourceAuthority::open(root.path()).unwrap();
    assert_eq!(user_version(&root), 7);
    assert_eq!(
        reopened.inspect_throttle_decisions(reservation_id).unwrap(),
        decisions
    );
}

#[test]
fn partial_throttle_schema_fails_closed() {
    // (dropper, stamped version): each case leaves some but not all of the
    // v7 objects — the tri-state contract refuses anything partial whether
    // the dispatch arrives stamped v7 or v6.
    for (dropper, version) in [
        (
            "DROP TRIGGER reservation_throttle_binding_insert;
             DROP TRIGGER reservation_throttle_decisions_immutable_update;
             DROP TRIGGER reservation_throttle_decisions_immutable_delete;",
            7,
        ),
        ("DROP TRIGGER reservation_throttle_binding_insert;", 6),
    ] {
        let root = Root::new("throttle-migration-partial");
        {
            let authority = ResourceAuthority::open(root.path()).unwrap();
            reserved(&authority, 93);
        }
        {
            let raw = Connection::open(root.database()).unwrap();
            raw.execute_batch(&format!("{dropper} PRAGMA user_version = {version};"))
                .unwrap();
        }
        assert!(
            matches!(
                ResourceAuthority::open(root.path()),
                Err(ResourceAuthorityError::CorruptRecord(
                    "partial resource throttle schema"
                ))
            ),
            "dropper must leave a partial schema that fails closed: {dropper}"
        );
    }
}

/// The ledger rows are DDL-immutable history, and the binding trigger is
/// the schema-level backstop for out-of-band inserts.
#[test]
fn throttle_decision_rows_are_ddl_immutable_and_binding_checked() {
    let root = Root::new("throttle-immutable");
    let authority = ResourceAuthority::open(root.path()).unwrap();
    let reservation = reserved(&authority, 94);
    authority
        .record_throttle_decision(throttle_request(94, reservation.reservation_id, 50))
        .unwrap();
    // A quarantined reservation for the overlay half of the binding trigger.
    let quarantined = reserved(&authority, 95);
    let activation = authority
        .activate(activate_request(&quarantined))
        .unwrap()
        .receipt();
    authority
        .quarantine(QuarantineReservationRequest {
            reservation_id: quarantined.reservation_id,
            operation_id: quarantined.operation_id,
            activation_receipt_id: activation.receipt_id,
            reason_digest: [0x95; 32],
            quarantined_at_ms: 4_500,
        })
        .unwrap();
    drop(authority);

    let raw = Connection::open(root.database()).unwrap();
    assert!(
        raw.execute(
            "UPDATE reservation_throttle_decisions SET throttle_percent = 100",
            []
        )
        .is_err(),
        "out-of-band UPDATE must be DDL-rejected"
    );
    assert!(
        raw.execute("DELETE FROM reservation_throttle_decisions", [])
            .is_err(),
        "out-of-band DELETE must be DDL-rejected"
    );
    // Binding trigger, operation mismatch: rejected even on a live
    // reservation.
    assert!(
        raw.execute(
            "INSERT INTO reservation_throttle_decisions VALUES(
                X'00112233445566778899aabbccddeeff',
                X'00112233445566778899aabbccdddeef',
                ?1, ?2, 2, 50, 64, 512, 5, 32, 256, 2, 5000)",
            rusqlite::params![
                reservation.reservation_id.as_bytes().as_slice(),
                [0u8; 16].as_slice()
            ],
        )
        .is_err()
    );
    // Binding trigger, terminal overlay: rejected even with a matching
    // operation.
    assert!(
        raw.execute(
            "INSERT INTO reservation_throttle_decisions VALUES(
                X'00112233445566778899aabbccddeeff',
                X'00112233445566778899aabbccddfeef',
                ?1, ?2, 1, 50, 64, 512, 5, 32, 256, 2, 5000)",
            rusqlite::params![
                quarantined.reservation_id.as_bytes().as_slice(),
                quarantined.operation_id.as_bytes().as_slice()
            ],
        )
        .is_err()
    );
}
