//! Single-node durable authority store for NLOS operations.
//!
//! Operation state and its wake/reconciliation notification are committed in
//! one `SQLite` transaction. Consumers must acknowledge outbox entries only
//! after applying them idempotently; a crash may therefore redeliver an entry,
//! but cannot lose a committed transition.
//!
//! A persistently unappliable entry may additionally be *parked* (schema v5
//! dead-letter parking, W57-B): [`SqliteOperationStore::park_outbox_entry`]
//! stamps it one-way with a timestamp and a human-facing reason, and
//! [`SqliteOperationStore::pending_outbox`] then skips it so later entries
//! flow. Parking is never an acknowledgement — the entry stays durable and
//! unacknowledged for manual adjudication through
//! [`SqliteOperationStore::inspect_parked_outbox`].
//!
//! A parked entry may be *unparked* by explicit human adjudication (schema
//! v6, W58-1): [`SqliteOperationStore::unpark_outbox_entry`] performs the
//! one controlled reverse of the parking columns — both go back to `NULL`
//! in the same transaction that stamps the unpark adjudication evidence —
//! and the entry then rejoins [`SqliteOperationStore::pending_outbox`] in
//! durable sequence order. Every other rewrite of the parking facts stays
//! aborted by the storage triggers, and
//! [`SqliteOperationStore::inspect_outbox_park_history`] renders the full
//! parked/recovered history with the per-entry park counts.

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
    ApplicationId, CallbackId, CancelEpoch, CancellationScopeId, ExecutionFiberId, Generation,
    IdempotencyKey, OperationId, ReceiptId, TaskParticipantId,
};
use rusqlite::{Connection, OpenFlags, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};

const SCHEMA_VERSION: i64 = 6;
const MAX_ENDPOINT_COMPONENT_BYTES: usize = 128;
const MAX_DURABLE_RESULT_BYTES: usize = 1024 * 1024;
const MAX_PARK_REASON_BYTES: usize = 1024;

#[derive(Debug)]
pub enum StoreError {
    Sqlite(rusqlite::Error),
    Operation(OperationError),
    CorruptRecord(&'static str),
    UnsupportedSchema(i64),
    OutboxEntryNotFound,
    OutboxParkConflict,
    /// Fail-closed refusal of a parking-lane request whose shape is invalid
    /// for the durable row it names. This one variant serves both directions
    /// of the parking family (W57-B park, W58-1 unpark): malformed
    /// reasons/timestamps, an unpark of an entry that was never parked, and
    /// an unpark replay carrying a different reason than the durable
    /// adjudication. There are deliberately no per-direction variants —
    /// downstream consumers (for example the mock driver's retry mapping)
    /// match this enum exhaustively, and every parking-lane refusal shares
    /// the same operational meaning: a request the authority will never
    /// execute as stated.
    InvalidParkRequest(&'static str),
    InvalidIdempotencyScope,
    IdempotencyConflict,
    IdempotencyRecordNotFound,
    DispatchPreparationNotFound,
    DispatchPreparationConflict,
    OperationNotActivated,
    CancelEpochConflict {
        expected: u64,
        current: u64,
    },
    DurableResultTooLarge {
        actual: usize,
        maximum: usize,
    },
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
            Self::OutboxParkConflict => formatter.write_str(
                "outbox entry is already parked with a different park reason; parking is one-way",
            ),
            Self::InvalidParkRequest(reason) => {
                write!(formatter, "invalid outbox parking request: {reason}")
            }
            Self::InvalidIdempotencyScope => formatter.write_str(
                "idempotency service and method must be non-empty bounded strings without NUL",
            ),
            Self::IdempotencyConflict => formatter
                .write_str("idempotency key was reused for different request or result bytes"),
            Self::IdempotencyRecordNotFound => {
                formatter.write_str("operation has no durable idempotency record")
            }
            Self::DispatchPreparationNotFound => {
                formatter.write_str("operation has no durable dispatch preparation")
            }
            Self::DispatchPreparationConflict => formatter.write_str(
                "operation dispatch preparation does not match the durable owner-bound request",
            ),
            Self::OperationNotActivated => {
                formatter.write_str("operation has no durable dispatch activation")
            }
            Self::CancelEpochConflict { expected, current } => write!(
                formatter,
                "operation cancel epoch conflict: expected {expected}, current {current}"
            ),
            Self::DurableResultTooLarge { actual, maximum } => write!(
                formatter,
                "durable result exceeds bound: {actual} bytes (maximum {maximum})"
            ),
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

/// Durable owner-bound preparation for an Operation dispatch.
///
/// A preparation is not an effect permit and does not transition the
/// Operation. It records the exact callback identity and owner facts that the
/// later activation must present again after a restart.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OperationDispatchPreparation {
    pub operation: OperationHandle,
    pub owner_fiber: FiberHandle,
    pub cancellation_scope_id: CancellationScopeId,
    pub cancellation_generation: Generation,
    pub callback_id: CallbackId,
    pub cancel_epoch: CancelEpoch,
    pub preparation_receipt_id: ReceiptId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationPrepareDecision {
    Prepared(OperationDispatchPreparation),
    Replayed(OperationDispatchPreparation),
}

/// Durable result of consuming an Operation dispatch preparation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OperationDispatchActivation {
    pub preparation: OperationDispatchPreparation,
    pub ticket: CallbackTicket,
    pub activation_receipt_id: ReceiptId,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationActivationDecision {
    Activated(OperationDispatchActivation),
    Replayed(OperationDispatchActivation),
}

/// Stable authority scope for an application-visible idempotency key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IdempotencyScope {
    pub application_id: ApplicationId,
    pub service: String,
    pub method: String,
}

/// Stable service-result bytes for a completed idempotent call.
///
/// Transport adapters must build a fresh envelope for each exchange; volatile
/// request/correlation identifiers must not be stored in this value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DurableCallResult {
    pub operation: OperationHandle,
    pub receipt_id: ReceiptId,
    pub result_wire: Vec<u8>,
}

/// Result of atomically claiming an idempotency key and registering its Operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum IdempotencyDecision {
    /// This caller durably claimed the key and may dispatch the Operation once.
    Created(OperationHandle),
    /// The key already names the same request. Query this Operation; do not redispatch.
    PendingOrUncertain(OperationHandle),
    /// The original immutable result is available for exact replay.
    Completed(DurableCallResult),
}

impl IdempotencyDecision {
    #[must_use]
    pub fn operation(&self) -> OperationHandle {
        match self {
            Self::Created(operation) | Self::PendingOrUncertain(operation) => *operation,
            Self::Completed(result) => result.operation,
        }
    }
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

/// Linearized result of an idempotent Operation cancellation request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancelRequestDecision {
    Applied(OperationSnapshot),
    Replayed(OperationSnapshot),
    AlreadyTerminal(OperationSnapshot),
}

impl CancelRequestDecision {
    #[must_use]
    pub const fn snapshot(self) -> OperationSnapshot {
        match self {
            Self::Applied(snapshot)
            | Self::Replayed(snapshot)
            | Self::AlreadyTerminal(snapshot) => snapshot,
        }
    }
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

/// Linearized result of a one-way outbox parking request (W57-B).
///
/// `Parked` stamped this call; `Replayed` proves the entry was already parked
/// with the exact same reason and returns the original parking timestamp.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutboxParkDecision {
    Parked { sequence: i64, parked_at_ms: i64 },
    Replayed { sequence: i64, parked_at_ms: i64 },
}

/// Linearized result of a controlled outbox unpark request (W58-1).
///
/// `Unparked` performed the reverse this call: the parking columns went
/// back to `NULL`, the entry rejoined the pending lane, and the unpark
/// adjudication (timestamp + reason) became durable evidence. `Replayed`
/// proves the entry was already unparked with the exact same reason and
/// returns the original unpark timestamp without rewriting anything.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutboxUnparkDecision {
    Unparked { sequence: i64, unparked_at_ms: i64 },
    Replayed { sequence: i64, unparked_at_ms: i64 },
}

/// One parked (dead-letter) outbox entry as observed by
/// [`SqliteOperationStore::inspect_parked_outbox`].
///
/// `acknowledged` is carried deliberately: parking is never an
/// acknowledgement, so a healthy parked row reads `acknowledged == false`
/// until a human adjudicates it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParkedOutboxEntry {
    pub sequence: i64,
    pub kind: OutboxKind,
    pub operation: OperationHandle,
    pub owner_fiber: FiberHandle,
    pub callback_id: Option<CallbackId>,
    pub state: OperationState,
    pub acknowledged: bool,
    pub parked_at_ms: i64,
    pub park_reason: String,
}

/// One outbox entry that has been parked at least once, as observed by
/// [`SqliteOperationStore::inspect_outbox_park_history`]: currently parked
/// (dead-letter) rows and already-unparked (recovered) rows alike.
///
/// This is the W58-1 audit surface of the parking family: `parked` is the
/// "停泊/已恢复" status, `park_count`/`unpark_count` are the per-entry
/// counts, and the last park/unpark facts carry their adjudication
/// evidence. The last park facts are `None` once unparked — the controlled
/// reverse clears the parking columns by design — while the unpark facts
/// survive (a later re-park may again supersede the parked state).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OutboxParkHistoryEntry {
    pub sequence: i64,
    pub kind: OutboxKind,
    pub operation: OperationHandle,
    pub owner_fiber: FiberHandle,
    pub callback_id: Option<CallbackId>,
    pub state: OperationState,
    pub acknowledged: bool,
    /// `true` while the entry sits in the parked (dead-letter) lane;
    /// `false` once a manual unpark recovered it into the pending lane.
    pub parked: bool,
    /// How many times this entry was ever parked (a row parked under the
    /// v5 one-way schema reads exactly 1).
    pub park_count: i64,
    /// How many times a manual unpark recovered this entry.
    pub unpark_count: i64,
    /// Timestamp and reason of the most recent park; `None` once unparked.
    pub parked_at_ms: Option<i64>,
    pub park_reason: Option<String>,
    /// Timestamp and reason of the most recent unpark adjudication; `None`
    /// while the entry was never unparked.
    pub unparked_at_ms: Option<i64>,
    pub unpark_reason: Option<String>,
}

/// Authority-derived proof for the current Operation endpoint.
///
/// The proof is derived from the durable Operation registration row and is
/// authoritative only after exact `OperationId + Generation` readback. A
/// caller may transport the tuple, but a consumer must query the owning
/// `SqliteOperationStore` again before admitting it into a `TaskWriteSet`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OperationEndpointProof {
    pub operation: OperationHandle,
    pub owner_fiber: FiberHandle,
    pub cancellation_scope_id: CancellationScopeId,
    pub cancellation_generation: Generation,
    pub participant_id: TaskParticipantId,
    pub participant_generation: Generation,
    pub admission_receipt_id: ReceiptId,
}

/// Authority-derived proof that an Operation's prepared dispatch was
/// durably activated.  This is intentionally separate from
/// [`OperationEndpointProof`]: registration/participant admission does not
/// imply that the one-shot dispatch boundary has opened.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OperationActivationProof {
    pub operation: OperationHandle,
    pub preparation_receipt_id: ReceiptId,
    pub activation_receipt_id: ReceiptId,
    pub callback_id: CallbackId,
    pub cancel_epoch: CancelEpoch,
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

    /// Durably prepares an Operation for a later owner-bound activation.
    ///
    /// Preparation records the callback identity and the current owner/cancel
    /// facts without changing the Operation state. Repeating the exact
    /// request returns the same preparation after a restart; a different
    /// callback for the same Operation is rejected. The legacy direct
    /// [`Self::dispatch`] path is fenced while a preparation exists, so a
    /// caller cannot bypass the durable prepare/activate boundary.
    ///
    /// # Errors
    ///
    /// Returns a stale-generation, state, conflict, or storage error.
    pub fn prepare_dispatch(
        &self,
        handle: OperationHandle,
        callback_id: CallbackId,
    ) -> Result<OperationPrepareDecision, StoreError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (machine, _) = load_machine(&transaction, handle.operation_id)?;
        let snapshot = machine.snapshot();
        if snapshot.handle != handle {
            return Err(OperationError::InvalidGeneration.into());
        }

        if let Some(existing) = load_dispatch_preparation_optional(&transaction, handle)? {
            if dispatch_preparation_matches_machine(&existing, &machine, callback_id) {
                transaction.commit()?;
                return Ok(OperationPrepareDecision::Replayed(existing));
            }
            return Err(StoreError::DispatchPreparationConflict);
        }
        if snapshot.state != OperationState::Registered {
            return Err(OperationError::InvalidState.into());
        }

        let expected = dispatch_preparation_from_machine(&machine, callback_id);
        insert_dispatch_preparation(&transaction, &expected)?;
        transaction.commit()?;
        Ok(OperationPrepareDecision::Prepared(expected))
    }

    /// Atomically consumes a durable dispatch preparation and transitions the
    /// Operation to `DISPATCHED`.
    ///
    /// The owner facts, callback identity and cancel epoch are all re-read
    /// from the Operation authority. The activation receipt is immutable and
    /// replaying the same preparation returns the original fenced callback
    /// ticket without dispatching again.
    ///
    /// # Errors
    ///
    /// Returns a stale-generation, missing-preparation, conflict, state, or
    /// storage error.
    pub fn activate_dispatch(
        &self,
        preparation: OperationDispatchPreparation,
    ) -> Result<OperationActivationDecision, StoreError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (mut machine, revision) =
            load_machine(&transaction, preparation.operation.operation_id)?;
        if machine.snapshot().handle != preparation.operation {
            return Err(OperationError::InvalidGeneration.into());
        }
        let durable_preparation =
            load_dispatch_preparation_optional(&transaction, preparation.operation)?
                .ok_or(StoreError::DispatchPreparationNotFound)?;
        if durable_preparation != preparation {
            return Err(StoreError::DispatchPreparationConflict);
        }

        let activation_receipt_id = ReceiptId::from_bytes(derive_operation_dispatch_receipt_id(
            b"nlos/operation-dispatch/activation/v1",
            preparation.operation,
            preparation.callback_id,
        ));
        if let Some(existing) =
            load_dispatch_activation_optional(&transaction, preparation.operation)?
        {
            if existing.operation != preparation.operation
                || existing.preparation_receipt_id != preparation.preparation_receipt_id
                || existing.activation_receipt_id != activation_receipt_id
                || existing.callback_id != preparation.callback_id
                || existing.cancel_epoch != preparation.cancel_epoch
                || !dispatch_preparation_matches_machine(
                    &preparation,
                    &machine,
                    preparation.callback_id,
                )
            {
                return Err(StoreError::DispatchPreparationConflict);
            }
            let issued = machine.issued_callback().ok_or(StoreError::CorruptRecord(
                "activation receipt lacks issued callback",
            ))?;
            if issued.callback_id != existing.callback_id
                || issued.cancel_epoch != existing.cancel_epoch
                || machine.snapshot().state == OperationState::Registered
            {
                return Err(StoreError::CorruptRecord(
                    "activation receipt disagrees with Operation state",
                ));
            }
            let activation = OperationDispatchActivation {
                preparation,
                ticket: CallbackTicket {
                    callback_id: existing.callback_id,
                    operation: preparation.operation,
                    owner_fiber: preparation.owner_fiber,
                    cancel_epoch: existing.cancel_epoch,
                },
                activation_receipt_id: existing.activation_receipt_id,
            };
            transaction.commit()?;
            return Ok(OperationActivationDecision::Replayed(activation));
        }

        if machine.snapshot().state != OperationState::Registered {
            return Err(OperationError::InvalidState.into());
        }
        if !dispatch_preparation_matches_machine(&preparation, &machine, preparation.callback_id) {
            return Err(StoreError::DispatchPreparationConflict);
        }
        let ticket = machine.dispatch(preparation.operation, preparation.callback_id)?;
        update_machine(&transaction, &machine, revision)?;
        insert_dispatch_activation(&transaction, preparation, activation_receipt_id)?;
        transaction.commit()?;
        Ok(OperationActivationDecision::Activated(
            OperationDispatchActivation {
                preparation,
                ticket,
                activation_receipt_id,
            },
        ))
    }

    /// Atomically claims a scoped idempotency key and registers its Operation.
    ///
    /// Exact replays return the existing Operation or its immutable result.
    /// Reusing a key with a different request digest is rejected. A replay of a
    /// nonterminal record never grants dispatch authority again.
    ///
    /// # Errors
    ///
    /// Returns a validation, conflict, operation-registration, or storage error.
    pub fn begin_idempotent_operation(
        &self,
        scope: &IdempotencyScope,
        idempotency_key: IdempotencyKey,
        request_digest_sha256: [u8; 32],
        spec: OperationSpec,
    ) -> Result<IdempotencyDecision, StoreError> {
        validate_idempotency_scope(scope)?;
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = load_idempotency_record(&transaction, scope, idempotency_key)? {
            if existing.request_digest_sha256 != request_digest_sha256 {
                return Err(StoreError::IdempotencyConflict);
            }
            transaction.commit()?;
            return Ok(existing.into_decision());
        }

        if let Some(existing) = load_machine_optional(&transaction, spec.operation_id)? {
            if existing.spec() != spec {
                return Err(OperationError::DuplicateOperation.into());
            }
            return Err(StoreError::IdempotencyConflict);
        }

        let machine = OperationMachine::new(spec);
        insert_machine(&transaction, &machine)?;
        let operation = machine.snapshot().handle;
        transaction.execute(
            "INSERT INTO idempotent_calls (
                application_id, service, method, idempotency_key, request_digest_sha256,
                operation_id, operation_generation, receipt_id, response_wire
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, NULL)",
            params![
                scope.application_id.as_bytes().as_slice(),
                scope.service,
                scope.method,
                idempotency_key.as_bytes().as_slice(),
                request_digest_sha256.as_slice(),
                operation.operation_id.as_bytes().as_slice(),
                encode_u64(operation.generation.get()).as_slice(),
            ],
        )?;
        transaction.commit()?;
        Ok(IdempotencyDecision::Created(operation))
    }

    /// Looks up a scoped idempotency key without claiming or dispatching it.
    ///
    /// This is the authority-side query path used to attach the original
    /// Operation to conflict/uncertain responses.
    ///
    /// # Errors
    ///
    /// Returns a scope-validation, corrupt-record, lock, or storage error.
    pub fn inspect_idempotent_operation(
        &self,
        scope: &IdempotencyScope,
        idempotency_key: IdempotencyKey,
    ) -> Result<Option<IdempotencyDecision>, StoreError> {
        validate_idempotency_scope(scope)?;
        let connection = self.lock_connection()?;
        Ok(
            load_idempotency_record(&*connection, scope, idempotency_key)?
                .map(IdempotencyRecord::into_decision),
        )
    }

    /// Completes an idempotent Operation and stores its stable result bytes.
    ///
    /// The terminal Operation transition, Receipt identity, immutable result,
    /// and wake/reconciliation Outbox entry commit in one transaction.
    ///
    /// # Errors
    ///
    /// Returns a bound, conflict, callback, state, or storage error.
    pub fn complete_idempotent_operation(
        &self,
        ticket: CallbackTicket,
        outcome: CompletionOutcome,
        result_wire: &[u8],
    ) -> Result<DurableCallResult, StoreError> {
        if result_wire.len() > MAX_DURABLE_RESULT_BYTES {
            return Err(StoreError::DurableResultTooLarge {
                actual: result_wire.len(),
                maximum: MAX_DURABLE_RESULT_BYTES,
            });
        }
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (mut machine, revision) = load_machine(&transaction, ticket.operation.operation_id)?;
        let decision = machine.complete(ticket, outcome)?;
        let record = load_idempotency_record_by_operation(&transaction, ticket.operation)?
            .ok_or(StoreError::IdempotencyRecordNotFound)?;
        let receipt_id = receipt_from_state(machine.snapshot().state).ok_or(
            StoreError::CorruptRecord("idempotent completion lacks final receipt"),
        )?;

        if let Some(existing) = record.result {
            if existing.receipt_id != receipt_id || existing.result_wire != result_wire {
                return Err(StoreError::IdempotencyConflict);
            }
            transaction.commit()?;
            return Ok(existing);
        }

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
        let changed = transaction.execute(
            "UPDATE idempotent_calls
             SET receipt_id = ?1, response_wire = ?2
             WHERE operation_id = ?3 AND operation_generation = ?4
               AND receipt_id IS NULL AND response_wire IS NULL",
            params![
                receipt_id.as_bytes().as_slice(),
                result_wire,
                ticket.operation.operation_id.as_bytes().as_slice(),
                encode_u64(ticket.operation.generation.get()).as_slice(),
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::CorruptRecord(
                "idempotency result compare-and-set failed",
            ));
        }
        let result = DurableCallResult {
            operation: ticket.operation,
            receipt_id,
            result_wire: result_wire.to_vec(),
        };
        transaction.commit()?;
        Ok(result)
    }

    /// Cancels an idempotent Operation before dispatch and stores its stable result.
    ///
    /// The no-effect terminal transition, Receipt identity, immutable result,
    /// and wake Outbox entry commit in one transaction. Exact retries return
    /// the original result; this method never converts a dispatched Operation
    /// into `CancelledBeforeEffect`.
    ///
    /// # Errors
    ///
    /// Returns a bound, conflict, state, or storage error.
    pub fn cancel_idempotent_before_dispatch(
        &self,
        handle: OperationHandle,
        no_effect_receipt: ReceiptId,
        result_wire: &[u8],
    ) -> Result<DurableCallResult, StoreError> {
        if result_wire.len() > MAX_DURABLE_RESULT_BYTES {
            return Err(StoreError::DurableResultTooLarge {
                actual: result_wire.len(),
                maximum: MAX_DURABLE_RESULT_BYTES,
            });
        }
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (mut machine, revision) = load_machine(&transaction, handle.operation_id)?;
        let record = load_idempotency_record_by_operation(&transaction, handle)?
            .ok_or(StoreError::IdempotencyRecordNotFound)?;

        if let Some(existing) = record.result {
            if existing.receipt_id != no_effect_receipt || existing.result_wire != result_wire {
                return Err(StoreError::IdempotencyConflict);
            }
            transaction.commit()?;
            return Ok(existing);
        }
        if machine.snapshot().state != OperationState::Registered {
            return Err(OperationError::InvalidState.into());
        }

        let snapshot = machine.request_cancel(handle, no_effect_receipt)?;
        if !matches!(snapshot.state, OperationState::CancelledBeforeEffect { .. }) {
            return Err(StoreError::CorruptRecord(
                "pre-dispatch cancellation did not produce a no-effect terminal state",
            ));
        }
        update_machine(&transaction, &machine, revision)?;
        insert_outbox(&transaction, OutboxKind::WakeFiber, &machine, None)?;
        let changed = transaction.execute(
            "UPDATE idempotent_calls
             SET receipt_id = ?1, response_wire = ?2
             WHERE operation_id = ?3 AND operation_generation = ?4
               AND receipt_id IS NULL AND response_wire IS NULL",
            params![
                no_effect_receipt.as_bytes().as_slice(),
                result_wire,
                handle.operation_id.as_bytes().as_slice(),
                encode_u64(handle.generation.get()).as_slice(),
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::CorruptRecord(
                "idempotency no-effect result compare-and-set failed",
            ));
        }
        let result = DurableCallResult {
            operation: handle,
            receipt_id: no_effect_receipt,
            result_wire: result_wire.to_vec(),
        };
        transaction.commit()?;
        Ok(result)
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
        if machine.snapshot().handle != handle {
            return Err(OperationError::InvalidGeneration.into());
        }
        if load_dispatch_preparation_optional(&transaction, handle)?.is_some() {
            return Err(OperationError::InvalidState.into());
        }
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

    /// Commits an Operation cancellation with an explicit cancel-epoch fence.
    ///
    /// The first request at `expected_cancel_epoch` advances the epoch exactly
    /// once. An exact retry observes the already-advanced state and returns it
    /// without emitting another Outbox item. If completion won before the
    /// cancellation CAS, its terminal state is returned without rewriting
    /// history. Any other epoch mismatch fails closed.
    ///
    /// # Errors
    ///
    /// Returns a stale handle, epoch conflict, state, or storage error.
    pub fn request_cancel_idempotent(
        &self,
        handle: OperationHandle,
        expected_cancel_epoch: CancelEpoch,
        no_effect_receipt: ReceiptId,
    ) -> Result<CancelRequestDecision, StoreError> {
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (mut machine, revision) = load_machine(&transaction, handle.operation_id)?;
        let before = machine.snapshot();
        if before.handle != handle {
            return Err(OperationError::InvalidGeneration.into());
        }

        if before.cancel_epoch == expected_cancel_epoch {
            if before.state.is_terminal() {
                transaction.commit()?;
                return Ok(CancelRequestDecision::AlreadyTerminal(before));
            }
            let snapshot = machine.request_cancel(handle, no_effect_receipt)?;
            update_machine(&transaction, &machine, revision)?;
            if matches!(snapshot.state, OperationState::CancelledBeforeEffect { .. }) {
                insert_outbox(&transaction, OutboxKind::WakeFiber, &machine, None)?;
            }
            transaction.commit()?;
            return Ok(CancelRequestDecision::Applied(snapshot));
        }

        if expected_cancel_epoch.checked_next() == Some(before.cancel_epoch)
            && matches!(
                before.state,
                OperationState::CancelRequested
                    | OperationState::CancelledBeforeEffect { .. }
                    | OperationState::Completed { .. }
                    | OperationState::Failed { .. }
                    | OperationState::PartialEffect { .. }
                    | OperationState::EffectUnknown { .. }
            )
        {
            if let OperationState::CancelledBeforeEffect { receipt_id } = before.state
                && receipt_id != no_effect_receipt
            {
                return Err(StoreError::IdempotencyConflict);
            }
            transaction.commit()?;
            return Ok(CancelRequestDecision::Replayed(before));
        }

        Err(StoreError::CancelEpochConflict {
            expected: expected_cancel_epoch.get(),
            current: before.cancel_epoch.get(),
        })
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

    /// Reads the authority-derived endpoint proof for an Operation.
    ///
    /// The proof is deterministic from the immutable registration identity and
    /// the current generation, but it is authoritative only after this exact
    /// owner readback. A stale or unknown generation is rejected before any
    /// participant tuple is returned.
    ///
    /// # Errors
    ///
    /// Returns a storage, corruption, or stale-generation error.
    pub fn inspect_endpoint_proof(
        &self,
        handle: OperationHandle,
    ) -> Result<OperationEndpointProof, StoreError> {
        let connection = self.lock_connection()?;
        let (machine, _) = load_machine(&*connection, handle.operation_id)?;
        let spec = machine.spec();
        if spec.generation != handle.generation {
            return Err(OperationError::InvalidGeneration.into());
        }
        let participant_id = TaskParticipantId::from_bytes(derive_endpoint_id(
            b"nlos/operation-endpoint/participant/v1",
            spec.operation_id,
            spec.generation,
        ));
        let admission_receipt_id = ReceiptId::from_bytes(derive_endpoint_id(
            b"nlos/operation-endpoint/admission/v1",
            spec.operation_id,
            spec.generation,
        ));
        Ok(OperationEndpointProof {
            operation: handle,
            owner_fiber: spec.owner_fiber,
            cancellation_scope_id: spec.cancellation_scope_id,
            cancellation_generation: spec.cancellation_generation,
            participant_id,
            participant_generation: spec.generation,
            admission_receipt_id,
        })
    }

    /// Reads and validates the immutable owner activation receipt for an
    /// Operation.  A registered or merely prepared Operation is rejected;
    /// the proof is returned only after exact `OperationId + Generation`
    /// readback and cross-checking against the durable preparation, callback
    /// fence and current Operation state.
    ///
    /// # Errors
    ///
    /// Returns a stale-generation, missing-preparation, unactivated,
    /// corrupt-record, or storage error.
    pub fn inspect_activation_proof(
        &self,
        handle: OperationHandle,
    ) -> Result<OperationActivationProof, StoreError> {
        let connection = self.lock_connection()?;
        let (machine, _) = load_machine(&*connection, handle.operation_id)?;
        if machine.snapshot().handle != handle {
            return Err(OperationError::InvalidGeneration.into());
        }
        let preparation = load_dispatch_preparation_optional(&*connection, handle)?
            .ok_or(StoreError::DispatchPreparationNotFound)?;
        let activation = load_dispatch_activation_optional(&*connection, handle)?
            .ok_or(StoreError::OperationNotActivated)?;
        if activation.operation != handle
            || activation.preparation_receipt_id != preparation.preparation_receipt_id
            || activation.callback_id != preparation.callback_id
            || activation.cancel_epoch != preparation.cancel_epoch
        {
            return Err(StoreError::CorruptRecord(
                "dispatch activation disagrees with preparation",
            ));
        }
        let expected_activation_receipt_id =
            ReceiptId::from_bytes(derive_operation_dispatch_receipt_id(
                b"nlos/operation-dispatch/activation/v1",
                handle,
                preparation.callback_id,
            ));
        if activation.activation_receipt_id != expected_activation_receipt_id {
            return Err(StoreError::CorruptRecord(
                "dispatch activation receipt identity mismatch",
            ));
        }
        let issued = machine.issued_callback().ok_or(StoreError::CorruptRecord(
            "dispatch activation lacks issued callback",
        ))?;
        if issued.callback_id != activation.callback_id
            || issued.cancel_epoch != activation.cancel_epoch
            || machine.snapshot().state == OperationState::Registered
        {
            return Err(StoreError::CorruptRecord(
                "dispatch activation disagrees with Operation state",
            ));
        }
        Ok(OperationActivationProof {
            operation: handle,
            preparation_receipt_id: preparation.preparation_receipt_id,
            activation_receipt_id: activation.activation_receipt_id,
            callback_id: activation.callback_id,
            cancel_epoch: activation.cancel_epoch,
        })
    }

    /// Lists unacknowledged, unparked outbox entries in durable sequence
    /// order.
    ///
    /// Parked (dead-letter) entries are skipped: they stay durable and
    /// observable through [`Self::inspect_parked_outbox`], but no longer
    /// block the queue head for consumers. A manual unpark (W58-1) clears
    /// the parked columns, so the recovered entry rejoins this listing at
    /// its durable sequence position — the queue head restores in order.
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
             WHERE acknowledged = 0 AND parked_at_ms IS NULL
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
    /// Parking does not change this surface: a parked entry may still be
    /// acknowledged (by an explicit adjudication path), but parking itself
    /// never acknowledges.
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

    /// Parks a persistently unappliable outbox entry one-way (W57-B
    /// dead-letter parking).
    ///
    /// Parking stamps `parked_at_ms`/`park_reason` on the entry and advances
    /// `park_count`; from then on [`Self::pending_outbox`] skips it, so later
    /// entries stop queueing behind it. Parking is **not** an
    /// acknowledgement: the entry keeps `acknowledged = 0` and stays durable
    /// for manual adjudication through [`Self::inspect_parked_outbox`]. The
    /// only legal successor of a parked state is the controlled unpark
    /// reverse ([`Self::unpark_outbox_entry`], W58-1); after an unpark the
    /// entry may be parked again, and every such cycle advances the counts
    /// on [`Self::inspect_outbox_park_history`].
    ///
    /// Idempotency: re-parking an entry that is already parked with the exact
    /// same `reason` replays the original decision (`Replayed` with the
    /// original timestamp) without rewriting the row; re-parking with a
    /// different `reason` is rejected with [`StoreError::OutboxParkConflict`]
    /// (the strict lane: a parked entry's reason is part of its durable
    /// evidence and cannot be rewritten after the fact).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::InvalidParkRequest`] for an empty, oversized
    /// (> 1024 byte), NUL-carrying, or non-UTF-8-bounded reason, or a
    /// negative `now_ms`; [`StoreError::OutboxEntryNotFound`] when no entry
    /// with `sequence` exists; and [`StoreError::OutboxParkConflict`] when
    /// the entry is already parked with a different reason.
    pub fn park_outbox_entry(
        &self,
        sequence: i64,
        reason: &str,
        now_ms: i64,
    ) -> Result<OutboxParkDecision, StoreError> {
        validate_park_request(reason, now_ms)?;
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut statement = transaction.prepare(
            "SELECT parked_at_ms, park_reason FROM operation_outbox WHERE sequence = ?1",
        )?;
        let mut rows = statement.query([sequence])?;
        let existing: Option<(Option<i64>, Option<String>)> = match rows.next()? {
            Some(row) => Some((row.get(0)?, row.get(1)?)),
            None => None,
        };
        drop(rows);
        drop(statement);
        match existing {
            None => return Err(StoreError::OutboxEntryNotFound),
            // Not parked yet: the one-way UPDATE below is legal.
            Some((None, None)) => {}
            // Already parked: the exact same reason replays the original
            // decision; a different reason is the typed one-way conflict;
            // any partial column is a corrupt durable disagreement.
            Some((parked_at_ms, park_reason)) => match (parked_at_ms, park_reason) {
                (Some(at), Some(why)) if why == reason => {
                    transaction.commit()?;
                    return Ok(OutboxParkDecision::Replayed {
                        sequence,
                        parked_at_ms: at,
                    });
                }
                (Some(_), Some(_)) => return Err(StoreError::OutboxParkConflict),
                _ => {
                    return Err(StoreError::CorruptRecord(
                        "outbox park timestamp and reason disagree",
                    ));
                }
            },
        }
        let changed = transaction.execute(
            "UPDATE operation_outbox
             SET parked_at_ms = ?1, park_reason = ?2, park_count = park_count + 1
             WHERE sequence = ?3 AND parked_at_ms IS NULL",
            params![now_ms, reason, sequence],
        )?;
        if changed != 1 {
            return Err(StoreError::CorruptRecord(
                "outbox park compare-and-set failed",
            ));
        }
        transaction.commit()?;
        Ok(OutboxParkDecision::Parked {
            sequence,
            parked_at_ms: now_ms,
        })
    }

    /// Lists parked (dead-letter) outbox entries in durable sequence order,
    /// with their parking timestamps, reasons, and acknowledgement state.
    ///
    /// This is the manual-adjudication read surface of W57-B: parked entries
    /// are explicit operational debt and this listing is the only supported
    /// way to see them (the pending lane skips them by design).
    ///
    /// # Errors
    ///
    /// Returns a storage error or corrupt-record error.
    pub fn inspect_parked_outbox(
        &self,
        limit: usize,
    ) -> Result<Vec<ParkedOutboxEntry>, StoreError> {
        let connection = self.lock_connection()?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut statement = connection.prepare(
            "SELECT sequence, kind, operation_id, operation_generation,
                    owner_fiber_id, owner_fiber_generation, callback_id,
                    state_kind, receipt_id, acknowledged, parked_at_ms, park_reason
             FROM operation_outbox
             WHERE parked_at_ms IS NOT NULL
             ORDER BY sequence
             LIMIT ?1",
        )?;
        let mut rows = statement.query([limit])?;
        let mut entries = Vec::new();
        while let Some(row) = rows.next()? {
            entries.push(decode_parked_outbox_row(row)?);
        }
        Ok(entries)
    }

    /// Unparks one parked outbox entry through the controlled reverse
    /// (W58-1 manual adjudication recovery).
    ///
    /// This is the only legal successor of a parked state. One transaction
    /// writes the parking columns back to `NULL`, stamps the unpark
    /// adjudication (`unparked_at_ms`/`unpark_reason`), and leaves the
    /// acknowledgement untouched — an unpark is never an ack, and an ack is
    /// never an unpark. From the commit on, [`Self::pending_outbox`] serves
    /// the entry again at its durable sequence position, so a recovered
    /// queue head restores in order and consumers redeliver it.
    ///
    /// Idempotency: replaying the unpark of an entry that is already
    /// unparked with the exact same `reason` replays the original decision
    /// (`Replayed` with the original unpark timestamp) without rewriting
    /// the row; a different `reason` fails closed with
    /// [`StoreError::InvalidParkRequest`] (the adjudication is one-way
    /// evidence, exactly like the park reason it answers).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::OutboxEntryNotFound`] when no entry with
    /// `sequence` exists; [`StoreError::InvalidParkRequest`] for an empty,
    /// oversized, or NUL-carrying reason, a negative `now_ms`, an unpark of
    /// an entry that was never parked, or an unpark replay with a different
    /// reason; and [`StoreError::CorruptRecord`] if the durable columns
    /// disagree.
    pub fn unpark_outbox_entry(
        &self,
        sequence: i64,
        reason: &str,
        now_ms: i64,
    ) -> Result<OutboxUnparkDecision, StoreError> {
        validate_unpark_request(reason, now_ms)?;
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut statement = transaction.prepare(
            "SELECT parked_at_ms, park_reason, unparked_at_ms, unpark_reason
             FROM operation_outbox WHERE sequence = ?1",
        )?;
        let mut rows = statement.query([sequence])?;
        let existing: Option<UnparkColumns> = match rows.next()? {
            Some(row) => Some((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            None => None,
        };
        drop(rows);
        drop(statement);
        let Some((parked_at_ms, park_reason, unparked_at_ms, unpark_reason)) = existing else {
            return Err(StoreError::OutboxEntryNotFound);
        };
        match (parked_at_ms, park_reason) {
            // Parked: the controlled reverse below is legal.
            (Some(_), Some(_)) => {}
            (None, None) => match (unparked_at_ms, unpark_reason) {
                // Already unparked: the exact same reason replays the
                // original adjudication; a different reason is the typed
                // one-way refusal; any partial column is a corrupt
                // durable disagreement.
                (Some(at), Some(why)) if why == reason => {
                    transaction.commit()?;
                    return Ok(OutboxUnparkDecision::Replayed {
                        sequence,
                        unparked_at_ms: at,
                    });
                }
                (Some(_), Some(_)) => {
                    return Err(StoreError::InvalidParkRequest(
                        "the entry is already unparked with a different reason; \
                         unpark adjudication is one-way",
                    ));
                }
                (None, None) => {
                    return Err(StoreError::InvalidParkRequest(
                        "the entry was never parked; only a parked entry can be unparked",
                    ));
                }
                _ => {
                    return Err(StoreError::CorruptRecord(
                        "outbox unpark timestamp and reason disagree",
                    ));
                }
            },
            _ => {
                return Err(StoreError::CorruptRecord(
                    "outbox park timestamp and reason disagree",
                ));
            }
        }
        let changed = transaction.execute(
            "UPDATE operation_outbox
             SET parked_at_ms = NULL, park_reason = NULL,
                 unparked_at_ms = ?1, unpark_reason = ?2
             WHERE sequence = ?3 AND parked_at_ms IS NOT NULL",
            params![now_ms, reason, sequence],
        )?;
        if changed != 1 {
            return Err(StoreError::CorruptRecord(
                "outbox unpark compare-and-set failed",
            ));
        }
        transaction.commit()?;
        Ok(OutboxUnparkDecision::Unparked {
            sequence,
            unparked_at_ms: now_ms,
        })
    }

    /// Lists every outbox entry that has been parked at least once, in
    /// durable sequence order — currently parked (dead-letter) and already
    /// unparked (recovered) rows alike.
    ///
    /// This is the W58-1 audit surface of the parking family: it answers
    /// "which entries were ever parked, which are still parked, which were
    /// recovered by adjudication, and how often" without perturbing either
    /// lane ([`Self::pending_outbox`] keeps serving only unparked rows and
    /// [`Self::inspect_parked_outbox`] keeps listing only currently parked
    /// ones).
    ///
    /// # Errors
    ///
    /// Returns a storage error or corrupt-record error.
    pub fn inspect_outbox_park_history(
        &self,
        limit: usize,
    ) -> Result<Vec<OutboxParkHistoryEntry>, StoreError> {
        let connection = self.lock_connection()?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut statement = connection.prepare(
            "SELECT sequence, kind, operation_id, operation_generation,
                    owner_fiber_id, owner_fiber_generation, callback_id,
                    state_kind, receipt_id, acknowledged,
                    parked_at_ms, park_reason, unparked_at_ms, unpark_reason, park_count
             FROM operation_outbox
             WHERE parked_at_ms IS NOT NULL OR unparked_at_ms IS NOT NULL
             ORDER BY sequence
             LIMIT ?1",
        )?;
        let mut rows = statement.query([limit])?;
        let mut entries = Vec::new();
        while let Some(row) = rows.next()? {
            entries.push(decode_outbox_park_history_row(row)?);
        }
        Ok(entries)
    }

    /// Reads one entry's parking history by durable sequence (the targeted
    /// form of [`Self::inspect_outbox_park_history`]); `None` when no entry
    /// with `sequence` exists. Consumers use this to observe whether a
    /// delivered entry is a post-unpark recovery.
    ///
    /// # Errors
    ///
    /// Returns a storage error or corrupt-record error.
    pub fn inspect_outbox_park_history_entry(
        &self,
        sequence: i64,
    ) -> Result<Option<OutboxParkHistoryEntry>, StoreError> {
        let connection = self.lock_connection()?;
        let mut statement = connection.prepare(
            "SELECT sequence, kind, operation_id, operation_generation,
                    owner_fiber_id, owner_fiber_generation, callback_id,
                    state_kind, receipt_id, acknowledged,
                    parked_at_ms, park_reason, unparked_at_ms, unpark_reason, park_count
             FROM operation_outbox
             WHERE sequence = ?1",
        )?;
        let mut rows = statement.query([sequence])?;
        match rows.next()? {
            Some(row) => Ok(Some(decode_outbox_park_history_row(row)?)),
            None => Ok(None),
        }
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

/// Adds the operation-scoped Outbox recovery index. The migration is a
/// single transaction, so interrupted upgrades leave either a complete v1
/// database or a complete v2 database, never a mixed schema.
fn migrate_v2(connection: &mut Connection) -> Result<(), StoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE INDEX operation_outbox_by_operation
            ON operation_outbox(operation_id, operation_generation, sequence);
         PRAGMA user_version = 2;",
    )?;
    transaction.commit()?;
    Ok(())
}

/// Adds durable SABI/KABI same-key deduplication and immutable result replay.
fn migrate_v3(connection: &mut Connection) -> Result<(), StoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE idempotent_calls (
            application_id BLOB NOT NULL CHECK(length(application_id) = 16),
            service TEXT NOT NULL
                CHECK(length(service) BETWEEN 1 AND 128 AND instr(service, char(0)) = 0),
            method TEXT NOT NULL
                CHECK(length(method) BETWEEN 1 AND 128 AND instr(method, char(0)) = 0),
            idempotency_key BLOB NOT NULL CHECK(length(idempotency_key) = 16),
            request_digest_sha256 BLOB NOT NULL CHECK(length(request_digest_sha256) = 32),
            operation_id BLOB NOT NULL CHECK(length(operation_id) = 16),
            operation_generation BLOB NOT NULL CHECK(length(operation_generation) = 8),
            receipt_id BLOB CHECK(receipt_id IS NULL OR length(receipt_id) = 16),
            -- Despite the historical internal column name, this stores only
            -- transport-independent stable service-result bytes.
            response_wire BLOB,
            PRIMARY KEY(application_id, service, method, idempotency_key),
            UNIQUE(operation_id, operation_generation),
            FOREIGN KEY(operation_id) REFERENCES operations(operation_id),
            CHECK((receipt_id IS NULL) = (response_wire IS NULL)),
            CHECK(response_wire IS NULL OR length(response_wire) <= 1048576)
        ) STRICT;

        CREATE TRIGGER idempotent_result_is_immutable
        BEFORE UPDATE OF receipt_id, response_wire ON idempotent_calls
        WHEN OLD.receipt_id IS NOT NULL AND
             (NEW.receipt_id IS NOT OLD.receipt_id OR
              NEW.response_wire IS NOT OLD.response_wire)
        BEGIN
            SELECT RAISE(ABORT, 'idempotent result is immutable');
        END;

        PRAGMA user_version = 3;",
    )?;
    transaction.commit()?;
    Ok(())
}

/// Adds immutable owner-bound Operation dispatch preparation and activation
/// receipts. Both tables are append-only; activation is a separate row so a
/// prepared Operation remains visibly unactivated until the activation
/// transaction commits.
fn migrate_v4(connection: &mut Connection) -> Result<(), StoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "CREATE TABLE operation_dispatch_preparations (
            operation_id BLOB PRIMARY KEY NOT NULL CHECK(length(operation_id) = 16),
            operation_generation BLOB NOT NULL CHECK(length(operation_generation) = 8),
            owner_fiber_id BLOB NOT NULL CHECK(length(owner_fiber_id) = 16),
            owner_fiber_generation BLOB NOT NULL CHECK(length(owner_fiber_generation) = 8),
            cancellation_scope_id BLOB NOT NULL CHECK(length(cancellation_scope_id) = 16),
            cancellation_generation BLOB NOT NULL CHECK(length(cancellation_generation) = 8),
            callback_id BLOB NOT NULL CHECK(length(callback_id) = 16),
            cancel_epoch BLOB NOT NULL CHECK(length(cancel_epoch) = 8),
            preparation_receipt_id BLOB UNIQUE NOT NULL CHECK(length(preparation_receipt_id) = 16),
            FOREIGN KEY(operation_id) REFERENCES operations(operation_id)
        ) STRICT;

        CREATE TABLE operation_dispatch_activation_receipts (
            activation_receipt_id BLOB PRIMARY KEY NOT NULL CHECK(length(activation_receipt_id) = 16),
            operation_id BLOB UNIQUE NOT NULL CHECK(length(operation_id) = 16),
            operation_generation BLOB NOT NULL CHECK(length(operation_generation) = 8),
            preparation_receipt_id BLOB UNIQUE NOT NULL CHECK(length(preparation_receipt_id) = 16),
            callback_id BLOB NOT NULL CHECK(length(callback_id) = 16),
            cancel_epoch BLOB NOT NULL CHECK(length(cancel_epoch) = 8),
            FOREIGN KEY(operation_id) REFERENCES operations(operation_id),
            FOREIGN KEY(preparation_receipt_id)
                REFERENCES operation_dispatch_preparations(preparation_receipt_id)
        ) STRICT;

        CREATE TRIGGER operation_dispatch_preparations_immutable_update
        BEFORE UPDATE ON operation_dispatch_preparations
        BEGIN
            SELECT RAISE(ABORT, 'operation dispatch preparation is immutable');
        END;

        CREATE TRIGGER operation_dispatch_preparations_immutable_delete
        BEFORE DELETE ON operation_dispatch_preparations
        BEGIN
            SELECT RAISE(ABORT, 'operation dispatch preparation is immutable');
        END;

        CREATE TRIGGER operation_dispatch_activation_receipts_immutable_update
        BEFORE UPDATE ON operation_dispatch_activation_receipts
        BEGIN
            SELECT RAISE(ABORT, 'operation dispatch activation receipt is immutable');
        END;

        CREATE TRIGGER operation_dispatch_activation_receipts_immutable_delete
        BEFORE DELETE ON operation_dispatch_activation_receipts
        BEGIN
            SELECT RAISE(ABORT, 'operation dispatch activation receipt is immutable');
        END;

        PRAGMA user_version = 4;",
    )?;
    transaction.commit()?;
    Ok(())
}

/// Adds one-way dead-letter parking to the Outbox (W57-B, schema v5).
///
/// Two nullable columns (`parked_at_ms`, `park_reason`) are added to
/// `operation_outbox`: `NULL` means "not parked". A trigger makes parking
/// strictly one-way at the storage layer — the only UPDATE of these columns
/// SQLite accepts is the single NULL→value stamp of
/// [`SqliteOperationStore::park_outbox_entry`]; a later re-park or an un-park
/// (value→NULL or value→different value) aborts, so parking is durable
/// evidence that cannot be rewritten after the fact. The `acknowledged`
/// semantics are deliberately untouched: acknowledging a row does not mention
/// the parked columns, and parking never acknowledges. A fresh partial index
/// keeps [`SqliteOperationStore::pending_outbox`] an index-order scan now
/// that it must also skip parked rows. Like every migration here this is one
/// transaction, so an interrupted upgrade leaves a complete v4 or a complete
/// v5 database, never a mixed schema, and no row data is rewritten.
fn migrate_v5(connection: &mut Connection) -> Result<(), StoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "ALTER TABLE operation_outbox
            ADD COLUMN parked_at_ms INTEGER
            CHECK(parked_at_ms IS NULL OR parked_at_ms >= 0);
         ALTER TABLE operation_outbox
            ADD COLUMN park_reason TEXT
            CHECK(park_reason IS NULL OR (
                length(park_reason) BETWEEN 1 AND 1024
                AND instr(park_reason, char(0)) = 0));

         CREATE TRIGGER operation_outbox_parking_is_one_way
         BEFORE UPDATE OF parked_at_ms, park_reason ON operation_outbox
         WHEN OLD.parked_at_ms IS NOT NULL
              OR (NEW.parked_at_ms IS NULL) <> (NEW.park_reason IS NULL)
         BEGIN
             SELECT RAISE(ABORT, 'operation_outbox parking is one-way');
         END;

         CREATE INDEX operation_outbox_pending_unparked
            ON operation_outbox(sequence)
            WHERE acknowledged = 0 AND parked_at_ms IS NULL;

         PRAGMA user_version = 5;",
    )?;
    transaction.commit()?;
    Ok(())
}

/// Adds the controlled unpark reverse to the Outbox parking family (W58-1,
/// schema v6).
///
/// Design choice (argued against the alternatives): the reverse keeps the
/// v5 column pair `parked_at_ms`/`park_reason` as the *only* parked-state
/// signal and writes both back to `NULL` on unpark, instead of introducing
/// a separate "unparked" state column. Every v5 read path therefore keeps
/// its exact meaning for an unparked row — `NULL` *is* "not parked", so
/// the pending partial index, [`SqliteOperationStore::pending_outbox`],
/// and the park API's fresh-park pre-read all serve recovered rows again
/// without a single query change, and a recovered head restores in durable
/// sequence order for free. What an unpark must additionally prove lives
/// in three new columns: `unparked_at_ms`/`unpark_reason` (the durable
/// adjudication evidence, also the idempotency anchor for replays) and
/// `park_count` (how many times the entry was ever parked — the "曾停泊
/// 次数" health fact). A full per-cycle evidence ledger table would carry
/// more history but is a strictly larger schema change than this lane
/// requires.
///
/// The v5 one-way trigger is replaced by a v6 trigger that keeps every v5
/// abort lane (a parked row's evidence is never rewritten in place, the
/// two parking columns never disagree) and opens exactly one new legal
/// transition: the *controlled* reverse, an UPDATE that NULLs both parking
/// columns while stamping both unpark columns in the same statement. A
/// bare value→NULL UPDATE still aborts — the reverse is a shaped lane, not
/// a permission for arbitrary rewrites. A second trigger guards the unpark
/// evidence: it may only be stamped while the row is parked (inside an
/// unpark), never cleared, and `park_count` may only advance inside a
/// fresh park. `acknowledged` stays outside both triggers exactly as in
/// v5: acking never mentions the parking columns, parking/unparking never
/// acknowledge.
///
/// The count is backfilled for rows already parked under v5 — v5 could
/// neither un-park nor re-park, so a v5-parked row was parked exactly
/// once, and the backfill derives that fact from the parked columns
/// without rewriting any other durable fact. Like every migration here
/// this is one transaction: an interrupted upgrade leaves a complete v5
/// or a complete v6 database, never a mixed schema.
fn migrate_v6(connection: &mut Connection) -> Result<(), StoreError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(
        "ALTER TABLE operation_outbox
            ADD COLUMN unparked_at_ms INTEGER
            CHECK(unparked_at_ms IS NULL OR unparked_at_ms >= 0);
         ALTER TABLE operation_outbox
            ADD COLUMN unpark_reason TEXT
            CHECK(unpark_reason IS NULL OR (
                length(unpark_reason) BETWEEN 1 AND 1024
                AND instr(unpark_reason, char(0)) = 0));
         ALTER TABLE operation_outbox
            ADD COLUMN park_count INTEGER NOT NULL DEFAULT 0
            CHECK(park_count >= 0);

         -- v5-parked rows were parked exactly once (no unpark existed):
         -- derive their count before the v6 triggers take over. This
         -- UPDATE mentions none of the v5 trigger's columns, so the v5
         -- one-way trigger stays silent.
         UPDATE operation_outbox SET park_count = 1
          WHERE parked_at_ms IS NOT NULL AND park_count = 0;

         DROP TRIGGER operation_outbox_parking_is_one_way;

         CREATE TRIGGER operation_outbox_parking_is_one_way
         BEFORE UPDATE OF parked_at_ms, park_reason ON operation_outbox
         WHEN (NEW.parked_at_ms IS NULL) <> (NEW.park_reason IS NULL)
              OR (OLD.parked_at_ms IS NULL AND NEW.parked_at_ms IS NOT NULL
                  AND NEW.park_count <> OLD.park_count + 1)
              OR (OLD.parked_at_ms IS NOT NULL AND NOT (
                  NEW.parked_at_ms IS NULL AND NEW.park_reason IS NULL
                  AND NEW.unparked_at_ms IS NOT NULL
                  AND NEW.unpark_reason IS NOT NULL))
         BEGIN
             SELECT RAISE(ABORT, 'operation_outbox parking is one-way');
         END;

         CREATE TRIGGER operation_outbox_unpark_evidence_is_one_way
         BEFORE UPDATE OF unparked_at_ms, unpark_reason, park_count ON operation_outbox
         WHEN (NEW.unparked_at_ms IS NULL) <> (NEW.unpark_reason IS NULL)
              OR (OLD.parked_at_ms IS NULL
                  AND (NEW.unparked_at_ms IS NOT OLD.unparked_at_ms
                       OR NEW.unpark_reason IS NOT OLD.unpark_reason))
              OR (NEW.park_count <> OLD.park_count
                  AND NOT (OLD.parked_at_ms IS NULL
                           AND NEW.parked_at_ms IS NOT NULL))
         BEGIN
             SELECT RAISE(ABORT, 'operation_outbox unpark evidence is one-way');
         END;

         PRAGMA user_version = 6;",
    )?;
    transaction.commit()?;
    Ok(())
}

struct IdempotencyRecord {
    request_digest_sha256: [u8; 32],
    operation: OperationHandle,
    result: Option<DurableCallResult>,
}

impl IdempotencyRecord {
    fn into_decision(self) -> IdempotencyDecision {
        match self.result {
            Some(result) => IdempotencyDecision::Completed(result),
            None => IdempotencyDecision::PendingOrUncertain(self.operation),
        }
    }
}

fn validate_idempotency_scope(scope: &IdempotencyScope) -> Result<(), StoreError> {
    let valid = |component: &str| {
        !component.is_empty()
            && component.len() <= MAX_ENDPOINT_COMPONENT_BYTES
            && !component.contains('\0')
    };
    if valid(&scope.service) && valid(&scope.method) {
        Ok(())
    } else {
        Err(StoreError::InvalidIdempotencyScope)
    }
}

/// Fail-closed validation of a parking request before any row is touched:
/// the reason becomes durable operational evidence, so it must be a
/// non-empty, bounded, NUL-free UTF-8 string, and the timestamp must fit the
/// column's non-negative domain.
fn validate_park_request(reason: &str, now_ms: i64) -> Result<(), StoreError> {
    if reason.is_empty() {
        return Err(StoreError::InvalidParkRequest(
            "park reason must be a non-empty string",
        ));
    }
    if reason.len() > MAX_PARK_REASON_BYTES {
        return Err(StoreError::InvalidParkRequest(
            "park reason exceeds the 1024-byte bound",
        ));
    }
    if reason.contains('\0') {
        return Err(StoreError::InvalidParkRequest(
            "park reason must not carry a NUL byte",
        ));
    }
    if now_ms < 0 {
        return Err(StoreError::InvalidParkRequest(
            "park timestamp must be non-negative milliseconds",
        ));
    }
    Ok(())
}

/// The four parking-family columns of one outbox row exactly as the unpark
/// lane reads them: `(parked_at_ms, park_reason, unparked_at_ms,
/// unpark_reason)`, each `NULL` meaning "absent".
type UnparkColumns = (Option<i64>, Option<String>, Option<i64>, Option<String>);

/// Fail-closed validation of an unpark request before any row is touched:
/// the adjudication reason becomes durable evidence exactly like a park
/// reason, so it obeys the same non-empty, bounded, NUL-free UTF-8 shape
/// and the timestamp the same non-negative domain.
fn validate_unpark_request(reason: &str, now_ms: i64) -> Result<(), StoreError> {
    if reason.is_empty() {
        return Err(StoreError::InvalidParkRequest(
            "unpark reason must be a non-empty string",
        ));
    }
    if reason.len() > MAX_PARK_REASON_BYTES {
        return Err(StoreError::InvalidParkRequest(
            "unpark reason exceeds the 1024-byte bound",
        ));
    }
    if reason.contains('\0') {
        return Err(StoreError::InvalidParkRequest(
            "unpark reason must not carry a NUL byte",
        ));
    }
    if now_ms < 0 {
        return Err(StoreError::InvalidParkRequest(
            "unpark timestamp must be non-negative milliseconds",
        ));
    }
    Ok(())
}

fn load_idempotency_record(
    source: &impl SqlRead,
    scope: &IdempotencyScope,
    idempotency_key: IdempotencyKey,
) -> Result<Option<IdempotencyRecord>, StoreError> {
    let mut statement = source.prepare_statement(
        "SELECT request_digest_sha256, operation_id, operation_generation,
                receipt_id, response_wire
         FROM idempotent_calls
         WHERE application_id = ?1 AND service = ?2 AND method = ?3
           AND idempotency_key = ?4",
    )?;
    let mut rows = statement.query(params![
        scope.application_id.as_bytes().as_slice(),
        scope.service,
        scope.method,
        idempotency_key.as_bytes().as_slice(),
    ])?;
    rows.next()?.map(decode_idempotency_row).transpose()
}

fn load_idempotency_record_by_operation(
    source: &impl SqlRead,
    operation: OperationHandle,
) -> Result<Option<IdempotencyRecord>, StoreError> {
    let mut statement = source.prepare_statement(
        "SELECT request_digest_sha256, operation_id, operation_generation,
                receipt_id, response_wire
         FROM idempotent_calls
         WHERE operation_id = ?1 AND operation_generation = ?2",
    )?;
    let mut rows = statement.query(params![
        operation.operation_id.as_bytes().as_slice(),
        encode_u64(operation.generation.get()).as_slice(),
    ])?;
    rows.next()?.map(decode_idempotency_row).transpose()
}

fn decode_idempotency_row(row: &rusqlite::Row<'_>) -> Result<IdempotencyRecord, StoreError> {
    let request_digest_sha256 = blob32(row, 0)?;
    let operation = OperationHandle {
        operation_id: OperationId::from_bytes(blob16(row, 1)?),
        generation: generation_from_blob(row, 2)?,
    };
    let receipt_id = optional_blob16(row, 3)?.map(ReceiptId::from_bytes);
    let result_wire: Option<Vec<u8>> = row.get(4)?;
    let result = match (receipt_id, result_wire) {
        (Some(receipt_id), Some(result_wire)) => {
            if result_wire.len() > MAX_DURABLE_RESULT_BYTES {
                return Err(StoreError::CorruptRecord(
                    "idempotency result exceeds durable bound",
                ));
            }
            Some(DurableCallResult {
                operation,
                receipt_id,
                result_wire,
            })
        }
        (None, None) => None,
        _ => {
            return Err(StoreError::CorruptRecord(
                "idempotency receipt and result disagree",
            ));
        }
    };
    Ok(IdempotencyRecord {
        request_digest_sha256,
        operation,
        result,
    })
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

fn dispatch_preparation_from_machine(
    machine: &OperationMachine,
    callback_id: CallbackId,
) -> OperationDispatchPreparation {
    let spec = machine.spec();
    let snapshot = machine.snapshot();
    OperationDispatchPreparation {
        operation: snapshot.handle,
        owner_fiber: spec.owner_fiber,
        cancellation_scope_id: spec.cancellation_scope_id,
        cancellation_generation: spec.cancellation_generation,
        callback_id,
        cancel_epoch: snapshot.cancel_epoch,
        preparation_receipt_id: ReceiptId::from_bytes(derive_operation_dispatch_receipt_id(
            b"nlos/operation-dispatch/preparation/v1",
            snapshot.handle,
            callback_id,
        )),
    }
}

fn dispatch_preparation_matches_machine(
    preparation: &OperationDispatchPreparation,
    machine: &OperationMachine,
    callback_id: CallbackId,
) -> bool {
    let spec = machine.spec();
    let handle = machine.snapshot().handle;
    preparation.operation == handle
        && preparation.owner_fiber == spec.owner_fiber
        && preparation.cancellation_scope_id == spec.cancellation_scope_id
        && preparation.cancellation_generation == spec.cancellation_generation
        && preparation.callback_id == callback_id
        && preparation.cancel_epoch == CancelEpoch::INITIAL
        && preparation.preparation_receipt_id
            == ReceiptId::from_bytes(derive_operation_dispatch_receipt_id(
                b"nlos/operation-dispatch/preparation/v1",
                handle,
                callback_id,
            ))
}

fn insert_dispatch_preparation(
    transaction: &Transaction<'_>,
    preparation: &OperationDispatchPreparation,
) -> Result<(), StoreError> {
    transaction.execute(
        "INSERT INTO operation_dispatch_preparations (
            operation_id, operation_generation, owner_fiber_id,
            owner_fiber_generation, cancellation_scope_id,
            cancellation_generation, callback_id, cancel_epoch,
            preparation_receipt_id
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            preparation.operation.operation_id.as_bytes().as_slice(),
            encode_u64(preparation.operation.generation.get()).as_slice(),
            preparation.owner_fiber.fiber_id.as_bytes().as_slice(),
            encode_u64(preparation.owner_fiber.generation.get()).as_slice(),
            preparation.cancellation_scope_id.as_bytes().as_slice(),
            encode_u64(preparation.cancellation_generation.get()).as_slice(),
            preparation.callback_id.as_bytes().as_slice(),
            encode_u64(preparation.cancel_epoch.get()).as_slice(),
            preparation.preparation_receipt_id.as_bytes().as_slice(),
        ],
    )?;
    Ok(())
}

fn load_dispatch_preparation_optional(
    source: &impl SqlRead,
    operation: OperationHandle,
) -> Result<Option<OperationDispatchPreparation>, StoreError> {
    let mut statement = source.prepare_statement(
        "SELECT operation_id, operation_generation, owner_fiber_id,
                owner_fiber_generation, cancellation_scope_id,
                cancellation_generation, callback_id, cancel_epoch,
                preparation_receipt_id
         FROM operation_dispatch_preparations
         WHERE operation_id = ?1",
    )?;
    let mut rows = statement.query([operation.operation_id.as_bytes().as_slice()])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    Ok(Some(OperationDispatchPreparation {
        operation: OperationHandle {
            operation_id: OperationId::from_bytes(blob16(row, 0)?),
            generation: generation_from_blob(row, 1)?,
        },
        owner_fiber: FiberHandle {
            fiber_id: ExecutionFiberId::from_bytes(blob16(row, 2)?),
            generation: generation_from_blob(row, 3)?,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes(blob16(row, 4)?),
        cancellation_generation: generation_from_blob(row, 5)?,
        callback_id: CallbackId::from_bytes(blob16(row, 6)?),
        cancel_epoch: CancelEpoch::new(u64_from_blob(row, 7)?),
        preparation_receipt_id: ReceiptId::from_bytes(blob16(row, 8)?),
    }))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DispatchActivationRecord {
    activation_receipt_id: ReceiptId,
    operation: OperationHandle,
    preparation_receipt_id: ReceiptId,
    callback_id: CallbackId,
    cancel_epoch: CancelEpoch,
}

fn insert_dispatch_activation(
    transaction: &Transaction<'_>,
    preparation: OperationDispatchPreparation,
    activation_receipt_id: ReceiptId,
) -> Result<(), StoreError> {
    transaction.execute(
        "INSERT INTO operation_dispatch_activation_receipts (
            activation_receipt_id, operation_id, operation_generation,
            preparation_receipt_id, callback_id, cancel_epoch
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            activation_receipt_id.as_bytes().as_slice(),
            preparation.operation.operation_id.as_bytes().as_slice(),
            encode_u64(preparation.operation.generation.get()).as_slice(),
            preparation.preparation_receipt_id.as_bytes().as_slice(),
            preparation.callback_id.as_bytes().as_slice(),
            encode_u64(preparation.cancel_epoch.get()).as_slice(),
        ],
    )?;
    Ok(())
}

fn load_dispatch_activation_optional(
    source: &impl SqlRead,
    operation: OperationHandle,
) -> Result<Option<DispatchActivationRecord>, StoreError> {
    let mut statement = source.prepare_statement(
        "SELECT activation_receipt_id, operation_id, operation_generation,
                preparation_receipt_id, callback_id, cancel_epoch
         FROM operation_dispatch_activation_receipts
         WHERE operation_id = ?1",
    )?;
    let mut rows = statement.query([operation.operation_id.as_bytes().as_slice()])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    Ok(Some(DispatchActivationRecord {
        activation_receipt_id: ReceiptId::from_bytes(blob16(row, 0)?),
        operation: OperationHandle {
            operation_id: OperationId::from_bytes(blob16(row, 1)?),
            generation: generation_from_blob(row, 2)?,
        },
        preparation_receipt_id: ReceiptId::from_bytes(blob16(row, 3)?),
        callback_id: CallbackId::from_bytes(blob16(row, 4)?),
        cancel_epoch: CancelEpoch::new(u64_from_blob(row, 5)?),
    }))
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

/// Decodes one parked row of [`SqliteOperationStore::inspect_parked_outbox`]:
/// the outbox fact columns plus the acknowledged flag and the two parking
/// columns (both of which must be present and mutually consistent — the
/// v5 trigger makes any other durable combination unreachable).
fn decode_parked_outbox_row(row: &rusqlite::Row<'_>) -> Result<ParkedOutboxEntry, StoreError> {
    let entry = decode_outbox_row(row)?;
    let acknowledged: i64 = row.get(9)?;
    let parked_at_ms: i64 = row.get(10)?;
    let park_reason: String = row.get(11)?;
    if !matches!(acknowledged, 0 | 1) {
        return Err(StoreError::CorruptRecord("outbox acknowledged flag"));
    }
    if parked_at_ms < 0 {
        return Err(StoreError::CorruptRecord("negative outbox park timestamp"));
    }
    Ok(ParkedOutboxEntry {
        sequence: entry.sequence,
        kind: entry.kind,
        operation: entry.operation,
        owner_fiber: entry.owner_fiber,
        callback_id: entry.callback_id,
        state: entry.state,
        acknowledged: acknowledged == 1,
        parked_at_ms,
        park_reason,
    })
}

/// Decodes one row of [`SqliteOperationStore::inspect_outbox_park_history`]:
/// the outbox fact columns plus the acknowledged flag, the two parking
/// columns, the two unpark columns, and the park count. Both column pairs
/// must agree in NULL-ness (the v6 triggers make any other durable
/// combination unreachable), the counts must be non-negative, and the
/// derived `unpark_count` is exactly the park cycles the adjudication lane
/// already closed.
fn decode_outbox_park_history_row(
    row: &rusqlite::Row<'_>,
) -> Result<OutboxParkHistoryEntry, StoreError> {
    let entry = decode_outbox_row(row)?;
    let acknowledged: i64 = row.get(9)?;
    let parked_at_ms: Option<i64> = row.get(10)?;
    let park_reason: Option<String> = row.get(11)?;
    let unparked_at_ms: Option<i64> = row.get(12)?;
    let unpark_reason: Option<String> = row.get(13)?;
    let park_count: i64 = row.get(14)?;
    if !matches!(acknowledged, 0 | 1) {
        return Err(StoreError::CorruptRecord("outbox acknowledged flag"));
    }
    if (parked_at_ms.is_some() != park_reason.is_some())
        || (unparked_at_ms.is_some() != unpark_reason.is_some())
    {
        return Err(StoreError::CorruptRecord(
            "outbox park or unpark timestamp and reason disagree",
        ));
    }
    if let (Some(at), Some(why)) = (parked_at_ms, park_reason.as_deref())
        && (at < 0 || why.is_empty() || why.len() > MAX_PARK_REASON_BYTES || why.contains('\0'))
    {
        return Err(StoreError::CorruptRecord("outbox park facts out of bound"));
    }
    if let (Some(at), Some(why)) = (unparked_at_ms, unpark_reason.as_deref())
        && (at < 0 || why.is_empty() || why.len() > MAX_PARK_REASON_BYTES || why.contains('\0'))
    {
        return Err(StoreError::CorruptRecord(
            "outbox unpark facts out of bound",
        ));
    }
    if park_count < 0 {
        return Err(StoreError::CorruptRecord("negative outbox park count"));
    }
    let parked = parked_at_ms.is_some();
    // Every unpark consumed a prior park, and only an unpark re-opens the
    // park lane, so the cycles strictly alternate: the closed ones are the
    // parks minus the one currently holding the row (a v5-parked row has
    // no closed cycle; its count of 1 was backfilled).
    let unpark_count = park_count - i64::from(parked);
    if unpark_count < 0 {
        return Err(StoreError::CorruptRecord(
            "outbox unpark count exceeds parks",
        ));
    }
    Ok(OutboxParkHistoryEntry {
        sequence: entry.sequence,
        kind: entry.kind,
        operation: entry.operation,
        owner_fiber: entry.owner_fiber,
        callback_id: entry.callback_id,
        state: entry.state,
        acknowledged: acknowledged == 1,
        parked,
        park_count,
        unpark_count,
        parked_at_ms,
        park_reason,
        unparked_at_ms,
        unpark_reason,
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

fn derive_endpoint_id(
    domain: &[u8],
    operation_id: OperationId,
    generation: Generation,
) -> [u8; 16] {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(operation_id.as_bytes());
    hasher.update(generation.get().to_be_bytes());
    let digest: [u8; 32] = hasher.finalize().into();
    let mut id = [0_u8; 16];
    id.copy_from_slice(&digest[..16]);
    id
}

fn derive_operation_dispatch_receipt_id(
    domain: &[u8],
    operation: OperationHandle,
    callback_id: CallbackId,
) -> [u8; 16] {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(operation.operation_id.as_bytes());
    hasher.update(operation.generation.get().to_be_bytes());
    hasher.update(callback_id.as_bytes());
    let digest: [u8; 32] = hasher.finalize().into();
    let mut id = [0_u8; 16];
    id.copy_from_slice(&digest[..16]);
    id
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

fn blob32(row: &rusqlite::Row<'_>, index: usize) -> Result<[u8; 32], StoreError> {
    let value: Vec<u8> = row.get(index)?;
    value
        .try_into()
        .map_err(|_| StoreError::CorruptRecord("expected 32-byte blob"))
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
