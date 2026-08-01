//! Single-node durable authority store for NLOS operations.
//!
//! Operation state and its wake/reconciliation notification are committed in
//! one `SQLite` transaction. Consumers must acknowledge outbox entries only
//! after applying them idempotently; a crash may therefore redeliver an entry,
//! but cannot lose a committed transition.

use std::error::Error;
use std::fmt;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use nlos_operation::{
    AcceptedCallback, CallbackTicket, CompletionDecision, CompletionOutcome, IssuedCallback,
    OperationError, OperationHandle, OperationMachine, OperationSnapshot, OperationSpec,
    OperationState,
};
use nlos_runtime::FiberHandle;
use nlos_types::{
    CallbackId, CancelEpoch, CancellationScopeId, ExecutionFiberId, Generation, OperationId,
    ReceiptId,
};
use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior, params};

const SCHEMA_VERSION: i64 = 1;

#[derive(Debug)]
pub enum StoreError {
    Sqlite(rusqlite::Error),
    Operation(OperationError),
    CorruptRecord(&'static str),
    UnsupportedSchema(i64),
    OutboxEntryNotFound,
    LockPoisoned,
    DurabilityUnavailable {
        journal_mode: String,
        synchronous: i64,
    },
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(formatter, "SQLite authority failure: {error}"),
            Self::Operation(error) => write!(formatter, "operation transition rejected: {error}"),
            Self::CorruptRecord(reason) => write!(formatter, "corrupt durable record: {reason}"),
            Self::UnsupportedSchema(version) => {
                write!(formatter, "unsupported authority schema version {version}")
            }
            Self::OutboxEntryNotFound => formatter.write_str("outbox entry does not exist"),
            Self::LockPoisoned => formatter.write_str("authority writer lock is poisoned"),
            Self::DurabilityUnavailable {
                journal_mode,
                synchronous,
            } => write!(
                formatter,
                "WAL/FULL durability unavailable: journal_mode={journal_mode}, synchronous={synchronous}"
            ),
        }
    }
}

impl Error for StoreError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            Self::Operation(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

impl From<OperationError> for StoreError {
    fn from(error: OperationError) -> Self {
        Self::Operation(error)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistrationDecision {
    Created(OperationHandle),
    Existing(OperationHandle),
}

impl RegistrationDecision {
    #[must_use]
    pub const fn handle(self) -> OperationHandle {
        match self {
            Self::Created(handle) | Self::Existing(handle) => handle,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutboxKind {
    WakeFiber,
    ReconcileEffect,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutboxEntry {
    pub sequence: i64,
    pub kind: OutboxKind,
    pub operation: OperationHandle,
    pub owner_fiber: FiberHandle,
    pub callback_id: Option<CallbackId>,
    pub state: OperationState,
}

/// A single-writer `SQLite` authority. The mutex is a process-local admission
/// gate; `SQLite` `BEGIN IMMEDIATE` remains the storage-level writer fence.
pub struct SqliteOperationStore {
    connection: Mutex<Connection>,
}

impl SqliteOperationStore {
    /// Opens or creates an authority database and validates its schema.
    ///
    /// Equivalent to [`SqliteOperationStore::open_with_vfs`] with `None`,
    /// i.e. the process-default `SQLite` VFS.
    ///
    /// # Errors
    ///
    /// Returns an error when the database cannot be opened, when WAL/FULL
    /// durability cannot be established (verified by reading the pragmas
    /// back; a silent fallback is rejected with
    /// [`StoreError::DurabilityUnavailable`]), or when the stored schema
    /// version cannot be migrated or validated.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        Self::open_with_vfs(path, None)
    }

    /// Opens or creates an authority database through a named `SQLite` VFS.
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
    /// with [`StoreError::DurabilityUnavailable`]), or when the stored schema
    /// version cannot be migrated or validated.
    pub fn open_with_vfs(path: impl AsRef<Path>, vfs: Option<&str>) -> Result<Self, StoreError> {
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
            return Err(StoreError::DurabilityUnavailable {
                journal_mode,
                synchronous,
            });
        }

        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        match version {
            0 => migrate_v1(&mut connection)?,
            SCHEMA_VERSION => {}
            other => return Err(StoreError::UnsupportedSchema(other)),
        }

        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// Registers an operation idempotently.
    ///
    /// Repeating the exact durable specification returns `Existing`; reusing
    /// the stable ID for different bytes is rejected.
    ///
    /// # Errors
    ///
    /// Returns a storage error or `DuplicateOperation` for conflicting reuse.
    pub fn register(&self, spec: OperationSpec) -> Result<RegistrationDecision, StoreError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = load_machine_optional(&transaction, spec.operation_id)? {
            if existing.spec() == spec {
                transaction.commit()?;
                return Ok(RegistrationDecision::Existing(existing.snapshot().handle));
            }
            return Err(OperationError::DuplicateOperation.into());
        }

        let machine = OperationMachine::new(spec);
        insert_machine(&transaction, &machine)?;
        transaction.commit()?;
        Ok(RegistrationDecision::Created(machine.snapshot().handle))
    }

    /// Commits the dispatch transition and returns its durable callback ticket.
    ///
    /// # Errors
    ///
    /// Returns a storage or operation state/generation error.
    pub fn dispatch(
        &self,
        handle: OperationHandle,
        callback_id: CallbackId,
    ) -> Result<CallbackTicket, StoreError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (mut machine, revision) = load_machine(&transaction, handle.operation_id)?;
        let ticket = machine.dispatch(handle, callback_id)?;
        update_machine(&transaction, &machine, revision)?;
        transaction.commit()?;
        Ok(ticket)
    }

    /// Commits cancellation and, when no effect was dispatched, atomically
    /// emits a wake outbox item for the waiting fiber.
    ///
    /// # Errors
    ///
    /// Returns a storage or operation state/generation error.
    pub fn request_cancel(
        &self,
        handle: OperationHandle,
        no_effect_receipt: ReceiptId,
    ) -> Result<OperationSnapshot, StoreError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (mut machine, revision) = load_machine(&transaction, handle.operation_id)?;
        let snapshot = machine.request_cancel(handle, no_effect_receipt)?;
        update_machine(&transaction, &machine, revision)?;
        if matches!(snapshot.state, OperationState::CancelledBeforeEffect { .. }) {
            insert_outbox(&transaction, OutboxKind::WakeFiber, &machine, None)?;
        }
        transaction.commit()?;
        Ok(snapshot)
    }

    /// Commits a terminal callback and its wake/reconciliation outbox item in
    /// the same transaction.
    ///
    /// # Errors
    ///
    /// Returns a storage error or rejects stale, forged, or conflicting input.
    pub fn complete(
        &self,
        ticket: CallbackTicket,
        outcome: CompletionOutcome,
    ) -> Result<CompletionDecision, StoreError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (mut machine, revision) = load_machine(&transaction, ticket.operation.operation_id)?;
        let decision = machine.complete(ticket, outcome)?;

        if !matches!(decision, CompletionDecision::Duplicate { .. }) {
            update_machine(&transaction, &machine, revision)?;
            let kind = match decision {
                CompletionDecision::CanonicalizedAndWake { .. } => OutboxKind::WakeFiber,
                CompletionDecision::CanonicalizedForReconciliation { .. } => {
                    OutboxKind::ReconcileEffect
                }
                CompletionDecision::Duplicate { .. } => unreachable!("handled above"),
            };
            insert_outbox(&transaction, kind, &machine, Some(ticket.callback_id))?;
        }

        transaction.commit()?;
        Ok(decision)
    }

    /// Reads a durable, invariant-checked snapshot.
    ///
    /// # Errors
    ///
    /// Returns a storage or stale-generation error.
    pub fn inspect(&self, handle: OperationHandle) -> Result<OperationSnapshot, StoreError> {
        let connection = self.lock_connection()?;
        let (machine, _) = load_machine(&*connection, handle.operation_id)?;
        let snapshot = machine.snapshot();
        if snapshot.handle.generation != handle.generation {
            return Err(OperationError::InvalidGeneration.into());
        }
        Ok(snapshot)
    }

    /// Lists unacknowledged outbox entries in durable sequence order.
    ///
    /// # Errors
    ///
    /// Returns a storage error or corrupt-record error.
    pub fn pending_outbox(&self, limit: usize) -> Result<Vec<OutboxEntry>, StoreError> {
        let connection = self.lock_connection()?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut statement = connection.prepare(
            "SELECT sequence, kind, operation_id, operation_generation,
                    owner_fiber_id, owner_fiber_generation, callback_id,
                    state_kind, receipt_id
             FROM operation_outbox
             WHERE acknowledged = 0
             ORDER BY sequence
             LIMIT ?1",
        )?;
        let mut rows = statement.query([limit])?;
        let mut entries = Vec::new();
        while let Some(row) = rows.next()? {
            entries.push(decode_outbox_row(row)?);
        }
        Ok(entries)
    }

    /// Acknowledges an outbox entry after the consumer has applied it
    /// idempotently. Repeating the ACK is safe.
    ///
    /// # Errors
    ///
    /// Returns an error when the entry does not exist or `SQLite` cannot commit.
    pub fn acknowledge_outbox(&self, sequence: i64) -> Result<(), StoreError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE operation_outbox SET acknowledged = 1 WHERE sequence = ?1",
            [sequence],
        )?;
        if changed == 0 {
            return Err(StoreError::OutboxEntryNotFound);
        }
        transaction.commit()?;
        Ok(())
    }

    fn lock_connection(&self) -> Result<MutexGuard<'_, Connection>, StoreError> {
        self.connection.lock().map_err(|_| StoreError::LockPoisoned)
    }
}

fn migrate_v1(connection: &mut Connection) -> Result<(), StoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE operations (
            operation_id BLOB PRIMARY KEY NOT NULL CHECK(length(operation_id) = 16),
            generation BLOB NOT NULL CHECK(length(generation) = 8),
            owner_fiber_id BLOB NOT NULL CHECK(length(owner_fiber_id) = 16),
            owner_fiber_generation BLOB NOT NULL CHECK(length(owner_fiber_generation) = 8),
            cancellation_scope_id BLOB NOT NULL CHECK(length(cancellation_scope_id) = 16),
            cancellation_generation BLOB NOT NULL CHECK(length(cancellation_generation) = 8),
            cancel_epoch BLOB NOT NULL CHECK(length(cancel_epoch) = 8),
            state_kind INTEGER NOT NULL,
            receipt_id BLOB CHECK(receipt_id IS NULL OR length(receipt_id) = 16),
            issued_callback_id BLOB
                CHECK(issued_callback_id IS NULL OR length(issued_callback_id) = 16),
            issued_cancel_epoch BLOB
                CHECK(issued_cancel_epoch IS NULL OR length(issued_cancel_epoch) = 8),
            accepted_callback_id BLOB
                CHECK(accepted_callback_id IS NULL OR length(accepted_callback_id) = 16),
            revision INTEGER NOT NULL DEFAULT 0
        ) STRICT;

        CREATE TABLE operation_outbox (
            sequence INTEGER PRIMARY KEY AUTOINCREMENT,
            kind INTEGER NOT NULL,
            operation_id BLOB NOT NULL CHECK(length(operation_id) = 16),
            operation_generation BLOB NOT NULL CHECK(length(operation_generation) = 8),
            owner_fiber_id BLOB NOT NULL CHECK(length(owner_fiber_id) = 16),
            owner_fiber_generation BLOB NOT NULL CHECK(length(owner_fiber_generation) = 8),
            callback_id BLOB CHECK(callback_id IS NULL OR length(callback_id) = 16),
            state_kind INTEGER NOT NULL,
            receipt_id BLOB NOT NULL CHECK(length(receipt_id) = 16),
            acknowledged INTEGER NOT NULL DEFAULT 0 CHECK(acknowledged IN (0, 1))
        ) STRICT;

        CREATE INDEX operation_outbox_pending
            ON operation_outbox(acknowledged, sequence);
        PRAGMA user_version = 1;",
    )?;
    transaction.commit()?;
    Ok(())
}

fn insert_machine(
    transaction: &Transaction<'_>,
    machine: &OperationMachine,
) -> Result<(), StoreError> {
    let encoded = EncodedMachine::from_machine(machine);
    transaction.execute(
        "INSERT INTO operations (
            operation_id, generation, owner_fiber_id, owner_fiber_generation,
            cancellation_scope_id, cancellation_generation, cancel_epoch,
            state_kind, receipt_id, issued_callback_id, issued_cancel_epoch,
            accepted_callback_id, revision
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 0)",
        params![
            encoded.operation_id.as_slice(),
            encoded.generation.as_slice(),
            encoded.owner_fiber_id.as_slice(),
            encoded.owner_fiber_generation.as_slice(),
            encoded.cancellation_scope_id.as_slice(),
            encoded.cancellation_generation.as_slice(),
            encoded.cancel_epoch.as_slice(),
            encoded.state_kind,
            encoded.receipt_id.as_ref().map(<[u8; 16]>::as_slice),
            encoded
                .issued_callback_id
                .as_ref()
                .map(<[u8; 16]>::as_slice),
            encoded
                .issued_cancel_epoch
                .as_ref()
                .map(<[u8; 8]>::as_slice),
            encoded
                .accepted_callback_id
                .as_ref()
                .map(<[u8; 16]>::as_slice),
        ],
    )?;
    Ok(())
}

fn update_machine(
    transaction: &Transaction<'_>,
    machine: &OperationMachine,
    expected_revision: i64,
) -> Result<(), StoreError> {
    let encoded = EncodedMachine::from_machine(machine);
    let changed = transaction.execute(
        "UPDATE operations SET
            cancel_epoch = ?1, state_kind = ?2, receipt_id = ?3,
            issued_callback_id = ?4, issued_cancel_epoch = ?5,
            accepted_callback_id = ?6, revision = revision + 1
         WHERE operation_id = ?7 AND generation = ?8 AND revision = ?9",
        params![
            encoded.cancel_epoch.as_slice(),
            encoded.state_kind,
            encoded.receipt_id.as_ref().map(<[u8; 16]>::as_slice),
            encoded
                .issued_callback_id
                .as_ref()
                .map(<[u8; 16]>::as_slice),
            encoded
                .issued_cancel_epoch
                .as_ref()
                .map(<[u8; 8]>::as_slice),
            encoded
                .accepted_callback_id
                .as_ref()
                .map(<[u8; 16]>::as_slice),
            encoded.operation_id.as_slice(),
            encoded.generation.as_slice(),
            expected_revision,
        ],
    )?;
    if changed != 1 {
        return Err(StoreError::CorruptRecord(
            "operation revision compare-and-swap failed",
        ));
    }
    Ok(())
}

fn insert_outbox(
    transaction: &Transaction<'_>,
    kind: OutboxKind,
    machine: &OperationMachine,
    callback_id: Option<CallbackId>,
) -> Result<(), StoreError> {
    let snapshot = machine.snapshot();
    let receipt_id = receipt_from_state(snapshot.state).ok_or(StoreError::CorruptRecord(
        "outbox state lacks final receipt",
    ))?;
    transaction.execute(
        "INSERT INTO operation_outbox (
            kind, operation_id, operation_generation, owner_fiber_id,
            owner_fiber_generation, callback_id, state_kind, receipt_id
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            encode_outbox_kind(kind),
            snapshot.handle.operation_id.as_bytes().as_slice(),
            encode_u64(snapshot.handle.generation.get()).as_slice(),
            snapshot.owner_fiber.fiber_id.as_bytes().as_slice(),
            encode_u64(snapshot.owner_fiber.generation.get()).as_slice(),
            callback_id
                .map(CallbackId::into_bytes)
                .as_ref()
                .map(<[u8; 16]>::as_slice),
            encode_state(snapshot.state).0,
            receipt_id.as_bytes().as_slice(),
        ],
    )?;
    Ok(())
}

trait SqlRead {
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

fn load_machine(
    source: &impl SqlRead,
    operation_id: OperationId,
) -> Result<(OperationMachine, i64), StoreError> {
    load_machine_optional_with_revision(source, operation_id)?
        .ok_or_else(|| OperationError::InvalidGeneration.into())
}

fn load_machine_optional(
    source: &impl SqlRead,
    operation_id: OperationId,
) -> Result<Option<OperationMachine>, StoreError> {
    Ok(load_machine_optional_with_revision(source, operation_id)?.map(|(machine, _)| machine))
}

fn load_machine_optional_with_revision(
    source: &impl SqlRead,
    operation_id: OperationId,
) -> Result<Option<(OperationMachine, i64)>, StoreError> {
    let mut statement = source.prepare_statement(
        "SELECT operation_id, generation, owner_fiber_id, owner_fiber_generation,
                cancellation_scope_id, cancellation_generation, cancel_epoch,
                state_kind, receipt_id, issued_callback_id, issued_cancel_epoch,
                accepted_callback_id, revision
         FROM operations WHERE operation_id = ?1",
    )?;
    let mut rows = statement.query([operation_id.as_bytes().as_slice()])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };

    let operation_id = OperationId::from_bytes(blob16(row, 0)?);
    let generation = generation_from_blob(row, 1)?;
    let owner_fiber = FiberHandle {
        fiber_id: ExecutionFiberId::from_bytes(blob16(row, 2)?),
        generation: generation_from_blob(row, 3)?,
    };
    let spec = OperationSpec {
        operation_id,
        generation,
        owner_fiber,
        cancellation_scope_id: CancellationScopeId::from_bytes(blob16(row, 4)?),
        cancellation_generation: generation_from_blob(row, 5)?,
    };
    let cancel_epoch = CancelEpoch::new(u64_from_blob(row, 6)?);
    let state_kind: i64 = row.get(7)?;
    let receipt = optional_blob16(row, 8)?.map(ReceiptId::from_bytes);
    let state = decode_state(state_kind, receipt)?;
    let issued_callback_id = optional_blob16(row, 9)?.map(CallbackId::from_bytes);
    let issued_cancel_epoch = optional_blob8(row, 10)?.map(u64::from_be_bytes);
    let issued_callback = match (issued_callback_id, issued_cancel_epoch) {
        (Some(callback_id), Some(epoch)) => Some(IssuedCallback {
            callback_id,
            cancel_epoch: CancelEpoch::new(epoch),
        }),
        (None, None) => None,
        _ => {
            return Err(StoreError::CorruptRecord(
                "issued callback identity and epoch disagree",
            ));
        }
    };
    let accepted_callback = optional_blob16(row, 11)?
        .map(CallbackId::from_bytes)
        .map(|callback_id| {
            completion_from_state(state).map(|outcome| AcceptedCallback {
                callback_id,
                outcome,
            })
        })
        .transpose()?;
    let revision: i64 = row.get(12)?;
    if revision < 0 {
        return Err(StoreError::CorruptRecord("negative operation revision"));
    }
    let machine = OperationMachine::restore(
        spec,
        cancel_epoch,
        state,
        issued_callback,
        accepted_callback,
    )?;
    Ok(Some((machine, revision)))
}

fn decode_outbox_row(row: &rusqlite::Row<'_>) -> Result<OutboxEntry, StoreError> {
    let sequence: i64 = row.get(0)?;
    let kind = decode_outbox_kind(row.get(1)?)?;
    let operation = OperationHandle {
        operation_id: OperationId::from_bytes(blob16(row, 2)?),
        generation: generation_from_blob(row, 3)?,
    };
    let owner_fiber = FiberHandle {
        fiber_id: ExecutionFiberId::from_bytes(blob16(row, 4)?),
        generation: generation_from_blob(row, 5)?,
    };
    let callback_id = optional_blob16(row, 6)?.map(CallbackId::from_bytes);
    let state_kind: i64 = row.get(7)?;
    let receipt_id = ReceiptId::from_bytes(blob16(row, 8)?);
    Ok(OutboxEntry {
        sequence,
        kind,
        operation,
        owner_fiber,
        callback_id,
        state: decode_state(state_kind, Some(receipt_id))?,
    })
}

struct EncodedMachine {
    operation_id: [u8; 16],
    generation: [u8; 8],
    owner_fiber_id: [u8; 16],
    owner_fiber_generation: [u8; 8],
    cancellation_scope_id: [u8; 16],
    cancellation_generation: [u8; 8],
    cancel_epoch: [u8; 8],
    state_kind: i64,
    receipt_id: Option<[u8; 16]>,
    issued_callback_id: Option<[u8; 16]>,
    issued_cancel_epoch: Option<[u8; 8]>,
    accepted_callback_id: Option<[u8; 16]>,
}

impl EncodedMachine {
    fn from_machine(machine: &OperationMachine) -> Self {
        let spec = machine.spec();
        let (state_kind, receipt_id) = encode_state(machine.snapshot().state);
        let issued = machine.issued_callback();
        Self {
            operation_id: spec.operation_id.into_bytes(),
            generation: encode_u64(spec.generation.get()),
            owner_fiber_id: spec.owner_fiber.fiber_id.into_bytes(),
            owner_fiber_generation: encode_u64(spec.owner_fiber.generation.get()),
            cancellation_scope_id: spec.cancellation_scope_id.into_bytes(),
            cancellation_generation: encode_u64(spec.cancellation_generation.get()),
            cancel_epoch: encode_u64(machine.snapshot().cancel_epoch.get()),
            state_kind,
            receipt_id: receipt_id.map(ReceiptId::into_bytes),
            issued_callback_id: issued.map(|callback| callback.callback_id.into_bytes()),
            issued_cancel_epoch: issued.map(|callback| encode_u64(callback.cancel_epoch.get())),
            accepted_callback_id: machine
                .accepted_callback()
                .map(|callback| callback.callback_id.into_bytes()),
        }
    }
}

fn encode_state(state: OperationState) -> (i64, Option<ReceiptId>) {
    match state {
        OperationState::Registered => (0, None),
        OperationState::Dispatched => (1, None),
        OperationState::CancelRequested => (2, None),
        OperationState::Completed { receipt_id } => (10, Some(receipt_id)),
        OperationState::Failed { receipt_id } => (11, Some(receipt_id)),
        OperationState::CancelledBeforeEffect { receipt_id } => (12, Some(receipt_id)),
        OperationState::PartialEffect { receipt_id } => (13, Some(receipt_id)),
        OperationState::EffectUnknown { receipt_id } => (14, Some(receipt_id)),
    }
}

fn decode_state(kind: i64, receipt: Option<ReceiptId>) -> Result<OperationState, StoreError> {
    let terminal_receipt =
        || receipt.ok_or(StoreError::CorruptRecord("terminal state lacks receipt"));
    match kind {
        0 if receipt.is_none() => Ok(OperationState::Registered),
        1 if receipt.is_none() => Ok(OperationState::Dispatched),
        2 if receipt.is_none() => Ok(OperationState::CancelRequested),
        10 => Ok(OperationState::Completed {
            receipt_id: terminal_receipt()?,
        }),
        11 => Ok(OperationState::Failed {
            receipt_id: terminal_receipt()?,
        }),
        12 => Ok(OperationState::CancelledBeforeEffect {
            receipt_id: terminal_receipt()?,
        }),
        13 => Ok(OperationState::PartialEffect {
            receipt_id: terminal_receipt()?,
        }),
        14 => Ok(OperationState::EffectUnknown {
            receipt_id: terminal_receipt()?,
        }),
        0..=2 => Err(StoreError::CorruptRecord(
            "non-terminal state unexpectedly carries receipt",
        )),
        _ => Err(StoreError::CorruptRecord("unknown operation state")),
    }
}

fn completion_from_state(state: OperationState) -> Result<CompletionOutcome, StoreError> {
    match state {
        OperationState::Completed { receipt_id } => Ok(CompletionOutcome::Completed { receipt_id }),
        OperationState::Failed { receipt_id } => Ok(CompletionOutcome::Failed { receipt_id }),
        OperationState::CancelledBeforeEffect { receipt_id } => {
            Ok(CompletionOutcome::CancelledBeforeEffect { receipt_id })
        }
        OperationState::PartialEffect { receipt_id } => {
            Ok(CompletionOutcome::PartialEffect { receipt_id })
        }
        OperationState::EffectUnknown { receipt_id } => {
            Ok(CompletionOutcome::EffectUnknown { receipt_id })
        }
        _ => Err(StoreError::CorruptRecord(
            "accepted callback references non-terminal state",
        )),
    }
}

fn receipt_from_state(state: OperationState) -> Option<ReceiptId> {
    encode_state(state).1
}

const fn encode_outbox_kind(kind: OutboxKind) -> i64 {
    match kind {
        OutboxKind::WakeFiber => 0,
        OutboxKind::ReconcileEffect => 1,
    }
}

fn decode_outbox_kind(kind: i64) -> Result<OutboxKind, StoreError> {
    match kind {
        0 => Ok(OutboxKind::WakeFiber),
        1 => Ok(OutboxKind::ReconcileEffect),
        _ => Err(StoreError::CorruptRecord("unknown outbox kind")),
    }
}

const fn encode_u64(value: u64) -> [u8; 8] {
    value.to_be_bytes()
}

fn generation_from_blob(row: &rusqlite::Row<'_>, index: usize) -> Result<Generation, StoreError> {
    let value = u64_from_blob(row, index)?;
    let non_zero =
        std::num::NonZeroU64::new(value).ok_or(StoreError::CorruptRecord("zero generation"))?;
    Ok(Generation::new(non_zero))
}

fn u64_from_blob(row: &rusqlite::Row<'_>, index: usize) -> Result<u64, StoreError> {
    Ok(u64::from_be_bytes(blob8(row, index)?))
}

fn blob16(row: &rusqlite::Row<'_>, index: usize) -> Result<[u8; 16], StoreError> {
    let value: Vec<u8> = row.get(index)?;
    value
        .try_into()
        .map_err(|_| StoreError::CorruptRecord("expected 16-byte blob"))
}

fn optional_blob16(row: &rusqlite::Row<'_>, index: usize) -> Result<Option<[u8; 16]>, StoreError> {
    let value: Option<Vec<u8>> = row.get(index)?;
    value
        .map(|bytes| {
            bytes
                .try_into()
                .map_err(|_| StoreError::CorruptRecord("expected optional 16-byte blob"))
        })
        .transpose()
}

fn blob8(row: &rusqlite::Row<'_>, index: usize) -> Result<[u8; 8], StoreError> {
    let value: Vec<u8> = row.get(index)?;
    value
        .try_into()
        .map_err(|_| StoreError::CorruptRecord("expected 8-byte blob"))
}

fn optional_blob8(row: &rusqlite::Row<'_>, index: usize) -> Result<Option<[u8; 8]>, StoreError> {
    let value: Option<Vec<u8>> = row.get(index)?;
    value
        .map(|bytes| {
            bytes
                .try_into()
                .map_err(|_| StoreError::CorruptRecord("expected optional 8-byte blob"))
        })
        .transpose()
}
