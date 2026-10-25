//! Partition reconnection bounded-loss lease settlement, cross-Cell semantic
//! event convergence, and artifact replica convergence coordinator (`W65` /
//! `C-PARTITION`).
//!
//! Composes the landed authorities without inventing duplicate authority state:
//! 1. **Epoch Advance & Quota Freeze (`[LEASE-LOSS-001]`, `[LEASE-FENCE-001]`)**:
//!    advances each partitioned Cell's `CellEpoch` and `CellFencingToken`,
//!    moving unsettled `QuotaLease`s into `QUARANTINED` without refunding
//!    unverified balances to `AVAILABLE`, fencing/quarantining unsettled
//!    `CapacityLease`s and `ExclusiveDeviceLease`s, invalidating pre-partition
//!    detector suspicions and capability name-cache entries, and transitioning
//!    `PartitionAutonomyGate` from `Partitioned` to `Reconnecting`.
//! 2. **Bounded-Loss Lease Reconciliation (`[LEASE-RECON-001]`, `[ROAD-C-001]`)**:
//!    settles quarantined leases via canonical `QuotaReconciliationReceipt`,
//!    `CapacityReconciliationReceipt`, and `DeviceReconciliationReceipt` records
//!    backed by verified `FenceBarrierAck` digests, computing a machine-verifiable
//!    [`PartitionBoundedLossProof`] proving exact pool conservation and the
//!    explicit upper bound `permanently_quarantined_loss <= max_unreported_inflight_window`.
//! 3. **Cross-Cell Semantic Event Convergence (`[DIST-EVENT-001]`,
//!    `[DIST-AUDIT-001]`, `[SEM-CHECKPOINT-001]`)**: exchanges public-key
//!    identity descriptors (never private keys), verifies
//!    `SignedSemanticVectorCheckpoint`s, computes `diff_vector_checkpoints`,
//!    exports and atomically converges `CrossDomainEventItem` sets on both
//!    Cells, and verifies post-convergence checkpoint equality (plus optional
//!    idempotent replay verification).
//! 4. **Cross-Cell Artifact Replica Convergence (`[DIST-ART-001]`)**: exports
//!    and imports `ArtifactTransferPackage`s under an explicit
//!    `ArtifactSyncPolicyRegistry`, surfacing identical canonical
//!    `ArtifactConflictReceipt` digests on divergent symmetric-peer edits while
//!    fast-forwarding non-conflicting replicas, completing reconnection to
//!    `Connected`, and sealing a federated `ReconciliationCheckpoint` verified
//!    via `CheckpointFact::verify_digest`.

use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::num::NonZeroU64;

use nlos_artifact::{
    ArtifactBlobPayload, ArtifactConflictReceipt, ArtifactSyncError, ArtifactSyncPolicy,
    ArtifactSyncPolicyError, ArtifactSyncPolicyRegistry, ArtifactTombstoneMetadata,
    ArtifactTransferPackage, ContentDigest, CrossStoreArtifactSyncReport,
    ExportArtifactTransferRequest, HeadState, ImportArtifactTransferRequest, ReplicaRevisionBundle,
    TransferRevisionEntry, export_artifact_transfer_package, import_artifact_transfer_package,
};
use nlos_capability::CapabilityTarget;
use nlos_cell::{
    CellEpoch, CellFence, CellFencingToken, CellIdentity, CheckpointDigestVerification,
    CheckpointPrefix, ControlPlaneLinkState, PartitionViolation,
};
use nlos_identity::BootstrapPrincipalRequest;
use nlos_lease::{
    CapacityLeaseState, CapacityReconcileError, CapacityReconciliationReceipt, CleanupAttestation,
    DeviceReconcileError, DeviceReconciliationReceipt, DeviceResetReceipt,
    ExclusiveDeviceLeaseState, FenceBarrierAck, QuotaLeaseState, QuotaReconcileError,
    QuotaReconciliationReceipt,
};
use nlos_semantic::{
    AdmissionDurability, AdmissionReceipt, CrossDomainEventItem, DomainSemanticWaterMark,
    SemanticAuthorityError, SemanticEventRecord, SemanticPayloadIdentity, SemanticVectorCheckpoint,
    SemanticVectorCheckpointError, SignedSemanticVectorCheckpoint, TaintFlags,
    VectorCheckpointDiff, VectorCheckpointRelation, diff_vector_checkpoints,
    verify_signed_vector_checkpoint,
};
use nlos_types::{
    ApplicationId, ArtifactId, CapacityLeaseId, CommitPermitId, ControlDomainId, DeviceId,
    ExclusiveDeviceLeaseId, Generation, IdempotencyKey, KeyId, NamespaceId, PrincipalId,
    QuotaLeaseId, ReceiptId, SemanticEventId, TaskId,
};
use sha2::{Digest, Sha256};

use crate::cell_host::CellHost;
use crate::error::SliceKError;

/// Domain separator for [`PartitionBoundedLossProof`] digests.
pub const PARTITION_BOUNDED_LOSS_PROOF_DOMAIN: &[u8] = b"llmos/partition-bounded-loss-proof/v1";

/// Domain separator for [`PartitionRecoveryReceipt`] digests.
pub const PARTITION_RECOVERY_RECEIPT_DOMAIN: &[u8] = b"llmos/partition-recovery-receipt/v1";

/// 8-byte entry magic for partition recovery entries in [`CheckpointPrefix`].
pub const PARTITION_RECOVERY_CHECKPOINT_ENTRY_MAGIC: &[u8; 8] = b"PARTRECV";

/// Wire magic for [`SignedSemanticVectorCheckpoint`] transport frames.
const VECTOR_CKPT_WIRE_MAGIC: &[u8; 8] = b"SVCKPTV1";

/// Wire magic for [`CrossDomainEventItem`] batch transport frames.
const XDOM_EVENTS_WIRE_MAGIC: &[u8; 8] = b"XDOMEVV1";

/// Wire magic for [`ArtifactTransferPackage`] transport frames.
const ART_PKG_WIRE_MAGIC: &[u8; 8] = b"ARTPKGV1";

/// Fail-closed errors emitted during [`drive_partition_reconnection_convergence`].
///
/// Large inner errors are boxed so `Result<_, PartitionRecoveryError>` stays
/// compact (`< 128` bytes).
#[derive(Debug)]
pub enum PartitionRecoveryError {
    /// Endpoint is not in `Partitioned` state at the start of recovery, or
    /// failed a [`PartitionAutonomyGate`](nlos_cell::PartitionAutonomyGate)
    /// state transition.
    PartitionState(PartitionViolation),
    /// Wrapped [`CellHost`] / [`SliceKError`] failure.
    SliceK(Box<SliceKError>),
    /// `QuotaLease` reconciliation failure.
    QuotaReconcile(QuotaReconcileError),
    /// `CapacityLease` reconciliation failure.
    CapacityReconcile(CapacityReconcileError),
    /// `ExclusiveDeviceLease` reconciliation failure.
    DeviceReconcile(DeviceReconcileError),
    /// Bounded-loss or pool conservation invariant check failed.
    BoundedLossViolation(&'static str),
    /// Cross-Cell semantic checkpoint or event-set convergence failure.
    Semantic(Box<SemanticAuthorityError>),
    /// Signed semantic vector checkpoint failed cryptographic verification.
    VectorCheckpointVerify(Box<SemanticVectorCheckpointError>),
    /// Post-convergence semantic vector checkpoints failed to reach equality.
    SemanticConvergenceIncomplete {
        /// Relation observed on Cell A vs Cell B after convergence.
        relation: VectorCheckpointRelation,
    },
    /// Artifact sync policy registration failure.
    ArtifactPolicy(Box<ArtifactSyncPolicyError>),
    /// Cross-store artifact transfer export/import failure.
    ArtifactSync(Box<ArtifactSyncError>),
    /// Symmetric-peer divergent conflict digests disagreed across the two Cells.
    AsymmetricArtifactConflictDigest {
        /// Artifact whose conflict digests disagreed.
        artifact_id: ArtifactId,
        /// Digest computed on Cell A.
        digest_a: [u8; 32],
        /// Digest computed on Cell B.
        digest_b: [u8; 32],
    },
    /// Federated `ReconciliationCheckpoint` failed `CheckpointFact::verify_digest`.
    CheckpointVerificationFailed(&'static str),
    /// Cross-process endpoint IPC or wire framing error.
    EndpointTransport(String),
}

impl From<PartitionViolation> for PartitionRecoveryError {
    fn from(error: PartitionViolation) -> Self {
        Self::PartitionState(error)
    }
}

impl From<SliceKError> for PartitionRecoveryError {
    fn from(error: SliceKError) -> Self {
        Self::SliceK(Box::new(error))
    }
}

impl From<QuotaReconcileError> for PartitionRecoveryError {
    fn from(error: QuotaReconcileError) -> Self {
        Self::QuotaReconcile(error)
    }
}

impl From<CapacityReconcileError> for PartitionRecoveryError {
    fn from(error: CapacityReconcileError) -> Self {
        Self::CapacityReconcile(error)
    }
}

impl From<DeviceReconcileError> for PartitionRecoveryError {
    fn from(error: DeviceReconcileError) -> Self {
        Self::DeviceReconcile(error)
    }
}

impl From<SemanticAuthorityError> for PartitionRecoveryError {
    fn from(error: SemanticAuthorityError) -> Self {
        Self::Semantic(Box::new(error))
    }
}

impl From<SemanticVectorCheckpointError> for PartitionRecoveryError {
    fn from(error: SemanticVectorCheckpointError) -> Self {
        Self::VectorCheckpointVerify(Box::new(error))
    }
}

impl From<ArtifactSyncPolicyError> for PartitionRecoveryError {
    fn from(error: ArtifactSyncPolicyError) -> Self {
        Self::ArtifactPolicy(Box::new(error))
    }
}

impl From<ArtifactSyncError> for PartitionRecoveryError {
    fn from(error: ArtifactSyncError) -> Self {
        Self::ArtifactSync(Box::new(error))
    }
}

impl fmt::Display for PartitionRecoveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PartitionState(err) => write!(formatter, "partition state refusal: {err}"),
            Self::SliceK(err) => write!(formatter, "CellHost/SliceK refusal: {err}"),
            Self::QuotaReconcile(err) => write!(formatter, "QuotaLease reconcile refusal: {err}"),
            Self::CapacityReconcile(err) => {
                write!(formatter, "CapacityLease reconcile refusal: {err}")
            }
            Self::DeviceReconcile(err) => {
                write!(formatter, "ExclusiveDeviceLease reconcile refusal: {err}")
            }
            Self::BoundedLossViolation(reason) => {
                write!(formatter, "bounded-loss conservation violation: {reason}")
            }
            Self::Semantic(err) => write!(formatter, "semantic convergence refusal: {err}"),
            Self::VectorCheckpointVerify(err) => {
                write!(formatter, "vector checkpoint verification refusal: {err}")
            }
            Self::SemanticConvergenceIncomplete { relation } => write!(
                formatter,
                "post-convergence semantic vector checkpoints not Equal: {relation:?}"
            ),
            Self::ArtifactPolicy(err) => write!(formatter, "artifact sync policy refusal: {err}"),
            Self::ArtifactSync(err) => write!(formatter, "artifact replica sync refusal: {err}"),
            Self::AsymmetricArtifactConflictDigest {
                artifact_id,
                digest_a,
                digest_b,
            } => write!(
                formatter,
                "asymmetric artifact conflict digest for {artifact_id:?}: {digest_a:?} != {digest_b:?}"
            ),
            Self::CheckpointVerificationFailed(reason) => {
                write!(
                    formatter,
                    "federation checkpoint verification failed: {reason}"
                )
            }
            Self::EndpointTransport(reason) => {
                write!(formatter, "partition endpoint transport failure: {reason}")
            }
        }
    }
}

impl Error for PartitionRecoveryError {}

/// Settlement specification for one quarantined `QuotaLease` upon partition
/// heal (`[LEASE-LOSS-001]`, `[LEASE-RECON-001]`, `[ROAD-C-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QuotaPartitionSettlementSpec {
    /// Quarantined `QuotaLeaseId`.
    pub lease_id: QuotaLeaseId,
    /// Idempotent reconciliation receipt identifier.
    pub receipt_id: ReceiptId,
    /// Strictly increasing per-lease reconciliation sequence (`>= 1`).
    pub reconcile_seq: u64,
    /// Enforcement gateway identifier bound into [`FenceBarrierAck`].
    pub gateway_id: [u8; 16],
    /// Attested high-water spend confirmed by the enforcement gateway.
    pub attested_spent: u64,
    /// Unattested in-flight window that CANNOT be proven unspent and therefore
    /// MUST remain permanently quarantined (`0` when full attestation exists).
    pub unattested_inflight_loss: u64,
    /// Provable upper bound on unreported local spend (`[ROAD-C-001]`);
    /// recovery fails closed if `unattested_inflight_loss > max_unreported_inflight_window`.
    pub max_unreported_inflight_window: u64,
}

/// Settlement specification for one `CapacityLease` upon partition heal
/// (`[LEASE-CAPACITY-002]`, `[LEASE-RECON-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapacityPartitionSettlementSpec {
    /// `CapacityLeaseId` to quarantine and reconcile.
    pub capacity_lease_id: CapacityLeaseId,
    /// Reconciliation receipt identifier.
    pub receipt_id: ReceiptId,
    /// Enforcement gateway identifier bound into [`FenceBarrierAck`].
    pub gateway_id: [u8; 16],
    /// Verified holder [`CleanupAttestation`].
    pub cleanup_attestation: CleanupAttestation,
}

/// Settlement specification for one `ExclusiveDeviceLease` upon partition heal
/// (`[LEASE-DEVICE-001]`, `[LEASE-RECON-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DevicePartitionSettlementSpec {
    /// `ExclusiveDeviceLeaseId` to quarantine and reconcile.
    pub device_lease_id: ExclusiveDeviceLeaseId,
    /// Reconciliation receipt identifier.
    pub receipt_id: ReceiptId,
    /// Enforcement gateway identifier bound into [`FenceBarrierAck`].
    pub gateway_id: [u8; 16],
    /// Hardware reset + zeroization receipt.
    pub reset_receipt: DeviceResetReceipt,
}

/// All lease settlements requested on one Cell during partition recovery.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CellLeaseReconciliationSpec {
    /// Quarantined `QuotaLease`s to reconcile with bounded-loss verification.
    pub quota_leases: Vec<QuotaPartitionSettlementSpec>,
    /// Optional `CapacityLease`s to quarantine and reconcile.
    pub capacity_leases: Vec<CapacityPartitionSettlementSpec>,
    /// Optional `ExclusiveDeviceLease`s to quarantine and reconcile.
    pub device_leases: Vec<DevicePartitionSettlementSpec>,
}

/// Cross-Cell semantic convergence configuration (`[DIST-EVENT-001]`,
/// `[SEM-CHECKPOINT-001]`).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SemanticPartitionConvergenceSpec {
    /// Additional public-key principal descriptors to register in Cell A's
    /// `IdentityAuthority` before verifying Cell B's events (Cell B's
    /// `SemanticWriter` public key is registered automatically).
    pub additional_principals_for_a: Vec<BootstrapPrincipalRequest>,
    /// Additional public-key principal descriptors to register in Cell B's
    /// `IdentityAuthority` before verifying Cell A's events (Cell A's
    /// `SemanticWriter` public key is registered automatically).
    pub additional_principals_for_b: Vec<BootstrapPrincipalRequest>,
    /// When `true`, immediately replays the exported remote event sets a second
    /// time on both Cells and verifies zero duplicate rows are imported.
    pub verify_idempotent_second_replay: bool,
}

/// Commit metadata bound into [`ImportArtifactTransferRequest`] on one Cell.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ArtifactImportCommitContext {
    /// Local convergence `TaskId`.
    pub task_id: TaskId,
    /// Local convergence `CommitPermitId`.
    pub permit_id: CommitPermitId,
    /// Write-set root digest bound to staged revisions.
    pub write_set_root: ContentDigest,
    /// Base idempotency key from which per-artifact import keys are derived.
    pub base_idempotency_key: IdempotencyKey,
}

impl Default for ArtifactImportCommitContext {
    fn default() -> Self {
        Self {
            task_id: TaskId::from_bytes([0x91; 16]),
            permit_id: CommitPermitId::from_bytes([0x92; 16]),
            write_set_root: ContentDigest::from_bytes([0x93; 32]),
            base_idempotency_key: IdempotencyKey::from_bytes([0x94; 16]),
        }
    }
}

/// Cross-Cell artifact replica convergence specification (`[DIST-ART-001]`).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ArtifactPartitionConvergenceSpec {
    /// Declared per-`content_type` synchronization policies.
    pub policies: Vec<ArtifactSyncPolicy>,
    /// Artifacts on Cell A whose heads/chains are exported and synced into Cell B.
    pub cell_a_exports: Vec<ArtifactId>,
    /// Artifacts on Cell B whose heads/chains are exported and synced into Cell A.
    pub cell_b_exports: Vec<ArtifactId>,
    /// Commit metadata used when importing packages into Cell A.
    pub cell_a_import_context: ArtifactImportCommitContext,
    /// Commit metadata used when importing packages into Cell B.
    pub cell_b_import_context: ArtifactImportCommitContext,
}

/// Complete request driving [`drive_partition_reconnection_convergence`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PartitionRecoveryRequest {
    /// Correlation identifier (`16` bytes) for this recovery session.
    pub recovery_id: [u8; 16],
    /// Monotonic logical tick for `begin_control_plane_reconnection`.
    pub reconnection_tick: u64,
    /// Monotonic logical tick for `complete_control_plane_reconnection`
    /// (`>= reconnection_tick`).
    pub completion_tick: u64,
    /// Authoritative timestamp (ms) for checkpoint signing and artifact sync.
    pub synced_at_ms: u64,
    /// Non-zero trust snapshot digest committed into Cell A's vector checkpoint.
    pub trust_snapshot_digest_a: [u8; 32],
    /// Non-zero trust snapshot digest committed into Cell B's vector checkpoint.
    pub trust_snapshot_digest_b: [u8; 32],
    /// Lease settlements for Cell A.
    pub cell_a_leases: CellLeaseReconciliationSpec,
    /// Lease settlements for Cell B.
    pub cell_b_leases: CellLeaseReconciliationSpec,
    /// Semantic event convergence configuration.
    pub semantic_convergence: SemanticPartitionConvergenceSpec,
    /// Artifact replica convergence configuration.
    pub artifact_convergence: ArtifactPartitionConvergenceSpec,
    /// When `true`, refreshes `CellDirectory` registrations and appends a
    /// verified structured `ReconciliationCheckpoint` on federated Cells.
    pub record_federation_checkpoints: bool,
}

/// Per-`QuotaLease` settlement audit item inside [`PartitionBoundedLossProof`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QuotaLeaseReconciliationSummary {
    /// Reconciled `QuotaLeaseId`.
    pub lease_id: QuotaLeaseId,
    /// Committed `QuotaReconciliationReceipt` id.
    pub receipt_id: ReceiptId,
    /// Pre-heal partition epoch under which the lease was granted.
    pub prior_epoch: CellEpoch,
    /// Total prepaid face value granted on the lease.
    pub face_value: u64,
    /// Attested spend settled onto the lease.
    pub attested_spent: u64,
    /// Proven unspent balance returned to `AVAILABLE`.
    pub refunded_to_available: u64,
    /// Unattested in-flight balance permanently held in `QUARANTINED` (never
    /// returned to `AVAILABLE`).
    pub permanently_quarantined_loss: u64,
    /// Declared upper bound on unreported local spend (`[ROAD-C-001]`).
    pub max_unreported_inflight_window: u64,
    /// Resulting `QuotaLeaseState` (`Closed` when `permanently_quarantined_loss == 0`,
    /// `Quarantined` when `> 0`).
    pub post_reconcile_state: QuotaLeaseState,
    /// Verified `FenceBarrierAck` digest (`32` bytes).
    pub fence_barrier_ack_digest: [u8; 32],
}

/// Transport-constructible summary of an epoch advance on one partitioned Cell.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PartitionEpochAdvanceSnapshot {
    /// Post-advance [`CellFence`].
    pub fence: CellFence,
    /// Count of cached capability-name entries fenced at the epoch boundary.
    pub entries_fenced: usize,
}

/// Transport-constructible summary of a reconciled `CapacityLease`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PartitionSettledCapacitySummary {
    /// Reconciled `CapacityLeaseId`.
    pub capacity_lease_id: CapacityLeaseId,
    /// Capacity amount returned to the pool.
    pub amount: u64,
    /// Post-reconciliation state (`Returned`).
    pub final_state: CapacityLeaseState,
    /// Verified `FenceBarrierAck` digest (`32` bytes).
    pub fence_ack_digest: [u8; 32],
}

/// Transport-constructible summary of a reconciled `ExclusiveDeviceLease`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PartitionSettledDeviceSummary {
    /// Reconciled `ExclusiveDeviceLeaseId`.
    pub device_lease_id: ExclusiveDeviceLeaseId,
    /// Device identity released back to `FREE`.
    pub device_id: DeviceId,
    /// Pre-heal exclusivity epoch fenced by the reset/reconcile barrier.
    pub exclusivity_epoch: CellEpoch,
    /// Resulting hardware reset generation.
    pub reset_generation: Generation,
    /// Post-reconciliation state (`Returned`).
    pub final_state: ExclusiveDeviceLeaseState,
    /// Verified `FenceBarrierAck` digest (`32` bytes).
    pub fence_ack_digest: [u8; 32],
}

/// Machine-verifiable proof of exact conservation and bounded quota loss for
/// one Cell across a partition heal (`[LEASE-LOSS-001]`, `[LEASE-RECON-001]`,
/// `[ROAD-C-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PartitionBoundedLossProof {
    /// Cell identity.
    pub cell_id: CellIdentity,
    /// Frozen partition fence prior to epoch advance.
    pub prior_frozen_fence: CellFence,
    /// Reconnected fence after epoch advance.
    pub reconnected_fence: CellFence,
    /// `AVAILABLE` quota pool immediately after epoch freeze and before
    /// reconciliation.
    pub pool_available_before_reconcile: u64,
    /// `AVAILABLE` quota pool after reconciling all specified `QuotaLease`s.
    pub pool_available_after_reconcile: u64,
    /// Sum of `face_value` across all reconciled `QuotaLease`s.
    pub total_granted_face_value: u64,
    /// Sum of `attested_spent` across all reconciled `QuotaLease`s.
    pub reconciled_spent: u64,
    /// Sum of `refunded_to_available` across all reconciled `QuotaLease`s.
    pub refunded_available: u64,
    /// Sum of `permanently_quarantined_loss` across all reconciled `QuotaLease`s.
    pub permanently_quarantined_loss: u64,
    /// Sum of `max_unreported_inflight_window` across all reconciled `QuotaLease`s.
    pub max_unreported_inflight_window: u64,
    /// Per-lease reconciliation summaries.
    pub quota_summaries: Vec<QuotaLeaseReconciliationSummary>,
    /// Reconciled `CapacityLease` snapshots (`RETURNED`).
    pub reconciled_capacity_leases: Vec<PartitionSettledCapacitySummary>,
    /// Reconciled `ExclusiveDeviceLease` snapshots (`RETURNED`).
    pub reconciled_device_leases: Vec<PartitionSettledDeviceSummary>,
    /// Domain-separated `SHA-256` digest over all proof fields.
    pub proof_digest: [u8; 32],
}

impl PartitionBoundedLossProof {
    /// Computes the canonical domain-separated `SHA-256` digest of this proof.
    #[must_use]
    pub fn compute_digest(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(PARTITION_BOUNDED_LOSS_PROOF_DOMAIN);
        hash_fence(&mut hasher, self.prior_frozen_fence);
        hash_fence(&mut hasher, self.reconnected_fence);
        hasher.update(self.pool_available_before_reconcile.to_be_bytes());
        hasher.update(self.pool_available_after_reconcile.to_be_bytes());
        hasher.update(self.total_granted_face_value.to_be_bytes());
        hasher.update(self.reconciled_spent.to_be_bytes());
        hasher.update(self.refunded_available.to_be_bytes());
        hasher.update(self.permanently_quarantined_loss.to_be_bytes());
        hasher.update(self.max_unreported_inflight_window.to_be_bytes());
        hash_len(&mut hasher, self.quota_summaries.len());
        for item in &self.quota_summaries {
            hasher.update(item.lease_id.as_bytes());
            hasher.update(item.receipt_id.as_bytes());
            hasher.update(item.prior_epoch.get().to_be_bytes());
            hasher.update(item.face_value.to_be_bytes());
            hasher.update(item.attested_spent.to_be_bytes());
            hasher.update(item.refunded_to_available.to_be_bytes());
            hasher.update(item.permanently_quarantined_loss.to_be_bytes());
            hasher.update(item.max_unreported_inflight_window.to_be_bytes());
            hasher.update(item.fence_barrier_ack_digest);
        }
        hash_len(&mut hasher, self.reconciled_capacity_leases.len());
        for cap in &self.reconciled_capacity_leases {
            hasher.update(cap.capacity_lease_id.as_bytes());
            hasher.update(cap.amount.to_be_bytes());
            hasher.update(cap.fence_ack_digest);
        }
        hash_len(&mut hasher, self.reconciled_device_leases.len());
        for dev in &self.reconciled_device_leases {
            hasher.update(dev.device_lease_id.as_bytes());
            hasher.update(dev.reset_generation.get().to_be_bytes());
            hasher.update(dev.fence_ack_digest);
        }
        hasher.finalize().into()
    }

    /// Machine-checks epoch advancement, exact conservation, zero oversell,
    /// bounded loss (`permanently_quarantined_loss <= max_unreported_inflight_window`),
    /// and `proof_digest` integrity.
    #[must_use]
    pub fn verify_invariants(&self) -> bool {
        if self.prior_frozen_fence.identity() != self.cell_id
            || self.reconnected_fence.identity() != self.cell_id
        {
            return false;
        }
        if self.reconnected_fence.epoch() <= self.prior_frozen_fence.epoch()
            || self.reconnected_fence.fencing_token() <= self.prior_frozen_fence.fencing_token()
        {
            return false;
        }
        let Some(total_accounted) = self
            .reconciled_spent
            .checked_add(self.refunded_available)
            .and_then(|sum| sum.checked_add(self.permanently_quarantined_loss))
        else {
            return false;
        };
        if total_accounted != self.total_granted_face_value {
            return false;
        }
        if self.permanently_quarantined_loss > self.max_unreported_inflight_window {
            return false;
        }
        if self.permanently_quarantined_loss > self.total_granted_face_value - self.reconciled_spent
        {
            return false;
        }
        for item in &self.quota_summaries {
            let Some(item_sum) = item
                .attested_spent
                .checked_add(item.refunded_to_available)
                .and_then(|s| s.checked_add(item.permanently_quarantined_loss))
            else {
                return false;
            };
            if item_sum != item.face_value {
                return false;
            }
            if item.permanently_quarantined_loss > item.max_unreported_inflight_window {
                return false;
            }
            let expected_state = if item.permanently_quarantined_loss == 0 {
                QuotaLeaseState::Closed
            } else {
                QuotaLeaseState::Quarantined
            };
            if item.post_reconcile_state != expected_state {
                return false;
            }
        }
        self.compute_digest() == self.proof_digest
    }
}

/// Transport-constructible summary of one atomic cross-domain semantic
/// convergence pass on a single Cell.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticConvergenceStepSnapshot {
    /// Newly imported `SemanticEventId`s in deterministic topological order.
    pub imported_event_ids: Vec<SemanticEventId>,
    /// Already-present `SemanticEventId`s idempotently skipped.
    pub skipped_existing_event_ids: Vec<SemanticEventId>,
    /// Count of `[DIST-AUDIT-001]` cross-domain audit records preserved.
    pub audit_record_count: u64,
    /// Post-convergence unsigned [`SemanticVectorCheckpoint`] of this store.
    pub vector_checkpoint: SemanticVectorCheckpoint,
}

/// Transport-constructible summary of one [`ArtifactTransferPackage`] import on
/// a single Cell (`[DIST-ART-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PartitionArtifactImportSummary {
    /// Verified missing blobs and contiguous revisions materialized, advancing
    /// the canonical head.
    ImportedFastForward {
        /// Imported `ArtifactId`.
        artifact_id: ArtifactId,
        /// Prior local head before import.
        prior_local_head: Option<HeadState>,
        /// New canonical head after import.
        new_head: HeadState,
        /// Newly imported revision numbers.
        imported_revisions: Vec<u64>,
    },
    /// Target store already had the identical `(revision, digest)` at head.
    NoOpConverged {
        /// Converged `ArtifactId`.
        artifact_id: ArtifactId,
        /// Unchanged canonical head.
        head: HeadState,
    },
    /// Concurrent divergent edit detected: local head stayed unchanged,
    /// competing blobs were preserved on disk, and both heads were pinned in
    /// [`nlos_artifact::ReplicaSyncRetentionSet`].
    ConflictIsolated {
        /// Canonical conflict receipt (`conflict_digest` is order-invariant).
        conflict_receipt: ArtifactConflictReceipt,
        /// `true` when both competing blobs are retained on disk.
        preserved_both_blobs: bool,
        /// Content digests pinned in [`nlos_artifact::ReplicaSyncRetentionSet`].
        pinned_digests: Vec<ContentDigest>,
    },
    /// Deletion tombstone revision chain applied.
    TombstoneApplied {
        /// Tombstoned `ArtifactId`.
        artifact_id: ArtifactId,
        /// Applied tombstone head.
        tombstone_head: HeadState,
    },
}

impl From<CrossStoreArtifactSyncReport> for PartitionArtifactImportSummary {
    fn from(report: CrossStoreArtifactSyncReport) -> Self {
        match report {
            CrossStoreArtifactSyncReport::ImportedFastForward {
                artifact_id,
                prior_local_head,
                new_head,
                imported_revisions,
                ..
            } => Self::ImportedFastForward {
                artifact_id,
                prior_local_head,
                new_head,
                imported_revisions,
            },
            CrossStoreArtifactSyncReport::NoOpConverged { artifact_id, head } => {
                Self::NoOpConverged { artifact_id, head }
            }
            CrossStoreArtifactSyncReport::ConflictIsolated {
                conflict_receipt,
                preserved_both_blobs,
                pinned_digests,
            } => Self::ConflictIsolated {
                conflict_receipt,
                preserved_both_blobs,
                pinned_digests,
            },
            CrossStoreArtifactSyncReport::TombstoneApplied {
                artifact_id,
                tombstone_head,
                ..
            } => Self::TombstoneApplied {
                artifact_id,
                tombstone_head,
            },
        }
    }
}

/// Transport-constructible summary of a federated `ReconciliationCheckpoint`
/// recorded by a reconnected Cell.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PartitionRecordedCheckpointSummary {
    /// Owning `CellIdentity`.
    pub cell: CellIdentity,
    /// Post-reconnection `CellEpoch`.
    pub epoch: CellEpoch,
    /// Post-reconnection `CellFencingToken`.
    pub fencing_token: CellFencingToken,
    /// Republished federation heartbeat counter (`0` if registration absent).
    pub heartbeat: u64,
    /// Append sequence in the Cell's checkpoint directory.
    pub sequence: u64,
    /// OS process id recorded in the checkpoint file.
    pub os_process_id: u32,
    /// Raw 32-byte structured checkpoint digest.
    pub digest_bytes: [u8; 32],
    /// `true` when `CheckpointFact::verify_digest` returned `Verified`.
    pub digest_verified: bool,
}

/// Report of cross-Cell semantic vector checkpoint diffing and event-set
/// convergence (`[DIST-EVENT-001]`, `[DIST-AUDIT-001]`, `[SEM-CHECKPOINT-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticPartitionConvergenceReport {
    /// Pre-heal signed vector checkpoint exported by Cell A.
    pub pre_sync_checkpoint_a: SignedSemanticVectorCheckpoint,
    /// Pre-heal signed vector checkpoint exported by Cell B.
    pub pre_sync_checkpoint_b: SignedSemanticVectorCheckpoint,
    /// Pre-heal causal diff observed by Cell A against Cell B.
    pub pre_sync_diff_on_a: VectorCheckpointDiff,
    /// Pre-heal causal diff observed by Cell B against Cell A.
    pub pre_sync_diff_on_b: VectorCheckpointDiff,
    /// Atomic convergence report on Cell A.
    pub cell_a_convergence: SemanticConvergenceStepSnapshot,
    /// Atomic convergence report on Cell B.
    pub cell_b_convergence: SemanticConvergenceStepSnapshot,
    /// Post-convergence signed vector checkpoint exported by Cell A.
    pub post_sync_checkpoint_a: SignedSemanticVectorCheckpoint,
    /// Post-convergence signed vector checkpoint exported by Cell B.
    pub post_sync_checkpoint_b: SignedSemanticVectorCheckpoint,
    /// Total distinct `SemanticEventId`s present in both converged stores.
    pub converged_event_count: u64,
    /// `true` when duplicate second-delivery replay was executed and imported
    /// `0` duplicate rows on both Cells.
    pub replay_idempotency_verified: bool,
}

/// Report of cross-Cell artifact replica convergence (`[DIST-ART-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtifactPartitionConvergenceReport {
    /// Sync outcomes when importing Cell B's exported packages into Cell A.
    pub cell_a_imports_from_b: Vec<PartitionArtifactImportSummary>,
    /// Sync outcomes when importing Cell A's exported packages into Cell B.
    pub cell_b_imports_from_a: Vec<PartitionArtifactImportSummary>,
    /// Conflict receipts isolated on Cell A.
    pub conflict_receipts_on_a: Vec<ArtifactConflictReceipt>,
    /// Conflict receipts isolated on Cell B.
    pub conflict_receipts_on_b: Vec<ArtifactConflictReceipt>,
    /// Sorted unique canonical `conflict_digest` values across isolated conflicts.
    pub canonical_conflict_digests: Vec<[u8; 32]>,
}

/// Canonical machine-verifiable receipt sealing a completed 4-phase partition
/// recovery across two Cells (`W65`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PartitionRecoveryReceipt {
    /// Recovery session correlation identifier.
    pub recovery_id: [u8; 16],
    /// Cell A identity.
    pub cell_a_id: CellIdentity,
    /// Cell B identity.
    pub cell_b_id: CellIdentity,
    /// Cell A frozen fence during partition.
    pub cell_a_prior_fence: CellFence,
    /// Cell A advanced fence after reconnection.
    pub cell_a_reconnected_fence: CellFence,
    /// Cell B frozen fence during partition.
    pub cell_b_prior_fence: CellFence,
    /// Cell B advanced fence after reconnection.
    pub cell_b_reconnected_fence: CellFence,
    /// Cell A [`PartitionBoundedLossProof::proof_digest`].
    pub cell_a_bounded_loss_digest: [u8; 32],
    /// Cell B [`PartitionBoundedLossProof::proof_digest`].
    pub cell_b_bounded_loss_digest: [u8; 32],
    /// Cell A permanently quarantined quota loss.
    pub cell_a_permanently_quarantined_loss: u64,
    /// Cell B permanently quarantined quota loss.
    pub cell_b_permanently_quarantined_loss: u64,
    /// Covered semantic event Merkle root converged on both Cells.
    pub converged_semantic_events_root: [u8; 32],
    /// Total converged semantic event count on each Cell.
    pub converged_semantic_event_count: u64,
    /// Sorted canonical `conflict_digest`s for any divergent artifacts.
    pub artifact_conflict_digests: Vec<[u8; 32]>,
    /// Domain-separated `SHA-256` receipt commitment.
    pub receipt_digest: [u8; 32],
}

impl PartitionRecoveryReceipt {
    /// Computes the canonical domain-separated `SHA-256` digest of this receipt.
    #[must_use]
    pub fn compute_digest(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(PARTITION_RECOVERY_RECEIPT_DOMAIN);
        hasher.update(self.recovery_id);
        hash_fence(&mut hasher, self.cell_a_prior_fence);
        hash_fence(&mut hasher, self.cell_a_reconnected_fence);
        hash_fence(&mut hasher, self.cell_b_prior_fence);
        hash_fence(&mut hasher, self.cell_b_reconnected_fence);
        hasher.update(self.cell_a_bounded_loss_digest);
        hasher.update(self.cell_b_bounded_loss_digest);
        hasher.update(self.cell_a_permanently_quarantined_loss.to_be_bytes());
        hasher.update(self.cell_b_permanently_quarantined_loss.to_be_bytes());
        hasher.update(self.converged_semantic_events_root);
        hasher.update(self.converged_semantic_event_count.to_be_bytes());
        hash_len(&mut hasher, self.artifact_conflict_digests.len());
        for digest in &self.artifact_conflict_digests {
            hasher.update(digest);
        }
        hasher.finalize().into()
    }

    /// Verifies [`Self::receipt_digest`] fail-closed.
    #[must_use]
    pub fn verify_digest(&self) -> bool {
        self.compute_digest() == self.receipt_digest
    }
}

/// Decoded fields of a canonical 136-byte `PARTRECV` [`CheckpointPrefix`] entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecodedPartitionRecoveryCheckpointEntry {
    /// Recovery session correlation identifier (`16` bytes).
    pub recovery_id: [u8; 16],
    /// Cell A identity bytes (`16` bytes).
    pub cell_a_id: [u8; 16],
    /// Cell B identity bytes (`16` bytes).
    pub cell_b_id: [u8; 16],
    /// Cell A post-heal epoch.
    pub cell_a_epoch: u64,
    /// Cell B post-heal epoch.
    pub cell_b_epoch: u64,
    /// Cell A permanently quarantined loss.
    pub cell_a_quarantined_loss: u64,
    /// Cell B permanently quarantined loss.
    pub cell_b_quarantined_loss: u64,
    /// Converged semantic event count.
    pub converged_semantic_event_count: u64,
    /// Count of isolated artifact conflict receipts.
    pub artifact_conflict_count: u64,
    /// Sealed 32-byte [`PartitionRecoveryReceipt::receipt_digest`].
    pub receipt_digest: [u8; 32],
}

/// Encodes a [`PartitionRecoveryReceipt`] into its canonical 136-byte
/// `PARTRECV` [`CheckpointPrefix`] entry.
#[must_use]
pub fn partition_recovery_checkpoint_entry(receipt: &PartitionRecoveryReceipt) -> Vec<u8> {
    let mut out = Vec::with_capacity(136);
    out.extend_from_slice(PARTITION_RECOVERY_CHECKPOINT_ENTRY_MAGIC);
    out.extend_from_slice(&receipt.recovery_id);
    out.extend_from_slice(receipt.cell_a_id.as_bytes());
    out.extend_from_slice(receipt.cell_b_id.as_bytes());
    out.extend_from_slice(&receipt.cell_a_reconnected_fence.epoch().get().to_be_bytes());
    out.extend_from_slice(&receipt.cell_b_reconnected_fence.epoch().get().to_be_bytes());
    out.extend_from_slice(&receipt.cell_a_permanently_quarantined_loss.to_be_bytes());
    out.extend_from_slice(&receipt.cell_b_permanently_quarantined_loss.to_be_bytes());
    out.extend_from_slice(&receipt.converged_semantic_event_count.to_be_bytes());
    out.extend_from_slice(
        &u64::try_from(receipt.artifact_conflict_digests.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    out.extend_from_slice(&receipt.receipt_digest);
    out
}

/// Strict inverse of [`partition_recovery_checkpoint_entry`].
#[must_use]
#[allow(clippy::similar_names)] // Paired Cell A / Cell B fields mirror the two-peer wire frame.
pub fn decode_partition_recovery_checkpoint_entry(
    bytes: &[u8],
) -> Option<DecodedPartitionRecoveryCheckpointEntry> {
    if bytes.len() != 136 || &bytes[..8] != PARTITION_RECOVERY_CHECKPOINT_ENTRY_MAGIC {
        return None;
    }
    let recovery_id: [u8; 16] = bytes[8..24].try_into().ok()?;
    let cell_a_id: [u8; 16] = bytes[24..40].try_into().ok()?;
    let cell_b_id: [u8; 16] = bytes[40..56].try_into().ok()?;
    let cell_a_epoch = u64::from_be_bytes(bytes[56..64].try_into().ok()?);
    let cell_b_epoch = u64::from_be_bytes(bytes[64..72].try_into().ok()?);
    if cell_a_epoch == 0 || cell_b_epoch == 0 {
        return None;
    }
    let cell_a_quarantined_loss = u64::from_be_bytes(bytes[72..80].try_into().ok()?);
    let cell_b_quarantined_loss = u64::from_be_bytes(bytes[80..88].try_into().ok()?);
    let converged_semantic_event_count = u64::from_be_bytes(bytes[88..96].try_into().ok()?);
    let artifact_conflict_count = u64::from_be_bytes(bytes[96..104].try_into().ok()?);
    let receipt_digest: [u8; 32] = bytes[104..136].try_into().ok()?;
    Some(DecodedPartitionRecoveryCheckpointEntry {
        recovery_id,
        cell_a_id,
        cell_b_id,
        cell_a_epoch,
        cell_b_epoch,
        cell_a_quarantined_loss,
        cell_b_quarantined_loss,
        converged_semantic_event_count,
        artifact_conflict_count,
        receipt_digest,
    })
}

/// Builds the canonical single-entry [`CheckpointPrefix`] committing a
/// [`PartitionRecoveryReceipt`].
#[must_use]
pub fn partition_recovery_checkpoint_prefix(
    receipt: &PartitionRecoveryReceipt,
) -> CheckpointPrefix {
    CheckpointPrefix::from_entries([partition_recovery_checkpoint_entry(receipt)])
}

/// Complete outcome of [`drive_partition_reconnection_convergence`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PartitionRecoveryOutcome {
    /// Sealed recovery receipt covering both Cells.
    pub receipt: PartitionRecoveryReceipt,
    /// Epoch-advance snapshot for Cell A.
    pub cell_a_epoch_advance: PartitionEpochAdvanceSnapshot,
    /// Epoch-advance snapshot for Cell B.
    pub cell_b_epoch_advance: PartitionEpochAdvanceSnapshot,
    /// Verified bounded-loss lease settlement proof for Cell A.
    pub cell_a_bounded_loss: PartitionBoundedLossProof,
    /// Verified bounded-loss lease settlement proof for Cell B.
    pub cell_b_bounded_loss: PartitionBoundedLossProof,
    /// Cross-Cell semantic event convergence report.
    pub semantic_convergence: SemanticPartitionConvergenceReport,
    /// Cross-Cell artifact replica convergence report.
    pub artifact_convergence: ArtifactPartitionConvergenceReport,
    /// Optional structured federation checkpoint recorded on Cell A.
    pub cell_a_checkpoint: Option<PartitionRecordedCheckpointSummary>,
    /// Optional structured federation checkpoint recorded on Cell B.
    pub cell_b_checkpoint: Option<PartitionRecordedCheckpointSummary>,
}

/// Transport-neutral endpoint interface for one partitioned [`CellHost`],
/// implemented both in-process ([`LocalCellHostPartitionEndpoint`]) and over
/// cross-process IPC for live dual-OS-process Cell recovery (`ADR-0018`).
#[allow(clippy::missing_errors_doc)] // Every trait operation propagates local authority or IPC errors via `PartitionRecoveryError`.
pub trait CellPartitionEndpoint {
    /// Current [`CellFence`] snapshot of the Cell.
    fn current_fence(&mut self) -> Result<CellFence, PartitionRecoveryError>;

    /// Current [`ControlPlaneLinkState`] of the Cell's [`PartitionAutonomyGate`](nlos_cell::PartitionAutonomyGate).
    fn connectivity_state(&mut self) -> Result<ControlPlaneLinkState, PartitionRecoveryError>;

    /// Advances the Cell's `CellEpoch` + `CellFencingToken`, quarantining
    /// unsettled `QuotaLease`s and broadcasting to detector + name cache.
    fn advance_cell_epoch(
        &mut self,
    ) -> Result<PartitionEpochAdvanceSnapshot, PartitionRecoveryError>;

    /// Transitions the Cell's gate from `Partitioned` to `Reconnecting`.
    fn begin_reconnection(
        &mut self,
        now_tick: u64,
    ) -> Result<ControlPlaneLinkState, PartitionRecoveryError>;

    /// Transitions the Cell's gate from `Reconnecting` to `Connected`.
    fn complete_reconnection(
        &mut self,
        now_tick: u64,
    ) -> Result<ControlPlaneLinkState, PartitionRecoveryError>;

    /// Current `AVAILABLE` pool of the Cell's quota lease sub-ledger.
    fn quota_available(&mut self) -> Result<u64, PartitionRecoveryError>;

    /// Settles one quarantined `QuotaLease` via [`QuotaReconciliationReceipt`]
    /// and [`FenceBarrierAck`], returning the verified per-lease summary and
    /// incremental refund added to `AVAILABLE`.
    fn settle_quarantined_quota_lease(
        &mut self,
        spec: QuotaPartitionSettlementSpec,
    ) -> Result<(QuotaLeaseReconciliationSummary, u64), PartitionRecoveryError>;

    /// Quarantines an unsettled `CapacityLease` and reconciles it via
    /// [`CapacityReconciliationReceipt`].
    fn quarantine_and_reconcile_capacity_lease(
        &mut self,
        spec: CapacityPartitionSettlementSpec,
    ) -> Result<PartitionSettledCapacitySummary, PartitionRecoveryError>;

    /// Quarantines an unsettled `ExclusiveDeviceLease` and reconciles it via
    /// [`DeviceReconciliationReceipt`].
    fn quarantine_and_reconcile_device_lease(
        &mut self,
        spec: DevicePartitionSettlementSpec,
    ) -> Result<PartitionSettledDeviceSummary, PartitionRecoveryError>;

    /// Exports the public-key [`BootstrapPrincipalRequest`] of this Cell's
    /// dedicated `SemanticWriter`.
    fn export_semantic_writer_principal(
        &mut self,
    ) -> Result<BootstrapPrincipalRequest, PartitionRecoveryError>;

    /// Idempotently registers a remote peer's public-key principal binding in
    /// this Cell's `IdentityAuthority`.
    fn register_peer_principal(
        &mut self,
        request: BootstrapPrincipalRequest,
    ) -> Result<(), PartitionRecoveryError>;

    /// Exports and signs a [`SignedSemanticVectorCheckpoint`].
    fn export_signed_vector_checkpoint(
        &mut self,
        trust_snapshot_digest: [u8; 32],
        signed_at_ms: u64,
    ) -> Result<SignedSemanticVectorCheckpoint, PartitionRecoveryError>;

    /// Verifies a peer's [`SignedSemanticVectorCheckpoint`] against this Cell's
    /// `IdentityAuthority`.
    fn verify_peer_vector_checkpoint(
        &mut self,
        checkpoint: &SignedSemanticVectorCheckpoint,
    ) -> Result<(), PartitionRecoveryError>;

    /// Exports this Cell's committed cross-domain semantic events.
    fn export_cross_domain_events(
        &mut self,
        trust_snapshot_digest: [u8; 32],
    ) -> Result<Vec<CrossDomainEventItem>, PartitionRecoveryError>;

    /// Atomically verifies and converges remote cross-domain semantic events
    /// into this Cell's `SemanticAuthority`.
    fn converge_cross_domain_events(
        &mut self,
        local_trust_snapshot_digest: [u8; 32],
        remote_items: &[CrossDomainEventItem],
    ) -> Result<SemanticConvergenceStepSnapshot, PartitionRecoveryError>;

    /// Exports an [`ArtifactTransferPackage`] up to the canonical head of
    /// `artifact_id`.
    fn export_artifact_package(
        &mut self,
        artifact_id: ArtifactId,
        now_ms: u64,
    ) -> Result<ArtifactTransferPackage, PartitionRecoveryError>;

    /// Verifies and imports an [`ArtifactTransferPackage`] under `policies` and
    /// `commit_ctx`.
    fn import_artifact_package(
        &mut self,
        policies: &[ArtifactSyncPolicy],
        package: &ArtifactTransferPackage,
        commit_ctx: ArtifactImportCommitContext,
        idempotency_key: IdempotencyKey,
        synced_at_ms: u64,
    ) -> Result<PartitionArtifactImportSummary, PartitionRecoveryError>;

    /// Refreshes federation registration and records a structured
    /// [`CheckpointPrefix`] if this Cell was opened federated.
    fn refresh_and_record_checkpoint(
        &mut self,
        prefix: &CheckpointPrefix,
    ) -> Result<Option<PartitionRecordedCheckpointSummary>, PartitionRecoveryError>;
}

/// In-process [`CellPartitionEndpoint`] backed directly by a `&mut CellHost`.
pub struct LocalCellHostPartitionEndpoint<'a> {
    host: &'a mut CellHost,
}

impl<'a> LocalCellHostPartitionEndpoint<'a> {
    /// Wraps `host` as a [`CellPartitionEndpoint`].
    #[must_use]
    pub fn new(host: &'a mut CellHost) -> Self {
        Self { host }
    }
}

impl CellPartitionEndpoint for LocalCellHostPartitionEndpoint<'_> {
    fn current_fence(&mut self) -> Result<CellFence, PartitionRecoveryError> {
        Ok(self.host.fence())
    }

    fn connectivity_state(&mut self) -> Result<ControlPlaneLinkState, PartitionRecoveryError> {
        Ok(self.host.connectivity_state())
    }

    fn advance_cell_epoch(
        &mut self,
    ) -> Result<PartitionEpochAdvanceSnapshot, PartitionRecoveryError> {
        let adv = self.host.advance_cell_epoch()?;
        Ok(PartitionEpochAdvanceSnapshot {
            fence: adv.fence,
            entries_fenced: adv.name_cache.entries_fenced,
        })
    }

    fn begin_reconnection(
        &mut self,
        now_tick: u64,
    ) -> Result<ControlPlaneLinkState, PartitionRecoveryError> {
        Ok(self.host.begin_control_plane_reconnection(now_tick)?)
    }

    fn complete_reconnection(
        &mut self,
        now_tick: u64,
    ) -> Result<ControlPlaneLinkState, PartitionRecoveryError> {
        Ok(self.host.complete_control_plane_reconnection(now_tick)?)
    }

    fn quota_available(&mut self) -> Result<u64, PartitionRecoveryError> {
        Ok(self.host.quota_available())
    }

    fn settle_quarantined_quota_lease(
        &mut self,
        spec: QuotaPartitionSettlementSpec,
    ) -> Result<(QuotaLeaseReconciliationSummary, u64), PartitionRecoveryError> {
        if spec.unattested_inflight_loss > spec.max_unreported_inflight_window {
            return Err(PartitionRecoveryError::BoundedLossViolation(
                "unattested_inflight_loss exceeds max_unreported_inflight_window",
            ));
        }
        let quarantined = self.host.query_quota_lease(spec.lease_id)?;
        if !matches!(
            quarantined.state(),
            QuotaLeaseState::Quarantined | QuotaLeaseState::Fenced
        ) {
            return Err(PartitionRecoveryError::QuotaReconcile(
                QuotaReconcileError::NotQuarantinedOrFenced {
                    state: quarantined.state(),
                },
            ));
        }
        let face_value = quarantined.face_value();
        let consumed_or_lost = spec
            .attested_spent
            .checked_add(spec.unattested_inflight_loss)
            .ok_or(PartitionRecoveryError::BoundedLossViolation(
                "attested_spent + unattested_inflight_loss overflowed u64",
            ))?;
        if consumed_or_lost > face_value {
            return Err(PartitionRecoveryError::BoundedLossViolation(
                "attested_spent + unattested_inflight_loss exceeds lease face_value",
            ));
        }
        let returned_to_available = face_value - consumed_or_lost;
        if returned_to_available < quarantined.returned() {
            return Err(PartitionRecoveryError::QuotaReconcile(
                QuotaReconcileError::ReturnedRegression {
                    reported: returned_to_available,
                    committed: quarantined.returned(),
                },
            ));
        }
        let delta_refund = returned_to_available - quarantined.returned();
        let ack = FenceBarrierAck::for_quota(&quarantined, spec.gateway_id, spec.attested_spent);
        if !ack.verify_digest() {
            return Err(PartitionRecoveryError::QuotaReconcile(
                QuotaReconcileError::InvalidFenceBarrierAck {
                    reason: "FenceBarrierAck self-verification failed",
                },
            ));
        }
        let ack_digest = ack.ack_digest();
        let receipt = QuotaReconciliationReceipt::for_lease(
            spec.receipt_id,
            spec.reconcile_seq,
            &quarantined,
            spec.attested_spent,
            returned_to_available,
            spec.unattested_inflight_loss,
            ack,
        );
        let settled = self.host.reconcile_quota_lease(receipt)?;
        Ok((
            QuotaLeaseReconciliationSummary {
                lease_id: spec.lease_id,
                receipt_id: spec.receipt_id,
                prior_epoch: quarantined.epoch(),
                face_value,
                attested_spent: settled.spent(),
                refunded_to_available: settled.returned(),
                permanently_quarantined_loss: settled.remaining(),
                max_unreported_inflight_window: spec.max_unreported_inflight_window,
                post_reconcile_state: settled.state(),
                fence_barrier_ack_digest: ack_digest,
            },
            delta_refund,
        ))
    }

    fn quarantine_and_reconcile_capacity_lease(
        &mut self,
        spec: CapacityPartitionSettlementSpec,
    ) -> Result<PartitionSettledCapacitySummary, PartitionRecoveryError> {
        let quarantined = self
            .host
            .quarantine_capacity_lease(spec.capacity_lease_id)?;
        let ack = FenceBarrierAck::for_capacity(&quarantined, spec.gateway_id);
        let fence_ack_digest = ack.ack_digest();
        let receipt = CapacityReconciliationReceipt::for_lease(
            spec.receipt_id,
            &quarantined,
            spec.cleanup_attestation,
            ack,
        );
        let settled = self.host.reconcile_capacity_lease(receipt)?;
        Ok(PartitionSettledCapacitySummary {
            capacity_lease_id: settled.capacity_lease_id(),
            amount: settled.amount(),
            final_state: settled.state(),
            fence_ack_digest,
        })
    }

    fn quarantine_and_reconcile_device_lease(
        &mut self,
        spec: DevicePartitionSettlementSpec,
    ) -> Result<PartitionSettledDeviceSummary, PartitionRecoveryError> {
        let quarantined = self.host.quarantine_device_lease(spec.device_lease_id)?;
        let ack = FenceBarrierAck::for_device(&quarantined, spec.gateway_id);
        let fence_ack_digest = ack.ack_digest();
        let receipt = DeviceReconciliationReceipt::for_lease(
            spec.receipt_id,
            &quarantined,
            true,
            true,
            true,
            spec.reset_receipt,
            ack,
        );
        let settled = self.host.reconcile_device_lease(receipt)?;
        Ok(PartitionSettledDeviceSummary {
            device_lease_id: settled.device_lease_id(),
            device_id: settled.device_id(),
            exclusivity_epoch: settled.exclusivity_epoch(),
            reset_generation: settled.reset_generation(),
            final_state: settled.state(),
            fence_ack_digest,
        })
    }

    fn export_semantic_writer_principal(
        &mut self,
    ) -> Result<BootstrapPrincipalRequest, PartitionRecoveryError> {
        Ok(self.host.semantic_writer_bootstrap_request()?)
    }

    fn register_peer_principal(
        &mut self,
        request: BootstrapPrincipalRequest,
    ) -> Result<(), PartitionRecoveryError> {
        let _ = self
            .host
            .runtime()
            .identity
            .bootstrap_principal(request)
            .map_err(SliceKError::from)?;
        Ok(())
    }

    fn export_signed_vector_checkpoint(
        &mut self,
        trust_snapshot_digest: [u8; 32],
        signed_at_ms: u64,
    ) -> Result<SignedSemanticVectorCheckpoint, PartitionRecoveryError> {
        let runtime = self.host.runtime();
        Ok(runtime.semantic().export_signed_vector_checkpoint(
            &runtime.identity,
            runtime.semantic_writer(),
            trust_snapshot_digest,
            signed_at_ms,
        )?)
    }

    fn verify_peer_vector_checkpoint(
        &mut self,
        checkpoint: &SignedSemanticVectorCheckpoint,
    ) -> Result<(), PartitionRecoveryError> {
        verify_signed_vector_checkpoint(&self.host.runtime().identity, checkpoint)?;
        Ok(())
    }

    fn export_cross_domain_events(
        &mut self,
        trust_snapshot_digest: [u8; 32],
    ) -> Result<Vec<CrossDomainEventItem>, PartitionRecoveryError> {
        Ok(self
            .host
            .runtime()
            .semantic()
            .export_cross_domain_event_set(trust_snapshot_digest)?)
    }

    fn converge_cross_domain_events(
        &mut self,
        local_trust_snapshot_digest: [u8; 32],
        remote_items: &[CrossDomainEventItem],
    ) -> Result<SemanticConvergenceStepSnapshot, PartitionRecoveryError> {
        let runtime = self.host.runtime();
        let report = runtime.semantic().import_cross_domain_event_set(
            &runtime.identity,
            runtime.semantic_writer(),
            local_trust_snapshot_digest,
            remote_items,
        )?;
        Ok(SemanticConvergenceStepSnapshot {
            imported_event_ids: report.imported_event_ids,
            skipped_existing_event_ids: report.skipped_existing_event_ids,
            audit_record_count: u64::try_from(report.audit_records.len()).unwrap_or(u64::MAX),
            vector_checkpoint: report.vector_checkpoint,
        })
    }

    fn export_artifact_package(
        &mut self,
        artifact_id: ArtifactId,
        now_ms: u64,
    ) -> Result<ArtifactTransferPackage, PartitionRecoveryError> {
        let fence = self.host.fence();
        let req = ExportArtifactTransferRequest::head(
            artifact_id,
            *fence.identity().as_bytes(),
            fence.epoch().get(),
            now_ms,
        );
        Ok(export_artifact_transfer_package(
            &self.host.runtime().artifacts,
            req,
        )?)
    }

    fn import_artifact_package(
        &mut self,
        policies: &[ArtifactSyncPolicy],
        package: &ArtifactTransferPackage,
        commit_ctx: ArtifactImportCommitContext,
        idempotency_key: IdempotencyKey,
        synced_at_ms: u64,
    ) -> Result<PartitionArtifactImportSummary, PartitionRecoveryError> {
        let mut registry = ArtifactSyncPolicyRegistry::new();
        for policy in policies {
            registry.register(policy.clone())?;
        }
        // Take retention set temporarily to avoid split-borrow across `self.host`.
        let mut retention = std::mem::take(self.host.artifact_sync_retention_mut());
        let result = import_artifact_transfer_package(
            &self.host.runtime().artifacts,
            &registry,
            &mut retention,
            ImportArtifactTransferRequest {
                package,
                expected_local_state: None,
                task_id: commit_ctx.task_id,
                permit_id: commit_ctx.permit_id,
                write_set_root: commit_ctx.write_set_root,
                idempotency_key,
                synced_at_ms,
            },
        );
        *self.host.artifact_sync_retention_mut() = retention;
        Ok(PartitionArtifactImportSummary::from(result?))
    }

    fn refresh_and_record_checkpoint(
        &mut self,
        prefix: &CheckpointPrefix,
    ) -> Result<Option<PartitionRecordedCheckpointSummary>, PartitionRecoveryError> {
        let reg = self.host.refresh_cell_registration()?;
        let heartbeat = reg.map_or(0, |entry| entry.heartbeat());
        let ckpt = self
            .host
            .record_structured_reconciliation_checkpoint(prefix)?;
        Ok(
            ckpt.map(|(digest, record)| PartitionRecordedCheckpointSummary {
                cell: record.cell(),
                epoch: record.fact().epoch(),
                fencing_token: record.fact().fencing_token(),
                heartbeat,
                sequence: record.sequence(),
                os_process_id: record.os_process_id(),
                digest_bytes: *digest.as_bytes(),
                digest_verified: record.fact().verify_digest(prefix)
                    == CheckpointDigestVerification::Verified,
            }),
        )
    }
}

/// Drives 4-phase partition reconnection convergence across two partitioned
/// Cell endpoints (`[DIST-LOCAL-001]`, `[LEASE-LOSS-001]`, `[LEASE-FENCE-001]`,
/// `[LEASE-RECON-001]`, `[ROAD-C-001]`, `[DIST-EVENT-001]`, `[DIST-AUDIT-001]`,
/// `[SEM-CHECKPOINT-001]`, `[DIST-ART-001]`).
///
/// # Errors
///
/// Fails closed with [`PartitionRecoveryError`] on any state, fence,
/// bounded-loss conservation, semantic verification, artifact policy/tamper, or
/// checkpoint verification violation.
#[allow(clippy::too_many_lines)]
#[allow(clippy::similar_names)] // Paired Cell A / Cell B orchestration variables are intentionally symmetric.
pub fn drive_partition_reconnection_convergence(
    cell_a: &mut impl CellPartitionEndpoint,
    cell_b: &mut impl CellPartitionEndpoint,
    request: &PartitionRecoveryRequest,
) -> Result<PartitionRecoveryOutcome, PartitionRecoveryError> {
    if request.completion_tick < request.reconnection_tick {
        return Err(PartitionRecoveryError::PartitionState(
            PartitionViolation::NonMonotonicTick {
                previous_tick: request.reconnection_tick,
                supplied_tick: request.completion_tick,
            },
        ));
    }

    // =========================================================================
    // Phase 1: Epoch Advance & Quota Freeze (`[LEASE-LOSS-001]`, `[LEASE-FENCE-001]`)
    // =========================================================================
    let state_a = cell_a.connectivity_state()?;
    let ControlPlaneLinkState::Partitioned {
        frozen_fence: prior_fence_a,
        ..
    } = state_a
    else {
        return Err(PartitionRecoveryError::PartitionState(
            PartitionViolation::InvalidStateTransition {
                current_phase: state_a.phase(),
                attempted: "drive_partition_reconnection_convergence(cell_a)",
            },
        ));
    };

    let state_b = cell_b.connectivity_state()?;
    let ControlPlaneLinkState::Partitioned {
        frozen_fence: prior_fence_b,
        ..
    } = state_b
    else {
        return Err(PartitionRecoveryError::PartitionState(
            PartitionViolation::InvalidStateTransition {
                current_phase: state_b.phase(),
                attempted: "drive_partition_reconnection_convergence(cell_b)",
            },
        ));
    };

    let available_pre_advance_a = cell_a.quota_available()?;
    let available_pre_advance_b = cell_b.quota_available()?;

    let cell_a_epoch_advance = cell_a.advance_cell_epoch()?;
    let cell_b_epoch_advance = cell_b.advance_cell_epoch()?;

    // Verify `[LEASE-LOSS-001]`: advancing epoch MUST NOT return any unverified
    // balance from unsettled leases into `AVAILABLE`!
    let available_post_advance_a = cell_a.quota_available()?;
    let available_post_advance_b = cell_b.quota_available()?;
    if available_post_advance_a != available_pre_advance_a
        || available_post_advance_b != available_pre_advance_b
    {
        return Err(PartitionRecoveryError::BoundedLossViolation(
            "epoch advance altered AVAILABLE pool before reconciliation",
        ));
    }

    cell_a.begin_reconnection(request.reconnection_tick)?;
    cell_b.begin_reconnection(request.reconnection_tick)?;

    // =========================================================================
    // Phase 2: Bounded-Loss Lease Reconciliation (`[LEASE-RECON-001]`, `[ROAD-C-001]`)
    // =========================================================================
    let cell_a_bounded_loss = settle_cell_leases_with_bounded_loss(
        cell_a,
        prior_fence_a,
        cell_a_epoch_advance.fence,
        available_post_advance_a,
        &request.cell_a_leases,
    )?;
    let cell_b_bounded_loss = settle_cell_leases_with_bounded_loss(
        cell_b,
        prior_fence_b,
        cell_b_epoch_advance.fence,
        available_post_advance_b,
        &request.cell_b_leases,
    )?;

    // =========================================================================
    // Phase 3: Cross-Cell Semantic Event Convergence (`[DIST-EVENT-001]`, `[SEM-CHECKPOINT-001]`)
    // =========================================================================
    let semantic_convergence = converge_partitioned_semantics(
        cell_a,
        cell_b,
        request.trust_snapshot_digest_a,
        request.trust_snapshot_digest_b,
        request.synced_at_ms,
        &request.semantic_convergence,
    )?;

    // =========================================================================
    // Phase 4: Artifact Replica Convergence & Checkpoint Sealing (`[DIST-ART-001]`)
    // =========================================================================
    let artifact_convergence = converge_partitioned_artifacts(
        cell_a,
        cell_b,
        request.synced_at_ms,
        &request.artifact_convergence,
    )?;

    cell_a.complete_reconnection(request.completion_tick)?;
    cell_b.complete_reconnection(request.completion_tick)?;

    let mut receipt = PartitionRecoveryReceipt {
        recovery_id: request.recovery_id,
        cell_a_id: prior_fence_a.identity(),
        cell_b_id: prior_fence_b.identity(),
        cell_a_prior_fence: prior_fence_a,
        cell_a_reconnected_fence: cell_a_epoch_advance.fence,
        cell_b_prior_fence: prior_fence_b,
        cell_b_reconnected_fence: cell_b_epoch_advance.fence,
        cell_a_bounded_loss_digest: cell_a_bounded_loss.proof_digest,
        cell_b_bounded_loss_digest: cell_b_bounded_loss.proof_digest,
        cell_a_permanently_quarantined_loss: cell_a_bounded_loss.permanently_quarantined_loss,
        cell_b_permanently_quarantined_loss: cell_b_bounded_loss.permanently_quarantined_loss,
        converged_semantic_events_root: semantic_convergence
            .post_sync_checkpoint_a
            .covered_event_ids_root,
        converged_semantic_event_count: semantic_convergence.converged_event_count,
        artifact_conflict_digests: artifact_convergence.canonical_conflict_digests.clone(),
        receipt_digest: [0_u8; 32],
    };
    receipt.receipt_digest = receipt.compute_digest();

    let (cell_a_checkpoint, cell_b_checkpoint) = if request.record_federation_checkpoints {
        let prefix = partition_recovery_checkpoint_prefix(&receipt);
        let ckpt_a = cell_a.refresh_and_record_checkpoint(&prefix)?;
        let ckpt_b = cell_b.refresh_and_record_checkpoint(&prefix)?;
        if let Some(ref rec_a) = ckpt_a
            && !rec_a.digest_verified
        {
            return Err(PartitionRecoveryError::CheckpointVerificationFailed(
                "Cell A structured checkpoint failed verify_digest",
            ));
        }
        if let Some(ref rec_b) = ckpt_b
            && !rec_b.digest_verified
        {
            return Err(PartitionRecoveryError::CheckpointVerificationFailed(
                "Cell B structured checkpoint failed verify_digest",
            ));
        }
        (ckpt_a, ckpt_b)
    } else {
        (None, None)
    };

    Ok(PartitionRecoveryOutcome {
        receipt,
        cell_a_epoch_advance,
        cell_b_epoch_advance,
        cell_a_bounded_loss,
        cell_b_bounded_loss,
        semantic_convergence,
        artifact_convergence,
        cell_a_checkpoint,
        cell_b_checkpoint,
    })
}

#[allow(clippy::too_many_lines)] // Single end-to-end per-Cell three-family lease settlement + bounded-loss proof builder.
fn settle_cell_leases_with_bounded_loss(
    endpoint: &mut impl CellPartitionEndpoint,
    prior_frozen_fence: CellFence,
    reconnected_fence: CellFence,
    pool_available_before_reconcile: u64,
    spec: &CellLeaseReconciliationSpec,
) -> Result<PartitionBoundedLossProof, PartitionRecoveryError> {
    let mut total_granted_face_value = 0_u64;
    let mut reconciled_spent = 0_u64;
    let mut refunded_available = 0_u64;
    let mut permanently_quarantined_loss = 0_u64;
    let mut max_unreported_inflight_window = 0_u64;
    let mut expected_delta_refunded = 0_u64;
    let mut quota_summaries = Vec::with_capacity(spec.quota_leases.len());

    for item in &spec.quota_leases {
        if item.unattested_inflight_loss > item.max_unreported_inflight_window {
            return Err(PartitionRecoveryError::BoundedLossViolation(
                "unattested_inflight_loss exceeds max_unreported_inflight_window",
            ));
        }
        let (summary, delta_refund) = endpoint.settle_quarantined_quota_lease(*item)?;

        total_granted_face_value = total_granted_face_value
            .checked_add(summary.face_value)
            .ok_or(PartitionRecoveryError::BoundedLossViolation(
                "total_granted_face_value overflow",
            ))?;
        reconciled_spent = reconciled_spent.checked_add(summary.attested_spent).ok_or(
            PartitionRecoveryError::BoundedLossViolation("reconciled_spent overflow"),
        )?;
        refunded_available = refunded_available
            .checked_add(summary.refunded_to_available)
            .ok_or(PartitionRecoveryError::BoundedLossViolation(
                "refunded_available overflow",
            ))?;
        permanently_quarantined_loss = permanently_quarantined_loss
            .checked_add(summary.permanently_quarantined_loss)
            .ok_or(PartitionRecoveryError::BoundedLossViolation(
                "permanently_quarantined_loss overflow",
            ))?;
        max_unreported_inflight_window = max_unreported_inflight_window
            .checked_add(summary.max_unreported_inflight_window)
            .ok_or(PartitionRecoveryError::BoundedLossViolation(
                "max_unreported_inflight_window overflow",
            ))?;
        expected_delta_refunded = expected_delta_refunded.checked_add(delta_refund).ok_or(
            PartitionRecoveryError::BoundedLossViolation("expected_delta_refunded overflow"),
        )?;

        quota_summaries.push(summary);
    }

    let pool_available_after_reconcile = endpoint.quota_available()?;
    if pool_available_after_reconcile
        != pool_available_before_reconcile.saturating_add(expected_delta_refunded)
    {
        return Err(PartitionRecoveryError::BoundedLossViolation(
            "post-reconciliation AVAILABLE pool disagrees with exact proven refunds",
        ));
    }

    let mut reconciled_capacity_leases = Vec::with_capacity(spec.capacity_leases.len());
    for cap_spec in &spec.capacity_leases {
        reconciled_capacity_leases
            .push(endpoint.quarantine_and_reconcile_capacity_lease(*cap_spec)?);
    }

    let mut reconciled_device_leases = Vec::with_capacity(spec.device_leases.len());
    for dev_spec in &spec.device_leases {
        reconciled_device_leases.push(endpoint.quarantine_and_reconcile_device_lease(*dev_spec)?);
    }

    let mut proof = PartitionBoundedLossProof {
        cell_id: prior_frozen_fence.identity(),
        prior_frozen_fence,
        reconnected_fence,
        pool_available_before_reconcile,
        pool_available_after_reconcile,
        total_granted_face_value,
        reconciled_spent,
        refunded_available,
        permanently_quarantined_loss,
        max_unreported_inflight_window,
        quota_summaries,
        reconciled_capacity_leases,
        reconciled_device_leases,
        proof_digest: [0_u8; 32],
    };
    proof.proof_digest = proof.compute_digest();
    if !proof.verify_invariants() {
        return Err(PartitionRecoveryError::BoundedLossViolation(
            "PartitionBoundedLossProof failed self-verification",
        ));
    }
    Ok(proof)
}

#[allow(clippy::similar_names)] // Paired Cell A / Cell B semantic convergence bindings are intentionally symmetric.
fn converge_partitioned_semantics(
    cell_a: &mut impl CellPartitionEndpoint,
    cell_b: &mut impl CellPartitionEndpoint,
    trust_snapshot_a: [u8; 32],
    trust_snapshot_b: [u8; 32],
    synced_at_ms: u64,
    spec: &SemanticPartitionConvergenceSpec,
) -> Result<SemanticPartitionConvergenceReport, PartitionRecoveryError> {
    // 1. Exchange public-key writer principals + any caller-declared principals.
    let writer_principal_a = cell_a.export_semantic_writer_principal()?;
    let writer_principal_b = cell_b.export_semantic_writer_principal()?;
    cell_a.register_peer_principal(writer_principal_b)?;
    cell_b.register_peer_principal(writer_principal_a)?;
    for extra_a in &spec.additional_principals_for_a {
        cell_a.register_peer_principal(*extra_a)?;
    }
    for extra_b in &spec.additional_principals_for_b {
        cell_b.register_peer_principal(*extra_b)?;
    }

    // 2. Export & cross-verify pre-heal signed vector checkpoints (`[SEM-CHECKPOINT-001]`).
    let pre_sync_checkpoint_a =
        cell_a.export_signed_vector_checkpoint(trust_snapshot_a, synced_at_ms)?;
    let pre_sync_checkpoint_b =
        cell_b.export_signed_vector_checkpoint(trust_snapshot_b, synced_at_ms)?;
    cell_a.verify_peer_vector_checkpoint(&pre_sync_checkpoint_b)?;
    cell_b.verify_peer_vector_checkpoint(&pre_sync_checkpoint_a)?;

    let pre_sync_diff_on_a = diff_vector_checkpoints(
        &pre_sync_checkpoint_a.checkpoint(),
        &pre_sync_checkpoint_b.checkpoint(),
    );
    let pre_sync_diff_on_b = diff_vector_checkpoints(
        &pre_sync_checkpoint_b.checkpoint(),
        &pre_sync_checkpoint_a.checkpoint(),
    );

    // 3. Export signed cross-domain event sets and converge when needed.
    let events_from_a = cell_a.export_cross_domain_events(trust_snapshot_a)?;
    let events_from_b = cell_b.export_cross_domain_events(trust_snapshot_b)?;

    let cell_a_convergence =
        cell_a.converge_cross_domain_events(trust_snapshot_a, &events_from_b)?;
    let cell_b_convergence =
        cell_b.converge_cross_domain_events(trust_snapshot_b, &events_from_a)?;

    let mut replay_idempotency_verified = false;
    if spec.verify_idempotent_second_replay {
        let replay_a = cell_a.converge_cross_domain_events(trust_snapshot_a, &events_from_b)?;
        let replay_b = cell_b.converge_cross_domain_events(trust_snapshot_b, &events_from_a)?;
        if !replay_a.imported_event_ids.is_empty() || !replay_b.imported_event_ids.is_empty() {
            return Err(PartitionRecoveryError::BoundedLossViolation(
                "duplicate cross-domain event replay imported non-zero rows",
            ));
        }
        replay_idempotency_verified = true;
    }

    // 4. Export post-convergence signed vector checkpoints and verify equality!
    let post_sync_checkpoint_a =
        cell_a.export_signed_vector_checkpoint(trust_snapshot_a, synced_at_ms)?;
    let post_sync_checkpoint_b =
        cell_b.export_signed_vector_checkpoint(trust_snapshot_b, synced_at_ms)?;
    cell_a.verify_peer_vector_checkpoint(&post_sync_checkpoint_b)?;
    cell_b.verify_peer_vector_checkpoint(&post_sync_checkpoint_a)?;

    // Compare per-domain event counts & Merkle roots (`covered_event_ids_root`).
    // Note: each domain watermark also records `trust_snapshot_digest` of the
    // exporting store, so when `trust_snapshot_a == trust_snapshot_b` (or when
    // comparing domain event counts + Merkle roots), `covered_event_ids_root`
    // MUST match identically across both Cells!
    if post_sync_checkpoint_a.covered_event_ids_root
        != post_sync_checkpoint_b.covered_event_ids_root
    {
        let post_diff = diff_vector_checkpoints(
            &post_sync_checkpoint_a.checkpoint(),
            &post_sync_checkpoint_b.checkpoint(),
        );
        return Err(PartitionRecoveryError::SemanticConvergenceIncomplete {
            relation: post_diff.relation,
        });
    }
    let post_diff = diff_vector_checkpoints(
        &post_sync_checkpoint_a.checkpoint(),
        &post_sync_checkpoint_b.checkpoint(),
    );
    if post_diff.relation != VectorCheckpointRelation::Equal {
        return Err(PartitionRecoveryError::SemanticConvergenceIncomplete {
            relation: post_diff.relation,
        });
    }

    let converged_event_count: u64 = post_sync_checkpoint_a
        .domain_watermarks
        .iter()
        .map(|wm| wm.event_count)
        .sum();

    Ok(SemanticPartitionConvergenceReport {
        pre_sync_checkpoint_a,
        pre_sync_checkpoint_b,
        pre_sync_diff_on_a,
        pre_sync_diff_on_b,
        cell_a_convergence,
        cell_b_convergence,
        post_sync_checkpoint_a,
        post_sync_checkpoint_b,
        converged_event_count,
        replay_idempotency_verified,
    })
}

fn converge_partitioned_artifacts(
    cell_a: &mut impl CellPartitionEndpoint,
    cell_b: &mut impl CellPartitionEndpoint,
    synced_at_ms: u64,
    spec: &ArtifactPartitionConvergenceSpec,
) -> Result<ArtifactPartitionConvergenceReport, PartitionRecoveryError> {
    // Validate policies upfront so any malformed/conflicting policy fails closed
    // before touching either store.
    let mut check_registry = ArtifactSyncPolicyRegistry::new();
    for policy in &spec.policies {
        check_registry.register(policy.clone())?;
    }

    let mut packages_from_a = Vec::with_capacity(spec.cell_a_exports.len());
    for artifact_id in &spec.cell_a_exports {
        packages_from_a.push(cell_a.export_artifact_package(*artifact_id, synced_at_ms)?);
    }

    let mut packages_from_b = Vec::with_capacity(spec.cell_b_exports.len());
    for artifact_id in &spec.cell_b_exports {
        packages_from_b.push(cell_b.export_artifact_package(*artifact_id, synced_at_ms)?);
    }

    let mut cell_b_imports_from_a = Vec::with_capacity(packages_from_a.len());
    let mut conflict_receipts_on_b = Vec::new();
    for (idx, pkg) in packages_from_a.iter().enumerate() {
        let key = derive_import_key(
            spec.cell_b_import_context.base_idempotency_key,
            pkg.bundle.artifact_id,
            idx as u64,
        );
        let report = cell_b.import_artifact_package(
            &spec.policies,
            pkg,
            spec.cell_b_import_context,
            key,
            synced_at_ms,
        )?;
        if let PartitionArtifactImportSummary::ConflictIsolated {
            ref conflict_receipt,
            ..
        } = report
        {
            conflict_receipts_on_b.push(conflict_receipt.clone());
        }
        cell_b_imports_from_a.push(report);
    }

    let mut cell_a_imports_from_b = Vec::with_capacity(packages_from_b.len());
    let mut conflict_receipts_on_a = Vec::new();
    for (idx, pkg) in packages_from_b.iter().enumerate() {
        let key = derive_import_key(
            spec.cell_a_import_context.base_idempotency_key,
            pkg.bundle.artifact_id,
            idx as u64,
        );
        let report = cell_a.import_artifact_package(
            &spec.policies,
            pkg,
            spec.cell_a_import_context,
            key,
            synced_at_ms,
        )?;
        if let PartitionArtifactImportSummary::ConflictIsolated {
            ref conflict_receipt,
            ..
        } = report
        {
            conflict_receipts_on_a.push(conflict_receipt.clone());
        }
        cell_a_imports_from_b.push(report);
    }

    // Verify symmetric conflict digest agreement for any artifact isolated on
    // BOTH Cells (`[DIST-ART-001]`).
    let mut canonical_conflict_set = BTreeSet::<[u8; 32]>::new();
    for receipt_a in &conflict_receipts_on_a {
        canonical_conflict_set.insert(*receipt_a.conflict_digest.as_bytes());
        for receipt_b in &conflict_receipts_on_b {
            if receipt_a.artifact_id == receipt_b.artifact_id
                && receipt_a.conflict_digest != receipt_b.conflict_digest
            {
                return Err(PartitionRecoveryError::AsymmetricArtifactConflictDigest {
                    artifact_id: receipt_a.artifact_id,
                    digest_a: *receipt_a.conflict_digest.as_bytes(),
                    digest_b: *receipt_b.conflict_digest.as_bytes(),
                });
            }
        }
    }
    for receipt_b in &conflict_receipts_on_b {
        canonical_conflict_set.insert(*receipt_b.conflict_digest.as_bytes());
    }

    Ok(ArtifactPartitionConvergenceReport {
        cell_a_imports_from_b,
        cell_b_imports_from_a,
        conflict_receipts_on_a,
        conflict_receipts_on_b,
        canonical_conflict_digests: canonical_conflict_set.into_iter().collect(),
    })
}

fn derive_import_key(base: IdempotencyKey, artifact_id: ArtifactId, index: u64) -> IdempotencyKey {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/partition-recovery/artifact-import-key/v1");
    hasher.update(base.as_bytes());
    hasher.update(artifact_id.as_bytes());
    hasher.update(index.to_be_bytes());
    let digest = hasher.finalize();
    let mut out = [0_u8; 16];
    out.copy_from_slice(&digest[..16]);
    IdempotencyKey::from_bytes(out)
}

fn hash_fence(hasher: &mut Sha256, fence: CellFence) {
    hasher.update(fence.identity().as_bytes());
    hasher.update(fence.node_boot_generation().get().to_be_bytes());
    hasher.update(fence.epoch().get().to_be_bytes());
    hasher.update(fence.fencing_token().get().to_be_bytes());
}

fn hash_len(hasher: &mut Sha256, len: usize) {
    hasher.update(u64::try_from(len).unwrap_or(u64::MAX).to_be_bytes());
}

// =============================================================================
// Cross-Process Wire Framing Helpers (used by dual-OS-process Cell workers)
// =============================================================================

/// Encodes a [`SignedSemanticVectorCheckpoint`] into deterministic binary wire
/// bytes for cross-process IPC transport.
#[must_use]
pub fn encode_signed_vector_checkpoint(ckpt: &SignedSemanticVectorCheckpoint) -> Vec<u8> {
    let mut out = Vec::with_capacity(256);
    out.extend_from_slice(VECTOR_CKPT_WIRE_MAGIC);
    out.extend_from_slice(&ckpt.checkpoint_id);
    put_len(&mut out, ckpt.domain_watermarks.len());
    for wm in &ckpt.domain_watermarks {
        out.extend_from_slice(&wm.authority_id);
        out.extend_from_slice(&wm.event_count.to_be_bytes());
        out.extend_from_slice(&wm.event_id_merkle_root);
        out.extend_from_slice(&wm.latest_admission_receipt_digest);
        out.extend_from_slice(&wm.trust_snapshot_digest);
    }
    out.extend_from_slice(&ckpt.covered_event_ids_root);
    out.extend_from_slice(&ckpt.checkpoint_digest);
    out.extend_from_slice(ckpt.signer_principal_id.as_bytes());
    out.extend_from_slice(&ckpt.signer_authority_id);
    out.extend_from_slice(ckpt.signer_key_id.as_bytes());
    out.extend_from_slice(&ckpt.signer_public_key);
    out.extend_from_slice(&ckpt.signed_at_ms.to_be_bytes());
    out.extend_from_slice(&ckpt.signature);
    out
}

/// Decodes a [`SignedSemanticVectorCheckpoint`] from wire bytes encoded by
/// [`encode_signed_vector_checkpoint`].
#[must_use]
pub fn decode_signed_vector_checkpoint(bytes: &[u8]) -> Option<SignedSemanticVectorCheckpoint> {
    let mut cursor = WireReader::new(bytes);
    if cursor.take_fixed::<8>()? != *VECTOR_CKPT_WIRE_MAGIC {
        return None;
    }
    let checkpoint_id = cursor.take_fixed::<16>()?;
    let wm_count = cursor.take_len()?;
    let mut domain_watermarks = Vec::with_capacity(wm_count);
    for _ in 0..wm_count {
        domain_watermarks.push(DomainSemanticWaterMark {
            authority_id: cursor.take_fixed::<16>()?,
            event_count: cursor.take_u64()?,
            event_id_merkle_root: cursor.take_fixed::<32>()?,
            latest_admission_receipt_digest: cursor.take_fixed::<32>()?,
            trust_snapshot_digest: cursor.take_fixed::<32>()?,
        });
    }
    let covered_event_ids_root = cursor.take_fixed::<32>()?;
    let checkpoint_digest = cursor.take_fixed::<32>()?;
    let signer_principal_id = PrincipalId::from_bytes(cursor.take_fixed::<16>()?);
    let signer_authority_id = cursor.take_fixed::<16>()?;
    let signer_key_id = KeyId::from_bytes(cursor.take_fixed::<16>()?);
    let signer_public_key = cursor.take_fixed::<32>()?;
    let signed_at_ms = cursor.take_u64()?;
    let signature = cursor.take_fixed::<64>()?;
    if !cursor.is_empty() {
        return None;
    }
    Some(SignedSemanticVectorCheckpoint {
        checkpoint_id,
        domain_watermarks,
        covered_event_ids_root,
        checkpoint_digest,
        signer_principal_id,
        signer_authority_id,
        signer_key_id,
        signer_public_key,
        signed_at_ms,
        signature,
    })
}

/// Encodes a slice of [`CrossDomainEventItem`]s into deterministic binary wire
/// bytes for cross-process IPC transport.
#[must_use]
pub fn encode_cross_domain_event_items(items: &[CrossDomainEventItem]) -> Vec<u8> {
    let mut out = Vec::with_capacity(128 + items.len() * 384);
    out.extend_from_slice(XDOM_EVENTS_WIRE_MAGIC);
    put_len(&mut out, items.len());
    for item in items {
        let ev = &item.event;
        out.extend_from_slice(ev.event_id.as_bytes());
        put_bytes(&mut out, &ev.canonical_unsigned_event);
        match ev.scope {
            CapabilityTarget::Namespace(ns) => {
                out.push(1);
                out.extend_from_slice(ns.as_bytes());
            }
            CapabilityTarget::Task(tid) => {
                out.push(2);
                out.extend_from_slice(tid.as_bytes());
            }
        }
        out.extend_from_slice(ev.issuer.as_bytes());
        out.extend_from_slice(ev.control_domain.as_bytes());
        out.extend_from_slice(ev.key_id.as_bytes());
        match ev.payload_identity {
            SemanticPayloadIdentity::AssertionContent(d) => {
                out.push(1);
                out.extend_from_slice(&d);
            }
            SemanticPayloadIdentity::IntentSpecBody(d) => {
                out.push(2);
                out.extend_from_slice(&d);
            }
            SemanticPayloadIdentity::Structural => out.push(3),
        }
        out.extend_from_slice(&ev.log_seq.to_be_bytes());
        out.extend_from_slice(&item.producer_signature);

        let adm = &item.admission;
        out.extend_from_slice(adm.receipt_id.as_bytes());
        out.extend_from_slice(adm.event_id.as_bytes());
        out.extend_from_slice(&adm.log_seq.to_be_bytes());
        out.extend_from_slice(&adm.admitted_at_ms.to_be_bytes());
        put_opt_u64(&mut out, adm.effective_valid_until_ms);
        put_len(&mut out, adm.captured_inputs.len());
        for cap_id in &adm.captured_inputs {
            out.extend_from_slice(cap_id.as_bytes());
        }
        out.extend_from_slice(&adm.effective_taint.bits().to_be_bytes());
        out.extend_from_slice(&adm.authz_policy_digest);
        out.extend_from_slice(adm.store_principal.as_bytes());
        out.extend_from_slice(adm.store_control_domain.as_bytes());
        out.extend_from_slice(adm.store_key_id.as_bytes());
        out.extend_from_slice(&adm.store_signature);
        out.extend_from_slice(&item.trust_snapshot_digest);
    }
    out
}

/// Decodes a batch of [`CrossDomainEventItem`]s from wire bytes encoded by
/// [`encode_cross_domain_event_items`].
#[must_use]
pub fn decode_cross_domain_event_items(bytes: &[u8]) -> Option<Vec<CrossDomainEventItem>> {
    let mut cursor = WireReader::new(bytes);
    if cursor.take_fixed::<8>()? != *XDOM_EVENTS_WIRE_MAGIC {
        return None;
    }
    let count = cursor.take_len()?;
    let mut items = Vec::with_capacity(count);
    for _ in 0..count {
        let event_id = SemanticEventId::from_bytes(cursor.take_fixed::<32>()?);
        let canonical_unsigned_event = cursor.take_vec()?;
        let scope = match cursor.take_u8()? {
            1 => CapabilityTarget::Namespace(NamespaceId::from_bytes(cursor.take_fixed::<16>()?)),
            2 => CapabilityTarget::Task(TaskId::from_bytes(cursor.take_fixed::<16>()?)),
            _ => return None,
        };
        let issuer = PrincipalId::from_bytes(cursor.take_fixed::<16>()?);
        let control_domain = ControlDomainId::from_bytes(cursor.take_fixed::<16>()?);
        let key_id = KeyId::from_bytes(cursor.take_fixed::<16>()?);
        let payload_identity = match cursor.take_u8()? {
            1 => SemanticPayloadIdentity::AssertionContent(cursor.take_fixed::<32>()?),
            2 => SemanticPayloadIdentity::IntentSpecBody(cursor.take_fixed::<32>()?),
            3 => SemanticPayloadIdentity::Structural,
            _ => return None,
        };
        let log_seq = cursor.take_u64()?;
        let producer_signature = cursor.take_fixed::<64>()?;

        let receipt_id = ReceiptId::from_bytes(cursor.take_fixed::<16>()?);
        let adm_event_id = SemanticEventId::from_bytes(cursor.take_fixed::<32>()?);
        let adm_log_seq = cursor.take_u64()?;
        let admitted_at_ms = cursor.take_u64()?;
        let effective_valid_until_ms = cursor.take_opt_u64()?;
        let cap_len = cursor.take_len()?;
        let mut captured_inputs = Vec::with_capacity(cap_len);
        for _ in 0..cap_len {
            captured_inputs.push(SemanticEventId::from_bytes(cursor.take_fixed::<32>()?));
        }
        let effective_taint = TaintFlags::from_bits(cursor.take_u64()?)?;
        let authz_policy_digest = cursor.take_fixed::<32>()?;
        let store_principal = PrincipalId::from_bytes(cursor.take_fixed::<16>()?);
        let store_control_domain = ControlDomainId::from_bytes(cursor.take_fixed::<16>()?);
        let store_key_id = KeyId::from_bytes(cursor.take_fixed::<16>()?);
        let store_signature = cursor.take_fixed::<64>()?;
        let trust_snapshot_digest = cursor.take_fixed::<32>()?;

        items.push(CrossDomainEventItem {
            event: SemanticEventRecord {
                event_id,
                canonical_unsigned_event,
                scope,
                issuer,
                control_domain,
                key_id,
                payload_identity,
                log_seq,
            },
            producer_signature,
            admission: AdmissionReceipt {
                receipt_id,
                event_id: adm_event_id,
                log_seq: adm_log_seq,
                admitted_at_ms,
                effective_valid_until_ms,
                captured_inputs,
                effective_taint,
                authz_policy_digest,
                durability: AdmissionDurability::Durable,
                store_principal,
                store_control_domain,
                store_key_id,
                store_signature,
            },
            trust_snapshot_digest,
        });
    }
    if !cursor.is_empty() {
        return None;
    }
    Some(items)
}

/// Encodes an [`ArtifactTransferPackage`] into deterministic binary wire bytes
/// for cross-process IPC transport.
#[must_use]
pub fn encode_artifact_transfer_package(pkg: &ArtifactTransferPackage) -> Vec<u8> {
    let mut out = Vec::with_capacity(256);
    out.extend_from_slice(ART_PKG_WIRE_MAGIC);
    out.extend_from_slice(pkg.bundle.artifact_id.as_bytes());
    put_bytes(&mut out, pkg.bundle.content_type.as_bytes());
    put_head(&mut out, pkg.bundle.remote_head);
    match pkg.bundle.parent_head {
        Some(parent) => {
            out.push(1);
            put_head(&mut out, parent);
        }
        None => out.push(0),
    }
    put_len(&mut out, pkg.bundle.ancestor_lineage.len());
    for anc in &pkg.bundle.ancestor_lineage {
        put_head(&mut out, *anc);
    }
    out.extend_from_slice(&pkg.bundle.origin_cell_id);
    put_bytes(&mut out, &pkg.bundle.bytes);
    out.push(u8::from(pkg.bundle.is_tombstone));

    match pkg.application_id {
        Some(app) => {
            out.push(1);
            out.extend_from_slice(app.as_bytes());
        }
        None => out.push(0),
    }
    match &pkg.owner {
        Some(owner) => {
            out.push(1);
            put_bytes(&mut out, owner.as_bytes());
        }
        None => out.push(0),
    }
    out.extend_from_slice(&pkg.artifact_created_at_ms.to_be_bytes());
    out.extend_from_slice(&pkg.producer_epoch.to_be_bytes());
    match pkg.tombstone {
        Some(meta) => {
            out.push(1);
            out.extend_from_slice(&meta.tombstone_revision.to_be_bytes());
            out.extend_from_slice(&meta.deleted_at_ms.to_be_bytes());
            put_opt_u64(&mut out, meta.retention_until_ms);
        }
        None => out.push(0),
    }
    put_len(&mut out, pkg.revisions.len());
    for rev in &pkg.revisions {
        out.extend_from_slice(&rev.revision.to_be_bytes());
        out.extend_from_slice(rev.digest.as_bytes());
        out.extend_from_slice(&rev.size_bytes.to_be_bytes());
        out.extend_from_slice(&rev.created_at_ms.to_be_bytes());
    }
    put_len(&mut out, pkg.blobs.len());
    for blob in &pkg.blobs {
        out.extend_from_slice(blob.blob_digest.as_bytes());
        put_bytes(&mut out, &blob.bytes);
    }
    out.extend_from_slice(&pkg.package_digest);
    out
}

/// Decodes an [`ArtifactTransferPackage`] from wire bytes encoded by
/// [`encode_artifact_transfer_package`].
#[must_use]
pub fn decode_artifact_transfer_package(bytes: &[u8]) -> Option<ArtifactTransferPackage> {
    let mut cursor = WireReader::new(bytes);
    if cursor.take_fixed::<8>()? != *ART_PKG_WIRE_MAGIC {
        return None;
    }
    let artifact_id = ArtifactId::from_bytes(cursor.take_fixed::<16>()?);
    let content_type = String::from_utf8(cursor.take_vec()?).ok()?;
    let remote_head = cursor.take_head()?;
    let parent_head = match cursor.take_u8()? {
        0 => None,
        1 => Some(cursor.take_head()?),
        _ => return None,
    };
    let anc_len = cursor.take_len()?;
    let mut ancestor_lineage = Vec::with_capacity(anc_len);
    for _ in 0..anc_len {
        ancestor_lineage.push(cursor.take_head()?);
    }
    let origin_cell_id = cursor.take_fixed::<16>()?;
    let tip_bytes = cursor.take_vec()?;
    let is_tombstone = match cursor.take_u8()? {
        0 => false,
        1 => true,
        _ => return None,
    };

    let application_id = match cursor.take_u8()? {
        0 => None,
        1 => Some(ApplicationId::from_bytes(cursor.take_fixed::<16>()?)),
        _ => return None,
    };
    let owner = match cursor.take_u8()? {
        0 => None,
        1 => Some(String::from_utf8(cursor.take_vec()?).ok()?),
        _ => return None,
    };
    let artifact_created_at_ms = cursor.take_u64()?;
    let producer_epoch = cursor.take_u64()?;
    let tombstone = match cursor.take_u8()? {
        0 => None,
        1 => Some(ArtifactTombstoneMetadata {
            tombstone_revision: cursor.take_u64()?,
            deleted_at_ms: cursor.take_u64()?,
            retention_until_ms: cursor.take_opt_u64()?,
        }),
        _ => return None,
    };
    let rev_len = cursor.take_len()?;
    let mut revisions = Vec::with_capacity(rev_len);
    for _ in 0..rev_len {
        revisions.push(TransferRevisionEntry {
            revision: cursor.take_u64()?,
            digest: ContentDigest::from_bytes(cursor.take_fixed::<32>()?),
            size_bytes: cursor.take_u64()?,
            created_at_ms: cursor.take_u64()?,
        });
    }
    let blob_len = cursor.take_len()?;
    let mut blobs = Vec::with_capacity(blob_len);
    for _ in 0..blob_len {
        blobs.push(ArtifactBlobPayload {
            blob_digest: ContentDigest::from_bytes(cursor.take_fixed::<32>()?),
            bytes: cursor.take_vec()?,
        });
    }
    let package_digest = cursor.take_fixed::<32>()?;
    if !cursor.is_empty() {
        return None;
    }

    Some(ArtifactTransferPackage {
        bundle: ReplicaRevisionBundle {
            artifact_id,
            content_type,
            remote_head,
            parent_head,
            ancestor_lineage,
            origin_cell_id,
            bytes: tip_bytes,
            is_tombstone,
        },
        application_id,
        owner,
        artifact_created_at_ms,
        producer_epoch,
        tombstone,
        revisions,
        blobs,
        package_digest,
    })
}

/// Encodes a [`CellFence`] as 40 big-endian bytes.
#[must_use]
pub fn encode_cell_fence(fence: CellFence) -> [u8; 40] {
    let mut out = [0_u8; 40];
    out[..16].copy_from_slice(fence.identity().as_bytes());
    out[16..24].copy_from_slice(&fence.node_boot_generation().get().to_be_bytes());
    out[24..32].copy_from_slice(&fence.epoch().get().to_be_bytes());
    out[32..40].copy_from_slice(&fence.fencing_token().get().to_be_bytes());
    out
}

/// Decodes a [`CellFence`] from 40 big-endian bytes.
#[must_use]
pub fn decode_cell_fence(bytes: &[u8; 40]) -> Option<CellFence> {
    let id_bytes: [u8; 16] = bytes[..16].try_into().ok()?;
    let boot = NonZeroU64::new(u64::from_be_bytes(bytes[16..24].try_into().ok()?))?;
    let epoch = CellEpoch::from_u64(u64::from_be_bytes(bytes[24..32].try_into().ok()?))?;
    let token = CellFencingToken::from_u64(u64::from_be_bytes(bytes[32..40].try_into().ok()?))?;
    Some(CellFence::present(
        CellIdentity::from_domain(nlos_types::SchedulerDomainId::from_bytes(id_bytes)),
        Generation::new(boot),
        epoch,
        token,
    ))
}

fn put_len(out: &mut Vec<u8>, len: usize) {
    out.extend_from_slice(&u64::try_from(len).unwrap_or(u64::MAX).to_be_bytes());
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    put_len(out, bytes.len());
    out.extend_from_slice(bytes);
}

fn put_opt_u64(out: &mut Vec<u8>, value: Option<u64>) {
    match value {
        Some(v) => {
            out.push(1);
            out.extend_from_slice(&v.to_be_bytes());
        }
        None => out.push(0),
    }
}

fn put_head(out: &mut Vec<u8>, head: HeadState) {
    out.extend_from_slice(&head.revision.to_be_bytes());
    out.extend_from_slice(head.digest.as_bytes());
}

struct WireReader<'a> {
    rest: &'a [u8],
}

impl<'a> WireReader<'a> {
    const fn new(rest: &'a [u8]) -> Self {
        Self { rest }
    }

    const fn is_empty(&self) -> bool {
        self.rest.is_empty()
    }

    fn take_u8(&mut self) -> Option<u8> {
        let (&first, tail) = self.rest.split_first()?;
        self.rest = tail;
        Some(first)
    }

    fn take_fixed<const N: usize>(&mut self) -> Option<[u8; N]> {
        if self.rest.len() < N {
            return None;
        }
        let (head, tail) = self.rest.split_at(N);
        self.rest = tail;
        head.try_into().ok()
    }

    fn take_u64(&mut self) -> Option<u64> {
        Some(u64::from_be_bytes(self.take_fixed::<8>()?))
    }

    #[allow(clippy::option_option)] // Outer `None` signals wire framing error; inner `Option<u64>` is the decoded payload field.
    fn take_opt_u64(&mut self) -> Option<Option<u64>> {
        match self.take_u8()? {
            0 => Some(None),
            1 => Some(Some(self.take_u64()?)),
            _ => None,
        }
    }

    fn take_len(&mut self) -> Option<usize> {
        let raw = self.take_u64()?;
        let len = usize::try_from(raw).ok()?;
        if len > self.rest.len() {
            return None;
        }
        Some(len)
    }

    fn take_vec(&mut self) -> Option<Vec<u8>> {
        let len = self.take_len()?;
        let (head, tail) = self.rest.split_at(len);
        self.rest = tail;
        Some(head.to_vec())
    }

    fn take_head(&mut self) -> Option<HeadState> {
        Some(HeadState {
            revision: self.take_u64()?,
            digest: ContentDigest::from_bytes(self.take_fixed::<32>()?),
        })
    }
}
