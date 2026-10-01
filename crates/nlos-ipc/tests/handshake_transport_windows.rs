#![cfg(windows)]

//! Minimal Windows named-pipe loopback for the ADR-0011 authenticated
//! transport wiring (mirrors the Unix `handshake_transport` chain test):
//! `authenticated_serve_one` + `authenticated_connect` over a real local
//! pipe with genuine `IdentityAuthority` verification.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer, SigningKey};
use nlos_identity::{
    BootstrapDecision, BootstrapPrincipalRequest, IdentityAuthority, IdentityBinding, KeyPurpose,
};
use nlos_ipc::handshake::transport::{
    ServerHandshakeContext, authenticated_connect, authenticated_serve_one,
};
use nlos_ipc::windows::NamedPipeListenerAdapter;
use nlos_ipc::{LocalRpcClient, OutboundResponse, PeerIdentity, TransportConfig};
use nlos_schema::SABI_ENVELOPE_SCHEMA;
use nlos_schema::sabi::v1::{Envelope, ExchangeRequest, ExchangeResponse, SchemaIdentity};
use nlos_types::IdempotencyKey;

const NONCE: [u8; 32] = [0x5D; 32];

fn config() -> TransportConfig {
    TransportConfig::new(
        4_096,
        Duration::from_secs(5),
        Duration::from_secs(5),
        Duration::from_secs(5),
    )
    .unwrap()
}

fn temp_root(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "nlos-ipc-handshake-windows-{label}-{}-{nonce}",
        std::process::id()
    ))
}

fn pipe_path(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    PathBuf::from(format!(
        r"\\.\pipe\nlos-ipc-hs-{label}-{}-{nonce}",
        std::process::id()
    ))
}

fn bootstrap(seed: u8) -> (PathBuf, IdentityAuthority, SigningKey, IdentityBinding) {
    let root = temp_root("root");
    let identity = IdentityAuthority::open(&root).unwrap();
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

#[tokio::test]
async fn authenticated_chain_crosses_a_real_windows_named_pipe() {
    let (_identity_root, identity, key, principal) = bootstrap(0x41);
    let path = pipe_path("chain");
    let ctx = ServerHandshakeContext::new(&path, 8).unwrap();
    let mut listener = NamedPipeListenerAdapter::bind(&path, 2, config()).unwrap();
    let allow = |_: &PeerIdentity| -> Result<(), String> { Ok(()) };

    let server = authenticated_serve_one(
        &mut listener,
        config(),
        &identity,
        ctx.nonces(),
        ctx.binding(),
        &allow,
        |validated| async move {
            Ok(OutboundResponse::Typed(ExchangeResponse {
                envelope: Some(validated.envelope().clone()),
            }))
        },
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
        Err(nlos_ipc::handshake::HandshakeError::NonceRejected)
    ));
}
