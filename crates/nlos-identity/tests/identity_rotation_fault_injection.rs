//! B-IDENTITY-002 (lane W13-I): kill-window / fault-injection matrix for
//! `IdentityAuthority::rotate_key` — the durable signing-key rotation
//! single `BEGIN IMMEDIATE` transaction (key version, immutable snapshot,
//! snapshot bindings, dual CAS on `key_heads` / `control_domains`, rotation
//! receipt).
//!
//! Harness follows the established matrices (`nlos-channel/tests/
//! channel_fault_injection.rs`, `nlos-wait/tests/wait_fault_injection.rs`):
//! kill-9 children synchronized through piped `READY` markers — never
//! sleeps, `FAULT_LOCK` process-wide serialization, WAL tail truncation,
//! typed error-chain assertions, raw row counts, `PRAGMA integrity_check`
//! per scenario.
//!
//! **Fault-VFS plumbing (documented harness constraint, same deviation as
//! the channel/topic matrices)**: `IdentityAuthority` has no `open_with_vfs`
//! constructor and the workspace forbids `unsafe`, so the shim is routed in
//! through a `SQLite` **URI filename**: `rusqlite`'s `Connection::open` sets
//! `SQLITE_OPEN_URI`, and `IdentityAuthority::open` passes
//! `root.join("identity-authority.db")` through unchanged, so a root of
//! `file:<db>?vfs=<shim>&tail=` routes that one authority connection through
//! the registered fault VFS (the appended `/identity-authority.db` tail lands
//! in the ignored `tail=` query parameter). The junk directory that
//! `IdentityAuthority::open`'s `create_dir_all(root)` call creates for the
//! literal URI path is kept inside a RAII sandbox process CWD — the worktree
//! is never touched. Every reopen / raw reader / integrity check uses the
//! plain default VFS and can never be faulted.
//!
//! Matrix (rotation window × scenario):
//! - W1 pre-commit IOERR on `rotate_key` — typed `Sqlite` error whose chain
//!   names the injected condition, zero phantom rotation rows (bootstrap
//!   prefix intact), disarm + redo converges;
//! - W2 pre-commit ENOSPC (`SQLITE_FULL`) on `rotate_key` — same
//!   fail-closed convergence;
//! - W3 commit-point `PowerLossAfter` on `rotate_key` — invisible (Phase A,
//!   page-cache loss modeled): head still points at gen1, redo is byte-equal
//!   to the phantom receipt; visible (Phase B, kill-9 after commit): rotation
//!   survives whole and same-key replay is byte-equal `Replayed`;
//! - W4 torn WAL tail on the rotation commit frame — the last transaction
//!   disappears whole (no half rotation: receipt, new key version, snapshot
//!   and CAS rows live and die together), redo converges byte-equal;
//! - W5 replay storm — same rotation request replayed 3+ times plus once
//!   after reopen: every call returns the identical receipt, exactly one
//!   rotation row set, stale-fence calls always fail `KeyGenerationFenceConflict`.
//!
//! **Crash semantics disclaimer** (as in every prior matrix): kill-9
//! simulates *process* crashes; the OS page cache survives process death,
//! so a killed process is NOT a machine power loss. Writes the kernel
//! accepted but the disk never saw are covered by
//! [`FaultMode::PowerLossAfter`] and by file-level WAL tail truncation.
//!
//! `allow: SIZE_OK` — one fault matrix per binary is the established repo
//! shape; fixtures are duplicated per matrix file by convention.

use std::error::Error as _;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use ed25519_dalek::SigningKey;
use nlos_identity::{
    BootstrapDecision, BootstrapPrincipalRequest, IdentityAuthority, IdentityAuthorityError,
    IdentityBinding, KeyPurpose, KeyRotationDecision, KeyRotationReceipt, RotateKeyRequest,
};
use nlos_store_fault::{FaultCode, FaultMode};
use nlos_types::{Generation, IdempotencyKey, KeyId};
use rusqlite::Connection;

const VFS_NAME: &str = "nlos-identity-rotation-fault";

const BOOTSTRAP_SEED: u8 = 0x35;
const NEW_KEY_SEED: u8 = 0x36;
const ROTATED_AT_MS: u64 = 3_000;
const NEW_VALID_FROM_MS: u64 = 2_000;
const NEW_VALID_UNTIL_MS: u64 = 10_000;

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

fn signing_key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn bootstrap_request(seed: u8, signing_key: &SigningKey) -> BootstrapPrincipalRequest {
    BootstrapPrincipalRequest {
        principal_profile_digest: [seed.wrapping_add(1); 32],
        control_domain_policy_digest: [seed.wrapping_add(2); 32],
        public_key: signing_key.verifying_key().to_bytes(),
        key_purpose: KeyPurpose::SemanticSigning,
        key_valid_from_ms: 1_000,
        key_valid_until_ms: 9_000,
        idempotency_key: key(seed.wrapping_add(3)),
        created_at_ms: 500,
    }
}

fn rotate_request(binding: IdentityBinding, new_public_key: [u8; 32]) -> RotateKeyRequest {
    RotateKeyRequest {
        key_id: binding.key_id,
        expected_key_generation: binding.key_generation,
        expected_identity_snapshot_id: binding.identity_snapshot_id,
        new_public_key,
        new_valid_from_ms: NEW_VALID_FROM_MS,
        new_valid_until_ms: NEW_VALID_UNTIL_MS,
        idempotency_key: key(0x88),
        rotated_at_ms: ROTATED_AT_MS,
    }
}

fn rotated(decision: KeyRotationDecision) -> KeyRotationReceipt {
    match decision {
        KeyRotationDecision::Rotated(receipt) => receipt,
        KeyRotationDecision::Replayed(_) => panic!("expected Rotated, got Replayed"),
    }
}

fn replayed(decision: KeyRotationDecision) -> KeyRotationReceipt {
    match decision {
        KeyRotationDecision::Replayed(receipt) => receipt,
        KeyRotationDecision::Rotated(_) => panic!("expected Replayed, got Rotated"),
    }
}

fn bootstrap_binding(authority: &IdentityAuthority) -> IdentityBinding {
    let old_key = signing_key(BOOTSTRAP_SEED);
    match authority.bootstrap_principal(bootstrap_request(BOOTSTRAP_SEED, &old_key)) {
        Ok(BootstrapDecision::Created(binding) | BootstrapDecision::Replayed(binding)) => binding,
        Err(error) => panic!("bootstrap: {error}"),
    }
}

fn new_public_key() -> [u8; 32] {
    signing_key(NEW_KEY_SEED).verifying_key().to_bytes()
}

/// Row counts of the tables the rotation transaction writes, scoped to one
/// key/domain fixture: `key_rotations`, `key_versions`, `identity_snapshots`,
/// `snapshot_key_bindings`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RotationCounts {
    key_rotations: i64,
    key_versions: i64,
    identity_snapshots: i64,
    snapshot_key_bindings: i64,
}

const BOOTSTRAP_COUNTS: RotationCounts = RotationCounts {
    key_rotations: 0,
    key_versions: 1,
    identity_snapshots: 1,
    snapshot_key_bindings: 1,
};

const ROTATED_COUNTS: RotationCounts = RotationCounts {
    key_rotations: 1,
    key_versions: 2,
    identity_snapshots: 2,
    snapshot_key_bindings: 2,
};

// ---------------------------------------------------------------------------
// test roots, sandbox CWD, fault-VFS open
// ---------------------------------------------------------------------------

struct TestRoot(PathBuf);

impl TestRoot {
    fn new(label: &str) -> Self {
        let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "nlos-identity-rotation-fault-{label}-{}-{suffix}",
            std::process::id()
        ));
        fs::create_dir_all(&base).expect("create test root");
        Self(base)
    }

    fn base(&self) -> &Path {
        &self.0
    }

    fn database(&self) -> PathBuf {
        self.0.join("identity-authority.db")
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct SandboxCwd {
    previous: PathBuf,
    directory: PathBuf,
}

impl SandboxCwd {
    fn new(label: &str) -> Self {
        let suffix = NEXT.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "nlos-identity-rotation-fault-cwd-{label}-{}-{suffix}",
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

fn fault_root(base: &Path) -> String {
    let database = base.join("identity-authority.db");
    let uri_path = database.to_string_lossy().replace('\\', "/");
    let trimmed = uri_path.trim_start_matches('/');
    format!("file:///{trimmed}?vfs={VFS_NAME}&tail=")
}

fn open_fault(base: &Path) -> IdentityAuthority {
    nlos_store_fault::register(VFS_NAME).expect("register fault vfs");
    IdentityAuthority::open(fault_root(base)).expect("open identity authority via fault vfs")
}

fn reopen(base: &Path) -> IdentityAuthority {
    IdentityAuthority::open(base).expect("reopen identity authority")
}

// ---------------------------------------------------------------------------
// shared assertions
// ---------------------------------------------------------------------------

fn error_chain(error: &IdentityAuthorityError) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(" <- ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

fn assert_sqlite_error_chain(error: &IdentityAuthorityError, needles: &[&str]) {
    assert!(
        matches!(error, IdentityAuthorityError::Sqlite(_)),
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

fn assert_rotation_counts(database: &Path, expected: RotationCounts) {
    assert_eq!(
        raw_count(database, "SELECT COUNT(*) FROM key_rotations"),
        expected.key_rotations,
        "unexpected key_rotations count"
    );
    assert_eq!(
        raw_count(database, "SELECT COUNT(*) FROM key_versions"),
        expected.key_versions,
        "unexpected key_versions count"
    );
    assert_eq!(
        raw_count(database, "SELECT COUNT(*) FROM identity_snapshots"),
        expected.identity_snapshots,
        "unexpected identity_snapshots count"
    );
    assert_eq!(
        raw_count(database, "SELECT COUNT(*) FROM snapshot_key_bindings"),
        expected.snapshot_key_bindings,
        "unexpected snapshot_key_bindings count"
    );
}

fn assert_integrity(database: &Path) {
    let connection = Connection::open(database).expect("open for integrity check");
    let result: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("run integrity_check");
    assert_eq!(result, "ok", "integrity_check must pass");
}

fn assert_generation_fence_conflict(authority: &IdentityAuthority, request: RotateKeyRequest) {
    assert!(
        matches!(
            authority.rotate_key(request),
            Err(IdentityAuthorityError::KeyGenerationFenceConflict)
        ),
        "stale generation fence must fail KeyGenerationFenceConflict"
    );
}

fn key_generation_sequence(database: &Path, key_id: KeyId) -> Vec<i64> {
    let connection = Connection::open(database).expect("open raw reader");
    let mut statement = connection
        .prepare("SELECT generation FROM key_versions WHERE key_id=?1 ORDER BY generation")
        .expect("prepare generation query");
    let rows = statement
        .query_map([key_id.as_bytes().as_slice()], |row| row.get::<_, i64>(0))
        .expect("query generations");
    rows.map(|row| row.expect("generation row")).collect()
}

// ---------------------------------------------------------------------------
// kill-9 child-process harness
// ---------------------------------------------------------------------------

fn spawn_child(scenario: &str, root: &TestRoot) -> Child {
    Command::new(std::env::current_exe().expect("current test executable"))
        .args(["--exact", "crash_child_helper", "--nocapture"])
        .env("NLOS_IDENTITY_CRASH_CHILD_SCENARIO", scenario)
        .env("NLOS_IDENTITY_CRASH_CHILD_ROOT", root.base().as_os_str())
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

fn hex_decode32(text: &str) -> [u8; 32] {
    assert_eq!(text.len(), 64, "public key hex is 32 bytes");
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

/// Decodes the `rotate-commit` marker:
/// `READY <key-id> <old-snapshot-id> <new-snapshot-id> <new-public-key> <receipt-id>`.
struct RotateMarker(IdentityBinding, KeyRotationReceipt);

fn decode_rotate_marker(marker: &str) -> RotateMarker {
    let key_id = KeyId::from_bytes(hex_decode16(marker_part(marker, 0)));
    let old_snapshot =
        nlos_types::IdentitySnapshotId::from_bytes(hex_decode16(marker_part(marker, 1)));
    let new_snapshot =
        nlos_types::IdentitySnapshotId::from_bytes(hex_decode16(marker_part(marker, 2)));
    let new_public_key = hex_decode32(marker_part(marker, 3));
    let receipt_id = nlos_types::ReceiptId::from_bytes(hex_decode16(marker_part(marker, 4)));

    let seed = IdentityBinding {
        key_id,
        identity_snapshot_id: old_snapshot,
        snapshot_generation: Generation::INITIAL,
        key_generation: Generation::INITIAL,
        public_key: signing_key(BOOTSTRAP_SEED).verifying_key().to_bytes(),
        key_valid_from_ms: 1_000,
        key_valid_until_ms: 9_000,
        key_revoked_at_ms: None,
        principal_id: nlos_types::PrincipalId::from_bytes([0; 16]),
        control_domain_id: nlos_types::ControlDomainId::from_bytes([0; 16]),
        key_purpose: KeyPurpose::SemanticSigning,
    };
    let receipt = KeyRotationReceipt {
        receipt_id,
        key_id,
        resulting_key_generation: Generation::new(
            std::num::NonZeroU64::new(2).expect("generation 2"),
        ),
        identity_snapshot_id: new_snapshot,
        snapshot_generation: Generation::new(std::num::NonZeroU64::new(2).expect("generation 2")),
        new_public_key,
        new_valid_from_ms: NEW_VALID_FROM_MS,
        new_valid_until_ms: NEW_VALID_UNTIL_MS,
        rotated_at_ms: ROTATED_AT_MS,
    };
    RotateMarker(seed, receipt)
}

#[test]
fn crash_child_helper() {
    let (Ok(scenario), Ok(root)) = (
        std::env::var("NLOS_IDENTITY_CRASH_CHILD_SCENARIO"),
        std::env::var("NLOS_IDENTITY_CRASH_CHILD_ROOT"),
    ) else {
        return;
    };
    let root = PathBuf::from(root);
    match scenario.as_str() {
        "rotate-commit" => child_rotate_commit(&root),
        other => panic!("unknown crash child scenario {other}"),
    }
}

fn child_rotate_commit(root: &Path) -> ! {
    let authority = reopen(root);
    let seed = bootstrap_binding(&authority);
    let request = rotate_request(seed, new_public_key());
    let receipt = rotated(authority.rotate_key(request).expect("child rotate"));
    announce(&format!(
        "READY {} {} {} {} {}",
        hex_encode(seed.key_id.as_bytes()),
        hex_encode(seed.identity_snapshot_id.as_bytes()),
        hex_encode(receipt.identity_snapshot_id.as_bytes()),
        hex_encode(receipt.new_public_key.as_slice()),
        hex_encode(receipt.receipt_id.as_bytes()),
    ));
    let _keeper = authority;
    loop {
        std::thread::park();
    }
}

// ---------------------------------------------------------------------------
// WAL tail truncation
// ---------------------------------------------------------------------------

fn sibling_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

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

fn commit_frames(wal: &[u8]) -> Vec<usize> {
    let (_, frame_size, frame_count) = wal_frame_layout(wal);
    (0..frame_count)
        .filter(|index| {
            let start = 32 + index * frame_size;
            u32::from_be_bytes(wal[start + 4..start + 8].try_into().expect("commit field")) != 0
        })
        .collect()
}

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

/// W1（rotate）：bootstrap 前缀先行落盘，`FailWritesAfter { 0, IoErr }` 注入
/// rotation 单事务提交写入 → typed `Sqlite` 失败；重开后 bootstrap 前缀
/// 保持、零幻影 rotation 行；disarm 后同请求重做 → `Rotated` gen2。
#[test]
#[allow(clippy::too_many_lines)]
fn identity_fault_rotate_precommit_ioerr_fails_typed_and_converges() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let _sandbox = SandboxCwd::new("ioerr");
    let root = TestRoot::new("ioerr");
    let authority = open_fault(root.base());
    let seed = bootstrap_binding(&authority);
    let request = rotate_request(seed, new_public_key());

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::IoErr,
    });
    let error = authority
        .rotate_key(request)
        .expect_err("rotate must fail under injected I/O error");
    assert_sqlite_error_chain(&error, &["i/o", "ioerr"]);
    assert!(nlos_store_fault::writes_observed() > 0);
    assert_rotation_counts(&root.database(), BOOTSTRAP_COUNTS);

    nlos_store_fault::disarm();
    drop(authority);
    let recovered = reopen(root.base());
    assert_eq!(
        recovered
            .inspect_current_binding(seed.key_id)
            .expect("head"),
        seed,
        "head must still point at bootstrap generation"
    );
    assert_rotation_counts(&root.database(), BOOTSTRAP_COUNTS);
    assert_integrity(&root.database());

    let receipt = rotated(
        recovered
            .rotate_key(request)
            .expect("rotate succeeds after disarm"),
    );
    assert_eq!(receipt.resulting_key_generation.get(), 2);
    assert_generation_fence_conflict(
        &recovered,
        RotateKeyRequest {
            idempotency_key: key(0x89),
            ..request
        },
    );
    drop(recovered);
    let verified = reopen(root.base());
    assert_rotation_counts(&root.database(), ROTATED_COUNTS);
    let current = verified
        .inspect_current_binding(seed.key_id)
        .expect("head advanced");
    assert_eq!(current.public_key, receipt.new_public_key);
    assert_integrity(&root.database());
}

/// W2（rotate）：`FailWritesAfter { 0, Full }` 下同一收敛——`SQLITE_FULL`
/// 显式失败、零幻影 rotation 行；disarm 后重试成功且行恰好一套。
#[test]
fn identity_fault_rotate_precommit_enospc_fails_typed_and_converges() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let _sandbox = SandboxCwd::new("full");
    let root = TestRoot::new("full");
    let authority = open_fault(root.base());
    let seed = bootstrap_binding(&authority);
    let request = rotate_request(seed, new_public_key());

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::Full,
    });
    let error = authority
        .rotate_key(request)
        .expect_err("rotate must fail under injected disk-full");
    assert_sqlite_error_chain(&error, &["full"]);
    assert_rotation_counts(&root.database(), BOOTSTRAP_COUNTS);

    nlos_store_fault::disarm();
    let receipt = rotated(
        authority
            .rotate_key(request)
            .expect("rotate succeeds after disarm"),
    );
    drop(authority);
    let verified = reopen(root.base());
    assert_rotation_counts(&root.database(), ROTATED_COUNTS);
    assert_eq!(
        replayed(verified.rotate_key(request).expect("replay after redo")),
        receipt
    );
    assert_integrity(&root.database());
}

// ---------------------------------------------------------------------------
// W3: PowerLossAfter the commit point (rotate)
// ---------------------------------------------------------------------------

/// W3（rotate）：
/// - Phase A（断电不可见）：bootstrap 先行落盘，`PowerLossAfter { 0 }` 下
///   rotate "报告成功"；重开后 head 仍指 gen1、零 rotation 行——rotation 整
///   体不可见；同请求重做 → `Rotated` 与幻影逐字节相等。
/// - Phase B（提交后 kill-9 可见）：子进程 bootstrap+rotate 全部提交后被强
///   杀；重开后 head=gen2；同 key 重放 → `Replayed` 逐字节相等。
#[test]
#[allow(clippy::too_many_lines)]
fn identity_fault_rotate_power_loss_commit_point_converges_both_ways() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    rotate_power_loss_invisible_redo_byte_equal();
    rotate_kill9_after_commit_visible_replay_byte_equal();
}

fn rotate_power_loss_invisible_redo_byte_equal() {
    let _sandbox = SandboxCwd::new("pl-rotate");
    let root = TestRoot::new("pl-rotate");
    let authority = open_fault(root.base());
    let seed = bootstrap_binding(&authority);
    let request = rotate_request(seed, new_public_key());

    nlos_store_fault::arm(FaultMode::PowerLossAfter { remaining: 0 });
    let phantom = rotated(
        authority
            .rotate_key(request)
            .expect("power loss drops writes silently"),
    );
    nlos_store_fault::disarm();
    drop(authority);

    let recovered = reopen(root.base());
    assert_eq!(
        recovered
            .inspect_current_binding(seed.key_id)
            .expect("head"),
        seed,
        "head must still point at bootstrap generation"
    );
    assert_rotation_counts(&root.database(), BOOTSTRAP_COUNTS);
    assert_integrity(&root.database());

    let redone = rotated(
        recovered
            .rotate_key(request)
            .expect("redo rotate after power loss"),
    );
    assert_eq!(
        redone.receipt_id, phantom.receipt_id,
        "redo must be byte-equal to the silently lost receipt"
    );
    assert_eq!(redone, phantom);
    assert_eq!(
        key_generation_sequence(&root.database(), seed.key_id),
        [1, 2]
    );
    drop(recovered);
    assert_rotation_counts(&root.database(), ROTATED_COUNTS);
    assert_integrity(&root.database());
}

fn rotate_kill9_after_commit_visible_replay_byte_equal() {
    let root = TestRoot::new("kill9-rotate");
    let mut child = spawn_child("rotate-commit", &root);
    let marker = await_marker(&mut child);
    let RotateMarker(seed, receipt) = decode_rotate_marker(&marker);
    kill_and_reap(&mut child);

    let recovered = reopen(root.base());
    assert_rotation_counts(&root.database(), ROTATED_COUNTS);
    let current = recovered
        .inspect_current_binding(seed.key_id)
        .expect("committed rotation must survive the kill");
    assert_eq!(current.public_key, receipt.new_public_key);
    assert_eq!(current.key_generation.get(), 2);
    assert_integrity(&root.database());

    let replay = replayed(
        recovered
            .rotate_key(rotate_request(seed, receipt.new_public_key))
            .expect("visible rotation replay"),
    );
    assert_eq!(replay, receipt, "replay must be byte-equal");
    assert_generation_fence_conflict(
        &recovered,
        RotateKeyRequest {
            idempotency_key: key(0x8a),
            ..rotate_request(seed, receipt.new_public_key)
        },
    );
    assert_rotation_counts(&root.database(), ROTATED_COUNTS);
    assert_integrity(&root.database());
}

// ---------------------------------------------------------------------------
// W4: torn WAL tail (rotate)
// ---------------------------------------------------------------------------

/// W4（rotate）：子进程 bootstrap+rotate 提交后被强杀，WAL 在最后 commit 帧
/// （rotation 事务）半帧处截断；重开后 rotation 整体消失——head 仍指
/// gen1、零 rotation 行；同请求重做 → `Rotated` 与子进程宣告逐字节相等。
#[test]
#[allow(clippy::too_many_lines)]
fn identity_fault_rotate_torn_wal_tail_discards_and_redo_converges() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let root = TestRoot::new("torn-rotate");
    let mut child = spawn_child("rotate-commit", &root);
    let marker = await_marker(&mut child);
    let RotateMarker(seed, receipt) = decode_rotate_marker(&marker);
    kill_and_reap(&mut child);

    truncate_wal_inside_last_commit(&root.database());

    let recovered = reopen(root.base());
    assert_rotation_counts(&root.database(), BOOTSTRAP_COUNTS);
    let current = recovered
        .inspect_current_binding(seed.key_id)
        .expect("head after torn tail");
    assert_eq!(current.key_generation, Generation::INITIAL);
    assert_eq!(
        current.public_key,
        signing_key(BOOTSTRAP_SEED).verifying_key().to_bytes(),
        "rotation must be discarded whole — head must retain bootstrap material"
    );
    assert_eq!(
        current.identity_snapshot_id, seed.identity_snapshot_id,
        "current snapshot must roll back to bootstrap"
    );
    assert_integrity(&root.database());

    let bootstrap = recovered
        .inspect_current_binding(seed.key_id)
        .expect("bootstrap head");
    let request = rotate_request(bootstrap, receipt.new_public_key);
    let redone = rotated(
        recovered
            .rotate_key(request)
            .expect("redo rotate after torn tail"),
    );
    assert_eq!(redone, receipt, "redo must match the killed transaction");
    drop(recovered);
    let verified = reopen(root.base());
    assert_eq!(
        replayed(verified.rotate_key(request).expect("replay after redo")),
        receipt
    );
    assert_rotation_counts(&root.database(), ROTATED_COUNTS);
    assert_integrity(&root.database());
}

// ---------------------------------------------------------------------------
// W5: replay storm
// ---------------------------------------------------------------------------

/// W5（rotate）：rotation 后同请求连放 3 次 + 重开后再放 1 次 → 每次
/// `Replayed` 逐字节相等；同 key 不同 fence 恒 `IdempotencyConflict`；
/// 恰好一套 rotation 增量行，同 key 不双 rotate。
#[test]
fn identity_fault_rotate_replay_storm_is_idempotent() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let root = TestRoot::new("storm-rotate");
    let authority = reopen(root.base());
    let seed = bootstrap_binding(&authority);
    let request = rotate_request(seed, new_public_key());
    let receipt = rotated(authority.rotate_key(request).expect("rotate"));

    for _ in 0..3 {
        assert_eq!(
            replayed(authority.rotate_key(request).expect("storm replay")),
            receipt,
            "every storm replay is byte-equal"
        );
    }
    assert!(
        matches!(
            authority.rotate_key(RotateKeyRequest {
                new_public_key: signing_key(0x99).verifying_key().to_bytes(),
                ..request
            }),
            Err(IdentityAuthorityError::IdempotencyConflict)
        ),
        "same key with different material must keep failing mid-storm"
    );
    drop(authority);
    let verified = reopen(root.base());
    assert_eq!(
        replayed(verified.rotate_key(request).expect("replay after reopen")),
        receipt
    );
    assert_rotation_counts(&root.database(), ROTATED_COUNTS);
    assert_eq!(
        key_generation_sequence(&root.database(), seed.key_id),
        [1, 2],
        "storm must not double-rotate the same key"
    );
    assert_integrity(&root.database());
}
