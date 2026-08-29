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
//! Cascade republish ([`TopicAuthority::republish`]) spends one unit of the
//! parent publication's cascade budget through a guarded compare-and-set
//! inside the same `Immediate` transaction that durably registers the child
//! publication (level `parent + 1`, parent key recorded for the auditable
//! provenance chain); the child enqueue then follows the publish
//! verify-then-commit path.  The budget spend (topic authority) strictly
//! precedes the child enqueue (channel authority): a crash between the two
//! leaves the budget spent and the child `PENDING_ENQUEUE`, converged by
//! replaying the same idempotency key without spending again.
//!
//! Delivery attempts (`RSM-FANOUT-001`, schema v4) are executed at the
//! enqueue-commit billing point: when a publication's enqueue commit lands
//! (the publish and republish child paths share it), every `ACTIVE`
//! subscription of the topic whose cursor was already behind the pre-enqueue
//! sequence high-water — a genuinely lagging subscriber — has its durable
//! `redelivery_used` counter advanced by one in the same topic-authority
//! transaction.  A fully caught-up subscriber receives a first delivery and
//! is not billed, and the counter is never reset by catching up: the budget
//! is granted once at the declared `delivery_attempts`.  The publication
//! whose increment reaches the declared bound flips the subscription
//! `QUARANTINED` in that same transaction.  Quarantine stops delivery only:
//! [`TopicAuthority::poll`] answers the typed
//! [`TopicAuthorityError::DeliveryQuarantined`] without reading any entry,
//! while cursor advance and unsubscribe keep working, no channel entry or
//! cursor is touched, and other subscribers are unaffected.  Recovery is the
//! explicit, token-authenticated [`TopicAuthority::reinstate_with_token`]:
//! the counter is zeroed, the state flips back to active and the cursor
//! stays exactly where it was.
//!
//! Retention (`RSM-FANOUT-001`, the declared `retained_bytes`/`retention_ms`
//! bounds) is executed as publish-side admission backpressure: in the same
//! `Immediate` transaction that reads the policy and before the first
//! durable write, both the publish path and the republish child path measure
//! the topic's unconsumed backlog as the exact payload-length sum over the
//! durable publication journal beyond
//! `min(active subscriber cursors, channel consume high-water)` (the lag of
//! a `QUARANTINED` subscriber holds no retention budget) and the age of the
//! oldest live entry still held by an active subscriber (measured against
//! the caller-supplied request time, never a wall clock).  Rows recorded by
//! pre-v5 schemas carry the `0` length sentinel; while such a row sits in
//! the summation window the measurement merges the exact known-row sum with
//! the channel-side retained upper bound in the never-understating
//! direction.  Exceeding either declared bound rejects the call with the
//! typed
//! [`TopicAuthorityError::TopicRetentionExhausted`] before any write: zero
//! partial state, nothing deleted — backpressure on the publisher, never an
//! automatic eviction.
//!
//! Matching-predicate subscriptions (schema v6, the ADR-0007
//! matching-predicate addendum) are the minimal prefix: a pattern is an exact
//! topic name or a name prefix followed by a single trailing `*` (matching is
//! byte-wise; the wildcard matches any suffix including the empty one, and
//! any other `*` placement is an
//! [`TopicAuthorityError::InvalidPattern`] rejection before any write).  A
//! pattern subscription is a durable pattern row with its own
//! authority-derived [`PatternId`], consumption token and generation.
//! Attach runs at exactly two time points — when the pattern is subscribed
//! (it enumerates every existing topic whose name matches and expands each
//! into a regular concrete subscription, subject to the topic's
//! `max_recipients` and skipped when the key already holds an active
//! subscription there, with every attachment and skip reported verbatim) and
//! when a topic is created (every active pattern row is checked against the
//! new topic's name; the attach results are observable through
//! [`TopicAuthority::inspect_pattern_attachments`], the minimal observation
//! entry).  Publish never evaluates patterns: the two time points are
//! exhaustive because every existing topic has passed `create_topic` or was
//! covered by the subscribe-time enumeration.  Attached subscriptions are
//! ordinary rows carrying `attached_by` provenance, so delivery, cursor
//! advance, delivery-attempt billing, compaction clamping and retention
//! treat them exactly like direct subscriptions — a pattern subscriber's
//! cursor, and its billing for every publish after the attach point, are
//! identical to a direct subscriber's.  [`TopicAuthority::cancel_pattern`]
//! flips the pattern row inactive and unsubscribes the active subscriptions
//! it attached; direct subscriptions of the same key are untouched.
//!
//! Payer metering (schema v7, the ADR-0007 payer-metering addendum,
//! `RSM-METER-002` minimal prefix) is a durable, immutable attribution
//! ledger — an accounting promise, never a charge: nothing bills, credits or
//! rejects.  Every accounted byte belongs to exactly one of two accounting
//! points.  [`TopicAuthority::advance_with_token`] records one `Attributed`
//! row per ENQUEUED publication its cursor just crossed, inside the same
//! topic-authority transaction as the cursor CAS (payer = the publication
//! row's payer, bytes = the recorded payload length, evidence = the single
//! channel sequence, [`ATTRIBUTION_POLICY_VERSION`] frozen into the row).
//! [`TopicAuthority::compact`] records one `Unallocated` row per publication
//! the channel trimmed without any delivery having crossed it (the isolated
//! or lagging subscriber's residual), in the topic-authority transaction that
//! follows the channel decision — re-running a compact heals rows a crash
//! window between the two authorities could have lost, and a replaying
//! compact records nothing new.  Both points obey first-accounting-event-
//! wins: a sequence is accounted at most once, by whichever accounting point
//! touches it first, so overlapping subscriber advances never double-count
//! one publication's fanout cost and the metering stays per publication
//! (never per subscriber — pattern-attached subscriptions are ordinary
//! rows and need no separate treatment).  [`TopicAuthority::
//! inspect_attribution`] reconciles the ledger against the publication
//! journal and fails closed as [`TopicAuthorityError::CorruptRecord`] on
//! any disagreement.
//!
//! It deliberately does not implement: runtime-automatic cascade triggering
//! (republish is an owner-invoked forwarding step), billing or credit
//! settlement against the attribution ledger (and any `ResourceAccount`
//! integration), cost allocation for several topics sharing one channel
//! log, attribute filtering or multi-segment wildcards on top of the minimal
//! prefix pattern language above (addendum review triggers), cross-process
//! access, wakeup wiring, or `TaskWriteSet` integration.

mod schema;

use std::collections::BTreeMap;
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
nominal_id!(PatternId);

/// The consumption identity binding issued by the authority at subscribe
/// time, mirroring the Channel [`FencingToken`] derivation style: an
/// authority-derived, domain-separated SHA-256 over the [`SubscriptionId`]
/// and the subscription generation.
///
/// It is a single-node symmetric proof of the subscribe grant, not a
/// cryptographic signature: it authenticates "the caller holds the token the
/// authority issued for this subscription generation" and nothing more.
pub type ConsumeToken = [u8; 32];

/// The delivery-attempt execution state of a subscription (schema v4,
/// `RSM-FANOUT-001`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubscriptionState {
    /// Delivery continues; the subscriber is billed one durable unit per
    /// publication that enqueues while it is genuinely lagging.
    Active,
    /// The declared `delivery_attempts` are exhausted: delivery is stopped
    /// (`poll` fails closed with [`TopicAuthorityError::DeliveryQuarantined`])
    /// until an explicit [`TopicAuthority::reinstate_with_token`].  Cursor
    /// advance and unsubscribe keep working, no message or cursor is
    /// touched, and other subscribers are unaffected.
    Quarantined,
}

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
/// [`TopicAuthority::advance`].  `consume_token` is the authority-issued
/// consumption proof for this subscription generation
/// ([`TopicAuthority::advance_with_token`] /
/// [`TopicAuthority::unsubscribe_with_token`] require it) and
/// `subscription_generation` counts the durable subscriptions of this
/// subscriber key (1 for the first, +1 per re-subscribe after an
/// unsubscribe); the generation participates in the token derivation so a
/// stale token from a previous subscription generation fails closed.
///
/// The delivery-attempt execution state (`RSM-FANOUT-001`): `state` is
/// [`Active`](SubscriptionState) until the durable `redelivery_used` counter
/// reaches the topic's declared `delivery_attempts`, then
/// [`Quarantined`](SubscriptionState); the counter is billed once per
/// publication that enqueued while the subscriber was genuinely lagging and
/// is zeroed only by an explicit reinstate.  `quarantined_at_ms` records the
/// most recent quarantine flip (0 while never), and `reinstated_at_ms` the
/// most recent explicit reinstate (0 while never) — the marker that
/// distinguishes a reinstate replay from a never-quarantined subscription.
/// A re-subscribe after an unsubscribe starts a fresh budget.
///
/// `attached_by` (schema v6) records the subscription's provenance: `None`
/// for a direct subscription, the originating [`PatternId`] for a
/// subscription expanded by a matching-pattern attach.  An attached
/// subscription is otherwise a fully ordinary row — delivery, cursor,
/// billing and compaction semantics are identical to a direct one — and the
/// next activation of the row (direct re-subscribe or a new attach)
/// overwrites the provenance.
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
    pub consume_token: ConsumeToken,
    pub subscription_generation: u64,
    pub state: SubscriptionState,
    pub redelivery_used: u64,
    pub quarantined_at_ms: u64,
    pub reinstated_at_ms: u64,
    pub attached_by: Option<PatternId>,
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

/// The explicit recovery decision for a quarantined subscription
/// (`RSM-FANOUT-001`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReinstateDecision {
    /// This call cleared the durable redelivery counter and flipped the
    /// subscription back to [`SubscriptionState::Active`] with its cursor
    /// exactly where it was.
    Reinstated(SubscriptionRecord),
    /// The subscription was already reinstated; the durable record returns
    /// (the original reinstate timestamp included) and nothing is written
    /// again.
    Replayed(SubscriptionRecord),
}

impl ReinstateDecision {
    #[must_use]
    pub fn record(self) -> SubscriptionRecord {
        match self {
            Self::Reinstated(record) | Self::Replayed(record) => record,
        }
    }
}

/// A matching-predicate subscription request (schema v6, the ADR-0007
/// matching-predicate addendum): the pattern text, an opaque non-zero
/// subscriber binding, the subscriber key, the idempotency scope and the
/// caller-supplied subscribe time.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscribePatternRequest {
    /// Exact topic name, or a name prefix followed by one trailing `*`.
    pub pattern: Vec<u8>,
    /// Opaque typed binding of the pattern subscriber; must not be the
    /// all-zero (unbound) account.
    pub binding: ResourceAccountId,
    pub subscriber_key: SubscriberKey,
    pub idempotency_key: IdempotencyKey,
    pub subscribed_at_ms: u64,
}

/// The durable pattern subscription row.
///
/// `pattern_id` is authority-derived from the pattern text and the
/// subscriber key; `consume_token` is derived from the pattern id and
/// `pattern_generation` (mirroring the concrete subscription token and
/// required by [`TopicAuthority::cancel_pattern`]); `cancelled_at_ms` is `0`
/// while the row is active and records the most recent cancel otherwise.
/// Re-subscribing a cancelled (pattern, key) pair bumps the generation and
/// issues a fresh token.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PatternRecord {
    pub pattern_id: PatternId,
    pub pattern: Vec<u8>,
    pub binding: ResourceAccountId,
    pub subscriber_key: SubscriberKey,
    pub active: bool,
    pub consume_token: ConsumeToken,
    pub pattern_generation: u64,
    pub subscribed_at_ms: u64,
    pub cancelled_at_ms: u64,
}

/// One topic the subscribe-time attach enumeration expanded the pattern
/// into.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AttachedSubscription {
    pub topic_id: TopicId,
    /// The regular concrete subscription created by the attach; carries
    /// `attached_by` provenance and the ordinary consume token.
    pub subscription: SubscriptionRecord,
}

/// Why the subscribe-time attach enumeration excluded one matching topic.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttachSkipReason {
    /// The subscriber key already holds an active subscription on the topic
    /// (direct or attached): the earlier grant wins and no duplicate
    /// delivery is created.
    AlreadySubscribed,
    /// The topic's active subscriptions are already at its declared
    /// `max_recipients`.
    RecipientLimitReached,
}

/// One matching topic the attach enumeration skipped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AttachSkipped {
    pub topic_id: TopicId,
    pub reason: AttachSkipReason,
}

/// The verbatim subscribe-time attach report: a pattern subscriber is not
/// guaranteed to observe every matching topic (a filled
/// `max_recipients` slot is skipped and reported, never queued).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AttachReport {
    pub attached: Vec<AttachedSubscription>,
    pub skipped: Vec<AttachSkipped>,
}

/// The subscribe-pattern outcome paired with its attach report.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PatternSubscribeOutcome {
    pub pattern: PatternRecord,
    pub report: AttachReport,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PatternSubscribeDecision {
    /// This call created (or re-activated) the pattern row and ran the
    /// attach enumeration over the existing matching topics.
    Subscribed(PatternSubscribeOutcome),
    /// The request replayed: the *current* durable pattern row returns (its
    /// state may have advanced past the original subscribe — e.g. been
    /// cancelled), the report is empty because the enumeration is a
    /// subscribe-time effect, and nothing is written.
    Replayed(PatternSubscribeOutcome),
}

impl PatternSubscribeDecision {
    #[must_use]
    pub fn pattern(self) -> PatternRecord {
        match self {
            Self::Subscribed(outcome) | Self::Replayed(outcome) => outcome.pattern,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CancelPatternRequest {
    pub pattern_id: PatternId,
    pub cancelled_at_ms: u64,
}

/// One attached subscription deactivated by a pattern cancel, with the
/// ordinary unsubscribe receipt it produced.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DetachReceipt {
    pub topic_id: TopicId,
    pub receipt: UnsubscribeReceipt,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CancelPatternReceipt {
    /// The pattern row in its post-call state.
    pub pattern: PatternRecord,
    /// The active attached subscriptions this call unsubscribed, ordered by
    /// topic id.
    pub detached: Vec<DetachReceipt>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CancelPatternDecision {
    /// This call flipped the pattern row cancelled and detached its active
    /// attached subscriptions.
    Cancelled(CancelPatternReceipt),
    /// The pattern row was already cancelled; the current row returns and
    /// nothing is detached again (the original receipts were returned by
    /// the cancelling call).
    Replayed(CancelPatternReceipt),
}

impl CancelPatternDecision {
    #[must_use]
    pub fn receipt(self) -> CancelPatternReceipt {
        match self {
            Self::Cancelled(receipt) | Self::Replayed(receipt) => receipt,
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
    /// Cascade level of this publication: 0 for a root publish, or the
    /// parent's level + 1 for a republished child.
    pub cascade_level: u64,
    /// The publication this row was republished from; `None` for roots.
    pub parent_publication_key: Option<IdempotencyKey>,
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

/// A cascade forward of one parent publication into a child topic
/// (`RSM-FANOUT-001`): one unit of the parent publication's cascade budget
/// is spent and the payload is published to the child topic as a fresh
/// publication with its own policy binding and idempotency scope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepublishRequest {
    pub child_topic_id: TopicId,
    pub parent_publication_key: IdempotencyKey,
    pub payload: Vec<u8>,
    pub idempotency_key: IdempotencyKey,
    pub republished_at_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepublishDecision {
    /// This call spent the parent budget (or resumed a spent-but-pending
    /// child) and completed the child enqueue.
    Republished(PublicationRecord),
    /// The exact request had already completed; the original record returns,
    /// the budget is not spent again and nothing is enqueued again.
    Replayed(PublicationRecord),
}

impl RepublishDecision {
    #[must_use]
    pub fn record(self) -> PublicationRecord {
        match self {
            Self::Republished(record) | Self::Replayed(record) => record,
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

/// How the byte bound's `backlog_bytes` figure in
/// [`TopicAuthorityError::TopicRetentionExhausted`] was measured
/// (`RSM-FANOUT-001` retention, increment 52).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RetentionBacklogPrecision {
    /// The exact ADR-0007 backlog: `Σ payload_bytes` over the enqueued
    /// publication rows beyond the release point
    /// `min(active subscriber cursors, channel consume high-water)`, with
    /// every row in the window carrying its true recorded payload length.
    Exact,
    /// Legacy rows recorded before schema v5 (the `0` length sentinel) sit
    /// in the summation window, so their true per-entry bytes are unknown;
    /// the reported figure merges the exact known-row sum with the
    /// channel-side retained upper bound in the never-understating
    /// direction.  The window returns to [`Self::Exact`] once catch-up and
    /// compaction advance the release point past the sentinel rows.
    LegacyConservative,
}

/// The versioned attribution policy of the metering ledger (the ADR-0007
/// payer-metering addendum, `RSM-METER-002`): a policy-level constant frozen
/// into every ledger row at write time, so a future policy change is
/// observable per row instead of silently rewriting history.  This build
/// writes and understands exactly version 1; a ledger row carrying any other
/// version fails closed as [`TopicAuthorityError::CorruptRecord`] at
/// inspection time.
pub const ATTRIBUTION_POLICY_VERSION: u64 = 1;

/// How the metering ledger accounted one publication (`RSM-METER-002`
/// minimal prefix).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttributionKind {
    /// The publication was delivered: an accepted cursor advance crossed its
    /// channel sequence (`TopicAuthority::advance_with_token` accounting
    /// point).
    Attributed,
    /// The publication left the channel log without ever being delivered (a
    /// compaction deleted an unconsumed residual — the isolated or lagging
    /// subscriber's lag) (`TopicAuthority::compact` accounting point).
    Unallocated,
}

impl AttributionKind {
    /// The durable discriminator stored in the ledger row's `kind` column.
    #[must_use]
    pub const fn code(self) -> i64 {
        match self {
            Self::Attributed => 1,
            Self::Unallocated => 2,
        }
    }

    fn from_code(code: i64) -> Option<Self> {
        match code {
            1 => Some(Self::Attributed),
            2 => Some(Self::Unallocated),
            _ => None,
        }
    }
}

/// The payer metering reconciliation report for one topic (the ADR-0007
/// payer-metering addendum).
///
/// The reconciliation identity, byte-exact: every ENQUEUED publication of
/// the topic is either covered by exactly one ledger row whose bytes equal
/// the publication's recorded payload length, or its bytes are still
/// *unsettled* — live backlog awaiting its accounting event (no accepted
/// advance has crossed the sequence and no compaction has deleted it yet).
/// Therefore
/// `attributed_bytes + unallocated_bytes + unsettled_bytes == total`, where
/// `total` is `Σ payload_bytes` over *every* ENQUEUED publication row of the
/// journal (trimmed rows included — the journal is durable and never
/// deleted).  A settled topic (`unsettled_bytes == 0`) reduces this to the
/// addendum's `attributed + unallocated == total`; a topic with live backlog
/// keeps the identity only through the third term, which is why the term is
/// reported explicitly instead of being silently folded into an unbalance.
///
/// `balanced` is `true` on every returned report: the inspection validates
/// the identity (and every covered row's bytes, payer, kind, policy version
/// and derived identity, plus the absence of ledger rows for sequences
/// without an ENQUEUED publication, and of uncovered publications at or
/// below the channel trim watermark — those are deleted bytes no accounting
/// event ever covered) and fails closed with
/// [`TopicAuthorityError::CorruptRecord`] instead of returning an
/// unbalanced report.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AttributionReport {
    pub topic_id: TopicId,
    /// `Σ` ledger bytes of [`AttributionKind::Attributed`] rows (delivered
    /// publications).
    pub attributed_bytes: u64,
    /// `Σ` ledger bytes of [`AttributionKind::Unallocated`] rows (deleted
    /// before delivery).
    pub unallocated_bytes: u64,
    /// `Σ` payload bytes of ENQUEUED publications covered by no ledger row
    /// yet: live backlog pending its accounting event.
    pub unsettled_bytes: u64,
    /// `Σ payload_bytes` over every ENQUEUED publication row of the topic
    /// (trimmed rows included).
    pub total: u64,
    /// The attribution policy version the ledger rows carry (validated to be
    /// [`ATTRIBUTION_POLICY_VERSION`]).
    pub policy_version: u64,
    /// Always `true` on a returned report; any violation of the
    /// reconciliation identity is a fail-closed
    /// [`TopicAuthorityError::CorruptRecord`], not a `false` flag.
    pub balanced: bool,
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
    /// The polled subscription is `QUARANTINED`: its declared delivery
    /// attempts are exhausted, so the poll fails closed without reading any
    /// entry (quarantine rejects the delivery service, not the caller's
    /// identity — `poll` stays token-free).
    DeliveryQuarantined(SubscriptionId),
    /// A reinstate was presented for a subscription that is not
    /// `QUARANTINED` and has no completed reinstate to replay; recovery is
    /// explicit, so nothing is written.
    NotQuarantined(SubscriptionId),
    /// A consumption token was presented that does not match the
    /// authority-issued token of the subscription's current generation; the
    /// caller is not the holder of the subscribe grant and nothing is written.
    ConsumptionTokenMismatch(SubscriptionId),
    /// A pattern subscription request was rejected before any write: the
    /// pattern text is neither an exact topic name nor a name prefix with a
    /// single trailing `*` (the empty string, a `*` in the middle, or a
    /// second `*`).
    InvalidPattern(&'static str),
    /// The requested pattern subscription row does not exist.
    TopicPatternNotFound(PatternId),
    /// A pattern consumption token was presented that does not match the
    /// authority-issued token of the pattern row's current generation; the
    /// caller is not the holder of the pattern grant and nothing is written.
    PatternConsumptionTokenMismatch(PatternId),
    PublicationNotFound(IdempotencyKey),
    /// The parent publication exists but has not reached the terminal
    /// [`PublicationStatus::Enqueued`] state, so it is not forwardable.
    PublicationNotEnqueued(IdempotencyKey),
    /// The guarded budget CAS affected no row: the parent publication's
    /// cascade budget is fully spent.
    CascadeBudgetExhausted(IdempotencyKey),
    CascadeDepthExceeded {
        parent_publication_key: IdempotencyKey,
        requested_level: u64,
        cascade_depth: u64,
    },
    /// A publish or republish was rejected by the topic's declared retention
    /// bounds before any durable write (`RSM-FANOUT-001` retention): the
    /// unconsumed backlog plus the new payload would exceed the declared
    /// `retained_bytes`, or the oldest live entry still held by an active
    /// subscriber is older than the declared `retention_ms` measured against
    /// the caller-supplied request time.  Fail-closed backpressure: the
    /// rejected call leaves zero partial state and nothing is deleted.
    TopicRetentionExhausted {
        topic_id: TopicId,
        /// Declared byte upper bound (`TopicPolicy::retained_bytes`).
        retained_bytes_declared: u64,
        /// Measured unconsumed backlog bytes before the rejected write;
        /// exact or conservatively merged per `backlog_precision`.
        backlog_bytes: u64,
        /// How `backlog_bytes` was measured: the exact publication-journal
        /// sum, or the legacy-sentinel conservative merge.
        backlog_precision: RetentionBacklogPrecision,
        /// Payload bytes of the rejected publication.
        payload_bytes: u64,
        /// Declared time upper bound (`TopicPolicy::retention_ms`).
        retention_ms_declared: u64,
        /// Age in ms of the oldest live entry still held by an active
        /// subscriber, measured against the caller-supplied request time
        /// (`0` when no live entry is currently held by one).
        oldest_unconsumed_age_ms: u64,
    },
    InvalidPolicy(&'static str),
    SubscriberLimitReached,
    IdempotencyConflict,
    InvalidPayload,
    InvalidSequence(&'static str),
    CorruptRecord(&'static str),
    LockPoisoned,
}

impl fmt::Display for TopicAuthorityError {
    #[allow(clippy::too_many_lines)] // One arm per typed variant keeps the failure text beside its definition.
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
            Self::DeliveryQuarantined(id) => write!(
                formatter,
                "subscription {id:?} is quarantined: the declared delivery attempts are \
                 exhausted and delivery is stopped until an explicit reinstate"
            ),
            Self::NotQuarantined(id) => write!(
                formatter,
                "subscription {id:?} is not quarantined, so there is nothing to reinstate"
            ),
            Self::ConsumptionTokenMismatch(id) => write!(
                formatter,
                "consumption token does not match the authority-issued token of subscription {id:?}"
            ),
            Self::InvalidPattern(reason) => {
                write!(
                    formatter,
                    "invalid topic pattern: {reason} (exact name or `prefix*` tail wildcard)"
                )
            }
            Self::TopicPatternNotFound(id) => {
                write!(formatter, "topic pattern {id:?} does not exist")
            }
            Self::PatternConsumptionTokenMismatch(id) => write!(
                formatter,
                "pattern consumption token does not match the authority-issued token of \
                 pattern {id:?}"
            ),
            Self::PublicationNotFound(key) => {
                write!(formatter, "publication {key:?} does not exist")
            }
            Self::PublicationNotEnqueued(key) => write!(
                formatter,
                "publication {key:?} has not reached the terminal enqueued state"
            ),
            Self::CascadeBudgetExhausted(key) => write!(
                formatter,
                "publication {key:?} has no cascade budget remaining"
            ),
            Self::CascadeDepthExceeded {
                parent_publication_key,
                requested_level,
                cascade_depth,
            } => write!(
                formatter,
                "cascade level {requested_level} from {parent_publication_key:?} exceeds \
                 the parent policy cascade_depth {cascade_depth}"
            ),
            Self::TopicRetentionExhausted {
                topic_id,
                retained_bytes_declared,
                backlog_bytes,
                backlog_precision,
                payload_bytes,
                retention_ms_declared,
                oldest_unconsumed_age_ms,
            } => write!(
                formatter,
                "topic {topic_id:?} retention bounds exhausted: backlog {backlog_bytes} ({backlog_precision:?}) + \
                 payload {payload_bytes} bytes against declared retained_bytes \
                 {retained_bytes_declared}, oldest unconsumed held entry \
                 {oldest_unconsumed_age_ms}ms against declared retention_ms \
                 {retention_ms_declared}"
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
            0 => {
                schema::migrate_v1(&mut connection)?;
                schema::migrate_v2(&mut connection)?;
                schema::migrate_v3(&mut connection)?;
                schema::migrate_v4(&mut connection)?;
                schema::migrate_v5(&mut connection)?;
                schema::migrate_v6(&mut connection)?;
                schema::migrate_v7(&mut connection)?;
            }
            1 => {
                schema::migrate_v2(&mut connection)?;
                schema::migrate_v3(&mut connection)?;
                schema::migrate_v4(&mut connection)?;
                schema::migrate_v5(&mut connection)?;
                schema::migrate_v6(&mut connection)?;
                schema::migrate_v7(&mut connection)?;
            }
            2 => {
                schema::migrate_v3(&mut connection)?;
                schema::migrate_v4(&mut connection)?;
                schema::migrate_v5(&mut connection)?;
                schema::migrate_v6(&mut connection)?;
                schema::migrate_v7(&mut connection)?;
            }
            3 => {
                schema::migrate_v4(&mut connection)?;
                schema::migrate_v5(&mut connection)?;
                schema::migrate_v6(&mut connection)?;
                schema::migrate_v7(&mut connection)?;
            }
            4 => {
                schema::migrate_v5(&mut connection)?;
                schema::migrate_v6(&mut connection)?;
                schema::migrate_v7(&mut connection)?;
            }
            // The watermark version: the rebuild chain is complete, so only
            // the idempotent additive pre-checks remain (no-ops when the v6
            // and v7 objects are already present).
            schema::SCHEMA_VERSION => {
                schema::migrate_v6(&mut connection)?;
                schema::migrate_v7(&mut connection)?;
            }
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
    /// Matching-predicate attach at the create time point (schema v6, the
    /// ADR-0007 matching addendum): after the topic head insert, every
    /// `ACTIVE` pattern row whose pattern matches the new topic's name
    /// expands into a regular concrete subscription inside this same
    /// transaction, under the same admission rules as
    /// [`TopicAuthority::subscribe`] (a filled `max_recipients` slot or an
    /// already-active key for the pattern's subscriber skips that pattern).
    /// The decision value stays the topic record; the minimal observation
    /// entry for the attach results is
    /// [`TopicAuthority::inspect_pattern_attachments`] plus the
    /// subscriptions' `attached_by` provenance (create-time skips are not
    /// recorded separately).  A failure in the attach step aborts the whole
    /// create: zero partial state.
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
        // Pattern attach at the create time point (ADR-0007 matching
        // addendum): part of the create transaction, so a failure here
        // aborts the create with zero durable state.
        attach_topic_to_matching_patterns(
            &transaction,
            &self.channel,
            &record,
            request.created_at_ms,
        )?;
        // The attach may have occupied admission slots: return the
        // post-attach verified head so the decision reflects the durable
        // counter.
        let created = load_topic_verified(&transaction, topic_id)?;
        transaction.commit()?;
        Ok(TopicDecision::Created(created))
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
    /// The authority also derives and durably stores a consumption
    /// [`ConsumeToken`] for the subscription
    /// (`"nlos/topic/consume-token/v1" ‖ subscription_id ‖ subscription
    /// generation`) and returns it in the decision record: the token is the
    /// identity binding that [`TopicAuthority::advance_with_token`],
    /// [`TopicAuthority::unsubscribe_with_token`] and
    /// [`TopicAuthority::reinstate_with_token`] require.  A replayed
    /// subscribe of the same active key returns the originally issued token;
    /// a re-subscribe after an unsubscribe bumps the subscription generation
    /// and issues a fresh token, so any token from a previous generation
    /// fails closed.  A (re-)subscribed key always starts with a zeroed
    /// delivery-attempt budget in the [`SubscriptionState::Active`] state.
    /// A direct subscription is recorded with `attached_by = NULL` (any
    /// provenance from a previous activation of the row is cleared).
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
        let previous =
            load_subscription_optional(&transaction, request.topic_id, request.subscriber_key)?;
        if let Some(existing) = &previous
            && existing.active
        {
            transaction.commit()?;
            return Ok(SubscribeDecision::Replayed(*existing));
        }
        let live = self
            .channel
            .inspect_queue(topic.channel_id)
            .map_err(TopicAuthorityError::Channel)?;
        let active = count_active_subscriptions(&transaction, request.topic_id)?;
        if active >= topic.policy.max_recipients {
            return Err(TopicAuthorityError::SubscriberLimitReached);
        }
        let subscription_id = subscription_id_for(request.topic_id, request.subscriber_key);
        let subscription_generation = previous
            .as_ref()
            .map_or(1, |existing| existing.subscription_generation + 1);
        let record = SubscriptionRecord {
            subscription_id,
            topic_id: request.topic_id,
            subscriber_key: request.subscriber_key,
            active: true,
            cursor: live.max_sequence,
            subscribed_at_ms: request.subscribed_at_ms,
            unsubscribed_at_ms: 0,
            last_advanced_at_ms: 0,
            consume_token: derive_consume_token(subscription_id, subscription_generation),
            subscription_generation,
            state: SubscriptionState::Active,
            redelivery_used: 0,
            quarantined_at_ms: 0,
            reinstated_at_ms: 0,
            attached_by: None,
        };
        insert_or_resubscribe(&transaction, &record, None)?;
        bump_active_count(&transaction, request.topic_id, active, active + 1)?;
        transaction.commit()?;
        Ok(SubscribeDecision::Subscribed(record))
    }

    /// Subscribes a matching predicate — an exact topic name or a `prefix*`
    /// tail wildcard — instead of one topic (schema v6, the ADR-0007
    /// matching addendum).
    ///
    /// Pattern language (byte-wise, precise): a non-empty byte string
    /// carrying at most one `*`, and a `*` only as the final byte.  A
    /// pattern without `*` matches exactly the topic names byte-equal to it;
    /// a trailing `*` matches every name that begins with the bytes before
    /// it — the matched suffix may be empty, so `prefix*` also matches the
    /// bare `prefix` topic, and a bare `*` matches every topic.  Anything
    /// else — the empty string, a `*` in the middle, a second `*` — is
    /// [`TopicAuthorityError::InvalidPattern`] before any write, as is a
    /// zero-valued `binding`.
    ///
    /// The durable pattern row carries an authority-derived [`PatternId`]
    /// (from the pattern text and the subscriber key), the opaque `binding`,
    /// a consumption token derived from the pattern id and its generation
    /// (mirroring the concrete subscription token; required by
    /// [`TopicAuthority::cancel_pattern`]) and a UNIQUE idempotency key.
    /// Replaying the exact key returns the *current* durable row (its state
    /// may have advanced past the original subscribe — e.g. been cancelled);
    /// the same for an already-active (pattern, key) pair under any key;
    /// re-subscribing a previously cancelled pair re-activates it with a
    /// bumped generation and a fresh token; a key rebound to a different
    /// pattern, binding or subscriber key is an
    /// [`TopicAuthorityError::IdempotencyConflict`].
    ///
    /// The subscribe time point enumerates every existing topic whose name
    /// matches and attaches each one as a regular concrete subscription —
    /// the same admission and cursor semantics as [`TopicAuthority::subscribe`]
    /// (cursor at the current channel sequence high-water, history never
    /// replayed, ordinary per-topic consume token) — recorded with
    /// `attached_by` provenance.  A matching topic where the key already
    /// holds an active subscription is skipped (the earlier grant wins, no
    /// duplicate delivery); a matching topic at its `max_recipients` is
    /// skipped as well; every attachment and skip is reported verbatim in
    /// the decision's [`AttachReport`] — a pattern subscriber is not
    /// guaranteed to observe every matching topic (declared limitation).
    /// Publish never evaluates patterns: a topic created after this call is
    /// covered by the `create_topic` attach time point.  The whole operation
    /// (pattern row plus attach enumeration) is one transaction: a failure
    /// leaves zero durable state.
    ///
    /// # Errors
    ///
    /// Fails closed for an invalid pattern or an unbound binding (both
    /// before any write), idempotency rebinding, a Channel readback failure
    /// during the enumeration, or a storage/corruption failure.
    #[allow(clippy::too_many_lines)] // One method owns the pattern-row state machine plus enumeration.
    pub fn subscribe_pattern(
        &self,
        request: SubscribePatternRequest,
    ) -> Result<PatternSubscribeDecision, TopicAuthorityError> {
        validate_pattern(&request.pattern)?;
        if request.binding.as_bytes() == &[0; 16] {
            return Err(TopicAuthorityError::InvalidPolicy(
                "pattern binding must be a non-zero ResourceAccountId",
            ));
        }
        let pattern_id = pattern_id_for(&request.pattern, request.subscriber_key);
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = load_pattern_by_key(&transaction, request.idempotency_key)? {
            if existing.pattern != request.pattern
                || existing.binding != request.binding
                || existing.subscriber_key != request.subscriber_key
            {
                return Err(TopicAuthorityError::IdempotencyConflict);
            }
            let current = load_pattern_optional(&transaction, pattern_id)?.ok_or(
                TopicAuthorityError::CorruptRecord("pattern row vanished during key replay"),
            )?;
            transaction.commit()?;
            return Ok(PatternSubscribeDecision::Replayed(
                PatternSubscribeOutcome {
                    pattern: current,
                    report: AttachReport::default(),
                },
            ));
        }
        let previous = load_pattern_optional(&transaction, pattern_id)?;
        if let Some(active) = previous.as_ref().filter(|row| row.active) {
            transaction.commit()?;
            return Ok(PatternSubscribeDecision::Replayed(
                PatternSubscribeOutcome {
                    pattern: active.clone(),
                    report: AttachReport::default(),
                },
            ));
        }
        let pattern_generation = previous
            .as_ref()
            .map_or(1, |existing| existing.pattern_generation + 1);
        let record = PatternRecord {
            pattern_id,
            pattern: request.pattern,
            binding: request.binding,
            subscriber_key: request.subscriber_key,
            active: true,
            consume_token: derive_pattern_token(pattern_id, pattern_generation),
            pattern_generation,
            subscribed_at_ms: request.subscribed_at_ms,
            cancelled_at_ms: 0,
        };
        insert_or_resubscribe_pattern(&transaction, &record, request.idempotency_key)?;
        let report = attach_pattern_to_existing_topics(
            &transaction,
            &self.channel,
            &record,
            request.subscribed_at_ms,
        )?;
        transaction.commit()?;
        Ok(PatternSubscribeDecision::Subscribed(
            PatternSubscribeOutcome {
                pattern: record,
                report,
            },
        ))
    }

    /// Flips a subscription to inactive.
    ///
    /// The state-row flip is paired with the topic's active-subscription
    /// counter decrement in one `Immediate` transaction.  Inactive
    /// subscriptions are excluded from the min-live-cursor compaction bound.
    /// Repeating the unsubscribe replays the original receipt.
    ///
    /// Deprecated compatibility entry: this call authenticates the caller
    /// only by the `subscriber_key` (token-free, the pre-binding semantics),
    /// so any caller that can name the key can impersonate the subscriber
    /// and flip the subscription inactive.  The identity binding is the
    /// consumption token required by
    /// [`TopicAuthority::unsubscribe_with_token`]; prefer that entry.
    ///
    /// # Errors
    ///
    /// Fails closed for an unknown Topic or subscription, or a
    /// storage/corruption failure.
    #[deprecated(
        since = "0.1.0",
        note = "use unsubscribe_with_token; the token-free entry lets an impersonator flip the subscription"
    )]
    pub fn unsubscribe(
        &self,
        request: UnsubscribeRequest,
    ) -> Result<UnsubscribeDecision, TopicAuthorityError> {
        self.unsubscribe_inner(request, None)
    }

    /// Flips a subscription to inactive, requiring the consumption token.
    ///
    /// Identical to [`TopicAuthority::unsubscribe`] except the caller must
    /// present the [`ConsumeToken`] issued for the subscription's current
    /// generation; any other token is
    /// [`TopicAuthorityError::ConsumptionTokenMismatch`] before any write
    /// (fail-closed, zero durable state change).
    ///
    /// # Errors
    ///
    /// Fails closed for a token mismatch, an unknown Topic or subscription,
    /// or a storage/corruption failure.
    pub fn unsubscribe_with_token(
        &self,
        request: UnsubscribeRequest,
        consume_token: &ConsumeToken,
    ) -> Result<UnsubscribeDecision, TopicAuthorityError> {
        self.unsubscribe_inner(request, Some(*consume_token))
    }

    fn unsubscribe_inner(
        &self,
        request: UnsubscribeRequest,
        consume_token: Option<ConsumeToken>,
    ) -> Result<UnsubscribeDecision, TopicAuthorityError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let topic = load_topic_verified(&transaction, request.topic_id)?;
        let existing =
            load_subscription_optional(&transaction, request.topic_id, request.subscriber_key)?
                .ok_or(TopicAuthorityError::SubscriptionNotFound(
                    subscription_id_for(request.topic_id, request.subscriber_key),
                ))?;
        verify_consume_token(&existing, consume_token)?;
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

    /// Cancels a pattern subscription, requiring the pattern's consumption
    /// token.
    ///
    /// The caller must present the [`ConsumeToken`] issued for the pattern
    /// row's current generation; any other token is
    /// [`TopicAuthorityError::PatternConsumptionTokenMismatch`] before any
    /// write (fail-closed, zero durable state), mirroring the concrete
    /// subscription token binding.  On success the pattern row flips to the
    /// cancelled state and every *active* concrete subscription carrying the
    /// row's `attached_by` provenance is unsubscribed — the ordinary
    /// unsubscribe semantics (active-bit CAS, active-subscription counter
    /// decrement, receipt), all inside one transaction.  Direct
    /// subscriptions of the same subscriber key are untouched, and a
    /// subscription attached by a *different* pattern row is untouched.
    /// Repeating the cancel of an already-cancelled pattern replays the
    /// current row with an empty detach list; the pattern row itself is
    /// durable and never deleted, so a later
    /// [`TopicAuthority::subscribe_pattern`] of the same (pattern, key) pair
    /// re-activates it with a fresh generation and token.
    ///
    /// # Errors
    ///
    /// Fails closed for an unknown pattern, a token mismatch, or a
    /// storage/corruption failure.
    pub fn cancel_pattern(
        &self,
        request: CancelPatternRequest,
        consume_token: &ConsumeToken,
    ) -> Result<CancelPatternDecision, TopicAuthorityError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = load_pattern_optional(&transaction, request.pattern_id)?.ok_or(
            TopicAuthorityError::TopicPatternNotFound(request.pattern_id),
        )?;
        if *consume_token != existing.consume_token {
            return Err(TopicAuthorityError::PatternConsumptionTokenMismatch(
                request.pattern_id,
            ));
        }
        if !existing.active {
            transaction.commit()?;
            return Ok(CancelPatternDecision::Replayed(CancelPatternReceipt {
                pattern: existing,
                detached: Vec::new(),
            }));
        }
        let changed = transaction.execute(
            "UPDATE topic_patterns
             SET active=0, cancelled_at_ms=?1
             WHERE pattern_id=?2 AND active=1",
            params![
                encode_u64(request.cancelled_at_ms)?,
                request.pattern_id.as_bytes().as_slice(),
            ],
        )?;
        if changed != 1 {
            return Err(TopicAuthorityError::CorruptRecord(
                "pattern cancel CAS lost",
            ));
        }
        let detached = detach_attached_subscriptions(
            &transaction,
            request.pattern_id,
            request.cancelled_at_ms,
        )?;
        let cancelled = load_pattern_optional(&transaction, request.pattern_id)?.ok_or(
            TopicAuthorityError::CorruptRecord("pattern row vanished during cancel"),
        )?;
        transaction.commit()?;
        Ok(CancelPatternDecision::Cancelled(CancelPatternReceipt {
            pattern: cancelled,
            detached,
        }))
    }

    /// Reinstates a `QUARANTINED` subscription, requiring the consumption
    /// token.
    ///
    /// The caller must present the [`ConsumeToken`] issued for the
    /// subscription's current generation; any other token is
    /// [`TopicAuthorityError::ConsumptionTokenMismatch`] before any write.
    /// Recovery is explicit, never automatic: a quarantined subscription is
    /// flipped back to [`SubscriptionState::Active`] with its durable
    /// `redelivery_used` counter zeroed and its cursor exactly where it was —
    /// no entry is skipped and none is replayed, and the budget is re-granted
    /// only through this entry.  Repeating an already-completed reinstate of
    /// the same key replays the durable record (the stored
    /// `reinstated_at_ms` marker distinguishes that replay from a
    /// never-quarantined subscription, which fails closed with
    /// [`TopicAuthorityError::NotQuarantined`]).
    ///
    /// # Errors
    ///
    /// Fails closed for a token mismatch, an unknown Topic or subscription,
    /// an inactive subscription, a subscription that is not quarantined, or
    /// a storage/corruption failure.
    pub fn reinstate_with_token(
        &self,
        topic_id: TopicId,
        subscriber_key: SubscriberKey,
        consume_token: &ConsumeToken,
        reinstated_at_ms: u64,
    ) -> Result<ReinstateDecision, TopicAuthorityError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        load_topic_verified(&transaction, topic_id)?;
        let existing = load_subscription_optional(&transaction, topic_id, subscriber_key)?.ok_or(
            TopicAuthorityError::SubscriptionNotFound(subscription_id_for(
                topic_id,
                subscriber_key,
            )),
        )?;
        verify_consume_token(&existing, Some(*consume_token))?;
        if !existing.active {
            return Err(TopicAuthorityError::SubscriptionInactive(
                existing.subscription_id,
            ));
        }
        if existing.state == SubscriptionState::Active {
            if existing.reinstated_at_ms > 0 {
                transaction.commit()?;
                return Ok(ReinstateDecision::Replayed(existing));
            }
            return Err(TopicAuthorityError::NotQuarantined(
                existing.subscription_id,
            ));
        }
        let changed = transaction.execute(
            "UPDATE topic_subscriptions
             SET state=0, redelivery_used=0, reinstated_at_ms=?1
             WHERE subscription_id=?2 AND state=1",
            params![
                encode_u64(reinstated_at_ms)?,
                existing.subscription_id.as_bytes().as_slice(),
            ],
        )?;
        if changed != 1 {
            return Err(TopicAuthorityError::CorruptRecord(
                "subscription quarantine reinstate CAS lost",
            ));
        }
        let reinstated = load_subscription_optional(&transaction, topic_id, subscriber_key)?
            .ok_or(TopicAuthorityError::CorruptRecord(
                "subscription row vanished during quarantine reinstate",
            ))?;
        transaction.commit()?;
        Ok(ReinstateDecision::Reinstated(reinstated))
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

    /// Reads the verified pattern subscription row.
    ///
    /// The stored [`PatternId`] is re-derived from the pattern text and the
    /// subscriber key, and the stored token from the pattern id and its
    /// generation; any disagreement is
    /// [`TopicAuthorityError::CorruptRecord`].
    ///
    /// # Errors
    ///
    /// Fails closed for an unknown pattern or a corrupt record.
    pub fn inspect_pattern(
        &self,
        pattern_id: PatternId,
    ) -> Result<PatternRecord, TopicAuthorityError> {
        let connection = self.lock()?;
        load_pattern_optional(&connection, pattern_id)?
            .ok_or(TopicAuthorityError::TopicPatternNotFound(pattern_id))
    }

    /// Reads the active concrete subscriptions a pattern currently holds
    /// (its `attached_by` provenance), ordered by topic id.
    ///
    /// This is the observation entry for the `create_topic` attach time point,
    /// whose results are deliberately not carried on the [`TopicDecision`]
    /// value (the minimal-change choice documented on
    /// [`TopicAuthority::create_topic`]); the subscribe-time attach results
    /// are additionally reported on the
    /// [`PatternSubscribeDecision::Subscribed`] outcome.
    ///
    /// # Errors
    ///
    /// Fails closed for an unknown pattern or a corrupt record.
    pub fn inspect_pattern_attachments(
        &self,
        pattern_id: PatternId,
    ) -> Result<Vec<SubscriptionRecord>, TopicAuthorityError> {
        let connection = self.lock()?;
        load_pattern_optional(&connection, pattern_id)?
            .ok_or(TopicAuthorityError::TopicPatternNotFound(pattern_id))?;
        let mut statement = connection.prepare(
            "SELECT topic_id, subscriber_key FROM topic_subscriptions
             WHERE attached_by=?1 AND active=1 ORDER BY topic_id",
        )?;
        let rows = statement
            .query_map([pattern_id.as_bytes().as_slice()], |row| {
                Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(topic_id, subscriber_key)| {
                let topic_id = TopicId::from_bytes(array16(topic_id)?);
                let subscriber_key = SubscriberKey::from_bytes(array16(subscriber_key)?);
                load_subscription_optional(&connection, topic_id, subscriber_key)?.ok_or(
                    TopicAuthorityError::CorruptRecord("attached subscription row vanished"),
                )
            })
            .collect()
    }

    /// Publishes one payload: verify-then-commit across the two authorities.
    ///
    /// 1. The immutable publication row is persisted first with status
    ///    `PENDING_ENQUEUE`, binding the topic's policy digest, payer,
    ///    payload digest and the initialized cascade budget (the Topic
    ///    authority never stores the payload body; the Channel authority is
    ///    the only message log).  The row is a level-0 root with no parent.
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
    /// Retention admission (`RSM-FANOUT-001`, the declared
    /// `retained_bytes`/`retention_ms`) runs in the step-1 `Immediate`
    /// transaction, after the idempotency gates and before the
    /// `PENDING_ENQUEUE` insert: the unconsumed backlog (channel payload
    /// bytes beyond `min(active subscriber cursors, channel consume
    /// high-water)`; a `QUARANTINED` subscriber's lag holds no budget) plus
    /// the new payload must fit `retained_bytes`, and the oldest live entry
    /// still held by an active subscriber must not be older than
    /// `retention_ms`, measured against the caller-supplied
    /// `published_at_ms`.  Either bound exceeded is
    /// [`TopicAuthorityError::TopicRetentionExhausted`] before any durable
    /// write: a rejected publish leaves zero partial state and nothing is
    /// deleted.  A resumed `PENDING_ENQUEUE` row replays without re-running
    /// the admission: its insert already passed it, and the channel's
    /// key-scoped replay converges without a duplicate enqueue.
    ///
    /// # Errors
    ///
    /// Fails closed for an empty payload, an unknown Topic, idempotency
    /// rebinding, the declared retention bounds
    /// ([`TopicAuthorityError::TopicRetentionExhausted`], before any durable
    /// write), a propagated Channel rejection (`StaleChannel`,
    /// `QueueFull`, ...) or a storage/corruption failure.  The publication
    /// row stays `PENDING_ENQUEUE` when the enqueue itself is rejected.
    pub fn publish(&self, request: PublishRequest) -> Result<PublishDecision, TopicAuthorityError> {
        let PublishRequest {
            topic_id,
            payload,
            idempotency_key,
            published_at_ms,
        } = request;
        if payload.is_empty() {
            return Err(TopicAuthorityError::InvalidPayload);
        }
        let digest = derive_payload_digest(&payload);

        // Step 1: durable PENDING row first (or replay short-circuit).
        let (topic, resumed) = {
            let mut connection = self.lock()?;
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let topic = load_topic_verified(&transaction, topic_id)?;
            if let Some(existing) = load_publication_by_key(&transaction, idempotency_key)? {
                if existing.topic_id != topic_id || existing.payload_digest != digest {
                    return Err(TopicAuthorityError::IdempotencyConflict);
                }
                if existing.status == PublicationStatus::Enqueued {
                    transaction.commit()?;
                    return Ok(PublishDecision::Replayed(existing));
                }
                transaction.commit()?;
                (topic, true)
            } else {
                // Retention admission (`RSM-FANOUT-001`): in this same
                // `Immediate` transaction, after the idempotency gates and
                // before the first durable write, so a rejected publish
                // leaves zero partial state.
                check_retention_admission(
                    &transaction,
                    &self.channel,
                    &topic,
                    published_at_ms,
                    payload.len() as u64,
                )?;
                insert_publication(
                    &transaction,
                    &topic,
                    idempotency_key,
                    digest,
                    payload.len() as u64,
                    None,
                    0,
                    published_at_ms,
                )?;
                transaction.commit()?;
                (topic, false)
            }
        };

        // Steps 2-3: fence acquisition, enqueue and the sequence-association
        // commit, shared with the cascade republish path.
        let (record, committed) =
            self.complete_enqueue(&topic, idempotency_key, &payload, published_at_ms, resumed)?;
        Ok(if committed {
            PublishDecision::Published(record)
        } else {
            PublishDecision::Replayed(record)
        })
    }

    /// Forwards one parent publication into a child topic, spending exactly
    /// one unit of the parent's cascade budget (`RSM-FANOUT-001`).
    ///
    /// Verify-then-commit across the two authorities, budget strictly first:
    ///
    /// 1. One `Immediate` topic-authority transaction: the owner reads the
    ///    parent publication row back (topic ownership, policy binding,
    ///    cascade budget, level) and audits the parent's provenance chain to
    ///    its root (a broken link or a cycle is
    ///    [`TopicAuthorityError::CorruptRecord`]); a missing parent fails
    ///    with [`TopicAuthorityError::PublicationNotFound`] and a parent that
    ///    has not reached the terminal [`PublicationStatus::Enqueued`] state
    ///    with [`TopicAuthorityError::PublicationNotEnqueued`]; the depth
    ///    bound (`parent level + 1` within the parent policy's
    ///    `cascade_depth`) is enforced pre-write
    ///    ([`TopicAuthorityError::CascadeDepthExceeded`]); the child topic's
    ///    declared retention bounds are then admitted pre-write as well
    ///    ([`TopicAuthorityError::TopicRetentionExhausted`], still before
    ///    any durable write, so a rejected republish spends no budget);
    ///    then one budget unit is spent through a guarded compare-and-set
    ///    (`UPDATE ... WHERE cascade_budget_remaining > 0`; zero affected
    ///    rows is [`TopicAuthorityError::CascadeBudgetExhausted`]) and the
    ///    child publication row is inserted `PENDING_ENQUEUE` with the parent
    ///    key and `parent level + 1` recorded.  Every rejection happens
    ///    before any write, so a failed republish leaves zero partial state.
    /// 2. The child enqueue follows the exact [`TopicAuthority::publish`]
    ///    path (fence, enqueue, `ENQUEUED` commit, crash-window convergence)
    ///    on the child topic's channel.
    ///
    /// Crash window: the budget spend (topic authority) and the child
    /// enqueue (channel authority) cannot commit atomically across
    /// authorities.  A crash after step 1 leaves the budget spent and the
    /// child `PENDING_ENQUEUE`; replaying the same idempotency key
    /// supplements the enqueue without spending again.  Replaying a key that
    /// already reached `ENQUEUED` returns the original record — the budget
    /// is never spent twice and the enqueue never duplicates; a key rebound
    /// to a different child topic, parent or payload is an
    /// [`TopicAuthorityError::IdempotencyConflict`].  Cross-authority
    /// atomicity is explicitly not claimed.
    ///
    /// # Errors
    ///
    /// Fails closed for an empty payload, an unknown child topic, a missing
    /// or non-enqueued parent, a broken or cyclic parent chain, the depth
    /// bound, the child topic's declared retention bounds
    /// ([`TopicAuthorityError::TopicRetentionExhausted`], before any durable
    /// write), an exhausted parent budget, idempotency rebinding, a
    /// propagated Channel rejection (`StaleChannel`, `QueueFull`, ...) or a
    /// storage/corruption failure.  The child row stays `PENDING_ENQUEUE`
    /// when the enqueue itself is rejected (the budget stays spent).
    #[allow(clippy::too_many_lines)] // One method owns the budget CAS + child registration sequence.
    pub fn republish(
        &self,
        request: RepublishRequest,
    ) -> Result<RepublishDecision, TopicAuthorityError> {
        let RepublishRequest {
            child_topic_id,
            parent_publication_key,
            payload,
            idempotency_key,
            republished_at_ms,
        } = request;
        if payload.is_empty() {
            return Err(TopicAuthorityError::InvalidPayload);
        }
        let digest = derive_payload_digest(&payload);

        // Step 1: one Immediate transaction — replay short-circuit, parent
        // readback and chain audit, pre-write gates, guarded budget CAS and
        // the durable PENDING child row.
        let (child_topic, resumed) = {
            let mut connection = self.lock()?;
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let child_topic = load_topic_verified(&transaction, child_topic_id)?;
            if let Some(existing) = load_publication_by_key(&transaction, idempotency_key)? {
                // A durable row for this key exists: it is either a completed
                // republish (replay) or a spent-but-pending crash window.
                if existing.topic_id != child_topic_id
                    || existing.payload_digest != digest
                    || existing.parent_publication_key != Some(parent_publication_key)
                {
                    return Err(TopicAuthorityError::IdempotencyConflict);
                }
                if existing.status == PublicationStatus::Enqueued {
                    transaction.commit()?;
                    return Ok(RepublishDecision::Replayed(existing));
                }
                // The original attempt committed the budget spend together
                // with this PENDING row; resume only the enqueue.
                transaction.commit()?;
                (child_topic, true)
            } else {
                let parent = load_publication_by_key(&transaction, parent_publication_key)?.ok_or(
                    TopicAuthorityError::PublicationNotFound(parent_publication_key),
                )?;
                let parent_topic = load_topic_verified(&transaction, parent.topic_id)?;
                if parent.policy_digest != parent_topic.policy_digest
                    || parent.payer != parent_topic.policy.payer
                {
                    return Err(TopicAuthorityError::CorruptRecord(
                        "parent publication binding disagrees with its topic head",
                    ));
                }
                if parent.status != PublicationStatus::Enqueued {
                    return Err(TopicAuthorityError::PublicationNotEnqueued(
                        parent_publication_key,
                    ));
                }
                verify_parent_chain(&transaction, &parent)?;
                let child_level = parent.cascade_level + 1;
                if child_level > parent_topic.policy.cascade_depth {
                    return Err(TopicAuthorityError::CascadeDepthExceeded {
                        parent_publication_key,
                        requested_level: child_level,
                        cascade_depth: parent_topic.policy.cascade_depth,
                    });
                }
                // Child-topic retention admission (`RSM-FANOUT-001`): the
                // same pre-write gate as publish, against the child topic's
                // declared bounds.  It stays with the other pre-write gates,
                // strictly before the budget compare-and-set (the first
                // durable write), so a rejected republish spends no budget
                // and leaves zero partial state.
                check_retention_admission(
                    &transaction,
                    &self.channel,
                    &child_topic,
                    republished_at_ms,
                    payload.len() as u64,
                )?;
                // Guarded budget CAS: exactly one unit of the parent's
                // remaining cascade budget, zero partial state on rejection.
                let changed = transaction.execute(
                    "UPDATE topic_publications
                     SET cascade_budget_remaining = cascade_budget_remaining - 1
                     WHERE idempotency_key=?1 AND cascade_budget_remaining > 0",
                    params![parent_publication_key.as_bytes().as_slice()],
                )?;
                if changed != 1 {
                    return Err(TopicAuthorityError::CascadeBudgetExhausted(
                        parent_publication_key,
                    ));
                }
                insert_publication(
                    &transaction,
                    &child_topic,
                    idempotency_key,
                    digest,
                    payload.len() as u64,
                    Some(parent_publication_key),
                    child_level,
                    republished_at_ms,
                )?;
                transaction.commit()?;
                (child_topic, false)
            }
        };

        // Step 2: the child enqueue on the child topic's channel, shared
        // with the publish path.
        let (record, committed) = self.complete_enqueue(
            &child_topic,
            idempotency_key,
            &payload,
            republished_at_ms,
            resumed,
        )?;
        Ok(if committed {
            RepublishDecision::Republished(record)
        } else {
            RepublishDecision::Replayed(record)
        })
    }

    /// Reads one publication for audit: status, cascade budget remaining,
    /// cascade level, parent provenance and channel association.
    ///
    /// Cross-checks mirror [`TopicAuthority::inspect_publications`] (the
    /// topic head binding and the sequence within the channel high-water)
    /// and add the cascade invariants: the parent chain must walk to a
    /// level-0 root with strictly decreasing levels, no cycle and no broken
    /// link, and the remaining budget must equal the topic's initialized
    /// `cascade_depth` minus the durable child publications referencing this
    /// row.
    ///
    /// # Errors
    ///
    /// Fails closed for an unknown publication key, corrupt rows, a broken
    /// or cyclic parent chain, or a propagated Channel rejection.
    pub fn inspect_publication(
        &self,
        publication_key: IdempotencyKey,
    ) -> Result<PublicationRecord, TopicAuthorityError> {
        let connection = self.lock()?;
        let record = load_publication_by_key(&connection, publication_key)?
            .ok_or(TopicAuthorityError::PublicationNotFound(publication_key))?;
        let topic = load_topic_verified(&connection, record.topic_id)?;
        if record.policy_digest != topic.policy_digest || record.payer != topic.policy.payer {
            return Err(TopicAuthorityError::CorruptRecord(
                "publication binding disagrees with the topic head",
            ));
        }
        if record.status == PublicationStatus::Enqueued {
            let queue = self
                .channel
                .inspect_queue(topic.channel_id)
                .map_err(TopicAuthorityError::Channel)?;
            if record.channel_sequence > queue.max_sequence {
                return Err(TopicAuthorityError::CorruptRecord(
                    "publication sequence exceeds the channel high-water",
                ));
            }
        }
        verify_parent_chain(&connection, &record)?;
        // The spent budget must reconcile with the durable children: every
        // referencing child row committed together with exactly one unit.
        let children: i64 = connection.query_row(
            "SELECT COUNT(*) FROM topic_publications WHERE parent_idempotency_key=?1",
            [publication_key.as_bytes().as_slice()],
            |row| row.get(0),
        )?;
        let spent = decode_u64(children)?;
        let Some(expected_remaining) = topic.policy.cascade_depth.checked_sub(spent) else {
            return Err(TopicAuthorityError::CorruptRecord(
                "cascade children exceed the initialized budget",
            ));
        };
        if record.cascade_budget_remaining != expected_remaining {
            return Err(TopicAuthorityError::CorruptRecord(
                "cascade budget disagrees with the durable child publications",
            ));
        }
        Ok(record)
    }

    /// Returns the entries this subscriber has not consumed yet.
    ///
    /// Deliberately not consumption-token gated: `poll` is a zero-write read
    /// path and presents nothing to replay or overwrite; the authenticated
    /// boundary is the cursor advance
    /// ([`TopicAuthority::advance_with_token`]), not the read.
    ///
    /// A `QUARANTINED` subscriber is rejected with the typed
    /// [`TopicAuthorityError::DeliveryQuarantined`] before any Channel read:
    /// its declared delivery attempts are exhausted, so delivery fails closed
    /// and leaks no entry (quarantine rejects the service, not the caller's
    /// identity — the entry stays readable for every other subscriber and for
    /// this one again after an explicit reinstate).
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
    /// subscription, a quarantined subscription, a cursor inconsistent with
    /// the channel high-water, or a propagated Channel rejection.
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
        if subscription.state == SubscriptionState::Quarantined {
            return Err(TopicAuthorityError::DeliveryQuarantined(
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
    /// Deprecated compatibility entry: this call authenticates the caller
    /// only by the `subscriber_key` (token-free, the pre-binding semantics),
    /// so any caller that can name the key can impersonate the subscriber
    /// and advance its consume cursor.  The identity binding is the
    /// consumption token required by [`TopicAuthority::advance_with_token`];
    /// prefer that entry.
    ///
    /// # Errors
    ///
    /// Fails closed for an unknown Topic or subscription, an inactive
    /// subscription, a regressing or out-of-range sequence, or a
    /// storage/corruption failure.
    #[deprecated(
        since = "0.1.0",
        note = "use advance_with_token; the token-free entry lets an impersonator advance the cursor"
    )]
    pub fn advance(&self, request: AdvanceRequest) -> Result<AdvanceDecision, TopicAuthorityError> {
        self.advance_inner(request, None)
    }

    /// Advances one subscriber's cursor, requiring the consumption token.
    ///
    /// Identical to [`TopicAuthority::advance`] except the caller must
    /// present the [`ConsumeToken`] issued for the subscription's current
    /// generation; any other token is
    /// [`TopicAuthorityError::ConsumptionTokenMismatch`] before any write
    /// (fail-closed, zero durable state change).  [`TopicAuthority::poll`]
    /// deliberately stays token-free: it is a zero-write read path, and the
    /// cursor advance — not the read — is the authenticated boundary.
    ///
    /// Payer metering (the ADR-0007 payer-metering addendum): inside this
    /// same transaction as the accepted cursor CAS, every ENQUEUED
    /// publication of the topic whose channel sequence lies in the crossed
    /// window `(old_cursor, new_cursor]` records one immutable `Attributed`
    /// ledger row (payer = the publication row's payer, bytes = the recorded
    /// payload length, evidence = the sequence,
    /// [`ATTRIBUTION_POLICY_VERSION`] frozen into the row) — zero crossed
    /// publications record zero rows and the advance still succeeds.  A
    /// sequence is attributed at most once (first-accounting-event-wins), so
    /// overlapping advances by several subscribers never double-count a
    /// publication, and an already-deleted uncovered sequence (at or below
    /// the channel trim watermark) is left to
    /// [`TopicAuthority::inspect_attribution`] to surface instead of being
    /// misattributed as delivered.  The replay path (repeating the exact
    /// current cursor) returns the stored decision and writes no ledger row.
    /// Subscriptions attached by a pattern are ordinary rows: their advances
    /// account exactly like direct ones.
    ///
    /// # Errors
    ///
    /// Fails closed for a token mismatch, an unknown Topic or subscription,
    /// an inactive subscription, a regressing or out-of-range sequence, or a
    /// storage/corruption failure.
    pub fn advance_with_token(
        &self,
        request: AdvanceRequest,
        consume_token: &ConsumeToken,
    ) -> Result<AdvanceDecision, TopicAuthorityError> {
        self.advance_inner(request, Some(*consume_token))
    }

    fn advance_inner(
        &self,
        request: AdvanceRequest,
        consume_token: Option<ConsumeToken>,
    ) -> Result<AdvanceDecision, TopicAuthorityError> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let topic = load_topic_verified(&transaction, request.topic_id)?;
        let subscription =
            load_subscription_optional(&transaction, request.topic_id, request.subscriber_key)?
                .ok_or(TopicAuthorityError::SubscriptionNotFound(
                    subscription_id_for(request.topic_id, request.subscriber_key),
                ))?;
        verify_consume_token(&subscription, consume_token)?;
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
        // Payer metering accounting point (`RSM-METER-002` minimal prefix):
        // the `Attributed` ledger rows for the crossed window commit in this
        // same transaction as the accepted cursor CAS — an advance and its
        // attribution are one durable fact.
        record_advance_attribution(
            &transaction,
            request.topic_id,
            subscription.cursor,
            request.up_to_sequence,
            queue.trim_high_water,
            request.advanced_at_ms,
        )?;
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
    /// Payer metering (the ADR-0007 payer-metering addendum): after the
    /// channel decision, a topic-authority transaction records one immutable
    /// `Unallocated` ledger row for every ENQUEUED publication at or below
    /// the effective trim watermark that no ledger row covers yet — bytes
    /// the log deleted without ever delivering (an isolated or lagging
    /// subscriber's residual; a subscription that cancelled or unsubscribed
    /// never records anything itself).  Rows are payer = the publication
    /// row's payer, bytes = the recorded payload length, evidence = the
    /// sequence; `recorded_at_ms` is the `0` marker because the compact
    /// entry carries no caller time (the crate never reads a wall clock).
    /// A compact the channel replays records nothing new — but still runs
    /// the same coverage pass, so a crash window between the channel trim
    /// and a previous attempt's ledger transaction is healed by re-running
    /// the compact (the pass is idempotent: covered sequences are skipped).
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
        let channel_decision = self
            .channel
            .compact(channel_id, target)
            .map_err(TopicAuthorityError::Channel)?;
        // Payer metering accounting point (`RSM-METER-002` minimal prefix):
        // the channel has accepted the watermark (freshly trimmed or
        // replayed), so everything at or below its trim watermark is
        // deleted-or-deleting and every uncovered publication in that prefix
        // is an `Unallocated` fact.  Same coverage pass on both arms —
        // a replaying compact writes no new rows but heals a crash window.
        {
            let mut connection = self.lock()?;
            let transaction =
                connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
            record_unallocated_prefix(
                &transaction,
                topic_id,
                channel_decision.receipt().trim_high_water,
            )?;
            transaction.commit()?;
        }
        Ok(match channel_decision {
            ChannelCompactDecision::Trimmed(receipt) => {
                TopicCompactDecision::Trimmed(wrap_compact(topic_id, channel_id, target, receipt))
            }
            ChannelCompactDecision::Replayed(receipt) => {
                TopicCompactDecision::Replayed(wrap_compact(topic_id, channel_id, target, receipt))
            }
        })
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
                    cascade_level, published_at_ms, enqueued_at_ms, parent_idempotency_key
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
                    row.get::<_, i64>(10)?,
                    row.get::<_, Option<Vec<u8>>>(11)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|row| {
                let key = IdempotencyKey::from_bytes(array16(row.0)?);
                let parent = row
                    .11
                    .map(|bytes| array16(bytes).map(IdempotencyKey::from_bytes))
                    .transpose()?;
                let record = decode_publication(
                    topic_id,
                    key,
                    parent,
                    (
                        row.1, row.2, row.3, row.4, row.5, row.6, row.7, row.8, row.9, row.10,
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

    /// Reconciles the payer metering ledger against the publication journal
    /// for one topic (the ADR-0007 payer-metering addendum,
    /// `RSM-METER-002`): byte-exact, per-publication coverage.  The report
    /// semantics — the reconciliation identity and what counts as unsettled
    /// — are documented on [`AttributionReport`].
    ///
    /// Every ledger row is cross-checked against the publication it
    /// accounts: the frozen policy version is the current
    /// [`ATTRIBUTION_POLICY_VERSION`], the kind decodes, the derived
    /// identity re-derives, and bytes and payer equal the publication row's.
    /// The publication side must have no uncovered sequence at or below the
    /// channel trim watermark (deleted bytes no accounting event ever
    /// covered — a bypassed or interrupted compaction is indistinguishable
    /// from corruption here and fails closed; re-running the topic compact
    /// heals the interrupted case).
    ///
    /// # Errors
    ///
    /// Fails closed for an unknown Topic, a propagated Channel rejection, or
    /// any ledger/journal disagreement
    /// ([`TopicAuthorityError::CorruptRecord`]) — never returns an
    /// unbalanced report.
    #[allow(clippy::too_many_lines)] // One method owns the whole journal-vs-ledger cross-check.
    pub fn inspect_attribution(
        &self,
        topic_id: TopicId,
    ) -> Result<AttributionReport, TopicAuthorityError> {
        let connection = self.lock()?;
        let topic = load_topic_verified(&connection, topic_id)?;
        let queue = self
            .channel
            .inspect_queue(topic.channel_id)
            .map_err(TopicAuthorityError::Channel)?;
        let mut statement = connection.prepare(
            "SELECT channel_sequence, payer_account_id, payload_bytes
             FROM topic_publications
            WHERE topic_id=?1 AND status=1
            ORDER BY channel_sequence",
        )?;
        let publications = statement
            .query_map([topic_id.as_bytes().as_slice()], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        let mut statement = connection.prepare(
            "SELECT ledger_id, payer_account_id, kind, payload_bytes,
                    policy_version, evidence_sequence
             FROM topic_attribution_ledger
            WHERE topic_id=?1
            ORDER BY evidence_sequence",
        )?;
        let ledger = statement
            .query_map([topic_id.as_bytes().as_slice()], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);

        let mut journal: BTreeMap<u64, ([u8; 16], u64, bool)> = BTreeMap::new();
        let mut total: u64 = 0;
        for (sequence, payer, payload_bytes) in publications {
            let sequence = decode_u64(sequence)?;
            let payload_bytes = decode_u64(payload_bytes)?;
            if journal
                .insert(sequence, (array16(payer)?, payload_bytes, false))
                .is_some()
            {
                return Err(TopicAuthorityError::CorruptRecord(
                    "two enqueued publications bind one channel sequence",
                ));
            }
            total = total
                .checked_add(payload_bytes)
                .ok_or(TopicAuthorityError::CorruptRecord(
                    "publication journal byte total overflows",
                ))?;
        }
        let mut attributed_bytes: u64 = 0;
        let mut unallocated_bytes: u64 = 0;
        for (ledger_id, payer, kind, payload_bytes, policy_version, evidence_sequence) in ledger {
            if decode_u64(policy_version)? != ATTRIBUTION_POLICY_VERSION {
                return Err(TopicAuthorityError::CorruptRecord(
                    "attribution ledger row carries an unknown policy version",
                ));
            }
            let kind = AttributionKind::from_code(kind).ok_or(
                TopicAuthorityError::CorruptRecord("attribution ledger row kind is unknown"),
            )?;
            let evidence_sequence = decode_u64(evidence_sequence)?;
            let payload_bytes = decode_u64(payload_bytes)?;
            if ledger_id.as_slice()
                != attribution_ledger_id(topic_id, kind.code(), evidence_sequence).as_slice()
            {
                return Err(TopicAuthorityError::CorruptRecord(
                    "attribution ledger identity disagrees with the derived identity",
                ));
            }
            let entry =
                journal
                    .get_mut(&evidence_sequence)
                    .ok_or(TopicAuthorityError::CorruptRecord(
                        "attribution ledger row binds no enqueued publication",
                    ))?;
            if entry.2 {
                return Err(TopicAuthorityError::CorruptRecord(
                    "one publication is covered by two ledger rows",
                ));
            }
            if entry.0 != array16(payer)? {
                return Err(TopicAuthorityError::CorruptRecord(
                    "attribution ledger payer disagrees with the publication payer",
                ));
            }
            if entry.1 != payload_bytes {
                return Err(TopicAuthorityError::CorruptRecord(
                    "attribution ledger bytes disagree with the publication payload length",
                ));
            }
            entry.2 = true;
            let sum = match kind {
                AttributionKind::Attributed => &mut attributed_bytes,
                AttributionKind::Unallocated => &mut unallocated_bytes,
            };
            *sum = sum
                .checked_add(payload_bytes)
                .ok_or(TopicAuthorityError::CorruptRecord(
                    "attribution ledger byte total overflows",
                ))?;
        }
        let mut unsettled_bytes: u64 = 0;
        for (sequence, (_, payload_bytes, covered)) in &journal {
            if *covered {
                continue;
            }
            if *sequence <= queue.trim_high_water {
                return Err(TopicAuthorityError::CorruptRecord(
                    "uncovered publication lies at or below the channel trim watermark",
                ));
            }
            unsettled_bytes = unsettled_bytes.checked_add(*payload_bytes).ok_or(
                TopicAuthorityError::CorruptRecord("unsettled publication byte total overflows"),
            )?;
        }
        let balanced = attributed_bytes
            .checked_add(unallocated_bytes)
            .and_then(|sum| sum.checked_add(unsettled_bytes))
            .is_some_and(|sum| sum == total);
        if !balanced {
            return Err(TopicAuthorityError::CorruptRecord(
                "attribution ledger does not reconcile with the publication journal",
            ));
        }
        Ok(AttributionReport {
            topic_id,
            attributed_bytes,
            unallocated_bytes,
            unsettled_bytes,
            total,
            policy_version: ATTRIBUTION_POLICY_VERSION,
            balanced: true,
        })
    }

    /// Completes the channel side of a durably registered publication: fence
    /// acquisition, [`ChannelAuthority::enqueue`] and the `ENQUEUED`
    /// sequence-association commit.  Returns the final record and whether
    /// this call performed the commit (a concurrent completion of the same
    /// key converges onto its record instead).
    ///
    /// A fresh publication enqueues against the fence bound on the topic head
    /// so a rotation is observable as `StaleChannel`; a resumed `PENDING`
    /// publication re-reads the Channel head live and re-binds before
    /// re-issuing the enqueue (the documented re-read convergence after a
    /// propagated failure).
    fn complete_enqueue(
        &self,
        topic: &TopicRecord,
        idempotency_key: IdempotencyKey,
        payload: &[u8],
        enqueued_at_ms: u64,
        resumed: bool,
    ) -> Result<(PublicationRecord, bool), TopicAuthorityError> {
        let (expected_generation, expected_fencing_token) = if resumed {
            let live = self
                .channel
                .inspect_channel(topic.channel_id)
                .map_err(TopicAuthorityError::Channel)?;
            self.rebind_channel_fence(topic, &live)?;
            (live.generation, live.fencing_token)
        } else {
            (topic.channel_generation, topic.channel_fencing_token)
        };

        // Typed rejections propagate without silent retry.  A `Replayed`
        // channel decision is the crash window: a prior attempt already
        // enqueued this key, so its record is the association to keep.
        let entry = match self
            .channel
            .enqueue(EnqueueRequest {
                channel_id: topic.channel_id,
                expected_generation,
                expected_fencing_token,
                payload: payload.to_vec(),
                idempotency_key,
                enqueued_at_ms,
            })
            .map_err(TopicAuthorityError::Channel)?
        {
            EnqueueDecision::Enqueued(entry) | EnqueueDecision::Replayed(entry) => entry,
        };

        // Commit the sequence association.  A row migrated from a pre-v5
        // schema carries the `0` payload-length sentinel (its length was
        // never recorded); the enqueue-commit transaction heals it with the
        // true length of the payload being enqueued — the only legal
        // sentinel transition, and the reason the commit UPDATE, not just
        // the insert, carries the column.
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE topic_publications
             SET status=1, channel_sequence=?1, channel_generation=?2,
                 enqueued_at_ms=?3,
                 payload_bytes=CASE WHEN payload_bytes=0 THEN ?4 ELSE payload_bytes END
             WHERE idempotency_key=?5 AND status=0",
            params![
                encode_u64(entry.sequence)?,
                encode_u64(entry.generation.get())?,
                encode_u64(entry.enqueued_at_ms)?,
                encode_u64(payload.len() as u64)?,
                idempotency_key.as_bytes().as_slice(),
            ],
        )?;
        if changed != 1 {
            // A concurrent call completed the same key first; converge to
            // its record or fail closed on disagreement.
            let existing = load_publication_by_key(&transaction, idempotency_key)?.ok_or(
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
            return Ok((existing, false));
        }
        // Delivery-attempt billing point (`RSM-FANOUT-001`, same
        // topic-authority transaction as the enqueue commit): every ACTIVE
        // subscription whose cursor is behind the pre-enqueue sequence
        // high-water (`cursor < sequence - 1`) was genuinely lagging when
        // this publication arrived and is billed one durable unit; a fully
        // caught-up subscriber receives a first delivery and is not billed.
        // `QUARANTINED` subscribers stopped receiving deliveries and are not
        // billed again, and nothing is deleted: the flip only stops
        // delivery.  The converged path above must not bill — the
        // committing transaction already did.
        let lag_bound = entry
            .sequence
            .checked_sub(1)
            .ok_or(TopicAuthorityError::CorruptRecord(
                "enqueued publication binds no channel sequence",
            ))?;
        transaction.execute(
            "UPDATE topic_subscriptions
             SET redelivery_used = redelivery_used + 1,
                 state = CASE WHEN redelivery_used + 1 >= ?1 THEN 1 ELSE state END,
                 quarantined_at_ms = CASE WHEN redelivery_used + 1 >= ?1
                                          THEN ?2 ELSE quarantined_at_ms END
             WHERE topic_id=?3 AND active=1 AND state=0 AND cursor < ?4",
            params![
                encode_u64(topic.policy.delivery_attempts)?,
                encode_u64(entry.enqueued_at_ms)?,
                topic.topic_id.as_bytes().as_slice(),
                encode_u64(lag_bound)?,
            ],
        )?;
        let record = load_publication_by_key(&transaction, idempotency_key)?.ok_or(
            TopicAuthorityError::CorruptRecord("publication row vanished after enqueue commit"),
        )?;
        transaction.commit()?;
        Ok((record, true))
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

/// Retention admission for one prospective publication (`RSM-FANOUT-001`,
/// ADR-0007 retention addendum): the topic's declared
/// `retained_bytes`/`retention_ms` bounds are enforced as publish-side
/// backpressure before any durable write, inside the caller's `Immediate`
/// transaction (the same one that read the policy), so a rejected
/// publication leaves zero partial state and nothing is ever deleted here.
///
/// Byte bound: the unconsumed backlog is measured exactly as the ADR-0007
/// addendum defines it — `Σ payload_bytes` over the enqueued
/// [`crate::PublicationRecord`] rows whose channel sequence lies beyond the
/// same release point [`TopicAuthority::compact_bound`] uses —
/// `min(active subscriber cursors, channel consume high-water)`, falling
/// back to the consume high-water when no active subscriber holds the log.
/// The per-row payload length is durable metadata recorded with the
/// publication itself since schema v5 (never a message-body copy: the body
/// stays in the Channel log), so the sum is available for any sequence
/// window — including the window a live subscriber shadows below the
/// channel consume point, which the channel-side `inspect_queue` byte
/// counters cannot expose.  A subscriber whose delivery attempts are
/// exhausted (`QUARANTINED`) has stopped receiving deliveries and holds no
/// retention budget: its lag is excluded, orthogonal to the
/// delivery-attempts mechanism that isolated it.  Rows recorded before
/// schema v5 carry the `0` length sentinel (enqueue rejects empty payloads,
/// so `0` unambiguously means "length unknown"); when such a row falls in
/// the window the measurement switches to the mixed mode: the exact
/// known-row sum merged with the channel-side total live retained bytes
/// (an upper bound on every live entry, this topic's or not) by taking the
/// larger of the two — a value that never understates the ADR backlog
/// while any sentinel row is live in the window.  Sentinel rows leave the
/// window as catch-up and compaction advance the bound past them (they die
/// with compaction), restoring [`RetentionBacklogPrecision::Exact`]; the
/// mode actually used is reported on the rejection as
/// `backlog_precision`.
///
/// Time bound: the oldest still-live entry held by an active subscriber
/// (its channel sequence beyond that cursor and beyond the channel trim
/// prefix) must not be older than the declared `retention_ms`, measured
/// with the entry's `enqueued_at_ms` against the caller-supplied request
/// time — never a wall clock.
///
/// The byte bound is checked first, then the time bound; either failing
/// returns [`TopicAuthorityError::TopicRetentionExhausted`] carrying both
/// declared bounds and the measured values.
fn check_retention_admission(
    transaction: &Transaction<'_>,
    channel: &ChannelAuthority,
    topic: &TopicRecord,
    request_at_ms: u64,
    payload_bytes: u64,
) -> Result<(), TopicAuthorityError> {
    // Only subscribers still receiving deliveries hold retention budget: a
    // `QUARANTINED` subscription has stopped consuming by policy, and its
    // lag is the delivery-attempts mechanism's concern, not retention's.
    let min_live: Option<Option<i64>> = transaction
        .query_row(
            "SELECT MIN(cursor) FROM topic_subscriptions
             WHERE topic_id=?1 AND active=1 AND state=0",
            [topic.topic_id.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .optional()?;
    let min_active_cursor = min_live.flatten().map(decode_u64).transpose()?;
    let queue = channel
        .inspect_queue(topic.channel_id)
        .map_err(TopicAuthorityError::Channel)?;
    // Same release-point trade-off as `compact_bound`: with no active
    // subscriber the channel consume high-water alone releases the log.
    let bound = min_active_cursor.map_or(queue.consume_high_water, |cursor| {
        cursor.min(queue.consume_high_water)
    });
    // Exact ADR-0007 backlog: the payload length was recorded durably with
    // each enqueued publication (schema v5), so any sequence window —
    // including the one a live subscriber shadows below the channel consume
    // point — sums without touching the channel log.  Legacy rows carry the
    // `0` sentinel; their count selects the mixed mode below.
    let (summed_bytes, sentinel_rows): (i64, i64) = transaction.query_row(
        "SELECT COALESCE(SUM(payload_bytes), 0),
                COALESCE(SUM(payload_bytes = 0), 0)
           FROM topic_publications
          WHERE topic_id=?1 AND status=1 AND channel_sequence > ?2",
        params![topic.topic_id.as_bytes().as_slice(), encode_u64(bound)?,],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let measured_bytes = decode_u64(summed_bytes)?;
    let (backlog_bytes, backlog_precision) = if decode_u64(sentinel_rows)? == 0 {
        (measured_bytes, RetentionBacklogPrecision::Exact)
    } else {
        // Mixed mode: sentinel rows' true per-entry bytes were never
        // recorded, so the sum over known rows alone would understate the
        // backlog.  The channel-side total live retained bytes upper-bound
        // every live entry's bytes (including consumed-but-untrimmed ones
        // and entries of other topics on the same channel), while the
        // known-row sum stays exact for rows the channel has already
        // trimmed — taking the larger of the two never understates the
        // ADR backlog.  A sentinel row already trimmed from the channel is
        // the one residual gap: its length is unrecoverable from either
        // side, so the merge bounds everything but it.
        (
            measured_bytes.max(queue.retained_bytes),
            RetentionBacklogPrecision::LegacyConservative,
        )
    };
    let bytes_exhausted = match backlog_bytes.checked_add(payload_bytes) {
        Some(total) => total > topic.policy.retained_bytes,
        None => true,
    };
    // The time bound needs a holder: an entry nobody actively subscribed is
    // holding exerts byte pressure only, never time pressure.
    let oldest_held_enqueued_at_ms: Option<i64> = match min_active_cursor {
        None => None,
        Some(min_cursor) => {
            // Held by an active subscriber (beyond its cursor) and still
            // live (beyond the channel trim prefix).
            let held_bound = min_cursor.max(queue.trim_high_water);
            transaction.query_row(
                "SELECT MIN(enqueued_at_ms) FROM topic_publications
                     WHERE topic_id=?1 AND status=1 AND channel_sequence > ?2",
                params![
                    topic.topic_id.as_bytes().as_slice(),
                    encode_u64(held_bound)?,
                ],
                |row| row.get(0),
            )?
        }
    };
    let oldest_unconsumed_age_ms = oldest_held_enqueued_at_ms
        .map(decode_u64)
        .transpose()?
        .map_or(0, |enqueued_at_ms| {
            request_at_ms.saturating_sub(enqueued_at_ms)
        });
    let time_exhausted = oldest_unconsumed_age_ms > topic.policy.retention_ms;
    if bytes_exhausted || time_exhausted {
        return Err(TopicAuthorityError::TopicRetentionExhausted {
            topic_id: topic.topic_id,
            retained_bytes_declared: topic.policy.retained_bytes,
            backlog_bytes,
            backlog_precision,
            payload_bytes,
            retention_ms_declared: topic.policy.retention_ms,
            oldest_unconsumed_age_ms,
        });
    }
    Ok(())
}

/// Derives the ledger row identity (the crate's authority-derived-identity
/// discipline, under its own domain tag): domain-separated over the topic,
/// the kind and the evidence sequence, so a row is self-verifying at
/// inspection time.  Call-level replay idempotency does not need the
/// calling key in the hash: the advance replay path and the replaying
/// compact write no rows at all, and the table-level
/// `UNIQUE(topic_id, evidence_sequence)` makes a second accounting event
/// for one sequence structurally impossible.
fn attribution_ledger_id(topic_id: TopicId, kind: i64, evidence_sequence: u64) -> [u8; 16] {
    derive_id(
        b"nlos/topic/attribution-ledger/id/v1",
        &[
            topic_id.as_bytes(),
            &kind.to_be_bytes(),
            &evidence_sequence.to_be_bytes(),
        ],
    )
}

/// One accounted publication selected by an accounting point: the channel
/// sequence, the publication row's payer and its recorded payload length.
type AccountedPublication = (i64, Vec<u8>, i64);

/// Records one immutable ledger row per accounted publication (the shared
/// write of both accounting points).
fn insert_ledger_rows(
    transaction: &Transaction<'_>,
    topic_id: TopicId,
    kind: AttributionKind,
    publications: Vec<AccountedPublication>,
    recorded_at_ms: u64,
) -> Result<(), TopicAuthorityError> {
    for (sequence, payer, payload_bytes) in publications {
        let sequence = decode_u64(sequence)?;
        transaction.execute(
            "INSERT INTO topic_attribution_ledger (
                ledger_id, topic_id, payer_account_id, kind, payload_bytes,
                policy_version, evidence_sequence, recorded_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                attribution_ledger_id(topic_id, kind.code(), sequence).as_slice(),
                topic_id.as_bytes().as_slice(),
                payer.as_slice(),
                kind.code(),
                payload_bytes,
                encode_u64(ATTRIBUTION_POLICY_VERSION)?,
                encode_u64(sequence)?,
                encode_u64(recorded_at_ms)?,
            ],
        )?;
    }
    Ok(())
}

/// The `Attributed` accounting point (the ADR-0007 payer-metering
/// addendum): one ledger row per ENQUEUED publication of the topic whose
/// channel sequence the accepted advance just crossed
/// (`(old_cursor, new_cursor]`), recorded inside the caller's advance
/// transaction.  First-accounting-event-wins: a sequence already covered by
/// any ledger row (attributed by an earlier advance — possibly another
/// subscriber's — or unallocated by a compaction) is skipped, so
/// overlapping subscriber advances never double-count.  Sequences at or
/// below the channel trim watermark are skipped as well: their entries are
/// already deleted, so attributing them as delivered would lauder an
/// uncovered hole that [`TopicAuthority::inspect_attribution`] must
/// surface.  Zero crossed publications record zero rows.
fn record_advance_attribution(
    transaction: &Transaction<'_>,
    topic_id: TopicId,
    old_cursor: u64,
    new_cursor: u64,
    channel_trim_high_water: u64,
    recorded_at_ms: u64,
) -> Result<(), TopicAuthorityError> {
    let mut statement = transaction.prepare(
        "SELECT channel_sequence, payer_account_id, payload_bytes
         FROM topic_publications
        WHERE topic_id=?1 AND status=1
          AND channel_sequence>?2 AND channel_sequence<=?3
          AND channel_sequence>?4
          AND NOT EXISTS (
                SELECT 1 FROM topic_attribution_ledger
                 WHERE topic_id=topic_publications.topic_id
                   AND evidence_sequence=topic_publications.channel_sequence)
        ORDER BY channel_sequence",
    )?;
    let publications = statement
        .query_map(
            params![
                topic_id.as_bytes().as_slice(),
                encode_u64(old_cursor)?,
                encode_u64(new_cursor)?,
                encode_u64(channel_trim_high_water)?,
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    insert_ledger_rows(
        transaction,
        topic_id,
        AttributionKind::Attributed,
        publications,
        recorded_at_ms,
    )
}

/// The `Unallocated` accounting point (the ADR-0007 payer-metering
/// addendum): one ledger row per ENQUEUED publication of the topic at or
/// below the channel trim watermark that no ledger row covers yet — log
/// bytes deleted without ever being delivered.  Recorded inside the
/// caller's compact transaction after the channel accepted the watermark;
/// idempotent (covered sequences are skipped), so re-running a compact
/// heals a crash window between the channel trim and the ledger write, and
/// a replaying compact records nothing new.
fn record_unallocated_prefix(
    transaction: &Transaction<'_>,
    topic_id: TopicId,
    channel_trim_high_water: u64,
) -> Result<(), TopicAuthorityError> {
    let mut statement = transaction.prepare(
        "SELECT channel_sequence, payer_account_id, payload_bytes
         FROM topic_publications
        WHERE topic_id=?1 AND status=1
          AND channel_sequence<=?2
          AND NOT EXISTS (
                SELECT 1 FROM topic_attribution_ledger
                 WHERE topic_id=topic_publications.topic_id
                   AND evidence_sequence=topic_publications.channel_sequence)
        ORDER BY channel_sequence",
    )?;
    let publications = statement
        .query_map(
            params![
                topic_id.as_bytes().as_slice(),
                encode_u64(channel_trim_high_water)?,
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    // The compact entry carries no caller time and the crate never reads a
    // wall clock: `recorded_at_ms` stays at the `0` marker for this
    // accounting point.
    insert_ledger_rows(
        transaction,
        topic_id,
        AttributionKind::Unallocated,
        publications,
        0,
    )
}

/// Validates a pattern (the precise addendum language): a non-empty byte
/// string with at most one `*`, and a `*` only as the final byte.  A
/// trailing `*` is a tail wildcard (the matched suffix may be empty); any
/// `*` elsewhere — in the middle, at the start, or a second one — and the
/// empty string are rejections.  Matching is byte-wise.
fn validate_pattern(pattern: &[u8]) -> Result<(), TopicAuthorityError> {
    if pattern.is_empty() {
        return Err(TopicAuthorityError::InvalidPattern(
            "pattern must be a non-empty byte string",
        ));
    }
    if let Some(star) = pattern.iter().position(|byte| *byte == b'*') {
        if star + 1 != pattern.len() {
            return Err(TopicAuthorityError::InvalidPattern(
                "a wildcard may only be the final byte",
            ));
        }
        if pattern[..star].contains(&b'*') {
            return Err(TopicAuthorityError::InvalidPattern(
                "pattern binds at most one wildcard",
            ));
        }
    }
    Ok(())
}

/// The single matching predicate: byte-equality for an exact pattern, or the
/// byte-prefix test for a trailing-`*` pattern (the empty suffix matches, so
/// `prefix*` also matches the bare `prefix` name and `*` matches everything).
fn pattern_matches(pattern: &[u8], name: &[u8]) -> bool {
    match pattern.split_last() {
        Some((b'*', prefix)) => name.starts_with(prefix),
        _ => pattern == name,
    }
}

/// Attempts to attach one pattern subscriber to one matching topic: the
/// shared core of both attach time points.  Mirrors
/// [`TopicAuthority::subscribe`] exactly — the same replay skip for an
/// already-active key, the same channel readback for the subscribe point,
/// the same `max_recipients` admission and the same re-activation CAS — and
/// additionally records the pattern provenance on the concrete row.
fn attach_one(
    transaction: &Transaction<'_>,
    channel: &ChannelAuthority,
    topic: &TopicRecord,
    pattern: &PatternRecord,
    subscribed_at_ms: u64,
) -> Result<Result<SubscriptionRecord, AttachSkipReason>, TopicAuthorityError> {
    let previous = load_subscription_optional(transaction, topic.topic_id, pattern.subscriber_key)?;
    if previous.as_ref().is_some_and(|existing| existing.active) {
        return Ok(Err(AttachSkipReason::AlreadySubscribed));
    }
    let live = channel
        .inspect_queue(topic.channel_id)
        .map_err(TopicAuthorityError::Channel)?;
    let active = count_active_subscriptions(transaction, topic.topic_id)?;
    if active >= topic.policy.max_recipients {
        return Ok(Err(AttachSkipReason::RecipientLimitReached));
    }
    let subscription_id = subscription_id_for(topic.topic_id, pattern.subscriber_key);
    let subscription_generation = previous
        .as_ref()
        .map_or(1, |existing| existing.subscription_generation + 1);
    let record = SubscriptionRecord {
        subscription_id,
        topic_id: topic.topic_id,
        subscriber_key: pattern.subscriber_key,
        active: true,
        cursor: live.max_sequence,
        subscribed_at_ms,
        unsubscribed_at_ms: 0,
        last_advanced_at_ms: 0,
        consume_token: derive_consume_token(subscription_id, subscription_generation),
        subscription_generation,
        state: SubscriptionState::Active,
        redelivery_used: 0,
        quarantined_at_ms: 0,
        reinstated_at_ms: 0,
        attached_by: Some(pattern.pattern_id),
    };
    insert_or_resubscribe(transaction, &record, Some(pattern.pattern_id))?;
    bump_active_count(transaction, topic.topic_id, active, active + 1)?;
    Ok(Ok(record))
}

/// The subscribe-time attach enumeration: every existing topic whose name
/// matches the pattern is offered to [`attach_one`], and every attachment
/// and skip is reported verbatim.  Topics are visited in id order so the
/// report is deterministic.
fn attach_pattern_to_existing_topics(
    transaction: &Transaction<'_>,
    channel: &ChannelAuthority,
    pattern: &PatternRecord,
    subscribed_at_ms: u64,
) -> Result<AttachReport, TopicAuthorityError> {
    let mut statement = transaction.prepare("SELECT topic_id FROM topics ORDER BY topic_id")?;
    let topic_ids = statement
        .query_map([], |row| row.get::<_, Vec<u8>>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let mut report = AttachReport::default();
    for id in topic_ids {
        let topic_id = TopicId::from_bytes(array16(id)?);
        let topic = load_topic_verified(transaction, topic_id)?;
        if !pattern_matches(&pattern.pattern, &topic.name) {
            continue;
        }
        match attach_one(transaction, channel, &topic, pattern, subscribed_at_ms)? {
            Ok(subscription) => report.attached.push(AttachedSubscription {
                topic_id,
                subscription,
            }),
            Err(reason) => report.skipped.push(AttachSkipped { topic_id, reason }),
        }
    }
    Ok(report)
}

/// The create-time attach enumeration: every `ACTIVE` pattern row whose
/// pattern matches the freshly created topic's name is offered to
/// [`attach_one`].  The attachments are observable through the
/// subscriptions' `attached_by` provenance and
/// [`TopicAuthority::inspect_pattern_attachments`]; create-time skips follow
/// the same admission rules but are not recorded separately (documented
/// minimal-observation choice).
fn attach_topic_to_matching_patterns(
    transaction: &Transaction<'_>,
    channel: &ChannelAuthority,
    topic: &TopicRecord,
    attached_at_ms: u64,
) -> Result<(), TopicAuthorityError> {
    for pattern in load_active_patterns(transaction)? {
        if !pattern_matches(&pattern.pattern, &topic.name) {
            continue;
        }
        // Skips follow the same admission rules as the subscribe-time
        // enumeration and are not recorded separately at this time point
        // (documented minimal-observation choice).
        let _attachment = attach_one(transaction, channel, topic, &pattern, attached_at_ms)?;
    }
    Ok(())
}

/// Unsubscribes every active concrete subscription carrying the pattern's
/// `attached_by` provenance, in topic-id order, using the ordinary
/// unsubscribe semantics (active-bit CAS plus the topic's
/// active-subscription counter decrement) — the pattern-cancel detach path.
fn detach_attached_subscriptions(
    transaction: &Transaction<'_>,
    pattern_id: PatternId,
    unsubscribed_at_ms: u64,
) -> Result<Vec<DetachReceipt>, TopicAuthorityError> {
    let mut statement = transaction.prepare(
        "SELECT topic_id, subscriber_key FROM topic_subscriptions
         WHERE attached_by=?1 AND active=1 ORDER BY topic_id",
    )?;
    let rows = statement
        .query_map([pattern_id.as_bytes().as_slice()], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let mut detached = Vec::new();
    for (topic_id, subscriber_key) in rows {
        let topic_id = TopicId::from_bytes(array16(topic_id)?);
        let subscriber_key = SubscriberKey::from_bytes(array16(subscriber_key)?);
        let subscription = load_subscription_optional(transaction, topic_id, subscriber_key)?
            .ok_or(TopicAuthorityError::CorruptRecord(
                "attached subscription row vanished",
            ))?;
        if !subscription.active {
            return Err(TopicAuthorityError::CorruptRecord(
                "attached subscription active-bit disagrees with the detach enumeration",
            ));
        }
        load_topic_verified(transaction, topic_id)?;
        let active = count_active_subscriptions(transaction, topic_id)?;
        let changed = transaction.execute(
            "UPDATE topic_subscriptions
             SET active=0, unsubscribed_at_ms=?1
             WHERE subscription_id=?2 AND active=1",
            params![
                encode_u64(unsubscribed_at_ms)?,
                subscription.subscription_id.as_bytes().as_slice(),
            ],
        )?;
        if changed != 1 {
            return Err(TopicAuthorityError::CorruptRecord(
                "subscription active-bit CAS lost",
            ));
        }
        bump_active_count(transaction, topic_id, active, active - 1)?;
        detached.push(DetachReceipt {
            topic_id,
            receipt: UnsubscribeReceipt {
                subscription_id: subscription.subscription_id,
                topic_id,
                subscriber_key,
                unsubscribed_at_ms,
            },
        });
    }
    Ok(detached)
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
/// columns: policy digest, payer, payload digest, status, sequence,
/// generation, budget, level, and the two timestamps, in that order.
type PublicationRow = (Vec<u8>, Vec<u8>, Vec<u8>, i64, i64, i64, i64, i64, i64, i64);

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
    attached_by: Option<PatternId>,
) -> Result<(), TopicAuthorityError> {
    let changed = transaction.execute(
        "INSERT INTO topic_subscriptions (
            subscription_id, topic_id, subscriber_key, active, cursor,
            subscribed_at_ms, unsubscribed_at_ms, last_advanced_at_ms,
            consume_token, subscription_generation, state, redelivery_used,
            quarantined_at_ms, reinstated_at_ms, attached_by
         ) VALUES (?1, ?2, ?3, 1, ?4, ?5, 0, 0, ?6, ?7, 0, 0, 0, 0, ?8)
         ON CONFLICT(topic_id, subscriber_key) DO UPDATE SET
            active=1, cursor=excluded.cursor,
            subscribed_at_ms=excluded.subscribed_at_ms,
            unsubscribed_at_ms=0, last_advanced_at_ms=0,
            consume_token=excluded.consume_token,
            subscription_generation=excluded.subscription_generation,
            state=0, redelivery_used=0, quarantined_at_ms=0, reinstated_at_ms=0,
            attached_by=excluded.attached_by
         WHERE topic_subscriptions.active=0",
        params![
            record.subscription_id.as_bytes().as_slice(),
            record.topic_id.as_bytes().as_slice(),
            record.subscriber_key.as_bytes().as_slice(),
            encode_u64(record.cursor)?,
            encode_u64(record.subscribed_at_ms)?,
            record.consume_token.as_slice(),
            encode_u64(record.subscription_generation)?,
            attached_by
                .as_ref()
                .map(|pattern_id| pattern_id.as_bytes().as_slice()),
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

// One argument past the clippy default: the payload digest and its cached
// length travel as separate typed metadata, like every other journal field.
#[allow(clippy::too_many_arguments)]
fn insert_publication(
    transaction: &Transaction<'_>,
    topic: &TopicRecord,
    idempotency_key: IdempotencyKey,
    payload_digest: [u8; 32],
    payload_bytes: u64,
    parent_publication_key: Option<IdempotencyKey>,
    cascade_level: u64,
    published_at_ms: u64,
) -> Result<(), TopicAuthorityError> {
    transaction.execute(
        "INSERT INTO topic_publications (
            idempotency_key, topic_id, policy_digest, payer_account_id,
            payload_digest, payload_bytes, status, channel_sequence,
            channel_generation, cascade_budget_remaining, cascade_level,
            parent_idempotency_key, published_at_ms, enqueued_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, 0, 0, ?7, ?8, ?9, ?10, 0)",
        params![
            idempotency_key.as_bytes().as_slice(),
            topic.topic_id.as_bytes().as_slice(),
            topic.policy_digest.as_slice(),
            topic.policy.payer.as_bytes().as_slice(),
            payload_digest.as_slice(),
            encode_u64(payload_bytes)?,
            encode_u64(topic.policy.cascade_depth)?,
            encode_u64(cascade_level)?,
            parent_publication_key
                .as_ref()
                .map(|key| key.as_bytes().as_slice()),
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
                    unsubscribed_at_ms, last_advanced_at_ms,
                    consume_token, subscription_generation,
                    state, redelivery_used, quarantined_at_ms, reinstated_at_ms,
                    attached_by
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
                    row.get::<_, Vec<u8>>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                    row.get::<_, i64>(9)?,
                    row.get::<_, i64>(10)?,
                    row.get::<_, i64>(11)?,
                    row.get::<_, Option<Vec<u8>>>(12)?,
                ))
            },
        )
        .optional()?;
    raw.map(
        |(
            subscription_id,
            active,
            cursor,
            subscribed,
            unsubscribed,
            advanced,
            token,
            generation,
            state,
            redelivery_used,
            quarantined_at_ms,
            reinstated_at_ms,
            attached_by,
        )| {
            let state = match state {
                0 => SubscriptionState::Active,
                1 => SubscriptionState::Quarantined,
                _ => {
                    return Err(TopicAuthorityError::CorruptRecord(
                        "subscription delivery state is unknown",
                    ));
                }
            };
            let record = SubscriptionRecord {
                subscription_id: SubscriptionId::from_bytes(array16(subscription_id)?),
                topic_id,
                subscriber_key,
                active: active == 1,
                cursor: decode_u64(cursor)?,
                subscribed_at_ms: decode_u64(subscribed)?,
                unsubscribed_at_ms: decode_u64(unsubscribed)?,
                last_advanced_at_ms: decode_u64(advanced)?,
                consume_token: array32(token)?,
                subscription_generation: decode_u64(generation)?,
                state,
                redelivery_used: decode_u64(redelivery_used)?,
                quarantined_at_ms: decode_u64(quarantined_at_ms)?,
                reinstated_at_ms: decode_u64(reinstated_at_ms)?,
                attached_by: attached_by
                    .map(|bytes| array16(bytes).map(PatternId::from_bytes))
                    .transpose()?,
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

/// Raw `topic_patterns` row without the `pattern_id` column: pattern text,
/// binding, subscriber key, active bit, token, generation and the two
/// timestamps, in that order.
type PatternRow = (Vec<u8>, Vec<u8>, Vec<u8>, i64, Vec<u8>, i64, i64, i64);

fn load_pattern_optional(
    connection: &Connection,
    pattern_id: PatternId,
) -> Result<Option<PatternRecord>, TopicAuthorityError> {
    let raw = connection
        .query_row(
            "SELECT pattern_text, binding, subscriber_key, active, consume_token,
                    pattern_generation, subscribed_at_ms, cancelled_at_ms
             FROM topic_patterns WHERE pattern_id=?1",
            [pattern_id.as_bytes().as_slice()],
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
                ))
            },
        )
        .optional()?;
    raw.map(|row| decode_pattern(pattern_id, row)).transpose()
}

fn load_pattern_by_key(
    connection: &Connection,
    key: IdempotencyKey,
) -> Result<Option<PatternRecord>, TopicAuthorityError> {
    let pattern_id = connection
        .query_row(
            "SELECT pattern_id FROM topic_patterns WHERE create_idempotency_key=?1",
            [key.as_bytes().as_slice()],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()?;
    pattern_id
        .map(|bytes| load_pattern_optional(connection, PatternId::from_bytes(array16(bytes)?)))
        .transpose()
        .map(Option::flatten)
}

/// Loads every `ACTIVE` pattern row in pattern-id order (the create-time
/// attach candidate set).
fn load_active_patterns(
    connection: &Connection,
) -> Result<Vec<PatternRecord>, TopicAuthorityError> {
    let mut statement =
        connection.prepare("SELECT pattern_id FROM topic_patterns WHERE active=1")?;
    let ids = statement
        .query_map([], |row| row.get::<_, Vec<u8>>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    ids.into_iter()
        .map(|id| {
            let pattern_id = PatternId::from_bytes(array16(id)?);
            load_pattern_optional(connection, pattern_id)?.ok_or(
                TopicAuthorityError::CorruptRecord("active pattern row vanished"),
            )
        })
        .collect()
}

/// Decodes one pattern row and enforces its structural invariants: the
/// pattern text is still a legal pattern, the binding is bound, the
/// authority-derived id and consumption token re-derive from the stored
/// fields, and the active bit agrees with the cancel timestamp.
fn decode_pattern(
    stored_id: PatternId,
    row: PatternRow,
) -> Result<PatternRecord, TopicAuthorityError> {
    let (
        pattern,
        binding,
        subscriber_key,
        active,
        token,
        pattern_generation,
        subscribed_at_ms,
        cancelled_at_ms,
    ) = row;
    if validate_pattern(&pattern).is_err() {
        return Err(TopicAuthorityError::CorruptRecord(
            "stored pattern text is not a legal pattern",
        ));
    }
    if binding == [0; 16] {
        return Err(TopicAuthorityError::CorruptRecord(
            "pattern binding is unbound",
        ));
    }
    let subscriber_key = SubscriberKey::from_bytes(array16(subscriber_key)?);
    if stored_id != pattern_id_for(&pattern, subscriber_key) {
        return Err(TopicAuthorityError::CorruptRecord(
            "pattern id disagrees with the authority-derived identity",
        ));
    }
    let active = active == 1;
    if active != (cancelled_at_ms == 0) {
        return Err(TopicAuthorityError::CorruptRecord(
            "pattern active bit disagrees with its cancel timestamp",
        ));
    }
    let record = PatternRecord {
        pattern_id: stored_id,
        pattern,
        binding: ResourceAccountId::from_bytes(array16(binding)?),
        subscriber_key,
        active,
        consume_token: array32(token)?,
        pattern_generation: decode_u64(pattern_generation)?,
        subscribed_at_ms: decode_u64(subscribed_at_ms)?,
        cancelled_at_ms: decode_u64(cancelled_at_ms)?,
    };
    if record.consume_token != derive_pattern_token(stored_id, record.pattern_generation) {
        return Err(TopicAuthorityError::CorruptRecord(
            "pattern token disagrees with the derived identity",
        ));
    }
    Ok(record)
}

fn insert_or_resubscribe_pattern(
    transaction: &Transaction<'_>,
    record: &PatternRecord,
    idempotency_key: IdempotencyKey,
) -> Result<(), TopicAuthorityError> {
    let changed = transaction.execute(
        "INSERT INTO topic_patterns (
            pattern_id, pattern_text, binding, subscriber_key, active,
            consume_token, pattern_generation, subscribed_at_ms,
            cancelled_at_ms, create_idempotency_key
         ) VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6, ?7, 0, ?8)
         ON CONFLICT(pattern_text, subscriber_key) DO UPDATE SET
            active=1, consume_token=excluded.consume_token,
            pattern_generation=excluded.pattern_generation,
            subscribed_at_ms=excluded.subscribed_at_ms, cancelled_at_ms=0,
            create_idempotency_key=excluded.create_idempotency_key
         WHERE topic_patterns.active=0",
        params![
            record.pattern_id.as_bytes().as_slice(),
            record.pattern.as_slice(),
            record.binding.as_bytes().as_slice(),
            record.subscriber_key.as_bytes().as_slice(),
            record.consume_token.as_slice(),
            encode_u64(record.pattern_generation)?,
            encode_u64(record.subscribed_at_ms)?,
            idempotency_key.as_bytes().as_slice(),
        ],
    )?;
    if changed != 1 {
        return Err(TopicAuthorityError::CorruptRecord(
            "pattern admission CAS lost",
        ));
    }
    Ok(())
}

fn load_publication_by_key(
    connection: &Connection,
    key: IdempotencyKey,
) -> Result<Option<PublicationRecord>, TopicAuthorityError> {
    let raw = connection
        .query_row(
            "SELECT topic_id, policy_digest, payer_account_id, payload_digest,
                    status, channel_sequence, channel_generation,
                    cascade_budget_remaining, cascade_level, published_at_ms,
                    enqueued_at_ms, parent_idempotency_key
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
                    row.get::<_, i64>(10)?,
                    row.get::<_, Option<Vec<u8>>>(11)?,
                ))
            },
        )
        .optional()?;
    raw.map(|row| {
        let topic_id = TopicId::from_bytes(array16(row.0)?);
        let parent = row
            .11
            .map(|bytes| array16(bytes).map(IdempotencyKey::from_bytes))
            .transpose()?;
        decode_publication(
            topic_id,
            key,
            parent,
            (
                row.1, row.2, row.3, row.4, row.5, row.6, row.7, row.8, row.9, row.10,
            ),
        )
    })
    .transpose()
}

/// Walks the parent chain from `publication` up to its root publication and
/// enforces the provenance invariants: every link resolves to a durable row,
/// the cascade level decreases by exactly one per hop (which no cycle can
/// satisfy), the walk terminates at a level-0 root binding no parent, and
/// the chain is never longer than the starting level.  A visited set gives
/// an explicit cycle signal on top of the monotonicity proof.
fn verify_parent_chain(
    connection: &Connection,
    publication: &PublicationRecord,
) -> Result<(), TopicAuthorityError> {
    let mut current = publication.clone();
    let mut visited = vec![publication.idempotency_key];
    // A well-formed chain of level L terminates after L hops; anything
    // longer is corrupt.
    for _ in 0..=publication.cascade_level {
        let parent_key = match current.parent_publication_key {
            Some(key) => key,
            None if current.cascade_level == 0 => return Ok(()),
            None => {
                return Err(TopicAuthorityError::CorruptRecord(
                    "non-root publication binds no parent",
                ));
            }
        };
        let parent = load_publication_by_key(connection, parent_key)?.ok_or(
            TopicAuthorityError::CorruptRecord("parent chain link resolves to no durable row"),
        )?;
        if visited.contains(&parent.idempotency_key) {
            return Err(TopicAuthorityError::CorruptRecord(
                "parent chain forms a cycle",
            ));
        }
        if parent.cascade_level + 1 != current.cascade_level {
            return Err(TopicAuthorityError::CorruptRecord(
                "parent chain cascade levels are not monotone",
            ));
        }
        visited.push(parent.idempotency_key);
        current = parent;
    }
    Err(TopicAuthorityError::CorruptRecord(
        "parent chain does not terminate at a root publication",
    ))
}

/// Decodes one publication row and enforces its structural invariants: the
/// status mapping is known, a `PENDING_ENQUEUE` row binds no sequence or
/// generation, an `ENQUEUED` row binds both, and the cascade binding is
/// self-consistent (a root binds level 0 and no parent, a child binds both).
/// Cross-checks against the topic head and the channel high-water run in
/// [`TopicAuthority::inspect_publications`] and
/// [`TopicAuthority::inspect_publication`], where both are known.
fn decode_publication(
    topic_id: TopicId,
    idempotency_key: IdempotencyKey,
    parent_publication_key: Option<IdempotencyKey>,
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
        cascade_level,
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
    let cascade_level = decode_u64(cascade_level)?;
    let root_binding = cascade_level == 0 && parent_publication_key.is_none();
    let child_binding = cascade_level >= 1 && parent_publication_key.is_some();
    if !root_binding && !child_binding {
        return Err(TopicAuthorityError::CorruptRecord(
            "publication cascade level disagrees with its parent binding",
        ));
    }
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
        cascade_level,
        parent_publication_key,
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

/// Derives the [`PatternId`] from the pattern text and the subscriber key
/// (the same authority-derived-identity discipline as the concrete
/// [`SubscriptionId`], under its own domain tag).
fn pattern_id_for(pattern: &[u8], subscriber_key: SubscriberKey) -> PatternId {
    PatternId::from_bytes(derive_id(
        b"nlos/topic/pattern/id/v1",
        &[pattern, subscriber_key.as_bytes()],
    ))
}

/// Derives the consumption token for one pattern generation: the concrete
/// subscription token's derivation, domain-separated for patterns.  The
/// generation participates so every pattern re-subscribe invalidates the
/// previous generation's token.
fn derive_pattern_token(pattern_id: PatternId, pattern_generation: u64) -> ConsumeToken {
    derive_token(
        b"nlos/topic/pattern-consume-token/v1",
        &[pattern_id.as_bytes(), &pattern_generation.to_be_bytes()],
    )
}

/// Derives the consumption token for one subscription generation:
/// domain-separated SHA-256 over the [`SubscriptionId`] and the subscription
/// generation (the Channel [`FencingToken`] derivation style).  The
/// generation participates so the token is not derivable from the public
/// [`SubscriptionId`] alone and so every re-subscribe invalidates the
/// previous generation's token.
fn derive_consume_token(
    subscription_id: SubscriptionId,
    subscription_generation: u64,
) -> ConsumeToken {
    derive_token(
        b"nlos/topic/consume-token/v1",
        &[
            subscription_id.as_bytes(),
            &subscription_generation.to_be_bytes(),
        ],
    )
}

/// Fail-closed consumption-token check: `Some(token)` must match the
/// subscription row's authority-issued token exactly
/// ([`TopicAuthorityError::ConsumptionTokenMismatch`] otherwise, before any
/// write); `None` is the token-free compatibility surface and is admitted.
fn verify_consume_token(
    subscription: &SubscriptionRecord,
    consume_token: Option<ConsumeToken>,
) -> Result<(), TopicAuthorityError> {
    match consume_token {
        Some(token) if token == subscription.consume_token => Ok(()),
        Some(_) => Err(TopicAuthorityError::ConsumptionTokenMismatch(
            subscription.subscription_id,
        )),
        None => Ok(()),
    }
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
