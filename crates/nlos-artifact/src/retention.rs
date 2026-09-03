//! Retention policy minimal prefix (B-ARTIFACT-005): a per-artifact time
//! upper bound (`retention_ms`) with an explicit set/inspect surface and a
//! fail-closed expiry gate on the read and content-admission paths.
//!
//! # Semantics: an upper bound, not a guarantee
//!
//! `retention_ms` is the *maximum* time an artifact stays readable, counted
//! from its durable `created_at_ms` anchor: the artifact is readable while
//! `now_ms <= created_at_ms + retention_ms` and **expired** strictly after.
//! Past the deadline, bytes are refused with
//! [`ArtifactError::RetentionExpired`] — a distinct typed error, so a caller
//! can tell "expired" from "not found" and neither state can be silently
//! confused with the other. The bound never renews and never extends on
//! write activity (no TTL-renewal engine in this prefix); a longer window
//! requires an explicit new [`ArtifactStore::set_retention`] call.
//!
//! The time source is always caller-supplied (`now_ms`), matching the
//! crate-wide `created_at_ms` discipline; this crate introduces no clock
//! dependency.
//!
//! # Relationship to the GC philosophy (b-artifact-004)
//!
//! The GC prefix fixed a fail-safe direction: `collect_orphan_blobs`
//! deletes only *provable orphans* — blobs no durable row references —
//! because "prefer retaining over wrongly deleting" (宁可保留不可误删).
//! Retention expiry deliberately does **not** change that reference set:
//!
//! 1. An expired artifact's revisions are still committed, immutable
//!    metadata rows; their blobs are referenced and mechanically not
//!    orphans. GC code needs no retention awareness at all.
//! 2. Deleting on expiry would be an irreversible policy action taken
//!    silently; refusing reads is reversible (extend or clear the policy)
//!    and never destroys evidence. The minimal honest prefix therefore
//!    chooses **mark-by-refusal** over deletion: expiry affects
//!    *readability* only, never *existence*.
//! 3. Physical reclamation of expired artifacts (a dedicated retention-GC
//!    with its own receipts, distinct from `collect_orphan_blobs`' orphan
//!    semantics) is registered as follow-up work, not smuggled into this
//!    prefix.
//!
//! The metadata plane stays deliberately un-gated: `inspect_artifact`,
//! `inspect_revision`, `list_revisions`, `recover`, and package
//! verification read rows, never bytes. An operator (and a future
//! retention-GC) must be able to *see* an expired artifact to make an
//! explicit reclamation decision — hiding metadata would make that
//! impossible.
//!
//! # Gate placement
//!
//! - **Byte reads**: [`ArtifactStore::get_revision`] and the poll surface
//!   [`ArtifactStore::resolve_head`] reject expired artifacts at the
//!   caller-supplied `now_ms`, before any revision or head state is
//!   returned.
//! - **Content admission**: a fresh revision insert (`put_revision`), a
//!   fresh stage (`stage_revision`), and a fresh publication
//!   (`publish_staged_revision`) are rejected when the artifact is expired
//!   at the request's own caller-supplied timestamp. An asymmetric gate
//!   (staging-only) would be trivially bypassed through `put_revision`
//!   and would manufacture writes that can never be read back.
//! - **Replays are never gated**: `Replayed` decisions return
//!   already-committed durable facts and create no new state (the
//!   replay-is-durable-authoritative precedent of B-ARTIFACT-003/004).
//!
//! # Storage and scope
//!
//! `retention_ms` is a nullable column on the durable `artifacts` row
//! (schema v6): `NULL` = unbounded, the state of every artifact that never
//! had a policy set. Setting is durable and idempotent — re-setting the
//! identical value replays [`SetRetentionDecision::Replayed`]; a different
//! value updates the bound and takes effect immediately (a shrink can make
//! previously readable data expired, which is the honest reading of "the
//! current upper bound").

use nlos_types::ArtifactId;
use rusqlite::{TransactionBehavior, params};

use crate::ArtifactError;
use crate::model::ArtifactRecord;
use crate::query::load_artifact_optional;
use crate::store::{ArtifactStore, encode_u64};

/// Request to set the retention time upper bound of one artifact.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SetRetentionRequest {
    pub artifact_id: ArtifactId,
    /// Maximum readability window in milliseconds, counted from the
    /// artifact's durable `created_at_ms`. Must fit a `SQLite` INTEGER.
    pub retention_ms: u64,
}

/// Durable retention policy state of one artifact.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RetentionRecord {
    pub artifact_id: ArtifactId,
    /// The stored upper bound; `None` = unbounded (no policy set).
    pub retention_ms: Option<u64>,
    /// Absolute deadline `created_at_ms + retention_ms` (saturating);
    /// `None` = unbounded. Readable while `now_ms <= expires_at_ms`.
    pub expires_at_ms: Option<u64>,
    /// The anchor instant of the bound: the artifact's durable creation
    /// timestamp. Retention never renews; this anchor is fixed.
    pub created_at_ms: u64,
}

/// Outcome of [`ArtifactStore::set_retention`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SetRetentionDecision {
    /// The stored bound changed to the requested value (including from
    /// absent).
    Updated(RetentionRecord),
    /// The identical bound was already stored; nothing changed and the
    /// stored record is replayed.
    Replayed(RetentionRecord),
}

impl SetRetentionDecision {
    #[must_use]
    pub const fn record(&self) -> &RetentionRecord {
        match self {
            Self::Updated(record) | Self::Replayed(record) => record,
        }
    }
}

/// Fail-closed expiry gate shared by every gated path: an artifact past
/// its upper bound at `now_ms` refuses reads and content admission.
/// Unbounded artifacts (`retention_ms = NULL`) pass at any time.
pub(crate) fn ensure_readable(artifact: &ArtifactRecord, now_ms: u64) -> Result<(), ArtifactError> {
    let Some(retention_ms) = artifact.retention_ms else {
        return Ok(());
    };
    let expires_at_ms = artifact.created_at_ms.saturating_add(retention_ms);
    if now_ms > expires_at_ms {
        Err(ArtifactError::RetentionExpired {
            artifact_id: artifact.artifact_id,
            expires_at_ms,
        })
    } else {
        Ok(())
    }
}

fn retention_record(artifact: &ArtifactRecord) -> RetentionRecord {
    RetentionRecord {
        artifact_id: artifact.artifact_id,
        retention_ms: artifact.retention_ms,
        expires_at_ms: artifact
            .retention_ms
            .map(|retention_ms| artifact.created_at_ms.saturating_add(retention_ms)),
        created_at_ms: artifact.created_at_ms,
    }
}

impl ArtifactStore {
    /// Sets the retention time upper bound of one artifact, durably and
    /// idempotently.
    ///
    /// Repeating the identical `(artifact_id, retention_ms)` replays
    /// [`SetRetentionDecision::Replayed`] with the stored record. A
    /// different value updates the bound immediately: an extension makes
    /// expired data readable again, a shrink can make readable data
    /// expire. There is no per-call idempotency key — this is a pure
    /// current-state assignment, and the stored value itself is the
    /// deduplicated durable state.
    ///
    /// The metadata plane (`inspect_retention` and every other
    /// `inspect_*`/list/recover path) is never gated by expiry; see the
    /// module docs for the audit-plane rationale.
    ///
    /// # Errors
    ///
    /// Returns [`ArtifactError::ArtifactNotFound`] for an unknown artifact,
    /// [`ArtifactError::InvalidSpec`] when `retention_ms` exceeds a `SQLite`
    /// INTEGER, or a storage error.
    pub fn set_retention(
        &self,
        request: SetRetentionRequest,
    ) -> Result<SetRetentionDecision, ArtifactError> {
        let retention_ms = encode_u64(request.retention_ms)?;
        let mut connection = self.lock_connection()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let artifact = load_artifact_optional(&transaction, request.artifact_id)?
            .ok_or(ArtifactError::ArtifactNotFound(request.artifact_id))?;
        if artifact.retention_ms == Some(request.retention_ms) {
            transaction.commit()?;
            return Ok(SetRetentionDecision::Replayed(retention_record(&artifact)));
        }
        transaction.execute(
            "UPDATE artifacts SET retention_ms = ?1 WHERE artifact_id = ?2",
            params![retention_ms, request.artifact_id.as_bytes().as_slice()],
        )?;
        transaction.commit()?;
        Ok(SetRetentionDecision::Updated(RetentionRecord {
            artifact_id: request.artifact_id,
            retention_ms: Some(request.retention_ms),
            expires_at_ms: Some(artifact.created_at_ms.saturating_add(request.retention_ms)),
            created_at_ms: artifact.created_at_ms,
        }))
    }

    /// Reads the durable retention policy state of one artifact. Never
    /// gated by expiry (audit plane).
    ///
    /// # Errors
    ///
    /// Returns [`ArtifactError::ArtifactNotFound`] or a storage error.
    pub fn inspect_retention(
        &self,
        artifact_id: ArtifactId,
    ) -> Result<RetentionRecord, ArtifactError> {
        let connection = self.lock_connection()?;
        load_artifact_optional(&*connection, artifact_id)?
            .ok_or(ArtifactError::ArtifactNotFound(artifact_id))
            .map(|artifact| retention_record(&artifact))
    }
}
