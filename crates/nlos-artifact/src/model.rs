//! Public value types of the artifact store: digests, specs, records,
//! decisions, and the recovery report.

use std::fmt;
use std::path::PathBuf;

use nlos_types::{ApplicationId, ArtifactId, CommitPermitId, IdempotencyKey, ReceiptId, TaskId};
use sha2::{Digest, Sha256};

/// Upper bound for caller-supplied bounded strings (content type, owner,
/// cache key). Mirrors the bounded-string discipline of `nlos-store`.
pub(crate) const MAX_TEXT_COMPONENT_BYTES: usize = 255;

/// Content digest addressing blob bytes.
///
/// The current algorithm is SHA-256 as a stage-B placeholder; the digest is
/// stored as opaque 32 bytes so a future algorithm agility slice can version
/// the envelope without changing this crate's schema layout.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ContentDigest([u8; 32]);

impl ContentDigest {
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn into_bytes(self) -> [u8; 32] {
        self.0
    }

    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Hashes `bytes` with the placeholder SHA-256 algorithm.
    #[must_use]
    pub fn of_bytes(bytes: &[u8]) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        Self(hasher.finalize().into())
    }

    /// Lowercase hex representation used for blob file names.
    #[must_use]
    pub fn to_hex(self) -> String {
        hex_encode(&self.0)
    }

    /// Parses a 64-character lowercase hex blob name back into a digest.
    #[must_use]
    pub fn from_hex(text: &str) -> Option<Self> {
        if text.len() != 64 {
            return None;
        }
        let mut bytes = [0_u8; 32];
        for (index, slot) in bytes.iter_mut().enumerate() {
            *slot = u8::from_str_radix(text.get(index * 2..index * 2 + 2)?, 16).ok()?;
        }
        Some(Self(bytes))
    }
}

impl fmt::Debug for ContentDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "ContentDigest({})", self.to_hex())
    }
}

impl fmt::Display for ContentDigest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_hex())
    }
}

pub(crate) fn hex_encode(value: &[u8]) -> String {
    use fmt::Write as _;
    value
        .iter()
        .fold(String::with_capacity(value.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// Authority-derived identity of one staged revision.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct StagingId([u8; 16]);

impl StagingId {
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

impl fmt::Debug for StagingId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "StagingId({})", hex_encode(&self.0))
    }
}

/// Durable lifecycle state of a staged revision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StagedRevisionState {
    Staged,
    Published,
}

/// Request to durably stage bytes without advancing the artifact head.
#[derive(Clone, Copy, Debug)]
pub struct StageRevisionRequest<'a> {
    pub artifact_id: ArtifactId,
    pub expected_head_revision: u64,
    pub bytes: &'a [u8],
    pub task_id: TaskId,
    pub permit_id: CommitPermitId,
    pub write_set_root: ContentDigest,
    pub idempotency_key: IdempotencyKey,
    pub created_at_ms: u64,
}

/// Durable staged revision metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StagedRevisionRecord {
    pub staging_id: StagingId,
    pub artifact_id: ArtifactId,
    pub expected_head_revision: u64,
    pub target_revision: u64,
    pub digest: ContentDigest,
    pub size_bytes: u64,
    pub task_id: TaskId,
    pub permit_id: CommitPermitId,
    pub write_set_root: ContentDigest,
    pub state: StagedRevisionState,
    pub publication_receipt_id: Option<ReceiptId>,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
}

/// Outcome of [`crate::ArtifactStore::stage_revision`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StageRevisionDecision {
    Staged(StagedRevisionRecord),
    Replayed(StagedRevisionRecord),
}

impl StageRevisionDecision {
    #[must_use]
    pub const fn record(&self) -> &StagedRevisionRecord {
        match self {
            Self::Staged(record) | Self::Replayed(record) => record,
        }
    }
}

/// Request to publish one staged revision under its original task binding.
#[derive(Clone, Copy, Debug)]
pub struct PublishStagedRevisionRequest {
    pub staging_id: StagingId,
    pub task_id: TaskId,
    pub permit_id: CommitPermitId,
    pub write_set_root: ContentDigest,
    pub published_at_ms: u64,
}

/// Immutable proof that `ArtifactAuthority` published one staged revision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactPublicationReceipt {
    pub receipt_id: ReceiptId,
    pub staging_id: StagingId,
    pub artifact_id: ArtifactId,
    pub revision: u64,
    pub digest: ContentDigest,
    pub size_bytes: u64,
    pub task_id: TaskId,
    pub permit_id: CommitPermitId,
    pub write_set_root: ContentDigest,
    pub prior_head_revision: u64,
    pub prior_head_digest: Option<ContentDigest>,
    pub new_head_revision: u64,
    pub new_head_digest: ContentDigest,
    pub created_at_ms: u64,
}

/// Outcome of [`crate::ArtifactStore::publish_staged_revision`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PublishStagedRevisionDecision {
    Published(ArtifactPublicationReceipt),
    Replayed(ArtifactPublicationReceipt),
}

impl PublishStagedRevisionDecision {
    #[must_use]
    pub const fn receipt(&self) -> &ArtifactPublicationReceipt {
        match self {
            Self::Published(receipt) | Self::Replayed(receipt) => receipt,
        }
    }
}

/// Durable specification of a new artifact. `artifact_id` is authority-issued
/// upstream; `idempotency_key` makes creation exactly-once per caller.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CreateArtifactSpec {
    pub artifact_id: ArtifactId,
    pub idempotency_key: IdempotencyKey,
    pub content_type: String,
    /// Application association placeholder (`[ART-OWNER-001]` separation of
    /// package content and user data is a later slice).
    pub application_id: Option<ApplicationId>,
    /// Owner placeholder; ownership/legal-hold semantics are a later slice.
    pub owner: Option<String>,
    /// Caller-supplied creation timestamp (milliseconds since Unix epoch).
    pub created_at_ms: u64,
}

/// Outcome of [`crate::ArtifactStore::create_artifact`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CreateArtifactDecision {
    /// The artifact was created by this call.
    Created(ArtifactRecord),
    /// The idempotency key already named the same specification; the stored
    /// record is returned unchanged.
    Existing(ArtifactRecord),
}

impl CreateArtifactDecision {
    #[must_use]
    pub const fn record(&self) -> &ArtifactRecord {
        match self {
            Self::Created(record) | Self::Existing(record) => record,
        }
    }
}

/// Request to append one immutable revision and advance the head.
///
/// The new revision number is derived by the authority as
/// `expected_head_revision + 1`; revision identity is therefore
/// deterministic and authority-issued, never caller-forged.
#[derive(Clone, Copy, Debug)]
pub struct PutRevisionRequest<'a> {
    pub artifact_id: ArtifactId,
    /// The head the caller observed. Zero means "no revision yet".
    pub expected_head_revision: u64,
    pub bytes: &'a [u8],
    /// Caller-supplied revision timestamp (milliseconds since Unix epoch).
    pub created_at_ms: u64,
}

/// Outcome of [`crate::ArtifactStore::put_revision`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PutRevisionDecision {
    /// The revision was inserted and the head advanced by this call.
    Committed(RevisionRecord),
    /// The exact same revision (same derived number, same digest) was
    /// already committed; the durable record is replayed unchanged.
    Replayed(RevisionRecord),
}

impl PutRevisionDecision {
    #[must_use]
    pub const fn record(&self) -> &RevisionRecord {
        match self {
            Self::Committed(record) | Self::Replayed(record) => record,
        }
    }
}

/// Durable artifact metadata row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactRecord {
    pub artifact_id: ArtifactId,
    pub content_type: String,
    pub application_id: Option<ApplicationId>,
    pub owner: Option<String>,
    /// Current head revision number; zero means the artifact has no
    /// revisions yet.
    pub head_revision: u64,
    pub head_digest: Option<ContentDigest>,
    pub created_at_ms: u64,
}

/// Durable immutable revision row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RevisionRecord {
    pub artifact_id: ArtifactId,
    pub revision: u64,
    pub digest: ContentDigest,
    pub size_bytes: u64,
    pub created_at_ms: u64,
}

/// Resolved mutable head pointer of an artifact.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HeadState {
    pub revision: u64,
    pub digest: ContentDigest,
}

/// A committed revision whose blob bytes are missing from the local
/// content-addressed store.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MissingBlob {
    pub artifact_id: ArtifactId,
    pub revision: u64,
    pub digest: ContentDigest,
}

/// A staged revision whose durable blob is missing.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MissingStagedBlob {
    pub staging_id: StagingId,
    pub artifact_id: ArtifactId,
    pub target_revision: u64,
    pub digest: ContentDigest,
}

/// Result of [`crate::ArtifactStore::recover`].
///
/// Recovery reconciles metadata against blob presence. It never repairs
/// `missing_blobs` (re-fetch/restore is a policy decision above this crate)
/// and never deletes `orphan_blobs`/`orphan_cache_blobs` in this slice
/// (listed for a later GC slice only). Orphan temporary files left by
/// interrupted blob commits are removed, because a tmp file is by definition
/// pre-rename and therefore uncommitted.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RecoveryReport {
    /// Committed revisions whose blob file is absent. Each entry makes the
    /// corresponding `get_revision` fail with
    /// [`crate::ArtifactError::BlobMissing`].
    pub missing_blobs: Vec<MissingBlob>,
    /// Staged candidates whose blob is absent. They remain staged so a
    /// caller can retry/repair without silently changing authority state.
    pub missing_staged_blobs: Vec<MissingStagedBlob>,
    /// Blob files under `artifacts/blobs/` no committed revision or active
    /// staged revision references.
    pub orphan_blobs: Vec<ContentDigest>,
    /// Files under `cache/blobs/` no cache entry references.
    pub orphan_cache_blobs: Vec<ContentDigest>,
    /// Cache metadata rows dropped because their blob is gone (best-effort
    /// cache self-heal; the entry degrades to a cache miss).
    pub cache_rows_dropped: u64,
    /// Orphan temporary files removed from `artifacts/tmp/` and
    /// `cache/tmp/` respectively.
    pub removed_tmp_files: u64,
    /// Files inside the blob trees whose names are not valid digest
    /// addresses; reported for operator attention, never touched.
    pub foreign_files: Vec<PathBuf>,
}
