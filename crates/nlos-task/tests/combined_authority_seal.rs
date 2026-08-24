//! Combined `Authorities` struct boundary tests (B-CHANNEL-001 increment).
//!
//! A write set whose effect endpoints mix `OperationBinding` and
//! `ChannelTopicBinding` spans two authority kinds that no single ladder
//! constructor carries together. These tests pin the gap (every maximal
//! ladder variant fails closed on the missing authority), then exercise
//! the struct-based seal/permit entries that destructure one
//! `Authorities` bundle into the existing inner Option threading.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_channel::{ChannelAuthority, ChannelDecision, CreateChannelRequest, RotateChannelRequest};
use nlos_task::{
    AttemptSpec, Authorities, LogicalEffectDescriptor, ParticipantType, PermitDecision,
    PermitRequest, PlannedEffect, SnapshotBundle, SnapshotConsistency, SqliteTaskAuthority,
    TaskSnapshotReceiptSpec, TaskSpec, TaskStoreError, TaskWriteSetArtifactRead,
    TaskWriteSetEffectEndpointKind, TaskWriteSetEffectEndpointRequest, TaskWriteSetRequest,
    empty_effect_history_root,
};
use nlos_types::{
    ArtifactId, CancellationScopeId, ExecutionFiberId, Generation, IdempotencyKey, OperationId,
    ReceiptId, TaskAttemptId, TaskId, TaskSnapshotId,
};

static NEXT: AtomicU64 = AtomicU64::new(1);

struct Database(PathBuf);

impl Database {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "nlos-task-combined-authority-{}-{}.sqlite3",
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
            "nlos-task-combined-authority-{label}-{}-{}",
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
    TaskId::from_bytes([0x31; 16])
}

fn attempt(seed: u8, head: u64, history: [u8; 32]) -> AttemptSpec {
    attempt_for(task_id(), seed, head, history)
}

fn attempt_for(task: TaskId, seed: u8, head: u64, history: [u8; 32]) -> AttemptSpec {
    AttemptSpec {
        task_id: task,
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

fn planned_effect(slot: u64) -> PlannedEffect {
    PlannedEffect {
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
    }
}

fn issued(decision: PermitDecision) -> nlos_task::PermitRecord {
    match decision {
        PermitDecision::Issued(record) => *record,
        other => panic!("expected issued permit, got {other:?}"),
    }
}

fn binding(
    registry: &nlos_task::ParticipantRegistryRecord,
) -> nlos_task::ParticipantRegistryBinding {
    nlos_task::ParticipantRegistryBinding {
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

fn operation_spec(seed: u8) -> nlos_operation::OperationSpec {
    nlos_operation::OperationSpec {
        operation_id: OperationId::from_bytes([seed; 16]),
        generation: Generation::INITIAL,
        owner_fiber: nlos_runtime::FiberHandle {
            fiber_id: ExecutionFiberId::from_bytes([seed.wrapping_add(1); 16]),
            generation: Generation::INITIAL,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes([seed.wrapping_add(2); 16]),
        cancellation_generation: Generation::INITIAL,
    }
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

struct MixedFixture {
    spec: AttemptSpec,
    operation_id: OperationId,
    operation_generation: Generation,
    channel: nlos_channel::ChannelRecord,
    artifact_id: ArtifactId,
}

fn register_task_attempt_and_both_participants(
    authority: &SqliteTaskAuthority,
    operation_store: &nlos_store::SqliteOperationStore,
    channel_authority: &ChannelAuthority,
    artifact_id: ArtifactId,
    operation: nlos_operation::OperationSpec,
    channel: nlos_channel::ChannelRecord,
) -> MixedFixture {
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
    let operation_binding = binding(&authority.inspect_participant_registry(task_id()).unwrap());
    authority
        .register_operation_binding_participant(
            operation_store,
            task_id(),
            operation_binding,
            operation.operation_id,
            operation.generation,
            1_150,
        )
        .unwrap();
    let channel_binding = binding(&authority.inspect_participant_registry(task_id()).unwrap());
    authority
        .register_channel_participant(
            channel_authority,
            task_id(),
            channel_binding,
            channel.channel_id,
            channel.generation,
            1_160,
        )
        .unwrap();
    MixedFixture {
        spec,
        operation_id: operation.operation_id,
        operation_generation: operation.generation,
        channel,
        artifact_id,
    }
}

fn mixed_write_set_request(fixture: &MixedFixture, key: [u8; 16]) -> TaskWriteSetRequest {
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
        planned_effects: vec![planned_effect(0), planned_effect(1)],
        effect_endpoints: vec![
            TaskWriteSetEffectEndpointRequest::OperationBinding {
                effect_seq: 0,
                operation_id: fixture.operation_id,
                expected_operation_generation: fixture.operation_generation,
            },
            TaskWriteSetEffectEndpointRequest::ChannelTopicBinding {
                effect_seq: 1,
                channel_id: fixture.channel.channel_id,
                expected_channel_generation: fixture.channel.generation,
            },
        ],
        idempotency_key: IdempotencyKey::from_bytes(key),
        sealed_at_ms: 1_200,
    }
}

/// Artifact/Operation/Channel owner authorities plus a task registry
/// holding both endpoint participants. The returned directory guards keep
/// the authority roots alive for the whole test so a reopen-after-drop
/// still finds the durable files (guards drop after the reopened handles).
struct FixtureRoots {
    _artifact: AuthorityRoot,
    _operation: AuthorityRoot,
    _channel: AuthorityRoot,
}

struct Owners {
    artifact: nlos_artifact::ArtifactStore,
    operation_store: nlos_store::SqliteOperationStore,
    channel_authority: ChannelAuthority,
    operation_path: PathBuf,
    channel_path: PathBuf,
}

fn owners_and_fixture(
    database: &Database,
) -> (FixtureRoots, Owners, SqliteTaskAuthority, MixedFixture) {
    let artifact_root = AuthorityRoot::new("artifact");
    let operation_root = AuthorityRoot::new("operation");
    let channel_root = AuthorityRoot::new("channel");
    std::fs::create_dir_all(&operation_root.0).unwrap();
    let operation_path = operation_root.0.join("authority.sqlite3");
    let artifact = nlos_artifact::ArtifactStore::open(&artifact_root.0).unwrap();
    let artifact_id = create_artifact(&artifact, 0xc1);
    let operation_store = nlos_store::SqliteOperationStore::open(&operation_path).unwrap();
    let operation = operation_spec(0xc2);
    operation_store.register(operation).unwrap();
    let channel_authority = ChannelAuthority::open(&channel_root.0).unwrap();
    let channel = create_channel(&channel_authority);

    let authority = database.open();
    let fixture = register_task_attempt_and_both_participants(
        &authority,
        &operation_store,
        &channel_authority,
        artifact_id,
        operation,
        channel,
    );
    let registry = authority.inspect_participant_registry(task_id()).unwrap();
    assert!(
        registry
            .participants
            .iter()
            .any(|participant| participant.participant_type == ParticipantType::OperationBinding)
            && registry
                .participants
                .iter()
                .any(|participant| participant.participant_type == ParticipantType::ChannelTopic)
    );
    let owners = Owners {
        artifact,
        operation_store,
        channel_authority,
        operation_path,
        channel_path: channel_root.0.clone(),
    };
    (
        FixtureRoots {
            _artifact: artifact_root,
            _operation: operation_root,
            _channel: channel_root,
        },
        owners,
        authority,
        fixture,
    )
}

fn mixed_authorities(owners: &Owners) -> Authorities<'_> {
    Authorities {
        artifact: Some(&owners.artifact),
        operation: Some(&owners.operation_store),
        channel: Some(&owners.channel_authority),
        ..Default::default()
    }
}

#[test]
fn ladder_variants_cannot_seal_mixed_operation_channel_write_set() {
    let database = Database::new();
    let (_roots, owners, authority, fixture) = owners_and_fixture(&database);
    let request = mixed_write_set_request(&fixture, [0xa1; 16]);

    // Even the maximal ladder variant carrying an Operation authority
    // (Process + Semantic + Resource + Operation) has no Channel slot.
    let process_root = AuthorityRoot::new("process");
    let process = nlos_process::ProcessAuthority::open(&process_root.0).unwrap();
    let semantic_root = AuthorityRoot::new("semantic");
    let semantic = nlos_semantic::SemanticAuthority::open(&semantic_root.0).unwrap();
    let resource_root = AuthorityRoot::new("resource");
    let resource = nlos_resource::ResourceAuthority::open(&resource_root.0).unwrap();
    assert!(matches!(
        authority.seal_task_write_set_with_authorities_and_operation_authority(
            &owners.artifact,
            &process,
            &semantic,
            &resource,
            &owners.operation_store,
            request.clone(),
        ),
        Err(TaskStoreError::TaskWriteSetConflict {
            reason: "Channel effect endpoint requires ChannelAuthority readback"
        })
    ));
    // The maximal Channel variant is the mirror image: no Operation slot.
    assert!(matches!(
        authority.seal_task_write_set_with_channel_authority(
            &owners.artifact,
            &owners.channel_authority,
            request.clone(),
        ),
        Err(TaskStoreError::TaskWriteSetConflict {
            reason: "Operation effect endpoint requires OperationAuthority readback"
        })
    ));
    // Every other ladder constructor carries a subset of one of these two
    // authority sets, so no existing entry can seal this write set, and no
    // partial seal row may survive either attempt.
    assert!(matches!(
        authority.inspect_task_write_set(task_id(), IdempotencyKey::from_bytes([0xa1; 16])),
        Err(TaskStoreError::TaskWriteSetNotFound)
    ));
}

#[test]
#[allow(clippy::too_many_lines)]
fn mixed_write_set_seals_permits_and_replays_via_authorities_struct() {
    let database = Database::new();
    let (_roots, owners, authority, fixture) = owners_and_fixture(&database);
    let authorities = mixed_authorities(&owners);
    let record = authority
        .seal_task_write_set_with_authorities_struct(
            authorities,
            mixed_write_set_request(&fixture, [0xa2; 16]),
        )
        .unwrap()
        .record()
        .clone();
    assert_eq!(record.effect_endpoints.len(), 2);
    assert_eq!(
        record.effect_endpoints[0].kind,
        TaskWriteSetEffectEndpointKind::OperationBinding
    );
    assert_eq!(
        record.effect_endpoints[0].object_id,
        fixture.operation_id.into_bytes()
    );
    assert_eq!(
        record.effect_endpoints[0].participant_generation,
        fixture.operation_generation
    );
    assert_eq!(
        record.effect_endpoints[0].participant_id,
        owners
            .operation_store
            .inspect_endpoint_proof(nlos_operation::OperationHandle {
                operation_id: fixture.operation_id,
                generation: fixture.operation_generation,
            })
            .unwrap()
            .participant_id
    );
    assert_eq!(
        record.effect_endpoints[1].kind,
        TaskWriteSetEffectEndpointKind::ChannelTopicBinding
    );
    assert_eq!(
        record.effect_endpoints[1].object_id,
        fixture.channel.channel_id.into_bytes()
    );
    assert_eq!(
        record.effect_endpoints[1].participant_generation,
        fixture.channel.generation
    );
    assert_eq!(
        record.effect_endpoints[1].participant_id,
        owners
            .channel_authority
            .inspect_endpoint_proof(fixture.channel.channel_id)
            .unwrap()
            .participant_id
    );

    let mut permit_request = permit(&fixture.spec, 0xa3);
    permit_request.write_set_root = record.write_set_root;
    permit_request.planned_effects = record.planned_effects.clone();
    let permit_record = issued(
        authority
            .request_commit_permit_with_authorities_struct(authorities, permit_request.clone())
            .unwrap(),
    );
    assert!(matches!(
        authority
            .request_commit_permit_with_authorities_struct(authorities, permit_request)
            .unwrap(),
        PermitDecision::Replayed(_)
    ));
    assert_eq!(permit_record.write_set_root, record.write_set_root);

    drop(authority);
    let operation_path = owners.operation_path.clone();
    let channel_path = owners.channel_path.clone();
    drop(owners);
    let reopened_operation = nlos_store::SqliteOperationStore::open(&operation_path).unwrap();
    let reopened_channel = ChannelAuthority::open(&channel_path).unwrap();
    assert_eq!(
        reopened_operation
            .inspect_endpoint_proof(nlos_operation::OperationHandle {
                operation_id: fixture.operation_id,
                generation: fixture.operation_generation,
            })
            .unwrap()
            .participant_generation,
        fixture.operation_generation
    );
    assert_eq!(
        reopened_channel
            .inspect_endpoint_proof(fixture.channel.channel_id)
            .unwrap()
            .participant_generation,
        fixture.channel.generation
    );
    let reopened = database.open();
    assert_eq!(
        reopened
            .inspect_task_write_set(task_id(), IdempotencyKey::from_bytes([0xa2; 16]))
            .unwrap(),
        record
    );
}

#[test]
fn struct_seal_without_channel_authority_fails_closed() {
    let database = Database::new();
    let (_roots, owners, authority, fixture) = owners_and_fixture(&database);
    let authorities = Authorities {
        artifact: Some(&owners.artifact),
        operation: Some(&owners.operation_store),
        channel: None,
        ..Default::default()
    };
    assert!(matches!(
        authority.seal_task_write_set_with_authorities_struct(
            authorities,
            mixed_write_set_request(&fixture, [0xa4; 16])
        ),
        Err(TaskStoreError::TaskWriteSetConflict {
            reason: "Channel effect endpoint requires ChannelAuthority readback"
        })
    ));
    assert!(matches!(
        authority.inspect_task_write_set(task_id(), IdempotencyKey::from_bytes([0xa4; 16])),
        Err(TaskStoreError::TaskWriteSetNotFound)
    ));
}

#[test]
fn channel_rotation_between_struct_seal_and_struct_permit_blocks_freeze() {
    let database = Database::new();
    let (_roots, owners, authority, fixture) = owners_and_fixture(&database);
    let authorities = mixed_authorities(&owners);
    let record = authority
        .seal_task_write_set_with_authorities_struct(
            authorities,
            mixed_write_set_request(&fixture, [0xa5; 16]),
        )
        .unwrap()
        .record()
        .clone();

    owners
        .channel_authority
        .rotate_channel(RotateChannelRequest {
            channel_id: fixture.channel.channel_id,
            expected_generation: fixture.channel.generation,
            expected_fencing_token: fixture.channel.fencing_token,
            idempotency_key: IdempotencyKey::from_bytes([0xa6; 16]),
            rotated_at_ms: 1_250,
        })
        .unwrap();
    let mut permit_request = permit(&fixture.spec, 0xa7);
    permit_request.write_set_root = record.write_set_root;
    permit_request.planned_effects = record.planned_effects.clone();
    assert!(matches!(
        authority.request_commit_permit_with_authorities_struct(authorities, permit_request),
        Err(TaskStoreError::TaskWriteSetConflict {
            reason: "Channel endpoint proof differs before permit freeze"
        })
    ));
}

#[test]
fn authority_absent_struct_permit_fails_closed_at_freeze() {
    let database = Database::new();
    let (_roots, owners, authority, fixture) = owners_and_fixture(&database);
    let authorities = mixed_authorities(&owners);
    let record = authority
        .seal_task_write_set_with_authorities_struct(
            authorities,
            mixed_write_set_request(&fixture, [0xa8; 16]),
        )
        .unwrap()
        .record()
        .clone();
    let mut permit_request = permit(&fixture.spec, 0xa9);
    permit_request.write_set_root = record.write_set_root;
    permit_request.planned_effects = record.planned_effects.clone();
    let without_channel = Authorities {
        artifact: Some(&owners.artifact),
        operation: Some(&owners.operation_store),
        ..Default::default()
    };
    assert!(matches!(
        authority
            .request_commit_permit_with_authorities_struct(without_channel, permit_request.clone()),
        Err(TaskStoreError::TaskWriteSetConflict {
            reason: "Channel effect endpoint requires ChannelAuthority readback before permit freeze"
        })
    ));
    let without_operation = Authorities {
        artifact: Some(&owners.artifact),
        channel: Some(&owners.channel_authority),
        ..Default::default()
    };
    assert!(matches!(
        authority.request_commit_permit_with_authorities_struct(
            without_operation,
            permit_request.clone()
        ),
        Err(TaskStoreError::TaskWriteSetConflict {
            reason: "Operation effect endpoint requires OperationAuthority readback before permit freeze"
        })
    ));
    // Neither failed freeze attempt may leave durable state behind: the
    // fully-authority'd struct permit still issues on the first call.
    assert!(matches!(
        authority.request_commit_permit_with_authorities_struct(authorities, permit_request),
        Ok(PermitDecision::Issued(_))
    ));
}

#[test]
#[allow(clippy::too_many_lines)]
fn all_none_authorities_struct_matches_legacy_seal_and_permit() {
    let artifact_root = AuthorityRoot::new("artifact");
    let artifact = nlos_artifact::ArtifactStore::open(&artifact_root.0).unwrap();
    let artifact_id = create_artifact(&artifact, 0xc1);
    let database = Database::new();
    let authority = database.open();

    let write_set = |task: TaskId, spec: &AttemptSpec, key: [u8; 16]| TaskWriteSetRequest {
        task_id: task,
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
        planned_effects: Vec::new(),
        effect_endpoints: Vec::new(),
        idempotency_key: IdempotencyKey::from_bytes(key),
        sealed_at_ms: 1_200,
    };
    let artifact_only = Authorities {
        artifact: Some(&artifact),
        ..Default::default()
    };

    // Given: one task sealed through the legacy plain entry and replayed
    // through the struct entry before any registry-freezing permit runs.
    let legacy_task = task_id();
    let legacy_spec = register_authority_free_attempt(&authority, legacy_task, 0xaa);
    let legacy_record = authority
        .seal_task_write_set(&artifact, write_set(legacy_task, &legacy_spec, [0xc0; 16]))
        .unwrap()
        .record()
        .clone();
    assert!(matches!(
        authority.seal_task_write_set_with_authorities_struct(
            artifact_only,
            write_set(legacy_task, &legacy_spec, [0xc0; 16])
        ),
        Ok(nlos_task::TaskWriteSetDecision::Replayed(record)) if record == legacy_record
    ));
    let mut legacy_permit_request = permit(&legacy_spec, 0xc1);
    legacy_permit_request.write_set_root = legacy_record.write_set_root;
    legacy_permit_request.planned_effects = legacy_record.planned_effects.clone();
    let legacy_permit = issued(
        authority
            .request_commit_permit(legacy_permit_request.clone())
            .unwrap(),
    );

    // ...and a second task sealed through the struct entry with only the
    // (seal-mandatory) Artifact authority set and everything else `None`,
    // replayed through the legacy entry. Full-record equality across the
    // two tasks is impossible by design: the registry root covers the
    // per-database `task_authority_identity` randomblob, so parity is
    // asserted through bidirectional idempotent replay of identical
    // request bytes instead.
    let struct_task = TaskId::from_bytes([0x32; 16]);
    let struct_spec = register_authority_free_attempt(&authority, struct_task, 0x9a);
    let struct_record = authority
        .seal_task_write_set_with_authorities_struct(
            artifact_only,
            write_set(struct_task, &struct_spec, [0xc0; 16]),
        )
        .unwrap()
        .record()
        .clone();
    assert!(matches!(
        authority.seal_task_write_set(
            &artifact,
            write_set(struct_task, &struct_spec, [0xc0; 16])
        ),
        Ok(nlos_task::TaskWriteSetDecision::Replayed(record)) if record == struct_record
    ));
    let mut struct_permit_request = permit(&struct_spec, 0xc1);
    struct_permit_request.write_set_root = struct_record.write_set_root;
    struct_permit_request.planned_effects = struct_record.planned_effects.clone();
    let struct_permit = issued(
        authority
            .request_commit_permit_with_authorities_struct(
                Authorities::default(),
                struct_permit_request.clone(),
            )
            .unwrap(),
    );

    // Then: each permit entry replays the other's durable record as well.
    assert!(matches!(
        authority.request_commit_permit_with_authorities_struct(
            Authorities::default(),
            legacy_permit_request
        ),
        Ok(PermitDecision::Replayed(record)) if *record == legacy_permit
    ));
    assert!(matches!(
        authority.request_commit_permit(struct_permit_request),
        Ok(PermitDecision::Replayed(record)) if *record == struct_permit
    ));
}

fn register_authority_free_attempt(
    authority: &SqliteTaskAuthority,
    task: TaskId,
    seed: u8,
) -> AttemptSpec {
    authority
        .register_task(TaskSpec {
            task_id: task,
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
        })
        .unwrap();
    let spec = attempt_for(task, seed, 0, empty_effect_history_root());
    let snapshot_receipt_id = ReceiptId::from_bytes([seed.wrapping_add(1); 16]);
    authority
        .register_snapshot_receipt(TaskSnapshotReceiptSpec {
            task_id: task,
            snapshot: spec.snapshot,
            receipt_id: snapshot_receipt_id,
            builder_id: [seed.wrapping_add(2); 16],
            builder_version_digest: [seed.wrapping_add(3); 32],
            per_authority_checkpoint_receipts: vec![ReceiptId::from_bytes(
                [seed.wrapping_add(4); 16],
            )],
            dependency_closure_root: [seed.wrapping_add(5); 32],
            semantic_resolver_digest: [seed.wrapping_add(6); 32],
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
    spec
}
