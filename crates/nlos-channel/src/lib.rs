//! Durable local Channel endpoint authority.
//!
//! This Stage-B slice owns the stable Channel identity, generation fence and
//! authority-derived participant proof required before a Task-scoped handle
//! can be registered.  It deliberately does not implement queue delivery,
//! Topic routing, fanout, payer accounting, or `TaskWriteSet` integration.

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
            0 => schema::migrate_v1(&mut connection)?,
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
