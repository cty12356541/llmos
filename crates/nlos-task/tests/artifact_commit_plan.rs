//! B-TASK-006A: durable Artifact publication plan bound to `CommitPermit`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_task::{
    ArtifactCommitPlanDecision, ArtifactCommitPlanState, ArtifactPublicationExpectation,
    AttemptSpec, PermitDecision, PermitRequest, PlanArtifactCommitRequest, SnapshotBundle,
    SqliteTaskAuthority, TaskSpec, TaskStoreError, artifact_publication_plan_root,
    empty_effect_history_root,
};
use nlos_types::{
    ArtifactId, CancellationScopeId, Generation, IdempotencyKey, TaskAttemptId, TaskId,
    TaskSnapshotId,
};
use rusqlite::Connection;

static NEXT_DATABASE: AtomicU64 = AtomicU64::new(0);

struct TestDatabase {
    path: PathBuf,
}

impl TestDatabase {
    fn new(name: &str) -> Self {
        let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
        Self {
            path: std::env::temp_dir().join(format!(
                "nlos-task-artifact-plan-{name}-{}-{sequence}.sqlite3",
                std::process::id()
            )),
        }
    }

    fn open(&self) -> SqliteTaskAuthority {
        SqliteTaskAuthority::open(&self.path).expect("open authority")
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        for path in [
            self.path.clone(),
            suffix_path(&self.path, "-wal"),
            suffix_path(&self.path, "-shm"),
        ] {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("remove test database: {error}"),
            }
        }
    }
}

fn suffix_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn expectation(seed: u8, revision: u64) -> ArtifactPublicationExpectation {
    ArtifactPublicationExpectation {
        staging_id: [0x20 + seed; 16],
        artifact_id: ArtifactId::from_bytes([0x40 + seed; 16]),
        target_revision: revision,
        digest: [0x60 + seed; 32],
        size_bytes: 100 + u64::from(seed),
    }
}

fn register_and_issue(
    authority: &SqliteTaskAuthority,
    expectations: &[ArtifactPublicationExpectation],
) -> (AttemptSpec, nlos_task::PermitRecord) {
    let task_id = TaskId::from_bytes([0x01; 16]);
    authority
        .register_task(TaskSpec {
            task_id,
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
        })
        .expect("register task");
    let attempt = AttemptSpec {
        task_id,
        attempt_id: TaskAttemptId::from_bytes([0x02; 16]),
        attempt_generation: Generation::INITIAL,
        snapshot: SnapshotBundle {
            snapshot_id: TaskSnapshotId::from_bytes([0x03; 16]),
            snapshot_digest: [0x04; 32],
            expected_head_commit_seq: 0,
            effect_history_root: empty_effect_history_root(),
            retry_fence_epoch: 0,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes([0x05; 16]),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes([0x06; 16]),
        registered_at_ms: 2_000,
    };
    authority
        .register_attempt(attempt)
        .expect("register attempt");
    let write_set_root = artifact_publication_plan_root(expectations).expect("plan root");
    let decision = authority
        .request_commit_permit(PermitRequest {
            task_id,
            attempt_id: attempt.attempt_id,
            attempt_generation: attempt.attempt_generation,
            write_set_root,
            planned_effects: Vec::new(),
            idempotency_key: IdempotencyKey::from_bytes([0x07; 16]),
            valid_until_ms: 20_000,
            requested_at_ms: 3_000,
        })
        .expect("issue permit");
    let PermitDecision::Issued(permit) = decision else {
        panic!("expected issued permit, got {decision:?}");
    };
    (attempt, *permit)
}

fn plan_request(
    attempt: AttemptSpec,
    permit: &nlos_task::PermitRecord,
    expectations: Vec<ArtifactPublicationExpectation>,
) -> PlanArtifactCommitRequest {
    PlanArtifactCommitRequest {
        task_id: attempt.task_id,
        attempt_id: attempt.attempt_id,
        attempt_generation: attempt.attempt_generation,
        permit_id: permit.permit_id,
        expectations,
        idempotency_key: IdempotencyKey::from_bytes([0x08; 16]),
        planned_at_ms: 4_000,
    }
}

#[test]
fn plan_root_is_order_independent_and_rejects_ambiguous_sets() {
    let left = expectation(1, 1);
    let right = expectation(2, 3);
    assert_eq!(
        artifact_publication_plan_root(&[left, right]).unwrap(),
        artifact_publication_plan_root(&[right, left]).unwrap()
    );
    assert!(matches!(
        artifact_publication_plan_root(&[]),
        Err(TaskStoreError::InvalidArtifactPublicationPlan { .. })
    ));
    let mut duplicate_staging = right;
    duplicate_staging.staging_id = left.staging_id;
    assert!(matches!(
        artifact_publication_plan_root(&[left, duplicate_staging]),
        Err(TaskStoreError::InvalidArtifactPublicationPlan { .. })
    ));
    let mut duplicate_revision = right;
    duplicate_revision.artifact_id = left.artifact_id;
    duplicate_revision.target_revision = left.target_revision;
    assert!(matches!(
        artifact_publication_plan_root(&[left, duplicate_revision]),
        Err(TaskStoreError::InvalidArtifactPublicationPlan { .. })
    ));
}

#[test]
fn plan_binds_issued_permit_without_advancing_task_head() {
    let database = TestDatabase::new("plan");
    let authority = database.open();
    let expectations = vec![expectation(2, 1), expectation(1, 4)];
    let (attempt, permit) = register_and_issue(&authority, &expectations);

    let decision = authority
        .plan_artifact_commit(plan_request(attempt, &permit, expectations.clone()))
        .expect("plan");
    assert!(matches!(decision, ArtifactCommitPlanDecision::Planned(_)));
    let record = decision.record();
    assert_eq!(record.state, ArtifactCommitPlanState::Planned);
    assert_eq!(record.write_set_root, permit.write_set_root);
    assert_eq!(
        record.expectations,
        vec![expectation(1, 4), expectation(2, 1)]
    );
    assert_eq!(
        authority
            .inspect_task(attempt.task_id)
            .unwrap()
            .head_commit_seq,
        0
    );
    assert_eq!(
        authority
            .inspect_permit(attempt.task_id, permit.permit_id)
            .unwrap()
            .state,
        nlos_task::PermitState::Issued
    );
}

#[test]
fn exact_plan_replays_across_restart_and_conflicting_key_fails_closed() {
    let database = TestDatabase::new("replay");
    let expectations = vec![expectation(1, 1), expectation(2, 1)];
    let (attempt, permit, first) = {
        let authority = database.open();
        let (attempt, permit) = register_and_issue(&authority, &expectations);
        let first = authority
            .plan_artifact_commit(plan_request(attempt, &permit, expectations.clone()))
            .unwrap()
            .record()
            .clone();
        (attempt, permit, first)
    };
    let reopened = database.open();
    let replay = reopened
        .plan_artifact_commit(plan_request(
            attempt,
            &permit,
            vec![expectation(2, 1), expectation(1, 1)],
        ))
        .expect("replay");
    assert!(matches!(replay, ArtifactCommitPlanDecision::Replayed(_)));
    assert_eq!(replay.record(), &first);
    assert_eq!(
        reopened
            .inspect_artifact_commit_plan(first.plan_id)
            .unwrap(),
        first
    );

    let mut changed = expectation(2, 1);
    changed.digest = [0xee; 32];
    assert!(matches!(
        reopened.plan_artifact_commit(plan_request(
            attempt,
            &permit,
            vec![expectation(1, 1), changed],
        )),
        Err(TaskStoreError::IdempotencyConflict)
    ));
}

#[test]
fn plan_root_must_equal_permit_write_set_root() {
    let database = TestDatabase::new("root-mismatch");
    let authority = database.open();
    let original = vec![expectation(1, 1)];
    let (attempt, permit) = register_and_issue(&authority, &original);
    assert!(matches!(
        authority.plan_artifact_commit(plan_request(attempt, &permit, vec![expectation(2, 1)],)),
        Err(TaskStoreError::InvalidArtifactPublicationPlan { .. })
    ));
    assert_eq!(
        authority
            .inspect_task(attempt.task_id)
            .unwrap()
            .head_commit_seq,
        0
    );
}

#[test]
fn plan_identity_and_expectations_are_ddl_immutable() {
    let database = TestDatabase::new("immutable");
    let authority = database.open();
    let expectations = vec![expectation(1, 1)];
    let (attempt, permit) = register_and_issue(&authority, &expectations);
    let plan = authority
        .plan_artifact_commit(plan_request(attempt, &permit, expectations))
        .unwrap()
        .record()
        .clone();
    drop(authority);

    let raw = Connection::open(&database.path).expect("open raw");
    assert!(
        raw.execute(
            "UPDATE task_artifact_commit_plans SET write_set_root = zeroblob(32)
             WHERE plan_id = ?1",
            [plan.plan_id.as_bytes().as_slice()],
        )
        .is_err()
    );
    assert!(
        raw.execute(
            "DELETE FROM task_artifact_publication_expectations WHERE plan_id = ?1",
            [plan.plan_id.as_bytes().as_slice()],
        )
        .is_err()
    );
    assert!(
        raw.execute(
            "DELETE FROM task_artifact_commit_plans WHERE plan_id = ?1",
            [plan.plan_id.as_bytes().as_slice()],
        )
        .is_err()
    );
}

#[test]
fn v5_database_migrates_to_v6_without_changing_existing_task() {
    let database = TestDatabase::new("migration");
    {
        let authority = database.open();
        authority
            .register_task(TaskSpec {
                task_id: TaskId::from_bytes([0x33; 16]),
                task_generation: Generation::INITIAL,
                registered_at_ms: 1_000,
            })
            .unwrap();
    }
    let raw = Connection::open(&database.path).expect("open raw");
    raw.execute_batch(
        "DROP TABLE task_artifact_publication_expectations;
         DROP TABLE task_artifact_commit_plans;
         PRAGMA user_version = 5;",
    )
    .expect("restore structural v5");
    drop(raw);

    let migrated = database.open();
    assert_eq!(
        migrated
            .inspect_task(TaskId::from_bytes([0x33; 16]))
            .unwrap()
            .head_commit_seq,
        0
    );
    drop(migrated);
    let raw = Connection::open(&database.path).expect("reopen raw");
    let version: i64 = raw
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 6);
}
