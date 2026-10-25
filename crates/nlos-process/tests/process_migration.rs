use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_process::{
    CapabilityRebindProof, CheckpointPayloadKind, CreateIsolationDomainRequest,
    DriverSessionAttachment, InitiateProcessMigrationRequest, IsolationDomainDecision,
    LeaseCloseRegrantProof, MigratedLeaseEntry, MigrationAbortOutcome, MigrationMode,
    MigrationStepDecision, PinnedSessionPolicy, ProcessAuthority, ProcessBindingDecision,
    ProcessBindingRecord, ProcessCheckpointProof, ProcessMigrationCoordinator,
    ProcessMigrationError, ProcessMigrationStep, QuiesceProof, ReboundCapabilityEntry,
    RegisterDelegatedProcessRequest, RestoreProcessDecision, RestoreProcessRequest,
    SourceProcessFenceProof, TargetRestoreProof,
};
use nlos_types::{Generation, IdempotencyKey, IsolationDomainId, ProcessId, TaskAttemptId, TaskId};

static NEXT_DIR_ID: AtomicU64 = AtomicU64::new(1);

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        let mut path = env::temp_dir();
        let id = NEXT_DIR_ID.fetch_add(1, Ordering::Relaxed);
        path.push(format!(
            "nlos-process-migration-{tag}-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).expect("temp dir should be created");
        Self(path)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn base_initiate_request(
    process_id: ProcessId,
    attached_sessions: Vec<DriverSessionAttachment>,
    pinned_session_policy: PinnedSessionPolicy,
) -> InitiateProcessMigrationRequest {
    InitiateProcessMigrationRequest {
        migration_id: [0x11; 16],
        process_id,
        source_cell_id: [0xA1; 16],
        target_cell_id: [0xB2; 16],
        source_generation: 1,
        target_generation: 2,
        attached_sessions,
        pinned_session_policy,
        initiated_at_ms: 1_000,
    }
}

fn sample_quiesce_proof(process_id: ProcessId) -> QuiesceProof {
    QuiesceProof {
        process_id,
        source_cell_id: [0xA1; 16],
        source_generation: 1,
        quiesced_fibers: 4,
        cancelled_inflight_ops: 2,
        inflight_effects_drained: true,
        quiesced_at_ms: 1_100,
    }
}

fn sample_checkpoint_proof(
    process_id: ProcessId,
    migrated_session_digests: Vec<([u8; 16], [u8; 32])>,
) -> ProcessCheckpointProof {
    ProcessCheckpointProof {
        process_id,
        source_generation: 1,
        payload: CheckpointPayloadKind::FullStateCheckpoint {
            state_checkpoint_digest: [0xCC; 32],
            migrated_session_digests,
        },
        checkpointed_at_ms: 1_200,
    }
}

fn sample_source_fence_proof(process_id: ProcessId, fence: [u8; 32]) -> SourceProcessFenceProof {
    SourceProcessFenceProof {
        process_id,
        source_cell_id: [0xA1; 16],
        source_cell_epoch: 7,
        fenced_generation: 1,
        source_fencing_token: fence,
        supervisor_unregistered: true,
        fenced_at_ms: 1_300,
    }
}

fn sample_capability_rebind_proof(process_id: ProcessId) -> CapabilityRebindProof {
    CapabilityRebindProof {
        process_id,
        rebound_capabilities: vec![
            ReboundCapabilityEntry {
                capability_id: [0x51; 16],
                source_cell_id: [0xA1; 16],
                source_generation: 1,
                target_cell_id: [0xB2; 16],
                target_generation: 2,
                attenuation_digest: [0xD1; 32],
            },
            ReboundCapabilityEntry {
                capability_id: [0x52; 16],
                source_cell_id: [0xA1; 16],
                source_generation: 1,
                target_cell_id: [0xB2; 16],
                target_generation: 2,
                attenuation_digest: [0xD2; 32],
            },
        ],
        rebound_at_ms: 1_400,
    }
}

fn sample_lease_regrant_proof(process_id: ProcessId) -> LeaseCloseRegrantProof {
    LeaseCloseRegrantProof {
        process_id,
        source_generation: 1,
        target_generation: 2,
        migrated_leases: vec![MigratedLeaseEntry {
            source_lease_id: [0x61; 16],
            source_cell_id: [0xA1; 16],
            source_closed_or_fenced: true,
            source_close_receipt_digest: [0xE1; 32],
            target_lease_id: [0x62; 16],
            target_cell_id: [0xB2; 16],
            target_grant_epoch: 9,
        }],
        regranted_at_ms: 1_500,
    }
}

fn sample_target_restore_proof(
    process_id: ProcessId,
    restored_digest: [u8; 32],
) -> TargetRestoreProof {
    TargetRestoreProof {
        process_id,
        target_cell_id: [0xB2; 16],
        restored_generation: 2,
        target_fencing_token: [0xF2; 32],
        target_isolation_domain_id: IsolationDomainId::from_bytes([0x77; 16]),
        restored_image_or_checkpoint_digest: restored_digest,
        restored_at_ms: 1_600,
    }
}

fn register_and_restore_process_on_authority(
    authority: &ProcessAuthority,
) -> (ProcessBindingRecord, ProcessBindingRecord) {
    let IsolationDomainDecision::Created(source_domain) = authority
        .create_isolation_domain(CreateIsolationDomainRequest {
            policy_digest: [0x10; 32],
            idempotency_key: IdempotencyKey::from_bytes([0x01; 16]),
            created_at_ms: 500,
        })
        .expect("source domain should be created")
    else {
        panic!("expected Created");
    };

    let IsolationDomainDecision::Created(target_domain) = authority
        .create_isolation_domain(CreateIsolationDomainRequest {
            policy_digest: [0x20; 32],
            idempotency_key: IdempotencyKey::from_bytes([0x02; 16]),
            created_at_ms: 510,
        })
        .expect("target domain should be created")
    else {
        panic!("expected Created");
    };

    let ProcessBindingDecision::Registered(initial_binding) = authority
        .register_delegated_process(RegisterDelegatedProcessRequest {
            task_id: TaskId::from_bytes([0x31; 16]),
            task_attempt_id: TaskAttemptId::from_bytes([0x32; 16]),
            attempt_generation: Generation::INITIAL,
            isolation_domain_id: source_domain.isolation_domain_id,
            isolation_domain_generation: source_domain.generation,
            isolation_domain_fencing_token: source_domain.fencing_token,
            idempotency_key: IdempotencyKey::from_bytes([0x03; 16]),
            created_at_ms: 600,
        })
        .expect("process should register")
    else {
        panic!("expected Registered");
    };

    let RestoreProcessDecision::Restored(restored_binding) = authority
        .restore_process(RestoreProcessRequest {
            process_id: initial_binding.process_id,
            expected_process_generation: initial_binding.process_generation,
            expected_process_fencing_token: initial_binding.process_fencing_token,
            isolation_domain_id: target_domain.isolation_domain_id,
            isolation_domain_generation: target_domain.generation,
            isolation_domain_fencing_token: target_domain.fencing_token,
            idempotency_key: IdempotencyKey::from_bytes([0x04; 16]),
            restored_at_ms: 1_600,
        })
        .expect("authority restore_process should succeed")
    else {
        panic!("expected Restored");
    };

    (initial_binding, restored_binding)
}

#[test]
fn transparent_checkpoint_restore_full_8_step_happy_path_and_deterministic_receipt() {
    let dir = TempDir::new("happy-path");
    let authority = ProcessAuthority::open(&dir.0).expect("authority should open");
    let (initial_binding, restored_binding) = register_and_restore_process_on_authority(&authority);
    let process_id = initial_binding.process_id;

    let mut coordinator = ProcessMigrationCoordinator::initiate(base_initiate_request(
        process_id,
        vec![DriverSessionAttachment::Migratable {
            session_id: [0x41; 16],
            state_digest: [0x42; 32],
        }],
        PinnedSessionPolicy::RefuseMigration,
    ))
    .expect("step 1 initiate should succeed");

    assert_eq!(coordinator.current_step(), ProcessMigrationStep::Initiated);
    assert_eq!(
        *coordinator.migration_mode(),
        MigrationMode::TransparentCheckpointRestore
    );

    assert_eq!(
        coordinator
            .record_quiesce(sample_quiesce_proof(process_id))
            .unwrap(),
        MigrationStepDecision::Advanced(ProcessMigrationStep::Quiesced)
    );
    assert_eq!(
        coordinator
            .record_checkpoint(sample_checkpoint_proof(
                process_id,
                vec![([0x41; 16], [0x42; 32])]
            ))
            .unwrap(),
        MigrationStepDecision::Advanced(ProcessMigrationStep::Checkpointed)
    );
    assert_eq!(
        coordinator
            .record_source_fence(sample_source_fence_proof(
                process_id,
                initial_binding.process_fencing_token
            ))
            .unwrap(),
        MigrationStepDecision::Advanced(ProcessMigrationStep::SourceFenced)
    );
    assert_eq!(
        coordinator
            .record_capability_rebind(sample_capability_rebind_proof(process_id))
            .unwrap(),
        MigrationStepDecision::Advanced(ProcessMigrationStep::CapabilitiesRebound)
    );
    assert_eq!(
        coordinator
            .record_lease_close_regrant(sample_lease_regrant_proof(process_id))
            .unwrap(),
        MigrationStepDecision::Advanced(ProcessMigrationStep::LeasesRegranted)
    );

    assert_eq!(restored_binding.process_generation.get(), 2);
    let restore_proof =
        TargetRestoreProof::from_restored_binding(&restored_binding, [0xB2; 16], [0xCC; 32]);
    assert_eq!(
        coordinator
            .record_target_restore(restore_proof.clone())
            .unwrap(),
        MigrationStepDecision::Advanced(ProcessMigrationStep::TargetRestored)
    );

    let receipt = coordinator
        .complete(1_700)
        .expect("step 8 complete should succeed");
    assert_eq!(coordinator.current_step(), ProcessMigrationStep::Completed);
    assert_eq!(receipt.source_generation, 1);
    assert_eq!(receipt.target_generation, 2);
    assert_eq!(
        receipt.migration_mode,
        MigrationMode::TransparentCheckpointRestore
    );
    assert_ne!(receipt.receipt_digest, [0u8; 32]);
    assert!(receipt.verify_digest());

    // Independent second coordinator with identical inputs yields byte-identical digest.
    let second_receipt = run_transparent_migration(
        process_id,
        initial_binding.process_fencing_token,
        restore_proof,
    );
    assert_eq!(receipt.receipt_digest, second_receipt.receipt_digest);
}

fn run_transparent_migration(
    process_id: ProcessId,
    source_fence: [u8; 32],
    restore_proof: TargetRestoreProof,
) -> nlos_process::ProcessMigrationReceipt {
    let mut coord = ProcessMigrationCoordinator::initiate(base_initiate_request(
        process_id,
        vec![DriverSessionAttachment::Migratable {
            session_id: [0x41; 16],
            state_digest: [0x42; 32],
        }],
        PinnedSessionPolicy::RefuseMigration,
    ))
    .unwrap();
    coord
        .record_quiesce(sample_quiesce_proof(process_id))
        .unwrap();
    coord
        .record_checkpoint(sample_checkpoint_proof(
            process_id,
            vec![([0x41; 16], [0x42; 32])],
        ))
        .unwrap();
    coord
        .record_source_fence(sample_source_fence_proof(process_id, source_fence))
        .unwrap();
    coord
        .record_capability_rebind(sample_capability_rebind_proof(process_id))
        .unwrap();
    coord
        .record_lease_close_regrant(sample_lease_regrant_proof(process_id))
        .unwrap();
    coord.record_target_restore(restore_proof).unwrap();
    coord.complete(1_700).unwrap()
}

#[test]
fn pinned_session_refuse_migration_policy_fails_closed_dist_migrate_002() {
    let process_id = ProcessId::from_bytes([0x21; 16]);
    let sessions = vec![
        DriverSessionAttachment::Migratable {
            session_id: [0x41; 16],
            state_digest: [0x42; 32],
        },
        DriverSessionAttachment::PinnedNonMigratable {
            session_id: [0x99; 16],
            pin_reason: "exclusive PCIe USB controller session".to_string(),
            artifact_restart_image_digest: Some([0xAA; 32]),
        },
    ];

    let err = ProcessMigrationCoordinator::initiate(base_initiate_request(
        process_id,
        sessions,
        PinnedSessionPolicy::RefuseMigration,
    ))
    .expect_err("RefuseMigration must reject pinned non-migratable session fail-closed");

    assert_eq!(
        err,
        ProcessMigrationError::PinnedSessionNonMigratable {
            session_id: [0x99; 16],
            pin_reason: "exclusive PCIe USB controller session".to_string(),
        }
    );
}

#[test]
fn pinned_session_downgrade_to_restart_from_artifact_requires_digest_and_forbids_transparent_checkpoint()
 {
    let process_id = ProcessId::from_bytes([0x22; 16]);

    // 1. Missing artifact restart image digest -> fail-closed.
    let missing_err = ProcessMigrationCoordinator::initiate(base_initiate_request(
        process_id,
        vec![DriverSessionAttachment::PinnedNonMigratable {
            session_id: [0x91; 16],
            pin_reason: "host GPU DMA ring".to_string(),
            artifact_restart_image_digest: None,
        }],
        PinnedSessionPolicy::DowngradeToRestartFromArtifact,
    ))
    .expect_err("downgrade without artifact digest must fail closed");
    assert_eq!(
        missing_err,
        ProcessMigrationError::MissingArtifactRestartDigest {
            session_id: [0x91; 16],
            pin_reason: "host GPU DMA ring".to_string(),
        }
    );

    // 2. Conflicting artifact restart image digests across multiple pinned sessions -> fail-closed.
    let conflict_err = ProcessMigrationCoordinator::initiate(base_initiate_request(
        process_id,
        vec![
            DriverSessionAttachment::PinnedNonMigratable {
                session_id: [0x91; 16],
                pin_reason: "host GPU DMA ring".to_string(),
                artifact_restart_image_digest: Some([0xAA; 32]),
            },
            DriverSessionAttachment::PinnedNonMigratable {
                session_id: [0x92; 16],
                pin_reason: "local audio capture".to_string(),
                artifact_restart_image_digest: Some([0xBB; 32]),
            },
        ],
        PinnedSessionPolicy::DowngradeToRestartFromArtifact,
    ))
    .expect_err("conflicting artifact digests must fail closed");
    assert_eq!(
        conflict_err,
        ProcessMigrationError::ConflictingArtifactRestartDigests {
            first_digest: [0xAA; 32],
            conflicting_digest: [0xBB; 32],
        }
    );

    // 3. Valid artifact restart image digest -> explicitly downgraded to DowngradedRestartFromArtifact.
    let mut coordinator = ProcessMigrationCoordinator::initiate(base_initiate_request(
        process_id,
        vec![DriverSessionAttachment::PinnedNonMigratable {
            session_id: [0x91; 16],
            pin_reason: "host GPU DMA ring".to_string(),
            artifact_restart_image_digest: Some([0xAA; 32]),
        }],
        PinnedSessionPolicy::DowngradeToRestartFromArtifact,
    ))
    .expect("downgrade with valid artifact digest should be admitted");

    let expected_mode = MigrationMode::DowngradedRestartFromArtifact {
        artifact_image_digest: [0xAA; 32],
        dropped_pinned_sessions: vec![[0x91; 16]],
    };
    assert_eq!(*coordinator.migration_mode(), expected_mode);
    coordinator
        .record_quiesce(sample_quiesce_proof(process_id))
        .unwrap();

    // 4. Attempting to submit a FullStateCheckpoint (pretending transparent migration!) MUST fail closed.
    assert_eq!(
        coordinator.record_checkpoint(sample_checkpoint_proof(process_id, vec![])),
        Err(ProcessMigrationError::CheckpointModeMismatch {
            mode: expected_mode.clone(),
        })
    );

    // 5. Submitting matching ArtifactRestartImage succeeds and completes downgraded migration.
    coordinator
        .record_checkpoint(ProcessCheckpointProof {
            process_id,
            source_generation: 1,
            payload: CheckpointPayloadKind::ArtifactRestartImage {
                artifact_image_digest: [0xAA; 32],
            },
            checkpointed_at_ms: 1_200,
        })
        .unwrap();
    coordinator
        .record_source_fence(sample_source_fence_proof(process_id, [0xF1; 32]))
        .unwrap();
    coordinator
        .record_capability_rebind(sample_capability_rebind_proof(process_id))
        .unwrap();
    coordinator
        .record_lease_close_regrant(sample_lease_regrant_proof(process_id))
        .unwrap();
    coordinator
        .record_target_restore(sample_target_restore_proof(process_id, [0xAA; 32]))
        .unwrap();

    let receipt = coordinator
        .complete(1_700)
        .expect("complete should succeed");
    assert_eq!(receipt.migration_mode, expected_mode);
    assert!(receipt.verify_digest());
}

#[test]
fn out_of_order_step_bypass_attempts_fail_closed() {
    let process_id = ProcessId::from_bytes([0x23; 16]);
    let mut coordinator = ProcessMigrationCoordinator::initiate(base_initiate_request(
        process_id,
        vec![],
        PinnedSessionPolicy::RefuseMigration,
    ))
    .unwrap();

    // From Initiated, skipping Quiesced to any later step fails closed.
    assert_eq!(
        coordinator.record_checkpoint(sample_checkpoint_proof(process_id, vec![])),
        Err(ProcessMigrationError::InvalidStepOrder {
            current_step: ProcessMigrationStep::Initiated,
            attempted_step: ProcessMigrationStep::Checkpointed,
        })
    );
    assert_eq!(
        coordinator.record_source_fence(sample_source_fence_proof(process_id, [0xF1; 32])),
        Err(ProcessMigrationError::InvalidStepOrder {
            current_step: ProcessMigrationStep::Initiated,
            attempted_step: ProcessMigrationStep::SourceFenced,
        })
    );
    assert_eq!(
        coordinator.record_target_restore(sample_target_restore_proof(process_id, [0xCC; 32])),
        Err(ProcessMigrationError::InvalidStepOrder {
            current_step: ProcessMigrationStep::Initiated,
            attempted_step: ProcessMigrationStep::TargetRestored,
        })
    );
    assert_eq!(
        coordinator.complete(1_700),
        Err(ProcessMigrationError::InvalidStepOrder {
            current_step: ProcessMigrationStep::Initiated,
            attempted_step: ProcessMigrationStep::Completed,
        })
    );

    // Advance to Quiesced; skipping Checkpointed fails closed.
    coordinator
        .record_quiesce(sample_quiesce_proof(process_id))
        .unwrap();
    assert_eq!(
        coordinator.record_source_fence(sample_source_fence_proof(process_id, [0xF1; 32])),
        Err(ProcessMigrationError::InvalidStepOrder {
            current_step: ProcessMigrationStep::Quiesced,
            attempted_step: ProcessMigrationStep::SourceFenced,
        })
    );

    // Advance to Checkpointed; skipping SourceFenced fails closed.
    coordinator
        .record_checkpoint(sample_checkpoint_proof(process_id, vec![]))
        .unwrap();
    assert_eq!(
        coordinator.record_capability_rebind(sample_capability_rebind_proof(process_id)),
        Err(ProcessMigrationError::InvalidStepOrder {
            current_step: ProcessMigrationStep::Checkpointed,
            attempted_step: ProcessMigrationStep::CapabilitiesRebound,
        })
    );
    assert_eq!(
        coordinator.record_target_restore(sample_target_restore_proof(process_id, [0xCC; 32])),
        Err(ProcessMigrationError::InvalidStepOrder {
            current_step: ProcessMigrationStep::Checkpointed,
            attempted_step: ProcessMigrationStep::TargetRestored,
        })
    );

    // Advance to SourceFenced; skipping CapabilitiesRebound fails closed.
    coordinator
        .record_source_fence(sample_source_fence_proof(process_id, [0xF1; 32]))
        .unwrap();
    assert_eq!(
        coordinator.record_lease_close_regrant(sample_lease_regrant_proof(process_id)),
        Err(ProcessMigrationError::InvalidStepOrder {
            current_step: ProcessMigrationStep::SourceFenced,
            attempted_step: ProcessMigrationStep::LeasesRegranted,
        })
    );

    // Advance to CapabilitiesRebound; skipping LeasesRegranted fails closed.
    coordinator
        .record_capability_rebind(sample_capability_rebind_proof(process_id))
        .unwrap();
    assert_eq!(
        coordinator.record_target_restore(sample_target_restore_proof(process_id, [0xCC; 32])),
        Err(ProcessMigrationError::InvalidStepOrder {
            current_step: ProcessMigrationStep::CapabilitiesRebound,
            attempted_step: ProcessMigrationStep::TargetRestored,
        })
    );

    // Advance to LeasesRegranted; skipping TargetRestored fails closed.
    coordinator
        .record_lease_close_regrant(sample_lease_regrant_proof(process_id))
        .unwrap();
    assert_eq!(
        coordinator.complete(1_700),
        Err(ProcessMigrationError::InvalidStepOrder {
            current_step: ProcessMigrationStep::LeasesRegranted,
            attempted_step: ProcessMigrationStep::Completed,
        })
    );
}

#[test]
fn invalid_initiation_parameters_and_wrong_target_generation_fail_closed() {
    let process_id = ProcessId::from_bytes([0x24; 16]);

    let mut same_cell =
        base_initiate_request(process_id, vec![], PinnedSessionPolicy::RefuseMigration);
    same_cell.target_cell_id = same_cell.source_cell_id;
    assert_eq!(
        ProcessMigrationCoordinator::initiate(same_cell),
        Err(ProcessMigrationError::SameSourceAndTargetCell {
            cell_id: [0xA1; 16],
        })
    );

    let mut wrong_gen =
        base_initiate_request(process_id, vec![], PinnedSessionPolicy::RefuseMigration);
    wrong_gen.source_generation = 3;
    wrong_gen.target_generation = 5;
    assert_eq!(
        ProcessMigrationCoordinator::initiate(wrong_gen),
        Err(ProcessMigrationError::InvalidTargetGeneration {
            source_generation: 3,
            target_generation: 5,
        })
    );

    let mut zero_gen =
        base_initiate_request(process_id, vec![], PinnedSessionPolicy::RefuseMigration);
    zero_gen.source_generation = 0;
    zero_gen.target_generation = 1;
    assert_eq!(
        ProcessMigrationCoordinator::initiate(zero_gen),
        Err(ProcessMigrationError::InvalidTargetGeneration {
            source_generation: 0,
            target_generation: 1,
        })
    );

    let dup_sessions = base_initiate_request(
        process_id,
        vec![
            DriverSessionAttachment::Migratable {
                session_id: [0x41; 16],
                state_digest: [0x42; 32],
            },
            DriverSessionAttachment::Migratable {
                session_id: [0x41; 16],
                state_digest: [0x43; 32],
            },
        ],
        PinnedSessionPolicy::RefuseMigration,
    );
    assert_eq!(
        ProcessMigrationCoordinator::initiate(dup_sessions),
        Err(ProcessMigrationError::DuplicateDriverSession {
            session_id: [0x41; 16],
        })
    );
}

#[test]
fn quiesce_and_source_fence_incomplete_proofs_fail_closed() {
    let process_id = ProcessId::from_bytes([0x25; 16]);
    let mut coordinator = ProcessMigrationCoordinator::initiate(base_initiate_request(
        process_id,
        vec![],
        PinnedSessionPolicy::RefuseMigration,
    ))
    .unwrap();

    let mut undrained = sample_quiesce_proof(process_id);
    undrained.inflight_effects_drained = false;
    assert_eq!(
        coordinator.record_quiesce(undrained),
        Err(ProcessMigrationError::SourceNotQuiesced)
    );

    let mut wrong_gen_quiesce = sample_quiesce_proof(process_id);
    wrong_gen_quiesce.source_generation = 99;
    assert_eq!(
        coordinator.record_quiesce(wrong_gen_quiesce),
        Err(ProcessMigrationError::BindingMismatch {
            step: ProcessMigrationStep::Quiesced,
            field: "source_generation",
        })
    );

    coordinator
        .record_quiesce(sample_quiesce_proof(process_id))
        .unwrap();
    coordinator
        .record_checkpoint(sample_checkpoint_proof(process_id, vec![]))
        .unwrap();

    let mut incomplete_fence = sample_source_fence_proof(process_id, [0xF1; 32]);
    incomplete_fence.supervisor_unregistered = false;
    assert_eq!(
        coordinator.record_source_fence(incomplete_fence),
        Err(ProcessMigrationError::SourceFenceIncomplete)
    );

    let zero_token_fence = sample_source_fence_proof(process_id, [0u8; 32]);
    assert_eq!(
        coordinator.record_source_fence(zero_token_fence),
        Err(ProcessMigrationError::SourceFenceIncomplete)
    );
}

#[test]
fn capability_rebind_wrong_cell_or_generation_fails_closed() {
    let process_id = ProcessId::from_bytes([0x26; 16]);
    let mut coordinator = ProcessMigrationCoordinator::initiate(base_initiate_request(
        process_id,
        vec![],
        PinnedSessionPolicy::RefuseMigration,
    ))
    .unwrap();
    coordinator
        .record_quiesce(sample_quiesce_proof(process_id))
        .unwrap();
    coordinator
        .record_checkpoint(sample_checkpoint_proof(process_id, vec![]))
        .unwrap();
    coordinator
        .record_source_fence(sample_source_fence_proof(process_id, [0xF1; 32]))
        .unwrap();

    let mut wrong_cap_gen = sample_capability_rebind_proof(process_id);
    wrong_cap_gen.rebound_capabilities[0].target_generation = 1;
    assert_eq!(
        coordinator.record_capability_rebind(wrong_cap_gen),
        Err(ProcessMigrationError::BindingMismatch {
            step: ProcessMigrationStep::CapabilitiesRebound,
            field: "target_generation",
        })
    );

    let mut wrong_cap_cell = sample_capability_rebind_proof(process_id);
    wrong_cap_cell.rebound_capabilities[0].target_cell_id = [0xFF; 16];
    assert_eq!(
        coordinator.record_capability_rebind(wrong_cap_cell),
        Err(ProcessMigrationError::BindingMismatch {
            step: ProcessMigrationStep::CapabilitiesRebound,
            field: "target_cell_id",
        })
    );
}

#[test]
fn lease_regrant_without_source_lease_close_fails_closed_lease_migrate_001() {
    let process_id = ProcessId::from_bytes([0x27; 16]);
    let mut coordinator = ProcessMigrationCoordinator::initiate(base_initiate_request(
        process_id,
        vec![],
        PinnedSessionPolicy::RefuseMigration,
    ))
    .unwrap();
    coordinator
        .record_quiesce(sample_quiesce_proof(process_id))
        .unwrap();
    coordinator
        .record_checkpoint(sample_checkpoint_proof(process_id, vec![]))
        .unwrap();
    coordinator
        .record_source_fence(sample_source_fence_proof(process_id, [0xF1; 32]))
        .unwrap();
    coordinator
        .record_capability_rebind(sample_capability_rebind_proof(process_id))
        .unwrap();

    let mut unclosed_lease = sample_lease_regrant_proof(process_id);
    unclosed_lease.migrated_leases[0].source_closed_or_fenced = false;
    assert_eq!(
        coordinator.record_lease_close_regrant(unclosed_lease),
        Err(ProcessMigrationError::SourceLeaseNotClosed {
            source_lease_id: [0x61; 16],
        })
    );

    let mut zero_receipt_lease = sample_lease_regrant_proof(process_id);
    zero_receipt_lease.migrated_leases[0].source_close_receipt_digest = [0u8; 32];
    assert_eq!(
        coordinator.record_lease_close_regrant(zero_receipt_lease),
        Err(ProcessMigrationError::SourceLeaseNotClosed {
            source_lease_id: [0x61; 16],
        })
    );
}

#[test]
fn target_restore_digest_mismatch_and_stale_fence_reuse_fail_closed() {
    let process_id = ProcessId::from_bytes([0x28; 16]);
    let mut coordinator = ProcessMigrationCoordinator::initiate(base_initiate_request(
        process_id,
        vec![],
        PinnedSessionPolicy::RefuseMigration,
    ))
    .unwrap();
    coordinator
        .record_quiesce(sample_quiesce_proof(process_id))
        .unwrap();
    coordinator
        .record_checkpoint(sample_checkpoint_proof(process_id, vec![]))
        .unwrap();
    coordinator
        .record_source_fence(sample_source_fence_proof(process_id, [0xF1; 32]))
        .unwrap();
    coordinator
        .record_capability_rebind(sample_capability_rebind_proof(process_id))
        .unwrap();
    coordinator
        .record_lease_close_regrant(sample_lease_regrant_proof(process_id))
        .unwrap();

    let bad_digest_restore = sample_target_restore_proof(process_id, [0xDD; 32]);
    assert_eq!(
        coordinator.record_target_restore(bad_digest_restore),
        Err(ProcessMigrationError::RestoreDigestMismatch {
            expected_digest: [0xCC; 32],
            actual_digest: [0xDD; 32],
        })
    );

    let mut stale_fence_restore = sample_target_restore_proof(process_id, [0xCC; 32]);
    stale_fence_restore.target_fencing_token = [0xF1; 32];
    assert_eq!(
        coordinator.record_target_restore(stale_fence_restore),
        Err(ProcessMigrationError::DuplicateOrStaleTargetFence)
    );

    let mut wrong_gen_restore = sample_target_restore_proof(process_id, [0xCC; 32]);
    wrong_gen_restore.restored_generation = 1;
    assert_eq!(
        coordinator.record_target_restore(wrong_gen_restore),
        Err(ProcessMigrationError::BindingMismatch {
            step: ProcessMigrationStep::TargetRestored,
            field: "restored_generation",
        })
    );
}

#[test]
fn idempotent_step_replay_succeeds_and_conflicting_proof_replay_fails_closed() {
    let process_id = ProcessId::from_bytes([0x29; 16]);
    let mut coordinator = ProcessMigrationCoordinator::initiate(base_initiate_request(
        process_id,
        vec![],
        PinnedSessionPolicy::RefuseMigration,
    ))
    .unwrap();

    let quiesce = sample_quiesce_proof(process_id);
    assert_eq!(
        coordinator.record_quiesce(quiesce.clone()).unwrap(),
        MigrationStepDecision::Advanced(ProcessMigrationStep::Quiesced)
    );
    assert_eq!(
        coordinator.record_quiesce(quiesce.clone()).unwrap(),
        MigrationStepDecision::Replayed(ProcessMigrationStep::Quiesced)
    );
    let mut conflicting_quiesce = quiesce;
    conflicting_quiesce.quiesced_fibers = 99;
    assert_eq!(
        coordinator.record_quiesce(conflicting_quiesce),
        Err(ProcessMigrationError::ConflictingStepProof {
            step: ProcessMigrationStep::Quiesced,
        })
    );

    let checkpoint = sample_checkpoint_proof(process_id, vec![]);
    assert_eq!(
        coordinator.record_checkpoint(checkpoint.clone()).unwrap(),
        MigrationStepDecision::Advanced(ProcessMigrationStep::Checkpointed)
    );
    assert_eq!(
        coordinator.record_checkpoint(checkpoint.clone()).unwrap(),
        MigrationStepDecision::Replayed(ProcessMigrationStep::Checkpointed)
    );
    let mut conflicting_checkpoint = checkpoint;
    conflicting_checkpoint.checkpointed_at_ms = 9_999;
    assert_eq!(
        coordinator.record_checkpoint(conflicting_checkpoint),
        Err(ProcessMigrationError::ConflictingStepProof {
            step: ProcessMigrationStep::Checkpointed,
        })
    );

    let fence = sample_source_fence_proof(process_id, [0xF1; 32]);
    assert_eq!(
        coordinator.record_source_fence(fence.clone()).unwrap(),
        MigrationStepDecision::Advanced(ProcessMigrationStep::SourceFenced)
    );
    assert_eq!(
        coordinator.record_source_fence(fence.clone()).unwrap(),
        MigrationStepDecision::Replayed(ProcessMigrationStep::SourceFenced)
    );
    let mut conflicting_fence = fence;
    conflicting_fence.source_cell_epoch = 88;
    assert_eq!(
        coordinator.record_source_fence(conflicting_fence),
        Err(ProcessMigrationError::ConflictingStepProof {
            step: ProcessMigrationStep::SourceFenced,
        })
    );

    let caps = sample_capability_rebind_proof(process_id);
    assert_eq!(
        coordinator.record_capability_rebind(caps.clone()).unwrap(),
        MigrationStepDecision::Advanced(ProcessMigrationStep::CapabilitiesRebound)
    );
    assert_eq!(
        coordinator.record_capability_rebind(caps).unwrap(),
        MigrationStepDecision::Replayed(ProcessMigrationStep::CapabilitiesRebound)
    );

    let leases = sample_lease_regrant_proof(process_id);
    assert_eq!(
        coordinator
            .record_lease_close_regrant(leases.clone())
            .unwrap(),
        MigrationStepDecision::Advanced(ProcessMigrationStep::LeasesRegranted)
    );
    assert_eq!(
        coordinator.record_lease_close_regrant(leases).unwrap(),
        MigrationStepDecision::Replayed(ProcessMigrationStep::LeasesRegranted)
    );

    let restore = sample_target_restore_proof(process_id, [0xCC; 32]);
    assert_eq!(
        coordinator.record_target_restore(restore.clone()).unwrap(),
        MigrationStepDecision::Advanced(ProcessMigrationStep::TargetRestored)
    );
    assert_eq!(
        coordinator.record_target_restore(restore).unwrap(),
        MigrationStepDecision::Replayed(ProcessMigrationStep::TargetRestored)
    );

    let receipt = coordinator.complete(1_700).unwrap();
    let replayed_receipt = coordinator.complete(1_700).unwrap();
    assert_eq!(receipt, replayed_receipt);
    assert_eq!(
        coordinator.complete(1_701),
        Err(ProcessMigrationError::ConflictingStepProof {
            step: ProcessMigrationStep::Completed,
        })
    );
}

#[test]
fn pre_fence_abort_transitions_to_aborted_and_blocks_further_steps() {
    let process_id = ProcessId::from_bytes([0x2A; 16]);
    let mut coordinator = ProcessMigrationCoordinator::initiate(base_initiate_request(
        process_id,
        vec![],
        PinnedSessionPolicy::RefuseMigration,
    ))
    .unwrap();
    coordinator
        .record_quiesce(sample_quiesce_proof(process_id))
        .unwrap();
    coordinator
        .record_checkpoint(sample_checkpoint_proof(process_id, vec![]))
        .unwrap();

    let outcome = coordinator
        .abort_before_target_restore("target cell unreachable before source fence", 1_250)
        .expect("abort before source fence should transition to AbortedPreFence");

    assert_eq!(coordinator.current_step(), ProcessMigrationStep::Aborted);
    assert_eq!(
        outcome,
        MigrationAbortOutcome::AbortedPreFence {
            migration_id: [0x11; 16],
            process_id,
            aborted_at_step: ProcessMigrationStep::Checkpointed,
            reason: "target cell unreachable before source fence".to_string(),
            aborted_at_ms: 1_250,
        }
    );

    assert_eq!(
        coordinator
            .abort_before_target_restore("target cell unreachable before source fence", 1_250)
            .unwrap(),
        outcome
    );

    assert_eq!(
        coordinator.record_source_fence(sample_source_fence_proof(process_id, [0xF1; 32])),
        Err(ProcessMigrationError::InvalidStepOrder {
            current_step: ProcessMigrationStep::Aborted,
            attempted_step: ProcessMigrationStep::SourceFenced,
        })
    );
}

#[test]
fn post_fence_abort_transitions_to_quarantined_and_abort_after_target_restore_is_rejected() {
    let process_id = ProcessId::from_bytes([0x2B; 16]);
    let mut coordinator = ProcessMigrationCoordinator::initiate(base_initiate_request(
        process_id,
        vec![],
        PinnedSessionPolicy::RefuseMigration,
    ))
    .unwrap();
    coordinator
        .record_quiesce(sample_quiesce_proof(process_id))
        .unwrap();
    coordinator
        .record_checkpoint(sample_checkpoint_proof(process_id, vec![]))
        .unwrap();
    coordinator
        .record_source_fence(sample_source_fence_proof(process_id, [0xF1; 32]))
        .unwrap();
    coordinator
        .record_capability_rebind(sample_capability_rebind_proof(process_id))
        .unwrap();

    let quarantine = coordinator
        .abort_before_target_restore("lease regrant rejected on target cell", 1_450)
        .expect("abort after source fence must transition to QuarantinedPostFence");

    assert_eq!(
        coordinator.current_step(),
        ProcessMigrationStep::Quarantined
    );
    assert_eq!(
        quarantine,
        MigrationAbortOutcome::QuarantinedPostFence {
            migration_id: [0x11; 16],
            process_id,
            quarantined_at_step: ProcessMigrationStep::CapabilitiesRebound,
            source_fencing_token: [0xF1; 32],
            reason: "lease regrant rejected on target cell".to_string(),
            quarantined_at_ms: 1_450,
        }
    );

    let mut restored_coord = ProcessMigrationCoordinator::initiate(base_initiate_request(
        process_id,
        vec![],
        PinnedSessionPolicy::RefuseMigration,
    ))
    .unwrap();
    restored_coord
        .record_quiesce(sample_quiesce_proof(process_id))
        .unwrap();
    restored_coord
        .record_checkpoint(sample_checkpoint_proof(process_id, vec![]))
        .unwrap();
    restored_coord
        .record_source_fence(sample_source_fence_proof(process_id, [0xF1; 32]))
        .unwrap();
    restored_coord
        .record_capability_rebind(sample_capability_rebind_proof(process_id))
        .unwrap();
    restored_coord
        .record_lease_close_regrant(sample_lease_regrant_proof(process_id))
        .unwrap();
    restored_coord
        .record_target_restore(sample_target_restore_proof(process_id, [0xCC; 32]))
        .unwrap();

    assert_eq!(
        restored_coord.abort_before_target_restore("too late", 1_650),
        Err(ProcessMigrationError::CannotAbortAfterTargetRestore {
            current_step: ProcessMigrationStep::TargetRestored,
        })
    );
}
