//! W26-001 semantic recovery ledger F1-F4 fault-injection matrix: the v42
//! `task_semantic_recovery` table group (CAS record/read-back, due scan,
//! alert surface, finalize-wired resolve) under the PoC-0003-aligned fault
//! matrix, closing the verification lane of the unified recovery plane.
//!
//! The harness reuses the `nlos-store-fault` VFS patterns established by
//! `takeover_fault_injection.rs` / `fault_injection.rs` and the restart-scan
//! replay shape of `artifact_multi_record_fault_restart.rs`: kill-9 child
//! processes synchronized through piped `READY` markers (never sleeps),
//! `FAULT_LOCK` process-wide serialization, typed error-chain assertions,
//! raw table-level counts, and a `PRAGMA integrity_check` re-verification at
//! the end of every scenario. The store/plan fixture mirrors
//! `semantic_recovery_ledger.rs` (itself the `semantic_pending_restart_scan.rs`
//! construction).
//!
//! Covered rows (public API only):
//! - F1 kill-9 mid-`record_semantic_recovery_failure` transaction: the
//!   interrupted ledger insert rolls back completely; the restarted due scan
//!   replays the record exactly once and the CAS fences any stale replay
//!   from double-recording;
//! - F2 kill-9 after the record commit: the durable `total_failures` prefix
//!   survives bitwise (backoff schedule included), the stale pre-crash
//!   replay of the very same request is CAS-rejected, and the re-read-driven
//!   continuation advances honestly on the same single row;
//! - F3 hard I/O error on the record transaction: the call fails closed with
//!   a typed storage error naming the injected condition, no half ledger
//!   state or plan-fact damage is committed, and the identical request
//!   succeeds once the fault is removed;
//! - F4 silent write loss (`PowerLossAfter`): the phantom record that
//!   "committed" never becomes durable, the ledger-less rescan rediscovers
//!   the plan, the redo counts the failure exactly once, and convergence
//!   reaches the unique `Finalized`/`Resolved` terminal state.
//!
//! **Crash semantics disclaimer**: the kill-9 rows use forced child
//! termination to simulate *process* crashes; the OS page cache survives a
//! process death, so a killed process is NOT a machine power loss. Writes
//! the kernel accepted but the disk never saw are covered by
//! [`FaultMode::PowerLossAfter`]. All evidence is single-node local
//! `SQLite` under the `nlos-store-fault` VFS shim — it proves nothing about
//! cross-authority atomicity (Task/Semantic/Artifact remain separate durable
//! domains). The fault state in `nlos-store-fault` is process-global, so
//! every test holds `FAULT_LOCK` for its entire duration; children never
//! arm the fault machinery.

use std::error::Error as _;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use nlos_artifact::ArtifactStore;
use nlos_capability::CapabilityTarget;
use nlos_semantic::{PublishSemanticPublicationRequest, SemanticAuthority};
use nlos_store_fault::{FaultCode, FaultMode};
use nlos_task::{
    AttemptSpec, FinalizeSemanticCommitRequest, NestedSemanticPublicationReceipt,
    ParticipantRegistryBinding, PermitDecision, PermitRequest, PlanSemanticCommitRequest,
    RecordSemanticPublicationsRequest, SemanticCommitPlanId, SemanticCommitPlanState,
    SemanticFinalizeDecision, SemanticRecoveryFailureRequest, SemanticRecoveryFailureSource,
    SemanticRecoveryRecord, SemanticRecoveryState, SnapshotBundle, SnapshotConsistency,
    SqliteTaskAuthority, TaskSnapshotReceiptSpec, TaskSpec, TaskStoreError, TaskWriteSetRequest,
    TaskWriteSetSemanticAppendRequest, TaskWriteSetSemanticRequiredDurability,
    TaskWriteSetSemanticTarget, empty_effect_history_root,
};
use nlos_types::{
    CancellationScopeId, Generation, IdempotencyKey, NamespaceId, ReceiptId, SemanticEventId,
    TaskAttemptId, TaskId, TaskSnapshotId,
};
use rusqlite::Connection;

const VFS_NAME: &str = "nlos-task-semantic-recovery-fault";

static FAULT_LOCK: Mutex<()> = Mutex::new(());

fn fault_lock() -> MutexGuard<'static, ()> {
    FAULT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

static NEXT: AtomicU64 = AtomicU64::new(1);

struct Fixture {
    base: PathBuf,
    task_path: PathBuf,
    semantic_root: PathBuf,
    artifact_root: PathBuf,
}

/// Derives the three authority locations from one base path. Parent and
/// kill-9 child share this derivation (the child receives the base through
/// an environment variable), so both address the same durable bytes.
fn authority_paths(base: &Path) -> (PathBuf, PathBuf, PathBuf) {
    (
        base.with_extension("sqlite3"),
        base.with_extension("semantic"),
        base.with_extension("artifact"),
    )
}

impl Fixture {
    fn new() -> Self {
        let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "nlos-task-semantic-recovery-fault-{}-{suffix}",
            std::process::id()
        ));
        let (task_path, semantic_root, artifact_root) = authority_paths(&base);
        Self {
            base,
            task_path,
            semantic_root,
            artifact_root,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let mut path = self.task_path.as_os_str().to_os_string();
            path.push(suffix);
            let _ = fs::remove_file(path);
        }
        let _ = fs::remove_dir_all(&self.semantic_root);
        let _ = fs::remove_dir_all(&self.artifact_root);
    }
}

fn seed_semantic_authority(
    root: &Path,
) -> (SemanticAuthority, SemanticEventId, ReceiptId, ReceiptId) {
    let semantic = SemanticAuthority::open(root).expect("open Semantic authority");
    let event_id = SemanticEventId::from_bytes([0x90; 32]);
    let admission_receipt_id = ReceiptId::from_bytes([0xa0; 16]);
    let durability_receipt_id = ReceiptId::from_bytes([0xb0; 16]);
    let target = NamespaceId::from_bytes([0xc0; 16]);
    let raw = Connection::open(root.join("semantic-authority.db")).expect("open raw Semantic db");
    raw.execute(
        "INSERT INTO content_objects (content_digest, media_type, exact_bytes)
         VALUES (?1, ?2, ?3)",
        rusqlite::params![[0xd0u8; 32].as_slice(), "text/plain", b"semantic"],
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
            [0xe1u8, 0xe2, 0xe3].as_slice(),
            target.as_bytes().as_slice(),
            [0xe4u8; 16].as_slice(),
            [0xe5u8; 16].as_slice(),
            [0xe6u8; 16].as_slice(),
            [0xe7u8; 16].as_slice(),
            [0xd0u8; 32].as_slice(),
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
            [0xe8u8; 32].as_slice(),
            [0xe9u8; 16].as_slice(),
            [0xeau8; 16].as_slice(),
            [0xebu8; 16].as_slice(),
            [0xecu8; 64].as_slice(),
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
            [0xedu8; 32].as_slice(),
            [0xeeu8; 64].as_slice(),
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

/// Builds a task authority holding one non-Finalized semantic commit plan at
/// the given locations, reusing the `semantic_recovery_ledger.rs`
/// construction (itself the `semantic_pending_restart_scan.rs` fixture):
/// seeded Semantic authority, sealed write set with one durable semantic
/// append, commit permit, then `plan_semantic_commit`. With `fault_vfs` the
/// task authority is opened through the registered fault-injection shim so
/// its writes can be failed or dropped; the Semantic/Artifact authorities
/// always use the default VFS (the shim only intercepts files opened
/// through its own name).
// The reference fixture drives the deprecated unbound seal/permit entry
// points; mirroring it verbatim keeps this fault fixture reviewable against
// its source.
#[allow(deprecated)]
#[allow(clippy::too_many_lines)] // One verbatim fixture construction, mirroring its source.
fn build_authorities_at(
    task_path: &Path,
    semantic_root: &Path,
    artifact_root: &Path,
    fault_vfs: bool,
) -> (SqliteTaskAuthority, SemanticAuthority, SemanticCommitPlanId) {
    let (semantic, event_id, admission_receipt_id, durability_receipt_id) =
        seed_semantic_authority(semantic_root);
    let artifact = ArtifactStore::open(artifact_root).expect("open Artifact authority");
    let task_id = TaskId::from_bytes([0x10; 16]);
    let attempt_id = TaskAttemptId::from_bytes([0x11; 16]);
    let target = NamespaceId::from_bytes([0xc0; 16]);
    let attempt = AttemptSpec {
        task_id,
        attempt_id,
        attempt_generation: Generation::INITIAL,
        snapshot: SnapshotBundle {
            snapshot_id: TaskSnapshotId::from_bytes([0x12; 16]),
            snapshot_digest: [0x13; 32],
            expected_head_commit_seq: 0,
            effect_history_root: empty_effect_history_root(),
            retry_fence_epoch: 0,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes([0x14; 16]),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes([0x15; 16]),
        registered_at_ms: 10,
    };
    let task = if fault_vfs {
        open_shim(task_path)
    } else {
        SqliteTaskAuthority::open(task_path).expect("open Task authority")
    };
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
        receipt_id: ReceiptId::from_bytes([0x16; 16]),
        builder_id: [0x17; 16],
        builder_version_digest: [0x18; 32],
        per_authority_checkpoint_receipts: vec![ReceiptId::from_bytes([0x19; 16])],
        dependency_closure_root: [0x1a; 32],
        semantic_resolver_digest: [0x1b; 32],
        canonical_iteration_digest: [0x1c; 32],
        achieved_consistency: SnapshotConsistency::Causal,
        built_at_ms: 2,
        authority_id: [0x1d; 16],
        key_id: [0x1e; 16],
        signature: [0x1f; 64],
    })
    .unwrap();
    task.register_attempt_with_snapshot_receipt(attempt, ReceiptId::from_bytes([0x16; 16]))
        .unwrap();
    let registry = task.inspect_participant_registry(task_id).unwrap();
    task.register_semantic_admission_participant(
        &semantic,
        task_id,
        ParticipantRegistryBinding {
            generation: registry.generation,
            root: registry.root,
        },
        3,
    )
    .unwrap();
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
                    expected_admission_policy_digest: [0xe8; 32],
                    durability_receipt_id: Some(durability_receipt_id),
                }],
                resource_reservations: Vec::new(),
                planned_effects: Vec::new(),
                effect_endpoints: Vec::new(),
                idempotency_key: IdempotencyKey::from_bytes([0x20; 16]),
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
    let permit = match task
        .request_commit_permit(PermitRequest {
            task_id,
            attempt_id,
            attempt_generation: Generation::INITIAL,
            write_set_root: write_set.write_set_root,
            planned_effects: Vec::new(),
            idempotency_key: IdempotencyKey::from_bytes([0x21; 16]),
            valid_until_ms: 1_000,
            requested_at_ms: 5,
        })
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
            idempotency_key: IdempotencyKey::from_bytes([0x22; 16]),
            planned_at_ms: 6,
        })
        .unwrap()
        .record()
        .plan_id;
    (task, semantic, plan)
}

/// One Semantic recovery failure request with the shared test delays
/// (base 100 ms, capped 5 000 ms, coordinator-reported).
fn failure_request(
    plan_id: SemanticCommitPlanId,
    expected_total_failures: u64,
    observed_at_ms: i64,
) -> SemanticRecoveryFailureRequest {
    SemanticRecoveryFailureRequest {
        plan_id,
        expected_total_failures,
        source: SemanticRecoveryFailureSource::Coordinator,
        observed_at_ms,
        base_delay_ms: 100,
        max_delay_ms: 5_000,
    }
}

/// Records one Semantic recovery failure and returns the durable record.
fn record_failure(
    authority: &SqliteTaskAuthority,
    plan_id: SemanticCommitPlanId,
    expected_total_failures: u64,
    observed_at_ms: i64,
) -> SemanticRecoveryRecord {
    authority
        .record_semantic_recovery_failure(failure_request(
            plan_id,
            expected_total_failures,
            observed_at_ms,
        ))
        .expect("record semantic recovery failure")
}

/// Manually replays the coordinator's converge prefix from
/// `semantic_pending_restart_scan.rs` without the coordinator crate:
/// authorize the publication, let the owner authority publish, and consume
/// the nested receipt set until the plan is `Ready`.
fn converge_semantic_plan_to_ready(
    task: &SqliteTaskAuthority,
    semantic: &SemanticAuthority,
    plan_id: SemanticCommitPlanId,
    now_ms: i64,
) {
    task.authorize_semantic_publication(plan_id, now_ms)
        .expect("authorize semantic publication");
    let progress = task.inspect_semantic_commit_progress(plan_id).unwrap();
    assert_eq!(progress.plan.state, SemanticCommitPlanState::Publishing);
    let expectation = task
        .inspect_semantic_commit_expectations(plan_id)
        .unwrap()
        .into_iter()
        .next()
        .expect("fixture declares one Semantic publication");
    let owner = semantic
        .publish_semantic_publication(PublishSemanticPublicationRequest {
            task_id: progress.plan.task_id,
            permit_id: progress.plan.permit_id,
            write_set_root: progress.plan.write_set_root,
            event_id: expectation.event_id,
            target: match expectation.target {
                TaskWriteSetSemanticTarget::Namespace(namespace) => {
                    CapabilityTarget::Namespace(namespace)
                }
                TaskWriteSetSemanticTarget::Task(task) => CapabilityTarget::Task(task),
            },
            admission_receipt_id: expectation.admission_receipt_id,
            durability_receipt_id: expectation.durability_receipt_id,
            published_at_ms: u64::try_from(now_ms).expect("non-negative converge clock"),
        })
        .expect("owner publishes sealed expectation")
        .receipt();
    let nested = NestedSemanticPublicationReceipt {
        receipt_id: owner.receipt_id,
        task_id: owner.task_id,
        permit_id: owner.permit_id,
        write_set_root: owner.write_set_root,
        event_id: owner.event_id,
        target: expectation.target,
        log_seq: owner.log_seq,
        admission_receipt_id: owner.admission_receipt_id,
        durability_receipt_id: owner.durability_receipt_id,
        semantic_checkpoint_after: owner.semantic_checkpoint_after,
        created_at_ms: owner.created_at_ms,
    };
    let updated = task
        .record_semantic_publications(
            semantic,
            RecordSemanticPublicationsRequest {
                plan_id,
                receipts: vec![nested],
                observed_at_ms: now_ms,
            },
        )
        .expect("consume owner publication receipt");
    assert_eq!(updated.plan.state, SemanticCommitPlanState::Ready);
}

/// Drives the full `semantic_pending_restart_scan.rs` converge loop to the
/// terminal finalize.
fn converge_semantic_plan(
    task: &SqliteTaskAuthority,
    semantic: &SemanticAuthority,
    plan_id: SemanticCommitPlanId,
    now_ms: i64,
) -> nlos_task::SemanticTaskCommitReceipt {
    converge_semantic_plan_to_ready(task, semantic, plan_id, now_ms);
    let decision = task
        .finalize_semantic_commit(FinalizeSemanticCommitRequest {
            plan_id,
            finalized_at_ms: now_ms,
        })
        .expect("terminal semantic finalize");
    match decision {
        SemanticFinalizeDecision::Committed(receipt)
        | SemanticFinalizeDecision::Replayed(receipt) => *receipt,
    }
}

// ---------------------------------------------------------------------------
// fault plumbing (`takeover_fault_injection.rs` patterns)
// ---------------------------------------------------------------------------

fn open_shim(path: &Path) -> SqliteTaskAuthority {
    nlos_store_fault::register(VFS_NAME).expect("register fault vfs");
    SqliteTaskAuthority::open_with_vfs(path, Some(VFS_NAME)).expect("open via fault vfs")
}

/// Runs `PRAGMA integrity_check` on the test's own rusqlite connection, as
/// parent-side verification independent of the authority under test.
fn assert_integrity(path: &Path) {
    let connection = Connection::open(path).expect("open for integrity check");
    let result: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("run integrity_check");
    assert_eq!(result, "ok", "integrity_check must pass");
}

/// Row count of one authority table, read through an independent raw
/// connection (WAL readers do not disturb the writer under test).
fn raw_count(path: &Path, table: &str) -> i64 {
    let connection = Connection::open(path).expect("open raw reader");
    connection
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .expect("count rows")
}

/// Full `Display` chain of a `TaskStoreError`, top cause last, for content
/// assertions (e.g. that `SQLITE_IOERR`'s message reaches the caller).
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
/// condition (`"i/o"` or `"ioerr"`): never a fake success, never a panic.
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

fn hex_encode(value: &[u8]) -> String {
    use std::fmt::Write as _;

    value
        .iter()
        .fold(String::with_capacity(value.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

// ---------------------------------------------------------------------------
// kill-9 child-process harness (`fault_injection.rs` /
// `takeover_fault_injection.rs` 范式: current_exe + env var + piped READY
// marker carrying the plan id, never sleeps)
// ---------------------------------------------------------------------------

fn spawn_child(scenario: &str, base: &Path) -> Child {
    Command::new(std::env::current_exe().expect("current test executable"))
        .args(["--exact", "crash_child_helper", "--nocapture"])
        .env("NLOS_SEMANTIC_CRASH_CHILD_SCENARIO", scenario)
        .env("NLOS_SEMANTIC_CRASH_CHILD_BASE", base)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn crash child")
}

/// Blocks until the child prints its `READY:<plan-id-hex>` marker (pipe
/// synchronization, no sleeps) and returns the marker line; kills and reaps
/// the child on timeout or early exit.
fn await_marker(child: &mut Child) -> String {
    let stdout = child.stdout.take().expect("piped stdout");
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // The libtest harness prints its own banner lines before the
        // helper's marker; scan until the marker (or EOF when the child
        // dies early).
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
        Ok(Ok(line)) => line,
        other => {
            let _ = child.kill();
            let _ = child.wait();
            panic!("child did not report READY: {other:?}");
        }
    }
}

/// Force-terminates the child and proves it did not exit cleanly.
fn kill_and_reap(child: &mut Child) {
    child.kill().expect("force-terminate child");
    let status = child.wait().expect("wait child");
    assert!(
        !status.success(),
        "killed child must not exit cleanly: {status}"
    );
}

/// Decodes the plan id carried by the child's `READY` marker so the parent
/// can address the same durable plan.
fn plan_id_from_marker(marker: &str) -> SemanticCommitPlanId {
    let hex = marker.strip_prefix("READY:").expect("READY marker payload");
    assert_eq!(hex.len(), 32, "plan id hex width");
    let mut bytes = [0_u8; 16];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).expect("plan id hex digit");
    }
    SemanticCommitPlanId::from_bytes(bytes)
}

fn announce_ready(plan_id: SemanticCommitPlanId) {
    println!("READY:{}", hex_encode(plan_id.as_bytes()));
    std::io::stdout().flush().expect("flush marker");
}

// ---------------------------------------------------------------------------
// kill-9 child scenarios
// ---------------------------------------------------------------------------

/// F1 fixture: the semantic plan prefix is committed, then a writer
/// transaction inserts a ledger row for the real plan (the exact row
/// `record_semantic_recovery_failure` would commit: total=1, Retrying,
/// `next_retry=1_100`) and dies before commit. If the interrupted insert
/// survived, the parent's replay would collide on the primary key / CAS.
fn child_mid_record_tx(base: &Path) -> ! {
    let (task_path, semantic_root, artifact_root) = authority_paths(base);
    let (authority, semantic, plan_id) =
        build_authorities_at(&task_path, &semantic_root, &artifact_root, false);
    let raw = Connection::open(&task_path).expect("open raw authority connection");
    raw.execute_batch("BEGIN IMMEDIATE").expect("begin mid-tx");
    raw.execute(
        "INSERT INTO task_semantic_recovery (
            plan_id, recovery_state, consecutive_failures, total_failures,
            last_failure_source, first_failed_at_ms, last_failed_at_ms,
            next_retry_at_ms, escalated_at_ms, resolved_at_ms, updated_at_ms
         ) VALUES (?1, 0, ?2, ?2, 2, 1000, 1000, 1100, NULL, NULL, 1000)",
        rusqlite::params![plan_id.as_bytes().as_slice(), encode_total_one().as_slice(),],
    )
    .expect("mid-tx phantom ledger row");
    announce_ready(plan_id);
    let _keepers = (authority, semantic, raw);
    loop {
        std::thread::park();
    }
}

/// Big-endian 8-byte blob for `total_failures = 1` (the store's
/// `encode_u64` wire shape), shared by the phantom row only.
fn encode_total_one() -> [u8; 8] {
    1_u64.to_be_bytes()
}

/// F2 fixture: exactly one `record_semantic_recovery_failure` transaction
/// (expected=0, observed at `1_000`) is committed through the public API and
/// returns before the kill; `total_failures` is durable at 1.
fn child_record_commit_complete(base: &Path) -> ! {
    let (task_path, semantic_root, artifact_root) = authority_paths(base);
    let (authority, semantic, plan_id) =
        build_authorities_at(&task_path, &semantic_root, &artifact_root, false);
    let record = record_failure(&authority, plan_id, 0, 1_000);
    assert_eq!(record.total_failures, 1);
    assert_eq!(record.state, SemanticRecoveryState::Retrying);
    announce_ready(plan_id);
    let _keepers = (authority, semantic);
    loop {
        std::thread::park();
    }
}

/// Child entry point. Runs only when spawned by a parent test with the
/// scenario environment set; a no-op in the normal test run.
#[test]
fn crash_child_helper() {
    let (Ok(scenario), Ok(base)) = (
        std::env::var("NLOS_SEMANTIC_CRASH_CHILD_SCENARIO"),
        std::env::var("NLOS_SEMANTIC_CRASH_CHILD_BASE"),
    ) else {
        return;
    };
    let base = PathBuf::from(base);
    match scenario.as_str() {
        "mid-record-tx" => child_mid_record_tx(&base),
        "record-commit-complete" => child_record_commit_complete(&base),
        other => panic!("unknown crash child scenario {other}"),
    }
}

// ---------------------------------------------------------------------------
// 矩阵行 1 (F1): kill-9 mid-record transaction
// ---------------------------------------------------------------------------

/// kill-9 中断 record 事务：子进程在 `BEGIN IMMEDIATE` 未提交(已插入与真实
/// 重放逐位相同的台账行:total=1、Retrying、`next_retry=1_100`)时被强杀;重开后
/// 中断事务完全回滚——台账无幻影行、plan 保持 `Planned` 且立即回到期扫描
/// (无台账行即到期,SEM-RECOV-005);重启 worker 重放 record 恰好记一次
/// (total=1、单行),再次以过期 expected=0 重放被 CAS 拒绝——任何崩溃窗口下
/// 都不产生双记。
#[test]
#[allow(clippy::too_many_lines)] // One test covers a full F1-F4 fault-matrix row.
fn fault_kill9_mid_record_tx_rolls_back_and_replay_records_once() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let fixture = Fixture::new();
    let mut child = spawn_child("mid-record-tx", &fixture.base);
    let marker = await_marker(&mut child);
    let plan_id = plan_id_from_marker(&marker);
    kill_and_reap(&mut child);

    // Nothing uncommitted may survive: no ledger row, no phantom alert
    // receipt.
    assert_eq!(raw_count(&fixture.task_path, "task_semantic_recovery"), 0);
    assert_eq!(
        raw_count(&fixture.task_path, "task_semantic_recovery_alert_receipts"),
        0
    );

    let authority = SqliteTaskAuthority::open(&fixture.task_path).expect("reopen after kill");
    assert!(
        authority
            .inspect_semantic_recovery(plan_id)
            .expect("inspect ledger")
            .is_none(),
        "interrupted ledger insert must roll back completely"
    );
    // The plan itself is the durable fact: still Planned, and the
    // ledger-less rescan returns it immediately.
    assert_eq!(
        authority
            .inspect_semantic_commit_progress(plan_id)
            .expect("inspect plan")
            .plan
            .state,
        SemanticCommitPlanState::Planned
    );
    let due = authority
        .list_due_semantic_commit_plans(10, 0)
        .expect("ledger-less rescan");
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].plan_id, plan_id);

    // The restarted worker replays the record exactly once.
    let replayed = record_failure(&authority, plan_id, 0, 2_000);
    assert_eq!(replayed.total_failures, 1);
    assert_eq!(replayed.consecutive_failures, 1);
    assert_eq!(replayed.state, SemanticRecoveryState::Retrying);
    assert_eq!(replayed.next_retry_at_ms, Some(2_100));
    assert_eq!(raw_count(&fixture.task_path, "task_semantic_recovery"), 1);

    // A stale replay carrying the pre-restart view (expected=0) is
    // CAS-fenced: the durable total cannot be double-recorded.
    assert!(matches!(
        authority.record_semantic_recovery_failure(failure_request(plan_id, 0, 2_100)),
        Err(TaskStoreError::SemanticRecoveryCasMismatch {
            expected: 0,
            current: 1
        })
    ));
    assert_eq!(raw_count(&fixture.task_path, "task_semantic_recovery"), 1);
    assert_integrity(&fixture.task_path);
}

// ---------------------------------------------------------------------------
// 矩阵行 2 (F2): kill-9 after the record commit
// ---------------------------------------------------------------------------

/// commit 后崩溃:子进程在一条 record 事务(expected=0,observed `1_000`)提交
/// 返回后被强杀;重开后已提交前缀逐位保留(单行、total=1、Retrying、
/// `next_retry=1_100`、失败时间戳与来源完整);过期退避日程驱动重启扫描
/// (`1_000` 未到期、`1_100` 到期);崩溃前同一请求的过期重放被 CAS 拒绝(不
/// 双记),按重读值续记(expected=1)在同一行上诚实地推进到 total=2。
#[test]
fn fault_kill9_after_record_commit_keeps_durable_total_and_fences_stale_replay() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let fixture = Fixture::new();
    let mut child = spawn_child("record-commit-complete", &fixture.base);
    let marker = await_marker(&mut child);
    let plan_id = plan_id_from_marker(&marker);
    kill_and_reap(&mut child);

    // The committed prefix survives the crash bitwise: exactly one ledger
    // row with the durable total and its backoff schedule.
    assert_eq!(raw_count(&fixture.task_path, "task_semantic_recovery"), 1);
    let authority = SqliteTaskAuthority::open(&fixture.task_path).expect("reopen after kill");
    let durable = authority
        .inspect_semantic_recovery(plan_id)
        .expect("inspect ledger")
        .expect("committed record survives the kill");
    assert_eq!(durable.state, SemanticRecoveryState::Retrying);
    assert_eq!(durable.consecutive_failures, 1);
    assert_eq!(durable.total_failures, 1);
    assert_eq!(
        durable.last_source,
        SemanticRecoveryFailureSource::Coordinator
    );
    assert_eq!(durable.first_failed_at_ms, 1_000);
    assert_eq!(durable.last_failed_at_ms, 1_000);
    assert_eq!(durable.next_retry_at_ms, Some(1_100));
    assert_eq!(durable.escalated_at_ms, None);
    assert_eq!(durable.resolved_at_ms, None);

    // The durable backoff schedule drives the restarted scan.
    assert!(
        authority
            .list_due_semantic_commit_plans(10, 1_000)
            .expect("scan before due")
            .is_empty()
    );
    let due = authority
        .list_due_semantic_commit_plans(10, 1_100)
        .expect("scan at due");
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].plan_id, plan_id);

    // The stale pre-crash replay of the very same request is CAS-rejected:
    // the durable total fences the double record.
    assert!(matches!(
        authority.record_semantic_recovery_failure(failure_request(plan_id, 0, 1_000)),
        Err(TaskStoreError::SemanticRecoveryCasMismatch {
            expected: 0,
            current: 1
        })
    ));

    // The re-read-driven continuation advances honestly on the same row.
    let continued = record_failure(&authority, plan_id, 1, 2_000);
    assert_eq!(continued.total_failures, 2);
    assert_eq!(continued.consecutive_failures, 2);
    assert_eq!(continued.next_retry_at_ms, Some(2_200));
    assert_eq!(raw_count(&fixture.task_path, "task_semantic_recovery"), 1);
    assert_integrity(&fixture.task_path);
}

// ---------------------------------------------------------------------------
// 矩阵行 3 (F3): hard I/O error on the record write fails closed
// ---------------------------------------------------------------------------

/// 写入硬 I/O 错误(record 事务):`FailWritesAfter { 0, IoErr }` 下
/// `record_semantic_recovery_failure` 必须以 `TaskStoreError::Sqlite`
/// 显式失败(错误链含 I/O 条件),不返回假成功;无半截状态(台账无行、
/// plan 保持 `Planned`、到期扫描照常返回该 plan——plan 事实无损);
/// disarm 后同一请求重试成功(total=1、单行、回读一致)。
#[test]
fn fault_io_error_on_record_fails_closed_and_retry_succeeds() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let fixture = Fixture::new();
    let (authority, _semantic, plan_id) = build_authorities_at(
        &fixture.task_path,
        &fixture.semantic_root,
        &fixture.artifact_root,
        true,
    );

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::IoErr,
    });
    let error = authority
        .record_semantic_recovery_failure(failure_request(plan_id, 0, 1_000))
        .expect_err("ledger record must fail under injected I/O error");
    assert_sqlite_error_chain(&error, &["i/o", "ioerr"]);
    assert!(nlos_store_fault::writes_observed() > 0);

    // Fail-closed leaves no half state and no plan-fact damage.
    assert_eq!(raw_count(&fixture.task_path, "task_semantic_recovery"), 0);
    assert!(
        authority
            .inspect_semantic_recovery(plan_id)
            .expect("inspect ledger")
            .is_none()
    );
    assert_eq!(
        authority
            .inspect_semantic_commit_progress(plan_id)
            .expect("inspect plan")
            .plan
            .state,
        SemanticCommitPlanState::Planned
    );
    let due = authority
        .list_due_semantic_commit_plans(10, 0)
        .expect("scan after failure");
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].plan_id, plan_id);

    // Removing the fault makes the very same request succeed.
    nlos_store_fault::disarm();
    let recorded = record_failure(&authority, plan_id, 0, 1_000);
    assert_eq!(recorded.total_failures, 1);
    assert_eq!(recorded.state, SemanticRecoveryState::Retrying);
    assert_eq!(
        authority
            .inspect_semantic_recovery(plan_id)
            .expect("inspect ledger after retry"),
        Some(recorded)
    );
    assert_eq!(raw_count(&fixture.task_path, "task_semantic_recovery"), 1);
    assert_integrity(&fixture.task_path);
}

// ---------------------------------------------------------------------------
// 矩阵行 4 (F4): silent write loss — converge to the unique terminal
// ---------------------------------------------------------------------------

/// 静默丢写(断电模型):`PowerLossAfter { 0 }` 下 record 事务"报告成功"
/// 但写入从未落盘;连接必须先死亡(真实断电同样会杀死它)使恢复只看落盘
/// 字节;重开后幻影台账行不得冒充已提交事实(无行、integrity ok),无台账
/// 行的 incomplete plan 立即回到期扫描(SEM-RECOV-005 自愈重扫);丢失的
/// record 可重做且失败恰好计一次(total=1,丢失的写入不计入);随后收敛到
/// 唯一终态:plan `Finalized`、台账同事务置 `Resolved`、finalize 幂等重放
/// 返回原 receipt、告警面与到期扫描为空、单行不变。
#[test]
#[allow(clippy::too_many_lines)] // One test covers a full F1-F4 fault-matrix row.
fn fault_silent_write_loss_converges_to_unique_terminal_state() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let fixture = Fixture::new();
    let (authority, _semantic, plan_id) = build_authorities_at(
        &fixture.task_path,
        &fixture.semantic_root,
        &fixture.artifact_root,
        true,
    );

    // "Power off" during the record transaction: the call reports success
    // but the writes never reach the disk.
    nlos_store_fault::arm(FaultMode::PowerLossAfter { remaining: 0 });
    let phantom = record_failure(&authority, plan_id, 0, 1_000);
    assert_eq!(phantom.total_failures, 1);
    nlos_store_fault::disarm();
    // The surviving connection keeps a wal-index that references frames the
    // disk never saw; it must die first (as a real power loss would kill it)
    // so recovery sees durable bytes alone.
    drop(authority);

    let recovered = SqliteTaskAuthority::open(&fixture.task_path).expect("reopen after power loss");
    assert!(
        recovered
            .inspect_semantic_recovery(plan_id)
            .expect("inspect ledger")
            .is_none(),
        "silently dropped record must not fabricate a ledger row"
    );
    assert_eq!(raw_count(&fixture.task_path, "task_semantic_recovery"), 0);
    assert_integrity(&fixture.task_path);
    // The ledger-less rescan rediscovers the plan: the plan is the durable
    // fact, a lost ledger row never blocks recovery.
    let due = recovered
        .list_due_semantic_commit_plans(10, 0)
        .expect("ledger-less rescan");
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].plan_id, plan_id);

    // The lost record is redoable and counts the failure exactly once.
    let redone = record_failure(&recovered, plan_id, 0, 3_000);
    assert_eq!(redone.total_failures, 1);
    drop(recovered);
    let verified = SqliteTaskAuthority::open(&fixture.task_path).expect("reopen after redo");
    assert_eq!(
        verified
            .inspect_semantic_recovery(plan_id)
            .expect("inspect ledger after redo"),
        Some(redone)
    );

    // Convergence reaches the unique terminal: Finalized plan, Resolved
    // ledger wired into the same finalize transaction.
    let semantic =
        SemanticAuthority::open(&fixture.semantic_root).expect("reopen Semantic authority");
    let committed = converge_semantic_plan(&verified, &semantic, plan_id, 20_000);
    assert_eq!(committed.task_receipt.new_head_commit_seq, 1);
    assert_eq!(
        verified
            .inspect_semantic_commit_progress(plan_id)
            .expect("inspect plan after converge")
            .plan
            .state,
        SemanticCommitPlanState::Finalized
    );
    let resolved = verified
        .inspect_semantic_recovery(plan_id)
        .expect("inspect ledger after converge")
        .expect("resolved row");
    assert_eq!(resolved.state, SemanticRecoveryState::Resolved);
    assert_eq!(resolved.total_failures, 1);
    assert_eq!(resolved.resolved_at_ms, Some(20_000));
    assert!(
        verified
            .list_semantic_recovery_alerts()
            .expect("alerts after converge")
            .is_empty()
    );
    assert!(
        verified
            .list_due_semantic_commit_plans(10, i64::MAX)
            .expect("scan after converge")
            .is_empty()
    );

    // Finalize replay is idempotent: same receipt, no phantom ledger
    // revival, still exactly one row.
    let replay = verified
        .finalize_semantic_commit(FinalizeSemanticCommitRequest {
            plan_id,
            finalized_at_ms: 21_000,
        })
        .expect("finalized replay");
    assert!(matches!(replay, SemanticFinalizeDecision::Replayed(_)));
    assert_eq!(replay.receipt(), &committed);
    assert_eq!(
        verified
            .inspect_semantic_recovery(plan_id)
            .expect("inspect ledger after replay")
            .expect("resolved row after replay")
            .state,
        SemanticRecoveryState::Resolved
    );
    assert_eq!(raw_count(&fixture.task_path, "task_semantic_recovery"), 1);
    assert_integrity(&fixture.task_path);
}
