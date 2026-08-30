#![cfg(unix)]

//! Transport-wiring tests for the ADR-0011 handshake: a real Unix-socket
//! full chain with genuine `IdentityAuthority` verification, plus the full
//! negative matrix (bad signature, replayed nonce, channel-binding drift,
//! unknown principal, out-of-band frame, pre-gate denial, timeout).

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer, SigningKey};
use nlos_identity::{
    BootstrapDecision, BootstrapPrincipalRequest, IdentityAuthority, IdentityBinding, KeyPurpose,
};
use nlos_ipc::handshake::transport::{
    AuthenticatedServeOutcome, ServerHandshakeContext, authenticated_connect,
    authenticated_serve_one, endpoint_channel_binding,
};
use nlos_ipc::handshake::{
    HandshakeError, HandshakeNonceRegistry, client_attestation, decode_challenge_wire,
    encode_attestation_wire, principal_handshake_message,
};
use nlos_ipc::unix::{UnixListenerAdapter, connect};
use nlos_ipc::{
    FramedIo, IoOperation, IpcError, LocalRpcClient, OutboundResponse, PeerAuthorizer,
    PeerIdentity, TransportConfig,
};
use nlos_schema::sabi::v1::{Envelope, ExchangeRequest, ExchangeResponse, SchemaIdentity};
use nlos_schema::{SABI_ENVELOPE_SCHEMA, ValidatedExchangeRequest, encode_exchange_request};
use nlos_types::{IdempotencyKey, PrincipalId};
use tokio::task::JoinHandle;

const NONCE: [u8; 32] = [0x5D; 32];

/// First nonce a spawned server issues: the sequence byte replaces
/// `NONCE[0]`, so the first connection sees `[1, ...]`.
fn first_issued_nonce() -> [u8; 32] {
    let mut issued = NONCE;
    issued[0] = 1;
    issued
}

struct TempRoot(PathBuf);

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

impl TempRoot {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "nlos-ipc-handshake-transport-{label}-{}-{nonce}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        )))
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Short socket path: macOS `SUN_LEN` caps socket paths at 104 bytes, and
/// the shared temp dir prefix alone consumes ~50 of them.
struct SocketPath(PathBuf);

static NEXT_SOCKET: AtomicU64 = AtomicU64::new(1);

impl SocketPath {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "nlos-ipc-t-{label}-{}-{nonce}-{}",
            std::process::id(),
            NEXT_SOCKET.fetch_add(1, Ordering::Relaxed)
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
        let _ = std::fs::remove_file(&self.0);
    }
}

fn config() -> TransportConfig {
    TransportConfig::new(
        4_096,
        Duration::from_secs(5),
        Duration::from_secs(5),
        Duration::from_secs(5),
    )
    .unwrap()
}

fn short_read_config() -> TransportConfig {
    TransportConfig::new(
        4_096,
        Duration::from_secs(5),
        Duration::from_millis(100),
        Duration::from_secs(5),
    )
    .unwrap()
}

fn bootstrap(seed: u8) -> (TempRoot, IdentityAuthority, SigningKey, IdentityBinding) {
    let root = TempRoot::new("root");
    let identity = IdentityAuthority::open(&root.0).unwrap();
    let key = SigningKey::from_bytes(&[seed; 32]);
    let BootstrapDecision::Created(binding) = identity
        .bootstrap_principal(BootstrapPrincipalRequest {
            principal_profile_digest: [seed.wrapping_add(1); 32],
            control_domain_policy_digest: [seed.wrapping_add(2); 32],
            public_key: key.verifying_key().to_bytes(),
            key_purpose: KeyPurpose::SemanticSigning,
            key_valid_from_ms: 0,
            key_valid_until_ms: 10_000,
            idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(3); 16]),
            created_at_ms: 0,
        })
        .unwrap()
    else {
        unreachable!("fresh authority bootstraps a new principal");
    };
    (root, identity, key, binding)
}

fn allow() -> impl Fn(&PeerIdentity) -> Result<(), String> + Copy {
    |_: &PeerIdentity| -> Result<(), String> { Ok(()) }
}

fn deny() -> impl Fn(&PeerIdentity) -> Result<(), String> + Copy {
    |_: &PeerIdentity| -> Result<(), String> { Err("pre-gate denies this peer".to_owned()) }
}

fn echo_handler()
-> impl Fn(ValidatedExchangeRequest) -> std::future::Ready<Result<OutboundResponse, IpcError>> + Copy
{
    |validated| {
        std::future::ready(Ok(OutboundResponse::Typed(ExchangeResponse {
            envelope: Some(validated.envelope().clone()),
        })))
    }
}

fn failing_handler()
-> impl Fn(ValidatedExchangeRequest) -> std::future::Ready<Result<OutboundResponse, IpcError>> + Copy
{
    |_validated| {
        std::future::ready(Err(IpcError::ServiceFailure(
            "handler declined after handshake",
        )))
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_serving_n<A, H, F>(
    listener: UnixListenerAdapter,
    transport: TransportConfig,
    identity: IdentityAuthority,
    nonces: Arc<HandshakeNonceRegistry>,
    binding: Vec<u8>,
    authorizer: A,
    handler: H,
    nonce: [u8; 32],
    connections: usize,
) -> JoinHandle<Vec<Result<AuthenticatedServeOutcome, HandshakeError>>>
where
    A: PeerAuthorizer + Copy + Send + Sync + 'static,
    H: Fn(ValidatedExchangeRequest) -> F + Copy + Send + 'static,
    F: Future<Output = Result<OutboundResponse, IpcError>> + Send,
{
    tokio::spawn(async move {
        let sequence = std::sync::atomic::AtomicU8::new(0);
        let mut outcomes = Vec::new();
        for _ in 0..connections {
            let next_nonce = || {
                let mut issued = nonce;
                issued[0] = sequence.fetch_add(1, Ordering::Relaxed) + 1;
                issued
            };
            outcomes.push(
                authenticated_serve_one(
                    &listener,
                    transport,
                    &identity,
                    &nonces,
                    &binding,
                    &authorizer,
                    handler,
                    next_nonce,
                    5_000,
                )
                .await,
            );
        }
        outcomes
    })
}

fn authenticated_request() -> ExchangeRequest {
    ExchangeRequest {
        envelope: Some(Envelope {
            schema: Some(SchemaIdentity {
                name: SABI_ENVELOPE_SCHEMA.to_owned(),
                major: 1,
                minor: 0,
                critical_extension_ids: Vec::new(),
                non_critical_extension_ids: Vec::new(),
            }),
            request_id: vec![9; 16],
            service: "operation".to_owned(),
            method: "get".to_owned(),
            common_context: None,
            payload: b"authenticated".to_vec(),
        }),
    }
}

#[test]
fn endpoint_channel_binding_is_deterministic_and_bounded() {
    let a = Path::new("/run/nlos/a.sock");
    let b = Path::new("/run/nlos/b.sock");
    assert_eq!(endpoint_channel_binding(a), endpoint_channel_binding(a));
    assert_ne!(endpoint_channel_binding(a), endpoint_channel_binding(b));
    assert_eq!(endpoint_channel_binding(a).len(), 32);
}

#[test]
fn server_context_derives_binding_and_holds_the_registry() {
    let root = TempRoot::new("ctx");
    let path = root.0.join("svc.sock");
    let ctx = ServerHandshakeContext::new(&path, 4).unwrap();
    assert_eq!(ctx.binding(), endpoint_channel_binding(&path));
    ctx.nonces().register([7_u8; 32]).unwrap();
    assert!(matches!(
        ctx.nonces().register([7_u8; 32]),
        Err(HandshakeError::NonceRejected)
    ));
    assert!(matches!(
        ServerHandshakeContext::new(&path, 0),
        Err(HandshakeError::InvalidConfig(
            "nonce registry capacity must be non-zero"
        ))
    ));
}

#[tokio::test]
async fn authenticated_chain_over_real_unix_socket() {
    let _root = TempRoot::new("chain");
    let path = SocketPath::new("chain");
    let (_identity_root, identity, key, principal) = bootstrap(0x31);
    let ctx = ServerHandshakeContext::new(&path, 8).unwrap();
    let listener = UnixListenerAdapter::bind(&path).unwrap();
    let allow = allow();

    let server = authenticated_serve_one(
        &listener,
        config(),
        &identity,
        ctx.nonces(),
        ctx.binding(),
        &allow,
        echo_handler(),
        || NONCE,
        5_000,
    );
    let client = async {
        let framed = authenticated_connect(
            &path,
            config(),
            principal.principal_id,
            |digest: &[u8; 32]| Ok(key.sign(digest).to_bytes()),
        )
        .await
        .unwrap();
        LocalRpcClient::new(framed.into_inner(), config())
            .exchange_validated(authenticated_request())
            .await
            .unwrap()
    };

    let (outcome, response) = tokio::join!(server, client);
    let outcome = outcome.unwrap();
    assert_eq!(outcome.verified().principal_id(), principal.principal_id);
    assert_eq!(outcome.verified().key_id(), principal.key_id);
    assert_eq!(
        outcome.verified().key_generation(),
        principal.key_generation
    );
    assert!(outcome.served().is_ok());
    assert_eq!(response.envelope().request_id, vec![9; 16]);

    // The single-use handshake nonce was consumed and is not returned.
    assert!(matches!(
        ctx.nonces().consume(&NONCE),
        Err(HandshakeError::NonceRejected)
    ));
}

#[tokio::test]
async fn bad_signature_rejects_the_connection_without_serving() {
    let _root = TempRoot::new("bad-sig");
    let path = SocketPath::new("bad-sig");
    let (_identity_root, identity, _key, principal) = bootstrap(0x32);
    let nonces = Arc::new(HandshakeNonceRegistry::new(8).unwrap());
    let binding = endpoint_channel_binding(&path).to_vec();
    let listener = UnixListenerAdapter::bind(&path).unwrap();

    let server = spawn_serving_n(
        listener,
        config(),
        identity,
        Arc::clone(&nonces),
        binding,
        allow(),
        echo_handler(),
        NONCE,
        1,
    );
    let mut framed = authenticated_connect(
        &path,
        config(),
        principal.principal_id,
        |_digest: &[u8; 32]| Ok([0_u8; 64]),
    )
    .await
    .unwrap();
    let client_error = framed.receive().await.unwrap_err();
    let outcomes = server.await.unwrap();
    assert_eq!(outcomes.len(), 1);
    assert!(
        matches!(&outcomes[0], Err(HandshakeError::SignatureInvalid)),
        "unexpected outcome: {:?}",
        outcomes[0]
    );
    // The server closed the connection before serving; the client observes EOF.
    assert!(matches!(
        client_error,
        IpcError::Io {
            operation: IoOperation::Read,
            ..
        }
    ));
    // The burned nonce was consumed before verification and never returned.
    assert!(matches!(
        nonces.consume(&first_issued_nonce()),
        Err(HandshakeError::NonceRejected)
    ));
}

#[tokio::test]
async fn replayed_attestation_fails_the_second_connection() {
    let _root = TempRoot::new("replay");
    let path = SocketPath::new("replay");
    let (_identity_root, identity, key, principal) = bootstrap(0x33);
    let nonces = Arc::new(HandshakeNonceRegistry::new(8).unwrap());
    let binding = endpoint_channel_binding(&path).to_vec();
    let listener = UnixListenerAdapter::bind(&path).unwrap();

    let server = spawn_serving_n(
        listener,
        config(),
        identity,
        Arc::clone(&nonces),
        binding.clone(),
        allow(),
        echo_handler(),
        NONCE,
        2,
    );

    // Connection 1: honest client answers the first challenge; its
    // attestation bytes are captured for the replay attempt below.
    let (captured_nonce, replayed_attestation) = {
        let (stream, _peer) = connect(&path, config()).await.unwrap();
        let mut framed = FramedIo::new(stream, config());
        let challenge = decode_challenge_wire(&framed.receive().await.unwrap()).unwrap();
        assert_eq!(challenge.nonce, first_issued_nonce().to_vec());
        let signature = key
            .sign(&principal_handshake_message(
                &first_issued_nonce(),
                principal.principal_id,
                &binding,
            ))
            .to_bytes();
        let attestation =
            client_attestation(principal.principal_id, &challenge, &binding, signature).unwrap();
        let wire = encode_attestation_wire(&attestation).unwrap();
        framed.send(&wire).await.unwrap();
        (first_issued_nonce(), wire)
    };

    // Connection 2: the server already issued a fresh nonce, so replaying
    // connection 1's attestation bytes verbatim must fail closed on the
    // consumed nonce. The client observes the connection being dropped.
    let second_error = {
        let (stream, _peer) = connect(&path, config()).await.unwrap();
        let mut framed = FramedIo::new(stream, config());
        let challenge = decode_challenge_wire(&framed.receive().await.unwrap()).unwrap();
        assert_ne!(challenge.nonce, captured_nonce.to_vec());
        framed.send(&replayed_attestation).await.unwrap();
        framed.receive().await.unwrap_err()
    };

    let mut outcomes = server.await.unwrap();
    assert!(outcomes[0].is_ok(), "first connection: {:?}", outcomes[0]);
    let replay_outcome = outcomes.remove(1);
    assert!(
        matches!(&replay_outcome, Err(HandshakeError::NonceRejected)),
        "unexpected replay outcome: {replay_outcome:?}"
    );
    assert!(matches!(
        &second_error,
        IpcError::Io {
            operation: IoOperation::Read,
            ..
        }
    ));
    // The replayed attestation's nonce was consumed by connection 1 and was
    // never returned to the registry.
    assert!(matches!(
        nonces.consume(&first_issued_nonce()),
        Err(HandshakeError::NonceRejected)
    ));
}

#[tokio::test]
async fn channel_binding_mismatch_fails_closed_without_burning_the_nonce() {
    let _root = TempRoot::new("binding");
    let path = SocketPath::new("binding");
    let (_identity_root, identity, key, principal) = bootstrap(0x34);
    let nonces = Arc::new(HandshakeNonceRegistry::new(8).unwrap());
    let wrong_binding = endpoint_channel_binding(Path::new("/other/endpoint.sock")).to_vec();
    assert_ne!(wrong_binding, endpoint_channel_binding(&path).to_vec());
    let listener = UnixListenerAdapter::bind(&path).unwrap();

    let server = spawn_serving_n(
        listener,
        config(),
        identity,
        Arc::clone(&nonces),
        wrong_binding,
        allow(),
        echo_handler(),
        NONCE,
        1,
    );
    // The server only rejects after the client already sent its attestation,
    // so the connect itself succeeds and the EOF surfaces on the next read.
    let mut framed = authenticated_connect(
        &path,
        config(),
        principal.principal_id,
        |digest: &[u8; 32]| Ok(key.sign(digest).to_bytes()),
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
        &client_error,
        IpcError::Io {
            operation: IoOperation::Read,
            ..
        }
    ));
    // A binding mismatch is rejected before nonce consumption, so the
    // honest nonce is still valid and consumable afterwards.
    nonces.consume(&first_issued_nonce()).unwrap();
}

#[tokio::test]
async fn unknown_principal_fails_closed() {
    let _root = TempRoot::new("stranger");
    let path = SocketPath::new("stranger");
    let (_identity_root, identity, _key, _principal) = bootstrap(0x35);
    let stranger = PrincipalId::from_bytes([0xEE; 16]);
    let stranger_key = SigningKey::from_bytes(&[0x35; 32]);
    let nonces = Arc::new(HandshakeNonceRegistry::new(8).unwrap());
    let binding = endpoint_channel_binding(&path).to_vec();
    let listener = UnixListenerAdapter::bind(&path).unwrap();

    let server = spawn_serving_n(
        listener,
        config(),
        identity,
        Arc::clone(&nonces),
        binding,
        allow(),
        echo_handler(),
        NONCE,
        1,
    );
    let _ = authenticated_connect(&path, config(), stranger, |digest: &[u8; 32]| {
        Ok(stranger_key.sign(digest).to_bytes())
    })
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
async fn out_of_band_frame_during_handshake_fails_closed() {
    let _root = TempRoot::new("oob");
    let path = SocketPath::new("oob");
    let (_identity_root, identity, _key, _principal) = bootstrap(0x36);
    let nonces = Arc::new(HandshakeNonceRegistry::new(8).unwrap());
    let binding = endpoint_channel_binding(&path).to_vec();
    let listener = UnixListenerAdapter::bind(&path).unwrap();

    let server = spawn_serving_n(
        listener,
        config(),
        identity,
        Arc::clone(&nonces),
        binding,
        allow(),
        echo_handler(),
        NONCE,
        1,
    );
    let (stream, _peer) = connect(&path, config()).await.unwrap();
    let mut framed = FramedIo::new(stream, config());
    let _challenge = framed.receive().await.unwrap();
    // Any non-attestation frame during the handshake must be a typed
    // rejection, here a well-formed ExchangeRequest smuggled in early.
    framed
        .send(&encode_exchange_request(&authenticated_request()).unwrap())
        .await
        .unwrap();

    let outcomes = server.await.unwrap();
    assert!(
        matches!(&outcomes[0], Err(HandshakeError::Schema(_))),
        "unexpected outcome: {:?}",
        outcomes[0]
    );
}

#[tokio::test]
async fn pre_gate_denial_happens_before_any_handshake_side_effect() {
    let _root = TempRoot::new("pre-gate");
    let path = SocketPath::new("pre-gate");
    let (_identity_root, identity, key, principal) = bootstrap(0x37);
    let nonces = Arc::new(HandshakeNonceRegistry::new(8).unwrap());
    let binding = endpoint_channel_binding(&path).to_vec();
    let listener = UnixListenerAdapter::bind(&path).unwrap();

    let server = spawn_serving_n(
        listener,
        config(),
        identity,
        Arc::clone(&nonces),
        binding,
        deny(),
        echo_handler(),
        NONCE,
        1,
    );
    let client_error = authenticated_connect(
        &path,
        config(),
        principal.principal_id,
        |digest: &[u8; 32]| Ok(key.sign(digest).to_bytes()),
    )
    .await
    .err()
    .expect("denied client handshake unexpectedly succeeded");

    let outcomes = server.await.unwrap();
    assert!(
        matches!(
            &outcomes[0],
            Err(HandshakeError::PeerAuthorization(reason))
                if reason.contains("pre-gate denies this peer")
        ),
        "unexpected outcome: {:?}",
        outcomes[0]
    );
    assert!(matches!(
        &client_error,
        HandshakeError::Transport(IpcError::Io {
            operation: IoOperation::Read,
            ..
        })
    ));
    // Zero side effects: the challenge (and its nonce) was never issued.
    nonces.register(NONCE).unwrap();
}

#[tokio::test]
async fn silent_client_times_out_fail_closed() {
    let _root = TempRoot::new("timeout");
    let path = SocketPath::new("timeout");
    let (_identity_root, identity, _key, _principal) = bootstrap(0x38);
    let nonces = Arc::new(HandshakeNonceRegistry::new(8).unwrap());
    let binding = endpoint_channel_binding(&path).to_vec();
    let listener = UnixListenerAdapter::bind(&path).unwrap();

    let server = spawn_serving_n(
        listener,
        short_read_config(),
        identity,
        Arc::clone(&nonces),
        binding,
        allow(),
        echo_handler(),
        NONCE,
        1,
    );
    let (stream, _peer) = connect(&path, config()).await.unwrap();
    let mut framed = FramedIo::new(stream, config());
    let _challenge = framed.receive().await.unwrap();
    // Never answer; the server's bounded read must time out.
    tokio::time::sleep(Duration::from_millis(300)).await;

    let outcomes = server.await.unwrap();
    assert!(
        matches!(
            &outcomes[0],
            Err(HandshakeError::Transport(IpcError::Timeout(
                IoOperation::Read
            )))
        ),
        "unexpected outcome: {:?}",
        outcomes[0]
    );
}

#[tokio::test]
async fn served_phase_error_surfaces_after_verified_handshake() {
    let _root = TempRoot::new("served");
    let path = SocketPath::new("served");
    let (_identity_root, identity, key, principal) = bootstrap(0x39);
    let nonces = Arc::new(HandshakeNonceRegistry::new(8).unwrap());
    let binding = endpoint_channel_binding(&path).to_vec();
    let listener = UnixListenerAdapter::bind(&path).unwrap();

    let server = spawn_serving_n(
        listener,
        config(),
        identity,
        Arc::clone(&nonces),
        binding,
        allow(),
        failing_handler(),
        NONCE,
        1,
    );
    let framed = authenticated_connect(
        &path,
        config(),
        principal.principal_id,
        |digest: &[u8; 32]| Ok(key.sign(digest).to_bytes()),
    )
    .await
    .unwrap();
    let exchange = LocalRpcClient::new(framed.into_inner(), config())
        .exchange_validated(authenticated_request())
        .await;

    let mut outcomes = server.await.unwrap();
    let (verified, serve_result) = outcomes.remove(0).unwrap().into_parts();
    assert_eq!(verified.principal_id(), principal.principal_id);
    // serve_one keeps its existing semantics: a handler error aborts the
    // exchange without sending a failure response, so the client observes EOF.
    assert!(matches!(
        serve_result,
        Err(IpcError::ServiceFailure("handler declined after handshake"))
    ));
    assert!(matches!(
        &exchange,
        Err(IpcError::Io {
            operation: IoOperation::Read,
            ..
        })
    ));
}
