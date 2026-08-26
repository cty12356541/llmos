#![allow(deprecated)] // Ladder constructors deprecated in favor of the *_with_authorities_struct entries.
//! B-TASK-003 schema-v3 fault-injection tests: the quarantine / adoption /
//! reconcile / effect-history / finalize-proofs table group (landed in
//! 743c88c) under the PoC-0003-aligned F1-F4 fault matrix, completing the
//! item deferred by `b-task-003-reconcile-effect-history.md` and
//! `b-task-003-crash-windows.md`. The harness reuses the `nlos-store-fault`
//! VFS patterns established by `fault_injection.rs` (B-TASK-001) and
//! `effect_fault_injection.rs` (B-TASK-002 effect tables): kill-9 child
//! processes synchronized through piped `READY` markers (never sleeps),
//! `FAULT_LOCK` process-wide serialization, `wal_commit_frames` tail
//! truncation, typed error-chain assertions, raw table-level counts, and a
//! `PRAGMA integrity_check` re-verification at the end of every scenario.
//!
//! Covered v3 flows (public API only):
//! - quarantine tombstone write (`finalize_commit_v3` / `close_permit` on
//!   an `EFFECT_UNKNOWN` slot);
//! - adoption (`adopt_permit`) and reconcile (`reconcile_effect`) with
//!   same-transaction history appends;
//! - `finalize_commit_v3` commits (`EFFECT_CLOSED` + `CONFIRMED_NO_EFFECT`
//!   closures) and `PARTIAL_EFFECT` finalize (fence advance + history
//!   append + finalize-proof row).
//!
//! **Crash semantics disclaimer**: the kill-9 rows use forced child
//! termination (`SIGKILL` on Unix, `TerminateProcess` on Windows via
//! `Child::kill`) to simulate *process* crashes. The OS page cache survives
//! a process death, so a killed process is NOT a machine power loss; writes
//! the kernel accepted but the disk never saw are covered by
//! [`FaultMode::PowerLossAfter`] and by file-level WAL tail truncation.
//! Child processes are synchronized through piped stdout markers (`READY`),
//! never through sleeps. The fault state in `nlos-store-fault` is
//! process-global, so every test holds `FAULT_LOCK` for its entire duration
//! (each integration binary is its own process).

use std::error::Error as _;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use nlos_store_fault::{FaultCode, FaultMode};
use nlos_task::{
    AdoptionReplay, AdoptionRequest, AttemptSpec, AttemptState, EffectPermitDecision,
    EffectPermitRequest, FinalizeDecision, FinalizeRequestV3, IssuedPermit,
    LogicalEffectDescriptor, NoEffectReason, NoEffectRequest, Outcome, OutcomeRequest,
    PermitDecision, PermitRecord, PermitRequest, PermitState, PlannedEffect, ReceiptOutcome,
    ReconcileOutcome, ReconcileReplay, ReconcileRequest, RequiredSatisfaction,
    RequiredSatisfactionProof, SlotState, SnapshotBundle, SqliteTaskAuthority, TaskSpec,
    TaskStoreError, empty_effect_history_root, expected_success_assertion_digest,
};
use nlos_types::{
    CancellationScopeId, CommitPermitId, Generation, IdempotencyKey, ReceiptId, TaskAttemptId,
    TaskId, TaskSnapshotId,
};

const VFS_NAME: &str = "nlos-task-reconcile-fault";

static FAULT_LOCK: Mutex<()> = Mutex::new(());

fn fault_lock() -> MutexGuard<'static, ()> {
    FAULT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

static NEXT_DATABASE: AtomicU64 = AtomicU64::new(0);

struct TestDatabase {
    path: PathBuf,
}

impl TestDatabase {
    fn new(name: &str) -> Self {
        let sequence = NEXT_DATABASE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "nlos-task-reconcile-fault-{name}-{}-{sequence}.sqlite3",
            std::process::id()
        ));
        Self { path }
    }

    fn sibling(path: &Path, suffix: &str) -> PathBuf {
        let mut value = path.as_os_str().to_os_string();
        value.push(suffix);
        PathBuf::from(value)
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        for path in [
            self.path.clone(),
            Self::sibling(&self.path, "-wal"),
            Self::sibling(&self.path, "-shm"),
        ] {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("remove test database: {error}"),
            }
        }
    }
}

/// Full `Display` chain of a `TaskStoreError`, top cause last, for content
/// assertions (e.g. that `SQLITE_FULL`'s message reaches the caller).
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

fn open_shim(path: &Path) -> SqliteTaskAuthority {
    nlos_store_fault::register(VFS_NAME).expect("register fault vfs");
    SqliteTaskAuthority::open_with_vfs(path, Some(VFS_NAME)).expect("open via fault vfs")
}

/// Runs `PRAGMA integrity_check` on the test's own rusqlite connection, as
/// parent-side verification independent of the authority under test.
fn assert_integrity(path: &Path) {
    let connection = rusqlite::Connection::open(path).expect("open for integrity check");
    let result: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("run integrity_check");
    assert_eq!(result, "ok", "integrity_check must pass");
}

/// Row count of one authority table, read through an independent raw
/// connection (WAL readers do not disturb the writer under test).
fn raw_count(path: &Path, table: &str) -> i64 {
    let connection = rusqlite::Connection::open(path).expect("open raw reader");
    connection
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .expect("count rows")
}

/// Asserts a typed storage failure whose cause chain names the injected
/// condition (`"i/o"` or `"full"`): never a fake success, never a panic.
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

/// Asserts every schema-v3 table group is empty: no phantom quarantine /
/// adoption / reconcile / history / sequence / finalize-proof rows.
fn assert_v3_tables_empty(path: &Path) {
    assert_eq!(raw_count(path, "task_quarantine_receipts"), 0);
    assert_eq!(raw_count(path, "task_adoption_receipts"), 0);
    assert_eq!(raw_count(path, "task_reconcile_receipts"), 0);
    assert_eq!(raw_count(path, "effect_history"), 0);
    assert_eq!(raw_count(path, "task_effect_sequences"), 0);
    assert_eq!(raw_count(path, "task_finalize_proofs"), 0);
}

fn bytes(value: u8) -> [u8; 16] {
    [value; 16]
}

fn task_id() -> TaskId {
    TaskId::from_bytes(bytes(0x01))
}

fn task_spec() -> TaskSpec {
    TaskSpec {
        task_id: task_id(),
        task_generation: Generation::INITIAL,
        registered_at_ms: 1_000,
    }
}

fn snapshot(head_seq: u64, fence: u64) -> SnapshotBundle {
    let tag = u8::try_from(head_seq).expect("test head fits in u8");
    SnapshotBundle {
        snapshot_id: TaskSnapshotId::from_bytes(bytes(0x10 + tag)),
        snapshot_digest: [0x20 + tag; 32],
        expected_head_commit_seq: head_seq,
        effect_history_root: if head_seq == 0 {
            empty_effect_history_root()
        } else {
            [0x30 + tag; 32]
        },
        retry_fence_epoch: fence,
    }
}

fn attempt_spec(seed: u8, bundle: SnapshotBundle) -> AttemptSpec {
    AttemptSpec {
        task_id: task_id(),
        attempt_id: TaskAttemptId::from_bytes(bytes(seed)),
        attempt_generation: Generation::INITIAL,
        snapshot: bundle,
        cancellation_scope_id: CancellationScopeId::from_bytes(bytes(0xc0 + seed)),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes(bytes(0xa0 + seed)),
        registered_at_ms: 2_000,
    }
}

fn descriptor(stable_action_slot: u64) -> LogicalEffectDescriptor {
    LogicalEffectDescriptor {
        task_id: task_id(),
        task_generation: Generation::INITIAL,
        intent_spec_id: [0x44; 32],
        stable_action_slot,
        target_authority_object_id: [0x55; 32],
        effect_class: 7,
        idempotency_scope: 3,
    }
}

fn planned(stable_action_slot: u64, required: bool) -> PlannedEffect {
    PlannedEffect {
        descriptor: descriptor(stable_action_slot),
        required,
        required_condition_digest: None,
        success_criteria_digest: [0x66; 32],
        action_proposal_digest: [0x77; 32],
    }
}

fn permit_request(spec: &AttemptSpec, seed: u8, effects: Vec<PlannedEffect>) -> PermitRequest {
    PermitRequest {
        task_id: spec.task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        write_set_root: [seed; 32],
        planned_effects: effects,
        idempotency_key: IdempotencyKey::from_bytes(bytes(0xb0 + seed)),
        valid_until_ms: 9_999,
        requested_at_ms: 3_000,
    }
}

fn effect_request(
    spec: &AttemptSpec,
    permit: &PermitRecord,
    effect_seq: u64,
    key_seed: u8,
) -> EffectPermitRequest {
    EffectPermitRequest {
        task_id: spec.task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        permit_id: permit.permit_id,
        permit_epoch: permit.permit_epoch,
        effect_seq,
        idempotency_key: IdempotencyKey::from_bytes(bytes(key_seed)),
        valid_until_ms: 9_999,
        requested_at_ms: 4_000,
    }
}

fn dispatch_request(
    spec: &AttemptSpec,
    permit: &PermitRecord,
    issued: &IssuedPermit,
) -> nlos_task::DispatchRequest {
    nlos_task::DispatchRequest {
        task_id: spec.task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        permit_id: permit.permit_id,
        permit_epoch: permit.permit_epoch,
        effect_permit_id: issued.effect_permit_id,
        dispatch_token: issued.one_shot_dispatch_token,
        dispatched_at_ms: 5_000,
    }
}

fn outcome_request(
    spec: &AttemptSpec,
    permit: &PermitRecord,
    effect_seq: u64,
    outcome: Outcome,
) -> OutcomeRequest {
    OutcomeRequest {
        task_id: spec.task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        permit_id: permit.permit_id,
        permit_epoch: permit.permit_epoch,
        effect_seq,
        outcome,
        recorded_at_ms: 6_000,
    }
}

fn no_effect_request(
    spec: &AttemptSpec,
    permit: &PermitRecord,
    effect_seq: u64,
    reason: NoEffectReason,
) -> NoEffectRequest {
    NoEffectRequest {
        task_id: spec.task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        permit_id: permit.permit_id,
        permit_epoch: permit.permit_epoch,
        effect_seq,
        reason,
        dispatch_token: None,
        recorded_at_ms: 6_000,
    }
}

fn finalize_v3(
    spec: &AttemptSpec,
    permit_id: CommitPermitId,
    proofs: Vec<RequiredSatisfaction>,
) -> FinalizeRequestV3 {
    FinalizeRequestV3 {
        base: nlos_task::FinalizeRequest {
            task_id: spec.task_id,
            attempt_id: spec.attempt_id,
            attempt_generation: spec.attempt_generation,
            permit_id,
            new_effect_history_root: [0u8; 32],
            new_retry_fence_epoch: 0,
            finalized_at_ms: 7_000,
        },
        required_satisfaction: proofs,
        fenced_participant_digest: [0xf1; 32],
    }
}

fn success_proof(
    authority: &SqliteTaskAuthority,
    permit_id: CommitPermitId,
    effect_seq: u64,
) -> RequiredSatisfaction {
    let slot = authority
        .inspect_effect_slot(permit_id, effect_seq)
        .expect("effect slot");
    let receipt = authority
        .inspect_effect_receipt(slot.effect_receipt_id.expect("effect receipt"))
        .expect("effect receipt record");
    RequiredSatisfaction {
        effect_seq,
        proof: RequiredSatisfactionProof::EffectClosedSuccess {
            success_assertion_digest: expected_success_assertion_digest(&slot, &receipt),
        },
    }
}

fn adopt_request(spec: &AttemptSpec, permit: &PermitRecord, key_seed: u8) -> AdoptionRequest {
    AdoptionRequest {
        task_id: spec.task_id,
        permit_id: permit.permit_id,
        permit_epoch: permit.permit_epoch,
        idempotency_key: IdempotencyKey::from_bytes(bytes(key_seed)),
        adopted_at_ms: 8_000,
    }
}

fn reconcile_request(
    spec: &AttemptSpec,
    permit: &PermitRecord,
    effect_seq: u64,
    adoption_receipt_id: ReceiptId,
    outcome: ReconcileOutcome,
    proof: [u8; 32],
) -> ReconcileRequest {
    ReconcileRequest {
        task_id: spec.task_id,
        permit_id: permit.permit_id,
        permit_epoch: permit.permit_epoch,
        effect_seq,
        adoption_receipt_id,
        outcome,
        closure_proof_digest: proof,
        reconciled_at_ms: 9_000,
    }
}

fn issued_permit(decision: PermitDecision) -> PermitRecord {
    match decision {
        PermitDecision::Issued(record) => *record,
        other => panic!("expected Issued, got {other:?}"),
    }
}

fn issued_effect_permit(decision: EffectPermitDecision) -> IssuedPermit {
    match decision {
        EffectPermitDecision::Issued(record) => *record,
        other @ EffectPermitDecision::Replayed(_) => panic!("expected Issued, got {other:?}"),
    }
}

fn adopted_record(decision: AdoptionReplay) -> nlos_task::AdoptionReceiptRecord {
    match decision {
        AdoptionReplay::Adopted(record) => *record,
        other @ AdoptionReplay::Replayed(_) => panic!("expected Adopted, got {other:?}"),
    }
}

fn reconciled_record(decision: ReconcileReplay) -> nlos_task::ReconciliationReceiptRecord {
    match decision {
        ReconcileReplay::Reconciled(record) => *record,
        other @ ReconcileReplay::Replayed(_) => panic!("expected Reconciled, got {other:?}"),
    }
}

/// Registers a task plus one attempt and issues its `CommitPermit` with the
/// given declared effect set: the shared committed prefix every scenario
/// starts from.
fn seed_winner(
    authority: &SqliteTaskAuthority,
    effects: Vec<PlannedEffect>,
) -> (AttemptSpec, PermitRecord) {
    authority.register_task(task_spec()).expect("register task");
    let spec = attempt_spec(0x0a, snapshot(0, 0));
    authority.register_attempt(spec).expect("register attempt");
    let permit = issued_permit(
        authority
            .request_commit_permit(permit_request(&spec, 0x01, effects))
            .expect("commit permit"),
    );
    (spec, permit)
}

/// Drives one slot through issue -> dispatch -> `EFFECT_UNKNOWN`: the v3
/// quarantine precondition.
fn dispatch_into_unknown(
    authority: &SqliteTaskAuthority,
    spec: &AttemptSpec,
    permit: &PermitRecord,
    effect_seq: u64,
    key_seed: u8,
) {
    let issued = issued_effect_permit(
        authority
            .request_effect_permit(effect_request(spec, permit, effect_seq, key_seed))
            .expect("issue"),
    );
    authority
        .consume_dispatch_token(dispatch_request(spec, permit, &issued))
        .expect("consume");
    authority
        .record_effect_outcome(outcome_request(
            spec,
            permit,
            effect_seq,
            Outcome::Unknown {
                uncertainty_digest: [0x99; 32],
            },
        ))
        .expect("register uncertainty");
}

/// Seeds the winner and puts slot 0 into `EFFECT_UNKNOWN`.
fn seed_unknown(
    authority: &SqliteTaskAuthority,
    effects: Vec<PlannedEffect>,
) -> (AttemptSpec, PermitRecord) {
    let (spec, permit) = seed_winner(authority, effects);
    dispatch_into_unknown(authority, &spec, &permit, 0, 0xe1);
    (spec, permit)
}

/// Quarantines the permit (slot 0 already `EFFECT_UNKNOWN`) and adopts it,
/// returning the durable adoption record.
fn quarantine_and_adopt(
    authority: &SqliteTaskAuthority,
    spec: &AttemptSpec,
    permit: &PermitRecord,
) -> nlos_task::AdoptionReceiptRecord {
    assert!(matches!(
        authority.finalize_commit_v3(finalize_v3(spec, permit.permit_id, Vec::new())),
        Err(TaskStoreError::Quarantined)
    ));
    adopted_record(
        authority
            .adopt_permit(adopt_request(spec, permit, 0xd1))
            .expect("adopt"),
    )
}

// ---------------------------------------------------------------------------
// kill-9 child-process harness (fault_injection.rs / effect_fault_injection.rs
// 范式: current_exe + env var + piped READY marker, never sleeps)
// ---------------------------------------------------------------------------

fn spawn_child(scenario: &str, path: &Path) -> Child {
    Command::new(std::env::current_exe().expect("current test executable"))
        .args(["--exact", "crash_child_helper", "--nocapture"])
        .env("NLOS_RECONCILE_CRASH_CHILD_SCENARIO", scenario)
        .env("NLOS_RECONCILE_CRASH_CHILD_DATABASE", path)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn crash child")
}

/// Blocks until the child prints its `READY` marker (pipe synchronization,
/// no sleeps); kills and reaps the child on timeout or early exit.
fn await_marker(child: &mut Child) -> String {
    let stdout = child.stdout.take().expect("piped stdout");
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        // The libtest harness prints its own banner lines before the helper's
        // marker; scan until the marker (or EOF when the child dies early).
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

/// Force-terminates the child and proves it did not exit cleanly.
fn kill_and_reap(child: &mut Child) {
    child.kill().expect("force-terminate child");
    let status = child.wait().expect("wait child");
    assert!(
        !status.success(),
        "killed child must not exit cleanly: {status}"
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

fn hex_decode16(text: &str) -> [u8; 16] {
    assert_eq!(text.len(), 32, "id hex is 16 bytes");
    let mut decoded = [0_u8; 16];
    for (index, slot) in decoded.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).expect("hex byte");
    }
    decoded
}

/// Parses the `READY <permit hex>` marker emitted by the child scenarios
/// that only need to hand the permit id back to the parent.
fn parse_permit_marker(marker: &str) -> CommitPermitId {
    let mut parts = marker.split_whitespace();
    assert_eq!(parts.next(), Some("READY"), "marker prefix");
    let permit_id = CommitPermitId::from_bytes(hex_decode16(parts.next().expect("permit hex")));
    assert!(parts.next().is_none(), "marker carries exactly one id");
    permit_id
}

/// Parses the `READY <permit hex> <adoption receipt hex>` marker emitted
/// by the child scenarios that commit an adoption.
fn parse_adoption_marker(marker: &str) -> (CommitPermitId, ReceiptId) {
    let mut parts = marker.split_whitespace();
    assert_eq!(parts.next(), Some("READY"), "marker prefix");
    let permit_id = CommitPermitId::from_bytes(hex_decode16(parts.next().expect("permit hex")));
    let adoption_id = ReceiptId::from_bytes(hex_decode16(parts.next().expect("adoption hex")));
    assert!(parts.next().is_none(), "marker carries exactly two ids");
    (permit_id, adoption_id)
}

fn announce(marker: &str) {
    println!("{marker}");
    std::io::stdout().flush().expect("flush marker");
}

/// WAL layout: 32-byte header, then frames of 24-byte header + page.
/// Returns the frame size and the indices of all commit frames (a frame
/// whose "database size in pages after commit" field is nonzero).
fn wal_commit_frames(wal: &[u8]) -> (usize, Vec<usize>) {
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
    (frame_size, commits)
}

/// Matrix row 1 fixture: a writer transaction has dirtied the v3 table
/// group (phantom quarantine tombstone, phantom history entry, phantom
/// sequence row, and the permit-state CAS dirt) but has not committed when
/// the process dies.
fn child_mid_v3_tx(path: &Path) -> ! {
    let authority = SqliteTaskAuthority::open(path).expect("open");
    let (_spec, permit) = seed_unknown(&authority, vec![planned(0, true)]);
    let raw = rusqlite::Connection::open(path).expect("raw connection");
    raw.execute_batch("BEGIN IMMEDIATE").expect("begin mid-tx");
    raw.execute(
        "UPDATE commit_permits SET permit_state = permit_state + 100",
        [],
    )
    .expect("mid-tx permit CAS dirt");
    let phantom_quarantine = format!(
        "INSERT INTO task_quarantine_receipts (
            receipt_id, task_id, task_generation, permit_id, permit_epoch,
            effect_set_root, outstanding_effect_quarantine_root,
            conflicting_target_digest, known_effect_receipts, unknown_slots,
            fenced_participant_digest, created_at_ms
         ) VALUES (
            X'99999999999999999999999999999999',
            X'01010101010101010101010101010101',
            X'0000000000000001',
            X'{}',
            X'0000000000000001',
            zeroblob(32), zeroblob(32), zeroblob(32),
            X'', zeroblob(8), zeroblob(32), 9000
         )",
        hex_encode(permit.permit_id.as_bytes())
    );
    raw.execute_batch(&phantom_quarantine)
        .expect("mid-tx phantom quarantine");
    raw.execute_batch(
        "INSERT INTO effect_history (
            task_id, effect_history_seq, logical_effect_id, retry_fence_epoch,
            action_proposal_digest, idempotency_identity_digest, operation_id,
            outcome, authoritative_effect_receipt_id, compensation_receipt_id,
            created_at_ms
         ) VALUES (
            X'01010101010101010101010101010101',
            X'0000000000000001',
            zeroblob(32), X'0000000000000000',
            zeroblob(32), zeroblob(32), NULL,
            0, zeroblob(16), NULL, 9000
         )",
    )
    .expect("mid-tx phantom history");
    raw.execute_batch(
        "INSERT INTO task_effect_sequences (task_id, effect_history_seq, adoption_epoch)
         VALUES (X'01010101010101010101010101010101', X'0000000000000001', X'0000000000000001')",
    )
    .expect("mid-tx phantom sequence");
    announce(&format!(
        "READY {}",
        hex_encode(permit.permit_id.as_bytes())
    ));
    let _keepers = (authority, raw);
    loop {
        std::thread::park();
    }
}

/// Matrix row 2 fixture: the complete committed v3 lifecycle — quarantine
/// tombstone, adoption, `EFFECT_CLOSED` + `CONFIRMED_NO_EFFECT` reconcile
/// closures (each with its history append), and a proved `COMMITTED`
/// finalize (finalize-proof row) — before the kill.
fn child_v3_commit_complete(path: &Path) -> ! {
    let authority = SqliteTaskAuthority::open(path).expect("open");
    let (spec, permit) = seed_winner(&authority, vec![planned(0, true), planned(1, false)]);
    dispatch_into_unknown(&authority, &spec, &permit, 0, 0xe1);
    dispatch_into_unknown(&authority, &spec, &permit, 1, 0xe2);
    let adoption = quarantine_and_adopt(&authority, &spec, &permit);
    reconciled_record(
        authority
            .reconcile_effect(reconcile_request(
                &spec,
                &permit,
                0,
                adoption.receipt_id,
                ReconcileOutcome::EffectClosed,
                [0xaa; 32],
            ))
            .expect("reconcile slot 0 to closed"),
    );
    reconciled_record(
        authority
            .reconcile_effect(reconcile_request(
                &spec,
                &permit,
                1,
                adoption.receipt_id,
                ReconcileOutcome::ConfirmedNoEffect,
                [0xcc; 32],
            ))
            .expect("reconcile slot 1 to confirmed-no-effect"),
    );
    match authority
        .finalize_commit_v3(finalize_v3(
            &spec,
            permit.permit_id,
            vec![success_proof(&authority, permit.permit_id, 0)],
        ))
        .expect("finalize commits")
    {
        FinalizeDecision::Committed(receipt) => {
            assert_eq!(receipt.outcome, ReceiptOutcome::Committed);
        }
        other @ FinalizeDecision::Replayed(_) => panic!("expected Committed, got {other:?}"),
    }
    announce(&format!(
        "READY {} {}",
        hex_encode(permit.permit_id.as_bytes()),
        hex_encode(adoption.receipt_id.as_bytes())
    ));
    let _keeper = authority;
    loop {
        std::thread::park();
    }
}

/// Matrix row 2b fixture: a committed `PARTIAL_EFFECT` finalize (required
/// slot 0 satisfied by proof, required slot 1 skipped, optional slot 2
/// closed) — fence advanced, `PARTIAL_EFFECT` history entry appended —
/// before the kill.
fn child_partial_commit(path: &Path) -> ! {
    let authority = SqliteTaskAuthority::open(path).expect("open");
    let (spec, permit) = seed_winner(
        &authority,
        vec![planned(0, true), planned(1, true), planned(2, false)],
    );
    for (seq, key) in [(0, 0xe1), (2, 0xe3)] {
        let issued = issued_effect_permit(
            authority
                .request_effect_permit(effect_request(&spec, &permit, seq, key))
                .expect("issue"),
        );
        authority
            .consume_dispatch_token(dispatch_request(&spec, &permit, &issued))
            .expect("consume");
        authority
            .record_effect_outcome(outcome_request(
                &spec,
                &permit,
                seq,
                Outcome::Closed {
                    authoritative_closure_digest: [0xaa; 32],
                },
            ))
            .expect("close slot");
    }
    authority
        .record_no_effect(no_effect_request(
            &spec,
            &permit,
            1,
            NoEffectReason::NotSelected,
        ))
        .expect("required slot 1 skipped");
    match authority
        .finalize_commit_v3(finalize_v3(
            &spec,
            permit.permit_id,
            vec![success_proof(&authority, permit.permit_id, 0)],
        ))
        .expect("partial finalize")
    {
        FinalizeDecision::Committed(receipt) => {
            assert_eq!(receipt.outcome, ReceiptOutcome::PartialEffect);
            assert_eq!(receipt.new_retry_fence_epoch, 1);
        }
        other @ FinalizeDecision::Replayed(_) => panic!("expected Committed, got {other:?}"),
    }
    announce(&format!(
        "READY {}",
        hex_encode(permit.permit_id.as_bytes())
    ));
    let _keeper = authority;
    loop {
        std::thread::park();
    }
}

/// Torn-tail fixture: every transaction through the quarantine tombstone
/// and the adoption is committed; the parent truncates the WAL inside the
/// adoption commit frame after the kill.
fn child_torn_wal_v3(path: &Path) -> ! {
    let authority = SqliteTaskAuthority::open(path).expect("open");
    let (spec, permit) = seed_unknown(&authority, vec![planned(0, true)]);
    let adoption = quarantine_and_adopt(&authority, &spec, &permit);
    announce(&format!(
        "READY {} {}",
        hex_encode(permit.permit_id.as_bytes()),
        hex_encode(adoption.receipt_id.as_bytes())
    ));
    let _keeper = authority;
    loop {
        std::thread::park();
    }
}

/// Child entry point. Runs only when spawned by a parent test with the
/// scenario environment set; a no-op in the normal test run.
#[test]
fn crash_child_helper() {
    let (Ok(scenario), Ok(path)) = (
        std::env::var("NLOS_RECONCILE_CRASH_CHILD_SCENARIO"),
        std::env::var("NLOS_RECONCILE_CRASH_CHILD_DATABASE"),
    ) else {
        return;
    };
    let path = PathBuf::from(path);
    match scenario.as_str() {
        "mid-v3-tx" => child_mid_v3_tx(&path),
        "v3-commit-complete" => child_v3_commit_complete(&path),
        "partial-commit" => child_partial_commit(&path),
        "torn-wal-v3" => child_torn_wal_v3(&path),
        other => panic!("unknown crash child scenario {other}"),
    }
}

// ---------------------------------------------------------------------------
// 矩阵行 1: kill-9 mid-transaction on the v3 table group leaves no half state
// ---------------------------------------------------------------------------

/// kill-9 中断 v3 表组写事务：子进程在 `BEGIN IMMEDIATE` 未提交（已写入
/// 幻影 quarantine tombstone、幻影 history 条目、幻影 sequence 行，并弄脏
/// `commit_permits.permit_state` CAS）时被强杀；重开后中断事务完全回滚
/// —— v3 六表全空、permit 回到已提交的 `ISSUED`、slot 保持
/// `EFFECT_UNKNOWN`、无半截 quarantine/adoption/reconcile/history 状态；
/// 随后同一 finalize 真实产生 quarantine tombstone 且重放观察到同一
/// 类型化拒绝（确定性派生 ID 一致）。
#[test]
fn fault_kill9_mid_v3_transaction_leaves_no_half_state() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestDatabase::new("kill9-mid-v3-tx");
    let mut child = spawn_child("mid-v3-tx", &database.path);
    let marker = await_marker(&mut child);
    let permit_id = parse_permit_marker(&marker);
    kill_and_reap(&mut child);

    // Nothing uncommitted may survive: the v3 table group is empty and the
    // permit CAS dirt rolled back to the committed prefix.
    assert_v3_tables_empty(&database.path);

    let authority = SqliteTaskAuthority::open(&database.path).expect("reopen after kill");
    let spec = attempt_spec(0x0a, snapshot(0, 0));
    let permit = authority
        .inspect_permit(task_id(), permit_id)
        .expect("permit");
    assert_eq!(
        permit.state,
        PermitState::Issued,
        "mid-transaction quarantine CAS dirt must be rolled back"
    );
    let slot = authority.inspect_effect_slot(permit_id, 0).expect("slot");
    assert_eq!(slot.state, SlotState::EffectUnknown);
    assert_eq!(slot.state_seq, 3);
    assert!(
        authority
            .inspect_quarantine_receipt(permit_id)
            .expect("quarantine lookup")
            .is_none(),
        "phantom tombstone must not be durable"
    );
    assert!(
        authority
            .list_effect_history(task_id())
            .expect("history")
            .is_empty(),
        "phantom history entry must not be durable"
    );

    // The interrupted decision is redoable from the committed prefix: the
    // same finalize now produces the real tombstone, and its replay
    // observes the same typed refusal (deterministic derived identity).
    assert!(matches!(
        authority.finalize_commit_v3(finalize_v3(&spec, permit_id, Vec::new())),
        Err(TaskStoreError::Quarantined)
    ));
    let quarantine = authority
        .inspect_quarantine_receipt(permit_id)
        .expect("quarantine receipt")
        .expect("tombstone persisted");
    assert_eq!(quarantine.unknown_slots, vec![0]);
    assert_eq!(
        authority
            .inspect_permit(task_id(), permit_id)
            .expect("permit")
            .state,
        PermitState::Quarantined
    );
    assert!(matches!(
        authority.finalize_commit_v3(finalize_v3(&spec, permit_id, Vec::new())),
        Err(TaskStoreError::Quarantined)
    ));
    assert_eq!(raw_count(&database.path, "task_quarantine_receipts"), 1);
    assert_integrity(&database.path);
}

// ---------------------------------------------------------------------------
// 矩阵行 2: kill-9 after commit — the committed v3 lifecycle survives
// ---------------------------------------------------------------------------

/// Asserts the durable bit-for-bit v3 state of the committed lifecycle:
/// tombstone, adoption, both reconcile receipts, both history entries
/// (gapless), the finalize proof, and the commit receipt.
fn assert_v3_lifecycle_durable(
    authority: &SqliteTaskAuthority,
    database: &TestDatabase,
    permit_id: CommitPermitId,
    adoption_id: ReceiptId,
) {
    assert_eq!(raw_count(&database.path, "task_quarantine_receipts"), 1);
    assert_eq!(raw_count(&database.path, "task_adoption_receipts"), 1);
    assert_eq!(raw_count(&database.path, "task_reconcile_receipts"), 2);
    assert_eq!(raw_count(&database.path, "effect_history"), 2);
    assert_eq!(raw_count(&database.path, "task_finalize_proofs"), 1);
    assert_eq!(raw_count(&database.path, "task_receipts"), 1);

    let quarantine = authority
        .inspect_quarantine_receipt(permit_id)
        .expect("quarantine receipt")
        .expect("tombstone durable");
    assert_eq!(quarantine.unknown_slots, vec![0, 1]);
    assert_eq!(quarantine.fenced_participant_digest, [0xf1; 32]);

    let adoption = authority
        .inspect_adoption_receipt(task_id(), adoption_id)
        .expect("adoption durable");
    assert_eq!(adoption.original_permit_id, permit_id);
    assert_eq!(adoption.adoption_epoch, 1);

    let closed = authority
        .inspect_reconcile_receipt(permit_id, 0)
        .expect("reconcile receipt 0")
        .expect("persisted");
    assert_eq!(closed.outcome, ReconcileOutcome::EffectClosed);
    assert_eq!(closed.closure_proof_digest, [0xaa; 32]);
    assert!(closed.effect_receipt_id.is_some());
    let no_effect = authority
        .inspect_reconcile_receipt(permit_id, 1)
        .expect("reconcile receipt 1")
        .expect("persisted");
    assert_eq!(no_effect.outcome, ReconcileOutcome::ConfirmedNoEffect);
    assert_eq!(no_effect.closure_proof_digest, [0xcc; 32]);

    let history = authority.list_effect_history(task_id()).expect("history");
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].effect_history_seq, 1);
    assert_eq!(history[1].effect_history_seq, 2);
    assert_eq!(
        history[0].outcome,
        nlos_task::EffectHistoryOutcome::EffectClosed
    );
    assert_eq!(
        history[1].outcome,
        nlos_task::EffectHistoryOutcome::ConfirmedNoEffect
    );

    let permit = authority
        .inspect_permit(task_id(), permit_id)
        .expect("permit");
    assert_eq!(permit.state, PermitState::Closed);
    let head = authority.inspect_task(task_id()).expect("head");
    assert_eq!(head.head_commit_seq, 1);
    assert_eq!(head.retry_fence_epoch, 0);
    assert_eq!(head.active_permit, None);
}

/// commit 后崩溃：子进程在 v3 全生命周期（quarantine tombstone →
/// adoption → slot0 reconcile `EFFECT_CLOSED` + slot1 reconcile
/// `CONFIRMED_NO_EFFECT`（各含同事务 history 追加）→ proved `COMMITTED`
/// finalize 含 finalize-proof 行）全部提交返回后被强杀；重开后全部逐位
/// 保留；finalize/adoption/reconcile 重放返回原结果、异 proof
/// `HistoryConflict`；history 不被双重追加（seq 保持无洞）。
#[test]
fn fault_kill9_after_v3_commit_preserves_everything() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestDatabase::new("kill9-v3-commit");
    let mut child = spawn_child("v3-commit-complete", &database.path);
    let marker = await_marker(&mut child);
    let (permit_id, adoption_id) = parse_adoption_marker(&marker);
    kill_and_reap(&mut child);

    let authority = SqliteTaskAuthority::open(&database.path).expect("reopen after kill");
    let spec = attempt_spec(0x0a, snapshot(0, 0));
    let permit = authority
        .inspect_permit(task_id(), permit_id)
        .expect("permit");
    assert_v3_lifecycle_durable(&authority, &database, permit_id, adoption_id);

    let attempt = authority
        .inspect_attempt(task_id(), spec.attempt_id)
        .expect("attempt");
    assert_eq!(attempt.state, AttemptState::Committed);
    let commit_receipt = authority
        .inspect_receipt(task_id(), attempt.receipt_id.expect("commit receipt id"))
        .expect("commit receipt");
    assert_eq!(commit_receipt.outcome, ReceiptOutcome::Committed);
    assert_eq!(commit_receipt.new_head_commit_seq, 1);

    // Replays after the crash return the original durable decisions.
    match authority
        .finalize_commit_v3(finalize_v3(
            &spec,
            permit_id,
            vec![success_proof(&authority, permit_id, 0)],
        ))
        .expect("replay finalize")
    {
        FinalizeDecision::Replayed(original) => assert_eq!(*original, commit_receipt),
        other @ FinalizeDecision::Committed(_) => panic!("expected Replayed, got {other:?}"),
    }
    assert!(matches!(
        authority.adopt_permit(adopt_request(&spec, &permit, 0xd1)),
        Ok(AdoptionReplay::Replayed(_))
    ));
    assert!(matches!(
        authority.reconcile_effect(reconcile_request(
            &spec,
            &permit,
            0,
            adoption_id,
            ReconcileOutcome::EffectClosed,
            [0xaa; 32],
        )),
        Ok(ReconcileReplay::Replayed(_))
    ));
    assert!(matches!(
        authority.reconcile_effect(reconcile_request(
            &spec,
            &permit,
            0,
            adoption_id,
            ReconcileOutcome::EffectClosed,
            [0xab; 32],
        )),
        Err(TaskStoreError::HistoryConflict)
    ));
    // No replay double-appends history: the sequence stays gapless at 2.
    let history = authority.list_effect_history(task_id()).expect("history");
    assert_eq!(history.len(), 2);
    assert_eq!(history[1].effect_history_seq, 2);
    assert_eq!(raw_count(&database.path, "effect_history"), 2);
    assert_eq!(raw_count(&database.path, "task_finalize_proofs"), 1);
    assert_integrity(&database.path);
}

// ---------------------------------------------------------------------------
// 矩阵行 2b: kill-9 after a PARTIAL_EFFECT finalize — fence + history survive
// ---------------------------------------------------------------------------

/// commit 后崩溃（`PARTIAL_EFFECT`）：子进程在 required slot0 证明满足、
/// required slot1 跳过、可选 slot2 闭合的 `PARTIAL_EFFECT` finalize
/// （fence 0→1、`PARTIAL_EFFECT` history 条目追加、finalize-proof 行）
/// 提交后被强杀；重开后 receipt/head/fence/history 逐位保留；同 bytes
/// finalize 重放返回原 receipt，fence 不再 +1，history 不双重追加
/// （seq 保持 1..=3 无洞）。
#[test]
fn fault_kill9_after_partial_effect_finalize_preserves_fence_and_history() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestDatabase::new("kill9-partial-commit");
    let mut child = spawn_child("partial-commit", &database.path);
    let marker = await_marker(&mut child);
    let permit_id = parse_permit_marker(&marker);
    kill_and_reap(&mut child);

    assert_eq!(raw_count(&database.path, "effect_history"), 3);
    assert_eq!(raw_count(&database.path, "task_finalize_proofs"), 1);
    assert_eq!(raw_count(&database.path, "task_receipts"), 1);

    let authority = SqliteTaskAuthority::open(&database.path).expect("reopen after kill");
    let spec = attempt_spec(0x0a, snapshot(0, 0));
    let attempt = authority
        .inspect_attempt(task_id(), spec.attempt_id)
        .expect("attempt");
    assert_eq!(attempt.state, AttemptState::Committed);
    let receipt = authority
        .inspect_receipt(task_id(), attempt.receipt_id.expect("receipt id"))
        .expect("receipt");
    assert_eq!(receipt.outcome, ReceiptOutcome::PartialEffect);
    assert_eq!(receipt.prior_retry_fence_epoch, 0);
    assert_eq!(receipt.new_retry_fence_epoch, 1);
    assert_eq!(receipt.new_head_commit_seq, 1);

    let head = authority.inspect_task(task_id()).expect("head");
    assert_eq!(head.head_commit_seq, 1);
    assert_eq!(head.retry_fence_epoch, 1);
    assert_eq!(
        head.head_effect_history_root,
        authority
            .compute_effect_history_root(task_id())
            .expect("root"),
        "the committed head root equals the recomputed history root"
    );

    let history = authority.list_effect_history(task_id()).expect("history");
    assert_eq!(history.len(), 3);
    for (index, entry) in history.iter().enumerate() {
        assert_eq!(
            entry.effect_history_seq,
            u64::try_from(index).expect("index") + 1,
            "history sequence stays gapless"
        );
    }
    let partial = &history[2];
    assert_eq!(
        partial.outcome,
        nlos_task::EffectHistoryOutcome::PartialEffect
    );
    assert_eq!(partial.retry_fence_epoch, 1);
    assert_eq!(partial.logical_effect_id, descriptor(1).logical_effect_id());

    // Replay of the same finalize returns the original receipt; the fence
    // is not re-incremented and no history entry is double-appended.
    match authority
        .finalize_commit_v3(finalize_v3(
            &spec,
            permit_id,
            vec![success_proof(&authority, permit_id, 0)],
        ))
        .expect("replay partial finalize")
    {
        FinalizeDecision::Replayed(original) => assert_eq!(*original, receipt),
        other @ FinalizeDecision::Committed(_) => panic!("expected Replayed, got {other:?}"),
    }
    let head = authority
        .inspect_task(task_id())
        .expect("head after replay");
    assert_eq!(
        head.retry_fence_epoch, 1,
        "replay must not re-advance the fence"
    );
    assert_eq!(head.head_commit_seq, 1);
    assert_eq!(
        authority
            .list_effect_history(task_id())
            .expect("history")
            .len(),
        3,
        "replay must not double-append history"
    );
    assert_eq!(raw_count(&database.path, "task_finalize_proofs"), 1);
    assert_integrity(&database.path);
}

// ---------------------------------------------------------------------------
// 矩阵行 3: hard I/O error on quarantine / reconcile writes fails closed
// ---------------------------------------------------------------------------

/// Phase A invariant: a failed quarantine write leaves no half state —
/// no tombstone, permit `ISSUED`, slot `EFFECT_UNKNOWN`, epoch unmoved.
fn assert_no_half_quarantine(
    authority: &SqliteTaskAuthority,
    database: &TestDatabase,
    permit_id: CommitPermitId,
    control_before: u64,
) {
    assert_eq!(raw_count(&database.path, "task_quarantine_receipts"), 0);
    assert_eq!(
        authority
            .inspect_permit(task_id(), permit_id)
            .expect("permit")
            .state,
        PermitState::Issued
    );
    assert_eq!(
        authority
            .inspect_effect_slot(permit_id, 0)
            .expect("slot")
            .state,
        SlotState::EffectUnknown
    );
    assert_eq!(
        authority
            .inspect_task(task_id())
            .expect("head")
            .control_epoch,
        control_before,
        "failed quarantine must not advance the control epoch"
    );
}

/// Phase B invariant: a failed reconcile write leaves no half state —
/// slot `EFFECT_UNKNOWN`, no reconcile/history rows, permit `QUARANTINED`.
fn assert_no_half_reconcile(
    authority: &SqliteTaskAuthority,
    database: &TestDatabase,
    permit_id: CommitPermitId,
    control_before: u64,
) {
    assert_eq!(
        authority
            .inspect_effect_slot(permit_id, 0)
            .expect("slot")
            .state,
        SlotState::EffectUnknown
    );
    assert_eq!(raw_count(&database.path, "task_reconcile_receipts"), 0);
    assert_eq!(raw_count(&database.path, "effect_history"), 0);
    assert_eq!(
        authority
            .inspect_permit(task_id(), permit_id)
            .expect("permit")
            .state,
        PermitState::Quarantined
    );
    assert_eq!(
        authority
            .inspect_task(task_id())
            .expect("head")
            .control_epoch,
        control_before
    );
}

/// 写入硬 I/O 错误（quarantine 与 reconcile 事务）：
/// `FailWritesAfter { 0, IoErr }` 下（a）产生 tombstone 的
/// `finalize_commit_v3` 与（b）reconcile 事务（slot CAS + 闭合 receipt +
/// reconcile receipt + history 追加同事务）都必须以
/// `TaskStoreError::Sqlite` 显式失败（错误链含 I/O 条件），不返回假成功；
/// 无半截状态（无 quarantine/reconcile/history 行、permit 保持
/// `ISSUED`/`QUARANTINED`、slot 保持 `EFFECT_UNKNOWN`、`control_epoch`
/// 不动）；disarm 后同一操作成功。
#[test]
fn fault_io_error_on_quarantine_and_reconcile_writes_fails_closed() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestDatabase::new("ioerr-v3");
    let authority = open_shim(&database.path);
    let (spec, permit) = seed_unknown(&authority, vec![planned(0, true)]);
    let control_before = authority
        .inspect_task(task_id())
        .expect("head")
        .control_epoch;

    // Phase A: the finalize transaction that would write the tombstone
    // dies on its first write.
    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::IoErr,
    });
    let error = authority
        .finalize_commit_v3(finalize_v3(&spec, permit.permit_id, Vec::new()))
        .expect_err("quarantine write must fail under injected I/O error");
    assert_sqlite_error_chain(&error, &["i/o", "ioerr"]);
    assert!(nlos_store_fault::writes_observed() > 0);
    assert_no_half_quarantine(&authority, &database, permit.permit_id, control_before);

    nlos_store_fault::disarm();
    let adoption = quarantine_and_adopt(&authority, &spec, &permit);
    let control_after_adoption = authority
        .inspect_task(task_id())
        .expect("head")
        .control_epoch;

    // Phase B: the reconcile transaction dies on its first write.
    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::IoErr,
    });
    let error = authority
        .reconcile_effect(reconcile_request(
            &spec,
            &permit,
            0,
            adoption.receipt_id,
            ReconcileOutcome::EffectClosed,
            [0xaa; 32],
        ))
        .expect_err("reconcile write must fail under injected I/O error");
    assert_sqlite_error_chain(&error, &["i/o", "ioerr"]);
    assert_no_half_reconcile(
        &authority,
        &database,
        permit.permit_id,
        control_after_adoption,
    );

    nlos_store_fault::disarm();
    let record = reconciled_record(
        authority
            .reconcile_effect(reconcile_request(
                &spec,
                &permit,
                0,
                adoption.receipt_id,
                ReconcileOutcome::EffectClosed,
                [0xaa; 32],
            ))
            .expect("reconcile succeeds after disarm"),
    );
    assert_eq!(record.outcome, ReconcileOutcome::EffectClosed);
    assert_eq!(
        authority
            .inspect_effect_slot(permit.permit_id, 0)
            .expect("slot")
            .state,
        SlotState::EffectClosed
    );
    assert_eq!(raw_count(&database.path, "effect_history"), 1);
    assert_integrity(&database.path);
}

// ---------------------------------------------------------------------------
// 矩阵行 4: disk-full (ENOSPC) on adoption / finalize-proof writes fails closed
// ---------------------------------------------------------------------------

/// disk-full（adoption 与 finalize-proofs 事务）：`FailWritesAfter { 0,
/// Full }` 下（a）adoption 写事务（receipt + sequence epoch 推进 +
/// `control_epoch` 同事务）与（b）proved `COMMITTED` finalize 写事务
/// （commit receipt + finalize-proof + permit 关闭 + attempt 终态 + head
/// 推进同事务）都必须以 `SQLITE_FULL` 显式失败（错误链含 full）；无半截
/// 状态（无 adoption/finalize-proof/receipt 行、sequence 不推进、head 不
/// 前进、permit 保持 `ISSUED`）；disarm 后同一操作成功。
#[test]
fn fault_enospc_on_adoption_and_finalize_proof_writes_fails_closed() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestDatabase::new("full-v3");
    let authority = open_shim(&database.path);
    let (spec, permit) = seed_unknown(&authority, vec![planned(0, true)]);
    authority
        .finalize_commit_v3(finalize_v3(&spec, permit.permit_id, Vec::new()))
        .expect_err("quarantine");

    // Phase A: the adoption write dies on disk-full.
    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::Full,
    });
    let error = authority
        .adopt_permit(adopt_request(&spec, &permit, 0xd1))
        .expect_err("adoption must fail under injected disk-full");
    assert_sqlite_error_chain(&error, &["full"]);
    assert!(nlos_store_fault::writes_observed() > 0);

    // No half-adoption: no receipt row, the sequence row never advanced,
    // the permit stays QUARANTINED.
    assert_eq!(raw_count(&database.path, "task_adoption_receipts"), 0);
    assert_eq!(raw_count(&database.path, "task_effect_sequences"), 0);
    assert_eq!(
        authority
            .inspect_permit(task_id(), permit.permit_id)
            .expect("permit")
            .state,
        PermitState::Quarantined
    );

    nlos_store_fault::disarm();
    let adoption = adopted_record(
        authority
            .adopt_permit(adopt_request(&spec, &permit, 0xd1))
            .expect("adoption succeeds after disarm"),
    );
    reconciled_record(
        authority
            .reconcile_effect(reconcile_request(
                &spec,
                &permit,
                0,
                adoption.receipt_id,
                ReconcileOutcome::EffectClosed,
                [0xaa; 32],
            ))
            .expect("reconcile to closed"),
    );

    // Phase B: the proved COMMITTED finalize (receipt + finalize-proof +
    // permit close + attempt transition + head advance in one transaction)
    // dies on disk-full.
    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::Full,
    });
    let error = authority
        .finalize_commit_v3(finalize_v3(
            &spec,
            permit.permit_id,
            vec![success_proof(&authority, permit.permit_id, 0)],
        ))
        .expect_err("finalize must fail under injected disk-full");
    assert_sqlite_error_chain(&error, &["full"]);

    // No half-commit: no receipt, no finalize-proof row, head unmoved,
    // permit still ISSUED, attempt still COMMIT_PERMITTED.
    assert_eq!(raw_count(&database.path, "task_receipts"), 0);
    assert_eq!(raw_count(&database.path, "task_finalize_proofs"), 0);
    let head = authority.inspect_task(task_id()).expect("head");
    assert_eq!(head.head_commit_seq, 0);
    assert_eq!(head.active_permit, Some(permit.permit_id));
    assert_eq!(
        authority
            .inspect_permit(task_id(), permit.permit_id)
            .expect("permit")
            .state,
        PermitState::Issued
    );
    assert_eq!(
        authority
            .inspect_attempt(task_id(), spec.attempt_id)
            .expect("attempt")
            .state,
        AttemptState::CommitPermitted
    );

    nlos_store_fault::disarm();
    match authority
        .finalize_commit_v3(finalize_v3(
            &spec,
            permit.permit_id,
            vec![success_proof(&authority, permit.permit_id, 0)],
        ))
        .expect("finalize succeeds after disarm")
    {
        FinalizeDecision::Committed(receipt) => {
            assert_eq!(receipt.outcome, ReceiptOutcome::Committed);
            assert_eq!(receipt.new_head_commit_seq, 1);
        }
        other @ FinalizeDecision::Replayed(_) => panic!("expected Committed, got {other:?}"),
    }
    assert_eq!(raw_count(&database.path, "task_finalize_proofs"), 1);
    assert_eq!(
        authority
            .inspect_task(task_id())
            .expect("head")
            .head_commit_seq,
        1
    );
    assert_integrity(&database.path);
}

// ---------------------------------------------------------------------------
// 矩阵行 5: silent write loss / torn tail fabricates no phantom v3 facts
// ---------------------------------------------------------------------------

/// 静默丢写/短写（v3 表组）：
/// - Phase A（断电模型）：`PowerLossAfter { 0 }` 下 reconcile 事务“报告
///   成功”但写入从未落盘；重开后幻影 reconcile receipt/history 条目不得
///   冒充已提交事实（slot 回到 `EFFECT_UNKNOWN`、无 reconcile 行、无
///   history 行、permit 保持 `QUARANTINED`、`control_epoch` 不前进），
///   同一请求可重做且确定性派生的 reconcile receipt id 逐位相同、重开后
///   真实持久。
/// - Phase B（短写/撕裂尾部）：子进程提交到 quarantine+adoption 后被杀，
///   父进程把 WAL 截断到最后一个 commit 帧（adoption 事务）的一半；重开
///   后 adoption 提交整体隐藏（幻影 adoption 不可见），此前合法前缀
///   （quarantine tombstone、permit `QUARANTINED`）完整保留，同一幂等
///   key 重做 adoption 且确定性派生 receipt id 逐位相同。
#[test]
fn fault_silent_write_loss_and_torn_tail_hide_phantom_v3_facts() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    power_loss_drops_reconcile_and_redo_is_durable();
    torn_wal_tail_hides_adoption_and_redo_is_durable();
}

/// Phase A: a silently dropped reconcile commit is invisible after
/// recovery and the lost decision is redoable with the same deterministic
/// identity.
fn power_loss_drops_reconcile_and_redo_is_durable() {
    let database = TestDatabase::new("power-loss-reconcile");
    let authority = open_shim(&database.path);
    let (spec, permit) = seed_unknown(&authority, vec![planned(0, true)]);
    let adoption = quarantine_and_adopt(&authority, &spec, &permit);
    let control_before = authority
        .inspect_task(task_id())
        .expect("head")
        .control_epoch;

    nlos_store_fault::arm(FaultMode::PowerLossAfter { remaining: 0 });
    let phantom = reconciled_record(
        authority
            .reconcile_effect(reconcile_request(
                &spec,
                &permit,
                0,
                adoption.receipt_id,
                ReconcileOutcome::EffectClosed,
                [0xaa; 32],
            ))
            .expect("power loss drops writes silently"),
    );
    nlos_store_fault::disarm();

    // The surviving connection keeps a wal-index that references frames the
    // disk never saw; it must die first (as a real power loss would kill
    // it) so recovery sees durable bytes alone (fault_crash.rs precedent).
    drop(authority);

    let recovered = SqliteTaskAuthority::open(&database.path).expect("reopen after power loss");
    let slot = recovered
        .inspect_effect_slot(permit.permit_id, 0)
        .expect("slot");
    assert_eq!(
        slot.state,
        SlotState::EffectUnknown,
        "silently dropped reconcile must not move the slot"
    );
    assert!(
        recovered
            .inspect_reconcile_receipt(permit.permit_id, 0)
            .expect("reconcile lookup")
            .is_none(),
        "phantom reconcile receipt must not fabricate a committed fact"
    );
    assert!(
        recovered
            .list_effect_history(task_id())
            .expect("history")
            .is_empty(),
        "phantom history entry must not be durable"
    );
    assert_eq!(
        recovered
            .inspect_permit(task_id(), permit.permit_id)
            .expect("permit")
            .state,
        PermitState::Quarantined,
        "the durable tombstone survives; the phantom lift does not"
    );
    assert_eq!(
        recovered
            .inspect_task(task_id())
            .expect("head")
            .control_epoch,
        control_before,
        "silently dropped reconcile must not advance the control epoch"
    );
    assert_eq!(raw_count(&database.path, "task_reconcile_receipts"), 0);
    assert_eq!(raw_count(&database.path, "effect_history"), 0);
    assert_integrity(&database.path);

    // The lost decision is redoable: the deterministic reconcile receipt
    // id is reused, and this time genuinely durable.
    let redone = reconciled_record(
        recovered
            .reconcile_effect(reconcile_request(
                &spec,
                &permit,
                0,
                adoption.receipt_id,
                ReconcileOutcome::EffectClosed,
                [0xaa; 32],
            ))
            .expect("redo after power loss"),
    );
    assert_eq!(redone.receipt_id, phantom.receipt_id);
    drop(recovered);
    let verified = SqliteTaskAuthority::open(&database.path).expect("reopen after redo");
    let slot = verified
        .inspect_effect_slot(permit.permit_id, 0)
        .expect("slot");
    assert_eq!(slot.state, SlotState::EffectClosed);
    assert_eq!(
        verified
            .list_effect_history(task_id())
            .expect("history")
            .len(),
        1
    );
    assert_integrity(&database.path);
    drop(verified);
    drop(database);
}

/// Phase B: truncating the WAL to half of the adoption transaction's
/// commit frame hides the adoption entirely while keeping the committed
/// quarantine prefix; the redo derives the same adoption receipt id.
fn torn_wal_tail_hides_adoption_and_redo_is_durable() {
    let database = TestDatabase::new("torn-tail-adoption");
    let mut child = spawn_child("torn-wal-v3", &database.path);
    let marker = await_marker(&mut child);
    let (permit_id, phantom_adoption_id) = parse_adoption_marker(&marker);
    kill_and_reap(&mut child);

    let wal_path = TestDatabase::sibling(&database.path, "-wal");
    let mut wal = fs::read(&wal_path).expect("read wal");
    let (frame_size, commits) = wal_commit_frames(&wal);
    assert!(
        commits.len() >= 8,
        "fixture must contain schema + register + attempt + permit + issue + consume + unknown + quarantine + adoption commits"
    );
    let last_commit = *commits.last().expect("commits exist");
    let half_frame_cut = 32 + last_commit * frame_size + frame_size / 2;
    wal.truncate(half_frame_cut);
    fs::write(&wal_path, &wal).expect("write truncated wal");
    fs::remove_file(TestDatabase::sibling(&database.path, "-shm")).expect("remove stale shm");

    // Recovery drops the torn adoption transaction entirely; the committed
    // quarantine prefix (tombstone, QUARANTINED permit, UNKNOWN slot) is
    // intact.
    let recovered = SqliteTaskAuthority::open(&database.path).expect("reopen after truncation");
    let spec = attempt_spec(0x0a, snapshot(0, 0));
    let permit = recovered
        .inspect_permit(task_id(), permit_id)
        .expect("permit");
    assert_eq!(
        permit.state,
        PermitState::Quarantined,
        "the committed quarantine prefix survives the torn tail"
    );
    let quarantine = recovered
        .inspect_quarantine_receipt(permit_id)
        .expect("quarantine receipt")
        .expect("tombstone durable");
    assert_eq!(quarantine.unknown_slots, vec![0]);
    assert_eq!(
        recovered
            .inspect_effect_slot(permit_id, 0)
            .expect("slot")
            .state,
        SlotState::EffectUnknown
    );
    assert!(
        matches!(
            recovered.inspect_adoption_receipt(task_id(), phantom_adoption_id),
            Err(TaskStoreError::ReceiptNotFound)
        ),
        "torn tail must not fabricate an adoption"
    );
    assert_eq!(raw_count(&database.path, "task_adoption_receipts"), 0);
    assert_eq!(raw_count(&database.path, "task_quarantine_receipts"), 1);
    assert_integrity(&database.path);

    // The hidden adoption is redoable with the same idempotency key; the
    // deterministic derived receipt id is identical, proving no
    // conflicting half-record was left behind.
    let redone = adopted_record(
        recovered
            .adopt_permit(adopt_request(&spec, &permit, 0xd1))
            .expect("redo adoption after torn tail"),
    );
    assert_eq!(redone.receipt_id, phantom_adoption_id);
    assert_eq!(redone.adoption_epoch, 1);
    drop(recovered);
    drop(database);
}

// ---------------------------------------------------------------------------
// 矩阵行 6: after the fault clears, the v3 flow continues from the prefix
// ---------------------------------------------------------------------------

/// Runs a full second competition on the advanced head: attempt B binds the
/// recomputed root/fence, declares a genuinely new business effect
/// (`stable_action_slot` 1 — slot 0's `LogicalEffectId` is `EFFECT_CLOSED`
/// in the durable cross-attempt history, `[TASK-EFFECT-ID-001]`), then
/// issues/consumes/closes and finalizes.
fn run_second_competition(authority: &SqliteTaskAuthority) -> PermitRecord {
    let root_after_a = authority
        .compute_effect_history_root(task_id())
        .expect("root after A");
    let mut bundle_b = snapshot(1, 0);
    bundle_b.snapshot_id = TaskSnapshotId::from_bytes(bytes(0x1b));
    bundle_b.effect_history_root = root_after_a;
    let spec_b = attempt_spec(0x0b, bundle_b);
    authority.register_attempt(spec_b).expect("register B");
    let permit_b = issued_permit(
        authority
            .request_commit_permit(permit_request(&spec_b, 0x02, vec![planned(1, true)]))
            .expect("permit B"),
    );
    assert_eq!(permit_b.permit_epoch, 2);
    assert_eq!(permit_b.expected_head_commit_seq, 1);
    assert_eq!(permit_b.expected_effect_history_root, root_after_a);
    let issued_b = issued_effect_permit(
        authority
            .request_effect_permit(effect_request(&spec_b, &permit_b, 0, 0xe2))
            .expect("issue B"),
    );
    authority
        .consume_dispatch_token(dispatch_request(&spec_b, &permit_b, &issued_b))
        .expect("consume B");
    authority
        .record_effect_outcome(outcome_request(
            &spec_b,
            &permit_b,
            0,
            Outcome::Closed {
                authoritative_closure_digest: [0xbb; 32],
            },
        ))
        .expect("close B");
    match authority
        .finalize_commit_v3(finalize_v3(
            &spec_b,
            permit_b.permit_id,
            vec![success_proof(authority, permit_b.permit_id, 0)],
        ))
        .expect("finalize B")
    {
        FinalizeDecision::Committed(receipt) => assert_eq!(receipt.new_head_commit_seq, 2),
        other @ FinalizeDecision::Replayed(_) => panic!("expected Committed, got {other:?}"),
    }
    permit_b
}

/// 故障解除后：reconcile 写事务在 `FailWritesAfter { 0, Full }` 下失败
/// 后 disarm，**同一 authority 实例**继续读写——已提交前缀（slot
/// `EFFECT_UNKNOWN`、permit `QUARANTINED`、tombstone、adoption、
/// `control_epoch`）与故障前逐位一致；reconcile 重试最终闭合
/// （`EFFECT_CLOSED` + history 追加 + tombstone 解除），proved
/// `COMMITTED` finalize 成功；新竞争（第二张 permit 绑定推进后的
/// head/root/fence）再走完整 v3 finalize 成功；完整重开后全部状态可
/// 恢复。
#[test]
fn fault_after_disarm_reconcile_retry_closes_from_committed_prefix() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestDatabase::new("disarm-v3-continue");
    let authority = open_shim(&database.path);
    let (spec_a, permit_a) = seed_unknown(&authority, vec![planned(0, true)]);
    let adoption = quarantine_and_adopt(&authority, &spec_a, &permit_a);
    let control_before = authority
        .inspect_task(task_id())
        .expect("head")
        .control_epoch;

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::Full,
    });
    authority
        .reconcile_effect(reconcile_request(
            &spec_a,
            &permit_a,
            0,
            adoption.receipt_id,
            ReconcileOutcome::EffectClosed,
            [0xaa; 32],
        ))
        .expect_err("reconcile must fail while the fault is armed");

    // The committed prefix observed through the same authority is
    // identical to the pre-fault state.
    assert_no_half_reconcile(&authority, &database, permit_a.permit_id, control_before);

    nlos_store_fault::disarm();

    // The reconcile retry eventually closes: the tombstone lifts and the
    // proved COMMITTED finalize succeeds on the same authority instance.
    reconciled_record(
        authority
            .reconcile_effect(reconcile_request(
                &spec_a,
                &permit_a,
                0,
                adoption.receipt_id,
                ReconcileOutcome::EffectClosed,
                [0xaa; 32],
            ))
            .expect("reconcile retry closes"),
    );
    assert_eq!(
        authority
            .inspect_permit(task_id(), permit_a.permit_id)
            .expect("permit")
            .state,
        PermitState::Issued,
        "the tombstone lifts once the last unknown slot resolves"
    );
    match authority
        .finalize_commit_v3(finalize_v3(
            &spec_a,
            permit_a.permit_id,
            vec![success_proof(&authority, permit_a.permit_id, 0)],
        ))
        .expect("finalize A")
    {
        FinalizeDecision::Committed(receipt) => {
            assert_eq!(receipt.outcome, ReceiptOutcome::Committed);
            assert_eq!(receipt.new_head_commit_seq, 1);
        }
        other @ FinalizeDecision::Replayed(_) => panic!("expected Committed, got {other:?}"),
    }

    // A full second competition on the advanced head.
    let permit_b = run_second_competition(&authority);
    drop(authority);

    // A full reopen confirms the post-recovery v3 writes are durable.
    assert_reopened_v3_state(&database, permit_a.permit_id, permit_b.permit_id);
}

/// Post-recovery invariant after a full reopen: head=2, both slots
/// `EFFECT_CLOSED`, gapless two-entry history, both finalize proofs.
fn assert_reopened_v3_state(
    database: &TestDatabase,
    first_permit_id: CommitPermitId,
    second_permit_id: CommitPermitId,
) {
    let reopened = SqliteTaskAuthority::open(&database.path).expect("reopen after recovery");
    let head = reopened.inspect_task(task_id()).expect("head after reopen");
    assert_eq!(head.head_commit_seq, 2);
    assert_eq!(head.permit_epoch, 2);
    assert_eq!(head.active_permit, None);
    assert_eq!(
        reopened
            .inspect_effect_slot(first_permit_id, 0)
            .expect("slot A")
            .state,
        SlotState::EffectClosed
    );
    assert_eq!(
        reopened
            .inspect_effect_slot(second_permit_id, 0)
            .expect("slot B")
            .state,
        SlotState::EffectClosed
    );
    let history = reopened.list_effect_history(task_id()).expect("history");
    assert_eq!(history.len(), 2);
    assert_eq!(history[0].effect_history_seq, 1);
    assert_eq!(history[1].effect_history_seq, 2);
    assert_eq!(raw_count(&database.path, "task_receipts"), 2);
    assert_eq!(raw_count(&database.path, "task_finalize_proofs"), 2);
    assert_integrity(&database.path);
}
