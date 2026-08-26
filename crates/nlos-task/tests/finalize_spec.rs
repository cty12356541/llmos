#![allow(deprecated)] // Ladder constructors deprecated in favor of the *_with_authorities_struct entries.
//! Struct-based finalize entry (`FinalizeSpec` / `finalize_commit_v3_with_spec`)
//! tests: slot-combination coverage, equivalence with the bare
//! `finalize_commit_v3`, precise fail-closed errors for absent authorities,
//! and replay/idempotency parity with the ladder constructors — including
//! the two ladder-inexpressible gaps the struct entry closes (persisted
//! Semantic finalize envelope + Resource owner receipts, and Semantic owner
//! guard + Resource receipts without a publication plan).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_capability::CapabilityTarget;
use nlos_resource::{
    AccountRecord, CreateAccountRequest, CreateQuoteRequest, DriverRecord, RegisterDriverRequest,
    ReservationRecord, ReserveRequest, ResourceAuthority,
};
use nlos_semantic::SemanticAuthority;
use nlos_task::{
    AttemptSpec, AuthorityLeasePermitRequest, AuthorityLeaseRecord, AuthorityLeaseRequest,
    EffectPermitDecision, EffectPermitRequest, FinalizeDecision, FinalizeRequest,
    FinalizeRequestV3, FinalizeSpec, FinalizeSpecDecision, LogicalEffectDescriptor,
    NestedResourceCostReceipt, NestedSemanticPublicationReceipt, NoEffectReason, NoEffectRequest,
    ParticipantRegistryBinding, PermitDecision, PermitRecord, PermitRequest, PermitState,
    PlanSemanticCommitRequest, PrepareSemanticFinalizeRequest, RecordSemanticPublicationsRequest,
    SemanticCommitPlanId, SemanticCommitPlanState, SemanticFinalizeDecision, SnapshotBundle,
    SnapshotConsistency, SqliteTaskAuthority, TaskSnapshotReceiptSpec, TaskSpec, TaskStoreError,
    TaskWriteSetEffectEndpointRequest, TaskWriteSetRequest, TaskWriteSetResourceReservationRequest,
    TaskWriteSetSemanticAppendRequest, TaskWriteSetSemanticRequiredDurability,
    TaskWriteSetSemanticTarget, empty_effect_history_root,
};
use nlos_types::{
    CallId, CancellationScopeId, Generation, IdempotencyKey, NamespaceId, OperationId, ProcessId,
    ReceiptId, SemanticEventId, TaskAttemptId, TaskId, TaskSnapshotId,
};
use rusqlite::Connection;

static NEXT: AtomicU64 = AtomicU64::new(1);

struct Fixture {
    task_path: PathBuf,
    semantic_root: PathBuf,
    resource_root: PathBuf,
    artifact_root: PathBuf,
    process_root: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "nlos-task-finalize-spec-{label}-{}-{suffix}",
            std::process::id()
        ));
        Self {
            task_path: base.with_extension("sqlite3"),
            semantic_root: base.with_extension("semantic"),
            resource_root: base.with_extension("resource"),
            artifact_root: base.with_extension("artifact"),
            process_root: base.with_extension("process"),
        }
    }

    fn open_task(&self) -> SqliteTaskAuthority {
        SqliteTaskAuthority::open(&self.task_path).expect("open task authority")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let mut path = self.task_path.as_os_str().to_os_string();
            path.push(suffix);
            let _ = std::fs::remove_file(path);
        }
        for root in [
            &self.semantic_root,
            &self.resource_root,
            &self.artifact_root,
            &self.process_root,
        ] {
            let _ = std::fs::remove_dir_all(root);
        }
    }
}

fn task_id() -> TaskId {
    TaskId::from_bytes([0x11; 16])
}

fn attempt_spec() -> AttemptSpec {
    AttemptSpec {
        task_id: task_id(),
        attempt_id: TaskAttemptId::from_bytes([0x21; 16]),
        attempt_generation: Generation::INITIAL,
        snapshot: SnapshotBundle {
            snapshot_id: TaskSnapshotId::from_bytes([0x22; 16]),
            snapshot_digest: [0x23; 32],
            expected_head_commit_seq: 0,
            effect_history_root: empty_effect_history_root(),
            retry_fence_epoch: 0,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes([0x24; 16]),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes([0x25; 16]),
        registered_at_ms: 1_010,
    }
}

fn finalize_request(
    permit_id: nlos_types::CommitPermitId,
    finalized_at_ms: i64,
) -> FinalizeRequestV3 {
    FinalizeRequestV3 {
        base: FinalizeRequest {
            task_id: task_id(),
            attempt_id: attempt_spec().attempt_id,
            attempt_generation: Generation::INITIAL,
            permit_id,
            new_effect_history_root: empty_effect_history_root(),
            new_retry_fence_epoch: 0,
            finalized_at_ms,
        },
        required_satisfaction: Vec::new(),
        fenced_participant_digest: [0; 32],
    }
}

fn register_task_and_attempt(authority: &SqliteTaskAuthority) {
    let spec = attempt_spec();
    authority
        .register_task(TaskSpec {
            task_id: task_id(),
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
        })
        .expect("register task");
    authority
        .register_snapshot_receipt(TaskSnapshotReceiptSpec {
            task_id: task_id(),
            snapshot: spec.snapshot,
            receipt_id: ReceiptId::from_bytes([0x31; 16]),
            builder_id: [0x32; 16],
            builder_version_digest: [0x33; 32],
            per_authority_checkpoint_receipts: vec![ReceiptId::from_bytes([0x34; 16])],
            dependency_closure_root: [0x35; 32],
            semantic_resolver_digest: [0x36; 32],
            canonical_iteration_digest: [0x37; 32],
            achieved_consistency: SnapshotConsistency::Causal,
            built_at_ms: 1_005,
            authority_id: [0x38; 16],
            key_id: [0x39; 16],
            signature: [0x3a; 64],
        })
        .expect("snapshot receipt");
    authority
        .register_attempt_with_snapshot_receipt(spec, ReceiptId::from_bytes([0x31; 16]))
        .expect("register attempt");
}

fn plain_effect() -> nlos_task::PlannedEffect {
    nlos_task::PlannedEffect {
        descriptor: LogicalEffectDescriptor {
            task_id: task_id(),
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

/// Seeds one admitted, durable Semantic event and returns the owner with
/// its proof identities (same raw-owner seeding as the Semantic bridge
/// tests).
type SemanticSeed = (
    SemanticAuthority,
    SemanticEventId,
    ReceiptId,
    ReceiptId,
    NamespaceId,
);

fn seed_semantic_authority(root: &Path) -> SemanticSeed {
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
        target,
    )
}

/// One Resource owner fixture (driver, account).
struct OwnerFixture {
    authority: ResourceAuthority,
    driver: DriverRecord,
    account: AccountRecord,
}

impl OwnerFixture {
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

    fn quote_and_reserve(&self, seed: u8, upper_bound: u64, call_seed: u8) -> ReservationRecord {
        let quote = self
            .authority
            .create_quote(CreateQuoteRequest {
                driver_id: self.driver.driver_id,
                driver_generation: self.driver.generation,
                driver_fencing_token: self.driver.fencing_token,
                operation_proposal_digest: [seed; 32],
                pricing_version: [seed ^ 0x11; 32],
                upper_bound,
                valid_until_ms: 9_000,
                idempotency_key: IdempotencyKey::from_bytes([seed ^ 0x12; 16]),
                created_at_ms: 1_000,
            })
            .expect("create quote")
            .record();
        self.authority
            .reserve(ReserveRequest {
                account_id: self.account.account_id,
                quote_id: quote.quote_id,
                call_id: CallId::from_bytes([call_seed; 16]),
                operation_id: OperationId::from_bytes([call_seed ^ 0x01; 16]),
                idempotency_key: IdempotencyKey::from_bytes([call_seed ^ 0x02; 16]),
                reserved_at_ms: 1_100,
            })
            .expect("reserve")
            .record()
    }

    /// Activates, records one ordered consumption, and finalizes, returning
    /// the owner finalization receipt.
    fn settle(&self, reservation: &ReservationRecord, final_usage: u64, seed: u8) {
        let activation = self
            .authority
            .activate(nlos_resource::ActivateReservationRequest {
                reservation_id: reservation.reservation_id,
                call_id: reservation.call_id,
                operation_id: reservation.operation_id,
                driver_id: reservation.driver_id,
                driver_generation: reservation.driver_generation,
                driver_fencing_token: reservation.driver_fencing_token,
                activation_token: reservation.activation_token,
                activated_at_ms: 1_400,
            })
            .expect("activate")
            .receipt();
        self.authority
            .consume(nlos_resource::ConsumeReservationRequest {
                reservation_id: reservation.reservation_id,
                operation_id: reservation.operation_id,
                activation_receipt_id: activation.receipt_id,
                sequence: 1,
                cumulative_usage: final_usage,
                consumed_at_ms: 1_500,
            })
            .expect("consume");
        self.authority
            .finalize_reservation(nlos_resource::FinalizeReservationRequest {
                reservation_id: reservation.reservation_id,
                operation_id: reservation.operation_id,
                activation_receipt_id: activation.receipt_id,
                effect_closed_proof_digest: [seed ^ 0x21; 32],
                final_seq: 1,
                final_usage,
                finalized_at_ms: 1_600,
            })
            .expect("owner finalize");
    }
}

/// Everything the mixed tests need after the permit exists.
struct MixedPermit {
    authority: SqliteTaskAuthority,
    permit: PermitRecord,
    write_set_root: [u8; 32],
    lease: Option<AuthorityLeaseRecord>,
}

/// Registers the task, snapshot receipt, attempt, and the three owner
/// endpoint participants (Semantic admission, Driver gateway, Resource
/// ledger), seals the combined write set, and issues the permit (optionally
/// bound to a durable authority lease and optionally declaring one
/// no-effect slot for the persisted-envelope path).
#[allow(clippy::too_many_lines)]
fn setup_mixed_permit(
    fixture: &Fixture,
    semantic: &SemanticAuthority,
    owner: &OwnerFixture,
    seed: &SemanticSeed,
    reservations: &[ReservationRecord],
    with_effect: bool,
    with_lease: bool,
) -> MixedPermit {
    let (_, event_id, _, durability_receipt_id, target) = seed;
    let authority = fixture.open_task();
    register_task_and_attempt(&authority);
    let registry = authority
        .inspect_participant_registry(task_id())
        .expect("registry");
    let first_binding = ParticipantRegistryBinding {
        generation: registry.generation,
        root: registry.root,
    };
    let semantic_registration = authority
        .register_semantic_admission_participant(semantic, task_id(), first_binding, 1_150)
        .expect("semantic participant");
    let second_binding = ParticipantRegistryBinding {
        generation: semantic_registration.registry().generation,
        root: semantic_registration.registry().root,
    };
    let driver_registration = authority
        .register_driver_gateway_participant(
            &owner.authority,
            task_id(),
            second_binding,
            owner.driver.driver_id,
            owner.driver.generation,
            1_160,
        )
        .expect("driver participant");
    let third_binding = ParticipantRegistryBinding {
        generation: driver_registration.registry().generation,
        root: driver_registration.registry().root,
    };
    authority
        .register_resource_ledger_participant(
            &owner.authority,
            task_id(),
            third_binding,
            owner.account.account_id,
            Generation::INITIAL,
            1_170,
        )
        .expect("ledger participant");
    let artifact = nlos_artifact::ArtifactStore::open(&fixture.artifact_root).expect("artifact");
    let process = nlos_process::ProcessAuthority::open(&fixture.process_root).expect("process");
    let planned_effects = if with_effect {
        vec![plain_effect()]
    } else {
        Vec::new()
    };
    let effect_endpoints = if with_effect {
        vec![TaskWriteSetEffectEndpointRequest::SemanticAdmission { effect_seq: 0 }]
    } else {
        Vec::new()
    };
    let write_set = authority
        .seal_task_write_set_with_authorities(
            &artifact,
            &process,
            semantic,
            &owner.authority,
            TaskWriteSetRequest {
                task_id: task_id(),
                attempt_id: attempt_spec().attempt_id,
                attempt_generation: Generation::INITIAL,
                artifact_reads: Vec::new(),
                artifact_writes: Vec::new(),
                process_binding: None,
                semantic_reads: Vec::new(),
                semantic_appends: vec![TaskWriteSetSemanticAppendRequest {
                    event_id: *event_id,
                    target: TaskWriteSetSemanticTarget::Namespace(*target),
                    required_durability: TaskWriteSetSemanticRequiredDurability::Durable,
                    expected_admission_policy_digest: [0x58; 32],
                    durability_receipt_id: Some(*durability_receipt_id),
                }],
                resource_reservations: reservations
                    .iter()
                    .map(|reservation| TaskWriteSetResourceReservationRequest {
                        reservation_id: reservation.reservation_id,
                        expected_call_id: reservation.call_id,
                        expected_operation_id: reservation.operation_id,
                        expected_quote_id: reservation.quote_id,
                    })
                    .collect(),
                planned_effects: planned_effects.clone(),
                effect_endpoints,
                idempotency_key: IdempotencyKey::from_bytes([0x41; 16]),
                sealed_at_ms: 1_200,
            },
        )
        .expect("seal combined write set")
        .record()
        .clone();
    let permit_request = PermitRequest {
        task_id: task_id(),
        attempt_id: attempt_spec().attempt_id,
        attempt_generation: Generation::INITIAL,
        write_set_root: write_set.write_set_root,
        planned_effects,
        idempotency_key: IdempotencyKey::from_bytes([0x42; 16]),
        valid_until_ms: 9_000,
        requested_at_ms: 1_300,
    };
    let lease = if with_lease {
        Some(
            authority
                .acquire_authority_lease(AuthorityLeaseRequest {
                    holder_id: ProcessId::from_bytes([0x01; 16]),
                    idempotency_key: IdempotencyKey::from_bytes([0x44; 16]),
                    requested_at_ms: 1_250,
                    ttl_ms: 9_000,
                })
                .expect("lease")
                .record(),
        )
    } else {
        None
    };
    let decision = if let Some(lease) = lease {
        authority
            .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
                permit: permit_request,
                lease,
            })
            .expect("lease permit")
    } else {
        authority
            .request_commit_permit(permit_request)
            .expect("permit")
    };
    let permit = match decision {
        PermitDecision::Issued(permit) => *permit,
        other => panic!("expected issued permit, got {other:?}"),
    };
    MixedPermit {
        authority,
        permit,
        write_set_root: write_set.write_set_root,
        lease,
    }
}

/// Drives the Semantic publication plan Planned → Publishing → READY and
/// returns the plan id with the exact owner publication copy.
fn drive_semantic_to_ready(
    mixed: &MixedPermit,
    semantic: &SemanticAuthority,
    seed: &SemanticSeed,
) -> (SemanticCommitPlanId, NestedSemanticPublicationReceipt) {
    let (_, event_id, admission_receipt_id, durability_receipt_id, target) = seed;
    let plan = mixed
        .authority
        .plan_semantic_commit(PlanSemanticCommitRequest {
            task_id: task_id(),
            attempt_id: attempt_spec().attempt_id,
            attempt_generation: Generation::INITIAL,
            permit_id: mixed.permit.permit_id,
            idempotency_key: IdempotencyKey::from_bytes([0x52; 16]),
            planned_at_ms: 1_350,
        })
        .expect("plan")
        .record()
        .clone();
    assert!(matches!(
        mixed
            .authority
            .authorize_semantic_publication(plan.plan_id, 1_400)
            .expect("authorize")
            .record()
            .state,
        SemanticCommitPlanState::Publishing
    ));
    let owner = semantic
        .publish_semantic_publication(nlos_semantic::PublishSemanticPublicationRequest {
            task_id: task_id(),
            permit_id: mixed.permit.permit_id,
            write_set_root: mixed.write_set_root,
            event_id: *event_id,
            target: CapabilityTarget::Namespace(*target),
            admission_receipt_id: *admission_receipt_id,
            durability_receipt_id: Some(*durability_receipt_id),
            published_at_ms: 1_450,
        })
        .expect("owner publish")
        .receipt();
    let owner_copy = NestedSemanticPublicationReceipt {
        receipt_id: owner.receipt_id,
        task_id: owner.task_id,
        permit_id: owner.permit_id,
        write_set_root: owner.write_set_root,
        event_id: owner.event_id,
        target: TaskWriteSetSemanticTarget::Namespace(*target),
        log_seq: owner.log_seq,
        admission_receipt_id: owner.admission_receipt_id,
        durability_receipt_id: owner.durability_receipt_id,
        semantic_checkpoint_after: owner.semantic_checkpoint_after,
        created_at_ms: owner.created_at_ms,
    };
    let progress = mixed
        .authority
        .record_semantic_publications(
            semantic,
            RecordSemanticPublicationsRequest {
                plan_id: plan.plan_id,
                receipts: vec![owner_copy],
                observed_at_ms: 1_500,
            },
        )
        .expect("record publications");
    assert_eq!(progress.plan.state, SemanticCommitPlanState::Ready);
    (plan.plan_id, owner_copy)
}

/// Closes the single declared effect slot as a no-effect record.
fn close_declared_effect(mixed: &MixedPermit) {
    let issued = match mixed
        .authority
        .request_effect_permit(EffectPermitRequest {
            task_id: task_id(),
            attempt_id: attempt_spec().attempt_id,
            attempt_generation: Generation::INITIAL,
            permit_id: mixed.permit.permit_id,
            permit_epoch: mixed.permit.permit_epoch,
            effect_seq: 0,
            idempotency_key: IdempotencyKey::from_bytes([0x77; 16]),
            valid_until_ms: 9_000,
            requested_at_ms: 1_550,
        })
        .expect("effect permit")
    {
        EffectPermitDecision::Issued(issued) | EffectPermitDecision::Replayed(issued) => *issued,
    };
    mixed
        .authority
        .record_no_effect(NoEffectRequest {
            task_id: task_id(),
            attempt_id: attempt_spec().attempt_id,
            attempt_generation: Generation::INITIAL,
            permit_id: mixed.permit.permit_id,
            permit_epoch: mixed.permit.permit_epoch,
            effect_seq: 0,
            reason: NoEffectReason::NotSelected,
            dispatch_token: Some(issued.one_shot_dispatch_token),
            recorded_at_ms: 1_560,
        })
        .expect("no effect");
}

/// Registers a bare task with one endpoint-free write set and permit (no
/// Semantic, Resource, or lease dimension), for the empty-spec equivalence
/// check.
fn plain_permit(fixture: &Fixture) -> MixedPermit {
    let authority = fixture.open_task();
    register_task_and_attempt(&authority);
    let artifact = nlos_artifact::ArtifactStore::open(&fixture.artifact_root).expect("artifact");
    let write_set = authority
        .seal_task_write_set(
            &artifact,
            TaskWriteSetRequest {
                task_id: task_id(),
                attempt_id: attempt_spec().attempt_id,
                attempt_generation: Generation::INITIAL,
                artifact_reads: Vec::new(),
                artifact_writes: Vec::new(),
                process_binding: None,
                semantic_reads: Vec::new(),
                semantic_appends: Vec::new(),
                resource_reservations: Vec::new(),
                planned_effects: Vec::new(),
                effect_endpoints: Vec::new(),
                idempotency_key: IdempotencyKey::from_bytes([0x81; 16]),
                sealed_at_ms: 1_200,
            },
        )
        .expect("seal plain write set")
        .record()
        .clone();
    let permit = match authority
        .request_commit_permit(PermitRequest {
            task_id: task_id(),
            attempt_id: attempt_spec().attempt_id,
            attempt_generation: Generation::INITIAL,
            write_set_root: write_set.write_set_root,
            planned_effects: Vec::new(),
            idempotency_key: IdempotencyKey::from_bytes([0x82; 16]),
            valid_until_ms: 9_000,
            requested_at_ms: 1_300,
        })
        .expect("permit")
    {
        PermitDecision::Issued(permit) => *permit,
        other => panic!("expected issued permit, got {other:?}"),
    };
    MixedPermit {
        authority,
        permit,
        write_set_root: write_set.write_set_root,
        lease: None,
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn empty_spec_finalizes_like_bare_finalize_v3() {
    // Given one bare permit finalized through the bare v3 constructor and a
    // structurally identical permit finalized through an empty spec.
    let bare_fixture = Fixture::new("bare");
    let bare = plain_permit(&bare_fixture);
    let bare_decision = bare
        .authority
        .finalize_commit_v3(finalize_request(bare.permit.permit_id, 1_700))
        .expect("bare finalize");
    let spec_fixture = Fixture::new("empty-spec");
    let spec = plain_permit(&spec_fixture);
    let spec_decision = spec
        .authority
        .finalize_commit_v3_with_spec(
            finalize_request(spec.permit.permit_id, 1_700),
            FinalizeSpec::default(),
        )
        .expect("empty-spec finalize");

    // Then both commit identically (single head advance, closed permit) and
    // the empty spec returns the Plain decision shape.
    let FinalizeDecision::Committed(bare_receipt) = bare_decision else {
        panic!("expected bare committed decision");
    };
    let FinalizeSpecDecision::Plain(FinalizeDecision::Committed(spec_receipt)) = spec_decision
    else {
        panic!("expected plain committed decision from empty spec");
    };
    assert_eq!(
        bare_receipt.new_head_commit_seq,
        spec_receipt.new_head_commit_seq
    );
    for (mixed, receipt) in [(&bare, &*bare_receipt), (&spec, &*spec_receipt)] {
        assert_eq!(receipt.new_head_commit_seq, 1);
        assert_eq!(
            mixed
                .authority
                .inspect_permit(task_id(), mixed.permit.permit_id)
                .expect("permit")
                .state,
            PermitState::Closed
        );
    }

    // And replaying the same bytes through the empty spec on the finalized
    // permit is idempotent (Replayed, byte-equal receipt), matching the
    // bare constructor's replay semantics.
    let replay = spec
        .authority
        .finalize_commit_v3_with_spec(
            finalize_request(spec.permit.permit_id, 1_700),
            FinalizeSpec::default(),
        )
        .expect("empty-spec replay");
    let FinalizeSpecDecision::Plain(FinalizeDecision::Replayed(replay_receipt)) = replay else {
        panic!("expected plain replayed decision from empty spec");
    };
    assert_eq!(replay_receipt, spec_receipt);
}

#[test]
fn spec_shape_conflicts_fail_closed() {
    // Given any task authority (the checks fire before any owner read).
    let fixture = Fixture::new("shape");
    let authority = fixture.open_task();
    let request = finalize_request(nlos_types::CommitPermitId::from_bytes([0x99; 16]), 1_700);

    // Then a Semantic plan without the Semantic authority fails closed with
    // the exact spec-shape conflict.
    assert!(matches!(
        authority.finalize_commit_v3_with_spec(
            request.clone(),
            FinalizeSpec {
                semantic_plan: Some(SemanticCommitPlanId::from_bytes([0xca; 16])),
                ..FinalizeSpec::default()
            },
        ),
        Err(TaskStoreError::TaskWriteSetConflict { reason })
            if reason.contains("requires the Semantic authority")
    ));

    // And a persisted envelope without a Semantic plan fails closed with the
    // exact spec-shape conflict.
    assert!(matches!(
        authority.finalize_commit_v3_with_spec(
            request,
            FinalizeSpec {
                persisted_envelope: Some(1_700),
                ..FinalizeSpec::default()
            },
        ),
        Err(TaskStoreError::TaskWriteSetConflict { reason })
            if reason.contains("requires a Semantic plan")
    ));
}

#[test]
fn semantic_authority_guard_slot_finalizes_plain_and_replays() {
    // Given a permit sealed over one Semantic append with no plan and no
    // Resource dimension.
    let fixture = Fixture::new("semantic-guard");
    let seed = seed_semantic_authority(&fixture.semantic_root);
    let (semantic, event_id, _, durability_receipt_id, target) = &seed;
    let authority = fixture.open_task();
    register_task_and_attempt(&authority);
    let registry = authority
        .inspect_participant_registry(task_id())
        .expect("registry");
    authority
        .register_semantic_admission_participant(
            semantic,
            task_id(),
            ParticipantRegistryBinding {
                generation: registry.generation,
                root: registry.root,
            },
            1_150,
        )
        .expect("semantic participant");
    let artifact = nlos_artifact::ArtifactStore::open(&fixture.artifact_root).expect("artifact");
    let write_set = authority
        .seal_task_write_set_with_semantic_authority(
            &artifact,
            semantic,
            TaskWriteSetRequest {
                task_id: task_id(),
                attempt_id: attempt_spec().attempt_id,
                attempt_generation: Generation::INITIAL,
                artifact_reads: Vec::new(),
                artifact_writes: Vec::new(),
                process_binding: None,
                semantic_reads: Vec::new(),
                semantic_appends: vec![TaskWriteSetSemanticAppendRequest {
                    event_id: *event_id,
                    target: TaskWriteSetSemanticTarget::Namespace(*target),
                    required_durability: TaskWriteSetSemanticRequiredDurability::Durable,
                    expected_admission_policy_digest: [0x58; 32],
                    durability_receipt_id: Some(*durability_receipt_id),
                }],
                resource_reservations: Vec::new(),
                planned_effects: Vec::new(),
                effect_endpoints: Vec::new(),
                idempotency_key: IdempotencyKey::from_bytes([0x91; 16]),
                sealed_at_ms: 1_200,
            },
        )
        .expect("seal semantic write set")
        .record()
        .clone();
    let permit = match authority
        .request_commit_permit(PermitRequest {
            task_id: task_id(),
            attempt_id: attempt_spec().attempt_id,
            attempt_generation: Generation::INITIAL,
            write_set_root: write_set.write_set_root,
            planned_effects: Vec::new(),
            idempotency_key: IdempotencyKey::from_bytes([0x92; 16]),
            valid_until_ms: 9_000,
            requested_at_ms: 1_300,
        })
        .expect("permit")
    {
        PermitDecision::Issued(permit) => *permit,
        other => panic!("expected issued permit, got {other:?}"),
    };

    // When the Semantic-authority guard slot alone is set, the finalize is
    // the plain ladder semantics (owner re-read then plain v3 commit).
    let decision = authority
        .finalize_commit_v3_with_spec(
            finalize_request(permit.permit_id, 1_700),
            FinalizeSpec {
                semantic_authority: Some(semantic),
                ..FinalizeSpec::default()
            },
        )
        .expect("semantic-guard spec finalize");
    let FinalizeSpecDecision::Plain(FinalizeDecision::Committed(receipt)) = decision else {
        panic!("expected plain committed decision");
    };
    assert_eq!(receipt.new_head_commit_seq, 1);
    assert_eq!(
        authority
            .inspect_permit(task_id(), permit.permit_id)
            .expect("permit")
            .state,
        PermitState::Closed
    );

    // And the replay with the same spec is idempotent (Replayed,
    // byte-equal receipt), consulting only durable Task rows.
    let replay = authority
        .finalize_commit_v3_with_spec(
            finalize_request(permit.permit_id, 1_700),
            FinalizeSpec {
                semantic_authority: Some(semantic),
                ..FinalizeSpec::default()
            },
        )
        .expect("semantic-guard spec replay");
    let FinalizeSpecDecision::Plain(FinalizeDecision::Replayed(replay_receipt)) = replay else {
        panic!("expected plain replayed decision");
    };
    assert_eq!(replay_receipt, receipt);
}

/// Ladder-inexpressible gap 1: a Semantic-owner guard plus Resource
/// receipts WITHOUT a publication plan. No `finalize_commit_v3_*`
/// constructor carries both dimensions without a plan; the struct entry
/// composes the Semantic guard with the Resource rung.
#[test]
fn semantic_guard_plus_resource_without_plan_commits_resource() {
    let fixture = Fixture::new("guard-resource");
    let seed = seed_semantic_authority(&fixture.semantic_root);
    let (semantic, _, _, _, _) = &seed;
    let owner = OwnerFixture::new(&fixture.resource_root, 0xb1);
    let reservation = owner.quote_and_reserve(0xb2, 100, 0xb3);
    let mixed = setup_mixed_permit(
        &fixture,
        semantic,
        &owner,
        &seed,
        &[reservation],
        false,
        false,
    );
    owner.settle(&reservation, 37, 0xb4);

    // When both the Semantic guard and the Resource owner are present but
    // no publication plan is.
    let decision = mixed
        .authority
        .finalize_commit_v3_with_spec(
            finalize_request(mixed.permit.permit_id, 1_700),
            FinalizeSpec {
                semantic_authority: Some(semantic),
                resource_authority: Some(&owner.authority),
                ..FinalizeSpec::default()
            },
        )
        .expect("guard+resource spec finalize");

    // Then the Resource decision shape commits with the nested owner
    // aggregate and a closed permit, exactly like the resource ladder rung.
    let FinalizeSpecDecision::Resource(nlos_task::ResourceFinalizeDecision::Committed(receipt)) =
        decision
    else {
        panic!("expected resource committed decision");
    };
    assert_eq!(receipt.task_receipt.new_head_commit_seq, 1);
    assert_eq!(
        mixed
            .authority
            .inspect_permit(task_id(), mixed.permit.permit_id)
            .expect("permit")
            .state,
        PermitState::Closed
    );
    let aggregate = owner
        .authority
        .inspect_cost_receipt(reservation.reservation_id)
        .expect("owner aggregate");
    assert_eq!(
        receipt.resource_cost_receipts,
        vec![NestedResourceCostReceipt::from_owner(aggregate)]
    );

    // And the replay is idempotent from the durable Task rows.
    let replay = mixed
        .authority
        .finalize_commit_v3_with_spec(
            finalize_request(mixed.permit.permit_id, 1_700),
            FinalizeSpec {
                semantic_authority: Some(semantic),
                resource_authority: Some(&owner.authority),
                ..FinalizeSpec::default()
            },
        )
        .expect("guard+resource spec replay");
    let FinalizeSpecDecision::Resource(nlos_task::ResourceFinalizeDecision::Replayed(
        replay_receipt,
    )) = replay
    else {
        panic!("expected resource replayed decision");
    };
    assert_eq!(*replay_receipt, *receipt);
}

/// Ladder-inexpressible gap 2: a persisted mixed-finalize envelope plus
/// Resource owner receipts (plus the authority lease the permit was bound
/// to). No `finalize_commit_v3_*` constructor carries the envelope together
/// with the Resource dimension; the struct entry reconstructs the request
/// from the envelope and commits BOTH nested evidence sets.
#[test]
#[allow(clippy::too_many_lines)]
fn persisted_envelope_plus_resource_commits_combined_and_replays() {
    let fixture = Fixture::new("envelope-resource");
    let seed = seed_semantic_authority(&fixture.semantic_root);
    let (semantic, _, _, _, _) = &seed;
    let owner = OwnerFixture::new(&fixture.resource_root, 0xc1);
    let reservation = owner.quote_and_reserve(0xc2, 100, 0xc3);
    let (committed, plan_id, expected_publication, lease) = {
        let mixed = setup_mixed_permit(
            &fixture,
            semantic,
            &owner,
            &seed,
            &[reservation],
            true,
            true,
        );
        let lease = mixed.lease.expect("lease path");
        let (plan_id, expected_publication) = drive_semantic_to_ready(&mixed, semantic, &seed);
        let envelope = mixed
            .authority
            .prepare_semantic_finalize(PrepareSemanticFinalizeRequest {
                plan_id,
                required_satisfaction: Vec::new(),
                fenced_participant_digest: [0; 32],
                prepared_at_ms: 1_540,
            })
            .expect("prepare envelope");
        assert_eq!(envelope.record().plan_id, plan_id);
        owner.settle(&reservation, 37, 0xc4);
        close_declared_effect(&mixed);

        // The spec reconstructs the finalize request from the immutable
        // envelope (the caller request bytes are overridden) and commits
        // with the lease, the READY plan, and the Resource owner.
        let decision = mixed
            .authority
            .finalize_commit_v3_with_spec(
                finalize_request(mixed.permit.permit_id, 1_700),
                FinalizeSpec {
                    semantic_authority: Some(semantic),
                    semantic_plan: Some(plan_id),
                    persisted_envelope: Some(1_700),
                    authority_lease: Some(lease),
                    resource_authority: Some(&owner.authority),
                },
            )
            .expect("envelope+resource spec finalize");
        let FinalizeSpecDecision::Combined(nlos_task::SemanticResourceFinalizeDecision::Committed(
            receipt,
        )) = decision
        else {
            panic!("expected combined committed decision");
        };
        assert_eq!(receipt.task_receipt.new_head_commit_seq, 1);
        assert_eq!(receipt.task_receipt.created_at_ms, 1_700);
        assert_eq!(receipt.semantic_publications, vec![expected_publication]);
        assert_eq!(receipt.resource_cost_receipts.len(), 1);
        assert_eq!(
            mixed
                .authority
                .inspect_permit(task_id(), mixed.permit.permit_id)
                .expect("permit")
                .state,
            PermitState::Closed
        );
        assert_eq!(
            mixed
                .authority
                .inspect_semantic_commit_progress(plan_id)
                .expect("plan progress")
                .plan
                .state,
            SemanticCommitPlanState::Finalized
        );
        (*receipt, plan_id, expected_publication, lease)
    };
    drop(owner);

    // When the Task authority is reopened and the same spec is replayed
    // against fresh, unrelated, EMPTY owners for BOTH authorities, the
    // replay must answer byte-equal from the Task rows alone.
    let empty_semantic_root = std::env::temp_dir().join(format!(
        "nlos-task-finalize-spec-empty-semantic-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let empty_resource_root = std::env::temp_dir().join(format!(
        "nlos-task-finalize-spec-empty-resource-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let empty_semantic = SemanticAuthority::open(&empty_semantic_root).expect("empty semantic");
    let empty_resource = ResourceAuthority::open(&empty_resource_root).expect("empty resource");
    let reopened = fixture.open_task();
    let replay = reopened
        .finalize_commit_v3_with_spec(
            finalize_request(nlos_types::CommitPermitId::from_bytes([0x00; 16]), 9_999),
            FinalizeSpec {
                semantic_authority: Some(&empty_semantic),
                semantic_plan: Some(plan_id),
                persisted_envelope: Some(9_999),
                authority_lease: Some(lease),
                resource_authority: Some(&empty_resource),
            },
        )
        .expect("envelope+resource spec replay against empty owners");
    let FinalizeSpecDecision::Combined(nlos_task::SemanticResourceFinalizeDecision::Replayed(
        replay_receipt,
    )) = replay
    else {
        panic!("expected combined replayed decision");
    };
    assert_eq!(*replay_receipt, committed);
    assert_eq!(
        replay_receipt.semantic_publications,
        vec![expected_publication]
    );
    drop(reopened);
    let _ = std::fs::remove_dir_all(&empty_semantic_root);
    let _ = std::fs::remove_dir_all(&empty_resource_root);
}

#[test]
fn missing_lease_slot_fails_closed_on_lease_bound_permit() {
    // Given a lease-bound mixed permit driven to READY/FINALIZED owners.
    let fixture = Fixture::new("lease-absent");
    let seed = seed_semantic_authority(&fixture.semantic_root);
    let (semantic, _, _, _, _) = &seed;
    let owner = OwnerFixture::new(&fixture.resource_root, 0xd1);
    let reservation = owner.quote_and_reserve(0xd2, 100, 0xd3);
    let mixed = setup_mixed_permit(
        &fixture,
        semantic,
        &owner,
        &seed,
        &[reservation],
        false,
        true,
    );
    let (plan_id, _) = drive_semantic_to_ready(&mixed, semantic, &seed);
    owner.settle(&reservation, 37, 0xd4);

    // When the spec names every authority EXCEPT the lease the permit was
    // bound to, the finalize fails closed with the exact typed
    // AuthorityLeaseRequired error and the permit stays issued.
    assert!(matches!(
        mixed.authority.finalize_commit_v3_with_spec(
            finalize_request(mixed.permit.permit_id, 1_700),
            FinalizeSpec {
                semantic_authority: Some(semantic),
                semantic_plan: Some(plan_id),
                resource_authority: Some(&owner.authority),
                ..FinalizeSpec::default()
            },
        ),
        Err(TaskStoreError::AuthorityLeaseRequired)
    ));
    assert_eq!(
        mixed
            .authority
            .inspect_permit(task_id(), mixed.permit.permit_id)
            .expect("permit")
            .state,
        PermitState::Issued
    );

    // And the plan is not consumed by the failed call.
    assert_eq!(
        mixed
            .authority
            .inspect_semantic_commit_progress(plan_id)
            .expect("plan progress")
            .plan
            .state,
        SemanticCommitPlanState::Ready
    );
}

/// The spec entry also covers the plain semantic-publications rung with a
/// lease, confirming `persisted_envelope: None` respects the
/// caller-supplied request bytes exactly like the ladder constructor.
#[test]
fn semantic_plan_slot_with_lease_matches_ladder_replay() {
    let fixture = Fixture::new("plan-lease");
    let seed = seed_semantic_authority(&fixture.semantic_root);
    let (semantic, _, _, _, _) = &seed;
    let owner = OwnerFixture::new(&fixture.resource_root, 0xe1);
    let reservation = owner.quote_and_reserve(0xe2, 100, 0xe3);
    let mixed = setup_mixed_permit(
        &fixture,
        semantic,
        &owner,
        &seed,
        &[reservation],
        false,
        true,
    );
    let lease = mixed.lease.expect("lease path");
    let (plan_id, expected_publication) = drive_semantic_to_ready(&mixed, semantic, &seed);
    owner.settle(&reservation, 37, 0xe4);

    let spec_decision = mixed
        .authority
        .finalize_commit_v3_with_spec(
            finalize_request(mixed.permit.permit_id, 1_700),
            FinalizeSpec {
                semantic_authority: Some(semantic),
                semantic_plan: Some(plan_id),
                authority_lease: Some(lease),
                ..FinalizeSpec::default()
            },
        )
        .expect("plan+lease spec finalize");

    // The Semantic slot alone yields the Semantic decision shape (no
    // Resource receipts even though the write set carries a Reservation,
    // because the Resource slot was not named).
    let FinalizeSpecDecision::Semantic(SemanticFinalizeDecision::Committed(receipt)) =
        spec_decision
    else {
        panic!("expected semantic committed decision");
    };
    assert_eq!(receipt.semantic_publications, vec![expected_publication]);
    assert_eq!(receipt.task_receipt.new_head_commit_seq, 1);

    // And the ladder replay constructor answers byte-equal from the same
    // durable bytes, confirming path parity.
    let ladder_replay = mixed
        .authority
        .finalize_commit_v3_with_semantic_publications_and_authority_lease(
            semantic,
            plan_id,
            finalize_request(mixed.permit.permit_id, 9_999),
            lease,
        )
        .expect("ladder replay");
    let SemanticFinalizeDecision::Replayed(ladder_receipt) = ladder_replay else {
        panic!("expected ladder replayed decision");
    };
    assert_eq!(*ladder_receipt, *receipt);
}
