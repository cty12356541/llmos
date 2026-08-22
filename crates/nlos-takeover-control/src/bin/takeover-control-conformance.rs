//! Feature-gated cross-language `TakeoverControl` conformance server.
//!
//! The server creates one deterministic pending takeover fixture, prints the
//! typed observation fields as a bounded key/value manifest, and serves two
//! one-request IPC connections by default (or a bounded test-configured
//! count). TypeScript and Python clients construct the
//! protobuf request from that manifest, submit it over the platform adapter,
//! and submit the same request again to prove durable replay.

use std::error::Error;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use ed25519_dalek::{Signer, SigningKey};
use nlos_identity::{BootstrapPrincipalRequest, IdentityAuthority, KeyPurpose};
use nlos_ipc::{
    IpcError, OutboundResponse, PeerAuthorizer, PeerIdentity, TransportConfig, serve_one,
};
use nlos_schema::sabi::v1::ExchangeResponse;
use nlos_takeover_control::participant_type_code;
use nlos_task::{
    AttemptSpec, AuthorityLeaseFinalizeRequest, AuthorityLeasePermitRequest, AuthorityLeaseRequest,
    AuthorityLeaseTakeoverFenceRequest, AuthorityTakeoverBarrierCoverageState, FinalizeRequest,
    FinalizeRequestV3, ParticipantRecord, PermitDecision, PermitRequest, SnapshotBundle,
    SqliteTaskAuthority, TaskSpec, barrier_observation_signature_message,
    empty_effect_history_root,
};
use nlos_types::{
    CancellationScopeId, Generation, IdempotencyKey, ProcessId, ReceiptId, TaskAttemptId, TaskId,
    TaskSnapshotId,
};

const OBSERVED_AT_MS: i64 = 220;
const MONOTONIC_NOW_NS: u64 = 10;
const CAPABILITY_SLOT: u64 = 5;
const CAPABILITY_GENERATION: u64 = 1;
const HOLD_BEFORE_COMMIT_ENV: &str = "NLOS_TAKEOVER_CONTROL_HOLD_BEFORE_COMMIT";
const HOLD_AFTER_COMMIT_ENV: &str = "NLOS_TAKEOVER_CONTROL_HOLD_AFTER_COMMIT";
const CONNECTIONS_ENV: &str = "NLOS_TAKEOVER_CONTROL_CONNECTIONS";
const ROUNDS_ENV: &str = "NLOS_TAKEOVER_CONTROL_ROUNDS";
const TRUNCATE_WAL_AFTER_COMMIT_ENV: &str = "NLOS_TAKEOVER_CONTROL_TRUNCATE_WAL_AFTER_COMMIT";
const RANDOM_CRASH_PHASE_ENV: &str = "NLOS_TAKEOVER_CONTROL_RANDOM_CRASH_PHASE";
const RANDOM_CRASH_SEED_ENV: &str = "NLOS_TAKEOVER_CONTROL_RANDOM_CRASH_SEED";

struct AllowPeer;

impl PeerAuthorizer for AllowPeer {
    fn authorize(&self, _: &PeerIdentity) -> Result<(), String> {
        Ok(())
    }
}

static ALLOW_PEER: AllowPeer = AllowPeer;

struct CapabilityPolicy;

impl nlos_takeover_control::TakeoverControlAuthorizer for CapabilityPolicy {
    fn authorize_submit_barrier_observation(
        &self,
        context: &nlos_schema::sabi::v1::SabiRequestContext,
        _: &nlos_schema::sabi::v1::SubmitBarrierObservationRequest,
    ) -> Result<(), &'static str> {
        if context.capability_handles.as_slice()
            == [nlos_schema::sabi::v1::CapabilityHandle {
                slot: CAPABILITY_SLOT,
                generation: CAPABILITY_GENERATION,
            }]
        {
            Ok(())
        } else {
            Err("missing takeover control capability")
        }
    }
}

static CAPABILITY_POLICY: CapabilityPolicy = CapabilityPolicy;

struct Fixture {
    takeover_receipt_id: ReceiptId,
    participant: ParticipantRecord,
    remote_receipt_id: ReceiptId,
    barrier_digest: [u8; 32],
    signer_principal_id: [u8; 16],
    signer_control_domain_id: [u8; 16],
    signer_key_id: [u8; 16],
    signature: [u8; 64],
}

fn lease_request(holder: u8, key: u8, requested_at_ms: i64, ttl_ms: i64) -> AuthorityLeaseRequest {
    AuthorityLeaseRequest {
        holder_id: ProcessId::from_bytes([key.wrapping_add(holder); 16]),
        idempotency_key: IdempotencyKey::from_bytes([key; 16]),
        requested_at_ms,
        ttl_ms,
    }
}

fn fixture_attempt(task_id: TaskId) -> AttemptSpec {
    AttemptSpec {
        task_id,
        attempt_id: TaskAttemptId::from_bytes([0xA2; 16]),
        attempt_generation: Generation::INITIAL,
        snapshot: SnapshotBundle {
            snapshot_id: TaskSnapshotId::from_bytes([0xA3; 16]),
            snapshot_digest: [0xA4; 32],
            expected_head_commit_seq: 0,
            effect_history_root: empty_effect_history_root(),
            retry_fence_epoch: 0,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes([0xA5; 16]),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes([0xA6; 16]),
        registered_at_ms: 2,
    }
}

#[allow(clippy::too_many_lines)]
fn prepare_fixture(
    authority: &SqliteTaskAuthority,
    identity: &IdentityAuthority,
) -> Result<Fixture, Box<dyn Error>> {
    let task_id = TaskId::from_bytes([0xA1; 16]);
    authority.register_task(TaskSpec {
        task_id,
        task_generation: Generation::INITIAL,
        registered_at_ms: 1,
    })?;
    let attempt = fixture_attempt(task_id);
    authority.register_attempt(attempt)?;

    let first_lease = authority
        .acquire_authority_lease(lease_request(1, 0xB1, 100, 100))?
        .record();
    let permit =
        match authority.request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
            permit: PermitRequest {
                task_id,
                attempt_id: attempt.attempt_id,
                attempt_generation: attempt.attempt_generation,
                write_set_root: [0xA7; 32],
                planned_effects: Vec::new(),
                idempotency_key: IdempotencyKey::from_bytes([0xA8; 16]),
                valid_until_ms: 10_000,
                requested_at_ms: 150,
            },
            lease: first_lease,
        })? {
            PermitDecision::Issued(record) | PermitDecision::Replayed(record) => *record,
            other => return Err(format!("fixture permit was not issued: {other:?}").into()),
        };
    authority.finalize_commit_v3_with_authority_lease(AuthorityLeaseFinalizeRequest {
        finalize: FinalizeRequestV3 {
            base: FinalizeRequest {
                task_id,
                attempt_id: attempt.attempt_id,
                attempt_generation: attempt.attempt_generation,
                permit_id: permit.permit_id,
                new_effect_history_root: empty_effect_history_root(),
                new_retry_fence_epoch: 0,
                finalized_at_ms: 160,
            },
            required_satisfaction: Vec::new(),
            fenced_participant_digest: [0; 32],
        },
        lease: first_lease,
    })?;

    let takeover_lease = authority
        .acquire_authority_lease(lease_request(2, 0xB2, 201, 1_000))?
        .record();
    let registry_binding = permit
        .participant_registry_binding
        .ok_or("permit did not retain a participant registry binding")?;
    let frozen =
        authority.prepare_authority_takeover_fence(AuthorityLeaseTakeoverFenceRequest {
            task_id,
            expected_registry_binding: registry_binding,
            lease: takeover_lease,
            requested_at_ms: 210,
        })?;
    let fence_receipt =
        authority.inspect_authority_takeover_fence_receipt(task_id, registry_binding)?;
    let takeover_receipt =
        authority.inspect_authority_takeover_receipt(task_id, fence_receipt.receipt_id)?;
    let participant = frozen
        .participants
        .first()
        .copied()
        .ok_or("takeover fixture has no participant")?;
    let fence_root = takeover_receipt
        .exact_fence_set_root
        .ok_or("takeover fixture lacks exact fence root")?;

    let key = SigningKey::from_bytes(&[0xB3; 32]);
    let identity_binding = identity
        .bootstrap_principal(BootstrapPrincipalRequest {
            principal_profile_digest: [0xB4; 32],
            control_domain_policy_digest: [0xB5; 32],
            public_key: key.verifying_key().to_bytes(),
            key_purpose: KeyPurpose::BarrierObservationSigning,
            key_valid_from_ms: 0,
            key_valid_until_ms: 10_000,
            idempotency_key: IdempotencyKey::from_bytes([0xB6; 16]),
            created_at_ms: 0,
        })?
        .binding();
    let remote_receipt_id = ReceiptId::from_bytes([0xB7; 16]);
    let barrier_digest = [0xB8; 32];
    let message = barrier_observation_signature_message(
        takeover_receipt.receipt_id,
        &participant,
        remote_receipt_id,
        barrier_digest,
        fence_root,
    );
    Ok(Fixture {
        takeover_receipt_id: takeover_receipt.receipt_id,
        participant,
        remote_receipt_id,
        barrier_digest,
        signer_principal_id: identity_binding.principal_id.into_bytes(),
        signer_control_domain_id: identity_binding.control_domain_id.into_bytes(),
        signer_key_id: identity_binding.key_id.into_bytes(),
        signature: key.sign(&message).to_bytes(),
    })
}

fn announce_ready() -> io::Result<()> {
    println!("READY");
    io::stdout().flush()
}

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        write!(&mut output, "{byte:02x}").expect("writing to a String cannot fail");
    }
    output
}

fn announce_fixture(fixture: &Fixture) {
    println!(
        "FIXTURE takeover_receipt_id={} participant_type={} participant_id={} participant_generation={} admission_receipt_id={} remote_receipt_id={} barrier_digest={} signer_principal_id={} signer_control_domain_id={} signer_key_id={} signature={} observed_at_ms={}",
        hex(fixture.takeover_receipt_id.as_bytes()),
        participant_type_code(fixture.participant.participant_type),
        hex(fixture.participant.participant_id.as_bytes()),
        fixture.participant.participant_generation.get(),
        hex(fixture.participant.admission_receipt_id.as_bytes()),
        hex(fixture.remote_receipt_id.as_bytes()),
        hex(&fixture.barrier_digest),
        hex(&fixture.signer_principal_id),
        hex(&fixture.signer_control_domain_id),
        hex(&fixture.signer_key_id),
        hex(&fixture.signature),
        OBSERVED_AT_MS,
    );
    io::stdout().flush().expect("flush fixture manifest");
}

fn announce_commit_ready() {
    println!("COMMIT_READY");
    io::stdout().flush().expect("flush commit marker");
}

fn announce_pre_commit_ready() {
    println!("PRE_COMMIT_READY");
    io::stdout().flush().expect("flush pre-commit marker");
}

fn announce_wal_torn_ready() {
    println!("WAL_TORN_READY");
    io::stdout().flush().expect("flush torn WAL marker");
}

#[derive(Clone, Copy)]
enum RandomCrashPhase {
    BeforeCommit,
    AfterCommit,
}

impl RandomCrashPhase {
    fn parse(value: &str) -> Result<Self, Box<dyn Error>> {
        match value {
            "before" => Ok(Self::BeforeCommit),
            "after" => Ok(Self::AfterCommit),
            other => Err(format!(
                "{RANDOM_CRASH_PHASE_ENV} must be `before` or `after`, got {other}"
            )
            .into()),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::BeforeCommit => "before",
            Self::AfterCommit => "after",
        }
    }
}

#[derive(Clone, Copy)]
struct RandomCrashInjection {
    phase: RandomCrashPhase,
    seed: u64,
    delay_ms: u64,
    kill_after_ms: u64,
}

impl RandomCrashInjection {
    fn applies_before_commit(self) -> bool {
        matches!(self.phase, RandomCrashPhase::BeforeCommit)
    }

    fn applies_after_commit(self) -> bool {
        matches!(self.phase, RandomCrashPhase::AfterCommit)
    }
}

/// Deterministically derive a short delay from a test seed. This is deliberately
/// a tiny test-server-only PRNG, not a production fault model: the client kills
/// the process during the first third of the announced delay so the phase is
/// stable while the delay points vary across the seed sequence.
fn random_crash_injection() -> Result<Option<RandomCrashInjection>, Box<dyn Error>> {
    let Some(raw_phase) = std::env::var_os(RANDOM_CRASH_PHASE_ENV) else {
        return Ok(None);
    };
    let phase = RandomCrashPhase::parse(&raw_phase.to_string_lossy())?;
    let seed = std::env::var(RANDOM_CRASH_SEED_ENV)
        .map_err(|_| format!("{RANDOM_CRASH_SEED_ENV} is required with {RANDOM_CRASH_PHASE_ENV}"))?
        .parse::<u64>()?;
    let mixed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
    let delay_ms = 40 + mixed % 121;
    Ok(Some(RandomCrashInjection {
        phase,
        seed,
        delay_ms,
        kill_after_ms: delay_ms / 3,
    }))
}

fn announce_random_crash_ready(injection: RandomCrashInjection) {
    println!(
        "RANDOM_CRASH_READY phase={} seed={} delay_ms={} kill_after_ms={}",
        injection.phase.label(),
        injection.seed,
        injection.delay_ms,
        injection.kill_after_ms,
    );
    io::stdout().flush().expect("flush random crash marker");
}

#[derive(Clone, Copy)]
struct CrashInjection {
    hold_before_commit: bool,
    hold_after_commit: bool,
    truncate_wal_after_commit: bool,
}

async fn serve_exchange<S>(
    stream: S,
    peer: PeerIdentity,
    authority: Arc<SqliteTaskAuthority>,
    identity: Arc<IdentityAuthority>,
    crash_injection: CrashInjection,
    random_crash: Option<RandomCrashInjection>,
    authority_path: PathBuf,
) -> Result<(), IpcError>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    serve_one(
        stream,
        TransportConfig::default(),
        peer,
        &ALLOW_PEER,
        move |validated| async move {
            if let Some(injection) = random_crash.filter(|item| item.applies_before_commit()) {
                announce_random_crash_ready(injection);
                tokio::time::sleep(Duration::from_millis(injection.delay_ms)).await;
            }
            if crash_injection.hold_before_commit {
                announce_pre_commit_ready();
                std::future::pending::<()>().await;
                unreachable!("pre-commit conformance hook must remain suspended");
            }
            let (response, should_hold) = match nlos_takeover_control::TakeoverControl::new(
                authority.as_ref(),
                identity.as_ref(),
                &CAPABILITY_POLICY,
            )
            .handle(validated.envelope(), MONOTONIC_NOW_NS, OBSERVED_AT_MS)
            {
                Ok(envelope) => (envelope, crash_injection.hold_after_commit),
                Err(error) => (
                    nlos_takeover_control::failure_envelope(validated.envelope(), &error),
                    false,
                ),
            };
            if let Some(injection) = random_crash.filter(|item| item.applies_after_commit()) {
                announce_random_crash_ready(injection);
                tokio::time::sleep(Duration::from_millis(injection.delay_ms)).await;
                std::future::pending::<()>().await;
            }
            if should_hold {
                announce_commit_ready();
                if crash_injection.truncate_wal_after_commit {
                    truncate_wal_inside_last_commit(&authority_path)
                        .expect("truncate WAL after committed IPC observation");
                    announce_wal_torn_ready();
                }
                std::future::pending::<()>().await;
            }
            Ok(OutboundResponse::Typed(ExchangeResponse {
                envelope: Some(response),
            }))
        },
    )
    .await
}

fn hold_after_commit_enabled() -> bool {
    std::env::var_os(HOLD_AFTER_COMMIT_ENV).is_some()
}

fn hold_before_commit_enabled() -> bool {
    std::env::var_os(HOLD_BEFORE_COMMIT_ENV).is_some()
}

fn truncate_wal_after_commit_enabled() -> bool {
    std::env::var_os(TRUNCATE_WAL_AFTER_COMMIT_ENV).is_some()
}

/// `SQLite` WAL layout: 32-byte header, then frames of 24-byte header + page.
/// Truncate the last commit frame halfway so recovery discards that final
/// transaction. This hook is only reachable from the feature-gated test
/// server and models an externally torn WAL tail after the IPC handler has
/// already reported its durable commit internally.
fn truncate_wal_inside_last_commit(path: &Path) -> io::Result<()> {
    let mut wal_path = path.as_os_str().to_os_string();
    wal_path.push("-wal");
    let wal_path = PathBuf::from(wal_path);
    let mut wal = std::fs::read(&wal_path)?;
    if wal.len() < 32 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "WAL is shorter than its header",
        ));
    }
    let page_size = u32::from_be_bytes(
        wal[8..12]
            .try_into()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "missing WAL page size"))?,
    );
    let page_size = if page_size == 1 {
        65_536
    } else {
        usize::try_from(page_size)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid WAL page size"))?
    };
    if page_size < 512 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "WAL page size is below SQLite minimum",
        ));
    }
    let frame_size = 24 + page_size;
    let frame_count = (wal.len() - 32) / frame_size;
    let last_commit = (0..frame_count)
        .rfind(|index| {
            let start = 32 + index * frame_size;
            let commit = [
                wal[start + 8],
                wal[start + 9],
                wal[start + 10],
                wal[start + 11],
            ];
            u32::from_be_bytes(commit) != 0
        })
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "WAL has no commit frame"))?;
    let cut = 32 + last_commit * frame_size + frame_size / 2;
    wal.truncate(cut);
    std::fs::write(wal_path, wal)
}

fn connection_count() -> Result<usize, Box<dyn Error>> {
    let count = std::env::var(CONNECTIONS_ENV)
        .ok()
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(2);
    if !(2..=32).contains(&count) {
        return Err(format!("{CONNECTIONS_ENV} must be within 2..=32, got {count}").into());
    }
    Ok(count)
}

fn round_count() -> Result<usize, Box<dyn Error>> {
    let count = std::env::var(ROUNDS_ENV)
        .ok()
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(1);
    if !(1..=8).contains(&count) {
        return Err(format!("{ROUNDS_ENV} must be within 1..=8, got {count}").into());
    }
    Ok(count)
}

/*
 * Keep the response construction above deliberately inside the handler future:
 * the marker is emitted only after `TakeoverControl::handle` has returned a
 * successful envelope, which means the store transaction has committed. The
 * feature-gated conformance process then waits forever until the client kills
 * it, modelling a process crash between durable commit and response delivery.
 */

fn endpoint_path(value: OsString) -> PathBuf {
    PathBuf::from(value)
}

#[cfg(unix)]
async fn run(
    endpoint: OsString,
    authority_path: OsString,
    identity_path: OsString,
) -> Result<(), Box<dyn Error>> {
    use nlos_ipc::unix::UnixListenerAdapter;
    use std::fs;

    struct EndpointGuard(PathBuf);

    impl Drop for EndpointGuard {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.0);
        }
    }

    let endpoint = endpoint_path(endpoint);
    let authority_path = endpoint_path(authority_path);
    let authority = Arc::new(SqliteTaskAuthority::open(&authority_path)?);
    let identity = Arc::new(IdentityAuthority::open(endpoint_path(identity_path))?);
    let fixture = prepare_fixture(authority.as_ref(), identity.as_ref())?;
    let listener = UnixListenerAdapter::bind(&endpoint)?;
    let _guard = EndpointGuard(endpoint);
    let hold_after_commit = hold_after_commit_enabled();
    let crash_injection = CrashInjection {
        hold_before_commit: hold_before_commit_enabled(),
        hold_after_commit,
        truncate_wal_after_commit: hold_after_commit && truncate_wal_after_commit_enabled(),
    };
    let random_crash = random_crash_injection()?;
    let connection_count = connection_count()?;
    let round_count = round_count()?;
    announce_ready()?;
    announce_fixture(&fixture);
    for _ in 0..round_count {
        let mut handlers = tokio::task::JoinSet::new();
        for _ in 0..connection_count {
            let (stream, peer) = listener.accept(TransportConfig::default()).await?;
            handlers.spawn(serve_exchange(
                stream,
                peer,
                Arc::clone(&authority),
                Arc::clone(&identity),
                crash_injection,
                random_crash,
                authority_path.clone(),
            ));
        }
        while let Some(result) = handlers.join_next().await {
            result.map_err(|error| format!("TakeoverControl handler task panicked: {error}"))??;
        }
    }
    let rows =
        authority.inspect_authority_takeover_barrier_receipts(fixture.takeover_receipt_id)?;
    if rows.len() != 1 {
        return Err(format!("expected one durable observation, got {}", rows.len()).into());
    }
    if authority
        .inspect_authority_takeover_barrier_coverage(fixture.takeover_receipt_id)?
        .state
        != AuthorityTakeoverBarrierCoverageState::LocallyCovered
    {
        return Err("TakeoverControl conformance did not reach LocallyCovered".into());
    }
    Ok(())
}

#[cfg(windows)]
async fn run(
    endpoint: OsString,
    authority_path: OsString,
    identity_path: OsString,
) -> Result<(), Box<dyn Error>> {
    use nlos_ipc::windows::NamedPipeListenerAdapter;

    let authority_path = endpoint_path(authority_path);
    let authority = Arc::new(SqliteTaskAuthority::open(&authority_path)?);
    let identity = Arc::new(IdentityAuthority::open(endpoint_path(identity_path))?);
    let fixture = prepare_fixture(authority.as_ref(), identity.as_ref())?;
    let connection_count = connection_count()?;
    // `accept` creates the next pipe instance before returning the current
    // one, so retain one spare instance while all requested handlers run.
    let mut listener =
        NamedPipeListenerAdapter::bind(endpoint, connection_count + 1, TransportConfig::default())?;
    let hold_after_commit = hold_after_commit_enabled();
    let truncate_wal_after_commit = hold_after_commit && truncate_wal_after_commit_enabled();
    let round_count = round_count()?;
    let crash_injection = CrashInjection {
        hold_before_commit: hold_before_commit_enabled(),
        hold_after_commit,
        truncate_wal_after_commit,
    };
    let random_crash = random_crash_injection()?;
    announce_ready()?;
    announce_fixture(&fixture);
    for _ in 0..round_count {
        let mut handlers = tokio::task::JoinSet::new();
        for _ in 0..connection_count {
            let (stream, peer) = listener.accept(TransportConfig::default()).await?;
            handlers.spawn(serve_exchange(
                stream,
                peer,
                Arc::clone(&authority),
                Arc::clone(&identity),
                crash_injection,
                random_crash,
                authority_path.clone(),
            ));
        }
        while let Some(result) = handlers.join_next().await {
            result.map_err(|error| format!("TakeoverControl handler task panicked: {error}"))??;
        }
    }
    let rows =
        authority.inspect_authority_takeover_barrier_receipts(fixture.takeover_receipt_id)?;
    if rows.len() != 1 {
        return Err(format!("expected one durable observation, got {}", rows.len()).into());
    }
    if authority
        .inspect_authority_takeover_barrier_coverage(fixture.takeover_receipt_id)?
        .state
        != AuthorityTakeoverBarrierCoverageState::LocallyCovered
    {
        return Err("TakeoverControl conformance did not reach LocallyCovered".into());
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = std::env::args_os().skip(1);
    let endpoint = arguments.next().ok_or("missing IPC endpoint")?;
    let authority_path = arguments.next().ok_or("missing TaskAuthority path")?;
    let identity_path = arguments.next().ok_or("missing IdentityAuthority path")?;
    if arguments.next().is_some() {
        return Err("unexpected extra argument".into());
    }
    run(endpoint, authority_path, identity_path).await
}
