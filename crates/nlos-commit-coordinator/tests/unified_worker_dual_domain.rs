#![allow(deprecated)] // Ladder constructors deprecated in favor of the *_with_authorities_struct entries.
//! W26-002 dual-domain worker coverage. Task 6 split `durable_cycle` into an
//! artifact half plus a quiescent semantic half; this binary now also covers
//! the wired semantic half (`start_with_semantic_authority`): restart
//! convergence without any caller, one worker converging both domains,
//! per-domain fault isolation, the durable semantic recovery ledger, and the
//! mixed infrastructure/plan-level failure combination. A worker started
//! through `start` supplies no semantic authority, so its semantic-domain
//! projection stays quiescent (all zero/false) by construction.
//!
//! This is local single-process recovery evidence, not a distributed
//! atomic-commit claim: no cross-authority atomicity is asserted.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use nlos_artifact::{
    ArtifactStore, ContentDigest, CreateArtifactSpec, StageRevisionRequest, staging_id_for,
};
use nlos_commit_coordinator::{
    RecoveryWorkerConfig, RecoveryWorkerHealth, RecoveryWorkerState,
    TaskAuthorityCommitRecoveryWorker,
};
use nlos_semantic::SemanticAuthority;
use nlos_store_fault::{FaultCode, FaultMode};
use nlos_task::{
    ArtifactCommitPlanState, ArtifactPublicationExpectation, ArtifactRecoveryResumeRequest,
    ArtifactRecoveryState, AttemptSpec, PermitDecision, PermitRequest, PlanArtifactCommitRequest,
    PlanSemanticCommitRequest, SemanticCommitPlanId, SemanticCommitPlanState,
    SemanticRecoveryFailureSource, SemanticRecoveryResumeRequest, SemanticRecoveryState,
    SnapshotBundle, SnapshotConsistency, SqliteTaskAuthority, TaskSnapshotReceiptSpec, TaskSpec,
    TaskWriteSetRequest, TaskWriteSetSemanticAppendRequest, TaskWriteSetSemanticRequiredDurability,
    TaskWriteSetSemanticTarget, artifact_publication_plan_root, empty_effect_history_root,
};
use nlos_types::{
    ArtifactId, CancellationScopeId, Generation, IdempotencyKey, NamespaceId, ReceiptId,
    SemanticEventId, TaskAttemptId, TaskId, TaskSnapshotId,
};
use rusqlite::Connection;

static NEXT_DATABASE: AtomicU64 = AtomicU64::new(0);
static FAULT_LOCK: Mutex<()> = Mutex::new(());
const VFS_NAME: &str = "nlos-dual-domain-worker-fault";

struct FaultDisarmGuard;

impl Drop for FaultDisarmGuard {
    fn drop(&mut self) {
        nlos_store_fault::disarm();
    }
}

fn fault_lock() -> MutexGuard<'static, ()> {
    FAULT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

struct TestAuthorities {
    root: PathBuf,
    task_path: PathBuf,
    artifact_root: PathBuf,
    semantic_root: PathBuf,
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
            root,
        }
    }

    fn open(&self) -> (SqliteTaskAuthority, ArtifactStore) {
        (
            SqliteTaskAuthority::open(&self.task_path).unwrap(),
            ArtifactStore::open(&self.artifact_root).unwrap(),
        )
    }

    fn open_semantic(&self) -> SemanticAuthority {
        SemanticAuthority::open(&self.semantic_root).unwrap()
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

fn artifact(seed: u8) -> ArtifactId {
    ArtifactId::from_bytes([seed; 16])
}

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

#[derive(Clone, Copy)]
struct PreparedSingle {
    plan: nlos_task::ArtifactCommitPlanId,
    task: TaskId,
    artifact: ArtifactId,
}

fn prepare_single(databases: &TestAuthorities, seed: u8) -> PreparedSingle {
    let (tasks, artifacts) = databases.open();
    let task_id = TaskId::from_bytes([seed; 16]);
    let artifact_id = artifact(seed.wrapping_add(1));
    let stage_key = IdempotencyKey::from_bytes([seed.wrapping_add(2); 16]);
    let bytes = [seed.wrapping_add(3); 32];
    create_artifact(&artifacts, artifact_id, seed.wrapping_add(4));
    tasks
        .register_task(TaskSpec {
            task_id,
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
            application_id: None,
            plan_revision: None,
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
        artifact: artifact_id,
    }
}

fn wait_until(predicate: impl FnMut() -> bool) {
    wait_until_within(predicate, Duration::from_secs(2));
}

fn wait_until_within(mut predicate: impl FnMut() -> bool, budget: Duration) {
    let deadline = Instant::now() + budget;
    while !predicate() {
        assert!(Instant::now() < deadline, "condition did not become true");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn execute_sql(path: &Path, sql: &str) {
    Connection::open(path).unwrap().execute_batch(sql).unwrap();
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut output, byte| {
        write!(output, "{byte:02x}").unwrap();
        output
    })
}

/// Seeds one owner publication prefix (content object, event, admission and
/// durability receipts) into the Semantic authority. Construction copied from
/// the coordinator's semantic convergence fixture
/// (`semantic_convergence.rs::seed_semantic_authority`, the fixture included
/// by `semantic_pending_restart_scan.rs`), with the byte patterns derived
/// from `seed` so several plans can coexist in one authority.
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

/// Leaves one durable Semantic commit plan in `Planned`: seed the owner
/// prefix, seal a write set with one durable semantic append, draw a commit
/// permit, and plan the commit. Construction copied from the coordinator's
/// semantic convergence fixture (`semantic_convergence.rs::prepare` with
/// `with_effect = false`, the fixture included by
/// `semantic_pending_restart_scan.rs`), with every identity derived from
/// `seed` so artifact and semantic plans coexist in one task authority.
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
            application_id: None,
            plan_revision: None,
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
            nlos_task::ParticipantRegistryBinding {
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

#[test]
fn artifact_pending_plan_converges_with_unchanged_health_and_quiescent_semantic_domain() {
    let databases = TestAuthorities::new("dual-domain-artifact-regression");
    let pending = prepare_single(&databases, 0xe1);
    let (tasks, artifacts) = databases.open();
    let tasks = Arc::new(tasks);
    let artifacts = Arc::new(artifacts);
    let mut worker = TaskAuthorityCommitRecoveryWorker::start(
        Arc::clone(&tasks),
        Arc::clone(&artifacts),
        RecoveryWorkerConfig {
            scan_limit: 16,
            poll_interval: Duration::from_secs(10),
            max_backoff: Duration::from_secs(10),
            failure_threshold: 3,
        },
    )
    .unwrap();

    wait_until(|| {
        tasks
            .inspect_artifact_commit_plan(pending.plan)
            .is_ok_and(|plan| plan.state == ArtifactCommitPlanState::Finalized)
            && worker.health().completed_cycles >= 1
    });

    // Existing artifact-domain projection: identical to the single-domain
    // worker for the same convergence scenario.
    let running = worker.health();
    assert_eq!(running.state, RecoveryWorkerState::Running);
    assert!(running.completed_cycles >= 1);
    assert_eq!(running.total_inspected, 1);
    assert_eq!(running.total_finalized, 1);
    assert_eq!(running.consecutive_failed_cycles, 0);
    assert_eq!(running.retry_delay, None);
    assert!(running.last_failures.is_empty());
    assert_eq!(running.durable_retrying, 0);
    assert_eq!(running.durable_escalated, 0);
    assert_eq!(running.durable_unacknowledged_escalated, 0);
    assert_eq!(running.durable_resolved, 0);

    // New semantic-domain projection: `start` supplies no semantic
    // authority, so the semantic half stays quiescent by construction.
    assert_eq!(running.semantic_total_inspected, 0);
    assert_eq!(running.semantic_total_finalized, 0);
    assert_eq!(running.semantic_consecutive_failed_cycles, 0);
    assert_eq!(running.semantic_durable_retrying, 0);
    assert_eq!(running.semantic_durable_escalated, 0);
    assert_eq!(running.semantic_durable_unacknowledged_escalated, 0);
    assert_eq!(running.semantic_durable_resolved, 0);
    assert!(!running.semantic_domain_faulted);
    assert!(!running.artifact_domain_faulted);

    let stop_started = Instant::now();
    worker.stop();
    assert!(stop_started.elapsed() < Duration::from_secs(1));
    let stopped = worker.health();
    assert_eq!(stopped.state, RecoveryWorkerState::Stopped);
    assert!(!stopped.semantic_domain_faulted);
    assert!(!stopped.artifact_domain_faulted);
    assert_eq!(
        tasks.inspect_task(pending.task).unwrap().head_commit_seq,
        1,
        "worker converged the artifact commit plan"
    );
    assert_eq!(
        artifacts
            .resolve_head(pending.artifact, u64::MAX)
            .unwrap()
            .expect("worker published the artifact head")
            .revision,
        1
    );
}

#[test]
fn recovery_worker_health_defaults_keep_semantic_domain_quiescent() {
    let health = RecoveryWorkerHealth::default();
    // Existing defaults unchanged.
    assert_eq!(health.state, RecoveryWorkerState::Starting);
    assert_eq!(health.completed_cycles, 0);
    assert_eq!(health.total_inspected, 0);
    assert_eq!(health.total_finalized, 0);
    assert_eq!(health.consecutive_failed_cycles, 0);
    assert_eq!(health.retry_delay, None);
    assert!(health.last_failures.is_empty());
    // New semantic-domain fields default to zero/false.
    assert_eq!(health.semantic_total_inspected, 0);
    assert_eq!(health.semantic_total_finalized, 0);
    assert_eq!(health.semantic_consecutive_failed_cycles, 0);
    assert_eq!(health.semantic_durable_retrying, 0);
    assert_eq!(health.semantic_durable_escalated, 0);
    assert_eq!(health.semantic_durable_unacknowledged_escalated, 0);
    assert_eq!(health.semantic_durable_resolved, 0);
    assert!(!health.semantic_domain_faulted);
    assert!(!health.artifact_domain_faulted);
}

#[test]
fn worker_converges_pending_semantic_plan_without_caller() {
    // Spec §5 core scenario: the planning caller is gone, nobody issues a
    // manual converge, and the worker alone drives the durable pending
    // Semantic plan to its unique terminal state.
    let databases = TestAuthorities::new("dual-domain-semantic-restart");
    let pending = prepare_semantic(&databases, 0x51);
    let (tasks, artifacts) = databases.open();
    let semantic = databases.open_semantic();
    let tasks = Arc::new(tasks);
    let mut worker = TaskAuthorityCommitRecoveryWorker::start_with_semantic_authority(
        Arc::clone(&tasks),
        Arc::new(artifacts),
        Some(Arc::new(semantic)),
        RecoveryWorkerConfig {
            scan_limit: 16,
            poll_interval: Duration::from_millis(10),
            max_backoff: Duration::from_millis(40),
            failure_threshold: 3,
        },
    )
    .unwrap();

    wait_until_within(
        || worker.health().semantic_total_finalized >= 1,
        Duration::from_secs(10),
    );

    let progress = tasks
        .inspect_semantic_commit_progress(pending.plan)
        .unwrap();
    assert_eq!(progress.plan.state, SemanticCommitPlanState::Finalized);
    assert_eq!(progress.publications.len(), 1);
    assert_eq!(
        tasks.inspect_task(pending.task).unwrap().head_commit_seq,
        1,
        "worker finalized the Semantic commit"
    );

    let running = worker.health();
    assert_eq!(running.state, RecoveryWorkerState::Running);
    assert_eq!(running.total_inspected, 0);
    assert_eq!(running.total_finalized, 0);
    assert_eq!(running.semantic_total_inspected, 1);
    assert_eq!(running.semantic_total_finalized, 1);
    assert_eq!(running.semantic_consecutive_failed_cycles, 0);
    assert!(running.last_failures.is_empty());
    assert_eq!(running.retry_delay, None);
    // Clean convergence never opens a semantic ledger row, so every durable
    // semantic gauge stays zero. `semantic_durable_resolved` only moves on
    // the failure path: finalizing a plan that has an open ledger row flips
    // it to `Resolved` inside the same TaskAuthority finalize transaction
    // (the SEM-RECOV-004 linkage, landed in the nlos-task lane). That loop
    // is asserted by the dedicated escalate->resume->resolve composite test
    // below; this scenario substitutes the zero-gauge assertion because its
    // write set never opens a ledger row.
    assert_eq!(running.semantic_durable_retrying, 0);
    assert_eq!(running.semantic_durable_escalated, 0);
    assert_eq!(running.semantic_durable_unacknowledged_escalated, 0);
    assert_eq!(running.semantic_durable_resolved, 0);
    assert!(!running.semantic_domain_faulted);
    assert!(!running.artifact_domain_faulted);

    worker.stop();
    assert_eq!(worker.health().state, RecoveryWorkerState::Stopped);
}

#[test]
fn both_domains_converge_in_one_worker() {
    let databases = TestAuthorities::new("dual-domain-both-converge");
    let artifact_pending = prepare_single(&databases, 0x61);
    let semantic_pending = prepare_semantic(&databases, 0x64);
    let (tasks, artifacts) = databases.open();
    let semantic = databases.open_semantic();
    let tasks = Arc::new(tasks);
    let artifacts = Arc::new(artifacts);
    let mut worker = TaskAuthorityCommitRecoveryWorker::start_with_semantic_authority(
        Arc::clone(&tasks),
        Arc::clone(&artifacts),
        Some(Arc::new(semantic)),
        RecoveryWorkerConfig {
            scan_limit: 16,
            poll_interval: Duration::from_millis(10),
            max_backoff: Duration::from_millis(40),
            failure_threshold: 3,
        },
    )
    .unwrap();

    wait_until_within(
        || {
            tasks
                .inspect_artifact_commit_plan(artifact_pending.plan)
                .is_ok_and(|plan| plan.state == ArtifactCommitPlanState::Finalized)
                && tasks
                    .inspect_semantic_commit_progress(semantic_pending.plan)
                    .is_ok_and(|progress| progress.plan.state == SemanticCommitPlanState::Finalized)
        },
        Duration::from_secs(10),
    );

    // 计数器与 plan 终态在同一 worker 迭代内先后落定——对计数器同样做有界
    // 轮询，消除「plan 已 Finalized 而计数尚未递增」的读侧竞态（慢 runner
    // 上曾真实命中：Windows CI run 35522355445 两条失败均此形态）。
    wait_until_within(
        || {
            let h = worker.health();
            h.state == RecoveryWorkerState::Running
                && h.total_inspected == 1
                && h.total_finalized == 1
                && h.semantic_total_inspected == 1
                && h.semantic_total_finalized == 1
        },
        Duration::from_secs(10),
    );

    let running = worker.health();
    assert_eq!(running.state, RecoveryWorkerState::Running);
    assert_eq!(running.total_inspected, 1);
    assert_eq!(running.total_finalized, 1);
    assert_eq!(running.semantic_total_inspected, 1);
    assert_eq!(running.semantic_total_finalized, 1);
    assert_eq!(running.consecutive_failed_cycles, 0);
    assert_eq!(running.semantic_consecutive_failed_cycles, 0);
    assert_eq!(running.retry_delay, None);
    assert!(running.last_failures.is_empty());
    assert_eq!(running.durable_retrying, 0);
    assert_eq!(running.durable_escalated, 0);
    assert_eq!(running.semantic_durable_retrying, 0);
    assert_eq!(running.semantic_durable_escalated, 0);
    assert!(!running.semantic_domain_faulted);
    assert!(!running.artifact_domain_faulted);
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
        artifacts
            .resolve_head(artifact_pending.artifact, u64::MAX)
            .unwrap()
            .expect("worker published the artifact head")
            .revision,
        1
    );
    worker.stop();
}

#[test]
fn semantic_domain_fault_is_isolated_from_artifact_convergence() {
    let databases = TestAuthorities::new("dual-domain-semantic-fault");
    let artifact_pending = prepare_single(&databases, 0x71);
    let semantic_pending = prepare_semantic(&databases, 0x74);
    // Persistent semantic-path storage failure: every Semantic plan-state
    // transition is rejected and the semantic recovery ledger cannot open a
    // row, so each cycle ends with a TaskAuthority infrastructure failure on
    // the semantic half only. The artifact-domain tables are untouched.
    execute_sql(
        &databases.task_path,
        "CREATE TRIGGER keep_semantic_plan_unconverged
         BEFORE UPDATE ON task_semantic_commit_plans
         WHEN NEW.plan_state != OLD.plan_state
         BEGIN SELECT RAISE(ABORT, 'persistent semantic storage failure'); END;
         CREATE TRIGGER semantic_ledger_unavailable
         BEFORE INSERT ON task_semantic_recovery
         BEGIN SELECT RAISE(ABORT, 'semantic ledger unavailable'); END;",
    );
    let (tasks, artifacts) = databases.open();
    let semantic = databases.open_semantic();
    let tasks = Arc::new(tasks);
    let mut worker = TaskAuthorityCommitRecoveryWorker::start_with_semantic_authority(
        Arc::clone(&tasks),
        Arc::new(artifacts),
        Some(Arc::new(semantic)),
        RecoveryWorkerConfig {
            scan_limit: 16,
            poll_interval: Duration::from_millis(10),
            max_backoff: Duration::from_millis(40),
            failure_threshold: 3,
        },
    )
    .unwrap();

    // Domain isolation core assertion: the artifact plan converges while the
    // semantic half keeps failing and then faults.
    wait_until_within(
        || {
            tasks
                .inspect_artifact_commit_plan(artifact_pending.plan)
                .is_ok_and(|plan| plan.state == ArtifactCommitPlanState::Finalized)
                && worker.health().semantic_domain_faulted
        },
        Duration::from_secs(10),
    );

    let isolated = worker.health();
    assert_eq!(
        isolated.state,
        RecoveryWorkerState::Running,
        "a faulted semantic domain must not terminate the worker thread"
    );
    assert!(isolated.semantic_domain_faulted);
    assert!(!isolated.artifact_domain_faulted);
    // Pinned observability decision: the counter that faulted the domain is
    // NOT cleared when the bit is set (see worker.rs `account_cycle`). It
    // stays at the threshold as the health-surface evidence of why the
    // sticky bit is set.
    assert_eq!(isolated.semantic_consecutive_failed_cycles, 3);
    assert_eq!(isolated.consecutive_failed_cycles, 0);
    assert!(isolated.semantic_total_inspected >= 3);
    assert_eq!(isolated.semantic_total_finalized, 0);
    assert_eq!(isolated.total_finalized, 1);
    assert_eq!(isolated.retry_delay, None);
    assert_eq!(isolated.semantic_durable_retrying, 0);
    assert_eq!(isolated.semantic_durable_escalated, 0);
    assert_eq!(isolated.semantic_durable_unacknowledged_escalated, 0);
    assert_eq!(isolated.semantic_durable_resolved, 0);

    let progress = tasks
        .inspect_semantic_commit_progress(semantic_pending.plan)
        .unwrap();
    assert_eq!(progress.plan.state, SemanticCommitPlanState::Planned);
    assert!(
        tasks
            .inspect_semantic_recovery(semantic_pending.plan)
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
    worker.stop();
    assert_eq!(worker.health().state, RecoveryWorkerState::Stopped);
}

#[test]
fn semantic_authority_failures_escalate_ledger_without_faulting_domain() {
    let _serialization = fault_lock();
    nlos_store_fault::register(VFS_NAME).unwrap();
    nlos_store_fault::disarm();
    let _fault_guard = FaultDisarmGuard;
    let databases = TestAuthorities::new("dual-domain-semantic-owner-failure");
    let pending = prepare_semantic(&databases, 0x82);
    let (tasks, artifacts) = databases.open();
    let semantic = Arc::new(
        SemanticAuthority::open_with_vfs(&databases.semantic_root, Some(VFS_NAME)).unwrap(),
    );
    // Fault construction copied from semantic_convergence_fault_injection.rs:
    // every write through the fault VFS fails with SQLITE_IOERR, so the
    // worker's owner publication fails while its TaskAuthority stays healthy.
    // These are plan-level failures: they belong in the durable semantic
    // ledger (source SemanticAuthority), not in the infrastructure budget.
    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::IoErr,
    });
    let tasks = Arc::new(tasks);
    let mut worker = TaskAuthorityCommitRecoveryWorker::start_with_semantic_authority(
        Arc::clone(&tasks),
        Arc::new(artifacts),
        Some(Arc::clone(&semantic)),
        RecoveryWorkerConfig {
            scan_limit: 16,
            poll_interval: Duration::from_millis(10),
            max_backoff: Duration::from_millis(40),
            failure_threshold: 3,
        },
    )
    .unwrap();

    wait_until_within(
        || worker.health().semantic_durable_escalated == 1,
        Duration::from_secs(10),
    );

    let escalated = worker.health();
    assert_eq!(escalated.state, RecoveryWorkerState::Running);
    assert!(!escalated.semantic_domain_faulted);
    assert!(!escalated.artifact_domain_faulted);
    assert_eq!(escalated.semantic_consecutive_failed_cycles, 0);
    assert_eq!(escalated.semantic_durable_unacknowledged_escalated, 1);
    assert_eq!(escalated.semantic_total_finalized, 0);
    assert!(escalated.semantic_total_inspected >= 8);

    let record = tasks
        .inspect_semantic_recovery(pending.plan)
        .unwrap()
        .expect("durable semantic ledger row");
    assert_eq!(record.state, SemanticRecoveryState::Escalated);
    assert_eq!(
        record.last_source,
        SemanticRecoveryFailureSource::SemanticAuthority
    );
    assert_eq!(record.consecutive_failures, 8);
    assert_eq!(record.total_failures, 8);
    assert_eq!(record.next_retry_at_ms, None);

    let progress = tasks
        .inspect_semantic_commit_progress(pending.plan)
        .unwrap();
    assert_eq!(
        progress.plan.state,
        SemanticCommitPlanState::Publishing,
        "authorize succeeded; the owner publication never landed"
    );
    assert!(progress.publications.is_empty());
    worker.stop();
}

#[test]
fn escalated_semantic_recovery_resumes_converges_and_resolves_ledger() {
    // W26 core claim, failure half: the worker drives the full recovery
    // loop — durable failure accounting escalates the ledger, an operator
    // resumes the terminal row by CAS, the worker reconverges the plan, and
    // the finalize->resolve linkage (SEM-RECOV-004) lands the ledger in
    // `Resolved`.
    let _serialization = fault_lock();
    nlos_store_fault::register(VFS_NAME).unwrap();
    nlos_store_fault::disarm();
    let _fault_guard = FaultDisarmGuard;
    let databases = TestAuthorities::new("dual-domain-semantic-resume-loop");
    let pending = prepare_semantic(&databases, 0xa2);
    let (tasks, artifacts) = databases.open();
    let semantic = Arc::new(
        SemanticAuthority::open_with_vfs(&databases.semantic_root, Some(VFS_NAME)).unwrap(),
    );
    // Same fault construction as the escalation test above: owner-side
    // writes fail, so failures are plan-level and belong in the ledger.
    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::IoErr,
    });
    let tasks = Arc::new(tasks);
    let mut worker = TaskAuthorityCommitRecoveryWorker::start_with_semantic_authority(
        Arc::clone(&tasks),
        Arc::new(artifacts),
        Some(Arc::clone(&semantic)),
        RecoveryWorkerConfig {
            scan_limit: 16,
            poll_interval: Duration::from_millis(10),
            max_backoff: Duration::from_millis(40),
            failure_threshold: 3,
        },
    )
    .unwrap();

    // Failure accounting half: the ledger row reaches its `Escalated`
    // terminal state (asserted on the ledger itself, not just the gauge;
    // the row is stable afterwards because escalation leaves the due scan).
    wait_until_within(
        || {
            tasks
                .inspect_semantic_recovery(pending.plan)
                .unwrap()
                .is_some_and(|record| record.state == SemanticRecoveryState::Escalated)
        },
        Duration::from_secs(10),
    );
    let escalated = tasks
        .inspect_semantic_recovery(pending.plan)
        .unwrap()
        .expect("escalated ledger row");
    assert_eq!(escalated.next_retry_at_ms, None);

    // Repair, then the manual resume half: the CAS on `total_failures`
    // requeues the escalated row as due for the next worker scan.
    nlos_store_fault::disarm();
    tasks
        .resume_semantic_recovery(SemanticRecoveryResumeRequest {
            plan_id: pending.plan,
            expected_total_failures: escalated.total_failures,
            resumed_at_ms: escalated.last_failed_at_ms,
        })
        .unwrap();

    // Automatic convergence half: the worker alone finalizes the plan and
    // the same finalize transaction resolves the ledger row.
    wait_until_within(
        || worker.health().semantic_total_finalized >= 1,
        Duration::from_secs(10),
    );
    assert_eq!(
        tasks
            .inspect_semantic_commit_progress(pending.plan)
            .unwrap()
            .plan
            .state,
        SemanticCommitPlanState::Finalized
    );
    let resolved = tasks
        .inspect_semantic_recovery(pending.plan)
        .unwrap()
        .expect("resolved ledger row");
    assert_eq!(resolved.state, SemanticRecoveryState::Resolved);
    assert_eq!(
        resolved.total_failures, escalated.total_failures,
        "resolution preserves the durable failure history"
    );
    let health = worker.health();
    assert_eq!(health.semantic_durable_resolved, 1);
    assert_eq!(health.semantic_durable_escalated, 0);
    assert_eq!(health.semantic_durable_unacknowledged_escalated, 0);
    worker.stop();
}

#[test]
#[allow(clippy::too_many_lines)]
fn mixed_semantic_infra_and_artifact_plan_failures_use_separate_budgets() {
    let databases = TestAuthorities::new("dual-domain-mixed-failure");
    let artifact_pending = prepare_single(&databases, 0x91);
    let semantic_pending = prepare_semantic(&databases, 0x94);
    // Semantic half: infrastructure failures (state transitions and ledger
    // rows both rejected). Artifact half: plan-level failures only (the
    // terminal finalize update is rejected, so the failure is recorded in
    // the artifact ledger instead of consuming the worker budget).
    execute_sql(
        &databases.task_path,
        &format!(
            "CREATE TRIGGER keep_semantic_plan_unconverged
             BEFORE UPDATE ON task_semantic_commit_plans
             WHEN NEW.plan_state != OLD.plan_state
             BEGIN SELECT RAISE(ABORT, 'persistent semantic storage failure'); END;
             CREATE TRIGGER semantic_ledger_unavailable
             BEFORE INSERT ON task_semantic_recovery
             BEGIN SELECT RAISE(ABORT, 'semantic ledger unavailable'); END;
             CREATE TRIGGER keep_artifact_plan_failing
             BEFORE UPDATE ON task_artifact_commit_plans
             WHEN NEW.plan_state = 3 AND NEW.plan_id = X'{plan}'
             BEGIN SELECT RAISE(ABORT, 'persistent artifact worker failure'); END;",
            plan = hex(artifact_pending.plan.as_bytes())
        ),
    );
    let (tasks, artifacts) = databases.open();
    let semantic = databases.open_semantic();
    let tasks = Arc::new(tasks);
    let mut worker = TaskAuthorityCommitRecoveryWorker::start_with_semantic_authority(
        Arc::clone(&tasks),
        Arc::new(artifacts),
        Some(Arc::new(semantic)),
        RecoveryWorkerConfig {
            scan_limit: 16,
            poll_interval: Duration::from_millis(50),
            max_backoff: Duration::from_secs(5),
            failure_threshold: 6,
        },
    )
    .unwrap();

    // Mixed phase: the semantic half burns its infrastructure budget while
    // the artifact half only accumulates durable plan-level failures. The
    // worker backs off on the semantic infrastructure delay and never
    // touches the artifact infrastructure budget.
    wait_until_within(
        || {
            let health = worker.health();
            health.semantic_consecutive_failed_cycles >= 1
                && !health.semantic_domain_faulted
                && tasks
                    .inspect_artifact_recovery(artifact_pending.plan)
                    .is_ok_and(|record| record.is_some_and(|record| record.total_failures >= 1))
        },
        Duration::from_secs(10),
    );
    let mixed = worker.health();
    assert_eq!(mixed.state, RecoveryWorkerState::BackingOff);
    assert!(mixed.retry_delay.is_some());
    assert_eq!(
        mixed.consecutive_failed_cycles, 0,
        "plan-level artifact failures must not consume the artifact budget"
    );
    assert!(mixed.semantic_consecutive_failed_cycles >= 1);
    assert!(!mixed.semantic_domain_faulted);
    assert!(!mixed.artifact_domain_faulted);

    // Terminal combination: the semantic domain faults (sticky) while the
    // artifact plan escalates in its own durable ledger.
    wait_until_within(
        || {
            let health = worker.health();
            health.semantic_domain_faulted && health.durable_escalated == 1
        },
        Duration::from_secs(10),
    );
    let settled = worker.health();
    assert_eq!(settled.state, RecoveryWorkerState::Running);
    assert_eq!(settled.retry_delay, None);
    assert_eq!(settled.semantic_consecutive_failed_cycles, 6);
    assert_eq!(settled.consecutive_failed_cycles, 0);
    let artifact_record = tasks
        .inspect_artifact_recovery(artifact_pending.plan)
        .unwrap()
        .expect("durable artifact ledger row");
    assert_eq!(artifact_record.state, ArtifactRecoveryState::Escalated);
    assert_eq!(artifact_record.consecutive_failures, 6);
    let semantic_progress = tasks
        .inspect_semantic_commit_progress(semantic_pending.plan)
        .unwrap();
    assert_eq!(
        semantic_progress.plan.state,
        SemanticCommitPlanState::Planned
    );
    assert!(
        tasks
            .inspect_semantic_recovery(semantic_pending.plan)
            .unwrap()
            .is_none()
    );

    // Repair and resume: the artifact domain converges while the semantic
    // domain stays faulted.
    execute_sql(
        &databases.task_path,
        "DROP TRIGGER keep_artifact_plan_failing;",
    );
    tasks
        .resume_artifact_recovery(ArtifactRecoveryResumeRequest {
            plan_id: artifact_pending.plan,
            expected_total_failures: artifact_record.total_failures,
            resumed_at_ms: artifact_record.last_failed_at_ms,
        })
        .unwrap();
    wait_until_within(
        || {
            tasks
                .inspect_artifact_commit_plan(artifact_pending.plan)
                .is_ok_and(|plan| plan.state == ArtifactCommitPlanState::Finalized)
        },
        Duration::from_secs(10),
    );
    wait_until_within(
        || {
            let h = worker.health();
            h.state == RecoveryWorkerState::Running && h.total_finalized == 1
        },
        Duration::from_secs(10),
    );
    let recovered = worker.health();
    assert_eq!(recovered.state, RecoveryWorkerState::Running);
    assert_eq!(recovered.total_finalized, 1);
    assert!(recovered.semantic_domain_faulted);
    assert!(!recovered.artifact_domain_faulted);
    assert_eq!(
        tasks
            .inspect_task(artifact_pending.task)
            .unwrap()
            .head_commit_seq,
        1
    );
    worker.stop();
}
