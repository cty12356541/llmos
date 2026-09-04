use nlos_capability::{CapabilityHandle, CapabilityTarget};
use nlos_types::{
    CommitPermitId, ControlDomainId, Generation, KeyId, NamespaceId, PrincipalId, ProcessId,
    ReceiptId, SemanticEventId, TaskId, TaskParticipantId,
};

pub const MAX_CANONICAL_EVENT_BYTES: usize = 65_536;
pub const MAX_CONTENT_BYTES: usize = 1_048_576;
pub const MAX_LINEAGE_ITEMS: usize = 64;
pub const MAX_NONCE_BYTES: usize = 32;
pub const MIN_NONCE_BYTES: usize = 16;
pub const MAX_SPEC_CRITERIA: usize = 64;
pub const MAX_SPEC_CAPABILITY_REFS: usize = 64;
pub const MAX_SPEC_EXTENSIONS: usize = 32;
pub const MAX_SPEC_EXTENSION_BYTES: usize = 4_096;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CriterionEffect {
    Hard,
    Soft,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EvaluatorKind {
    Model,
    DeterministicTool,
    Human,
}

impl EvaluatorKind {
    pub(crate) const fn encode(self) -> u8 {
        match self {
            Self::Model => 1,
            Self::DeterministicTool => 2,
            Self::Human => 3,
        }
    }

    pub(crate) const fn decode(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Model),
            2 => Some(Self::DeterministicTool),
            3 => Some(Self::Human),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImmutableEvaluatorReferenceKind {
    Artifact,
    AuthorityPolicy,
}

impl ImmutableEvaluatorReferenceKind {
    pub(crate) const fn encode(self) -> u8 {
        match self {
            Self::Artifact => 1,
            Self::AuthorityPolicy => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ImmutableEvaluatorReference {
    pub kind: ImmutableEvaluatorReferenceKind,
    pub digest: [u8; 32],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CriterionAggregation {
    pub pass_quorum: u16,
    pub fail_quorum: u16,
    pub veto_on_authorized_fail: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IntentCriterion {
    pub description_digest: [u8; 32],
    pub effect: CriterionEffect,
    pub evaluator_kind: EvaluatorKind,
    pub evaluator_ref: ImmutableEvaluatorReference,
    pub target_selector_digest: [u8; 32],
    pub timeout_ms: Option<u64>,
    pub independence_policy_digest: Option<[u8; 32]>,
    pub authority_policy_digest: Option<[u8; 32]>,
    pub risk_policy_digest: Option<[u8; 32]>,
    pub aggregation: CriterionAggregation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IntentConstraints {
    pub resource_vector_digest: [u8; 32],
    pub deadline_ms: Option<u64>,
    pub namespace_root: NamespaceId,
    pub allowed_capability_digests: Vec<[u8; 32]>,
    pub forbidden_capability_digests: Vec<[u8; 32]>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IntentCriticality {
    Low,
    Standard,
    High,
    Critical,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SettlementMode {
    None,
    Automatic,
    Manual,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SettlementTimeoutAction {
    Refund,
    Dispute,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IntentSettlement {
    pub mode: SettlementMode,
    pub hard_criteria_digest: Option<[u8; 32]>,
    pub on_timeout: SettlementTimeoutAction,
    pub challenge_window_ms: Option<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SpecExtension {
    pub id: u32,
    pub value: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IntentSpecBody {
    pub goal_digest: [u8; 32],
    pub acceptance: Vec<IntentCriterion>,
    pub constraints: IntentConstraints,
    pub criticality: IntentCriticality,
    pub settlement: IntentSettlement,
    pub critical_extensions: Vec<SpecExtension>,
    pub noncritical_extensions: Vec<SpecExtension>,
}

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
    pub declassification_receipt_id: Option<ReceiptId>,
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

    #[must_use]
    pub const fn contains(self, mask: Self) -> bool {
        (self.0 & mask.0) == mask.0
    }

    #[must_use]
    pub const fn without(self, mask: Self) -> Self {
        Self(self.0 & !mask.0)
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnsignedSpecEvent {
    pub scope: CapabilityTarget,
    pub issuer: PrincipalId,
    pub issuer_execution: LocalProcessRef,
    pub control_domain: ControlDomainId,
    pub issued_at_unix_ns: u64,
    pub nonce: Vec<u8>,
    pub declared_parents: Vec<SemanticEventId>,
    pub valid_until_ms: Option<u64>,
    pub purpose_digest: Option<[u8; 32]>,
    pub spec_body_digest: [u8; 32],
    pub canonical_spec_body: Vec<u8>,
    pub key_id: KeyId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppendSpecRequest {
    pub canonical_unsigned_event: Vec<u8>,
    pub claimed_event_id: SemanticEventId,
    pub signature: [u8; 64],
    pub capability: CapabilityHandle,
    pub captured_inputs: Vec<SemanticEventId>,
    pub ingress_taint: TaintFlags,
    pub authz_policy_digest: [u8; 32],
    pub admission_limit_ms: Option<u64>,
    pub admitted_at_ms: u64,
}

/// §17.2 `JudgmentEvent.relation`. `Equivalent` and `Contradicts` are
/// symmetric: their source/target endpoints MUST be normalized by `EventId`
/// byte order before canonical encoding (`[SEM-JUDGE-003]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum JudgmentRelation {
    Equivalent,
    Contradicts,
    Entails,
    Supports,
    Refines,
}

impl JudgmentRelation {
    pub(crate) const fn encode(self) -> u8 {
        match self {
            Self::Equivalent => 1,
            Self::Contradicts => 2,
            Self::Entails => 3,
            Self::Supports => 4,
            Self::Refines => 5,
        }
    }

    pub(crate) const fn decode(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Equivalent),
            2 => Some(Self::Contradicts),
            3 => Some(Self::Entails),
            4 => Some(Self::Supports),
            5 => Some(Self::Refines),
            _ => None,
        }
    }

    /// `EQUIVALENT` and `CONTRADICTS` are symmetric per `[SEM-JUDGE-003]`.
    #[must_use]
    pub const fn is_symmetric(self) -> bool {
        matches!(self, Self::Equivalent | Self::Contradicts)
    }
}

/// §17.3 `VerificationEvent.outcome`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VerificationOutcome {
    Pass,
    Fail,
    Inconclusive,
    Error,
}

impl VerificationOutcome {
    pub(crate) const fn encode(self) -> u8 {
        match self {
            Self::Pass => 1,
            Self::Fail => 2,
            Self::Inconclusive => 3,
            Self::Error => 4,
        }
    }

    pub(crate) const fn decode(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Pass),
            2 => Some(Self::Fail),
            3 => Some(Self::Inconclusive),
            4 => Some(Self::Error),
            _ => None,
        }
    }
}

/// §17.4 `RetractionEvent.mode`. Retraction withdraws the target event's
/// visibility claim; it never deletes the target row and never revives a
/// previously retracted event (`[SEM-RETRACT-004]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetractionMode {
    Withdraw,
    Invalidate,
}

impl RetractionMode {
    pub(crate) const fn encode(self) -> u8 {
        match self {
            Self::Withdraw => 1,
            Self::Invalidate => 2,
        }
    }

    pub(crate) const fn decode(value: u8) -> Option<Self> {
        match value {
            1 => Some(Self::Withdraw),
            2 => Some(Self::Invalidate),
            _ => None,
        }
    }
}

/// §17.3 `VerificationTarget.EventTarget` branch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EventVerificationTarget {
    pub event_id: SemanticEventId,
}

/// §17.3 `VerificationTarget.CriterionTarget` minimal Stage B subset.
///
/// `settlement_binding` is intentionally absent: it depends on the not-yet-
/// landed Escrow hold authority, so this profile refuses the field instead of
/// accepting an unverifiable binding.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CriterionVerificationTarget {
    pub spec_id: SemanticEventId,
    pub criterion_id: [u8; 32],
    pub artifact_set_digest: [u8; 32],
    pub procedure_digest: [u8; 32],
    pub evaluation_id: [u8; 32],
    pub producer_control_domains: Vec<ControlDomainId>,
}

/// §17.3 `VerificationTarget` tagged union. Admission accepts exactly one
/// branch; empty or mixed targets cannot be expressed (`[SEM-VERIFY-004]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VerificationTarget {
    Event(EventVerificationTarget),
    Criterion(CriterionVerificationTarget),
}

/// §17.2 `JudgmentEvent` unsigned envelope. All payload fields live inside
/// the canonical bytes, so the `EventId` covers the complete judgment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnsignedJudgmentEvent {
    pub scope: CapabilityTarget,
    pub issuer: PrincipalId,
    pub issuer_execution: LocalProcessRef,
    pub control_domain: ControlDomainId,
    pub issued_at_unix_ns: u64,
    pub nonce: Vec<u8>,
    pub declared_parents: Vec<SemanticEventId>,
    pub valid_until_ms: Option<u64>,
    pub purpose_digest: Option<[u8; 32]>,
    pub key_id: KeyId,
    pub relation: JudgmentRelation,
    pub source: SemanticEventId,
    pub target: SemanticEventId,
    pub context_digest: Option<[u8; 32]>,
    pub evaluator_evidence_receipt_id: ReceiptId,
    pub confidence_bp: Option<u16>,
}

/// §17.3 `VerificationEvent` unsigned envelope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnsignedVerificationEvent {
    pub scope: CapabilityTarget,
    pub issuer: PrincipalId,
    pub issuer_execution: LocalProcessRef,
    pub control_domain: ControlDomainId,
    pub issued_at_unix_ns: u64,
    pub nonce: Vec<u8>,
    pub declared_parents: Vec<SemanticEventId>,
    pub valid_until_ms: Option<u64>,
    pub purpose_digest: Option<[u8; 32]>,
    pub key_id: KeyId,
    pub target: VerificationTarget,
    pub outcome: VerificationOutcome,
    pub evaluator_kind: EvaluatorKind,
    pub procedure_ref: ImmutableEvaluatorReference,
    pub evaluator_evidence_receipt_id: ReceiptId,
    pub evidence: Vec<SemanticEventId>,
}

/// §17.4 `RetractionEvent` unsigned envelope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UnsignedRetractionEvent {
    pub scope: CapabilityTarget,
    pub issuer: PrincipalId,
    pub issuer_execution: LocalProcessRef,
    pub control_domain: ControlDomainId,
    pub issued_at_unix_ns: u64,
    pub nonce: Vec<u8>,
    pub declared_parents: Vec<SemanticEventId>,
    pub valid_until_ms: Option<u64>,
    pub purpose_digest: Option<[u8; 32]>,
    pub key_id: KeyId,
    pub target_event_id: SemanticEventId,
    pub mode: RetractionMode,
    pub reason_digest: Option<[u8; 32]>,
    pub authority_evidence_receipt_id: ReceiptId,
}

/// Admission request shared by the Judgment, Verification, and Retraction
/// typed events. Their complete payload is carried by the canonical bytes, so
/// no out-of-band content/body bytes are admitted.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AppendTypedEventRequest {
    pub canonical_unsigned_event: Vec<u8>,
    pub claimed_event_id: SemanticEventId,
    pub signature: [u8; 64],
    pub capability: CapabilityHandle,
    pub captured_inputs: Vec<SemanticEventId>,
    pub ingress_taint: TaintFlags,
    pub authz_policy_digest: [u8; 32],
    pub admission_limit_ms: Option<u64>,
    pub admitted_at_ms: u64,
}

/// One of the §17.2-§17.4 typed semantic events decoded from canonical bytes.
///
/// Superseded by the crate-internal `typed::TypedEvent` dispatch, which also
/// carries the per-type payload decode; kept as the public discriminant view.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TypedSemanticEvent {
    Judgment(UnsignedJudgmentEvent),
    Verification(UnsignedVerificationEvent),
    Retraction(UnsignedRetractionEvent),
}

impl TypedSemanticEvent {
    #[must_use]
    pub const fn event_type_discriminant(&self) -> u8 {
        match self {
            Self::Judgment(_) => 2,
            Self::Verification(_) => 3,
            Self::Retraction(_) => 4,
        }
    }
}

/// Durable retraction fact for one target event. This is an observation of
/// the admitted retraction event; it never filters or rewrites the target
/// row, and no visibility view semantics are derived here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RetractionRecord {
    pub target_event_id: SemanticEventId,
    pub retraction_event_id: SemanticEventId,
    pub mode: RetractionMode,
    pub reason_digest: Option<[u8; 32]>,
    pub retracted_by: PrincipalId,
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

/// Immutable owner-issued proof that an admitted event crossed a durable
/// checkpoint after admission. This receipt is optional when the authority
/// directly issued a `Durable` [`AdmissionReceipt`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DurabilityReceipt {
    pub receipt_id: ReceiptId,
    pub event_id: SemanticEventId,
    pub durable_checkpoint_id: [u8; 32],
    pub durable_at_ms: u64,
    pub store_signature: [u8; 64],
}

/// Durable transport status for one Semantic admission outbox item.
///
/// `acknowledged_at_ms` is only an outbox transport observation. It is not a
/// Semantic checkpoint or publication proof.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SemanticOutboxRecord {
    pub log_seq: u64,
    pub event_id: SemanticEventId,
    pub receipt_id: ReceiptId,
    pub acknowledged_at_ms: Option<u64>,
}

/// A transport consumer's owner-bound acknowledgement observation.
///
/// The event/log/receipt triple is supplied by the consumer and must match
/// the `SemanticAuthority` readback. `acknowledged_at_ms` is a monotonic
/// transport high-water only; it is not a checkpoint or publication proof.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AcknowledgeOutboxRequest {
    pub event_id: SemanticEventId,
    pub log_seq: u64,
    pub receipt_id: ReceiptId,
    pub acknowledged_at_ms: u64,
}

/// Request for the Semantic authority to make one already-admitted event
/// visible as a Task publication. Publication is a Semantic-domain fact: the
/// owner re-reads the event and its durable Admission/Durability receipts and
/// derives the checkpoint/receipt identities instead of trusting caller
/// supplied digests.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PublishSemanticPublicationRequest {
    pub task_id: TaskId,
    pub permit_id: CommitPermitId,
    pub write_set_root: [u8; 32],
    pub event_id: SemanticEventId,
    pub target: CapabilityTarget,
    pub admission_receipt_id: ReceiptId,
    pub durability_receipt_id: Option<ReceiptId>,
    pub published_at_ms: u64,
}

/// Immutable SemanticAuthority-owned proof that one admitted event crossed
/// the local publication boundary. `semantic_checkpoint_after` is a
/// deterministic local log-prefix checkpoint; it is not a distributed/global
/// vector checkpoint and must not be advertised as one.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SemanticPublicationReceipt {
    pub receipt_id: ReceiptId,
    pub task_id: TaskId,
    pub permit_id: CommitPermitId,
    pub write_set_root: [u8; 32],
    pub event_id: SemanticEventId,
    pub target: CapabilityTarget,
    pub log_seq: u64,
    pub admission_receipt_id: ReceiptId,
    pub durability_receipt_id: Option<ReceiptId>,
    pub semantic_checkpoint_after: [u8; 32],
    pub created_at_ms: u64,
}

/// Idempotent result of the Semantic publication boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SemanticPublicationDecision {
    Published(SemanticPublicationReceipt),
    Replayed(SemanticPublicationReceipt),
}

impl SemanticPublicationDecision {
    #[must_use]
    pub const fn receipt(self) -> SemanticPublicationReceipt {
        match self {
            Self::Published(receipt) | Self::Replayed(receipt) => receipt,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutboxAckDecision {
    Recorded(SemanticOutboxRecord),
    Replayed(SemanticOutboxRecord),
}
impl OutboxAckDecision {
    #[must_use]
    pub const fn record(self) -> SemanticOutboxRecord {
        match self {
            Self::Recorded(record) | Self::Replayed(record) => record,
        }
    }
}

/// Durable authority-issued identity of the Semantic admission endpoint.
///
/// Consumers must verify transported values by exact readback from the
/// owning [`crate::SemanticAuthority`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SemanticAdmissionEndpointProof {
    pub participant_id: TaskParticipantId,
    pub participant_generation: Generation,
    pub admission_receipt_id: ReceiptId,
}

/// Immutable authorization to remove bounded taint labels from one admission
/// (`[SEM-DECLASS-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeclassificationReceipt {
    pub receipt_id: ReceiptId,
    pub holder: PrincipalId,
    pub scope: CapabilityTarget,
    pub source_events: Vec<SemanticEventId>,
    pub removed_labels: TaintFlags,
    pub purpose_digest: Option<[u8; 32]>,
    pub expires_at_ms: u64,
    pub nonce: Vec<u8>,
    pub issued_at_ms: u64,
    pub store_principal: PrincipalId,
    pub store_control_domain: ControlDomainId,
    pub store_key_id: KeyId,
    pub store_signature: [u8; 64],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IssueDeclassificationReceiptRequest {
    pub holder: PrincipalId,
    pub scope: CapabilityTarget,
    pub source_events: Vec<SemanticEventId>,
    pub removed_labels: TaintFlags,
    pub purpose_digest: Option<[u8; 32]>,
    pub expires_at_ms: u64,
    pub nonce: Vec<u8>,
    pub issued_at_ms: u64,
    pub capability: CapabilityHandle,
    pub adjudicator_key_id: KeyId,
    pub adjudicator_signature: [u8; 64],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IssueDeclassificationDecision {
    Issued(DeclassificationReceipt),
    Replayed(DeclassificationReceipt),
}

impl IssueDeclassificationDecision {
    #[must_use]
    pub fn receipt(&self) -> &DeclassificationReceipt {
        match self {
            Self::Issued(receipt) | Self::Replayed(receipt) => receipt,
        }
    }
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
    pub payload_identity: SemanticPayloadIdentity,
    pub log_seq: u64,
}

impl SemanticEventRecord {
    /// Stable scope kind used by `TaskWriteSet` publication declarations.
    #[must_use]
    pub const fn scope_kind(&self) -> u8 {
        match self.scope {
            CapabilityTarget::Namespace(_) => 1,
            CapabilityTarget::Task(_) => 2,
        }
    }

    /// Stable 16-byte scope identity used by `TaskWriteSet` publication
    /// declarations.
    #[must_use]
    pub const fn scope_id(&self) -> [u8; 16] {
        match self.scope {
            CapabilityTarget::Namespace(id) => id.into_bytes(),
            CapabilityTarget::Task(id) => id.into_bytes(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SemanticPayloadIdentity {
    AssertionContent([u8; 32]),
    IntentSpecBody([u8; 32]),
    /// Judgment/Verification/Retraction carry their complete payload inside
    /// the canonical envelope bytes; no detached digest object exists.
    Structural,
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
