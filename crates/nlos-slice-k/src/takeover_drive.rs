//! Cross-Cell takeover drive (`CS0-A` + `CS1-D`) — the `DIST-TASK-002`
//! linearized CAS chain, `DIST-TASK-004` successor-registry baseline,
//! `nlos-task` schema `v47` (`CS1-C`) production takeover consumption, and
//! `nlos-cell` federation `MigrationIntent` + `ReconciliationCheckpoint`
//! cryptographic record binding (`CS1-D`).
//!
//! **What this module is.** A transport-neutral production orchestrator over
//! the landed local gates of [`SqliteTaskAuthority`], [`IdentityAuthority`],
//! and [`CellDirectory`]:
//! `prepare_authority_takeover_fence_with_receipt_extensions` → optional
//! signed [`ParticipantRegistryRecord`] snapshot verification → one signed
//! `record_authority_takeover_barrier_receipt_signed` per exact-fence
//! manifest member → `complete_authority_takeover` →
//! `reopen_successor_registry` → optional [`bind_takeover_to_federation`]
//! (`MigrationIntent` + `CheckpointFact` binding and fail-closed
//! [`verify_dist_task_binding`]). The drive adds no store semantics of its
//! own: every durable transition is the store's own single-transaction gate;
//! this module sequences them, fail-closes on readbacks that do not match the
//! gates' contracts, and returns the durable facts as one outcome record for
//! evidence assertions.
//!
//! **Production transport boundary (`c-shard-plan.md` R2 + `CS1-D`).**
//! [`BarrierObservationSource`] together with [`drive_cross_cell_takeover`] /
//! [`drive_cross_cell_takeover_with_options`] is the canonical
//! transport-neutral production orchestrator seam over [`SqliteTaskAuthority`]
//! and [`IdentityAuthority`], while the `nlos-takeover-control` SABI handler
//! remains the canonical local/IPC reference handler over the exact same
//! signed store gates (`record_authority_takeover_barrier_receipt_signed` +
//! `IdentityAuthority::verify_barrier_observation_signature`). In the
//! ADR-0018 single-host multi-Cell topology (one OS process = one Cell;
//! shared kernel, clock domain, and filesystem), the successor opens the
//! shared durable task store and obtains endpoint-signed barrier observations
//! through [`BarrierObservationSource`]. No cross-machine network protocol or
//! Raft/consensus protocol is invented or implied.
//!
//! **Schema `v47` (`CS1-C`) & federation binding (`CS1-D`) semantics.**
//! - **Signed `ParticipantRegistryRecord` snapshot verification**:
//!   [`participant_registry_snapshot_signature_message`],
//!   [`record_verified_participant_registry_signature`], and
//!   [`verify_frozen_registry_signature`] verify an `nlos-identity`
//!   `BarrierObservationSigning` principal signature over the frozen
//!   registry's immutable generation commitment (`registry_id`, `task_id`,
//!   `task_generation`, `generation`, `prior_root`, `root`) and persist the
//!   verified `(authority, signature)` pair on `ParticipantRegistryRecord`.
//! - **Takeover receipt extensions**: [`ReceiptExtensions`] propagates
//!   `outstanding_effect_quarantine_root` (either supplied explicitly or
//!   resolved fail-closed from a quarantined permit's
//!   `QuarantineReceiptRecord` via `quarantined_permit_id`) and the passive
//!   `consensus_commit_index` metadata slot (which carries **zero** consensus
//!   protocol semantics per ADR-0018 Decision 5 and ADR-0020) into the
//!   durable `AuthorityTakeoverReceiptRecord`.
//! - **Assignment 5-state transitions (`Active`, `TakeoverPending`, `Fenced`,
//!   `Expired`, `Quarantined`)**: [`expire_authority_assignment`] enforces an
//!   anchored lease-expiry check before transitioning an assignment to
//!   `Expired`, [`quarantine_authority_assignment`] transitions an `Active` or
//!   `TakeoverPending` assignment to `Quarantined`, and
//!   [`ProductionTakeoverOptions::quarantine_on_barrier_failure`] implements
//!   the `[DIST-TASK-002]` tail clause by quarantining a `TakeoverPending`
//!   incumbent assignment when barrier collection or coverage fails.
//! - **Federation `MigrationIntent` + `ReconciliationCheckpoint` binding**:
//!   [`bind_takeover_to_federation`] and
//!   [`drive_takeover_with_federation_binding`] project the takeover's durable
//!   `AuthorityAssignmentRecord`, `AuthorityTakeoverReceiptRecord`, and
//!   `ParticipantRegistryRecord` into [`DistTaskRecordBinding`]s, persist
//!   bound `MigrationIntent`s and a `CheckpointFact` in [`CellDirectory`], and
//!   verify them fail-closed via [`verify_dist_task_binding`].
//!
//! **What this module is not.** It does not touch the Cell epoch: the only
//! epoch-advance entry stays the quota family's
//! `advance_epoch_and_quarantine` (pinned by the `cell_host` assembly); this
//! drive walks the `TaskAuthority` lease/term/fencing machinery exclusively.
//! Evidence produced through this drive is single-host dual-process evidence:
//! citing it as cross-host, network-partition, or cross-clock evidence would
//! violate `RISK-B-11`.

use std::error::Error;
use std::fmt;

use nlos_cell::{
    CellDirectory, CellEpoch, CellFencingToken, CellIdentity, CheckpointDigest, CheckpointFact,
    CheckpointPrefix, CheckpointRecord, DistTaskBindingVerification, DistTaskMigrationKind,
    DistTaskRecordBinding, FederationError, MigrationIntent, MigrationIntentRecord,
    verify_dist_task_binding,
};
use nlos_identity::{
    IdentityAuthority, IdentityAuthorityError, VerifiedBarrierObservationSigner,
    VerifyBarrierObservationSignatureRequest,
};
use nlos_task::{
    AuthorityAssignmentRecord, AuthorityAssignmentState, AuthorityLeaseDecision,
    AuthorityLeaseRecord, AuthorityLeaseRequest, AuthorityLeaseTakeoverFenceRecord,
    AuthorityLeaseTakeoverFenceRequest, AuthoritySuccessorRegistryReopenRecord,
    AuthoritySuccessorRegistryReopenRequest, AuthorityTakeoverBarrierCoverageState,
    AuthorityTakeoverBarrierReceiptRecord, AuthorityTakeoverBarrierReceiptRequest,
    AuthorityTakeoverCompletionRecord, AuthorityTakeoverFenceMemberRecord,
    AuthorityTakeoverReceiptRecord, AuthorityTakeoverReceiptState, BarrierObservationSignature,
    CompleteAuthorityTakeoverRequest, ParticipantRecord, ParticipantRegistryBinding,
    ParticipantRegistryRecord, ParticipantRegistryState, SqliteTaskAuthority, TaskStoreError,
};
use nlos_types::{
    CommitPermitId, ControlDomainId, Generation, IdempotencyKey, KeyId, PrincipalId, ProcessId,
    ReceiptId, TaskId,
};
use sha2::{Digest, Sha256};

/// Domain-separation tag for signing and verifying a
/// [`ParticipantRegistryRecord`] generation snapshot (`CS1-D`).
pub const PARTICIPANT_REGISTRY_SNAPSHOT_SIGNATURE_DOMAIN: &[u8] =
    b"llmos/participant-registry-snapshot-signature/v1";

/// Principal signature material over [`participant_registry_snapshot_signature_message`]
/// (`CS1-D`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegistrySnapshotSignature {
    /// Principal identity of the signer (`authority` column on
    /// `ParticipantRegistryRecord`).
    pub issuer: PrincipalId,
    /// Control domain the signing key is bound to.
    pub control_domain_id: ControlDomainId,
    /// Signing key identifier (`KeyPurpose::BarrierObservationSigning`).
    pub key_id: KeyId,
    /// 64-byte Ed25519 signature over
    /// [`participant_registry_snapshot_signature_message`].
    pub signature: [u8; 64],
}

/// Optional §26.1 schema `v47` metadata extensions recorded on a
/// `TaskAuthorityTakeoverReceipt` (`CS1-C` / `CS1-D`).
///
/// **Semantic boundary (ADR-0018 Decision 5 & ADR-0020)**:
/// `consensus_commit_index` is a passive metadata slot only and carries
/// **zero** Raft / multi-node consensus protocol semantics.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ReceiptExtensions {
    /// Optional 32-byte `outstanding_effect_quarantine_root`.
    pub outstanding_effect_quarantine_root: Option<[u8; 32]>,
    /// Optional passive `consensus_commit_index` slot (zero consensus
    /// protocol semantics).
    pub consensus_commit_index: Option<u64>,
}

/// Optional Phase 1 (`CS1-D`) production takeover controls layered onto
/// [`CrossCellTakeoverRequest`] via [`drive_cross_cell_takeover_with_options`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ProductionTakeoverOptions {
    /// Optional `v47` takeover receipt extensions (`outstanding_effect_quarantine_root`
    /// and passive `consensus_commit_index`).
    pub receipt_extensions: ReceiptExtensions,
    /// Optional quarantined `CommitPermitId` whose durable `QuarantineReceiptRecord`
    /// supplies (or cross-checks) `outstanding_effect_quarantine_root` on the
    /// takeover receipt.
    pub quarantined_permit_id: Option<CommitPermitId>,
    /// Optional principal signature over the frozen
    /// [`ParticipantRegistryRecord`] snapshot, verified through
    /// [`IdentityAuthority`] and persisted before takeover completion.
    pub frozen_registry_signer: Option<RegistrySnapshotSignature>,
    /// When `true`, any barrier collection or barrier coverage failure after
    /// the fence gate transitions the incumbent `TakeoverPending` assignment
    /// to `Quarantined` (`[DIST-TASK-002]` tail clause).
    pub quarantine_on_barrier_failure: bool,
}

/// Durable federation `MigrationIntent` + `ReconciliationCheckpoint` records
/// and `DIST-TASK` bindings produced by [`bind_takeover_to_federation`]
/// (`CS1-D`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TakeoverFederationBindingOutcome {
    /// Cryptographic binding for the fenced incumbent assignment.
    pub old_assignment_binding: DistTaskRecordBinding,
    /// Cryptographic binding for the completed `AuthorityTakeoverReceiptRecord`.
    pub takeover_receipt_binding: DistTaskRecordBinding,
    /// Cryptographic binding for the reopened successor active assignment.
    pub active_assignment_binding: DistTaskRecordBinding,
    /// Cryptographic binding for the reopened successor
    /// `ParticipantRegistryRecord`.
    pub reopened_registry_binding: DistTaskRecordBinding,
    /// Recorded `MigrationIntent` bound to `takeover_receipt_binding`.
    pub takeover_receipt_intent: MigrationIntentRecord,
    /// Recorded `MigrationIntent` bound to `active_assignment_binding`.
    pub active_assignment_intent: MigrationIntentRecord,
    /// Recorded `MigrationIntent` bound to `reopened_registry_binding`.
    pub reopened_registry_intent: MigrationIntentRecord,
    /// Canonical `CheckpointPrefix` containing all four `DTBINDV1` entries.
    pub checkpoint_prefix: CheckpointPrefix,
    /// Structured `CheckpointDigest` over `checkpoint_prefix`.
    pub checkpoint_digest: CheckpointDigest,
    /// Recorded `CheckpointRecord` in the target Cell's checkpoint log.
    pub checkpoint_record: CheckpointRecord,
}

/// Inputs for [`bind_takeover_to_federation`] and
/// [`drive_takeover_with_federation_binding`] (`CS1-D`).
#[derive(Clone, Copy, Debug)]
pub struct TakeoverFederationContext<'a> {
    /// Shared federation directory where intents and checkpoints are recorded.
    pub directory: &'a CellDirectory,
    /// Old authority Cell identity (`source` of the `MigrationIntent`s).
    pub source_cell: CellIdentity,
    /// Successor authority Cell identity (`target` of the `MigrationIntent`s
    /// and owner of the recorded `CheckpointFact`).
    pub target_cell: CellIdentity,
    /// Successor Cell's node boot generation for the `CheckpointFact`.
    pub checkpoint_boot_generation: Generation,
    /// Successor Cell's epoch for the `CheckpointFact`.
    pub checkpoint_epoch: CellEpoch,
    /// Successor Cell's fencing token for the `CheckpointFact`.
    pub checkpoint_fencing_token: CellFencingToken,
}

/// What one fence-member endpoint is asked to attest: the pending
/// takeover receipt it is being barriered for, its own frozen participant
/// identity, and the exact fence-set root the observation must bind.
///
/// This is the successor's *demand*; the endpoint host answers with a
/// [`SignedBarrierObservation`]. The demand/response split is the seam
/// where the dual-process harness crosses the OS process boundary — the drive
/// itself never learns how the answer travelled.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct BarrierObservationDemand {
    /// Pending `TaskAuthorityTakeoverReceipt` the observation covers.
    pub takeover_receipt_id: ReceiptId,
    /// The endpoint's frozen participant record from the exact-fence
    /// manifest.
    pub participant: ParticipantRecord,
    /// Exact fence-set root of the frozen takeover fence.
    pub fence_set_root: [u8; 32],
}

/// One endpoint's answer to a [`BarrierObservationDemand`]: the endpoint's
/// own durable observation receipt identity and digest, plus the
/// principal Ed25519 signature over the domain-separated observation
/// message the store recomputes and verifies.
///
/// Every field is endpoint-authored material — the successor carries it
/// verbatim into the store's signed gate and the identity authority, not
/// the successor, decides whether it is trustworthy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SignedBarrierObservation {
    /// Endpoint-durable receipt identity for this barrier observation.
    pub remote_receipt_id: ReceiptId,
    /// Endpoint-authored barrier digest.
    pub barrier_digest: [u8; 32],
    /// Principal Ed25519 signature over the observation message.
    pub signature: BarrierObservationSignature,
}

/// Source of signed barrier observations for one takeover drive — the
/// endpoint-facing half of the drive. Implemented by whatever carries the
/// successor's demand to the fence-member endpoints and back; the
/// dual-process harness implements it over temp-file signaling to the old
/// Cell's process (see `tests/dual_cell_takeover.rs`).
///
/// # Errors
///
/// Implementations report their own channel failures through
/// [`TakeoverDriveError::BarrierSource`]; the drive fail-closes on the
/// first error and writes nothing further.
pub trait BarrierObservationSource {
    /// Obtains one endpoint's signed observation for the given demand.
    ///
    /// # Errors
    ///
    /// Returns a [`TakeoverDriveError::BarrierSource`] failure when the
    /// endpoint channel cannot deliver or the answer is unusable.
    fn observe_barrier(
        &mut self,
        demand: BarrierObservationDemand,
    ) -> Result<SignedBarrierObservation, TakeoverDriveError>;
}

/// Inputs of one cross-Cell takeover drive. The store and identity
/// handles are opened by the caller — the successor opens the old
/// authority's task store (and the shared identity root) directly off the
/// shared filesystem; the drive is agnostic to how they were obtained.
pub struct CrossCellTakeoverRequest<'a, S> {
    /// The durable task authority the chain is driven against.
    pub store: &'a SqliteTaskAuthority,
    /// Identity authority that verifies barrier-observation signatures.
    pub identity: &'a IdentityAuthority,
    /// Endpoint channel used for the per-member barrier demands.
    pub barrier_source: &'a mut S,
    /// Task whose authority is being taken over.
    pub task_id: TaskId,
    /// Lease holder identity of the successor (its process).
    pub successor_holder_id: ProcessId,
    /// Idempotency key of the successor lease request. Re-driving with
    /// the same key replays the same durable lease instead of minting a
    /// new term.
    pub lease_idempotency_key: IdempotencyKey,
    /// TTL of the successor lease. The drive's completion and reopen
    /// gates revalidate the lease against the clock, so the caller must
    /// keep the logical clock inside this window.
    pub lease_ttl_ms: i64,
    /// Caller-supplied logical clock (the store layer holds no clock by
    /// convention). Drawn once per gate in drive order; readings must be
    /// non-negative and monotone for the store's timestamp gates to
    /// accept them.
    pub now_ms: &'a mut dyn FnMut() -> i64,
}

/// Durable facts of one completed cross-Cell takeover drive: the
/// successor lease, the frozen fence with its exact roots and manifest,
/// the pending receipt as it stood right after the fence gate, every
/// stored signed barrier observation, the completion record, the completed
/// takeover receipt readback (`v47`), the optional verified frozen-registry
/// signer proof (`v47`), and the successor-registry reopen with the final
/// readbacks. Assertable field by field as the schema `v27`–`v47` durable
/// evidence of the drive.
#[derive(Clone, Debug)]
pub struct CrossCellTakeoverOutcome {
    /// The successor term's lease record (term advanced from the old
    /// holder's).
    pub successor_lease: AuthorityLeaseRecord,
    /// Old assignment as read before the drive (the CAS chain's subject).
    pub old_assignment: AuthorityAssignmentRecord,
    /// Registry record as frozen by the fence gate
    /// (`FrozenForTakeover`), including any verified `v47` `(authority,
    /// signature)` pair.
    pub frozen_registry: ParticipantRegistryRecord,
    /// Verified `nlos-identity` signer proof for `frozen_registry` when
    /// [`ProductionTakeoverOptions::frozen_registry_signer`] was supplied.
    pub verified_registry_signer: Option<VerifiedBarrierObservationSigner>,
    /// Immutable `FROZEN_FOR_TAKEOVER` fence receipt with the exact
    /// roots.
    pub fence_receipt: AuthorityLeaseTakeoverFenceRecord,
    /// Exact-fence member manifest (the barrier coverage set).
    pub fence_members: Vec<AuthorityTakeoverFenceMemberRecord>,
    /// Pending takeover receipt as read between the fence gate and
    /// completion (`Pending`, no successor assignment yet).
    pub takeover_receipt_pending: AuthorityTakeoverReceiptRecord,
    /// Stored signed barrier observations, one per manifest member.
    pub observations: Vec<AuthorityTakeoverBarrierReceiptRecord>,
    /// Completion record (`Pending → Complete`, successor assignment
    /// activated).
    pub completion: AuthorityTakeoverCompletionRecord,
    /// Completed takeover receipt readback (`Complete`, carrying
    /// `new_assignment_id`, `outstanding_effect_quarantine_root`, and
    /// passive `consensus_commit_index`).
    pub takeover_receipt_completed: AuthorityTakeoverReceiptRecord,
    /// Successor-registry reopen record (new generation, new root, new
    /// active assignment).
    pub reopen: AuthoritySuccessorRegistryReopenRecord,
    /// Active assignment after the reopen, bound to the successor
    /// registry generation.
    pub active_assignment: AuthorityAssignmentRecord,
    /// Registry record after the reopen (new `Open` generation).
    pub reopened_registry: ParticipantRegistryRecord,
}

/// Fail-closed errors of the cross-Cell takeover drive. The store's own
/// gate failures surface verbatim as [`TakeoverDriveError::Store`]; the
/// remaining variants name drive-level preconditions, signature checks,
/// federation bindings, and readbacks.
#[derive(Debug)]
pub enum TakeoverDriveError {
    /// A store gate refused the drive; carried verbatim.
    Store(TaskStoreError),
    /// A federation directory operation refused the drive.
    Federation(FederationError),
    /// Federation `MigrationIntent` or `CheckpointFact` binding verification
    /// failed closed.
    FederationBindingVerification(DistTaskBindingVerification),
    /// `nlos-identity` rejected the frozen `ParticipantRegistryRecord`
    /// snapshot signature.
    RegistrySignatureInvalid(IdentityAuthorityError),
    /// Durable `ParticipantRegistryRecord` `(authority, signature)` columns
    /// did not match the verified `RegistrySnapshotSignature`.
    RegistrySignatureMismatch,
    /// A `quarantined_permit_id` was supplied in [`ProductionTakeoverOptions`]
    /// but no durable `QuarantineReceiptRecord` exists for that permit.
    QuarantineReceiptMissing(CommitPermitId),
    /// [`expire_authority_assignment`] refused because the assignment's
    /// bound lease has not yet expired under the anchored clock.
    AssignmentLeaseStillLive {
        /// Lease expiration timestamp bound on the assignment.
        expires_at_ms: i64,
        /// Effective anchored observation timestamp (`min(expired_at_ms, wall_now_ms)`).
        effective_now_ms: i64,
    },
    /// The caller's logical clock produced a negative reading.
    NegativeClock,
    /// There is no authority lease yet to take over (`Acquired` shape) —
    /// a cross-Cell takeover needs an incumbent term.
    NoIncumbentAuthorityLease,
    /// The successor holder is the incumbent lease holder (`Renewed`
    /// shape) — not a cross-Cell takeover.
    SuccessorIsIncumbentHolder,
    /// The task has no current authority assignment to freeze.
    NoActiveAssignment,
    /// The current assignment is not `Active` (already takeover-pending
    /// or fenced).
    OldAssignmentNotActive {
        /// The assignment state the drive refused to continue from.
        state: AuthorityAssignmentState,
    },
    /// The frozen fence has no exact fence-set root, so the barrier
    /// coverage set cannot be resolved and the chain cannot proceed.
    FenceManifestUnavailable,
    /// The takeover receipt is already complete — the drive is one-shot
    /// per receipt; its durable facts are the evidence, not a re-drive.
    TakeoverAlreadyComplete,
    /// Barrier coverage is not locally complete before completion.
    BarrierCoverageIncomplete {
        /// The coverage state the drive refused to complete from.
        state: AuthorityTakeoverBarrierCoverageState,
        /// How many manifest members still lack a stored observation.
        missing: usize,
    },
    /// The endpoint channel could not deliver a signed observation.
    BarrierSource(String),
    /// A post-gate readback contradicted the gate's contract — treated
    /// as corruption of the drive's own expectations.
    PostCondition(&'static str),
}

impl fmt::Display for TakeoverDriveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "task authority gate: {error}"),
            Self::Federation(error) => write!(formatter, "federation directory gate: {error}"),
            Self::FederationBindingVerification(verification) => write!(
                formatter,
                "federation dist-task binding verification failed: {verification:?}"
            ),
            Self::RegistrySignatureInvalid(error) => {
                write!(formatter, "participant registry signature invalid: {error}")
            }
            Self::RegistrySignatureMismatch => write!(
                formatter,
                "participant registry durable signature mismatches verified signer"
            ),
            Self::QuarantineReceiptMissing(permit_id) => write!(
                formatter,
                "quarantine receipt missing for permit {permit_id:?}"
            ),
            Self::AssignmentLeaseStillLive {
                expires_at_ms,
                effective_now_ms,
            } => write!(
                formatter,
                "authority assignment lease is still live at {effective_now_ms} (expires at {expires_at_ms})"
            ),
            Self::NegativeClock => {
                write!(
                    formatter,
                    "takeover drive clock produced a negative reading"
                )
            }
            Self::NoIncumbentAuthorityLease => write!(
                formatter,
                "no incumbent authority lease to take over (lease was acquired, not taken over)"
            ),
            Self::SuccessorIsIncumbentHolder => write!(
                formatter,
                "successor holder is the incumbent lease holder (lease was renewed, not taken over)"
            ),
            Self::NoActiveAssignment => {
                write!(formatter, "task has no current authority assignment")
            }
            Self::OldAssignmentNotActive { state } => write!(
                formatter,
                "current authority assignment is not active anymore: {state:?}"
            ),
            Self::FenceManifestUnavailable => write!(
                formatter,
                "frozen takeover fence has no exact fence-set root (manifest unavailable)"
            ),
            Self::TakeoverAlreadyComplete => write!(
                formatter,
                "takeover receipt is already complete; the drive is one-shot per receipt"
            ),
            Self::BarrierCoverageIncomplete { state, missing } => write!(
                formatter,
                "barrier coverage is not locally complete before completion: {state:?}, {missing} member(s) missing"
            ),
            Self::BarrierSource(reason) => {
                write!(formatter, "barrier observation channel: {reason}")
            }
            Self::PostCondition(reason) => {
                write!(
                    formatter,
                    "takeover drive post-condition violated: {reason}"
                )
            }
        }
    }
}

impl Error for TakeoverDriveError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            Self::Federation(error) => Some(error),
            Self::RegistrySignatureInvalid(error) => Some(error),
            _ => None,
        }
    }
}

impl From<TaskStoreError> for TakeoverDriveError {
    fn from(error: TaskStoreError) -> Self {
        Self::Store(error)
    }
}

impl From<FederationError> for TakeoverDriveError {
    fn from(error: FederationError) -> Self {
        Self::Federation(error)
    }
}

/// Draws the next logical timestamp, rejecting negative readings the
/// store's gates would refuse anyway.
fn next_now(now_ms: &mut dyn FnMut() -> i64) -> Result<i64, TakeoverDriveError> {
    let now = now_ms();
    if now < 0 {
        return Err(TakeoverDriveError::NegativeClock);
    }
    Ok(now)
}

/// Computes the domain-separated `SHA-256` message digest for signing or
/// verifying a [`ParticipantRegistryRecord`] generation snapshot (`CS1-D`):
/// `SHA-256("llmos/participant-registry-snapshot-signature/v1" ||
/// registry_id (16B) || task_id (16B) || be64(task_generation) ||
/// be64(generation) || prior_root (32B) || root (32B))`.
///
/// Because all six axes are immutable across `Open → FrozenForPermit →
/// FrozenForTakeover` within a single registry generation, an authority can
/// sign the snapshot either when active or when frozen and any verifier
/// recomputes the identical digest.
#[must_use]
pub fn participant_registry_snapshot_signature_message(
    registry: &ParticipantRegistryRecord,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(PARTICIPANT_REGISTRY_SNAPSHOT_SIGNATURE_DOMAIN);
    hasher.update(registry.registry_id.as_bytes());
    hasher.update(registry.task_id.as_bytes());
    hasher.update(registry.task_generation.get().to_be_bytes());
    hasher.update(registry.generation.to_be_bytes());
    hasher.update(registry.prior_root);
    hasher.update(registry.root);
    hasher.finalize().into()
}

/// Verifies a [`RegistrySnapshotSignature`] over `registry` through
/// [`IdentityAuthority::verify_barrier_observation_signature`] and checks
/// that any already-persisted `(authority, signature)` columns on `registry`
/// match byte-for-byte (`CS1-D`).
///
/// # Errors
///
/// Returns [`TakeoverDriveError::NegativeClock`] when `verified_at_ms < 0`,
/// [`TakeoverDriveError::RegistrySignatureInvalid`] when `nlos-identity`
/// rejects the principal, control domain, key purpose, validity window,
/// revocation state, or Ed25519 signature, or
/// [`TakeoverDriveError::RegistrySignatureMismatch`] when `registry` already
/// carries a different durable `(authority, signature)`.
pub fn verify_frozen_registry_signature(
    identity: &IdentityAuthority,
    registry: &ParticipantRegistryRecord,
    signer: RegistrySnapshotSignature,
    verified_at_ms: i64,
) -> Result<VerifiedBarrierObservationSigner, TakeoverDriveError> {
    let verified_at_u64 =
        u64::try_from(verified_at_ms).map_err(|_| TakeoverDriveError::NegativeClock)?;
    let message_digest = participant_registry_snapshot_signature_message(registry);
    let proof = identity
        .verify_barrier_observation_signature(VerifyBarrierObservationSignatureRequest {
            message_digest,
            issuer: signer.issuer,
            control_domain_id: signer.control_domain_id,
            key_id: signer.key_id,
            signature: signer.signature,
            verified_at_ms: verified_at_u64,
        })
        .map_err(TakeoverDriveError::RegistrySignatureInvalid)?;
    if registry
        .authority
        .is_some_and(|authority| authority != *signer.issuer.as_bytes())
        || registry
            .signature
            .is_some_and(|signature| signature != signer.signature)
    {
        return Err(TakeoverDriveError::RegistrySignatureMismatch);
    }
    Ok(proof)
}

/// Verifies `signer` against the current durable [`ParticipantRegistryRecord`]
/// for `task_id` at `expected_binding` through [`IdentityAuthority`] and
/// persists the verified `(authority, signature)` pair via
/// [`SqliteTaskAuthority::record_participant_registry_signature`] (`CS1-D`).
///
/// # Errors
///
/// Returns [`TakeoverDriveError::Store`] if the registry does not match
/// `expected_binding` or has a conflicting durable signature, or
/// [`TakeoverDriveError::RegistrySignatureInvalid`] /
/// [`TakeoverDriveError::RegistrySignatureMismatch`] if signature
/// verification fails.
pub fn record_verified_participant_registry_signature(
    store: &SqliteTaskAuthority,
    identity: &IdentityAuthority,
    task_id: TaskId,
    expected_binding: ParticipantRegistryBinding,
    signer: RegistrySnapshotSignature,
    signed_at_ms: i64,
) -> Result<(ParticipantRegistryRecord, VerifiedBarrierObservationSigner), TakeoverDriveError> {
    if signed_at_ms < 0 {
        return Err(TakeoverDriveError::NegativeClock);
    }
    let current = store.inspect_participant_registry(task_id)?;
    if current.generation != expected_binding.generation || current.root != expected_binding.root {
        return Err(TakeoverDriveError::Store(
            TaskStoreError::ParticipantRegistryBindingMismatch,
        ));
    }
    let proof = verify_frozen_registry_signature(identity, &current, signer, signed_at_ms)?;
    let updated = store.record_participant_registry_signature(
        task_id,
        expected_binding,
        *signer.issuer.as_bytes(),
        signer.signature,
        signed_at_ms,
    )?;
    if updated.authority != Some(*signer.issuer.as_bytes())
        || updated.signature != Some(signer.signature)
    {
        return Err(TakeoverDriveError::RegistrySignatureMismatch);
    }
    Ok((updated, proof))
}

/// Transitions the current assignment of `task_id` to the terminal §26.1
/// `Expired` (`4`) state after verifying that the assignment's bound lease
/// has expired under the anchored clock (`min(expired_at_ms, wall_now_ms) >=
/// expires_at_ms`) (`CS1-D`).
///
/// Follows the same fail-closed anchored clock discipline as
/// [`SqliteTaskAuthority::acquire_authority_lease_anchored`]: `wall_now_ms == 0`
/// means no independent wall observation is available and fails closed as
/// live ([`TakeoverDriveError::AssignmentLeaseStillLive`]).
///
/// # Errors
///
/// Returns [`TakeoverDriveError::NegativeClock`] on negative `expired_at_ms`,
/// [`TakeoverDriveError::NoActiveAssignment`] if the task has no assignment,
/// [`TakeoverDriveError::AssignmentLeaseStillLive`] if the assignment's
/// bound lease is still live under the anchored clock, or
/// [`TakeoverDriveError::Store`] if the store gate rejects the transition.
pub fn expire_authority_assignment(
    store: &SqliteTaskAuthority,
    task_id: TaskId,
    wall_now_ms: u64,
    expired_at_ms: i64,
) -> Result<AuthorityAssignmentRecord, TakeoverDriveError> {
    if expired_at_ms < 0 {
        return Err(TakeoverDriveError::NegativeClock);
    }
    let current = match store.inspect_authority_assignment(task_id) {
        Ok(record) => record,
        Err(TaskStoreError::ReceiptNotFound) => return Err(TakeoverDriveError::NoActiveAssignment),
        Err(error) => return Err(error.into()),
    };
    if current.state != AuthorityAssignmentState::Expired {
        let effective_now_ms = if wall_now_ms == 0 {
            i64::MIN
        } else {
            let wall_ms = i64::try_from(wall_now_ms).unwrap_or(i64::MAX);
            expired_at_ms.min(wall_ms)
        };
        if current.authority_lease_binding.expires_at_ms > effective_now_ms {
            return Err(TakeoverDriveError::AssignmentLeaseStillLive {
                expires_at_ms: current.authority_lease_binding.expires_at_ms,
                effective_now_ms,
            });
        }
    }
    let updated =
        store.expire_authority_assignment(task_id, current.assignment_id, expired_at_ms)?;
    if updated.state != AuthorityAssignmentState::Expired {
        return Err(TakeoverDriveError::PostCondition(
            "expire_authority_assignment did not leave assignment in Expired state",
        ));
    }
    Ok(updated)
}

/// Transitions the current `Active` or `TakeoverPending` assignment of
/// `task_id` to the terminal §26.1 `Quarantined` (`5`) state (`CS1-D`,
/// `[DIST-TASK-002]` tail clause).
///
/// # Errors
///
/// Returns [`TakeoverDriveError::NegativeClock`] on negative
/// `quarantined_at_ms`, [`TakeoverDriveError::NoActiveAssignment`] if the
/// task has no assignment, or [`TakeoverDriveError::Store`] if the
/// assignment is already `Fenced` or `Expired`.
pub fn quarantine_authority_assignment(
    store: &SqliteTaskAuthority,
    task_id: TaskId,
    quarantined_at_ms: i64,
) -> Result<AuthorityAssignmentRecord, TakeoverDriveError> {
    if quarantined_at_ms < 0 {
        return Err(TakeoverDriveError::NegativeClock);
    }
    let current = match store.inspect_authority_assignment(task_id) {
        Ok(record) => record,
        Err(TaskStoreError::ReceiptNotFound) => return Err(TakeoverDriveError::NoActiveAssignment),
        Err(error) => return Err(error.into()),
    };
    let updated =
        store.quarantine_authority_assignment(task_id, current.assignment_id, quarantined_at_ms)?;
    if updated.state != AuthorityAssignmentState::Quarantined {
        return Err(TakeoverDriveError::PostCondition(
            "quarantine_authority_assignment did not leave assignment in Quarantined state",
        ));
    }
    Ok(updated)
}

/// Projects an [`AuthorityAssignmentRecord`] into its canonical
/// [`DistTaskRecordBinding`] (`CS1-D`).
#[must_use]
pub fn dist_task_binding_for_assignment(
    record: &AuthorityAssignmentRecord,
) -> DistTaskRecordBinding {
    DistTaskRecordBinding {
        kind: DistTaskMigrationKind::AuthorityAssignment,
        task_id: *record.task_id.as_bytes(),
        task_generation: record.task_generation.get(),
        record_id: *record.assignment_id.as_bytes(),
        authority_term: record.authority_lease_binding.term,
        control_epoch: record.control_epoch,
        participant_registry_generation: record.participant_registry_binding.generation,
        participant_registry_root: record.participant_registry_binding.root,
        exact_fence_set_root: None,
    }
}

/// Projects an [`AuthorityTakeoverReceiptRecord`] into its canonical
/// [`DistTaskRecordBinding`] (`CS1-D`).
#[must_use]
pub fn dist_task_binding_for_takeover_receipt(
    record: &AuthorityTakeoverReceiptRecord,
) -> DistTaskRecordBinding {
    DistTaskRecordBinding {
        kind: DistTaskMigrationKind::TakeoverReceipt,
        task_id: *record.task_id.as_bytes(),
        task_generation: record.task_generation.get(),
        record_id: *record.receipt_id.as_bytes(),
        authority_term: record.new_authority_lease_binding.term,
        control_epoch: record.new_control_epoch,
        participant_registry_generation: record.frozen_registry_binding.generation,
        participant_registry_root: record.frozen_registry_binding.root,
        exact_fence_set_root: record.exact_fence_set_root,
    }
}

/// Projects a [`ParticipantRegistryRecord`] at `(authority_term,
/// control_epoch)` into its canonical [`DistTaskRecordBinding`] (`CS1-D`).
#[must_use]
pub fn dist_task_binding_for_registry(
    record: &ParticipantRegistryRecord,
    authority_term: u64,
    control_epoch: u64,
) -> DistTaskRecordBinding {
    DistTaskRecordBinding {
        kind: DistTaskMigrationKind::ParticipantRegistry,
        task_id: *record.task_id.as_bytes(),
        task_generation: record.task_generation.get(),
        record_id: *record.registry_id.as_bytes(),
        authority_term,
        control_epoch,
        participant_registry_generation: record.generation,
        participant_registry_root: record.root,
        exact_fence_set_root: None,
    }
}

/// Records cryptographically bound [`MigrationIntent`]s and a
/// [`CheckpointFact`] in `context.directory` for a completed
/// [`CrossCellTakeoverOutcome`] and verifies every binding fail-closed via
/// [`verify_dist_task_binding`] (`CS1-D`).
///
/// # Errors
///
/// Returns [`TakeoverDriveError::Federation`] if recording an intent or
/// checkpoint fails, or [`TakeoverDriveError::FederationBindingVerification`]
/// if any post-record cryptographic binding verification does not return
/// [`DistTaskBindingVerification::Verified`].
pub fn bind_takeover_to_federation(
    context: TakeoverFederationContext<'_>,
    outcome: &CrossCellTakeoverOutcome,
) -> Result<TakeoverFederationBindingOutcome, TakeoverDriveError> {
    let old_assignment_binding = dist_task_binding_for_assignment(&outcome.old_assignment);
    let takeover_receipt_binding =
        dist_task_binding_for_takeover_receipt(&outcome.takeover_receipt_completed);
    let active_assignment_binding = dist_task_binding_for_assignment(&outcome.active_assignment);
    let reopened_registry_binding = dist_task_binding_for_registry(
        &outcome.reopened_registry,
        outcome.successor_lease.term,
        outcome.active_assignment.control_epoch,
    );

    let takeover_intent = MigrationIntent::from_dist_task_binding(
        context.source_cell,
        context.target_cell,
        &takeover_receipt_binding,
    )?;
    let takeover_receipt_intent = context
        .directory
        .record_migration_intent(&takeover_intent)?;

    let assignment_intent = MigrationIntent::from_dist_task_binding(
        context.source_cell,
        context.target_cell,
        &active_assignment_binding,
    )?;
    let active_assignment_intent = context
        .directory
        .record_migration_intent(&assignment_intent)?;

    let registry_intent = MigrationIntent::from_dist_task_binding(
        context.source_cell,
        context.target_cell,
        &reopened_registry_binding,
    )?;
    let reopened_registry_intent = context
        .directory
        .record_migration_intent(&registry_intent)?;

    let checkpoint_prefix = CheckpointPrefix::from_dist_task_bindings(&[
        old_assignment_binding,
        takeover_receipt_binding,
        active_assignment_binding,
        reopened_registry_binding,
    ]);
    let checkpoint_digest = CheckpointDigest::from_prefix(
        context.checkpoint_boot_generation,
        context.checkpoint_epoch,
        context.checkpoint_fencing_token,
        &checkpoint_prefix,
    );
    let checkpoint_fact = CheckpointFact::new(
        context.checkpoint_boot_generation,
        context.checkpoint_epoch,
        context.checkpoint_fencing_token,
        &checkpoint_digest.to_text(),
    )?;
    let checkpoint_record = context
        .directory
        .record_checkpoint(context.target_cell, &checkpoint_fact)?;

    for (intent_record, expected_binding) in [
        (&takeover_receipt_intent, &takeover_receipt_binding),
        (&active_assignment_intent, &active_assignment_binding),
        (&reopened_registry_intent, &reopened_registry_binding),
    ] {
        let verification = verify_dist_task_binding(
            intent_record.intent(),
            checkpoint_record.fact(),
            &checkpoint_prefix,
            expected_binding,
        );
        if verification != DistTaskBindingVerification::Verified {
            return Err(TakeoverDriveError::FederationBindingVerification(
                verification,
            ));
        }
    }
    let old_verification = checkpoint_record
        .fact()
        .verify_dist_task_binding(&checkpoint_prefix, &old_assignment_binding);
    if old_verification != DistTaskBindingVerification::Verified {
        return Err(TakeoverDriveError::FederationBindingVerification(
            old_verification,
        ));
    }

    Ok(TakeoverFederationBindingOutcome {
        old_assignment_binding,
        takeover_receipt_binding,
        active_assignment_binding,
        reopened_registry_binding,
        takeover_receipt_intent,
        active_assignment_intent,
        reopened_registry_intent,
        checkpoint_prefix,
        checkpoint_digest,
        checkpoint_record,
    })
}

/// Drives one complete cross-Cell takeover with [`ProductionTakeoverOptions`]
/// and binds the resulting durable `DIST-TASK` records to `context.directory`
/// (`MigrationIntent` + `ReconciliationCheckpoint`) in one orchestrated call
/// (`CS1-D`).
///
/// # Errors
///
/// Returns any [`TakeoverDriveError`] from
/// [`drive_cross_cell_takeover_with_options`] or
/// [`bind_takeover_to_federation`].
pub fn drive_takeover_with_federation_binding<S: BarrierObservationSource>(
    request: CrossCellTakeoverRequest<'_, S>,
    options: ProductionTakeoverOptions,
    context: TakeoverFederationContext<'_>,
) -> Result<(CrossCellTakeoverOutcome, TakeoverFederationBindingOutcome), TakeoverDriveError> {
    let outcome = drive_cross_cell_takeover_with_options(request, options)?;
    let binding = bind_takeover_to_federation(context, &outcome)?;
    Ok((outcome, binding))
}

/// Drives one complete cross-Cell takeover with default
/// [`ProductionTakeoverOptions`]: successor lease (term advance) → fence
/// prepare → per-member signed barrier observations → completion →
/// successor-registry reopen, with fail-closed readbacks after each leg.
///
/// # Errors
///
/// Returns [`TakeoverDriveError::Store`] when any gate refuses (typed
/// lease, registry, coverage, or CAS failure — nothing is retried or
/// papered over), a drive-level precondition error before the first
/// mutation, or a post-condition error when a readback contradicts a
/// gate's contract.
#[allow(clippy::needless_pass_by_value)]
pub fn drive_cross_cell_takeover<S: BarrierObservationSource>(
    request: CrossCellTakeoverRequest<'_, S>,
) -> Result<CrossCellTakeoverOutcome, TakeoverDriveError> {
    drive_cross_cell_takeover_with_options(request, ProductionTakeoverOptions::default())
}

/// Resolves the effective `outstanding_effect_quarantine_root` from
/// [`ProductionTakeoverOptions`], cross-checking any durable
/// `QuarantineReceiptRecord` referenced by `quarantined_permit_id`.
fn resolve_effective_quarantine_root(
    store: &SqliteTaskAuthority,
    options: &ProductionTakeoverOptions,
) -> Result<Option<[u8; 32]>, TakeoverDriveError> {
    let Some(permit_id) = options.quarantined_permit_id else {
        return Ok(options
            .receipt_extensions
            .outstanding_effect_quarantine_root);
    };
    let quarantine_receipt = store
        .inspect_quarantine_receipt(permit_id)?
        .ok_or(TakeoverDriveError::QuarantineReceiptMissing(permit_id))?;
    if options
        .receipt_extensions
        .outstanding_effect_quarantine_root
        .is_some_and(|explicit_root| {
            explicit_root != quarantine_receipt.outstanding_effect_quarantine_root
        })
    {
        return Err(TakeoverDriveError::PostCondition(
            "explicit outstanding_effect_quarantine_root mismatches quarantined permit receipt",
        ));
    }
    Ok(Some(quarantine_receipt.outstanding_effect_quarantine_root))
}

/// Drives one complete cross-Cell takeover with explicit
/// [`ProductionTakeoverOptions`] (`CS1-D`):
/// successor lease (term advance) → fence prepare with `v47` receipt
/// extensions (`outstanding_effect_quarantine_root` & passive
/// `consensus_commit_index`) → optional signed [`ParticipantRegistryRecord`]
/// snapshot verification → per-member signed barrier observations (with
/// optional `[DIST-TASK-002]` tail-clause quarantine on barrier failure) →
/// completion → successor-registry reopen, with fail-closed readbacks after
/// each leg.
///
/// # Errors
///
/// Returns [`TakeoverDriveError`] on any store gate refusal, invalid
/// registry snapshot signature, missing quarantine receipt, barrier source
/// failure, coverage incompleteness, or post-condition readback mismatch.
#[allow(clippy::needless_pass_by_value, clippy::too_many_lines)] // One function keeps the CAS-chain leg order visible.
pub fn drive_cross_cell_takeover_with_options<S: BarrierObservationSource>(
    request: CrossCellTakeoverRequest<'_, S>,
    options: ProductionTakeoverOptions,
) -> Result<CrossCellTakeoverOutcome, TakeoverDriveError> {
    // Leg 0 — successor lease. The anchored judgement uses the same
    // logical reading as the wall observation: in the shared-clock ADR
    // topology that can only keep the incumbent more alive, never less.
    let lease_now_ms = next_now(request.now_ms)?;
    let wall_now_ms = u64::try_from(lease_now_ms).map_err(|_| TakeoverDriveError::NegativeClock)?;
    let successor_lease = match request.store.acquire_authority_lease_anchored(
        AuthorityLeaseRequest {
            holder_id: request.successor_holder_id,
            idempotency_key: request.lease_idempotency_key,
            requested_at_ms: lease_now_ms,
            ttl_ms: request.lease_ttl_ms,
        },
        wall_now_ms,
    )? {
        AuthorityLeaseDecision::TakenOver(record) | AuthorityLeaseDecision::Replayed(record) => {
            record
        }
        AuthorityLeaseDecision::Acquired(_) => {
            return Err(TakeoverDriveError::NoIncumbentAuthorityLease);
        }
        AuthorityLeaseDecision::Renewed(_) => {
            return Err(TakeoverDriveError::SuccessorIsIncumbentHolder);
        }
    };

    // The CAS chain's subject: the incumbent term's active assignment.
    let old_assignment = match request.store.inspect_authority_assignment(request.task_id) {
        Ok(record) => record,
        Err(TaskStoreError::ReceiptNotFound) => {
            return Err(TakeoverDriveError::NoActiveAssignment);
        }
        Err(error) => return Err(error.into()),
    };
    if old_assignment.state != AuthorityAssignmentState::Active {
        return Err(TakeoverDriveError::OldAssignmentNotActive {
            state: old_assignment.state,
        });
    }

    let effective_quarantine_root = resolve_effective_quarantine_root(request.store, &options)?;
    let effective_consensus_commit_index = options.receipt_extensions.consensus_commit_index;

    // Leg 1 — fence prepare: registry → `FrozenForTakeover`, exact roots,
    // manifest, old assignment → `TakeoverPending`, pending receipt with
    // optional v47 receipt extensions.
    let fence_now_ms = next_now(request.now_ms)?;
    let mut frozen_registry = request
        .store
        .prepare_authority_takeover_fence_with_receipt_extensions(
            AuthorityLeaseTakeoverFenceRequest {
                task_id: request.task_id,
                expected_registry_binding: old_assignment.participant_registry_binding,
                lease: successor_lease,
                requested_at_ms: fence_now_ms,
            },
            effective_quarantine_root,
            effective_consensus_commit_index,
        )?;
    if frozen_registry.state != ParticipantRegistryState::FrozenForTakeover {
        return Err(TakeoverDriveError::PostCondition(
            "registry is not frozen for takeover after the fence gate",
        ));
    }
    let frozen_binding = ParticipantRegistryBinding {
        generation: frozen_registry.generation,
        root: frozen_registry.root,
    };
    if frozen_binding != old_assignment.participant_registry_binding {
        return Err(TakeoverDriveError::PostCondition(
            "frozen registry binding drifted from the old assignment binding",
        ));
    }

    // Optional CS1-D / v47 frozen registry snapshot signature verification &
    // persistence.
    let verified_registry_signer = if let Some(signer) = options.frozen_registry_signer {
        let (signed_registry, proof) = record_verified_participant_registry_signature(
            request.store,
            request.identity,
            request.task_id,
            frozen_binding,
            signer,
            fence_now_ms,
        )?;
        frozen_registry = signed_registry;
        Some(proof)
    } else {
        None
    };

    let fence_receipt = request
        .store
        .inspect_authority_takeover_fence_receipt(request.task_id, frozen_binding)?;
    let exact_fence_set_root = fence_receipt
        .exact_fence_set_root
        .ok_or(TakeoverDriveError::FenceManifestUnavailable)?;
    let takeover_receipt_pending = request
        .store
        .inspect_authority_takeover_receipt(request.task_id, fence_receipt.receipt_id)?;
    if takeover_receipt_pending.barrier_state != AuthorityTakeoverReceiptState::Pending
        || takeover_receipt_pending.new_assignment_id.is_some()
    {
        return Err(TakeoverDriveError::TakeoverAlreadyComplete);
    }
    if takeover_receipt_pending.outstanding_effect_quarantine_root != effective_quarantine_root
        || takeover_receipt_pending.consensus_commit_index != effective_consensus_commit_index
    {
        return Err(TakeoverDriveError::PostCondition(
            "pending takeover receipt extensions drifted from requested v47 extensions",
        ));
    }
    let fence_members = request
        .store
        .inspect_authority_takeover_fence_members(request.task_id, frozen_binding)?;

    // Leg 2 — one signed barrier observation per exact-fence member. The
    // endpoint hosts sign; the store's gate verifies through the identity
    // authority before anything becomes durable. If `quarantine_on_barrier_failure`
    // is enabled and any observation or coverage gate fails, the incumbent
    // `TakeoverPending` assignment is quarantined before returning the error
    // (`[DIST-TASK-002]` tail clause).
    let mut observations = Vec::with_capacity(fence_members.len());
    for member in &fence_members {
        let observed_at_ms = next_now(request.now_ms)?;
        let observation = match request
            .barrier_source
            .observe_barrier(BarrierObservationDemand {
                takeover_receipt_id: takeover_receipt_pending.receipt_id,
                participant: member.participant,
                fence_set_root: exact_fence_set_root,
            }) {
            Ok(observation) => observation,
            Err(error) => {
                if options.quarantine_on_barrier_failure {
                    let _ = request.store.quarantine_authority_assignment(
                        request.task_id,
                        old_assignment.assignment_id,
                        observed_at_ms,
                    );
                }
                return Err(error);
            }
        };
        let record = match request
            .store
            .record_authority_takeover_barrier_receipt_signed(
                request.identity,
                AuthorityTakeoverBarrierReceiptRequest {
                    takeover_receipt_id: takeover_receipt_pending.receipt_id,
                    participant: member.participant,
                    remote_receipt_id: observation.remote_receipt_id,
                    barrier_digest: observation.barrier_digest,
                    observed_at_ms,
                },
                observation.signature,
            ) {
            Ok(record) => record,
            Err(error) => {
                if options.quarantine_on_barrier_failure {
                    let _ = request.store.quarantine_authority_assignment(
                        request.task_id,
                        old_assignment.assignment_id,
                        observed_at_ms,
                    );
                }
                return Err(error.into());
            }
        };
        if record.signer.is_none() {
            return Err(TakeoverDriveError::PostCondition(
                "signed barrier observation stored without a signer proof",
            ));
        }
        observations.push(record);
    }

    // Pre-completion coverage gate: every manifest member observed at the
    // same exact root — the read-only view the store recomputes.
    let coverage = request
        .store
        .inspect_authority_takeover_barrier_coverage(takeover_receipt_pending.receipt_id)?;
    if coverage.state != AuthorityTakeoverBarrierCoverageState::LocallyCovered {
        if options.quarantine_on_barrier_failure
            && let Ok(quarantine_now_ms) = next_now(request.now_ms)
        {
            let _ = request.store.quarantine_authority_assignment(
                request.task_id,
                old_assignment.assignment_id,
                quarantine_now_ms,
            );
        }
        return Err(TakeoverDriveError::BarrierCoverageIncomplete {
            state: coverage.state,
            missing: coverage.missing_participants.len(),
        });
    }

    // Leg 3 — completion: receipt `Pending → Complete`, old assignment
    // fenced, successor assignment activated on the frozen binding.
    let completed_at_ms = next_now(request.now_ms)?;
    let completion =
        request
            .store
            .complete_authority_takeover(CompleteAuthorityTakeoverRequest {
                takeover_receipt_id: takeover_receipt_pending.receipt_id,
                lease: successor_lease,
                completed_at_ms,
            })?;
    if completion.barrier_state != AuthorityTakeoverReceiptState::Complete {
        return Err(TakeoverDriveError::PostCondition(
            "completion gate returned without completing the receipt",
        ));
    }
    let takeover_receipt_completed = request
        .store
        .inspect_authority_takeover_receipt(request.task_id, fence_receipt.receipt_id)?;
    if takeover_receipt_completed.barrier_state != AuthorityTakeoverReceiptState::Complete
        || takeover_receipt_completed.new_assignment_id != Some(completion.new_assignment_id)
        || takeover_receipt_completed.outstanding_effect_quarantine_root
            != effective_quarantine_root
        || takeover_receipt_completed.consensus_commit_index != effective_consensus_commit_index
    {
        return Err(TakeoverDriveError::PostCondition(
            "completed takeover receipt readback drifted from completion and v47 extensions",
        ));
    }

    // Leg 4 — successor registry reopen (`DIST-TASK-004`): a new `Open`
    // generation with a fresh root; the frozen old root is never unfrozen
    // in place.
    let reopened_at_ms = next_now(request.now_ms)?;
    let reopen =
        request
            .store
            .reopen_successor_registry(AuthoritySuccessorRegistryReopenRequest {
                takeover_receipt_id: takeover_receipt_pending.receipt_id,
                lease: successor_lease,
                reopened_at_ms,
            })?;
    if reopen.successor_registry_binding.generation <= frozen_binding.generation
        || reopen.successor_registry_binding.root == frozen_binding.root
    {
        return Err(TakeoverDriveError::PostCondition(
            "successor registry reopen did not advance to a new generation and root",
        ));
    }

    // Final readbacks — the durable state the evidence asserts against.
    let active_assignment = request
        .store
        .inspect_authority_assignment(request.task_id)?;
    if active_assignment.state != AuthorityAssignmentState::Active
        || active_assignment.assignment_id != reopen.active_assignment_id
        || active_assignment.participant_registry_binding != reopen.successor_registry_binding
        || active_assignment.authority_lease_binding != successor_lease.binding()
    {
        return Err(TakeoverDriveError::PostCondition(
            "active assignment does not bind the reopened successor registry and lease",
        ));
    }
    let reopened_registry = request
        .store
        .inspect_participant_registry(request.task_id)?;
    if reopened_registry.state != ParticipantRegistryState::Open
        || reopened_registry.generation != reopen.successor_registry_binding.generation
        || reopened_registry.root != reopen.successor_registry_binding.root
        || reopened_registry.prior_root != frozen_binding.root
        || reopened_registry.prior_participant_registry_root != Some(frozen_binding.root)
    {
        return Err(TakeoverDriveError::PostCondition(
            "registry is not the reopened successor generation chained to the frozen root",
        ));
    }

    Ok(CrossCellTakeoverOutcome {
        successor_lease,
        old_assignment,
        frozen_registry,
        verified_registry_signer,
        fence_receipt,
        fence_members,
        takeover_receipt_pending,
        observations,
        completion,
        takeover_receipt_completed,
        reopen,
        active_assignment,
        reopened_registry,
    })
}
