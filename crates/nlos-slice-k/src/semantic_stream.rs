//! The semantic notification stream lane (W50): the production consumer of
//! the `semantic_outbox` rows every admission writes (the W49 gap — the
//! payload bridge's admits had no consumer, so the pending prefix grew
//! forever).
//!
//! Composition, nothing invented: every semantic, topic, channel and clock
//! semantic below is an already-landed authority API. This lane only fixes
//! well-known identities, a conservative policy, and a pump discipline:
//!
//! * **Bootstrap** ([`bootstrap_semantic_stream`]): one system Channel
//!   (`<root>/channel`, the daemon W46 path style) and one system Topic
//!   (`<root>/topics`) with identities derived domain-separated from the
//!   public seed — creation is idempotent per authority replay, so a reopen
//!   or crash-restart converges without double-creating anything.
//! * **Envelope** ([`semantic_stream_envelope`]): one fixed 73-byte
//!   notification per pending row — a *notification stream*, not a byte
//!   stream: the full canonical event stays behind the semantic authority's
//!   read paths; subscribers re-read there after being notified.
//! * **Pump** ([`SemanticStreamPump`]): a dedicated OS thread (the
//!   `nlos-runtime-tokio` pump discipline — named thread, `catch_unwind`
//!   guarded cycles, bounded exponential backoff, fault threshold,
//!   idempotent stop/join) that per cycle lists the pending outbox prefix,
//!   publishes one envelope per row under an event-derived idempotency key
//!   (a crash between publish and ack re-delivers with zero double-send),
//!   acknowledges the semantic outbox row under the runtime clock's
//!   read-only wall high-water, then — as the system subscriber — follows
//!   its own cursor (`poll` zero-write → `advance_with_token` → `compact`,
//!   the W45-C1 compact/high-water linkage) so the channel's bounded
//!   capacity is released instead of dead-ending on `QueueFull`.
//!
//! The stream is deliberately an *at-least-once notification* with
//! effectively-once delivery: re-delivery replays the same
//! event-id-derived publish key, and the semantic outbox row leaves the
//! pending prefix only after the topic authority durably enqueued the
//! envelope.

use std::error::Error;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use nlos_channel::{ChannelAuthority, ChannelRecord, CreateChannelRequest};
use nlos_clock::{AuthorityClock, NowRequest};
use nlos_semantic::{AcknowledgeOutboxRequest, SemanticAuthority, SemanticOutboxRow};
use nlos_topic::{
    AdvanceRequest, CreateTopicRequest, PublishDecision, PublishRequest, SubscribeRequest,
    SubscriberKey, SubscriptionRecord, TopicAuthority, TopicPolicy, TopicRecord,
};
use nlos_types::{ChannelId, IdempotencyKey, ResourceAccountId, SemanticEventId};
use sha2::{Digest, Sha256};

use crate::error::SliceKResult;

/// Domain separator of every well-known semantic-stream identity derivation
/// (channel/topic creation keys, topic policy fingerprint, payer binding,
/// system subscriber key, publish idempotency keys, bootstrap clock keys).
pub const SEMANTIC_STREAM_DOMAIN: &[u8] = b"llmos/slice-k/semantic-stream/v1";

/// The well-known system topic name: `llmos/semantic/admission-outbox/v1`.
pub const SEMANTIC_STREAM_TOPIC_NAME: &[u8] = b"llmos/semantic/admission-outbox/v1";

/// Width of one semantic notification envelope (see
/// [`semantic_stream_envelope`] for the byte layout).
pub const SEMANTIC_STREAM_ENVELOPE_BYTES: usize = 73;

/// Conservative default channel capacity: 16 MiB of envelopes (≈ 230k
/// envelopes at 73 bytes) released once per pump cycle.
const DEFAULT_CHANNEL_CAPACITY_BYTES: u64 = 16 * 1024 * 1024;

/// Conservative default topic retention bound: 16 MiB of unconsumed
/// backlog. This lane's pump follows its own cursor every cycle, so the
/// bound only guards the in-flight window of one batch; a lagging external
/// subscriber can still hold it (documented limitation, visible as a typed
/// `TopicRetentionExhausted` refusal in pump health).
const DEFAULT_RETAINED_BYTES: u64 = 16 * 1024 * 1024;

/// Conservative default retention age: one day of the oldest live entry
/// still held by an active subscriber.
const DEFAULT_RETENTION_MS: u64 = 24 * 60 * 60 * 1000;

/// Conservative default redelivery budget. The system subscriber is
/// *deliberately* lagging within each pump cycle (it publishes a batch,
/// then follows its cursor), so the durable billing advances once per
/// published envelope; the default keeps quarantine (a stopped pump poll,
/// surfaced as a typed pump-health error) far out of any slice-scale run.
const DEFAULT_DELIVERY_ATTEMPTS: u64 = 1_000_000;

/// Default fallback poll interval (also the failure-backoff base).
const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Default consecutive-failure threshold before the pump faults.
const DEFAULT_FAILURE_THRESHOLD: usize = 16;

/// Exponential-backoff cap as a multiple of the poll interval (the
/// `nlos-runtime-tokio` pump cap).
const BACKOFF_CAP_MULTIPLE: u32 = 64;

/// Floor for [`SemanticStreamConfig::poll_interval`]: a sub-millisecond
/// interval would degenerate into an unbacked busy-poll over durable
/// storage.
const MIN_POLL_INTERVAL: Duration = Duration::from_millis(1);

/// How long a bounded stream-pump stop waits for the thread join before
/// giving up on the join (the stop flag and hint are already delivered, so
/// the thread still exits).
const STOP_JOIN_DEADLINE: Duration = Duration::from_secs(5);

/// Tuning and durable-policy surface of the semantic stream lane.
///
/// Every field participates in one of two groups:
///
/// - **durable identity** — `channel_capacity_bytes` and the five
///   `topic_*` policy fields derive the well-known channel/topic creation
///   requests, so they are frozen per root: the bootstrap replays only the
///   exact original declarations, and a later start with a different value
///   against the same root fails closed with the authorities' typed
///   idempotency conflict;
/// - **runtime tuning** — `batch_limit`, `poll_interval` and
///   `failure_threshold` shape only the pump loop and may change freely
///   between starts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SemanticStreamConfig {
    /// System channel capacity in bytes. Must hold at least one full batch
    /// of envelopes (`batch_limit * 73`), enforced at pump start.
    pub channel_capacity_bytes: u64,
    /// Topic `max_recipients`: the system subscriber plus external
    /// observers of the notification stream.
    pub topic_max_recipients: u64,
    /// Topic `delivery_attempts` redelivery budget (see
    /// [`DEFAULT_DELIVERY_ATTEMPTS`]).
    pub topic_delivery_attempts: u64,
    /// Topic `cascade_depth`: this lane never republishes; the minimum
    /// declarable depth keeps the policy valid.
    pub topic_cascade_depth: u64,
    /// Topic `retained_bytes` backlog bound.
    pub topic_retained_bytes: u64,
    /// Topic `retention_ms` age bound.
    pub topic_retention_ms: u64,
    /// Pending rows listed (and envelopes published) per pump cycle.
    pub batch_limit: usize,
    /// Fallback poll interval and failure-backoff base.
    pub poll_interval: Duration,
    /// Consecutive cycle failures after which the pump faults.
    pub failure_threshold: usize,
}

impl Default for SemanticStreamConfig {
    fn default() -> Self {
        Self {
            channel_capacity_bytes: DEFAULT_CHANNEL_CAPACITY_BYTES,
            topic_max_recipients: 16,
            topic_delivery_attempts: DEFAULT_DELIVERY_ATTEMPTS,
            topic_cascade_depth: 1,
            topic_retained_bytes: DEFAULT_RETAINED_BYTES,
            topic_retention_ms: DEFAULT_RETENTION_MS,
            batch_limit: 32,
            poll_interval: DEFAULT_POLL_INTERVAL,
            failure_threshold: DEFAULT_FAILURE_THRESHOLD,
        }
    }
}

impl SemanticStreamConfig {
    /// Validates the pump-loop invariants: a positive batch, a
    /// sub-millisecond-free poll interval, and capacity/retention bounds
    /// that can hold one full batch of envelopes (otherwise the pump would
    /// deadlock itself on `QueueFull`/`TopicRetentionExhausted` inside its
    /// own batch).
    ///
    /// # Errors
    ///
    /// Returns the named invariant that failed; the caller surfaces it as
    /// the typed [`crate::SliceKError::Pump`] start refusal.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.batch_limit == 0 {
            return Err("batch_limit must be positive");
        }
        if self.poll_interval < MIN_POLL_INTERVAL {
            return Err("poll_interval must be at least 1ms");
        }
        let batch_bytes =
            (self.batch_limit as u64).saturating_mul(SEMANTIC_STREAM_ENVELOPE_BYTES as u64);
        if self.channel_capacity_bytes < batch_bytes {
            return Err("channel_capacity_bytes must hold one full batch of envelopes");
        }
        if self.topic_retained_bytes < batch_bytes {
            return Err("topic_retained_bytes must hold one full batch of envelopes");
        }
        Ok(())
    }
}

/// The well-known durable binding one bootstrap established: the system
/// channel, the system topic, and the pump's system subscription (whose
/// `consume_token` the cursor advance requires).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticStreamBinding {
    pub channel: ChannelRecord,
    pub topic: TopicRecord,
    pub subscription: SubscriptionRecord,
}

impl SemanticStreamBinding {
    /// The system topic identity every notification is published under.
    #[must_use]
    pub const fn topic_id(&self) -> nlos_topic::TopicId {
        self.topic.topic_id
    }

    /// The system channel identity backing the topic.
    #[must_use]
    pub const fn channel_id(&self) -> ChannelId {
        self.topic.channel_id
    }

    /// The system subscriber key whose cursor the pump follows.
    #[must_use]
    pub const fn subscriber_key(&self) -> SubscriberKey {
        self.subscription.subscriber_key
    }
}

/// Encodes one pending semantic outbox row as the fixed-width notification
/// envelope (exactly [`SEMANTIC_STREAM_ENVELOPE_BYTES`] bytes):
///
/// ```text
/// offset  width  field
/// 0       1      event type — the semantic authority's code:
///                 1 assertion, 2 judgment, 3 verification,
///                 4 retraction, 5 spec
/// 1       8      log_seq — unsigned big-endian
/// 9       32     event id — the 32-byte SemanticEventId
/// 41      32     payload-identity digest — the event's content digest
///                 (assertions) or spec-body digest (spec events); the
///                 all-zero sentinel when the event is structural and
///                 carries no payload identity
/// ```
///
/// This is a notification, not the event bytes: subscribers re-read the
/// full canonical event, receipt, and trust view through the
/// [`SemanticAuthority`] inspect paths after being notified.
#[must_use]
pub fn semantic_stream_envelope(row: &SemanticOutboxRow) -> Vec<u8> {
    let mut envelope = Vec::with_capacity(SEMANTIC_STREAM_ENVELOPE_BYTES);
    envelope.push(row.event_type);
    envelope.extend_from_slice(&row.log_seq.to_be_bytes());
    envelope.extend_from_slice(row.event_id.as_bytes());
    match row.content_digest {
        Some(digest) => envelope.extend_from_slice(&digest),
        None => envelope.extend_from_slice(&[0_u8; 32]),
    }
    envelope
}

/// The publish idempotency key of one pending row: derived
/// domain-separated from the event id alone, so a crash between publish
/// and outbox acknowledgement re-delivers the *same* key and the topic
/// authority replays the original publication — zero double-send.
#[must_use]
pub fn semantic_stream_publish_key(event_id: SemanticEventId) -> IdempotencyKey {
    IdempotencyKey::from_bytes(hash16(
        SEMANTIC_STREAM_DOMAIN,
        &[b"publish", event_id.as_bytes()],
    ))
}

/// The well-known system subscriber key.
#[must_use]
pub fn system_subscriber_key() -> SubscriberKey {
    SubscriberKey::from_bytes(hash16(SEMANTIC_STREAM_DOMAIN, &[b"system-subscriber"]))
}

/// The idempotent bootstrap of the system channel, system topic, and system
/// subscription (see the module doc). Every request — including the three
/// bootstrap clock readings, taken under fixed-purpose keys — is derived
/// deterministically from [`SEMANTIC_STREAM_DOMAIN`] and the config's
/// durable-identity fields, so an exact re-run replays every authority
/// decision instead of conflicting with itself.
///
/// # Errors
///
/// Fails typed through [`SliceKError`] on channel, topic, or clock
/// refusals (including the frozen-identity conflict a changed
/// durable-identity field raises against an existing root).
pub(crate) fn bootstrap_semantic_stream(
    channel: &ChannelAuthority,
    topics: &TopicAuthority,
    clock: &AuthorityClock,
    config: &SemanticStreamConfig,
) -> SliceKResult<SemanticStreamBinding> {
    let channel_record = channel
        .create_channel(CreateChannelRequest {
            capacity_bytes: config.channel_capacity_bytes,
            policy_digest: hash32(
                SEMANTIC_STREAM_DOMAIN,
                &[
                    b"channel-policy",
                    &config.channel_capacity_bytes.to_be_bytes(),
                ],
            ),
            idempotency_key: IdempotencyKey::from_bytes(hash16(
                SEMANTIC_STREAM_DOMAIN,
                &[b"channel", &config.channel_capacity_bytes.to_be_bytes()],
            )),
            created_at_ms: wall(clock, b"clock-channel")?,
        })?
        .record();
    let policy = TopicPolicy {
        max_recipients: config.topic_max_recipients,
        delivery_attempts: config.topic_delivery_attempts,
        cascade_depth: config.topic_cascade_depth,
        retained_bytes: config.topic_retained_bytes,
        retention_ms: config.topic_retention_ms,
        payer: ResourceAccountId::from_bytes(hash16(SEMANTIC_STREAM_DOMAIN, &[b"payer"])),
    };
    let topic_record = topics
        .create_topic(CreateTopicRequest {
            channel_id: channel_record.channel_id,
            name: SEMANTIC_STREAM_TOPIC_NAME.to_vec(),
            policy,
            idempotency_key: IdempotencyKey::from_bytes(hash16(
                SEMANTIC_STREAM_DOMAIN,
                &[b"topic", &topic_policy_fingerprint(&policy)],
            )),
            created_at_ms: wall(clock, b"clock-topic")?,
        })?
        .record();
    // The system subscription's cursor starts at the channel's subscribe-time
    // high-water (0 on the fresh bootstrap channel): history before the
    // subscribe point is never replayed, which is exactly right for a
    // notification stream — the pump follows only what it published itself.
    let subscription = topics
        .subscribe(SubscribeRequest {
            topic_id: topic_record.topic_id,
            subscriber_key: system_subscriber_key(),
            subscribed_at_ms: wall(clock, b"clock-subscribe")?,
        })?
        .record();
    Ok(SemanticStreamBinding {
        channel: channel_record,
        topic: topic_record,
        subscription,
    })
}

/// The durable authorities and well-known binding one pump thread drives.
pub(crate) struct SemanticStreamDeps {
    pub(crate) semantic: Arc<SemanticAuthority>,
    pub(crate) topics: Arc<TopicAuthority>,
    pub(crate) clock: Arc<AuthorityClock>,
    pub(crate) binding: SemanticStreamBinding,
}

/// Lifecycle state of the semantic stream pump thread.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SemanticStreamState {
    /// Cycling normally.
    Running,
    /// The thread exited after too many consecutive cycle failures; the
    /// pending prefix stays durable, so a fresh pump redelivers everything.
    Faulted,
    /// The thread exited cleanly (`stop` or runtime drop).
    Stopped,
}

/// Observable health/counters surface of the semantic stream pump.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticStreamHealth {
    pub state: SemanticStreamState,
    /// Successful cycles completed.
    pub cycles: u64,
    /// Envelopes this pump freshly enqueued.
    pub published: u64,
    /// Publishes that replayed an existing publication (crash window or
    /// at-least-once redelivery — zero double-send evidence).
    pub publish_replays: u64,
    /// Outbox rows this pump acknowledged.
    pub acknowledged: u64,
    /// Cycles that advanced the system cursor.
    pub advanced_cycles: u64,
    /// Cycles that compacted (released channel capacity).
    pub compacted_cycles: u64,
    /// Consecutive failed cycles since the last success.
    pub consecutive_failures: usize,
    /// `Display` text of the most recent cycle failure.
    pub last_error: Option<String>,
}

const STATE_RUNNING: usize = 0;
const STATE_FAULTED: usize = 1;
const STATE_STOPPED: usize = 2;

/// Interior-shared health and counters written by the pump thread.
struct StreamHealthInner {
    state: AtomicUsize,
    consecutive_failures: AtomicUsize,
    last_error: Mutex<Option<String>>,
    cycles: AtomicU64,
    published: AtomicU64,
    publish_replays: AtomicU64,
    acknowledged: AtomicU64,
    advanced_cycles: AtomicU64,
    compacted_cycles: AtomicU64,
}

impl StreamHealthInner {
    fn new() -> Self {
        Self {
            state: AtomicUsize::new(STATE_RUNNING),
            consecutive_failures: AtomicUsize::new(0),
            last_error: Mutex::new(None),
            cycles: AtomicU64::new(0),
            published: AtomicU64::new(0),
            publish_replays: AtomicU64::new(0),
            acknowledged: AtomicU64::new(0),
            advanced_cycles: AtomicU64::new(0),
            compacted_cycles: AtomicU64::new(0),
        }
    }

    fn record_success(&self) {
        self.consecutive_failures.store(0, Ordering::Release);
        *self.lock_detail() = None;
    }

    fn record_failure(&self, error: String) -> usize {
        *self.lock_detail() = Some(error);
        self.consecutive_failures.fetch_add(1, Ordering::AcqRel) + 1
    }

    fn set_state(&self, state: SemanticStreamState) {
        let value = match state {
            SemanticStreamState::Running => STATE_RUNNING,
            SemanticStreamState::Faulted => STATE_FAULTED,
            SemanticStreamState::Stopped => STATE_STOPPED,
        };
        self.state.store(value, Ordering::Release);
    }

    fn snapshot(&self) -> SemanticStreamHealth {
        let state = match self.state.load(Ordering::Acquire) {
            STATE_FAULTED => SemanticStreamState::Faulted,
            STATE_STOPPED => SemanticStreamState::Stopped,
            _ => SemanticStreamState::Running,
        };
        SemanticStreamHealth {
            state,
            cycles: self.cycles.load(Ordering::Acquire),
            published: self.published.load(Ordering::Acquire),
            publish_replays: self.publish_replays.load(Ordering::Acquire),
            acknowledged: self.acknowledged.load(Ordering::Acquire),
            advanced_cycles: self.advanced_cycles.load(Ordering::Acquire),
            compacted_cycles: self.compacted_cycles.load(Ordering::Acquire),
            consecutive_failures: self.consecutive_failures.load(Ordering::Acquire),
            last_error: self.lock_detail().clone(),
        }
    }

    fn lock_detail(&self) -> MutexGuard<'_, Option<String>> {
        self.last_error
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Failure to start a [`SemanticStreamPump`]: no thread exists and no
/// durable state changed in either case, so a caller may retry after
/// resolving the cause.
#[derive(Debug)]
pub enum SemanticStreamPumpStartError {
    /// The configuration is unusable (see [`SemanticStreamConfig::validate`]).
    InvalidConfig(&'static str),
    /// The OS refused the dedicated pump thread.
    Spawn(std::io::Error),
}

impl fmt::Display for SemanticStreamPumpStartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(reason) => {
                write!(formatter, "invalid semantic stream pump config: {reason}")
            }
            Self::Spawn(error) => {
                write!(
                    formatter,
                    "could not spawn semantic stream pump thread: {error}"
                )
            }
        }
    }
}

impl std::error::Error for SemanticStreamPumpStartError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidConfig(_) => None,
            Self::Spawn(error) => Some(error),
        }
    }
}

/// The dedicated semantic-stream pump thread: lists the pending semantic
/// outbox prefix, publishes one 73-byte envelope per row under the
/// event-derived idempotency key, acknowledges the outbox, then follows the
/// system subscriber cursor and compacts so the bounded channel capacity is
/// released. See the module doc for the full cycle and failure contract
/// (bounded backoff, fault threshold, idempotent stop).
pub struct SemanticStreamPump {
    stop: Arc<AtomicBool>,
    hint: SyncSender<()>,
    health: Arc<StreamHealthInner>,
    worker: Option<JoinHandle<()>>,
}

impl SemanticStreamPump {
    /// Spawns the pump thread over `deps` (the runtime's shared authorities
    /// plus the bootstrapped well-known binding).
    ///
    /// # Errors
    ///
    /// Returns [`SemanticStreamPumpStartError::InvalidConfig`] for an
    /// unusable [`SemanticStreamConfig`] (no thread spawned) or
    /// [`SemanticStreamPumpStartError::Spawn`] when the OS refuses the
    /// thread.
    pub(crate) fn start(
        deps: SemanticStreamDeps,
        config: SemanticStreamConfig,
    ) -> Result<Self, SemanticStreamPumpStartError> {
        config
            .validate()
            .map_err(SemanticStreamPumpStartError::InvalidConfig)?;
        let stop = Arc::new(AtomicBool::new(false));
        let health = Arc::new(StreamHealthInner::new());
        // Capacity 1: one pending hint is enough to schedule a cycle, and a
        // full channel makes `hint` drop instead of blocking the writer.
        let (hint, hints) = sync_channel::<()>(1);
        let worker = {
            let stop = Arc::clone(&stop);
            let health = Arc::clone(&health);
            std::thread::Builder::new()
                .name("nlos-slice-k-semantic-stream".to_owned())
                .spawn(move || stream_loop(&deps, &hints, &stop, &health, &config))
                .map_err(SemanticStreamPumpStartError::Spawn)?
        };
        Ok(Self {
            stop,
            hint,
            health,
            worker: Some(worker),
        })
    }

    /// Bounded, non-blocking wake-up hint; `false` when no pump runs or a
    /// hint is already pending. The fallback poll bounds delivery either
    /// way, so callers may ignore the result.
    #[must_use]
    pub fn hint(&self) -> bool {
        self.hint.try_send(()).is_ok()
    }

    /// Current health snapshot (`None` semantics belong to the runtime's
    /// pump slot, not the pump itself).
    #[must_use]
    pub fn health(&self) -> SemanticStreamHealth {
        self.health.snapshot()
    }

    /// Signals the pump thread to stop and joins it. Idempotent by move
    /// semantics (the second call happens on a different value or none).
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        // Wake the thread out of `recv_timeout`; the bounded channel may be
        // full, which is fine because the flag is the real stop signal.
        let _ = self.hint.try_send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Drop for SemanticStreamPump {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// What one cycle observed, for the loop's continue-or-wait decision.
struct CycleOutcome {
    /// A full batch likely leaves more pending rows: drain again without
    /// waiting.
    full_batch: bool,
}

/// One pump cycle (see the module doc). Every timestamp the cycle writes —
/// publish, acknowledgement, cursor advance — is the single **read-only**
/// wall high-water reading taken once at cycle start: the reading is
/// monotonic across restarts and clock rollback, so it is by construction
/// at or above every `admitted_at_ms` (admissions took their readings from
/// the same monotonic water) and at or above any previous per-row
/// acknowledgement, satisfying `acknowledge_outbox`'s not-before-admission
/// and per-row monotonic checks without any clock write on this path.
fn run_cycle(
    deps: &SemanticStreamDeps,
    config: &SemanticStreamConfig,
    health: &StreamHealthInner,
) -> Result<CycleOutcome, String> {
    let rows = deps
        .semantic
        .list_pending_outbox(config.batch_limit)
        .map_err(|error| format!("pending outbox read failed: {error}"))?;
    let pending_seen = rows.len();
    let now_ms = deps
        .clock
        .inspect_wall()
        .map_err(|error| format!("clock wall high-water read failed: {error}"))?
        .as_u64();
    // Idempotent subscription refresh: an already-active system subscription
    // replays without a write and re-issues the durable consume token the
    // cursor advance below requires, so the cycle never caches a stale
    // token. (The refresh re-subscribes at the channel high-water only when
    // someone externally unsubscribed the system subscriber — history
    // before that point is never replayed, exactly the topic authority's
    // documented subscribe semantics.)
    let subscription = deps
        .topics
        .subscribe(SubscribeRequest {
            topic_id: deps.binding.topic.topic_id,
            subscriber_key: system_subscriber_key(),
            subscribed_at_ms: now_ms,
        })
        .map_err(|error| format!("system subscription refresh failed: {error}"))?
        .record();

    for row in rows {
        let publish = deps
            .topics
            .publish(PublishRequest {
                topic_id: deps.binding.topic.topic_id,
                idempotency_key: semantic_stream_publish_key(row.event_id),
                published_at_ms: now_ms,
                payload: semantic_stream_envelope(&row),
            })
            .map_err(|error| format!("envelope publish failed: {error}"))?;
        match publish {
            PublishDecision::Published(_) => {
                health.published.fetch_add(1, Ordering::AcqRel);
            }
            PublishDecision::Replayed(_) => {
                health.publish_replays.fetch_add(1, Ordering::AcqRel);
            }
        }
        deps.semantic
            .acknowledge_outbox(AcknowledgeOutboxRequest {
                event_id: row.event_id,
                log_seq: row.log_seq,
                receipt_id: row.receipt_id,
                acknowledged_at_ms: now_ms,
            })
            .map_err(|error| {
                format!(
                    "outbox acknowledgement failed for log_seq {}: {error}",
                    row.log_seq
                )
            })?;
        health.acknowledged.fetch_add(1, Ordering::AcqRel);
    }

    // System-subscriber cursor follow: zero-write poll, token-authenticated
    // advance, then compact — the W45-C1 linkage that advances the channel
    // consume high-water to the minimum active cursor before trimming, so
    // the bounded channel capacity the publishes above consumed is
    // released. A cycle that consumed nothing writes nothing here.
    let entries = deps
        .topics
        .poll(
            deps.binding.topic.topic_id,
            system_subscriber_key(),
            config.batch_limit,
        )
        .map_err(|error| format!("system subscriber poll failed: {error}"))?;
    if let Some(last) = entries.last() {
        deps.topics
            .advance_with_token(
                AdvanceRequest {
                    topic_id: deps.binding.topic.topic_id,
                    subscriber_key: system_subscriber_key(),
                    up_to_sequence: last.sequence,
                    advanced_at_ms: now_ms,
                },
                &subscription.consume_token,
            )
            .map_err(|error| format!("system cursor advance failed: {error}"))?;
        health.advanced_cycles.fetch_add(1, Ordering::AcqRel);
        deps.topics
            .compact(deps.binding.topic.topic_id, last.sequence)
            .map_err(|error| format!("topic compact failed: {error}"))?;
        health.compacted_cycles.fetch_add(1, Ordering::AcqRel);
    }
    Ok(CycleOutcome {
        full_batch: pending_seen == config.batch_limit,
    })
}

/// One guarded cycle: an authority failure surfaces as error text and a
/// panic is caught (no `unsafe`) and reported the same way, so the pump
/// thread can never die silently.
fn cycle_guarded(
    deps: &SemanticStreamDeps,
    config: &SemanticStreamConfig,
    health: &StreamHealthInner,
) -> Result<CycleOutcome, String> {
    match catch_unwind(AssertUnwindSafe(|| run_cycle(deps, config, health))) {
        Ok(Ok(outcome)) => Ok(outcome),
        Ok(Err(error)) => Err(error),
        Err(_) => Err("semantic stream cycle panicked".to_owned()),
    }
}

/// Bounded exponential backoff after the `failures`-th consecutive failure:
/// `poll_interval * 2^(failures-1)`, capped at 64 × `poll_interval`.
fn backoff(poll_interval: Duration, failures: usize) -> Duration {
    let shift = u32::try_from(failures.saturating_sub(1))
        .unwrap_or(u32::MAX)
        .min(BACKOFF_CAP_MULTIPLE.trailing_zeros());
    poll_interval.saturating_mul(1_u32 << shift)
}

/// The pump thread body (the `nlos-runtime-tokio` pump loop discipline):
/// drain full batches back-to-back, otherwise wait for a hint or the
/// fallback interval; a failed cycle backs off boundedly, is observable via
/// health, and too many consecutive failures fault the pump. The pending
/// prefix is never acknowledged away by failure — rows stay durable for a
/// future pump.
fn stream_loop(
    deps: &SemanticStreamDeps,
    hints: &Receiver<()>,
    stop: &AtomicBool,
    health: &StreamHealthInner,
    config: &SemanticStreamConfig,
) {
    'outer: while !stop.load(Ordering::Acquire) {
        loop {
            if stop.load(Ordering::Acquire) {
                break 'outer;
            }
            match cycle_guarded(deps, config, health) {
                Ok(outcome) => {
                    health.record_success();
                    health.cycles.fetch_add(1, Ordering::AcqRel);
                    if outcome.full_batch {
                        continue;
                    }
                    break;
                }
                Err(error) => {
                    let failures = health.record_failure(error);
                    if failures >= config.failure_threshold {
                        health.set_state(SemanticStreamState::Faulted);
                        return;
                    }
                    // The wait doubles per consecutive failure; a hint or
                    // the stop signal still wakes the thread out of it.
                    let _ = hints.recv_timeout(backoff(config.poll_interval, failures));
                    continue 'outer;
                }
            }
        }
        let _ = hints.recv_timeout(config.poll_interval);
    }
    health.set_state(SemanticStreamState::Stopped);
}

/// Stops a semantic stream pump with a bounded join (the same contract as
/// the outbox-pump bound: the stop flag and hint are delivered first, the
/// joiner waits at most [`STOP_JOIN_DEADLINE`], and a detached helper
/// finishes the join if the deadline expires).
pub(crate) fn stop_semantic_stream_bounded(pump: SemanticStreamPump) -> bool {
    let (done, done_rx) = std::sync::mpsc::channel::<()>();
    let helper = std::thread::Builder::new()
        .name("nlos-slice-k-semantic-stream-stop".to_owned())
        .spawn(move || {
            pump.stop();
            let _ = done.send(());
        });
    match helper {
        Ok(_) => done_rx.recv_timeout(STOP_JOIN_DEADLINE).is_ok(),
        // Thread spawn failed: dropping the pump delivers the same
        // stop-and-join inline, so the shutdown signal still reaches the
        // pump thread — only the bound is lost this one time.
        Err(_) => true,
    }
}

/// The fixed-purpose bootstrap clock reading of one tag.
fn wall(clock: &AuthorityClock, tag: &[u8]) -> SliceKResult<u64> {
    Ok(clock
        .wall_now(NowRequest {
            idempotency_key: IdempotencyKey::from_bytes(hash16(SEMANTIC_STREAM_DOMAIN, &[tag])),
        })?
        .reading()
        .as_u64())
}

/// The durable-identity fingerprint of the topic policy (the five declared
/// RSM-FANOUT-001 fields, big-endian, fixed order).
fn topic_policy_fingerprint(policy: &TopicPolicy) -> [u8; 40] {
    let mut fingerprint = [0_u8; 40];
    fingerprint[..8].copy_from_slice(&policy.max_recipients.to_be_bytes());
    fingerprint[8..16].copy_from_slice(&policy.delivery_attempts.to_be_bytes());
    fingerprint[16..24].copy_from_slice(&policy.cascade_depth.to_be_bytes());
    fingerprint[24..32].copy_from_slice(&policy.retained_bytes.to_be_bytes());
    fingerprint[32..].copy_from_slice(&policy.retention_ms.to_be_bytes());
    fingerprint
}

fn hash32(domain: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    for part in parts {
        hasher.update(part);
    }
    hasher.finalize().into()
}

fn hash16(domain: &[u8], parts: &[&[u8]]) -> [u8; 16] {
    let digest = hash32(domain, parts);
    let mut id = [0_u8; 16];
    id.copy_from_slice(&digest[..16]);
    id
}
