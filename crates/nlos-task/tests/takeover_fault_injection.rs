//! B-TASK-008C2G-COORD schema-v27..v35 fault-injection tests: the authority
//! lease / takeover-fence table group under the PoC-0003-aligned F1-F4 fault
//! matrix, closing the "完整故障矩阵仍未接入" gap recorded by
//! `b-task-008c2g-semantic-coordinator.md`.
//!
//! The group under test is the durable local takeover chain landed in schema
//! v27..v35: `task_authority_leases` / `task_authority_lease_history`,
//! `task_authority_takeover_fence_receipts`,
//! `task_authority_takeover_fence_members`, `task_authority_assignments`,
//! `task_authority_takeover_receipts` and
//! `task_authority_takeover_barrier_receipts` (v35 digest column), plus the
//! two CAS dirt targets they touch: `task_participant_registries.registry_state`
//! and `tasks.control_epoch`. The harness reuses the `nlos-store-fault` VFS
//! patterns established by `fault_injection.rs` (B-TASK-001),
//! `effect_fault_injection.rs` (B-TASK-002) and `reconcile_fault_injection.rs`
//! (B-TASK-003): kill-9 child processes synchronized through piped `READY`
//! markers (never sleeps), `FAULT_LOCK` process-wide serialization,
//! `wal_commit_frames` tail truncation, typed error-chain assertions, raw
//! table-level counts, and a `PRAGMA integrity_check` re-verification at the
//! end of every scenario.
//!
//! Covered flows (public API only):
//! - authority lease acquire/renew/take-over (`acquire_authority_lease`);
//! - lease-bound `CommitPermit` issuance (assignment baseline) and plain
//!   lease-bound finalize;
//! - `prepare_authority_takeover_fence` (registry freeze + fence receipt +
//!   exact roots + member manifest + assignment `TakeoverPending` + pending
//!   takeover receipt in one transaction);
//! - `record_authority_takeover_barrier_receipt` (v35 digest observation).
//!
//! **Crash semantics disclaimer**: the kill-9 rows use forced child
//! termination to simulate *process* crashes; the OS page cache survives a
//! process death, so a killed process is NOT a machine power loss. Writes the
//! kernel accepted but the disk never saw are covered by
//! [`FaultMode::PowerLossAfter`] and by file-level WAL tail truncation. Child
//! processes are synchronized through piped stdout markers, never through
//! sleeps. The fault state in `nlos-store-fault` is process-global, so every
//! test holds `FAULT_LOCK` for its entire duration.

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
    AttemptRegistrationDecision, AttemptSpec, AuthorityAssignmentRecord, AuthorityAssignmentState,
    AuthorityLeaseDecision, AuthorityLeaseFinalizeRequest, AuthorityLeasePermitRequest,
    AuthorityLeaseRecord, AuthorityLeaseRequest, AuthorityLeaseTakeoverFenceRequest,
    AuthorityTakeoverBarrierCoverageState, AuthorityTakeoverBarrierReceiptRequest,
    AuthorityTakeoverFenceMemberRecord, AuthorityTakeoverReceiptRecord, FinalizeDecision,
    FinalizeRequest, FinalizeRequestV3, ParticipantRegistryBinding, ParticipantRegistryState,
    PermitDecision, PermitRecord, PermitRequest, SnapshotBundle, SqliteTaskAuthority,
    TaskRegistrationDecision, TaskSpec, TaskStoreError, empty_effect_history_root,
};
use nlos_types::{
    CancellationScopeId, Generation, IdempotencyKey, ProcessId, ReceiptId, TaskAttemptId, TaskId,
    TaskSnapshotId,
};

const VFS_NAME: &str = "nlos-task-takeover-fault";

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
        Self {
            path: std::env::temp_dir().join(format!(
                "nlos-task-takeover-fault-{name}-{}-{sequence}.sqlite3",
                std::process::id()
            )),
        }
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

/// The pre-takeover committed prefix carries exactly two lease history rows
/// (term-1 acquire + term-2 takeover) and exactly one Active assignment
/// (created by the lease-bound permit issuance); every takeover-chain table
/// must be empty: no phantom fence receipt / member / takeover receipt /
/// barrier row.
fn assert_no_phantom_takeover_rows(path: &Path) {
    assert_eq!(raw_count(path, "task_authority_lease_history"), 2);
    assert_eq!(raw_count(path, "task_authority_assignments"), 1);
    assert_eq!(raw_count(path, "task_authority_takeover_fence_receipts"), 0);
    assert_eq!(raw_count(path, "task_authority_takeover_fence_members"), 0);
    assert_eq!(raw_count(path, "task_authority_takeover_receipts"), 0);
    assert_eq!(
        raw_count(path, "task_authority_takeover_barrier_receipts"),
        0
    );
}

fn bytes(value: u8) -> [u8; 16] {
    [value; 16]
}

fn process(seed: u8) -> ProcessId {
    ProcessId::from_bytes(bytes(seed))
}

fn lease_request(holder: u8, key: u8, at_ms: i64, ttl_ms: i64) -> AuthorityLeaseRequest {
    AuthorityLeaseRequest {
        holder_id: process(holder),
        idempotency_key: IdempotencyKey::from_bytes([key; 16]),
        requested_at_ms: at_ms,
        ttl_ms,
    }
}

fn lease_record(decision: AuthorityLeaseDecision) -> AuthorityLeaseRecord {
    decision.record()
}

fn task_id() -> TaskId {
    TaskId::from_bytes(bytes(0x01))
}

/// Pure constructor of the shared attempt spec; does not touch the database
/// (used by parent-side replay paths on an already-seeded database).
fn attempt_spec() -> AttemptSpec {
    AttemptSpec {
        task_id: task_id(),
        attempt_id: TaskAttemptId::from_bytes(bytes(0x02)),
        attempt_generation: Generation::INITIAL,
        snapshot: SnapshotBundle {
            snapshot_id: TaskSnapshotId::from_bytes(bytes(0x03)),
            snapshot_digest: [0x04; 32],
            expected_head_commit_seq: 0,
            effect_history_root: empty_effect_history_root(),
            retry_fence_epoch: 0,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes(bytes(0x05)),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes(bytes(0x06)),
        registered_at_ms: 2,
    }
}

fn register_task_attempt(authority: &SqliteTaskAuthority) -> AttemptSpec {
    assert!(matches!(
        authority.register_task(TaskSpec {
            task_id: task_id(),
            task_generation: Generation::INITIAL,
            registered_at_ms: 1,
        }),
        Ok(TaskRegistrationDecision::Created(_))
    ));
    let attempt = attempt_spec();
    assert!(matches!(
        authority.register_attempt(attempt),
        Ok(AttemptRegistrationDecision::Created(_))
    ));
    attempt
}

fn permit_request(attempt: &AttemptSpec, key: u8, requested_at_ms: i64) -> PermitRequest {
    PermitRequest {
        task_id: attempt.task_id,
        attempt_id: attempt.attempt_id,
        attempt_generation: attempt.attempt_generation,
        write_set_root: [key; 32],
        planned_effects: Vec::new(),
        idempotency_key: IdempotencyKey::from_bytes([key.wrapping_add(0x10); 16]),
        valid_until_ms: 10_000,
        requested_at_ms,
    }
}

fn finalize_request(
    attempt: &AttemptSpec,
    permit_id: nlos_types::CommitPermitId,
) -> FinalizeRequestV3 {
    FinalizeRequestV3 {
        base: FinalizeRequest {
            task_id: attempt.task_id,
            attempt_id: attempt.attempt_id,
            attempt_generation: attempt.attempt_generation,
            permit_id,
            new_effect_history_root: [0; 32],
            new_retry_fence_epoch: 0,
            finalized_at_ms: 160,
        },
        required_satisfaction: Vec::new(),
        fenced_participant_digest: [0; 32],
    }
}

fn issued_permit(decision: PermitDecision) -> PermitRecord {
    match decision {
        PermitDecision::Issued(record) => *record,
        other => panic!("expected Issued, got {other:?}"),
    }
}

fn replayed_permit(decision: PermitDecision) -> PermitRecord {
    match decision {
        PermitDecision::Replayed(record) => *record,
        other => panic!("expected Replayed, got {other:?}"),
    }
}

/// The shared committed prefix every scenario starts from:
/// register task/attempt -> lease term 1 -> lease-bound permit issuance
/// (Active assignment baseline, registry binding captured) -> lease-bound
/// finalize -> expired lease taken over by holder 2 (term 2).
struct TakeoverPrefix {
    spec: AttemptSpec,
    lease_two: AuthorityLeaseRecord,
    registry_binding: ParticipantRegistryBinding,
}

fn seed_prefix(authority: &SqliteTaskAuthority) -> TakeoverPrefix {
    let spec = register_task_attempt(authority);
    let lease_one = lease_record(
        authority
            .acquire_authority_lease(lease_request(1, 0xa1, 100, 100))
            .expect("initial lease"),
    );
    let permit = issued_permit(
        authority
            .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
                permit: permit_request(&spec, 0xb1, 150),
                lease: lease_one,
            })
            .expect("lease-bound permit"),
    );
    let registry_binding = permit
        .participant_registry_binding
        .expect("permit registry binding");
    match authority
        .finalize_commit_v3_with_authority_lease(AuthorityLeaseFinalizeRequest {
            finalize: finalize_request(&spec, permit.permit_id),
            lease: lease_one,
        })
        .expect("plain lease-bound finalize")
    {
        FinalizeDecision::Committed(_) => {}
        other @ FinalizeDecision::Replayed(_) => panic!("expected Committed, got {other:?}"),
    }
    let lease_two = lease_record(
        authority
            .acquire_authority_lease(lease_request(2, 0xa2, 201, 100))
            .expect("expired lease takeover"),
    );
    assert_eq!(lease_two.term, lease_one.term + 1);
    TakeoverPrefix {
        spec,
        lease_two,
        registry_binding,
    }
}

/// Runs the takeover fence transaction (one durable transaction: registry
/// freeze + fence receipt + exact roots + member manifest + assignment
/// `TakeoverPending` + pending takeover receipt) and returns the frozen
/// registry record.
fn fence_takeover(
    authority: &SqliteTaskAuthority,
    prefix: &TakeoverPrefix,
    requested_at_ms: i64,
) -> nlos_task::ParticipantRegistryRecord {
    authority
        .prepare_authority_takeover_fence(AuthorityLeaseTakeoverFenceRequest {
            task_id: prefix.spec.task_id,
            expected_registry_binding: prefix.registry_binding,
            lease: prefix.lease_two,
            requested_at_ms,
        })
        .expect("freeze current registry")
}

/// Records one immutable endpoint barrier observation for the first member
/// of the fence manifest.
fn barrier_observation(
    authority: &SqliteTaskAuthority,
    prefix: &TakeoverPrefix,
    takeover: &AuthorityTakeoverReceiptRecord,
    observed_at_ms: i64,
    remote_seed: u8,
    digest_seed: u8,
) -> nlos_task::AuthorityTakeoverBarrierReceiptRecord {
    let members = authority
        .inspect_authority_takeover_fence_members(prefix.spec.task_id, prefix.registry_binding)
        .expect("fence member manifest");
    let participant = members.first().expect("fence member").participant;
    authority
        .record_authority_takeover_barrier_receipt(AuthorityTakeoverBarrierReceiptRequest {
            takeover_receipt_id: takeover.receipt_id,
            participant,
            remote_receipt_id: ReceiptId::from_bytes([remote_seed; 16]),
            barrier_digest: [digest_seed; 32],
            observed_at_ms,
        })
        .expect("record endpoint barrier observation")
}

fn assignment(authority: &SqliteTaskAuthority) -> AuthorityAssignmentRecord {
    authority
        .inspect_authority_assignment(task_id())
        .expect("assignment")
}

// ---------------------------------------------------------------------------
// kill-9 child-process harness (fault_injection.rs / reconcile_fault_injection.rs
// 范式: current_exe + env var + piped READY marker, never sleeps)
// ---------------------------------------------------------------------------

fn spawn_child(scenario: &str, path: &Path) -> Child {
    Command::new(std::env::current_exe().expect("current test executable"))
        .args(["--exact", "crash_child_helper", "--nocapture"])
        .env("NLOS_TAKEOVER_CRASH_CHILD_SCENARIO", scenario)
        .env("NLOS_TAKEOVER_CRASH_CHILD_DATABASE", path)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn crash child")
}

/// Blocks until the child prints its `READY` marker (pipe synchronization,
/// no sleeps); kills and reaps the child on timeout or early exit.
fn await_marker(child: &mut Child) {
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
        Ok(Ok(_)) => {}
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

// ---------------------------------------------------------------------------
// kill-9 child scenarios
// ---------------------------------------------------------------------------

/// Matrix row 1 fixture: the lease-bound permit prefix is committed, then a
/// writer transaction dirties the takeover table group (phantom fence
/// receipt with the real registry binding, phantom member, phantom Active
/// assignment, phantom pending takeover receipt, phantom barrier observation,
/// phantom term-2 lease history) plus the `commit_permits.permit_state` CAS
/// dirt, and dies before commit.
#[allow(clippy::too_many_lines)] // One test covers a full F1-F4 fault-matrix row.
fn child_mid_takeover_tx(path: &Path) -> ! {
    let authority = SqliteTaskAuthority::open(path).expect("open");
    let prefix = seed_prefix(&authority);
    let raw = rusqlite::Connection::open(path).expect("raw connection");
    raw.execute_batch("BEGIN IMMEDIATE").expect("begin mid-tx");
    raw.execute(
        "UPDATE commit_permits SET permit_state = permit_state + 100",
        [],
    )
    .expect("mid-tx permit CAS dirt");
    let task_hex = "01010101010101010101010101010101";
    let generation_hex = "0000000000000001";
    let authority_id_hex = hex_encode(
        authority
            .inspect_authority_lease()
            .expect("current lease")
            .authority_id
            .as_bytes(),
    );
    let registry_generation_hex = format!("{:016x}", prefix.registry_binding.generation);
    let registry_root_hex = hex_encode(&prefix.registry_binding.root);
    // A phantom fence receipt that would collide with the real one if it
    // survived: same (task_id, frozen generation, frozen root) uniqueness key.
    raw.execute_batch(&format!(
        "INSERT INTO task_authority_takeover_fence_receipts (
            receipt_id, task_id, task_generation, frozen_registry_generation,
            frozen_registry_root, authority_lease_authority_id,
            authority_lease_holder_id, authority_lease_term,
            authority_lease_epoch, authority_lease_fencing_token,
            authority_lease_expires_at_ms, control_epoch,
            exact_fence_set_root, outstanding_operation_participant_root,
            created_at_ms
         ) VALUES (
            X'99999999999999999999999999999999', X'{task_hex}', X'{generation_hex}',
            X'{registry_generation_hex}', X'{registry_root_hex}',
            X'11111111111111111111111111111111', X'22222222222222222222222222222222',
            X'0000000000000002', X'0000000000000003',
            X'3333333333333333333333333333333333333333333333333333333333333333',
            300, X'0000000000000001',
            X'4444444444444444444444444444444444444444444444444444444444444444',
            X'5555555555555555555555555555555555555555555555555555555555555555', 210
         )"
    ))
    .expect("mid-tx phantom fence receipt");
    raw.execute_batch(&format!(
        "INSERT INTO task_authority_takeover_fence_members (
            fence_receipt_id, task_id, task_generation,
            participant_type, participant_id, participant_generation,
            admission_receipt_id
         ) VALUES (
            X'99999999999999999999999999999999', X'{task_hex}', X'{generation_hex}',
            1, X'66666666666666666666666666666666', X'{generation_hex}',
            X'77777777777777777777777777777777'
         )"
    ))
    .expect("mid-tx phantom fence member");
    raw.execute_batch(&format!(
        "INSERT INTO task_authority_assignments (
            assignment_id, task_id, task_generation, authority_id,
            authority_lease_holder_id, authority_lease_term,
            authority_lease_epoch, authority_lease_fencing_token,
            authority_lease_expires_at_ms, control_epoch,
            participant_registry_generation, participant_registry_root,
            assignment_state, created_at_ms, updated_at_ms
         ) VALUES (
            X'88888888888888888888888888888888', X'{task_hex}', X'{generation_hex}',
            X'11111111111111111111111111111111', X'22222222222222222222222222222222',
            X'0000000000000002', X'0000000000000003',
            X'3333333333333333333333333333333333333333333333333333333333333333',
            300, X'0000000000000001',
            X'{registry_generation_hex}', X'{registry_root_hex}',
            1, 210, 210
         )"
    ))
    .expect("mid-tx phantom assignment");
    raw.execute_batch(&format!(
        "INSERT INTO task_authority_takeover_receipts (
            receipt_id, task_id, task_generation,
            old_assignment_id, new_assignment_id, fence_receipt_id,
            frozen_old_authority_term, frozen_old_control_epoch,
            new_authority_id, new_authority_lease_holder_id,
            new_authority_lease_term, new_authority_lease_epoch,
            new_authority_lease_fencing_token,
            new_authority_lease_expires_at_ms, new_control_epoch,
            frozen_registry_generation, frozen_registry_root,
            exact_fence_set_root, outstanding_operation_participant_root,
            barrier_state, created_at_ms
         ) VALUES (
            X'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa', X'{task_hex}', X'{generation_hex}',
            X'88888888888888888888888888888888', NULL, X'99999999999999999999999999999999',
            X'0000000000000001', X'0000000000000000',
            X'11111111111111111111111111111111', X'22222222222222222222222222222222',
            X'0000000000000002', X'0000000000000003',
            X'3333333333333333333333333333333333333333333333333333333333333333',
            300, X'0000000000000002',
            X'{registry_generation_hex}', X'{registry_root_hex}',
            X'4444444444444444444444444444444444444444444444444444444444444444',
            X'5555555555555555555555555555555555555555555555555555555555555555', 1, 210
         )"
    ))
    .expect("mid-tx phantom takeover receipt");
    raw.execute_batch(&format!(
        "INSERT INTO task_authority_takeover_barrier_receipts (
            receipt_id, takeover_receipt_id, task_id, task_generation,
            participant_type, participant_id, participant_generation,
            admission_receipt_id, remote_receipt_id, barrier_receipt_digest,
            fence_set_root, barrier_state, observed_at_ms
         ) VALUES (
            X'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb', X'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
            X'{task_hex}', X'{generation_hex}',
            1, X'66666666666666666666666666666666', X'{generation_hex}',
            X'77777777777777777777777777777777', X'cccccccccccccccccccccccccccccccc',
            X'dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd',
            X'4444444444444444444444444444444444444444444444444444444444444444', 1, 213
         )"
    ))
    .expect("mid-tx phantom barrier observation");
    raw.execute_batch(&format!(
        "INSERT INTO task_authority_lease_history (
            authority_id, lease_epoch, term, holder_id, fencing_token,
            idempotency_key, requested_at_ms, expires_at_ms, ttl_ms,
            transition_kind
         ) VALUES (
            X'{authority_id_hex}', X'0000000000000003',
            X'0000000000000002', X'22222222222222222222222222222222',
            X'3333333333333333333333333333333333333333333333333333333333333333',
            X'ffffffffffffffffffffffffffffffff', 201, 301, 100, 3
         )"
    ))
    .expect("mid-tx phantom lease history");
    announce("READY");
    let _keepers = (authority, raw);
    loop {
        std::thread::park();
    }
}

/// Matrix row 2 fixture: the complete committed takeover chain — term-2
/// lease, fence receipt with exact roots, member manifest, assignment
/// `TakeoverPending`, pending takeover receipt and one v35 barrier
/// observation — before the kill.
fn child_takeover_commit_complete(path: &Path) -> ! {
    let authority = SqliteTaskAuthority::open(path).expect("open");
    let prefix = seed_prefix(&authority);
    fence_takeover(&authority, &prefix, 210);
    let fence = authority
        .inspect_authority_takeover_fence_receipt(task_id(), prefix.registry_binding)
        .expect("fence receipt");
    let takeover = authority
        .inspect_authority_takeover_receipt(task_id(), fence.receipt_id)
        .expect("takeover receipt");
    barrier_observation(&authority, &prefix, &takeover, 213, 0x91, 0x92);
    announce("READY");
    let _keeper = authority;
    loop {
        std::thread::park();
    }
}

/// Torn-tail fixture A: the takeover fence transaction (registry freeze +
/// fence receipt + members + assignment `TakeoverPending` + pending takeover
/// receipt) is the last committed transaction before the kill.
fn child_torn_wal_fence(path: &Path) -> ! {
    let authority = SqliteTaskAuthority::open(path).expect("open");
    let prefix = seed_prefix(&authority);
    fence_takeover(&authority, &prefix, 210);
    announce("READY");
    let _keeper = authority;
    loop {
        std::thread::park();
    }
}

/// Torn-tail fixture B: the barrier observation is the last committed
/// transaction (the full fence chain is already durable).
fn child_torn_wal_barrier(path: &Path) -> ! {
    let authority = SqliteTaskAuthority::open(path).expect("open");
    let prefix = seed_prefix(&authority);
    fence_takeover(&authority, &prefix, 210);
    let fence = authority
        .inspect_authority_takeover_fence_receipt(task_id(), prefix.registry_binding)
        .expect("fence receipt");
    let takeover = authority
        .inspect_authority_takeover_receipt(task_id(), fence.receipt_id)
        .expect("takeover receipt");
    barrier_observation(&authority, &prefix, &takeover, 213, 0x91, 0x92);
    announce("READY");
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
        std::env::var("NLOS_TAKEOVER_CRASH_CHILD_SCENARIO"),
        std::env::var("NLOS_TAKEOVER_CRASH_CHILD_DATABASE"),
    ) else {
        return;
    };
    let path = PathBuf::from(path);
    match scenario.as_str() {
        "mid-takeover-tx" => child_mid_takeover_tx(&path),
        "takeover-commit-complete" => child_takeover_commit_complete(&path),
        "torn-wal-fence" => child_torn_wal_fence(&path),
        "torn-wal-barrier" => child_torn_wal_barrier(&path),
        other => panic!("unknown crash child scenario {other}"),
    }
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
// 矩阵行 1: kill-9 mid-transaction on the takeover table group
// ---------------------------------------------------------------------------

/// kill-9 中断 takeover 表组写事务：子进程在 `BEGIN IMMEDIATE` 未提交（已写入
/// 幻影 fence receipt / member / assignment / takeover receipt / barrier
/// observation / term-2 lease history，并弄脏 `commit_permits.permit_state`
/// CAS）时被强杀；重开后中断事务完全回滚——接管六表无幻影行、permit 回到
/// 已提交的 `ISSUED`、registry 保持未冻结（`FrozenForPermit`）、
/// `control_epoch` 不动；随后同一 takeover fence 重做成功且确定性派生
/// receipt id 一致，重放不再推进 control epoch。
#[test]
#[allow(clippy::too_many_lines)] // One test covers a full F1-F4 fault-matrix row.
fn fault_kill9_mid_takeover_tx_leaves_no_half_state() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestDatabase::new("kill9-mid-takeover-tx");
    let mut child = spawn_child("mid-takeover-tx", &database.path);
    await_marker(&mut child);
    kill_and_reap(&mut child);

    // Nothing uncommitted may survive: the takeover table group shows only
    // the pre-fence prefix (one lease history, one Active assignment).
    assert_no_phantom_takeover_rows(&database.path);

    let authority = SqliteTaskAuthority::open(&database.path).expect("reopen after kill");
    let spec = attempt_spec();
    let permit = replayed_permit(
        authority
            .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
                permit: permit_request(&spec, 0xb1, 150),
                lease: lease_record(
                    authority
                        .acquire_authority_lease(lease_request(1, 0xa1, 100, 100))
                        .expect("replay lease one"),
                ),
            })
            .expect("replay permit"),
    );
    assert_eq!(
        permit.state,
        nlos_task::PermitState::Closed,
        "mid-transaction permit CAS dirt must be rolled back"
    );
    let registry_binding = permit
        .participant_registry_binding
        .expect("permit registry binding");
    assert_eq!(
        authority
            .inspect_participant_registry(task_id())
            .expect("registry")
            .state,
        ParticipantRegistryState::FrozenForPermit,
        "mid-transaction registry freeze dirt must be rolled back"
    );
    let control_before = authority
        .inspect_task(task_id())
        .expect("head")
        .control_epoch;
    let assignment_before = assignment(&authority);
    assert_eq!(assignment_before.state, AuthorityAssignmentState::Active);

    // The interrupted decision is redoable from the committed prefix: the
    // term-2 lease + takeover fence now succeed and derive the same
    // deterministic fence receipt on replay.
    let lease_two = lease_record(
        authority
            .acquire_authority_lease(lease_request(2, 0xa2, 201, 100))
            .expect("expired lease takeover"),
    );
    let frozen = authority
        .prepare_authority_takeover_fence(AuthorityLeaseTakeoverFenceRequest {
            task_id: task_id(),
            expected_registry_binding: registry_binding,
            lease: lease_two,
            requested_at_ms: 210,
        })
        .expect("freeze after rollback");
    assert_eq!(frozen.state, ParticipantRegistryState::FrozenForTakeover);
    assert_eq!(
        authority
            .inspect_task(task_id())
            .expect("head")
            .control_epoch,
        control_before + 1,
        "the real fence advances the control epoch exactly once"
    );
    let fence = authority
        .inspect_authority_takeover_fence_receipt(task_id(), registry_binding)
        .expect("fence receipt");
    assert!(fence.exact_fence_set_root.is_some());
    assert_eq!(fence.outstanding_operation_participant_root, Some([0; 32]));
    assert!(!fence_members(&authority, registry_binding).is_empty());
    let takeover = authority
        .inspect_authority_takeover_receipt(task_id(), fence.receipt_id)
        .expect("takeover receipt");
    assert_eq!(takeover.new_assignment_id, None);
    assert_eq!(
        assignment(&authority).state,
        AuthorityAssignmentState::TakeoverPending
    );
    assert_eq!(
        raw_count(&database.path, "task_authority_takeover_fence_receipts"),
        1
    );
    assert_eq!(
        raw_count(&database.path, "task_authority_takeover_receipts"),
        1
    );

    // Fence replay is read-only: same registry, control epoch not advanced
    // twice, same deterministic receipt.
    let replayed = authority
        .prepare_authority_takeover_fence(AuthorityLeaseTakeoverFenceRequest {
            task_id: task_id(),
            expected_registry_binding: registry_binding,
            lease: lease_two,
            requested_at_ms: 211,
        })
        .expect("fence replay");
    assert_eq!(replayed, frozen);
    assert_eq!(
        authority
            .inspect_task(task_id())
            .expect("head")
            .control_epoch,
        control_before + 1,
        "fence replay must not advance the control epoch twice"
    );
    assert_eq!(
        authority
            .inspect_authority_takeover_fence_receipt(task_id(), registry_binding)
            .expect("fence receipt after replay")
            .receipt_id,
        fence.receipt_id
    );
    assert_eq!(
        authority
            .inspect_authority_takeover_receipt(task_id(), fence.receipt_id)
            .expect("takeover receipt after replay"),
        takeover
    );
    assert_integrity(&database.path);
}

fn fence_members(
    authority: &SqliteTaskAuthority,
    registry_binding: ParticipantRegistryBinding,
) -> Vec<AuthorityTakeoverFenceMemberRecord> {
    authority
        .inspect_authority_takeover_fence_members(task_id(), registry_binding)
        .expect("fence member manifest")
}

// ---------------------------------------------------------------------------
// 矩阵行 2: kill-9 after commit — the committed takeover chain survives
// ---------------------------------------------------------------------------

/// commit 后崩溃：子进程在完整 takeover 链（term-2 lease、fence receipt 含
/// exact roots、member manifest、assignment `TakeoverPending`、pending
/// takeover receipt、v35 barrier observation）全部提交返回后被强杀；重开后
/// 全部逐位保留（registry `FrozenForTakeover`、`control_epoch` 恰好 +1、
/// barrier digest 持久）；fence 与 barrier 重放返回原结果、control epoch
/// 不再推进、无重复 observation。
#[test]
#[allow(clippy::too_many_lines)] // One test covers a full F1-F4 fault-matrix row.
fn fault_kill9_after_takeover_commit_preserves_everything() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestDatabase::new("kill9-takeover-commit");
    let mut child = spawn_child("takeover-commit-complete", &database.path);
    await_marker(&mut child);
    kill_and_reap(&mut child);

    assert_eq!(raw_count(&database.path, "task_authority_lease_history"), 2);
    assert_eq!(
        raw_count(&database.path, "task_authority_takeover_fence_receipts"),
        1
    );
    assert_eq!(
        raw_count(&database.path, "task_authority_takeover_receipts"),
        1
    );
    assert_eq!(
        raw_count(&database.path, "task_authority_takeover_barrier_receipts"),
        1
    );

    let authority = SqliteTaskAuthority::open(&database.path).expect("reopen after kill");
    let spec = attempt_spec();
    let permit = replayed_permit(
        authority
            .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
                permit: permit_request(&spec, 0xb1, 150),
                lease: lease_record(
                    authority
                        .acquire_authority_lease(lease_request(1, 0xa1, 100, 100))
                        .expect("replay lease one"),
                ),
            })
            .expect("replay permit"),
    );
    let registry_binding = permit
        .participant_registry_binding
        .expect("permit registry binding");

    assert_eq!(
        authority
            .inspect_participant_registry(task_id())
            .expect("registry")
            .state,
        ParticipantRegistryState::FrozenForTakeover
    );
    let fence = authority
        .inspect_authority_takeover_fence_receipt(task_id(), registry_binding)
        .expect("fence receipt durable");
    assert!(fence.exact_fence_set_root.is_some());
    assert_eq!(fence.outstanding_operation_participant_root, Some([0; 32]));
    let members = fence_members(&authority, registry_binding);
    assert!(!members.is_empty());
    let takeover = authority
        .inspect_authority_takeover_receipt(task_id(), fence.receipt_id)
        .expect("takeover receipt durable");
    assert_eq!(takeover.new_assignment_id, None);
    assert_eq!(takeover.exact_fence_set_root, fence.exact_fence_set_root);
    assert_eq!(
        takeover.frozen_old_authority_term,
        authority
            .inspect_authority_lease()
            .expect("live lease")
            .term
            - 1
    );
    assert_eq!(
        assignment(&authority).state,
        AuthorityAssignmentState::TakeoverPending
    );
    let control_after = authority
        .inspect_task(task_id())
        .expect("head")
        .control_epoch;

    let barrier = authority
        .inspect_authority_takeover_barrier_receipts(takeover.receipt_id)
        .expect("barrier observations durable");
    assert_eq!(barrier.len(), 1);
    assert_eq!(barrier[0].barrier_digest, Some([0x92; 32]));
    assert_eq!(
        barrier[0].fence_set_root,
        takeover.exact_fence_set_root.expect("exact root")
    );
    let coverage = authority
        .inspect_authority_takeover_barrier_coverage(takeover.receipt_id)
        .expect("coverage durable");
    assert_eq!(
        coverage.state,
        AuthorityTakeoverBarrierCoverageState::LocallyCovered
    );

    // Replays after the crash return the original durable decisions.
    let lease_two = lease_record(
        authority
            .acquire_authority_lease(lease_request(2, 0xa2, 201, 100))
            .expect("replay lease two"),
    );
    let replayed_fence = authority
        .prepare_authority_takeover_fence(AuthorityLeaseTakeoverFenceRequest {
            task_id: task_id(),
            expected_registry_binding: registry_binding,
            lease: lease_two,
            requested_at_ms: 210,
        })
        .expect("fence replay after kill");
    assert_eq!(
        replayed_fence.state,
        ParticipantRegistryState::FrozenForTakeover
    );
    assert_eq!(
        authority
            .inspect_task(task_id())
            .expect("head")
            .control_epoch,
        control_after,
        "takeover fence replay must not advance control epoch again"
    );
    let participant = members.first().expect("member").participant;
    let replayed_barrier = authority
        .record_authority_takeover_barrier_receipt(AuthorityTakeoverBarrierReceiptRequest {
            takeover_receipt_id: takeover.receipt_id,
            participant,
            remote_receipt_id: ReceiptId::from_bytes([0x91; 16]),
            barrier_digest: [0x92; 32],
            observed_at_ms: 213,
        })
        .expect("barrier replay after kill");
    assert_eq!(replayed_barrier, barrier[0]);
    assert_eq!(
        authority
            .inspect_authority_takeover_barrier_receipts(takeover.receipt_id)
            .expect("observations after replay")
            .len(),
        1,
        "exact replay must not duplicate the observation"
    );
    assert_eq!(
        raw_count(&database.path, "task_authority_takeover_barrier_receipts"),
        1
    );
    assert_integrity(&database.path);
}

// ---------------------------------------------------------------------------
// 矩阵行 3: hard I/O error on takeover writes fails closed
// ---------------------------------------------------------------------------

/// 写入硬 I/O 错误（takeover fence 与 barrier 事务）：
/// `FailWritesAfter { 0, IoErr }` 下（a）`prepare_authority_takeover_fence`
/// 事务（registry 冻结 + fence receipt + members + assignment 状态 + takeover
/// receipt 同事务）与（b）`record_authority_takeover_barrier_receipt`
/// 事务都必须以 `TaskStoreError::Sqlite` 显式失败（错误链含 I/O 条件），
/// 不返回假成功；无半截状态（无 fence/member/takeover/barrier 行、registry
/// 保持 `FrozenForPermit`、assignment 保持 `Active`、`control_epoch` 不动）；
/// disarm 后同一操作成功。
#[test]
#[allow(clippy::too_many_lines)] // One test covers a full F1-F4 fault-matrix row.
fn fault_io_error_on_takeover_fence_and_barrier_writes_fails_closed() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestDatabase::new("ioerr-takeover");
    let authority = open_shim(&database.path);
    let prefix = seed_prefix(&authority);
    let control_before = authority
        .inspect_task(task_id())
        .expect("head")
        .control_epoch;

    // Phase A: the takeover fence transaction dies on its first write.
    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::IoErr,
    });
    let error = authority
        .prepare_authority_takeover_fence(AuthorityLeaseTakeoverFenceRequest {
            task_id: prefix.spec.task_id,
            expected_registry_binding: prefix.registry_binding,
            lease: prefix.lease_two,
            requested_at_ms: 210,
        })
        .expect_err("takeover fence must fail under injected I/O error");
    assert_sqlite_error_chain(&error, &["i/o", "ioerr"]);
    assert!(nlos_store_fault::writes_observed() > 0);
    assert_no_phantom_takeover_rows(&database.path);
    assert_eq!(
        authority
            .inspect_participant_registry(task_id())
            .expect("registry")
            .state,
        ParticipantRegistryState::FrozenForPermit,
        "failed fence must not freeze the registry"
    );
    assert_eq!(
        assignment(&authority).state,
        AuthorityAssignmentState::Active
    );
    assert_eq!(
        authority
            .inspect_task(task_id())
            .expect("head")
            .control_epoch,
        control_before,
        "failed fence must not advance the control epoch"
    );

    nlos_store_fault::disarm();
    let frozen = fence_takeover(&authority, &prefix, 210);
    assert_eq!(frozen.state, ParticipantRegistryState::FrozenForTakeover);
    let fence = authority
        .inspect_authority_takeover_fence_receipt(task_id(), prefix.registry_binding)
        .expect("fence receipt");
    let takeover = authority
        .inspect_authority_takeover_receipt(task_id(), fence.receipt_id)
        .expect("takeover receipt");

    // Phase B: the barrier observation transaction dies on its first write.
    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::IoErr,
    });
    let members = fence_members(&authority, prefix.registry_binding);
    let participant = members.first().expect("fence member").participant;
    let error = authority
        .record_authority_takeover_barrier_receipt(AuthorityTakeoverBarrierReceiptRequest {
            takeover_receipt_id: takeover.receipt_id,
            participant,
            remote_receipt_id: ReceiptId::from_bytes([0x91; 16]),
            barrier_digest: [0x92; 32],
            observed_at_ms: 213,
        })
        .expect_err("barrier write must fail under injected I/O error");
    assert_sqlite_error_chain(&error, &["i/o", "ioerr"]);
    assert_eq!(
        authority
            .inspect_authority_takeover_barrier_receipts(takeover.receipt_id)
            .expect("observations")
            .len(),
        0,
        "failed barrier must leave no observation row"
    );
    assert_eq!(
        raw_count(&database.path, "task_authority_takeover_barrier_receipts"),
        0
    );
    let coverage = authority
        .inspect_authority_takeover_barrier_coverage(takeover.receipt_id)
        .expect("coverage");
    assert_eq!(coverage.observed_member_count, 0);
    assert_eq!(
        coverage.state,
        AuthorityTakeoverBarrierCoverageState::Partial
    );

    nlos_store_fault::disarm();
    let recorded = barrier_observation(&authority, &prefix, &takeover, 213, 0x91, 0x92);
    assert_eq!(recorded.barrier_digest, Some([0x92; 32]));
    assert_eq!(
        authority
            .inspect_authority_takeover_barrier_receipts(takeover.receipt_id)
            .expect("observations after disarm")
            .len(),
        1
    );
    assert_eq!(
        authority
            .inspect_authority_takeover_barrier_coverage(takeover.receipt_id)
            .expect("coverage after disarm")
            .state,
        AuthorityTakeoverBarrierCoverageState::LocallyCovered
    );
    assert_integrity(&database.path);
}

// ---------------------------------------------------------------------------
// 矩阵行 4: disk-full (ENOSPC) on takeover writes fails closed
// ---------------------------------------------------------------------------

/// disk-full（takeover fence 与 barrier 事务）：`FailWritesAfter { 0, Full }`
/// 下（a）takeover fence 写事务与（b）barrier observation 写事务都必须以
/// `SQLITE_FULL` 显式失败（错误链含 full）；无半截状态（无
/// fence/member/takeover/barrier 行、registry 不冻结、assignment 保持
/// `Active`、`control_epoch` 不动）；disarm 后同一操作成功。
#[test]
#[allow(clippy::too_many_lines)] // One test covers a full F1-F4 fault-matrix row.
fn fault_enospc_on_takeover_fence_and_barrier_writes_fails_closed() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestDatabase::new("full-takeover");
    let authority = open_shim(&database.path);
    let prefix = seed_prefix(&authority);
    let control_before = authority
        .inspect_task(task_id())
        .expect("head")
        .control_epoch;

    // Phase A: the takeover fence transaction dies on disk-full.
    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::Full,
    });
    let error = authority
        .prepare_authority_takeover_fence(AuthorityLeaseTakeoverFenceRequest {
            task_id: prefix.spec.task_id,
            expected_registry_binding: prefix.registry_binding,
            lease: prefix.lease_two,
            requested_at_ms: 210,
        })
        .expect_err("takeover fence must fail under injected disk-full");
    assert_sqlite_error_chain(&error, &["full"]);
    assert!(nlos_store_fault::writes_observed() > 0);
    assert_no_phantom_takeover_rows(&database.path);
    assert_eq!(
        authority
            .inspect_participant_registry(task_id())
            .expect("registry")
            .state,
        ParticipantRegistryState::FrozenForPermit
    );
    assert_eq!(
        assignment(&authority).state,
        AuthorityAssignmentState::Active
    );
    assert_eq!(
        authority
            .inspect_task(task_id())
            .expect("head")
            .control_epoch,
        control_before
    );

    nlos_store_fault::disarm();
    fence_takeover(&authority, &prefix, 210);
    let fence = authority
        .inspect_authority_takeover_fence_receipt(task_id(), prefix.registry_binding)
        .expect("fence receipt");
    let takeover = authority
        .inspect_authority_takeover_receipt(task_id(), fence.receipt_id)
        .expect("takeover receipt");

    // Phase B: the barrier observation transaction dies on disk-full.
    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::Full,
    });
    let members = fence_members(&authority, prefix.registry_binding);
    let participant = members.first().expect("fence member").participant;
    let error = authority
        .record_authority_takeover_barrier_receipt(AuthorityTakeoverBarrierReceiptRequest {
            takeover_receipt_id: takeover.receipt_id,
            participant,
            remote_receipt_id: ReceiptId::from_bytes([0x91; 16]),
            barrier_digest: [0x92; 32],
            observed_at_ms: 213,
        })
        .expect_err("barrier write must fail under injected disk-full");
    assert_sqlite_error_chain(&error, &["full"]);
    assert_eq!(
        raw_count(&database.path, "task_authority_takeover_barrier_receipts"),
        0
    );

    nlos_store_fault::disarm();
    let recorded = barrier_observation(&authority, &prefix, &takeover, 213, 0x91, 0x92);
    assert_eq!(recorded.barrier_digest, Some([0x92; 32]));
    assert_eq!(
        authority
            .inspect_authority_takeover_barrier_coverage(takeover.receipt_id)
            .expect("coverage after disarm")
            .state,
        AuthorityTakeoverBarrierCoverageState::LocallyCovered
    );
    assert_integrity(&database.path);
}

// ---------------------------------------------------------------------------
// 矩阵行 5: silent write loss / torn tail fabricates no phantom takeover facts
// ---------------------------------------------------------------------------

/// 静默丢写/短写（takeover 表组）：
/// - Phase A（断电模型）：`PowerLossAfter { 0 }` 下 barrier observation
///   “报告成功”但写入从未落盘；重开后幻影 observation 不得冒充已提交事实
///   （无 barrier 行、coverage `Partial`/0 observed），同一请求可重做且
///   确定性派生的 barrier receipt id 逐位相同、重开后真实持久。
/// - Phase B（撕裂尾部，隐藏整个 fence 事务）：子进程提交到 takeover fence
///   后被杀，父进程把 WAL 截断到最后一个 commit 帧（fence 事务）的一半；
///   重开后 registry 回到 `FrozenForPermit`、fence 六表无行、`control_epoch`
///   不前进，同一 fence 重做且确定性派生 receipt id 一致、`control_epoch`
///   恰好 +1。
/// - Phase C（撕裂尾部，隐藏 barrier 事务）：子进程提交 fence + barrier 后
///   被杀，WAL 截断在 barrier commit 帧一半；重开后 fence 前缀完整、
///   barrier 整体隐藏，同一 barrier 重做且 receipt id 逐位一致。
#[test]
fn fault_silent_write_loss_and_torn_tail_hide_phantom_takeover_facts() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    power_loss_drops_barrier_and_redo_is_durable();
    torn_wal_tail_hides_fence_and_redo_is_durable();
    torn_wal_tail_hides_barrier_and_redo_is_durable();
}

/// Phase A: a silently dropped barrier observation is invisible after
/// recovery and the lost observation is redoable with the same deterministic
/// identity.
#[allow(clippy::too_many_lines)] // One test covers a full F1-F4 fault-matrix row.
fn power_loss_drops_barrier_and_redo_is_durable() {
    let database = TestDatabase::new("power-loss-barrier");
    let authority = open_shim(&database.path);
    let prefix = seed_prefix(&authority);
    fence_takeover(&authority, &prefix, 210);
    let fence = authority
        .inspect_authority_takeover_fence_receipt(task_id(), prefix.registry_binding)
        .expect("fence receipt");
    let takeover = authority
        .inspect_authority_takeover_receipt(task_id(), fence.receipt_id)
        .expect("takeover receipt");

    nlos_store_fault::arm(FaultMode::PowerLossAfter { remaining: 0 });
    let phantom = barrier_observation(&authority, &prefix, &takeover, 213, 0x91, 0x92);
    nlos_store_fault::disarm();

    // The surviving connection keeps a wal-index that references frames the
    // disk never saw; it must die first (as a real power loss would kill
    // it) so recovery sees durable bytes alone (fault_crash.rs precedent).
    drop(authority);

    let recovered = SqliteTaskAuthority::open(&database.path).expect("reopen after power loss");
    assert!(
        recovered
            .inspect_authority_takeover_barrier_receipts(takeover.receipt_id)
            .expect("observations")
            .is_empty(),
        "silently dropped barrier must not fabricate an observation"
    );
    let coverage = recovered
        .inspect_authority_takeover_barrier_coverage(takeover.receipt_id)
        .expect("coverage");
    assert_eq!(coverage.observed_member_count, 0);
    assert_eq!(
        coverage.state,
        AuthorityTakeoverBarrierCoverageState::Partial
    );
    assert_eq!(
        raw_count(&database.path, "task_authority_takeover_barrier_receipts"),
        0
    );
    assert_integrity(&database.path);

    // The lost observation is redoable: the deterministic barrier receipt id
    // is reused, and this time genuinely durable.
    let redone = barrier_observation(&recovered, &prefix, &takeover, 213, 0x91, 0x92);
    assert_eq!(redone.receipt_id, phantom.receipt_id);
    drop(recovered);
    let verified = SqliteTaskAuthority::open(&database.path).expect("reopen after redo");
    assert_eq!(
        verified
            .inspect_authority_takeover_barrier_receipts(takeover.receipt_id)
            .expect("observations after redo")
            .len(),
        1
    );
    assert_eq!(
        verified
            .inspect_authority_takeover_barrier_coverage(takeover.receipt_id)
            .expect("coverage after redo")
            .state,
        AuthorityTakeoverBarrierCoverageState::LocallyCovered
    );
    assert_integrity(&database.path);
    drop(verified);
    drop(database);
}

/// Phase B: truncating the WAL to half of the takeover fence transaction's
/// commit frame hides the entire fence (registry freeze, fence receipt,
/// members, assignment transition, pending takeover receipt) while keeping
/// the committed permit prefix; the redo derives the same deterministic
/// fence receipt and advances the control epoch exactly once.
#[allow(clippy::too_many_lines)] // One test covers a full F1-F4 fault-matrix row.
fn torn_wal_tail_hides_fence_and_redo_is_durable() {
    let database = TestDatabase::new("torn-tail-fence");
    let mut child = spawn_child("torn-wal-fence", &database.path);
    await_marker(&mut child);
    kill_and_reap(&mut child);

    truncate_wal_inside_last_commit(&database.path);

    let recovered = SqliteTaskAuthority::open(&database.path).expect("reopen after truncation");
    let spec = attempt_spec();
    let permit = replayed_permit(
        recovered
            .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
                permit: permit_request(&spec, 0xb1, 150),
                lease: lease_record(
                    recovered
                        .acquire_authority_lease(lease_request(1, 0xa1, 100, 100))
                        .expect("replay lease one"),
                ),
            })
            .expect("replay permit"),
    );
    let registry_binding = permit
        .participant_registry_binding
        .expect("permit registry binding");
    let control_before = recovered
        .inspect_task(task_id())
        .expect("head")
        .control_epoch;
    assert_eq!(
        recovered
            .inspect_participant_registry(task_id())
            .expect("registry")
            .state,
        ParticipantRegistryState::FrozenForPermit,
        "torn fence tail must not freeze the registry"
    );
    assert_eq!(
        assignment(&recovered).state,
        AuthorityAssignmentState::Active
    );
    assert!(matches!(
        recovered.inspect_authority_takeover_fence_receipt(task_id(), registry_binding),
        Err(TaskStoreError::ReceiptNotFound)
    ));
    assert_no_phantom_takeover_rows(&database.path);

    // The hidden fence is redoable: the deterministic fence receipt id is
    // identical, and the control epoch advances exactly once.
    let lease_two = lease_record(
        recovered
            .acquire_authority_lease(lease_request(2, 0xa2, 201, 100))
            .expect("expired lease takeover"),
    );
    recovered
        .prepare_authority_takeover_fence(AuthorityLeaseTakeoverFenceRequest {
            task_id: task_id(),
            expected_registry_binding: registry_binding,
            lease: lease_two,
            requested_at_ms: 210,
        })
        .expect("redo fence after torn tail");
    assert_eq!(
        recovered
            .inspect_task(task_id())
            .expect("head")
            .control_epoch,
        control_before + 1
    );
    let fence = recovered
        .inspect_authority_takeover_fence_receipt(task_id(), registry_binding)
        .expect("redone fence receipt");
    assert!(fence.exact_fence_set_root.is_some());
    drop(recovered);
    let verified = SqliteTaskAuthority::open(&database.path).expect("reopen after redo");
    assert_eq!(
        verified
            .inspect_authority_takeover_fence_receipt(task_id(), registry_binding)
            .expect("fence receipt after reopen")
            .receipt_id,
        fence.receipt_id
    );
    assert_eq!(
        verified
            .inspect_task(task_id())
            .expect("head")
            .control_epoch,
        control_before + 1,
        "redo must advance the control epoch exactly once"
    );
    assert_integrity(&database.path);
    drop(verified);
    drop(database);
}

/// Phase C: truncating the WAL to half of the barrier commit frame hides
/// only the observation; the committed fence prefix (fence receipt, exact
/// roots, members, pending takeover receipt) survives bit-for-bit.
#[allow(clippy::too_many_lines)] // One test covers a full F1-F4 fault-matrix row.
fn torn_wal_tail_hides_barrier_and_redo_is_durable() {
    let database = TestDatabase::new("torn-tail-barrier");
    let mut child = spawn_child("torn-wal-barrier", &database.path);
    await_marker(&mut child);
    kill_and_reap(&mut child);

    truncate_wal_inside_last_commit(&database.path);

    let recovered = SqliteTaskAuthority::open(&database.path).expect("reopen after truncation");
    let spec = attempt_spec();
    let permit = replayed_permit(
        recovered
            .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
                permit: permit_request(&spec, 0xb1, 150),
                lease: lease_record(
                    recovered
                        .acquire_authority_lease(lease_request(1, 0xa1, 100, 100))
                        .expect("replay lease one"),
                ),
            })
            .expect("replay permit"),
    );
    let registry_binding = permit
        .participant_registry_binding
        .expect("permit registry binding");
    assert_eq!(
        recovered
            .inspect_participant_registry(task_id())
            .expect("registry")
            .state,
        ParticipantRegistryState::FrozenForTakeover,
        "the committed fence prefix survives the torn barrier tail"
    );
    let fence = recovered
        .inspect_authority_takeover_fence_receipt(task_id(), registry_binding)
        .expect("fence receipt survives");
    assert!(fence.exact_fence_set_root.is_some());
    let takeover = recovered
        .inspect_authority_takeover_receipt(task_id(), fence.receipt_id)
        .expect("takeover receipt survives");
    assert!(
        recovered
            .inspect_authority_takeover_barrier_receipts(takeover.receipt_id)
            .expect("observations")
            .is_empty(),
        "torn barrier tail must not fabricate an observation"
    );
    assert_eq!(
        raw_count(&database.path, "task_authority_takeover_barrier_receipts"),
        0
    );
    assert_eq!(
        raw_count(&database.path, "task_authority_takeover_fence_receipts"),
        1
    );
    assert_integrity(&database.path);

    // The hidden observation is redoable with the same deterministic id and
    // then genuinely durable.
    let members = fence_members(&recovered, registry_binding);
    let participant = members.first().expect("fence member").participant;
    let redone = recovered
        .record_authority_takeover_barrier_receipt(AuthorityTakeoverBarrierReceiptRequest {
            takeover_receipt_id: takeover.receipt_id,
            participant,
            remote_receipt_id: ReceiptId::from_bytes([0x91; 16]),
            barrier_digest: [0x92; 32],
            observed_at_ms: 213,
        })
        .expect("redo barrier after torn tail");
    drop(recovered);
    let verified = SqliteTaskAuthority::open(&database.path).expect("reopen after redo");
    let durable = verified
        .inspect_authority_takeover_barrier_receipts(takeover.receipt_id)
        .expect("observations after reopen");
    assert_eq!(durable.len(), 1);
    assert_eq!(durable[0].receipt_id, redone.receipt_id);
    assert_eq!(durable[0].barrier_digest, Some([0x92; 32]));
    assert_integrity(&database.path);
    drop(verified);
    drop(database);
}

/// Truncates the WAL file to half of its last commit frame: the last
/// transaction is a torn commit that recovery must discard entirely.
fn truncate_wal_inside_last_commit(path: &Path) {
    let wal_path = TestDatabase::sibling(path, "-wal");
    let mut wal = fs::read(&wal_path).expect("read wal");
    let (frame_size, commits) = wal_commit_frames(&wal);
    let last_commit = *commits.last().expect("commits exist");
    let half_frame_cut = 32 + last_commit * frame_size + frame_size / 2;
    wal.truncate(half_frame_cut);
    fs::write(&wal_path, &wal).expect("write truncated wal");
    fs::remove_file(TestDatabase::sibling(path, "-shm")).expect("remove stale shm");
}

// ---------------------------------------------------------------------------
// 矩阵行 6: after the fault clears, the takeover chain continues from the
// committed prefix
// ---------------------------------------------------------------------------

/// 故障解除后：takeover fence 写事务在 `FailWritesAfter { 0, Full }` 下失败
/// 后 disarm，**同一 authority 实例**继续读写——已提交前缀（registry
/// `FrozenForPermit`、assignment `Active`、`control_epoch`）与故障前逐位
/// 一致；fence 重试成功（registry `FrozenForTakeover`、`control_epoch` +1、
/// exact roots + member manifest + pending takeover receipt），barrier
/// observation 成功、coverage `LocallyCovered`；完整重开后全部状态可恢复。
#[test]
#[allow(clippy::too_many_lines)] // One test covers a full F1-F4 fault-matrix row.
fn fault_after_disarm_takeover_chain_continues_from_committed_prefix() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestDatabase::new("disarm-takeover-continue");
    let authority = open_shim(&database.path);
    let prefix = seed_prefix(&authority);
    let control_before = authority
        .inspect_task(task_id())
        .expect("head")
        .control_epoch;

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::Full,
    });
    authority
        .prepare_authority_takeover_fence(AuthorityLeaseTakeoverFenceRequest {
            task_id: prefix.spec.task_id,
            expected_registry_binding: prefix.registry_binding,
            lease: prefix.lease_two,
            requested_at_ms: 210,
        })
        .expect_err("takeover fence must fail while the fault is armed");

    // The committed prefix observed through the same authority is identical
    // to the pre-fault state.
    assert_no_phantom_takeover_rows(&database.path);
    assert_eq!(
        authority
            .inspect_participant_registry(task_id())
            .expect("registry")
            .state,
        ParticipantRegistryState::FrozenForPermit
    );
    assert_eq!(
        assignment(&authority).state,
        AuthorityAssignmentState::Active
    );
    assert_eq!(
        authority
            .inspect_task(task_id())
            .expect("head")
            .control_epoch,
        control_before
    );

    nlos_store_fault::disarm();

    // The retry succeeds on the same authority instance and the full chain
    // completes.
    fence_takeover(&authority, &prefix, 210);
    assert_eq!(
        authority
            .inspect_task(task_id())
            .expect("head")
            .control_epoch,
        control_before + 1
    );
    let fence = authority
        .inspect_authority_takeover_fence_receipt(task_id(), prefix.registry_binding)
        .expect("fence receipt");
    let takeover = authority
        .inspect_authority_takeover_receipt(task_id(), fence.receipt_id)
        .expect("takeover receipt");
    barrier_observation(&authority, &prefix, &takeover, 213, 0x91, 0x92);
    assert_eq!(
        authority
            .inspect_authority_takeover_barrier_coverage(takeover.receipt_id)
            .expect("coverage")
            .state,
        AuthorityTakeoverBarrierCoverageState::LocallyCovered
    );
    drop(authority);

    // A full reopen confirms the post-recovery takeover writes are durable.
    let reopened = SqliteTaskAuthority::open(&database.path).expect("reopen after recovery");
    assert_eq!(
        reopened
            .inspect_participant_registry(task_id())
            .expect("registry")
            .state,
        ParticipantRegistryState::FrozenForTakeover
    );
    assert_eq!(
        reopened
            .inspect_task(task_id())
            .expect("head")
            .control_epoch,
        control_before + 1
    );
    assert_eq!(
        reopened
            .inspect_authority_takeover_barrier_receipts(takeover.receipt_id)
            .expect("observations")
            .len(),
        1
    );
    assert_eq!(
        raw_count(&database.path, "task_authority_takeover_fence_receipts"),
        1
    );
    assert_eq!(
        raw_count(&database.path, "task_authority_takeover_receipts"),
        1
    );
    assert_eq!(
        raw_count(&database.path, "task_authority_takeover_barrier_receipts"),
        1
    );
    assert_eq!(raw_count(&database.path, "task_authority_lease_history"), 2);
    assert_integrity(&database.path);
}
