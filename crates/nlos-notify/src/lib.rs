//! Notification service: a thin subscription face over the `Topic`/`Channel`/`Wait`
//! authorities (`B-NOTIFY-001`, W33-D / X-1 first half) extended with background
//! `Task` lifecycle notifications, one-shot bound action tokens, and local
//! offline history pagination (`TASK-NOTIFY-001`, `DUI-NOTIFY-001`,
//! `DUI-OFFLINE-001`).
//!
//! > **生产装配状态（2026-10-26 W66 / `C-UNFREEZE-001`）**：
//! > - 已接入 `nlos-system-control`（`NotificationServiceInspector`、`SystemControlDaemon`、`system-control-cli`、`nl`），提供有界只读订阅状态与离线历史分页观测及 `[CTRL-PARITY-001]` 收据合成。
//! > - 已接入 `nlos-slice-k`（`SliceKRuntime`、`CellHost`、`slice-k-demo`），驱动 `Task` 生命周期状态转移通知、一次性绑定动作令牌与离线历史投递。
//! > - 通知面扩展能力（`C-NOTIFY-001`：`TASK-NOTIFY-001` 后台任务状态通知、`DUI-NOTIFY-001` 一次性绑定动作令牌与 `DUI-OFFLINE-001` 离线历史分页）已与上述生产消费面完成端到端装配。
//!
//! Stage-B decision point 2 (`stage-b-progress` §6.5.4) fixed the boundary:
//! Notification is a *thin layer* over the existing Topic authority — no
//! new fanout machinery (ADR-0007's single-log fanout owns that), no
//! second source of truth.  Every canonical fanout fact of this crate's
//! surface lives in an authority database:
//!
//! - the subscription, its cursor and its delivery state are
//!   [`nlos_topic::TopicAuthority`] rows;
//! - the message log (the single enqueue per publication) is the
//!   [`nlos_channel::ChannelAuthority`] queue;
//! - the durable "wake me at sequence N" registrations are
//!   [`nlos_wait::WaitAuthority`] rows.
//!
//! [`NotificationService`] holds durable *references* and one-shot action
//! verification tombstones in `notify-service.db`:
//!
//! - `notify_subscriptions` (`v1`): maps a derived [`NotificationId`] to
//!   `(topic_id, subscriber_key)` plus the authority-issued [`ConsumeToken`],
//!   so the face can route delivery reads, token-authenticated acks and
//!   cancels to the authority on the subscriber's behalf without caching
//!   state, cursor, queue or payload;
//! - `notify_action_tokens` (`v2`, `DUI-NOTIFY-001`): stores cryptographically
//!   bound one-shot action token verifier commitments (`SHA-256` digest of
//!   `secret_proof`, never raw secrets) bound to `(notification_id,
//!   application_id, task_id, delivery_sequence, action_code)` with `SQLite`
//!   triggers forbidding bound-column mutation, status rollback, or deletion;
//! - `notify_history_refs` and `notify_history_prune_watermarks` (`v2`,
//!   `DUI-OFFLINE-001`): compact sequence-indexed reference metadata for
//!   deterministic cursor-paginated offline notification history queries and
//!   monotonic retention pruning.
//!
//! Cross-authority discipline follows the repository convention
//! (authority-first, idempotent replay converges): subscribe/cancel/publish
//! write the authority decision first and the local reference second, so a
//! crash between the two leaves a replayable authority fact and a missing or
//! stale reference that the next identical call converges.

#![forbid(unsafe_code)]

use std::error::Error;
use std::fmt;
use std::num::NonZeroU64;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use nlos_channel::QueueEntryRecord;
use nlos_topic::{
    AdvanceDecision, AdvanceReceipt, ConsumeToken, PublicationRecord, PublishDecision,
    SubscribeDecision, SubscribeRequest, SubscriberKey, TopicAuthority, TopicAuthorityError,
    TopicId, UnsubscribeDecision, UnsubscribeReceipt, UnsubscribeRequest,
};
use nlos_types::{ApplicationId, ChannelId, Generation, IdempotencyKey, ReceiptId, TaskId};
use nlos_wait::{
    BindingId, NotifyCommitsRequest, RegisterDecision, RegisterWaitRequest, WaitAuthority,
    WaitAuthorityError, WakeReport,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};

mod schema;
pub mod task_notify;

pub use task_notify::{
    DeliveredTaskNotification, TASK_NOTIFICATION_WIRE_LEN, TASK_NOTIFICATION_WIRE_MAGIC,
    TaskHistoryMetadata, TaskNotificationEnvelope, TaskNotificationWireError, TaskTransitionKind,
    decode_task_notification_envelope, encode_task_notification_envelope,
};

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

// The face identity of one managed subscription: a domain-separated
// SHA-256 over the referenced Topic-authority entities (topic id and
// subscriber key), so the same (topic, subscriber) pair always resolves to
// the same face identity across restarts and re-subscribes.
nominal_id!(NotificationId);

// Stable identity of a one-shot notification action token (DUI-NOTIFY-001).
nominal_id!(ActionTokenId);

/// Domain separator used to derive [`NotificationActionToken::secret_proof`]
/// (`DUI-NOTIFY-001`).
pub const ACTION_TOKEN_DOMAIN: &[u8] = b"llmos/notify-action-token/v1";

/// Maximum bounded page size accepted by
/// [`NotificationService::query_notification_history_page`] (`DUI-OFFLINE-001`).
pub const MAX_HISTORY_PAGE_LIMIT: usize = 1_024;

/// Derives the canonical domain-separated `SHA-256` `secret_proof` for a
/// one-shot notification action token (`DUI-NOTIFY-001`).
#[must_use]
pub fn derive_action_secret_proof(
    token_id: ActionTokenId,
    notification_id: NotificationId,
    application_id: ApplicationId,
    task_id: TaskId,
    delivery_sequence: u64,
    action_code: u16,
    secret_nonce: &[u8; 32],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(ACTION_TOKEN_DOMAIN);
    hasher.update(token_id.as_bytes());
    hasher.update(notification_id.as_bytes());
    hasher.update(application_id.as_bytes());
    hasher.update(task_id.as_bytes());
    hasher.update(delivery_sequence.to_be_bytes());
    hasher.update(action_code.to_be_bytes());
    hasher.update(secret_nonce);
    hasher.finalize().into()
}

/// Computes the `SHA-256` verifier commitment stored in `SQLite` for a derived
/// `secret_proof`. Raw `secret_nonce` and `secret_proof` bytes are never
/// persisted in `notify-service.db`.
#[must_use]
pub fn action_secret_commitment(secret_proof: &[u8; 32]) -> [u8; 32] {
    Sha256::digest(secret_proof).into()
}

/// One durable thin-layer reference row.
///
/// `consume_token` is the credential the Topic authority issued at
/// subscribe time, stored so the face can present it at the authenticated
/// boundaries (`advance_with_token`, `unsubscribe_with_token`) on the
/// subscriber's behalf; the authority re-validates it against the
/// subscription's current generation on every use, so a stale stored token
/// fails closed until a face re-subscribe refreshes it.
/// `registered_at_ms` is the face registration time — a fact about the
/// face call, deliberately distinct from the authority's own
/// `subscribed_at_ms`.  Nothing else is stored: no state, cursor, queue or
/// payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NotificationSubscription {
    pub notification_id: NotificationId,
    pub topic_id: TopicId,
    pub subscriber_key: SubscriberKey,
    pub consume_token: ConsumeToken,
    pub registered_at_ms: u64,
}

/// One list entry: the thin reference plus the *live* authority state read
/// through [`TopicAuthority::inspect_subscription`] at list time.
///
/// A reference whose authority rows have diverged away (the topic or
/// subscription no longer resolves in the bound Topic authority) surfaces
/// as [`Self::Dangling`] — reported, never silently dropped and never
/// papered over with a copied state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NotificationSubscriptionView {
    Live {
        subscription: NotificationSubscription,
        authority: nlos_topic::SubscriptionRecord,
    },
    Dangling {
        subscription: NotificationSubscription,
    },
}

/// A subscribe request through the face: the authority entities plus the
/// face registration time.  The face identity is derived, never supplied.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SubscribeNotificationRequest {
    pub topic_id: TopicId,
    pub subscriber_key: SubscriberKey,
    pub subscribed_at_ms: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NotificationSubscribeDecision {
    /// The Topic authority created (or re-activated) the subscription and
    /// the face recorded (or refreshed) its reference.
    Subscribed(NotificationSubscription),
    /// The authority row was already active and replayed; a stored
    /// reference was refreshed to the current generation's token if it had
    /// drifted.
    Replayed(NotificationSubscription),
}

impl NotificationSubscribeDecision {
    #[must_use]
    pub const fn subscription(self) -> NotificationSubscription {
        match self {
            Self::Subscribed(subscription) | Self::Replayed(subscription) => subscription,
        }
    }
}

/// A cancel request addressed by the face identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CancelNotificationRequest {
    pub notification_id: NotificationId,
    pub cancelled_at_ms: u64,
}

/// The cancel receipt: which face reference was cancelled plus the
/// authority's own unsubscribe receipt verbatim.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NotificationCancelReceipt {
    pub notification_id: NotificationId,
    pub authority: UnsubscribeReceipt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NotificationCancelDecision {
    /// The authority flipped the subscription inactive and the face
    /// removed its reference.
    Cancelled(NotificationCancelReceipt),
    /// The authority subscription was already inactive (a retry after the
    /// crash window between the authority write and the reference
    /// removal); the receipt replays and the reference is removed now.
    Replayed(NotificationCancelReceipt),
}

impl NotificationCancelDecision {
    #[must_use]
    pub const fn receipt(self) -> NotificationCancelReceipt {
        match self {
            Self::Cancelled(receipt) | Self::Replayed(receipt) => receipt,
        }
    }
}

/// An ack request: advance the referenced subscription's cursor to
/// `up_to_sequence`, authenticated by the stored credential.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AckNotificationRequest {
    pub notification_id: NotificationId,
    pub up_to_sequence: u64,
    pub acked_at_ms: u64,
}

/// The ack receipt: the face identity plus the authority's advance receipt
/// verbatim.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NotificationAckReceipt {
    pub notification_id: NotificationId,
    pub authority: AdvanceReceipt,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NotificationAckDecision {
    Acknowledged(NotificationAckReceipt),
    Replayed(NotificationAckReceipt),
}

impl NotificationAckDecision {
    #[must_use]
    pub const fn receipt(self) -> NotificationAckReceipt {
        match self {
            Self::Acknowledged(receipt) | Self::Replayed(receipt) => receipt,
        }
    }
}

/// A durable delivery-wait registration request (ADR-0008 semantics,
/// unchanged): wake `binding` when the referenced subscription's channel
/// commits at least `target_sequence`.  The wait row lives in the Wait
/// authority; the face stores nothing for it — the channel is resolved
/// live from the Topic authority's topic record.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegisterDeliveryWaitRequest {
    pub notification_id: NotificationId,
    pub target_sequence: u64,
    pub binding: BindingId,
    pub idempotency_key: IdempotencyKey,
    pub registered_at_ms: u64,
}

/// A publish request through the face.  Fields are the Topic authority's
/// [`nlos_topic::PublishRequest`] verbatim; the payload is enqueued
/// exactly once by the Topic authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PublishNotificationRequest {
    pub topic_id: TopicId,
    pub payload: Vec<u8>,
    pub idempotency_key: IdempotencyKey,
    pub published_at_ms: u64,
}

/// The publish receipt: the authority's publication record verbatim plus
/// the Wait registry's wake report for the commit notification this face
/// issued (the ADR-0008 commit-side wiring; empty when no registered wait
/// was covered).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NotificationPublishReceipt {
    pub publication: PublicationRecord,
    pub wakes: WakeReport,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NotificationPublishDecision {
    Published(NotificationPublishReceipt),
    Replayed(NotificationPublishReceipt),
}

impl NotificationPublishDecision {
    #[must_use]
    pub fn receipt(self) -> NotificationPublishReceipt {
        match self {
            Self::Published(receipt) | Self::Replayed(receipt) => receipt,
        }
    }
}

/// Request to issue a one-shot cryptographically bound notification action
/// token (`DUI-NOTIFY-001`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IssueNotificationActionTokenRequest {
    pub notification_id: NotificationId,
    pub application_id: ApplicationId,
    pub task_id: TaskId,
    pub delivery_sequence: u64,
    pub action_code: u16,
    pub secret_nonce: [u8; 32],
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
    pub idempotency_key: IdempotencyKey,
}

/// Issued one-shot notification action token carrying the capability
/// `secret_proof` (`DUI-NOTIFY-001`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NotificationActionToken {
    pub token_id: ActionTokenId,
    pub notification_id: NotificationId,
    pub application_id: ApplicationId,
    pub task_id: TaskId,
    pub delivery_sequence: u64,
    pub action_code: u16,
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
    pub secret_proof: [u8; 32],
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IssueActionTokenDecision {
    Issued(NotificationActionToken),
    Replayed(NotificationActionToken),
}

impl IssueActionTokenDecision {
    #[must_use]
    pub const fn token(self) -> NotificationActionToken {
        match self {
            Self::Issued(token) | Self::Replayed(token) => token,
        }
    }
}

/// Durable lifecycle state of a stored notification action token.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ActionTokenStatus {
    Issued,
    Redeemed {
        redeemed_at_ms: u64,
        receipt_id: ReceiptId,
    },
    Revoked {
        revoked_at_ms: u64,
    },
}

/// Durable verifier record stored in `notify_action_tokens` (`secret_commitment`
/// only, never raw `secret_nonce` or `secret_proof`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ActionTokenRecord {
    pub token_id: ActionTokenId,
    pub idempotency_key: IdempotencyKey,
    pub notification_id: NotificationId,
    pub application_id: ApplicationId,
    pub task_id: TaskId,
    pub delivery_sequence: u64,
    pub action_code: u16,
    pub secret_commitment: [u8; 32],
    pub issued_at_ms: u64,
    pub expires_at_ms: u64,
    pub status: ActionTokenStatus,
}

/// Request to redeem a one-shot notification action token (`DUI-NOTIFY-001`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RedeemNotificationActionRequest {
    pub token_id: ActionTokenId,
    pub presented_proof: [u8; 32],
    pub caller_notification_id: NotificationId,
    pub caller_application_id: ApplicationId,
    pub caller_task_id: TaskId,
    pub action_code: u16,
    pub redeemed_at_ms: u64,
}

/// Receipt proving the one-shot consumption of a bound notification action
/// token (`DUI-NOTIFY-001`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NotificationActionRedeemReceipt {
    pub receipt_id: ReceiptId,
    pub token_id: ActionTokenId,
    pub notification_id: NotificationId,
    pub application_id: ApplicationId,
    pub task_id: TaskId,
    pub delivery_sequence: u64,
    pub action_code: u16,
    pub redeemed_at_ms: u64,
}

/// Request to record a durable offline notification history reference
/// (`DUI-OFFLINE-001`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RecordDeliveryHistoryRequest {
    pub topic_id: TopicId,
    pub channel_id: ChannelId,
    pub channel_sequence: u64,
    pub payload_digest: [u8; 32],
    pub recorded_at_ms: u64,
    pub task_metadata: Option<TaskHistoryMetadata>,
}

/// One durable offline notification history reference row (`DUI-OFFLINE-001`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NotificationHistoryEntry {
    pub topic_id: TopicId,
    pub channel_id: ChannelId,
    pub channel_sequence: u64,
    pub payload_digest: [u8; 32],
    pub recorded_at_ms: u64,
    pub task_metadata: Option<TaskHistoryMetadata>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecordHistoryDecision {
    Recorded(NotificationHistoryEntry),
    Replayed(NotificationHistoryEntry),
}

impl RecordHistoryDecision {
    #[must_use]
    pub const fn entry(self) -> NotificationHistoryEntry {
        match self {
            Self::Recorded(entry) | Self::Replayed(entry) => entry,
        }
    }
}

/// Bounded cursor-paginated offline notification history page
/// (`DUI-OFFLINE-001`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NotificationHistoryPage {
    pub topic_id: TopicId,
    pub entries: Vec<NotificationHistoryEntry>,
    pub next_after_sequence: Option<u64>,
    pub has_more: bool,
    pub retained_from_sequence: u64,
}

/// Receipt returned by [`NotificationService::prune_history_before`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HistoryPruneReceipt {
    pub topic_id: TopicId,
    pub retained_from_sequence: u64,
    pub pruned_entries: u64,
}

#[derive(Debug)]
pub enum NotifyError {
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
    DurabilityUnavailable {
        journal_mode: String,
        synchronous: i64,
    },
    SchemaVersionUnsupported(i64),
    /// A typed rejection from the Topic authority, propagated without
    /// silent retry or translation.
    Topic(TopicAuthorityError),
    /// A typed rejection from the Wait authority, propagated without
    /// silent retry or translation.
    Wait(WaitAuthorityError),
    /// The face holds no reference for the requested notification id
    /// (never subscribed through the face, or cancelled and removed).
    NotificationNotFound(NotificationId),
    CorruptRecord(&'static str),
    LockPoisoned,
    /// Malformed or truncated [`TaskNotificationEnvelope`] wire bytes.
    MalformedTaskNotification(TaskNotificationWireError),
    /// Malformed parameters in an action token issuance or redemption request.
    InvalidActionTokenRequest(&'static str),
    /// Reused `idempotency_key` with conflicting action token parameters.
    ActionIdempotencyConflict(IdempotencyKey),
    /// Unknown one-shot action token identity.
    ActionTokenNotFound(ActionTokenId),
    /// Cross-`Application` spoofing attempt (`DUI-NOTIFY-001`).
    ActionCrossApplicationSpoof {
        expected: ApplicationId,
        actual: ApplicationId,
    },
    /// Mismatched `NotificationId`, `TaskId`, or `action_code` binding.
    ActionBindingMismatch(&'static str),
    /// Invalid `presented_proof` against the stored verifier commitment.
    ActionTokenProofInvalid(ActionTokenId),
    /// Action token redeemed past its `expires_at_ms` deadline.
    ActionTokenExpired {
        expires_at_ms: u64,
        redeemed_at_ms: u64,
    },
    /// Replay / second redemption attempt on an already-redeemed one-shot
    /// action token (`DUI-NOTIFY-001`).
    ActionTokenAlreadyRedeemed {
        first_redeemed_at_ms: u64,
        receipt_id: ReceiptId,
    },
    /// Redemption attempt on an administratively revoked action token.
    ActionTokenRevoked {
        revoked_at_ms: u64,
    },
    /// Invalid pagination or history retention argument (`limit == 0` or
    /// `limit > MAX_HISTORY_PAGE_LIMIT`).
    InvalidHistoryPagination(&'static str),
    /// Conflicting history reference recorded for the same `(topic_id,
    /// channel_sequence)`.
    HistoryConflict {
        topic_id: TopicId,
        channel_sequence: u64,
    },
    /// Attempt to record a history sequence below the monotonic retention
    /// prune watermark.
    HistorySequencePruned {
        topic_id: TopicId,
        channel_sequence: u64,
        retained_from_sequence: u64,
    },
}

impl fmt::Display for NotifyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(formatter, "SQLite notification face failure: {error}"),
            Self::Io(error) => write!(formatter, "notification face I/O failure: {error}"),
            Self::DurabilityUnavailable {
                journal_mode,
                synchronous,
            } => write!(
                formatter,
                "WAL/FULL durability unavailable: journal_mode={journal_mode}, \
                 synchronous={synchronous}"
            ),
            Self::SchemaVersionUnsupported(version) => {
                write!(
                    formatter,
                    "unsupported notification face schema version {version}"
                )
            }
            Self::Topic(error) => {
                write!(
                    formatter,
                    "topic authority rejected notification operation: {error}"
                )
            }
            Self::Wait(error) => {
                write!(
                    formatter,
                    "wait authority rejected notification operation: {error}"
                )
            }
            Self::NotificationNotFound(id) => {
                write!(
                    formatter,
                    "notification {id:?} is not registered with this face"
                )
            }
            Self::CorruptRecord(reason) => {
                write!(formatter, "corrupt notification record: {reason}")
            }
            Self::LockPoisoned => formatter.write_str("notification face writer lock is poisoned"),
            other => fmt_extension_error(other, formatter),
        }
    }
}

fn fmt_extension_error(error: &NotifyError, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
    match error {
        NotifyError::MalformedTaskNotification(inner) => {
            write!(formatter, "malformed task notification payload: {inner}")
        }
        NotifyError::InvalidActionTokenRequest(reason) => {
            write!(
                formatter,
                "invalid notification action token request: {reason}"
            )
        }
        NotifyError::ActionIdempotencyConflict(key) => write!(
            formatter,
            "action token idempotency key {key:?} rebound to conflicting parameters"
        ),
        NotifyError::ActionTokenNotFound(token_id) => {
            write!(
                formatter,
                "notification action token {token_id:?} not found"
            )
        }
        NotifyError::ActionCrossApplicationSpoof { expected, actual } => write!(
            formatter,
            "cross-application notification action spoof rejected: bound to {expected:?}, \
             caller presented {actual:?}"
        ),
        NotifyError::ActionBindingMismatch(reason) => {
            write!(
                formatter,
                "notification action token binding mismatch: {reason}"
            )
        }
        NotifyError::ActionTokenProofInvalid(token_id) => write!(
            formatter,
            "invalid cryptographic proof presented for action token {token_id:?}"
        ),
        NotifyError::ActionTokenExpired {
            expires_at_ms,
            redeemed_at_ms,
        } => write!(
            formatter,
            "notification action token expired at {expires_at_ms}ms \
             (redeem attempted at {redeemed_at_ms}ms)"
        ),
        NotifyError::ActionTokenAlreadyRedeemed {
            first_redeemed_at_ms,
            receipt_id,
        } => write!(
            formatter,
            "one-shot notification action token was already redeemed at \
             {first_redeemed_at_ms}ms (receipt {receipt_id:?})"
        ),
        NotifyError::ActionTokenRevoked { revoked_at_ms } => {
            write!(
                formatter,
                "notification action token was revoked at {revoked_at_ms}ms"
            )
        }
        NotifyError::InvalidHistoryPagination(reason) => {
            write!(formatter, "invalid notification history query: {reason}")
        }
        NotifyError::HistoryConflict {
            topic_id,
            channel_sequence,
        } => write!(
            formatter,
            "conflicting notification history reference for {topic_id:?} \
             at sequence {channel_sequence}"
        ),
        NotifyError::HistorySequencePruned {
            topic_id,
            channel_sequence,
            retained_from_sequence,
        } => write!(
            formatter,
            "notification history sequence {channel_sequence} on {topic_id:?} \
             is below retention watermark {retained_from_sequence}"
        ),
        _ => unreachable!("base NotifyError variants formatted in Display::fmt"),
    }
}

impl Error for NotifyError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Topic(error) => Some(error),
            Self::Wait(error) => Some(error),
            Self::MalformedTaskNotification(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for NotifyError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

impl From<TaskNotificationWireError> for NotifyError {
    fn from(error: TaskNotificationWireError) -> Self {
        Self::MalformedTaskNotification(error)
    }
}

/// The notification service face: a single writer over its own small
/// reference database, delegating every canonical fanout decision to the
/// bound authorities.
///
/// The `wait` authority must be bound to the same
/// [`nlos_channel::ChannelAuthority`] as the `topic` authority (each
/// authority verifies channels through its own bound Channel owner); the
/// face cannot observe the binding and trusts the host's wiring.
pub struct NotificationService {
    topic: Arc<TopicAuthority>,
    wait: Arc<WaitAuthority>,
    connection: Mutex<Connection>,
}

impl NotificationService {
    /// Opens or creates `<root>/notify-service.db` bound to the given
    /// authorities.
    ///
    /// A brand-new database (`user_version == 0`) initializes the thin `v1`
    /// subscription reference table (`notify_subscriptions`) and promotes to
    /// `SCHEMA_VERSION` (`2`) transparently upon reopening an existing `v1`
    /// database or invoking any `v2` task/action-token/history capability.
    ///
    /// # Errors
    ///
    /// Fails closed when `SQLite` cannot provide WAL/FULL durability, when
    /// stored schema tables/triggers are partial or corrupt, or when a stored
    /// schema version is unknown.
    pub fn open(
        root: impl AsRef<Path>,
        topic: Arc<TopicAuthority>,
        wait: Arc<WaitAuthority>,
    ) -> Result<Self, NotifyError> {
        if !root.as_ref().to_string_lossy().starts_with("file:") {
            std::fs::create_dir_all(root.as_ref()).map_err(NotifyError::Io)?;
        }
        let mut connection = Connection::open(root.as_ref().join("notify-service.db"))?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;

        let journal_mode: String =
            connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        let synchronous: i64 =
            connection.pragma_query_value(None, "synchronous", |row| row.get(0))?;
        if !journal_mode.eq_ignore_ascii_case("wal") || synchronous != 2 {
            return Err(NotifyError::DurabilityUnavailable {
                journal_mode,
                synchronous,
            });
        }

        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        match version {
            0 => schema::migrate_v1(&mut connection)?,
            schema::SCHEMA_VERSION_V1 => schema::migrate_v2(&mut connection)?,
            schema::SCHEMA_VERSION => schema::verify_v2(&connection)?,
            other => return Err(NotifyError::SchemaVersionUnsupported(other)),
        }
        Ok(Self {
            topic,
            wait,
            connection: Mutex::new(connection),
        })
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>, NotifyError> {
        self.connection
            .lock()
            .map_err(|_| NotifyError::LockPoisoned)
    }

    /// Subscribes through the Topic authority and records the thin
    /// reference.
    ///
    /// Authority-first: [`TopicAuthority::subscribe`] owns the canonical
    /// decision (admission, subscribe point, generation, issued token);
    /// the face then upserts its `(notification_id, token)` reference.  A
    /// crash between the two leaves the authority fact and no reference —
    /// replaying the identical call replays the authority decision and
    /// completes the reference, zero duplication.  Re-subscribing an
    /// active key replays; re-subscribing after a cancel mints the next
    /// generation under the same derived face identity.
    ///
    /// # Errors
    ///
    /// Fails closed with the propagated authority rejection (unknown
    /// topic, recipient limit, storage failure) before any face write.
    pub fn subscribe(
        &self,
        request: SubscribeNotificationRequest,
    ) -> Result<NotificationSubscribeDecision, NotifyError> {
        let decision = self
            .topic
            .subscribe(SubscribeRequest {
                topic_id: request.topic_id,
                subscriber_key: request.subscriber_key,
                subscribed_at_ms: request.subscribed_at_ms,
            })
            .map_err(NotifyError::Topic)?;
        let record = decision.record();
        let subscription = NotificationSubscription {
            notification_id: notification_id_for(request.topic_id, request.subscriber_key),
            topic_id: request.topic_id,
            subscriber_key: request.subscriber_key,
            consume_token: record.consume_token,
            registered_at_ms: request.subscribed_at_ms,
        };
        let fresh = matches!(decision, SubscribeDecision::Subscribed(_));
        let connection = self.lock()?;
        upsert_subscription(&connection, &subscription)?;
        Ok(if fresh {
            NotificationSubscribeDecision::Subscribed(subscription)
        } else {
            NotificationSubscribeDecision::Replayed(subscription)
        })
    }

    /// Inspects one thin subscription reference by its [`NotificationId`],
    /// joined with the live [`TopicAuthority`] subscription state.
    ///
    /// # Errors
    ///
    /// Fails closed with [`NotifyError::NotificationNotFound`] when
    /// `notification_id` is unknown, or on face/authority storage errors.
    pub fn inspect_subscription(
        &self,
        notification_id: NotificationId,
    ) -> Result<NotificationSubscriptionView, NotifyError> {
        let subscription = {
            let connection = self.lock()?;
            resolve_subscription(&connection, notification_id)?
        };
        match self
            .topic
            .inspect_subscription(subscription.topic_id, subscription.subscriber_key)
        {
            Ok(authority) => Ok(NotificationSubscriptionView::Live {
                subscription,
                authority,
            }),
            Err(
                TopicAuthorityError::TopicNotFound(_)
                | TopicAuthorityError::SubscriptionNotFound(_),
            ) => Ok(NotificationSubscriptionView::Dangling { subscription }),
            Err(other) => Err(NotifyError::Topic(other)),
        }
    }

    /// Lists the thin references, each joined with the live authority
    /// subscription state read at list time (ordered by face identity).
    ///
    /// The face never caches the authority state: an out-of-band authority
    /// unsubscribe shows through as `authority.active == false`, and a
    /// reference whose authority rows no longer resolve surfaces as
    /// [`NotificationSubscriptionView::Dangling`].
    ///
    /// # Errors
    ///
    /// Fails closed for a face storage/corruption failure, or a propagated
    /// Topic-authority failure other than the not-found family that maps
    /// to [`NotificationSubscriptionView::Dangling`].
    pub fn list_subscriptions(&self) -> Result<Vec<NotificationSubscriptionView>, NotifyError> {
        let references = {
            let connection = self.lock()?;
            load_all_subscriptions(&connection)?
        };
        references
            .into_iter()
            .map(|subscription| {
                match self
                    .topic
                    .inspect_subscription(subscription.topic_id, subscription.subscriber_key)
                {
                    Ok(authority) => Ok(NotificationSubscriptionView::Live {
                        subscription,
                        authority,
                    }),
                    Err(
                        TopicAuthorityError::TopicNotFound(_)
                        | TopicAuthorityError::SubscriptionNotFound(_),
                    ) => Ok(NotificationSubscriptionView::Dangling { subscription }),
                    Err(other) => Err(NotifyError::Topic(other)),
                }
            })
            .collect()
    }

    /// Cancels through the Topic authority, presenting the stored
    /// credential, then removes the thin reference.
    ///
    /// Authority-first: [`TopicAuthority::unsubscribe_with_token`] owns the
    /// canonical inactive flip (and re-validates the stored token against
    /// the subscription's current generation — a stale token fails closed
    /// with `ConsumptionTokenMismatch`); the face deletes its reference
    /// only after the authority accepted.  A crash between the two leaves
    /// the reference in place; a retry replays the authority receipt and
    /// removes it.  The authority's unsubscribe audit row is the durable
    /// record, so the reference deletion loses no fact.  Cancelling an
    /// unknown face identity is [`NotifyError::NotificationNotFound`].
    ///
    /// # Errors
    ///
    /// Fails closed for an unknown face identity, a propagated authority
    /// rejection (token mismatch, storage failure), or a face
    /// storage/corruption failure.
    pub fn cancel(
        &self,
        request: CancelNotificationRequest,
    ) -> Result<NotificationCancelDecision, NotifyError> {
        let subscription = {
            let connection = self.lock()?;
            resolve_subscription(&connection, request.notification_id)?
        };
        let decision = self
            .topic
            .unsubscribe_with_token(
                UnsubscribeRequest {
                    topic_id: subscription.topic_id,
                    subscriber_key: subscription.subscriber_key,
                    unsubscribed_at_ms: request.cancelled_at_ms,
                },
                &subscription.consume_token,
            )
            .map_err(NotifyError::Topic)?;
        let fresh = matches!(decision, UnsubscribeDecision::Unsubscribed(_));
        {
            let connection = self.lock()?;
            delete_subscription(&connection, request.notification_id)?;
        }
        let receipt = NotificationCancelReceipt {
            notification_id: request.notification_id,
            authority: decision.receipt(),
        };
        Ok(if fresh {
            NotificationCancelDecision::Cancelled(receipt)
        } else {
            NotificationCancelDecision::Replayed(receipt)
        })
    }

    /// Delivers by reading what the Topic authority fanned out: a
    /// zero-write pass-through of [`TopicAuthority::poll`] for the
    /// referenced subscription (its cursor filters the shared single-log
    /// window).
    ///
    /// # Errors
    ///
    /// Fails closed for an unknown face identity, a propagated authority
    /// rejection (inactive subscription, delivery quarantine, storage
    /// failure), or a face storage/corruption failure.
    pub fn poll(
        &self,
        notification_id: NotificationId,
        limit: usize,
    ) -> Result<Vec<QueueEntryRecord>, NotifyError> {
        let subscription = {
            let connection = self.lock()?;
            resolve_subscription(&connection, notification_id)?
        };
        self.topic
            .poll(subscription.topic_id, subscription.subscriber_key, limit)
            .map_err(NotifyError::Topic)
    }

    /// Acknowledges through the existing Topic ack machinery: the
    /// token-authenticated per-subscriber cursor advance
    /// ([`TopicAuthority::advance_with_token`], which also records the
    /// payer attribution ledger rows for the crossed window).
    ///
    /// The stored credential is presented on the subscriber's behalf; the
    /// authority re-validates it against the current generation, so a
    /// token staled by an out-of-band authority re-subscribe fails closed
    /// until a face re-subscribe refreshes it.
    ///
    /// # Errors
    ///
    /// Fails closed for an unknown face identity, a token mismatch, a
    /// regressing or out-of-range sequence, or a storage/corruption
    /// failure.
    pub fn ack(
        &self,
        request: AckNotificationRequest,
    ) -> Result<NotificationAckDecision, NotifyError> {
        let subscription = {
            let connection = self.lock()?;
            resolve_subscription(&connection, request.notification_id)?
        };
        let decision = self
            .topic
            .advance_with_token(
                nlos_topic::AdvanceRequest {
                    topic_id: subscription.topic_id,
                    subscriber_key: subscription.subscriber_key,
                    up_to_sequence: request.up_to_sequence,
                    advanced_at_ms: request.acked_at_ms,
                },
                &subscription.consume_token,
            )
            .map_err(NotifyError::Topic)?;
        let fresh = matches!(decision, AdvanceDecision::Advanced(_));
        let receipt = NotificationAckReceipt {
            notification_id: request.notification_id,
            authority: decision.receipt(),
        };
        Ok(if fresh {
            NotificationAckDecision::Acknowledged(receipt)
        } else {
            NotificationAckDecision::Replayed(receipt)
        })
    }

    /// Registers a durable delivery wait through the Wait authority
    /// (ADR-0008 semantics, unchanged by this face).
    ///
    /// The channel is resolved live from the Topic authority's topic
    /// record; the wait row, its idempotency and its state machine live
    /// entirely in the Wait authority — the face keeps no wait record.
    /// The wait is not flipped by this face's reads; only a commit
    /// notification (see [`NotificationService::publish`]) or an explicit
    /// `notify_commits` does that.
    ///
    /// # Errors
    ///
    /// Fails closed for an unknown face identity, a propagated authority
    /// rejection (zero target sequence, zero binding, idempotency
    /// rebinding, storage failure), or a face storage/corruption failure.
    pub fn register_delivery_wait(
        &self,
        request: RegisterDeliveryWaitRequest,
    ) -> Result<RegisterDecision, NotifyError> {
        let subscription = {
            let connection = self.lock()?;
            resolve_subscription(&connection, request.notification_id)?
        };
        let topic = self
            .topic
            .inspect_topic(subscription.topic_id)
            .map_err(NotifyError::Topic)?;
        self.wait
            .register_wait(RegisterWaitRequest {
                binding: request.binding,
                channel_id: topic.channel_id,
                target_sequence: request.target_sequence,
                idempotency_key: request.idempotency_key,
                registered_at_ms: request.registered_at_ms,
            })
            .map_err(NotifyError::Wait)
    }

    /// Publishes through the Topic authority (the single enqueue of the
    /// ADR-0007 single-log fanout) and then issues the ADR-0008
    /// commit-side wiring: the explicit idempotent `notify_commits` to the
    /// Wait registry for the publication's channel and sequence.
    ///
    /// The notify idempotency key is derived deterministically from the
    /// publication's idempotency key, so a replayed publish re-reports the
    /// original durable wake set without a second enqueue or wake flip.
    /// If the enqueue committed but the notify failed (for example a wait
    /// storage failure), the call fails with [`NotifyError::Wait`] while
    /// the publication stays durably enqueued: replaying the identical
    /// request converges (the publication replays, the notify completes) —
    /// the same authority-first convergence discipline as everywhere in
    /// this crate.  The face performs no enqueue of its own under any
    /// path.
    ///
    /// # Errors
    ///
    /// Fails closed with the propagated Topic rejection (empty payload,
    /// unknown topic, retention bounds, insufficient credit, idempotency
    /// rebinding, propagated Channel rejection) before the notify step, or
    /// with the propagated Wait rejection after a committed enqueue.
    pub fn publish(
        &self,
        request: PublishNotificationRequest,
    ) -> Result<NotificationPublishDecision, NotifyError> {
        let decision = self
            .topic
            .publish(nlos_topic::PublishRequest {
                topic_id: request.topic_id,
                payload: request.payload,
                idempotency_key: request.idempotency_key,
                published_at_ms: request.published_at_ms,
            })
            .map_err(NotifyError::Topic)?;
        let fresh = matches!(decision, PublishDecision::Published(_));
        // An `Ok` publish decision always binds an ENQUEUED record with a
        // non-zero channel sequence (the authority's contract: a resumed
        // PENDING row converges inside the call or the call fails).
        let record = decision.record();
        let topic = self
            .topic
            .inspect_topic(request.topic_id)
            .map_err(NotifyError::Topic)?;
        let wakes = self
            .wait
            .notify_commits(NotifyCommitsRequest {
                channel_id: topic.channel_id,
                up_to_sequence: record.channel_sequence,
                notified_at_ms: request.published_at_ms.max(1),
                idempotency_key: commit_notify_key_for(request.idempotency_key),
            })
            .map_err(NotifyError::Wait)?;
        let receipt = NotificationPublishReceipt {
            publication: record,
            wakes,
        };
        Ok(if fresh {
            NotificationPublishDecision::Published(receipt)
        } else {
            NotificationPublishDecision::Replayed(receipt)
        })
    }

    /// Publishes a canonical background `Task` lifecycle transition
    /// (`TASK-NOTIFY-001`, `TASK-STATE-001`) through the single
    /// [`TopicAuthority`] + [`WaitAuthority`] `notify_commits` path (zero
    /// shadow fanout), and records a compact offline history reference
    /// (`DUI-OFFLINE-001`).
    ///
    /// # Errors
    ///
    /// Fails closed with any propagated `TopicAuthority` or `WaitAuthority`
    /// rejection, or on local `SQLite` durability/conflict errors.
    pub fn publish_task_transition(
        &self,
        topic_id: TopicId,
        envelope: &TaskNotificationEnvelope,
        idempotency_key: IdempotencyKey,
    ) -> Result<NotificationPublishDecision, NotifyError> {
        let payload = encode_task_notification_envelope(envelope);
        let decision = self.publish(PublishNotificationRequest {
            topic_id,
            payload,
            idempotency_key,
            published_at_ms: envelope.occurred_at_ms,
        })?;
        let topic = self
            .topic
            .inspect_topic(topic_id)
            .map_err(NotifyError::Topic)?;
        let publication = match &decision {
            NotificationPublishDecision::Published(receipt)
            | NotificationPublishDecision::Replayed(receipt) => &receipt.publication,
        };
        let history_request = RecordDeliveryHistoryRequest {
            topic_id,
            channel_id: topic.channel_id,
            channel_sequence: publication.channel_sequence,
            payload_digest: publication.payload_digest,
            recorded_at_ms: envelope.occurred_at_ms,
            task_metadata: Some(TaskHistoryMetadata::from(envelope)),
        };
        match self.record_delivery_history(history_request) {
            Ok(_) => {}
            Err(NotifyError::HistorySequencePruned { .. })
                if matches!(decision, NotificationPublishDecision::Replayed(_)) => {}
            Err(other) => return Err(other),
        }
        Ok(decision)
    }

    /// Polls unacknowledged background `Task` transition notifications for
    /// `notification_id` directly from the shared `TopicAuthority` single-log
    /// window (`TASK-NOTIFY-001`), decoding each wire payload fail-closed.
    ///
    /// # Errors
    ///
    /// Fails closed for an unknown `notification_id`, any propagated
    /// `TopicAuthority` rejection, or any malformed/truncated task
    /// notification payload ([`NotifyError::MalformedTaskNotification`]).
    pub fn poll_task_notifications(
        &self,
        notification_id: NotificationId,
        limit: usize,
    ) -> Result<Vec<DeliveredTaskNotification>, NotifyError> {
        let raw_entries = self.poll(notification_id, limit)?;
        let mut decoded = Vec::with_capacity(raw_entries.len());
        for entry in raw_entries {
            let envelope = decode_task_notification_envelope(&entry.payload)?;
            decoded.push(DeliveredTaskNotification {
                notification_id,
                channel_id: entry.channel_id,
                sequence: entry.sequence,
                enqueued_at_ms: entry.enqueued_at_ms,
                publication_idempotency_key: entry.idempotency_key,
                envelope,
            });
        }
        Ok(decoded)
    }

    /// Issues a cryptographically bound one-shot notification action token
    /// (`DUI-NOTIFY-001`).
    ///
    /// Verifies that `request.notification_id` is currently registered, derives
    /// `secret_proof` via `SHA-256` under [`ACTION_TOKEN_DOMAIN`], persists
    /// only the `SHA-256` verifier commitment in `SQLite` (never raw
    /// `secret_nonce` or `secret_proof`), and replays idempotently by
    /// `request.idempotency_key`.
    ///
    /// # Errors
    ///
    /// Fails closed when `notification_id` is unknown, when request bounds are
    /// invalid (`delivery_sequence == 0`, `expires_at_ms < issued_at_ms`, or
    /// all-zero `secret_nonce`), when `idempotency_key` is reused with
    /// different bound parameters, or on `SQLite` errors.
    pub fn issue_action_token(
        &self,
        request: IssueNotificationActionTokenRequest,
    ) -> Result<IssueActionTokenDecision, NotifyError> {
        let (delivery_sequence_i64, issued_at_i64, expires_at_i64) =
            validate_issue_token_request(&request)?;

        let mut connection = self.lock()?;
        let _ = resolve_subscription(&connection, request.notification_id)?;
        schema::ensure_v2(&mut connection)?;

        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let _ = resolve_subscription(&transaction, request.notification_id)?;

        if let Some(replayed) = replay_action_token_if_present(&transaction, &request)? {
            return Ok(IssueActionTokenDecision::Replayed(replayed));
        }

        let token_id = action_token_id_for(&request);
        let secret_proof = derive_action_secret_proof(
            token_id,
            request.notification_id,
            request.application_id,
            request.task_id,
            request.delivery_sequence,
            request.action_code,
            &request.secret_nonce,
        );
        let secret_commitment = action_secret_commitment(&secret_proof);

        transaction.execute(
            "INSERT INTO notify_action_tokens (
                token_id, idempotency_key, notification_id, application_id, task_id,
                delivery_sequence, action_code, secret_commitment, issued_at_ms, expires_at_ms,
                status, redeemed_at_ms, redeem_receipt_id, revoked_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 'ISSUED', NULL, NULL, NULL)",
            params![
                token_id.as_bytes().as_slice(),
                request.idempotency_key.as_bytes().as_slice(),
                request.notification_id.as_bytes().as_slice(),
                request.application_id.as_bytes().as_slice(),
                request.task_id.as_bytes().as_slice(),
                delivery_sequence_i64,
                i64::from(request.action_code),
                secret_commitment.as_slice(),
                issued_at_i64,
                expires_at_i64,
            ],
        )?;
        transaction.commit()?;

        Ok(IssueActionTokenDecision::Issued(NotificationActionToken {
            token_id,
            notification_id: request.notification_id,
            application_id: request.application_id,
            task_id: request.task_id,
            delivery_sequence: request.delivery_sequence,
            action_code: request.action_code,
            issued_at_ms: request.issued_at_ms,
            expires_at_ms: request.expires_at_ms,
            secret_proof,
        }))
    }

    /// Redeems a one-shot notification action token (`DUI-NOTIFY-001`),
    /// enforcing strict binding to `(NotificationId, ApplicationId, TaskId,
    /// action_code)` and strict single-consumption semantics.
    ///
    /// # Errors
    ///
    /// Fails closed with distinct typed [`NotifyError`] variants on:
    /// 1. Unknown `token_id` ([`NotifyError::ActionTokenNotFound`]);
    /// 2. Cross-`Application` spoofing
    ///    ([`NotifyError::ActionCrossApplicationSpoof`]);
    /// 3. Wrong `NotificationId`, `TaskId`, or `action_code`
    ///    ([`NotifyError::ActionBindingMismatch`]);
    /// 4. Invalid `presented_proof`
    ///    ([`NotifyError::ActionTokenProofInvalid`]);
    /// 5. Expired token (`redeemed_at_ms > expires_at_ms` ->
    ///    [`NotifyError::ActionTokenExpired`]);
    /// 6. Second use / replay of an already redeemed token
    ///    ([`NotifyError::ActionTokenAlreadyRedeemed`]).
    pub fn redeem_action_token(
        &self,
        request: RedeemNotificationActionRequest,
    ) -> Result<NotificationActionRedeemReceipt, NotifyError> {
        let redeemed_at_i64 = i64::try_from(request.redeemed_at_ms).map_err(|_| {
            NotifyError::InvalidActionTokenRequest("redeemed_at_ms exceeds SQLite i64")
        })?;

        let mut connection = self.lock()?;
        schema::ensure_v2(&mut connection)?;

        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let stored = load_action_token(&transaction, request.token_id)?
            .ok_or(NotifyError::ActionTokenNotFound(request.token_id))?;

        if request.caller_application_id != stored.application_id {
            return Err(NotifyError::ActionCrossApplicationSpoof {
                expected: stored.application_id,
                actual: request.caller_application_id,
            });
        }
        if request.caller_notification_id != stored.notification_id {
            return Err(NotifyError::ActionBindingMismatch(
                "caller_notification_id does not match bound notification_id",
            ));
        }
        if request.caller_task_id != stored.task_id {
            return Err(NotifyError::ActionBindingMismatch(
                "caller_task_id does not match bound task_id",
            ));
        }
        if request.action_code != stored.action_code {
            return Err(NotifyError::ActionBindingMismatch(
                "action_code does not match bound action_code",
            ));
        }

        let presented_commitment = action_secret_commitment(&request.presented_proof);
        if !constant_time_eq_32(&presented_commitment, &stored.secret_commitment) {
            return Err(NotifyError::ActionTokenProofInvalid(request.token_id));
        }

        match stored.status {
            ActionTokenStatus::Redeemed {
                redeemed_at_ms,
                receipt_id,
            } => {
                return Err(NotifyError::ActionTokenAlreadyRedeemed {
                    first_redeemed_at_ms: redeemed_at_ms,
                    receipt_id,
                });
            }
            ActionTokenStatus::Revoked { revoked_at_ms } => {
                return Err(NotifyError::ActionTokenRevoked { revoked_at_ms });
            }
            ActionTokenStatus::Issued => {}
        }

        if request.redeemed_at_ms > stored.expires_at_ms {
            return Err(NotifyError::ActionTokenExpired {
                expires_at_ms: stored.expires_at_ms,
                redeemed_at_ms: request.redeemed_at_ms,
            });
        }
        if request.redeemed_at_ms < stored.issued_at_ms {
            return Err(NotifyError::InvalidActionTokenRequest(
                "redeemed_at_ms precedes issued_at_ms",
            ));
        }

        let receipt_id = redeem_receipt_id_for(&stored, request.redeemed_at_ms);
        let updated = transaction.execute(
            "UPDATE notify_action_tokens
             SET status='REDEEMED', redeemed_at_ms=?1, redeem_receipt_id=?2
             WHERE token_id=?3 AND status='ISSUED'",
            params![
                redeemed_at_i64,
                receipt_id.as_bytes().as_slice(),
                request.token_id.as_bytes().as_slice(),
            ],
        )?;
        if updated != 1 {
            return Err(NotifyError::CorruptRecord(
                "expected single ISSUED action token row transition",
            ));
        }
        transaction.commit()?;

        Ok(NotificationActionRedeemReceipt {
            receipt_id,
            token_id: stored.token_id,
            notification_id: stored.notification_id,
            application_id: stored.application_id,
            task_id: stored.task_id,
            delivery_sequence: stored.delivery_sequence,
            action_code: stored.action_code,
            redeemed_at_ms: request.redeemed_at_ms,
        })
    }

    /// Administratively revokes an unredeemed action token so it can never be
    /// redeemed afterwards.
    ///
    /// # Errors
    ///
    /// Fails closed if `token_id` is unknown, if the token was already
    /// redeemed, or on `SQLite` errors.
    pub fn revoke_action_token(
        &self,
        token_id: ActionTokenId,
        revoked_at_ms: u64,
    ) -> Result<(), NotifyError> {
        let revoked_at_i64 = i64::try_from(revoked_at_ms).map_err(|_| {
            NotifyError::InvalidActionTokenRequest("revoked_at_ms exceeds SQLite i64")
        })?;
        let mut connection = self.lock()?;
        schema::ensure_v2(&mut connection)?;

        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let stored = load_action_token(&transaction, token_id)?
            .ok_or(NotifyError::ActionTokenNotFound(token_id))?;
        match stored.status {
            ActionTokenStatus::Redeemed {
                redeemed_at_ms,
                receipt_id,
            } => {
                return Err(NotifyError::ActionTokenAlreadyRedeemed {
                    first_redeemed_at_ms: redeemed_at_ms,
                    receipt_id,
                });
            }
            ActionTokenStatus::Revoked { .. } => return Ok(()),
            ActionTokenStatus::Issued => {}
        }

        transaction.execute(
            "UPDATE notify_action_tokens
             SET status='REVOKED', revoked_at_ms=?1
             WHERE token_id=?2 AND status='ISSUED'",
            params![revoked_at_i64, token_id.as_bytes().as_slice()],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Inspects the durable verifier metadata and status of an action token.
    ///
    /// # Errors
    ///
    /// Fails closed with [`NotifyError::ActionTokenNotFound`] when `token_id`
    /// is unknown or on `SQLite`/corruption errors.
    pub fn inspect_action_token(
        &self,
        token_id: ActionTokenId,
    ) -> Result<ActionTokenRecord, NotifyError> {
        let mut connection = self.lock()?;
        schema::ensure_v2(&mut connection)?;
        load_action_token(&connection, token_id)?.ok_or(NotifyError::ActionTokenNotFound(token_id))
    }

    /// Records a durable offline notification history reference indexed by
    /// `(topic_id, channel_sequence)` (`DUI-OFFLINE-001`).
    ///
    /// Replays idempotently when the identical `(topic_id, channel_sequence)`
    /// reference already exists, and fails closed if conflicting metadata is
    /// presented for an existing sequence or if the sequence lies below the
    /// topic's monotonic prune watermark.
    ///
    /// # Errors
    ///
    /// Fails closed on zero `channel_sequence`, pruned sequence insertion,
    /// conflicting duplicate `(topic_id, channel_sequence)`, or `SQLite`
    /// errors.
    pub fn record_delivery_history(
        &self,
        request: RecordDeliveryHistoryRequest,
    ) -> Result<RecordHistoryDecision, NotifyError> {
        if request.channel_sequence == 0 {
            return Err(NotifyError::InvalidHistoryPagination(
                "channel_sequence must be non-zero",
            ));
        }
        let seq_i64 = i64::try_from(request.channel_sequence).map_err(|_| {
            NotifyError::InvalidHistoryPagination("channel_sequence exceeds SQLite i64")
        })?;
        let recorded_at_i64 = i64::try_from(request.recorded_at_ms).map_err(|_| {
            NotifyError::InvalidHistoryPagination("recorded_at_ms exceeds SQLite i64")
        })?;

        let mut connection = self.lock()?;
        schema::ensure_v2(&mut connection)?;

        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let retained_from = load_prune_watermark(&transaction, request.topic_id)?;
        if request.channel_sequence < retained_from {
            return Err(NotifyError::HistorySequencePruned {
                topic_id: request.topic_id,
                channel_sequence: request.channel_sequence,
                retained_from_sequence: retained_from,
            });
        }

        if let Some(existing) =
            load_history_entry(&transaction, request.topic_id, request.channel_sequence)?
        {
            if existing.channel_id != request.channel_id
                || existing.payload_digest != request.payload_digest
                || existing.task_metadata != request.task_metadata
            {
                return Err(NotifyError::HistoryConflict {
                    topic_id: request.topic_id,
                    channel_sequence: request.channel_sequence,
                });
            }
            return Ok(RecordHistoryDecision::Replayed(existing));
        }

        insert_history_row(&transaction, &request, seq_i64, recorded_at_i64)?;
        transaction.commit()?;

        Ok(RecordHistoryDecision::Recorded(NotificationHistoryEntry {
            topic_id: request.topic_id,
            channel_id: request.channel_id,
            channel_sequence: request.channel_sequence,
            payload_digest: request.payload_digest,
            recorded_at_ms: request.recorded_at_ms,
            task_metadata: request.task_metadata,
        }))
    }

    /// Queries a bounded, deterministically ordered offline notification
    /// history page strictly after `after_sequence` (`DUI-OFFLINE-001`).
    ///
    /// Pass `after_sequence = 0` for the first page, and feed
    /// `page.next_after_sequence` into subsequent calls while `page.has_more`
    /// is `true`.
    ///
    /// # Errors
    ///
    /// Fails closed when `limit == 0` or `limit > MAX_HISTORY_PAGE_LIMIT`, or
    /// on `SQLite`/corruption errors.
    pub fn query_notification_history_page(
        &self,
        topic_id: TopicId,
        after_sequence: u64,
        limit: usize,
    ) -> Result<NotificationHistoryPage, NotifyError> {
        if limit == 0 {
            return Err(NotifyError::InvalidHistoryPagination(
                "limit must be non-zero",
            ));
        }
        if limit > MAX_HISTORY_PAGE_LIMIT {
            return Err(NotifyError::InvalidHistoryPagination(
                "limit exceeds MAX_HISTORY_PAGE_LIMIT",
            ));
        }
        let after_i64 = i64::try_from(after_sequence).map_err(|_| {
            NotifyError::InvalidHistoryPagination("after_sequence exceeds SQLite i64")
        })?;
        let fetch_limit_i64 = i64::try_from(limit + 1)
            .map_err(|_| NotifyError::InvalidHistoryPagination("limit exceeds SQLite i64"))?;

        let mut connection = self.lock()?;
        schema::ensure_v2(&mut connection)?;
        let retained_from_sequence = load_prune_watermark(&connection, topic_id)?;

        let mut statement = connection.prepare(
            "SELECT
                topic_id,
                channel_sequence,
                channel_id,
                payload_digest,
                recorded_at_ms,
                entry_kind,
                task_id,
                application_id,
                task_generation,
                transition_tag,
                requires_user_action,
                summary_digest
             FROM notify_history_refs
             WHERE topic_id=?1 AND channel_sequence > ?2
             ORDER BY channel_sequence ASC
             LIMIT ?3",
        )?;
        let raw_rows = statement
            .query_map(
                params![topic_id.as_bytes().as_slice(), after_i64, fetch_limit_i64],
                history_columns,
            )?
            .collect::<Result<Vec<_>, _>>()?;

        let mut entries = Vec::with_capacity(raw_rows.len().min(limit));
        for columns in raw_rows {
            entries.push(history_from_columns(columns)?);
        }

        let has_more = entries.len() > limit;
        if has_more {
            entries.truncate(limit);
        }
        let next_after_sequence = entries.last().map(|entry| entry.channel_sequence);

        Ok(NotificationHistoryPage {
            topic_id,
            entries,
            next_after_sequence,
            has_more,
            retained_from_sequence,
        })
    }

    /// Prunes offline notification history entries with
    /// `channel_sequence < retain_from_sequence` for `topic_id`, advancing a
    /// monotonic retention watermark (`DUI-OFFLINE-001`).
    ///
    /// # Errors
    ///
    /// Fails closed if `retain_from_sequence` regresses below the previously
    /// committed watermark for `topic_id` or on `SQLite` errors.
    pub fn prune_history_before(
        &self,
        topic_id: TopicId,
        retain_from_sequence: u64,
    ) -> Result<HistoryPruneReceipt, NotifyError> {
        let retain_i64 = i64::try_from(retain_from_sequence).map_err(|_| {
            NotifyError::InvalidHistoryPagination("retain_from_sequence exceeds SQLite i64")
        })?;

        let mut connection = self.lock()?;
        schema::ensure_v2(&mut connection)?;

        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current_watermark = load_prune_watermark(&transaction, topic_id)?;
        if retain_from_sequence < current_watermark {
            return Err(NotifyError::InvalidHistoryPagination(
                "retain_from_sequence must not regress below current watermark",
            ));
        }

        transaction.execute(
            "INSERT INTO notify_history_prune_watermarks (topic_id, retained_from_sequence)
             VALUES (?1, ?2)
             ON CONFLICT(topic_id) DO UPDATE SET
                retained_from_sequence=excluded.retained_from_sequence",
            params![topic_id.as_bytes().as_slice(), retain_i64],
        )?;
        let deleted = transaction.execute(
            "DELETE FROM notify_history_refs
             WHERE topic_id=?1 AND channel_sequence < ?2",
            params![topic_id.as_bytes().as_slice(), retain_i64],
        )?;
        transaction.commit()?;

        let pruned_entries = u64::try_from(deleted)
            .map_err(|_| NotifyError::CorruptRecord("deleted row count overflows u64"))?;
        Ok(HistoryPruneReceipt {
            topic_id,
            retained_from_sequence: retain_from_sequence,
            pruned_entries,
        })
    }
}

fn validate_issue_token_request(
    request: &IssueNotificationActionTokenRequest,
) -> Result<(i64, i64, i64), NotifyError> {
    if request.delivery_sequence == 0 {
        return Err(NotifyError::InvalidActionTokenRequest(
            "delivery_sequence must be non-zero",
        ));
    }
    if request.expires_at_ms < request.issued_at_ms {
        return Err(NotifyError::InvalidActionTokenRequest(
            "expires_at_ms must be greater than or equal to issued_at_ms",
        ));
    }
    if request.secret_nonce == [0u8; 32] {
        return Err(NotifyError::InvalidActionTokenRequest(
            "secret_nonce must be non-zero",
        ));
    }
    let delivery_sequence_i64 = i64::try_from(request.delivery_sequence).map_err(|_| {
        NotifyError::InvalidActionTokenRequest("delivery_sequence exceeds SQLite i64")
    })?;
    let issued_at_i64 = i64::try_from(request.issued_at_ms)
        .map_err(|_| NotifyError::InvalidActionTokenRequest("issued_at_ms exceeds SQLite i64"))?;
    let expires_at_i64 = i64::try_from(request.expires_at_ms)
        .map_err(|_| NotifyError::InvalidActionTokenRequest("expires_at_ms exceeds SQLite i64"))?;
    Ok((delivery_sequence_i64, issued_at_i64, expires_at_i64))
}

fn replay_action_token_if_present(
    connection: &Connection,
    request: &IssueNotificationActionTokenRequest,
) -> Result<Option<NotificationActionToken>, NotifyError> {
    let Some(existing) = load_action_token_by_key(connection, request.idempotency_key)? else {
        return Ok(None);
    };
    let candidate_proof = derive_action_secret_proof(
        existing.token_id,
        request.notification_id,
        request.application_id,
        request.task_id,
        request.delivery_sequence,
        request.action_code,
        &request.secret_nonce,
    );
    let candidate_commitment = action_secret_commitment(&candidate_proof);
    let bindings_match = existing.notification_id == request.notification_id
        && existing.application_id == request.application_id
        && existing.task_id == request.task_id
        && existing.delivery_sequence == request.delivery_sequence
        && existing.action_code == request.action_code
        && existing.expires_at_ms == request.expires_at_ms
        && constant_time_eq_32(&existing.secret_commitment, &candidate_commitment);
    if !bindings_match {
        return Err(NotifyError::ActionIdempotencyConflict(
            request.idempotency_key,
        ));
    }
    Ok(Some(NotificationActionToken {
        token_id: existing.token_id,
        notification_id: existing.notification_id,
        application_id: existing.application_id,
        task_id: existing.task_id,
        delivery_sequence: existing.delivery_sequence,
        action_code: existing.action_code,
        issued_at_ms: existing.issued_at_ms,
        expires_at_ms: existing.expires_at_ms,
        secret_proof: candidate_proof,
    }))
}

fn insert_history_row(
    connection: &Connection,
    request: &RecordDeliveryHistoryRequest,
    seq_i64: i64,
    recorded_at_i64: i64,
) -> Result<(), NotifyError> {
    match &request.task_metadata {
        None => {
            connection.execute(
                "INSERT INTO notify_history_refs (
                    topic_id, channel_sequence, channel_id, payload_digest, recorded_at_ms,
                    entry_kind, task_id, application_id, task_generation, transition_tag,
                    requires_user_action, summary_digest
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 'GENERIC', NULL, NULL, NULL, NULL, NULL, NULL)",
                params![
                    request.topic_id.as_bytes().as_slice(),
                    seq_i64,
                    request.channel_id.as_bytes().as_slice(),
                    request.payload_digest.as_slice(),
                    recorded_at_i64,
                ],
            )?;
        }
        Some(meta) => {
            let task_gen_i64 = i64::try_from(meta.task_generation.get()).map_err(|_| {
                NotifyError::InvalidHistoryPagination("task_generation exceeds SQLite i64")
            })?;
            connection.execute(
                "INSERT INTO notify_history_refs (
                    topic_id, channel_sequence, channel_id, payload_digest, recorded_at_ms,
                    entry_kind, task_id, application_id, task_generation, transition_tag,
                    requires_user_action, summary_digest
                 ) VALUES (?1, ?2, ?3, ?4, ?5, 'TASK_TRANSITION', ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    request.topic_id.as_bytes().as_slice(),
                    seq_i64,
                    request.channel_id.as_bytes().as_slice(),
                    request.payload_digest.as_slice(),
                    recorded_at_i64,
                    meta.task_id.as_bytes().as_slice(),
                    meta.application_id.as_bytes().as_slice(),
                    task_gen_i64,
                    i64::from(meta.transition.wire_tag()),
                    i64::from(u8::from(meta.requires_user_action)),
                    meta.summary_digest.as_slice(),
                ],
            )?;
        }
    }
    Ok(())
}

fn notification_id_for(topic_id: TopicId, subscriber_key: SubscriberKey) -> NotificationId {
    let mut hasher = Sha256::new();
    hasher.update(b"nlos/notify/id/v1");
    hasher.update(topic_id.as_bytes());
    hasher.update(subscriber_key.as_bytes());
    NotificationId::from_bytes(hasher.finalize()[..16].try_into().expect("16 of 32 bytes"))
}

fn commit_notify_key_for(publication_key: IdempotencyKey) -> IdempotencyKey {
    let mut hasher = Sha256::new();
    hasher.update(b"nlos/notify/commit-notify/v1");
    hasher.update(publication_key.as_bytes());
    IdempotencyKey::from_bytes(hasher.finalize()[..16].try_into().expect("16 of 32 bytes"))
}

fn action_token_id_for(request: &IssueNotificationActionTokenRequest) -> ActionTokenId {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/notify-action-token-id/v1");
    hasher.update(request.idempotency_key.as_bytes());
    hasher.update(request.notification_id.as_bytes());
    hasher.update(request.application_id.as_bytes());
    hasher.update(request.task_id.as_bytes());
    hasher.update(request.delivery_sequence.to_be_bytes());
    hasher.update(request.action_code.to_be_bytes());
    ActionTokenId::from_bytes(hasher.finalize()[..16].try_into().expect("16 of 32 bytes"))
}

fn redeem_receipt_id_for(stored: &ActionTokenRecord, redeemed_at_ms: u64) -> ReceiptId {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/notify-action-redeem-receipt/v1");
    hasher.update(stored.token_id.as_bytes());
    hasher.update(stored.notification_id.as_bytes());
    hasher.update(stored.application_id.as_bytes());
    hasher.update(stored.task_id.as_bytes());
    hasher.update(stored.delivery_sequence.to_be_bytes());
    hasher.update(stored.action_code.to_be_bytes());
    hasher.update(redeemed_at_ms.to_be_bytes());
    ReceiptId::from_bytes(hasher.finalize()[..16].try_into().expect("16 of 32 bytes"))
}

fn constant_time_eq_32(left: &[u8; 32], right: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for index in 0..32 {
        diff |= left[index] ^ right[index];
    }
    diff == 0
}

fn upsert_subscription(
    connection: &Connection,
    subscription: &NotificationSubscription,
) -> Result<(), NotifyError> {
    connection.execute(
        "INSERT INTO notify_subscriptions
            (notification_id, topic_id, subscriber_key, consume_token, registered_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(notification_id) DO UPDATE SET
            consume_token=excluded.consume_token,
            registered_at_ms=excluded.registered_at_ms",
        params![
            subscription.notification_id.as_bytes().as_slice(),
            subscription.topic_id.as_bytes().as_slice(),
            subscription.subscriber_key.as_bytes().as_slice(),
            &subscription.consume_token[..],
            i64::try_from(subscription.registered_at_ms)
                .map_err(|_| { NotifyError::CorruptRecord("u64 exceeds SQLite i64") })?,
        ],
    )?;
    Ok(())
}

type SubscriptionColumns = (Vec<u8>, Vec<u8>, Vec<u8>, Vec<u8>, i64);

fn subscription_columns(row: &rusqlite::Row<'_>) -> rusqlite::Result<SubscriptionColumns> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
    ))
}

fn subscription_from_columns(
    columns: SubscriptionColumns,
) -> Result<NotificationSubscription, NotifyError> {
    let (notification_id, topic_id, subscriber_key, consume_token, registered_at_ms) = columns;
    Ok(NotificationSubscription {
        notification_id: NotificationId::from_bytes(array16(
            notification_id,
            "notification id length is not 16",
        )?),
        topic_id: TopicId::from_bytes(array16(topic_id, "topic id length is not 16")?),
        subscriber_key: SubscriberKey::from_bytes(array16(
            subscriber_key,
            "subscriber key length is not 16",
        )?),
        consume_token: array32(consume_token, "consume token length is not 32")?,
        registered_at_ms: u64::try_from(registered_at_ms)
            .map_err(|_| NotifyError::CorruptRecord("negative integer"))?,
    })
}

fn array16(bytes: Vec<u8>, reason: &'static str) -> Result<[u8; 16], NotifyError> {
    bytes
        .try_into()
        .map_err(|_| NotifyError::CorruptRecord(reason))
}

fn array32(bytes: Vec<u8>, reason: &'static str) -> Result<[u8; 32], NotifyError> {
    bytes
        .try_into()
        .map_err(|_| NotifyError::CorruptRecord(reason))
}

fn verify_subscription(
    subscription: NotificationSubscription,
) -> Result<NotificationSubscription, NotifyError> {
    if subscription.notification_id
        != notification_id_for(subscription.topic_id, subscription.subscriber_key)
    {
        return Err(NotifyError::CorruptRecord(
            "stored notification id does not derive from its reference fields",
        ));
    }
    Ok(subscription)
}

fn resolve_subscription(
    connection: &Connection,
    notification_id: NotificationId,
) -> Result<NotificationSubscription, NotifyError> {
    connection
        .query_row(
            "SELECT notification_id, topic_id, subscriber_key, consume_token, registered_at_ms
             FROM notify_subscriptions WHERE notification_id=?1",
            params![notification_id.as_bytes().as_slice()],
            subscription_columns,
        )
        .optional()?
        .map(subscription_from_columns)
        .transpose()?
        .ok_or(NotifyError::NotificationNotFound(notification_id))
        .and_then(verify_subscription)
}

fn load_all_subscriptions(
    connection: &Connection,
) -> Result<Vec<NotificationSubscription>, NotifyError> {
    let mut statement = connection.prepare(
        "SELECT notification_id, topic_id, subscriber_key, consume_token, registered_at_ms
         FROM notify_subscriptions ORDER BY notification_id",
    )?;
    let rows = statement
        .query_map([], subscription_columns)?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter()
        .map(subscription_from_columns)
        .map(|result| result.and_then(verify_subscription))
        .collect()
}

fn delete_subscription(
    connection: &Connection,
    notification_id: NotificationId,
) -> Result<(), NotifyError> {
    connection.execute(
        "DELETE FROM notify_subscriptions WHERE notification_id=?1",
        params![notification_id.as_bytes().as_slice()],
    )?;
    Ok(())
}

type ActionTokenColumns = (
    Vec<u8>,
    Vec<u8>,
    Vec<u8>,
    Vec<u8>,
    Vec<u8>,
    i64,
    i64,
    Vec<u8>,
    i64,
    i64,
    String,
    Option<i64>,
    Option<Vec<u8>>,
    Option<i64>,
);

fn action_token_columns(row: &rusqlite::Row<'_>) -> rusqlite::Result<ActionTokenColumns> {
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
        row.get(13)?,
    ))
}

fn action_token_from_columns(
    columns: ActionTokenColumns,
) -> Result<ActionTokenRecord, NotifyError> {
    let (
        token_id,
        idempotency_key,
        notification_id,
        application_id,
        task_id,
        delivery_sequence,
        action_code,
        secret_commitment,
        issued_at_ms,
        expires_at_ms,
        status_raw,
        redeemed_at_ms,
        redeem_receipt_id,
        revoked_at_ms,
    ) = columns;

    let status = match status_raw.as_str() {
        "ISSUED" => {
            if redeemed_at_ms.is_some() || redeem_receipt_id.is_some() || revoked_at_ms.is_some() {
                return Err(NotifyError::CorruptRecord(
                    "ISSUED action token carries non-null terminal fields",
                ));
            }
            ActionTokenStatus::Issued
        }
        "REDEEMED" => {
            let at = redeemed_at_ms.ok_or(NotifyError::CorruptRecord(
                "REDEEMED token missing redeemed_at_ms",
            ))?;
            let receipt_bytes = redeem_receipt_id.ok_or(NotifyError::CorruptRecord(
                "REDEEMED token missing redeem_receipt_id",
            ))?;
            if revoked_at_ms.is_some() {
                return Err(NotifyError::CorruptRecord(
                    "REDEEMED token carries revoked_at_ms",
                ));
            }
            ActionTokenStatus::Redeemed {
                redeemed_at_ms: u64::try_from(at)
                    .map_err(|_| NotifyError::CorruptRecord("negative redeemed_at_ms"))?,
                receipt_id: ReceiptId::from_bytes(array16(
                    receipt_bytes,
                    "redeem_receipt_id length is not 16",
                )?),
            }
        }
        "REVOKED" => {
            let at = revoked_at_ms.ok_or(NotifyError::CorruptRecord(
                "REVOKED token missing revoked_at_ms",
            ))?;
            if redeemed_at_ms.is_some() || redeem_receipt_id.is_some() {
                return Err(NotifyError::CorruptRecord(
                    "REVOKED token carries redeemed fields",
                ));
            }
            ActionTokenStatus::Revoked {
                revoked_at_ms: u64::try_from(at)
                    .map_err(|_| NotifyError::CorruptRecord("negative revoked_at_ms"))?,
            }
        }
        _ => return Err(NotifyError::CorruptRecord("unknown action token status")),
    };

    let delivery_sequence_u64 = u64::try_from(delivery_sequence)
        .map_err(|_| NotifyError::CorruptRecord("invalid action token delivery_sequence"))?;
    if delivery_sequence_u64 == 0 {
        return Err(NotifyError::CorruptRecord(
            "zero action token delivery_sequence",
        ));
    }
    let action_code_u16 = u16::try_from(action_code)
        .map_err(|_| NotifyError::CorruptRecord("action_code out of u16 range"))?;

    Ok(ActionTokenRecord {
        token_id: ActionTokenId::from_bytes(array16(token_id, "token_id length is not 16")?),
        idempotency_key: IdempotencyKey::from_bytes(array16(
            idempotency_key,
            "idempotency_key length is not 16",
        )?),
        notification_id: NotificationId::from_bytes(array16(
            notification_id,
            "notification_id length is not 16",
        )?),
        application_id: ApplicationId::from_bytes(array16(
            application_id,
            "application_id length is not 16",
        )?),
        task_id: TaskId::from_bytes(array16(task_id, "task_id length is not 16")?),
        delivery_sequence: delivery_sequence_u64,
        action_code: action_code_u16,
        secret_commitment: array32(secret_commitment, "secret_commitment length is not 32")?,
        issued_at_ms: u64::try_from(issued_at_ms)
            .map_err(|_| NotifyError::CorruptRecord("negative issued_at_ms"))?,
        expires_at_ms: u64::try_from(expires_at_ms)
            .map_err(|_| NotifyError::CorruptRecord("negative expires_at_ms"))?,
        status,
    })
}

fn load_action_token(
    connection: &Connection,
    token_id: ActionTokenId,
) -> Result<Option<ActionTokenRecord>, NotifyError> {
    connection
        .query_row(
            "SELECT
                token_id,
                idempotency_key,
                notification_id,
                application_id,
                task_id,
                delivery_sequence,
                action_code,
                secret_commitment,
                issued_at_ms,
                expires_at_ms,
                status,
                redeemed_at_ms,
                redeem_receipt_id,
                revoked_at_ms
             FROM notify_action_tokens
             WHERE token_id=?1",
            params![token_id.as_bytes().as_slice()],
            action_token_columns,
        )
        .optional()?
        .map(action_token_from_columns)
        .transpose()
}

fn load_action_token_by_key(
    connection: &Connection,
    idempotency_key: IdempotencyKey,
) -> Result<Option<ActionTokenRecord>, NotifyError> {
    connection
        .query_row(
            "SELECT
                token_id,
                idempotency_key,
                notification_id,
                application_id,
                task_id,
                delivery_sequence,
                action_code,
                secret_commitment,
                issued_at_ms,
                expires_at_ms,
                status,
                redeemed_at_ms,
                redeem_receipt_id,
                revoked_at_ms
             FROM notify_action_tokens
             WHERE idempotency_key=?1",
            params![idempotency_key.as_bytes().as_slice()],
            action_token_columns,
        )
        .optional()?
        .map(action_token_from_columns)
        .transpose()
}

type HistoryColumns = (
    Vec<u8>,
    i64,
    Vec<u8>,
    Vec<u8>,
    i64,
    String,
    Option<Vec<u8>>,
    Option<Vec<u8>>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<Vec<u8>>,
);

fn history_columns(row: &rusqlite::Row<'_>) -> rusqlite::Result<HistoryColumns> {
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
    ))
}

fn history_from_columns(columns: HistoryColumns) -> Result<NotificationHistoryEntry, NotifyError> {
    let (
        topic_id,
        channel_sequence,
        channel_id,
        payload_digest,
        recorded_at_ms,
        entry_kind,
        task_id,
        application_id,
        task_generation,
        transition_tag,
        requires_user_action,
        summary_digest,
    ) = columns;

    let seq_u64 = u64::try_from(channel_sequence)
        .map_err(|_| NotifyError::CorruptRecord("negative history channel_sequence"))?;
    if seq_u64 == 0 {
        return Err(NotifyError::CorruptRecord("zero history channel_sequence"));
    }

    let task_metadata = match entry_kind.as_str() {
        "GENERIC" => None,
        "TASK_TRANSITION" => {
            let task_bytes = task_id.ok_or(NotifyError::CorruptRecord(
                "TASK_TRANSITION missing task_id",
            ))?;
            let app_bytes = application_id.ok_or(NotifyError::CorruptRecord(
                "TASK_TRANSITION missing application_id",
            ))?;
            let gen_raw = task_generation.ok_or(NotifyError::CorruptRecord(
                "TASK_TRANSITION missing task_generation",
            ))?;
            let gen_u64 = u64::try_from(gen_raw)
                .map_err(|_| NotifyError::CorruptRecord("negative task_generation"))?;
            let generation = NonZeroU64::new(gen_u64)
                .map(Generation::new)
                .ok_or(NotifyError::CorruptRecord("zero task_generation"))?;
            let tag_raw = transition_tag.ok_or(NotifyError::CorruptRecord(
                "TASK_TRANSITION missing transition_tag",
            ))?;
            let tag_u8 = u8::try_from(tag_raw)
                .map_err(|_| NotifyError::CorruptRecord("transition_tag out of u8 range"))?;
            let transition = TaskTransitionKind::from_wire_tag(tag_u8)
                .ok_or(NotifyError::CorruptRecord("unknown stored transition_tag"))?;
            let action_raw = requires_user_action.ok_or(NotifyError::CorruptRecord(
                "TASK_TRANSITION missing requires_user_action",
            ))?;
            let requires_action = match action_raw {
                0 => false,
                1 => true,
                _ => {
                    return Err(NotifyError::CorruptRecord(
                        "invalid stored requires_user_action",
                    ));
                }
            };
            let summary_bytes = summary_digest.ok_or(NotifyError::CorruptRecord(
                "TASK_TRANSITION missing summary_digest",
            ))?;
            Some(TaskHistoryMetadata {
                task_id: TaskId::from_bytes(array16(task_bytes, "history task_id length != 16")?),
                application_id: ApplicationId::from_bytes(array16(
                    app_bytes,
                    "history application_id length != 16",
                )?),
                task_generation: generation,
                transition,
                requires_user_action: requires_action,
                summary_digest: array32(summary_bytes, "history summary_digest length != 32")?,
            })
        }
        _ => return Err(NotifyError::CorruptRecord("unknown history entry_kind")),
    };

    Ok(NotificationHistoryEntry {
        topic_id: TopicId::from_bytes(array16(topic_id, "history topic_id length != 16")?),
        channel_id: ChannelId::from_bytes(array16(channel_id, "history channel_id length != 16")?),
        channel_sequence: seq_u64,
        payload_digest: array32(payload_digest, "history payload_digest length != 32")?,
        recorded_at_ms: u64::try_from(recorded_at_ms)
            .map_err(|_| NotifyError::CorruptRecord("negative history recorded_at_ms"))?,
        task_metadata,
    })
}

fn load_history_entry(
    connection: &Connection,
    topic_id: TopicId,
    channel_sequence: u64,
) -> Result<Option<NotificationHistoryEntry>, NotifyError> {
    let seq_i64 = i64::try_from(channel_sequence)
        .map_err(|_| NotifyError::CorruptRecord("channel_sequence exceeds SQLite i64"))?;
    connection
        .query_row(
            "SELECT
                topic_id,
                channel_sequence,
                channel_id,
                payload_digest,
                recorded_at_ms,
                entry_kind,
                task_id,
                application_id,
                task_generation,
                transition_tag,
                requires_user_action,
                summary_digest
             FROM notify_history_refs
             WHERE topic_id=?1 AND channel_sequence=?2",
            params![topic_id.as_bytes().as_slice(), seq_i64],
            history_columns,
        )
        .optional()?
        .map(history_from_columns)
        .transpose()
}

fn load_prune_watermark(connection: &Connection, topic_id: TopicId) -> Result<u64, NotifyError> {
    let watermark = connection
        .query_row(
            "SELECT retained_from_sequence
             FROM notify_history_prune_watermarks
             WHERE topic_id=?1",
            params![topic_id.as_bytes().as_slice()],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    match watermark {
        None => Ok(0),
        Some(value) => u64::try_from(value)
            .map_err(|_| NotifyError::CorruptRecord("negative retained_from_sequence")),
    }
}
