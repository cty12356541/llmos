#![allow(deprecated)] // Ladder constructors deprecated in favor of the *_with_authorities_struct entries.
//! TASK-RESOURCE-BRIDGE-FAULT-01: kill-window / fault-injection matrix for
//! the Task↔Resource bridge finalize rungs —
//! `finalize_commit_v3_with_resource_authority[_and_authority_lease]`
//! (resource-only rung, landed 7431372) and
//! `finalize_commit_v3_with_semantic_publications_and_resource_authority[_and_authority_lease]`
//! (mixed rung, landed 52a7d32).
//!
//! Harness and fixtures follow the established matrices exactly:
//! `tests/fault_injection.rs` + `tests/reconcile_fault_injection.rs`
//! (Task-side fault-VFS plumbing, kill-9 children synchronized through
//! piped `READY` markers — never sleeps, `FAULT_LOCK` process-wide
//! serialization, `wal_commit_frames` tail truncation, typed error-chain
//! assertions, raw table counts, `PRAGMA integrity_check` per scenario)
//! and `nlos-resource/tests/finalize_fault_injection.rs` (F1–F7 matrix
//! row shape, Phase A/B power-loss structure). Bridge fixtures reuse the
//! owner/seal/permit setup of `tests/resource_commit.rs` and
//! `tests/mixed_semantic_resource_commit.rs`.
//!
//! **Fault targeting**: the Resource and Semantic owners are FINALIZED /
//! READY and immutable before the bridge runs, so every injected fault
//! targets the Task authority connection, which is opened through the
//! `nlos-store-fault` VFS (`open_with_vfs`); the owner connections use
//! the plain VFS and are unaffected. The bridge is **verify-then-commit,
//! not cross-authority atomicity**: this matrix proves idempotent
//! convergence of the Task terminal transaction from the durable prefix,
//! never cross-authority atomicity.
//!
//! Matrix (scenario × rung):
//! - W1 pre-commit IOERR — typed durability error, prefix intact,
//!   unfaulted retry converges to the full aggregate;
//! - W2 pre-commit ENOSPC — same convergence;
//! - W3 `PowerLossAfter` commit-point — invisible (Phase A) or fully
//!   visible (Phase B, `kill-9` after commit), never partial; redo is
//!   byte-equal `Committed`, replay is byte-equal `Replayed`;
//! - W4 torn WAL tail — last commit frame truncated: the bridge commit
//!   is discarded whole and redo converges;
//! - W5 kill-window replay idempotence — identical finalize replayed
//!   N≥2 times plus reopen: every call `Replayed`, one receipt row set;
//! - W6 mixed double-sided joint visibility — both nested evidence sets
//!   appear and disappear together with the receipt (single Immediate
//!   transaction), asserted both ways.
//!
//! **Crash semantics disclaimer** (as in every prior matrix): kill-9
//! simulates *process* crashes; the OS page cache survives process
//! death, so a killed process is NOT a machine power loss. Writes the
//! kernel accepted but the disk never saw are covered by
//! [`FaultMode::PowerLossAfter`] and by file-level WAL tail truncation.
//!
//! `allow: SIZE_OK` — one fault matrix per binary is the established
//! repo shape (all seven prior `*_fault_injection.rs` files are
//! monolithic); fixtures are duplicated per matrix file by convention.

use std::error::Error as _;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use nlos_capability::CapabilityTarget;
use nlos_resource::{
    AccountRecord, CreateAccountRequest, CreateQuoteRequest, DriverRecord, QuoteRecord,
    RegisterDriverRequest, ReservationRecord, ReserveRequest, ResourceAuthority,
};
use nlos_semantic::{PublishSemanticPublicationRequest, SemanticAuthority};
use nlos_store_fault::{FaultCode, FaultMode};
use nlos_task::{
    AttemptSpec, AuthorityLeasePermitRequest, AuthorityLeaseRecord, AuthorityLeaseRequest,
    FinalizeRequest, FinalizeRequestV3, NestedResourceCostReceipt,
    NestedSemanticPublicationReceipt, ParticipantRegistryBinding, PermitDecision, PermitRecord,
    PermitRequest, PermitState, PlanSemanticCommitRequest, RecordSemanticPublicationsRequest,
    ResourceFinalizeDecision, SemanticCommitPlanId, SemanticCommitPlanState,
    SemanticResourceFinalizeDecision, SnapshotBundle, SnapshotConsistency, SqliteTaskAuthority,
    TaskSnapshotReceiptSpec, TaskSpec, TaskStoreError, TaskWriteSetRequest,
    TaskWriteSetResourceReservationRequest, TaskWriteSetSemanticAppendRequest,
    TaskWriteSetSemanticRequiredDurability, TaskWriteSetSemanticTarget, empty_effect_history_root,
};
use nlos_types::{
    CallId, CancellationScopeId, CommitPermitId, Generation, IdempotencyKey, NamespaceId,
    OperationId, ProcessId, ReceiptId, SemanticEventId, TaskAttemptId, TaskId, TaskSnapshotId,
};
use rusqlite::Connection;

const VFS_NAME: &str = "nlos-task-resource-bridge-fault";

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

    fn process_root(&self) -> PathBuf {
        self.0.join("process")
    }

    fn empty_replay_resource_root(&self) -> PathBuf {
        self.0.join("replay-empty-resource")
    }

    fn empty_replay_semantic_root(&self) -> PathBuf {
        self.0.join("replay-empty-semantic")
    }
}

/// RAII test root: one fresh directory tree per scenario, removed on drop
/// (task db + wal/shm and every authority root inside).
struct TestRoot {
    layout: Layout,
}

impl TestRoot {
    fn new(label: &str) -> Self {
        let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "nlos-task-resource-bridge-fault-{label}-{}-{suffix}",
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

// ---------------------------------------------------------------------------
// shared plumbing (fault_injection.rs / reconcile_fault_injection.rs 范式)
// ---------------------------------------------------------------------------

/// Full `Display` chain of a `TaskStoreError`, top cause last, for
/// content assertions (e.g. that `SQLITE_FULL`'s message reaches the
/// caller).
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

/// Asserts a typed storage failure whose cause chain names the injected
/// condition (`"i/o"` / `"ioerr"` / `"full"`): never a fake success,
/// never a panic.
fn assert_sqlite_error_chain(error: &TaskStoreError, needles: &[&str]) {
    assert!(
        matches!(error, TaskStoreError::Sqlite(_)),
        "expected a storage error, got {error}"
    );
    let chain = error_chain(error).to_lowercase();
    assert!(
        needles.iter().any(|needle| chain.contains(needle)),
        "error chain must name the injected condition, got: {chain}"
    );
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

/// The durable bridge prefix intact: the permit is still the issued,
/// active permit and the Task head never advanced.
fn assert_permit_issued_at_prefix(authority: &SqliteTaskAuthority, permit_id: CommitPermitId) {
    assert_eq!(
        authority
            .inspect_permit(task_id(), permit_id)
            .expect("permit")
            .state,
        PermitState::Issued,
        "failed finalize must leave the permit issued"
    );
    let head = authority.inspect_task(task_id()).expect("head");
    assert_eq!(head.head_commit_seq, 0, "TaskHead must not advance");
    assert_eq!(head.active_permit, Some(permit_id));
}

/// Zero terminal rows across every table the bridge terminal transaction
/// writes: no receipt, no finalize proof, no nested resource rows.
fn assert_no_terminal_bridge_rows(path: &Path) {
    for table in [
        "task_receipts",
        "task_finalize_proofs",
        "task_resource_cost_receipts",
        "task_resource_cost_consumptions",
    ] {
        assert_eq!(
            raw_count(path, &format!("SELECT COUNT(*) FROM {table}")),
            0,
            "phantom rows must not survive in {table}"
        );
    }
}

/// The exact two-reservation full aggregate of the shared bridge fixture:
/// R1 uses 30 then 37 of 100 (refund 63), R2 uses 10 of 25 (refund 15).
fn assert_full_two_reservation_aggregate(nested: &[NestedResourceCostReceipt]) {
    assert_eq!(nested.len(), 2);
    let heavy = nested
        .iter()
        .find(|record| record.finalization.final_usage == 37)
        .expect("37-usage receipt");
    assert_eq!(heavy.upper_bound, 100);
    assert_eq!(heavy.finalization.refund_credit, 63);
    assert_eq!(heavy.consumptions.len(), 2);
    let light = nested
        .iter()
        .find(|record| record.finalization.final_usage == 10)
        .expect("10-usage receipt");
    assert_eq!(light.upper_bound, 25);
    assert_eq!(light.finalization.refund_credit, 15);
    assert_eq!(light.consumptions.len(), 1);
}

// ---------------------------------------------------------------------------
// Resource owner fixture (resource_commit.rs / mixed test 范式)
// ---------------------------------------------------------------------------

/// One Resource owner fixture: driver, account, quote, reserve, settle.
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

    /// Activates, records the ordered consumptions, and finalizes with
    /// the given final usage: the FINALIZED owner aggregate the bridge
    /// re-reads. Returns the owner finalization receipt.
    fn settle(
        &self,
        reservation: &ReservationRecord,
        consumptions: &[(u64, u64)],
        final_usage: u64,
        seed: u8,
    ) -> nlos_resource::FinalizationReceipt {
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

/// The exact sealed-set derivation the bridge performs, computed from the
/// owner for cross-checking the committed nested set.
fn expected_nested(
    owner: &OwnerFixture,
    reservations: &[ReservationRecord],
) -> Vec<NestedResourceCostReceipt> {
    let mut nested = reservations
        .iter()
        .map(|reservation| {
            NestedResourceCostReceipt::from_owner(
                owner
                    .authority
                    .inspect_cost_receipt(reservation.reservation_id)
                    .expect("owner aggregate"),
            )
        })
        .collect::<Vec<_>>();
    nested.sort_unstable_by_key(|record| record.reservation_id);
    nested
}

// ---------------------------------------------------------------------------
// Semantic owner fixture (mixed_semantic_resource_commit.rs 范式)
// ---------------------------------------------------------------------------

/// Seeds one admitted, durable Semantic event and returns the owner with
/// its proof identities.
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

// ---------------------------------------------------------------------------
// bridge fixture setup (resource_commit.rs / mixed test 范式)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum PermitPath {
    Plain,
    AuthorityLease,
}

/// Registers the task, snapshot receipt, attempt, endpoint participants,
/// seals the reservation-bearing write set, and issues the permit. The
/// Task authority is the caller's (fault-VFS in-process, plain in the
/// crash child).
#[allow(clippy::too_many_lines)]
fn setup_resource_bridge(
    authority: &SqliteTaskAuthority,
    layout: &Layout,
    owner: &OwnerFixture,
    reservations: &[ReservationRecord],
    path: PermitPath,
) -> (PermitRecord, Option<AuthorityLeaseRecord>) {
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
    let driver_registration = authority
        .register_driver_gateway_participant(
            &owner.authority,
            task_id(),
            first_binding,
            owner.driver.driver_id,
            owner.driver.generation,
            1_150,
        )
        .expect("driver participant");
    let second_binding = ParticipantRegistryBinding {
        generation: driver_registration.registry().generation,
        root: driver_registration.registry().root,
    };
    authority
        .register_resource_ledger_participant(
            &owner.authority,
            task_id(),
            second_binding,
            owner.account.account_id,
            Generation::INITIAL,
            1_160,
        )
        .expect("ledger participant");
    let artifact = nlos_artifact::ArtifactStore::open(layout.artifact_root()).expect("artifact");
    let write_set = authority
        .seal_task_write_set_with_resource_authority(
            &artifact,
            &owner.authority,
            TaskWriteSetRequest {
                task_id: task_id(),
                attempt_id: spec.attempt_id,
                attempt_generation: spec.attempt_generation,
                artifact_reads: Vec::new(),
                artifact_writes: Vec::new(),
                process_binding: None,
                semantic_reads: Vec::new(),
                semantic_appends: Vec::new(),
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
        .expect("seal write set")
        .record()
        .clone();
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
    (permit, lease)
}

/// Everything the mixed-rung tests need after the permit exists.
struct MixedPermit {
    permit: PermitRecord,
    write_set_root: [u8; 32],
    lease: Option<AuthorityLeaseRecord>,
}

/// Registers the task plus the three owner endpoint participants
/// (Semantic admission, Driver gateway, Resource ledger), seals the
/// combined Semantic + Resource write set, and issues the permit.
#[allow(clippy::too_many_lines)]
fn setup_mixed_bridge(
    authority: &SqliteTaskAuthority,
    layout: &Layout,
    semantic: &SemanticAuthority,
    seed: &SemanticSeed,
    owner: &OwnerFixture,
    reservations: &[ReservationRecord],
    path: PermitPath,
) -> MixedPermit {
    let (_, event_id, admission_receipt_id, durability_receipt_id, target) = seed;
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
    let artifact = nlos_artifact::ArtifactStore::open(layout.artifact_root()).expect("artifact");
    let process = nlos_process::ProcessAuthority::open(layout.process_root()).expect("process");
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
        permit,
        write_set_root: write_set.write_set_root,
        lease,
    }
}

/// Drives the Semantic publication plan Planned → Publishing → READY and
/// returns the plan id with the exact owner publication copy.
fn drive_semantic_to_ready(
    authority: &SqliteTaskAuthority,
    mixed: &MixedPermit,
    semantic: &SemanticAuthority,
    seed: &SemanticSeed,
) -> (SemanticCommitPlanId, NestedSemanticPublicationReceipt) {
    let (_, event_id, admission_receipt_id, durability_receipt_id, target) = seed;
    let plan = authority
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
        authority
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
    let progress = authority
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

/// The two sealed reservations of the shared bridge fixture: R1 uses 30
/// then 37 of 100, R2 uses 10 of 25.
fn bridge_reservations(owner: &OwnerFixture) -> Vec<ReservationRecord> {
    let quote_one = owner.quote(0xa2, 100);
    let quote_two = owner.quote(0xa3, 25);
    vec![
        owner.reserve(
            &quote_one,
            CallId::from_bytes([0xa5; 16]),
            OperationId::from_bytes([0xa6; 16]),
            IdempotencyKey::from_bytes([0xa7; 16]),
        ),
        owner.reserve(
            &quote_two,
            CallId::from_bytes([0xa9; 16]),
            OperationId::from_bytes([0xaa; 16]),
            IdempotencyKey::from_bytes([0xab; 16]),
        ),
    ]
}

fn settle_bridge_reservations(owner: &OwnerFixture, reservations: &[ReservationRecord]) {
    owner.settle(&reservations[0], &[(1, 30), (2, 37)], 37, 0xac);
    owner.settle(&reservations[1], &[(1, 10)], 10, 0xad);
}

fn committed_resource(decision: ResourceFinalizeDecision) -> nlos_task::ResourceTaskCommitReceipt {
    match decision {
        ResourceFinalizeDecision::Committed(receipt) => *receipt,
        other @ ResourceFinalizeDecision::Replayed(_) => {
            panic!("expected Committed, got {other:?}")
        }
    }
}

fn replayed_resource(decision: ResourceFinalizeDecision) -> nlos_task::ResourceTaskCommitReceipt {
    match decision {
        ResourceFinalizeDecision::Replayed(receipt) => *receipt,
        other @ ResourceFinalizeDecision::Committed(_) => {
            panic!("expected Replayed, got {other:?}")
        }
    }
}

fn committed_combined(
    decision: SemanticResourceFinalizeDecision,
) -> nlos_task::SemanticResourceTaskCommitReceipt {
    match decision {
        SemanticResourceFinalizeDecision::Committed(receipt) => *receipt,
        other @ SemanticResourceFinalizeDecision::Replayed(_) => {
            panic!("expected Committed, got {other:?}")
        }
    }
}

fn replayed_combined(
    decision: SemanticResourceFinalizeDecision,
) -> nlos_task::SemanticResourceTaskCommitReceipt {
    match decision {
        SemanticResourceFinalizeDecision::Replayed(receipt) => *receipt,
        other @ SemanticResourceFinalizeDecision::Committed(_) => {
            panic!("expected Replayed, got {other:?}")
        }
    }
}

// ---------------------------------------------------------------------------
// kill-9 child-process harness (fault_injection.rs 范式)
// ---------------------------------------------------------------------------

fn spawn_child(scenario: &str, layout: &Layout) -> Child {
    Command::new(std::env::current_exe().expect("current test executable"))
        .args(["--exact", "crash_child_helper", "--nocapture"])
        .env("NLOS_TASK_BRIDGE_CRASH_CHILD_SCENARIO", scenario)
        .env(
            "NLOS_TASK_BRIDGE_CRASH_CHILD_ROOT",
            layout.base().as_os_str(),
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn crash child")
}

/// Blocks until the child prints its `READY` marker (pipe
/// synchronization, no sleeps); kills and reaps the child on timeout or
/// early exit.
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

fn hex_decode_permit(text: &str) -> CommitPermitId {
    CommitPermitId::from_bytes(hex_decode16(text))
}

fn hex_decode_plan(text: &str) -> SemanticCommitPlanId {
    SemanticCommitPlanId::from_bytes(hex_decode16(text))
}

fn sibling_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

/// Truncates the WAL in the middle of the LAST commit frame group (the
/// bridge terminal transaction in the child fixture), so recovery must
/// discard that transaction whole while every earlier commit survives.
fn truncate_wal_inside_last_commit(db: &Path) {
    let wal_path = sibling_path(db, "-wal");
    let mut wal = fs::read(&wal_path).expect("read wal");
    assert!(wal.len() >= 32, "WAL must have a header");
    let page_size = match u32::from_be_bytes(wal[8..12].try_into().expect("page size field")) {
        1 => 65_536,
        value => value as usize,
    };
    assert!(page_size >= 512, "valid SQLite page size");
    let frame_size = 24 + page_size;
    let frame_count = (wal.len() - 32) / frame_size;
    assert!(frame_count > 0, "fixture must contain frames");
    let commits: Vec<usize> = (0..frame_count)
        .filter(|index| {
            let start = 32 + index * frame_size;
            u32::from_be_bytes(wal[start + 8..start + 12].try_into().expect("commit field")) != 0
        })
        .collect();
    assert!(
        commits.len() >= 2,
        "fixture must contain several committed transactions"
    );
    let last_commit = *commits.last().expect("commits exist");
    let half_frame_cut = 32 + last_commit * frame_size + frame_size / 2;
    wal.truncate(half_frame_cut);
    fs::write(&wal_path, &wal).expect("write truncated wal");
    fs::remove_file(sibling_path(db, "-shm")).expect("remove stale shm");
}

/// Child entry point. Runs only when spawned by a parent test with the
/// scenario environment set; a no-op in the normal test run.
#[test]
fn crash_child_helper() {
    let (Ok(scenario), Ok(root)) = (
        std::env::var("NLOS_TASK_BRIDGE_CRASH_CHILD_SCENARIO"),
        std::env::var("NLOS_TASK_BRIDGE_CRASH_CHILD_ROOT"),
    ) else {
        return;
    };
    let layout = Layout::new(PathBuf::from(root));
    match scenario.as_str() {
        "resource-bridge-commit" => child_resource_bridge_commit(&layout),
        "mixed-bridge-commit" => child_mixed_bridge_commit(&layout),
        other => panic!("unknown crash child scenario {other}"),
    }
}

/// Child fixture: the full resource-only bridge lifecycle with the
/// terminal transaction fully committed; the kill lands AFTER the commit
/// point (visible case) and leaves the WAL on disk (torn-tail fixture).
fn child_resource_bridge_commit(layout: &Layout) -> ! {
    let owner = OwnerFixture::new(&layout.resource_root(), 0xa1);
    let reservations = bridge_reservations(&owner);
    let task = SqliteTaskAuthority::open(layout.task_path()).expect("open task authority");
    let (permit, _) =
        setup_resource_bridge(&task, layout, &owner, &reservations, PermitPath::Plain);
    settle_bridge_reservations(&owner, &reservations);
    let decision = task
        .finalize_commit_v3_with_resource_authority(
            &owner.authority,
            finalize_request(permit.permit_id, 1_700),
        )
        .expect("bridge commit");
    assert!(matches!(decision, ResourceFinalizeDecision::Committed(_)));
    announce(&format!(
        "READY {}",
        hex_encode(permit.permit_id.as_bytes())
    ));
    let _keepers = (task, owner);
    loop {
        std::thread::park();
    }
}

/// Child fixture: the full combined Semantic + Resource bridge lifecycle
/// with the terminal transaction fully committed; the kill lands AFTER
/// the commit point (visible case) and leaves the WAL on disk. The
/// marker carries both the permit id and the semantic plan id.
fn child_mixed_bridge_commit(layout: &Layout) -> ! {
    let seed = seed_semantic_authority(&layout.semantic_root());
    let (semantic, _, _, _, _) = &seed;
    let owner = OwnerFixture::new(&layout.resource_root(), 0xa1);
    let reservations = bridge_reservations(&owner);
    let task = SqliteTaskAuthority::open(layout.task_path()).expect("open task authority");
    let mixed = setup_mixed_bridge(
        &task,
        layout,
        semantic,
        &seed,
        &owner,
        &reservations,
        PermitPath::Plain,
    );
    let (plan_id, _) = drive_semantic_to_ready(&task, &mixed, semantic, &seed);
    settle_bridge_reservations(&owner, &reservations);
    let decision = task
        .finalize_commit_v3_with_semantic_publications_and_resource_authority(
            semantic,
            &owner.authority,
            plan_id,
            finalize_request(mixed.permit.permit_id, 1_700),
        )
        .expect("combined bridge commit");
    assert!(matches!(
        decision,
        SemanticResourceFinalizeDecision::Committed(_)
    ));
    announce(&format!(
        "READY {} {}",
        hex_encode(mixed.permit.permit_id.as_bytes()),
        hex_encode(plan_id.as_bytes())
    ));
    let _keepers = (task, owner);
    loop {
        std::thread::park();
    }
}

/// Decodes the `READY <permit-hex> <plan-hex>` marker of the mixed
/// crash-child scenarios.
fn decode_mixed_marker(marker: &str) -> (CommitPermitId, SemanticCommitPlanId) {
    let mut parts = marker
        .trim()
        .strip_prefix("READY ")
        .expect("marker")
        .split(' ');
    let permit = hex_decode_permit(parts.next().expect("permit id"));
    let plan = hex_decode_plan(parts.next().expect("plan id"));
    assert!(parts.next().is_none(), "marker carries exactly two ids");
    (permit, plan)
}

// ---------------------------------------------------------------------------
// W1/W2 × resource-only rung: pre-commit IOERR / ENOSPC fail typed and
// converge
// ---------------------------------------------------------------------------

/// W1（resource-only）：`FailWritesAfter { 0, IoErr }` 注入 Task 终结事务
/// 写入 → 桥接 finalize 以 `TaskStoreError::Sqlite` 显式失败（错误链含
/// I/O 条件），permit 保持 Issued、TaskHead 不变、四张终结表零行；disarm
/// 后同一请求重试 → `Committed` 且嵌套集合与 owner 聚合逐字段一致、
/// permit 关闭、head 推进 1；重开后嵌套行可读、integrity ok。
#[test]
fn bridge_fault_resource_rung_ioerr_precommit_fails_typed_and_converges() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let root = TestRoot::new("res-ioerr");
    let owner = OwnerFixture::new(&root.layout.resource_root(), 0xa1);
    let reservations = bridge_reservations(&owner);
    let authority = open_task_shim(&root.layout);
    let (permit, _) = setup_resource_bridge(
        &authority,
        &root.layout,
        &owner,
        &reservations,
        PermitPath::Plain,
    );
    settle_bridge_reservations(&owner, &reservations);
    let request = finalize_request(permit.permit_id, 1_700);

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::IoErr,
    });
    let error = authority
        .finalize_commit_v3_with_resource_authority(&owner.authority, request.clone())
        .expect_err("bridge finalize must fail under injected I/O error");
    assert_sqlite_error_chain(&error, &["i/o", "ioerr"]);
    assert!(nlos_store_fault::writes_observed() > 0);
    assert_permit_issued_at_prefix(&authority, permit.permit_id);
    assert_no_terminal_bridge_rows(&root.layout.task_path());
    drop(authority);
    let reopened = reopen_task(&root.layout);
    assert_permit_issued_at_prefix(&reopened, permit.permit_id);
    assert_no_terminal_bridge_rows(&root.layout.task_path());
    assert_integrity(&root.layout.task_path());

    nlos_store_fault::disarm();
    let receipt = committed_resource(
        reopened
            .finalize_commit_v3_with_resource_authority(&owner.authority, request)
            .expect("bridge finalize succeeds after disarm"),
    );
    assert_eq!(receipt.task_receipt.new_head_commit_seq, 1);
    assert_full_two_reservation_aggregate(&receipt.resource_cost_receipts);
    assert_eq!(
        receipt.resource_cost_receipts,
        expected_nested(&owner, &reservations)
    );
    assert_eq!(
        reopened
            .inspect_permit(task_id(), permit.permit_id)
            .expect("permit")
            .state,
        PermitState::Closed
    );
    drop(reopened);
    let verified = reopen_task(&root.layout);
    assert_eq!(
        verified
            .inspect_resource_cost_receipts(task_id(), receipt.task_receipt.receipt_id)
            .expect("nested readback"),
        receipt.resource_cost_receipts
    );
    assert_integrity(&root.layout.task_path());
}

/// W2（resource-only）：`FailWritesAfter { 0, Full }` 下同一收敛——
/// `SQLITE_FULL` 显式失败、前缀不变、零幻影行；disarm 后重试成功且
/// 聚合完整。
#[test]
fn bridge_fault_resource_rung_enospc_precommit_fails_typed_and_converges() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let root = TestRoot::new("res-full");
    let owner = OwnerFixture::new(&root.layout.resource_root(), 0xa1);
    let reservations = bridge_reservations(&owner);
    let authority = open_task_shim(&root.layout);
    let (permit, _) = setup_resource_bridge(
        &authority,
        &root.layout,
        &owner,
        &reservations,
        PermitPath::Plain,
    );
    settle_bridge_reservations(&owner, &reservations);
    let request = finalize_request(permit.permit_id, 1_700);

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::Full,
    });
    let error = authority
        .finalize_commit_v3_with_resource_authority(&owner.authority, request.clone())
        .expect_err("bridge finalize must fail under injected disk-full");
    assert_sqlite_error_chain(&error, &["full"]);
    assert!(nlos_store_fault::writes_observed() > 0);
    assert_permit_issued_at_prefix(&authority, permit.permit_id);
    assert_no_terminal_bridge_rows(&root.layout.task_path());

    nlos_store_fault::disarm();
    let receipt = committed_resource(
        authority
            .finalize_commit_v3_with_resource_authority(&owner.authority, request)
            .expect("bridge finalize succeeds after disarm"),
    );
    assert_eq!(receipt.task_receipt.new_head_commit_seq, 1);
    assert_full_two_reservation_aggregate(&receipt.resource_cost_receipts);
    drop(authority);
    let reopened = reopen_task(&root.layout);
    assert_eq!(
        reopened
            .inspect_resource_cost_receipts(task_id(), receipt.task_receipt.receipt_id)
            .expect("nested readback"),
        receipt.resource_cost_receipts
    );
    assert_integrity(&root.layout.task_path());
}

// ---------------------------------------------------------------------------
// W3 × resource-only rung: PowerLossAfter the commit point
// ---------------------------------------------------------------------------

/// W3（resource-only）：
/// - Phase A（断电不可见）：`PowerLossAfter { 0 }` 下桥接 finalize "报告
///   成功"但写入从未落盘；重开后一切不可见（permit Issued、head 0、四表
///   零行）——不是部分可见；同一请求重做 → `Committed`，与幻影 receipt
///   逐字节相等（确定性 receipt id），重开后真实持久。
/// - Phase B（提交后 kill-9 可见）：子进程完整提交桥接事务后被强杀；
///   重开后 committed 状态完全可见（receipt + 全部嵌套行 + permit
///   Closed + head 1）——同样不是部分可见；同请求重放 → `Replayed`
///   逐字节相等、无重复行。
#[test]
#[allow(clippy::too_many_lines)]
fn bridge_fault_resource_rung_power_loss_commit_point_converges_both_ways() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    resource_power_loss_invisible_redo_byte_equal();
    resource_kill9_after_commit_visible_replay_byte_equal();
}

fn resource_power_loss_invisible_redo_byte_equal() {
    let root = TestRoot::new("res-power-loss");
    let owner = OwnerFixture::new(&root.layout.resource_root(), 0xa1);
    let reservations = bridge_reservations(&owner);
    let authority = open_task_shim(&root.layout);
    let (permit, _) = setup_resource_bridge(
        &authority,
        &root.layout,
        &owner,
        &reservations,
        PermitPath::Plain,
    );
    settle_bridge_reservations(&owner, &reservations);
    let request = finalize_request(permit.permit_id, 1_700);

    nlos_store_fault::arm(FaultMode::PowerLossAfter { remaining: 0 });
    let phantom = committed_resource(
        authority
            .finalize_commit_v3_with_resource_authority(&owner.authority, request.clone())
            .expect("power loss drops writes silently"),
    );
    nlos_store_fault::disarm();
    // The surviving connection keeps a wal-index referencing frames the
    // disk never saw; it must die first (as a real power loss would kill
    // it) so recovery sees durable bytes alone (fault_injection.rs
    // precedent).
    drop(authority);

    let recovered = reopen_task(&root.layout);
    assert_permit_issued_at_prefix(&recovered, permit.permit_id);
    assert_no_terminal_bridge_rows(&root.layout.task_path());
    assert_integrity(&root.layout.task_path());

    let redone = committed_resource(
        recovered
            .finalize_commit_v3_with_resource_authority(&owner.authority, request)
            .expect("redo bridge finalize after power loss"),
    );
    assert_eq!(
        redone, phantom,
        "redo must be byte-equal to the silently lost decision"
    );
    assert_full_two_reservation_aggregate(&redone.resource_cost_receipts);
    drop(recovered);
    let verified = reopen_task(&root.layout);
    assert_eq!(
        verified
            .inspect_permit(task_id(), permit.permit_id)
            .expect("permit")
            .state,
        PermitState::Closed
    );
    assert_eq!(
        verified
            .inspect_task(task_id())
            .expect("head")
            .head_commit_seq,
        1
    );
    assert_eq!(
        verified
            .inspect_resource_cost_receipts(task_id(), redone.task_receipt.receipt_id)
            .expect("nested readback"),
        redone.resource_cost_receipts
    );
    assert_integrity(&root.layout.task_path());
    drop(verified);
    drop(root);
}

fn resource_kill9_after_commit_visible_replay_byte_equal() {
    let root = TestRoot::new("res-kill9-commit");
    let mut child = spawn_child("resource-bridge-commit", &root.layout);
    let marker = await_marker(&mut child);
    let permit_id = hex_decode_permit(
        marker
            .trim()
            .strip_prefix("READY ")
            .expect("marker carries the permit id"),
    );
    kill_and_reap(&mut child);

    let recovered = reopen_task(&root.layout);
    assert_eq!(
        recovered
            .inspect_permit(task_id(), permit_id)
            .expect("permit")
            .state,
        PermitState::Closed,
        "committed bridge must survive the kill"
    );
    assert_eq!(
        recovered
            .inspect_task(task_id())
            .expect("head")
            .head_commit_seq,
        1
    );
    assert_eq!(
        raw_count(
            &root.layout.task_path(),
            "SELECT COUNT(*) FROM task_receipts"
        ),
        1
    );
    assert_eq!(
        raw_count(
            &root.layout.task_path(),
            "SELECT COUNT(*) FROM task_resource_cost_receipts"
        ),
        2
    );
    assert_eq!(
        raw_count(
            &root.layout.task_path(),
            "SELECT COUNT(*) FROM task_resource_cost_consumptions"
        ),
        3
    );
    assert_integrity(&root.layout.task_path());

    // Replay consults only the durable Task rows: an empty Resource
    // authority proves the owner is never re-read.
    let replay_owner =
        ResourceAuthority::open(root.layout.empty_replay_resource_root()).expect("empty owner");
    let replay_one = replayed_resource(
        recovered
            .finalize_commit_v3_with_resource_authority(
                &replay_owner,
                finalize_request(permit_id, 9_999),
            )
            .expect("visible replay"),
    );
    assert_full_two_reservation_aggregate(&replay_one.resource_cost_receipts);
    let replay_two = replayed_resource(
        recovered
            .finalize_commit_v3_with_resource_authority(
                &replay_owner,
                finalize_request(permit_id, 9_999),
            )
            .expect("second replay"),
    );
    assert_eq!(replay_two, replay_one, "replay must be byte-stable");
    assert_eq!(
        recovered
            .inspect_resource_cost_receipts(task_id(), replay_one.task_receipt.receipt_id)
            .expect("nested readback"),
        replay_one.resource_cost_receipts
    );
    assert_eq!(
        raw_count(
            &root.layout.task_path(),
            "SELECT COUNT(*) FROM task_receipts"
        ),
        1,
        "replays must not append a second receipt"
    );
    assert_integrity(&root.layout.task_path());
    drop(recovered);
    drop(root);
}

// ---------------------------------------------------------------------------
// W4 × resource-only rung: torn WAL tail
// ---------------------------------------------------------------------------

/// W4（resource-only）：子进程提交完整桥接事务后被强杀，父进程把 WAL
/// 截断到最后一个 commit 帧（桥接事务）的一半；重开后桥接事务整体隐藏
/// （permit Issued、head 0、四表零行），此前已提交前缀（task/attempt/
/// write set/permit）保留；同一请求重做 → `Committed`，聚合完整且
/// receipt id 确定性一致；重开后重放逐字节相等、行数恰一套。
#[test]
fn bridge_fault_resource_rung_torn_wal_tail_discards_and_converges() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let root = TestRoot::new("res-torn-tail");
    let mut child = spawn_child("resource-bridge-commit", &root.layout);
    let marker = await_marker(&mut child);
    let permit_id = hex_decode_permit(
        marker
            .trim()
            .strip_prefix("READY ")
            .expect("marker carries the permit id"),
    );
    kill_and_reap(&mut child);

    truncate_wal_inside_last_commit(&root.layout.task_path());

    let recovered = reopen_task(&root.layout);
    assert_permit_issued_at_prefix(&recovered, permit_id);
    assert_no_terminal_bridge_rows(&root.layout.task_path());
    assert_integrity(&root.layout.task_path());

    // Redo needs the real FINALIZED owner aggregate: reopen the owner
    // from the durable files the child settled.
    let owner = OwnerFixture::new(&root.layout.resource_root(), 0xa1);
    let reservations = bridge_reservations(&owner);
    let receipt = committed_resource(
        recovered
            .finalize_commit_v3_with_resource_authority(
                &owner.authority,
                finalize_request(permit_id, 1_700),
            )
            .expect("redo bridge finalize after torn tail"),
    );
    assert_eq!(receipt.task_receipt.new_head_commit_seq, 1);
    assert_full_two_reservation_aggregate(&receipt.resource_cost_receipts);
    assert_eq!(
        receipt.resource_cost_receipts,
        expected_nested(&owner, &reservations),
        "redo must re-derive the exact owner aggregate"
    );
    drop(recovered);
    let verified = reopen_task(&root.layout);
    let replay = replayed_resource(
        verified
            .finalize_commit_v3_with_resource_authority(
                &owner.authority,
                finalize_request(permit_id, 9_999),
            )
            .expect("replay after redo"),
    );
    assert_eq!(replay, receipt, "replay must equal the durable commit");
    assert_eq!(
        raw_count(
            &root.layout.task_path(),
            "SELECT COUNT(*) FROM task_receipts"
        ),
        1
    );
    assert_eq!(
        raw_count(
            &root.layout.task_path(),
            "SELECT COUNT(*) FROM task_resource_cost_receipts"
        ),
        2
    );
    assert_eq!(
        raw_count(
            &root.layout.task_path(),
            "SELECT COUNT(*) FROM task_resource_cost_consumptions"
        ),
        3
    );
    assert_integrity(&root.layout.task_path());
    drop(verified);
    drop(root);
}

// ---------------------------------------------------------------------------
// W5 × resource-only rung: kill-window replay idempotence (lease variant)
// ---------------------------------------------------------------------------

/// W5（resource-only，lease 变体）：桥接 commit 成功后，同一 finalize
/// 请求连续重放 3 次 + 重开后再重放 1 次（kill-window 重试风暴）→ 每次
/// `Replayed` 且与 committed receipt 逐字节相等；receipt/嵌套父行/子行
/// 恰好一套，无重复。
#[test]
fn bridge_fault_resource_rung_replay_storm_after_commit_is_idempotent() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let root = TestRoot::new("res-replay-storm");
    let owner = OwnerFixture::new(&root.layout.resource_root(), 0xb1);
    let quote = owner.quote(0xb2, 60);
    let reservation = owner.reserve(
        &quote,
        CallId::from_bytes([0xb4; 16]),
        OperationId::from_bytes([0xb5; 16]),
        IdempotencyKey::from_bytes([0xb6; 16]),
    );
    let authority = open_task_shim(&root.layout);
    let (permit, lease) = setup_resource_bridge(
        &authority,
        &root.layout,
        &owner,
        &[reservation],
        PermitPath::AuthorityLease,
    );
    let lease = lease.expect("lease path");
    owner.settle(&reservation, &[(1, 12)], 12, 0xb7);
    let request = finalize_request(permit.permit_id, 1_700);

    let committed = committed_resource(
        authority
            .finalize_commit_v3_with_resource_authority_and_authority_lease(
                &owner.authority,
                request.clone(),
                lease,
            )
            .expect("lease bridge commit"),
    );
    assert_eq!(committed.resource_cost_receipts.len(), 1);
    assert_eq!(
        committed.resource_cost_receipts[0]
            .finalization
            .refund_credit,
        48
    );

    for _ in 0..3 {
        let replay = replayed_resource(
            authority
                .finalize_commit_v3_with_resource_authority_and_authority_lease(
                    &owner.authority,
                    request.clone(),
                    lease,
                )
                .expect("storm replay"),
        );
        assert_eq!(replay, committed, "every storm replay is byte-equal");
    }
    drop(authority);
    let reopened = reopen_task(&root.layout);
    let replay = replayed_resource(
        reopened
            .finalize_commit_v3_with_resource_authority_and_authority_lease(
                &owner.authority,
                request,
                lease,
            )
            .expect("replay after reopen"),
    );
    assert_eq!(replay, committed);
    assert_eq!(
        raw_count(
            &root.layout.task_path(),
            "SELECT COUNT(*) FROM task_receipts"
        ),
        1,
        "retry storm must not duplicate the receipt"
    );
    assert_eq!(
        raw_count(
            &root.layout.task_path(),
            "SELECT COUNT(*) FROM task_resource_cost_receipts"
        ),
        1
    );
    assert_eq!(
        raw_count(
            &root.layout.task_path(),
            "SELECT COUNT(*) FROM task_resource_cost_consumptions"
        ),
        1
    );
    assert_integrity(&root.layout.task_path());
}

// ---------------------------------------------------------------------------
// W1/W2 × mixed rung: pre-commit IOERR / ENOSPC fail typed and converge
// ---------------------------------------------------------------------------

/// W1（mixed）：plan READY、owner FINALIZED 后，`FailWritesAfter { 0,
/// IoErr }` 注入 Task 终结事务 → `Sqlite` 显式失败；permit Issued、head
/// 0、receipt/嵌套 resource 行零、plan 仍 Ready（publications 行属 READY
/// 前缀、保留）；disarm 后重试 → `Committed`，两个嵌套集合同时落位、
/// plan FINALIZED；重开后可读。
#[test]
#[allow(clippy::too_many_lines)]
fn bridge_fault_mixed_rung_ioerr_precommit_fails_typed_and_converges() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let root = TestRoot::new("mixed-ioerr");
    let seed = seed_semantic_authority(&root.layout.semantic_root());
    let semantic = SemanticAuthority::open(root.layout.semantic_root()).expect("semantic");
    let owner = OwnerFixture::new(&root.layout.resource_root(), 0xa1);
    let reservations = bridge_reservations(&owner);
    let authority = open_task_shim(&root.layout);
    let mixed = setup_mixed_bridge(
        &authority,
        &root.layout,
        &semantic,
        &seed,
        &owner,
        &reservations,
        PermitPath::Plain,
    );
    let (plan_id, expected_publication) =
        drive_semantic_to_ready(&authority, &mixed, &semantic, &seed);
    settle_bridge_reservations(&owner, &reservations);
    let request = finalize_request(mixed.permit.permit_id, 1_700);

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::IoErr,
    });
    let error = authority
        .finalize_commit_v3_with_semantic_publications_and_resource_authority(
            &semantic,
            &owner.authority,
            plan_id,
            request.clone(),
        )
        .expect_err("mixed bridge finalize must fail under injected I/O error");
    assert_sqlite_error_chain(&error, &["i/o", "ioerr"]);
    assert!(nlos_store_fault::writes_observed() > 0);
    assert_permit_issued_at_prefix(&authority, mixed.permit.permit_id);
    assert_no_terminal_bridge_rows(&root.layout.task_path());
    assert_eq!(
        authority
            .inspect_semantic_commit_progress(plan_id)
            .expect("progress")
            .plan
            .state,
        SemanticCommitPlanState::Ready,
        "failed mixed finalize must leave the plan READY"
    );
    assert_eq!(
        raw_count(
            &root.layout.task_path(),
            "SELECT COUNT(*) FROM task_semantic_publication_receipts"
        ),
        1,
        "the READY prefix publications survive"
    );

    nlos_store_fault::disarm();
    let receipt = committed_combined(
        authority
            .finalize_commit_v3_with_semantic_publications_and_resource_authority(
                &semantic,
                &owner.authority,
                plan_id,
                request,
            )
            .expect("mixed bridge finalize succeeds after disarm"),
    );
    assert_eq!(receipt.task_receipt.new_head_commit_seq, 1);
    assert_eq!(receipt.semantic_publications, vec![expected_publication]);
    assert_full_two_reservation_aggregate(&receipt.resource_cost_receipts);
    assert_eq!(
        receipt.resource_cost_receipts,
        expected_nested(&owner, &reservations)
    );
    let progress = authority
        .inspect_semantic_commit_progress(plan_id)
        .expect("progress");
    assert_eq!(progress.plan.state, SemanticCommitPlanState::Finalized);
    assert_eq!(
        progress.plan.task_receipt_id,
        Some(receipt.task_receipt.receipt_id)
    );
    drop(authority);
    let reopened = reopen_task(&root.layout);
    assert_eq!(
        reopened
            .inspect_resource_cost_receipts(task_id(), receipt.task_receipt.receipt_id)
            .expect("nested readback"),
        receipt.resource_cost_receipts
    );
    assert_integrity(&root.layout.task_path());
}

/// W2（mixed）：`FailWritesAfter { 0, Full }` 下同一收敛——`SQLITE_FULL`
/// 显式失败、plan Ready、零终结行；disarm 后重试成功且双侧集合完整。
#[test]
fn bridge_fault_mixed_rung_enospc_precommit_fails_typed_and_converges() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let root = TestRoot::new("mixed-full");
    let seed = seed_semantic_authority(&root.layout.semantic_root());
    let semantic = SemanticAuthority::open(root.layout.semantic_root()).expect("semantic");
    let owner = OwnerFixture::new(&root.layout.resource_root(), 0xa1);
    let reservations = bridge_reservations(&owner);
    let authority = open_task_shim(&root.layout);
    let mixed = setup_mixed_bridge(
        &authority,
        &root.layout,
        &semantic,
        &seed,
        &owner,
        &reservations,
        PermitPath::Plain,
    );
    let (plan_id, expected_publication) =
        drive_semantic_to_ready(&authority, &mixed, &semantic, &seed);
    settle_bridge_reservations(&owner, &reservations);
    let request = finalize_request(mixed.permit.permit_id, 1_700);

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::Full,
    });
    let error = authority
        .finalize_commit_v3_with_semantic_publications_and_resource_authority(
            &semantic,
            &owner.authority,
            plan_id,
            request.clone(),
        )
        .expect_err("mixed bridge finalize must fail under injected disk-full");
    assert_sqlite_error_chain(&error, &["full"]);
    assert!(nlos_store_fault::writes_observed() > 0);
    assert_permit_issued_at_prefix(&authority, mixed.permit.permit_id);
    assert_no_terminal_bridge_rows(&root.layout.task_path());
    assert_eq!(
        authority
            .inspect_semantic_commit_progress(plan_id)
            .expect("progress")
            .plan
            .state,
        SemanticCommitPlanState::Ready
    );

    nlos_store_fault::disarm();
    let receipt = committed_combined(
        authority
            .finalize_commit_v3_with_semantic_publications_and_resource_authority(
                &semantic,
                &owner.authority,
                plan_id,
                request,
            )
            .expect("mixed bridge finalize succeeds after disarm"),
    );
    assert_eq!(receipt.semantic_publications, vec![expected_publication]);
    assert_full_two_reservation_aggregate(&receipt.resource_cost_receipts);
    drop(authority);
    let reopened = reopen_task(&root.layout);
    assert_eq!(
        reopened
            .inspect_resource_cost_receipts(task_id(), receipt.task_receipt.receipt_id)
            .expect("nested readback"),
        receipt.resource_cost_receipts
    );
    assert_integrity(&root.layout.task_path());
}

// ---------------------------------------------------------------------------
// W3 + W6 × mixed rung: PowerLossAfter the commit point, double-sided
// ---------------------------------------------------------------------------

/// W3+W6（mixed）：
/// - Phase A（断电不可见 + 双侧同灭）：`PowerLossAfter { 0 }` 下 mixed
///   finalize "报告成功"；重开后 permit Issued、head 0、receipt 与嵌套
///   resource 行皆无、plan 仍 Ready——两个嵌套证据集合与 receipt 同生
///   同灭，绝不存在"只有一侧"的中间态；同一请求重做 → `Committed`
///   与幻影逐字节相等，plan FINALIZED、双侧同时落位。
/// - Phase B（提交后 kill-9 可见 + 双侧同现）：子进程完整提交 mixed
///   桥接事务后被强杀；重开后 receipt、全部嵌套 resource 行、plan
///   FINALIZED、permit Closed 同时可见；对空 owner 重放 → `Replayed`
///   逐字节相等。
#[test]
#[allow(clippy::too_many_lines)]
fn bridge_fault_mixed_rung_power_loss_commit_point_converges_both_ways() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    mixed_power_loss_invisible_redo_byte_equal();
    mixed_kill9_after_commit_visible_double_sided();
}

fn mixed_power_loss_invisible_redo_byte_equal() {
    let root = TestRoot::new("mixed-power-loss");
    let seed = seed_semantic_authority(&root.layout.semantic_root());
    let semantic = SemanticAuthority::open(root.layout.semantic_root()).expect("semantic");
    let owner = OwnerFixture::new(&root.layout.resource_root(), 0xa1);
    let reservations = bridge_reservations(&owner);
    let authority = open_task_shim(&root.layout);
    let mixed = setup_mixed_bridge(
        &authority,
        &root.layout,
        &semantic,
        &seed,
        &owner,
        &reservations,
        PermitPath::Plain,
    );
    let (plan_id, expected_publication) =
        drive_semantic_to_ready(&authority, &mixed, &semantic, &seed);
    settle_bridge_reservations(&owner, &reservations);
    let request = finalize_request(mixed.permit.permit_id, 1_700);

    nlos_store_fault::arm(FaultMode::PowerLossAfter { remaining: 0 });
    let phantom = committed_combined(
        authority
            .finalize_commit_v3_with_semantic_publications_and_resource_authority(
                &semantic,
                &owner.authority,
                plan_id,
                request.clone(),
            )
            .expect("power loss drops writes silently"),
    );
    nlos_store_fault::disarm();
    drop(authority);

    let recovered = reopen_task(&root.layout);
    assert_permit_issued_at_prefix(&recovered, mixed.permit.permit_id);
    assert_no_terminal_bridge_rows(&root.layout.task_path());
    assert_eq!(
        recovered
            .inspect_semantic_commit_progress(plan_id)
            .expect("progress")
            .plan
            .state,
        SemanticCommitPlanState::Ready,
        "the FINALIZED flip must vanish together with the receipt"
    );
    assert_eq!(
        raw_count(
            &root.layout.task_path(),
            "SELECT COUNT(*) FROM task_semantic_publication_receipts"
        ),
        1
    );
    assert_integrity(&root.layout.task_path());

    let redone = committed_combined(
        recovered
            .finalize_commit_v3_with_semantic_publications_and_resource_authority(
                &semantic,
                &owner.authority,
                plan_id,
                request,
            )
            .expect("redo mixed bridge finalize after power loss"),
    );
    assert_eq!(
        redone, phantom,
        "redo must be byte-equal to the silently lost decision"
    );
    assert_eq!(redone.semantic_publications, vec![expected_publication]);
    assert_full_two_reservation_aggregate(&redone.resource_cost_receipts);
    let progress = recovered
        .inspect_semantic_commit_progress(plan_id)
        .expect("progress");
    assert_eq!(progress.plan.state, SemanticCommitPlanState::Finalized);
    assert_eq!(
        progress.plan.task_receipt_id,
        Some(redone.task_receipt.receipt_id)
    );
    drop(recovered);
    let verified = reopen_task(&root.layout);
    assert_eq!(
        verified
            .inspect_resource_cost_receipts(task_id(), redone.task_receipt.receipt_id)
            .expect("nested readback"),
        redone.resource_cost_receipts
    );
    assert_integrity(&root.layout.task_path());
    drop(verified);
    drop(root);
}

fn mixed_kill9_after_commit_visible_double_sided() {
    let root = TestRoot::new("mixed-kill9-commit");
    let mut child = spawn_child("mixed-bridge-commit", &root.layout);
    let (permit_id, plan_id) = decode_mixed_marker(&await_marker(&mut child));
    kill_and_reap(&mut child);

    let recovered = reopen_task(&root.layout);
    assert_eq!(
        recovered
            .inspect_permit(task_id(), permit_id)
            .expect("permit")
            .state,
        PermitState::Closed
    );
    assert_eq!(
        recovered
            .inspect_task(task_id())
            .expect("head")
            .head_commit_seq,
        1
    );
    assert_eq!(
        raw_count(
            &root.layout.task_path(),
            "SELECT COUNT(*) FROM task_receipts"
        ),
        1
    );
    assert_eq!(
        raw_count(
            &root.layout.task_path(),
            "SELECT COUNT(*) FROM task_resource_cost_receipts"
        ),
        2,
        "resource rows present with the receipt"
    );
    assert_eq!(
        raw_count(
            &root.layout.task_path(),
            "SELECT COUNT(*) FROM task_resource_cost_consumptions"
        ),
        3
    );
    assert_eq!(
        raw_count(
            &root.layout.task_path(),
            "SELECT COUNT(*) FROM task_semantic_publication_receipts"
        ),
        1
    );
    assert_integrity(&root.layout.task_path());

    // Replay against BOTH empty owners: the combined replay authority is
    // the durable Task rows alone.
    let empty_semantic =
        SemanticAuthority::open(root.layout.empty_replay_semantic_root()).expect("empty semantic");
    let empty_owner =
        ResourceAuthority::open(root.layout.empty_replay_resource_root()).expect("empty owner");
    let replay_one = replayed_combined(
        recovered
            .finalize_commit_v3_with_semantic_publications_and_resource_authority(
                &empty_semantic,
                &empty_owner,
                plan_id,
                finalize_request(permit_id, 9_999),
            )
            .expect("visible replay"),
    );
    assert_eq!(replay_one.semantic_publications.len(), 1);
    assert_full_two_reservation_aggregate(&replay_one.resource_cost_receipts);
    let replay_two = replayed_combined(
        recovered
            .finalize_commit_v3_with_semantic_publications_and_resource_authority(
                &empty_semantic,
                &empty_owner,
                plan_id,
                finalize_request(permit_id, 9_999),
            )
            .expect("second replay"),
    );
    assert_eq!(replay_two, replay_one);
    assert_eq!(
        recovered
            .inspect_resource_cost_receipts(task_id(), replay_one.task_receipt.receipt_id)
            .expect("nested readback"),
        replay_one.resource_cost_receipts
    );
    assert_eq!(
        raw_count(
            &root.layout.task_path(),
            "SELECT COUNT(*) FROM task_receipts"
        ),
        1,
        "replays must not append a second receipt"
    );
    assert_integrity(&root.layout.task_path());
    drop(recovered);
    drop(root);
}

// ---------------------------------------------------------------------------
// W4 × mixed rung: torn WAL tail discards both sides together
// ---------------------------------------------------------------------------

/// W4+W6（mixed）：子进程完整提交 mixed 桥接事务后被强杀，父进程把 WAL
/// 截断到最后一个 commit 帧的一半；重开后桥接事务整体隐藏——receipt、
/// 嵌套 resource 行、plan FINALIZED 翻转一起消失（绝无"只剩一侧"），
/// permit Issued、head 0，READY 前缀（含 publications 行）保留；用磁盘上
/// 的真实 owner 重做 → `Committed`，双侧集合 + FINALIZED 同时落位；重开
/// 后重放逐字节相等、行数恰一套。
#[test]
#[allow(clippy::too_many_lines)]
fn bridge_fault_mixed_rung_torn_wal_tail_discards_both_sides_together() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let root = TestRoot::new("mixed-torn-tail");
    let mut child = spawn_child("mixed-bridge-commit", &root.layout);
    let (permit_id, plan_id) = decode_mixed_marker(&await_marker(&mut child));
    kill_and_reap(&mut child);

    truncate_wal_inside_last_commit(&root.layout.task_path());

    let recovered = reopen_task(&root.layout);
    assert_permit_issued_at_prefix(&recovered, permit_id);
    assert_no_terminal_bridge_rows(&root.layout.task_path());
    assert_eq!(
        recovered
            .inspect_semantic_commit_progress(plan_id)
            .expect("progress")
            .plan
            .state,
        SemanticCommitPlanState::Ready,
        "the FINALIZED flip must be discarded with the receipt"
    );
    assert_eq!(
        raw_count(
            &root.layout.task_path(),
            "SELECT COUNT(*) FROM task_semantic_publication_receipts"
        ),
        1,
        "the READY prefix publications survive the torn tail"
    );
    assert_integrity(&root.layout.task_path());

    // Redo against the real owners reopened from the durable files.
    let semantic = SemanticAuthority::open(root.layout.semantic_root()).expect("semantic");
    let owner = OwnerFixture::new(&root.layout.resource_root(), 0xa1);
    let reservations = bridge_reservations(&owner);
    let receipt = committed_combined(
        recovered
            .finalize_commit_v3_with_semantic_publications_and_resource_authority(
                &semantic,
                &owner.authority,
                plan_id,
                finalize_request(permit_id, 1_700),
            )
            .expect("redo mixed bridge finalize after torn tail"),
    );
    assert_eq!(receipt.task_receipt.new_head_commit_seq, 1);
    assert_eq!(receipt.semantic_publications.len(), 1);
    assert_full_two_reservation_aggregate(&receipt.resource_cost_receipts);
    assert_eq!(
        receipt.resource_cost_receipts,
        expected_nested(&owner, &reservations)
    );
    drop(recovered);
    let verified = reopen_task(&root.layout);
    let replay = replayed_combined(
        verified
            .finalize_commit_v3_with_semantic_publications_and_resource_authority(
                &semantic,
                &owner.authority,
                plan_id,
                finalize_request(permit_id, 9_999),
            )
            .expect("replay after redo"),
    );
    assert_eq!(replay, receipt);
    assert_eq!(
        raw_count(
            &root.layout.task_path(),
            "SELECT COUNT(*) FROM task_receipts"
        ),
        1
    );
    assert_eq!(
        raw_count(
            &root.layout.task_path(),
            "SELECT COUNT(*) FROM task_resource_cost_receipts"
        ),
        2
    );
    assert_eq!(
        raw_count(
            &root.layout.task_path(),
            "SELECT COUNT(*) FROM task_resource_cost_consumptions"
        ),
        3
    );
    assert_integrity(&root.layout.task_path());
    drop(verified);
    drop(root);
}

// ---------------------------------------------------------------------------
// W5 × mixed rung: kill-window replay idempotence (lease variant)
// ---------------------------------------------------------------------------

/// W5（mixed，lease 变体）：mixed 桥接 commit 成功后，同一 finalize 请求
/// 连续重放 3 次 + 重开（对空 owner）重放 1 次 → 每次 `Replayed` 且与
/// committed 逐字节相等；receipt/publication/嵌套行各恰好一套，plan
/// FINALIZED 绑定唯一 receipt。
#[test]
#[allow(clippy::too_many_lines)]
fn bridge_fault_mixed_rung_replay_storm_keeps_both_sets_single_and_byte_equal() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let root = TestRoot::new("mixed-replay-storm");
    let seed = seed_semantic_authority(&root.layout.semantic_root());
    let semantic = SemanticAuthority::open(root.layout.semantic_root()).expect("semantic");
    let owner = OwnerFixture::new(&root.layout.resource_root(), 0xb1);
    let quote = owner.quote(0xb2, 60);
    let reservation = owner.reserve(
        &quote,
        CallId::from_bytes([0xb4; 16]),
        OperationId::from_bytes([0xb5; 16]),
        IdempotencyKey::from_bytes([0xb6; 16]),
    );
    let authority = open_task_shim(&root.layout);
    let mixed = setup_mixed_bridge(
        &authority,
        &root.layout,
        &semantic,
        &seed,
        &owner,
        &[reservation],
        PermitPath::AuthorityLease,
    );
    let lease = mixed.lease.expect("lease path");
    let (plan_id, expected_publication) =
        drive_semantic_to_ready(&authority, &mixed, &semantic, &seed);
    owner.settle(&reservation, &[(1, 12)], 12, 0xb7);
    let request = finalize_request(mixed.permit.permit_id, 1_700);

    let committed = committed_combined(
        authority
            .finalize_commit_v3_with_semantic_publications_and_resource_authority_and_authority_lease(
                &semantic,
                &owner.authority,
                plan_id,
                request.clone(),
                lease,
            )
            .expect("lease mixed bridge commit"),
    );
    assert_eq!(committed.semantic_publications, vec![expected_publication]);
    assert_eq!(committed.resource_cost_receipts.len(), 1);
    assert_eq!(
        committed.resource_cost_receipts[0]
            .finalization
            .refund_credit,
        48
    );

    for _ in 0..3 {
        let replay = replayed_combined(
            authority
                .finalize_commit_v3_with_semantic_publications_and_resource_authority_and_authority_lease(
                    &semantic,
                    &owner.authority,
                    plan_id,
                    request.clone(),
                    lease,
                )
                .expect("storm replay"),
        );
        assert_eq!(replay, committed, "every storm replay is byte-equal");
    }
    drop(authority);
    let reopened = reopen_task(&root.layout);
    let empty_semantic =
        SemanticAuthority::open(root.layout.empty_replay_semantic_root()).expect("empty semantic");
    let empty_owner =
        ResourceAuthority::open(root.layout.empty_replay_resource_root()).expect("empty owner");
    let replay = replayed_combined(
        reopened
            .finalize_commit_v3_with_semantic_publications_and_resource_authority_and_authority_lease(
                &empty_semantic,
                &empty_owner,
                plan_id,
                request,
                lease,
            )
            .expect("replay after reopen"),
    );
    assert_eq!(replay, committed);
    let progress = reopened
        .inspect_semantic_commit_progress(plan_id)
        .expect("progress");
    assert_eq!(progress.plan.state, SemanticCommitPlanState::Finalized);
    assert_eq!(
        progress.plan.task_receipt_id,
        Some(committed.task_receipt.receipt_id)
    );
    assert_eq!(
        raw_count(
            &root.layout.task_path(),
            "SELECT COUNT(*) FROM task_receipts"
        ),
        1,
        "retry storm must not duplicate the receipt"
    );
    assert_eq!(
        raw_count(
            &root.layout.task_path(),
            "SELECT COUNT(*) FROM task_semantic_publication_receipts"
        ),
        1
    );
    assert_eq!(
        raw_count(
            &root.layout.task_path(),
            "SELECT COUNT(*) FROM task_resource_cost_receipts"
        ),
        1
    );
    assert_eq!(
        raw_count(
            &root.layout.task_path(),
            "SELECT COUNT(*) FROM task_resource_cost_consumptions"
        ),
        1
    );
    assert_integrity(&root.layout.task_path());
}
