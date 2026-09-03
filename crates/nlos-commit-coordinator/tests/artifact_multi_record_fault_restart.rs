#![allow(deprecated)] // Ladder constructors deprecated in favor of the *_with_authorities_struct entries.
//! Bounded multi-Artifact record fault-prefix/restart coverage.
//!
//! The first owner publication is consumed by `TaskAuthority`. The second owner
//! publication is durable, but its Task-side nested-receipt transaction is
//! aborted. After the fault is removed and both authorities are reopened, the
//! public pending scan must replay only the missing owner receipt and finish
//! the same plan exactly once. This is local `SQLite` evidence, not a distributed
//! atomic-commit or real power-loss claim.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_artifact::{
    ArtifactStore, ContentDigest, CreateArtifactSpec, StageRevisionRequest, staging_id_for,
};
use nlos_commit_coordinator::{
    ArtifactCommitCoordinator, ConvergeArtifactCommitRequest, ConvergeStep, CoordinatorError,
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
    fn new() -> Self {
        let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "nlos-artifact-multi-record-fault-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create test authorities");
        Self {
            task_path: root.join("tasks.sqlite3"),
            artifact_root: root.join("artifact-store"),
            root,
        }
    }

    fn open(&self) -> (SqliteTaskAuthority, ArtifactStore) {
        (
            SqliteTaskAuthority::open(&self.task_path).expect("open TaskAuthority"),
            ArtifactStore::open(&self.artifact_root).expect("open ArtifactAuthority"),
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
        .expect("create artifact");
}

fn execute_sql(path: &Path, sql: &str) {
    rusqlite::Connection::open(path)
        .expect("open raw authority connection")
        .execute_batch(sql)
        .expect("execute test fault SQL");
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    bytes.iter().fold(String::new(), |mut output, byte| {
        write!(output, "{byte:02x}").expect("write hex");
        output
    })
}

#[derive(Clone, Copy)]
struct PreparedPlan {
    plan_id: nlos_task::ArtifactCommitPlanId,
    task_id: TaskId,
    first_artifact: ArtifactId,
    second_artifact: ArtifactId,
}

#[allow(clippy::too_many_lines)]
fn prepare_plan(authorities: &TestAuthorities) -> PreparedPlan {
    let (tasks, artifacts) = authorities.open();
    let task_id = TaskId::from_bytes([0x01; 16]);
    let attempt_id = TaskAttemptId::from_bytes([0x02; 16]);
    let first_artifact = artifact(0x11);
    let second_artifact = artifact(0x12);
    let first_key = IdempotencyKey::from_bytes([0x21; 16]);
    let second_key = IdempotencyKey::from_bytes([0x22; 16]);
    let first_bytes = b"first owner publication";
    let second_bytes = b"second owner publication";

    create_artifact(&artifacts, first_artifact, 0x31);
    create_artifact(&artifacts, second_artifact, 0x32);
    tasks
        .register_task(TaskSpec {
            task_id,
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
        })
        .expect("register task");
    tasks
        .register_attempt(AttemptSpec {
            task_id,
            attempt_id,
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
        })
        .expect("register attempt");

    let expectations = vec![
        ArtifactPublicationExpectation {
            staging_id: staging_id_for(first_artifact, first_key).into_bytes(),
            artifact_id: first_artifact,
            target_revision: 1,
            digest: ContentDigest::of_bytes(first_bytes).into_bytes(),
            size_bytes: u64::try_from(first_bytes.len()).expect("first size fits"),
        },
        ArtifactPublicationExpectation {
            staging_id: staging_id_for(second_artifact, second_key).into_bytes(),
            artifact_id: second_artifact,
            target_revision: 1,
            digest: ContentDigest::of_bytes(second_bytes).into_bytes(),
            size_bytes: u64::try_from(second_bytes.len()).expect("second size fits"),
        },
    ];
    let write_set_root = artifact_publication_plan_root(&expectations).expect("plan root");
    let PermitDecision::Issued(permit) = tasks
        .request_commit_permit(PermitRequest {
            task_id,
            attempt_id,
            attempt_generation: Generation::INITIAL,
            write_set_root,
            planned_effects: Vec::new(),
            idempotency_key: IdempotencyKey::from_bytes([0x07; 16]),
            valid_until_ms: i64::MAX,
            requested_at_ms: 3_000,
        })
        .expect("issue commit permit")
    else {
        panic!("expected issued permit");
    };

    for (artifact_id, stage_key, bytes) in [
        (first_artifact, first_key, first_bytes.as_slice()),
        (second_artifact, second_key, second_bytes.as_slice()),
    ] {
        artifacts
            .stage_revision(StageRevisionRequest {
                artifact_id,
                expected_head_revision: 0,
                bytes,
                task_id,
                permit_id: permit.permit_id,
                write_set_root: ContentDigest::from_bytes(write_set_root),
                idempotency_key: stage_key,
                created_at_ms: 3_500,
            })
            .expect("stage Artifact revision");
    }

    let plan_id = tasks
        .plan_artifact_commit(PlanArtifactCommitRequest {
            task_id,
            attempt_id,
            attempt_generation: Generation::INITIAL,
            permit_id: permit.permit_id,
            expectations,
            idempotency_key: IdempotencyKey::from_bytes([0x08; 16]),
            planned_at_ms: 4_000,
        })
        .expect("plan Artifact commit")
        .record()
        .plan_id;

    PreparedPlan {
        plan_id,
        task_id,
        first_artifact,
        second_artifact,
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn second_artifact_record_failure_restarts_from_owner_prefix() {
    let authorities = TestAuthorities::new();
    let prepared = prepare_plan(&authorities);

    // Authorize, then publish and record exactly the first expectation. The
    // second owner publication remains the next durable coordinator step.
    let missing_staging_id = {
        let (tasks, artifacts) = authorities.open();
        let coordinator = ArtifactCommitCoordinator::new(&tasks, &artifacts);
        assert_eq!(
            coordinator
                .converge_one_step(ConvergeArtifactCommitRequest {
                    plan_id: prepared.plan_id,
                    now_ms: 5_000,
                })
                .expect("authorize plan"),
            ConvergeStep::Authorized
        );
        assert!(matches!(
            coordinator
                .converge_one_step(ConvergeArtifactCommitRequest {
                    plan_id: prepared.plan_id,
                    now_ms: 5_100,
                })
                .expect("publish and record first Artifact"),
            ConvergeStep::PublishedOne {
                state_after: ArtifactCommitPlanState::Publishing,
                ..
            }
        ));
        let progress = tasks
            .inspect_artifact_commit_progress(prepared.plan_id)
            .expect("inspect first durable prefix");
        assert_eq!(progress.publications.len(), 1);
        progress
            .plan
            .expectations
            .iter()
            .find(|expectation| {
                !progress
                    .publications
                    .iter()
                    .any(|receipt| receipt.staging_id == expectation.staging_id)
            })
            .expect("second expectation remains missing")
            .staging_id
    };

    // The second Artifact publication itself is allowed to commit. Only the
    // Task-side nested receipt insert is aborted, leaving an honest prefix:
    // both owner heads are visible, but TaskAuthority has one receipt.
    execute_sql(
        &authorities.task_path,
        &format!(
            "CREATE TRIGGER fail_second_nested_receipt
             BEFORE INSERT ON task_artifact_publication_receipts
             WHEN NEW.staging_id = X'{}'
             BEGIN SELECT RAISE(ABORT, 'injected second record failure'); END;",
            hex(&missing_staging_id)
        ),
    );
    {
        let (tasks, artifacts) = authorities.open();
        assert!(matches!(
            ArtifactCommitCoordinator::new(&tasks, &artifacts).converge_one_step(
                ConvergeArtifactCommitRequest {
                    plan_id: prepared.plan_id,
                    now_ms: 5_200,
                }
            ),
            Err(CoordinatorError::Task(_))
        ));
        let progress = tasks
            .inspect_artifact_commit_progress(prepared.plan_id)
            .expect("inspect partial record prefix");
        assert_eq!(progress.plan.state, ArtifactCommitPlanState::Publishing);
        assert_eq!(progress.publications.len(), 1);
        assert_eq!(
            artifacts
                .resolve_head(prepared.first_artifact, u64::MAX)
                .expect("resolve first head")
                .expect("first owner publication")
                .revision,
            1
        );
        assert_eq!(
            artifacts
                .resolve_head(prepared.second_artifact, u64::MAX)
                .expect("resolve second head")
                .expect("second owner publication survived Task failure")
                .revision,
            1
        );
        assert_eq!(
            tasks
                .inspect_task(prepared.task_id)
                .expect("inspect Task")
                .head_commit_seq,
            0
        );
    }

    // Remove the injected failure only after the failed authority has been
    // closed. Reopening both stores models a coordinator process restart.
    execute_sql(
        &authorities.task_path,
        "DROP TRIGGER fail_second_nested_receipt;",
    );
    let committed = {
        let (tasks, artifacts) = authorities.open();
        let receipts = ArtifactCommitCoordinator::new(&tasks, &artifacts)
            .converge_pending(8, 5_300)
            .expect("pending restart scan converges missing record");
        assert_eq!(receipts.len(), 1);
        let committed = receipts.into_iter().next().expect("one Task receipt");
        assert_eq!(committed.artifact_publications.len(), 2);
        assert_eq!(
            tasks
                .inspect_artifact_commit_progress(prepared.plan_id)
                .expect("inspect finalized progress")
                .plan
                .state,
            ArtifactCommitPlanState::Finalized
        );
        assert_eq!(
            tasks
                .inspect_task(prepared.task_id)
                .expect("inspect finalized Task")
                .head_commit_seq,
            1
        );
        committed
    };

    let (tasks, artifacts) = authorities.open();
    assert_eq!(
        ArtifactCommitCoordinator::new(&tasks, &artifacts)
            .converge(ConvergeArtifactCommitRequest {
                plan_id: prepared.plan_id,
                now_ms: 5_400,
            })
            .expect("finalized replay")
            .clone(),
        committed
    );
    assert!(
        ArtifactCommitCoordinator::new(&tasks, &artifacts)
            .converge_pending(8, 5_500)
            .expect("empty pending scan")
            .is_empty()
    );
}
