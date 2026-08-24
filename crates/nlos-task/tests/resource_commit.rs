//! Resource-aware v3 finalize: owner-derived cost-receipt bridge tests
//! (Attempt TASK-RESOURCE-COMMIT-01).
//!
//! Every test follows reserve → sealed `TaskWriteSet` → permit → activate →
//! consume → owner finalize → `finalize_commit_v3_with_resource_authority`.
//! The caller never supplies activation/finalization IDs, consumption
//! sequences, usage, or refund facts; the Task authority derives the exact
//! sealed Reservation set and re-reads the FINALIZED owner aggregate.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_resource::{
    AccountRecord, CreateAccountRequest, CreateQuoteRequest, DriverRecord, FinalizationReceipt,
    QuoteRecord, RegisterDriverRequest, ReservationRecord, ReserveRequest, ResourceAuthority,
};
use nlos_task::{
    AttemptSpec, AuthorityLeasePermitRequest, AuthorityLeaseRequest, FinalizeRequest,
    FinalizeRequestV3, NestedResourceCostReceipt, ParticipantRegistryBinding, PermitDecision,
    PermitRecord, PermitRequest, PermitState, ResourceFinalizeDecision, SnapshotBundle,
    SnapshotConsistency, SqliteTaskAuthority, TaskSnapshotReceiptSpec, TaskSpec, TaskStoreError,
    TaskWriteSetRequest, TaskWriteSetResourceReservationRequest, empty_effect_history_root,
};
use nlos_types::{
    CallId, CancellationScopeId, Generation, IdempotencyKey, OperationId, ProcessId, ReceiptId,
    TaskAttemptId, TaskId, TaskSnapshotId,
};
use rusqlite::Connection;

static NEXT: AtomicU64 = AtomicU64::new(1);

struct TestDatabase(PathBuf);

impl TestDatabase {
    fn new(label: &str) -> Self {
        Self(std::env::temp_dir().join(format!(
            "nlos-task-resource-commit-{label}-{}-{}.sqlite3",
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
            "nlos-task-resource-commit-{label}-{}-{}",
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
        fenced_participant_digest: [0; 32],
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
                reserved_at_ms: 1_100,
            })
            .expect("reserve")
            .record()
    }

    /// Only activates the reservation (no consumption, no finalization).
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
    /// finalizes with the given final usage. Returns the owner
    /// finalization receipt.
    fn settle(
        &self,
        reservation: &ReservationRecord,
        consumptions: &[(u64, u64)],
        final_usage: u64,
        seed: u8,
    ) -> FinalizationReceipt {
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
            .expect("owner finalize")
            .receipt()
    }
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

#[derive(Clone, Copy)]
enum PermitPath {
    Plain,
    AuthorityLease,
}

/// Registers the task, registers the Driver/Ledger endpoint participants,
/// seals the reservation-bearing write set, and issues the permit. The
/// `AuthorityLease` path binds the permit to a freshly acquired lease.
#[allow(clippy::too_many_lines)]
fn setup_task_and_permit(
    database: &TestDatabase,
    owner: &OwnerFixture,
    reservations: &[ReservationRecord],
    path: PermitPath,
) -> (
    SqliteTaskAuthority,
    PermitRecord,
    Option<nlos_task::AuthorityLeaseRecord>,
) {
    let authority = database.open();
    authority
        .register_task(TaskSpec {
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
    let permit_request = PermitRequest {
        task_id: task_id(),
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        write_set_root: write_set.write_set_root,
        planned_effects: Vec::new(),
        idempotency_key: IdempotencyKey::from_bytes([0x42; 16]),
        valid_until_ms: 9_000,
        requested_at_ms: 1_300,
    };
    let lease = if matches!(path, PermitPath::AuthorityLease) {
        Some(
            authority
                .acquire_authority_lease(AuthorityLeaseRequest {
                    holder_id: ProcessId::from_bytes([0x01; 16]),
                    idempotency_key: IdempotencyKey::from_bytes([0x44; 16]),
                    requested_at_ms: 1_250,
                    ttl_ms: 9_000,
                })
                .expect("lease")
                .record(),
        )
    } else {
        None
    };
    let decision = match path {
        PermitPath::Plain => authority
            .request_commit_permit_with_resource_authority(&owner.authority, permit_request)
            .expect("permit"),
        PermitPath::AuthorityLease => authority
            .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
                permit: permit_request,
                lease: lease.expect("lease path"),
            })
            .expect("lease permit"),
    };
    let permit = match decision {
        PermitDecision::Issued(permit) => *permit,
        other => panic!("expected issued permit, got {other:?}"),
    };
    (authority, permit, lease)
}

fn raw_count(database: &TestDatabase, sql: &str) -> i64 {
    let raw = Connection::open(&database.0).expect("raw task database");
    raw.query_row(sql, [], |row| row.get(0))
        .expect("count query")
}

#[test]
fn resource_aware_finalize_commits_full_owner_aggregate_for_multiple_consumptions() {
    // Given two sealed reservations whose owner aggregates carry multiple
    // ordered consumptions (R1: 30 then 37 of 100; R2: 10 of 25), both
    // settled on the owner before Task finalization.
    let database = TestDatabase::new("happy");
    let resource_root = AuthorityRoot::new("happy-resource");
    let owner = OwnerFixture::new(&resource_root.0, 0xa1);
    let quote_one = owner.quote(0xa2, 100);
    let quote_two = owner.quote(0xa3, 25);
    let reservation_one = owner.reserve(
        &quote_one,
        CallId::from_bytes([0xa5; 16]),
        OperationId::from_bytes([0xa6; 16]),
        IdempotencyKey::from_bytes([0xa7; 16]),
    );
    let reservation_two = owner.reserve(
        &quote_two,
        CallId::from_bytes([0xa9; 16]),
        OperationId::from_bytes([0xaa; 16]),
        IdempotencyKey::from_bytes([0xab; 16]),
    );
    let (authority, permit, _) = setup_task_and_permit(
        &database,
        &owner,
        &[reservation_one, reservation_two],
        PermitPath::Plain,
    );
    owner.settle(&reservation_one, &[(1, 30), (2, 37)], 37, 0xac);
    owner.settle(&reservation_two, &[(1, 10)], 10, 0xad);

    // When the resource-aware v3 finalize runs against the owner.
    let decision = authority
        .finalize_commit_v3_with_resource_authority(
            &owner.authority,
            finalize_request(permit.permit_id, 1_700),
        )
        .expect("resource finalize");

    // Then the commit nests the exact owner aggregates, preserves
    // conservation, and advances the Task head exactly once.
    let ResourceFinalizeDecision::Committed(receipt) = decision else {
        panic!("expected committed decision");
    };
    assert_eq!(receipt.task_receipt.new_head_commit_seq, 1);
    assert_eq!(
        authority
            .inspect_permit(task_id(), permit.permit_id)
            .expect("permit")
            .state,
        PermitState::Closed
    );
    let mut nested = receipt.resource_cost_receipts.clone();
    nested.sort_unstable_by_key(|record| record.reservation_id);
    assert_eq!(nested.len(), 2);
    for (record, reservation) in [
        (&nested[0], &reservation_one),
        (&nested[1], &reservation_two),
    ] {
        let aggregate = owner
            .authority
            .inspect_cost_receipt(reservation.reservation_id)
            .expect("owner aggregate");
        assert_eq!(record, &nested_from_owner(&aggregate));
        assert_eq!(
            record.upper_bound - record.finalization.final_usage,
            record.finalization.refund_credit
        );
        assert_eq!(
            record.consumptions.last().map(|last| last.sequence),
            Some(record.finalization.high_water_seq)
        );
    }
    assert_eq!(nested[0].finalization.final_usage, 37);
    assert_eq!(nested[0].finalization.refund_credit, 63);
    assert_eq!(nested[0].consumptions.len(), 2);
    assert_eq!(nested[1].finalization.final_usage, 10);
    assert_eq!(nested[1].finalization.refund_credit, 15);
    assert_eq!(nested[1].consumptions.len(), 1);
    assert_eq!(
        authority
            .inspect_resource_cost_receipts(task_id(), receipt.task_receipt.receipt_id)
            .expect("nested readback"),
        nested
    );

    // And both nested tables reject mutation.
    let raw = Connection::open(&database.0).expect("raw task database");
    assert!(
        raw.execute(
            "UPDATE task_resource_cost_receipts SET refund_credit = refund_credit",
            []
        )
        .is_err()
    );
    assert!(
        raw.execute("DELETE FROM task_resource_cost_receipts", [])
            .is_err()
    );
    assert!(
        raw.execute(
            "UPDATE task_resource_cost_consumptions SET cumulative_usage = cumulative_usage",
            [],
        )
        .is_err()
    );
    assert!(
        raw.execute("DELETE FROM task_resource_cost_consumptions", [])
            .is_err()
    );
}

#[test]
fn zero_consumption_closure_commits_with_and_without_final_usage_and_lease_parity() {
    // Given a permit bound to an authority lease over two sealed
    // reservations that were owner-finalized with zero consumptions: R1
    // refunds the whole bound (final usage 0), R2 settles a legal nonzero
    // final usage (5 of 40).
    let database = TestDatabase::new("zero");
    let resource_root = AuthorityRoot::new("zero-resource");
    let owner = OwnerFixture::new(&resource_root.0, 0xb1);
    let quote_one = owner.quote(0xb2, 50);
    let quote_two = owner.quote(0xb3, 40);
    let reservation_one = owner.reserve(
        &quote_one,
        CallId::from_bytes([0xb5; 16]),
        OperationId::from_bytes([0xb6; 16]),
        IdempotencyKey::from_bytes([0xb7; 16]),
    );
    let reservation_two = owner.reserve(
        &quote_two,
        CallId::from_bytes([0xb9; 16]),
        OperationId::from_bytes([0xba; 16]),
        IdempotencyKey::from_bytes([0xbb; 16]),
    );
    let (authority, permit, lease) = setup_task_and_permit(
        &database,
        &owner,
        &[reservation_one, reservation_two],
        PermitPath::AuthorityLease,
    );
    let lease = lease.expect("lease path");
    owner.settle(&reservation_one, &[], 0, 0xbd);
    owner.settle(&reservation_two, &[], 5, 0xbe);

    // When the resource finalize omits the lease the typed refusal leaves
    // the permit issued; with the lease the commit carries both zero
    // consumption aggregates.
    let request = finalize_request(permit.permit_id, 1_700);
    assert!(matches!(
        authority.finalize_commit_v3_with_resource_authority(&owner.authority, request.clone()),
        Err(TaskStoreError::AuthorityLeaseRequired)
    ));
    assert_eq!(
        authority
            .inspect_permit(task_id(), permit.permit_id)
            .expect("permit")
            .state,
        PermitState::Issued
    );
    let decision = authority
        .finalize_commit_v3_with_resource_authority_and_authority_lease(
            &owner.authority,
            request,
            lease,
        )
        .expect("resource finalize with lease");
    let ResourceFinalizeDecision::Committed(receipt) = decision else {
        panic!("expected committed decision");
    };
    let mut nested = receipt.resource_cost_receipts.clone();
    nested.sort_unstable_by_key(|record| record.reservation_id);
    assert_eq!(nested.len(), 2);
    assert!(nested.iter().all(|record| record.consumptions.is_empty()));
    let by_zero_usage = nested
        .iter()
        .find(|record| record.finalization.final_usage == 0)
        .expect("zero usage receipt");
    assert_eq!(by_zero_usage.upper_bound, 50);
    assert_eq!(by_zero_usage.finalization.refund_credit, 50);
    assert_eq!(
        by_zero_usage.finalization.high_water_seq,
        by_zero_usage.finalization.high_water
    );
    let nonzero = nested
        .iter()
        .find(|record| record.finalization.final_usage == 5)
        .expect("nonzero usage receipt");
    assert_eq!(nonzero.upper_bound, 40);
    assert_eq!(nonzero.finalization.refund_credit, 35);
    assert_eq!(
        raw_count(
            &database,
            "SELECT COUNT(*) FROM task_resource_cost_consumptions",
        ),
        0
    );
}

#[test]
fn non_finalized_owner_reservation_fails_closed_without_task_mutation() {
    // Given a sealed, permitted reservation that was activated but never
    // finalized on the owner.
    let database = TestDatabase::new("active");
    let resource_root = AuthorityRoot::new("active-resource");
    let owner = OwnerFixture::new(&resource_root.0, 0xc1);
    let quote = owner.quote(0xc2, 100);
    let reservation = owner.reserve(
        &quote,
        CallId::from_bytes([0xc4; 16]),
        OperationId::from_bytes([0xc5; 16]),
        IdempotencyKey::from_bytes([0xc6; 16]),
    );
    let (authority, permit, _) =
        setup_task_and_permit(&database, &owner, &[reservation], PermitPath::Plain);
    owner.activate_only(&reservation);

    // When the resource finalize re-reads the owner, the typed Resource
    // authority error fails closed.
    assert!(matches!(
        authority.finalize_commit_v3_with_resource_authority(
            &owner.authority,
            finalize_request(permit.permit_id, 1_700),
        ),
        Err(TaskStoreError::ResourceParticipantAuthority(_))
    ));

    // Then the permit stays issued and no Task terminal row exists.
    assert_eq!(
        authority
            .inspect_permit(task_id(), permit.permit_id)
            .expect("permit")
            .state,
        PermitState::Issued
    );
    assert_eq!(
        raw_count(&database, "SELECT COUNT(*) FROM task_receipts"),
        0
    );
    assert_eq!(
        raw_count(
            &database,
            "SELECT COUNT(*) FROM task_resource_cost_receipts"
        ),
        0
    );
}

#[test]
fn sealed_binding_mismatch_fails_closed_without_terminal_mutation() {
    // Given a permit sealed over reservation R (authority A), and a second
    // authority B holding a FINALIZED reservation with the same identity
    // but a different account/quote binding.
    let database = TestDatabase::new("mismatch");
    let resource_root = AuthorityRoot::new("mismatch-resource");
    let owner = OwnerFixture::new(&resource_root.0, 0xd1);
    let quote = owner.quote(0xd2, 100);
    let shared_key = IdempotencyKey::from_bytes([0xd3; 16]);
    let shared_operation = OperationId::from_bytes([0xd4; 16]);
    let reservation = owner.reserve(
        &quote,
        CallId::from_bytes([0xd6; 16]),
        shared_operation,
        shared_key,
    );
    let (authority, permit, _) =
        setup_task_and_permit(&database, &owner, &[reservation], PermitPath::Plain);

    let other_root = AuthorityRoot::new("mismatch-other");
    let other = OwnerFixture::new(&other_root.0, 0xe1);
    let other_quote = other.quote(0xe2, 80);
    let other_reservation = other.reserve(
        &other_quote,
        CallId::from_bytes([0xe4; 16]),
        shared_operation,
        shared_key,
    );
    assert_eq!(other_reservation.reservation_id, reservation.reservation_id);
    other.settle(&other_reservation, &[(1, 20)], 20, 0xe5);

    // When the finalize is driven by authority B, the sealed binding
    // mismatch fails closed before any Task mutation.
    assert!(matches!(
        authority.finalize_commit_v3_with_resource_authority(
            &other.authority,
            finalize_request(permit.permit_id, 1_700),
        ),
        Err(TaskStoreError::TaskWriteSetResourceReservationConflict)
    ));

    // Then the permit stays issued and no terminal rows were written.
    assert_eq!(
        authority
            .inspect_permit(task_id(), permit.permit_id)
            .expect("permit")
            .state,
        PermitState::Issued
    );
    assert_eq!(
        raw_count(&database, "SELECT COUNT(*) FROM task_receipts"),
        0
    );
    assert_eq!(
        raw_count(
            &database,
            "SELECT COUNT(*) FROM task_resource_cost_receipts"
        ),
        0
    );
}

#[test]
fn replay_after_restart_reads_only_task_rows_and_duplicates_nothing() {
    // Given a committed resource-aware finalize for one reservation with
    // two ordered consumptions.
    let database = TestDatabase::new("replay");
    let resource_root = AuthorityRoot::new("replay-resource");
    let owner = OwnerFixture::new(&resource_root.0, 0xf1);
    let quote = owner.quote(0xf2, 100);
    let reservation = owner.reserve(
        &quote,
        CallId::from_bytes([0xf4; 16]),
        OperationId::from_bytes([0xf5; 16]),
        IdempotencyKey::from_bytes([0xf6; 16]),
    );
    let committed = {
        let (authority, permit, _) =
            setup_task_and_permit(&database, &owner, &[reservation], PermitPath::Plain);
        owner.settle(&reservation, &[(1, 30), (2, 37)], 37, 0xf7);
        let decision = authority
            .finalize_commit_v3_with_resource_authority(
                &owner.authority,
                finalize_request(permit.permit_id, 1_700),
            )
            .expect("resource finalize");
        let ResourceFinalizeDecision::Committed(receipt) = decision else {
            panic!("expected committed decision");
        };
        *receipt
    };
    drop(owner);
    drop(resource_root);

    // When the Task authority is reopened and the same finalize is replayed
    // against a fresh, unrelated (empty) Resource authority, the replay
    // must succeed byte-equal without reading the owner.
    let reopened = database.open();
    let empty_root = AuthorityRoot::new("replay-empty-resource");
    let empty_owner = OwnerFixture::new(&empty_root.0, 0xf8);
    let mut replay_request =
        finalize_request(committed.task_receipt.permit_id.expect("permit"), 1_999);
    replay_request.base.finalized_at_ms = 9_999;
    let replay = reopened
        .finalize_commit_v3_with_resource_authority(&empty_owner.authority, replay_request)
        .expect("replay");
    assert!(matches!(replay, ResourceFinalizeDecision::Replayed(_)));
    assert_eq!(replay.receipt(), &committed);

    // And no duplicate nested rows were appended.
    assert_eq!(
        raw_count(
            &database,
            "SELECT COUNT(*) FROM task_resource_cost_receipts",
        ),
        1
    );
    assert_eq!(
        raw_count(
            &database,
            "SELECT COUNT(*) FROM task_resource_cost_consumptions",
        ),
        2
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn v38_database_migrates_to_v39_and_preserves_legacy_receipts() {
    // Given a v38-shaped database whose only Task receipt predates v39.
    let database = TestDatabase::new("migration");
    let legacy_receipt = {
        let authority = database.open();
        authority
            .register_task(TaskSpec {
                task_id: task_id(),
                task_generation: Generation::INITIAL,
                registered_at_ms: 1_000,
            })
            .expect("register task");
        authority
            .register_attempt(attempt_spec())
            .expect("register attempt");
        let permit = match authority
            .request_commit_permit(PermitRequest {
                task_id: task_id(),
                attempt_id: attempt_spec().attempt_id,
                attempt_generation: Generation::INITIAL,
                write_set_root: [0; 32],
                planned_effects: Vec::new(),
                idempotency_key: IdempotencyKey::from_bytes([0x51; 16]),
                valid_until_ms: 9_000,
                requested_at_ms: 1_300,
            })
            .expect("legacy permit")
        {
            PermitDecision::Issued(permit) => permit,
            other => panic!("expected issued permit, got {other:?}"),
        };
        match authority
            .finalize_commit(FinalizeRequest {
                task_id: task_id(),
                attempt_id: attempt_spec().attempt_id,
                attempt_generation: Generation::INITIAL,
                permit_id: permit.permit_id,
                new_effect_history_root: empty_effect_history_root(),
                new_retry_fence_epoch: 0,
                finalized_at_ms: 1_400,
            })
            .expect("legacy finalize")
        {
            nlos_task::FinalizeDecision::Committed(receipt) => *receipt,
            other @ nlos_task::FinalizeDecision::Replayed(_) => {
                panic!("expected committed receipt, got {other:?}")
            }
        }
    };
    drop(database.open());
    let raw = Connection::open(&database.0).expect("raw task database");
    raw.execute_batch(
        "DROP TABLE task_resource_cost_consumptions;
         DROP TABLE task_resource_cost_receipts;
         PRAGMA user_version = 38;",
    )
    .expect("construct v38 schema");

    // When the authority reopens, the migration is additive and idempotent.
    drop(raw);
    let authority = database.open();
    let raw = Connection::open(&database.0).expect("migrated database");
    let version: i64 = raw
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .expect("schema version");
    assert_eq!(version, 40);

    // Then the prior Task receipt stays readable with an empty nested set.
    assert_eq!(
        authority
            .inspect_receipt(task_id(), legacy_receipt.receipt_id)
            .expect("legacy receipt"),
        legacy_receipt
    );
    assert_eq!(
        authority
            .inspect_resource_cost_receipts(task_id(), legacy_receipt.receipt_id)
            .expect("nested legacy readback"),
        Vec::new()
    );

    // And the migrated nested tables remain immutable evidence.
    let receipt_blob = legacy_receipt.receipt_id.as_bytes().as_slice().to_vec();
    raw.execute(
        "INSERT INTO task_resource_cost_receipts (
             task_receipt_id, task_id, reservation_id, account_id, quote_id,
             call_id, operation_id, upper_bound, activation_receipt_id,
             activated_at_ms, finalization_receipt_id, effect_closed_proof_digest,
             high_water_seq, final_seq, high_water, final_usage,
             refund_credit, finalized_at_ms
         ) VALUES (?1, ?2, ?3, ?3, ?3, ?3, ?3, ?4, ?3, ?4, ?3, ?5, ?4, ?4, ?4, ?4, ?4, ?4)",
        rusqlite::params![
            receipt_blob,
            task_id().as_bytes().as_slice(),
            [0x61u8; 16].as_slice(),
            100_u64.to_be_bytes().as_slice(),
            [0x62u8; 32].as_slice(),
        ],
    )
    .expect("seed nested parent row");
    assert!(
        raw.execute(
            "UPDATE task_resource_cost_receipts SET refund_credit = ?1",
            rusqlite::params![0_u64.to_be_bytes().as_slice()],
        )
        .is_err()
    );
    assert!(
        raw.execute("DELETE FROM task_resource_cost_receipts", [])
            .is_err()
    );
    raw.execute(
        "INSERT INTO task_resource_cost_consumptions (
             task_receipt_id, reservation_id, sequence, receipt_id,
             cumulative_usage, consumed_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            receipt_blob,
            [0x61u8; 16].as_slice(),
            1_u64.to_be_bytes().as_slice(),
            [0x63u8; 16].as_slice(),
            10_u64.to_be_bytes().as_slice(),
            1_500_u64.to_be_bytes().as_slice(),
        ],
    )
    .expect("seed nested child row");
    assert!(
        raw.execute(
            "UPDATE task_resource_cost_consumptions SET cumulative_usage = ?1",
            rusqlite::params![11_u64.to_be_bytes().as_slice()],
        )
        .is_err()
    );
    assert!(
        raw.execute("DELETE FROM task_resource_cost_consumptions", [])
            .is_err()
    );
}
