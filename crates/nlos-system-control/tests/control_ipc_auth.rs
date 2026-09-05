#![cfg(unix)]
#![allow(deprecated)] // Ladder constructors deprecated in favor of the *_with_authorities_struct entries.
//! B-CONTROL-002 integration evidence: the ADR-0011 authenticated control
//! service path over a real Unix socket, a real `IdentityAuthority`
//! (genuine Ed25519 verification), and a real durable `AuthorityClock`
//! (W2-E), plus the fail-closed negative matrix (bad signature, replayed
//! nonce, unknown principal, channel-binding drift, validity judged at the
//! clock reading, unbounded correlation).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer, SigningKey};
use nlos_clock::{AuthorityClock, NowRequest, WallNowDecision, WallReading, WallSource};
use nlos_identity::{
    BootstrapDecision, BootstrapPrincipalRequest, IdentityAuthority, IdentityBinding, KeyPurpose,
};
use nlos_ipc::handshake::transport::{
    AuthenticatedServeOutcome, ServerHandshakeContext, authenticated_connect,
    endpoint_channel_binding,
};
use nlos_ipc::handshake::{
    HandshakeError, client_attestation, decode_challenge_wire, encode_attestation_wire,
    principal_handshake_message,
};
use nlos_ipc::unix::{UnixListenerAdapter, connect};
use nlos_ipc::{
    FramedIo, IoOperation, IpcError, LocalRpcClient, PeerAuthorizer, PeerIdentity, TransportConfig,
};
use nlos_schema::SABI_ENVELOPE_SCHEMA;
use nlos_schema::sabi::v1::{
    Envelope, ExchangeRequest, GetSystemControlRequest, SabiErrorCode, SabiRequestContext,
    SchemaIdentity,
};
use nlos_system_control::auth::{
    authenticated_serve_one_control, command_wall_key, dispatch_over_authenticated_socket,
};
use nlos_system_control::control::{ControlCommand, ControlOutcome, RecoveryWorkerLifecycle};
use nlos_system_control::{
    RecoveryHealthSource, RecoverySystemControl, SYSTEM_CONTROL_SERVICE, SystemControlAuthorizer,
};
use nlos_task::{
    ArtifactCommitPlanId, ArtifactPublicationExpectation, ArtifactRecoveryFailureRequest,
    ArtifactRecoveryFailureSource, AttemptSpec, PermitDecision, PermitRequest,
    PlanArtifactCommitRequest, SnapshotBundle, SqliteTaskAuthority, artifact_publication_plan_root,
    empty_effect_history_root,
};
use nlos_types::{
    ArtifactId, CancellationScopeId, Generation, IdempotencyKey, PrincipalId, TaskAttemptId,
    TaskId, TaskSnapshotId,
};
use tokio::task::JoinHandle;

const NONCE: [u8; 32] = [0x5D; 32];
const MONOTONIC_NOW_NS: u64 = 10;
const CLOCK_WALL_MS: u64 = 42_000;
const ACK_COMMAND_ID: [u8; 16] = [0x51; 16];
const ACK_REASON: &str = "authenticated acknowledgement evidence";

/// First nonce a spawned server issues: the sequence byte replaces
/// `NONCE[0]`, so the first connection sees `[1, ...]`.
fn first_issued_nonce() -> [u8; 32] {
    let mut issued = NONCE;
    issued[0] = 1;
    issued
}

struct TempRoot(PathBuf);

static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

impl TempRoot {
    fn new(label: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "nlos-sc-auth-{label}-{}-{}-{nanos}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed),
        )))
    }

    fn identity_root(&self) -> PathBuf {
        self.0.join("identity")
    }

    fn clock_root(&self) -> PathBuf {
        self.0.join("clock")
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Short socket path: macOS `SUN_LEN` caps socket paths at 104 bytes.
struct SocketPath(PathBuf);

static NEXT_SOCKET: AtomicU64 = AtomicU64::new(0);

impl SocketPath {
    fn new(label: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "nlos-sc-auth-sock-{label}-{}-{}-{nanos}",
            std::process::id(),
            NEXT_SOCKET.fetch_add(1, Ordering::Relaxed),
        )))
    }
}

impl std::ops::Deref for SocketPath {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for SocketPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for SocketPath {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Deterministic durable wall source: the clock facility is real (`SQLite`
/// WAL/FULL high-water with replay receipts); only the wall source is
/// pinned so the judged validity instant is assertable.
struct FixedWall(u64);

impl WallSource for FixedWall {
    fn now_ms(&self) -> Result<u64, nlos_clock::AuthorityClockError> {
        Ok(self.0)
    }
}

struct TestDatabase {
    path: PathBuf,
}

impl TestDatabase {
    fn new(label: &str) -> Self {
        Self {
            path: std::env::temp_dir().join(format!(
                "nlos-sc-auth-db-{label}-{}-{}.sqlite3",
                std::process::id(),
                NEXT_ROOT.fetch_add(1, Ordering::Relaxed),
            )),
        }
    }

    fn open(&self) -> SqliteTaskAuthority {
        SqliteTaskAuthority::open(&self.path).unwrap()
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let mut value = self.path.clone().into_os_string();
            value.push(suffix);
            match fs::remove_file(PathBuf::from(value)) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("remove test database: {error}"),
            }
        }
    }
}

struct AllowPeer;

impl PeerAuthorizer for AllowPeer {
    fn authorize(&self, _: &PeerIdentity) -> Result<(), String> {
        Ok(())
    }
}

struct CapabilityPolicy;

impl SystemControlAuthorizer for CapabilityPolicy {
    fn authorize_get(
        &self,
        context: &SabiRequestContext,
        _: &GetSystemControlRequest,
    ) -> Result<(), &'static str> {
        authorize(context)
    }

    fn authorize_submit(
        &self,
        context: &SabiRequestContext,
        command: &nlos_schema::sabi::v1::ControlCommand,
    ) -> Result<(), &'static str> {
        authorize(context)?;
        if command.reason.starts_with("denied") {
            Err("policy denied this acknowledgement")
        } else {
            Ok(())
        }
    }
}

fn authorize(context: &SabiRequestContext) -> Result<(), &'static str> {
    let expected = nlos_schema::sabi::v1::CapabilityHandle {
        slot: nlos_system_control::control::CONTROL_CAPABILITY_SLOT,
        generation: nlos_system_control::control::CONTROL_CAPABILITY_GENERATION,
    };
    if context.capability_handles.as_slice() == [expected] {
        Ok(())
    } else {
        Err("missing recovery operations capability")
    }
}

#[derive(Clone)]
struct StubHealth(nlos_commit_coordinator::RecoveryWorkerHealth);

impl RecoveryHealthSource for StubHealth {
    fn recovery_health(&self) -> nlos_commit_coordinator::RecoveryWorkerHealth {
        self.0.clone()
    }
}

fn stub_health(plan_id: &ArtifactCommitPlanId) -> StubHealth {
    StubHealth(nlos_commit_coordinator::RecoveryWorkerHealth {
        state: nlos_commit_coordinator::RecoveryWorkerState::BackingOff,
        completed_cycles: 4,
        total_inspected: 3,
        total_finalized: 2,
        consecutive_failed_cycles: 0,
        retry_delay: Some(Duration::from_millis(250)),
        last_failures: vec![nlos_commit_coordinator::RecoveryWorkerFailure {
            plan_id: Some(*plan_id),
            authority: nlos_commit_coordinator::RecoveryFailureAuthority::Artifact,
            message: "secret local database path must not cross IPC".to_owned(),
        }],
        durable_retrying: 0,
        durable_escalated: 1,
        durable_unacknowledged_escalated: 1,
        durable_resolved: 0,
    })
}

fn create_escalated_plan(authority: &SqliteTaskAuthority) -> ArtifactCommitPlanId {
    let task_id = TaskId::from_bytes([0x11; 16]);
    authority
        .register_task(nlos_task::TaskSpec {
            task_id,
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
        })
        .unwrap();
    let attempt = AttemptSpec {
        task_id,
        attempt_id: TaskAttemptId::from_bytes([0x12; 16]),
        attempt_generation: Generation::INITIAL,
        snapshot: SnapshotBundle {
            snapshot_id: TaskSnapshotId::from_bytes([0x13; 16]),
            snapshot_digest: [0x14; 32],
            expected_head_commit_seq: 0,
            effect_history_root: empty_effect_history_root(),
            retry_fence_epoch: 0,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes([0x15; 16]),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes([0x16; 16]),
        registered_at_ms: 2_000,
    };
    authority.register_attempt(attempt).unwrap();
    let expectation = ArtifactPublicationExpectation {
        staging_id: [0x21; 16],
        artifact_id: ArtifactId::from_bytes([0x22; 16]),
        target_revision: 1,
        digest: [0x23; 32],
        size_bytes: 10,
    };
    let PermitDecision::Issued(permit) = authority
        .request_commit_permit(PermitRequest {
            task_id,
            attempt_id: attempt.attempt_id,
            attempt_generation: attempt.attempt_generation,
            write_set_root: artifact_publication_plan_root(std::slice::from_ref(&expectation))
                .unwrap(),
            planned_effects: Vec::new(),
            idempotency_key: IdempotencyKey::from_bytes([0x17; 16]),
            valid_until_ms: 20_000,
            requested_at_ms: 3_000,
        })
        .unwrap()
    else {
        panic!("expected permit");
    };
    let plan = authority
        .plan_artifact_commit(PlanArtifactCommitRequest {
            task_id,
            attempt_id: attempt.attempt_id,
            attempt_generation: attempt.attempt_generation,
            permit_id: permit.permit_id,
            idempotency_key: IdempotencyKey::from_bytes([0x18; 16]),
            expectations: vec![expectation],
            planned_at_ms: 4_000,
        })
        .unwrap()
        .record()
        .clone();
    authority
        .record_artifact_recovery_failure(ArtifactRecoveryFailureRequest {
            plan_id: plan.plan_id,
            expected_total_failures: 0,
            source: ArtifactRecoveryFailureSource::ArtifactAuthority,
            observed_at_ms: 5_000,
            base_delay_ms: 100,
            max_delay_ms: 1_000,
            escalation_threshold: 1,
        })
        .unwrap();
    plan.plan_id
}

/// Bootstraps one real principal with a genuine Ed25519 keypair. The binding
/// validity window must cover the clock wall reading the server will judge
/// at.
fn bootstrap(
    root: &Path,
    seed: u8,
    key_valid_until_ms: u64,
) -> (IdentityAuthority, SigningKey, IdentityBinding) {
    let identity = IdentityAuthority::open(root).unwrap();
    let key = SigningKey::from_bytes(&[seed; 32]);
    let BootstrapDecision::Created(binding) = identity
        .bootstrap_principal(BootstrapPrincipalRequest {
            principal_profile_digest: [seed.wrapping_add(1); 32],
            control_domain_policy_digest: [seed.wrapping_add(2); 32],
            public_key: key.verifying_key().to_bytes(),
            key_purpose: KeyPurpose::SemanticSigning,
            key_valid_from_ms: 0,
            key_valid_until_ms,
            idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(3); 16]),
            created_at_ms: 0,
        })
        .unwrap()
    else {
        unreachable!("fresh authority bootstraps a new principal");
    };
    (identity, key, binding)
}

type ServeOutcomes = Vec<Result<AuthenticatedServeOutcome, HandshakeError>>;

/// Owns every authority of one authenticated control service fixture and
/// spawns a task serving exactly `connections` exchanges.
struct Fixture {
    root: TempRoot,
    tasks: Arc<SqliteTaskAuthority>,
    identity: Arc<IdentityAuthority>,
    clock: Arc<AuthorityClock>,
    handshake: Arc<ServerHandshakeContext>,
    listener: Option<UnixListenerAdapter>,
    health: StubHealth,
    socket: SocketPath,
    plan_id: ArtifactCommitPlanId,
}

impl Fixture {
    fn new(label: &str, seed: u8, key_valid_until_ms: u64) -> (Self, SigningKey, IdentityBinding) {
        let root = TempRoot::new(label);
        let socket = SocketPath::new(label);
        let (identity, key, binding) = bootstrap(&root.identity_root(), seed, key_valid_until_ms);
        let database = TestDatabase::new(label);
        let tasks = Arc::new(database.open());
        let plan_id = create_escalated_plan(tasks.as_ref());
        let clock = Arc::new(
            AuthorityClock::open_with_wall_source(root.clock_root(), FixedWall(CLOCK_WALL_MS))
                .unwrap(),
        );
        let handshake = Arc::new(ServerHandshakeContext::new(&socket, 8).unwrap());
        let listener = UnixListenerAdapter::bind(&socket).unwrap();
        (
            Self {
                root,
                tasks,
                identity: Arc::new(identity),
                clock,
                handshake,
                listener: Some(listener),
                health: stub_health(&plan_id),
                socket,
                plan_id,
            },
            key,
            binding,
        )
    }

    /// Points the server-side binding at a foreign endpoint while the
    /// listener stays on the real socket: honest clients then fail the
    /// channel-binding pin.
    fn with_drifted_binding(mut self) -> Self {
        self.handshake = Arc::new(
            ServerHandshakeContext::new(Path::new("/llmos-auth-test/other.sock"), 8).unwrap(),
        );
        self
    }

    fn spawn_serving_n(&mut self, connections: usize) -> JoinHandle<ServeOutcomes> {
        let listener = self.listener.take().unwrap();
        let tasks = Arc::clone(&self.tasks);
        let identity = Arc::clone(&self.identity);
        let clock = Arc::clone(&self.clock);
        let handshake = Arc::clone(&self.handshake);
        let health = self.health.clone();
        tokio::spawn(async move {
            let sequence = AtomicU8::new(0);
            let mut outcomes = Vec::new();
            for _ in 0..connections {
                let next_nonce = || {
                    let mut issued = NONCE;
                    issued[0] = sequence.fetch_add(1, Ordering::Relaxed) + 1;
                    issued
                };
                let control =
                    RecoverySystemControl::new(tasks.as_ref(), &health, &CapabilityPolicy);
                outcomes.push(
                    authenticated_serve_one_control(
                        &listener,
                        TransportConfig::default(),
                        &control,
                        identity.as_ref(),
                        clock.as_ref(),
                        handshake.as_ref(),
                        &AllowPeer,
                        MONOTONIC_NOW_NS,
                        next_nonce,
                    )
                    .await,
                );
            }
            outcomes
        })
    }
}

fn acknowledge_command(plan_id: &ArtifactCommitPlanId) -> ControlCommand {
    ControlCommand::AcknowledgeRecoveryAlert {
        control_command_id: ACK_COMMAND_ID,
        plan_id: *plan_id.as_bytes(),
        expected_total_failures: 1,
        reason: ACK_REASON.to_owned(),
    }
}

fn honest_signer(key: &SigningKey) -> impl Fn(&[u8; 32]) -> Result<[u8; 64], HandshakeError> + '_ {
    |digest: &[u8; 32]| Ok(key.sign(digest).to_bytes())
}

#[test]
fn command_wall_key_is_deterministic_request_correlation() {
    assert_eq!(
        command_wall_key(&ACK_COMMAND_ID),
        command_wall_key(&ACK_COMMAND_ID)
    );
    assert_ne!(
        command_wall_key(&ACK_COMMAND_ID),
        command_wall_key(&[0x52; 16])
    );
}

#[tokio::test]
async fn authenticated_roundtrip_inspect_and_acknowledge() {
    let (mut fixture, key, binding) = Fixture::new("round", 0x51, 1_000_000);
    let principal = binding.principal_id;
    let signer = honest_signer(&key);
    let server = fixture.spawn_serving_n(2);

    let inspect = dispatch_over_authenticated_socket(
        &fixture.socket,
        principal,
        &signer,
        &ControlCommand::InspectHealth,
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(inspect.control_command_id, [0xC0; 16]);
    let ControlOutcome::Inspected(inspection) = inspect.outcome.as_ref().unwrap() else {
        panic!("expected inspection receipt");
    };
    assert_eq!(inspection.worker_state, RecoveryWorkerLifecycle::BackingOff);
    assert_eq!(inspection.durable_escalated, 1);
    assert_eq!(inspection.alerts.len(), 1);

    let acknowledge = dispatch_over_authenticated_socket(
        &fixture.socket,
        principal,
        &signer,
        &acknowledge_command(&fixture.plan_id),
        None,
        None,
    )
    .await
    .unwrap();
    let ControlOutcome::Acknowledged { receipt_id } = acknowledge.outcome.as_ref().unwrap() else {
        panic!("expected acknowledgement receipt");
    };
    assert_eq!(receipt_id.len(), 16);

    let outcomes = server.await.unwrap();
    assert!(outcomes[0].as_ref().unwrap().served().is_ok());
    let ack_outcome = outcomes[1].as_ref().unwrap();
    assert_eq!(ack_outcome.verified().principal_id(), principal);
    assert_eq!(ack_outcome.verified().key_id(), binding.key_id);
    assert_eq!(
        ack_outcome.verified().key_generation(),
        binding.key_generation
    );
    assert!(ack_outcome.served().is_ok());

    // The acknowledgement's wall time was issued by the fixture clock for
    // the command's correlation key.
    assert_eq!(
        fixture
            .clock
            .wall_now(NowRequest {
                idempotency_key: command_wall_key(&ACK_COMMAND_ID),
            })
            .unwrap(),
        WallNowDecision::Replayed(WallReading::from_u64(CLOCK_WALL_MS))
    );
    // The reading is durable: a fresh clock handle replays it unchanged.
    let reopened = AuthorityClock::open_with_wall_source(
        fixture.root.clock_root(),
        FixedWall(CLOCK_WALL_MS + 1_000),
    )
    .unwrap();
    assert_eq!(
        reopened
            .wall_now(NowRequest {
                idempotency_key: command_wall_key(&ACK_COMMAND_ID),
            })
            .unwrap(),
        WallNowDecision::Replayed(WallReading::from_u64(CLOCK_WALL_MS))
    );
    // The mutation reached the durable authority exactly once.
    assert!(
        fixture
            .tasks
            .list_artifact_recovery_alerts(8)
            .unwrap()
            .first()
            .unwrap()
            .acknowledgement
            .is_some()
    );
}

#[tokio::test]
async fn bad_signature_refuses_the_connection_without_serving() {
    let (mut fixture, _, binding) = Fixture::new("bad-sig", 0x61, 1_000_000);
    let handshake = Arc::clone(&fixture.handshake);
    let impostor = SigningKey::from_bytes(&[0x99; 32]);
    let server = fixture.spawn_serving_n(1);

    let mut framed = authenticated_connect(
        fixture.socket.as_ref(),
        TransportConfig::default(),
        binding.principal_id,
        |digest: &[u8; 32]| Ok(impostor.sign(digest).to_bytes()),
    )
    .await
    .unwrap();
    let client_error = framed.receive().await.unwrap_err();

    let outcomes = server.await.unwrap();
    assert!(
        matches!(&outcomes[0], Err(HandshakeError::SignatureInvalid)),
        "unexpected outcome: {:?}",
        outcomes[0]
    );
    assert!(matches!(
        client_error,
        IpcError::Io {
            operation: IoOperation::Read,
            ..
        }
    ));
    // The burned nonce is never returned to the registry.
    assert!(matches!(
        handshake.nonces().consume(&first_issued_nonce()),
        Err(HandshakeError::NonceRejected)
    ));
}

#[tokio::test]
async fn replayed_attestation_fails_the_second_connection() {
    let (mut fixture, key, binding) = Fixture::new("replay", 0x62, 1_000_000);
    let binding_bytes = endpoint_channel_binding(&fixture.socket);
    let server = fixture.spawn_serving_n(2);

    let captured_attestation = {
        let (stream, _peer) = connect(&fixture.socket, TransportConfig::default())
            .await
            .unwrap();
        let mut framed = FramedIo::new(stream, TransportConfig::default());
        let challenge = decode_challenge_wire(&framed.receive().await.unwrap()).unwrap();
        assert_eq!(challenge.nonce, first_issued_nonce().to_vec());
        let signature = key
            .sign(&principal_handshake_message(
                &first_issued_nonce(),
                binding.principal_id,
                &binding_bytes,
            ))
            .to_bytes();
        let attestation =
            client_attestation(binding.principal_id, &challenge, &binding_bytes, signature)
                .unwrap();
        let wire = encode_attestation_wire(&attestation).unwrap();
        framed.send(&wire).await.unwrap();
        wire
    };

    let second_error = {
        let (stream, _peer) = connect(&fixture.socket, TransportConfig::default())
            .await
            .unwrap();
        let mut framed = FramedIo::new(stream, TransportConfig::default());
        let challenge = decode_challenge_wire(&framed.receive().await.unwrap()).unwrap();
        assert_ne!(challenge.nonce, first_issued_nonce().to_vec());
        framed.send(&captured_attestation).await.unwrap();
        framed.receive().await.unwrap_err()
    };

    let mut outcomes = server.await.unwrap();
    assert!(outcomes[0].is_ok(), "first connection: {:?}", outcomes[0]);
    assert!(
        matches!(outcomes.remove(1), Err(HandshakeError::NonceRejected)),
        "replayed attestation must fail closed"
    );
    assert!(matches!(
        second_error,
        IpcError::Io {
            operation: IoOperation::Read,
            ..
        }
    ));
}

#[tokio::test]
async fn unknown_principal_fails_closed() {
    let (mut fixture, _, _) = Fixture::new("stranger", 0x63, 1_000_000);
    let stranger = PrincipalId::from_bytes([0xEE; 16]);
    let stranger_key = SigningKey::from_bytes(&[0x63; 32]);
    let server = fixture.spawn_serving_n(1);

    let _ = authenticated_connect(
        fixture.socket.as_ref(),
        TransportConfig::default(),
        stranger,
        |digest: &[u8; 32]| Ok(stranger_key.sign(digest).to_bytes()),
    )
    .await;

    let outcomes = server.await.unwrap();
    assert!(
        matches!(
            &outcomes[0],
            Err(HandshakeError::PrincipalUnknown(id)) if *id == stranger
        ),
        "unexpected outcome: {:?}",
        outcomes[0]
    );
}

#[tokio::test]
async fn channel_binding_mismatch_fails_closed_without_burning_the_nonce() {
    let (fixture, key, binding) = Fixture::new("binding", 0x64, 1_000_000);
    let mut fixture = fixture.with_drifted_binding();
    let handshake = Arc::clone(&fixture.handshake);
    let server = fixture.spawn_serving_n(1);

    let mut framed = authenticated_connect(
        fixture.socket.as_ref(),
        TransportConfig::default(),
        binding.principal_id,
        honest_signer(&key),
    )
    .await
    .unwrap();
    let client_error = framed.receive().await.unwrap_err();

    let outcomes = server.await.unwrap();
    assert!(
        matches!(&outcomes[0], Err(HandshakeError::ChannelBindingMismatch)),
        "unexpected outcome: {:?}",
        outcomes[0]
    );
    assert!(matches!(
        client_error,
        IpcError::Io {
            operation: IoOperation::Read,
            ..
        }
    ));
    // The mismatch is rejected before nonce consumption, so the honest
    // nonce is still valid and consumable afterwards.
    handshake.nonces().consume(&first_issued_nonce()).unwrap();
}

#[tokio::test]
async fn key_validity_is_judged_at_the_clock_wall_reading() {
    // The binding expires at 10_000 but the fixture clock reads 42_000: the
    // handshake must fail closed on the clock-anchored instant, proving the
    // verified-at time comes from the AuthorityClock (W2-E), not from any
    // caller-supplied wall time.
    let (mut fixture, key, binding) = Fixture::new("expired", 0x65, 10_000);
    let server = fixture.spawn_serving_n(1);

    let mut framed = authenticated_connect(
        fixture.socket.as_ref(),
        TransportConfig::default(),
        binding.principal_id,
        honest_signer(&key),
    )
    .await
    .unwrap();
    let _client_error = framed.receive().await;

    let outcomes = server.await.unwrap();
    assert!(
        matches!(&outcomes[0], Err(HandshakeError::KeyExpired)),
        "unexpected outcome: {:?}",
        outcomes[0]
    );
}

#[tokio::test]
async fn unbounded_correlation_is_a_typed_rejection_not_a_guessed_time() {
    let (mut fixture, key, binding) = Fixture::new("corr", 0x66, 1_000_000);
    let server = fixture.spawn_serving_n(1);

    let framed = authenticated_connect(
        fixture.socket.as_ref(),
        TransportConfig::default(),
        binding.principal_id,
        honest_signer(&key),
    )
    .await
    .unwrap();
    let request = Envelope {
        schema: Some(SchemaIdentity {
            name: SABI_ENVELOPE_SCHEMA.to_owned(),
            major: 1,
            minor: 1,
            critical_extension_ids: Vec::new(),
            non_critical_extension_ids: Vec::new(),
        }),
        request_id: vec![9; 16],
        service: SYSTEM_CONTROL_SERVICE.to_owned(),
        method: "get".to_owned(),
        common_context: None,
        payload: b"no correlation".to_vec(),
    };
    let response = LocalRpcClient::new(framed.into_inner(), TransportConfig::default())
        .exchange_validated(ExchangeRequest {
            envelope: Some(request),
        })
        .await
        .unwrap();
    let Some(nlos_schema::sabi::v1::SabiResponseContext { failure, .. }) = response
        .envelope()
        .common_context
        .as_ref()
        .and_then(|context| match context {
            nlos_schema::sabi::v1::envelope::CommonContext::ResponseContext(response) => {
                Some(response.clone())
            }
            nlos_schema::sabi::v1::envelope::CommonContext::RequestContext(_) => None,
        })
    else {
        panic!("expected a typed response context");
    };
    let failure = failure.expect("expected a typed failure");
    assert_eq!(failure.code, i32::from(SabiErrorCode::InvalidArgument));

    let outcomes = server.await.unwrap();
    assert!(outcomes[0].as_ref().unwrap().served().is_ok());
}

/// Denial reason must carry the fixture policy's `denied` prefix.
const PARITY_DENIED_REASON: &str = "denied: multi-entry typed-failure parity probe";
const PARITY_DENIED_COMMAND_ID: [u8; 16] = [0x7D; 16];

type PlainServeOutcomes = Vec<Result<(), IpcError>>;

/// Plain local-IPC entry: one dedicated socket over the same durable
/// authority and policy, serving exactly the `handle_for_ipc` projection
/// for three sequential exchanges.
fn spawn_plain_entry(
    listener: UnixListenerAdapter,
    tasks: Arc<SqliteTaskAuthority>,
    health: StubHealth,
) -> tokio::task::JoinHandle<Result<PlainServeOutcomes, IpcError>> {
    use nlos_ipc::{OutboundResponse, serve_one};
    use nlos_schema::sabi::v1::ExchangeResponse;

    tokio::spawn(async move {
        let wall_ms = i64::try_from(CLOCK_WALL_MS).unwrap();
        let mut served = Vec::new();
        for _ in 0..3 {
            let tasks = Arc::clone(&tasks);
            let health = health.clone();
            let (stream, peer) = listener.accept(TransportConfig::default()).await?;
            served.push(
                serve_one(
                    stream,
                    TransportConfig::default(),
                    peer,
                    &AllowPeer,
                    move |validated| {
                        let response =
                            RecoverySystemControl::new(tasks.as_ref(), &health, &CapabilityPolicy)
                                .handle_for_ipc(validated.envelope(), MONOTONIC_NOW_NS, wall_ms);
                        async move {
                            Ok(OutboundResponse::Typed(ExchangeResponse {
                                envelope: Some(response),
                            }))
                        }
                    },
                )
                .await,
            );
        }
        Ok(served)
    })
}

/// B-TASK-006L multi-entry parity: the same command dispatched through the
/// in-process handler, the plain local-IPC entry, and the ADR-0011
/// authenticated entry produces byte-identical [`ControlReceipt`]s —
/// including the bounded [`SabiFailure`] rejections (`NotFound`, `Rights`).
/// The authenticated surface therefore never invents a second rejection
/// vocabulary: it is the same `handle_for_ipc` projection behind every
/// entry.
#[cfg(all(unix, feature = "cli"))]
async fn dispatch_all_entries(
    control: &RecoverySystemControl<'_, StubHealth, CapabilityPolicy>,
    authenticated_socket: &SocketPath,
    plain_socket: &SocketPath,
    principal: PrincipalId,
    signer: &impl Fn(&[u8; 32]) -> Result<[u8; 64], HandshakeError>,
    command: &ControlCommand,
    wall_ms: i64,
) -> [nlos_system_control::control::ControlReceipt; 3] {
    use nlos_system_control::control::{dispatch_in_process, dispatch_over_socket};

    let in_process =
        dispatch_in_process(control, command, MONOTONIC_NOW_NS, wall_ms, None, None).unwrap();
    let plain = dispatch_over_socket(plain_socket, command, None, None)
        .await
        .unwrap();
    let authenticated = dispatch_over_authenticated_socket(
        authenticated_socket,
        principal,
        signer,
        command,
        None,
        None,
    )
    .await
    .unwrap();
    [in_process, plain, authenticated]
}

#[tokio::test]
#[cfg(all(unix, feature = "cli"))]
async fn same_command_receipts_are_identical_across_in_process_plain_and_authenticated_entries() {
    use nlos_schema::sabi::v1::RetryDirective;

    let (mut fixture, key, binding) = Fixture::new("parity", 0x67, 1_000_000);
    let principal = binding.principal_id;
    let signer = honest_signer(&key);
    let wall_ms = i64::try_from(CLOCK_WALL_MS).unwrap();

    let plain_socket = SocketPath::new("pp");
    let plain_listener = UnixListenerAdapter::bind(&plain_socket).unwrap();
    let plain_entry = spawn_plain_entry(
        plain_listener,
        Arc::clone(&fixture.tasks),
        fixture.health.clone(),
    );
    let server = fixture.spawn_serving_n(3);

    let missing = ControlCommand::InspectTask {
        plan_id: [0xEE; 16],
    };
    let denied = ControlCommand::AcknowledgeRecoveryAlert {
        control_command_id: PARITY_DENIED_COMMAND_ID,
        plan_id: *fixture.plan_id.as_bytes(),
        expected_total_failures: 1,
        reason: PARITY_DENIED_REASON.to_owned(),
    };
    let control =
        RecoverySystemControl::new(fixture.tasks.as_ref(), &fixture.health, &CapabilityPolicy);

    let inspect_receipts = dispatch_all_entries(
        &control,
        &fixture.socket,
        &plain_socket,
        principal,
        &signer,
        &ControlCommand::InspectHealth,
        wall_ms,
    )
    .await;
    let missing_receipts = dispatch_all_entries(
        &control,
        &fixture.socket,
        &plain_socket,
        principal,
        &signer,
        &missing,
        wall_ms,
    )
    .await;
    let denied_receipts = dispatch_all_entries(
        &control,
        &fixture.socket,
        &plain_socket,
        principal,
        &signer,
        &denied,
        wall_ms,
    )
    .await;

    // Success parity: the read-only inspection is byte-identical through
    // every entry.
    assert!(
        inspect_receipts
            .iter()
            .all(|receipt| receipt.to_bytes() == inspect_receipts[0].to_bytes())
    );

    // Typed failure parity (`NotFound`): every entry answers the same
    // missing target with the byte-identical bounded failure receipt.
    let Err(missing_failure) = missing_receipts[0].outcome.as_ref() else {
        panic!("expected typed NotFound failure");
    };
    assert_eq!(missing_failure.code, i32::from(SabiErrorCode::NotFound));
    assert_eq!(missing_failure.retry, i32::from(RetryDirective::DoNotRetry));
    assert!(
        missing_receipts
            .iter()
            .all(|receipt| receipt.to_bytes() == missing_receipts[0].to_bytes())
    );

    // Typed failure parity (`Rights`): the policy denial is byte-identical
    // through every entry, with the same sanitized failure vocabulary.
    let Err(denial) = denied_receipts[0].outcome.as_ref() else {
        panic!("expected typed Rights failure");
    };
    assert_eq!(denial.code, i32::from(SabiErrorCode::Rights));
    assert_eq!(denial.retry, i32::from(RetryDirective::DoNotRetry));
    assert_eq!(denial.safe_message, "SystemControl authorization denied");
    assert!(
        denied_receipts
            .iter()
            .all(|receipt| receipt.to_bytes() == denied_receipts[0].to_bytes())
    );

    // The denied acknowledgement mutated nothing on any entry.
    assert!(
        fixture
            .tasks
            .list_artifact_recovery_alerts(8)
            .unwrap()
            .first()
            .unwrap()
            .acknowledgement
            .is_none()
    );

    let outcomes = server.await.unwrap();
    assert_eq!(outcomes.len(), 3);
    for outcome in &outcomes {
        assert!(outcome.as_ref().unwrap().served().is_ok());
    }
    let plain_outcomes = plain_entry.await.unwrap().unwrap();
    assert_eq!(plain_outcomes.len(), 3);
    assert!(plain_outcomes.iter().all(Result::is_ok));
}
