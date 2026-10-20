//! W48-2: the late Operation-outcome routing lane
//! ([`SqliteTaskAuthority::reconcile_late_operation_outcome`]).
//!
//! The durable closed loop under test: an operation whose wake permission
//! was fenced by a cancel still terminalized with an authoritative receipt
//! (`CanonicalizedForReconciliation` on the Operation side); the outbox
//! consumer replays those facts into the task effect plane, and the unique
//! effect slot bound by the sealed `OperationBinding` endpoint converges to
//! `EffectClosed` — reconciled by its real outcome, never renamed
//! (`[TASK-CANCEL-003]`). A handle with no bound slot is a typed no-route
//! answer, and only a `Completed` terminal state is consumable evidence.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_operation::OperationState;
use nlos_runtime::FiberHandle;
use nlos_task::{
    AttemptSpec, EffectPermitDecision, EffectPermitRequest, IssuedPermit,
    LateOperationOutcomeDecision, LateOperationOutcomeRequest, LogicalEffectDescriptor,
    PermitDecision, PermitRecord, PermitRequest, PlannedEffect, RequiredSatisfaction,
    RequiredSatisfactionProof, SlotState, SnapshotBundle, SnapshotConsistency, SqliteTaskAuthority,
    TaskSnapshotReceiptSpec, TaskSpec, TaskWriteSetArtifactRead, TaskWriteSetEffectEndpointRequest,
    TaskWriteSetRequest, empty_effect_history_root, expected_success_assertion_digest,
    late_operation_closure_digest,
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
            "nlos-task-late-outcome-{}-{}.sqlite3",
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
            "nlos-task-late-outcome-{label}-{}-{}",
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

fn attempt(seed: u8) -> AttemptSpec {
    AttemptSpec {
        task_id: task_id(),
        attempt_id: TaskAttemptId::from_bytes([seed; 16]),
        attempt_generation: Generation::INITIAL,
        snapshot: SnapshotBundle {
            snapshot_id: TaskSnapshotId::from_bytes([seed.wrapping_add(1); 16]),
            snapshot_digest: [seed.wrapping_add(2); 32],
            expected_head_commit_seq: 0,
            effect_history_root: empty_effect_history_root(),
            retry_fence_epoch: 0,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes([seed.wrapping_add(3); 16]),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(4); 16]),
        registered_at_ms: 2_000 + i64::from(seed),
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
        required: true,
        required_condition_digest: None,
        success_criteria_digest: [0x93; 32],
        action_proposal_digest: [0x94; 32],
    }
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

/// The late-outcome routing request mirroring one durable outbox entry.
fn late_outcome_request(
    operation: &nlos_operation::OperationSpec,
    terminal_state: OperationState,
    now_ms: i64,
) -> LateOperationOutcomeRequest {
    LateOperationOutcomeRequest {
        operation: nlos_operation::OperationHandle {
            operation_id: operation.operation_id,
            generation: operation.generation,
        },
        terminal_state,
        reconciled_at_ms: now_ms,
    }
}

/// Given-fixture: task + attempt + snapshot receipt + registered Operation
/// participant + sealed write set with an `OperationBinding` endpoint on
/// (required) effect slot 0 + commit permit + activated dispatch + consumed
/// one-shot token, so the slot is `Dispatched` exactly as a fenced fiber
/// would have left it.
struct DispatchedOperationSlot {
    database: Database,
    _roots: (AuthorityRoot, AuthorityRoot),
    operation: nlos_operation::OperationSpec,
    spec: AttemptSpec,
    commit_permit: PermitRecord,
}

fn dispatched_operation_slot(label: &str) -> DispatchedOperationSlot {
    operation_slot(label, true)
}

/// The same fixture with `consume_token = false` stops right after the
/// effect-permit mint, leaving the slot `Permitted`. The durable chain
/// assembles in one piece, so the constructor stays contiguous for audit.
#[allow(clippy::too_many_lines)]
fn operation_slot(label: &str, consume_token: bool) -> DispatchedOperationSlot {
    let database = Database::new();
    let artifact_root = AuthorityRoot::new(label);
    let operation_root = AuthorityRoot::new(label);
    std::fs::create_dir_all(&operation_root.0).unwrap();
    let operation_path = operation_root.0.join("authority.sqlite3");
    let artifact = nlos_artifact::ArtifactStore::open(&artifact_root.0).unwrap();
    let artifact_id = ArtifactId::from_bytes([0xc1; 16]);
    artifact
        .create_artifact(nlos_artifact::CreateArtifactSpec {
            artifact_id,
            idempotency_key: IdempotencyKey::from_bytes([0xc2; 16]),
            content_type: "application/octet-stream".to_owned(),
            application_id: None,
            owner: None,
            created_at_ms: 1_500,
        })
        .unwrap();
    let operation_store = nlos_store::SqliteOperationStore::open(&operation_path).unwrap();
    let operation = operation_spec(0xc3);
    operation_store.register(operation).unwrap();

    let authority = database.open();
    authority
        .register_task(TaskSpec {
            application_id: None,
            plan_revision: None,
            task_id: task_id(),
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
        })
        .unwrap();
    let spec = attempt(0xc4);
    let snapshot_receipt = ReceiptId::from_bytes([0xc5; 16]);
    authority
        .register_snapshot_receipt(TaskSnapshotReceiptSpec {
            task_id: task_id(),
            snapshot: spec.snapshot,
            receipt_id: snapshot_receipt,
            builder_id: [0xc6; 16],
            builder_version_digest: [0xc7; 32],
            per_authority_checkpoint_receipts: vec![ReceiptId::from_bytes([0xc8; 16])],
            dependency_closure_root: [0xc9; 32],
            semantic_resolver_digest: [0xca; 32],
            canonical_iteration_digest: [0xcb; 32],
            achieved_consistency: SnapshotConsistency::Causal,
            built_at_ms: 1_100,
            authority_id: [0xcc; 16],
            key_id: [0xcd; 16],
            signature: [0xce; 64],
        })
        .unwrap();
    authority
        .register_attempt_with_snapshot_receipt(spec, snapshot_receipt)
        .unwrap();
    let registry = authority.inspect_participant_registry(task_id()).unwrap();
    let registry_binding = nlos_task::ParticipantRegistryBinding {
        generation: registry.generation,
        root: registry.root,
    };
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
    let authorities = nlos_task::Authorities {
        artifact: Some(&artifact),
        process: None,
        semantic: None,
        resource: None,
        operation: Some(&operation_store),
        channel: None,
    };
    let sealed = authority
        .seal_task_write_set_with_authorities_struct(
            authorities,
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
                effect_endpoints: vec![TaskWriteSetEffectEndpointRequest::OperationBinding {
                    effect_seq: 0,
                    operation_id: operation.operation_id,
                    expected_operation_generation: operation.generation,
                }],
                idempotency_key: IdempotencyKey::from_bytes([0xcf; 16]),
                sealed_at_ms: 1_200,
            },
        )
        .unwrap()
        .record()
        .clone();
    let commit_permit = issued_commit(
        authority
            .request_commit_permit_with_authorities_struct(
                authorities,
                PermitRequest {
                    task_id: task_id(),
                    attempt_id: spec.attempt_id,
                    attempt_generation: spec.attempt_generation,
                    write_set_root: sealed.write_set_root,
                    planned_effects: sealed.planned_effects.clone(),
                    idempotency_key: IdempotencyKey::from_bytes([0xd0; 16]),
                    valid_until_ms: 9_000,
                    requested_at_ms: 3_000,
                },
            )
            .unwrap(),
    );
    // Owner-side activation (ADR-0005 authority-first order), then the
    // effect permit mint and the one-shot token consumption.
    let preparation = match operation_store
        .prepare_dispatch(
            nlos_operation::OperationHandle {
                operation_id: operation.operation_id,
                generation: operation.generation,
            },
            CallbackId::from_bytes([0xd1; 16]),
        )
        .unwrap()
    {
        nlos_store::OperationPrepareDecision::Prepared(preparation)
        | nlos_store::OperationPrepareDecision::Replayed(preparation) => preparation,
    };
    operation_store.activate_dispatch(preparation).unwrap();
    let issued = issued_effect_permit(
        authority
            .request_effect_permit_with_operation_authority(
                &operation_store,
                EffectPermitRequest {
                    task_id: task_id(),
                    attempt_id: spec.attempt_id,
                    attempt_generation: spec.attempt_generation,
                    permit_id: commit_permit.permit_id,
                    permit_epoch: commit_permit.permit_epoch,
                    effect_seq: 0,
                    idempotency_key: IdempotencyKey::from_bytes([0xd2; 16]),
                    valid_until_ms: 9_000,
                    requested_at_ms: 4_000,
                },
            )
            .unwrap(),
    );
    if consume_token {
        authority
            .consume_dispatch_token(nlos_task::DispatchRequest {
                task_id: task_id(),
                attempt_id: spec.attempt_id,
                attempt_generation: spec.attempt_generation,
                permit_id: commit_permit.permit_id,
                permit_epoch: commit_permit.permit_epoch,
                effect_permit_id: issued.effect_permit_id,
                dispatch_token: issued.one_shot_dispatch_token,
                dispatched_at_ms: 5_000,
            })
            .unwrap();
    }
    DispatchedOperationSlot {
        database,
        _roots: (artifact_root, operation_root),
        operation,
        spec,
        commit_permit,
    }
}

/// Given/When/Then: given a `Dispatched` slot bound to an operation whose
/// fenced terminal callback completed with an authoritative receipt; when
/// the late outcome is routed; then the slot converges to `EffectClosed`
/// with a receipt whose proof digest binds the outbox facts, the
/// cross-attempt history entry cites the Operation, the permit stays
/// `Issued`, and the permit can then finalize on the closure evidence.
#[test]
fn late_completed_outcome_converges_slot_and_enables_finalize() {
    let slot = dispatched_operation_slot("converge");
    let authority = slot.database.open();

    let decision = authority
        .reconcile_late_operation_outcome(late_outcome_request(
            &slot.operation,
            OperationState::Completed {
                receipt_id: ReceiptId::from_bytes([0xe1; 16]),
            },
            6_000,
        ))
        .unwrap();
    let receipt = match decision {
        LateOperationOutcomeDecision::Closed(receipt) => *receipt,
        other => panic!("expected a fresh closure, got {other:?}"),
    };
    assert_eq!(
        receipt.proof_digest,
        late_operation_closure_digest(
            nlos_operation::OperationHandle {
                operation_id: slot.operation.operation_id,
                generation: slot.operation.generation,
            },
            ReceiptId::from_bytes([0xe1; 16]),
        ),
        "the closure proof must bind the outbox facts deterministically"
    );

    let stored = authority
        .inspect_effect_slot(slot.commit_permit.permit_id, 0)
        .unwrap();
    assert_eq!(stored.state, SlotState::EffectClosed);
    assert_eq!(stored.effect_receipt_id, Some(receipt.receipt_id));

    let history = authority.list_effect_history(task_id()).unwrap();
    let entry = history
        .iter()
        .find(|entry| entry.logical_effect_id == stored.logical_effect_id)
        .expect("closure appended an effect history entry");
    assert_eq!(
        entry.operation_id,
        Some(slot.operation.operation_id.into_bytes())
    );
    assert_eq!(entry.authoritative_effect_receipt_id, receipt.receipt_id);

    // The permit is terminalizable on the late closure evidence: the
    // required slot is satisfied by the receipt-bound success assertion.
    let proof = RequiredSatisfaction {
        effect_seq: 0,
        proof: RequiredSatisfactionProof::EffectClosedSuccess {
            success_assertion_digest: expected_success_assertion_digest(&stored, &receipt),
        },
    };
    let finalized = authority
        .finalize_commit_v3(nlos_task::FinalizeRequestV3 {
            base: nlos_task::FinalizeRequest {
                task_id: task_id(),
                attempt_id: slot.spec.attempt_id,
                attempt_generation: slot.spec.attempt_generation,
                permit_id: slot.commit_permit.permit_id,
                new_effect_history_root: [0; 32],
                new_retry_fence_epoch: 0,
                finalized_at_ms: 7_000,
            },
            required_satisfaction: vec![proof],
            fenced_participant_digest: [0xe2; 32],
        })
        .unwrap();
    assert!(matches!(
        finalized,
        nlos_task::FinalizeDecision::Committed(_)
    ));
}

/// Given/When/Then: given a slot already converged by the late-outcome
/// lane; when the same outbox facts are redelivered (at-least-once); then
/// the exact durable receipt replays, no second history entry appears, and
/// the control epoch does not move twice.
#[test]
fn late_outcome_redelivery_replays_the_original_receipt() {
    let slot = dispatched_operation_slot("replay");
    let authority = slot.database.open();
    let request = late_outcome_request(
        &slot.operation,
        OperationState::Completed {
            receipt_id: ReceiptId::from_bytes([0xe3; 16]),
        },
        6_000,
    );
    let first = match authority.reconcile_late_operation_outcome(request).unwrap() {
        LateOperationOutcomeDecision::Closed(receipt) => *receipt,
        other => panic!("expected a fresh closure, got {other:?}"),
    };
    let epoch_after_first = authority.inspect_task(task_id()).unwrap().control_epoch;
    let history_len = authority.list_effect_history(task_id()).unwrap().len();

    let second = match authority.reconcile_late_operation_outcome(request).unwrap() {
        LateOperationOutcomeDecision::Replayed(receipt) => *receipt,
        other => panic!("expected an idempotent replay, got {other:?}"),
    };
    assert_eq!(second.receipt_id, first.receipt_id);
    assert_eq!(
        authority.inspect_task(task_id()).unwrap().control_epoch,
        epoch_after_first,
        "a replay must not bump the control epoch again"
    );
    assert_eq!(
        authority.list_effect_history(task_id()).unwrap().len(),
        history_len,
        "a replay must not append a second history entry"
    );

    // A divergent late fact for the same converged slot fails closed.
    let conflict = authority.reconcile_late_operation_outcome(late_outcome_request(
        &slot.operation,
        OperationState::Completed {
            receipt_id: ReceiptId::from_bytes([0xe4; 16]),
        },
        6_500,
    ));
    assert!(matches!(
        conflict,
        Err(nlos_task::TaskStoreError::IdempotencyConflict)
    ));
}

/// Given/When/Then: given an operation no write set ever bound (a wake-only
/// operation); when its late outcome is offered; then the typed no-route
/// decision returns without writing anything — no fact is invented.
#[test]
fn unbound_operation_returns_typed_no_route() {
    let slot = dispatched_operation_slot("no-route");
    let authority = slot.database.open();
    let stranger = operation_spec(0xe5);

    let decision = authority
        .reconcile_late_operation_outcome(late_outcome_request(
            &stranger,
            OperationState::Completed {
                receipt_id: ReceiptId::from_bytes([0xe6; 16]),
            },
            6_000,
        ))
        .unwrap();
    assert_eq!(
        decision,
        LateOperationOutcomeDecision::NoBoundSlot {
            operation_id: stranger.operation_id,
            generation: stranger.generation,
        }
    );

    // The bound slot is untouched by the stranger's routing attempt.
    let stored = authority
        .inspect_effect_slot(slot.commit_permit.permit_id, 0)
        .unwrap();
    assert_eq!(stored.state, SlotState::Dispatched);
    assert_eq!(
        authority.list_effect_history(task_id()).unwrap(),
        Vec::new()
    );
}

/// Given/When/Then: given a fenced terminal state that is not `Completed`;
/// when it is offered as closure evidence; then the routing fails closed
/// with the typed reconcile-state error and the slot keeps its `Dispatched`
/// state — `Failed`/`PartialEffect` facts are durable truth about the
/// Operation but not consumable proof that the task-plane effect closed.
#[test]
fn non_completed_terminal_state_is_not_consumable_evidence() {
    let slot = dispatched_operation_slot("failed-outcome");
    let authority = slot.database.open();

    let rejected = authority.reconcile_late_operation_outcome(late_outcome_request(
        &slot.operation,
        OperationState::Failed {
            receipt_id: ReceiptId::from_bytes([0xe7; 16]),
        },
        6_000,
    ));
    assert!(matches!(
        rejected,
        Err(nlos_task::TaskStoreError::InvalidReconcileState { .. })
    ));
    let stored = authority
        .inspect_effect_slot(slot.commit_permit.permit_id, 0)
        .unwrap();
    assert_eq!(stored.state, SlotState::Dispatched);
    assert_eq!(
        authority.list_effect_history(task_id()).unwrap(),
        Vec::new()
    );
}

/// Given/When/Then: given a bound slot whose dispatch token was never
/// consumed (still `Permitted`); when a late completion is routed; then the
/// typed slot-state error refuses and nothing is written — the task plane
/// never dispatched, so an Operation-side completion cannot rename an
/// unconsumed token into an effect closure.
#[test]
fn permitted_slot_refuses_late_outcome_routing() {
    let slot = operation_slot("permitted-slot", false);
    let authority = slot.database.open();
    let stored = authority
        .inspect_effect_slot(slot.commit_permit.permit_id, 0)
        .unwrap();
    assert_eq!(stored.state, SlotState::Permitted);

    let rejected = authority.reconcile_late_operation_outcome(late_outcome_request(
        &slot.operation,
        OperationState::Completed {
            receipt_id: ReceiptId::from_bytes([0xe9; 16]),
        },
        6_000,
    ));
    assert!(matches!(
        rejected,
        Err(nlos_task::TaskStoreError::InvalidEffectSlotState {
            state: SlotState::Permitted
        })
    ));
    let after = authority
        .inspect_effect_slot(slot.commit_permit.permit_id, 0)
        .unwrap();
    assert_eq!(after, stored, "the refusal must leave the slot untouched");
    assert_eq!(
        authority.list_effect_history(task_id()).unwrap(),
        Vec::new()
    );
}
