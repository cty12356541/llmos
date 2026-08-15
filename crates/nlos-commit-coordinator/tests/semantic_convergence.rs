use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_artifact::ArtifactStore;
use nlos_commit_coordinator::{
    ConvergeSemanticCommitRequest, ConvergeSemanticStep, SemanticCommitCoordinator,
};
use nlos_semantic::SemanticAuthority;
use nlos_task::{
    AttemptSpec, PermitDecision, PermitRequest, PlanSemanticCommitRequest, SemanticCommitPlanId,
    SemanticCommitPlanState, SnapshotBundle, SnapshotConsistency, SqliteTaskAuthority,
    TaskSnapshotReceiptSpec, TaskSpec, TaskWriteSetRequest, TaskWriteSetSemanticAppendRequest,
    TaskWriteSetSemanticRequiredDurability, TaskWriteSetSemanticTarget, empty_effect_history_root,
};
use nlos_types::{
    CancellationScopeId, Generation, IdempotencyKey, NamespaceId, ReceiptId, SemanticEventId,
    TaskAttemptId, TaskId, TaskSnapshotId,
};
use rusqlite::Connection;

static NEXT: AtomicU64 = AtomicU64::new(1);

struct Fixture {
    task_path: PathBuf,
    semantic_root: PathBuf,
    artifact_root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "nlos-semantic-coordinator-{}-{suffix}",
            std::process::id()
        ));
        Self {
            task_path: base.with_extension("sqlite3"),
            semantic_root: base.with_extension("semantic"),
            artifact_root: base.with_extension("artifact"),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let mut path = self.task_path.as_os_str().to_os_string();
            path.push(suffix);
            let _ = std::fs::remove_file(path);
        }
        let _ = std::fs::remove_dir_all(&self.semantic_root);
        let _ = std::fs::remove_dir_all(&self.artifact_root);
    }
}

fn seed_semantic_authority(
    root: &Path,
) -> (SemanticAuthority, SemanticEventId, ReceiptId, ReceiptId) {
    let semantic = SemanticAuthority::open(root).expect("open Semantic authority");
    let event_id = SemanticEventId::from_bytes([0x90; 32]);
    let admission_receipt_id = ReceiptId::from_bytes([0xa0; 16]);
    let durability_receipt_id = ReceiptId::from_bytes([0xb0; 16]);
    let target = NamespaceId::from_bytes([0xc0; 16]);
    let raw = Connection::open(root.join("semantic-authority.db")).expect("open raw Semantic db");
    raw.execute(
        "INSERT INTO content_objects (content_digest, media_type, exact_bytes)
         VALUES (?1, ?2, ?3)",
        rusqlite::params![[0xd0u8; 32].as_slice(), "text/plain", b"semantic"],
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
            [0xe1u8, 0xe2, 0xe3].as_slice(),
            target.as_bytes().as_slice(),
            [0xe4u8; 16].as_slice(),
            [0xe5u8; 16].as_slice(),
            [0xe6u8; 16].as_slice(),
            [0xe7u8; 16].as_slice(),
            [0xd0u8; 32].as_slice(),
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
            [0xe9u8; 16].as_slice(),
            [0xeau8; 16].as_slice(),
            [0xebu8; 16].as_slice(),
            [0xecu8; 64].as_slice(),
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
            [0xedu8; 32].as_slice(),
            [0xeeu8; 64].as_slice(),
        ],
    )
    .expect("insert durability");
    drop(raw);
    (
        semantic,
        event_id,
        admission_receipt_id,
        durability_receipt_id,
    )
}

#[allow(clippy::too_many_lines)]
fn prepare(fixture: &Fixture) -> (SqliteTaskAuthority, SemanticAuthority, SemanticCommitPlanId) {
    let (semantic, event_id, admission_receipt_id, durability_receipt_id) =
        seed_semantic_authority(&fixture.semantic_root);
    let artifact = ArtifactStore::open(&fixture.artifact_root).expect("open Artifact authority");
    let task_id = TaskId::from_bytes([0x10; 16]);
    let attempt_id = TaskAttemptId::from_bytes([0x11; 16]);
    let target = NamespaceId::from_bytes([0xc0; 16]);
    let attempt = AttemptSpec {
        task_id,
        attempt_id,
        attempt_generation: Generation::INITIAL,
        snapshot: SnapshotBundle {
            snapshot_id: TaskSnapshotId::from_bytes([0x12; 16]),
            snapshot_digest: [0x13; 32],
            expected_head_commit_seq: 0,
            effect_history_root: empty_effect_history_root(),
            retry_fence_epoch: 0,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes([0x14; 16]),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes([0x15; 16]),
        registered_at_ms: 10,
    };
    let task = SqliteTaskAuthority::open(&fixture.task_path).expect("open Task authority");
    task.register_task(TaskSpec {
        task_id,
        task_generation: Generation::INITIAL,
        registered_at_ms: 1,
    })
    .unwrap();
    task.register_snapshot_receipt(TaskSnapshotReceiptSpec {
        task_id,
        snapshot: attempt.snapshot,
        receipt_id: ReceiptId::from_bytes([0x16; 16]),
        builder_id: [0x17; 16],
        builder_version_digest: [0x18; 32],
        per_authority_checkpoint_receipts: vec![ReceiptId::from_bytes([0x19; 16])],
        dependency_closure_root: [0x1a; 32],
        semantic_resolver_digest: [0x1b; 32],
        canonical_iteration_digest: [0x1c; 32],
        achieved_consistency: SnapshotConsistency::Causal,
        built_at_ms: 2,
        authority_id: [0x1d; 16],
        key_id: [0x1e; 16],
        signature: [0x1f; 64],
    })
    .unwrap();
    task.register_attempt_with_snapshot_receipt(attempt, ReceiptId::from_bytes([0x16; 16]))
        .unwrap();
    let registry = task.inspect_participant_registry(task_id).unwrap();
    task.register_semantic_admission_participant(
        &semantic,
        task_id,
        nlos_task::ParticipantRegistryBinding {
            generation: registry.generation,
            root: registry.root,
        },
        3,
    )
    .unwrap();
    let write_set = task
        .seal_task_write_set_with_semantic_authority(
            &artifact,
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
                idempotency_key: IdempotencyKey::from_bytes([0x20; 16]),
                sealed_at_ms: 4,
            },
        )
        .unwrap()
        .record()
        .clone();
    assert_eq!(
        write_set.semantic_appends[0].admission_receipt_id,
        admission_receipt_id
    );
    let permit = match task
        .request_commit_permit(PermitRequest {
            task_id,
            attempt_id,
            attempt_generation: Generation::INITIAL,
            write_set_root: write_set.write_set_root,
            planned_effects: Vec::new(),
            idempotency_key: IdempotencyKey::from_bytes([0x21; 16]),
            valid_until_ms: 1_000,
            requested_at_ms: 5,
        })
        .unwrap()
    {
        PermitDecision::Issued(permit) => *permit,
        other => panic!("expected issued permit, got {other:?}"),
    };
    let plan = task
        .plan_semantic_commit(PlanSemanticCommitRequest {
            task_id,
            attempt_id,
            attempt_generation: Generation::INITIAL,
            permit_id: permit.permit_id,
            idempotency_key: IdempotencyKey::from_bytes([0x22; 16]),
            planned_at_ms: 6,
        })
        .unwrap()
        .record()
        .plan_id;
    (task, semantic, plan)
}

#[test]
fn semantic_coordinator_survives_restart_between_publication_and_finalize() {
    let fixture = Fixture::new();
    let (task, semantic, plan_id) = prepare(&fixture);
    let coordinator = SemanticCommitCoordinator::new(&task, &semantic);
    assert!(matches!(
        coordinator
            .converge_one_step(ConvergeSemanticCommitRequest { plan_id, now_ms: 7 })
            .unwrap(),
        ConvergeSemanticStep::Authorized
    ));
    let published = coordinator
        .converge_one_step(ConvergeSemanticCommitRequest { plan_id, now_ms: 8 })
        .unwrap();
    assert!(matches!(
        published,
        ConvergeSemanticStep::PublishedOne {
            state_after: SemanticCommitPlanState::Ready,
            ..
        }
    ));
    drop(task);

    let reopened = SqliteTaskAuthority::open(&fixture.task_path).unwrap();
    let restarted = SemanticCommitCoordinator::new(&reopened, &semantic);
    let receipt = restarted
        .converge(ConvergeSemanticCommitRequest { plan_id, now_ms: 9 })
        .unwrap();
    assert_eq!(receipt.semantic_publications.len(), 1);
    assert_eq!(receipt.task_receipt.new_head_commit_seq, 1);
    let replay = restarted
        .converge_one_step(ConvergeSemanticCommitRequest {
            plan_id,
            now_ms: 99,
        })
        .unwrap();
    assert!(matches!(replay, ConvergeSemanticStep::AlreadyFinalized(_)));
    assert_eq!(restarted.converge_pending(8, 100).unwrap().len(), 0);
    assert!(matches!(
        reopened.inspect_semantic_commit_progress(plan_id),
        Ok(progress) if progress.plan.state == SemanticCommitPlanState::Finalized
    ));
}

#[test]
fn semantic_coordinator_rejects_negative_timestamp_before_mutation() {
    let fixture = Fixture::new();
    let (task, semantic, plan_id) = prepare(&fixture);
    let coordinator = SemanticCommitCoordinator::new(&task, &semantic);
    assert!(matches!(
        coordinator.converge_one_step(ConvergeSemanticCommitRequest {
            plan_id,
            now_ms: -1
        }),
        Err(nlos_commit_coordinator::CoordinatorError::InvalidTimestamp)
    ));
    assert!(matches!(
        task.inspect_semantic_commit_progress(plan_id),
        Ok(progress) if progress.plan.state == SemanticCommitPlanState::Planned
    ));
}
