#![cfg(windows)]

//! Minimal Windows named-pipe loopback for the ADR-0011 authenticated mock
//! driver entry (mirrors the Unix `ipc_authenticated_chain` full chain): one
//! `AuthenticatedMockDriverServer::serve_one` cycle plus one
//! `authenticated_connect` over a real local pipe, with genuine
//! `IdentityAuthority` verification, an `AuthorityClock` durable wall
//! reading, and one register MUTATION landing in the durable
//! `SqliteOperationStore`. The Unix-only fail-closed matrix (unknown
//! principal, forged signature, replayed attestation) stays in
//! `ipc_authenticated_chain.rs`; the handshake core both platforms share is
//! byte-identical, so the CI Windows leg only needs this loopback.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer, SigningKey};
use nlos_clock::{AuthorityClock, NowRequest};
use nlos_driver_mock::MockProvider;
use nlos_driver_mock::authenticated::AuthenticatedMockDriverServer;
use nlos_driver_mock::codec::{self, RegisterOperationWire};
use nlos_driver_mock::ipc::{MOCK_DRIVER_SERVICE, MockDriverAuthorizer, REGISTER_OPERATION_METHOD};
use nlos_identity::{BootstrapDecision, BootstrapPrincipalRequest, IdentityAuthority, KeyPurpose};
use nlos_ipc::handshake::transport::{ServerHandshakeContext, authenticated_connect};
use nlos_ipc::windows::NamedPipeListenerAdapter;
use nlos_ipc::{LocalRpcClient, PeerAuthorizer, PeerIdentity, TransportConfig};
use nlos_operation::OperationHandle;
use nlos_schema::SABI_ENVELOPE_SCHEMA;
use nlos_schema::sabi::v1::{
    CallerIdentity, CapabilityHandle, Envelope, ExchangeRequest, SabiRequestContext,
    SchemaIdentity, envelope,
};
use nlos_store::SqliteOperationStore;
use nlos_types::{Generation, IdempotencyKey, OperationId, PrincipalId};

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

struct Root(PathBuf);

impl Root {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "nlos-driver-mock-auth-windows-{label}-{}-{nonce}",
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

fn pipe_path(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    PathBuf::from(format!(
        r"\\.\pipe\nlos-dm-auth-{label}-{}-{nonce}",
        std::process::id()
    ))
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

/// Opens the clock authority and advances its wall domain once, so the
/// verified-at reading is the real current epoch-millisecond high-water.
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

fn register_request() -> ExchangeRequest {
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
            service: MOCK_DRIVER_SERVICE.to_owned(),
            method: REGISTER_OPERATION_METHOD.to_owned(),
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
                    idempotency_key: vec![0x35; 16],
                    deadline_monotonic_ns: 0,
                    capability_handles: vec![CapabilityHandle {
                        slot: 9,
                        generation: 1,
                    }],
                    reservation_handle: None,
                    proposal_or_input_digest_sha256: Vec::new(),
                },
            )),
            payload: codec::encode_register_request(&REGISTER_WIRE).unwrap(),
        }),
    }
}

#[tokio::test]
async fn authenticated_register_crosses_a_real_windows_named_pipe() {
    let root = Root::new("roundtrip");
    let path = pipe_path("roundtrip");
    let principal = bootstrap(&root, 0x41);
    let clock = clock_with_advanced_wall(&root);
    let provider = open_provider(&root);

    let handshake = ServerHandshakeContext::new(&path, 8).unwrap();
    let server = AuthenticatedMockDriverServer::new(
        Arc::clone(&provider),
        AllowDriver,
        principal.identity,
        clock,
        handshake,
    );
    // One serve cycle; each accept creates the next pipe instance, so retain
    // spare instances.
    let mut listener = NamedPipeListenerAdapter::bind(&path, 4, transport_config()).unwrap();
    let server_task = tokio::spawn(async move {
        server
            .serve_one(&mut listener, transport_config(), &AllowPeer, 0, || {
                [0x5D; 32]
            })
            .await
    });

    let framed = authenticated_connect(
        &path,
        transport_config(),
        principal.id,
        |digest: &[u8; 32]| Ok(principal.signing.sign(digest).to_bytes()),
    )
    .await
    .unwrap();
    let client = LocalRpcClient::new(framed.into_inner(), transport_config());
    let response = client.exchange_validated(register_request()).await.unwrap();
    let envelope = response.envelope();
    match envelope.common_context.as_ref() {
        Some(envelope::CommonContext::ResponseContext(context)) => {
            assert!(
                context.failure.is_none(),
                "register must succeed, got {context:?}"
            );
            assert_eq!(context.receipts.len(), 1);
        }
        other => panic!("register response lacks a response context: {other:?}"),
    }
    let result = codec::decode_register_result(&envelope.payload).unwrap();
    assert!(!result.replayed);
    assert_eq!(result.operation_id, REGISTER_WIRE.operation_id);
    assert_ne!(
        result.admission_receipt_id, [0; 16],
        "authority-derived admission receipt must be non-zero"
    );
    assert_eq!(
        context_receipt_id(envelope),
        result.admission_receipt_id.to_vec()
    );

    // The durable authority agrees with the wire result: the operation is
    // admitted, not yet terminal.
    let snapshot = provider
        .store()
        .inspect(OperationHandle {
            operation_id: OperationId::from_bytes(REGISTER_WIRE.operation_id),
            generation: Generation::INITIAL,
        })
        .unwrap();
    assert!(!snapshot.state.is_terminal());

    let outcome = server_task.await.unwrap().unwrap();
    assert!(outcome.served().is_ok());
    assert_eq!(outcome.verified().principal_id(), principal.id);
}

fn context_receipt_id(envelope: &Envelope) -> Vec<u8> {
    match envelope.common_context.as_ref() {
        Some(envelope::CommonContext::ResponseContext(context)) => {
            context.receipts[0].receipt_id.clone()
        }
        other => panic!("register response lacks a response context: {other:?}"),
    }
}
