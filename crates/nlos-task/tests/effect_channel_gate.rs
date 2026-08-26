#![allow(deprecated)] // Ladder constructors deprecated in favor of the *_with_authorities_struct entries.
//! `B-CHANNEL-001` `EffectPermit` channel gate: the channel-gated issuance
//! variant must re-read the owning Channel authority's CURRENT-generation
//! endpoint proof and byte-compare the sealed triple before minting a
//! one-shot token for a slot whose sealed `TaskWriteSet` endpoint is a
//! `ChannelTopicBinding`, while the legacy authority-free issuance path
//! stays enforcement-free by design.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_channel::{ChannelAuthority, ChannelDecision, CreateChannelRequest, RotateChannelRequest};
use nlos_task::{
    AttemptSpec, EffectPermitDecision, EffectPermitRequest, IssuedPermit, LogicalEffectDescriptor,
    ParticipantRegistryBinding, PermitDecision, PermitRecord, PermitRequest, PlannedEffect,
    SlotState, SnapshotBundle, SnapshotConsistency, SqliteTaskAuthority, TaskSnapshotReceiptSpec,
    TaskSpec, TaskWriteSetArtifactRead, TaskWriteSetEffectEndpointRequest, TaskWriteSetRequest,
    empty_effect_history_root,
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
            "nlos-task-effect-channel-gate-{}-{}.sqlite3",
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
            "nlos-task-effect-channel-gate-{label}-{}-{}",
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

/// Full `ChannelTopicBinding` Given-fixture: task + attempt + snapshot
/// receipt, registered Channel participant, sealed write set with a Channel
/// endpoint on effect slot 0, and the channel-revalidated commit permit.
/// `_artifact_root` / `_channel_root` are carried for their RAII lifetimes:
/// the owner directories must outlive the stores each test reopens from
/// them.
struct SealedChannelSlot {
    database: Database,
    _artifact_root: AuthorityRoot,
    channel_root_path: PathBuf,
    _channel_root: AuthorityRoot,
    channel: nlos_channel::ChannelRecord,
    spec: AttemptSpec,
    commit_permit: PermitRecord,
}

fn sealed_channel_slot(label: &str) -> SealedChannelSlot {
    let database = Database::new();
    let artifact_root = AuthorityRoot::new(label);
    let channel_root = AuthorityRoot::new(label);
    let artifact = nlos_artifact::ArtifactStore::open(&artifact_root.0).unwrap();
    let artifact_id = create_artifact(&artifact, 0xc1);
    let channel_authority = ChannelAuthority::open(&channel_root.0).unwrap();
    let channel = create_channel(&channel_authority);

    let authority = database.open();
    let spec = registered_attempt(&authority, 0xc3);
    let registry_binding = binding(&authority.inspect_participant_registry(task_id()).unwrap());
    authority
        .register_channel_participant(
            &channel_authority,
            task_id(),
            registry_binding,
            channel.channel_id,
            channel.generation,
            1_150,
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
        effect_endpoints: vec![TaskWriteSetEffectEndpointRequest::ChannelTopicBinding {
            effect_seq: 0,
            channel_id: channel.channel_id,
            expected_channel_generation: channel.generation,
        }],
        idempotency_key: IdempotencyKey::from_bytes([0xce; 16]),
        sealed_at_ms: 1_200,
    };
    let record = authority
        .seal_task_write_set_with_channel_authority(&artifact, &channel_authority, request)
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
            .request_commit_permit_with_channel_authority(&channel_authority, commit_request)
            .unwrap(),
    );
    SealedChannelSlot {
        database,
        _artifact_root: artifact_root,
        channel_root_path: channel_root.0.clone(),
        _channel_root: channel_root,
        channel,
        spec,
        commit_permit,
    }
}

fn effect_request(slot: &SealedChannelSlot, key_seed: u8) -> EffectPermitRequest {
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
fn channel_gated_effect_permit_mints_one_shot_token_when_proof_matches() {
    // Given: a sealed ChannelTopicBinding effect slot whose owner proof still
    // byte-matches the sealed triple at the CURRENT generation.
    let slot = sealed_channel_slot("channel-gate-happy");
    let channel_authority = ChannelAuthority::open(&slot.channel_root_path).unwrap();
    let authority = slot.database.open();

    // When: the channel-gated issuance runs.
    let permit = issued_effect_permit(
        authority
            .request_effect_permit_with_channel_authority(
                &channel_authority,
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
#[allow(clippy::too_many_lines)]
fn channel_rotation_between_seal_and_effect_permit_fails_closed_without_partial_state() {
    // Given: the Channel rotated after the commit permit froze the sealed
    // triple, so the owner's CURRENT endpoint proof no longer byte-matches.
    let slot = sealed_channel_slot("channel-gate-rotation");
    let channel_authority = ChannelAuthority::open(&slot.channel_root_path).unwrap();
    rotate_channel(&channel_authority, &slot.channel);
    let authority = slot.database.open();

    // When: the channel-gated issuance runs.
    let result = authority.request_effect_permit_with_channel_authority(
        &channel_authority,
        effect_request(&slot, 0xa2),
    );

    // Then: the typed triple-mismatch rejection surfaces and no token is
    // minted; the slot is untouched.
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

    // And: the failed gated call persisted nothing — the authority-free
    // legacy issuance with the SAME idempotency key mints a fresh permit
    // (Issued, not Replayed), per its own gate-free semantics.
    match authority
        .request_effect_permit(effect_request(&slot, 0xa2))
        .unwrap()
    {
        EffectPermitDecision::Issued(_) => {}
        other @ EffectPermitDecision::Replayed(_) => {
            panic!("expected issued effect permit from legacy retry, got {other:?}")
        }
    }
}

#[test]
fn channel_gate_fails_closed_on_owner_readback_error() {
    // Given: the gated issuance is pointed at a Channel authority over an
    // empty directory — the sealed Channel never existed there.
    let slot = sealed_channel_slot("channel-gate-missing");
    let unrelated_root = AuthorityRoot::new("channel-gate-missing-unrelated");
    let unrelated = ChannelAuthority::open(&unrelated_root.0).unwrap();
    let authority = slot.database.open();

    // When: the channel-gated issuance runs.
    let result = authority
        .request_effect_permit_with_channel_authority(&unrelated, effect_request(&slot, 0xa3));

    // Then: the typed Channel owner readback error surfaces and no token is
    // minted.
    assert!(matches!(
        result,
        Err(nlos_task::TaskStoreError::ChannelParticipantAuthority(
            nlos_channel::ChannelAuthorityError::ChannelNotFound(_)
        ))
    ));
    let stored = authority
        .inspect_effect_slot(slot.commit_permit.permit_id, 0)
        .unwrap();
    assert_eq!(stored.state, SlotState::Planned);
    assert_eq!(stored.effect_permit_id, None);
}

#[test]
fn channel_gate_replay_returns_durable_token_without_owner_readback() {
    // Given: the channel-gated issuance already minted a permit.
    let slot = sealed_channel_slot("channel-gate-replay");
    let minted = {
        let channel_authority = ChannelAuthority::open(&slot.channel_root_path).unwrap();
        let authority = slot.database.open();
        issued_effect_permit(
            authority
                .request_effect_permit_with_channel_authority(
                    &channel_authority,
                    effect_request(&slot, 0xa4),
                )
                .unwrap(),
        )
    };

    // When: the Task authority is reopened and the same request is replayed
    // against a fresh, unrelated, EMPTY Channel authority — any owner
    // readback would fail, so a returned token proves the replay path never
    // reads the owner.
    let authority = slot.database.open();
    let unrelated_root = AuthorityRoot::new("channel-gate-replay-unrelated");
    let unrelated = ChannelAuthority::open(&unrelated_root.0).unwrap();
    let first = replayed_effect_permit(
        authority
            .request_effect_permit_with_channel_authority(&unrelated, effect_request(&slot, 0xa4))
            .unwrap(),
    );

    // Then: the durable token returns unchanged without any owner readback.
    assert_eq!(first.effect_permit_id, minted.effect_permit_id);
    assert_eq!(
        first.one_shot_dispatch_token,
        minted.one_shot_dispatch_token
    );
    let second = replayed_effect_permit(
        authority
            .request_effect_permit_with_channel_authority(&unrelated, effect_request(&slot, 0xa4))
            .unwrap(),
    );
    assert_eq!(second.effect_permit_id, minted.effect_permit_id);
    assert_eq!(
        second.one_shot_dispatch_token,
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

#[test]
fn legacy_effect_permit_issuance_skips_channel_gate_for_channel_slot() {
    // Given: the sealed Channel endpoint's owner proof has DRIFTED (the
    // channel rotated) — the gate would reject — but the call carries no
    // authority at all.
    let slot = sealed_channel_slot("channel-gate-legacy");
    let channel_authority = ChannelAuthority::open(&slot.channel_root_path).unwrap();
    rotate_channel(&channel_authority, &slot.channel);
    drop(channel_authority);
    let authority = slot.database.open();

    // When: the authority-free legacy issuance runs.
    let permit = issued_effect_permit(
        authority
            .request_effect_permit(effect_request(&slot, 0xa5))
            .unwrap(),
    );

    // Then: it still mints without any owner interaction — the enforcement
    // boundary lives exclusively in the new authority-carrying variant.
    assert_ne!(permit.one_shot_dispatch_token, [0; 32]);
    assert_eq!(
        authority
            .inspect_effect_slot(slot.commit_permit.permit_id, 0)
            .unwrap()
            .state,
        SlotState::Permitted
    );
}

#[test]
fn channel_gate_passes_through_operation_binding_slot() {
    // Given: a sealed OperationBinding effect slot (merely Registered on its
    // Operation owner — no dispatch preparation exists) and a fresh, EMPTY
    // Channel authority that would fail any endpoint readback.
    let database = Database::new();
    let artifact_root = AuthorityRoot::new("channel-gate-passthrough");
    let operation_root = AuthorityRoot::new("channel-gate-passthrough");
    std::fs::create_dir_all(&operation_root.0).unwrap();
    let operation_path = operation_root.0.join("authority.sqlite3");
    let artifact = nlos_artifact::ArtifactStore::open(&artifact_root.0).unwrap();
    let artifact_id = create_artifact(&artifact, 0xe1);
    let operation_store = nlos_store::SqliteOperationStore::open(&operation_path).unwrap();
    let operation = nlos_operation::OperationSpec {
        operation_id: nlos_types::OperationId::from_bytes([0xe2; 16]),
        generation: Generation::INITIAL,
        owner_fiber: nlos_runtime::FiberHandle {
            fiber_id: nlos_types::ExecutionFiberId::from_bytes([0xe3; 16]),
            generation: Generation::INITIAL,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes([0xe4; 16]),
        cancellation_generation: Generation::INITIAL,
    };
    operation_store.register(operation).unwrap();

    let authority = database.open();
    let spec = registered_attempt(&authority, 0xe5);
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
        effect_endpoints: vec![TaskWriteSetEffectEndpointRequest::OperationBinding {
            effect_seq: 0,
            operation_id: operation.operation_id,
            expected_operation_generation: operation.generation,
        }],
        idempotency_key: IdempotencyKey::from_bytes([0xe6; 16]),
        sealed_at_ms: 1_200,
    };
    let record = authority
        .seal_task_write_set_with_operation_authority(&artifact, &operation_store, request)
        .unwrap()
        .record()
        .clone();
    let mut commit_request = commit_permit_request(&spec, 0xe7);
    commit_request.write_set_root = record.write_set_root;
    commit_request.planned_effects = record.planned_effects.clone();
    let commit_permit = issued_commit(
        authority
            .request_commit_permit_with_operation_authority(&operation_store, commit_request)
            .unwrap(),
    );

    // When: the channel-gated issuance runs against the EMPTY Channel
    // authority.
    let unrelated_root = AuthorityRoot::new("channel-gate-passthrough-unrelated");
    let unrelated = ChannelAuthority::open(&unrelated_root.0).unwrap();
    let permit = issued_effect_permit(
        authority
            .request_effect_permit_with_channel_authority(
                &unrelated,
                EffectPermitRequest {
                    task_id: task_id(),
                    attempt_id: spec.attempt_id,
                    attempt_generation: spec.attempt_generation,
                    permit_id: commit_permit.permit_id,
                    permit_epoch: commit_permit.permit_epoch,
                    effect_seq: 0,
                    idempotency_key: IdempotencyKey::from_bytes([0xa6; 16]),
                    valid_until_ms: 9_000,
                    requested_at_ms: 4_000,
                },
            )
            .unwrap(),
    );

    // Then: the gate passes through for the non-Channel endpoint kind —
    // single-slot requests need only their own kind's authority — and the
    // permit mints without consulting the Channel authority at all.
    assert_ne!(permit.one_shot_dispatch_token, [0; 32]);
    assert_eq!(
        authority
            .inspect_effect_slot(commit_permit.permit_id, 0)
            .unwrap()
            .state,
        SlotState::Permitted
    );
}
