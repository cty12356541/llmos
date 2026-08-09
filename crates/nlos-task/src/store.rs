//! Single-writer `SQLite` implementation of the durable task authority.
//!
//! The process-local mutex is an admission gate only; `BEGIN IMMEDIATE`
//! remains the storage-level writer fence, identical to `nlos-store`. Every
//! linearized decision (permit CAS, cancellation, finalize) commits its
//! state transition, epoch advance, and receipt in one transaction, so a
//! crash cannot split a decision from its durable record.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use nlos_types::{
    CancellationScopeId, CommitPermitId, Generation, IdempotencyKey, ReceiptId, TaskAttemptId,
    TaskId, TaskSnapshotId,
};
use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior, params};

use crate::model::{derive_closure_receipt_id, derive_permit_id, empty_effect_history_root};
use crate::{
    AttemptHandle, AttemptRecord, AttemptRegistrationDecision, AttemptSpec, AttemptState,
    CancelDecision, CancelRequest, ClosedAttempt, PermitConflict, PermitDecision, PermitRecord,
    PermitRequest, PermitState, ReceiptOutcome, SnapshotBundle, TaskReceiptRecord, TaskRecord,
    TaskRegistrationDecision, TaskSpec, TaskState, TaskStoreError,
};

const SCHEMA_VERSION: i64 = 6;

/// A single-writer `SQLite` task authority.
///
/// All mutating APIs are linearized through one `BEGIN IMMEDIATE`
/// transaction per call, which serializes cancel, permit issuance, and
/// finalize on the same control/cancel/permit epochs
/// (`[TASK-CANCEL-003]`).
pub struct SqliteTaskAuthority {
    connection: Mutex<Connection>,
}

pub(crate) struct StoredTask {
    pub(crate) record: TaskRecord,
    revision: i64,
}

struct StoredCancel {
    idempotency_key: IdempotencyKey,
    cancel_epoch_after: u64,
}

impl SqliteTaskAuthority {
    /// Opens or creates a task authority database and validates its schema.
    ///
    /// Equivalent to [`SqliteTaskAuthority::open_with_vfs`] with `None`,
    /// i.e. the process-default `SQLite` VFS.
    ///
    /// # Errors
    ///
    /// Returns an error when the database cannot be opened, when WAL/FULL
    /// durability cannot be established (verified by reading the pragmas
    /// back; a silent fallback is rejected with
    /// [`TaskStoreError::DurabilityUnavailable`]), or when the stored schema
    /// version cannot be migrated or validated.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, TaskStoreError> {
        Self::open_with_vfs(path, None)
    }

    /// Opens or creates a task authority database through a named
    /// `SQLite` VFS.
    ///
    /// `vfs = None` uses the process-default VFS; `Some(name)` selects a VFS
    /// previously registered under that name (e.g. a fault-injection shim
    /// registered by tests). The open flags are identical to
    /// [`Connection::open`] regardless of the chosen VFS.
    ///
    /// # Errors
    ///
    /// Returns an error when the named VFS does not exist, when the database
    /// cannot be opened, when WAL/FULL durability cannot be established
    /// (verified by reading the pragmas back; a silent fallback is rejected
    /// with [`TaskStoreError::DurabilityUnavailable`]), or when the stored
    /// schema version cannot be migrated or validated.
    pub fn open_with_vfs(
        path: impl AsRef<Path>,
        vfs: Option<&str>,
    ) -> Result<Self, TaskStoreError> {
        let mut connection = match vfs {
            None => Connection::open(path)?,
            Some(name) => Connection::open_with_flags_and_vfs(path, OpenFlags::default(), name)?,
        };
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;

        // `pragma_update` discards the result row of `journal_mode`, so a
        // failed WAL transition would silently fall back (e.g. to `delete`).
        // Read both durability pragmas back and fail closed.
        let journal_mode: String =
            connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        let synchronous: i64 =
            connection.pragma_query_value(None, "synchronous", |row| row.get(0))?;
        if !journal_mode.eq_ignore_ascii_case("wal") || synchronous != 2 {
            return Err(TaskStoreError::DurabilityUnavailable {
                journal_mode,
                synchronous,
            });
        }

        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        match version {
            0 => {
                migrate_v1(&mut connection)?;
                migrate_v2(&mut connection)?;
                migrate_v3(&mut connection)?;
                migrate_v4(&mut connection)?;
                migrate_v5(&mut connection)?;
                migrate_v6(&mut connection)?;
            }
            1 => {
                migrate_v2(&mut connection)?;
                migrate_v3(&mut connection)?;
                migrate_v4(&mut connection)?;
                migrate_v5(&mut connection)?;
                migrate_v6(&mut connection)?;
            }
            2 => {
                migrate_v3(&mut connection)?;
                migrate_v4(&mut connection)?;
                migrate_v5(&mut connection)?;
                migrate_v6(&mut connection)?;
            }
            3 => {
                migrate_v4(&mut connection)?;
                migrate_v5(&mut connection)?;
                migrate_v6(&mut connection)?;
            }
            4 => {
                migrate_v5(&mut connection)?;
                migrate_v6(&mut connection)?;
            }
            5 => migrate_v6(&mut connection)?,
            SCHEMA_VERSION => {}
            other => return Err(TaskStoreError::UnsupportedSchema(other)),
        }

        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// Registers a Task idempotently.
    ///
    /// The initial `TaskHead` is `commit_seq = 0`, the domain-separated
    /// empty effect-history root (`[TASK-EFFECT-ID-001]`), and
    /// `retry_fence_epoch = 0`. Repeating the exact specification returns
    /// `Existing`; reusing the task ID with a different generation is
    /// rejected fail-closed.
    ///
    /// # Errors
    ///
    /// Returns a storage error or `DuplicateTask` for conflicting reuse.
    pub fn register_task(
        &self,
        spec: TaskSpec,
    ) -> Result<TaskRegistrationDecision, TaskStoreError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = load_task_optional(&transaction, spec.task_id)? {
            if existing.record.task_generation == spec.task_generation {
                transaction.commit()?;
                return Ok(TaskRegistrationDecision::Existing(spec.task_id));
            }
            return Err(TaskStoreError::DuplicateTask);
        }
        let record = TaskRecord {
            task_id: spec.task_id,
            task_generation: spec.task_generation,
            head_commit_seq: 0,
            head_effect_history_root: empty_effect_history_root(),
            retry_fence_epoch: 0,
            control_epoch: 1,
            cancel_epoch: 0,
            permit_epoch: 0,
            state: TaskState::Active,
            active_permit: None,
            created_at_ms: spec.registered_at_ms,
            updated_at_ms: spec.registered_at_ms,
        };
        insert_task(&transaction, &record)?;
        transaction.commit()?;
        Ok(TaskRegistrationDecision::Created(spec.task_id))
    }

    /// Registers one `TaskAttempt` idempotently.
    ///
    /// Each attempt carries an independent ID/generation and its own
    /// cancellation scope (`[TASK-ATTEMPT-001]`), and binds an immutable
    /// frozen-input snapshot bundle (`[TASK-SNAPSHOT-001]`). Several
    /// attempts MAY bind the same snapshot. Replaying the same idempotency
    /// key with the same bytes returns the original handle; different bytes
    /// under the same key or ID fail closed. A cancelled task admits no new
    /// attempts.
    ///
    /// # Errors
    ///
    /// Returns a validation, conflict, or storage error.
    pub fn register_attempt(
        &self,
        spec: AttemptSpec,
    ) -> Result<AttemptRegistrationDecision, TaskStoreError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task = load_task(&transaction, spec.task_id)?;
        if let Some(existing) =
            load_attempt_by_key(&transaction, spec.task_id, spec.idempotency_key)?
        {
            if attempt_matches_spec(&existing, &spec) {
                transaction.commit()?;
                return Ok(AttemptRegistrationDecision::Existing(handle_of(&existing)));
            }
            return Err(TaskStoreError::IdempotencyConflict);
        }
        if load_attempt_global(&transaction, spec.attempt_id)?.is_some() {
            return Err(TaskStoreError::DuplicateAttempt);
        }
        if task.record.state == TaskState::Cancelled {
            return Err(TaskStoreError::TaskCancelled);
        }
        insert_snapshot_if_absent(
            &transaction,
            spec.task_id,
            &spec.snapshot,
            spec.registered_at_ms,
        )?;
        let record = AttemptRecord {
            task_id: spec.task_id,
            attempt_id: spec.attempt_id,
            attempt_generation: spec.attempt_generation,
            snapshot: spec.snapshot,
            cancellation_scope_id: spec.cancellation_scope_id,
            cancellation_generation: spec.cancellation_generation,
            state: AttemptState::Created,
            receipt_id: None,
            created_at_ms: spec.registered_at_ms,
            updated_at_ms: spec.registered_at_ms,
        };
        insert_attempt(&transaction, &record, spec.idempotency_key)?;
        transaction.commit()?;
        Ok(AttemptRegistrationDecision::Created(handle_of(&record)))
    }

    /// Runs the linearizable `CommitPermit` CAS (`[TASK-COMMIT-001]`).
    ///
    /// A new permit is issued only when no permit is outstanding. The
    /// attempt's snapshot binding must match the current `TaskHead`
    /// bit-for-bit (commit seq, effect-history root, retry-fence epoch);
    /// otherwise the attempt is durably `Conflicted`. If another attempt
    /// holds the outstanding permit, the requester is durably `Superseded`
    /// with the winner's identity. If cancellation committed first
    /// (`[TASK-CANCEL-003]`), no permit is issued: the attempt closes
    /// pre-permit with a `CANCELLED_BEFORE_EFFECT` closure receipt and the
    /// `TaskHead` stays unchanged. Same key + same bytes replays the
    /// original permit; same key + different bytes fails closed.
    ///
    /// # Errors
    ///
    /// Returns a not-found, generation, idempotency-conflict, or storage
    /// error.
    // By-value request mirrors every other mutating API here; the lint only
    // fires because `planned_effects: Vec<_>` ended `Copy`.
    #[allow(clippy::needless_pass_by_value)]
    pub fn request_commit_permit(
        &self,
        request: PermitRequest,
    ) -> Result<PermitDecision, TaskStoreError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task = load_task(&transaction, request.task_id)?;
        if let Some(existing) =
            load_permit_by_key(&transaction, request.task_id, request.idempotency_key)?
        {
            let decision = replay_permit(&transaction, existing, &request)?;
            transaction.commit()?;
            return Ok(decision);
        }
        let attempt = load_attempt(&transaction, request.task_id, request.attempt_id)?;
        if attempt.attempt_generation != request.attempt_generation {
            return Err(TaskStoreError::InvalidGeneration);
        }
        if task.record.cancel_epoch > 0 {
            let decision = close_attempt_for_cancel(
                &transaction,
                &task.record,
                &attempt,
                request.requested_at_ms,
            )?;
            transaction.commit()?;
            return Ok(decision);
        }
        let decision = compete_for_permit(&transaction, &task, &attempt, &request)?;
        transaction.commit()?;
        Ok(decision)
    }

    /// Commits a Task cancellation (`[TASK-CANCEL-002]`).
    ///
    /// The first cancellation atomically increments `cancel_epoch` before
    /// anything else, blocking all later permit issuance. Every open
    /// pre-permit attempt closes with a `CANCELLED_BEFORE_EFFECT` closure
    /// receipt and the `TaskHead` stays unchanged. An already-issued permit
    /// is NOT cleared (`[TASK-COMMIT-003]`); its holder may still finalize
    /// (permit-first linearization; effect-level fencing is deferred to the
    /// `EffectPermit` slice). Replaying the same key returns the original
    /// decision without re-incrementing; a different key after cancellation
    /// observes `AlreadyCancelled`.
    ///
    /// # Errors
    ///
    /// Returns a not-found, epoch-exhaustion, or storage error.
    pub fn cancel_task(&self, request: CancelRequest) -> Result<CancelDecision, TaskStoreError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let task = load_task(&transaction, request.task_id)?;
        if let Some(cancel) = load_cancel(&transaction, request.task_id)? {
            let decision = if cancel.idempotency_key == request.idempotency_key {
                CancelDecision::Replayed {
                    cancel_epoch: cancel.cancel_epoch_after,
                }
            } else {
                CancelDecision::AlreadyCancelled {
                    cancel_epoch: task.record.cancel_epoch,
                }
            };
            transaction.commit()?;
            return Ok(decision);
        }
        let cancel_epoch = task
            .record
            .cancel_epoch
            .checked_add(1)
            .ok_or(TaskStoreError::EpochExhausted)?;
        let control_epoch = task
            .record
            .control_epoch
            .checked_add(1)
            .ok_or(TaskStoreError::EpochExhausted)?;
        let mut closed_attempts = Vec::new();
        for attempt in list_open_attempts(&transaction, request.task_id)? {
            let receipt_id =
                derive_closure_receipt_id(request.task_id, attempt.attempt_id, cancel_epoch);
            insert_receipt(
                &transaction,
                &closure_receipt(&task.record, &attempt, receipt_id, request.requested_at_ms),
            )?;
            set_attempt_state(
                &transaction,
                &attempt,
                AttemptState::Cancelled,
                Some(receipt_id),
                request.requested_at_ms,
            )?;
            closed_attempts.push(ClosedAttempt {
                attempt_id: attempt.attempt_id,
                attempt_generation: attempt.attempt_generation,
                receipt_id,
            });
        }
        update_task(&transaction, &task, request.requested_at_ms, |record| {
            record.cancel_epoch = cancel_epoch;
            record.control_epoch = control_epoch;
            record.state = TaskState::Cancelled;
        })?;
        insert_cancel(
            &transaction,
            request.task_id,
            request.idempotency_key,
            cancel_epoch,
            request.requested_at_ms,
        )?;
        transaction.commit()?;
        Ok(CancelDecision::Applied {
            cancel_epoch,
            closed_attempts,
        })
    }

    /// Reads the durable head/control view of a Task, including the
    /// currently outstanding permit (if any).
    ///
    /// # Errors
    ///
    /// Returns `TaskNotFound` or a storage error.
    pub fn inspect_task(&self, task_id: TaskId) -> Result<TaskRecord, TaskStoreError> {
        let connection = self.lock_connection()?;
        let mut stored = load_task(&*connection, task_id)?;
        stored.record.active_permit =
            load_outstanding_permit(&*connection, task_id)?.map(|permit| permit.permit_id);
        Ok(stored.record)
    }

    /// Reads the durable view of one `TaskAttempt`.
    ///
    /// # Errors
    ///
    /// Returns `AttemptNotFound` or a storage error.
    pub fn inspect_attempt(
        &self,
        task_id: TaskId,
        attempt_id: TaskAttemptId,
    ) -> Result<AttemptRecord, TaskStoreError> {
        let connection = self.lock_connection()?;
        load_attempt(&*connection, task_id, attempt_id)
    }

    /// Reads the durable view of one `CommitPermit`.
    ///
    /// # Errors
    ///
    /// Returns `PermitNotFound` or a storage error.
    pub fn inspect_permit(
        &self,
        task_id: TaskId,
        permit_id: CommitPermitId,
    ) -> Result<PermitRecord, TaskStoreError> {
        let connection = self.lock_connection()?;
        load_permit_by_id(&*connection, task_id, permit_id)
    }

    /// Reads one durable task receipt (commit or closure).
    ///
    /// # Errors
    ///
    /// Returns `ReceiptNotFound` or a storage error.
    pub fn inspect_receipt(
        &self,
        task_id: TaskId,
        receipt_id: ReceiptId,
    ) -> Result<TaskReceiptRecord, TaskStoreError> {
        let connection = self.lock_connection()?;
        load_receipt(&*connection, task_id, receipt_id)
    }

    pub(crate) fn lock_connection(&self) -> Result<MutexGuard<'_, Connection>, TaskStoreError> {
        self.connection
            .lock()
            .map_err(|_| TaskStoreError::LockPoisoned)
    }
}

fn replay_permit(
    transaction: &Transaction<'_>,
    existing: PermitRecord,
    request: &PermitRequest,
) -> Result<PermitDecision, TaskStoreError> {
    let stored_root = crate::effect::stored_effect_set_root(transaction, existing.permit_id)?
        .unwrap_or_else(crate::effect::empty_effect_set_root);
    let same_bytes = existing.attempt_id == request.attempt_id
        && existing.attempt_generation == request.attempt_generation
        && existing.write_set_root == request.write_set_root
        && existing.valid_until_ms == request.valid_until_ms
        && stored_root == crate::effect::effect_set_root_of(&request.planned_effects);
    if same_bytes {
        Ok(PermitDecision::Replayed(Box::new(existing)))
    } else {
        Err(TaskStoreError::IdempotencyConflict)
    }
}

fn close_attempt_for_cancel(
    transaction: &Transaction<'_>,
    task: &TaskRecord,
    attempt: &AttemptRecord,
    now_ms: i64,
) -> Result<PermitDecision, TaskStoreError> {
    if attempt.state == AttemptState::Cancelled {
        let receipt_id = attempt.receipt_id.ok_or(TaskStoreError::CorruptRecord(
            "cancelled attempt lacks closure receipt",
        ))?;
        return Ok(PermitDecision::CancelledBeforeEffect { receipt_id });
    }
    if !attempt.state.is_open_candidate() {
        return Err(TaskStoreError::InvalidAttemptState {
            state: attempt.state,
        });
    }
    let receipt_id = derive_closure_receipt_id(task.task_id, attempt.attempt_id, task.cancel_epoch);
    insert_receipt(
        transaction,
        &closure_receipt(task, attempt, receipt_id, now_ms),
    )?;
    set_attempt_state(
        transaction,
        attempt,
        AttemptState::Cancelled,
        Some(receipt_id),
        now_ms,
    )?;
    Ok(PermitDecision::CancelledBeforeEffect { receipt_id })
}

fn compete_for_permit(
    transaction: &Transaction<'_>,
    task: &StoredTask,
    attempt: &AttemptRecord,
    request: &PermitRequest,
) -> Result<PermitDecision, TaskStoreError> {
    if !attempt.state.is_open_candidate() {
        return Err(TaskStoreError::InvalidAttemptState {
            state: attempt.state,
        });
    }
    if let Some(reason) = validate_head_binding(&task.record, &attempt.snapshot) {
        set_attempt_state(
            transaction,
            attempt,
            AttemptState::Conflicted,
            None,
            request.requested_at_ms,
        )?;
        return Ok(PermitDecision::Conflicted { reason });
    }
    // `[TASK-COMMIT-003]` / `[TASK-EFFECT-003]`: a quarantine tombstone
    // blocks new winner issuance until every unknown slot is reconciled.
    if let Some(tombstone) = load_quarantined_permit(transaction, task.record.task_id)? {
        set_attempt_state(
            transaction,
            attempt,
            AttemptState::Superseded,
            None,
            request.requested_at_ms,
        )?;
        return Ok(PermitDecision::Quarantined {
            quarantine_receipt_id: crate::model::derive_quarantine_receipt_id(tombstone.permit_id),
        });
    }
    if let Some(active) = load_active_permit(transaction, task.record.task_id)? {
        // The conceptual CREATED → READY_TO_COMMIT walk collapses into this
        // atomic CAS: the durable outcomes are only COMMIT_PERMITTED,
        // SUPERSEDED, or CONFLICTED.
        if active.attempt_id == attempt.attempt_id {
            set_attempt_state(
                transaction,
                attempt,
                AttemptState::Conflicted,
                None,
                request.requested_at_ms,
            )?;
            return Ok(PermitDecision::Conflicted {
                reason: PermitConflict::AttemptAlreadyHoldsPermit {
                    permit_id: active.permit_id,
                },
            });
        }
        set_attempt_state(
            transaction,
            attempt,
            AttemptState::Superseded,
            None,
            request.requested_at_ms,
        )?;
        return Ok(PermitDecision::Superseded {
            winner: Box::new(active),
        });
    }
    let record = issue_permit(transaction, task, attempt, request)?;
    Ok(PermitDecision::Issued(Box::new(record)))
}

fn validate_head_binding(task: &TaskRecord, snapshot: &SnapshotBundle) -> Option<PermitConflict> {
    if snapshot.expected_head_commit_seq != task.head_commit_seq {
        return Some(PermitConflict::StaleTaskHead {
            expected: snapshot.expected_head_commit_seq,
            current: task.head_commit_seq,
        });
    }
    if snapshot.effect_history_root != task.head_effect_history_root {
        return Some(PermitConflict::StaleEffectHistoryRoot);
    }
    if snapshot.retry_fence_epoch != task.retry_fence_epoch {
        return Some(PermitConflict::StaleRetryFenceEpoch);
    }
    None
}

fn issue_permit(
    transaction: &Transaction<'_>,
    task: &StoredTask,
    attempt: &AttemptRecord,
    request: &PermitRequest,
) -> Result<PermitRecord, TaskStoreError> {
    let effect_set_root = crate::effect::validate_planned_effects(
        task.record.task_id,
        task.record.task_generation,
        &request.planned_effects,
    )?;
    let permit_epoch = task
        .record
        .permit_epoch
        .checked_add(1)
        .ok_or(TaskStoreError::EpochExhausted)?;
    let control_epoch = task
        .record
        .control_epoch
        .checked_add(1)
        .ok_or(TaskStoreError::EpochExhausted)?;
    let record = PermitRecord {
        permit_id: derive_permit_id(request.task_id, request.idempotency_key),
        task_id: request.task_id,
        idempotency_key: request.idempotency_key,
        attempt_id: attempt.attempt_id,
        attempt_generation: attempt.attempt_generation,
        expected_head_commit_seq: attempt.snapshot.expected_head_commit_seq,
        expected_effect_history_root: attempt.snapshot.effect_history_root,
        expected_retry_fence_epoch: attempt.snapshot.retry_fence_epoch,
        write_set_root: request.write_set_root,
        group_binding: crate::group::current_commit_binding(transaction, attempt.attempt_id)?,
        permit_epoch,
        control_epoch,
        cancel_epoch: task.record.cancel_epoch,
        valid_until_ms: request.valid_until_ms,
        state: PermitState::Issued,
        created_at_ms: request.requested_at_ms,
        updated_at_ms: request.requested_at_ms,
    };
    insert_permit(transaction, &record)?;
    crate::effect::insert_effect_set(
        transaction,
        &record,
        &request.planned_effects,
        effect_set_root,
        request.requested_at_ms,
    )?;
    set_attempt_state(
        transaction,
        attempt,
        AttemptState::CommitPermitted,
        None,
        request.requested_at_ms,
    )?;
    update_task(transaction, task, request.requested_at_ms, |task_record| {
        task_record.permit_epoch = permit_epoch;
        task_record.control_epoch = control_epoch;
    })?;
    Ok(record)
}

pub(crate) fn closure_receipt(
    task: &TaskRecord,
    attempt: &AttemptRecord,
    receipt_id: ReceiptId,
    now_ms: i64,
) -> TaskReceiptRecord {
    TaskReceiptRecord {
        receipt_id,
        task_id: task.task_id,
        permit_id: None,
        attempt_id: attempt.attempt_id,
        attempt_generation: attempt.attempt_generation,
        group_binding: None,
        outcome: ReceiptOutcome::CancelledBeforeEffect,
        prior_head_commit_seq: task.head_commit_seq,
        prior_effect_history_root: task.head_effect_history_root,
        prior_retry_fence_epoch: task.retry_fence_epoch,
        new_head_commit_seq: task.head_commit_seq,
        new_effect_history_root: task.head_effect_history_root,
        new_retry_fence_epoch: task.retry_fence_epoch,
        created_at_ms: now_ms,
    }
}

pub(crate) fn handle_of(record: &AttemptRecord) -> AttemptHandle {
    AttemptHandle {
        attempt_id: record.attempt_id,
        attempt_generation: record.attempt_generation,
        snapshot_id: record.snapshot.snapshot_id,
    }
}

pub(crate) fn attempt_matches_spec(record: &AttemptRecord, spec: &AttemptSpec) -> bool {
    record.task_id == spec.task_id
        && record.attempt_id == spec.attempt_id
        && record.attempt_generation == spec.attempt_generation
        && record.snapshot == spec.snapshot
        && record.cancellation_scope_id == spec.cancellation_scope_id
        && record.cancellation_generation == spec.cancellation_generation
}

fn migrate_v1(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V1_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v1 → v2 is purely additive (new effect-plane tables + `user_version`),
/// committed in one transaction: a failure anywhere rolls back to a
/// complete v1 database, never a half-migrated one.
fn migrate_v2(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(crate::effect::SCHEMA_V2_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v2 → v3 is purely additive (effect history + quarantine/adoption/
/// reconcile receipts + monotonic sequences + `user_version`), committed
/// in one transaction: a failure anywhere rolls back to a complete v2
/// database, never a half-migrated one.
fn migrate_v3(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(crate::effect::SCHEMA_V3_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v3 → v4 is purely additive (`TaskGroup` plane: groups, members,
/// admission/removal receipts, group cancels, attempt group bindings +
/// `user_version`), committed in one transaction: a failure anywhere
/// rolls back to a complete v3 database, never a half-migrated one.
fn migrate_v4(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(crate::group::SCHEMA_V4_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v4 → v5 is purely additive: nullable group-binding columns are added to
/// permits and receipts. Existing ungrouped/v1-v4 rows decode as `None`;
/// new grouped permits persist all four fields and copy them verbatim into
/// their terminal receipt.
fn migrate_v5(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(SCHEMA_V5_SQL)?;
    transaction.commit()?;
    Ok(())
}

/// v5 → v6 adds the immutable Artifact publication plan bound to one
/// outstanding permit. It records intent only; publication authorization
/// and receipt consumption are later state transitions.
fn migrate_v6(connection: &mut Connection) -> Result<(), TaskStoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(crate::commit::SCHEMA_V6_SQL)?;
    transaction.commit()?;
    Ok(())
}

const SCHEMA_V5_SQL: &str =
    "ALTER TABLE commit_permits ADD COLUMN group_id BLOB CHECK(group_id IS NULL OR length(group_id) = 16);
     ALTER TABLE commit_permits ADD COLUMN membership_generation BLOB CHECK(membership_generation IS NULL OR length(membership_generation) = 8);
     ALTER TABLE commit_permits ADD COLUMN membership_root BLOB CHECK(membership_root IS NULL OR length(membership_root) = 32);
     ALTER TABLE commit_permits ADD COLUMN group_policy_digest BLOB CHECK(group_policy_digest IS NULL OR length(group_policy_digest) = 32);
     ALTER TABLE task_receipts ADD COLUMN group_id BLOB CHECK(group_id IS NULL OR length(group_id) = 16);
     ALTER TABLE task_receipts ADD COLUMN membership_generation BLOB CHECK(membership_generation IS NULL OR length(membership_generation) = 8);
     ALTER TABLE task_receipts ADD COLUMN membership_root BLOB CHECK(membership_root IS NULL OR length(membership_root) = 32);
     ALTER TABLE task_receipts ADD COLUMN group_policy_digest BLOB CHECK(group_policy_digest IS NULL OR length(group_policy_digest) = 32);
     PRAGMA user_version = 5;";

const SCHEMA_V1_SQL: &str =
    "CREATE TABLE tasks (
            task_id BLOB PRIMARY KEY NOT NULL CHECK(length(task_id) = 16),
            task_generation BLOB NOT NULL CHECK(length(task_generation) = 8),
            head_commit_seq BLOB NOT NULL CHECK(length(head_commit_seq) = 8),
            head_effect_history_root BLOB NOT NULL CHECK(length(head_effect_history_root) = 32),
            retry_fence_epoch BLOB NOT NULL CHECK(length(retry_fence_epoch) = 8),
            control_epoch BLOB NOT NULL CHECK(length(control_epoch) = 8),
            cancel_epoch BLOB NOT NULL CHECK(length(cancel_epoch) = 8),
            permit_epoch BLOB NOT NULL CHECK(length(permit_epoch) = 8),
            task_state INTEGER NOT NULL,
            revision INTEGER NOT NULL DEFAULT 0,
            created_at_ms INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL
        ) STRICT;

        CREATE TABLE task_snapshots (
            task_id BLOB NOT NULL CHECK(length(task_id) = 16),
            snapshot_id BLOB NOT NULL CHECK(length(snapshot_id) = 16),
            snapshot_digest BLOB NOT NULL CHECK(length(snapshot_digest) = 32),
            expected_head_commit_seq BLOB NOT NULL CHECK(length(expected_head_commit_seq) = 8),
            effect_history_root BLOB NOT NULL CHECK(length(effect_history_root) = 32),
            retry_fence_epoch BLOB NOT NULL CHECK(length(retry_fence_epoch) = 8),
            created_at_ms INTEGER NOT NULL,
            PRIMARY KEY(task_id, snapshot_id),
            FOREIGN KEY(task_id) REFERENCES tasks(task_id)
        ) STRICT;

        CREATE TRIGGER task_snapshot_is_immutable
        BEFORE UPDATE ON task_snapshots
        BEGIN
            SELECT RAISE(ABORT, 'task snapshot is immutable');
        END;

        CREATE TABLE task_attempts (
            attempt_id BLOB PRIMARY KEY NOT NULL CHECK(length(attempt_id) = 16),
            task_id BLOB NOT NULL CHECK(length(task_id) = 16),
            attempt_generation BLOB NOT NULL CHECK(length(attempt_generation) = 8),
            snapshot_id BLOB NOT NULL CHECK(length(snapshot_id) = 16),
            cancellation_scope_id BLOB NOT NULL CHECK(length(cancellation_scope_id) = 16),
            cancellation_generation BLOB NOT NULL CHECK(length(cancellation_generation) = 8),
            idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
            attempt_state INTEGER NOT NULL,
            receipt_id BLOB CHECK(receipt_id IS NULL OR length(receipt_id) = 16),
            created_at_ms INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL,
            UNIQUE(task_id, idempotency_key),
            FOREIGN KEY(task_id) REFERENCES tasks(task_id)
        ) STRICT;

        CREATE TABLE commit_permits (
            permit_id BLOB PRIMARY KEY NOT NULL CHECK(length(permit_id) = 16),
            task_id BLOB NOT NULL CHECK(length(task_id) = 16),
            idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
            attempt_id BLOB NOT NULL CHECK(length(attempt_id) = 16),
            attempt_generation BLOB NOT NULL CHECK(length(attempt_generation) = 8),
            expected_head_commit_seq BLOB NOT NULL CHECK(length(expected_head_commit_seq) = 8),
            expected_effect_history_root BLOB NOT NULL CHECK(length(expected_effect_history_root) = 32),
            expected_retry_fence_epoch BLOB NOT NULL CHECK(length(expected_retry_fence_epoch) = 8),
            write_set_root BLOB NOT NULL CHECK(length(write_set_root) = 32),
            permit_epoch BLOB NOT NULL CHECK(length(permit_epoch) = 8),
            control_epoch BLOB NOT NULL CHECK(length(control_epoch) = 8),
            cancel_epoch BLOB NOT NULL CHECK(length(cancel_epoch) = 8),
            valid_until_ms INTEGER NOT NULL,
            permit_state INTEGER NOT NULL,
            created_at_ms INTEGER NOT NULL,
            updated_at_ms INTEGER NOT NULL,
            UNIQUE(task_id, idempotency_key),
            FOREIGN KEY(task_id) REFERENCES tasks(task_id)
        ) STRICT;

        -- Defense in depth: the single-writer transaction already
        -- serializes issuance; this index makes a second outstanding
        -- permit per task unrepresentable on disk.
        CREATE UNIQUE INDEX commit_permits_single_active
            ON commit_permits(task_id) WHERE permit_state = 0;

        CREATE TABLE task_cancels (
            task_id BLOB PRIMARY KEY NOT NULL CHECK(length(task_id) = 16),
            idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
            cancel_epoch_after BLOB NOT NULL CHECK(length(cancel_epoch_after) = 8),
            created_at_ms INTEGER NOT NULL,
            FOREIGN KEY(task_id) REFERENCES tasks(task_id)
        ) STRICT;

        CREATE TABLE task_receipts (
            receipt_id BLOB PRIMARY KEY NOT NULL CHECK(length(receipt_id) = 16),
            task_id BLOB NOT NULL CHECK(length(task_id) = 16),
            permit_id BLOB CHECK(permit_id IS NULL OR length(permit_id) = 16),
            attempt_id BLOB NOT NULL CHECK(length(attempt_id) = 16),
            attempt_generation BLOB NOT NULL CHECK(length(attempt_generation) = 8),
            outcome INTEGER NOT NULL,
            prior_head_commit_seq BLOB NOT NULL CHECK(length(prior_head_commit_seq) = 8),
            prior_effect_history_root BLOB NOT NULL CHECK(length(prior_effect_history_root) = 32),
            prior_retry_fence_epoch BLOB NOT NULL CHECK(length(prior_retry_fence_epoch) = 8),
            new_head_commit_seq BLOB NOT NULL CHECK(length(new_head_commit_seq) = 8),
            new_effect_history_root BLOB NOT NULL CHECK(length(new_effect_history_root) = 32),
            new_retry_fence_epoch BLOB NOT NULL CHECK(length(new_retry_fence_epoch) = 8),
            created_at_ms INTEGER NOT NULL,
            FOREIGN KEY(task_id) REFERENCES tasks(task_id)
        ) STRICT;

        CREATE INDEX task_receipts_by_permit
            ON task_receipts(task_id, permit_id);

        CREATE TRIGGER task_receipt_is_immutable
        BEFORE UPDATE ON task_receipts
        BEGIN
            SELECT RAISE(ABORT, 'task receipt is immutable');
        END;

        PRAGMA user_version = 1;";

pub(crate) trait SqlRead {
    fn prepare_statement(&self, sql: &str) -> Result<rusqlite::Statement<'_>, rusqlite::Error>;
}

impl SqlRead for Connection {
    fn prepare_statement(&self, sql: &str) -> Result<rusqlite::Statement<'_>, rusqlite::Error> {
        self.prepare(sql)
    }
}

impl SqlRead for Transaction<'_> {
    fn prepare_statement(&self, sql: &str) -> Result<rusqlite::Statement<'_>, rusqlite::Error> {
        self.prepare(sql)
    }
}

fn insert_task(transaction: &Transaction<'_>, record: &TaskRecord) -> Result<(), TaskStoreError> {
    transaction.execute(
        "INSERT INTO tasks (
            task_id, task_generation, head_commit_seq, head_effect_history_root,
            retry_fence_epoch, control_epoch, cancel_epoch, permit_epoch,
            task_state, revision, created_at_ms, updated_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, ?10, ?11)",
        params![
            record.task_id.as_bytes().as_slice(),
            encode_u64(record.task_generation.get()).as_slice(),
            encode_u64(record.head_commit_seq).as_slice(),
            record.head_effect_history_root.as_slice(),
            encode_u64(record.retry_fence_epoch).as_slice(),
            encode_u64(record.control_epoch).as_slice(),
            encode_u64(record.cancel_epoch).as_slice(),
            encode_u64(record.permit_epoch).as_slice(),
            record.state.code(),
            record.created_at_ms,
            record.updated_at_ms,
        ],
    )?;
    Ok(())
}

pub(crate) fn update_task(
    transaction: &Transaction<'_>,
    task: &StoredTask,
    now_ms: i64,
    mutate: impl FnOnce(&mut TaskRecord),
) -> Result<(), TaskStoreError> {
    let mut record = task.record.clone();
    mutate(&mut record);
    let changed = transaction.execute(
        "UPDATE tasks SET
            head_commit_seq = ?1, head_effect_history_root = ?2,
            retry_fence_epoch = ?3, control_epoch = ?4, cancel_epoch = ?5,
            permit_epoch = ?6, task_state = ?7, updated_at_ms = ?8,
            revision = revision + 1
         WHERE task_id = ?9 AND revision = ?10",
        params![
            encode_u64(record.head_commit_seq).as_slice(),
            record.head_effect_history_root.as_slice(),
            encode_u64(record.retry_fence_epoch).as_slice(),
            encode_u64(record.control_epoch).as_slice(),
            encode_u64(record.cancel_epoch).as_slice(),
            encode_u64(record.permit_epoch).as_slice(),
            record.state.code(),
            now_ms,
            record.task_id.as_bytes().as_slice(),
            task.revision,
        ],
    )?;
    if changed != 1 {
        return Err(TaskStoreError::CorruptRecord(
            "task revision compare-and-swap failed",
        ));
    }
    Ok(())
}

const TASK_COLUMNS: &str = "task_id, task_generation, head_commit_seq, head_effect_history_root,
     retry_fence_epoch, control_epoch, cancel_epoch, permit_epoch,
     task_state, revision, created_at_ms, updated_at_ms";

pub(crate) fn load_task(
    source: &impl SqlRead,
    task_id: TaskId,
) -> Result<StoredTask, TaskStoreError> {
    load_task_optional(source, task_id)?.ok_or(TaskStoreError::TaskNotFound)
}

fn load_task_optional(
    source: &impl SqlRead,
    task_id: TaskId,
) -> Result<Option<StoredTask>, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {TASK_COLUMNS} FROM tasks WHERE task_id = ?1"
    ))?;
    let mut rows = statement.query([task_id.as_bytes().as_slice()])?;
    rows.next()?.map(decode_task_row).transpose()
}

fn decode_task_row(row: &rusqlite::Row<'_>) -> Result<StoredTask, TaskStoreError> {
    let revision: i64 = row.get(9)?;
    if revision < 0 {
        return Err(TaskStoreError::CorruptRecord("negative task revision"));
    }
    Ok(StoredTask {
        record: TaskRecord {
            task_id: TaskId::from_bytes(blob16(row, 0)?),
            task_generation: generation_from_blob(row, 1)?,
            head_commit_seq: u64_from_blob(row, 2)?,
            head_effect_history_root: blob32(row, 3)?,
            retry_fence_epoch: u64_from_blob(row, 4)?,
            control_epoch: u64_from_blob(row, 5)?,
            cancel_epoch: u64_from_blob(row, 6)?,
            permit_epoch: u64_from_blob(row, 7)?,
            state: TaskState::from_code(row.get(8)?)?,
            active_permit: None,
            created_at_ms: row.get(10)?,
            updated_at_ms: row.get(11)?,
        },
        revision,
    })
}

pub(crate) fn insert_snapshot_if_absent(
    transaction: &Transaction<'_>,
    task_id: TaskId,
    snapshot: &SnapshotBundle,
    now_ms: i64,
) -> Result<(), TaskStoreError> {
    if let Some(existing) = load_snapshot_optional(transaction, task_id, snapshot.snapshot_id)? {
        if existing == *snapshot {
            return Ok(());
        }
        return Err(TaskStoreError::SnapshotConflict);
    }
    transaction.execute(
        "INSERT INTO task_snapshots (
            task_id, snapshot_id, snapshot_digest, expected_head_commit_seq,
            effect_history_root, retry_fence_epoch, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            task_id.as_bytes().as_slice(),
            snapshot.snapshot_id.as_bytes().as_slice(),
            snapshot.snapshot_digest.as_slice(),
            encode_u64(snapshot.expected_head_commit_seq).as_slice(),
            snapshot.effect_history_root.as_slice(),
            encode_u64(snapshot.retry_fence_epoch).as_slice(),
            now_ms,
        ],
    )?;
    Ok(())
}

fn load_snapshot_optional(
    source: &impl SqlRead,
    task_id: TaskId,
    snapshot_id: TaskSnapshotId,
) -> Result<Option<SnapshotBundle>, TaskStoreError> {
    let mut statement = source.prepare_statement(
        "SELECT snapshot_id, snapshot_digest, expected_head_commit_seq,
                effect_history_root, retry_fence_epoch
         FROM task_snapshots WHERE task_id = ?1 AND snapshot_id = ?2",
    )?;
    let mut rows = statement.query(params![
        task_id.as_bytes().as_slice(),
        snapshot_id.as_bytes().as_slice(),
    ])?;
    rows.next()?
        .map(|row| {
            Ok(SnapshotBundle {
                snapshot_id: TaskSnapshotId::from_bytes(blob16(row, 0)?),
                snapshot_digest: blob32(row, 1)?,
                expected_head_commit_seq: u64_from_blob(row, 2)?,
                effect_history_root: blob32(row, 3)?,
                retry_fence_epoch: u64_from_blob(row, 4)?,
            })
        })
        .transpose()
}

pub(crate) fn insert_attempt(
    transaction: &Transaction<'_>,
    record: &AttemptRecord,
    idempotency_key: IdempotencyKey,
) -> Result<(), TaskStoreError> {
    transaction.execute(
        "INSERT INTO task_attempts (
            attempt_id, task_id, attempt_generation, snapshot_id,
            cancellation_scope_id, cancellation_generation, idempotency_key,
            attempt_state, receipt_id, created_at_ms, updated_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, ?9, ?10)",
        params![
            record.attempt_id.as_bytes().as_slice(),
            record.task_id.as_bytes().as_slice(),
            encode_u64(record.attempt_generation.get()).as_slice(),
            record.snapshot.snapshot_id.as_bytes().as_slice(),
            record.cancellation_scope_id.as_bytes().as_slice(),
            encode_u64(record.cancellation_generation.get()).as_slice(),
            idempotency_key.as_bytes().as_slice(),
            record.state.code(),
            record.created_at_ms,
            record.updated_at_ms,
        ],
    )?;
    Ok(())
}

// Attempt rows always join their immutable snapshot row so the digest
// bundle an attempt is bound to can never drift from the snapshot table.
const ATTEMPT_SELECT: &str = "SELECT a.attempt_id, a.task_id, a.attempt_generation, a.snapshot_id,
            a.cancellation_scope_id, a.cancellation_generation,
            a.attempt_state, a.receipt_id, a.created_at_ms, a.updated_at_ms,
            s.snapshot_digest, s.expected_head_commit_seq,
            s.effect_history_root, s.retry_fence_epoch
     FROM task_attempts a
     JOIN task_snapshots s
       ON s.task_id = a.task_id AND s.snapshot_id = a.snapshot_id";

pub(crate) fn load_attempt(
    source: &impl SqlRead,
    task_id: TaskId,
    attempt_id: TaskAttemptId,
) -> Result<AttemptRecord, TaskStoreError> {
    load_attempt_optional(source, task_id, attempt_id)?.ok_or(TaskStoreError::AttemptNotFound)
}

fn load_attempt_optional(
    source: &impl SqlRead,
    task_id: TaskId,
    attempt_id: TaskAttemptId,
) -> Result<Option<AttemptRecord>, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "{ATTEMPT_SELECT} WHERE a.task_id = ?1 AND a.attempt_id = ?2"
    ))?;
    let mut rows = statement.query(params![
        task_id.as_bytes().as_slice(),
        attempt_id.as_bytes().as_slice(),
    ])?;
    rows.next()?.map(decode_attempt_row).transpose()
}

pub(crate) fn load_attempt_global(
    source: &impl SqlRead,
    attempt_id: TaskAttemptId,
) -> Result<Option<AttemptRecord>, TaskStoreError> {
    let mut statement =
        source.prepare_statement(&format!("{ATTEMPT_SELECT} WHERE a.attempt_id = ?1"))?;
    let mut rows = statement.query([attempt_id.as_bytes().as_slice()])?;
    rows.next()?.map(decode_attempt_row).transpose()
}

pub(crate) fn load_attempt_by_key(
    source: &impl SqlRead,
    task_id: TaskId,
    idempotency_key: IdempotencyKey,
) -> Result<Option<AttemptRecord>, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "{ATTEMPT_SELECT} WHERE a.task_id = ?1 AND a.idempotency_key = ?2"
    ))?;
    let mut rows = statement.query(params![
        task_id.as_bytes().as_slice(),
        idempotency_key.as_bytes().as_slice(),
    ])?;
    rows.next()?.map(decode_attempt_row).transpose()
}

pub(crate) fn list_open_attempts(
    source: &impl SqlRead,
    task_id: TaskId,
) -> Result<Vec<AttemptRecord>, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "{ATTEMPT_SELECT}
         WHERE a.task_id = ?1 AND a.attempt_state IN (?2, ?3)
         ORDER BY a.created_at_ms, a.attempt_id"
    ))?;
    let mut rows = statement.query(params![
        task_id.as_bytes().as_slice(),
        AttemptState::Created.code(),
        AttemptState::ReadyToCommit.code(),
    ])?;
    let mut attempts = Vec::new();
    while let Some(row) = rows.next()? {
        attempts.push(decode_attempt_row(row)?);
    }
    Ok(attempts)
}

fn decode_attempt_row(row: &rusqlite::Row<'_>) -> Result<AttemptRecord, TaskStoreError> {
    Ok(AttemptRecord {
        attempt_id: TaskAttemptId::from_bytes(blob16(row, 0)?),
        task_id: TaskId::from_bytes(blob16(row, 1)?),
        attempt_generation: generation_from_blob(row, 2)?,
        snapshot: SnapshotBundle {
            snapshot_id: TaskSnapshotId::from_bytes(blob16(row, 3)?),
            snapshot_digest: blob32(row, 10)?,
            expected_head_commit_seq: u64_from_blob(row, 11)?,
            effect_history_root: blob32(row, 12)?,
            retry_fence_epoch: u64_from_blob(row, 13)?,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes(blob16(row, 4)?),
        cancellation_generation: generation_from_blob(row, 5)?,
        state: AttemptState::from_code(row.get(6)?)?,
        receipt_id: optional_blob16(row, 7)?.map(ReceiptId::from_bytes),
        created_at_ms: row.get(8)?,
        updated_at_ms: row.get(9)?,
    })
}

pub(crate) fn set_attempt_state(
    transaction: &Transaction<'_>,
    attempt: &AttemptRecord,
    state: AttemptState,
    receipt_id: Option<ReceiptId>,
    now_ms: i64,
) -> Result<(), TaskStoreError> {
    let changed = transaction.execute(
        "UPDATE task_attempts
         SET attempt_state = ?1, receipt_id = ?2, updated_at_ms = ?3
         WHERE attempt_id = ?4 AND attempt_state = ?5",
        params![
            state.code(),
            receipt_id
                .map(ReceiptId::into_bytes)
                .as_ref()
                .map(<[u8; 16]>::as_slice),
            now_ms,
            attempt.attempt_id.as_bytes().as_slice(),
            attempt.state.code(),
        ],
    )?;
    if changed != 1 {
        return Err(TaskStoreError::CorruptRecord(
            "attempt state compare-and-swap failed",
        ));
    }
    Ok(())
}

fn insert_permit(
    transaction: &Transaction<'_>,
    record: &PermitRecord,
) -> Result<(), TaskStoreError> {
    let group_id = record
        .group_binding
        .map(|binding| binding.group_id.into_bytes());
    let membership_generation = record
        .group_binding
        .map(|binding| encode_u64(binding.membership_generation));
    let membership_root = record.group_binding.map(|binding| binding.membership_root);
    let group_policy_digest = record
        .group_binding
        .map(|binding| binding.group_policy_digest);
    transaction.execute(
        "INSERT INTO commit_permits (
            permit_id, task_id, idempotency_key, attempt_id, attempt_generation,
            expected_head_commit_seq, expected_effect_history_root,
            expected_retry_fence_epoch, write_set_root, permit_epoch,
            control_epoch, cancel_epoch, valid_until_ms, permit_state,
            created_at_ms, updated_at_ms, group_id, membership_generation,
            membership_root, group_policy_digest
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                   ?17, ?18, ?19, ?20)",
        params![
            record.permit_id.as_bytes().as_slice(),
            record.task_id.as_bytes().as_slice(),
            record.idempotency_key.as_bytes().as_slice(),
            record.attempt_id.as_bytes().as_slice(),
            encode_u64(record.attempt_generation.get()).as_slice(),
            encode_u64(record.expected_head_commit_seq).as_slice(),
            record.expected_effect_history_root.as_slice(),
            encode_u64(record.expected_retry_fence_epoch).as_slice(),
            record.write_set_root.as_slice(),
            encode_u64(record.permit_epoch).as_slice(),
            encode_u64(record.control_epoch).as_slice(),
            encode_u64(record.cancel_epoch).as_slice(),
            record.valid_until_ms,
            record.state.code(),
            record.created_at_ms,
            record.updated_at_ms,
            group_id.as_ref().map(<[u8; 16]>::as_slice),
            membership_generation.as_ref().map(<[u8; 8]>::as_slice),
            membership_root.as_ref().map(<[u8; 32]>::as_slice),
            group_policy_digest.as_ref().map(<[u8; 32]>::as_slice),
        ],
    )?;
    Ok(())
}

const PERMIT_COLUMNS: &str = "permit_id, task_id, idempotency_key, attempt_id, attempt_generation,
     expected_head_commit_seq, expected_effect_history_root,
     expected_retry_fence_epoch, write_set_root, permit_epoch,
     control_epoch, cancel_epoch, valid_until_ms, permit_state,
     created_at_ms, updated_at_ms, group_id, membership_generation,
     membership_root, group_policy_digest";

fn load_permit_by_key(
    source: &impl SqlRead,
    task_id: TaskId,
    idempotency_key: IdempotencyKey,
) -> Result<Option<PermitRecord>, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {PERMIT_COLUMNS} FROM commit_permits
         WHERE task_id = ?1 AND idempotency_key = ?2"
    ))?;
    let mut rows = statement.query(params![
        task_id.as_bytes().as_slice(),
        idempotency_key.as_bytes().as_slice(),
    ])?;
    rows.next()?.map(decode_permit_row).transpose()
}

pub(crate) fn load_permit_by_id(
    source: &impl SqlRead,
    task_id: TaskId,
    permit_id: CommitPermitId,
) -> Result<PermitRecord, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {PERMIT_COLUMNS} FROM commit_permits
         WHERE task_id = ?1 AND permit_id = ?2"
    ))?;
    let mut rows = statement.query(params![
        task_id.as_bytes().as_slice(),
        permit_id.as_bytes().as_slice(),
    ])?;
    rows.next()?
        .map(decode_permit_row)
        .transpose()?
        .ok_or(TaskStoreError::PermitNotFound)
}

fn load_active_permit(
    source: &impl SqlRead,
    task_id: TaskId,
) -> Result<Option<PermitRecord>, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {PERMIT_COLUMNS} FROM commit_permits
         WHERE task_id = ?1 AND permit_state = ?2"
    ))?;
    let mut rows = statement.query(params![
        task_id.as_bytes().as_slice(),
        PermitState::Issued.code(),
    ])?;
    rows.next()?.map(decode_permit_row).transpose()
}

/// The outstanding permit for head reporting: `Issued` or the
/// non-reusable `Quarantined` tombstone (`[TASK-EFFECT-003]`).
fn load_outstanding_permit(
    source: &impl SqlRead,
    task_id: TaskId,
) -> Result<Option<PermitRecord>, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {PERMIT_COLUMNS} FROM commit_permits
         WHERE task_id = ?1 AND permit_state IN (?2, ?3)"
    ))?;
    let mut rows = statement.query(params![
        task_id.as_bytes().as_slice(),
        PermitState::Issued.code(),
        PermitState::Quarantined.code(),
    ])?;
    rows.next()?.map(decode_permit_row).transpose()
}

/// The quarantine tombstone blocking new winner issuance, if any
/// (`[TASK-COMMIT-003]`).
pub(crate) fn load_quarantined_permit(
    source: &impl SqlRead,
    task_id: TaskId,
) -> Result<Option<PermitRecord>, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {PERMIT_COLUMNS} FROM commit_permits
         WHERE task_id = ?1 AND permit_state = ?2"
    ))?;
    let mut rows = statement.query(params![
        task_id.as_bytes().as_slice(),
        PermitState::Quarantined.code(),
    ])?;
    rows.next()?.map(decode_permit_row).transpose()
}

fn decode_permit_row(row: &rusqlite::Row<'_>) -> Result<PermitRecord, TaskStoreError> {
    let group_binding = decode_group_binding(row, 16)?;
    Ok(PermitRecord {
        permit_id: CommitPermitId::from_bytes(blob16(row, 0)?),
        task_id: TaskId::from_bytes(blob16(row, 1)?),
        idempotency_key: IdempotencyKey::from_bytes(blob16(row, 2)?),
        attempt_id: TaskAttemptId::from_bytes(blob16(row, 3)?),
        attempt_generation: generation_from_blob(row, 4)?,
        expected_head_commit_seq: u64_from_blob(row, 5)?,
        expected_effect_history_root: blob32(row, 6)?,
        expected_retry_fence_epoch: u64_from_blob(row, 7)?,
        write_set_root: blob32(row, 8)?,
        group_binding,
        permit_epoch: u64_from_blob(row, 9)?,
        control_epoch: u64_from_blob(row, 10)?,
        cancel_epoch: u64_from_blob(row, 11)?,
        valid_until_ms: row.get(12)?,
        state: PermitState::from_code(row.get(13)?)?,
        created_at_ms: row.get(14)?,
        updated_at_ms: row.get(15)?,
    })
}

pub(crate) fn close_permit(
    transaction: &Transaction<'_>,
    permit: &PermitRecord,
    now_ms: i64,
) -> Result<(), TaskStoreError> {
    let changed = transaction.execute(
        "UPDATE commit_permits
         SET permit_state = ?1, updated_at_ms = ?2
         WHERE permit_id = ?3 AND permit_state = ?4",
        params![
            PermitState::Closed.code(),
            now_ms,
            permit.permit_id.as_bytes().as_slice(),
            PermitState::Issued.code(),
        ],
    )?;
    if changed != 1 {
        return Err(TaskStoreError::CorruptRecord(
            "permit close compare-and-swap failed",
        ));
    }
    Ok(())
}

fn insert_cancel(
    transaction: &Transaction<'_>,
    task_id: TaskId,
    idempotency_key: IdempotencyKey,
    cancel_epoch_after: u64,
    now_ms: i64,
) -> Result<(), TaskStoreError> {
    transaction.execute(
        "INSERT INTO task_cancels (task_id, idempotency_key, cancel_epoch_after, created_at_ms)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            task_id.as_bytes().as_slice(),
            idempotency_key.as_bytes().as_slice(),
            encode_u64(cancel_epoch_after).as_slice(),
            now_ms,
        ],
    )?;
    Ok(())
}

fn load_cancel(
    source: &impl SqlRead,
    task_id: TaskId,
) -> Result<Option<StoredCancel>, TaskStoreError> {
    let mut statement = source.prepare_statement(
        "SELECT idempotency_key, cancel_epoch_after FROM task_cancels WHERE task_id = ?1",
    )?;
    let mut rows = statement.query([task_id.as_bytes().as_slice()])?;
    rows.next()?
        .map(|row| {
            Ok(StoredCancel {
                idempotency_key: IdempotencyKey::from_bytes(blob16(row, 0)?),
                cancel_epoch_after: u64_from_blob(row, 1)?,
            })
        })
        .transpose()
}

pub(crate) fn insert_receipt(
    transaction: &Transaction<'_>,
    record: &TaskReceiptRecord,
) -> Result<(), TaskStoreError> {
    if !record.outcome.is_producible() {
        return Err(TaskStoreError::CorruptRecord(
            "reserved receipt outcome is not producible in this slice",
        ));
    }
    let group_id = record
        .group_binding
        .map(|binding| binding.group_id.into_bytes());
    let membership_generation = record
        .group_binding
        .map(|binding| encode_u64(binding.membership_generation));
    let membership_root = record.group_binding.map(|binding| binding.membership_root);
    let group_policy_digest = record
        .group_binding
        .map(|binding| binding.group_policy_digest);
    transaction.execute(
        "INSERT INTO task_receipts (
            receipt_id, task_id, permit_id, attempt_id, attempt_generation,
            outcome, prior_head_commit_seq, prior_effect_history_root,
            prior_retry_fence_epoch, new_head_commit_seq,
            new_effect_history_root, new_retry_fence_epoch, created_at_ms,
            group_id, membership_generation, membership_root, group_policy_digest
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                   ?14, ?15, ?16, ?17)",
        params![
            record.receipt_id.as_bytes().as_slice(),
            record.task_id.as_bytes().as_slice(),
            record
                .permit_id
                .map(CommitPermitId::into_bytes)
                .as_ref()
                .map(<[u8; 16]>::as_slice),
            record.attempt_id.as_bytes().as_slice(),
            encode_u64(record.attempt_generation.get()).as_slice(),
            record.outcome.code(),
            encode_u64(record.prior_head_commit_seq).as_slice(),
            record.prior_effect_history_root.as_slice(),
            encode_u64(record.prior_retry_fence_epoch).as_slice(),
            encode_u64(record.new_head_commit_seq).as_slice(),
            record.new_effect_history_root.as_slice(),
            encode_u64(record.new_retry_fence_epoch).as_slice(),
            record.created_at_ms,
            group_id.as_ref().map(<[u8; 16]>::as_slice),
            membership_generation.as_ref().map(<[u8; 8]>::as_slice),
            membership_root.as_ref().map(<[u8; 32]>::as_slice),
            group_policy_digest.as_ref().map(<[u8; 32]>::as_slice),
        ],
    )?;
    Ok(())
}

const RECEIPT_COLUMNS: &str = "receipt_id, task_id, permit_id, attempt_id, attempt_generation,
     outcome, prior_head_commit_seq, prior_effect_history_root,
     prior_retry_fence_epoch, new_head_commit_seq,
     new_effect_history_root, new_retry_fence_epoch, created_at_ms,
     group_id, membership_generation, membership_root, group_policy_digest";

pub(crate) fn load_receipt(
    source: &impl SqlRead,
    task_id: TaskId,
    receipt_id: ReceiptId,
) -> Result<TaskReceiptRecord, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {RECEIPT_COLUMNS} FROM task_receipts
         WHERE task_id = ?1 AND receipt_id = ?2"
    ))?;
    let mut rows = statement.query(params![
        task_id.as_bytes().as_slice(),
        receipt_id.as_bytes().as_slice(),
    ])?;
    rows.next()?
        .map(decode_receipt_row)
        .transpose()?
        .ok_or(TaskStoreError::ReceiptNotFound)
}

pub(crate) fn load_receipt_by_permit(
    source: &impl SqlRead,
    task_id: TaskId,
    permit_id: CommitPermitId,
) -> Result<Option<TaskReceiptRecord>, TaskStoreError> {
    let mut statement = source.prepare_statement(&format!(
        "SELECT {RECEIPT_COLUMNS} FROM task_receipts
         WHERE task_id = ?1 AND permit_id = ?2"
    ))?;
    let mut rows = statement.query(params![
        task_id.as_bytes().as_slice(),
        permit_id.as_bytes().as_slice(),
    ])?;
    rows.next()?.map(decode_receipt_row).transpose()
}

fn decode_receipt_row(row: &rusqlite::Row<'_>) -> Result<TaskReceiptRecord, TaskStoreError> {
    Ok(TaskReceiptRecord {
        receipt_id: ReceiptId::from_bytes(blob16(row, 0)?),
        task_id: TaskId::from_bytes(blob16(row, 1)?),
        permit_id: optional_blob16(row, 2)?.map(CommitPermitId::from_bytes),
        attempt_id: TaskAttemptId::from_bytes(blob16(row, 3)?),
        attempt_generation: generation_from_blob(row, 4)?,
        group_binding: decode_group_binding(row, 13)?,
        outcome: ReceiptOutcome::from_code(row.get(5)?)?,
        prior_head_commit_seq: u64_from_blob(row, 6)?,
        prior_effect_history_root: blob32(row, 7)?,
        prior_retry_fence_epoch: u64_from_blob(row, 8)?,
        new_head_commit_seq: u64_from_blob(row, 9)?,
        new_effect_history_root: blob32(row, 10)?,
        new_retry_fence_epoch: u64_from_blob(row, 11)?,
        created_at_ms: row.get(12)?,
    })
}

fn decode_group_binding(
    row: &rusqlite::Row<'_>,
    first_index: usize,
) -> Result<Option<crate::TaskGroupCommitBinding>, TaskStoreError> {
    let group_id = optional_blob::<16>(row, first_index)?;
    let generation = optional_blob::<8>(row, first_index + 1)?;
    let root = optional_blob::<32>(row, first_index + 2)?;
    let policy = optional_blob::<32>(row, first_index + 3)?;
    match (group_id, generation, root, policy) {
        (None, None, None, None) => Ok(None),
        (Some(group_id), Some(generation), Some(root), Some(policy)) => {
            Ok(Some(crate::TaskGroupCommitBinding {
                group_id: crate::TaskGroupId::from_bytes(group_id),
                membership_generation: u64::from_be_bytes(generation),
                membership_root: root,
                group_policy_digest: policy,
            }))
        }
        _ => Err(TaskStoreError::CorruptRecord(
            "partial task group commit binding",
        )),
    }
}

pub(crate) const fn encode_u64(value: u64) -> [u8; 8] {
    value.to_be_bytes()
}

pub(crate) fn generation_from_blob(
    row: &rusqlite::Row<'_>,
    index: usize,
) -> Result<Generation, TaskStoreError> {
    let value = u64_from_blob(row, index)?;
    let non_zero =
        std::num::NonZeroU64::new(value).ok_or(TaskStoreError::CorruptRecord("zero generation"))?;
    Ok(Generation::new(non_zero))
}

pub(crate) fn u64_from_blob(row: &rusqlite::Row<'_>, index: usize) -> Result<u64, TaskStoreError> {
    Ok(u64::from_be_bytes(blob8(row, index)?))
}

pub(crate) fn blob16(row: &rusqlite::Row<'_>, index: usize) -> Result<[u8; 16], TaskStoreError> {
    let value: Vec<u8> = row.get(index)?;
    value
        .try_into()
        .map_err(|_| TaskStoreError::CorruptRecord("expected 16-byte blob"))
}

pub(crate) fn blob32(row: &rusqlite::Row<'_>, index: usize) -> Result<[u8; 32], TaskStoreError> {
    let value: Vec<u8> = row.get(index)?;
    value
        .try_into()
        .map_err(|_| TaskStoreError::CorruptRecord("expected 32-byte blob"))
}

pub(crate) fn optional_blob16(
    row: &rusqlite::Row<'_>,
    index: usize,
) -> Result<Option<[u8; 16]>, TaskStoreError> {
    let value: Option<Vec<u8>> = row.get(index)?;
    value
        .map(|bytes| {
            bytes
                .try_into()
                .map_err(|_| TaskStoreError::CorruptRecord("expected optional 16-byte blob"))
        })
        .transpose()
}

fn optional_blob<const N: usize>(
    row: &rusqlite::Row<'_>,
    index: usize,
) -> Result<Option<[u8; N]>, TaskStoreError> {
    let value: Option<Vec<u8>> = row.get(index)?;
    value
        .map(|bytes| {
            bytes
                .try_into()
                .map_err(|_| TaskStoreError::CorruptRecord("unexpected optional blob width"))
        })
        .transpose()
}

fn blob8(row: &rusqlite::Row<'_>, index: usize) -> Result<[u8; 8], TaskStoreError> {
    let value: Vec<u8> = row.get(index)?;
    value
        .try_into()
        .map_err(|_| TaskStoreError::CorruptRecord("expected 8-byte blob"))
}
