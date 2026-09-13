#![allow(deprecated)] // Ladder constructors deprecated in favor of the *_with_authorities_struct entries.
//! W26-002 dual-domain worker regression: splitting `durable_cycle` into an
//! artifact half plus a semantic stub must leave every existing artifact
//! `RecoveryWorkerHealth` behavior bit-for-bit unchanged, while the new
//! semantic-domain projection stays quiescent (all zero/false) until the
//! semantic scan is wired in a follow-up lane.
//!
//! This is local recovery evidence, not a distributed atomic-commit claim.

use std::fs;
use std::path::PathBuf;
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
use nlos_task::{
    ArtifactCommitPlanState, ArtifactPublicationExpectation, AttemptSpec, PermitDecision,
    PermitRequest, PlanArtifactCommitRequest, SnapshotBundle, SqliteTaskAuthority, TaskSpec,
    artifact_publication_plan_root, empty_effect_history_root,
};
use nlos_types::{
    ArtifactId, CancellationScopeId, Generation, IdempotencyKey, TaskAttemptId, TaskId,
    TaskSnapshotId,
};

static NEXT_DATABASE: AtomicU64 = AtomicU64::new(0);

struct TestAuthorities {
    root: PathBuf,
    task_path: PathBuf,
    artifact_root: PathBuf,
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
            root,
        }
    }

    fn open(&self) -> (SqliteTaskAuthority, ArtifactStore) {
        (
            SqliteTaskAuthority::open(&self.task_path).unwrap(),
            ArtifactStore::open(&self.artifact_root).unwrap(),
        )
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

fn wait_until(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !predicate() {
        assert!(Instant::now() < deadline, "condition did not become true");
        std::thread::sleep(Duration::from_millis(5));
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

    // New semantic-domain projection: the stub half keeps every field zero.
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
