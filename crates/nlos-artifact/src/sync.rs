//! Content-type-keyed Artifact synchronization policy registry and evaluator
//! (`[DIST-ART-001]`).
//!
//! Per `docs/design/06-架构设计总纲-v0.5.md` §14.1 (`[ART-VERSION-001]`,
//! `[ART-LOCAL-001]`) and §26.2 (`[DIST-ART-001]`):
//!
//! > `[DIST-ART-001]` Artifact sync MUST 为每种内容类型声明 merge、conflict、
//! > authoritative replica 和 deletion semantics。
//!
//! # Four Mandatory Policy Axes
//!
//! Every registered `content_type` in [`ArtifactSyncPolicyRegistry`] must
//! explicitly bind all four axes:
//!
//! 1. [`ArtifactMergeStrategy`] — `ImmutableContentAddressed`,
//!    `FastForwardLineageOnly`, or `AuthoritativeReplicaWins`.
//! 2. [`ArtifactConflictStrategy`] — `FailClosedConflict`,
//!    `QuarantineDivergentBranch`, or `DeferToAuthoritativeReplica`.
//! 3. [`ArtifactReplicaAuthority`] — `SymmetricPeer` or
//!    `SingleAuthoritativeCell { authoritative_cell_id }`.
//! 4. [`ArtifactDeletionSemantics`] — `Forbidden`,
//!    `TombstoneRevisionRequired`, or `GcAfterRetentionBarrier`.
//!
//! Empty or unregistered content types and invalid policy combinations
//! (such as [`ArtifactConflictStrategy::DeferToAuthoritativeReplica`] or
//! [`ArtifactMergeStrategy::AuthoritativeReplicaWins`] paired with
//! [`ArtifactReplicaAuthority::SymmetricPeer`]) fail closed.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::error::Error;
use std::fmt;

use nlos_types::{ApplicationId, ArtifactId, CommitPermitId, IdempotencyKey, ReceiptId, TaskId};
use rusqlite::{Row, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use crate::ArtifactError;
use crate::blob;
use crate::gc::{CollectOrphanBlobsDecision, CollectOrphanBlobsRequest, GcReceipt};
use crate::model::{
    ArtifactPublicationReceipt, ArtifactRecord, ContentDigest, CreateArtifactSpec, HeadState,
    PublishStagedRevisionRequest, StageRevisionRequest, StagedRevisionRecord,
};
use crate::query::{SqlRead, load_all_revision_digests};
use crate::retention::SetRetentionRequest;
use crate::store::{ArtifactStore, encode_u64, validate_text_component};

/// Domain separation tag for deterministic [`ArtifactConflictReceipt`] digests.
pub const ARTIFACT_CONFLICT_RECEIPT_DOMAIN: &[u8] = b"llmos/artifact-conflict-receipt/v1";

/// Domain separation tag for deterministic [`ArtifactTransferPackage`] digests.
pub const ARTIFACT_TRANSFER_PACKAGE_DOMAIN: &[u8] = b"llmos/artifact-transfer-package/v1";

/// Merge semantics declared for a content type (`[DIST-ART-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtifactMergeStrategy {
    /// Revisions are immutable and content-addressed; only an identical
    /// `(revision, digest)` replay or a strictly newer contiguous revision
    /// (`local_head_revision + 1`) building on the local head digest is
    /// accepted.
    ImmutableContentAddressed,
    /// Accepts a remote head only when its ancestor lineage includes the
    /// local head `(revision, digest)` (fast-forward only); concurrent
    /// divergent branches never auto-merge.
    FastForwardLineageOnly,
    /// Accepts a remote head when originated by the declared authoritative
    /// replica cell (`SingleAuthoritativeCell`).
    AuthoritativeReplicaWins,
}

/// Conflict handling strategy when local and remote heads diverge
/// (`[DIST-ART-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtifactConflictStrategy {
    /// Emits a deterministic [`ArtifactConflictReceipt`] and refuses to
    /// advance the local head.
    FailClosedConflict,
    /// Records both local and remote heads in a quarantined conflict outcome
    /// requiring explicit human or controller resolution.
    QuarantineDivergentBranch,
    /// Defers conflict resolution to the declared single authoritative
    /// replica cell. Valid only when
    /// [`ArtifactReplicaAuthority::SingleAuthoritativeCell`] is declared.
    DeferToAuthoritativeReplica,
}

/// Replica authority topology for a content type (`[DIST-ART-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtifactReplicaAuthority {
    /// No single cell owns canonical head advancement.
    SymmetricPeer,
    /// A single designated cell (`authoritative_cell_id`) is authoritative
    /// for this content type.
    SingleAuthoritativeCell { authoritative_cell_id: [u8; 16] },
}

/// Deletion synchronization semantics for a content type (`[DIST-ART-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtifactDeletionSemantics {
    /// Deletion synchronization is rejected fail-closed.
    Forbidden,
    /// Logical deletion must be represented as an explicit staged/published
    /// tombstone revision advancing `head_revision + 1`, never silent file
    /// removal.
    TombstoneRevisionRequired,
    /// Allows physical reclamation only after both retention expiry and
    /// zero active reference-count barriers are satisfied.
    GcAfterRetentionBarrier,
}

/// Complete `[DIST-ART-001]` synchronization policy bound to one
/// `content_type`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactSyncPolicy {
    pub content_type: String,
    pub merge_strategy: ArtifactMergeStrategy,
    pub conflict_strategy: ArtifactConflictStrategy,
    pub replica_authority: ArtifactReplicaAuthority,
    pub deletion_semantics: ArtifactDeletionSemantics,
}

impl ArtifactSyncPolicy {
    /// Validates the content type and cross-axis policy invariants.
    ///
    /// # Errors
    ///
    /// Returns [`ArtifactSyncPolicyError`] if `content_type` is empty,
    /// oversized, or contains NUL, if `SingleAuthoritativeCell` uses an
    /// all-zero cell ID, or if `AuthoritativeReplicaWins` /
    /// `DeferToAuthoritativeReplica` is paired with `SymmetricPeer`.
    pub fn validate(&self) -> Result<(), ArtifactSyncPolicyError> {
        validate_text_component("content_type", &self.content_type)
            .map_err(|_| ArtifactSyncPolicyError::InvalidContentType)?;

        if let ArtifactReplicaAuthority::SingleAuthoritativeCell {
            authoritative_cell_id,
        } = self.replica_authority
            && authoritative_cell_id == [0_u8; 16]
        {
            return Err(ArtifactSyncPolicyError::ZeroAuthoritativeCellId);
        }

        if self.merge_strategy == ArtifactMergeStrategy::AuthoritativeReplicaWins
            && matches!(
                self.replica_authority,
                ArtifactReplicaAuthority::SymmetricPeer
            )
        {
            return Err(
                ArtifactSyncPolicyError::IncompatibleAuthorityForMergeStrategy {
                    merge_strategy: self.merge_strategy,
                    replica_authority: self.replica_authority,
                },
            );
        }

        if self.conflict_strategy == ArtifactConflictStrategy::DeferToAuthoritativeReplica
            && matches!(
                self.replica_authority,
                ArtifactReplicaAuthority::SymmetricPeer
            )
        {
            return Err(
                ArtifactSyncPolicyError::IncompatibleAuthorityForConflictStrategy {
                    conflict_strategy: self.conflict_strategy,
                    replica_authority: self.replica_authority,
                },
            );
        }

        Ok(())
    }
}

/// Outcome of registering a policy in [`ArtifactSyncPolicyRegistry`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PolicyRegistrationDecision {
    /// The policy was newly registered for its `content_type`.
    Registered(ArtifactSyncPolicy),
    /// The exact same policy was already registered for its `content_type`.
    Replayed(ArtifactSyncPolicy),
}

impl PolicyRegistrationDecision {
    #[must_use]
    pub const fn policy(&self) -> &ArtifactSyncPolicy {
        match self {
            Self::Registered(policy) | Self::Replayed(policy) => policy,
        }
    }
}

/// Deterministic content-type-keyed `[DIST-ART-001]` policy registry.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ArtifactSyncPolicyRegistry {
    policies: BTreeMap<String, ArtifactSyncPolicy>,
}

impl ArtifactSyncPolicyRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self {
            policies: BTreeMap::new(),
        }
    }

    /// Validates and registers a synchronization policy for `policy.content_type`.
    ///
    /// Re-registering the exact same policy replays [`PolicyRegistrationDecision::Replayed`];
    /// attempting to rebind an existing `content_type` to a different policy
    /// fails closed with [`ArtifactSyncPolicyError::ConflictingPolicyRegistration`].
    ///
    /// # Errors
    ///
    /// Returns [`ArtifactSyncPolicyError`] on validation or conflict failure.
    pub fn register(
        &mut self,
        policy: ArtifactSyncPolicy,
    ) -> Result<PolicyRegistrationDecision, ArtifactSyncPolicyError> {
        policy.validate()?;
        if let Some(existing) = self.policies.get(&policy.content_type) {
            if *existing == policy {
                return Ok(PolicyRegistrationDecision::Replayed(existing.clone()));
            }
            return Err(ArtifactSyncPolicyError::ConflictingPolicyRegistration {
                content_type: policy.content_type,
            });
        }
        let content_type = policy.content_type.clone();
        self.policies.insert(content_type, policy.clone());
        Ok(PolicyRegistrationDecision::Registered(policy))
    }

    /// Resolves the declared `[DIST-ART-001]` policy for `content_type`.
    ///
    /// # Errors
    ///
    /// Fails closed if `content_type` is malformed or unregistered.
    pub fn resolve(
        &self,
        content_type: &str,
    ) -> Result<&ArtifactSyncPolicy, ArtifactSyncPolicyError> {
        validate_text_component("content_type", content_type)
            .map_err(|_| ArtifactSyncPolicyError::InvalidContentType)?;
        self.policies.get(content_type).ok_or_else(|| {
            ArtifactSyncPolicyError::UnregisteredContentType {
                content_type: content_type.to_string(),
            }
        })
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.policies.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.policies.is_empty()
    }
}

/// Policy registration and lookup errors (`[DIST-ART-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArtifactSyncPolicyError {
    /// `content_type` was empty, exceeded `MAX_TEXT_COMPONENT_BYTES`, or
    /// contained a NUL byte.
    InvalidContentType,
    /// `SingleAuthoritativeCell` specified an all-zero cell identifier.
    ZeroAuthoritativeCellId,
    /// `merge_strategy` requires `SingleAuthoritativeCell`, but
    /// `SymmetricPeer` was declared.
    IncompatibleAuthorityForMergeStrategy {
        merge_strategy: ArtifactMergeStrategy,
        replica_authority: ArtifactReplicaAuthority,
    },
    /// `conflict_strategy` requires `SingleAuthoritativeCell`, but
    /// `SymmetricPeer` was declared.
    IncompatibleAuthorityForConflictStrategy {
        conflict_strategy: ArtifactConflictStrategy,
        replica_authority: ArtifactReplicaAuthority,
    },
    /// Another policy with different axes is already registered for
    /// `content_type`.
    ConflictingPolicyRegistration { content_type: String },
    /// No `[DIST-ART-001]` policy is registered for `content_type`.
    UnregisteredContentType { content_type: String },
}

impl fmt::Display for ArtifactSyncPolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidContentType => {
                formatter.write_str("invalid artifact content_type (empty, oversized, or NUL)")
            }
            Self::ZeroAuthoritativeCellId => {
                formatter.write_str("SingleAuthoritativeCell cannot use an all-zero cell ID")
            }
            Self::IncompatibleAuthorityForMergeStrategy {
                merge_strategy,
                replica_authority,
            } => write!(
                formatter,
                "merge strategy {merge_strategy:?} is incompatible with replica authority {replica_authority:?}"
            ),
            Self::IncompatibleAuthorityForConflictStrategy {
                conflict_strategy,
                replica_authority,
            } => write!(
                formatter,
                "conflict strategy {conflict_strategy:?} is incompatible with replica authority {replica_authority:?}"
            ),
            Self::ConflictingPolicyRegistration { content_type } => write!(
                formatter,
                "conflicting sync policy registration for content_type {content_type:?}"
            ),
            Self::UnregisteredContentType { content_type } => write!(
                formatter,
                "unregistered artifact sync policy for content_type {content_type:?}"
            ),
        }
    }
}

impl Error for ArtifactSyncPolicyError {}

/// Deterministic conflict receipt emitted when local and remote replica heads
/// diverge (`[DIST-ART-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactConflictReceipt {
    pub artifact_id: ArtifactId,
    pub content_type: String,
    pub local_head: HeadState,
    pub remote_head: HeadState,
    pub origin_cell_id: [u8; 16],
    pub strategy: ArtifactConflictStrategy,
    /// Domain-separated SHA-256 digest over `(artifact_id, content_type,
    /// canonical_pair((local_rev, local_digest), (remote_rev, remote_digest)))`.
    /// Byte-stable across `(local_head, remote_head)` order permutations.
    pub conflict_digest: ContentDigest,
}

/// Computes the domain-separated SHA-256 `conflict_digest` for two divergent
/// heads of `artifact_id` under `content_type`.
///
/// The two `(revision, digest)` pairs are canonicalized in ascending
/// `(revision, digest)` order so both replicas observing the same divergence
/// derive the exact same `conflict_digest`.
#[must_use]
pub fn compute_conflict_digest(
    artifact_id: ArtifactId,
    content_type: &str,
    head_a: HeadState,
    head_b: HeadState,
) -> ContentDigest {
    let ((first_rev, first_digest), (second_rev, second_digest)) =
        if (head_a.revision, head_a.digest.as_bytes())
            <= (head_b.revision, head_b.digest.as_bytes())
        {
            (
                (head_a.revision, head_a.digest),
                (head_b.revision, head_b.digest),
            )
        } else {
            (
                (head_b.revision, head_b.digest),
                (head_a.revision, head_a.digest),
            )
        };

    let mut hasher = Sha256::new();
    hasher.update(ARTIFACT_CONFLICT_RECEIPT_DOMAIN);
    hasher.update(artifact_id.as_bytes());
    let content_type_len = u64::try_from(content_type.len()).unwrap_or(u64::MAX);
    hasher.update(content_type_len.to_be_bytes());
    hasher.update(content_type.as_bytes());
    hasher.update(first_rev.to_be_bytes());
    hasher.update(first_digest.as_bytes());
    hasher.update(second_rev.to_be_bytes());
    hasher.update(second_digest.as_bytes());
    ContentDigest::from_bytes(hasher.finalize().into())
}

/// Local replica state supplied to [`evaluate_artifact_sync`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactLocalSyncState {
    pub artifact_id: ArtifactId,
    pub content_type: String,
    pub local_head: Option<HeadState>,
}

/// Operation kind presented by a remote replica during synchronization.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ArtifactSyncOperationKind {
    /// Standard content revision synchronization.
    RevisionUpdate,
    /// Logical deletion represented as a signed/staged tombstone revision
    /// advancing `head_revision + 1`.
    TombstoneDeletion,
    /// Attempted unversioned/silent deletion without a tombstone revision.
    SilentUnversionedDeletion,
    /// Attempted physical GC reclamation of an artifact's blobs.
    PhysicalGcReclamation {
        retention_expired: bool,
        active_ref_count: u64,
    },
}

/// Remote synchronization candidate evaluated against [`ArtifactLocalSyncState`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactRemoteSyncCandidate {
    pub origin_cell_id: [u8; 16],
    pub operation_kind: ArtifactSyncOperationKind,
    pub remote_head: Option<HeadState>,
    pub parent_head: Option<HeadState>,
    pub ancestor_lineage: Vec<HeadState>,
}

impl ArtifactRemoteSyncCandidate {
    /// Constructs a [`ArtifactSyncOperationKind::RevisionUpdate`] candidate.
    #[must_use]
    pub const fn revision_update(
        origin_cell_id: [u8; 16],
        remote_head: HeadState,
        parent_head: Option<HeadState>,
        ancestor_lineage: Vec<HeadState>,
    ) -> Self {
        Self {
            origin_cell_id,
            operation_kind: ArtifactSyncOperationKind::RevisionUpdate,
            remote_head: Some(remote_head),
            parent_head,
            ancestor_lineage,
        }
    }

    /// Constructs a [`ArtifactSyncOperationKind::TombstoneDeletion`] candidate.
    #[must_use]
    pub const fn tombstone_deletion(
        origin_cell_id: [u8; 16],
        tombstone_head: HeadState,
        parent_head: Option<HeadState>,
        ancestor_lineage: Vec<HeadState>,
    ) -> Self {
        Self {
            origin_cell_id,
            operation_kind: ArtifactSyncOperationKind::TombstoneDeletion,
            remote_head: Some(tombstone_head),
            parent_head,
            ancestor_lineage,
        }
    }

    /// Constructs a [`ArtifactSyncOperationKind::SilentUnversionedDeletion`]
    /// candidate.
    #[must_use]
    pub const fn silent_unversioned_deletion(origin_cell_id: [u8; 16]) -> Self {
        Self {
            origin_cell_id,
            operation_kind: ArtifactSyncOperationKind::SilentUnversionedDeletion,
            remote_head: None,
            parent_head: None,
            ancestor_lineage: Vec::new(),
        }
    }

    /// Constructs a [`ArtifactSyncOperationKind::PhysicalGcReclamation`]
    /// candidate.
    #[must_use]
    pub const fn physical_gc_reclamation(
        origin_cell_id: [u8; 16],
        retention_expired: bool,
        active_ref_count: u64,
    ) -> Self {
        Self {
            origin_cell_id,
            operation_kind: ArtifactSyncOperationKind::PhysicalGcReclamation {
                retention_expired,
                active_ref_count,
            },
            remote_head: None,
            parent_head: None,
            ancestor_lineage: Vec::new(),
        }
    }
}

/// Pure deterministic outcome of [`evaluate_artifact_sync`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArtifactSyncDecision {
    /// Local head already matches `(remote_head.revision, remote_head.digest)`.
    NoOpAlreadySynced {
        artifact_id: ArtifactId,
        head: HeadState,
    },
    /// Remote head is accepted to advance the local replica head.
    AcceptRemoteHead {
        artifact_id: ArtifactId,
        prior_local_head: Option<HeadState>,
        new_head: HeadState,
        origin_cell_id: [u8; 16],
        is_tombstone: bool,
    },
    /// Divergent head refused fail-closed with a deterministic conflict
    /// receipt.
    RefuseConflict(ArtifactConflictReceipt),
    /// Divergent head recorded into quarantine with a deterministic conflict
    /// receipt; canonical head does not advance.
    QuarantineConflict(ArtifactConflictReceipt),
    /// Physical blob reclamation is permitted after satisfying the retention
    /// and reference-count barrier.
    ReclaimEligible { artifact_id: ArtifactId },
}

/// Typed failures of artifact synchronization evaluation and replica staging.
#[derive(Debug)]
pub enum ArtifactSyncError {
    /// Policy lookup or validation failed.
    Policy(ArtifactSyncPolicyError),
    /// Underlying local [`ArtifactStore`] failure.
    Store(Box<ArtifactError>),
    /// The remote bundle's `content_type` disagrees with the local artifact's
    /// durable `content_type`.
    ContentTypeMismatch { expected: String, actual: String },
    /// Candidate's `origin_cell_id` was all zeros.
    ZeroOriginCellId,
    /// A revision or tombstone sync operation omitted `remote_head`.
    MissingRemoteHead,
    /// `remote_head.revision` was zero (revisions are 1-based).
    InvalidRemoteRevision(u64),
    /// `parent_head` or `ancestor_lineage` violated revision ordering or
    /// internal digest consistency.
    InvalidLineage(&'static str),
    /// Identical revision number presented with a conflicting content digest
    /// where branching does not apply.
    CorruptRevisionDigestMismatch {
        artifact_id: ArtifactId,
        revision: u64,
        local_digest: ContentDigest,
        remote_digest: ContentDigest,
    },
    /// Remote payload bytes did not hash to `remote_head.digest`.
    RemotePayloadDigestMismatch {
        expected: ContentDigest,
        actual: ContentDigest,
    },
    /// Remote revision is not contiguous with the local head when contiguity
    /// is required (`ImmutableContentAddressed`, tombstone revisions, or
    /// single-step store staging).
    NonContiguousRevision {
        expected_revision: u64,
        remote_revision: u64,
    },
    /// A non-authoritative cell attempted to advance or override head under
    /// `SingleAuthoritativeCell`.
    NonAuthoritativeReplicaRejected {
        artifact_id: ArtifactId,
        origin_cell_id: [u8; 16],
        authoritative_cell_id: [u8; 16],
        conflict_receipt: Option<Box<ArtifactConflictReceipt>>,
    },
    /// Deletion synchronization was attempted on a content type with
    /// [`ArtifactDeletionSemantics::Forbidden`].
    DeletionForbidden {
        artifact_id: ArtifactId,
        content_type: String,
    },
    /// Silent/unversioned deletion was attempted on a content type requiring
    /// an explicit tombstone revision.
    TombstoneRevisionRequired {
        artifact_id: ArtifactId,
        content_type: String,
    },
    /// Physical GC reclamation was attempted under
    /// [`ArtifactDeletionSemantics::TombstoneRevisionRequired`].
    PhysicalGcNotPermitted {
        artifact_id: ArtifactId,
        content_type: String,
    },
    /// Physical GC reclamation was attempted before the retention and
    /// active-reference barriers were satisfied.
    RetentionBarrierUnsatisfied {
        artifact_id: ArtifactId,
        retention_expired: bool,
        active_ref_count: u64,
    },
    /// Attempted to export a transfer package for an artifact with zero
    /// committed revisions.
    EmptyArtifactCannotExport(ArtifactId),
    /// Pre-materialization `SHA-256` verification of an [`ArtifactBlobPayload`]
    /// or bundle payload failed; zero durable writes occurred.
    TamperedBlobDigest {
        declared: ContentDigest,
        computed: ContentDigest,
    },
    /// Domain-separated `package_digest` (`"llmos/artifact-transfer-package/v1"`)
    /// verification failed; zero durable writes occurred.
    TamperedPackageDigest {
        expected: [u8; 32],
        actual: [u8; 32],
    },
    /// A revision entry in [`ArtifactTransferPackage`] referenced a digest
    /// absent from `package.blobs`.
    MissingBlobInTransferPackage {
        revision: u64,
        digest: ContentDigest,
    },
    /// Tombstone metadata disagreed with `bundle.is_tombstone`, target
    /// revision, or retention timestamp ordering.
    InvalidTombstoneMetadata(&'static str),
    /// Caller-supplied `expected_local_state` disagreed with `target_store`'s
    /// durable head state (optimistic CAS guard).
    LocalHeadStateDrift {
        expected: Option<HeadState>,
        actual: Option<HeadState>,
    },
    /// Requested sync blob or conflict digest is not actively pinned in
    /// [`ReplicaSyncRetentionSet`].
    SyncBlobNotPinned(ContentDigest),
    /// Requested tombstone reclamation check targets an artifact with no
    /// tracked tombstone entry in [`ReplicaSyncRetentionSet`].
    TombstoneNotTracked(ArtifactId),
}

impl fmt::Display for ArtifactSyncError {
    // Exhaustive diagnostic match over every typed sync failure mode.
    #[allow(clippy::too_many_lines)]
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Policy(error) => write!(formatter, "artifact sync policy error: {error}"),
            Self::Store(error) => write!(formatter, "artifact store error during sync: {error}"),
            Self::ContentTypeMismatch { expected, actual } => write!(
                formatter,
                "artifact content_type mismatch: local={expected:?}, remote={actual:?}"
            ),
            Self::ZeroOriginCellId => {
                formatter.write_str("remote sync candidate origin_cell_id cannot be all zeros")
            }
            Self::MissingRemoteHead => {
                formatter.write_str("remote sync candidate is missing remote_head")
            }
            Self::InvalidRemoteRevision(revision) => {
                write!(formatter, "invalid remote revision number {revision}")
            }
            Self::InvalidLineage(reason) => {
                write!(formatter, "invalid remote revision lineage: {reason}")
            }
            Self::CorruptRevisionDigestMismatch {
                artifact_id,
                revision,
                local_digest,
                remote_digest,
            } => write!(
                formatter,
                "corrupt revision digest mismatch on {artifact_id:?} rev {revision}: \
                 local={local_digest}, remote={remote_digest}"
            ),
            Self::RemotePayloadDigestMismatch { expected, actual } => write!(
                formatter,
                "remote payload digest mismatch: expected {expected}, actual {actual}"
            ),
            Self::NonContiguousRevision {
                expected_revision,
                remote_revision,
            } => write!(
                formatter,
                "non-contiguous remote revision: expected {expected_revision}, got {remote_revision}"
            ),
            Self::NonAuthoritativeReplicaRejected {
                artifact_id,
                origin_cell_id,
                authoritative_cell_id,
                ..
            } => write!(
                formatter,
                "non-authoritative replica {origin_cell_id:02x?} rejected for {artifact_id:?} \
                 (authoritative cell is {authoritative_cell_id:02x?})"
            ),
            Self::DeletionForbidden {
                artifact_id,
                content_type,
            } => write!(
                formatter,
                "deletion sync is forbidden for {artifact_id:?} ({content_type:?})"
            ),
            Self::TombstoneRevisionRequired {
                artifact_id,
                content_type,
            } => write!(
                formatter,
                "tombstone revision is required for deleting {artifact_id:?} ({content_type:?})"
            ),
            Self::PhysicalGcNotPermitted {
                artifact_id,
                content_type,
            } => write!(
                formatter,
                "physical GC reclamation is not permitted for {artifact_id:?} ({content_type:?})"
            ),
            Self::RetentionBarrierUnsatisfied {
                artifact_id,
                retention_expired,
                active_ref_count,
            } => write!(
                formatter,
                "retention/refcount barrier unsatisfied for {artifact_id:?}: \
                 retention_expired={retention_expired}, active_ref_count={active_ref_count}"
            ),
            Self::EmptyArtifactCannotExport(artifact_id) => write!(
                formatter,
                "cannot export transfer package for {artifact_id:?}: no committed revisions"
            ),
            Self::TamperedBlobDigest { declared, computed } => write!(
                formatter,
                "tampered transfer blob digest: declared={declared}, computed={computed}"
            ),
            Self::TamperedPackageDigest { expected, actual } => write!(
                formatter,
                "tampered transfer package digest: expected={expected:02x?}, actual={actual:02x?}"
            ),
            Self::MissingBlobInTransferPackage { revision, digest } => write!(
                formatter,
                "transfer package missing blob {digest} for revision {revision}"
            ),
            Self::InvalidTombstoneMetadata(reason) => {
                write!(formatter, "invalid tombstone metadata: {reason}")
            }
            Self::LocalHeadStateDrift { expected, actual } => write!(
                formatter,
                "local sync head state drifted: expected={expected:?}, actual={actual:?}"
            ),
            Self::SyncBlobNotPinned(digest) => write!(
                formatter,
                "sync blob or conflict digest {digest} is not actively pinned"
            ),
            Self::TombstoneNotTracked(artifact_id) => write!(
                formatter,
                "no tombstone retention entry tracked for {artifact_id:?}"
            ),
        }
    }
}

impl Error for ArtifactSyncError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Policy(error) => Some(error),
            Self::Store(error) => Some(error.as_ref()),
            _ => None,
        }
    }
}

impl From<ArtifactSyncPolicyError> for ArtifactSyncError {
    fn from(error: ArtifactSyncPolicyError) -> Self {
        Self::Policy(error)
    }
}

impl From<ArtifactError> for ArtifactSyncError {
    fn from(error: ArtifactError) -> Self {
        Self::Store(Box::new(error))
    }
}

/// Pure deterministic `[DIST-ART-001]` synchronization evaluator.
///
/// Resolves the mandatory policy for `local.content_type` and evaluates
/// `remote` across merge, conflict, replica-authority, and deletion axes.
///
/// # Errors
///
/// Returns [`ArtifactSyncError`] on unregistered content type, malformed
/// candidate lineage, corrupt same-revision digest mismatch, unauthorized
/// replica write, or forbidden deletion semantics.
pub fn evaluate_artifact_sync(
    registry: &ArtifactSyncPolicyRegistry,
    local: &ArtifactLocalSyncState,
    remote: &ArtifactRemoteSyncCandidate,
) -> Result<ArtifactSyncDecision, ArtifactSyncError> {
    let policy = registry.resolve(&local.content_type)?;
    if remote.origin_cell_id == [0_u8; 16] {
        return Err(ArtifactSyncError::ZeroOriginCellId);
    }

    match remote.operation_kind {
        ArtifactSyncOperationKind::SilentUnversionedDeletion => match policy.deletion_semantics {
            ArtifactDeletionSemantics::Forbidden => Err(ArtifactSyncError::DeletionForbidden {
                artifact_id: local.artifact_id,
                content_type: local.content_type.clone(),
            }),
            ArtifactDeletionSemantics::TombstoneRevisionRequired
            | ArtifactDeletionSemantics::GcAfterRetentionBarrier => {
                Err(ArtifactSyncError::TombstoneRevisionRequired {
                    artifact_id: local.artifact_id,
                    content_type: local.content_type.clone(),
                })
            }
        },
        ArtifactSyncOperationKind::PhysicalGcReclamation {
            retention_expired,
            active_ref_count,
        } => match policy.deletion_semantics {
            ArtifactDeletionSemantics::Forbidden => Err(ArtifactSyncError::DeletionForbidden {
                artifact_id: local.artifact_id,
                content_type: local.content_type.clone(),
            }),
            ArtifactDeletionSemantics::TombstoneRevisionRequired => {
                Err(ArtifactSyncError::PhysicalGcNotPermitted {
                    artifact_id: local.artifact_id,
                    content_type: local.content_type.clone(),
                })
            }
            ArtifactDeletionSemantics::GcAfterRetentionBarrier => {
                if retention_expired && active_ref_count == 0 {
                    Ok(ArtifactSyncDecision::ReclaimEligible {
                        artifact_id: local.artifact_id,
                    })
                } else {
                    Err(ArtifactSyncError::RetentionBarrierUnsatisfied {
                        artifact_id: local.artifact_id,
                        retention_expired,
                        active_ref_count,
                    })
                }
            }
        },
        ArtifactSyncOperationKind::TombstoneDeletion => match policy.deletion_semantics {
            ArtifactDeletionSemantics::Forbidden => Err(ArtifactSyncError::DeletionForbidden {
                artifact_id: local.artifact_id,
                content_type: local.content_type.clone(),
            }),
            ArtifactDeletionSemantics::TombstoneRevisionRequired
            | ArtifactDeletionSemantics::GcAfterRetentionBarrier => {
                evaluate_revision_sync(policy, local, remote, true)
            }
        },
        ArtifactSyncOperationKind::RevisionUpdate => {
            evaluate_revision_sync(policy, local, remote, false)
        }
    }
}

fn evaluate_revision_sync(
    policy: &ArtifactSyncPolicy,
    local: &ArtifactLocalSyncState,
    remote: &ArtifactRemoteSyncCandidate,
    is_tombstone: bool,
) -> Result<ArtifactSyncDecision, ArtifactSyncError> {
    let remote_head = remote
        .remote_head
        .ok_or(ArtifactSyncError::MissingRemoteHead)?;
    if remote_head.revision == 0 {
        return Err(ArtifactSyncError::InvalidRemoteRevision(0));
    }
    if let Some(local_head) = local.local_head
        && local_head.revision == 0
    {
        return Err(ArtifactSyncError::InvalidLineage(
            "local head revision cannot be zero when present",
        ));
    }
    validate_remote_lineage(remote_head, remote.parent_head, &remote.ancestor_lineage)?;

    // Idempotent replay: identical (revision, digest) is always a no-op.
    if let Some(local_head) = local.local_head
        && local_head == remote_head
    {
        return Ok(ArtifactSyncDecision::NoOpAlreadySynced {
            artifact_id: local.artifact_id,
            head: local_head,
        });
    }

    // Replica authority check: under SingleAuthoritativeCell, non-authoritative
    // cells cannot advance or override head.
    if let ArtifactReplicaAuthority::SingleAuthoritativeCell {
        authoritative_cell_id,
    } = policy.replica_authority
        && remote.origin_cell_id != authoritative_cell_id
    {
        let conflict_receipt = local.local_head.map(|local_head| {
            Box::new(build_conflict_receipt(
                local.artifact_id,
                &local.content_type,
                local_head,
                remote_head,
                remote.origin_cell_id,
                policy.conflict_strategy,
            ))
        });
        return Err(ArtifactSyncError::NonAuthoritativeReplicaRejected {
            artifact_id: local.artifact_id,
            origin_cell_id: remote.origin_cell_id,
            authoritative_cell_id,
            conflict_receipt,
        });
    }

    match policy.merge_strategy {
        ArtifactMergeStrategy::AuthoritativeReplicaWins => {
            if is_tombstone {
                let expected = local.local_head.map_or(1, |h| h.revision.saturating_add(1));
                if remote_head.revision != expected {
                    return Err(ArtifactSyncError::NonContiguousRevision {
                        expected_revision: expected,
                        remote_revision: remote_head.revision,
                    });
                }
            }
            Ok(ArtifactSyncDecision::AcceptRemoteHead {
                artifact_id: local.artifact_id,
                prior_local_head: local.local_head,
                new_head: remote_head,
                origin_cell_id: remote.origin_cell_id,
                is_tombstone,
            })
        }
        ArtifactMergeStrategy::ImmutableContentAddressed => {
            evaluate_immutable_content_addressed(policy, local, remote, remote_head, is_tombstone)
        }
        ArtifactMergeStrategy::FastForwardLineageOnly => {
            evaluate_fast_forward_lineage(policy, local, remote, remote_head, is_tombstone)
        }
    }
}

fn evaluate_immutable_content_addressed(
    policy: &ArtifactSyncPolicy,
    local: &ArtifactLocalSyncState,
    remote: &ArtifactRemoteSyncCandidate,
    remote_head: HeadState,
    is_tombstone: bool,
) -> Result<ArtifactSyncDecision, ArtifactSyncError> {
    let Some(local_head) = local.local_head else {
        if remote_head.revision != 1 {
            return Err(ArtifactSyncError::NonContiguousRevision {
                expected_revision: 1,
                remote_revision: remote_head.revision,
            });
        }
        return Ok(ArtifactSyncDecision::AcceptRemoteHead {
            artifact_id: local.artifact_id,
            prior_local_head: None,
            new_head: remote_head,
            origin_cell_id: remote.origin_cell_id,
            is_tombstone,
        });
    };

    if remote_head.revision == local_head.revision {
        return Err(ArtifactSyncError::CorruptRevisionDigestMismatch {
            artifact_id: local.artifact_id,
            revision: local_head.revision,
            local_digest: local_head.digest,
            remote_digest: remote_head.digest,
        });
    }

    let expected_next = local_head.revision.saturating_add(1);
    if remote_head.revision != expected_next {
        return Err(ArtifactSyncError::NonContiguousRevision {
            expected_revision: expected_next,
            remote_revision: remote_head.revision,
        });
    }

    if remote.parent_head == Some(local_head) {
        Ok(ArtifactSyncDecision::AcceptRemoteHead {
            artifact_id: local.artifact_id,
            prior_local_head: Some(local_head),
            new_head: remote_head,
            origin_cell_id: remote.origin_cell_id,
            is_tombstone,
        })
    } else {
        resolve_conflict(
            policy,
            local.artifact_id,
            &local.content_type,
            local_head,
            remote_head,
            remote.origin_cell_id,
            is_tombstone,
        )
    }
}

fn evaluate_fast_forward_lineage(
    policy: &ArtifactSyncPolicy,
    local: &ArtifactLocalSyncState,
    remote: &ArtifactRemoteSyncCandidate,
    remote_head: HeadState,
    is_tombstone: bool,
) -> Result<ArtifactSyncDecision, ArtifactSyncError> {
    let Some(local_head) = local.local_head else {
        if is_tombstone && remote_head.revision != 1 {
            return Err(ArtifactSyncError::NonContiguousRevision {
                expected_revision: 1,
                remote_revision: remote_head.revision,
            });
        }
        if remote_head.revision == 1
            || remote
                .parent_head
                .is_some_and(|parent| parent.revision == 1)
            || remote
                .ancestor_lineage
                .iter()
                .any(|entry| entry.revision == 1)
        {
            return Ok(ArtifactSyncDecision::AcceptRemoteHead {
                artifact_id: local.artifact_id,
                prior_local_head: None,
                new_head: remote_head,
                origin_cell_id: remote.origin_cell_id,
                is_tombstone,
            });
        }
        return Err(ArtifactSyncError::NonContiguousRevision {
            expected_revision: 1,
            remote_revision: remote_head.revision,
        });
    };

    if remote_head.revision == local_head.revision {
        if remote.parent_head.is_none() {
            return Err(ArtifactSyncError::CorruptRevisionDigestMismatch {
                artifact_id: local.artifact_id,
                revision: local_head.revision,
                local_digest: local_head.digest,
                remote_digest: remote_head.digest,
            });
        }
        return resolve_conflict(
            policy,
            local.artifact_id,
            &local.content_type,
            local_head,
            remote_head,
            remote.origin_cell_id,
            is_tombstone,
        );
    }

    if is_tombstone {
        let expected_next = local_head.revision.saturating_add(1);
        if remote_head.revision != expected_next {
            return Err(ArtifactSyncError::NonContiguousRevision {
                expected_revision: expected_next,
                remote_revision: remote_head.revision,
            });
        }
    }

    if remote_head.revision > local_head.revision && remote_lineage_includes(remote, local_head) {
        return Ok(ArtifactSyncDecision::AcceptRemoteHead {
            artifact_id: local.artifact_id,
            prior_local_head: Some(local_head),
            new_head: remote_head,
            origin_cell_id: remote.origin_cell_id,
            is_tombstone,
        });
    }

    resolve_conflict(
        policy,
        local.artifact_id,
        &local.content_type,
        local_head,
        remote_head,
        remote.origin_cell_id,
        is_tombstone,
    )
}

fn validate_remote_lineage(
    remote_head: HeadState,
    parent_head: Option<HeadState>,
    ancestor_lineage: &[HeadState],
) -> Result<(), ArtifactSyncError> {
    if remote_head.revision == 1 && parent_head.is_some() {
        return Err(ArtifactSyncError::InvalidLineage(
            "revision 1 cannot have a parent head",
        ));
    }
    let mut seen = BTreeMap::<u64, ContentDigest>::new();
    if let Some(parent) = parent_head {
        if parent.revision == 0 || parent.revision >= remote_head.revision {
            return Err(ArtifactSyncError::InvalidLineage(
                "parent revision must be in 1..remote_head.revision",
            ));
        }
        seen.insert(parent.revision, parent.digest);
    }
    for ancestor in ancestor_lineage {
        if ancestor.revision == 0 || ancestor.revision >= remote_head.revision {
            return Err(ArtifactSyncError::InvalidLineage(
                "ancestor revision must be in 1..remote_head.revision",
            ));
        }
        if let Some(existing) = seen.get(&ancestor.revision)
            && *existing != ancestor.digest
        {
            return Err(ArtifactSyncError::InvalidLineage(
                "conflicting digests for the same revision in remote lineage",
            ));
        }
        seen.insert(ancestor.revision, ancestor.digest);
    }
    Ok(())
}

fn remote_lineage_includes(remote: &ArtifactRemoteSyncCandidate, local_head: HeadState) -> bool {
    let mut matched = false;
    if let Some(parent) = remote.parent_head {
        if parent.revision == local_head.revision && parent.digest != local_head.digest {
            return false;
        }
        if parent == local_head {
            matched = true;
        }
    }
    for ancestor in &remote.ancestor_lineage {
        if ancestor.revision == local_head.revision && ancestor.digest != local_head.digest {
            return false;
        }
        if *ancestor == local_head {
            matched = true;
        }
    }
    matched
}

fn build_conflict_receipt(
    artifact_id: ArtifactId,
    content_type: &str,
    local_head: HeadState,
    remote_head: HeadState,
    origin_cell_id: [u8; 16],
    strategy: ArtifactConflictStrategy,
) -> ArtifactConflictReceipt {
    let conflict_digest =
        compute_conflict_digest(artifact_id, content_type, local_head, remote_head);
    ArtifactConflictReceipt {
        artifact_id,
        content_type: content_type.to_string(),
        local_head,
        remote_head,
        origin_cell_id,
        strategy,
        conflict_digest,
    }
}

fn resolve_conflict(
    policy: &ArtifactSyncPolicy,
    artifact_id: ArtifactId,
    content_type: &str,
    local_head: HeadState,
    remote_head: HeadState,
    origin_cell_id: [u8; 16],
    is_tombstone: bool,
) -> Result<ArtifactSyncDecision, ArtifactSyncError> {
    let receipt = build_conflict_receipt(
        artifact_id,
        content_type,
        local_head,
        remote_head,
        origin_cell_id,
        policy.conflict_strategy,
    );
    match policy.conflict_strategy {
        ArtifactConflictStrategy::FailClosedConflict => {
            Ok(ArtifactSyncDecision::RefuseConflict(receipt))
        }
        ArtifactConflictStrategy::QuarantineDivergentBranch => {
            Ok(ArtifactSyncDecision::QuarantineConflict(receipt))
        }
        ArtifactConflictStrategy::DeferToAuthoritativeReplica => match policy.replica_authority {
            ArtifactReplicaAuthority::SingleAuthoritativeCell {
                authoritative_cell_id,
            } => {
                if origin_cell_id == authoritative_cell_id {
                    Ok(ArtifactSyncDecision::AcceptRemoteHead {
                        artifact_id,
                        prior_local_head: Some(local_head),
                        new_head: remote_head,
                        origin_cell_id,
                        is_tombstone,
                    })
                } else {
                    Err(ArtifactSyncError::NonAuthoritativeReplicaRejected {
                        artifact_id,
                        origin_cell_id,
                        authoritative_cell_id,
                        conflict_receipt: Some(Box::new(receipt)),
                    })
                }
            }
            ArtifactReplicaAuthority::SymmetricPeer => Err(ArtifactSyncError::Policy(
                ArtifactSyncPolicyError::IncompatibleAuthorityForConflictStrategy {
                    conflict_strategy: policy.conflict_strategy,
                    replica_authority: policy.replica_authority,
                },
            )),
        },
    }
}

/// Verified revision bundle exported from a replica [`ArtifactStore`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReplicaRevisionBundle {
    pub artifact_id: ArtifactId,
    pub content_type: String,
    pub remote_head: HeadState,
    pub parent_head: Option<HeadState>,
    pub ancestor_lineage: Vec<HeadState>,
    pub origin_cell_id: [u8; 16],
    pub bytes: Vec<u8>,
    pub is_tombstone: bool,
}

/// Request to evaluate and durably stage + publish a remote replica revision
/// into a local [`ArtifactStore`].
#[derive(Clone, Copy, Debug)]
pub struct SyncArtifactReplicaRequest<'a> {
    pub bundle: &'a ReplicaRevisionBundle,
    pub task_id: TaskId,
    pub permit_id: CommitPermitId,
    pub write_set_root: ContentDigest,
    pub idempotency_key: IdempotencyKey,
    pub synced_at_ms: u64,
}

/// Outcome of [`sync_artifact_replica_head`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArtifactReplicaSyncOutcome {
    /// Local store already had the identical `(revision, digest)` at head.
    NoOpAlreadySynced {
        artifact_id: ArtifactId,
        head: HeadState,
    },
    /// Remote revision passed `[DIST-ART-001]` policy evaluation and was
    /// durably staged and published into the local [`ArtifactStore`].
    Synced {
        publication_receipt: Box<ArtifactPublicationReceipt>,
        staged_record: Box<StagedRevisionRecord>,
        is_tombstone: bool,
    },
    /// Divergent head refused under [`ArtifactConflictStrategy::FailClosedConflict`];
    /// local store head was not advanced.
    RefusedConflict(ArtifactConflictReceipt),
    /// Divergent head quarantined under
    /// [`ArtifactConflictStrategy::QuarantineDivergentBranch`]; local store
    /// head was not advanced.
    QuarantinedConflict(ArtifactConflictReceipt),
}

/// Exports one committed revision and its durable ancestor lineage from a
/// replica [`ArtifactStore`], re-verifying blob digest integrity on read.
///
/// # Errors
///
/// Returns [`ArtifactSyncError`] if `origin_cell_id` is all zeros or if
/// reading metadata or verified blob bytes from `store` fails.
pub fn export_replica_revision_bundle(
    store: &ArtifactStore,
    artifact_id: ArtifactId,
    revision: u64,
    origin_cell_id: [u8; 16],
    is_tombstone: bool,
    now_ms: u64,
) -> Result<ReplicaRevisionBundle, ArtifactSyncError> {
    if origin_cell_id == [0_u8; 16] {
        return Err(ArtifactSyncError::ZeroOriginCellId);
    }
    let artifact = store.inspect_artifact(artifact_id)?;
    let rev_record = store.inspect_revision(artifact_id, revision)?;
    let bytes = store.get_revision(artifact_id, revision, now_ms)?;
    let all_revisions = store.list_revisions(artifact_id)?;

    let mut ancestor_lineage = Vec::new();
    let mut parent_head = None;
    for prior in all_revisions {
        if prior.revision < revision {
            let state = HeadState {
                revision: prior.revision,
                digest: prior.digest,
            };
            if prior.revision.saturating_add(1) == revision {
                parent_head = Some(state);
            }
            ancestor_lineage.push(state);
        }
    }

    Ok(ReplicaRevisionBundle {
        artifact_id,
        content_type: artifact.content_type,
        remote_head: HeadState {
            revision: rev_record.revision,
            digest: rev_record.digest,
        },
        parent_head,
        ancestor_lineage,
        origin_cell_id,
        bytes,
        is_tombstone,
    })
}

/// Evaluates a remote replica revision bundle under `registry` and, when
/// accepted, stages and publishes the verified bytes into `local_store`.
///
/// # Fail-Closed Order
///
/// 1. Loads the local artifact record and resolves its current head at
///    `request.synced_at_ms` (enforcing retention readability).
/// 2. Verifies `local_artifact.content_type == request.bundle.content_type`
///    and resolves the mandatory `[DIST-ART-001]` policy in `registry`.
/// 3. Verifies `ContentDigest::of_bytes(&request.bundle.bytes) ==
///    request.bundle.remote_head.digest` before any local blob or stage write.
/// 4. Evaluates [`evaluate_artifact_sync`]. On `RefuseConflict` or
///    `QuarantineConflict`, returns the conflict outcome without touching
///    `local_store`.
/// 5. On `AcceptRemoteHead`, durably stages and publishes the revision into
///    `local_store` via [`ArtifactStore::stage_revision`] and
///    [`ArtifactStore::publish_staged_revision`].
///
/// # Errors
///
/// Returns [`ArtifactSyncError`] on policy failure, digest mismatch,
/// non-contiguous store revision, unauthorized replica, or storage error.
pub fn sync_artifact_replica_head(
    local_store: &ArtifactStore,
    registry: &ArtifactSyncPolicyRegistry,
    request: SyncArtifactReplicaRequest<'_>,
) -> Result<ArtifactReplicaSyncOutcome, ArtifactSyncError> {
    let bundle = request.bundle;
    let local_artifact = local_store.inspect_artifact(bundle.artifact_id)?;
    if local_artifact.content_type != bundle.content_type {
        return Err(ArtifactSyncError::ContentTypeMismatch {
            expected: local_artifact.content_type,
            actual: bundle.content_type.clone(),
        });
    }

    // Fail closed if the content_type has no registered [DIST-ART-001] policy.
    registry.resolve(&local_artifact.content_type)?;

    // Enforce retention gate and resolve current local head.
    let local_head = local_store.resolve_head(bundle.artifact_id, request.synced_at_ms)?;

    // Verify payload bytes match the claimed remote head digest before
    // evaluating or writing anything to the local store.
    let actual_digest = ContentDigest::of_bytes(&bundle.bytes);
    if actual_digest != bundle.remote_head.digest {
        return Err(ArtifactSyncError::RemotePayloadDigestMismatch {
            expected: bundle.remote_head.digest,
            actual: actual_digest,
        });
    }

    let local_state = ArtifactLocalSyncState {
        artifact_id: bundle.artifact_id,
        content_type: local_artifact.content_type,
        local_head,
    };
    let candidate = if bundle.is_tombstone {
        ArtifactRemoteSyncCandidate::tombstone_deletion(
            bundle.origin_cell_id,
            bundle.remote_head,
            bundle.parent_head,
            bundle.ancestor_lineage.clone(),
        )
    } else {
        ArtifactRemoteSyncCandidate::revision_update(
            bundle.origin_cell_id,
            bundle.remote_head,
            bundle.parent_head,
            bundle.ancestor_lineage.clone(),
        )
    };

    let decision = evaluate_artifact_sync(registry, &local_state, &candidate)?;
    match decision {
        ArtifactSyncDecision::NoOpAlreadySynced { artifact_id, head } => {
            Ok(ArtifactReplicaSyncOutcome::NoOpAlreadySynced { artifact_id, head })
        }
        ArtifactSyncDecision::RefuseConflict(receipt) => {
            Ok(ArtifactReplicaSyncOutcome::RefusedConflict(receipt))
        }
        ArtifactSyncDecision::QuarantineConflict(receipt) => {
            Ok(ArtifactReplicaSyncOutcome::QuarantinedConflict(receipt))
        }
        ArtifactSyncDecision::ReclaimEligible { .. } => {
            unreachable!("revision/tombstone candidate never produces ReclaimEligible")
        }
        ArtifactSyncDecision::AcceptRemoteHead {
            prior_local_head,
            new_head,
            is_tombstone,
            ..
        } => {
            let expected_head_revision = prior_local_head.map_or(0, |head| head.revision);
            let expected_next = expected_head_revision.saturating_add(1);
            if new_head.revision != expected_next {
                return Err(ArtifactSyncError::NonContiguousRevision {
                    expected_revision: expected_next,
                    remote_revision: new_head.revision,
                });
            }

            let staged_decision = local_store.stage_revision(StageRevisionRequest {
                artifact_id: bundle.artifact_id,
                expected_head_revision,
                bytes: &bundle.bytes,
                task_id: request.task_id,
                permit_id: request.permit_id,
                write_set_root: request.write_set_root,
                idempotency_key: request.idempotency_key,
                created_at_ms: request.synced_at_ms,
            })?;
            let staged_record = staged_decision.record().clone();
            let published_decision =
                local_store.publish_staged_revision(PublishStagedRevisionRequest {
                    staging_id: staged_record.staging_id,
                    task_id: request.task_id,
                    permit_id: request.permit_id,
                    write_set_root: request.write_set_root,
                    published_at_ms: request.synced_at_ms,
                })?;
            let publication_receipt = published_decision.receipt().clone();
            let refreshed_stage = local_store.inspect_staged_revision(staged_record.staging_id)?;
            Ok(ArtifactReplicaSyncOutcome::Synced {
                publication_receipt: Box::new(publication_receipt),
                staged_record: Box::new(refreshed_stage),
                is_tombstone,
            })
        }
    }
}

/// Content-addressed blob payload carried inside an [`ArtifactTransferPackage`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactBlobPayload {
    pub blob_digest: ContentDigest,
    pub bytes: Vec<u8>,
}

impl ArtifactBlobPayload {
    /// Constructs a payload addressed by the `SHA-256` digest of `bytes`.
    #[must_use]
    pub fn new(bytes: Vec<u8>) -> Self {
        let blob_digest = ContentDigest::of_bytes(&bytes);
        Self { blob_digest, bytes }
    }

    /// Recomputes `SHA-256` over `self.bytes` in memory and verifies it matches
    /// `self.blob_digest`.
    ///
    /// # Errors
    ///
    /// Returns [`ArtifactSyncError::TamperedBlobDigest`] on digest mismatch.
    pub fn verify_digest(&self) -> Result<(), ArtifactSyncError> {
        let computed = ContentDigest::of_bytes(&self.bytes);
        if computed != self.blob_digest {
            return Err(ArtifactSyncError::TamperedBlobDigest {
                declared: self.blob_digest,
                computed,
            });
        }
        Ok(())
    }
}

/// Optional logical deletion tombstone metadata carried by an
/// [`ArtifactTransferPackage`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArtifactTombstoneMetadata {
    /// Revision number acting as the logical deletion tombstone.
    pub tombstone_revision: u64,
    /// Origin cell deletion timestamp (milliseconds since Unix epoch).
    pub deleted_at_ms: u64,
    /// Optional retention barrier deadline (`now_ms <= retention_until_ms`
    /// retains blobs; strictly after `retention_until_ms` allows GC reclamation
    /// once active references drop to zero).
    pub retention_until_ms: Option<u64>,
}

/// One immutable revision metadata entry in an [`ArtifactTransferPackage`]'s
/// exported revision chain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TransferRevisionEntry {
    pub revision: u64,
    pub digest: ContentDigest,
    pub size_bytes: u64,
    pub created_at_ms: u64,
}

/// End-to-end cross-store artifact replica transfer package (`[DIST-ART-001]`).
///
/// Bundles the tip [`ReplicaRevisionBundle`], artifact creation metadata,
/// originating cell/epoch, optional [`ArtifactTombstoneMetadata`], the
/// contiguous revision chain, deduplicated content-addressed
/// [`ArtifactBlobPayload`]s, and a domain-separated `SHA-256` `package_digest`
/// (`"llmos/artifact-transfer-package/v1"`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactTransferPackage {
    pub bundle: ReplicaRevisionBundle,
    pub application_id: Option<ApplicationId>,
    pub owner: Option<String>,
    pub artifact_created_at_ms: u64,
    pub producer_epoch: u64,
    pub tombstone: Option<ArtifactTombstoneMetadata>,
    pub revisions: Vec<TransferRevisionEntry>,
    pub blobs: Vec<ArtifactBlobPayload>,
    pub package_digest: [u8; 32],
}

impl ArtifactTransferPackage {
    /// Recomputes the domain-separated `SHA-256` package digest over all
    /// metadata, lineage, revision entries, and content-addressed blob headers.
    #[must_use]
    pub fn compute_digest(&self) -> [u8; 32] {
        compute_transfer_package_digest(
            &self.bundle,
            self.application_id,
            self.owner.as_deref(),
            self.artifact_created_at_ms,
            self.producer_epoch,
            self.tombstone,
            &self.revisions,
            &self.blobs,
        )
    }
}

/// Computes the domain-separated `SHA-256` digest
/// ([`ARTIFACT_TRANSFER_PACKAGE_DOMAIN`]) for an [`ArtifactTransferPackage`].
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn compute_transfer_package_digest(
    bundle: &ReplicaRevisionBundle,
    application_id: Option<ApplicationId>,
    owner: Option<&str>,
    artifact_created_at_ms: u64,
    producer_epoch: u64,
    tombstone: Option<ArtifactTombstoneMetadata>,
    revisions: &[TransferRevisionEntry],
    blobs: &[ArtifactBlobPayload],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(ARTIFACT_TRANSFER_PACKAGE_DOMAIN);
    hasher.update(bundle.artifact_id.as_bytes());
    hash_length_prefixed(&mut hasher, bundle.content_type.as_bytes());
    match application_id {
        Some(app_id) => {
            hasher.update([1_u8]);
            hasher.update(app_id.as_bytes());
        }
        None => hasher.update([0_u8]),
    }
    match owner {
        Some(owner_str) => {
            hasher.update([1_u8]);
            hash_length_prefixed(&mut hasher, owner_str.as_bytes());
        }
        None => hasher.update([0_u8]),
    }
    hasher.update(artifact_created_at_ms.to_be_bytes());
    hasher.update(bundle.origin_cell_id);
    hasher.update(producer_epoch.to_be_bytes());
    hash_head(&mut hasher, bundle.remote_head);
    match bundle.parent_head {
        Some(parent) => {
            hasher.update([1_u8]);
            hash_head(&mut hasher, parent);
        }
        None => hasher.update([0_u8]),
    }
    hash_u64_len(&mut hasher, bundle.ancestor_lineage.len());
    for ancestor in &bundle.ancestor_lineage {
        hash_head(&mut hasher, *ancestor);
    }
    hasher.update([u8::from(bundle.is_tombstone)]);
    hash_tombstone(&mut hasher, tombstone);
    hash_u64_len(&mut hasher, bundle.bytes.len());
    hasher.update(bundle.remote_head.digest.as_bytes());
    hash_u64_len(&mut hasher, revisions.len());
    for rev in revisions {
        hasher.update(rev.revision.to_be_bytes());
        hasher.update(rev.digest.as_bytes());
        hasher.update(rev.size_bytes.to_be_bytes());
        hasher.update(rev.created_at_ms.to_be_bytes());
    }
    hash_u64_len(&mut hasher, blobs.len());
    for blob_payload in blobs {
        hasher.update(blob_payload.blob_digest.as_bytes());
        hash_u64_len(&mut hasher, blob_payload.bytes.len());
    }
    hasher.finalize().into()
}

fn hash_u64_len(hasher: &mut Sha256, len: usize) {
    let value = u64::try_from(len).unwrap_or(u64::MAX);
    hasher.update(value.to_be_bytes());
}

fn hash_length_prefixed(hasher: &mut Sha256, bytes: &[u8]) {
    hash_u64_len(hasher, bytes.len());
    hasher.update(bytes);
}

fn hash_head(hasher: &mut Sha256, head: HeadState) {
    hasher.update(head.revision.to_be_bytes());
    hasher.update(head.digest.as_bytes());
}

fn hash_tombstone(hasher: &mut Sha256, tombstone: Option<ArtifactTombstoneMetadata>) {
    match tombstone {
        Some(meta) => {
            hasher.update([1_u8]);
            hasher.update(meta.tombstone_revision.to_be_bytes());
            hasher.update(meta.deleted_at_ms.to_be_bytes());
            match meta.retention_until_ms {
                Some(until_ms) => {
                    hasher.update([1_u8]);
                    hasher.update(until_ms.to_be_bytes());
                }
                None => hasher.update([0_u8]),
            }
        }
        None => hasher.update([0_u8]),
    }
}

/// Request to export an [`ArtifactTransferPackage`] from an [`ArtifactStore`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ExportArtifactTransferRequest {
    pub artifact_id: ArtifactId,
    /// Optional upper revision bound; `None` exports up to the current head.
    pub target_revision: Option<u64>,
    pub origin_cell_id: [u8; 16],
    pub producer_epoch: u64,
    pub tombstone: Option<ArtifactTombstoneMetadata>,
    pub now_ms: u64,
}

impl ExportArtifactTransferRequest {
    /// Exports up to the current canonical head of `artifact_id`.
    #[must_use]
    pub const fn head(
        artifact_id: ArtifactId,
        origin_cell_id: [u8; 16],
        producer_epoch: u64,
        now_ms: u64,
    ) -> Self {
        Self {
            artifact_id,
            target_revision: None,
            origin_cell_id,
            producer_epoch,
            tombstone: None,
            now_ms,
        }
    }

    /// Exports the current canonical head of `artifact_id` as a logical
    /// deletion tombstone with an optional retention barrier window.
    #[must_use]
    pub const fn tombstone_head(
        artifact_id: ArtifactId,
        tombstone_revision: u64,
        origin_cell_id: [u8; 16],
        producer_epoch: u64,
        deleted_at_ms: u64,
        retention_until_ms: Option<u64>,
        now_ms: u64,
    ) -> Self {
        Self {
            artifact_id,
            target_revision: Some(tombstone_revision),
            origin_cell_id,
            producer_epoch,
            tombstone: Some(ArtifactTombstoneMetadata {
                tombstone_revision,
                deleted_at_ms,
                retention_until_ms,
            }),
            now_ms,
        }
    }
}

/// Exports the revision chain (`1..=target_revision`) and verified
/// content-addressed blob bytes from `store` into an [`ArtifactTransferPackage`].
///
/// # Errors
///
/// Returns [`ArtifactSyncError`] if `origin_cell_id` is zero, the artifact has
/// no committed revisions, tombstone metadata is inconsistent, or reading
/// verified revision bytes from `store` fails.
pub fn export_artifact_transfer_package(
    store: &ArtifactStore,
    request: ExportArtifactTransferRequest,
) -> Result<ArtifactTransferPackage, ArtifactSyncError> {
    if request.origin_cell_id == [0_u8; 16] {
        return Err(ArtifactSyncError::ZeroOriginCellId);
    }
    let artifact = store.inspect_artifact(request.artifact_id)?;
    let resolved_head = store.resolve_head(request.artifact_id, request.now_ms)?;
    let up_to_revision = match request.target_revision {
        Some(0) => return Err(ArtifactSyncError::InvalidRemoteRevision(0)),
        Some(revision) => revision,
        None => {
            resolved_head
                .ok_or(ArtifactSyncError::EmptyArtifactCannotExport(
                    request.artifact_id,
                ))?
                .revision
        }
    };

    if let Some(meta) = request.tombstone {
        if meta.tombstone_revision != up_to_revision {
            return Err(ArtifactSyncError::InvalidTombstoneMetadata(
                "tombstone_revision must match exported target revision",
            ));
        }
        if let Some(until_ms) = meta.retention_until_ms {
            if until_ms < meta.deleted_at_ms {
                return Err(ArtifactSyncError::InvalidTombstoneMetadata(
                    "retention_until_ms precedes deleted_at_ms",
                ));
            }
            if until_ms < artifact.created_at_ms {
                return Err(ArtifactSyncError::InvalidTombstoneMetadata(
                    "retention_until_ms precedes artifact created_at_ms",
                ));
            }
        }
    }

    let bundle = export_replica_revision_bundle(
        store,
        request.artifact_id,
        up_to_revision,
        request.origin_cell_id,
        request.tombstone.is_some(),
        request.now_ms,
    )?;

    let all_revisions = store.list_revisions(request.artifact_id)?;
    let mut revisions = Vec::new();
    let mut blob_by_digest = BTreeMap::<ContentDigest, Vec<u8>>::new();
    for rev in all_revisions {
        if rev.revision <= up_to_revision {
            let bytes = store.get_revision(request.artifact_id, rev.revision, request.now_ms)?;
            revisions.push(TransferRevisionEntry {
                revision: rev.revision,
                digest: rev.digest,
                size_bytes: rev.size_bytes,
                created_at_ms: rev.created_at_ms,
            });
            blob_by_digest.entry(rev.digest).or_insert(bytes);
        }
    }

    let blobs: Vec<ArtifactBlobPayload> = blob_by_digest
        .into_iter()
        .map(|(blob_digest, bytes)| ArtifactBlobPayload { blob_digest, bytes })
        .collect();

    let package_digest = compute_transfer_package_digest(
        &bundle,
        artifact.application_id,
        artifact.owner.as_deref(),
        artifact.created_at_ms,
        request.producer_epoch,
        request.tombstone,
        &revisions,
        &blobs,
    );

    Ok(ArtifactTransferPackage {
        bundle,
        application_id: artifact.application_id,
        owner: artifact.owner,
        artifact_created_at_ms: artifact.created_at_ms,
        producer_epoch: request.producer_epoch,
        tombstone: request.tombstone,
        revisions,
        blobs,
        package_digest,
    })
}

/// Request to verify and import an [`ArtifactTransferPackage`] into a target
/// [`ArtifactStore`].
#[derive(Clone, Copy, Debug)]
pub struct ImportArtifactTransferRequest<'a> {
    pub package: &'a ArtifactTransferPackage,
    /// Optional caller-observed [`ArtifactLocalSyncState`] CAS expectation;
    /// when `Some`, verified against `target_store`'s durable head before any
    /// write occurs.
    pub expected_local_state: Option<&'a ArtifactLocalSyncState>,
    pub task_id: TaskId,
    pub permit_id: CommitPermitId,
    pub write_set_root: ContentDigest,
    pub idempotency_key: IdempotencyKey,
    pub synced_at_ms: u64,
}

/// End-to-end outcome of [`import_artifact_transfer_package`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CrossStoreArtifactSyncReport {
    /// Verified missing blobs and contiguous revision metadata were materialized
    /// into `target_store`, advancing the canonical head.
    ImportedFastForward {
        artifact_id: ArtifactId,
        prior_local_head: Option<HeadState>,
        new_head: HeadState,
        imported_revisions: Vec<u64>,
        materialized_blob_digests: Vec<ContentDigest>,
        publication_receipts: Vec<ArtifactPublicationReceipt>,
    },
    /// `target_store` already had the identical `(revision, digest)` at head;
    /// import replayed idempotently with zero state mutation.
    NoOpConverged {
        artifact_id: ArtifactId,
        head: HeadState,
    },
    /// Concurrent divergent edit detected (`QuarantineDivergentBranch` or
    /// `FailClosedConflict`): `target_store`'s canonical head stayed unchanged,
    /// verified incoming blobs were materialized on disk, and both competing
    /// heads were pinned in [`ReplicaSyncRetentionSet`].
    ConflictIsolated {
        conflict_receipt: ArtifactConflictReceipt,
        preserved_both_blobs: bool,
        pinned_digests: Vec<ContentDigest>,
    },
    /// Verified deletion tombstone revision chain was materialized into
    /// `target_store` and its retention barrier was registered in
    /// [`ReplicaSyncRetentionSet`].
    TombstoneApplied {
        artifact_id: ArtifactId,
        prior_local_head: Option<HeadState>,
        tombstone_head: HeadState,
        retention_until_ms: Option<u64>,
        publication_receipts: Vec<ArtifactPublicationReceipt>,
    },
}

/// Quarantined conflict entry pinned in [`ReplicaSyncRetentionSet`] until
/// explicit conflict resolution.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct QuarantinedConflictPin {
    pub conflict_receipt: ArtifactConflictReceipt,
    pub local_head: HeadState,
    pub remote_head: HeadState,
    pub pinned_digests: BTreeSet<ContentDigest>,
    pub pinned_at_ms: u64,
}

/// Tombstone retention entry pinned in [`ReplicaSyncRetentionSet`] until
/// `retention_until_ms` expires.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TombstoneRetentionPin {
    pub artifact_id: ArtifactId,
    pub content_type: String,
    pub tombstone_head: HeadState,
    pub pinned_digests: BTreeSet<ContentDigest>,
    pub deleted_at_ms: u64,
    /// Retained while `now_ms <= retention_until_ms`; unpinned strictly after
    /// `retention_until_ms` (`now_ms > retention_until_ms`). `None` retains
    /// indefinitely.
    pub retention_until_ms: Option<u64>,
}

/// Sync-root retention guard protecting quarantined competing conflict blobs
/// and active tombstone-retained blobs from premature orphan GC deletion.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ReplicaSyncRetentionSet {
    quarantined_conflicts: BTreeMap<ContentDigest, QuarantinedConflictPin>,
    tombstones: BTreeMap<(ArtifactId, u64), TombstoneRetentionPin>,
}

impl ReplicaSyncRetentionSet {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.quarantined_conflicts.is_empty() && self.tombstones.is_empty()
    }

    /// Pins both `receipt.local_head.digest` and `receipt.remote_head.digest`
    /// (plus any `extra_digests`) under `receipt.conflict_digest` until
    /// [`Self::resolve_conflict`] or [`Self::retire_conflict_into_tombstone_retention`]
    /// is invoked.
    pub fn pin_conflict(
        &mut self,
        receipt: ArtifactConflictReceipt,
        extra_digests: impl IntoIterator<Item = ContentDigest>,
        pinned_at_ms: u64,
    ) -> &QuarantinedConflictPin {
        let key = receipt.conflict_digest;
        let entry = self.quarantined_conflicts.entry(key).or_insert_with(|| {
            let mut pinned_digests = BTreeSet::new();
            pinned_digests.insert(receipt.local_head.digest);
            pinned_digests.insert(receipt.remote_head.digest);
            QuarantinedConflictPin {
                local_head: receipt.local_head,
                remote_head: receipt.remote_head,
                conflict_receipt: receipt,
                pinned_digests,
                pinned_at_ms,
            }
        });
        entry.pinned_digests.extend(extra_digests);
        entry
    }

    /// Resolves and unpins a quarantined conflict by `conflict_digest`.
    pub fn resolve_conflict(
        &mut self,
        conflict_digest: ContentDigest,
    ) -> Option<QuarantinedConflictPin> {
        self.quarantined_conflicts.remove(&conflict_digest)
    }

    /// Retires an isolated conflict into a bounded tombstone retention window
    /// (`retention_until_ms`) so superseded competing blobs stay audit-readable
    /// until expiry and become GC-collectible strictly after `retention_until_ms`.
    ///
    /// # Errors
    ///
    /// Returns [`ArtifactSyncError::SyncBlobNotPinned`] if `conflict_digest` is
    /// not an active conflict, or [`ArtifactSyncError::InvalidTombstoneMetadata`]
    /// if `retention_until_ms` precedes `deleted_at_ms`.
    pub fn retire_conflict_into_tombstone_retention(
        &mut self,
        conflict_digest: ContentDigest,
        deleted_at_ms: u64,
        retention_until_ms: Option<u64>,
    ) -> Result<TombstoneRetentionPin, ArtifactSyncError> {
        if let Some(until_ms) = retention_until_ms
            && until_ms < deleted_at_ms
        {
            return Err(ArtifactSyncError::InvalidTombstoneMetadata(
                "retention_until_ms precedes deleted_at_ms",
            ));
        }
        let conflict = self
            .quarantined_conflicts
            .remove(&conflict_digest)
            .ok_or(ArtifactSyncError::SyncBlobNotPinned(conflict_digest))?;
        let tombstone_pin = TombstoneRetentionPin {
            artifact_id: conflict.conflict_receipt.artifact_id,
            content_type: conflict.conflict_receipt.content_type,
            tombstone_head: conflict.remote_head,
            pinned_digests: conflict.pinned_digests,
            deleted_at_ms,
            retention_until_ms,
        };
        self.pin_tombstone(tombstone_pin.clone());
        Ok(tombstone_pin)
    }

    #[must_use]
    pub fn inspect_conflict(
        &self,
        conflict_digest: ContentDigest,
    ) -> Option<&QuarantinedConflictPin> {
        self.quarantined_conflicts.get(&conflict_digest)
    }

    pub fn active_conflicts(&self) -> impl Iterator<Item = &QuarantinedConflictPin> {
        self.quarantined_conflicts.values()
    }

    #[must_use]
    pub fn active_conflict_count_for(&self, artifact_id: ArtifactId) -> u64 {
        let count = self
            .quarantined_conflicts
            .values()
            .filter(|pin| pin.conflict_receipt.artifact_id == artifact_id)
            .count();
        u64::try_from(count).unwrap_or(u64::MAX)
    }

    /// Registers or merges a tombstone retention pin.
    pub fn pin_tombstone(&mut self, pin: TombstoneRetentionPin) {
        let key = (pin.artifact_id, pin.tombstone_head.revision);
        match self.tombstones.get_mut(&key) {
            Some(existing) => {
                existing.pinned_digests.extend(pin.pinned_digests);
                existing.retention_until_ms =
                    match (existing.retention_until_ms, pin.retention_until_ms) {
                        (None, _) | (_, None) => None,
                        (Some(left), Some(right)) => Some(left.max(right)),
                    };
            }
            None => {
                self.tombstones.insert(key, pin);
            }
        }
    }

    /// Lists tombstone pins whose retention window is still active at `now_ms`
    /// (`now_ms <= retention_until_ms`, or unbounded `None`).
    #[must_use]
    pub fn active_tombstones(&self, now_ms: u64) -> Vec<&TombstoneRetentionPin> {
        self.tombstones
            .values()
            .filter(|pin| {
                pin.retention_until_ms
                    .is_none_or(|until_ms| now_ms <= until_ms)
            })
            .collect()
    }

    /// Removes and returns tombstone pins whose retention window has expired at
    /// `now_ms` (`now_ms > retention_until_ms`).
    pub fn prune_expired_tombstones(&mut self, now_ms: u64) -> Vec<TombstoneRetentionPin> {
        let expired_keys: Vec<(ArtifactId, u64)> = self
            .tombstones
            .iter()
            .filter_map(|(key, pin)| {
                pin.retention_until_ms
                    .is_some_and(|until_ms| now_ms > until_ms)
                    .then_some(*key)
            })
            .collect();
        let mut removed = Vec::with_capacity(expired_keys.len());
        for key in expired_keys {
            if let Some(pin) = self.tombstones.remove(&key) {
                removed.push(pin);
            }
        }
        removed
    }

    /// Computes all content digests pinned against orphan GC at `now_ms`:
    /// every unresolved conflict digest plus every non-expired tombstone digest.
    #[must_use]
    pub fn active_pinned_digests(&self, now_ms: u64) -> BTreeSet<ContentDigest> {
        let mut pinned = BTreeSet::new();
        for conflict in self.quarantined_conflicts.values() {
            pinned.extend(conflict.pinned_digests.iter().copied());
        }
        for tombstone in self.active_tombstones(now_ms) {
            pinned.extend(tombstone.pinned_digests.iter().copied());
        }
        pinned
    }

    #[must_use]
    pub fn is_digest_pinned(&self, digest: ContentDigest, now_ms: u64) -> bool {
        self.active_pinned_digests(now_ms).contains(&digest)
    }

    /// Evaluates `[DIST-ART-001]` physical GC reclamation eligibility for a
    /// tombstoned artifact at `now_ms`, combining tombstone window expiry and
    /// unresolved conflict reference counts.
    ///
    /// # Errors
    ///
    /// Returns [`ArtifactSyncError::TombstoneNotTracked`] if `artifact_id` has
    /// no tombstone entry, or the policy/barrier error from
    /// [`evaluate_artifact_sync`].
    pub fn evaluate_tombstone_reclamation(
        &self,
        registry: &ArtifactSyncPolicyRegistry,
        artifact_id: ArtifactId,
        origin_cell_id: [u8; 16],
        now_ms: u64,
        external_active_ref_count: u64,
    ) -> Result<ArtifactSyncDecision, ArtifactSyncError> {
        let matching: Vec<&TombstoneRetentionPin> = self
            .tombstones
            .values()
            .filter(|pin| pin.artifact_id == artifact_id)
            .collect();
        let latest = matching
            .last()
            .copied()
            .ok_or(ArtifactSyncError::TombstoneNotTracked(artifact_id))?;

        let retention_expired = matching.iter().all(|pin| {
            pin.retention_until_ms
                .is_some_and(|until_ms| now_ms > until_ms)
        });
        let active_ref_count = self
            .active_conflict_count_for(artifact_id)
            .saturating_add(external_active_ref_count);

        let local_state = ArtifactLocalSyncState {
            artifact_id,
            content_type: latest.content_type.clone(),
            local_head: Some(latest.tombstone_head),
        };
        let candidate = ArtifactRemoteSyncCandidate::physical_gc_reclamation(
            origin_cell_id,
            retention_expired,
            active_ref_count,
        );
        evaluate_artifact_sync(registry, &local_state, &candidate)
    }
}

/// Verifies an [`ArtifactTransferPackage`] in memory, evaluates the mandatory
/// `[DIST-ART-001]` 4-axis policy in `registry`, and materializes verified
/// blobs and revisions into `target_store` while updating `retention_set`.
///
/// # Fail-Closed Order
///
/// 1. Verifies `origin_cell_id`, `registry` policy, every
///    [`ArtifactBlobPayload`]'s `SHA-256` digest, `package_digest`, revision
///    chain contiguity, and tombstone invariants purely in memory (zero writes
///    on corruption or tamper).
/// 2. Inspects `target_store` local state (enforcing `expected_local_state` CAS
///    when provided) and evaluates [`evaluate_artifact_sync`] before touching
///    disk or `SQLite`.
/// 3. Materializes verified blobs/revisions or isolates competing conflict
///    blobs into `target_store` and pins roots in `retention_set`.
///
/// # Errors
///
/// Returns [`ArtifactSyncError`] on tamper, policy failure, CAS drift,
/// non-contiguous revision chain, or storage error.
pub fn import_artifact_transfer_package(
    target_store: &ArtifactStore,
    registry: &ArtifactSyncPolicyRegistry,
    retention_set: &mut ReplicaSyncRetentionSet,
    request: ImportArtifactTransferRequest<'_>,
) -> Result<CrossStoreArtifactSyncReport, ArtifactSyncError> {
    let package = request.package;
    let blob_map = verify_transfer_package_in_memory(registry, package)?;
    let target_state = resolve_target_local_sync_state(
        target_store,
        package,
        request.expected_local_state,
        request.synced_at_ms,
    )?;

    let local_state = ArtifactLocalSyncState {
        artifact_id: package.bundle.artifact_id,
        content_type: package.bundle.content_type.clone(),
        local_head: target_state.local_head,
    };
    let candidate = if package.bundle.is_tombstone {
        ArtifactRemoteSyncCandidate::tombstone_deletion(
            package.bundle.origin_cell_id,
            package.bundle.remote_head,
            package.bundle.parent_head,
            package.bundle.ancestor_lineage.clone(),
        )
    } else {
        ArtifactRemoteSyncCandidate::revision_update(
            package.bundle.origin_cell_id,
            package.bundle.remote_head,
            package.bundle.parent_head,
            package.bundle.ancestor_lineage.clone(),
        )
    };

    let decision = evaluate_artifact_sync(registry, &local_state, &candidate)?;
    match decision {
        ArtifactSyncDecision::NoOpAlreadySynced { artifact_id, head } => {
            if let Some(meta) = package.tombstone {
                retention_set.pin_tombstone(TombstoneRetentionPin {
                    artifact_id,
                    content_type: package.bundle.content_type.clone(),
                    tombstone_head: head,
                    pinned_digests: package.blobs.iter().map(|b| b.blob_digest).collect(),
                    deleted_at_ms: meta.deleted_at_ms,
                    retention_until_ms: meta.retention_until_ms,
                });
            }
            Ok(CrossStoreArtifactSyncReport::NoOpConverged { artifact_id, head })
        }
        ArtifactSyncDecision::RefuseConflict(conflict_receipt)
        | ArtifactSyncDecision::QuarantineConflict(conflict_receipt) => {
            isolate_conflicting_package_blobs(
                target_store,
                retention_set,
                package,
                conflict_receipt,
                request.synced_at_ms,
            )
        }
        ArtifactSyncDecision::ReclaimEligible { .. } => {
            unreachable!("revision/tombstone import never produces ReclaimEligible")
        }
        ArtifactSyncDecision::AcceptRemoteHead {
            prior_local_head,
            new_head,
            is_tombstone,
            ..
        } => materialize_accepted_transfer_package(
            target_store,
            retention_set,
            &request,
            &blob_map,
            &target_state,
            prior_local_head,
            new_head,
            is_tombstone,
        ),
    }
}

fn verify_transfer_package_in_memory<'a>(
    registry: &ArtifactSyncPolicyRegistry,
    package: &'a ArtifactTransferPackage,
) -> Result<BTreeMap<ContentDigest, &'a [u8]>, ArtifactSyncError> {
    if package.bundle.origin_cell_id == [0_u8; 16] {
        return Err(ArtifactSyncError::ZeroOriginCellId);
    }
    registry.resolve(&package.bundle.content_type)?;
    if let Some(owner) = &package.owner {
        validate_text_component("owner", owner)?;
    }
    if package.bundle.remote_head.revision == 0 {
        return Err(ArtifactSyncError::InvalidRemoteRevision(0));
    }

    // Step 1: Verify every content-addressed blob payload and tip bytes SHA-256
    // BEFORE checking package_digest so any byte corruption surfaces cleanly as
    // TamperedBlobDigest with zero durable writes.
    let mut blob_map = BTreeMap::<ContentDigest, &'a [u8]>::new();
    for payload in &package.blobs {
        payload.verify_digest()?;
        if let Some(existing) = blob_map.insert(payload.blob_digest, payload.bytes.as_slice())
            && existing != payload.bytes.as_slice()
        {
            return Err(ArtifactSyncError::InvalidLineage(
                "duplicate blob_digest with differing payload bytes",
            ));
        }
    }
    let tip_computed = ContentDigest::of_bytes(&package.bundle.bytes);
    if tip_computed != package.bundle.remote_head.digest {
        return Err(ArtifactSyncError::TamperedBlobDigest {
            declared: package.bundle.remote_head.digest,
            computed: tip_computed,
        });
    }

    // Step 2: Verify domain-separated package digest ("llmos/artifact-transfer-package/v1").
    let computed_pkg_digest = package.compute_digest();
    if computed_pkg_digest != package.package_digest {
        return Err(ArtifactSyncError::TamperedPackageDigest {
            expected: package.package_digest,
            actual: computed_pkg_digest,
        });
    }

    // Step 3: Verify lineage, revision entries, and tombstone invariants.
    validate_remote_lineage(
        package.bundle.remote_head,
        package.bundle.parent_head,
        &package.bundle.ancestor_lineage,
    )?;
    validate_package_revisions_and_tombstone(package, &blob_map)?;
    Ok(blob_map)
}

fn validate_package_revisions_and_tombstone(
    package: &ArtifactTransferPackage,
    blob_map: &BTreeMap<ContentDigest, &[u8]>,
) -> Result<(), ArtifactSyncError> {
    if package.bundle.is_tombstone != package.tombstone.is_some() {
        return Err(ArtifactSyncError::InvalidTombstoneMetadata(
            "bundle.is_tombstone disagrees with tombstone metadata presence",
        ));
    }
    if let Some(meta) = package.tombstone {
        if meta.tombstone_revision != package.bundle.remote_head.revision {
            return Err(ArtifactSyncError::InvalidTombstoneMetadata(
                "tombstone_revision disagrees with remote_head.revision",
            ));
        }
        if let Some(until_ms) = meta.retention_until_ms
            && until_ms < meta.deleted_at_ms
        {
            return Err(ArtifactSyncError::InvalidTombstoneMetadata(
                "retention_until_ms precedes deleted_at_ms",
            ));
        }
    }

    if package.revisions.is_empty() {
        return Err(ArtifactSyncError::InvalidLineage(
            "transfer package revisions cannot be empty",
        ));
    }
    let mut prev_rev: Option<u64> = None;
    for entry in &package.revisions {
        if entry.revision == 0 {
            return Err(ArtifactSyncError::InvalidRemoteRevision(0));
        }
        if let Some(prev) = prev_rev
            && entry.revision != prev.saturating_add(1)
        {
            return Err(ArtifactSyncError::InvalidLineage(
                "transfer package revisions must be strictly contiguous",
            ));
        }
        let Some(bytes) = blob_map.get(&entry.digest) else {
            return Err(ArtifactSyncError::MissingBlobInTransferPackage {
                revision: entry.revision,
                digest: entry.digest,
            });
        };
        if u64::try_from(bytes.len()).ok() != Some(entry.size_bytes) {
            return Err(ArtifactSyncError::InvalidLineage(
                "revision size_bytes disagrees with verified blob payload length",
            ));
        }
        prev_rev = Some(entry.revision);
    }

    let tip = package
        .revisions
        .last()
        .expect("revisions checked non-empty");
    if tip.revision != package.bundle.remote_head.revision
        || tip.digest != package.bundle.remote_head.digest
    {
        return Err(ArtifactSyncError::InvalidLineage(
            "transfer package tip revision disagrees with bundle.remote_head",
        ));
    }
    Ok(())
}

struct TargetLocalResolution {
    existing_artifact: Option<ArtifactRecord>,
    local_head: Option<HeadState>,
}

fn resolve_target_local_sync_state(
    target_store: &ArtifactStore,
    package: &ArtifactTransferPackage,
    expected_local_state: Option<&ArtifactLocalSyncState>,
    synced_at_ms: u64,
) -> Result<TargetLocalResolution, ArtifactSyncError> {
    let artifact_id = package.bundle.artifact_id;
    let (existing_artifact, local_head) = match target_store.inspect_artifact(artifact_id) {
        Ok(record) => {
            if record.content_type != package.bundle.content_type {
                return Err(ArtifactSyncError::ContentTypeMismatch {
                    expected: record.content_type,
                    actual: package.bundle.content_type.clone(),
                });
            }
            let durable_head = match (record.head_revision, record.head_digest) {
                (0, None) => None,
                (revision, Some(digest)) => Some(HeadState { revision, digest }),
                _ => {
                    return Err(ArtifactSyncError::Store(Box::new(
                        ArtifactError::CorruptRecord("head revision and head digest disagree"),
                    )));
                }
            };
            // Durable replay precedence: if target_store already committed the
            // exact remote head, replay without failing on post-tombstone expiry.
            let head = if durable_head == Some(package.bundle.remote_head) {
                durable_head
            } else {
                target_store.resolve_head(artifact_id, synced_at_ms)?
            };
            (Some(record), head)
        }
        Err(ArtifactError::ArtifactNotFound(_)) => (None, None),
        Err(error) => return Err(ArtifactSyncError::Store(Box::new(error))),
    };

    if let Some(expected) = expected_local_state {
        if expected.artifact_id != artifact_id {
            return Err(ArtifactSyncError::InvalidLineage(
                "expected_local_state artifact_id disagrees with package",
            ));
        }
        if expected.content_type != package.bundle.content_type {
            return Err(ArtifactSyncError::ContentTypeMismatch {
                expected: expected.content_type.clone(),
                actual: package.bundle.content_type.clone(),
            });
        }
        if expected.local_head != local_head {
            return Err(ArtifactSyncError::LocalHeadStateDrift {
                expected: expected.local_head,
                actual: local_head,
            });
        }
    }

    let effective_created_at_ms = existing_artifact
        .as_ref()
        .map_or(package.artifact_created_at_ms, |rec| rec.created_at_ms);
    if let Some(meta) = package.tombstone
        && let Some(until_ms) = meta.retention_until_ms
    {
        if until_ms < effective_created_at_ms {
            return Err(ArtifactSyncError::InvalidTombstoneMetadata(
                "tombstone retention_until_ms precedes target artifact created_at_ms",
            ));
        }
        if i64::try_from(until_ms - effective_created_at_ms).is_err() {
            return Err(ArtifactSyncError::InvalidTombstoneMetadata(
                "tombstone retention window exceeds SQLite INTEGER",
            ));
        }
    }

    Ok(TargetLocalResolution {
        existing_artifact,
        local_head,
    })
}

fn isolate_conflicting_package_blobs(
    target_store: &ArtifactStore,
    retention_set: &mut ReplicaSyncRetentionSet,
    package: &ArtifactTransferPackage,
    conflict_receipt: ArtifactConflictReceipt,
    synced_at_ms: u64,
) -> Result<CrossStoreArtifactSyncReport, ArtifactSyncError> {
    let guard = target_store.lock_connection()?;
    let local_blob_path = target_store
        .paths()
        .artifacts
        .blob_path(conflict_receipt.local_head.digest);
    blob::read_blob_verified(
        &target_store.paths().artifacts,
        conflict_receipt.local_head.digest,
    )?
    .ok_or(ArtifactError::BlobMissing {
        artifact_id: conflict_receipt.artifact_id,
        revision: conflict_receipt.local_head.revision,
        digest: conflict_receipt.local_head.digest,
        path: local_blob_path,
    })?;

    for payload in &package.blobs {
        blob::commit_blob(
            &target_store.paths().artifacts,
            payload.blob_digest,
            &payload.bytes,
        )?;
    }

    let pin = retention_set.pin_conflict(
        conflict_receipt.clone(),
        package.blobs.iter().map(|b| b.blob_digest),
        synced_at_ms,
    );
    let pinned_digests: Vec<ContentDigest> = pin.pinned_digests.iter().copied().collect();
    drop(guard);

    Ok(CrossStoreArtifactSyncReport::ConflictIsolated {
        conflict_receipt,
        preserved_both_blobs: true,
        pinned_digests,
    })
}

#[allow(clippy::too_many_arguments)]
fn materialize_accepted_transfer_package(
    target_store: &ArtifactStore,
    retention_set: &mut ReplicaSyncRetentionSet,
    request: &ImportArtifactTransferRequest<'_>,
    blob_map: &BTreeMap<ContentDigest, &[u8]>,
    target_state: &TargetLocalResolution,
    prior_local_head: Option<HeadState>,
    new_head: HeadState,
    is_tombstone: bool,
) -> Result<CrossStoreArtifactSyncReport, ArtifactSyncError> {
    let package = request.package;
    let artifact_id = package.bundle.artifact_id;
    let start_rev = prior_local_head.map_or(0, |head| head.revision);
    if new_head.revision <= start_rev {
        return Err(ArtifactSyncError::Store(Box::new(
            ArtifactError::RevisionConflict {
                artifact_id,
                revision: new_head.revision,
            },
        )));
    }

    let rev_lookup: BTreeMap<u64, &TransferRevisionEntry> = package
        .revisions
        .iter()
        .map(|entry| (entry.revision, entry))
        .collect();
    for needed_rev in (start_rev + 1)..=new_head.revision {
        if !rev_lookup.contains_key(&needed_rev) {
            return Err(ArtifactSyncError::NonContiguousRevision {
                expected_revision: needed_rev,
                remote_revision: new_head.revision,
            });
        }
    }

    if target_state.existing_artifact.is_none() {
        let create_key = derive_sync_idempotency_key(
            b"llmos/artifact-transfer-create/v1",
            request.idempotency_key,
            artifact_id,
            0,
        );
        target_store.create_artifact(CreateArtifactSpec {
            artifact_id,
            idempotency_key: create_key,
            content_type: package.bundle.content_type.clone(),
            application_id: package.application_id,
            owner: package.owner.clone(),
            created_at_ms: package.artifact_created_at_ms,
        })?;
    }

    let mut imported_revisions = Vec::new();
    let mut materialized_set = BTreeSet::new();
    let mut publication_receipts = Vec::new();

    for rev_num in (start_rev + 1)..=new_head.revision {
        let rev_entry = rev_lookup[&rev_num];
        let bytes = blob_map[&rev_entry.digest];
        let published = stage_and_publish_transfer_revision(
            target_store,
            request,
            artifact_id,
            rev_num,
            bytes,
        )?;
        imported_revisions.push(rev_num);
        materialized_set.insert(rev_entry.digest);
        publication_receipts.push(published);
    }

    if is_tombstone {
        let meta = apply_tombstone_retention_barrier(
            target_store,
            retention_set,
            package,
            artifact_id,
            new_head,
        )?;
        return Ok(CrossStoreArtifactSyncReport::TombstoneApplied {
            artifact_id,
            prior_local_head,
            tombstone_head: new_head,
            retention_until_ms: meta.retention_until_ms,
            publication_receipts,
        });
    }

    Ok(CrossStoreArtifactSyncReport::ImportedFastForward {
        artifact_id,
        prior_local_head,
        new_head,
        imported_revisions,
        materialized_blob_digests: materialized_set.into_iter().collect(),
        publication_receipts,
    })
}

fn stage_and_publish_transfer_revision(
    target_store: &ArtifactStore,
    request: &ImportArtifactTransferRequest<'_>,
    artifact_id: ArtifactId,
    rev_num: u64,
    bytes: &[u8],
) -> Result<ArtifactPublicationReceipt, ArtifactSyncError> {
    let stage_key = derive_sync_idempotency_key(
        b"llmos/artifact-transfer-stage/v1",
        request.idempotency_key,
        artifact_id,
        rev_num,
    );
    let staged = target_store
        .stage_revision(StageRevisionRequest {
            artifact_id,
            expected_head_revision: rev_num - 1,
            bytes,
            task_id: request.task_id,
            permit_id: request.permit_id,
            write_set_root: request.write_set_root,
            idempotency_key: stage_key,
            created_at_ms: request.synced_at_ms,
        })?
        .record()
        .clone();
    let published = target_store
        .publish_staged_revision(PublishStagedRevisionRequest {
            staging_id: staged.staging_id,
            task_id: request.task_id,
            permit_id: request.permit_id,
            write_set_root: request.write_set_root,
            published_at_ms: request.synced_at_ms,
        })?
        .receipt()
        .clone();
    Ok(published)
}

fn apply_tombstone_retention_barrier(
    target_store: &ArtifactStore,
    retention_set: &mut ReplicaSyncRetentionSet,
    package: &ArtifactTransferPackage,
    artifact_id: ArtifactId,
    new_head: HeadState,
) -> Result<ArtifactTombstoneMetadata, ArtifactSyncError> {
    let meta = package
        .tombstone
        .expect("tombstone metadata validated in memory");
    if let Some(until_ms) = meta.retention_until_ms {
        let durable = target_store.inspect_artifact(artifact_id)?;
        let retention_ms = until_ms.saturating_sub(durable.created_at_ms);
        target_store.set_retention(SetRetentionRequest {
            artifact_id,
            retention_ms,
        })?;
    }
    retention_set.pin_tombstone(TombstoneRetentionPin {
        artifact_id,
        content_type: package.bundle.content_type.clone(),
        tombstone_head: new_head,
        pinned_digests: package.blobs.iter().map(|b| b.blob_digest).collect(),
        deleted_at_ms: meta.deleted_at_ms,
        retention_until_ms: meta.retention_until_ms,
    });
    Ok(meta)
}

fn derive_sync_idempotency_key(
    domain: &[u8],
    base_key: IdempotencyKey,
    artifact_id: ArtifactId,
    revision: u64,
) -> IdempotencyKey {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update(base_key.as_bytes());
    hasher.update(artifact_id.as_bytes());
    hasher.update(revision.to_be_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    IdempotencyKey::from_bytes(bytes)
}

/// Reads and `SHA-256`-verifies a sync-pinned blob (such as an isolated
/// competing conflict branch or an active tombstone-retained blob) from `store`
/// at `now_ms`.
///
/// # Errors
///
/// Fails closed with [`ArtifactSyncError::SyncBlobNotPinned`] if `digest` is
/// not actively pinned in `retention_set` at `now_ms`, or a typed blob error if
/// missing/corrupted.
pub fn read_retained_sync_blob(
    store: &ArtifactStore,
    retention_set: &ReplicaSyncRetentionSet,
    digest: ContentDigest,
    now_ms: u64,
) -> Result<Vec<u8>, ArtifactSyncError> {
    if !retention_set.is_digest_pinned(digest, now_ms) {
        return Err(ArtifactSyncError::SyncBlobNotPinned(digest));
    }
    let guard = store.lock_connection()?;
    let path = store.paths().artifacts.blob_path(digest);
    let bytes = blob::read_blob_verified(&store.paths().artifacts, digest)?.ok_or(
        ArtifactError::BlobMissing {
            artifact_id: ArtifactId::from_bytes([0_u8; 16]),
            revision: 0,
            digest,
            path,
        },
    )?;
    drop(guard);
    Ok(bytes)
}

/// Sync-aware explicit orphan blob garbage collector: removes only artifact
/// blobs referenced by neither durable `SQLite` metadata nor active pins in
/// `retention_set` at `request.collected_at_ms`.
///
/// Serialized by the same process-local writer mutex and `BEGIN IMMEDIATE`
/// transaction as [`ArtifactStore::collect_orphan_blobs`], recording an
/// immutable [`GcReceipt`] in `artifact_gc_receipts` inspectable via
/// [`ArtifactStore::inspect_gc_receipt`].
///
/// # Errors
///
/// Returns [`ArtifactError`] on storage or blob removal failure.
pub fn collect_orphan_blobs_with_sync_roots(
    store: &ArtifactStore,
    request: CollectOrphanBlobsRequest,
    retention_set: &ReplicaSyncRetentionSet,
) -> Result<CollectOrphanBlobsDecision, ArtifactError> {
    let mut connection = store.lock_connection()?;
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    if let Some(existing) = load_gc_receipt_by_key(&transaction, request.idempotency_key)? {
        transaction.commit()?;
        return Ok(CollectOrphanBlobsDecision::Replayed(existing));
    }

    let mut referenced = load_sync_gc_referenced_digests(&transaction)?;
    referenced.extend(retention_set.active_pinned_digests(request.collected_at_ms));

    let scan = blob::scan_blobs(&store.paths().artifacts.blobs)?;
    let scanned_blob_count = u64::try_from(scan.present.len()).unwrap_or(u64::MAX);
    let mut orphans: Vec<ContentDigest> = scan
        .present
        .into_iter()
        .filter(|digest| !referenced.contains(digest))
        .collect();
    orphans.sort();

    for digest in &orphans {
        blob::remove_blob(&store.paths().artifacts, *digest)?;
    }

    let receipt = GcReceipt {
        receipt_id: derive_gc_receipt_id(request.idempotency_key, &orphans),
        collected_count: u64::try_from(orphans.len()).unwrap_or(u64::MAX),
        collected_digests: orphans,
        scanned_blob_count,
        created_at_ms: request.collected_at_ms,
    };
    insert_gc_receipt(&transaction, &receipt, request.idempotency_key)?;
    transaction.commit()?;
    Ok(CollectOrphanBlobsDecision::Collected(receipt))
}

fn load_sync_gc_referenced_digests(
    source: &impl SqlRead,
) -> Result<HashSet<ContentDigest>, ArtifactError> {
    let mut referenced: HashSet<ContentDigest> = load_all_revision_digests(source)?
        .into_iter()
        .map(|(_, _, digest)| digest)
        .collect();
    let mut staged_stmt =
        source.prepare_statement("SELECT digest FROM artifact_staged_revisions")?;
    let mut staged_rows = staged_stmt.query([])?;
    while let Some(row) = staged_rows.next()? {
        referenced.insert(ContentDigest::from_bytes(sync_blob_n(row, 0)?));
    }
    let mut head_stmt = source
        .prepare_statement("SELECT head_digest FROM artifacts WHERE head_digest IS NOT NULL")?;
    let mut head_rows = head_stmt.query([])?;
    while let Some(row) = head_rows.next()? {
        referenced.insert(ContentDigest::from_bytes(sync_blob_n(row, 0)?));
    }
    Ok(referenced)
}

fn derive_gc_receipt_id(key: IdempotencyKey, collected: &[ContentDigest]) -> ReceiptId {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/artifact-gc-receipt/v1");
    hasher.update(key.as_bytes());
    for digest in collected {
        hasher.update(digest.as_bytes());
    }
    let digest = hasher.finalize();
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    ReceiptId::from_bytes(bytes)
}

fn insert_gc_receipt(
    transaction: &rusqlite::Transaction<'_>,
    receipt: &GcReceipt,
    idempotency_key: IdempotencyKey,
) -> Result<(), ArtifactError> {
    let mut packed = Vec::with_capacity(receipt.collected_digests.len() * 32);
    for digest in &receipt.collected_digests {
        packed.extend_from_slice(digest.as_bytes());
    }
    transaction.execute(
        "INSERT INTO artifact_gc_receipts (
            receipt_id, idempotency_key, collected_digests,
            collected_count, scanned_blob_count, created_at_ms
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            receipt.receipt_id.as_bytes().as_slice(),
            idempotency_key.as_bytes().as_slice(),
            packed,
            encode_u64(receipt.collected_count)?,
            encode_u64(receipt.scanned_blob_count)?,
            encode_u64(receipt.created_at_ms)?,
        ],
    )?;
    Ok(())
}

fn load_gc_receipt_by_key(
    source: &impl SqlRead,
    idempotency_key: IdempotencyKey,
) -> Result<Option<GcReceipt>, ArtifactError> {
    let mut statement = source.prepare_statement(
        "SELECT receipt_id, collected_digests, collected_count,
                scanned_blob_count, created_at_ms
         FROM artifact_gc_receipts WHERE idempotency_key = ?1",
    )?;
    let mut rows = statement.query([idempotency_key.as_bytes().as_slice()])?;
    rows.next()?.map(decode_gc_receipt_row).transpose()
}

fn decode_gc_receipt_row(row: &Row<'_>) -> Result<GcReceipt, ArtifactError> {
    let receipt_id = ReceiptId::from_bytes(sync_blob_n(row, 0)?);
    let packed: Vec<u8> = row.get(1)?;
    let collected_count = sync_decode_u64(row, 2)?;
    let scanned_blob_count = sync_decode_u64(row, 3)?;
    let created_at_ms = sync_decode_u64(row, 4)?;
    if !packed.len().is_multiple_of(32) {
        return Err(ArtifactError::CorruptRecord("gc receipt digest packing"));
    }
    let collected_digests: Vec<ContentDigest> = packed
        .as_chunks::<32>()
        .0
        .iter()
        .map(|chunk| ContentDigest::from_bytes(*chunk))
        .collect();
    if u64::try_from(collected_digests.len()).unwrap_or(u64::MAX) != collected_count {
        return Err(ArtifactError::CorruptRecord(
            "gc receipt count disagrees with digest packing",
        ));
    }
    Ok(GcReceipt {
        receipt_id,
        collected_digests,
        collected_count,
        scanned_blob_count,
        created_at_ms,
    })
}

fn sync_decode_u64(row: &Row<'_>, index: usize) -> Result<u64, ArtifactError> {
    let value: i64 = row.get(index)?;
    u64::try_from(value).map_err(|_| ArtifactError::CorruptRecord("negative u64 column"))
}

fn sync_blob_n<const N: usize>(row: &Row<'_>, index: usize) -> Result<[u8; N], ArtifactError> {
    let bytes: Vec<u8> = row.get(index)?;
    <[u8; N]>::try_from(bytes.as_slice())
        .map_err(|_| ArtifactError::CorruptRecord("gc receipt blob length mismatch"))
}

impl ArtifactStore {
    /// Exports an [`ArtifactTransferPackage`] containing the revision chain and
    /// verified content-addressed blob bytes. See
    /// [`export_artifact_transfer_package`] for the full contract.
    ///
    /// # Errors
    ///
    /// Returns [`ArtifactSyncError`] on invalid request or storage error.
    pub fn export_artifact_transfer_package(
        &self,
        request: ExportArtifactTransferRequest,
    ) -> Result<ArtifactTransferPackage, ArtifactSyncError> {
        export_artifact_transfer_package(self, request)
    }

    /// Verifies and imports an [`ArtifactTransferPackage`] under `registry`,
    /// materializing missing blobs and revisions or isolating competing conflict
    /// blobs in `retention_set`. See [`import_artifact_transfer_package`] for
    /// the fail-closed order.
    ///
    /// # Errors
    ///
    /// Returns [`ArtifactSyncError`] on tamper, policy failure, CAS drift, or
    /// storage error.
    pub fn import_artifact_transfer_package(
        &self,
        registry: &ArtifactSyncPolicyRegistry,
        retention_set: &mut ReplicaSyncRetentionSet,
        request: ImportArtifactTransferRequest<'_>,
    ) -> Result<CrossStoreArtifactSyncReport, ArtifactSyncError> {
        import_artifact_transfer_package(self, registry, retention_set, request)
    }

    /// Runs explicit conservative orphan blob GC while additionally protecting
    /// all digests actively pinned in `retention_set` at
    /// `request.collected_at_ms`.
    ///
    /// # Errors
    ///
    /// Returns [`ArtifactError`] on storage or blob removal failure.
    pub fn collect_orphan_blobs_with_sync_roots(
        &self,
        request: CollectOrphanBlobsRequest,
        retention_set: &ReplicaSyncRetentionSet,
    ) -> Result<CollectOrphanBlobsDecision, ArtifactError> {
        collect_orphan_blobs_with_sync_roots(self, request, retention_set)
    }

    /// Reads and `SHA-256`-verifies a sync-pinned blob at `now_ms`.
    ///
    /// # Errors
    ///
    /// Returns [`ArtifactSyncError`] if `digest` is not actively pinned in
    /// `retention_set` at `now_ms` or if reading the blob fails.
    pub fn read_retained_sync_blob(
        &self,
        retention_set: &ReplicaSyncRetentionSet,
        digest: ContentDigest,
        now_ms: u64,
    ) -> Result<Vec<u8>, ArtifactSyncError> {
        read_retained_sync_blob(self, retention_set, digest, now_ms)
    }
}
