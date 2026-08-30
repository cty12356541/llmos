use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer, SigningKey};
use nlos_identity::{
    BootstrapDecision, BootstrapPrincipalRequest, IdentityAuthority, IdentityBinding, KeyPurpose,
    RevokeKeyRequest,
};
use nlos_ipc::handshake::{
    HandshakeError, HandshakeNonceRegistry, client_attestation, decode_attestation_wire,
    decode_challenge_wire, encode_attestation_wire, encode_challenge_wire, issue_challenge,
    principal_handshake_message, verify_attestation,
};
#[cfg(unix)]
use nlos_ipc::{FramedIo, LocalRpcClient, OutboundResponse, serve_one};
use nlos_ipc::{PeerAuthorizer, PeerIdentity, TransportConfig};
use nlos_schema::sabi::v1::{
    Envelope, ExchangeRequest, PrincipalHandshakeAttestation, SchemaIdentity,
};
#[cfg(unix)]
use nlos_schema::sabi::v1::ExchangeResponse;
use nlos_schema::{CompatibilityError, SABI_ENVELOPE_SCHEMA, principal_handshake_schema_identity};
use nlos_types::{IdempotencyKey, PrincipalId};
use prost::Message as _;

#[cfg(unix)]
fn config() -> TransportConfig {
    TransportConfig::new(
        4_096,
        Duration::from_secs(5),
        Duration::from_secs(5),
        Duration::from_secs(5),
    )
    .unwrap()
}

struct TempRoot(std::path::PathBuf);

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

impl TempRoot {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "nlos-ipc-handshake-{label}-{}-{nonce}-{}",
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

fn sign_digest(
    key: &SigningKey,
    nonce: &[u8; 32],
    principal: PrincipalId,
    channel_binding: &[u8],
) -> [u8; 64] {
    key.sign(&principal_handshake_message(
        nonce,
        principal,
        channel_binding,
    ))
    .to_bytes()
}

fn attestation_for(
    key: &SigningKey,
    principal: &IdentityBinding,
    nonce: &[u8; 32],
    channel_binding: &[u8],
) -> PrincipalHandshakeAttestation {
    client_attestation(
        principal.principal_id,
        &nlos_schema::sabi::v1::PrincipalHandshakeChallenge {
            schema: Some(principal_handshake_schema_identity()),
            nonce: nonce.to_vec(),
        },
        channel_binding,
        sign_digest(key, nonce, principal.principal_id, channel_binding),
    )
    .unwrap()
}

#[cfg(unix)]
struct Allow;

#[cfg(unix)]
impl PeerAuthorizer for Allow {
    fn authorize(&self, _: &PeerIdentity) -> Result<(), String> {
        Ok(())
    }
}

#[test]
fn nonce_registry_is_bounded_and_one_time() {
    assert!(matches!(
        HandshakeNonceRegistry::new(0).unwrap_err(),
        HandshakeError::InvalidConfig("nonce registry capacity must be non-zero")
    ));

    let registry = HandshakeNonceRegistry::new(2).unwrap();
    registry.register([1; 32]).unwrap();
    assert!(matches!(
        registry.register([1; 32]),
        Err(HandshakeError::NonceRejected)
    ));
    registry.register([2; 32]).unwrap();
    assert!(matches!(
        registry.register([3; 32]),
        Err(HandshakeError::NonceCapacityExhausted { capacity: 2 })
    ));
    assert!(matches!(
        registry.consume(&[4; 32]),
        Err(HandshakeError::NonceRejected)
    ));
    registry.consume(&[1; 32]).unwrap();
    assert!(matches!(
        registry.consume(&[1; 32]),
        Err(HandshakeError::NonceRejected)
    ));
}

#[test]
fn valid_handshake_authenticates_the_current_binding() {
    let (_root, identity, key, binding) = bootstrap(0x21);
    let registry = HandshakeNonceRegistry::new(8).unwrap();

    let challenge = issue_challenge(&registry, [0x11; 32]).unwrap();
    let nonce: [u8; 32] = challenge.nonce.clone().try_into().unwrap();
    let attestation = attestation_for(&key, &binding, &nonce, b"unix:///tmp/nlos-handshake.sock");
    let verified = verify_attestation(
        &identity,
        &registry,
        &attestation,
        b"unix:///tmp/nlos-handshake.sock",
        5_000,
    )
    .unwrap();
    assert_eq!(verified.principal_id(), binding.principal_id);
    assert_eq!(verified.control_domain_id(), binding.control_domain_id);
    assert_eq!(verified.key_id(), binding.key_id);
    assert_eq!(verified.key_generation(), binding.key_generation);
}

#[test]
fn unknown_principal_fails_closed() {
    let (_root, identity, _key, _binding) = bootstrap(0x22);
    let registry = HandshakeNonceRegistry::new(8).unwrap();
    let stranger = PrincipalId::from_bytes([0xEE; 16]);
    let stranger_key = SigningKey::from_bytes(&[0x22; 32]);

    let challenge = issue_challenge(&registry, [0x12; 32]).unwrap();
    let nonce: [u8; 32] = challenge.nonce.clone().try_into().unwrap();
    let attestation = client_attestation(
        stranger,
        &challenge,
        b"binding",
        sign_digest(&stranger_key, &nonce, stranger, b"binding"),
    )
    .unwrap();
    assert!(matches!(
        verify_attestation(&identity, &registry, &attestation, b"binding", 5_000),
        Err(HandshakeError::PrincipalUnknown(id)) if id == stranger
    ));
}

#[test]
fn revoked_key_fails_closed() {
    let (_root, identity, key, binding) = bootstrap(0x23);
    identity
        .revoke_key(RevokeKeyRequest {
            key_id: binding.key_id,
            expected_key_generation: binding.key_generation,
            expected_identity_snapshot_id: binding.identity_snapshot_id,
            idempotency_key: IdempotencyKey::from_bytes([0x7C; 16]),
            revoked_at_ms: 6_000,
        })
        .unwrap();
    let registry = HandshakeNonceRegistry::new(8).unwrap();

    let challenge = issue_challenge(&registry, [0x13; 32]).unwrap();
    let nonce: [u8; 32] = challenge.nonce.clone().try_into().unwrap();
    let attestation = attestation_for(&key, &binding, &nonce, b"binding");
    assert!(matches!(
        verify_attestation(&identity, &registry, &attestation, b"binding", 7_000),
        Err(HandshakeError::KeyRevoked)
    ));
}

#[test]
fn stale_generation_signature_fails_closed() {
    let (_root, identity, _current_key, binding) = bootstrap(0x24);
    let registry = HandshakeNonceRegistry::new(8).unwrap();

    // A signature by key material that is not the principal's current
    // binding (for example a rotated-away generation) fails closed even
    // though the attestation names the right principal.
    let rotated_away = SigningKey::from_bytes(&[0xFF; 32]);
    let challenge = issue_challenge(&registry, [0x14; 32]).unwrap();
    let nonce: [u8; 32] = challenge.nonce.clone().try_into().unwrap();
    let attestation = attestation_for(&rotated_away, &binding, &nonce, b"binding");
    assert!(matches!(
        verify_attestation(&identity, &registry, &attestation, b"binding", 5_000),
        Err(HandshakeError::SignatureInvalid)
    ));
}

#[test]
fn expired_key_window_fails_closed() {
    let (_root, identity, key, binding) = bootstrap(0x25);
    let registry = HandshakeNonceRegistry::new(8).unwrap();

    let challenge = issue_challenge(&registry, [0x15; 32]).unwrap();
    let nonce: [u8; 32] = challenge.nonce.clone().try_into().unwrap();
    let attestation = attestation_for(&key, &binding, &nonce, b"binding");
    assert!(matches!(
        verify_attestation(&identity, &registry, &attestation, b"binding", 20_000),
        Err(HandshakeError::KeyExpired)
    ));
}

#[test]
fn wrong_nonce_fails_closed() {
    let (_root, identity, key, binding) = bootstrap(0x26);
    let registry = HandshakeNonceRegistry::new(8).unwrap();
    let _ = issue_challenge(&registry, [0x16; 32]).unwrap();

    let forged = PrincipalHandshakeAttestation {
        schema: Some(principal_handshake_schema_identity()),
        principal_id: binding.principal_id.as_bytes().to_vec(),
        nonce: vec![0x99; 32],
        channel_binding: b"binding".to_vec(),
        signature: sign_digest(&key, &[0x99; 32], binding.principal_id, b"binding").to_vec(),
    };
    assert!(matches!(
        verify_attestation(&identity, &registry, &forged, b"binding", 5_000),
        Err(HandshakeError::NonceRejected)
    ));
}

#[test]
fn tampered_signature_fails_closed_and_burns_the_nonce() {
    let (_root, identity, key, binding) = bootstrap(0x27);
    let registry = HandshakeNonceRegistry::new(8).unwrap();

    let challenge = issue_challenge(&registry, [0x17; 32]).unwrap();
    let nonce: [u8; 32] = challenge.nonce.clone().try_into().unwrap();
    let mut attestation = attestation_for(&key, &binding, &nonce, b"binding");
    attestation.signature[7] ^= 0x01;
    assert!(matches!(
        verify_attestation(&identity, &registry, &attestation, b"binding", 5_000),
        Err(HandshakeError::SignatureInvalid)
    ));

    // The tampered attempt consumed the single-use nonce, so replaying the
    // untampered original afterwards still fails closed.
    let original = attestation_for(&key, &binding, &nonce, b"binding");
    assert!(matches!(
        verify_attestation(&identity, &registry, &original, b"binding", 5_000),
        Err(HandshakeError::NonceRejected)
    ));
}

#[test]
fn replayed_attestation_is_rejected() {
    let (_root, identity, key, binding) = bootstrap(0x28);
    let registry = HandshakeNonceRegistry::new(8).unwrap();

    let challenge = issue_challenge(&registry, [0x18; 32]).unwrap();
    let nonce: [u8; 32] = challenge.nonce.clone().try_into().unwrap();
    let attestation = attestation_for(&key, &binding, &nonce, b"binding");
    verify_attestation(&identity, &registry, &attestation, b"binding", 5_000).unwrap();
    assert!(matches!(
        verify_attestation(&identity, &registry, &attestation, b"binding", 5_000),
        Err(HandshakeError::NonceRejected)
    ));
}

#[test]
fn channel_binding_mismatch_fails_closed_without_burning_the_nonce() {
    let (_root, identity, key, binding) = bootstrap(0x29);
    let registry = HandshakeNonceRegistry::new(8).unwrap();

    let challenge = issue_challenge(&registry, [0x19; 32]).unwrap();
    let nonce: [u8; 32] = challenge.nonce.clone().try_into().unwrap();
    let attacker_signed = attestation_for(&key, &binding, &nonce, b"unix:///tmp/attacker.sock");
    assert!(matches!(
        verify_attestation(
            &identity,
            &registry,
            &attacker_signed,
            b"unix:///tmp/real.sock",
            5_000
        ),
        Err(HandshakeError::ChannelBindingMismatch)
    ));

    // The mismatch was rejected before the nonce was consumed, so the honest
    // client can still complete its handshake on the true binding.
    let honest = attestation_for(&key, &binding, &nonce, b"unix:///tmp/real.sock");
    verify_attestation(
        &identity,
        &registry,
        &honest,
        b"unix:///tmp/real.sock",
        5_000,
    )
    .unwrap();
}

#[test]
fn wire_codec_wrappers_fail_closed_on_schema_violations() {
    let registry = HandshakeNonceRegistry::new(8).unwrap();
    let challenge = issue_challenge(&registry, [0x1A; 32]).unwrap();
    let wire = encode_challenge_wire(&challenge).unwrap();
    assert_eq!(decode_challenge_wire(&wire).unwrap(), challenge);

    let unbound = nlos_schema::sabi::v1::PrincipalHandshakeChallenge {
        schema: None,
        nonce: challenge.nonce.clone(),
    };
    assert!(matches!(
        decode_challenge_wire(&unbound.encode_to_vec()),
        Err(HandshakeError::Schema(
            CompatibilityError::MissingSchemaIdentity
        ))
    ));

    let (_root, _identity, key, binding) = bootstrap(0x2A);
    let nonce: [u8; 32] = challenge.nonce.clone().try_into().unwrap();
    let attestation = attestation_for(&key, &binding, &nonce, b"binding");
    let attestation_wire = encode_attestation_wire(&attestation).unwrap();
    assert_eq!(
        decode_attestation_wire(&attestation_wire).unwrap(),
        attestation
    );
}

#[cfg(unix)]
#[tokio::test]
async fn handshake_roundtrip_over_real_unix_socket_then_authenticated_exchange() {
    use nlos_ipc::unix::{UnixListenerAdapter, connect};

    let path = std::env::temp_dir().join(format!(
        "nlos-ipc-handshake-{}-{}.sock",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let binding = path.to_string_lossy().into_owned();
    let (_root, identity, key, principal) = bootstrap(0x2B);
    let server_binding = binding.clone();
    let listener = UnixListenerAdapter::bind(&path).unwrap();
    let server = tokio::spawn(async move {
        let handshake: Result<
            (tokio::net::UnixStream, PeerIdentity),
            Box<dyn std::error::Error + Send + Sync>,
        > = async {
            let (stream, peer) = listener.accept(config()).await?;
            assert!(matches!(peer, PeerIdentity::Unix { .. }));

            let registry = HandshakeNonceRegistry::new(8).unwrap();
            let challenge = issue_challenge(&registry, [0x2B; 32]).unwrap();
            let mut framed = FramedIo::new(stream, config());
            framed
                .send(&encode_challenge_wire(&challenge).unwrap())
                .await?;
            let attestation = decode_attestation_wire(&framed.receive().await?)?;
            let verified = verify_attestation(
                &identity,
                &registry,
                &attestation,
                server_binding.as_bytes(),
                5_000,
            )?;
            assert_eq!(verified.key_generation(), principal.key_generation);
            Ok((framed.into_inner(), peer))
        }
        .await;

        let (authenticated, peer) = handshake.unwrap();
        serve_one(
            authenticated,
            config(),
            peer,
            &Allow,
            |validated| async move {
                Ok(OutboundResponse::Typed(ExchangeResponse {
                    envelope: Some(validated.envelope().clone()),
                }))
            },
        )
        .await
    });

    let (stream, _peer) = connect(&binding, config()).await.unwrap();
    let mut framed = FramedIo::new(stream, config());
    let challenge = decode_challenge_wire(&framed.receive().await.unwrap()).unwrap();
    let nonce: [u8; 32] = challenge.nonce.clone().try_into().unwrap();
    let attestation = client_attestation(
        principal.principal_id,
        &challenge,
        binding.as_bytes(),
        sign_digest(&key, &nonce, principal.principal_id, binding.as_bytes()),
    )
    .unwrap();
    framed
        .send(&encode_attestation_wire(&attestation).unwrap())
        .await
        .unwrap();

    let response = LocalRpcClient::new(framed.into_inner(), config())
        .exchange_validated(authenticated_request())
        .await
        .unwrap();
    assert_eq!(response.envelope().request_id, vec![9; 16]);
    server.await.unwrap().unwrap();
    std::fs::remove_file(binding).unwrap();
}

#[cfg(unix)]
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
