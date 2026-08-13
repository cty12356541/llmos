use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_task::{
    AttemptSpec, EffectPermitDecision, EffectPermitRequest, FinalizeDecision, FinalizeRequest,
    LogicalEffectDescriptor, Outcome, OutcomeRequest, ParticipantRegistrationDecision,
    ParticipantRegistryBinding, ParticipantRegistryState, ParticipantType, PermitDecision,
    PermitRequest, PlannedEffect, SnapshotBundle, SnapshotConsistency, SqliteTaskAuthority,
    TaskSpec, TaskStoreError, TaskWriteSetArtifactRead, TaskWriteSetRequest,
    empty_effect_history_root,
};
use nlos_types::{
    ArtifactId, CancellationScopeId, Generation, IdempotencyKey, TaskAttemptId, TaskId,
    TaskSnapshotId,
};
use rusqlite::Connection;

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
fn verified_write_set_seal_binds_receipted_snapshot_and_artifact_reads() {
    let database = Database::new();
    let artifact_root = AuthorityRoot::new("write-set-artifact");
    let artifact = nlos_artifact::ArtifactStore::open(&artifact_root.0).unwrap();
    let artifact_id = create_artifact(&artifact, 0xb1);
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
    let request = TaskWriteSetRequest {
        task_id: task_id(),
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        artifact_reads: vec![TaskWriteSetArtifactRead {
            artifact_id,
            expected_head_revision: 0,
            expected_head_digest: None,
        }],
        idempotency_key: IdempotencyKey::from_bytes([0xbc; 16]),
        sealed_at_ms: 1_200,
    };
    let record = match authority
        .seal_task_write_set(&artifact, request.clone())
        .unwrap()
    {
        nlos_task::TaskWriteSetDecision::Sealed(record) => record,
        nlos_task::TaskWriteSetDecision::Replayed(_) => panic!("expected new write-set seal"),
    };
    assert_eq!(record.snapshot_receipt_id, receipt_id);
    assert_eq!(record.artifact_reads, request.artifact_reads);
    assert_eq!(record.group_binding, None);
    assert_eq!(
        record.participant_registry_binding,
        binding(&authority.inspect_participant_registry(task_id()).unwrap())
    );
    assert!(matches!(
        authority.seal_task_write_set(
            &artifact,
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
    match authority.seal_task_write_set(&artifact, request).unwrap() {
        nlos_task::TaskWriteSetDecision::Replayed(replayed) => assert_eq!(replayed, record),
        nlos_task::TaskWriteSetDecision::Sealed(_) => panic!("expected replayed write-set seal"),
    }
    let mut permit_request = permit(&spec, 0xbd);
    permit_request.write_set_root = record.write_set_root;
    let permit_record = issued(authority.request_commit_permit(permit_request).unwrap());
    assert_eq!(permit_record.write_set_root, record.write_set_root);
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
