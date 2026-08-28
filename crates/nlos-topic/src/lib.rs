//! Durable local Topic service-layer authority (fanout prefix).
//!
//! This Stage-B slice implements the Topic side of `[LAYER-SVC-001]`: Topic is
//! a first-class system service built on top of the Channel endpoint
//! authority, which stays the single message-log source of truth.  The Topic
//! authority owns a separate `SQLite` database with the durable topic head
//! (RSM-FANOUT-001 policy: `max_recipients`, `delivery_attempts`,
//! `cascade_depth`, `retained_bytes`/`retention_ms` declarations, a non-empty
//! payer typed binding and the idempotency scope), authority-derived
//! [`TopicId`]/[`SubscriptionId`] identities, per-subscriber cursors, and the
//! immutable publication journal that binds every publish to its policy
//! digest, payer, cascade budget and channel sequence association.
//!
//! Cross-authority semantics follow a verify-then-commit discipline: a publish
//! first persists a `PENDING_ENQUEUE` publication row, then enqueues through
//! [`ChannelAuthority::enqueue`], then advances the row to `ENQUEUED`.  A
//! crash between those steps is repaired by replaying the same idempotency
//! key: the channel's key-scoped replay converges without a duplicate
//! enqueue.  The slice deliberately does not claim cross-authority atomicity.
//!
//! It deliberately does not implement: cascade republish budget consumption
//! (the budget is initialized and persisted only), delivery-attempt execution,
//! real payer metering, interest/matching predicates, cross-process access,
//! wakeup wiring, or `TaskWriteSet` integration.

mod schema;

use std::error::Error;
use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use nlos_channel::{
    ChannelAuthority, ChannelAuthorityError, ChannelRecord,
    CompactDecision as ChannelCompactDecision, CompactReceipt as ChannelCompactReceipt,
    EnqueueDecision, EnqueueRequest, FencingToken, QueueEntryRecord,
};
use nlos_types::{ChannelId, Generation, IdempotencyKey, ResourceAccountId};
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

nominal_id!(TopicId);
nominal_id!(SubscriptionId);
nominal_id!(SubscriberKey);

/// The RSM-FANOUT-001 policy bound to a topic before the first enqueue.
///
/// Every field is a durable declaration; `payer` is an opaque typed binding
/// that must not be the all-zero (unbound) account.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TopicPolicy {
    pub max_recipients: u64,
    pub delivery_attempts: u64,
    pub cascade_depth: u64,
    pub retained_bytes: u64,
    pub retention_ms: u64,
    pub payer: ResourceAccountId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreateTopicRequest {
    pub channel_id: ChannelId,
    pub name: Vec<u8>,
    pub policy: TopicPolicy,
    pub idempotency_key: IdempotencyKey,
    pub created_at_ms: u64,
}

/// The durable topic head, including the channel generation/fence snapshot
/// bound at creation time (the fence binding the publish path enqueues
/// against; see [`TopicAuthority::publish`]).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TopicRecord {
    pub topic_id: TopicId,
    pub channel_id: ChannelId,
    pub name: Vec<u8>,
    pub channel_generation: Generation,
    pub channel_fencing_token: FencingToken,
    pub policy: TopicPolicy,
    pub policy_digest: [u8; 32],
    pub idempotency_key: IdempotencyKey,
    pub active_subscriptions: u64,
    pub created_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TopicDecision {
    Created(TopicRecord),
    Replayed(TopicRecord),
}

impl TopicDecision {
    #[must_use]
    pub fn record(self) -> TopicRecord {
        match self {
            Self::Created(record) | Self::Replayed(record) => record,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubscribeRequest {
    pub topic_id: TopicId,
    pub subscriber_key: SubscriberKey,
    pub subscribed_at_ms: u64,
}

/// The durable per-subscriber state row.
///
/// `cursor` is the subscriber's own consume point (initialized to the
/// subscribe point; history before it is never replayed), `unsubscribed_at_ms`
/// records the most recent unsubscribe (0 if never), and
/// `last_advanced_at_ms` carries the timestamp of the last accepted
/// [`TopicAuthority::advance`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubscriptionRecord {
    pub subscription_id: SubscriptionId,
    pub topic_id: TopicId,
    pub subscriber_key: SubscriberKey,
    pub active: bool,
    pub cursor: u64,
    pub subscribed_at_ms: u64,
    pub unsubscribed_at_ms: u64,
    pub last_advanced_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubscribeDecision {
    Subscribed(SubscriptionRecord),
    Replayed(SubscriptionRecord),
}

impl SubscribeDecision {
    #[must_use]
    pub fn record(self) -> SubscriptionRecord {
        match self {
            Self::Subscribed(record) | Self::Replayed(record) => record,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnsubscribeRequest {
    pub topic_id: TopicId,
    pub subscriber_key: SubscriberKey,
    pub unsubscribed_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct UnsubscribeReceipt {
    pub subscription_id: SubscriptionId,
    pub topic_id: TopicId,
    pub subscriber_key: SubscriberKey,
    pub unsubscribed_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnsubscribeDecision {
    Unsubscribed(UnsubscribeReceipt),
    Replayed(UnsubscribeReceipt),
}

impl UnsubscribeDecision {
    #[must_use]
    pub const fn receipt(self) -> UnsubscribeReceipt {
        match self {
            Self::Unsubscribed(receipt) | Self::Replayed(receipt) => receipt,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublishRequest {
    pub topic_id: TopicId,
    pub payload: Vec<u8>,
    pub idempotency_key: IdempotencyKey,
    pub published_at_ms: u64,
}

/// Whether a publication has completed its channel enqueue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublicationStatus {
    /// The durable publication row exists but the channel enqueue has not
    /// been observed to complete (in-flight or crashed window).
    PendingEnqueue,
    /// The channel enqueue completed; `channel_sequence` is bound.
    Enqueued,
}

/// One immutable publication binding plus its enqueue commit state.
///
/// The topic authority never stores the payload body; `payload_digest` binds
/// the enqueued bytes for replay/conflict detection, and the payer/policy
/// digest/cascade budget mirror the topic head at first-publish time.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublicationRecord {
    pub topic_id: TopicId,
    pub idempotency_key: IdempotencyKey,
    pub policy_digest: [u8; 32],
    pub payer: ResourceAccountId,
    pub payload_digest: [u8; 32],
    pub status: PublicationStatus,
    /// Channel sequence of the enqueued entry; 0 while pending.
    pub channel_sequence: u64,
    /// Channel generation of the enqueued entry; 0 while pending.
    pub channel_generation: u64,
    pub cascade_budget_remaining: u64,
    pub published_at_ms: u64,
    pub enqueued_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PublishDecision {
    /// This call wrote (or resumed and completed) the publication's enqueue.
    Published(PublicationRecord),
    /// The exact request had already completed; the original record returns
    /// and nothing is enqueued again.
    Replayed(PublicationRecord),
}

impl PublishDecision {
    #[must_use]
    pub fn record(self) -> PublicationRecord {
        match self {
            Self::Published(record) | Self::Replayed(record) => record,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdvanceRequest {
    pub topic_id: TopicId,
    pub subscriber_key: SubscriberKey,
    pub up_to_sequence: u64,
    pub advanced_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AdvanceReceipt {
    pub subscription_id: SubscriptionId,
    pub topic_id: TopicId,
    pub subscriber_key: SubscriberKey,
    pub cursor: u64,
    pub advanced_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdvanceDecision {
    Advanced(AdvanceReceipt),
    Replayed(AdvanceReceipt),
}

impl AdvanceDecision {
    #[must_use]
    pub const fn receipt(self) -> AdvanceReceipt {
        match self {
            Self::Advanced(receipt) | Self::Replayed(receipt) => receipt,
        }
    }
}

/// The service-layer compaction decision wrapping the channel receipt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TopicCompactReceipt {
    pub topic_id: TopicId,
    pub channel_id: ChannelId,
    /// The effective watermark actually requested from the channel:
    /// `min(trim_to_sequence, compact_bound)`.
    pub effective_trim_high_water: u64,
    pub channel: ChannelCompactReceipt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TopicCompactDecision {
    Trimmed(TopicCompactReceipt),
    Replayed(TopicCompactReceipt),
}

impl TopicCompactDecision {
    #[must_use]
    pub const fn receipt(self) -> TopicCompactReceipt {
        match self {
            Self::Trimmed(receipt) | Self::Replayed(receipt) => receipt,
        }
    }
}

#[derive(Debug)]
pub enum TopicAuthorityError {
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
    DurabilityUnavailable {
        journal_mode: String,
        synchronous: i64,
    },
    SchemaVersionUnsupported(i64),
    /// A typed rejection from the Channel authority (for example
    /// [`ChannelAuthorityError::StaleChannel`] or
    /// [`ChannelAuthorityError::QueueFull`]), propagated without silent
    /// retry.
    Channel(ChannelAuthorityError),
    TopicNotFound(TopicId),
    SubscriptionNotFound(SubscriptionId),
    SubscriptionInactive(SubscriptionId),
    InvalidPolicy(&'static str),
    SubscriberLimitReached,
    IdempotencyConflict,
    InvalidPayload,
    InvalidSequence(&'static str),
    CorruptRecord(&'static str),
    LockPoisoned,
}

impl fmt::Display for TopicAuthorityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(formatter, "SQLite topic authority failure: {error}"),
            Self::Io(error) => write!(formatter, "topic authority I/O failure: {error}"),
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
                    "unsupported topic authority schema version {version}"
                )
            }
            Self::Channel(error) => {
                write!(
                    formatter,
                    "channel authority rejected topic operation: {error}"
                )
            }
            Self::TopicNotFound(id) => write!(formatter, "topic {id:?} does not exist"),
            Self::SubscriptionNotFound(id) => {
                write!(formatter, "subscription {id:?} does not exist")
            }
            Self::SubscriptionInactive(id) => write!(
                formatter,
                "subscription {id:?} is inactive and must re-subscribe first"
            ),
            Self::InvalidPolicy(reason) => {
                write!(formatter, "invalid topic policy: {reason}")
            }
            Self::SubscriberLimitReached => {
                formatter.write_str("topic active subscriptions reached max_recipients")
            }
            Self::IdempotencyConflict => formatter.write_str(
                "idempotency key or authority-assigned identity was rebound to different input",
            ),
            Self::InvalidPayload => formatter.write_str("published payload must be non-empty"),
            Self::InvalidSequence(reason) => {
                write!(formatter, "invalid subscriber cursor sequence: {reason}")
            }
            Self::CorruptRecord(reason) => write!(formatter, "corrupt topic record: {reason}"),
            Self::LockPoisoned => formatter.write_str("topic authority writer lock is poisoned"),
        }
    }
}

impl Error for TopicAuthorityError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Channel(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for TopicAuthorityError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

/// A single-node Topic service owner with WAL/FULL durable topic,
/// subscription and publication records, delegating the message log to the
/// [`ChannelAuthority`].
pub struct TopicAuthority {
    channel: Arc<ChannelAuthority>,
    connection: Mutex<Connection>,
}

impl TopicAuthority {
    /// Opens or creates `<root>/topic-authority.db` bound to the given
    /// Channel authority.
    ///
    /// # Errors
    ///
    /// Fails closed when `SQLite` cannot provide WAL/FULL durability or when a
    /// stored schema version is unknown.
    pub fn open(
        root: impl AsRef<Path>,
        channel: Arc<ChannelAuthority>,
    ) -> Result<Self, TopicAuthorityError> {
        std::fs::create_dir_all(root.as_ref()).map_err(TopicAuthorityError::Io)?;
        let mut connection = Connection::open(root.as_ref().join("topic-authority.db"))?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;

        let journal_mode: String =
            connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        let synchronous: i64 =
            connection.pragma_query_value(None, "synchronous", |row| row.get(0))?;
        if !journal_mode.eq_ignore_ascii_case("wal") || synchronous != 2 {
            return Err(TopicAuthorityError::DurabilityUnavailable {
                journal_mode,
                synchronous,
            });
        }

        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        match version {
            0 => schema::migrate_v1(&mut connection)?,
            schema::SCHEMA_VERSION => {}
            other => return Err(TopicAuthorityError::SchemaVersionUnsupported(other)),
        }
        Ok(Self {
            channel,
            connection: Mutex::new(connection),
        })
    }

    /// Creates a durable topic head bound to an existing Channel.
    ///
    /// The Channel's existence and its current generation/fence snapshot are
    /// verified through the owner readback [`ChannelAuthority::inspect_channel`]
    /// before any durable write; the snapshot is then bound into the topic
    /// head and is the fence the publish path enqueues against (so staleness
    /// after a rotation surfaces as a propagated
    /// [`ChannelAuthorityError::StaleChannel`] instead of being silently
    /// absorbed).
    ///
    /// The [`TopicId`] is authority-derived from the channel id and the topic
    /// name; it is never a caller field.  Repeating the exact request returns
    /// the original record (the generation/fence snapshot is authority state
    /// and is not compared on replay); rebinding the key to a different
    /// channel, name or policy is an
    /// [`TopicAuthorityError::IdempotencyConflict`].
    ///
    /// # Errors
    ///
    /// Fails closed for an invalid policy (out-of-range declarations, empty
    /// name, empty payer binding), an unknown Channel, idempotency rebinding,
    /// or a storage/corruption failure.  Rejections leave zero durable state.
    pub fn create_topic(
        &self,
        request: CreateTopicRequest,
    ) -> Result<TopicDecision, TopicAuthorityError> {
        validate_name(&request.name)?;
        validate_policy(&request.policy)?;
        // Owner readback: the Channel and its current fence must be durable
        // before the topic head references them.
        let channel_head = self
            .channel
            .inspect_channel(request.channel_id)
            .map_err(TopicAuthorityError::Channel)?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = load_topic_by_create_key(&transaction, request.idempotency_key)? {
            if existing.channel_id != request.channel_id
                || existing.name != request.name
                || existing.policy != request.policy
            {
                return Err(TopicAuthorityError::IdempotencyConflict);
            }
            transaction.commit()?;
            return Ok(TopicDecision::Replayed(existing));
        }

        let topic_id = topic_id_for(request.channel_id, &request.name);
        if load_topic_optional(&transaction, topic_id)?.is_some() {
            return Err(TopicAuthorityError::IdempotencyConflict);
        }
        let record = TopicRecord {
            topic_id,
            channel_id: request.channel_id,
            name: request.name,
            channel_generation: channel_head.generation,
            channel_fencing_token: channel_head.fencing_token,
            policy: request.policy,
            policy_digest: derive_policy_digest(&request.policy),
            idempotency_key: request.idempotency_key,
            active_subscriptions: 0,
            created_at_ms: request.created_at_ms,
        };
        insert_topic(&transaction, &record)?;
        transaction.commit()?;
        Ok(TopicDecision::Created(record))
    }

    /// Reads the verified topic head.
    ///
    /// The stored policy digest and authority-derived identity are
    /// re-derived and the active-subscription counter is cross-checked
    /// against the subscription rows; any disagreement is
    /// [`TopicAuthorityError::CorruptRecord`].
    ///
    /// # Errors
    ///
    /// Fails closed for an unknown Topic or a corrupt record.
    pub fn inspect_topic(&self, topic_id: TopicId) -> Result<TopicRecord, TopicAuthorityError> {
        let connection = self.lock()?;
        load_topic_verified(&connection, topic_id)
    }

    /// Subscribes a caller-provided key to a topic.
    ///
    /// The [`SubscriptionId`] is authority-derived from the topic and the
    /// subscriber key.  Admission requires the active subscription count to
    /// stay below `max_recipients`
    /// ([`TopicAuthorityError::SubscriberLimitReached`] otherwise).  The
    /// subscriber cursor is initialized to the Channel's current durable
    /// sequence high-water: publications before the subscribe point are never
    /// replayed to the new subscriber.  Subscribing an already-active key is
    /// idempotent; subscribing a previously unsubscribed key re-activates it
    /// with a fresh subscribe point.
    ///
    /// # Errors
    ///
    /// Fails closed for an unknown Topic, the recipient limit, a Channel
    /// readback failure, or a storage/corruption failure.
    pub fn subscribe(
        &self,
        request: SubscribeRequest,
    ) -> Result<SubscribeDecision, TopicAuthorityError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let topic = load_topic_verified(&transaction, request.topic_id)?;
        if let Some(existing) =
            load_subscription_optional(&transaction, request.topic_id, request.subscriber_key)?
            && existing.active
        {
            transaction.commit()?;
            return Ok(SubscribeDecision::Replayed(existing));
        }
        let live = self
            .channel
            .inspect_queue(topic.channel_id)
            .map_err(TopicAuthorityError::Channel)?;
        let active = count_active_subscriptions(&transaction, request.topic_id)?;
        if active >= topic.policy.max_recipients {
            return Err(TopicAuthorityError::SubscriberLimitReached);
        }
        let record = SubscriptionRecord {
            subscription_id: subscription_id_for(request.topic_id, request.subscriber_key),
            topic_id: request.topic_id,
            subscriber_key: request.subscriber_key,
            active: true,
            cursor: live.max_sequence,
            subscribed_at_ms: request.subscribed_at_ms,
            unsubscribed_at_ms: 0,
            last_advanced_at_ms: 0,
        };
        insert_or_resubscribe(&transaction, &record)?;
        bump_active_count(&transaction, request.topic_id, active, active + 1)?;
        transaction.commit()?;
        Ok(SubscribeDecision::Subscribed(record))
    }

    /// Flips a subscription to inactive.
    ///
    /// The state-row flip is paired with the topic's active-subscription
    /// counter decrement in one `Immediate` transaction.  Inactive
    /// subscriptions are excluded from the min-live-cursor compaction bound.
    /// Repeating the unsubscribe replays the original receipt.
    ///
    /// # Errors
    ///
    /// Fails closed for an unknown Topic or subscription, or a
    /// storage/corruption failure.
    pub fn unsubscribe(
        &self,
        request: UnsubscribeRequest,
    ) -> Result<UnsubscribeDecision, TopicAuthorityError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let topic = load_topic_verified(&transaction, request.topic_id)?;
        let existing =
            load_subscription_optional(&transaction, request.topic_id, request.subscriber_key)?
                .ok_or(TopicAuthorityError::SubscriptionNotFound(
                    subscription_id_for(request.topic_id, request.subscriber_key),
                ))?;
        if !existing.active {
            transaction.commit()?;
            return Ok(UnsubscribeDecision::Replayed(UnsubscribeReceipt {
                subscription_id: existing.subscription_id,
                topic_id: existing.topic_id,
                subscriber_key: existing.subscriber_key,
                unsubscribed_at_ms: existing.unsubscribed_at_ms,
            }));
        }
        let active = count_active_subscriptions(&transaction, request.topic_id)?;
        let changed = transaction.execute(
            "UPDATE topic_subscriptions
             SET active=0, unsubscribed_at_ms=?1
             WHERE subscription_id=?2 AND active=1",
            params![
                encode_u64(request.unsubscribed_at_ms)?,
                existing.subscription_id.as_bytes().as_slice(),
            ],
        )?;
        if changed != 1 {
            return Err(TopicAuthorityError::CorruptRecord(
                "subscription active-bit CAS lost",
            ));
        }
        bump_active_count(&transaction, topic.topic_id, active, active - 1)?;
        transaction.commit()?;
        Ok(UnsubscribeDecision::Unsubscribed(UnsubscribeReceipt {
            subscription_id: existing.subscription_id,
            topic_id: existing.topic_id,
            subscriber_key: existing.subscriber_key,
            unsubscribed_at_ms: request.unsubscribed_at_ms,
        }))
    }

    /// Reads the verified subscription state row.
    ///
    /// The stored [`SubscriptionId`] is re-derived from the topic and
    /// subscriber key; a disagreement is
    /// [`TopicAuthorityError::CorruptRecord`].
    ///
    /// # Errors
    ///
    /// Fails closed for an unknown Topic or subscription, or a corrupt record.
    pub fn inspect_subscription(
        &self,
        topic_id: TopicId,
        subscriber_key: SubscriberKey,
    ) -> Result<SubscriptionRecord, TopicAuthorityError> {
        let connection = self.lock()?;
        load_topic_verified(&connection, topic_id)?;
        load_subscription_optional(&connection, topic_id, subscriber_key)?.ok_or(
            TopicAuthorityError::SubscriptionNotFound(subscription_id_for(
                topic_id,
                subscriber_key,
            )),
        )
    }

    /// Publishes one payload: verify-then-commit across the two authorities.
    ///
    /// 1. The immutable publication row is persisted first with status
    ///    `PENDING_ENQUEUE`, binding the topic's policy digest, payer,
    ///    payload digest and the initialized cascade budget (the Topic
    ///    authority never stores the payload body; the Channel authority is
    ///    the only message log).
    /// 2. [`ChannelAuthority::enqueue`] is called with the fence bound on the
    ///    topic head at creation time, so a Channel rotation after
    ///    `create_topic` surfaces as a propagated
    ///    [`ChannelAuthorityError::StaleChannel`] — there is no silent
    ///    auto-retry inside one call.
    /// 3. On success the row commits to `ENQUEUED` with the channel sequence
    ///    association.
    ///
    /// Crash window: a row left `PENDING_ENQUEUE` (crash or a propagated
    /// enqueue failure) is converged by replaying the same idempotency key
    /// and payload: the replay re-reads the Channel head live
    /// ([`ChannelAuthority::inspect_channel`]), re-binds the topic head fence
    /// and re-issues the enqueue.  Because the Channel checks its key-scoped
    /// idempotency before the fence, this converges without a duplicate
    /// enqueue when the original attempt had completed; if the Channel
    /// rotated in between, the replay fails closed with
    /// [`ChannelAuthorityError::IdempotencyConflict`].  Replaying a key that
    /// already reached `ENQUEUED` returns the original record and does not
    /// enqueue again; a key rebound to a different topic or payload is an
    /// [`TopicAuthorityError::IdempotencyConflict`].  This is verify-then-
    /// commit semantics; no cross-authority atomicity is claimed.
    ///
    /// # Errors
    ///
    /// Fails closed for an empty payload, an unknown Topic, idempotency
    /// rebinding, a propagated Channel rejection (`StaleChannel`,
    /// `QueueFull`, ...) or a storage/corruption failure.  The publication
    /// row stays `PENDING_ENQUEUE` when the enqueue itself is rejected.
    #[allow(clippy::too_many_lines)] // One method owns the verify-then-commit publish sequence.
    pub fn publish(&self, request: PublishRequest) -> Result<PublishDecision, TopicAuthorityError> {
        if request.payload.is_empty() {
            return Err(TopicAuthorityError::InvalidPayload);
        }
        let digest = derive_payload_digest(&request.payload);

        // Step 1: durable PENDING row first (or replay short-circuit).
        let (topic, resumed) = {
            let mut connection = self.lock()?;
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let topic = load_topic_verified(&transaction, request.topic_id)?;
            if let Some(existing) = load_publication_by_key(&transaction, request.idempotency_key)?
            {
                if existing.topic_id != request.topic_id || existing.payload_digest != digest {
                    return Err(TopicAuthorityError::IdempotencyConflict);
                }
                if existing.status == PublicationStatus::Enqueued {
                    transaction.commit()?;
                    return Ok(PublishDecision::Replayed(existing));
                }
                transaction.commit()?;
                (topic, true)
            } else {
                insert_publication(
                    &transaction,
                    &topic,
                    request.idempotency_key,
                    digest,
                    request.published_at_ms,
                )?;
                transaction.commit()?;
                (topic, false)
            }
        };

        // Step 2: fence acquisition.  A fresh publish enqueues against the
        // fence bound on the topic head so a rotation is observable as
        // StaleChannel; a resumed PENDING publication re-reads the Channel
        // head live and re-binds before re-issuing the enqueue (the
        // documented re-read convergence after a propagated failure).
        let (expected_generation, expected_fencing_token) = if resumed {
            let live = self
                .channel
                .inspect_channel(topic.channel_id)
                .map_err(TopicAuthorityError::Channel)?;
            self.rebind_channel_fence(&topic, &live)?;
            (live.generation, live.fencing_token)
        } else {
            (topic.channel_generation, topic.channel_fencing_token)
        };

        // Step 3: enqueue; typed rejections propagate without silent retry.
        // A `Replayed` channel decision is the crash window: a prior attempt
        // already enqueued this key, so its record is the association to keep.
        let entry = match self
            .channel
            .enqueue(EnqueueRequest {
                channel_id: topic.channel_id,
                expected_generation,
                expected_fencing_token,
                payload: request.payload,
                idempotency_key: request.idempotency_key,
                enqueued_at_ms: request.published_at_ms,
            })
            .map_err(TopicAuthorityError::Channel)?
        {
            EnqueueDecision::Enqueued(entry) | EnqueueDecision::Replayed(entry) => entry,
        };

        // Step 4: commit the sequence association.
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE topic_publications
             SET status=1, channel_sequence=?1, channel_generation=?2, enqueued_at_ms=?3
             WHERE idempotency_key=?4 AND status=0",
            params![
                encode_u64(entry.sequence)?,
                encode_u64(entry.generation.get())?,
                encode_u64(entry.enqueued_at_ms)?,
                request.idempotency_key.as_bytes().as_slice(),
            ],
        )?;
        if changed != 1 {
            // A concurrent call completed the same key first; converge to
            // its record or fail closed on disagreement.
            let existing = load_publication_by_key(&transaction, request.idempotency_key)?.ok_or(
                TopicAuthorityError::CorruptRecord(
                    "publication row vanished during enqueue commit",
                ),
            )?;
            if existing.status != PublicationStatus::Enqueued
                || existing.channel_sequence != entry.sequence
            {
                return Err(TopicAuthorityError::CorruptRecord(
                    "publication enqueue commit disagrees with the channel entry",
                ));
            }
            transaction.commit()?;
            return Ok(PublishDecision::Replayed(existing));
        }
        let record = load_publication_by_key(&transaction, request.idempotency_key)?.ok_or(
            TopicAuthorityError::CorruptRecord("publication row vanished after enqueue commit"),
        )?;
        transaction.commit()?;
        Ok(PublishDecision::Published(record))
    }

    /// Returns the entries this subscriber has not consumed yet.
    ///
    /// Zero-write: the Channel receive window (`sequence >
    /// consume_high_water`, ordered, limited to `limit`) is filtered by the
    /// subscriber's own cursor (`sequence > cursor`) and no cursor moves.  The
    /// result may be shorter than `limit` when the shared channel-level
    /// consume high-water or the personal cursor already covered the window.
    /// A slow subscriber's lag never hides entries from another subscriber
    /// and vice versa; only the shared channel consume high-water (advanced
    /// by the channel owner, never by this service) bounds everyone.
    ///
    /// # Errors
    ///
    /// Fails closed for an unknown Topic or subscription, an inactive
    /// subscription, a cursor inconsistent with the channel high-water, or a
    /// propagated Channel rejection.
    pub fn poll(
        &self,
        topic_id: TopicId,
        subscriber_key: SubscriberKey,
        limit: usize,
    ) -> Result<Vec<QueueEntryRecord>, TopicAuthorityError> {
        let connection = self.lock()?;
        let topic = load_topic_verified(&connection, topic_id)?;
        let subscription = load_subscription_optional(&connection, topic_id, subscriber_key)?
            .ok_or(TopicAuthorityError::SubscriptionNotFound(
                subscription_id_for(topic_id, subscriber_key),
            ))?;
        if !subscription.active {
            return Err(TopicAuthorityError::SubscriptionInactive(
                subscription.subscription_id,
            ));
        }
        let queue = self
            .channel
            .inspect_queue(topic.channel_id)
            .map_err(TopicAuthorityError::Channel)?;
        if subscription.cursor > queue.max_sequence {
            return Err(TopicAuthorityError::CorruptRecord(
                "subscriber cursor exceeds the channel sequence high-water",
            ));
        }
        let window = self
            .channel
            .receive(topic.channel_id, limit)
            .map_err(TopicAuthorityError::Channel)?;
        Ok(window
            .into_iter()
            .filter(|entry| entry.sequence > subscription.cursor)
            .collect())
    }

    /// Advances one subscriber's cursor, monotonically.
    ///
    /// Mirrors the channel ack semantics: repeating the exact current cursor
    /// replays the original decision (with its stored timestamp); a lower
    /// sequence, or one beyond the channel's durable sequence high-water, is
    /// [`TopicAuthorityError::InvalidSequence`] before any write.  The
    /// advance is a per-subscriber CAS and never moves another subscriber's
    /// cursor or any channel cursor.
    ///
    /// # Errors
    ///
    /// Fails closed for an unknown Topic or subscription, an inactive
    /// subscription, a regressing or out-of-range sequence, or a
    /// storage/corruption failure.
    pub fn advance(&self, request: AdvanceRequest) -> Result<AdvanceDecision, TopicAuthorityError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let topic = load_topic_verified(&transaction, request.topic_id)?;
        let subscription =
            load_subscription_optional(&transaction, request.topic_id, request.subscriber_key)?
                .ok_or(TopicAuthorityError::SubscriptionNotFound(
                    subscription_id_for(request.topic_id, request.subscriber_key),
                ))?;
        if !subscription.active {
            return Err(TopicAuthorityError::SubscriptionInactive(
                subscription.subscription_id,
            ));
        }
        let queue = self
            .channel
            .inspect_queue(topic.channel_id)
            .map_err(TopicAuthorityError::Channel)?;
        if subscription.cursor > queue.max_sequence {
            return Err(TopicAuthorityError::CorruptRecord(
                "subscriber cursor exceeds the channel sequence high-water",
            ));
        }
        if request.up_to_sequence == subscription.cursor {
            let receipt = AdvanceReceipt {
                subscription_id: subscription.subscription_id,
                topic_id: request.topic_id,
                subscriber_key: request.subscriber_key,
                cursor: subscription.cursor,
                advanced_at_ms: subscription.last_advanced_at_ms,
            };
            transaction.commit()?;
            return Ok(AdvanceDecision::Replayed(receipt));
        }
        if request.up_to_sequence < subscription.cursor {
            return Err(TopicAuthorityError::InvalidSequence(
                "subscriber cursor advance regresses below the consume point",
            ));
        }
        if request.up_to_sequence > queue.max_sequence {
            return Err(TopicAuthorityError::InvalidSequence(
                "subscriber cursor advance exceeds the channel sequence high-water",
            ));
        }
        let changed = transaction.execute(
            "UPDATE topic_subscriptions
             SET cursor=?1, last_advanced_at_ms=?2
             WHERE subscription_id=?3 AND cursor=?4",
            params![
                encode_u64(request.up_to_sequence)?,
                encode_u64(request.advanced_at_ms)?,
                subscription.subscription_id.as_bytes().as_slice(),
                encode_u64(subscription.cursor)?,
            ],
        )?;
        if changed != 1 {
            return Err(TopicAuthorityError::CorruptRecord(
                "subscriber cursor CAS lost",
            ));
        }
        transaction.commit()?;
        Ok(AdvanceDecision::Advanced(AdvanceReceipt {
            subscription_id: subscription.subscription_id,
            topic_id: request.topic_id,
            subscriber_key: request.subscriber_key,
            cursor: request.up_to_sequence,
            advanced_at_ms: request.advanced_at_ms,
        }))
    }

    /// Computes the service-layer trim bound for a topic:
    /// `min(min-active-subscriber cursor, channel consume high-water)`.
    ///
    /// With no active subscribers the bound is the channel consume high-water
    /// alone (the retention declarations are durable policy but are not
    /// enforced by this slice).  The bound only consults this topic's
    /// subscribers; channels hosting several topics need a cross-topic
    /// aggregation before trimming (a known limitation of this slice).
    ///
    /// # Errors
    ///
    /// Fails closed for an unknown Topic, corrupt state, or a propagated
    /// Channel rejection.
    pub fn compact_bound(&self, topic_id: TopicId) -> Result<u64, TopicAuthorityError> {
        let connection = self.lock()?;
        let topic = load_topic_verified(&connection, topic_id)?;
        // `MIN(cursor)` is SQL NULL when the topic has rows but none active,
        // and the query returns no row at all when it has no rows: both mean
        // "no live subscriber" and fall back to the consume high-water.
        let min_live: Option<Option<i64>> = connection
            .query_row(
                "SELECT MIN(cursor) FROM topic_subscriptions WHERE topic_id=?1 AND active=1",
                [topic_id.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .optional()?;
        let queue = self
            .channel
            .inspect_queue(topic.channel_id)
            .map_err(TopicAuthorityError::Channel)?;
        Ok(min_live
            .flatten()
            .map(decode_u64)
            .transpose()?
            .map_or(queue.consume_high_water, |cursor| {
                cursor.min(queue.consume_high_water)
            }))
    }

    /// Trims the channel log to `min(trim_to_sequence, compact_bound)`.
    ///
    /// The effective watermark is delegated to
    /// [`ChannelAuthority::compact`], whose own clamping (never past the
    /// consume high-water) and replay/regression semantics are preserved and
    /// returned in the wrapped receipt.
    ///
    /// # Errors
    ///
    /// Fails closed for an unknown Topic, corrupt state, or a propagated
    /// Channel rejection (including a regressing effective watermark raised
    /// by the channel).
    pub fn compact(
        &self,
        topic_id: TopicId,
        trim_to_sequence: u64,
    ) -> Result<TopicCompactDecision, TopicAuthorityError> {
        let channel_id = {
            let connection = self.lock()?;
            load_topic_verified(&connection, topic_id)?.channel_id
        };
        let bound = self.compact_bound(topic_id)?;
        let target = trim_to_sequence.min(bound);
        let receipt = match self
            .channel
            .compact(channel_id, target)
            .map_err(TopicAuthorityError::Channel)?
        {
            ChannelCompactDecision::Trimmed(receipt) => {
                TopicCompactDecision::Trimmed(wrap_compact(topic_id, channel_id, target, receipt))
            }
            ChannelCompactDecision::Replayed(receipt) => {
                TopicCompactDecision::Replayed(wrap_compact(topic_id, channel_id, target, receipt))
            }
        };
        Ok(receipt)
    }

    /// Reads the verified publication journal of one topic in publish order.
    ///
    /// Every row must still carry the topic head's policy digest and payer
    /// binding, and every enqueued sequence must stay at or below the
    /// channel's durable sequence high-water; any disagreement is
    /// [`TopicAuthorityError::CorruptRecord`].
    ///
    /// # Errors
    ///
    /// Fails closed for an unknown Topic, corrupt rows, or a propagated
    /// Channel rejection.
    pub fn inspect_publications(
        &self,
        topic_id: TopicId,
    ) -> Result<Vec<PublicationRecord>, TopicAuthorityError> {
        let connection = self.lock()?;
        let topic = load_topic_verified(&connection, topic_id)?;
        let queue = self
            .channel
            .inspect_queue(topic.channel_id)
            .map_err(TopicAuthorityError::Channel)?;
        let mut statement = connection.prepare(
            "SELECT idempotency_key, policy_digest, payer_account_id, payload_digest,
                    status, channel_sequence, channel_generation, cascade_budget_remaining,
                    published_at_ms, enqueued_at_ms
             FROM topic_publications WHERE topic_id=?1
             ORDER BY published_at_ms, idempotency_key",
        )?;
        let rows = statement
            .query_map([topic_id.as_bytes().as_slice()], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, i64>(9)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|row| {
                let key = IdempotencyKey::from_bytes(array16(row.0)?);
                let record = decode_publication(
                    topic_id,
                    key,
                    (
                        row.1, row.2, row.3, row.4, row.5, row.6, row.7, row.8, row.9,
                    ),
                )?;
                if record.policy_digest != topic.policy_digest || record.payer != topic.policy.payer
                {
                    return Err(TopicAuthorityError::CorruptRecord(
                        "publication binding disagrees with the topic head",
                    ));
                }
                if record.status == PublicationStatus::Enqueued
                    && record.channel_sequence > queue.max_sequence
                {
                    return Err(TopicAuthorityError::CorruptRecord(
                        "publication sequence exceeds the channel high-water",
                    ));
                }
                Ok(record)
            })
            .collect()
    }

    /// Re-binds the topic head's channel fence snapshot after a live
    /// readback, tolerating a concurrent rebind to the same live value.
    fn rebind_channel_fence(
        &self,
        topic: &TopicRecord,
        live: &ChannelRecord,
    ) -> Result<(), TopicAuthorityError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE topics
             SET channel_generation=?1, channel_fencing_token=?2
             WHERE topic_id=?3 AND channel_generation=?4 AND channel_fencing_token=?5",
            params![
                encode_generation(live.generation)?,
                live.fencing_token.as_slice(),
                topic.topic_id.as_bytes().as_slice(),
                encode_generation(topic.channel_generation)?,
                topic.channel_fencing_token.as_slice(),
            ],
        )?;
        if changed != 1 {
            let current = load_topic_verified(&transaction, topic.topic_id)?;
            if current.channel_generation != live.generation
                || current.channel_fencing_token != live.fencing_token
            {
                return Err(TopicAuthorityError::CorruptRecord(
                    "topic channel fence rebind CAS lost",
                ));
            }
        }
        transaction.commit()?;
        Ok(())
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>, TopicAuthorityError> {
        self.connection
            .lock()
            .map_err(|_| TopicAuthorityError::LockPoisoned)
    }
}

fn wrap_compact(
    topic_id: TopicId,
    channel_id: ChannelId,
    target: u64,
    receipt: ChannelCompactReceipt,
) -> TopicCompactReceipt {
    TopicCompactReceipt {
        topic_id,
        channel_id,
        effective_trim_high_water: target,
        channel: receipt,
    }
}

/// Raw `topics` row without the constant `topic_id` column.
type TopicRow = (
    Vec<u8>,
    Vec<u8>,
    Vec<u8>,
    i64,
    Vec<u8>,
    i64,
    i64,
    i64,
    i64,
    i64,
    Vec<u8>,
    Vec<u8>,
    i64,
    i64,
);

/// Raw `topic_publications` row without the `topic_id` and `idempotency_key`
/// columns.
type PublicationRow = (Vec<u8>, Vec<u8>, Vec<u8>, i64, i64, i64, i64, i64, i64);

fn insert_topic(
    transaction: &Transaction<'_>,
    record: &TopicRecord,
) -> Result<(), TopicAuthorityError> {
    transaction.execute(
        "INSERT INTO topics (
            topic_id, channel_id, topic_name, create_idempotency_key,
            channel_generation, channel_fencing_token, max_recipients,
            delivery_attempts, cascade_depth, retained_bytes, retention_ms,
            payer_account_id, policy_digest, active_subscriptions, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, 0, ?14)",
        params![
            record.topic_id.as_bytes().as_slice(),
            record.channel_id.as_bytes().as_slice(),
            record.name.as_slice(),
            record.idempotency_key.as_bytes().as_slice(),
            encode_generation(record.channel_generation)?,
            record.channel_fencing_token.as_slice(),
            encode_u64(record.policy.max_recipients)?,
            encode_u64(record.policy.delivery_attempts)?,
            encode_u64(record.policy.cascade_depth)?,
            encode_u64(record.policy.retained_bytes)?,
            encode_u64(record.policy.retention_ms)?,
            record.policy.payer.as_bytes().as_slice(),
            record.policy_digest.as_slice(),
            encode_u64(record.created_at_ms)?,
        ],
    )?;
    Ok(())
}

fn insert_or_resubscribe(
    transaction: &Transaction<'_>,
    record: &SubscriptionRecord,
) -> Result<(), TopicAuthorityError> {
    let changed = transaction.execute(
        "INSERT INTO topic_subscriptions (
            subscription_id, topic_id, subscriber_key, active, cursor,
            subscribed_at_ms, unsubscribed_at_ms, last_advanced_at_ms
         ) VALUES (?1, ?2, ?3, 1, ?4, ?5, 0, 0)
         ON CONFLICT(topic_id, subscriber_key) DO UPDATE SET
            active=1, cursor=excluded.cursor,
            subscribed_at_ms=excluded.subscribed_at_ms,
            unsubscribed_at_ms=0, last_advanced_at_ms=0
         WHERE topic_subscriptions.active=0",
        params![
            record.subscription_id.as_bytes().as_slice(),
            record.topic_id.as_bytes().as_slice(),
            record.subscriber_key.as_bytes().as_slice(),
            encode_u64(record.cursor)?,
            encode_u64(record.subscribed_at_ms)?,
        ],
    )?;
    if changed != 1 {
        return Err(TopicAuthorityError::CorruptRecord(
            "subscription admission CAS lost",
        ));
    }
    Ok(())
}

fn bump_active_count(
    transaction: &Transaction<'_>,
    topic_id: TopicId,
    expected: u64,
    updated: u64,
) -> Result<(), TopicAuthorityError> {
    let changed = transaction.execute(
        "UPDATE topics SET active_subscriptions=?1
         WHERE topic_id=?2 AND active_subscriptions=?3",
        params![
            encode_u64(updated)?,
            topic_id.as_bytes().as_slice(),
            encode_u64(expected)?,
        ],
    )?;
    if changed != 1 {
        return Err(TopicAuthorityError::CorruptRecord(
            "topic active-subscription counter CAS lost",
        ));
    }
    Ok(())
}

fn insert_publication(
    transaction: &Transaction<'_>,
    topic: &TopicRecord,
    idempotency_key: IdempotencyKey,
    payload_digest: [u8; 32],
    published_at_ms: u64,
) -> Result<(), TopicAuthorityError> {
    transaction.execute(
        "INSERT INTO topic_publications (
            idempotency_key, topic_id, policy_digest, payer_account_id,
            payload_digest, status, channel_sequence, channel_generation,
            cascade_budget_remaining, published_at_ms, enqueued_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, 0, 0, 0, ?6, ?7, 0)",
        params![
            idempotency_key.as_bytes().as_slice(),
            topic.topic_id.as_bytes().as_slice(),
            topic.policy_digest.as_slice(),
            topic.policy.payer.as_bytes().as_slice(),
            payload_digest.as_slice(),
            encode_u64(topic.policy.cascade_depth)?,
            encode_u64(published_at_ms)?,
        ],
    )?;
    Ok(())
}

fn count_active_subscriptions(
    connection: &Connection,
    topic_id: TopicId,
) -> Result<u64, TopicAuthorityError> {
    let count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM topic_subscriptions WHERE topic_id=?1 AND active=1",
        [topic_id.as_bytes().as_slice()],
        |row| row.get(0),
    )?;
    decode_u64(count)
}

fn load_topic_by_create_key(
    connection: &Connection,
    key: IdempotencyKey,
) -> Result<Option<TopicRecord>, TopicAuthorityError> {
    let topic_id = connection
        .query_row(
            "SELECT topic_id FROM topics WHERE create_idempotency_key=?1",
            [key.as_bytes().as_slice()],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()?;
    topic_id
        .map(|bytes| load_topic_optional(connection, TopicId::from_bytes(array16(bytes)?)))
        .transpose()
        .map(Option::flatten)
}

fn load_topic_optional(
    connection: &Connection,
    topic_id: TopicId,
) -> Result<Option<TopicRecord>, TopicAuthorityError> {
    let raw = connection
        .query_row(
            "SELECT channel_id, topic_name, create_idempotency_key,
                    channel_generation, channel_fencing_token, max_recipients,
                    delivery_attempts, cascade_depth, retained_bytes, retention_ms,
                    payer_account_id, policy_digest, active_subscriptions, created_at_ms
             FROM topics WHERE topic_id=?1",
            [topic_id.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, i64>(9)?,
                    row.get::<_, Vec<u8>>(10)?,
                    row.get::<_, Vec<u8>>(11)?,
                    row.get::<_, i64>(12)?,
                    row.get::<_, i64>(13)?,
                ))
            },
        )
        .optional()?;
    raw.map(|row| decode_topic(topic_id, row)).transpose()
}

/// Loads the topic head and cross-checks it against the derived state: the
/// authority-derived identity, the re-derived policy digest and the
/// active-subscription counter re-counted from the subscription rows.
fn load_topic_verified(
    connection: &Connection,
    topic_id: TopicId,
) -> Result<TopicRecord, TopicAuthorityError> {
    let record = load_topic_optional(connection, topic_id)?
        .ok_or(TopicAuthorityError::TopicNotFound(topic_id))?;
    let active = count_active_subscriptions(connection, topic_id)?;
    if record.active_subscriptions != active {
        return Err(TopicAuthorityError::CorruptRecord(
            "topic active-subscription counter disagrees with the subscription rows",
        ));
    }
    Ok(record)
}

fn decode_topic(stored_id: TopicId, row: TopicRow) -> Result<TopicRecord, TopicAuthorityError> {
    let (
        channel_id,
        name,
        idempotency_key,
        generation,
        fencing_token,
        max_recipients,
        delivery_attempts,
        cascade_depth,
        retained_bytes,
        retention_ms,
        payer,
        stored_policy_digest,
        active_subscriptions,
        created_at_ms,
    ) = row;
    let channel_id = ChannelId::from_bytes(array16(channel_id)?);
    let policy = TopicPolicy {
        max_recipients: decode_u64(max_recipients)?,
        delivery_attempts: decode_u64(delivery_attempts)?,
        cascade_depth: decode_u64(cascade_depth)?,
        retained_bytes: decode_u64(retained_bytes)?,
        retention_ms: decode_u64(retention_ms)?,
        payer: ResourceAccountId::from_bytes(array16(payer)?),
    };
    validate_name(&name)?;
    validate_policy(&policy)?;
    if stored_policy_digest != derive_policy_digest(&policy) {
        return Err(TopicAuthorityError::CorruptRecord(
            "topic policy digest disagrees with the stored policy",
        ));
    }
    if stored_id != topic_id_for(channel_id, &name) {
        return Err(TopicAuthorityError::CorruptRecord(
            "topic id disagrees with the authority-derived identity",
        ));
    }
    Ok(TopicRecord {
        topic_id: stored_id,
        channel_id,
        name,
        channel_generation: decode_generation(generation)?,
        channel_fencing_token: array32(fencing_token)?,
        policy,
        policy_digest: array32(stored_policy_digest)?,
        idempotency_key: IdempotencyKey::from_bytes(array16(idempotency_key)?),
        active_subscriptions: decode_u64(active_subscriptions)?,
        created_at_ms: decode_u64(created_at_ms)?,
    })
}

fn load_subscription_optional(
    connection: &Connection,
    topic_id: TopicId,
    subscriber_key: SubscriberKey,
) -> Result<Option<SubscriptionRecord>, TopicAuthorityError> {
    let raw = connection
        .query_row(
            "SELECT subscription_id, active, cursor, subscribed_at_ms,
                    unsubscribed_at_ms, last_advanced_at_ms
             FROM topic_subscriptions WHERE topic_id=?1 AND subscriber_key=?2",
            params![
                topic_id.as_bytes().as_slice(),
                subscriber_key.as_bytes().as_slice(),
            ],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            },
        )
        .optional()?;
    raw.map(
        |(subscription_id, active, cursor, subscribed, unsubscribed, advanced)| {
            let record = SubscriptionRecord {
                subscription_id: SubscriptionId::from_bytes(array16(subscription_id)?),
                topic_id,
                subscriber_key,
                active: active == 1,
                cursor: decode_u64(cursor)?,
                subscribed_at_ms: decode_u64(subscribed)?,
                unsubscribed_at_ms: decode_u64(unsubscribed)?,
                last_advanced_at_ms: decode_u64(advanced)?,
            };
            if record.subscription_id != subscription_id_for(topic_id, subscriber_key) {
                return Err(TopicAuthorityError::CorruptRecord(
                    "subscription id disagrees with the authority-derived identity",
                ));
            }
            Ok(record)
        },
    )
    .transpose()
}

fn load_publication_by_key(
    connection: &Connection,
    key: IdempotencyKey,
) -> Result<Option<PublicationRecord>, TopicAuthorityError> {
    let raw = connection
        .query_row(
            "SELECT topic_id, policy_digest, payer_account_id, payload_digest,
                    status, channel_sequence, channel_generation,
                    cascade_budget_remaining, published_at_ms, enqueued_at_ms
             FROM topic_publications WHERE idempotency_key=?1",
            [key.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, i64>(9)?,
                ))
            },
        )
        .optional()?;
    raw.map(|row| {
        let topic_id = TopicId::from_bytes(array16(row.0)?);
        decode_publication(
            topic_id,
            key,
            (
                row.1, row.2, row.3, row.4, row.5, row.6, row.7, row.8, row.9,
            ),
        )
    })
    .transpose()
}

/// Decodes one publication row and enforces its structural invariants: the
/// status mapping is known, a `PENDING_ENQUEUE` row binds no sequence or
/// generation, and an `ENQUEUED` row binds both.  Cross-checks against the
/// topic head and the channel high-water run in
/// [`TopicAuthority::inspect_publications`], where both are known.
fn decode_publication(
    topic_id: TopicId,
    idempotency_key: IdempotencyKey,
    row: PublicationRow,
) -> Result<PublicationRecord, TopicAuthorityError> {
    let (
        policy_digest,
        payer,
        payload_digest,
        status,
        sequence,
        generation,
        budget,
        published,
        enqueued,
    ) = row;
    let status = match status {
        0 => PublicationStatus::PendingEnqueue,
        1 => PublicationStatus::Enqueued,
        _ => {
            return Err(TopicAuthorityError::CorruptRecord(
                "publication status is unknown",
            ));
        }
    };
    let record = PublicationRecord {
        topic_id,
        idempotency_key,
        policy_digest: array32(policy_digest)?,
        payer: ResourceAccountId::from_bytes(array16(payer)?),
        payload_digest: array32(payload_digest)?,
        status,
        channel_sequence: decode_u64(sequence)?,
        channel_generation: decode_u64(generation)?,
        cascade_budget_remaining: decode_u64(budget)?,
        published_at_ms: decode_u64(published)?,
        enqueued_at_ms: decode_u64(enqueued)?,
    };
    match record.status {
        PublicationStatus::PendingEnqueue => {
            if record.channel_sequence != 0 || record.channel_generation != 0 {
                return Err(TopicAuthorityError::CorruptRecord(
                    "pending publication binds a channel sequence",
                ));
            }
        }
        PublicationStatus::Enqueued => {
            if record.channel_sequence < 1 || record.channel_generation < 1 {
                return Err(TopicAuthorityError::CorruptRecord(
                    "enqueued publication binds no channel sequence",
                ));
            }
        }
    }
    Ok(record)
}

fn validate_name(name: &[u8]) -> Result<(), TopicAuthorityError> {
    if name.is_empty() {
        return Err(TopicAuthorityError::InvalidPolicy(
            "topic name must be non-empty",
        ));
    }
    Ok(())
}

fn validate_policy(policy: &TopicPolicy) -> Result<(), TopicAuthorityError> {
    if policy.max_recipients < 1 {
        return Err(TopicAuthorityError::InvalidPolicy(
            "max_recipients must be at least 1",
        ));
    }
    if policy.delivery_attempts < 1 {
        return Err(TopicAuthorityError::InvalidPolicy(
            "delivery_attempts must be at least 1",
        ));
    }
    if policy.cascade_depth < 1 {
        return Err(TopicAuthorityError::InvalidPolicy(
            "cascade_depth must be at least 1",
        ));
    }
    if policy.retained_bytes < 1 {
        return Err(TopicAuthorityError::InvalidPolicy(
            "retained_bytes must be at least 1",
        ));
    }
    if policy.retention_ms < 1 {
        return Err(TopicAuthorityError::InvalidPolicy(
            "retention_ms must be at least 1",
        ));
    }
    if policy.payer.as_bytes() == &[0; 16] {
        return Err(TopicAuthorityError::InvalidPolicy(
            "payer binding must be a non-empty ResourceAccountId",
        ));
    }
    Ok(())
}

fn topic_id_for(channel_id: ChannelId, name: &[u8]) -> TopicId {
    TopicId::from_bytes(derive_id(
        b"nlos/topic/id/v1",
        &[channel_id.as_bytes(), name],
    ))
}

fn subscription_id_for(topic_id: TopicId, subscriber_key: SubscriberKey) -> SubscriptionId {
    SubscriptionId::from_bytes(derive_id(
        b"nlos/topic/subscription/id/v1",
        &[topic_id.as_bytes(), subscriber_key.as_bytes()],
    ))
}

fn derive_policy_digest(policy: &TopicPolicy) -> [u8; 32] {
    derive_token(
        b"nlos/topic/policy/v1",
        &[
            &policy.max_recipients.to_be_bytes(),
            &policy.delivery_attempts.to_be_bytes(),
            &policy.cascade_depth.to_be_bytes(),
            &policy.retained_bytes.to_be_bytes(),
            &policy.retention_ms.to_be_bytes(),
            policy.payer.as_bytes(),
        ],
    )
}

fn derive_payload_digest(payload: &[u8]) -> [u8; 32] {
    derive_token(b"nlos/topic/payload/v1", &[payload])
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

fn encode_generation(generation: Generation) -> Result<i64, TopicAuthorityError> {
    encode_u64(generation.get())
}

fn encode_u64(value: u64) -> Result<i64, TopicAuthorityError> {
    i64::try_from(value).map_err(|_| TopicAuthorityError::CorruptRecord("u64 exceeds SQLite i64"))
}

fn decode_u64(value: i64) -> Result<u64, TopicAuthorityError> {
    u64::try_from(value).map_err(|_| TopicAuthorityError::CorruptRecord("negative integer"))
}

fn decode_generation(value: i64) -> Result<Generation, TopicAuthorityError> {
    let value = decode_u64(value)?;
    std::num::NonZeroU64::new(value)
        .map(Generation::new)
        .ok_or(TopicAuthorityError::CorruptRecord("zero generation"))
}

fn array16(bytes: Vec<u8>) -> Result<[u8; 16], TopicAuthorityError> {
    bytes
        .try_into()
        .map_err(|_| TopicAuthorityError::CorruptRecord("identity length is not 16"))
}

fn array32(bytes: Vec<u8>) -> Result<[u8; 32], TopicAuthorityError> {
    bytes
        .try_into()
        .map_err(|_| TopicAuthorityError::CorruptRecord("digest length is not 32"))
}
