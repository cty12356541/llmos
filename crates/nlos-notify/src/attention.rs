//! Durable human-attention policy controller (`[RSM-HUMAN-001]`,
//! `[RSM-ATTN-002]`, `[TUI-CONSENT-001]`, `[DUI-NOTIFY-001]`).
//!
//! Operates as an isolated sidecar `SQLite` database (`attention-policy.db`,
//! `ATTENTION_POLICY_SCHEMA_VERSION = 1`) beside `notify-service.db`
//! (`SCHEMA_VERSION = 2`) without touching `NotificationService` schema or
//! fanout state.
//!
//! Core guarantees:
//! 1. **No payment bypass (`[RSM-HUMAN-001]`)**: any request with
//!    `payment_priority_override_attempted == true` fails closed unconditionally
//!    with [`AttentionPolicyError::PaymentBypassForbidden`] after persisting an
//!    immutable audit rejection receipt.
//! 2. **Origin isolation (`[TUI-CONSENT-001]`)**: `ConsentPrompt` and
//!    `SystemCritical` requests are accepted solely from
//!    [`AttentionOrigin::TrustedSystemUi`]; application origins fail closed
//!    with [`AttentionPolicyError::UntrustedConsentOrigin`] and persist an
//!    audit rejection receipt.
//! 3. **Dual-lane isolation (`[RSM-ATTN-002]`)**: trusted `ConsentPrompt` and
//!    `SystemCritical` alerts route directly to
//!    [`AttentionQueueTarget::PriorityConsentLane`] and are never blocked or
//!    throttled by quiet hours, coalescing, or per-app/global ordinary quotas.
//! 4. **Quiet hours, coalescing & window quotas**: `Ordinary` and
//!    `BackgroundDigest` requests first check local quiet hours (including
//!    cross-midnight intervals and `utc_offset_minutes`), then collapse matching
//!    `(origin, coalesce_key, active_window)` bursts when coalescing is
//!    enabled, and otherwise enforce per-origin and global window quotas with
//!    deterministic `idempotency_key` replay across restarts.

#![forbid(unsafe_code)]

use std::error::Error;
use std::fmt;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use nlos_types::{ApplicationId, IdempotencyKey, ReceiptId};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};

/// Filename of the sidecar attention-policy `SQLite` ledger.
pub const ATTENTION_POLICY_DB_FILENAME: &str = "attention-policy.db";

/// Schema version stored in `PRAGMA user_version` for `attention-policy.db`.
pub const ATTENTION_POLICY_SCHEMA_VERSION: i64 = 1;

/// Maximum UTF-8 byte length permitted for [`AttentionDeliveryRequest::coalesce_key`].
pub const MAX_COALESCE_KEY_LEN: usize = 256;

/// Maximum bounded page size accepted by [`AttentionPolicyController::list_receipts`].
pub const MAX_RECEIPT_LIST_LIMIT: usize = 1_024;

const MINUTES_PER_DAY: u16 = 1_440;
const MIN_UTC_OFFSET_MINUTES: i16 = -720;
const MAX_UTC_OFFSET_MINUTES: i16 = 840;
const MS_PER_MINUTE: i128 = 60_000;
const MS_PER_DAY: i128 = 1_440 * MS_PER_MINUTE;

const INIT_V1_SQL: &str = "
CREATE TABLE attention_policy_config (
    policy_generation INTEGER PRIMARY KEY NOT NULL CHECK(policy_generation >= 1),
    max_per_app_per_window INTEGER NOT NULL CHECK(max_per_app_per_window >= 1),
    max_global_ordinary_per_window INTEGER NOT NULL CHECK(max_global_ordinary_per_window >= 1),
    window_ms INTEGER NOT NULL CHECK(window_ms >= 1),
    allow_coalescing INTEGER NOT NULL CHECK(allow_coalescing IN (0, 1)),
    quiet_enabled INTEGER NOT NULL CHECK(quiet_enabled IN (0, 1)),
    quiet_start_minute INTEGER NOT NULL CHECK(quiet_start_minute >= 0 AND quiet_start_minute < 1440),
    quiet_end_minute INTEGER NOT NULL CHECK(quiet_end_minute >= 0 AND quiet_end_minute < 1440),
    quiet_utc_offset_minutes INTEGER NOT NULL
        CHECK(quiet_utc_offset_minutes >= -720 AND quiet_utc_offset_minutes <= 840),
    CHECK(quiet_enabled = 0 OR quiet_start_minute != quiet_end_minute)
) STRICT;

CREATE TRIGGER trg_attention_policy_config_no_update
BEFORE UPDATE ON attention_policy_config
BEGIN
    SELECT RAISE(ABORT, 'attention_policy_config rows are immutable');
END;

CREATE TRIGGER trg_attention_policy_config_no_delete
BEFORE DELETE ON attention_policy_config
BEGIN
    SELECT RAISE(ABORT, 'attention_policy_config rows cannot be deleted');
END;

CREATE TABLE attention_decision_receipts (
    receipt_seq INTEGER PRIMARY KEY AUTOINCREMENT,
    receipt_id BLOB NOT NULL UNIQUE CHECK(length(receipt_id) = 16),
    idempotency_key BLOB NOT NULL UNIQUE CHECK(length(idempotency_key) = 16),
    request_fingerprint BLOB NOT NULL CHECK(length(request_fingerprint) = 32),
    policy_generation INTEGER NOT NULL
        REFERENCES attention_policy_config(policy_generation),
    origin_kind TEXT NOT NULL CHECK(origin_kind IN ('TRUSTED_SYSTEM_UI', 'APPLICATION')),
    origin_key BLOB NOT NULL CHECK(length(origin_key) = 17),
    application_id BLOB CHECK(application_id IS NULL OR length(application_id) = 16),
    lane TEXT NOT NULL
        CHECK(lane IN ('CONSENT_PROMPT', 'SYSTEM_CRITICAL', 'ORDINARY', 'BACKGROUND_DIGEST')),
    coalesce_key TEXT CHECK(
        coalesce_key IS NULL
        OR (length(coalesce_key) > 0 AND length(CAST(coalesce_key AS BLOB)) <= 256)
    ),
    requested_at_ms INTEGER NOT NULL CHECK(requested_at_ms >= 0),
    payment_priority_override_attempted INTEGER NOT NULL
        CHECK(payment_priority_override_attempted IN (0, 1)),
    outcome TEXT NOT NULL CHECK(outcome IN (
        'DELIVERED',
        'COALESCED',
        'RATE_LIMITED',
        'DEFERRED_QUIET_HOURS',
        'REJECTED_PAYMENT_BYPASS',
        'REJECTED_UNTRUSTED_ORIGIN'
    )),
    queue_target TEXT CHECK(
        queue_target IS NULL
        OR queue_target IN ('PRIORITY_CONSENT_LANE', 'STANDARD_NOTIFICATION_LANE')
    ),
    window_start_ms INTEGER CHECK(window_start_ms IS NULL OR window_start_ms >= 0),
    coalesced_count INTEGER CHECK(coalesced_count IS NULL OR coalesced_count >= 1),
    retry_after_ms INTEGER CHECK(retry_after_ms IS NULL OR retry_after_ms >= 1),
    resume_at_ms INTEGER CHECK(resume_at_ms IS NULL OR resume_at_ms >= 0),
    CHECK(
        (origin_kind = 'TRUSTED_SYSTEM_UI' AND application_id IS NULL)
        OR (origin_kind = 'APPLICATION' AND application_id IS NOT NULL)
    ),
    CHECK(
        (outcome = 'DELIVERED'
            AND queue_target IS NOT NULL
            AND coalesced_count IS NULL
            AND retry_after_ms IS NULL
            AND resume_at_ms IS NULL)
        OR (outcome = 'COALESCED'
            AND queue_target IS NULL
            AND coalesce_key IS NOT NULL
            AND coalesced_count IS NOT NULL
            AND window_start_ms IS NOT NULL
            AND retry_after_ms IS NULL
            AND resume_at_ms IS NULL)
        OR (outcome = 'RATE_LIMITED'
            AND queue_target IS NULL
            AND coalesced_count IS NULL
            AND retry_after_ms IS NOT NULL
            AND window_start_ms IS NOT NULL
            AND resume_at_ms IS NULL)
        OR (outcome = 'DEFERRED_QUIET_HOURS'
            AND queue_target IS NULL
            AND coalesced_count IS NULL
            AND retry_after_ms IS NULL
            AND resume_at_ms IS NOT NULL)
        OR (outcome = 'REJECTED_PAYMENT_BYPASS'
            AND queue_target IS NULL
            AND coalesced_count IS NULL
            AND retry_after_ms IS NULL
            AND resume_at_ms IS NULL
            AND payment_priority_override_attempted = 1)
        OR (outcome = 'REJECTED_UNTRUSTED_ORIGIN'
            AND queue_target IS NULL
            AND coalesced_count IS NULL
            AND retry_after_ms IS NULL
            AND resume_at_ms IS NULL
            AND origin_kind = 'APPLICATION'
            AND lane IN ('CONSENT_PROMPT', 'SYSTEM_CRITICAL'))
    )
) STRICT;

CREATE INDEX idx_attention_receipts_window_quota
    ON attention_decision_receipts (outcome, queue_target, requested_at_ms);

CREATE INDEX idx_attention_receipts_origin_window_quota
    ON attention_decision_receipts (outcome, queue_target, origin_key, requested_at_ms);

CREATE TRIGGER trg_attention_decision_receipts_no_update
BEFORE UPDATE ON attention_decision_receipts
BEGIN
    SELECT RAISE(ABORT, 'attention_decision_receipts rows are immutable');
END;

CREATE TRIGGER trg_attention_decision_receipts_no_delete
BEFORE DELETE ON attention_decision_receipts
BEGIN
    SELECT RAISE(ABORT, 'attention_decision_receipts rows cannot be deleted');
END;

CREATE TABLE attention_coalesce_counters (
    origin_key BLOB NOT NULL CHECK(length(origin_key) = 17),
    origin_kind TEXT NOT NULL CHECK(origin_kind IN ('TRUSTED_SYSTEM_UI', 'APPLICATION')),
    application_id BLOB CHECK(application_id IS NULL OR length(application_id) = 16),
    coalesce_key TEXT NOT NULL CHECK(
        length(coalesce_key) > 0 AND length(CAST(coalesce_key AS BLOB)) <= 256
    ),
    window_start_ms INTEGER NOT NULL CHECK(window_start_ms >= 0),
    delivered_count INTEGER NOT NULL CHECK(delivered_count = 1),
    coalesced_count INTEGER NOT NULL CHECK(coalesced_count >= 0),
    initial_receipt_id BLOB NOT NULL CHECK(length(initial_receipt_id) = 16)
        REFERENCES attention_decision_receipts(receipt_id),
    last_receipt_id BLOB NOT NULL CHECK(length(last_receipt_id) = 16)
        REFERENCES attention_decision_receipts(receipt_id),
    last_updated_at_ms INTEGER NOT NULL CHECK(last_updated_at_ms >= 0),
    PRIMARY KEY (origin_key, coalesce_key, window_start_ms),
    CHECK(
        (origin_kind = 'TRUSTED_SYSTEM_UI' AND application_id IS NULL)
        OR (origin_kind = 'APPLICATION' AND application_id IS NOT NULL)
    )
) STRICT;

CREATE TRIGGER trg_attention_coalesce_counters_monotonic_update
BEFORE UPDATE ON attention_coalesce_counters
WHEN OLD.origin_key IS NOT NEW.origin_key
  OR OLD.origin_kind IS NOT NEW.origin_kind
  OR OLD.application_id IS NOT NEW.application_id
  OR OLD.coalesce_key IS NOT NEW.coalesce_key
  OR OLD.window_start_ms IS NOT NEW.window_start_ms
  OR OLD.delivered_count IS NOT NEW.delivered_count
  OR OLD.initial_receipt_id IS NOT NEW.initial_receipt_id
  OR NEW.coalesced_count != OLD.coalesced_count + 1
BEGIN
    SELECT RAISE(
        ABORT,
        'attention_coalesce_counters updates must preserve keys and increment coalesced_count by 1'
    );
END;

CREATE TRIGGER trg_attention_coalesce_counters_no_delete
BEFORE DELETE ON attention_coalesce_counters
BEGIN
    SELECT RAISE(ABORT, 'attention_coalesce_counters rows cannot be deleted');
END;

PRAGMA user_version = 1;
";

/// Attention classification of an incoming notification or consent prompt.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum AttentionLaneClass {
    /// High-risk authorization / `Z4` Trusted UI consent prompt.
    ConsentPrompt,
    /// System-critical security or hardware-safety alert from Trusted UI.
    SystemCritical,
    /// Ordinary application or task notification subject to quiet hours and quotas.
    Ordinary,
    /// Low-priority background summary / digest notification.
    BackgroundDigest,
}

impl AttentionLaneClass {
    /// Returns `true` when this lane represents a high-risk priority surface
    /// reserved exclusively for [`AttentionOrigin::TrustedSystemUi`].
    #[must_use]
    pub const fn is_priority_class(self) -> bool {
        matches!(self, Self::ConsentPrompt | Self::SystemCritical)
    }

    const fn as_db_str(self) -> &'static str {
        match self {
            Self::ConsentPrompt => "CONSENT_PROMPT",
            Self::SystemCritical => "SYSTEM_CRITICAL",
            Self::Ordinary => "ORDINARY",
            Self::BackgroundDigest => "BACKGROUND_DIGEST",
        }
    }

    const fn tag_byte(self) -> u8 {
        match self {
            Self::ConsentPrompt => 1,
            Self::SystemCritical => 2,
            Self::Ordinary => 3,
            Self::BackgroundDigest => 4,
        }
    }

    fn from_db_str(raw: &str) -> Result<Self, AttentionPolicyError> {
        match raw {
            "CONSENT_PROMPT" => Ok(Self::ConsentPrompt),
            "SYSTEM_CRITICAL" => Ok(Self::SystemCritical),
            "ORDINARY" => Ok(Self::Ordinary),
            "BACKGROUND_DIGEST" => Ok(Self::BackgroundDigest),
            _ => Err(AttentionPolicyError::CorruptRecord(
                "unknown stored attention lane class",
            )),
        }
    }
}

/// Verified caller origin presenting an attention delivery request.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum AttentionOrigin {
    /// Authenticated Trusted UI / system security surface (`Z4` authority).
    TrustedSystemUi,
    /// Ordinary sandboxed or host-registered application identity.
    Application(ApplicationId),
}

impl AttentionOrigin {
    const fn db_kind(self) -> &'static str {
        match self {
            Self::TrustedSystemUi => "TRUSTED_SYSTEM_UI",
            Self::Application(_) => "APPLICATION",
        }
    }

    const fn application_id(self) -> Option<ApplicationId> {
        match self {
            Self::TrustedSystemUi => None,
            Self::Application(app_id) => Some(app_id),
        }
    }

    fn canonical_key(self) -> [u8; 17] {
        let mut out = [0u8; 17];
        if let Self::Application(app_id) = self {
            out[0] = 1;
            out[1..17].copy_from_slice(app_id.as_bytes());
        }
        out
    }
}

/// Strictly isolated target queue selected for an admitted delivery.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum AttentionQueueTarget {
    /// Isolated high-priority queue for verified `ConsentPrompt` / `SystemCritical`.
    PriorityConsentLane,
    /// Standard user-attention notification queue subject to throttling and coalescing.
    StandardNotificationLane,
}

impl AttentionQueueTarget {
    const fn as_db_str(self) -> &'static str {
        match self {
            Self::PriorityConsentLane => "PRIORITY_CONSENT_LANE",
            Self::StandardNotificationLane => "STANDARD_NOTIFICATION_LANE",
        }
    }

    fn from_db_str(raw: &str) -> Result<Self, AttentionPolicyError> {
        match raw {
            "PRIORITY_CONSENT_LANE" => Ok(Self::PriorityConsentLane),
            "STANDARD_NOTIFICATION_LANE" => Ok(Self::StandardNotificationLane),
            _ => Err(AttentionPolicyError::CorruptRecord(
                "unknown stored attention queue target",
            )),
        }
    }
}

/// Tumbling-window rate and coalescing policy for non-critical notifications
/// (`Ordinary` and `BackgroundDigest`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AttentionRatePolicy {
    pub max_per_app_per_window: u32,
    pub max_global_ordinary_per_window: u32,
    pub window_ms: u64,
    pub allow_coalescing: bool,
}

impl AttentionRatePolicy {
    /// Validates rate-policy bounds, failing closed on zero limits or `i64`
    /// overflow.
    ///
    /// # Errors
    ///
    /// Returns [`AttentionPolicyError::InvalidPolicy`] when any numeric bound
    /// is zero or exceeds `i64::MAX`.
    pub fn validate(&self) -> Result<(), AttentionPolicyError> {
        if self.max_per_app_per_window == 0 {
            return Err(AttentionPolicyError::InvalidPolicy(
                "max_per_app_per_window must be non-zero",
            ));
        }
        if self.max_global_ordinary_per_window == 0 {
            return Err(AttentionPolicyError::InvalidPolicy(
                "max_global_ordinary_per_window must be non-zero",
            ));
        }
        if self.window_ms == 0 {
            return Err(AttentionPolicyError::InvalidPolicy(
                "window_ms must be non-zero",
            ));
        }
        if i64::try_from(self.window_ms).is_err() {
            return Err(AttentionPolicyError::InvalidPolicy(
                "window_ms exceeds SQLite i64",
            ));
        }
        Ok(())
    }
}

/// Local-time quiet-hours window supporting both daytime (`13:00..15:00`) and
/// cross-midnight (`22:00..07:00`) schedules.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QuietHoursPolicy {
    pub enabled: bool,
    pub start_minute_of_day: u16,
    pub end_minute_of_day: u16,
    pub utc_offset_minutes: i16,
}

impl QuietHoursPolicy {
    /// Validates quiet-hours field ranges (`< 1440` minutes of day,
    /// `utc_offset_minutes` within `-720..=840`, and distinct start/end when
    /// enabled).
    ///
    /// # Errors
    ///
    /// Returns [`AttentionPolicyError::InvalidPolicy`] if any field violates
    /// its valid range.
    pub fn validate(&self) -> Result<(), AttentionPolicyError> {
        if self.start_minute_of_day >= MINUTES_PER_DAY {
            return Err(AttentionPolicyError::InvalidPolicy(
                "start_minute_of_day must be < 1440",
            ));
        }
        if self.end_minute_of_day >= MINUTES_PER_DAY {
            return Err(AttentionPolicyError::InvalidPolicy(
                "end_minute_of_day must be < 1440",
            ));
        }
        if !(MIN_UTC_OFFSET_MINUTES..=MAX_UTC_OFFSET_MINUTES).contains(&self.utc_offset_minutes) {
            return Err(AttentionPolicyError::InvalidPolicy(
                "utc_offset_minutes must be within -720..=840",
            ));
        }
        if self.enabled && self.start_minute_of_day == self.end_minute_of_day {
            return Err(AttentionPolicyError::InvalidPolicy(
                "enabled quiet hours require distinct start_minute_of_day and end_minute_of_day",
            ));
        }
        Ok(())
    }
}

/// Request evaluated by [`AttentionPolicyController::evaluate`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttentionDeliveryRequest {
    pub idempotency_key: IdempotencyKey,
    pub origin: AttentionOrigin,
    pub lane: AttentionLaneClass,
    pub coalesce_key: Option<String>,
    pub requested_at_ms: u64,
    pub payment_priority_override_attempted: bool,
}

/// Deterministic decision returned by [`AttentionPolicyController::evaluate`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AttentionDecision {
    Delivered {
        queue: AttentionQueueTarget,
        receipt_id: ReceiptId,
    },
    Coalesced {
        coalesce_key: String,
        coalesced_count: u64,
        window_start_ms: u64,
        receipt_id: ReceiptId,
    },
    RateLimited {
        retry_after_ms: u64,
        receipt_id: ReceiptId,
    },
    DeferredQuietHours {
        resume_at_ms: u64,
        receipt_id: ReceiptId,
    },
}

impl AttentionDecision {
    /// Returns the durable [`ReceiptId`] bound to this decision.
    #[must_use]
    pub const fn receipt_id(&self) -> ReceiptId {
        match self {
            Self::Delivered { receipt_id, .. }
            | Self::Coalesced { receipt_id, .. }
            | Self::RateLimited { receipt_id, .. }
            | Self::DeferredQuietHours { receipt_id, .. } => *receipt_id,
        }
    }
}

/// Durable outcome classification recorded in `attention_decision_receipts`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AttentionOutcomeKind {
    Delivered,
    Coalesced,
    RateLimited,
    DeferredQuietHours,
    RejectedPaymentBypass,
    RejectedUntrustedOrigin,
}

impl AttentionOutcomeKind {
    const fn as_db_str(self) -> &'static str {
        match self {
            Self::Delivered => "DELIVERED",
            Self::Coalesced => "COALESCED",
            Self::RateLimited => "RATE_LIMITED",
            Self::DeferredQuietHours => "DEFERRED_QUIET_HOURS",
            Self::RejectedPaymentBypass => "REJECTED_PAYMENT_BYPASS",
            Self::RejectedUntrustedOrigin => "REJECTED_UNTRUSTED_ORIGIN",
        }
    }

    fn from_db_str(raw: &str) -> Result<Self, AttentionPolicyError> {
        match raw {
            "DELIVERED" => Ok(Self::Delivered),
            "COALESCED" => Ok(Self::Coalesced),
            "RATE_LIMITED" => Ok(Self::RateLimited),
            "DEFERRED_QUIET_HOURS" => Ok(Self::DeferredQuietHours),
            "REJECTED_PAYMENT_BYPASS" => Ok(Self::RejectedPaymentBypass),
            "REJECTED_UNTRUSTED_ORIGIN" => Ok(Self::RejectedUntrustedOrigin),
            _ => Err(AttentionPolicyError::CorruptRecord(
                "unknown stored attention outcome kind",
            )),
        }
    }
}

/// Immutable audit receipt stored in `attention_decision_receipts`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AttentionReceiptRecord {
    pub receipt_seq: u64,
    pub receipt_id: ReceiptId,
    pub idempotency_key: IdempotencyKey,
    pub request_fingerprint: [u8; 32],
    pub policy_generation: u64,
    pub origin: AttentionOrigin,
    pub lane: AttentionLaneClass,
    pub coalesce_key: Option<String>,
    pub requested_at_ms: u64,
    pub payment_priority_override_attempted: bool,
    pub outcome: AttentionOutcomeKind,
    pub queue_target: Option<AttentionQueueTarget>,
    pub window_start_ms: Option<u64>,
    pub coalesced_count: Option<u64>,
    pub retry_after_ms: Option<u64>,
    pub resume_at_ms: Option<u64>,
}

/// Active-window coalescing statistics stored in `attention_coalesce_counters`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CoalesceCounterRecord {
    pub origin: AttentionOrigin,
    pub coalesce_key: String,
    pub window_start_ms: u64,
    pub delivered_count: u64,
    pub coalesced_count: u64,
    pub initial_receipt_id: ReceiptId,
    pub last_receipt_id: ReceiptId,
    pub last_updated_at_ms: u64,
}

impl CoalesceCounterRecord {
    /// Total events absorbed by this coalesce bucket (`delivered_count + coalesced_count`).
    #[must_use]
    pub const fn total_events(&self) -> u64 {
        self.delivered_count.saturating_add(self.coalesced_count)
    }
}

/// Active-window quota usage snapshot for [`AttentionQueueTarget::StandardNotificationLane`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowUsageSnapshot {
    pub window_start_ms: u64,
    pub window_end_ms: u64,
    pub per_origin_delivered: u64,
    pub global_standard_delivered: u64,
}

/// Fail-closed typed errors produced by [`AttentionPolicyController`].
#[derive(Debug)]
pub enum AttentionPolicyError {
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
    DurabilityUnavailable {
        journal_mode: String,
        synchronous: i64,
        foreign_keys: i64,
    },
    SchemaVersionUnsupported(i64),
    InvalidPolicy(&'static str),
    InvalidRequest(&'static str),
    /// Payment-priority bypass attempt rejected fail-closed (`[RSM-HUMAN-001]`).
    PaymentBypassForbidden,
    /// Untrusted application attempted to claim `ConsentPrompt` or
    /// `SystemCritical` (`[TUI-CONSENT-001]`, `[RSM-ATTN-002]`).
    UntrustedConsentOrigin,
    /// Reused `idempotency_key` with conflicting request parameters.
    IdempotencyConflict(IdempotencyKey),
    CorruptRecord(&'static str),
    LockPoisoned,
}

impl fmt::Display for AttentionPolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(formatter, "SQLite attention policy failure: {error}"),
            Self::Io(error) => write!(formatter, "attention policy I/O failure: {error}"),
            Self::DurabilityUnavailable {
                journal_mode,
                synchronous,
                foreign_keys,
            } => write!(
                formatter,
                "attention-policy.db durability unavailable: journal_mode={journal_mode}, \
                 synchronous={synchronous}, foreign_keys={foreign_keys}"
            ),
            Self::SchemaVersionUnsupported(version) => {
                write!(
                    formatter,
                    "unsupported attention-policy.db schema version {version}"
                )
            }
            Self::InvalidPolicy(reason) => {
                write!(formatter, "invalid attention policy configuration: {reason}")
            }
            Self::InvalidRequest(reason) => {
                write!(formatter, "invalid attention delivery request: {reason}")
            }
            Self::PaymentBypassForbidden => formatter.write_str(
                "payment priority override attempted: human attention cannot be bypassed with credits",
            ),
            Self::UntrustedConsentOrigin => formatter.write_str(
                "untrusted application origin cannot request ConsentPrompt or SystemCritical lane",
            ),
            Self::IdempotencyConflict(key) => write!(
                formatter,
                "attention idempotency key {key:?} rebound to conflicting request parameters"
            ),
            Self::CorruptRecord(reason) => {
                write!(formatter, "corrupt attention policy record: {reason}")
            }
            Self::LockPoisoned => {
                formatter.write_str("attention policy controller lock is poisoned")
            }
        }
    }
}

impl Error for AttentionPolicyError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for AttentionPolicyError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

/// Durable human-attention policy controller backed by `<root>/attention-policy.db`.
pub struct AttentionPolicyController {
    connection: Mutex<Connection>,
}

impl AttentionPolicyController {
    /// Opens or creates `<root>/attention-policy.db` and applies or verifies the
    /// active `(rate_policy, quiet_hours)` configuration.
    ///
    /// Initializes schema `ATTENTION_POLICY_SCHEMA_VERSION` (`1`) when
    /// `user_version == 0`, verifies all `STRICT` tables and immutability
    /// triggers when `user_version == 1`, and appends a new immutable
    /// `attention_policy_config` revision if the supplied policies differ from
    /// the stored active revision.
    ///
    /// # Errors
    ///
    /// Fails closed on invalid policies, missing `WAL`/`FULL`/`foreign_keys`
    /// durability, unsupported schema versions, missing/tampered schema
    /// objects, or `SQLite`/I/O failures.
    pub fn open(
        root: impl AsRef<Path>,
        rate_policy: AttentionRatePolicy,
        quiet_hours: QuietHoursPolicy,
    ) -> Result<Self, AttentionPolicyError> {
        rate_policy.validate()?;
        quiet_hours.validate()?;

        let mut connection = open_configured_connection(root.as_ref())?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        match version {
            0 => initialize_v1(&mut connection, rate_policy, quiet_hours)?,
            ATTENTION_POLICY_SCHEMA_VERSION => {
                verify_v1(&connection)?;
                sync_policy_if_changed(&mut connection, rate_policy, quiet_hours)?;
            }
            other => return Err(AttentionPolicyError::SchemaVersionUnsupported(other)),
        }

        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// Reopens an existing `<root>/attention-policy.db` using the persisted
    /// active configuration in `attention_policy_config`.
    ///
    /// # Errors
    ///
    /// Fails closed when the database does not carry schema version `1`, lacks
    /// `WAL`/`FULL`/`foreign_keys` durability, has tampered schema objects, or
    /// contains no active policy configuration row.
    pub fn open_existing(root: impl AsRef<Path>) -> Result<Self, AttentionPolicyError> {
        let connection = open_configured_connection(root.as_ref())?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if version != ATTENTION_POLICY_SCHEMA_VERSION {
            return Err(AttentionPolicyError::SchemaVersionUnsupported(version));
        }
        verify_v1(&connection)?;
        let _ = load_latest_policy(&connection)?;

        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>, AttentionPolicyError> {
        self.connection
            .lock()
            .map_err(|_| AttentionPolicyError::LockPoisoned)
    }

    /// Returns the currently active `(AttentionRatePolicy, QuietHoursPolicy)`.
    ///
    /// # Errors
    ///
    /// Fails closed on lock poisoning or `SQLite`/corruption errors.
    pub fn current_policy(
        &self,
    ) -> Result<(AttentionRatePolicy, QuietHoursPolicy), AttentionPolicyError> {
        let connection = self.lock()?;
        let (_generation, rate, quiet) = load_latest_policy(&connection)?;
        Ok((rate, quiet))
    }

    /// Returns the active monotonic `policy_generation`.
    ///
    /// # Errors
    ///
    /// Fails closed on lock poisoning or `SQLite`/corruption errors.
    pub fn policy_generation(&self) -> Result<u64, AttentionPolicyError> {
        let connection = self.lock()?;
        let (generation, _rate, _quiet) = load_latest_policy(&connection)?;
        Ok(generation)
    }

    /// Appends a new immutable policy revision to `attention_policy_config` when
    /// `(rate_policy, quiet_hours)` differs from the current revision,
    /// returning the active `policy_generation`.
    ///
    /// # Errors
    ///
    /// Fails closed on invalid policies, lock poisoning, or `SQLite` errors.
    pub fn update_policy(
        &self,
        rate_policy: AttentionRatePolicy,
        quiet_hours: QuietHoursPolicy,
    ) -> Result<u64, AttentionPolicyError> {
        rate_policy.validate()?;
        quiet_hours.validate()?;

        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (current_gen, current_rate, current_quiet) = load_latest_policy(&transaction)?;
        if current_rate == rate_policy && current_quiet == quiet_hours {
            return Ok(current_gen);
        }
        let next_gen = current_gen
            .checked_add(1)
            .ok_or(AttentionPolicyError::InvalidPolicy(
                "policy_generation overflow",
            ))?;
        insert_policy_row(&transaction, next_gen, rate_policy, quiet_hours)?;
        transaction.commit()?;
        Ok(next_gen)
    }

    /// Evaluates one attention delivery request under the active policy,
    /// durably persisting its immutable receipt and updating active-window
    /// coalesce counters atomically.
    ///
    /// # Errors
    ///
    /// Fails closed with:
    /// - [`AttentionPolicyError::InvalidRequest`] on malformed bounds
    ///   (`requested_at_ms > i64::MAX`, empty/oversized/NUL `coalesce_key`);
    /// - [`AttentionPolicyError::IdempotencyConflict`] when `idempotency_key`
    ///   was previously bound to different request parameters;
    /// - [`AttentionPolicyError::PaymentBypassForbidden`] when
    ///   `payment_priority_override_attempted == true` (after durably committing
    ///   an audit rejection receipt);
    /// - [`AttentionPolicyError::UntrustedConsentOrigin`] when an
    ///   [`AttentionOrigin::Application`] requests `ConsentPrompt` or
    ///   `SystemCritical` (after durably committing an audit rejection receipt);
    /// - [`AttentionPolicyError::Sqlite`] / [`AttentionPolicyError::CorruptRecord`]
    ///   on storage or data-integrity failures.
    pub fn evaluate(
        &self,
        request: &AttentionDeliveryRequest,
    ) -> Result<AttentionDecision, AttentionPolicyError> {
        validate_request(request)?;
        let fingerprint = compute_request_fingerprint(request);

        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;

        if let Some(existing) = load_receipt_by_key(&transaction, request.idempotency_key)? {
            return replay_existing_receipt(&existing, request, &fingerprint);
        }

        let (policy_generation, rate_policy, quiet_hours) = load_latest_policy(&transaction)?;
        let receipt_id = derive_receipt_id(request.idempotency_key, &fingerprint);

        if request.payment_priority_override_attempted {
            let record = build_base_receipt(
                receipt_id,
                request,
                fingerprint,
                policy_generation,
                AttentionOutcomeKind::RejectedPaymentBypass,
            );
            insert_receipt(&transaction, &record)?;
            transaction.commit()?;
            return Err(AttentionPolicyError::PaymentBypassForbidden);
        }

        if matches!(request.origin, AttentionOrigin::Application(_))
            && request.lane.is_priority_class()
        {
            let record = build_base_receipt(
                receipt_id,
                request,
                fingerprint,
                policy_generation,
                AttentionOutcomeKind::RejectedUntrustedOrigin,
            );
            insert_receipt(&transaction, &record)?;
            transaction.commit()?;
            return Err(AttentionPolicyError::UntrustedConsentOrigin);
        }

        if matches!(request.origin, AttentionOrigin::TrustedSystemUi)
            && request.lane.is_priority_class()
        {
            let mut record = build_base_receipt(
                receipt_id,
                request,
                fingerprint,
                policy_generation,
                AttentionOutcomeKind::Delivered,
            );
            record.queue_target = Some(AttentionQueueTarget::PriorityConsentLane);
            insert_receipt(&transaction, &record)?;
            transaction.commit()?;
            return Ok(AttentionDecision::Delivered {
                queue: AttentionQueueTarget::PriorityConsentLane,
                receipt_id,
            });
        }

        let decision = evaluate_standard_lane(
            &transaction,
            request,
            fingerprint,
            policy_generation,
            receipt_id,
            &rate_policy,
            quiet_hours,
        )?;
        transaction.commit()?;
        Ok(decision)
    }

    /// Inspects one durable audit receipt by its [`ReceiptId`].
    ///
    /// # Errors
    ///
    /// Fails closed on lock poisoning or `SQLite`/corruption errors.
    pub fn inspect_receipt(
        &self,
        receipt_id: ReceiptId,
    ) -> Result<Option<AttentionReceiptRecord>, AttentionPolicyError> {
        let connection = self.lock()?;
        load_receipt_by_id(&connection, receipt_id)
    }

    /// Inspects one durable audit receipt by its [`IdempotencyKey`].
    ///
    /// # Errors
    ///
    /// Fails closed on lock poisoning or `SQLite`/corruption errors.
    pub fn inspect_receipt_by_idempotency_key(
        &self,
        idempotency_key: IdempotencyKey,
    ) -> Result<Option<AttentionReceiptRecord>, AttentionPolicyError> {
        let connection = self.lock()?;
        load_receipt_by_key(&connection, idempotency_key)
    }

    /// Lists durable audit receipts ordered by `receipt_seq ASC` strictly after
    /// `after_sequence`.
    ///
    /// # Errors
    ///
    /// Fails closed when `limit == 0`, `limit > MAX_RECEIPT_LIST_LIMIT`,
    /// arguments overflow `SQLite` `i64`, or storage corruption is detected.
    pub fn list_receipts(
        &self,
        after_sequence: u64,
        limit: usize,
    ) -> Result<Vec<AttentionReceiptRecord>, AttentionPolicyError> {
        if limit == 0 {
            return Err(AttentionPolicyError::InvalidRequest(
                "list_receipts limit must be non-zero",
            ));
        }
        if limit > MAX_RECEIPT_LIST_LIMIT {
            return Err(AttentionPolicyError::InvalidRequest(
                "list_receipts limit exceeds MAX_RECEIPT_LIST_LIMIT",
            ));
        }
        let after_i64 = i64::try_from(after_sequence).map_err(|_| {
            AttentionPolicyError::InvalidRequest("after_sequence exceeds SQLite i64")
        })?;
        let limit_i64 = i64::try_from(limit)
            .map_err(|_| AttentionPolicyError::InvalidRequest("limit exceeds SQLite i64"))?;

        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT
                receipt_seq,
                receipt_id,
                idempotency_key,
                request_fingerprint,
                policy_generation,
                origin_kind,
                origin_key,
                application_id,
                lane,
                coalesce_key,
                requested_at_ms,
                payment_priority_override_attempted,
                outcome,
                queue_target,
                window_start_ms,
                coalesced_count,
                retry_after_ms,
                resume_at_ms
             FROM attention_decision_receipts
             WHERE receipt_seq > ?1
             ORDER BY receipt_seq ASC
             LIMIT ?2",
        )?;
        let rows = statement
            .query_map(params![after_i64, limit_i64], receipt_columns)?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter().map(receipt_from_columns).collect()
    }

    /// Inspects one active-window coalesce counter row for `(origin,
    /// coalesce_key, window_start_ms)`.
    ///
    /// # Errors
    ///
    /// Fails closed on invalid `coalesce_key`, `SQLite` `i64` overflow, or
    /// storage corruption.
    pub fn inspect_coalesce_counter(
        &self,
        origin: AttentionOrigin,
        coalesce_key: &str,
        window_start_ms: u64,
    ) -> Result<Option<CoalesceCounterRecord>, AttentionPolicyError> {
        validate_coalesce_key(coalesce_key)?;
        let window_start_i64 = i64::try_from(window_start_ms).map_err(|_| {
            AttentionPolicyError::InvalidRequest("window_start_ms exceeds SQLite i64")
        })?;
        let connection = self.lock()?;
        load_coalesce_counter(
            &connection,
            &origin.canonical_key(),
            coalesce_key,
            window_start_i64,
        )
    }

    /// Reports current active-window quota usage for
    /// [`AttentionQueueTarget::StandardNotificationLane`] at timestamp `at_ms`.
    ///
    /// # Errors
    ///
    /// Fails closed if `at_ms` overflows `SQLite` `i64` or storage fails.
    pub fn window_usage(
        &self,
        origin: AttentionOrigin,
        at_ms: u64,
    ) -> Result<WindowUsageSnapshot, AttentionPolicyError> {
        let _ = i64::try_from(at_ms)
            .map_err(|_| AttentionPolicyError::InvalidRequest("at_ms exceeds SQLite i64"))?;
        let connection = self.lock()?;
        let (_gen, rate_policy, _quiet) = load_latest_policy(&connection)?;
        let window_start_ms = (at_ms / rate_policy.window_ms) * rate_policy.window_ms;
        let window_end_u128 = u128::from(window_start_ms) + u128::from(rate_policy.window_ms);
        let window_end_ms = u64::try_from(window_end_u128).unwrap_or(u64::MAX);
        let (per_origin_delivered, global_standard_delivered) = query_window_delivery_counts(
            &connection,
            &origin.canonical_key(),
            window_start_ms,
            window_end_u128,
        )?;
        Ok(WindowUsageSnapshot {
            window_start_ms,
            window_end_ms,
            per_origin_delivered,
            global_standard_delivered,
        })
    }
}

fn open_configured_connection(root: &Path) -> Result<Connection, AttentionPolicyError> {
    if !root.to_string_lossy().starts_with("file:") {
        std::fs::create_dir_all(root).map_err(AttentionPolicyError::Io)?;
    }
    let connection = Connection::open(root.join(ATTENTION_POLICY_DB_FILENAME))?;
    connection.busy_timeout(Duration::from_secs(5))?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "FULL")?;
    connection.pragma_update(None, "foreign_keys", "ON")?;

    let journal_mode: String =
        connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
    let synchronous: i64 = connection.pragma_query_value(None, "synchronous", |row| row.get(0))?;
    let foreign_keys: i64 =
        connection.pragma_query_value(None, "foreign_keys", |row| row.get(0))?;
    if !journal_mode.eq_ignore_ascii_case("wal") || synchronous != 2 || foreign_keys != 1 {
        return Err(AttentionPolicyError::DurabilityUnavailable {
            journal_mode,
            synchronous,
            foreign_keys,
        });
    }
    Ok(connection)
}

fn initialize_v1(
    connection: &mut Connection,
    rate_policy: AttentionRatePolicy,
    quiet_hours: QuietHoursPolicy,
) -> Result<(), AttentionPolicyError> {
    let existing_objects: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type IN ('table', 'trigger')
           AND name IN (
               'attention_policy_config',
               'attention_decision_receipts',
               'attention_coalesce_counters',
               'trg_attention_policy_config_no_update',
               'trg_attention_policy_config_no_delete',
               'trg_attention_decision_receipts_no_update',
               'trg_attention_decision_receipts_no_delete',
               'trg_attention_coalesce_counters_monotonic_update',
               'trg_attention_coalesce_counters_no_delete'
           )",
        [],
        |row| row.get(0),
    )?;
    if existing_objects != 0 {
        return Err(AttentionPolicyError::CorruptRecord(
            "partial attention-policy.db schema with user_version=0",
        ));
    }

    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    transaction.execute_batch(INIT_V1_SQL)?;
    insert_policy_row(&transaction, 1, rate_policy, quiet_hours)?;
    transaction.commit()?;
    Ok(())
}

fn verify_v1(connection: &Connection) -> Result<(), AttentionPolicyError> {
    let tables: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='table' AND name IN (
             'attention_policy_config',
             'attention_decision_receipts',
             'attention_coalesce_counters'
         )",
        [],
        |row| row.get(0),
    )?;
    let triggers: i64 = connection.query_row(
        "SELECT COUNT(*) FROM sqlite_master
         WHERE type='trigger' AND name IN (
             'trg_attention_policy_config_no_update',
             'trg_attention_policy_config_no_delete',
             'trg_attention_decision_receipts_no_update',
             'trg_attention_decision_receipts_no_delete',
             'trg_attention_coalesce_counters_monotonic_update',
             'trg_attention_coalesce_counters_no_delete'
         )",
        [],
        |row| row.get(0),
    )?;
    if tables != 3 || triggers != 6 {
        return Err(AttentionPolicyError::CorruptRecord(
            "corrupt or incomplete attention-policy.db v1 schema",
        ));
    }
    Ok(())
}

fn sync_policy_if_changed(
    connection: &mut Connection,
    rate_policy: AttentionRatePolicy,
    quiet_hours: QuietHoursPolicy,
) -> Result<(), AttentionPolicyError> {
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let (current_gen, current_rate, current_quiet) = load_latest_policy(&transaction)?;
    if current_rate != rate_policy || current_quiet != quiet_hours {
        let next_gen = current_gen
            .checked_add(1)
            .ok_or(AttentionPolicyError::InvalidPolicy(
                "policy_generation overflow",
            ))?;
        insert_policy_row(&transaction, next_gen, rate_policy, quiet_hours)?;
        transaction.commit()?;
    }
    Ok(())
}

fn insert_policy_row(
    connection: &Connection,
    policy_generation: u64,
    rate_policy: AttentionRatePolicy,
    quiet_hours: QuietHoursPolicy,
) -> Result<(), AttentionPolicyError> {
    let gen_i64 = i64::try_from(policy_generation)
        .map_err(|_| AttentionPolicyError::InvalidPolicy("policy_generation exceeds SQLite i64"))?;
    let window_i64 = i64::try_from(rate_policy.window_ms)
        .map_err(|_| AttentionPolicyError::InvalidPolicy("window_ms exceeds SQLite i64"))?;
    connection.execute(
        "INSERT INTO attention_policy_config (
            policy_generation,
            max_per_app_per_window,
            max_global_ordinary_per_window,
            window_ms,
            allow_coalescing,
            quiet_enabled,
            quiet_start_minute,
            quiet_end_minute,
            quiet_utc_offset_minutes
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            gen_i64,
            i64::from(rate_policy.max_per_app_per_window),
            i64::from(rate_policy.max_global_ordinary_per_window),
            window_i64,
            i64::from(u8::from(rate_policy.allow_coalescing)),
            i64::from(u8::from(quiet_hours.enabled)),
            i64::from(quiet_hours.start_minute_of_day),
            i64::from(quiet_hours.end_minute_of_day),
            i64::from(quiet_hours.utc_offset_minutes),
        ],
    )?;
    Ok(())
}

fn load_latest_policy(
    connection: &Connection,
) -> Result<(u64, AttentionRatePolicy, QuietHoursPolicy), AttentionPolicyError> {
    let row = connection
        .query_row(
            "SELECT
                policy_generation,
                max_per_app_per_window,
                max_global_ordinary_per_window,
                window_ms,
                allow_coalescing,
                quiet_enabled,
                quiet_start_minute,
                quiet_end_minute,
                quiet_utc_offset_minutes
             FROM attention_policy_config
             ORDER BY policy_generation DESC
             LIMIT 1",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    row.get::<_, i64>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                ))
            },
        )
        .optional()?
        .ok_or(AttentionPolicyError::CorruptRecord(
            "missing attention_policy_config row",
        ))?;

    let (
        gen_raw,
        max_app_raw,
        max_global_raw,
        window_raw,
        coalescing_raw,
        quiet_enabled_raw,
        quiet_start_raw,
        quiet_end_raw,
        quiet_offset_raw,
    ) = row;

    let generation = u64::try_from(gen_raw)
        .map_err(|_| AttentionPolicyError::CorruptRecord("negative policy_generation"))?;
    if generation == 0 {
        return Err(AttentionPolicyError::CorruptRecord(
            "zero policy_generation",
        ));
    }

    let rate = AttentionRatePolicy {
        max_per_app_per_window: u32::try_from(max_app_raw)
            .map_err(|_| AttentionPolicyError::CorruptRecord("invalid max_per_app_per_window"))?,
        max_global_ordinary_per_window: u32::try_from(max_global_raw).map_err(|_| {
            AttentionPolicyError::CorruptRecord("invalid max_global_ordinary_per_window")
        })?,
        window_ms: u64::try_from(window_raw)
            .map_err(|_| AttentionPolicyError::CorruptRecord("negative window_ms"))?,
        allow_coalescing: parse_bool_i64(coalescing_raw, "invalid stored allow_coalescing")?,
    };
    rate.validate()
        .map_err(|_| AttentionPolicyError::CorruptRecord("stored rate policy out of bounds"))?;

    let quiet = QuietHoursPolicy {
        enabled: parse_bool_i64(quiet_enabled_raw, "invalid stored quiet_enabled")?,
        start_minute_of_day: u16::try_from(quiet_start_raw).map_err(|_| {
            AttentionPolicyError::CorruptRecord("invalid stored quiet_start_minute")
        })?,
        end_minute_of_day: u16::try_from(quiet_end_raw)
            .map_err(|_| AttentionPolicyError::CorruptRecord("invalid stored quiet_end_minute"))?,
        utc_offset_minutes: i16::try_from(quiet_offset_raw).map_err(|_| {
            AttentionPolicyError::CorruptRecord("invalid stored quiet_utc_offset_minutes")
        })?,
    };
    quiet
        .validate()
        .map_err(|_| AttentionPolicyError::CorruptRecord("stored quiet hours out of bounds"))?;

    Ok((generation, rate, quiet))
}

fn validate_request(request: &AttentionDeliveryRequest) -> Result<(), AttentionPolicyError> {
    if i64::try_from(request.requested_at_ms).is_err() {
        return Err(AttentionPolicyError::InvalidRequest(
            "requested_at_ms exceeds SQLite i64",
        ));
    }
    if let Some(ref key) = request.coalesce_key {
        validate_coalesce_key(key)?;
    }
    Ok(())
}

fn validate_coalesce_key(key: &str) -> Result<(), AttentionPolicyError> {
    if key.is_empty() {
        return Err(AttentionPolicyError::InvalidRequest(
            "coalesce_key must not be empty",
        ));
    }
    if key.len() > MAX_COALESCE_KEY_LEN {
        return Err(AttentionPolicyError::InvalidRequest(
            "coalesce_key exceeds MAX_COALESCE_KEY_LEN",
        ));
    }
    if key.as_bytes().contains(&0) {
        return Err(AttentionPolicyError::InvalidRequest(
            "coalesce_key must not contain NUL bytes",
        ));
    }
    Ok(())
}

fn compute_request_fingerprint(request: &AttentionDeliveryRequest) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/notify-attention-request/v1");
    hasher.update(request.idempotency_key.as_bytes());
    hasher.update(request.origin.canonical_key());
    hasher.update([request.lane.tag_byte()]);
    match &request.coalesce_key {
        None => hasher.update([0u8]),
        Some(key) => {
            hasher.update([1u8]);
            hasher.update((key.len() as u64).to_be_bytes());
            hasher.update(key.as_bytes());
        }
    }
    hasher.update(request.requested_at_ms.to_be_bytes());
    hasher.update([u8::from(request.payment_priority_override_attempted)]);
    hasher.finalize().into()
}

fn derive_receipt_id(idempotency_key: IdempotencyKey, fingerprint: &[u8; 32]) -> ReceiptId {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/notify-attention-receipt/v1");
    hasher.update(idempotency_key.as_bytes());
    hasher.update(fingerprint);
    ReceiptId::from_bytes(hasher.finalize()[..16].try_into().expect("16 of 32 bytes"))
}

fn build_base_receipt(
    receipt_id: ReceiptId,
    request: &AttentionDeliveryRequest,
    fingerprint: [u8; 32],
    policy_generation: u64,
    outcome: AttentionOutcomeKind,
) -> AttentionReceiptRecord {
    AttentionReceiptRecord {
        receipt_seq: 0,
        receipt_id,
        idempotency_key: request.idempotency_key,
        request_fingerprint: fingerprint,
        policy_generation,
        origin: request.origin,
        lane: request.lane,
        coalesce_key: request.coalesce_key.clone(),
        requested_at_ms: request.requested_at_ms,
        payment_priority_override_attempted: request.payment_priority_override_attempted,
        outcome,
        queue_target: None,
        window_start_ms: None,
        coalesced_count: None,
        retry_after_ms: None,
        resume_at_ms: None,
    }
}

fn replay_existing_receipt(
    existing: &AttentionReceiptRecord,
    request: &AttentionDeliveryRequest,
    fingerprint: &[u8; 32],
) -> Result<AttentionDecision, AttentionPolicyError> {
    if existing.request_fingerprint != *fingerprint {
        return Err(AttentionPolicyError::IdempotencyConflict(
            request.idempotency_key,
        ));
    }
    match existing.outcome {
        AttentionOutcomeKind::Delivered => {
            let queue = existing
                .queue_target
                .ok_or(AttentionPolicyError::CorruptRecord(
                    "DELIVERED receipt missing queue_target",
                ))?;
            Ok(AttentionDecision::Delivered {
                queue,
                receipt_id: existing.receipt_id,
            })
        }
        AttentionOutcomeKind::Coalesced => {
            let coalesce_key =
                existing
                    .coalesce_key
                    .clone()
                    .ok_or(AttentionPolicyError::CorruptRecord(
                        "COALESCED receipt missing coalesce_key",
                    ))?;
            let coalesced_count =
                existing
                    .coalesced_count
                    .ok_or(AttentionPolicyError::CorruptRecord(
                        "COALESCED receipt missing coalesced_count",
                    ))?;
            let window_start_ms =
                existing
                    .window_start_ms
                    .ok_or(AttentionPolicyError::CorruptRecord(
                        "COALESCED receipt missing window_start_ms",
                    ))?;
            Ok(AttentionDecision::Coalesced {
                coalesce_key,
                coalesced_count,
                window_start_ms,
                receipt_id: existing.receipt_id,
            })
        }
        AttentionOutcomeKind::RateLimited => {
            let retry_after_ms =
                existing
                    .retry_after_ms
                    .ok_or(AttentionPolicyError::CorruptRecord(
                        "RATE_LIMITED receipt missing retry_after_ms",
                    ))?;
            Ok(AttentionDecision::RateLimited {
                retry_after_ms,
                receipt_id: existing.receipt_id,
            })
        }
        AttentionOutcomeKind::DeferredQuietHours => {
            let resume_at_ms = existing
                .resume_at_ms
                .ok_or(AttentionPolicyError::CorruptRecord(
                    "DEFERRED_QUIET_HOURS receipt missing resume_at_ms",
                ))?;
            Ok(AttentionDecision::DeferredQuietHours {
                resume_at_ms,
                receipt_id: existing.receipt_id,
            })
        }
        AttentionOutcomeKind::RejectedPaymentBypass => {
            Err(AttentionPolicyError::PaymentBypassForbidden)
        }
        AttentionOutcomeKind::RejectedUntrustedOrigin => {
            Err(AttentionPolicyError::UntrustedConsentOrigin)
        }
    }
}

fn evaluate_standard_lane(
    connection: &Connection,
    request: &AttentionDeliveryRequest,
    fingerprint: [u8; 32],
    policy_generation: u64,
    receipt_id: ReceiptId,
    rate_policy: &AttentionRatePolicy,
    quiet_hours: QuietHoursPolicy,
) -> Result<AttentionDecision, AttentionPolicyError> {
    if let Some(resume_at_ms) = evaluate_quiet_hours(quiet_hours, request.requested_at_ms)? {
        return record_quiet_hours_deferral(
            connection,
            request,
            fingerprint,
            policy_generation,
            receipt_id,
            resume_at_ms,
        );
    }

    let window_start_ms = (request.requested_at_ms / rate_policy.window_ms) * rate_policy.window_ms;
    let window_start_i64 = i64::try_from(window_start_ms)
        .map_err(|_| AttentionPolicyError::InvalidRequest("window_start_ms exceeds SQLite i64"))?;
    let window_end_u128 = u128::from(window_start_ms) + u128::from(rate_policy.window_ms);
    let origin_key = request.origin.canonical_key();

    if rate_policy.allow_coalescing
        && let Some(ref coalesce_key) = request.coalesce_key
        && let Some(decision) = try_coalesce_in_window(
            connection,
            request,
            fingerprint,
            policy_generation,
            receipt_id,
            &origin_key,
            coalesce_key,
            window_start_ms,
            window_start_i64,
        )?
    {
        return Ok(decision);
    }

    let (per_origin_delivered, global_delivered) =
        query_window_delivery_counts(connection, &origin_key, window_start_ms, window_end_u128)?;

    if per_origin_delivered >= u64::from(rate_policy.max_per_app_per_window)
        || global_delivered >= u64::from(rate_policy.max_global_ordinary_per_window)
    {
        return record_rate_limited(
            connection,
            request,
            fingerprint,
            policy_generation,
            receipt_id,
            window_start_ms,
            window_end_u128,
        );
    }

    record_standard_delivery(
        connection,
        request,
        fingerprint,
        policy_generation,
        receipt_id,
        &origin_key,
        window_start_ms,
        window_start_i64,
        rate_policy.allow_coalescing,
    )
}

fn record_quiet_hours_deferral(
    connection: &Connection,
    request: &AttentionDeliveryRequest,
    fingerprint: [u8; 32],
    policy_generation: u64,
    receipt_id: ReceiptId,
    resume_at_ms: u64,
) -> Result<AttentionDecision, AttentionPolicyError> {
    let mut record = build_base_receipt(
        receipt_id,
        request,
        fingerprint,
        policy_generation,
        AttentionOutcomeKind::DeferredQuietHours,
    );
    record.resume_at_ms = Some(resume_at_ms);
    insert_receipt(connection, &record)?;
    Ok(AttentionDecision::DeferredQuietHours {
        resume_at_ms,
        receipt_id,
    })
}

#[allow(clippy::too_many_arguments)]
fn try_coalesce_in_window(
    connection: &Connection,
    request: &AttentionDeliveryRequest,
    fingerprint: [u8; 32],
    policy_generation: u64,
    receipt_id: ReceiptId,
    origin_key: &[u8; 17],
    coalesce_key: &str,
    window_start_ms: u64,
    window_start_i64: i64,
) -> Result<Option<AttentionDecision>, AttentionPolicyError> {
    let Some(existing_counter) =
        load_coalesce_counter(connection, origin_key, coalesce_key, window_start_i64)?
    else {
        return Ok(None);
    };

    let next_count = existing_counter.coalesced_count.checked_add(1).ok_or(
        AttentionPolicyError::CorruptRecord("coalesced_count overflows u64"),
    )?;
    let next_count_i64 = i64::try_from(next_count)
        .map_err(|_| AttentionPolicyError::CorruptRecord("coalesced_count exceeds SQLite i64"))?;
    let requested_at_i64 = i64::try_from(request.requested_at_ms)
        .map_err(|_| AttentionPolicyError::InvalidRequest("requested_at_ms exceeds SQLite i64"))?;

    let mut record = build_base_receipt(
        receipt_id,
        request,
        fingerprint,
        policy_generation,
        AttentionOutcomeKind::Coalesced,
    );
    record.coalesced_count = Some(next_count);
    record.window_start_ms = Some(window_start_ms);
    insert_receipt(connection, &record)?;

    let updated = connection.execute(
        "UPDATE attention_coalesce_counters
         SET coalesced_count = ?1, last_receipt_id = ?2, last_updated_at_ms = ?3
         WHERE origin_key = ?4 AND coalesce_key = ?5 AND window_start_ms = ?6",
        params![
            next_count_i64,
            receipt_id.as_bytes().as_slice(),
            requested_at_i64,
            origin_key.as_slice(),
            coalesce_key,
            window_start_i64,
        ],
    )?;
    if updated != 1 {
        return Err(AttentionPolicyError::CorruptRecord(
            "expected single coalesce counter row update",
        ));
    }

    Ok(Some(AttentionDecision::Coalesced {
        coalesce_key: coalesce_key.to_owned(),
        coalesced_count: next_count,
        window_start_ms,
        receipt_id,
    }))
}

fn record_rate_limited(
    connection: &Connection,
    request: &AttentionDeliveryRequest,
    fingerprint: [u8; 32],
    policy_generation: u64,
    receipt_id: ReceiptId,
    window_start_ms: u64,
    window_end_u128: u128,
) -> Result<AttentionDecision, AttentionPolicyError> {
    let retry_after_u128 = window_end_u128 - u128::from(request.requested_at_ms);
    let retry_after_ms = u64::try_from(retry_after_u128)
        .map_err(|_| AttentionPolicyError::InvalidRequest("retry_after_ms overflows u64"))?;
    if i64::try_from(retry_after_ms).is_err() {
        return Err(AttentionPolicyError::InvalidRequest(
            "retry_after_ms exceeds SQLite i64",
        ));
    }
    let mut record = build_base_receipt(
        receipt_id,
        request,
        fingerprint,
        policy_generation,
        AttentionOutcomeKind::RateLimited,
    );
    record.retry_after_ms = Some(retry_after_ms);
    record.window_start_ms = Some(window_start_ms);
    insert_receipt(connection, &record)?;
    Ok(AttentionDecision::RateLimited {
        retry_after_ms,
        receipt_id,
    })
}

#[allow(clippy::too_many_arguments)]
fn record_standard_delivery(
    connection: &Connection,
    request: &AttentionDeliveryRequest,
    fingerprint: [u8; 32],
    policy_generation: u64,
    receipt_id: ReceiptId,
    origin_key: &[u8; 17],
    window_start_ms: u64,
    window_start_i64: i64,
    allow_coalescing: bool,
) -> Result<AttentionDecision, AttentionPolicyError> {
    let mut record = build_base_receipt(
        receipt_id,
        request,
        fingerprint,
        policy_generation,
        AttentionOutcomeKind::Delivered,
    );
    record.queue_target = Some(AttentionQueueTarget::StandardNotificationLane);
    record.window_start_ms = Some(window_start_ms);
    insert_receipt(connection, &record)?;

    if allow_coalescing && let Some(ref coalesce_key) = request.coalesce_key {
        let requested_at_i64 = i64::try_from(request.requested_at_ms).map_err(|_| {
            AttentionPolicyError::InvalidRequest("requested_at_ms exceeds SQLite i64")
        })?;
        let app_bytes = request
            .origin
            .application_id()
            .map(ApplicationId::into_bytes);
        connection.execute(
            "INSERT INTO attention_coalesce_counters (
                origin_key,
                origin_kind,
                application_id,
                coalesce_key,
                window_start_ms,
                delivered_count,
                coalesced_count,
                initial_receipt_id,
                last_receipt_id,
                last_updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, 1, 0, ?6, ?6, ?7)",
            params![
                origin_key.as_slice(),
                request.origin.db_kind(),
                app_bytes.as_ref().map(<[u8; 16]>::as_slice),
                coalesce_key.as_str(),
                window_start_i64,
                receipt_id.as_bytes().as_slice(),
                requested_at_i64,
            ],
        )?;
    }

    Ok(AttentionDecision::Delivered {
        queue: AttentionQueueTarget::StandardNotificationLane,
        receipt_id,
    })
}

/// Evaluates whether `requested_at_ms` falls within `[start, end)` in local
/// time (`utc_offset_minutes`), returning `Some(resume_at_ms)` when deferred.
fn evaluate_quiet_hours(
    policy: QuietHoursPolicy,
    requested_at_ms: u64,
) -> Result<Option<u64>, AttentionPolicyError> {
    if !policy.enabled {
        return Ok(None);
    }

    let offset_ms = i128::from(policy.utc_offset_minutes) * MS_PER_MINUTE;
    let local_ms = i128::from(requested_at_ms) + offset_ms;
    let local_day_index = local_ms.div_euclid(MS_PER_DAY);
    let ms_of_day = local_ms.rem_euclid(MS_PER_DAY);

    let start_ms = i128::from(policy.start_minute_of_day) * MS_PER_MINUTE;
    let end_ms = i128::from(policy.end_minute_of_day) * MS_PER_MINUTE;

    let local_resume_ms = if policy.start_minute_of_day < policy.end_minute_of_day {
        if ms_of_day >= start_ms && ms_of_day < end_ms {
            local_day_index * MS_PER_DAY + end_ms
        } else {
            return Ok(None);
        }
    } else if ms_of_day >= start_ms {
        (local_day_index + 1) * MS_PER_DAY + end_ms
    } else if ms_of_day < end_ms {
        local_day_index * MS_PER_DAY + end_ms
    } else {
        return Ok(None);
    };

    let utc_resume_ms = local_resume_ms - offset_ms;
    let resume_u64 = u64::try_from(utc_resume_ms).map_err(|_| {
        AttentionPolicyError::InvalidRequest("quiet hours resume_at_ms underflows u64")
    })?;
    if i64::try_from(resume_u64).is_err() {
        return Err(AttentionPolicyError::InvalidRequest(
            "quiet hours resume_at_ms exceeds SQLite i64",
        ));
    }
    Ok(Some(resume_u64))
}

fn query_window_delivery_counts(
    connection: &Connection,
    origin_key: &[u8; 17],
    window_start_ms: u64,
    window_end_u128: u128,
) -> Result<(u64, u64), AttentionPolicyError> {
    let start_i64 = i64::try_from(window_start_ms)
        .map_err(|_| AttentionPolicyError::InvalidRequest("window_start_ms exceeds SQLite i64"))?;
    let end_i64 = i64::try_from(window_end_u128).unwrap_or(i64::MAX);
    let end_inclusive = i64::try_from(window_end_u128).is_err();

    let per_origin_raw: i64 = if end_inclusive {
        connection.query_row(
            "SELECT COUNT(*)
             FROM attention_decision_receipts
             WHERE outcome = 'DELIVERED'
               AND queue_target = 'STANDARD_NOTIFICATION_LANE'
               AND origin_key = ?1
               AND requested_at_ms >= ?2
               AND requested_at_ms <= ?3",
            params![origin_key.as_slice(), start_i64, end_i64],
            |row| row.get(0),
        )?
    } else {
        connection.query_row(
            "SELECT COUNT(*)
             FROM attention_decision_receipts
             WHERE outcome = 'DELIVERED'
               AND queue_target = 'STANDARD_NOTIFICATION_LANE'
               AND origin_key = ?1
               AND requested_at_ms >= ?2
               AND requested_at_ms < ?3",
            params![origin_key.as_slice(), start_i64, end_i64],
            |row| row.get(0),
        )?
    };

    let global_raw: i64 = if end_inclusive {
        connection.query_row(
            "SELECT COUNT(*)
             FROM attention_decision_receipts
             WHERE outcome = 'DELIVERED'
               AND queue_target = 'STANDARD_NOTIFICATION_LANE'
               AND requested_at_ms >= ?1
               AND requested_at_ms <= ?2",
            params![start_i64, end_i64],
            |row| row.get(0),
        )?
    } else {
        connection.query_row(
            "SELECT COUNT(*)
             FROM attention_decision_receipts
             WHERE outcome = 'DELIVERED'
               AND queue_target = 'STANDARD_NOTIFICATION_LANE'
               AND requested_at_ms >= ?1
               AND requested_at_ms < ?2",
            params![start_i64, end_i64],
            |row| row.get(0),
        )?
    };

    let per_origin = u64::try_from(per_origin_raw)
        .map_err(|_| AttentionPolicyError::CorruptRecord("negative per_origin count"))?;
    let global = u64::try_from(global_raw)
        .map_err(|_| AttentionPolicyError::CorruptRecord("negative global count"))?;
    Ok((per_origin, global))
}

fn insert_receipt(
    connection: &Connection,
    record: &AttentionReceiptRecord,
) -> Result<(), AttentionPolicyError> {
    let policy_gen_i64 = i64::try_from(record.policy_generation)
        .map_err(|_| AttentionPolicyError::CorruptRecord("policy_generation exceeds SQLite i64"))?;
    let requested_at_i64 = i64::try_from(record.requested_at_ms)
        .map_err(|_| AttentionPolicyError::InvalidRequest("requested_at_ms exceeds SQLite i64"))?;
    let origin_key = record.origin.canonical_key();
    let app_bytes = record
        .origin
        .application_id()
        .map(ApplicationId::into_bytes);

    let window_start_i64 = record
        .window_start_ms
        .map(i64::try_from)
        .transpose()
        .map_err(|_| AttentionPolicyError::InvalidRequest("window_start_ms exceeds SQLite i64"))?;
    let coalesced_count_i64 = record
        .coalesced_count
        .map(i64::try_from)
        .transpose()
        .map_err(|_| AttentionPolicyError::CorruptRecord("coalesced_count exceeds SQLite i64"))?;
    let retry_after_i64 = record
        .retry_after_ms
        .map(i64::try_from)
        .transpose()
        .map_err(|_| AttentionPolicyError::InvalidRequest("retry_after_ms exceeds SQLite i64"))?;
    let resume_at_i64 = record
        .resume_at_ms
        .map(i64::try_from)
        .transpose()
        .map_err(|_| AttentionPolicyError::InvalidRequest("resume_at_ms exceeds SQLite i64"))?;

    connection.execute(
        "INSERT INTO attention_decision_receipts (
            receipt_id,
            idempotency_key,
            request_fingerprint,
            policy_generation,
            origin_kind,
            origin_key,
            application_id,
            lane,
            coalesce_key,
            requested_at_ms,
            payment_priority_override_attempted,
            outcome,
            queue_target,
            window_start_ms,
            coalesced_count,
            retry_after_ms,
            resume_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
        params![
            record.receipt_id.as_bytes().as_slice(),
            record.idempotency_key.as_bytes().as_slice(),
            record.request_fingerprint.as_slice(),
            policy_gen_i64,
            record.origin.db_kind(),
            origin_key.as_slice(),
            app_bytes.as_ref().map(<[u8; 16]>::as_slice),
            record.lane.as_db_str(),
            record.coalesce_key.as_deref(),
            requested_at_i64,
            i64::from(u8::from(record.payment_priority_override_attempted)),
            record.outcome.as_db_str(),
            record.queue_target.map(AttentionQueueTarget::as_db_str),
            window_start_i64,
            coalesced_count_i64,
            retry_after_i64,
            resume_at_i64,
        ],
    )?;
    Ok(())
}

type ReceiptColumns = (
    i64,
    Vec<u8>,
    Vec<u8>,
    Vec<u8>,
    i64,
    String,
    Vec<u8>,
    Option<Vec<u8>>,
    String,
    Option<String>,
    i64,
    i64,
    String,
    Option<String>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<i64>,
);

fn receipt_columns(row: &rusqlite::Row<'_>) -> rusqlite::Result<ReceiptColumns> {
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
        row.get(14)?,
        row.get(15)?,
        row.get(16)?,
        row.get(17)?,
    ))
}

fn receipt_from_columns(
    columns: ReceiptColumns,
) -> Result<AttentionReceiptRecord, AttentionPolicyError> {
    let (
        receipt_seq_raw,
        receipt_id_bytes,
        idem_bytes,
        fingerprint_bytes,
        policy_gen_raw,
        origin_kind,
        origin_key_bytes,
        app_id_bytes,
        lane_raw,
        coalesce_key,
        requested_at_raw,
        payment_override_raw,
        outcome_raw,
        queue_target_raw,
        window_start_raw,
        coalesced_count_raw,
        retry_after_raw,
        resume_at_raw,
    ) = columns;

    let origin = parse_stored_origin(&origin_kind, &origin_key_bytes, app_id_bytes)?;
    let queue_target = queue_target_raw
        .as_deref()
        .map(AttentionQueueTarget::from_db_str)
        .transpose()?;

    Ok(AttentionReceiptRecord {
        receipt_seq: parse_non_negative_u64(receipt_seq_raw, "negative receipt_seq")?,
        receipt_id: ReceiptId::from_bytes(array16(
            receipt_id_bytes,
            "receipt_id length is not 16",
        )?),
        idempotency_key: IdempotencyKey::from_bytes(array16(
            idem_bytes,
            "idempotency_key length is not 16",
        )?),
        request_fingerprint: array32(fingerprint_bytes, "request_fingerprint length is not 32")?,
        policy_generation: parse_non_negative_u64(policy_gen_raw, "negative policy_generation")?,
        origin,
        lane: AttentionLaneClass::from_db_str(&lane_raw)?,
        coalesce_key,
        requested_at_ms: parse_non_negative_u64(requested_at_raw, "negative requested_at_ms")?,
        payment_priority_override_attempted: parse_bool_i64(
            payment_override_raw,
            "invalid payment_priority_override_attempted",
        )?,
        outcome: AttentionOutcomeKind::from_db_str(&outcome_raw)?,
        queue_target,
        window_start_ms: window_start_raw
            .map(|value| parse_non_negative_u64(value, "negative window_start_ms"))
            .transpose()?,
        coalesced_count: coalesced_count_raw
            .map(|value| parse_non_negative_u64(value, "negative coalesced_count"))
            .transpose()?,
        retry_after_ms: retry_after_raw
            .map(|value| parse_non_negative_u64(value, "negative retry_after_ms"))
            .transpose()?,
        resume_at_ms: resume_at_raw
            .map(|value| parse_non_negative_u64(value, "negative resume_at_ms"))
            .transpose()?,
    })
}

fn load_receipt_by_key(
    connection: &Connection,
    idempotency_key: IdempotencyKey,
) -> Result<Option<AttentionReceiptRecord>, AttentionPolicyError> {
    connection
        .query_row(
            "SELECT
                receipt_seq,
                receipt_id,
                idempotency_key,
                request_fingerprint,
                policy_generation,
                origin_kind,
                origin_key,
                application_id,
                lane,
                coalesce_key,
                requested_at_ms,
                payment_priority_override_attempted,
                outcome,
                queue_target,
                window_start_ms,
                coalesced_count,
                retry_after_ms,
                resume_at_ms
             FROM attention_decision_receipts
             WHERE idempotency_key = ?1",
            params![idempotency_key.as_bytes().as_slice()],
            receipt_columns,
        )
        .optional()?
        .map(receipt_from_columns)
        .transpose()
}

fn load_receipt_by_id(
    connection: &Connection,
    receipt_id: ReceiptId,
) -> Result<Option<AttentionReceiptRecord>, AttentionPolicyError> {
    connection
        .query_row(
            "SELECT
                receipt_seq,
                receipt_id,
                idempotency_key,
                request_fingerprint,
                policy_generation,
                origin_kind,
                origin_key,
                application_id,
                lane,
                coalesce_key,
                requested_at_ms,
                payment_priority_override_attempted,
                outcome,
                queue_target,
                window_start_ms,
                coalesced_count,
                retry_after_ms,
                resume_at_ms
             FROM attention_decision_receipts
             WHERE receipt_id = ?1",
            params![receipt_id.as_bytes().as_slice()],
            receipt_columns,
        )
        .optional()?
        .map(receipt_from_columns)
        .transpose()
}

type CoalesceColumns = (
    Vec<u8>,
    String,
    Option<Vec<u8>>,
    String,
    i64,
    i64,
    i64,
    Vec<u8>,
    Vec<u8>,
    i64,
);

fn coalesce_columns(row: &rusqlite::Row<'_>) -> rusqlite::Result<CoalesceColumns> {
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
    ))
}

fn load_coalesce_counter(
    connection: &Connection,
    origin_key: &[u8; 17],
    coalesce_key: &str,
    window_start_i64: i64,
) -> Result<Option<CoalesceCounterRecord>, AttentionPolicyError> {
    connection
        .query_row(
            "SELECT
                origin_key,
                origin_kind,
                application_id,
                coalesce_key,
                window_start_ms,
                delivered_count,
                coalesced_count,
                initial_receipt_id,
                last_receipt_id,
                last_updated_at_ms
             FROM attention_coalesce_counters
             WHERE origin_key = ?1
               AND coalesce_key = ?2
               AND window_start_ms = ?3",
            params![origin_key.as_slice(), coalesce_key, window_start_i64],
            coalesce_columns,
        )
        .optional()?
        .map(coalesce_from_columns)
        .transpose()
}

fn coalesce_from_columns(
    columns: CoalesceColumns,
) -> Result<CoalesceCounterRecord, AttentionPolicyError> {
    let (
        origin_key_bytes,
        origin_kind,
        app_id_bytes,
        coalesce_key,
        window_start_raw,
        delivered_count_raw,
        coalesced_count_raw,
        initial_receipt_bytes,
        last_receipt_bytes,
        last_updated_raw,
    ) = columns;

    let origin = parse_stored_origin(&origin_kind, &origin_key_bytes, app_id_bytes)?;
    Ok(CoalesceCounterRecord {
        origin,
        coalesce_key,
        window_start_ms: parse_non_negative_u64(window_start_raw, "negative window_start_ms")?,
        delivered_count: parse_non_negative_u64(delivered_count_raw, "negative delivered_count")?,
        coalesced_count: parse_non_negative_u64(coalesced_count_raw, "negative coalesced_count")?,
        initial_receipt_id: ReceiptId::from_bytes(array16(
            initial_receipt_bytes,
            "initial_receipt_id length is not 16",
        )?),
        last_receipt_id: ReceiptId::from_bytes(array16(
            last_receipt_bytes,
            "last_receipt_id length is not 16",
        )?),
        last_updated_at_ms: parse_non_negative_u64(
            last_updated_raw,
            "negative last_updated_at_ms",
        )?,
    })
}

fn parse_stored_origin(
    origin_kind: &str,
    origin_key_bytes: &[u8],
    app_id_bytes: Option<Vec<u8>>,
) -> Result<AttentionOrigin, AttentionPolicyError> {
    let origin = match origin_kind {
        "TRUSTED_SYSTEM_UI" => {
            if app_id_bytes.is_some() {
                return Err(AttentionPolicyError::CorruptRecord(
                    "TRUSTED_SYSTEM_UI row carries application_id",
                ));
            }
            AttentionOrigin::TrustedSystemUi
        }
        "APPLICATION" => {
            let bytes = app_id_bytes.ok_or(AttentionPolicyError::CorruptRecord(
                "APPLICATION row missing application_id",
            ))?;
            AttentionOrigin::Application(ApplicationId::from_bytes(array16(
                bytes,
                "application_id length is not 16",
            )?))
        }
        _ => {
            return Err(AttentionPolicyError::CorruptRecord(
                "unknown stored origin_kind",
            ));
        }
    };
    if origin_key_bytes != origin.canonical_key() {
        return Err(AttentionPolicyError::CorruptRecord(
            "stored origin_key does not match origin_kind/application_id",
        ));
    }
    Ok(origin)
}

fn parse_bool_i64(value: i64, reason: &'static str) -> Result<bool, AttentionPolicyError> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(AttentionPolicyError::CorruptRecord(reason)),
    }
}

fn parse_non_negative_u64(value: i64, reason: &'static str) -> Result<u64, AttentionPolicyError> {
    u64::try_from(value).map_err(|_| AttentionPolicyError::CorruptRecord(reason))
}

fn array16(bytes: Vec<u8>, reason: &'static str) -> Result<[u8; 16], AttentionPolicyError> {
    bytes
        .try_into()
        .map_err(|_| AttentionPolicyError::CorruptRecord(reason))
}

fn array32(bytes: Vec<u8>, reason: &'static str) -> Result<[u8; 32], AttentionPolicyError> {
    bytes
        .try_into()
        .map_err(|_| AttentionPolicyError::CorruptRecord(reason))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_and_quiet_policy_validation_rejects_invalid_bounds() {
        let valid_rate = AttentionRatePolicy {
            max_per_app_per_window: 3,
            max_global_ordinary_per_window: 10,
            window_ms: 60_000,
            allow_coalescing: true,
        };
        assert!(valid_rate.validate().is_ok());

        for bad in [
            AttentionRatePolicy {
                max_per_app_per_window: 0,
                ..valid_rate
            },
            AttentionRatePolicy {
                max_global_ordinary_per_window: 0,
                ..valid_rate
            },
            AttentionRatePolicy {
                window_ms: 0,
                ..valid_rate
            },
            AttentionRatePolicy {
                window_ms: u64::MAX,
                ..valid_rate
            },
        ] {
            assert!(matches!(
                bad.validate(),
                Err(AttentionPolicyError::InvalidPolicy(_))
            ));
        }

        let valid_quiet = QuietHoursPolicy {
            enabled: true,
            start_minute_of_day: 22 * 60,
            end_minute_of_day: 7 * 60,
            utc_offset_minutes: 480,
        };
        assert!(valid_quiet.validate().is_ok());

        for bad in [
            QuietHoursPolicy {
                start_minute_of_day: 1_440,
                ..valid_quiet
            },
            QuietHoursPolicy {
                end_minute_of_day: 1_440,
                ..valid_quiet
            },
            QuietHoursPolicy {
                utc_offset_minutes: -721,
                ..valid_quiet
            },
            QuietHoursPolicy {
                utc_offset_minutes: 841,
                ..valid_quiet
            },
            QuietHoursPolicy {
                start_minute_of_day: 600,
                end_minute_of_day: 600,
                ..valid_quiet
            },
        ] {
            assert!(matches!(
                bad.validate(),
                Err(AttentionPolicyError::InvalidPolicy(_))
            ));
        }

        let disabled_equal = QuietHoursPolicy {
            enabled: false,
            start_minute_of_day: 0,
            end_minute_of_day: 0,
            utc_offset_minutes: 0,
        };
        assert!(disabled_equal.validate().is_ok());
    }

    #[test]
    fn quiet_hours_handles_cross_midnight_and_negative_utc_offsets() {
        let policy = QuietHoursPolicy {
            enabled: true,
            start_minute_of_day: 22 * 60,
            end_minute_of_day: 7 * 60,
            utc_offset_minutes: -300,
        };
        // 1970-01-01T02:59:59.999Z => 1969-12-31 21:59:59.999 -05:00 (not quiet)
        let before_start = (2 * 3_600 + 59 * 60 + 59) * 1_000 + 999;
        assert_eq!(evaluate_quiet_hours(policy, before_start).unwrap(), None);

        // 1970-01-01T03:00:00.000Z => 1969-12-31 22:00:00.000 -05:00 (quiet until 12:00:00Z)
        let exact_start = 3 * 3_600 * 1_000;
        let expected_resume = 12 * 3_600 * 1_000;
        assert_eq!(
            evaluate_quiet_hours(policy, exact_start).unwrap(),
            Some(expected_resume)
        );
        assert_eq!(evaluate_quiet_hours(policy, expected_resume).unwrap(), None);
    }
}
