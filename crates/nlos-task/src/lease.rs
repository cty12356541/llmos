//! Durable `TaskAuthority` lease/term primitive for cross-process fencing.
//!
//! This module deliberately owns only the authority lease record. It does not
//! authenticate an IPC peer, perform a distributed consensus decision, or
//! adopt an old `CommitPermit`; those callers remain explicit next gates.

use nlos_types::{IdempotencyKey, ProcessId, TaskId, TaskParticipantId};
use rusqlite::{Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use crate::TaskStoreError;
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
