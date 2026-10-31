//! Typed 7-class ecosystem dependency selector resolution (`[PLAN-DEPENDENCY-001]`,
//! `U-8`) and G3 `WaitingAuthorization` + `Namespace` / `ResourceContract` / `Fanout`
//! authorization-conditioned materialization gate (`[PLAN-LAZY-001]`, `U-7`,
//! `ADR-0027` section 5.2, `RISK-E-05`).
//!
//! [`SqliteEcosystemDependencyGate`] owns its own independent `SQLite` database
//! (`WAL` + `FULL` durability verified on open, `STRICT` tables, write-once
//! immutability triggers) and coordinates seamlessly with [`SqlitePlanAuthority`]
//! through its public state-transition and materialization faces, preserving
//! [`SqlitePlanAuthority`]'s `SCHEMA_VERSION = 9` unchanged.

use std::error::Error;
use std::fmt;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use nlos_types::{IdempotencyKey, NamespaceId, PrincipalId, ReceiptId, TaskNodeId, TaskPlanId};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use crate::PlanStoreError;
use crate::materialization::unresolved_dependencies;
use crate::model::{
    FanoutCondition, MaterializationAdmissionVerdict, MaterializationApproval,
    MaterializationRejection, MaterializationRequest, MaterializationResolution,
    MaterializationResolutionDecision, NamespaceCondition, NodeTransitionRequest, PlanNodeState,
    ResourceContractCondition,
};
use crate::scheduler::{AdmissionConsult, AdmissionConsultOutcome};
use crate::store::{SqlitePlanAuthority, decode_u64, digest16, encode_u64, fixed16};

/// Number of canonical ecosystem dependency entity classes required by
/// `[PLAN-DEPENDENCY-001]` (`Package`, `Skill`, `Tool`, `Model`, `Artifact`,
/// `Topic`, and `ExternalService`).
pub const PLAN_DEPENDENCY_ENTITY_KIND_COUNT: usize = 7;

/// Schema version of `ecosystem-dependency-gate.db`.
pub const ECOSYSTEM_GATE_SCHEMA_VERSION: i64 = 1;

/// Default filename when [`SqliteEcosystemDependencyGate::open`] is given a directory path.
pub const ECOSYSTEM_GATE_DB_FILENAME: &str = "ecosystem-dependency-gate.db";

const DOMAIN_PINNED_HANDLE_RECEIPT_ID: &[u8] = b"nlos/plan/ecosystem-pinned-handle-id/v1";
const DOMAIN_PINNED_HANDLES_ROOT: &[u8] = b"nlos/plan/ecosystem-pinned-handles-root/v1";
const DOMAIN_GATE_DRIVE_KEY: &[u8] = b"nlos/plan/conditioned-gate-drive-key/v1";
const DOMAIN_GATE_AUDIT_RECEIPT_ID: &[u8] = b"nlos/plan/conditioned-gate-audit-receipt/v1";

const ECOSYSTEM_GATE_DDL: &str = "
CREATE TABLE IF NOT EXISTS node_dependency_binding_manifests (
    plan_id           BLOB NOT NULL CHECK (length(plan_id) = 16),
    task_node_id      BLOB NOT NULL CHECK (length(task_node_id) = 16),
    declared_revision INTEGER NOT NULL CHECK (declared_revision >= 1),
    selector_count    INTEGER NOT NULL CHECK (selector_count >= 0),
    idempotency_key   BLOB NOT NULL UNIQUE CHECK (length(idempotency_key) = 16),
    bound_at_ms       INTEGER NOT NULL CHECK (bound_at_ms >= 0),
    PRIMARY KEY (plan_id, task_node_id, declared_revision)
) STRICT;

CREATE TRIGGER IF NOT EXISTS node_dependency_binding_manifests_no_update
BEFORE UPDATE ON node_dependency_binding_manifests
BEGIN
    SELECT RAISE(ABORT, 'node_dependency_binding_manifests rows are immutable');
END;

CREATE TRIGGER IF NOT EXISTS node_dependency_binding_manifests_no_delete
BEFORE DELETE ON node_dependency_binding_manifests
BEGIN
    SELECT RAISE(ABORT, 'node_dependency_binding_manifests rows cannot be deleted');
END;

CREATE TABLE IF NOT EXISTS node_declared_dependency_selectors (
    plan_id                 BLOB NOT NULL CHECK (length(plan_id) = 16),
    task_node_id            BLOB NOT NULL CHECK (length(task_node_id) = 16),
    declared_revision       INTEGER NOT NULL CHECK (declared_revision >= 1),
    selector_slot           INTEGER NOT NULL CHECK (selector_slot >= 0),
    entity_kind             INTEGER NOT NULL CHECK (entity_kind BETWEEN 1 AND 7),
    entity_id               BLOB NOT NULL CHECK (length(entity_id) = 16),
    pin_kind                INTEGER NOT NULL CHECK (pin_kind IN (1, 2)),
    pin_generation          INTEGER NOT NULL CHECK (pin_generation >= 0),
    pin_tag                 TEXT NOT NULL,
    expected_content_digest BLOB CHECK (expected_content_digest IS NULL OR length(expected_content_digest) = 32),
    provenance              INTEGER NOT NULL CHECK (provenance IN (1, 2)),
    PRIMARY KEY (plan_id, task_node_id, declared_revision, selector_slot),
    UNIQUE (plan_id, task_node_id, declared_revision, entity_kind, entity_id),
    FOREIGN KEY (plan_id, task_node_id, declared_revision)
        REFERENCES node_dependency_binding_manifests(plan_id, task_node_id, declared_revision)
) STRICT;

CREATE TRIGGER IF NOT EXISTS node_declared_dependency_selectors_no_update
BEFORE UPDATE ON node_declared_dependency_selectors
BEGIN
    SELECT RAISE(ABORT, 'node_declared_dependency_selectors rows are immutable');
END;

CREATE TRIGGER IF NOT EXISTS node_declared_dependency_selectors_no_delete
BEFORE DELETE ON node_declared_dependency_selectors
BEGIN
    SELECT RAISE(ABORT, 'node_declared_dependency_selectors rows cannot be deleted');
END;

CREATE TABLE IF NOT EXISTS pinned_ecosystem_dependency_handles (
    receipt_id         BLOB PRIMARY KEY NOT NULL CHECK (length(receipt_id) = 16),
    idempotency_key    BLOB NOT NULL UNIQUE CHECK (length(idempotency_key) = 16),
    plan_id            BLOB NOT NULL CHECK (length(plan_id) = 16),
    task_node_id       BLOB NOT NULL CHECK (length(task_node_id) = 16),
    declared_revision  INTEGER NOT NULL CHECK (declared_revision >= 1),
    entity_kind        INTEGER NOT NULL CHECK (entity_kind BETWEEN 1 AND 7),
    entity_id          BLOB NOT NULL CHECK (length(entity_id) = 16),
    pinned_version_tag TEXT NOT NULL CHECK (length(pinned_version_tag) >= 1),
    provenance         INTEGER NOT NULL CHECK (provenance IN (1, 2)),
    pinned_generation  INTEGER NOT NULL CHECK (pinned_generation >= 1),
    content_digest     BLOB NOT NULL CHECK (length(content_digest) = 32),
    resolved_at_ms     INTEGER NOT NULL CHECK (resolved_at_ms >= 0),
    UNIQUE (plan_id, task_node_id, declared_revision, entity_kind, entity_id)
) STRICT;

CREATE TRIGGER IF NOT EXISTS pinned_ecosystem_dependency_handles_no_update
BEFORE UPDATE ON pinned_ecosystem_dependency_handles
BEGIN
    SELECT RAISE(ABORT, 'pinned_ecosystem_dependency_handles rows are immutable');
END;

CREATE TRIGGER IF NOT EXISTS pinned_ecosystem_dependency_handles_no_delete
BEFORE DELETE ON pinned_ecosystem_dependency_handles
BEGIN
    SELECT RAISE(ABORT, 'pinned_ecosystem_dependency_handles rows cannot be deleted');
END;

CREATE TABLE IF NOT EXISTS conditioned_authorization_audit_receipts (
    receipt_id                  BLOB PRIMARY KEY NOT NULL CHECK (length(receipt_id) = 16),
    idempotency_key             BLOB NOT NULL UNIQUE CHECK (length(idempotency_key) = 16),
    plan_id                     BLOB NOT NULL CHECK (length(plan_id) = 16),
    task_node_id                BLOB NOT NULL CHECK (length(task_node_id) = 16),
    observed_revision           INTEGER NOT NULL CHECK (observed_revision >= 1),
    pinned_handles_digest       BLOB NOT NULL CHECK (length(pinned_handles_digest) = 32),
    dag_downstream_fanout       INTEGER NOT NULL CHECK (dag_downstream_fanout >= 0),
    effective_downstream_fanout INTEGER NOT NULL CHECK (effective_downstream_fanout >= 1),
    outcome_code                INTEGER NOT NULL CHECK (outcome_code BETWEEN 1 AND 6),
    materialization_voucher_id  BLOB CHECK (materialization_voucher_id IS NULL OR length(materialization_voucher_id) = 16),
    capability_or_grant_id      BLOB CHECK (capability_or_grant_id IS NULL OR length(capability_or_grant_id) = 16),
    payer_principal_id          BLOB CHECK (payer_principal_id IS NULL OR length(payer_principal_id) = 16),
    denied_namespace_id         BLOB CHECK (denied_namespace_id IS NULL OR length(denied_namespace_id) = 16),
    req_cpu_shares              INTEGER NOT NULL CHECK (req_cpu_shares >= 0),
    req_memory_mib              INTEGER NOT NULL CHECK (req_memory_mib >= 0),
    req_io_weight               INTEGER NOT NULL CHECK (req_io_weight >= 0),
    declared_max_fanout         INTEGER NOT NULL CHECK (declared_max_fanout >= 0),
    task_rejection_kind         INTEGER NOT NULL CHECK (task_rejection_kind IN (0, 1, 2)),
    task_rejection_profile      TEXT,
    task_rejection_observed     INTEGER NOT NULL CHECK (task_rejection_observed >= 0),
    task_rejection_cap          INTEGER NOT NULL CHECK (task_rejection_cap >= 0),
    denial_detail               TEXT NOT NULL,
    evaluated_at_ms             INTEGER NOT NULL CHECK (evaluated_at_ms >= 0)
) STRICT;

CREATE TRIGGER IF NOT EXISTS conditioned_authorization_audit_receipts_no_update
BEFORE UPDATE ON conditioned_authorization_audit_receipts
BEGIN
    SELECT RAISE(ABORT, 'conditioned_authorization_audit_receipts rows are immutable');
END;

CREATE TRIGGER IF NOT EXISTS conditioned_authorization_audit_receipts_no_delete
BEFORE DELETE ON conditioned_authorization_audit_receipts
BEGIN
    SELECT RAISE(ABORT, 'conditioned_authorization_audit_receipts rows cannot be deleted');
END;
";

/// Seven canonical cross-ecosystem dependency entity classes (`[PLAN-DEPENDENCY-001]`, `U-8`).
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum PlanDependencyEntityKind {
    /// Immutable application or driver package (`PackageId`).
    Package = 1,
    /// Declarative or executable agent skill bundle.
    Skill = 2,
    /// Host or sandbox tool endpoint contract.
    Tool = 3,
    /// Pinned foundation or local model weights/adapter revision.
    Model = 4,
    /// Content-addressed `ArtifactStore` revision (`ArtifactId`).
    Artifact = 5,
    /// Typed pub-sub `Topic` schema and stream contract.
    Topic = 6,
    /// External HTTP/RPC service endpoint contract and pinned schema revision.
    ExternalService = 7,
}

impl PlanDependencyEntityKind {
    /// Complete closed set of all seven `[PLAN-DEPENDENCY-001]` entity classes.
    pub const ALL: [Self; PLAN_DEPENDENCY_ENTITY_KIND_COUNT] = [
        Self::Package,
        Self::Skill,
        Self::Tool,
        Self::Model,
        Self::Artifact,
        Self::Topic,
        Self::ExternalService,
    ];

    /// Stable 1-byte wire and `SQLite` discriminant (`1..=7`).
    #[must_use]
    pub const fn encode(self) -> u8 {
        self as u8
    }

    /// Decodes a `SQLite` integer column into a [`PlanDependencyEntityKind`].
    #[must_use]
    pub const fn decode(value: i64) -> Option<Self> {
        match value {
            1 => Some(Self::Package),
            2 => Some(Self::Skill),
            3 => Some(Self::Tool),
            4 => Some(Self::Model),
            5 => Some(Self::Artifact),
            6 => Some(Self::Topic),
            7 => Some(Self::ExternalService),
            _ => None,
        }
    }

    /// Canonical uppercase identifier string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Package => "PACKAGE",
            Self::Skill => "SKILL",
            Self::Tool => "TOOL",
            Self::Model => "MODEL",
            Self::Artifact => "ARTIFACT",
            Self::Topic => "TOPIC",
            Self::ExternalService => "EXTERNAL_SERVICE",
        }
    }
}

impl fmt::Display for PlanDependencyEntityKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Origin of a dependency selector candidate (`发现 != 授权`, `[PLAN-DEPENDENCY-001]`).
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum SelectorProvenance {
    /// Pinned directly inside an audited declarative plan/package manifest.
    DeclaredPinnedManifest = 1,
    /// Explicitly confirmed and locked by a trusted user/operator selection.
    TrustedUserSelection = 2,
    /// Raw discovery or semantic search candidate that has NOT been explicitly
    /// pinned and authorized (`MUST NOT` pass directly into dependency binding).
    UnpinnedSearchResult = 3,
}

impl SelectorProvenance {
    /// Stable 1-byte wire and storage discriminant (`1..=3`).
    #[must_use]
    pub const fn encode(self) -> u8 {
        self as u8
    }

    const fn decode_authorized(code: i64) -> Result<Self, EcosystemGateError> {
        match code {
            1 => Ok(Self::DeclaredPinnedManifest),
            2 => Ok(Self::TrustedUserSelection),
            _ => Err(EcosystemGateError::CorruptRecord(
                "invalid authorized SelectorProvenance in storage",
            )),
        }
    }
}

/// Immutable version or generation lock carried by a [`TypedDependencySelectorProposal`].
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum SelectorVersionPin {
    /// Exact non-zero monotonic generation (`generation >= 1`).
    ExactGeneration(u64),
    /// Exact immutable release or digest tag (floating tags such as `"latest"` or `"*"`
    /// are deterministically rejected by [`parse_version_pin`] and selector validation).
    ExactTag(String),
}

const FORBIDDEN_FLOATING_TAGS: &[&str] = &[
    "latest", "head", "main", "master", "canary", "nightly", "edge", "dev", "trunk", "current",
    "floating", "auto",
];

/// Parses and validates a version/generation selector string, forbidding floating
/// tags (`"latest"`, `"*"`, `"head"`, `"main"`, `"canary"`, `"nightly"`, wildcards,
/// semver ranges, or empty strings) per `[PLAN-DEPENDENCY-001]`.
///
/// Strings prefixed with `"gen:"` (e.g., `"gen:3"`) parse into
/// [`SelectorVersionPin::ExactGeneration`]; all other valid non-floating strings
/// parse into [`SelectorVersionPin::ExactTag`].
///
/// # Errors
///
/// Returns [`EcosystemGateError::FloatingVersionForbidden`] if `raw` is empty,
/// contains whitespace/wildcards/range operators, resolves to generation `0`, or
/// matches any floating alias.
pub fn parse_version_pin(raw: &str) -> Result<SelectorVersionPin, EcosystemGateError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed != raw {
        return Err(EcosystemGateError::FloatingVersionForbidden {
            selector_tag: raw.to_owned(),
        });
    }
    if let Some(gen_suffix) = trimmed.strip_prefix("gen:") {
        let parsed = gen_suffix.parse::<u64>().map_err(|_| {
            EcosystemGateError::FloatingVersionForbidden {
                selector_tag: raw.to_owned(),
            }
        })?;
        if parsed == 0 {
            return Err(EcosystemGateError::FloatingVersionForbidden {
                selector_tag: raw.to_owned(),
            });
        }
        return Ok(SelectorVersionPin::ExactGeneration(parsed));
    }

    validate_exact_tag_str(trimmed)?;
    Ok(SelectorVersionPin::ExactTag(trimmed.to_owned()))
}

fn validate_exact_tag_str(tag: &str) -> Result<(), EcosystemGateError> {
    if tag.is_empty() {
        return Err(EcosystemGateError::FloatingVersionForbidden {
            selector_tag: tag.to_owned(),
        });
    }
    let lower = tag.to_ascii_lowercase();
    if FORBIDDEN_FLOATING_TAGS.contains(&lower.as_str()) {
        return Err(EcosystemGateError::FloatingVersionForbidden {
            selector_tag: tag.to_owned(),
        });
    }
    if tag.bytes().any(|b| {
        b.is_ascii_whitespace() || matches!(b, b'*' | b'^' | b'~' | b'>' | b'<' | b'?' | b'|')
    }) {
        return Err(EcosystemGateError::FloatingVersionForbidden {
            selector_tag: tag.to_owned(),
        });
    }
    if lower.strip_suffix(".x").is_some() || lower.contains(".*") {
        return Err(EcosystemGateError::FloatingVersionForbidden {
            selector_tag: tag.to_owned(),
        });
    }
    Ok(())
}

impl SelectorVersionPin {
    /// Validates that this [`SelectorVersionPin`] carries no floating tag or zero generation.
    ///
    /// # Errors
    ///
    /// Returns [`EcosystemGateError::FloatingVersionForbidden`] on generation `0` or
    /// any floating/wildcard tag.
    pub fn validate(&self) -> Result<(), EcosystemGateError> {
        match self {
            Self::ExactGeneration(0) => Err(EcosystemGateError::FloatingVersionForbidden {
                selector_tag: "gen:0".to_owned(),
            }),
            Self::ExactGeneration(_) => Ok(()),
            Self::ExactTag(tag) => {
                if tag.trim() != tag.as_str() {
                    return Err(EcosystemGateError::FloatingVersionForbidden {
                        selector_tag: tag.clone(),
                    });
                }
                validate_exact_tag_str(tag)
            }
        }
    }

    /// Canonical string representation recorded in durable receipts.
    #[must_use]
    pub fn canonical_tag(&self) -> String {
        match self {
            Self::ExactGeneration(generation) => format!("gen:{generation}"),
            Self::ExactTag(tag) => tag.clone(),
        }
    }
}

/// Unvalidated caller proposal for a `[PLAN-DEPENDENCY-001]` typed dependency selector.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TypedDependencySelectorProposal {
    pub kind: PlanDependencyEntityKind,
    pub entity_id: [u8; 16],
    pub version_or_tag: SelectorVersionPin,
    pub expected_content_digest: Option<[u8; 32]>,
    pub provenance: SelectorProvenance,
}

/// Validated, non-floating, provenance-checked dependency selector (`[PLAN-DEPENDENCY-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TypedDependencySelector {
    kind: PlanDependencyEntityKind,
    entity_id: [u8; 16],
    version_or_tag: SelectorVersionPin,
    expected_content_digest: Option<[u8; 32]>,
    provenance: SelectorProvenance,
}

impl TypedDependencySelector {
    /// Validates a [`TypedDependencySelectorProposal`], rejecting unpinned search
    /// results (`发现 != 授权`) and floating version tags (`"latest"`, `"*"`, etc.).
    ///
    /// # Errors
    ///
    /// - [`EcosystemGateError::UnpinnedSearchResultForbidden`] when
    ///   `proposal.provenance == SelectorProvenance::UnpinnedSearchResult`.
    /// - [`EcosystemGateError::FloatingVersionForbidden`] when `proposal.version_or_tag`
    ///   is empty, wildcarded, generation `0`, or a floating alias.
    pub fn from_proposal(
        proposal: TypedDependencySelectorProposal,
    ) -> Result<Self, EcosystemGateError> {
        if proposal.provenance == SelectorProvenance::UnpinnedSearchResult {
            return Err(EcosystemGateError::UnpinnedSearchResultForbidden {
                kind: proposal.kind,
                entity_id: proposal.entity_id,
            });
        }
        proposal.version_or_tag.validate()?;
        Ok(Self {
            kind: proposal.kind,
            entity_id: proposal.entity_id,
            version_or_tag: proposal.version_or_tag,
            expected_content_digest: proposal.expected_content_digest,
            provenance: proposal.provenance,
        })
    }

    #[must_use]
    pub const fn kind(&self) -> PlanDependencyEntityKind {
        self.kind
    }

    #[must_use]
    pub const fn entity_id(&self) -> [u8; 16] {
        self.entity_id
    }

    #[must_use]
    pub const fn version_or_tag(&self) -> &SelectorVersionPin {
        &self.version_or_tag
    }

    #[must_use]
    pub const fn expected_content_digest(&self) -> Option<[u8; 32]> {
        self.expected_content_digest
    }

    #[must_use]
    pub const fn provenance(&self) -> SelectorProvenance {
        self.provenance
    }
}

/// Concrete immutable entity state returned by a [`CrossEcosystemEntitySource`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EcosystemEntitySnapshot {
    pub pinned_version_tag: String,
    pub pinned_generation: u64,
    pub content_digest: [u8; 32],
}

/// Lookup result returned by [`CrossEcosystemEntitySource`] during initial pin
/// resolution or pre-materialization freshness/revocation re-verification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CrossEcosystemLookup {
    /// Entity revision exists and is active.
    Found(EcosystemEntitySnapshot),
    /// Entity or requested pinned version/generation does not exist.
    NotFound,
    /// Entity or generation was recalled/revoked by store or authority governance.
    RecalledOrRevoked { reason: String },
}

/// Authority adapter boundary resolving all seven [`PlanDependencyEntityKind`] classes
/// into immutable `(pinned_generation, content_digest)` handles (`[PLAN-DEPENDENCY-001]`).
pub trait CrossEcosystemEntitySource {
    /// Transport or backing-store error type of the entity source.
    type Error: fmt::Display;

    /// Returns `true` if this source serves `kind`.
    fn supports_kind(&self, kind: PlanDependencyEntityKind) -> bool;

    /// Resolves a validated [`TypedDependencySelector`] against its backing authority.
    ///
    /// # Errors
    ///
    /// Returns `Self::Error` if the underlying authority cannot be queried.
    fn resolve_pinned(
        &self,
        selector: &TypedDependencySelector,
    ) -> Result<CrossEcosystemLookup, Self::Error>;

    /// Re-verifies an already-resolved [`PinnedEcosystemDependencyHandle`] at the
    /// pre-materialization fence to detect upstream recalls, revocations, or drift.
    ///
    /// # Errors
    ///
    /// Returns `Self::Error` if the underlying authority cannot be queried.
    fn verify_handle_live(
        &self,
        handle: &PinnedEcosystemDependencyHandle,
    ) -> Result<CrossEcosystemLookup, Self::Error>;
}

/// Durable immutable dependency handle produced before a plan node may materialize
/// (`[PLAN-DEPENDENCY-001]`, `U-8`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PinnedEcosystemDependencyHandle {
    pub receipt_id: ReceiptId,
    pub plan_id: TaskPlanId,
    pub node_id: TaskNodeId,
    pub declared_revision: u64,
    pub kind: PlanDependencyEntityKind,
    pub entity_id: [u8; 16],
    pub pinned_version_tag: String,
    pub provenance: SelectorProvenance,
    pub pinned_generation: u64,
    pub content_digest: [u8; 32],
    pub idempotency_key: IdempotencyKey,
    pub resolved_at_ms: u64,
}

/// Outcome of resolving one node ecosystem dependency selector.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PinnedDependencyDecision {
    /// Newly resolved and durably persisted [`PinnedEcosystemDependencyHandle`].
    Resolved(PinnedEcosystemDependencyHandle),
    /// Byte-identical replay answered from the durable handle receipt.
    Replayed(PinnedEcosystemDependencyHandle),
}

impl PinnedDependencyDecision {
    /// Consumes the decision and returns its [`PinnedEcosystemDependencyHandle`].
    #[must_use]
    pub fn into_handle(self) -> PinnedEcosystemDependencyHandle {
        match self {
            Self::Resolved(handle) | Self::Replayed(handle) => handle,
        }
    }
}

/// Request to bind a complete set of `[PLAN-DEPENDENCY-001]` dependency selector
/// proposals to `(plan_id, node_id)` at its current `declared_revision`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BindNodeDependenciesRequest {
    pub plan_id: TaskPlanId,
    pub node_id: TaskNodeId,
    pub proposals: Vec<TypedDependencySelectorProposal>,
    pub idempotency_key: IdempotencyKey,
    pub bound_at_ms: u64,
}

/// Request to resolve one bound `(kind, entity_id)` dependency of `(plan_id, node_id)`
/// into a durable [`PinnedEcosystemDependencyHandle`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResolveNodeDependencyRequest {
    pub plan_id: TaskPlanId,
    pub node_id: TaskNodeId,
    pub kind: PlanDependencyEntityKind,
    pub entity_id: [u8; 16],
    pub idempotency_key: IdempotencyKey,
    pub resolved_at_ms: u64,
}

/// Explicit authorization check verdict for node execution and its resolved handles.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthorizationCheckVerdict {
    /// Explicit capability or operator grant verified active at `at_ms`.
    Authorized { capability_or_grant_id: [u8; 16] },
    /// Capability missing, expired, attenuated out of scope, or revoked.
    Denied { reason: String },
}

/// Namespace scope verdict for [`NamespaceCondition`] (`[PLAN-LAZY-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NamespaceGateVerdict {
    /// Every declared [`NamespaceId`] in [`NamespaceCondition`] is permitted.
    Permitted,
    /// At least one declared [`NamespaceId`] is outside the authorized scope.
    Denied {
        denied_namespace: NamespaceId,
        reason: String,
    },
}

/// Resource quota/ceiling verdict for [`ResourceContractCondition`] (`[PLAN-LAZY-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResourceContractVerdict {
    /// `cpu_shares`, `memory_mib`, and `io_weight` all fit within budget ceilings.
    WithinQuota,
    /// At least one resource dimension exceeds the active allocation or quota ceiling.
    Exceeded { reason: String },
}

/// Downstream fanout & payer verdict for [`FanoutCondition`] (`[PLAN-LAZY-001]`, `[MSG-PAY-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FanoutGateVerdict {
    /// `effective_downstream_fanout` is within budget and backed by `payer_principal_id`.
    Approved { payer_principal_id: PrincipalId },
    /// Fanout bound exceeded or no valid fee payer/grant covers the downstream fanout.
    Denied { reason: String },
}

/// External policy/authority consult boundary enforcing the four G3 pre-execution
/// hard gates before a node in [`PlanNodeState::WaitingAuthorization`] may enter
/// [`PlanNodeState::Materializing`] (`[PLAN-LAZY-001]`, `U-7`).
pub trait NodeExecutionGateConsult {
    /// Failure type when an authority consult is unreachable.
    type Error: fmt::Display;

    /// Gate 1: Verifies explicit capability/grant authorization for executing
    /// `(plan_id, node_id)` with its exact [`PinnedEcosystemDependencyHandle`] set.
    ///
    /// # Errors
    ///
    /// Returns `Self::Error` if the capability authority cannot be queried.
    fn check_authorization(
        &self,
        plan_id: TaskPlanId,
        node_id: TaskNodeId,
        pinned_handles: &[PinnedEcosystemDependencyHandle],
        at_ms: u64,
    ) -> Result<AuthorizationCheckVerdict, Self::Error>;

    /// Gate 2: Verifies that every [`NamespaceId`] declared in `namespace` is within
    /// the node's authorized namespace scope.
    ///
    /// # Errors
    ///
    /// Returns `Self::Error` if the namespace authority cannot be queried.
    fn check_namespace_scope(
        &self,
        plan_id: TaskPlanId,
        node_id: TaskNodeId,
        namespace: &NamespaceCondition,
        at_ms: u64,
    ) -> Result<NamespaceGateVerdict, Self::Error>;

    /// Gate 3: Verifies that `resource_contract` (`cpu_shares`, `memory_mib`, `io_weight`)
    /// fits within the active resource contract and quota ceiling.
    ///
    /// # Errors
    ///
    /// Returns `Self::Error` if the resource authority cannot be queried.
    fn check_resource_contract(
        &self,
        plan_id: TaskPlanId,
        node_id: TaskNodeId,
        resource_contract: &ResourceContractCondition,
        at_ms: u64,
    ) -> Result<ResourceContractVerdict, Self::Error>;

    /// Gate 4: Verifies `fanout.max_downstream_fanout`, actual `dag_downstream_fanout`,
    /// `effective_downstream_fanout = max(declared, dag)`, and fee payer accountability.
    ///
    /// # Errors
    ///
    /// Returns `Self::Error` if the fanout/payer authority cannot be queried.
    fn check_fanout_and_payer(
        &self,
        plan_id: TaskPlanId,
        node_id: TaskNodeId,
        fanout: &FanoutCondition,
        dag_downstream_fanout: u64,
        effective_downstream_fanout: u64,
        at_ms: u64,
    ) -> Result<FanoutGateVerdict, Self::Error>;
}

/// Typed denial reason persisted inside [`ConditionedAuthorizationAuditReceipt`] when
/// a node is held in [`PlanNodeState::WaitingAuthorization`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConditionedGateDenialReason {
    /// Gate 1 denied: missing, expired, or revoked capability/authorization.
    AuthorizationDenied { detail: String },
    /// Gate 2 denied: `denied_namespace` is outside authorized namespace scope.
    NamespaceOutOfScope {
        denied_namespace: NamespaceId,
        detail: String,
    },
    /// Gate 3 denied: `requested` resource contract exceeds quota/ceiling.
    ResourceContractExceeded {
        requested: ResourceContractCondition,
        detail: String,
    },
    /// Gate 4 denied: DAG or declared downstream fanout exceeded bound or lacked payer.
    FanoutOrPayerDenied {
        declared_max_fanout: u64,
        dag_downstream_fanout: u64,
        effective_downstream_fanout: u64,
        detail: String,
    },
    /// Task authority `ScaleProfile` materialization admission denied the round.
    TaskMaterializationRejected(MaterializationRejection),
}

/// Outcome recorded in an immutable [`ConditionedAuthorizationAuditReceipt`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConditionedGateOutcome {
    /// All four G3 condition gates and Task admission passed; node transitioned
    /// `WaitingAuthorization -> Materializing`.
    Approved {
        materialization_voucher_id: ReceiptId,
        capability_or_grant_id: [u8; 16],
        payer_principal_id: PrincipalId,
    },
    /// At least one gate denied; node remained in [`PlanNodeState::WaitingAuthorization`].
    Denied(ConditionedGateDenialReason),
}

/// Immutable durable audit receipt emitted for every G3 conditioned materialization
/// evaluation (`[PLAN-LAZY-001]`, `U-7`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConditionedAuthorizationAuditReceipt {
    pub receipt_id: ReceiptId,
    pub idempotency_key: IdempotencyKey,
    pub plan_id: TaskPlanId,
    pub node_id: TaskNodeId,
    pub observed_revision: u64,
    pub pinned_handles_digest: [u8; 32],
    pub dag_downstream_fanout: u64,
    pub effective_downstream_fanout: u64,
    pub outcome: ConditionedGateOutcome,
    pub evaluated_at_ms: u64,
}

/// Request to advance a ready node to [`PlanNodeState::WaitingAuthorization`], evaluate
/// all G3 conditioned gates, and materialize on full approval.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuthorizeNodeMaterializationRequest {
    pub plan_id: TaskPlanId,
    pub node_id: TaskNodeId,
    pub idempotency_key: IdempotencyKey,
    pub at_ms: u64,
}

/// Decision returned by [`SqliteEcosystemDependencyGate::advance_and_authorize_materialization`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConditionedMaterializationDecision {
    /// Node passed all dependency, authorization, `Namespace`, `ResourceContract`,
    /// `Fanout`, and Task admission gates, transitioning `WaitingAuthorization -> Materializing`.
    Materializing {
        approval: Box<MaterializationApproval>,
        audit_receipt: ConditionedAuthorizationAuditReceipt,
    },
    /// Node was legally advanced to (and held in) [`PlanNodeState::WaitingAuthorization`]
    /// with an immutable [`ConditionedAuthorizationAuditReceipt`] recording the denial.
    DeniedInWaitingAuthorization(ConditionedAuthorizationAuditReceipt),
    /// Byte-identical replay of a previously committed gate evaluation.
    Replayed(ConditionedAuthorizationAuditReceipt),
}

impl ConditionedMaterializationDecision {
    /// Borrows the durable [`ConditionedAuthorizationAuditReceipt`] for this evaluation.
    #[must_use]
    pub const fn audit_receipt(&self) -> &ConditionedAuthorizationAuditReceipt {
        match self {
            Self::Materializing { audit_receipt, .. }
            | Self::DeniedInWaitingAuthorization(audit_receipt)
            | Self::Replayed(audit_receipt) => audit_receipt,
        }
    }
}

/// Typed fail-closed errors emitted by [`SqliteEcosystemDependencyGate`].
#[derive(Debug)]
pub enum EcosystemGateError {
    /// Underlying `SQLite` failure in `ecosystem-dependency-gate.db`.
    Sqlite(rusqlite::Error),
    /// Filesystem I/O failure while creating the gate database directory.
    Io(std::io::Error),
    /// `WAL` + `FULL` durability pragmas could not be verified on open.
    DurabilityUnavailable {
        journal_mode: String,
        synchronous: i64,
    },
    /// Stored `user_version` does not match [`ECOSYSTEM_GATE_SCHEMA_VERSION`].
    SchemaVersionUnsupported(i64),
    /// Corrupt stored row detected during readback verification.
    CorruptRecord(&'static str),
    /// Process-local mutex poisoned.
    LockPoisoned,
    /// Propagated error from [`SqlitePlanAuthority`].
    Plan(PlanStoreError),
    /// Floating version alias (`"latest"`, `"*"`, `"head"`, `"main"`, `"canary"`,
    /// `"nightly"`, empty, wildcard, or `gen:0`) forbidden by `[PLAN-DEPENDENCY-001]`.
    FloatingVersionForbidden { selector_tag: String },
    /// Unpinned discovery/search candidate (`SelectorProvenance::UnpinnedSearchResult`)
    /// forbidden from acting directly as an authorized dependency (`发现 != 授权`).
    UnpinnedSearchResultForbidden {
        kind: PlanDependencyEntityKind,
        entity_id: [u8; 16],
    },
    /// Duplicate `(kind, entity_id)` selector in a single node binding manifest.
    DuplicateDependencySelector {
        kind: PlanDependencyEntityKind,
        entity_id: [u8; 16],
    },
    /// Idempotency key replayed with different request parameters.
    IdempotencyConflict,
    /// Node already bound a conflicting dependency selector manifest for `declared_revision`.
    ConflictingNodeDependencyManifest {
        plan_id: TaskPlanId,
        node_id: TaskNodeId,
        declared_revision: u64,
    },
    /// Node has not bound any `[PLAN-DEPENDENCY-001]` dependency manifest for `declared_revision`.
    NodeDependenciesUnbound {
        plan_id: TaskPlanId,
        node_id: TaskNodeId,
        declared_revision: u64,
    },
    /// Requested `(kind, entity_id)` was not declared in the node's bound manifest.
    SelectorNotDeclaredForNode {
        plan_id: TaskPlanId,
        node_id: TaskNodeId,
        kind: PlanDependencyEntityKind,
        entity_id: [u8; 16],
    },
    /// One or more bound ecosystem dependency selectors have not yet been resolved
    /// into durable [`PinnedEcosystemDependencyHandle`] rows.
    UnresolvedEcosystemDependencies {
        plan_id: TaskPlanId,
        node_id: TaskNodeId,
        unresolved_count: usize,
    },
    /// No entity source registered for `kind`.
    EntitySourceUnavailable { kind: PlanDependencyEntityKind },
    /// Backing entity source failed during lookup or live verification.
    EntitySourceFailed(String),
    /// Entity or requested pinned version/generation was not found in the authority.
    EntityNotFound {
        kind: PlanDependencyEntityKind,
        entity_id: [u8; 16],
    },
    /// Resolved entity generation or content digest disagreed with the selector pin.
    SelectorPinMismatch {
        kind: PlanDependencyEntityKind,
        entity_id: [u8; 16],
        detail: String,
    },
    /// Pre-materialization fence rejected a handle whose upstream entity was recalled or revoked.
    PinnedDependencyRevokedOrRecalled {
        receipt_id: ReceiptId,
        kind: PlanDependencyEntityKind,
        entity_id: [u8; 16],
        reason: String,
    },
    /// Pre-materialization fence detected upstream generation or digest drift against a pinned handle.
    PinnedDependencyFenceDrifted {
        receipt_id: ReceiptId,
        kind: PlanDependencyEntityKind,
        entity_id: [u8; 16],
        pinned_generation: u64,
        live_generation: u64,
    },
    /// Node was declared without structured [`crate::model::NodeConditions`] (`Namespace`,
    /// `ResourceContract`, `Fanout`), so conditioned G3 materialization fails closed.
    MissingStructuredNodeConditions {
        plan_id: TaskPlanId,
        node_id: TaskNodeId,
        declared_revision: u64,
    },
    /// External [`NodeExecutionGateConsult`] or [`AdmissionConsult`] transport failed.
    GateConsultFailed(String),
}

impl fmt::Display for EcosystemGateError {
    #[allow(clippy::too_many_lines)]
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sqlite(err) => write!(formatter, "ecosystem gate SQLite failure: {err}"),
            Self::Io(err) => write!(formatter, "ecosystem gate I/O failure: {err}"),
            Self::DurabilityUnavailable {
                journal_mode,
                synchronous,
            } => write!(
                formatter,
                "ecosystem gate WAL+FULL durability unavailable: journal_mode={journal_mode}, synchronous={synchronous}"
            ),
            Self::SchemaVersionUnsupported(ver) => {
                write!(formatter, "unsupported ecosystem gate schema version {ver}")
            }
            Self::CorruptRecord(reason) => {
                write!(formatter, "corrupt ecosystem gate record: {reason}")
            }
            Self::LockPoisoned => formatter.write_str("ecosystem gate mutex is poisoned"),
            Self::Plan(err) => write!(formatter, "plan authority error: {err}"),
            Self::FloatingVersionForbidden { selector_tag } => write!(
                formatter,
                "floating or wildcard dependency version selector '{selector_tag}' is forbidden (PLAN-DEPENDENCY-001)"
            ),
            Self::UnpinnedSearchResultForbidden { kind, entity_id } => write!(
                formatter,
                "unpinned search result for {kind} {entity_id:02x?} cannot act as an authorized dependency (PLAN-DEPENDENCY-001)"
            ),
            Self::DuplicateDependencySelector { kind, entity_id } => write!(
                formatter,
                "duplicate dependency selector for {kind} {entity_id:02x?}"
            ),
            Self::IdempotencyConflict => {
                formatter.write_str("idempotency key replayed with different request parameters")
            }
            Self::ConflictingNodeDependencyManifest {
                plan_id,
                node_id,
                declared_revision,
            } => write!(
                formatter,
                "conflicting dependency manifest for plan {plan_id:?} node {node_id:?} revision {declared_revision}"
            ),
            Self::NodeDependenciesUnbound {
                plan_id,
                node_id,
                declared_revision,
            } => write!(
                formatter,
                "node {node_id:?} of plan {plan_id:?} revision {declared_revision} has no bound dependency manifest"
            ),
            Self::SelectorNotDeclaredForNode {
                plan_id,
                node_id,
                kind,
                entity_id,
            } => write!(
                formatter,
                "selector {kind} {entity_id:02x?} is not declared for plan {plan_id:?} node {node_id:?}"
            ),
            Self::UnresolvedEcosystemDependencies {
                plan_id,
                node_id,
                unresolved_count,
            } => write!(
                formatter,
                "node {node_id:?} of plan {plan_id:?} still has {unresolved_count} unresolved ecosystem dependencies"
            ),
            Self::EntitySourceUnavailable { kind } => {
                write!(formatter, "no CrossEcosystemEntitySource supports {kind}")
            }
            Self::EntitySourceFailed(detail) => {
                write!(
                    formatter,
                    "CrossEcosystemEntitySource lookup failed: {detail}"
                )
            }
            Self::EntityNotFound { kind, entity_id } => {
                write!(
                    formatter,
                    "ecosystem dependency {kind} {entity_id:02x?} not found"
                )
            }
            Self::SelectorPinMismatch {
                kind,
                entity_id,
                detail,
            } => write!(
                formatter,
                "selector pin mismatch for {kind} {entity_id:02x?}: {detail}"
            ),
            Self::PinnedDependencyRevokedOrRecalled {
                receipt_id,
                kind,
                entity_id,
                reason,
            } => write!(
                formatter,
                "pinned handle {receipt_id:?} ({kind} {entity_id:02x?}) recalled or revoked: {reason}"
            ),
            Self::PinnedDependencyFenceDrifted {
                receipt_id,
                kind,
                entity_id,
                pinned_generation,
                live_generation,
            } => write!(
                formatter,
                "pinned handle {receipt_id:?} ({kind} {entity_id:02x?}) drifted: pinned generation {pinned_generation}, live {live_generation}"
            ),
            Self::MissingStructuredNodeConditions {
                plan_id,
                node_id,
                declared_revision,
            } => write!(
                formatter,
                "node {node_id:?} of plan {plan_id:?} revision {declared_revision} lacks structured NodeConditions (PLAN-LAZY-001)"
            ),
            Self::GateConsultFailed(detail) => {
                write!(
                    formatter,
                    "conditioned execution gate consult failed: {detail}"
                )
            }
        }
    }
}

impl Error for EcosystemGateError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Sqlite(err) => Some(err),
            Self::Io(err) => Some(err),
            Self::Plan(err) => Some(err),
            _ => None,
        }
    }
}

impl From<rusqlite::Error> for EcosystemGateError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Sqlite(err)
    }
}

impl From<PlanStoreError> for EcosystemGateError {
    fn from(err: PlanStoreError) -> Self {
        Self::Plan(err)
    }
}

/// Independent `SQLite`-backed (`WAL` + `FULL`, `STRICT` tables) authority for
/// 7-class `[PLAN-DEPENDENCY-001]` pinned dependency handles and `[PLAN-LAZY-001]`
/// G3 conditioned `WaitingAuthorization -> Materializing` gate enforcement.
pub struct SqliteEcosystemDependencyGate {
    connection: Mutex<Connection>,
}

impl SqliteEcosystemDependencyGate {
    /// Opens or initializes the ecosystem dependency gate `SQLite` database with
    /// verified `WAL` + `FULL` durability.
    ///
    /// If `path` is a directory (or carries no file extension), opens
    /// `<path>/ecosystem-dependency-gate.db`; if `path` carries a file extension,
    /// opens `path` directly.
    ///
    /// # Errors
    ///
    /// Fails closed if the directory cannot be created, `WAL` + `FULL` durability
    /// cannot be verified, or `user_version` is unsupported.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, EcosystemGateError> {
        let raw_path = path.as_ref();
        let db_path = if raw_path.is_dir() {
            raw_path.join(ECOSYSTEM_GATE_DB_FILENAME)
        } else if raw_path.extension().is_some() {
            if let Some(parent) = raw_path.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent).map_err(EcosystemGateError::Io)?;
            }
            raw_path.to_path_buf()
        } else {
            std::fs::create_dir_all(raw_path).map_err(EcosystemGateError::Io)?;
            raw_path.join(ECOSYSTEM_GATE_DB_FILENAME)
        };

        let mut connection = Connection::open(db_path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;

        let journal_mode: String =
            connection.pragma_query_value(None, "journal_mode", |row| row.get(0))?;
        let synchronous: i64 =
            connection.pragma_query_value(None, "synchronous", |row| row.get(0))?;
        if !journal_mode.eq_ignore_ascii_case("wal") || synchronous != 2 {
            return Err(EcosystemGateError::DurabilityUnavailable {
                journal_mode,
                synchronous,
            });
        }

        init_gate_schema(&mut connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>, EcosystemGateError> {
        self.connection
            .lock()
            .map_err(|_| EcosystemGateError::LockPoisoned)
    }

    /// Validates and binds a node's complete `[PLAN-DEPENDENCY-001]` 7-class
    /// dependency selector manifest for its current `declared_revision`.
    ///
    /// Rejects any floating version (`"latest"`, `"*"`, etc.) with
    /// [`EcosystemGateError::FloatingVersionForbidden`] and any unpinned search
    /// result (`SelectorProvenance::UnpinnedSearchResult`) with
    /// [`EcosystemGateError::UnpinnedSearchResultForbidden`] before writing any row.
    ///
    /// # Errors
    ///
    /// Fails closed on unknown plan/node, floating version, unpinned search provenance,
    /// duplicate `(kind, entity_id)` in one manifest, or idempotency/manifest conflict.
    #[allow(clippy::too_many_lines)]
    pub fn bind_node_dependency_proposals(
        &self,
        plan_authority: &SqlitePlanAuthority,
        request: BindNodeDependenciesRequest,
    ) -> Result<Vec<TypedDependencySelector>, EcosystemGateError> {
        let mut validated = Vec::with_capacity(request.proposals.len());
        let mut seen = std::collections::HashSet::with_capacity(request.proposals.len());
        for proposal in request.proposals {
            let selector = TypedDependencySelector::from_proposal(proposal)?;
            if !seen.insert((selector.kind, selector.entity_id)) {
                return Err(EcosystemGateError::DuplicateDependencySelector {
                    kind: selector.kind,
                    entity_id: selector.entity_id,
                });
            }
            validated.push(selector);
        }

        let node = plan_authority
            .inspect_node(request.plan_id, request.node_id)?
            .ok_or(PlanStoreError::NodeNotFound {
                plan_id: request.plan_id,
                node_id: request.node_id,
            })?;

        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        if let Some((existing_plan, existing_node, existing_rev, existing_ts)) =
            load_manifest_by_key(&tx, request.idempotency_key)?
        {
            if existing_plan != request.plan_id
                || existing_node != request.node_id
                || existing_rev != node.declared_revision
                || existing_ts != request.bound_at_ms
            {
                return Err(EcosystemGateError::IdempotencyConflict);
            }
            let stored =
                load_declared_selectors(&tx, request.plan_id, request.node_id, existing_rev)?;
            if stored != validated {
                return Err(EcosystemGateError::IdempotencyConflict);
            }
            tx.commit()?;
            return Ok(stored);
        }

        if manifest_exists_for_node(
            &tx,
            request.plan_id,
            request.node_id,
            node.declared_revision,
        )? {
            return Err(EcosystemGateError::ConflictingNodeDependencyManifest {
                plan_id: request.plan_id,
                node_id: request.node_id,
                declared_revision: node.declared_revision,
            });
        }

        let count_u64 = u64::try_from(validated.len())
            .map_err(|_| EcosystemGateError::CorruptRecord("selector_count overflow"))?;
        tx.execute(
            "INSERT INTO node_dependency_binding_manifests (
                plan_id, task_node_id, declared_revision, selector_count,
                idempotency_key, bound_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                request.plan_id.as_bytes().as_slice(),
                request.node_id.as_bytes().as_slice(),
                encode_gate_u64(node.declared_revision)?,
                encode_gate_u64(count_u64)?,
                request.idempotency_key.as_bytes().as_slice(),
                encode_gate_u64(request.bound_at_ms)?,
            ],
        )?;

        for (slot_idx, selector) in validated.iter().enumerate() {
            let slot_u64 = u64::try_from(slot_idx)
                .map_err(|_| EcosystemGateError::CorruptRecord("selector_slot overflow"))?;
            let (pin_kind, pin_gen, pin_tag) = match &selector.version_or_tag {
                SelectorVersionPin::ExactGeneration(gen_val) => {
                    (1_i64, *gen_val, selector.version_or_tag.canonical_tag())
                }
                SelectorVersionPin::ExactTag(tag) => (2_i64, 0_u64, tag.clone()),
            };
            tx.execute(
                "INSERT INTO node_declared_dependency_selectors (
                    plan_id, task_node_id, declared_revision, selector_slot,
                    entity_kind, entity_id, pin_kind, pin_generation, pin_tag,
                    expected_content_digest, provenance
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    request.plan_id.as_bytes().as_slice(),
                    request.node_id.as_bytes().as_slice(),
                    encode_gate_u64(node.declared_revision)?,
                    encode_gate_u64(slot_u64)?,
                    i64::from(selector.kind.encode()),
                    selector.entity_id.as_slice(),
                    pin_kind,
                    encode_gate_u64(pin_gen)?,
                    pin_tag,
                    selector
                        .expected_content_digest
                        .as_ref()
                        .map(<[u8; 32]>::as_slice),
                    i64::from(selector.provenance.encode()),
                ],
            )?;
        }

        tx.commit()?;
        Ok(validated)
    }

    /// Resolves one bound `[PLAN-DEPENDENCY-001]` selector of `(plan_id, node_id)`
    /// through `source` into an immutable, durable [`PinnedEcosystemDependencyHandle`].
    ///
    /// Supports deterministic idempotent replay across restarts.
    ///
    /// # Errors
    ///
    /// Fails closed if the node or selector is unbound, `source` does not support
    /// `request.kind`, the entity is missing/recalled/revoked, or the returned
    /// generation/digest disagrees with the selector's pinned expectation.
    #[allow(clippy::too_many_lines)]
    pub fn resolve_node_dependency<S: CrossEcosystemEntitySource>(
        &self,
        plan_authority: &SqlitePlanAuthority,
        source: &S,
        request: ResolveNodeDependencyRequest,
    ) -> Result<PinnedDependencyDecision, EcosystemGateError> {
        let node = plan_authority
            .inspect_node(request.plan_id, request.node_id)?
            .ok_or(PlanStoreError::NodeNotFound {
                plan_id: request.plan_id,
                node_id: request.node_id,
            })?;

        // Check idempotent replay first.
        {
            let conn = self.lock()?;
            if let Some(existing) = load_pinned_handle_by_key(&conn, request.idempotency_key)? {
                if existing.plan_id != request.plan_id
                    || existing.node_id != request.node_id
                    || existing.declared_revision != node.declared_revision
                    || existing.kind != request.kind
                    || existing.entity_id != request.entity_id
                    || existing.resolved_at_ms != request.resolved_at_ms
                {
                    return Err(EcosystemGateError::IdempotencyConflict);
                }
                return Ok(PinnedDependencyDecision::Replayed(existing));
            }
        }

        let selector = {
            let conn = self.lock()?;
            if !manifest_exists_for_node(
                &conn,
                request.plan_id,
                request.node_id,
                node.declared_revision,
            )? {
                return Err(EcosystemGateError::NodeDependenciesUnbound {
                    plan_id: request.plan_id,
                    node_id: request.node_id,
                    declared_revision: node.declared_revision,
                });
            }
            load_single_declared_selector(
                &conn,
                request.plan_id,
                request.node_id,
                node.declared_revision,
                request.kind,
                request.entity_id,
            )?
            .ok_or(EcosystemGateError::SelectorNotDeclaredForNode {
                plan_id: request.plan_id,
                node_id: request.node_id,
                kind: request.kind,
                entity_id: request.entity_id,
            })?
        };

        if !source.supports_kind(selector.kind) {
            return Err(EcosystemGateError::EntitySourceUnavailable {
                kind: selector.kind,
            });
        }

        let snapshot = match source
            .resolve_pinned(&selector)
            .map_err(|err| EcosystemGateError::EntitySourceFailed(err.to_string()))?
        {
            CrossEcosystemLookup::Found(snap) => snap,
            CrossEcosystemLookup::NotFound => {
                return Err(EcosystemGateError::EntityNotFound {
                    kind: selector.kind,
                    entity_id: selector.entity_id,
                });
            }
            CrossEcosystemLookup::RecalledOrRevoked { reason } => {
                return Err(EcosystemGateError::SelectorPinMismatch {
                    kind: selector.kind,
                    entity_id: selector.entity_id,
                    detail: format!("entity recalled or revoked: {reason}"),
                });
            }
        };

        validate_snapshot_against_selector(&selector, &snapshot)?;

        let receipt_id = derive_pinned_handle_receipt_id(
            request.idempotency_key,
            request.plan_id,
            request.node_id,
            node.declared_revision,
            selector.kind,
            &selector.entity_id,
            snapshot.pinned_generation,
            &snapshot.content_digest,
        );
        let handle = PinnedEcosystemDependencyHandle {
            receipt_id,
            plan_id: request.plan_id,
            node_id: request.node_id,
            declared_revision: node.declared_revision,
            kind: selector.kind,
            entity_id: selector.entity_id,
            pinned_version_tag: snapshot.pinned_version_tag,
            provenance: selector.provenance,
            pinned_generation: snapshot.pinned_generation,
            content_digest: snapshot.content_digest,
            idempotency_key: request.idempotency_key,
            resolved_at_ms: request.resolved_at_ms,
        };

        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = load_pinned_handle_by_key(&tx, request.idempotency_key)? {
            if existing != handle {
                return Err(EcosystemGateError::IdempotencyConflict);
            }
            tx.commit()?;
            return Ok(PinnedDependencyDecision::Replayed(existing));
        }

        tx.execute(
            "INSERT INTO pinned_ecosystem_dependency_handles (
                receipt_id, idempotency_key, plan_id, task_node_id, declared_revision,
                entity_kind, entity_id, pinned_version_tag, provenance,
                pinned_generation, content_digest, resolved_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                handle.receipt_id.as_bytes().as_slice(),
                handle.idempotency_key.as_bytes().as_slice(),
                handle.plan_id.as_bytes().as_slice(),
                handle.node_id.as_bytes().as_slice(),
                encode_gate_u64(handle.declared_revision)?,
                i64::from(handle.kind.encode()),
                handle.entity_id.as_slice(),
                handle.pinned_version_tag,
                i64::from(handle.provenance.encode()),
                encode_gate_u64(handle.pinned_generation)?,
                handle.content_digest.as_slice(),
                encode_gate_u64(handle.resolved_at_ms)?,
            ],
        )?;
        tx.commit()?;
        Ok(PinnedDependencyDecision::Resolved(handle))
    }

    /// Inspects a single durable [`PinnedEcosystemDependencyHandle`] by [`ReceiptId`].
    ///
    /// # Errors
    ///
    /// Fails closed on corrupt stored records or `SQLite` failure.
    pub fn inspect_pinned_handle(
        &self,
        receipt_id: ReceiptId,
    ) -> Result<Option<PinnedEcosystemDependencyHandle>, EcosystemGateError> {
        let conn = self.lock()?;
        load_pinned_handle_by_id(&conn, receipt_id)
    }

    /// Lists all durable [`PinnedEcosystemDependencyHandle`] rows bound to `(plan_id, node_id)`
    /// at `declared_revision`, deterministically ordered by `(entity_kind, entity_id)`.
    ///
    /// # Errors
    ///
    /// Fails closed on corrupt stored records or `SQLite` failure.
    pub fn list_node_pinned_handles(
        &self,
        plan_id: TaskPlanId,
        node_id: TaskNodeId,
        declared_revision: u64,
    ) -> Result<Vec<PinnedEcosystemDependencyHandle>, EcosystemGateError> {
        let conn = self.lock()?;
        load_node_pinned_handles(&conn, plan_id, node_id, declared_revision)
    }

    /// Lists all immutable [`ConditionedAuthorizationAuditReceipt`] rows recorded for
    /// `(plan_id, node_id)` in evaluation order.
    ///
    /// # Errors
    ///
    /// Fails closed on corrupt stored records or `SQLite` failure.
    pub fn list_node_authorization_audit_receipts(
        &self,
        plan_id: TaskPlanId,
        node_id: TaskNodeId,
    ) -> Result<Vec<ConditionedAuthorizationAuditReceipt>, EcosystemGateError> {
        let conn = self.lock()?;
        load_node_audit_receipts(&conn, plan_id, node_id)
    }

    /// Drives the full `[PLAN-DEPENDENCY-001]` + `[PLAN-LAZY-001]` G3 conditioned
    /// materialization gate for `(request.plan_id, request.node_id)`:
    ///
    /// 1. Verifies all upstream DAG `dependency_keys` in [`SqlitePlanAuthority`] are
    ///    [`PlanNodeState::Completed`] (advancing `Declared -> BlockedDependency` on unmet deps).
    /// 2. Verifies the node bound a `[PLAN-DEPENDENCY-001]` selector manifest, resolved
    ///    100% of declared selectors into durable [`PinnedEcosystemDependencyHandle`]s,
    ///    and re-verifies every handle against `entity_source` (failing closed on recall,
    ///    revocation, deletion, or generation/digest drift).
    /// 3. Requires structured [`crate::model::NodeConditions`] (`Namespace`,
    ///    `ResourceContract`, `Fanout`) and computes `dag_downstream_fanout` from the
    ///    pinned revision DAG edges plus `effective_downstream_fanout = max(declared, dag)`.
    /// 4. Legally advances the node to [`PlanNodeState::WaitingAuthorization`]
    ///    (`Declared/BlockedDependency -> Eligible -> WaitingAuthorization`).
    /// 5. Evaluates `Authorization` + `Namespace` + `ResourceContract` + `Fanout` +
    ///    Task `ScaleProfile` materialization admission:
    ///    - On ANY denial, **holds the node in [`PlanNodeState::WaitingAuthorization`]**
    ///      (never entering `Materializing`) and persists an immutable
    ///      [`ConditionedAuthorizationAuditReceipt`] recording the exact denial reason.
    ///    - Only when ALL checks pass, atomically transitions
    ///      `WaitingAuthorization -> Materializing` via `request_materialization` +
    ///      `resolve_materialization` and persists the approved audit receipt.
    ///
    /// # Errors
    ///
    /// Returns [`EcosystemGateError`] on unmet plan/ecosystem dependencies, recalled or
    /// drifted handles, missing structured `NodeConditions`, idempotency conflicts, or
    /// underlying authority/storage failures.
    #[allow(clippy::too_many_lines)]
    pub fn advance_and_authorize_materialization<S, G, A>(
        &self,
        plan_authority: &SqlitePlanAuthority,
        entity_source: &S,
        gate_consult: &G,
        task_admission: &A,
        request: AuthorizeNodeMaterializationRequest,
    ) -> Result<ConditionedMaterializationDecision, EcosystemGateError>
    where
        S: CrossEcosystemEntitySource,
        G: NodeExecutionGateConsult,
        A: AdmissionConsult,
        A::Error: fmt::Display,
    {
        // 1. Idempotent replay check on the durable conditioned audit ledger.
        {
            let conn = self.lock()?;
            if let Some(existing) = load_audit_receipt_by_key(&conn, request.idempotency_key)? {
                if existing.plan_id != request.plan_id
                    || existing.node_id != request.node_id
                    || existing.evaluated_at_ms != request.at_ms
                {
                    return Err(EcosystemGateError::IdempotencyConflict);
                }
                return Ok(ConditionedMaterializationDecision::Replayed(existing));
            }
        }

        // 2. Inspect node, verify pre-materialization state, and check upstream DAG completion
        //    plus compute true DAG downstream fanout from `plan_revision_edges`.
        let node = plan_authority
            .inspect_node(request.plan_id, request.node_id)?
            .ok_or(PlanStoreError::NodeNotFound {
                plan_id: request.plan_id,
                node_id: request.node_id,
            })?;

        if !matches!(
            node.state,
            PlanNodeState::Declared
                | PlanNodeState::BlockedDependency
                | PlanNodeState::Eligible
                | PlanNodeState::WaitingAuthorization
        ) {
            return Err(EcosystemGateError::Plan(
                PlanStoreError::NodeNotAwaitingMaterialization {
                    node_id: request.node_id,
                    current: node.state,
                },
            ));
        }

        let (unresolved_dag_deps, dag_downstream_fanout) = {
            let plan_conn = plan_authority.lock()?;
            let unresolved = unresolved_dependencies(&plan_conn, &node)?;
            let downstream = count_dag_downstream_dependents(
                &plan_conn,
                request.plan_id,
                node.declared_revision,
                request.node_id,
            )?;
            (unresolved, downstream)
        };

        if !unresolved_dag_deps.is_empty() {
            if node.state == PlanNodeState::Declared {
                plan_authority.record_node_transition(NodeTransitionRequest {
                    plan_id: request.plan_id,
                    node_id: request.node_id,
                    expected_declared_revision: node.declared_revision,
                    from_state: PlanNodeState::Declared,
                    to_state: PlanNodeState::BlockedDependency,
                    idempotency_key: derive_gate_step_key(request.idempotency_key, 0),
                    transitioned_at_ms: request.at_ms,
                })?;
            }
            return Err(EcosystemGateError::Plan(
                PlanStoreError::DependenciesNotReady {
                    node_id: request.node_id,
                    unresolved: unresolved_dag_deps,
                },
            ));
        }

        // 3. Verify 7-class ecosystem dependency binding completeness & live freshness fence.
        let handles = {
            let conn = self.lock()?;
            let Some(declared_count) = load_manifest_selector_count(
                &conn,
                request.plan_id,
                request.node_id,
                node.declared_revision,
            )?
            else {
                return Err(EcosystemGateError::NodeDependenciesUnbound {
                    plan_id: request.plan_id,
                    node_id: request.node_id,
                    declared_revision: node.declared_revision,
                });
            };
            let handles = load_node_pinned_handles(
                &conn,
                request.plan_id,
                request.node_id,
                node.declared_revision,
            )?;
            if handles.len() < declared_count {
                return Err(EcosystemGateError::UnresolvedEcosystemDependencies {
                    plan_id: request.plan_id,
                    node_id: request.node_id,
                    unresolved_count: declared_count - handles.len(),
                });
            }
            handles
        };

        for handle in &handles {
            if !entity_source.supports_kind(handle.kind) {
                return Err(EcosystemGateError::EntitySourceUnavailable { kind: handle.kind });
            }
            match entity_source
                .verify_handle_live(handle)
                .map_err(|err| EcosystemGateError::EntitySourceFailed(err.to_string()))?
            {
                CrossEcosystemLookup::Found(live) => {
                    if live.pinned_generation != handle.pinned_generation
                        || live.content_digest != handle.content_digest
                    {
                        return Err(EcosystemGateError::PinnedDependencyFenceDrifted {
                            receipt_id: handle.receipt_id,
                            kind: handle.kind,
                            entity_id: handle.entity_id,
                            pinned_generation: handle.pinned_generation,
                            live_generation: live.pinned_generation,
                        });
                    }
                }
                CrossEcosystemLookup::NotFound => {
                    return Err(EcosystemGateError::EntityNotFound {
                        kind: handle.kind,
                        entity_id: handle.entity_id,
                    });
                }
                CrossEcosystemLookup::RecalledOrRevoked { reason } => {
                    return Err(EcosystemGateError::PinnedDependencyRevokedOrRecalled {
                        receipt_id: handle.receipt_id,
                        kind: handle.kind,
                        entity_id: handle.entity_id,
                        reason,
                    });
                }
            }
        }

        // 4. Require structured G3 `NodeConditions` (`Namespace` + `ResourceContract` + `Fanout`).
        let conditions = plan_authority
            .inspect_node_conditions(request.plan_id, node.declared_revision, request.node_id)?
            .ok_or(EcosystemGateError::MissingStructuredNodeConditions {
                plan_id: request.plan_id,
                node_id: request.node_id,
                declared_revision: node.declared_revision,
            })?;

        let effective_downstream_fanout = conditions
            .fanout
            .max_downstream_fanout
            .max(dag_downstream_fanout);
        let pinned_handles_digest = compute_pinned_handles_digest(&handles);

        // 5. Legally advance the ready node into `PlanNodeState::WaitingAuthorization`.
        drive_node_to_waiting_authorization(
            plan_authority,
            request.plan_id,
            request.node_id,
            node.declared_revision,
            node.state,
            request.idempotency_key,
            request.at_ms,
        )?;

        // 6. Execute the four G3 pre-execution hard gates while in `WaitingAuthorization`.
        // Gate 6a: Explicit authorization over node & resolved dependency handles.
        let capability_or_grant_id = match gate_consult
            .check_authorization(request.plan_id, request.node_id, &handles, request.at_ms)
            .map_err(|err| EcosystemGateError::GateConsultFailed(err.to_string()))?
        {
            AuthorizationCheckVerdict::Authorized {
                capability_or_grant_id,
            } => capability_or_grant_id,
            AuthorizationCheckVerdict::Denied { reason } => {
                let audit = self.persist_audit_receipt(
                    request,
                    node.declared_revision,
                    pinned_handles_digest,
                    dag_downstream_fanout,
                    effective_downstream_fanout,
                    ConditionedGateOutcome::Denied(
                        ConditionedGateDenialReason::AuthorizationDenied { detail: reason },
                    ),
                )?;
                return Ok(ConditionedMaterializationDecision::DeniedInWaitingAuthorization(audit));
            }
        };

        // Gate 6b: `NodeConditions.namespace` all `NamespaceId` scopes.
        if let NamespaceGateVerdict::Denied {
            denied_namespace,
            reason,
        } = gate_consult
            .check_namespace_scope(
                request.plan_id,
                request.node_id,
                &conditions.namespace,
                request.at_ms,
            )
            .map_err(|err| EcosystemGateError::GateConsultFailed(err.to_string()))?
        {
            let audit = self.persist_audit_receipt(
                request,
                node.declared_revision,
                pinned_handles_digest,
                dag_downstream_fanout,
                effective_downstream_fanout,
                ConditionedGateOutcome::Denied(ConditionedGateDenialReason::NamespaceOutOfScope {
                    denied_namespace,
                    detail: reason,
                }),
            )?;
            return Ok(ConditionedMaterializationDecision::DeniedInWaitingAuthorization(audit));
        }

        // Gate 6c: `NodeConditions.resource_contract` (`cpu_shares`, `memory_mib`, `io_weight`).
        if let ResourceContractVerdict::Exceeded { reason } = gate_consult
            .check_resource_contract(
                request.plan_id,
                request.node_id,
                &conditions.resource_contract,
                request.at_ms,
            )
            .map_err(|err| EcosystemGateError::GateConsultFailed(err.to_string()))?
        {
            let audit = self.persist_audit_receipt(
                request,
                node.declared_revision,
                pinned_handles_digest,
                dag_downstream_fanout,
                effective_downstream_fanout,
                ConditionedGateOutcome::Denied(
                    ConditionedGateDenialReason::ResourceContractExceeded {
                        requested: conditions.resource_contract,
                        detail: reason,
                    },
                ),
            )?;
            return Ok(ConditionedMaterializationDecision::DeniedInWaitingAuthorization(audit));
        }

        // Gate 6d: `NodeConditions.fanout.max_downstream_fanout` vs actual DAG downstream fanout
        // AND payer accountability over `effective_downstream_fanout = max(declared, dag)`.
        let fanout_consult = gate_consult
            .check_fanout_and_payer(
                request.plan_id,
                request.node_id,
                &conditions.fanout,
                dag_downstream_fanout,
                effective_downstream_fanout,
                request.at_ms,
            )
            .map_err(|err| EcosystemGateError::GateConsultFailed(err.to_string()))?;

        if dag_downstream_fanout > conditions.fanout.max_downstream_fanout {
            let detail = match fanout_consult {
                FanoutGateVerdict::Denied { reason } => format!(
                    "DAG downstream fanout {dag_downstream_fanout} exceeds declared max_downstream_fanout {}; payer consult also denied: {reason}",
                    conditions.fanout.max_downstream_fanout
                ),
                FanoutGateVerdict::Approved { .. } => format!(
                    "DAG downstream fanout {dag_downstream_fanout} exceeds node declared max_downstream_fanout {}",
                    conditions.fanout.max_downstream_fanout
                ),
            };
            let audit = self.persist_audit_receipt(
                request,
                node.declared_revision,
                pinned_handles_digest,
                dag_downstream_fanout,
                effective_downstream_fanout,
                ConditionedGateOutcome::Denied(ConditionedGateDenialReason::FanoutOrPayerDenied {
                    declared_max_fanout: conditions.fanout.max_downstream_fanout,
                    dag_downstream_fanout,
                    effective_downstream_fanout,
                    detail,
                }),
            )?;
            return Ok(ConditionedMaterializationDecision::DeniedInWaitingAuthorization(audit));
        }

        let payer_principal_id = match fanout_consult {
            FanoutGateVerdict::Approved { payer_principal_id } => payer_principal_id,
            FanoutGateVerdict::Denied { reason } => {
                let audit = self.persist_audit_receipt(
                    request,
                    node.declared_revision,
                    pinned_handles_digest,
                    dag_downstream_fanout,
                    effective_downstream_fanout,
                    ConditionedGateOutcome::Denied(
                        ConditionedGateDenialReason::FanoutOrPayerDenied {
                            declared_max_fanout: conditions.fanout.max_downstream_fanout,
                            dag_downstream_fanout,
                            effective_downstream_fanout,
                            detail: reason,
                        },
                    ),
                )?;
                return Ok(ConditionedMaterializationDecision::DeniedInWaitingAuthorization(audit));
            }
        };

        // 7. All four G3 condition gates passed while node is in `WaitingAuthorization`.
        //    Consult Task `ScaleProfile` materialization admission, then atomically open
        //    and resolve the `SqlitePlanAuthority` materialization round (`WaitingAuthorization -> Materializing`).
        let other_declared = plan_authority
            .inspect_declared_task_node_count()?
            .saturating_sub(1);
        let admission_outcome = task_admission
            .consult_materialization(other_declared)
            .map_err(|err| EcosystemGateError::GateConsultFailed(err.to_string()))?;

        let mat_request_key = derive_gate_step_key(request.idempotency_key, 3);
        plan_authority.request_materialization(MaterializationRequest {
            plan_id: request.plan_id,
            node_id: request.node_id,
            idempotency_key: mat_request_key,
            requested_at_ms: request.at_ms,
        })?;

        let verdict = match admission_outcome {
            AdmissionConsultOutcome::Admitted(admission) => {
                MaterializationAdmissionVerdict::Approved(admission)
            }
            AdmissionConsultOutcome::Denied(rejection) => {
                MaterializationAdmissionVerdict::Rejected(rejection)
            }
        };

        match plan_authority.resolve_materialization(MaterializationResolution {
            request_key: mat_request_key,
            verdict,
            resolved_at_ms: request.at_ms,
        })? {
            MaterializationResolutionDecision::Approved(approval)
            | MaterializationResolutionDecision::ReplayedApproved(approval) => {
                let audit_receipt = self.persist_audit_receipt(
                    request,
                    node.declared_revision,
                    pinned_handles_digest,
                    dag_downstream_fanout,
                    effective_downstream_fanout,
                    ConditionedGateOutcome::Approved {
                        materialization_voucher_id: approval.voucher.voucher_id,
                        capability_or_grant_id,
                        payer_principal_id,
                    },
                )?;
                Ok(ConditionedMaterializationDecision::Materializing {
                    approval: Box::new(approval),
                    audit_receipt,
                })
            }
            MaterializationResolutionDecision::Rejected(record)
            | MaterializationResolutionDecision::ReplayedRejected(record) => {
                let rejection = record.rejection.ok_or(EcosystemGateError::CorruptRecord(
                    "rejected materialization record missing rejection reason",
                ))?;
                let audit = self.persist_audit_receipt(
                    request,
                    node.declared_revision,
                    pinned_handles_digest,
                    dag_downstream_fanout,
                    effective_downstream_fanout,
                    ConditionedGateOutcome::Denied(
                        ConditionedGateDenialReason::TaskMaterializationRejected(rejection),
                    ),
                )?;
                Ok(ConditionedMaterializationDecision::DeniedInWaitingAuthorization(audit))
            }
        }
    }

    fn persist_audit_receipt(
        &self,
        request: AuthorizeNodeMaterializationRequest,
        observed_revision: u64,
        pinned_handles_digest: [u8; 32],
        dag_downstream_fanout: u64,
        effective_downstream_fanout: u64,
        outcome: ConditionedGateOutcome,
    ) -> Result<ConditionedAuthorizationAuditReceipt, EcosystemGateError> {
        let (
            outcome_code,
            voucher_id,
            cap_id,
            payer_id,
            denied_ns,
            req_cpu,
            req_mem,
            req_io,
            decl_fanout,
            task_rej_kind,
            task_rej_profile,
            task_rej_obs,
            task_rej_cap,
            detail,
        ) = encode_audit_outcome(&outcome);

        let receipt_id = derive_audit_receipt_id(
            request.idempotency_key,
            request.plan_id,
            request.node_id,
            observed_revision,
            &pinned_handles_digest,
            outcome_code,
        );
        let receipt = ConditionedAuthorizationAuditReceipt {
            receipt_id,
            idempotency_key: request.idempotency_key,
            plan_id: request.plan_id,
            node_id: request.node_id,
            observed_revision,
            pinned_handles_digest,
            dag_downstream_fanout,
            effective_downstream_fanout,
            outcome,
            evaluated_at_ms: request.at_ms,
        };

        let mut conn = self.lock()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO conditioned_authorization_audit_receipts (
                receipt_id, idempotency_key, plan_id, task_node_id, observed_revision,
                pinned_handles_digest, dag_downstream_fanout, effective_downstream_fanout,
                outcome_code, materialization_voucher_id, capability_or_grant_id,
                payer_principal_id, denied_namespace_id, req_cpu_shares, req_memory_mib,
                req_io_weight, declared_max_fanout, task_rejection_kind,
                task_rejection_profile, task_rejection_observed, task_rejection_cap,
                denial_detail, evaluated_at_ms
             ) VALUES (
                ?1, ?2, ?3, ?4, ?5,
                ?6, ?7, ?8,
                ?9, ?10, ?11,
                ?12, ?13, ?14, ?15,
                ?16, ?17, ?18,
                ?19, ?20, ?21,
                ?22, ?23
             )",
            params![
                receipt.receipt_id.as_bytes().as_slice(),
                receipt.idempotency_key.as_bytes().as_slice(),
                receipt.plan_id.as_bytes().as_slice(),
                receipt.node_id.as_bytes().as_slice(),
                encode_gate_u64(receipt.observed_revision)?,
                receipt.pinned_handles_digest.as_slice(),
                encode_gate_u64(receipt.dag_downstream_fanout)?,
                encode_gate_u64(receipt.effective_downstream_fanout)?,
                i64::from(outcome_code),
                voucher_id
                    .as_ref()
                    .map(ReceiptId::as_bytes)
                    .map(<[u8; 16]>::as_slice),
                cap_id.as_ref().map(<[u8; 16]>::as_slice),
                payer_id
                    .as_ref()
                    .map(PrincipalId::as_bytes)
                    .map(<[u8; 16]>::as_slice),
                denied_ns
                    .as_ref()
                    .map(NamespaceId::as_bytes)
                    .map(<[u8; 16]>::as_slice),
                encode_gate_u64(req_cpu)?,
                encode_gate_u64(req_mem)?,
                encode_gate_u64(req_io)?,
                encode_gate_u64(decl_fanout)?,
                task_rej_kind,
                task_rej_profile,
                encode_gate_u64(task_rej_obs)?,
                encode_gate_u64(task_rej_cap)?,
                detail,
                encode_gate_u64(receipt.evaluated_at_ms)?,
            ],
        )?;
        tx.commit()?;
        Ok(receipt)
    }
}

// ---------------------------------------------------------------------------
// Internal helpers, schema init, and row codecs
// ---------------------------------------------------------------------------

fn init_gate_schema(connection: &mut Connection) -> Result<(), EcosystemGateError> {
    let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let user_version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
    match user_version {
        0 => {
            tx.execute_batch(ECOSYSTEM_GATE_DDL)?;
            tx.pragma_update(None, "user_version", ECOSYSTEM_GATE_SCHEMA_VERSION)?;
        }
        ECOSYSTEM_GATE_SCHEMA_VERSION => {
            tx.execute_batch(ECOSYSTEM_GATE_DDL)?;
        }
        other => return Err(EcosystemGateError::SchemaVersionUnsupported(other)),
    }
    tx.commit()?;
    Ok(())
}

fn encode_gate_u64(value: u64) -> Result<i64, EcosystemGateError> {
    encode_u64(value).map_err(EcosystemGateError::Plan)
}

fn decode_gate_u64(value: i64) -> Result<u64, EcosystemGateError> {
    decode_u64(value).map_err(EcosystemGateError::Plan)
}

fn gate_fixed16(bytes: Vec<u8>, field: &'static str) -> Result<[u8; 16], EcosystemGateError> {
    fixed16(bytes, field).map_err(EcosystemGateError::Plan)
}

fn gate_fixed32(bytes: Vec<u8>, field: &'static str) -> Result<[u8; 32], EcosystemGateError> {
    bytes
        .try_into()
        .map_err(|_| EcosystemGateError::CorruptRecord(field))
}

fn validate_snapshot_against_selector(
    selector: &TypedDependencySelector,
    snapshot: &EcosystemEntitySnapshot,
) -> Result<(), EcosystemGateError> {
    if snapshot.pinned_generation == 0 {
        return Err(EcosystemGateError::SelectorPinMismatch {
            kind: selector.kind,
            entity_id: selector.entity_id,
            detail: "resolved entity generation must be >= 1".to_owned(),
        });
    }
    validate_exact_tag_str(&snapshot.pinned_version_tag)?;
    match &selector.version_or_tag {
        SelectorVersionPin::ExactGeneration(expected_gen) => {
            if snapshot.pinned_generation != *expected_gen {
                return Err(EcosystemGateError::SelectorPinMismatch {
                    kind: selector.kind,
                    entity_id: selector.entity_id,
                    detail: format!(
                        "expected generation {expected_gen}, got {}",
                        snapshot.pinned_generation
                    ),
                });
            }
        }
        SelectorVersionPin::ExactTag(expected_tag) => {
            if &snapshot.pinned_version_tag != expected_tag {
                return Err(EcosystemGateError::SelectorPinMismatch {
                    kind: selector.kind,
                    entity_id: selector.entity_id,
                    detail: format!(
                        "expected version tag '{expected_tag}', got '{}'",
                        snapshot.pinned_version_tag
                    ),
                });
            }
        }
    }
    if let Some(expected_digest) = selector.expected_content_digest
        && snapshot.content_digest != expected_digest
    {
        return Err(EcosystemGateError::SelectorPinMismatch {
            kind: selector.kind,
            entity_id: selector.entity_id,
            detail: "resolved content_digest does not match selector expected_content_digest"
                .to_owned(),
        });
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn derive_pinned_handle_receipt_id(
    idempotency_key: IdempotencyKey,
    plan_id: TaskPlanId,
    node_id: TaskNodeId,
    declared_revision: u64,
    kind: PlanDependencyEntityKind,
    entity_id: &[u8; 16],
    pinned_generation: u64,
    content_digest: &[u8; 32],
) -> ReceiptId {
    ReceiptId::from_bytes(digest16(
        DOMAIN_PINNED_HANDLE_RECEIPT_ID,
        &[
            idempotency_key.as_bytes(),
            plan_id.as_bytes(),
            node_id.as_bytes(),
            &declared_revision.to_be_bytes(),
            &[kind.encode()],
            entity_id.as_slice(),
            &pinned_generation.to_be_bytes(),
            content_digest.as_slice(),
        ],
    ))
}

fn compute_pinned_handles_digest(handles: &[PinnedEcosystemDependencyHandle]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(DOMAIN_PINNED_HANDLES_ROOT);
    let count = u64::try_from(handles.len()).unwrap_or(u64::MAX);
    hasher.update(count.to_be_bytes());
    for handle in handles {
        hasher.update(handle.receipt_id.as_bytes());
        hasher.update([handle.kind.encode()]);
        hasher.update(handle.entity_id);
        hasher.update(handle.pinned_generation.to_be_bytes());
        hasher.update(handle.content_digest);
    }
    hasher.finalize().into()
}

fn derive_gate_step_key(gate_key: IdempotencyKey, step: u8) -> IdempotencyKey {
    IdempotencyKey::from_bytes(digest16(
        DOMAIN_GATE_DRIVE_KEY,
        &[gate_key.as_bytes(), &[step]],
    ))
}

fn derive_audit_receipt_id(
    idempotency_key: IdempotencyKey,
    plan_id: TaskPlanId,
    node_id: TaskNodeId,
    observed_revision: u64,
    pinned_handles_digest: &[u8; 32],
    outcome_code: u8,
) -> ReceiptId {
    ReceiptId::from_bytes(digest16(
        DOMAIN_GATE_AUDIT_RECEIPT_ID,
        &[
            idempotency_key.as_bytes(),
            plan_id.as_bytes(),
            node_id.as_bytes(),
            &observed_revision.to_be_bytes(),
            pinned_handles_digest.as_slice(),
            &[outcome_code],
        ],
    ))
}

fn count_dag_downstream_dependents(
    plan_conn: &Connection,
    plan_id: TaskPlanId,
    declared_revision: u64,
    node_id: TaskNodeId,
) -> Result<u64, EcosystemGateError> {
    let raw_count: i64 = plan_conn.query_row(
        "SELECT COUNT(*) FROM plan_revision_edges
         WHERE plan_id = ?1 AND revision = ?2 AND dependency_node_id = ?3",
        params![
            plan_id.as_bytes().as_slice(),
            encode_gate_u64(declared_revision)?,
            node_id.as_bytes().as_slice(),
        ],
        |row| row.get(0),
    )?;
    decode_gate_u64(raw_count)
}

fn drive_node_to_waiting_authorization(
    plan_authority: &SqlitePlanAuthority,
    plan_id: TaskPlanId,
    node_id: TaskNodeId,
    declared_revision: u64,
    current_state: PlanNodeState,
    gate_key: IdempotencyKey,
    at_ms: u64,
) -> Result<(), EcosystemGateError> {
    match current_state {
        PlanNodeState::Declared | PlanNodeState::BlockedDependency => {
            plan_authority.record_node_transition(NodeTransitionRequest {
                plan_id,
                node_id,
                expected_declared_revision: declared_revision,
                from_state: current_state,
                to_state: PlanNodeState::Eligible,
                idempotency_key: derive_gate_step_key(gate_key, 1),
                transitioned_at_ms: at_ms,
            })?;
            plan_authority.record_node_transition(NodeTransitionRequest {
                plan_id,
                node_id,
                expected_declared_revision: declared_revision,
                from_state: PlanNodeState::Eligible,
                to_state: PlanNodeState::WaitingAuthorization,
                idempotency_key: derive_gate_step_key(gate_key, 2),
                transitioned_at_ms: at_ms,
            })?;
        }
        PlanNodeState::Eligible => {
            plan_authority.record_node_transition(NodeTransitionRequest {
                plan_id,
                node_id,
                expected_declared_revision: declared_revision,
                from_state: PlanNodeState::Eligible,
                to_state: PlanNodeState::WaitingAuthorization,
                idempotency_key: derive_gate_step_key(gate_key, 2),
                transitioned_at_ms: at_ms,
            })?;
        }
        PlanNodeState::WaitingAuthorization => {}
        other => {
            return Err(EcosystemGateError::Plan(
                PlanStoreError::NodeNotAwaitingMaterialization {
                    node_id,
                    current: other,
                },
            ));
        }
    }
    Ok(())
}

fn load_manifest_by_key(
    conn: &Connection,
    idempotency_key: IdempotencyKey,
) -> Result<Option<(TaskPlanId, TaskNodeId, u64, u64)>, EcosystemGateError> {
    let raw = conn
        .query_row(
            "SELECT plan_id, task_node_id, declared_revision, bound_at_ms
             FROM node_dependency_binding_manifests
             WHERE idempotency_key = ?1",
            [idempotency_key.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Vec<u8>>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((p, n, rev, ts)) = raw else {
        return Ok(None);
    };
    Ok(Some((
        TaskPlanId::from_bytes(gate_fixed16(p, "plan_id")?),
        TaskNodeId::from_bytes(gate_fixed16(n, "task_node_id")?),
        decode_gate_u64(rev)?,
        decode_gate_u64(ts)?,
    )))
}

fn manifest_exists_for_node(
    conn: &Connection,
    plan_id: TaskPlanId,
    node_id: TaskNodeId,
    declared_revision: u64,
) -> Result<bool, EcosystemGateError> {
    Ok(load_manifest_selector_count(conn, plan_id, node_id, declared_revision)?.is_some())
}

fn load_manifest_selector_count(
    conn: &Connection,
    plan_id: TaskPlanId,
    node_id: TaskNodeId,
    declared_revision: u64,
) -> Result<Option<usize>, EcosystemGateError> {
    let raw: Option<i64> = conn
        .query_row(
            "SELECT selector_count FROM node_dependency_binding_manifests
             WHERE plan_id = ?1 AND task_node_id = ?2 AND declared_revision = ?3",
            params![
                plan_id.as_bytes().as_slice(),
                node_id.as_bytes().as_slice(),
                encode_gate_u64(declared_revision)?,
            ],
            |row| row.get(0),
        )
        .optional()?;
    let Some(raw_selector_count) = raw else {
        return Ok(None);
    };
    let decoded = decode_gate_u64(raw_selector_count)?;
    let count = usize::try_from(decoded)
        .map_err(|_| EcosystemGateError::CorruptRecord("selector_count usize"))?;
    Ok(Some(count))
}

type RawSelectorTuple = (i64, Vec<u8>, i64, i64, String, Option<Vec<u8>>, i64);

fn decode_selector_tuple(
    raw: RawSelectorTuple,
) -> Result<TypedDependencySelector, EcosystemGateError> {
    let (kind_i64, eid_vec, pin_kind, pin_gen_i64, pin_tag, exp_digest_opt, prov_i64) = raw;
    let kind = PlanDependencyEntityKind::decode(kind_i64)
        .ok_or(EcosystemGateError::CorruptRecord("unknown entity_kind"))?;
    let entity_id = gate_fixed16(eid_vec, "entity_id")?;
    let version_or_tag = match pin_kind {
        1 => {
            let gen_u64 = decode_gate_u64(pin_gen_i64)?;
            SelectorVersionPin::ExactGeneration(gen_u64)
        }
        2 => SelectorVersionPin::ExactTag(pin_tag),
        _ => return Err(EcosystemGateError::CorruptRecord("unknown pin_kind")),
    };
    version_or_tag.validate()?;
    let expected_content_digest = exp_digest_opt
        .map(|bytes| gate_fixed32(bytes, "expected_content_digest"))
        .transpose()?;
    let provenance = SelectorProvenance::decode_authorized(prov_i64)?;
    Ok(TypedDependencySelector {
        kind,
        entity_id,
        version_or_tag,
        expected_content_digest,
        provenance,
    })
}

fn load_declared_selectors(
    conn: &Connection,
    plan_id: TaskPlanId,
    node_id: TaskNodeId,
    declared_revision: u64,
) -> Result<Vec<TypedDependencySelector>, EcosystemGateError> {
    let mut stmt = conn.prepare(
        "SELECT entity_kind, entity_id, pin_kind, pin_generation, pin_tag,
                expected_content_digest, provenance
         FROM node_declared_dependency_selectors
         WHERE plan_id = ?1 AND task_node_id = ?2 AND declared_revision = ?3
         ORDER BY selector_slot ASC",
    )?;
    let rows = stmt.query_map(
        params![
            plan_id.as_bytes().as_slice(),
            node_id.as_bytes().as_slice(),
            encode_gate_u64(declared_revision)?,
        ],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        },
    )?;
    let mut out = Vec::new();
    for row in rows {
        out.push(decode_selector_tuple(row?)?);
    }
    Ok(out)
}

fn load_single_declared_selector(
    conn: &Connection,
    plan_id: TaskPlanId,
    node_id: TaskNodeId,
    declared_revision: u64,
    kind: PlanDependencyEntityKind,
    entity_id: [u8; 16],
) -> Result<Option<TypedDependencySelector>, EcosystemGateError> {
    let raw: Option<RawSelectorTuple> = conn
        .query_row(
            "SELECT entity_kind, entity_id, pin_kind, pin_generation, pin_tag,
                    expected_content_digest, provenance
             FROM node_declared_dependency_selectors
             WHERE plan_id = ?1 AND task_node_id = ?2 AND declared_revision = ?3
               AND entity_kind = ?4 AND entity_id = ?5",
            params![
                plan_id.as_bytes().as_slice(),
                node_id.as_bytes().as_slice(),
                encode_gate_u64(declared_revision)?,
                i64::from(kind.encode()),
                entity_id.as_slice(),
            ],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .optional()?;
    raw.map(decode_selector_tuple).transpose()
}

type RawPinnedHandleTuple = (
    Vec<u8>,
    Vec<u8>,
    Vec<u8>,
    Vec<u8>,
    i64,
    i64,
    Vec<u8>,
    String,
    i64,
    i64,
    Vec<u8>,
    i64,
);

fn read_raw_pinned_handle(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawPinnedHandleTuple> {
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
    ))
}

fn decode_pinned_handle(
    raw: RawPinnedHandleTuple,
) -> Result<PinnedEcosystemDependencyHandle, EcosystemGateError> {
    let (rid, ikey, pid, nid, rev_i64, kind_i64, eid_vec, ver_tag, prov_i64, gen_i64, cdig, ts) =
        raw;
    let receipt_id = ReceiptId::from_bytes(gate_fixed16(rid, "receipt_id")?);
    let idempotency_key = IdempotencyKey::from_bytes(gate_fixed16(ikey, "idempotency_key")?);
    let plan_id = TaskPlanId::from_bytes(gate_fixed16(pid, "plan_id")?);
    let node_id = TaskNodeId::from_bytes(gate_fixed16(nid, "task_node_id")?);
    let declared_revision = decode_gate_u64(rev_i64)?;
    let kind = PlanDependencyEntityKind::decode(kind_i64).ok_or(
        EcosystemGateError::CorruptRecord("unknown handle entity_kind"),
    )?;
    let entity_id = gate_fixed16(eid_vec, "entity_id")?;
    validate_exact_tag_str(&ver_tag)?;
    let provenance = SelectorProvenance::decode_authorized(prov_i64)?;
    let pinned_generation = decode_gate_u64(gen_i64)?;
    if pinned_generation == 0 {
        return Err(EcosystemGateError::CorruptRecord("zero pinned_generation"));
    }
    let content_digest = gate_fixed32(cdig, "content_digest")?;
    let resolved_at_ms = decode_gate_u64(ts)?;

    let expected_receipt_id = derive_pinned_handle_receipt_id(
        idempotency_key,
        plan_id,
        node_id,
        declared_revision,
        kind,
        &entity_id,
        pinned_generation,
        &content_digest,
    );
    if receipt_id != expected_receipt_id {
        return Err(EcosystemGateError::CorruptRecord(
            "pinned handle receipt_id does not re-derive from stored columns",
        ));
    }

    Ok(PinnedEcosystemDependencyHandle {
        receipt_id,
        plan_id,
        node_id,
        declared_revision,
        kind,
        entity_id,
        pinned_version_tag: ver_tag,
        provenance,
        pinned_generation,
        content_digest,
        idempotency_key,
        resolved_at_ms,
    })
}

fn load_pinned_handle_by_key(
    conn: &Connection,
    idempotency_key: IdempotencyKey,
) -> Result<Option<PinnedEcosystemDependencyHandle>, EcosystemGateError> {
    let raw = conn
        .query_row(
            "SELECT receipt_id, idempotency_key, plan_id, task_node_id, declared_revision,
                    entity_kind, entity_id, pinned_version_tag, provenance,
                    pinned_generation, content_digest, resolved_at_ms
             FROM pinned_ecosystem_dependency_handles
             WHERE idempotency_key = ?1",
            [idempotency_key.as_bytes().as_slice()],
            read_raw_pinned_handle,
        )
        .optional()?;
    raw.map(decode_pinned_handle).transpose()
}

fn load_pinned_handle_by_id(
    conn: &Connection,
    receipt_id: ReceiptId,
) -> Result<Option<PinnedEcosystemDependencyHandle>, EcosystemGateError> {
    let raw = conn
        .query_row(
            "SELECT receipt_id, idempotency_key, plan_id, task_node_id, declared_revision,
                    entity_kind, entity_id, pinned_version_tag, provenance,
                    pinned_generation, content_digest, resolved_at_ms
             FROM pinned_ecosystem_dependency_handles
             WHERE receipt_id = ?1",
            [receipt_id.as_bytes().as_slice()],
            read_raw_pinned_handle,
        )
        .optional()?;
    raw.map(decode_pinned_handle).transpose()
}

fn load_node_pinned_handles(
    conn: &Connection,
    plan_id: TaskPlanId,
    node_id: TaskNodeId,
    declared_revision: u64,
) -> Result<Vec<PinnedEcosystemDependencyHandle>, EcosystemGateError> {
    let mut stmt = conn.prepare(
        "SELECT receipt_id, idempotency_key, plan_id, task_node_id, declared_revision,
                entity_kind, entity_id, pinned_version_tag, provenance,
                pinned_generation, content_digest, resolved_at_ms
         FROM pinned_ecosystem_dependency_handles
         WHERE plan_id = ?1 AND task_node_id = ?2 AND declared_revision = ?3
         ORDER BY entity_kind ASC, entity_id ASC",
    )?;
    let rows = stmt.query_map(
        params![
            plan_id.as_bytes().as_slice(),
            node_id.as_bytes().as_slice(),
            encode_gate_u64(declared_revision)?,
        ],
        read_raw_pinned_handle,
    )?;
    let mut out = Vec::new();
    for row in rows {
        out.push(decode_pinned_handle(row?)?);
    }
    Ok(out)
}

type EncodedAuditOutcome = (
    u8,
    Option<ReceiptId>,
    Option<[u8; 16]>,
    Option<PrincipalId>,
    Option<NamespaceId>,
    u64,
    u64,
    u64,
    u64,
    i64,
    Option<String>,
    u64,
    u64,
    String,
);

#[allow(clippy::too_many_lines)]
fn encode_audit_outcome(outcome: &ConditionedGateOutcome) -> EncodedAuditOutcome {
    match outcome {
        ConditionedGateOutcome::Approved {
            materialization_voucher_id,
            capability_or_grant_id,
            payer_principal_id,
        } => (
            1,
            Some(*materialization_voucher_id),
            Some(*capability_or_grant_id),
            Some(*payer_principal_id),
            None,
            0,
            0,
            0,
            0,
            0,
            None,
            0,
            0,
            String::new(),
        ),
        ConditionedGateOutcome::Denied(ConditionedGateDenialReason::AuthorizationDenied {
            detail,
        }) => (
            2,
            None,
            None,
            None,
            None,
            0,
            0,
            0,
            0,
            0,
            None,
            0,
            0,
            detail.clone(),
        ),
        ConditionedGateOutcome::Denied(ConditionedGateDenialReason::NamespaceOutOfScope {
            denied_namespace,
            detail,
        }) => (
            3,
            None,
            None,
            None,
            Some(*denied_namespace),
            0,
            0,
            0,
            0,
            0,
            None,
            0,
            0,
            detail.clone(),
        ),
        ConditionedGateOutcome::Denied(ConditionedGateDenialReason::ResourceContractExceeded {
            requested,
            detail,
        }) => (
            4,
            None,
            None,
            None,
            None,
            requested.cpu_shares,
            requested.memory_mib,
            requested.io_weight,
            0,
            0,
            None,
            0,
            0,
            detail.clone(),
        ),
        ConditionedGateOutcome::Denied(ConditionedGateDenialReason::FanoutOrPayerDenied {
            declared_max_fanout,
            detail,
            ..
        }) => (
            5,
            None,
            None,
            None,
            None,
            0,
            0,
            0,
            *declared_max_fanout,
            0,
            None,
            0,
            0,
            detail.clone(),
        ),
        ConditionedGateOutcome::Denied(
            ConditionedGateDenialReason::TaskMaterializationRejected(rejection),
        ) => {
            let (kind, profile, obs, cap) = match rejection {
                MaterializationRejection::WorkingSetFull {
                    profile_id,
                    active_count,
                    max_active_working_set,
                } => (
                    1_i64,
                    profile_id.clone(),
                    *active_count,
                    *max_active_working_set,
                ),
                MaterializationRejection::TaskNodeCapExceeded {
                    profile_id,
                    task_count,
                    max_task_nodes,
                } => (2_i64, profile_id.clone(), *task_count, *max_task_nodes),
            };
            (
                6,
                None,
                None,
                None,
                None,
                0,
                0,
                0,
                0,
                kind,
                Some(profile),
                obs,
                cap,
                "task ScaleProfile materialization admission rejected".to_owned(),
            )
        }
    }
}

type RawAuditRow = (
    Vec<u8>,
    Vec<u8>,
    Vec<u8>,
    Vec<u8>,
    i64,
    Vec<u8>,
    i64,
    i64,
    i64,
    Option<Vec<u8>>,
    Option<Vec<u8>>,
    Option<Vec<u8>>,
    Option<Vec<u8>>,
    i64,
    i64,
    i64,
    i64,
    i64,
    Option<String>,
    i64,
    i64,
    String,
    i64,
);

fn read_raw_audit_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawAuditRow> {
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
        row.get(18)?,
        row.get(19)?,
        row.get(20)?,
        row.get(21)?,
        row.get(22)?,
    ))
}

#[allow(clippy::too_many_lines)]
fn decode_audit_row(
    raw: RawAuditRow,
) -> Result<ConditionedAuthorizationAuditReceipt, EcosystemGateError> {
    let (
        rid,
        ikey,
        pid,
        nid,
        rev_i64,
        handles_dig_vec,
        dag_fanout_i64,
        eff_fanout_i64,
        outcome_i64,
        voucher_opt,
        cap_opt,
        payer_opt,
        denied_ns_opt,
        cpu_i64,
        mem_i64,
        io_i64,
        decl_fanout_i64,
        task_rej_kind,
        task_rej_profile,
        task_rej_obs_i64,
        task_rej_cap_i64,
        detail,
        ts_i64,
    ) = raw;

    let receipt_id = ReceiptId::from_bytes(gate_fixed16(rid, "receipt_id")?);
    let idempotency_key = IdempotencyKey::from_bytes(gate_fixed16(ikey, "idempotency_key")?);
    let plan_id = TaskPlanId::from_bytes(gate_fixed16(pid, "plan_id")?);
    let node_id = TaskNodeId::from_bytes(gate_fixed16(nid, "task_node_id")?);
    let observed_revision = decode_gate_u64(rev_i64)?;
    let pinned_handles_digest = gate_fixed32(handles_dig_vec, "pinned_handles_digest")?;
    let dag_downstream_fanout = decode_gate_u64(dag_fanout_i64)?;
    let effective_downstream_fanout = decode_gate_u64(eff_fanout_i64)?;
    let outcome_code = u8::try_from(outcome_i64)
        .map_err(|_| EcosystemGateError::CorruptRecord("outcome_code u8"))?;

    let outcome = match outcome_code {
        1 => {
            let voucher_bytes = voucher_opt.ok_or(EcosystemGateError::CorruptRecord(
                "missing voucher_id on Approved",
            ))?;
            let cap_bytes = cap_opt.ok_or(EcosystemGateError::CorruptRecord(
                "missing capability_id on Approved",
            ))?;
            let payer_bytes = payer_opt.ok_or(EcosystemGateError::CorruptRecord(
                "missing payer_id on Approved",
            ))?;
            ConditionedGateOutcome::Approved {
                materialization_voucher_id: ReceiptId::from_bytes(gate_fixed16(
                    voucher_bytes,
                    "materialization_voucher_id",
                )?),
                capability_or_grant_id: gate_fixed16(cap_bytes, "capability_or_grant_id")?,
                payer_principal_id: PrincipalId::from_bytes(gate_fixed16(
                    payer_bytes,
                    "payer_principal_id",
                )?),
            }
        }
        2 => ConditionedGateOutcome::Denied(ConditionedGateDenialReason::AuthorizationDenied {
            detail,
        }),
        3 => {
            let ns_bytes = denied_ns_opt.ok_or(EcosystemGateError::CorruptRecord(
                "missing denied_namespace_id",
            ))?;
            ConditionedGateOutcome::Denied(ConditionedGateDenialReason::NamespaceOutOfScope {
                denied_namespace: NamespaceId::from_bytes(gate_fixed16(
                    ns_bytes,
                    "denied_namespace_id",
                )?),
                detail,
            })
        }
        4 => {
            ConditionedGateOutcome::Denied(ConditionedGateDenialReason::ResourceContractExceeded {
                requested: ResourceContractCondition {
                    cpu_shares: decode_gate_u64(cpu_i64)?,
                    memory_mib: decode_gate_u64(mem_i64)?,
                    io_weight: decode_gate_u64(io_i64)?,
                },
                detail,
            })
        }
        5 => ConditionedGateOutcome::Denied(ConditionedGateDenialReason::FanoutOrPayerDenied {
            declared_max_fanout: decode_gate_u64(decl_fanout_i64)?,
            dag_downstream_fanout,
            effective_downstream_fanout,
            detail,
        }),
        6 => {
            let profile_id = task_rej_profile.ok_or(EcosystemGateError::CorruptRecord(
                "missing task_rejection_profile",
            ))?;
            let obs = decode_gate_u64(task_rej_obs_i64)?;
            let cap = decode_gate_u64(task_rej_cap_i64)?;
            let rejection = match task_rej_kind {
                1 => MaterializationRejection::WorkingSetFull {
                    profile_id,
                    active_count: obs,
                    max_active_working_set: cap,
                },
                2 => MaterializationRejection::TaskNodeCapExceeded {
                    profile_id,
                    task_count: obs,
                    max_task_nodes: cap,
                },
                _ => {
                    return Err(EcosystemGateError::CorruptRecord(
                        "unknown task_rejection_kind",
                    ));
                }
            };
            ConditionedGateOutcome::Denied(
                ConditionedGateDenialReason::TaskMaterializationRejected(rejection),
            )
        }
        _ => return Err(EcosystemGateError::CorruptRecord("unknown outcome_code")),
    };

    let expected_id = derive_audit_receipt_id(
        idempotency_key,
        plan_id,
        node_id,
        observed_revision,
        &pinned_handles_digest,
        outcome_code,
    );
    if receipt_id != expected_id {
        return Err(EcosystemGateError::CorruptRecord(
            "audit receipt_id does not re-derive from stored columns",
        ));
    }

    Ok(ConditionedAuthorizationAuditReceipt {
        receipt_id,
        idempotency_key,
        plan_id,
        node_id,
        observed_revision,
        pinned_handles_digest,
        dag_downstream_fanout,
        effective_downstream_fanout,
        outcome,
        evaluated_at_ms: decode_gate_u64(ts_i64)?,
    })
}

fn load_audit_receipt_by_key(
    conn: &Connection,
    idempotency_key: IdempotencyKey,
) -> Result<Option<ConditionedAuthorizationAuditReceipt>, EcosystemGateError> {
    let raw = conn
        .query_row(
            "SELECT receipt_id, idempotency_key, plan_id, task_node_id, observed_revision,
                    pinned_handles_digest, dag_downstream_fanout, effective_downstream_fanout,
                    outcome_code, materialization_voucher_id, capability_or_grant_id,
                    payer_principal_id, denied_namespace_id, req_cpu_shares, req_memory_mib,
                    req_io_weight, declared_max_fanout, task_rejection_kind,
                    task_rejection_profile, task_rejection_observed, task_rejection_cap,
                    denial_detail, evaluated_at_ms
             FROM conditioned_authorization_audit_receipts
             WHERE idempotency_key = ?1",
            [idempotency_key.as_bytes().as_slice()],
            read_raw_audit_row,
        )
        .optional()?;
    raw.map(decode_audit_row).transpose()
}

fn load_node_audit_receipts(
    conn: &Connection,
    plan_id: TaskPlanId,
    node_id: TaskNodeId,
) -> Result<Vec<ConditionedAuthorizationAuditReceipt>, EcosystemGateError> {
    let mut stmt = conn.prepare(
        "SELECT receipt_id, idempotency_key, plan_id, task_node_id, observed_revision,
                pinned_handles_digest, dag_downstream_fanout, effective_downstream_fanout,
                outcome_code, materialization_voucher_id, capability_or_grant_id,
                payer_principal_id, denied_namespace_id, req_cpu_shares, req_memory_mib,
                req_io_weight, declared_max_fanout, task_rejection_kind,
                task_rejection_profile, task_rejection_observed, task_rejection_cap,
                denial_detail, evaluated_at_ms
         FROM conditioned_authorization_audit_receipts
         WHERE plan_id = ?1 AND task_node_id = ?2
         ORDER BY evaluated_at_ms ASC, rowid ASC",
    )?;
    let rows = stmt.query_map(
        params![plan_id.as_bytes().as_slice(), node_id.as_bytes().as_slice()],
        read_raw_audit_row,
    )?;
    let mut out = Vec::new();
    for row in rows {
        out.push(decode_audit_row(row?)?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::{
        EcosystemGateError, PLAN_DEPENDENCY_ENTITY_KIND_COUNT, PlanDependencyEntityKind,
        SelectorProvenance, SelectorVersionPin, TypedDependencySelector,
        TypedDependencySelectorProposal, parse_version_pin,
    };

    #[test]
    fn plan_dependency_covers_seven_entity_classes() {
        assert_eq!(PLAN_DEPENDENCY_ENTITY_KIND_COUNT, 7);
        assert_eq!(
            PlanDependencyEntityKind::ALL.len(),
            PLAN_DEPENDENCY_ENTITY_KIND_COUNT
        );
        for (idx, kind) in PlanDependencyEntityKind::ALL.into_iter().enumerate() {
            let expected_code = u8::try_from(idx + 1).expect("1..=7");
            assert_eq!(kind.encode(), expected_code);
            assert_eq!(
                PlanDependencyEntityKind::decode(i64::from(expected_code)),
                Some(kind)
            );
            assert_ne!(kind.as_str(), "");
        }
    }

    #[test]
    fn version_pin_parser_rejects_floating_tags_and_accepts_pinned_versions() {
        for forbidden in [
            "", "   ", "latest", "LATEST", "*", "head", "HEAD", "main", "canary", "nightly", "1.*",
            "2.x", "^1.2.3", "~1.0.0", ">=1.0", "gen:0", " v1.0.0 ",
        ] {
            assert!(
                matches!(
                    parse_version_pin(forbidden),
                    Err(EcosystemGateError::FloatingVersionForbidden { .. })
                ),
                "expected floating rejection for '{forbidden}'"
            );
        }

        assert_eq!(
            parse_version_pin("gen:7").expect("gen:7 valid"),
            SelectorVersionPin::ExactGeneration(7)
        );
        assert_eq!(
            parse_version_pin("v1.4.2").expect("v1.4.2 valid"),
            SelectorVersionPin::ExactTag("v1.4.2".to_owned())
        );

        let search_rejected =
            TypedDependencySelector::from_proposal(TypedDependencySelectorProposal {
                kind: PlanDependencyEntityKind::Tool,
                entity_id: [0x22; 16],
                version_or_tag: SelectorVersionPin::ExactGeneration(2),
                expected_content_digest: None,
                provenance: SelectorProvenance::UnpinnedSearchResult,
            });
        assert!(matches!(
            search_rejected,
            Err(EcosystemGateError::UnpinnedSearchResultForbidden {
                kind: PlanDependencyEntityKind::Tool,
                ..
            })
        ));
    }
}
