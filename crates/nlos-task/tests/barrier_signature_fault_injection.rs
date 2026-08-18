//! PoC-0003-aligned F1-F6 fault-injection tests for the SIGNED takeover
//! barrier observation write path: `SqliteTaskAuthority::
//! record_authority_takeover_barrier_receipt_signed` (schema v36, five
//! durable signer columns) under the process-crash / hard-I/O / disk-full /
//! silent-write-loss matrix.
//!
//! The write path under test is one Immediate transaction on the TASK
//! database: [shared core validation: pending takeover + exact
//! `fence_set_root` + `FrozenForTakeover` binding + manifest membership] ->
//! [Ed25519 verification against the `nlos-identity` key authority, a READ
//! on a separate, always-healthy database] -> [insert incl. the five signer
//! columns] -> commit. The identity authority database is never faulted:
//! every fault targets the task database only, so every scenario keeps
//! proving that signature verification itself never fabricates durable
//! task-store state.
//!
//! The harness reuses the `nlos-store-fault` VFS patterns established by
//! `takeover_fault_injection.rs` and `lease_binding_fault_injection.rs`:
//! kill-9 child processes synchronized through piped `READY` markers (never
//! sleeps), `FAULT_LOCK` process-wide serialization, `wal_commit_frames`
//! tail truncation, typed error-chain assertions, raw table-level counts,
//! and a `PRAGMA integrity_check` re-verification at the end of every
//! scenario. Determinism of the barrier receipt id is additionally anchored
//! by an independent SHA-256 mirror of the
//! `llmos/task-authority-takeover-barrier/v1` derivation (the id domain is
//! unchanged from the unsigned path), so every redo row is compared against
//! what any clean run would derive from the same material.
//!
//! **Crash semantics disclaimer**: the kill-9 rows use forced child
//! termination to simulate *process* crashes; the OS page cache survives a
//! process death, so a killed process is NOT a machine power loss. Writes
//! the kernel accepted but the disk never saw are covered by
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

use ed25519_dalek::{Signer, SigningKey};
use nlos_identity::{BootstrapPrincipalRequest, IdentityAuthority, KeyPurpose};
use nlos_store_fault::{FaultCode, FaultMode};
use nlos_task::{
    AttemptRegistrationDecision, AttemptSpec, AuthorityAssignmentRecord, AuthorityAssignmentState,
    AuthorityLeaseDecision, AuthorityLeaseFinalizeRequest, AuthorityLeasePermitRequest,
    AuthorityLeaseRecord, AuthorityLeaseRequest, AuthorityLeaseTakeoverFenceRequest,
    AuthorityTakeoverBarrierCoverageState, AuthorityTakeoverBarrierReceiptRecord,
    AuthorityTakeoverBarrierReceiptRequest, AuthorityTakeoverBarrierSigner,
    AuthorityTakeoverReceiptRecord, BarrierObservationSignature, FinalizeDecision, FinalizeRequest,
    FinalizeRequestV3, ParticipantRecord, ParticipantRegistryBinding, ParticipantRegistryState,
    ParticipantType, PermitDecision, PermitRecord, PermitRequest, SnapshotBundle,
    SqliteTaskAuthority, TaskRegistrationDecision, TaskSpec, TaskStoreError,
    barrier_observation_signature_message, empty_effect_history_root,
};
use nlos_types::{
    CancellationScopeId, Generation, IdempotencyKey, ProcessId, ReceiptId, TaskAttemptId, TaskId,
    TaskSnapshotId,
};
use sha2::{Digest, Sha256};

const VFS_NAME: &str = "nlos-task-barrier-sig-fault";

/// Fixed signer material shared by parent and child processes: the Ed25519
/// key seed, the bootstrap request seeds, and the observation constants.
const SIGNER_SEED: u8 = 0x31;
const REMOTE_SEED: u8 = 0x91;
const DIGEST_SEED: u8 = 0x92;
const OBSERVED_AT_MS: i64 = 220;

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
                "nlos-task-barrier-sig-fault-{name}-{}-{sequence}.sqlite3",
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

/// A temp directory holding the (never faulted) identity authority store.
/// Parent and child processes open the same root; bootstrap is idempotent
/// and derives identical ids from the fixed request bytes.
struct IdentityRoot(PathBuf);

impl IdentityRoot {
    fn new(label: &str) -> Self {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("monotonic clock")
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "nlos-task-barrier-sig-fault-identity-{label}-{}-{nonce}-{}",
            std::process::id(),
            NEXT_DATABASE.fetch_add(1, Ordering::Relaxed)
        )))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for IdentityRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
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
        idempotency_key: IdempotencyKey::from_bytes([0x06; 16]),
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

/// The shared committed prefix every scenario starts from: register
/// task/attempt -> lease term 1 -> lease-bound permit issuance (Active
/// assignment baseline, registry binding captured) -> lease-bound finalize
/// -> expired lease taken over by holder 2 (term 2).
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

fn assignment(authority: &SqliteTaskAuthority) -> AuthorityAssignmentRecord {
    authority
        .inspect_authority_assignment(task_id())
        .expect("assignment")
}

// ---------------------------------------------------------------------------
// signed-path fixture (barrier_signature.rs fused with the fault harness)
// ---------------------------------------------------------------------------

/// The verified signing principal reconstructed identically in every
/// process: a fixed Ed25519 seed plus an idempotent, id-deriving bootstrap.
struct BarrierSigner {
    key: SigningKey,
    binding: nlos_identity::IdentityBinding,
}

fn bootstrap_barrier_signer(identity: &IdentityAuthority) -> BarrierSigner {
    let seed = SIGNER_SEED;
    let key = SigningKey::from_bytes(&[seed; 32]);
    let binding = identity
        .bootstrap_principal(BootstrapPrincipalRequest {
            principal_profile_digest: [seed.wrapping_add(1); 32],
            control_domain_policy_digest: [seed.wrapping_add(2); 32],
            public_key: key.verifying_key().to_bytes(),
            key_purpose: KeyPurpose::BarrierObservationSigning,
            key_valid_from_ms: 0,
            key_valid_until_ms: 10_000,
            idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(3); 16]),
            created_at_ms: 0,
        })
        .expect("bootstrap signer")
        .binding();
    BarrierSigner { key, binding }
}

/// The canonical signed-observation request for the first fence member of
/// the given pending takeover.
fn barrier_request(
    takeover: &AuthorityTakeoverReceiptRecord,
    participant: ParticipantRecord,
) -> AuthorityTakeoverBarrierReceiptRequest {
    AuthorityTakeoverBarrierReceiptRequest {
        takeover_receipt_id: takeover.receipt_id,
        participant,
        remote_receipt_id: ReceiptId::from_bytes([REMOTE_SEED; 16]),
        barrier_digest: [DIGEST_SEED; 32],
        observed_at_ms: OBSERVED_AT_MS,
    }
}

/// Signs the domain-separated observation message digest for the request
/// material with the bootstrapped key.
fn barrier_signature(
    signer: &BarrierSigner,
    message_digest: [u8; 32],
) -> BarrierObservationSignature {
    BarrierObservationSignature {
        issuer: signer.binding.principal_id,
        control_domain_id: signer.binding.control_domain_id,
        key_id: signer.binding.key_id,
        signature: signer.key.sign(&message_digest).to_bytes(),
    }
}

/// The signer columns a verified record must carry: every field comes from
/// the identity binding and the signature bytes, never caller assertions.
fn expected_signer(
    signer: &BarrierSigner,
    signature: &BarrierObservationSignature,
) -> AuthorityTakeoverBarrierSigner {
    AuthorityTakeoverBarrierSigner {
        principal_id: signer.binding.principal_id,
        control_domain_id: signer.binding.control_domain_id,
        key_id: signer.binding.key_id,
        key_generation: signer.binding.key_generation,
        signature: signature.signature,
    }
}

fn participant_type_code(participant_type: ParticipantType) -> u64 {
    match participant_type {
        ParticipantType::TaskStore => 1,
        ParticipantType::ArtifactHead => 2,
        ParticipantType::SemanticAdmission => 3,
        ParticipantType::ChannelTopic => 4,
        ParticipantType::DriverGateway => 5,
        ParticipantType::ResourceLedger => 6,
        ParticipantType::ProcessBinding => 7,
        ParticipantType::OperationBinding => 8,
    }
}

/// Independent SHA-256 mirror of the crate's deterministic barrier receipt
/// id derivation (`llmos/task-authority-takeover-barrier/v1`, truncated to
/// the first 16 bytes), so tests can pin the exact id any clean run derives
/// from the same material.
fn expected_receipt_id(
    takeover_receipt_id: ReceiptId,
    participant: &ParticipantRecord,
    remote_receipt_id: ReceiptId,
    barrier_digest: [u8; 32],
    fence_set_root: [u8; 32],
) -> ReceiptId {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/task-authority-takeover-barrier/v1");
    hasher.update(takeover_receipt_id.as_bytes());
    hasher.update(participant_type_code(participant.participant_type).to_be_bytes());
    hasher.update(participant.participant_id.as_bytes());
    hasher.update(participant.participant_generation.get().to_be_bytes());
    hasher.update(participant.admission_receipt_id.as_bytes());
    hasher.update(remote_receipt_id.as_bytes());
    hasher.update(barrier_digest);
    hasher.update(fence_set_root);
    let digest: [u8; 32] = hasher.finalize().into();
    ReceiptId::from_bytes(digest[..16].try_into().expect("receipt id prefix"))
}

/// Recovers the signed-observation fixture pieces every parent-side redo
/// needs from the reopened task database: the pending takeover receipt, the
/// first fence-manifest participant, and the exact fence set root.
fn signed_fixture(
    authority: &SqliteTaskAuthority,
    registry_binding: ParticipantRegistryBinding,
) -> (AuthorityTakeoverReceiptRecord, ParticipantRecord, [u8; 32]) {
    let fence = authority
        .inspect_authority_takeover_fence_receipt(task_id(), registry_binding)
        .expect("fence receipt");
    let takeover = authority
        .inspect_authority_takeover_receipt(task_id(), fence.receipt_id)
        .expect("takeover receipt");
    let participant = authority
        .inspect_authority_takeover_fence_members(task_id(), registry_binding)
        .expect("fence member manifest")
        .first()
        .expect("fence member")
        .participant;
    let fence_set_root = takeover.exact_fence_set_root.expect("exact fence set root");
    (takeover, participant, fence_set_root)
}

/// Builds the request + signature for the canonical observation and drives
/// the PUBLIC signed API end to end.
fn record_signed_observation(
    authority: &SqliteTaskAuthority,
    identity: &IdentityAuthority,
    signer: &BarrierSigner,
    takeover: &AuthorityTakeoverReceiptRecord,
    participant: ParticipantRecord,
    fence_set_root: [u8; 32],
) -> (
    AuthorityTakeoverBarrierReceiptRecord,
    AuthorityTakeoverBarrierReceiptRequest,
    BarrierObservationSignature,
) {
    let request = barrier_request(takeover, participant);
    let message_digest = barrier_observation_signature_message(
        request.takeover_receipt_id,
        &request.participant,
        request.remote_receipt_id,
        request.barrier_digest,
        fence_set_root,
    );
    let signature = barrier_signature(signer, message_digest);
    let record = authority
        .record_authority_takeover_barrier_receipt_signed(
            identity,
            request,
            barrier_signature(signer, message_digest),
        )
        .expect("record signed barrier observation");
    (record, request, signature)
}

// ---------------------------------------------------------------------------
// kill-9 child-process harness (takeover_fault_injection.rs 范式:
// current_exe + env vars + piped READY marker, never sleeps)
// ---------------------------------------------------------------------------

fn spawn_child(scenario: &str, path: &Path, identity: &Path) -> Child {
    Command::new(std::env::current_exe().expect("current test executable"))
        .args(["--exact", "crash_child_helper", "--nocapture"])
        .env("NLOS_BARRIER_SIG_CRASH_CHILD_SCENARIO", scenario)
        .env("NLOS_BARRIER_SIG_CRASH_CHILD_DATABASE", path)
        .env("NLOS_BARRIER_SIG_CRASH_CHILD_IDENTITY", identity)
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
// kill-9 child scenarios
// ---------------------------------------------------------------------------

/// Matrix row 1 fixture: the full signed-write prefix is committed (task,
/// lease-bound permit, finalize, term-2 lease, fence, identity bootstrap,
/// signature over the real observation preimage), then a raw writer
/// transaction inserts the phantom SIGNED barrier row (all material columns
/// real, all five signer columns real, a colliding fake receipt id) and dies
/// before commit — modelling `record_authority_takeover_barrier_receipt_
/// signed` interrupted between the insert and the commit.
#[allow(clippy::too_many_lines)] // One test covers a full F1-F6 fault-matrix row.
fn child_mid_signed_record_tx(path: &Path, identity_path: &Path) -> ! {
    let authority = SqliteTaskAuthority::open(path).expect("open");
    let prefix = seed_prefix(&authority);
    fence_takeover(&authority, &prefix, 210);
    let (takeover, participant, fence_set_root) =
        signed_fixture(&authority, prefix.registry_binding);
    let identity = IdentityAuthority::open(identity_path).expect("open identity authority");
    let signer = bootstrap_barrier_signer(&identity);
    let message_digest = barrier_observation_signature_message(
        takeover.receipt_id,
        &participant,
        ReceiptId::from_bytes([REMOTE_SEED; 16]),
        [DIGEST_SEED; 32],
        fence_set_root,
    );
    let signature = barrier_signature(&signer, message_digest);
    let raw = rusqlite::Connection::open(path).expect("raw connection");
    raw.execute_batch("BEGIN IMMEDIATE").expect("begin mid-tx");
    let task_hex = "01010101010101010101010101010101";
    raw.execute_batch(&format!(
        "INSERT INTO task_authority_takeover_barrier_receipts (
            receipt_id, takeover_receipt_id, task_id, task_generation,
            participant_type, participant_id, participant_generation,
            admission_receipt_id, remote_receipt_id, barrier_receipt_digest,
            fence_set_root, barrier_state, observed_at_ms,
            signer_principal_id, signer_control_domain_id, signer_key_id,
            signer_key_generation, signer_signature
         ) VALUES (
            X'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb', X'{}', X'{task_hex}',
            X'0000000000000001',
            {}, X'{}', X'{:016x}',
            X'{}', X'{}', X'{}',
            X'{}', 1, {OBSERVED_AT_MS},
            X'{}', X'{}', X'{}',
            {}, X'{}'
         )",
        hex_encode(takeover.receipt_id.as_bytes()),
        participant_type_code(participant.participant_type),
        hex_encode(participant.participant_id.as_bytes()),
        participant.participant_generation.get(),
        hex_encode(participant.admission_receipt_id.as_bytes()),
        hex_encode(&[REMOTE_SEED; 16]),
        hex_encode(&[DIGEST_SEED; 32]),
        hex_encode(&fence_set_root),
        hex_encode(signer.binding.principal_id.as_bytes()),
        hex_encode(signer.binding.control_domain_id.as_bytes()),
        hex_encode(signer.binding.key_id.as_bytes()),
        signer.binding.key_generation.get(),
        hex_encode(&signature.signature),
    ))
    .expect("mid-tx phantom signed barrier observation");
    announce("READY");
    let _keepers = (authority, raw, identity);
    loop {
        std::thread::park();
    }
}

/// Matrix row 2 / torn-tail fixture: the complete chain — prefix, fence,
/// identity bootstrap, and the SIGNED observation committed through the
/// public API — is durable before the kill; the signed insert is the last
/// committed WAL transaction.
fn child_signed_chain_complete(path: &Path, identity_path: &Path) -> ! {
    let authority = SqliteTaskAuthority::open(path).expect("open");
    let prefix = seed_prefix(&authority);
    fence_takeover(&authority, &prefix, 210);
    let (takeover, participant, fence_set_root) =
        signed_fixture(&authority, prefix.registry_binding);
    let identity = IdentityAuthority::open(identity_path).expect("open identity authority");
    let signer = bootstrap_barrier_signer(&identity);
    record_signed_observation(
        &authority,
        &identity,
        &signer,
        &takeover,
        participant,
        fence_set_root,
    );
    announce("READY");
    let _keepers = (authority, identity);
    loop {
        std::thread::park();
    }
}

/// Child entry point. Runs only when spawned by a parent test with the
/// scenario environment set; a no-op in the normal test run.
#[test]
fn crash_child_helper() {
    let (Ok(scenario), Ok(path), Ok(identity_path)) = (
        std::env::var("NLOS_BARRIER_SIG_CRASH_CHILD_SCENARIO"),
        std::env::var("NLOS_BARRIER_SIG_CRASH_CHILD_DATABASE"),
        std::env::var("NLOS_BARRIER_SIG_CRASH_CHILD_IDENTITY"),
    ) else {
        return;
    };
    let path = PathBuf::from(path);
    let identity_path = PathBuf::from(identity_path);
    match scenario.as_str() {
        "mid-signed-record-tx" => child_mid_signed_record_tx(&path, &identity_path),
        "signed-record-commit-complete" | "torn-wal-signed-record" => {
            child_signed_chain_complete(&path, &identity_path);
        }
        other => panic!("unknown crash child scenario {other}"),
    }
}

// ---------------------------------------------------------------------------
// 矩阵行 1: kill-9 mid-transaction on the signed observation write
// ---------------------------------------------------------------------------

/// kill-9 中断签名 barrier observation 写事务：子进程在幻影签名行（真实
/// material + 真实 5 signer 列 + 碰撞的伪造 receipt id）已写入未提交时被
/// 强杀；重开后幻影行完全回滚（barrier 表空、registry 保持
/// `FrozenForTakeover`、assignment `TakeoverPending`、`control_epoch`
/// 不动）；同一 key/material 经公共签名 API 重做成功，receipt id 与独立
/// SHA-256 镜像推导（clean-run 确定性 id）逐位一致，重放返回含 signer
/// 字段的完整原记录。
#[test]
#[allow(clippy::too_many_lines)] // One test covers a full F1-F6 fault-matrix row.
fn fault_kill9_mid_signed_barrier_tx_leaves_no_half_state() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestDatabase::new("kill9-mid-signed-tx");
    let identity_root = IdentityRoot::new("kill9-mid-signed-tx");
    let mut child = spawn_child("mid-signed-record-tx", &database.path, identity_root.path());
    await_marker(&mut child);
    kill_and_reap(&mut child);

    // Nothing uncommitted may survive: the uncommitted signed phantom is
    // gone while the committed takeover chain is intact.
    assert_eq!(
        raw_count(&database.path, "task_authority_takeover_barrier_receipts"),
        0,
        "phantom signed row must be rolled back"
    );
    assert_eq!(
        raw_count(&database.path, "task_authority_takeover_fence_receipts"),
        1
    );
    assert_eq!(
        raw_count(&database.path, "task_authority_takeover_receipts"),
        1
    );
    assert_eq!(raw_count(&database.path, "task_authority_lease_history"), 2);
    assert_eq!(raw_count(&database.path, "task_authority_assignments"), 1);

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
        ParticipantRegistryState::FrozenForTakeover,
        "mid-transaction dirt must not unfreeze the registry"
    );
    assert_eq!(
        assignment(&authority).state,
        AuthorityAssignmentState::TakeoverPending
    );
    let (takeover, participant, fence_set_root) = signed_fixture(&authority, registry_binding);
    assert_eq!(
        authority
            .inspect_task(task_id())
            .expect("head")
            .control_epoch,
        authority
            .inspect_authority_takeover_fence_receipt(task_id(), registry_binding)
            .expect("fence receipt")
            .control_epoch,
        "mid-transaction dirt must not advance the control epoch"
    );

    // The interrupted decision is redoable from the committed prefix through
    // the same key/material, deriving the deterministic clean-run receipt id.
    let identity = IdentityAuthority::open(identity_root.path()).expect("reopen identity");
    let signer = bootstrap_barrier_signer(&identity);
    let (record, request, signature) = record_signed_observation(
        &authority,
        &identity,
        &signer,
        &takeover,
        participant,
        fence_set_root,
    );
    assert_eq!(
        record.receipt_id,
        expected_receipt_id(
            request.takeover_receipt_id,
            &participant,
            request.remote_receipt_id,
            request.barrier_digest,
            fence_set_root
        ),
        "redo must derive the deterministic clean-run receipt id"
    );
    assert_eq!(record.signer, Some(expected_signer(&signer, &signature)));
    assert_eq!(
        authority
            .inspect_authority_takeover_barrier_coverage(takeover.receipt_id)
            .expect("coverage after redo")
            .state,
        AuthorityTakeoverBarrierCoverageState::LocallyCovered
    );

    // The exact signed replay returns the identical record, signer included,
    // and never duplicates the row.
    assert_eq!(
        authority
            .record_authority_takeover_barrier_receipt_signed(&identity, request, signature)
            .expect("signed replay after redo"),
        record
    );
    assert_eq!(
        raw_count(&database.path, "task_authority_takeover_barrier_receipts"),
        1
    );
    assert_integrity(&database.path);
}

// ---------------------------------------------------------------------------
// 矩阵行 2: kill-9 after commit — the signed observation survives byte-equal
// ---------------------------------------------------------------------------

/// commit 后崩溃：子进程在签名 observation 完整提交返回后被强杀；重开后
/// 每个事实逐位保留（签名行含 5 signer 列与 digest、registry
/// `FrozenForTakeover`、coverage `LocallyCovered`）；同一 key/material 的
/// 重放返回原记录、无重复行。
#[test]
#[allow(clippy::too_many_lines)] // One test covers a full F1-F6 fault-matrix row.
fn fault_kill9_after_signed_barrier_commit_preserves_everything() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestDatabase::new("kill9-signed-commit");
    let identity_root = IdentityRoot::new("kill9-signed-commit");
    let mut child = spawn_child(
        "signed-record-commit-complete",
        &database.path,
        identity_root.path(),
    );
    await_marker(&mut child);
    kill_and_reap(&mut child);

    assert_eq!(
        raw_count(&database.path, "task_authority_takeover_barrier_receipts"),
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
        raw_count(&database.path, "task_authority_takeover_fence_members"),
        1
    );
    assert_eq!(raw_count(&database.path, "task_authority_lease_history"), 2);

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
    let (takeover, participant, fence_set_root) = signed_fixture(&authority, registry_binding);
    let durable = authority
        .inspect_authority_takeover_barrier_receipts(takeover.receipt_id)
        .expect("signed observations durable");
    assert_eq!(durable.len(), 1);
    assert_eq!(durable[0].barrier_digest, Some([DIGEST_SEED; 32]));
    assert_eq!(durable[0].fence_set_root, fence_set_root);

    // The five signer columns are byte-preserved and reconstructible from
    // the same deterministic key material after the crash.
    let identity = IdentityAuthority::open(identity_root.path()).expect("reopen identity");
    let signer = bootstrap_barrier_signer(&identity);
    let message_digest = barrier_observation_signature_message(
        takeover.receipt_id,
        &participant,
        ReceiptId::from_bytes([REMOTE_SEED; 16]),
        [DIGEST_SEED; 32],
        fence_set_root,
    );
    let signature = barrier_signature(&signer, message_digest);
    assert_eq!(
        durable[0].signer,
        Some(expected_signer(&signer, &signature)),
        "signer columns must survive the kill byte-equal"
    );
    assert_eq!(
        durable[0].receipt_id,
        expected_receipt_id(
            takeover.receipt_id,
            &participant,
            ReceiptId::from_bytes([REMOTE_SEED; 16]),
            [DIGEST_SEED; 32],
            fence_set_root
        )
    );
    assert_eq!(
        authority
            .inspect_authority_takeover_barrier_coverage(takeover.receipt_id)
            .expect("coverage durable")
            .state,
        AuthorityTakeoverBarrierCoverageState::LocallyCovered
    );

    // The exact signed replay returns the original record without duplicating.
    let request = barrier_request(&takeover, participant);
    assert_eq!(
        authority
            .record_authority_takeover_barrier_receipt_signed(&identity, request, signature)
            .expect("signed replay after kill"),
        durable[0]
    );
    assert_eq!(
        raw_count(&database.path, "task_authority_takeover_barrier_receipts"),
        1,
        "exact signed replay must not duplicate the observation"
    );
    assert_integrity(&database.path);
}

// ---------------------------------------------------------------------------
// 矩阵行 3: hard I/O error on the signed observation write fails closed
// ---------------------------------------------------------------------------

/// 写入硬 I/O 错误（签名 observation 事务）：`FailWritesAfter { 0, IoErr }`
/// 下 `record_authority_takeover_barrier_receipt_signed` 必须以
/// `TaskStoreError::Sqlite` 显式失败（错误链含 I/O 条件），不返回假成功；
/// 无半截状态（无签名行、takeover receipt/registry/coverage 原样、
/// coverage 仍缺该 participant、`control_epoch` 不动）；disarm 后同一
/// key/material 操作成功、coverage 达到 `LocallyCovered`。
#[test]
#[allow(clippy::too_many_lines)] // One test covers a full F1-F6 fault-matrix row.
fn fault_io_error_on_signed_barrier_write_fails_closed() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestDatabase::new("ioerr-signed-barrier");
    let identity_root = IdentityRoot::new("ioerr-signed-barrier");
    let authority = open_shim(&database.path);
    let prefix = seed_prefix(&authority);
    fence_takeover(&authority, &prefix, 210);
    let (takeover, participant, fence_set_root) =
        signed_fixture(&authority, prefix.registry_binding);
    let control_before = authority
        .inspect_task(task_id())
        .expect("head")
        .control_epoch;
    let identity = IdentityAuthority::open(identity_root.path()).expect("open identity");
    let signer = bootstrap_barrier_signer(&identity);
    let request = barrier_request(&takeover, participant);
    let message_digest = barrier_observation_signature_message(
        request.takeover_receipt_id,
        &request.participant,
        request.remote_receipt_id,
        request.barrier_digest,
        fence_set_root,
    );
    let signature = barrier_signature(&signer, message_digest);

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::IoErr,
    });
    let error = authority
        .record_authority_takeover_barrier_receipt_signed(&identity, request, signature)
        .expect_err("signed barrier write must fail under injected I/O error");
    assert_sqlite_error_chain(&error, &["i/o", "ioerr"]);
    assert!(nlos_store_fault::writes_observed() > 0);

    // Zero half-state: no row, and the takeover chain is untouched.
    assert!(
        authority
            .inspect_authority_takeover_barrier_receipts(takeover.receipt_id)
            .expect("observations")
            .is_empty(),
        "failed signed write must leave no observation row"
    );
    assert_eq!(
        raw_count(&database.path, "task_authority_takeover_barrier_receipts"),
        0
    );
    assert_eq!(
        authority
            .inspect_authority_takeover_receipt(task_id(), takeover.fence_receipt_id)
            .expect("takeover receipt unchanged"),
        takeover
    );
    assert_eq!(
        authority
            .inspect_participant_registry(task_id())
            .expect("registry")
            .state,
        ParticipantRegistryState::FrozenForTakeover
    );
    assert_eq!(
        authority
            .inspect_task(task_id())
            .expect("head")
            .control_epoch,
        control_before,
        "failed signed write must not advance the control epoch"
    );
    let coverage = authority
        .inspect_authority_takeover_barrier_coverage(takeover.receipt_id)
        .expect("coverage");
    assert_eq!(coverage.observed_member_count, 0);
    assert_eq!(
        coverage.state,
        AuthorityTakeoverBarrierCoverageState::Partial
    );
    assert_eq!(coverage.missing_participants, vec![participant]);

    nlos_store_fault::disarm();
    let (record, request, signature) = record_signed_observation(
        &authority,
        &identity,
        &signer,
        &takeover,
        participant,
        fence_set_root,
    );
    assert_eq!(
        record.receipt_id,
        expected_receipt_id(
            request.takeover_receipt_id,
            &participant,
            request.remote_receipt_id,
            request.barrier_digest,
            fence_set_root
        )
    );
    assert_eq!(record.signer, Some(expected_signer(&signer, &signature)));
    assert_eq!(
        authority
            .inspect_authority_takeover_barrier_coverage(takeover.receipt_id)
            .expect("coverage after disarm")
            .state,
        AuthorityTakeoverBarrierCoverageState::LocallyCovered
    );
    assert_eq!(
        raw_count(&database.path, "task_authority_takeover_barrier_receipts"),
        1
    );
    assert_integrity(&database.path);
}

// ---------------------------------------------------------------------------
// 矩阵行 4: disk-full (ENOSPC) on the signed observation write fails closed
// ---------------------------------------------------------------------------

/// disk-full（签名 observation 事务）：`FailWritesAfter { 0, Full }` 下签名
/// 写事务必须以 `SQLITE_FULL` 显式失败（错误链含 full）；无半截状态；
/// disarm 后同一操作成功、coverage `LocallyCovered`。
#[test]
#[allow(clippy::too_many_lines)] // One test covers a full F1-F6 fault-matrix row.
fn fault_enospc_on_signed_barrier_write_fails_closed() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestDatabase::new("full-signed-barrier");
    let identity_root = IdentityRoot::new("full-signed-barrier");
    let authority = open_shim(&database.path);
    let prefix = seed_prefix(&authority);
    fence_takeover(&authority, &prefix, 210);
    let (takeover, participant, fence_set_root) =
        signed_fixture(&authority, prefix.registry_binding);
    let control_before = authority
        .inspect_task(task_id())
        .expect("head")
        .control_epoch;
    let identity = IdentityAuthority::open(identity_root.path()).expect("open identity");
    let signer = bootstrap_barrier_signer(&identity);
    let request = barrier_request(&takeover, participant);
    let message_digest = barrier_observation_signature_message(
        request.takeover_receipt_id,
        &request.participant,
        request.remote_receipt_id,
        request.barrier_digest,
        fence_set_root,
    );
    let signature = barrier_signature(&signer, message_digest);

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::Full,
    });
    let error = authority
        .record_authority_takeover_barrier_receipt_signed(&identity, request, signature)
        .expect_err("signed barrier write must fail under injected disk-full");
    assert_sqlite_error_chain(&error, &["full"]);
    assert!(nlos_store_fault::writes_observed() > 0);

    assert!(
        authority
            .inspect_authority_takeover_barrier_receipts(takeover.receipt_id)
            .expect("observations")
            .is_empty()
    );
    assert_eq!(
        raw_count(&database.path, "task_authority_takeover_barrier_receipts"),
        0
    );
    assert_eq!(
        authority
            .inspect_authority_takeover_receipt(task_id(), takeover.fence_receipt_id)
            .expect("takeover receipt unchanged"),
        takeover
    );
    assert_eq!(
        authority
            .inspect_task(task_id())
            .expect("head")
            .control_epoch,
        control_before
    );

    nlos_store_fault::disarm();
    let (record, _, _) = record_signed_observation(
        &authority,
        &identity,
        &signer,
        &takeover,
        participant,
        fence_set_root,
    );
    assert_eq!(record.signer, Some(expected_signer(&signer, &signature)));
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
// 矩阵行 5: silent write loss / torn tail hides the signed observation
// ---------------------------------------------------------------------------

/// 静默丢写/短写（签名 observation）：
/// - Phase A（断电模型）：`PowerLossAfter { 0 }` 下签名 observation “报告
///   成功”（返回完整记录含 signer）但写入从未落盘；重开后幻影不可见
///   （无行、coverage `Partial`/0 observed），同一请求重做且确定性
///   receipt id 与断电前 API 返回值及独立镜像推导逐位一致、signer 真实
///   持久后逐字节 round-trip。
/// - Phase B（撕裂尾部）：子进程提交 fence + 签名 observation 后被杀，WAL
///   截断在签名 insert commit 帧一半；重开后 fence 前缀完整、签名行整体
///   隐藏，同一签名请求重做收敛到相同确定性 id。
#[test]
fn fault_silent_write_loss_and_torn_tail_hide_signed_barrier_facts() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    power_loss_drops_signed_barrier_and_redo_is_durable();
    torn_wal_tail_hides_signed_barrier_and_redo_converges();
}

/// Phase A: a silently dropped signed observation is invisible after
/// recovery; the lost record is redoable with the same deterministic
/// identity and signer bytes.
#[allow(clippy::too_many_lines)] // One test covers a full F1-F6 fault-matrix row.
fn power_loss_drops_signed_barrier_and_redo_is_durable() {
    let database = TestDatabase::new("power-loss-signed-barrier");
    let identity_root = IdentityRoot::new("power-loss-signed-barrier");
    let authority = open_shim(&database.path);
    let prefix = seed_prefix(&authority);
    fence_takeover(&authority, &prefix, 210);
    let (takeover, participant, fence_set_root) =
        signed_fixture(&authority, prefix.registry_binding);
    let identity = IdentityAuthority::open(identity_root.path()).expect("open identity");
    let signer = bootstrap_barrier_signer(&identity);
    let request = barrier_request(&takeover, participant);
    let message_digest = barrier_observation_signature_message(
        request.takeover_receipt_id,
        &request.participant,
        request.remote_receipt_id,
        request.barrier_digest,
        fence_set_root,
    );
    let signature = barrier_signature(&signer, message_digest);

    nlos_store_fault::arm(FaultMode::PowerLossAfter { remaining: 0 });
    let phantom = authority
        .record_authority_takeover_barrier_receipt_signed(&identity, request, signature)
        .expect("power loss fabricates success, not an error");
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
        "silently dropped signed barrier must not fabricate an observation"
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

    // The lost signed observation is redoable: the phantom receipt id the
    // API reported matches both the independent clean-run derivation and
    // the genuinely durable redo, signer bytes included.
    let (redone, redo_request, _) = record_signed_observation(
        &recovered,
        &identity,
        &signer,
        &takeover,
        participant,
        fence_set_root,
    );
    assert_eq!(redone.receipt_id, phantom.receipt_id);
    assert_eq!(redone, phantom, "redo must reproduce the phantom record");
    assert_eq!(
        redone.receipt_id,
        expected_receipt_id(
            redo_request.takeover_receipt_id,
            &participant,
            redo_request.remote_receipt_id,
            redo_request.barrier_digest,
            fence_set_root
        )
    );
    assert_eq!(
        redone.signer,
        Some(expected_signer(&signer, &signature)),
        "signer must round-trip byte-equal after real persistence"
    );
    drop(recovered);
    let verified = SqliteTaskAuthority::open(&database.path).expect("reopen after redo");
    let durable = verified
        .inspect_authority_takeover_barrier_receipts(takeover.receipt_id)
        .expect("observations after redo");
    assert_eq!(durable.len(), 1);
    assert_eq!(durable[0], redone);
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

/// Phase B: truncating the WAL to half of the signed insert's commit frame
/// hides the entire signed observation while the committed fence prefix
/// survives bit-for-bit; the signed redo converges to the same
/// deterministic id.
#[allow(clippy::too_many_lines)] // One test covers a full F1-F6 fault-matrix row.
fn torn_wal_tail_hides_signed_barrier_and_redo_converges() {
    let database = TestDatabase::new("torn-tail-signed-barrier");
    let identity_root = IdentityRoot::new("torn-tail-signed-barrier");
    let mut child = spawn_child(
        "torn-wal-signed-record",
        &database.path,
        identity_root.path(),
    );
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
        "the committed fence prefix survives the torn signed tail"
    );
    let (takeover, participant, fence_set_root) = signed_fixture(&recovered, registry_binding);
    assert!(
        recovered
            .inspect_authority_takeover_barrier_receipts(takeover.receipt_id)
            .expect("observations")
            .is_empty(),
        "torn signed tail must hide the observation entirely"
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

    // The hidden signed observation is redoable with the same deterministic
    // clean-run id and then genuinely durable.
    let identity = IdentityAuthority::open(identity_root.path()).expect("reopen identity");
    let signer = bootstrap_barrier_signer(&identity);
    let (redone, redo_request, redo_signature) = record_signed_observation(
        &recovered,
        &identity,
        &signer,
        &takeover,
        participant,
        fence_set_root,
    );
    assert_eq!(
        redone.receipt_id,
        expected_receipt_id(
            redo_request.takeover_receipt_id,
            &participant,
            redo_request.remote_receipt_id,
            redo_request.barrier_digest,
            fence_set_root
        )
    );
    assert_eq!(
        redone.signer,
        Some(expected_signer(&signer, &redo_signature))
    );
    drop(recovered);
    let verified = SqliteTaskAuthority::open(&database.path).expect("reopen after redo");
    let durable = verified
        .inspect_authority_takeover_barrier_receipts(takeover.receipt_id)
        .expect("observations after redo");
    assert_eq!(durable.len(), 1);
    assert_eq!(durable[0], redone);
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

// ---------------------------------------------------------------------------
// 矩阵行 6: after the fault clears, the signed path continues from the
// committed prefix
// ---------------------------------------------------------------------------

/// 故障解除后：签名 observation 写事务在 `FailWritesAfter { 0, Full }` 下
/// 失败后 disarm，**同一 authority 实例**继续读写——已提交前缀（registry
/// `FrozenForTakeover`、assignment `TakeoverPending`、takeover receipt、
/// `control_epoch`）与故障前逐位一致；签名重试成功、coverage
/// `LocallyCovered`；完整重开后签名行（含 signer）逐位保留、重放返回原
/// 记录。
#[test]
#[allow(clippy::too_many_lines)] // One test covers a full F1-F6 fault-matrix row.
fn fault_after_disarm_signed_barrier_continues_from_committed_prefix() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestDatabase::new("disarm-signed-continue");
    let identity_root = IdentityRoot::new("disarm-signed-continue");
    let authority = open_shim(&database.path);
    let prefix = seed_prefix(&authority);
    fence_takeover(&authority, &prefix, 210);
    let (takeover, participant, fence_set_root) =
        signed_fixture(&authority, prefix.registry_binding);
    let control_before = authority
        .inspect_task(task_id())
        .expect("head")
        .control_epoch;
    let identity = IdentityAuthority::open(identity_root.path()).expect("open identity");
    let signer = bootstrap_barrier_signer(&identity);
    let request = barrier_request(&takeover, participant);
    let message_digest = barrier_observation_signature_message(
        request.takeover_receipt_id,
        &request.participant,
        request.remote_receipt_id,
        request.barrier_digest,
        fence_set_root,
    );
    let signature = barrier_signature(&signer, message_digest);

    nlos_store_fault::arm(FaultMode::FailWritesAfter {
        remaining: 0,
        code: FaultCode::Full,
    });
    let error = authority
        .record_authority_takeover_barrier_receipt_signed(&identity, request, signature)
        .expect_err("signed barrier write must fail while the fault is armed");
    assert_sqlite_error_chain(&error, &["full"]);

    // The committed prefix observed through the same authority is identical
    // to the pre-fault state.
    assert_eq!(
        raw_count(&database.path, "task_authority_takeover_barrier_receipts"),
        0
    );
    assert_eq!(
        authority
            .inspect_participant_registry(task_id())
            .expect("registry")
            .state,
        ParticipantRegistryState::FrozenForTakeover
    );
    assert_eq!(
        assignment(&authority).state,
        AuthorityAssignmentState::TakeoverPending
    );
    assert_eq!(
        authority
            .inspect_authority_takeover_receipt(task_id(), takeover.fence_receipt_id)
            .expect("takeover receipt identical"),
        takeover
    );
    assert_eq!(
        authority
            .inspect_task(task_id())
            .expect("head")
            .control_epoch,
        control_before
    );

    nlos_store_fault::disarm();

    // The retry succeeds on the same authority instance.
    let (record, _, _) = record_signed_observation(
        &authority,
        &identity,
        &signer,
        &takeover,
        participant,
        fence_set_root,
    );
    assert_eq!(record.signer, Some(expected_signer(&signer, &signature)));
    assert_eq!(
        authority
            .inspect_authority_takeover_barrier_coverage(takeover.receipt_id)
            .expect("coverage")
            .state,
        AuthorityTakeoverBarrierCoverageState::LocallyCovered
    );
    drop(authority);

    // A full reopen confirms the post-recovery signed write is durable.
    let reopened = SqliteTaskAuthority::open(&database.path).expect("reopen after recovery");
    assert_eq!(
        reopened
            .inspect_participant_registry(task_id())
            .expect("registry")
            .state,
        ParticipantRegistryState::FrozenForTakeover
    );
    let durable = reopened
        .inspect_authority_takeover_barrier_receipts(takeover.receipt_id)
        .expect("signed observations durable");
    assert_eq!(durable, vec![record]);
    assert_eq!(
        reopened
            .inspect_task(task_id())
            .expect("head")
            .control_epoch,
        control_before
    );
    assert_eq!(
        raw_count(&database.path, "task_authority_takeover_barrier_receipts"),
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
    assert_eq!(raw_count(&database.path, "task_authority_lease_history"), 2);

    // The exact signed replay against the reopened identity authority
    // returns the original durable record.
    let replay_identity = IdentityAuthority::open(identity_root.path()).expect("identity reopen");
    let replay_signer = bootstrap_barrier_signer(&replay_identity);
    let replay_message = barrier_observation_signature_message(
        request.takeover_receipt_id,
        &request.participant,
        request.remote_receipt_id,
        request.barrier_digest,
        fence_set_root,
    );
    assert_eq!(
        reopened
            .record_authority_takeover_barrier_receipt_signed(
                &replay_identity,
                request,
                barrier_signature(&replay_signer, replay_message)
            )
            .expect("signed replay after reopen"),
        record
    );
    assert_eq!(
        raw_count(&database.path, "task_authority_takeover_barrier_receipts"),
        1
    );
    assert_integrity(&database.path);
}

// ---------------------------------------------------------------------------
// 反重放语义测试: a term-3 takeover cannot adopt a term-2 signature
// ---------------------------------------------------------------------------

/// Anti-replay probe for the signed observation surface: after the term-2
/// fence, the term-2 lease is expired and a term-3 lease acquired. A second
/// `prepare_authority_takeover_fence` on the already-`FrozenForTakeover`
/// registry with the term-3 lease must fail closed
/// (`AuthorityLeaseBindingMismatch` against the immutable term-2 fence
/// receipt), and a fresh lease-bound permit on the frozen registry must also
/// fail closed — so no term-3 takeover receipt (and therefore no term-3
/// preimage using a new `fence_set_root`) is constructible via the public
/// API. The durable term-2 signed observation stays replayable byte-equal
/// and nothing new is written.
#[test]
#[allow(clippy::too_many_lines)] // One test covers a full F1-F6 fault-matrix row.
fn fault_term3_takeover_cannot_replay_term2_signed_observation() {
    let _serialization = fault_lock();
    nlos_store_fault::disarm();
    let database = TestDatabase::new("term3-anti-replay");
    let identity_root = IdentityRoot::new("term3-anti-replay");
    let authority = SqliteTaskAuthority::open(&database.path).expect("open");
    let prefix = seed_prefix(&authority);
    fence_takeover(&authority, &prefix, 210);
    let (takeover, participant, fence_set_root) =
        signed_fixture(&authority, prefix.registry_binding);
    let identity = IdentityAuthority::open(identity_root.path()).expect("open identity");
    let signer = bootstrap_barrier_signer(&identity);
    let (term2_record, term2_request, term2_signature) = record_signed_observation(
        &authority,
        &identity,
        &signer,
        &takeover,
        participant,
        fence_set_root,
    );

    // Term-2 lease (expires at 301) is expired at 401; holder 3 takes over.
    let lease_three = lease_record(
        authority
            .acquire_authority_lease(lease_request(3, 0xa3, 401, 100))
            .expect("term-3 lease"),
    );
    assert_eq!(lease_three.term, 3);

    // Second fence on the already-frozen registry with the term-3 lease:
    // the durable term-2 fence receipt is immutable, so the binding CAS
    // fails closed — this is the structural anti-replay gate.
    let fence_error = authority
        .prepare_authority_takeover_fence(AuthorityLeaseTakeoverFenceRequest {
            task_id: task_id(),
            expected_registry_binding: prefix.registry_binding,
            lease: lease_three,
            requested_at_ms: 410,
        })
        .expect_err("second fence on the frozen registry must fail closed");
    assert!(
        matches!(fence_error, TaskStoreError::AuthorityLeaseBindingMismatch),
        "expected the immutable term-2 fence binding to reject the term-3 fence, got {fence_error}"
    );

    // Alternate construction probe: a fresh lease-bound permit on the frozen
    // registry (which would mint a new registry generation to fence) is
    // also rejected fail-closed.
    let second_attempt = AttemptSpec {
        attempt_id: TaskAttemptId::from_bytes(bytes(0x12)),
        idempotency_key: IdempotencyKey::from_bytes([0x13; 16]),
        snapshot: SnapshotBundle {
            snapshot_id: TaskSnapshotId::from_bytes(bytes(0x14)),
            snapshot_digest: [0x15; 32],
            // The finalize of the first attempt advanced the head commit seq
            // to 1; the fresh snapshot must observe the current head to get
            // past the staleness gate and reach the registry freeze.
            expected_head_commit_seq: 1,
            ..attempt_spec().snapshot
        },
        ..attempt_spec()
    };
    assert!(matches!(
        authority.register_attempt(second_attempt),
        Ok(AttemptRegistrationDecision::Created(_))
    ));
    let permit_error = authority
        .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
            permit: permit_request(&second_attempt, 0xb3, 450),
            lease: lease_three,
        })
        .expect_err("a new permit must not supersede a takeover-frozen registry");
    assert!(
        matches!(
            permit_error,
            TaskStoreError::ParticipantRegistryFrozen {
                state: ParticipantRegistryState::FrozenForTakeover
            }
        ),
        "expected the frozen registry to reject the permit, got {permit_error}"
    );

    // Zero rows exist for any term-3 takeover: only the term-2 chain is
    // durable, and the term-2 signed observation replays byte-equal.
    assert_eq!(
        raw_count(&database.path, "task_authority_takeover_fence_receipts"),
        1
    );
    assert_eq!(
        raw_count(&database.path, "task_authority_takeover_receipts"),
        1,
        "no term-3 takeover receipt may exist"
    );
    assert_eq!(
        raw_count(&database.path, "task_authority_takeover_barrier_receipts"),
        1
    );
    assert_eq!(raw_count(&database.path, "task_authority_lease_history"), 3);
    assert_eq!(
        authority
            .record_authority_takeover_barrier_receipt_signed(
                &identity,
                term2_request,
                term2_signature
            )
            .expect("term-2 signed replay stays valid"),
        term2_record
    );
    assert_eq!(
        raw_count(&database.path, "task_authority_takeover_barrier_receipts"),
        1
    );
    assert_integrity(&database.path);
}
