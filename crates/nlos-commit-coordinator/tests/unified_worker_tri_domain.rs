#![allow(deprecated)] // Ladder constructors deprecated in favor of the *_with_authorities_struct entries.
//! W28-C-3 tri-domain worker coverage (ADR-0017 appendix A, gates G3/G6):
//! the commit recovery worker's third domain half (`resource_cycle`) scans
//! due Resource finalize plans and converges them through the nlos-task
//! `converge_resource_commit_plan` API. Covered here: no-caller convergence
//! from durable plan/envelope bytes plus owner reads alone (G3 worker
//! half), the not-due boundary making zero ledger writes through the
//! worker (G4 worker half), one worker converging artifact, semantic, and
//! resource domains in the same cycle, and per-domain fault isolation when
//! the resource half burns its infrastructure budget (G6). A worker
//! started through the older seams supplies no resource authority, so its
//! resource-domain projection stays quiescent by construction.
//!
//! This is local single-process recovery evidence, not a distributed
//! atomic-commit claim: no cross-authority atomicity is asserted.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use nlos_artifact::{
    ArtifactStore, ContentDigest, CreateArtifactSpec, StageRevisionRequest, staging_id_for,
};
use nlos_commit_coordinator::{
    RecoveryWorkerConfig, RecoveryWorkerHealth, RecoveryWorkerState,
    TaskAuthorityCommitRecoveryWorker,
};
use nlos_resource::{
    AccountRecord, CreateAccountRequest, CreateQuoteRequest, DriverRecord, QuoteRecord,
    RegisterDriverRequest, ReservationRecord, ReservationState, ReserveRequest, ResourceAuthority,
    ResourceDemand,
};
use nlos_semantic::SemanticAuthority;
use nlos_task::{
    ArtifactCommitPlanState, ArtifactPublicationExpectation, AttemptSpec,
    NestedResourceCostReceipt, ParticipantRegistryBinding, PermitDecision, PermitRequest,
    PlanArtifactCommitRequest, PlanSemanticCommitRequest, PrepareResourceFinalizeRequest,
    ResourceCommitPlanId, ResourceCommitPlanState, ResourceConvergeDecision, SemanticCommitPlanId,
    SemanticCommitPlanState, SnapshotBundle, SnapshotConsistency, SqliteTaskAuthority,
    TaskSnapshotReceiptSpec, TaskSpec, TaskWriteSetRequest, TaskWriteSetResourceReservationRequest,
    TaskWriteSetSemanticAppendRequest, TaskWriteSetSemanticRequiredDurability,
    TaskWriteSetSemanticTarget, artifact_publication_plan_root, empty_effect_history_root,
};
use nlos_types::{
    ArtifactId, CallId, CancellationScopeId, Generation, IdempotencyKey, NamespaceId, OperationId,
    ReceiptId, SemanticEventId, TaskAttemptId, TaskId, TaskSnapshotId,
};
use rusqlite::Connection;

static NEXT_DATABASE: AtomicU64 = AtomicU64::new(0);
const EXPLICIT_CONVERGE_MS: i64 = 4_102_444_800_000; // 2100-01-01: above any wall clock this test can observe.

fn fast_config() -> RecoveryWorkerConfig {
    RecoveryWorkerConfig {
        scan_limit: 16,
        poll_interval: Duration::from_millis(10),
        max_backoff: Duration::from_millis(40),
        failure_threshold: 3,
    }
}

fn wait_until(predicate: impl FnMut() -> bool) {
    wait_until_within(predicate, Duration::from_secs(10));
}

fn wait_until_within(mut predicate: impl FnMut() -> bool, budget: Duration) {
    let deadline = Instant::now() + budget;
    while !predicate() {
        assert!(Instant::now() < deadline, "condition did not become true");
        std::thread::sleep(Duration::from_millis(5));
    }
}

struct TestAuthorities {
    root: PathBuf,
    task_path: PathBuf,
    artifact_root: PathBuf,
    semantic_root: PathBuf,
    resource_root: PathBuf,
}

impl TestAuthorities {
    fn new(name: &str) -> Self {
        let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "nlos-commit-coordinator-{name}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        Self {
            task_path: root.join("tasks.sqlite3"),
            artifact_root: root.join("artifact-store"),
            semantic_root: root.join("semantic-authority"),
            resource_root: root.join("resource-authority"),
            root,
        }
    }

    fn open(&self) -> (SqliteTaskAuthority, ArtifactStore) {
        (
            SqliteTaskAuthority::open(&self.task_path).unwrap(),
            ArtifactStore::open(&self.artifact_root).unwrap(),
        )
    }

    fn open_resource(&self) -> ResourceAuthority {
        ResourceAuthority::open(&self.resource_root).unwrap()
    }
}

impl Drop for TestAuthorities {
    fn drop(&mut self) {
        match fs::remove_dir_all(&self.root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("remove test authorities: {error}"),
        }
    }
}

fn execute_sql(path: &Path, sql: &str) {
    Connection::open(path).unwrap().execute_batch(sql).unwrap();
}

fn raw_count(path: &Path, sql: &str) -> i64 {
    Connection::open(path)
        .unwrap()
        .query_row(sql, [], |row| row.get(0))
        .unwrap()
}

// ---------------------------------------------------------------------
// Artifact-domain fixture (copied from unified_worker_dual_domain.rs so
// the three domains coexist in one task authority).
// ---------------------------------------------------------------------

fn create_artifact(store: &ArtifactStore, artifact_id: ArtifactId, seed: u8) {
    store
        .create_artifact(CreateArtifactSpec {
            artifact_id,
            idempotency_key: IdempotencyKey::from_bytes([seed; 16]),
            content_type: "application/octet-stream".to_string(),
            application_id: None,
            owner: None,
            created_at_ms: 1_000,
        })
        .unwrap();
}

struct PreparedSingle {
    plan: nlos_task::ArtifactCommitPlanId,
    task: TaskId,
}

fn prepare_single(databases: &TestAuthorities, seed: u8) -> PreparedSingle {
    let (tasks, artifacts) = databases.open();
    let task_id = TaskId::from_bytes([seed; 16]);
    let artifact_id = ArtifactId::from_bytes([seed.wrapping_add(1); 16]);
    let stage_key = IdempotencyKey::from_bytes([seed.wrapping_add(2); 16]);
    let bytes = [seed.wrapping_add(3); 32];
    create_artifact(&artifacts, artifact_id, seed.wrapping_add(4));
    tasks
        .register_task(TaskSpec {
            task_id,
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
        })
        .unwrap();
    let attempt = AttemptSpec {
        task_id,
        attempt_id: TaskAttemptId::from_bytes([seed.wrapping_add(5); 16]),
        attempt_generation: Generation::INITIAL,
        snapshot: SnapshotBundle {
            snapshot_id: TaskSnapshotId::from_bytes([seed.wrapping_add(6); 16]),
            snapshot_digest: [seed.wrapping_add(7); 32],
            expected_head_commit_seq: 0,
            effect_history_root: empty_effect_history_root(),
            retry_fence_epoch: 0,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes([seed.wrapping_add(8); 16]),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(9); 16]),
        registered_at_ms: 2_000,
    };
    tasks.register_attempt(attempt).unwrap();
    let expectation = ArtifactPublicationExpectation {
        staging_id: staging_id_for(artifact_id, stage_key).into_bytes(),
        artifact_id,
        target_revision: 1,
        digest: ContentDigest::of_bytes(&bytes).into_bytes(),
        size_bytes: u64::try_from(bytes.len()).unwrap(),
    };
    let write_set_root = artifact_publication_plan_root(&[expectation]).unwrap();
    let PermitDecision::Issued(permit) = tasks
        .request_commit_permit(PermitRequest {
            task_id,
            attempt_id: attempt.attempt_id,
            attempt_generation: Generation::INITIAL,
            write_set_root,
            planned_effects: Vec::new(),
            idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(10); 16]),
            valid_until_ms: i64::MAX,
            requested_at_ms: 3_000,
        })
        .unwrap()
    else {
        panic!("expected issued permit");
    };
    artifacts
        .stage_revision(StageRevisionRequest {
            artifact_id,
            expected_head_revision: 0,
            bytes: &bytes,
            task_id,
            permit_id: permit.permit_id,
            write_set_root: ContentDigest::from_bytes(write_set_root),
            idempotency_key: stage_key,
            created_at_ms: 3_500,
        })
        .unwrap();
    let plan_id = tasks
        .plan_artifact_commit(PlanArtifactCommitRequest {
            task_id,
            attempt_id: attempt.attempt_id,
            attempt_generation: Generation::INITIAL,
            permit_id: permit.permit_id,
            expectations: vec![expectation],
            idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(11); 16]),
            planned_at_ms: 4_000,
        })
        .unwrap()
        .record()
        .plan_id;
    PreparedSingle {
        plan: plan_id,
        task: task_id,
    }
}

// ---------------------------------------------------------------------
// Semantic-domain fixture (copied from unified_worker_dual_domain.rs).
// ---------------------------------------------------------------------

fn seed_semantic_event(root: &Path, seed: u8) -> (SemanticEventId, ReceiptId, ReceiptId) {
    let _authority = SemanticAuthority::open(root).expect("open Semantic authority");
    let event_id = SemanticEventId::from_bytes([seed; 32]);
    let admission_receipt_id = ReceiptId::from_bytes([seed.wrapping_add(1); 16]);
    let durability_receipt_id = ReceiptId::from_bytes([seed.wrapping_add(2); 16]);
    let target = NamespaceId::from_bytes([0xc0; 16]);
    let content_digest = [seed.wrapping_add(3); 32];
    let raw = Connection::open(root.join("semantic-authority.db")).expect("open raw Semantic db");
    raw.execute(
        "INSERT INTO content_objects (content_digest, media_type, exact_bytes)
         VALUES (?1, ?2, ?3)",
        rusqlite::params![content_digest.as_slice(), "text/plain", b"semantic"],
    )
    .expect("insert content");
    raw.execute(
        "INSERT INTO semantic_events (
            event_id, canonical_unsigned_event, event_type, scope_kind, scope_id,
            issuer_principal_id, issuer_process_id, issuer_process_generation,
            control_domain_id, issued_at_unix_ns, valid_until_ms, purpose_digest,
            key_id, content_digest
         ) VALUES (?1, ?2, 1, 1, ?3, ?4, ?5, 1, ?6, 1, NULL, NULL, ?7, ?8)",
        rusqlite::params![
            event_id.as_bytes().as_slice(),
            [seed, seed.wrapping_add(1), seed.wrapping_add(2)].as_slice(),
            target.as_bytes().as_slice(),
            [seed.wrapping_add(4); 16].as_slice(),
            [seed.wrapping_add(5); 16].as_slice(),
            [seed.wrapping_add(6); 16].as_slice(),
            [seed.wrapping_add(7); 16].as_slice(),
            content_digest.as_slice(),
        ],
    )
    .expect("insert event");
    raw.execute(
        "INSERT INTO event_log (event_id) VALUES (?1)",
        [event_id.as_bytes().as_slice()],
    )
    .expect("insert event log");
    raw.execute(
        "INSERT INTO admission_receipts (
            receipt_id, event_id, log_seq, admitted_at_ms, effective_valid_until_ms,
            effective_taint, authz_policy_digest, durability, store_principal_id,
            store_control_domain_id, store_key_id, store_signature
         ) VALUES (?1, ?2, 1, 100, NULL, 0, ?3, 2, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            admission_receipt_id.as_bytes().as_slice(),
            event_id.as_bytes().as_slice(),
            [0xe8u8; 32].as_slice(),
            [seed.wrapping_add(8); 16].as_slice(),
            [seed.wrapping_add(9); 16].as_slice(),
            [seed.wrapping_add(10); 16].as_slice(),
            [seed.wrapping_add(11); 64].as_slice(),
        ],
    )
    .expect("insert admission");
    raw.execute(
        "INSERT INTO durability_receipts (
            receipt_id, event_id, durable_checkpoint_id, durable_at_ms, store_signature
         ) VALUES (?1, ?2, ?3, 110, ?4)",
        rusqlite::params![
            durability_receipt_id.as_bytes().as_slice(),
            event_id.as_bytes().as_slice(),
            [seed.wrapping_add(12); 32].as_slice(),
            [seed.wrapping_add(13); 64].as_slice(),
        ],
    )
    .expect("insert durability");
    drop(raw);
    (event_id, admission_receipt_id, durability_receipt_id)
}

struct PreparedSemantic {
    plan: SemanticCommitPlanId,
    task: TaskId,
}

#[allow(clippy::too_many_lines)]
fn prepare_semantic(databases: &TestAuthorities, seed: u8) -> PreparedSemantic {
    let (event_id, admission_receipt_id, durability_receipt_id) =
        seed_semantic_event(&databases.semantic_root, seed);
    let (tasks, artifacts) = databases.open();
    let semantic = SemanticAuthority::open(&databases.semantic_root).unwrap();
    let task_id = TaskId::from_bytes([seed; 16]);
    let attempt_id = TaskAttemptId::from_bytes([seed.wrapping_add(16); 16]);
    let target = NamespaceId::from_bytes([0xc0; 16]);
    let attempt = AttemptSpec {
        task_id,
        attempt_id,
        attempt_generation: Generation::INITIAL,
        snapshot: SnapshotBundle {
            snapshot_id: TaskSnapshotId::from_bytes([seed.wrapping_add(17); 16]),
            snapshot_digest: [seed.wrapping_add(18); 32],
            expected_head_commit_seq: 0,
            effect_history_root: empty_effect_history_root(),
            retry_fence_epoch: 0,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes([seed.wrapping_add(19); 16]),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(20); 16]),
        registered_at_ms: 2_000,
    };
    tasks
        .register_task(TaskSpec {
            task_id,
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
        })
        .unwrap();
    let snapshot_receipt = ReceiptId::from_bytes([seed.wrapping_add(21); 16]);
    tasks
        .register_snapshot_receipt(TaskSnapshotReceiptSpec {
            task_id,
            snapshot: attempt.snapshot,
            receipt_id: snapshot_receipt,
            builder_id: [seed.wrapping_add(22); 16],
            builder_version_digest: [seed.wrapping_add(23); 32],
            per_authority_checkpoint_receipts: vec![ReceiptId::from_bytes(
                [seed.wrapping_add(24); 16],
            )],
            dependency_closure_root: [seed.wrapping_add(25); 32],
            semantic_resolver_digest: [seed.wrapping_add(26); 32],
            canonical_iteration_digest: [seed.wrapping_add(27); 32],
            achieved_consistency: SnapshotConsistency::Causal,
            built_at_ms: 1_500,
            authority_id: [seed.wrapping_add(28); 16],
            key_id: [seed.wrapping_add(29); 16],
            signature: [seed.wrapping_add(30); 64],
        })
        .unwrap();
    tasks
        .register_attempt_with_snapshot_receipt(attempt, snapshot_receipt)
        .unwrap();
    let registry = tasks.inspect_participant_registry(task_id).unwrap();
    tasks
        .register_semantic_admission_participant(
            &semantic,
            task_id,
            ParticipantRegistryBinding {
                generation: registry.generation,
                root: registry.root,
            },
            2_500,
        )
        .unwrap();
    let write_set = tasks
        .seal_task_write_set_with_semantic_authority(
            &artifacts,
            &semantic,
            TaskWriteSetRequest {
                task_id,
                attempt_id,
                attempt_generation: Generation::INITIAL,
                artifact_reads: Vec::new(),
                artifact_writes: Vec::new(),
                process_binding: None,
                semantic_reads: Vec::new(),
                semantic_appends: vec![TaskWriteSetSemanticAppendRequest {
                    event_id,
                    target: TaskWriteSetSemanticTarget::Namespace(target),
                    required_durability: TaskWriteSetSemanticRequiredDurability::Durable,
                    expected_admission_policy_digest: [0xe8; 32],
                    durability_receipt_id: Some(durability_receipt_id),
                }],
                resource_reservations: Vec::new(),
                planned_effects: Vec::new(),
                effect_endpoints: Vec::new(),
                idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(31); 16]),
                sealed_at_ms: 3_000,
            },
        )
        .unwrap()
        .record()
        .clone();
    assert_eq!(
        write_set.semantic_appends[0].admission_receipt_id,
        admission_receipt_id
    );
    let permit = match tasks
        .request_commit_permit(PermitRequest {
            task_id,
            attempt_id,
            attempt_generation: Generation::INITIAL,
            write_set_root: write_set.write_set_root,
            planned_effects: Vec::new(),
            idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(32); 16]),
            valid_until_ms: i64::MAX,
            requested_at_ms: 3_500,
        })
        .unwrap()
    {
        PermitDecision::Issued(permit) => *permit,
        other => panic!("expected issued permit, got {other:?}"),
    };
    let plan_id = tasks
        .plan_semantic_commit(PlanSemanticCommitRequest {
            task_id,
            attempt_id,
            attempt_generation: Generation::INITIAL,
            permit_id: permit.permit_id,
            idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(33); 16]),
            planned_at_ms: 4_000,
        })
        .unwrap()
        .record()
        .plan_id;
    PreparedSemantic {
        plan: plan_id,
        task: task_id,
    }
}

// ---------------------------------------------------------------------
// Resource-domain fixture (adapted from the nlos-task W28-C-1 fixture in
// tests/resource_prepare_finalize.rs, identity bytes derived from `seed`
// so the three domains coexist in one task authority).
// ---------------------------------------------------------------------

/// One Resource owner fixture: driver, account, and reservation helpers.
struct OwnerFixture {
    authority: ResourceAuthority,
    driver: DriverRecord,
    #[allow(dead_code)]
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
                // Large but i64-safe: above any wall clock this test can
                // observe, like the fixture permits' valid_until_ms.
                valid_until_ms: u64::try_from(EXPLICIT_CONVERGE_MS).unwrap(),
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
}

/// Two reservations: R1 uses 30 then 37 of 100 (refund 63), R2 uses 10 of
/// 25 (refund 15) — the shared bridge shape of the nlos-task fixtures.
fn two_reservations(owner: &OwnerFixture, seed: u8) -> Vec<ReservationRecord> {
    let quote_one = owner.quote(seed.wrapping_add(0x02), 100);
    let quote_two = owner.quote(seed.wrapping_add(0x03), 25);
    vec![
        owner.reserve(
            &quote_one,
            CallId::from_bytes([seed.wrapping_add(0x04); 16]),
            OperationId::from_bytes([seed.wrapping_add(0x05); 16]),
            IdempotencyKey::from_bytes([seed.wrapping_add(0x06); 16]),
        ),
        owner.reserve(
            &quote_two,
            CallId::from_bytes([seed.wrapping_add(0x07); 16]),
            OperationId::from_bytes([seed.wrapping_add(0x08); 16]),
            IdempotencyKey::from_bytes([seed.wrapping_add(0x09); 16]),
        ),
    ]
}

/// Activates the reservation, records the ordered consumptions, and
/// finalizes with the given final usage (the caller/gateway half of the
/// bridge; ADR-0017 constraint 4 keeps this out of the worker).
fn settle(
    authority: &ResourceAuthority,
    reservation: &ReservationRecord,
    consumptions: &[(u64, u64)],
    final_usage: u64,
    seed: u8,
) {
    let activation = authority
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
        authority
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
    authority
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

/// Settles both fixture reservations on a fresh owner handle: after this,
/// every owner Reservation is FINALIZED and the plan is due.
fn settle_all(databases: &TestAuthorities, reservations: &[ReservationRecord], seed: u8) {
    let owner = databases.open_resource();
    settle(
        &owner,
        &reservations[0],
        &[(1, 30), (2, 37)],
        37,
        seed.wrapping_add(0x0a),
    );
    settle(
        &owner,
        &reservations[1],
        &[(1, 10)],
        10,
        seed.wrapping_add(0x0b),
    );
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
    databases: &TestAuthorities,
    reservations: &[ReservationRecord],
) -> Vec<NestedResourceCostReceipt> {
    let owner = databases.open_resource();
    let mut nested = reservations
        .iter()
        .map(|reservation| {
            nested_from_owner(
                &owner
                    .inspect_cost_receipt(reservation.reservation_id)
                    .expect("owner aggregate"),
            )
        })
        .collect::<Vec<_>>();
    nested.sort_unstable_by_key(|record| record.reservation_id);
    nested
}

struct PreparedResource {
    plan: ResourceCommitPlanId,
    task: TaskId,
    reservations: Vec<ReservationRecord>,
}

/// Leaves one durable Resource finalize plan in `Planned`: register the
/// task half, register the owner participants, seal the reservation-bearing
/// write set, draw a resource-aware permit, and persist the finalize
/// envelope. The owner reservations stay `Reserved` (not due).
#[allow(clippy::too_many_lines)]
fn prepare_resource(databases: &TestAuthorities, seed: u8) -> PreparedResource {
    let owner = OwnerFixture::new(&databases.resource_root, seed);
    let reservations = two_reservations(&owner, seed);
    let (tasks, artifacts) = databases.open();
    let task_id = TaskId::from_bytes([seed; 16]);
    let attempt_id = TaskAttemptId::from_bytes([seed.wrapping_add(0x20); 16]);
    let attempt = AttemptSpec {
        task_id,
        attempt_id,
        attempt_generation: Generation::INITIAL,
        snapshot: SnapshotBundle {
            snapshot_id: TaskSnapshotId::from_bytes([seed.wrapping_add(0x21); 16]),
            snapshot_digest: [seed.wrapping_add(0x22); 32],
            expected_head_commit_seq: 0,
            effect_history_root: empty_effect_history_root(),
            retry_fence_epoch: 0,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes([seed.wrapping_add(0x23); 16]),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(0x24); 16]),
        registered_at_ms: 1_010,
    };
    tasks
        .register_task(TaskSpec {
            task_id,
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
        })
        .expect("register task");
    let snapshot_receipt = ReceiptId::from_bytes([seed.wrapping_add(0x25); 16]);
    tasks
        .register_snapshot_receipt(TaskSnapshotReceiptSpec {
            task_id,
            snapshot: attempt.snapshot,
            receipt_id: snapshot_receipt,
            builder_id: [seed.wrapping_add(0x26); 16],
            builder_version_digest: [seed.wrapping_add(0x27); 32],
            per_authority_checkpoint_receipts: vec![ReceiptId::from_bytes(
                [seed.wrapping_add(0x28); 16],
            )],
            dependency_closure_root: [seed.wrapping_add(0x29); 32],
            semantic_resolver_digest: [seed.wrapping_add(0x2a); 32],
            canonical_iteration_digest: [seed.wrapping_add(0x2b); 32],
            achieved_consistency: SnapshotConsistency::Causal,
            built_at_ms: 1_005,
            authority_id: [seed.wrapping_add(0x2c); 16],
            key_id: [seed.wrapping_add(0x2d); 16],
            signature: [seed.wrapping_add(0x2e); 64],
        })
        .expect("snapshot receipt");
    tasks
        .register_attempt_with_snapshot_receipt(attempt, snapshot_receipt)
        .expect("register attempt");
    let registry = tasks
        .inspect_participant_registry(task_id)
        .expect("registry");
    let first_binding = ParticipantRegistryBinding {
        generation: registry.generation,
        root: registry.root,
    };
    let driver_registration = tasks
        .register_driver_gateway_participant(
            &owner.authority,
            task_id,
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
    tasks
        .register_resource_ledger_participant(
            &owner.authority,
            task_id,
            second_binding,
            owner.account.account_id,
            Generation::INITIAL,
            1_160,
        )
        .expect("ledger participant");
    let write_set = tasks
        .seal_task_write_set_with_resource_authority(
            &artifacts,
            &owner.authority,
            TaskWriteSetRequest {
                task_id,
                attempt_id,
                attempt_generation: Generation::INITIAL,
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
                idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(0x2f); 16]),
                sealed_at_ms: 1_200,
            },
        )
        .expect("seal write set")
        .record()
        .clone();
    let permit = match tasks
        .request_commit_permit_with_resource_authority(
            &owner.authority,
            PermitRequest {
                task_id,
                attempt_id,
                attempt_generation: Generation::INITIAL,
                write_set_root: write_set.write_set_root,
                planned_effects: Vec::new(),
                idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(0x30); 16]),
                valid_until_ms: i64::MAX,
                requested_at_ms: 1_300,
            },
        )
        .expect("permit")
    {
        PermitDecision::Issued(permit) => *permit,
        other => panic!("expected issued permit, got {other:?}"),
    };
    let plan_id = tasks
        .prepare_resource_finalize(PrepareResourceFinalizeRequest {
            task_id,
            attempt_id,
            attempt_generation: Generation::INITIAL,
            permit_id: permit.permit_id,
            idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(0x31); 16]),
            required_satisfaction: Vec::new(),
            fenced_participant_digest: [0x45; 32],
            prepared_at_ms: 1_450,
        })
        .expect("prepare resource finalize envelope")
        .record()
        .plan_id;
    PreparedResource {
        plan: plan_id,
        task: task_id,
        reservations,
    }
}

// ---------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------

#[test]
fn recovery_worker_health_defaults_keep_resource_domain_quiescent() {
    let health = RecoveryWorkerHealth::default();
    assert_eq!(health.state, RecoveryWorkerState::Starting);
    // Resource-domain fields default to zero/false, mirroring the semantic
    // projection for a worker started without that authority.
    assert_eq!(health.resource_total_inspected, 0);
    assert_eq!(health.resource_total_finalized, 0);
    assert_eq!(health.resource_consecutive_failed_cycles, 0);
    assert_eq!(health.resource_durable_retrying, 0);
    assert_eq!(health.resource_durable_escalated, 0);
    assert_eq!(health.resource_durable_unacknowledged_escalated, 0);
    assert_eq!(health.resource_durable_resolved, 0);
    assert!(!health.resource_domain_faulted);
    assert!(!health.semantic_domain_faulted);
    assert!(!health.artifact_domain_faulted);
}

#[test]
fn worker_without_resource_authority_keeps_resource_domain_quiescent() {
    // The additive seam must not change the older callers' projections: a
    // worker started through `start_with_semantic_authority` supplies no
    // resource authority, so a pending resource plan is never scanned and
    // every resource health field stays at its zero default.
    let databases = TestAuthorities::new("tri-domain-no-resource-authority");
    let pending = prepare_resource(&databases, 0x31);
    let (tasks, artifacts) = databases.open();
    let semantic = SemanticAuthority::open(&databases.semantic_root).unwrap();
    let tasks = Arc::new(tasks);
    let mut worker = TaskAuthorityCommitRecoveryWorker::start_with_semantic_authority(
        Arc::clone(&tasks),
        Arc::new(artifacts),
        Some(Arc::new(semantic)),
        fast_config(),
    )
    .unwrap();

    wait_until(|| worker.health().completed_cycles >= 3);
    let health = worker.health();
    assert_eq!(health.state, RecoveryWorkerState::Running);
    assert_eq!(
        tasks
            .inspect_resource_commit_plan(pending.plan)
            .unwrap()
            .state,
        ResourceCommitPlanState::Planned,
        "no resource authority means no resource scanning"
    );
    assert_eq!(health.resource_total_inspected, 0);
    assert_eq!(health.resource_total_finalized, 0);
    assert_eq!(health.resource_consecutive_failed_cycles, 0);
    assert_eq!(health.resource_durable_retrying, 0);
    assert_eq!(health.resource_durable_escalated, 0);
    assert_eq!(health.resource_durable_unacknowledged_escalated, 0);
    assert_eq!(health.resource_durable_resolved, 0);
    assert!(!health.resource_domain_faulted);
    worker.stop();
    assert_eq!(worker.health().state, RecoveryWorkerState::Stopped);
}

#[test]
fn worker_converges_pending_resource_plan_without_caller() {
    // ADR-0017 appendix A G3, worker half: prepare(envelope) → settle every
    // owner Reservation → drop the caller → the worker alone converges the
    // durable plan to its unique terminal state from plan/envelope bytes
    // plus owner reads.
    let databases = TestAuthorities::new("tri-domain-resource-restart");
    let pending = prepare_resource(&databases, 0x51);
    settle_all(&databases, &pending.reservations, 0x51);
    let (tasks, artifacts) = databases.open();
    let resource = databases.open_resource();
    let tasks = Arc::new(tasks);
    let mut worker =
        TaskAuthorityCommitRecoveryWorker::start_with_semantic_and_resource_authorities(
            Arc::clone(&tasks),
            Arc::new(artifacts),
            None,
            Some(Arc::new(resource)),
            fast_config(),
        )
        .unwrap();

    wait_until(|| worker.health().resource_total_finalized >= 1);

    // Unique terminal state: plan FINALIZED and bound to its receipt, head
    // advanced, exactly one nested receipt set, scan empty.
    let plan = tasks.inspect_resource_commit_plan(pending.plan).unwrap();
    assert_eq!(plan.state, ResourceCommitPlanState::Finalized);
    let receipt_id = plan.task_receipt_id.expect("bound task receipt");
    let task = tasks.inspect_task(pending.task).unwrap();
    assert_eq!(task.head_commit_seq, 1);
    let mut nested = tasks
        .inspect_resource_cost_receipts(pending.task, receipt_id)
        .unwrap();
    nested.sort_unstable_by_key(|record| record.reservation_id);
    assert_eq!(nested, expected_nested(&databases, &pending.reservations));
    assert_eq!(
        raw_count(
            &databases.task_path,
            "SELECT COUNT(*) FROM task_resource_cost_receipts",
        ),
        2
    );
    assert_eq!(
        raw_count(
            &databases.task_path,
            "SELECT COUNT(*) FROM task_resource_cost_consumptions",
        ),
        3
    );
    assert!(
        tasks
            .list_incomplete_resource_commit_plans(8)
            .unwrap()
            .is_empty()
    );

    // Explicit replays are byte-equal with zero duplicated rows.
    let resource = databases.open_resource();
    let first = tasks
        .converge_resource_commit_plan(&resource, pending.plan, EXPLICIT_CONVERGE_MS)
        .unwrap();
    let second = tasks
        .converge_resource_commit_plan(&resource, pending.plan, EXPLICIT_CONVERGE_MS + 1)
        .unwrap();
    let (ResourceConvergeDecision::Replayed(first), ResourceConvergeDecision::Replayed(second)) =
        (first, second)
    else {
        panic!("expected replayed decisions");
    };
    assert_eq!(first.task_receipt, second.task_receipt);
    assert_eq!(first.resource_cost_receipts, second.resource_cost_receipts);
    assert_eq!(
        raw_count(
            &databases.task_path,
            "SELECT COUNT(*) FROM task_resource_cost_receipts",
        ),
        2
    );

    // Clean convergence never opens a resource ledger row.
    assert!(
        tasks
            .inspect_resource_recovery(pending.plan)
            .unwrap()
            .is_none()
    );
    let health = worker.health();
    assert_eq!(health.state, RecoveryWorkerState::Running);
    assert_eq!(health.resource_total_inspected, 1);
    assert_eq!(health.resource_total_finalized, 1);
    assert_eq!(health.resource_consecutive_failed_cycles, 0);
    assert!(health.last_failures.is_empty());
    assert_eq!(health.retry_delay, None);
    assert_eq!(health.resource_durable_retrying, 0);
    assert_eq!(health.resource_durable_escalated, 0);
    assert_eq!(health.resource_durable_unacknowledged_escalated, 0);
    assert_eq!(health.resource_durable_resolved, 0);
    assert!(!health.resource_domain_faulted);
    assert!(!health.semantic_domain_faulted);
    assert!(!health.artifact_domain_faulted);
    worker.stop();
    assert_eq!(worker.health().state, RecoveryWorkerState::Stopped);
}

#[test]
fn not_due_resource_plan_records_no_failure_and_keeps_scanning() {
    // ADR-0017 G4 through the worker: the owner Reservations stay
    // `Reserved`, so every worker cycle reports NotDue — no failure, no
    // ledger row, no backoff — and the stuck plan remains an inspectable
    // durable fact that the due scan keeps returning.
    let databases = TestAuthorities::new("tri-domain-resource-not-due");
    let pending = prepare_resource(&databases, 0x53);
    let (tasks, artifacts) = databases.open();
    let resource = databases.open_resource();
    let owner_reservations_before: Vec<ReservationRecord> = pending
        .reservations
        .iter()
        .map(|reservation| {
            resource
                .inspect_reservation(reservation.reservation_id)
                .unwrap()
        })
        .collect();
    let tasks = Arc::new(tasks);
    let mut worker =
        TaskAuthorityCommitRecoveryWorker::start_with_semantic_and_resource_authorities(
            Arc::clone(&tasks),
            Arc::new(artifacts),
            None,
            Some(Arc::new(resource)),
            fast_config(),
        )
        .unwrap();

    wait_until(|| worker.health().resource_total_inspected >= 3);

    let health = worker.health();
    assert_eq!(health.state, RecoveryWorkerState::Running);
    assert_eq!(health.resource_total_finalized, 0);
    assert_eq!(health.resource_consecutive_failed_cycles, 0);
    assert!(health.last_failures.is_empty());
    assert_eq!(health.retry_delay, None);
    assert_eq!(health.resource_durable_retrying, 0);
    assert_eq!(health.resource_durable_escalated, 0);
    assert_eq!(health.resource_durable_unacknowledged_escalated, 0);
    assert_eq!(health.resource_durable_resolved, 0);
    assert!(!health.resource_domain_faulted);

    // Zero ledger writes: no recovery row exists for the not-due plan.
    assert!(
        tasks
            .inspect_resource_recovery(pending.plan)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        raw_count(
            &databases.task_path,
            "SELECT COUNT(*) FROM task_resource_recovery",
        ),
        0
    );

    // The plan stays Planned and the due scan keeps returning it.
    assert_eq!(
        tasks
            .inspect_resource_commit_plan(pending.plan)
            .unwrap()
            .state,
        ResourceCommitPlanState::Planned
    );
    let due = tasks.list_due_resource_commit_plans(8, 9_999_999).unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].plan_id, pending.plan);

    // The owner was not mutated by any worker cycle.
    let resource = databases.open_resource();
    for (reservation, before) in pending.reservations.iter().zip(&owner_reservations_before) {
        assert_eq!(
            resource
                .inspect_reservation(reservation.reservation_id)
                .unwrap()
                .state,
            ReservationState::Reserved
        );
        assert_eq!(
            resource
                .inspect_reservation(reservation.reservation_id)
                .unwrap(),
            *before
        );
    }
    worker.stop();
}

#[test]
fn tri_domain_pending_plans_converge_in_one_worker() {
    // ADR-0017 G6 first clause: with pending plans in all three domains,
    // one worker cycle sequence (artifact → semantic → resource) converges
    // every domain.
    let databases = TestAuthorities::new("tri-domain-all-converge");
    let artifact_pending = prepare_single(&databases, 0x61);
    let semantic_pending = prepare_semantic(&databases, 0x64);
    let resource_pending = prepare_resource(&databases, 0x51);
    settle_all(&databases, &resource_pending.reservations, 0x51);
    let (tasks, artifacts) = databases.open();
    let semantic = SemanticAuthority::open(&databases.semantic_root).unwrap();
    let resource = databases.open_resource();
    let tasks = Arc::new(tasks);
    let mut worker =
        TaskAuthorityCommitRecoveryWorker::start_with_semantic_and_resource_authorities(
            Arc::clone(&tasks),
            Arc::new(artifacts),
            Some(Arc::new(semantic)),
            Some(Arc::new(resource)),
            fast_config(),
        )
        .unwrap();

    wait_until(|| {
        tasks
            .inspect_artifact_commit_plan(artifact_pending.plan)
            .is_ok_and(|plan| plan.state == ArtifactCommitPlanState::Finalized)
            && tasks
                .inspect_semantic_commit_progress(semantic_pending.plan)
                .is_ok_and(|progress| progress.plan.state == SemanticCommitPlanState::Finalized)
            && tasks
                .inspect_resource_commit_plan(resource_pending.plan)
                .is_ok_and(|plan| plan.state == ResourceCommitPlanState::Finalized)
    });

    let health = worker.health();
    assert_eq!(health.state, RecoveryWorkerState::Running);
    assert_eq!(health.total_inspected, 1);
    assert_eq!(health.total_finalized, 1);
    assert_eq!(health.semantic_total_inspected, 1);
    assert_eq!(health.semantic_total_finalized, 1);
    assert_eq!(health.resource_total_inspected, 1);
    assert_eq!(health.resource_total_finalized, 1);
    assert_eq!(health.consecutive_failed_cycles, 0);
    assert_eq!(health.semantic_consecutive_failed_cycles, 0);
    assert_eq!(health.resource_consecutive_failed_cycles, 0);
    assert!(health.last_failures.is_empty());
    assert_eq!(health.retry_delay, None);
    assert!(!health.artifact_domain_faulted);
    assert!(!health.semantic_domain_faulted);
    assert!(!health.resource_domain_faulted);
    assert_eq!(
        tasks
            .inspect_task(artifact_pending.task)
            .unwrap()
            .head_commit_seq,
        1
    );
    assert_eq!(
        tasks
            .inspect_task(semantic_pending.task)
            .unwrap()
            .head_commit_seq,
        1
    );
    assert_eq!(
        tasks
            .inspect_task(resource_pending.task)
            .unwrap()
            .head_commit_seq,
        1
    );
    worker.stop();
    assert_eq!(worker.health().state, RecoveryWorkerState::Stopped);
}

#[test]
fn resource_plan_failures_back_off_escalate_resume_and_resolve_through_worker() {
    // The full durable recovery loop driven by the worker alone (the W26
    // semantic escalate→resume→resolve composite, mirrored): plan-level
    // converge failures are recorded in the resource ledger with durable
    // backoff, exhaust its pinned escalation threshold, an operator resume
    // requeues the row by CAS, and the same finalize transaction that
    // converges the plan resolves the ledger.
    let databases = TestAuthorities::new("tri-domain-resource-escalate");
    let pending = prepare_resource(&databases, 0x55);
    settle_all(&databases, &pending.reservations, 0x55);
    execute_sql(
        &databases.task_path,
        "CREATE TRIGGER keep_resource_plan_unconverged
         BEFORE UPDATE ON task_resource_commit_plans
         WHEN NEW.plan_state != OLD.plan_state
         BEGIN SELECT RAISE(ABORT, 'persistent resource storage failure'); END;",
    );
    let (tasks, artifacts) = databases.open();
    let resource = databases.open_resource();
    let tasks = Arc::new(tasks);
    let mut worker =
        TaskAuthorityCommitRecoveryWorker::start_with_semantic_and_resource_authorities(
            Arc::clone(&tasks),
            Arc::new(artifacts),
            None,
            Some(Arc::new(resource)),
            fast_config(),
        )
        .unwrap();

    // Failure half: durable backoff advances to the pinned escalation
    // threshold (8, inside nlos-task), asserted on the ledger row itself.
    wait_until(|| {
        tasks
            .inspect_resource_recovery(pending.plan)
            .unwrap()
            .is_some_and(|record| record.state == nlos_task::ResourceRecoveryState::Escalated)
    });
    let escalated = tasks
        .inspect_resource_recovery(pending.plan)
        .unwrap()
        .expect("escalated resource ledger row");
    assert_eq!(escalated.next_retry_at_ms, None);
    assert_eq!(escalated.consecutive_failures, 8);
    assert_eq!(
        escalated.last_source,
        nlos_task::ResourceRecoveryFailureSource::TaskAuthority
    );
    let health = worker.health();
    assert_eq!(health.state, RecoveryWorkerState::Running);
    assert_eq!(health.resource_durable_escalated, 1);
    assert_eq!(health.resource_durable_unacknowledged_escalated, 1);
    assert_eq!(
        health.resource_consecutive_failed_cycles, 0,
        "plan-level resource failures must not consume the worker budget"
    );
    assert!(!health.resource_domain_faulted);
    assert_eq!(
        tasks
            .inspect_resource_commit_plan(pending.plan)
            .unwrap()
            .state,
        ResourceCommitPlanState::Planned
    );

    // Repair, then the manual resume half: the CAS on `total_failures`
    // requeues the escalated row as due for the next worker scan.
    execute_sql(
        &databases.task_path,
        "DROP TRIGGER keep_resource_plan_unconverged;",
    );
    tasks
        .resume_resource_recovery(nlos_task::ResourceRecoveryResumeRequest {
            plan_id: pending.plan,
            expected_total_failures: escalated.total_failures,
            resumed_at_ms: escalated.last_failed_at_ms,
        })
        .unwrap();

    // Automatic convergence half: the worker alone finalizes the plan and
    // the same finalize transaction resolves the ledger row.
    wait_until(|| worker.health().resource_total_finalized >= 1);
    assert_eq!(
        tasks
            .inspect_resource_commit_plan(pending.plan)
            .unwrap()
            .state,
        ResourceCommitPlanState::Finalized
    );
    assert_eq!(tasks.inspect_task(pending.task).unwrap().head_commit_seq, 1);
    let resolved = tasks
        .inspect_resource_recovery(pending.plan)
        .unwrap()
        .expect("resolved resource ledger row");
    assert_eq!(resolved.state, nlos_task::ResourceRecoveryState::Resolved);
    assert_eq!(
        resolved.total_failures, escalated.total_failures,
        "resolution preserves the durable failure history"
    );
    let health = worker.health();
    assert_eq!(health.resource_durable_resolved, 1);
    assert_eq!(health.resource_durable_escalated, 0);
    assert_eq!(health.resource_durable_unacknowledged_escalated, 0);
    assert!(!health.resource_domain_faulted);
    worker.stop();
    assert_eq!(worker.health().state, RecoveryWorkerState::Stopped);
}

#[test]
fn resource_domain_fault_is_isolated_from_artifact_and_semantic_convergence() {
    // ADR-0017 G6 core: persistent resource-path storage failures (plan
    // flip rejected and the resource ledger cannot open a row) burn only
    // the resource domain's infrastructure budget; artifact and semantic
    // convergence keep running on the same thread.
    let databases = TestAuthorities::new("tri-domain-resource-fault");
    let artifact_pending = prepare_single(&databases, 0x71);
    let semantic_pending = prepare_semantic(&databases, 0x74);
    let resource_pending = prepare_resource(&databases, 0x51);
    // The plan must be due (owner settled) so the worker attempts the
    // finalize transaction, whose plan flip the trigger rejects.
    settle_all(&databases, &resource_pending.reservations, 0x51);
    execute_sql(
        &databases.task_path,
        "CREATE TRIGGER keep_resource_plan_unconverged
         BEFORE UPDATE ON task_resource_commit_plans
         WHEN NEW.plan_state != OLD.plan_state
         BEGIN SELECT RAISE(ABORT, 'persistent resource storage failure'); END;
         CREATE TRIGGER resource_ledger_unavailable
         BEFORE INSERT ON task_resource_recovery
         BEGIN SELECT RAISE(ABORT, 'resource ledger unavailable'); END;",
    );
    let (tasks, artifacts) = databases.open();
    let semantic = SemanticAuthority::open(&databases.semantic_root).unwrap();
    let resource = databases.open_resource();
    let tasks = Arc::new(tasks);
    let mut worker =
        TaskAuthorityCommitRecoveryWorker::start_with_semantic_and_resource_authorities(
            Arc::clone(&tasks),
            Arc::new(artifacts),
            Some(Arc::new(semantic)),
            Some(Arc::new(resource)),
            fast_config(),
        )
        .unwrap();

    // Domain isolation core assertion: artifact and semantic plans converge
    // while the resource half keeps failing and then faults.
    wait_until(|| {
        tasks
            .inspect_artifact_commit_plan(artifact_pending.plan)
            .is_ok_and(|plan| plan.state == ArtifactCommitPlanState::Finalized)
            && tasks
                .inspect_semantic_commit_progress(semantic_pending.plan)
                .is_ok_and(|progress| progress.plan.state == SemanticCommitPlanState::Finalized)
            && worker.health().resource_domain_faulted
    });

    let isolated = worker.health();
    assert_eq!(
        isolated.state,
        RecoveryWorkerState::Running,
        "a faulted resource domain must not terminate the worker thread"
    );
    assert!(isolated.resource_domain_faulted);
    assert!(!isolated.semantic_domain_faulted);
    assert!(!isolated.artifact_domain_faulted);
    // Pinned observability decision mirrored from the semantic domain: the
    // counter that faulted the domain is NOT cleared when the bit is set.
    assert_eq!(isolated.resource_consecutive_failed_cycles, 3);
    assert_eq!(isolated.semantic_consecutive_failed_cycles, 0);
    assert_eq!(isolated.consecutive_failed_cycles, 0);
    assert_eq!(isolated.resource_total_finalized, 0);
    assert_eq!(isolated.total_finalized, 1);
    assert_eq!(isolated.semantic_total_finalized, 1);
    assert_eq!(isolated.retry_delay, None);
    assert_eq!(isolated.resource_durable_retrying, 0);
    assert_eq!(isolated.resource_durable_escalated, 0);
    assert_eq!(isolated.resource_durable_unacknowledged_escalated, 0);
    assert_eq!(isolated.resource_durable_resolved, 0);

    // The resource plan itself stays Planned and the ledger trigger must
    // have rejected every row.
    assert_eq!(
        tasks
            .inspect_resource_commit_plan(resource_pending.plan)
            .unwrap()
            .state,
        ResourceCommitPlanState::Planned
    );
    assert!(
        tasks
            .inspect_resource_recovery(resource_pending.plan)
            .unwrap()
            .is_none(),
        "the ledger trigger must have rejected every row"
    );
    assert_eq!(
        tasks
            .inspect_task(artifact_pending.task)
            .unwrap()
            .head_commit_seq,
        1
    );
    assert_eq!(
        tasks
            .inspect_task(semantic_pending.task)
            .unwrap()
            .head_commit_seq,
        1
    );

    // A faulted resource domain stops scanning while the other domains
    // keep their cadence: after the fault, further cycles do not grow the
    // resource inspected counter.
    let inspected_at_fault = isolated.resource_total_inspected;
    wait_until(|| worker.health().completed_cycles >= isolated.completed_cycles + 2);
    let after = worker.health();
    assert!(after.resource_domain_faulted);
    assert_eq!(
        after.resource_total_inspected, inspected_at_fault,
        "a faulted resource domain is skipped"
    );
    worker.stop();
    assert_eq!(worker.health().state, RecoveryWorkerState::Stopped);
}
