//! Production wiring faces for [`WaitAuthority`](nlos_wait::WaitAuthority),
//! [`NotificationService`](nlos_notify::NotificationService), and
//! [`SearchService`](nlos_search::SearchService) inside [`SliceKRuntime`] and
//! [`CellHost`] (`W66`, unfreezing `nlos-notify` and `nlos-search`).
//!
//! Preserves strict authority boundaries:
//! - Notification subscriptions, `[TASK-NOTIFY-001]` task transitions,
//!   `[DUI-NOTIFY-001]` bound one-shot `ActionToken`s, and `[DUI-OFFLINE-001]`
//!   offline history pagination delegate through [`NotificationService`] over
//!   the shared [`TopicAuthority`](nlos_topic::TopicAuthority),
//!   [`ChannelAuthority`](nlos_channel::ChannelAuthority), and
//!   [`WaitAuthority`](nlos_wait::WaitAuthority) (single-log fanout, zero
//!   shadow queue);
//! - Search discovery, `[SEM-CHECKPOINT-001]` checkpoint-bounded snapshot
//!   queries, `[TASK-SNAPSHOT-003]` candidate re-verification, and multi-Cell
//!   federated candidate aggregation delegate through [`SearchService`] over
//!   the read-only `SQLite` connection and live
//!   [`SemanticAuthority`](nlos_semantic::SemanticAuthority), preserving
//!   [`SearchUseRestrictions::NON_AUTHORITATIVE`] (`[CONS-DECL-001]`,
//!   `[CONS-CTRL-001]`, `[DIST-FAIL-001]`).

use nlos_cell::{RemoteCallOutcome, RemoteRetryDirective};
use nlos_channel::{ChannelRecord, CreateChannelRequest};
use nlos_clock::NowRequest;
use nlos_notify::{
    NotificationSubscribeDecision, NotificationSubscription, SubscribeNotificationRequest,
};
use nlos_search::{
    AssertionSelector, CellSearchShardOutcome, CheckpointVerifier, FederatedSearchReport,
    NonAuthoritativeCandidateHit, PaginatedAssertionSelector, RemoteSearchFailureReason,
    SearchAchievedConsistency, SearchPage, SearchUseRestrictions, VerifiedCheckpointCandidate,
    aggregate_federated_candidates,
};
use nlos_semantic::SignedSemanticVectorCheckpoint;
use nlos_topic::{CreateTopicRequest, SubscriberKey, TopicId, TopicPolicy, TopicRecord};
use nlos_types::{ApplicationId, ChannelId, IdempotencyKey, ResourceAccountId};
use sha2::{Digest, Sha256};

use crate::cell_host::CellHost;
use crate::error::SliceKResult;
use crate::runtime::SliceKRuntime;

/// Domain separator for deterministic notification-topic bootstrap keys.
pub const TASK_NOTIFICATION_TOPIC_DOMAIN: &[u8] = b"llmos/slice-k/task-notify/v1";

/// Default bounded channel capacity for an application/task notification topic (4 MiB).
const DEFAULT_NOTIFY_CHANNEL_CAPACITY_BYTES: u64 = 4 * 1024 * 1024;

/// Default topic retained backlog bound (4 MiB).
const DEFAULT_NOTIFY_RETAINED_BYTES: u64 = 4 * 1024 * 1024;

/// Default topic retention age bound (24 hours).
const DEFAULT_NOTIFY_RETENTION_MS: u64 = 24 * 60 * 60 * 1_000;

/// Declarative configuration for bootstrapping an idempotent notification
/// channel + topic pair on [`SliceKRuntime`] or [`CellHost`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NotificationTopicConfig {
    /// Logical topic name recorded in [`TopicAuthority`](nlos_topic::TopicAuthority).
    pub topic_name: Vec<u8>,
    /// Resource account billed for topic retention/delivery policy.
    pub payer: ResourceAccountId,
    /// Underlying channel capacity in bytes.
    pub channel_capacity_bytes: u64,
    /// Maximum concurrent subscribers on the topic.
    pub max_recipients: u64,
    /// Maximum redelivery budget before subscriber quarantine.
    pub delivery_attempts: u64,
    /// Maximum topic cascade depth (`>= 1`).
    pub cascade_depth: u64,
    /// Maximum retained unconsumed bytes on the topic.
    pub retained_bytes: u64,
    /// Maximum retention age in milliseconds.
    pub retention_ms: u64,
}

impl NotificationTopicConfig {
    /// Builds a standard task-notification topic configuration scoped to one
    /// [`ApplicationId`].
    #[must_use]
    pub fn for_application(application_id: ApplicationId) -> Self {
        let mut topic_name = b"llmos/application/".to_vec();
        topic_name.extend_from_slice(crate::short_hex(application_id.as_bytes()).as_bytes());
        topic_name.extend_from_slice(b"/task-transitions/v1");
        let payer = ResourceAccountId::from_bytes(hash16(
            TASK_NOTIFICATION_TOPIC_DOMAIN,
            &[b"payer", application_id.as_bytes()],
        ));
        Self::named(topic_name, payer)
    }

    /// Builds a conservative notification topic configuration with an explicit
    /// `topic_name` and `payer`.
    #[must_use]
    pub fn named(topic_name: impl Into<Vec<u8>>, payer: ResourceAccountId) -> Self {
        Self {
            topic_name: topic_name.into(),
            payer,
            channel_capacity_bytes: DEFAULT_NOTIFY_CHANNEL_CAPACITY_BYTES,
            max_recipients: 16,
            delivery_attempts: 1_024,
            cascade_depth: 1,
            retained_bytes: DEFAULT_NOTIFY_RETAINED_BYTES,
            retention_ms: DEFAULT_NOTIFY_RETENTION_MS,
        }
    }
}

/// Durable [`ChannelRecord`] and [`TopicRecord`] materialized by
/// [`bootstrap_notification_topic`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NotificationTopicBinding {
    /// Underlying single-log queue channel record.
    pub channel: ChannelRecord,
    /// Topic fanout record bound to `channel`.
    pub topic: TopicRecord,
}

impl NotificationTopicBinding {
    /// Topic identifier.
    #[must_use]
    pub const fn topic_id(&self) -> TopicId {
        self.topic.topic_id
    }

    /// Underlying channel identifier.
    #[must_use]
    pub const fn channel_id(&self) -> ChannelId {
        self.topic.channel_id
    }
}

/// Combined outcome of idempotently bootstrapping a notification topic and
/// registering a [`NotificationService`](nlos_notify::NotificationService)
/// subscription on it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SubscribedNotificationTopic {
    /// Durable channel + topic binding.
    pub binding: NotificationTopicBinding,
    /// Managed [`NotificationSubscription`] reference.
    pub subscription: NotificationSubscription,
    /// `true` when the subscription replayed an already-active registration.
    pub replayed: bool,
}

impl SubscribedNotificationTopic {
    /// Topic identifier.
    #[must_use]
    pub const fn topic_id(&self) -> TopicId {
        self.binding.topic.topic_id
    }

    /// Underlying channel identifier.
    #[must_use]
    pub const fn channel_id(&self) -> ChannelId {
        self.binding.topic.channel_id
    }

    /// Managed [`nlos_notify::NotificationId`] handle.
    #[must_use]
    pub const fn notification_id(&self) -> nlos_notify::NotificationId {
        self.subscription.notification_id
    }
}

/// Idempotently creates (or replays) the backing channel and topic for
/// `config` on `runtime`.
///
/// # Errors
///
/// Fails closed with [`crate::SliceKError`] if the clock, channel, or topic
/// authority rejects the request.
pub fn bootstrap_notification_topic(
    runtime: &SliceKRuntime,
    config: &NotificationTopicConfig,
) -> SliceKResult<NotificationTopicBinding> {
    let policy = TopicPolicy {
        max_recipients: config.max_recipients,
        delivery_attempts: config.delivery_attempts,
        cascade_depth: config.cascade_depth,
        retained_bytes: config.retained_bytes,
        retention_ms: config.retention_ms,
        payer: config.payer,
    };
    let fingerprint = config_fingerprint(config, &policy);
    let channel_wall = runtime
        .clock
        .wall_now(NowRequest {
            idempotency_key: IdempotencyKey::from_bytes(hash16(
                TASK_NOTIFICATION_TOPIC_DOMAIN,
                &[b"clock-channel", &fingerprint],
            )),
        })?
        .reading()
        .as_u64();
    let channel = runtime
        .channel
        .create_channel(CreateChannelRequest {
            capacity_bytes: config.channel_capacity_bytes,
            policy_digest: hash32(
                TASK_NOTIFICATION_TOPIC_DOMAIN,
                &[b"channel-policy", &fingerprint],
            ),
            idempotency_key: IdempotencyKey::from_bytes(hash16(
                TASK_NOTIFICATION_TOPIC_DOMAIN,
                &[b"channel", &fingerprint],
            )),
            created_at_ms: channel_wall,
        })?
        .record();

    let topic_wall = runtime
        .clock
        .wall_now(NowRequest {
            idempotency_key: IdempotencyKey::from_bytes(hash16(
                TASK_NOTIFICATION_TOPIC_DOMAIN,
                &[b"clock-topic", &fingerprint],
            )),
        })?
        .reading()
        .as_u64();
    let topic = runtime
        .topics
        .create_topic(CreateTopicRequest {
            channel_id: channel.channel_id,
            name: config.topic_name.clone(),
            policy,
            idempotency_key: IdempotencyKey::from_bytes(hash16(
                TASK_NOTIFICATION_TOPIC_DOMAIN,
                &[b"topic", &fingerprint],
            )),
            created_at_ms: topic_wall,
        })?
        .record();

    Ok(NotificationTopicBinding { channel, topic })
}

/// Idempotently bootstraps `config` and subscribes `subscriber_key` through
/// [`SliceKRuntime::notify`].
///
/// # Errors
///
/// Propagates any channel, topic, clock, or notification-service error.
pub fn bootstrap_and_subscribe_notification_topic(
    runtime: &SliceKRuntime,
    config: &NotificationTopicConfig,
    subscriber_key: SubscriberKey,
    subscribed_at_ms: u64,
) -> SliceKResult<SubscribedNotificationTopic> {
    let binding = bootstrap_notification_topic(runtime, config)?;
    let decision = runtime.notify().subscribe(SubscribeNotificationRequest {
        topic_id: binding.topic.topic_id,
        subscriber_key,
        subscribed_at_ms,
    })?;
    let (subscription, replayed) = match decision {
        NotificationSubscribeDecision::Subscribed(sub) => (sub, false),
        NotificationSubscribeDecision::Replayed(sub) => (sub, true),
    };
    Ok(SubscribedNotificationTopic {
        binding,
        subscription,
        replayed,
    })
}

/// Queries a cursor-paginated page of non-authoritative candidates from
/// `runtime`'s [`SearchService`](nlos_search::SearchService), tagging every hit
/// with `runtime.semantic_control_domain_id()`,
/// [`SearchAchievedConsistency::EventualLiveScan`], and
/// [`SearchUseRestrictions::NON_AUTHORITATIVE`] (`[CONS-DECL-001]`,
/// `[CONS-CTRL-001]`).
///
/// # Errors
///
/// Fails closed with [`crate::SliceKError::Search`] on invalid limits, decode
/// corruption, or authority trust-view lookup errors.
pub fn query_runtime_non_authoritative_candidates(
    runtime: &SliceKRuntime,
    paginated: &PaginatedAssertionSelector,
) -> SliceKResult<SearchPage<NonAuthoritativeCandidateHit>> {
    let raw_page = runtime.search().search_assertions_paginated(paginated)?;
    let domain_id = runtime.semantic_control_domain_id();
    let items = raw_page
        .items
        .into_iter()
        .map(|hit| {
            NonAuthoritativeCandidateHit::new(
                hit,
                domain_id,
                SearchAchievedConsistency::EventualLiveScan,
            )
        })
        .collect();
    Ok(SearchPage {
        items,
        next_after_log_seq: raw_page.next_after_log_seq,
        exhausted: raw_page.exhausted,
        achieved_consistency: raw_page.achieved_consistency,
        use_restrictions: SearchUseRestrictions::NON_AUTHORITATIVE,
    })
}

/// Queries checkpoint-bounded non-authoritative candidates on `runtime` using
/// `verifying_key` (`[SEM-CHECKPOINT-001]`, `[TASK-SNAPSHOT-003]`,
/// `[CONS-DECL-001]`, `[CONS-CTRL-001]`).
///
/// # Errors
///
/// Fails closed if `checkpoint` fails verification, if the local watermark
/// is missing (when `require_domain_present == true`) or mismatched, or if
/// `selector.limit == 0`.
pub fn query_runtime_at_checkpoint_with_verifier<V: CheckpointVerifier>(
    runtime: &SliceKRuntime,
    checkpoint: &SignedSemanticVectorCheckpoint,
    verifying_key: &V,
    selector: &AssertionSelector,
    after_log_seq: Option<u64>,
    require_domain_present: bool,
) -> SliceKResult<SearchPage<NonAuthoritativeCandidateHit>> {
    Ok(runtime
        .search()
        .search_candidates_at_checkpoint_with_options(
            runtime.semantic_control_domain_id(),
            checkpoint,
            verifying_key,
            selector,
            after_log_seq,
            require_domain_present,
        )?)
}

/// Re-verifies a discovered [`NonAuthoritativeCandidateHit`] against
/// `checkpoint` and `runtime`'s live [`SemanticAuthority`](nlos_semantic::SemanticAuthority)
/// (`[TASK-SNAPSHOT-003]`, `[CONS-CTRL-001]`).
///
/// # Errors
///
/// Fails closed on tampered `use_restrictions`, invalid checkpoint signature,
/// watermark mismatch, authority record mismatch, durable retraction, or
/// post-checkpoint `log_seq`.
pub fn verify_runtime_candidate_against_checkpoint<V: CheckpointVerifier>(
    runtime: &SliceKRuntime,
    candidate: &NonAuthoritativeCandidateHit,
    checkpoint: &SignedSemanticVectorCheckpoint,
    verifying_key: &V,
) -> SliceKResult<VerifiedCheckpointCandidate> {
    Ok(runtime.search().verify_candidate_against_checkpoint(
        candidate,
        checkpoint,
        verifying_key,
    )?)
}

/// Exports a [`CellSearchShardOutcome::Available`] shard from a live
/// [`CellHost`] (`[SEM-CHECKPOINT-001]`, `[CONS-CTRL-001]`).
///
/// When `checkpoint` is `Some(ckpt)`, verifies `ckpt` against the Cell's
/// [`IdentityAuthority`](nlos_identity::IdentityAuthority) and bounds the shard
/// scan to `ckpt`'s watermark cut (recording `ckpt.checkpoint_digest`).
///
/// # Errors
///
/// Fails closed on invalid `selector` or checkpoint verification/watermark
/// failure.
pub fn export_cell_search_shard(
    host: &CellHost,
    selector: &AssertionSelector,
    checkpoint: Option<&SignedSemanticVectorCheckpoint>,
) -> SliceKResult<CellSearchShardOutcome> {
    let cell_domain_id = *host.fence().identity().as_bytes();
    if let Some(ckpt) = checkpoint {
        let page = host
            .runtime()
            .query_at_signed_checkpoint_with_options(ckpt, selector, None, false)?;
        let hits = page.items.into_iter().map(|cand| cand.hit).collect();
        Ok(CellSearchShardOutcome::Available {
            cell_domain_id,
            hits,
            checkpoint_digest: Some(ckpt.checkpoint_digest),
        })
    } else {
        let hits = host.runtime().search().search_assertions(selector)?;
        Ok(CellSearchShardOutcome::Available {
            cell_domain_id,
            hits,
            checkpoint_digest: None,
        })
    }
}

/// Maps a `[DIST-FAIL-001]` [`RemoteCallOutcome<CellSearchShardOutcome>`] into
/// a [`CellSearchShardOutcome`] suitable for
/// [`aggregate_federated_cell_candidates`].
#[must_use]
pub fn remote_call_outcome_to_search_shard(
    cell_domain_id: [u8; 16],
    outcome: RemoteCallOutcome<CellSearchShardOutcome>,
) -> CellSearchShardOutcome {
    match outcome {
        RemoteCallOutcome::Succeeded { value, .. } => value,
        RemoteCallOutcome::Unavailable {
            retry_directive, ..
        } => CellSearchShardOutcome::Failed {
            cell_domain_id,
            reason: RemoteSearchFailureReason::Unavailable,
            retryable: is_retry_directive_retryable(retry_directive),
        },
        RemoteCallOutcome::DeadlineExceededUncertain {
            deadline_tick,
            retry_directive,
            ..
        } => CellSearchShardOutcome::Failed {
            cell_domain_id,
            reason: RemoteSearchFailureReason::DeadlineExceeded {
                budget_ms: deadline_tick,
            },
            retryable: is_retry_directive_retryable(retry_directive),
        },
        RemoteCallOutcome::Partial {
            retry_directive, ..
        } => CellSearchShardOutcome::Failed {
            cell_domain_id,
            reason: RemoteSearchFailureReason::Uncertain,
            retryable: is_retry_directive_retryable(retry_directive),
        },
    }
}

/// Aggregates [`CellSearchShardOutcome`]s across multiple Cells while
/// preserving `[DIST-FAIL-001]` failure semantics and
/// [`SearchUseRestrictions::NON_AUTHORITATIVE`] (`[CONS-DECL-001]`,
/// `[CONS-CTRL-001]`, `[SEM-CHECKPOINT-001]`).
///
/// # Errors
///
/// Fails closed with [`crate::SliceKError::Search`] when `max_results == 0`,
/// `shards` is empty, duplicate `cell_domain_id`s are supplied, or a remote
/// shard failed while `allow_partial_on_remote_failure == false`.
pub fn aggregate_federated_cell_candidates(
    shards: &[CellSearchShardOutcome],
    max_results: usize,
    allow_partial_on_remote_failure: bool,
) -> SliceKResult<FederatedSearchReport> {
    Ok(aggregate_federated_candidates(
        shards,
        max_results,
        allow_partial_on_remote_failure,
    )?)
}

/// Queries two live [`CellHost`] instances and aggregates their non-authoritative
/// search shards via [`aggregate_federated_candidates`].
///
/// # Errors
///
/// Propagates any shard query error or federated aggregation refusal.
pub fn federated_search_across_cell_hosts(
    first_host: &CellHost,
    second_host: &CellHost,
    selector: &AssertionSelector,
    max_results: usize,
    allow_partial_on_remote_failure: bool,
) -> SliceKResult<FederatedSearchReport> {
    let first_shard = export_cell_search_shard(first_host, selector, None)?;
    let second_shard = export_cell_search_shard(second_host, selector, None)?;
    aggregate_federated_cell_candidates(
        &[first_shard, second_shard],
        max_results,
        allow_partial_on_remote_failure,
    )
}

const fn is_retry_directive_retryable(directive: RemoteRetryDirective) -> bool {
    matches!(
        directive,
        RemoteRetryDirective::SafeToRetryWithSameKey | RemoteRetryDirective::RetryFailedShardsOnly
    )
}

fn config_fingerprint(config: &NotificationTopicConfig, policy: &TopicPolicy) -> [u8; 32] {
    hash32(
        TASK_NOTIFICATION_TOPIC_DOMAIN,
        &[
            &config.topic_name,
            &config.channel_capacity_bytes.to_be_bytes(),
            &policy.max_recipients.to_be_bytes(),
            &policy.delivery_attempts.to_be_bytes(),
            &policy.cascade_depth.to_be_bytes(),
            &policy.retained_bytes.to_be_bytes(),
            &policy.retention_ms.to_be_bytes(),
            policy.payer.as_bytes(),
        ],
    )
}

fn hash32(domain: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update((domain.len() as u64).to_be_bytes());
    hasher.update(domain);
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    hasher.finalize().into()
}

fn hash16(domain: &[u8], parts: &[&[u8]]) -> [u8; 16] {
    let digest = hash32(domain, parts);
    let mut out = [0_u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}
