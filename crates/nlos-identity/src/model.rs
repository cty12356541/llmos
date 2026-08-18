use nlos_types::{
    ControlDomainId, Generation, IdempotencyKey, IdentitySnapshotId, KeyId, PrincipalId, ReceiptId,
    SemanticEventId,
};

pub type Ed25519PublicKey = [u8; 32];
pub type Ed25519Signature = [u8; 64];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum KeyPurpose {
    SemanticSigning = 1,
    BarrierObservationSigning = 2,
}

impl KeyPurpose {
    pub(crate) const fn encode(self) -> i64 {
        self as i64
    }

    pub(crate) fn decode(value: i64) -> Option<Self> {
        match value {
            1 => Some(Self::SemanticSigning),
            2 => Some(Self::BarrierObservationSigning),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::KeyPurpose;

    #[test]
    fn key_purpose_codec_roundtrips_known_values_and_fails_closed() {
        assert_eq!(
            KeyPurpose::decode(KeyPurpose::SemanticSigning.encode()),
            Some(KeyPurpose::SemanticSigning)
        );
        assert_eq!(
            KeyPurpose::decode(KeyPurpose::BarrierObservationSigning.encode()),
            Some(KeyPurpose::BarrierObservationSigning)
        );
        assert_eq!(KeyPurpose::decode(0), None);
        assert_eq!(KeyPurpose::decode(3), None);
        assert_eq!(KeyPurpose::decode(-1), None);
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
    pub(crate) principal_id: PrincipalId,
    pub(crate) control_domain_id: ControlDomainId,
    pub(crate) identity_snapshot_id: IdentitySnapshotId,
    pub(crate) snapshot_generation: Generation,
    pub(crate) key_id: KeyId,
    pub(crate) key_generation: Generation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VerifySemanticAuthoritySignatureRequest {
    pub message_digest: [u8; 32],
    pub issuer: PrincipalId,
    pub control_domain_id: ControlDomainId,
    pub key_id: KeyId,
    pub signature: Ed25519Signature,
    pub verified_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VerifiedSemanticAuthoritySigner {
    pub(crate) principal_id: PrincipalId,
    pub(crate) control_domain_id: ControlDomainId,
    pub(crate) key_id: KeyId,
    pub(crate) key_generation: Generation,
}

impl VerifiedSemanticAuthoritySigner {
    #[must_use]
    pub const fn principal_id(self) -> PrincipalId {
        self.principal_id
    }

    #[must_use]
    pub const fn control_domain_id(self) -> ControlDomainId {
        self.control_domain_id
    }

    #[must_use]
    pub const fn key_id(self) -> KeyId {
        self.key_id
    }

    #[must_use]
    pub const fn key_generation(self) -> Generation {
        self.key_generation
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VerifyBarrierObservationSignatureRequest {
    pub message_digest: [u8; 32],
    pub issuer: PrincipalId,
    pub control_domain_id: ControlDomainId,
    pub key_id: KeyId,
    pub signature: Ed25519Signature,
    pub verified_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VerifiedBarrierObservationSigner {
    pub(crate) principal_id: PrincipalId,
    pub(crate) control_domain_id: ControlDomainId,
    pub(crate) key_id: KeyId,
    pub(crate) key_generation: Generation,
}

impl VerifiedBarrierObservationSigner {
    #[must_use]
    pub const fn principal_id(self) -> PrincipalId {
        self.principal_id
    }

    #[must_use]
    pub const fn control_domain_id(self) -> ControlDomainId {
        self.control_domain_id
    }

    #[must_use]
    pub const fn key_id(self) -> KeyId {
        self.key_id
    }

    #[must_use]
    pub const fn key_generation(self) -> Generation {
        self.key_generation
    }
}

impl VerifiedSemanticSigner {
    #[must_use]
    pub const fn principal_id(self) -> PrincipalId {
        self.principal_id
    }

    #[must_use]
    pub const fn control_domain_id(self) -> ControlDomainId {
        self.control_domain_id
    }

    #[must_use]
    pub const fn identity_snapshot_id(self) -> IdentitySnapshotId {
        self.identity_snapshot_id
    }

    #[must_use]
    pub const fn snapshot_generation(self) -> Generation {
        self.snapshot_generation
    }

    #[must_use]
    pub const fn key_id(self) -> KeyId {
        self.key_id
    }

    #[must_use]
    pub const fn key_generation(self) -> Generation {
        self.key_generation
    }
}
