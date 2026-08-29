//! Durable local wait registry authority.
//!
//! This Stage-B slice owns the durable wake side of the Channel endpoint
//! authority: callers register a wait ("wake me when channel `C` commits at
//! least sequence `T`") and the producer explicitly notifies commits after
//! enqueue.  [`WaitAuthority::register_wait`] verifies the Channel through
//! the owner readback [`ChannelAuthority::inspect_channel`], binds the
//! registration to the channel's current generation/fence snapshot and
//! stores a `PENDING` state-machine row keyed by the authority-derived
//! [`WaitId`].  [`WaitAuthority::notify_commits`] runs in a single
//! `Immediate` transaction: it flips every still-`PENDING` wait of that
//! channel with `target_sequence <= up_to_sequence` to `WOKEN` and returns
//! the exact [`WakeReport`], durably recorded so the same idempotency key
//! replays the original report without re-flipping.  [`WaitAuthority::
//! cancel_wait`] performs the complementary `PENDING -> CANCELLED` flip.
//! Restart replay is exact: every wait row, notify receipt and cancellation
//! receipt is durable and re-validates field-for-field after reopen.
//!
//! Wakeup is therefore explicit-notify plus restart replay; there is no
//! polling loop.  The slice deliberately does not implement: any runtime or
//! tokio wiring (fibre wakes are a registered follow-up), per-channel notify
//! watermarks (each notify independently evaluates `target_sequence <=
//! up_to_sequence`; already-`WOKEN` rows are naturally untouched, so
//! repeated, reordered or regressed notifies stay safe), interpretation of
//! the opaque [`BindingId`] (this prefix never inspects the waiter's
//! internal structure), retention of terminal rows, or `TaskWriteSet`
//! integration.

mod schema;

use std::error::Error;
use std::fmt;
use std::num::NonZeroU64;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use nlos_channel::{ChannelAuthority, ChannelAuthorityError, FencingToken};
use nlos_types::{ChannelId, Generation, IdempotencyKey};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};

macro_rules! nominal_id {
    ($name:ident) => {
        #[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
        pub struct $name([u8; 16]);

        impl $name {
            #[must_use]
            pub const fn from_bytes(bytes: [u8; 16]) -> Self {
                Self(bytes)
            }

            #[must_use]
            pub const fn into_bytes(self) -> [u8; 16] {
                self.0
            }

            #[must_use]
            pub const fn as_bytes(&self) -> &[u8; 16] {
                &self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(stringify!($name))?;
                formatter.write_str("(")?;
                for byte in self.0 {
                    write!(formatter, "{byte:02x}")?;
                }
                formatter.write_str(")")
            }
        }
    };
}

// The opaque identity of the waiting fibre or process binding: callers
// provide it, this prefix treats it as an opaque 16-byte token and never
// interprets its internal structure.  The all-zero value is not a binding
// ([`WaitAuthorityError::InvalidBinding`]).
nominal_id!(BindingId);

// The authority-derived identity of one registered wait: a domain-separated
// SHA-256 over the binding, the channel id, the target sequence and the
// registration idempotency key, so two registrations of the same
// `(binding, channel, target)` under different keys are distinct waits.
nominal_id!(WaitId);

/// The execution state of one durable wait row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WaitState {
    /// Registered and not yet woken or cancelled; the only state a wait can
    /// leave.
    Pending,
    /// A commit notification covered the wait's target sequence.
    Woken,
    /// Explicitly cancelled before it was woken.
    Cancelled,
}

impl WaitState {
    const fn code(self) -> u64 {
        match self {
            Self::Pending => 0,
            Self::Woken => 1,
            Self::Cancelled => 2,
        }
    }

    fn from_code(code: u64) -> Option<Self> {
        match code {
            0 => Some(Self::Pending),
            1 => Some(Self::Woken),
            2 => Some(Self::Cancelled),
            _ => None,
        }
    }
}

impl fmt::Display for WaitState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Pending => "pending",
            Self::Woken => "woken",
            Self::Cancelled => "cancelled",
        })
    }
}

/// One immutable-as-identity durable wait row.
///
/// `channel_generation`/`channel_fencing_token` are the snapshot the Channel
/// owner read back at registration time; they are historical registration
/// state, not a live fence.  The wake and cancellation timestamps are zero
/// while the corresponding transition has not happened.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WaitRecord {
    pub wait_id: WaitId,
    pub binding: BindingId,
    pub channel_id: ChannelId,
    pub channel_generation: Generation,
    pub channel_fencing_token: FencingToken,
    pub target_sequence: u64,
    pub state: WaitState,
    pub idempotency_key: IdempotencyKey,
    pub registered_at_ms: u64,
    pub woken_at_ms: u64,
    /// The `up_to_sequence` snapshot of the notification that woke the wait.
    pub woken_up_to_sequence: u64,
    pub cancelled_at_ms: u64,
}

/// A wait registration request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegisterWaitRequest {
    /// Opaque waiter binding; must not be the all-zero value.
    pub binding: BindingId,
    pub channel_id: ChannelId,
    /// The channel sequence whose commit should wake the wait; must be >= 1.
    pub target_sequence: u64,
    pub idempotency_key: IdempotencyKey,
    pub registered_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RegisterDecision {
    Registered(WaitRecord),
    Replayed(WaitRecord),
}

impl RegisterDecision {
    #[must_use]
    pub fn record(self) -> WaitRecord {
        match self {
            Self::Registered(record) | Self::Replayed(record) => record,
        }
    }
}

/// An explicit commit notification for one Channel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NotifyCommitsRequest {
    pub channel_id: ChannelId,
    /// Inclusive upper bound: every still-`PENDING` wait of this channel
    /// with `target_sequence <= up_to_sequence` is woken; must be >= 1.
    pub up_to_sequence: u64,
    pub notified_at_ms: u64,
    pub idempotency_key: IdempotencyKey,
}

/// The exact set of waits flipped `PENDING -> WOKEN` by one commit
/// notification (or by its durable replay).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WakeReport {
    pub woken: Vec<WaitRecord>,
}

/// A wait cancellation request addressed by the authority-derived
/// [`WaitId`] (the crate convention: operations reference the
/// authority-derived row identity, as Channel operations reference the
/// `ChannelId`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CancelWaitRequest {
    pub wait_id: WaitId,
    /// Must be >= 1; zero is the durable "not cancelled" sentinel.
    pub cancelled_at_ms: u64,
    pub idempotency_key: IdempotencyKey,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CancelDecision {
    Cancelled(WaitRecord),
    Replayed(WaitRecord),
}

impl CancelDecision {
    #[must_use]
    pub fn record(self) -> WaitRecord {
        match self {
            Self::Cancelled(record) | Self::Replayed(record) => record,
        }
    }
}

#[derive(Debug)]
pub enum WaitAuthorityError {
    Channel(ChannelAuthorityError),
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
    DurabilityUnavailable {
        journal_mode: String,
        synchronous: i64,
    },
    SchemaVersionUnsupported(i64),
    WaitNotFound(WaitId),
    WaitNotPending(WaitState),
    IdempotencyConflict,
    InvalidBinding,
    InvalidSequence(&'static str),
    InvalidTimestamp(&'static str),
    CorruptRecord(&'static str),
    LockPoisoned,
}

impl fmt::Display for WaitAuthorityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Channel(error) => write!(formatter, "channel authority failure: {error}"),
            Self::Sqlite(error) => write!(formatter, "SQLite wait authority failure: {error}"),
            Self::Io(error) => write!(formatter, "wait authority I/O failure: {error}"),
            Self::DurabilityUnavailable {
                journal_mode,
                synchronous,
            } => write!(
                formatter,
                "WAL/FULL durability unavailable: journal_mode={journal_mode}, synchronous={synchronous}"
            ),
            Self::SchemaVersionUnsupported(version) => {
                write!(
                    formatter,
                    "unsupported wait authority schema version {version}"
                )
            }
            Self::WaitNotFound(id) => write!(formatter, "wait {id:?} does not exist"),
            Self::WaitNotPending(state) => {
                write!(formatter, "wait is not pending: current state is {state}")
            }
            Self::IdempotencyConflict => formatter.write_str(
                "idempotency key or authority-assigned identity was rebound to different input",
            ),
            Self::InvalidBinding => {
                formatter.write_str("wait binding must not be the all-zero value")
            }
            Self::InvalidSequence(reason) => write!(formatter, "invalid wait sequence: {reason}"),
            Self::InvalidTimestamp(reason) => {
                write!(formatter, "invalid wait timestamp: {reason}")
            }
            Self::CorruptRecord(reason) => write!(formatter, "corrupt wait record: {reason}"),
            Self::LockPoisoned => formatter.write_str("wait authority writer lock is poisoned"),
        }
    }
}

impl Error for WaitAuthorityError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Channel(error) => Some(error),
            Self::Sqlite(error) => Some(error),
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for WaitAuthorityError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

/// A single-node durable wait registry bound to the Channel owner, with
/// WAL/FULL durable wait state.
pub struct WaitAuthority {
    channel: Arc<ChannelAuthority>,
    connection: Mutex<Connection>,
}

impl WaitAuthority {
    /// Opens or creates `<root>/wait-authority.db` bound to the given
    /// Channel authority.
    ///
    /// # Errors
    ///
    /// Fails closed when `SQLite` cannot provide WAL/FULL durability or when
    /// a stored schema version is unknown.
    pub fn open(
        root: impl AsRef<Path>,
        channel: Arc<ChannelAuthority>,
    ) -> Result<Self, WaitAuthorityError> {
        std::fs::create_dir_all(root.as_ref()).map_err(WaitAuthorityError::Io)?;
        let mut connection = Connection::open(root.as_ref().join("wait-authority.db"))?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;

        let journal_mode: String =
            connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        let synchronous: i64 =
            connection.pragma_query_value(None, "synchronous", |row| row.get(0))?;
        if !journal_mode.eq_ignore_ascii_case("wal") || synchronous != 2 {
            return Err(WaitAuthorityError::DurabilityUnavailable {
                journal_mode,
                synchronous,
            });
        }

        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        match version {
            0 => schema::migrate_v1(&mut connection)?,
            schema::SCHEMA_VERSION => {}
            other => return Err(WaitAuthorityError::SchemaVersionUnsupported(other)),
        }
        Ok(Self {
            channel,
            connection: Mutex::new(connection),
        })
    }

    /// Registers one durable wait on an existing Channel.
    ///
    /// The Channel's existence and its current generation/fence snapshot are
    /// verified through the owner readback [`ChannelAuthority::
    /// inspect_channel`] before any durable write, and the snapshot is bound
    /// into the row as registration-time state.  The [`WaitId`] is
    /// authority-derived (domain-separated SHA-256 over the binding, channel
    /// id, target sequence and idempotency key); it is never a caller field.
    ///
    /// Order of the fail-closed gates, all of which run before any durable
    /// write:
    ///
    /// 1. `target_sequence == 0` is rejected
    ///    ([`WaitAuthorityError::InvalidSequence`]);
    /// 2. the all-zero binding is rejected
    ///    ([`WaitAuthorityError::InvalidBinding`]);
    /// 3. an exact idempotency replay returns the original row
    ///    ([`RegisterDecision::Replayed`]; the generation/fence snapshot is
    ///    authority state and is not compared on replay); a key rebound to a
    ///    different binding, channel or target is an
    ///    [`WaitAuthorityError::IdempotencyConflict`];
    /// 4. an unknown Channel fails closed through the Channel readback.
    ///
    /// A rejected registration leaves zero durable state: its idempotency
    /// key stays free and a later retry registers fresh.
    ///
    /// # Errors
    ///
    /// Fails closed for an invalid target sequence, a zero binding, an
    /// unknown Channel, idempotency rebinding, or a storage/corruption
    /// failure.
    pub fn register_wait(
        &self,
        request: RegisterWaitRequest,
    ) -> Result<RegisterDecision, WaitAuthorityError> {
        if request.target_sequence == 0 {
            return Err(WaitAuthorityError::InvalidSequence(
                "wait target sequence must be non-zero",
            ));
        }
        if is_zero_binding(request.binding) {
            return Err(WaitAuthorityError::InvalidBinding);
        }
        // Owner readback: the Channel and its current fence must be durable
        // before a wait row references them.
        let channel_head = self
            .channel
            .inspect_channel(request.channel_id)
            .map_err(WaitAuthorityError::Channel)?;

        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = load_by_register_key(&transaction, request.idempotency_key)? {
            if existing.binding != request.binding
                || existing.channel_id != request.channel_id
                || existing.target_sequence != request.target_sequence
            {
                return Err(WaitAuthorityError::IdempotencyConflict);
            }
            transaction.commit()?;
            return Ok(RegisterDecision::Replayed(existing));
        }

        let wait_id = wait_id_for(
            request.binding,
            request.channel_id,
            request.target_sequence,
            request.idempotency_key,
        );
        if load_wait_optional(&transaction, wait_id)?.is_some() {
            return Err(WaitAuthorityError::IdempotencyConflict);
        }
        let record = WaitRecord {
            wait_id,
            binding: request.binding,
            channel_id: request.channel_id,
            channel_generation: channel_head.generation,
            channel_fencing_token: channel_head.fencing_token,
            target_sequence: request.target_sequence,
            state: WaitState::Pending,
            idempotency_key: request.idempotency_key,
            registered_at_ms: request.registered_at_ms,
            woken_at_ms: 0,
            woken_up_to_sequence: 0,
            cancelled_at_ms: 0,
        };
        insert_wait(&transaction, &record)?;
        transaction.commit()?;
        Ok(RegisterDecision::Registered(record))
    }

    /// Wakes every still-`PENDING` wait of one Channel whose
    /// `target_sequence <= up_to_sequence`.
    ///
    /// The channel readback verification, the batched `PENDING -> WOKEN` CAS
    /// flip (each flipped row records the notification timestamp and the
    /// `up_to_sequence` wake snapshot) and the durable notify receipt commit
    /// in one `Immediate` transaction; the readback runs before the wait
    /// registry transaction opens.  An empty wake set is a successful empty
    /// [`WakeReport`], never an error.
    ///
    /// Idempotency: the exact key returns the durably recorded original
    /// report without re-flipping (already-`WOKEN` rows are never touched);
    /// the key rebound to a different channel or `up_to_sequence` is an
    /// [`WaitAuthorityError::IdempotencyConflict`].  The presented
    /// `notified_at_ms` is authority state on first execution and is not
    /// compared on replay.
    ///
    /// There is no per-channel notify watermark: every notification
    /// independently evaluates `target_sequence <= up_to_sequence` against
    /// the still-`PENDING` rows.  Because `WOKEN` is terminal, repeating,
    /// reordering or regressing `up_to_sequence` across distinct keys is
    /// safe — a lower or repeated notification simply finds fewer or no
    /// `PENDING` rows and reports exactly what it flipped.  The tradeoff is
    /// that the authority never summarizes "everything up to `X` has been
    /// notified"; callers that need that summary own it.
    ///
    /// # Errors
    ///
    /// Fails closed for a zero `up_to_sequence`, a zero notification
    /// timestamp, an unknown Channel, idempotency rebinding, or a
    /// storage/corruption failure.
    pub fn notify_commits(
        &self,
        request: NotifyCommitsRequest,
    ) -> Result<WakeReport, WaitAuthorityError> {
        if request.up_to_sequence == 0 {
            return Err(WaitAuthorityError::InvalidSequence(
                "notify up_to_sequence must be non-zero",
            ));
        }
        if request.notified_at_ms == 0 {
            return Err(WaitAuthorityError::InvalidTimestamp(
                "notify timestamp must be non-zero",
            ));
        }
        self.channel
            .inspect_channel(request.channel_id)
            .map_err(WaitAuthorityError::Channel)?;

        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = load_notify_by_key(&transaction, request.idempotency_key)? {
            if existing.channel_id != request.channel_id
                || existing.up_to_sequence != request.up_to_sequence
            {
                return Err(WaitAuthorityError::IdempotencyConflict);
            }
            let mut woken = Vec::with_capacity(existing.woken_wait_ids.len());
            for wait_id in &existing.woken_wait_ids {
                woken.push(load_wait_optional(&transaction, *wait_id)?.ok_or(
                    WaitAuthorityError::CorruptRecord(
                        "notify receipt references a missing wait row",
                    ),
                )?);
            }
            sort_woken(&mut woken);
            transaction.commit()?;
            return Ok(WakeReport { woken });
        }

        let mut pending = select_pending(&transaction, request.channel_id, request.up_to_sequence)?;
        if !pending.is_empty() {
            let changed = transaction.execute(
                "UPDATE waits
                 SET status=1, woken_at_ms=?1, woken_up_to_sequence=?2
                 WHERE channel_id=?3 AND status=0 AND target_sequence<=?4",
                params![
                    encode_u64(request.notified_at_ms)?,
                    encode_u64(request.up_to_sequence)?,
                    request.channel_id.as_bytes().as_slice(),
                    encode_u64(request.up_to_sequence)?,
                ],
            )?;
            if changed != pending.len() {
                return Err(WaitAuthorityError::CorruptRecord("wait wake CAS lost"));
            }
        }
        for record in &mut pending {
            record.state = WaitState::Woken;
            record.woken_at_ms = request.notified_at_ms;
            record.woken_up_to_sequence = request.up_to_sequence;
        }
        insert_notify(
            &transaction,
            request.idempotency_key,
            request.channel_id,
            request.up_to_sequence,
            request.notified_at_ms,
            &pending,
        )?;
        transaction.commit()?;
        Ok(WakeReport { woken: pending })
    }

    /// Cancels a still-`PENDING` wait, addressed by its authority-derived
    /// [`WaitId`].
    ///
    /// The `PENDING -> CANCELLED` flip (recording `cancelled_at_ms`) and the
    /// durable cancellation receipt commit in one `Immediate` transaction.
    /// Cancelling an already-terminal wait fails closed with
    /// [`WaitAuthorityError::WaitNotPending`] — a woken wait can never be
    /// retroactively cancelled and a cancelled one cannot be cancelled
    /// twice under a fresh key.  The exact idempotency key replays the
    /// original cancellation (including its stored timestamp); the key
    /// rebound to a different wait is an
    /// [`WaitAuthorityError::IdempotencyConflict`].
    ///
    /// # Errors
    ///
    /// Fails closed for a zero cancellation timestamp, an unknown wait, a
    /// non-`PENDING` wait, idempotency rebinding, or a storage/corruption
    /// failure.
    pub fn cancel_wait(
        &self,
        request: CancelWaitRequest,
    ) -> Result<CancelDecision, WaitAuthorityError> {
        if request.cancelled_at_ms == 0 {
            return Err(WaitAuthorityError::InvalidTimestamp(
                "cancel timestamp must be non-zero",
            ));
        }
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = load_cancellation_by_key(&transaction, request.idempotency_key)? {
            if existing.wait_id != request.wait_id {
                return Err(WaitAuthorityError::IdempotencyConflict);
            }
            let record = load_wait_optional(&transaction, request.wait_id)?.ok_or(
                WaitAuthorityError::CorruptRecord(
                    "cancellation receipt references a missing wait row",
                ),
            )?;
            if record.state != WaitState::Cancelled
                || record.cancelled_at_ms != existing.cancelled_at_ms
            {
                return Err(WaitAuthorityError::CorruptRecord(
                    "cancellation receipt disagrees with wait state",
                ));
            }
            transaction.commit()?;
            return Ok(CancelDecision::Replayed(record));
        }

        let mut record = load_wait_optional(&transaction, request.wait_id)?
            .ok_or(WaitAuthorityError::WaitNotFound(request.wait_id))?;
        if record.state != WaitState::Pending {
            return Err(WaitAuthorityError::WaitNotPending(record.state));
        }
        let changed = transaction.execute(
            "UPDATE waits SET status=2, cancelled_at_ms=?1
             WHERE wait_id=?2 AND status=0",
            params![
                encode_u64(request.cancelled_at_ms)?,
                request.wait_id.as_bytes().as_slice(),
            ],
        )?;
        if changed != 1 {
            return Err(WaitAuthorityError::CorruptRecord("wait cancel CAS lost"));
        }
        record.state = WaitState::Cancelled;
        record.cancelled_at_ms = request.cancelled_at_ms;
        transaction.execute(
            "INSERT INTO wait_cancellations (
                idempotency_key, wait_id, cancelled_at_ms
             ) VALUES (?1, ?2, ?3)",
            params![
                request.idempotency_key.as_bytes().as_slice(),
                request.wait_id.as_bytes().as_slice(),
                encode_u64(request.cancelled_at_ms)?,
            ],
        )?;
        transaction.commit()?;
        Ok(CancelDecision::Cancelled(record))
    }

    /// Reads one wait row after re-deriving its binding digest, checking the
    /// stored state machine against its wake/cancellation fields and
    /// verifying the bound Channel still exists through the owner readback.
    ///
    /// # Errors
    ///
    /// Fails closed for an unknown wait, a tampered or inconsistent row, or
    /// a Channel readback failure.
    pub fn inspect_wait(&self, wait_id: WaitId) -> Result<WaitRecord, WaitAuthorityError> {
        let record = {
            let connection = self.lock()?;
            load_wait_optional(&connection, wait_id)?
                .ok_or(WaitAuthorityError::WaitNotFound(wait_id))?
        };
        match self.channel.inspect_channel(record.channel_id) {
            Ok(_) => Ok(record),
            Err(ChannelAuthorityError::ChannelNotFound(_)) => Err(
                WaitAuthorityError::CorruptRecord("wait references unknown channel"),
            ),
            Err(error) => Err(WaitAuthorityError::Channel(error)),
        }
    }

    /// Reads every wait row bound to one Channel in
    /// `(target_sequence, wait_id)` order, each validated exactly like
    /// [`Self::inspect_wait`].
    ///
    /// # Errors
    ///
    /// Fails closed for an unknown Channel, any tampered or inconsistent
    /// row, or a read failure.
    pub fn inspect_channel_waits(
        &self,
        channel_id: ChannelId,
    ) -> Result<Vec<WaitRecord>, WaitAuthorityError> {
        self.channel
            .inspect_channel(channel_id)
            .map_err(WaitAuthorityError::Channel)?;
        let connection = self.lock()?;
        let mut statement = connection.prepare(&format!(
            "SELECT {WAIT_COLUMNS}
             FROM waits
             WHERE channel_id=?1
             ORDER BY target_sequence, wait_id"
        ))?;
        let rows = statement
            .query_map([channel_id.as_bytes().as_slice()], map_raw_wait)?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter().map(decode_raw_wait).collect()
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>, WaitAuthorityError> {
        self.connection
            .lock()
            .map_err(|_| WaitAuthorityError::LockPoisoned)
    }
}

/// Raw `waits` row without derived structure.
type WaitRow = (
    Vec<u8>,
    Vec<u8>,
    Vec<u8>,
    i64,
    Vec<u8>,
    i64,
    Vec<u8>,
    i64,
    Vec<u8>,
    i64,
    i64,
    i64,
    i64,
);

/// The `rusqlite` row mapper for [`WaitRow`], in [`WAIT_COLUMNS`] order.
fn map_raw_wait(row: &rusqlite::Row<'_>) -> Result<WaitRow, rusqlite::Error> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
        row.get(12)?,
    ))
}

const WAIT_COLUMNS: &str = "wait_id, binding_id, channel_id, channel_generation,
        channel_fencing_token, target_sequence, binding_digest, status,
        register_idempotency_key, registered_at_ms, woken_at_ms,
        woken_up_to_sequence, cancelled_at_ms";

fn wait_id_for(
    binding: BindingId,
    channel_id: ChannelId,
    target_sequence: u64,
    key: IdempotencyKey,
) -> WaitId {
    WaitId::from_bytes(derive_id(
        b"nlos/wait/id/v1",
        &[
            binding.as_bytes(),
            channel_id.as_bytes(),
            &target_sequence.to_be_bytes(),
            key.as_bytes(),
        ],
    ))
}

/// The row integrity digest binding the registration identity to the
/// channel snapshot and target: domain-separated SHA-256, written once at
/// registration and frozen by trigger, re-derived on every read so any
/// drift of the bound fields (channel id, snapshot generation or fence,
/// target sequence) fails closed as [`WaitAuthorityError::CorruptRecord`].
fn binding_digest(record: &WaitRecord) -> [u8; 32] {
    derive_token(
        b"nlos/wait/binding/v1",
        &[
            record.wait_id.as_bytes(),
            record.binding.as_bytes(),
            record.channel_id.as_bytes(),
            &record.channel_generation.get().to_be_bytes(),
            record.channel_fencing_token.as_slice(),
            &record.target_sequence.to_be_bytes(),
            record.idempotency_key.as_bytes(),
            &record.registered_at_ms.to_be_bytes(),
        ],
    )
}

fn is_zero_binding(binding: BindingId) -> bool {
    binding.as_bytes().iter().all(|&byte| byte == 0)
}

fn insert_wait(
    transaction: &Transaction<'_>,
    record: &WaitRecord,
) -> Result<(), WaitAuthorityError> {
    transaction.execute(
        "INSERT INTO waits (
            wait_id, binding_id, channel_id, channel_generation,
            channel_fencing_token, target_sequence, binding_digest, status,
            register_idempotency_key, registered_at_ms,
            woken_at_ms, woken_up_to_sequence, cancelled_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 0, 0, 0)",
        params![
            record.wait_id.as_bytes().as_slice(),
            record.binding.as_bytes().as_slice(),
            record.channel_id.as_bytes().as_slice(),
            encode_generation(record.channel_generation)?,
            record.channel_fencing_token.as_slice(),
            encode_u64(record.target_sequence)?,
            binding_digest(record).as_slice(),
            encode_u64(record.state.code())?,
            record.idempotency_key.as_bytes().as_slice(),
            encode_u64(record.registered_at_ms)?,
        ],
    )?;
    Ok(())
}

fn insert_notify(
    transaction: &Transaction<'_>,
    key: IdempotencyKey,
    channel_id: ChannelId,
    up_to_sequence: u64,
    notified_at_ms: u64,
    woken: &[WaitRecord],
) -> Result<(), WaitAuthorityError> {
    let mut woken_ids = Vec::with_capacity(woken.len() * 16);
    for record in woken {
        woken_ids.extend_from_slice(record.wait_id.as_bytes());
    }
    transaction.execute(
        "INSERT INTO channel_notifies (
            idempotency_key, channel_id, up_to_sequence, notified_at_ms,
            woken_wait_ids
         ) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            key.as_bytes().as_slice(),
            channel_id.as_bytes().as_slice(),
            encode_u64(up_to_sequence)?,
            encode_u64(notified_at_ms)?,
            woken_ids.as_slice(),
        ],
    )?;
    Ok(())
}

/// Decodes one raw row and validates it: the status must decode to a legal
/// state, the state must agree with its wake/cancellation fields, the
/// target sequence must be at least one, the binding must be non-zero and
/// the stored binding digest must match the re-derived one.
fn decode_raw_wait(row: WaitRow) -> Result<WaitRecord, WaitAuthorityError> {
    let (
        wait_id,
        binding,
        channel_id,
        channel_generation,
        channel_fencing_token,
        target_sequence,
        stored_digest,
        status,
        register_key,
        registered_at_ms,
        woken_at_ms,
        woken_up_to_sequence,
        cancelled_at_ms,
    ) = row;
    let state = WaitState::from_code(decode_u64(status)?)
        .ok_or(WaitAuthorityError::CorruptRecord("unknown wait status"))?;
    let record = WaitRecord {
        wait_id: WaitId::from_bytes(array16(wait_id)?),
        binding: BindingId::from_bytes(array16(binding)?),
        channel_id: ChannelId::from_bytes(array16(channel_id)?),
        channel_generation: decode_generation(channel_generation)?,
        channel_fencing_token: array32(channel_fencing_token)?,
        target_sequence: decode_u64(target_sequence)?,
        state,
        idempotency_key: IdempotencyKey::from_bytes(array16(register_key)?),
        registered_at_ms: decode_u64(registered_at_ms)?,
        woken_at_ms: decode_u64(woken_at_ms)?,
        woken_up_to_sequence: decode_u64(woken_up_to_sequence)?,
        cancelled_at_ms: decode_u64(cancelled_at_ms)?,
    };
    validate_wait_state(&record)?;
    if binding_digest(&record) != array32(stored_digest)? {
        return Err(WaitAuthorityError::CorruptRecord(
            "wait binding digest disagrees with its bound identity",
        ));
    }
    Ok(record)
}

/// The stored state machine, cross-checked against the transition fields:
/// only `PENDING` carries zeroed wake and cancellation fields, only `WOKEN`
/// carries a wake timestamp and snapshot, and only `CANCELLED` carries a
/// cancellation timestamp.
fn validate_wait_state(record: &WaitRecord) -> Result<(), WaitAuthorityError> {
    if is_zero_binding(record.binding) {
        return Err(WaitAuthorityError::CorruptRecord("wait binding is zero"));
    }
    if record.target_sequence == 0 {
        return Err(WaitAuthorityError::CorruptRecord(
            "wait target sequence is zero",
        ));
    }
    match record.state {
        WaitState::Pending => {
            if record.woken_at_ms != 0
                || record.woken_up_to_sequence != 0
                || record.cancelled_at_ms != 0
            {
                return Err(WaitAuthorityError::CorruptRecord(
                    "pending wait carries wake or cancel fields",
                ));
            }
        }
        WaitState::Woken => {
            if record.woken_at_ms == 0
                || record.woken_up_to_sequence == 0
                || record.cancelled_at_ms != 0
            {
                return Err(WaitAuthorityError::CorruptRecord(
                    "woken wait has inconsistent wake or cancel fields",
                ));
            }
        }
        WaitState::Cancelled => {
            if record.cancelled_at_ms == 0
                || record.woken_at_ms != 0
                || record.woken_up_to_sequence != 0
            {
                return Err(WaitAuthorityError::CorruptRecord(
                    "cancelled wait has inconsistent wake or cancel fields",
                ));
            }
        }
    }
    Ok(())
}

fn select_pending(
    connection: &Connection,
    channel_id: ChannelId,
    up_to_sequence: u64,
) -> Result<Vec<WaitRecord>, WaitAuthorityError> {
    let mut statement = connection.prepare(&format!(
        "SELECT {WAIT_COLUMNS}
         FROM waits
         WHERE channel_id=?1 AND status=0 AND target_sequence<=?2
         ORDER BY target_sequence, wait_id"
    ))?;
    let rows = statement
        .query_map(
            params![
                channel_id.as_bytes().as_slice(),
                encode_u64(up_to_sequence)?,
            ],
            map_raw_wait,
        )?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter().map(decode_raw_wait).collect()
}

fn load_wait_optional(
    connection: &Connection,
    wait_id: WaitId,
) -> Result<Option<WaitRecord>, WaitAuthorityError> {
    let raw = connection
        .query_row(
            &format!("SELECT {WAIT_COLUMNS} FROM waits WHERE wait_id=?1"),
            [wait_id.as_bytes().as_slice()],
            map_raw_wait,
        )
        .optional()?;
    raw.map(decode_raw_wait).transpose()
}

fn load_by_register_key(
    connection: &Connection,
    key: IdempotencyKey,
) -> Result<Option<WaitRecord>, WaitAuthorityError> {
    let wait_id = connection
        .query_row(
            "SELECT wait_id FROM waits WHERE register_idempotency_key=?1",
            [key.as_bytes().as_slice()],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()?;
    wait_id
        .map(|bytes| load_wait_optional(connection, WaitId::from_bytes(array16(bytes)?)))
        .transpose()
        .map(Option::flatten)
}

struct NotifyReplay {
    channel_id: ChannelId,
    up_to_sequence: u64,
    woken_wait_ids: Vec<WaitId>,
}

fn load_notify_by_key(
    connection: &Connection,
    key: IdempotencyKey,
) -> Result<Option<NotifyReplay>, WaitAuthorityError> {
    let raw = connection
        .query_row(
            "SELECT channel_id, up_to_sequence, woken_wait_ids
             FROM channel_notifies WHERE idempotency_key=?1",
            [key.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            },
        )
        .optional()?;
    raw.map(|(channel_id, up_to_sequence, woken_ids)| {
        if woken_ids.len() % 16 != 0 {
            return Err(WaitAuthorityError::CorruptRecord(
                "notify receipt wait id list is not 16-byte aligned",
            ));
        }
        Ok(NotifyReplay {
            channel_id: ChannelId::from_bytes(array16(channel_id)?),
            up_to_sequence: decode_u64(up_to_sequence)?,
            woken_wait_ids: woken_ids
                .chunks_exact(16)
                .map(|chunk| WaitId::from_bytes(chunk.try_into().expect("chunk is 16 bytes")))
                .collect(),
        })
    })
    .transpose()
}

struct CancellationReplay {
    wait_id: WaitId,
    cancelled_at_ms: u64,
}

fn load_cancellation_by_key(
    connection: &Connection,
    key: IdempotencyKey,
) -> Result<Option<CancellationReplay>, WaitAuthorityError> {
    let raw = connection
        .query_row(
            "SELECT wait_id, cancelled_at_ms
             FROM wait_cancellations WHERE idempotency_key=?1",
            [key.as_bytes().as_slice()],
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?;
    raw.map(|(wait_id, cancelled_at_ms)| {
        Ok(CancellationReplay {
            wait_id: WaitId::from_bytes(array16(wait_id)?),
            cancelled_at_ms: decode_u64(cancelled_at_ms)?,
        })
    })
    .transpose()
}

/// The report order is the durable selection order:
/// `(target_sequence, wait_id)` byte-lexicographic, matching the SQL
/// `ORDER BY` so a replayed report is field-for-field the original.
fn sort_woken(woken: &mut [WaitRecord]) {
    woken.sort_by(|left, right| {
        (&left.target_sequence, left.wait_id).cmp(&(&right.target_sequence, right.wait_id))
    });
}

fn derive_id(tag: &[u8], parts: &[&[u8]]) -> [u8; 16] {
    let digest = derive_token(tag, parts);
    digest[..16].try_into().expect("digest has fixed length")
}

fn derive_token(tag: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update((tag.len() as u64).to_be_bytes());
    hasher.update(tag);
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    hasher.finalize().into()
}

fn encode_generation(generation: Generation) -> Result<i64, WaitAuthorityError> {
    encode_u64(generation.get())
}

fn encode_u64(value: u64) -> Result<i64, WaitAuthorityError> {
    i64::try_from(value).map_err(|_| WaitAuthorityError::CorruptRecord("u64 exceeds SQLite i64"))
}

fn decode_u64(value: i64) -> Result<u64, WaitAuthorityError> {
    u64::try_from(value).map_err(|_| WaitAuthorityError::CorruptRecord("negative integer"))
}

fn decode_generation(value: i64) -> Result<Generation, WaitAuthorityError> {
    let value = decode_u64(value)?;
    NonZeroU64::new(value)
        .map(Generation::new)
        .ok_or(WaitAuthorityError::CorruptRecord("zero generation"))
}

fn array16(bytes: Vec<u8>) -> Result<[u8; 16], WaitAuthorityError> {
    bytes
        .try_into()
        .map_err(|_| WaitAuthorityError::CorruptRecord("identity length is not 16"))
}

fn array32(bytes: Vec<u8>) -> Result<[u8; 32], WaitAuthorityError> {
    bytes
        .try_into()
        .map_err(|_| WaitAuthorityError::CorruptRecord("digest length is not 32"))
}
