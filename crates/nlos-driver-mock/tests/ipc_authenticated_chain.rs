#![cfg(unix)]

//! Authenticated mock driver IPC: a real Unix-socket full chain (ADR-0011
//! handshake → verified principal → register/dispatch/complete), provider
//! restart replay over IPC, and the fail-closed authentication matrix
//! (unknown principal, bad signature, replayed attestation). The in-process
//! core chain is covered by `provider_core.rs` on every platform.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use ed25519_dalek::{Signer, SigningKey};
use nlos_clock::{AuthorityClock, NowRequest};
use nlos_driver_mock::authenticated::{
    AuthenticatedMockDriverError, AuthenticatedMockDriverServer,
};
use nlos_driver_mock::codec::{
    self, CompleteOperationWire, DispatchOperationResultWire, DispatchOperationWire,
    RegisterOperationResultWire, RegisterOperationWire,
};
use nlos_driver_mock::ipc::{
    COMPLETE_OPERATION_METHOD, DISPATCH_OPERATION_METHOD, MOCK_DRIVER_SERVICE,
    MockDriverAuthorizer, REGISTER_OPERATION_METHOD,
};
use nlos_driver_mock::{MockProvider, derive_provider_outcome};
use nlos_identity::{BootstrapDecision, BootstrapPrincipalRequest, IdentityAuthority, KeyPurpose};
use nlos_ipc::handshake::transport::{ServerHandshakeContext, authenticated_connect};
use nlos_ipc::handshake::{
    HandshakeError, client_attestation, decode_challenge_wire, encode_attestation_wire,
    principal_handshake_message,
};
use nlos_ipc::unix::{UnixListenerAdapter, connect};
use nlos_ipc::{FramedIo, LocalRpcClient, PeerAuthorizer, PeerIdentity, TransportConfig};
use nlos_schema::SABI_ENVELOPE_SCHEMA;
use nlos_schema::sabi::v1::{
    CallerIdentity, CapabilityHandle, Envelope, ExchangeRequest, SabiRequestContext,
    SchemaIdentity, envelope,
};
use nlos_store::SqliteOperationStore;
use nlos_types::{Generation, IdempotencyKey, PrincipalId};

/// Epoch-ms of 2100-01-01: a "not expiring" key window that still fits the
/// identity authority's `SQLite` i64 encoding.
const KEY_VALID_UNTIL_MS: u64 = 4_102_444_800_000;

const REGISTER_WIRE: RegisterOperationWire = RegisterOperationWire {
    operation_id: [0x51; 16],
    operation_generation: 1,
    owner_fiber_id: [0x52; 16],
    owner_fiber_generation: 1,
    cancellation_scope_id: [0x53; 16],
    cancellation_generation: 1,
};

const COMPLETE_SEED: [u8; 32] = [0x77; 32];

struct Root(PathBuf);

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

impl Root {
    fn new(label: &str) -> Self {
        let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        let root = Self(std::env::temp_dir().join(format!(
            "nlos-driver-mock-ipc-{label}-{}-{sequence}",
            std::process::id()
        )));
        fs::create_dir_all(root.path()).unwrap();
        root
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
            "nlos-dm-ipc-{label}-{}-{sequence}",
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

struct AllowDriver;

impl MockDriverAuthorizer for AllowDriver {
    fn authorize_register(
        &self,
        _: PrincipalId,
        _: &SabiRequestContext,
    ) -> Result<(), &'static str> {
        Ok(())
    }

    fn authorize_dispatch(
        &self,
        _: PrincipalId,
        _: &SabiRequestContext,
    ) -> Result<(), &'static str> {
        Ok(())
    }

    fn authorize_complete(
        &self,
        _: PrincipalId,
        _: &SabiRequestContext,
    ) -> Result<(), &'static str> {
        Ok(())
    }
}

struct Principal {
    identity: IdentityAuthority,
    signing: SigningKey,
    id: PrincipalId,
}

fn bootstrap(root: &Root, seed: u8) -> Principal {
    let identity = IdentityAuthority::open(root.path().join("identity")).unwrap();
    let signing = SigningKey::from_bytes(&[seed; 32]);
    let BootstrapDecision::Created(binding) = identity
        .bootstrap_principal(BootstrapPrincipalRequest {
            principal_profile_digest: [seed.wrapping_add(1); 32],
            control_domain_policy_digest: [seed.wrapping_add(2); 32],
            public_key: signing.verifying_key().to_bytes(),
            key_purpose: KeyPurpose::SemanticSigning,
            key_valid_from_ms: 0,
            key_valid_until_ms: KEY_VALID_UNTIL_MS,
            idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(3); 16]),
            created_at_ms: 0,
        })
        .unwrap()
    else {
        unreachable!("fresh identity authority bootstraps a new principal");
    };
    Principal {
        identity,
        signing,
        id: binding.principal_id,
    }
}

fn clock_with_advanced_wall(root: &Root) -> AuthorityClock {
    let clock = AuthorityClock::open(root.path().join("clock")).unwrap();
    clock
        .wall_now(NowRequest {
            idempotency_key: IdempotencyKey::from_bytes([0xEE; 16]),
        })
        .unwrap();
    clock
}

fn open_provider(root: &Root) -> Arc<MockProvider> {
    Arc::new(MockProvider::new(Arc::new(
        SqliteOperationStore::open(root.path().join("ops")).unwrap(),
    )))
}

fn request_envelope(method: &str, payload: Vec<u8>, request_seed: u8) -> ExchangeRequest {
    ExchangeRequest {
        envelope: Some(Envelope {
            schema: Some(SchemaIdentity {
                name: SABI_ENVELOPE_SCHEMA.to_owned(),
                major: 1,
                minor: 1,
                critical_extension_ids: Vec::new(),
                non_critical_extension_ids: Vec::new(),
            }),
            request_id: vec![request_seed; 16],
            service: MOCK_DRIVER_SERVICE.to_owned(),
            method: method.to_owned(),
            common_context: Some(envelope::CommonContext::RequestContext(
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
                    idempotency_key: vec![request_seed; 16],
                    deadline_monotonic_ns: 0,
                    capability_handles: vec![CapabilityHandle {
                        slot: 9,
                        generation: 1,
                    }],
                    reservation_handle: None,
                    proposal_or_input_digest_sha256: Vec::new(),
                },
            )),
            payload,
        }),
    }
}

fn register_request(request_seed: u8) -> ExchangeRequest {
    request_envelope(
        REGISTER_OPERATION_METHOD,
        codec::encode_register_request(&REGISTER_WIRE).unwrap(),
        request_seed,
    )
}

fn dispatch_request(request_seed: u8, callback_id: [u8; 16]) -> ExchangeRequest {
    request_envelope(
        DISPATCH_OPERATION_METHOD,
        codec::encode_dispatch_request(&DispatchOperationWire {
            operation_id: REGISTER_WIRE.operation_id,
            operation_generation: REGISTER_WIRE.operation_generation,
            callback_id,
        })
        .unwrap(),
        request_seed,
    )
}

fn complete_request(request_seed: u8, callback_id: [u8; 16]) -> ExchangeRequest {
    request_envelope(
        COMPLETE_OPERATION_METHOD,
        codec::encode_complete_request(&CompleteOperationWire {
            operation_id: REGISTER_WIRE.operation_id,
            operation_generation: REGISTER_WIRE.operation_generation,
            callback_id,
            seed: COMPLETE_SEED,
        })
        .unwrap(),
        request_seed,
    )
}

/// Binds the endpoint synchronously, then serves exactly `cycles`
/// authenticated connections with a deterministic nonce sequence, returning
/// every serve outcome in order.
fn spawn_server(
    provider: Arc<MockProvider>,
    identity: IdentityAuthority,
    clock: AuthorityClock,
    socket: &Path,
    cycles: usize,
) -> tokio::task::JoinHandle<Vec<Result<bool, AuthenticatedMockDriverError>>> {
    let listener = UnixListenerAdapter::bind(socket).unwrap();
    let handshake = ServerHandshakeContext::new(socket, 16).unwrap();
    let server =
        AuthenticatedMockDriverServer::new(provider, AllowDriver, identity, clock, handshake);
    tokio::spawn(async move {
        let mut outcomes = Vec::new();
        for cycle in 0..cycles {
            let mut nonce = [0x5D_u8; 32];
            nonce[0] = u8::try_from(cycle + 1).unwrap();
            let outcome = server
                .serve_one(&listener, transport_config(), &AllowPeer, 0, || nonce)
                .await;
            outcomes.push(outcome.map(|served| served.served().is_ok()));
        }
        outcomes
    })
}

async fn exchange_authenticated(
    socket: &Path,
    principal_id: PrincipalId,
    signing: &SigningKey,
    request: ExchangeRequest,
) -> nlos_schema::sabi::v1::Envelope {
    let framed = authenticated_connect(
        socket,
        transport_config(),
        principal_id,
        |digest: &[u8; 32]| Ok(signing.sign(digest).to_bytes()),
    )
    .await
    .unwrap();
    let client = LocalRpcClient::new(framed.into_inner(), transport_config());
    let validated = client.exchange_validated(request).await.unwrap();
    validated.envelope().clone()
}

fn assert_success(envelope: &Envelope) {
    match envelope.common_context.as_ref() {
        Some(envelope::CommonContext::ResponseContext(context)) => {
            assert!(
                context.failure.is_none(),
                "driver request must succeed, got {context:?}"
            );
        }
        other => panic!("driver response lacks a response context: {other:?}"),
    }
}

async fn register_over_ipc(
    socket: &Path,
    principal_id: PrincipalId,
    signing: &SigningKey,
) -> RegisterOperationResultWire {
    let response = exchange_authenticated(socket, principal_id, signing, register_request(1)).await;
    assert_success(&response);
    let result = codec::decode_register_result(&response.payload).unwrap();
    assert!(!result.replayed);
    assert_eq!(result.operation_id, REGISTER_WIRE.operation_id);
    assert_ne!(
        result.admission_receipt_id, [0; 16],
        "authority-derived admission receipt must be non-zero"
    );
    match response.common_context.as_ref() {
        Some(envelope::CommonContext::ResponseContext(context)) => {
            assert_eq!(context.receipts.len(), 1);
            assert_eq!(
                context.receipts[0].receipt_id,
                result.admission_receipt_id.to_vec()
            );
        }
        other => panic!("register response lacks a response context: {other:?}"),
    }
    result
}

async fn dispatch_over_ipc(
    socket: &Path,
    principal_id: PrincipalId,
    signing: &SigningKey,
    callback: [u8; 16],
    request_seed: u8,
) -> DispatchOperationResultWire {
    let response = exchange_authenticated(
        socket,
        principal_id,
        signing,
        dispatch_request(request_seed, callback),
    )
    .await;
    assert_success(&response);
    let result = codec::decode_dispatch_result(&response.payload).unwrap();
    assert_eq!(result.callback_id, callback);
    match response.common_context.as_ref() {
        Some(envelope::CommonContext::ResponseContext(context)) => {
            assert_eq!(context.receipts.len(), 2);
            assert_eq!(
                context.receipts[0].receipt_id,
                result.preparation_receipt_id.to_vec()
            );
            assert_eq!(
                context.receipts[1].receipt_id,
                result.activation_receipt_id.to_vec()
            );
        }
        other => panic!("dispatch response lacks a response context: {other:?}"),
    }
    result
}

#[tokio::test]
async fn authenticated_ipc_full_chain_register_dispatch_complete() {
    let root = Root::new("chain");
    let socket = SocketPath::new("chain");
    let principal = bootstrap(&root, 0x41);
    let clock = clock_with_advanced_wall(&root);
    let provider = open_provider(&root);

    let server_task = spawn_server(
        Arc::clone(&provider),
        principal.identity,
        clock,
        socket.path(),
        3,
    );
    let callback = [0x61; 16];

    register_over_ipc(socket.path(), principal.id, &principal.signing).await;

    let dispatch_result =
        dispatch_over_ipc(socket.path(), principal.id, &principal.signing, callback, 2).await;
    assert!(!dispatch_result.replayed);
    assert_eq!(dispatch_result.cancel_epoch, 0);

    let complete_response = exchange_authenticated(
        socket.path(),
        principal.id,
        &principal.signing,
        complete_request(3, callback),
    )
    .await;
    assert_success(&complete_response);
    let complete_result = codec::decode_complete_result(&complete_response.payload).unwrap();
    assert!(!complete_result.replayed);
    let handle = nlos_operation::OperationHandle {
        operation_id: nlos_types::OperationId::from_bytes(REGISTER_WIRE.operation_id),
        generation: Generation::INITIAL,
    };
    let expected = derive_provider_outcome(
        handle,
        nlos_types::CallbackId::from_bytes(callback),
        &COMPLETE_SEED,
    );
    assert_eq!(
        complete_result.outcome_code,
        codec::outcome_wire_code(expected)
    );
    let expected_receipt = match expected {
        nlos_operation::CompletionOutcome::Completed { receipt_id }
        | nlos_operation::CompletionOutcome::Failed { receipt_id }
        | nlos_operation::CompletionOutcome::CancelledBeforeEffect { receipt_id }
        | nlos_operation::CompletionOutcome::PartialEffect { receipt_id }
        | nlos_operation::CompletionOutcome::EffectUnknown { receipt_id } => receipt_id,
    };
    assert_eq!(complete_result.receipt_id, *expected_receipt.as_bytes());
    assert!(
        codec::state_from_wire_code(complete_result.state_code)
            .unwrap()
            .is_terminal()
    );

    let outcomes = server_task.await.unwrap();
    for outcome in outcomes {
        assert!(outcome.unwrap());
    }
    // The durable authority agrees with the wire result.
    let snapshot = provider.store().inspect(handle).unwrap();
    assert!(snapshot.state.is_terminal());
}

#[tokio::test]
async fn authenticated_ipc_restart_replays_in_flight_dispatch_exactly() {
    let root = Root::new("restart");
    let socket = SocketPath::new("restart");
    let callback = [0x62; 16];

    // Phase 1: provider process registers and dispatches over IPC, then the
    // whole provider process (store, identity, clock, listener) is dropped.
    let principal = bootstrap(&root, 0x42);
    let clock = clock_with_advanced_wall(&root);
    let provider = open_provider(&root);
    let phase_one = spawn_server(
        Arc::clone(&provider),
        principal.identity,
        clock,
        socket.path(),
        2,
    );
    register_over_ipc(socket.path(), principal.id, &principal.signing).await;
    let first_dispatch =
        dispatch_over_ipc(socket.path(), principal.id, &principal.signing, callback, 2).await;
    assert!(!first_dispatch.replayed);
    for outcome in phase_one.await.unwrap() {
        assert!(outcome.unwrap());
    }
    drop(provider);

    // Provider restart: every authority is reopened from disk; the endpoint
    // is rebound on the same path (same channel binding).
    fs::remove_file(socket.path()).unwrap();
    let identity = IdentityAuthority::open(root.path().join("identity")).unwrap();
    let clock = clock_with_advanced_wall(&root);
    let provider = open_provider(&root);
    let phase_two = spawn_server(provider, identity, clock, socket.path(), 3);

    let replayed_dispatch =
        dispatch_over_ipc(socket.path(), principal.id, &principal.signing, callback, 3).await;
    assert_eq!(
        DispatchOperationResultWire {
            replayed: true,
            ..replayed_dispatch
        },
        DispatchOperationResultWire {
            replayed: true,
            ..first_dispatch
        },
        "restart must replay the exact ticket and durable receipts"
    );

    let completion_response = exchange_authenticated(
        socket.path(),
        principal.id,
        &principal.signing,
        complete_request(4, callback),
    )
    .await;
    assert_success(&completion_response);
    let completion = codec::decode_complete_result(&completion_response.payload).unwrap();
    assert!(!completion.replayed);

    let replayed_completion_response = exchange_authenticated(
        socket.path(),
        principal.id,
        &principal.signing,
        complete_request(5, callback),
    )
    .await;
    assert_success(&replayed_completion_response);
    let replayed_completion =
        codec::decode_complete_result(&replayed_completion_response.payload).unwrap();
    assert!(replayed_completion.replayed);
    assert_eq!(replayed_completion.receipt_id, completion.receipt_id);
    assert_eq!(replayed_completion.state_code, completion.state_code);

    for outcome in phase_two.await.unwrap() {
        assert!(outcome.unwrap());
    }
}

#[tokio::test]
async fn authenticated_ipc_rejects_unknown_principal() {
    let root = Root::new("stranger");
    let socket = SocketPath::new("stranger");
    let principal = bootstrap(&root, 0x43);
    let clock = clock_with_advanced_wall(&root);
    let provider = open_provider(&root);
    let server_task = spawn_server(provider, principal.identity, clock, socket.path(), 1);

    // The client connect is optimistic (it returns after sending its
    // attestation); the rejection surfaces as a dropped connection on the
    // first exchange, and as the server's typed handshake failure.
    let stranger = SigningKey::from_bytes(&[0x46; 32]);
    let stranger_id = PrincipalId::from_bytes([0x46; 16]);
    let framed = authenticated_connect(
        socket.path(),
        transport_config(),
        stranger_id,
        |digest: &[u8; 32]| Ok(stranger.sign(digest).to_bytes()),
    )
    .await
    .unwrap();
    let client = LocalRpcClient::new(framed.into_inner(), transport_config());
    assert!(
        client
            .exchange_validated(register_request(1))
            .await
            .is_err(),
        "stranger exchange must fail: the server drops the unauthenticated connection"
    );

    let outcomes = server_task.await.unwrap();
    match &outcomes[0] {
        Err(AuthenticatedMockDriverError::Handshake(HandshakeError::PrincipalUnknown(_))) => {}
        other => panic!("server must fail closed on the stranger, got {other:?}"),
    }
}

#[tokio::test]
async fn authenticated_ipc_rejects_bad_signature() {
    let root = Root::new("forged");
    let socket = SocketPath::new("forged");
    let principal = bootstrap(&root, 0x44);
    let clock = clock_with_advanced_wall(&root);
    let provider = open_provider(&root);
    let server_task = spawn_server(provider, principal.identity, clock, socket.path(), 1);

    let wrong_key = SigningKey::from_bytes(&[0x47; 32]);
    let framed = authenticated_connect(
        socket.path(),
        transport_config(),
        principal.id,
        |digest: &[u8; 32]| Ok(wrong_key.sign(digest).to_bytes()),
    )
    .await
    .unwrap();
    let client = LocalRpcClient::new(framed.into_inner(), transport_config());
    assert!(
        client
            .exchange_validated(register_request(1))
            .await
            .is_err(),
        "forged-signature exchange must fail: the server drops the connection"
    );

    let outcomes = server_task.await.unwrap();
    match &outcomes[0] {
        Err(AuthenticatedMockDriverError::Handshake(HandshakeError::SignatureInvalid)) => {}
        other => panic!("server must fail closed on the forged signature, got {other:?}"),
    }
}

#[tokio::test]
async fn authenticated_ipc_rejects_replayed_attestation() {
    let root = Root::new("replay");
    let socket = SocketPath::new("replay");
    let principal = bootstrap(&root, 0x45);
    let clock = clock_with_advanced_wall(&root);
    let provider = open_provider(&root);
    let server_task = spawn_server(provider, principal.identity, clock, socket.path(), 2);

    // Cycle 1: a complete manual handshake plus one real register exchange.
    let (stream, _) = connect(socket.path(), transport_config()).await.unwrap();
    let mut framed = FramedIo::new(stream, transport_config());
    let challenge = decode_challenge_wire(&framed.receive().await.unwrap()).unwrap();
    let nonce: [u8; 32] = challenge.nonce.as_slice().try_into().unwrap();
    let binding = nlos_ipc::handshake::transport::endpoint_channel_binding(socket.path());
    let digest = principal_handshake_message(&nonce, principal.id, &binding);
    let attestation = client_attestation(
        principal.id,
        &challenge,
        &binding,
        principal.signing.sign(&digest).to_bytes(),
    )
    .unwrap();
    let attestation_wire = encode_attestation_wire(&attestation).unwrap();
    framed.send(&attestation_wire).await.unwrap();
    let client = LocalRpcClient::new(framed.into_inner(), transport_config());
    let validated = client
        .exchange_validated(register_request(1))
        .await
        .unwrap();
    assert_success(validated.envelope());

    // Cycle 2: a fresh challenge is issued, but the old attestation bytes
    // are replayed. The one-time nonce was consumed in cycle 1, so the
    // server must fail closed and drop the connection before any request
    // byte is served.
    let (stream, _) = connect(socket.path(), transport_config()).await.unwrap();
    let mut framed = FramedIo::new(stream, transport_config());
    let _fresh_challenge = decode_challenge_wire(&framed.receive().await.unwrap()).unwrap();
    framed.send(&attestation_wire).await.unwrap();
    let replay_result = framed.receive().await;
    assert!(
        replay_result.is_err(),
        "server must drop the replayed connection without a response"
    );

    let outcomes = server_task.await.unwrap();
    assert!(outcomes[0].as_ref().unwrap());
    match &outcomes[1] {
        Err(AuthenticatedMockDriverError::Handshake(HandshakeError::NonceRejected)) => {}
        other => panic!("server must reject the replayed attestation, got {other:?}"),
    }
}
