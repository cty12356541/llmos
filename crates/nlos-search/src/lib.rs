//! Search service: a read-only, non-authoritative query and candidate-discovery
//! face over the Semantic authority's admitted facts (`B-SEARCH-001`, W33-E /
//! X-1 second half; `C-SEARCH-001`, W65).
//!
//! > **状态声明（F1 / 2026-10-05 → W65 更新）**：
//! > - 2026-10-05 F1：本 crate 在 workspace 内零生产消费方（`cargo tree -i` 复核），
//! >   裁定**冻结保留**——能力与测试不作废，待接线（SC inspect 族现走各权威
//! >   inspector，未走本面；登记：`docs/management/stage-c-progress.md`
//! >   2026-10-05 F1 段）。
//! > - 2026-10-25 W65：超最小面扩展落地（非权威候选发现、向量检查点有界快照查询、游标分页与多 Cell 联邦候选归并）；生产消费方接线随 W66 紧接落地。
//!
//! Stage-B decision point 2 (`stage-b-progress` §6.5.4) and Stage-C `#15`
//! (`[CONS-DECL-001]`, `[CONS-CTRL-001]`, `[TASK-SNAPSHOT-003]`,
//! `[SEM-CHECKPOINT-001]`, `[DIST-FAIL-001]`, `[PERF-CONTROL-001]`) fix the
//! architectural boundary: Search is a *read-only, non-authoritative query
//! face* — zero `SemanticAuthority` writes under every path, and zero authority
//! to drive `Capability`, `ResourceAccount`, `TaskHead`, `Package`, or
//! `IrreversibleAction` gates on its own:
//!
//! - structural and checkpoint-bounded queries read the authority's durable
//!   rows through a `SQLite` connection opened with `SQLITE_OPEN_READ_ONLY` — a
//!   write attempt through that connection is rejected by `SQLite` itself, not
//!   by discipline;
//! - trust-view-derived lookups and `[TASK-SNAPSHOT-003]` candidate
//!   re-verification route through the authority's own read APIs
//!   ([`SemanticAuthority::inspect_trust_view`],
//!   [`SemanticAuthority::inspect_event`],
//!   [`SemanticAuthority::inspect_admission_receipt`], and
//!   [`SemanticAuthority::inspect_event_retraction`]), so derivation logic never
//!   has a second implementation here;
//! - [`SemanticIndex`] is an *optional* derived in-memory index built purely
//!   from authority reads: rebuildable at any time, never persisted, never
//!   canonical. A stale index can only under-report retractions (they are
//!   append-only and never revived, `[SEM-RETRACT-004]`); re-verifying any
//!   discovered candidate via [`verify_candidate_against_checkpoint`] fails
//!   closed on post-index retractions or post-checkpoint drift;
//! - multi-Cell candidate aggregation ([`federated`]) merges shard hits by
//!   `SemanticEventId` without assuming any cross-Cell global scalar `log_seq`
//!   (`[SEM-CHECKPOINT-001]`) and surfaces explicit `[DIST-FAIL-001]` failure
//!   semantics.
//!
//! The face fails closed on unknown authority schema versions: it reads the
//! durable `user_version` and refuses versions outside
//! [`SUPPORTED_AUTHORITY_SCHEMA_VERSIONS`], so a future authority migration
//! must extend that list before this face may read the new rows.

pub mod federated;

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

pub use federated::{
    CellSearchShardOutcome, FailedShardSummary, FederatedCandidateHit, FederatedSearchReport,
    RemoteSearchFailureReason, aggregate_federated_candidates,
};
pub use nlos_capability::custody::{Ed25519PublicKey, IdentityAuthority};
use nlos_capability::{CapabilityTarget, verify_ed25519_signature_strict};
use nlos_semantic::{
    AdmissionReceipt, AssertionMode, DomainSemanticWaterMark, SemanticAuthority,
    SemanticAuthorityError, SemanticVectorCheckpointError, SignedSemanticVectorCheckpoint,
    TrustViewSnapshot, TrustViewVerificationStatus, UnsignedAssertionEvent,
    compute_event_id_merkle_root, decode_unsigned_assertion_event, semantic_event_id,
    semantic_vector_checkpoint_signature_message, verify_signed_vector_checkpoint,
    verify_vector_checkpoint,
};
use nlos_types::{PrincipalId, SemanticEventId};
use rusqlite::{Connection, OpenFlags, ToSql};

/// Authority `user_version` values this face knows how to read. Coupled to
/// `nlos-semantic`'s private schema version by review: an authority
/// migration that keeps the tables this face reads compatible must add the
/// new version here, otherwise every open fails closed.
/// v7/v8 are additive authority migrations (v8: the immutable typed-link
/// index table); v9 is additive too (v9: the durability-receipt signature
/// preimage shape marker on a table this face never reads); the read face's
/// tables are unchanged, so all stay readable (W43-INT seam for the W43-E2
/// semantic v8 bump; W61-B for the v9 canonical-preimage bump).
const SUPPORTED_AUTHORITY_SCHEMA_VERSIONS: &[i64] = &[6, 7, 8, 9];

const EVENT_TYPE_ASSERTION: i64 = 1;

/// Requested read consistency declared by a search caller (`[CONS-DECL-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchRequestedConsistency {
    /// Allows eventual live scans or derived in-memory index projections.
    EventualOrDerived,
    /// Requires a cryptographically verified [`SignedSemanticVectorCheckpoint`]
    /// bounding the candidate snapshot cut (`[SEM-CHECKPOINT-001]`,
    /// `[TASK-SNAPSHOT-001]`, `[TASK-SNAPSHOT-003]`).
    CheckpointBoundedSnapshot,
}

impl SearchRequestedConsistency {
    /// Verifies that `achieved` satisfies `self` under `[CONS-DECL-001]`.
    ///
    /// When `achieved` is weaker than `self`, the caller must explicitly set
    /// `allow_downgrade = true` and list `achieved` inside
    /// `accepted_lower_modes`; otherwise this check fails closed with
    /// [`SearchError::ConsistencyUnsatisfied`].
    ///
    /// # Errors
    ///
    /// Returns [`SearchError::ConsistencyUnsatisfied`] when `achieved` does not
    /// satisfy `self` and downgrade to `achieved` was not explicitly accepted.
    pub fn verify_achieved(
        self,
        achieved: SearchAchievedConsistency,
        allow_downgrade: bool,
        accepted_lower_modes: &[SearchAchievedConsistency],
    ) -> Result<(), SearchError> {
        let directly_satisfied = match self {
            Self::EventualOrDerived => matches!(
                achieved,
                SearchAchievedConsistency::EventualLiveScan
                    | SearchAchievedConsistency::DerivedInMemoryIndex
                    | SearchAchievedConsistency::CheckpointBoundedSnapshot
            ),
            Self::CheckpointBoundedSnapshot => {
                achieved == SearchAchievedConsistency::CheckpointBoundedSnapshot
            }
        };
        if directly_satisfied || (allow_downgrade && accepted_lower_modes.contains(&achieved)) {
            Ok(())
        } else {
            Err(SearchError::ConsistencyUnsatisfied {
                requested: self,
                achieved,
            })
        }
    }
}

/// Actual consistency achieved by a search or candidate-aggregation operation
/// (`[CONS-DECL-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SearchAchievedConsistency {
    /// Read-only live scan over the local authority's `SQLite` rows.
    EventualLiveScan,
    /// Point-in-time scan over a rebuildable [`SemanticIndex`].
    DerivedInMemoryIndex,
    /// Read-only scan bounded to a verified [`SignedSemanticVectorCheckpoint`]
    /// watermark cut.
    CheckpointBoundedSnapshot,
    /// Multi-Cell federated aggregation where `allow_partial_on_remote_failure`
    /// was explicitly enabled and at least one remote shard failed
    /// (`[DIST-FAIL-001]`, `[CONS-DECL-001]`).
    FederatedPartialEventual,
}

/// Consumer gate category attempting to ingest search output
/// (`[CONS-CTRL-001]`, `[TASK-SNAPSHOT-003]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlConsumerKind {
    /// Non-authoritative candidate discovery (`[TASK-SNAPSHOT-003]`): allowed,
    /// subject to authority/checkpoint re-verification before commit.
    CandidateDiscovery,
    /// `Capability` issuance, delegation, revocation, or consumption decision.
    CapabilityDecision,
    /// `ResourceAccount` reservation or ledger commit.
    ResourceAccountCommit,
    /// Canonical `TaskHead` or `CommitPermit` commit.
    TaskHeadCommit,
    /// `Package` installation, update, or rollback head commit.
    PackageUpdate,
    /// Irreversible external `Action` or driver effect dispatch.
    IrreversibleAction,
}

impl fmt::Display for ControlConsumerKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CandidateDiscovery => formatter.write_str("CandidateDiscovery"),
            Self::CapabilityDecision => formatter.write_str("CapabilityDecision"),
            Self::ResourceAccountCommit => formatter.write_str("ResourceAccountCommit"),
            Self::TaskHeadCommit => formatter.write_str("TaskHeadCommit"),
            Self::PackageUpdate => formatter.write_str("PackageUpdate"),
            Self::IrreversibleAction => formatter.write_str("IrreversibleAction"),
        }
    }
}

/// Explicit non-authoritative use-restriction envelope carried by search pages,
/// candidate hits, and federated reports (`[CONS-DECL-001]`, `[CONS-CTRL-001]`).
#[allow(clippy::struct_excessive_bools)] // Mirrors the five explicit `[CONS-CTRL-001]` authority gates.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SearchUseRestrictions {
    pub forbid_capability_decision: bool,
    pub forbid_resource_ledger_commit: bool,
    pub forbid_task_head_commit: bool,
    pub forbid_package_update: bool,
    pub forbid_irreversible_action: bool,
}

impl Default for SearchUseRestrictions {
    fn default() -> Self {
        Self::NON_AUTHORITATIVE
    }
}

impl SearchUseRestrictions {
    /// Canonical `[CONS-CTRL-001]` restriction constant prohibiting search
    /// outputs from driving any control-plane authority gate directly.
    pub const NON_AUTHORITATIVE: Self = Self {
        forbid_capability_decision: true,
        forbid_resource_ledger_commit: true,
        forbid_task_head_commit: true,
        forbid_package_update: true,
        forbid_irreversible_action: true,
    };

    /// Returns `true` only when all five `[CONS-CTRL-001]` control-gate
    /// prohibitions remain enabled.
    #[must_use]
    pub const fn is_strictly_non_authoritative(&self) -> bool {
        self.forbid_capability_decision
            && self.forbid_resource_ledger_commit
            && self.forbid_task_head_commit
            && self.forbid_package_update
            && self.forbid_irreversible_action
    }

    /// Fail-closed `[CONS-CTRL-001]` gate helper: allows
    /// [`ControlConsumerKind::CandidateDiscovery`] only when this envelope
    /// remains strictly non-authoritative, and rejects every attempt to feed
    /// search output directly into `Capability`, `ResourceAccount`, `TaskHead`,
    /// `Package`, or `IrreversibleAction` gates.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError::ControlAuthoritativeUseForbidden`] whenever
    /// `purpose` targets a control-plane gate or any `forbid_*` flag was
    /// cleared.
    pub const fn enforce_not_control_authoritative(
        &self,
        purpose: ControlConsumerKind,
    ) -> Result<(), SearchError> {
        if !self.is_strictly_non_authoritative() {
            return Err(SearchError::ControlAuthoritativeUseForbidden(purpose));
        }
        match purpose {
            ControlConsumerKind::CandidateDiscovery => Ok(()),
            ControlConsumerKind::CapabilityDecision
            | ControlConsumerKind::ResourceAccountCommit
            | ControlConsumerKind::TaskHeadCommit
            | ControlConsumerKind::PackageUpdate
            | ControlConsumerKind::IrreversibleAction => {
                Err(SearchError::ControlAuthoritativeUseForbidden(purpose))
            }
        }
    }
}

/// Fail-closed `[CONS-CTRL-001]` gate helper rejecting any attempt to route
/// search output directly into `Capability`, `ResourceAccount`, `TaskHead`,
/// `Package`, or `IrreversibleAction` gates.
///
/// # Errors
///
/// Fails closed with [`SearchError::ControlAuthoritativeUseForbidden`] for
/// every consumer other than [`ControlConsumerKind::CandidateDiscovery`].
pub const fn enforce_not_control_authoritative(
    purpose: ControlConsumerKind,
) -> Result<(), SearchError> {
    SearchUseRestrictions::NON_AUTHORITATIVE.enforce_not_control_authoritative(purpose)
}

/// Cryptographic verifier for a [`SignedSemanticVectorCheckpoint`]
/// (`[SEM-CHECKPOINT-001]`).
///
/// Implemented for both a pinned 32-byte `Ed25519` public key
/// ([`Ed25519PublicKey`] / `[u8; 32]`) and a full [`IdentityAuthority`] handle
/// (which additionally checks principal/domain binding, key purpose, validity
/// window, and non-revocation via [`verify_signed_vector_checkpoint`]).
pub trait CheckpointVerifier {
    /// Verifies the structural, Merkle-root, digest, and `Ed25519` signature
    /// invariants of `checkpoint`.
    ///
    /// # Errors
    ///
    /// Fails closed with [`SemanticVectorCheckpointError`] on any tampering or
    /// signer mismatch.
    fn verify_checkpoint(
        &self,
        checkpoint: &SignedSemanticVectorCheckpoint,
    ) -> Result<(), SemanticVectorCheckpointError>;
}

impl CheckpointVerifier for Ed25519PublicKey {
    fn verify_checkpoint(
        &self,
        checkpoint: &SignedSemanticVectorCheckpoint,
    ) -> Result<(), SemanticVectorCheckpointError> {
        verify_vector_checkpoint(&checkpoint.checkpoint())?;
        if checkpoint.signer_authority_id == [0_u8; 16]
            || checkpoint.signer_public_key == [0_u8; 32]
            || checkpoint.signature == [0_u8; 64]
            || *self == [0_u8; 32]
        {
            return Err(SemanticVectorCheckpointError::InvalidSignerMetadata);
        }
        if checkpoint.signer_public_key != *self {
            return Err(SemanticVectorCheckpointError::SignerBindingMismatch);
        }
        let message_digest = semantic_vector_checkpoint_signature_message(
            checkpoint.checkpoint_id,
            checkpoint.checkpoint_digest,
            checkpoint.signer_authority_id,
            checkpoint.signer_public_key,
        );
        if !verify_ed25519_signature_strict(self, &message_digest, &checkpoint.signature) {
            return Err(SemanticVectorCheckpointError::InvalidSignature);
        }
        Ok(())
    }
}

impl CheckpointVerifier for IdentityAuthority {
    fn verify_checkpoint(
        &self,
        checkpoint: &SignedSemanticVectorCheckpoint,
    ) -> Result<(), SemanticVectorCheckpointError> {
        verify_signed_vector_checkpoint(self, checkpoint)
    }
}

#[derive(Debug)]
pub enum SearchError {
    Sqlite(rusqlite::Error),
    /// A typed read failure propagated from the Semantic authority without
    /// retry or translation (including trust-view lookups).
    Authority(SemanticAuthorityError),
    SchemaVersionUnsupported(i64),
    /// A zero result limit: the caller asked for nothing, which is treated
    /// as an input error instead of a silent empty page.
    InvalidLimit,
    /// The pure in-memory index cannot join authority-derived trust state;
    /// verification-filtered queries must go through [`SearchService`].
    VerificationJoinUnavailable,
    /// `[CONS-CTRL-001]` Search or index output was routed into a prohibited
    /// control-plane authority gate (or carried tampered use restrictions).
    ControlAuthoritativeUseForbidden(ControlConsumerKind),
    /// `[CONS-DECL-001]` Achieved search consistency was weaker than the
    /// requested consistency without explicit downgrade authorization.
    ConsistencyUnsatisfied {
        requested: SearchRequestedConsistency,
        achieved: SearchAchievedConsistency,
    },
    /// `[SEM-CHECKPOINT-001]` The supplied [`SignedSemanticVectorCheckpoint`]
    /// failed structural, digest, signer-binding, or `Ed25519` signature
    /// verification.
    InvalidCheckpoint(SemanticVectorCheckpointError),
    /// `[SEM-CHECKPOINT-001]` The verified checkpoint contains no watermark for
    /// the requested `domain_id`.
    MissingDomainWatermark {
        domain_id: [u8; 16],
    },
    /// `[SEM-CHECKPOINT-001]` The local authority store's `event_log` prefix
    /// for `domain_id` disagrees with the pinned checkpoint's `event_count` or
    /// `event_id_merkle_root`.
    CheckpointWatermarkMismatch {
        domain_id: [u8; 16],
    },
    /// `[TASK-SNAPSHOT-003]` A discovered candidate disagrees with the live
    /// [`SemanticAuthority`] event, content, or admission record.
    CandidateAuthorityMismatch(&'static str),
    /// `[TASK-SNAPSHOT-003]` / `[SEM-RETRACT-004]` A discovered candidate has
    /// been durably retracted in [`SemanticAuthority`].
    CandidateRetracted {
        event_id: SemanticEventId,
    },
    /// `[TASK-SNAPSHOT-003]` A discovered candidate lies outside the pinned
    /// [`SignedSemanticVectorCheckpoint`] watermark cut.
    CandidateOutsideCheckpoint {
        event_id: SemanticEventId,
        candidate_log_seq: u64,
        watermark_max_log_seq: u64,
    },
    /// `[DIST-FAIL-001]` One or more remote Cell shards failed while
    /// `allow_partial_on_remote_failure == false`.
    RemoteShardFailed(Vec<FailedShardSummary>),
    CorruptRecord(&'static str),
}

impl fmt::Display for SearchError {
    #[allow(clippy::too_many_lines)] // Exhaustive typed display across the search error taxonomy.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(error) => write!(formatter, "SQLite search face failure: {error}"),
            Self::Authority(error) => {
                write!(
                    formatter,
                    "semantic authority rejected search read: {error}"
                )
            }
            Self::SchemaVersionUnsupported(version) => {
                write!(
                    formatter,
                    "unsupported semantic authority schema version {version}"
                )
            }
            Self::InvalidLimit => formatter.write_str("result limit must be at least 1"),
            Self::VerificationJoinUnavailable => formatter.write_str(
                "the in-memory index cannot join trust views; query through the service",
            ),
            Self::ControlAuthoritativeUseForbidden(purpose) => write!(
                formatter,
                "non-authoritative search output is forbidden for control consumer {purpose} ([CONS-CTRL-001])"
            ),
            Self::ConsistencyUnsatisfied {
                requested,
                achieved,
            } => write!(
                formatter,
                "search achieved consistency {achieved:?} does not satisfy requested {requested:?} without explicit downgrade ([CONS-DECL-001])"
            ),
            Self::InvalidCheckpoint(error) => {
                write!(formatter, "invalid signed vector checkpoint: {error}")
            }
            Self::MissingDomainWatermark { domain_id } => write!(
                formatter,
                "signed vector checkpoint has no watermark for domain {domain_id:?}"
            ),
            Self::CheckpointWatermarkMismatch { domain_id } => write!(
                formatter,
                "local authority log prefix disagrees with checkpoint watermark for domain {domain_id:?}"
            ),
            Self::CandidateAuthorityMismatch(reason) => write!(
                formatter,
                "candidate disagrees with semantic authority record: {reason}"
            ),
            Self::CandidateRetracted { event_id } => write!(
                formatter,
                "candidate event {event_id:?} is durably retracted in semantic authority"
            ),
            Self::CandidateOutsideCheckpoint {
                event_id,
                candidate_log_seq,
                watermark_max_log_seq,
            } => write!(
                formatter,
                "candidate event {event_id:?} at log_seq {candidate_log_seq} exceeds checkpoint watermark {watermark_max_log_seq}"
            ),
            Self::RemoteShardFailed(shards) => write!(
                formatter,
                "{} remote search shard(s) failed under fail-closed policy",
                shards.len()
            ),
            Self::CorruptRecord(reason) => {
                write!(formatter, "corrupt semantic authority record: {reason}")
            }
        }
    }
}

impl Error for SearchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlite(error) => Some(error),
            Self::Authority(error) => Some(error),
            Self::InvalidCheckpoint(error) => Some(error),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for SearchError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}

/// One query hit: an admitted Assertion addressed by the fields a selector
/// can predicate on. Everything else (canonical bytes, receipts, lineage)
/// stays behind the authority's point-read APIs.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AssertionHit {
    pub event_id: SemanticEventId,
    /// Admission order in the authority's single event log.
    pub log_seq: u64,
    pub scope: CapabilityTarget,
    pub issuer: PrincipalId,
    pub assertion_mode: AssertionMode,
    pub content_digest: [u8; 32],
    pub content_media_type: String,
    pub admitted_at_ms: u64,
    /// Whether the authority's durable `event_retractions` holds a row for
    /// this event **at read time** (live scan), **at checkpoint cut**
    /// (checkpoint-bounded scan), or **at index build time** (derived
    /// snapshot).
    pub retracted: bool,
}

/// Non-authoritative candidate envelope wrapping an [`AssertionHit`] with its
/// originating `domain_id`, `achieved_consistency`, and
/// [`SearchUseRestrictions::NON_AUTHORITATIVE`] (`[CONS-DECL-001]`,
/// `[CONS-CTRL-001]`, `[TASK-SNAPSHOT-003]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NonAuthoritativeCandidateHit {
    pub hit: AssertionHit,
    pub domain_id: [u8; 16],
    pub achieved_consistency: SearchAchievedConsistency,
    pub use_restrictions: SearchUseRestrictions,
}

impl NonAuthoritativeCandidateHit {
    /// Wraps `hit` into a strictly non-authoritative candidate envelope.
    #[must_use]
    pub const fn new(
        hit: AssertionHit,
        domain_id: [u8; 16],
        achieved_consistency: SearchAchievedConsistency,
    ) -> Self {
        Self {
            hit,
            domain_id,
            achieved_consistency,
            use_restrictions: SearchUseRestrictions::NON_AUTHORITATIVE,
        }
    }

    /// Enforces `[CONS-CTRL-001]` non-authoritative usage rules on this
    /// candidate.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError::ControlAuthoritativeUseForbidden`] for any
    /// control-plane gate consumer.
    pub const fn enforce_not_control_authoritative(
        &self,
        purpose: ControlConsumerKind,
    ) -> Result<(), SearchError> {
        self.use_restrictions
            .enforce_not_control_authoritative(purpose)
    }
}

/// Authority-backed proof witness returned by
/// [`verify_candidate_against_checkpoint`] (`[TASK-SNAPSHOT-003]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedCheckpointCandidate {
    pub event_id: SemanticEventId,
    pub domain_id: [u8; 16],
    pub local_log_seq: u64,
    pub watermark_max_log_seq: u64,
    pub checkpoint_id: [u8; 16],
    pub checkpoint_digest: [u8; 32],
    pub admission_receipt: AdmissionReceipt,
}

/// How the durable retraction fact filters hits.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RetractionFilter {
    /// Keep every assertion regardless of retraction state.
    #[default]
    Any,
    ExcludeRetracted,
    OnlyRetracted,
}

/// How authority-derived verification state filters hits. Evaluation routes
/// through [`SemanticAuthority::inspect_trust_view`] at query time — the
/// derivation is never reimplemented here.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum VerificationFilter {
    #[default]
    Any,
    Status(TrustViewVerificationStatus),
}

/// The assertion query shape: every predicate is optional and predicates
/// compose with AND; hits return in admission (`log_seq`) order and `limit`
/// applies after all filters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AssertionSelector {
    pub scope: Option<CapabilityTarget>,
    pub issuer: Option<PrincipalId>,
    pub assertion_mode: Option<AssertionMode>,
    pub content_digest: Option<[u8; 32]>,
    pub retraction: RetractionFilter,
    pub verification: VerificationFilter,
    pub limit: usize,
}

impl AssertionSelector {
    /// A structural catch-all with the given result limit.
    #[must_use]
    pub const fn new(limit: usize) -> Self {
        Self {
            scope: None,
            issuer: None,
            assertion_mode: None,
            content_digest: None,
            retraction: RetractionFilter::Any,
            verification: VerificationFilter::Any,
            limit,
        }
    }
}

/// Cursor-paginated and optionally upper-bounded assertion selector
/// (`[PERF-CONTROL-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PaginatedAssertionSelector {
    pub selector: AssertionSelector,
    /// Exclusive lower keyset cursor (`l.log_seq > after_log_seq`).
    pub after_log_seq: Option<u64>,
    /// Optional inclusive upper `log_seq` bound (`l.log_seq <= max_log_seq_inclusive`).
    pub max_log_seq_inclusive: Option<u64>,
}

impl PaginatedAssertionSelector {
    /// Creates a first-page selector (`after_log_seq = None`,
    /// `max_log_seq_inclusive = None`).
    #[must_use]
    pub const fn new(selector: AssertionSelector) -> Self {
        Self {
            selector,
            after_log_seq: None,
            max_log_seq_inclusive: None,
        }
    }

    /// Sets the exclusive `after_log_seq` keyset cursor.
    #[must_use]
    pub const fn with_after_log_seq(mut self, after_log_seq: Option<u64>) -> Self {
        self.after_log_seq = after_log_seq;
        self
    }

    /// Sets the inclusive `max_log_seq_inclusive` upper bound.
    #[must_use]
    pub const fn with_max_log_seq_inclusive(mut self, max_log_seq_inclusive: Option<u64>) -> Self {
        self.max_log_seq_inclusive = max_log_seq_inclusive;
        self
    }
}

/// One cursor-paginated result page carrying explicit `[CONS-DECL-001]`
/// consistency and `[CONS-CTRL-001]` use-restriction metadata
/// (`[PERF-CONTROL-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SearchPage<T> {
    pub items: Vec<T>,
    pub next_after_log_seq: Option<u64>,
    pub exhausted: bool,
    pub achieved_consistency: SearchAchievedConsistency,
    pub use_restrictions: SearchUseRestrictions,
}

impl<T> SearchPage<T> {
    /// Enforces `[CONS-CTRL-001]` non-authoritative usage rules on this page.
    ///
    /// # Errors
    ///
    /// Returns [`SearchError::ControlAuthoritativeUseForbidden`] for any
    /// control-plane gate consumer.
    pub const fn enforce_not_control_authoritative(
        &self,
        purpose: ControlConsumerKind,
    ) -> Result<(), SearchError> {
        self.use_restrictions
            .enforce_not_control_authoritative(purpose)
    }
}

fn verify_candidate_matches_live_authority(
    service: &SearchService,
    candidate: &NonAuthoritativeCandidateHit,
) -> Result<(u64, AdmissionReceipt), SearchError> {
    let event_id = candidate.hit.event_id;
    let event_record = service
        .authority
        .inspect_event(event_id)
        .map_err(SearchError::Authority)?;
    if event_record.control_domain.into_bytes() != candidate.domain_id {
        return Err(SearchError::CandidateAuthorityMismatch(
            "candidate domain_id disagrees with authority event control_domain",
        ));
    }
    if event_record.log_seq != candidate.hit.log_seq {
        return Err(SearchError::CandidateAuthorityMismatch(
            "candidate log_seq disagrees with authority event record",
        ));
    }
    if event_record.scope != candidate.hit.scope {
        return Err(SearchError::CandidateAuthorityMismatch(
            "candidate scope disagrees with authority event record",
        ));
    }
    if event_record.issuer != candidate.hit.issuer {
        return Err(SearchError::CandidateAuthorityMismatch(
            "candidate issuer disagrees with authority event record",
        ));
    }

    let decoded: UnsignedAssertionEvent =
        decode_unsigned_assertion_event(&event_record.canonical_unsigned_event)
            .map_err(SearchError::Authority)?;
    if decoded.assertion_mode != candidate.hit.assertion_mode {
        return Err(SearchError::CandidateAuthorityMismatch(
            "candidate assertion_mode disagrees with canonical assertion event",
        ));
    }
    if decoded.content_digest != candidate.hit.content_digest {
        return Err(SearchError::CandidateAuthorityMismatch(
            "candidate content_digest disagrees with canonical assertion event",
        ));
    }

    let stored_media_type: String = service
        .connection
        .query_row(
            "SELECT media_type FROM content_objects WHERE content_digest = ?1",
            [candidate.hit.content_digest.as_slice()],
            |row| row.get(0),
        )
        .map_err(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => {
                SearchError::CorruptRecord("missing content_object row for admitted assertion")
            }
            other => SearchError::Sqlite(other),
        })?;
    if stored_media_type != candidate.hit.content_media_type {
        return Err(SearchError::CandidateAuthorityMismatch(
            "candidate content_media_type disagrees with authority content_object",
        ));
    }

    let admission_receipt = service
        .authority
        .inspect_admission_receipt(event_id)
        .map_err(SearchError::Authority)?;
    if admission_receipt.log_seq != event_record.log_seq
        || admission_receipt.admitted_at_ms != candidate.hit.admitted_at_ms
    {
        return Err(SearchError::CandidateAuthorityMismatch(
            "candidate admission metadata disagrees with authority admission receipt",
        ));
    }

    if service
        .authority
        .inspect_event_retraction(event_id)
        .map_err(SearchError::Authority)?
        .is_some()
    {
        return Err(SearchError::CandidateRetracted { event_id });
    }

    Ok((event_record.log_seq, admission_receipt))
}

/// Re-verifies a discovered [`NonAuthoritativeCandidateHit`] against both a
/// cryptographically verified [`SignedSemanticVectorCheckpoint`] and the live
/// [`SemanticAuthority`] admission/retraction state (`[TASK-SNAPSHOT-003]`,
/// `[SEM-CHECKPOINT-001]`, `[SEM-RETRACT-004]`).
///
/// Candidate discovery via [`SearchService`] or [`SemanticIndex`] is strictly
/// non-authoritative (`[CONS-CTRL-001]`): before any candidate may enter a
/// `TaskSnapshot`-pinned read set or commit check, this helper proves:
///
/// 1. the candidate's [`SearchUseRestrictions`] remain untampered;
/// 2. `checkpoint`'s `Ed25519` signature, Merkle roots, and digests verify
///    against `verifying_key`;
/// 3. `checkpoint` contains a [`DomainSemanticWaterMark`] for
///    `candidate.domain_id`, and the local store's `event_log` prefix matches
///    the watermark's `event_count` and `event_id_merkle_root`;
/// 4. the live [`SemanticAuthority`] holds an intact event record, content
///    object, and [`AdmissionReceipt`] matching every field of `candidate.hit`;
/// 5. the event is **not** durably retracted in [`SemanticAuthority`] (catching
///    post-index or post-checkpoint retractions fail-closed); and
/// 6. the event's local `log_seq` and `event_id` fall inside the verified
///    checkpoint watermark cut (rejecting post-checkpoint admissions).
///
/// # Errors
///
/// Fails closed with a typed [`SearchError`] (`InvalidCheckpoint`,
/// `MissingDomainWatermark`, `CheckpointWatermarkMismatch`, `Authority`,
/// `CandidateAuthorityMismatch`, `CandidateRetracted`, or
/// `CandidateOutsideCheckpoint`).
pub fn verify_candidate_against_checkpoint<V: CheckpointVerifier>(
    service: &SearchService,
    candidate: &NonAuthoritativeCandidateHit,
    checkpoint: &SignedSemanticVectorCheckpoint,
    verifying_key: &V,
) -> Result<VerifiedCheckpointCandidate, SearchError> {
    candidate
        .use_restrictions
        .enforce_not_control_authoritative(ControlConsumerKind::CandidateDiscovery)?;
    verifying_key
        .verify_checkpoint(checkpoint)
        .map_err(SearchError::InvalidCheckpoint)?;

    let watermark = checkpoint
        .domain_watermarks
        .iter()
        .find(|wm| wm.authority_id == candidate.domain_id)
        .ok_or(SearchError::MissingDomainWatermark {
            domain_id: candidate.domain_id,
        })?;
    let cut = resolve_domain_checkpoint_cut(&service.connection, candidate.domain_id, watermark)?;
    let (local_log_seq, admission_receipt) =
        verify_candidate_matches_live_authority(service, candidate)?;
    let event_id = candidate.hit.event_id;

    if local_log_seq > cut.max_log_seq || !cut.covered_event_ids.contains(&event_id) {
        return Err(SearchError::CandidateOutsideCheckpoint {
            event_id,
            candidate_log_seq: local_log_seq,
            watermark_max_log_seq: cut.max_log_seq,
        });
    }

    Ok(VerifiedCheckpointCandidate {
        event_id,
        domain_id: candidate.domain_id,
        local_log_seq,
        watermark_max_log_seq: cut.max_log_seq,
        checkpoint_id: checkpoint.checkpoint_id,
        checkpoint_digest: checkpoint.checkpoint_digest,
        admission_receipt,
    })
}

/// The read-only query face over one Semantic authority store.
///
/// The authority handle is kept for trust-view-derived lookups (its own
/// read APIs are the single source of the derivation); structural queries
/// go through the read-only connection, so the face has no write path at
/// all — not to the authority, and no database of its own either.
pub struct SearchService {
    authority: Arc<SemanticAuthority>,
    connection: Connection,
}

impl SearchService {
    /// Opens the face over `<root>/semantic-authority.db`, strictly
    /// read-only, bound to the live authority handle.
    ///
    /// The authority handle should own the same root (the face cannot
    /// observe the binding and trusts the host's wiring, like
    /// `nlos-notify`). Opening a WAL-mode database whose `-wal`/`-shm`
    /// sidecars are missing after an unclean shutdown fails closed with the
    /// propagated `SQLite` error; re-opening the authority recovers the log
    /// first.
    ///
    /// # Errors
    ///
    /// Fails closed when the database cannot be opened read-only or its
    /// `user_version` is outside the supported authority schema versions.
    pub fn open(
        root: impl AsRef<Path>,
        authority: Arc<SemanticAuthority>,
    ) -> Result<Self, SearchError> {
        let database = root.as_ref().join("semantic-authority.db");
        let connection = Connection::open_with_flags(database, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        let version: i64 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if !SUPPORTED_AUTHORITY_SCHEMA_VERSIONS.contains(&version) {
            return Err(SearchError::SchemaVersionUnsupported(version));
        }
        Ok(Self {
            authority,
            connection,
        })
    }

    /// Queries admitted assertions by selector shape against the live
    /// authority rows.
    ///
    /// Structural predicates (scope, issuer, content digest) are evaluated
    /// in SQL over the read-only connection; the assertion-mode predicate
    /// is evaluated after decoding the canonical bytes with the
    /// authority's own public decoder; a verification predicate is
    /// evaluated by reading the authority's live
    /// [`SemanticAuthority::inspect_trust_view`] per surviving candidate.
    /// `limit` applies after all filters.
    ///
    /// The scan is limit-aware (audit #30): each round fetches one
    /// amplified SQL window — `LIMIT` pushed into the structural query
    /// over the deterministic `log_seq` order, keyset-paginated on the
    /// unique `log_seq` — and applies the Rust-side filters to it; when a
    /// window's survivors fall short of `limit`, the next window is
    /// fetched until `limit` survivors are collected or the event log is
    /// exhausted. A small-limit query therefore never decodes or
    /// trust-view-joins the unfiltered tail.
    ///
    /// # Errors
    ///
    /// Fails closed with [`SearchError::InvalidLimit`] for a zero limit, a
    /// propagated authority failure (for example `EventNotFound`-family or
    /// dangling-lineage during the trust-view join), or a storage/corruption
    /// failure.
    pub fn search_assertions(
        &self,
        selector: &AssertionSelector,
    ) -> Result<Vec<AssertionHit>, SearchError> {
        let page = self.search_assertions_paginated(&PaginatedAssertionSelector::new(*selector))?;
        Ok(page.items)
    }

    /// Executes a cursor-paginated, read-only assertion scan with bounded
    /// per-page `SQLite` fetch windows (`[PERF-CONTROL-001]`,
    /// `[CONS-DECL-001]`, `[CONS-CTRL-001]`).
    ///
    /// - `paginated.after_log_seq` advances strictly forward across the unique
    ///   `event_log.log_seq` primary key (`l.log_seq > after_log_seq`),
    ///   guaranteeing zero duplicates and zero gaps across pages;
    /// - `paginated.max_log_seq_inclusive` optionally caps the scan at an
    ///   inclusive `log_seq` ceiling (`l.log_seq <= max_log_seq_inclusive`);
    /// - each page carries [`SearchAchievedConsistency::EventualLiveScan`] and
    ///   [`SearchUseRestrictions::NON_AUTHORITATIVE`] (only
    ///   [`SearchService::search_candidates_at_checkpoint`] may advertise
    ///   [`SearchAchievedConsistency::CheckpointBoundedSnapshot`]).
    ///
    /// # Errors
    ///
    /// Fails closed on `selector.limit == 0`, storage/decode corruption, or a
    /// propagated authority trust-view lookup failure.
    pub fn search_assertions_paginated(
        &self,
        paginated: &PaginatedAssertionSelector,
    ) -> Result<SearchPage<AssertionHit>, SearchError> {
        self.scan_assertion_page(
            &paginated.selector,
            None,
            paginated.after_log_seq,
            paginated.max_log_seq_inclusive,
            SearchAchievedConsistency::EventualLiveScan,
        )
    }

    /// Discovers non-authoritative assertion candidates bounded to a
    /// cryptographically verified [`SignedSemanticVectorCheckpoint`]
    /// (`[SEM-CHECKPOINT-001]`, `[TASK-SNAPSHOT-001]`, `[TASK-SNAPSHOT-003]`,
    /// `[CONS-DECL-001]`, `[CONS-CTRL-001]`).
    ///
    /// Verifies `checkpoint` against `verifying_key` first, requires a
    /// [`DomainSemanticWaterMark`] for `local_domain_id` (`require_domain_present
    /// = true`), verifies that the local read-only `SQLite` prefix of length
    /// `watermark.event_count` hashes to `watermark.event_id_merkle_root`, and
    /// bounds the read-only scan strictly to `l.log_seq <= domain_watermark` so
    /// assertions admitted after the checkpoint are excluded.
    ///
    /// # Errors
    ///
    /// Fails closed on tampered checkpoint signatures/digests, missing or
    /// mismatched domain watermarks, `selector.limit == 0`, or storage/authority
    /// read errors.
    pub fn search_candidates_at_checkpoint<V: CheckpointVerifier>(
        &self,
        local_domain_id: [u8; 16],
        checkpoint: &SignedSemanticVectorCheckpoint,
        verifying_key: &V,
        selector: &AssertionSelector,
        after_log_seq: Option<u64>,
    ) -> Result<SearchPage<NonAuthoritativeCandidateHit>, SearchError> {
        self.search_candidates_at_checkpoint_with_options(
            local_domain_id,
            checkpoint,
            verifying_key,
            selector,
            after_log_seq,
            true,
        )
    }

    /// Checkpoint-bounded candidate scan with explicit `require_domain_present`
    /// policy (`[SEM-CHECKPOINT-001]`).
    ///
    /// When `require_domain_present == false` and `local_domain_id` has no
    /// watermark in `checkpoint` (meaning zero events from `local_domain_id`
    /// were covered at checkpoint creation), returns an empty exhausted page
    /// after verifying `checkpoint`'s signature.
    ///
    /// # Errors
    ///
    /// Fails closed on invalid checkpoint signatures/digests, `selector.limit
    /// == 0`, missing domain watermark when `require_domain_present == true`,
    /// local watermark Merkle-root mismatch, or storage/authority read errors.
    pub fn search_candidates_at_checkpoint_with_options<V: CheckpointVerifier>(
        &self,
        local_domain_id: [u8; 16],
        checkpoint: &SignedSemanticVectorCheckpoint,
        verifying_key: &V,
        selector: &AssertionSelector,
        after_log_seq: Option<u64>,
        require_domain_present: bool,
    ) -> Result<SearchPage<NonAuthoritativeCandidateHit>, SearchError> {
        verifying_key
            .verify_checkpoint(checkpoint)
            .map_err(SearchError::InvalidCheckpoint)?;
        if selector.limit == 0 {
            return Err(SearchError::InvalidLimit);
        }
        let Some(watermark) = checkpoint
            .domain_watermarks
            .iter()
            .find(|wm| wm.authority_id == local_domain_id)
        else {
            if require_domain_present {
                return Err(SearchError::MissingDomainWatermark {
                    domain_id: local_domain_id,
                });
            }
            return Ok(SearchPage {
                items: Vec::new(),
                next_after_log_seq: None,
                exhausted: true,
                achieved_consistency: SearchAchievedConsistency::CheckpointBoundedSnapshot,
                use_restrictions: SearchUseRestrictions::NON_AUTHORITATIVE,
            });
        };

        let cut = resolve_domain_checkpoint_cut(&self.connection, local_domain_id, watermark)?;
        let raw_page = self.scan_assertion_page(
            selector,
            Some(local_domain_id),
            after_log_seq,
            Some(cut.max_log_seq),
            SearchAchievedConsistency::CheckpointBoundedSnapshot,
        )?;
        let items = raw_page
            .items
            .into_iter()
            .map(|hit| {
                NonAuthoritativeCandidateHit::new(
                    hit,
                    local_domain_id,
                    SearchAchievedConsistency::CheckpointBoundedSnapshot,
                )
            })
            .collect();
        Ok(SearchPage {
            items,
            next_after_log_seq: raw_page.next_after_log_seq,
            exhausted: raw_page.exhausted,
            achieved_consistency: SearchAchievedConsistency::CheckpointBoundedSnapshot,
            use_restrictions: SearchUseRestrictions::NON_AUTHORITATIVE,
        })
    }

    /// Re-verifies a discovered [`NonAuthoritativeCandidateHit`] against
    /// `checkpoint` and this service's live [`SemanticAuthority`]
    /// (`[TASK-SNAPSHOT-003]`).
    ///
    /// # Errors
    ///
    /// Delegates to [`verify_candidate_against_checkpoint`]; fails closed on
    /// any signature, watermark, authority-record, or retraction violation.
    pub fn verify_candidate_against_checkpoint<V: CheckpointVerifier>(
        &self,
        candidate: &NonAuthoritativeCandidateHit,
        checkpoint: &SignedSemanticVectorCheckpoint,
        verifying_key: &V,
    ) -> Result<VerifiedCheckpointCandidate, SearchError> {
        verify_candidate_against_checkpoint(self, candidate, checkpoint, verifying_key)
    }

    /// Reads the authority's `TrustView` snapshot for one committed event,
    /// verbatim.
    ///
    /// # Errors
    ///
    /// Propagates the authority's typed failures (`EventNotFound`,
    /// `DanglingLineage`, storage/corruption) without translation.
    pub fn trust_view(&self, event_id: SemanticEventId) -> Result<TrustViewSnapshot, SearchError> {
        self.authority
            .inspect_trust_view(event_id)
            .map_err(SearchError::Authority)
    }

    /// Builds the derived in-memory index from a full read pass over the
    /// authority's admitted assertions.
    ///
    /// The index is a point-in-time snapshot: never persisted, never
    /// canonical, and rebuildable at any moment. Each entry re-derives the
    /// `EventId` from the canonical bytes and fails closed on mismatch.
    /// The pass stays deliberately unbounded (audit #30's documented
    /// full-scan design): no cursor, no LIMIT window, one whole-log read.
    ///
    /// # Errors
    ///
    /// Fails closed for a storage, decode, or read-integrity failure.
    pub fn build_index(&self) -> Result<SemanticIndex, SearchError> {
        let entries = fetch_assertion_rows(
            &self.connection,
            &AssertionSelector::new(usize::MAX),
            &SqlScanBounds {
                control_domain_id: None,
                after_log_seq: None,
                max_log_seq_inclusive: None,
                fetch_limit: None,
            },
        )?;
        Ok(SemanticIndex { entries })
    }

    fn scan_assertion_page(
        &self,
        selector: &AssertionSelector,
        control_domain_id: Option<[u8; 16]>,
        initial_after_log_seq: Option<u64>,
        max_log_seq_inclusive: Option<u64>,
        achieved_consistency: SearchAchievedConsistency,
    ) -> Result<SearchPage<AssertionHit>, SearchError> {
        if selector.limit == 0 {
            return Err(SearchError::InvalidLimit);
        }
        if let (Some(after), Some(max)) = (initial_after_log_seq, max_log_seq_inclusive)
            && after >= max
        {
            return Ok(SearchPage {
                items: Vec::new(),
                next_after_log_seq: None,
                exhausted: true,
                achieved_consistency,
                use_restrictions: SearchUseRestrictions::NON_AUTHORITATIVE,
            });
        }

        let has_post_sql_filter = selector.assertion_mode.is_some()
            || selector.retraction != RetractionFilter::Any
            || selector.verification != VerificationFilter::Any;
        let window = if has_post_sql_filter {
            sql_fetch_window(selector.limit)
        } else {
            selector.limit.saturating_add(1)
        };
        let window_capped_in_sql = i64::try_from(window).is_ok();

        let mut hits: Vec<AssertionHit> = Vec::new();
        // Keyset cursor over the unique, admission-monotone `log_seq`
        // (`event_log.log_seq` is `INTEGER PRIMARY KEY AUTOINCREMENT` on an
        // insert-only table): every non-empty page strictly advances the
        // cursor, so pages never skip or repeat rows.
        let mut after_log_seq = initial_after_log_seq;
        loop {
            let page = fetch_assertion_rows(
                &self.connection,
                selector,
                &SqlScanBounds {
                    control_domain_id,
                    after_log_seq,
                    max_log_seq_inclusive,
                    fetch_limit: Some(window),
                },
            )?;
            let page_len = page.len();
            let Some(last) = page.last() else {
                break;
            };
            after_log_seq = Some(last.log_seq);

            for (idx, hit) in page.into_iter().enumerate() {
                if selector
                    .assertion_mode
                    .is_some_and(|mode| hit.assertion_mode != mode)
                {
                    continue;
                }
                let retraction_keeps = match selector.retraction {
                    RetractionFilter::Any => true,
                    RetractionFilter::ExcludeRetracted => !hit.retracted,
                    RetractionFilter::OnlyRetracted => hit.retracted,
                };
                if !retraction_keeps {
                    continue;
                }
                // Verification join: one authority trust-view lookup per
                // surviving candidate. Post-v8 each lookup is an indexed
                // point read on the authority side, so the join costs
                // O(survivors) small point queries bounded by `window`.
                if let VerificationFilter::Status(status) = selector.verification {
                    let view = self
                        .authority
                        .inspect_trust_view(hit.event_id)
                        .map_err(SearchError::Authority)?;
                    if view.verification_status != status {
                        continue;
                    }
                }
                let cursor_at_hit = hit.log_seq;
                hits.push(hit);
                if hits.len() == selector.limit {
                    let sql_exhausted_here =
                        idx + 1 == page_len && (!window_capped_in_sql || page_len < window);
                    return Ok(SearchPage {
                        items: hits,
                        next_after_log_seq: (!sql_exhausted_here).then_some(cursor_at_hit),
                        exhausted: sql_exhausted_here,
                        achieved_consistency,
                        use_restrictions: SearchUseRestrictions::NON_AUTHORITATIVE,
                    });
                }
            }

            if !window_capped_in_sql || page_len < window {
                break;
            }
        }

        Ok(SearchPage {
            items: hits,
            next_after_log_seq: None,
            exhausted: true,
            achieved_consistency,
            use_restrictions: SearchUseRestrictions::NON_AUTHORITATIVE,
        })
    }
}

/// The optional derived in-memory index: a rebuildable snapshot of every
/// admitted assertion, queryable without touching the authority again.
///
/// Never canonical: entries are frozen at build time, so an out-of-band
/// retraction (append-only, never revoked) leaves a stale index
/// under-reporting — the live [`SearchService`] read or a rebuild always
/// wins. The index is a plain admission-ordered vector scanned linearly;
/// this minimal service deliberately owns no query engine.
pub struct SemanticIndex {
    entries: Vec<AssertionHit>,
}

impl SemanticIndex {
    /// Queries the snapshot with the structural predicates of `selector`.
    ///
    /// # Errors
    ///
    /// Fails closed with [`SearchError::InvalidLimit`] for a zero limit and
    /// [`SearchError::VerificationJoinUnavailable`] when a verification
    /// predicate is requested — authority-derived trust state is only
    /// joined by [`SearchService::search_assertions`].
    pub fn query(&self, selector: &AssertionSelector) -> Result<Vec<AssertionHit>, SearchError> {
        if selector.limit == 0 {
            return Err(SearchError::InvalidLimit);
        }
        if selector.verification != VerificationFilter::Any {
            return Err(SearchError::VerificationJoinUnavailable);
        }
        let mut hits = self
            .entries
            .iter()
            .filter(|hit| {
                selector.scope.is_none_or(|scope| hit.scope == scope)
                    && selector.issuer.is_none_or(|issuer| hit.issuer == issuer)
                    && selector
                        .assertion_mode
                        .is_none_or(|mode| hit.assertion_mode == mode)
                    && selector
                        .content_digest
                        .is_none_or(|digest| hit.content_digest == digest)
                    && match selector.retraction {
                        RetractionFilter::Any => true,
                        RetractionFilter::ExcludeRetracted => !hit.retracted,
                        RetractionFilter::OnlyRetracted => hit.retracted,
                    }
            })
            .cloned()
            .collect::<Vec<_>>();
        hits.truncate(selector.limit);
        Ok(hits)
    }

    /// Queries the derived index and wraps each hit in a
    /// [`NonAuthoritativeCandidateHit`] tagged with
    /// [`SearchAchievedConsistency::DerivedInMemoryIndex`] and
    /// [`SearchUseRestrictions::NON_AUTHORITATIVE`] (`[CONS-DECL-001]`,
    /// `[CONS-CTRL-001]`, `[TASK-SNAPSHOT-003]`).
    ///
    /// # Errors
    ///
    /// Propagates [`SemanticIndex::query`] errors (`InvalidLimit`,
    /// `VerificationJoinUnavailable`).
    pub fn query_candidates(
        &self,
        domain_id: [u8; 16],
        selector: &AssertionSelector,
    ) -> Result<Vec<NonAuthoritativeCandidateHit>, SearchError> {
        let hits = self.query(selector)?;
        Ok(hits
            .into_iter()
            .map(|hit| {
                NonAuthoritativeCandidateHit::new(
                    hit,
                    domain_id,
                    SearchAchievedConsistency::DerivedInMemoryIndex,
                )
            })
            .collect())
    }

    /// The snapshot entries in admission (`log_seq`) order.
    #[must_use]
    pub fn entries(&self) -> &[AssertionHit] {
        &self.entries
    }

    /// How many admitted assertions the snapshot holds.
    #[must_use]
    pub fn assertion_count(&self) -> usize {
        self.entries.len()
    }
}

struct DomainCheckpointCut {
    max_log_seq: u64,
    covered_event_ids: BTreeSet<SemanticEventId>,
}

fn resolve_domain_checkpoint_cut(
    connection: &Connection,
    domain_id: [u8; 16],
    watermark: &DomainSemanticWaterMark,
) -> Result<DomainCheckpointCut, SearchError> {
    let expected_count = usize::try_from(watermark.event_count)
        .map_err(|_| SearchError::CorruptRecord("watermark event_count exceeds usize"))?;
    let limit_i64 = i64::try_from(watermark.event_count)
        .map_err(|_| SearchError::CorruptRecord("watermark event_count exceeds sqlite integer"))?;

    let mut statement = connection.prepare(
        "SELECT e.event_id, l.log_seq
         FROM semantic_events e
         JOIN event_log l ON l.event_id = e.event_id
         WHERE e.control_domain_id = ?1
         ORDER BY l.log_seq ASC
         LIMIT ?2",
    )?;
    let raw_rows = statement
        .query_map(rusqlite::params![domain_id.as_slice(), limit_i64], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    if raw_rows.len() != expected_count {
        return Err(SearchError::CheckpointWatermarkMismatch { domain_id });
    }

    let mut event_ids = Vec::with_capacity(raw_rows.len());
    let mut max_log_seq = 0_u64;
    for (raw_id, raw_seq) in raw_rows {
        let id_bytes = decode_array::<32>(raw_id, "event id length is not 32")?;
        let seq = decode_u64(raw_seq)?;
        event_ids.push(SemanticEventId::from_bytes(id_bytes));
        max_log_seq = seq;
    }

    let derived_root = compute_event_id_merkle_root(&event_ids);
    if derived_root != watermark.event_id_merkle_root {
        return Err(SearchError::CheckpointWatermarkMismatch { domain_id });
    }

    Ok(DomainCheckpointCut {
        max_log_seq,
        covered_event_ids: event_ids.into_iter().collect(),
    })
}

type AssertionColumns = (
    Vec<u8>,
    Vec<u8>,
    i64,
    Vec<u8>,
    Vec<u8>,
    Vec<u8>,
    i64,
    i64,
    String,
    i64,
);

fn assertion_columns(row: &rusqlite::Row<'_>) -> rusqlite::Result<AssertionColumns> {
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

struct SqlScanBounds {
    control_domain_id: Option<[u8; 16]>,
    after_log_seq: Option<u64>,
    max_log_seq_inclusive: Option<u64>,
    fetch_limit: Option<usize>,
}

/// Fetches the assertion rows matching the selector's SQL-side structural
/// predicates, in admission (`log_seq`) order.
///
/// `bounds.after_log_seq` keyset-pages the scan (`l.log_seq > ?n`),
/// `bounds.max_log_seq_inclusive` optionally caps both the `event_log` rows
/// and the `event_retractions` subquery at `<= ?n` so checkpoint-bounded
/// snapshot scans never observe post-checkpoint admissions or retractions, and
/// `bounds.fetch_limit` pushes a `LIMIT ?n` window into the SQL.
fn fetch_assertion_rows(
    connection: &Connection,
    selector: &AssertionSelector,
    bounds: &SqlScanBounds,
) -> Result<Vec<AssertionHit>, SearchError> {
    let mut conditions: Vec<String> = Vec::new();
    // Binding ?1 is the fixed event-type; optional parameters number upward.
    let mut parameters: Vec<Box<dyn ToSql>> = vec![Box::new(EVENT_TYPE_ASSERTION)];

    let retracted_projection = if let Some(max_seq) = bounds.max_log_seq_inclusive {
        let max_seq_i64 = i64::try_from(max_seq)
            .map_err(|_| SearchError::CorruptRecord("max log seq exceeds sqlite integer"))?;
        let next = parameters.len() + 1;
        parameters.push(Box::new(max_seq_i64));
        conditions.push(format!("l.log_seq<=?{next}"));
        format!(
            "EXISTS(SELECT 1 FROM event_retractions r
                    JOIN event_log rl ON rl.event_id = r.retraction_event_id
                    WHERE r.target_event_id = e.event_id AND rl.log_seq <= ?{next}) AS retracted"
        )
    } else {
        String::from(
            "EXISTS(SELECT 1 FROM event_retractions r
                    WHERE r.target_event_id = e.event_id) AS retracted",
        )
    };

    if let Some(domain_id) = bounds.control_domain_id {
        let next = parameters.len() + 1;
        conditions.push(format!("e.control_domain_id=?{next}"));
        parameters.push(Box::new(domain_id.to_vec()));
    }
    if let Some(scope) = selector.scope {
        let (kind, id) = encode_scope(scope);
        let next = parameters.len() + 1;
        conditions.push(format!("e.scope_kind=?{next} AND e.scope_id=?{}", next + 1));
        parameters.push(Box::new(kind));
        parameters.push(Box::new(id.to_vec()));
    }
    if let Some(issuer) = selector.issuer {
        let next = parameters.len() + 1;
        conditions.push(format!("e.issuer_principal_id=?{next}"));
        parameters.push(Box::new(issuer.as_bytes().to_vec()));
    }
    if let Some(digest) = selector.content_digest {
        let next = parameters.len() + 1;
        conditions.push(format!("e.content_digest=?{next}"));
        parameters.push(Box::new(digest.to_vec()));
    }
    if let Some(log_seq) = bounds.after_log_seq {
        let next = parameters.len() + 1;
        conditions.push(format!("l.log_seq>?{next}"));
        parameters.push(Box::new(i64::try_from(log_seq).map_err(|_| {
            SearchError::CorruptRecord("log seq exceeds sqlite integer")
        })?));
    }

    let mut sql = format!(
        "SELECT e.event_id, e.canonical_unsigned_event, e.scope_kind, e.scope_id,
                e.issuer_principal_id, e.content_digest, l.log_seq, a.admitted_at_ms,
                c.media_type,
                {retracted_projection}
         FROM semantic_events e
         JOIN event_log l ON l.event_id = e.event_id
         JOIN admission_receipts a ON a.event_id = e.event_id
         JOIN content_objects c ON c.content_digest = e.content_digest
         WHERE e.event_type = ?1"
    );
    if !conditions.is_empty() {
        sql.push_str(" AND ");
        sql.push_str(&conditions.join(" AND "));
    }
    sql.push_str(" ORDER BY l.log_seq");
    if let Some(limit) = bounds.fetch_limit {
        let next = parameters.len() + 1;
        let limit_sql = format!(" LIMIT ?{next}");
        sql.push_str(&limit_sql);
        parameters.push(Box::new(i64::try_from(limit).unwrap_or(i64::MAX)));
    }
    let parameter_refs: Vec<&dyn ToSql> = parameters.iter().map(Box::as_ref).collect();
    let mut statement = connection.prepare(&sql)?;
    let rows = statement
        .query_map(parameter_refs.as_slice(), assertion_columns)?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter().map(assertion_hit_from_columns).collect()
}

/// The SQL fetch window for one pagination round when post-SQL filters are
/// active (audit #30).
///
/// The mode, retraction, and verification filters run *after* the SQL
/// fetch, so the window oversamples the raw row count — factor 4 plus 64
/// rows of slack — which keeps small-limit queries to a single round
/// while any heavier filtering converges through keyset pagination until
/// the caller's `limit` post-filter survivors are collected or the event
/// log is exhausted. Saturating on purpose: a `usize::MAX` limit
/// degenerates to one effectively unbounded pass.
fn sql_fetch_window(limit: usize) -> usize {
    limit.saturating_mul(4).saturating_add(64)
}

fn assertion_hit_from_columns(columns: AssertionColumns) -> Result<AssertionHit, SearchError> {
    let (
        event_id,
        canonical,
        scope_kind,
        scope_id,
        issuer,
        content_digest,
        log_seq,
        admitted,
        media_type,
        retracted,
    ) = columns;
    let event_id = decode_array::<32>(event_id, "event id length is not 32")?;
    let event: UnsignedAssertionEvent =
        decode_unsigned_assertion_event(&canonical).map_err(SearchError::Authority)?;
    if semantic_event_id(&canonical).into_bytes() != event_id {
        return Err(SearchError::CorruptRecord(
            "row event id does not derive from its canonical bytes",
        ));
    }
    let scope = decode_scope(scope_kind, decode_array::<16>(scope_id, "scope id")?)?;
    let issuer = PrincipalId::from_bytes(decode_array::<16>(issuer, "issuer")?);
    let hit = AssertionHit {
        event_id: SemanticEventId::from_bytes(event_id),
        log_seq: decode_u64(log_seq)?,
        scope,
        issuer,
        assertion_mode: event.assertion_mode,
        content_digest: decode_array::<32>(content_digest, "content digest")?,
        content_media_type: media_type,
        admitted_at_ms: decode_u64(admitted)?,
        retracted: retracted != 0,
    };
    Ok(hit)
}

fn encode_scope(scope: CapabilityTarget) -> (i64, [u8; 16]) {
    match scope {
        CapabilityTarget::Namespace(id) => (1, id.into_bytes()),
        CapabilityTarget::Task(id) => (2, id.into_bytes()),
    }
}

fn decode_scope(kind: i64, bytes: [u8; 16]) -> Result<CapabilityTarget, SearchError> {
    match kind {
        1 => Ok(CapabilityTarget::Namespace(
            nlos_types::NamespaceId::from_bytes(bytes),
        )),
        2 => Ok(CapabilityTarget::Task(nlos_types::TaskId::from_bytes(
            bytes,
        ))),
        _ => Err(SearchError::CorruptRecord("scope kind")),
    }
}

fn decode_u64(value: i64) -> Result<u64, SearchError> {
    u64::try_from(value).map_err(|_| SearchError::CorruptRecord("negative integer"))
}

fn decode_array<const N: usize>(
    bytes: Vec<u8>,
    field: &'static str,
) -> Result<[u8; N], SearchError> {
    bytes
        .try_into()
        .map_err(|_| SearchError::CorruptRecord(field))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::SearchService;

    static NEXT: AtomicU64 = AtomicU64::new(1);

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new(label: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos();
            Self(std::env::temp_dir().join(format!(
                "nlos-search-unit-{label}-{}-{nonce}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            )))
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn read_only_connection_rejects_writes() {
        let root = TempRoot::new("readonly");
        let authority =
            Arc::new(nlos_semantic::SemanticAuthority::open(root.path()).expect("open authority"));
        let service = SearchService::open(root.path(), authority).expect("open search face");
        // A valid write statement through the face's own connection must be
        // rejected by SQLite's read-only enforcement, not by policy.
        let result = service
            .connection
            .execute("UPDATE semantic_events SET key_id = key_id WHERE 0", []);
        let error = result.expect_err("write must fail");
        assert!(
            error.to_string().contains("readonly"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn open_fails_closed_without_an_authority_database() {
        let root = TempRoot::new("missing-db");
        let authority_root = TempRoot::new("missing-db-authority");
        let authority = Arc::new(
            nlos_semantic::SemanticAuthority::open(authority_root.path()).expect("open authority"),
        );
        std::fs::create_dir_all(root.path()).expect("create root");
        assert!(SearchService::open(root.path(), authority).is_err());
    }
}
