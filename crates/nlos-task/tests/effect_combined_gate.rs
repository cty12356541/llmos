//! Combined effect-permit gate: the struct-based issuance entry
//! (`request_effect_permit_with_authorities_struct`) must run BOTH the
//! `[B-OP-FENCE-003]` activation gate and the `B-CHANNEL-001` channel gate
//! for a sealed slot carrying an `OperationBinding` AND a
//! `ChannelTopicBinding` endpoint, fail closed on either authority's
//! rotation between seal and mint, fall back to single-gate / legacy
//! semantics when a slot is absent, and replay strictly from the Task rows.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_channel::{ChannelAuthority, ChannelDecision, CreateChannelRequest, RotateChannelRequest};
use nlos_runtime::FiberHandle;
use nlos_task::{
    AttemptSpec, Authorities, EffectPermitAuthorities, EffectPermitDecision, EffectPermitRequest,
    IssuedPermit, LogicalEffectDescriptor, ParticipantRegistryBinding, PermitDecision,
    PermitRecord, PermitRequest, PlannedEffect, SlotState, SnapshotBundle, SnapshotConsistency,
    SqliteTaskAuthority, TaskSnapshotReceiptSpec, TaskSpec, TaskWriteSetArtifactRead,
    TaskWriteSetEffectEndpointRequest, TaskWriteSetRequest, empty_effect_history_root,
};
use nlos_types::{
    ArtifactId, CallbackId, CancellationScopeId, ExecutionFiberId, Generation, IdempotencyKey,
    OperationId, ReceiptId, TaskAttemptId, TaskId, TaskSnapshotId,
};

static NEXT: AtomicU64 = AtomicU64::new(1);

struct Database(PathBuf);

impl Database {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "nlos-task-effect-combined-gate-{}-{}.sqlite3",
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
            "nlos-task-effect-combined-gate-{label}-{}-{}",
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
    TaskId::from_bytes([0x41; 16])
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

fn commit_permit_request(spec: &AttemptSpec, seed: u8) -> PermitRequest {
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
            intent_spec_id: [0xf1; 32],
            stable_action_slot: 0,
            target_authority_object_id: [0xf2; 32],
            effect_class: 1,
            idempotency_scope: 1,
        },
        required: false,
        required_condition_digest: None,
        success_criteria_digest: [0xf3; 32],
        action_proposal_digest: [0xf4; 32],
    }
}

fn issued_commit(decision: PermitDecision) -> PermitRecord {
    match decision {
        PermitDecision::Issued(record) => *record,
        other => panic!("expected issued commit permit, got {other:?}"),
    }
}

fn issued_effect_permit(decision: EffectPermitDecision) -> IssuedPermit {
    match decision {
        EffectPermitDecision::Issued(record) => *record,
        other @ EffectPermitDecision::Replayed(_) => {
            panic!("expected issued effect permit, got {other:?}")
        }
    }
}

fn replayed_effect_permit(decision: EffectPermitDecision) -> IssuedPermit {
    match decision {
        EffectPermitDecision::Replayed(record) => *record,
        other @ EffectPermitDecision::Issued(_) => {
            panic!("expected replayed effect permit, got {other:?}")
        }
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

fn operation_spec(seed: u8) -> nlos_operation::OperationSpec {
    nlos_operation::OperationSpec {
        operation_id: OperationId::from_bytes([seed; 16]),
        generation: Generation::INITIAL,
        owner_fiber: FiberHandle {
            fiber_id: ExecutionFiberId::from_bytes([seed.wrapping_add(1); 16]),
            generation: Generation::INITIAL,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes([seed.wrapping_add(2); 16]),
        cancellation_generation: Generation::INITIAL,
    }
}

fn operation_handle(spec: &nlos_operation::OperationSpec) -> nlos_operation::OperationHandle {
    nlos_operation::OperationHandle {
        operation_id: spec.operation_id,
        generation: spec.generation,
    }
}

fn activate_operation(
    store: &nlos_store::SqliteOperationStore,
    spec: &nlos_operation::OperationSpec,
) {
    let preparation = match store
        .prepare_dispatch(operation_handle(spec), CallbackId::from_bytes([0xd1; 16]))
        .unwrap()
    {
        nlos_store::OperationPrepareDecision::Prepared(preparation)
        | nlos_store::OperationPrepareDecision::Replayed(preparation) => preparation,
    };
    store.activate_dispatch(preparation).unwrap();
}

fn create_channel(authority: &ChannelAuthority) -> nlos_channel::ChannelRecord {
    match authority
        .create_channel(CreateChannelRequest {
            capacity_bytes: 4096,
            policy_digest: [0xf5; 32],
            idempotency_key: IdempotencyKey::from_bytes([0xf6; 16]),
            created_at_ms: 1_060,
        })
        .unwrap()
    {
        ChannelDecision::Created(record) => record,
        ChannelDecision::Replayed(_) => panic!("first channel create cannot replay"),
    }
}

fn registered_attempt(authority: &SqliteTaskAuthority, seed: u8) -> AttemptSpec {
    authority
        .register_task(TaskSpec {
            task_id: task_id(),
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
        })
        .unwrap();
    let spec = attempt(seed, 0, empty_effect_history_root());
    let receipt_id = ReceiptId::from_bytes([seed.wrapping_add(1); 16]);
    authority
        .register_snapshot_receipt(TaskSnapshotReceiptSpec {
            task_id: task_id(),
            snapshot: spec.snapshot,
            receipt_id,
            builder_id: [seed.wrapping_add(2); 16],
            builder_version_digest: [seed.wrapping_add(3); 32],
            per_authority_checkpoint_receipts: vec![ReceiptId::from_bytes(
                [seed.wrapping_add(4); 16],
            )],
            dependency_closure_root: [seed.wrapping_add(5); 32],
            semantic_resolver_digest: [seed.wrapping_add(6); 32],
            canonical_iteration_digest: [seed.wrapping_add(7); 32],
            achieved_consistency: SnapshotConsistency::Causal,
            built_at_ms: 1_100,
            authority_id: [seed.wrapping_add(8); 16],
            key_id: [seed.wrapping_add(9); 16],
            signature: [seed.wrapping_add(10); 64],
        })
        .unwrap();
    authority
        .register_attempt_with_snapshot_receipt(spec, receipt_id)
        .unwrap();
    spec
}

/// Full combined-gate Given-fixture: task + attempt + snapshot receipt,
/// registered Operation AND Channel participants, and a sealed write set
/// whose effect slot 0 carries BOTH an `OperationBinding` and a
/// `ChannelTopicBinding` endpoint, sealed and commit-permitted through the
/// struct-based `Authorities` bundle. The root fields are carried for their
/// RAII lifetimes: the owner directories must outlive the stores each test
/// reopens from them.
struct SealedDualSlot {
    database: Database,
    _artifact_root: AuthorityRoot,
    operation_path: PathBuf,
    _operation_root: AuthorityRoot,
    channel_root_path: PathBuf,
    _channel_root: AuthorityRoot,
    operation: nlos_operation::OperationSpec,
    channel: nlos_channel::ChannelRecord,
    spec: AttemptSpec,
    commit_permit: PermitRecord,
}

#[allow(clippy::too_many_lines)]
fn sealed_dual_slot(label: &str) -> SealedDualSlot {
    let database = Database::new();
    let artifact_root = AuthorityRoot::new(label);
    let operation_root = AuthorityRoot::new(label);
    let channel_root = AuthorityRoot::new(label);
    let artifact = nlos_artifact::ArtifactStore::open(&artifact_root.0).unwrap();
    let artifact_id = create_artifact(&artifact, 0xc1);
    std::fs::create_dir_all(&operation_root.0).unwrap();
    let operation_path = operation_root.0.join("authority.sqlite3");
    let operation_store = nlos_store::SqliteOperationStore::open(&operation_path).unwrap();
    let operation = operation_spec(0xc2);
    operation_store.register(operation).unwrap();
    let channel_authority = ChannelAuthority::open(&channel_root.0).unwrap();
    let channel = create_channel(&channel_authority);

    let authority = database.open();
    let spec = registered_attempt(&authority, 0xc3);
    let registry_binding = binding(&authority.inspect_participant_registry(task_id()).unwrap());
    authority
        .register_operation_binding_participant(
            &operation_store,
            task_id(),
            registry_binding,
            operation.operation_id,
            operation.generation,
            1_150,
        )
        .unwrap();
    let registry_binding = binding(&authority.inspect_participant_registry(task_id()).unwrap());
    authority
        .register_channel_participant(
            &channel_authority,
            task_id(),
            registry_binding,
            channel.channel_id,
            channel.generation,
            1_160,
        )
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
        effect_endpoints: vec![
            TaskWriteSetEffectEndpointRequest::OperationBinding {
                effect_seq: 0,
                operation_id: operation.operation_id,
                expected_operation_generation: operation.generation,
            },
            TaskWriteSetEffectEndpointRequest::ChannelTopicBinding {
                effect_seq: 0,
                channel_id: channel.channel_id,
                expected_channel_generation: channel.generation,
            },
        ],
        idempotency_key: IdempotencyKey::from_bytes([0xfe; 16]),
        sealed_at_ms: 1_200,
    };
    let record = authority
        .seal_task_write_set_with_authorities_struct(
            Authorities {
                artifact: Some(&artifact),
                operation: Some(&operation_store),
                channel: Some(&channel_authority),
                ..Authorities::default()
            },
            request,
        )
        .unwrap()
        .record()
        .clone();
    let mut commit_request = commit_permit_request(&spec, 0xcf);
    commit_request.write_set_root = record.write_set_root;
    commit_request
        .planned_effects
        .clone_from(&record.planned_effects);
    let commit_permit = issued_commit(
        authority
            .request_commit_permit_with_authorities_struct(
                Authorities {
                    operation: Some(&operation_store),
                    channel: Some(&channel_authority),
                    ..Authorities::default()
                },
                commit_request,
            )
            .unwrap(),
    );
    SealedDualSlot {
        database,
        _artifact_root: artifact_root,
        operation_path,
        _operation_root: operation_root,
        channel_root_path: channel_root.0.clone(),
        _channel_root: channel_root,
        operation,
        channel,
        spec,
        commit_permit,
    }
}

fn effect_request(slot: &SealedDualSlot, key_seed: u8) -> EffectPermitRequest {
    EffectPermitRequest {
        task_id: task_id(),
        attempt_id: slot.spec.attempt_id,
        attempt_generation: slot.spec.attempt_generation,
        permit_id: slot.commit_permit.permit_id,
        permit_epoch: slot.commit_permit.permit_epoch,
        effect_seq: 0,
        idempotency_key: IdempotencyKey::from_bytes([key_seed; 16]),
        valid_until_ms: 9_000,
        requested_at_ms: 4_000,
    }
}

fn both_gates<'a>(
    operation: &'a nlos_store::SqliteOperationStore,
    channel: &'a ChannelAuthority,
) -> EffectPermitAuthorities<'a> {
    EffectPermitAuthorities {
        operation: Some(operation),
        channel: Some(channel),
    }
}

fn rotate_channel(authority: &ChannelAuthority, channel: &nlos_channel::ChannelRecord) {
    authority
        .rotate_channel(RotateChannelRequest {
            channel_id: channel.channel_id,
            expected_generation: channel.generation,
            expected_fencing_token: channel.fencing_token,
            idempotency_key: IdempotencyKey::from_bytes([0xe3; 16]),
            rotated_at_ms: 1_250,
        })
        .unwrap();
}

#[test]
fn combined_gates_pass_mint_one_shot_token_when_both_proofs_hold() {
    // Given: a dual-endpoint slot whose Operation dispatch is durably
    // prepared and activated AND whose Channel proof still byte-matches the
    // sealed triple at the CURRENT generation.
    let slot = sealed_dual_slot("combined-happy");
    let operation = nlos_store::SqliteOperationStore::open(&slot.operation_path).unwrap();
    let channel_authority = ChannelAuthority::open(&slot.channel_root_path).unwrap();
    let authority = slot.database.open();
    activate_operation(&operation, &slot.operation);

    // When: the struct-based combined issuance runs with both authorities.
    let permit = issued_effect_permit(
        authority
            .request_effect_permit_with_authorities_struct(
                both_gates(&operation, &channel_authority),
                effect_request(&slot, 0xa1),
            )
            .unwrap(),
    );

    // Then: the one-shot dispatch token is minted and the slot is Permitted.
    assert_ne!(permit.one_shot_dispatch_token, [0; 32]);
    let stored = authority
        .inspect_effect_slot(slot.commit_permit.permit_id, 0)
        .unwrap();
    assert_eq!(stored.state, SlotState::Permitted);
    assert_eq!(stored.effect_permit_id, Some(permit.effect_permit_id));
}

#[test]
fn combined_gate_fails_closed_when_operation_never_activated() {
    // Given: the Channel proof matches but the Operation dispatch was never
    // prepared or activated on the owner.
    let slot = sealed_dual_slot("combined-operation-inactive");
    let operation = nlos_store::SqliteOperationStore::open(&slot.operation_path).unwrap();
    let channel_authority = ChannelAuthority::open(&slot.channel_root_path).unwrap();
    let authority = slot.database.open();

    // When: the combined issuance runs.
    let result = authority.request_effect_permit_with_authorities_struct(
        both_gates(&operation, &channel_authority),
        effect_request(&slot, 0xa2),
    );

    // Then: the typed Operation owner rejection surfaces and no token is
    // minted; the slot is untouched.
    assert!(matches!(
        result,
        Err(nlos_task::TaskStoreError::OperationParticipantAuthority(
            nlos_store::StoreError::DispatchPreparationNotFound
        ))
    ));
    let stored = authority
        .inspect_effect_slot(slot.commit_permit.permit_id, 0)
        .unwrap();
    assert_eq!(stored.state, SlotState::Planned);
    assert_eq!(stored.effect_permit_id, None);
}

#[test]
#[allow(clippy::too_many_lines)]
fn combined_gate_fails_closed_on_channel_rotation_between_seal_and_mint() {
    // Given: the Operation dispatch is activated, but the Channel rotated
    // after the commit permit froze the sealed triple, so the owner's
    // CURRENT endpoint proof no longer byte-matches.
    let slot = sealed_dual_slot("combined-channel-rotation");
    let operation = nlos_store::SqliteOperationStore::open(&slot.operation_path).unwrap();
    let channel_authority = ChannelAuthority::open(&slot.channel_root_path).unwrap();
    rotate_channel(&channel_authority, &slot.channel);
    let authority = slot.database.open();
    activate_operation(&operation, &slot.operation);

    // When: the combined issuance runs.
    let result = authority.request_effect_permit_with_authorities_struct(
        both_gates(&operation, &channel_authority),
        effect_request(&slot, 0xa3),
    );

    // Then: the typed triple-mismatch rejection surfaces with the precise
    // conflict reason and no token is minted; the slot is untouched.
    assert!(matches!(
        result,
        Err(nlos_task::TaskStoreError::TaskWriteSetConflict {
            reason: "Channel endpoint proof differs before effect permit mint"
        })
    ));
    let stored = authority
        .inspect_effect_slot(slot.commit_permit.permit_id, 0)
        .unwrap();
    assert_eq!(stored.state, SlotState::Planned);
    assert_eq!(stored.effect_permit_id, None);

    // And: the failed combined call persisted nothing — the single-gate
    // operation-only issuance with the SAME idempotency key mints a fresh
    // permit (Issued, not Replayed), matching the ladder variant's
    // channel-free semantics for this call shape.
    match authority
        .request_effect_permit_with_authorities_struct(
            EffectPermitAuthorities {
                operation: Some(&operation),
                channel: None,
            },
            effect_request(&slot, 0xa3),
        )
        .unwrap()
    {
        EffectPermitDecision::Issued(_) => {}
        other @ EffectPermitDecision::Replayed(_) => {
            panic!("expected issued effect permit from operation-only retry, got {other:?}")
        }
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn combined_gate_fails_closed_on_operation_rotation_between_seal_and_mint() {
    // Given: the Channel proof still matches and the Operation dispatch was
    // activated at the sealed handle, but the consulted Operation authority
    // now holds the same operation_id at the NEXT generation — the sealed
    // binding rotated past the gate.
    let slot = sealed_dual_slot("combined-operation-rotation");
    let drifted_root = AuthorityRoot::new("combined-operation-rotation-drifted");
    std::fs::create_dir_all(&drifted_root.0).unwrap();
    let drifted =
        nlos_store::SqliteOperationStore::open(drifted_root.0.join("authority.sqlite3")).unwrap();
    let mut next_spec = operation_spec(0xc2);
    next_spec.generation = slot.operation.generation.checked_next().unwrap();
    drifted.register(next_spec).unwrap();
    activate_operation(&drifted, &next_spec);
    let channel_authority = ChannelAuthority::open(&slot.channel_root_path).unwrap();
    let authority = slot.database.open();

    // When: the combined issuance consults the rotated (drifted) authority.
    let result = authority.request_effect_permit_with_authorities_struct(
        both_gates(&drifted, &channel_authority),
        effect_request(&slot, 0xa4),
    );

    // Then: the typed stale-generation rejection surfaces and no token is
    // minted; the slot is untouched.
    assert!(matches!(
        result,
        Err(nlos_task::TaskStoreError::OperationParticipantAuthority(
            nlos_store::StoreError::Operation(nlos_operation::OperationError::InvalidGeneration)
        ))
    ));
    let stored = authority
        .inspect_effect_slot(slot.commit_permit.permit_id, 0)
        .unwrap();
    assert_eq!(stored.state, SlotState::Planned);
    assert_eq!(stored.effect_permit_id, None);
}

#[test]
#[allow(clippy::too_many_lines)]
fn single_gate_slots_match_ladder_variant_behavior() {
    // Given: the dual-endpoint slot with an activated Operation dispatch.
    let slot = sealed_dual_slot("combined-single-gate");
    let operation = nlos_store::SqliteOperationStore::open(&slot.operation_path).unwrap();
    let unrelated_channel_root = AuthorityRoot::new("combined-single-gate-unrelated-channel");
    let unrelated_channel = ChannelAuthority::open(&unrelated_channel_root.0).unwrap();
    let authority = slot.database.open();
    activate_operation(&operation, &slot.operation);

    // When: the struct entry carries ONLY the Operation authority.
    let struct_only_operation = issued_effect_permit(
        authority
            .request_effect_permit_with_authorities_struct(
                EffectPermitAuthorities {
                    operation: Some(&operation),
                    channel: None,
                },
                effect_request(&slot, 0xa5),
            )
            .unwrap(),
    );

    // Then: it mints exactly like the ladder operation variant would for a
    // fresh slot — the absent Channel slot skips only the channel gate.
    assert_ne!(struct_only_operation.one_shot_dispatch_token, [0; 32]);

    // And: given a second dual slot where the Channel still matches but the
    // Operation dispatch was never activated, the struct entry carrying
    // ONLY the Channel authority mints (channel gate alone passes), while
    // the ladder channel variant behaves the same way on a third copy.
    let channel_slot = sealed_dual_slot("combined-single-gate-channel");
    let channel_authority = ChannelAuthority::open(&channel_slot.channel_root_path).unwrap();
    let channel_task = channel_slot.database.open();
    let struct_only_channel = issued_effect_permit(
        channel_task
            .request_effect_permit_with_authorities_struct(
                EffectPermitAuthorities {
                    operation: None,
                    channel: Some(&channel_authority),
                },
                effect_request(&channel_slot, 0xa6),
            )
            .unwrap(),
    );
    assert_ne!(struct_only_channel.one_shot_dispatch_token, [0; 32]);

    let ladder_slot = sealed_dual_slot("combined-single-gate-ladder");
    let ladder_channel_authority = ChannelAuthority::open(&ladder_slot.channel_root_path).unwrap();
    let ladder_task = ladder_slot.database.open();
    let ladder_only_channel = issued_effect_permit(
        ladder_task
            .request_effect_permit_with_channel_authority(
                &ladder_channel_authority,
                effect_request(&ladder_slot, 0xa7),
            )
            .unwrap(),
    );
    assert_ne!(ladder_only_channel.one_shot_dispatch_token, [0; 32]);
    drop(unrelated_channel);

    // And: the default (all-None) bundle reproduces the legacy
    // authority-free issuance even when nothing was activated.
    let legacy_slot = sealed_dual_slot("combined-single-gate-legacy");
    let legacy_task = legacy_slot.database.open();
    let legacy = issued_effect_permit(
        legacy_task
            .request_effect_permit_with_authorities_struct(
                EffectPermitAuthorities::default(),
                effect_request(&legacy_slot, 0xa8),
            )
            .unwrap(),
    );
    assert_ne!(legacy.one_shot_dispatch_token, [0; 32]);
}

#[test]
#[allow(clippy::too_many_lines)]
fn combined_gate_replay_trusts_only_task_rows() {
    // Given: the combined issuance already minted a permit.
    let slot = sealed_dual_slot("combined-replay");
    let minted = {
        let operation = nlos_store::SqliteOperationStore::open(&slot.operation_path).unwrap();
        let channel_authority = ChannelAuthority::open(&slot.channel_root_path).unwrap();
        let authority = slot.database.open();
        activate_operation(&operation, &slot.operation);
        issued_effect_permit(
            authority
                .request_effect_permit_with_authorities_struct(
                    both_gates(&operation, &channel_authority),
                    effect_request(&slot, 0xa9),
                )
                .unwrap(),
        )
    };

    // When: the Task authority is reopened and the same request is replayed
    // after BOTH owners drifted past the sealed state — the Channel rotated
    // and the Operation authority is an empty unrelated store — so any
    // owner readback would fail; a returned token proves the replay path
    // trusts only the Task rows.
    let authority = slot.database.open();
    let real_channel = ChannelAuthority::open(&slot.channel_root_path).unwrap();
    rotate_channel(&real_channel, &slot.channel);
    drop(real_channel);
    let unrelated_root = AuthorityRoot::new("combined-replay-unrelated");
    std::fs::create_dir_all(&unrelated_root.0).unwrap();
    let unrelated_operation =
        nlos_store::SqliteOperationStore::open(unrelated_root.0.join("authority.sqlite3")).unwrap();
    let unrelated_channel_root = AuthorityRoot::new("combined-replay-unrelated-channel");
    let unrelated_channel = ChannelAuthority::open(&unrelated_channel_root.0).unwrap();
    let first = replayed_effect_permit(
        authority
            .request_effect_permit_with_authorities_struct(
                both_gates(&unrelated_operation, &unrelated_channel),
                effect_request(&slot, 0xa9),
            )
            .unwrap(),
    );

    // Then: the durable token returns unchanged without any owner readback.
    assert_eq!(first.effect_permit_id, minted.effect_permit_id);
    assert_eq!(
        first.one_shot_dispatch_token,
        minted.one_shot_dispatch_token
    );
    assert_eq!(
        authority
            .inspect_effect_permit(task_id(), minted.effect_permit_id)
            .unwrap(),
        minted
    );
    assert_eq!(
        authority
            .inspect_effect_slot(slot.commit_permit.permit_id, 0)
            .unwrap()
            .state,
        SlotState::Permitted
    );
}
