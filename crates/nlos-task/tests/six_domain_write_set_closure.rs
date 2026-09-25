//! W30-A (B3-3): complete-TaskWriteSet six-domain closure acceptance.
//!
//! One sealed write set spanning all six domain faces — Effect
//! (`planned_effects`), Artifact (`artifact_reads` + `artifact_writes`),
//! Semantic (`semantic_appends`), Resource (`resource_reservations`),
//! Operation (`OperationBinding` effect endpoint), and Channel
//! (`ChannelTopicBinding` effect endpoint) — sealed through the struct
//! `Authorities` bundle, frozen into one unique permit, driven per domain
//! (effect slots closed, Semantic plan READY, Resource owner FINALIZED,
//! Operation owner dispatch activated, Artifact plan driven to READY), and
//! terminalized through the struct finalize rung
//! (`finalize_commit_v3_with_spec` with `semantic_plan` +
//! `resource_authority` + `operation_authority`: the Combined
//! Semantic+Resource ladder dimension plus the ADR-0017 O-B verify gate).
//!
//! The unified read-side `TaskCommitReceipt` then carries what the
//! architecture actually persists for this mixed set: the base
//! `TaskReceiptRecord` (whose `new_effect_history_root` is the Effect
//! domain's durable history), the Semantic publication nested set, and the
//! Resource cost nested set (`SemanticResource` variant — the largest the
//! five-variant enum has). Operation participates as the finalize-time
//! activation-receipt verify gate (guard-only, nothing nested); Channel
//! participates as the seal/permit-frozen endpoint binding (its full
//! machinery — no publication protocol and no finalize gate exists); and
//! the Artifact publication plan is drivable to READY but its own terminal
//! rung is artifact-only by design, so its publications cannot ride the
//! mixed terminal transaction. Those boundaries are pinned by typed
//! assertions below and registered in
//! `docs/evidence/stage-b/b-task-008c2g-complete-writeset.md` instead of
//! being forced.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_capability::CapabilityTarget;
use nlos_channel::{ChannelAuthority, ChannelDecision, CreateChannelRequest, RotateChannelRequest};
use nlos_resource::{
    AccountRecord, CreateAccountRequest, CreateQuoteRequest, DriverRecord, FinalizationReceipt,
    QuoteRecord, RegisterDriverRequest, ReservationRecord, ReserveRequest, ResourceAuthority,
    ResourceDemand,
};
use nlos_semantic::{PublishSemanticPublicationRequest, SemanticAuthority};
use nlos_task::{
    ArtifactPublicationExpectation, AttemptSpec, Authorities, DispatchRequest,
    EffectPermitDecision, EffectPermitRequest, FinalizeRequest, FinalizeRequestV3, FinalizeSpec,
    FinalizeSpecDecision, LogicalEffectDescriptor, NestedResourceCostReceipt,
    NestedSemanticPublicationReceipt, Outcome, OutcomeRequest, ParticipantRegistryBinding,
    PermitDecision, PermitRecord, PermitRequest, PermitState, PlanArtifactCommitRequest,
    PlanSemanticCommitRequest, PlannedEffect, RecordSemanticPublicationsRequest,
    SemanticCommitPlanId, SemanticCommitPlanState, SemanticResourceFinalizeDecision,
    SemanticResourceTaskCommitReceipt, SnapshotBundle, SnapshotConsistency, SqliteTaskAuthority,
    TaskCommitReceipt, TaskSnapshotReceiptSpec, TaskSpec, TaskStoreError, TaskWriteSetArtifactRead,
    TaskWriteSetArtifactWriteRequest, TaskWriteSetEffectEndpointKind,
    TaskWriteSetEffectEndpointRequest, TaskWriteSetRecord, TaskWriteSetRequest,
    TaskWriteSetResourceReservationRequest, TaskWriteSetSemanticAppendRequest,
    TaskWriteSetSemanticRequiredDurability, TaskWriteSetSemanticTarget, empty_effect_history_root,
};
use nlos_types::{
    ArtifactId, CallId, CallbackId, CancellationScopeId, CommitPermitId, ExecutionFiberId,
    Generation, IdempotencyKey, NamespaceId, OperationId, ReceiptId, SemanticEventId,
    TaskAttemptId, TaskId, TaskSnapshotId,
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
}

impl Fixture {
    fn new(label: &str) -> Self {
        let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "nlos-task-six-domain-{label}-{}-{suffix}",
            std::process::id()
        ));
        Self {
            task_path: base.with_extension("sqlite3"),
            semantic_root: base.with_extension("semantic"),
            resource_root: base.with_extension("resource"),
            artifact_root: base.with_extension("artifact"),
            operation_root: base.with_extension("operation"),
            channel_root: base.with_extension("channel"),
        }
    }

    fn open_task(&self) -> SqliteTaskAuthority {
        SqliteTaskAuthority::open(&self.task_path).expect("open task authority")
    }

    fn operation_path(&self) -> PathBuf {
        std::fs::create_dir_all(&self.operation_root).expect("create operation root");
        self.operation_root.join("authority.sqlite3")
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
            &self.operation_root,
            &self.channel_root,
        ] {
            let _ = std::fs::remove_dir_all(root);
        }
    }
}

fn task_id() -> TaskId {
    TaskId::from_bytes([0x61; 16])
}

fn attempt_spec() -> AttemptSpec {
    AttemptSpec {
        task_id: task_id(),
        attempt_id: TaskAttemptId::from_bytes([0x62; 16]),
        attempt_generation: Generation::INITIAL,
        snapshot: SnapshotBundle {
            snapshot_id: TaskSnapshotId::from_bytes([0x63; 16]),
            snapshot_digest: [0x64; 32],
            expected_head_commit_seq: 0,
            effect_history_root: empty_effect_history_root(),
            retry_fence_epoch: 0,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes([0x65; 16]),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes([0x66; 16]),
        registered_at_ms: 1_010,
    }
}

fn finalize_request(permit_id: CommitPermitId, finalized_at_ms: i64) -> FinalizeRequestV3 {
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

/// Seeds one admitted, durable Semantic event (same raw-owner seeding as
/// the Semantic bridge tests).
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
                demand_capacity: ResourceDemand::default(),
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
                demand: ResourceDemand::default(),
                reserved_at_ms: 1_100,
            })
            .expect("reserve")
            .record()
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

fn planned_effect(slot: u64) -> PlannedEffect {
    PlannedEffect {
        descriptor: LogicalEffectDescriptor {
            task_id: task_id(),
            task_generation: Generation::INITIAL,
            intent_spec_id: [0x81; 32],
            stable_action_slot: slot,
            target_authority_object_id: [0x82; 32],
            effect_class: 1,
            idempotency_scope: 1,
        },
        required: false,
        required_condition_digest: None,
        success_criteria_digest: [0x83; 32],
        action_proposal_digest: [0x84; 32],
    }
}

/// Every owner authority the six-domain write set needs, plus the durable
/// identities the assertions compare against.
struct Owners {
    artifact: nlos_artifact::ArtifactStore,
    semantic_seed: SemanticSeed,
    resource: OwnerFixture,
    operation_store: nlos_store::SqliteOperationStore,
    channel: ChannelAuthority,
    operation: nlos_operation::OperationSpec,
    channel_record: nlos_channel::ChannelRecord,
    read_artifact: ArtifactId,
    write_artifact: ArtifactId,
    payload: Vec<u8>,
    staging_key: IdempotencyKey,
}

fn owners(fixture: &Fixture) -> Owners {
    let artifact = nlos_artifact::ArtifactStore::open(&fixture.artifact_root).expect("artifact");
    let read_artifact = ArtifactId::from_bytes([0x90; 16]);
    artifact
        .create_artifact(nlos_artifact::CreateArtifactSpec {
            artifact_id: read_artifact,
            idempotency_key: IdempotencyKey::from_bytes([0x91; 16]),
            content_type: "application/octet-stream".to_owned(),
            application_id: None,
            owner: None,
            created_at_ms: 1_020,
        })
        .expect("create read artifact");
    let write_artifact = ArtifactId::from_bytes([0x92; 16]);
    artifact
        .create_artifact(nlos_artifact::CreateArtifactSpec {
            artifact_id: write_artifact,
            idempotency_key: IdempotencyKey::from_bytes([0x93; 16]),
            content_type: "application/octet-stream".to_owned(),
            application_id: None,
            owner: None,
            created_at_ms: 1_021,
        })
        .expect("create write artifact");
    let semantic_seed = seed_semantic_authority(&fixture.semantic_root);
    let resource = OwnerFixture::new(&fixture.resource_root, 0x94);
    let operation_store =
        nlos_store::SqliteOperationStore::open(fixture.operation_path()).expect("operation store");
    let operation = nlos_operation::OperationSpec {
        operation_id: OperationId::from_bytes([0x95; 16]),
        generation: Generation::INITIAL,
        owner_fiber: nlos_runtime::FiberHandle {
            fiber_id: ExecutionFiberId::from_bytes([0x96; 16]),
            generation: Generation::INITIAL,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes([0x97; 16]),
        cancellation_generation: Generation::INITIAL,
    };
    operation_store
        .register(operation)
        .expect("register operation");
    let channel = ChannelAuthority::open(&fixture.channel_root).expect("channel authority");
    let channel_record = match channel
        .create_channel(CreateChannelRequest {
            capacity_bytes: 4096,
            policy_digest: [0x98; 32],
            idempotency_key: IdempotencyKey::from_bytes([0x99; 16]),
            created_at_ms: 1_030,
        })
        .expect("create channel")
    {
        ChannelDecision::Created(record) => record,
        ChannelDecision::Replayed(_) => panic!("first channel create cannot replay"),
    };
    Owners {
        artifact,
        semantic_seed,
        resource,
        operation_store,
        channel,
        operation,
        channel_record,
        read_artifact,
        write_artifact,
        payload: vec![0x9a; 32],
        staging_key: IdempotencyKey::from_bytes([0x9b; 16]),
    }
}

fn authorities_bundle(owners: &Owners) -> Authorities<'_> {
    Authorities {
        artifact: Some(&owners.artifact),
        process: None,
        semantic: Some(&owners.semantic_seed.0),
        resource: Some(&owners.resource.authority),
        operation: Some(&owners.operation_store),
        channel: Some(&owners.channel),
    }
}

/// The sealed six-domain write set before any permit exists.
struct SixDomainSeal {
    authority: SqliteTaskAuthority,
    sealed: TaskWriteSetRecord,
}

/// Registers the task, snapshot receipt, attempt, and the five registry
/// participants the six-domain seal needs (Semantic admission, Driver
/// gateway, Resource ledger, Operation binding, Channel topic), then seals
/// the six-face write set through the struct Authorities bundle.
#[allow(clippy::too_many_lines)]
fn setup_task_and_seal(
    fixture: &Fixture,
    owners: &Owners,
    reservations: &[ReservationRecord],
) -> SixDomainSeal {
    let (_, event_id, _, durability_receipt_id, target) = &owners.semantic_seed;
    let authority = fixture.open_task();
    authority
        .register_task(TaskSpec {
            application_id: None,
            plan_revision: None,
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
            receipt_id: ReceiptId::from_bytes([0xa1; 16]),
            builder_id: [0xa2; 16],
            builder_version_digest: [0xa3; 32],
            per_authority_checkpoint_receipts: vec![ReceiptId::from_bytes([0xa4; 16])],
            dependency_closure_root: [0xa5; 32],
            semantic_resolver_digest: [0xa6; 32],
            canonical_iteration_digest: [0xa7; 32],
            achieved_consistency: SnapshotConsistency::Causal,
            built_at_ms: 1_005,
            authority_id: [0xa8; 16],
            key_id: [0xa9; 16],
            signature: [0xaa; 64],
        })
        .expect("snapshot receipt");
    authority
        .register_attempt_with_snapshot_receipt(spec, ReceiptId::from_bytes([0xa1; 16]))
        .expect("register attempt");
    let binding = |authority: &SqliteTaskAuthority| {
        let registry = authority
            .inspect_participant_registry(task_id())
            .expect("registry");
        ParticipantRegistryBinding {
            generation: registry.generation,
            root: registry.root,
        }
    };
    let semantic_registration = authority
        .register_semantic_admission_participant(
            &owners.semantic_seed.0,
            task_id(),
            binding(&authority),
            1_040,
        )
        .expect("semantic participant");
    let driver_registration = authority
        .register_driver_gateway_participant(
            &owners.resource.authority,
            task_id(),
            ParticipantRegistryBinding {
                generation: semantic_registration.registry().generation,
                root: semantic_registration.registry().root,
            },
            owners.resource.driver.driver_id,
            owners.resource.driver.generation,
            1_041,
        )
        .expect("driver participant");
    let ledger_registration = authority
        .register_resource_ledger_participant(
            &owners.resource.authority,
            task_id(),
            ParticipantRegistryBinding {
                generation: driver_registration.registry().generation,
                root: driver_registration.registry().root,
            },
            owners.resource.account.account_id,
            Generation::INITIAL,
            1_042,
        )
        .expect("ledger participant");
    let operation_registration = authority
        .register_operation_binding_participant(
            &owners.operation_store,
            task_id(),
            ParticipantRegistryBinding {
                generation: ledger_registration.registry().generation,
                root: ledger_registration.registry().root,
            },
            owners.operation.operation_id,
            owners.operation.generation,
            1_043,
        )
        .expect("operation participant");
    let channel_registration = authority
        .register_channel_participant(
            &owners.channel,
            task_id(),
            ParticipantRegistryBinding {
                generation: operation_registration.registry().generation,
                root: operation_registration.registry().root,
            },
            owners.channel_record.channel_id,
            owners.channel_record.generation,
            1_044,
        )
        .expect("channel participant");
    authority
        .register_artifact_head_participant(
            &owners.artifact,
            task_id(),
            ParticipantRegistryBinding {
                generation: channel_registration.registry().generation,
                root: channel_registration.registry().root,
            },
            owners.write_artifact,
            1_045,
        )
        .expect("artifact head participant for the write face");

    let sealed = authority
        .seal_task_write_set_with_authorities_struct(
            authorities_bundle(owners),
            TaskWriteSetRequest {
                task_id: task_id(),
                attempt_id: spec.attempt_id,
                attempt_generation: spec.attempt_generation,
                artifact_reads: vec![TaskWriteSetArtifactRead {
                    artifact_id: owners.read_artifact,
                    expected_head_revision: 0,
                    expected_head_digest: None,
                }],
                artifact_writes: vec![TaskWriteSetArtifactWriteRequest {
                    artifact_id: owners.write_artifact,
                    expected_head_revision: 0,
                    proposed_revision: 1,
                    content_digest: nlos_artifact::ContentDigest::of_bytes(&owners.payload)
                        .into_bytes(),
                    size_bytes: owners.payload.len() as u64,
                }],
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
                planned_effects: vec![planned_effect(0), planned_effect(1)],
                effect_endpoints: vec![
                    TaskWriteSetEffectEndpointRequest::OperationBinding {
                        effect_seq: 0,
                        operation_id: owners.operation.operation_id,
                        expected_operation_generation: owners.operation.generation,
                    },
                    TaskWriteSetEffectEndpointRequest::ChannelTopicBinding {
                        effect_seq: 1,
                        channel_id: owners.channel_record.channel_id,
                        expected_channel_generation: owners.channel_record.generation,
                    },
                ],
                idempotency_key: IdempotencyKey::from_bytes([0xab; 16]),
                sealed_at_ms: 1_200,
            },
        )
        .expect("seal six-domain write set")
        .record()
        .clone();
    SixDomainSeal { authority, sealed }
}

/// Freezes the sealed six-domain write set into its unique permit through
/// the struct Authorities entry (Artifact/Resource/Operation/Channel
/// revalidation at permit time).
fn freeze_permit(seal: &SixDomainSeal, owners: &Owners) -> PermitRecord {
    let spec = attempt_spec();
    let permit_request = PermitRequest {
        task_id: task_id(),
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        write_set_root: seal.sealed.write_set_root,
        planned_effects: seal.sealed.planned_effects.clone(),
        idempotency_key: IdempotencyKey::from_bytes([0xac; 16]),
        valid_until_ms: 9_000,
        requested_at_ms: 1_300,
    };
    let permit_authorities = Authorities {
        artifact: Some(&owners.artifact),
        process: None,
        semantic: None,
        resource: Some(&owners.resource.authority),
        operation: Some(&owners.operation_store),
        channel: Some(&owners.channel),
    };
    let decision = seal
        .authority
        .request_commit_permit_with_authorities_struct(permit_authorities, permit_request.clone())
        .expect("struct permit");
    let PermitDecision::Issued(permit) = decision else {
        panic!("expected issued permit, got {decision:?}");
    };
    assert!(matches!(
        seal.authority
            .request_commit_permit_with_authorities_struct(permit_authorities, permit_request)
            .expect("permit replay"),
        PermitDecision::Replayed(_)
    ));
    *permit
}

/// The sealed write set plus its unique permit.
struct SixDomainPermit {
    authority: SqliteTaskAuthority,
    permit: PermitRecord,
    sealed: TaskWriteSetRecord,
}

fn setup_task_and_permit(
    fixture: &Fixture,
    owners: &Owners,
    reservations: &[ReservationRecord],
) -> SixDomainPermit {
    let seal = setup_task_and_seal(fixture, owners, reservations);
    let permit = freeze_permit(&seal, owners);
    SixDomainPermit {
        authority: seal.authority,
        permit,
        sealed: seal.sealed,
    }
}

/// Two reservations with distinct quotes (37/100 and 10/25 usage shapes,
/// same as the mixed bridge fixtures).
fn two_reservations(owner: &OwnerFixture, seed: u8) -> Vec<ReservationRecord> {
    let quote_one = owner.quote(seed, 100);
    let quote_two = owner.quote(seed ^ 0x10, 25);
    vec![
        owner.reserve(
            &quote_one,
            CallId::from_bytes([seed ^ 0x20; 16]),
            OperationId::from_bytes([seed ^ 0x21; 16]),
            IdempotencyKey::from_bytes([seed ^ 0x22; 16]),
        ),
        owner.reserve(
            &quote_two,
            CallId::from_bytes([seed ^ 0x30; 16]),
            OperationId::from_bytes([seed ^ 0x31; 16]),
            IdempotencyKey::from_bytes([seed ^ 0x32; 16]),
        ),
    ]
}

/// Drives one effect slot to `EffectClosed` through the effect permit →
/// dispatch token → outcome path.
fn close_effect_slot(mixed: &SixDomainPermit, effect_seq: u64, key_seed: u8, closure: [u8; 32]) {
    let spec = attempt_spec();
    let permit = match mixed
        .authority
        .request_effect_permit(EffectPermitRequest {
            task_id: task_id(),
            attempt_id: spec.attempt_id,
            attempt_generation: spec.attempt_generation,
            permit_id: mixed.permit.permit_id,
            permit_epoch: mixed.permit.permit_epoch,
            effect_seq,
            idempotency_key: IdempotencyKey::from_bytes([key_seed; 16]),
            valid_until_ms: 9_000,
            requested_at_ms: 1_320,
        })
        .expect("effect permit")
    {
        EffectPermitDecision::Issued(record) => record,
        other @ EffectPermitDecision::Replayed(_) => {
            panic!("expected issued effect permit, got {other:?}")
        }
    };
    mixed
        .authority
        .consume_dispatch_token(DispatchRequest {
            task_id: task_id(),
            attempt_id: spec.attempt_id,
            attempt_generation: spec.attempt_generation,
            permit_id: mixed.permit.permit_id,
            permit_epoch: mixed.permit.permit_epoch,
            effect_permit_id: permit.effect_permit_id,
            dispatch_token: permit.one_shot_dispatch_token,
            dispatched_at_ms: 1_330,
        })
        .expect("dispatch token");
    mixed
        .authority
        .record_effect_outcome(OutcomeRequest {
            task_id: task_id(),
            attempt_id: spec.attempt_id,
            attempt_generation: spec.attempt_generation,
            permit_id: mixed.permit.permit_id,
            permit_epoch: mixed.permit.permit_epoch,
            effect_seq,
            outcome: Outcome::Closed {
                authoritative_closure_digest: closure,
            },
            recorded_at_ms: 1_340,
        })
        .expect("effect outcome");
}

/// Drives the Semantic publication plan Planned → Publishing → READY and
/// returns the plan id with the exact owner publication copy.
fn drive_semantic_to_ready(
    mixed: &SixDomainPermit,
    owners: &Owners,
) -> (SemanticCommitPlanId, NestedSemanticPublicationReceipt) {
    let (_, event_id, admission_receipt_id, durability_receipt_id, target) = &owners.semantic_seed;
    let spec = attempt_spec();
    let plan = mixed
        .authority
        .plan_semantic_commit(PlanSemanticCommitRequest {
            task_id: task_id(),
            attempt_id: spec.attempt_id,
            attempt_generation: spec.attempt_generation,
            permit_id: mixed.permit.permit_id,
            idempotency_key: IdempotencyKey::from_bytes([0xbb; 16]),
            planned_at_ms: 1_350,
        })
        .expect("plan")
        .record()
        .clone();
    assert!(matches!(
        mixed
            .authority
            .authorize_semantic_publication(plan.plan_id, 1_360)
            .expect("authorize")
            .record()
            .state,
        SemanticCommitPlanState::Publishing
    ));
    let owner = owners
        .semantic_seed
        .0
        .publish_semantic_publication(PublishSemanticPublicationRequest {
            task_id: task_id(),
            permit_id: mixed.permit.permit_id,
            write_set_root: mixed.sealed.write_set_root,
            event_id: *event_id,
            target: CapabilityTarget::Namespace(*target),
            admission_receipt_id: *admission_receipt_id,
            durability_receipt_id: Some(*durability_receipt_id),
            published_at_ms: 1_370,
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
            &owners.semantic_seed.0,
            RecordSemanticPublicationsRequest {
                plan_id: plan.plan_id,
                receipts: vec![owner_copy],
                observed_at_ms: 1_380,
            },
        )
        .expect("record publications");
    assert_eq!(progress.plan.state, SemanticCommitPlanState::Ready);
    (plan.plan_id, owner_copy)
}

/// Drives the Artifact face: owner stage + publish (durable owner
/// evidence), while the Task-side ladder admission is typed-rejected for
/// this mixed effect-bearing permit (`MixedEffectArtifactWriteSet`, zero
/// durable plan rows) — the artifact-only ladder never accepts it.
fn drive_artifact_owner_evidence(mixed: &SixDomainPermit, owners: &Owners) {
    let staging_id = nlos_artifact::staging_id_for(owners.write_artifact, owners.staging_key);
    let expectation = ArtifactPublicationExpectation {
        staging_id: staging_id.into_bytes(),
        artifact_id: owners.write_artifact,
        target_revision: 1,
        digest: nlos_artifact::ContentDigest::of_bytes(&owners.payload).into_bytes(),
        size_bytes: owners.payload.len() as u64,
    };
    owners
        .artifact
        .stage_revision(nlos_artifact::StageRevisionRequest {
            artifact_id: owners.write_artifact,
            expected_head_revision: 0,
            bytes: &owners.payload,
            task_id: task_id(),
            permit_id: mixed.permit.permit_id,
            write_set_root: nlos_artifact::ContentDigest::from_bytes(mixed.sealed.write_set_root),
            idempotency_key: owners.staging_key,
            created_at_ms: 1_385,
        })
        .expect("owner stage");
    let spec = attempt_spec();
    assert!(
        matches!(
            mixed
                .authority
                .plan_artifact_commit(PlanArtifactCommitRequest {
                    task_id: task_id(),
                    attempt_id: spec.attempt_id,
                    attempt_generation: spec.attempt_generation,
                    permit_id: mixed.permit.permit_id,
                    expectations: vec![expectation],
                    idempotency_key: IdempotencyKey::from_bytes([0xbc; 16]),
                    planned_at_ms: 1_386,
                }),
            Err(TaskStoreError::MixedEffectArtifactWriteSet)
        ),
        "the mixed effect-bearing permit must never enter the artifact-only ladder"
    );
    let receipt = owners
        .artifact
        .publish_staged_revision(nlos_artifact::PublishStagedRevisionRequest {
            staging_id,
            task_id: task_id(),
            permit_id: mixed.permit.permit_id,
            write_set_root: nlos_artifact::ContentDigest::from_bytes(mixed.sealed.write_set_root),
            published_at_ms: 1_388,
        })
        .expect("owner publish")
        .receipt()
        .clone();
    assert_eq!(receipt.revision, 1);
}

/// Prepares and activates the Operation owner dispatch (ADR-0017 O-B
/// activation receipt consumed by the finalize gate).
fn activate_operation(owners: &Owners) {
    let handle = nlos_operation::OperationHandle {
        operation_id: owners.operation.operation_id,
        generation: owners.operation.generation,
    };
    let preparation = match owners
        .operation_store
        .prepare_dispatch(handle, CallbackId::from_bytes([0xbd; 16]))
        .expect("prepare dispatch")
    {
        nlos_store::OperationPrepareDecision::Prepared(preparation)
        | nlos_store::OperationPrepareDecision::Replayed(preparation) => preparation,
    };
    owners
        .operation_store
        .activate_dispatch(preparation)
        .expect("activate dispatch");
}

/// The six-domain finalize rung: Combined Semantic+Resource receipts plus
/// the Operation activation verify gate, through the struct entry.
fn six_domain_finalize<'a>(
    authority: &'a SqliteTaskAuthority,
    permit_id: CommitPermitId,
    semantic: &'a SemanticAuthority,
    resource: &'a ResourceAuthority,
    operation: &'a nlos_store::SqliteOperationStore,
    plan_id: SemanticCommitPlanId,
    finalized_at_ms: i64,
) -> Result<FinalizeSpecDecision, TaskStoreError> {
    authority.finalize_commit_v3_with_spec(
        finalize_request(permit_id, finalized_at_ms),
        FinalizeSpec {
            semantic_authority: Some(semantic),
            semantic_plan: Some(plan_id),
            persisted_envelope: None,
            authority_lease: None,
            resource_authority: Some(resource),
            operation_authority: Some(operation),
        },
    )
}

#[test]
#[allow(clippy::too_many_lines)]
fn six_domain_write_set_seals_permits_and_finalizes_with_unified_receipt() {
    // Given one write set spanning all six domain faces, sealed through
    // the struct Authorities bundle and frozen into one unique permit.
    let fixture = Fixture::new("happy");
    let owners = owners(&fixture);
    let reservations = two_reservations(&owners.resource, 0xb1);
    let mixed = setup_task_and_permit(&fixture, &owners, &reservations);

    // Then the sealed record carries every face with owner-derived facts:
    // the Artifact write declaration resolved against the current head,
    // the Semantic append bound to the admission receipt, both
    // Reservations, both planned effects, and the two effect endpoints
    // whose participant identities are the owner endpoint proofs.
    assert_eq!(mixed.sealed.artifact_writes.len(), 1);
    assert_eq!(
        mixed.sealed.artifact_writes[0].artifact_id,
        owners.write_artifact
    );
    assert_eq!(mixed.sealed.artifact_writes[0].proposed_revision, 1);
    assert_eq!(mixed.sealed.artifact_reads.len(), 1);
    assert_eq!(mixed.sealed.semantic_appends.len(), 1);
    assert_eq!(
        mixed.sealed.semantic_appends[0].admission_receipt_id,
        owners.semantic_seed.2
    );
    assert_eq!(mixed.sealed.resource_reservations.len(), 2);
    assert_eq!(mixed.sealed.planned_effects.len(), 2);
    assert_eq!(mixed.sealed.effect_endpoints.len(), 2);
    let operation_endpoint = &mixed.sealed.effect_endpoints[0];
    assert_eq!(
        operation_endpoint.kind,
        TaskWriteSetEffectEndpointKind::OperationBinding
    );
    assert_eq!(
        operation_endpoint.participant_id,
        owners
            .operation_store
            .inspect_endpoint_proof(nlos_operation::OperationHandle {
                operation_id: owners.operation.operation_id,
                generation: owners.operation.generation,
            })
            .expect("operation proof")
            .participant_id
    );
    let channel_endpoint = &mixed.sealed.effect_endpoints[1];
    assert_eq!(
        channel_endpoint.kind,
        TaskWriteSetEffectEndpointKind::ChannelTopicBinding
    );
    assert_eq!(
        channel_endpoint.participant_id,
        owners
            .channel
            .inspect_endpoint_proof(owners.channel_record.channel_id)
            .expect("channel proof")
            .participant_id
    );
    assert_eq!(
        mixed
            .authority
            .inspect_task_write_set(task_id(), IdempotencyKey::from_bytes([0xab; 16]))
            .expect("write set replay"),
        mixed.sealed
    );

    // When every drivable domain reaches its owner evidence: both effect
    // slots closed, the Semantic plan READY, the Artifact plan READY, both
    // Reservations FINALIZED, and the Operation dispatch activated.
    close_effect_slot(&mixed, 0, 0xb2, [0xb3; 32]);
    close_effect_slot(&mixed, 1, 0xb4, [0xb5; 32]);
    let (plan_id, expected_publication) = drive_semantic_to_ready(&mixed, &owners);
    drive_artifact_owner_evidence(&mixed, &owners);
    owners
        .resource
        .settle(&reservations[0], &[(1, 30), (2, 37)], 37, 0xb6);
    owners
        .resource
        .settle(&reservations[1], &[(1, 10)], 10, 0xb7);
    activate_operation(&owners);

    // And the honest Artifact boundary now holds at ADMISSION: the
    // artifact-only ladder typed-refused this effect-bearing write set
    // before any durable plan existed, so there is no stuck READY plan
    // and nothing of the Artifact face can ride a terminal transaction
    // of this write set (the owner-side publication is durable evidence
    // only).
    assert!(
        mixed
            .authority
            .list_incomplete_artifact_commit_plans(10)
            .expect("incomplete artifact plans")
            .is_empty()
    );

    // Then the Combined rung commits all of it in one terminal
    // transaction and the unified receipt carries the base Effect
    // history plus both nested evidence sets.
    let decision = six_domain_finalize(
        &mixed.authority,
        mixed.permit.permit_id,
        &owners.semantic_seed.0,
        &owners.resource.authority,
        &owners.operation_store,
        plan_id,
        1_700,
    )
    .expect("six-domain combined finalize");
    let FinalizeSpecDecision::Combined(SemanticResourceFinalizeDecision::Committed(receipt)) =
        decision
    else {
        panic!("expected combined committed decision, got {decision:?}");
    };
    assert_eq!(receipt.task_receipt.new_head_commit_seq, 1);
    assert_ne!(
        receipt.task_receipt.new_effect_history_root,
        empty_effect_history_root(),
        "two closed effect slots must advance the durable effect history"
    );
    assert_eq!(
        mixed
            .authority
            .inspect_task(task_id())
            .expect("task")
            .head_effect_history_root,
        receipt.task_receipt.new_effect_history_root
    );
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
        (&nested[0], &reservations[0]),
        (&nested[1], &reservations[1]),
    ] {
        let aggregate = owners
            .resource
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
    assert_eq!(nested[0].consumptions.len(), 2);
    assert_eq!(nested[1].finalization.final_usage, 10);
    assert_eq!(nested[1].consumptions.len(), 1);
    assert_eq!(
        mixed
            .authority
            .inspect_resource_cost_receipts(task_id(), receipt.task_receipt.receipt_id)
            .expect("nested resource readback"),
        nested
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

    // And the unified read-side receipt over the durable rows produces
    // the deterministic digest of the SemanticResource variant.
    let rebuilt = SemanticResourceTaskCommitReceipt {
        task_receipt: mixed
            .authority
            .inspect_receipt(task_id(), receipt.task_receipt.receipt_id)
            .expect("receipt readback"),
        semantic_publications: vec![expected_publication],
        resource_cost_receipts: nested.clone(),
    };
    assert_eq!(rebuilt, *receipt);
    assert_eq!(
        TaskCommitReceipt::SemanticResource((*receipt).clone()).commit_receipt_digest(),
        TaskCommitReceipt::SemanticResource(rebuilt).commit_receipt_digest()
    );

    assert_eq!(raw_count(&fixture, "SELECT COUNT(*) FROM task_receipts"), 1);
    assert_eq!(
        raw_count(
            &fixture,
            "SELECT COUNT(*) FROM task_semantic_publication_receipts"
        ),
        1
    );
    assert_eq!(
        raw_count(&fixture, "SELECT COUNT(*) FROM task_resource_cost_receipts"),
        2
    );
    assert_eq!(
        raw_count(
            &fixture,
            "SELECT COUNT(*) FROM task_resource_cost_consumptions"
        ),
        3
    );
    assert_eq!(
        raw_count(
            &fixture,
            "SELECT COUNT(*) FROM task_artifact_publication_receipts"
        ),
        0,
        "the mixed permit never entered the Task-side artifact ladder"
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn six_domain_replay_after_restart_reads_only_task_rows() {
    // Given a committed six-domain finalize.
    let fixture = Fixture::new("replay");
    let owners = owners(&fixture);
    let reservations = two_reservations(&owners.resource, 0xc1);
    let (committed, plan_id) = {
        let mixed = setup_task_and_permit(&fixture, &owners, &reservations);
        close_effect_slot(&mixed, 0, 0xc2, [0xc3; 32]);
        close_effect_slot(&mixed, 1, 0xc4, [0xc5; 32]);
        let (plan_id, _) = drive_semantic_to_ready(&mixed, &owners);
        drive_artifact_owner_evidence(&mixed, &owners);
        owners
            .resource
            .settle(&reservations[0], &[(1, 30), (2, 37)], 37, 0xc6);
        owners
            .resource
            .settle(&reservations[1], &[(1, 10)], 10, 0xc7);
        activate_operation(&owners);
        let decision = six_domain_finalize(
            &mixed.authority,
            mixed.permit.permit_id,
            &owners.semantic_seed.0,
            &owners.resource.authority,
            &owners.operation_store,
            plan_id,
            1_700,
        )
        .expect("six-domain combined finalize");
        let FinalizeSpecDecision::Combined(SemanticResourceFinalizeDecision::Committed(receipt)) =
            decision
        else {
            panic!("expected combined committed decision");
        };
        (*receipt, plan_id)
    };
    drop(owners);

    // When the Task authority is reopened and the same finalize is
    // replayed against fresh, unrelated, EMPTY owner authorities for all
    // three finalize-gate domains, the replay must succeed byte-equal
    // without reading any owner.
    let reopened = fixture.open_task();
    let empty_root = std::env::temp_dir().join(format!(
        "nlos-task-six-domain-replay-empty-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let empty_semantic = SemanticAuthority::open(&empty_root).expect("empty semantic");
    let empty_resource = ResourceAuthority::open(&empty_root).expect("empty resource");
    let empty_operation_store =
        nlos_store::SqliteOperationStore::open(empty_root.join("operation-empty.sqlite3"))
            .expect("empty operation");
    let replay = reopened
        .finalize_commit_v3_with_spec(
            finalize_request(committed.task_receipt.permit_id.expect("permit"), 9_999),
            FinalizeSpec {
                semantic_authority: Some(&empty_semantic),
                semantic_plan: Some(plan_id),
                persisted_envelope: None,
                authority_lease: None,
                resource_authority: Some(&empty_resource),
                operation_authority: Some(&empty_operation_store),
            },
        )
        .expect("replay");
    let FinalizeSpecDecision::Combined(SemanticResourceFinalizeDecision::Replayed(replayed)) =
        replay
    else {
        panic!("expected combined replayed decision, got {replay:?}");
    };
    assert_eq!(*replayed, committed);
    assert_eq!(
        TaskCommitReceipt::SemanticResource((*replayed).clone()).commit_receipt_digest(),
        TaskCommitReceipt::SemanticResource(committed.clone()).commit_receipt_digest()
    );
    let _ = std::fs::remove_dir_all(&empty_root);

    // And no duplicate rows were appended anywhere.
    assert_eq!(raw_count(&fixture, "SELECT COUNT(*) FROM task_receipts"), 1);
    assert_eq!(
        raw_count(
            &fixture,
            "SELECT COUNT(*) FROM task_semantic_publication_receipts"
        ),
        1
    );
    assert_eq!(
        raw_count(&fixture, "SELECT COUNT(*) FROM task_resource_cost_receipts"),
        2
    );
    assert_eq!(
        raw_count(
            &fixture,
            "SELECT COUNT(*) FROM task_resource_cost_consumptions"
        ),
        3
    );
}

#[test]
fn six_domain_channel_face_rotation_between_seal_and_permit_fails_closed() {
    // Given a sealed six-domain write set whose Channel face was frozen
    // at the current generation, and a rotation that happens after the
    // seal but before the permit freeze.
    let fixture = Fixture::new("channel-rotate");
    let owners = owners(&fixture);
    let reservations = two_reservations(&owners.resource, 0xd1);
    let seal = setup_task_and_seal(&fixture, &owners, &reservations);
    owners
        .channel
        .rotate_channel(RotateChannelRequest {
            channel_id: owners.channel_record.channel_id,
            expected_generation: owners.channel_record.generation,
            expected_fencing_token: owners.channel_record.fencing_token,
            idempotency_key: IdempotencyKey::from_bytes([0xd2; 16]),
            rotated_at_ms: 1_250,
        })
        .expect("rotate channel");

    // Then the permit freeze fails closed on the drifted Channel proof
    // and leaves no permit row behind.
    let spec = attempt_spec();
    let permit_request = PermitRequest {
        task_id: task_id(),
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        write_set_root: seal.sealed.write_set_root,
        planned_effects: seal.sealed.planned_effects.clone(),
        idempotency_key: IdempotencyKey::from_bytes([0xac; 16]),
        valid_until_ms: 9_000,
        requested_at_ms: 1_300,
    };
    let permit_authorities = Authorities {
        artifact: Some(&owners.artifact),
        process: None,
        semantic: None,
        resource: Some(&owners.resource.authority),
        operation: Some(&owners.operation_store),
        channel: Some(&owners.channel),
    };
    assert!(matches!(
        seal.authority
            .request_commit_permit_with_authorities_struct(permit_authorities, permit_request),
        Err(TaskStoreError::TaskWriteSetConflict {
            reason: "Channel endpoint proof differs before permit freeze"
        })
    ));
    // And the sealed six-face record itself is untouched durable state.
    assert_eq!(
        seal.authority
            .inspect_task_write_set(task_id(), IdempotencyKey::from_bytes([0xab; 16]))
            .expect("write set replay after refused freeze"),
        seal.sealed
    );
}
