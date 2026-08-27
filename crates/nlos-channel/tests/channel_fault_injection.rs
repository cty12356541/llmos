//! B-CHANNEL-001 (lane F): kill-window / fault-injection matrix for the
//! durable Channel endpoint authority — `ChannelAuthority::create_channel`,
//! `rotate_channel`, `inspect_channel`, `inspect_endpoint_proof`.
//!
//! Harness and fixtures follow the established matrices exactly:
//! `nlos-task/tests/fault_injection.rs`,
//! `nlos-task/tests/resource_bridge_fault_injection.rs` (kill-9 children
//! synchronized through piped `READY` markers — never sleeps, `FAULT_LOCK`
//! process-wide serialization, WAL tail truncation, typed error-chain
//! assertions, raw table counts, `PRAGMA integrity_check` per scenario) and
//! `nlos-resource/tests/finalize_fault_injection.rs` (W1–W6 matrix row
//! shape, Phase A/B power-loss structure).
//!
//! **Fault-VFS plumbing deviation (documented harness constraint)**:
//! unlike `SqliteTaskAuthority` / `ResourceAuthority` / `SemanticAuthority`,
//! `ChannelAuthority` has no `open_with_vfs` constructor, and the workspace
//! forbids `unsafe`, so the shim cannot be installed as the process-default
//! VFS from a test. Instead this harness routes the authority connection
//! through the shim with a `SQLite` **URI filename**: `rusqlite`'s
//! `Connection::open` sets `SQLITE_OPEN_URI`, and `ChannelAuthority::open`
//! passes `root.join("channel-authority.db")` through unchanged, so a root
//! of `file:<db>?vfs=<shim>&tail=` makes the authority connection use the
//! registered fault VFS (the appended `/channel-authority.db` tail lands in
//! the ignored `tail=` query parameter; `SQLite` silently discards
//! unrecognized query parameters). The junk directory that
//! `ChannelAuthority::open`'s `create_dir_all(root)` creates for the literal
//! URI path is kept inside a RAII sandbox process CWD — the worktree is
//! never touched. Every reopen / raw reader / integrity check uses the plain
//! default VFS and can never be faulted.
//!
//! **Fault targeting**: every injected fault targets the single
//! `channel-authority.db` connection that the authority under test owns;
//! each scenario owns a fresh root, so no other authority exists to disturb.
//! The matrix proves **durable-prefix idempotent convergence** of the
//! create/rotate durable single `BEGIN IMMEDIATE` transactions — head,
//! identity, generation, rotation receipt and endpoint proof rows live and
//! die together — never cross-authority atomicity.
//!
//! Matrix (window × scenario):
//! - W1 pre-commit IOERR (create + rotate) — typed `Sqlite` error whose
//!   chain names the injected condition, zero durable rows (invisible),
//!   reopen keeps the prefix, unfaulted retry of the same request converges;
//! - W2 pre-commit ENOSPC (create) — same convergence with `SQLITE_FULL`;
//! - W3 `PowerLossAfter` commit-point (create + rotate) — invisible (Phase
//!   A, page-cache loss modeled) or fully visible (Phase B, kill-9 after
//!   commit), never partial; redo is byte-equal `Created`/`Rotated`, replay
//!   is byte-equal `Replayed`;
//! - W4 torn WAL tail (create + rotate) — the last commit frame group
//!   truncated at every cut point of the final transaction: the transaction
//!   disappears whole (all five tables together) or survives whole, proving
//!   "only some tables written" impossible in both directions;
//! - W5 replay storm (create + rotate) — same request replayed 3+ times
//!   plus one after reopen: every call returns the identical durable record,
//!   exactly one row set, and stale-fence CAS calls always fail
//!   `StaleChannel`, with no intermediate generation ever exposed;
//! - W6 proof consistency — after crash recovery `inspect_endpoint_proof`
//!   is either complete and valid or fails closed `CorruptRecord` (tamper
//!   cases), never a bypassable soft success. One pinned counterexample is
//!   `#[ignore]`d: `inspect_channel`'s head/generation fence cross-check is
//!   vacuous (it compares `channels.current_fencing_token` against itself),
//!   so an out-of-band head-fence tamper is silently returned — see
//!   `channel_fault_head_fence_tamper_is_undetected_by_inspect_channel_counterexample`.
//!
//! **Crash semantics disclaimer** (as in every prior matrix): kill-9
//! simulates *process* crashes; the OS page cache survives process death,
//! so a killed process is NOT a machine power loss. Writes the kernel
//! accepted but the disk never saw are covered by
//! [`FaultMode::PowerLossAfter`] and by file-level WAL tail truncation.
//!
//! `allow: SIZE_OK` — one fault matrix per binary is the established repo
//! shape (all prior `*_fault_injection.rs` files are monolithic); fixtures
//! are duplicated per matrix file by convention.

use std::error::Error as _;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use nlos_channel::{
    ChannelAuthority, ChannelAuthorityError, ChannelDecision, ChannelRecord,
    ChannelRotationDecision, CreateChannelRequest, RotateChannelRequest,
};
use nlos_store_fault::{FaultCode, FaultMode};
use nlos_types::{ChannelId, Generation, IdempotencyKey};
use rusqlite::Connection;

const VFS_NAME: &str = "nlos-channel-fault";

const CAPACITY_BYTES: u64 = 4096;
const POLICY_DIGEST: [u8; 32] = [0xa1; 32];
const CREATED_AT_MS: u64 = 1_000;
const ROTATED_AT_MS: u64 = 2_000;

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

fn generation(value: u64) -> Generation {
    Generation::new(NonZeroU64::new(value).expect("generation is non-zero"))
}

fn create_request() -> CreateChannelRequest {
    CreateChannelRequest {
        capacity_bytes: CAPACITY_BYTES,
        policy_digest: POLICY_DIGEST,
        idempotency_key: key(0xb1),
        created_at_ms: CREATED_AT_MS,
    }
}

fn rotate_request(channel_id: ChannelId, fence: [u8; 32]) -> RotateChannelRequest {
    RotateChannelRequest {
        channel_id,
        expected_generation: Generation::INITIAL,
        expected_fencing_token: fence,
        idempotency_key: key(0xb2),
        rotated_at_ms: ROTATED_AT_MS,
    }
}

fn expected_created(channel_id: ChannelId, fence: [u8; 32]) -> ChannelRecord {
    ChannelRecord {
        channel_id,
        generation: Generation::INITIAL,
        fencing_token: fence,
        capacity_bytes: CAPACITY_BYTES,
        policy_digest: POLICY_DIGEST,
        idempotency_key: key(0xb1),
        created_at_ms: CREATED_AT_MS,
    }
}

fn expected_rotated(channel_id: ChannelId, fence: [u8; 32]) -> ChannelRecord {
    // The rotated generation row carries the rotation timestamp as its
    // durable `created_at_ms` (see `make_record` for `rotate_channel`).
    ChannelRecord {
        generation: generation(2),
        fencing_token: fence,
        created_at_ms: ROTATED_AT_MS,
        ..expected_created(channel_id, [0; 32])
    }
}

fn created(decision: ChannelDecision) -> ChannelRecord {
    match decision {
        ChannelDecision::Created(record) => record,
        ChannelDecision::Replayed(_) => panic!("expected Created, got Replayed"),
    }
}

fn replayed(decision: ChannelDecision) -> ChannelRecord {
    match decision {
        ChannelDecision::Replayed(record) => record,
        ChannelDecision::Created(_) => panic!("expected Replayed, got Created"),
    }
}

fn rotated(decision: ChannelRotationDecision) -> ChannelRecord {
    match decision {
        ChannelRotationDecision::Rotated(record) => record,
        ChannelRotationDecision::Replayed(_) => panic!("expected Rotated, got Replayed"),
    }
}

fn replayed_rotation(decision: ChannelRotationDecision) -> ChannelRecord {
    match decision {
        ChannelRotationDecision::Replayed(record) => record,
        ChannelRotationDecision::Rotated(_) => panic!("expected Replayed, got Rotated"),
    }
}

// ---------------------------------------------------------------------------
// test roots, sandbox CWD, fault-VFS open (see header deviation note)
// ---------------------------------------------------------------------------

/// RAII test root: one fresh directory per scenario, removed on drop.
struct TestRoot(PathBuf);

impl TestRoot {
    fn new(label: &str) -> Self {
        let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "nlos-channel-fault-{label}-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&base).expect("create test root");
        Self(base)
    }

    fn base(&self) -> &Path {
        &self.0
    }

    fn database(&self) -> PathBuf {
        self.0.join("channel-authority.db")
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// RAII sandbox process CWD. `ChannelAuthority::open` runs
/// `create_dir_all(root)` on the literal URI root string, which is a
/// relative OS path; the sandbox keeps that junk directory tree inside a
/// temp directory that is removed on drop, so the worktree stays clean.
/// All fault tests are serialized by `FAULT_LOCK`, and every other test in
/// this binary is either a no-op (`crash_child_helper` without the scenario
/// environment) or uses absolute paths only.
struct SandboxCwd {
    previous: PathBuf,
    directory: PathBuf,
}

impl SandboxCwd {
    fn new(label: &str) -> Self {
        let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "nlos-channel-fault-cwd-{label}-{}-{suffix}",
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

/// The URI root that routes `ChannelAuthority::open`'s connection through
/// the registered fault VFS: the decoded path component is the real
/// database file, `vfs=` selects the shim, and `tail=` swallows the
/// `/channel-authority.db` that `ChannelAuthority::open` appends after the
/// root (`SQLite` ignores unrecognized query parameters).
fn fault_root(base: &Path) -> String {
    let database = base.join("channel-authority.db");
    format!("file:{}?vfs={VFS_NAME}&tail=", database.display())
}

/// Opens the authority with its connection on the fault VFS. Pragmas and
/// the migration run while faults are disarmed, so the schema prefix is
/// always durable before any injection.
fn open_fault(base: &Path) -> ChannelAuthority {
    nlos_store_fault::register(VFS_NAME).expect("register fault vfs");
    ChannelAuthority::open(fault_root(base)).expect("open channel authority via fault vfs")
}

/// Reopens the authority through the plain default VFS (never faulted).
fn reopen(base: &Path) -> ChannelAuthority {
    ChannelAuthority::open(base).expect("reopen channel authority")
}

// ---------------------------------------------------------------------------
// shared assertions (fault_injection.rs 范式)
// ---------------------------------------------------------------------------

/// Full `Display` chain of a `ChannelAuthorityError`, top cause last, for
/// content assertions (e.g. that `SQLITE_FULL`'s message reaches the
/// caller).
fn error_chain(error: &ChannelAuthorityError) -> String {
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
fn assert_sqlite_error_chain(error: &ChannelAuthorityError, needles: &[&str]) {
    assert!(
        matches!(error, ChannelAuthorityError::Sqlite(_)),
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

/// Row counts of the five tables the durable transactions write:
/// `channels`, `channel_topic_identities`, `channel_generations`,
/// `channel_rotations`, `channel_endpoint_proofs`. Each fixture owns exactly
/// one channel, so global counts equal per-channel counts.
fn assert_channel_counts(database: &Path, expected: [i64; 5]) {
    let tables = [
        "channels",
        "channel_topic_identities",
        "channel_generations",
        "channel_rotations",
        "channel_endpoint_proofs",
    ];
    for (table, want) in tables.iter().zip(expected) {
        assert_eq!(
            raw_count(database, &format!("SELECT COUNT(*) FROM {table}")),
            want,
            "unexpected row count in {table}"
        );
    }
}

/// The create/rotate transaction left no durable trace at all — the
/// bidirectional "no partial table writes" invisible direction.
fn assert_channel_absent(database: &Path) {
    assert_channel_counts(database, [0, 0, 0, 0, 0]);
}

fn assert_integrity(database: &Path) {
    let connection = Connection::open(database).expect("open for integrity check");
    let result: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("run integrity_check");
    assert_eq!(result, "ok", "integrity_check must pass");
}

fn assert_not_found(authority: &ChannelAuthority, channel_id: ChannelId) {
    assert!(
        matches!(
            authority.inspect_channel(channel_id),
            Err(ChannelAuthorityError::ChannelNotFound(_))
        ),
        "absent channel must fail ChannelNotFound"
    );
    assert!(
        matches!(
            authority.inspect_endpoint_proof(channel_id),
            Err(ChannelAuthorityError::ChannelNotFound(_))
        ),
        "absent channel proof must fail ChannelNotFound"
    );
}

fn assert_stale(authority: &ChannelAuthority, request: RotateChannelRequest) {
    assert!(
        matches!(
            authority.rotate_channel(request),
            Err(ChannelAuthorityError::StaleChannel)
        ),
        "stale CAS must fail StaleChannel"
    );
}

fn generation_sequence(database: &Path, channel_id: ChannelId) -> Vec<i64> {
    let connection = Connection::open(database).expect("open raw reader");
    let mut statement = connection
        .prepare(
            "SELECT channel_generation FROM channel_generations
             WHERE channel_id=?1 ORDER BY channel_generation",
        )
        .expect("prepare generation query");
    let rows = statement
        .query_map([channel_id.as_bytes().as_slice()], |row| {
            row.get::<_, i64>(0)
        })
        .expect("query generations");
    rows.map(|row| row.expect("generation row")).collect()
}

// ---------------------------------------------------------------------------
// kill-9 child-process harness (fault_injection.rs 范式)
// ---------------------------------------------------------------------------

fn spawn_child(scenario: &str, root: &TestRoot) -> Child {
    Command::new(std::env::current_exe().expect("current test executable"))
        .args(["--exact", "crash_child_helper", "--nocapture"])
        .env("NLOS_CHANNEL_CRASH_CHILD_SCENARIO", scenario)
        .env("NLOS_CHANNEL_CRASH_CHILD_ROOT", root.base().as_os_str())
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

fn hex_decode32(text: &str) -> [u8; 32] {
    assert_eq!(text.len(), 64, "fence hex is 32 bytes");
    let mut decoded = [0_u8; 32];
    for (index, slot) in decoded.iter_mut().enumerate() {
        *slot = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).expect("hex byte");
    }
    decoded
}

fn marker_part(marker: &str, index: usize) -> &str {
    marker
        .trim()
        .strip_prefix("READY ")
        .expect("marker")
        .split(' ')
        .nth(index)
        .unwrap_or_else(|| panic!("marker part {index}"))
}

/// Decodes the `create-commit` marker:
/// `READY <channel-id> <fence> <participant> <receipt>`.
struct CreateMarker(ChannelRecord, [u8; 16], [u8; 16]);

fn decode_create_marker(marker: &str) -> CreateMarker {
    let channel_id = ChannelId::from_bytes(hex_decode16(marker_part(marker, 0)));
    let record = expected_created(channel_id, hex_decode32(marker_part(marker, 1)));
    CreateMarker(
        record,
        hex_decode16(marker_part(marker, 2)),
        hex_decode16(marker_part(marker, 3)),
    )
}

/// Decodes the `rotate-commit` marker:
/// `READY <channel-id> <fence1> <fence2> <participant> <receipt2>`.
struct RotateMarker(ChannelRecord, ChannelRecord, [u8; 16], [u8; 16]);

fn decode_rotate_marker(marker: &str) -> RotateMarker {
    let channel_id = ChannelId::from_bytes(hex_decode16(marker_part(marker, 0)));
    let fence_one = hex_decode32(marker_part(marker, 1));
    let created_record = expected_created(channel_id, fence_one);
    let rotated_record = expected_rotated(channel_id, hex_decode32(marker_part(marker, 2)));
    RotateMarker(
        created_record,
        rotated_record,
        hex_decode16(marker_part(marker, 3)),
        hex_decode16(marker_part(marker, 4)),
    )
}

/// Child entry point. Runs only when spawned by a parent test with the
/// scenario environment set; a no-op in the normal test run.
#[test]
fn crash_child_helper() {
    let (Ok(scenario), Ok(root)) = (
        std::env::var("NLOS_CHANNEL_CRASH_CHILD_SCENARIO"),
        std::env::var("NLOS_CHANNEL_CRASH_CHILD_ROOT"),
    ) else {
        return;
    };
    let root = PathBuf::from(root);
    match scenario.as_str() {
        "create-commit" => child_create_commit(&root),
        "rotate-commit" => child_rotate_commit(&root),
        other => panic!("unknown crash child scenario {other}"),
    }
}

/// Child fixture: one fully committed create transaction on the plain VFS;
/// the kill lands AFTER the commit point (visible case) and leaves the WAL
/// on disk (torn-tail fixture).
fn child_create_commit(root: &Path) -> ! {
    let authority = reopen(root);
    let record = created(
        authority
            .create_channel(create_request())
            .expect("child create"),
    );
    let proof = authority
        .inspect_endpoint_proof(record.channel_id)
        .expect("child proof");
    announce(&format!(
        "READY {} {} {} {}",
        hex_encode(record.channel_id.as_bytes()),
        hex_encode(record.fencing_token.as_slice()),
        hex_encode(proof.participant_id.as_bytes()),
        hex_encode(proof.admission_receipt_id.as_bytes()),
    ));
    let _keeper = authority;
    loop {
        std::thread::park();
    }
}

/// Child fixture: a committed create followed by a fully committed
/// rotation; the kill lands AFTER the rotation commit point.
fn child_rotate_commit(root: &Path) -> ! {
    let authority = reopen(root);
    let seed = created(
        authority
            .create_channel(create_request())
            .expect("child create"),
    );
    let record = rotated(
        authority
            .rotate_channel(rotate_request(seed.channel_id, seed.fencing_token))
            .expect("child rotate"),
    );
    let proof = authority
        .inspect_endpoint_proof(record.channel_id)
        .expect("child proof");
    announce(&format!(
        "READY {} {} {} {} {}",
        hex_encode(record.channel_id.as_bytes()),
        hex_encode(seed.fencing_token.as_slice()),
        hex_encode(record.fencing_token.as_slice()),
        hex_encode(proof.participant_id.as_bytes()),
        hex_encode(proof.admission_receipt_id.as_bytes()),
    ));
    let _keeper = authority;
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

/// Byte offset that cuts the WAL in the middle of the LAST commit frame
/// (found via the real commit markers), discarding that transaction whole
/// while every earlier commit survives.
fn cut_inside_last_commit(wal: &[u8]) -> usize {
    let (_, frame_size, _) = wal_frame_layout(wal);
    let commits = commit_frames(wal);
    assert!(
        commits.len() >= 2,
        "fixture must contain several committed transactions"
    );
    let last = *commits.last().expect("commits exist");
    32 + last * frame_size + frame_size / 2
}

/// Every cut offset that truncates the WAL anywhere inside the final
/// transaction's frame span (from the end of the previous commit frame to
/// the last commit frame inclusive): frame boundaries, half-frame points,
/// and last-byte points. Every such cut must leave the final transaction
/// wholly invisible.
fn torn_tail_cuts(wal: &[u8]) -> Vec<usize> {
    let (_, frame_size, _) = wal_frame_layout(wal);
    let commits = commit_frames(wal);
    assert!(commits.len() >= 2, "fixture must have earlier commits");
    let previous = commits[commits.len() - 2];
    let last = commits[commits.len() - 1];
    let mut cuts = vec![32 + (previous + 1) * frame_size];
    for index in (previous + 1)..=last {
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

/// The on-disk state a killed child leaves behind, restorable per sweep
/// iteration so every torn-tail cut starts from identical bytes.
struct WalSnapshot {
    database: Vec<u8>,
    wal: Vec<u8>,
}

impl WalSnapshot {
    fn capture(database: &Path) -> Self {
        Self {
            database: fs::read(database).expect("read database"),
            wal: fs::read(sibling_path(database, "-wal")).expect("read wal"),
        }
    }

    /// Restores the database, rewrites the WAL truncated to `cut` (or in
    /// full for `None`), and drops the stale wal-index.
    fn restore(&self, database: &Path, cut: Option<usize>) {
        fs::write(database, &self.database).expect("restore database");
        let wal = match cut {
            Some(cut) => &self.wal[..cut],
            None => &self.wal[..],
        };
        fs::write(sibling_path(database, "-wal"), wal).expect("restore wal");
        let _ = fs::remove_file(sibling_path(database, "-shm"));
    }
}

/// Truncates the WAL of a freshly killed child fixture in the middle of
/// its last commit frame.
fn truncate_wal_inside_last_commit(database: &Path) {
    let wal_path = sibling_path(database, "-wal");
    let wal = fs::read(&wal_path).expect("read wal");
    let cut = cut_inside_last_commit(&wal);
    fs::write(&wal_path, &wal[..cut]).expect("write truncated wal");
    let _ = fs::remove_file(sibling_path(database, "-shm"));
}

// ---------------------------------------------------------------------------
// W1/W2: pre-commit IOERR / ENOSPC fail typed and converge
// ---------------------------------------------------------------------------

/// W1（create）：`FailWritesAfter { 0, IoErr }` 注入 create 单事务提交的
/// WAL 写入 → `ChannelAuthorityError::Sqlite` 显式失败（错误链含 I/O 条
/// 件）；重开后五表零行（schema 前缀保留、通道完全不可见）、integrity
/// ok；disarm 后同一请求重做 → `Created` 且记录确定性成立；重开后
/// （1,1,1,0,1）恰好一套行、proof 有效。
#[test]
#[allow(clippy::too_many_lines)]
fn channel_fault_create_precommit_ioerr_fails_typed_and_converges() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let _sandbox = SandboxCwd::new("ioerr");
    let root = TestRoot::new("ioerr");
    let authority = open_fault(root.base());

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::IoErr,
    });
    let error = authority
        .create_channel(create_request())
        .expect_err("create must fail under injected I/O error");
    assert_sqlite_error_chain(&error, &["i/o", "ioerr"]);
    assert!(nlos_store_fault::writes_observed() > 0);
    assert_channel_absent(&root.database());

    nlos_store_fault::disarm();
    drop(authority);
    let recovered = reopen(root.base());
    assert_channel_absent(&root.database());
    assert_integrity(&root.database());

    let record = created(
        recovered
            .create_channel(create_request())
            .expect("create succeeds after disarm"),
    );
    assert_eq!(record.generation, Generation::INITIAL);
    drop(recovered);
    let verified = reopen(root.base());
    assert_channel_counts(&root.database(), [1, 1, 1, 0, 1]);
    assert_eq!(
        verified.inspect_channel(record.channel_id).expect("head"),
        record
    );
    let proof = verified
        .inspect_endpoint_proof(record.channel_id)
        .expect("proof after redo");
    assert_eq!(proof.channel_id, record.channel_id);
    assert_eq!(proof.participant_generation, Generation::INITIAL);
    assert_integrity(&root.database());
}

/// W2（create）：`FailWritesAfter { 0, Full }` 下同一收敛——`SQLITE_FULL`
/// 显式失败（链含 full）、零幻影行；disarm 后重试成功且行恰好一套。
#[test]
fn channel_fault_create_precommit_enospc_fails_typed_and_converges() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let _sandbox = SandboxCwd::new("full");
    let root = TestRoot::new("full");
    let authority = open_fault(root.base());

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::Full,
    });
    let error = authority
        .create_channel(create_request())
        .expect_err("create must fail under injected disk-full");
    assert_sqlite_error_chain(&error, &["full"]);
    assert!(nlos_store_fault::writes_observed() > 0);
    assert_channel_absent(&root.database());

    nlos_store_fault::disarm();
    let record = created(
        authority
            .create_channel(create_request())
            .expect("create succeeds after disarm"),
    );
    drop(authority);
    let verified = reopen(root.base());
    assert_channel_counts(&root.database(), [1, 1, 1, 0, 1]);
    assert_eq!(
        replayed(
            verified
                .create_channel(create_request())
                .expect("replay after redo")
        ),
        record
    );
    assert_integrity(&root.database());
}

/// W1（rotate）：create 前缀先行落盘，`FailWritesAfter { 0, IoErr }` 注入
/// rotation 单事务（generation + proof + receipt + CAS head）提交写入 →
/// typed `Sqlite` 失败；重开后 head 仍指旧 generation、（1,1,1,0,1）保持
/// 前缀；disarm 后同请求重做 → `Rotated` gen2；旧 fence 再 CAS →
/// `StaleChannel`。
#[test]
#[allow(clippy::too_many_lines)]
fn channel_fault_rotate_precommit_ioerr_fails_typed_and_converges() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let _sandbox = SandboxCwd::new("rot-ioerr");
    let root = TestRoot::new("rot-ioerr");
    let authority = open_fault(root.base());
    let seed = created(
        authority
            .create_channel(create_request())
            .expect("create prefix"),
    );
    let request = rotate_request(seed.channel_id, seed.fencing_token);

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::IoErr,
    });
    let error = authority
        .rotate_channel(request)
        .expect_err("rotate must fail under injected I/O error");
    assert_sqlite_error_chain(&error, &["i/o", "ioerr"]);
    assert!(nlos_store_fault::writes_observed() > 0);
    assert_channel_counts(&root.database(), [1, 1, 1, 0, 1]);

    nlos_store_fault::disarm();
    drop(authority);
    let recovered = reopen(root.base());
    assert_eq!(
        recovered
            .inspect_channel(seed.channel_id)
            .expect("head still old generation"),
        seed
    );
    assert_channel_counts(&root.database(), [1, 1, 1, 0, 1]);
    assert_integrity(&root.database());

    let record = rotated(
        recovered
            .rotate_channel(request)
            .expect("rotate succeeds after disarm"),
    );
    assert_eq!(record.generation, generation(2));
    assert_stale(
        &recovered,
        RotateChannelRequest {
            idempotency_key: key(0xc1),
            ..rotate_request(seed.channel_id, seed.fencing_token)
        },
    );
    drop(recovered);
    let verified = reopen(root.base());
    assert_channel_counts(&root.database(), [1, 1, 2, 1, 2]);
    assert_eq!(
        verified
            .inspect_channel(seed.channel_id)
            .expect("head advanced"),
        record
    );
    assert_integrity(&root.database());
}

// ---------------------------------------------------------------------------
// W3: PowerLossAfter the commit point (create + rotate)
// ---------------------------------------------------------------------------

/// W3（create）：
/// - Phase A（断电不可见）：`PowerLossAfter { 0 }` 下 create "报告成功"但
///   写入从未落盘；重开后五表零行、`ChannelNotFound`——不是部分可见；
///   同一请求重做 → `Created` 与幻影记录逐字节相等（确定性 id/fence），
///   重开后恰好一套行、proof 有效。
/// - Phase B（提交后 kill-9 可见）：子进程完整提交 create 事务后被强杀；
///   重开后 (1,1,1,0,1) 全可见、proof 与子进程宣告逐字节一致；同请求
///   重放 → `Replayed` 逐字节相等、无重复行。
#[test]
#[allow(clippy::too_many_lines)]
fn channel_fault_create_power_loss_commit_point_converges_both_ways() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    create_power_loss_invisible_redo_byte_equal();
    create_kill9_after_commit_visible_replay_byte_equal();
}

fn create_power_loss_invisible_redo_byte_equal() {
    let _sandbox = SandboxCwd::new("pl-create");
    let root = TestRoot::new("pl-create");
    let authority = open_fault(root.base());

    nlos_store_fault::arm(FaultMode::PowerLossAfter { remaining: 0 });
    let phantom = created(
        authority
            .create_channel(create_request())
            .expect("power loss drops writes silently"),
    );
    nlos_store_fault::disarm();
    // The surviving connection keeps a wal-index referencing frames the
    // disk never saw; it must die first (as a real power loss would kill
    // it) so recovery sees durable bytes alone (fault_injection.rs
    // precedent).
    drop(authority);

    let recovered = reopen(root.base());
    assert_not_found(&recovered, phantom.channel_id);
    assert_channel_absent(&root.database());
    assert_integrity(&root.database());

    let redone = created(
        recovered
            .create_channel(create_request())
            .expect("redo create after power loss"),
    );
    assert_eq!(
        redone, phantom,
        "redo must be byte-equal to the silently lost record"
    );
    drop(recovered);
    let verified = reopen(root.base());
    assert_channel_counts(&root.database(), [1, 1, 1, 0, 1]);
    assert_eq!(
        verified
            .inspect_channel(phantom.channel_id)
            .expect("head visible after redo"),
        phantom
    );
    let proof = verified
        .inspect_endpoint_proof(phantom.channel_id)
        .expect("proof valid after redo");
    assert_eq!(proof.participant_generation, Generation::INITIAL);
    assert_integrity(&root.database());
}

fn create_kill9_after_commit_visible_replay_byte_equal() {
    let root = TestRoot::new("kill9-create");
    let mut child = spawn_child("create-commit", &root);
    let marker = await_marker(&mut child);
    let CreateMarker(record, participant, receipt) = decode_create_marker(&marker);
    kill_and_reap(&mut child);

    let recovered = reopen(root.base());
    assert_channel_counts(&root.database(), [1, 1, 1, 0, 1]);
    assert_eq!(
        recovered
            .inspect_channel(record.channel_id)
            .expect("committed create must survive the kill"),
        record
    );
    let proof = recovered
        .inspect_endpoint_proof(record.channel_id)
        .expect("proof must survive the kill");
    assert_eq!(proof.channel_id, record.channel_id);
    assert_eq!(*proof.participant_id.as_bytes(), participant);
    assert_eq!(*proof.admission_receipt_id.as_bytes(), receipt);
    assert_eq!(proof.participant_generation, Generation::INITIAL);
    assert_integrity(&root.database());

    let replay = replayed(
        recovered
            .create_channel(create_request())
            .expect("visible replay"),
    );
    assert_eq!(replay, record, "replay must be byte-equal");
    assert_channel_counts(&root.database(), [1, 1, 1, 0, 1]);
    assert_integrity(&root.database());
    drop(recovered);
}

/// W3（rotate）：
/// - Phase A（断电不可见）：create 先行落盘，`PowerLossAfter { 0 }` 下
///   rotate "报告成功"；重开后 head 仍指 gen1、(1,1,1,0,1)——rotation 整
///   体不可见、无中间代；同请求重做 → `Rotated` 与幻影逐字节相等。
/// - Phase B（提交后 kill-9 可见）：子进程 create+rotate 全部提交后被强
///   杀；重开后 head=gen2、(1,1,2,1,2)；同 key 重放 → `Replayed` 逐字节
///   相等；旧 fence 新 key → `StaleChannel`；create 幂等行仍可重放。
#[test]
#[allow(clippy::too_many_lines)]
fn channel_fault_rotate_power_loss_commit_point_converges_both_ways() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    rotate_power_loss_invisible_redo_byte_equal();
    rotate_kill9_after_commit_visible_replay_byte_equal();
}

fn rotate_power_loss_invisible_redo_byte_equal() {
    let _sandbox = SandboxCwd::new("pl-rotate");
    let root = TestRoot::new("pl-rotate");
    let authority = open_fault(root.base());
    let seed = created(
        authority
            .create_channel(create_request())
            .expect("create prefix"),
    );
    let request = rotate_request(seed.channel_id, seed.fencing_token);

    nlos_store_fault::arm(FaultMode::PowerLossAfter { remaining: 0 });
    let phantom = rotated(
        authority
            .rotate_channel(request)
            .expect("power loss drops writes silently"),
    );
    nlos_store_fault::disarm();
    drop(authority);

    let recovered = reopen(root.base());
    assert_eq!(
        recovered
            .inspect_channel(seed.channel_id)
            .expect("head still the old generation"),
        seed
    );
    assert_channel_counts(&root.database(), [1, 1, 1, 0, 1]);
    assert_integrity(&root.database());

    let redone = rotated(
        recovered
            .rotate_channel(request)
            .expect("redo rotate after power loss"),
    );
    assert_eq!(
        redone, phantom,
        "redo must be byte-equal to the silently lost record"
    );
    assert_eq!(
        generation_sequence(&root.database(), seed.channel_id),
        [1, 2]
    );
    drop(recovered);
    let verified = reopen(root.base());
    assert_channel_counts(&root.database(), [1, 1, 2, 1, 2]);
    assert_eq!(
        verified
            .inspect_channel(seed.channel_id)
            .expect("head advanced after redo"),
        redone
    );
    assert_integrity(&root.database());
}

fn rotate_kill9_after_commit_visible_replay_byte_equal() {
    let root = TestRoot::new("kill9-rotate");
    let mut child = spawn_child("rotate-commit", &root);
    let marker = await_marker(&mut child);
    let RotateMarker(seed, record, participant, receipt) = decode_rotate_marker(&marker);
    kill_and_reap(&mut child);

    let recovered = reopen(root.base());
    assert_channel_counts(&root.database(), [1, 1, 2, 1, 2]);
    assert_eq!(
        recovered
            .inspect_channel(seed.channel_id)
            .expect("committed rotation must survive the kill"),
        record
    );
    let proof = recovered
        .inspect_endpoint_proof(seed.channel_id)
        .expect("proof must survive the kill");
    assert_eq!(*proof.participant_id.as_bytes(), participant);
    assert_eq!(*proof.admission_receipt_id.as_bytes(), receipt);
    assert_eq!(proof.participant_generation, generation(2));
    assert_integrity(&root.database());

    // Same-key replay of the rotation consults only the durable receipt.
    let replay = replayed_rotation(
        recovered
            .rotate_channel(rotate_request(seed.channel_id, seed.fencing_token))
            .expect("visible rotation replay"),
    );
    assert_eq!(replay, record, "replay must be byte-equal");
    // A stale fence (old generation token, fresh key) never re-rotates.
    assert_stale(
        &recovered,
        RotateChannelRequest {
            idempotency_key: key(0xc2),
            ..rotate_request(seed.channel_id, seed.fencing_token)
        },
    );
    // The create idempotency row is untouched by rotation.
    assert_eq!(
        replayed(
            recovered
                .create_channel(create_request())
                .expect("create replay"),
        ),
        record
    );
    assert_channel_counts(&root.database(), [1, 1, 2, 1, 2]);
    assert_integrity(&root.database());
    drop(recovered);
}

// ---------------------------------------------------------------------------
// W4: torn WAL tail (create + rotate)
// ---------------------------------------------------------------------------

/// W4（create，双向证明）：子进程提交完整 create 事务后被强杀；父进程对
/// WAL 最后一个 commit 帧组的**每一个**截断点（帧边界/半帧/末字节，含
/// 整个尾部移除）逐一恢复重开 → 五表恒零行、`ChannelNotFound`——事务整
/// 体消失，"只有部分表写入"被穷举证明不可能；恢复完整 WAL 重开 →
/// (1,1,1,0,1) 恰好一套行（可见方向），同请求重放逐字节相等。
#[test]
#[allow(clippy::too_many_lines)]
fn channel_fault_create_torn_wal_tail_sweep_never_exposes_partial_rows() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let root = TestRoot::new("torn-create");
    let mut child = spawn_child("create-commit", &root);
    let marker = await_marker(&mut child);
    let CreateMarker(record, ..) = decode_create_marker(&marker);
    kill_and_reap(&mut child);

    let database = root.database();
    let snapshot = WalSnapshot::capture(&database);

    // Visible direction: the untouched WAL recovers the whole transaction.
    let recovered = reopen(root.base());
    assert_channel_counts(&database, [1, 1, 1, 0, 1]);
    assert_eq!(
        recovered
            .inspect_channel(record.channel_id)
            .expect("visible control"),
        record
    );
    drop(recovered);

    // Invisible direction: every cut inside the final transaction's frame
    // span discards it whole — head, identity, generation, receipt and
    // proof disappear together, never partially.
    let cuts = torn_tail_cuts(&snapshot.wal);
    assert!(
        cuts.len() >= 4,
        "sweep must cover several cut points, got {cuts:?}"
    );
    for cut in cuts {
        snapshot.restore(&database, Some(cut));
        let authority = reopen(root.base());
        assert_not_found(&authority, record.channel_id);
        assert_channel_absent(&database);
        assert_integrity(&database);
        drop(authority);
    }

    // Full restore returns to the visible world; the same request replays
    // byte-equal without appending a second row set.
    snapshot.restore(&database, None);
    let verified = reopen(root.base());
    assert_channel_counts(&database, [1, 1, 1, 0, 1]);
    assert_eq!(
        replayed(
            verified
                .create_channel(create_request())
                .expect("replay after full restore"),
        ),
        record
    );
    assert_integrity(&database);
}

/// W4（rotate）：子进程 create+rotate 提交后被强杀，WAL 在最后 commit 帧
/// （rotation 事务）半帧处截断；重开后 rotation 整体消失——head 仍指
/// gen1、(1,1,1,0,1)、无中间代；同请求重做 → `Rotated` 与子进程宣告逐
/// 字节相等；重开后重放逐字节相等、(1,1,2,1,2) 恰一套增量行。
#[test]
#[allow(clippy::too_many_lines)]
fn channel_fault_rotate_torn_wal_tail_discards_and_redo_converges() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let root = TestRoot::new("torn-rotate");
    let mut child = spawn_child("rotate-commit", &root);
    let marker = await_marker(&mut child);
    let RotateMarker(seed, record, ..) = decode_rotate_marker(&marker);
    kill_and_reap(&mut child);

    truncate_wal_inside_last_commit(&root.database());

    let recovered = reopen(root.base());
    assert_eq!(
        recovered
            .inspect_channel(seed.channel_id)
            .expect("rotation must be discarded whole"),
        seed,
        "head must still point at the old generation"
    );
    assert_channel_counts(&root.database(), [1, 1, 1, 0, 1]);
    assert_integrity(&root.database());

    let redone = rotated(
        recovered
            .rotate_channel(rotate_request(seed.channel_id, seed.fencing_token))
            .expect("redo rotate after torn tail"),
    );
    assert_eq!(redone, record, "redo must match the killed transaction");
    drop(recovered);
    let verified = reopen(root.base());
    let replay = replayed_rotation(
        verified
            .rotate_channel(rotate_request(seed.channel_id, seed.fencing_token))
            .expect("replay after redo"),
    );
    assert_eq!(replay, record);
    assert_channel_counts(&root.database(), [1, 1, 2, 1, 2]);
    assert_integrity(&root.database());
}

// ---------------------------------------------------------------------------
// W5: kill-window replay storms and the rotate CAS kill-window
// ---------------------------------------------------------------------------

/// W5（create）：commit 后同一 create 请求连放 3 次 + 重开后再放 1 次 →
/// 每次 `Replayed` 与 committed 记录逐字节相等；冲突请求恒
/// `IdempotencyConflict`；(1,1,1,0,1) 恰好一套行。
#[test]
fn channel_fault_create_replay_storm_is_idempotent() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let root = TestRoot::new("storm-create");
    let authority = reopen(root.base());
    let record = created(authority.create_channel(create_request()).expect("create"));

    for _ in 0..3 {
        assert_eq!(
            replayed(
                authority
                    .create_channel(create_request())
                    .expect("storm replay"),
            ),
            record,
            "every storm replay is byte-equal"
        );
    }
    assert!(
        matches!(
            authority.create_channel(CreateChannelRequest {
                capacity_bytes: CAPACITY_BYTES + 1,
                ..create_request()
            }),
            Err(ChannelAuthorityError::IdempotencyConflict)
        ),
        "conflicting rebind must keep failing mid-storm"
    );
    drop(authority);
    let verified = reopen(root.base());
    assert_eq!(
        replayed(
            verified
                .create_channel(create_request())
                .expect("replay after reopen"),
        ),
        record
    );
    assert_channel_counts(&root.database(), [1, 1, 1, 0, 1]);
    assert_integrity(&root.database());
}

/// W5（rotate）：rotation 后同请求连放 3 次 + 重开后再放 1 次 → 每次
/// `Replayed` 逐字节相等；同 key 不同 CAS fence 恒 `IdempotencyConflict`；
/// (1,1,2,1,2) 恰好一套增量行。
#[test]
fn channel_fault_rotate_replay_storm_is_idempotent() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let root = TestRoot::new("storm-rotate");
    let authority = reopen(root.base());
    let seed = created(authority.create_channel(create_request()).expect("create"));
    let request = rotate_request(seed.channel_id, seed.fencing_token);
    let record = rotated(authority.rotate_channel(request).expect("rotate"));

    for _ in 0..3 {
        assert_eq!(
            replayed_rotation(authority.rotate_channel(request).expect("storm replay")),
            record,
            "every storm replay is byte-equal"
        );
    }
    assert!(
        matches!(
            authority.rotate_channel(RotateChannelRequest {
                expected_fencing_token: [0xee; 32],
                ..request
            }),
            Err(ChannelAuthorityError::IdempotencyConflict)
        ),
        "same key with a different CAS fence must keep failing mid-storm"
    );
    drop(authority);
    let verified = reopen(root.base());
    assert_eq!(
        replayed_rotation(
            verified
                .rotate_channel(request)
                .expect("replay after reopen"),
        ),
        record
    );
    assert_channel_counts(&root.database(), [1, 1, 2, 1, 2]);
    assert_integrity(&root.database());
}

/// W5（rotate CAS kill-window）：rotation 单事务在 CAS UPDATE 前后崩溃只
/// 有两个合法终点——Phase A（`PowerLossAfter{0}`，CAS 未持久化）head 仍指
/// 旧 generation，可重做；重做后（CAS 已持久化）head 指完整新 generation，
/// 可重放。任何状态下伪造 fence / 旧 fence 新 key 的 CAS 调用恒
/// `StaleChannel`；generations 表恒为连续 [1,2]，绝无中间代。
#[test]
#[allow(clippy::too_many_lines)]
fn channel_fault_rotate_cas_kill_window_has_no_intermediate_generation() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let _sandbox = SandboxCwd::new("cas");
    let root = TestRoot::new("cas");
    let authority = open_fault(root.base());
    let seed = created(
        authority
            .create_channel(create_request())
            .expect("create prefix"),
    );
    let request = rotate_request(seed.channel_id, seed.fencing_token);

    // Crash before the CAS UPDATE is durable: the whole rotation vanishes.
    nlos_store_fault::arm(FaultMode::PowerLossAfter { remaining: 0 });
    let phantom = rotated(
        authority
            .rotate_channel(request)
            .expect("power loss drops writes silently"),
    );
    nlos_store_fault::disarm();
    drop(authority);

    let recovered = reopen(root.base());
    assert_eq!(
        recovered
            .inspect_channel(seed.channel_id)
            .expect("old generation survives"),
        seed,
        "kill before CAS durable must leave the head at the old generation"
    );
    assert_eq!(
        generation_sequence(&root.database(), seed.channel_id),
        [1],
        "no intermediate generation may leak"
    );
    // A fabricated fence can never rotate, in either crash state.
    assert_stale(
        &recovered,
        RotateChannelRequest {
            expected_fencing_token: [0xee; 32],
            idempotency_key: key(0xd1),
            ..rotate_request(seed.channel_id, seed.fencing_token)
        },
    );
    // Neither can the not-yet-durable new-generation fence.
    assert_stale(
        &recovered,
        RotateChannelRequest {
            expected_generation: phantom.generation,
            expected_fencing_token: phantom.fencing_token,
            idempotency_key: key(0xd2),
            ..rotate_request(seed.channel_id, seed.fencing_token)
        },
    );

    // Redo crosses the CAS exactly once; the result is byte-equal.
    let redone = rotated(
        recovered
            .rotate_channel(request)
            .expect("redo rotate across the kill window"),
    );
    assert_eq!(redone, phantom);
    assert_eq!(
        generation_sequence(&root.database(), seed.channel_id),
        [1, 2],
        "generations must stay contiguous"
    );
    // After the head advanced, every stale CAS form fails closed.
    assert_stale(
        &recovered,
        RotateChannelRequest {
            idempotency_key: key(0xd3),
            ..rotate_request(seed.channel_id, seed.fencing_token)
        },
    );
    assert_stale(
        &recovered,
        RotateChannelRequest {
            expected_fencing_token: [0xee; 32],
            idempotency_key: key(0xd4),
            ..rotate_request(seed.channel_id, seed.fencing_token)
        },
    );
    // The same key still replays the durable rotation.
    assert_eq!(
        replayed_rotation(
            recovered
                .rotate_channel(request)
                .expect("same-key replay after redo"),
        ),
        redone
    );
    assert_channel_counts(&root.database(), [1, 1, 2, 1, 2]);
    assert_integrity(&root.database());
    drop(recovered);
}

// ---------------------------------------------------------------------------
// W6: proof consistency after recovery (valid or fail-closed)
// ---------------------------------------------------------------------------

/// W6：恢复后的端点证明要么完整有效、要么以 `CorruptRecord` 硬失败——
/// 不存在可绕过的软成功：
/// - 篡改 proof 的 participant（绕过 immutable 触发器）→
///   `CorruptRecord("…disagrees with authority-derived identity")`；
/// - 删除当前代 proof 行 → `CorruptRecord("…has no endpoint proof")`；
/// - 篡改 head fence → `inspect_endpoint_proof` 经派生校验
///   `CorruptRecord("…disagrees with authority-derived identity")` 硬失败
///   （`inspect_channel` 对同一篡改的现状见下方 `#[ignore]` 反例）。
#[test]
#[allow(clippy::too_many_lines)]
fn channel_fault_endpoint_proof_recovery_is_valid_or_fails_closed() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();

    // Tampered participant identity: derivation mismatch, fail closed.
    let root = TestRoot::new("proof-tamper");
    let authority = reopen(root.base());
    let seed = created(authority.create_channel(create_request()).expect("create"));
    rotated(
        authority
            .rotate_channel(rotate_request(seed.channel_id, seed.fencing_token))
            .expect("rotate"),
    );
    let raw = Connection::open(root.database()).expect("open raw writer");
    raw.execute_batch("DROP TRIGGER channel_endpoint_proofs_immutable_update;")
        .expect("drop update trigger");
    raw.execute(
        "UPDATE channel_endpoint_proofs SET participant_id=?1
         WHERE channel_id=?2 AND channel_generation=2",
        rusqlite::params![
            [0xee_u8; 16].as_slice(),
            seed.channel_id.as_bytes().as_slice()
        ],
    )
    .expect("tamper participant");
    drop(raw);
    assert!(
        matches!(
            authority.inspect_endpoint_proof(seed.channel_id),
            Err(ChannelAuthorityError::CorruptRecord(
                "channel endpoint proof disagrees with authority-derived identity"
            ))
        ),
        "tampered participant must fail CorruptRecord"
    );
    drop(authority);

    // Missing current-generation proof row: fail closed, never a bypass.
    let root = TestRoot::new("proof-missing");
    let authority = reopen(root.base());
    let seed = created(authority.create_channel(create_request()).expect("create"));
    let raw = Connection::open(root.database()).expect("open raw writer");
    raw.execute_batch("DROP TRIGGER channel_endpoint_proofs_immutable_delete;")
        .expect("drop delete trigger");
    raw.execute(
        "DELETE FROM channel_endpoint_proofs WHERE channel_id=?1",
        [seed.channel_id.as_bytes().as_slice()],
    )
    .expect("delete proofs");
    drop(raw);
    assert!(
        matches!(
            authority.inspect_endpoint_proof(seed.channel_id),
            Err(ChannelAuthorityError::CorruptRecord(
                "current channel generation has no endpoint proof"
            ))
        ),
        "missing proof must fail CorruptRecord"
    );
    drop(authority);

    // Head fence tampered away from the generation row: the endpoint-proof
    // read fails closed through the authority-derived receipt check (the
    // stored proof was derived from the true fence, the head now carries
    // the tampered one).
    let root = TestRoot::new("head-tamper");
    let authority = reopen(root.base());
    let seed = created(authority.create_channel(create_request()).expect("create"));
    let raw = Connection::open(root.database()).expect("open raw writer");
    raw.execute(
        "UPDATE channels SET current_fencing_token=?1 WHERE channel_id=?2",
        rusqlite::params![
            [0xee_u8; 32].as_slice(),
            seed.channel_id.as_bytes().as_slice()
        ],
    )
    .expect("tamper head fence");
    drop(raw);
    assert!(
        matches!(
            authority.inspect_endpoint_proof(seed.channel_id),
            Err(ChannelAuthorityError::CorruptRecord(
                "channel endpoint proof disagrees with authority-derived identity"
            ))
        ),
        "tampered head fence must fail the proof read closed"
    );
    assert_integrity(&root.database());
}

/// **反例（真实缺陷，等待修复决策，勿删）**：head fence 与 generation 行
/// fence 不一致时 `inspect_channel` 不失败。
///
/// 复现窗口：绕过应用层（raw connection）篡改
/// `channels.current_fencing_token`，使其与
/// `channel_generations.fencing_token`（同 channel 同 `current_generation` 行）
/// 不一致，随后 `ChannelAuthority::inspect_channel`。
///
/// 现状（HEAD c5dd155，2026-08-26 钉住）：`inspect_channel` 返回
/// `Ok(ChannelRecord)`，其 `fencing_token` 即被篡改的 head 值。根因：
/// `load_current_optional`（crates/nlos-channel/src/lib.rs）的 SELECT 直接
/// 取 `c.current_fencing_token` 装入 record，随后的 head/generation 交叉
/// 校验 `load_head_fence` 重新读的是**同一列**（`channels
/// .current_fencing_token`）并与 record 比较——自己比自己恒相等，
/// `CorruptRecord("channel head fence disagrees with current generation")`
/// 分支成为不可达死代码。缓解：`inspect_endpoint_proof` 的派生校验能拦下
/// 该篡改（上方活动测试已覆盖），但 `inspect_channel` 单独使用时会静默
/// 返回被污染的 fence（例如被用作 rotate CAS 的期望值来源时）。
///
/// 期望（修复后应满足，届时移除 `#[ignore]`）：`inspect_channel` 以
/// `CorruptRecord("channel head fence disagrees with current generation")`
/// 硬失败（校验应改为对照 `channel_generations.fencing_token`）。
#[test]
#[ignore = "counterexample: inspect_channel head/generation fence cross-check is vacuous (self-comparison); awaiting fix decision"]
fn channel_fault_head_fence_tamper_is_undetected_by_inspect_channel_counterexample() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let root = TestRoot::new("head-fence-defect");
    let authority = reopen(root.base());
    let seed = created(authority.create_channel(create_request()).expect("create"));
    let raw = Connection::open(root.database()).expect("open raw writer");
    raw.execute(
        "UPDATE channels SET current_fencing_token=?1 WHERE channel_id=?2",
        rusqlite::params![
            [0xee_u8; 32].as_slice(),
            seed.channel_id.as_bytes().as_slice()
        ],
    )
    .expect("tamper head fence");
    drop(raw);
    assert!(
        matches!(
            authority.inspect_channel(seed.channel_id),
            Err(ChannelAuthorityError::CorruptRecord(
                "channel head fence disagrees with current generation"
            ))
        ),
        "inspect_channel must fail closed when the head fence diverges from the generation row"
    );
}
