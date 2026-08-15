//! Domain model for the durable task authority.
//!
//! This module contains the caller-supplied specifications, the durable
//! record types returned by inspections, the lifecycle enums, and the
//! deterministic digest/identity formulas used by this slice. All digest
//! formulas are domain-separated SHA-256 placeholders: they fix the
//! deterministic shape required by `[TASK-EFFECT-ID-001]` but do not yet
//! implement the canonical deterministic-CBOR encoding or signatures
//! mandated by the full §25.1 contract.

use nlos_types::{
    AgentInstanceId, ArtifactId, CallId, CancellationScopeId, CommitPermitId, DeviceId, DriverId,
    Generation, IdempotencyKey, IsolationDomainId, NamespaceId, OperationId, ProcessId, QuoteId,
    ReceiptId, ReservationId, ResourceAccountId, SemanticEventId, TaskAttemptId, TaskId,
    TaskParticipantId, TaskSnapshotId,
};
use sha2::{Digest, Sha256};

use crate::TaskStoreError;

/// Computes the domain-separated placeholder digest of the empty task effect
/// history.
///
/// `[TASK-EFFECT-ID-001]` requires the initial `TaskHead` history to be the
/// fixed formula over empty entries. This slice uses
/// `SHA-256("llmos/task-effect-history/v1" || 0x80)` where `0x80` is the CBOR
/// empty array standing in for the deterministic-CBOR encoding of an empty
/// entry list. The constant is stable across restarts and platforms.
#[must_use]
pub fn empty_effect_history_root() -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/task-effect-history/v1");
    hasher.update([0x80u8]);
    hasher.finalize().into()
}

/// Durable specification of a registered Task.
///
/// Registration is idempotent on `task_id`: repeating the exact same
/// specification returns the existing record, while reusing the ID with a
/// different generation is rejected fail-closed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskSpec {
    pub task_id: TaskId,
    pub task_generation: Generation,
    /// Caller-supplied registration time in milliseconds (wall-clock is not
    /// part of the authority's causality; it is stored for observability).
    pub registered_at_ms: i64,
}

/// Frozen-input core shared by a full `TaskSnapshot` and its durable receipt.
///
/// `[TASK-SNAPSHOT-001/002]` requires a causal-closed cut over the current
/// `TaskHead`, the durable effect-history root, and the retry-fence epoch,
/// frozen before the attempt starts. In this slice the snapshot is
/// The bundle is immutable once inserted. Schema v10 can additionally bind
/// it to a durable [`TaskSnapshotReceiptRecord`]; legacy attempts may retain
/// an unreceipted bundle during forward migration but cannot be confused with
/// the receipted registration API.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SnapshotBundle {
    pub snapshot_id: TaskSnapshotId,
    pub snapshot_digest: [u8; 32],
    pub expected_head_commit_seq: u64,
    pub effect_history_root: [u8; 32],
    pub retry_fence_epoch: u64,
}

/// Consistency actually achieved by a snapshot builder. Ordering is not a
/// strength relation; callers must compare the exact requested contract.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotConsistency {
    Causal,
    SerializableDomain,
    LinearizableAuthority,
    MixedNonSettleable,
}

impl SnapshotConsistency {
    pub(crate) const fn code(self) -> i64 {
        match self {
            Self::Causal => 0,
            Self::SerializableDomain => 1,
            Self::LinearizableAuthority => 2,
            Self::MixedNonSettleable => 3,
        }
    }

    pub(crate) fn from_code(code: i64) -> Result<Self, TaskStoreError> {
        match code {
            0 => Ok(Self::Causal),
            1 => Ok(Self::SerializableDomain),
            2 => Ok(Self::LinearizableAuthority),
            3 => Ok(Self::MixedNonSettleable),
            _ => Err(TaskStoreError::CorruptRecord(
                "unknown snapshot consistency",
            )),
        }
    }
}

/// Immutable receipt supplied by the snapshot builder. Signature bytes are
/// stored and replayed exactly; cryptographic verification belongs to the
/// Identity/key-trust authority integration rather than this local slice.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskSnapshotReceiptSpec {
    pub task_id: TaskId,
    pub snapshot: SnapshotBundle,
    pub receipt_id: ReceiptId,
    pub builder_id: [u8; 16],
    pub builder_version_digest: [u8; 32],
    pub per_authority_checkpoint_receipts: Vec<ReceiptId>,
    pub dependency_closure_root: [u8; 32],
    pub semantic_resolver_digest: [u8; 32],
    pub canonical_iteration_digest: [u8; 32],
    pub achieved_consistency: SnapshotConsistency,
    pub built_at_ms: i64,
    pub authority_id: [u8; 16],
    pub key_id: [u8; 16],
    pub signature: [u8; 64],
}

/// Durable readback of one `TaskSnapshotReceipt`.
pub type TaskSnapshotReceiptRecord = TaskSnapshotReceiptSpec;

/// Durable specification of one `TaskAttempt` registration.
///
/// Every retry or parallel candidate MUST use an independent
/// `attempt_id`/`attempt_generation` pair with its own cancellation scope
/// (`[TASK-ATTEMPT-001]`); the authority never rewrites an old attempt to
/// fake "the same success".
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AttemptSpec {
    pub task_id: TaskId,
    pub attempt_id: TaskAttemptId,
    pub attempt_generation: Generation,
    pub snapshot: SnapshotBundle,
    pub cancellation_scope_id: CancellationScopeId,
    pub cancellation_generation: Generation,
    pub idempotency_key: IdempotencyKey,
    pub registered_at_ms: i64,
}

/// One exact Artifact head revision observed while sealing a verified
/// `TaskWriteSet` read set. `revision=0` with `digest=None` represents an
/// artifact that exists but has no committed head yet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskWriteSetArtifactRead {
    pub artifact_id: ArtifactId,
    pub expected_head_revision: u64,
    pub expected_head_digest: Option<[u8; 32]>,
}

/// Proposed Artifact revision declared by a `TaskWriteSet` before a permit is
/// issued. The digest/size are proposal bytes; the current head and target
/// revision are checked against `ArtifactAuthority` during seal.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct TaskWriteSetArtifactWriteRequest {
    pub artifact_id: ArtifactId,
    pub expected_head_revision: u64,
    pub proposed_revision: u64,
    pub content_digest: [u8; 32],
    pub size_bytes: u64,
}

/// Durable Artifact write declaration copied from the authority-verified
/// seal. The proposed content is not called published until a later
/// Artifact publication plan/receipt path consumes it.
pub type TaskWriteSetArtifactWrite = TaskWriteSetArtifactWriteRequest;

/// Caller-declared current Process/AgentInstance/IsolationDomain binding.
/// Every field is verified by `ProcessAuthority` before it enters a durable
/// `TaskWriteSet`; the endpoint proof is supplied by that owner authority.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskWriteSetProcessBindingRequest {
    pub process_id: ProcessId,
    pub process_generation: Generation,
    pub process_fencing_token: [u8; 32],
    pub agent_instance_id: AgentInstanceId,
    pub agent_instance_generation: Generation,
    pub isolation_domain_id: IsolationDomainId,
    pub isolation_domain_generation: Generation,
    pub isolation_domain_fencing_token: [u8; 32],
}

impl From<nlos_process::ActiveProcessBinding> for TaskWriteSetProcessBindingRequest {
    fn from(binding: nlos_process::ActiveProcessBinding) -> Self {
        Self {
            process_id: binding.process_id,
            process_generation: binding.process_generation,
            process_fencing_token: binding.process_fencing_token,
            agent_instance_id: binding.agent_instance_id,
            agent_instance_generation: binding.agent_instance_generation,
            isolation_domain_id: binding.isolation_domain_id,
            isolation_domain_generation: binding.isolation_domain_generation,
            isolation_domain_fencing_token: binding.isolation_domain_fencing_token,
        }
    }
}

/// Owner-read Process binding persisted in the sealed write set, including
/// the Process authority's participant endpoint proof.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskWriteSetProcessBinding {
    pub process_id: ProcessId,
    pub process_generation: Generation,
    pub process_fencing_token: [u8; 32],
    pub agent_instance_id: AgentInstanceId,
    pub agent_instance_generation: Generation,
    pub isolation_domain_id: IsolationDomainId,
    pub isolation_domain_generation: Generation,
    pub isolation_domain_fencing_token: [u8; 32],
    pub participant_id: TaskParticipantId,
    pub participant_generation: Generation,
    pub admission_receipt_id: ReceiptId,
}

/// Caller-declared Semantic events read by the `TaskWriteSet`. The Semantic
/// authority must confirm both the durable log sequence and canonical bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskWriteSetSemanticRead {
    pub event_id: SemanticEventId,
    pub expected_log_seq: u64,
    pub expected_canonical_digest: [u8; 32],
}

/// Semantic scope targeted by a staged append. The owner authority confirms
/// that this caller-declared scope matches the admitted event envelope.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum TaskWriteSetSemanticTarget {
    Namespace(NamespaceId),
    Task(TaskId),
}

impl TaskWriteSetSemanticTarget {
    pub(crate) const fn kind(self) -> u8 {
        match self {
            Self::Namespace(_) => 1,
            Self::Task(_) => 2,
        }
    }

    pub(crate) const fn id(self) -> [u8; 16] {
        match self {
            Self::Namespace(id) => id.into_bytes(),
            Self::Task(id) => id.into_bytes(),
        }
    }
}

/// Durability requested for a Semantic staging item. This slice accepts only
/// the `SemanticAuthority` direct durable-admission path.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum TaskWriteSetSemanticRequiredDurability {
    Durable,
}

impl TaskWriteSetSemanticRequiredDurability {
    pub(crate) const fn code() -> u8 {
        2
    }
}

/// Caller-declared Semantic append that must already have an owner-issued
/// durable `AdmissionReceipt` before the `TaskWriteSet` can be sealed.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct TaskWriteSetSemanticAppendRequest {
    pub event_id: SemanticEventId,
    pub target: TaskWriteSetSemanticTarget,
    pub required_durability: TaskWriteSetSemanticRequiredDurability,
    /// Expected admission-policy digest from the Semantic authority. The
    /// sealing API compares this declaration with the owner-issued
    /// `AdmissionReceipt`; it is not trusted as a receipt fact.
    pub expected_admission_policy_digest: [u8; 32],
    /// Optional owner-issued proof when the event crossed a later durable
    /// checkpoint. Direct `Durable` `AdmissionReceipt` admission does not need
    /// a second receipt.
    pub durability_receipt_id: Option<ReceiptId>,
}

/// Owner-verified Semantic append persisted in the sealed `TaskWriteSet`.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct TaskWriteSetSemanticAppend {
    pub event_id: SemanticEventId,
    pub target: TaskWriteSetSemanticTarget,
    pub required_durability: TaskWriteSetSemanticRequiredDurability,
    pub admission_receipt_id: ReceiptId,
    /// Owner-verified admission-policy declaration. `None` is retained only
    /// for pre-v23 historical rows that never declared a policy digest.
    pub admission_policy_digest: Option<[u8; 32]>,
    pub durability_receipt_id: Option<ReceiptId>,
}

/// Caller-declared Reservation binding expected by a planned action. Stable
/// owner fields are persisted after `ResourceAuthority` readback; activation
/// tokens are never copied into the write-set root.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskWriteSetResourceReservationRequest {
    pub reservation_id: ReservationId,
    pub expected_call_id: CallId,
    pub expected_operation_id: OperationId,
    pub expected_quote_id: QuoteId,
}

/// Owner-read stable Reservation binding persisted in the sealed write set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskWriteSetResourceReservation {
    pub reservation_id: ReservationId,
    pub account_id: ResourceAccountId,
    pub quote_id: QuoteId,
    pub call_id: CallId,
    pub operation_id: OperationId,
    pub driver_id: DriverId,
    pub device_id: DeviceId,
    pub driver_generation: Generation,
    pub driver_fencing_token: [u8; 32],
    pub upper_bound: u64,
}

/// Owner endpoint kind attached to one planned effect slot. The kind is
/// deliberately finite: every endpoint that enters a `TaskWriteSet` must have
/// a concrete authority capable of returning a durable proof.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskWriteSetEffectEndpointKind {
    ArtifactHead,
    SemanticAdmission,
    ProcessBinding,
    DriverGateway,
    ResourceLedger,
    OperationBinding,
}

impl TaskWriteSetEffectEndpointKind {
    pub(crate) const fn code(self) -> u8 {
        match self {
            Self::ArtifactHead => 1,
            Self::SemanticAdmission => 2,
            Self::ProcessBinding => 3,
            Self::DriverGateway => 4,
            Self::ResourceLedger => 5,
            Self::OperationBinding => 6,
        }
    }

    pub(crate) fn from_code(code: i64) -> Result<Self, TaskStoreError> {
        match code {
            1 => Ok(Self::ArtifactHead),
            2 => Ok(Self::SemanticAdmission),
            3 => Ok(Self::ProcessBinding),
            4 => Ok(Self::DriverGateway),
            5 => Ok(Self::ResourceLedger),
            6 => Ok(Self::OperationBinding),
            _ => Err(TaskStoreError::CorruptRecord(
                "TaskWriteSet effect endpoint kind",
            )),
        }
    }
}

/// Caller-declared owner endpoint needed by one planned effect. The caller
/// supplies only the stable object identity (or the authority-wide Semantic
/// admission endpoint); the owner proof is read back during sealing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskWriteSetEffectEndpointRequest {
    ArtifactHead {
        effect_seq: u64,
        artifact_id: ArtifactId,
    },
    SemanticAdmission {
        effect_seq: u64,
    },
    ProcessBinding {
        effect_seq: u64,
        process_id: ProcessId,
        expected_process_generation: Generation,
    },
    DriverGateway {
        effect_seq: u64,
        driver_id: DriverId,
        expected_driver_generation: Generation,
    },
    ResourceLedger {
        effect_seq: u64,
        account_id: ResourceAccountId,
        expected_account_generation: Generation,
    },
    OperationBinding {
        effect_seq: u64,
        operation_id: OperationId,
        expected_operation_generation: Generation,
    },
}

impl TaskWriteSetEffectEndpointRequest {
    pub(crate) const fn effect_seq(self) -> u64 {
        match self {
            Self::ArtifactHead { effect_seq, .. }
            | Self::SemanticAdmission { effect_seq }
            | Self::ProcessBinding { effect_seq, .. }
            | Self::DriverGateway { effect_seq, .. }
            | Self::ResourceLedger { effect_seq, .. }
            | Self::OperationBinding { effect_seq, .. } => effect_seq,
        }
    }
}

/// Authority-read endpoint proof persisted beside the planned effect set.
/// `object_id` is the fixed-width stable ID for the endpoint kind; Semantic
/// admission is authority-wide and therefore uses an all-zero object ID.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskWriteSetEffectEndpoint {
    pub effect_seq: u64,
    pub kind: TaskWriteSetEffectEndpointKind,
    pub object_id: [u8; 16],
    pub participant_id: TaskParticipantId,
    pub participant_generation: Generation,
    pub admission_receipt_id: ReceiptId,
}

/// Authority-verified `TaskWriteSet` seal request for the snapshot/read-set
/// slice. Owner authorities are queried by the sealing API; callers provide
/// only stable object identities and the revision they intend to read.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskWriteSetRequest {
    pub task_id: TaskId,
    pub attempt_id: TaskAttemptId,
    pub attempt_generation: Generation,
    pub artifact_reads: Vec<TaskWriteSetArtifactRead>,
    pub artifact_writes: Vec<TaskWriteSetArtifactWriteRequest>,
    pub process_binding: Option<TaskWriteSetProcessBindingRequest>,
    pub semantic_reads: Vec<TaskWriteSetSemanticRead>,
    pub semantic_appends: Vec<TaskWriteSetSemanticAppendRequest>,
    pub resource_reservations: Vec<TaskWriteSetResourceReservationRequest>,
    /// Planned effect slots sealed before `CommitPermit` issuance. The
    /// authority validates each descriptor against the task generation and
    /// persists the exact ordered declaration.
    pub planned_effects: Vec<PlannedEffect>,
    /// Owner endpoint proofs to bind to the declared effect slots. The
    /// sealing API reads the proof from the relevant authority and rejects
    /// an endpoint that is not already in the OPEN participant registry.
    pub effect_endpoints: Vec<TaskWriteSetEffectEndpointRequest>,
    pub idempotency_key: IdempotencyKey,
    pub sealed_at_ms: i64,
}

/// Durable authority-derived `TaskWriteSet` seal. The root covers the
/// receipted snapshot, TaskHead/fence, group/participant bindings, exact
/// owner-read sets, and (when present) the ordered planned effect set.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskWriteSetRecord {
    pub task_id: TaskId,
    pub attempt_id: TaskAttemptId,
    pub attempt_generation: Generation,
    pub idempotency_key: IdempotencyKey,
    pub snapshot_id: TaskSnapshotId,
    pub snapshot_receipt_id: ReceiptId,
    pub expected_head_commit_seq: u64,
    pub effect_history_root: [u8; 32],
    pub retry_fence_epoch: u64,
    pub group_binding: Option<crate::TaskGroupCommitBinding>,
    pub participant_registry_binding: crate::ParticipantRegistryBinding,
    pub artifact_reads: Vec<TaskWriteSetArtifactRead>,
    pub artifact_writes: Vec<TaskWriteSetArtifactWrite>,
    pub process_binding: Option<TaskWriteSetProcessBinding>,
    pub semantic_reads: Vec<TaskWriteSetSemanticRead>,
    pub semantic_appends: Vec<TaskWriteSetSemanticAppend>,
    pub resource_reservations: Vec<TaskWriteSetResourceReservation>,
    pub planned_effects: Vec<PlannedEffect>,
    pub effect_endpoints: Vec<TaskWriteSetEffectEndpoint>,
    pub artifact_read_set_root: [u8; 32],
    /// Zero for legacy/no-Semantic-append seals; otherwise the canonical
    /// owner-verified Semantic append declaration root.
    pub semantic_append_set_root: [u8; 32],
    /// Zero for legacy/no-artifact-write seals; otherwise the canonical
    /// proposed Artifact write declaration root.
    pub artifact_write_set_root: [u8; 32],
    pub semantic_read_set_root: [u8; 32],
    pub resource_reservation_set_root: [u8; 32],
    /// Zero for legacy/no-effect seals; otherwise the canonical effect-set
    /// root used by `EffectSlot` issuance.
    pub effect_set_root: [u8; 32],
    /// Zero for legacy/no-endpoint seals; otherwise the canonical owner
    /// endpoint proof root bound to the planned effect sequence.
    pub effect_endpoint_set_root: [u8; 32],
    pub write_set_root: [u8; 32],
    pub sealed_at_ms: i64,
}

/// Idempotent result of sealing a `TaskWriteSet`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskWriteSetDecision {
    Sealed(TaskWriteSetRecord),
    Replayed(TaskWriteSetRecord),
}

impl TaskWriteSetDecision {
    #[must_use]
    pub const fn record(&self) -> &TaskWriteSetRecord {
        match self {
            Self::Sealed(record) | Self::Replayed(record) => record,
        }
    }
}

pub(crate) fn artifact_read_set_root(reads: &[TaskWriteSetArtifactRead]) -> [u8; 32] {
    let mut ordered = reads.to_vec();
    ordered.sort_unstable_by_key(|read| read.artifact_id.into_bytes());
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/task-write-set-artifact-reads/v1");
    hasher.update((ordered.len() as u64).to_be_bytes());
    for read in ordered {
        hasher.update(read.artifact_id.as_bytes());
        hasher.update(read.expected_head_revision.to_be_bytes());
        match read.expected_head_digest {
            Some(digest) => {
                hasher.update([1u8]);
                hasher.update(digest);
            }
            None => hasher.update([0u8]),
        }
    }
    hasher.finalize().into()
}

pub(crate) fn artifact_write_set_root(writes: &[TaskWriteSetArtifactWrite]) -> [u8; 32] {
    if writes.is_empty() {
        return [0; 32];
    }
    let mut ordered = writes.to_vec();
    ordered.sort_unstable_by_key(|write| {
        (
            write.artifact_id.into_bytes(),
            write.proposed_revision,
            write.content_digest,
        )
    });
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/task-write-set-artifact-writes/v1");
    hasher.update((ordered.len() as u64).to_be_bytes());
    for write in ordered {
        hasher.update(write.artifact_id.as_bytes());
        hasher.update(write.expected_head_revision.to_be_bytes());
        hasher.update(write.proposed_revision.to_be_bytes());
        hasher.update(write.content_digest);
        hasher.update(write.size_bytes.to_be_bytes());
    }
    hasher.finalize().into()
}

pub(crate) fn semantic_read_set_root(reads: &[TaskWriteSetSemanticRead]) -> [u8; 32] {
    if reads.is_empty() {
        return [0; 32];
    }
    let mut ordered = reads.to_vec();
    ordered.sort_unstable_by_key(|read| read.event_id);
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/task-write-set-semantic-reads/v1");
    hasher.update((ordered.len() as u64).to_be_bytes());
    for read in ordered {
        hasher.update(read.event_id.as_bytes());
        hasher.update(read.expected_log_seq.to_be_bytes());
        hasher.update(read.expected_canonical_digest);
    }
    hasher.finalize().into()
}

pub(crate) fn semantic_append_set_root(appends: &[TaskWriteSetSemanticAppend]) -> [u8; 32] {
    if appends.is_empty() {
        return [0; 32];
    }
    let mut ordered = appends.to_vec();
    ordered.sort_unstable_by_key(|append| append.event_id);
    let has_durability_receipts = ordered
        .iter()
        .any(|append| append.durability_receipt_id.is_some());
    let has_admission_policy_digests = ordered
        .iter()
        .any(|append| append.admission_policy_digest.is_some());
    let mut hasher = Sha256::new();
    hasher.update(if has_admission_policy_digests {
        b"llmos/task-write-set-semantic-appends/v3".as_slice()
    } else if has_durability_receipts {
        b"llmos/task-write-set-semantic-appends/v2".as_slice()
    } else {
        b"llmos/task-write-set-semantic-appends/v1".as_slice()
    });
    hasher.update((ordered.len() as u64).to_be_bytes());
    for append in ordered {
        hasher.update(append.event_id.as_bytes());
        hasher.update([append.target.kind()]);
        hasher.update(append.target.id());
        hasher.update([TaskWriteSetSemanticRequiredDurability::code()]);
        hasher.update(append.admission_receipt_id.as_bytes());
        if has_admission_policy_digests {
            match append.admission_policy_digest {
                Some(digest) => {
                    hasher.update([1u8]);
                    hasher.update(digest);
                }
                None => hasher.update([0u8]),
            }
        }
        if has_durability_receipts {
            match append.durability_receipt_id {
                Some(receipt_id) => {
                    hasher.update([1u8]);
                    hasher.update(receipt_id.as_bytes());
                }
                None => hasher.update([0u8]),
            }
        }
    }
    hasher.finalize().into()
}

pub(crate) fn semantic_canonical_digest(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/task-write-set-semantic-event/v1");
    hasher.update(bytes);
    hasher.finalize().into()
}

pub(crate) fn resource_reservation_set_root(
    reservations: &[TaskWriteSetResourceReservation],
) -> [u8; 32] {
    if reservations.is_empty() {
        return [0; 32];
    }
    let mut ordered = reservations.to_vec();
    ordered.sort_unstable_by_key(|reservation| reservation.reservation_id);
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/task-write-set-resource-reservations/v1");
    hasher.update((ordered.len() as u64).to_be_bytes());
    for reservation in ordered {
        hasher.update(reservation.reservation_id.as_bytes());
        hasher.update(reservation.account_id.as_bytes());
        hasher.update(reservation.quote_id.as_bytes());
        hasher.update(reservation.call_id.as_bytes());
        hasher.update(reservation.operation_id.as_bytes());
        hasher.update(reservation.driver_id.as_bytes());
        hasher.update(reservation.device_id.as_bytes());
        hasher.update(reservation.driver_generation.get().to_be_bytes());
        hasher.update(reservation.driver_fencing_token);
        hasher.update(reservation.upper_bound.to_be_bytes());
    }
    hasher.finalize().into()
}

pub(crate) fn effect_endpoint_set_root(endpoints: &[TaskWriteSetEffectEndpoint]) -> [u8; 32] {
    if endpoints.is_empty() {
        return [0; 32];
    }
    let mut ordered = endpoints.to_vec();
    ordered.sort_unstable_by_key(|endpoint| {
        (
            endpoint.effect_seq,
            endpoint.kind.code(),
            endpoint.object_id,
            endpoint.participant_id.into_bytes(),
        )
    });
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/task-write-set-effect-endpoints/v1");
    hasher.update((ordered.len() as u64).to_be_bytes());
    for endpoint in ordered {
        hasher.update(endpoint.effect_seq.to_be_bytes());
        hasher.update([endpoint.kind.code()]);
        hasher.update(endpoint.object_id);
        hasher.update(endpoint.participant_id.as_bytes());
        hasher.update(endpoint.participant_generation.get().to_be_bytes());
        hasher.update(endpoint.admission_receipt_id.as_bytes());
    }
    hasher.finalize().into()
}

pub(crate) fn task_write_set_root(record: &TaskWriteSetRecord) -> [u8; 32] {
    let has_artifact_writes = !record.artifact_writes.is_empty();
    let has_semantic_appends = !record.semantic_appends.is_empty();
    let has_effects = !record.planned_effects.is_empty();
    let has_effect_endpoints = !record.effect_endpoints.is_empty();
    let extended = record.process_binding.is_some()
        || !record.semantic_reads.is_empty()
        || has_semantic_appends
        || !record.resource_reservations.is_empty()
        || has_effects
        || has_effect_endpoints;
    let mut hasher = Sha256::new();
    hasher.update(if has_semantic_appends {
        b"llmos/task-write-set/v6".as_slice()
    } else if has_artifact_writes {
        b"llmos/task-write-set/v5".as_slice()
    } else if has_effect_endpoints {
        b"llmos/task-write-set/v4".as_slice()
    } else if has_effects {
        b"llmos/task-write-set/v3".as_slice()
    } else if extended {
        b"llmos/task-write-set/v2".as_slice()
    } else {
        b"llmos/task-write-set/v1".as_slice()
    });
    hasher.update(record.task_id.as_bytes());
    hasher.update(record.attempt_id.as_bytes());
    hasher.update(record.attempt_generation.get().to_be_bytes());
    hasher.update(record.snapshot_id.as_bytes());
    hasher.update(record.snapshot_receipt_id.as_bytes());
    hasher.update(record.expected_head_commit_seq.to_be_bytes());
    hasher.update(record.effect_history_root);
    hasher.update(record.retry_fence_epoch.to_be_bytes());
    match record.group_binding {
        Some(binding) => {
            hasher.update([1u8]);
            hasher.update(binding.group_id.as_bytes());
            hasher.update(binding.membership_generation.to_be_bytes());
            hasher.update(binding.membership_root);
            hasher.update(binding.group_policy_digest);
        }
        None => hasher.update([0u8]),
    }
    if extended {
        match record.process_binding {
            Some(binding) => {
                hasher.update([1u8]);
                hasher.update(binding.process_id.as_bytes());
                hasher.update(binding.process_generation.get().to_be_bytes());
                hasher.update(binding.process_fencing_token);
                hasher.update(binding.agent_instance_id.as_bytes());
                hasher.update(binding.agent_instance_generation.get().to_be_bytes());
                hasher.update(binding.isolation_domain_id.as_bytes());
                hasher.update(binding.isolation_domain_generation.get().to_be_bytes());
                hasher.update(binding.isolation_domain_fencing_token);
                hasher.update(binding.participant_id.as_bytes());
                hasher.update(binding.participant_generation.get().to_be_bytes());
                hasher.update(binding.admission_receipt_id.as_bytes());
            }
            None => hasher.update([0u8]),
        }
        hasher.update(record.semantic_read_set_root);
        if has_semantic_appends {
            hasher.update(record.semantic_append_set_root);
        }
        hasher.update(record.resource_reservation_set_root);
    }
    if has_effects {
        hasher.update(record.effect_set_root);
    }
    if has_effect_endpoints {
        hasher.update(record.effect_endpoint_set_root);
    }
    hasher.update(record.participant_registry_binding.generation.to_be_bytes());
    hasher.update(record.participant_registry_binding.root);
    hasher.update(record.artifact_read_set_root);
    if has_artifact_writes {
        hasher.update(record.artifact_write_set_root);
    }
    hasher.finalize().into()
}

/// One planned effect slot declared inside a `TaskWriteSet`
/// (`planned_actions` subset, §25.1).
///
/// The vector index inside [`PermitRequest::planned_effects`] IS the
/// `effect_seq`: the authority assigns dense sequence numbers `0..n` by
/// position, so a gap or duplicate sequence number is unrepresentable by
/// construction (`[TASK-EFFECT-002]`). `stable_action_slot` lives inside the
/// [`LogicalEffectDescriptor`](crate::LogicalEffectDescriptor); attempt-bound
/// identities (`ActionId`/`OperationId`/driver/reservation) are deliberately
/// absent from the identity-bearing part of the declaration
/// (`[TASK-EFFECT-ID-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlannedEffect {
    /// Cross-attempt-stable logical effect identity input. The descriptor
    /// carries no attempt/action/operation/incarnation/nonce fields by
    /// construction (`[TASK-EFFECT-ID-001]`).
    pub descriptor: crate::effect::LogicalEffectDescriptor,
    /// Whether the slot is a required obligation for `outcome=COMMITTED`.
    /// Required-slot success-criteria semantics remain a placeholder in this
    /// slice (see evidence doc); the flag is recorded durably.
    pub required: bool,
    /// Pre-bound condition digest for conditional required slots. The
    /// snapshot-bound authoritative false-proof is a later slice.
    pub required_condition_digest: Option<[u8; 32]>,
    /// Caller-supplied success-criteria digest placeholder.
    pub success_criteria_digest: [u8; 32],
    /// Caller-supplied digest placeholder binding the staged action proposal
    /// the slot is allowed to dispatch.
    pub action_proposal_digest: [u8; 32],
}

/// A `CommitPermit` issuance request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PermitRequest {
    pub task_id: TaskId,
    pub attempt_id: TaskAttemptId,
    pub attempt_generation: Generation,
    /// Caller-supplied digest placeholder for the staged `TaskWriteSet`.
    pub write_set_root: [u8; 32],
    /// Complete planned effect slot set committed at permit issuance
    /// (`effect_set_root`, `[TASK-EFFECT-002]`). An empty vector declares a
    /// no-effect write set and keeps the pre-effect-slice finalize behavior.
    pub planned_effects: Vec<PlannedEffect>,
    pub idempotency_key: IdempotencyKey,
    /// Caller-supplied expiry. Expiry never clears an issued permit
    /// (`[TASK-COMMIT-003]`); it is stored for the effect slice.
    pub valid_until_ms: i64,
    pub requested_at_ms: i64,
}

/// A permit-holder request to close an issued permit with a
/// `TaskPermitClosureReceipt`-shaped record while keeping the `TaskHead`
/// unchanged (`[TASK-COMMIT-002]` final clause, `[TASK-CANCEL-003]`).
///
/// Every planned slot must hold an authoritative absence proof:
/// `NoEffect` (token verifiably unconsumed) or `ConfirmedNoEffect`
/// (external authority confirmed no effect). Any slot in `EffectClosed` or
/// `EffectUnknown` forbids this path (`[TASK-RETRY-EFFECT-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClosePermitRequest {
    pub task_id: TaskId,
    pub attempt_id: TaskAttemptId,
    pub attempt_generation: Generation,
    pub permit_id: CommitPermitId,
    pub outcome: PermitClosureOutcome,
    /// Caller-supplied participant-fence proof placeholder, persisted on
    /// the quarantine receipt if any slot is `EffectUnknown`
    /// (`[TASK-EFFECT-003]`).
    pub fenced_participant_digest: [u8; 32],
    pub closed_at_ms: i64,
}

/// Outcome of a pre-effect permit closure (`TaskPermitClosureReceipt`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PermitClosureOutcome {
    FailedBeforeEffect,
    CancelledBeforeEffect,
}

impl PermitClosureOutcome {
    pub(crate) const fn receipt_outcome(self) -> ReceiptOutcome {
        match self {
            Self::FailedBeforeEffect => ReceiptOutcome::FailedBeforeEffect,
            Self::CancelledBeforeEffect => ReceiptOutcome::CancelledBeforeEffect,
        }
    }
}

/// A Task cancellation request (`[TASK-CANCEL-002]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CancelRequest {
    pub task_id: TaskId,
    pub idempotency_key: IdempotencyKey,
    pub requested_at_ms: i64,
}

/// A permit-holder finalize request (B-TASK-001/002 shape).
///
/// For permits with a declared effect set the full `[TASK-COMMIT-002]`
/// semantics live in
/// [`FinalizeRequestV3`](crate::FinalizeRequestV3), and the authority
/// computes the post-commit roots itself; for permits with no declared
/// effect set (all B-TASK-001 flows) the caller-supplied roots below stay
/// authoritative and the fence must never regress.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FinalizeRequest {
    pub task_id: TaskId,
    pub attempt_id: TaskAttemptId,
    pub attempt_generation: Generation,
    pub permit_id: CommitPermitId,
    pub new_effect_history_root: [u8; 32],
    pub new_retry_fence_epoch: u64,
    pub finalized_at_ms: i64,
}

/// Caller-asserted success proof for one required slot
/// (`[TASK-COMMIT-002]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RequiredSatisfaction {
    pub effect_seq: u64,
    pub proof: RequiredSatisfactionProof,
}

/// How a required slot's obligation is asserted as met.
///
/// The authority never verifies proof *content* — digests are
/// caller-supplied placeholders — but it enforces the structural rule:
/// `EffectClosedSuccess` only pairs with an `EffectClosed` slot, and
/// `ConditionNotApplicable` only pairs with a `NoEffect` slot whose
/// `TaskNoEffectReceipt` reason is `ConditionNotApplicable` and whose
/// pre-bound `required_condition_digest` matches. All other `NoEffect`
/// reasons and `ConfirmedNoEffect` can never satisfy a required slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RequiredSatisfactionProof {
    /// The slot closed with an effect and the caller asserts its
    /// `success_criteria_digest` is met (placeholder digest binding).
    EffectClosedSuccess { success_assertion_digest: [u8; 32] },
    /// The slot's pre-bound condition is authoritatively false: the proof
    /// digest binds the original snapshot identity plus the pre-bound
    /// `required_condition_digest` (placeholder binding, see evidence).
    ConditionNotApplicable {
        condition_false_proof_digest: [u8; 32],
    },
}

/// Lifecycle of a registered Task in this slice.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskState {
    Active,
    Cancelled,
}

/// Pre-permit subset of the §25.1 `TaskAttempt` state machine.
///
/// Reachable in this slice: `Created` → `ReadyToCommit` →
/// `CommitPermitted` (CAS win) | `Superseded` (CAS loss) | `Conflicted`
/// (validation failure), `CommitPermitted` → `Committed` (finalize), and any
/// open pre-permit state → `Cancelled`. The remaining variants are reserved
/// for the scheduling and effect slices and cannot be produced here;
/// post-permit `EFFECTING`/`FINALIZING`/`UNCERTAIN`/`RECONCILING` are
/// represented as permit states rather than attempt states in this slice.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttemptState {
    Created,
    /// Reserved for the scheduling slice; not producible here.
    Admitted,
    /// Reserved for the scheduling slice; not producible here.
    Running,
    /// Reserved for the scheduling slice; not producible here.
    Waiting,
    /// Reserved for the scheduling slice; not producible here.
    Sealing,
    /// Reserved for the scheduling slice; not producible here.
    Sealed,
    /// Reserved for the scheduling slice; not producible here.
    Validating,
    /// Permit CAS validation failed (stale snapshot/head binding).
    Conflicted,
    /// Candidate sealed and competing for the `CommitPermit`.
    ReadyToCommit,
    /// Lost the permit CAS to another attempt; terminally fenced.
    Superseded,
    /// Holds the issued `CommitPermit`.
    CommitPermitted,
    /// Reserved for the cancellation-drain slice; not producible here.
    Cancelling,
    /// Closed before any effect with a closure receipt; `TaskHead` unchanged.
    Cancelled,
    /// Reserved for the failure-reporting slice; not producible here.
    Failed,
    /// Permit holder finalized; `TaskHead` advanced with a commit receipt.
    Committed,
}

impl AttemptState {
    /// Whether the attempt is still an open pre-permit candidate that may
    /// request a `CommitPermit`.
    #[must_use]
    pub const fn is_open_candidate(self) -> bool {
        matches!(self, Self::Created | Self::ReadyToCommit)
    }

    /// Whether the attempt has reached a state this slice never leaves.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Conflicted | Self::Superseded | Self::Cancelled | Self::Committed
        )
    }

    pub(crate) const fn code(self) -> i64 {
        match self {
            Self::Created => 0,
            Self::Admitted => 1,
            Self::Running => 2,
            Self::Waiting => 3,
            Self::Sealing => 4,
            Self::Sealed => 5,
            Self::Validating => 6,
            Self::Conflicted => 7,
            Self::ReadyToCommit => 8,
            Self::Superseded => 9,
            Self::CommitPermitted => 10,
            Self::Cancelling => 11,
            Self::Cancelled => 12,
            Self::Failed => 13,
            Self::Committed => 14,
        }
    }

    pub(crate) fn from_code(code: i64) -> Result<Self, TaskStoreError> {
        match code {
            0 => Ok(Self::Created),
            1 => Ok(Self::Admitted),
            2 => Ok(Self::Running),
            3 => Ok(Self::Waiting),
            4 => Ok(Self::Sealing),
            5 => Ok(Self::Sealed),
            6 => Ok(Self::Validating),
            7 => Ok(Self::Conflicted),
            8 => Ok(Self::ReadyToCommit),
            9 => Ok(Self::Superseded),
            10 => Ok(Self::CommitPermitted),
            11 => Ok(Self::Cancelling),
            12 => Ok(Self::Cancelled),
            13 => Ok(Self::Failed),
            14 => Ok(Self::Committed),
            _ => Err(TaskStoreError::CorruptRecord("unknown attempt state")),
        }
    }
}

/// Lifecycle of a `CommitPermit`.
///
/// `Superseded` remains reserved: CAS losers never receive a permit row
/// (the losing *attempt* enters `AttemptState::Superseded`). `Quarantined`
/// is the non-reusable tombstone produced when any slot is `EffectUnknown`
/// at closure time (`[TASK-COMMIT-003]` / `[TASK-EFFECT-003]`): the
/// `TaskHead` stays frozen and no new winner may be issued until every
/// unknown slot is reconciled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PermitState {
    Issued,
    Closed,
    /// Reserved tombstone; not producible in this slice.
    Superseded,
    /// Reserved tombstone; not producible in this slice.
    Quarantined,
}

impl PermitState {
    pub(crate) const fn code(self) -> i64 {
        match self {
            Self::Issued => 0,
            Self::Closed => 1,
            Self::Superseded => 2,
            Self::Quarantined => 3,
        }
    }

    pub(crate) fn from_code(code: i64) -> Result<Self, TaskStoreError> {
        match code {
            0 => Ok(Self::Issued),
            1 => Ok(Self::Closed),
            2 => Ok(Self::Superseded),
            3 => Ok(Self::Quarantined),
            _ => Err(TaskStoreError::CorruptRecord("unknown permit state")),
        }
    }
}

/// Outcome recorded on a durable task receipt.
///
/// `PartialEffect` and `FailedAfterEffect` are producible since schema v3
/// (`[TASK-RETRY-EFFECT-001]`): when a required slot is unsatisfied but at
/// least one effect already happened, finalize writes a commit receipt
/// with one of these outcomes and advances the head, history, and fence.
/// The rule choosing between them: `FailedAfterEffect` when the caller did
/// not present a proof for any required slot (the attempt's goal failed);
/// `PartialEffect` when at least one required slot is satisfied (the
/// commit is partially usable). `Partial` remains reserved.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReceiptOutcome {
    Committed,
    FailedBeforeEffect,
    CancelledBeforeEffect,
    /// Reserved for the administrative task-outcome layer; not producible.
    Partial,
    PartialEffect,
    FailedAfterEffect,
}

impl ReceiptOutcome {
    pub(crate) const fn is_producible(self) -> bool {
        !matches!(self, Self::Partial)
    }

    pub(crate) const fn code(self) -> i64 {
        match self {
            Self::Committed => 0,
            Self::FailedBeforeEffect => 1,
            Self::CancelledBeforeEffect => 2,
            Self::Partial => 3,
            Self::PartialEffect => 4,
            Self::FailedAfterEffect => 5,
        }
    }

    pub(crate) fn from_code(code: i64) -> Result<Self, TaskStoreError> {
        match code {
            0 => Ok(Self::Committed),
            1 => Ok(Self::FailedBeforeEffect),
            2 => Ok(Self::CancelledBeforeEffect),
            3 => Ok(Self::Partial),
            4 => Ok(Self::PartialEffect),
            5 => Ok(Self::FailedAfterEffect),
            _ => Err(TaskStoreError::CorruptRecord("unknown receipt outcome")),
        }
    }
}

impl TaskState {
    pub(crate) const fn code(self) -> i64 {
        match self {
            Self::Active => 0,
            Self::Cancelled => 1,
        }
    }

    pub(crate) fn from_code(code: i64) -> Result<Self, TaskStoreError> {
        match code {
            0 => Ok(Self::Active),
            1 => Ok(Self::Cancelled),
            _ => Err(TaskStoreError::CorruptRecord("unknown task state")),
        }
    }
}

/// Identity of a registered attempt returned by registration decisions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AttemptHandle {
    pub attempt_id: TaskAttemptId,
    pub attempt_generation: Generation,
    pub snapshot_id: TaskSnapshotId,
}

/// Durable head/control view of a Task (`TaskHead` + `TaskControlRecord`
/// subset for a single authority).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskRecord {
    pub task_id: TaskId,
    pub task_generation: Generation,
    pub head_commit_seq: u64,
    pub head_effect_history_root: [u8; 32],
    pub retry_fence_epoch: u64,
    pub control_epoch: u64,
    pub cancel_epoch: u64,
    pub permit_epoch: u64,
    pub state: TaskState,
    /// The currently outstanding permit, if any. A `Closed` permit is not
    /// reported here; the CAS gate recomputes eligibility from permit rows.
    pub active_permit: Option<CommitPermitId>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// Durable view of one `TaskAttempt`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttemptRecord {
    pub task_id: TaskId,
    pub attempt_id: TaskAttemptId,
    pub attempt_generation: Generation,
    pub snapshot: SnapshotBundle,
    /// Present only when registration used the schema-v10 receipted path.
    pub snapshot_receipt_id: Option<ReceiptId>,
    pub cancellation_scope_id: CancellationScopeId,
    pub cancellation_generation: Generation,
    pub state: AttemptState,
    pub receipt_id: Option<ReceiptId>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// Durable view of one `CommitPermit`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PermitRecord {
    pub permit_id: CommitPermitId,
    pub task_id: TaskId,
    pub idempotency_key: IdempotencyKey,
    pub attempt_id: TaskAttemptId,
    pub attempt_generation: Generation,
    pub expected_head_commit_seq: u64,
    pub expected_effect_history_root: [u8; 32],
    pub expected_retry_fence_epoch: u64,
    pub write_set_root: [u8; 32],
    /// Exact `TaskGroup` membership position bound with the write set at
    /// permit issuance. `None` preserves ungrouped B-TASK-001 behavior.
    pub group_binding: Option<crate::TaskGroupCommitBinding>,
    /// Participant registry atomically frozen by this permit issuance.
    /// `None` is reserved for pre-v11 migrated permits.
    pub participant_registry_binding: Option<crate::ParticipantRegistryBinding>,
    pub permit_epoch: u64,
    pub control_epoch: u64,
    pub cancel_epoch: u64,
    pub valid_until_ms: i64,
    pub state: PermitState,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// Durable view of one task receipt (commit or closure).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskReceiptRecord {
    pub receipt_id: ReceiptId,
    pub task_id: TaskId,
    /// `None` for pre-permit closure receipts, which have no permit binding.
    pub permit_id: Option<CommitPermitId>,
    pub attempt_id: TaskAttemptId,
    pub attempt_generation: Generation,
    /// Verbatim copy of the permit's `TaskGroup` membership binding. The
    /// authority revalidates it immediately before terminalization.
    pub group_binding: Option<crate::TaskGroupCommitBinding>,
    /// Verbatim copy of the permit's participant registry binding. `None`
    /// is reserved for pre-permit closure or pre-v12 migrated receipts.
    pub participant_registry_binding: Option<crate::ParticipantRegistryBinding>,
    pub outcome: ReceiptOutcome,
    pub prior_head_commit_seq: u64,
    pub prior_effect_history_root: [u8; 32],
    pub prior_retry_fence_epoch: u64,
    pub new_head_commit_seq: u64,
    pub new_effect_history_root: [u8; 32],
    pub new_retry_fence_epoch: u64,
    pub created_at_ms: i64,
}

/// Decision of an idempotent Task registration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskRegistrationDecision {
    Created(TaskId),
    Existing(TaskId),
}

impl TaskRegistrationDecision {
    #[must_use]
    pub const fn task_id(self) -> TaskId {
        match self {
            Self::Created(task_id) | Self::Existing(task_id) => task_id,
        }
    }
}

/// Decision of an idempotent `TaskAttempt` registration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttemptRegistrationDecision {
    Created(AttemptHandle),
    Existing(AttemptHandle),
}

impl AttemptRegistrationDecision {
    #[must_use]
    pub const fn handle(self) -> AttemptHandle {
        match self {
            Self::Created(handle) | Self::Existing(handle) => handle,
        }
    }
}

/// Machine-readable reason a permit request was rejected as `Conflicted`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PermitConflict {
    /// The attempt snapshot's expected head commit sequence no longer
    /// matches the current `TaskHead`.
    StaleTaskHead { expected: u64, current: u64 },
    /// The attempt snapshot's effect-history root no longer matches the
    /// current `TaskHead`.
    StaleEffectHistoryRoot,
    /// The attempt snapshot's retry-fence epoch no longer matches the
    /// current `TaskHead`.
    StaleRetryFenceEpoch,
    /// The same attempt already holds an issued permit under a different
    /// idempotency key.
    AttemptAlreadyHoldsPermit { permit_id: CommitPermitId },
}

/// Linearized decision of a `CommitPermit` request (`[TASK-COMMIT-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PermitDecision {
    /// This attempt won the CAS and holds the new permit.
    Issued(Box<PermitRecord>),
    /// Same idempotency key and same request bytes: the original permit.
    Replayed(Box<PermitRecord>),
    /// Another attempt already holds the outstanding permit; the requesting
    /// attempt is durably fenced as `Superseded`.
    Superseded { winner: Box<PermitRecord> },
    /// Validation failed; the requesting attempt is durably `Conflicted`.
    Conflicted { reason: PermitConflict },
    /// Cancellation committed first (`[TASK-CANCEL-003]`): no permit was
    /// issued, the attempt closed pre-permit with a closure receipt, and the
    /// `TaskHead` is unchanged.
    CancelledBeforeEffect { receipt_id: ReceiptId },
    /// A quarantine tombstone blocks new winner issuance
    /// (`[TASK-COMMIT-003]` / `[TASK-EFFECT-003]`): the requesting attempt
    /// is durably `Superseded`.
    Quarantined { quarantine_receipt_id: ReceiptId },
}

/// One attempt closed by a committed cancellation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ClosedAttempt {
    pub attempt_id: TaskAttemptId,
    pub attempt_generation: Generation,
    pub receipt_id: ReceiptId,
}

/// Linearized decision of a Task cancellation (`[TASK-CANCEL-002]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CancelDecision {
    /// The first committed cancellation: `cancel_epoch` advanced exactly
    /// once and every open pre-permit attempt closed with a closure receipt.
    Applied {
        cancel_epoch: u64,
        closed_attempts: Vec<ClosedAttempt>,
    },
    /// Exact replay of the original cancellation key; nothing re-applied.
    Replayed { cancel_epoch: u64 },
    /// A different key arrived after cancellation was already committed;
    /// the epoch is not incremented again.
    AlreadyCancelled { cancel_epoch: u64 },
}

/// Decision of a permit-holder finalize (`[TASK-COMMIT-001]` CAS commit).
///
/// The enum keeps its B-TASK-001 two-variant shape; the quarantine
/// tombstone (`[TASK-EFFECT-003]`) is reported as the typed
/// [`TaskStoreError::Quarantined`](crate::TaskStoreError::Quarantined)
/// refusal — the tombstone commits, the `TaskHead` does not advance, and
/// replaying the same finalize observes the same refusal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FinalizeDecision {
    /// `TaskHead` advanced and the permit closed in one transaction. The
    /// receipt outcome is `Committed`, `PartialEffect`, or
    /// `FailedAfterEffect` (`[TASK-RETRY-EFFECT-001]`).
    Committed(Box<TaskReceiptRecord>),
    /// Exact replay of an already-finalized permit: the original receipt.
    Replayed(Box<TaskReceiptRecord>),
}

/// A `TaskEffectQuarantineReceipt`-shaped durable record
/// (`[TASK-EFFECT-003]`, single-authority placeholder subset).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuarantineReceiptRecord {
    pub receipt_id: ReceiptId,
    pub task_id: TaskId,
    pub task_generation: Generation,
    pub permit_id: CommitPermitId,
    pub permit_epoch: u64,
    pub effect_set_root: [u8; 32],
    pub outstanding_effect_quarantine_root: [u8; 32],
    /// Durable identity digest of the `EffectUnknown` slot with the
    /// smallest `effect_seq` (conflicting-target placeholder).
    pub conflicting_target_digest: [u8; 32],
    pub known_effect_receipts: Vec<ReceiptId>,
    pub unknown_slots: Vec<u64>,
    /// Participant-fence proof placeholder (caller-supplied digest).
    pub fenced_participant_digest: [u8; 32],
    pub created_at_ms: i64,
}

/// A `PermitAdoptionReceipt`-shaped durable record (`[TASK-COMMIT-003]`,
/// single-authority subset). Its scope is fixed to
/// `RECONCILE_CLOSE_OR_QUARANTINE_ONLY`: it never authorizes new
/// `EffectPermit`s, dispatches, effects, or proposal changes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdoptionReceiptRecord {
    pub receipt_id: ReceiptId,
    pub task_id: TaskId,
    pub task_generation: Generation,
    pub original_permit_id: CommitPermitId,
    pub original_permit_epoch: u64,
    pub original_control_epoch: u64,
    pub original_cancel_epoch: u64,
    pub effect_set_root: [u8; 32],
    pub observed_effect_slot_state_root: [u8; 32],
    pub adoption_epoch: u64,
    pub created_at_ms: i64,
}

/// Outcome of one reconcile step on an `EffectUnknown` slot
/// (`[TASK-EFFECT-003]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReconcileOutcome {
    /// The external authority proves the effect happened.
    EffectClosed,
    /// The external authority proves no effect happened. This is NOT a
    /// `TaskNoEffectReceipt` (token-unconsumed proof); it never satisfies
    /// a required slot but it is a valid absence proof for
    /// `FAILED_BEFORE_EFFECT`/`CANCELLED_BEFORE_EFFECT` closure.
    ConfirmedNoEffect,
    /// Still unknown: the slot returns to `EffectUnknown` and the permit
    /// stays `Quarantined`.
    EffectUnknown,
}

/// A `TaskEffectReconciliationReceipt`-shaped durable record
/// (`[TASK-EFFECT-003]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReconciliationReceiptRecord {
    pub receipt_id: ReceiptId,
    pub task_id: TaskId,
    pub permit_id: CommitPermitId,
    pub permit_epoch: u64,
    pub permit_adoption_receipt_id: ReceiptId,
    pub effect_slot_id: crate::EffectSlotId,
    pub effect_seq: u64,
    pub logical_effect_id: [u8; 32],
    pub retry_fence_epoch: u64,
    pub effect_set_root: [u8; 32],
    pub outcome: ReconcileOutcome,
    /// Caller-supplied digest placeholder for the gateway/provider
    /// authoritative closure proof.
    pub closure_proof_digest: [u8; 32],
    /// Present when `outcome == EffectClosed`: the effect receipt that
    /// closes the slot (history entry `authoritative_effect_receipt_id`).
    pub effect_receipt_id: Option<ReceiptId>,
    pub effect_slot_state_root_after: [u8; 32],
    pub created_at_ms: i64,
}

/// Outcome recorded on a `TaskEffectHistoryEntry` (`[TASK-EFFECT-ID-001]`
/// / `[TASK-RETRY-EFFECT-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EffectHistoryOutcome {
    EffectClosed,
    ConfirmedNoEffect,
    PartialEffect,
    /// Recorded but never executed in this slice (no compensation
    /// execution; see evidence limitations).
    Compensated,
}

impl EffectHistoryOutcome {
    pub(crate) const fn code(self) -> i64 {
        match self {
            Self::EffectClosed => 0,
            Self::ConfirmedNoEffect => 1,
            Self::PartialEffect => 2,
            Self::Compensated => 3,
        }
    }

    pub(crate) fn from_code(code: i64) -> Result<Self, TaskStoreError> {
        match code {
            0 => Ok(Self::EffectClosed),
            1 => Ok(Self::ConfirmedNoEffect),
            2 => Ok(Self::PartialEffect),
            3 => Ok(Self::Compensated),
            _ => Err(TaskStoreError::CorruptRecord(
                "unknown effect history outcome",
            )),
        }
    }
}

/// One durable `TaskEffectHistoryEntry` (`[TASK-EFFECT-ID-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectHistoryEntry {
    /// Strictly increasing from 1, no gaps per task.
    pub effect_history_seq: u64,
    pub task_id: TaskId,
    pub logical_effect_id: [u8; 32],
    pub retry_fence_epoch: u64,
    pub action_proposal_digest: [u8; 32],
    pub idempotency_identity_digest: [u8; 32],
    /// Placeholder: always `None` in this slice (no Operation binding).
    pub operation_id: Option<[u8; 16]>,
    pub outcome: EffectHistoryOutcome,
    pub authoritative_effect_receipt_id: ReceiptId,
    pub compensation_receipt_id: Option<ReceiptId>,
    pub created_at_ms: i64,
}

/// Read-back view of a cross-attempt effect-history lookup
/// (`[TASK-RETRY-EFFECT-001]`): the entry plus the original authoritative
/// effect receipt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectHistoryLookup {
    pub entry: EffectHistoryEntry,
    pub original_receipt: crate::EffectReceipt,
}

/// The original receipt returned by a `close_permit` replay/issuance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClosePermitDecision {
    Closed(Box<TaskReceiptRecord>),
    /// At least one slot is `EffectUnknown`: the permit became a
    /// non-reusable `Quarantined` tombstone and the `TaskHead` did NOT
    /// advance (`[TASK-EFFECT-003]`).
    Quarantined(Box<QuarantineReceiptRecord>),
    Replayed(Box<TaskReceiptRecord>),
    ReplayedQuarantine(Box<QuarantineReceiptRecord>),
}

fn sha256_prefix16(domain: &str, parts: &[&[u8]]) -> [u8; 16] {
    let mut hasher = Sha256::new();
    hasher.update(domain.as_bytes());
    for part in parts {
        hasher.update(part);
    }
    let full: [u8; 32] = hasher.finalize().into();
    let mut prefix = [0u8; 16];
    prefix.copy_from_slice(&full[..16]);
    prefix
}

/// Deterministic authority-issued permit identity. Deriving the ID from the
/// task and idempotency key makes replay after a restart return the same
/// permit and makes a never-issued "ghost" permit unrepresentable.
pub(crate) fn derive_permit_id(task_id: TaskId, idempotency_key: IdempotencyKey) -> CommitPermitId {
    CommitPermitId::from_bytes(sha256_prefix16(
        "llmos/task-commit-permit/v1",
        &[task_id.as_bytes(), idempotency_key.as_bytes()],
    ))
}

/// Deterministic commit receipt identity bound to its permit.
pub(crate) fn derive_commit_receipt_id(permit_id: CommitPermitId) -> ReceiptId {
    ReceiptId::from_bytes(sha256_prefix16(
        "llmos/task-commit-receipt/v1",
        &[permit_id.as_bytes()],
    ))
}

/// Deterministic pre-permit closure receipt identity bound to the attempt
/// and the cancel epoch that closed it (`[TASK-CANCEL-003]`).
pub(crate) fn derive_closure_receipt_id(
    task_id: TaskId,
    attempt_id: TaskAttemptId,
    cancel_epoch: u64,
) -> ReceiptId {
    ReceiptId::from_bytes(sha256_prefix16(
        "llmos/task-closure-receipt/v1",
        &[
            task_id.as_bytes(),
            attempt_id.as_bytes(),
            &cancel_epoch.to_be_bytes(),
        ],
    ))
}

/// Deterministic `TaskPermitClosureReceipt` identity bound to its permit.
pub(crate) fn derive_permit_closure_receipt_id(permit_id: CommitPermitId) -> ReceiptId {
    ReceiptId::from_bytes(sha256_prefix16(
        "llmos/task-permit-closure-receipt/v1",
        &[permit_id.as_bytes()],
    ))
}

/// Deterministic quarantine tombstone identity bound to its permit.
pub(crate) fn derive_quarantine_receipt_id(permit_id: CommitPermitId) -> ReceiptId {
    ReceiptId::from_bytes(sha256_prefix16(
        "llmos/task-effect-quarantine/v1",
        &[permit_id.as_bytes()],
    ))
}

/// Deterministic adoption identity bound to the original permit and the
/// adoption sequence (`adoption_epoch`), so repeated adoptions of the same
/// quarantined permit get distinct durable records.
pub(crate) fn derive_adoption_receipt_id(
    permit_id: CommitPermitId,
    adoption_epoch: u64,
) -> ReceiptId {
    ReceiptId::from_bytes(sha256_prefix16(
        "llmos/task-permit-adoption/v1",
        &[permit_id.as_bytes(), &adoption_epoch.to_be_bytes()],
    ))
}

/// Deterministic reconcile identity bound to the slot and the slot-state
/// sequence of the transition it records (a slot that returns to
/// `EffectUnknown` can be reconciled again with a fresh identity).
pub(crate) fn derive_reconcile_receipt_id(
    effect_slot_id: crate::EffectSlotId,
    state_seq: u64,
) -> ReceiptId {
    ReceiptId::from_bytes(sha256_prefix16(
        "llmos/task-effect-reconciliation/v1",
        &[effect_slot_id.as_bytes(), &state_seq.to_be_bytes()],
    ))
}
