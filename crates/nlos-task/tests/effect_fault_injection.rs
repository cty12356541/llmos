//! B-TASK-003 crash-window and effect-table fault-injection tests: the
//! `EffectPermit`/`EffectSlot` machinery (schema v2, commit 6233890) under
//! the three canonical crash windows (issue-31 evidence gate item 5) and
//! the PoC-0003-aligned F1-F4 fault matrix on the effect table group
//! (item 6), reusing the `nlos-store-fault` VFS harness established for
//! B-TASK-001 (`fault_injection.rs`) and `nlos-store` (`fault_crash.rs`).
//!
//! The three windows:
//! 1. dispatch token minted but unconsumed (slot `PERMITTED`) -> kill-9:
//!    after reopen the token is provably unconsumed, a `NO_EFFECT` closure
//!    is legal, and the slot can never masquerade as executed.
//! 2. token consumed (slot `DISPATCHED`), external call in flight ->
//!    kill-9: after reopen the slot stays `DISPATCHED` and unclosed;
//!    finalization is blocked (`OutstandingEffectSlots`) until the caller
//!    registers `EFFECT_CLOSED` or `EFFECT_UNKNOWN`.
//! 3. external call succeeded, effect receipt not yet written -> kill-9:
//!    identical durable shape as window 2; the caller holding the real
//!    result registers `EFFECT_CLOSED` and the permit commits.
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
    AttemptSpec, AttemptState, DispatchRequest, EffectPermitDecision, EffectPermitId,
    EffectPermitRequest, EffectReceiptDecision, FinalizeDecision, FinalizeRequest, IssuedPermit,
    LogicalEffectDescriptor, NoEffectReason, NoEffectRequest, Outcome, OutcomeRequest,
    PermitDecision, PermitRecord, PermitRequest, PermitState, PlannedEffect, ReceiptKind,
    ReceiptOutcome, SlotState, SnapshotBundle, SqliteTaskAuthority, TaskSpec, TaskStoreError,
    empty_effect_history_root,
};
use nlos_types::{
    CancellationScopeId, CommitPermitId, Generation, IdempotencyKey, TaskAttemptId, TaskId,
    TaskSnapshotId,
};

const VFS_NAME: &str = "nlos-task-effect-fault";

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
            "nlos-task-effect-fault-{name}-{}-{sequence}.sqlite3",
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
) -> DispatchRequest {
    DispatchRequest {
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
    token: Option<[u8; 32]>,
) -> NoEffectRequest {
    NoEffectRequest {
        task_id: spec.task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        permit_id: permit.permit_id,
        permit_epoch: permit.permit_epoch,
        effect_seq,
        reason,
        dispatch_token: token,
        recorded_at_ms: 6_000,
    }
}

fn finalize_request(spec: &AttemptSpec, permit_id: CommitPermitId) -> FinalizeRequest {
    FinalizeRequest {
        task_id: spec.task_id,
        attempt_id: spec.attempt_id,
        attempt_generation: spec.attempt_generation,
        permit_id,
        new_effect_history_root: [0x31; 32],
        new_retry_fence_epoch: 0,
        finalized_at_ms: 7_000,
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

/// Registers a task plus one attempt, and issues its `CommitPermit` with
/// the given declared effect set: the shared committed prefix every
/// scenario starts from.
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

/// Reopens the database after a kill-9 and rebuilds the request fixtures
/// the parent needs (spec + inspected permit record).
fn reopen_after_kill(
    database: &TestDatabase,
    permit_id: CommitPermitId,
) -> (SqliteTaskAuthority, AttemptSpec, PermitRecord) {
    let authority = SqliteTaskAuthority::open(&database.path).expect("reopen after kill");
    let spec = attempt_spec(0x0a, snapshot(0, 0));
    let permit = authority
        .inspect_permit(task_id(), permit_id)
        .expect("permit");
    (authority, spec, permit)
}

// ---------------------------------------------------------------------------
// kill-9 child-process harness (fault_injection.rs / fault_crash.rs 范式:
// current_exe + env var + piped READY marker, never sleeps)
// ---------------------------------------------------------------------------

fn spawn_child(scenario: &str, path: &Path) -> Child {
    Command::new(std::env::current_exe().expect("current test executable"))
        .args(["--exact", "crash_child_helper", "--nocapture"])
        .env("NLOS_EFFECT_CRASH_CHILD_SCENARIO", scenario)
        .env("NLOS_EFFECT_CRASH_CHILD_DATABASE", path)
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

/// Parses the `READY <permit hex> <effect permit hex>` marker emitted by
/// the child scenarios that mint an `EffectPermit`.
fn parse_marker(marker: &str) -> (CommitPermitId, EffectPermitId) {
    let mut parts = marker.split_whitespace();
    assert_eq!(parts.next(), Some("READY"), "marker prefix");
    let permit_id = CommitPermitId::from_bytes(hex_decode16(parts.next().expect("permit hex")));
    let effect_permit_id =
        EffectPermitId::from_bytes(hex_decode16(parts.next().expect("effect permit hex")));
    assert!(parts.next().is_none(), "marker carries exactly two ids");
    (permit_id, effect_permit_id)
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

fn child_marker(permit: &PermitRecord, issued: &IssuedPermit) -> String {
    format!(
        "READY {} {}",
        hex_encode(permit.permit_id.as_bytes()),
        hex_encode(issued.effect_permit_id.as_bytes())
    )
}

/// Window 1: the dispatch token is minted (slot `PERMITTED`) but never
/// consumed when the process dies.
fn child_window1_permitted(path: &Path) -> ! {
    let authority = SqliteTaskAuthority::open(path).expect("open");
    let (spec, permit) = seed_winner(&authority, vec![planned(0, false)]);
    let issued = issued_effect_permit(
        authority
            .request_effect_permit(effect_request(&spec, &permit, 0, 0xe1))
            .expect("issue"),
    );
    announce(&child_marker(&permit, &issued));
    let _keeper = authority;
    loop {
        std::thread::park();
    }
}

/// Windows 2+3 and the torn-tail fixture: the token is consumed (slot
/// `DISPATCHED`) and the external call is in flight; no outcome is
/// registered before the kill. The windows differ only in what the caller
/// knows after recovery, which is the parent's side of the test.
fn child_dispatched_unclosed(path: &Path) -> ! {
    let authority = SqliteTaskAuthority::open(path).expect("open");
    let (spec, permit) = seed_winner(&authority, vec![planned(0, true)]);
    let issued = issued_effect_permit(
        authority
            .request_effect_permit(effect_request(&spec, &permit, 0, 0xe1))
            .expect("issue"),
    );
    authority
        .consume_dispatch_token(dispatch_request(&spec, &permit, &issued))
        .expect("consume");
    announce(&child_marker(&permit, &issued));
    let _keeper = authority;
    loop {
        std::thread::park();
    }
}

/// Fault row 1: a writer transaction has dirtied the effect tables but has
/// not committed when the process dies.
fn child_mid_effect_tx(path: &Path) -> ! {
    let authority = SqliteTaskAuthority::open(path).expect("open");
    let _winner = seed_winner(&authority, vec![planned(0, true), planned(1, false)]);
    let raw = rusqlite::Connection::open(path).expect("raw connection");
    raw.execute_batch("BEGIN IMMEDIATE").expect("begin mid-tx");
    raw.execute("UPDATE effect_slots SET slot_state = slot_state + 100", [])
        .expect("mid-tx slot write");
    raw.execute(
        "UPDATE permit_effect_sets SET revision = revision + 100",
        [],
    )
    .expect("mid-tx summary write");
    announce("READY");
    let _keepers = (authority, raw);
    loop {
        std::thread::park();
    }
}

/// Fault row 2: a complete committed effect lifecycle (issue -> dispatch ->
/// `EFFECT_CLOSED` on the required slot, `NO_EFFECT` on the optional slot,
/// finalize) before the kill.
fn child_after_effect_commit(path: &Path) -> ! {
    let authority = SqliteTaskAuthority::open(path).expect("open");
    let (spec, permit) = seed_winner(&authority, vec![planned(0, true), planned(1, false)]);
    let issued = issued_effect_permit(
        authority
            .request_effect_permit(effect_request(&spec, &permit, 0, 0xe1))
            .expect("issue"),
    );
    authority
        .consume_dispatch_token(dispatch_request(&spec, &permit, &issued))
        .expect("consume");
    authority
        .record_effect_outcome(outcome_request(
            &spec,
            &permit,
            0,
            Outcome::Closed {
                authoritative_closure_digest: [0xaa; 32],
            },
        ))
        .expect("close slot 0");
    authority
        .record_no_effect(no_effect_request(
            &spec,
            &permit,
            1,
            NoEffectReason::NotSelected,
            None,
        ))
        .expect("no-effect slot 1");
    authority
        .finalize_commit(finalize_request(&spec, permit.permit_id))
        .expect("finalize");
    announce(&child_marker(&permit, &issued));
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
        std::env::var("NLOS_EFFECT_CRASH_CHILD_SCENARIO"),
        std::env::var("NLOS_EFFECT_CRASH_CHILD_DATABASE"),
    ) else {
        return;
    };
    let path = PathBuf::from(path);
    match scenario.as_str() {
        "window1-permitted" => child_window1_permitted(&path),
        "dispatched-unclosed" | "effect-wal-setup" => child_dispatched_unclosed(&path),
        "mid-effect-tx" => child_mid_effect_tx(&path),
        "after-effect-commit" => child_after_effect_commit(&path),
        other => panic!("unknown crash child scenario {other}"),
    }
}

// ---------------------------------------------------------------------------
// 窗口 1: token issued, never consumed (slot PERMITTED) -> kill-9
// ---------------------------------------------------------------------------

/// 窗口1：dispatch token 已签发、未消费（slot `PERMITTED`）时子进程被
/// SIGKILL。重开后 slot 仍为 `PERMITTED`、签发重放返回同一 token（token
/// 可证明未消费）；该 slot 不得被冒充为已执行（PERMITTED 上登记 outcome
/// 类型化拒绝、伪造 token 类型化拒绝且状态不动）；以未消费 token 走
/// `NO_EFFECT` 收口合法；全部 slot 终态后 permit 正常 COMMITTED。
#[test]
fn crash_window1_unconsumed_token_closes_no_effect_and_permit_commits() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestDatabase::new("window1-permitted");
    let mut child = spawn_child("window1-permitted", &database.path);
    let marker = await_marker(&mut child);
    let (permit_id, effect_permit_id) = parse_marker(&marker);
    kill_and_reap(&mut child);

    let (authority, spec, permit) = reopen_after_kill(&database, permit_id);

    // The slot is durably PERMITTED with the token minted but unconsumed.
    let slot = authority.inspect_effect_slot(permit_id, 0).expect("slot");
    assert_eq!(slot.state, SlotState::Permitted);
    assert_eq!(slot.state_seq, 1);
    assert_eq!(slot.effect_permit_id, Some(effect_permit_id));
    assert_eq!(slot.effect_receipt_id, None);
    assert_eq!(raw_count(&database.path, "effect_permits"), 1);
    assert_eq!(raw_count(&database.path, "effect_receipts"), 0);
    // PERMITTED is not a terminal state: it blocks permit closure.
    assert!(SlotState::Permitted.blocks_finalization());
    assert!(matches!(
        authority.finalize_commit(finalize_request(&spec, permit_id)),
        Err(TaskStoreError::OutstandingEffectSlots { count: 1 })
    ));

    // Issuance replay after the crash returns the original permit with the
    // same token: the token is durable and provably unconsumed.
    let issued = match authority
        .request_effect_permit(effect_request(&spec, &permit, 0, 0xe1))
        .expect("replay issuance")
    {
        EffectPermitDecision::Replayed(original) => *original,
        other @ EffectPermitDecision::Issued(_) => panic!("expected Replayed, got {other:?}"),
    };
    assert_eq!(issued.effect_permit_id, effect_permit_id);

    // The slot can never masquerade as executed: an outcome on a PERMITTED
    // slot is refused with its exact durable state, and a forged token is a
    // typed mismatch that moves nothing.
    assert!(matches!(
        authority.record_effect_outcome(outcome_request(
            &spec,
            &permit,
            0,
            Outcome::Closed {
                authoritative_closure_digest: [0xaa; 32],
            },
        )),
        Err(TaskStoreError::InvalidEffectSlotState {
            state: SlotState::Permitted
        })
    ));
    let mut forged = dispatch_request(&spec, &permit, &issued);
    forged.dispatch_token = [0xee; 32];
    assert!(matches!(
        authority.consume_dispatch_token(forged),
        Err(TaskStoreError::DispatchTokenMismatch)
    ));
    assert_eq!(
        authority
            .inspect_effect_slot(permit_id, 0)
            .expect("slot")
            .state,
        SlotState::Permitted,
        "forged token must not move the slot"
    );

    // Presenting the verifiably unconsumed token makes the NO_EFFECT
    // closure legal.
    let recorded = authority
        .record_no_effect(no_effect_request(
            &spec,
            &permit,
            0,
            NoEffectReason::PolicySkipped,
            Some(issued.one_shot_dispatch_token),
        ))
        .expect("no-effect closure");
    match recorded {
        EffectReceiptDecision::Recorded(receipt) => {
            assert_eq!(receipt.kind, ReceiptKind::NoEffect);
            assert_eq!(receipt.prior_slot_state, SlotState::Permitted);
            assert_eq!(
                receipt.no_effect_reason,
                Some(NoEffectReason::PolicySkipped)
            );
        }
        other @ EffectReceiptDecision::Replayed(_) => {
            panic!("expected Recorded, got {other:?}")
        }
    }

    // Every declared slot is terminal, so the permit closes normally
    // (required=false slot: no required-COMMITTED semantics asserted).
    match authority
        .finalize_commit(finalize_request(&spec, permit_id))
        .expect("finalize with all slots terminal")
    {
        FinalizeDecision::Committed(receipt) => {
            assert_eq!(receipt.new_head_commit_seq, 1);
        }
        other @ FinalizeDecision::Replayed(_) => panic!("expected Committed, got {other:?}"),
    }
    let head = authority.inspect_task(task_id()).expect("head");
    assert_eq!(head.head_commit_seq, 1);
    assert_eq!(head.active_permit, None);
    assert_eq!(
        authority
            .inspect_permit(task_id(), permit_id)
            .expect("permit")
            .state,
        PermitState::Closed
    );
    assert_integrity(&database.path);
}

// ---------------------------------------------------------------------------
// 窗口 2: token consumed (slot DISPATCHED), external call in flight -> kill-9
// ---------------------------------------------------------------------------

/// 窗口2：token 已消费（slot `DISPATCHED`）、外部调用进行中时子进程被
/// SIGKILL。重开后 slot 保持 `DISPATCHED` 未闭合：不得静默视为成功（无
/// receipt、finalize 被 `OutstandingEffectSlots` 阻塞），也不得静默视为
/// 失败（`DISPATCHED` 拒绝 `NO_EFFECT` 改名）；调用方不确定时显式登记
/// `EFFECT_UNKNOWN`，持久且跨重启阻塞关闭（四态区分：UNKNOWN/DISPATCHED
/// 不冒充任何终态；permit 保持 `ISSUED`，不冒充失败）。
#[test]
fn crash_window2_dispatched_unclosed_blocks_finalize_and_unknown_stays_durable() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestDatabase::new("window2-dispatched");
    let mut child = spawn_child("dispatched-unclosed", &database.path);
    let marker = await_marker(&mut child);
    let (permit_id, effect_permit_id) = parse_marker(&marker);
    kill_and_reap(&mut child);

    let (authority, spec, permit) = reopen_after_kill(&database, permit_id);

    // The slot is durably DISPATCHED and unclosed: neither a silent success
    // (no receipt exists) nor a silent failure.
    let slot = authority.inspect_effect_slot(permit_id, 0).expect("slot");
    assert_eq!(slot.state, SlotState::Dispatched);
    assert_eq!(slot.state_seq, 2);
    assert_eq!(slot.effect_permit_id, Some(effect_permit_id));
    assert_eq!(slot.effect_receipt_id, None);
    assert_eq!(raw_count(&database.path, "effect_receipts"), 0);

    // Four-state distinction (issue-31 item 6): DISPATCHED and
    // EFFECT_UNKNOWN never masquerade as a terminal closure; only
    // NO_EFFECT and EFFECT_CLOSED unblock permit closure.
    assert!(SlotState::Dispatched.blocks_finalization());
    assert!(SlotState::EffectUnknown.blocks_finalization());
    assert!(!SlotState::NoEffect.blocks_finalization());
    assert!(!SlotState::EffectClosed.blocks_finalization());
    assert!(matches!(
        authority.finalize_commit(finalize_request(&spec, permit_id)),
        Err(TaskStoreError::OutstandingEffectSlots { count: 1 })
    ));

    // A consumed token can never be renamed no-effect: the DISPATCHED slot
    // rejects the no-effect path with its exact durable state.
    assert!(matches!(
        authority.record_no_effect(no_effect_request(
            &spec,
            &permit,
            0,
            NoEffectReason::CancelledBeforeDispatch,
            None,
        )),
        Err(TaskStoreError::InvalidEffectSlotState {
            state: SlotState::Dispatched
        })
    ));

    // The caller cannot prove the outcome: EFFECT_UNKNOWN registers
    // durably and keeps blocking closure across another restart.
    let uncertainty = Outcome::Unknown {
        uncertainty_digest: [0x99; 32],
    };
    let recorded = authority
        .record_effect_outcome(outcome_request(&spec, &permit, 0, uncertainty))
        .expect("register uncertainty");
    let receipt_id = match recorded {
        EffectReceiptDecision::Recorded(receipt) => {
            assert_eq!(receipt.kind, ReceiptKind::EffectUnknown);
            assert_eq!(receipt.prior_slot_state, SlotState::Dispatched);
            receipt.receipt_id
        }
        other @ EffectReceiptDecision::Replayed(_) => {
            panic!("expected Recorded, got {other:?}")
        }
    };
    drop(authority);

    let authority = SqliteTaskAuthority::open(&database.path).expect("second reopen");
    let slot = authority.inspect_effect_slot(permit_id, 0).expect("slot");
    assert_eq!(slot.state, SlotState::EffectUnknown);
    assert_eq!(slot.effect_receipt_id, Some(receipt_id));
    assert!(
        matches!(
            authority.finalize_commit(finalize_request(&spec, permit_id)),
            Err(TaskStoreError::OutstandingEffectSlots { count: 1 })
        ),
        "EFFECT_UNKNOWN durably blocks closure (PARTIAL mapping: reconcile is mainline work)"
    );
    assert_eq!(
        authority
            .inspect_permit(task_id(), permit_id)
            .expect("permit")
            .state,
        PermitState::Issued,
        "UNKNOWN must not masquerade as failure: the permit stays open"
    );
    // Exact replay of the uncertainty registration returns the original
    // receipt; the slot refuses a rewrite to EFFECT_CLOSED (reconcile is
    // the next slice).
    assert!(matches!(
        authority.record_effect_outcome(outcome_request(&spec, &permit, 0, uncertainty)),
        Ok(EffectReceiptDecision::Replayed(_))
    ));
    assert!(matches!(
        authority.record_effect_outcome(outcome_request(
            &spec,
            &permit,
            0,
            Outcome::Closed {
                authoritative_closure_digest: [0xaa; 32],
            },
        )),
        Err(TaskStoreError::InvalidEffectSlotState {
            state: SlotState::EffectUnknown
        })
    ));
    assert_integrity(&database.path);
}

// ---------------------------------------------------------------------------
// 窗口 3: external call succeeded, effect receipt not yet written -> kill-9
// ---------------------------------------------------------------------------

/// 窗口3：外部调用已成功、effect receipt 写入前子进程被 SIGKILL（耐久
/// 形态与窗口2相同：slot `DISPATCHED` 未闭合）。持有真实结果的调用方在
/// 重开后登记 `EFFECT_CLOSED`，permit 随即 COMMITTED（required slot 以
/// `EFFECT_CLOSED` 收口，不断言 `NO_EFFECT` 收口的 required 语义）。
#[test]
fn crash_window3_dispatched_then_effect_closed_commits_after_restart() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestDatabase::new("window3-dispatched");
    let mut child = spawn_child("dispatched-unclosed", &database.path);
    let marker = await_marker(&mut child);
    let (permit_id, effect_permit_id) = parse_marker(&marker);
    kill_and_reap(&mut child);

    let (authority, spec, permit) = reopen_after_kill(&database, permit_id);

    // Same durable shape as window 2: DISPATCHED, unclosed, blocking.
    let slot = authority.inspect_effect_slot(permit_id, 0).expect("slot");
    assert_eq!(slot.state, SlotState::Dispatched);
    assert_eq!(slot.state_seq, 2);
    assert_eq!(slot.effect_permit_id, Some(effect_permit_id));
    assert_eq!(slot.effect_receipt_id, None);
    assert_eq!(raw_count(&database.path, "effect_receipts"), 0);
    assert!(matches!(
        authority.finalize_commit(finalize_request(&spec, permit_id)),
        Err(TaskStoreError::OutstandingEffectSlots { count: 1 })
    ));

    // The caller holds the real result: EFFECT_CLOSED unblocks the permit.
    let recorded = authority
        .record_effect_outcome(outcome_request(
            &spec,
            &permit,
            0,
            Outcome::Closed {
                authoritative_closure_digest: [0xaa; 32],
            },
        ))
        .expect("register closure");
    match recorded {
        EffectReceiptDecision::Recorded(receipt) => {
            assert_eq!(receipt.kind, ReceiptKind::EffectClosed);
            assert_eq!(receipt.prior_slot_state, SlotState::Dispatched);
            assert_eq!(receipt.proof_digest, [0xaa; 32]);
        }
        other @ EffectReceiptDecision::Replayed(_) => {
            panic!("expected Recorded, got {other:?}")
        }
    }

    match authority
        .finalize_commit(finalize_request(&spec, permit_id))
        .expect("finalize after EFFECT_CLOSED")
    {
        FinalizeDecision::Committed(receipt) => {
            assert_eq!(receipt.new_head_commit_seq, 1);
        }
        other @ FinalizeDecision::Replayed(_) => panic!("expected Committed, got {other:?}"),
    }
    let head = authority.inspect_task(task_id()).expect("head");
    assert_eq!(head.head_commit_seq, 1);
    assert_eq!(head.active_permit, None);
    assert_eq!(
        authority
            .inspect_permit(task_id(), permit_id)
            .expect("permit")
            .state,
        PermitState::Closed
    );
    assert_eq!(
        authority
            .inspect_attempt(task_id(), spec.attempt_id)
            .expect("attempt")
            .state,
        AttemptState::Committed
    );
    let summary = authority
        .inspect_effect_set(permit_id)
        .expect("summary")
        .expect("declared set");
    assert_eq!(summary.satisfied_required_effect_count, 1);
    assert_eq!(summary.terminal_effect_count, 1);
    assert_integrity(&database.path);
}

// ---------------------------------------------------------------------------
// 矩阵行 1: kill-9 mid-transaction on the effect tables leaves no half state
// ---------------------------------------------------------------------------

/// Raw parent-side proof that the killed child's uncommitted dirt on the
/// effect tables rolled back completely: both slots back to the committed
/// `PLANNED`/`state_seq=0`, the summary revision back to its committed value.
fn assert_effect_tables_rolled_back(path: &Path) {
    let raw = rusqlite::Connection::open(path).expect("raw reopen");
    let mut statement = raw
        .prepare("SELECT slot_state, state_seq FROM effect_slots ORDER BY effect_seq")
        .expect("prepare slot rows");
    let rows = statement
        .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))
        .expect("query slot rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect slot rows");
    assert_eq!(
        rows,
        vec![(0, 0), (0, 0)],
        "mid-transaction slot dirt must be rolled back to PLANNED/state_seq=0"
    );
    drop(statement);
    let revision: i64 = raw
        .query_row("SELECT revision FROM permit_effect_sets", [], |row| {
            row.get(0)
        })
        .expect("query summary revision");
    assert_eq!(
        revision, 0,
        "mid-transaction summary dirt must be rolled back"
    );
}

/// kill-9 中断 effect 表写事务：子进程在 `BEGIN IMMEDIATE` 未提交（已弄脏
/// `effect_slots.slot_state` 与 `permit_effect_sets.revision`）时被强杀；
/// 重开后中断事务完全回滚，slot 保持已提交的 `PLANNED`/`state_seq=0`，
/// summary revision 回到已提交值，无半截 slot 状态，effect 签发/dispatch
/// 表无任何幻影行，authority 正常重开。
#[test]
fn fault_kill9_mid_effect_transaction_leaves_no_half_state() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestDatabase::new("kill9-mid-effect-tx");
    let mut child = spawn_child("mid-effect-tx", &database.path);
    await_marker(&mut child);
    kill_and_reap(&mut child);

    assert_effect_tables_rolled_back(&database.path);
    assert_eq!(raw_count(&database.path, "effect_slots"), 2);
    assert_eq!(raw_count(&database.path, "permit_effect_sets"), 1);
    assert_eq!(raw_count(&database.path, "effect_permits"), 0);
    assert_eq!(raw_count(&database.path, "effect_receipts"), 0);

    let authority = SqliteTaskAuthority::open(&database.path).expect("reopen after kill");
    let head = authority.inspect_task(task_id()).expect("head");
    assert_eq!(
        head.permit_epoch, 1,
        "the committed prefix keeps the permit"
    );
    let permit_id = head.active_permit.expect("active permit");
    for effect_seq in [0, 1] {
        let slot = authority
            .inspect_effect_slot(permit_id, effect_seq)
            .expect("slot");
        assert_eq!(slot.state, SlotState::Planned);
        assert_eq!(slot.state_seq, 0);
        assert_eq!(slot.effect_permit_id, None);
        assert_eq!(slot.effect_receipt_id, None);
    }
    assert_eq!(
        authority
            .inspect_permit(task_id(), permit_id)
            .expect("permit")
            .state,
        PermitState::Issued
    );
    assert_integrity(&database.path);
}

// ---------------------------------------------------------------------------
// 矩阵行 2: kill-9 after commit — the committed effect lifecycle survives
// ---------------------------------------------------------------------------

/// Verifies the durable bit-for-bit effect state of the committed
/// lifecycle fixture: slot 0 PLANNED -> PERMITTED -> DISPATCHED ->
/// `EFFECT_CLOSED`, slot 1 PLANNED -> `NO_EFFECT`, and the spec-mandated
/// summary counts.
fn assert_effect_lifecycle_durable(
    authority: &SqliteTaskAuthority,
    permit_id: CommitPermitId,
    effect_permit_id: EffectPermitId,
) {
    let slot0 = authority.inspect_effect_slot(permit_id, 0).expect("slot 0");
    assert_eq!(slot0.state, SlotState::EffectClosed);
    assert_eq!(slot0.state_seq, 3);
    assert_eq!(slot0.effect_permit_id, Some(effect_permit_id));
    let receipt0 = authority
        .inspect_effect_receipt(slot0.effect_receipt_id.expect("closure receipt id"))
        .expect("closure receipt");
    assert_eq!(receipt0.kind, ReceiptKind::EffectClosed);
    assert_eq!(receipt0.prior_slot_state, SlotState::Dispatched);
    assert_eq!(receipt0.proof_digest, [0xaa; 32]);

    let slot1 = authority.inspect_effect_slot(permit_id, 1).expect("slot 1");
    assert_eq!(slot1.state, SlotState::NoEffect);
    assert_eq!(slot1.state_seq, 1);
    let receipt1 = authority
        .inspect_effect_receipt(slot1.effect_receipt_id.expect("no-effect receipt id"))
        .expect("no-effect receipt");
    assert_eq!(receipt1.kind, ReceiptKind::NoEffect);
    assert_eq!(receipt1.prior_slot_state, SlotState::Planned);
    assert_eq!(receipt1.no_effect_reason, Some(NoEffectReason::NotSelected));

    let summary = authority
        .inspect_effect_set(permit_id)
        .expect("summary")
        .expect("declared set");
    assert_eq!(summary.required_effect_count, 1);
    assert_eq!(summary.satisfied_required_effect_count, 1);
    assert_eq!(summary.terminal_effect_count, 2);
}

/// commit 后崩溃等价：子进程在 effect 全生命周期（签发 → dispatch →
/// 必填槽 `EFFECT_CLOSED` + 可选槽 `NO_EFFECT` → finalize）全部提交返回后
/// 被强杀；重开后 slot/permit/token/receipt/summary 全部逐位保留，permit
/// 与 finalize 重放返回原结果，effect 签发重放返回同一 token，关闭后的
/// permit 对迟到的 outcome/no-effect 登记以类型化 `PermitNotIssued`
/// 拒绝。
#[test]
fn fault_kill9_after_effect_commit_preserves_everything() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestDatabase::new("kill9-effect-commit");
    let mut child = spawn_child("after-effect-commit", &database.path);
    let marker = await_marker(&mut child);
    let (permit_id, effect_permit_id) = parse_marker(&marker);
    kill_and_reap(&mut child);

    assert_eq!(raw_count(&database.path, "effect_slots"), 2);
    assert_eq!(raw_count(&database.path, "effect_permits"), 1);
    assert_eq!(raw_count(&database.path, "effect_receipts"), 2);
    assert_eq!(raw_count(&database.path, "permit_effect_sets"), 1);
    assert_eq!(raw_count(&database.path, "task_receipts"), 1);

    let (authority, spec, permit) = reopen_after_kill(&database, permit_id);
    assert_eq!(permit.state, PermitState::Closed);
    let head = authority.inspect_task(task_id()).expect("head");
    assert_eq!(head.head_commit_seq, 1);
    assert_eq!(head.permit_epoch, 1);
    assert_eq!(head.active_permit, None);
    assert_effect_lifecycle_durable(&authority, permit_id, effect_permit_id);

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
        .request_commit_permit(permit_request(
            &spec,
            0x01,
            vec![planned(0, true), planned(1, false)],
        ))
        .expect("replay commit permit")
    {
        PermitDecision::Replayed(original) => {
            assert_eq!(original.permit_id, permit_id);
            assert_eq!(original.state, PermitState::Closed);
        }
        other => panic!("expected Replayed, got {other:?}"),
    }
    match authority
        .request_effect_permit(effect_request(&spec, &permit, 0, 0xe1))
        .expect("replay effect issuance")
    {
        EffectPermitDecision::Replayed(original) => {
            assert_eq!(original.effect_permit_id, effect_permit_id);
        }
        other @ EffectPermitDecision::Issued(_) => panic!("expected Replayed, got {other:?}"),
    }
    match authority
        .finalize_commit(finalize_request(&spec, permit_id))
        .expect("replay finalize")
    {
        FinalizeDecision::Replayed(original) => assert_eq!(*original, commit_receipt),
        other @ FinalizeDecision::Committed(_) => panic!("expected Replayed, got {other:?}"),
    }
    // The closed permit fences late outcome/no-effect registrations with a
    // typed error; the durable receipts stay readable via inspect.
    assert!(matches!(
        authority.record_effect_outcome(outcome_request(
            &spec,
            &permit,
            0,
            Outcome::Closed {
                authoritative_closure_digest: [0xaa; 32],
            },
        )),
        Err(TaskStoreError::PermitNotIssued)
    ));
    assert!(matches!(
        authority.record_no_effect(no_effect_request(
            &spec,
            &permit,
            1,
            NoEffectReason::NotSelected,
            None,
        )),
        Err(TaskStoreError::PermitNotIssued)
    ));
    assert_integrity(&database.path);
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

// ---------------------------------------------------------------------------
// 矩阵行 3: hard I/O error on effect write transactions fails closed
// ---------------------------------------------------------------------------

/// 写入硬 I/O 错误：`FailWritesAfter { 0, IoErr }` 下（a）携带声明
/// effect 集的 permit CAS 与（b）effect 签发 CAS 都必须以
/// `TaskStoreError::Sqlite` 显式失败（错误链含 I/O 条件），不返回假成功；
/// 不产生半截状态（无 permit/slot/summary/effect-permit 行、slot 保持
/// `PLANNED`、`control_epoch` 不动）；disarm 后同一请求成功。
#[test]
fn fault_io_error_on_effect_writes_fails_closed_without_half_state() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestDatabase::new("ioerr-effect");
    let authority = open_shim(&database.path);
    authority.register_task(task_spec()).expect("register task");
    let spec = attempt_spec(0x0a, snapshot(0, 0));
    authority.register_attempt(spec).expect("register attempt");

    // Phase A: the commit-permit CAS that would persist the declared
    // effect set dies on its first write.
    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::IoErr,
    });
    let error = authority
        .request_commit_permit(permit_request(&spec, 0x01, vec![planned(0, true)]))
        .expect_err("permit CAS must fail under injected I/O error");
    assert_sqlite_error_chain(&error, &["i/o", "ioerr"]);
    assert!(nlos_store_fault::writes_observed() > 0);

    // No half-decision: no permit, no slot rows, no effect-set control row.
    let head = authority.inspect_task(task_id()).expect("head");
    assert_eq!(head.permit_epoch, 0);
    assert_eq!(head.active_permit, None);
    assert_eq!(raw_count(&database.path, "commit_permits"), 0);
    assert_eq!(raw_count(&database.path, "effect_slots"), 0);
    assert_eq!(raw_count(&database.path, "permit_effect_sets"), 0);
    assert_eq!(raw_count(&database.path, "effect_permits"), 0);

    nlos_store_fault::disarm();
    let permit = issued_permit(
        authority
            .request_commit_permit(permit_request(&spec, 0x01, vec![planned(0, true)]))
            .expect("permit CAS succeeds after disarm"),
    );
    assert_eq!(raw_count(&database.path, "effect_slots"), 1);
    let control_before = authority
        .inspect_task(task_id())
        .expect("head")
        .control_epoch;

    // Phase B: the effect-issuance CAS dies on its first write.
    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::IoErr,
    });
    let error = authority
        .request_effect_permit(effect_request(&spec, &permit, 0, 0xe1))
        .expect_err("effect issuance must fail under injected I/O error");
    assert_sqlite_error_chain(&error, &["i/o", "ioerr"]);

    // The slot never left PLANNED; no phantom effect permit exists.
    let slot = authority
        .inspect_effect_slot(permit.permit_id, 0)
        .expect("slot");
    assert_eq!(slot.state, SlotState::Planned);
    assert_eq!(slot.state_seq, 0);
    assert_eq!(slot.effect_permit_id, None);
    assert_eq!(raw_count(&database.path, "effect_permits"), 0);
    assert_eq!(
        authority
            .inspect_task(task_id())
            .expect("head")
            .control_epoch,
        control_before,
        "failed issuance must not advance the control epoch"
    );

    nlos_store_fault::disarm();
    let issued = issued_effect_permit(
        authority
            .request_effect_permit(effect_request(&spec, &permit, 0, 0xe1))
            .expect("issuance succeeds after disarm"),
    );
    let slot = authority
        .inspect_effect_slot(permit.permit_id, 0)
        .expect("slot");
    assert_eq!(slot.state, SlotState::Permitted);
    assert_eq!(slot.effect_permit_id, Some(issued.effect_permit_id));
    assert_integrity(&database.path);
}

// ---------------------------------------------------------------------------
// 矩阵行 4: disk-full (ENOSPC) on dispatch/outcome writes fails closed
// ---------------------------------------------------------------------------

/// disk-full（dispatch 写）：`FailWritesAfter { 0, Full }` 下 dispatch
/// token 消费 CAS 必须以 `SQLITE_FULL` 显式失败（错误链含 full）；不产生
/// 半截 dispatch（slot 保持 `PERMITTED`、token 未消费）；disarm 后同一
/// 消费成功。
#[test]
fn fault_enospc_on_dispatch_write_fails_closed() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestDatabase::new("full-effect-dispatch");
    let authority = open_shim(&database.path);
    let (spec, permit) = seed_winner(&authority, vec![planned(0, true)]);
    let issued = issued_effect_permit(
        authority
            .request_effect_permit(effect_request(&spec, &permit, 0, 0xe1))
            .expect("issue"),
    );

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::Full,
    });
    let error = authority
        .consume_dispatch_token(dispatch_request(&spec, &permit, &issued))
        .expect_err("dispatch must fail under injected disk-full");
    assert_sqlite_error_chain(&error, &["full"]);
    assert!(nlos_store_fault::writes_observed() > 0);

    // No half-dispatch: the slot is still PERMITTED, the token unconsumed.
    let slot = authority
        .inspect_effect_slot(permit.permit_id, 0)
        .expect("slot");
    assert_eq!(slot.state, SlotState::Permitted);
    assert_eq!(slot.state_seq, 1);

    nlos_store_fault::disarm();
    let dispatched = authority
        .consume_dispatch_token(dispatch_request(&spec, &permit, &issued))
        .expect("dispatch succeeds after disarm");
    assert_eq!(dispatched.state, SlotState::Dispatched);
    assert_integrity(&database.path);
}

/// disk-full（outcome 写）：`FailWritesAfter { 0, Full }` 下 effect
/// outcome 写事务（receipt + slot CAS + roots 重算同事务）必须以
/// `SQLITE_FULL` 显式失败（错误链含 full）；不产生半截闭合（slot 保持
/// `DISPATCHED`、无 receipt 行）；disarm 后同一登记成功。
#[test]
fn fault_enospc_on_outcome_write_fails_closed() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestDatabase::new("full-effect-outcome");
    let authority = open_shim(&database.path);
    let (spec, permit) = seed_winner(&authority, vec![planned(0, true)]);
    let issued = issued_effect_permit(
        authority
            .request_effect_permit(effect_request(&spec, &permit, 0, 0xe1))
            .expect("issue"),
    );
    authority
        .consume_dispatch_token(dispatch_request(&spec, &permit, &issued))
        .expect("consume");

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::Full,
    });
    let error = authority
        .record_effect_outcome(outcome_request(
            &spec,
            &permit,
            0,
            Outcome::Closed {
                authoritative_closure_digest: [0xaa; 32],
            },
        ))
        .expect_err("outcome write must fail under injected disk-full");
    assert_sqlite_error_chain(&error, &["full"]);

    // No half-closure: the slot is still DISPATCHED, no receipt row exists.
    let slot = authority
        .inspect_effect_slot(permit.permit_id, 0)
        .expect("slot");
    assert_eq!(slot.state, SlotState::Dispatched);
    assert_eq!(slot.state_seq, 2);
    assert_eq!(slot.effect_receipt_id, None);
    assert_eq!(raw_count(&database.path, "effect_receipts"), 0);

    nlos_store_fault::disarm();
    let recorded = authority
        .record_effect_outcome(outcome_request(
            &spec,
            &permit,
            0,
            Outcome::Closed {
                authoritative_closure_digest: [0xaa; 32],
            },
        ))
        .expect("outcome succeeds after disarm");
    assert!(matches!(recorded, EffectReceiptDecision::Recorded(_)));
    let slot = authority
        .inspect_effect_slot(permit.permit_id, 0)
        .expect("slot");
    assert_eq!(slot.state, SlotState::EffectClosed);
    assert_eq!(raw_count(&database.path, "effect_receipts"), 1);
    assert_integrity(&database.path);
}

// ---------------------------------------------------------------------------
// 矩阵行 5: silent write loss / torn tail fabricates no phantom effect facts
// ---------------------------------------------------------------------------

/// 静默丢写/短写（effect 表组）：
/// - Phase A（断电模型）：`PowerLossAfter { 0 }` 下 effect 签发 CAS
///   “报告成功”但写入从未落盘；重开后幻影 `EffectPermit` 不得冒充已提交
///   事实（slot 回到 `PLANNED`、`EffectPermitNotFound`、`control_epoch`
///   不前进），同一请求可重做且确定性身份/token 逐位相同、真实持久。
/// - Phase B（短写/撕裂尾部）：子进程提交 签发+dispatch 后被杀，父进程
///   把 WAL 截断到最后一个 commit 帧的一半；重开后 dispatch 提交整体
///   隐藏（slot 回到 `PERMITTED`），此前合法提交（含签发）完整保留，
///   幻影 DISPATCHED 不可见（PERMITTED 上登记 outcome 类型化拒绝），
///   同一 token 重放后可干净再消费。
#[test]
fn fault_silent_write_loss_and_torn_tail_hide_phantom_effect_facts() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    power_loss_drops_effect_issuance_and_redo_is_durable();
    torn_wal_tail_hides_dispatch_and_reconsume_succeeds();
}

/// Phase A: a silently dropped effect-issuance commit is invisible after
/// recovery and the lost decision is redoable with the same deterministic
/// identities.
fn power_loss_drops_effect_issuance_and_redo_is_durable() {
    let database = TestDatabase::new("power-loss-effect");
    let authority = open_shim(&database.path);
    let (spec, permit) = seed_winner(&authority, vec![planned(0, true)]);
    let control_before = authority
        .inspect_task(task_id())
        .expect("head")
        .control_epoch;

    nlos_store_fault::arm(FaultMode::PowerLossAfter { remaining: 0 });
    let phantom = issued_effect_permit(
        authority
            .request_effect_permit(effect_request(&spec, &permit, 0, 0xe1))
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
        SlotState::Planned,
        "silently dropped issuance must not move the slot"
    );
    assert_eq!(slot.state_seq, 0);
    assert_eq!(slot.effect_permit_id, None);
    assert!(
        matches!(
            recovered.inspect_effect_permit(task_id(), phantom.effect_permit_id),
            Err(TaskStoreError::EffectPermitNotFound)
        ),
        "phantom effect permit must not fabricate a committed fact"
    );
    assert_eq!(
        recovered
            .inspect_task(task_id())
            .expect("head")
            .control_epoch,
        control_before,
        "silently dropped issuance must not advance the control epoch"
    );
    assert_eq!(raw_count(&database.path, "effect_permits"), 0);
    assert_integrity(&database.path);

    // The lost decision is redoable: the deterministic effect permit id
    // and dispatch token are reused, and this time genuinely durable.
    let redone = issued_effect_permit(
        recovered
            .request_effect_permit(effect_request(&spec, &permit, 0, 0xe1))
            .expect("redo after power loss"),
    );
    assert_eq!(redone.effect_permit_id, phantom.effect_permit_id);
    assert_eq!(
        redone.one_shot_dispatch_token,
        phantom.one_shot_dispatch_token
    );
    drop(recovered);
    let verified = SqliteTaskAuthority::open(&database.path).expect("reopen after redo");
    let slot = verified
        .inspect_effect_slot(permit.permit_id, 0)
        .expect("slot");
    assert_eq!(slot.state, SlotState::Permitted);
    assert_eq!(slot.effect_permit_id, Some(redone.effect_permit_id));
    drop(verified);
    drop(database);
}

/// Phase B: truncating the WAL to half of the dispatch transaction's
/// commit frame hides the consumption entirely while keeping the committed
/// issuance prefix.
fn torn_wal_tail_hides_dispatch_and_reconsume_succeeds() {
    let database = TestDatabase::new("torn-tail-dispatch");
    let mut child = spawn_child("effect-wal-setup", &database.path);
    let marker = await_marker(&mut child);
    let (permit_id, effect_permit_id) = parse_marker(&marker);
    kill_and_reap(&mut child);

    let wal_path = TestDatabase::sibling(&database.path, "-wal");
    let mut wal = fs::read(&wal_path).expect("read wal");
    let (frame_size, commits) = wal_commit_frames(&wal);
    assert!(
        commits.len() >= 5,
        "fixture must contain schema + register + attempt + permit + issuance + dispatch commits"
    );
    let last_commit = *commits.last().expect("commits exist");
    let half_frame_cut = 32 + last_commit * frame_size + frame_size / 2;
    wal.truncate(half_frame_cut);
    fs::write(&wal_path, &wal).expect("write truncated wal");
    fs::remove_file(TestDatabase::sibling(&database.path, "-shm")).expect("remove stale shm");

    // Recovery drops the torn dispatch transaction entirely: the slot is
    // back to PERMITTED; the committed prefix (task/attempt/permit/
    // issuance) is intact.
    let recovered = SqliteTaskAuthority::open(&database.path).expect("reopen after truncation");
    let slot = recovered.inspect_effect_slot(permit_id, 0).expect("slot");
    assert_eq!(
        slot.state,
        SlotState::Permitted,
        "torn dispatch commit must be hidden"
    );
    assert_eq!(slot.state_seq, 1);
    assert_eq!(slot.effect_permit_id, Some(effect_permit_id));
    assert_eq!(raw_count(&database.path, "effect_permits"), 1);
    assert_eq!(raw_count(&database.path, "effect_receipts"), 0);

    // The phantom DISPATCHED state is invisible: an outcome registration
    // fails with the exact durable state.
    let spec = attempt_spec(0x0a, snapshot(0, 0));
    let permit = recovered
        .inspect_permit(task_id(), permit_id)
        .expect("permit");
    assert!(matches!(
        recovered.record_effect_outcome(outcome_request(
            &spec,
            &permit,
            0,
            Outcome::Closed {
                authoritative_closure_digest: [0xaa; 32],
            },
        )),
        Err(TaskStoreError::InvalidEffectSlotState {
            state: SlotState::Permitted
        })
    ));
    assert_integrity(&database.path);

    // The durable issuance replays the same token, which then consumes
    // cleanly: no conflicting half-record was left behind.
    let issued = match recovered
        .request_effect_permit(effect_request(&spec, &permit, 0, 0xe1))
        .expect("replay issuance")
    {
        EffectPermitDecision::Replayed(original) => *original,
        other @ EffectPermitDecision::Issued(_) => panic!("expected Replayed, got {other:?}"),
    };
    let dispatched = recovered
        .consume_dispatch_token(dispatch_request(&spec, &permit, &issued))
        .expect("re-consume after torn tail");
    assert_eq!(dispatched.state, SlotState::Dispatched);
    assert_eq!(dispatched.state_seq, 2);
    drop(recovered);
    drop(database);
}

// ---------------------------------------------------------------------------
// 矩阵行 6: after the fault clears, the effect flow continues from the prefix
// ---------------------------------------------------------------------------

/// Runs a full second competition on the advanced head: attempt B, a new
/// permit declaring a genuinely new business effect (`stable_action_slot`
/// 1 — slot 0's `LogicalEffectId` is already `EFFECT_CLOSED` in the durable
/// cross-attempt effect history, `[TASK-EFFECT-ID-001]`, schema v3), then
/// issue/consume/close and finalize.
fn run_second_competition(authority: &SqliteTaskAuthority) -> PermitRecord {
    let spec_b = attempt_spec(0x0b, snapshot(1, 0));
    authority.register_attempt(spec_b).expect("register B");
    let permit_b = issued_permit(
        authority
            .request_commit_permit(permit_request(&spec_b, 0x02, vec![planned(1, true)]))
            .expect("permit B"),
    );
    assert_eq!(permit_b.permit_epoch, 2);
    assert_eq!(permit_b.expected_head_commit_seq, 1);
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
    authority
        .finalize_commit(finalize_request(&spec_b, permit_b.permit_id))
        .expect("finalize B");
    permit_b
}

/// Asserts the committed prefix observed through the same authority is
/// identical to the pre-fault state: slot still `DISPATCHED` and unclosed,
/// summary roots untouched, `control_epoch` unmoved.
fn assert_prefix_unchanged(
    authority: &SqliteTaskAuthority,
    permit: &PermitRecord,
    summary_before: &nlos_task::SetSummary,
    control_before: u64,
) {
    let slot = authority
        .inspect_effect_slot(permit.permit_id, 0)
        .expect("slot");
    assert_eq!(slot.state, SlotState::Dispatched);
    assert_eq!(slot.state_seq, 2);
    assert_eq!(slot.effect_receipt_id, None);
    let summary_during = authority
        .inspect_effect_set(permit.permit_id)
        .expect("summary")
        .expect("declared set");
    assert_eq!(
        &summary_during, summary_before,
        "failed outcome write must not touch the summary roots"
    );
    assert_eq!(
        authority
            .inspect_task(task_id())
            .expect("head")
            .control_epoch,
        control_before
    );
}

/// 故障解除后：同一 authority（不重建）在 disarm 后继续正常读写；已提交
/// 前缀（slot `DISPATCHED`、summary roots、`control_epoch`）与故障前逐位
/// 一致；随后 `EFFECT_CLOSED` + `NO_EFFECT` 收口、finalize、新竞争（第二张
/// permit 绑定推进后的 head）、再 finalize 全部成功，完整重开后全部
/// effect 状态可恢复。
#[test]
fn fault_after_disarm_effect_flow_continues_from_committed_prefix() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestDatabase::new("disarm-effect-continue");
    let authority = open_shim(&database.path);
    let (spec_a, permit_a) = seed_winner(&authority, vec![planned(0, true), planned(1, false)]);
    let issued = issued_effect_permit(
        authority
            .request_effect_permit(effect_request(&spec_a, &permit_a, 0, 0xe1))
            .expect("issue"),
    );
    authority
        .consume_dispatch_token(dispatch_request(&spec_a, &permit_a, &issued))
        .expect("consume");
    let summary_before = authority
        .inspect_effect_set(permit_a.permit_id)
        .expect("summary")
        .expect("declared set");
    let control_before = authority
        .inspect_task(task_id())
        .expect("head")
        .control_epoch;

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::Full,
    });
    authority
        .record_effect_outcome(outcome_request(
            &spec_a,
            &permit_a,
            0,
            Outcome::Closed {
                authoritative_closure_digest: [0xaa; 32],
            },
        ))
        .expect_err("outcome write must fail while the fault is armed");

    // The committed prefix observed through the same authority is
    // identical to the pre-fault state.
    assert_prefix_unchanged(&authority, &permit_a, &summary_before, control_before);
    assert_eq!(raw_count(&database.path, "effect_receipts"), 0);

    nlos_store_fault::disarm();

    // The same authority instance continues: close both slots (required
    // slot via EFFECT_CLOSED) and commit.
    authority
        .record_effect_outcome(outcome_request(
            &spec_a,
            &permit_a,
            0,
            Outcome::Closed {
                authoritative_closure_digest: [0xaa; 32],
            },
        ))
        .expect("close slot 0 after disarm");
    authority
        .record_no_effect(no_effect_request(
            &spec_a,
            &permit_a,
            1,
            NoEffectReason::NotSelected,
            None,
        ))
        .expect("no-effect slot 1");
    match authority
        .finalize_commit(finalize_request(&spec_a, permit_a.permit_id))
        .expect("finalize A")
    {
        FinalizeDecision::Committed(receipt) => assert_eq!(receipt.new_head_commit_seq, 1),
        other @ FinalizeDecision::Replayed(_) => panic!("expected Committed, got {other:?}"),
    }

    // A full second competition on the advanced head.
    let permit_b = run_second_competition(&authority);

    let head = authority
        .inspect_task(task_id())
        .expect("head after recovery");
    assert_eq!(head.head_commit_seq, 2);
    assert_eq!(head.active_permit, None);
    drop(authority);

    // A full reopen confirms the post-recovery effect writes are durable.
    let reopened = SqliteTaskAuthority::open(&database.path).expect("reopen after recovery");
    let head = reopened.inspect_task(task_id()).expect("head after reopen");
    assert_eq!(head.head_commit_seq, 2);
    assert_eq!(head.permit_epoch, 2);
    let slot_a = reopened
        .inspect_effect_slot(permit_a.permit_id, 0)
        .expect("slot A");
    assert_eq!(slot_a.state, SlotState::EffectClosed);
    let slot_b = reopened
        .inspect_effect_slot(permit_b.permit_id, 0)
        .expect("slot B");
    assert_eq!(slot_b.state, SlotState::EffectClosed);
    assert_eq!(raw_count(&database.path, "effect_receipts"), 3);
    assert_integrity(&database.path);
}
