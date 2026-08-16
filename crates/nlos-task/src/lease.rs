//! Durable `TaskAuthority` lease/term primitive for cross-process fencing.
//!
//! This module owns the authority lease and the local assignment baseline
//! copied into lease-bound Task permits. It does not authenticate an IPC
//! peer, perform a distributed consensus decision, or adopt an old
//! `CommitPermit`; those callers remain explicit next gates.

use std::num::NonZeroU64;

use nlos_types::{
    Generation, IdempotencyKey, ProcessId, ReceiptId, TaskAuthorityAssignmentId, TaskId,
    TaskParticipantId,
};
use rusqlite::{Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use crate::ParticipantRegistryBinding;
use crate::TaskStoreError;
use crate::store::optional_blob;
use crate::store::{SqlRead, SqliteTaskAuthority, blob16, blob32, encode_u64, u64_from_blob};

/// Bounds a single authority lease so a forgotten holder cannot retain a
/// fence indefinitely.
pub const MAX_AUTHORITY_LEASE_TTL_MS: i64 = 86_400_000;

/// Request to acquire, renew, or take over the `TaskAuthority` lease.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuthorityLeaseRequest {
    pub holder_id: ProcessId,
    pub idempotency_key: IdempotencyKey,
    pub requested_at_ms: i64,
    pub ttl_ms: i64,
}

/// A permit/terminal-path binding copied from a durable authority lease.
///
/// The request timestamp, TTL, and idempotency key remain inside the lease
/// record; mutation paths only need the authority identity, holder, current
/// term/epoch, token, and expiry to reject stale writers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuthorityLeaseBinding {
    pub authority_id: TaskParticipantId,
    pub holder_id: ProcessId,
    pub term: u64,
    pub lease_epoch: u64,
    pub fencing_token: [u8; 32],
    pub expires_at_ms: i64,
}

/// Current durable authority lease, including the term and fencing token that
/// downstream authority operations must bind before accepting a mutation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuthorityLeaseRecord {
    pub authority_id: TaskParticipantId,
    pub holder_id: ProcessId,
    pub term: u64,
    pub lease_epoch: u64,
    pub fencing_token: [u8; 32],
    pub requested_at_ms: i64,
    pub expires_at_ms: i64,
    pub ttl_ms: i64,
    pub idempotency_key: IdempotencyKey,
}

/// State of the local durable `TaskAuthority` assignment. A successor is not
/// activated by this slice; takeover only moves the current assignment to
/// `TakeoverPending` in the next fence/receipt gate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthorityAssignmentState {
    Active,
    TakeoverPending,
    Fenced,
}

impl AuthorityAssignmentState {
    pub(crate) const fn code(self) -> i64 {
        match self {
            Self::Active => 1,
            Self::TakeoverPending => 2,
            Self::Fenced => 3,
        }
    }

    pub(crate) fn from_code(code: i64) -> Result<Self, TaskStoreError> {
        match code {
            1 => Ok(Self::Active),
            2 => Ok(Self::TakeoverPending),
            3 => Ok(Self::Fenced),
            _ => Err(TaskStoreError::CorruptRecord("assignment state")),
        }
    }
}

/// Durable local assignment bound to one Task generation and registry
/// baseline. Lease renewal may update the copied live binding; assignment
/// identity remains stable for the same term/registry pair.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuthorityAssignmentRecord {
    pub assignment_id: TaskAuthorityAssignmentId,
    pub task_id: TaskId,
    pub task_generation: Generation,
    pub authority_lease_binding: AuthorityLeaseBinding,
    pub control_epoch: u64,
    pub participant_registry_binding: ParticipantRegistryBinding,
    pub state: AuthorityAssignmentState,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// Local state of a takeover receipt. `Pending` deliberately means that the
/// frozen local fence is durable but no remote endpoint barrier has been
/// attested yet.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthorityTakeoverReceiptState {
    Pending,
}

impl AuthorityTakeoverReceiptState {
    pub(crate) const fn code(self) -> i64 {
        match self {
            Self::Pending => 1,
        }
    }

    pub(crate) fn from_code(code: i64) -> Result<Self, TaskStoreError> {
        match code {
            1 => Ok(Self::Pending),
            _ => Err(TaskStoreError::CorruptRecord("takeover receipt state")),
        }
    }
}

/// Durable local prefix of a `TaskAuthorityTakeoverReceipt`.
///
/// The old assignment is moved to `TakeoverPending` in the same transaction
/// as this record. `new_assignment_id` is required to remain `None` in this
/// slice: no successor registry or assignment is active until a later
/// cross-authority barrier gate consumes all endpoint receipts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuthorityTakeoverReceiptRecord {
    pub receipt_id: ReceiptId,
    pub task_id: TaskId,
    pub task_generation: Generation,
    pub old_assignment_id: TaskAuthorityAssignmentId,
    pub new_assignment_id: Option<TaskAuthorityAssignmentId>,
    pub fence_receipt_id: ReceiptId,
    pub frozen_old_authority_term: u64,
    pub frozen_old_control_epoch: u64,
    pub new_authority_lease_binding: AuthorityLeaseBinding,
    pub new_control_epoch: u64,
    pub frozen_registry_binding: ParticipantRegistryBinding,
    pub exact_fence_set_root: Option<[u8; 32]>,
    pub outstanding_operation_participant_root: Option<[u8; 32]>,
    pub barrier_state: AuthorityTakeoverReceiptState,
    pub created_at_ms: i64,
}

impl AuthorityLeaseRecord {
    #[must_use]
    pub const fn binding(self) -> AuthorityLeaseBinding {
        AuthorityLeaseBinding {
            authority_id: self.authority_id,
            holder_id: self.holder_id,
            term: self.term,
            lease_epoch: self.lease_epoch,
            fencing_token: self.fencing_token,
            expires_at_ms: self.expires_at_ms,
        }
    }
}

pub(crate) fn derive_assignment_id(
    task_id: TaskId,
    task_generation: Generation,
    authority_id: TaskParticipantId,
    authority_term: u64,
    registry_binding: ParticipantRegistryBinding,
) -> TaskAuthorityAssignmentId {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/task-authority-assignment/v1");
    hasher.update(task_id.as_bytes());
    hasher.update(task_generation.get().to_be_bytes());
    hasher.update(authority_id.as_bytes());
    hasher.update(authority_term.to_be_bytes());
    hasher.update(registry_binding.generation.to_be_bytes());
    hasher.update(registry_binding.root);
    let digest: [u8; 32] = hasher.finalize().into();
    TaskAuthorityAssignmentId::from_bytes(digest[..16].try_into().expect("assignment id prefix"))
}

/// Opt-in `CommitPermit` issuance request bound to one durable authority
/// lease. Legacy `PermitRequest` callers remain unbound for compatibility.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorityLeasePermitRequest {
    pub permit: crate::model::PermitRequest,
    pub lease: AuthorityLeaseRecord,
}

/// Opt-in local takeover-fence request.
///
/// This request only freezes the current Task participant registry under a
/// newly validated live lease. It deliberately does not claim to be a
/// `TaskAuthorityAssignment`, `TakeoverReceipt`, or cross-authority barrier
/// proof; those fields remain a later distributed gate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuthorityLeaseTakeoverFenceRequest {
    pub task_id: TaskId,
    pub expected_registry_binding: crate::ParticipantRegistryBinding,
    pub lease: AuthorityLeaseRecord,
    pub requested_at_ms: i64,
}

/// Durable local receipt for the `FROZEN_FOR_TAKEOVER` pre-gate.
///
/// When the local durable write set exposes a complete participant mapping,
/// the two roots are deterministic facts for the frozen registry union and
/// outstanding-operation participant set. `None` means that mapping is not
/// complete; neither form attests a remote endpoint barrier. Assignment/
/// `TakeoverReceipt` activation remains a later distributed gate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuthorityLeaseTakeoverFenceRecord {
    pub receipt_id: ReceiptId,
    pub task_id: TaskId,
    pub task_generation: Generation,
    pub frozen_registry_binding: ParticipantRegistryBinding,
    pub authority_lease_binding: AuthorityLeaseBinding,
    pub control_epoch: u64,
    pub exact_fence_set_root: Option<[u8; 32]>,
    pub outstanding_operation_participant_root: Option<[u8; 32]>,
    pub created_at_ms: i64,
}

/// Idempotent lease transition result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthorityLeaseDecision {
    Acquired(AuthorityLeaseRecord),
    Renewed(AuthorityLeaseRecord),
    TakenOver(AuthorityLeaseRecord),
    Replayed(AuthorityLeaseRecord),
}

impl AuthorityLeaseDecision {
    #[must_use]
    pub const fn record(self) -> AuthorityLeaseRecord {
        match self {
            Self::Acquired(record)
            | Self::Renewed(record)
            | Self::TakenOver(record)
            | Self::Replayed(record) => record,
        }
    }
}

impl SqliteTaskAuthority {
    /// Acquires, renews, or takes over the durable `TaskAuthority` lease.
    ///
    /// A live lease may only be renewed by its current holder. Once expired,
    /// a new holder advances the term and fencing epoch in one `SQLite`
    /// transaction; the previous token can never validate again.
    ///
    /// # Errors
    ///
    /// Returns a typed lease conflict, invalid request, storage, or
    /// monotonic-epoch error.
    #[allow(clippy::too_many_lines)] // One transaction keeps lease CAS and history atomic.
    pub fn acquire_authority_lease(
        &self,
        request: AuthorityLeaseRequest,
    ) -> Result<AuthorityLeaseDecision, TaskStoreError> {
        let expires_at_ms = validate_request(&request)?;
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let authority_id = load_authority_id(&transaction)?;
        if let Some(history) =
            load_lease_history_by_key(&transaction, authority_id, request.idempotency_key)?
        {
            if history.holder_id == request.holder_id
                && history.requested_at_ms == request.requested_at_ms
                && history.ttl_ms == request.ttl_ms
                && history.expires_at_ms == expires_at_ms
            {
                transaction.commit()?;
                return Ok(AuthorityLeaseDecision::Replayed(history));
            }
            return Err(TaskStoreError::InvalidAuthorityLease {
                reason: "idempotency key conflicts with durable lease bytes",
            });
        }
        let current = load_lease_optional(&transaction, authority_id)?;

        if let Some(current) = current {
            let (term, transition) = if current.expires_at_ms > request.requested_at_ms {
                if current.holder_id != request.holder_id {
                    return Err(TaskStoreError::AuthorityLeaseHeld);
                }
                (current.term, LeaseTransition::Renewed)
            } else {
                (
                    current
                        .term
                        .checked_add(1)
                        .ok_or(TaskStoreError::EpochExhausted)?,
                    LeaseTransition::TakenOver,
                )
            };
            let lease_epoch = current
                .lease_epoch
                .checked_add(1)
                .ok_or(TaskStoreError::EpochExhausted)?;
            let record = AuthorityLeaseRecord {
                authority_id,
                holder_id: request.holder_id,
                term,
                lease_epoch,
                fencing_token: derive_fencing_token(
                    authority_id,
                    request.holder_id,
                    term,
                    lease_epoch,
                    expires_at_ms,
                    request.idempotency_key,
                ),
                requested_at_ms: request.requested_at_ms,
                expires_at_ms,
                ttl_ms: request.ttl_ms,
                idempotency_key: request.idempotency_key,
            };
            insert_lease_history(&transaction, &record, transition.code())?;
            let changed = transaction.execute(
                "UPDATE task_authority_leases
                 SET holder_id = ?1, term = ?2, lease_epoch = ?3,
                     fencing_token = ?4, requested_at_ms = ?5,
                     expires_at_ms = ?6, ttl_ms = ?7, idempotency_key = ?8
                 WHERE authority_id = ?9 AND term = ?10 AND lease_epoch = ?11
                   AND holder_id = ?12 AND fencing_token = ?13",
                params![
                    record.holder_id.as_bytes().as_slice(),
                    encode_u64(record.term).as_slice(),
                    encode_u64(record.lease_epoch).as_slice(),
                    record.fencing_token.as_slice(),
                    record.requested_at_ms,
                    record.expires_at_ms,
                    record.ttl_ms,
                    record.idempotency_key.as_bytes().as_slice(),
                    current.authority_id.as_bytes().as_slice(),
                    encode_u64(current.term).as_slice(),
                    encode_u64(current.lease_epoch).as_slice(),
                    current.holder_id.as_bytes().as_slice(),
                    current.fencing_token.as_slice(),
                ],
            )?;
            if changed != 1 {
                return Err(TaskStoreError::AuthorityLeaseFenced);
            }
            transaction.commit()?;
            return Ok(match transition {
                LeaseTransition::Renewed => AuthorityLeaseDecision::Renewed(record),
                LeaseTransition::TakenOver => AuthorityLeaseDecision::TakenOver(record),
                LeaseTransition::Acquired => unreachable!("existing lease cannot be acquired"),
            });
        }

        let record = AuthorityLeaseRecord {
            authority_id,
            holder_id: request.holder_id,
            term: 1,
            lease_epoch: 1,
            fencing_token: derive_fencing_token(
                authority_id,
                request.holder_id,
                1,
                1,
                expires_at_ms,
                request.idempotency_key,
            ),
            requested_at_ms: request.requested_at_ms,
            expires_at_ms,
            ttl_ms: request.ttl_ms,
            idempotency_key: request.idempotency_key,
        };
        transaction.execute(
            "INSERT INTO task_authority_leases (
                authority_id, holder_id, term, lease_epoch, fencing_token,
                requested_at_ms, expires_at_ms, ttl_ms, idempotency_key
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                record.authority_id.as_bytes().as_slice(),
                record.holder_id.as_bytes().as_slice(),
                encode_u64(record.term).as_slice(),
                encode_u64(record.lease_epoch).as_slice(),
                record.fencing_token.as_slice(),
                record.requested_at_ms,
                record.expires_at_ms,
                record.ttl_ms,
                record.idempotency_key.as_bytes().as_slice(),
            ],
        )?;
        insert_lease_history(&transaction, &record, LeaseTransition::Acquired.code())?;
        transaction.commit()?;
        Ok(AuthorityLeaseDecision::Acquired(record))
    }

    /// Reads the current authority lease after restart.
    ///
    /// # Errors
    ///
    /// Returns `AuthorityLeaseNotFound` when the authority has not acquired
    /// its first lease, or a storage/corruption error.
    pub fn inspect_authority_lease(&self) -> Result<AuthorityLeaseRecord, TaskStoreError> {
        let connection = self.lock_connection()?;
        let authority_id = load_authority_id(&*connection)?;
        load_lease_optional(&*connection, authority_id)?
            .ok_or(TaskStoreError::AuthorityLeaseNotFound)
    }

    /// Validates a previously issued lease against the current term, epoch,
    /// fencing token, and wall-clock expiry.
    ///
    /// # Errors
    ///
    /// Returns a typed stale-term, expired, missing, invalid-timestamp, or
    /// storage error.
    pub fn validate_authority_lease(
        &self,
        lease: AuthorityLeaseRecord,
        now_ms: i64,
    ) -> Result<(), TaskStoreError> {
        if now_ms < 0 {
            return Err(TaskStoreError::InvalidAuthorityLease {
                reason: "validation timestamp must be non-negative",
            });
        }
        let connection = self.lock_connection()?;
        let current = load_lease_optional(&*connection, lease.authority_id)?
            .ok_or(TaskStoreError::AuthorityLeaseNotFound)?;
        if current != lease {
            return Err(TaskStoreError::AuthorityLeaseFenced);
        }
        if lease.expires_at_ms <= now_ms {
            return Err(TaskStoreError::AuthorityLeaseExpired);
        }
        Ok(())
    }
}

pub(crate) fn validate_authority_lease_binding_in_transaction(
    transaction: &Transaction<'_>,
    binding: AuthorityLeaseBinding,
    now_ms: i64,
) -> Result<(), TaskStoreError> {
    if now_ms < 0 {
        return Err(TaskStoreError::InvalidAuthorityLease {
            reason: "validation timestamp must be non-negative",
        });
    }
    let authority_id = load_authority_id(transaction)?;
    if authority_id != binding.authority_id {
        return Err(TaskStoreError::AuthorityLeaseFenced);
    }
    let current = load_lease_optional(transaction, authority_id)?
        .ok_or(TaskStoreError::AuthorityLeaseNotFound)?;
    if current.binding() != binding {
        return Err(TaskStoreError::AuthorityLeaseFenced);
    }
    if binding.expires_at_ms <= now_ms {
        return Err(TaskStoreError::AuthorityLeaseExpired);
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum LeaseTransition {
    Acquired,
    Renewed,
    TakenOver,
}

impl LeaseTransition {
    const fn code(self) -> i64 {
        match self {
            Self::Acquired => 1,
            Self::Renewed => 2,
            Self::TakenOver => 3,
        }
    }
}

fn validate_request(request: &AuthorityLeaseRequest) -> Result<i64, TaskStoreError> {
    if request.requested_at_ms < 0 {
        return Err(TaskStoreError::InvalidAuthorityLease {
            reason: "request timestamp must be non-negative",
        });
    }
    if request.ttl_ms <= 0 || request.ttl_ms > MAX_AUTHORITY_LEASE_TTL_MS {
        return Err(TaskStoreError::InvalidAuthorityLease {
            reason: "lease TTL is outside the bounded positive range",
        });
    }
    request
        .requested_at_ms
        .checked_add(request.ttl_ms)
        .ok_or(TaskStoreError::EpochExhausted)
}

fn load_authority_id(source: &impl SqlRead) -> Result<TaskParticipantId, TaskStoreError> {
    let mut statement = source.prepare_statement(
        "SELECT participant_id FROM task_authority_identity WHERE singleton = 1",
    )?;
    let mut rows = statement.query([])?;
    let row = rows.next()?.ok_or(TaskStoreError::CorruptRecord(
        "missing TaskAuthority identity",
    ))?;
    Ok(TaskParticipantId::from_bytes(blob16(row, 0)?))
}

fn derive_fencing_token(
    authority_id: TaskParticipantId,
    holder_id: ProcessId,
    term: u64,
    lease_epoch: u64,
    expires_at_ms: i64,
    idempotency_key: IdempotencyKey,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/task-authority-lease/v1");
    hasher.update(authority_id.as_bytes());
    hasher.update(holder_id.as_bytes());
    hasher.update(term.to_be_bytes());
    hasher.update(lease_epoch.to_be_bytes());
    hasher.update(expires_at_ms.to_be_bytes());
    hasher.update(idempotency_key.as_bytes());
    hasher.finalize().into()
}

fn insert_lease_history(
    transaction: &Transaction<'_>,
    record: &AuthorityLeaseRecord,
    transition_kind: i64,
) -> Result<(), TaskStoreError> {
    transaction.execute(
        "INSERT INTO task_authority_lease_history (
            authority_id, lease_epoch, term, holder_id, fencing_token,
            idempotency_key, requested_at_ms, expires_at_ms, ttl_ms,
            transition_kind
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            record.authority_id.as_bytes().as_slice(),
            encode_u64(record.lease_epoch).as_slice(),
            encode_u64(record.term).as_slice(),
            record.holder_id.as_bytes().as_slice(),
            record.fencing_token.as_slice(),
            record.idempotency_key.as_bytes().as_slice(),
            record.requested_at_ms,
            record.expires_at_ms,
            record.ttl_ms,
            transition_kind,
        ],
    )?;
    Ok(())
}

fn load_lease_optional(
    source: &impl SqlRead,
    authority_id: TaskParticipantId,
) -> Result<Option<AuthorityLeaseRecord>, TaskStoreError> {
    let mut statement = source.prepare_statement(
        "SELECT authority_id, holder_id, term, lease_epoch, fencing_token,
                requested_at_ms, expires_at_ms, ttl_ms, idempotency_key
         FROM task_authority_leases WHERE authority_id = ?1",
    )?;
    let mut rows = statement.query([authority_id.as_bytes().as_slice()])?;
    rows.next()?.map(decode_lease_row).transpose()
}

fn load_lease_history_by_key(
    source: &impl SqlRead,
    authority_id: TaskParticipantId,
    idempotency_key: IdempotencyKey,
) -> Result<Option<AuthorityLeaseRecord>, TaskStoreError> {
    let mut statement = source.prepare_statement(
        "SELECT authority_id, holder_id, term, lease_epoch, fencing_token,
                requested_at_ms, expires_at_ms, ttl_ms, idempotency_key
         FROM task_authority_lease_history
         WHERE authority_id = ?1 AND idempotency_key = ?2",
    )?;
    let mut rows = statement.query(params![
        authority_id.as_bytes().as_slice(),
        idempotency_key.as_bytes().as_slice(),
    ])?;
    rows.next()?.map(decode_lease_row).transpose()
}

fn decode_lease_row(row: &rusqlite::Row<'_>) -> Result<AuthorityLeaseRecord, TaskStoreError> {
    Ok(AuthorityLeaseRecord {
        authority_id: TaskParticipantId::from_bytes(blob16(row, 0)?),
        holder_id: ProcessId::from_bytes(blob16(row, 1)?),
        term: u64_from_blob(row, 2)?,
        lease_epoch: u64_from_blob(row, 3)?,
        fencing_token: blob32(row, 4)?,
        requested_at_ms: row.get(5)?,
        expires_at_ms: row.get(6)?,
        ttl_ms: row.get(7)?,
        idempotency_key: IdempotencyKey::from_bytes(blob16(row, 8)?),
    })
}

pub(crate) fn load_current_assignment(
    source: &impl SqlRead,
    task_id: TaskId,
) -> Result<Option<AuthorityAssignmentRecord>, TaskStoreError> {
    let mut statement = source.prepare_statement(
        "SELECT assignment_id, task_id, task_generation,
                authority_id, authority_lease_holder_id,
                authority_lease_term, authority_lease_epoch,
                authority_lease_fencing_token, authority_lease_expires_at_ms,
                control_epoch, participant_registry_generation,
                participant_registry_root, assignment_state,
                created_at_ms, updated_at_ms
         FROM task_authority_assignments
         WHERE task_id = ?1
         ORDER BY created_at_ms DESC, assignment_id DESC
         LIMIT 1",
    )?;
    let mut rows = statement.query([task_id.as_bytes().as_slice()])?;
    rows.next()?.map(decode_assignment_row).transpose()
}

pub(crate) fn insert_assignment(
    transaction: &Transaction<'_>,
    record: &AuthorityAssignmentRecord,
) -> Result<(), TaskStoreError> {
    transaction.execute(
        "INSERT INTO task_authority_assignments (
            assignment_id, task_id, task_generation, authority_id,
            authority_lease_holder_id, authority_lease_term,
            authority_lease_epoch, authority_lease_fencing_token,
            authority_lease_expires_at_ms, control_epoch,
            participant_registry_generation, participant_registry_root,
            assignment_state, created_at_ms, updated_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                   ?11, ?12, ?13, ?14, ?15)",
        params![
            record.assignment_id.as_bytes().as_slice(),
            record.task_id.as_bytes().as_slice(),
            encode_u64(record.task_generation.get()).as_slice(),
            record
                .authority_lease_binding
                .authority_id
                .as_bytes()
                .as_slice(),
            record
                .authority_lease_binding
                .holder_id
                .as_bytes()
                .as_slice(),
            encode_u64(record.authority_lease_binding.term).as_slice(),
            encode_u64(record.authority_lease_binding.lease_epoch).as_slice(),
            record.authority_lease_binding.fencing_token.as_slice(),
            record.authority_lease_binding.expires_at_ms,
            encode_u64(record.control_epoch).as_slice(),
            encode_u64(record.participant_registry_binding.generation).as_slice(),
            record.participant_registry_binding.root.as_slice(),
            record.state.code(),
            record.created_at_ms,
            record.updated_at_ms,
        ],
    )?;
    Ok(())
}

pub(crate) fn refresh_active_assignment(
    transaction: &Transaction<'_>,
    record: &AuthorityAssignmentRecord,
    lease: AuthorityLeaseBinding,
    control_epoch: u64,
    now_ms: i64,
) -> Result<AuthorityAssignmentRecord, TaskStoreError> {
    let changed = transaction.execute(
        "UPDATE task_authority_assignments
         SET authority_lease_holder_id = ?1,
             authority_lease_epoch = ?2,
             authority_lease_fencing_token = ?3,
             authority_lease_expires_at_ms = ?4,
             control_epoch = ?5,
             updated_at_ms = ?6
         WHERE assignment_id = ?7 AND assignment_state = ?8",
        params![
            lease.holder_id.as_bytes().as_slice(),
            encode_u64(lease.lease_epoch).as_slice(),
            lease.fencing_token.as_slice(),
            lease.expires_at_ms,
            encode_u64(control_epoch).as_slice(),
            now_ms,
            record.assignment_id.as_bytes().as_slice(),
            AuthorityAssignmentState::Active.code(),
        ],
    )?;
    if changed != 1 {
        return Err(TaskStoreError::AuthorityLeaseFenced);
    }
    Ok(AuthorityAssignmentRecord {
        authority_lease_binding: lease,
        control_epoch,
        updated_at_ms: now_ms,
        ..*record
    })
}

pub(crate) fn mark_assignment_takeover_pending(
    transaction: &Transaction<'_>,
    record: &AuthorityAssignmentRecord,
    now_ms: i64,
) -> Result<AuthorityAssignmentRecord, TaskStoreError> {
    let changed = transaction.execute(
        "UPDATE task_authority_assignments
         SET assignment_state = ?1, updated_at_ms = ?2
         WHERE assignment_id = ?3 AND assignment_state = ?4",
        params![
            AuthorityAssignmentState::TakeoverPending.code(),
            now_ms,
            record.assignment_id.as_bytes().as_slice(),
            AuthorityAssignmentState::Active.code(),
        ],
    )?;
    if changed != 1 {
        return Err(TaskStoreError::AuthorityLeaseFenced);
    }
    Ok(AuthorityAssignmentRecord {
        state: AuthorityAssignmentState::TakeoverPending,
        updated_at_ms: now_ms,
        ..*record
    })
}

fn decode_assignment_row(
    row: &rusqlite::Row<'_>,
) -> Result<AuthorityAssignmentRecord, TaskStoreError> {
    let task_generation = Generation::new(
        NonZeroU64::new(u64_from_blob(row, 2)?)
            .ok_or(TaskStoreError::CorruptRecord("assignment task generation"))?,
    );
    Ok(AuthorityAssignmentRecord {
        assignment_id: TaskAuthorityAssignmentId::from_bytes(blob16(row, 0)?),
        task_id: TaskId::from_bytes(blob16(row, 1)?),
        task_generation,
        authority_lease_binding: AuthorityLeaseBinding {
            authority_id: TaskParticipantId::from_bytes(blob16(row, 3)?),
            holder_id: ProcessId::from_bytes(blob16(row, 4)?),
            term: u64_from_blob(row, 5)?,
            lease_epoch: u64_from_blob(row, 6)?,
            fencing_token: blob32(row, 7)?,
            expires_at_ms: row.get(8)?,
        },
        control_epoch: u64_from_blob(row, 9)?,
        participant_registry_binding: ParticipantRegistryBinding {
            generation: u64_from_blob(row, 10)?,
            root: blob32(row, 11)?,
        },
        state: AuthorityAssignmentState::from_code(row.get(12)?)?,
        created_at_ms: row.get(13)?,
        updated_at_ms: row.get(14)?,
    })
}

pub(crate) fn derive_takeover_fence_receipt_id(
    task_id: TaskId,
    task_generation: Generation,
    registry_binding: ParticipantRegistryBinding,
    lease_binding: AuthorityLeaseBinding,
) -> ReceiptId {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/task-authority-takeover-fence/v1");
    hasher.update(task_id.as_bytes());
    hasher.update(task_generation.get().to_be_bytes());
    hasher.update(registry_binding.generation.to_be_bytes());
    hasher.update(registry_binding.root);
    hasher.update(lease_binding.authority_id.as_bytes());
    hasher.update(lease_binding.holder_id.as_bytes());
    hasher.update(lease_binding.term.to_be_bytes());
    hasher.update(lease_binding.lease_epoch.to_be_bytes());
    hasher.update(lease_binding.fencing_token);
    hasher.update(lease_binding.expires_at_ms.to_be_bytes());
    let digest: [u8; 32] = hasher.finalize().into();
    ReceiptId::from_bytes(digest[..16].try_into().expect("receipt id prefix"))
}

pub(crate) fn derive_takeover_receipt_id(
    task_id: TaskId,
    task_generation: Generation,
    old_assignment_id: TaskAuthorityAssignmentId,
    fence_receipt_id: ReceiptId,
    new_authority_lease_binding: AuthorityLeaseBinding,
    new_control_epoch: u64,
) -> ReceiptId {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/task-authority-takeover/v1");
    hasher.update(task_id.as_bytes());
    hasher.update(task_generation.get().to_be_bytes());
    hasher.update(old_assignment_id.as_bytes());
    hasher.update(fence_receipt_id.as_bytes());
    hasher.update(new_authority_lease_binding.authority_id.as_bytes());
    hasher.update(new_authority_lease_binding.holder_id.as_bytes());
    hasher.update(new_authority_lease_binding.term.to_be_bytes());
    hasher.update(new_authority_lease_binding.lease_epoch.to_be_bytes());
    hasher.update(new_authority_lease_binding.fencing_token);
    hasher.update(new_authority_lease_binding.expires_at_ms.to_be_bytes());
    hasher.update(new_control_epoch.to_be_bytes());
    let digest: [u8; 32] = hasher.finalize().into();
    ReceiptId::from_bytes(digest[..16].try_into().expect("takeover receipt id prefix"))
}

pub(crate) fn load_takeover_receipt(
    source: &impl SqlRead,
    task_id: TaskId,
    fence_receipt_id: ReceiptId,
) -> Result<Option<AuthorityTakeoverReceiptRecord>, TaskStoreError> {
    let mut statement = source.prepare_statement(
        "SELECT receipt_id, task_id, task_generation,
                old_assignment_id, new_assignment_id, fence_receipt_id,
                frozen_old_authority_term, frozen_old_control_epoch,
                new_authority_id, new_authority_lease_holder_id,
                new_authority_lease_term, new_authority_lease_epoch,
                new_authority_lease_fencing_token,
                new_authority_lease_expires_at_ms, new_control_epoch,
                frozen_registry_generation, frozen_registry_root,
                exact_fence_set_root, outstanding_operation_participant_root,
                barrier_state, created_at_ms
         FROM task_authority_takeover_receipts
         WHERE task_id = ?1 AND fence_receipt_id = ?2",
    )?;
    let mut rows = statement.query(params![
        task_id.as_bytes().as_slice(),
        fence_receipt_id.as_bytes().as_slice(),
    ])?;
    rows.next()?.map(decode_takeover_receipt_row).transpose()
}

pub(crate) fn insert_takeover_receipt(
    transaction: &Transaction<'_>,
    record: &AuthorityTakeoverReceiptRecord,
) -> Result<(), TaskStoreError> {
    transaction.execute(
        "INSERT INTO task_authority_takeover_receipts (
            receipt_id, task_id, task_generation,
            old_assignment_id, new_assignment_id, fence_receipt_id,
            frozen_old_authority_term, frozen_old_control_epoch,
            new_authority_id, new_authority_lease_holder_id,
            new_authority_lease_term, new_authority_lease_epoch,
            new_authority_lease_fencing_token,
            new_authority_lease_expires_at_ms, new_control_epoch,
            frozen_registry_generation, frozen_registry_root,
            exact_fence_set_root, outstanding_operation_participant_root,
            barrier_state, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                   ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21)",
        params![
            record.receipt_id.as_bytes().as_slice(),
            record.task_id.as_bytes().as_slice(),
            encode_u64(record.task_generation.get()).as_slice(),
            record.old_assignment_id.as_bytes().as_slice(),
            record
                .new_assignment_id
                .map(|assignment_id| assignment_id.into_bytes().to_vec()),
            record.fence_receipt_id.as_bytes().as_slice(),
            encode_u64(record.frozen_old_authority_term).as_slice(),
            encode_u64(record.frozen_old_control_epoch).as_slice(),
            record
                .new_authority_lease_binding
                .authority_id
                .as_bytes()
                .as_slice(),
            record
                .new_authority_lease_binding
                .holder_id
                .as_bytes()
                .as_slice(),
            encode_u64(record.new_authority_lease_binding.term).as_slice(),
            encode_u64(record.new_authority_lease_binding.lease_epoch).as_slice(),
            record.new_authority_lease_binding.fencing_token.as_slice(),
            record.new_authority_lease_binding.expires_at_ms,
            encode_u64(record.new_control_epoch).as_slice(),
            encode_u64(record.frozen_registry_binding.generation).as_slice(),
            record.frozen_registry_binding.root.as_slice(),
            record
                .exact_fence_set_root
                .as_ref()
                .map(<[u8; 32]>::as_slice),
            record
                .outstanding_operation_participant_root
                .as_ref()
                .map(<[u8; 32]>::as_slice),
            record.barrier_state.code(),
            record.created_at_ms,
        ],
    )?;
    Ok(())
}

fn decode_takeover_receipt_row(
    row: &rusqlite::Row<'_>,
) -> Result<AuthorityTakeoverReceiptRecord, TaskStoreError> {
    let task_generation = Generation::new(NonZeroU64::new(u64_from_blob(row, 2)?).ok_or(
        TaskStoreError::CorruptRecord("takeover receipt task generation"),
    )?);
    Ok(AuthorityTakeoverReceiptRecord {
        receipt_id: ReceiptId::from_bytes(blob16(row, 0)?),
        task_id: TaskId::from_bytes(blob16(row, 1)?),
        task_generation,
        old_assignment_id: TaskAuthorityAssignmentId::from_bytes(blob16(row, 3)?),
        new_assignment_id: optional_blob::<16>(row, 4)?.map(TaskAuthorityAssignmentId::from_bytes),
        fence_receipt_id: ReceiptId::from_bytes(blob16(row, 5)?),
        frozen_old_authority_term: u64_from_blob(row, 6)?,
        frozen_old_control_epoch: u64_from_blob(row, 7)?,
        new_authority_lease_binding: AuthorityLeaseBinding {
            authority_id: TaskParticipantId::from_bytes(blob16(row, 8)?),
            holder_id: ProcessId::from_bytes(blob16(row, 9)?),
            term: u64_from_blob(row, 10)?,
            lease_epoch: u64_from_blob(row, 11)?,
            fencing_token: blob32(row, 12)?,
            expires_at_ms: row.get(13)?,
        },
        new_control_epoch: u64_from_blob(row, 14)?,
        frozen_registry_binding: ParticipantRegistryBinding {
            generation: u64_from_blob(row, 15)?,
            root: blob32(row, 16)?,
        },
        exact_fence_set_root: optional_blob::<32>(row, 17)?,
        outstanding_operation_participant_root: optional_blob::<32>(row, 18)?,
        barrier_state: AuthorityTakeoverReceiptState::from_code(row.get(19)?)?,
        created_at_ms: row.get(20)?,
    })
}

pub(crate) fn load_takeover_fence_receipt(
    source: &impl SqlRead,
    task_id: TaskId,
    registry_binding: ParticipantRegistryBinding,
) -> Result<Option<AuthorityLeaseTakeoverFenceRecord>, TaskStoreError> {
    let mut statement = source.prepare_statement(
        "SELECT receipt_id, task_id, task_generation,
                frozen_registry_generation, frozen_registry_root,
                authority_lease_authority_id, authority_lease_holder_id,
                authority_lease_term, authority_lease_epoch,
                authority_lease_fencing_token, authority_lease_expires_at_ms,
                control_epoch, exact_fence_set_root,
                outstanding_operation_participant_root, created_at_ms
         FROM task_authority_takeover_fence_receipts
         WHERE task_id = ?1 AND frozen_registry_generation = ?2
           AND frozen_registry_root = ?3",
    )?;
    let mut rows = statement.query(params![
        task_id.as_bytes().as_slice(),
        encode_u64(registry_binding.generation).as_slice(),
        registry_binding.root.as_slice(),
    ])?;
    rows.next()?.map(decode_takeover_fence_row).transpose()
}

pub(crate) fn insert_takeover_fence_receipt(
    transaction: &Transaction<'_>,
    record: &AuthorityLeaseTakeoverFenceRecord,
) -> Result<(), TaskStoreError> {
    transaction.execute(
        "INSERT INTO task_authority_takeover_fence_receipts (
            receipt_id, task_id, task_generation,
            frozen_registry_generation, frozen_registry_root,
            authority_lease_authority_id, authority_lease_holder_id,
            authority_lease_term, authority_lease_epoch,
            authority_lease_fencing_token, authority_lease_expires_at_ms,
            control_epoch, exact_fence_set_root,
            outstanding_operation_participant_root, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10,
                   ?11, ?12, ?13, ?14, ?15)",
        params![
            record.receipt_id.as_bytes().as_slice(),
            record.task_id.as_bytes().as_slice(),
            encode_u64(record.task_generation.get()).as_slice(),
            encode_u64(record.frozen_registry_binding.generation).as_slice(),
            record.frozen_registry_binding.root.as_slice(),
            record
                .authority_lease_binding
                .authority_id
                .as_bytes()
                .as_slice(),
            record
                .authority_lease_binding
                .holder_id
                .as_bytes()
                .as_slice(),
            encode_u64(record.authority_lease_binding.term).as_slice(),
            encode_u64(record.authority_lease_binding.lease_epoch).as_slice(),
            record.authority_lease_binding.fencing_token.as_slice(),
            record.authority_lease_binding.expires_at_ms,
            encode_u64(record.control_epoch).as_slice(),
            record
                .exact_fence_set_root
                .as_ref()
                .map(<[u8; 32]>::as_slice),
            record
                .outstanding_operation_participant_root
                .as_ref()
                .map(<[u8; 32]>::as_slice),
            record.created_at_ms,
        ],
    )?;
    Ok(())
}

fn decode_takeover_fence_row(
    row: &rusqlite::Row<'_>,
) -> Result<AuthorityLeaseTakeoverFenceRecord, TaskStoreError> {
    let task_generation = Generation::new(NonZeroU64::new(u64_from_blob(row, 2)?).ok_or(
        TaskStoreError::CorruptRecord("takeover fence task generation"),
    )?);
    let authority_lease_binding = AuthorityLeaseBinding {
        authority_id: TaskParticipantId::from_bytes(blob16(row, 5)?),
        holder_id: ProcessId::from_bytes(blob16(row, 6)?),
        term: u64_from_blob(row, 7)?,
        lease_epoch: u64_from_blob(row, 8)?,
        fencing_token: blob32(row, 9)?,
        expires_at_ms: row.get(10)?,
    };
    Ok(AuthorityLeaseTakeoverFenceRecord {
        receipt_id: ReceiptId::from_bytes(blob16(row, 0)?),
        task_id: TaskId::from_bytes(blob16(row, 1)?),
        task_generation,
        frozen_registry_binding: ParticipantRegistryBinding {
            generation: u64_from_blob(row, 3)?,
            root: blob32(row, 4)?,
        },
        authority_lease_binding,
        control_epoch: u64_from_blob(row, 11)?,
        exact_fence_set_root: optional_blob::<32>(row, 12)?,
        outstanding_operation_participant_root: optional_blob::<32>(row, 13)?,
        created_at_ms: row.get(14)?,
    })
}
