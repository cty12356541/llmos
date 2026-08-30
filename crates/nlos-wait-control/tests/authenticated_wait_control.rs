#![cfg(all(unix, feature = "authenticated-server"))]

//! ADR-0011 authenticated `WaitControl` variant: a real Unix-socket full
//! chain (handshake → verified principal → principal-bound mutation) plus
//! the negative matrix (expired key against the clock wall, bad signature,
//! replayed attestation, unknown principal). The local trust-domain path is
//! covered unchanged by `wait_control.rs`.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::time::Duration;

use ed25519_dalek::{Signer, SigningKey};
use nlos_channel::{ChannelAuthority, ChannelDecision, ChannelRecord, CreateChannelRequest};
use nlos_clock::{AuthorityClock, NowRequest};
use nlos_identity::{
    BootstrapDecision, BootstrapPrincipalRequest, IdentityAuthority, IdentityBinding, KeyPurpose,
};
use nlos_ipc::handshake::transport::{ServerHandshakeContext, authenticated_connect};
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
    CallerIdentity, CapabilityHandle, Envelope, ExchangeRequest, SabiRequestContext,
    SchemaIdentity, envelope,
};
use nlos_types::{IdempotencyKey, PrincipalId};
use nlos_wait::{BindingId, WaitAuthority};
use nlos_wait_control::authenticated::{
    AuthenticatedWaitControlError, AuthenticatedWaitControlServer, principal_bound_idempotency_key,
};
use nlos_wait_control::{
    REGISTER_WAIT_METHOD, WAIT_CONTROL_SERVICE, WaitControlAuthorizer, decode_register_wait_result,
    encode_register_wait_request, payload, wait_control_schema_identity,
};

const NONCE: [u8; 32] = [0x5D; 32];

/// Epoch-ms of 2100-01-01: a "not expiring" key window that still fits the
/// identity authority's `SQLite` i64 encoding.
const KEY_VALID_UNTIL_MS: u64 = 4_102_444_800_000;

fn first_issued_nonce() -> [u8; 32] {
    let mut issued = NONCE;
    issued[0] = 1;
    issued
}

fn key(seed: u8) -> IdempotencyKey {
    IdempotencyKey::from_bytes([seed; 16])
}

fn binding(seed: u8) -> BindingId {
    BindingId::from_bytes([seed; 16])
}

struct Root(PathBuf);

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

impl Root {
    fn new(label: &str) -> Self {
        let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!(
            "nlos-wait-control-auth-{label}-{}-{sequence}",
            std::process::id()
        )))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Short socket path: macOS `SUN_LEN` caps socket paths at 104 bytes.
struct SocketPath(PathBuf);

impl SocketPath {
    fn new(label: &str) -> Self {
        let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!(
            "nlos-wc-auth-{label}-{}-{sequence}",
            std::process::id()
        )))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for SocketPath {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
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

struct AllowPeer;

impl PeerAuthorizer for AllowPeer {
    fn authorize(&self, _: &PeerIdentity) -> Result<(), String> {
        Ok(())
    }
}

struct AllowCapability;

impl WaitControlAuthorizer for AllowCapability {
    fn authorize_register_wait(
        &self,
        _: &SabiRequestContext,
        _: &payload::RegisterWaitRequest,
    ) -> Result<(), &'static str> {
        Ok(())
    }

    fn authorize_notify_commits(
        &self,
        _: &SabiRequestContext,
        _: &payload::NotifyCommitsRequest,
    ) -> Result<(), &'static str> {
        Ok(())
    }

    fn authorize_cancel_wait(
        &self,
        _: &SabiRequestContext,
        _: &payload::CancelWaitRequest,
    ) -> Result<(), &'static str> {
        Ok(())
    }

    fn authorize_list_waits(
        &self,
        _: &SabiRequestContext,
        _: &payload::ListWaitsRequest,
    ) -> Result<(), &'static str> {
        Ok(())
    }

    fn authorize_inspect_wait(
        &self,
        _: &SabiRequestContext,
        _: &payload::InspectWaitRequest,
    ) -> Result<(), &'static str> {
        Ok(())
    }
}

struct Principal {
    identity: IdentityAuthority,
    signing: SigningKey,
    binding: IdentityBinding,
}

/// Bootstraps one principal with an Ed25519 key valid over the given window
/// (epoch milliseconds). The identity and clock authorities live in their
/// own subdirectories so all four authorities coexist on one durable root.
fn bootstrap(root: &Root, seed: u8, valid_until_ms: u64) -> Principal {
    let identity = IdentityAuthority::open(root.path().join("identity")).unwrap();
    let signing = SigningKey::from_bytes(&[seed; 32]);
    let BootstrapDecision::Created(binding) = identity
        .bootstrap_principal(BootstrapPrincipalRequest {
            principal_profile_digest: [seed.wrapping_add(1); 32],
            control_domain_policy_digest: [seed.wrapping_add(2); 32],
            public_key: signing.verifying_key().to_bytes(),
            key_purpose: KeyPurpose::SemanticSigning,
            key_valid_from_ms: 0,
            key_valid_until_ms: valid_until_ms,
            idempotency_key: key(seed.wrapping_add(3)),
            created_at_ms: 0,
        })
        .unwrap()
    else {
        unreachable!("fresh identity authority bootstraps a new principal");
    };
    Principal {
        identity,
        signing,
        binding,
    }
}

/// Opens the clock authority and advances its wall domain once, so the
/// verified-at reading is the real current epoch-millisecond high-water.
fn clock_with_advanced_wall(root: &Root) -> AuthorityClock {
    let clock = AuthorityClock::open(root.path().join("clock")).unwrap();
    clock
        .wall_now(NowRequest {
            idempotency_key: key(0xEE),
        })
        .unwrap();
    clock
}

fn open_waits(root: &Root) -> (Arc<ChannelAuthority>, Arc<WaitAuthority>) {
    let channel = Arc::new(ChannelAuthority::open(root.path()).unwrap());
    let wait = Arc::new(WaitAuthority::open(root.path(), Arc::clone(&channel)).unwrap());
    (channel, wait)
}

fn create_channel(channel: &ChannelAuthority, seed: u8) -> ChannelRecord {
    match channel
        .create_channel(CreateChannelRequest {
            capacity_bytes: 4_096,
            policy_digest: [0x44; 32],
            idempotency_key: key(seed),
            created_at_ms: 900,
        })
        .unwrap()
    {
        ChannelDecision::Created(record) => record,
        ChannelDecision::Replayed(_) => panic!("fresh create cannot replay"),
    }
}

fn request_context(idempotency_key: [u8; 16]) -> SabiRequestContext {
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
        idempotency_key: idempotency_key.to_vec(),
        deadline_monotonic_ns: 0,
        capability_handles: vec![CapabilityHandle {
            slot: 9,
            generation: 1,
        }],
        reservation_handle: None,
        proposal_or_input_digest_sha256: Vec::new(),
    }
}

fn register_envelope(channel: &ChannelRecord, key_seed: u8) -> ExchangeRequest {
    let payload_bytes = encode_register_wait_request(&payload::RegisterWaitRequest {
        schema: Some(wait_control_schema_identity()),
        binding: binding(1).as_bytes().to_vec(),
        channel_id: channel.channel_id.as_bytes().to_vec(),
        target_sequence: 5,
        idempotency_key: key(key_seed).as_bytes().to_vec(),
        registered_at_ms: 1_000,
    })
    .unwrap();
    ExchangeRequest {
        envelope: Some(Envelope {
            schema: Some(SchemaIdentity {
                name: SABI_ENVELOPE_SCHEMA.to_owned(),
                major: 1,
                minor: 1,
                critical_extension_ids: Vec::new(),
                non_critical_extension_ids: Vec::new(),
            }),
            request_id: vec![0x35; 16],
            service: WAIT_CONTROL_SERVICE.to_owned(),
            method: REGISTER_WAIT_METHOD.to_owned(),
            common_context: Some(envelope::CommonContext::RequestContext(request_context(
                key(key_seed).as_bytes().to_owned(),
            ))),
            payload: payload_bytes,
        }),
    }
}

async fn connect_authenticated(
    path: &Path,
    principal_id: PrincipalId,
    signing: &SigningKey,
) -> LocalRpcClient<tokio::net::UnixStream> {
    let framed = authenticated_connect(
        path,
        transport_config(),
        principal_id,
        |digest: &[u8; 32]| Ok(signing.sign(digest).to_bytes()),
    )
    .await
    .unwrap();
    LocalRpcClient::new(framed.into_inner(), transport_config())
}

#[tokio::test]
async fn authenticated_roundtrip_binds_mutation_keys_to_the_principal() {
    let root = Root::new("roundtrip");
    let socket = SocketPath::new("roundtrip");
    let principal = bootstrap(&root, 0x41, KEY_VALID_UNTIL_MS);
    let clock = clock_with_advanced_wall(&root);
    let handshake = ServerHandshakeContext::new(socket.path(), 8).unwrap();
    let (channel_authority, waits) = open_waits(&root);
    let channel = create_channel(&channel_authority, 0xA1);

    let listener = UnixListenerAdapter::bind(socket.path()).unwrap();
    let server = AuthenticatedWaitControlServer::new(
        Arc::clone(&waits),
        AllowCapability,
        principal.identity,
        clock,
        handshake,
    );
    let server_task = tokio::spawn(async move {
        let sequence = AtomicU8::new(0);
        let mut outcomes = Vec::new();
        for _ in 0..2 {
            let next_nonce = || {
                let mut issued = NONCE;
                issued[0] = sequence.fetch_add(1, Ordering::Relaxed) + 1;
                issued
            };
            outcomes.push(
                server
                    .serve_one(&listener, transport_config(), &AllowPeer, 10, next_nonce)
                    .await,
            );
        }
        outcomes
    });

    let response = connect_authenticated(
        socket.path(),
        principal.binding.principal_id,
        &principal.signing,
    )
    .await
    .exchange_validated(register_envelope(&channel, 1))
    .await
    .unwrap();

    // The registration went through the authenticated path and the durable
    // row carries the principal-bound key, not the raw client key bytes.
    let result = decode_register_wait_result(&response.envelope().payload).unwrap();
    assert!(!result.replayed);
    let record = result.record.expect("registered record");
    let raw_key = key(1).as_bytes().to_vec();
    assert_ne!(record.idempotency_key, raw_key);
    assert_eq!(
        record.idempotency_key,
        principal_bound_idempotency_key(principal.binding.principal_id, *key(1).as_bytes())
            .to_vec()
    );
    let durable = waits.list_waits(None).unwrap();
    assert_eq!(durable.len(), 1);
    assert_eq!(
        durable[0].idempotency_key.as_bytes().as_slice(),
        record.idempotency_key.as_slice()
    );

    // A byte-identical retry over a second authenticated connection replays
    // the same durable row: the bound key is a stable replay identity.
    let replay = connect_authenticated(
        socket.path(),
        principal.binding.principal_id,
        &principal.signing,
    )
    .await
    .exchange_validated(register_envelope(&channel, 1))
    .await
    .unwrap();
    let replay_result = decode_register_wait_result(&replay.envelope().payload).unwrap();
    assert!(replay_result.replayed);
    assert_eq!(replay_result.record, Some(record));

    let outcomes = server_task.await.unwrap();
    assert!(outcomes[0].is_ok(), "first connection: {:?}", outcomes[0]);
    assert!(outcomes[1].is_ok(), "second connection: {:?}", outcomes[1]);
    assert_eq!(
        outcomes[0].as_ref().unwrap().verified().principal_id(),
        principal.binding.principal_id
    );
}

#[tokio::test]
async fn expired_signing_key_fails_closed_against_the_clock_wall() {
    let root = Root::new("expired");
    let socket = SocketPath::new("expired");
    // The key died at wall-ms 1_000; the seeded wall reading is the real
    // current epoch millisecond, so verification must fail closed as
    // KeyExpired — proving verified_at comes from the clock authority.
    let principal = bootstrap(&root, 0x42, 1_000);
    let clock = clock_with_advanced_wall(&root);
    let handshake = ServerHandshakeContext::new(socket.path(), 8).unwrap();
    let (_channel_authority, waits) = open_waits(&root);

    let listener = UnixListenerAdapter::bind(socket.path()).unwrap();
    let server = AuthenticatedWaitControlServer::new(
        Arc::clone(&waits),
        AllowCapability,
        principal.identity,
        clock,
        handshake,
    );
    let server_task = tokio::spawn(async move {
        server
            .serve_one(&listener, transport_config(), &AllowPeer, 10, || NONCE)
            .await
    });
    // The connect itself succeeds (the server answered the challenge); the
    // fail-closed rejection surfaces as EOF on the client's next read.
    let mut framed = authenticated_connect(
        socket.path(),
        transport_config(),
        principal.binding.principal_id,
        |digest: &[u8; 32]| Ok(principal.signing.sign(digest).to_bytes()),
    )
    .await
    .unwrap();
    let client_error = framed.receive().await.unwrap_err();

    let outcome = server_task.await.unwrap().unwrap_err();
    assert!(
        matches!(
            outcome,
            AuthenticatedWaitControlError::Handshake(HandshakeError::KeyExpired)
        ),
        "unexpected outcome: {outcome:?}"
    );
    assert!(
        matches!(
            client_error,
            IpcError::Io {
                operation: IoOperation::Read,
                ..
            }
        ),
        "unexpected client error: {client_error:?}"
    );
    assert!(waits.list_waits(None).unwrap().is_empty());
}

#[tokio::test]
async fn bad_signature_fails_closed_without_touching_the_registry() {
    let root = Root::new("bad-sig");
    let socket = SocketPath::new("bad-sig");
    let principal = bootstrap(&root, 0x43, KEY_VALID_UNTIL_MS);
    let clock = clock_with_advanced_wall(&root);
    let handshake = ServerHandshakeContext::new(socket.path(), 8).unwrap();
    let (_channel_authority, waits) = open_waits(&root);

    let listener = UnixListenerAdapter::bind(socket.path()).unwrap();
    let server = AuthenticatedWaitControlServer::new(
        Arc::clone(&waits),
        AllowCapability,
        principal.identity,
        clock,
        handshake,
    );
    let server_task = tokio::spawn(async move {
        server
            .serve_one(&listener, transport_config(), &AllowPeer, 10, || NONCE)
            .await
    });
    let mut framed = authenticated_connect(
        socket.path(),
        transport_config(),
        principal.binding.principal_id,
        |_digest: &[u8; 32]| Ok([0_u8; 64]),
    )
    .await
    .unwrap();
    let client_error = framed.receive().await.unwrap_err();

    let outcome = server_task.await.unwrap().unwrap_err();
    assert!(
        matches!(
            outcome,
            AuthenticatedWaitControlError::Handshake(HandshakeError::SignatureInvalid)
        ),
        "unexpected outcome: {outcome:?}"
    );
    assert!(
        matches!(
            client_error,
            IpcError::Io {
                operation: IoOperation::Read,
                ..
            }
        ),
        "unexpected client error: {client_error:?}"
    );
    assert!(waits.list_waits(None).unwrap().is_empty());
}

#[tokio::test]
async fn replayed_attestation_fails_the_second_connection() {
    let root = Root::new("replay");
    let socket = SocketPath::new("replay");
    let principal = bootstrap(&root, 0x44, KEY_VALID_UNTIL_MS);
    let clock = clock_with_advanced_wall(&root);
    let handshake = ServerHandshakeContext::new(socket.path(), 8).unwrap();
    let (_channel_authority, waits) = open_waits(&root);
    let expected_binding = handshake.binding().to_vec();

    let listener = UnixListenerAdapter::bind(socket.path()).unwrap();
    let server = AuthenticatedWaitControlServer::new(
        Arc::clone(&waits),
        AllowCapability,
        principal.identity,
        clock,
        handshake,
    );
    let server_task = tokio::spawn(async move {
        let sequence = AtomicU8::new(0);
        let mut outcomes = Vec::new();
        for _ in 0..2 {
            let next_nonce = || {
                let mut issued = NONCE;
                issued[0] = sequence.fetch_add(1, Ordering::Relaxed) + 1;
                issued
            };
            outcomes.push(
                server
                    .serve_one(&listener, transport_config(), &AllowPeer, 10, next_nonce)
                    .await,
            );
        }
        outcomes
    });

    // Connection 1: an honest client answers the challenge; its attestation
    // bytes are captured verbatim for the replay attempt.
    let replayed_attestation = {
        let (stream, _peer) = connect(socket.path(), transport_config()).await.unwrap();
        let mut framed = FramedIo::new(stream, transport_config());
        let challenge = decode_challenge_wire(&framed.receive().await.unwrap()).unwrap();
        assert_eq!(challenge.nonce, first_issued_nonce().to_vec());
        let signature = principal
            .signing
            .sign(&principal_handshake_message(
                &first_issued_nonce(),
                principal.binding.principal_id,
                &expected_binding,
            ))
            .to_bytes();
        let attestation = client_attestation(
            principal.binding.principal_id,
            &challenge,
            &expected_binding,
            signature,
        )
        .unwrap();
        let wire = encode_attestation_wire(&attestation).unwrap();
        framed.send(&wire).await.unwrap();
        wire
    };

    // Connection 2: the server has already issued a fresh nonce, so the
    // verbatim attestation must fail closed on the consumed nonce.
    {
        let (stream, _peer) = connect(socket.path(), transport_config()).await.unwrap();
        let mut framed = FramedIo::new(stream, transport_config());
        let challenge = decode_challenge_wire(&framed.receive().await.unwrap()).unwrap();
        assert_ne!(challenge.nonce, first_issued_nonce().to_vec());
        framed.send(&replayed_attestation).await.unwrap();
        framed.receive().await.unwrap_err();
    }

    let outcomes = server_task.await.unwrap();
    assert!(outcomes[0].is_ok(), "first connection: {:?}", outcomes[0]);
    assert!(
        matches!(
            &outcomes[1],
            Err(AuthenticatedWaitControlError::Handshake(
                HandshakeError::NonceRejected
            ))
        ),
        "unexpected replay outcome: {:?}",
        outcomes[1]
    );
}

#[tokio::test]
async fn unknown_principal_fails_closed() {
    let root = Root::new("stranger");
    let socket = SocketPath::new("stranger");
    let seeded = bootstrap(&root, 0x45, KEY_VALID_UNTIL_MS);
    let stranger = PrincipalId::from_bytes([0xEE; 16]);
    let stranger_signing = SigningKey::from_bytes(&[0x46; 32]);
    let clock = clock_with_advanced_wall(&root);
    let handshake = ServerHandshakeContext::new(socket.path(), 8).unwrap();
    let (_channel_authority, waits) = open_waits(&root);

    let listener = UnixListenerAdapter::bind(socket.path()).unwrap();
    let server = AuthenticatedWaitControlServer::new(
        Arc::clone(&waits),
        AllowCapability,
        seeded.identity,
        clock,
        handshake,
    );
    let server_task = tokio::spawn(async move {
        server
            .serve_one(&listener, transport_config(), &AllowPeer, 10, || NONCE)
            .await
    });
    let _ = authenticated_connect(
        socket.path(),
        transport_config(),
        stranger,
        |digest: &[u8; 32]| Ok(stranger_signing.sign(digest).to_bytes()),
    )
    .await;

    let outcome = server_task.await.unwrap().unwrap_err();
    assert!(
        matches!(
            outcome,
            AuthenticatedWaitControlError::Handshake(HandshakeError::PrincipalUnknown(id))
                if id == stranger
        ),
        "unexpected outcome: {outcome:?}"
    );
    assert!(waits.list_waits(None).unwrap().is_empty());
}
