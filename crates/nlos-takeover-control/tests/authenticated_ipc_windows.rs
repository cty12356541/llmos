#![cfg(all(windows, feature = "authenticated-ipc"))]

//! Minimal Windows named-pipe loopback for the authenticated
//! `TakeoverControl` serving variant (mirrors the Unix
//! `authenticated_ipc` roundtrip test): this crate's authenticated serving
//! entry over a real local pipe with genuine `IdentityAuthority`
//! verification at the `AuthorityClock`'s committed wall reading, driving
//! one real barrier-observation submission to a durable, locally covered
//! receipt.
//!
//! The transport handshake principal is deliberately a different principal
//! (and key purpose) from the barrier-observation signer, so every assertion
//! exercises the documented two-signature-layer model.

mod authenticated_support;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use authenticated_support::{
    AllowPeer, CapabilityPolicy, Fixture, NONCE, durable_rows, first_issued_nonce, observation,
    submit_request, transport_config,
};
use ed25519_dalek::Signer;
use nlos_clock::AuthorityClock;
use nlos_identity::IdentityAuthority;
use nlos_ipc::handshake::HandshakeError;
use nlos_ipc::handshake::transport::{
    AuthenticatedServeOutcome, ServerHandshakeContext, authenticated_connect,
};
use nlos_ipc::windows::NamedPipeListenerAdapter;
use nlos_ipc::{LocalRpcClient, TransportConfig};
use nlos_schema::sabi::v1::ExchangeRequest;
use nlos_schema::{
    MethodSemantics, decode_barrier_observation_record, validate_sabi_response_context,
};
use nlos_takeover_control::authenticated::{AuthenticatedIpcError, AuthenticatedTakeoverControl};
use nlos_task::{AuthorityTakeoverBarrierCoverageState, SqliteTaskAuthority};
use tokio::task::JoinHandle;

fn pipe_path(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("monotonic clock")
        .as_nanos();
    PathBuf::from(format!(
        r"\\.\pipe\nlos-takeover-nta-{label}-{}-{nonce}",
        std::process::id()
    ))
}

#[allow(clippy::too_many_arguments)]
fn spawn_authenticated_serve(
    mut listener: NamedPipeListenerAdapter,
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
async fn authenticated_observation_roundtrip_over_real_windows_named_pipe() {
    let fixture = Fixture::new("winround");
    let path = pipe_path("round");
    let context = Arc::new(ServerHandshakeContext::new(&path, 8).unwrap());
    // One served connection; `accept` retains one spare listening instance,
    // so the instance floor of 2 is exactly met.
    let listener = NamedPipeListenerAdapter::bind(&path, 2, transport_config()).unwrap();
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
        &path,
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
        .expect("barrier observation crosses the authenticated pipe path");

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
