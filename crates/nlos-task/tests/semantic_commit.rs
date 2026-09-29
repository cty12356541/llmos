#![allow(deprecated)] // Ladder constructors deprecated in favor of the *_with_authorities_struct entries.
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_capability::CapabilityTarget;
use nlos_semantic::{
    PublishSemanticPublicationRequest, SemanticAuthority, SemanticPublicationReceipt,
};
use nlos_task::{
    AdoptionRequest, AttemptSpec, Authorities, AuthorityLeaseEffectPermitRequest,
    AuthorityLeaseNoEffectRequest, AuthorityLeasePermitRequest, AuthorityLeaseRequest,
    DispatchRequest, EffectPermitDecision, EffectPermitRequest, FinalizeDecision, FinalizeRequest,
    FinalizeRequestV3, FinalizeSemanticCommitRequest, LogicalEffectDescriptor,
    NestedSemanticPublicationReceipt, NoEffectReason, NoEffectRequest, Outcome, OutcomeRequest,
    PermitDecision, PermitRecord, PermitRequest, PlanSemanticCommitRequest, PlannedEffect,
    PrepareSemanticFinalizeRequest, RecordSemanticPublicationsRequest, RequiredSatisfaction,
    RequiredSatisfactionProof, SemanticCommitPlanState, SemanticFinalizeDecision, SnapshotBundle,
    SnapshotConsistency, SqliteTaskAuthority, TaskSnapshotReceiptSpec, TaskSpec, TaskStoreError,
    TaskWriteSetEffectEndpointRequest, TaskWriteSetRequest, TaskWriteSetSemanticAppendRequest,
    TaskWriteSetSemanticRequiredDurability, TaskWriteSetSemanticTarget, empty_effect_history_root,
};
use nlos_types::{
    CancellationScopeId, Generation, IdempotencyKey, NamespaceId, ProcessId, ReceiptId,
    SemanticEventId, TaskAttemptId, TaskId, TaskSnapshotId,
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
            "nlos-task-semantic-commit-{}-{suffix}",
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
    let event_id = SemanticEventId::from_bytes([0x10; 32]);
    let admission_receipt_id = ReceiptId::from_bytes([0x20; 16]);
    let durability_receipt_id = ReceiptId::from_bytes([0x30; 16]);
    let target = NamespaceId::from_bytes([0x40; 16]);
    let raw = Connection::open(root.join("semantic-authority.db")).expect("open raw Semantic db");
    raw.execute(
        "INSERT INTO content_objects (content_digest, media_type, exact_bytes)
         VALUES (?1, ?2, ?3)",
        rusqlite::params![[0x50u8; 32].as_slice(), "text/plain", b"semantic"],
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
            [0x51u8, 0x52, 0x53].as_slice(),
            target.as_bytes().as_slice(),
            [0x54u8; 16].as_slice(),
            [0x55u8; 16].as_slice(),
            [0x56u8; 16].as_slice(),
            [0x57u8; 16].as_slice(),
            [0x50u8; 32].as_slice(),
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
            [0x58u8; 32].as_slice(),
            [0x59u8; 16].as_slice(),
            [0x5au8; 16].as_slice(),
            [0x5bu8; 16].as_slice(),
            [0x5cu8; 64].as_slice(),
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
            [0x5du8; 32].as_slice(),
            [0x5eu8; 64].as_slice(),
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

fn nested(
    owner: &SemanticPublicationReceipt,
    target: NamespaceId,
) -> NestedSemanticPublicationReceipt {
    NestedSemanticPublicationReceipt {
        receipt_id: owner.receipt_id,
        task_id: owner.task_id,
        permit_id: owner.permit_id,
        write_set_root: owner.write_set_root,
        event_id: owner.event_id,
        target: TaskWriteSetSemanticTarget::Namespace(target),
        log_seq: owner.log_seq,
        admission_receipt_id: owner.admission_receipt_id,
        durability_receipt_id: owner.durability_receipt_id,
        semantic_checkpoint_after: owner.semantic_checkpoint_after,
        created_at_ms: owner.created_at_ms,
    }
}

fn mixed_effect(task_id: TaskId) -> PlannedEffect {
    PlannedEffect {
        descriptor: LogicalEffectDescriptor {
            task_id,
            task_generation: Generation::INITIAL,
            intent_spec_id: [0x73; 32],
            stable_action_slot: 1,
            target_authority_object_id: [0x74; 32],
            effect_class: 1,
            idempotency_scope: 1,
        },
        required: false,
        required_condition_digest: None,
        success_criteria_digest: [0x75; 32],
        action_proposal_digest: [0x76; 32],
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn semantic_owner_receipt_is_consumed_nested_and_replayed() {
    run_semantic_owner_receipt_lifecycle(false);
}

#[test]
#[allow(clippy::too_many_lines)]
fn mixed_effect_semantic_receipt_finalizes_in_one_transaction() {
    run_semantic_owner_receipt_lifecycle(true);
}

#[allow(clippy::too_many_lines)]
fn run_semantic_owner_receipt_lifecycle(with_effect: bool) {
    let fixture = Fixture::new();
    let (semantic, event_id, admission_receipt_id, durability_receipt_id) =
        seed_semantic_authority(&fixture.semantic_root);
    let artifact = nlos_artifact::ArtifactStore::open(&fixture.artifact_root).unwrap();
    let task_id = TaskId::from_bytes([0x60; 16]);
    let attempt_id = TaskAttemptId::from_bytes([0x61; 16]);
    let target = NamespaceId::from_bytes([0x40; 16]);
    let attempt = AttemptSpec {
        task_id,
        attempt_id,
        attempt_generation: Generation::INITIAL,
        snapshot: SnapshotBundle {
            snapshot_id: TaskSnapshotId::from_bytes([0x62; 16]),
            snapshot_digest: [0x63; 32],
            expected_head_commit_seq: 0,
            effect_history_root: empty_effect_history_root(),
            retry_fence_epoch: 0,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes([0x64; 16]),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes([0x65; 16]),
        registered_at_ms: 10,
    };
    let task = SqliteTaskAuthority::open(&fixture.task_path).unwrap();
    task.register_task(TaskSpec {
        application_id: None,
        plan_revision: None,
        task_id,
        task_generation: Generation::INITIAL,
        registered_at_ms: 1,
    })
    .unwrap();
    task.register_snapshot_receipt(TaskSnapshotReceiptSpec {
        task_id,
        snapshot: attempt.snapshot,
        receipt_id: ReceiptId::from_bytes([0x66; 16]),
        builder_id: [0x67; 16],
        builder_version_digest: [0x68; 32],
        per_authority_checkpoint_receipts: vec![ReceiptId::from_bytes([0x69; 16])],
        dependency_closure_root: [0x6a; 32],
        semantic_resolver_digest: [0x6b; 32],
        canonical_iteration_digest: [0x6c; 32],
        achieved_consistency: SnapshotConsistency::Causal,
        built_at_ms: 2,
        authority_id: [0x6d; 16],
        key_id: [0x6e; 16],
        signature: [0x6f; 64],
    })
    .unwrap();
    task.register_attempt_with_snapshot_receipt(attempt, ReceiptId::from_bytes([0x66; 16]))
        .unwrap();
    let authority_lease = task
        .acquire_authority_lease(AuthorityLeaseRequest {
            holder_id: ProcessId::from_bytes([0x01; 16]),
            idempotency_key: IdempotencyKey::from_bytes([0x6f; 16]),
            requested_at_ms: 5,
            ttl_ms: 1_000,
        })
        .unwrap()
        .record();
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
    let effect = mixed_effect(task_id);
    let planned_effects = if with_effect {
        vec![effect.clone()]
    } else {
        Vec::new()
    };
    let effect_endpoints = if with_effect {
        vec![TaskWriteSetEffectEndpointRequest::SemanticAdmission { effect_seq: 0 }]
    } else {
        Vec::new()
    };
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
                    expected_admission_policy_digest: [0x58; 32],
                    durability_receipt_id: Some(durability_receipt_id),
                }],
                resource_reservations: Vec::new(),
                planned_effects: planned_effects.clone(),
                effect_endpoints,
                idempotency_key: IdempotencyKey::from_bytes([0x70; 16]),
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
    let permit_request = PermitRequest {
        task_id,
        attempt_id,
        attempt_generation: Generation::INITIAL,
        write_set_root: write_set.write_set_root,
        planned_effects: planned_effects.clone(),
        idempotency_key: IdempotencyKey::from_bytes([0x71; 16]),
        valid_until_ms: 1_000,
        requested_at_ms: 5,
    };
    let permit_decision = task
        .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
            permit: permit_request,
            lease: authority_lease,
        })
        .unwrap();
    let permit = match permit_decision {
        PermitDecision::Issued(permit) => *permit,
        other => panic!("expected issued permit, got {other:?}"),
    };
    let plan = task
        .plan_semantic_commit(PlanSemanticCommitRequest {
            task_id,
            attempt_id,
            attempt_generation: Generation::INITIAL,
            permit_id: permit.permit_id,
            idempotency_key: IdempotencyKey::from_bytes([0x72; 16]),
            planned_at_ms: 6,
        })
        .unwrap()
        .record()
        .clone();
    assert!(matches!(
        task.authorize_semantic_publication(plan.plan_id, 7)
            .unwrap()
            .record()
            .state,
        SemanticCommitPlanState::Publishing
    ));
    let owner = semantic
        .publish_semantic_publication(PublishSemanticPublicationRequest {
            task_id,
            permit_id: permit.permit_id,
            write_set_root: write_set.write_set_root,
            event_id,
            target: CapabilityTarget::Namespace(target),
            admission_receipt_id,
            durability_receipt_id: Some(durability_receipt_id),
            published_at_ms: 8,
        })
        .unwrap()
        .receipt();
    let owner_copy = nested(&owner, target);
    let mut bad_copy = owner_copy;
    bad_copy.semantic_checkpoint_after[0] ^= 1;
    assert!(matches!(
        task.record_semantic_publications(
            &semantic,
            RecordSemanticPublicationsRequest {
                plan_id: plan.plan_id,
                receipts: vec![bad_copy],
                observed_at_ms: 9,
            },
        ),
        Err(TaskStoreError::SemanticPublicationConflict { .. })
    ));
    let progress = task
        .record_semantic_publications(
            &semantic,
            RecordSemanticPublicationsRequest {
                plan_id: plan.plan_id,
                receipts: vec![owner_copy],
                observed_at_ms: 10,
            },
        )
        .unwrap();
    assert_eq!(progress.plan.state, SemanticCommitPlanState::Ready);
    assert_eq!(progress.publications, vec![owner_copy]);
    if with_effect {
        let envelope = task
            .prepare_semantic_finalize(PrepareSemanticFinalizeRequest {
                plan_id: plan.plan_id,
                required_satisfaction: Vec::new(),
                fenced_participant_digest: [0; 32],
                prepared_at_ms: 11,
            })
            .unwrap();
        assert_eq!(envelope.record().plan_id, plan.plan_id);
        assert_eq!(
            task.inspect_semantic_finalize_envelope(plan.plan_id)
                .unwrap()
                .unwrap(),
            *envelope.record()
        );
        assert!(matches!(
            task.finalize_semantic_commit(FinalizeSemanticCommitRequest {
                plan_id: plan.plan_id,
                finalized_at_ms: 11,
            }),
            Err(TaskStoreError::AuthorityLeaseRequired)
        ));
        let premature_request = FinalizeRequestV3 {
            base: FinalizeRequest {
                task_id,
                attempt_id,
                attempt_generation: Generation::INITIAL,
                permit_id: permit.permit_id,
                new_effect_history_root: empty_effect_history_root(),
                new_retry_fence_epoch: 0,
                finalized_at_ms: 13,
            },
            required_satisfaction: Vec::new(),
            fenced_participant_digest: [0; 32],
        };
        assert!(matches!(
            task.finalize_commit_v3_with_semantic_publications_and_authority_lease(
                &semantic,
                plan.plan_id,
                premature_request,
                authority_lease,
            ),
            Err(TaskStoreError::OutstandingEffectSlots { count: 1 })
        ));
        assert_eq!(
            task.inspect_semantic_commit_progress(plan.plan_id)
                .unwrap()
                .plan
                .state,
            SemanticCommitPlanState::Ready
        );
    }
    let (committed, replay_request) = if with_effect {
        // The permit is lease-bound, so the effect plane must present the
        // live lease (`TK-B2`).
        let issued = match task
            .request_effect_permit_with_authority_lease(AuthorityLeaseEffectPermitRequest {
                permit: EffectPermitRequest {
                    task_id,
                    attempt_id,
                    attempt_generation: Generation::INITIAL,
                    permit_id: permit.permit_id,
                    permit_epoch: permit.permit_epoch,
                    effect_seq: 0,
                    idempotency_key: IdempotencyKey::from_bytes([0x77; 16]),
                    valid_until_ms: 1_000,
                    requested_at_ms: 11,
                },
                lease: authority_lease,
            })
            .unwrap()
        {
            EffectPermitDecision::Issued(issued) | EffectPermitDecision::Replayed(issued) => {
                *issued
            }
        };
        task.record_no_effect_with_authority_lease(AuthorityLeaseNoEffectRequest {
            no_effect: NoEffectRequest {
                task_id,
                attempt_id,
                attempt_generation: Generation::INITIAL,
                permit_id: permit.permit_id,
                permit_epoch: permit.permit_epoch,
                effect_seq: 0,
                reason: NoEffectReason::NotSelected,
                dispatch_token: Some(issued.one_shot_dispatch_token),
                recorded_at_ms: 12,
            },
            lease: authority_lease,
        })
        .unwrap();
        let finalize_request = FinalizeRequestV3 {
            base: FinalizeRequest {
                task_id,
                attempt_id,
                attempt_generation: Generation::INITIAL,
                permit_id: permit.permit_id,
                new_effect_history_root: empty_effect_history_root(),
                new_retry_fence_epoch: 0,
                finalized_at_ms: 13,
            },
            required_satisfaction: Vec::new(),
            fenced_participant_digest: [0; 32],
        };
        let decision = task
            .finalize_commit_v3_with_persisted_semantic_envelope_and_authority_lease(
                &semantic,
                plan.plan_id,
                13,
                authority_lease,
            )
            .unwrap();
        assert!(matches!(decision, SemanticFinalizeDecision::Committed(_)));
        assert_eq!(decision.receipt().semantic_publications, vec![owner_copy]);
        assert_eq!(decision.receipt().task_receipt.new_head_commit_seq, 1);
        (decision, Some(finalize_request))
    } else {
        assert!(matches!(
            task.finalize_semantic_commit(FinalizeSemanticCommitRequest {
                plan_id: plan.plan_id,
                finalized_at_ms: 11,
            }),
            Err(TaskStoreError::AuthorityLeaseRequired)
        ));
        let decision = task
            .finalize_semantic_commit_with_authority_lease(
                FinalizeSemanticCommitRequest {
                    plan_id: plan.plan_id,
                    finalized_at_ms: 11,
                },
                authority_lease,
            )
            .unwrap();
        assert!(matches!(decision, SemanticFinalizeDecision::Committed(_)));
        assert_eq!(decision.receipt().semantic_publications, vec![owner_copy]);
        assert_eq!(decision.receipt().task_receipt.new_head_commit_seq, 1);
        (decision, None)
    };
    drop(task);
    let raw = Connection::open(&fixture.task_path).unwrap();
    assert!(
        raw.execute(
            "UPDATE task_semantic_publication_receipts SET created_at_ms = 12",
            [],
        )
        .is_err()
    );
    if with_effect {
        assert!(
            raw.execute(
                "UPDATE task_semantic_finalize_envelopes SET prepared_at_ms = 12",
                [],
            )
            .is_err()
        );
    }
    drop(raw);
    let reopened = SqliteTaskAuthority::open(&fixture.task_path).unwrap();
    let replay = if let Some(mut request) = replay_request {
        request.base.finalized_at_ms = 99;
        if with_effect {
            reopened
                .finalize_commit_v3_with_persisted_semantic_envelope_and_authority_lease(
                    &semantic,
                    plan.plan_id,
                    99,
                    authority_lease,
                )
                .unwrap()
        } else {
            reopened
                .finalize_commit_v3_with_semantic_publications(&semantic, plan.plan_id, request)
                .unwrap()
        }
    } else {
        reopened
            .finalize_semantic_commit_with_authority_lease(
                FinalizeSemanticCommitRequest {
                    plan_id: plan.plan_id,
                    finalized_at_ms: 99,
                },
                authority_lease,
            )
            .unwrap()
    };
    assert!(matches!(replay, SemanticFinalizeDecision::Replayed(_)));
    assert_eq!(replay.receipt(), committed.receipt());
}

/// Drives the direct Semantic-only lifecycle (no lease, no Effect slots) up
/// to a READY plan and returns every handle the W43-E1 regression tests
/// need: the fixture roots, the owner authority, the Task/attempt identity,
/// the winning permit, and the READY plan id.
struct SemanticOnlyReadyPlan {
    fixture: Fixture,
    task_id: TaskId,
    attempt_id: TaskAttemptId,
    permit: PermitRecord,
    plan_id: nlos_task::SemanticCommitPlanId,
}

#[allow(clippy::too_many_lines)]
fn semantic_only_ready_plan(with_effect: bool) -> SemanticOnlyReadyPlan {
    let fixture = Fixture::new();
    let (semantic, event_id, admission_receipt_id, durability_receipt_id) =
        seed_semantic_authority(&fixture.semantic_root);
    let artifact = nlos_artifact::ArtifactStore::open(&fixture.artifact_root).unwrap();
    let task_id = TaskId::from_bytes([0x80; 16]);
    let attempt_id = TaskAttemptId::from_bytes([0x81; 16]);
    let target = NamespaceId::from_bytes([0x40; 16]);
    let attempt = AttemptSpec {
        task_id,
        attempt_id,
        attempt_generation: Generation::INITIAL,
        snapshot: SnapshotBundle {
            snapshot_id: TaskSnapshotId::from_bytes([0x82; 16]),
            snapshot_digest: [0x83; 32],
            expected_head_commit_seq: 0,
            effect_history_root: empty_effect_history_root(),
            retry_fence_epoch: 0,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes([0x84; 16]),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes([0x85; 16]),
        registered_at_ms: 10,
    };
    let task = SqliteTaskAuthority::open(&fixture.task_path).unwrap();
    task.register_task(TaskSpec {
        application_id: None,
        plan_revision: None,
        task_id,
        task_generation: Generation::INITIAL,
        registered_at_ms: 1,
    })
    .unwrap();
    task.register_snapshot_receipt(TaskSnapshotReceiptSpec {
        task_id,
        snapshot: attempt.snapshot,
        receipt_id: ReceiptId::from_bytes([0x86; 16]),
        builder_id: [0x87; 16],
        builder_version_digest: [0x88; 32],
        per_authority_checkpoint_receipts: vec![ReceiptId::from_bytes([0x89; 16])],
        dependency_closure_root: [0x8a; 32],
        semantic_resolver_digest: [0x8b; 32],
        canonical_iteration_digest: [0x8c; 32],
        achieved_consistency: SnapshotConsistency::Causal,
        built_at_ms: 2,
        authority_id: [0x8d; 16],
        key_id: [0x8e; 16],
        signature: [0x8f; 64],
    })
    .unwrap();
    task.register_attempt_with_snapshot_receipt(attempt, ReceiptId::from_bytes([0x86; 16]))
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
    let planned_effects = if with_effect {
        vec![mixed_effect(task_id)]
    } else {
        Vec::new()
    };
    let permit_planned_effects = planned_effects.clone();
    let effect_endpoints = if with_effect {
        vec![TaskWriteSetEffectEndpointRequest::SemanticAdmission { effect_seq: 0 }]
    } else {
        Vec::new()
    };
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
                    expected_admission_policy_digest: [0x58; 32],
                    durability_receipt_id: Some(durability_receipt_id),
                }],
                resource_reservations: Vec::new(),
                planned_effects,
                effect_endpoints,
                idempotency_key: IdempotencyKey::from_bytes([0x90; 16]),
                sealed_at_ms: 4,
            },
        )
        .unwrap()
        .record()
        .clone();
    let permit = match task
        .request_commit_permit_with_authorities_struct(
            Authorities::default(),
            PermitRequest {
                task_id,
                attempt_id,
                attempt_generation: Generation::INITIAL,
                write_set_root: write_set.write_set_root,
                planned_effects: permit_planned_effects,
                idempotency_key: IdempotencyKey::from_bytes([0x91; 16]),
                valid_until_ms: 1_000,
                requested_at_ms: 5,
            },
        )
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
            idempotency_key: IdempotencyKey::from_bytes([0x92; 16]),
            planned_at_ms: 6,
        })
        .unwrap()
        .record()
        .clone();
    assert!(matches!(
        task.authorize_semantic_publication(plan.plan_id, 7)
            .unwrap()
            .record()
            .state,
        SemanticCommitPlanState::Publishing
    ));
    let owner = semantic
        .publish_semantic_publication(PublishSemanticPublicationRequest {
            task_id,
            permit_id: permit.permit_id,
            write_set_root: write_set.write_set_root,
            event_id,
            target: CapabilityTarget::Namespace(target),
            admission_receipt_id,
            durability_receipt_id: Some(durability_receipt_id),
            published_at_ms: 8,
        })
        .unwrap()
        .receipt();
    let progress = task
        .record_semantic_publications(
            &semantic,
            RecordSemanticPublicationsRequest {
                plan_id: plan.plan_id,
                receipts: vec![nested(&owner, target)],
                observed_at_ms: 9,
            },
        )
        .unwrap();
    assert_eq!(progress.plan.state, SemanticCommitPlanState::Ready);
    drop(task);
    SemanticOnlyReadyPlan {
        fixture,
        task_id,
        attempt_id,
        permit,
        plan_id: plan.plan_id,
    }
}

/// W43-E1: the direct Semantic-only finalize must leave the same
/// `task_finalize_proofs` row the unified v3 terminal transaction writes,
/// so a later plain `finalize_commit_v3` replay compares the stored proof
/// digest instead of falling into the legacy caller-supplied-root branch
/// (which false-positives with `IdempotencyConflict` whenever the replaying
/// coordinator computed different root bytes).
#[test]
fn direct_semantic_finalize_replays_through_plain_v3_by_proof_digest() {
    let ready = semantic_only_ready_plan(false);
    let task = SqliteTaskAuthority::open(&ready.fixture.task_path).unwrap();
    let committed = task
        .finalize_semantic_commit(FinalizeSemanticCommitRequest {
            plan_id: ready.plan_id,
            finalized_at_ms: 11,
        })
        .unwrap();
    assert!(matches!(committed, SemanticFinalizeDecision::Committed(_)));
    // The receipt's effect root/fence are the preserved pre-commit values,
    // NOT any caller-recomputed bytes: v3 replay authority is the stored
    // finalize-proof digest.
    let replay_with_foreign_roots = FinalizeRequestV3 {
        base: FinalizeRequest {
            task_id: ready.task_id,
            attempt_id: ready.attempt_id,
            attempt_generation: Generation::INITIAL,
            permit_id: ready.permit.permit_id,
            new_effect_history_root: [0xee; 32],
            new_retry_fence_epoch: 77,
            finalized_at_ms: 99,
        },
        required_satisfaction: Vec::new(),
        fenced_participant_digest: [0; 32],
    };
    match task.finalize_commit_v3(replay_with_foreign_roots) {
        Ok(FinalizeDecision::Replayed(receipt)) => {
            assert_eq!(*receipt, committed.receipt().task_receipt);
        }
        other => panic!("expected plain v3 replay, got {other:?}"),
    }
    // Divergent satisfaction bytes still fail closed, typed by the proof
    // digest comparison (`HistoryConflict`), never as a legacy-root
    // idempotency false positive.
    let divergent = FinalizeRequestV3 {
        base: FinalizeRequest {
            task_id: ready.task_id,
            attempt_id: ready.attempt_id,
            attempt_generation: Generation::INITIAL,
            permit_id: ready.permit.permit_id,
            new_effect_history_root: [0xee; 32],
            new_retry_fence_epoch: 77,
            finalized_at_ms: 99,
        },
        required_satisfaction: vec![RequiredSatisfaction {
            effect_seq: 0,
            proof: RequiredSatisfactionProof::EffectClosedSuccess {
                success_assertion_digest: [0xab; 32],
            },
        }],
        fenced_participant_digest: [0; 32],
    };
    assert!(matches!(
        task.finalize_commit_v3(divergent),
        Err(TaskStoreError::HistoryConflict)
    ));
}

/// W43-E1: the direct Semantic-only finalize carries the `[TASK-COMMIT-003]`
/// adoption scope fence of the effect-plane mint/dispatch entries. Once a
/// permit is quarantined (an `EffectUnknown` slot) and adopted, the direct
/// path must refuse it with the typed scope violation instead of relying on
/// the generic `PermitNotIssued` refusal of the later context checks.
#[test]
fn direct_semantic_finalize_refuses_adopted_permits() {
    let ready = semantic_only_ready_plan(true);
    let task = SqliteTaskAuthority::open(&ready.fixture.task_path).unwrap();
    // Drive the single declared slot into `EffectUnknown` while the plan is
    // already READY.
    let issued = match task
        .request_effect_permit(EffectPermitRequest {
            task_id: ready.task_id,
            attempt_id: ready.attempt_id,
            attempt_generation: Generation::INITIAL,
            permit_id: ready.permit.permit_id,
            permit_epoch: ready.permit.permit_epoch,
            effect_seq: 0,
            idempotency_key: IdempotencyKey::from_bytes([0x93; 16]),
            valid_until_ms: 1_000,
            requested_at_ms: 10,
        })
        .unwrap()
    {
        EffectPermitDecision::Issued(issued) => issued,
        other @ EffectPermitDecision::Replayed(_) => {
            panic!("expected issued effect permit, got {other:?}")
        }
    };
    task.consume_dispatch_token(DispatchRequest {
        task_id: ready.task_id,
        attempt_id: ready.attempt_id,
        attempt_generation: Generation::INITIAL,
        permit_id: ready.permit.permit_id,
        permit_epoch: ready.permit.permit_epoch,
        effect_permit_id: issued.effect_permit_id,
        dispatch_token: issued.one_shot_dispatch_token,
        dispatched_at_ms: 11,
    })
    .unwrap();
    task.record_effect_outcome(OutcomeRequest {
        task_id: ready.task_id,
        attempt_id: ready.attempt_id,
        attempt_generation: Generation::INITIAL,
        permit_id: ready.permit.permit_id,
        permit_epoch: ready.permit.permit_epoch,
        effect_seq: 0,
        outcome: Outcome::Unknown {
            uncertainty_digest: [0xcd; 32],
        },
        recorded_at_ms: 12,
    })
    .unwrap();
    // The v3 terminal path quarantines the permit (`[TASK-EFFECT-003]`).
    assert!(matches!(
        task.finalize_commit_v3(FinalizeRequestV3 {
            base: FinalizeRequest {
                task_id: ready.task_id,
                attempt_id: ready.attempt_id,
                attempt_generation: Generation::INITIAL,
                permit_id: ready.permit.permit_id,
                new_effect_history_root: empty_effect_history_root(),
                new_retry_fence_epoch: 0,
                finalized_at_ms: 13,
            },
            required_satisfaction: Vec::new(),
            fenced_participant_digest: [0; 32],
        }),
        Err(TaskStoreError::Quarantined)
    ));
    let adoption = task
        .adopt_permit(AdoptionRequest {
            task_id: ready.task_id,
            permit_id: ready.permit.permit_id,
            permit_epoch: ready.permit.permit_epoch,
            idempotency_key: IdempotencyKey::from_bytes([0x94; 16]),
            adopted_at_ms: 14,
        })
        .unwrap();
    assert!(matches!(adoption, nlos_task::AdoptionReplay::Adopted(_)));
    // The adopted permit is outside this direct path's scope.
    assert!(matches!(
        task.finalize_semantic_commit(FinalizeSemanticCommitRequest {
            plan_id: ready.plan_id,
            finalized_at_ms: 15,
        }),
        Err(TaskStoreError::AdoptionScopeViolation)
    ));
}
