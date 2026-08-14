use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_task::{
    AttemptSpec, EffectPermitDecision, EffectPermitRequest, FinalizeDecision, FinalizeRequest,
    FinalizeRequestV3, LogicalEffectDescriptor, NoEffectReason, NoEffectRequest, Outcome,
    OutcomeRequest, ParticipantRegistrationDecision, ParticipantRegistryBinding,
    ParticipantRegistryState, ParticipantType, PermitDecision, PermitRequest, PlannedEffect,
    SnapshotBundle, SnapshotConsistency, SqliteTaskAuthority, TaskSnapshotReceiptSpec, TaskSpec,
    TaskStoreError, TaskWriteSetArtifactRead, TaskWriteSetArtifactWriteRequest,
    TaskWriteSetEffectEndpointRequest, TaskWriteSetRequest, TaskWriteSetResourceReservationRequest,
    TaskWriteSetSemanticAppendRequest, TaskWriteSetSemanticRead,
    TaskWriteSetSemanticRequiredDurability, TaskWriteSetSemanticTarget, empty_effect_history_root,
};
use nlos_types::{
    ArtifactId, CallId, CancellationScopeId, Generation, IdempotencyKey, NamespaceId, OperationId,
    SemanticEventId, TaskAttemptId, TaskId, TaskSnapshotId,
};
use rusqlite::Connection;
use sha2::{Digest, Sha256};

static NEXT: AtomicU64 = AtomicU64::new(1);

struct Database(PathBuf);

impl Database {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "nlos-task-participant-{}-{}.sqlite3",
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
            "nlos-task-participant-{label}-{}-{}",
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
            created_at_ms: 1_500,
        })
        .unwrap();
    artifact_id
}

#[test]
fn task_registration_assigns_durable_self_participant_and_receipts() {
    let database = Database::new();
    let participant_id = {
        let authority = database.open();
        authority
            .register_task(TaskSpec {
                task_id: task_id(),
                task_generation: Generation::INITIAL,
                registered_at_ms: 1_000,
            })
            .unwrap();
        let registry = authority.inspect_participant_registry(task_id()).unwrap();
        assert_eq!(registry.generation, 1);
        assert_eq!(registry.state, ParticipantRegistryState::Open);
        assert_eq!(registry.participants.len(), 1);
        assert_eq!(
            registry.participants[0].participant_type,
            ParticipantType::TaskStore
        );
        registry.participants[0].participant_id
    };

    let authority = database.open();
    assert_eq!(
        authority
            .inspect_participant_registry(task_id())
            .unwrap()
            .participants[0]
            .participant_id,
        participant_id
    );
    drop(authority);
    let raw = Connection::open(&database.0).unwrap();
    assert!(raw.execute("DELETE FROM task_participants", []).is_err());
    assert!(
        raw.execute(
            "UPDATE task_authority_identity SET participant_id=zeroblob(16)",
            []
        )
        .is_err()
    );
    assert_eq!(
        raw.query_row(
            "SELECT COUNT(*) FROM task_participant_registry_receipts",
            [],
            |row| { row.get::<_, i64>(0) }
        )
        .unwrap(),
        1
    );
}

#[test]
fn permit_atomically_freezes_and_binds_exact_registry_generation_root() {
    let database = Database::new();
    let binding = {
        let authority = database.open();
        authority
            .register_task(TaskSpec {
                task_id: task_id(),
                task_generation: Generation::INITIAL,
                registered_at_ms: 1_000,
            })
            .unwrap();
        let spec = attempt(0x21, 0, empty_effect_history_root());
        authority.register_attempt(spec).unwrap();
        let request = permit(&spec, 0x31);
        let record = issued(authority.request_commit_permit(request.clone()).unwrap());
        let binding = record.participant_registry_binding.unwrap();
        let registry = authority.inspect_participant_registry(task_id()).unwrap();
        assert_eq!(registry.state, ParticipantRegistryState::FrozenForPermit);
        assert_eq!(binding.generation, registry.generation);
        assert_eq!(binding.root, registry.root);
        match authority.request_commit_permit(request).unwrap() {
            PermitDecision::Replayed(replayed) => {
                assert_eq!(replayed.participant_registry_binding, Some(binding));
            }
            other => panic!("expected replay, got {other:?}"),
        }
        binding
    };

    let authority = database.open();
    assert_eq!(
        authority
            .inspect_participant_registry(task_id())
            .unwrap()
            .root,
        binding.root
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One lifecycle proves issuance, dispatch, finalize, restart.
fn effect_permit_dispatch_and_task_receipt_copy_and_revalidate_registry_binding() {
    let database = Database::new();
    let authority = database.open();
    authority
        .register_task(TaskSpec {
            task_id: task_id(),
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
        })
        .unwrap();
    let spec = attempt(0x32, 0, empty_effect_history_root());
    authority.register_attempt(spec).unwrap();
    let mut commit_request = permit(&spec, 0x33);
    commit_request.planned_effects = vec![planned_effect()];
    let commit_permit = issued(authority.request_commit_permit(commit_request).unwrap());
    let binding = commit_permit.participant_registry_binding.unwrap();
    let effect_permit = match authority
        .request_effect_permit(EffectPermitRequest {
            task_id: task_id(),
            attempt_id: spec.attempt_id,
            attempt_generation: spec.attempt_generation,
            permit_id: commit_permit.permit_id,
            permit_epoch: commit_permit.permit_epoch,
            effect_seq: 0,
            idempotency_key: IdempotencyKey::from_bytes([0xa1; 16]),
            valid_until_ms: 9_000,
            requested_at_ms: 4_000,
        })
        .unwrap()
    {
        EffectPermitDecision::Issued(record) => *record,
        other @ EffectPermitDecision::Replayed(_) => {
            panic!("expected issued effect permit, got {other:?}")
        }
    };
    assert_eq!(effect_permit.participant_registry_binding, Some(binding));

    let raw = Connection::open(&database.0).unwrap();
    raw.execute(
        "UPDATE task_participant_registries SET registry_state=1 WHERE task_id=?1",
        [task_id().as_bytes().as_slice()],
    )
    .unwrap();
    let dispatch = nlos_task::DispatchRequest {
        task_id: task_id(),
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        permit_id: commit_permit.permit_id,
        permit_epoch: commit_permit.permit_epoch,
        effect_permit_id: effect_permit.effect_permit_id,
        dispatch_token: effect_permit.one_shot_dispatch_token,
        dispatched_at_ms: 5_000,
    };
    assert!(matches!(
        authority.consume_dispatch_token(dispatch),
        Err(TaskStoreError::ParticipantRegistryFrozen {
            state: ParticipantRegistryState::Open
        })
    ));
    raw.execute(
        "UPDATE task_participant_registries SET registry_state=2 WHERE task_id=?1",
        [task_id().as_bytes().as_slice()],
    )
    .unwrap();
    authority.consume_dispatch_token(dispatch).unwrap();
    authority
        .record_effect_outcome(OutcomeRequest {
            task_id: task_id(),
            attempt_id: spec.attempt_id,
            attempt_generation: spec.attempt_generation,
            permit_id: commit_permit.permit_id,
            permit_epoch: commit_permit.permit_epoch,
            effect_seq: 0,
            outcome: Outcome::Closed {
                authoritative_closure_digest: [0xa2; 32],
            },
            recorded_at_ms: 6_000,
        })
        .unwrap();

    raw.execute(
        "UPDATE task_participant_registries SET registry_state=1 WHERE task_id=?1",
        [task_id().as_bytes().as_slice()],
    )
    .unwrap();
    let finalize = FinalizeRequest {
        task_id: task_id(),
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        permit_id: commit_permit.permit_id,
        new_effect_history_root: [0xa3; 32],
        new_retry_fence_epoch: 0,
        finalized_at_ms: 7_000,
    };
    assert!(matches!(
        authority.finalize_commit(finalize),
        Err(TaskStoreError::ParticipantRegistryFrozen {
            state: ParticipantRegistryState::Open
        })
    ));
    raw.execute(
        "UPDATE task_participant_registries SET registry_state=2 WHERE task_id=?1",
        [task_id().as_bytes().as_slice()],
    )
    .unwrap();
    let receipt = match authority.finalize_commit(finalize).unwrap() {
        FinalizeDecision::Committed(receipt) => *receipt,
        other @ FinalizeDecision::Replayed(_) => {
            panic!("expected committed receipt, got {other:?}")
        }
    };
    assert_eq!(receipt.participant_registry_binding, Some(binding));
    drop(raw);
    drop(authority);

    let reopened = database.open();
    assert_eq!(
        reopened
            .inspect_effect_permit(task_id(), effect_permit.effect_permit_id)
            .unwrap()
            .participant_registry_binding,
        Some(binding)
    );
    assert_eq!(
        reopened
            .inspect_receipt(task_id(), receipt.receipt_id)
            .unwrap()
            .participant_registry_binding,
        Some(binding)
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn verified_write_set_seal_binds_receipted_snapshot_and_artifact_reads() {
    let database = Database::new();
    let artifact_root = AuthorityRoot::new("write-set-artifact");
    let artifact = nlos_artifact::ArtifactStore::open(&artifact_root.0).unwrap();
    let artifact_id = create_artifact(&artifact, 0xb1);
    let process_root = AuthorityRoot::new("write-set-process");
    let process = nlos_process::ProcessAuthority::open(&process_root.0).unwrap();
    let authority = database.open();
    authority
        .register_task(TaskSpec {
            task_id: task_id(),
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
        })
        .unwrap();
    let spec = attempt(0x62, 0, empty_effect_history_root());
    let receipt_id = nlos_types::ReceiptId::from_bytes([0xb2; 16]);
    authority
        .register_snapshot_receipt(nlos_task::TaskSnapshotReceiptSpec {
            task_id: task_id(),
            snapshot: spec.snapshot,
            receipt_id,
            builder_id: [0xb3; 16],
            builder_version_digest: [0xb4; 32],
            per_authority_checkpoint_receipts: vec![nlos_types::ReceiptId::from_bytes([0xb5; 16])],
            dependency_closure_root: [0xb6; 32],
            semantic_resolver_digest: [0xb7; 32],
            canonical_iteration_digest: [0xb8; 32],
            achieved_consistency: SnapshotConsistency::Causal,
            built_at_ms: 1_100,
            authority_id: [0xb9; 16],
            key_id: [0xba; 16],
            signature: [0xbb; 64],
        })
        .unwrap();
    authority
        .register_attempt_with_snapshot_receipt(spec, receipt_id)
        .unwrap();
    let isolation_domain = process
        .create_isolation_domain(nlos_process::CreateIsolationDomainRequest {
            policy_digest: [0xc0; 32],
            idempotency_key: IdempotencyKey::from_bytes([0xc1; 16]),
            created_at_ms: 1_150,
        })
        .unwrap()
        .record()
        .clone();
    let process_binding = process
        .register_delegated_process(nlos_process::RegisterDelegatedProcessRequest {
            task_id: task_id(),
            task_attempt_id: spec.attempt_id,
            attempt_generation: spec.attempt_generation,
            isolation_domain_id: isolation_domain.isolation_domain_id,
            isolation_domain_generation: isolation_domain.generation,
            isolation_domain_fencing_token: isolation_domain.fencing_token,
            idempotency_key: IdempotencyKey::from_bytes([0xc2; 16]),
            created_at_ms: 1_160,
        })
        .unwrap()
        .record()
        .clone();
    let active_process = process
        .inspect_active_process_binding(process_binding.process_id)
        .unwrap();
    let registry_binding = binding(&authority.inspect_participant_registry(task_id()).unwrap());
    authority
        .register_process_binding_participant(
            &process,
            task_id(),
            registry_binding,
            spec.attempt_id,
            spec.attempt_generation,
            active_process.process_id,
            active_process.process_generation,
            1_170,
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
        process_binding: Some(nlos_process::ActiveProcessBinding::from(&active_process).into()),
        semantic_reads: Vec::new(),
        semantic_appends: Vec::new(),
        resource_reservations: Vec::new(),
        planned_effects: vec![planned_effect()],
        effect_endpoints: vec![TaskWriteSetEffectEndpointRequest::ProcessBinding {
            effect_seq: 0,
            process_id: active_process.process_id,
            expected_process_generation: active_process.process_generation,
        }],
        idempotency_key: IdempotencyKey::from_bytes([0xbc; 16]),
        sealed_at_ms: 1_200,
    };
    let record = match authority
        .seal_task_write_set_with_process_authority(&artifact, &process, request.clone())
        .unwrap()
    {
        nlos_task::TaskWriteSetDecision::Sealed(record) => record,
        nlos_task::TaskWriteSetDecision::Replayed(_) => panic!("expected new write-set seal"),
    };
    assert_eq!(record.snapshot_receipt_id, receipt_id);
    assert_eq!(record.artifact_reads, request.artifact_reads);
    assert_eq!(
        record.process_binding.unwrap().process_id,
        active_process.process_id
    );
    assert_eq!(record.group_binding, None);
    assert_eq!(
        record.participant_registry_binding,
        binding(&authority.inspect_participant_registry(task_id()).unwrap())
    );
    assert!(matches!(
        authority.seal_task_write_set_with_process_authority(
            &artifact,
            &process,
            TaskWriteSetRequest {
                artifact_reads: vec![TaskWriteSetArtifactRead {
                    artifact_id,
                    expected_head_revision: 1,
                    expected_head_digest: None,
                }],
                ..request.clone()
            }
        ),
        Err(TaskStoreError::TaskWriteSetReadConflict)
    ));
    match authority
        .seal_task_write_set_with_process_authority(&artifact, &process, request)
        .unwrap()
    {
        nlos_task::TaskWriteSetDecision::Replayed(replayed) => assert_eq!(replayed, record),
        nlos_task::TaskWriteSetDecision::Sealed(_) => panic!("expected replayed write-set seal"),
    }
    let mut permit_request = permit(&spec, 0xbd);
    permit_request.write_set_root = record.write_set_root;
    permit_request.planned_effects = record.planned_effects.clone();
    let wrong_process_root = AuthorityRoot::new("wrong-process-permit");
    let wrong_process = nlos_process::ProcessAuthority::open(&wrong_process_root.0).unwrap();
    assert!(matches!(
        authority
            .request_commit_permit_with_process_authority(&wrong_process, permit_request.clone(),),
        Err(TaskStoreError::ProcessParticipantAuthority(_))
    ));
    let permit_record = issued(
        authority
            .request_commit_permit_with_process_authority(&process, permit_request.clone())
            .unwrap(),
    );
    assert_eq!(permit_record.write_set_root, record.write_set_root);
    assert!(matches!(
        authority
            .request_commit_permit_with_process_authority(&process, permit_request)
            .unwrap(),
        PermitDecision::Replayed(_)
    ));
    assert_eq!(
        authority
            .inspect_participant_registry(task_id())
            .unwrap()
            .state,
        ParticipantRegistryState::FrozenForPermit
    );
    drop(authority);
    let reopened = database.open();
    assert_eq!(
        reopened
            .inspect_task_write_set(task_id(), IdempotencyKey::from_bytes([0xbc; 16]))
            .unwrap(),
        record
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn verified_write_set_seal_binds_planned_effects_to_permit_and_replays_after_restart() {
    let database = Database::new();
    let artifact_root = AuthorityRoot::new("write-set-effect-artifact");
    let artifact = nlos_artifact::ArtifactStore::open(&artifact_root.0).unwrap();
    let artifact_id = create_artifact(&artifact, 0xf8);
    let authority = database.open();
    authority
        .register_task(TaskSpec {
            task_id: task_id(),
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
        })
        .unwrap();
    let spec = attempt(0x91, 0, empty_effect_history_root());
    let receipt_id = nlos_types::ReceiptId::from_bytes([0xf9; 16]);
    authority
        .register_snapshot_receipt(nlos_task::TaskSnapshotReceiptSpec {
            task_id: task_id(),
            snapshot: spec.snapshot,
            receipt_id,
            builder_id: [0xfa; 16],
            builder_version_digest: [0xfb; 32],
            per_authority_checkpoint_receipts: vec![nlos_types::ReceiptId::from_bytes([0xfc; 16])],
            dependency_closure_root: [0xfd; 32],
            semantic_resolver_digest: [0xfe; 32],
            canonical_iteration_digest: [0xff; 32],
            achieved_consistency: SnapshotConsistency::Causal,
            built_at_ms: 1_050,
            authority_id: [0x81; 16],
            key_id: [0x82; 16],
            signature: [0x83; 64],
        })
        .unwrap();
    authority
        .register_attempt_with_snapshot_receipt(spec, receipt_id)
        .unwrap();
    let registry_binding = binding(&authority.inspect_participant_registry(task_id()).unwrap());
    authority
        .register_artifact_head_participant(
            &artifact,
            task_id(),
            registry_binding,
            artifact_id,
            1_100,
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
        effect_endpoints: vec![TaskWriteSetEffectEndpointRequest::ArtifactHead {
            effect_seq: 0,
            artifact_id,
        }],
        idempotency_key: IdempotencyKey::from_bytes([0x84; 16]),
        sealed_at_ms: 1_200,
    };
    let record = authority
        .seal_task_write_set(&artifact, request.clone())
        .unwrap()
        .record()
        .clone();
    assert_eq!(record.planned_effects, request.planned_effects);
    assert_ne!(record.effect_set_root, [0; 32]);
    assert_eq!(record.effect_endpoints.len(), 1);
    assert_ne!(record.effect_endpoint_set_root, [0; 32]);
    let mut permit_request = permit(&spec, 0x85);
    permit_request.write_set_root = record.write_set_root;
    permit_request.planned_effects = request.planned_effects.clone();
    let mut conflicting_permit_request = permit_request.clone();
    conflicting_permit_request.idempotency_key = IdempotencyKey::from_bytes([0x87; 16]);
    conflicting_permit_request.planned_effects[0].action_proposal_digest = [0x86; 32];
    assert!(matches!(
        authority.request_commit_permit(conflicting_permit_request),
        Err(TaskStoreError::TaskWriteSetConflict { .. })
    ));
    let permit_record = issued(
        authority
            .request_commit_permit(permit_request.clone())
            .unwrap(),
    );
    assert_eq!(
        authority
            .inspect_effect_set(permit_record.permit_id)
            .unwrap()
            .unwrap()
            .effect_set_root,
        record.effect_set_root
    );
    assert_eq!(
        authority
            .list_effect_slots(permit_record.permit_id)
            .unwrap()
            .len(),
        1
    );
    drop(authority);
    let raw = Connection::open(&database.0).unwrap();
    assert!(
        raw.execute(
            "UPDATE task_write_set_planned_effects
             SET action_proposal_digest = zeroblob(32)
             WHERE task_id = ?1 AND idempotency_key = ?2 AND effect_seq = 0",
            rusqlite::params![
                task_id().as_bytes().as_slice(),
                request.idempotency_key.as_bytes().as_slice()
            ],
        )
        .is_err()
    );
    assert!(
        raw.execute(
            "UPDATE task_write_set_effect_endpoints
             SET participant_id = zeroblob(16)
             WHERE task_id = ?1 AND idempotency_key = ?2 AND endpoint_seq = 0",
            rusqlite::params![
                task_id().as_bytes().as_slice(),
                request.idempotency_key.as_bytes().as_slice()
            ],
        )
        .is_err()
    );
    assert!(
        raw.execute(
            "DELETE FROM task_write_set_effect_endpoints
             WHERE task_id = ?1 AND idempotency_key = ?2 AND endpoint_seq = 0",
            rusqlite::params![
                task_id().as_bytes().as_slice(),
                request.idempotency_key.as_bytes().as_slice()
            ],
        )
        .is_err()
    );
    assert!(
        raw.execute(
            "DELETE FROM task_write_set_planned_effects
             WHERE task_id = ?1 AND idempotency_key = ?2 AND effect_seq = 0",
            rusqlite::params![
                task_id().as_bytes().as_slice(),
                request.idempotency_key.as_bytes().as_slice()
            ],
        )
        .is_err()
    );
    drop(raw);
    let reopened = database.open();
    assert_eq!(
        reopened
            .inspect_task_write_set(task_id(), request.idempotency_key)
            .unwrap(),
        record
    );
    let mut replay_conflict = permit_request;
    replay_conflict.planned_effects[0].action_proposal_digest = [0x86; 32];
    assert!(matches!(
        reopened.request_commit_permit(PermitRequest {
            planned_effects: replay_conflict.planned_effects,
            ..replay_conflict
        }),
        Err(TaskStoreError::IdempotencyConflict)
    ));
}

#[test]
#[allow(clippy::too_many_lines)]
fn artifact_write_declaration_binds_post_permit_publication_plan() {
    let database = Database::new();
    let artifact_root = AuthorityRoot::new("artifact-write-plan-artifact");
    let artifact = nlos_artifact::ArtifactStore::open(&artifact_root.0).unwrap();
    let artifact_id = create_artifact(&artifact, 0xa1);
    let authority = database.open();
    authority
        .register_task(TaskSpec {
            task_id: task_id(),
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
        })
        .unwrap();
    let spec = attempt(0xa2, 0, empty_effect_history_root());
    let snapshot_receipt_id = nlos_types::ReceiptId::from_bytes([0xa3; 16]);
    authority
        .register_snapshot_receipt(TaskSnapshotReceiptSpec {
            task_id: task_id(),
            snapshot: spec.snapshot,
            receipt_id: snapshot_receipt_id,
            builder_id: [0xa4; 16],
            builder_version_digest: [0xa5; 32],
            per_authority_checkpoint_receipts: vec![nlos_types::ReceiptId::from_bytes([0xa6; 16])],
            dependency_closure_root: [0xa7; 32],
            semantic_resolver_digest: [0xa8; 32],
            canonical_iteration_digest: [0xa9; 32],
            achieved_consistency: SnapshotConsistency::Causal,
            built_at_ms: 1_100,
            authority_id: [0xaa; 16],
            key_id: [0xab; 16],
            signature: [0xac; 64],
        })
        .unwrap();
    authority
        .register_attempt_with_snapshot_receipt(spec, snapshot_receipt_id)
        .unwrap();
    let registry_binding = binding(&authority.inspect_participant_registry(task_id()).unwrap());
    authority
        .register_artifact_head_participant(
            &artifact,
            task_id(),
            registry_binding,
            artifact_id,
            1_120,
        )
        .unwrap();

    let payload = b"c2b-artifact";
    let content_digest = nlos_artifact::ContentDigest::of_bytes(payload).into_bytes();
    let staging_key = IdempotencyKey::from_bytes([0xad; 16]);
    let request = TaskWriteSetRequest {
        task_id: task_id(),
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        artifact_reads: Vec::new(),
        artifact_writes: vec![TaskWriteSetArtifactWriteRequest {
            artifact_id,
            expected_head_revision: 0,
            proposed_revision: 1,
            content_digest,
            size_bytes: payload.len() as u64,
        }],
        process_binding: None,
        semantic_reads: Vec::new(),
        semantic_appends: Vec::new(),
        resource_reservations: Vec::new(),
        planned_effects: vec![planned_effect()],
        effect_endpoints: vec![TaskWriteSetEffectEndpointRequest::ArtifactHead {
            effect_seq: 0,
            artifact_id,
        }],
        idempotency_key: IdempotencyKey::from_bytes([0xae; 16]),
        sealed_at_ms: 1_130,
    };
    let record = authority
        .seal_task_write_set(&artifact, request.clone())
        .unwrap()
        .record()
        .clone();

    let mut permit_request = permit(&spec, 0xaf);
    permit_request.write_set_root = record.write_set_root;
    permit_request.planned_effects = request.planned_effects.clone();
    let wrong_artifact_root = AuthorityRoot::new("wrong-artifact-permit");
    let wrong_artifact = nlos_artifact::ArtifactStore::open(&wrong_artifact_root.0).unwrap();
    assert!(matches!(
        authority.request_commit_permit_with_artifact_authority(
            &wrong_artifact,
            permit_request.clone(),
        ),
        Err(TaskStoreError::ArtifactParticipantAuthority(_))
    ));
    let permit_record = issued(
        authority
            .request_commit_permit_with_artifact_authority(&artifact, permit_request.clone())
            .unwrap(),
    );
    assert!(matches!(
        authority
            .request_commit_permit_with_artifact_authority(&artifact, permit_request)
            .unwrap(),
        PermitDecision::Replayed(_)
    ));
    let expectation = nlos_task::ArtifactPublicationExpectation {
        staging_id: nlos_artifact::staging_id_for(artifact_id, staging_key).into_bytes(),
        artifact_id,
        target_revision: 1,
        digest: content_digest,
        size_bytes: payload.len() as u64,
    };
    let plan_request = |expectations| nlos_task::PlanArtifactCommitRequest {
        task_id: task_id(),
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        permit_id: permit_record.permit_id,
        expectations,
        idempotency_key: IdempotencyKey::from_bytes([0xb0; 16]),
        planned_at_ms: 1_140,
    };
    let mut mismatch = expectation;
    mismatch.digest = [0xff; 32];
    assert!(matches!(
        authority.plan_artifact_commit(plan_request(vec![mismatch])),
        Err(TaskStoreError::InvalidArtifactPublicationPlan { .. })
    ));
    let plan = authority
        .plan_artifact_commit(plan_request(vec![expectation]))
        .unwrap()
        .record()
        .clone();
    assert_eq!(plan.write_set_root, record.write_set_root);
    assert_ne!(
        plan.write_set_root,
        nlos_task::artifact_publication_plan_root(&[expectation]).unwrap()
    );

    let staged = artifact
        .stage_revision(nlos_artifact::StageRevisionRequest {
            artifact_id,
            expected_head_revision: 0,
            bytes: payload,
            task_id: task_id(),
            permit_id: permit_record.permit_id,
            write_set_root: nlos_artifact::ContentDigest::from_bytes(record.write_set_root),
            idempotency_key: staging_key,
            created_at_ms: 1_150,
        })
        .unwrap()
        .record()
        .clone();
    assert_eq!(staged.staging_id.into_bytes(), expectation.staging_id);
    assert!(matches!(
        authority
            .authorize_artifact_publication(plan.plan_id, 1_160)
            .unwrap(),
        nlos_task::ArtifactPublicationAuthorizationDecision::Authorized(_)
    ));
}

#[test]
#[allow(clippy::too_many_lines)]
fn verified_write_set_seal_binds_reserved_resource_owner_facts() {
    let database = Database::new();
    let artifact_root = AuthorityRoot::new("resource-write-set-artifact");
    let artifact = nlos_artifact::ArtifactStore::open(&artifact_root.0).unwrap();
    let artifact_id = create_artifact(&artifact, 0xd1);
    let resource_root = AuthorityRoot::new("write-set-resource");
    let resource = nlos_resource::ResourceAuthority::open(&resource_root.0).unwrap();
    let driver = resource
        .register_driver(nlos_resource::RegisterDriverRequest {
            profile_digest: [0xd2; 32],
            idempotency_key: IdempotencyKey::from_bytes([0xd3; 16]),
            created_at_ms: 1_000,
        })
        .unwrap()
        .record();
    let account = resource
        .create_account(nlos_resource::CreateAccountRequest {
            initial_credit: 100,
            idempotency_key: IdempotencyKey::from_bytes([0xd4; 16]),
            created_at_ms: 1_000,
        })
        .unwrap();
    let quote = resource
        .create_quote(nlos_resource::CreateQuoteRequest {
            driver_id: driver.driver_id,
            driver_generation: driver.generation,
            driver_fencing_token: driver.fencing_token,
            operation_proposal_digest: [0xd5; 32],
            pricing_version: [0xd6; 32],
            upper_bound: 25,
            valid_until_ms: 9_000,
            idempotency_key: IdempotencyKey::from_bytes([0xd7; 16]),
            created_at_ms: 1_000,
        })
        .unwrap()
        .record();
    let reservation = resource
        .reserve(nlos_resource::ReserveRequest {
            account_id: account.account_id,
            quote_id: quote.quote_id,
            call_id: CallId::from_bytes([0xd8; 16]),
            operation_id: OperationId::from_bytes([0xd9; 16]),
            idempotency_key: IdempotencyKey::from_bytes([0xda; 16]),
            reserved_at_ms: 1_100,
        })
        .unwrap()
        .record();
    let authority = database.open();
    authority
        .register_task(TaskSpec {
            task_id: task_id(),
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
        })
        .unwrap();
    let spec = attempt(0x73, 0, empty_effect_history_root());
    let receipt_id = nlos_types::ReceiptId::from_bytes([0xdb; 16]);
    authority
        .register_snapshot_receipt(nlos_task::TaskSnapshotReceiptSpec {
            task_id: task_id(),
            snapshot: spec.snapshot,
            receipt_id,
            builder_id: [0xdc; 16],
            builder_version_digest: [0xdd; 32],
            per_authority_checkpoint_receipts: vec![nlos_types::ReceiptId::from_bytes([0xe5; 16])],
            dependency_closure_root: [0xde; 32],
            semantic_resolver_digest: [0xdf; 32],
            canonical_iteration_digest: [0xe0; 32],
            achieved_consistency: SnapshotConsistency::Causal,
            built_at_ms: 1_050,
            authority_id: [0xe1; 16],
            key_id: [0xe2; 16],
            signature: [0xe3; 64],
        })
        .unwrap();
    authority
        .register_attempt_with_snapshot_receipt(spec, receipt_id)
        .unwrap();
    let first_binding = binding(&authority.inspect_participant_registry(task_id()).unwrap());
    let driver_registration = authority
        .register_driver_gateway_participant(
            &resource,
            task_id(),
            first_binding,
            driver.driver_id,
            driver.generation,
            1_150,
        )
        .unwrap();
    let second_binding = binding(driver_registration.registry());
    authority
        .register_resource_ledger_participant(
            &resource,
            task_id(),
            second_binding,
            account.account_id,
            Generation::INITIAL,
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
        resource_reservations: vec![TaskWriteSetResourceReservationRequest {
            reservation_id: reservation.reservation_id,
            expected_call_id: reservation.call_id,
            expected_operation_id: reservation.operation_id,
            expected_quote_id: reservation.quote_id,
        }],
        planned_effects: vec![planned_effect()],
        effect_endpoints: vec![
            TaskWriteSetEffectEndpointRequest::DriverGateway {
                effect_seq: 0,
                driver_id: driver.driver_id,
                expected_driver_generation: driver.generation,
            },
            TaskWriteSetEffectEndpointRequest::ResourceLedger {
                effect_seq: 0,
                account_id: account.account_id,
                expected_account_generation: Generation::INITIAL,
            },
        ],
        idempotency_key: IdempotencyKey::from_bytes([0xe4; 16]),
        sealed_at_ms: 1_200,
    };
    let record = authority
        .seal_task_write_set_with_resource_authority(&artifact, &resource, request.clone())
        .unwrap()
        .record()
        .clone();
    assert_eq!(record.resource_reservations.len(), 1);
    assert_eq!(
        record.resource_reservations[0].reservation_id,
        reservation.reservation_id
    );
    let mut conflict = request.clone();
    conflict.resource_reservations[0].expected_operation_id = OperationId::from_bytes([0xee; 16]);
    assert!(matches!(
        authority.seal_task_write_set_with_resource_authority(&artifact, &resource, conflict),
        Err(TaskStoreError::TaskWriteSetResourceReservationConflict)
    ));
    let mut permit_request = permit(&spec, 0xeb);
    permit_request.write_set_root = record.write_set_root;
    permit_request.planned_effects = request.planned_effects.clone();
    let wrong_resource_root = AuthorityRoot::new("wrong-resource-permit");
    let wrong_resource = nlos_resource::ResourceAuthority::open(&wrong_resource_root.0).unwrap();
    assert!(matches!(
        authority.request_commit_permit_with_resource_authority(
            &wrong_resource,
            permit_request.clone(),
        ),
        Err(TaskStoreError::ResourceParticipantAuthority(_))
    ));
    let permit_record = issued(
        authority
            .request_commit_permit_with_resource_authority(&resource, permit_request.clone())
            .unwrap(),
    );
    assert_eq!(permit_record.write_set_root, record.write_set_root);
    assert!(matches!(
        authority
            .request_commit_permit_with_resource_authority(&resource, permit_request)
            .unwrap(),
        PermitDecision::Replayed(_)
    ));
    drop(authority);
    let reopened = database.open();
    assert_eq!(
        reopened
            .inspect_task_write_set(task_id(), IdempotencyKey::from_bytes([0xe4; 16]))
            .unwrap(),
        record
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn verified_write_set_seal_binds_semantic_event_readback_and_append() {
    let database = Database::new();
    let artifact_root = AuthorityRoot::new("semantic-write-set-artifact");
    let artifact = nlos_artifact::ArtifactStore::open(&artifact_root.0).unwrap();
    let semantic_root = AuthorityRoot::new("write-set-semantic");
    let semantic = nlos_semantic::SemanticAuthority::open(&semantic_root.0).unwrap();
    let event_id = SemanticEventId::from_bytes([0xe6; 32]);
    let canonical = vec![0x01, 0x02, 0x03, 0x04];
    let content_digest: [u8; 32] = [0xe7; 32];
    let seed_bytes: &[u8] = b"seed";
    let raw = Connection::open(semantic_root.0.join("semantic-authority.db")).unwrap();
    raw.execute(
        "INSERT INTO content_objects (content_digest, media_type, exact_bytes)
         VALUES (?1, ?2, ?3)",
        rusqlite::params![content_digest.as_slice(), "text/plain", seed_bytes],
    )
    .unwrap();
    raw.execute(
        "INSERT INTO semantic_events (
            event_id, canonical_unsigned_event, event_type, scope_kind, scope_id,
            issuer_principal_id, issuer_process_id, issuer_process_generation,
            control_domain_id, issued_at_unix_ns, valid_until_ms, purpose_digest,
            key_id, content_digest
         ) VALUES (?1, ?2, 1, 1, ?3, ?4, ?5, 1, ?6, 1, NULL, NULL, ?7, ?8)",
        rusqlite::params![
            event_id.as_bytes().as_slice(),
            canonical.as_slice(),
            [0xe8u8; 16].as_slice(),
            [0xe9u8; 16].as_slice(),
            [0xeau8; 16].as_slice(),
            [0xebu8; 16].as_slice(),
            [0xecu8; 16].as_slice(),
            content_digest.as_slice(),
        ],
    )
    .unwrap();
    raw.execute(
        "INSERT INTO event_log (event_id) VALUES (?1)",
        [event_id.as_bytes().as_slice()],
    )
    .unwrap();
    raw.execute(
        "INSERT INTO admission_receipts (
            receipt_id, event_id, log_seq, admitted_at_ms, effective_valid_until_ms,
            effective_taint, authz_policy_digest, durability, store_principal_id,
            store_control_domain_id, store_key_id, store_signature
         ) VALUES (?1, ?2, 1, ?3, NULL, 0, ?4, 2, ?5, ?6, ?7, ?8)",
        rusqlite::params![
            [0xedu8; 16].as_slice(),
            event_id.as_bytes().as_slice(),
            1_040i64,
            [0xddu8; 32].as_slice(),
            [0xdeu8; 16].as_slice(),
            [0xdfu8; 16].as_slice(),
            [0xe0u8; 16].as_slice(),
            [0xe1u8; 64].as_slice(),
        ],
    )
    .unwrap();
    raw.execute(
        "INSERT INTO durability_receipts (
            receipt_id, event_id, durable_checkpoint_id, durable_at_ms, store_signature
         ) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            [0xf1u8; 16].as_slice(),
            event_id.as_bytes().as_slice(),
            [0xf2u8; 32].as_slice(),
            1_060i64,
            [0xf3u8; 64].as_slice(),
        ],
    )
    .unwrap();
    drop(raw);
    let endpoint = semantic.inspect_admission_endpoint_proof().unwrap();
    let authority = database.open();
    authority
        .register_task(TaskSpec {
            task_id: task_id(),
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
        })
        .unwrap();
    let spec = attempt(0x84, 0, empty_effect_history_root());
    let receipt_id = nlos_types::ReceiptId::from_bytes([0xed; 16]);
    authority
        .register_snapshot_receipt(nlos_task::TaskSnapshotReceiptSpec {
            task_id: task_id(),
            snapshot: spec.snapshot,
            receipt_id,
            builder_id: [0xee; 16],
            builder_version_digest: [0xef; 32],
            per_authority_checkpoint_receipts: vec![nlos_types::ReceiptId::from_bytes([0xf0; 16])],
            dependency_closure_root: [0xf1; 32],
            semantic_resolver_digest: [0xf2; 32],
            canonical_iteration_digest: [0xf3; 32],
            achieved_consistency: SnapshotConsistency::Causal,
            built_at_ms: 1_050,
            authority_id: [0xf4; 16],
            key_id: [0xf5; 16],
            signature: [0xf6; 64],
        })
        .unwrap();
    authority
        .register_attempt_with_snapshot_receipt(spec, receipt_id)
        .unwrap();
    let registry_binding = binding(&authority.inspect_participant_registry(task_id()).unwrap());
    authority
        .register_semantic_admission_participant(&semantic, task_id(), registry_binding, 1_100)
        .unwrap();
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/task-write-set-semantic-event/v1");
    hasher.update(&canonical);
    let request = TaskWriteSetRequest {
        task_id: task_id(),
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        artifact_reads: Vec::new(),
        artifact_writes: Vec::new(),
        process_binding: None,
        semantic_reads: vec![TaskWriteSetSemanticRead {
            event_id,
            expected_log_seq: 1,
            expected_canonical_digest: hasher.finalize().into(),
        }],
        semantic_appends: vec![TaskWriteSetSemanticAppendRequest {
            event_id,
            target: TaskWriteSetSemanticTarget::Namespace(NamespaceId::from_bytes([0xe8; 16])),
            required_durability: TaskWriteSetSemanticRequiredDurability::Durable,
            expected_admission_policy_digest: [0xdd; 32],
            durability_receipt_id: Some(nlos_types::ReceiptId::from_bytes([0xf1; 16])),
        }],
        resource_reservations: Vec::new(),
        planned_effects: vec![planned_effect()],
        effect_endpoints: vec![TaskWriteSetEffectEndpointRequest::SemanticAdmission {
            effect_seq: 0,
        }],
        idempotency_key: IdempotencyKey::from_bytes([0xf7; 16]),
        sealed_at_ms: 1_200,
    };
    let record = authority
        .seal_task_write_set_with_semantic_authority(&artifact, &semantic, request.clone())
        .unwrap()
        .record()
        .clone();
    assert_eq!(record.semantic_reads, request.semantic_reads);
    assert_eq!(record.semantic_appends.len(), 1);
    assert_eq!(
        record.semantic_appends[0].admission_receipt_id,
        nlos_types::ReceiptId::from_bytes([0xed; 16])
    );
    assert_eq!(
        record.semantic_appends[0].admission_policy_digest,
        Some([0xdd; 32])
    );
    assert_eq!(
        record.semantic_appends[0].durability_receipt_id,
        Some(nlos_types::ReceiptId::from_bytes([0xf1; 16]))
    );
    assert_ne!(record.semantic_append_set_root, [0; 32]);
    assert_eq!(endpoint.participant_generation.get(), 1);
    match authority
        .seal_task_write_set_with_semantic_authority(&artifact, &semantic, request.clone())
        .unwrap()
    {
        nlos_task::TaskWriteSetDecision::Replayed(replayed) => assert_eq!(replayed, record),
        nlos_task::TaskWriteSetDecision::Sealed(_) => panic!("expected Semantic append replay"),
    }
    let mut durability_conflict = request.clone();
    durability_conflict.semantic_appends[0].durability_receipt_id =
        Some(nlos_types::ReceiptId::from_bytes([0xf4; 16]));
    assert!(matches!(
        authority.seal_task_write_set_with_semantic_authority(
            &artifact,
            &semantic,
            durability_conflict,
        ),
        Err(TaskStoreError::SemanticParticipantAuthority(_))
    ));
    let mut admission_policy_conflict = request.clone();
    admission_policy_conflict.semantic_appends[0].expected_admission_policy_digest = [0xdcu8; 32];
    assert!(matches!(
        authority.seal_task_write_set_with_semantic_authority(
            &artifact,
            &semantic,
            admission_policy_conflict,
        ),
        Err(TaskStoreError::TaskWriteSetConflict {
            reason: "Semantic append admission policy differs from owner receipt",
        })
    ));
    let mut target_conflict = request.clone();
    target_conflict.semantic_appends[0].target =
        TaskWriteSetSemanticTarget::Namespace(NamespaceId::from_bytes([0xe9; 16]));
    assert!(matches!(
        authority.seal_task_write_set_with_semantic_authority(
            &artifact,
            &semantic,
            target_conflict,
        ),
        Err(TaskStoreError::TaskWriteSetConflict {
            reason: "Semantic append target scope differs from admitted event",
        })
    ));
    let mut permit_request = permit(&spec, 0xf8);
    permit_request.write_set_root = record.write_set_root;
    permit_request.planned_effects = request.planned_effects.clone();
    let permit_record = issued(authority.request_commit_permit(permit_request).unwrap());
    assert_eq!(permit_record.write_set_root, record.write_set_root);
    let mut conflict = request;
    conflict.semantic_reads[0].expected_log_seq = 2;
    assert!(matches!(
        authority.seal_task_write_set_with_semantic_authority(&artifact, &semantic, conflict),
        Err(TaskStoreError::TaskWriteSetSemanticReadConflict)
    ));
    authority
        .record_no_effect(NoEffectRequest {
            task_id: task_id(),
            attempt_id: spec.attempt_id,
            attempt_generation: spec.attempt_generation,
            permit_id: permit_record.permit_id,
            permit_epoch: permit_record.permit_epoch,
            effect_seq: 0,
            reason: NoEffectReason::NotSelected,
            dispatch_token: None,
            recorded_at_ms: 1_250,
        })
        .unwrap();
    let finalize_request = FinalizeRequestV3 {
        base: FinalizeRequest {
            task_id: task_id(),
            attempt_id: spec.attempt_id,
            attempt_generation: spec.attempt_generation,
            permit_id: permit_record.permit_id,
            new_effect_history_root: empty_effect_history_root(),
            new_retry_fence_epoch: 0,
            finalized_at_ms: 1_300,
        },
        required_satisfaction: Vec::new(),
        fenced_participant_digest: [0; 32],
    };
    let wrong_semantic_root = AuthorityRoot::new("wrong-semantic-finalize");
    let wrong_semantic = nlos_semantic::SemanticAuthority::open(&wrong_semantic_root.0).unwrap();
    assert!(matches!(
        authority
            .finalize_commit_v3_with_semantic_authority(&wrong_semantic, finalize_request.clone(),),
        Err(TaskStoreError::SemanticParticipantAuthority(_))
    ));
    assert_eq!(
        authority
            .inspect_permit(task_id(), permit_record.permit_id)
            .unwrap()
            .state,
        nlos_task::PermitState::Issued
    );
    match authority
        .finalize_commit_v3_with_semantic_authority(&semantic, finalize_request.clone())
        .unwrap()
    {
        FinalizeDecision::Committed(receipt) => {
            assert_eq!(receipt.outcome, nlos_task::ReceiptOutcome::Committed);
        }
        other @ FinalizeDecision::Replayed(_) => {
            panic!("expected Semantic-guarded commit, got {other:?}")
        }
    }
    assert!(matches!(
        authority
            .finalize_commit_v3_with_semantic_authority(&semantic, finalize_request)
            .unwrap(),
        FinalizeDecision::Replayed(_)
    ));
    drop(authority);
    let reopened = database.open();
    assert_eq!(
        reopened
            .inspect_task_write_set(task_id(), IdempotencyKey::from_bytes([0xf7; 16]))
            .unwrap(),
        record
    );
}

#[test]
fn next_competition_creates_new_registry_generation_instead_of_unfreezing_old() {
    let database = Database::new();
    let authority = database.open();
    authority
        .register_task(TaskSpec {
            task_id: task_id(),
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
        })
        .unwrap();
    let first_attempt = attempt(0x41, 0, empty_effect_history_root());
    authority.register_attempt(first_attempt).unwrap();
    let first = issued(
        authority
            .request_commit_permit(permit(&first_attempt, 0x51))
            .unwrap(),
    );
    let first_binding = first.participant_registry_binding.unwrap();
    assert!(matches!(
        authority
            .finalize_commit(FinalizeRequest {
                task_id: task_id(),
                attempt_id: first_attempt.attempt_id,
                attempt_generation: Generation::INITIAL,
                permit_id: first.permit_id,
                new_effect_history_root: [0x61; 32],
                new_retry_fence_epoch: 0,
                finalized_at_ms: 5_000,
            })
            .unwrap(),
        FinalizeDecision::Committed(_)
    ));

    let second_attempt = attempt(0x42, 1, [0x61; 32]);
    authority.register_attempt(second_attempt).unwrap();
    let second = issued(
        authority
            .request_commit_permit(permit(&second_attempt, 0x52))
            .unwrap(),
    );
    let second_binding = second.participant_registry_binding.unwrap();
    assert_eq!(second_binding.generation, first_binding.generation + 1);
    assert_ne!(second_binding.root, first_binding.root);
    let current = authority.inspect_participant_registry(task_id()).unwrap();
    assert_eq!(current.generation, 2);
    assert_eq!(current.state, ParticipantRegistryState::FrozenForPermit);
    assert_eq!(current.prior_root, first_binding.root);
    assert_eq!(current.participants.len(), 1);
}

#[test]
fn verified_artifact_and_semantic_registration_cas_replays_and_survives_restart() {
    let database = Database::new();
    let artifact_root = AuthorityRoot::new("artifact");
    let semantic_root = AuthorityRoot::new("semantic");
    let artifact = nlos_artifact::ArtifactStore::open(&artifact_root.0).unwrap();
    let artifact_id = create_artifact(&artifact, 0x71);
    let artifact_proof = artifact.inspect_head_endpoint_proof(artifact_id).unwrap();
    let semantic = nlos_semantic::SemanticAuthority::open(&semantic_root.0).unwrap();
    let semantic_proof = semantic.inspect_admission_endpoint_proof().unwrap();

    let authority = database.open();
    authority
        .register_task(TaskSpec {
            task_id: task_id(),
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
        })
        .unwrap();
    let initial = authority.inspect_participant_registry(task_id()).unwrap();
    let artifact_registry = match authority
        .register_artifact_head_participant(
            &artifact,
            task_id(),
            binding(&initial),
            artifact_id,
            2_000,
        )
        .unwrap()
    {
        ParticipantRegistrationDecision::Registered(registry) => registry,
        other @ ParticipantRegistrationDecision::Replayed(_) => {
            panic!("expected registration, got {other:?}")
        }
    };
    assert_eq!(artifact_registry.generation, 2);
    assert!(artifact_registry.participants.iter().any(|participant| {
        participant.participant_type == ParticipantType::ArtifactHead
            && participant.participant_id == artifact_proof.participant_id
            && participant.admission_receipt_id == artifact_proof.admission_receipt_id
    }));
    match authority
        .register_artifact_head_participant(
            &artifact,
            task_id(),
            binding(&initial),
            artifact_id,
            2_001,
        )
        .unwrap()
    {
        ParticipantRegistrationDecision::Replayed(registry) => {
            assert_eq!(registry, artifact_registry);
        }
        other @ ParticipantRegistrationDecision::Registered(_) => {
            panic!("expected replay, got {other:?}")
        }
    }
    let semantic_registry = match authority
        .register_semantic_admission_participant(
            &semantic,
            task_id(),
            binding(&artifact_registry),
            2_100,
        )
        .unwrap()
    {
        ParticipantRegistrationDecision::Registered(registry) => registry,
        other @ ParticipantRegistrationDecision::Replayed(_) => {
            panic!("expected registration, got {other:?}")
        }
    };
    assert_eq!(semantic_registry.generation, 3);
    assert_eq!(semantic_registry.participants.len(), 3);
    assert!(semantic_registry.participants.iter().any(|participant| {
        participant.participant_type == ParticipantType::SemanticAdmission
            && participant.participant_id == semantic_proof.participant_id
            && participant.admission_receipt_id == semantic_proof.admission_receipt_id
    }));
    drop(authority);

    assert_eq!(
        database
            .open()
            .inspect_participant_registry(task_id())
            .unwrap(),
        semantic_registry
    );
}

#[test]
fn stale_or_frozen_registration_fails_without_mutating_registry() {
    let database = Database::new();
    let artifact_root = AuthorityRoot::new("artifact-fence");
    let artifact = nlos_artifact::ArtifactStore::open(&artifact_root.0).unwrap();
    let first_artifact = create_artifact(&artifact, 0x61);
    let second_artifact = create_artifact(&artifact, 0x62);
    let authority = database.open();
    authority
        .register_task(TaskSpec {
            task_id: task_id(),
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
        })
        .unwrap();
    let initial = authority.inspect_participant_registry(task_id()).unwrap();
    assert!(matches!(
        authority.register_artifact_head_participant(
            &artifact,
            task_id(),
            binding(&initial),
            ArtifactId::from_bytes([0x63; 16]),
            1_900,
        ),
        Err(TaskStoreError::ArtifactParticipantAuthority(
            nlos_artifact::ArtifactError::ArtifactNotFound(_)
        ))
    ));
    assert_eq!(
        authority.inspect_participant_registry(task_id()).unwrap(),
        initial
    );
    let current = authority
        .register_artifact_head_participant(
            &artifact,
            task_id(),
            binding(&initial),
            first_artifact,
            2_000,
        )
        .unwrap()
        .registry()
        .clone();
    assert!(matches!(
        authority.register_artifact_head_participant(
            &artifact,
            task_id(),
            binding(&initial),
            second_artifact,
            2_100,
        ),
        Err(TaskStoreError::ParticipantRegistryCasMismatch)
    ));
    assert_eq!(
        authority.inspect_participant_registry(task_id()).unwrap(),
        current
    );

    let spec = attempt(0x51, 0, empty_effect_history_root());
    authority.register_attempt(spec).unwrap();
    issued(
        authority
            .request_commit_permit(permit(&spec, 0x52))
            .unwrap(),
    );
    let frozen = authority.inspect_participant_registry(task_id()).unwrap();
    assert!(matches!(
        authority.register_artifact_head_participant(
            &artifact,
            task_id(),
            binding(&frozen),
            second_artifact,
            3_000,
        ),
        Err(TaskStoreError::ParticipantRegistryFrozen {
            state: ParticipantRegistryState::FrozenForPermit
        })
    ));
    assert_eq!(
        authority.inspect_participant_registry(task_id()).unwrap(),
        frozen
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One lifecycle proves register, rotate, replace, replay, restart.
fn verified_resource_registration_tracks_driver_rotation_and_replays() {
    let database = Database::new();
    let resource_root = AuthorityRoot::new("resource");
    let resource = nlos_resource::ResourceAuthority::open(&resource_root.0).unwrap();
    let driver = resource
        .register_driver(nlos_resource::RegisterDriverRequest {
            profile_digest: [0x81; 32],
            idempotency_key: IdempotencyKey::from_bytes([0x82; 16]),
            created_at_ms: 1_000,
        })
        .unwrap()
        .record();
    let account = resource
        .create_account(nlos_resource::CreateAccountRequest {
            initial_credit: 100,
            idempotency_key: IdempotencyKey::from_bytes([0x83; 16]),
            created_at_ms: 1_000,
        })
        .unwrap();
    let initial_driver_proof = resource
        .inspect_driver_gateway_endpoint_proof(driver.driver_id)
        .unwrap();

    let authority = database.open();
    authority
        .register_task(TaskSpec {
            task_id: task_id(),
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
        })
        .unwrap();
    let initial = authority.inspect_participant_registry(task_id()).unwrap();
    let generation_two = Generation::INITIAL.checked_next().unwrap();
    assert!(matches!(
        authority.register_driver_gateway_participant(
            &resource,
            task_id(),
            binding(&initial),
            driver.driver_id,
            generation_two,
            1_900,
        ),
        Err(TaskStoreError::ParticipantEndpointGenerationMismatch {
            expected: 2,
            current: 1
        })
    ));
    assert_eq!(
        authority.inspect_participant_registry(task_id()).unwrap(),
        initial
    );
    let driver_registry = authority
        .register_driver_gateway_participant(
            &resource,
            task_id(),
            binding(&initial),
            driver.driver_id,
            driver.generation,
            2_000,
        )
        .unwrap()
        .registry()
        .clone();
    let ledger_registry = authority
        .register_resource_ledger_participant(
            &resource,
            task_id(),
            binding(&driver_registry),
            account.account_id,
            Generation::INITIAL,
            2_100,
        )
        .unwrap()
        .registry()
        .clone();
    assert_eq!(ledger_registry.participants.len(), 3);

    let rotated = resource
        .rotate_driver(nlos_resource::RotateDriverRequest {
            driver_id: driver.driver_id,
            expected_generation: driver.generation,
            expected_fencing_token: driver.fencing_token,
            idempotency_key: IdempotencyKey::from_bytes([0x84; 16]),
            rotated_at_ms: 2_200,
        })
        .unwrap()
        .record();
    assert!(matches!(
        authority.register_driver_gateway_participant(
            &resource,
            task_id(),
            binding(&ledger_registry),
            driver.driver_id,
            driver.generation,
            2_300,
        ),
        Err(TaskStoreError::ParticipantEndpointGenerationMismatch {
            expected: 1,
            current: 2
        })
    ));
    let rotated_registry = authority
        .register_driver_gateway_participant(
            &resource,
            task_id(),
            binding(&ledger_registry),
            driver.driver_id,
            rotated.generation,
            2_400,
        )
        .unwrap()
        .registry()
        .clone();
    assert_eq!(rotated_registry.participants.len(), 3);
    let rotated_participant = rotated_registry
        .participants
        .iter()
        .find(|participant| participant.participant_type == ParticipantType::DriverGateway)
        .unwrap();
    assert_eq!(
        rotated_participant.participant_id,
        initial_driver_proof.participant_id
    );
    assert_eq!(
        rotated_participant.participant_generation,
        rotated.generation
    );
    assert_ne!(
        rotated_participant.admission_receipt_id,
        initial_driver_proof.admission_receipt_id
    );
    match authority
        .register_driver_gateway_participant(
            &resource,
            task_id(),
            binding(&ledger_registry),
            driver.driver_id,
            rotated.generation,
            2_500,
        )
        .unwrap()
    {
        ParticipantRegistrationDecision::Replayed(registry) => {
            assert_eq!(registry, rotated_registry);
        }
        other @ ParticipantRegistrationDecision::Registered(_) => {
            panic!("expected replay, got {other:?}")
        }
    }
    drop(authority);
    assert_eq!(
        database
            .open()
            .inspect_participant_registry(task_id())
            .unwrap(),
        rotated_registry
    );
}
