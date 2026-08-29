//! B-WAIT-001 (lane Y2): kill-window / fault-injection matrix for the
//! durable wait registry authority — `WaitAuthority::register_wait`,
//! `notify_commits` (the commit→notify cross-authority window) and
//! `cancel_wait` (the cancel CAS), plus the trigger-guarded tamper surface.
//!
//! Harness and fixtures follow the established matrices exactly, most
//! recently `nlos-topic/tests/topic_fault_injection.rs` (kill-9 children
//! synchronized through piped `READY` markers — never sleeps, `FAULT_LOCK`
//! process-wide serialization, WAL tail truncation sweeps, typed error-chain
//! assertions, raw row counts, `PRAGMA integrity_check` per scenario).
//!
//! **Fault-VFS plumbing (documented harness constraint, same deviation as
//! the channel/topic matrices)**: `WaitAuthority` has no `open_with_vfs`
//! constructor and the workspace forbids `unsafe`, so the shim is routed in
//! through a `SQLite` **URI filename**: `rusqlite`'s `Connection::open` sets
//! `SQLITE_OPEN_URI`, and `WaitAuthority::open` passes
//! `root.join("wait-authority.db")` through unchanged, so a root of
//! `file:<db>?vfs=<shim>&tail=` routes that one authority connection through
//! the registered fault VFS (the appended `/wait-authority.db` tail lands in
//! the ignored `tail=` query parameter). The junk directory that the
//! authority `create_dir_all(root)` call creates for the literal URI path is
//! kept inside a RAII sandbox process CWD — the worktree is never touched.
//! Every reopen / raw reader / integrity check uses the plain default VFS
//! and can never be faulted.
//!
//! **Fault targeting**: every scenario routes exactly ONE authority
//! connection (the wait authority) through the shim; the channel authority
//! always stays on the plain VFS, so a "power loss" on the wait side never
//! disturbs the channel's durable bytes — that asymmetry is precisely what
//! makes the cross-authority "channel enqueue commit → wait notify" window
//! observable.
//!
//! Matrix:
//! - W1 pre-commit IOERR on all three entries (`register_wait`,
//!   `notify_commits`, `cancel_wait`) — typed `Sqlite` error whose chain
//!   names the injected condition, zero phantom rows after reopen (and for
//!   notify: no partial `PENDING -> WOKEN` flip), the disarmed same request
//!   converges, and the durable result replays byte-equal;
//! - W2 pre-commit ENOSPC (`SQLITE_FULL`) on the same three entries — the
//!   same fail-closed convergence;
//! - W3 commit-point `PowerLossAfter` on `register_wait`, both directions:
//!   invisible (Phase A, page-cache loss modeled) — the registration is
//!   wholly absent after reopen and the redo is byte-equal to the phantom
//!   (the `WaitId` is a deterministic authority digest); visible (Phase B,
//!   kill-9 after commit) — both registrations survive whole and replay
//!   byte-equal;
//! - W4 (core) the notify cross-authority kill window: the producer's
//!   channel enqueue is already committed when the wait authority's notify
//!   transaction crashes —
//!   Phase A (`PowerLossAfter`): the notify "reports success" but nothing
//!   is durable: after reopen every wait of the channel is wholly `PENDING`
//!   (redo-able), the notify receipt row is gone with the flip (they live
//!   and die together), and the same-key redo wakes exactly the covered
//!   waits byte-equal to the phantom report;
//!   Phase B (kill-9 after the notify commit): every covered wait is wholly
//!   `WOKEN` (replay-able), the receipt exists, and the same-key replay
//!   returns the original report without re-flipping. There is no partial
//!   flip in either direction — the batched `UPDATE` and the receipt row
//!   commit inside one transaction;
//! - W5 torn WAL tail on the wait authority, one sweep on the register path
//!   and one on the notify path: every representative cut inside the final
//!   transaction frame span leaves the affected rows wholly visible or
//!   wholly invisible (never a half row — the readback validator would fail
//!   closed), the notify sweep additionally asserts the flip+receipt co-life
//!   invariant at every cut, and both paths converge through the same-key
//!   redo/replay onto byte-equal originals;
//! - W6 replay storm — register/notify/cancel same-key replays 3+ times and
//!   once after reopen: byte-equal originals, exactly one row set, conflict
//!   forms keep failing mid-storm;
//! - W7 cancel CAS kill-window — crash before the `PENDING -> CANCELLED`
//!   CAS is durable leaves the wait wholly `PENDING` (redo-able, no
//!   receipt), after it wholly `CANCELLED` (replay-able with the stored
//!   timestamp); a fresh key against a terminal wait always fails
//!   `WaitNotPending`;
//! - W8 trigger guards after recovery — the DDL guards (frozen registration
//!   identity, terminal-state transition abort, no-delete, receipt
//!   immutability) still abort raw tampering on a database that went
//!   through an injected crash and its recovery.
//!
//! **Crash semantics disclaimer** (as in every prior matrix): kill-9
//! simulates *process* crashes; the OS page cache survives process death,
//! so a killed process is NOT a machine power loss. Writes the kernel
//! accepted but the disk never saw are covered by
//! [`FaultMode::PowerLossAfter`] and by file-level WAL tail truncation —
//! both are *models* of a lost write, not real power-cut measurements.
//!
//! `allow: SIZE_OK` — one fault matrix per binary is the established repo
//! shape (all prior `*_fault_injection.rs` files are monolithic); fixtures
//! are duplicated per matrix file by convention.

use std::error::Error as _;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use nlos_channel::{
    ChannelAuthority, ChannelDecision, ChannelRecord, CreateChannelRequest, EnqueueDecision,
    EnqueueRequest, QueueEntryRecord,
};
use nlos_store_fault::{FaultCode, FaultMode};
use nlos_types::{ChannelId, IdempotencyKey};
use nlos_wait::{
    BindingId, CancelDecision, CancelWaitRequest, NotifyCommitsRequest, RegisterDecision,
    RegisterWaitRequest, WaitAuthority, WaitAuthorityError, WaitId, WaitRecord, WaitState,
    WakeReport,
};
use rusqlite::{Connection, params};

const VFS_NAME: &str = "nlos-wait-fault";

const CHANNEL_KEY_SEED: u8 = 0xB0;
const CHANNEL_CREATED_AT_MS: u64 = 900;
const REGISTERED_AT_MS: u64 = 1_000;
const REGISTERED_LATER_AT_MS: u64 = 1_050;
const ENQUEUED_AT_MS: u64 = 1_500;
const NOTIFIED_AT_MS: u64 = 2_000;
const CANCELLED_AT_MS: u64 = 3_000;

static FAULT_LOCK: Mutex<()> = Mutex::new(());
static NEXT: AtomicU64 = AtomicU64::new(1);

fn fault_lock() -> MutexGuard<'static, ()> {
    FAULT_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn key(seed: u8) -> IdempotencyKey {
    IdempotencyKey::from_bytes([seed; 16])
}

fn binding(seed: u8) -> BindingId {
    BindingId::from_bytes([seed; 16])
}

// ---------------------------------------------------------------------------
// request builders and decision unwrap helpers
// ---------------------------------------------------------------------------

fn create_channel(authority: &ChannelAuthority, key_seed: u8) -> ChannelRecord {
    match authority
        .create_channel(CreateChannelRequest {
            capacity_bytes: 4_096,
            policy_digest: [0x44; 32],
            idempotency_key: key(key_seed),
            created_at_ms: CHANNEL_CREATED_AT_MS,
        })
        .expect("create channel")
    {
        ChannelDecision::Created(record) => record,
        ChannelDecision::Replayed(_) => panic!("fresh channel create cannot replay"),
    }
}

fn register_request(
    head: &ChannelRecord,
    waiter: u8,
    target_sequence: u64,
    key_seed: u8,
    registered_at_ms: u64,
) -> RegisterWaitRequest {
    RegisterWaitRequest {
        binding: binding(waiter),
        channel_id: head.channel_id,
        target_sequence,
        idempotency_key: key(key_seed),
        registered_at_ms,
    }
}

fn registered(authority: &WaitAuthority, request: &RegisterWaitRequest) -> WaitRecord {
    match authority.register_wait(*request).expect("register wait") {
        RegisterDecision::Registered(record) => record,
        RegisterDecision::Replayed(_) => panic!("fresh register cannot replay"),
    }
}

fn replayed_register(authority: &WaitAuthority, request: &RegisterWaitRequest) -> WaitRecord {
    match authority.register_wait(*request).expect("register replay") {
        RegisterDecision::Replayed(record) => record,
        RegisterDecision::Registered(_) => panic!("expected Replayed, got Registered"),
    }
}

fn notify_request(
    channel_id: ChannelId,
    up_to_sequence: u64,
    key_seed: u8,
    notified_at_ms: u64,
) -> NotifyCommitsRequest {
    NotifyCommitsRequest {
        channel_id,
        up_to_sequence,
        notified_at_ms,
        idempotency_key: key(key_seed),
    }
}

fn notified(authority: &WaitAuthority, request: &NotifyCommitsRequest) -> WakeReport {
    authority.notify_commits(*request).expect("notify commits")
}

fn cancel_request(wait_id: WaitId, key_seed: u8, cancelled_at_ms: u64) -> CancelWaitRequest {
    CancelWaitRequest {
        wait_id,
        cancelled_at_ms,
        idempotency_key: key(key_seed),
    }
}

fn cancelled(authority: &WaitAuthority, request: &CancelWaitRequest) -> WaitRecord {
    match authority.cancel_wait(*request).expect("cancel wait") {
        CancelDecision::Cancelled(record) => record,
        CancelDecision::Replayed(_) => panic!("fresh cancel cannot replay"),
    }
}

fn replayed_cancel(authority: &WaitAuthority, request: &CancelWaitRequest) -> WaitRecord {
    match authority.cancel_wait(*request).expect("cancel replay") {
        CancelDecision::Replayed(record) => record,
        CancelDecision::Cancelled(_) => panic!("expected Replayed, got Cancelled"),
    }
}

fn enqueue_request(head: &ChannelRecord, payload: &[u8], key_seed: u8, at: u64) -> EnqueueRequest {
    EnqueueRequest {
        channel_id: head.channel_id,
        expected_generation: head.generation,
        expected_fencing_token: head.fencing_token,
        payload: payload.to_vec(),
        idempotency_key: key(key_seed),
        enqueued_at_ms: at,
    }
}

fn enqueued(authority: &ChannelAuthority, request: &EnqueueRequest) -> QueueEntryRecord {
    match authority.enqueue(request.clone()).expect("enqueue") {
        EnqueueDecision::Enqueued(entry) => entry,
        EnqueueDecision::Replayed(_) => panic!("fresh enqueue cannot replay"),
    }
}

/// The `PENDING` row a registration is expected to produce, rebuilt from the
/// fixture constants plus the durable channel snapshot.
fn expected_pending(
    head: &ChannelRecord,
    wait_id: WaitId,
    waiter: u8,
    target_sequence: u64,
    key_seed: u8,
    registered_at_ms: u64,
) -> WaitRecord {
    WaitRecord {
        wait_id,
        binding: binding(waiter),
        channel_id: head.channel_id,
        channel_generation: head.generation,
        channel_fencing_token: head.fencing_token,
        target_sequence,
        state: WaitState::Pending,
        idempotency_key: key(key_seed),
        registered_at_ms,
        woken_at_ms: 0,
        woken_up_to_sequence: 0,
        cancelled_at_ms: 0,
    }
}

fn woken_copy(record: &WaitRecord, notified_at_ms: u64, up_to_sequence: u64) -> WaitRecord {
    let mut woken = record.clone();
    woken.state = WaitState::Woken;
    woken.woken_at_ms = notified_at_ms;
    woken.woken_up_to_sequence = up_to_sequence;
    woken
}

fn cancelled_copy(record: &WaitRecord, cancelled_at_ms: u64) -> WaitRecord {
    let mut cancelled = record.clone();
    cancelled.state = WaitState::Cancelled;
    cancelled.cancelled_at_ms = cancelled_at_ms;
    cancelled
}

// ---------------------------------------------------------------------------
// test roots, sandbox CWD, fault-VFS open (topic-matrix deviation note)
// ---------------------------------------------------------------------------

/// RAII test root: one fresh directory per scenario, removed on drop.
struct TestRoot(PathBuf);

impl TestRoot {
    fn new(label: &str) -> Self {
        let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "nlos-wait-fault-{label}-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&base).expect("create test root");
        Self(base)
    }

    fn base(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// RAII sandbox process CWD. The fault-VFS open runs `create_dir_all(root)`
/// on the literal URI root string, which is a relative OS path; the sandbox
/// keeps that junk directory tree inside a temp directory that is removed on
/// drop, so the worktree stays clean. All fault tests are serialized by
/// `FAULT_LOCK`, and every other test in this binary is either a no-op
/// (`crash_child_helper` without the scenario environment) or uses absolute
/// paths only.
struct SandboxCwd {
    previous: PathBuf,
    directory: PathBuf,
}

impl SandboxCwd {
    fn new(label: &str) -> Self {
        let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "nlos-wait-fault-cwd-{label}-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&directory).expect("create sandbox cwd");
        let previous = std::env::current_dir().expect("capture previous cwd");
        std::env::set_current_dir(&directory).expect("enter sandbox cwd");
        Self {
            previous,
            directory,
        }
    }
}

impl Drop for SandboxCwd {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.previous);
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn wait_database(base: &Path) -> PathBuf {
    base.join("wait-authority.db")
}

fn channel_database(base: &Path) -> PathBuf {
    base.join("channel-authority.db")
}

/// The URI root that routes the wait authority's connection through the
/// registered fault VFS (see the header deviation note).
fn fault_wait_root(base: &Path) -> String {
    format!(
        "file:{}?vfs={VFS_NAME}&tail=",
        wait_database(base).display()
    )
}

fn register_fault_vfs() {
    nlos_store_fault::register(VFS_NAME).expect("register fault vfs");
}

fn open_channel(base: &Path) -> Arc<ChannelAuthority> {
    Arc::new(ChannelAuthority::open(base).expect("open channel authority"))
}

/// Opens the wait authority with its connection on the fault VFS while the
/// channel authority stays on the plain VFS. Pragmas and the migration run
/// while faults are disarmed, so the schema prefix is always durable before
/// any injection.
fn open_wait_fault(base: &Path, channel: &Arc<ChannelAuthority>) -> WaitAuthority {
    register_fault_vfs();
    WaitAuthority::open(fault_wait_root(base), Arc::clone(channel))
        .expect("open wait authority via fault vfs")
}

fn reopen_wait(base: &Path, channel: &Arc<ChannelAuthority>) -> WaitAuthority {
    WaitAuthority::open(base, Arc::clone(channel)).expect("reopen wait authority")
}

// ---------------------------------------------------------------------------
// shared assertions (fault_injection.rs 范式)
// ---------------------------------------------------------------------------

/// Full `Display` chain of a `WaitAuthorityError`, top cause last, for
/// content assertions (e.g. that `SQLITE_FULL`'s message reaches the
/// caller).
fn error_chain(error: &WaitAuthorityError) -> String {
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
/// condition (`"i/o"` / `"ioerr"` / `"full"`): never a fake success, never
/// a panic.
fn assert_sqlite_error_chain(error: &WaitAuthorityError, needles: &[&str]) {
    assert!(
        matches!(error, WaitAuthorityError::Sqlite(_)),
        "expected a storage error, got {error}"
    );
    let chain = error_chain(error).to_lowercase();
    assert!(
        needles.iter().any(|needle| chain.contains(needle)),
        "error chain must name the injected condition, got: {chain}"
    );
}

fn raw_count(database: &Path, sql: &str) -> i64 {
    let connection = Connection::open(database).expect("open raw reader");
    connection
        .query_row(sql, [], |row| row.get(0))
        .expect("count rows")
}

/// Row counts of the three wait tables: `waits`, `channel_notifies`,
/// `wait_cancellations`.
fn assert_wait_counts(base: &Path, expected: [i64; 3]) {
    let tables = ["waits", "channel_notifies", "wait_cancellations"];
    for (table, want) in tables.iter().zip(expected) {
        assert_eq!(
            raw_count(
                &wait_database(base),
                &format!("SELECT COUNT(*) FROM {table}")
            ),
            want,
            "unexpected row count in {table}"
        );
    }
}

fn raw_notify_count(base: &Path) -> i64 {
    raw_count(
        &wait_database(base),
        "SELECT COUNT(*) FROM channel_notifies",
    )
}

fn raw_channel_entry_count(base: &Path) -> i64 {
    raw_count(
        &channel_database(base),
        "SELECT COUNT(*) FROM channel_queue_entries",
    )
}

fn assert_integrity(base: &Path) {
    let connection = Connection::open(wait_database(base)).expect("open for integrity check");
    let result: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("run integrity_check");
    assert_eq!(result, "ok", "integrity_check must pass");
}

// ---------------------------------------------------------------------------
// kill-9 child-process harness (fault_injection.rs 范式)
// ---------------------------------------------------------------------------

fn spawn_child(scenario: &str, root: &TestRoot) -> Child {
    Command::new(std::env::current_exe().expect("current test executable"))
        .args(["--exact", "crash_child_helper", "--nocapture"])
        .env("NLOS_WAIT_CRASH_CHILD_SCENARIO", scenario)
        .env("NLOS_WAIT_CRASH_CHILD_ROOT", root.base().as_os_str())
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

/// Decodes the plain id markers: `READY <channel-id> <wait-id>...`.
struct CrashIds {
    channel_id: ChannelId,
    wait_ids: Vec<WaitId>,
}

fn decode_marker(marker: &str) -> CrashIds {
    let payload = marker.trim().strip_prefix("READY ").expect("marker");
    let parts: Vec<&str> = payload.split(' ').collect();
    assert!(
        parts.len() >= 2,
        "marker must carry the channel id and wait ids"
    );
    CrashIds {
        channel_id: ChannelId::from_bytes(hex_decode16(parts[0])),
        wait_ids: parts[1..]
            .iter()
            .map(|part| WaitId::from_bytes(hex_decode16(part)))
            .collect(),
    }
}

/// Child entry point. Runs only when spawned by a parent test with the
/// scenario environment set; a no-op in the normal test run.
#[test]
fn crash_child_helper() {
    let (Ok(scenario), Ok(root)) = (
        std::env::var("NLOS_WAIT_CRASH_CHILD_SCENARIO"),
        std::env::var("NLOS_WAIT_CRASH_CHILD_ROOT"),
    ) else {
        return;
    };
    let root = PathBuf::from(root);
    match scenario.as_str() {
        "register-two-commit" => child_register_two_commit(&root),
        "notify-commit" => child_notify_commit(&root),
        "cancel-commit" => child_cancel_commit(&root),
        other => panic!("unknown crash child scenario {other}"),
    }
}

fn child_pair(root: &Path) -> (Arc<ChannelAuthority>, WaitAuthority) {
    let channel = open_channel(root);
    let wait = reopen_wait(root, &channel);
    (channel, wait)
}

/// Child fixture: one committed channel plus two fully committed
/// registrations. The wait WAL's final transaction is the second
/// registration. Marker: `READY <channel-id> <wait-id-1> <wait-id-2>`.
fn child_register_two_commit(root: &Path) -> ! {
    let (channel, wait) = child_pair(root);
    let head = create_channel(&channel, CHANNEL_KEY_SEED);
    let first = registered(
        &wait,
        &register_request(&head, 0x11, 3, 0x01, REGISTERED_AT_MS),
    );
    let second = registered(
        &wait,
        &register_request(&head, 0x12, 5, 0x02, REGISTERED_LATER_AT_MS),
    );
    announce(&format!(
        "READY {} {} {}",
        hex_encode(head.channel_id.as_bytes()),
        hex_encode(first.wait_id.as_bytes()),
        hex_encode(second.wait_id.as_bytes()),
    ));
    let _keeper = (channel, wait);
    loop {
        std::thread::park();
    }
}

/// Child fixture: the cross-authority sequence, committed whole — the
/// channel enqueue, three registrations, then the notify (waking the two
/// covered waits, leaving the tail `PENDING`). The wait WAL's final
/// transaction is the notify. Marker:
/// `READY <channel-id> <wait-id-1> <wait-id-2> <wait-id-3>`.
fn child_notify_commit(root: &Path) -> ! {
    let (channel, wait) = child_pair(root);
    let head = create_channel(&channel, CHANNEL_KEY_SEED);
    let entry = enqueued(
        &channel,
        &enqueue_request(&head, b"notify-window", 0xE0, ENQUEUED_AT_MS),
    );
    let first = registered(
        &wait,
        &register_request(&head, 0x11, 2, 0x01, REGISTERED_AT_MS),
    );
    let second = registered(
        &wait,
        &register_request(&head, 0x12, 4, 0x02, REGISTERED_AT_MS + 1),
    );
    let third = registered(
        &wait,
        &register_request(&head, 0x13, 6, 0x03, REGISTERED_AT_MS + 2),
    );
    let report = notified(
        &wait,
        &notify_request(head.channel_id, 4, 0x30, NOTIFIED_AT_MS),
    );
    assert_eq!(report.woken.len(), 2, "the notify covers the first two");
    assert_eq!(entry.sequence, 1);
    announce(&format!(
        "READY {} {} {} {}",
        hex_encode(head.channel_id.as_bytes()),
        hex_encode(first.wait_id.as_bytes()),
        hex_encode(second.wait_id.as_bytes()),
        hex_encode(third.wait_id.as_bytes()),
    ));
    let _keeper = (channel, wait);
    loop {
        std::thread::park();
    }
}

/// Child fixture: one committed channel, one registration, one fully
/// committed cancellation. Marker: `READY <channel-id> <wait-id>`.
fn child_cancel_commit(root: &Path) -> ! {
    let (channel, wait) = child_pair(root);
    let head = create_channel(&channel, CHANNEL_KEY_SEED);
    let only = registered(
        &wait,
        &register_request(&head, 0x11, 3, 0x01, REGISTERED_AT_MS),
    );
    let record = cancelled(&wait, &cancel_request(only.wait_id, 0x40, CANCELLED_AT_MS));
    assert_eq!(record.state, WaitState::Cancelled);
    announce(&format!(
        "READY {} {}",
        hex_encode(head.channel_id.as_bytes()),
        hex_encode(only.wait_id.as_bytes()),
    ));
    let _keeper = (channel, wait);
    loop {
        std::thread::park();
    }
}

// ---------------------------------------------------------------------------
// WAL tail truncation (fault_injection.rs 范式)
// ---------------------------------------------------------------------------

fn sibling_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

/// Returns `(page_size, frame_size, frame_count)` of a WAL image.
fn wal_frame_layout(wal: &[u8]) -> (usize, usize, usize) {
    assert!(wal.len() >= 32, "WAL must have a header");
    let page_size = match u32::from_be_bytes(wal[8..12].try_into().expect("page size field")) {
        1 => 65_536,
        value => value as usize,
    };
    assert!(page_size >= 512, "valid SQLite page size");
    let frame_size = 24 + page_size;
    let frame_count = (wal.len() - 32) / frame_size;
    assert!(frame_count > 0, "fixture must contain frames");
    (page_size, frame_size, frame_count)
}

/// Indices of the frames that carry a commit marker (a non-zero
/// database-size-in-pages field at frame-header offset 4..8 — the last
/// frame of each committed transaction).
fn commit_frames(wal: &[u8]) -> Vec<usize> {
    let (_, frame_size, frame_count) = wal_frame_layout(wal);
    (0..frame_count)
        .filter(|index| {
            let start = 32 + index * frame_size;
            u32::from_be_bytes(wal[start + 4..start + 8].try_into().expect("commit field")) != 0
        })
        .collect()
}

/// Every cut offset that truncates the WAL inside the final `txs_from_end+1`
/// transactions' frame span (from the end of the last surviving commit to
/// the last commit frame inclusive): frame boundaries, half-frame points and
/// last-byte points. `txs_from_end == 0` cuts away the last transaction
/// (torn or whole); `txs_from_end == 1` cuts away the last two.
fn tail_tx_cuts(wal: &[u8], txs_from_end: usize) -> Vec<usize> {
    let (_, frame_size, _) = wal_frame_layout(wal);
    let commits = commit_frames(wal);
    assert!(
        commits.len() >= 2 + txs_from_end,
        "fixture must have enough committed transactions"
    );
    let keep = commits[commits.len() - 2 - txs_from_end];
    let last = commits[commits.len() - 1];
    let mut cuts = vec![32 + (keep + 1) * frame_size];
    for index in (keep + 1)..=last {
        let start = 32 + index * frame_size;
        cuts.push(start);
        cuts.push(start + frame_size / 2);
        cuts.push(start + frame_size - 1);
    }
    cuts.sort_unstable();
    cuts.dedup();
    cuts.retain(|cut| *cut < wal.len());
    cuts
}

/// The on-disk state a killed child leaves behind (wait authority),
/// restorable per sweep iteration so every torn-tail cut starts from
/// identical bytes.
struct WaitSnapshot {
    database: Vec<u8>,
    wal: Vec<u8>,
}

impl WaitSnapshot {
    fn capture(base: &Path) -> Self {
        Self {
            database: fs::read(wait_database(base)).expect("read database"),
            wal: fs::read(sibling_path(&wait_database(base), "-wal")).expect("read wal"),
        }
    }

    /// Restores the database, rewrites the WAL truncated to `cut` (or in
    /// full for `None`), and drops the stale wal-index.
    fn restore(&self, base: &Path, cut: Option<usize>) {
        fs::write(wait_database(base), &self.database).expect("restore database");
        let wal = match cut {
            Some(cut) => &self.wal[..cut],
            None => &self.wal[..],
        };
        fs::write(sibling_path(&wait_database(base), "-wal"), wal).expect("restore wal");
        let _ = fs::remove_file(sibling_path(&wait_database(base), "-shm"));
    }
}

// ---------------------------------------------------------------------------
// W1: pre-commit IOERR fails typed and converges (register entry)
// ---------------------------------------------------------------------------

/// W1（`register_wait`）：`FailWritesAfter { 0, IoErr }` 注入 register 单事务
/// （owner 读back后的 wait 行插入）提交的 WAL 写入 →
/// `WaitAuthorityError::Sqlite` 显式失败（错误链含 I/O 条件）；重开后三表
/// 零行（schema 前缀保留、注册完全不可见）、integrity ok；disarm 后同一请
/// 求重做 → `Registered`；重开后恰好一行、同请求重放逐字节 `Replayed`。
#[test]
#[allow(clippy::too_many_lines)]
fn wait_fault_register_precommit_ioerr_fails_typed_and_converges() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let _sandbox = SandboxCwd::new("ioerr-register");
    let root = TestRoot::new("ioerr-register");
    let channel = open_channel(root.base());
    let head = create_channel(&channel, CHANNEL_KEY_SEED);
    let wait = open_wait_fault(root.base(), &channel);
    let request = register_request(&head, 0x11, 3, 0x01, REGISTERED_AT_MS);

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::IoErr,
    });
    let error = wait
        .register_wait(request)
        .expect_err("register_wait must fail under injected I/O error");
    assert_sqlite_error_chain(&error, &["i/o", "ioerr"]);
    assert!(nlos_store_fault::writes_observed() > 0);
    assert_wait_counts(root.base(), [0, 0, 0]);

    nlos_store_fault::disarm();
    drop(wait);
    let recovered = reopen_wait(root.base(), &channel);
    assert_wait_counts(root.base(), [0, 0, 0]);
    assert!(
        recovered
            .inspect_channel_waits(head.channel_id)
            .expect("list waits after the failed commit")
            .is_empty(),
        "a rejected registration leaves zero durable state"
    );
    assert_integrity(root.base());

    let record = registered(&recovered, &request);
    assert_eq!(record.state, WaitState::Pending);
    assert_eq!(record.target_sequence, 3);
    drop(recovered);
    let verified = reopen_wait(root.base(), &channel);
    assert_wait_counts(root.base(), [1, 0, 0]);
    assert_eq!(
        verified
            .inspect_wait(record.wait_id)
            .expect("wait after redo"),
        record
    );
    assert_eq!(replayed_register(&verified, &request), record);
    assert_wait_counts(root.base(), [1, 0, 0]);
    assert_integrity(root.base());
}

// ---------------------------------------------------------------------------
// W1: pre-commit IOERR (notify entry) — no partial flip
// ---------------------------------------------------------------------------

/// W1（`notify_commits`）：两个注册先行落盘，`FailWritesAfter { 0, IoErr }` 注
/// 入 notify 单事务（批量 `PENDING -> WOKEN` UPDATE + 回执行插入）的提交写
/// 入 → typed `Sqlite` 失败；重开后零幻影行且**无部分翻转**（两行仍整体
/// `PENDING`、零回执）；disarm 后同请求重做 → 恰好唤醒两行、恰好一条回执
/// 与翻转同事务落盘；重开后重放逐字节相等。
#[test]
#[allow(clippy::too_many_lines)]
fn wait_fault_notify_precommit_ioerr_fails_typed_and_converges() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let _sandbox = SandboxCwd::new("ioerr-notify");
    let root = TestRoot::new("ioerr-notify");
    let channel = open_channel(root.base());
    let head = create_channel(&channel, CHANNEL_KEY_SEED);
    let wait = open_wait_fault(root.base(), &channel);
    let early = registered(
        &wait,
        &register_request(&head, 0x11, 2, 0x01, REGISTERED_AT_MS),
    );
    let late = registered(
        &wait,
        &register_request(&head, 0x12, 4, 0x02, REGISTERED_AT_MS),
    );
    let notify = notify_request(head.channel_id, 4, 0x30, NOTIFIED_AT_MS);

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::IoErr,
    });
    let error = wait
        .notify_commits(notify)
        .expect_err("notify_commits must fail under injected I/O error");
    assert_sqlite_error_chain(&error, &["i/o", "ioerr"]);
    assert!(nlos_store_fault::writes_observed() > 0);
    assert_wait_counts(root.base(), [2, 0, 0]);
    let rows = wait
        .inspect_channel_waits(head.channel_id)
        .expect("rows after the failed notify");
    assert!(
        rows.iter()
            .all(|record| record.state == WaitState::Pending && record.woken_at_ms == 0),
        "a failed notify must leave no partial flip"
    );

    nlos_store_fault::disarm();
    drop(wait);
    let recovered = reopen_wait(root.base(), &channel);
    let expected = vec![
        woken_copy(&early, NOTIFIED_AT_MS, 4),
        woken_copy(&late, NOTIFIED_AT_MS, 4),
    ];
    let report = notified(&recovered, &notify);
    assert_eq!(report.woken, expected);
    drop(recovered);
    let verified = reopen_wait(root.base(), &channel);
    assert_wait_counts(root.base(), [2, 1, 0]);
    assert_eq!(
        verified
            .inspect_channel_waits(head.channel_id)
            .expect("rows after redo"),
        expected
    );
    assert_eq!(
        verified.notify_commits(notify).expect("notify replay"),
        WakeReport { woken: expected }
    );
    assert_wait_counts(root.base(), [2, 1, 0]);
    assert_integrity(root.base());
}

// ---------------------------------------------------------------------------
// W1: pre-commit IOERR (cancel entry)
// ---------------------------------------------------------------------------

/// W1（`cancel_wait`）：注册先行落盘，`FailWritesAfter { 0, IoErr }` 注入
/// cancel 单事务（`PENDING -> CANCELLED` CAS + 回执插入）的提交写入 →
/// typed `Sqlite` 失败；重开后 wait 仍整体 `PENDING`、零回执；disarm 后同
/// 请求重做 → `Cancelled`；重开后重放携带存储时间戳逐字节相等。
#[test]
#[allow(clippy::too_many_lines)]
fn wait_fault_cancel_precommit_ioerr_fails_typed_and_converges() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let _sandbox = SandboxCwd::new("ioerr-cancel");
    let root = TestRoot::new("ioerr-cancel");
    let channel = open_channel(root.base());
    let head = create_channel(&channel, CHANNEL_KEY_SEED);
    let wait = open_wait_fault(root.base(), &channel);
    let record = registered(
        &wait,
        &register_request(&head, 0x11, 3, 0x01, REGISTERED_AT_MS),
    );
    let cancel = cancel_request(record.wait_id, 0x40, CANCELLED_AT_MS);

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::IoErr,
    });
    let error = wait
        .cancel_wait(cancel)
        .expect_err("cancel_wait must fail under injected I/O error");
    assert_sqlite_error_chain(&error, &["i/o", "ioerr"]);
    assert!(nlos_store_fault::writes_observed() > 0);
    assert_wait_counts(root.base(), [1, 0, 0]);
    assert_eq!(
        wait.inspect_wait(record.wait_id)
            .expect("wait stays pending"),
        record,
        "a failed cancel must leave the wait wholly PENDING"
    );

    nlos_store_fault::disarm();
    drop(wait);
    let recovered = reopen_wait(root.base(), &channel);
    let redone = cancelled(&recovered, &cancel);
    assert_eq!(redone.state, WaitState::Cancelled);
    assert_eq!(redone.cancelled_at_ms, CANCELLED_AT_MS);
    drop(recovered);
    let verified = reopen_wait(root.base(), &channel);
    assert_wait_counts(root.base(), [1, 0, 1]);
    assert_eq!(
        verified
            .inspect_wait(record.wait_id)
            .expect("wait after redo"),
        cancelled_copy(&record, CANCELLED_AT_MS)
    );
    assert_eq!(
        replayed_cancel(&verified, &cancel_request(record.wait_id, 0x40, 9_999)),
        cancelled_copy(&record, CANCELLED_AT_MS)
    );
    assert_wait_counts(root.base(), [1, 0, 1]);
    assert_integrity(root.base());
}

// ---------------------------------------------------------------------------
// W2: pre-commit ENOSPC (all three entries)
// ---------------------------------------------------------------------------

/// W2：`FailWritesAfter { 0, Full }`（`SQLITE_FULL`）对 register / notify /
/// cancel 三个入口同一收敛——typed 失败链含 "full"、零幻影行（notify 场景
/// 零部分翻转、零回执；cancel 场景 wait 仍 `PENDING`）；disarm 后重做成功、
/// 行恰好一套、重放幂等。
#[test]
#[allow(clippy::too_many_lines)]
fn wait_fault_precommit_enospc_fails_typed_and_converges() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();

    // (a) register under SQLITE_FULL.
    {
        let _sandbox = SandboxCwd::new("full-register");
        let root = TestRoot::new("full-register");
        let channel = open_channel(root.base());
        let head = create_channel(&channel, CHANNEL_KEY_SEED);
        let wait = open_wait_fault(root.base(), &channel);
        let request = register_request(&head, 0x11, 3, 0x01, REGISTERED_AT_MS);

        nlos_store_fault::arm(FaultMode::FailWritesAfter {
            remaining: 0,
            code: FaultCode::Full,
        });
        let error = wait
            .register_wait(request)
            .expect_err("register_wait must fail under injected disk-full");
        assert_sqlite_error_chain(&error, &["full"]);
        assert_wait_counts(root.base(), [0, 0, 0]);

        nlos_store_fault::disarm();
        let record = registered(&wait, &request);
        drop(wait);
        let verified = reopen_wait(root.base(), &channel);
        assert_wait_counts(root.base(), [1, 0, 0]);
        assert_eq!(verified.inspect_wait(record.wait_id).expect("head"), record);
        assert_eq!(replayed_register(&verified, &request), record);
        assert_integrity(root.base());
    }

    // (b) notify under SQLITE_FULL: no partial flip, no receipt.
    {
        let _sandbox = SandboxCwd::new("full-notify");
        let root = TestRoot::new("full-notify");
        let channel = open_channel(root.base());
        let head = create_channel(&channel, CHANNEL_KEY_SEED);
        let wait = open_wait_fault(root.base(), &channel);
        let early = registered(
            &wait,
            &register_request(&head, 0x11, 2, 0x01, REGISTERED_AT_MS),
        );
        let late = registered(
            &wait,
            &register_request(&head, 0x12, 4, 0x02, REGISTERED_AT_MS),
        );
        let notify = notify_request(head.channel_id, 4, 0x30, NOTIFIED_AT_MS);

        nlos_store_fault::arm(FaultMode::FailWritesAfter {
            remaining: 0,
            code: FaultCode::Full,
        });
        let error = wait
            .notify_commits(notify)
            .expect_err("notify_commits must fail under injected disk-full");
        assert_sqlite_error_chain(&error, &["full"]);
        assert_wait_counts(root.base(), [2, 0, 0]);
        assert!(
            wait.inspect_channel_waits(head.channel_id)
                .expect("rows after failed notify")
                .iter()
                .all(|record| record.state == WaitState::Pending)
        );

        nlos_store_fault::disarm();
        let report = notified(&wait, &notify);
        assert_eq!(
            report.woken,
            vec![
                woken_copy(&early, NOTIFIED_AT_MS, 4),
                woken_copy(&late, NOTIFIED_AT_MS, 4)
            ]
        );
        drop(wait);
        let verified = reopen_wait(root.base(), &channel);
        assert_wait_counts(root.base(), [2, 1, 0]);
        assert_eq!(verified.notify_commits(notify).expect("replay"), report);
        assert_integrity(root.base());
    }

    // (c) cancel under SQLITE_FULL: the wait stays wholly PENDING.
    {
        let _sandbox = SandboxCwd::new("full-cancel");
        let root = TestRoot::new("full-cancel");
        let channel = open_channel(root.base());
        let head = create_channel(&channel, CHANNEL_KEY_SEED);
        let wait = open_wait_fault(root.base(), &channel);
        let record = registered(
            &wait,
            &register_request(&head, 0x11, 3, 0x01, REGISTERED_AT_MS),
        );
        let cancel = cancel_request(record.wait_id, 0x40, CANCELLED_AT_MS);

        nlos_store_fault::arm(FaultMode::FailWritesAfter {
            remaining: 0,
            code: FaultCode::Full,
        });
        let error = wait
            .cancel_wait(cancel)
            .expect_err("cancel_wait must fail under injected disk-full");
        assert_sqlite_error_chain(&error, &["full"]);
        assert_wait_counts(root.base(), [1, 0, 0]);
        assert_eq!(
            wait.inspect_wait(record.wait_id)
                .expect("wait stays pending"),
            record
        );

        nlos_store_fault::disarm();
        let redone = cancelled(&wait, &cancel);
        assert_eq!(redone.cancelled_at_ms, CANCELLED_AT_MS);
        drop(wait);
        let verified = reopen_wait(root.base(), &channel);
        assert_wait_counts(root.base(), [1, 0, 1]);
        assert_eq!(
            replayed_cancel(&verified, &cancel_request(record.wait_id, 0x40, 9_999)),
            cancelled_copy(&record, CANCELLED_AT_MS)
        );
        assert_integrity(root.base());
    }
}

// ---------------------------------------------------------------------------
// W3: PowerLossAfter the register commit point, both directions
// ---------------------------------------------------------------------------

/// W3（`register_wait`）：
/// - Phase A（断电不可见方向）：`PowerLossAfter { 0 }` 下 register "报告成
///   功"；重开后三表零行、channel 无 wait 行——不是部分可见；同请求重做 →
///   `Registered` 与幻影记录逐字节相等（WaitId 是确定性权威摘要）；重放幂
///   等。
/// - Phase B（提交后 kill-9 可见方向）：子进程完整提交两个注册后被强杀；
///   重开 → 两行整体可见、字段与常量逐字段一致；同请求重放（时间戳漂移）
///   → `Replayed` 逐字节相等。
#[test]
#[allow(clippy::too_many_lines)]
fn wait_fault_register_power_loss_commit_point_converges_both_ways() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    register_power_loss_invisible_redo_byte_equal();
    register_kill9_after_commit_visible_replay_byte_equal();
}

fn register_power_loss_invisible_redo_byte_equal() {
    let _sandbox = SandboxCwd::new("pl-register");
    let root = TestRoot::new("pl-register");
    let channel = open_channel(root.base());
    let head = create_channel(&channel, CHANNEL_KEY_SEED);
    let wait = open_wait_fault(root.base(), &channel);
    let request = register_request(&head, 0x11, 3, 0x01, REGISTERED_AT_MS);

    nlos_store_fault::arm(FaultMode::PowerLossAfter { remaining: 0 });
    let phantom = registered(&wait, &request);
    nlos_store_fault::disarm();
    // The surviving wait connection keeps a wal-index referencing frames the
    // disk never saw; it must die first (as a real power loss would kill it)
    // so recovery sees durable bytes alone (fault_injection.rs precedent).
    drop(wait);

    let recovered = reopen_wait(root.base(), &channel);
    assert_wait_counts(root.base(), [0, 0, 0]);
    assert!(
        recovered
            .inspect_channel_waits(head.channel_id)
            .expect("no waits after power loss")
            .is_empty()
    );
    assert_integrity(root.base());

    let redone = registered(&recovered, &request);
    assert_eq!(
        redone, phantom,
        "redo must be byte-equal to the silently lost record"
    );
    assert_wait_counts(root.base(), [1, 0, 0]);
    assert_eq!(replayed_register(&recovered, &request), phantom);
    assert_integrity(root.base());
}

fn register_kill9_after_commit_visible_replay_byte_equal() {
    let root = TestRoot::new("kill9-register");
    let mut child = spawn_child("register-two-commit", &root);
    let marker = await_marker(&mut child);
    let ids = decode_marker(&marker);
    kill_and_reap(&mut child);

    let channel = open_channel(root.base());
    let wait = reopen_wait(root.base(), &channel);
    let head = channel
        .inspect_channel(ids.channel_id)
        .expect("channel survives the kill");
    let rows = wait
        .inspect_channel_waits(ids.channel_id)
        .expect("waits must survive the kill");
    let expected = vec![
        expected_pending(&head, ids.wait_ids[0], 0x11, 3, 0x01, REGISTERED_AT_MS),
        expected_pending(
            &head,
            ids.wait_ids[1],
            0x12,
            5,
            0x02,
            REGISTERED_LATER_AT_MS,
        ),
    ];
    assert_eq!(rows, expected, "both registrations survive whole");
    assert_wait_counts(root.base(), [2, 0, 0]);

    // Visible direction: the exact requests replay byte-equal (the replayed
    // request may drift its registration timestamp — authority state).
    assert_eq!(
        replayed_register(&wait, &register_request(&head, 0x11, 3, 0x01, 9_999)),
        rows[0]
    );
    assert_eq!(
        replayed_register(&wait, &register_request(&head, 0x12, 5, 0x02, 9_999)),
        rows[1]
    );
    assert_wait_counts(root.base(), [2, 0, 0]);
    assert_integrity(root.base());
}

// ---------------------------------------------------------------------------
// W4 (core): the notify cross-authority kill window
// ---------------------------------------------------------------------------

/// W4（核心，commit→notify 跨权威窗口，Phase A 断电不可见方向）：channel 的
/// enqueue 已提交落盘后，`PowerLossAfter { 0 }` 注入 wait 侧 notify 事务——
/// notify "报告成功"但翻转与回执全部未落盘。重开后：channel 恰好一条已提
/// 交 entry（跨权威侧不受扰动）、wait 行**全部整体 `PENDING`（可重做
/// notify）**、零回执（翻转与回执行同生同灭——批量 UPDATE 与回执同事务，
/// 无部分翻转）；同 key 重做 → 恰好唤醒被覆盖的两行、回执与翻转同事务落
/// 盘、report 与幻影逐字节相等；再重放 → 幂等。
#[test]
#[allow(clippy::too_many_lines)]
fn wait_fault_notify_window_power_loss_redoes_all_pending() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let _sandbox = SandboxCwd::new("pl-notify");
    let root = TestRoot::new("pl-notify");
    let channel = open_channel(root.base());
    let head = create_channel(&channel, CHANNEL_KEY_SEED);
    let entry = enqueued(
        &channel,
        &enqueue_request(&head, b"notify-window", 0xE0, ENQUEUED_AT_MS),
    );
    let wait = open_wait_fault(root.base(), &channel);
    let first = registered(
        &wait,
        &register_request(&head, 0x11, 2, 0x01, REGISTERED_AT_MS),
    );
    let second = registered(
        &wait,
        &register_request(&head, 0x12, 4, 0x02, REGISTERED_AT_MS + 1),
    );
    let tail = registered(
        &wait,
        &register_request(&head, 0x13, 6, 0x03, REGISTERED_AT_MS + 2),
    );
    let notify = notify_request(head.channel_id, 4, 0x30, NOTIFIED_AT_MS);

    // The channel enqueue is already durable; the crash lands inside the
    // wait authority's notify transaction — the cross-authority window.
    nlos_store_fault::arm(FaultMode::PowerLossAfter { remaining: 0 });
    let phantom = notified(&wait, &notify);
    assert_eq!(phantom.woken.len(), 2);
    nlos_store_fault::disarm();
    drop(wait);

    let recovered = reopen_wait(root.base(), &channel);
    assert_eq!(raw_channel_entry_count(root.base()), 1);
    assert_eq!(entry.sequence, 1);
    assert_wait_counts(root.base(), [3, 0, 0]);
    let rows = recovered
        .inspect_channel_waits(head.channel_id)
        .expect("rows after power loss");
    assert_eq!(
        rows,
        vec![first.clone(), second.clone(), tail.clone()],
        "every wait is wholly PENDING — no partial flip"
    );
    assert!(rows.iter().all(|record| record.state == WaitState::Pending));
    assert_integrity(root.base());

    // Redo the same key: the flip and its receipt land together, byte-equal
    // to the phantom report.
    let redone = notified(&recovered, &notify);
    assert_eq!(
        redone, phantom,
        "redo must be byte-equal to the silently lost report"
    );
    assert_wait_counts(root.base(), [3, 1, 0]);
    let rows = recovered
        .inspect_channel_waits(head.channel_id)
        .expect("rows after redo");
    assert_eq!(
        rows,
        vec![
            woken_copy(&first, NOTIFIED_AT_MS, 4),
            woken_copy(&second, NOTIFIED_AT_MS, 4),
            tail.clone(),
        ],
        "exactly the covered waits are WOKEN; the tail is untouched"
    );
    assert_eq!(
        notified(&recovered, &notify_request(head.channel_id, 4, 0x30, 9_999)),
        redone,
        "follow-up replay is idempotent"
    );
    assert_wait_counts(root.base(), [3, 1, 0]);
    assert_integrity(root.base());
}

/// W4（核心，Phase B kill-9 可见方向）：子进程完整提交「enqueue → 三注册 →
/// notify」后被强杀；重开后：channel 恰好一条 entry、恰好一条 notify 回执、
/// 被覆盖两行**整体 `WOKEN`**（wake 字段精确）、尾行 `PENDING`——全 `WOKEN`
/// （可重放）分支；同 key notify 重放返回原 report（不重翻转）；fresh key
/// 同范围 notify 翻转零行（terminal 行不可再触）；integrity ok。
#[test]
#[allow(clippy::too_many_lines)]
fn wait_fault_notify_window_kill9_replays_original_report() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let root = TestRoot::new("kill9-notify");
    let mut child = spawn_child("notify-commit", &root);
    let marker = await_marker(&mut child);
    let ids = decode_marker(&marker);
    kill_and_reap(&mut child);

    let channel = open_channel(root.base());
    let wait = reopen_wait(root.base(), &channel);
    let head = channel
        .inspect_channel(ids.channel_id)
        .expect("channel survives the kill");
    // Cross-authority co-life, lived side: the committed enqueue is whole,
    // the notify receipt exists, and exactly the covered waits are WOKEN.
    assert_eq!(raw_channel_entry_count(root.base()), 1);
    assert_eq!(
        channel
            .inspect_queue(ids.channel_id)
            .expect("queue state")
            .max_sequence,
        1
    );
    assert_wait_counts(root.base(), [3, 1, 0]);
    let pending = [
        expected_pending(&head, ids.wait_ids[0], 0x11, 2, 0x01, REGISTERED_AT_MS),
        expected_pending(&head, ids.wait_ids[1], 0x12, 4, 0x02, REGISTERED_AT_MS + 1),
        expected_pending(&head, ids.wait_ids[2], 0x13, 6, 0x03, REGISTERED_AT_MS + 2),
    ];
    let rows = wait
        .inspect_channel_waits(ids.channel_id)
        .expect("waits survive the kill");
    let expected = vec![
        woken_copy(&pending[0], NOTIFIED_AT_MS, 4),
        woken_copy(&pending[1], NOTIFIED_AT_MS, 4),
        pending[2].clone(),
    ];
    assert_eq!(
        rows, expected,
        "covered waits are wholly WOKEN, the tail wholly PENDING"
    );

    // kill-9 后同 key notify 重放返回原 report，不重翻转。
    let replayed = notified(&wait, &notify_request(ids.channel_id, 4, 0x30, 9_999));
    assert_eq!(
        replayed.woken,
        vec![expected[0].clone(), expected[1].clone()]
    );
    assert_wait_counts(root.base(), [3, 1, 0]);
    // A fresh key over the same range flips nothing: WOKEN is terminal. The
    // empty report is still durably recorded under its own key, so the
    // receipt count grows to exactly two while the rows stay untouched.
    assert!(
        notified(&wait, &notify_request(ids.channel_id, 4, 0x31, 9_999))
            .woken
            .is_empty()
    );
    assert_eq!(
        wait.inspect_channel_waits(ids.channel_id)
            .expect("rows after re-notify"),
        expected
    );
    assert_wait_counts(root.base(), [3, 2, 0]);
    assert_integrity(root.base());
}

// ---------------------------------------------------------------------------
// W5: torn WAL tail (register path)
// ---------------------------------------------------------------------------

/// W5（register 路径）：子进程提交两个注册后被强杀，父进程对 wait WAL 末段
/// 事务帧组的每个截断点与再深一段的截断点（合计 ≥6 个代表点）逐一恢复重
/// 开：可见行恒为控制列表的**逐字节前缀**（行整体可见或整体不可见，绝无
/// 半行——decode 校验会 fail-closed）；每个截断点下重做缺失注册 → 与控制
/// 记录逐字节相等；重放幂等；完整恢复对照逐字节相等。
#[test]
#[allow(clippy::too_many_lines)]
fn wait_fault_register_torn_wal_tail_rows_are_whole_or_absent() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let root = TestRoot::new("torn-register");
    let mut child = spawn_child("register-two-commit", &root);
    let marker = await_marker(&mut child);
    let ids = decode_marker(&marker);
    kill_and_reap(&mut child);

    let snapshot = WaitSnapshot::capture(root.base());
    let channel_id = ids.channel_id;

    // Visible control: the untouched WAL recovers both registrations whole.
    let (head, control) = {
        let channel = open_channel(root.base());
        let wait = reopen_wait(root.base(), &channel);
        let head = channel.inspect_channel(channel_id).expect("control head");
        let rows = wait
            .inspect_channel_waits(channel_id)
            .expect("control rows");
        assert_eq!(
            rows,
            vec![
                expected_pending(&head, ids.wait_ids[0], 0x11, 3, 0x01, REGISTERED_AT_MS),
                expected_pending(
                    &head,
                    ids.wait_ids[1],
                    0x12,
                    5,
                    0x02,
                    REGISTERED_LATER_AT_MS
                ),
            ]
        );
        drop(wait);
        drop(channel);
        (head, rows)
    };
    let requests = [
        (
            register_request(&head, 0x11, 3, 0x01, REGISTERED_AT_MS),
            control[0].clone(),
        ),
        (
            register_request(&head, 0x12, 5, 0x02, REGISTERED_LATER_AT_MS),
            control[1].clone(),
        ),
    ];

    let mut cuts = [
        tail_tx_cuts(&snapshot.wal, 0),
        tail_tx_cuts(&snapshot.wal, 1),
    ]
    .concat();
    cuts.sort_unstable();
    cuts.dedup();
    assert!(
        cuts.len() >= 6,
        "sweep must cover at least 6 representative cut points, got {cuts:?}"
    );

    for cut in cuts {
        snapshot.restore(root.base(), Some(cut));
        let channel = open_channel(root.base());
        let wait = reopen_wait(root.base(), &channel);
        assert_integrity(root.base());
        let rows = wait
            .inspect_channel_waits(channel_id)
            .expect("rows after torn tail");
        assert_eq!(
            rows,
            control[..rows.len()],
            "visible rows must be whole control rows (byte-equal prefix)"
        );

        // Redo the missing registrations; each converges byte-equal.
        for (index, (request, expected)) in requests.iter().enumerate().skip(rows.len()) {
            assert_eq!(
                registered(&wait, request),
                *expected,
                "redo of registration {index} must be byte-equal"
            );
        }
        assert_wait_counts(root.base(), [2, 0, 0]);
        let rows = wait
            .inspect_channel_waits(channel_id)
            .expect("rows after redo");
        assert_eq!(rows, control);
        for (request, expected) in &requests {
            assert_eq!(replayed_register(&wait, request), *expected);
        }
        assert_wait_counts(root.base(), [2, 0, 0]);
        assert_integrity(root.base());
        drop(wait);
        drop(channel);
    }

    // Full restore returns to the visible world.
    snapshot.restore(root.base(), None);
    let channel = open_channel(root.base());
    let wait = reopen_wait(root.base(), &channel);
    assert_eq!(
        wait.inspect_channel_waits(channel_id)
            .expect("rows after full restore"),
        control
    );
    assert_integrity(root.base());
}

// ---------------------------------------------------------------------------
// W5: torn WAL tail (notify path) — flip and receipt live and die together
// ---------------------------------------------------------------------------

/// W5（notify 路径）：子进程完整提交「enqueue → 三注册 → notify」后被强杀，
/// wait WAL 末段事务帧组（notify 事务）的每个截断点与再深一段的截断点逐一
/// 恢复重开：
/// - notify 事务**从不存活**于任何截断点：零回执行，且被覆盖行**全部整体
///   `PENDING`**（零部分翻转）——翻转与回执行同生同灭的「死」方向（「生」
///   方向由 kill-9 场景证明）；
/// - 注册行恒为控制列表的逐字节前缀；channel 侧已提交 enqueue 恒为恰好一
///   条（wait 侧截断不扰动 channel 文件）；
/// - 每个截断点同一收敛路径：重做缺失注册（逐字节相等）→ 同 key notify 重
///   做/重放（两分支同一调用）→ report 逐字节等于控制 report、恰好一条回
///   执；完整恢复对照逐字节相等。
#[test]
#[allow(clippy::too_many_lines)]
fn wait_fault_notify_torn_wal_tail_flip_and_receipt_die_together() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let root = TestRoot::new("torn-notify");
    let mut child = spawn_child("notify-commit", &root);
    let marker = await_marker(&mut child);
    let ids = decode_marker(&marker);
    kill_and_reap(&mut child);

    let snapshot = WaitSnapshot::capture(root.base());
    let channel_id = ids.channel_id;
    let notify = notify_request(channel_id, 4, 0x30, NOTIFIED_AT_MS);

    // Visible control: flip + receipt + untouched enqueue, all whole.
    let (control_registrations, control_woken, control_requests) = {
        let channel = open_channel(root.base());
        let wait = reopen_wait(root.base(), &channel);
        let head = channel.inspect_channel(channel_id).expect("control head");
        let pending = [
            expected_pending(&head, ids.wait_ids[0], 0x11, 2, 0x01, REGISTERED_AT_MS),
            expected_pending(&head, ids.wait_ids[1], 0x12, 4, 0x02, REGISTERED_AT_MS + 1),
            expected_pending(&head, ids.wait_ids[2], 0x13, 6, 0x03, REGISTERED_AT_MS + 2),
        ];
        let rows = wait
            .inspect_channel_waits(channel_id)
            .expect("control rows");
        let woken = vec![
            woken_copy(&pending[0], NOTIFIED_AT_MS, 4),
            woken_copy(&pending[1], NOTIFIED_AT_MS, 4),
        ];
        // The child committed the notify before the kill, so the visible
        // control is the WOKEN world (the exact replay direction).
        assert_eq!(
            rows,
            [woken[0].clone(), woken[1].clone(), pending[2].clone()]
        );
        assert_eq!(raw_notify_count(root.base()), 1);
        assert_eq!(raw_channel_entry_count(root.base()), 1);
        drop(wait);
        drop(channel);
        let requests = [
            register_request(&head, 0x11, 2, 0x01, REGISTERED_AT_MS),
            register_request(&head, 0x12, 4, 0x02, REGISTERED_AT_MS + 1),
            register_request(&head, 0x13, 6, 0x03, REGISTERED_AT_MS + 2),
        ];
        (pending.to_vec(), woken, requests)
    };

    let mut cuts = [
        tail_tx_cuts(&snapshot.wal, 0),
        tail_tx_cuts(&snapshot.wal, 1),
    ]
    .concat();
    cuts.sort_unstable();
    cuts.dedup();
    assert!(
        cuts.len() >= 6,
        "sweep must cover at least 6 representative cut points, got {cuts:?}"
    );

    for cut in cuts {
        snapshot.restore(root.base(), Some(cut));
        let channel = open_channel(root.base());
        let wait = reopen_wait(root.base(), &channel);
        assert_eq!(
            raw_channel_entry_count(root.base()),
            1,
            "wait-side truncation never disturbs the committed enqueue"
        );
        assert_integrity(root.base());

        // Registration rows survive as a byte-equal prefix of the control.
        let rows = wait
            .inspect_channel_waits(channel_id)
            .expect("rows after torn tail");
        assert_eq!(
            rows,
            control_registrations[..rows.len()],
            "visible rows must be whole control rows"
        );
        // The notify transaction never survives a cut: zero receipt rows and
        // no partial flip — flip and receipt died together.
        assert_eq!(
            raw_notify_count(root.base()),
            0,
            "the notify receipt dies with the flip"
        );
        assert!(
            rows.iter()
                .all(|record| record.state == WaitState::Pending && record.woken_at_ms == 0),
            "no partial wake may survive"
        );

        // Redo the missing registrations byte-equal, then redo the notify:
        // the same key wakes exactly the covered pair.
        for (index, (request, expected)) in control_requests
            .iter()
            .zip(control_registrations.iter())
            .enumerate()
            .skip(rows.len())
        {
            assert_eq!(
                registered(&wait, request),
                *expected,
                "redo of registration {index} must be byte-equal"
            );
        }
        let report = notified(&wait, &notify);
        assert_eq!(report.woken, control_woken);
        assert_wait_counts(root.base(), [3, 1, 0]);
        let rows = wait
            .inspect_channel_waits(channel_id)
            .expect("rows after convergence");
        assert_eq!(
            rows,
            [
                control_woken[0].clone(),
                control_woken[1].clone(),
                control_registrations[2].clone()
            ]
        );
        assert_eq!(
            notified(&wait, &notify_request(channel_id, 4, 0x30, 9_999)),
            report
        );
        assert_wait_counts(root.base(), [3, 1, 0]);
        assert_integrity(root.base());
        drop(wait);
        drop(channel);
    }

    // Full restore returns to the visible world.
    snapshot.restore(root.base(), None);
    let channel = open_channel(root.base());
    let wait = reopen_wait(root.base(), &channel);
    assert_eq!(
        wait.inspect_channel_waits(channel_id)
            .expect("rows after full restore"),
        [
            control_woken[0].clone(),
            control_woken[1].clone(),
            control_registrations[2].clone()
        ]
    );
    assert_integrity(root.base());
}

// ---------------------------------------------------------------------------
// W6: replay storm
// ---------------------------------------------------------------------------

/// W6：register / notify / cancel 同 key 各连放 3 次 + 重开后再各放 1 次 →
/// 每次返回与原始 durable 记录/report **逐字节相等**（重放携带漂移时间戳仍
/// 返回原记录）；恰好一套行（waits 2、回执各 1）；中途冲突请求恒
/// `IdempotencyConflict`；integrity ok。
#[test]
#[allow(clippy::too_many_lines)]
fn wait_fault_replay_storm_register_notify_cancel_no_duplicates() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let root = TestRoot::new("storm");
    let channel = open_channel(root.base());
    let wait = reopen_wait(root.base(), &channel);
    let head = create_channel(&channel, CHANNEL_KEY_SEED);
    let first = registered(
        &wait,
        &register_request(&head, 0x11, 2, 0x01, REGISTERED_AT_MS),
    );
    let second = registered(
        &wait,
        &register_request(&head, 0x12, 2, 0x02, REGISTERED_AT_MS),
    );
    let receipt = cancelled(
        &wait,
        &cancel_request(second.wait_id, 0x40, CANCELLED_AT_MS),
    );
    let notify = notify_request(head.channel_id, 2, 0x30, NOTIFIED_AT_MS);
    let report = notified(&wait, &notify);
    assert_eq!(
        report.woken,
        vec![woken_copy(&first, NOTIFIED_AT_MS, 2)],
        "the cancelled wait is skipped by the wake"
    );
    // The register replay truth is the CURRENT durable row: the notify above
    // legitimately moved it to WOKEN, so same-key replays return the woken
    // row (topic-matrix precedent — replay reflects the durable row, the
    // registration identity fields stay frozen).
    let first_current = wait.inspect_wait(first.wait_id).expect("current row");
    assert_eq!(first_current, woken_copy(&first, NOTIFIED_AT_MS, 2));

    for _ in 0..3 {
        assert_eq!(
            replayed_register(&wait, &register_request(&head, 0x11, 2, 0x01, 9_999)),
            first_current
        );
        assert_eq!(
            replayed_cancel(&wait, &cancel_request(second.wait_id, 0x40, 9_999)),
            receipt
        );
        assert_eq!(notified(&wait, &notify), report);
    }
    // Conflicting forms keep failing mid-storm.
    assert!(matches!(
        wait.register_wait(register_request(&head, 0x11, 5, 0x01, 9_999)),
        Err(WaitAuthorityError::IdempotencyConflict)
    ));
    assert!(matches!(
        wait.notify_commits(notify_request(head.channel_id, 3, 0x30, 9_999)),
        Err(WaitAuthorityError::IdempotencyConflict)
    ));
    assert!(matches!(
        wait.cancel_wait(cancel_request(first.wait_id, 0x40, 9_999)),
        Err(WaitAuthorityError::IdempotencyConflict)
    ));

    drop(wait);
    let verified = reopen_wait(root.base(), &channel);
    assert_eq!(
        replayed_register(&verified, &register_request(&head, 0x11, 2, 0x01, 9_999)),
        first_current,
        "replay after reopen stays byte-equal"
    );
    assert_eq!(
        replayed_cancel(&verified, &cancel_request(second.wait_id, 0x40, 9_999)),
        receipt
    );
    assert_eq!(notified(&verified, &notify), report);
    assert_wait_counts(root.base(), [2, 1, 1]);
    assert_integrity(root.base());
}

// ---------------------------------------------------------------------------
// W7: cancel CAS kill-window
// ---------------------------------------------------------------------------

/// W7（cancel CAS kill-window）：cancel 单事务在 `PENDING -> CANCELLED`
/// CAS 持久化前后崩溃只有两个合法终点——Phase A（`PowerLossAfter { 0 }`，
/// CAS 未持久化）wait 处于**整体 `PENDING`**（可重做）、零回执，raw 行级与
/// API 双重断言；重做恰一次跨过 CAS → `Cancelled` 与幻影逐字节相等；之后
/// 同请求重放携带原时间戳逐字节相等；对已 `WOKEN` 行 cancel 恒
/// `WaitNotPending`。Phase B（kill-9 可见方向）：子进程完整提交 cancel 后
/// 被强杀；重开 → wait 整体 `CANCELLED`、恰好一条回执；重放携带存储时间戳
/// 逐字节相等；fresh key 恒 `WaitNotPending(Cancelled)`。
#[test]
#[allow(clippy::too_many_lines)]
fn wait_fault_cancel_cas_kill_window_has_no_intermediate_state() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    cancel_power_loss_before_cas_redo_converges();
    cancel_kill9_after_commit_replays_receipt();
}

fn cancel_power_loss_before_cas_redo_converges() {
    let _sandbox = SandboxCwd::new("pl-cancel");
    let root = TestRoot::new("pl-cancel");
    let channel = open_channel(root.base());
    let head = create_channel(&channel, CHANNEL_KEY_SEED);
    let wait = open_wait_fault(root.base(), &channel);
    let record = registered(
        &wait,
        &register_request(&head, 0x11, 3, 0x01, REGISTERED_AT_MS),
    );
    let cancel = cancel_request(record.wait_id, 0x40, CANCELLED_AT_MS);

    // Crash before the CAS is durable: the phantom cancel "succeeded" but
    // nothing reached the disk.
    nlos_store_fault::arm(FaultMode::PowerLossAfter { remaining: 0 });
    let phantom = cancelled(&wait, &cancel);
    assert_eq!(phantom.state, WaitState::Cancelled);
    nlos_store_fault::disarm();
    drop(wait);

    let recovered = reopen_wait(root.base(), &channel);
    assert_wait_counts(root.base(), [1, 0, 0]);
    let rows = recovered
        .inspect_channel_waits(head.channel_id)
        .expect("rows after power loss");
    assert_eq!(
        rows,
        vec![record.clone()],
        "the cancel vanished whole: the wait stays wholly PENDING"
    );
    assert_integrity(root.base());

    // Redo crosses the CAS exactly once; the receipt is byte-equal.
    let redone = cancelled(&recovered, &cancel);
    assert_eq!(
        redone, phantom,
        "redo must be byte-equal to the silently lost record"
    );
    assert_wait_counts(root.base(), [1, 0, 1]);
    assert_eq!(
        replayed_cancel(&recovered, &cancel_request(record.wait_id, 0x40, 9_999)),
        phantom
    );

    // A woken wait can never be retroactively cancelled, window or not.
    let woken = registered(
        &recovered,
        &register_request(&head, 0x12, 5, 0x02, REGISTERED_AT_MS),
    );
    notified(
        &recovered,
        &notify_request(head.channel_id, 5, 0x31, NOTIFIED_AT_MS + 1),
    );
    assert!(matches!(
        recovered.cancel_wait(cancel_request(woken.wait_id, 0x41, CANCELLED_AT_MS + 1)),
        Err(WaitAuthorityError::WaitNotPending(WaitState::Woken))
    ));
    assert_wait_counts(root.base(), [2, 1, 1]);
    assert_integrity(root.base());
}

fn cancel_kill9_after_commit_replays_receipt() {
    let root = TestRoot::new("kill9-cancel");
    let mut child = spawn_child("cancel-commit", &root);
    let marker = await_marker(&mut child);
    let ids = decode_marker(&marker);
    kill_and_reap(&mut child);

    let channel = open_channel(root.base());
    let wait = reopen_wait(root.base(), &channel);
    let head = channel
        .inspect_channel(ids.channel_id)
        .expect("channel survives the kill");
    let rows = wait
        .inspect_channel_waits(ids.channel_id)
        .expect("wait survives the kill");
    let expected = vec![cancelled_copy(
        &expected_pending(&head, ids.wait_ids[0], 0x11, 3, 0x01, REGISTERED_AT_MS),
        CANCELLED_AT_MS,
    )];
    assert_eq!(rows, expected, "the cancellation survives whole");
    assert_wait_counts(root.base(), [1, 0, 1]);

    // Replaying the exact key returns the stored timestamp byte-equal.
    assert_eq!(
        replayed_cancel(&wait, &cancel_request(ids.wait_ids[0], 0x40, 9_999)),
        expected[0]
    );
    // A fresh key against the terminal row fails closed.
    assert!(matches!(
        wait.cancel_wait(cancel_request(ids.wait_ids[0], 0x41, CANCELLED_AT_MS + 1)),
        Err(WaitAuthorityError::WaitNotPending(WaitState::Cancelled))
    ));
    assert_wait_counts(root.base(), [1, 0, 1]);
    assert_integrity(root.base());
}

// ---------------------------------------------------------------------------
// W8: trigger guards survive injection and recovery
// ---------------------------------------------------------------------------

/// W8：一次注入崩溃（notify 窗口断电 + 恢复 + 重做）之后，raw `UPDATE` 的
/// 非法状态翻转仍被 DDL 守卫 abort——terminal 行不可回翻、注册身份字段冻
/// 结、wait 行不可删除、notify 回执行不可改不可删；守卫下的权威读路径照常
/// 服务、wake 字段未被扰动。
#[test]
#[allow(clippy::too_many_lines)]
fn wait_fault_trigger_guards_survive_injection_and_recovery() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let _sandbox = SandboxCwd::new("guards");
    let root = TestRoot::new("guards");
    let channel = open_channel(root.base());
    let head = create_channel(&channel, CHANNEL_KEY_SEED);
    let wait = open_wait_fault(root.base(), &channel);
    let record = registered(
        &wait,
        &register_request(&head, 0x11, 3, 0x01, REGISTERED_AT_MS),
    );

    // Crash through the notify window: the WOKEN flip and receipt are lost
    // together, then redone — the recovered database is a post-crash
    // recovery product.
    nlos_store_fault::arm(FaultMode::PowerLossAfter { remaining: 0 });
    let phantom = notified(
        &wait,
        &notify_request(head.channel_id, 3, 0x30, NOTIFIED_AT_MS),
    );
    nlos_store_fault::disarm();
    drop(wait);
    let recovered = reopen_wait(root.base(), &channel);
    assert_wait_counts(root.base(), [1, 0, 0]);
    let redone = notified(
        &recovered,
        &notify_request(head.channel_id, 3, 0x30, NOTIFIED_AT_MS),
    );
    assert_eq!(redone, phantom);
    assert_wait_counts(root.base(), [1, 1, 0]);
    let woken = recovered
        .inspect_wait(record.wait_id)
        .expect("woken row after recovery");
    assert_eq!(woken.state, WaitState::Woken);

    let raw = Connection::open(wait_database(root.base())).expect("open raw connection");
    // An illegal state flip out of terminal WOKEN aborts (trigger guard).
    assert!(
        raw.execute(
            "UPDATE waits SET status=0 WHERE wait_id=?1",
            [record.wait_id.as_bytes().as_slice()]
        )
        .is_err(),
        "terminal rows can never transition again"
    );
    // The registration identity fields are frozen by trigger.
    assert!(
        raw.execute(
            "UPDATE waits SET binding_id=?1 WHERE wait_id=?2",
            params![
                binding(0x99).as_bytes().as_slice(),
                record.wait_id.as_bytes().as_slice()
            ],
        )
        .is_err(),
        "wait binding is trigger-frozen"
    );
    assert!(
        raw.execute(
            "UPDATE waits SET registered_at_ms=1 WHERE wait_id=?1",
            [record.wait_id.as_bytes().as_slice()]
        )
        .is_err(),
        "registration timestamp is trigger-frozen"
    );
    // Durable rows can never be deleted.
    assert!(
        raw.execute(
            "DELETE FROM waits WHERE wait_id=?1",
            [record.wait_id.as_bytes().as_slice()]
        )
        .is_err()
    );
    // The notify receipt is immutable and durable.
    assert!(
        raw.execute("UPDATE channel_notifies SET up_to_sequence=9", [])
            .is_err()
    );
    assert!(raw.execute("DELETE FROM channel_notifies", []).is_err());

    // The guarded registry still serves reads; the wake fields are untouched.
    assert_eq!(
        recovered.inspect_wait(record.wait_id).expect("inspect"),
        woken
    );
    assert_wait_counts(root.base(), [1, 1, 0]);
    assert_integrity(root.base());
}
