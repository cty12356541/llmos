//! W30-A (B3-3): kill-window fault matrix for the six-domain mixed
//! `TaskWriteSet` (`tests/six_domain_write_set_closure.rs` fixture).
//!
//! Harness and fixtures follow the established matrices exactly
//! (`tests/resource_bridge_fault_injection.rs` W/WE series: kill-9 child
//! processes synchronized through piped `READY` markers — never sleeps,
//! `FAULT_LOCK` process-wide serialization, `PowerLossAfter` commit-point
//! phases, typed error assertions, raw table counts,
//! `PRAGMA integrity_check` per row).
//!
//! One row per domain face of the six-domain write set. Each kill-window
//! child parks at a rest point where every prior commit is durable, and
//! every owner-side step is idempotent, so the parent can re-run the same
//! drive to complete the missing domain after the kill:
//!
//! - **Effect** — crash before the slot outcomes: the Combined rung
//!   refuses with typed `OutstandingEffectSlots`, zero terminal rows;
//!   closing both slots after the restart converges to the unique
//!   terminal.
//! - **Semantic** — crash after the plan reaches Publishing but before the
//!   owner publication: typed `SemanticCommitPlanNotReady`; publishing and
//!   recording after the restart converges.
//! - **Resource** — crash before the owner settles: typed
//!   `ResourceParticipantAuthority`; settling after the restart converges.
//! - **Operation** — crash after the owner prepared the dispatch but
//!   before activation: typed `OperationDispatchNotActivated` naming the
//!   Operation; activating after the restart converges.
//! - **Artifact** — both windows: crash before any artifact drive (the
//!   Combined rung has NO artifact gate, so the terminal commits while
//!   the declared write stays unpublished — pinned honestly), and crash
//!   after the plan reached READY with durable receipts (the terminal
//!   commits and the plan stays READY, unlinked).
//! - **Channel** — crash after seal+permit (the endpoint proof is the
//!   whole owner evidence — there is no publication protocol): driving
//!   every other domain after the restart converges, and the replay
//!   trusts the Task rows only.
//! - **Terminal commit point** — `PowerLossAfter` (invisible phase) and
//!   kill-9 after the commit (visible phase): the whole terminal —
//!   receipt, nested Semantic/Resource rows, permit close, head advance —
//!   appears and disappears together; redo is byte-equal and replay
//!   against EMPTY owners is byte-equal.
//!
//! **Crash semantics disclaimer** (as in every prior matrix): kill-9
//! simulates *process* crashes; the OS page cache survives process death.
//! Writes the kernel accepted but the disk never saw are covered by
//! [`FaultMode::PowerLossAfter`]. The matrix proves verify-then-commit
//! idempotent convergence of the Task terminal transaction from the
//! durable prefix, never cross-authority atomicity.

use std::error::Error as _;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use nlos_capability::CapabilityTarget;
use nlos_channel::{ChannelAuthority, ChannelDecision, CreateChannelRequest};
use nlos_resource::{
    AccountRecord, CreateAccountRequest, CreateQuoteRequest, DriverRecord, QuoteRecord,
    RegisterDriverRequest, ReservationRecord, ReserveRequest, ResourceAuthority, ResourceDemand,
};
use nlos_semantic::{PublishSemanticPublicationRequest, SemanticAuthority};
use nlos_store_fault::FaultMode;
use nlos_task::{
    ArtifactPublicationAuthorizationDecision, ArtifactPublicationExpectation, AttemptSpec,
    Authorities, DispatchRequest, EffectPermitDecision, EffectPermitRequest, FinalizeRequest,
    FinalizeRequestV3, FinalizeSpec, FinalizeSpecDecision, LogicalEffectDescriptor,
    NestedArtifactPublicationReceipt, NestedSemanticPublicationReceipt, Outcome, OutcomeRequest,
    ParticipantRegistryBinding, PermitDecision, PermitRecord, PermitRequest, PermitState,
    PlanArtifactCommitRequest, PlanSemanticCommitRequest, PlannedEffect,
    RecordArtifactPublicationsRequest, RecordSemanticPublicationsRequest, SemanticCommitPlanId,
    SemanticCommitPlanState, SemanticResourceFinalizeDecision, SemanticResourceTaskCommitReceipt,
    SlotState, SnapshotBundle, SnapshotConsistency, SqliteTaskAuthority, TaskSnapshotReceiptSpec,
    TaskSpec, TaskStoreError, TaskWriteSetArtifactRead, TaskWriteSetArtifactWriteRequest,
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

const VFS_NAME: &str = "nlos-task-six-domain-fault";

static FAULT_LOCK: Mutex<()> = Mutex::new(());

fn fault_lock() -> MutexGuard<'static, ()> {
    FAULT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

static NEXT: AtomicU64 = AtomicU64::new(1);

/// Deterministic directory layout shared by in-process fixtures and the
/// kill-9 child scenarios (the child rebuilds the same fixture from the
/// root alone).
struct Layout(PathBuf);

impl Layout {
    fn new(base: PathBuf) -> Self {
        Self(base)
    }

    fn base(&self) -> &Path {
        &self.0
    }

    fn task_path(&self) -> PathBuf {
        self.0.join("task-authority.sqlite3")
    }

    fn semantic_root(&self) -> PathBuf {
        self.0.join("semantic")
    }

    fn resource_root(&self) -> PathBuf {
        self.0.join("resource")
    }

    fn artifact_root(&self) -> PathBuf {
        self.0.join("artifact")
    }

    fn operation_path(&self) -> PathBuf {
        self.0.join("operation")
    }

    fn channel_root(&self) -> PathBuf {
        self.0.join("channel")
    }

    fn empty_replay_root(&self) -> PathBuf {
        self.0.join("replay-empty")
    }
}

/// RAII test root: one fresh directory tree per scenario, removed on drop.
struct TestRoot {
    layout: Layout,
}

impl TestRoot {
    fn new(label: &str) -> Self {
        let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "nlos-task-six-domain-fault-{label}-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&base).expect("create test root");
        Self {
            layout: Layout::new(base),
        }
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(self.layout.base());
    }
}

fn open_task_shim(layout: &Layout) -> SqliteTaskAuthority {
    nlos_store_fault::register(VFS_NAME).expect("register fault vfs");
    SqliteTaskAuthority::open_with_vfs(layout.task_path(), Some(VFS_NAME))
        .expect("open task authority via fault vfs")
}

fn reopen_task(layout: &Layout) -> SqliteTaskAuthority {
    SqliteTaskAuthority::open(layout.task_path()).expect("reopen task authority")
}

/// Runs `PRAGMA integrity_check` through an independent raw connection.
fn assert_integrity(path: &Path) {
    let connection = Connection::open(path).expect("open for integrity check");
    let result: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("run integrity_check");
    assert_eq!(result, "ok", "integrity_check must pass");
}

fn raw_count(path: &Path, sql: &str) -> i64 {
    let connection = Connection::open(path).expect("open raw reader");
    connection
        .query_row(sql, [], |row| row.get(0))
        .expect("count rows")
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

fn error_chain(error: &TaskStoreError) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(" <- ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// Asserts no subset of the terminal-transaction facts survived a refused
/// finalize (the Semantic publication rows are pre-terminal plan-READY
/// state and stay durable; their plan binding is asserted separately).
fn assert_no_terminal_rows(path: &Path) {
    assert_eq!(raw_count(path, "SELECT COUNT(*) FROM task_receipts"), 0);
    assert_eq!(
        raw_count(path, "SELECT COUNT(*) FROM task_resource_cost_receipts"),
        0
    );
    assert_eq!(
        raw_count(path, "SELECT COUNT(*) FROM task_resource_cost_consumptions"),
        0
    );
}

/// Asserts the full terminal row set of the six-domain Combined commit:
/// one receipt, one Semantic publication, two Resource cost receipts with
/// three consumptions.
fn assert_full_terminal_rows(path: &Path) {
    assert_eq!(raw_count(path, "SELECT COUNT(*) FROM task_receipts"), 1);
    assert_eq!(
        raw_count(
            path,
            "SELECT COUNT(*) FROM task_semantic_publication_receipts"
        ),
        1
    );
    assert_eq!(
        raw_count(path, "SELECT COUNT(*) FROM task_resource_cost_receipts"),
        2
    );
    assert_eq!(
        raw_count(path, "SELECT COUNT(*) FROM task_resource_cost_consumptions"),
        3
    );
}

type SemanticSeed = (
    SemanticAuthority,
    SemanticEventId,
    ReceiptId,
    ReceiptId,
    NamespaceId,
);

fn seed_semantic_authority(root: &Path) -> SemanticSeed {
    // The raw seed rows are byte-identical on every re-run, so OR IGNORE
    // makes the parent's re-derivation after a child kill an exact replay.
    let semantic = SemanticAuthority::open(root).expect("open Semantic authority");
    let event_id = SemanticEventId::from_bytes([0x70; 32]);
    let admission_receipt_id = ReceiptId::from_bytes([0x71; 16]);
    let durability_receipt_id = ReceiptId::from_bytes([0x72; 16]);
    let target = NamespaceId::from_bytes([0x73; 16]);
    let raw = Connection::open(root.join("semantic-authority.db")).expect("open raw Semantic db");
    raw.execute(
        "INSERT OR IGNORE INTO content_objects (content_digest, media_type, exact_bytes)
         VALUES (?1, ?2, ?3)",
        rusqlite::params![[0x74u8; 32].as_slice(), "text/plain", b"semantic"],
    )
    .expect("insert content");
    raw.execute(
        "INSERT OR IGNORE INTO semantic_events (
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
        "INSERT OR IGNORE INTO event_log (event_id) VALUES (?1)",
        [event_id.as_bytes().as_slice()],
    )
    .expect("insert event log");
    raw.execute(
        "INSERT OR IGNORE INTO admission_receipts (
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
        "INSERT OR IGNORE INTO durability_receipts (
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

    fn settle(
        &self,
        reservation: &ReservationRecord,
        consumptions: &[(u64, u64)],
        final_usage: u64,
        seed: u8,
    ) {
        if matches!(
            self.authority
                .inspect_reservation(reservation.reservation_id)
                .expect("inspect reservation")
                .state,
            nlos_resource::ReservationState::Finalized
        ) {
            return;
        }
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
            .expect("owner finalize");
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

/// Every owner authority the six-domain write set needs. All idempotency
/// keys are fixed, so re-running the constructors against the durable
/// roots (parent after child kill) replays the exact same records.
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

fn owners(layout: &Layout) -> Owners {
    let artifact = nlos_artifact::ArtifactStore::open(layout.artifact_root()).expect("artifact");
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
    let semantic_seed = seed_semantic_authority(&layout.semantic_root());
    let resource = OwnerFixture::new(&layout.resource_root(), 0x94);
    fs::create_dir_all(layout.operation_path().parent().expect("operation dir"))
        .expect("create operation dir");
    let operation_store =
        nlos_store::SqliteOperationStore::open(layout.operation_path()).expect("operation store");
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
    let channel = ChannelAuthority::open(layout.channel_root()).expect("channel authority");
    let channel_record = match channel
        .create_channel(CreateChannelRequest {
            capacity_bytes: 4096,
            policy_digest: [0x98; 32],
            idempotency_key: IdempotencyKey::from_bytes([0x99; 16]),
            created_at_ms: 1_030,
        })
        .expect("create channel")
    {
        ChannelDecision::Created(record) | ChannelDecision::Replayed(record) => record,
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

/// The task-side driving context: the authority handle plus the durable
/// permit and sealed write set (re-read after a kill).
struct Drive {
    authority: SqliteTaskAuthority,
    permit: PermitRecord,
    sealed: TaskWriteSetRecord,
    reservations: Vec<ReservationRecord>,
}

impl Drive {
    /// Re-opens the task authority and re-reads the durable permit, the
    /// sealed six-domain write set (fixed idempotency key), and re-derives
    /// the reservations through idempotent owner replay.
    fn after_kill(layout: &Layout, owners: &Owners, permit_id: CommitPermitId) -> Self {
        let authority = reopen_task(layout);
        let permit = authority
            .inspect_permit(task_id(), permit_id)
            .expect("permit survived the kill");
        let sealed = authority
            .inspect_task_write_set(task_id(), IdempotencyKey::from_bytes([0xab; 16]))
            .expect("sealed write set survived the kill");
        Self {
            authority,
            permit,
            sealed,
            reservations: two_reservations(&owners.resource, 0xb1),
        }
    }
}

#[allow(clippy::too_many_lines)]
fn setup_task_and_permit(
    layout: &Layout,
    owners: &Owners,
    open_task: impl FnOnce(&Layout) -> SqliteTaskAuthority,
) -> Drive {
    let reservations = two_reservations(&owners.resource, 0xb1);
    let (_, event_id, _, durability_receipt_id, target) = &owners.semantic_seed;
    let authority = open_task(layout);
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
        .expect("artifact head participant");

    let sealed = authority
        .seal_task_write_set_with_authorities_struct(
            Authorities {
                artifact: Some(&owners.artifact),
                process: None,
                semantic: Some(&owners.semantic_seed.0),
                resource: Some(&owners.resource.authority),
                operation: Some(&owners.operation_store),
                channel: Some(&owners.channel),
            },
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
    let permit_request = PermitRequest {
        task_id: task_id(),
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        write_set_root: sealed.write_set_root,
        planned_effects: sealed.planned_effects.clone(),
        idempotency_key: IdempotencyKey::from_bytes([0xac; 16]),
        valid_until_ms: 9_000,
        requested_at_ms: 1_300,
    };
    let decision = authority
        .request_commit_permit_with_authorities_struct(
            Authorities {
                artifact: Some(&owners.artifact),
                process: None,
                semantic: None,
                resource: Some(&owners.resource.authority),
                operation: Some(&owners.operation_store),
                channel: Some(&owners.channel),
            },
            permit_request,
        )
        .expect("struct permit");
    let PermitDecision::Issued(permit) = decision else {
        panic!("expected issued permit, got {decision:?}");
    };
    Drive {
        authority,
        permit: *permit,
        sealed,
        reservations,
    }
}

fn close_effect_slot(drive: &Drive, effect_seq: u64, key_seed: u8, closure: [u8; 32]) {
    if matches!(
        drive
            .authority
            .inspect_effect_slot(drive.permit.permit_id, effect_seq)
            .expect("inspect effect slot")
            .state,
        SlotState::EffectClosed
    ) {
        return;
    }
    let spec = attempt_spec();
    let permit = match drive
        .authority
        .request_effect_permit(EffectPermitRequest {
            task_id: task_id(),
            attempt_id: spec.attempt_id,
            attempt_generation: spec.attempt_generation,
            permit_id: drive.permit.permit_id,
            permit_epoch: drive.permit.permit_epoch,
            effect_seq,
            idempotency_key: IdempotencyKey::from_bytes([key_seed; 16]),
            valid_until_ms: 9_000,
            requested_at_ms: 1_320,
        })
        .expect("effect permit")
    {
        EffectPermitDecision::Issued(record) | EffectPermitDecision::Replayed(record) => record,
    };
    drive
        .authority
        .consume_dispatch_token(DispatchRequest {
            task_id: task_id(),
            attempt_id: spec.attempt_id,
            attempt_generation: spec.attempt_generation,
            permit_id: drive.permit.permit_id,
            permit_epoch: drive.permit.permit_epoch,
            effect_permit_id: permit.effect_permit_id,
            dispatch_token: permit.one_shot_dispatch_token,
            dispatched_at_ms: 1_330,
        })
        .expect("dispatch token");
    drive
        .authority
        .record_effect_outcome(OutcomeRequest {
            task_id: task_id(),
            attempt_id: spec.attempt_id,
            attempt_generation: spec.attempt_generation,
            permit_id: drive.permit.permit_id,
            permit_epoch: drive.permit.permit_epoch,
            effect_seq,
            outcome: Outcome::Closed {
                authoritative_closure_digest: closure,
            },
            recorded_at_ms: 1_340,
        })
        .expect("effect outcome");
}

fn close_both_effect_slots(drive: &Drive) {
    close_effect_slot(drive, 0, 0xb2, [0xb3; 32]);
    close_effect_slot(drive, 1, 0xb4, [0xb5; 32]);
}

/// Semantic plan Planned → authorized; `publish` additionally performs the
/// owner publication and records it (→ READY). Every step replays
/// idempotently, so the parent can re-run it after a kill. Returns the
/// plan id.
fn drive_semantic(drive: &Drive, owners: &Owners, publish: bool) -> SemanticCommitPlanId {
    let (_, event_id, admission_receipt_id, durability_receipt_id, target) = &owners.semantic_seed;
    let spec = attempt_spec();
    let plan = drive
        .authority
        .plan_semantic_commit(PlanSemanticCommitRequest {
            task_id: task_id(),
            attempt_id: spec.attempt_id,
            attempt_generation: spec.attempt_generation,
            permit_id: drive.permit.permit_id,
            idempotency_key: IdempotencyKey::from_bytes([0xbb; 16]),
            planned_at_ms: 1_350,
        })
        .expect("plan")
        .record()
        .clone();
    assert!(matches!(
        drive
            .authority
            .authorize_semantic_publication(plan.plan_id, 1_360)
            .expect("authorize")
            .record()
            .state,
        SemanticCommitPlanState::Publishing | SemanticCommitPlanState::Ready
    ));
    if !publish {
        return plan.plan_id;
    }
    let owner = owners
        .semantic_seed
        .0
        .publish_semantic_publication(PublishSemanticPublicationRequest {
            task_id: task_id(),
            permit_id: drive.permit.permit_id,
            write_set_root: drive.sealed.write_set_root,
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
    let progress = drive
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
    plan.plan_id
}

/// Artifact face staged + planned + authorized + published + recorded
/// (→ READY). Every step replays idempotently. Returns the plan id.
fn drive_artifact(drive: &Drive, owners: &Owners) -> nlos_task::ArtifactCommitPlanId {
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
            permit_id: drive.permit.permit_id,
            write_set_root: nlos_artifact::ContentDigest::from_bytes(drive.sealed.write_set_root),
            idempotency_key: owners.staging_key,
            created_at_ms: 1_385,
        })
        .expect("owner stage");
    let spec = attempt_spec();
    let plan = drive
        .authority
        .plan_artifact_commit(PlanArtifactCommitRequest {
            task_id: task_id(),
            attempt_id: spec.attempt_id,
            attempt_generation: spec.attempt_generation,
            permit_id: drive.permit.permit_id,
            expectations: vec![expectation],
            idempotency_key: IdempotencyKey::from_bytes([0xbc; 16]),
            planned_at_ms: 1_386,
        })
        .expect("artifact plan")
        .record()
        .clone();
    let authorization = drive
        .authority
        .authorize_artifact_publication(plan.plan_id, 1_387)
        .expect("authorize artifact publication");
    assert!(matches!(
        authorization,
        ArtifactPublicationAuthorizationDecision::Authorized(_)
            | ArtifactPublicationAuthorizationDecision::Replayed(_)
    ));
    let receipt = owners
        .artifact
        .publish_staged_revision(nlos_artifact::PublishStagedRevisionRequest {
            staging_id,
            task_id: task_id(),
            permit_id: drive.permit.permit_id,
            write_set_root: nlos_artifact::ContentDigest::from_bytes(drive.sealed.write_set_root),
            published_at_ms: 1_388,
        })
        .expect("owner publish")
        .receipt()
        .clone();
    let nested = NestedArtifactPublicationReceipt {
        receipt_id: receipt.receipt_id,
        staging_id: receipt.staging_id.into_bytes(),
        artifact_id: receipt.artifact_id,
        revision: receipt.revision,
        digest: receipt.digest.into_bytes(),
        size_bytes: receipt.size_bytes,
        task_id: receipt.task_id,
        permit_id: receipt.permit_id,
        write_set_root: receipt.write_set_root.into_bytes(),
        prior_head_revision: receipt.prior_head_revision,
        prior_head_digest: receipt
            .prior_head_digest
            .map(nlos_artifact::ContentDigest::into_bytes),
        new_head_revision: receipt.new_head_revision,
        new_head_digest: receipt.new_head_digest.into_bytes(),
        created_at_ms: i64::try_from(receipt.created_at_ms).expect("created_at fits i64"),
    };
    let progress = drive
        .authority
        .record_artifact_publications(RecordArtifactPublicationsRequest {
            plan_id: plan.plan_id,
            receipts: vec![nested],
            observed_at_ms: 1_389,
        })
        .expect("record artifact publications");
    assert_eq!(
        progress.plan.state,
        nlos_task::ArtifactCommitPlanState::Ready
    );
    plan.plan_id
}

fn settle_reservations(drive: &Drive, owners: &Owners) {
    owners
        .resource
        .settle(&drive.reservations[0], &[(1, 30), (2, 37)], 37, 0xb6);
    owners
        .resource
        .settle(&drive.reservations[1], &[(1, 10)], 10, 0xb7);
}

/// Prepares (and optionally activates) the Operation owner dispatch.
/// Both steps replay idempotently.
fn drive_operation(owners: &Owners, activate: bool) {
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
    if !activate {
        return;
    }
    owners
        .operation_store
        .activate_dispatch(preparation)
        .expect("activate dispatch");
}

fn six_domain_finalize(
    drive: &Drive,
    owners: &Owners,
    plan_id: SemanticCommitPlanId,
    finalized_at_ms: i64,
) -> Result<FinalizeSpecDecision, TaskStoreError> {
    drive.authority.finalize_commit_v3_with_spec(
        finalize_request(drive.permit.permit_id, finalized_at_ms),
        FinalizeSpec {
            semantic_authority: Some(&owners.semantic_seed.0),
            semantic_plan: Some(plan_id),
            persisted_envelope: None,
            authority_lease: None,
            resource_authority: Some(&owners.resource.authority),
            operation_authority: Some(&owners.operation_store),
        },
    )
}

fn committed_combined(decision: FinalizeSpecDecision) -> SemanticResourceTaskCommitReceipt {
    match decision {
        FinalizeSpecDecision::Combined(SemanticResourceFinalizeDecision::Committed(receipt)) => {
            *receipt
        }
        other => panic!("expected combined committed decision, got {other:?}"),
    }
}

fn replayed_combined(decision: FinalizeSpecDecision) -> SemanticResourceTaskCommitReceipt {
    match decision {
        FinalizeSpecDecision::Combined(SemanticResourceFinalizeDecision::Replayed(receipt)) => {
            *receipt
        }
        other => panic!("expected combined replayed decision, got {other:?}"),
    }
}

/// Replays the committed finalize against EMPTY owner authorities for all
/// three finalize-gate domains — the "replay trusts Task rows only"
/// assertion of every row.
fn replay_against_empty_owners(
    layout: &Layout,
    permit_id: CommitPermitId,
    plan_id: SemanticCommitPlanId,
) -> SemanticResourceTaskCommitReceipt {
    let reopened = reopen_task(layout);
    let empty_root = layout.empty_replay_root();
    let empty_semantic = SemanticAuthority::open(&empty_root).expect("empty semantic");
    let empty_resource = ResourceAuthority::open(&empty_root).expect("empty resource");
    let empty_operation =
        nlos_store::SqliteOperationStore::open(empty_root.join("operation.sqlite3"))
            .expect("empty operation");
    replayed_combined(
        reopened
            .finalize_commit_v3_with_spec(
                finalize_request(permit_id, 9_999),
                FinalizeSpec {
                    semantic_authority: Some(&empty_semantic),
                    semantic_plan: Some(plan_id),
                    persisted_envelope: None,
                    authority_lease: None,
                    resource_authority: Some(&empty_resource),
                    operation_authority: Some(&empty_operation),
                },
            )
            .expect("empty-owner replay"),
    )
}

/// Common row tail: re-drive everything (all steps idempotent), finalize,
/// assert the unique terminal, and replay against empty owners. The
/// re-derived Semantic plan must equal the durable one whenever the child
/// had planned it (nonzero marker plan id).
fn complete_converge_and_replay(
    root: &TestRoot,
    owners: &Owners,
    permit_id: CommitPermitId,
    marker_plan: SemanticCommitPlanId,
) -> SemanticResourceTaskCommitReceipt {
    let drive = Drive::after_kill(&root.layout, owners, permit_id);
    close_both_effect_slots(&drive);
    let plan_id = drive_semantic(&drive, owners, true);
    if *marker_plan.as_bytes() != [0_u8; 16] {
        assert_eq!(
            plan_id, marker_plan,
            "re-derived plan must equal the durable plan of the killed child"
        );
    }
    let _ = drive_artifact(&drive, owners);
    settle_reservations(&drive, owners);
    drive_operation(owners, true);
    let committed = committed_combined(
        six_domain_finalize(&drive, owners, plan_id, 1_700)
            .expect("post-restart combined finalize"),
    );
    assert_eq!(committed.task_receipt.new_head_commit_seq, 1);
    assert_eq!(
        drive
            .authority
            .inspect_permit(task_id(), permit_id)
            .expect("permit")
            .state,
        PermitState::Closed
    );
    assert_full_terminal_rows(&root.layout.task_path());
    assert_integrity(&root.layout.task_path());
    let replay = replay_against_empty_owners(&root.layout, permit_id, plan_id);
    assert_eq!(replay, committed, "replay must equal the durable commit");
    committed
}

// ---------------------------------------------------------------------------
// kill-9 child-process harness (resource_bridge_fault_injection.rs 范式)
// ---------------------------------------------------------------------------

fn spawn_child(scenario: &str, layout: &Layout) -> Child {
    Command::new(std::env::current_exe().expect("current test executable"))
        .args(["--exact", "crash_child_helper", "--nocapture"])
        .env("NLOS_TASK_SIX_DOMAIN_CRASH_CHILD_SCENARIO", scenario)
        .env(
            "NLOS_TASK_SIX_DOMAIN_CRASH_CHILD_ROOT",
            layout.base().as_os_str(),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn crash child")
}

fn await_marker(child: &mut Child) -> String {
    let stdout = child.stdout.take().expect("piped stdout");
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut lines = BufReader::new(stdout).lines();
        let mut marker = None;
        for line in lines.by_ref() {
            match line {
                Ok(line) if line.starts_with("READY") => {
                    marker = Some(line);
                    break;
                }
                Ok(_) => {}
                Err(error) => {
                    let _ = sender.send(Err(error.to_string()));
                    return;
                }
            }
        }
        let _ = sender.send(marker.ok_or_else(|| "child exited without READY".to_string()));
    });
    match receiver.recv_timeout(Duration::from_mins(1)) {
        Ok(Ok(marker)) => marker,
        other => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("child did not report READY: {other:?}");
        }
    }
}

fn kill_and_reap(child: &mut Child) {
    child.kill().expect("force-terminate child");
    let status = child.wait().expect("wait child");
    assert!(
        !status.success(),
        "killed child must not exit cleanly: {status}"
    );
}

fn announce(marker: &str) {
    println!("{marker}");
    std::io::stdout().flush().expect("flush marker");
}

fn hex_encode(value: &[u8]) -> String {
    use std::fmt::Write as _;
    value
        .iter()
        .fold(String::with_capacity(value.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

fn hex_decode16(text: &str) -> [u8; 16] {
    assert_eq!(text.len(), 32, "id hex is 16 bytes");
    let mut decoded = [0_u8; 16];
    for (index, slot) in decoded.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).expect("hex byte");
    }
    decoded
}

/// Decodes the `READY <permit-hex> <semantic-plan-hex>` marker shared by
/// every six-domain crash-child scenario (zero plan id when the child
/// stopped before planning).
fn decode_marker(marker: &str) -> (CommitPermitId, SemanticCommitPlanId) {
    let mut parts = marker
        .trim()
        .strip_prefix("READY ")
        .expect("marker")
        .split(' ');
    let permit = CommitPermitId::from_bytes(hex_decode16(parts.next().expect("permit id")));
    let plan = SemanticCommitPlanId::from_bytes(hex_decode16(parts.next().expect("plan id")));
    assert!(parts.next().is_none(), "marker carries exactly two ids");
    (permit, plan)
}

/// Child entry point. Runs only when spawned by a parent test with the
/// scenario environment set; a no-op in the normal test run.
#[test]
fn crash_child_helper() {
    let (Ok(scenario), Ok(root)) = (
        std::env::var("NLOS_TASK_SIX_DOMAIN_CRASH_CHILD_SCENARIO"),
        std::env::var("NLOS_TASK_SIX_DOMAIN_CRASH_CHILD_ROOT"),
    ) else {
        return;
    };
    let layout = Layout::new(PathBuf::from(root));
    match scenario.as_str() {
        "effect-outstanding" => child_six_domain(&layout, ChildStop::BeforeEffectOutcomes),
        "semantic-unpublished" => child_six_domain(&layout, ChildStop::BeforeSemanticPublish),
        "resource-unsettled" => child_six_domain(&layout, ChildStop::BeforeResourceSettle),
        "operation-unactivated" => child_six_domain(&layout, ChildStop::BeforeOperationActivate),
        "artifact-undriven" => child_six_domain(&layout, ChildStop::BeforeArtifactDrive),
        "artifact-ready" => child_six_domain(&layout, ChildStop::AfterArtifactReady),
        "channel-bound" => child_six_domain(&layout, ChildStop::AfterSealAndPermit),
        "terminal-committed" => child_six_domain(&layout, ChildStop::AfterTerminal),
        other => panic!("unknown crash child scenario {other}"),
    }
}

/// Where each kill-window child parks. Everything before the stop point is
/// fully committed durable state; everything after never ran.
#[derive(Clone, Copy)]
enum ChildStop {
    /// Seal + permit only (Channel row: the endpoint proof is the whole
    /// owner evidence; nothing else driven).
    AfterSealAndPermit,
    /// Effects not closed; everything else driven.
    BeforeEffectOutcomes,
    /// Semantic plan at Publishing, owner not published; everything else
    /// driven.
    BeforeSemanticPublish,
    /// Reservations still Reserved on the owner; everything else driven.
    BeforeResourceSettle,
    /// Operation prepared but never activated; everything else driven.
    BeforeOperationActivate,
    /// Artifact face completely undriven; everything else driven.
    BeforeArtifactDrive,
    /// Artifact plan READY with durable receipts; everything else driven,
    /// terminal NOT run.
    AfterArtifactReady,
    /// Everything driven INCLUDING the Combined terminal commit.
    AfterTerminal,
}

fn child_six_domain(layout: &Layout, stop: ChildStop) -> ! {
    let owners = owners(layout);
    let drive = setup_task_and_permit(layout, &owners, |layout| {
        SqliteTaskAuthority::open(layout.task_path()).expect("open child task authority")
    });
    let mut plan_id = None;
    if !matches!(stop, ChildStop::AfterSealAndPermit) {
        if !matches!(stop, ChildStop::BeforeEffectOutcomes) {
            close_both_effect_slots(&drive);
        }
        plan_id = Some(drive_semantic(
            &drive,
            &owners,
            !matches!(stop, ChildStop::BeforeSemanticPublish),
        ));
        if !matches!(stop, ChildStop::BeforeArtifactDrive) {
            let _ = drive_artifact(&drive, &owners);
        }
        if !matches!(stop, ChildStop::BeforeResourceSettle) {
            settle_reservations(&drive, &owners);
        }
        match stop {
            ChildStop::BeforeOperationActivate => drive_operation(&owners, false),
            _ => drive_operation(&owners, true),
        }
    }
    if matches!(stop, ChildStop::AfterTerminal) {
        let plan_id = plan_id.expect("semantic plan driven");
        let decision = six_domain_finalize(&drive, &owners, plan_id, 1_700);
        assert!(matches!(
            decision,
            Ok(FinalizeSpecDecision::Combined(
                SemanticResourceFinalizeDecision::Committed(_)
            ))
        ));
    }
    announce(&format!(
        "READY {} {}",
        hex_encode(drive.permit.permit_id.as_bytes()),
        hex_encode(
            plan_id
                .map_or([0_u8; 16], |plan| *plan.as_bytes())
                .as_slice()
        )
    ));
    let _keepers = (drive, owners);
    loop {
        std::thread::park();
    }
}

// ---------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------

/// Effect 面：crash before owner evidence（两个 effect slot 尚无
/// outcome）——Combined rung typed `OutstandingEffectSlots` 拒绝、零终结行；
/// 重启后补齐 outcome，同一请求收敛唯一终态，重放只信 Task 行。
#[test]
#[allow(clippy::too_many_lines)]
fn effect_face_crash_before_outcomes_converges() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let root = TestRoot::new("effect-outstanding");
    let mut child = spawn_child("effect-outstanding", &root.layout);
    let marker = await_marker(&mut child);
    kill_and_reap(&mut child);
    let (permit_id, plan_id) = decode_marker(&marker);

    let owners = owners(&root.layout);
    let drive = Drive::after_kill(&root.layout, &owners, permit_id);
    assert!(matches!(
        six_domain_finalize(&drive, &owners, plan_id, 1_700),
        Err(TaskStoreError::OutstandingEffectSlots { count: 2 })
    ));
    assert_no_terminal_rows(&root.layout.task_path());
    assert_eq!(
        drive
            .authority
            .inspect_permit(task_id(), permit_id)
            .expect("permit")
            .state,
        PermitState::Issued
    );
    assert_integrity(&root.layout.task_path());
    drop(drive);

    complete_converge_and_replay(&root, &owners, permit_id, plan_id);
}

/// Semantic 面：crash after plan/authorize、before owner publication——
/// Combined rung typed `SemanticCommitPlanNotReady{Publishing}` 拒绝、零
/// 终结行；重启后 owner publish + record，同一请求收敛唯一终态，重放只信
/// Task 行。
#[test]
#[allow(clippy::too_many_lines)]
fn semantic_face_crash_before_owner_publication_converges() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let root = TestRoot::new("semantic-unpublished");
    let mut child = spawn_child("semantic-unpublished", &root.layout);
    let marker = await_marker(&mut child);
    kill_and_reap(&mut child);
    let (permit_id, plan_id) = decode_marker(&marker);

    let owners = owners(&root.layout);
    let drive = Drive::after_kill(&root.layout, &owners, permit_id);
    assert!(matches!(
        six_domain_finalize(&drive, &owners, plan_id, 1_700),
        Err(TaskStoreError::SemanticCommitPlanNotReady {
            state: SemanticCommitPlanState::Publishing
        })
    ));
    assert_no_terminal_rows(&root.layout.task_path());
    assert_eq!(
        raw_count(
            &root.layout.task_path(),
            "SELECT COUNT(*) FROM task_semantic_publication_receipts"
        ),
        0,
        "the unpublished plan must have consumed no publication receipts"
    );
    assert_eq!(
        drive
            .authority
            .inspect_semantic_commit_progress(plan_id)
            .expect("plan progress")
            .plan
            .state,
        SemanticCommitPlanState::Publishing
    );
    assert_integrity(&root.layout.task_path());
    drop(drive);

    complete_converge_and_replay(&root, &owners, permit_id, plan_id);
}

/// Resource 面：crash before owner settle（Reservations 仍 Reserved）——
/// Combined rung typed `ResourceParticipantAuthority` 拒绝、零终结行；
/// 重启后 owner settle，同一请求收敛唯一终态，重放只信 Task 行。
#[test]
#[allow(clippy::too_many_lines)]
fn resource_face_crash_before_owner_settle_converges() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let root = TestRoot::new("resource-unsettled");
    let mut child = spawn_child("resource-unsettled", &root.layout);
    let marker = await_marker(&mut child);
    kill_and_reap(&mut child);
    let (permit_id, plan_id) = decode_marker(&marker);

    let owners = owners(&root.layout);
    let drive = Drive::after_kill(&root.layout, &owners, permit_id);
    let refusal = six_domain_finalize(&drive, &owners, plan_id, 1_700)
        .expect_err("unsettled owner must fail closed");
    assert!(
        matches!(refusal, TaskStoreError::ResourceParticipantAuthority(_)),
        "expected the Resource owner gate, got: {}",
        error_chain(&refusal)
    );
    assert_no_terminal_rows(&root.layout.task_path());
    assert_eq!(
        drive
            .authority
            .inspect_permit(task_id(), permit_id)
            .expect("permit")
            .state,
        PermitState::Issued
    );
    assert_integrity(&root.layout.task_path());
    drop(drive);

    complete_converge_and_replay(&root, &owners, permit_id, plan_id);
}

/// Operation 面：crash after prepare、before activate——finalize 门前置
/// owner 复核 typed `OperationDispatchNotActivated`（命名 Operation）拒绝、
/// 零终结行；重启后 activate，同一请求收敛唯一终态，重放只信 Task 行。
#[test]
#[allow(clippy::too_many_lines)]
fn operation_face_crash_before_activation_converges() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let root = TestRoot::new("operation-unactivated");
    let mut child = spawn_child("operation-unactivated", &root.layout);
    let marker = await_marker(&mut child);
    kill_and_reap(&mut child);
    let (permit_id, plan_id) = decode_marker(&marker);

    let owners = owners(&root.layout);
    let drive = Drive::after_kill(&root.layout, &owners, permit_id);
    assert!(matches!(
        six_domain_finalize(&drive, &owners, plan_id, 1_700),
        Err(TaskStoreError::OperationDispatchNotActivated {
            operation_id,
            ..
        }) if operation_id == owners.operation.operation_id
    ));
    assert_no_terminal_rows(&root.layout.task_path());
    assert_eq!(
        drive
            .authority
            .inspect_permit(task_id(), permit_id)
            .expect("permit")
            .state,
        PermitState::Issued
    );
    assert_integrity(&root.layout.task_path());
    drop(drive);

    complete_converge_and_replay(&root, &owners, permit_id, plan_id);
}

/// Artifact 面窗口 i：crash before 任何 artifact drive——Combined rung
/// **没有** artifact 门：终结照常提交（诚实钉死该缺口），声明的 artifact
/// 写从未发布（owner head 仍为空、Task 侧零 publication 行）；重放只信
/// Task 行。
#[test]
#[allow(clippy::too_many_lines)]
fn artifact_face_crash_before_drive_terminal_commits_without_artifact_gate() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let root = TestRoot::new("artifact-undriven");
    let mut child = spawn_child("artifact-undriven", &root.layout);
    let marker = await_marker(&mut child);
    kill_and_reap(&mut child);
    let (permit_id, plan_id) = decode_marker(&marker);

    let owners = owners(&root.layout);
    let drive = Drive::after_kill(&root.layout, &owners, permit_id);
    let committed = committed_combined(
        six_domain_finalize(&drive, &owners, plan_id, 1_700)
            .expect("the Combined rung has no artifact gate — the terminal commits"),
    );
    assert_full_terminal_rows(&root.layout.task_path());
    assert_eq!(
        raw_count(
            &root.layout.task_path(),
            "SELECT COUNT(*) FROM task_artifact_publication_receipts"
        ),
        0,
        "the declared artifact write was never published and nothing refused the terminal"
    );
    assert!(
        owners
            .artifact
            .resolve_head(owners.write_artifact, 2_000)
            .expect("owner head read")
            .is_none(),
        "the artifact owner head must still be empty"
    );
    assert_integrity(&root.layout.task_path());
    let replay = replay_against_empty_owners(&root.layout, permit_id, plan_id);
    assert_eq!(replay, committed);
}

/// Artifact 面窗口 ii：crash after plan READY（owner 发布 + Task 记账
/// 完成）、before Task finalize——终结照常提交，artifact plan 保持 READY
/// 且 publication 行持久（未链接任何 Task receipt——诚实边界）；重放只信
/// Task 行。
#[test]
#[allow(clippy::too_many_lines)]
fn artifact_face_crash_after_ready_terminal_leaves_plan_unlinked() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let root = TestRoot::new("artifact-ready");
    let mut child = spawn_child("artifact-ready", &root.layout);
    let marker = await_marker(&mut child);
    kill_and_reap(&mut child);
    let (permit_id, plan_id) = decode_marker(&marker);

    let owners = owners(&root.layout);
    let drive = Drive::after_kill(&root.layout, &owners, permit_id);
    assert_eq!(
        raw_count(
            &root.layout.task_path(),
            "SELECT COUNT(*) FROM task_artifact_publication_receipts"
        ),
        1,
        "the artifact publications are durable owner-side evidence"
    );
    let committed = committed_combined(
        six_domain_finalize(&drive, &owners, plan_id, 1_700)
            .expect("post-restart combined finalize"),
    );
    assert_full_terminal_rows(&root.layout.task_path());
    let artifact_plans = drive
        .authority
        .list_incomplete_artifact_commit_plans(10)
        .expect("incomplete artifact plans");
    assert_eq!(artifact_plans.len(), 1);
    assert_eq!(
        artifact_plans[0].state,
        nlos_task::ArtifactCommitPlanState::Ready,
        "the artifact plan stays READY, unlinked from the terminal receipt"
    );
    assert!(artifact_plans[0].task_receipt_id.is_none());
    assert_integrity(&root.layout.task_path());
    let replay = replay_against_empty_owners(&root.layout, permit_id, plan_id);
    assert_eq!(replay, committed);
}

/// Channel 面：crash after seal+permit——endpoint proof（durable 注册行）即
/// Channel 面的全部 owner evidence（无发布协议）；重启后驱动其余五域，
/// 同一请求收敛唯一终态，重放只信 Task 行（Channel 不在终结参数内）。
#[test]
#[allow(clippy::too_many_lines)]
fn channel_face_crash_after_binding_converges_without_channel_gate() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let root = TestRoot::new("channel-bound");
    let mut child = spawn_child("channel-bound", &root.layout);
    let marker = await_marker(&mut child);
    kill_and_reap(&mut child);
    let (permit_id, marker_plan) = decode_marker(&marker);
    assert_eq!(*marker_plan.as_bytes(), [0_u8; 16]);

    let owners = owners(&root.layout);
    let proof = owners
        .channel
        .inspect_endpoint_proof(owners.channel_record.channel_id)
        .expect("channel proof survived the kill");
    assert_eq!(
        proof.participant_generation,
        owners.channel_record.generation
    );
    assert_integrity(&root.layout.task_path());

    complete_converge_and_replay(&root, &owners, permit_id, marker_plan);
}

/// 终结提交点（W3 双向 + WE2 语义的六域版）：
/// - Phase A（断电不可见）：`PowerLossAfter { 0 }` 下 Combined finalize
///   "报告成功"但写入从未落盘；重开后一切不可见（permit Issued、head 0、
///   终结四表零行）——不是部分可见；同一请求重做 → `Committed`，与幻影
///   receipt 逐字节相等（确定性 receipt id）。
/// - Phase B（提交后 kill-9 可见）：子进程完整提交六域终结后被强杀；重开
///   后 committed 状态完全可见；对空 owner 重放 → `Replayed` 逐字节相等、
///   无重复行。
#[test]
#[allow(clippy::too_many_lines)]
fn terminal_commit_point_power_loss_and_kill9_converge_both_ways() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    terminal_power_loss_invisible_redo_byte_equal();
    terminal_kill9_after_commit_visible_replay_byte_equal();
}

fn terminal_power_loss_invisible_redo_byte_equal() {
    let root = TestRoot::new("terminal-power-loss");
    let owners = owners(&root.layout);
    let drive = setup_task_and_permit(&root.layout, &owners, open_task_shim);
    close_both_effect_slots(&drive);
    let plan_id = drive_semantic(&drive, &owners, true);
    let _ = drive_artifact(&drive, &owners);
    settle_reservations(&drive, &owners);
    drive_operation(&owners, true);

    nlos_store_fault::arm(FaultMode::PowerLossAfter { remaining: 0 });
    let phantom = committed_combined(
        six_domain_finalize(&drive, &owners, plan_id, 1_700)
            .expect("power loss drops writes silently"),
    );
    nlos_store_fault::disarm();
    let permit_id = drive.permit.permit_id;
    drop(drive);

    let recovered = Drive::after_kill(&root.layout, &owners, permit_id);
    assert_eq!(
        recovered
            .authority
            .inspect_permit(task_id(), recovered.permit.permit_id)
            .expect("permit")
            .state,
        PermitState::Issued,
        "the silently lost terminal must leave the permit issued"
    );
    assert_eq!(
        raw_count(
            &root.layout.task_path(),
            "SELECT COUNT(*) FROM task_receipts"
        ),
        0
    );
    assert_eq!(
        raw_count(
            &root.layout.task_path(),
            "SELECT COUNT(*) FROM task_resource_cost_receipts"
        ),
        0
    );
    assert_eq!(
        raw_count(
            &root.layout.task_path(),
            "SELECT COUNT(*) FROM task_resource_cost_consumptions"
        ),
        0
    );
    assert_eq!(
        raw_count(
            &root.layout.task_path(),
            "SELECT COUNT(*) FROM task_semantic_publication_receipts"
        ),
        1,
        "the semantic publication row is pre-terminal durable state (plan READY evidence)"
    );
    assert_integrity(&root.layout.task_path());

    let redone = committed_combined(
        six_domain_finalize(&recovered, &owners, plan_id, 1_700)
            .expect("redo combined finalize after power loss"),
    );
    assert_eq!(
        redone, phantom,
        "redo must be byte-equal to the silently lost decision"
    );
    assert_full_terminal_rows(&root.layout.task_path());
    let replay = replay_against_empty_owners(&root.layout, recovered.permit.permit_id, plan_id);
    assert_eq!(replay, redone);
}

fn terminal_kill9_after_commit_visible_replay_byte_equal() {
    let root = TestRoot::new("terminal-kill9");
    let mut child = spawn_child("terminal-committed", &root.layout);
    let marker = await_marker(&mut child);
    kill_and_reap(&mut child);
    let (permit_id, plan_id) = decode_marker(&marker);

    let owners = owners(&root.layout);
    let drive = Drive::after_kill(&root.layout, &owners, permit_id);
    assert_eq!(
        drive
            .authority
            .inspect_permit(task_id(), permit_id)
            .expect("permit")
            .state,
        PermitState::Closed,
        "committed six-domain terminal must survive the kill"
    );
    assert_eq!(
        drive
            .authority
            .inspect_task(task_id())
            .expect("task")
            .head_commit_seq,
        1
    );
    assert_full_terminal_rows(&root.layout.task_path());
    assert_integrity(&root.layout.task_path());

    let replay_one = replay_against_empty_owners(&root.layout, permit_id, plan_id);
    assert_eq!(replay_one.resource_cost_receipts.len(), 2);
    assert_eq!(replay_one.semantic_publications.len(), 1);
    let replay_two = replay_against_empty_owners(&root.layout, permit_id, plan_id);
    assert_eq!(replay_two, replay_one, "replay must be byte-stable");
    assert_full_terminal_rows(&root.layout.task_path());
}
