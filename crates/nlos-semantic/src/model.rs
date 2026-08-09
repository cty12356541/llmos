use nlos_capability::{CapabilityHandle, CapabilityTarget};
use nlos_types::{
    ControlDomainId, Generation, KeyId, PrincipalId, ProcessId, ReceiptId, SemanticEventId,
};

pub const MAX_CANONICAL_EVENT_BYTES: usize = 65_536;
pub const MAX_CONTENT_BYTES: usize = 1_048_576;
pub const MAX_LINEAGE_ITEMS: usize = 64;
pub const MAX_NONCE_BYTES: usize = 32;
pub const MIN_NONCE_BYTES: usize = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AssertionMode {
    FactFromTool,
    Inference,
    Speculation,
    Directive,
}

impl AssertionMode {
    pub(crate) const fn encode(self) -> u8 {
        match self {
            Self::FactFromTool => 1,
            Self::Inference => 2,
            Self::Speculation => 3,
            Self::Directive => 4,
        }
    }

    pub(crate) const fn decode(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::FactFromTool),
            2 => Some(Self::Inference),
            3 => Some(Self::Speculation),
            4 => Some(Self::Directive),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LocalProcessRef {
    pub process_id: ProcessId,
    pub generation: Generation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnsignedAssertionEvent {
    pub scope: CapabilityTarget,
    pub issuer: PrincipalId,
    pub issuer_execution: LocalProcessRef,
    pub control_domain: ControlDomainId,
    pub issued_at_unix_ns: u64,
    pub nonce: Vec<u8>,
    pub declared_parents: Vec<SemanticEventId>,
    pub valid_until_ms: Option<u64>,
    pub purpose_digest: Option<[u8; 32]>,
    pub content_digest: [u8; 32],
    pub assertion_mode: AssertionMode,
    pub execution_evidence_receipt_id: Option<ReceiptId>,
    pub confidence_bp: Option<u16>,
    pub key_id: KeyId,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TaintFlags(u64);

impl TaintFlags {
    pub const PRIVATE: Self = Self(1 << 0);
    pub const PROVENANCE_INCOMPLETE: Self = Self(1 << 1);
    pub const UNTRUSTED_INGRESS: Self = Self(1 << 2);
    const ALLOWED: u64 =
        Self::PRIVATE.0 | Self::PROVENANCE_INCOMPLETE.0 | Self::UNTRUSTED_INGRESS.0;

    #[must_use]
    pub const fn from_bits(bits: u64) -> Option<Self> {
        if bits & !Self::ALLOWED == 0 {
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
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppendAssertionRequest {
    pub canonical_unsigned_event: Vec<u8>,
    pub claimed_event_id: SemanticEventId,
    pub signature: [u8; 64],
    pub capability: CapabilityHandle,
    pub content_media_type: String,
    pub content_bytes: Vec<u8>,
    pub captured_inputs: Vec<SemanticEventId>,
    pub ingress_taint: TaintFlags,
    pub authz_policy_digest: [u8; 32],
    pub admission_limit_ms: Option<u64>,
    pub admitted_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdmissionDurability {
    Durable,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdmissionReceipt {
    pub receipt_id: ReceiptId,
    pub event_id: SemanticEventId,
    pub log_seq: u64,
    pub admitted_at_ms: u64,
    pub effective_valid_until_ms: Option<u64>,
    pub captured_inputs: Vec<SemanticEventId>,
    pub effective_taint: TaintFlags,
    pub authz_policy_digest: [u8; 32],
    pub durability: AdmissionDurability,
    pub store_principal: PrincipalId,
    pub store_control_domain: ControlDomainId,
    pub store_key_id: KeyId,
    pub store_signature: [u8; 64],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AppendDecision {
    Admitted(AdmissionReceipt),
    Replayed(AdmissionReceipt),
}

impl AppendDecision {
    #[must_use]
    pub fn receipt(&self) -> &AdmissionReceipt {
        match self {
            Self::Admitted(receipt) | Self::Replayed(receipt) => receipt,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticEventRecord {
    pub event_id: SemanticEventId,
    pub canonical_unsigned_event: Vec<u8>,
    pub scope: CapabilityTarget,
    pub issuer: PrincipalId,
    pub control_domain: ControlDomainId,
    pub key_id: KeyId,
    pub content_digest: [u8; 32],
    pub log_seq: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoreSignerError {
    message: String,
}

impl StoreSignerError {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }

    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

pub trait StoreSigner {
    fn principal_id(&self) -> PrincipalId;
    fn control_domain_id(&self) -> ControlDomainId;
    fn key_id(&self) -> KeyId;
    /// Signs a domain-separated 32-byte Semantic authority message.
    ///
    /// # Errors
    ///
    /// Returns a bounded signer error when a Keychain/HSM/backend cannot sign.
    fn sign(&self, message_digest: &[u8; 32]) -> Result<[u8; 64], StoreSignerError>;
}
