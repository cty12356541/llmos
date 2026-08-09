use nlos_types::{
    ControlDomainId, Generation, IdempotencyKey, IdentitySnapshotId, KeyId, PrincipalId, ReceiptId,
    SemanticEventId,
};

pub type Ed25519PublicKey = [u8; 32];
pub type Ed25519Signature = [u8; 64];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum KeyPurpose {
    SemanticEventSigning = 1,
}

impl KeyPurpose {
    pub(crate) const fn encode(self) -> i64 {
        self as i64
    }

    pub(crate) fn decode(value: i64) -> Option<Self> {
        match value {
            1 => Some(Self::SemanticEventSigning),
            _ => None,
        }
    }
}

/// Trusted local-bootstrap input. The authority assigns every stable identity;
/// callers do not provide Principal, `ControlDomain`, Key, or snapshot IDs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BootstrapPrincipalRequest {
    pub principal_profile_digest: [u8; 32],
    pub control_domain_policy_digest: [u8; 32],
    pub public_key: Ed25519PublicKey,
    pub key_purpose: KeyPurpose,
    pub key_valid_from_ms: u64,
    pub key_valid_until_ms: u64,
    pub idempotency_key: IdempotencyKey,
    pub created_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IdentityBinding {
    pub principal_id: PrincipalId,
    pub control_domain_id: ControlDomainId,
    pub identity_snapshot_id: IdentitySnapshotId,
    pub snapshot_generation: Generation,
    pub key_id: KeyId,
    pub key_generation: Generation,
    pub key_purpose: KeyPurpose,
    pub public_key: Ed25519PublicKey,
    pub key_valid_from_ms: u64,
    pub key_valid_until_ms: u64,
    pub key_revoked_at_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BootstrapDecision {
    Created(IdentityBinding),
    Replayed(IdentityBinding),
}

impl BootstrapDecision {
    #[must_use]
    pub const fn binding(self) -> IdentityBinding {
        match self {
            Self::Created(binding) | Self::Replayed(binding) => binding,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RevokeKeyRequest {
    pub key_id: KeyId,
    pub expected_key_generation: Generation,
    pub expected_identity_snapshot_id: IdentitySnapshotId,
    pub idempotency_key: IdempotencyKey,
    pub revoked_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct KeyRevocationReceipt {
    pub receipt_id: ReceiptId,
    pub key_id: KeyId,
    pub resulting_key_generation: Generation,
    pub identity_snapshot_id: IdentitySnapshotId,
    pub snapshot_generation: Generation,
    pub revoked_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum KeyRevocationDecision {
    Revoked(KeyRevocationReceipt),
    Replayed(KeyRevocationReceipt),
}

impl KeyRevocationDecision {
    #[must_use]
    pub const fn receipt(self) -> KeyRevocationReceipt {
        match self {
            Self::Revoked(receipt) | Self::Replayed(receipt) => receipt,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VerifySemanticSignatureRequest {
    pub event_id: SemanticEventId,
    pub issuer: PrincipalId,
    pub control_domain_id: ControlDomainId,
    pub key_id: KeyId,
    pub signature: Ed25519Signature,
    pub admitted_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VerifiedSemanticSigner {
    pub principal_id: PrincipalId,
    pub control_domain_id: ControlDomainId,
    pub identity_snapshot_id: IdentitySnapshotId,
    pub snapshot_generation: Generation,
    pub key_id: KeyId,
    pub key_generation: Generation,
}
