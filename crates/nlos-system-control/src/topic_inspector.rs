//! Optional [`TopicInspectSource`] adapter backed by the durable
//! [`nlos_topic::TopicAuthority`] rows (W32-G, B5-3).
//!
//! Enabled with the crate's `topic` feature; the default control prefix
//! uses [`crate::UnwiredTopicInspectSource`] until a host wires this
//! adapter.

use nlos_schema::sabi::v1::{RetryDirective, SabiErrorCode, SabiFailure};
use nlos_topic::{TopicAuthority, TopicAuthorityError, TopicId};

use crate::TopicInspectSource;
use crate::control::TopicInspection;

/// Reads bounded durable topic facts through the topic authority.
pub struct TopicAuthoritySource<'a> {
    authority: &'a TopicAuthority,
}

impl<'a> TopicAuthoritySource<'a> {
    #[must_use]
    pub const fn new(authority: &'a TopicAuthority) -> Self {
        Self { authority }
    }
}

impl TopicInspectSource for TopicAuthoritySource<'_> {
    fn inspect_topic(&self, topic_id: [u8; 16]) -> Result<TopicInspection, SabiFailure> {
        let record = self
            .authority
            .inspect_topic(TopicId::from_bytes(topic_id))
            .map_err(|error| map_topic_error(&error))?;
        if record.name.len() > nlos_schema::MAX_SYSTEM_CONTROL_TOPIC_NAME_BYTES
            || record.name.contains(&0)
        {
            // The admitted name is unrepresentable on the bounded wire view;
            // fail closed here instead of leaking a malformed projection.
            return Err(SabiFailure {
                code: SabiErrorCode::InvalidArgument.into(),
                retry: RetryDirective::DoNotRetry.into(),
                safe_message: "topic name exceeds the inspect projection bound".to_owned(),
            });
        }
        Ok(TopicInspection {
            topic_id: *record.topic_id.as_bytes(),
            channel_id: *record.channel_id.as_bytes(),
            channel_generation: record.channel_generation.get(),
            name: record.name,
            active_subscriptions: record.active_subscriptions,
            policy_digest: record.policy_digest.to_vec(),
            created_at_ms: record.created_at_ms,
        })
    }
}

fn map_topic_error(error: &TopicAuthorityError) -> SabiFailure {
    let (code, retry, safe_message) = match &error {
        TopicAuthorityError::TopicNotFound(_) => (
            SabiErrorCode::NotFound,
            RetryDirective::DoNotRetry,
            "requested topic was not found",
        ),
        TopicAuthorityError::CorruptRecord(_) => (
            SabiErrorCode::Driver,
            RetryDirective::DoNotRetry,
            "topic authority returned an invalid record",
        ),
        _ => (
            SabiErrorCode::Durability,
            RetryDirective::DoNotRetry,
            "topic authority storage failure",
        ),
    };
    SabiFailure {
        code: code.into(),
        retry: retry.into(),
        safe_message: safe_message.to_owned(),
    }
}
