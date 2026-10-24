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

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use nlos_types::{ArtifactId, CommitPermitId, IdempotencyKey, TaskId};
use sha2::{Digest, Sha256};

use crate::ArtifactError;
use crate::model::{
    ArtifactPublicationReceipt, ContentDigest, HeadState, PublishStagedRevisionRequest,
    StageRevisionRequest, StagedRevisionRecord,
};
use crate::store::{ArtifactStore, validate_text_component};

/// Domain separation tag for deterministic [`ArtifactConflictReceipt`] digests.
pub const ARTIFACT_CONFLICT_RECEIPT_DOMAIN: &[u8] = b"llmos/artifact-conflict-receipt/v1";

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
}

impl fmt::Display for ArtifactSyncError {
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
