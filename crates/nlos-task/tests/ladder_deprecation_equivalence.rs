//! Deprecation semantics for the seal/permit ladder constructors.
//!
//! Every ladder constructor is deprecated in favor of the struct entries
//! `seal_task_write_set_with_authorities_struct` /
//! `request_commit_permit_with_authorities_struct` (the authority-lease
//! permit variant stays undeprecated because the struct entry carries no
//! lease slot). These tests sample representative deprecated ladder
//! variants and pin that they remain callable and behaviorally equivalent
//! to the struct entry with the matching `Authorities` bundle: sealing or
//! permitting through one entry replays identically through the other.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_channel::{ChannelAuthority, ChannelDecision, CreateChannelRequest};
use nlos_task::{
    Authorities, LogicalEffectDescriptor, PermitDecision, PermitRequest, PlannedEffect,
    SnapshotBundle, SnapshotConsistency, SqliteTaskAuthority, TaskSnapshotReceiptSpec, TaskSpec,
    TaskWriteSetArtifactRead, TaskWriteSetEffectEndpointKind, TaskWriteSetEffectEndpointRequest,
    TaskWriteSetRequest, empty_effect_history_root,
};
use nlos_types::{
    ArtifactId, CancellationScopeId, Generation, IdempotencyKey, ReceiptId, TaskAttemptId, TaskId,
    TaskSnapshotId,
};

static NEXT: AtomicU64 = AtomicU64::new(1);

struct Database(PathBuf);

impl Database {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "nlos-task-ladder-deprecation-{}-{}.sqlite3",
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

fn unique_root(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "nlos-task-ladder-deprecation-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

fn task_id() -> TaskId {
    TaskId::from_bytes([0x41; 16])
}

/// Registers a task and an attempt with one Channel effect endpoint, then
/// returns the attempt spec, the channel, and the owner authorities.
#[allow(clippy::type_complexity)]
fn fixture() -> (
    SqliteTaskAuthority,
    nlos_artifact::ArtifactStore,
    ChannelAuthority,
    nlos_channel::ChannelRecord,
    nlos_task::AttemptSpec,
) {
    let artifact_root = unique_root("artifact");
    let channel_root = unique_root("channel");
    let artifact = nlos_artifact::ArtifactStore::open(&artifact_root).unwrap();
    let artifact_id = ArtifactId::from_bytes([0xc1; 16]);
    artifact
        .create_artifact(nlos_artifact::CreateArtifactSpec {
            artifact_id,
            idempotency_key: IdempotencyKey::from_bytes([0xc2; 16]),
            content_type: "application/octet-stream".to_owned(),
            application_id: None,
            owner: None,
            created_at_ms: 1_050,
        })
        .unwrap();
    let channel_authority = ChannelAuthority::open(&channel_root).unwrap();
    let channel = match channel_authority
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
    };

    let authority = Database::new().open();
    authority
        .register_task(TaskSpec {
            task_id: task_id(),
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
        })
        .unwrap();
    let spec = nlos_task::AttemptSpec {
        task_id: task_id(),
        attempt_id: TaskAttemptId::from_bytes([0xb3; 16]),
        attempt_generation: Generation::INITIAL,
        snapshot: SnapshotBundle {
            snapshot_id: TaskSnapshotId::from_bytes([0xb4; 16]),
            snapshot_digest: [0xb5; 32],
            expected_head_commit_seq: 0,
            effect_history_root: empty_effect_history_root(),
            retry_fence_epoch: 0,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes([0xb6; 16]),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes([0xb7; 16]),
        registered_at_ms: 2_000,
    };
    let snapshot_receipt_id = ReceiptId::from_bytes([0xb8; 16]);
    authority
        .register_snapshot_receipt(TaskSnapshotReceiptSpec {
            task_id: task_id(),
            snapshot: spec.snapshot,
            receipt_id: snapshot_receipt_id,
            builder_id: [0xb9; 16],
            builder_version_digest: [0xba; 32],
            per_authority_checkpoint_receipts: vec![ReceiptId::from_bytes([0xbb; 16])],
            dependency_closure_root: [0xbc; 32],
            semantic_resolver_digest: [0xbd; 32],
            canonical_iteration_digest: [0xbe; 32],
            achieved_consistency: SnapshotConsistency::Causal,
            built_at_ms: 1_100,
            authority_id: [0xbf; 16],
            key_id: [0xc0; 16],
            signature: [0xc1; 64],
        })
        .unwrap();
    authority
        .register_attempt_with_snapshot_receipt(spec, snapshot_receipt_id)
        .unwrap();
    let registry = authority.inspect_participant_registry(task_id()).unwrap();
    let binding = nlos_task::ParticipantRegistryBinding {
        generation: registry.generation,
        root: registry.root,
    };
    authority
        .register_channel_participant(
            &channel_authority,
            task_id(),
            binding,
            channel.channel_id,
            channel.generation,
            1_160,
        )
        .unwrap();
    (authority, artifact, channel_authority, channel, spec)
}

fn write_set_request(
    spec: &nlos_task::AttemptSpec,
    channel: &nlos_channel::ChannelRecord,
    artifact_id: ArtifactId,
    key: [u8; 16],
    slot: u64,
) -> TaskWriteSetRequest {
    TaskWriteSetRequest {
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
        planned_effects: vec![PlannedEffect {
            descriptor: LogicalEffectDescriptor {
                task_id: task_id(),
                task_generation: Generation::INITIAL,
                intent_spec_id: [0xa1; 32],
                stable_action_slot: slot,
                target_authority_object_id: [0xa2; 32],
                effect_class: 1,
                idempotency_scope: 1,
            },
            required: false,
            required_condition_digest: None,
            success_criteria_digest: [0xa3; 32],
            action_proposal_digest: [0xa4; 32],
        }],
        effect_endpoints: vec![TaskWriteSetEffectEndpointRequest::ChannelTopicBinding {
            effect_seq: 0,
            channel_id: channel.channel_id,
            expected_channel_generation: channel.generation,
        }],
        idempotency_key: IdempotencyKey::from_bytes(key),
        sealed_at_ms: 1_200,
    }
}

fn permit_request(
    spec: &nlos_task::AttemptSpec,
    record: &nlos_task::TaskWriteSetRecord,
    seed: u8,
) -> PermitRequest {
    PermitRequest {
        task_id: spec.task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        write_set_root: record.write_set_root,
        planned_effects: record.planned_effects.clone(),
        idempotency_key: IdempotencyKey::from_bytes([seed; 16]),
        valid_until_ms: 9_000,
        requested_at_ms: 3_000 + i64::from(seed),
    }
}

fn issued(decision: PermitDecision) -> nlos_task::PermitRecord {
    match decision {
        PermitDecision::Issued(record) => *record,
        other => panic!("expected issued permit, got {other:?}"),
    }
}

/// The deprecated Channel seal ladder stays callable and its durable
/// record replays byte-identically through the struct entry with the same
/// Channel authority, and vice versa.
#[test]
#[allow(deprecated)]
fn deprecated_channel_seal_ladder_matches_struct_entry() {
    let (authority, artifact, channel_authority, channel, spec) = fixture();
    let artifact_id = ArtifactId::from_bytes([0xc1; 16]);
    let authorities = Authorities {
        artifact: Some(&artifact),
        channel: Some(&channel_authority),
        ..Default::default()
    };

    let ladder_record = authority
        .seal_task_write_set_with_channel_authority(
            &artifact,
            &channel_authority,
            write_set_request(&spec, &channel, artifact_id, [0xd1; 16], 0),
        )
        .unwrap()
        .record()
        .clone();
    assert_eq!(ladder_record.effect_endpoints.len(), 1);
    assert_eq!(
        ladder_record.effect_endpoints[0].kind,
        TaskWriteSetEffectEndpointKind::ChannelTopicBinding
    );
    assert!(matches!(
        authority.seal_task_write_set_with_authorities_struct(
            authorities,
            write_set_request(&spec, &channel, artifact_id, [0xd1; 16], 0)
        ),
        Ok(nlos_task::TaskWriteSetDecision::Replayed(record)) if record == ladder_record
    ));

    let struct_record = authority
        .seal_task_write_set_with_authorities_struct(
            authorities,
            write_set_request(&spec, &channel, artifact_id, [0xd2; 16], 1),
        )
        .unwrap()
        .record()
        .clone();
    assert!(matches!(
        authority.seal_task_write_set_with_channel_authority(
            &artifact,
            &channel_authority,
            write_set_request(&spec, &channel, artifact_id, [0xd2; 16], 1)
        ),
        Ok(nlos_task::TaskWriteSetDecision::Replayed(record)) if record == struct_record
    ));
}

/// The deprecated Channel permit ladder stays callable and its durable
/// permit replays identically through the struct entry, and vice versa.
#[test]
#[allow(deprecated)]
fn deprecated_channel_permit_ladder_matches_struct_entry() {
    // Direction 1: the ladder entry issues; the struct entry replays the
    // exact same durable permit for the same idempotency key.
    let (authority, artifact, channel_authority, channel, spec) = fixture();
    let artifact_id = ArtifactId::from_bytes([0xc1; 16]);
    let authorities = Authorities {
        artifact: Some(&artifact),
        channel: Some(&channel_authority),
        ..Default::default()
    };
    let record = authority
        .seal_task_write_set_with_authorities_struct(
            authorities,
            write_set_request(&spec, &channel, artifact_id, [0xd3; 16], 0),
        )
        .unwrap()
        .record()
        .clone();

    let ladder_permit = issued(
        authority
            .request_commit_permit_with_channel_authority(
                &channel_authority,
                permit_request(&spec, &record, 0xd4),
            )
            .unwrap(),
    );
    assert!(matches!(
        authority.request_commit_permit_with_authorities_struct(
            authorities,
            permit_request(&spec, &record, 0xd4)
        ),
        Ok(PermitDecision::Replayed(record)) if *record == ladder_permit
    ));

    // Direction 2: the struct entry issues on a fresh task; the ladder
    // entry replays the exact same durable permit.
    let (authority, artifact, channel_authority, channel, spec) = fixture();
    let artifact_id = ArtifactId::from_bytes([0xc1; 16]);
    let authorities = Authorities {
        artifact: Some(&artifact),
        channel: Some(&channel_authority),
        ..Default::default()
    };
    let record = authority
        .seal_task_write_set_with_authorities_struct(
            authorities,
            write_set_request(&spec, &channel, artifact_id, [0xd3; 16], 0),
        )
        .unwrap()
        .record()
        .clone();
    let struct_permit = issued(
        authority
            .request_commit_permit_with_authorities_struct(
                authorities,
                permit_request(&spec, &record, 0xd5),
            )
            .unwrap(),
    );
    assert!(matches!(
        authority.request_commit_permit_with_channel_authority(
            &channel_authority,
            permit_request(&spec, &record, 0xd5)
        ),
        Ok(PermitDecision::Replayed(record)) if *record == struct_permit
    ));
}
