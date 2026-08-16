//! Schema v3: cross-attempt effect history (`[TASK-EFFECT-ID-001]` /
//! `[TASK-RETRY-EFFECT-001]`), the `EFFECT_UNKNOWN` quarantine /
//! adoption / reconcile lifecycle (`[TASK-EFFECT-003]` /
//! `[TASK-COMMIT-003]`), and the full required-slot success semantics of
//! `[TASK-COMMIT-002]`.
//!
//! Identity and digest formulas are domain-separated SHA-256 placeholders
//! fixing the deterministic shape required by the spec; canonical
//! deterministic-CBOR and signatures remain out of scope. Single
//! authority only: adoption is by the same authority after
//! restart/uncertainty, never a cross-term takeover.

use nlos_types::{CommitPermitId, IdempotencyKey, ProcessId, ReceiptId, TaskId, TaskParticipantId};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use crate::effect::{
    self, SlotRecord, SlotState, insert_effect_receipt, list_slots, load_slot, refresh_summary,
};
use crate::lease::{AuthorityLeaseBinding, validate_authority_lease_binding_in_transaction};
use crate::model::{
    self, ClosePermitRequest, EffectHistoryEntry, EffectHistoryLookup, EffectHistoryOutcome,
    FinalizeRequest, QuarantineReceiptRecord, ReceiptOutcome, ReconcileOutcome,
    RequiredSatisfactionProof,
};
use crate::store::{
    self, SqlRead, SqliteTaskAuthority, StoredTask, blob16, blob32, close_permit, encode_u64,
    generation_from_blob, insert_receipt, load_receipt_by_permit, optional_blob, optional_blob16,
    set_attempt_state, u64_from_blob, update_task,
};
use crate::{
    AdoptionReceiptRecord, AttemptState, AuthorityLeaseRecord, ClosePermitDecision,
    FinalizeDecision, PermitRecord, PermitState, ReconciliationReceiptRecord, SemanticCommitPlanId,
    SemanticFinalizeDecision, SemanticTaskCommitReceipt, TaskReceiptRecord, TaskStoreError,
};

/// A `PermitAdoptionReceipt` issuance request (`[TASK-COMMIT-003]`,
/// single-authority subset).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdoptionRequest {
    pub task_id: TaskId,
    pub permit_id: CommitPermitId,
    pub permit_epoch: u64,
    pub idempotency_key: IdempotencyKey,
    pub adopted_at_ms: i64,
}

/// Linearized decision of an adoption request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AdoptionReplay {
    Adopted(Box<AdoptionReceiptRecord>),
    /// Same idempotency key and same bytes: the original record.
    Replayed(Box<AdoptionReceiptRecord>),
}

/// One reconcile step on an `EffectUnknown` slot (`[TASK-EFFECT-003]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReconcileRequest {
    pub task_id: TaskId,
    pub permit_id: CommitPermitId,
    pub permit_epoch: u64,
    pub effect_seq: u64,
    /// The durable adoption this reconcile runs under.
    pub adoption_receipt_id: ReceiptId,
    pub outcome: ReconcileOutcome,
    /// Caller-supplied digest placeholder for the gateway/provider
    /// authoritative closure proof.
    pub closure_proof_digest: [u8; 32],
    pub reconciled_at_ms: i64,
}

/// Linearized decision of a reconcile request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ReconcileReplay {
    Reconciled(Box<ReconciliationReceiptRecord>),
    /// Same slot, same adoption, same outcome, same proof: the original
    /// receipt (no double reconcile).
    Replayed(Box<ReconciliationReceiptRecord>),
}

/// Full `[TASK-COMMIT-002]` finalize request (schema v3).
///
/// `base` keeps the B-TASK-001/002 request shape. For a permit with a
/// declared effect set the base root/fence fields are ignored — the
/// authority computes the post-commit history root and retry-fence epoch
/// from the durable history itself — while for a permit with no declared
/// effect set (all B-TASK-001 flows) the legacy caller-supplied roots
/// remain authoritative.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FinalizeRequestV3 {
    pub base: FinalizeRequest,
    /// Per-required-slot success proofs (`[TASK-COMMIT-002]`). Only
    /// required slots may be covered; duplicates or proofs for
    /// non-required/unknown slots fail closed.
    pub required_satisfaction: Vec<crate::RequiredSatisfaction>,
    /// Caller-supplied participant-fence proof placeholder, persisted on
    /// the quarantine receipt if any slot is `EffectUnknown`.
    pub fenced_participant_digest: [u8; 32],
}

/// Opt-in v3 finalization request carrying the same durable authority lease
/// that was copied into the permit at issuance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorityLeaseFinalizeRequest {
    pub finalize: FinalizeRequestV3,
    pub lease: AuthorityLeaseRecord,
}

/// Opt-in pre-effect closure request carrying the lease copied into the
/// permit at issuance.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuthorityLeaseCloseRequest {
    pub close: ClosePermitRequest,
    pub lease: AuthorityLeaseRecord,
}

/// Opt-in adoption request carrying the live lease copied into the
/// quarantined permit. The resulting adoption receipt stores the exact
/// binding; cross-term takeover proof remains a separate future gate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorityLeaseAdoptionRequest {
    pub adoption: AdoptionRequest,
    pub lease: AuthorityLeaseRecord,
}

/// Opt-in reconcile request carrying the live lease bound into the adoption
/// receipt. Reconcile replay remains readable without presenting the lease.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuthorityLeaseReconcileRequest {
    pub reconcile: ReconcileRequest,
    pub lease: AuthorityLeaseRecord,
}

/// `SHA-256("llmos/task-effect-history/v1" || canonical(entries by seq))`.
///
/// The fixed-width placeholder encoding per entry: `seq(8) ||
/// logical_effect_id(32) || retry_fence_epoch(8) ||
/// action_proposal_digest(32) || idempotency_identity_digest(32) ||
/// operation_id flag+bytes(1|17) || outcome(1) ||
/// authoritative_effect_receipt_id(16) || compensation flag+bytes(1|17)`.
/// The empty entry list hashes to exactly
/// [`crate::empty_effect_history_root`], keeping the B-TASK-001 initial
/// head bit-compatible.
#[must_use]
pub fn effect_history_root_of(entries: &[EffectHistoryEntry]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/task-effect-history/v1");
    if entries.is_empty() {
        hasher.update([0x80u8]);
    }
    for entry in entries {
        hasher.update(entry.effect_history_seq.to_be_bytes());
        hasher.update(entry.logical_effect_id);
        hasher.update(entry.retry_fence_epoch.to_be_bytes());
        hasher.update(entry.action_proposal_digest);
        hasher.update(entry.idempotency_identity_digest);
        match entry.operation_id {
            Some(operation) => {
                hasher.update([1u8]);
                hasher.update(operation);
            }
            None => hasher.update([0u8]),
        }
        hasher.update([u8::try_from(entry.outcome.code()).unwrap_or(u8::MAX)]);
        hasher.update(entry.authoritative_effect_receipt_id.as_bytes());
        match entry.compensation_receipt_id {
            Some(compensation) => {
                hasher.update([1u8]);
                hasher.update(compensation.as_bytes());
            }
            None => hasher.update([0u8]),
        }
    }
    hasher.finalize().into()
}

fn sha256(domain: &str, parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(domain.as_bytes());
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

/// Re-reads every Semantic append proof from its owning authority immediately
/// before an issued permit is finalized. This is deliberately a guard only:
/// it does not acknowledge the Semantic outbox, create a checkpoint, or
/// manufacture a publication receipt (`[TASK-WRITE-003]`).
fn validate_semantic_finalization(
    semantic_authority: &nlos_semantic::SemanticAuthority,
    record: &crate::TaskWriteSetRecord,
) -> Result<(), TaskStoreError> {
    for append in &record.semantic_appends {
        let event = semantic_authority
            .inspect_event(append.event_id)
            .map_err(TaskStoreError::SemanticParticipantAuthority)?;
        if event.scope_kind() != append.target.kind() || event.scope_id() != append.target.id() {
            return Err(TaskStoreError::TaskWriteSetConflict {
                reason: "Semantic append finalization target scope differs from admitted event",
            });
        }
        let admission = semantic_authority
            .inspect_admission_receipt(append.event_id)
            .map_err(TaskStoreError::SemanticParticipantAuthority)?;
        if admission.receipt_id != append.admission_receipt_id
            || admission.event_id != append.event_id
            || admission.log_seq != event.log_seq
            || !matches!(
                (append.required_durability, admission.durability),
                (
                    crate::TaskWriteSetSemanticRequiredDurability::Durable,
                    nlos_semantic::AdmissionDurability::Durable
                )
            )
        {
            return Err(TaskStoreError::TaskWriteSetConflict {
                reason: "Semantic append finalization receipt differs from sealed owner proof",
            });
        }
        if append
            .admission_policy_digest
            .is_some_and(|digest| digest != admission.authz_policy_digest)
        {
            return Err(TaskStoreError::TaskWriteSetConflict {
                reason: "Semantic finalization admission policy differs from sealed owner proof",
            });
        }
        if let Some(durability_receipt_id) = append.durability_receipt_id {
            let durability = semantic_authority
                .inspect_durability_receipt(append.event_id, durability_receipt_id)
                .map_err(TaskStoreError::SemanticParticipantAuthority)?;
            if durability.receipt_id != durability_receipt_id
                || durability.event_id != append.event_id
            {
                return Err(TaskStoreError::TaskWriteSetConflict {
                    reason: "Semantic finalization durability receipt differs from sealed owner proof",
                });
            }
        }
    }
    Ok(())
}

/// Whether the logical effect is durably `EFFECT_CLOSED` in the
/// cross-attempt history (`[TASK-RETRY-EFFECT-001]` re-dispatch fence).
pub(crate) fn is_effect_closed_in_history(
    source: &impl SqlRead,
    task_id: TaskId,
    logical_effect_id: &[u8; 32],
) -> Result<bool, TaskStoreError> {
    let mut statement = source.prepare_statement(
        "SELECT COUNT(*) FROM effect_history
         WHERE task_id = ?1 AND logical_effect_id = ?2 AND outcome = ?3",
    )?;
    let count: i64 = statement.query_row(
        params![
            task_id.as_bytes().as_slice(),
            logical_effect_id.as_slice(),
            EffectHistoryOutcome::EffectClosed.code(),
        ],
        |row| row.get(0),
    )?;
    Ok(count > 0)
}

/// Fail-closed guard for `EffectPermit` issuance
/// (`[TASK-RETRY-EFFECT-001]`): a logical effect already `EFFECT_CLOSED`
/// in the durable history must never be silently re-dispatched.
pub(crate) fn check_not_closed_in_history(
    source: &impl SqlRead,
    task_id: TaskId,
    logical_effect_id: &[u8; 32],
) -> Result<(), TaskStoreError> {
    if is_effect_closed_in_history(source, task_id, logical_effect_id)? {
        return Err(TaskStoreError::EffectAlreadyClosed);
    }
    Ok(())
}

/// Whether any adoption receipt binds this permit (`[TASK-COMMIT-003]`
/// scope fence for new EffectPermits/dispatches).
pub(crate) fn has_adoption(
    source: &impl SqlRead,
    permit_id: CommitPermitId,
) -> Result<bool, TaskStoreError> {
    let mut statement = source.prepare_statement(
        "SELECT COUNT(*) FROM task_adoption_receipts WHERE original_permit_id = ?1",
    )?;
    let count: i64 = statement.query_row([permit_id.as_bytes().as_slice()], |row| row.get(0))?;
    Ok(count > 0)
}

const HISTORY_COLUMNS: &str = "task_id, effect_history_seq, logical_effect_id,
     retry_fence_epoch, action_proposal_digest, idempotency_identity_digest,
     operation_id, outcome, authoritative_effect_receipt_id,
     compensation_receipt_id, created_at_ms";

fn decode_history_row(row: &rusqlite::Row<'_>) -> Result<EffectHistoryEntry, TaskStoreError> {
    Ok(EffectHistoryEntry {
        task_id: TaskId::from_bytes(blob16(row, 0)?),
        effect_history_seq: u64_from_blob(row, 1)?,
        logical_effect_id: blob32(row, 2)?,
        retry_fence_epoch: u64_from_blob(row, 3)?,
        action_proposal_digest: blob32(row, 4)?,
        idempotency_identity_digest: blob32(row, 5)?,
        operation_id: optional_blob16(row, 6)?,
        outcome: EffectHistoryOutcome::from_code(row.get(7)?)?,
        authoritative_effect_receipt_id: ReceiptId::from_bytes(blob16(row, 8)?),
        compensation_receipt_id: optional_blob16(row, 9)?.map(ReceiptId::from_bytes),
        created_at_ms: row.get(10)?,
    })
}

fn list_history(
    source: &impl SqlRead,
    task_id: TaskId,
) -> Result<Vec<EffectHistoryEntry>, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {HISTORY_COLUMNS} FROM effect_history
         WHERE task_id = ?1 ORDER BY effect_history_seq"
    ))?;
    let mut rows = statement.query([task_id.as_bytes().as_slice()])?;
    let mut entries = Vec::new();
    while let Some(row) = rows.next()? {
        entries.push(decode_history_row(row)?);
    }
    Ok(entries)
}

fn history_root(source: &impl SqlRead, task_id: TaskId) -> Result<[u8; 32], TaskStoreError> {
    Ok(effect_history_root_of(&list_history(source, task_id)?))
}

/// Monotonic per-task sequence advance under CAS. The single-writer gate
/// makes a lost race impossible; a failed CAS is corruption (fail closed).
fn advance_sequence(
    transaction: &Transaction<'_>,
    task_id: TaskId,
    column: &str,
) -> Result<u64, TaskStoreError> {
    let current: Option<Vec<u8>> = transaction
        .query_row(
            &format!("SELECT {column} FROM task_effect_sequences WHERE task_id = ?1"),
            [task_id.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .optional()?;
    let next = match &current {
        None => 1u64,
        Some(blob) => {
            let bytes: [u8; 8] = blob
                .as_slice()
                .try_into()
                .map_err(|_| TaskStoreError::CorruptRecord("expected 8-byte sequence"))?;
            u64::from_be_bytes(bytes)
                .checked_add(1)
                .ok_or(TaskStoreError::EpochExhausted)?
        }
    };
    let changed = match &current {
        None => transaction.execute(
            "INSERT INTO task_effect_sequences (task_id, effect_history_seq, adoption_epoch)
             VALUES (?1, ?2, ?3)",
            params![
                task_id.as_bytes().as_slice(),
                encode_u64(if column == "effect_history_seq" {
                    next
                } else {
                    0
                })
                .as_slice(),
                encode_u64(if column == "adoption_epoch" { next } else { 0 }).as_slice(),
            ],
        )?,
        Some(blob) => transaction.execute(
            &format!(
                "UPDATE task_effect_sequences SET {column} = ?2
                 WHERE task_id = ?1 AND {column} = ?3"
            ),
            params![
                task_id.as_bytes().as_slice(),
                encode_u64(next).as_slice(),
                blob.as_slice(),
            ],
        )?,
    };
    if changed != 1 {
        return Err(TaskStoreError::CorruptRecord(
            "task effect sequence compare-and-swap failed",
        ));
    }
    Ok(next)
}

/// Input bundle for one history append (kept under the argument-count
/// lint without smuggling a throwaway config object).
pub(crate) struct HistoryAppend<'a> {
    pub(crate) task_id: TaskId,
    pub(crate) retry_fence_epoch: u64,
    pub(crate) slot: &'a SlotRecord,
    pub(crate) outcome: EffectHistoryOutcome,
    pub(crate) authoritative_effect_receipt_id: ReceiptId,
    pub(crate) now_ms: i64,
}

/// Appends one `TaskEffectHistoryEntry` in the caller's transaction
/// (`[TASK-EFFECT-ID-001]`): the sequence is strictly increasing from 1
/// with no gaps per task.
pub(crate) fn append_history_entry(
    transaction: &Transaction<'_>,
    append: &HistoryAppend<'_>,
) -> Result<EffectHistoryEntry, TaskStoreError> {
    let effect_history_seq = advance_sequence(transaction, append.task_id, "effect_history_seq")?;
    let entry = EffectHistoryEntry {
        effect_history_seq,
        task_id: append.task_id,
        logical_effect_id: append.slot.logical_effect_id,
        retry_fence_epoch: append.retry_fence_epoch,
        action_proposal_digest: append.slot.action_proposal_digest,
        idempotency_identity_digest: append.slot.idempotency_identity_digest,
        operation_id: None,
        outcome: append.outcome,
        authoritative_effect_receipt_id: append.authoritative_effect_receipt_id,
        compensation_receipt_id: None,
        created_at_ms: append.now_ms,
    };
    insert_history_entry(transaction, &entry)?;
    Ok(entry)
}

fn insert_history_entry(
    transaction: &Transaction<'_>,
    entry: &EffectHistoryEntry,
) -> Result<(), TaskStoreError> {
    transaction.execute(
        "INSERT INTO effect_history (
            task_id, effect_history_seq, logical_effect_id, retry_fence_epoch,
            action_proposal_digest, idempotency_identity_digest, operation_id,
            outcome, authoritative_effect_receipt_id, compensation_receipt_id,
            created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            entry.task_id.as_bytes().as_slice(),
            encode_u64(entry.effect_history_seq).as_slice(),
            entry.logical_effect_id.as_slice(),
            encode_u64(entry.retry_fence_epoch).as_slice(),
            entry.action_proposal_digest.as_slice(),
            entry.idempotency_identity_digest.as_slice(),
            entry.operation_id.as_ref().map(<[u8; 16]>::as_slice),
            entry.outcome.code(),
            entry.authoritative_effect_receipt_id.as_bytes().as_slice(),
            entry
                .compensation_receipt_id
                .map(ReceiptId::into_bytes)
                .as_ref()
                .map(<[u8; 16]>::as_slice),
            entry.created_at_ms,
        ],
    )?;
    Ok(())
}

const QUARANTINE_COLUMNS: &str = "receipt_id, task_id, task_generation, permit_id,
     permit_epoch, effect_set_root, outstanding_effect_quarantine_root,
     conflicting_target_digest, known_effect_receipts, unknown_slots,
     fenced_participant_digest, created_at_ms";

fn decode_quarantine_row(
    row: &rusqlite::Row<'_>,
) -> Result<QuarantineReceiptRecord, TaskStoreError> {
    let known_blob: Vec<u8> = row.get(8)?;
    if !known_blob.len().is_multiple_of(16) {
        return Err(TaskStoreError::CorruptRecord(
            "quarantine known receipts blob is not 16-byte aligned",
        ));
    }
    let unknown_blob: Vec<u8> = row.get(9)?;
    if !unknown_blob.len().is_multiple_of(8) {
        return Err(TaskStoreError::CorruptRecord(
            "quarantine unknown slots blob is not 8-byte aligned",
        ));
    }
    let mut known_effect_receipts = Vec::with_capacity(known_blob.len() / 16);
    for chunk in known_blob.chunks_exact(16) {
        let mut bytes = [0u8; 16];
        bytes.copy_from_slice(chunk);
        known_effect_receipts.push(ReceiptId::from_bytes(bytes));
    }
    let mut unknown_slots = Vec::with_capacity(unknown_blob.len() / 8);
    for chunk in unknown_blob.chunks_exact(8) {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(chunk);
        unknown_slots.push(u64::from_be_bytes(bytes));
    }
    Ok(QuarantineReceiptRecord {
        receipt_id: ReceiptId::from_bytes(blob16(row, 0)?),
        task_id: TaskId::from_bytes(blob16(row, 1)?),
        task_generation: generation_from_blob(row, 2)?,
        permit_id: CommitPermitId::from_bytes(blob16(row, 3)?),
        permit_epoch: u64_from_blob(row, 4)?,
        effect_set_root: blob32(row, 5)?,
        outstanding_effect_quarantine_root: blob32(row, 6)?,
        conflicting_target_digest: blob32(row, 7)?,
        known_effect_receipts,
        unknown_slots,
        fenced_participant_digest: blob32(row, 10)?,
        created_at_ms: row.get(11)?,
    })
}

fn load_quarantine_by_permit(
    source: &impl SqlRead,
    permit_id: CommitPermitId,
) -> Result<Option<QuarantineReceiptRecord>, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {QUARANTINE_COLUMNS} FROM task_quarantine_receipts WHERE permit_id = ?1"
    ))?;
    let mut rows = statement.query([permit_id.as_bytes().as_slice()])?;
    rows.next()?.map(decode_quarantine_row).transpose()
}

fn insert_quarantine(
    transaction: &Transaction<'_>,
    record: &QuarantineReceiptRecord,
) -> Result<(), TaskStoreError> {
    let known_blob: Vec<u8> = record
        .known_effect_receipts
        .iter()
        .flat_map(|id| id.into_bytes())
        .collect();
    let unknown_blob: Vec<u8> = record
        .unknown_slots
        .iter()
        .flat_map(|seq| seq.to_be_bytes())
        .collect();
    transaction.execute(
        "INSERT INTO task_quarantine_receipts (
            receipt_id, task_id, task_generation, permit_id, permit_epoch,
            effect_set_root, outstanding_effect_quarantine_root,
            conflicting_target_digest, known_effect_receipts, unknown_slots,
            fenced_participant_digest, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            record.receipt_id.as_bytes().as_slice(),
            record.task_id.as_bytes().as_slice(),
            encode_u64(record.task_generation.get()).as_slice(),
            record.permit_id.as_bytes().as_slice(),
            encode_u64(record.permit_epoch).as_slice(),
            record.effect_set_root.as_slice(),
            record.outstanding_effect_quarantine_root.as_slice(),
            record.conflicting_target_digest.as_slice(),
            known_blob,
            unknown_blob,
            record.fenced_participant_digest.as_slice(),
            record.created_at_ms,
        ],
    )?;
    Ok(())
}

/// Builds and durably registers the quarantine tombstone, or replays it
/// when the same closure request produced it earlier
/// (`[TASK-EFFECT-003]`). The permit CAS `Issued → Quarantined` happens
/// here; the `TaskHead` is untouched.
fn quarantine_permit(
    transaction: &Transaction<'_>,
    task: &StoredTask,
    permit: &PermitRecord,
    fenced_participant_digest: [u8; 32],
    now_ms: i64,
) -> Result<QuarantineReceiptRecord, TaskStoreError> {
    if let Some(existing) = load_quarantine_by_permit(transaction, permit.permit_id)? {
        if existing.fenced_participant_digest == fenced_participant_digest {
            return Ok(existing);
        }
        return Err(TaskStoreError::HistoryConflict);
    }
    let slots = list_slots(transaction, permit.permit_id)?;
    let unknown: Vec<&SlotRecord> = slots
        .iter()
        .filter(|slot| slot.state == SlotState::EffectUnknown)
        .collect();
    let first_unknown = unknown
        .first()
        .ok_or(TaskStoreError::InvalidReconcileState {
            reason: "quarantine requires at least one EFFECT_UNKNOWN slot",
        })?;
    let mut root_hasher = Sha256::new();
    root_hasher.update(b"llmos/task-effect-quarantine-outstanding/v1");
    for slot in &unknown {
        root_hasher.update(slot.effect_seq.to_be_bytes());
        root_hasher.update(slot.logical_effect_id);
    }
    let outstanding_effect_quarantine_root: [u8; 32] = root_hasher.finalize().into();
    let record = QuarantineReceiptRecord {
        receipt_id: model::derive_quarantine_receipt_id(permit.permit_id),
        task_id: task.record.task_id,
        task_generation: task.record.task_generation,
        permit_id: permit.permit_id,
        permit_epoch: permit.permit_epoch,
        effect_set_root: effect::stored_effect_set_root(transaction, permit.permit_id)?
            .unwrap_or_else(effect::empty_effect_set_root),
        outstanding_effect_quarantine_root,
        conflicting_target_digest: sha256(
            "llmos/task-quarantine-target/v1",
            &[
                first_unknown.effect_slot_id.as_bytes(),
                &first_unknown.logical_effect_id,
            ],
        ),
        known_effect_receipts: slots
            .iter()
            .filter_map(|slot| slot.effect_receipt_id)
            .collect(),
        unknown_slots: unknown.iter().map(|slot| slot.effect_seq).collect(),
        fenced_participant_digest,
        created_at_ms: now_ms,
    };
    insert_quarantine(transaction, &record)?;
    let changed = transaction.execute(
        "UPDATE commit_permits SET permit_state = ?1, updated_at_ms = ?2
         WHERE permit_id = ?3 AND permit_state = ?4",
        params![
            PermitState::Quarantined.code(),
            now_ms,
            permit.permit_id.as_bytes().as_slice(),
            PermitState::Issued.code(),
        ],
    )?;
    if changed != 1 {
        return Err(TaskStoreError::CorruptRecord(
            "permit quarantine compare-and-swap failed",
        ));
    }
    Ok(record)
}

const ADOPTION_COLUMNS: &str = "receipt_id, task_id, task_generation, idempotency_key,
     original_permit_id, original_permit_epoch, original_control_epoch,
     original_cancel_epoch, effect_set_root, observed_effect_slot_state_root,
     adoption_epoch, created_at_ms, authority_lease_authority_id,
     authority_lease_holder_id, authority_lease_term, authority_lease_epoch,
     authority_lease_fencing_token, authority_lease_expires_at_ms";

fn decode_adoption_row(row: &rusqlite::Row<'_>) -> Result<AdoptionReceiptRecord, TaskStoreError> {
    let authority_lease_authority_id = optional_blob16(row, 12)?;
    let authority_lease_holder_id = optional_blob16(row, 13)?;
    let authority_lease_term = optional_blob::<8>(row, 14)?;
    let authority_lease_epoch = optional_blob::<8>(row, 15)?;
    let authority_lease_fencing_token = optional_blob::<32>(row, 16)?;
    let authority_lease_expires_at_ms: Option<i64> = row.get(17)?;
    let authority_lease_binding = match (
        authority_lease_authority_id,
        authority_lease_holder_id,
        authority_lease_term,
        authority_lease_epoch,
        authority_lease_fencing_token,
        authority_lease_expires_at_ms,
    ) {
        (
            Some(authority_id),
            Some(holder_id),
            Some(term),
            Some(lease_epoch),
            Some(fencing_token),
            Some(expires_at_ms),
        ) => Some(AuthorityLeaseBinding {
            authority_id: TaskParticipantId::from_bytes(authority_id),
            holder_id: ProcessId::from_bytes(holder_id),
            term: u64::from_be_bytes(term),
            lease_epoch: u64::from_be_bytes(lease_epoch),
            fencing_token,
            expires_at_ms,
        }),
        (None, None, None, None, None, None) => None,
        _ => {
            return Err(TaskStoreError::CorruptRecord(
                "partial authority lease adoption binding",
            ));
        }
    };
    Ok(AdoptionReceiptRecord {
        receipt_id: ReceiptId::from_bytes(blob16(row, 0)?),
        task_id: TaskId::from_bytes(blob16(row, 1)?),
        task_generation: generation_from_blob(row, 2)?,
        original_permit_id: CommitPermitId::from_bytes(blob16(row, 4)?),
        original_permit_epoch: u64_from_blob(row, 5)?,
        original_control_epoch: u64_from_blob(row, 6)?,
        original_cancel_epoch: u64_from_blob(row, 7)?,
        effect_set_root: blob32(row, 8)?,
        observed_effect_slot_state_root: blob32(row, 9)?,
        adoption_epoch: u64_from_blob(row, 10)?,
        authority_lease_binding,
        created_at_ms: row.get(11)?,
    })
}

fn load_adoption_by_key(
    source: &impl SqlRead,
    task_id: TaskId,
    idempotency_key: IdempotencyKey,
) -> Result<Option<AdoptionReceiptRecord>, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {ADOPTION_COLUMNS} FROM task_adoption_receipts
         WHERE task_id = ?1 AND idempotency_key = ?2"
    ))?;
    let mut rows = statement.query(params![
        task_id.as_bytes().as_slice(),
        idempotency_key.as_bytes().as_slice(),
    ])?;
    rows.next()?.map(decode_adoption_row).transpose()
}

fn load_adoption_by_id(
    source: &impl SqlRead,
    task_id: TaskId,
    receipt_id: ReceiptId,
) -> Result<Option<AdoptionReceiptRecord>, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {ADOPTION_COLUMNS} FROM task_adoption_receipts
         WHERE task_id = ?1 AND receipt_id = ?2"
    ))?;
    let mut rows = statement.query(params![
        task_id.as_bytes().as_slice(),
        receipt_id.as_bytes().as_slice(),
    ])?;
    rows.next()?.map(decode_adoption_row).transpose()
}

const RECONCILE_COLUMNS: &str = "receipt_id, task_id, permit_id, permit_epoch,
     permit_adoption_receipt_id, effect_slot_id, effect_seq, logical_effect_id,
     retry_fence_epoch, effect_set_root, outcome, closure_proof_digest,
     effect_receipt_id, effect_slot_state_root_after, created_at_ms";

fn decode_reconcile_row(
    row: &rusqlite::Row<'_>,
) -> Result<ReconciliationReceiptRecord, TaskStoreError> {
    let outcome = match row.get::<_, i64>(10)? {
        0 => ReconcileOutcome::EffectClosed,
        1 => ReconcileOutcome::ConfirmedNoEffect,
        2 => ReconcileOutcome::EffectUnknown,
        _ => {
            return Err(TaskStoreError::CorruptRecord(
                "unknown reconcile receipt outcome",
            ));
        }
    };
    Ok(ReconciliationReceiptRecord {
        receipt_id: ReceiptId::from_bytes(blob16(row, 0)?),
        task_id: TaskId::from_bytes(blob16(row, 1)?),
        permit_id: CommitPermitId::from_bytes(blob16(row, 2)?),
        permit_epoch: u64_from_blob(row, 3)?,
        permit_adoption_receipt_id: ReceiptId::from_bytes(blob16(row, 4)?),
        effect_slot_id: crate::EffectSlotId::from_bytes(blob16(row, 5)?),
        effect_seq: u64_from_blob(row, 6)?,
        logical_effect_id: blob32(row, 7)?,
        retry_fence_epoch: u64_from_blob(row, 8)?,
        effect_set_root: blob32(row, 9)?,
        outcome,
        closure_proof_digest: blob32(row, 11)?,
        effect_receipt_id: optional_blob16(row, 12)?.map(ReceiptId::from_bytes),
        effect_slot_state_root_after: blob32(row, 13)?,
        created_at_ms: row.get(14)?,
    })
}

fn load_latest_reconcile_for_slot(
    source: &impl SqlRead,
    permit_id: CommitPermitId,
    effect_seq: u64,
) -> Result<Option<ReconciliationReceiptRecord>, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {RECONCILE_COLUMNS} FROM task_reconcile_receipts
         WHERE permit_id = ?1 AND effect_seq = ?2 ORDER BY rowid DESC LIMIT 1"
    ))?;
    let mut rows = statement.query(params![
        permit_id.as_bytes().as_slice(),
        encode_u64(effect_seq).as_slice(),
    ])?;
    rows.next()?.map(decode_reconcile_row).transpose()
}

fn insert_reconcile_receipt(
    transaction: &Transaction<'_>,
    record: &ReconciliationReceiptRecord,
) -> Result<(), TaskStoreError> {
    let outcome_code = match record.outcome {
        ReconcileOutcome::EffectClosed => 0,
        ReconcileOutcome::ConfirmedNoEffect => 1,
        ReconcileOutcome::EffectUnknown => 2,
    };
    transaction.execute(
        "INSERT INTO task_reconcile_receipts (
            receipt_id, task_id, permit_id, permit_epoch,
            permit_adoption_receipt_id, effect_slot_id, effect_seq,
            logical_effect_id, retry_fence_epoch, effect_set_root, outcome,
            closure_proof_digest, effect_receipt_id,
            effect_slot_state_root_after, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![
            record.receipt_id.as_bytes().as_slice(),
            record.task_id.as_bytes().as_slice(),
            record.permit_id.as_bytes().as_slice(),
            encode_u64(record.permit_epoch).as_slice(),
            record.permit_adoption_receipt_id.as_bytes().as_slice(),
            record.effect_slot_id.as_bytes().as_slice(),
            encode_u64(record.effect_seq).as_slice(),
            record.logical_effect_id.as_slice(),
            encode_u64(record.retry_fence_epoch).as_slice(),
            record.effect_set_root.as_slice(),
            outcome_code,
            record.closure_proof_digest.as_slice(),
            record
                .effect_receipt_id
                .map(ReceiptId::into_bytes)
                .as_ref()
                .map(<[u8; 16]>::as_slice),
            record.effect_slot_state_root_after.as_slice(),
            record.created_at_ms,
        ],
    )?;
    Ok(())
}

fn count_unknown_slots(
    source: &impl SqlRead,
    permit_id: CommitPermitId,
) -> Result<u64, TaskStoreError> {
    let mut statement = source.prepare_statement(
        "SELECT COUNT(*) FROM effect_slots
         WHERE permit_id = ?1 AND slot_state = ?2",
    )?;
    let count: i64 = statement.query_row(
        params![
            permit_id.as_bytes().as_slice(),
            SlotState::EffectUnknown.code(),
        ],
        |row| row.get(0),
    )?;
    u64::try_from(count).map_err(|_| TaskStoreError::CorruptRecord("negative slot count"))
}

fn finalize_proof_digest(request: &FinalizeRequestV3) -> [u8; 32] {
    let mut encoded: Vec<[u8; 41]> = Vec::with_capacity(request.required_satisfaction.len());
    for satisfaction in &request.required_satisfaction {
        let mut bytes = [0u8; 41];
        bytes[..8].copy_from_slice(&satisfaction.effect_seq.to_be_bytes());
        match satisfaction.proof {
            RequiredSatisfactionProof::EffectClosedSuccess {
                success_assertion_digest,
            } => {
                bytes[8] = 0;
                bytes[9..].copy_from_slice(&success_assertion_digest);
            }
            RequiredSatisfactionProof::ConditionNotApplicable {
                condition_false_proof_digest,
            } => {
                bytes[8] = 1;
                bytes[9..].copy_from_slice(&condition_false_proof_digest);
            }
        }
        encoded.push(bytes);
    }
    let parts: Vec<&[u8]> = encoded.iter().map(<[u8; 41]>::as_slice).collect();
    sha256("llmos/task-finalize-proofs/v1", &parts)
}

fn insert_finalize_proof(
    transaction: &Transaction<'_>,
    receipt_id: ReceiptId,
    proof_digest: [u8; 32],
) -> Result<(), TaskStoreError> {
    transaction.execute(
        "INSERT INTO task_finalize_proofs (receipt_id, proof_digest) VALUES (?1, ?2)",
        params![receipt_id.as_bytes().as_slice(), proof_digest.as_slice()],
    )?;
    Ok(())
}

fn load_finalize_proof(
    source: &impl SqlRead,
    receipt_id: ReceiptId,
) -> Result<Option<[u8; 32]>, TaskStoreError> {
    let mut statement = source
        .prepare_statement("SELECT proof_digest FROM task_finalize_proofs WHERE receipt_id = ?1")?;
    let mut rows = statement.query([receipt_id.as_bytes().as_slice()])?;
    rows.next()?
        .map(|row| {
            let value: Vec<u8> = row.get(0)?;
            <[u8; 32]>::try_from(value.as_slice())
                .map_err(|_| TaskStoreError::CorruptRecord("expected 32-byte finalize proof"))
        })
        .transpose()
}

/// Expected snapshot-bound placeholder binding of a
/// `CONDITION_NOT_APPLICABLE` condition-false proof (`[TASK-COMMIT-002]`).
fn expected_condition_false_proof(
    snapshot_digest: &[u8; 32],
    required_condition_digest: &[u8; 32],
) -> [u8; 32] {
    sha256(
        "llmos/task-condition-false-proof/v1",
        &[snapshot_digest, required_condition_digest],
    )
}

/// The slot transition half of one reconcile step: CAS
/// `EffectUnknown → Reconciling → target`, the closure effect receipt for
/// resolved outcomes, and the same-transaction history append
/// (`[TASK-EFFECT-003]` / `[TASK-EFFECT-ID-001]`). Returns the final
/// state sequence and the closure receipt identity (if any).
fn apply_reconcile_outcome(
    transaction: &Transaction<'_>,
    task: &StoredTask,
    slot: &SlotRecord,
    request: &ReconcileRequest,
) -> Result<(u64, Option<ReceiptId>), TaskStoreError> {
    let reconciling = effect::cas_slot(
        transaction,
        slot,
        SlotState::Reconciling,
        None,
        None,
        None,
        request.reconciled_at_ms,
    )?;
    let final_state_seq = reconciling.state_seq + 1;
    let (target, kind) = match request.outcome {
        ReconcileOutcome::EffectClosed => {
            (SlotState::EffectClosed, crate::ReceiptKind::EffectClosed)
        }
        ReconcileOutcome::ConfirmedNoEffect => (
            SlotState::ConfirmedNoEffect,
            crate::ReceiptKind::ConfirmedNoEffect,
        ),
        ReconcileOutcome::EffectUnknown => {
            (SlotState::EffectUnknown, crate::ReceiptKind::EffectUnknown)
        }
    };
    let effect_receipt_id = match request.outcome {
        ReconcileOutcome::EffectUnknown => None,
        ReconcileOutcome::EffectClosed | ReconcileOutcome::ConfirmedNoEffect => {
            let receipt = effect::EffectReceipt {
                receipt_id: effect::derive_effect_receipt_id(
                    "llmos/task-effect-reconcile-closure/v1",
                    slot.effect_slot_id,
                    final_state_seq,
                ),
                task_id: request.task_id,
                permit_id: request.permit_id,
                effect_slot_id: slot.effect_slot_id,
                effect_seq: slot.effect_seq,
                logical_effect_id: slot.logical_effect_id,
                kind,
                prior_slot_state: SlotState::EffectUnknown,
                no_effect_reason: None,
                proof_digest: request.closure_proof_digest,
                created_at_ms: request.reconciled_at_ms,
            };
            insert_effect_receipt(transaction, &receipt)?;
            Some(receipt.receipt_id)
        }
    };
    let updated = effect::cas_slot(
        transaction,
        &reconciling,
        target,
        None,
        None,
        effect_receipt_id,
        request.reconciled_at_ms,
    )?;
    if let Some(receipt_id) = effect_receipt_id {
        let history_outcome = match request.outcome {
            ReconcileOutcome::EffectClosed => EffectHistoryOutcome::EffectClosed,
            ReconcileOutcome::ConfirmedNoEffect => EffectHistoryOutcome::ConfirmedNoEffect,
            ReconcileOutcome::EffectUnknown => {
                return Err(TaskStoreError::CorruptRecord(
                    "unknown reconcile must not write an effect receipt",
                ));
            }
        };
        append_history_entry(
            transaction,
            &HistoryAppend {
                task_id: task.record.task_id,
                retry_fence_epoch: task.record.retry_fence_epoch,
                slot: &updated,
                outcome: history_outcome,
                authoritative_effect_receipt_id: receipt_id,
                now_ms: request.reconciled_at_ms,
            },
        )?;
    }
    Ok((final_state_seq, effect_receipt_id))
}

/// Shared context for the permit-terminal writes (quarantine / commit /
/// closure), keeping the helpers under the argument-count lint.
struct TerminalCtx<'a> {
    task: &'a StoredTask,
    permit: &'a PermitRecord,
    attempt: &'a crate::AttemptRecord,
    now_ms: i64,
}

fn validate_permit_authority_lease(
    transaction: &Transaction<'_>,
    permit: &PermitRecord,
    now_ms: i64,
    authority_lease: Option<AuthorityLeaseRecord>,
) -> Result<(), TaskStoreError> {
    match (permit.authority_lease_binding, authority_lease) {
        (None, None) => Ok(()),
        (None, Some(_)) => Err(TaskStoreError::AuthorityLeaseBindingMismatch),
        (Some(_), None) => Err(TaskStoreError::AuthorityLeaseRequired),
        (Some(binding), Some(lease)) => {
            if binding != lease.binding() {
                return Err(TaskStoreError::AuthorityLeaseBindingMismatch);
            }
            validate_authority_lease_binding_in_transaction(transaction, binding, now_ms)
        }
    }
}

/// Writes/replays the quarantine tombstone and optionally advances the
/// control epoch (fresh quarantines only). The `TaskHead` is never
/// touched here (`[TASK-EFFECT-003]`).
fn quarantine_decision(
    transaction: &Transaction<'_>,
    ctx: &TerminalCtx<'_>,
    fenced_participant_digest: [u8; 32],
    bump_control_epoch: bool,
) -> Result<QuarantineReceiptRecord, TaskStoreError> {
    let record = quarantine_permit(
        transaction,
        ctx.task,
        ctx.permit,
        fenced_participant_digest,
        ctx.now_ms,
    )?;
    if bump_control_epoch {
        let control_epoch = ctx
            .task
            .record
            .control_epoch
            .checked_add(1)
            .ok_or(TaskStoreError::EpochExhausted)?;
        update_task(transaction, ctx.task, ctx.now_ms, |record| {
            record.control_epoch = control_epoch;
        })?;
    }
    Ok(record)
}

/// Builds, stores, and applies the commit receipt: head advance, permit
/// close, attempt transition, finalize-proof record, and task update in
/// the caller's transaction (`[TASK-COMMIT-001]` / `[TASK-COMMIT-002]`).
fn write_commit_receipt(
    transaction: &Transaction<'_>,
    ctx: &TerminalCtx<'_>,
    request: &FinalizeRequestV3,
    outcome: ReceiptOutcome,
    new_fence: u64,
) -> Result<TaskReceiptRecord, TaskStoreError> {
    let new_seq = ctx
        .task
        .record
        .head_commit_seq
        .checked_add(1)
        .ok_or(TaskStoreError::EpochExhausted)?;
    let control_epoch = ctx
        .task
        .record
        .control_epoch
        .checked_add(1)
        .ok_or(TaskStoreError::EpochExhausted)?;
    let new_root = history_root(transaction, ctx.task.record.task_id)?;
    let receipt_id = model::derive_commit_receipt_id(ctx.permit.permit_id);
    let receipt = TaskReceiptRecord {
        receipt_id,
        task_id: ctx.task.record.task_id,
        permit_id: Some(ctx.permit.permit_id),
        attempt_id: ctx.attempt.attempt_id,
        attempt_generation: ctx.attempt.attempt_generation,
        group_binding: ctx.permit.group_binding,
        participant_registry_binding: ctx.permit.participant_registry_binding,
        outcome,
        prior_head_commit_seq: ctx.task.record.head_commit_seq,
        prior_effect_history_root: ctx.task.record.head_effect_history_root,
        prior_retry_fence_epoch: ctx.task.record.retry_fence_epoch,
        new_head_commit_seq: new_seq,
        new_effect_history_root: new_root,
        new_retry_fence_epoch: new_fence,
        created_at_ms: ctx.now_ms,
    };
    insert_receipt(transaction, &receipt)?;
    insert_finalize_proof(transaction, receipt_id, finalize_proof_digest(request))?;
    close_permit(transaction, ctx.permit, ctx.now_ms)?;
    let attempt_state = match outcome {
        ReceiptOutcome::FailedAfterEffect => AttemptState::Failed,
        _ => AttemptState::Committed,
    };
    set_attempt_state(
        transaction,
        ctx.attempt,
        attempt_state,
        Some(receipt_id),
        ctx.now_ms,
    )?;
    update_task(transaction, ctx.task, ctx.now_ms, |record| {
        record.head_commit_seq = new_seq;
        record.head_effect_history_root = new_root;
        record.retry_fence_epoch = new_fence;
        record.control_epoch = control_epoch;
    })?;
    Ok(receipt)
}

enum FinalizeImplResult {
    Plain(FinalizeDecision),
    Semantic(SemanticFinalizeDecision),
}

/// Builds, stores, and applies the `TaskPermitClosureReceipt`-shaped
/// record: permit close and attempt transition with the `TaskHead`
/// unchanged (`[TASK-COMMIT-002]` final clause).
fn write_closure_receipt(
    transaction: &Transaction<'_>,
    ctx: &TerminalCtx<'_>,
    outcome: crate::PermitClosureOutcome,
) -> Result<TaskReceiptRecord, TaskStoreError> {
    let control_epoch = ctx
        .task
        .record
        .control_epoch
        .checked_add(1)
        .ok_or(TaskStoreError::EpochExhausted)?;
    let receipt_id = model::derive_permit_closure_receipt_id(ctx.permit.permit_id);
    let receipt = TaskReceiptRecord {
        receipt_id,
        task_id: ctx.task.record.task_id,
        permit_id: Some(ctx.permit.permit_id),
        attempt_id: ctx.attempt.attempt_id,
        attempt_generation: ctx.attempt.attempt_generation,
        group_binding: ctx.permit.group_binding,
        participant_registry_binding: ctx.permit.participant_registry_binding,
        outcome: outcome.receipt_outcome(),
        prior_head_commit_seq: ctx.task.record.head_commit_seq,
        prior_effect_history_root: ctx.task.record.head_effect_history_root,
        prior_retry_fence_epoch: ctx.task.record.retry_fence_epoch,
        new_head_commit_seq: ctx.task.record.head_commit_seq,
        new_effect_history_root: ctx.task.record.head_effect_history_root,
        new_retry_fence_epoch: ctx.task.record.retry_fence_epoch,
        created_at_ms: ctx.now_ms,
    };
    insert_receipt(transaction, &receipt)?;
    close_permit(transaction, ctx.permit, ctx.now_ms)?;
    let attempt_state = match outcome.receipt_outcome() {
        ReceiptOutcome::CancelledBeforeEffect => AttemptState::Cancelled,
        _ => AttemptState::Failed,
    };
    set_attempt_state(
        transaction,
        ctx.attempt,
        attempt_state,
        Some(receipt_id),
        ctx.now_ms,
    )?;
    update_task(transaction, ctx.task, ctx.now_ms, |record| {
        record.control_epoch = control_epoch;
    })?;
    Ok(receipt)
}

impl SqliteTaskAuthority {
    /// Legacy finalize entry point (B-TASK-001/002 request shape AND
    /// semantics, preserved bit-for-bit): any non-terminal slot blocks
    /// with `OutstandingEffectSlots`, a fully terminal permit commits
    /// with the caller-supplied roots, and the fence must never regress.
    /// The strict `[TASK-COMMIT-002]` proof semantics, the quarantine
    /// tombstone, and `[TASK-RETRY-EFFECT-001]` fence advancement live in
    /// [`SqliteTaskAuthority::finalize_commit_v3`].
    ///
    /// # Errors
    ///
    /// See [`SqliteTaskAuthority::finalize_commit_v3`].
    pub fn finalize_commit(
        &self,
        request: FinalizeRequest,
    ) -> Result<FinalizeDecision, TaskStoreError> {
        self.finalize_impl(
            &FinalizeRequestV3 {
                base: request,
                required_satisfaction: Vec::new(),
                fenced_participant_digest: [0u8; 32],
            },
            true,
        )
    }

    /// Finalizes an issued permit (`[TASK-COMMIT-001]` CAS commit +
    /// `[TASK-COMMIT-002]` full required-slot semantics since schema v3).
    ///
    /// One linearized decision per call:
    /// - any `EffectUnknown` slot → the permit becomes a non-reusable
    ///   `Quarantined` tombstone with a durable quarantine receipt; the
    ///   `TaskHead` does NOT advance and the call returns the typed
    ///   [`TaskStoreError::Quarantined`] refusal (`[TASK-EFFECT-003]`);
    ///   replaying the same finalize observes the same refusal;
    /// - any other non-terminal slot → typed `OutstandingEffectSlots`;
    /// - all terminal and every required slot satisfied (a slot/Receipt-bound
    ///   `EffectClosedSuccess`, or snapshot-bound
    ///   `ConditionNotApplicable`) → `Committed`;
    /// - required unsatisfied with at least one effect already closed →
    ///   `PartialEffect` (at least one required slot satisfied) or
    ///   `FailedAfterEffect` (none), appending `PartialEffect` history
    ///   entries for the unsatisfied required slots and advancing
    ///   head + history root + retry fence in the same CAS
    ///   (`[TASK-RETRY-EFFECT-001]`);
    /// - required unsatisfied with zero effects → typed
    ///   `RequiredEffectUnsatisfied`, permit stays open.
    ///
    /// Replaying a finalized/quarantined permit with the same bytes
    /// returns the original lifecycle state; different bytes fail closed.
    ///
    /// # Errors
    ///
    /// Returns a not-found, holder, stale-head, slot-state,
    /// required-proof, replay-conflict, or storage error.
    // By-value request mirrors every other mutating API here; the lint
    // only fires because the implementation borrows it internally.
    #[allow(clippy::needless_pass_by_value)]
    pub fn finalize_commit_v3(
        &self,
        request: FinalizeRequestV3,
    ) -> Result<FinalizeDecision, TaskStoreError> {
        self.finalize_impl(&request, false)
    }

    /// Finalizes a permit whose issuance was bound to a durable authority
    /// lease. The lease term, epoch, fencing token, and expiry are checked in
    /// the same transaction as the terminal Task CAS.
    ///
    /// # Errors
    ///
    /// Returns the same lifecycle errors as `finalize_commit_v3`, plus a typed
    /// lease-required, lease-fenced, or lease-expired error.
    #[allow(clippy::needless_pass_by_value)]
    pub fn finalize_commit_v3_with_authority_lease(
        &self,
        request: AuthorityLeaseFinalizeRequest,
    ) -> Result<FinalizeDecision, TaskStoreError> {
        self.finalize_impl_with_authority_lease(&request.finalize, false, Some(request.lease))
    }

    /// Finalizes an issued permit after re-reading any sealed Semantic append
    /// proofs from the owning [`nlos_semantic::SemanticAuthority`]. A
    /// closed/quarantined permit follows the normal replay path and does not
    /// require a second owner read. This boundary is intentionally weaker
    /// than Semantic publication: it never acknowledges an outbox row,
    /// creates a checkpoint, or adds publication fields to the task receipt.
    ///
    /// # Errors
    ///
    /// Returns the same lifecycle errors as [`Self::finalize_commit_v3`], plus
    /// a fail-closed Semantic authority or sealed-write-set error when an
    /// issued permit's owner proof cannot be re-read exactly.
    #[allow(clippy::needless_pass_by_value)]
    pub fn finalize_commit_v3_with_semantic_authority(
        &self,
        semantic_authority: &nlos_semantic::SemanticAuthority,
        request: FinalizeRequestV3,
    ) -> Result<FinalizeDecision, TaskStoreError> {
        let permit = self.inspect_permit(request.base.task_id, request.base.permit_id)?;
        if permit.state == PermitState::Issued && permit.write_set_root != [0; 32] {
            let record = {
                let connection = self.lock_connection()?;
                store::load_write_set_by_root(
                    &*connection,
                    request.base.task_id,
                    permit.write_set_root,
                )?
            }
            .ok_or(TaskStoreError::TaskWriteSetNotFound)?;
            if record.write_set_root != crate::model::task_write_set_root(&record) {
                return Err(TaskStoreError::CorruptRecord(
                    "TaskWriteSet canonical root mismatch before Semantic finalization",
                ));
            }
            validate_semantic_finalization(semantic_authority, &record)?;
        }
        self.finalize_impl(&request, false)
    }

    /// Semantic-owner revalidation plus terminalization for a permit bound to
    /// an authority lease. The owner readback and Task CAS remain separate
    /// facts, while the lease check is part of the Task transaction.
    ///
    /// # Errors
    ///
    /// Returns the same errors as `finalize_commit_v3_with_semantic_authority`,
    /// plus a typed lease-required, lease-fenced, or lease-expired error.
    #[allow(clippy::needless_pass_by_value)]
    pub fn finalize_commit_v3_with_semantic_authority_and_authority_lease(
        &self,
        semantic_authority: &nlos_semantic::SemanticAuthority,
        request: FinalizeRequestV3,
        authority_lease: AuthorityLeaseRecord,
    ) -> Result<FinalizeDecision, TaskStoreError> {
        let permit = self.inspect_permit(request.base.task_id, request.base.permit_id)?;
        if permit.state == PermitState::Issued && permit.write_set_root != [0; 32] {
            let record = {
                let connection = self.lock_connection()?;
                store::load_write_set_by_root(
                    &*connection,
                    request.base.task_id,
                    permit.write_set_root,
                )?
            }
            .ok_or(TaskStoreError::TaskWriteSetNotFound)?;
            if record.write_set_root != crate::model::task_write_set_root(&record) {
                return Err(TaskStoreError::CorruptRecord(
                    "TaskWriteSet canonical root mismatch before lease-bound Semantic finalization",
                ));
            }
            validate_semantic_finalization(semantic_authority, &record)?;
        }
        self.finalize_impl_with_authority_lease(&request, false, Some(authority_lease))
    }

    /// Finalizes a mixed Effect + Semantic permit after the Task-side
    /// Semantic publication plan is `READY`. The existing v3 effect-slot
    /// evaluation and history append run unchanged; the immutable nested
    /// Semantic receipts and the Task terminal receipt are committed in the
    /// same `TaskAuthority` transaction.
    ///
    /// # Errors
    ///
    /// Returns the normal v3 lifecycle errors, plus Semantic plan readiness,
    /// owner-proof, binding, and storage errors. A failed call leaves both
    /// the plan and all terminal Task facts at their prior durable prefix.
    #[allow(clippy::needless_pass_by_value)]
    pub fn finalize_commit_v3_with_semantic_publications(
        &self,
        semantic_authority: &nlos_semantic::SemanticAuthority,
        plan_id: SemanticCommitPlanId,
        request: FinalizeRequestV3,
    ) -> Result<SemanticFinalizeDecision, TaskStoreError> {
        let permit = self.inspect_permit(request.base.task_id, request.base.permit_id)?;
        if permit.state == PermitState::Issued && permit.write_set_root != [0; 32] {
            let record = {
                let connection = self.lock_connection()?;
                store::load_write_set_by_root(
                    &*connection,
                    request.base.task_id,
                    permit.write_set_root,
                )?
            }
            .ok_or(TaskStoreError::TaskWriteSetNotFound)?;
            if record.write_set_root != crate::model::task_write_set_root(&record) {
                return Err(TaskStoreError::CorruptRecord(
                    "TaskWriteSet canonical root mismatch before mixed Semantic finalization",
                ));
            }
            validate_semantic_finalization(semantic_authority, &record)?;
        }
        self.finalize_impl_with_semantic_plan(&request, plan_id)
    }

    /// Mixed Effect + Semantic finalization with both owner proof revalidation
    /// and an immutable authority-lease binding.
    ///
    /// # Errors
    ///
    /// Returns the same errors as `finalize_commit_v3_with_semantic_publications`,
    /// plus a typed lease-required, lease-fenced, or lease-expired error.
    #[allow(clippy::needless_pass_by_value)]
    pub fn finalize_commit_v3_with_semantic_publications_and_authority_lease(
        &self,
        semantic_authority: &nlos_semantic::SemanticAuthority,
        plan_id: SemanticCommitPlanId,
        request: FinalizeRequestV3,
        authority_lease: AuthorityLeaseRecord,
    ) -> Result<SemanticFinalizeDecision, TaskStoreError> {
        let permit = self.inspect_permit(request.base.task_id, request.base.permit_id)?;
        if permit.state == PermitState::Issued && permit.write_set_root != [0; 32] {
            let record = {
                let connection = self.lock_connection()?;
                store::load_write_set_by_root(
                    &*connection,
                    request.base.task_id,
                    permit.write_set_root,
                )?
            }
            .ok_or(TaskStoreError::TaskWriteSetNotFound)?;
            if record.write_set_root != crate::model::task_write_set_root(&record) {
                return Err(TaskStoreError::CorruptRecord(
                    "TaskWriteSet canonical root mismatch before lease-bound mixed Semantic finalization",
                ));
            }
            validate_semantic_finalization(semantic_authority, &record)?;
        }
        self.finalize_impl_with_semantic_plan_and_authority_lease(
            &request,
            plan_id,
            authority_lease,
        )
    }

    /// Reconstructs a mixed v3 finalize request from the immutable envelope
    /// prepared on the Semantic plan, then runs the same owner revalidation
    /// and `TaskAuthority` transaction as the direct API.
    ///
    /// # Errors
    ///
    /// Returns the normal mixed-finalize errors, or a typed invalid-plan
    /// error when the plan has no persisted envelope.
    #[allow(clippy::needless_pass_by_value)]
    pub fn finalize_commit_v3_with_persisted_semantic_envelope(
        &self,
        semantic_authority: &nlos_semantic::SemanticAuthority,
        plan_id: SemanticCommitPlanId,
        finalized_at_ms: i64,
    ) -> Result<SemanticFinalizeDecision, TaskStoreError> {
        let plan = self.inspect_semantic_commit_plan(plan_id)?;
        let envelope = self.inspect_semantic_finalize_envelope(plan_id)?.ok_or(
            TaskStoreError::InvalidSemanticPublicationPlan {
                reason: "mixed finalize envelope is not prepared",
            },
        )?;
        self.finalize_commit_v3_with_semantic_publications(
            semantic_authority,
            plan_id,
            FinalizeRequestV3 {
                base: FinalizeRequest {
                    task_id: plan.task_id,
                    attempt_id: plan.attempt_id,
                    attempt_generation: plan.attempt_generation,
                    permit_id: plan.permit_id,
                    new_effect_history_root: [0; 32],
                    new_retry_fence_epoch: 0,
                    finalized_at_ms,
                },
                required_satisfaction: envelope.required_satisfaction,
                fenced_participant_digest: envelope.fenced_participant_digest,
            },
        )
    }

    /// Reconstructs a persisted mixed-finalize envelope and terminalizes it
    /// under the same authority lease bound to the permit.
    ///
    /// # Errors
    ///
    /// Returns the same errors as `finalize_commit_v3_with_persisted_semantic_envelope`,
    /// plus a typed lease-required, lease-fenced, or lease-expired error.
    #[allow(clippy::needless_pass_by_value)]
    pub fn finalize_commit_v3_with_persisted_semantic_envelope_and_authority_lease(
        &self,
        semantic_authority: &nlos_semantic::SemanticAuthority,
        plan_id: SemanticCommitPlanId,
        finalized_at_ms: i64,
        authority_lease: AuthorityLeaseRecord,
    ) -> Result<SemanticFinalizeDecision, TaskStoreError> {
        let plan = self.inspect_semantic_commit_plan(plan_id)?;
        let envelope = self.inspect_semantic_finalize_envelope(plan_id)?.ok_or(
            TaskStoreError::InvalidSemanticPublicationPlan {
                reason: "mixed finalize envelope is not prepared",
            },
        )?;
        self.finalize_commit_v3_with_semantic_publications_and_authority_lease(
            semantic_authority,
            plan_id,
            FinalizeRequestV3 {
                base: FinalizeRequest {
                    task_id: plan.task_id,
                    attempt_id: plan.attempt_id,
                    attempt_generation: plan.attempt_generation,
                    permit_id: plan.permit_id,
                    new_effect_history_root: [0; 32],
                    new_retry_fence_epoch: 0,
                    finalized_at_ms,
                },
                required_satisfaction: envelope.required_satisfaction,
                fenced_participant_digest: envelope.fenced_participant_digest,
            },
            authority_lease,
        )
    }

    fn finalize_impl(
        &self,
        request: &FinalizeRequestV3,
        legacy: bool,
    ) -> Result<FinalizeDecision, TaskStoreError> {
        match self.finalize_impl_inner(request, legacy, None, None)? {
            FinalizeImplResult::Plain(decision) => Ok(decision),
            FinalizeImplResult::Semantic(_) => Err(TaskStoreError::CorruptRecord(
                "Semantic finalize result returned through plain API",
            )),
        }
    }

    fn finalize_impl_with_authority_lease(
        &self,
        request: &FinalizeRequestV3,
        legacy: bool,
        authority_lease: Option<AuthorityLeaseRecord>,
    ) -> Result<FinalizeDecision, TaskStoreError> {
        match self.finalize_impl_inner(request, legacy, authority_lease, None)? {
            FinalizeImplResult::Plain(decision) => Ok(decision),
            FinalizeImplResult::Semantic(_) => Err(TaskStoreError::CorruptRecord(
                "Semantic finalize result returned through plain API",
            )),
        }
    }

    fn finalize_impl_with_semantic_plan(
        &self,
        request: &FinalizeRequestV3,
        plan_id: SemanticCommitPlanId,
    ) -> Result<SemanticFinalizeDecision, TaskStoreError> {
        match self.finalize_impl_inner(request, false, None, Some(plan_id))? {
            FinalizeImplResult::Semantic(decision) => Ok(decision),
            FinalizeImplResult::Plain(_) => Err(TaskStoreError::CorruptRecord(
                "plain finalize result returned through Semantic API",
            )),
        }
    }

    fn finalize_impl_with_semantic_plan_and_authority_lease(
        &self,
        request: &FinalizeRequestV3,
        plan_id: SemanticCommitPlanId,
        authority_lease: AuthorityLeaseRecord,
    ) -> Result<SemanticFinalizeDecision, TaskStoreError> {
        match self.finalize_impl_inner(request, false, Some(authority_lease), Some(plan_id))? {
            FinalizeImplResult::Semantic(decision) => Ok(decision),
            FinalizeImplResult::Plain(_) => Err(TaskStoreError::CorruptRecord(
                "plain finalize result returned through Semantic API",
            )),
        }
    }

    #[allow(clippy::too_many_lines)] // Lifecycle branches stay adjacent for transaction audit.
    fn finalize_impl_inner(
        &self,
        request: &FinalizeRequestV3,
        legacy: bool,
        authority_lease: Option<AuthorityLeaseRecord>,
        semantic_plan_id: Option<SemanticCommitPlanId>,
    ) -> Result<FinalizeImplResult, TaskStoreError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task = store::load_task(&transaction, request.base.task_id)?;
        let permit =
            store::load_permit_by_id(&transaction, request.base.task_id, request.base.permit_id)?;
        let attempt =
            store::load_attempt(&transaction, request.base.task_id, request.base.attempt_id)?;
        if permit.state == PermitState::Issued {
            validate_permit_authority_lease(
                &transaction,
                &permit,
                request.base.finalized_at_ms,
                authority_lease,
            )?;
        }
        if attempt.attempt_generation != request.base.attempt_generation {
            return Err(TaskStoreError::InvalidGeneration);
        }
        let ctx = TerminalCtx {
            task: &task,
            permit: &permit,
            attempt: &attempt,
            now_ms: request.base.finalized_at_ms,
        };
        match permit.state {
            PermitState::Quarantined => {
                // Replay of the request that produced the tombstone: same
                // lifecycle state (typed refusal), no double quarantine.
                quarantine_decision(&transaction, &ctx, request.fenced_participant_digest, false)?;
                transaction.commit()?;
                return Err(TaskStoreError::Quarantined);
            }
            PermitState::Closed => {
                let decision = replay_finalize(&transaction, &permit, request)?;
                if let Some(plan_id) = semantic_plan_id {
                    let receipt = match &decision {
                        FinalizeDecision::Committed(receipt)
                        | FinalizeDecision::Replayed(receipt) => (**receipt).clone(),
                    };
                    let publications =
                        crate::semantic_commit::load_finalized_semantic_publications(
                            &transaction,
                            plan_id,
                            request.base.task_id,
                            receipt.receipt_id,
                        )?;
                    transaction.commit()?;
                    return Ok(FinalizeImplResult::Semantic(
                        SemanticFinalizeDecision::Replayed(Box::new(SemanticTaskCommitReceipt {
                            task_receipt: receipt,
                            semantic_publications: publications,
                        })),
                    ));
                }
                transaction.commit()?;
                return Ok(FinalizeImplResult::Plain(decision));
            }
            PermitState::Issued => {}
            PermitState::Superseded => return Err(TaskStoreError::PermitNotIssued),
        }
        if permit.attempt_id != request.base.attempt_id
            || permit.attempt_generation != request.base.attempt_generation
        {
            return Err(TaskStoreError::NotPermitHolder);
        }
        if task.record.head_commit_seq != permit.expected_head_commit_seq
            || task.record.head_effect_history_root != permit.expected_effect_history_root
            || task.record.retry_fence_epoch != permit.expected_retry_fence_epoch
        {
            return Err(TaskStoreError::StaleTaskHead);
        }
        crate::group::validate_commit_binding(
            &transaction,
            attempt.attempt_id,
            permit.group_binding,
        )?;
        crate::participant::validate_frozen_binding(
            &transaction,
            &task.record,
            permit.participant_registry_binding,
        )?;
        if legacy {
            if semantic_plan_id.is_some() {
                return Err(TaskStoreError::InvalidSemanticPublicationPlan {
                    reason: "Semantic publication requires v3 finalize semantics",
                });
            }
            let decision = finalize_legacy(&transaction, &task, &permit, &attempt, request)?;
            transaction.commit()?;
            return Ok(FinalizeImplResult::Plain(decision));
        }
        let semantic_context = if let Some(plan_id) = semantic_plan_id {
            let write_set = store::load_write_set_by_root(
                &transaction,
                request.base.task_id,
                permit.write_set_root,
            )?
            .ok_or(TaskStoreError::TaskWriteSetNotFound)?;
            Some(crate::semantic_commit::load_ready_semantic_plan(
                &transaction,
                plan_id,
                request.base.task_id,
                permit.permit_id,
                &write_set,
            )?)
        } else {
            None
        };
        let slots = list_slots(&transaction, permit.permit_id)?;
        if slots
            .iter()
            .any(|slot| slot.state == SlotState::EffectUnknown)
        {
            // `[TASK-EFFECT-003]`: the tombstone and the control-epoch
            // advance commit here; the `TaskHead` does NOT advance.
            quarantine_decision(&transaction, &ctx, request.fenced_participant_digest, true)?;
            transaction.commit()?;
            return Err(TaskStoreError::Quarantined);
        }
        let blocking = slots
            .iter()
            .filter(|slot| slot.state.blocks_finalization())
            .count();
        if blocking > 0 {
            return Err(TaskStoreError::OutstandingEffectSlots {
                count: u64::try_from(blocking).unwrap_or(u64::MAX),
            });
        }
        let evaluation = evaluate_required(
            &transaction,
            &slots,
            &attempt.snapshot.snapshot_digest,
            request,
        )?;
        let (outcome, new_fence) = if evaluation.unsatisfied.is_empty() {
            (ReceiptOutcome::Committed, task.record.retry_fence_epoch)
        } else {
            partial_outcome_and_entries(
                &transaction,
                &task,
                &slots,
                &evaluation,
                request.base.finalized_at_ms,
            )?
        };
        let receipt = write_commit_receipt(&transaction, &ctx, request, outcome, new_fence)?;
        if let Some((plan, publications)) = semantic_context {
            crate::semantic_commit::finalize_plan(
                &transaction,
                plan.plan_id,
                receipt.receipt_id,
                request.base.finalized_at_ms,
            )?;
            transaction.commit()?;
            return Ok(FinalizeImplResult::Semantic(
                SemanticFinalizeDecision::Committed(Box::new(SemanticTaskCommitReceipt {
                    task_receipt: receipt,
                    semantic_publications: publications,
                })),
            ));
        }
        transaction.commit()?;
        Ok(FinalizeImplResult::Plain(FinalizeDecision::Committed(
            Box::new(receipt),
        )))
    }

    /// Closes an issued permit with a `TaskPermitClosureReceipt`-shaped
    /// record while keeping the `TaskHead` unchanged
    /// (`[TASK-COMMIT-002]` final clause, `[TASK-CANCEL-003]`).
    ///
    /// Every declared slot must hold an authoritative absence proof:
    /// `NoEffect` (token verifiably unconsumed) or `ConfirmedNoEffect`
    /// (external authority). Any `EffectClosed` slot forbids the path
    /// (`PermitHasEffects`); any `EffectUnknown` slot quarantines the
    /// permit instead; any other non-terminal slot is a typed
    /// `OutstandingEffectSlots` refusal. Replays return the original
    /// lifecycle state.
    ///
    /// # Errors
    ///
    /// Returns a not-found, holder, slot-state, effects-present,
    /// replay-conflict, or storage error.
    pub fn close_permit(
        &self,
        request: ClosePermitRequest,
    ) -> Result<ClosePermitDecision, TaskStoreError> {
        self.close_permit_inner(request, None)
    }

    /// Closes a lease-bound permit after checking the same live lease binding
    /// used at issuance.
    ///
    /// # Errors
    ///
    /// Returns the same lifecycle errors as `close_permit`, plus a typed
    /// lease-required, lease-fenced, or lease-expired error.
    pub fn close_permit_with_authority_lease(
        &self,
        request: AuthorityLeaseCloseRequest,
    ) -> Result<ClosePermitDecision, TaskStoreError> {
        self.close_permit_inner(request.close, Some(request.lease))
    }

    fn close_permit_inner(
        &self,
        request: ClosePermitRequest,
        authority_lease: Option<AuthorityLeaseRecord>,
    ) -> Result<ClosePermitDecision, TaskStoreError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task = store::load_task(&transaction, request.task_id)?;
        let permit = store::load_permit_by_id(&transaction, request.task_id, request.permit_id)?;
        let attempt = store::load_attempt(&transaction, request.task_id, request.attempt_id)?;
        if attempt.attempt_generation != request.attempt_generation {
            return Err(TaskStoreError::InvalidGeneration);
        }
        if permit.state == PermitState::Issued {
            validate_permit_authority_lease(
                &transaction,
                &permit,
                request.closed_at_ms,
                authority_lease,
            )?;
        }
        let ctx = TerminalCtx {
            task: &task,
            permit: &permit,
            attempt: &attempt,
            now_ms: request.closed_at_ms,
        };
        match permit.state {
            PermitState::Quarantined => {
                let record = quarantine_decision(
                    &transaction,
                    &ctx,
                    request.fenced_participant_digest,
                    false,
                )?;
                transaction.commit()?;
                return Ok(ClosePermitDecision::ReplayedQuarantine(Box::new(record)));
            }
            PermitState::Closed => {
                let receipt =
                    load_receipt_by_permit(&transaction, request.task_id, permit.permit_id)?
                        .ok_or(TaskStoreError::PermitNotIssued)?;
                if receipt.participant_registry_binding != permit.participant_registry_binding {
                    return Err(TaskStoreError::ParticipantRegistryBindingMismatch);
                }
                if receipt.outcome != request.outcome.receipt_outcome() {
                    return Err(TaskStoreError::HistoryConflict);
                }
                transaction.commit()?;
                return Ok(ClosePermitDecision::Replayed(Box::new(receipt)));
            }
            PermitState::Issued => {}
            PermitState::Superseded => return Err(TaskStoreError::PermitNotIssued),
        }
        if permit.attempt_id != request.attempt_id
            || permit.attempt_generation != request.attempt_generation
        {
            return Err(TaskStoreError::NotPermitHolder);
        }
        crate::group::validate_commit_binding(
            &transaction,
            attempt.attempt_id,
            permit.group_binding,
        )?;
        crate::participant::validate_frozen_binding(
            &transaction,
            &task.record,
            permit.participant_registry_binding,
        )?;
        let slots = list_slots(&transaction, permit.permit_id)?;
        if slots
            .iter()
            .any(|slot| slot.state == SlotState::EffectUnknown)
        {
            let record =
                quarantine_decision(&transaction, &ctx, request.fenced_participant_digest, true)?;
            transaction.commit()?;
            return Ok(ClosePermitDecision::Quarantined(Box::new(record)));
        }
        let blocking = slots
            .iter()
            .filter(|slot| slot.state.blocks_finalization())
            .count();
        if blocking > 0 {
            return Err(TaskStoreError::OutstandingEffectSlots {
                count: u64::try_from(blocking).unwrap_or(u64::MAX),
            });
        }
        let effects = slots
            .iter()
            .filter(|slot| slot.state == SlotState::EffectClosed)
            .count();
        if effects > 0 {
            return Err(TaskStoreError::PermitHasEffects {
                count: u64::try_from(effects).unwrap_or(u64::MAX),
            });
        }
        let receipt = write_closure_receipt(&transaction, &ctx, request.outcome)?;
        transaction.commit()?;
        Ok(ClosePermitDecision::Closed(Box::new(receipt)))
    }

    /// Issues a `PermitAdoptionReceipt`-shaped durable record for a
    /// quarantined permit (`[TASK-COMMIT-003]`, single-authority subset).
    ///
    /// The receipt binds the original permit and its epochs; its scope is
    /// fixed to `RECONCILE_CLOSE_OR_QUARANTINE_ONLY` — once any adoption
    /// exists, `request_effect_permit` and `consume_dispatch_token` refuse
    /// with `AdoptionScopeViolation`. Only quarantined permits can be
    /// adopted. Same idempotency key + same bytes replays the original
    /// record; different bytes fail closed.
    ///
    /// # Errors
    ///
    /// Returns a not-found, epoch, reconcile-state, replay-conflict, or
    /// storage error.
    pub fn adopt_permit(&self, request: AdoptionRequest) -> Result<AdoptionReplay, TaskStoreError> {
        self.adopt_permit_inner(request, None)
    }

    /// Adopts a quarantined permit while proving the exact live authority
    /// lease copied into that permit. The binding is persisted in the
    /// adoption receipt and is required again for mutating reconcile steps.
    /// This is same-term/local lease fencing only; Assignment/TakeoverReceipt
    /// coverage for cross-term adoption remains outside this slice.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::adopt_permit`], plus a typed
    /// lease-required, lease-fenced, or lease-expired error.
    #[allow(clippy::needless_pass_by_value)]
    pub fn adopt_permit_with_authority_lease(
        &self,
        request: AuthorityLeaseAdoptionRequest,
    ) -> Result<AdoptionReplay, TaskStoreError> {
        self.adopt_permit_inner(request.adoption, Some(request.lease))
    }

    #[allow(clippy::too_many_lines)] // Adoption CAS and lease binding stay one transaction.
    fn adopt_permit_inner(
        &self,
        request: AdoptionRequest,
        authority_lease: Option<AuthorityLeaseRecord>,
    ) -> Result<AdoptionReplay, TaskStoreError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task = store::load_task(&transaction, request.task_id)?;
        let permit = store::load_permit_by_id(&transaction, request.task_id, request.permit_id)?;
        if let Some(existing) =
            load_adoption_by_key(&transaction, request.task_id, request.idempotency_key)?
        {
            let same_bytes = existing.original_permit_id == request.permit_id
                && existing.original_permit_epoch == request.permit_epoch;
            if !same_bytes {
                return Err(TaskStoreError::IdempotencyConflict);
            }
            transaction.commit()?;
            return Ok(AdoptionReplay::Replayed(Box::new(existing)));
        }
        if permit.state != PermitState::Quarantined {
            return Err(TaskStoreError::InvalidReconcileState {
                reason: "only a quarantined permit can be adopted",
            });
        }
        if permit.permit_epoch != request.permit_epoch {
            return Err(TaskStoreError::PermitEpochMismatch);
        }
        validate_permit_authority_lease(
            &transaction,
            &permit,
            request.adopted_at_ms,
            authority_lease,
        )?;
        let adoption_epoch = advance_sequence(&transaction, request.task_id, "adoption_epoch")?;
        let record = AdoptionReceiptRecord {
            receipt_id: model::derive_adoption_receipt_id(permit.permit_id, adoption_epoch),
            task_id: task.record.task_id,
            task_generation: task.record.task_generation,
            original_permit_id: permit.permit_id,
            original_permit_epoch: permit.permit_epoch,
            original_control_epoch: permit.control_epoch,
            original_cancel_epoch: permit.cancel_epoch,
            effect_set_root: effect::stored_effect_set_root(&transaction, permit.permit_id)?
                .unwrap_or_else(effect::empty_effect_set_root),
            observed_effect_slot_state_root: effect::load_summary(&transaction, permit.permit_id)?
                .map_or_else(effect::empty_effect_set_root, |stored| {
                    stored.summary.effect_slot_state_root
                }),
            adoption_epoch,
            authority_lease_binding: authority_lease.map(AuthorityLeaseRecord::binding),
            created_at_ms: request.adopted_at_ms,
        };
        let authority_lease_authority_id = record
            .authority_lease_binding
            .map(|binding| binding.authority_id.into_bytes());
        let authority_lease_holder_id = record
            .authority_lease_binding
            .map(|binding| binding.holder_id.into_bytes());
        let authority_lease_term = record
            .authority_lease_binding
            .map(|binding| encode_u64(binding.term));
        let authority_lease_epoch = record
            .authority_lease_binding
            .map(|binding| encode_u64(binding.lease_epoch));
        let authority_lease_fencing_token = record
            .authority_lease_binding
            .map(|binding| binding.fencing_token);
        let authority_lease_expires_at_ms = record
            .authority_lease_binding
            .map(|binding| binding.expires_at_ms);
        transaction.execute(
            "INSERT INTO task_adoption_receipts (
                receipt_id, task_id, task_generation, idempotency_key,
                original_permit_id, original_permit_epoch, original_control_epoch,
                original_cancel_epoch, effect_set_root,
                observed_effect_slot_state_root, adoption_epoch, created_at_ms,
                authority_lease_authority_id, authority_lease_holder_id,
                authority_lease_term, authority_lease_epoch,
                authority_lease_fencing_token, authority_lease_expires_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                       ?13, ?14, ?15, ?16, ?17, ?18)",
            params![
                record.receipt_id.as_bytes().as_slice(),
                record.task_id.as_bytes().as_slice(),
                encode_u64(record.task_generation.get()).as_slice(),
                request.idempotency_key.as_bytes().as_slice(),
                record.original_permit_id.as_bytes().as_slice(),
                encode_u64(record.original_permit_epoch).as_slice(),
                encode_u64(record.original_control_epoch).as_slice(),
                encode_u64(record.original_cancel_epoch).as_slice(),
                record.effect_set_root.as_slice(),
                record.observed_effect_slot_state_root.as_slice(),
                encode_u64(record.adoption_epoch).as_slice(),
                record.created_at_ms,
                authority_lease_authority_id
                    .as_ref()
                    .map(<[u8; 16]>::as_slice),
                authority_lease_holder_id.as_ref().map(<[u8; 16]>::as_slice),
                authority_lease_term.as_ref().map(<[u8; 8]>::as_slice),
                authority_lease_epoch.as_ref().map(<[u8; 8]>::as_slice),
                authority_lease_fencing_token
                    .as_ref()
                    .map(<[u8; 32]>::as_slice),
                authority_lease_expires_at_ms,
            ],
        )?;
        let control_epoch = task
            .record
            .control_epoch
            .checked_add(1)
            .ok_or(TaskStoreError::EpochExhausted)?;
        update_task(&transaction, &task, request.adopted_at_ms, |task_record| {
            task_record.control_epoch = control_epoch;
        })?;
        transaction.commit()?;
        Ok(AdoptionReplay::Adopted(Box::new(record)))
    }

    /// Reconciles one `EffectUnknown` slot of a quarantined permit under a
    /// durable adoption (`[TASK-EFFECT-003]`).
    ///
    /// In one transaction the slot CASes `EffectUnknown → Reconciling`,
    /// the caller-supplied authoritative closure proof is consumed, a
    /// `TaskEffectReconciliationReceipt`-shaped record is written, and the
    /// slot moves to `EffectClosed` (appending the cross-attempt history
    /// entry), `ConfirmedNoEffect` (appending a `ConfirmedNoEffect`
    /// history entry — never a required-slot satisfaction), or back to
    /// `EffectUnknown` (permit stays `Quarantined`). When the last
    /// unknown slot resolves, the tombstone lifts: the permit returns to
    /// `Issued` for the original holder. Reconciling an already-resolved
    /// slot with the same proof replays the original receipt; a different
    /// proof fails closed.
    ///
    /// # Errors
    ///
    /// Returns a not-found, epoch, reconcile-state, replay-conflict, or
    /// storage error.
    pub fn reconcile_effect(
        &self,
        request: ReconcileRequest,
    ) -> Result<ReconcileReplay, TaskStoreError> {
        self.reconcile_effect_inner(request, None)
    }

    /// Reconciles one unknown effect while proving the live lease copied into
    /// the adoption receipt. Exact replay of an already-resolved slot remains
    /// readable without presenting the lease again.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::reconcile_effect`], plus a typed
    /// lease-required, lease-fenced, or lease-expired error.
    #[allow(clippy::needless_pass_by_value)]
    pub fn reconcile_effect_with_authority_lease(
        &self,
        request: AuthorityLeaseReconcileRequest,
    ) -> Result<ReconcileReplay, TaskStoreError> {
        self.reconcile_effect_inner(request.reconcile, Some(request.lease))
    }

    fn reconcile_effect_inner(
        &self,
        request: ReconcileRequest,
        authority_lease: Option<AuthorityLeaseRecord>,
    ) -> Result<ReconcileReplay, TaskStoreError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task = store::load_task(&transaction, request.task_id)?;
        let permit = store::load_permit_by_id(&transaction, request.task_id, request.permit_id)?;
        if permit.permit_epoch != request.permit_epoch {
            return Err(TaskStoreError::PermitEpochMismatch);
        }
        let adoption =
            load_adoption_by_id(&transaction, request.task_id, request.adoption_receipt_id)?
                .ok_or(TaskStoreError::ReceiptNotFound)?;
        if adoption.original_permit_id != permit.permit_id {
            return Err(TaskStoreError::InvalidReconcileState {
                reason: "adoption receipt does not bind this permit",
            });
        }
        let slot = load_slot(&transaction, request.permit_id, request.effect_seq)?;
        if slot.state != SlotState::EffectUnknown {
            return replay_reconcile(&transaction, &permit, &slot, &adoption, &request);
        }
        if adoption.authority_lease_binding != permit.authority_lease_binding {
            return Err(TaskStoreError::AuthorityLeaseBindingMismatch);
        }
        validate_permit_authority_lease(
            &transaction,
            &permit,
            request.reconciled_at_ms,
            authority_lease,
        )?;
        if permit.state != PermitState::Quarantined {
            return Err(TaskStoreError::InvalidReconcileState {
                reason: "permit is not quarantined",
            });
        }
        let (final_state_seq, effect_receipt_id) =
            apply_reconcile_outcome(&transaction, &task, &slot, &request)?;
        refresh_summary(&transaction, request.permit_id, request.reconciled_at_ms)?;
        let slot_state_root_after = effect::load_summary(&transaction, request.permit_id)?
            .ok_or(TaskStoreError::CorruptRecord(
                "effect slot exists without its effect-set control row",
            ))?
            .summary
            .effect_slot_state_root;
        let record = ReconciliationReceiptRecord {
            receipt_id: model::derive_reconcile_receipt_id(slot.effect_slot_id, final_state_seq),
            task_id: request.task_id,
            permit_id: permit.permit_id,
            permit_epoch: permit.permit_epoch,
            permit_adoption_receipt_id: adoption.receipt_id,
            effect_slot_id: slot.effect_slot_id,
            effect_seq: slot.effect_seq,
            logical_effect_id: slot.logical_effect_id,
            retry_fence_epoch: task.record.retry_fence_epoch,
            effect_set_root: effect::stored_effect_set_root(&transaction, permit.permit_id)?
                .unwrap_or_else(effect::empty_effect_set_root),
            outcome: request.outcome,
            closure_proof_digest: request.closure_proof_digest,
            effect_receipt_id,
            effect_slot_state_root_after: slot_state_root_after,
            created_at_ms: request.reconciled_at_ms,
        };
        insert_reconcile_receipt(&transaction, &record)?;
        // When the last unknown slot resolves, the tombstone lifts and the
        // original holder regains finalize/close ability
        // (`[TASK-EFFECT-003]` final clause).
        if count_unknown_slots(&transaction, request.permit_id)? == 0 {
            let changed = transaction.execute(
                "UPDATE commit_permits SET permit_state = ?1, updated_at_ms = ?2
                 WHERE permit_id = ?3 AND permit_state = ?4",
                params![
                    PermitState::Issued.code(),
                    request.reconciled_at_ms,
                    permit.permit_id.as_bytes().as_slice(),
                    PermitState::Quarantined.code(),
                ],
            )?;
            if changed != 1 {
                return Err(TaskStoreError::CorruptRecord(
                    "permit unquarantine compare-and-swap failed",
                ));
            }
        }
        let control_epoch = task
            .record
            .control_epoch
            .checked_add(1)
            .ok_or(TaskStoreError::EpochExhausted)?;
        update_task(
            &transaction,
            &task,
            request.reconciled_at_ms,
            |task_record| {
                task_record.control_epoch = control_epoch;
            },
        )?;
        transaction.commit()?;
        Ok(ReconcileReplay::Reconciled(Box::new(record)))
    }

    /// Reads back a cross-attempt effect-history entry plus the original
    /// authoritative effect receipt (`[TASK-RETRY-EFFECT-001]`).
    ///
    /// # Errors
    ///
    /// Returns a storage error, or `CorruptRecord` if the referenced
    /// original receipt is missing.
    pub fn lookup_effect_history(
        &self,
        task_id: TaskId,
        logical_effect_id: [u8; 32],
    ) -> Result<Option<EffectHistoryLookup>, TaskStoreError> {
        let connection = self.lock_connection()?;
        let mut statement = connection.prepare(&format!(
            "SELECT {HISTORY_COLUMNS} FROM effect_history
             WHERE task_id = ?1 AND logical_effect_id = ?2
             ORDER BY effect_history_seq DESC LIMIT 1"
        ))?;
        let mut rows = statement.query(params![
            task_id.as_bytes().as_slice(),
            logical_effect_id.as_slice(),
        ])?;
        let Some(entry) = rows.next()?.map(decode_history_row).transpose()? else {
            return Ok(None);
        };
        let original_receipt =
            effect::load_effect_receipt(&*connection, entry.authoritative_effect_receipt_id)?;
        Ok(Some(EffectHistoryLookup {
            entry,
            original_receipt,
        }))
    }

    /// Lists the durable effect history of a task in
    /// `effect_history_seq` order.
    ///
    /// # Errors
    ///
    /// Returns a storage error.
    pub fn list_effect_history(
        &self,
        task_id: TaskId,
    ) -> Result<Vec<EffectHistoryEntry>, TaskStoreError> {
        let connection = self.lock_connection()?;
        list_history(&*connection, task_id)
    }

    /// Recomputes the `TaskEffectHistoryRoot` from the durable entries.
    ///
    /// # Errors
    ///
    /// Returns a storage error.
    pub fn compute_effect_history_root(&self, task_id: TaskId) -> Result<[u8; 32], TaskStoreError> {
        let connection = self.lock_connection()?;
        history_root(&*connection, task_id)
    }

    /// Reads the quarantine receipt of a permit, if quarantined.
    ///
    /// # Errors
    ///
    /// Returns a storage error.
    pub fn inspect_quarantine_receipt(
        &self,
        permit_id: CommitPermitId,
    ) -> Result<Option<QuarantineReceiptRecord>, TaskStoreError> {
        let connection = self.lock_connection()?;
        load_quarantine_by_permit(&*connection, permit_id)
    }

    /// Reads one adoption receipt.
    ///
    /// # Errors
    ///
    /// Returns `ReceiptNotFound` or a storage error.
    pub fn inspect_adoption_receipt(
        &self,
        task_id: TaskId,
        receipt_id: ReceiptId,
    ) -> Result<AdoptionReceiptRecord, TaskStoreError> {
        let connection = self.lock_connection()?;
        load_adoption_by_id(&*connection, task_id, receipt_id)?
            .ok_or(TaskStoreError::ReceiptNotFound)
    }

    /// Reads the latest reconcile receipt of a slot, if any.
    ///
    /// # Errors
    ///
    /// Returns a storage error.
    pub fn inspect_reconcile_receipt(
        &self,
        permit_id: CommitPermitId,
        effect_seq: u64,
    ) -> Result<Option<ReconciliationReceiptRecord>, TaskStoreError> {
        let connection = self.lock_connection()?;
        load_latest_reconcile_for_slot(&*connection, permit_id, effect_seq)
    }
}

/// B-TASK-001/002 finalize semantics, preserved bit-for-bit for the
/// legacy `finalize_commit` entry point: any non-terminal slot (including
/// `EffectUnknown`) blocks with `OutstandingEffectSlots` and the permit
/// stays `Issued`; when every declared slot is terminal the permit
/// commits with the caller-supplied roots, and the fence must never
/// regress. The strict `[TASK-COMMIT-002]` proof semantics, quarantine,
/// and retry-fence advancement live in `finalize_commit_v3`.
fn finalize_legacy(
    transaction: &Transaction<'_>,
    task: &StoredTask,
    permit: &PermitRecord,
    attempt: &crate::AttemptRecord,
    request: &FinalizeRequestV3,
) -> Result<FinalizeDecision, TaskStoreError> {
    let blocking = list_slots(transaction, permit.permit_id)?
        .iter()
        .filter(|slot| slot.state.blocks_finalization())
        .count();
    if blocking > 0 {
        return Err(TaskStoreError::OutstandingEffectSlots {
            count: u64::try_from(blocking).unwrap_or(u64::MAX),
        });
    }
    if request.base.new_retry_fence_epoch < task.record.retry_fence_epoch {
        return Err(TaskStoreError::FenceRegression);
    }
    let new_seq = task
        .record
        .head_commit_seq
        .checked_add(1)
        .ok_or(TaskStoreError::EpochExhausted)?;
    let control_epoch = task
        .record
        .control_epoch
        .checked_add(1)
        .ok_or(TaskStoreError::EpochExhausted)?;
    let receipt_id = model::derive_commit_receipt_id(permit.permit_id);
    let receipt = TaskReceiptRecord {
        receipt_id,
        task_id: request.base.task_id,
        permit_id: Some(permit.permit_id),
        attempt_id: request.base.attempt_id,
        attempt_generation: request.base.attempt_generation,
        group_binding: permit.group_binding,
        participant_registry_binding: permit.participant_registry_binding,
        outcome: ReceiptOutcome::Committed,
        prior_head_commit_seq: task.record.head_commit_seq,
        prior_effect_history_root: task.record.head_effect_history_root,
        prior_retry_fence_epoch: task.record.retry_fence_epoch,
        new_head_commit_seq: new_seq,
        new_effect_history_root: request.base.new_effect_history_root,
        new_retry_fence_epoch: request.base.new_retry_fence_epoch,
        created_at_ms: request.base.finalized_at_ms,
    };
    insert_receipt(transaction, &receipt)?;
    close_permit(transaction, permit, request.base.finalized_at_ms)?;
    set_attempt_state(
        transaction,
        attempt,
        AttemptState::Committed,
        Some(receipt_id),
        request.base.finalized_at_ms,
    )?;
    update_task(transaction, task, request.base.finalized_at_ms, |record| {
        record.head_commit_seq = new_seq;
        record.head_effect_history_root = request.base.new_effect_history_root;
        record.retry_fence_epoch = request.base.new_retry_fence_epoch;
        record.control_epoch = control_epoch;
    })?;
    Ok(FinalizeDecision::Committed(Box::new(receipt)))
}

struct RequiredEvaluation {
    satisfied_count: u64,
    unsatisfied: Vec<u64>,
}

/// The `[TASK-RETRY-EFFECT-001]` branch of finalize: at least one
/// required slot is unsatisfied but at least one effect already happened.
/// Appends one `PartialEffect` history entry per unsatisfied required
/// slot, strictly increments the retry fence, and picks the receipt
/// outcome: `FailedAfterEffect` when no required slot was satisfied (the
/// attempt's goal failed), `PartialEffect` otherwise (the commit is
/// partially usable).
fn partial_outcome_and_entries(
    transaction: &Transaction<'_>,
    task: &StoredTask,
    slots: &[SlotRecord],
    evaluation: &RequiredEvaluation,
    now_ms: i64,
) -> Result<(ReceiptOutcome, u64), TaskStoreError> {
    let effects_happened = slots
        .iter()
        .any(|slot| slot.state == SlotState::EffectClosed);
    if !effects_happened {
        return Err(TaskStoreError::RequiredEffectUnsatisfied {
            effect_seq: evaluation.unsatisfied[0],
            reason: "required slot unsatisfied and no effect happened; use close_permit",
        });
    }
    let new_fence = task
        .record
        .retry_fence_epoch
        .checked_add(1)
        .ok_or(TaskStoreError::EpochExhausted)?;
    for effect_seq in &evaluation.unsatisfied {
        let slot = slots
            .iter()
            .find(|slot| slot.effect_seq == *effect_seq)
            .ok_or(TaskStoreError::EffectSlotNotFound)?;
        let receipt_id = slot.effect_receipt_id.ok_or(TaskStoreError::CorruptRecord(
            "terminal slot lacks its receipt",
        ))?;
        append_history_entry(
            transaction,
            &HistoryAppend {
                task_id: task.record.task_id,
                retry_fence_epoch: new_fence,
                slot,
                outcome: EffectHistoryOutcome::PartialEffect,
                authoritative_effect_receipt_id: receipt_id,
                now_ms,
            },
        )?;
    }
    let outcome = if evaluation.satisfied_count > 0 {
        ReceiptOutcome::PartialEffect
    } else {
        ReceiptOutcome::FailedAfterEffect
    };
    Ok((outcome, new_fence))
}

/// Per-slot `[TASK-COMMIT-002]` required evaluation: proofs must cover
/// only required slots, each proof must match the slot's durable terminal
/// state and receipt contents, and plain `NoEffect` /
/// `ConfirmedNoEffect` never satisfy.
fn evaluate_required(
    transaction: &Transaction<'_>,
    slots: &[SlotRecord],
    snapshot_digest: &[u8; 32],
    request: &FinalizeRequestV3,
) -> Result<RequiredEvaluation, TaskStoreError> {
    let mut proofs: std::collections::BTreeMap<u64, RequiredSatisfactionProof> =
        std::collections::BTreeMap::new();
    for satisfaction in &request.required_satisfaction {
        if proofs
            .insert(satisfaction.effect_seq, satisfaction.proof)
            .is_some()
        {
            return Err(TaskStoreError::RequiredEffectUnsatisfied {
                effect_seq: satisfaction.effect_seq,
                reason: "duplicate satisfaction proof for one slot",
            });
        }
    }
    let mut satisfied_count = 0u64;
    let mut unsatisfied = Vec::new();
    for slot in slots.iter().filter(|slot| slot.required) {
        let proof = proofs.remove(&slot.effect_seq);
        match (slot.state, proof) {
            (
                SlotState::EffectClosed,
                Some(RequiredSatisfactionProof::EffectClosedSuccess {
                    success_assertion_digest,
                }),
            ) => {
                validate_effect_closed_success(transaction, slot, success_assertion_digest)?;
                satisfied_count += 1;
            }
            (
                SlotState::NoEffect,
                Some(RequiredSatisfactionProof::ConditionNotApplicable {
                    condition_false_proof_digest,
                }),
            ) => {
                let receipt_id = slot.effect_receipt_id.ok_or(TaskStoreError::CorruptRecord(
                    "no-effect slot lacks its effect receipt",
                ))?;
                let receipt = effect::load_effect_receipt(transaction, receipt_id)?;
                if receipt.no_effect_reason != Some(crate::NoEffectReason::ConditionNotApplicable) {
                    return Err(TaskStoreError::RequiredEffectUnsatisfied {
                        effect_seq: slot.effect_seq,
                        reason: "slot no-effect reason is not CONDITION_NOT_APPLICABLE",
                    });
                }
                let Some(condition_digest) = slot.required_condition_digest else {
                    return Err(TaskStoreError::RequiredEffectUnsatisfied {
                        effect_seq: slot.effect_seq,
                        reason: "condition proof presented for an unconditional required slot",
                    });
                };
                if condition_false_proof_digest
                    != expected_condition_false_proof(snapshot_digest, &condition_digest)
                {
                    return Err(TaskStoreError::RequiredEffectUnsatisfied {
                        effect_seq: slot.effect_seq,
                        reason: "condition-false proof does not match the snapshot-bound digest",
                    });
                }
                satisfied_count += 1;
            }
            (_, None) => unsatisfied.push(slot.effect_seq),
            (state, Some(_)) => {
                let reason = match state {
                    SlotState::NoEffect => "this no-effect reason cannot satisfy a required slot",
                    SlotState::ConfirmedNoEffect => {
                        "CONFIRMED_NO_EFFECT never satisfies a required slot"
                    }
                    SlotState::EffectClosed => {
                        "condition proof presented for an effect-closed slot"
                    }
                    _ => "non-terminal slot cannot satisfy a required slot",
                };
                return Err(TaskStoreError::RequiredEffectUnsatisfied {
                    effect_seq: slot.effect_seq,
                    reason,
                });
            }
        }
    }
    if let Some((effect_seq, _)) = proofs.into_iter().next() {
        return Err(TaskStoreError::RequiredEffectUnsatisfied {
            effect_seq,
            reason: "satisfaction proof presented for a non-required or unknown slot",
        });
    }
    Ok(RequiredEvaluation {
        satisfied_count,
        unsatisfied,
    })
}

fn validate_effect_closed_success(
    transaction: &Transaction<'_>,
    slot: &SlotRecord,
    success_assertion_digest: [u8; 32],
) -> Result<(), TaskStoreError> {
    let receipt_id = slot.effect_receipt_id.ok_or(TaskStoreError::CorruptRecord(
        "effect-closed slot lacks its effect receipt",
    ))?;
    let receipt = effect::load_effect_receipt(transaction, receipt_id)?;
    let matches_slot = receipt.kind == effect::ReceiptKind::EffectClosed
        && receipt.no_effect_reason.is_none()
        && receipt.task_id == slot.task_id
        && receipt.permit_id == slot.permit_id
        && receipt.effect_slot_id == slot.effect_slot_id
        && receipt.effect_seq == slot.effect_seq
        && receipt.logical_effect_id == slot.logical_effect_id;
    if !matches_slot
        || success_assertion_digest != effect::expected_success_assertion_digest(slot, &receipt)
    {
        return Err(TaskStoreError::RequiredEffectUnsatisfied {
            effect_seq: slot.effect_seq,
            reason: "success proof does not match the slot contract and closure receipt",
        });
    }
    Ok(())
}

fn replay_finalize(
    transaction: &Transaction<'_>,
    permit: &PermitRecord,
    request: &FinalizeRequestV3,
) -> Result<FinalizeDecision, TaskStoreError> {
    let receipt = load_receipt_by_permit(transaction, permit.task_id, permit.permit_id)?
        .ok_or(TaskStoreError::PermitNotIssued)?;
    if receipt.participant_registry_binding != permit.participant_registry_binding {
        return Err(TaskStoreError::ParticipantRegistryBindingMismatch);
    }
    if matches!(
        receipt.outcome,
        ReceiptOutcome::FailedBeforeEffect | ReceiptOutcome::CancelledBeforeEffect
    ) {
        return Err(TaskStoreError::PermitNotIssued);
    }
    // Same key + different bytes fails closed, exactly as in
    // B-TASK-001/002: the attempt binding is part of the replay bytes.
    if receipt.attempt_id != request.base.attempt_id
        || receipt.attempt_generation != request.base.attempt_generation
    {
        return Err(TaskStoreError::IdempotencyConflict);
    }
    // Legacy receipts (no declared effect set) compare the caller-supplied
    // roots exactly as in B-TASK-001/002; v3 receipts compare the stored
    // proof digest instead.
    match load_finalize_proof(transaction, receipt.receipt_id)? {
        Some(digest) => {
            if digest != finalize_proof_digest(request) {
                return Err(TaskStoreError::HistoryConflict);
            }
        }
        None => {
            if receipt.new_effect_history_root != request.base.new_effect_history_root
                || receipt.new_retry_fence_epoch != request.base.new_retry_fence_epoch
                || !request.required_satisfaction.is_empty()
            {
                return Err(TaskStoreError::IdempotencyConflict);
            }
        }
    }
    Ok(FinalizeDecision::Replayed(Box::new(receipt)))
}

fn replay_reconcile(
    transaction: &Transaction<'_>,
    permit: &PermitRecord,
    slot: &SlotRecord,
    adoption: &AdoptionReceiptRecord,
    request: &ReconcileRequest,
) -> Result<ReconcileReplay, TaskStoreError> {
    let Some(latest) =
        load_latest_reconcile_for_slot(transaction, permit.permit_id, slot.effect_seq)?
    else {
        return Err(TaskStoreError::InvalidReconcileState {
            reason: "slot was never EFFECT_UNKNOWN on this permit",
        });
    };
    let same_bytes = latest.permit_adoption_receipt_id == adoption.receipt_id
        && latest.outcome == request.outcome
        && latest.closure_proof_digest == request.closure_proof_digest;
    if !same_bytes {
        return Err(TaskStoreError::HistoryConflict);
    }
    Ok(ReconcileReplay::Replayed(Box::new(latest)))
}
