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

mod authenticated_support;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use authenticated_support::{
    AllowPeer, CapabilityPolicy, Fixture, NONCE, durable_rows, first_issued_nonce, next_sequence,
    observation, submit_request, transport_config,
};
use ed25519_dalek::Signer;
use nlos_clock::AuthorityClock;
use nlos_identity::IdentityAuthority;
use nlos_ipc::handshake::transport::{
    AuthenticatedServeOutcome, ServerHandshakeContext, authenticated_connect,
};
use nlos_ipc::handshake::{
    HandshakeError, client_attestation, decode_challenge_wire, encode_attestation_wire,
    principal_handshake_message,
};
use nlos_ipc::unix::{UnixListenerAdapter, connect};
use nlos_ipc::{FramedIo, IoOperation, IpcError, LocalRpcClient, TransportConfig};
use nlos_schema::sabi::v1::ExchangeRequest;
use nlos_schema::{
    MethodSemantics, decode_barrier_observation_record, validate_sabi_response_context,
};
use nlos_takeover_control::authenticated::{AuthenticatedIpcError, AuthenticatedTakeoverControl};
use nlos_task::{AuthorityTakeoverBarrierCoverageState, SqliteTaskAuthority};
use tokio::task::JoinHandle;

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
            next_sequence()
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

#[allow(clippy::too_many_arguments)]
fn spawn_authenticated_serve(
    mut listener: UnixListenerAdapter,
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
                    .serve_one(&mut listener, transport, &AllowPeer, 10, 6_000, next_nonce)
                    .await,
            );
        }
        outcomes
    })
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
    let stranger_key = ed25519_dalek::SigningKey::from_bytes(&[0x77; 32]);
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
