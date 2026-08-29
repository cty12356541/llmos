use nlos_identity::{Ed25519Signature, VerifiedSemanticSigner};
use nlos_types::{
    CapabilityId, ControlDomainId, Generation, IdempotencyKey, KeyId, NamespaceId, PrincipalId,
    ReceiptId, TaskId,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapabilityTarget {
    Namespace(NamespaceId),
    Task(TaskId),
}

impl CapabilityTarget {
    pub(crate) const fn kind(self) -> u8 {
        match self {
            Self::Namespace(_) => 1,
            Self::Task(_) => 2,
        }
    }

    pub(crate) const fn bytes(self) -> [u8; 16] {
        match self {
            Self::Namespace(id) => id.into_bytes(),
            Self::Task(id) => id.into_bytes(),
        }
    }

    pub(crate) fn decode(kind: i64, bytes: [u8; 16]) -> Option<Self> {
        match kind {
            1 => Some(Self::Namespace(NamespaceId::from_bytes(bytes))),
            2 => Some(Self::Task(TaskId::from_bytes(bytes))),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapabilityRights(u64);

impl CapabilityRights {
    pub const SEMANTIC_APPEND: Self = Self(1 << 0);
    pub const SEMANTIC_RETRACT: Self = Self(1 << 1);
    pub const SEMANTIC_ADJUDICATE: Self = Self(1 << 2);
    pub const DELEGATE: Self = Self(1 << 3);
    const ALLOWED: u64 = Self::SEMANTIC_APPEND.0
        | Self::SEMANTIC_RETRACT.0
        | Self::SEMANTIC_ADJUDICATE.0
        | Self::DELEGATE.0;

    #[must_use]
    pub const fn from_bits(bits: u64) -> Option<Self> {
        if bits != 0 && bits & !Self::ALLOWED == 0 {
            Some(Self(bits))
        } else {
            None
        }
    }

    #[must_use]
    pub const fn bits(self) -> u64 {
        self.0
    }

    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    #[must_use]
    pub const fn contains(self, required: Self) -> bool {
        self.0 & required.0 == required.0
    }

    #[must_use]
    pub const fn is_subset_of(self, parent: Self) -> bool {
        parent.contains(self)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapabilityHandle {
    pub capability_id: CapabilityId,
    pub generation: Generation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapabilityRecord {
    pub handle: CapabilityHandle,
    pub issuer: PrincipalId,
    pub issuer_control_domain: ControlDomainId,
    pub holder: PrincipalId,
    pub holder_control_domain: ControlDomainId,
    pub target: CapabilityTarget,
    pub rights: CapabilityRights,
    pub purpose_digest: Option<[u8; 32]>,
    pub valid_from_ms: u64,
    pub valid_until_ms: u64,
    pub delegation_depth_remaining: u8,
    pub call_limit: Option<u64>,
    pub parent: Option<CapabilityHandle>,
    pub revoked_at_ms: Option<u64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IssueRootCapabilityRequest {
    pub issuer_key_id: KeyId,
    pub holder_key_id: KeyId,
    pub target: CapabilityTarget,
    pub rights: CapabilityRights,
    pub purpose_digest: Option<[u8; 32]>,
    pub valid_from_ms: u64,
    pub valid_until_ms: u64,
    pub delegation_depth_remaining: u8,
    pub call_limit: Option<u64>,
    pub idempotency_key: IdempotencyKey,
    pub issued_at_ms: u64,
}

/// Signature-gated root issuance (ADR-0010): the exact trusted command plus
/// the acting issuer principal and its Ed25519 signature over
/// `issue_root_command_message`. The durable decision digest covers only
/// `command`, so signed and deprecated unsigned entries stay
/// inter-replayable for identical commands.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SignedIssueRootCapabilityRequest {
    pub command: IssueRootCapabilityRequest,
    pub signer: PrincipalId,
    pub signature: Ed25519Signature,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DelegateCapabilityRequest {
    pub parent: CapabilityHandle,
    pub delegator_key_id: KeyId,
    pub recipient_key_id: KeyId,
    pub target: CapabilityTarget,
    pub rights: CapabilityRights,
    pub purpose_digest: Option<[u8; 32]>,
    pub valid_from_ms: u64,
    pub valid_until_ms: u64,
    pub delegation_depth_remaining: u8,
    pub call_limit: Option<u64>,
    pub idempotency_key: IdempotencyKey,
    pub delegated_at_ms: u64,
}

/// Signature-gated delegation (ADR-0010): the acting delegator principal
/// must sign `delegate_command_message` under its current Identity key
/// binding; the child record's durable issuer column is that signer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SignedDelegateCapabilityRequest {
    pub command: DelegateCapabilityRequest,
    pub signer: PrincipalId,
    pub signature: Ed25519Signature,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapabilityIssueReceipt {
    pub receipt_id: ReceiptId,
    pub capability_id: CapabilityId,
    pub generation: Generation,
    pub parent: Option<CapabilityHandle>,
    pub issued_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapabilityIssueDecision {
    Issued(CapabilityRecord, CapabilityIssueReceipt),
    Replayed(CapabilityRecord, CapabilityIssueReceipt),
}

impl CapabilityIssueDecision {
    #[must_use]
    pub const fn record(self) -> CapabilityRecord {
        match self {
            Self::Issued(record, _) | Self::Replayed(record, _) => record,
        }
    }

    #[must_use]
    pub const fn receipt(self) -> CapabilityIssueReceipt {
        match self {
            Self::Issued(_, receipt) | Self::Replayed(_, receipt) => receipt,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RevokeCapabilityRequest {
    pub handle: CapabilityHandle,
    pub revoker_key_id: KeyId,
    pub idempotency_key: IdempotencyKey,
    pub revoked_at_ms: u64,
}

/// Signature-gated revocation (ADR-0010): the acting revoker principal must
/// sign `revoke_command_message` under its current Identity key binding; the
/// durable revocation receipt records that principal as `revoker`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SignedRevokeCapabilityRequest {
    pub command: RevokeCapabilityRequest,
    pub signer: PrincipalId,
    pub signature: Ed25519Signature,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapabilityRevocationReceipt {
    pub receipt_id: ReceiptId,
    pub capability_id: CapabilityId,
    pub prior_generation: Generation,
    pub resulting_generation: Generation,
    pub revoker: PrincipalId,
    pub revoked_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapabilityRevocationDecision {
    Revoked(CapabilityRevocationReceipt),
    Replayed(CapabilityRevocationReceipt),
}

impl CapabilityRevocationDecision {
    #[must_use]
    pub const fn receipt(self) -> CapabilityRevocationReceipt {
        match self {
            Self::Revoked(receipt) | Self::Replayed(receipt) => receipt,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuthorizeSemanticRequest {
    pub handle: CapabilityHandle,
    pub signer: VerifiedSemanticSigner,
    pub target: CapabilityTarget,
    pub required_right: CapabilityRights,
    pub purpose_digest: Option<[u8; 32]>,
    pub admitted_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SemanticAuthorization {
    pub capability_id: CapabilityId,
    pub generation: Generation,
    pub holder: PrincipalId,
    pub target: CapabilityTarget,
    pub granted_rights: CapabilityRights,
    pub purpose_digest: Option<[u8; 32]>,
}
