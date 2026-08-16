//! B-TASK-008C2G schema-v28/v29 lease-binding write-path fault-injection
//! tests: the authority-lease binding columns on `commit_permits` (v28) and
//! `task_adoption_receipts` (v29) under the PoC-0003-aligned F1-F4 matrix,
//! closing the "v28/v29 lease-binding 列未逐列注入" gap recorded by
//! `b-task-008c2g-takeover-fault-matrix.md`.
//!
//! The three lease-bound write paths under test are the only mutations that
//! persist the binding columns:
//! - `request_commit_permit_with_authority_lease` (writes
//!   `commit_permits.authority_lease_*` and the v31 Active assignment
//!   baseline in one transaction);
//! - `finalize_commit_v3_with_authority_lease` (terminal mutation of a
//!   lease-bound permit: receipt + permit close + head advance);
//! - `adopt_permit_with_authority_lease` (writes
//!   `task_adoption_receipts.authority_lease_*` in one transaction).
//!
//! The harness reuses the `nlos-store-fault` VFS patterns established by
//! `takeover_fault_injection.rs` / `reconcile_fault_injection.rs`: kill-9
//! child processes synchronized through piped `READY` markers (never
//! sleeps), `FAULT_LOCK` process-wide serialization, `wal_commit_frames`
//! tail truncation, typed error-chain assertions, raw table-level counts,
//! and a `PRAGMA integrity_check` re-verification at the end of every
//! scenario.
//!
//! **Crash semantics disclaimer**: the kill-9 rows use forced child
//! termination to simulate *process* crashes; the OS page cache survives a
//! process death, so a killed process is NOT a machine power loss. Writes
//! the kernel accepted but the disk never saw are covered by
//! [`FaultMode::PowerLossAfter`] and by file-level WAL tail truncation.

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
    AdoptionReplay, AdoptionRequest, AttemptRegistrationDecision, AttemptSpec,
    AuthorityLeaseAdoptionRequest, AuthorityLeaseDecision, AuthorityLeaseFinalizeRequest,
    AuthorityLeasePermitRequest, AuthorityLeaseRecord, DispatchRequest, EffectPermitDecision,
    EffectPermitRequest, FinalizeDecision, FinalizeRequest, FinalizeRequestV3, IssuedPermit,
    LogicalEffectDescriptor, Outcome, OutcomeRequest, PermitDecision, PermitRecord, PermitRequest,
    PlannedEffect, SnapshotBundle, SqliteTaskAuthority, TaskRegistrationDecision, TaskSpec,
    TaskStoreError, empty_effect_history_root,
};
use nlos_types::{
    CancellationScopeId, CommitPermitId, Generation, IdempotencyKey, ProcessId, TaskAttemptId,
    TaskId, TaskSnapshotId,
};

const VFS_NAME: &str = "nlos-task-lease-binding-fault";

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
                "nlos-task-lease-binding-fault-{name}-{}-{sequence}.sqlite3",
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

fn assert_integrity(path: &Path) {
    let connection = rusqlite::Connection::open(path).expect("open for integrity check");
    let result: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .expect("run integrity_check");
    assert_eq!(result, "ok", "integrity_check must pass");
}

fn raw_count(path: &Path, table: &str) -> i64 {
    let connection = rusqlite::Connection::open(path).expect("open raw reader");
    connection
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .expect("count rows")
}

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

fn bytes(value: u8) -> [u8; 16] {
    [value; 16]
}

fn process(seed: u8) -> ProcessId {
    ProcessId::from_bytes(bytes(seed))
}

fn lease_request(holder: u8, key: u8, at_ms: i64, ttl_ms: i64) -> nlos_task::AuthorityLeaseRequest {
    nlos_task::AuthorityLeaseRequest {
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

fn permit_request(attempt: &AttemptSpec, key: u8, effects: Vec<PlannedEffect>) -> PermitRequest {
    PermitRequest {
        task_id: attempt.task_id,
        attempt_id: attempt.attempt_id,
        attempt_generation: attempt.attempt_generation,
        write_set_root: [key; 32],
        planned_effects: effects,
        idempotency_key: IdempotencyKey::from_bytes([key.wrapping_add(0x10); 16]),
        valid_until_ms: 10_000,
        requested_at_ms: 150,
    }
}

fn descriptor() -> LogicalEffectDescriptor {
    LogicalEffectDescriptor {
        task_id: task_id(),
        task_generation: Generation::INITIAL,
        intent_spec_id: [0x44; 32],
        stable_action_slot: 0,
        target_authority_object_id: [0x55; 32],
        effect_class: 7,
        idempotency_scope: 3,
    }
}

fn planned_required() -> PlannedEffect {
    PlannedEffect {
        descriptor: descriptor(),
        required: true,
        required_condition_digest: None,
        success_criteria_digest: [0x66; 32],
        action_proposal_digest: [0x77; 32],
    }
}

fn finalize_request(attempt: &AttemptSpec, permit_id: CommitPermitId) -> FinalizeRequestV3 {
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

fn adopted_record(decision: AdoptionReplay) -> nlos_task::AdoptionReceiptRecord {
    match decision {
        AdoptionReplay::Adopted(record) => *record,
        other @ AdoptionReplay::Replayed(_) => panic!("expected Adopted, got {other:?}"),
    }
}

/// The shared plain prefix: task/attempt + lease term 1 (holder 1, valid
/// until 200) + lease-bound permit with **no** declared effects issued at
/// 150. The permit carries the v28 binding and the Active assignment
/// baseline; the registry is `FrozenForPermit`.
struct PlainPrefix {
    spec: AttemptSpec,
    permit: PermitRecord,
    lease: AuthorityLeaseRecord,
}

fn seed_plain_prefix(authority: &SqliteTaskAuthority) -> PlainPrefix {
    let spec = register_task_attempt(authority);
    let lease = lease_record(
        authority
            .acquire_authority_lease(lease_request(1, 0xa1, 100, 100))
            .expect("initial lease"),
    );
    let permit = issued_permit(
        authority
            .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
                permit: permit_request(&spec, 0xb1, Vec::new()),
                lease,
            })
            .expect("lease-bound permit"),
    );
    assert_eq!(permit.authority_lease_binding, Some(lease.binding()));
    PlainPrefix {
        spec,
        permit,
        lease,
    }
}

/// Drives the lease-bound permit's slot 0 through issue -> dispatch ->
/// `EFFECT_UNKNOWN`, then finalize (no proofs) which durably quarantines the
/// permit: the adoption precondition.
fn seed_quarantined_prefix(authority: &SqliteTaskAuthority) -> PlainPrefix {
    let spec = register_task_attempt(authority);
    let lease = lease_record(
        authority
            .acquire_authority_lease(lease_request(1, 0xa1, 100, 100))
            .expect("initial lease"),
    );
    let permit = issued_permit(
        authority
            .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
                permit: permit_request(&spec, 0xb1, vec![planned_required()]),
                lease,
            })
            .expect("lease-bound permit with planned effect"),
    );
    let issued = issued_effect_permit(
        authority
            .request_effect_permit(EffectPermitRequest {
                task_id: spec.task_id,
                attempt_id: spec.attempt_id,
                attempt_generation: spec.attempt_generation,
                permit_id: permit.permit_id,
                permit_epoch: permit.permit_epoch,
                effect_seq: 0,
                idempotency_key: IdempotencyKey::from_bytes([0xe1; 16]),
                valid_until_ms: 10_000,
                requested_at_ms: 151,
            })
            .expect("issue effect permit"),
    );
    authority
        .consume_dispatch_token(DispatchRequest {
            task_id: spec.task_id,
            attempt_id: spec.attempt_id,
            attempt_generation: spec.attempt_generation,
            permit_id: permit.permit_id,
            permit_epoch: permit.permit_epoch,
            effect_permit_id: issued.effect_permit_id,
            dispatch_token: issued.one_shot_dispatch_token,
            dispatched_at_ms: 152,
        })
        .expect("consume dispatch token");
    authority
        .record_effect_outcome(OutcomeRequest {
            task_id: spec.task_id,
            attempt_id: spec.attempt_id,
            attempt_generation: spec.attempt_generation,
            permit_id: permit.permit_id,
            permit_epoch: permit.permit_epoch,
            effect_seq: 0,
            outcome: Outcome::Unknown {
                uncertainty_digest: [0x99; 32],
            },
            recorded_at_ms: 153,
        })
        .expect("register uncertainty");
    assert!(matches!(
        authority.finalize_commit_v3_with_authority_lease(AuthorityLeaseFinalizeRequest {
            finalize: finalize_request(&spec, permit.permit_id),
            lease,
        }),
        Err(TaskStoreError::Quarantined)
    ));
    let permit = authority
        .inspect_permit(task_id(), permit.permit_id)
        .expect("permit after quarantine");
    assert_eq!(permit.state, nlos_task::PermitState::Quarantined);
    assert_eq!(permit.authority_lease_binding, Some(lease.binding()));
    PlainPrefix {
        spec,
        permit,
        lease,
    }
}

fn issued_effect_permit(decision: EffectPermitDecision) -> IssuedPermit {
    match decision {
        EffectPermitDecision::Issued(record) => *record,
        other @ EffectPermitDecision::Replayed(_) => panic!("expected Issued, got {other:?}"),
    }
}

fn adopt_request(prefix: &PlainPrefix, key: u8) -> AdoptionRequest {
    AdoptionRequest {
        task_id: prefix.spec.task_id,
        permit_id: prefix.permit.permit_id,
        permit_epoch: prefix.permit.permit_epoch,
        idempotency_key: IdempotencyKey::from_bytes([key; 16]),
        adopted_at_ms: 170,
    }
}

fn adopt_with_lease(
    authority: &SqliteTaskAuthority,
    prefix: &PlainPrefix,
) -> Result<AdoptionReplay, TaskStoreError> {
    authority.adopt_permit_with_authority_lease(AuthorityLeaseAdoptionRequest {
        adoption: adopt_request(prefix, 0xd1),
        lease: prefix.lease,
    })
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
// kill-9 child-process harness
// ---------------------------------------------------------------------------

fn spawn_child(scenario: &str, path: &Path) -> Child {
    Command::new(std::env::current_exe().expect("current test executable"))
        .args(["--exact", "crash_child_helper", "--nocapture"])
        .env("NLOS_LEASE_BINDING_CRASH_CHILD_SCENARIO", scenario)
        .env("NLOS_LEASE_BINDING_CRASH_CHILD_DATABASE", path)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn crash child")
}

fn await_marker(child: &mut Child) {
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
        Ok(Ok(_)) => {}
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
// kill-9 child scenarios
// ---------------------------------------------------------------------------

/// Matrix row 3 fixture: the quarantined lease-bound permit prefix is
/// committed; a writer transaction then dirties the v29 binding write path
/// (phantom adoption receipt **with** the authority-lease binding columns,
/// phantom effect-sequence row) plus the `commit_permits.permit_state` CAS
/// dirt, and dies before commit.
fn child_mid_binding_tx(path: &Path) -> ! {
    let authority = SqliteTaskAuthority::open(path).expect("open");
    let prefix = seed_quarantined_prefix(&authority);
    let binding = prefix
        .permit
        .authority_lease_binding
        .expect("permit lease binding");
    let raw = rusqlite::Connection::open(path).expect("raw connection");
    raw.execute_batch("BEGIN IMMEDIATE").expect("begin mid-tx");
    raw.execute(
        "UPDATE commit_permits SET permit_state = permit_state + 100",
        [],
    )
    .expect("mid-tx permit CAS dirt");
    let task_hex = "01010101010101010101010101010101";
    let generation_hex = "0000000000000001";
    raw.execute_batch(&format!(
        "INSERT INTO task_adoption_receipts (
            receipt_id, task_id, task_generation, idempotency_key,
            original_permit_id, original_permit_epoch, original_control_epoch,
            original_cancel_epoch, effect_set_root,
            observed_effect_slot_state_root, adoption_epoch, created_at_ms,
            authority_lease_authority_id, authority_lease_holder_id,
            authority_lease_term, authority_lease_epoch,
            authority_lease_fencing_token, authority_lease_expires_at_ms
         ) VALUES (
            X'99999999999999999999999999999999', X'{task_hex}', X'{generation_hex}',
            X'eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee',
            X'{permit}', X'0000000000000001', X'0000000000000000',
            X'0000000000000000',
            X'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa',
            X'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb',
            X'0000000000000001', 170,
            X'{authority}', X'{holder}',
            X'{term:016x}', X'{epoch:016x}',
            X'{token}', 200
         )",
        permit = hex_encode(prefix.permit.permit_id.as_bytes()),
        authority = hex_encode(binding.authority_id.as_bytes()),
        holder = hex_encode(binding.holder_id.as_bytes()),
        term = binding.term,
        epoch = binding.lease_epoch,
        token = hex_encode(&binding.fencing_token),
    ))
    .expect("mid-tx phantom v29 adoption receipt");
    raw.execute_batch(
        "INSERT INTO task_effect_sequences (task_id, effect_history_seq, adoption_epoch)
         VALUES (X'01010101010101010101010101010101', X'0000000000000000', X'0000000000000001')",
    )
    .expect("mid-tx phantom sequence");
    announce("READY");
    let _keepers = (authority, raw);
    loop {
        std::thread::park();
    }
}

/// Matrix row 4 fixture: the complete committed lease-bound adoption
/// (quarantine tombstone -> adoption with v29 binding) before the kill.
fn child_binding_commit_complete(path: &Path) -> ! {
    let authority = SqliteTaskAuthority::open(path).expect("open");
    let prefix = seed_quarantined_prefix(&authority);
    adopted_record(adopt_with_lease(&authority, &prefix).expect("lease-bound adoption"));
    announce("READY");
    let _keeper = authority;
    loop {
        std::thread::park();
    }
}

/// Torn-tail fixture: the lease-bound plain finalize is the last committed
/// transaction (permit issuance is already durable).
fn child_torn_wal_finalize(path: &Path) -> ! {
    let authority = SqliteTaskAuthority::open(path).expect("open");
    let prefix = seed_plain_prefix(&authority);
    match authority
        .finalize_commit_v3_with_authority_lease(AuthorityLeaseFinalizeRequest {
            finalize: finalize_request(&prefix.spec, prefix.permit.permit_id),
            lease: prefix.lease,
        })
        .expect("lease-bound finalize")
    {
        FinalizeDecision::Committed(_) => {}
        other @ FinalizeDecision::Replayed(_) => panic!("expected Committed, got {other:?}"),
    }
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
        std::env::var("NLOS_LEASE_BINDING_CRASH_CHILD_SCENARIO"),
        std::env::var("NLOS_LEASE_BINDING_CRASH_CHILD_DATABASE"),
    ) else {
        return;
    };
    let path = PathBuf::from(path);
    match scenario.as_str() {
        "mid-binding-tx" => child_mid_binding_tx(&path),
        "binding-commit-complete" => child_binding_commit_complete(&path),
        "torn-wal-finalize" => child_torn_wal_finalize(&path),
        other => panic!("unknown crash child scenario {other}"),
    }
}

// ---------------------------------------------------------------------------
// 矩阵行 1: hard I/O error on lease-bound issue / finalize fails closed
// ---------------------------------------------------------------------------

/// 写入硬 I/O 错误（v28 lease-bound permit 签发与 lease-bound finalize）：
/// `FailWritesAfter { 0, IoErr }` 下（a）`request_commit_permit_with_authority_lease`
/// 事务（permit 行 + v28 binding 列 + v31 Active assignment baseline +
/// registry freeze 同事务）与（b）`finalize_commit_v3_with_authority_lease`
/// 事务（receipt + permit 关闭 + head 推进同事务）都必须以
/// `TaskStoreError::Sqlite` 显式失败（错误链含 I/O 条件），不返回假成功；
/// 无半截状态（无 permit/assignment/receipt 行、registry 保持 `Open`、
/// permit 保持 `ISSUED`、head 不动）；disarm 后同一操作成功且 binding 持久。
#[test]
#[allow(clippy::too_many_lines)] // One test covers a full F1-F4 fault-matrix row.
fn fault_io_error_on_lease_bound_issue_and_finalize_fails_closed() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestDatabase::new("ioerr-binding");
    let authority = open_shim(&database.path);
    let spec = register_task_attempt(&authority);
    let lease = lease_record(
        authority
            .acquire_authority_lease(lease_request(1, 0xa1, 100, 100))
            .expect("initial lease"),
    );

    // Phase A: lease-bound permit issuance dies on its first write.
    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::IoErr,
    });
    let error = authority
        .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
            permit: permit_request(&spec, 0xb1, Vec::new()),
            lease,
        })
        .expect_err("lease-bound permit must fail under injected I/O error");
    assert_sqlite_error_chain(&error, &["i/o", "ioerr"]);
    assert!(nlos_store_fault::writes_observed() > 0);
    assert_eq!(raw_count(&database.path, "commit_permits"), 0);
    assert_eq!(raw_count(&database.path, "task_authority_assignments"), 0);
    assert_eq!(
        authority
            .inspect_participant_registry(task_id())
            .expect("registry")
            .state,
        nlos_task::ParticipantRegistryState::Open,
        "failed issuance must not freeze the registry"
    );

    nlos_store_fault::disarm();
    let permit = issued_permit(
        authority
            .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
                permit: permit_request(&spec, 0xb1, Vec::new()),
                lease,
            })
            .expect("permit issuance succeeds after disarm"),
    );
    assert_eq!(permit.authority_lease_binding, Some(lease.binding()));
    let head_before = authority
        .inspect_task(task_id())
        .expect("head")
        .head_commit_seq;

    // Phase B: lease-bound finalize dies on its first write.
    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::IoErr,
    });
    let error = authority
        .finalize_commit_v3_with_authority_lease(AuthorityLeaseFinalizeRequest {
            finalize: finalize_request(&spec, permit.permit_id),
            lease,
        })
        .expect_err("lease-bound finalize must fail under injected I/O error");
    assert_sqlite_error_chain(&error, &["i/o", "ioerr"]);
    assert_eq!(raw_count(&database.path, "task_receipts"), 0);
    assert_eq!(
        authority
            .inspect_permit(task_id(), permit.permit_id)
            .expect("permit")
            .state,
        nlos_task::PermitState::Issued
    );
    assert_eq!(
        authority
            .inspect_task(task_id())
            .expect("head")
            .head_commit_seq,
        head_before,
        "failed finalize must not advance the head"
    );

    nlos_store_fault::disarm();
    match authority
        .finalize_commit_v3_with_authority_lease(AuthorityLeaseFinalizeRequest {
            finalize: finalize_request(&spec, permit.permit_id),
            lease,
        })
        .expect("finalize succeeds after disarm")
    {
        FinalizeDecision::Committed(receipt) => {
            assert_eq!(receipt.new_head_commit_seq, head_before + 1);
        }
        other @ FinalizeDecision::Replayed(_) => panic!("expected Committed, got {other:?}"),
    }
    assert_eq!(raw_count(&database.path, "task_receipts"), 1);
    assert_integrity(&database.path);
}

// ---------------------------------------------------------------------------
// 矩阵行 2: disk-full (ENOSPC) on lease-bound adoption fails closed
// ---------------------------------------------------------------------------

/// disk-full（v29 lease-bound adoption）：`FailWritesAfter { 0, Full }` 下
/// `adopt_permit_with_authority_lease` 事务（adoption receipt 含 v29
/// binding 列 + sequence epoch 推进同事务）必须以 `SQLITE_FULL` 显式失败
/// （错误链含 full）；无半截状态（无 adoption/sequence 行、permit 保持
/// `QUARANTINED`）；disarm 后同一 adoption 成功且 binding 持久、重启后仍可
/// 回读。
#[test]
#[allow(clippy::too_many_lines)] // One test covers a full F1-F4 fault-matrix row.
fn fault_enospc_on_lease_bound_adoption_fails_closed() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestDatabase::new("full-binding");
    let authority = open_shim(&database.path);
    let prefix = seed_quarantined_prefix(&authority);

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::Full,
    });
    let error = authority
        .adopt_permit_with_authority_lease(AuthorityLeaseAdoptionRequest {
            adoption: adopt_request(&prefix, 0xd1),
            lease: prefix.lease,
        })
        .expect_err("lease-bound adoption must fail under injected disk-full");
    assert_sqlite_error_chain(&error, &["full"]);
    assert!(nlos_store_fault::writes_observed() > 0);
    assert_eq!(raw_count(&database.path, "task_adoption_receipts"), 0);
    assert_eq!(raw_count(&database.path, "task_effect_sequences"), 0);
    assert_eq!(
        authority
            .inspect_permit(task_id(), prefix.permit.permit_id)
            .expect("permit")
            .state,
        nlos_task::PermitState::Quarantined
    );

    nlos_store_fault::disarm();
    let adopted = adopted_record(
        adopt_with_lease(&authority, &prefix).expect("adoption succeeds after disarm"),
    );
    assert_eq!(
        adopted.authority_lease_binding,
        Some(prefix.lease.binding()),
        "the v29 adoption receipt persists the exact lease binding"
    );
    assert_eq!(raw_count(&database.path, "task_adoption_receipts"), 1);
    assert_eq!(raw_count(&database.path, "task_effect_sequences"), 1);
    assert_integrity(&database.path);

    // The v29 binding survives a full restart readback.
    drop(authority);
    let reopened = SqliteTaskAuthority::open(&database.path).expect("reopen");
    let readback = reopened
        .inspect_adoption_receipt(task_id(), adopted.receipt_id)
        .expect("adoption receipt after restart");
    assert_eq!(
        readback.authority_lease_binding,
        Some(prefix.lease.binding())
    );
    assert_integrity(&database.path);
}

// ---------------------------------------------------------------------------
// 矩阵行 3: kill-9 mid-transaction on the v29 binding write path
// ---------------------------------------------------------------------------

/// kill-9 中断 v29 binding 写事务：子进程在 `BEGIN IMMEDIATE` 未提交（已写入
/// 带 authority-lease binding 列的幻影 adoption receipt、幻影 sequence 行，
/// 并弄脏 `commit_permits.permit_state` CAS）时被强杀；重开后中断事务完全
/// 回滚——无幻影 adoption/sequence 行、permit 回到已提交 `QUARANTINED`；
/// 随后同一 lease-bound adoption 重做成功且确定性派生 receipt id 一致、
/// v29 binding 持久。
#[test]
#[allow(clippy::too_many_lines)] // One test covers a full F1-F4 fault-matrix row.
fn fault_kill9_mid_lease_binding_tx_leaves_no_half_state() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestDatabase::new("kill9-mid-binding-tx");
    let mut child = spawn_child("mid-binding-tx", &database.path);
    await_marker(&mut child);
    kill_and_reap(&mut child);

    assert_eq!(raw_count(&database.path, "task_adoption_receipts"), 0);
    assert_eq!(raw_count(&database.path, "task_effect_sequences"), 0);

    let authority = SqliteTaskAuthority::open(&database.path).expect("reopen after kill");
    let spec = attempt_spec();
    let permit = replayed_permit(
        authority
            .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
                permit: permit_request(&spec, 0xb1, vec![planned_required()]),
                lease: lease_record(
                    authority
                        .acquire_authority_lease(lease_request(1, 0xa1, 100, 100))
                        .expect("replay lease"),
                ),
            })
            .expect("replay permit"),
    );
    assert_eq!(
        permit.state,
        nlos_task::PermitState::Quarantined,
        "mid-transaction permit CAS dirt must be rolled back"
    );
    let lease = lease_record(
        authority
            .acquire_authority_lease(lease_request(1, 0xa1, 100, 100))
            .expect("replay lease record"),
    );
    let prefix = PlainPrefix {
        spec,
        permit,
        lease,
    };
    let adopted =
        adopted_record(adopt_with_lease(&authority, &prefix).expect("redo lease-bound adoption"));
    assert_eq!(
        adopted.authority_lease_binding,
        Some(prefix.lease.binding())
    );
    assert_eq!(raw_count(&database.path, "task_adoption_receipts"), 1);
    assert_integrity(&database.path);

    // Restart readback keeps the v29 binding and the deterministic receipt.
    drop(authority);
    let reopened = SqliteTaskAuthority::open(&database.path).expect("reopen after redo");
    assert_eq!(
        reopened
            .inspect_adoption_receipt(task_id(), adopted.receipt_id)
            .expect("adoption receipt")
            .authority_lease_binding,
        Some(prefix.lease.binding())
    );
    assert_integrity(&database.path);
}

// ---------------------------------------------------------------------------
// 矩阵行 4: kill-9 after commit — the committed lease-bound adoption survives
// ---------------------------------------------------------------------------

/// commit 后崩溃：子进程在 lease-bound adoption（quarantine tombstone +
/// v29 binding receipt）全部提交返回后被强杀；重开后 adoption receipt 含
/// binding 逐位保留、adoption 重放返回原记录（不重复推进）、v29 binding 列
/// 的 UPDATE 被 immutable trigger 拒绝、重启后 binding 仍可回读。
#[test]
#[allow(clippy::too_many_lines)] // One test covers a full F1-F4 fault-matrix row.
fn fault_kill9_after_lease_bound_adoption_commit_preserves_binding() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestDatabase::new("kill9-binding-commit");
    let mut child = spawn_child("binding-commit-complete", &database.path);
    await_marker(&mut child);
    kill_and_reap(&mut child);

    assert_eq!(raw_count(&database.path, "task_adoption_receipts"), 1);
    assert_eq!(raw_count(&database.path, "task_quarantine_receipts"), 1);

    let authority = SqliteTaskAuthority::open(&database.path).expect("reopen after kill");
    let spec = attempt_spec();
    let permit = replayed_permit(
        authority
            .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
                permit: permit_request(&spec, 0xb1, vec![planned_required()]),
                lease: lease_record(
                    authority
                        .acquire_authority_lease(lease_request(1, 0xa1, 100, 100))
                        .expect("replay lease"),
                ),
            })
            .expect("replay permit"),
    );
    assert_eq!(permit.state, nlos_task::PermitState::Quarantined);
    let lease = lease_record(
        authority
            .acquire_authority_lease(lease_request(1, 0xa1, 100, 100))
            .expect("replay lease record"),
    );
    let prefix = PlainPrefix {
        spec,
        permit,
        lease,
    };

    // The committed adoption receipt carries the v29 binding bit-for-bit;
    // the idempotent replay returns the original durable record.
    let adoption = match adopt_with_lease(&authority, &prefix).expect("adoption replay") {
        AdoptionReplay::Replayed(replayed) => *replayed,
        other @ AdoptionReplay::Adopted(_) => panic!("expected Replayed, got {other:?}"),
    };
    assert_eq!(
        adoption.authority_lease_binding,
        Some(prefix.lease.binding())
    );
    assert_eq!(raw_count(&database.path, "task_adoption_receipts"), 1);

    // The v29 binding columns are immutable.
    let raw = rusqlite::Connection::open(&database.path).expect("raw connection");
    assert!(
        raw.execute(
            "UPDATE task_adoption_receipts
             SET authority_lease_term = zeroblob(8)
             WHERE receipt_id = ?1",
            rusqlite::params![adoption.receipt_id.as_bytes().as_slice()],
        )
        .is_err()
    );
    drop(raw);
    assert_integrity(&database.path);

    // Restart readback keeps the binding.
    drop(authority);
    let reopened = SqliteTaskAuthority::open(&database.path).expect("reopen after kill");
    assert_eq!(
        reopened
            .inspect_adoption_receipt(task_id(), adoption.receipt_id)
            .expect("adoption receipt after restart")
            .authority_lease_binding,
        Some(prefix.lease.binding())
    );
    assert_integrity(&database.path);
}

// ---------------------------------------------------------------------------
// 矩阵行 5: silent write loss on lease-bound finalize
// ---------------------------------------------------------------------------

/// 静默丢写（v28 lease-bound finalize）：`PowerLossAfter { 0 }` 下 finalize
/// “报告成功”但写入从未落盘；重开后幻影 receipt 不得冒充已提交事实（permit
/// 回到 `ISSUED`、无 receipt 行、head 不动、v28 binding 保持），同一请求可
/// 重做且确定性派生的 receipt id 逐位相同、重开后真实持久。
#[test]
#[allow(clippy::too_many_lines)] // One test covers a full F1-F4 fault-matrix row.
fn fault_power_loss_drops_lease_bound_finalize_and_redo_is_durable() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestDatabase::new("power-loss-binding");
    let authority = open_shim(&database.path);
    let prefix = seed_plain_prefix(&authority);
    let head_before = authority
        .inspect_task(task_id())
        .expect("head")
        .head_commit_seq;

    nlos_store_fault::arm(FaultMode::PowerLossAfter { remaining: 0 });
    let phantom = match authority
        .finalize_commit_v3_with_authority_lease(AuthorityLeaseFinalizeRequest {
            finalize: finalize_request(&prefix.spec, prefix.permit.permit_id),
            lease: prefix.lease,
        })
        .expect("power loss drops writes silently")
    {
        FinalizeDecision::Committed(receipt) => receipt,
        other @ FinalizeDecision::Replayed(_) => panic!("expected Committed, got {other:?}"),
    };
    nlos_store_fault::disarm();
    drop(authority);

    let recovered = SqliteTaskAuthority::open(&database.path).expect("reopen after power loss");
    assert_eq!(
        recovered
            .inspect_permit(task_id(), prefix.permit.permit_id)
            .expect("permit")
            .state,
        nlos_task::PermitState::Issued,
        "silently dropped finalize must not close the permit"
    );
    assert_eq!(
        recovered
            .inspect_task(task_id())
            .expect("head")
            .head_commit_seq,
        head_before,
        "silently dropped finalize must not advance the head"
    );
    assert_eq!(raw_count(&database.path, "task_receipts"), 0);
    assert_integrity(&database.path);

    // The lost decision is redoable: the deterministic receipt id is reused,
    // and this time genuinely durable.
    let redone = match recovered
        .finalize_commit_v3_with_authority_lease(AuthorityLeaseFinalizeRequest {
            finalize: finalize_request(&prefix.spec, prefix.permit.permit_id),
            lease: prefix.lease,
        })
        .expect("redo finalize after power loss")
    {
        FinalizeDecision::Committed(receipt) => receipt,
        other @ FinalizeDecision::Replayed(_) => panic!("expected Committed, got {other:?}"),
    };
    assert_eq!(redone.receipt_id, phantom.receipt_id);
    drop(recovered);
    let verified = SqliteTaskAuthority::open(&database.path).expect("reopen after redo");
    assert_eq!(
        verified
            .inspect_task(task_id())
            .expect("head")
            .head_commit_seq,
        head_before + 1
    );
    assert_eq!(raw_count(&database.path, "task_receipts"), 1);
    assert_integrity(&database.path);
    drop(verified);
    drop(database);
}

// ---------------------------------------------------------------------------
// 矩阵行 6: torn WAL tail hides the lease-bound finalize commit
// ---------------------------------------------------------------------------

/// 撕裂尾部：子进程提交 permit 签发 + lease-bound finalize 后被强杀，父进程
/// 把 WAL 截断到最后一个 commit 帧（finalize 事务）的一半；重开后 finalize
/// 整体隐藏（permit 回到 `ISSUED`、无 receipt 行、head 不动、v28 binding
/// 保持），同一 finalize 重做且确定性派生 receipt id 一致。
#[test]
#[allow(clippy::too_many_lines)] // One test covers a full F1-F4 fault-matrix row.
fn fault_torn_wal_tail_hides_lease_bound_finalize_and_redo_is_durable() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestDatabase::new("torn-tail-binding");
    let mut child = spawn_child("torn-wal-finalize", &database.path);
    await_marker(&mut child);
    kill_and_reap(&mut child);

    truncate_wal_inside_last_commit(&database.path);

    let recovered = SqliteTaskAuthority::open(&database.path).expect("reopen after truncation");
    let spec = attempt_spec();
    let permit = replayed_permit(
        recovered
            .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
                permit: permit_request(&spec, 0xb1, Vec::new()),
                lease: lease_record(
                    recovered
                        .acquire_authority_lease(lease_request(1, 0xa1, 100, 100))
                        .expect("replay lease"),
                ),
            })
            .expect("replay permit"),
    );
    assert_eq!(
        permit.state,
        nlos_task::PermitState::Issued,
        "torn finalize tail must not close the permit"
    );
    assert_eq!(
        permit.authority_lease_binding,
        Some(permit.authority_lease_binding.expect("binding")),
        "the committed v28 binding survives the torn tail"
    );
    assert_eq!(
        recovered
            .inspect_task(task_id())
            .expect("head")
            .head_commit_seq,
        0
    );
    assert_eq!(raw_count(&database.path, "task_receipts"), 0);
    assert_integrity(&database.path);

    // The hidden finalize is redoable with the same deterministic receipt.
    let lease = lease_record(
        recovered
            .acquire_authority_lease(lease_request(1, 0xa1, 100, 100))
            .expect("replay lease record"),
    );
    let redone = match recovered
        .finalize_commit_v3_with_authority_lease(AuthorityLeaseFinalizeRequest {
            finalize: finalize_request(&spec, permit.permit_id),
            lease,
        })
        .expect("redo finalize after torn tail")
    {
        FinalizeDecision::Committed(receipt) => receipt,
        other @ FinalizeDecision::Replayed(_) => panic!("expected Committed, got {other:?}"),
    };
    drop(recovered);
    let verified = SqliteTaskAuthority::open(&database.path).expect("reopen after redo");
    let durable = verified
        .inspect_receipt(task_id(), redone.receipt_id)
        .expect("receipt after reopen");
    assert_eq!(durable.receipt_id, redone.receipt_id);
    assert_eq!(
        verified
            .inspect_task(task_id())
            .expect("head")
            .head_commit_seq,
        1
    );
    assert_integrity(&database.path);
    drop(verified);
    drop(database);
}
