//! Durable local monotonic clock authority (ADR-0011 `AuthorityClock`).
//!
//! This Stage-B slice owns the local time semantics that validity/anti-replay
//! consumers build on, replacing the "caller-supplied time" placeholder
//! (ADR-0010 explicit non-goal, closed by ADR-0011).  It carries **two
//! strictly separated domains**:
//!
//! - **Logical tick domain** — a pure logical clock that never reads a wall
//!   clock or any external time source.  [`AuthorityClock::now`] advances a
//!   durable high-water mark (`reading`) by exactly one per distinct
//!   idempotency key and returns the new [`Reading`]; the advance and its
//!   durable tick receipt commit in one `Immediate` transaction with a
//!   read-then-write CAS against the previous high-water.  A replayed key
//!   returns the durably recorded original reading without moving the
//!   watermark — same-key replays neither regress nor double-jump.
//! - **Wall domain** — [`AuthorityClock::wall_now`] reads the system wall
//!   clock (milliseconds since the Unix epoch, unit `ms`) through an
//!   injectable [`WallSource`] and persists a **durable wall high-water**
//!   ([`WallReading`]): every reading is `max(durable watermark, system
//!   clock)`, so after any restart or system clock rollback the next reading
//!   is still at least the last durably committed value.  The first call
//!   against a fresh store *bootstraps* the watermark from the system clock;
//!   each advance and its idempotent receipt commit in one `Immediate`
//!   transaction (same CAS discipline as the tick domain).  Wall readings
//!   are *not* dense: distinct keys issued within the same millisecond share
//!   one reading, and a replayed key returns its durably recorded original
//!   reading without consulting the system clock.  A wall source that cannot
//!   provide a reading fails closed
//!   ([`AuthorityClockError::WallClockUnavailable`]); no time is ever
//!   guessed.
//!
//! The domains share neither watermark nor receipts: `now` never reads a
//! clock, `wall_now` never advances the tick counter, and the two receipt
//! tables are independent, so a key used in one domain is invisible to the
//! other.
//!
//! Schema v2 keeps each domain's high-water in its **own single-row
//! watermark table** (seeded at migration with reading 0, "no reading issued
//! yet") guarded by STRICT CHECKs and DDL triggers: rows cannot be deleted
//! or re-inserted, singleton identity is frozen, and any update that would
//! decrease a reading aborts.  Combined with `SQLite`'s transaction
//! atomicity this makes the monotonicity invariants fail-closed at every
//! layer: a crash leaves a watermark either at the old or the new value,
//! never between, and no writer — not even raw SQL — can move it down.
//! (Design rationale: an append-only tick log derives monotonicity only
//! through a max-scan over an unbounded table and needs a predecessor guard
//! to reject lower inserts; the single-row watermark carries the invariant
//! in the row itself and keeps the durable surface minimal.)
//!
//! The slice deliberately does not implement: external time source alignment
//! (an explicit ADR-0011 review trigger, not a feature of this prefix — wall
//! readings anchor to the local system clock only), cross-process IPC
//! transport, a validity/anti-replay issuance API (consumers wire in later
//! slices), persisted epoch/offset facts (none were needed; registering them
//! is the owning slice's job per ADR-0011), or `TaskWriteSet` integration.

mod schema;

use std::error::Error;
use std::fmt;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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

/// One durable wall-clock reading: **milliseconds since the Unix epoch**,
/// never below any previously issued or durably committed wall reading.
/// Unlike [`Reading`] the wall domain is not dense — distinct keys issued
/// within the same millisecond share a reading, because the value is the
/// high-water of the system clock, not a per-key counter.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct WallReading(u64);

impl WallReading {
    /// Rebuilds a wall reading from its `u64` epoch-milliseconds value.
    #[must_use]
    pub const fn from_u64(value: u64) -> Self {
        Self(value)
    }

    /// The `u64` epoch-milliseconds value of this reading.
    #[must_use]
    pub const fn as_u64(self) -> u64 {
        self.0
    }
}

impl From<u64> for WallReading {
    fn from(value: u64) -> Self {
        Self(value)
    }
}

impl fmt::Display for WallReading {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// A clock reading request: the caller's idempotency key for exactly one
/// reading.  There are no other input fields — every key value is legal, and
/// the same key always denotes the same durable reading *within its domain*:
/// `now` and `wall_now` track receipts in independent tables, so the same
/// key may denote a logical tick reading and a wall reading side by side.
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

/// The outcome of one [`AuthorityClock::wall_now`] call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WallNowDecision {
    /// First execution of this key: the durable wall high-water advanced to
    /// `max(durable watermark, system clock)` and the wall receipt committed
    /// with it (the advance may be zero-width when the system clock has not
    /// moved past the watermark).
    Advanced(WallReading),
    /// Durable replay: this key already advanced the wall watermark and the
    /// recorded original reading is returned unchanged — without consulting
    /// the system clock (no re-read, no double-jump).
    Replayed(WallReading),
}

impl WallNowDecision {
    /// The reading this call denotes, whichever branch produced it.
    #[must_use]
    pub const fn reading(self) -> WallReading {
        match self {
            Self::Advanced(reading) | Self::Replayed(reading) => reading,
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
    /// The wall source could not provide a reading (system clock before the
    /// Unix epoch, or an injected test source refusal).  Fail-closed: no
    /// time is guessed and zero durable state changes.
    WallClockUnavailable,
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
            Self::WallClockUnavailable => {
                formatter.write_str("wall clock source is unavailable; refusing to guess a time")
            }
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

/// The wall-clock source behind [`AuthorityClock::wall_now`].  Production
/// uses [`SystemWallSource`]; tests inject controlled sources to model
/// system-clock rollback and refusal.  Fail-closed by contract: an
/// implementation that cannot produce a trustworthy reading returns an
/// error instead of a guessed value.
pub trait WallSource: Send + Sync {
    /// The current wall time in milliseconds since the Unix epoch.
    ///
    /// # Errors
    ///
    /// Returned instead of a fabricated value whenever no trustworthy
    /// reading exists.
    fn now_ms(&self) -> Result<u64, AuthorityClockError>;
}

/// The production [`WallSource`]: the local system clock.  A clock set
/// before the Unix epoch (or an unrepresentable reading) yields
/// [`AuthorityClockError::WallClockUnavailable`] — never a guessed time.
pub struct SystemWallSource;

impl WallSource for SystemWallSource {
    fn now_ms(&self) -> Result<u64, AuthorityClockError> {
        let elapsed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| AuthorityClockError::WallClockUnavailable)?;
        u64::try_from(elapsed.as_millis()).map_err(|_| AuthorityClockError::WallClockUnavailable)
    }
}

/// A single-node durable monotonic clock bound to its own `SQLite` store
/// (`clock-authority.db`, WAL/FULL, single writer), with each domain's
/// watermark guarded by trigger-enforced monotonicity: the logical tick
/// high-water and the wall high-water live side by side without sharing
/// state.
pub struct AuthorityClock {
    connection: Mutex<Connection>,
    wall_source: Box<dyn WallSource + Send + Sync>,
}

impl AuthorityClock {
    /// Opens or creates `<root>/clock-authority.db` with the system clock as
    /// the wall source.
    ///
    /// # Errors
    ///
    /// Fails closed when `SQLite` cannot provide WAL/FULL durability, when
    /// the root directory cannot be created, or when a stored schema version
    /// is unknown.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, AuthorityClockError> {
        Self::open_with_wall_source(root, SystemWallSource)
    }

    /// Opens or creates `<root>/clock-authority.db` with an injected
    /// [`WallSource`].  The injection seam exists so consumers and tests can
    /// model system-clock rollback and refusal deterministically;
    /// production callers use [`AuthorityClock::open`].
    ///
    /// # Errors
    ///
    /// Same failure modes as [`AuthorityClock::open`].
    pub fn open_with_wall_source(
        root: impl AsRef<Path>,
        wall_source: impl WallSource + 'static,
    ) -> Result<Self, AuthorityClockError> {
        // A `file:` URI root (fault-injection tests) is not a directory to
        // create; its target directory already exists and Windows rejects
        // the `?`/`:` characters outright.
        if !root.as_ref().to_string_lossy().starts_with("file:") {
            std::fs::create_dir_all(root.as_ref()).map_err(AuthorityClockError::Io)?;
        }
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
            0 => {
                schema::migrate_v1(&mut connection)?;
                schema::migrate_v2(&mut connection)?;
            }
            1 => schema::migrate_v2(&mut connection)?,
            schema::SCHEMA_VERSION => {}
            other => return Err(AuthorityClockError::SchemaVersionUnsupported(other)),
        }
        Ok(Self {
            connection: Mutex::new(connection),
            wall_source: Box::new(wall_source),
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

    /// Reads the system wall clock through the configured [`WallSource`] and
    /// persists a **durable wall high-water** (unit: milliseconds since the
    /// Unix epoch), initializing it from the system clock on the very first
    /// call against a fresh store (bootstrap semantics).
    ///
    /// The whole advance — replay check, system-clock read, watermark read,
    /// CAS-guarded write (`UPDATE ... WHERE reading_ms = <observed>`), wall
    /// receipt insert and commit — runs in one `Immediate` transaction.  A
    /// fresh key issues `max(durable watermark, system clock)`: after any
    /// restart or system-clock rollback the reading is still at least the
    /// last durably committed value, and a source that cannot provide a
    /// reading fails closed ([`AuthorityClockError::WallClockUnavailable`])
    /// with zero durable state change — no time is guessed.  Wall readings
    /// are not dense: keys issued within the same millisecond share a
    /// reading.  The first execution of a key returns
    /// [`WallNowDecision::Advanced`]; the exact same key afterwards (and
    /// after any restart) returns [`WallNowDecision::Replayed`] carrying the
    /// durably recorded original reading without consulting the system
    /// clock: replays never regress and never double-jump.
    ///
    /// This is the wall domain, strictly separate from [`AuthorityClock::now`]
    /// (the logical tick domain): the two share neither watermark nor
    /// receipts, and `now`'s semantics are untouched by this method.
    ///
    /// # Errors
    ///
    /// Fails closed (zero durable state change) for an unavailable wall
    /// source, a storage failure, a lost watermark CAS, a reading outside
    /// the representable range, or a corrupted store.
    pub fn wall_now(&self, request: NowRequest) -> Result<WallNowDecision, AuthorityClockError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(reading) = load_wall_receipt(&transaction, request.idempotency_key)? {
            transaction.commit()?;
            return Ok(WallNowDecision::Replayed(reading));
        }

        let system_ms = self.wall_source.now_ms()?;
        let current = load_wall_watermark(&transaction)?;
        let next = WallReading::from_u64(current.as_u64().max(system_ms));
        let changed = transaction.execute(
            "UPDATE wall_watermark SET reading_ms=?1 WHERE singleton=1 AND reading_ms=?2",
            params![encode_wall(next)?, encode_wall(current)?],
        )?;
        if changed != 1 {
            return Err(AuthorityClockError::CorruptRecord(
                "wall watermark CAS lost",
            ));
        }
        transaction.execute(
            "INSERT INTO wall_receipts (idempotency_key, reading_ms) VALUES (?1, ?2)",
            params![
                request.idempotency_key.as_bytes().as_slice(),
                encode_wall(next)?,
            ],
        )?;
        transaction.commit()?;
        Ok(WallNowDecision::Advanced(next))
    }

    /// Reads the durable wall high-water without any durable side effect.
    ///
    /// This is the restart-side verification read of the wall domain: the
    /// returned reading is the value `wall_now` is guaranteed to never fall
    /// below, even after a crash or a system-clock rollback.
    ///
    /// # Errors
    ///
    /// Fails closed when the wall watermark row is missing, carries an
    /// unrepresentable value, or the read fails.
    pub fn inspect_wall(&self) -> Result<WallReading, AuthorityClockError> {
        let connection = self.lock()?;
        load_wall_watermark(&connection)
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

fn load_wall_watermark(connection: &Connection) -> Result<WallReading, AuthorityClockError> {
    let raw: Option<i64> = connection
        .query_row(
            "SELECT reading_ms FROM wall_watermark WHERE singleton=1",
            params![],
            |row| row.get(0),
        )
        .optional()?;
    decode_wall(raw.ok_or(AuthorityClockError::CorruptRecord(
        "wall watermark row is missing",
    ))?)
}

fn load_wall_receipt(
    connection: &Connection,
    key: IdempotencyKey,
) -> Result<Option<WallReading>, AuthorityClockError> {
    let raw: Option<i64> = connection
        .query_row(
            "SELECT reading_ms FROM wall_receipts WHERE idempotency_key=?1",
            [key.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .optional()?;
    raw.map(decode_wall).transpose()
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

fn encode_wall(value: WallReading) -> Result<i64, AuthorityClockError> {
    i64::try_from(value.as_u64())
        .map_err(|_| AuthorityClockError::CorruptRecord("wall reading exceeds SQLite i64"))
}

fn decode_wall(value: i64) -> Result<WallReading, AuthorityClockError> {
    u64::try_from(value)
        .map(WallReading::from_u64)
        .map_err(|_| AuthorityClockError::CorruptRecord("negative wall reading"))
}
