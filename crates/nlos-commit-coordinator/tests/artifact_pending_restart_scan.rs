#![allow(deprecated)] // Ladder constructors deprecated in favor of the *_with_authorities_struct entries.
//! Bounded restart-scan coverage for an Artifact publication prefix.
//!
//! Both `ArtifactAuthority` publication receipts are durable before the
//! `TaskAuthority` is reopened. The coordinator must consume those exact owner
//! replays, finalize the Task once, and return the same nested receipt on a
//! subsequent replay. This is local recovery evidence, not a distributed
//! atomic-commit claim.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_artifact::{
    ArtifactStore, ContentDigest, CreateArtifactSpec, PublishStagedRevisionRequest,
    StageRevisionRequest, staging_id_for,
};
use nlos_commit_coordinator::{ArtifactCommitCoordinator, ConvergeArtifactCommitRequest};
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
            "nlos-artifact-pending-restart-{}-{sequence}",
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

fn setup_attempt(tasks: &SqliteTaskAuthority, task_id: TaskId, attempt_id: TaskAttemptId) {
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
}

#[test]
#[allow(clippy::too_many_lines)]
fn pending_restart_scan_consumes_all_owner_publication_replays() {
    let authorities = TestAuthorities::new();
    let task_id = TaskId::from_bytes([0x01; 16]);
    let attempt_id = TaskAttemptId::from_bytes([0x02; 16]);
    let first_artifact = artifact(0x11);
    let second_artifact = artifact(0x12);
    let first_key = IdempotencyKey::from_bytes([0x21; 16]);
    let second_key = IdempotencyKey::from_bytes([0x22; 16]);
    let first_bytes = b"first owner publication";
    let second_bytes = b"second owner publication";

    let (plan_id, owner_receipt_ids) = {
        let (tasks, artifacts) = authorities.open();
        create_artifact(&artifacts, first_artifact, 0x31);
        create_artifact(&artifacts, second_artifact, 0x32);
        setup_attempt(&tasks, task_id, attempt_id);

        let expectations = vec![
            ArtifactPublicationExpectation {
                staging_id: staging_id_for(first_artifact, first_key).into_bytes(),
                artifact_id: first_artifact,
                target_revision: 1,
                digest: ContentDigest::of_bytes(first_bytes).into_bytes(),
                size_bytes: u64::try_from(first_bytes.len()).expect("first size"),
            },
            ArtifactPublicationExpectation {
                staging_id: staging_id_for(second_artifact, second_key).into_bytes(),
                artifact_id: second_artifact,
                target_revision: 1,
                digest: ContentDigest::of_bytes(second_bytes).into_bytes(),
                size_bytes: u64::try_from(second_bytes.len()).expect("second size"),
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
                idempotency_key: IdempotencyKey::from_bytes([0x41; 16]),
                valid_until_ms: 30_000,
                requested_at_ms: 3_000,
            })
            .expect("issue permit")
        else {
            panic!("expected issued permit");
        };

        for (artifact_id, staging_key, bytes) in [
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
                    idempotency_key: staging_key,
                    created_at_ms: 3_500,
                })
                .expect("stage revision");
        }
        let plan_id = tasks
            .plan_artifact_commit(PlanArtifactCommitRequest {
                task_id,
                attempt_id,
                attempt_generation: Generation::INITIAL,
                permit_id: permit.permit_id,
                expectations,
                idempotency_key: IdempotencyKey::from_bytes([0x42; 16]),
                planned_at_ms: 4_000,
            })
            .expect("plan artifact commit")
            .record()
            .plan_id;
        tasks
            .authorize_artifact_publication(plan_id, 4_500)
            .expect("authorize publication");

        let first_receipt = artifacts
            .publish_staged_revision(PublishStagedRevisionRequest {
                staging_id: staging_id_for(first_artifact, first_key),
                task_id,
                permit_id: permit.permit_id,
                write_set_root: ContentDigest::from_bytes(write_set_root),
                published_at_ms: 5_000,
            })
            .expect("publish first artifact");
        let second_receipt = artifacts
            .publish_staged_revision(PublishStagedRevisionRequest {
                staging_id: staging_id_for(second_artifact, second_key),
                task_id,
                permit_id: permit.permit_id,
                write_set_root: ContentDigest::from_bytes(write_set_root),
                published_at_ms: 5_100,
            })
            .expect("publish second artifact");

        let progress = tasks
            .inspect_artifact_commit_progress(plan_id)
            .expect("inspect publishing prefix");
        assert_eq!(progress.plan.state, ArtifactCommitPlanState::Publishing);
        assert!(progress.publications.is_empty());
        (
            plan_id,
            vec![
                first_receipt.receipt().receipt_id,
                second_receipt.receipt().receipt_id,
            ],
        )
    };

    let (tasks, artifacts) = authorities.open();
    let reopened_prefix = tasks
        .inspect_artifact_commit_progress(plan_id)
        .expect("inspect reopened publishing prefix");
    assert_eq!(
        reopened_prefix.plan.state,
        ArtifactCommitPlanState::Publishing
    );
    assert!(reopened_prefix.publications.is_empty());
    let receipts = ArtifactCommitCoordinator::new(&tasks, &artifacts)
        .converge_pending(1, 6_000)
        .expect("converge pending artifact plan");
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].artifact_publications.len(), 2);
    assert_eq!(
        receipts[0]
            .artifact_publications
            .iter()
            .map(|receipt| receipt.receipt_id)
            .collect::<Vec<_>>(),
        owner_receipt_ids
    );
    assert_eq!(receipts[0].task_receipt.new_head_commit_seq, 1);
    assert_eq!(
        tasks
            .inspect_artifact_commit_plan(plan_id)
            .expect("inspect finalized plan")
            .state,
        ArtifactCommitPlanState::Finalized
    );
    assert_eq!(
        artifacts
            .resolve_head(first_artifact)
            .expect("resolve first head")
            .expect("first head exists")
            .revision,
        1
    );
    assert_eq!(
        artifacts
            .resolve_head(second_artifact)
            .expect("resolve second head")
            .expect("second head exists")
            .revision,
        1
    );

    let replay = ArtifactCommitCoordinator::new(&tasks, &artifacts)
        .converge(ConvergeArtifactCommitRequest {
            plan_id,
            now_ms: 7_000,
        })
        .expect("replay finalized plan");
    assert_eq!(replay, receipts[0]);
    assert!(
        ArtifactCommitCoordinator::new(&tasks, &artifacts)
            .converge_pending(1, 8_000)
            .expect("scan after replay")
            .is_empty()
    );
}
