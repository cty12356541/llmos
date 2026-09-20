//! Notification minimal service: a thin subscription face over the
//! Topic/Channel/Wait authorities (B-NOTIFY-001, W33-D / X-1 first half).
//!
//! Stage-B decision point 2 (stage-b-progress §6.5.4) fixed the boundary:
//! Notification is a *thin layer* over the existing Topic authority — no
//! new fanout machinery (ADR-0007's single-log fanout owns that), no
//! second source of truth.  Every canonical fact of this crate's surface
//! lives in an authority database:
//!
//! - the subscription, its cursor and its delivery state are
//!   [`nlos_topic::TopicAuthority`] rows;
//! - the message log (the single enqueue per publication) is the
//!   [`nlos_channel::ChannelAuthority`] queue;
//! - the durable "wake me at sequence N" registrations are
//!   [`nlos_wait::WaitAuthority`] rows.
//!
//! [`NotificationService`] holds only durable *references*: one small
//! table mapping a derived [`NotificationId`] to `(topic_id,
//! subscriber_key)` plus the authority-issued [`ConsumeToken`], so the
//! face can route delivery reads, token-authenticated acks and cancels to
//! the authority on the subscriber's behalf.  The face table has no
//! state, cursor, queue or payload column — an out-of-band authority
//! change is always observed live (see
//! [`NotificationService::list_subscriptions`]), never a copied snapshot.
//!
//! Cross-authority discipline follows the repository convention
//! (authority-first, idempotent replay converges): subscribe/cancel write
//! the authority decision first and the local reference second, so a crash
//! between the two leaves a replayable authority fact and a stale or
//! missing reference that the next identical call converges.  The publish
//! face additionally provides the ADR-0008 commit-side wiring the Topic
//! authority does not own: after the Topic authority's single enqueue, it
//! issues the explicit idempotent `notify_commits` to the Wait registry
//! with a notify key deterministically derived from the publication's
//! idempotency key, so replaying a publish re-reports the original wake
//! set without a second enqueue or wake flip.

use std::error::Error;
use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use nlos_channel::QueueEntryRecord;
use nlos_topic::{
    AdvanceDecision, AdvanceReceipt, ConsumeToken, PublicationRecord, PublishDecision,
    SubscribeDecision, SubscribeRequest, SubscriberKey, TopicAuthority, TopicAuthorityError,
    TopicId, UnsubscribeDecision, UnsubscribeReceipt, UnsubscribeRequest,
};
use nlos_types::IdempotencyKey;
use nlos_wait::{
    BindingId, NotifyCommitsRequest, RegisterDecision, RegisterWaitRequest, WaitAuthority,
    WaitAuthorityError, WakeReport,
};
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

mod schema;

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
        }
    }
}

impl Error for NotifyError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Topic(error) => Some(error),
            Self::Wait(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for NotifyError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

/// The notification service face: a single writer over its own small
/// reference database, delegating every canonical decision to the bound
/// authorities.
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
    /// # Errors
    ///
    /// Fails closed when `SQLite` cannot provide WAL/FULL durability or
    /// when a stored schema version is unknown.
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
            schema::SCHEMA_VERSION => {}
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
        consume_token: array32(consume_token)?,
        registered_at_ms: u64::try_from(registered_at_ms)
            .map_err(|_| NotifyError::CorruptRecord("negative integer"))?,
    })
}

fn array16(bytes: Vec<u8>, reason: &'static str) -> Result<[u8; 16], NotifyError> {
    bytes
        .try_into()
        .map_err(|_| NotifyError::CorruptRecord(reason))
}

fn array32(bytes: Vec<u8>) -> Result<[u8; 32], NotifyError> {
    bytes
        .try_into()
        .map_err(|_| NotifyError::CorruptRecord("consume token length is not 32"))
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
