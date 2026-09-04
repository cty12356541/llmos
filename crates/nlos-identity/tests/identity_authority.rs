use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer, SigningKey};
use nlos_clock::{AuthorityClock, AuthorityClockError, WallSource};
use nlos_identity::{
    BootstrapDecision, BootstrapPrincipalRequest, IdentityAuthority, IdentityAuthorityError,
    KeyPurpose, KeyRevocationDecision, KeyRotationDecision, RevokeKeyRequest, RotateKeyRequest,
    VerifyBarrierObservationSignatureRequest, VerifyCapabilityCommandSignatureAtClockRequest,
    VerifySemanticSignatureRequest, semantic_signature_message,
};
use nlos_types::{Generation, IdempotencyKey, PrincipalId, SemanticEventId};
use rusqlite::Connection;

static NEXT: AtomicU64 = AtomicU64::new(1);

struct Root(PathBuf);

impl Root {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "nlos-identity-{label}-{}-{nonce}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn signing_key(seed: u8) -> SigningKey {
    SigningKey::from_bytes(&[seed; 32])
}

fn bootstrap_request(seed: u8, signing_key: &SigningKey) -> BootstrapPrincipalRequest {
    BootstrapPrincipalRequest {
        principal_profile_digest: [seed.wrapping_add(1); 32],
        control_domain_policy_digest: [seed.wrapping_add(2); 32],
        public_key: signing_key.verifying_key().to_bytes(),
        key_purpose: KeyPurpose::SemanticSigning,
        key_valid_from_ms: 1_000,
        key_valid_until_ms: 9_000,
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(3); 16]),
        created_at_ms: 500,
    }
}

fn barrier_bootstrap_request(seed: u8, signing_key: &SigningKey) -> BootstrapPrincipalRequest {
    BootstrapPrincipalRequest {
        key_purpose: KeyPurpose::BarrierObservationSigning,
        ..bootstrap_request(seed, signing_key)
    }
}

fn verify_request(
    signing_key: &SigningKey,
    binding: nlos_identity::IdentityBinding,
    event_id: SemanticEventId,
    admitted_at_ms: u64,
) -> VerifySemanticSignatureRequest {
    let signature = signing_key
        .sign(&semantic_signature_message(event_id))
        .to_bytes();
    VerifySemanticSignatureRequest {
        event_id,
        issuer: binding.principal_id,
        control_domain_id: binding.control_domain_id,
        key_id: binding.key_id,
        signature,
        admitted_at_ms,
    }
}

#[test]
fn bootstrap_is_atomic_durable_and_exactly_replayable() {
    let root = Root::new("bootstrap");
    let key = signing_key(10);
    let request = bootstrap_request(10, &key);
    let binding = {
        let authority = IdentityAuthority::open(root.path()).unwrap();
        let first = authority.bootstrap_principal(request).unwrap();
        assert!(matches!(first, BootstrapDecision::Created(_)));
        let replay = authority.bootstrap_principal(request).unwrap();
        assert!(matches!(replay, BootstrapDecision::Replayed(_)));
        assert_eq!(first.binding(), replay.binding());
        first.binding()
    };

    let reopened = IdentityAuthority::open(root.path()).unwrap();
    assert_eq!(
        reopened.bootstrap_principal(request).unwrap().binding(),
        binding
    );
    assert_eq!(
        reopened.inspect_current_binding(binding.key_id).unwrap(),
        binding
    );
    assert_eq!(binding.snapshot_generation, Generation::INITIAL);
    assert_eq!(binding.key_generation, Generation::INITIAL);
}

#[test]
fn semantic_verification_checks_signature_binding_and_validity() {
    let root = Root::new("verify");
    let key = signing_key(20);
    let authority = IdentityAuthority::open(root.path()).unwrap();
    let binding = authority
        .bootstrap_principal(bootstrap_request(20, &key))
        .unwrap()
        .binding();
    let event_id = SemanticEventId::from_bytes([0x55; 32]);
    let request = verify_request(&key, binding, event_id, 2_000);
    let verified = authority.verify_semantic_signature(request).unwrap();
    assert_eq!(verified.principal_id(), binding.principal_id);
    assert_eq!(
        verified.identity_snapshot_id(),
        binding.identity_snapshot_id
    );

    let mut wrong_issuer = request;
    wrong_issuer.issuer = PrincipalId::from_bytes([0xee; 16]);
    assert!(matches!(
        authority.verify_semantic_signature(wrong_issuer),
        Err(IdentityAuthorityError::SignerBindingMismatch)
    ));

    let mut wrong_signature = request;
    wrong_signature.signature[0] ^= 1;
    assert!(matches!(
        authority.verify_semantic_signature(wrong_signature),
        Err(IdentityAuthorityError::InvalidSignature)
    ));
    let mut early = request;
    early.admitted_at_ms = 999;
    assert!(matches!(
        authority.verify_semantic_signature(early),
        Err(IdentityAuthorityError::KeyNotYetValid)
    ));
    let mut expired = request;
    expired.admitted_at_ms = 9_001;
    assert!(matches!(
        authority.verify_semantic_signature(expired),
        Err(IdentityAuthorityError::KeyExpired)
    ));
}

#[test]
fn revocation_advances_both_fences_and_survives_restart() {
    let root = Root::new("revoke");
    let key = signing_key(30);
    let (binding, request, receipt) = {
        let authority = IdentityAuthority::open(root.path()).unwrap();
        let binding = authority
            .bootstrap_principal(bootstrap_request(30, &key))
            .unwrap()
            .binding();
        let request = RevokeKeyRequest {
            key_id: binding.key_id,
            expected_key_generation: binding.key_generation,
            expected_identity_snapshot_id: binding.identity_snapshot_id,
            idempotency_key: IdempotencyKey::from_bytes([0x77; 16]),
            revoked_at_ms: 3_000,
        };
        let first = authority.revoke_key(request).unwrap();
        assert!(matches!(first, KeyRevocationDecision::Revoked(_)));
        let replay = authority.revoke_key(request).unwrap();
        assert!(matches!(replay, KeyRevocationDecision::Replayed(_)));
        assert_eq!(first.receipt(), replay.receipt());
        (binding, request, first.receipt())
    };

    assert_eq!(receipt.resulting_key_generation.get(), 2);
    assert_eq!(receipt.snapshot_generation.get(), 2);
    assert_ne!(receipt.identity_snapshot_id, binding.identity_snapshot_id);

    let reopened = IdentityAuthority::open(root.path()).unwrap();
    assert_eq!(reopened.revoke_key(request).unwrap().receipt(), receipt);
    let historical = reopened
        .inspect_binding_at_snapshot(binding.identity_snapshot_id, binding.key_id)
        .unwrap();
    assert_eq!(historical, binding);
    let current = reopened.inspect_current_binding(binding.key_id).unwrap();
    assert_eq!(current.identity_snapshot_id, receipt.identity_snapshot_id);
    assert_eq!(current.key_revoked_at_ms, Some(3_000));
    let event_id = SemanticEventId::from_bytes([0x66; 32]);
    assert!(matches!(
        reopened.verify_semantic_signature(verify_request(&key, binding, event_id, 2_000)),
        Err(IdentityAuthorityError::KeyRevoked)
    ));
    assert!(matches!(
        reopened.revoke_key(RevokeKeyRequest {
            idempotency_key: IdempotencyKey::from_bytes([0x78; 16]),
            ..request
        }),
        Err(IdentityAuthorityError::KeyGenerationFenceConflict)
    ));
}

#[test]
fn rotation_advances_both_fences_rejects_old_signatures_and_survives_restart() {
    let root = Root::new("rotate");
    let old_key = signing_key(35);
    let new_key = signing_key(36);
    let (binding, request, receipt) = {
        let authority = IdentityAuthority::open(root.path()).unwrap();
        let binding = authority
            .bootstrap_principal(bootstrap_request(35, &old_key))
            .unwrap()
            .binding();
        let request = RotateKeyRequest {
            key_id: binding.key_id,
            expected_key_generation: binding.key_generation,
            expected_identity_snapshot_id: binding.identity_snapshot_id,
            new_public_key: new_key.verifying_key().to_bytes(),
            new_valid_from_ms: 2_000,
            new_valid_until_ms: 10_000,
            idempotency_key: IdempotencyKey::from_bytes([0x88; 16]),
            rotated_at_ms: 3_000,
        };
        let first = authority.rotate_key(request).unwrap();
        assert!(matches!(first, KeyRotationDecision::Rotated(_)));
        let replay = authority.rotate_key(request).unwrap();
        assert!(matches!(replay, KeyRotationDecision::Replayed(_)));
        assert_eq!(first.receipt(), replay.receipt());
        (binding, request, first.receipt())
    };

    assert_eq!(receipt.resulting_key_generation.get(), 2);
    assert_eq!(receipt.snapshot_generation.get(), 2);
    assert_eq!(receipt.new_public_key, new_key.verifying_key().to_bytes());
    assert_ne!(receipt.identity_snapshot_id, binding.identity_snapshot_id);

    let reopened = IdentityAuthority::open(root.path()).unwrap();
    assert_eq!(reopened.rotate_key(request).unwrap().receipt(), receipt);
    let historical = reopened
        .inspect_binding_at_snapshot(binding.identity_snapshot_id, binding.key_id)
        .unwrap();
    assert_eq!(historical, binding);
    let current = reopened.inspect_current_binding(binding.key_id).unwrap();
    assert_eq!(current.identity_snapshot_id, receipt.identity_snapshot_id);
    assert_eq!(current.public_key, receipt.new_public_key);
    assert_eq!(current.key_generation.get(), 2);

    let event_id = SemanticEventId::from_bytes([0x67; 32]);
    assert!(matches!(
        reopened.verify_semantic_signature(verify_request(&old_key, binding, event_id, 2_500)),
        Err(IdentityAuthorityError::InvalidSignature)
    ));
    let verified = reopened
        .verify_semantic_signature(verify_request(&new_key, current, event_id, 2_500))
        .unwrap();
    assert_eq!(verified.key_generation().get(), 2);

    assert!(matches!(
        reopened.rotate_key(RotateKeyRequest {
            idempotency_key: IdempotencyKey::from_bytes([0x89; 16]),
            ..request
        }),
        Err(IdentityAuthorityError::KeyGenerationFenceConflict)
    ));
}

#[test]
fn bootstrap_rebinding_and_invalid_validity_fail_closed() {
    let root = Root::new("conflict");
    let key = signing_key(40);
    let authority = IdentityAuthority::open(root.path()).unwrap();
    let request = bootstrap_request(40, &key);
    authority.bootstrap_principal(request).unwrap();
    let mut changed = request;
    changed.control_domain_policy_digest = [0xaa; 32];
    assert!(matches!(
        authority.bootstrap_principal(changed),
        Err(IdentityAuthorityError::IdempotencyConflict)
    ));
    let mut invalid = bootstrap_request(41, &signing_key(41));
    invalid.key_valid_from_ms = 5_000;
    invalid.key_valid_until_ms = 4_999;
    assert!(matches!(
        authority.bootstrap_principal(invalid),
        Err(IdentityAuthorityError::InvalidKeyValidity)
    ));
}

#[test]
fn snapshot_key_versions_and_revocation_receipts_are_ddl_immutable() {
    let root = Root::new("immutable");
    let key = signing_key(50);
    let new_key = signing_key(51);
    let authority = IdentityAuthority::open(root.path()).unwrap();
    let binding = authority
        .bootstrap_principal(bootstrap_request(50, &key))
        .unwrap()
        .binding();
    authority
        .revoke_key(RevokeKeyRequest {
            key_id: binding.key_id,
            expected_key_generation: binding.key_generation,
            expected_identity_snapshot_id: binding.identity_snapshot_id,
            idempotency_key: IdempotencyKey::from_bytes([0x99; 16]),
            revoked_at_ms: 3_000,
        })
        .unwrap();
    let rotate_binding = authority
        .bootstrap_principal(bootstrap_request(52, &new_key))
        .unwrap()
        .binding();
    authority
        .rotate_key(RotateKeyRequest {
            key_id: rotate_binding.key_id,
            expected_key_generation: rotate_binding.key_generation,
            expected_identity_snapshot_id: rotate_binding.identity_snapshot_id,
            new_public_key: signing_key(53).verifying_key().to_bytes(),
            new_valid_from_ms: 2_000,
            new_valid_until_ms: 10_000,
            idempotency_key: IdempotencyKey::from_bytes([0x9a; 16]),
            rotated_at_ms: 3_000,
        })
        .unwrap();
    drop(authority);

    let raw = Connection::open(root.path().join("identity-authority.db")).unwrap();
    assert!(
        raw.execute("UPDATE key_versions SET valid_until_ms=9999", [])
            .is_err()
    );
    assert!(raw.execute("DELETE FROM identity_snapshots", []).is_err());
    assert!(raw.execute("DELETE FROM key_revocations", []).is_err());
    assert!(raw.execute("DELETE FROM key_rotations", []).is_err());
}

#[test]
fn barrier_observation_verification_proves_signer_identity() {
    let root = Root::new("barrier-verify");
    let key = signing_key(60);
    let authority = IdentityAuthority::open(root.path()).unwrap();
    let binding = authority
        .bootstrap_principal(barrier_bootstrap_request(60, &key))
        .unwrap()
        .binding();
    assert_eq!(binding.key_purpose, KeyPurpose::BarrierObservationSigning);
    assert_eq!(
        authority
            .inspect_current_binding(binding.key_id)
            .unwrap()
            .key_purpose,
        KeyPurpose::BarrierObservationSigning
    );
    let message_digest = [0x5a; 32];
    let signature = key.sign(&message_digest).to_bytes();
    let verified = authority
        .verify_barrier_observation_signature(VerifyBarrierObservationSignatureRequest {
            message_digest,
            issuer: binding.principal_id,
            control_domain_id: binding.control_domain_id,
            key_id: binding.key_id,
            signature,
            verified_at_ms: 2_000,
        })
        .unwrap();
    assert_eq!(verified.principal_id(), binding.principal_id);
    assert_eq!(verified.control_domain_id(), binding.control_domain_id);
    assert_eq!(verified.key_id(), binding.key_id);
    assert_eq!(verified.key_generation(), binding.key_generation);

    let mut wrong_issuer = VerifyBarrierObservationSignatureRequest {
        message_digest,
        issuer: PrincipalId::from_bytes([0xee; 16]),
        control_domain_id: binding.control_domain_id,
        key_id: binding.key_id,
        signature,
        verified_at_ms: 2_000,
    };
    assert!(matches!(
        authority.verify_barrier_observation_signature(wrong_issuer),
        Err(IdentityAuthorityError::SignerBindingMismatch)
    ));
    wrong_issuer.issuer = binding.principal_id;
    wrong_issuer.verified_at_ms = 999;
    assert!(matches!(
        authority.verify_barrier_observation_signature(wrong_issuer),
        Err(IdentityAuthorityError::KeyNotYetValid)
    ));
    wrong_issuer.verified_at_ms = 9_001;
    assert!(matches!(
        authority.verify_barrier_observation_signature(wrong_issuer),
        Err(IdentityAuthorityError::KeyExpired)
    ));
}

#[test]
fn barrier_observation_verification_rejects_semantic_signing_keys() {
    let root = Root::new("barrier-purpose");
    let key = signing_key(70);
    let authority = IdentityAuthority::open(root.path()).unwrap();
    let binding = authority
        .bootstrap_principal(bootstrap_request(70, &key))
        .unwrap()
        .binding();
    assert_eq!(binding.key_purpose, KeyPurpose::SemanticSigning);
    let message_digest = [0x5b; 32];
    let signature = key.sign(&message_digest).to_bytes();
    assert!(matches!(
        authority.verify_barrier_observation_signature(VerifyBarrierObservationSignatureRequest {
            message_digest,
            issuer: binding.principal_id,
            control_domain_id: binding.control_domain_id,
            key_id: binding.key_id,
            signature,
            verified_at_ms: 2_000,
        }),
        Err(IdentityAuthorityError::KeyPurposeMismatch)
    ));
}

#[test]
fn barrier_observation_verification_rejects_tampered_signatures() {
    let root = Root::new("barrier-tampered");
    let key = signing_key(80);
    let authority = IdentityAuthority::open(root.path()).unwrap();
    let binding = authority
        .bootstrap_principal(barrier_bootstrap_request(80, &key))
        .unwrap()
        .binding();
    let message_digest = [0x5c; 32];
    let mut signature = key.sign(&message_digest).to_bytes();
    signature[0] ^= 1;
    assert!(matches!(
        authority.verify_barrier_observation_signature(VerifyBarrierObservationSignatureRequest {
            message_digest,
            issuer: binding.principal_id,
            control_domain_id: binding.control_domain_id,
            key_id: binding.key_id,
            signature,
            verified_at_ms: 2_000,
        }),
        Err(IdentityAuthorityError::InvalidSignature)
    ));
}

/// Test-controlled `AuthorityClock` wall source: `set` moves the reading
/// arbitrarily — including backwards — the minimal deterministic model of a
/// system-clock rollback.  Cloning shares the reading.
#[derive(Clone)]
struct ManualWallSource(Arc<AtomicU64>);

impl ManualWallSource {
    fn at(ms: u64) -> Self {
        Self(Arc::new(AtomicU64::new(ms)))
    }

    fn set(&self, ms: u64) {
        self.0.store(ms, Ordering::Relaxed);
    }
}

impl WallSource for ManualWallSource {
    fn now_ms(&self) -> Result<u64, AuthorityClockError> {
        Ok(self.0.load(Ordering::Relaxed))
    }
}

struct FailingWallSource;

impl WallSource for FailingWallSource {
    fn now_ms(&self) -> Result<u64, AuthorityClockError> {
        Err(AuthorityClockError::WallClockUnavailable)
    }
}

fn at_clock_request(
    key: &SigningKey,
    binding: nlos_identity::IdentityBinding,
    message_digest: [u8; 32],
    idempotency_key: IdempotencyKey,
) -> VerifyCapabilityCommandSignatureAtClockRequest {
    VerifyCapabilityCommandSignatureAtClockRequest {
        message_digest,
        principal: binding.principal_id,
        signature: key.sign(&message_digest).to_bytes(),
        idempotency_key,
    }
}

/// The at-clock variant judges validity at the `AuthorityClock`'s durable wall
/// reading across every validity branch — not-yet-valid, valid, expired —
/// and revocation keeps priority over the clock: an expired-range reading
/// still yields `KeyRevoked`, not `KeyExpired`.  A replayed idempotency key
/// re-reads its original durable reading, so the same command yields the
/// same verdict even after the source has moved on.
#[test]
fn capability_command_at_clock_judges_validity_at_wall_reading() {
    let root = Root::new("at-clock-validity");
    let clock_root = Root::new("at-clock-validity-clock");
    let source = ManualWallSource::at(500);
    let clock = AuthorityClock::open_with_wall_source(clock_root.path(), source.clone())
        .expect("open clock");
    let authority = IdentityAuthority::open(root.path()).unwrap();
    let key = signing_key(21);
    let binding = authority
        .bootstrap_principal(bootstrap_request(21, &key))
        .unwrap()
        .binding();
    // bootstrap_request: valid_from 1_000, valid_until 9_000.

    // Not yet valid: the first wall reading (500) is below valid_from.
    let command = [0x21; 32];
    let early = at_clock_request(
        &key,
        binding,
        command,
        IdempotencyKey::from_bytes([0xE1; 16]),
    );
    assert!(matches!(
        authority.verify_capability_command_signature_at_clock(early, &clock),
        Err(IdentityAuthorityError::KeyNotYetValid)
    ));

    // Replay with the same key after the source has moved on: the original
    // durable reading (500) is re-read, so the verdict is unchanged.
    source.set(5_000);
    assert!(matches!(
        authority.verify_capability_command_signature_at_clock(early, &clock),
        Err(IdentityAuthorityError::KeyNotYetValid)
    ));

    // Valid: a fresh key reads the advanced source (5_000) inside the window.
    let verified = authority
        .verify_capability_command_signature_at_clock(
            at_clock_request(
                &key,
                binding,
                command,
                IdempotencyKey::from_bytes([0xE2; 16]),
            ),
            &clock,
        )
        .expect("valid window must verify at the clock reading");
    assert_eq!(verified.principal_id(), binding.principal_id);
    assert_eq!(verified.key_generation(), Generation::INITIAL);

    // Expired: a fresh key reads 12_000, past valid_until.
    let expired = at_clock_request(
        &key,
        binding,
        command,
        IdempotencyKey::from_bytes([0xE3; 16]),
    );
    source.set(12_000);
    assert!(matches!(
        authority.verify_capability_command_signature_at_clock(expired, &clock),
        Err(IdentityAuthorityError::KeyExpired)
    ));

    // Revocation outranks the clock: with the reading far past valid_until,
    // the revoked key fails as KeyRevoked, not KeyExpired.
    authority
        .revoke_key(RevokeKeyRequest {
            key_id: binding.key_id,
            expected_key_generation: binding.key_generation,
            expected_identity_snapshot_id: binding.identity_snapshot_id,
            idempotency_key: IdempotencyKey::from_bytes([0xE4; 16]),
            revoked_at_ms: 6_000,
        })
        .unwrap();
    let after_revocation = at_clock_request(
        &key,
        binding,
        command,
        IdempotencyKey::from_bytes([0xE5; 16]),
    );
    assert!(matches!(
        authority.verify_capability_command_signature_at_clock(after_revocation, &clock),
        Err(IdentityAuthorityError::KeyRevoked)
    ));

    // Unknown principals still fail closed before any clock-anchored
    // validity judgment.
    let unknown = VerifyCapabilityCommandSignatureAtClockRequest {
        principal: PrincipalId::from_bytes([0x7e; 16]),
        ..after_revocation
    };
    assert!(matches!(
        authority.verify_capability_command_signature_at_clock(unknown, &clock),
        Err(IdentityAuthorityError::PrincipalNotFound(_))
    ));
}

/// A clock that cannot serve a wall reading fails closed as
/// `IdentityAuthorityError::Clock` (typed, source-chained, zero side
/// effects on either authority), and the same verification succeeds once a
/// healthy clock serves the store.
#[test]
fn capability_command_at_clock_fails_closed_when_clock_refuses() {
    let root = Root::new("at-clock-failure");
    let clock_root = Root::new("at-clock-failure-clock");
    let authority = IdentityAuthority::open(root.path()).unwrap();
    let key = signing_key(22);
    let binding = authority
        .bootstrap_principal(bootstrap_request(22, &key))
        .unwrap()
        .binding();
    let broken_clock = AuthorityClock::open_with_wall_source(clock_root.path(), FailingWallSource)
        .expect("open broken clock");

    let request = at_clock_request(
        &key,
        binding,
        [0x22; 32],
        IdempotencyKey::from_bytes([0xE6; 16]),
    );
    let error = authority
        .verify_capability_command_signature_at_clock(request, &broken_clock)
        .expect_err("a refusing clock must fail the verification closed");
    assert!(
        matches!(
            error,
            IdentityAuthorityError::Clock(AuthorityClockError::WallClockUnavailable)
        ),
        "expected Clock(WallClockUnavailable), got {error}"
    );

    let healthy_clock =
        AuthorityClock::open_with_wall_source(clock_root.path(), ManualWallSource::at(5_000))
            .expect("open healthy clock");
    let verified = authority
        .verify_capability_command_signature_at_clock(request, &healthy_clock)
        .expect("verification must succeed at a healthy clock");
    assert_eq!(verified.principal_id(), binding.principal_id);
}
