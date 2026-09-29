#![cfg(unix)]

//! Degradation over the authenticated IPC face (W30-C): an unreachable
//! provider surfaces as the bounded SABI failure
//! (`HostLost` + `RetrySameIdempotencyKey`), the durable row keeps its
//! pre-degradation state, and retrying the same request bytes after the
//! provider returns converges through the durable replay semantics.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use ed25519_dalek::{Signer, SigningKey};
use nlos_clock::{AuthorityClock, NowRequest};
use nlos_driver_mock::authenticated::AuthenticatedMockDriverServer;
use nlos_driver_mock::codec::{
    self, CompleteOperationWire, DispatchOperationWire, RegisterOperationWire,
};
use nlos_driver_mock::ipc::{
    COMPLETE_OPERATION_METHOD, DISPATCH_OPERATION_METHOD, MOCK_DRIVER_SERVICE,
    MockDriverAuthorizer, REGISTER_OPERATION_METHOD,
};
use nlos_driver_mock::{MockProvider, ProviderFaultMode};
use nlos_identity::{BootstrapDecision, BootstrapPrincipalRequest, IdentityAuthority, KeyPurpose};
use nlos_ipc::handshake::transport::{ServerHandshakeContext, authenticated_connect};
use nlos_ipc::unix::UnixListenerAdapter;
use nlos_ipc::{LocalRpcClient, PeerAuthorizer, PeerIdentity, TransportConfig};
use nlos_schema::SABI_ENVELOPE_SCHEMA;
use nlos_schema::sabi::v1::envelope;
use nlos_schema::sabi::v1::{
    CallerIdentity, CapabilityHandle, Envelope, ExchangeRequest, RetryDirective, SabiErrorCode,
    SabiRequestContext, SchemaIdentity,
};
use nlos_store::SqliteOperationStore;
use nlos_types::{Generation, IdempotencyKey, PrincipalId};

const KEY_VALID_UNTIL_MS: u64 = 4_102_444_800_000;

const REGISTER_WIRE: RegisterOperationWire = RegisterOperationWire {
    operation_id: [0xB1; 16],
    operation_generation: 1,
    owner_fiber_id: [0xB2; 16],
    owner_fiber_generation: 1,
    cancellation_scope_id: [0xB3; 16],
    cancellation_generation: 1,
};

const COMPLETE_SEED: [u8; 32] = [0xB7; 32];

struct Root(PathBuf);

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

impl Root {
    fn new(label: &str) -> Self {
        let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        let root = Self(std::env::temp_dir().join(format!(
            "nlos-dm-ipc-degrade-{label}-{}-{sequence}",
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

struct SocketPath(PathBuf);

impl SocketPath {
    fn new(label: &str) -> Self {
        let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!(
            "nlos-dm-ipcd-{label}-{}-{sequence}",
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

fn bootstrap(root: &Root, seed: u8) -> (IdentityAuthority, SigningKey, PrincipalId) {
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
    (identity, signing, binding.principal_id)
}

fn clock_with_advanced_wall(root: &Root) -> AuthorityClock {
    let clock = AuthorityClock::open(root.path().join("clock")).unwrap();
    clock
        .wall_now(NowRequest {
            idempotency_key: IdempotencyKey::from_bytes([0xBE; 16]),
        })
        .unwrap();
    clock
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
                        principal_id: vec![0xB4; 16],
                        application_id: vec![0xB5; 16],
                        process_id: vec![0xB6; 16],
                        process_generation: 1,
                    }),
                    activity_context: Vec::new(),
                    task_execution_binding: None,
                    correlation_id: vec![0xB8; 16],
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

async fn exchange_authenticated(
    socket: &Path,
    principal_id: PrincipalId,
    signing: &SigningKey,
    request: ExchangeRequest,
) -> Envelope {
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

fn response_context(envelope: &Envelope) -> &nlos_schema::sabi::v1::SabiResponseContext {
    match envelope.common_context.as_ref() {
        Some(envelope::CommonContext::ResponseContext(context)) => context,
        other => panic!("driver response lacks a response context: {other:?}"),
    }
}

fn assert_no_failure(envelope: &Envelope) {
    let context = response_context(envelope);
    assert!(
        context.failure.is_none(),
        "driver request must succeed, got {context:?}"
    );
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

#[tokio::test]
async fn authenticated_ipc_degraded_provider_fails_bounded_and_recovers() {
    let root = Root::new("degrade");
    let socket = SocketPath::new("degrade");
    let (identity, signing, principal_id) = bootstrap(&root, 0xC1);
    let clock = clock_with_advanced_wall(&root);
    let provider = Arc::new(MockProvider::new(Arc::new(
        SqliteOperationStore::open(root.path().join("ops")).unwrap(),
    )));
    let handle = nlos_operation::OperationHandle {
        operation_id: nlos_types::OperationId::from_bytes(REGISTER_WIRE.operation_id),
        generation: Generation::INITIAL,
    };

    let listener = UnixListenerAdapter::bind(socket.path()).unwrap();
    let handshake = ServerHandshakeContext::new(socket.path(), 16).unwrap();
    let server = AuthenticatedMockDriverServer::new(
        Arc::clone(&provider),
        AllowDriver,
        identity,
        clock,
        handshake,
    );
    let server_task = tokio::spawn(async move {
        let mut outcomes = Vec::new();
        for cycle in 0..4_u8 {
            let mut nonce = [0xC3_u8; 32];
            nonce[0] = cycle + 1;
            let outcome = server
                .serve_one(&listener, transport_config(), &AllowPeer, 0, || nonce)
                .await;
            outcomes.push(outcome.map(|served| served.served().is_ok()));
        }
        outcomes
    });

    // Cycle 1: healthy register.
    let registered =
        exchange_authenticated(socket.path(), principal_id, &signing, register_request(1)).await;
    assert_no_failure(&registered);

    // Cycle 2: the provider goes unreachable; the dispatch surfaces as the
    // bounded failure envelope.
    provider.arm_fault(ProviderFaultMode::FailProviderRpc);
    let callback = [0xC2; 16];
    let degraded = exchange_authenticated(
        socket.path(),
        principal_id,
        &signing,
        dispatch_request(2, callback),
    )
    .await;
    let failure_context = response_context(&degraded);
    let failure = failure_context
        .failure
        .as_ref()
        .expect("bounded degradation failure");
    assert_eq!(failure.code, i32::from(SabiErrorCode::HostLost));
    assert_eq!(
        failure.retry,
        i32::from(RetryDirective::RetrySameIdempotencyKey)
    );
    assert!(
        !failure.safe_message.is_empty() && !failure.safe_message.contains('\0'),
        "the safe message stays bounded diagnostics"
    );
    assert!(degraded.payload.is_empty(), "no success evidence may leak");
    assert!(failure_context.receipts.is_empty());
    assert_eq!(
        provider.store().inspect(handle).unwrap().state,
        nlos_operation::OperationState::Registered,
        "the degraded window keeps the durable row in its pre-degradation state"
    );

    // Cycles 3 and 4: the provider returns; retrying the same request bytes
    // (same request id, same idempotency key) converges through the durable
    // replay semantics.
    provider.disarm_fault();
    let retried = exchange_authenticated(
        socket.path(),
        principal_id,
        &signing,
        dispatch_request(2, callback),
    )
    .await;
    assert_no_failure(&retried);
    let completed = exchange_authenticated(
        socket.path(),
        principal_id,
        &signing,
        complete_request(3, callback),
    )
    .await;
    assert_no_failure(&completed);
    assert!(
        provider
            .store()
            .inspect(handle)
            .unwrap()
            .state
            .is_terminal()
    );

    let outcomes = server_task.await.unwrap();
    for outcome in outcomes {
        assert!(
            outcome.unwrap(),
            "every degradation cycle stays inside the typed surface"
        );
    }
}
