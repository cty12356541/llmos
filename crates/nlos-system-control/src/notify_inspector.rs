//! Real [`NotificationInspector`] adapter backed by
//! [`nlos_notify::NotificationService`] and [`nlos_topic::TopicAuthority`]
//! (`C-UNFREEZE-001`, `[TASK-NOTIFY-001]`, `[DUI-OFFLINE-001]`,
//! `[PERF-CONTROL-001]`).
//!
//! Resolves one [`NotificationId`] through the face's O(1) primary-key lookup
//! ([`NotificationService::inspect_subscription`]), verifies the backing topic
//! head on [`TopicAuthority`], and reads one bounded offline history page
//! ([`INSPECT_NOTIFICATION_HISTORY_LIMIT`]) via
//! [`NotificationService::query_notification_history_page`].

use nlos_notify::{NotificationId, NotificationService, NotificationSubscriptionView, NotifyError};
use nlos_schema::sabi::v1::{RetryDirective, SabiErrorCode, SabiFailure};
use nlos_topic::{TopicAuthority, TopicAuthorityError};

use crate::control::{NotificationInspection, NotificationInspector};

/// Bounded per-inspection offline history page limit (`[PERF-CONTROL-001]`).
pub const INSPECT_NOTIFICATION_HISTORY_LIMIT: usize = 64;

/// Read-only [`NotificationInspector`] over a live [`NotificationService`] and
/// its backing [`TopicAuthority`].
pub struct NotificationServiceInspector<'a> {
    notify: &'a NotificationService,
    topics: &'a TopicAuthority,
}

impl<'a> NotificationServiceInspector<'a> {
    /// Binds one inspector to `notify` and its authoritative `topics`.
    #[must_use]
    pub const fn new(notify: &'a NotificationService, topics: &'a TopicAuthority) -> Self {
        Self { notify, topics }
    }
}

impl NotificationInspector for NotificationServiceInspector<'_> {
    fn inspect_notification(
        &self,
        notification_id: [u8; 16],
    ) -> Result<NotificationInspection, SabiFailure> {
        let id = NotificationId::from_bytes(notification_id);
        let view = self
            .notify
            .inspect_subscription(id)
            .map_err(|error| map_notify_error(&error))?;

        let (subscription, authority) = match view {
            NotificationSubscriptionView::Live {
                subscription,
                authority,
            } => (subscription, authority),
            NotificationSubscriptionView::Dangling { .. } => {
                return Err(not_found_failure(
                    "notification subscription references an absent topic subscription",
                ));
            }
        };

        self.topics
            .inspect_topic(subscription.topic_id)
            .map_err(|error| map_topic_error(&error))?;

        let page = self
            .notify
            .query_notification_history_page(
                subscription.topic_id,
                0,
                INSPECT_NOTIFICATION_HISTORY_LIMIT,
            )
            .map_err(|error| map_notify_error(&error))?;

        Ok(NotificationInspection {
            notification_id: *subscription.notification_id.as_bytes(),
            topic_id: *subscription.topic_id.as_bytes(),
            subscriber_key: *subscription.subscriber_key.as_bytes(),
            next_sequence: authority.cursor.saturating_add(1),
            history_entry_count: u64::try_from(page.entries.len()).unwrap_or(u64::MAX),
            history_truncated: page.has_more,
            prune_watermark_sequence: page.retained_from_sequence,
        })
    }
}

fn not_found_failure(message: &'static str) -> SabiFailure {
    SabiFailure {
        code: SabiErrorCode::NotFound.into(),
        retry: RetryDirective::DoNotRetry.into(),
        safe_message: message.to_owned(),
    }
}

fn map_topic_error(error: &TopicAuthorityError) -> SabiFailure {
    match error {
        TopicAuthorityError::TopicNotFound(_) | TopicAuthorityError::SubscriptionNotFound(_) => {
            not_found_failure("requested notification topic was not found")
        }
        _ => SabiFailure {
            code: SabiErrorCode::Driver.into(),
            retry: RetryDirective::DoNotRetry.into(),
            safe_message: "topic authority failed to inspect notification backing state".to_owned(),
        },
    }
}

fn map_notify_error(error: &NotifyError) -> SabiFailure {
    match error {
        NotifyError::NotificationNotFound(_) => {
            not_found_failure("requested notification subscription was not found")
        }
        NotifyError::Topic(inner) => map_topic_error(inner),
        NotifyError::InvalidHistoryPagination(_) => SabiFailure {
            code: SabiErrorCode::InvalidArgument.into(),
            retry: RetryDirective::DoNotRetry.into(),
            safe_message: "notification history pagination bound was invalid".to_owned(),
        },
        _ => SabiFailure {
            code: SabiErrorCode::Driver.into(),
            retry: RetryDirective::DoNotRetry.into(),
            safe_message: "notification service failed to inspect subscription state".to_owned(),
        },
    }
}
