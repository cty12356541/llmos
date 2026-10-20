//! CS0-A: the single-host dual-Cell evidence of the cross-Cell takeover
//! drive — ADR-0019 R1.2.2's only takeover-execution channel (the
//! `DIST-TASK-002` CAS chain driven by a successor) plus the
//! `DIST-TASK-004` successor-registry baseline.
//!
//! Two independent OS processes share one task store on the same
//! filesystem (ADR-0018: one process = one Cell; shared kernel, shared
//! clock domain, shared filesystem):
//!
//! 1. the child process is Cell A, the old authority — it creates the
//!    task store, holds the term-1 `TaskAuthority` lease, registers the
//!    Task / attempt / lease-bound `CommitPermit` that establishes the
//!    participant-registry baseline, finalizes that permit, bootstraps a
//!    `BarrierObservationSigning` principal in the shared identity
//!    authority, and stays alive throughout;
//! 2. the parent process is Cell B, the successor — after Cell A's lease
//!    has logically expired it drives
//!    [`drive_cross_cell_takeover`](nlos_slice_k::drive_cross_cell_takeover)
//!    over the same store file: term-2 lease takeover, fence prepare, one
//!    signed barrier observation per exact-fence manifest member (each
//!    demand round-trips to Cell A, which signs it; the store's signed
//!    gate verifies the Ed25519 proof through the shared identity
//!    authority before anything becomes durable), completion, successor
//!    assignment activation, and the successor-registry reopen;
//! 3. the still-live old Cell A then proves it is fenced: both a
//!    lease-bound prepare and a lease-bound commit-permit request under
//!    its stale term-1 lease are rejected.
//!
//! Durable-table evidence of the schema v27–v38 family is asserted
//! read-only through the typed inspect APIs and, where no typed reader
//! exists (lease/term history rows, fenced assignment states, raw receipt
//! columns), through direct SQLite reads of the same database file.
//!
//! **Evidence scope (honesty bound):** this is *single-host, dual-process*
//! evidence — shared filesystem, shared kernel, shared clock domain. It
//! is exactly what ADR-0019 decision 1 defers cross-machine semantics
//! behind; citing it as cross-host / network-partition / cross-clock
//! evidence would violate RISK-B-11. Endpoint coverage declared: this
//! run's exact-fence manifest covers the `TaskStore` participant
//! endpoint only — the six-type endpoint mapping is Phase 1 (CS1-C)
//! work, and no other endpoint coverage is claimed.
//!
//! Harness follows the `nlos-cell` dual-process convention: an `#[ignore]`
//! child helper spawned from this very test binary via env + temp-file
//! signaling, no product IPC. One parent `#[test]` owns the store-scoped
//! drive exactly once.

use std::fs;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use ed25519_dalek::{Signer, SigningKey};
use nlos_identity::{BootstrapPrincipalRequest, IdentityAuthority, KeyPurpose};
use nlos_slice_k::{
    BarrierObservationDemand, BarrierObservationSource, CrossCellTakeoverRequest,
    SignedBarrierObservation, TakeoverDriveError, drive_cross_cell_takeover,
};
use nlos_task::{
    AuthorityAssignmentState, AuthorityLeaseDecision, AuthorityLeaseFinalizeRequest,
    AuthorityLeasePermitRequest, AuthorityLeaseRequest, AuthorityLeaseTakeoverFenceRequest,
    AuthorityTakeoverReceiptState, BarrierObservationSignature, FinalizeRequest, FinalizeRequestV3,
    ParticipantRecord, ParticipantRegistryState, ParticipantType, PermitDecision, PermitRequest,
    SnapshotBundle, SqliteTaskAuthority, TaskStoreError, barrier_observation_signature_message,
    empty_effect_history_root,
};
use nlos_types::{
    CancellationScopeId, ControlDomainId, Generation, IdempotencyKey, KeyId, PrincipalId,
    ProcessId, ReceiptId, TaskAttemptId, TaskId, TaskSnapshotId,
};
use rusqlite::Connection;
use sha2::{Digest, Sha256};

const STORE_PATH_ENV: &str = "NLOS_CS0A_STORE_PATH";
const IDENTITY_ROOT_ENV: &str = "NLOS_CS0A_IDENTITY_ROOT";
const OUT_ENV: &str = "NLOS_CS0A_OUT";
const SIGN_REQUEST_ENV: &str = "NLOS_CS0A_SIGN_REQUEST";
const SIGN_RESPONSE_ENV: &str = "NLOS_CS0A_SIGN_RESPONSE";
const VERIFY_ENV: &str = "NLOS_CS0A_VERIFY";
const RELEASE_ENV: &str = "NLOS_CS0A_RELEASE";
const POLL: Duration = Duration::from_millis(10);
const CHILD_WAIT: Duration = Duration::from_secs(30);

/// Logical clock of the run. Cell A's term-1 lease is requested at 100
/// with TTL 100 (expires at 200); the successor's clock starts after
/// that expiry so the lease takeover is the legal expired-incumbent
/// path, never a live-incumbent grab.
const LEASE_ONE_AT_MS: i64 = 100;
const LEASE_ONE_TTL_MS: i64 = 100;
const PARENT_CLOCK_BASE_MS: i64 = 200;
const PARENT_CLOCK_STEP_MS: i64 = 10;
/// Generous successor-lease TTL: every later drive timestamp (≤ 250) and
/// nothing else must stay inside the window.
const PARENT_LEASE_TTL_MS: i64 = 100_000;
/// Cell A's stale-lease rejection attempts run logically after the whole
/// drive; the binding mismatch, not expiry, is what must fence them.
const CHILD_VERIFY_AT_MS: i64 = 300;

const CHILD_SEED: u8 = 0xC1;
const CHILD_HOLDER_TAG: u8 = 0xB1;
const PARENT_HOLDER_TAG: u8 = 0xB2;
const SIGNER_SEED: u8 = 0xA1;
/// The Task every durable fact of this run is about.
const TASK_SEED: u8 = 0xD1;

fn task_id() -> TaskId {
    TaskId::from_bytes([TASK_SEED; 16])
}

fn holder_from_pid(pid: u32, tag: u8) -> ProcessId {
    let mut bytes = [0_u8; 16];
    bytes[..4].copy_from_slice(&pid.to_be_bytes());
    bytes[15] = tag;
    ProcessId::from_bytes(bytes)
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut out, "{byte:02x}").expect("write hex nibble");
    }
    out
}

fn hex_nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        other => panic!("hex nibble byte {}", char::from(other)),
    }
}

fn fixed_hex<const N: usize>(line: &str) -> [u8; N] {
    let bytes = line.as_bytes();
    assert_eq!(bytes.len(), N * 2, "hex line must be {N} bytes wide");
    let mut out = [0_u8; N];
    for (index, slot) in out.iter_mut().enumerate() {
        *slot = (hex_nibble(bytes[2 * index]) << 4) | hex_nibble(bytes[2 * index + 1]);
    }
    out
}

/// Mirrors the store's private participant wire codes (1..=8) so the
/// fixture's deterministic endpoint-receipt derivation can hash the same
/// participant axes the signature message binds.
fn participant_wire(participant_type: ParticipantType) -> u8 {
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

/// Deterministic endpoint-durable receipt identity Cell A mints for one
/// barrier observation. Shared by the child (minter) and the parent
/// (expectation) so the stored rows can be asserted byte-exact.
fn poc_remote_receipt_id(
    takeover_receipt_id: ReceiptId,
    participant: &ParticipantRecord,
) -> ReceiptId {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/cs0a-dual-cell/remote-receipt/v1");
    hasher.update(takeover_receipt_id.as_bytes());
    hasher.update([participant_wire(participant.participant_type)]);
    hasher.update(participant.participant_id.as_bytes());
    hasher.update(participant.participant_generation.get().to_be_bytes());
    hasher.update(participant.admission_receipt_id.as_bytes());
    let digest: [u8; 32] = hasher.finalize().into();
    ReceiptId::from_bytes(digest[..16].try_into().expect("remote receipt prefix"))
}

/// Deterministic endpoint-authored barrier digest Cell A binds into its
/// observation.
fn poc_barrier_digest(takeover_receipt_id: ReceiptId, participant: &ParticipantRecord) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/cs0a-dual-cell/barrier-digest/v1");
    hasher.update(takeover_receipt_id.as_bytes());
    hasher.update([participant_wire(participant.participant_type)]);
    hasher.update(participant.participant_id.as_bytes());
    hasher.update(participant.participant_generation.get().to_be_bytes());
    hasher.update(participant.admission_receipt_id.as_bytes());
    hasher.finalize().into()
}

struct TempDir {
    root: PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("nlos-slice-k-cs0a-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).expect("create temp root");
        Self { root }
    }

    fn path(&self) -> &Path {
        &self.root
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        match fs::remove_dir_all(&self.root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("remove cs0a temp root: {error}"),
        }
    }
}

/// Atomically (temp file + rename) publishes `payload` at `path`, so the
/// polling reader never observes a half-written file.
fn publish(path: &Path, payload: &str) {
    let mut staging = path.as_os_str().to_os_string();
    staging.push(".tmp");
    let staging = PathBuf::from(staging);
    fs::write(&staging, payload).expect("write staging file");
    fs::rename(&staging, path).expect("publish file atomically");
}

fn append_line(path: &Path, line: &str) {
    let mut file = OpenOptions::new()
        .append(true)
        .open(path)
        .expect("open receipt file for append");
    writeln!(file, "{line}").expect("append receipt line");
}

/// Polls for `path` to exist and contain `needle` while the child is
/// still alive, panicking on early child exit or timeout (the `nlos-cell`
/// harness convention).
fn wait_while_alive(child: &mut Child, what: &str, path: &Path, needle: &str) {
    let deadline = Instant::now() + CHILD_WAIT;
    loop {
        if fs::read_to_string(path).is_ok_and(|text| text.contains(needle)) {
            return;
        }
        match child.try_wait() {
            Ok(Some(status)) => panic!("child exited before {what}: {status:?}"),
            Ok(None) => {}
            Err(error) => panic!("child try_wait failed: {error}"),
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(POLL);
    }
}

/// Polls for the atomic publication at `path` while the child is still
/// alive and returns its full content.
fn wait_for_file_while_alive(child: &mut Child, what: &str, path: &Path) -> String {
    let deadline = Instant::now() + CHILD_WAIT;
    loop {
        if let Ok(text) = fs::read_to_string(path) {
            return text;
        }
        match child.try_wait() {
            Ok(Some(status)) => panic!("child exited before {what}: {status:?}"),
            Ok(None) => {}
            Err(error) => panic!("child try_wait failed: {error}"),
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(POLL);
    }
}

fn register_task_attempt(
    authority: &SqliteTaskAuthority,
    seed: u8,
    at_ms: i64,
) -> nlos_task::AttemptSpec {
    authority
        .register_task(nlos_task::TaskSpec {
            application_id: None,
            plan_revision: None,
            task_id: task_id(),
            task_generation: Generation::INITIAL,
            registered_at_ms: at_ms,
        })
        .expect("register task");
    let attempt = nlos_task::AttemptSpec {
        task_id: task_id(),
        attempt_id: TaskAttemptId::from_bytes([seed.wrapping_add(1); 16]),
        attempt_generation: Generation::INITIAL,
        snapshot: SnapshotBundle {
            snapshot_id: TaskSnapshotId::from_bytes([seed.wrapping_add(2); 16]),
            snapshot_digest: [seed.wrapping_add(3); 32],
            expected_head_commit_seq: 0,
            effect_history_root: empty_effect_history_root(),
            retry_fence_epoch: 0,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes([seed.wrapping_add(4); 16]),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(5); 16]),
        registered_at_ms: at_ms + 1,
    };
    authority
        .register_attempt(attempt)
        .expect("register attempt");
    attempt
}

fn permit_request(
    attempt: &nlos_task::AttemptSpec,
    seed: u8,
    requested_at_ms: i64,
) -> PermitRequest {
    PermitRequest {
        task_id: attempt.task_id,
        attempt_id: attempt.attempt_id,
        attempt_generation: attempt.attempt_generation,
        write_set_root: [seed; 32],
        planned_effects: Vec::new(),
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(10); 16]),
        valid_until_ms: 10_000,
        requested_at_ms,
    }
}

fn finalize_request(
    attempt: &nlos_task::AttemptSpec,
    permit_id: nlos_types::CommitPermitId,
    finalized_at_ms: i64,
) -> FinalizeRequestV3 {
    FinalizeRequestV3 {
        base: FinalizeRequest {
            task_id: attempt.task_id,
            attempt_id: attempt.attempt_id,
            attempt_generation: attempt.attempt_generation,
            permit_id,
            new_effect_history_root: [0; 32],
            new_retry_fence_epoch: 0,
            finalized_at_ms,
        },
        required_satisfaction: Vec::new(),
        fenced_participant_digest: [0; 32],
    }
}

/// Harness-only entry: Cell A, the old authority. Creates the task store,
/// holds the term-1 lease, establishes the registry baseline through a
/// lease-bound permit, finalizes it, bootstraps the barrier signer, then
/// serves signed barrier observations on demand until the successor
/// signals the fenced-verification stage — where the still-live old Cell
/// proves its stale lease no longer commits or prepares.
#[test]
#[ignore = "spawned by successor_cell_drives_full_takeover_and_old_cell_is_fenced"]
#[allow(clippy::too_many_lines)]
fn child_old_authority_cell_helper() {
    // Batch `--ignored`/`--include-ignored` runs reach this entry without
    // env; only the parent spawn makes it meaningful.
    let (
        Ok(store_path),
        Ok(identity_root),
        Ok(out_path),
        Ok(sign_request),
        Ok(sign_response),
        Ok(verify),
        Ok(release),
    ) = (
        std::env::var(STORE_PATH_ENV),
        std::env::var(IDENTITY_ROOT_ENV),
        std::env::var(OUT_ENV),
        std::env::var(SIGN_REQUEST_ENV),
        std::env::var(SIGN_RESPONSE_ENV),
        std::env::var(VERIFY_ENV),
        std::env::var(RELEASE_ENV),
    )
    else {
        eprintln!("skipped: cs0a child env unset (harness entry, parent spawn only)");
        return;
    };
    let (sign_request, sign_response) = (Path::new(&sign_request), Path::new(&sign_response));

    // --- Cell A establishes its authority term and the registry baseline.
    let authority = SqliteTaskAuthority::open(&store_path).expect("cell A opens its task store");
    let lease_one = match authority
        .acquire_authority_lease(AuthorityLeaseRequest {
            holder_id: holder_from_pid(std::process::id(), CHILD_HOLDER_TAG),
            idempotency_key: IdempotencyKey::from_bytes([CHILD_SEED; 16]),
            requested_at_ms: LEASE_ONE_AT_MS,
            ttl_ms: LEASE_ONE_TTL_MS,
        })
        .expect("cell A acquires its term-1 lease")
    {
        AuthorityLeaseDecision::Acquired(record) => record,
        other => panic!("expected a fresh acquire, got {other:?}"),
    };
    assert_eq!(lease_one.term, 1);
    let attempt = register_task_attempt(&authority, CHILD_SEED, LEASE_ONE_AT_MS + 1);
    let permit = match authority
        .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
            permit: permit_request(&attempt, CHILD_SEED.wrapping_add(0x21), LEASE_ONE_AT_MS + 3),
            lease: lease_one,
        })
        .expect("cell A issues its lease-bound permit")
    {
        PermitDecision::Issued(permit) => *permit,
        other => panic!("expected issued permit, got {other:?}"),
    };
    let registry_binding = permit
        .participant_registry_binding
        .expect("permit carries the registry baseline binding");
    authority
        .finalize_commit_v3_with_authority_lease(AuthorityLeaseFinalizeRequest {
            finalize: finalize_request(&attempt, permit.permit_id, LEASE_ONE_AT_MS + 4),
            lease: lease_one,
        })
        .expect("cell A finalizes its committed prefix");

    // --- The barrier signer of this Cell's endpoints, in the shared
    // identity authority both processes read.
    let identity = IdentityAuthority::open(&identity_root).expect("cell A opens identity root");
    let signing_key = SigningKey::from_bytes(&[SIGNER_SEED; 32]);
    let binding = identity
        .bootstrap_principal(BootstrapPrincipalRequest {
            principal_profile_digest: [SIGNER_SEED.wrapping_add(1); 32],
            control_domain_policy_digest: [SIGNER_SEED.wrapping_add(2); 32],
            public_key: signing_key.verifying_key().to_bytes(),
            key_purpose: KeyPurpose::BarrierObservationSigning,
            key_valid_from_ms: 0,
            key_valid_until_ms: 10_000_000,
            idempotency_key: IdempotencyKey::from_bytes([SIGNER_SEED.wrapping_add(3); 16]),
            created_at_ms: 0,
        })
        .expect("bootstrap the barrier signer")
        .binding();

    publish(
        Path::new(&out_path),
        &format!(
            "registered\n{}\n{}\n{}\n{}\n",
            std::process::id(),
            hex(binding.principal_id.as_bytes()),
            hex(binding.control_domain_id.as_bytes()),
            hex(binding.key_id.as_bytes()),
        ),
    );

    // --- Serve signed barrier observations until the fenced-verification
    // stage. Each demand names the pending takeover receipt, the frozen
    // participant, and the exact fence-set root; this Cell mints its own
    // endpoint receipt identity and digest and signs the store's
    // domain-separated message.
    let deadline = Instant::now() + CHILD_WAIT;
    while !Path::new(&verify).exists() {
        assert!(
            Instant::now() < deadline,
            "cell A timed out waiting for stage"
        );
        let Ok(request) = fs::read_to_string(sign_request) else {
            thread::sleep(POLL);
            continue;
        };
        fs::remove_file(sign_request).expect("consume sign request");
        let mut lines = request.lines();
        let takeover_receipt_id =
            ReceiptId::from_bytes(fixed_hex(lines.next().expect("takeover receipt line")));
        let participant_type_code = lines
            .next()
            .expect("participant type line")
            .parse::<u8>()
            .expect("participant type code");
        let participant = ParticipantRecord {
            participant_type: match participant_type_code {
                1 => ParticipantType::TaskStore,
                2 => ParticipantType::ArtifactHead,
                3 => ParticipantType::SemanticAdmission,
                4 => ParticipantType::ChannelTopic,
                5 => ParticipantType::DriverGateway,
                6 => ParticipantType::ResourceLedger,
                7 => ParticipantType::ProcessBinding,
                8 => ParticipantType::OperationBinding,
                other => panic!("unknown participant type code {other}"),
            },
            participant_id: nlos_types::TaskParticipantId::from_bytes(fixed_hex(
                lines.next().expect("participant id line"),
            )),
            participant_generation: Generation::new(
                NonZeroU64::new(
                    lines
                        .next()
                        .expect("participant generation line")
                        .parse::<u64>()
                        .expect("participant generation"),
                )
                .expect("nonzero generation"),
            ),
            admission_receipt_id: ReceiptId::from_bytes(fixed_hex(
                lines.next().expect("admission receipt line"),
            )),
        };
        let fence_set_root = fixed_hex(lines.next().expect("fence root line"));
        let remote_receipt_id = poc_remote_receipt_id(takeover_receipt_id, &participant);
        let barrier_digest = poc_barrier_digest(takeover_receipt_id, &participant);
        let message = barrier_observation_signature_message(
            takeover_receipt_id,
            &participant,
            remote_receipt_id,
            barrier_digest,
            fence_set_root,
        );
        let signature = signing_key.sign(&message).to_bytes();
        publish(
            sign_response,
            &format!(
                "{}\n{}\n{}\n",
                hex(remote_receipt_id.as_bytes()),
                hex(barrier_digest.as_slice()),
                hex(signature.as_slice()),
            ),
        );
    }

    // --- Fenced-verification stage: the old Cell is still alive, its
    // term-1 lease is stale, and both authority-bound mutations must be
    // rejected. A stale prepare first...
    let prepare_rejected = matches!(
        authority.prepare_authority_takeover_fence(AuthorityLeaseTakeoverFenceRequest {
            task_id: task_id(),
            expected_registry_binding: registry_binding,
            lease: lease_one,
            requested_at_ms: CHILD_VERIFY_AT_MS,
        }),
        Err(TaskStoreError::AuthorityLeaseFenced)
    );
    assert!(
        prepare_rejected,
        "stale term-1 lease must be fenced at the prepare gate"
    );
    append_line(
        Path::new(&out_path),
        "prepare_rejected:authority_lease_fenced",
    );

    // ...then a stale lease-bound commit permit over a freshly registered
    // attempt (so the refusal can only come from the lease fence).
    let head = authority.inspect_task(task_id()).expect("task head");
    let second_attempt = nlos_task::AttemptSpec {
        task_id: task_id(),
        attempt_id: TaskAttemptId::from_bytes([0xE1; 16]),
        attempt_generation: Generation::INITIAL,
        snapshot: SnapshotBundle {
            snapshot_id: TaskSnapshotId::from_bytes([0xE2; 16]),
            snapshot_digest: [0xE3; 32],
            expected_head_commit_seq: head.head_commit_seq,
            effect_history_root: head.head_effect_history_root,
            retry_fence_epoch: head.retry_fence_epoch,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes([0xE4; 16]),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes([0xE5; 16]),
        registered_at_ms: CHILD_VERIFY_AT_MS + 1,
    };
    authority
        .register_attempt(second_attempt)
        .expect("attempt registration itself is not lease-bound");
    let permit_rejected = matches!(
        authority.request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
            permit: permit_request(&second_attempt, 0xE6, CHILD_VERIFY_AT_MS + 2),
            lease: lease_one,
        }),
        Err(TaskStoreError::AuthorityLeaseFenced)
    );
    assert!(
        permit_rejected,
        "stale term-1 lease must be fenced at the commit gate"
    );
    append_line(
        Path::new(&out_path),
        "permit_rejected:authority_lease_fenced",
    );
    append_line(Path::new(&out_path), "fenced_ok");

    // Stay alive until the successor releases — live dual-Cell overlap.
    let deadline = Instant::now() + CHILD_WAIT;
    while !Path::new(&release).exists() {
        assert!(
            Instant::now() < deadline,
            "cell A timed out waiting for release"
        );
        thread::sleep(POLL);
    }
}

fn spawn_child(
    store_path: &Path,
    identity_root: &Path,
    out_path: &Path,
    sign_request: &Path,
    sign_response: &Path,
    verify: &Path,
    release: &Path,
) -> Child {
    Command::new(std::env::current_exe().expect("test executable"))
        .args([
            "--exact",
            "child_old_authority_cell_helper",
            "--nocapture",
            "--ignored",
        ])
        .env(STORE_PATH_ENV, store_path)
        .env(IDENTITY_ROOT_ENV, identity_root)
        .env(OUT_ENV, out_path)
        .env(SIGN_REQUEST_ENV, sign_request)
        .env(SIGN_RESPONSE_ENV, sign_response)
        .env(VERIFY_ENV, verify)
        .env(RELEASE_ENV, release)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn the old-authority Cell process")
}

/// Parent-side endpoint channel: carries the successor's barrier demand
/// to the still-live old Cell over the harness temp files and returns its
/// signed observation. Signer identity fields come from the child's
/// registration receipt, never from the response payload.
struct FileSignChannel<'a> {
    child: &'a mut Child,
    request_path: PathBuf,
    response_path: PathBuf,
    issuer: PrincipalId,
    control_domain_id: ControlDomainId,
    key_id: KeyId,
}

impl BarrierObservationSource for FileSignChannel<'_> {
    fn observe_barrier(
        &mut self,
        demand: BarrierObservationDemand,
    ) -> Result<SignedBarrierObservation, TakeoverDriveError> {
        let _ = fs::remove_file(&self.response_path);
        let participant = demand.participant;
        let payload = format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n",
            hex(demand.takeover_receipt_id.as_bytes()),
            participant_wire(participant.participant_type),
            hex(participant.participant_id.as_bytes()),
            participant.participant_generation.get(),
            hex(participant.admission_receipt_id.as_bytes()),
            hex(demand.fence_set_root.as_slice()),
        );
        fs::write(&self.request_path, payload)
            .map_err(|error| TakeoverDriveError::BarrierSource(format!("sign request: {error}")))?;
        let response = wait_for_file_while_alive(
            self.child,
            "signed barrier observation",
            &self.response_path,
        );
        let mut lines = response.lines();
        let remote_receipt_id =
            ReceiptId::from_bytes(fixed_hex(lines.next().expect("remote receipt line")));
        let barrier_digest: [u8; 32] = fixed_hex(lines.next().expect("barrier digest line"));
        let signature: [u8; 64] = fixed_hex(lines.next().expect("signature line"));
        fs::remove_file(&self.response_path).map_err(|error| {
            TakeoverDriveError::BarrierSource(format!("consume response: {error}"))
        })?;
        Ok(SignedBarrierObservation {
            remote_receipt_id,
            barrier_digest,
            signature: BarrierObservationSignature {
                issuer: self.issuer,
                control_domain_id: self.control_domain_id,
                key_id: self.key_id,
                signature,
            },
        })
    }
}

fn blob_u64(value: &[u8]) -> u64 {
    u64::from_be_bytes(value.try_into().expect("8-byte u64 blob"))
}

/// The successor Cell: drives the full cross-Cell takeover over the old
/// Cell's still-open task store and asserts the durable evidence of the
/// schema v27–v38 family, plus the still-live old Cell's fenced
/// rejections.
#[test]
#[allow(clippy::too_many_lines)]
fn successor_cell_drives_full_takeover_and_old_cell_is_fenced() {
    let stamp = std::process::id();
    let store_dir = TempDir::new("store");
    let identity_dir = TempDir::new("identity");
    let store_path = store_dir.path().join("task-authority.sqlite3");
    let out_path = std::env::temp_dir().join(format!("nlos-slice-k-cs0a-out-{stamp}.txt"));
    let sign_request = std::env::temp_dir().join(format!("nlos-slice-k-cs0a-req-{stamp}.txt"));
    let sign_response = std::env::temp_dir().join(format!("nlos-slice-k-cs0a-resp-{stamp}.txt"));
    let verify = std::env::temp_dir().join(format!("nlos-slice-k-cs0a-verify-{stamp}.flag"));
    let release = std::env::temp_dir().join(format!("nlos-slice-k-cs0a-release-{stamp}.flag"));
    for path in [&out_path, &sign_request, &sign_response, &verify, &release] {
        let _ = fs::remove_file(path);
    }

    let mut child = spawn_child(
        &store_path,
        identity_dir.path(),
        &out_path,
        &sign_request,
        &sign_response,
        &verify,
        &release,
    );

    // Stage 1: the old-authority Cell registered its term and signer.
    let registration = wait_for_file_while_alive(&mut child, "child registration", &out_path);
    let mut lines = registration.lines();
    assert_eq!(lines.next(), Some("registered"));
    let child_pid: u32 = lines
        .next()
        .expect("child pid line")
        .parse()
        .expect("child pid u32");
    let issuer = PrincipalId::from_bytes(fixed_hex(lines.next().expect("principal line")));
    let control_domain_id =
        ControlDomainId::from_bytes(fixed_hex(lines.next().expect("domain line")));
    let key_id = KeyId::from_bytes(fixed_hex(lines.next().expect("key line")));
    assert_ne!(child_pid, std::process::id());
    let child_holder = holder_from_pid(child_pid, CHILD_HOLDER_TAG);
    let parent_holder = holder_from_pid(std::process::id(), PARENT_HOLDER_TAG);

    // The successor opens the SAME durable store the old authority holds
    // (the Phase 0 transport shape: same-host shared-filesystem SQLite).
    let successor_store =
        SqliteTaskAuthority::open(&store_path).expect("successor opens the shared task store");
    let pre_lease = successor_store
        .inspect_authority_lease()
        .expect("read the incumbent lease");
    assert_eq!(pre_lease.term, 1);
    assert_eq!(pre_lease.holder_id, child_holder);
    let old_assignment = successor_store
        .inspect_authority_assignment(task_id())
        .expect("incumbent assignment baseline");
    assert_eq!(old_assignment.state, AuthorityAssignmentState::Active);
    let old_binding = old_assignment.participant_registry_binding;

    let identity =
        IdentityAuthority::open(identity_dir.path()).expect("successor opens identity root");
    let mut source = FileSignChannel {
        child: &mut child,
        request_path: sign_request.clone(),
        response_path: sign_response.clone(),
        issuer,
        control_domain_id,
        key_id,
    };
    let mut tick = PARENT_CLOCK_BASE_MS;
    let mut clock = || {
        tick += PARENT_CLOCK_STEP_MS;
        tick
    };

    let outcome = drive_cross_cell_takeover(CrossCellTakeoverRequest {
        store: &successor_store,
        identity: &identity,
        barrier_source: &mut source,
        task_id: task_id(),
        successor_holder_id: parent_holder,
        lease_idempotency_key: IdempotencyKey::from_bytes([0x42; 16]),
        lease_ttl_ms: PARENT_LEASE_TTL_MS,
        now_ms: &mut clock,
    })
    .expect("the successor drives the full takeover chain");
    drop(source);
    assert!(
        child.try_wait().expect("child alive at evidence").is_none(),
        "the old Cell must still be live when the takeover completes"
    );

    // --- Lease/term evidence: the successor holds term 2 of the same
    // authority, fencing the term-1 holder.
    assert_eq!(outcome.successor_lease.term, pre_lease.term + 1);
    assert_eq!(outcome.successor_lease.holder_id, parent_holder);
    assert_ne!(
        outcome.successor_lease.fencing_token, pre_lease.fencing_token,
        "the fencing token must rotate with the term"
    );

    // --- Fence evidence: `FROZEN_FOR_TAKEOVER` receipt with exact roots.
    assert_eq!(
        outcome.old_assignment.assignment_id,
        old_assignment.assignment_id
    );
    assert_eq!(
        outcome.frozen_registry.state,
        ParticipantRegistryState::FrozenForTakeover
    );
    assert_eq!(registry_binding_of(&outcome.frozen_registry), old_binding);
    assert_eq!(
        outcome.fence_receipt.frozen_registry_binding, old_binding,
        "the fence receipt pins the frozen generation/root"
    );
    assert_eq!(
        outcome.fence_receipt.authority_lease_binding,
        outcome.successor_lease.binding()
    );
    let exact_fence_set_root = outcome
        .fence_receipt
        .exact_fence_set_root
        .expect("exact fence set root is resolvable in this baseline");
    assert!(
        outcome
            .fence_receipt
            .outstanding_operation_participant_root
            .is_some()
    );

    // --- Pending receipt evidence: the old assignment was CAS-linked.
    assert_eq!(
        outcome.takeover_receipt_pending.barrier_state,
        AuthorityTakeoverReceiptState::Pending
    );
    assert_eq!(outcome.takeover_receipt_pending.new_assignment_id, None);
    assert_eq!(
        outcome.takeover_receipt_pending.old_assignment_id,
        old_assignment.assignment_id
    );
    assert_eq!(
        outcome.takeover_receipt_pending.frozen_registry_binding,
        old_binding
    );
    assert_eq!(
        outcome.takeover_receipt_pending.exact_fence_set_root,
        Some(exact_fence_set_root)
    );

    // --- Manifest + signed observation evidence. Endpoint coverage is
    // declared, not assumed: this run covers `TaskStore` endpoints only.
    assert_ne!(
        outcome.fence_members.len(),
        0,
        "the exact-fence manifest must be resolvable and non-empty in this baseline"
    );
    for member in &outcome.fence_members {
        assert_eq!(member.task_id, task_id());
        assert_eq!(
            member.participant.participant_type,
            ParticipantType::TaskStore,
            "declared endpoint coverage: TaskStore participants only"
        );
    }
    assert_eq!(outcome.observations.len(), outcome.fence_members.len());
    for (member, observation) in outcome.fence_members.iter().zip(&outcome.observations) {
        assert_eq!(observation.participant, member.participant);
        assert_eq!(observation.fence_set_root, exact_fence_set_root);
        assert_eq!(
            observation.remote_receipt_id,
            poc_remote_receipt_id(
                outcome.takeover_receipt_pending.receipt_id,
                &member.participant
            ),
            "the stored observation carries the endpoint-minted receipt identity"
        );
        assert_eq!(
            observation.barrier_digest,
            Some(poc_barrier_digest(
                outcome.takeover_receipt_pending.receipt_id,
                &member.participant
            )),
            "the stored observation carries the endpoint-authored digest"
        );
        let signer = observation
            .signer
            .as_ref()
            .expect("the v36 signer column is filled by the signed gate");
        assert_eq!(signer.principal_id, issuer);
        assert_eq!(signer.control_domain_id, control_domain_id);
        assert_eq!(signer.key_id, key_id);
    }

    // --- Completion evidence: receipt complete, successor activated.
    assert_eq!(
        outcome.completion.barrier_state,
        AuthorityTakeoverReceiptState::Complete
    );
    assert_eq!(
        outcome.completion.old_assignment_id,
        old_assignment.assignment_id
    );
    let successor_assignment_id = outcome.completion.new_assignment_id;

    // --- DIST-TASK-004 evidence: new OPEN registry generation with a
    // fresh root, chained to the frozen root; never an in-place unfreeze.
    assert_eq!(outcome.reopen.old_registry_binding, old_binding);
    assert_eq!(
        outcome.reopen.successor_registry_binding.generation,
        old_binding.generation + 1
    );
    assert_ne!(
        outcome.reopen.successor_registry_binding.root,
        old_binding.root
    );
    assert_eq!(outcome.reopen.fenced_assignment_id, successor_assignment_id);
    assert_eq!(
        outcome.reopened_registry.state,
        ParticipantRegistryState::Open
    );
    assert_eq!(
        outcome.reopened_registry.prior_root, old_binding.root,
        "the successor generation chains to the frozen root"
    );
    assert_eq!(
        outcome.reopened_registry.participants.len(),
        outcome.frozen_registry.participants.len(),
        "the frozen participant tuples carry into the successor generation"
    );
    for participant in &outcome.frozen_registry.participants {
        assert!(
            outcome.reopened_registry.participants.contains(participant),
            "frozen participant {participant:?} must survive the reopen"
        );
    }
    assert_eq!(
        outcome.active_assignment.assignment_id,
        outcome.reopen.active_assignment_id
    );
    assert_eq!(
        outcome.active_assignment.state,
        AuthorityAssignmentState::Active
    );
    assert_eq!(
        outcome.active_assignment.participant_registry_binding,
        outcome.reopen.successor_registry_binding
    );
    assert_eq!(
        outcome.active_assignment.authority_lease_binding,
        outcome.successor_lease.binding()
    );

    // --- Durable raw-table evidence where no typed reader exists: lease
    // history (v27), fenced assignment states, receipt columns.
    let raw = Connection::open(&store_path).expect("open raw evidence reader");
    let mut history = Vec::new();
    {
        let mut statement = raw
            .prepare(
                "SELECT term, transition_kind FROM task_authority_lease_history
                 ORDER BY lease_epoch",
            )
            .expect("prepare lease history read");
        let rows = statement
            .query_map([], |row| {
                Ok((blob_u64(&row.get::<_, Vec<u8>>(0)?), row.get::<_, i64>(1)?))
            })
            .expect("read lease history rows");
        for row in rows {
            history.push(row.expect("lease history row"));
        }
    }
    assert_eq!(
        history,
        vec![(1, 1), (2, 3)],
        "durable lease history: term 1 acquired by the old Cell, term 2 taken over by the successor"
    );
    let assignment_state = |assignment_id: &[u8]| {
        raw.query_row(
            "SELECT assignment_state FROM task_authority_assignments WHERE assignment_id = ?1",
            [assignment_id],
            |row| row.get::<_, i64>(0),
        )
        .expect("read assignment state")
    };
    assert_eq!(
        assignment_state(old_assignment.assignment_id.as_bytes()),
        3,
        "the old assignment is durably Fenced"
    );
    assert_eq!(
        assignment_state(successor_assignment_id.as_bytes()),
        3,
        "the completion successor assignment is fenced by the registry rotation"
    );
    assert_eq!(
        assignment_state(outcome.reopen.active_assignment_id.as_bytes()),
        1,
        "the reopened active assignment is durably Active"
    );
    let assignment_count: i64 = raw
        .query_row(
            "SELECT COUNT(*) FROM task_authority_assignments",
            [],
            |row| row.get(0),
        )
        .expect("count assignments");
    assert_eq!(assignment_count, 3);
    let (receipt_state, receipt_successor): (i64, Option<Vec<u8>>) = raw
        .query_row(
            "SELECT barrier_state, new_assignment_id FROM task_authority_takeover_receipts
             WHERE receipt_id = ?1",
            [outcome
                .takeover_receipt_pending
                .receipt_id
                .as_bytes()
                .as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read takeover receipt row");
    assert_eq!(receipt_state, 2, "the takeover receipt is durably Complete");
    assert_eq!(
        receipt_successor.as_deref(),
        Some(successor_assignment_id.as_bytes().as_slice())
    );
    {
        let mut statement = raw
            .prepare(
                "SELECT signer_principal_id, barrier_receipt_digest, fence_set_root
                 FROM task_authority_takeover_barrier_receipts
                 WHERE takeover_receipt_id = ?1",
            )
            .expect("prepare barrier receipt read");
        let rows = statement
            .query_map(
                [outcome
                    .takeover_receipt_pending
                    .receipt_id
                    .as_bytes()
                    .as_slice()],
                |row| {
                    Ok((
                        row.get::<_, Option<Vec<u8>>>(0)?,
                        row.get::<_, Option<Vec<u8>>>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                },
            )
            .expect("read barrier receipt rows");
        let mut observed = 0;
        for row in rows {
            let (signer_principal, digest, root) = row.expect("barrier receipt row");
            assert_eq!(
                signer_principal.as_deref(),
                Some(issuer.as_bytes().as_slice()),
                "the v36 signer column is durably filled"
            );
            assert!(digest.is_some(), "the v35 digest column is durably filled");
            assert_eq!(root.as_slice(), exact_fence_set_root);
            observed += 1;
        }
        assert_eq!(
            observed,
            outcome.fence_members.len(),
            "one durable observation row per manifest member"
        );
    }
    drop(raw);

    // --- The still-live old Cell proves it is fenced.
    fs::write(&verify, b"verify").expect("signal the fenced-verification stage");
    wait_while_alive(
        &mut child,
        "old Cell fenced receipts",
        &out_path,
        "fenced_ok",
    );
    let receipts = fs::read_to_string(&out_path).expect("read child receipts");
    assert!(receipts.contains("prepare_rejected:authority_lease_fenced"));
    assert!(receipts.contains("permit_rejected:authority_lease_fenced"));
    assert!(
        child
            .try_wait()
            .expect("child alive at fenced evidence")
            .is_none(),
        "both Cells must be live when the fencing evidence is cited"
    );

    fs::write(&release, b"release").expect("signal child release");
    let status = child.wait().expect("wait child after release");
    assert!(status.success(), "old Cell process failed: {status:?}");

    for path in [&out_path, &sign_request, &sign_response, &verify, &release] {
        let _ = fs::remove_file(path);
    }
}

/// Local helper keeping the frozen-binding comparison readable.
fn registry_binding_of(
    registry: &nlos_task::ParticipantRegistryRecord,
) -> nlos_task::ParticipantRegistryBinding {
    nlos_task::ParticipantRegistryBinding {
        generation: registry.generation,
        root: registry.root,
    }
}
