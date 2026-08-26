#![allow(deprecated)] // Ladder constructors deprecated in favor of the *_with_authorities_struct entries.
//! `[B-OP-FENCE-003]` `TaskWriteSet` consumption wiring: the activation-gated
//! `EffectPermit` issuance variant must re-read the owning Operation
//! authority's dispatch activation proof before minting a one-shot token for
//! a slot whose sealed endpoint is an `OperationBinding`, while the legacy
//! authority-free issuance path stays enforcement-free by design.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_runtime::FiberHandle;
use nlos_task::{
    AttemptSpec, EffectPermitDecision, EffectPermitRequest, IssuedPermit, LogicalEffectDescriptor,
    ParticipantRegistryBinding, PermitDecision, PermitRecord, PermitRequest, PlannedEffect,
    SlotState, SnapshotBundle, SnapshotConsistency, SqliteTaskAuthority, TaskSnapshotReceiptSpec,
    TaskSpec, TaskWriteSetArtifactRead, TaskWriteSetEffectEndpointRequest, TaskWriteSetRequest,
    empty_effect_history_root,
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
            "nlos-task-effect-gate-{}-{}.sqlite3",
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
            "nlos-task-effect-gate-{label}-{}-{}",
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
    TaskId::from_bytes([0x11; 16])
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
            intent_spec_id: [0x91; 32],
            stable_action_slot: 0,
            target_authority_object_id: [0x92; 32],
            effect_class: 1,
            idempotency_scope: 1,
        },
        required: false,
        required_condition_digest: None,
        success_criteria_digest: [0x93; 32],
        action_proposal_digest: [0x94; 32],
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
            created_at_ms: 1_500,
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

fn prepare_operation(
    store: &nlos_store::SqliteOperationStore,
    spec: &nlos_operation::OperationSpec,
) {
    store
        .prepare_dispatch(operation_handle(spec), CallbackId::from_bytes([0xd1; 16]))
        .unwrap();
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

/// Full `OperationBinding` Given-fixture: task + attempt + snapshot receipt,
/// registered Operation participant, sealed write set with an Operation
/// endpoint on effect slot 0, and the v24 activation-free commit permit.
/// `_operation_root` is carried for its RAII lifetime only: the owner
/// directory must outlive the stores each test reopens from it.
struct SealedOperationSlot {
    database: Database,
    _operation_root: AuthorityRoot,
    operation_path: PathBuf,
    operation: nlos_operation::OperationSpec,
    spec: AttemptSpec,
    commit_permit: PermitRecord,
}

fn sealed_operation_slot(label: &str) -> SealedOperationSlot {
    let database = Database::new();
    let artifact_root = AuthorityRoot::new(label);
    let operation_root = AuthorityRoot::new(label);
    std::fs::create_dir_all(&operation_root.0).unwrap();
    let operation_path = operation_root.0.join("authority.sqlite3");
    let artifact = nlos_artifact::ArtifactStore::open(&artifact_root.0).unwrap();
    let artifact_id = create_artifact(&artifact, 0xc1);
    let operation_store = nlos_store::SqliteOperationStore::open(&operation_path).unwrap();
    let operation = operation_spec(0xc2);
    operation_store.register(operation).unwrap();

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
        idempotency_key: IdempotencyKey::from_bytes([0xce; 16]),
        sealed_at_ms: 1_200,
    };
    let record = authority
        .seal_task_write_set_with_operation_authority(&artifact, &operation_store, request)
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
            .request_commit_permit_with_operation_authority(&operation_store, commit_request)
            .unwrap(),
    );
    SealedOperationSlot {
        database,
        _operation_root: operation_root,
        operation_path,
        operation,
        spec,
        commit_permit,
    }
}

fn effect_request(slot: &SealedOperationSlot, key_seed: u8) -> EffectPermitRequest {
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

#[test]
#[allow(clippy::too_many_lines)]
fn activation_gated_effect_permit_mints_one_shot_token_after_owner_activation() {
    // Given: a sealed OperationBinding effect slot whose owner has durably
    // prepared and activated the dispatch (ADR-0005 authority-first order).
    let slot = sealed_operation_slot("gate-happy");
    let operation = nlos_store::SqliteOperationStore::open(&slot.operation_path).unwrap();
    let authority = slot.database.open();
    activate_operation(&operation, &slot.operation);

    // When: the activation-gated issuance runs.
    let permit = issued_effect_permit(
        authority
            .request_effect_permit_with_operation_authority(&operation, effect_request(&slot, 0xa1))
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
fn activation_gate_fails_closed_on_registered_only_operation_and_recovers_after_activation() {
    // Given: the sealed Operation endpoint is still merely Registered on the
    // owning authority (no dispatch preparation exists).
    let slot = sealed_operation_slot("gate-registered");
    let operation = nlos_store::SqliteOperationStore::open(&slot.operation_path).unwrap();
    let authority = slot.database.open();

    // When: the activation-gated issuance runs.
    let result = authority
        .request_effect_permit_with_operation_authority(&operation, effect_request(&slot, 0xa2));

    // Then: the typed owner rejection surfaces and no token is minted.
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

    // And: after owner activation the identical retry mints a fresh permit
    // (Issued, not Replayed — proving the failed call persisted nothing).
    activate_operation(&operation, &slot.operation);
    match authority
        .request_effect_permit_with_operation_authority(&operation, effect_request(&slot, 0xa2))
        .unwrap()
    {
        EffectPermitDecision::Issued(_) => {}
        other @ EffectPermitDecision::Replayed(_) => {
            panic!("expected issued effect permit after activation, got {other:?}")
        }
    }
}

#[test]
fn activation_gate_fails_closed_on_prepared_only_operation() {
    // Given: the owner durably prepared the dispatch but never activated it.
    let slot = sealed_operation_slot("gate-prepared");
    let operation = nlos_store::SqliteOperationStore::open(&slot.operation_path).unwrap();
    let authority = slot.database.open();
    prepare_operation(&operation, &slot.operation);

    // When: the activation-gated issuance runs.
    let result = authority
        .request_effect_permit_with_operation_authority(&operation, effect_request(&slot, 0xa3));

    // Then: the typed unactivated rejection surfaces and no token is minted.
    assert!(matches!(
        result,
        Err(nlos_task::TaskStoreError::OperationParticipantAuthority(
            nlos_store::StoreError::OperationNotActivated
        ))
    ));
    let stored = authority
        .inspect_effect_slot(slot.commit_permit.permit_id, 0)
        .unwrap();
    assert_eq!(stored.state, SlotState::Planned);
    assert_eq!(stored.effect_permit_id, None);
}

#[test]
fn activation_gate_fails_closed_on_stale_sealed_generation() {
    // Given: the sealed endpoint pinned generation INITIAL, but the Operation
    // authority consulted at gate time holds the same operation_id at the
    // next generation (sealed binding is stale).
    let slot = sealed_operation_slot("gate-stale");
    let drifted_root = AuthorityRoot::new("gate-stale-drifted");
    std::fs::create_dir_all(&drifted_root.0).unwrap();
    let drifted =
        nlos_store::SqliteOperationStore::open(drifted_root.0.join("authority.sqlite3")).unwrap();
    let mut next_spec = operation_spec(0xc2);
    next_spec.generation = slot.operation.generation.checked_next().unwrap();
    drifted.register(next_spec).unwrap();
    let authority = slot.database.open();

    // When: the activation-gated issuance consults the drifted authority.
    let result = authority
        .request_effect_permit_with_operation_authority(&drifted, effect_request(&slot, 0xa4));

    // Then: the stale-generation rejection surfaces and no token is minted.
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
fn activation_gate_replay_returns_durable_token_without_owner_readback() {
    // Given: the activation-gated issuance already minted a permit.
    let slot = sealed_operation_slot("gate-replay");
    let minted = {
        let operation = nlos_store::SqliteOperationStore::open(&slot.operation_path).unwrap();
        let authority = slot.database.open();
        activate_operation(&operation, &slot.operation);
        issued_effect_permit(
            authority
                .request_effect_permit_with_operation_authority(
                    &operation,
                    effect_request(&slot, 0xa5),
                )
                .unwrap(),
        )
    };

    // When: both authorities are reopened and the same request is replayed
    // against a fresh, unrelated, EMPTY Operation authority.
    let authority = slot.database.open();
    let unrelated_root = AuthorityRoot::new("gate-replay-unrelated");
    std::fs::create_dir_all(&unrelated_root.0).unwrap();
    let unrelated =
        nlos_store::SqliteOperationStore::open(unrelated_root.0.join("authority.sqlite3")).unwrap();
    let first = replayed_effect_permit(
        authority
            .request_effect_permit_with_operation_authority(&unrelated, effect_request(&slot, 0xa5))
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
            .request_effect_permit_with_operation_authority(&unrelated, effect_request(&slot, 0xa5))
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
fn legacy_effect_permit_issuance_skips_activation_gate_for_operation_slot() {
    // Given: the sealed Operation endpoint is merely Registered — the owner
    // has neither prepared nor activated anything.
    let slot = sealed_operation_slot("gate-legacy");
    let authority = slot.database.open();

    // When: the authority-free legacy issuance runs.
    let permit = issued_effect_permit(
        authority
            .request_effect_permit(effect_request(&slot, 0xa6))
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
#[allow(clippy::too_many_lines)]
fn activation_gate_passes_through_non_operation_effect_endpoint() {
    // Given: a sealed ArtifactHead effect endpoint (non-OperationBinding) and
    // a fresh, EMPTY Operation authority that would fail any readback.
    let database = Database::new();
    let artifact_root = AuthorityRoot::new("gate-artifact");
    let artifact = nlos_artifact::ArtifactStore::open(&artifact_root.0).unwrap();
    let artifact_id = create_artifact(&artifact, 0xe1);
    let empty_operation_root = AuthorityRoot::new("gate-artifact-operation");
    std::fs::create_dir_all(&empty_operation_root.0).unwrap();
    let empty_operation =
        nlos_store::SqliteOperationStore::open(empty_operation_root.0.join("authority.sqlite3"))
            .unwrap();
    let authority = database.open();
    let spec = registered_attempt(&authority, 0xe2);
    let registry_binding = binding(&authority.inspect_participant_registry(task_id()).unwrap());
    authority
        .register_artifact_head_participant(
            &artifact,
            task_id(),
            registry_binding,
            artifact_id,
            1_150,
        )
        .unwrap();
    let record = authority
        .seal_task_write_set(
            &artifact,
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
                planned_effects: vec![planned_effect()],
                effect_endpoints: vec![TaskWriteSetEffectEndpointRequest::ArtifactHead {
                    effect_seq: 0,
                    artifact_id,
                }],
                idempotency_key: IdempotencyKey::from_bytes([0xe3; 16]),
                sealed_at_ms: 1_200,
            },
        )
        .unwrap()
        .record()
        .clone();
    let mut commit_request = commit_permit_request(&spec, 0xe4);
    commit_request.write_set_root = record.write_set_root;
    commit_request.planned_effects = record.planned_effects.clone();
    let commit_permit = issued_commit(authority.request_commit_permit(commit_request).unwrap());

    // When: the activation-gated issuance runs with the empty authority.
    let permit = issued_effect_permit(
        authority
            .request_effect_permit_with_operation_authority(
                &empty_operation,
                EffectPermitRequest {
                    task_id: task_id(),
                    attempt_id: spec.attempt_id,
                    attempt_generation: spec.attempt_generation,
                    permit_id: commit_permit.permit_id,
                    permit_epoch: commit_permit.permit_epoch,
                    effect_seq: 0,
                    idempotency_key: IdempotencyKey::from_bytes([0xa7; 16]),
                    valid_until_ms: 9_000,
                    requested_at_ms: 4_000,
                },
            )
            .unwrap(),
    );

    // Then: the gate passes through for the non-Operation endpoint kind and
    // the permit mints without consulting the Operation authority at all.
    assert_ne!(permit.one_shot_dispatch_token, [0; 32]);
    assert_eq!(
        authority
            .inspect_effect_slot(commit_permit.permit_id, 0)
            .unwrap()
            .state,
        SlotState::Permitted
    );
}
