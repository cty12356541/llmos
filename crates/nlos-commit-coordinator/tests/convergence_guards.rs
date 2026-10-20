//! Guards for the convergence-loop hazards reconciled from registry 48:
//! the per-invocation step budget that turns an unbounded spin into a
//! typed not-converged error (#10), the pending scan that attempts every
//! plan before returning the first failure (#11), the terminal-state step
//! that answers idempotently without re-issuing the finalize write intent
//! (audit 41 D2), and the cooperative stop flag that interrupts a worker
//! scan between plans (#13).

#![allow(deprecated)] // Ladder constructors deprecated in favor of the *_with_authorities_struct entries.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use nlos_artifact::{
    ArtifactStore, ContentDigest, CreateArtifactSpec, StageRevisionRequest, staging_id_for,
};
use nlos_commit_coordinator::{
    ArtifactCommitCoordinator, CONVERGE_MAX_STEPS, ConvergeArtifactCommitRequest, ConvergeStep,
    CoordinatorError, RecoveryWorkerConfig, RecoveryWorkerState, TaskAuthorityCommitRecoveryWorker,
};
use nlos_task::{
    ArtifactCommitPlanId, ArtifactCommitPlanState, ArtifactPublicationExpectation, AttemptSpec,
    PermitDecision, PermitRequest, PlanArtifactCommitRequest, SnapshotBundle, SqliteTaskAuthority,
    TaskSpec, artifact_publication_plan_root, empty_effect_history_root,
};
use nlos_types::{
    ArtifactId, CancellationScopeId, Generation, IdempotencyKey, TaskAttemptId, TaskId,
    TaskSnapshotId,
};

static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

struct TestAuthorities {
    root: PathBuf,
    task_path: PathBuf,
    artifact_root: PathBuf,
}

impl TestAuthorities {
    fn new(name: &str) -> Self {
        let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "nlos-coordinator-guards-{name}-{}-{sequence}",
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

#[derive(Clone, Copy)]
struct PreparedPlan {
    plan: ArtifactCommitPlanId,
    task: TaskId,
}

fn prefixed16(prefix: u8, index: usize) -> [u8; 16] {
    let mut bytes = [prefix; 16];
    bytes[12..].copy_from_slice(&u32::try_from(index).unwrap().to_be_bytes());
    bytes
}

fn prefixed32(prefix: u8, index: usize) -> [u8; 32] {
    let mut bytes = [prefix; 32];
    bytes[28..].copy_from_slice(&u32::try_from(index).unwrap().to_be_bytes());
    bytes
}

fn create_artifact(store: &ArtifactStore, artifact_id: ArtifactId, key: IdempotencyKey) {
    store
        .create_artifact(CreateArtifactSpec {
            artifact_id,
            idempotency_key: key,
            content_type: "application/octet-stream".to_string(),
            application_id: None,
            owner: None,
            created_at_ms: 1_000,
        })
        .unwrap();
}

/// One artifact-only plan whose expectations are addressed by `prefix`-ed
/// ids, so many plans (or many expectations) coexist in one database.
fn prepare_plan(
    databases: &TestAuthorities,
    prefix: u8,
    planned_at_ms: i64,
    expectation_count: usize,
) -> PreparedPlan {
    let (tasks, artifacts) = databases.open();
    let task_id = TaskId::from_bytes(prefixed16(prefix, 0));
    let attempt_id = TaskAttemptId::from_bytes(prefixed16(prefix, 1));
    tasks
        .register_task(TaskSpec {
            task_id,
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
            application_id: None,
            plan_revision: None,
        })
        .unwrap();
    tasks
        .register_attempt(AttemptSpec {
            task_id,
            attempt_id,
            attempt_generation: Generation::INITIAL,
            snapshot: SnapshotBundle {
                snapshot_id: TaskSnapshotId::from_bytes(prefixed16(prefix, 2)),
                snapshot_digest: prefixed32(prefix, 2),
                expected_head_commit_seq: 0,
                effect_history_root: empty_effect_history_root(),
                retry_fence_epoch: 0,
            },
            cancellation_scope_id: CancellationScopeId::from_bytes(prefixed16(prefix, 3)),
            cancellation_generation: Generation::INITIAL,
            idempotency_key: IdempotencyKey::from_bytes(prefixed16(prefix, 4)),
            registered_at_ms: 2_000,
        })
        .unwrap();

    let mut expectations = Vec::with_capacity(expectation_count);
    for index in 0..expectation_count {
        let artifact_id = ArtifactId::from_bytes(prefixed16(prefix, 16 + index));
        let stage_key = IdempotencyKey::from_bytes(prefixed16(prefix, 16 + index));
        let bytes = prefixed32(prefix, index);
        create_artifact(&artifacts, artifact_id, stage_key);
        expectations.push(ArtifactPublicationExpectation {
            staging_id: staging_id_for(artifact_id, stage_key).into_bytes(),
            artifact_id,
            target_revision: 1,
            digest: ContentDigest::of_bytes(&bytes).into_bytes(),
            size_bytes: u64::try_from(bytes.len()).unwrap(),
        });
    }
    let write_set_root = artifact_publication_plan_root(&expectations).unwrap();
    let PermitDecision::Issued(permit) = tasks
        .request_commit_permit(PermitRequest {
            task_id,
            attempt_id,
            attempt_generation: Generation::INITIAL,
            write_set_root,
            planned_effects: Vec::new(),
            idempotency_key: IdempotencyKey::from_bytes(prefixed16(prefix, 5)),
            valid_until_ms: i64::MAX,
            requested_at_ms: 3_000,
        })
        .unwrap()
    else {
        panic!("expected issued permit");
    };
    for (index, expectation) in expectations.iter().enumerate() {
        artifacts
            .stage_revision(StageRevisionRequest {
                artifact_id: expectation.artifact_id,
                expected_head_revision: 0,
                bytes: &prefixed32(prefix, index),
                task_id,
                permit_id: permit.permit_id,
                write_set_root: ContentDigest::from_bytes(write_set_root),
                idempotency_key: IdempotencyKey::from_bytes(prefixed16(prefix, 16 + index)),
                created_at_ms: 3_500,
            })
            .unwrap();
    }
    let plan_id = tasks
        .plan_artifact_commit(PlanArtifactCommitRequest {
            task_id,
            attempt_id,
            attempt_generation: Generation::INITIAL,
            permit_id: permit.permit_id,
            expectations,
            idempotency_key: IdempotencyKey::from_bytes(prefixed16(prefix, 6)),
            planned_at_ms,
        })
        .unwrap()
        .record()
        .plan_id;
    PreparedPlan {
        plan: plan_id,
        task: task_id,
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut output, byte| {
        let _ = write!(output, "{byte:02x}");
        output
    })
}

fn execute_sql(path: &PathBuf, sql: &str) {
    rusqlite::Connection::open(path)
        .unwrap()
        .execute_batch(sql)
        .unwrap();
}

fn wait_until(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !predicate() {
        assert!(Instant::now() < deadline, "condition did not become true");
        thread::sleep(Duration::from_millis(5));
    }
}

/// Registry 48 #10: a plan with more expectations than the step budget
/// gets a typed not-converged error instead of an unbounded loop, keeps
/// its durable prefix, and finishes on the next call.
#[test]
fn converge_step_budget_returns_typed_error_and_resumes_from_prefix() {
    let databases = TestAuthorities::new("step-budget");
    let expectation_count = CONVERGE_MAX_STEPS + 44;
    let pending = prepare_plan(&databases, 0x30, 4_000, expectation_count);

    let (tasks, artifacts) = databases.open();
    let coordinator = ArtifactCommitCoordinator::new(&tasks, &artifacts);
    match coordinator.converge(ConvergeArtifactCommitRequest {
        plan_id: pending.plan,
        now_ms: 5_000,
    }) {
        Err(CoordinatorError::IterationLimitExceeded { steps }) => {
            assert_eq!(steps, CONVERGE_MAX_STEPS);
        }
        other => panic!("expected the step budget error, got {other:?}"),
    }
    let prefix = tasks
        .inspect_artifact_commit_progress(pending.plan)
        .unwrap();
    assert_eq!(prefix.plan.state, ArtifactCommitPlanState::Publishing);
    // One authorize step plus 255 publication steps stay durably recorded.
    assert_eq!(prefix.publications.len(), CONVERGE_MAX_STEPS - 1);

    let receipt = coordinator
        .converge(ConvergeArtifactCommitRequest {
            plan_id: pending.plan,
            now_ms: 6_000,
        })
        .unwrap();
    assert_eq!(receipt.artifact_publications.len(), expectation_count);
    assert_eq!(receipt.task_receipt.new_head_commit_seq, 1);
    assert_eq!(
        tasks
            .inspect_artifact_commit_plan(pending.plan)
            .unwrap()
            .state,
        ArtifactCommitPlanState::Finalized
    );
}

/// Registry 48 #11: `converge_pending` attempts every plan in the
/// snapshot; the first typed failure is returned only after the later
/// plans have converged, so one bad plan cannot starve the rest.
#[test]
fn pending_scan_attempts_every_plan_before_returning_first_error() {
    let databases = TestAuthorities::new("pending-full-pass");
    // Earlier `planned_at_ms` puts the failing plan first in the scan
    // ordering (`ORDER BY created_at_ms, plan_id`).
    let failing = prepare_plan(&databases, 0x40, 4_000, 1);
    let healthy = prepare_plan(&databases, 0x50, 4_001, 1);
    execute_sql(
        &databases.task_path,
        &format!(
            "CREATE TRIGGER fail_first_plan_finalize
             BEFORE UPDATE ON task_artifact_commit_plans
             WHEN NEW.plan_state = 3 AND NEW.plan_id = X'{}'
             BEGIN SELECT RAISE(ABORT, 'injected first-plan failure'); END;",
            hex(failing.plan.as_bytes())
        ),
    );

    {
        let (tasks, artifacts) = databases.open();
        let error = ArtifactCommitCoordinator::new(&tasks, &artifacts)
            .converge_pending(16, 6_000)
            .unwrap_err();
        assert!(matches!(error, CoordinatorError::Task(_)));
        // The failing plan was attempted and stayed short of terminal...
        assert_eq!(
            tasks
                .inspect_artifact_commit_plan(failing.plan)
                .unwrap()
                .state,
            ArtifactCommitPlanState::Ready
        );
        // ...while the later plan in the same snapshot still converged.
        assert_eq!(
            tasks
                .inspect_artifact_commit_plan(healthy.plan)
                .unwrap()
                .state,
            ArtifactCommitPlanState::Finalized
        );
        assert_eq!(tasks.inspect_task(healthy.task).unwrap().head_commit_seq, 1);
    }

    execute_sql(
        &databases.task_path,
        "DROP TRIGGER fail_first_plan_finalize;",
    );
    let (tasks, artifacts) = databases.open();
    let repaired = ArtifactCommitCoordinator::new(&tasks, &artifacts)
        .converge_pending(16, 7_000)
        .unwrap();
    assert_eq!(repaired.len(), 1);
    assert_eq!(repaired[0].task_receipt.task_id, failing.task);
}

/// Audit 41 D2: a `Finalized` plan answers `converge_one_step`
/// idempotently from the durable receipt without issuing a second
/// finalize write intent. Holding the Task write lock proves the path is
/// read-only: the pre-fix code opened a write transaction here and would
/// block on the lock.
#[test]
fn finalized_plan_answers_from_durable_receipt_without_finalize_write() {
    let databases = TestAuthorities::new("finalized-idempotent");
    let pending = prepare_plan(&databases, 0x60, 4_000, 1);
    let (tasks, artifacts) = databases.open();
    let coordinator = ArtifactCommitCoordinator::new(&tasks, &artifacts);
    let committed = coordinator
        .converge(ConvergeArtifactCommitRequest {
            plan_id: pending.plan,
            now_ms: 5_000,
        })
        .unwrap();
    assert_eq!(committed.task_receipt.new_head_commit_seq, 1);

    // Hold the TaskAuthority write lock for the whole block.
    let blocker = rusqlite::Connection::open(&databases.task_path).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE;").unwrap();
    let started = Instant::now();
    let step = coordinator
        .converge_one_step(ConvergeArtifactCommitRequest {
            plan_id: pending.plan,
            now_ms: 6_000,
        })
        .unwrap();
    assert!(
        matches!(step, ConvergeStep::AlreadyFinalized(ref receipt) if **receipt == committed),
        "terminal step must replay the durable receipt: {step:?}"
    );
    let replay = coordinator
        .converge(ConvergeArtifactCommitRequest {
            plan_id: pending.plan,
            now_ms: 7_000,
        })
        .unwrap();
    assert_eq!(replay, committed);
    // A read-only answer returns long before the five-second busy timeout
    // a write transaction would have burned on the held lock.
    assert!(started.elapsed() < Duration::from_secs(2));
    blocker.execute_batch("ROLLBACK;").unwrap();
}

/// Registry 48 #13: `stop()` publishes a cooperative flag that the worker
/// scan honors between plans. With the first plan's publication blocked on
/// a held Artifact write lock, the stop lands mid-scan: the first plan
/// still finishes (no half-plan abortion), the remaining plans are left
/// untouched for the next worker instance, and the joined stop returns.
#[test]
fn worker_stop_interrupts_scan_between_plans() {
    let databases = TestAuthorities::new("stop-between-plans");
    let plans: Vec<PreparedPlan> = (0..6_u32)
        .map(|index| {
            prepare_plan(
                &databases,
                u8::try_from(0x70 + index).expect("prefix fits u8"),
                4_000 + i64::from(index),
                1,
            )
        })
        .collect();

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

    // Block artifact publications for a short window: the worker commits
    // the first plan's authorization, then stalls inside its publication.
    let metadata_path = databases.artifact_root.join("metadata.db");
    let lock_thread = thread::spawn(move || {
        let blocker = rusqlite::Connection::open(metadata_path).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE;").unwrap();
        thread::sleep(Duration::from_millis(700));
        blocker.execute_batch("ROLLBACK;").unwrap();
    });

    wait_until(|| {
        tasks
            .inspect_artifact_commit_plan(plans[0].plan)
            .is_ok_and(|plan| plan.state == ArtifactCommitPlanState::Publishing)
    });

    let stop_started = Instant::now();
    worker.stop();
    let stop_elapsed = stop_started.elapsed();
    lock_thread.join().expect("artifact lock thread");
    assert!(stop_elapsed < Duration::from_secs(5));

    let health = worker.health();
    assert_eq!(health.state, RecoveryWorkerState::Stopped);
    assert!(!health.artifact_domain_faulted);
    // The stalled plan completed once the lock lifted: the stop never
    // aborts a plan mid-flight.
    assert_eq!(
        tasks
            .inspect_artifact_commit_plan(plans[0].plan)
            .unwrap()
            .state,
        ArtifactCommitPlanState::Finalized
    );
    // Every later plan was skipped by the between-plans stop check instead
    // of being converged by the remainder of the scan.
    for plan in &plans[1..] {
        assert_eq!(
            tasks.inspect_artifact_commit_plan(plan.plan).unwrap().state,
            ArtifactCommitPlanState::Planned,
            "stop must leave the unscanned plans for the next worker"
        );
    }
}
