//! Background `Task` state transition notification wire contract and delivery
//! records (`TASK-NOTIFY-001`, `TASK-STATE-001`).
//!
//! Background task transitions publish through [`crate::NotificationService`]
//! over the single canonical `TopicAuthority` + `WaitAuthority` commit path
//! without introducing any shadow fanout queue or requiring an open chat
//! surface.

use std::error::Error;
use std::fmt;
use std::num::NonZeroU64;

use nlos_types::{ApplicationId, ChannelId, Generation, IdempotencyKey, TaskId};

use crate::NotificationId;

/// Canonical fixed-width wire magic and version tag for
/// [`TaskNotificationEnvelope`].
pub const TASK_NOTIFICATION_WIRE_MAGIC: &[u8; 9] = b"NLOSTNTF\x01";

/// Canonical fixed byte width of an encoded [`TaskNotificationEnvelope`]:
/// `9` (`magic`) + `16` (`task_id`) + `16` (`application_id`) + `8`
/// (`task_generation`) + `1` (`transition`) + `8` (`occurred_at_ms`) + `32`
/// (`summary_digest`) + `1` (`requires_user_action`) = `91`.
pub const TASK_NOTIFICATION_WIRE_LEN: usize = 91;

/// Observable lifecycle state transitions of a background `Task`
/// (`TASK-STATE-001`, `TASK-NOTIFY-001`).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum TaskTransitionKind {
    Proposed = 1,
    WaitingConsent = 2,
    Authorized = 3,
    Running = 4,
    WaitingInput = 5,
    WaitingResource = 6,
    WaitingNetwork = 7,
    Suspended = 8,
    Recovering = 9,
    Cancelling = 10,
    Completing = 11,
    Completed = 12,
    Partial = 13,
    Disputed = 14,
    Failed = 15,
    Cancelled = 16,
}

impl TaskTransitionKind {
    /// Returns the canonical 1-byte wire tag for this transition kind.
    #[must_use]
    pub const fn wire_tag(self) -> u8 {
        self as u8
    }

    /// Decodes a 1-byte wire tag into [`Self`], returning `None` for unknown
    /// or reserved tags.
    #[must_use]
    pub const fn from_wire_tag(tag: u8) -> Option<Self> {
        match tag {
            1 => Some(Self::Proposed),
            2 => Some(Self::WaitingConsent),
            3 => Some(Self::Authorized),
            4 => Some(Self::Running),
            5 => Some(Self::WaitingInput),
            6 => Some(Self::WaitingResource),
            7 => Some(Self::WaitingNetwork),
            8 => Some(Self::Suspended),
            9 => Some(Self::Recovering),
            10 => Some(Self::Cancelling),
            11 => Some(Self::Completing),
            12 => Some(Self::Completed),
            13 => Some(Self::Partial),
            14 => Some(Self::Disputed),
            15 => Some(Self::Failed),
            16 => Some(Self::Cancelled),
            _ => None,
        }
    }

    /// Returns `true` when this state is a terminal `[TASK-STATE-001]`
    /// outcome (`Completed`, `Partial`, `Disputed`, `Failed`, `Cancelled`).
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Partial | Self::Disputed | Self::Failed | Self::Cancelled
        )
    }

    /// Canonical `[TASK-STATE-001]` identifier string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "PROPOSED",
            Self::WaitingConsent => "WAITING_CONSENT",
            Self::Authorized => "AUTHORIZED",
            Self::Running => "RUNNING",
            Self::WaitingInput => "WAITING_INPUT",
            Self::WaitingResource => "WAITING_RESOURCE",
            Self::WaitingNetwork => "WAITING_NETWORK",
            Self::Suspended => "SUSPENDED",
            Self::Recovering => "RECOVERING",
            Self::Cancelling => "CANCELLING",
            Self::Completing => "COMPLETING",
            Self::Completed => "COMPLETED",
            Self::Partial => "PARTIAL",
            Self::Disputed => "DISPUTED",
            Self::Failed => "FAILED",
            Self::Cancelled => "CANCELLED",
        }
    }
}

impl fmt::Display for TaskTransitionKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Canonical payload envelope describing one background `Task` lifecycle
/// transition (`TASK-NOTIFY-001`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskNotificationEnvelope {
    pub task_id: TaskId,
    pub application_id: ApplicationId,
    pub task_generation: Generation,
    pub transition: TaskTransitionKind,
    pub occurred_at_ms: u64,
    pub summary_digest: [u8; 32],
    pub requires_user_action: bool,
}

impl TaskNotificationEnvelope {
    /// Encodes this envelope into a deterministic 91-byte fixed-width wire
    /// array prefixed by [`TASK_NOTIFICATION_WIRE_MAGIC`].
    #[must_use]
    pub const fn encode_fixed(&self) -> [u8; TASK_NOTIFICATION_WIRE_LEN] {
        let mut out = [0u8; TASK_NOTIFICATION_WIRE_LEN];
        let mut cursor = 0usize;

        while cursor < 9 {
            out[cursor] = TASK_NOTIFICATION_WIRE_MAGIC[cursor];
            cursor += 1;
        }

        let task_bytes = self.task_id.as_bytes();
        let mut idx = 0usize;
        while idx < 16 {
            out[9 + idx] = task_bytes[idx];
            idx += 1;
        }

        let app_bytes = self.application_id.as_bytes();
        idx = 0;
        while idx < 16 {
            out[25 + idx] = app_bytes[idx];
            idx += 1;
        }

        let gen_bytes = self.task_generation.get().to_be_bytes();
        idx = 0;
        while idx < 8 {
            out[41 + idx] = gen_bytes[idx];
            idx += 1;
        }

        out[49] = self.transition.wire_tag();

        let time_bytes = self.occurred_at_ms.to_be_bytes();
        idx = 0;
        while idx < 8 {
            out[50 + idx] = time_bytes[idx];
            idx += 1;
        }

        idx = 0;
        while idx < 32 {
            out[58 + idx] = self.summary_digest[idx];
            idx += 1;
        }

        out[90] = if self.requires_user_action { 1 } else { 0 };
        out
    }
}

/// Fail-closed wire decoding errors for [`TaskNotificationEnvelope`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskNotificationWireError {
    InvalidLength { expected: usize, actual: usize },
    InvalidMagic,
    ZeroTaskGeneration,
    UnknownTransitionTag(u8),
    InvalidUserActionFlag(u8),
}

impl fmt::Display for TaskNotificationWireError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLength { expected, actual } => write!(
                formatter,
                "invalid task notification wire length: expected {expected} bytes, got {actual}"
            ),
            Self::InvalidMagic => {
                formatter.write_str("invalid task notification wire magic header")
            }
            Self::ZeroTaskGeneration => {
                formatter.write_str("task notification wire carries zero task_generation")
            }
            Self::UnknownTransitionTag(tag) => {
                write!(
                    formatter,
                    "unknown task notification transition wire tag {tag:#04x}"
                )
            }
            Self::InvalidUserActionFlag(flag) => {
                write!(
                    formatter,
                    "invalid requires_user_action wire byte {flag:#04x} (expected 0 or 1)"
                )
            }
        }
    }
}

impl Error for TaskNotificationWireError {}

/// Encodes [`TaskNotificationEnvelope`] into its canonical 91-byte fixed-width
/// wire representation prefixed by [`TASK_NOTIFICATION_WIRE_MAGIC`].
#[must_use]
pub fn encode_task_notification_envelope(envelope: &TaskNotificationEnvelope) -> Vec<u8> {
    envelope.encode_fixed().to_vec()
}

/// Decodes a canonical 91-byte [`TaskNotificationEnvelope`] wire payload.
///
/// # Errors
///
/// Fails closed on truncated or oversized byte slices, mismatched magic
/// prefix, zero `task_generation`, unknown `TaskTransitionKind` wire tag, or
/// non-boolean `requires_user_action` flag byte.
pub fn decode_task_notification_envelope(
    bytes: &[u8],
) -> Result<TaskNotificationEnvelope, TaskNotificationWireError> {
    if bytes.len() != TASK_NOTIFICATION_WIRE_LEN {
        return Err(TaskNotificationWireError::InvalidLength {
            expected: TASK_NOTIFICATION_WIRE_LEN,
            actual: bytes.len(),
        });
    }
    if &bytes[0..9] != TASK_NOTIFICATION_WIRE_MAGIC {
        return Err(TaskNotificationWireError::InvalidMagic);
    }

    let mut task_bytes = [0u8; 16];
    task_bytes.copy_from_slice(&bytes[9..25]);

    let mut app_bytes = [0u8; 16];
    app_bytes.copy_from_slice(&bytes[25..41]);

    let mut gen_bytes = [0u8; 8];
    gen_bytes.copy_from_slice(&bytes[41..49]);
    let raw_generation = u64::from_be_bytes(gen_bytes);
    let task_generation = NonZeroU64::new(raw_generation)
        .map(Generation::new)
        .ok_or(TaskNotificationWireError::ZeroTaskGeneration)?;

    let transition = TaskTransitionKind::from_wire_tag(bytes[49])
        .ok_or(TaskNotificationWireError::UnknownTransitionTag(bytes[49]))?;

    let mut time_bytes = [0u8; 8];
    time_bytes.copy_from_slice(&bytes[50..58]);
    let occurred_at_ms = u64::from_be_bytes(time_bytes);

    let mut summary_digest = [0u8; 32];
    summary_digest.copy_from_slice(&bytes[58..90]);

    let requires_user_action = match bytes[90] {
        0 => false,
        1 => true,
        other => return Err(TaskNotificationWireError::InvalidUserActionFlag(other)),
    };

    Ok(TaskNotificationEnvelope {
        task_id: TaskId::from_bytes(task_bytes),
        application_id: ApplicationId::from_bytes(app_bytes),
        task_generation,
        transition,
        occurred_at_ms,
        summary_digest,
        requires_user_action,
    })
}

/// One decoded background `Task` notification read back from the single
/// `TopicAuthority` / `ChannelAuthority` fanout log (`TASK-NOTIFY-001`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeliveredTaskNotification {
    pub notification_id: NotificationId,
    pub channel_id: ChannelId,
    pub sequence: u64,
    pub enqueued_at_ms: u64,
    pub publication_idempotency_key: IdempotencyKey,
    pub envelope: TaskNotificationEnvelope,
}

/// Compact `Task` transition metadata attached to a durable offline
/// notification history reference (`DUI-OFFLINE-001`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TaskHistoryMetadata {
    pub task_id: TaskId,
    pub application_id: ApplicationId,
    pub task_generation: Generation,
    pub transition: TaskTransitionKind,
    pub requires_user_action: bool,
    pub summary_digest: [u8; 32],
}

impl From<&TaskNotificationEnvelope> for TaskHistoryMetadata {
    fn from(envelope: &TaskNotificationEnvelope) -> Self {
        Self {
            task_id: envelope.task_id,
            application_id: envelope.application_id,
            task_generation: envelope.task_generation,
            transition: envelope.transition,
            requires_user_action: envelope.requires_user_action,
            summary_digest: envelope.summary_digest,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_sixteen_task_transition_tags_roundtrip_and_classify_terminality() {
        let all = [
            (TaskTransitionKind::Proposed, 1, false),
            (TaskTransitionKind::WaitingConsent, 2, false),
            (TaskTransitionKind::Authorized, 3, false),
            (TaskTransitionKind::Running, 4, false),
            (TaskTransitionKind::WaitingInput, 5, false),
            (TaskTransitionKind::WaitingResource, 6, false),
            (TaskTransitionKind::WaitingNetwork, 7, false),
            (TaskTransitionKind::Suspended, 8, false),
            (TaskTransitionKind::Recovering, 9, false),
            (TaskTransitionKind::Cancelling, 10, false),
            (TaskTransitionKind::Completing, 11, false),
            (TaskTransitionKind::Completed, 12, true),
            (TaskTransitionKind::Partial, 13, true),
            (TaskTransitionKind::Disputed, 14, true),
            (TaskTransitionKind::Failed, 15, true),
            (TaskTransitionKind::Cancelled, 16, true),
        ];
        for (kind, expected_tag, terminal) in all {
            assert_eq!(kind.wire_tag(), expected_tag);
            assert_eq!(TaskTransitionKind::from_wire_tag(expected_tag), Some(kind));
            assert_eq!(kind.is_terminal(), terminal);
            assert_ne!(kind.as_str(), "");
        }
        assert_eq!(TaskTransitionKind::from_wire_tag(0), None);
        assert_eq!(TaskTransitionKind::from_wire_tag(17), None);
    }

    #[test]
    fn task_envelope_fixed_wire_encoding_is_deterministic() {
        let envelope = TaskNotificationEnvelope {
            task_id: TaskId::from_bytes([0x11; 16]),
            application_id: ApplicationId::from_bytes([0x22; 16]),
            task_generation: Generation::INITIAL.checked_next().expect("gen 2"),
            transition: TaskTransitionKind::WaitingConsent,
            occurred_at_ms: 42_000,
            summary_digest: [0xAB; 32],
            requires_user_action: true,
        };
        let wire = encode_task_notification_envelope(&envelope);
        assert_eq!(wire.len(), TASK_NOTIFICATION_WIRE_LEN);
        assert_eq!(&wire[..9], TASK_NOTIFICATION_WIRE_MAGIC);
        let decoded = decode_task_notification_envelope(&wire).expect("decode");
        assert_eq!(decoded, envelope);
    }
}
