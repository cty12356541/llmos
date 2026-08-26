#![allow(deprecated)] // Ladder constructors deprecated in favor of the *_with_authorities_struct entries.
//! Combined Semantic + Resource authority finalize rung tests (Attempt
//! TASK-SEMANTIC-RESOURCE-RUNG-01).
//!
//! Every mixed test follows one narrative: seed a Semantic owner event,
//! reserve on the Resource owner, seal a `TaskWriteSet` carrying BOTH
//! `semantic_appends` AND `resource_reservations`, issue the permit, drive
//! the Semantic publication plan to READY and the Reservations to
//! FINALIZED, then call the combined finalize. The Task terminal
//! transaction must persist BOTH nested evidence sets; replay must answer
//! from the Task rows alone against empty owners.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_capability::CapabilityTarget;
use nlos_resource::{
    AccountRecord, CreateAccountRequest, CreateQuoteRequest, DriverRecord, FinalizationReceipt,
    QuoteRecord, RegisterDriverRequest, ReservationRecord, ReserveRequest, ResourceAuthority,
};
use nlos_semantic::{PublishSemanticPublicationRequest, SemanticAuthority};
use nlos_task::{
    AttemptSpec, AuthorityLeasePermitRequest, AuthorityLeaseRecord, AuthorityLeaseRequest,
    FinalizeRequest, FinalizeRequestV3, NestedResourceCostReceipt,
    NestedSemanticPublicationReceipt, ParticipantRegistryBinding, PermitDecision, PermitRecord,
    PermitRequest, PermitState, PlanSemanticCommitRequest, RecordSemanticPublicationsRequest,
    SemanticCommitPlanId, SemanticCommitPlanState, SemanticFinalizeDecision,
    SemanticResourceFinalizeDecision, SnapshotBundle, SnapshotConsistency, SqliteTaskAuthority,
    TaskSnapshotReceiptSpec, TaskSpec, TaskStoreError, TaskWriteSetRequest,
    TaskWriteSetResourceReservationRequest, TaskWriteSetSemanticAppendRequest,
    TaskWriteSetSemanticRequiredDurability, TaskWriteSetSemanticTarget, empty_effect_history_root,
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
            "nlos-task-mixed-semantic-resource-{label}-{}-{suffix}",
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

fn raw_count(fixture: &Fixture, sql: &str) -> i64 {
    let raw = Connection::open(&fixture.task_path).expect("raw task database");
    raw.query_row(sql, [], |row| row.get(0))
        .expect("count query")
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

/// One Resource owner fixture (driver, account, quote, reserve, settle).
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

    fn quote(&self, seed: u8, upper_bound: u64) -> QuoteRecord {
        self.authority
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
                reserved_at_ms: 1_100,
            })
            .expect("reserve")
            .record()
    }

    /// Only activates the reservation (no consumption, no finalization).
    fn activate_only(&self, reservation: &ReservationRecord) {
        self.authority
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
            .expect("activate");
    }

    /// Activates, records the ordered consumptions, and finalizes with the
    /// given final usage, returning the owner finalization receipt.
    fn settle(
        &self,
        reservation: &ReservationRecord,
        consumptions: &[(u64, u64)],
        final_usage: u64,
        seed: u8,
    ) -> FinalizationReceipt {
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
        for (index, (sequence, cumulative_usage)) in consumptions.iter().enumerate() {
            self.authority
                .consume(nlos_resource::ConsumeReservationRequest {
                    reservation_id: reservation.reservation_id,
                    operation_id: reservation.operation_id,
                    activation_receipt_id: activation.receipt_id,
                    sequence: *sequence,
                    cumulative_usage: *cumulative_usage,
                    consumed_at_ms: 1_500 + 10 * (index as u64),
                })
                .expect("consume");
        }
        let final_seq = consumptions.last().map_or(0, |(sequence, _)| *sequence);
        self.authority
            .finalize_reservation(nlos_resource::FinalizeReservationRequest {
                reservation_id: reservation.reservation_id,
                operation_id: reservation.operation_id,
                activation_receipt_id: activation.receipt_id,
                effect_closed_proof_digest: [seed ^ 0x21; 32],
                final_seq,
                final_usage,
                finalized_at_ms: 1_600,
            })
            .expect("owner finalize")
            .receipt()
    }
}

#[derive(Clone, Copy)]
enum PermitPath {
    Plain,
    AuthorityLease,
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
/// ledger), seals the combined write set, and issues the permit.
#[allow(clippy::too_many_lines)]
fn setup_task_and_permit(
    fixture: &Fixture,
    semantic: &SemanticAuthority,
    owner: &OwnerFixture,
    seed: &SemanticSeed,
    reservations: &[ReservationRecord],
    path: PermitPath,
) -> MixedPermit {
    let (_, event_id, admission_receipt_id, durability_receipt_id, target) = seed;
    let authority = fixture.open_task();
    authority
        .register_task(TaskSpec {
            task_id: task_id(),
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
        })
        .expect("register task");
    let spec = attempt_spec();
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
    let write_set = authority
        .seal_task_write_set_with_authorities(
            &artifact,
            &process,
            semantic,
            &owner.authority,
            TaskWriteSetRequest {
                task_id: task_id(),
                attempt_id: spec.attempt_id,
                attempt_generation: spec.attempt_generation,
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
                planned_effects: Vec::new(),
                effect_endpoints: Vec::new(),
                idempotency_key: IdempotencyKey::from_bytes([0x41; 16]),
                sealed_at_ms: 1_200,
            },
        )
        .expect("seal combined write set")
        .record()
        .clone();
    assert_eq!(
        write_set.semantic_appends[0].admission_receipt_id,
        *admission_receipt_id
    );
    let permit_request = PermitRequest {
        task_id: task_id(),
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        write_set_root: write_set.write_set_root,
        planned_effects: Vec::new(),
        idempotency_key: IdempotencyKey::from_bytes([0x42; 16]),
        valid_until_ms: 9_000,
        requested_at_ms: 1_300,
    };
    let lease = if matches!(path, PermitPath::AuthorityLease) {
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
    let decision = match path {
        PermitPath::Plain => authority
            .request_commit_permit_with_resource_authority(&owner.authority, permit_request)
            .expect("permit"),
        PermitPath::AuthorityLease => authority
            .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
                permit: permit_request,
                lease: lease.expect("lease path"),
            })
            .expect("lease permit"),
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
        .publish_semantic_publication(PublishSemanticPublicationRequest {
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

#[test]
#[allow(clippy::too_many_lines)]
fn mixed_finalize_commits_both_nested_sets_in_one_transaction() {
    // Given a permit whose sealed write set carries one Semantic append and
    // two Reservations, with the plan driven to READY and both owner
    // aggregates FINALIZED (R1: 30 then 37 of 100; R2: 10 of 25).
    let fixture = Fixture::new("happy");
    let seed = seed_semantic_authority(&fixture.semantic_root);
    let (semantic, _, _, _, _) = &seed;
    let owner = OwnerFixture::new(&fixture.resource_root, 0xa1);
    let quote_one = owner.quote(0xa2, 100);
    let quote_two = owner.quote(0xa3, 25);
    let reservation_one = owner.reserve(
        &quote_one,
        CallId::from_bytes([0xa5; 16]),
        OperationId::from_bytes([0xa6; 16]),
        IdempotencyKey::from_bytes([0xa7; 16]),
    );
    let reservation_two = owner.reserve(
        &quote_two,
        CallId::from_bytes([0xa9; 16]),
        OperationId::from_bytes([0xaa; 16]),
        IdempotencyKey::from_bytes([0xab; 16]),
    );
    let mixed = setup_task_and_permit(
        &fixture,
        semantic,
        &owner,
        &seed,
        &[reservation_one, reservation_two],
        PermitPath::Plain,
    );
    let (plan_id, expected_publication) = drive_semantic_to_ready(&mixed, semantic, &seed);
    owner.settle(&reservation_one, &[(1, 30), (2, 37)], 37, 0xac);
    owner.settle(&reservation_two, &[(1, 10)], 10, 0xad);

    // When the combined finalize runs against both owners.
    let decision = mixed
        .authority
        .finalize_commit_v3_with_semantic_publications_and_resource_authority(
            semantic,
            &owner.authority,
            plan_id,
            finalize_request(mixed.permit.permit_id, 1_700),
        )
        .expect("combined finalize");

    // Then one terminal receipt carries BOTH nested sets, each field-equal
    // to its owner source, conservation holds, and the permit closes with
    // a single head advance.
    let SemanticResourceFinalizeDecision::Committed(receipt) = decision else {
        panic!("expected committed decision");
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
    assert_eq!(receipt.semantic_publications, vec![expected_publication]);
    let mut nested = receipt.resource_cost_receipts.clone();
    nested.sort_unstable_by_key(|record| record.reservation_id);
    assert_eq!(nested.len(), 2);
    for (record, reservation) in [
        (&nested[0], &reservation_one),
        (&nested[1], &reservation_two),
    ] {
        let aggregate = owner
            .authority
            .inspect_cost_receipt(reservation.reservation_id)
            .expect("owner aggregate");
        assert_eq!(record, &NestedResourceCostReceipt::from_owner(aggregate));
        assert_eq!(
            record.upper_bound - record.finalization.final_usage,
            record.finalization.refund_credit
        );
    }
    assert_eq!(nested[0].finalization.final_usage, 37);
    assert_eq!(nested[0].finalization.refund_credit, 63);
    assert_eq!(nested[0].consumptions.len(), 2);
    assert_eq!(nested[1].finalization.final_usage, 10);
    assert_eq!(nested[1].finalization.refund_credit, 15);
    assert_eq!(nested[1].consumptions.len(), 1);

    // And both nested sets read back from the Task rows, the plan is
    // FINALIZED against this receipt, and the empty-dimension guarantee
    // holds for every evidence table touched.
    assert_eq!(
        mixed
            .authority
            .inspect_resource_cost_receipts(task_id(), receipt.task_receipt.receipt_id)
            .expect("nested resource readback"),
        nested
    );
    let progress = mixed
        .authority
        .inspect_semantic_commit_progress(plan_id)
        .expect("plan progress");
    assert_eq!(progress.plan.state, SemanticCommitPlanState::Finalized);
    assert_eq!(
        progress.plan.task_receipt_id,
        Some(receipt.task_receipt.receipt_id)
    );
    assert_eq!(progress.publications, vec![expected_publication]);
}

#[test]
#[allow(clippy::too_many_lines)]
fn combined_replay_after_restart_reads_only_task_rows() {
    // Given a committed combined finalize over one Semantic append and one
    // Reservation with two ordered consumptions.
    let fixture = Fixture::new("replay");
    let seed = seed_semantic_authority(&fixture.semantic_root);
    let (semantic, _, _, _, _) = &seed;
    let owner = OwnerFixture::new(&fixture.resource_root, 0xf1);
    let quote = owner.quote(0xf2, 100);
    let reservation = owner.reserve(
        &quote,
        CallId::from_bytes([0xf4; 16]),
        OperationId::from_bytes([0xf5; 16]),
        IdempotencyKey::from_bytes([0xf6; 16]),
    );
    let (committed, plan_id) = {
        let mixed = setup_task_and_permit(
            &fixture,
            semantic,
            &owner,
            &seed,
            &[reservation],
            PermitPath::Plain,
        );
        let (plan_id, _) = drive_semantic_to_ready(&mixed, semantic, &seed);
        owner.settle(&reservation, &[(1, 30), (2, 37)], 37, 0xf7);
        let decision = mixed
            .authority
            .finalize_commit_v3_with_semantic_publications_and_resource_authority(
                semantic,
                &owner.authority,
                plan_id,
                finalize_request(mixed.permit.permit_id, 1_700),
            )
            .expect("combined finalize");
        let SemanticResourceFinalizeDecision::Committed(receipt) = decision else {
            panic!("expected committed decision");
        };
        (*receipt, plan_id)
    };
    drop(owner);

    // When the Task authority is reopened and the same finalize is replayed
    // against fresh, unrelated, EMPTY owners for BOTH authorities, the
    // replay must succeed byte-equal without reading either owner.
    let reopened = fixture.open_task();
    let empty_semantic_root = std::env::temp_dir().join(format!(
        "nlos-task-mixed-replay-empty-semantic-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let empty_resource_root = std::env::temp_dir().join(format!(
        "nlos-task-mixed-replay-empty-resource-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let empty_semantic = SemanticAuthority::open(&empty_semantic_root).expect("empty semantic");
    let empty_owner = OwnerFixture::new(&empty_resource_root, 0xf8);
    let mut replay_request =
        finalize_request(committed.task_receipt.permit_id.expect("permit"), 1_700);
    replay_request.base.finalized_at_ms = 9_999;
    let replay = reopened
        .finalize_commit_v3_with_semantic_publications_and_resource_authority(
            &empty_semantic,
            &empty_owner.authority,
            plan_id,
            replay_request,
        )
        .expect("replay");
    assert!(matches!(
        replay,
        SemanticResourceFinalizeDecision::Replayed(_)
    ));
    assert_eq!(replay.receipt(), &committed);
    let _ = std::fs::remove_dir_all(empty_semantic_root);
    let _ = std::fs::remove_dir_all(empty_resource_root);

    // And no duplicate nested rows were appended on either side.
    assert_eq!(
        raw_count(&fixture, "SELECT COUNT(*) FROM task_resource_cost_receipts"),
        1
    );
    assert_eq!(
        raw_count(
            &fixture,
            "SELECT COUNT(*) FROM task_resource_cost_consumptions"
        ),
        2
    );
    assert_eq!(
        raw_count(
            &fixture,
            "SELECT COUNT(*) FROM task_semantic_publication_receipts"
        ),
        1
    );
}

#[test]
fn combined_finalize_fails_closed_when_resource_side_is_not_finalized() {
    // Given a READY Semantic plan but a Reservation that was only
    // activated (still ACTIVE, never finalized on the owner).
    let fixture = Fixture::new("resource-active");
    let seed = seed_semantic_authority(&fixture.semantic_root);
    let (semantic, _, _, _, _) = &seed;
    let owner = OwnerFixture::new(&fixture.resource_root, 0xc1);
    let quote = owner.quote(0xc2, 100);
    let reservation = owner.reserve(
        &quote,
        CallId::from_bytes([0xc4; 16]),
        OperationId::from_bytes([0xc5; 16]),
        IdempotencyKey::from_bytes([0xc6; 16]),
    );
    let mixed = setup_task_and_permit(
        &fixture,
        semantic,
        &owner,
        &seed,
        &[reservation],
        PermitPath::Plain,
    );
    let (plan_id, _) = drive_semantic_to_ready(&mixed, semantic, &seed);
    owner.activate_only(&reservation);

    // When the combined finalize re-reads the owner, the typed Resource
    // authority error fails closed before any Task mutation.
    assert!(matches!(
        mixed
            .authority
            .finalize_commit_v3_with_semantic_publications_and_resource_authority(
                semantic,
                &owner.authority,
                plan_id,
                finalize_request(mixed.permit.permit_id, 1_700),
            ),
        Err(TaskStoreError::ResourceParticipantAuthority(_))
    ));

    // Then the permit stays issued and no terminal or nested row exists.
    assert_eq!(
        mixed
            .authority
            .inspect_permit(task_id(), mixed.permit.permit_id)
            .expect("permit")
            .state,
        PermitState::Issued
    );
    assert_eq!(raw_count(&fixture, "SELECT COUNT(*) FROM task_receipts"), 0);
    assert_eq!(
        raw_count(&fixture, "SELECT COUNT(*) FROM task_resource_cost_receipts"),
        0
    );
    assert_eq!(
        raw_count(
            &fixture,
            "SELECT COUNT(*) FROM task_semantic_publication_receipts"
        ),
        1
    );
    assert_eq!(
        mixed
            .authority
            .inspect_semantic_commit_progress(plan_id)
            .expect("progress")
            .plan
            .state,
        SemanticCommitPlanState::Ready
    );
}

#[test]
fn combined_finalize_fails_closed_when_semantic_plan_is_not_ready() {
    // Given FINALIZED Reservations but a Semantic publication plan still
    // Publishing (the owner publication was never consumed by the plan).
    let fixture = Fixture::new("semantic-publishing");
    let seed = seed_semantic_authority(&fixture.semantic_root);
    let (semantic, _, _, _, _) = &seed;
    let owner = OwnerFixture::new(&fixture.resource_root, 0xd1);
    let quote = owner.quote(0xd2, 100);
    let reservation = owner.reserve(
        &quote,
        CallId::from_bytes([0xd4; 16]),
        OperationId::from_bytes([0xd5; 16]),
        IdempotencyKey::from_bytes([0xd6; 16]),
    );
    let mixed = setup_task_and_permit(
        &fixture,
        semantic,
        &owner,
        &seed,
        &[reservation],
        PermitPath::Plain,
    );
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
    owner.settle(&reservation, &[(1, 20)], 20, 0xd7);

    // When the combined finalize runs, the typed Semantic plan-readiness
    // error fails closed inside the Task transaction with no partial
    // commit.
    assert!(matches!(
        mixed
            .authority
            .finalize_commit_v3_with_semantic_publications_and_resource_authority(
                semantic,
                &owner.authority,
                plan.plan_id,
                finalize_request(mixed.permit.permit_id, 1_700),
            ),
        Err(TaskStoreError::SemanticCommitPlanNotReady {
            state: SemanticCommitPlanState::Publishing
        })
    ));

    // Then the permit stays issued and no terminal or nested row exists.
    assert_eq!(
        mixed
            .authority
            .inspect_permit(task_id(), mixed.permit.permit_id)
            .expect("permit")
            .state,
        PermitState::Issued
    );
    assert_eq!(raw_count(&fixture, "SELECT COUNT(*) FROM task_receipts"), 0);
    assert_eq!(
        raw_count(&fixture, "SELECT COUNT(*) FROM task_resource_cost_receipts"),
        0
    );
    assert_eq!(
        mixed
            .authority
            .inspect_semantic_commit_progress(plan.plan_id)
            .expect("progress")
            .plan
            .state,
        SemanticCommitPlanState::Publishing
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn combined_finalize_lease_parity_commits_and_replays() {
    // Given a lease-bound permit over a combined write set with both owner
    // sides terminal.
    let fixture = Fixture::new("lease");
    let seed = seed_semantic_authority(&fixture.semantic_root);
    let (semantic, _, _, _, _) = &seed;
    let owner = OwnerFixture::new(&fixture.resource_root, 0xb1);
    let quote = owner.quote(0xb2, 60);
    let reservation = owner.reserve(
        &quote,
        CallId::from_bytes([0xb4; 16]),
        OperationId::from_bytes([0xb5; 16]),
        IdempotencyKey::from_bytes([0xb6; 16]),
    );
    let mixed = setup_task_and_permit(
        &fixture,
        semantic,
        &owner,
        &seed,
        &[reservation],
        PermitPath::AuthorityLease,
    );
    let lease = mixed.lease.expect("lease path");
    let (plan_id, expected_publication) = drive_semantic_to_ready(&mixed, semantic, &seed);
    owner.settle(&reservation, &[(1, 12)], 12, 0xb7);

    // When the combined finalize omits the lease, the typed refusal leaves
    // the permit issued; with the lease both nested sets commit.
    let request = finalize_request(mixed.permit.permit_id, 1_700);
    assert!(matches!(
        mixed
            .authority
            .finalize_commit_v3_with_semantic_publications_and_resource_authority(
                semantic,
                &owner.authority,
                plan_id,
                request.clone(),
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
    let decision = mixed
        .authority
        .finalize_commit_v3_with_semantic_publications_and_resource_authority_and_authority_lease(
            semantic,
            &owner.authority,
            plan_id,
            request,
            lease,
        )
        .expect("combined finalize with lease");
    let SemanticResourceFinalizeDecision::Committed(receipt) = decision else {
        panic!("expected committed decision");
    };
    assert_eq!(receipt.semantic_publications, vec![expected_publication]);
    assert_eq!(receipt.resource_cost_receipts.len(), 1);
    assert_eq!(
        receipt.resource_cost_receipts[0].finalization.final_usage,
        12
    );
    assert_eq!(
        receipt.resource_cost_receipts[0].finalization.refund_credit,
        48
    );

    // And reopening the Task authority and replaying under the same lease
    // binding returns the byte-equal receipt from the Task rows alone.
    let permit_id = mixed.permit.permit_id;
    drop(mixed.authority);
    let reopened = fixture.open_task();
    let replay = reopened
        .finalize_commit_v3_with_semantic_publications_and_resource_authority_and_authority_lease(
            semantic,
            &owner.authority,
            plan_id,
            finalize_request(permit_id, 9_999),
            lease,
        )
        .expect("lease replay");
    assert!(matches!(
        replay,
        SemanticResourceFinalizeDecision::Replayed(_)
    ));
    assert_eq!(replay.receipt(), &*receipt);
}

#[test]
#[allow(clippy::too_many_lines)]
fn pure_resource_permit_still_finalizes_through_resource_variant() {
    // Given a write set with ONLY a Reservation (no Semantic appends) whose
    // owner aggregate is FINALIZED.
    let fixture = Fixture::new("pure-resource");
    let seed = seed_semantic_authority(&fixture.semantic_root);
    let (semantic, _, _, _, _) = &seed;
    let owner = OwnerFixture::new(&fixture.resource_root, 0xe1);
    let quote = owner.quote(0xe2, 90);
    let reservation = owner.reserve(
        &quote,
        CallId::from_bytes([0xe4; 16]),
        OperationId::from_bytes([0xe5; 16]),
        IdempotencyKey::from_bytes([0xe6; 16]),
    );
    let mixed = setup_task_and_permit(
        &fixture,
        semantic,
        &owner,
        &seed,
        &[reservation],
        PermitPath::Plain,
    );
    owner.settle(&reservation, &[(1, 40)], 40, 0xe7);

    // When the single-authority resource variant finalizes the permit.
    let decision = mixed
        .authority
        .finalize_commit_v3_with_resource_authority(
            &owner.authority,
            finalize_request(mixed.permit.permit_id, 1_700),
        )
        .expect("resource finalize");

    // Then it commits unchanged with its own nested set only.
    let nlos_task::ResourceFinalizeDecision::Committed(receipt) = decision else {
        panic!("expected committed decision");
    };
    assert_eq!(receipt.resource_cost_receipts.len(), 1);
    assert_eq!(
        receipt.resource_cost_receipts[0].finalization.refund_credit,
        50
    );
    assert_eq!(
        raw_count(
            &fixture,
            "SELECT COUNT(*) FROM task_semantic_publication_receipts"
        ),
        0
    );
    assert_eq!(
        mixed
            .authority
            .inspect_permit(task_id(), mixed.permit.permit_id)
            .expect("permit")
            .state,
        PermitState::Closed
    );
}

#[test]
fn pure_semantic_permit_still_finalizes_through_semantic_variant() {
    // Given a write set with ONLY a Semantic append whose plan is READY.
    let fixture = Fixture::new("pure-semantic");
    let seed = seed_semantic_authority(&fixture.semantic_root);
    let (semantic, _, _, _, _) = &seed;
    let owner = OwnerFixture::new(&fixture.resource_root, 0x91);
    let mixed = setup_task_and_permit(&fixture, semantic, &owner, &seed, &[], PermitPath::Plain);
    let (plan_id, expected_publication) = drive_semantic_to_ready(&mixed, semantic, &seed);

    // When the single-authority semantic publications variant finalizes.
    let decision = mixed
        .authority
        .finalize_commit_v3_with_semantic_publications(
            semantic,
            plan_id,
            finalize_request(mixed.permit.permit_id, 1_700),
        )
        .expect("semantic finalize");

    // Then it commits unchanged with its own nested set only.
    let SemanticFinalizeDecision::Committed(receipt) = decision else {
        panic!("expected committed decision");
    };
    assert_eq!(receipt.semantic_publications, vec![expected_publication]);
    assert_eq!(receipt.task_receipt.new_head_commit_seq, 1);
    assert_eq!(
        raw_count(&fixture, "SELECT COUNT(*) FROM task_resource_cost_receipts"),
        0
    );
    assert_eq!(
        mixed
            .authority
            .inspect_permit(task_id(), mixed.permit.permit_id)
            .expect("permit")
            .state,
        PermitState::Closed
    );
}
