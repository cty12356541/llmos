use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_artifact::{
    ArtifactStore, ContentDigest, CreateArtifactSpec, PublishStagedRevisionRequest,
    StageRevisionRequest, staging_id_for,
};
use nlos_commit_coordinator::{
    ArtifactCommitCoordinator, ConvergeArtifactCommitRequest, ConvergeStep,
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

#[test]
#[allow(clippy::too_many_lines)]
fn every_cross_authority_prefix_converges_after_restart() {
    let databases = TestAuthorities::new("restart-prefixes");
    let task_id = TaskId::from_bytes([0x01; 16]);
    let attempt_id = TaskAttemptId::from_bytes([0x02; 16]);
    let first_artifact = artifact(0x31);
    let second_artifact = artifact(0x32);
    let first_key = IdempotencyKey::from_bytes([0x41; 16]);
    let second_key = IdempotencyKey::from_bytes([0x42; 16]);
    let first_bytes = b"first artifact payload";
    let second_bytes = b"second artifact payload";

    let (plan_id, permit_id, write_set_root) = {
        let (tasks, artifacts) = databases.open();
        create_artifact(&artifacts, first_artifact, 0x51);
        create_artifact(&artifacts, second_artifact, 0x52);
        tasks
            .register_task(TaskSpec {
                task_id,
                task_generation: Generation::INITIAL,
                registered_at_ms: 1_000,
            })
            .unwrap();
        let attempt = AttemptSpec {
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
        };
        tasks.register_attempt(attempt).unwrap();
        let expectations = vec![
            ArtifactPublicationExpectation {
                staging_id: staging_id_for(first_artifact, first_key).into_bytes(),
                artifact_id: first_artifact,
                target_revision: 1,
                digest: ContentDigest::of_bytes(first_bytes).into_bytes(),
                size_bytes: u64::try_from(first_bytes.len()).unwrap(),
            },
            ArtifactPublicationExpectation {
                staging_id: staging_id_for(second_artifact, second_key).into_bytes(),
                artifact_id: second_artifact,
                target_revision: 1,
                digest: ContentDigest::of_bytes(second_bytes).into_bytes(),
                size_bytes: u64::try_from(second_bytes.len()).unwrap(),
            },
        ];
        let write_set_root = artifact_publication_plan_root(&expectations).unwrap();
        let PermitDecision::Issued(permit) = tasks
            .request_commit_permit(PermitRequest {
                task_id,
                attempt_id,
                attempt_generation: Generation::INITIAL,
                write_set_root,
                planned_effects: Vec::new(),
                idempotency_key: IdempotencyKey::from_bytes([0x07; 16]),
                valid_until_ms: 30_000,
                requested_at_ms: 3_000,
            })
            .unwrap()
        else {
            panic!("expected issued permit");
        };
        for (artifact_id, key, bytes) in [
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
                    idempotency_key: key,
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
                idempotency_key: IdempotencyKey::from_bytes([0x08; 16]),
                planned_at_ms: 4_000,
            })
            .unwrap()
            .record()
            .plan_id;
        (plan_id, permit.permit_id, write_set_root)
    };

    {
        // Simulate a crash after ArtifactAuthority commits but before the
        // coordinator records the nested receipt in TaskAuthority.
        let (_tasks, artifacts) = databases.open();
        artifacts
            .publish_staged_revision(PublishStagedRevisionRequest {
                staging_id: staging_id_for(first_artifact, first_key),
                task_id,
                permit_id,
                write_set_root: ContentDigest::from_bytes(write_set_root),
                published_at_ms: 5_500,
            })
            .unwrap();
    }
    {
        let (tasks, artifacts) = databases.open();
        let step = ArtifactCommitCoordinator::new(&tasks, &artifacts)
            .converge_one_step(ConvergeArtifactCommitRequest {
                plan_id,
                now_ms: 5_000,
            })
            .unwrap();
        assert_eq!(step, ConvergeStep::Authorized);
    }
    {
        let (tasks, artifacts) = databases.open();
        let step = ArtifactCommitCoordinator::new(&tasks, &artifacts)
            .converge_one_step(ConvergeArtifactCommitRequest {
                plan_id,
                now_ms: 6_000,
            })
            .unwrap();
        assert!(matches!(
            step,
            ConvergeStep::PublishedOne {
                state_after: ArtifactCommitPlanState::Publishing,
                ..
            }
        ));
        assert_eq!(
            tasks
                .inspect_artifact_commit_progress(plan_id)
                .unwrap()
                .publications
                .len(),
            1
        );
    }
    {
        let (tasks, artifacts) = databases.open();
        let step = ArtifactCommitCoordinator::new(&tasks, &artifacts)
            .converge_one_step(ConvergeArtifactCommitRequest {
                plan_id,
                now_ms: 6_500,
            })
            .unwrap();
        assert!(matches!(
            step,
            ConvergeStep::PublishedOne {
                state_after: ArtifactCommitPlanState::Ready,
                ..
            }
        ));
    }
    let committed = {
        let (tasks, artifacts) = databases.open();
        let mut completed = ArtifactCommitCoordinator::new(&tasks, &artifacts)
            .converge_pending(16, 7_000)
            .unwrap();
        assert_eq!(completed.len(), 1);
        completed.pop().unwrap()
    };
    assert_eq!(committed.artifact_publications.len(), 2);
    assert_eq!(committed.task_receipt.new_head_commit_seq, 1);

    let (tasks, artifacts) = databases.open();
    assert_eq!(
        artifacts
            .resolve_head(first_artifact)
            .unwrap()
            .unwrap()
            .revision,
        1
    );
    assert_eq!(
        artifacts
            .resolve_head(second_artifact)
            .unwrap()
            .unwrap()
            .revision,
        1
    );
    let replay = ArtifactCommitCoordinator::new(&tasks, &artifacts)
        .converge(ConvergeArtifactCommitRequest {
            plan_id,
            now_ms: 9_000,
        })
        .unwrap();
    assert_eq!(replay, committed);
    assert!(
        ArtifactCommitCoordinator::new(&tasks, &artifacts)
            .converge_pending(16, 10_000)
            .unwrap()
            .is_empty()
    );
}
