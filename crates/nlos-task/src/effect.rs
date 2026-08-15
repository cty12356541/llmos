//! `EffectPermit` issuance and the per-slot `EffectSlot` state machine
//! (`[TASK-EFFECT-001]` / `[TASK-EFFECT-002]`, B-TASK second slice).
//!
//! Identity and digest formulas are domain-separated SHA-256 placeholders
//! fixing the deterministic shape required by `[TASK-EFFECT-ID-001]`; the
//! canonical deterministic-CBOR encoding and signatures remain out of scope.
//!
//! The `LogicalEffectDescriptor` struct has no attempt/action/operation/
//! incarnation/nonce fields *by construction*: those identities can never
//! enter `LogicalEffectId`, so a retry, hedge, or agent swap cannot mint a
//! new logical effect (`[TASK-EFFECT-ID-001]`).

use nlos_types::{
    CommitPermitId, EffectPermitId, EffectSlotId, Generation, IdempotencyKey, ReceiptId,
    TaskAttemptId, TaskId,
};
use rusqlite::{Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use crate::model::PlannedEffect;
use crate::store::{
    SqlRead, SqliteTaskAuthority, StoredTask, blob16, blob32, decode_participant_binding,
    encode_u64, generation_from_blob, load_attempt, load_permit_by_id, load_task, optional_blob16,
    u64_from_blob, update_task,
};
use crate::{PermitRecord, PermitState, TaskStoreError};

/// Domain-separated placeholder digest of the empty planned effect set.
#[must_use]
pub fn empty_effect_set_root() -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/task-effect-set/v1");
    hasher.finalize().into()
}

/// Cross-attempt-stable identity input of one logical effect (§25.1).
///
/// `[TASK-EFFECT-ID-001]` forbids `AttemptId`, `ActionId`, `OperationId`,
/// process/agent incarnation, and random nonces in this descriptor; the
/// struct simply does not have such fields, so the prohibition is enforced
/// by construction rather than by validation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LogicalEffectDescriptor {
    pub task_id: TaskId,
    pub task_generation: Generation,
    /// Caller-supplied placeholder for the immutable `IntentSpec` identity.
    pub intent_spec_id: [u8; 32],
    /// Stable slot inside the intent/task contract; a genuinely new business
    /// effect must come from an explicitly different slot.
    pub stable_action_slot: u64,
    /// Authoritative stable identity of the target object (digest
    /// placeholder).
    pub target_authority_object_id: [u8; 32],
    /// Caller-supplied effect-class code placeholder.
    pub effect_class: u32,
    /// Caller-supplied idempotency-scope code placeholder.
    pub idempotency_scope: u32,
}

impl LogicalEffectDescriptor {
    /// Fixed-width canonical placeholder encoding standing in for
    /// deterministic CBOR: fields in declaration order, integers
    /// big-endian, no length prefixes (every field is fixed-width).
    fn canonical_bytes(&self) -> [u8; 104] {
        let mut bytes = [0u8; 104];
        bytes[0..16].copy_from_slice(self.task_id.as_bytes());
        bytes[16..24].copy_from_slice(&self.task_generation.get().to_be_bytes());
        bytes[24..56].copy_from_slice(&self.intent_spec_id);
        bytes[56..64].copy_from_slice(&self.stable_action_slot.to_be_bytes());
        bytes[64..96].copy_from_slice(&self.target_authority_object_id);
        bytes[96..100].copy_from_slice(&self.effect_class.to_be_bytes());
        bytes[100..104].copy_from_slice(&self.idempotency_scope.to_be_bytes());
        bytes
    }

    /// `SHA-256("llmos/task-logical-effect/v1" || canonical(descriptor))`.
    #[must_use]
    pub fn logical_effect_id(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(b"llmos/task-logical-effect/v1");
        hasher.update(self.canonical_bytes());
        hasher.finalize().into()
    }

    /// `SHA-256("llmos/task-effect-idempotency-identity/v1" || LogicalEffectId)`.
    #[must_use]
    pub fn idempotency_identity_digest(&self) -> [u8; 32] {
        idempotency_identity_digest(&self.logical_effect_id())
    }
}

/// `SHA-256("llmos/task-effect-idempotency-identity/v1" || LogicalEffectId)`.
#[must_use]
pub fn idempotency_identity_digest(logical_effect_id: &[u8; 32]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/task-effect-idempotency-identity/v1");
    hasher.update(logical_effect_id);
    hasher.finalize().into()
}

/// Lifecycle of one planned effect slot (`[TASK-EFFECT-002]`).
///
/// Producible: `Planned` → `Permitted` → `Dispatched` →
/// `EffectClosed` | `EffectUnknown`, `Planned` | `Permitted` → `NoEffect`
/// when the dispatch token is verifiably unconsumed, and since schema v3
/// `EffectUnknown` → `Reconciling` → `EffectClosed` |
/// `ConfirmedNoEffect` | `EffectUnknown` via the explicit reconcile API
/// (`[TASK-EFFECT-003]`). `EffectUnknown` never silently resolves.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SlotState {
    Planned,
    Permitted,
    Dispatched,
    /// Closed with a no-effect receipt; the token was never consumed.
    NoEffect,
    /// Closed with an authoritative effect receipt.
    EffectClosed,
    /// Crash-window uncertainty durably registered; blocks permit closure
    /// until the explicit reconcile API resolves it.
    EffectUnknown,
    /// Mid-reconcile marker inside one authority transaction
    /// (`[TASK-EFFECT-003]`); durable between the adoption CAS and the
    /// reconciliation-receipt write.
    Reconciling,
    /// The external effect authority proved no effect happened. Distinct
    /// from `NoEffect`: never satisfies a required slot, but is a valid
    /// absence proof for pre-effect permit closure.
    ConfirmedNoEffect,
}

impl SlotState {
    /// Whether the slot still blocks permit closure under
    /// `[TASK-COMMIT-002]`: anything that is not a known terminal
    /// (`NoEffect` / `EffectClosed` / `ConfirmedNoEffect`) blocks,
    /// including `Planned`, `Reconciling`, and `EffectUnknown`.
    #[must_use]
    pub const fn blocks_finalization(self) -> bool {
        !matches!(
            self,
            Self::NoEffect | Self::EffectClosed | Self::ConfirmedNoEffect
        )
    }

    pub(crate) const fn code(self) -> i64 {
        match self {
            Self::Planned => 0,
            Self::Permitted => 1,
            Self::Dispatched => 2,
            Self::NoEffect => 3,
            Self::EffectClosed => 4,
            Self::EffectUnknown => 5,
            Self::Reconciling => 6,
            Self::ConfirmedNoEffect => 7,
        }
    }

    pub(crate) fn from_code(code: i64) -> Result<Self, TaskStoreError> {
        match code {
            0 => Ok(Self::Planned),
            1 => Ok(Self::Permitted),
            2 => Ok(Self::Dispatched),
            3 => Ok(Self::NoEffect),
            4 => Ok(Self::EffectClosed),
            5 => Ok(Self::EffectUnknown),
            6 => Ok(Self::Reconciling),
            7 => Ok(Self::ConfirmedNoEffect),
            _ => Err(TaskStoreError::CorruptRecord("unknown effect slot state")),
        }
    }
}

/// Reason recorded on a `TaskNoEffectReceipt`-shaped closure
/// (`[TASK-EFFECT-002]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NoEffectReason {
    NotSelected,
    CancelledBeforeDispatch,
    ExpiredBeforeDispatch,
    PolicySkipped,
    /// Only accepted for slots with a pre-bound `required_condition_digest`;
    /// the snapshot-bound authoritative false-proof is a later slice.
    ConditionNotApplicable,
}

impl NoEffectReason {
    pub(crate) const fn code(self) -> i64 {
        match self {
            Self::NotSelected => 0,
            Self::CancelledBeforeDispatch => 1,
            Self::ExpiredBeforeDispatch => 2,
            Self::PolicySkipped => 3,
            Self::ConditionNotApplicable => 4,
        }
    }

    pub(crate) fn from_code(code: i64) -> Result<Self, TaskStoreError> {
        match code {
            0 => Ok(Self::NotSelected),
            1 => Ok(Self::CancelledBeforeDispatch),
            2 => Ok(Self::ExpiredBeforeDispatch),
            3 => Ok(Self::PolicySkipped),
            4 => Ok(Self::ConditionNotApplicable),
            _ => Err(TaskStoreError::CorruptRecord("unknown no-effect reason")),
        }
    }
}

/// Outcome a caller registers for a `Dispatched` slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Outcome {
    /// The driver/gateway produced an authoritative closure; the digest is
    /// the closure proof carried by the effect Receipt.
    Closed {
        authoritative_closure_digest: [u8; 32],
    },
    /// The caller cannot prove whether the effect happened (crash window).
    /// Durable and terminal in this slice; quarantine/reconcile is the next
    /// slice (`[TASK-EFFECT-003]`).
    Unknown { uncertainty_digest: [u8; 32] },
}

impl Outcome {
    fn target_state(self) -> SlotState {
        match self {
            Self::Closed { .. } => SlotState::EffectClosed,
            Self::Unknown { .. } => SlotState::EffectUnknown,
        }
    }

    fn proof_digest(self) -> [u8; 32] {
        match self {
            Self::Closed {
                authoritative_closure_digest,
            }
            | Self::Unknown {
                uncertainty_digest: authoritative_closure_digest,
            } => authoritative_closure_digest,
        }
    }
}

/// Kind of a durable effect receipt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReceiptKind {
    EffectClosed,
    EffectUnknown,
    NoEffect,
    /// External authority confirmed no effect happened
    /// (`[TASK-EFFECT-003]`); written by the reconcile path only.
    ConfirmedNoEffect,
}

impl ReceiptKind {
    pub(crate) const fn code(self) -> i64 {
        match self {
            Self::EffectClosed => 0,
            Self::EffectUnknown => 1,
            Self::NoEffect => 2,
            Self::ConfirmedNoEffect => 3,
        }
    }

    pub(crate) fn from_code(code: i64) -> Result<Self, TaskStoreError> {
        match code {
            0 => Ok(Self::EffectClosed),
            1 => Ok(Self::EffectUnknown),
            2 => Ok(Self::NoEffect),
            3 => Ok(Self::ConfirmedNoEffect),
            _ => Err(TaskStoreError::CorruptRecord("unknown effect receipt kind")),
        }
    }
}

/// Durable view of one effect slot (`EffectSlotRecord` subset, §25.1).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SlotRecord {
    pub permit_id: CommitPermitId,
    pub effect_seq: u64,
    pub task_id: TaskId,
    pub effect_slot_id: EffectSlotId,
    pub logical_effect_id: [u8; 32],
    pub idempotency_identity_digest: [u8; 32],
    pub required: bool,
    pub required_condition_digest: Option<[u8; 32]>,
    pub success_criteria_digest: [u8; 32],
    pub action_proposal_digest: [u8; 32],
    pub state: SlotState,
    pub state_seq: u64,
    pub effect_permit_id: Option<EffectPermitId>,
    pub effect_receipt_id: Option<ReceiptId>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// An `EffectPermit` issuance request. Only the outstanding `CommitPermit`
/// holder may present one (`[TASK-RACE-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PermitRequest {
    pub task_id: TaskId,
    pub attempt_id: TaskAttemptId,
    pub attempt_generation: Generation,
    pub permit_id: CommitPermitId,
    pub permit_epoch: u64,
    /// Which declared slot of the permit's committed effect set to permit.
    pub effect_seq: u64,
    pub idempotency_key: IdempotencyKey,
    pub valid_until_ms: i64,
    pub requested_at_ms: i64,
}

/// Durable view of one issued `EffectPermit` (§25.1 subset), including the
/// one-shot dispatch token.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IssuedPermit {
    pub effect_permit_id: EffectPermitId,
    pub idempotency_key: IdempotencyKey,
    pub permit_id: CommitPermitId,
    pub permit_epoch: u64,
    pub task_id: TaskId,
    pub attempt_id: TaskAttemptId,
    pub attempt_generation: Generation,
    pub effect_slot_id: EffectSlotId,
    pub effect_seq: u64,
    pub logical_effect_id: [u8; 32],
    pub retry_fence_epoch: u64,
    pub idempotency_identity_digest: [u8; 32],
    pub effect_set_root: [u8; 32],
    pub action_proposal_digest: [u8; 32],
    /// Verbatim copy of the parent `CommitPermit` registry binding. `None`
    /// is reserved for pre-v12 migrated effect permits.
    pub participant_registry_binding: Option<crate::ParticipantRegistryBinding>,
    pub control_epoch: u64,
    pub cancel_epoch: u64,
    /// Deterministically derived, single-use bearer token. Consumption is
    /// the `Permitted` → `Dispatched` CAS; a consumed token can never be
    /// presented as unexecuted (`[TASK-EFFECT-001]`).
    pub one_shot_dispatch_token: [u8; 32],
    pub valid_until_ms: i64,
    pub created_at_ms: i64,
}

/// Linearized decision of an `EffectPermit` request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EffectPermitDecision {
    Issued(Box<IssuedPermit>),
    /// Same idempotency key and same request bytes: the original permit
    /// with the same dispatch token.
    Replayed(Box<IssuedPermit>),
}

/// A one-shot dispatch-token consumption request (`[TASK-EFFECT-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DispatchRequest {
    pub task_id: TaskId,
    pub attempt_id: TaskAttemptId,
    pub attempt_generation: Generation,
    pub permit_id: CommitPermitId,
    pub permit_epoch: u64,
    pub effect_permit_id: EffectPermitId,
    pub dispatch_token: [u8; 32],
    pub dispatched_at_ms: i64,
}

/// An outcome registration request for a `Dispatched` slot.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutcomeRequest {
    pub task_id: TaskId,
    pub attempt_id: TaskAttemptId,
    pub attempt_generation: Generation,
    pub permit_id: CommitPermitId,
    pub permit_epoch: u64,
    pub effect_seq: u64,
    pub outcome: Outcome,
    pub recorded_at_ms: i64,
}

/// A no-effect closure request (`TaskNoEffectReceipt` path,
/// `[TASK-EFFECT-002]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NoEffectRequest {
    pub task_id: TaskId,
    pub attempt_id: TaskAttemptId,
    pub attempt_generation: Generation,
    pub permit_id: CommitPermitId,
    pub permit_epoch: u64,
    pub effect_seq: u64,
    pub reason: NoEffectReason,
    /// Token-unconsumed proof: for a `Permitted` slot the caller must
    /// present the still-unconsumed dispatch token; for a `Planned` slot no
    /// token exists yet and this must be `None`.
    pub dispatch_token: Option<[u8; 32]>,
    pub recorded_at_ms: i64,
}

/// Durable view of one effect receipt (closure / uncertainty / no-effect).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EffectReceipt {
    pub receipt_id: ReceiptId,
    pub task_id: TaskId,
    pub permit_id: CommitPermitId,
    pub effect_slot_id: EffectSlotId,
    pub effect_seq: u64,
    pub logical_effect_id: [u8; 32],
    pub kind: ReceiptKind,
    pub prior_slot_state: SlotState,
    pub no_effect_reason: Option<NoEffectReason>,
    pub proof_digest: [u8; 32],
    pub created_at_ms: i64,
}

/// Computes the Task-side success assertion for one authoritative
/// `EffectClosed` receipt. The assertion binds the immutable slot contract to
/// the exact closure receipt, so a proof copied from another effect, slot, or
/// closure attempt cannot satisfy this required obligation.
#[must_use]
pub fn expected_success_assertion_digest(slot: &SlotRecord, receipt: &EffectReceipt) -> [u8; 32] {
    let effect_seq = slot.effect_seq.to_be_bytes();
    let receipt_kind = receipt.kind.code().to_be_bytes();
    sha256(
        "llmos/task-effect-success-proof/v1",
        &[
            slot.effect_slot_id.as_bytes(),
            slot.logical_effect_id.as_slice(),
            &effect_seq,
            slot.success_criteria_digest.as_slice(),
            receipt.receipt_id.as_bytes(),
            receipt.proof_digest.as_slice(),
            &receipt_kind,
        ],
    )
}

/// Decision of an outcome or no-effect registration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EffectReceiptDecision {
    Recorded(Box<EffectReceipt>),
    /// Exact replay of an already-recorded registration.
    Replayed(Box<EffectReceipt>),
}

/// Durable per-permit effect-set control view: the committed
/// `effect_set_root`, the current slot-state root, the spec-mandated counts,
/// and the issued/dispatched/closed/outstanding roots
/// (`[TASK-COMMIT-002]` / `[TASK-EFFECT-002]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SetSummary {
    pub permit_id: CommitPermitId,
    pub effect_set_root: [u8; 32],
    pub effect_slot_state_root: [u8; 32],
    pub required_effect_count: u64,
    /// Placeholder semantics: required slots currently in `EffectClosed`.
    /// Success-criteria verification is out of scope for this slice.
    pub satisfied_required_effect_count: u64,
    /// Slots in `NoEffect` / `EffectClosed` / `EffectUnknown` /
    /// `ConfirmedNoEffect`.
    pub terminal_effect_count: u64,
    pub issued_effect_root: [u8; 32],
    pub dispatched_effect_root: [u8; 32],
    pub closed_effect_root: [u8; 32],
    pub outstanding_effect_root: [u8; 32],
}

pub(crate) struct StoredSummary {
    pub(crate) summary: SetSummary,
    revision: i64,
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

fn sha256(domain: &str, parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(domain.as_bytes());
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

pub(crate) fn derive_effect_slot_id(permit_id: CommitPermitId, effect_seq: u64) -> EffectSlotId {
    EffectSlotId::from_bytes(sha256_prefix16(
        "llmos/task-effect-slot/v1",
        &[permit_id.as_bytes(), &effect_seq.to_be_bytes()],
    ))
}

fn derive_effect_permit_id(
    permit_id: CommitPermitId,
    idempotency_key: IdempotencyKey,
) -> EffectPermitId {
    EffectPermitId::from_bytes(sha256_prefix16(
        "llmos/task-effect-permit/v1",
        &[permit_id.as_bytes(), idempotency_key.as_bytes()],
    ))
}

fn derive_dispatch_token(
    effect_permit_id: EffectPermitId,
    attempt_id: TaskAttemptId,
    attempt_generation: Generation,
) -> [u8; 32] {
    sha256(
        "llmos/task-effect-dispatch-token/v1",
        &[
            effect_permit_id.as_bytes(),
            attempt_id.as_bytes(),
            &attempt_generation.get().to_be_bytes(),
        ],
    )
}

fn dispatch_token_digest(token: &[u8; 32]) -> [u8; 32] {
    sha256("llmos/task-effect-dispatch-token-digest/v1", &[token])
}

pub(crate) fn derive_effect_receipt_id(
    domain: &str,
    slot_id: EffectSlotId,
    state_seq: u64,
) -> ReceiptId {
    ReceiptId::from_bytes(sha256_prefix16(
        domain,
        &[slot_id.as_bytes(), &state_seq.to_be_bytes()],
    ))
}

/// Pure `effect_set_root` hash over a declared effect set (no validation);
/// used for replay comparison where the stored set was already validated.
pub(crate) fn effect_set_root_of(planned_effects: &[PlannedEffect]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/task-effect-set/v1");
    for (index, planned) in planned_effects.iter().enumerate() {
        let seq = index as u64;
        hasher.update(seq.to_be_bytes());
        hasher.update(planned.descriptor.logical_effect_id());
        hasher.update(planned.descriptor.idempotency_identity_digest());
        hasher.update([u8::from(planned.required)]);
        match planned.required_condition_digest {
            Some(digest) => {
                hasher.update([1u8]);
                hasher.update(digest);
            }
            None => hasher.update([0u8]),
        }
        hasher.update(planned.success_criteria_digest);
        hasher.update(planned.action_proposal_digest);
    }
    hasher.finalize().into()
}

/// Validates the declared planned effect set (`[TASK-EFFECT-002]`): dense
/// `effect_seq` by vector position, unique `LogicalEffectId` per set, and
/// every descriptor bound to this task and generation.
pub(crate) fn validate_planned_effects(
    task_id: TaskId,
    task_generation: Generation,
    planned_effects: &[PlannedEffect],
) -> Result<[u8; 32], TaskStoreError> {
    let mut seen: Vec<[u8; 32]> = Vec::with_capacity(planned_effects.len());
    for planned in planned_effects {
        if planned.descriptor.task_id != task_id
            || planned.descriptor.task_generation != task_generation
        {
            return Err(TaskStoreError::InvalidEffectSet {
                reason: "descriptor is not bound to this task and generation",
            });
        }
        let logical_effect_id = planned.descriptor.logical_effect_id();
        if seen.contains(&logical_effect_id) {
            return Err(TaskStoreError::InvalidEffectSet {
                reason: "duplicate LogicalEffectId inside one effect set",
            });
        }
        seen.push(logical_effect_id);
    }
    Ok(effect_set_root_of(planned_effects))
}

fn subset_root(domain: &str, slots: &[&SlotRecord]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(domain.as_bytes());
    for slot in slots {
        hasher.update(slot.effect_seq.to_be_bytes());
        hasher.update(slot.logical_effect_id);
    }
    hasher.finalize().into()
}

fn slot_state_root(slots: &[SlotRecord]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/task-effect-slot-state/v1");
    for slot in slots {
        hasher.update(slot.effect_seq.to_be_bytes());
        hasher.update([u8::try_from(slot.state.code()).unwrap_or(u8::MAX)]);
        hasher.update(slot.state_seq.to_be_bytes());
    }
    hasher.finalize().into()
}

fn as_count(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

/// Recomputes the spec-mandated roots and counts from the current slot rows
/// (`[TASK-EFFECT-002]` final clause).
fn summarize(
    permit_id: CommitPermitId,
    effect_set_root: [u8; 32],
    slots: &[SlotRecord],
) -> SetSummary {
    let issued: Vec<&SlotRecord> = slots
        .iter()
        .filter(|slot| slot.effect_permit_id.is_some())
        .collect();
    let dispatched: Vec<&SlotRecord> = slots
        .iter()
        .filter(|slot| {
            matches!(
                slot.state,
                SlotState::Dispatched | SlotState::EffectClosed | SlotState::EffectUnknown
            )
        })
        .collect();
    let closed: Vec<&SlotRecord> = slots
        .iter()
        .filter(|slot| slot.state == SlotState::EffectClosed)
        .collect();
    let outstanding: Vec<&SlotRecord> = slots
        .iter()
        .filter(|slot| {
            matches!(
                slot.state,
                SlotState::Permitted | SlotState::Dispatched | SlotState::EffectUnknown
            )
        })
        .collect();
    SetSummary {
        permit_id,
        effect_set_root,
        effect_slot_state_root: slot_state_root(slots),
        required_effect_count: as_count(slots.iter().filter(|slot| slot.required).count()),
        satisfied_required_effect_count: as_count(
            slots
                .iter()
                .filter(|slot| slot.required && slot.state == SlotState::EffectClosed)
                .count(),
        ),
        terminal_effect_count: as_count(
            slots
                .iter()
                .filter(|slot| {
                    matches!(
                        slot.state,
                        SlotState::NoEffect
                            | SlotState::EffectClosed
                            | SlotState::EffectUnknown
                            | SlotState::ConfirmedNoEffect
                    )
                })
                .count(),
        ),
        issued_effect_root: subset_root("llmos/task-effect-issued/v1", &issued),
        dispatched_effect_root: subset_root("llmos/task-effect-dispatched/v1", &dispatched),
        closed_effect_root: subset_root("llmos/task-effect-closed/v1", &closed),
        outstanding_effect_root: subset_root("llmos/task-effect-outstanding/v1", &outstanding),
    }
}

const SLOT_COLUMNS: &str = "permit_id, effect_seq, task_id, effect_slot_id, logical_effect_id,
     idempotency_identity_digest, required, required_condition_digest,
     success_criteria_digest, action_proposal_digest, slot_state, state_seq,
     effect_permit_id, effect_receipt_id, created_at_ms, updated_at_ms";

fn decode_slot_row(row: &rusqlite::Row<'_>) -> Result<SlotRecord, TaskStoreError> {
    let required: i64 = row.get(6)?;
    let condition: Option<Vec<u8>> = row.get(7)?;
    let state_seq: i64 = row.get(11)?;
    if state_seq < 0 {
        return Err(TaskStoreError::CorruptRecord(
            "negative effect slot state_seq",
        ));
    }
    Ok(SlotRecord {
        permit_id: CommitPermitId::from_bytes(blob16(row, 0)?),
        effect_seq: u64_from_blob(row, 1)?,
        task_id: TaskId::from_bytes(blob16(row, 2)?),
        effect_slot_id: EffectSlotId::from_bytes(blob16(row, 3)?),
        logical_effect_id: blob32(row, 4)?,
        idempotency_identity_digest: blob32(row, 5)?,
        required: required != 0,
        required_condition_digest: condition
            .map(|bytes| {
                <[u8; 32]>::try_from(bytes.as_slice())
                    .map_err(|_| TaskStoreError::CorruptRecord("expected 32-byte condition digest"))
            })
            .transpose()?,
        success_criteria_digest: blob32(row, 8)?,
        action_proposal_digest: blob32(row, 9)?,
        state: SlotState::from_code(row.get(10)?)?,
        state_seq: u64::try_from(state_seq)
            .map_err(|_| TaskStoreError::CorruptRecord("slot state_seq overflow"))?,
        effect_permit_id: optional_blob16(row, 12)?.map(EffectPermitId::from_bytes),
        effect_receipt_id: optional_blob16(row, 13)?.map(ReceiptId::from_bytes),
        created_at_ms: row.get(14)?,
        updated_at_ms: row.get(15)?,
    })
}

pub(crate) fn load_slot(
    source: &impl SqlRead,
    permit_id: CommitPermitId,
    effect_seq: u64,
) -> Result<SlotRecord, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {SLOT_COLUMNS} FROM effect_slots WHERE permit_id = ?1 AND effect_seq = ?2"
    ))?;
    let mut rows = statement.query(params![
        permit_id.as_bytes().as_slice(),
        encode_u64(effect_seq).as_slice(),
    ])?;
    rows.next()?
        .map(decode_slot_row)
        .transpose()?
        .ok_or(TaskStoreError::EffectSlotNotFound)
}

pub(crate) fn list_slots(
    source: &impl SqlRead,
    permit_id: CommitPermitId,
) -> Result<Vec<SlotRecord>, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {SLOT_COLUMNS} FROM effect_slots WHERE permit_id = ?1 ORDER BY effect_seq"
    ))?;
    let mut rows = statement.query([permit_id.as_bytes().as_slice()])?;
    let mut slots = Vec::new();
    while let Some(row) = rows.next()? {
        slots.push(decode_slot_row(row)?);
    }
    Ok(slots)
}

/// Reads the committed `effect_set_root` of a permit, if it declared one.
pub(crate) fn stored_effect_set_root(
    source: &impl SqlRead,
    permit_id: CommitPermitId,
) -> Result<Option<[u8; 32]>, TaskStoreError> {
    let mut statement = source
        .prepare_statement("SELECT effect_set_root FROM permit_effect_sets WHERE permit_id = ?1")?;
    let mut rows = statement.query([permit_id.as_bytes().as_slice()])?;
    rows.next()?
        .map(|row| {
            let value: Vec<u8> = row.get(0)?;
            <[u8; 32]>::try_from(value.as_slice())
                .map_err(|_| TaskStoreError::CorruptRecord("expected 32-byte effect set root"))
        })
        .transpose()
}

/// Persists the declared effect set at permit issuance: one `Planned` slot
/// row per entry plus the per-permit effect-set control row.
pub(crate) fn insert_effect_set(
    transaction: &Transaction<'_>,
    permit: &PermitRecord,
    planned_effects: &[PlannedEffect],
    effect_set_root: [u8; 32],
    now_ms: i64,
) -> Result<(), TaskStoreError> {
    if planned_effects.is_empty() {
        return Ok(());
    }
    let mut slots = Vec::with_capacity(planned_effects.len());
    for (index, planned) in planned_effects.iter().enumerate() {
        let effect_seq = index as u64;
        let slot = SlotRecord {
            permit_id: permit.permit_id,
            effect_seq,
            task_id: permit.task_id,
            effect_slot_id: derive_effect_slot_id(permit.permit_id, effect_seq),
            logical_effect_id: planned.descriptor.logical_effect_id(),
            idempotency_identity_digest: planned.descriptor.idempotency_identity_digest(),
            required: planned.required,
            required_condition_digest: planned.required_condition_digest,
            success_criteria_digest: planned.success_criteria_digest,
            action_proposal_digest: planned.action_proposal_digest,
            state: SlotState::Planned,
            state_seq: 0,
            effect_permit_id: None,
            effect_receipt_id: None,
            created_at_ms: now_ms,
            updated_at_ms: now_ms,
        };
        insert_slot(transaction, &slot)?;
        slots.push(slot);
    }
    let summary = summarize(permit.permit_id, effect_set_root, &slots);
    transaction.execute(
        "INSERT INTO permit_effect_sets (
            permit_id, task_id, effect_set_root, effect_slot_state_root,
            required_effect_count, satisfied_required_effect_count,
            terminal_effect_count, issued_effect_root, dispatched_effect_root,
            closed_effect_root, outstanding_effect_root, revision,
            created_at_ms, updated_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 0, ?12, ?13)",
        params![
            permit.permit_id.as_bytes().as_slice(),
            permit.task_id.as_bytes().as_slice(),
            summary.effect_set_root.as_slice(),
            summary.effect_slot_state_root.as_slice(),
            count_to_i64(summary.required_effect_count)?,
            count_to_i64(summary.satisfied_required_effect_count)?,
            count_to_i64(summary.terminal_effect_count)?,
            summary.issued_effect_root.as_slice(),
            summary.dispatched_effect_root.as_slice(),
            summary.closed_effect_root.as_slice(),
            summary.outstanding_effect_root.as_slice(),
            now_ms,
            now_ms,
        ],
    )?;
    Ok(())
}

fn count_to_i64(count: u64) -> Result<i64, TaskStoreError> {
    i64::try_from(count).map_err(|_| TaskStoreError::EpochExhausted)
}

fn insert_slot(transaction: &Transaction<'_>, slot: &SlotRecord) -> Result<(), TaskStoreError> {
    transaction.execute(
        "INSERT INTO effect_slots (
            permit_id, effect_seq, task_id, effect_slot_id, logical_effect_id,
            idempotency_identity_digest, required, required_condition_digest,
            success_criteria_digest, action_proposal_digest, slot_state,
            state_seq, effect_permit_id, effect_receipt_id,
            created_at_ms, updated_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, 0, NULL, NULL, ?12, ?13)",
        params![
            slot.permit_id.as_bytes().as_slice(),
            encode_u64(slot.effect_seq).as_slice(),
            slot.task_id.as_bytes().as_slice(),
            slot.effect_slot_id.as_bytes().as_slice(),
            slot.logical_effect_id.as_slice(),
            slot.idempotency_identity_digest.as_slice(),
            i64::from(slot.required),
            slot.required_condition_digest
                .as_ref()
                .map(<[u8; 32]>::as_slice),
            slot.success_criteria_digest.as_slice(),
            slot.action_proposal_digest.as_slice(),
            slot.state.code(),
            slot.created_at_ms,
            slot.updated_at_ms,
        ],
    )?;
    Ok(())
}

/// Compare-and-swap one slot transition; `changed != 1` means the durable
/// state moved under us, which the single-writer gate makes impossible, so
/// it is treated as corruption (fail closed).
pub(crate) fn cas_slot(
    transaction: &Transaction<'_>,
    slot: &SlotRecord,
    new_state: SlotState,
    effect_permit_id: Option<EffectPermitId>,
    token_digest: Option<[u8; 32]>,
    effect_receipt_id: Option<ReceiptId>,
    now_ms: i64,
) -> Result<SlotRecord, TaskStoreError> {
    let changed = transaction.execute(
        "UPDATE effect_slots SET
            slot_state = ?1, state_seq = state_seq + 1,
            effect_permit_id = COALESCE(?2, effect_permit_id),
            dispatch_token_digest = COALESCE(?3, dispatch_token_digest),
            effect_receipt_id = COALESCE(?4, effect_receipt_id),
            updated_at_ms = ?5
         WHERE permit_id = ?6 AND effect_seq = ?7 AND slot_state = ?8 AND state_seq = ?9",
        params![
            new_state.code(),
            effect_permit_id
                .map(EffectPermitId::into_bytes)
                .as_ref()
                .map(<[u8; 16]>::as_slice),
            token_digest.as_ref().map(<[u8; 32]>::as_slice),
            effect_receipt_id
                .map(ReceiptId::into_bytes)
                .as_ref()
                .map(<[u8; 16]>::as_slice),
            now_ms,
            slot.permit_id.as_bytes().as_slice(),
            encode_u64(slot.effect_seq).as_slice(),
            slot.state.code(),
            count_to_i64(slot.state_seq)?,
        ],
    )?;
    if changed != 1 {
        return Err(TaskStoreError::CorruptRecord(
            "effect slot compare-and-swap failed",
        ));
    }
    let mut updated = slot.clone();
    updated.state = new_state;
    updated.state_seq += 1;
    if effect_permit_id.is_some() {
        updated.effect_permit_id = effect_permit_id;
    }
    if effect_receipt_id.is_some() {
        updated.effect_receipt_id = effect_receipt_id;
    }
    updated.updated_at_ms = now_ms;
    Ok(updated)
}

pub(crate) fn load_summary(
    source: &impl SqlRead,
    permit_id: CommitPermitId,
) -> Result<Option<StoredSummary>, TaskStoreError> {
    let mut statement = source.prepare_statement(
        "SELECT effect_set_root, effect_slot_state_root, required_effect_count,
                satisfied_required_effect_count, terminal_effect_count,
                issued_effect_root, dispatched_effect_root, closed_effect_root,
                outstanding_effect_root, revision
         FROM permit_effect_sets WHERE permit_id = ?1",
    )?;
    let mut rows = statement.query([permit_id.as_bytes().as_slice()])?;
    rows.next()?
        .map(|row| {
            let revision: i64 = row.get(9)?;
            if revision < 0 {
                return Err(TaskStoreError::CorruptRecord(
                    "negative effect set revision",
                ));
            }
            Ok(StoredSummary {
                summary: SetSummary {
                    permit_id,
                    effect_set_root: blob32(row, 0)?,
                    effect_slot_state_root: blob32(row, 1)?,
                    required_effect_count: count_from_row(row, 2)?,
                    satisfied_required_effect_count: count_from_row(row, 3)?,
                    terminal_effect_count: count_from_row(row, 4)?,
                    issued_effect_root: blob32(row, 5)?,
                    dispatched_effect_root: blob32(row, 6)?,
                    closed_effect_root: blob32(row, 7)?,
                    outstanding_effect_root: blob32(row, 8)?,
                },
                revision,
            })
        })
        .transpose()
}

fn count_from_row(row: &rusqlite::Row<'_>, index: usize) -> Result<u64, TaskStoreError> {
    let value: i64 = row.get(index)?;
    u64::try_from(value).map_err(|_| TaskStoreError::CorruptRecord("negative effect count"))
}

/// Recomputes and persists all roots/counts after a slot transition
/// (`[TASK-EFFECT-002]` final clause), under revision CAS.
pub(crate) fn refresh_summary(
    transaction: &Transaction<'_>,
    permit_id: CommitPermitId,
    now_ms: i64,
) -> Result<(), TaskStoreError> {
    let stored = load_summary(transaction, permit_id)?.ok_or(TaskStoreError::CorruptRecord(
        "effect slot exists without its effect-set control row",
    ))?;
    let slots = list_slots(transaction, permit_id)?;
    let summary = summarize(permit_id, stored.summary.effect_set_root, &slots);
    let changed = transaction.execute(
        "UPDATE permit_effect_sets SET
            effect_slot_state_root = ?1, required_effect_count = ?2,
            satisfied_required_effect_count = ?3, terminal_effect_count = ?4,
            issued_effect_root = ?5, dispatched_effect_root = ?6,
            closed_effect_root = ?7, outstanding_effect_root = ?8,
            updated_at_ms = ?9, revision = revision + 1
         WHERE permit_id = ?10 AND revision = ?11",
        params![
            summary.effect_slot_state_root.as_slice(),
            count_to_i64(summary.required_effect_count)?,
            count_to_i64(summary.satisfied_required_effect_count)?,
            count_to_i64(summary.terminal_effect_count)?,
            summary.issued_effect_root.as_slice(),
            summary.dispatched_effect_root.as_slice(),
            summary.closed_effect_root.as_slice(),
            summary.outstanding_effect_root.as_slice(),
            now_ms,
            permit_id.as_bytes().as_slice(),
            stored.revision,
        ],
    )?;
    if changed != 1 {
        return Err(TaskStoreError::CorruptRecord(
            "effect set revision compare-and-swap failed",
        ));
    }
    Ok(())
}

const EFFECT_PERMIT_COLUMNS: &str = "effect_permit_id, task_id, idempotency_key, permit_id,
     permit_epoch, attempt_id, attempt_generation, effect_slot_id, effect_seq,
     logical_effect_id, retry_fence_epoch, idempotency_identity_digest,
     effect_set_root, action_proposal_digest, control_epoch, cancel_epoch,
     valid_until_ms, created_at_ms, participant_registry_generation,
     participant_registry_root";

fn decode_effect_permit_row(row: &rusqlite::Row<'_>) -> Result<IssuedPermit, TaskStoreError> {
    let effect_permit_id = EffectPermitId::from_bytes(blob16(row, 0)?);
    let attempt_id = TaskAttemptId::from_bytes(blob16(row, 5)?);
    let attempt_generation = generation_from_blob(row, 6)?;
    Ok(IssuedPermit {
        effect_permit_id,
        idempotency_key: IdempotencyKey::from_bytes(blob16(row, 2)?),
        permit_id: CommitPermitId::from_bytes(blob16(row, 3)?),
        permit_epoch: u64_from_blob(row, 4)?,
        task_id: TaskId::from_bytes(blob16(row, 1)?),
        attempt_id,
        attempt_generation,
        effect_slot_id: EffectSlotId::from_bytes(blob16(row, 7)?),
        effect_seq: u64_from_blob(row, 8)?,
        logical_effect_id: blob32(row, 9)?,
        retry_fence_epoch: u64_from_blob(row, 10)?,
        idempotency_identity_digest: blob32(row, 11)?,
        effect_set_root: blob32(row, 12)?,
        action_proposal_digest: blob32(row, 13)?,
        participant_registry_binding: decode_participant_binding(row, 18)?,
        control_epoch: u64_from_blob(row, 14)?,
        cancel_epoch: u64_from_blob(row, 15)?,
        one_shot_dispatch_token: derive_dispatch_token(
            effect_permit_id,
            attempt_id,
            attempt_generation,
        ),
        valid_until_ms: row.get(16)?,
        created_at_ms: row.get(17)?,
    })
}

fn load_effect_permit_by_key(
    source: &impl SqlRead,
    task_id: TaskId,
    idempotency_key: IdempotencyKey,
) -> Result<Option<IssuedPermit>, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {EFFECT_PERMIT_COLUMNS} FROM effect_permits
         WHERE task_id = ?1 AND idempotency_key = ?2"
    ))?;
    let mut rows = statement.query(params![
        task_id.as_bytes().as_slice(),
        idempotency_key.as_bytes().as_slice(),
    ])?;
    rows.next()?.map(decode_effect_permit_row).transpose()
}

fn load_effect_permit_by_id(
    source: &impl SqlRead,
    task_id: TaskId,
    effect_permit_id: EffectPermitId,
) -> Result<IssuedPermit, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {EFFECT_PERMIT_COLUMNS} FROM effect_permits
         WHERE task_id = ?1 AND effect_permit_id = ?2"
    ))?;
    let mut rows = statement.query(params![
        task_id.as_bytes().as_slice(),
        effect_permit_id.as_bytes().as_slice(),
    ])?;
    rows.next()?
        .map(decode_effect_permit_row)
        .transpose()?
        .ok_or(TaskStoreError::EffectPermitNotFound)
}

fn insert_effect_permit(
    transaction: &Transaction<'_>,
    record: &IssuedPermit,
    token_digest: [u8; 32],
) -> Result<(), TaskStoreError> {
    if record.participant_registry_binding.is_none() {
        return Err(TaskStoreError::ParticipantRegistryBindingMissing);
    }
    let participant_generation = record
        .participant_registry_binding
        .map(|binding| encode_u64(binding.generation));
    let participant_root = record
        .participant_registry_binding
        .map(|binding| binding.root);
    transaction.execute(
        "INSERT INTO effect_permits (
            effect_permit_id, task_id, idempotency_key, permit_id, permit_epoch,
            attempt_id, attempt_generation, effect_slot_id, effect_seq,
            logical_effect_id, retry_fence_epoch, idempotency_identity_digest,
            effect_set_root, action_proposal_digest, control_epoch, cancel_epoch,
            dispatch_token_digest, valid_until_ms, created_at_ms,
            participant_registry_generation, participant_registry_root
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)",
        params![
            record.effect_permit_id.as_bytes().as_slice(),
            record.task_id.as_bytes().as_slice(),
            record.idempotency_key.as_bytes().as_slice(),
            record.permit_id.as_bytes().as_slice(),
            encode_u64(record.permit_epoch).as_slice(),
            record.attempt_id.as_bytes().as_slice(),
            encode_u64(record.attempt_generation.get()).as_slice(),
            record.effect_slot_id.as_bytes().as_slice(),
            encode_u64(record.effect_seq).as_slice(),
            record.logical_effect_id.as_slice(),
            encode_u64(record.retry_fence_epoch).as_slice(),
            record.idempotency_identity_digest.as_slice(),
            record.effect_set_root.as_slice(),
            record.action_proposal_digest.as_slice(),
            encode_u64(record.control_epoch).as_slice(),
            encode_u64(record.cancel_epoch).as_slice(),
            token_digest.as_slice(),
            record.valid_until_ms,
            record.created_at_ms,
            participant_generation.as_ref().map(<[u8; 8]>::as_slice),
            participant_root.as_ref().map(<[u8; 32]>::as_slice),
        ],
    )?;
    Ok(())
}

fn load_dispatch_token_digest(
    source: &impl SqlRead,
    effect_permit_id: EffectPermitId,
) -> Result<[u8; 32], TaskStoreError> {
    let mut statement = source.prepare_statement(
        "SELECT dispatch_token_digest FROM effect_permits WHERE effect_permit_id = ?1",
    )?;
    let mut rows = statement.query([effect_permit_id.as_bytes().as_slice()])?;
    let value: Vec<u8> = rows
        .next()?
        .ok_or(TaskStoreError::EffectPermitNotFound)?
        .get(0)?;
    <[u8; 32]>::try_from(value.as_slice())
        .map_err(|_| TaskStoreError::CorruptRecord("expected 32-byte dispatch token digest"))
}

pub(crate) fn insert_effect_receipt(
    transaction: &Transaction<'_>,
    receipt: &EffectReceipt,
) -> Result<(), TaskStoreError> {
    transaction.execute(
        "INSERT INTO effect_receipts (
            receipt_id, task_id, permit_id, effect_slot_id, effect_seq,
            logical_effect_id, receipt_kind, prior_slot_state,
            no_effect_reason, proof_digest, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            receipt.receipt_id.as_bytes().as_slice(),
            receipt.task_id.as_bytes().as_slice(),
            receipt.permit_id.as_bytes().as_slice(),
            receipt.effect_slot_id.as_bytes().as_slice(),
            encode_u64(receipt.effect_seq).as_slice(),
            receipt.logical_effect_id.as_slice(),
            receipt.kind.code(),
            receipt.prior_slot_state.code(),
            receipt.no_effect_reason.map(NoEffectReason::code),
            receipt.proof_digest.as_slice(),
            receipt.created_at_ms,
        ],
    )?;
    Ok(())
}

pub(crate) fn load_effect_receipt(
    source: &impl SqlRead,
    receipt_id: ReceiptId,
) -> Result<EffectReceipt, TaskStoreError> {
    let mut statement = source.prepare_statement(
        "SELECT receipt_id, task_id, permit_id, effect_slot_id, effect_seq,
                logical_effect_id, receipt_kind, prior_slot_state,
                no_effect_reason, proof_digest, created_at_ms
         FROM effect_receipts WHERE receipt_id = ?1",
    )?;
    let mut rows = statement.query([receipt_id.as_bytes().as_slice()])?;
    rows.next()?
        .map(decode_effect_receipt_row)
        .transpose()?
        .ok_or(TaskStoreError::ReceiptNotFound)
}

fn decode_effect_receipt_row(row: &rusqlite::Row<'_>) -> Result<EffectReceipt, TaskStoreError> {
    let reason: Option<i64> = row.get(8)?;
    Ok(EffectReceipt {
        receipt_id: ReceiptId::from_bytes(blob16(row, 0)?),
        task_id: TaskId::from_bytes(blob16(row, 1)?),
        permit_id: CommitPermitId::from_bytes(blob16(row, 2)?),
        effect_slot_id: EffectSlotId::from_bytes(blob16(row, 3)?),
        effect_seq: u64_from_blob(row, 4)?,
        logical_effect_id: blob32(row, 5)?,
        kind: ReceiptKind::from_code(row.get(6)?)?,
        prior_slot_state: SlotState::from_code(row.get(7)?)?,
        no_effect_reason: reason.map(NoEffectReason::from_code).transpose()?,
        proof_digest: blob32(row, 9)?,
        created_at_ms: row.get(10)?,
    })
}

fn replay_no_effect(
    transaction: &Transaction<'_>,
    slot: &SlotRecord,
    reason: NoEffectReason,
) -> Result<EffectReceipt, TaskStoreError> {
    let receipt_id = slot.effect_receipt_id.ok_or(TaskStoreError::CorruptRecord(
        "no-effect slot lacks its effect receipt",
    ))?;
    let receipt = load_effect_receipt(transaction, receipt_id)?;
    if receipt.no_effect_reason == Some(reason) {
        Ok(receipt)
    } else {
        Err(TaskStoreError::IdempotencyConflict)
    }
}

fn build_no_effect_receipt(slot: &SlotRecord, request: &NoEffectRequest) -> EffectReceipt {
    let receipt_id = derive_effect_receipt_id(
        "llmos/task-no-effect-receipt/v1",
        slot.effect_slot_id,
        slot.state_seq + 1,
    );
    let proof_digest = sha256(
        "llmos/task-no-effect-proof/v1",
        &[
            slot.effect_slot_id.as_bytes(),
            &slot.state_seq.to_be_bytes(),
            &request.dispatch_token.unwrap_or([0u8; 32]),
        ],
    );
    EffectReceipt {
        receipt_id,
        task_id: request.task_id,
        permit_id: request.permit_id,
        effect_slot_id: slot.effect_slot_id,
        effect_seq: slot.effect_seq,
        logical_effect_id: slot.logical_effect_id,
        kind: ReceiptKind::NoEffect,
        prior_slot_state: slot.state,
        no_effect_reason: Some(request.reason),
        proof_digest,
        created_at_ms: request.recorded_at_ms,
    }
}

/// Shared holder/epoch validation for every effect-plane mutation: the
/// caller must be the outstanding `CommitPermit` holder presenting the
/// exact attempt generation and permit epoch (`[TASK-RACE-001]`).
pub(crate) struct HolderContext {
    task: StoredTask,
    permit: PermitRecord,
}

pub(crate) fn check_holder(
    transaction: &Transaction<'_>,
    task_id: TaskId,
    attempt_id: TaskAttemptId,
    attempt_generation: Generation,
    permit_id: CommitPermitId,
    permit_epoch: u64,
) -> Result<HolderContext, TaskStoreError> {
    let task = load_task(transaction, task_id)?;
    let permit = load_permit_by_id(transaction, task_id, permit_id)?;
    let attempt = load_attempt(transaction, task_id, attempt_id)?;
    if attempt.attempt_generation != attempt_generation {
        return Err(TaskStoreError::InvalidGeneration);
    }
    if permit.state != PermitState::Issued {
        return Err(TaskStoreError::PermitNotIssued);
    }
    if permit.attempt_id != attempt.attempt_id
        || permit.attempt_generation != attempt.attempt_generation
    {
        return Err(TaskStoreError::NotPermitHolder);
    }
    if permit.permit_epoch != permit_epoch {
        return Err(TaskStoreError::PermitEpochMismatch);
    }
    Ok(HolderContext { task, permit })
}

/// The head/fence revalidation every effect-permit issuance and every
/// dispatch must pass (`[TASK-EFFECT-001]`): the current `TaskHead` must
/// still be bit-identical to the head the `CommitPermit` expected.
fn check_head_unchanged(task: &StoredTask, permit: &PermitRecord) -> Result<(), TaskStoreError> {
    if task.record.head_commit_seq != permit.expected_head_commit_seq
        || task.record.head_effect_history_root != permit.expected_effect_history_root
        || task.record.retry_fence_epoch != permit.expected_retry_fence_epoch
    {
        return Err(TaskStoreError::StaleTaskHead);
    }
    Ok(())
}

fn check_group_unchanged(
    transaction: &Transaction<'_>,
    attempt_id: TaskAttemptId,
    permit: &PermitRecord,
) -> Result<(), TaskStoreError> {
    crate::group::validate_commit_binding(transaction, attempt_id, permit.group_binding)
}

fn check_commit_context(
    transaction: &Transaction<'_>,
    attempt_id: TaskAttemptId,
    context: &HolderContext,
) -> Result<(), TaskStoreError> {
    check_head_unchanged(&context.task, &context.permit)?;
    check_group_unchanged(transaction, attempt_id, &context.permit)?;
    crate::participant::validate_frozen_binding(
        transaction,
        &context.task.record,
        context.permit.participant_registry_binding,
    )?;
    Ok(())
}

impl SqliteTaskAuthority {
    /// Runs the linearized `EffectPermit` issuance CAS (`[TASK-EFFECT-001]`
    /// first half, `[TASK-RACE-001]`).
    ///
    /// Only the outstanding `CommitPermit` holder can obtain an
    /// `EffectPermit`, only for a slot declared in the permit's committed
    /// effect set, only while the `TaskHead` is still exactly the permitted
    /// head, and only when no cancellation committed after permit issuance
    /// (`[TASK-CANCEL-002]` blocks new effect permits). The slot moves
    /// `Planned` → `Permitted`, the one-shot dispatch token is minted, and
    /// the issued/outstanding roots are recomputed in the same transaction.
    /// Same key + same bytes replays the original permit (and token);
    /// same key + different bytes fails closed.
    ///
    /// # Errors
    ///
    /// Returns a not-found, holder, epoch, stale-head, cancel, slot-state,
    /// idempotency-conflict, or storage error.
    #[allow(clippy::too_many_lines)] // Keep the one-transaction authority decision contiguous.
    pub fn request_effect_permit(
        &self,
        request: PermitRequest,
    ) -> Result<EffectPermitDecision, TaskStoreError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) =
            load_effect_permit_by_key(&transaction, request.task_id, request.idempotency_key)?
        {
            let parent = load_permit_by_id(&transaction, request.task_id, existing.permit_id)?;
            if existing.participant_registry_binding != parent.participant_registry_binding {
                return Err(TaskStoreError::ParticipantRegistryBindingMismatch);
            }
            let same_bytes = existing.permit_id == request.permit_id
                && existing.attempt_id == request.attempt_id
                && existing.attempt_generation == request.attempt_generation
                && existing.effect_seq == request.effect_seq
                && existing.valid_until_ms == request.valid_until_ms;
            if !same_bytes {
                return Err(TaskStoreError::IdempotencyConflict);
            }
            transaction.commit()?;
            return Ok(EffectPermitDecision::Replayed(Box::new(existing)));
        }
        let context = check_holder(
            &transaction,
            request.task_id,
            request.attempt_id,
            request.attempt_generation,
            request.permit_id,
            request.permit_epoch,
        )?;
        check_commit_context(&transaction, request.attempt_id, &context)?;
        if context.task.record.cancel_epoch != context.permit.cancel_epoch {
            return Err(TaskStoreError::CancellationCommitted {
                cancel_epoch: context.task.record.cancel_epoch,
            });
        }
        // `[TASK-COMMIT-003]`: an adopted permit's scope is
        // RECONCILE_CLOSE_OR_QUARANTINE_ONLY — no new EffectPermits.
        if crate::reconcile::has_adoption(&transaction, request.permit_id)? {
            return Err(TaskStoreError::AdoptionScopeViolation);
        }
        let slot = load_slot(&transaction, request.permit_id, request.effect_seq)?;
        if slot.state != SlotState::Planned {
            return Err(TaskStoreError::InvalidEffectSlotState { state: slot.state });
        }
        // `[TASK-RETRY-EFFECT-001]`: a logical effect already
        // `EFFECT_CLOSED` in the durable cross-attempt history must never
        // be silently re-dispatched; the original result stays readable
        // via `lookup_effect_history`.
        crate::reconcile::check_not_closed_in_history(
            &transaction,
            request.task_id,
            &slot.logical_effect_id,
        )?;
        let effect_permit_id = derive_effect_permit_id(request.permit_id, request.idempotency_key);
        let token = derive_dispatch_token(
            effect_permit_id,
            request.attempt_id,
            request.attempt_generation,
        );
        let token_digest = dispatch_token_digest(&token);
        let control_epoch = context
            .task
            .record
            .control_epoch
            .checked_add(1)
            .ok_or(TaskStoreError::EpochExhausted)?;
        let effect_set_root = stored_effect_set_root(&transaction, request.permit_id)?.ok_or(
            TaskStoreError::CorruptRecord("effect slot exists without its effect-set control row"),
        )?;
        let record = IssuedPermit {
            effect_permit_id,
            idempotency_key: request.idempotency_key,
            permit_id: request.permit_id,
            permit_epoch: context.permit.permit_epoch,
            task_id: request.task_id,
            attempt_id: request.attempt_id,
            attempt_generation: request.attempt_generation,
            effect_slot_id: slot.effect_slot_id,
            effect_seq: slot.effect_seq,
            logical_effect_id: slot.logical_effect_id,
            retry_fence_epoch: context.task.record.retry_fence_epoch,
            idempotency_identity_digest: slot.idempotency_identity_digest,
            effect_set_root,
            action_proposal_digest: slot.action_proposal_digest,
            participant_registry_binding: context.permit.participant_registry_binding,
            control_epoch,
            cancel_epoch: context.task.record.cancel_epoch,
            one_shot_dispatch_token: token,
            valid_until_ms: request.valid_until_ms,
            created_at_ms: request.requested_at_ms,
        };
        insert_effect_permit(&transaction, &record, token_digest)?;
        cas_slot(
            &transaction,
            &slot,
            SlotState::Permitted,
            Some(effect_permit_id),
            Some(token_digest),
            None,
            request.requested_at_ms,
        )?;
        refresh_summary(&transaction, request.permit_id, request.requested_at_ms)?;
        update_task(
            &transaction,
            &context.task,
            request.requested_at_ms,
            |record| {
                record.control_epoch = control_epoch;
            },
        )?;
        transaction.commit()?;
        Ok(EffectPermitDecision::Issued(Box::new(record)))
    }

    /// Atomically consumes a one-shot dispatch token: `Permitted` →
    /// `Dispatched` (`[TASK-EFFECT-001]` second half).
    ///
    /// The current head/fence must still match the permit, and the cancel
    /// epoch must be unchanged since the `EffectPermit` was issued: a
    /// cancellation committed in between yields a typed
    /// [`TaskStoreError::CancellationCommitted`] rejection and the slot
    /// stays `Permitted` for the cancel path to close as no-effect
    /// (`[TASK-CANCEL-003]`). Presenting the same token twice fails closed
    /// with [`TaskStoreError::DispatchTokenConsumed`]; a consumed token can
    /// never masquerade as unexecuted.
    ///
    /// # Errors
    ///
    /// Returns a not-found, holder, epoch, stale-head, cancel, token,
    /// slot-state, or storage error.
    pub fn consume_dispatch_token(
        &self,
        request: DispatchRequest,
    ) -> Result<SlotRecord, TaskStoreError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let context = check_holder(
            &transaction,
            request.task_id,
            request.attempt_id,
            request.attempt_generation,
            request.permit_id,
            request.permit_epoch,
        )?;
        check_commit_context(&transaction, request.attempt_id, &context)?;
        // `[TASK-COMMIT-003]`: an adopted permit's scope is
        // RECONCILE_CLOSE_OR_QUARANTINE_ONLY — no new dispatches.
        if crate::reconcile::has_adoption(&transaction, request.permit_id)? {
            return Err(TaskStoreError::AdoptionScopeViolation);
        }
        let effect_permit =
            load_effect_permit_by_id(&transaction, request.task_id, request.effect_permit_id)?;
        if effect_permit.permit_id != request.permit_id {
            return Err(TaskStoreError::EffectPermitNotFound);
        }
        crate::participant::validate_copied_binding(
            context.permit.participant_registry_binding,
            effect_permit.participant_registry_binding,
        )?;
        let slot = load_slot(&transaction, request.permit_id, effect_permit.effect_seq)?;
        let presented_digest = dispatch_token_digest(&request.dispatch_token);
        let stored_digest = load_dispatch_token_digest(&transaction, request.effect_permit_id)?;
        match slot.state {
            SlotState::Permitted => {
                if presented_digest != stored_digest {
                    return Err(TaskStoreError::DispatchTokenMismatch);
                }
                if context.task.record.cancel_epoch != effect_permit.cancel_epoch {
                    return Err(TaskStoreError::CancellationCommitted {
                        cancel_epoch: context.task.record.cancel_epoch,
                    });
                }
            }
            SlotState::Dispatched => {
                if presented_digest == stored_digest {
                    return Err(TaskStoreError::DispatchTokenConsumed);
                }
                return Err(TaskStoreError::DispatchTokenMismatch);
            }
            other => {
                return Err(TaskStoreError::InvalidEffectSlotState { state: other });
            }
        }
        let control_epoch = context
            .task
            .record
            .control_epoch
            .checked_add(1)
            .ok_or(TaskStoreError::EpochExhausted)?;
        let updated = cas_slot(
            &transaction,
            &slot,
            SlotState::Dispatched,
            None,
            None,
            None,
            request.dispatched_at_ms,
        )?;
        refresh_summary(&transaction, request.permit_id, request.dispatched_at_ms)?;
        update_task(
            &transaction,
            &context.task,
            request.dispatched_at_ms,
            |record| {
                record.control_epoch = control_epoch;
            },
        )?;
        transaction.commit()?;
        Ok(updated)
    }

    /// Registers the outcome of a `Dispatched` slot: `EffectClosed` with an
    /// authoritative closure digest, or `EffectUnknown` when the caller
    /// cannot prove whether the effect happened (crash window).
    ///
    /// Both outcomes are durable; `EffectUnknown` permanently blocks permit
    /// closure until the reconcile slice (`[TASK-EFFECT-003]`). Neither is
    /// fenced by cancellation: a token consumed before a cancel must be
    /// reconciled by its real effect, never renamed
    /// (`[TASK-CANCEL-003]`). Exact replays return the original receipt;
    /// a different digest for an already-registered outcome fails closed.
    ///
    /// # Errors
    ///
    /// Returns a not-found, holder, epoch, slot-state, replay-conflict, or
    /// storage error.
    pub fn record_effect_outcome(
        &self,
        request: OutcomeRequest,
    ) -> Result<EffectReceiptDecision, TaskStoreError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let context = check_holder(
            &transaction,
            request.task_id,
            request.attempt_id,
            request.attempt_generation,
            request.permit_id,
            request.permit_epoch,
        )?;
        let slot = load_slot(&transaction, request.permit_id, request.effect_seq)?;
        let target = request.outcome.target_state();
        if slot.state != SlotState::Dispatched {
            if slot.state == target {
                let receipt_id = slot.effect_receipt_id.ok_or(TaskStoreError::CorruptRecord(
                    "closed slot lacks its effect receipt",
                ))?;
                let receipt = load_effect_receipt(&transaction, receipt_id)?;
                if receipt.proof_digest == request.outcome.proof_digest() {
                    transaction.commit()?;
                    return Ok(EffectReceiptDecision::Replayed(Box::new(receipt)));
                }
                return Err(TaskStoreError::IdempotencyConflict);
            }
            return Err(TaskStoreError::InvalidEffectSlotState { state: slot.state });
        }
        let (domain, kind) = match request.outcome {
            Outcome::Closed { .. } => (
                "llmos/task-effect-closed-receipt/v1",
                ReceiptKind::EffectClosed,
            ),
            Outcome::Unknown { .. } => (
                "llmos/task-effect-unknown-receipt/v1",
                ReceiptKind::EffectUnknown,
            ),
        };
        let receipt_id = derive_effect_receipt_id(domain, slot.effect_slot_id, slot.state_seq + 1);
        let receipt = EffectReceipt {
            receipt_id,
            task_id: request.task_id,
            permit_id: request.permit_id,
            effect_slot_id: slot.effect_slot_id,
            effect_seq: slot.effect_seq,
            logical_effect_id: slot.logical_effect_id,
            kind,
            prior_slot_state: slot.state,
            no_effect_reason: None,
            proof_digest: request.outcome.proof_digest(),
            created_at_ms: request.recorded_at_ms,
        };
        insert_effect_receipt(&transaction, &receipt)?;
        let control_epoch = context
            .task
            .record
            .control_epoch
            .checked_add(1)
            .ok_or(TaskStoreError::EpochExhausted)?;
        let updated = cas_slot(
            &transaction,
            &slot,
            target,
            None,
            None,
            Some(receipt_id),
            request.recorded_at_ms,
        )?;
        // `[TASK-EFFECT-ID-001]`: a slot closing with an effect appends
        // its `TaskEffectHistoryEntry` in the same transaction.
        if target == SlotState::EffectClosed {
            crate::reconcile::append_history_entry(
                &transaction,
                &crate::reconcile::HistoryAppend {
                    task_id: context.task.record.task_id,
                    retry_fence_epoch: context.task.record.retry_fence_epoch,
                    slot: &updated,
                    outcome: crate::EffectHistoryOutcome::EffectClosed,
                    authoritative_effect_receipt_id: receipt_id,
                    now_ms: request.recorded_at_ms,
                },
            )?;
        }
        refresh_summary(&transaction, request.permit_id, request.recorded_at_ms)?;
        update_task(
            &transaction,
            &context.task,
            request.recorded_at_ms,
            |record| {
                record.control_epoch = control_epoch;
            },
        )?;
        transaction.commit()?;
        Ok(EffectReceiptDecision::Recorded(Box::new(receipt)))
    }

    /// Closes a slot as `NoEffect` with a `TaskNoEffectReceipt`-shaped
    /// record, only when the dispatch token is verifiably unconsumed
    /// (`[TASK-EFFECT-002]`): a `Planned` slot never had a token, and a
    /// `Permitted` slot requires presenting the still-unconsumed token.
    /// A `Dispatched` slot can never take this path — a consumed token must
    /// be reconciled by its real effect (`[TASK-CANCEL-003]`).
    ///
    /// # Errors
    ///
    /// Returns a not-found, holder, epoch, token, slot-state, condition,
    /// replay-conflict, or storage error.
    pub fn record_no_effect(
        &self,
        request: NoEffectRequest,
    ) -> Result<EffectReceiptDecision, TaskStoreError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let context = check_holder(
            &transaction,
            request.task_id,
            request.attempt_id,
            request.attempt_generation,
            request.permit_id,
            request.permit_epoch,
        )?;
        let slot = load_slot(&transaction, request.permit_id, request.effect_seq)?;
        match slot.state {
            SlotState::Planned => {
                if request.dispatch_token.is_some() {
                    return Err(TaskStoreError::DispatchTokenMismatch);
                }
            }
            SlotState::Permitted => {
                let presented = request
                    .dispatch_token
                    .as_ref()
                    .ok_or(TaskStoreError::DispatchTokenMismatch)?;
                let effect_permit_id = slot.effect_permit_id.ok_or(
                    TaskStoreError::CorruptRecord("permitted slot lacks its effect permit"),
                )?;
                let stored_digest = load_dispatch_token_digest(&transaction, effect_permit_id)?;
                if dispatch_token_digest(presented) != stored_digest {
                    return Err(TaskStoreError::DispatchTokenMismatch);
                }
            }
            SlotState::NoEffect => {
                let receipt = replay_no_effect(&transaction, &slot, request.reason)?;
                transaction.commit()?;
                return Ok(EffectReceiptDecision::Replayed(Box::new(receipt)));
            }
            other => {
                return Err(TaskStoreError::InvalidEffectSlotState { state: other });
            }
        }
        if request.reason == NoEffectReason::ConditionNotApplicable
            && slot.required_condition_digest.is_none()
        {
            return Err(TaskStoreError::ConditionNotBound);
        }
        let receipt = build_no_effect_receipt(&slot, &request);
        insert_effect_receipt(&transaction, &receipt)?;
        let control_epoch = context
            .task
            .record
            .control_epoch
            .checked_add(1)
            .ok_or(TaskStoreError::EpochExhausted)?;
        cas_slot(
            &transaction,
            &slot,
            SlotState::NoEffect,
            None,
            None,
            Some(receipt.receipt_id),
            request.recorded_at_ms,
        )?;
        refresh_summary(&transaction, request.permit_id, request.recorded_at_ms)?;
        update_task(
            &transaction,
            &context.task,
            request.recorded_at_ms,
            |record| {
                record.control_epoch = control_epoch;
            },
        )?;
        transaction.commit()?;
        Ok(EffectReceiptDecision::Recorded(Box::new(receipt)))
    }

    /// Reads the durable view of one effect slot of a permit.
    ///
    /// # Errors
    ///
    /// Returns `EffectSlotNotFound` or a storage error.
    pub fn inspect_effect_slot(
        &self,
        permit_id: CommitPermitId,
        effect_seq: u64,
    ) -> Result<SlotRecord, TaskStoreError> {
        let connection = self.lock_connection()?;
        load_slot(&*connection, permit_id, effect_seq)
    }

    /// Lists all declared effect slots of a permit in `effect_seq` order.
    ///
    /// # Errors
    ///
    /// Returns a storage error.
    pub fn list_effect_slots(
        &self,
        permit_id: CommitPermitId,
    ) -> Result<Vec<SlotRecord>, TaskStoreError> {
        let connection = self.lock_connection()?;
        list_slots(&*connection, permit_id)
    }

    /// Reads the durable view of one issued `EffectPermit`.
    ///
    /// # Errors
    ///
    /// Returns `EffectPermitNotFound` or a storage error.
    pub fn inspect_effect_permit(
        &self,
        task_id: TaskId,
        effect_permit_id: EffectPermitId,
    ) -> Result<IssuedPermit, TaskStoreError> {
        let connection = self.lock_connection()?;
        load_effect_permit_by_id(&*connection, task_id, effect_permit_id)
    }

    /// Reads the per-permit effect-set control view, if the permit declared
    /// an effect set.
    ///
    /// # Errors
    ///
    /// Returns a storage error.
    pub fn inspect_effect_set(
        &self,
        permit_id: CommitPermitId,
    ) -> Result<Option<SetSummary>, TaskStoreError> {
        let connection = self.lock_connection()?;
        Ok(load_summary(&*connection, permit_id)?.map(|stored| stored.summary))
    }

    /// Reads one durable effect receipt.
    ///
    /// # Errors
    ///
    /// Returns `ReceiptNotFound` or a storage error.
    pub fn inspect_effect_receipt(
        &self,
        receipt_id: ReceiptId,
    ) -> Result<EffectReceipt, TaskStoreError> {
        let connection = self.lock_connection()?;
        load_effect_receipt(&*connection, receipt_id)
    }
}

/// Schema v2: the effect plane. Purely additive over v1 — no v1 table is
/// altered, so a v1 database migrates losslessly in one transaction.
pub(crate) const SCHEMA_V2_SQL: &str =
    "CREATE TABLE effect_slots (
            permit_id BLOB NOT NULL CHECK(length(permit_id) = 16),
            effect_seq BLOB NOT NULL CHECK(length(effect_seq) = 8),
            task_id BLOB NOT NULL CHECK(length(task_id) = 16),
            effect_slot_id BLOB NOT NULL CHECK(length(effect_slot_id) = 16),
            logical_effect_id BLOB NOT NULL CHECK(length(logical_effect_id) = 32),
            idempotency_identity_digest BLOB NOT NULL CHECK(length(idempotency_identity_digest) = 32),
            required INTEGER NOT NULL,
            required_condition_digest BLOB CHECK(required_condition_digest IS NULL OR length(required_condition_digest) = 32),
            success_criteria_digest BLOB NOT NULL CHECK(length(success_criteria_digest) = 32),
            action_proposal_digest BLOB NOT NULL CHECK(length(action_proposal_digest) = 32),
            slot_state INTEGER NOT NULL,
            state_seq INTEGER NOT NULL,
            effect_permit_id BLOB CHECK(effect_permit_id IS NULL OR length(effect_permit_id) = 16),
            dispatch_token_digest BLOB CHECK(dispatch_token_digest IS NULL OR length(dispatch_token_digest) = 32),
            effect_receipt_id BLOB CHECK(effect_receipt_id IS NULL OR length(effect_receipt_id) = 16),
            created_at_ms INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL,
            PRIMARY KEY(permit_id, effect_seq),
            UNIQUE(permit_id, logical_effect_id),
            FOREIGN KEY(permit_id) REFERENCES commit_permits(permit_id),
            FOREIGN KEY(task_id) REFERENCES tasks(task_id)
        ) STRICT;

        CREATE TABLE permit_effect_sets (
            permit_id BLOB PRIMARY KEY NOT NULL CHECK(length(permit_id) = 16),
            task_id BLOB NOT NULL CHECK(length(task_id) = 16),
            effect_set_root BLOB NOT NULL CHECK(length(effect_set_root) = 32),
            effect_slot_state_root BLOB NOT NULL CHECK(length(effect_slot_state_root) = 32),
            required_effect_count INTEGER NOT NULL,
            satisfied_required_effect_count INTEGER NOT NULL,
            terminal_effect_count INTEGER NOT NULL,
            issued_effect_root BLOB NOT NULL CHECK(length(issued_effect_root) = 32),
            dispatched_effect_root BLOB NOT NULL CHECK(length(dispatched_effect_root) = 32),
            closed_effect_root BLOB NOT NULL CHECK(length(closed_effect_root) = 32),
            outstanding_effect_root BLOB NOT NULL CHECK(length(outstanding_effect_root) = 32),
            revision INTEGER NOT NULL DEFAULT 0,
            created_at_ms INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL,
            FOREIGN KEY(permit_id) REFERENCES commit_permits(permit_id)
        ) STRICT;

        CREATE TABLE effect_permits (
            effect_permit_id BLOB PRIMARY KEY NOT NULL CHECK(length(effect_permit_id) = 16),
            task_id BLOB NOT NULL CHECK(length(task_id) = 16),
            idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
            permit_id BLOB NOT NULL CHECK(length(permit_id) = 16),
            permit_epoch BLOB NOT NULL CHECK(length(permit_epoch) = 8),
            attempt_id BLOB NOT NULL CHECK(length(attempt_id) = 16),
            attempt_generation BLOB NOT NULL CHECK(length(attempt_generation) = 8),
            effect_slot_id BLOB NOT NULL CHECK(length(effect_slot_id) = 16),
            effect_seq BLOB NOT NULL CHECK(length(effect_seq) = 8),
            logical_effect_id BLOB NOT NULL CHECK(length(logical_effect_id) = 32),
            retry_fence_epoch BLOB NOT NULL CHECK(length(retry_fence_epoch) = 8),
            idempotency_identity_digest BLOB NOT NULL CHECK(length(idempotency_identity_digest) = 32),
            effect_set_root BLOB NOT NULL CHECK(length(effect_set_root) = 32),
            action_proposal_digest BLOB NOT NULL CHECK(length(action_proposal_digest) = 32),
            control_epoch BLOB NOT NULL CHECK(length(control_epoch) = 8),
            cancel_epoch BLOB NOT NULL CHECK(length(cancel_epoch) = 8),
            dispatch_token_digest BLOB NOT NULL CHECK(length(dispatch_token_digest) = 32),
            valid_until_ms INTEGER NOT NULL,
            created_at_ms INTEGER NOT NULL,
            UNIQUE(task_id, idempotency_key),
            FOREIGN KEY(task_id) REFERENCES tasks(task_id),
            FOREIGN KEY(permit_id) REFERENCES commit_permits(permit_id)
        ) STRICT;

        CREATE TABLE effect_receipts (
            receipt_id BLOB PRIMARY KEY NOT NULL CHECK(length(receipt_id) = 16),
            task_id BLOB NOT NULL CHECK(length(task_id) = 16),
            permit_id BLOB NOT NULL CHECK(length(permit_id) = 16),
            effect_slot_id BLOB NOT NULL CHECK(length(effect_slot_id) = 16),
            effect_seq BLOB NOT NULL CHECK(length(effect_seq) = 8),
            logical_effect_id BLOB NOT NULL CHECK(length(logical_effect_id) = 32),
            receipt_kind INTEGER NOT NULL,
            prior_slot_state INTEGER NOT NULL,
            no_effect_reason INTEGER,
            proof_digest BLOB NOT NULL CHECK(length(proof_digest) = 32),
            created_at_ms INTEGER NOT NULL,
            FOREIGN KEY(task_id) REFERENCES tasks(task_id)
        ) STRICT;

        CREATE INDEX effect_receipts_by_slot
            ON effect_receipts(permit_id, effect_seq);

        CREATE TRIGGER effect_receipt_is_immutable
        BEFORE UPDATE ON effect_receipts
        BEGIN
            SELECT RAISE(ABORT, 'effect receipt is immutable');
        END;

        PRAGMA user_version = 2;";

/// Schema v3: cross-attempt effect history, quarantine/adoption/reconcile
/// receipts, and per-task monotonic sequences. Purely additive over v2 —
/// no v2 table is altered, so a v2 database migrates losslessly in one
/// transaction.
pub(crate) const SCHEMA_V3_SQL: &str =
    "CREATE TABLE effect_history (
            task_id BLOB NOT NULL CHECK(length(task_id) = 16),
            effect_history_seq BLOB NOT NULL CHECK(length(effect_history_seq) = 8),
            logical_effect_id BLOB NOT NULL CHECK(length(logical_effect_id) = 32),
            retry_fence_epoch BLOB NOT NULL CHECK(length(retry_fence_epoch) = 8),
            action_proposal_digest BLOB NOT NULL CHECK(length(action_proposal_digest) = 32),
            idempotency_identity_digest BLOB NOT NULL CHECK(length(idempotency_identity_digest) = 32),
            operation_id BLOB CHECK(operation_id IS NULL OR length(operation_id) = 16),
            outcome INTEGER NOT NULL,
            authoritative_effect_receipt_id BLOB NOT NULL CHECK(length(authoritative_effect_receipt_id) = 16),
            compensation_receipt_id BLOB CHECK(compensation_receipt_id IS NULL OR length(compensation_receipt_id) = 16),
            created_at_ms INTEGER NOT NULL,
            PRIMARY KEY(task_id, effect_history_seq),
            FOREIGN KEY(task_id) REFERENCES tasks(task_id)
        ) STRICT;

        CREATE INDEX effect_history_by_logical
            ON effect_history(task_id, logical_effect_id);

        CREATE TRIGGER effect_history_is_immutable
        BEFORE UPDATE ON effect_history
        BEGIN
            SELECT RAISE(ABORT, 'effect history entry is immutable');
        END;

        CREATE TABLE task_quarantine_receipts (
            receipt_id BLOB PRIMARY KEY NOT NULL CHECK(length(receipt_id) = 16),
            task_id BLOB NOT NULL CHECK(length(task_id) = 16),
            task_generation BLOB NOT NULL CHECK(length(task_generation) = 8),
            permit_id BLOB NOT NULL CHECK(length(permit_id) = 16),
            permit_epoch BLOB NOT NULL CHECK(length(permit_epoch) = 8),
            effect_set_root BLOB NOT NULL CHECK(length(effect_set_root) = 32),
            outstanding_effect_quarantine_root BLOB NOT NULL CHECK(length(outstanding_effect_quarantine_root) = 32),
            conflicting_target_digest BLOB NOT NULL CHECK(length(conflicting_target_digest) = 32),
            known_effect_receipts BLOB NOT NULL,
            unknown_slots BLOB NOT NULL,
            fenced_participant_digest BLOB NOT NULL CHECK(length(fenced_participant_digest) = 32),
            created_at_ms INTEGER NOT NULL,
            FOREIGN KEY(task_id) REFERENCES tasks(task_id)
        ) STRICT;

        CREATE TRIGGER task_quarantine_receipt_is_immutable
        BEFORE UPDATE ON task_quarantine_receipts
        BEGIN
            SELECT RAISE(ABORT, 'quarantine receipt is immutable');
        END;

        CREATE TABLE task_adoption_receipts (
            receipt_id BLOB PRIMARY KEY NOT NULL CHECK(length(receipt_id) = 16),
            task_id BLOB NOT NULL CHECK(length(task_id) = 16),
            task_generation BLOB NOT NULL CHECK(length(task_generation) = 8),
            idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
            original_permit_id BLOB NOT NULL CHECK(length(original_permit_id) = 16),
            original_permit_epoch BLOB NOT NULL CHECK(length(original_permit_epoch) = 8),
            original_control_epoch BLOB NOT NULL CHECK(length(original_control_epoch) = 8),
            original_cancel_epoch BLOB NOT NULL CHECK(length(original_cancel_epoch) = 8),
            effect_set_root BLOB NOT NULL CHECK(length(effect_set_root) = 32),
            observed_effect_slot_state_root BLOB NOT NULL CHECK(length(observed_effect_slot_state_root) = 32),
            adoption_epoch BLOB NOT NULL CHECK(length(adoption_epoch) = 8),
            created_at_ms INTEGER NOT NULL,
            UNIQUE(task_id, idempotency_key),
            FOREIGN KEY(task_id) REFERENCES tasks(task_id)
        ) STRICT;

        CREATE INDEX task_adoption_receipts_by_permit
            ON task_adoption_receipts(task_id, original_permit_id);

        CREATE TRIGGER task_adoption_receipt_is_immutable
        BEFORE UPDATE ON task_adoption_receipts
        BEGIN
            SELECT RAISE(ABORT, 'adoption receipt is immutable');
        END;

        CREATE TABLE task_reconcile_receipts (
            receipt_id BLOB PRIMARY KEY NOT NULL CHECK(length(receipt_id) = 16),
            task_id BLOB NOT NULL CHECK(length(task_id) = 16),
            permit_id BLOB NOT NULL CHECK(length(permit_id) = 16),
            permit_epoch BLOB NOT NULL CHECK(length(permit_epoch) = 8),
            permit_adoption_receipt_id BLOB NOT NULL CHECK(length(permit_adoption_receipt_id) = 16),
            effect_slot_id BLOB NOT NULL CHECK(length(effect_slot_id) = 16),
            effect_seq BLOB NOT NULL CHECK(length(effect_seq) = 8),
            logical_effect_id BLOB NOT NULL CHECK(length(logical_effect_id) = 32),
            retry_fence_epoch BLOB NOT NULL CHECK(length(retry_fence_epoch) = 8),
            effect_set_root BLOB NOT NULL CHECK(length(effect_set_root) = 32),
            outcome INTEGER NOT NULL,
            closure_proof_digest BLOB NOT NULL CHECK(length(closure_proof_digest) = 32),
            effect_receipt_id BLOB CHECK(effect_receipt_id IS NULL OR length(effect_receipt_id) = 16),
            effect_slot_state_root_after BLOB NOT NULL CHECK(length(effect_slot_state_root_after) = 32),
            created_at_ms INTEGER NOT NULL,
            FOREIGN KEY(task_id) REFERENCES tasks(task_id)
        ) STRICT;

        CREATE INDEX task_reconcile_receipts_by_slot
            ON task_reconcile_receipts(permit_id, effect_seq);

        CREATE TRIGGER task_reconcile_receipt_is_immutable
        BEFORE UPDATE ON task_reconcile_receipts
        BEGIN
            SELECT RAISE(ABORT, 'reconcile receipt is immutable');
        END;

        CREATE TABLE task_effect_sequences (
            task_id BLOB PRIMARY KEY NOT NULL CHECK(length(task_id) = 16),
            effect_history_seq BLOB NOT NULL CHECK(length(effect_history_seq) = 8),
            adoption_epoch BLOB NOT NULL CHECK(length(adoption_epoch) = 8),
            FOREIGN KEY(task_id) REFERENCES tasks(task_id)
        ) STRICT;

        CREATE TABLE task_finalize_proofs (
            receipt_id BLOB PRIMARY KEY NOT NULL CHECK(length(receipt_id) = 16),
            proof_digest BLOB NOT NULL CHECK(length(proof_digest) = 32),
            FOREIGN KEY(receipt_id) REFERENCES task_receipts(receipt_id)
        ) STRICT;

        PRAGMA user_version = 3;";
