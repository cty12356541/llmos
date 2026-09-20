#![allow(deprecated)] // Ladder constructors deprecated in favor of the *_with_authorities_struct entries.
//! W28-C-1 prepare/finalize coordinator acceptance: the durable Resource
//! finalize envelope + plan state machine (schema v43, ADR-0017 decision
//! R-C). `prepare_resource_finalize` persists the terminal request
//! identity and the Reservation set derived from the sealed write set;
//! `converge_resource_commit_plan` scans incomplete plans, converges the
//! Task side through the existing resource-aware v3 single-transaction
//! path when every owner Reservation is FINALIZED, and reports a typed
//! not-due decision (zero owner mutation, zero ledger failure record)
//! otherwise. All crash windows here are convergence statements, never
//! cross-authority atomicity claims.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_resource::{
    AccountRecord, CreateAccountRequest, CreateQuoteRequest, DriverRecord, QuoteRecord,
    RegisterDriverRequest, ReservationRecord, ReservationState, ReserveRequest, ResourceAuthority,
    ResourceDemand,
};
use nlos_task::{
    AttemptSpec, FinalizeRequest, FinalizeRequestV3, NestedResourceCostReceipt,
    ParticipantRegistryBinding, PermitDecision, PermitRecord, PermitRequest, PermitState,
    PrepareResourceFinalizeRequest, ResourceCommitPlanState, ResourceConvergeDecision,
    ResourceFinalizeDecision, ResourceFinalizeEnvelopeDecision, SnapshotBundle,
    SnapshotConsistency, SqliteTaskAuthority, TaskSnapshotReceiptSpec, TaskSpec, TaskStoreError,
    TaskWriteSetRequest, TaskWriteSetResourceReservationRequest, empty_effect_history_root,
};
use nlos_types::{
    CallId, CancellationScopeId, Generation, IdempotencyKey, OperationId, ReceiptId, TaskAttemptId,
    TaskId, TaskSnapshotId,
};
use rusqlite::Connection;

static NEXT: AtomicU64 = AtomicU64::new(1);

struct TestDatabase(PathBuf);

impl TestDatabase {
    fn new(label: &str) -> Self {
        Self(std::env::temp_dir().join(format!(
            "nlos-task-resource-prepare-{label}-{}-{}.sqlite3",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
    }

    fn open(&self) -> SqliteTaskAuthority {
        SqliteTaskAuthority::open(&self.0).expect("open task authority")
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", self.0.display(), suffix));
        }
    }
}

struct AuthorityRoot(PathBuf);

impl AuthorityRoot {
    fn new(label: &str) -> Self {
        Self(std::env::temp_dir().join(format!(
            "nlos-task-resource-prepare-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
    }
}

impl Drop for AuthorityRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn task_id() -> TaskId {
    TaskId::from_bytes([0x11; 16])
}

fn attempt_spec() -> AttemptSpec {
    AttemptSpec {
        task_id: task_id(),
        attempt_id: TaskAttemptId::from_bytes([0x21; 16]),
        attempt_generation: Generation::INITIAL,
        snapshot: SnapshotBundle {
            snapshot_id: TaskSnapshotId::from_bytes([0x22; 16]),
            snapshot_digest: [0x23; 32],
            expected_head_commit_seq: 0,
            effect_history_root: empty_effect_history_root(),
            retry_fence_epoch: 0,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes([0x24; 16]),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes([0x25; 16]),
        registered_at_ms: 1_010,
    }
}

fn finalize_request(
    permit_id: nlos_types::CommitPermitId,
    finalized_at_ms: i64,
) -> FinalizeRequestV3 {
    FinalizeRequestV3 {
        base: FinalizeRequest {
            task_id: task_id(),
            attempt_id: attempt_spec().attempt_id,
            attempt_generation: Generation::INITIAL,
            permit_id,
            new_effect_history_root: empty_effect_history_root(),
            new_retry_fence_epoch: 0,
            finalized_at_ms,
        },
        required_satisfaction: Vec::new(),
        fenced_participant_digest: [0x45; 32],
    }
}

fn prepare_request(permit_id: nlos_types::CommitPermitId) -> PrepareResourceFinalizeRequest {
    PrepareResourceFinalizeRequest {
        task_id: task_id(),
        attempt_id: attempt_spec().attempt_id,
        attempt_generation: Generation::INITIAL,
        permit_id,
        idempotency_key: IdempotencyKey::from_bytes([0x43; 16]),
        required_satisfaction: Vec::new(),
        fenced_participant_digest: [0x45; 32],
        prepared_at_ms: 1_450,
    }
}

/// One Resource owner fixture: driver, account, and reservation helpers.
struct OwnerFixture {
    authority: ResourceAuthority,
    driver: DriverRecord,
    account: AccountRecord,
}

impl OwnerFixture {
    fn new(root: &Path, seed: u8) -> Self {
        let authority = ResourceAuthority::open(root).expect("open resource authority");
        let driver = authority
            .register_driver(RegisterDriverRequest {
                profile_digest: [seed; 32],
                idempotency_key: IdempotencyKey::from_bytes([seed ^ 0x01; 16]),
                created_at_ms: 1_000,
            })
            .expect("register driver")
            .record();
        let account = authority
            .create_account(CreateAccountRequest {
                initial_credit: 1_000,
                idempotency_key: IdempotencyKey::from_bytes([seed ^ 0x02; 16]),
                created_at_ms: 1_000,
            })
            .expect("create account");
        Self {
            authority,
            driver,
            account,
        }
    }

    fn quote(&self, seed: u8, upper_bound: u64) -> QuoteRecord {
        self.authority
            .create_quote(CreateQuoteRequest {
                driver_id: self.driver.driver_id,
                driver_generation: self.driver.generation,
                driver_fencing_token: self.driver.fencing_token,
                operation_proposal_digest: [seed; 32],
                pricing_version: [seed ^ 0x11; 32],
                upper_bound,
                demand_capacity: ResourceDemand::default(),
                valid_until_ms: 9_000,
                idempotency_key: IdempotencyKey::from_bytes([seed ^ 0x12; 16]),
                created_at_ms: 1_000,
            })
            .expect("create quote")
            .record()
    }

    fn reserve(
        &self,
        quote: &QuoteRecord,
        call_id: CallId,
        operation_id: OperationId,
        key: IdempotencyKey,
    ) -> ReservationRecord {
        self.authority
            .reserve(ReserveRequest {
                account_id: self.account.account_id,
                quote_id: quote.quote_id,
                call_id,
                operation_id,
                idempotency_key: key,
                demand: ResourceDemand::default(),
                reserved_at_ms: 1_100,
            })
            .expect("reserve")
            .record()
    }

    /// Only activates the reservation (no finalization).
    fn activate_only(&self, reservation: &ReservationRecord) {
        self.authority
            .activate(nlos_resource::ActivateReservationRequest {
                reservation_id: reservation.reservation_id,
                call_id: reservation.call_id,
                operation_id: reservation.operation_id,
                driver_id: reservation.driver_id,
                driver_generation: reservation.driver_generation,
                driver_fencing_token: reservation.driver_fencing_token,
                activation_token: reservation.activation_token,
                activated_at_ms: 1_400,
            })
            .expect("activate");
    }

    /// Activates the reservation, records the ordered consumptions, and
    /// finalizes with the given final usage.
    fn settle(
        &self,
        reservation: &ReservationRecord,
        consumptions: &[(u64, u64)],
        final_usage: u64,
        seed: u8,
    ) {
        let activation = self
            .authority
            .activate(nlos_resource::ActivateReservationRequest {
                reservation_id: reservation.reservation_id,
                call_id: reservation.call_id,
                operation_id: reservation.operation_id,
                driver_id: reservation.driver_id,
                driver_generation: reservation.driver_generation,
                driver_fencing_token: reservation.driver_fencing_token,
                activation_token: reservation.activation_token,
                activated_at_ms: 1_400,
            })
            .expect("activate")
            .receipt();
        for (index, (sequence, cumulative_usage)) in consumptions.iter().enumerate() {
            self.authority
                .consume(nlos_resource::ConsumeReservationRequest {
                    reservation_id: reservation.reservation_id,
                    operation_id: reservation.operation_id,
                    activation_receipt_id: activation.receipt_id,
                    sequence: *sequence,
                    cumulative_usage: *cumulative_usage,
                    consumed_at_ms: 1_500 + 10 * (index as u64),
                })
                .expect("consume");
        }
        let final_seq = consumptions.last().map_or(0, |(sequence, _)| *sequence);
        self.authority
            .finalize_reservation(nlos_resource::FinalizeReservationRequest {
                reservation_id: reservation.reservation_id,
                operation_id: reservation.operation_id,
                activation_receipt_id: activation.receipt_id,
                effect_closed_proof_digest: [seed ^ 0x21; 32],
                final_seq,
                final_usage,
                finalized_at_ms: 1_600,
            })
            .expect("owner finalize");
    }
}

/// Two reservations: R1 uses 30 then 37 of 100 (refund 63), R2 uses 10 of
/// 25 (refund 15) — the shared bridge shape of `resource_commit.rs`.
fn two_reservations(owner: &OwnerFixture) -> Vec<ReservationRecord> {
    let quote_one = owner.quote(0xa2, 100);
    let quote_two = owner.quote(0xa3, 25);
    vec![
        owner.reserve(
            &quote_one,
            CallId::from_bytes([0xa5; 16]),
            OperationId::from_bytes([0xa6; 16]),
            IdempotencyKey::from_bytes([0xa7; 16]),
        ),
        owner.reserve(
            &quote_two,
            CallId::from_bytes([0xa9; 16]),
            OperationId::from_bytes([0xaa; 16]),
            IdempotencyKey::from_bytes([0xab; 16]),
        ),
    ]
}

fn settle_all(owner: &OwnerFixture, reservations: &[ReservationRecord]) {
    owner.settle(&reservations[0], &[(1, 30), (2, 37)], 37, 0xac);
    owner.settle(&reservations[1], &[(1, 10)], 10, 0xad);
}

/// Seals the reservation-bearing write set (no semantic appends) and
/// issues a plain permit. Returns the sealed write-set record too so tests
/// can compare the plan's durable Reservation-set facts against it.
#[allow(clippy::too_many_lines)] // One fixture builder mirrors the sibling bridge fixtures.
fn setup_task_and_permit(
    database: &TestDatabase,
    owner: &OwnerFixture,
    reservations: &[ReservationRecord],
) -> (
    SqliteTaskAuthority,
    PermitRecord,
    nlos_task::TaskWriteSetRecord,
) {
    let authority = database.open();
    authority
        .register_task(TaskSpec {
            application_id: None,
            plan_revision: None,
            task_id: task_id(),
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
        })
        .expect("register task");
    let spec = attempt_spec();
    authority
        .register_snapshot_receipt(TaskSnapshotReceiptSpec {
            task_id: task_id(),
            snapshot: spec.snapshot,
            receipt_id: ReceiptId::from_bytes([0x31; 16]),
            builder_id: [0x32; 16],
            builder_version_digest: [0x33; 32],
            per_authority_checkpoint_receipts: vec![ReceiptId::from_bytes([0x34; 16])],
            dependency_closure_root: [0x35; 32],
            semantic_resolver_digest: [0x36; 32],
            canonical_iteration_digest: [0x37; 32],
            achieved_consistency: SnapshotConsistency::Causal,
            built_at_ms: 1_005,
            authority_id: [0x38; 16],
            key_id: [0x39; 16],
            signature: [0x3a; 64],
        })
        .expect("snapshot receipt");
    authority
        .register_attempt_with_snapshot_receipt(spec, ReceiptId::from_bytes([0x31; 16]))
        .expect("register attempt");
    let registry = authority
        .inspect_participant_registry(task_id())
        .expect("registry");
    let first_binding = ParticipantRegistryBinding {
        generation: registry.generation,
        root: registry.root,
    };
    let driver_registration = authority
        .register_driver_gateway_participant(
            &owner.authority,
            task_id(),
            first_binding,
            owner.driver.driver_id,
            owner.driver.generation,
            1_150,
        )
        .expect("driver participant");
    let second_binding = ParticipantRegistryBinding {
        generation: driver_registration.registry().generation,
        root: driver_registration.registry().root,
    };
    authority
        .register_resource_ledger_participant(
            &owner.authority,
            task_id(),
            second_binding,
            owner.account.account_id,
            Generation::INITIAL,
            1_160,
        )
        .expect("ledger participant");
    let artifact_root = AuthorityRoot::new("artifact");
    let artifact = nlos_artifact::ArtifactStore::open(&artifact_root.0).expect("artifact store");
    let write_set = authority
        .seal_task_write_set_with_resource_authority(
            &artifact,
            &owner.authority,
            TaskWriteSetRequest {
                task_id: task_id(),
                attempt_id: spec.attempt_id,
                attempt_generation: spec.attempt_generation,
                artifact_reads: Vec::new(),
                artifact_writes: Vec::new(),
                process_binding: None,
                semantic_reads: Vec::new(),
                semantic_appends: Vec::new(),
                resource_reservations: reservations
                    .iter()
                    .map(|reservation| TaskWriteSetResourceReservationRequest {
                        reservation_id: reservation.reservation_id,
                        expected_call_id: reservation.call_id,
                        expected_operation_id: reservation.operation_id,
                        expected_quote_id: reservation.quote_id,
                    })
                    .collect(),
                planned_effects: Vec::new(),
                effect_endpoints: Vec::new(),
                idempotency_key: IdempotencyKey::from_bytes([0x41; 16]),
                sealed_at_ms: 1_200,
            },
        )
        .expect("seal write set")
        .record()
        .clone();
    let decision = authority
        .request_commit_permit_with_resource_authority(
            &owner.authority,
            PermitRequest {
                task_id: task_id(),
                attempt_id: spec.attempt_id,
                attempt_generation: spec.attempt_generation,
                write_set_root: write_set.write_set_root,
                planned_effects: Vec::new(),
                idempotency_key: IdempotencyKey::from_bytes([0x42; 16]),
                valid_until_ms: 9_000,
                requested_at_ms: 1_300,
            },
        )
        .expect("permit");
    let permit = match decision {
        PermitDecision::Issued(permit) => *permit,
        other => panic!("expected issued permit, got {other:?}"),
    };
    (authority, permit, write_set)
}

fn raw_count(database: &TestDatabase, sql: &str) -> i64 {
    let raw = Connection::open(&database.0).expect("raw task database");
    raw.query_row(sql, [], |row| row.get(0))
        .expect("count query")
}

fn nested_from_owner(owner: &nlos_resource::ResourceCostReceipt) -> NestedResourceCostReceipt {
    NestedResourceCostReceipt {
        reservation_id: owner.reservation_id,
        account_id: owner.account_id,
        quote_id: owner.quote_id,
        call_id: owner.call_id,
        operation_id: owner.operation_id,
        upper_bound: owner.upper_bound,
        activation: owner.activation,
        consumptions: owner.consumptions.clone(),
        finalization: owner.finalization,
    }
}

/// The exact nested aggregate set the two-reservation fixture must commit,
/// sorted by reservation id.
fn expected_nested(
    owner: &OwnerFixture,
    reservations: &[ReservationRecord],
) -> Vec<NestedResourceCostReceipt> {
    let mut nested = reservations
        .iter()
        .map(|reservation| {
            nested_from_owner(
                &owner
                    .authority
                    .inspect_cost_receipt(reservation.reservation_id)
                    .expect("owner aggregate"),
            )
        })
        .collect::<Vec<_>>();
    nested.sort_unstable_by_key(|record| record.reservation_id);
    nested
}

#[test]
fn prepare_persists_envelope_and_plan_and_exact_replays() {
    // Given a sealed, permitted two-reservation write set.
    let database = TestDatabase::new("prepare");
    let resource_root = AuthorityRoot::new("prepare-resource");
    let owner = OwnerFixture::new(&resource_root.0, 0xa1);
    let reservations = two_reservations(&owner);
    let (authority, permit, write_set) = setup_task_and_permit(&database, &owner, &reservations);

    // When the finalize envelope is prepared, the plan is durably Planned
    // with the derived Reservation set facts, and the exact retry replays.
    let decision = authority
        .prepare_resource_finalize(prepare_request(permit.permit_id))
        .expect("prepare envelope");
    let record = decision.record().clone();
    assert!(matches!(
        decision,
        ResourceFinalizeEnvelopeDecision::Prepared(_)
    ));
    let plan_id = record.plan_id;
    let plan = authority
        .inspect_resource_commit_plan(plan_id)
        .expect("inspect plan");
    assert_eq!(plan.state, ResourceCommitPlanState::Planned);
    assert_eq!(plan.permit_id, permit.permit_id);
    assert_eq!(plan.expected_reservation_count, 2);
    assert_eq!(plan.task_receipt_id, None);
    assert_eq!(plan.write_set_root, write_set.write_set_root);
    assert_eq!(
        plan.resource_reservation_set_root,
        write_set.resource_reservation_set_root
    );
    let pending = authority.list_incomplete_resource_commit_plans(8).unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].plan_id, plan_id);
    let envelope = authority
        .inspect_resource_finalize_envelope(plan_id)
        .expect("inspect envelope")
        .expect("envelope exists");
    assert_eq!(
        envelope.required_satisfaction,
        Vec::<nlos_task::RequiredSatisfaction>::new()
    );
    assert_eq!(envelope.fenced_participant_digest, [0x45; 32]);
    assert_eq!(envelope.prepared_at_ms, 1_450);
    let replay = authority
        .prepare_resource_finalize(prepare_request(permit.permit_id))
        .expect("exact replay");
    assert!(matches!(
        replay,
        ResourceFinalizeEnvelopeDecision::Replayed(_)
    ));
    assert_eq!(replay.record(), &record);

    // And conflicting envelope bytes fail closed with zero Task terminal
    // mutation and zero duplicate rows.
    let mut conflicting = prepare_request(permit.permit_id);
    conflicting.fenced_participant_digest = [0x99; 32];
    assert!(matches!(
        authority.prepare_resource_finalize(conflicting),
        Err(TaskStoreError::InvalidResourcePlan { .. })
    ));
    assert_eq!(
        raw_count(&database, "SELECT COUNT(*) FROM task_resource_commit_plans"),
        1
    );
    assert_eq!(
        raw_count(
            &database,
            "SELECT COUNT(*) FROM task_resource_finalize_envelopes"
        ),
        1
    );

    // And the envelope/satisfaction/plan rows are immutable evidence.
    let raw = Connection::open(&database.0).expect("raw task database");
    for sql in [
        "UPDATE task_resource_finalize_envelopes SET prepared_at_ms = 0",
        "DELETE FROM task_resource_finalize_envelopes",
    ] {
        assert!(raw.execute(sql, []).is_err(), "{sql} must be rejected");
    }
    assert!(
        raw.execute(
            "UPDATE task_resource_commit_plans SET expected_reservation_count = ?1",
            rusqlite::params![1_u64.to_be_bytes().as_slice()],
        )
        .is_err(),
        "plan identity columns are immutable"
    );
    assert!(
        raw.execute("DELETE FROM task_resource_commit_plans", [])
            .is_err()
    );
}

#[test]
fn prepare_fails_closed_for_non_resource_write_sets_and_non_issued_permits() {
    // Given a permit whose sealed write set is already terminal-closed by
    // a direct resource finalize without a plan.
    let database = TestDatabase::new("closed");
    let resource_root = AuthorityRoot::new("closed-resource");
    let owner = OwnerFixture::new(&resource_root.0, 0xb1);
    let reservations = two_reservations(&owner);
    let (authority, permit, _) = setup_task_and_permit(&database, &owner, &reservations);
    settle_all(&owner, &reservations);
    authority
        .finalize_commit_v3_with_resource_authority(
            &owner.authority,
            finalize_request(permit.permit_id, 1_700),
        )
        .expect("direct finalize");
    assert!(matches!(
        authority.prepare_resource_finalize(prepare_request(permit.permit_id)),
        Err(TaskStoreError::PermitNotIssued)
    ));
    assert_eq!(
        raw_count(&database, "SELECT COUNT(*) FROM task_resource_commit_plans"),
        0
    );

    // And a permit id that does not exist fails closed as not-found.
    let missing = nlos_types::CommitPermitId::from_bytes([0xee; 16]);
    assert!(matches!(
        authority.prepare_resource_finalize(prepare_request(missing)),
        Err(TaskStoreError::PermitNotFound)
    ));
}

#[test]
fn converge_pending_after_restart_reaches_unique_terminal_state_without_caller() {
    // Given a prepared envelope whose owner reservations are settled and
    // an authority handle dropped right after prepare (the caller
    // "disappears" — convergence may only use durable plan/envelope bytes
    // and owner reads).
    let database = TestDatabase::new("converge");
    let resource_root = AuthorityRoot::new("converge-resource");
    let owner = OwnerFixture::new(&resource_root.0, 0xc1);
    let reservations = two_reservations(&owner);
    let plan_id = {
        let (authority, permit, _) = setup_task_and_permit(&database, &owner, &reservations);
        let decision = authority
            .prepare_resource_finalize(prepare_request(permit.permit_id))
            .expect("prepare envelope");
        decision.record().plan_id
    };
    settle_all(&owner, &reservations);

    // When the authority restarts and the incomplete scan drives the plan.
    let restarted = database.open();
    let pending = restarted
        .list_incomplete_resource_commit_plans(8)
        .expect("scan");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].plan_id, plan_id);
    let decision = restarted
        .converge_resource_commit_plan(&owner.authority, plan_id, 1_800)
        .expect("converge");
    let ResourceConvergeDecision::Finalized(receipt) = decision else {
        panic!("expected finalized decision");
    };

    // Then the unique terminal state holds: plan FINALIZED bound to the
    // receipt, one nested row set equal to the owner aggregates, head
    // advanced, permit closed, and the scan no longer returns the plan.
    let plan = restarted
        .inspect_resource_commit_plan(plan_id)
        .expect("plan");
    assert_eq!(plan.state, ResourceCommitPlanState::Finalized);
    assert_eq!(plan.task_receipt_id, Some(receipt.task_receipt.receipt_id));
    assert_eq!(receipt.task_receipt.new_head_commit_seq, 1);
    assert_eq!(
        restarted
            .inspect_permit(task_id(), plan.permit_id)
            .expect("permit")
            .state,
        PermitState::Closed
    );
    let mut nested = receipt.resource_cost_receipts.clone();
    nested.sort_unstable_by_key(|record| record.reservation_id);
    assert_eq!(nested, expected_nested(&owner, &reservations));
    assert!(
        restarted
            .list_incomplete_resource_commit_plans(8)
            .unwrap()
            .is_empty()
    );

    // And the explicit replay is byte-equal with no duplicated rows.
    let replay = restarted
        .converge_resource_commit_plan(&owner.authority, plan_id, 1_999)
        .expect("replay converge");
    let ResourceConvergeDecision::Replayed(replayed) = replay else {
        panic!("expected replayed decision");
    };
    assert_eq!(replayed.task_receipt, receipt.task_receipt);
    assert_eq!(
        replayed.resource_cost_receipts,
        receipt.resource_cost_receipts
    );
    assert_eq!(
        raw_count(
            &database,
            "SELECT COUNT(*) FROM task_resource_cost_receipts"
        ),
        2
    );
    assert_eq!(
        raw_count(
            &database,
            "SELECT COUNT(*) FROM task_resource_cost_consumptions"
        ),
        3
    );
}

#[test]
fn converge_not_due_leaves_owner_untouched_and_records_nothing() {
    // Given a prepared envelope whose owner reservations are only ACTIVE.
    let database = TestDatabase::new("notdue-active");
    let resource_root = AuthorityRoot::new("notdue-active-resource");
    let owner = OwnerFixture::new(&resource_root.0, 0xd1);
    let reservations = two_reservations(&owner);
    let (authority, permit, _) = setup_task_and_permit(&database, &owner, &reservations);
    let plan_id = authority
        .prepare_resource_finalize(prepare_request(permit.permit_id))
        .expect("prepare envelope")
        .record()
        .plan_id;
    owner.activate_only(&reservations[0]);
    owner.activate_only(&reservations[1]);
    let reservations_before: Vec<ReservationRecord> = reservations
        .iter()
        .map(|reservation| {
            owner
                .authority
                .inspect_reservation(reservation.reservation_id)
                .expect("reservation before")
        })
        .collect();
    let account_before = owner
        .authority
        .inspect_account(owner.account.account_id)
        .expect("account before");

    // When converge runs, the typed not-due decision makes zero owner
    // mutation and records zero ledger failure.
    for _ in 0..3 {
        let decision = authority
            .converge_resource_commit_plan(&owner.authority, plan_id, 1_800)
            .expect("converge not due");
        assert!(matches!(decision, ResourceConvergeDecision::NotDue(_)));
    }
    let reservations_after: Vec<ReservationRecord> = reservations
        .iter()
        .map(|reservation| {
            owner
                .authority
                .inspect_reservation(reservation.reservation_id)
                .expect("reservation after")
        })
        .collect();
    assert_eq!(reservations_after, reservations_before);
    assert_eq!(
        owner
            .authority
            .inspect_account(owner.account.account_id)
            .expect("account after"),
        account_before
    );
    assert!(
        reservations_after
            .iter()
            .all(|record| record.state == ReservationState::Active)
    );
    assert_eq!(
        raw_count(&database, "SELECT COUNT(*) FROM task_resource_recovery"),
        0
    );
    assert_eq!(
        raw_count(&database, "SELECT COUNT(*) FROM task_receipts"),
        0
    );
    let plan = authority
        .inspect_resource_commit_plan(plan_id)
        .expect("plan");
    assert_eq!(plan.state, ResourceCommitPlanState::Planned);
    assert_eq!(plan.task_receipt_id, None);

    // And the stuck plan remains an inspectable durable fact that keeps
    // re-entering the incomplete scan.
    assert_eq!(
        authority
            .list_incomplete_resource_commit_plans(8)
            .unwrap()
            .len(),
        1
    );
    assert!(
        authority
            .inspect_resource_finalize_envelope(plan_id)
            .unwrap()
            .is_some()
    );

    // And an owner that never saw the reservation (not settled on this
    // owner) is also not-due instead of an authority failure.
    let empty_root = AuthorityRoot::new("notdue-empty-resource");
    let empty_owner = OwnerFixture::new(&empty_root.0, 0xe1);
    assert!(matches!(
        authority.converge_resource_commit_plan(&empty_owner.authority, plan_id, 1_850),
        Ok(ResourceConvergeDecision::NotDue(_))
    ));
}

#[test]
fn converge_flips_plan_when_permit_was_finalized_directly() {
    // Given a prepared envelope whose permit was terminalized through the
    // direct resource-aware v3 API (no plan threading): the plan stays
    // Planned until converge replays the terminal state.
    let database = TestDatabase::new("direct");
    let resource_root = AuthorityRoot::new("direct-resource");
    let owner = OwnerFixture::new(&resource_root.0, 0xf1);
    let reservations = two_reservations(&owner);
    let (authority, permit, _) = setup_task_and_permit(&database, &owner, &reservations);
    let plan_id = authority
        .prepare_resource_finalize(prepare_request(permit.permit_id))
        .expect("prepare envelope")
        .record()
        .plan_id;
    settle_all(&owner, &reservations);
    let direct = authority
        .finalize_commit_v3_with_resource_authority(
            &owner.authority,
            finalize_request(permit.permit_id, 1_700),
        )
        .expect("direct finalize");
    let ResourceFinalizeDecision::Committed(direct_receipt) = direct else {
        panic!("expected committed receipt");
    };
    assert_eq!(
        authority
            .inspect_resource_commit_plan(plan_id)
            .unwrap()
            .state,
        ResourceCommitPlanState::Planned
    );

    // When converge runs against the closed permit, the plan flips to
    // FINALIZED bound to the existing receipt and nothing duplicates.
    let decision = authority
        .converge_resource_commit_plan(&owner.authority, plan_id, 1_900)
        .expect("converge replay");
    let ResourceConvergeDecision::Replayed(replayed) = decision else {
        panic!("expected replayed decision");
    };
    assert_eq!(replayed.task_receipt, direct_receipt.task_receipt);
    let plan = authority.inspect_resource_commit_plan(plan_id).unwrap();
    assert_eq!(plan.state, ResourceCommitPlanState::Finalized);
    assert_eq!(
        plan.task_receipt_id,
        Some(direct_receipt.task_receipt.receipt_id)
    );
    assert_eq!(
        raw_count(
            &database,
            "SELECT COUNT(*) FROM task_resource_cost_receipts"
        ),
        2
    );

    // And a second converge stays idempotent.
    assert!(matches!(
        authority.converge_resource_commit_plan(&owner.authority, plan_id, 1_950),
        Ok(ResourceConvergeDecision::Replayed(_))
    ));
    assert_eq!(
        raw_count(
            &database,
            "SELECT COUNT(*) FROM task_resource_cost_receipts"
        ),
        2
    );
}
