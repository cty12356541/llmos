#![allow(deprecated)] // Ladder constructors deprecated in favor of the *_with_authorities_struct entries.
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_channel::{ChannelAuthority, ChannelDecision, CreateChannelRequest, RotateChannelRequest};
use nlos_task::{
    AttemptSpec, LogicalEffectDescriptor, ParticipantRegistryBinding, ParticipantType,
    PermitDecision, PermitRequest, PlannedEffect, SnapshotBundle, SnapshotConsistency,
    SqliteTaskAuthority, TaskSnapshotReceiptSpec, TaskSpec, TaskStoreError,
    TaskWriteSetArtifactRead, TaskWriteSetEffectEndpointKind, TaskWriteSetEffectEndpointRequest,
    TaskWriteSetRequest, empty_effect_history_root,
};
use nlos_types::{
    ArtifactId, CancellationScopeId, Generation, IdempotencyKey, ReceiptId, TaskAttemptId, TaskId,
    TaskSnapshotId,
};
use rusqlite::Connection;
use sha2::{Digest, Sha256};

static NEXT: AtomicU64 = AtomicU64::new(1);

struct Database(PathBuf);

impl Database {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "nlos-task-channel-endpoint-{}-{}.sqlite3",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
    }

    fn open(&self) -> SqliteTaskAuthority {
        SqliteTaskAuthority::open(&self.0).unwrap()
    }
}

impl Drop for Database {
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
            "nlos-task-channel-endpoint-{label}-{}-{}",
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
    TaskId::from_bytes([0x21; 16])
}

fn attempt(seed: u8, head: u64, history: [u8; 32]) -> AttemptSpec {
    AttemptSpec {
        task_id: task_id(),
        attempt_id: TaskAttemptId::from_bytes([seed; 16]),
        attempt_generation: Generation::INITIAL,
        snapshot: SnapshotBundle {
            snapshot_id: TaskSnapshotId::from_bytes([seed.wrapping_add(1); 16]),
            snapshot_digest: [seed.wrapping_add(2); 32],
            expected_head_commit_seq: head,
            effect_history_root: history,
            retry_fence_epoch: 0,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes([seed.wrapping_add(3); 16]),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(4); 16]),
        registered_at_ms: 2_000 + i64::from(seed),
    }
}

fn permit(spec: &AttemptSpec, seed: u8) -> PermitRequest {
    PermitRequest {
        task_id: spec.task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        write_set_root: [seed; 32],
        planned_effects: Vec::new(),
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(10); 16]),
        valid_until_ms: 9_000,
        requested_at_ms: 3_000 + i64::from(seed),
    }
}

fn planned_effect() -> PlannedEffect {
    PlannedEffect {
        descriptor: LogicalEffectDescriptor {
            task_id: task_id(),
            task_generation: Generation::INITIAL,
            intent_spec_id: [0xa1; 32],
            stable_action_slot: 0,
            target_authority_object_id: [0xa2; 32],
            effect_class: 1,
            idempotency_scope: 1,
        },
        required: false,
        required_condition_digest: None,
        success_criteria_digest: [0xa3; 32],
        action_proposal_digest: [0xa4; 32],
    }
}

fn issued(decision: PermitDecision) -> nlos_task::PermitRecord {
    match decision {
        PermitDecision::Issued(record) => *record,
        other => panic!("expected issued permit, got {other:?}"),
    }
}

fn binding(registry: &nlos_task::ParticipantRegistryRecord) -> ParticipantRegistryBinding {
    ParticipantRegistryBinding {
        generation: registry.generation,
        root: registry.root,
    }
}

fn create_artifact(store: &nlos_artifact::ArtifactStore, seed: u8) -> ArtifactId {
    let artifact_id = ArtifactId::from_bytes([seed; 16]);
    store
        .create_artifact(nlos_artifact::CreateArtifactSpec {
            artifact_id,
            idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(1); 16]),
            content_type: "application/octet-stream".to_owned(),
            application_id: None,
            owner: None,
            created_at_ms: 1_050,
        })
        .unwrap();
    artifact_id
}

fn create_channel(authority: &ChannelAuthority) -> nlos_channel::ChannelRecord {
    match authority
        .create_channel(CreateChannelRequest {
            capacity_bytes: 4096,
            policy_digest: [0xb1; 32],
            idempotency_key: IdempotencyKey::from_bytes([0xb2; 16]),
            created_at_ms: 1_060,
        })
        .unwrap()
    {
        ChannelDecision::Created(record) => record,
        ChannelDecision::Replayed(_) => panic!("first channel create cannot replay"),
    }
}

/// Registers the task/attempt/snapshot-receipt/Channel-participant fixture
/// shared by every seal-and-permit test, returning the attempt spec and the
/// created Channel record.
struct ChannelFixture {
    spec: AttemptSpec,
    channel: nlos_channel::ChannelRecord,
    artifact_id: ArtifactId,
}

fn register_task_attempt_and_channel_participant(
    authority: &SqliteTaskAuthority,
    channel_authority: &ChannelAuthority,
    artifact_id: ArtifactId,
    channel: nlos_channel::ChannelRecord,
) -> ChannelFixture {
    authority
        .register_task(TaskSpec {
            task_id: task_id(),
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
        })
        .unwrap();
    let spec = attempt(0xb3, 0, empty_effect_history_root());
    let snapshot_receipt_id = ReceiptId::from_bytes([0xb4; 16]);
    authority
        .register_snapshot_receipt(TaskSnapshotReceiptSpec {
            task_id: task_id(),
            snapshot: spec.snapshot,
            receipt_id: snapshot_receipt_id,
            builder_id: [0xb5; 16],
            builder_version_digest: [0xb6; 32],
            per_authority_checkpoint_receipts: vec![ReceiptId::from_bytes([0xb7; 16])],
            dependency_closure_root: [0xb8; 32],
            semantic_resolver_digest: [0xb9; 32],
            canonical_iteration_digest: [0xba; 32],
            achieved_consistency: SnapshotConsistency::Causal,
            built_at_ms: 1_100,
            authority_id: [0xbb; 16],
            key_id: [0xbc; 16],
            signature: [0xbd; 64],
        })
        .unwrap();
    authority
        .register_attempt_with_snapshot_receipt(spec, snapshot_receipt_id)
        .unwrap();
    let registry_binding = binding(&authority.inspect_participant_registry(task_id()).unwrap());
    authority
        .register_channel_participant(
            channel_authority,
            task_id(),
            registry_binding,
            channel.channel_id,
            channel.generation,
            1_150,
        )
        .unwrap();
    ChannelFixture {
        spec,
        channel,
        artifact_id,
    }
}

fn channel_write_set_request(
    fixture: &ChannelFixture,
    expected_channel_generation: Generation,
    key: [u8; 16],
) -> TaskWriteSetRequest {
    TaskWriteSetRequest {
        task_id: task_id(),
        attempt_id: fixture.spec.attempt_id,
        attempt_generation: fixture.spec.attempt_generation,
        artifact_reads: vec![TaskWriteSetArtifactRead {
            artifact_id: fixture.artifact_id,
            expected_head_revision: 0,
            expected_head_digest: None,
        }],
        artifact_writes: Vec::new(),
        process_binding: None,
        semantic_reads: Vec::new(),
        semantic_appends: Vec::new(),
        resource_reservations: Vec::new(),
        planned_effects: vec![planned_effect()],
        effect_endpoints: vec![TaskWriteSetEffectEndpointRequest::ChannelTopicBinding {
            effect_seq: 0,
            channel_id: fixture.channel.channel_id,
            expected_channel_generation,
        }],
        idempotency_key: IdempotencyKey::from_bytes(key),
        sealed_at_ms: 1_200,
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn verified_channel_endpoint_is_rechecked_during_seal_and_permit() {
    let database = Database::new();
    let artifact_root = AuthorityRoot::new("artifact");
    let channel_root = AuthorityRoot::new("authority");
    let artifact = nlos_artifact::ArtifactStore::open(&artifact_root.0).unwrap();
    let artifact_id = create_artifact(&artifact, 0xc1);
    let channel_authority = ChannelAuthority::open(&channel_root.0).unwrap();
    let channel = create_channel(&channel_authority);

    let authority = database.open();
    let fixture = register_task_attempt_and_channel_participant(
        &authority,
        &channel_authority,
        artifact_id,
        channel,
    );
    assert!(
        authority
            .inspect_participant_registry(task_id())
            .unwrap()
            .participants
            .iter()
            .any(|participant| participant.participant_type == ParticipantType::ChannelTopic)
    );

    let request = channel_write_set_request(&fixture, channel.generation, [0xce; 16]);
    let record = authority
        .seal_task_write_set_with_channel_authority(&artifact, &channel_authority, request)
        .unwrap()
        .record()
        .clone();
    assert_eq!(record.effect_endpoints.len(), 1);
    assert_eq!(
        record.effect_endpoints[0].kind,
        TaskWriteSetEffectEndpointKind::ChannelTopicBinding
    );
    assert_eq!(
        record.effect_endpoints[0].object_id,
        channel.channel_id.into_bytes()
    );
    assert_eq!(
        record.effect_endpoints[0].participant_generation,
        channel.generation
    );
    assert_eq!(
        record.effect_endpoints[0].participant_id,
        channel_authority
            .inspect_endpoint_proof(channel.channel_id)
            .unwrap()
            .participant_id
    );

    let mut permit_request = permit(&fixture.spec, 0xcf);
    permit_request.write_set_root = record.write_set_root;
    permit_request.planned_effects = record.planned_effects.clone();
    assert!(matches!(
        authority.request_commit_permit(permit_request.clone()),
        Err(TaskStoreError::TaskWriteSetConflict {
            reason: "Channel effect endpoint requires ChannelAuthority readback before permit freeze"
        })
    ));
    let permit_record = issued(
        authority
            .request_commit_permit_with_channel_authority(
                &channel_authority,
                permit_request.clone(),
            )
            .unwrap(),
    );
    assert!(matches!(
        authority
            .request_commit_permit_with_channel_authority(&channel_authority, permit_request)
            .unwrap(),
        PermitDecision::Replayed(_)
    ));
    assert_eq!(permit_record.write_set_root, record.write_set_root);

    drop(authority);
    drop(channel_authority);
    let reopened_channel = ChannelAuthority::open(&channel_root.0).unwrap();
    let proof = reopened_channel
        .inspect_endpoint_proof(channel.channel_id)
        .unwrap();
    assert_eq!(proof.participant_generation, channel.generation);
    let reopened = database.open();
    assert_eq!(
        reopened
            .inspect_task_write_set(task_id(), IdempotencyKey::from_bytes([0xce; 16]))
            .unwrap(),
        record
    );
}

#[test]
fn stale_expected_channel_generation_is_rejected_at_seal_without_partial_seal() {
    let database = Database::new();
    let artifact_root = AuthorityRoot::new("artifact");
    let channel_root = AuthorityRoot::new("authority");
    let artifact = nlos_artifact::ArtifactStore::open(&artifact_root.0).unwrap();
    let artifact_id = create_artifact(&artifact, 0xd1);
    let channel_authority = ChannelAuthority::open(&channel_root.0).unwrap();
    let channel = create_channel(&channel_authority);

    let authority = database.open();
    let fixture = register_task_attempt_and_channel_participant(
        &authority,
        &channel_authority,
        artifact_id,
        channel,
    );
    let stale = channel_write_set_request(
        &fixture,
        channel.generation.checked_next().unwrap(),
        [0xd2; 16],
    );
    assert!(matches!(
        authority.seal_task_write_set_with_channel_authority(&artifact, &channel_authority, stale),
        Err(TaskStoreError::ParticipantEndpointGenerationMismatch {
            expected: 2,
            current: 1,
        })
    ));
    assert!(matches!(
        authority.inspect_task_write_set(task_id(), IdempotencyKey::from_bytes([0xd2; 16])),
        Err(TaskStoreError::TaskWriteSetNotFound)
    ));
}

#[test]
fn channel_rotation_between_seal_and_permit_fails_closed() {
    let database = Database::new();
    let artifact_root = AuthorityRoot::new("artifact");
    let channel_root = AuthorityRoot::new("authority");
    let artifact = nlos_artifact::ArtifactStore::open(&artifact_root.0).unwrap();
    let artifact_id = create_artifact(&artifact, 0xe1);
    let channel_authority = ChannelAuthority::open(&channel_root.0).unwrap();
    let channel = create_channel(&channel_authority);

    let authority = database.open();
    let fixture = register_task_attempt_and_channel_participant(
        &authority,
        &channel_authority,
        artifact_id,
        channel,
    );
    let request = channel_write_set_request(&fixture, channel.generation, [0xe2; 16]);
    let record = authority
        .seal_task_write_set_with_channel_authority(&artifact, &channel_authority, request)
        .unwrap()
        .record()
        .clone();

    channel_authority
        .rotate_channel(RotateChannelRequest {
            channel_id: channel.channel_id,
            expected_generation: channel.generation,
            expected_fencing_token: channel.fencing_token,
            idempotency_key: IdempotencyKey::from_bytes([0xe3; 16]),
            rotated_at_ms: 1_250,
        })
        .unwrap();
    let mut permit_request = permit(&fixture.spec, 0xe4);
    permit_request.write_set_root = record.write_set_root;
    permit_request.planned_effects = record.planned_effects.clone();
    assert!(matches!(
        authority.request_commit_permit_with_channel_authority(&channel_authority, permit_request),
        Err(TaskStoreError::TaskWriteSetConflict {
            reason: "Channel endpoint proof differs before permit freeze"
        })
    ));
}

#[test]
fn channel_endpoint_requires_prior_participant_registration() {
    let database = Database::new();
    let artifact_root = AuthorityRoot::new("artifact");
    let channel_root = AuthorityRoot::new("authority");
    let artifact = nlos_artifact::ArtifactStore::open(&artifact_root.0).unwrap();
    let artifact_id = create_artifact(&artifact, 0xf1);
    let channel_authority = ChannelAuthority::open(&channel_root.0).unwrap();
    let channel = create_channel(&channel_authority);

    let authority = database.open();
    authority
        .register_task(TaskSpec {
            task_id: task_id(),
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
        })
        .unwrap();
    let spec = attempt(0xf2, 0, empty_effect_history_root());
    let snapshot_receipt_id = ReceiptId::from_bytes([0xf4; 16]);
    authority
        .register_snapshot_receipt(TaskSnapshotReceiptSpec {
            task_id: task_id(),
            snapshot: spec.snapshot,
            receipt_id: snapshot_receipt_id,
            builder_id: [0xf5; 16],
            builder_version_digest: [0xf6; 32],
            per_authority_checkpoint_receipts: vec![ReceiptId::from_bytes([0xf7; 16])],
            dependency_closure_root: [0xf8; 32],
            semantic_resolver_digest: [0xf9; 32],
            canonical_iteration_digest: [0xfa; 32],
            achieved_consistency: SnapshotConsistency::Causal,
            built_at_ms: 1_100,
            authority_id: [0xfb; 16],
            key_id: [0xfc; 16],
            signature: [0xfd; 64],
        })
        .unwrap();
    authority
        .register_attempt_with_snapshot_receipt(spec, snapshot_receipt_id)
        .unwrap();
    let request = TaskWriteSetRequest {
        task_id: task_id(),
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        artifact_reads: vec![TaskWriteSetArtifactRead {
            artifact_id,
            expected_head_revision: 0,
            expected_head_digest: None,
        }],
        artifact_writes: Vec::new(),
        process_binding: None,
        semantic_reads: Vec::new(),
        semantic_appends: Vec::new(),
        resource_reservations: Vec::new(),
        planned_effects: vec![planned_effect()],
        effect_endpoints: vec![TaskWriteSetEffectEndpointRequest::ChannelTopicBinding {
            effect_seq: 0,
            channel_id: channel.channel_id,
            expected_channel_generation: channel.generation,
        }],
        idempotency_key: IdempotencyKey::from_bytes([0xf3; 16]),
        sealed_at_ms: 1_200,
    };
    assert!(matches!(
        authority.seal_task_write_set_with_channel_authority(
            &artifact,
            &channel_authority,
            request
        ),
        Err(TaskStoreError::TaskWriteSetConflict {
            reason: "planned effect endpoint is not registered in participant registry"
        })
    ));
}

#[test]
fn schema_migrates_v39_endpoint_check_to_v40() {
    let database = Database::new();
    drop(database.open());
    {
        let raw = Connection::open(&database.0).unwrap();
        raw.pragma_update(None, "foreign_keys", "OFF").unwrap();
        raw.execute_batch(
            "DROP TRIGGER task_write_set_effect_endpoint_is_immutable;
             DROP TRIGGER task_write_set_effect_endpoint_is_immutable_delete;
             DROP TABLE task_write_set_effect_endpoints;
             CREATE TABLE task_write_set_effect_endpoints (
                 task_id BLOB NOT NULL CHECK(length(task_id) = 16),
                 idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
                 endpoint_seq INTEGER NOT NULL CHECK(endpoint_seq >= 0),
                 effect_seq INTEGER NOT NULL CHECK(effect_seq >= 0),
                 endpoint_kind INTEGER NOT NULL CHECK(endpoint_kind BETWEEN 1 AND 6),
                 object_id BLOB NOT NULL CHECK(length(object_id) = 16),
                 participant_id BLOB NOT NULL CHECK(length(participant_id) = 16),
                 participant_generation BLOB NOT NULL CHECK(length(participant_generation) = 8),
                 admission_receipt_id BLOB NOT NULL CHECK(length(admission_receipt_id) = 16),
                 PRIMARY KEY(task_id, idempotency_key, endpoint_seq),
                 UNIQUE(task_id, idempotency_key, effect_seq, endpoint_kind, object_id),
                 FOREIGN KEY(task_id, idempotency_key)
                     REFERENCES task_write_sets(task_id, idempotency_key)
             ) STRICT;
             CREATE TRIGGER task_write_set_effect_endpoint_is_immutable
             BEFORE UPDATE ON task_write_set_effect_endpoints
             BEGIN SELECT RAISE(ABORT, 'TaskWriteSet effect endpoint is immutable'); END;
             CREATE TRIGGER task_write_set_effect_endpoint_is_immutable_delete
             BEFORE DELETE ON task_write_set_effect_endpoints
             BEGIN SELECT RAISE(ABORT, 'TaskWriteSet effect endpoint is immutable'); END;
             PRAGMA user_version = 39;",
        )
        .unwrap();
    }
    drop(database.open());
    let raw = Connection::open(&database.0).unwrap();
    let version: i64 = raw
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 41);
    let endpoint_sql: String = raw
        .query_row(
            "SELECT sql FROM sqlite_master
             WHERE type='table' AND name='task_write_set_effect_endpoints'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(endpoint_sql.contains("BETWEEN 1 AND 7"));
    let trigger_count: i64 = raw
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name IN
                ('task_write_set_effect_endpoint_is_immutable',
                 'task_write_set_effect_endpoint_is_immutable_delete')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(trigger_count, 2);
}

#[test]
fn endpoint_root_separates_channel_kind_and_empty_set_stays_zero() {
    let database = Database::new();
    let artifact_root = AuthorityRoot::new("artifact");
    let channel_root = AuthorityRoot::new("authority");
    let artifact = nlos_artifact::ArtifactStore::open(&artifact_root.0).unwrap();
    let artifact_id = create_artifact(&artifact, 0x9a);
    let channel_authority = ChannelAuthority::open(&channel_root.0).unwrap();
    let channel = create_channel(&channel_authority);

    let authority = database.open();
    let fixture = register_task_attempt_and_channel_participant(
        &authority,
        &channel_authority,
        artifact_id,
        channel,
    );
    let record = authority
        .seal_task_write_set_with_channel_authority(
            &artifact,
            &channel_authority,
            channel_write_set_request(&fixture, channel.generation, [0x9b; 16]),
        )
        .unwrap()
        .record()
        .clone();

    let endpoint_root_with_kind = |kind_code: u8| -> [u8; 32] {
        let endpoint = &record.effect_endpoints[0];
        let mut hasher = Sha256::new();
        hasher.update(b"llmos/task-write-set-effect-endpoints/v1");
        hasher.update(1u64.to_be_bytes());
        hasher.update(endpoint.effect_seq.to_be_bytes());
        hasher.update([kind_code]);
        hasher.update(endpoint.object_id);
        hasher.update(endpoint.participant_id.as_bytes());
        hasher.update(endpoint.participant_generation.get().to_be_bytes());
        hasher.update(endpoint.admission_receipt_id.as_bytes());
        hasher.finalize().into()
    };
    let channel_kind_root = endpoint_root_with_kind(7);
    let operation_kind_root = endpoint_root_with_kind(6);
    assert_eq!(record.effect_endpoint_set_root, channel_kind_root);
    assert_ne!(channel_kind_root, operation_kind_root);

    let mut endpoint_free = channel_write_set_request(&fixture, channel.generation, [0x9c; 16]);
    endpoint_free.effect_endpoints = Vec::new();
    let endpoint_free_record = authority
        .seal_task_write_set(&artifact, endpoint_free)
        .unwrap()
        .record()
        .clone();
    assert_eq!(endpoint_free_record.effect_endpoint_set_root, [0; 32]);
}
