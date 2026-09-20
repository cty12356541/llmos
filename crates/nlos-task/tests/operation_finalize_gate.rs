//! W29-C (ADR-0017 决定 2 / 附录 B, O-B): the Operation verify-half wiring
//! on the Task finalize gate. `finalize_commit_v3_with_spec` with an
//! `operation_authority` re-reads the owner's durable dispatch activation
//! receipt for every sealed `OperationBinding` endpoint before the terminal
//! Task transaction opens (`[TASK-COMMIT-002]` slot evidence-chain owner
//! revalidation, mirroring the `B-TASK-008C2G-RES/ART/PROCESS`
//! owner-revalidation family). Prepared-but-never-activated, canceled, and
//! stale-generation preparations fail closed with typed errors naming the
//! Operation; a closed permit replays from the durable Task rows only, and
//! the owner-side prepare/activate restart exact replay (`f6530fc`) is
//! preserved across every gate.
//!
//! The final test is the ADR-0017 negative gate: W29-C must NOT introduce
//! any Task-side Operation plan state machine, recovery-ledger table group,
//! or worker cycle — asserted here at the schema level.

use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_runtime::FiberHandle;
use nlos_task::{
    AttemptSpec, Authorities, EffectPermitDecision, EffectPermitRequest, FinalizeDecision,
    FinalizeRequest, FinalizeRequestV3, FinalizeSpec, FinalizeSpecDecision, IssuedPermit,
    LogicalEffectDescriptor, Outcome, OutcomeRequest, PermitDecision, PermitRecord, PermitRequest,
    PlannedEffect, SlotState, SnapshotBundle, SnapshotConsistency, SqliteTaskAuthority,
    TaskSnapshotReceiptSpec, TaskSpec, TaskWriteSetArtifactRead, TaskWriteSetEffectEndpointRequest,
    TaskWriteSetRequest, empty_effect_history_root,
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
            "nlos-task-operation-finalize-{}-{}.sqlite3",
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
            "nlos-task-operation-finalize-{label}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
    }
}

impl Drop for AuthorityRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
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
) -> nlos_store::OperationDispatchPreparation {
    match store
        .prepare_dispatch(operation_handle(spec), CallbackId::from_bytes([0xd1; 16]))
        .unwrap()
    {
        nlos_store::OperationPrepareDecision::Prepared(preparation)
        | nlos_store::OperationPrepareDecision::Replayed(preparation) => preparation,
    }
}

fn activate_operation(
    store: &nlos_store::SqliteOperationStore,
    spec: &nlos_operation::OperationSpec,
) {
    let preparation = prepare_operation(store, spec);
    store.activate_dispatch(preparation).unwrap();
}

fn registered_attempt(authority: &SqliteTaskAuthority, seed: u8) -> AttemptSpec {
    authority
        .register_task(TaskSpec {
            application_id: None,
            plan_revision: None,
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
/// endpoint on effect slot 0, and the v24 commit permit (registration-proof
/// revalidation only — activation happens after permit freeze,
/// ADR-0005 authority-first). `_operation_root` is carried for its RAII
/// lifetime only.
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
        .seal_task_write_set_with_authorities_struct(
            Authorities {
                artifact: Some(&artifact),
                operation: Some(&operation_store),
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
            .request_commit_permit_decision_with_authorities_struct(
                Authorities {
                    operation: Some(&operation_store),
                    ..Authorities::default()
                },
                commit_request,
            )
            .unwrap()
            .permit,
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

/// Drives the sealed Operation slot to a finalizable `EffectClosed` state
/// through the LEGACY authority-free effect-permit path — the exact hole the
/// finalize gate must close (`[B-OP-FENCE-003]` mint gate is opt-in, so a
/// slot can claim a dispatched effect while the owner never activated).
fn close_slot_with_effect_via_legacy_mint(
    authority: &SqliteTaskAuthority,
    slot: &SealedOperationSlot,
    key_seed: u8,
) {
    let permit = issued_effect_permit(
        authority
            .request_effect_permit(effect_request(slot, key_seed))
            .unwrap(),
    );
    authority
        .consume_dispatch_token(nlos_task::DispatchRequest {
            task_id: task_id(),
            attempt_id: slot.spec.attempt_id,
            attempt_generation: slot.spec.attempt_generation,
            permit_id: slot.commit_permit.permit_id,
            permit_epoch: slot.commit_permit.permit_epoch,
            effect_permit_id: permit.effect_permit_id,
            dispatch_token: permit.one_shot_dispatch_token,
            dispatched_at_ms: 5_000,
        })
        .unwrap();
    authority
        .record_effect_outcome(OutcomeRequest {
            task_id: task_id(),
            attempt_id: slot.spec.attempt_id,
            attempt_generation: slot.spec.attempt_generation,
            permit_id: slot.commit_permit.permit_id,
            permit_epoch: slot.commit_permit.permit_epoch,
            effect_seq: 0,
            outcome: Outcome::Closed {
                authoritative_closure_digest: [0xa9; 32],
            },
            recorded_at_ms: 6_000,
        })
        .unwrap();
}

fn finalize_request(slot: &SealedOperationSlot, finalized_at_ms: i64) -> FinalizeRequestV3 {
    FinalizeRequestV3 {
        base: FinalizeRequest {
            task_id: task_id(),
            attempt_id: slot.spec.attempt_id,
            attempt_generation: slot.spec.attempt_generation,
            permit_id: slot.commit_permit.permit_id,
            new_effect_history_root: empty_effect_history_root(),
            new_retry_fence_epoch: 0,
            finalized_at_ms,
        },
        required_satisfaction: Vec::new(),
        fenced_participant_digest: [0; 32],
    }
}

fn operation_finalize_spec(operation: &nlos_store::SqliteOperationStore) -> FinalizeSpec<'_> {
    FinalizeSpec {
        operation_authority: Some(operation),
        ..FinalizeSpec::default()
    }
}

fn committed_receipt(decision: FinalizeSpecDecision) -> nlos_task::TaskReceiptRecord {
    let FinalizeSpecDecision::Plain(FinalizeDecision::Committed(receipt)) = decision else {
        panic!("expected plain committed finalize, got {decision:?}")
    };
    *receipt
}

fn assert_permit_still_issued(slot: &SealedOperationSlot, authority: &SqliteTaskAuthority) {
    let permit = authority
        .inspect_permit(task_id(), slot.commit_permit.permit_id)
        .unwrap();
    assert_eq!(permit.state, nlos_task::PermitState::Issued);
    assert_eq!(
        authority
            .inspect_effect_slot(slot.commit_permit.permit_id, 0)
            .unwrap()
            .state,
        SlotState::EffectClosed
    );
}

#[test]
fn operation_aware_finalize_commits_after_verified_activation() {
    // Given: the owner durably prepared AND activated the dispatch, the
    // activation-gated effect permit minted and the slot closed with an
    // effect through the gated path.
    let slot = sealed_operation_slot("finalize-happy");
    let operation = nlos_store::SqliteOperationStore::open(&slot.operation_path).unwrap();
    let authority = slot.database.open();
    activate_operation(&operation, &slot.operation);
    let permit = issued_effect_permit(
        authority
            .request_effect_permit_with_operation_authority(&operation, effect_request(&slot, 0xb1))
            .unwrap(),
    );
    authority
        .consume_dispatch_token(nlos_task::DispatchRequest {
            task_id: task_id(),
            attempt_id: slot.spec.attempt_id,
            attempt_generation: slot.spec.attempt_generation,
            permit_id: slot.commit_permit.permit_id,
            permit_epoch: slot.commit_permit.permit_epoch,
            effect_permit_id: permit.effect_permit_id,
            dispatch_token: permit.one_shot_dispatch_token,
            dispatched_at_ms: 5_000,
        })
        .unwrap();
    authority
        .record_effect_outcome(OutcomeRequest {
            task_id: task_id(),
            attempt_id: slot.spec.attempt_id,
            attempt_generation: slot.spec.attempt_generation,
            permit_id: slot.commit_permit.permit_id,
            permit_epoch: slot.commit_permit.permit_epoch,
            effect_seq: 0,
            outcome: Outcome::Closed {
                authoritative_closure_digest: [0xa9; 32],
            },
            recorded_at_ms: 6_000,
        })
        .unwrap();

    // When: the operation-aware finalize re-reads the activation receipt.
    let receipt = committed_receipt(
        authority
            .finalize_commit_v3_with_spec(
                finalize_request(&slot, 7_000),
                operation_finalize_spec(&operation),
            )
            .unwrap(),
    );

    // Then: the terminal commit lands with the permit closed and head
    // advanced — the owner activation evidence chain is complete.
    assert_ne!(receipt.receipt_id, ReceiptId::from_bytes([0; 16]));
    let permit = authority
        .inspect_permit(task_id(), slot.commit_permit.permit_id)
        .unwrap();
    assert_eq!(permit.state, nlos_task::PermitState::Closed);
}

#[test]
fn finalize_gate_fails_closed_when_owner_never_prepared() {
    // Given: the sealed Operation is still merely Registered (no durable
    // preparation), while the slot claims a closed effect via the legacy
    // authority-free mint.
    let slot = sealed_operation_slot("finalize-unprepared");
    let operation = nlos_store::SqliteOperationStore::open(&slot.operation_path).unwrap();
    let authority = slot.database.open();
    close_slot_with_effect_via_legacy_mint(&authority, &slot, 0xb2);

    // When: the operation-aware finalize runs.
    let result = authority.finalize_commit_v3_with_spec(
        finalize_request(&slot, 7_000),
        operation_finalize_spec(&operation),
    );

    // Then: the typed rejection names the Operation and nothing terminal
    // mutated (permit stays Issued with the slot's own facts intact).
    assert!(matches!(
        result,
        Err(nlos_task::TaskStoreError::OperationDispatchNotPrepared {
            operation_id,
            generation: 1,
        }) if operation_id == slot.operation.operation_id
    ));
    assert_permit_still_issued(&slot, &authority);

    // And: after the owner durably prepares AND activates, the identical
    // finalize commits — the failed gate persisted nothing.
    activate_operation(&operation, &slot.operation);
    committed_receipt(
        authority
            .finalize_commit_v3_with_spec(
                finalize_request(&slot, 7_000),
                operation_finalize_spec(&operation),
            )
            .unwrap(),
    );
}

#[test]
fn finalize_gate_fails_closed_on_prepared_but_never_activated() {
    // Given: the owner durably prepared the dispatch but never activated it
    // (still Registered — activation may still happen later), while the
    // slot claims a closed effect via the legacy authority-free mint.
    let slot = sealed_operation_slot("finalize-prepared");
    let operation = nlos_store::SqliteOperationStore::open(&slot.operation_path).unwrap();
    let authority = slot.database.open();
    prepare_operation(&operation, &slot.operation);
    close_slot_with_effect_via_legacy_mint(&authority, &slot, 0xb3);

    // When: the operation-aware finalize runs.
    let result = authority.finalize_commit_v3_with_spec(
        finalize_request(&slot, 7_000),
        operation_finalize_spec(&operation),
    );

    // Then: the typed rejection names the Operation; zero terminal
    // mutation.
    assert!(matches!(
        result,
        Err(nlos_task::TaskStoreError::OperationDispatchNotActivated {
            operation_id,
            generation: 1,
        }) if operation_id == slot.operation.operation_id
    ));
    assert_permit_still_issued(&slot, &authority);

    // And: the retryable window stays honest — after the owner activates,
    // the same finalize commits.
    activate_operation(&operation, &slot.operation);
    committed_receipt(
        authority
            .finalize_commit_v3_with_spec(
                finalize_request(&slot, 7_000),
                operation_finalize_spec(&operation),
            )
            .unwrap(),
    );
}

#[test]
fn finalize_gate_fails_closed_on_canceled_preparation() {
    // Given: the owner prepared the dispatch and then the Operation was
    // canceled before activation (terminal CancelledBeforeEffect — the
    // preparation can never activate), while the slot claims a closed
    // effect via the legacy authority-free mint.
    let slot = sealed_operation_slot("finalize-canceled");
    let operation = nlos_store::SqliteOperationStore::open(&slot.operation_path).unwrap();
    let authority = slot.database.open();
    prepare_operation(&operation, &slot.operation);
    let snapshot = operation
        .request_cancel(
            operation_handle(&slot.operation),
            ReceiptId::from_bytes([0xdc; 16]),
        )
        .unwrap();
    assert!(matches!(
        snapshot.state,
        nlos_operation::OperationState::CancelledBeforeEffect { .. }
    ));
    close_slot_with_effect_via_legacy_mint(&authority, &slot, 0xb4);

    // When: the operation-aware finalize runs.
    let result = authority.finalize_commit_v3_with_spec(
        finalize_request(&slot, 7_000),
        operation_finalize_spec(&operation),
    );

    // Then: the typed canceled rejection names the Operation (distinct from
    // the retryable not-activated shape); zero terminal mutation.
    assert!(matches!(
        result,
        Err(nlos_task::TaskStoreError::OperationDispatchCancelled {
            operation_id,
            generation: 1,
        }) if operation_id == slot.operation.operation_id
    ));
    assert_permit_still_issued(&slot, &authority);
}

#[test]
fn finalize_gate_fails_closed_on_stale_sealed_generation() {
    // Given: the sealed endpoint pinned generation INITIAL, but the
    // Operation authority consulted at finalize time holds the same
    // operation_id at the next generation (stale sealed binding).
    let slot = sealed_operation_slot("finalize-stale");
    let drifted_root = AuthorityRoot::new("finalize-stale-drifted");
    std::fs::create_dir_all(&drifted_root.0).unwrap();
    let drifted =
        nlos_store::SqliteOperationStore::open(drifted_root.0.join("authority.sqlite3")).unwrap();
    let mut next_spec = operation_spec(0xc2);
    next_spec.generation = slot.operation.generation.checked_next().unwrap();
    drifted.register(next_spec).unwrap();
    let authority = slot.database.open();
    close_slot_with_effect_via_legacy_mint(&authority, &slot, 0xb5);

    // When: the operation-aware finalize consults the drifted authority.
    let result = authority.finalize_commit_v3_with_spec(
        finalize_request(&slot, 7_000),
        operation_finalize_spec(&drifted),
    );

    // Then: the typed stale-generation rejection names the Operation with
    // its sealed generation; zero terminal mutation.
    assert!(matches!(
        result,
        Err(nlos_task::TaskStoreError::OperationDispatchStaleGeneration {
            operation_id,
            sealed_generation: 1,
        }) if operation_id == slot.operation.operation_id
    ));
    assert_permit_still_issued(&slot, &authority);
}

#[test]
fn finalize_replay_reads_only_durable_task_rows() {
    // Given: an operation-aware finalize already committed.
    let slot = sealed_operation_slot("finalize-replay");
    let receipt = {
        let operation = nlos_store::SqliteOperationStore::open(&slot.operation_path).unwrap();
        let authority = slot.database.open();
        activate_operation(&operation, &slot.operation);
        close_slot_with_effect_via_legacy_mint(&authority, &slot, 0xb6);
        committed_receipt(
            authority
                .finalize_commit_v3_with_spec(
                    finalize_request(&slot, 7_000),
                    operation_finalize_spec(&operation),
                )
                .unwrap(),
        )
    };

    // When: the Task authority is reopened and the same finalize is
    // replayed against a fresh, unrelated, EMPTY Operation authority.
    let authority = slot.database.open();
    let unrelated_root = AuthorityRoot::new("finalize-replay-unrelated");
    std::fs::create_dir_all(&unrelated_root.0).unwrap();
    let unrelated =
        nlos_store::SqliteOperationStore::open(unrelated_root.0.join("authority.sqlite3")).unwrap();
    let decision = authority
        .finalize_commit_v3_with_spec(
            finalize_request(&slot, 7_000),
            operation_finalize_spec(&unrelated),
        )
        .unwrap();

    // Then: the replay returns the original receipt byte-for-byte without
    // any owner readback — the closed Task rows are the replay authority.
    let FinalizeSpecDecision::Plain(FinalizeDecision::Replayed(replayed)) = decision else {
        panic!("expected replayed finalize, got {decision:?}")
    };
    assert_eq!(*replayed, receipt);
}

#[test]
#[allow(clippy::too_many_lines)]
fn owner_prepare_activate_restart_replay_preserves_the_gate() {
    // Given: the owner durably prepared and activated, then the owner
    // authority was dropped and reopened (restart window). Exact
    // prepare/activate replay after the restart returns the original
    // durable receipts without re-dispatching (`f6530fc` semantics).
    let slot = sealed_operation_slot("finalize-restart");
    let (activation_receipt_id, first_proof) = {
        let operation = nlos_store::SqliteOperationStore::open(&slot.operation_path).unwrap();
        let preparation = prepare_operation(&operation, &slot.operation);
        let activation = match operation.activate_dispatch(preparation).unwrap() {
            nlos_store::OperationActivationDecision::Activated(activation) => activation,
            other @ nlos_store::OperationActivationDecision::Replayed(_) => {
                panic!("expected fresh activation, got {other:?}")
            }
        };
        (
            activation.activation_receipt_id,
            operation
                .inspect_activation_proof(operation_handle(&slot.operation))
                .unwrap(),
        )
    };
    assert_eq!(activation_receipt_id, first_proof.activation_receipt_id);

    // Restart: a fresh handle onto the same durable owner database.
    let operation = nlos_store::SqliteOperationStore::open(&slot.operation_path).unwrap();
    let replayed_preparation = prepare_operation(&operation, &slot.operation);
    assert_eq!(
        replayed_preparation.preparation_receipt_id,
        first_proof.preparation_receipt_id
    );
    let replay_decision = operation.activate_dispatch(replayed_preparation).unwrap();
    assert!(matches!(
        replay_decision,
        nlos_store::OperationActivationDecision::Replayed(_)
    ));
    assert_eq!(
        operation
            .inspect_activation_proof(operation_handle(&slot.operation))
            .unwrap(),
        first_proof
    );

    // When: the restarted owner gates the effect-permit mint and the
    // finalize in one continuous Task flow, then the owner restarts again
    // between mint and finalize.
    let authority = slot.database.open();
    let permit = issued_effect_permit(
        authority
            .request_effect_permit_with_operation_authority(&operation, effect_request(&slot, 0xb7))
            .unwrap(),
    );
    authority
        .consume_dispatch_token(nlos_task::DispatchRequest {
            task_id: task_id(),
            attempt_id: slot.spec.attempt_id,
            attempt_generation: slot.spec.attempt_generation,
            permit_id: slot.commit_permit.permit_id,
            permit_epoch: slot.commit_permit.permit_epoch,
            effect_permit_id: permit.effect_permit_id,
            dispatch_token: permit.one_shot_dispatch_token,
            dispatched_at_ms: 5_000,
        })
        .unwrap();
    authority
        .record_effect_outcome(OutcomeRequest {
            task_id: task_id(),
            attempt_id: slot.spec.attempt_id,
            attempt_generation: slot.spec.attempt_generation,
            permit_id: slot.commit_permit.permit_id,
            permit_epoch: slot.commit_permit.permit_epoch,
            effect_seq: 0,
            outcome: Outcome::Closed {
                authoritative_closure_digest: [0xa9; 32],
            },
            recorded_at_ms: 6_000,
        })
        .unwrap();
    drop(operation);
    let restarted = nlos_store::SqliteOperationStore::open(&slot.operation_path).unwrap();
    let receipt = committed_receipt(
        authority
            .finalize_commit_v3_with_spec(
                finalize_request(&slot, 7_000),
                operation_finalize_spec(&restarted),
            )
            .unwrap(),
    );

    // Then: the exact-replay receipts keep both gates open and the finalize
    // commits against the restarted owner.
    assert_ne!(receipt.receipt_id, first_proof.activation_receipt_id);
}

#[test]
fn negative_gate_no_task_side_operation_plan_machinery_in_schema() {
    // ADR-0017 决定 2 negative gate: W29-C is verify-half only. The Task
    // authority schema must contain NO Operation plan state machine,
    // recovery-ledger table group, or worker-cycle storage — recovery
    // ownership stays with the effect machinery (EffectSlot/effect
    // history/EFFECT_UNKNOWN).
    let database = Database::new();
    drop(database.open());
    let raw = rusqlite::Connection::open(&database.0).unwrap();

    // No schema bump this lane: the current version stays v44 (W29-A).
    let version: i64 = raw
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 44);

    // No Task-side Operation table family exists at all — the
    // semantic/resource naming family (`task_*_commit_plans`,
    // `task_*_recovery*`, `task_*_finalize_envelopes`) has no Operation
    // mirror.
    let mut statement = raw
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'task_operation%' ORDER BY name")
        .unwrap();
    let operation_tables: Vec<String> = statement
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(
        operation_tables,
        Vec::<String>::new(),
        "ADR-0017 negative gate violated: Task-side Operation tables exist"
    );

    // Explicit absence of the plan/ledger shapes the ADR forbids.
    let mut statement = raw
        .prepare(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name IN (
                'task_operation_plans',
                'task_operation_recovery',
                'task_operation_recovery_alert_receipts',
                'task_operation_finalize_envelopes',
                'task_operation_finalize_satisfactions',
                'operation_commit_plans',
                'operation_dispatch_ledger'
            )",
        )
        .unwrap();
    let forbidden: i64 = statement.query_row([], |row| row.get(0)).unwrap();
    assert_eq!(forbidden, 0);
}
