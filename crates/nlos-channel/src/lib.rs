//! Durable local Channel endpoint authority.
//!
//! This Stage-B slice owns the stable Channel identity, generation fence and
//! authority-derived participant proof required before a Task-scoped handle
//! can be registered, plus the owner-side durable queue delivery prefix:
//! [`ChannelAuthority::enqueue`] with capacity admission and fencing CAS,
//! zero-write ordered [`ChannelAuthority::receive`], the non-destructive
//! consume high-water advanced by [`ChannelAuthority::ack`], and explicit
//! [`ChannelAuthority::compact`] trimming guarded by a durable trim
//! high-water.  It deliberately does not implement Topic routing, fanout,
//! payer accounting, retention policy beyond explicit compaction, cancel
//! propagation, wakeup wiring, or `TaskWriteSet` integration.

mod schema;

use std::error::Error;
use std::fmt;
use std::num::NonZeroU64;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use nlos_types::{ChannelId, Generation, IdempotencyKey, ReceiptId, TaskParticipantId};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};

pub type FencingToken = [u8; 32];

/// The minimum durable queue capacity accepted by the endpoint authority.
/// Queue admission and actual delivery remain outside this slice.
pub const MIN_CAPACITY_BYTES: u64 = 1;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CreateChannelRequest {
    pub capacity_bytes: u64,
    pub policy_digest: [u8; 32],
    pub idempotency_key: IdempotencyKey,
    pub created_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChannelRecord {
    pub channel_id: ChannelId,
    pub generation: Generation,
    pub fencing_token: FencingToken,
    pub capacity_bytes: u64,
    pub policy_digest: [u8; 32],
    pub idempotency_key: IdempotencyKey,
    pub created_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChannelDecision {
    Created(ChannelRecord),
    Replayed(ChannelRecord),
}

impl ChannelDecision {
    #[must_use]
    pub const fn record(self) -> ChannelRecord {
        match self {
            Self::Created(record) | Self::Replayed(record) => record,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RotateChannelRequest {
    pub channel_id: ChannelId,
    pub expected_generation: Generation,
    pub expected_fencing_token: FencingToken,
    pub idempotency_key: IdempotencyKey,
    pub rotated_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChannelRotationDecision {
    Rotated(ChannelRecord),
    Replayed(ChannelRecord),
}

impl ChannelRotationDecision {
    #[must_use]
    pub const fn record(self) -> ChannelRecord {
        match self {
            Self::Rotated(record) | Self::Replayed(record) => record,
        }
    }
}

/// Proof returned by the Channel owner after durable current-generation
/// readback.  Consumers must not construct this tuple from caller input.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ChannelEndpointProof {
    pub channel_id: ChannelId,
    pub participant_id: TaskParticipantId,
    pub participant_generation: Generation,
    pub admission_receipt_id: ReceiptId,
}

/// A durable queue delivery append carrying the writer's fencing CAS.
///
/// The request must present the Channel's current generation and fencing
/// token; a stale fence fails closed with
/// [`ChannelAuthorityError::StaleChannel`] before any durable write.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnqueueRequest {
    pub channel_id: ChannelId,
    pub expected_generation: Generation,
    pub expected_fencing_token: FencingToken,
    pub payload: Vec<u8>,
    pub idempotency_key: IdempotencyKey,
    pub enqueued_at_ms: u64,
}

/// One immutable durable queue entry, exactly as written by the owner.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QueueEntryRecord {
    pub channel_id: ChannelId,
    pub generation: Generation,
    pub fencing_token: FencingToken,
    pub sequence: u64,
    pub payload: Vec<u8>,
    pub payload_bytes: u64,
    pub idempotency_key: IdempotencyKey,
    pub enqueued_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum EnqueueDecision {
    Enqueued(QueueEntryRecord),
    Replayed(QueueEntryRecord),
}

impl EnqueueDecision {
    #[must_use]
    pub fn record(self) -> QueueEntryRecord {
        match self {
            Self::Enqueued(record) | Self::Replayed(record) => record,
        }
    }
}

/// A consume confirmation advancing the Channel's consume high-water.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AckRequest {
    pub channel_id: ChannelId,
    pub up_to_sequence: u64,
    pub acked_at_ms: u64,
}

/// The durable consume decision record for one Channel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AckReceipt {
    pub channel_id: ChannelId,
    pub consume_high_water: u64,
    pub acked_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AckDecision {
    Advanced(AckReceipt),
    Replayed(AckReceipt),
}

impl AckDecision {
    #[must_use]
    pub const fn receipt(self) -> AckReceipt {
        match self {
            Self::Advanced(receipt) | Self::Replayed(receipt) => receipt,
        }
    }
}

/// The durable trim decision record for one Channel.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CompactReceipt {
    pub channel_id: ChannelId,
    pub trim_high_water: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompactDecision {
    Trimmed(CompactReceipt),
    Replayed(CompactReceipt),
}

impl CompactDecision {
    #[must_use]
    pub const fn receipt(self) -> CompactReceipt {
        match self {
            Self::Trimmed(receipt) | Self::Replayed(receipt) => receipt,
        }
    }
}

/// The verified queue state of one Channel after cross-checking the cursor
/// and byte bookkeeping rows against the durable entries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QueueState {
    pub channel_id: ChannelId,
    pub capacity_bytes: u64,
    /// Inclusive upper bound of consumed sequences (0 = nothing consumed).
    pub consume_high_water: u64,
    /// Inclusive upper bound of the compacted prefix; always
    /// `<= consume_high_water`, so the prefix was consumed before trimming.
    pub trim_high_water: u64,
    /// Sum of `payload_bytes` over entries with `sequence >
    /// consume_high_water` (the unacknowledged backlog admitted against the
    /// capacity).
    pub backlog_bytes: u64,
    /// Sum of `payload_bytes` over all live entries, including the consumed
    /// prefix that explicit compaction has not deleted yet.
    pub retained_bytes: u64,
    /// Highest sequence ever durably written for this Channel.
    pub max_sequence: u64,
}

#[derive(Debug)]
pub enum ChannelAuthorityError {
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
    DurabilityUnavailable {
        journal_mode: String,
        synchronous: i64,
    },
    SchemaVersionUnsupported(i64),
    ChannelNotFound(ChannelId),
    IdempotencyConflict,
    StaleChannel,
    InvalidCapacity,
    GenerationExhausted,
    QueueFull,
    InvalidPayload,
    InvalidSequence(&'static str),
    CorruptRecord(&'static str),
    LockPoisoned,
}

impl fmt::Display for ChannelAuthorityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(formatter, "SQLite channel authority failure: {error}"),
            Self::Io(error) => write!(formatter, "channel authority I/O failure: {error}"),
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
                    "unsupported channel authority schema version {version}"
                )
            }
            Self::ChannelNotFound(id) => write!(formatter, "channel {id:?} does not exist"),
            Self::IdempotencyConflict => formatter.write_str(
                "idempotency key or authority-assigned identity was rebound to different input",
            ),
            Self::StaleChannel => {
                formatter.write_str("channel generation or fencing token is stale")
            }
            Self::InvalidCapacity => formatter.write_str("channel capacity must be non-zero"),
            Self::GenerationExhausted => formatter.write_str("channel generation space exhausted"),
            Self::QueueFull => {
                formatter.write_str("queue backlog plus payload exceeds channel capacity")
            }
            Self::InvalidPayload => formatter.write_str("queue payload must be non-empty"),
            Self::InvalidSequence(reason) => {
                write!(formatter, "invalid queue sequence: {reason}")
            }
            Self::CorruptRecord(reason) => write!(formatter, "corrupt channel record: {reason}"),
            Self::LockPoisoned => formatter.write_str("channel authority writer lock is poisoned"),
        }
    }
}

impl Error for ChannelAuthorityError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for ChannelAuthorityError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

/// A single-node Channel owner with WAL/FULL durable generation records.
pub struct ChannelAuthority {
    connection: Mutex<Connection>,
}

impl ChannelAuthority {
    /// Opens or creates `<root>/channel-authority.db`.
    ///
    /// # Errors
    ///
    /// Fails closed when `SQLite` cannot provide WAL/FULL durability or when a
    /// stored schema version is unknown.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, ChannelAuthorityError> {
        std::fs::create_dir_all(root.as_ref()).map_err(ChannelAuthorityError::Io)?;
        let mut connection = Connection::open(root.as_ref().join("channel-authority.db"))?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;

        let journal_mode: String =
            connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        let synchronous: i64 =
            connection.pragma_query_value(None, "synchronous", |row| row.get(0))?;
        if !journal_mode.eq_ignore_ascii_case("wal") || synchronous != 2 {
            return Err(ChannelAuthorityError::DurabilityUnavailable {
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
            other => return Err(ChannelAuthorityError::SchemaVersionUnsupported(other)),
        }
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// Registers a Channel identity and its initial immutable generation.
    ///
    /// The endpoint proof is inserted in the same transaction as the head;
    /// repeating the exact request returns the original durable record.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid capacity, idempotency rebinding, storage
    /// failure, or an unsupported durable record.
    pub fn create_channel(
        &self,
        request: CreateChannelRequest,
    ) -> Result<ChannelDecision, ChannelAuthorityError> {
        validate_capacity(request.capacity_bytes)?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = load_by_create_key(&transaction, request.idempotency_key)? {
            if existing.capacity_bytes != request.capacity_bytes
                || existing.policy_digest != request.policy_digest
            {
                return Err(ChannelAuthorityError::IdempotencyConflict);
            }
            transaction.commit()?;
            return Ok(ChannelDecision::Replayed(existing));
        }

        let channel_id = ChannelId::from_bytes(derive_id(
            b"nlos/channel/id/v1",
            &[
                request.idempotency_key.as_bytes(),
                &request.capacity_bytes.to_be_bytes(),
                request.policy_digest.as_slice(),
            ],
        ));
        if load_current_optional(&transaction, channel_id)?.is_some() {
            return Err(ChannelAuthorityError::IdempotencyConflict);
        }
        let record = make_record(
            channel_id,
            Generation::INITIAL,
            request.capacity_bytes,
            request.policy_digest,
            request.idempotency_key,
            request.created_at_ms,
        );
        insert_head(&transaction, &record)?;
        insert_identity(&transaction, record.channel_id)?;
        insert_generation(&transaction, &record)?;
        insert_endpoint_proof(&transaction, &record)?;
        insert_queue_state(&transaction, record.channel_id)?;
        transaction.commit()?;
        Ok(ChannelDecision::Created(record))
    }

    /// Rotates the current Channel generation with an expected fence CAS.
    ///
    /// Old generation handles remain readable only as historical rows; this
    /// authority's current endpoint proof advances with the new generation.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown Channel, a stale generation/fence,
    /// idempotency rebinding, generation exhaustion, or storage corruption.
    pub fn rotate_channel(
        &self,
        request: RotateChannelRequest,
    ) -> Result<ChannelRotationDecision, ChannelAuthorityError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = load_rotation_by_key(&transaction, request.idempotency_key)? {
            if existing.channel_id != request.channel_id
                || existing.expected_generation != request.expected_generation
                || existing.expected_fencing_token != request.expected_fencing_token
            {
                return Err(ChannelAuthorityError::IdempotencyConflict);
            }
            let record = load_generation(
                &transaction,
                request.channel_id,
                existing.resulting_generation,
            )?;
            if record.fencing_token != existing.resulting_fencing_token {
                return Err(ChannelAuthorityError::CorruptRecord(
                    "rotation receipt disagrees with channel generation",
                ));
            }
            transaction.commit()?;
            return Ok(ChannelRotationDecision::Replayed(record));
        }

        let current = load_current_optional(&transaction, request.channel_id)?
            .ok_or(ChannelAuthorityError::ChannelNotFound(request.channel_id))?;
        if current.generation != request.expected_generation
            || current.fencing_token != request.expected_fencing_token
        {
            return Err(ChannelAuthorityError::StaleChannel);
        }
        let generation = current
            .generation
            .checked_next()
            .ok_or(ChannelAuthorityError::GenerationExhausted)?;
        let record = make_record(
            current.channel_id,
            generation,
            current.capacity_bytes,
            current.policy_digest,
            current.idempotency_key,
            request.rotated_at_ms,
        );
        insert_generation(&transaction, &record)?;
        insert_endpoint_proof(&transaction, &record)?;
        transaction.execute(
            "INSERT INTO channel_rotations (
                idempotency_key, channel_id, expected_generation,
                expected_fencing_token, resulting_generation,
                resulting_fencing_token, rotated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                request.idempotency_key.as_bytes().as_slice(),
                request.channel_id.as_bytes().as_slice(),
                encode_generation(request.expected_generation)?,
                request.expected_fencing_token.as_slice(),
                encode_generation(generation)?,
                record.fencing_token.as_slice(),
                encode_u64(request.rotated_at_ms)?,
            ],
        )?;
        let changed = transaction.execute(
            "UPDATE channels
             SET current_generation=?1, current_fencing_token=?2, updated_at_ms=?3
             WHERE channel_id=?4 AND current_generation=?5 AND current_fencing_token=?6",
            params![
                encode_generation(generation)?,
                record.fencing_token.as_slice(),
                encode_u64(request.rotated_at_ms)?,
                request.channel_id.as_bytes().as_slice(),
                encode_generation(request.expected_generation)?,
                request.expected_fencing_token.as_slice(),
            ],
        )?;
        if changed != 1 {
            return Err(ChannelAuthorityError::StaleChannel);
        }
        transaction.commit()?;
        Ok(ChannelRotationDecision::Rotated(record))
    }

    /// Reads the current Channel record after checking the head/generation
    /// join and immutable row invariants.
    ///
    /// # Errors
    ///
    /// Returns an error when the Channel is unknown, corrupt, or unreadable.
    pub fn inspect_channel(
        &self,
        channel_id: ChannelId,
    ) -> Result<ChannelRecord, ChannelAuthorityError> {
        let connection = self.lock()?;
        load_current_optional(&connection, channel_id)?
            .ok_or(ChannelAuthorityError::ChannelNotFound(channel_id))
    }

    /// Reads the authority-derived endpoint proof for the current generation.
    ///
    /// # Errors
    ///
    /// Returns an error when the Channel is unknown, its current proof is
    /// missing or corrupt, or durable storage cannot be read.
    pub fn inspect_endpoint_proof(
        &self,
        channel_id: ChannelId,
    ) -> Result<ChannelEndpointProof, ChannelAuthorityError> {
        let connection = self.lock()?;
        let record = load_current_optional(&connection, channel_id)?
            .ok_or(ChannelAuthorityError::ChannelNotFound(channel_id))?;
        let raw = connection
            .query_row(
                "SELECT participant_id, admission_receipt_id
                 FROM channel_endpoint_proofs
                 WHERE channel_id=?1 AND channel_generation=?2",
                params![
                    channel_id.as_bytes().as_slice(),
                    encode_generation(record.generation)?,
                ],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .optional()?
            .ok_or(ChannelAuthorityError::CorruptRecord(
                "current channel generation has no endpoint proof",
            ))?;
        let participant_id = TaskParticipantId::from_bytes(array16(raw.0)?);
        let admission_receipt_id = ReceiptId::from_bytes(array16(raw.1)?);
        let expected_participant = participant_id_for(channel_id);
        let expected_receipt = receipt_id_for(channel_id, record.generation, record.fencing_token);
        if participant_id != expected_participant || admission_receipt_id != expected_receipt {
            return Err(ChannelAuthorityError::CorruptRecord(
                "channel endpoint proof disagrees with authority-derived identity",
            ));
        }
        Ok(ChannelEndpointProof {
            channel_id,
            participant_id,
            participant_generation: record.generation,
            admission_receipt_id,
        })
    }

    /// Appends one payload to the Channel's durable delivery queue.
    ///
    /// The owner allocates a per-Channel globally monotonic `sequence` and
    /// writes the entry, the byte bookkeeping update and nothing else inside
    /// a single `Immediate` transaction.  Order of the fail-closed gates,
    /// all of which run before any durable write:
    ///
    /// 1. empty payloads are rejected ([`ChannelAuthorityError::InvalidPayload`]);
    /// 2. an exact idempotency replay returns the original record
    ///    ([`EnqueueDecision::Replayed`]); a key rebound to a different
    ///    channel, generation, fence or payload is an
    ///    [`ChannelAuthorityError::IdempotencyConflict`];
    /// 3. the fencing CAS requires the current generation and fencing token
    ///    ([`ChannelAuthorityError::StaleChannel`] otherwise);
    /// 4. capacity admission requires `backlog_bytes + payload_bytes <=
    ///    capacity_bytes`, where the backlog counts only entries beyond the
    ///    consume high-water ([`ChannelAuthorityError::QueueFull`] otherwise).
    ///
    /// # Errors
    ///
    /// Fails closed for an empty payload, idempotency rebinding, a stale
    /// fence, exhausted capacity, an unknown Channel, or a storage/corruption
    /// failure.  A rejected enqueue leaves zero durable state: its
    /// idempotency key stays free and a later retry enqueues fresh.
    pub fn enqueue(
        &self,
        request: EnqueueRequest,
    ) -> Result<EnqueueDecision, ChannelAuthorityError> {
        if request.payload.is_empty() {
            return Err(ChannelAuthorityError::InvalidPayload);
        }
        // usize -> u64 is a widening cast on every supported target.
        let payload_bytes = request.payload.len() as u64;
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = load_entry_by_key(&transaction, request.idempotency_key)? {
            if existing.channel_id != request.channel_id
                || existing.generation != request.expected_generation
                || existing.fencing_token != request.expected_fencing_token
                || existing.payload != request.payload
            {
                return Err(ChannelAuthorityError::IdempotencyConflict);
            }
            transaction.commit()?;
            return Ok(EnqueueDecision::Replayed(existing));
        }

        let current = load_current_optional(&transaction, request.channel_id)?
            .ok_or(ChannelAuthorityError::ChannelNotFound(request.channel_id))?;
        if current.generation != request.expected_generation
            || current.fencing_token != request.expected_fencing_token
        {
            return Err(ChannelAuthorityError::StaleChannel);
        }
        let cursors = load_queue_cursors(&transaction, request.channel_id)?;
        let bytes = load_queue_bytes(&transaction, request.channel_id)?;
        let admitted_backlog = bytes
            .backlog_bytes
            .checked_add(payload_bytes)
            .ok_or(ChannelAuthorityError::QueueFull)?;
        if admitted_backlog > current.capacity_bytes {
            return Err(ChannelAuthorityError::QueueFull);
        }
        let retained = bytes.retained_bytes.checked_add(payload_bytes).ok_or(
            ChannelAuthorityError::CorruptRecord("queue retained bytes overflow"),
        )?;
        let max_written =
            max_live_sequence(&transaction, request.channel_id)?.max(cursors.consume_high_water);
        let sequence = max_written
            .checked_add(1)
            .ok_or(ChannelAuthorityError::CorruptRecord(
                "queue sequence space exhausted",
            ))?;
        let record = QueueEntryRecord {
            channel_id: request.channel_id,
            generation: current.generation,
            fencing_token: current.fencing_token,
            sequence,
            payload: request.payload,
            payload_bytes,
            idempotency_key: request.idempotency_key,
            enqueued_at_ms: request.enqueued_at_ms,
        };
        insert_queue_entry(&transaction, &record)?;
        let changed = transaction.execute(
            "UPDATE channel_queue_bytes
             SET backlog_bytes=?1, retained_bytes=?2
             WHERE channel_id=?3 AND backlog_bytes=?4 AND retained_bytes=?5",
            params![
                encode_u64(admitted_backlog)?,
                encode_u64(retained)?,
                record.channel_id.as_bytes().as_slice(),
                encode_u64(bytes.backlog_bytes)?,
                encode_u64(bytes.retained_bytes)?,
            ],
        )?;
        if changed != 1 {
            return Err(ChannelAuthorityError::CorruptRecord(
                "queue backlog bookkeeping CAS lost",
            ));
        }
        transaction.commit()?;
        Ok(EnqueueDecision::Enqueued(record))
    }

    /// Returns the unacknowledged delivery window in sequence order.
    ///
    /// The window is every entry with `sequence > consume_high_water` and
    /// `sequence > trim_high_water` (the trim watermark is never above the
    /// consume watermark, so the binding constraint is the consume
    /// high-water), ordered by sequence and truncated to `limit`.  The read
    /// path is a per-Channel total order across generations: after a
    /// rotation, unconsumed entries enqueued under the old fence remain
    /// receivable, while [`Self::enqueue`] only accepts the current fence.
    ///
    /// Receive never advances any cursor and performs zero writes; the
    /// caller confirms consumption separately with [`Self::ack`].  The
    /// cursor and byte bookkeeping rows are cross-checked against the
    /// entries first, so a tampered queue fails closed as
    /// [`ChannelAuthorityError::CorruptRecord`].
    ///
    /// # Errors
    ///
    /// Fails closed for an unknown Channel, a queue whose derived state
    /// disagrees with its cursors or bookkeeping, or a read failure.
    pub fn receive(
        &self,
        channel_id: ChannelId,
        limit: usize,
    ) -> Result<Vec<QueueEntryRecord>, ChannelAuthorityError> {
        let connection = self.lock()?;
        load_current_optional(&connection, channel_id)?
            .ok_or(ChannelAuthorityError::ChannelNotFound(channel_id))?;
        let cursors = verify_queue_state(&connection, channel_id)?;
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut statement = connection.prepare(
            "SELECT channel_generation, fencing_token, sequence, payload,
                    payload_bytes, idempotency_key, enqueued_at_ms
             FROM channel_queue_entries
             WHERE channel_id=?1 AND sequence>?2 AND sequence>?3
             ORDER BY sequence
             LIMIT ?4",
        )?;
        let rows = statement
            .query_map(
                params![
                    channel_id.as_bytes().as_slice(),
                    encode_u64(cursors.consume_high_water)?,
                    encode_u64(cursors.trim_high_water)?,
                    limit,
                ],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, Vec<u8>>(5)?,
                        row.get::<_, i64>(6)?,
                    ))
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|row| decode_queue_entry(channel_id, row))
            .collect()
    }

    /// Advances the consume high-water to `up_to_sequence`, monotonically.
    ///
    /// The cursor advance, the backlog bookkeeping decrease for the newly
    /// confirmed prefix and nothing else commit in one `Immediate`
    /// transaction.  Repeating the exact current high-water replays the
    /// original decision (including its stored `acked_at_ms`); requesting a
    /// lower sequence, or one beyond the highest sequence ever written, is
    /// rejected as [`ChannelAuthorityError::InvalidSequence`] before any
    /// write.  Because the trim watermark can never exceed the consume
    /// watermark, an ack can never cross into the compacted prefix.
    ///
    /// # Errors
    ///
    /// Fails closed for an unknown Channel, a regressing or out-of-range
    /// sequence, or a storage/corruption failure.
    pub fn ack(&self, request: AckRequest) -> Result<AckDecision, ChannelAuthorityError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        load_current_optional(&transaction, request.channel_id)?
            .ok_or(ChannelAuthorityError::ChannelNotFound(request.channel_id))?;
        let cursors = load_queue_cursors(&transaction, request.channel_id)?;
        if request.up_to_sequence == cursors.consume_high_water {
            let receipt = AckReceipt {
                channel_id: request.channel_id,
                consume_high_water: cursors.consume_high_water,
                acked_at_ms: cursors.last_ack_at_ms,
            };
            transaction.commit()?;
            return Ok(AckDecision::Replayed(receipt));
        }
        if request.up_to_sequence < cursors.consume_high_water {
            return Err(ChannelAuthorityError::InvalidSequence(
                "queue ack regresses below the consume high-water",
            ));
        }
        let durable_max =
            max_live_sequence(&transaction, request.channel_id)?.max(cursors.consume_high_water);
        if request.up_to_sequence > durable_max {
            return Err(ChannelAuthorityError::InvalidSequence(
                "queue ack exceeds the durable queue maximum",
            ));
        }
        let confirmed: i64 = transaction.query_row(
            "SELECT COALESCE(SUM(payload_bytes), 0)
             FROM channel_queue_entries
             WHERE channel_id=?1 AND sequence>?2 AND sequence<=?3",
            params![
                request.channel_id.as_bytes().as_slice(),
                encode_u64(cursors.consume_high_water)?,
                encode_u64(request.up_to_sequence)?,
            ],
            |row| row.get(0),
        )?;
        let confirmed = decode_u64(confirmed)?;
        let bytes = load_queue_bytes(&transaction, request.channel_id)?;
        let backlog = bytes.backlog_bytes.checked_sub(confirmed).ok_or(
            ChannelAuthorityError::CorruptRecord(
                "queue backlog bookkeeping underflows the confirmed prefix",
            ),
        )?;
        let changed = transaction.execute(
            "UPDATE channel_queue_cursors
             SET consume_high_water=?1, last_ack_at_ms=?2
             WHERE channel_id=?3 AND consume_high_water=?4",
            params![
                encode_u64(request.up_to_sequence)?,
                encode_u64(request.acked_at_ms)?,
                request.channel_id.as_bytes().as_slice(),
                encode_u64(cursors.consume_high_water)?,
            ],
        )?;
        if changed != 1 {
            return Err(ChannelAuthorityError::CorruptRecord(
                "queue consume high-water CAS lost",
            ));
        }
        let changed = transaction.execute(
            "UPDATE channel_queue_bytes
             SET backlog_bytes=?1
             WHERE channel_id=?2 AND backlog_bytes=?3 AND retained_bytes=?4",
            params![
                encode_u64(backlog)?,
                request.channel_id.as_bytes().as_slice(),
                encode_u64(bytes.backlog_bytes)?,
                encode_u64(bytes.retained_bytes)?,
            ],
        )?;
        if changed != 1 {
            return Err(ChannelAuthorityError::CorruptRecord(
                "queue backlog bookkeeping CAS lost",
            ));
        }
        transaction.commit()?;
        Ok(AckDecision::Advanced(AckReceipt {
            channel_id: request.channel_id,
            consume_high_water: request.up_to_sequence,
            acked_at_ms: request.acked_at_ms,
        }))
    }

    /// Deletes the consumed prefix up to `min(trim_to_sequence,
    /// consume_high_water)`.
    ///
    /// Unconsumed entries are never deleted: the request is clamped to the
    /// consume high-water, and a `BEFORE DELETE` trigger additionally aborts
    /// any deletion of a row above the durable trim watermark, so row
    /// deletion is only reachable through this path.  The trim watermark
    /// advance, the prefix deletion and the retained-byte bookkeeping
    /// decrease commit in one `Immediate` transaction.  Repeating the
    /// current effective watermark replays
    /// ([`CompactDecision::Replayed`]); requesting a lower effective
    /// watermark is [`ChannelAuthorityError::InvalidSequence`].
    ///
    /// Trimming deletes durable entries together with their idempotency
    /// records: replaying an enqueue whose entry was already compacted
    /// re-admits it as a new entry with a fresh sequence.
    ///
    /// # Errors
    ///
    /// Fails closed for an unknown Channel, a regressing trim request, or a
    /// storage/corruption failure.
    pub fn compact(
        &self,
        channel_id: ChannelId,
        trim_to_sequence: u64,
    ) -> Result<CompactDecision, ChannelAuthorityError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        load_current_optional(&transaction, channel_id)?
            .ok_or(ChannelAuthorityError::ChannelNotFound(channel_id))?;
        let cursors = load_queue_cursors(&transaction, channel_id)?;
        let target = trim_to_sequence.min(cursors.consume_high_water);
        if target == cursors.trim_high_water {
            let receipt = CompactReceipt {
                channel_id,
                trim_high_water: target,
            };
            transaction.commit()?;
            return Ok(CompactDecision::Replayed(receipt));
        }
        if target < cursors.trim_high_water {
            return Err(ChannelAuthorityError::InvalidSequence(
                "queue trim regresses below the trim high-water",
            ));
        }
        let released: i64 = transaction.query_row(
            "SELECT COALESCE(SUM(payload_bytes), 0)
             FROM channel_queue_entries
             WHERE channel_id=?1 AND sequence>?2 AND sequence<=?3",
            params![
                channel_id.as_bytes().as_slice(),
                encode_u64(cursors.trim_high_water)?,
                encode_u64(target)?,
            ],
            |row| row.get(0),
        )?;
        let released = decode_u64(released)?;
        let bytes = load_queue_bytes(&transaction, channel_id)?;
        let retained = bytes.retained_bytes.checked_sub(released).ok_or(
            ChannelAuthorityError::CorruptRecord(
                "queue retained bookkeeping underflows the trim prefix",
            ),
        )?;
        let changed = transaction.execute(
            "UPDATE channel_queue_cursors
             SET trim_high_water=?1
             WHERE channel_id=?2 AND trim_high_water=?3 AND trim_high_water<=consume_high_water",
            params![
                encode_u64(target)?,
                channel_id.as_bytes().as_slice(),
                encode_u64(cursors.trim_high_water)?,
            ],
        )?;
        if changed != 1 {
            return Err(ChannelAuthorityError::CorruptRecord(
                "queue trim high-water CAS lost",
            ));
        }
        transaction.execute(
            "DELETE FROM channel_queue_entries
             WHERE channel_id=?1 AND sequence<=?2",
            params![channel_id.as_bytes().as_slice(), encode_u64(target)?,],
        )?;
        let changed = transaction.execute(
            "UPDATE channel_queue_bytes
             SET retained_bytes=?1
             WHERE channel_id=?2 AND backlog_bytes=?3 AND retained_bytes=?4",
            params![
                encode_u64(retained)?,
                channel_id.as_bytes().as_slice(),
                encode_u64(bytes.backlog_bytes)?,
                encode_u64(bytes.retained_bytes)?,
            ],
        )?;
        if changed != 1 {
            return Err(ChannelAuthorityError::CorruptRecord(
                "queue retained bookkeeping CAS lost",
            ));
        }
        transaction.commit()?;
        Ok(CompactDecision::Trimmed(CompactReceipt {
            channel_id,
            trim_high_water: target,
        }))
    }

    /// Reads the queue state after cross-checking cursors and bookkeeping.
    ///
    /// The consume/trim high-waters are compared against the entry set
    /// (contiguity with the trim prefix, no residue below it, consume not
    /// beyond the durable maximum) and the byte bookkeeping is re-derived
    /// from the entries; any disagreement is
    /// [`ChannelAuthorityError::CorruptRecord`].
    ///
    /// # Errors
    ///
    /// Fails closed for an unknown Channel, a queue whose derived state
    /// disagrees with its cursors or bookkeeping, or a read failure.
    pub fn inspect_queue(
        &self,
        channel_id: ChannelId,
    ) -> Result<QueueState, ChannelAuthorityError> {
        let connection = self.lock()?;
        let current = load_current_optional(&connection, channel_id)?
            .ok_or(ChannelAuthorityError::ChannelNotFound(channel_id))?;
        let verified = verify_queue_state(&connection, channel_id)?;
        Ok(QueueState {
            channel_id,
            capacity_bytes: current.capacity_bytes,
            consume_high_water: verified.consume_high_water,
            trim_high_water: verified.trim_high_water,
            backlog_bytes: verified.backlog_bytes,
            retained_bytes: verified.retained_bytes,
            max_sequence: verified.max_live_sequence.max(verified.consume_high_water),
        })
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>, ChannelAuthorityError> {
        self.connection
            .lock()
            .map_err(|_| ChannelAuthorityError::LockPoisoned)
    }
}

fn make_record(
    channel_id: ChannelId,
    generation: Generation,
    capacity_bytes: u64,
    policy_digest: [u8; 32],
    idempotency_key: IdempotencyKey,
    created_at_ms: u64,
) -> ChannelRecord {
    ChannelRecord {
        channel_id,
        generation,
        fencing_token: derive_token(
            b"nlos/channel/fence/v1",
            &[
                channel_id.as_bytes(),
                &generation.get().to_be_bytes(),
                idempotency_key.as_bytes(),
            ],
        ),
        capacity_bytes,
        policy_digest,
        idempotency_key,
        created_at_ms,
    }
}

fn participant_id_for(channel_id: ChannelId) -> TaskParticipantId {
    TaskParticipantId::from_bytes(derive_id(
        b"nlos/channel-topic/participant/v1",
        &[channel_id.as_bytes()],
    ))
}

fn receipt_id_for(
    channel_id: ChannelId,
    generation: Generation,
    fencing_token: FencingToken,
) -> ReceiptId {
    ReceiptId::from_bytes(derive_id(
        b"nlos/channel-topic/admission/v1",
        &[
            channel_id.as_bytes(),
            &generation.get().to_be_bytes(),
            fencing_token.as_slice(),
        ],
    ))
}

fn insert_head(
    transaction: &Transaction<'_>,
    record: &ChannelRecord,
) -> Result<(), ChannelAuthorityError> {
    transaction.execute(
        "INSERT INTO channels (
            channel_id, create_idempotency_key, current_generation,
            current_fencing_token, capacity_bytes, policy_digest,
            created_at_ms, updated_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
        params![
            record.channel_id.as_bytes().as_slice(),
            record.idempotency_key.as_bytes().as_slice(),
            encode_generation(record.generation)?,
            record.fencing_token.as_slice(),
            encode_u64(record.capacity_bytes)?,
            record.policy_digest.as_slice(),
            encode_u64(record.created_at_ms)?,
        ],
    )?;
    Ok(())
}

fn insert_generation(
    transaction: &Transaction<'_>,
    record: &ChannelRecord,
) -> Result<(), ChannelAuthorityError> {
    transaction.execute(
        "INSERT INTO channel_generations (
            channel_id, channel_generation, fencing_token, capacity_bytes,
            policy_digest, idempotency_key, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            record.channel_id.as_bytes().as_slice(),
            encode_generation(record.generation)?,
            record.fencing_token.as_slice(),
            encode_u64(record.capacity_bytes)?,
            record.policy_digest.as_slice(),
            record.idempotency_key.as_bytes().as_slice(),
            encode_u64(record.created_at_ms)?,
        ],
    )?;
    Ok(())
}

fn insert_identity(
    transaction: &Transaction<'_>,
    channel_id: ChannelId,
) -> Result<(), ChannelAuthorityError> {
    transaction.execute(
        "INSERT INTO channel_topic_identities (channel_id, participant_id)
         VALUES (?1, ?2)",
        params![
            channel_id.as_bytes().as_slice(),
            participant_id_for(channel_id).as_bytes().as_slice(),
        ],
    )?;
    Ok(())
}

fn insert_endpoint_proof(
    transaction: &Transaction<'_>,
    record: &ChannelRecord,
) -> Result<(), ChannelAuthorityError> {
    transaction.execute(
        "INSERT INTO channel_endpoint_proofs (
            channel_id, channel_generation, participant_id, admission_receipt_id
         ) VALUES (?1, ?2, ?3, ?4)",
        params![
            record.channel_id.as_bytes().as_slice(),
            encode_generation(record.generation)?,
            participant_id_for(record.channel_id).as_bytes().as_slice(),
            receipt_id_for(record.channel_id, record.generation, record.fencing_token)
                .as_bytes()
                .as_slice(),
        ],
    )?;
    Ok(())
}

/// Raw `channel_queue_entries` row without the constant `channel_id` column.
type QueueEntryRow = (i64, Vec<u8>, i64, Vec<u8>, i64, Vec<u8>, i64);

struct QueueCursors {
    consume_high_water: u64,
    trim_high_water: u64,
    last_ack_at_ms: u64,
}

struct QueueBytes {
    backlog_bytes: u64,
    retained_bytes: u64,
}

struct VerifiedQueueState {
    consume_high_water: u64,
    trim_high_water: u64,
    max_live_sequence: u64,
    backlog_bytes: u64,
    retained_bytes: u64,
}

fn insert_queue_state(
    transaction: &Transaction<'_>,
    channel_id: ChannelId,
) -> Result<(), ChannelAuthorityError> {
    transaction.execute(
        "INSERT INTO channel_queue_cursors (channel_id) VALUES (?1)",
        params![channel_id.as_bytes().as_slice()],
    )?;
    transaction.execute(
        "INSERT INTO channel_queue_bytes (channel_id) VALUES (?1)",
        params![channel_id.as_bytes().as_slice()],
    )?;
    Ok(())
}

fn insert_queue_entry(
    transaction: &Transaction<'_>,
    record: &QueueEntryRecord,
) -> Result<(), ChannelAuthorityError> {
    transaction.execute(
        "INSERT INTO channel_queue_entries (
            channel_id, channel_generation, fencing_token, sequence,
            payload, payload_bytes, idempotency_key, enqueued_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            record.channel_id.as_bytes().as_slice(),
            encode_generation(record.generation)?,
            record.fencing_token.as_slice(),
            encode_u64(record.sequence)?,
            record.payload.as_slice(),
            encode_u64(record.payload_bytes)?,
            record.idempotency_key.as_bytes().as_slice(),
            encode_u64(record.enqueued_at_ms)?,
        ],
    )?;
    Ok(())
}

fn decode_queue_entry(
    channel_id: ChannelId,
    row: QueueEntryRow,
) -> Result<QueueEntryRecord, ChannelAuthorityError> {
    let (generation, fencing_token, sequence, payload, payload_bytes, key, enqueued_at_ms) = row;
    let record = QueueEntryRecord {
        channel_id,
        generation: decode_generation(generation)?,
        fencing_token: array32(fencing_token)?,
        sequence: decode_u64(sequence)?,
        payload_bytes: decode_u64(payload_bytes)?,
        payload,
        idempotency_key: IdempotencyKey::from_bytes(array16(key)?),
        enqueued_at_ms: decode_u64(enqueued_at_ms)?,
    };
    // usize -> u64 is a widening cast on every supported target.
    if record.payload_bytes != record.payload.len() as u64 {
        return Err(ChannelAuthorityError::CorruptRecord(
            "queue entry byte count disagrees with its payload",
        ));
    }
    Ok(record)
}

fn load_entry_by_key(
    connection: &Connection,
    key: IdempotencyKey,
) -> Result<Option<QueueEntryRecord>, ChannelAuthorityError> {
    let raw = connection
        .query_row(
            "SELECT channel_id, channel_generation, fencing_token, sequence,
                    payload, payload_bytes, idempotency_key, enqueued_at_ms
             FROM channel_queue_entries WHERE idempotency_key=?1",
            [key.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, Vec<u8>>(6)?,
                    row.get::<_, i64>(7)?,
                ))
            },
        )
        .optional()?;
    raw.map(
        |(channel_id, generation, fencing_token, sequence, payload, payload_bytes, key, at)| {
            decode_queue_entry(
                ChannelId::from_bytes(array16(channel_id)?),
                (
                    generation,
                    fencing_token,
                    sequence,
                    payload,
                    payload_bytes,
                    key,
                    at,
                ),
            )
        },
    )
    .transpose()
}

fn load_queue_cursors(
    connection: &Connection,
    channel_id: ChannelId,
) -> Result<QueueCursors, ChannelAuthorityError> {
    let raw = connection
        .query_row(
            "SELECT consume_high_water, trim_high_water, last_ack_at_ms
             FROM channel_queue_cursors WHERE channel_id=?1",
            [channel_id.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()?
        .ok_or(ChannelAuthorityError::CorruptRecord(
            "channel queue cursor row is missing",
        ))?;
    Ok(QueueCursors {
        consume_high_water: decode_u64(raw.0)?,
        trim_high_water: decode_u64(raw.1)?,
        last_ack_at_ms: decode_u64(raw.2)?,
    })
}

fn load_queue_bytes(
    connection: &Connection,
    channel_id: ChannelId,
) -> Result<QueueBytes, ChannelAuthorityError> {
    let raw = connection
        .query_row(
            "SELECT backlog_bytes, retained_bytes
             FROM channel_queue_bytes WHERE channel_id=?1",
            [channel_id.as_bytes().as_slice()],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?
        .ok_or(ChannelAuthorityError::CorruptRecord(
            "channel queue byte bookkeeping row is missing",
        ))?;
    Ok(QueueBytes {
        backlog_bytes: decode_u64(raw.0)?,
        retained_bytes: decode_u64(raw.1)?,
    })
}

fn max_live_sequence(
    connection: &Connection,
    channel_id: ChannelId,
) -> Result<u64, ChannelAuthorityError> {
    let max: i64 = connection.query_row(
        "SELECT COALESCE(MAX(sequence), 0) FROM channel_queue_entries WHERE channel_id=?1",
        [channel_id.as_bytes().as_slice()],
        |row| row.get(0),
    )?;
    decode_u64(max)
}

/// Re-derives the queue aggregate state from the durable entries and
/// cross-checks it against the cursor and byte bookkeeping rows, mirroring
/// the resource consumption high-water audit.
fn verify_queue_state(
    connection: &Connection,
    channel_id: ChannelId,
) -> Result<VerifiedQueueState, ChannelAuthorityError> {
    let cursors = load_queue_cursors(connection, channel_id)?;
    if cursors.trim_high_water > cursors.consume_high_water {
        return Err(ChannelAuthorityError::CorruptRecord(
            "queue trim high-water exceeds the consume high-water",
        ));
    }
    let raw = connection.query_row(
        "SELECT COUNT(*), COALESCE(MIN(sequence), 0), COALESCE(MAX(sequence), 0),
                COALESCE(SUM(payload_bytes), 0),
                COALESCE(SUM(
                    CASE WHEN sequence > ?2 THEN payload_bytes ELSE 0 END
                ), 0)
         FROM channel_queue_entries WHERE channel_id=?1",
        params![
            channel_id.as_bytes().as_slice(),
            encode_u64(cursors.consume_high_water)?,
        ],
        |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        },
    )?;
    let count = decode_u64(raw.0)?;
    let min_live = decode_u64(raw.1)?;
    let max_live = decode_u64(raw.2)?;
    let derived_retained = decode_u64(raw.3)?;
    let derived_backlog = decode_u64(raw.4)?;
    if count > 0 {
        let first_live =
            cursors
                .trim_high_water
                .checked_add(1)
                .ok_or(ChannelAuthorityError::CorruptRecord(
                    "queue trim high-water is saturated",
                ))?;
        if min_live != first_live {
            return Err(ChannelAuthorityError::CorruptRecord(
                "queue prefix is not contiguous with the trim high-water",
            ));
        }
        if max_live < cursors.consume_high_water {
            return Err(ChannelAuthorityError::CorruptRecord(
                "consume high-water exceeds the durable queue maximum",
            ));
        }
        if count != max_live - cursors.trim_high_water {
            return Err(ChannelAuthorityError::CorruptRecord(
                "queue sequence range has gaps",
            ));
        }
    }
    let bytes = load_queue_bytes(connection, channel_id)?;
    if bytes.backlog_bytes != derived_backlog {
        return Err(ChannelAuthorityError::CorruptRecord(
            "queue backlog bookkeeping disagrees with durable entries",
        ));
    }
    if bytes.retained_bytes != derived_retained {
        return Err(ChannelAuthorityError::CorruptRecord(
            "queue retained bookkeeping disagrees with durable entries",
        ));
    }
    if bytes.backlog_bytes > bytes.retained_bytes {
        return Err(ChannelAuthorityError::CorruptRecord(
            "queue backlog exceeds retained bytes",
        ));
    }
    Ok(VerifiedQueueState {
        consume_high_water: cursors.consume_high_water,
        trim_high_water: cursors.trim_high_water,
        max_live_sequence: max_live,
        backlog_bytes: bytes.backlog_bytes,
        retained_bytes: bytes.retained_bytes,
    })
}

fn load_by_create_key(
    connection: &Connection,
    key: IdempotencyKey,
) -> Result<Option<ChannelRecord>, ChannelAuthorityError> {
    let channel_id = connection
        .query_row(
            "SELECT channel_id FROM channels WHERE create_idempotency_key=?1",
            [key.as_bytes().as_slice()],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()?;
    channel_id
        .map(|bytes| load_current_optional(connection, ChannelId::from_bytes(array16(bytes)?)))
        .transpose()
        .map(Option::flatten)
}

fn load_current_optional(
    connection: &Connection,
    channel_id: ChannelId,
) -> Result<Option<ChannelRecord>, ChannelAuthorityError> {
    let raw = connection
        .query_row(
            "SELECT c.current_generation, c.current_fencing_token,
                    g.capacity_bytes, g.policy_digest, g.idempotency_key,
                    g.created_at_ms
             FROM channels c
             JOIN channel_generations g
               ON g.channel_id=c.channel_id
              AND g.channel_generation=c.current_generation
             WHERE c.channel_id=?1",
            [channel_id.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            },
        )
        .optional()?;
    raw.map(
        |(generation, fencing_token, capacity, policy, key, created)| {
            let generation = decode_generation(generation)?;
            let record = ChannelRecord {
                channel_id,
                generation,
                fencing_token: array32(fencing_token)?,
                capacity_bytes: decode_u64(capacity)?,
                policy_digest: array32(policy)?,
                idempotency_key: IdempotencyKey::from_bytes(array16(key)?),
                created_at_ms: decode_u64(created)?,
            };
            let head_fence = load_head_fence(connection, channel_id)?;
            if head_fence != record.fencing_token {
                return Err(ChannelAuthorityError::CorruptRecord(
                    "channel head fence disagrees with current generation",
                ));
            }
            validate_capacity(record.capacity_bytes)?;
            Ok(record)
        },
    )
    .transpose()
}

fn load_head_fence(
    connection: &Connection,
    channel_id: ChannelId,
) -> Result<FencingToken, ChannelAuthorityError> {
    let bytes = connection.query_row(
        "SELECT current_fencing_token FROM channels WHERE channel_id=?1",
        [channel_id.as_bytes().as_slice()],
        |row| row.get::<_, Vec<u8>>(0),
    )?;
    array32(bytes)
}

fn load_generation(
    connection: &Connection,
    channel_id: ChannelId,
    generation: Generation,
) -> Result<ChannelRecord, ChannelAuthorityError> {
    let raw = connection
        .query_row(
            "SELECT fencing_token, capacity_bytes, policy_digest,
                    idempotency_key, created_at_ms
             FROM channel_generations
             WHERE channel_id=?1 AND channel_generation=?2",
            params![
                channel_id.as_bytes().as_slice(),
                encode_generation(generation)?,
            ],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()?
        .ok_or(ChannelAuthorityError::CorruptRecord(
            "rotation receipt references absent channel generation",
        ))?;
    Ok(ChannelRecord {
        channel_id,
        generation,
        fencing_token: array32(raw.0)?,
        capacity_bytes: decode_u64(raw.1)?,
        policy_digest: array32(raw.2)?,
        idempotency_key: IdempotencyKey::from_bytes(array16(raw.3)?),
        created_at_ms: decode_u64(raw.4)?,
    })
}

struct RotationReplay {
    channel_id: ChannelId,
    expected_generation: Generation,
    expected_fencing_token: FencingToken,
    resulting_generation: Generation,
    resulting_fencing_token: FencingToken,
}

fn load_rotation_by_key(
    connection: &Connection,
    key: IdempotencyKey,
) -> Result<Option<RotationReplay>, ChannelAuthorityError> {
    let raw = connection
        .query_row(
            "SELECT channel_id, expected_generation, expected_fencing_token,
                    resulting_generation, resulting_fencing_token
             FROM channel_rotations WHERE idempotency_key=?1",
            [key.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                ))
            },
        )
        .optional()?;
    raw.map(
        |(
            channel_id,
            expected_generation,
            expected_fence,
            resulting_generation,
            resulting_fence,
        )| {
            Ok(RotationReplay {
                channel_id: ChannelId::from_bytes(array16(channel_id)?),
                expected_generation: decode_generation(expected_generation)?,
                expected_fencing_token: array32(expected_fence)?,
                resulting_generation: decode_generation(resulting_generation)?,
                resulting_fencing_token: array32(resulting_fence)?,
            })
        },
    )
    .transpose()
}

fn validate_capacity(capacity_bytes: u64) -> Result<(), ChannelAuthorityError> {
    if capacity_bytes < MIN_CAPACITY_BYTES {
        return Err(ChannelAuthorityError::InvalidCapacity);
    }
    Ok(())
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

fn encode_generation(generation: Generation) -> Result<i64, ChannelAuthorityError> {
    encode_u64(generation.get())
}

fn encode_u64(value: u64) -> Result<i64, ChannelAuthorityError> {
    i64::try_from(value).map_err(|_| ChannelAuthorityError::CorruptRecord("u64 exceeds SQLite i64"))
}

fn decode_u64(value: i64) -> Result<u64, ChannelAuthorityError> {
    u64::try_from(value).map_err(|_| ChannelAuthorityError::CorruptRecord("negative integer"))
}

fn decode_generation(value: i64) -> Result<Generation, ChannelAuthorityError> {
    let value = decode_u64(value)?;
    NonZeroU64::new(value)
        .map(Generation::new)
        .ok_or(ChannelAuthorityError::CorruptRecord("zero generation"))
}

fn array16(bytes: Vec<u8>) -> Result<[u8; 16], ChannelAuthorityError> {
    bytes
        .try_into()
        .map_err(|_| ChannelAuthorityError::CorruptRecord("identity length is not 16"))
}

fn array32(bytes: Vec<u8>) -> Result<[u8; 32], ChannelAuthorityError> {
    bytes
        .try_into()
        .map_err(|_| ChannelAuthorityError::CorruptRecord("digest length is not 32"))
}
