//! C-SHARD Phase 1 (`CS1-B` + `CS1-C`): `[DIST-TASK-001]`, `[DIST-TASK-002]`,
//! and `[DIST-TASK-004]` conformance and gap-closure integration tests.
//!
//! Covers:
//! 1. `[DIST-TASK-001]` (`CS1-B`): Participant registration before operation
//!    admission (`verify_dist_task_operation_admission`), same-transaction
//!    atomic registration + admission (`admit_operation_binding_with_authority`
//!    / `admit_verified_participant_operation`), refusal of unregistered
//!    endpoints, refusal when the registry is frozen (`FrozenForPermit`,
//!    `FrozenForTakeover`, `Superseded`), permit issuance freezing the
//!    participant registry (`FrozenForPermit`), and bitwise equality of
//!    `(participant_registry_generation, participant_registry_root)` across
//!    `TaskWriteSet`, `CommitPermit`, and every `EffectPermit`.
//! 2. `[DIST-TASK-004]` (`CS1-B` / `CS1-C`): Non-`Active`
//!    `TaskAuthorityAssignment` states (`TakeoverPending`, `Fenced`, `Expired`,
//!    `Quarantined`) refuse participant registration, operation admission,
//!    `TaskWriteSet` sealing, `CommitPermit` issuance, and terminal progression;
//!    `TaskPermitClosureReceipt` (`FAILED_BEFORE_EFFECT` /
//!    `CANCELLED_BEFORE_EFFECT` with unchanged `TaskHead`) forbids in-place
//!    unfreezing of `FrozenForPermit` (including raw SQLite trigger enforcement),
//!    marks the closed generation `Superseded`, advances to a new `Open`
//!    `participant_registry_generation + 1` with
//!    `prior_participant_registry_root = Some(old_frozen_root)`, and rotates
//!    the active `TaskAuthorityAssignment` so a retry attempt can register new
//!    endpoints and re-compete.
//! 3. `[DIST-TASK-002]` (`CS1-C`): Takeover fence union set
//!    (`frozen registry ∪ durable outstanding operation participants`) covers
//!    all six canonical architecture §26.1 endpoint participant types
//!    (`TaskStore`, `ArtifactHead`, `SemanticAdmission`, `ChannelTopic`,
//!    `DriverGateway`, `ResourceLedger`) plus `OperationBinding` across
//!    `artifact_writes`, `semantic_reads`, `semantic_appends`,
//!    `resource_reservations`, and `effect_endpoints`; requires principal-signed
//!    barrier observations across the full union before takeover completion;
//!    and automatically links `QuarantineReceiptRecord.outstanding_effect_quarantine_root`
//!    into `TaskAuthorityTakeoverReceipt.outstanding_effect_quarantine_root`
//!    while rejecting conflicting roots fail-closed.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use ed25519_dalek::{Signer, SigningKey};
use nlos_channel::{ChannelAuthority, ChannelDecision, CreateChannelRequest};
use nlos_identity::{BootstrapPrincipalRequest, IdentityAuthority, KeyPurpose};
use nlos_resource::{
    AccountRecord, CreateAccountRequest, CreateQuoteRequest, DriverRecord, QuoteRecord,
    RegisterDriverRequest, ReservationRecord, ReserveRequest, ResourceAuthority, ResourceDemand,
};
use nlos_semantic::SemanticAuthority;
use nlos_task::{
    AttemptSpec, Authorities, AuthorityAssignmentState, AuthorityLeaseCloseRequest,
    AuthorityLeaseDispatchRequest, AuthorityLeaseEffectPermitRequest,
    AuthorityLeaseFinalizeRequest, AuthorityLeaseOutcomeRequest, AuthorityLeasePermitRequest,
    AuthorityLeaseRecord, AuthorityLeaseRequest, AuthorityLeaseTakeoverFenceRequest,
    AuthorityTakeoverBarrierCoverageState, AuthorityTakeoverBarrierReceiptRequest,
    AuthorityTakeoverBarrierReceiptState, AuthorityTakeoverReceiptRecord,
    AuthorityTakeoverReceiptState, BarrierObservationSignature, ClosePermitDecision,
    ClosePermitRequest, CompleteAuthorityTakeoverRequest, DispatchRequest,
    DistTaskOperationAdmissionRequest, EffectPermitDecision, EffectPermitRequest, FinalizeDecision,
    FinalizeRequest, FinalizeRequestV3, LogicalEffectDescriptor, NoEffectReason, NoEffectRequest,
    Outcome, OutcomeRequest, ParticipantRecord, ParticipantRegistryBinding,
    ParticipantRegistryState, ParticipantType, PermitClosureOutcome,
    PermitClosureRegistryReopenRequest, PermitDecision, PermitRequest, PlannedEffect,
    ReceiptOutcome, SnapshotBundle, SnapshotConsistency, SqliteTaskAuthority,
    TaskSnapshotReceiptSpec, TaskSpec, TaskStoreError, TaskWriteSetArtifactRead,
    TaskWriteSetArtifactWriteRequest, TaskWriteSetDecision, TaskWriteSetEffectEndpointRequest,
    TaskWriteSetRequest, TaskWriteSetResourceReservationRequest, TaskWriteSetSemanticAppendRequest,
    TaskWriteSetSemanticRequiredDurability, TaskWriteSetSemanticTarget,
    barrier_observation_signature_message, empty_effect_history_root,
};
use nlos_types::{
    ArtifactId, CallId, CancellationScopeId, ExecutionFiberId, Generation, IdempotencyKey,
    NamespaceId, OperationId, ProcessId, ReceiptId, SemanticEventId, TaskAttemptId, TaskId,
    TaskSnapshotId,
};
use rusqlite::Connection;

static NEXT: AtomicU64 = AtomicU64::new(1);

struct Fixture {
    task_path: PathBuf,
    semantic_root: PathBuf,
    resource_root: PathBuf,
    artifact_root: PathBuf,
    operation_root: PathBuf,
    channel_root: PathBuf,
    identity_root: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "nlos-task-cs1bc-{label}-{}-{suffix}",
            std::process::id()
        ));
        Self {
            task_path: base.with_extension("sqlite3"),
            semantic_root: base.with_extension("semantic"),
            resource_root: base.with_extension("resource"),
            artifact_root: base.with_extension("artifact"),
            operation_root: base.with_extension("operation"),
            channel_root: base.with_extension("channel"),
            identity_root: base.with_extension("identity"),
        }
    }

    fn open_task(&self) -> SqliteTaskAuthority {
        SqliteTaskAuthority::open(&self.task_path).expect("open task authority")
    }

    fn operation_path(&self) -> PathBuf {
        fs::create_dir_all(&self.operation_root).expect("create operation root");
        self.operation_root.join("authority.sqlite3")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let mut path = self.task_path.as_os_str().to_os_string();
            path.push(suffix);
            let _ = fs::remove_file(path);
        }
        for root in [
            &self.semantic_root,
            &self.resource_root,
            &self.artifact_root,
            &self.operation_root,
            &self.channel_root,
            &self.identity_root,
        ] {
            let _ = fs::remove_dir_all(root);
        }
    }
}

struct BarrierSigner {
    key: SigningKey,
    binding: nlos_identity::IdentityBinding,
}

fn bootstrap_barrier_signer(
    identity: &IdentityAuthority,
    seed: u8,
    purpose: KeyPurpose,
) -> BarrierSigner {
    let key = SigningKey::from_bytes(&[seed; 32]);
    let binding = identity
        .bootstrap_principal(BootstrapPrincipalRequest {
            principal_profile_digest: [seed.wrapping_add(1); 32],
            control_domain_policy_digest: [seed.wrapping_add(2); 32],
            public_key: key.verifying_key().to_bytes(),
            key_purpose: purpose,
            key_valid_from_ms: 0,
            key_valid_until_ms: 100_000,
            idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(3); 16]),
            created_at_ms: 0,
        })
        .expect("bootstrap signer")
        .binding();
    BarrierSigner { key, binding }
}

fn acquire_lease(
    authority: &SqliteTaskAuthority,
    holder_seed: u8,
    key_seed: u8,
    at_ms: i64,
    ttl_ms: i64,
) -> AuthorityLeaseRecord {
    authority
        .acquire_authority_lease(AuthorityLeaseRequest {
            holder_id: ProcessId::from_bytes([holder_seed; 16]),
            idempotency_key: IdempotencyKey::from_bytes([key_seed; 16]),
            requested_at_ms: at_ms,
            ttl_ms,
        })
        .expect("acquire authority lease")
        .record()
}

fn register_task_and_receipted_attempt(
    authority: &SqliteTaskAuthority,
    task_seed: u8,
    attempt_seed: u8,
    expected_head_commit_seq: u64,
    effect_history_root: [u8; 32],
    retry_fence_epoch: u64,
    at_ms: i64,
) -> AttemptSpec {
    let task_id = TaskId::from_bytes([task_seed; 16]);
    if authority.inspect_task(task_id).is_err() {
        authority
            .register_task(TaskSpec {
                application_id: None,
                plan_revision: None,
                task_id,
                task_generation: Generation::INITIAL,
                registered_at_ms: at_ms,
            })
            .expect("register task");
    }
    let snapshot = SnapshotBundle {
        snapshot_id: TaskSnapshotId::from_bytes([attempt_seed.wrapping_add(1); 16]),
        snapshot_digest: [attempt_seed.wrapping_add(2); 32],
        expected_head_commit_seq,
        effect_history_root,
        retry_fence_epoch,
    };
    let snapshot_receipt_id = ReceiptId::from_bytes([attempt_seed.wrapping_add(3); 16]);
    authority
        .register_snapshot_receipt(TaskSnapshotReceiptSpec {
            task_id,
            snapshot,
            receipt_id: snapshot_receipt_id,
            builder_id: [attempt_seed.wrapping_add(4); 16],
            builder_version_digest: [attempt_seed.wrapping_add(5); 32],
            per_authority_checkpoint_receipts: vec![ReceiptId::from_bytes(
                [attempt_seed.wrapping_add(6); 16],
            )],
            dependency_closure_root: [attempt_seed.wrapping_add(7); 32],
            semantic_resolver_digest: [attempt_seed.wrapping_add(8); 32],
            canonical_iteration_digest: [attempt_seed.wrapping_add(9); 32],
            achieved_consistency: SnapshotConsistency::Causal,
            built_at_ms: at_ms,
            authority_id: [attempt_seed.wrapping_add(10); 16],
            key_id: [attempt_seed.wrapping_add(11); 16],
            signature: [attempt_seed.wrapping_add(12); 64],
        })
        .expect("register snapshot receipt");
    let attempt = AttemptSpec {
        task_id,
        attempt_id: TaskAttemptId::from_bytes([attempt_seed; 16]),
        attempt_generation: Generation::INITIAL,
        snapshot,
        cancellation_scope_id: CancellationScopeId::from_bytes([attempt_seed.wrapping_add(13); 16]),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes([attempt_seed.wrapping_add(14); 16]),
        registered_at_ms: at_ms + 1,
    };
    authority
        .register_attempt_with_snapshot_receipt(attempt, snapshot_receipt_id)
        .expect("register attempt with snapshot receipt");
    attempt
}

fn planned_effect(task_id: TaskId, slot: u64, seed: u8) -> PlannedEffect {
    PlannedEffect {
        descriptor: LogicalEffectDescriptor {
            task_id,
            task_generation: Generation::INITIAL,
            intent_spec_id: [seed; 32],
            stable_action_slot: slot,
            target_authority_object_id: [seed.wrapping_add(1); 32],
            effect_class: 1,
            idempotency_scope: 1,
        },
        required: false,
        required_condition_digest: None,
        success_criteria_digest: [seed.wrapping_add(2); 32],
        action_proposal_digest: [seed.wrapping_add(3); 32],
    }
}

type SemanticSeed = (
    SemanticAuthority,
    SemanticEventId,
    ReceiptId,
    ReceiptId,
    NamespaceId,
);

fn seed_semantic_authority(root: &Path) -> SemanticSeed {
    let semantic = SemanticAuthority::open(root).expect("open Semantic authority");
    let event_id = SemanticEventId::from_bytes([0x70; 32]);
    let admission_receipt_id = ReceiptId::from_bytes([0x71; 16]);
    let durability_receipt_id = ReceiptId::from_bytes([0x72; 16]);
    let target = NamespaceId::from_bytes([0x73; 16]);
    let raw = Connection::open(root.join("semantic-authority.db")).expect("open raw Semantic db");
    raw.execute(
        "INSERT INTO content_objects (content_digest, media_type, exact_bytes)
         VALUES (?1, ?2, ?3)",
        rusqlite::params![[0x74u8; 32].as_slice(), "text/plain", b"semantic"],
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
            [0x75u8, 0x76, 0x77].as_slice(),
            target.as_bytes().as_slice(),
            [0x78u8; 16].as_slice(),
            [0x79u8; 16].as_slice(),
            [0x7au8; 16].as_slice(),
            [0xa0u8; 16].as_slice(),
            [0x74u8; 32].as_slice(),
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
            [0x7bu8; 16].as_slice(),
            [0x7cu8; 16].as_slice(),
            [0x7du8; 16].as_slice(),
            [0x7eu8; 64].as_slice(),
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
            [0x7fu8; 32].as_slice(),
            [0x80u8; 64].as_slice(),
        ],
    )
    .expect("insert durability");
    drop(raw);
    (
        semantic,
        event_id,
        admission_receipt_id,
        durability_receipt_id,
        target,
    )
}

struct ResourceOwnerFixture {
    authority: ResourceAuthority,
    driver: DriverRecord,
    account: AccountRecord,
}

impl ResourceOwnerFixture {
    fn new(root: &Path, seed: u8) -> Self {
        let authority = ResourceAuthority::open(root).expect("open resource authority");
        let driver = authority
            .register_driver(RegisterDriverRequest {
                profile_digest: [seed; 32],
                idempotency_key: IdempotencyKey::from_bytes([seed ^ 0x01; 16]),
                created_at_ms: 1_000,
            })
            .expect("register driver")
            .record();
        let account = authority
            .create_account(CreateAccountRequest {
                initial_credit: 1_000,
                idempotency_key: IdempotencyKey::from_bytes([seed ^ 0x02; 16]),
                created_at_ms: 1_000,
            })
            .expect("create account");
        Self {
            authority,
            driver,
            account,
        }
    }

    fn quote(&self, seed: u8, upper_bound: u64) -> QuoteRecord {
        self.authority
            .create_quote(CreateQuoteRequest {
                driver_id: self.driver.driver_id,
                driver_generation: self.driver.generation,
                driver_fencing_token: self.driver.fencing_token,
                operation_proposal_digest: [seed; 32],
                pricing_version: [seed ^ 0x11; 32],
                upper_bound,
                demand_capacity: ResourceDemand::default(),
                valid_until_ms: 90_000,
                idempotency_key: IdempotencyKey::from_bytes([seed ^ 0x12; 16]),
                created_at_ms: 1_000,
            })
            .expect("create quote")
            .record()
    }

    fn reserve(
        &self,
        quote: &QuoteRecord,
        call_id: CallId,
        operation_id: OperationId,
        key: IdempotencyKey,
    ) -> ReservationRecord {
        self.authority
            .reserve(ReserveRequest {
                account_id: self.account.account_id,
                quote_id: quote.quote_id,
                call_id,
                operation_id,
                idempotency_key: key,
                demand: ResourceDemand::default(),
                reserved_at_ms: 1_100,
            })
            .expect("reserve")
            .record()
    }
}

fn record_signed_observation(
    authority: &SqliteTaskAuthority,
    identity: &IdentityAuthority,
    signer: &BarrierSigner,
    takeover: &AuthorityTakeoverReceiptRecord,
    participant: ParticipantRecord,
    seed: u8,
) {
    let fence_set_root = takeover
        .exact_fence_set_root
        .expect("exact fence set root must be present");
    let remote_receipt_id = ReceiptId::from_bytes([seed; 16]);
    let barrier_digest = [seed.wrapping_add(1); 32];
    let observed_at_ms = 2_200;
    let preimage = barrier_observation_signature_message(
        takeover.receipt_id,
        &participant,
        remote_receipt_id,
        barrier_digest,
        fence_set_root,
    );
    let signature = signer.key.sign(&preimage).to_bytes();
    let observation = authority
        .record_authority_takeover_barrier_receipt_signed(
            identity,
            AuthorityTakeoverBarrierReceiptRequest {
                takeover_receipt_id: takeover.receipt_id,
                participant,
                remote_receipt_id,
                barrier_digest,
                observed_at_ms,
            },
            BarrierObservationSignature {
                issuer: signer.binding.principal_id,
                control_domain_id: signer.binding.control_domain_id,
                key_id: signer.binding.key_id,
                signature,
            },
        )
        .expect("record signed barrier observation");
    assert_eq!(
        observation.state,
        AuthorityTakeoverBarrierReceiptState::Observed
    );
    assert!(observation.signer.is_some());
}

#[test]
#[allow(clippy::too_many_lines)]
fn cs1b_dist_task_001_operation_admission_and_same_transaction_registration() {
    let fixture = Fixture::new("cs1b-op-admission");
    let authority = fixture.open_task();
    let operation_store =
        nlos_store::SqliteOperationStore::open(fixture.operation_path()).expect("operation store");
    let op_one = nlos_operation::OperationSpec {
        operation_id: OperationId::from_bytes([0x31; 16]),
        generation: Generation::INITIAL,
        owner_fiber: nlos_runtime::FiberHandle {
            fiber_id: ExecutionFiberId::from_bytes([0x32; 16]),
            generation: Generation::INITIAL,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes([0x33; 16]),
        cancellation_generation: Generation::INITIAL,
    };
    operation_store.register(op_one).expect("register op one");
    let op_one_proof = operation_store
        .inspect_endpoint_proof(nlos_operation::OperationHandle {
            operation_id: op_one.operation_id,
            generation: op_one.generation,
        })
        .expect("op one proof");
    let op_one_participant = ParticipantRecord {
        participant_type: ParticipantType::OperationBinding,
        participant_id: op_one_proof.participant_id,
        participant_generation: op_one_proof.participant_generation,
        admission_receipt_id: op_one_proof.admission_receipt_id,
    };

    let lease_one = acquire_lease(&authority, 1, 0x11, 100, 10_000);
    let attempt_one = register_task_and_receipted_attempt(
        &authority,
        0x21,
        0x22,
        0,
        empty_effect_history_root(),
        0,
        110,
    );

    // Issue and immediately close a pure no-effect permit so an Active
    // TaskAuthorityAssignment exists together with an OPEN registry generation 2.
    let permit_one = match authority
        .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
            permit: PermitRequest {
                task_id: attempt_one.task_id,
                attempt_id: attempt_one.attempt_id,
                attempt_generation: attempt_one.attempt_generation,
                write_set_root: [0x41; 32],
                planned_effects: Vec::new(),
                idempotency_key: IdempotencyKey::from_bytes([0x42; 16]),
                valid_until_ms: 10_000,
                requested_at_ms: 120,
            },
            lease: lease_one,
        })
        .expect("issue first permit")
    {
        PermitDecision::Issued(permit) => *permit,
        other => panic!("expected Issued, got {other:?}"),
    };
    authority
        .close_permit_with_authority_lease(AuthorityLeaseCloseRequest {
            close: ClosePermitRequest {
                task_id: attempt_one.task_id,
                attempt_id: attempt_one.attempt_id,
                attempt_generation: attempt_one.attempt_generation,
                permit_id: permit_one.permit_id,
                outcome: PermitClosureOutcome::FailedBeforeEffect,
                fenced_participant_digest: [0; 32],
                closed_at_ms: 130,
            },
            lease: lease_one,
        })
        .expect("close first permit");

    let task_after_close = authority
        .inspect_task(attempt_one.task_id)
        .expect("task after close");
    let open_registry = authority
        .inspect_participant_registry(attempt_one.task_id)
        .expect("open registry after close");
    assert_eq!(open_registry.state, ParticipantRegistryState::Open);
    assert_eq!(open_registry.generation, 2);
    let open_binding = ParticipantRegistryBinding {
        generation: open_registry.generation,
        root: open_registry.root,
    };

    // 1. `verify_dist_task_operation_admission` refuses an unregistered endpoint participant.
    assert!(matches!(
        authority.verify_dist_task_operation_admission(DistTaskOperationAdmissionRequest {
            task_id: attempt_one.task_id,
            expected_registry_binding: open_binding,
            lease: lease_one,
            expected_authority_term: lease_one.term,
            expected_control_epoch: task_after_close.control_epoch,
            participant: op_one_participant,
            admitted_at_ms: 140,
        }),
        Err(TaskStoreError::TaskWriteSetConflict { .. })
    ));

    // 2. Term mismatch or control_epoch mismatch is refused fail-closed.
    assert!(matches!(
        authority.admit_verified_participant_operation(DistTaskOperationAdmissionRequest {
            task_id: attempt_one.task_id,
            expected_registry_binding: open_binding,
            lease: lease_one,
            expected_authority_term: lease_one.term + 1,
            expected_control_epoch: task_after_close.control_epoch,
            participant: op_one_participant,
            admitted_at_ms: 141,
        }),
        Err(TaskStoreError::AuthorityLeaseFenced)
    ));
    assert!(matches!(
        authority.admit_verified_participant_operation(DistTaskOperationAdmissionRequest {
            task_id: attempt_one.task_id,
            expected_registry_binding: open_binding,
            lease: lease_one,
            expected_authority_term: lease_one.term,
            expected_control_epoch: task_after_close.control_epoch + 1,
            participant: op_one_participant,
            admitted_at_ms: 141,
        }),
        Err(TaskStoreError::AuthorityLeaseBindingMismatch)
    ));

    // 3. `admit_operation_binding_with_authority` atomically registers the verified
    //    participant in the OPEN registry (gen 2 -> 3) AND rotates the Active assignment
    //    inside the same authority transaction.
    let admitted = authority
        .admit_operation_binding_with_authority(
            &operation_store,
            attempt_one.task_id,
            open_binding,
            lease_one,
            lease_one.term,
            task_after_close.control_epoch,
            op_one.operation_id,
            op_one.generation,
            145,
        )
        .expect("atomic same-transaction registration and operation admission");
    assert!(admitted.was_newly_registered);
    assert_eq!(admitted.participant, op_one_participant);
    assert_eq!(admitted.participant_registry_binding.generation, 3);

    let registry_gen3 = authority
        .inspect_participant_registry(attempt_one.task_id)
        .expect("registry gen 3");
    assert_eq!(registry_gen3.state, ParticipantRegistryState::Open);
    assert_eq!(registry_gen3.generation, 3);
    assert_eq!(
        registry_gen3.prior_participant_registry_root,
        Some(open_binding.root)
    );
    assert!(registry_gen3.participants.contains(&op_one_participant));

    let active_assignment = authority
        .inspect_authority_assignment(attempt_one.task_id)
        .expect("active assignment at gen 3");
    assert_eq!(active_assignment.state, AuthorityAssignmentState::Active);
    assert_eq!(
        active_assignment.participant_registry_binding,
        admitted.participant_registry_binding
    );

    // 4. Subsequent `verify_dist_task_operation_admission` succeeds with `was_newly_registered == false`.
    let verified = authority
        .verify_dist_task_operation_admission(DistTaskOperationAdmissionRequest {
            task_id: attempt_one.task_id,
            expected_registry_binding: admitted.participant_registry_binding,
            lease: lease_one,
            expected_authority_term: lease_one.term,
            expected_control_epoch: task_after_close.control_epoch,
            participant: op_one_participant,
            admitted_at_ms: 146,
        })
        .expect("verify already-registered operation admission");
    assert!(!verified.was_newly_registered);
    assert_eq!(verified.assignment_id, active_assignment.assignment_id);
}

#[test]
#[allow(clippy::too_many_lines)]
fn cs1b_dist_task_001_permit_freeze_and_bitwise_effect_permit_binding() {
    let fixture = Fixture::new("cs1b-permit-freeze");
    let authority = fixture.open_task();
    let artifact = nlos_artifact::ArtifactStore::open(&fixture.artifact_root).expect("artifact");
    let write_artifact = ArtifactId::from_bytes([0x51; 16]);
    artifact
        .create_artifact(nlos_artifact::CreateArtifactSpec {
            artifact_id: write_artifact,
            idempotency_key: IdempotencyKey::from_bytes([0x52; 16]),
            content_type: "application/octet-stream".to_owned(),
            application_id: None,
            owner: None,
            created_at_ms: 100,
        })
        .expect("create write artifact");
    let extra_artifact = ArtifactId::from_bytes([0x53; 16]);
    artifact
        .create_artifact(nlos_artifact::CreateArtifactSpec {
            artifact_id: extra_artifact,
            idempotency_key: IdempotencyKey::from_bytes([0x54; 16]),
            content_type: "application/octet-stream".to_owned(),
            application_id: None,
            owner: None,
            created_at_ms: 101,
        })
        .expect("create extra artifact");
    let (semantic, _, _, _, _) = seed_semantic_authority(&fixture.semantic_root);
    let resource = ResourceOwnerFixture::new(&fixture.resource_root, 0x55);
    let channel = ChannelAuthority::open(&fixture.channel_root).expect("channel authority");
    let channel_record = match channel
        .create_channel(CreateChannelRequest {
            capacity_bytes: 4096,
            policy_digest: [0x56; 32],
            idempotency_key: IdempotencyKey::from_bytes([0x57; 16]),
            created_at_ms: 102,
        })
        .expect("create channel")
    {
        ChannelDecision::Created(record) => record,
        ChannelDecision::Replayed(_) => panic!("unexpected replay"),
    };

    let lease_one = acquire_lease(&authority, 1, 0x61, 100, 10_000);
    let attempt = register_task_and_receipted_attempt(
        &authority,
        0x62,
        0x63,
        0,
        empty_effect_history_root(),
        0,
        110,
    );

    let current_binding = |authority: &SqliteTaskAuthority| {
        let reg = authority
            .inspect_participant_registry(attempt.task_id)
            .expect("inspect registry");
        ParticipantRegistryBinding {
            generation: reg.generation,
            root: reg.root,
        }
    };

    authority
        .register_artifact_head_participant(
            &artifact,
            attempt.task_id,
            current_binding(&authority),
            write_artifact,
            120,
        )
        .expect("register artifact participant");
    authority
        .register_semantic_admission_participant(
            &semantic,
            attempt.task_id,
            current_binding(&authority),
            121,
        )
        .expect("register semantic participant");
    authority
        .register_channel_participant(
            &channel,
            attempt.task_id,
            current_binding(&authority),
            channel_record.channel_id,
            channel_record.generation,
            122,
        )
        .expect("register channel participant");
    authority
        .register_driver_gateway_participant(
            &resource.authority,
            attempt.task_id,
            current_binding(&authority),
            resource.driver.driver_id,
            resource.driver.generation,
            123,
        )
        .expect("register driver participant");
    authority
        .register_resource_ledger_participant(
            &resource.authority,
            attempt.task_id,
            current_binding(&authority),
            resource.account.account_id,
            Generation::INITIAL,
            124,
        )
        .expect("register ledger participant");

    let effects = vec![
        planned_effect(attempt.task_id, 0, 0x71),
        planned_effect(attempt.task_id, 1, 0x75),
    ];
    let write_set = match authority
        .seal_task_write_set_with_authorities_struct(
            Authorities {
                artifact: Some(&artifact),
                process: None,
                semantic: Some(&semantic),
                resource: Some(&resource.authority),
                operation: None,
                channel: Some(&channel),
            },
            TaskWriteSetRequest {
                task_id: attempt.task_id,
                attempt_id: attempt.attempt_id,
                attempt_generation: attempt.attempt_generation,
                artifact_reads: Vec::new(),
                artifact_writes: vec![TaskWriteSetArtifactWriteRequest {
                    artifact_id: write_artifact,
                    expected_head_revision: 0,
                    proposed_revision: 1,
                    content_digest: [0x81; 32],
                    size_bytes: 64,
                }],
                process_binding: None,
                semantic_reads: Vec::new(),
                semantic_appends: Vec::new(),
                resource_reservations: Vec::new(),
                planned_effects: effects.clone(),
                effect_endpoints: vec![
                    TaskWriteSetEffectEndpointRequest::SemanticAdmission { effect_seq: 0 },
                    TaskWriteSetEffectEndpointRequest::ChannelTopicBinding {
                        effect_seq: 1,
                        channel_id: channel_record.channel_id,
                        expected_channel_generation: channel_record.generation,
                    },
                ],
                idempotency_key: IdempotencyKey::from_bytes([0x82; 16]),
                sealed_at_ms: 130,
            },
        )
        .expect("seal write set")
    {
        TaskWriteSetDecision::Sealed(record) | TaskWriteSetDecision::Replayed(record) => record,
    };

    let commit_permit = match authority
        .request_commit_permit_with_authorities_and_lease(
            Authorities {
                artifact: Some(&artifact),
                process: None,
                semantic: Some(&semantic),
                resource: None,
                operation: None,
                channel: Some(&channel),
            },
            AuthorityLeasePermitRequest {
                permit: PermitRequest {
                    task_id: attempt.task_id,
                    attempt_id: attempt.attempt_id,
                    attempt_generation: attempt.attempt_generation,
                    write_set_root: write_set.write_set_root,
                    planned_effects: effects,
                    idempotency_key: IdempotencyKey::from_bytes([0x83; 16]),
                    valid_until_ms: 10_000,
                    requested_at_ms: 140,
                },
                lease: lease_one,
            },
        )
        .expect("issue commit permit")
    {
        PermitDecision::Issued(permit) => *permit,
        other => panic!("expected Issued, got {other:?}"),
    };

    // Registry is now FrozenForPermit and refuses any behind-the-permit expansion.
    let frozen_registry = authority
        .inspect_participant_registry(attempt.task_id)
        .expect("frozen registry");
    assert_eq!(
        frozen_registry.state,
        ParticipantRegistryState::FrozenForPermit
    );
    assert_eq!(
        commit_permit.participant_registry_binding,
        Some(write_set.participant_registry_binding)
    );
    assert!(matches!(
        authority.register_artifact_head_participant(
            &artifact,
            attempt.task_id,
            write_set.participant_registry_binding,
            extra_artifact,
            145,
        ),
        Err(TaskStoreError::ParticipantRegistryFrozen {
            state: ParticipantRegistryState::FrozenForPermit
        })
    ));

    // Every EffectPermit carries the exact same `(participant_registry_generation, participant_registry_root)`.
    for effect_seq in [0_u64, 1_u64] {
        let effect_permit = match authority
            .request_effect_permit_with_authority_lease(AuthorityLeaseEffectPermitRequest {
                permit: EffectPermitRequest {
                    task_id: attempt.task_id,
                    attempt_id: attempt.attempt_id,
                    attempt_generation: attempt.attempt_generation,
                    permit_id: commit_permit.permit_id,
                    permit_epoch: commit_permit.permit_epoch,
                    effect_seq,
                    idempotency_key: IdempotencyKey::from_bytes(
                        [0x90 + u8::try_from(effect_seq).unwrap(); 16],
                    ),
                    valid_until_ms: 10_000,
                    requested_at_ms: 150 + i64::try_from(effect_seq).unwrap(),
                },
                lease: lease_one,
            })
            .expect("issue effect permit")
        {
            EffectPermitDecision::Issued(issued) => *issued,
            other @ EffectPermitDecision::Replayed(_) => {
                panic!("expected Issued effect permit, got {other:?}")
            }
        };
        assert_eq!(
            effect_permit.participant_registry_binding,
            Some(write_set.participant_registry_binding),
            "EffectPermit slot {effect_seq} must carry bitwise-equal participant_registry_binding"
        );
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn cs1b_cs1c_dist_task_004_non_active_assignment_states_refuse_admission_and_permits() {
    let fixture = Fixture::new("cs1bc-non-active-states");
    let authority = fixture.open_task();
    let artifact = nlos_artifact::ArtifactStore::open(&fixture.artifact_root).expect("artifact");
    let artifact_id = ArtifactId::from_bytes([0x11; 16]);
    artifact
        .create_artifact(nlos_artifact::CreateArtifactSpec {
            artifact_id,
            idempotency_key: IdempotencyKey::from_bytes([0x12; 16]),
            content_type: "application/octet-stream".to_owned(),
            application_id: None,
            owner: None,
            created_at_ms: 100,
        })
        .expect("create artifact");
    let proof = artifact
        .inspect_head_endpoint_proof(artifact_id)
        .expect("artifact proof");
    let participant = ParticipantRecord {
        participant_type: ParticipantType::ArtifactHead,
        participant_id: proof.participant_id,
        participant_generation: proof.participant_generation,
        admission_receipt_id: proof.admission_receipt_id,
    };

    let lease_one = acquire_lease(&authority, 1, 0x21, 100, 100);
    // Test each non-Active assignment state (`TakeoverPending` = 2, `Fenced` = 3,
    // `Expired` = 4, `Quarantined` = 5) on its own fresh task so the forward-only
    // assignment state trigger is respected while verifying that every non-Active
    // assignment state refuses participant registration, operation admission,
    // write-set sealing, and commit permit issuance.
    for (idx, state) in [
        AuthorityAssignmentState::TakeoverPending,
        AuthorityAssignmentState::Fenced,
        AuthorityAssignmentState::Expired,
        AuthorityAssignmentState::Quarantined,
    ]
    .into_iter()
    .enumerate()
    {
        let base_seed = 0x30 + u8::try_from(idx * 8).unwrap();
        let attempt = register_task_and_receipted_attempt(
            &authority,
            base_seed,
            base_seed + 1,
            0,
            empty_effect_history_root(),
            0,
            110,
        );
        let permit = match authority
            .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
                permit: PermitRequest {
                    task_id: attempt.task_id,
                    attempt_id: attempt.attempt_id,
                    attempt_generation: attempt.attempt_generation,
                    write_set_root: [base_seed + 2; 32],
                    planned_effects: Vec::new(),
                    idempotency_key: IdempotencyKey::from_bytes([base_seed + 3; 16]),
                    valid_until_ms: 10_000,
                    requested_at_ms: 120,
                },
                lease: lease_one,
            })
            .expect("issue permit")
        {
            PermitDecision::Issued(permit) => *permit,
            other => panic!("expected Issued, got {other:?}"),
        };
        authority
            .close_permit_with_authority_lease(AuthorityLeaseCloseRequest {
                close: ClosePermitRequest {
                    task_id: attempt.task_id,
                    attempt_id: attempt.attempt_id,
                    attempt_generation: attempt.attempt_generation,
                    permit_id: permit.permit_id,
                    outcome: PermitClosureOutcome::FailedBeforeEffect,
                    fenced_participant_digest: [0; 32],
                    closed_at_ms: 130,
                },
                lease: lease_one,
            })
            .expect("close permit to reopen registry");

        let open_reg = authority
            .inspect_participant_registry(attempt.task_id)
            .expect("open registry");
        assert_eq!(open_reg.state, ParticipantRegistryState::Open);
        let open_binding = ParticipantRegistryBinding {
            generation: open_reg.generation,
            root: open_reg.root,
        };
        let active_assignment = authority
            .inspect_authority_assignment(attempt.task_id)
            .expect("active assignment");
        assert_eq!(active_assignment.state, AuthorityAssignmentState::Active);

        let attempt_two = register_task_and_receipted_attempt(
            &authority,
            base_seed,
            base_seed + 4,
            0,
            empty_effect_history_root(),
            0,
            135,
        );
        let state_code = match state {
            AuthorityAssignmentState::Active => unreachable!(),
            AuthorityAssignmentState::TakeoverPending => 2_i64,
            AuthorityAssignmentState::Fenced => 3_i64,
            AuthorityAssignmentState::Expired => 4_i64,
            AuthorityAssignmentState::Quarantined => 5_i64,
        };
        let raw = Connection::open(&fixture.task_path).expect("open raw db");
        raw.execute(
            "UPDATE task_authority_assignments SET assignment_state = ?1 WHERE assignment_id = ?2",
            rusqlite::params![
                state_code,
                active_assignment.assignment_id.as_bytes().as_slice()
            ],
        )
        .expect("set assignment state");
        drop(raw);

        assert_eq!(
            authority
                .inspect_authority_assignment(attempt.task_id)
                .expect("inspect assignment")
                .state,
            state
        );
        let task_now = authority.inspect_task(attempt.task_id).expect("task");

        assert!(
            matches!(
                authority.register_artifact_head_participant(
                    &artifact,
                    attempt.task_id,
                    open_binding,
                    artifact_id,
                    140,
                ),
                Err(TaskStoreError::AuthorityLeaseFenced)
            ),
            "register_artifact_head_participant must refuse in state {state:?}"
        );
        assert!(
            matches!(
                authority.admit_verified_participant_operation(DistTaskOperationAdmissionRequest {
                    task_id: attempt.task_id,
                    expected_registry_binding: open_binding,
                    lease: lease_one,
                    expected_authority_term: lease_one.term,
                    expected_control_epoch: task_now.control_epoch,
                    participant,
                    admitted_at_ms: 140,
                }),
                Err(TaskStoreError::AuthorityLeaseFenced)
            ),
            "admit_verified_participant_operation must refuse in state {state:?}"
        );
        assert!(
            matches!(
                authority.seal_task_write_set_with_authorities_struct(
                    Authorities {
                        artifact: Some(&artifact),
                        process: None,
                        semantic: None,
                        resource: None,
                        operation: None,
                        channel: None,
                    },
                    TaskWriteSetRequest {
                        task_id: attempt_two.task_id,
                        attempt_id: attempt_two.attempt_id,
                        attempt_generation: attempt_two.attempt_generation,
                        artifact_reads: Vec::new(),
                        artifact_writes: Vec::new(),
                        process_binding: None,
                        semantic_reads: Vec::new(),
                        semantic_appends: Vec::new(),
                        resource_reservations: Vec::new(),
                        planned_effects: Vec::new(),
                        effect_endpoints: Vec::new(),
                        idempotency_key: IdempotencyKey::from_bytes([0x60; 16]),
                        sealed_at_ms: 140,
                    },
                ),
                Err(TaskStoreError::AuthorityLeaseFenced)
            ),
            "seal_task_write_set must refuse in state {state:?}"
        );
        assert!(
            matches!(
                authority.request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
                    permit: PermitRequest {
                        task_id: attempt_two.task_id,
                        attempt_id: attempt_two.attempt_id,
                        attempt_generation: attempt_two.attempt_generation,
                        write_set_root: [0x61; 32],
                        planned_effects: Vec::new(),
                        idempotency_key: IdempotencyKey::from_bytes([0x62; 16]),
                        valid_until_ms: 10_000,
                        requested_at_ms: 140,
                    },
                    lease: lease_one,
                }),
                Err(TaskStoreError::AuthorityLeaseFenced)
            ),
            "request_commit_permit_with_authority_lease must refuse in state {state:?}"
        );
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn cs1c_dist_task_004_permit_closure_receipt_forbids_in_place_unfreeze_and_rotates_to_new_open_generation()
 {
    let fixture = Fixture::new("cs1c-permit-closure-reopen");
    let authority = fixture.open_task();
    let artifact = nlos_artifact::ArtifactStore::open(&fixture.artifact_root).expect("artifact");
    let artifact_one = ArtifactId::from_bytes([0x71; 16]);
    artifact
        .create_artifact(nlos_artifact::CreateArtifactSpec {
            artifact_id: artifact_one,
            idempotency_key: IdempotencyKey::from_bytes([0x72; 16]),
            content_type: "application/octet-stream".to_owned(),
            application_id: None,
            owner: None,
            created_at_ms: 100,
        })
        .expect("create artifact one");

    let lease_one = acquire_lease(&authority, 1, 0x81, 100, 10_000);
    let attempt_one = register_task_and_receipted_attempt(
        &authority,
        0x91,
        0x92,
        0,
        empty_effect_history_root(),
        0,
        110,
    );

    let effect = planned_effect(attempt_one.task_id, 0, 0x93);
    let permit_one = match authority
        .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
            permit: PermitRequest {
                task_id: attempt_one.task_id,
                attempt_id: attempt_one.attempt_id,
                attempt_generation: attempt_one.attempt_generation,
                write_set_root: [0x94; 32],
                planned_effects: vec![effect],
                idempotency_key: IdempotencyKey::from_bytes([0x95; 16]),
                valid_until_ms: 10_000,
                requested_at_ms: 120,
            },
            lease: lease_one,
        })
        .expect("issue permit one")
    {
        PermitDecision::Issued(permit) => *permit,
        other => panic!("expected Issued, got {other:?}"),
    };
    let frozen_gen1_binding = permit_one
        .participant_registry_binding
        .expect("frozen gen 1 binding");
    assert_eq!(frozen_gen1_binding.generation, 1);
    let assignment_gen1 = authority
        .inspect_authority_assignment(attempt_one.task_id)
        .expect("assignment gen 1");
    assert_eq!(
        assignment_gen1.participant_registry_binding,
        frozen_gen1_binding
    );

    // Record NoEffect on slot 0 and close with CancelledBeforeEffect while TaskHead is unchanged.
    authority
        .record_no_effect_with_authority_lease(nlos_task::AuthorityLeaseNoEffectRequest {
            no_effect: NoEffectRequest {
                task_id: attempt_one.task_id,
                attempt_id: attempt_one.attempt_id,
                attempt_generation: attempt_one.attempt_generation,
                permit_id: permit_one.permit_id,
                permit_epoch: permit_one.permit_epoch,
                effect_seq: 0,
                reason: NoEffectReason::CancelledBeforeDispatch,
                dispatch_token: None,
                recorded_at_ms: 125,
            },
            lease: lease_one,
        })
        .expect("record no effect");

    let closure_receipt = match authority
        .close_permit_with_authority_lease(AuthorityLeaseCloseRequest {
            close: ClosePermitRequest {
                task_id: attempt_one.task_id,
                attempt_id: attempt_one.attempt_id,
                attempt_generation: attempt_one.attempt_generation,
                permit_id: permit_one.permit_id,
                outcome: PermitClosureOutcome::CancelledBeforeEffect,
                fenced_participant_digest: [0; 32],
                closed_at_ms: 130,
            },
            lease: lease_one,
        })
        .expect("close permit")
    {
        ClosePermitDecision::Closed(receipt) => *receipt,
        other => panic!("expected Closed, got {other:?}"),
    };
    assert_eq!(
        closure_receipt.outcome,
        ReceiptOutcome::CancelledBeforeEffect
    );
    assert_eq!(
        closure_receipt.new_head_commit_seq,
        closure_receipt.prior_head_commit_seq
    );

    // Raw SQLite trigger forbids unfreezing generation 1 once it has been superseded by permit closure.
    let raw = Connection::open(&fixture.task_path).expect("open raw db");
    assert!(
        raw.execute(
            "UPDATE task_participant_registries
             SET registry_state = 1
             WHERE task_id = ?1 AND registry_generation = X'0000000000000001'",
            [attempt_one.task_id.as_bytes().as_slice()],
        )
        .is_err(),
        "in-place unfreezing of closed permit registry generation 1 must be rejected by SQLite trigger"
    );
    drop(raw);

    // Verify via `reopen_participant_registry_after_permit_closure` and raw table inspection:
    // - Generation 1 is Superseded (4), never unfrozen in-place.
    // - Generation 2 is Open (1) with `prior_participant_registry_root == Some(frozen_gen1_binding.root)`.
    // - Assignment 1 is Fenced (3) and a new Active (1) assignment is bound to generation 2.
    let reopen_record = authority
        .reopen_participant_registry_after_permit_closure(PermitClosureRegistryReopenRequest {
            task_id: attempt_one.task_id,
            closure_receipt_id: closure_receipt.receipt_id,
            lease: Some(lease_one),
            reopened_at_ms: 131,
        })
        .expect("inspect/reopen after permit closure");
    assert_eq!(
        reopen_record.old_frozen_registry_binding,
        frozen_gen1_binding
    );
    assert_eq!(reopen_record.reopened_registry_binding.generation, 2);
    assert_eq!(
        reopen_record.fenced_assignment_id,
        Some(assignment_gen1.assignment_id)
    );
    assert!(reopen_record.active_assignment_id.is_some());

    let current_registry = authority
        .inspect_participant_registry(attempt_one.task_id)
        .expect("current registry after closure");
    assert_eq!(current_registry.state, ParticipantRegistryState::Open);
    assert_eq!(current_registry.generation, 2);
    assert_eq!(
        current_registry.prior_participant_registry_root,
        Some(frozen_gen1_binding.root)
    );

    // Register a new participant in the new OPEN generation 2 -> advances to generation 3,
    // seal a new write set against the unchanged TaskHead (commit_seq = 0), and win + commit a new permit!
    let reg_gen3 = authority
        .register_artifact_head_participant(
            &artifact,
            attempt_one.task_id,
            reopen_record.reopened_registry_binding,
            artifact_one,
            135,
        )
        .expect("register new participant after permit closure")
        .registry()
        .clone();
    assert_eq!(reg_gen3.generation, 3);
    assert_eq!(reg_gen3.state, ParticipantRegistryState::Open);

    let attempt_two = register_task_and_receipted_attempt(
        &authority,
        0x91,
        0x98,
        0,
        empty_effect_history_root(),
        0,
        140,
    );
    let write_set_two = match authority
        .seal_task_write_set_with_authorities_struct(
            Authorities {
                artifact: Some(&artifact),
                process: None,
                semantic: None,
                resource: None,
                operation: None,
                channel: None,
            },
            TaskWriteSetRequest {
                task_id: attempt_two.task_id,
                attempt_id: attempt_two.attempt_id,
                attempt_generation: attempt_two.attempt_generation,
                artifact_reads: Vec::new(),
                artifact_writes: vec![TaskWriteSetArtifactWriteRequest {
                    artifact_id: artifact_one,
                    expected_head_revision: 0,
                    proposed_revision: 1,
                    content_digest: [0x99; 32],
                    size_bytes: 32,
                }],
                process_binding: None,
                semantic_reads: Vec::new(),
                semantic_appends: Vec::new(),
                resource_reservations: Vec::new(),
                planned_effects: Vec::new(),
                effect_endpoints: Vec::new(),
                idempotency_key: IdempotencyKey::from_bytes([0x9a; 16]),
                sealed_at_ms: 145,
            },
        )
        .expect("seal write set for retry attempt")
    {
        TaskWriteSetDecision::Sealed(record) | TaskWriteSetDecision::Replayed(record) => record,
    };
    assert_eq!(write_set_two.participant_registry_binding.generation, 3);

    let permit_two = match authority
        .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
            permit: PermitRequest {
                task_id: attempt_two.task_id,
                attempt_id: attempt_two.attempt_id,
                attempt_generation: attempt_two.attempt_generation,
                write_set_root: write_set_two.write_set_root,
                planned_effects: Vec::new(),
                idempotency_key: IdempotencyKey::from_bytes([0x9b; 16]),
                valid_until_ms: 10_000,
                requested_at_ms: 150,
            },
            lease: lease_one,
        })
        .expect("issue retry permit")
    {
        PermitDecision::Issued(permit) => *permit,
        other => panic!("expected Issued, got {other:?}"),
    };
    let committed = match authority
        .finalize_commit_v3_with_authority_lease(AuthorityLeaseFinalizeRequest {
            finalize: FinalizeRequestV3 {
                base: FinalizeRequest {
                    task_id: attempt_two.task_id,
                    attempt_id: attempt_two.attempt_id,
                    attempt_generation: attempt_two.attempt_generation,
                    permit_id: permit_two.permit_id,
                    new_effect_history_root: empty_effect_history_root(),
                    new_retry_fence_epoch: 0,
                    finalized_at_ms: 160,
                },
                required_satisfaction: Vec::new(),
                fenced_participant_digest: [0; 32],
            },
            lease: lease_one,
        })
        .expect("commit retry attempt")
    {
        FinalizeDecision::Committed(receipt) | FinalizeDecision::Replayed(receipt) => *receipt,
    };
    assert_eq!(committed.new_head_commit_seq, 1);
}

#[test]
#[allow(clippy::too_many_lines)]
fn cs1c_dist_task_002_six_canonical_endpoint_types_and_union_fence_barrier_verification() {
    let fixture = Fixture::new("cs1c-six-endpoint-barrier");
    let authority = fixture.open_task();
    let identity = IdentityAuthority::open(&fixture.identity_root).expect("open identity");
    let signer = bootstrap_barrier_signer(&identity, 0x33, KeyPurpose::BarrierObservationSigning);

    let artifact = nlos_artifact::ArtifactStore::open(&fixture.artifact_root).expect("artifact");
    let read_artifact = ArtifactId::from_bytes([0xa1; 16]);
    artifact
        .create_artifact(nlos_artifact::CreateArtifactSpec {
            artifact_id: read_artifact,
            idempotency_key: IdempotencyKey::from_bytes([0xa2; 16]),
            content_type: "application/octet-stream".to_owned(),
            application_id: None,
            owner: None,
            created_at_ms: 100,
        })
        .expect("create read artifact");
    let write_artifact = ArtifactId::from_bytes([0xa3; 16]);
    artifact
        .create_artifact(nlos_artifact::CreateArtifactSpec {
            artifact_id: write_artifact,
            idempotency_key: IdempotencyKey::from_bytes([0xa4; 16]),
            content_type: "application/octet-stream".to_owned(),
            application_id: None,
            owner: None,
            created_at_ms: 101,
        })
        .expect("create write artifact");

    let (semantic, event_id, _, durability_receipt_id, target) =
        seed_semantic_authority(&fixture.semantic_root);
    let resource = ResourceOwnerFixture::new(&fixture.resource_root, 0xa5);
    let operation_store =
        nlos_store::SqliteOperationStore::open(fixture.operation_path()).expect("operation store");
    let operation = nlos_operation::OperationSpec {
        operation_id: OperationId::from_bytes([0xa6; 16]),
        generation: Generation::INITIAL,
        owner_fiber: nlos_runtime::FiberHandle {
            fiber_id: ExecutionFiberId::from_bytes([0xa7; 16]),
            generation: Generation::INITIAL,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes([0xa8; 16]),
        cancellation_generation: Generation::INITIAL,
    };
    operation_store
        .register(operation)
        .expect("register operation");
    let quote = resource.quote(0xa9, 100);
    let reservation = resource.reserve(
        &quote,
        CallId::from_bytes([0xaa; 16]),
        operation.operation_id,
        IdempotencyKey::from_bytes([0xab; 16]),
    );
    let channel = ChannelAuthority::open(&fixture.channel_root).expect("channel authority");
    let channel_record = match channel
        .create_channel(CreateChannelRequest {
            capacity_bytes: 4096,
            policy_digest: [0xac; 32],
            idempotency_key: IdempotencyKey::from_bytes([0xad; 16]),
            created_at_ms: 102,
        })
        .expect("create channel")
    {
        ChannelDecision::Created(record) => record,
        ChannelDecision::Replayed(_) => panic!("unexpected replay"),
    };

    let lease_one = acquire_lease(&authority, 1, 0xb1, 100, 100);
    let attempt = register_task_and_receipted_attempt(
        &authority,
        0xb2,
        0xb3,
        0,
        empty_effect_history_root(),
        0,
        110,
    );

    let current_binding = |authority: &SqliteTaskAuthority| {
        let reg = authority
            .inspect_participant_registry(attempt.task_id)
            .expect("inspect registry");
        ParticipantRegistryBinding {
            generation: reg.generation,
            root: reg.root,
        }
    };

    // Register all 5 non-TaskStore canonical §26.1 endpoint types + OperationBinding
    // (TaskStore is registered at genesis, making all 6 canonical §26.1 types present).
    authority
        .register_artifact_head_participant(
            &artifact,
            attempt.task_id,
            current_binding(&authority),
            write_artifact,
            120,
        )
        .expect("register ArtifactHead");
    authority
        .register_semantic_admission_participant(
            &semantic,
            attempt.task_id,
            current_binding(&authority),
            121,
        )
        .expect("register SemanticAdmission");
    authority
        .register_channel_participant(
            &channel,
            attempt.task_id,
            current_binding(&authority),
            channel_record.channel_id,
            channel_record.generation,
            122,
        )
        .expect("register ChannelTopic");
    authority
        .register_driver_gateway_participant(
            &resource.authority,
            attempt.task_id,
            current_binding(&authority),
            resource.driver.driver_id,
            resource.driver.generation,
            123,
        )
        .expect("register DriverGateway");
    authority
        .register_resource_ledger_participant(
            &resource.authority,
            attempt.task_id,
            current_binding(&authority),
            resource.account.account_id,
            Generation::INITIAL,
            124,
        )
        .expect("register ResourceLedger");
    authority
        .register_operation_binding_participant(
            &operation_store,
            attempt.task_id,
            current_binding(&authority),
            operation.operation_id,
            operation.generation,
            125,
        )
        .expect("register OperationBinding");

    let effects = vec![
        planned_effect(attempt.task_id, 0, 0xc1),
        planned_effect(attempt.task_id, 1, 0xc5),
    ];
    let write_set = match authority
        .seal_task_write_set_with_authorities_struct(
            Authorities {
                artifact: Some(&artifact),
                process: None,
                semantic: Some(&semantic),
                resource: Some(&resource.authority),
                operation: Some(&operation_store),
                channel: Some(&channel),
            },
            TaskWriteSetRequest {
                task_id: attempt.task_id,
                attempt_id: attempt.attempt_id,
                attempt_generation: attempt.attempt_generation,
                artifact_reads: vec![TaskWriteSetArtifactRead {
                    artifact_id: read_artifact,
                    expected_head_revision: 0,
                    expected_head_digest: None,
                }],
                artifact_writes: vec![TaskWriteSetArtifactWriteRequest {
                    artifact_id: write_artifact,
                    expected_head_revision: 0,
                    proposed_revision: 1,
                    content_digest: [0xc9; 32],
                    size_bytes: 32,
                }],
                process_binding: None,
                semantic_reads: Vec::new(),
                semantic_appends: vec![TaskWriteSetSemanticAppendRequest {
                    event_id,
                    target: TaskWriteSetSemanticTarget::Namespace(target),
                    required_durability: TaskWriteSetSemanticRequiredDurability::Durable,
                    expected_admission_policy_digest: [0x58; 32],
                    durability_receipt_id: Some(durability_receipt_id),
                }],
                resource_reservations: vec![TaskWriteSetResourceReservationRequest {
                    reservation_id: reservation.reservation_id,
                    expected_call_id: reservation.call_id,
                    expected_operation_id: reservation.operation_id,
                    expected_quote_id: reservation.quote_id,
                }],
                planned_effects: effects.clone(),
                effect_endpoints: vec![
                    TaskWriteSetEffectEndpointRequest::OperationBinding {
                        effect_seq: 0,
                        operation_id: operation.operation_id,
                        expected_operation_generation: operation.generation,
                    },
                    TaskWriteSetEffectEndpointRequest::ChannelTopicBinding {
                        effect_seq: 1,
                        channel_id: channel_record.channel_id,
                        expected_channel_generation: channel_record.generation,
                    },
                ],
                idempotency_key: IdempotencyKey::from_bytes([0xca; 16]),
                sealed_at_ms: 130,
            },
        )
        .expect("seal six-domain write set")
    {
        TaskWriteSetDecision::Sealed(record) | TaskWriteSetDecision::Replayed(record) => record,
    };

    let permit = match authority
        .request_commit_permit_with_authorities_and_lease(
            Authorities {
                artifact: Some(&artifact),
                process: None,
                semantic: Some(&semantic),
                resource: Some(&resource.authority),
                operation: Some(&operation_store),
                channel: Some(&channel),
            },
            AuthorityLeasePermitRequest {
                permit: PermitRequest {
                    task_id: attempt.task_id,
                    attempt_id: attempt.attempt_id,
                    attempt_generation: attempt.attempt_generation,
                    write_set_root: write_set.write_set_root,
                    planned_effects: effects,
                    idempotency_key: IdempotencyKey::from_bytes([0xcb; 16]),
                    valid_until_ms: 10_000,
                    requested_at_ms: 140,
                },
                lease: lease_one,
            },
        )
        .expect("issue lease-bound permit")
    {
        PermitDecision::Issued(permit) => *permit,
        other => panic!("expected Issued, got {other:?}"),
    };
    let registry_binding = permit
        .participant_registry_binding
        .expect("permit registry binding");

    // Acquire successor lease in term 2 after lease_one expires and freeze for takeover.
    let lease_two = acquire_lease(&authority, 2, 0xcc, 210, 10_000);
    assert_eq!(lease_two.term, lease_one.term + 1);
    let frozen = authority
        .prepare_authority_takeover_fence(AuthorityLeaseTakeoverFenceRequest {
            task_id: attempt.task_id,
            expected_registry_binding: registry_binding,
            lease: lease_two,
            requested_at_ms: 220,
        })
        .expect("prepare takeover fence over six-domain outstanding write set");
    assert_eq!(frozen.state, ParticipantRegistryState::FrozenForTakeover);

    let fence = authority
        .inspect_authority_takeover_fence_receipt(attempt.task_id, registry_binding)
        .expect("fence receipt");
    assert!(
        fence.exact_fence_set_root.is_some(),
        "union fence set root must be resolved across all 6 domain faces"
    );
    assert!(
        fence.outstanding_operation_participant_root.is_some()
            && fence.outstanding_operation_participant_root != Some([0; 32]),
        "outstanding participant root must be non-zero when an outstanding 6-domain write set exists"
    );

    let takeover = authority
        .inspect_authority_takeover_receipt(attempt.task_id, fence.receipt_id)
        .expect("takeover receipt");
    let audit_before = authority
        .inspect_takeover_endpoint_barrier_audit(takeover.receipt_id)
        .expect("audit before barrier observations");
    assert!(audit_before.all_six_canonical_types_present);
    assert_eq!(
        audit_before.covered_canonical_types,
        ParticipantType::CANONICAL_SECTION_26_1.to_vec()
    );
    assert_eq!(audit_before.missing_canonical_types, Vec::new());
    assert!(!audit_before.all_union_members_signed_and_observed);
    assert_eq!(audit_before.union_fence_members.len(), 7);

    // Verification and completion fail closed before all union members have signed barrier observations.
    assert!(matches!(
        authority.verify_six_endpoint_takeover_barrier_coverage(takeover.receipt_id),
        Err(TaskStoreError::BarrierCoverageIncomplete {
            expected: 7,
            observed: 0
        })
    ));
    assert!(matches!(
        authority.complete_authority_takeover(CompleteAuthorityTakeoverRequest {
            takeover_receipt_id: takeover.receipt_id,
            lease: lease_two,
            completed_at_ms: 2_300,
        }),
        Err(TaskStoreError::CorruptRecord(_))
    ));

    // Record principal-signed barrier observations for all 6 canonical types + OperationBinding (all 7 union members),
    // stopping 1 short first to prove partial coverage is still rejected.
    for (idx, member) in audit_before
        .union_fence_members
        .iter()
        .take(6)
        .copied()
        .enumerate()
    {
        record_signed_observation(
            &authority,
            &identity,
            &signer,
            &takeover,
            member,
            0xd0 + u8::try_from(idx).unwrap(),
        );
    }
    assert!(matches!(
        authority.verify_six_endpoint_takeover_barrier_coverage(takeover.receipt_id),
        Err(TaskStoreError::BarrierCoverageIncomplete {
            expected: 7,
            observed: 6
        })
    ));

    let last_member = *audit_before.union_fence_members.last().unwrap();
    record_signed_observation(&authority, &identity, &signer, &takeover, last_member, 0xdf);

    let audit_after = authority
        .verify_six_endpoint_takeover_barrier_coverage(takeover.receipt_id)
        .expect("six-endpoint barrier coverage verified");
    assert!(audit_after.all_six_canonical_types_present);
    assert!(audit_after.all_union_members_signed_and_observed);
    assert_eq!(audit_after.unsigned_or_unobserved_members, Vec::new());

    let coverage = authority
        .inspect_authority_takeover_barrier_coverage(takeover.receipt_id)
        .expect("barrier coverage");
    assert_eq!(
        coverage.state,
        AuthorityTakeoverBarrierCoverageState::LocallyCovered
    );

    let completed = authority
        .complete_authority_takeover(CompleteAuthorityTakeoverRequest {
            takeover_receipt_id: takeover.receipt_id,
            lease: lease_two,
            completed_at_ms: 2_400,
        })
        .expect("complete takeover after full 6-endpoint union barrier coverage");
    assert_eq!(
        completed.barrier_state,
        AuthorityTakeoverReceiptState::Complete
    );
    let new_assignment = authority
        .inspect_authority_assignment_by_id(attempt.task_id, completed.new_assignment_id)
        .expect("new assignment");
    assert_eq!(new_assignment.state, AuthorityAssignmentState::Active);
    let old_assignment = authority
        .inspect_authority_assignment_by_id(attempt.task_id, completed.old_assignment_id)
        .expect("old assignment");
    assert_eq!(old_assignment.state, AuthorityAssignmentState::Fenced);
}

#[test]
#[allow(clippy::too_many_lines)]
fn cs1c_dist_task_002_quarantine_receipt_auto_links_outstanding_effect_quarantine_root_into_takeover_receipt()
 {
    let fixture = Fixture::new("cs1c-quarantine-root-linkage");
    let authority = fixture.open_task();
    let artifact = nlos_artifact::ArtifactStore::open(&fixture.artifact_root).expect("artifact");
    let (semantic, _, _, _, _) = seed_semantic_authority(&fixture.semantic_root);

    let lease_one = acquire_lease(&authority, 1, 0xe1, 100, 100);
    let attempt = register_task_and_receipted_attempt(
        &authority,
        0xe2,
        0xe3,
        0,
        empty_effect_history_root(),
        0,
        110,
    );
    let init_reg = authority
        .inspect_participant_registry(attempt.task_id)
        .expect("init registry");
    authority
        .register_semantic_admission_participant(
            &semantic,
            attempt.task_id,
            ParticipantRegistryBinding {
                generation: init_reg.generation,
                root: init_reg.root,
            },
            115,
        )
        .expect("register semantic participant");

    let effect = planned_effect(attempt.task_id, 0, 0xe4);
    let write_set = match authority
        .seal_task_write_set_with_authorities_struct(
            Authorities {
                artifact: Some(&artifact),
                process: None,
                semantic: Some(&semantic),
                resource: None,
                operation: None,
                channel: None,
            },
            TaskWriteSetRequest {
                task_id: attempt.task_id,
                attempt_id: attempt.attempt_id,
                attempt_generation: attempt.attempt_generation,
                artifact_reads: Vec::new(),
                artifact_writes: Vec::new(),
                process_binding: None,
                semantic_reads: Vec::new(),
                semantic_appends: Vec::new(),
                resource_reservations: Vec::new(),
                planned_effects: vec![effect.clone()],
                effect_endpoints: vec![TaskWriteSetEffectEndpointRequest::SemanticAdmission {
                    effect_seq: 0,
                }],
                idempotency_key: IdempotencyKey::from_bytes([0xe5; 16]),
                sealed_at_ms: 120,
            },
        )
        .expect("seal write set")
    {
        TaskWriteSetDecision::Sealed(record) | TaskWriteSetDecision::Replayed(record) => record,
    };

    let permit = match authority
        .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
            permit: PermitRequest {
                task_id: attempt.task_id,
                attempt_id: attempt.attempt_id,
                attempt_generation: attempt.attempt_generation,
                write_set_root: write_set.write_set_root,
                planned_effects: vec![effect],
                idempotency_key: IdempotencyKey::from_bytes([0xe6; 16]),
                valid_until_ms: 10_000,
                requested_at_ms: 130,
            },
            lease: lease_one,
        })
        .expect("issue permit")
    {
        PermitDecision::Issued(permit) => *permit,
        other => panic!("expected Issued, got {other:?}"),
    };
    let registry_binding = permit
        .participant_registry_binding
        .expect("permit registry binding");

    let effect_permit = match authority
        .request_effect_permit_with_authority_lease(AuthorityLeaseEffectPermitRequest {
            permit: EffectPermitRequest {
                task_id: attempt.task_id,
                attempt_id: attempt.attempt_id,
                attempt_generation: attempt.attempt_generation,
                permit_id: permit.permit_id,
                permit_epoch: permit.permit_epoch,
                effect_seq: 0,
                idempotency_key: IdempotencyKey::from_bytes([0xe7; 16]),
                valid_until_ms: 10_000,
                requested_at_ms: 140,
            },
            lease: lease_one,
        })
        .expect("issue effect permit")
    {
        EffectPermitDecision::Issued(issued) => *issued,
        other @ EffectPermitDecision::Replayed(_) => {
            panic!("expected Issued effect permit, got {other:?}")
        }
    };
    authority
        .consume_dispatch_token_with_authority_lease(AuthorityLeaseDispatchRequest {
            dispatch: DispatchRequest {
                task_id: attempt.task_id,
                attempt_id: attempt.attempt_id,
                attempt_generation: attempt.attempt_generation,
                permit_id: permit.permit_id,
                permit_epoch: permit.permit_epoch,
                effect_permit_id: effect_permit.effect_permit_id,
                dispatch_token: effect_permit.one_shot_dispatch_token,
                dispatched_at_ms: 150,
            },
            lease: lease_one,
        })
        .expect("consume dispatch token");
    authority
        .record_effect_outcome_with_authority_lease(AuthorityLeaseOutcomeRequest {
            outcome: OutcomeRequest {
                task_id: attempt.task_id,
                attempt_id: attempt.attempt_id,
                attempt_generation: attempt.attempt_generation,
                permit_id: permit.permit_id,
                permit_epoch: permit.permit_epoch,
                effect_seq: 0,
                outcome: Outcome::Unknown {
                    uncertainty_digest: [0xe8; 32],
                },
                recorded_at_ms: 160,
            },
            lease: lease_one,
        })
        .expect("record unknown outcome");

    let quarantine_receipt = match authority
        .close_permit_with_authority_lease(AuthorityLeaseCloseRequest {
            close: ClosePermitRequest {
                task_id: attempt.task_id,
                attempt_id: attempt.attempt_id,
                attempt_generation: attempt.attempt_generation,
                permit_id: permit.permit_id,
                outcome: PermitClosureOutcome::FailedBeforeEffect,
                fenced_participant_digest: [0xe9; 32],
                closed_at_ms: 170,
            },
            lease: lease_one,
        })
        .expect("quarantine on unknown slot")
    {
        ClosePermitDecision::Quarantined(record) => *record,
        other => panic!("expected Quarantined, got {other:?}"),
    };

    let lease_two = acquire_lease(&authority, 2, 0xea, 210, 10_000);

    // 1. Passing a conflicting `outstanding_effect_quarantine_root` fails closed.
    assert!(matches!(
        authority.prepare_authority_takeover_fence_with_receipt_extensions(
            AuthorityLeaseTakeoverFenceRequest {
                task_id: attempt.task_id,
                expected_registry_binding: registry_binding,
                lease: lease_two,
                requested_at_ms: 220,
            },
            Some([0xff; 32]),
            Some(10),
        ),
        Err(TaskStoreError::AuthorityLeaseBindingMismatch)
    ));

    // 2. Calling `prepare_authority_takeover_fence` (with `None`) automatically populates
    //    `takeover.outstanding_effect_quarantine_root` from `quarantine_receipt`.
    authority
        .prepare_authority_takeover_fence(AuthorityLeaseTakeoverFenceRequest {
            task_id: attempt.task_id,
            expected_registry_binding: registry_binding,
            lease: lease_two,
            requested_at_ms: 221,
        })
        .expect("prepare takeover fence auto-links quarantine root");

    let fence = authority
        .inspect_authority_takeover_fence_receipt(attempt.task_id, registry_binding)
        .expect("fence receipt");
    let takeover = authority
        .inspect_authority_takeover_receipt(attempt.task_id, fence.receipt_id)
        .expect("takeover receipt");
    assert_eq!(
        takeover.outstanding_effect_quarantine_root,
        Some(quarantine_receipt.outstanding_effect_quarantine_root),
        "TaskAuthorityTakeoverReceipt must automatically inherit QuarantineReceiptRecord.outstanding_effect_quarantine_root"
    );
}
