#![cfg(all(unix, feature = "authenticated-ipc"))]

//! Acceptance tests for the opt-in authenticated `TakeoverControl` serving
//! variant: a real Unix-socket roundtrip whose barrier observation travels
//! the authenticated path, plus the typed negative matrix (bad handshake
//! signature, replayed attestation, unknown principal) — each proving zero
//! durable rows and the nonce-burning fail-closed contract.
//!
//! The transport handshake principal is deliberately a different principal
//! (and key purpose) from the barrier-observation signer, so every assertion
//! exercises the documented two-signature-layer model.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer, SigningKey};
use nlos_clock::{AuthorityClock, AuthorityClockError, NowRequest, WallSource};
use nlos_identity::{BootstrapPrincipalRequest, IdentityAuthority, IdentityBinding, KeyPurpose};
use nlos_ipc::handshake::transport::{
    AuthenticatedServeOutcome, ServerHandshakeContext, authenticated_connect,
};
use nlos_ipc::handshake::{
    HandshakeError, client_attestation, decode_challenge_wire, encode_attestation_wire,
    principal_handshake_message,
};
use nlos_ipc::unix::{UnixListenerAdapter, connect};
use nlos_ipc::{
    FramedIo, IoOperation, IpcError, LocalRpcClient, PeerAuthorizer, PeerIdentity, TransportConfig,
};
use nlos_schema::sabi::v1::{
    BarrierObservationEvidence, BarrierObservationSignature as BarrierObservationSignatureProto,
    BarrierObservationTarget, CallerIdentity, CapabilityHandle, Envelope, ExchangeRequest,
    SabiRequestContext, SchemaIdentity, SubmitBarrierObservationRequest, envelope,
};
use nlos_schema::{
    MethodSemantics, SABI_ENVELOPE_SCHEMA, decode_barrier_observation_record,
    encode_submit_barrier_observation_request, takeover_control_schema_identity,
    validate_sabi_response_context,
};
use nlos_takeover_control::authenticated::{AuthenticatedIpcError, AuthenticatedTakeoverControl};
use nlos_takeover_control::{
    SUBMIT_BARRIER_OBSERVATION_METHOD, TAKEOVER_CONTROL_SERVICE, TakeoverControlAuthorizer,
    participant_type_code,
};
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
use tokio::task::JoinHandle;

const NONCE: [u8; 32] = [0x5D; 32];
const WALL_ANCHOR_MS: u64 = 5_000;

/// First nonce a spawned server issues: the sequence byte replaces
/// `NONCE[0]`, so the first connection sees `[1, ...]`.
fn first_issued_nonce() -> [u8; 32] {
    let mut issued = NONCE;
    issued[0] = 1;
    issued
}

static NEXT: AtomicU64 = AtomicU64::new(1);

struct TestDatabase {
    path: PathBuf,
}

impl TestDatabase {
    fn new(label: &str) -> Self {
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        Self {
            path: std::env::temp_dir().join(format!(
                "nta-{label}-{}-{sequence}.sqlite3",
                std::process::id()
            )),
        }
    }

    fn open(&self) -> SqliteTaskAuthority {
        SqliteTaskAuthority::open(&self.path).expect("open task authority")
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        for path in [
            self.path.clone(),
            suffix_path(&self.path, "-wal"),
            suffix_path(&self.path, "-shm"),
        ] {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("remove test database: {error}"),
            }
        }
    }
}

fn suffix_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

struct TempRoot(PathBuf);

impl TempRoot {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("monotonic clock")
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "nta-{label}-{}-{nonce}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Short socket path: macOS `SUN_LEN` caps socket paths at 104 bytes.
struct SocketPath(PathBuf);

impl SocketPath {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("monotonic clock")
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "nta-{label}-{}-{nonce}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
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

/// Deterministic wall source: the transport-layer verification instant is
/// anchored to this committed durable reading, never to the system clock.
struct ManualWallSource(Arc<AtomicU64>);

impl ManualWallSource {
    fn at(ms: u64) -> Self {
        Self(Arc::new(AtomicU64::new(ms)))
    }
}

impl WallSource for ManualWallSource {
    fn now_ms(&self) -> Result<u64, AuthorityClockError> {
        Ok(self.0.load(Ordering::Relaxed))
    }
}

struct TestSigner {
    key: SigningKey,
    binding: IdentityBinding,
}

fn bootstrap_test_signer(
    identity: &IdentityAuthority,
    seed: u8,
    purpose: KeyPurpose,
    valid_from_ms: u64,
    valid_until_ms: u64,
) -> TestSigner {
    let key = SigningKey::from_bytes(&[seed; 32]);
    let binding = identity
        .bootstrap_principal(BootstrapPrincipalRequest {
            principal_profile_digest: [seed.wrapping_add(1); 32],
            control_domain_policy_digest: [seed.wrapping_add(2); 32],
            public_key: key.verifying_key().to_bytes(),
            key_purpose: purpose,
            key_valid_from_ms: valid_from_ms,
            key_valid_until_ms: valid_until_ms,
            idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(3); 16]),
            created_at_ms: 0,
        })
        .expect("bootstrap signer")
        .binding();
    TestSigner { key, binding }
}

struct FrozenFence {
    takeover_receipt_id: ReceiptId,
    participant: ParticipantRecord,
    fence_set_root: [u8; 32],
}

fn lease_request(holder: u8, key: u8, at_ms: i64, ttl_ms: i64) -> AuthorityLeaseRequest {
    AuthorityLeaseRequest {
        holder_id: ProcessId::from_bytes([key.wrapping_add(holder); 16]),
        idempotency_key: IdempotencyKey::from_bytes([key; 16]),
        requested_at_ms: at_ms,
        ttl_ms,
    }
}

fn register_task_attempt(authority: &SqliteTaskAuthority, seed: u8) -> AttemptSpec {
    let task_id = TaskId::from_bytes([seed; 16]);
    authority
        .register_task(TaskSpec {
            task_id,
            task_generation: Generation::INITIAL,
            registered_at_ms: 1,
        })
        .expect("register task");
    let attempt = AttemptSpec {
        task_id,
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
        registered_at_ms: 2,
    };
    authority
        .register_attempt(attempt)
        .expect("register attempt");
    attempt
}

fn fence_takeover(authority: &SqliteTaskAuthority, seed: u8) -> FrozenFence {
    let lease_one = authority
        .acquire_authority_lease(lease_request(1, seed, 100, 100))
        .expect("initial lease")
        .record();
    let attempt = register_task_attempt(authority, seed);
    let permit = match authority
        .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
            permit: PermitRequest {
                task_id: attempt.task_id,
                attempt_id: attempt.attempt_id,
                attempt_generation: attempt.attempt_generation,
                write_set_root: [seed; 32],
                planned_effects: Vec::new(),
                idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(10); 16]),
                valid_until_ms: 10_000,
                requested_at_ms: 150,
            },
            lease: lease_one,
        })
        .expect("lease-bound permit")
    {
        PermitDecision::Issued(permit) => *permit,
        other => panic!("expected issued permit, got {other:?}"),
    };
    let registry_binding = permit
        .participant_registry_binding
        .expect("permit registry binding");
    authority
        .finalize_commit_v3_with_authority_lease(AuthorityLeaseFinalizeRequest {
            finalize: FinalizeRequestV3 {
                base: FinalizeRequest {
                    task_id: attempt.task_id,
                    attempt_id: attempt.attempt_id,
                    attempt_generation: attempt.attempt_generation,
                    permit_id: permit.permit_id,
                    new_effect_history_root: [0; 32],
                    new_retry_fence_epoch: 0,
                    finalized_at_ms: 160,
                },
                required_satisfaction: Vec::new(),
                fenced_participant_digest: [0; 32],
            },
            lease: lease_one,
        })
        .expect("close permit before takeover");
    let lease_two = authority
        .acquire_authority_lease(lease_request(2, seed.wrapping_add(0x11), 201, 1_000))
        .expect("takeover lease")
        .record();
    let frozen = authority
        .prepare_authority_takeover_fence(AuthorityLeaseTakeoverFenceRequest {
            task_id: attempt.task_id,
            expected_registry_binding: registry_binding,
            lease: lease_two,
            requested_at_ms: 210,
        })
        .expect("freeze current registry");
    let fence_receipt = authority
        .inspect_authority_takeover_fence_receipt(attempt.task_id, registry_binding)
        .expect("takeover fence receipt");
    let takeover_receipt = authority
        .inspect_authority_takeover_receipt(attempt.task_id, fence_receipt.receipt_id)
        .expect("pending takeover receipt");
    FrozenFence {
        takeover_receipt_id: takeover_receipt.receipt_id,
        participant: frozen
            .participants
            .first()
            .copied()
            .expect("frozen registry participant"),
        fence_set_root: takeover_receipt
            .exact_fence_set_root
            .expect("exact fence set root"),
    }
}

struct Fixture {
    authority: Arc<SqliteTaskAuthority>,
    identity: Arc<IdentityAuthority>,
    clock: Arc<AuthorityClock>,
    handshake: TestSigner,
    barrier: TestSigner,
    fence: FrozenFence,
    // Held only for its Drop-time SQLite connection close and file cleanup;
    // the path is never read after Fixture::new opens the authority.
    #[expect(dead_code)]
    database: TestDatabase,
    _identity_root: TempRoot,
    _clock_root: TempRoot,
}

impl Fixture {
    /// Builds the full authenticated-serving fixture. The clock's wall
    /// domain is committed once at `WALL_ANCHOR_MS`, and the handshake
    /// principal's key window is `[1_000, 5_500]`: a fresh-store reading of
    /// 0 fails `KeyNotYetValid` and a real system-clock reading fails
    /// `KeyExpired`, so only the committed durable wall reading verifies.
    fn new(label: &str) -> Self {
        let database = TestDatabase::new(label);
        let identity_root = TempRoot::new(label);
        let clock_root = TempRoot::new(label);
        let authority = Arc::new(database.open());
        let identity = Arc::new(IdentityAuthority::open(identity_root.path()).unwrap());
        let clock = Arc::new(
            AuthorityClock::open_with_wall_source(
                clock_root.path(),
                ManualWallSource::at(WALL_ANCHOR_MS),
            )
            .unwrap(),
        );
        let advanced = clock
            .wall_now(NowRequest {
                idempotency_key: IdempotencyKey::from_bytes([0x5A; 16]),
            })
            .expect("commit the wall anchor reading");
        assert_eq!(advanced.reading().as_u64(), WALL_ANCHOR_MS);
        // Transport layer: SemanticSigning key, valid only at the anchored wall reading.
        let handshake =
            bootstrap_test_signer(&identity, 0x43, KeyPurpose::SemanticSigning, 1_000, 5_500);
        // Payload layer: BarrierObservationSigning key under the unchanged
        // store-side time semantics.
        let barrier = bootstrap_test_signer(
            &identity,
            0x44,
            KeyPurpose::BarrierObservationSigning,
            0,
            10_000,
        );
        let fence = fence_takeover(&authority, 0x45);
        Self {
            authority,
            identity,
            clock,
            handshake,
            barrier,
            fence,
            database,
            _identity_root: identity_root,
            _clock_root: clock_root,
        }
    }
}

struct CapabilityPolicy;

impl TakeoverControlAuthorizer for CapabilityPolicy {
    fn authorize_submit_barrier_observation(
        &self,
        context: &SabiRequestContext,
        _: &SubmitBarrierObservationRequest,
    ) -> Result<(), &'static str> {
        if context.capability_handles
            == [CapabilityHandle {
                slot: 5,
                generation: 1,
            }]
        {
            Ok(())
        } else {
            Err("missing takeover control capability")
        }
    }
}

struct AllowPeer;

impl PeerAuthorizer for AllowPeer {
    fn authorize(&self, _: &PeerIdentity) -> Result<(), String> {
        Ok(())
    }
}

fn remote_receipt_id() -> ReceiptId {
    ReceiptId::from_bytes([0x91; 16])
}

fn barrier_digest() -> [u8; 32] {
    [0x92; 32]
}

const OBSERVED_AT_MS: i64 = 220;

fn observation(fence: &FrozenFence, signer: &TestSigner) -> [u8; 64] {
    let digest = barrier_observation_signature_message(
        fence.takeover_receipt_id,
        &fence.participant,
        remote_receipt_id(),
        barrier_digest(),
        fence.fence_set_root,
    );
    signer.key.sign(&digest).to_bytes()
}

fn request_context() -> SabiRequestContext {
    SabiRequestContext {
        caller: Some(CallerIdentity {
            principal_id: vec![0x31; 16],
            application_id: vec![0x32; 16],
            process_id: vec![0x33; 16],
            process_generation: 1,
        }),
        activity_context: Vec::new(),
        task_execution_binding: None,
        correlation_id: vec![0x34; 16],
        idempotency_key: vec![0x45; 16],
        deadline_monotonic_ns: 0,
        capability_handles: vec![CapabilityHandle {
            slot: 5,
            generation: 1,
        }],
        reservation_handle: None,
        proposal_or_input_digest_sha256: Vec::new(),
    }
}

fn submit_request(fence: &FrozenFence, signer: &TestSigner, signature: [u8; 64]) -> Envelope {
    let submit = SubmitBarrierObservationRequest {
        schema: Some(takeover_control_schema_identity()),
        target: Some(BarrierObservationTarget {
            takeover_receipt_id: fence.takeover_receipt_id.into_bytes().to_vec(),
            participant_type: participant_type_code(fence.participant.participant_type),
            participant_id: fence.participant.participant_id.as_bytes().to_vec(),
            participant_generation: fence.participant.participant_generation.get(),
            admission_receipt_id: fence.participant.admission_receipt_id.into_bytes().to_vec(),
        }),
        evidence: Some(BarrierObservationEvidence {
            remote_receipt_id: remote_receipt_id().into_bytes().to_vec(),
            barrier_digest: barrier_digest().to_vec(),
            observed_at_ms: OBSERVED_AT_MS,
        }),
        signature: Some(BarrierObservationSignatureProto {
            signer_principal_id: signer.binding.principal_id.as_bytes().to_vec(),
            signer_control_domain_id: signer.binding.control_domain_id.as_bytes().to_vec(),
            signer_key_id: signer.binding.key_id.as_bytes().to_vec(),
            signature: signature.to_vec(),
        }),
    };
    Envelope {
        schema: Some(SchemaIdentity {
            name: SABI_ENVELOPE_SCHEMA.to_owned(),
            major: 1,
            minor: 1,
            critical_extension_ids: Vec::new(),
            non_critical_extension_ids: Vec::new(),
        }),
        request_id: vec![0x35; 16],
        service: TAKEOVER_CONTROL_SERVICE.to_owned(),
        method: SUBMIT_BARRIER_OBSERVATION_METHOD.to_owned(),
        common_context: Some(envelope::CommonContext::RequestContext(request_context())),
        payload: encode_submit_barrier_observation_request(&submit).unwrap(),
    }
}

fn transport_config() -> TransportConfig {
    TransportConfig::new(
        64 * 1024,
        Duration::from_secs(5),
        Duration::from_secs(5),
        Duration::from_secs(5),
    )
    .unwrap()
}

#[allow(clippy::too_many_arguments)]
fn spawn_authenticated_serve(
    listener: UnixListenerAdapter,
    transport: TransportConfig,
    authority: Arc<SqliteTaskAuthority>,
    identity: Arc<IdentityAuthority>,
    clock: Arc<AuthorityClock>,
    context: Arc<ServerHandshakeContext>,
    connections: usize,
) -> JoinHandle<Vec<Result<AuthenticatedServeOutcome, AuthenticatedIpcError>>> {
    tokio::spawn(async move {
        let sequence = AtomicU8::new(0);
        let mut outcomes = Vec::new();
        for _ in 0..connections {
            let server = AuthenticatedTakeoverControl::new(
                authority.as_ref(),
                identity.as_ref(),
                &CapabilityPolicy,
                clock.as_ref(),
                context.as_ref(),
            );
            let next_nonce = || {
                let mut issued = NONCE;
                issued[0] = sequence.fetch_add(1, Ordering::Relaxed) + 1;
                issued
            };
            outcomes.push(
                server
                    .serve_one(&listener, transport, &AllowPeer, 10, 6_000, next_nonce)
                    .await,
            );
        }
        outcomes
    })
}

fn durable_rows(fixture: &Fixture) -> usize {
    fixture
        .authority
        .inspect_authority_takeover_barrier_receipts(fixture.fence.takeover_receipt_id)
        .expect("inspect durable barrier rows")
        .len()
}

#[tokio::test]
async fn authenticated_observation_roundtrip_over_real_unix_socket() {
    let fixture = Fixture::new("round");
    let socket_path = SocketPath::new("round");
    let context = Arc::new(ServerHandshakeContext::new(&socket_path, 8).unwrap());
    let listener = UnixListenerAdapter::bind(&socket_path).unwrap();
    let server = spawn_authenticated_serve(
        listener,
        transport_config(),
        Arc::clone(&fixture.authority),
        Arc::clone(&fixture.identity),
        Arc::clone(&fixture.clock),
        Arc::clone(&context),
        1,
    );

    let framed = authenticated_connect(
        &socket_path,
        transport_config(),
        fixture.handshake.binding.principal_id,
        |digest: &[u8; 32]| Ok(fixture.handshake.key.sign(digest).to_bytes()),
    )
    .await
    .expect("authenticated connect succeeds at the wall-anchored reading");
    let request = ExchangeRequest {
        envelope: Some(submit_request(
            &fixture.fence,
            &fixture.barrier,
            observation(&fixture.fence, &fixture.barrier),
        )),
    };
    let response = LocalRpcClient::new(framed.into_inner(), transport_config())
        .exchange_validated(request)
        .await
        .expect("barrier observation crosses the authenticated path");

    let outcome = server.await.unwrap().remove(0).expect("handshake verified");
    assert_eq!(
        outcome.verified().principal_id(),
        fixture.handshake.binding.principal_id
    );
    assert!(outcome.served().is_ok());

    let response_envelope = response.envelope();
    validate_sabi_response_context(response_envelope, MethodSemantics::MUTATION).unwrap();
    let record = decode_barrier_observation_record(&response_envelope.payload).unwrap();
    assert!(record.signed);
    // Two layers, two principals: the durable signer is the remote barrier
    // observer, not the authenticated transport caller.
    assert_ne!(
        fixture.handshake.binding.principal_id,
        fixture.barrier.binding.principal_id
    );
    assert_eq!(
        record.signer_principal_id,
        fixture.barrier.binding.principal_id.as_bytes().to_vec()
    );
    assert_eq!(durable_rows(&fixture), 1);
    assert_eq!(
        fixture
            .authority
            .inspect_authority_takeover_barrier_coverage(fixture.fence.takeover_receipt_id)
            .unwrap()
            .state,
        AuthorityTakeoverBarrierCoverageState::LocallyCovered
    );
    // The single-use handshake nonce was consumed and never returned.
    assert!(matches!(
        context.nonces().consume(&first_issued_nonce()),
        Err(HandshakeError::NonceRejected)
    ));
}

#[tokio::test]
async fn bad_handshake_signature_rejects_before_serving() {
    let fixture = Fixture::new("bsig");
    let socket_path = SocketPath::new("bsig");
    let context = Arc::new(ServerHandshakeContext::new(&socket_path, 8).unwrap());
    let listener = UnixListenerAdapter::bind(&socket_path).unwrap();
    let server = spawn_authenticated_serve(
        listener,
        transport_config(),
        Arc::clone(&fixture.authority),
        Arc::clone(&fixture.identity),
        Arc::clone(&fixture.clock),
        Arc::clone(&context),
        1,
    );

    let mut framed = authenticated_connect(
        &socket_path,
        transport_config(),
        fixture.handshake.binding.principal_id,
        |_digest: &[u8; 32]| Ok([0_u8; 64]),
    )
    .await
    .expect("the client sends its attestation before the server rejects");
    let client_error = framed.receive().await.unwrap_err();

    let error = server.await.unwrap().remove(0).unwrap_err();
    assert!(matches!(
        error,
        AuthenticatedIpcError::Handshake(HandshakeError::SignatureInvalid)
    ));
    // The connection was dropped before any request byte was served.
    assert!(matches!(
        client_error,
        IpcError::Io {
            operation: IoOperation::Read,
            ..
        }
    ));
    assert_eq!(durable_rows(&fixture), 0);
    // The burned nonce was consumed by the failed verification.
    assert!(matches!(
        context.nonces().consume(&first_issued_nonce()),
        Err(HandshakeError::NonceRejected)
    ));
}

#[tokio::test]
async fn replayed_attestation_fails_the_second_connection() {
    let fixture = Fixture::new("rpl");
    let socket_path = SocketPath::new("rpl");
    let context = Arc::new(ServerHandshakeContext::new(&socket_path, 8).unwrap());
    let listener = UnixListenerAdapter::bind(&socket_path).unwrap();
    let binding = context.binding().to_vec();
    let server = spawn_authenticated_serve(
        listener,
        transport_config(),
        Arc::clone(&fixture.authority),
        Arc::clone(&fixture.identity),
        Arc::clone(&fixture.clock),
        Arc::clone(&context),
        2,
    );

    // Connection 1: an honest client answers the challenge; its attestation
    // wire bytes are captured, then it disconnects without a request.
    let (captured_nonce, captured_wire) = {
        let (stream, _peer) = connect(&socket_path, transport_config()).await.unwrap();
        let mut framed = FramedIo::new(stream, transport_config());
        let challenge = decode_challenge_wire(&framed.receive().await.unwrap()).unwrap();
        assert_eq!(challenge.nonce, first_issued_nonce().to_vec());
        let signature = fixture
            .handshake
            .key
            .sign(&principal_handshake_message(
                &first_issued_nonce(),
                fixture.handshake.binding.principal_id,
                &binding,
            ))
            .to_bytes();
        let attestation = client_attestation(
            fixture.handshake.binding.principal_id,
            &challenge,
            &binding,
            signature,
        )
        .unwrap();
        let wire = encode_attestation_wire(&attestation).unwrap();
        framed.send(&wire).await.unwrap();
        (first_issued_nonce(), wire)
    };

    // Connection 2: the server issued a fresh nonce; replaying connection 1's
    // attestation verbatim must fail closed on the consumed nonce.
    let second_error = {
        let (stream, _peer) = connect(&socket_path, transport_config()).await.unwrap();
        let mut framed = FramedIo::new(stream, transport_config());
        let challenge = decode_challenge_wire(&framed.receive().await.unwrap()).unwrap();
        assert_ne!(challenge.nonce, captured_nonce.to_vec());
        framed.send(&captured_wire).await.unwrap();
        framed.receive().await.unwrap_err()
    };

    let mut outcomes = server.await.unwrap();
    let first = outcomes.remove(0).expect("first handshake verified");
    assert_eq!(
        first.verified().principal_id(),
        fixture.handshake.binding.principal_id
    );
    // The honest client disconnected before a request; the serve phase keeps
    // its unchanged semantics and reports the transport EOF.
    assert!(matches!(first.served(), Err(IpcError::Io { .. })));
    assert!(matches!(
        outcomes.remove(0),
        Err(AuthenticatedIpcError::Handshake(
            HandshakeError::NonceRejected
        ))
    ));
    assert!(matches!(
        second_error,
        IpcError::Io {
            operation: IoOperation::Read,
            ..
        }
    ));
    assert_eq!(durable_rows(&fixture), 0);
    assert!(matches!(
        context.nonces().consume(&captured_nonce),
        Err(HandshakeError::NonceRejected)
    ));
}

#[tokio::test]
async fn unknown_principal_fails_closed() {
    let fixture = Fixture::new("str");
    let socket_path = SocketPath::new("str");
    let context = Arc::new(ServerHandshakeContext::new(&socket_path, 8).unwrap());
    let listener = UnixListenerAdapter::bind(&socket_path).unwrap();
    let server = spawn_authenticated_serve(
        listener,
        transport_config(),
        Arc::clone(&fixture.authority),
        Arc::clone(&fixture.identity),
        Arc::clone(&fixture.clock),
        Arc::clone(&context),
        1,
    );

    let stranger = nlos_types::PrincipalId::from_bytes([0xEE; 16]);
    let stranger_key = SigningKey::from_bytes(&[0x77; 32]);
    let _ = authenticated_connect(
        &socket_path,
        transport_config(),
        stranger,
        |digest: &[u8; 32]| Ok(stranger_key.sign(digest).to_bytes()),
    )
    .await;

    let error = server.await.unwrap().remove(0).unwrap_err();
    assert!(matches!(
        error,
        AuthenticatedIpcError::Handshake(HandshakeError::PrincipalUnknown(id)) if id == stranger
    ));
    assert_eq!(durable_rows(&fixture), 0);
    assert!(matches!(
        context.nonces().consume(&first_issued_nonce()),
        Err(HandshakeError::NonceRejected)
    ));
}
