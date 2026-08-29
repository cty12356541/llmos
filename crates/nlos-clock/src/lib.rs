//! Durable local monotonic clock authority (ADR-0011 `AuthorityClock`).
//!
//! This Stage-B slice owns the local monotonic time semantics that
//! validity/anti-replay consumers build on, replacing the "caller-supplied
//! time" placeholder (ADR-0010 explicit non-goal, closed by ADR-0011).  The
//! authority is a pure logical clock: it never reads a wall clock or any
//! external time source.  [`AuthorityClock::now`] advances a durable
//! high-water mark (`reading`) by exactly one per distinct idempotency key
//! and returns the new [`Reading`]; the advance and its durable tick receipt
//! commit in one `Immediate` transaction with a read-then-write CAS against
//! the previous high-water.  A replayed key returns the durably recorded
//! original reading without moving the watermark — same-key replays neither
//! regress nor double-jump.  After any restart the next reading is at least
//! the last durably committed one: the reading can never go backwards.
//!
//! Schema v1 keeps the high-water in a **single-row watermark table** (seeded
//! at migration with reading 0, "no reading issued yet") guarded by STRICT
//! CHECKs and DDL triggers: the row cannot be deleted or re-inserted, its
//! singleton identity is frozen, and any update that would decrease the
//! reading aborts.  Combined with `SQLite`'s transaction atomicity this makes
//! the monotonicity invariant fail-closed at every layer: a crash leaves the
//! watermark either at the old or the new value, never between, and no
//! writer — not even raw SQL — can move it down.  (Design rationale: an
//! append-only tick log derives monotonicity only through a max-scan over an
//! unbounded table and needs a predecessor guard to reject lower inserts;
//! the single-row watermark carries the invariant in the row itself and
//! keeps the durable surface minimal.)
//!
//! The slice deliberately does not implement: wall-clock or external time
//! source alignment (an explicit ADR-0011 review trigger, not a feature of
//! this prefix), cross-process IPC transport, a validity/anti-replay
//! issuance API (consumers wire in later slices), persisted epoch/offset
//! facts (none were needed; registering them is the owning slice's job per
//! ADR-0011), or `TaskWriteSet` integration.

mod schema;

use std::error::Error;
use std::fmt;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use nlos_types::IdempotencyKey;
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

/// One durable monotonic clock reading: the value the watermark held after
/// some [`AuthorityClock::now`] execution committed.  Readings are dense
/// (`u64`, starting at 1), totally ordered, and never reused or reissued.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Reading(u64);

impl Reading {
    /// Rebuilds a reading from its `u64` value (the wire/counter form).
    #[must_use]
    pub const fn from_u64(value: u64) -> Self {
        Self(value)
    }

    /// The `u64` counter value of this reading.
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl From<u64> for Reading {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl fmt::Display for Reading {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// A clock tick request: the caller's idempotency key for exactly one
/// monotonic reading.  There are no other input fields — every key value is
/// legal, and the same key always denotes the same durable reading.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NowRequest {
    pub idempotency_key: IdempotencyKey,
}

/// The outcome of one [`AuthorityClock::now`] call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NowDecision {
    /// First execution of this key: the watermark advanced by exactly one
    /// and the tick receipt committed with it.
    Tick(Reading),
    /// Durable replay: this key already advanced the watermark and the
    /// recorded original reading is returned unchanged (no re-advance, no
    /// double-jump).
    Replayed(Reading),
}

impl NowDecision {
    /// The reading this call denotes, whichever branch produced it.
    #[must_use]
    pub const fn reading(self) -> Reading {
        match self {
            Self::Tick(reading) | Self::Replayed(reading) => reading,
        }
    }
}

/// Fail-closed typed errors of the clock authority.  Every variant is a hard
/// refusal: the caller never receives a reading whose durability is in
/// doubt, and a rejected tick leaves zero durable state.
#[derive(Debug)]
pub enum AuthorityClockError {
    /// Storage failure (injected I/O error, disk full, corruption surfaced
    /// by `SQLite`).
    Sqlite(rusqlite::Error),
    /// Filesystem failure outside `SQLite` (root directory creation).
    Io(std::io::Error),
    /// `SQLite` cannot provide WAL/FULL durability on this platform.
    DurabilityUnavailable {
        journal_mode: String,
        synchronous: i64,
    },
    /// The stored schema version is unknown to this build.
    SchemaVersionUnsupported(i64),
    /// A durable invariant is violated: missing watermark row, a CAS that
    /// lost, a reading outside the representable range.
    CorruptRecord(&'static str),
    /// The authority writer lock is poisoned.
    LockPoisoned,
}

impl fmt::Display for AuthorityClockError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(formatter, "SQLite clock authority failure: {error}"),
            Self::Io(error) => write!(formatter, "clock authority I/O failure: {error}"),
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
                    "unsupported clock authority schema version {version}"
                )
            }
            Self::CorruptRecord(reason) => write!(formatter, "corrupt clock record: {reason}"),
            Self::LockPoisoned => formatter.write_str("clock authority writer lock is poisoned"),
        }
    }
}

impl Error for AuthorityClockError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for AuthorityClockError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

/// A single-node durable monotonic clock bound to its own `SQLite` store
/// (`clock-authority.db`, WAL/FULL, single writer), with the watermark
/// guarded by trigger-enforced monotonicity.
pub struct AuthorityClock {
    connection: Mutex<Connection>,
}

impl AuthorityClock {
    /// Opens or creates `<root>/clock-authority.db`.
    ///
    /// # Errors
    ///
    /// Fails closed when `SQLite` cannot provide WAL/FULL durability, when
    /// the root directory cannot be created, or when a stored schema version
    /// is unknown.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, AuthorityClockError> {
        std::fs::create_dir_all(root.as_ref()).map_err(AuthorityClockError::Io)?;
        let mut connection = Connection::open(root.as_ref().join("clock-authority.db"))?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;

        let journal_mode: String =
            connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        let synchronous: i64 =
            connection.pragma_query_value(None, "synchronous", |row| row.get(0))?;
        if !journal_mode.eq_ignore_ascii_case("wal") || synchronous != 2 {
            return Err(AuthorityClockError::DurabilityUnavailable {
                journal_mode,
                synchronous,
            });
        }

        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        match version {
            0 => schema::migrate_v1(&mut connection)?,
            schema::SCHEMA_VERSION => {}
            other => return Err(AuthorityClockError::SchemaVersionUnsupported(other)),
        }
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// Advances the durable high-water by exactly one and returns the new
    /// [`Reading`], initializing the first reading (`1`) on the very first
    /// call against a fresh store.
    ///
    /// The whole tick — replay check, watermark read, CAS-guarded write
    /// (`UPDATE ... WHERE reading = <observed>`), tick-receipt insert and
    /// commit — runs in one `Immediate` transaction.  The first execution of
    /// a key returns [`NowDecision::Tick`]; the exact same key afterwards
    /// (and after any restart) returns [`NowDecision::Replayed`] carrying
    /// the durably recorded original reading, without touching the
    /// watermark: replays never regress and never double-jump.
    ///
    /// Monotonicity is enforced at every layer: the transaction makes a
    /// crash leave the watermark at the old or the new value (never
    /// between), the CAS update refuses to act on a stale observation, and
    /// the DDL guard aborts any update that would decrease the reading.
    ///
    /// # Errors
    ///
    /// Fails closed (zero durable state change) for a storage failure, a
    /// lost watermark CAS, a reading outside the representable range, or a
    /// corrupted store.
    pub fn now(&self, request: NowRequest) -> Result<NowDecision, AuthorityClockError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(reading) = load_receipt(&transaction, request.idempotency_key)? {
            transaction.commit()?;
            return Ok(NowDecision::Replayed(reading));
        }

        let current = load_watermark(&transaction)?;
        let next = Reading::from_u64(current.as_u64().checked_add(1).ok_or(
            AuthorityClockError::CorruptRecord("clock reading space is exhausted"),
        )?);
        let changed = transaction.execute(
            "UPDATE watermark SET reading=?1 WHERE singleton=1 AND reading=?2",
            params![encode_reading(next)?, encode_reading(current)?],
        )?;
        if changed != 1 {
            return Err(AuthorityClockError::CorruptRecord("watermark CAS lost"));
        }
        transaction.execute(
            "INSERT INTO tick_receipts (idempotency_key, reading) VALUES (?1, ?2)",
            params![
                request.idempotency_key.as_bytes().as_slice(),
                encode_reading(next)?,
            ],
        )?;
        transaction.commit()?;
        Ok(NowDecision::Tick(next))
    }

    /// Reads the durable high-water without any durable side effect.
    ///
    /// This is the restart-side verification read: the returned reading is
    /// the value `now` is guaranteed to never fall below, even after a
    /// crash between two ticks.
    ///
    /// # Errors
    ///
    /// Fails closed when the watermark row is missing, carries an
    /// unrepresentable value, or the read fails.
    pub fn inspect(&self) -> Result<Reading, AuthorityClockError> {
        let connection = self.lock()?;
        load_watermark(&connection)
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>, AuthorityClockError> {
        self.connection
            .lock()
            .map_err(|_| AuthorityClockError::LockPoisoned)
    }
}

fn load_watermark(connection: &Connection) -> Result<Reading, AuthorityClockError> {
    let raw: Option<i64> = connection
        .query_row(
            "SELECT reading FROM watermark WHERE singleton=1",
            params![],
            |row| row.get(0),
        )
        .optional()?;
    decode_reading(raw.ok_or(AuthorityClockError::CorruptRecord(
        "watermark row is missing",
    ))?)
}

fn load_receipt(
    connection: &Connection,
    key: IdempotencyKey,
) -> Result<Option<Reading>, AuthorityClockError> {
    let raw: Option<i64> = connection
        .query_row(
            "SELECT reading FROM tick_receipts WHERE idempotency_key=?1",
            [key.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .optional()?;
    raw.map(decode_reading).transpose()
}

fn encode_reading(value: Reading) -> Result<i64, AuthorityClockError> {
    i64::try_from(value.as_u64())
        .map_err(|_| AuthorityClockError::CorruptRecord("reading exceeds SQLite i64"))
}

fn decode_reading(value: i64) -> Result<Reading, AuthorityClockError> {
    u64::try_from(value)
        .map(Reading::from_u64)
        .map_err(|_| AuthorityClockError::CorruptRecord("negative reading"))
}
