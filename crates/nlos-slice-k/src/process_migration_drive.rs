//! Cross-Cell [`CellHost`] process migration end-to-end orchestrator (`W64`,
//! `C-MIGRATE-002`): drives all eight steps of [`ProcessMigrationCoordinator`]
//! (`[DIST-MIGRATE-001]`, `[DIST-MIGRATE-002]`) across a federated Source
//! [`CellHost`] and Target [`CellHost`] sharing one [`CellDirectory`] root
//! (`ADR-0018`: one OS process = one Cell), binding real `nlos-lease`
//! close/regrant conservation (`[LEASE-MIGRATE-001]`), signed
//! [`CapabilityAuthority`](nlos_capability::CapabilityAuthority) delegation
//! rebind without authority expansion, [`ArtifactStore`](nlos_artifact::ArtifactStore)
//! checkpoint / restart-image verification (`[DIST-ART-001]`), and Target
//! `ReconciliationCheckpoint` sealing ([`CheckpointDigest::from_prefix`]).
//!
//! # Why Endpoint Traits + Local [`CellHost`] Adapters
//!
//! Per `ADR-0018`, [`CellAuthority`](nlos_cell::CellAuthority) claims a
//! process-unique slot that is never released within an OS process, so a real
//! federated Source [`CellHost`] and Target [`CellHost`] execute in distinct
//! OS processes. [`SourceCellMigrationEndpoint`] and
//! [`TargetCellMigrationEndpoint`] define the transport-neutral step faces
//! between the two Cells (symmetric to [`crate::BarrierObservationSource`] in
//! [`crate::takeover_drive`]), while [`LocalCellHostSourceEndpoint`] and
//! [`LocalCellHostTargetEndpoint`] implement those faces directly against a
//! live [`CellHost`] and its real durable authorities.

use std::error::Error;
use std::fmt;
use std::fmt::Write as _;

use nlos_artifact::sync::{
    ArtifactConflictStrategy, ArtifactDeletionSemantics, ArtifactMergeStrategy,
    ArtifactReplicaAuthority, ArtifactReplicaSyncOutcome, ArtifactSyncPolicy,
    ArtifactSyncPolicyRegistry, ReplicaRevisionBundle, SyncArtifactReplicaRequest,
    export_replica_revision_bundle, sync_artifact_replica_head,
};
use nlos_artifact::{ContentDigest, CreateArtifactSpec, PutRevisionRequest};
use nlos_capability::{
    CapabilityAuthorityError, CapabilityHandle, CapabilityIssueReceipt, CapabilityRecord,
    CapabilityRevocationReceipt, CapabilityTarget, SignedDelegateCapabilityRequest,
    SignedIssueRootCapabilityRequest, SignedRevokeCapabilityRequest,
};
use nlos_cell::{
    CellEpoch, CellFence, CellFencingToken, CellIdentity, CheckpointDigest,
    CheckpointDigestVerification, CheckpointPrefix, CheckpointRecord, Liveness, MigrationIntent,
    MigrationIntentRecord, MigrationObject, MonitoredSubject, NamePath,
};
use nlos_identity::BootstrapPrincipalRequest;
use nlos_lease::{
    CapacityLeaseGrant, CapacityLeaseState, CapacityReconcileError, CapacityReconciliationReceipt,
    CleanupAttestation, DeviceReconciliationReceipt, DeviceResetReceipt, ExclusiveDeviceLeaseGrant,
    ExclusiveDeviceLeaseState, FenceBarrierAck, LeaseInstant, QuotaLeaseGrant, QuotaLeaseState,
};
use nlos_operation::OperationHandle;
use nlos_process::{
    CapabilityRebindProof, CheckpointPayloadKind, CreateIsolationDomainRequest,
    DriverSessionAttachment, FencingToken, InitiateProcessMigrationRequest, LeaseCloseRegrantProof,
    MarkProcessTerminatedRequest, MigratedLeaseEntry, MigrationAbortOutcome, MigrationMode,
    PinnedSessionPolicy, ProcessAuthorityError, ProcessBindingRecord, ProcessCheckpointProof,
    ProcessLifecycleState, ProcessMigrationCoordinator, ProcessMigrationError,
    ProcessMigrationReceipt, ProcessMigrationStep, PropagateCancelToFibersRequest, QuiesceProof,
    ReboundCapabilityEntry, RegisterDelegatedProcessRequest, RegisterFiberIncarnationRequest,
    RegisterSupervisorPidRequest, RestoreProcessRequest, SourceProcessFenceProof,
    TargetRestoreProof,
};
use nlos_runtime::RuntimeAdapter;
use nlos_runtime_tokio::TokioRuntimeAdapter;
use nlos_types::{
    ArtifactId, CancellationScopeId, CapacityLeaseId, CommitPermitId, ExclusiveDeviceLeaseId,
    ExecutionFiberId, Generation, IdempotencyKey, IsolationDomainId, ProcessId, QuotaLeaseId,
    ReceiptId, TaskAttemptId, TaskId,
};
use sha2::{Digest, Sha256};

use crate::cell_host::CellHost;
use crate::error::SliceKError;

/// Prefix tag of canonical single-line [`MigrationIntent`] reasons emitted for
/// cross-Cell process migrations.
pub const PROCESS_MIGRATION_INTENT_TAG: &str = "v1:process-migration:";

/// 8-byte wire magic prefix of canonical [`CheckpointPrefix`] entries sealing
/// a completed [`ProcessMigrationReceipt`] on the Target [`CellHost`].
pub const PROCESS_MIGRATION_RECEIPT_ENTRY_MAGIC: &[u8; 8] = b"PMIGRCPT";

/// Domain separation tag for computing transparent process state checkpoint
/// digests (`[DIST-MIGRATE-001]`).
pub const PROCESS_MIGRATION_CHECKPOINT_STATE_DOMAIN: &[u8] =
    b"llmos/slice-k-process-migration-state/v1";

/// Domain separation tag for computing [`ReboundCapabilityEntry::attenuation_digest`]
/// over verified delegated capability records.
pub const PROCESS_MIGRATION_ATTENUATION_DOMAIN: &[u8] =
    b"llmos/slice-k-process-migration-cap-attenuation/v1";

/// Builds the canonical single-line [`MigrationIntent`] reason string binding
/// `(migration_id, process_id, source_generation, target_generation)`.
#[must_use]
pub fn process_migration_intent_reason(
    migration_id: [u8; 16],
    process_id: ProcessId,
    source_generation: Generation,
    target_generation: Generation,
) -> String {
    let mut out = String::with_capacity(PROCESS_MIGRATION_INTENT_TAG.len() + 96);
    out.push_str(PROCESS_MIGRATION_INTENT_TAG);
    append_hex(&mut out, &migration_id);
    out.push(':');
    append_hex(&mut out, process_id.as_bytes());
    let _ = write!(
        out,
        ":{}->{}",
        source_generation.get(),
        target_generation.get()
    );
    out
}

/// Decoded fields of a canonical `PMIGRCPT` [`CheckpointPrefix`] entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecodedProcessMigrationCheckpointEntry {
    /// Migration correlation identifier (`16` bytes).
    pub migration_id: [u8; 16],
    /// Migrated `ProcessId`.
    pub process_id: ProcessId,
    /// Source `CellIdentity` bytes (`16` bytes).
    pub source_cell_id: [u8; 16],
    /// Target `CellIdentity` bytes (`16` bytes).
    pub target_cell_id: [u8; 16],
    /// Fenced generation on the Source Cell.
    pub source_generation: u64,
    /// Restored generation (`source_generation + 1`) on the Target Cell.
    pub target_generation: u64,
    /// `0` for [`MigrationMode::TransparentCheckpointRestore`], `1` for
    /// [`MigrationMode::DowngradedRestartFromArtifact`].
    pub mode_code: u8,
    /// Sealed 32-byte [`ProcessMigrationReceipt::receipt_digest`].
    pub receipt_digest: [u8; 32],
}

/// Encodes a [`ProcessMigrationReceipt`] into its canonical 121-byte
/// `PMIGRCPT` [`CheckpointPrefix`] entry.
#[must_use]
pub fn process_migration_receipt_checkpoint_entry(receipt: &ProcessMigrationReceipt) -> Vec<u8> {
    let mode_code = match receipt.migration_mode {
        MigrationMode::TransparentCheckpointRestore => 0u8,
        MigrationMode::DowngradedRestartFromArtifact { .. } => 1u8,
    };
    let mut out = Vec::with_capacity(121);
    out.extend_from_slice(PROCESS_MIGRATION_RECEIPT_ENTRY_MAGIC);
    out.extend_from_slice(&receipt.migration_id);
    out.extend_from_slice(receipt.process_id.as_bytes());
    out.extend_from_slice(&receipt.source_cell_id);
    out.extend_from_slice(&receipt.target_cell_id);
    out.extend_from_slice(&receipt.source_generation.to_be_bytes());
    out.extend_from_slice(&receipt.target_generation.to_be_bytes());
    out.push(mode_code);
    out.extend_from_slice(&receipt.receipt_digest);
    out
}

/// Strict inverse of [`process_migration_receipt_checkpoint_entry`].
#[must_use]
pub fn decode_process_migration_receipt_checkpoint_entry(
    bytes: &[u8],
) -> Option<DecodedProcessMigrationCheckpointEntry> {
    if bytes.len() != 121 || &bytes[..8] != PROCESS_MIGRATION_RECEIPT_ENTRY_MAGIC {
        return None;
    }
    let migration_id: [u8; 16] = bytes[8..24].try_into().ok()?;
    let process_bytes: [u8; 16] = bytes[24..40].try_into().ok()?;
    let source_cell_id: [u8; 16] = bytes[40..56].try_into().ok()?;
    let target_cell_id: [u8; 16] = bytes[56..72].try_into().ok()?;
    let source_generation = u64::from_be_bytes(bytes[72..80].try_into().ok()?);
    let target_generation = u64::from_be_bytes(bytes[80..88].try_into().ok()?);
    let mode_code = bytes[88];
    if mode_code > 1 || source_generation == 0 || target_generation != source_generation + 1 {
        return None;
    }
    let receipt_digest: [u8; 32] = bytes[89..121].try_into().ok()?;
    Some(DecodedProcessMigrationCheckpointEntry {
        migration_id,
        process_id: ProcessId::from_bytes(process_bytes),
        source_cell_id,
        target_cell_id,
        source_generation,
        target_generation,
        mode_code,
        receipt_digest,
    })
}

/// Builds the canonical single-entry [`CheckpointPrefix`] committing a
/// [`ProcessMigrationReceipt`].
#[must_use]
pub fn process_migration_receipt_checkpoint_prefix(
    receipt: &ProcessMigrationReceipt,
) -> CheckpointPrefix {
    CheckpointPrefix::from_entries([process_migration_receipt_checkpoint_entry(receipt)])
}

/// Computes the deterministic 32-byte state checkpoint digest for
/// [`MigrationMode::TransparentCheckpointRestore`].
#[must_use]
pub fn compute_transparent_checkpoint_digest(
    migration_id: [u8; 16],
    process_id: ProcessId,
    source_generation: Generation,
    source_fencing_token: FencingToken,
    state_payload: &[u8],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(PROCESS_MIGRATION_CHECKPOINT_STATE_DOMAIN);
    hasher.update(migration_id);
    hasher.update(process_id.as_bytes());
    hasher.update(source_generation.get().to_be_bytes());
    hasher.update(source_fencing_token);
    hasher.update((state_payload.len() as u64).to_be_bytes());
    hasher.update(state_payload);
    hasher.finalize().into()
}

/// Computes the domain-separated 32-byte attenuation digest committing a
/// delegated [`CapabilityRecord`] rebound for `(target_cell_id, target_generation)`.
#[must_use]
pub fn compute_rebound_capability_attenuation_digest(
    record: &CapabilityRecord,
    source_cell_id: [u8; 16],
    source_generation: u64,
    target_cell_id: [u8; 16],
    target_generation: u64,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(PROCESS_MIGRATION_ATTENUATION_DOMAIN);
    hasher.update(record.handle.capability_id.as_bytes());
    hasher.update(record.handle.generation.get().to_be_bytes());
    hasher.update(source_cell_id);
    hasher.update(source_generation.to_be_bytes());
    hasher.update(target_cell_id);
    hasher.update(target_generation.to_be_bytes());
    let (target_kind, target_bytes) = match record.target {
        CapabilityTarget::Namespace(id) => (1u8, id.into_bytes()),
        CapabilityTarget::Task(id) => (2u8, id.into_bytes()),
    };
    hasher.update([target_kind]);
    hasher.update(target_bytes);
    hasher.update(record.rights.bits().to_be_bytes());
    match record.purpose_digest {
        Some(digest) => {
            hasher.update([1u8]);
            hasher.update(digest);
        }
        None => hasher.update([0u8]),
    }
    hasher.update(record.valid_from_ms.to_be_bytes());
    hasher.update(record.valid_until_ms.to_be_bytes());
    hasher.update([record.delegation_depth_remaining]);
    match record.call_limit {
        Some(limit) => {
            hasher.update([1u8]);
            hasher.update(limit.to_be_bytes());
        }
        None => hasher.update([0u8]),
    }
    hasher.finalize().into()
}

/// Genesis parameters that deterministically derive a `ProcessId`,
/// `AgentInstanceId`, and generation-1 `FencingToken` inside
/// [`nlos_process::ProcessAuthority::register_delegated_process`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessLineageGenesis {
    /// Owning `TaskId`.
    pub task_id: TaskId,
    /// Owning `TaskAttemptId`.
    pub task_attempt_id: TaskAttemptId,
    /// Attempt generation bound at process registration.
    pub attempt_generation: Generation,
    /// Genesis registration `IdempotencyKey` (`seeded_key(seed, 113)` for
    /// [`CellHost::spawn_monitored_process`]).
    pub idempotency_key: IdempotencyKey,
    /// Creation timestamp recorded on the genesis binding.
    pub created_at_ms: u64,
}

impl ProcessLineageGenesis {
    /// Builds the genesis lineage descriptor matching a binding produced by
    /// [`CellHost::spawn_monitored_process`] for `seed`.
    #[must_use]
    pub fn from_spawned_binding(seed: u8, binding: &ProcessBindingRecord) -> Self {
        Self {
            task_id: binding.task_id,
            task_attempt_id: binding.task_attempt_id,
            attempt_generation: binding.attempt_generation,
            idempotency_key: crate::seeded_key(seed, 113),
            created_at_ms: binding.created_at_ms,
        }
    }
}

/// One prior [`nlos_process::ProcessAuthority::restore_process`] hop in a
/// multi-generation process lineage (`1 -> 2`, `2 -> 3`, ...).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessLineageRestoreHop {
    /// Prior generation (`N`) expected before this hop.
    pub expected_generation: Generation,
    /// Prior 32-byte `FencingToken` expected at `expected_generation`.
    pub expected_fencing_token: FencingToken,
    /// Idempotency key that derived generation `N + 1` and its fencing token.
    pub idempotency_key: IdempotencyKey,
    /// Timestamp recorded on the generation `N + 1` binding.
    pub restored_at_ms: u64,
}

/// Complete deterministic lineage prefix allowing a Target [`CellHost`]'s
/// [`nlos_process::ProcessAuthority`] to verify and advance a `ProcessId` from
/// `source_generation` to `target_generation = source_generation + 1`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessLineageSpec {
    /// Generation-1 registration parameters.
    pub genesis: ProcessLineageGenesis,
    /// Ordered restore hops from generation `1` up to `source_generation`.
    pub prior_restores: Vec<ProcessLineageRestoreHop>,
}

/// Snapshot of the Source [`CellHost`]'s federation registration and active
/// process placement inspected during Step 1 (`Initiated`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceCellPlacementSnapshot {
    /// Source `CellIdentity`.
    pub cell_id: CellIdentity,
    /// Source Cell's `node_boot_generation`.
    pub node_boot_generation: Generation,
    /// Source Cell's current [`CellEpoch`].
    pub epoch: CellEpoch,
    /// Source Cell's current [`CellFencingToken`].
    pub fencing_token: CellFencingToken,
    /// Whether both Source and Target Cells are visible in Source's [`CellDirectory`].
    pub both_cells_registered_in_directory: bool,
    /// Active [`ProcessBindingRecord`] on Source before migration.
    pub active_binding: ProcessBindingRecord,
    /// Whether `(process_id, source_generation)` is registered in Source's
    /// [`nlos_process::SupervisorPidRegistry`].
    pub supervisor_registered: bool,
}

/// Snapshot of the Target [`CellHost`]'s federation registration inspected
/// during Step 1 (`Initiated`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TargetCellPlacementSnapshot {
    /// Target `CellIdentity`.
    pub cell_id: CellIdentity,
    /// Target Cell's current [`CellFence`].
    pub fence: CellFence,
    /// Whether both Source and Target Cells are visible in Target's [`CellDirectory`].
    pub both_cells_registered_in_directory: bool,
}

/// Step 2 (`Quiesced`) request executed on the Source [`CellHost`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceQuiesceRequest {
    /// Migrating `ProcessId`.
    pub process_id: ProcessId,
    /// Source `CellIdentity`.
    pub source_cell_id: CellIdentity,
    /// Source `Generation`.
    pub source_generation: Generation,
    /// Optional structured cancellation scope `(scope_id, generation)` to
    /// cancel on the Source runtime adapter during quiesce.
    pub cancel_scope: Option<(CancellationScopeId, Generation)>,
    /// Count of runtime fibers owned by `(process_id, source_generation)` to
    /// account in [`QuiesceProof::quiesced_fibers`] (in addition to any scope
    /// cancellation).
    pub active_fiber_count: u32,
    /// In-flight driver operations `(handle, no_effect_receipt_id)` to cancel
    /// on the Source `SqliteOperationStore` during quiesce.
    pub inflight_operations: Vec<(OperationHandle, ReceiptId)>,
    /// Authority timestamp for [`QuiesceProof`].
    pub quiesced_at_ms: u64,
}

/// Checkpoint or restart-image specification for Step 3 (`Checkpointed`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CheckpointSourceSpec {
    /// Full state checkpoint for [`MigrationMode::TransparentCheckpointRestore`].
    TransparentState {
        /// Serialized process state bytes committed into the checkpoint digest.
        state_payload: Vec<u8>,
        /// Optional `ArtifactStore` cache key (`cache/` domain) to persist on
        /// Source and verify byte-identically on Target.
        cache_key: Option<String>,
        /// Timestamp recorded in [`ProcessCheckpointProof`].
        checkpointed_at_ms: u64,
    },
    /// Real [`nlos_artifact::ArtifactStore`] image specification for
    /// [`MigrationMode::DowngradedRestartFromArtifact`] (`[DIST-MIGRATE-002]`).
    ArtifactRestartImage {
        /// `ArtifactId` holding the restart image.
        artifact_id: ArtifactId,
        /// 1-based committed revision number holding the restart image.
        revision: u64,
        /// When `true`, exports a verified [`ReplicaRevisionBundle`] from
        /// Source's `ArtifactStore` and syncs it into Target's `ArtifactStore`
        /// under `[DIST-ART-001]` if Target does not already hold `(artifact_id, revision)`.
        sync_from_source_if_missing: bool,
        /// Observation timestamp used for `ArtifactStore` retention-gated readback
        /// and [`ProcessCheckpointProof::checkpointed_at_ms`].
        checkpointed_at_ms: u64,
    },
}

/// Source-side Step 3 (`Checkpointed`) capture request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceCheckpointRequest {
    /// Migration correlation ID.
    pub migration_id: [u8; 16],
    /// Migrating `ProcessId`.
    pub process_id: ProcessId,
    /// Source `CellIdentity`.
    pub source_cell_id: CellIdentity,
    /// Source `Generation`.
    pub source_generation: Generation,
    /// Resolved [`MigrationMode`] from Step 1.
    pub migration_mode: MigrationMode,
    /// Admitted migratable driver session digests from Step 1.
    pub migratable_sessions: Vec<([u8; 16], [u8; 32])>,
    /// Checkpoint or artifact-image source specification.
    pub spec: CheckpointSourceSpec,
}

/// Output captured on Source during Step 3 (`Checkpointed`), handed to Target
/// for cache/artifact verification before recording [`ProcessCheckpointProof`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceCheckpointBundle {
    /// Proof ready to record in [`ProcessMigrationCoordinator::record_checkpoint`]
    /// once Target verifies any cache or artifact-image payload.
    pub proof: ProcessCheckpointProof,
    /// Optional cache `(cache_key, payload)` to materialize and verify on Target.
    pub transparent_cache_transfer: Option<(String, Vec<u8>, u64)>,
    /// Optional `[DIST-ART-001]` [`ReplicaRevisionBundle`] exported from Source
    /// when `sync_from_source_if_missing` is enabled for `DowngradedRestartFromArtifact`.
    pub exported_artifact_bundle: Option<ReplicaRevisionBundle>,
}

/// Target-side Step 3 verification request for [`SourceCheckpointBundle`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TargetCheckpointVerifyRequest {
    /// Resolved [`MigrationMode`].
    pub migration_mode: MigrationMode,
    /// Checkpoint source specification.
    pub spec: CheckpointSourceSpec,
    /// Bundle produced by Source.
    pub source_bundle: SourceCheckpointBundle,
    /// Deterministic idempotency seed for `[DIST-ART-001]` replica staging when
    /// syncing an exported [`ReplicaRevisionBundle`] onto Target.
    pub sync_seed: u8,
}

/// Step 4 (`SourceFenced`) request executed on the Source [`CellHost`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceFenceRequest {
    /// Migrating `ProcessId`.
    pub process_id: ProcessId,
    /// Source `CellIdentity`.
    pub source_cell_id: CellIdentity,
    /// Source `Generation` to fence and unregister.
    pub source_generation: Generation,
    /// Cached [`NamePath`] entries bound to the migrating process to invalidate
    /// through `source_generation` on Source's `CapabilityNameCache`.
    pub invalidate_name_paths: Vec<NamePath>,
    /// Idempotency key for [`nlos_process::ProcessAuthority::mark_process_terminated`]
    /// and fiber cancel propagation.
    pub terminate_idempotency_key: IdempotencyKey,
    /// Probe fiber ID used to verify that stale-generation fiber registration /
    /// dispatch on Source fails closed immediately after fencing.
    pub stale_dispatch_probe_fiber_id: ExecutionFiberId,
    /// Probe idempotency key used for the stale-generation dispatch refusal check.
    pub stale_dispatch_probe_key: IdempotencyKey,
    /// Authority timestamp recorded on the terminal marker and [`SourceProcessFenceProof`].
    pub fenced_at_ms: u64,
}

/// Verified outcome of Step 4 (`SourceFenced`) on the Source [`CellHost`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceFenceReport {
    /// Step 4 proof for [`ProcessMigrationCoordinator::record_source_fence`].
    pub proof: SourceProcessFenceProof,
    /// Confirmed `true`: stale-generation heartbeat (`record_process_heartbeat`
    /// and `record_fenced_process_heartbeat`) was refused fail-closed on Source.
    pub stale_heartbeat_refused: bool,
    /// Confirmed `true`: stale-generation fiber registration/dispatch was
    /// refused fail-closed on Source (`ProcessBindingTerminal`).
    pub stale_dispatch_refused: bool,
    /// Number of `CapabilityNameCache` paths invalidated through `source_generation`.
    pub invalidated_name_paths: usize,
}

/// One capability delegation & name-cache rebind specification for Step 5
/// (`CapabilitiesRebound`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilityRebindItemSpec {
    /// Optional principal bootstrap requests to replay idempotently on Target's
    /// `IdentityAuthority` before verifying capability command signatures.
    pub bootstrap_principals: Vec<BootstrapPrincipalRequest>,
    /// Optional signed root capability issuance to replay idempotently on
    /// Target's `CapabilityAuthority` so the parent handle exists locally prior
    /// to attenuated delegation.
    pub seed_parent_root: Option<SignedIssueRootCapabilityRequest>,
    /// Signed attenuated or equivalent delegation command executed via
    /// [`nlos_capability::CapabilityAuthority::delegate_signed`] on Target's
    /// `CapabilityAuthority` (enforcing zero rights, scope, purpose, validity,
    /// call-limit, or depth amplification).
    pub signed_delegation: SignedDelegateCapabilityRequest,
    /// Optional Target `CapabilityNameCache` path under which the rebound
    /// `CapabilityId` is cached at `target_generation`.
    pub target_name_path: Option<NamePath>,
    /// Optional signed revocation of the old capability handle on Source's
    /// `CapabilityAuthority`.
    pub revoke_on_source: Option<SignedRevokeCapabilityRequest>,
}

/// Target-side Step 5 (`CapabilitiesRebound`) request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TargetCapabilityRebindRequest {
    /// Migrating `ProcessId`.
    pub process_id: ProcessId,
    /// Source `CellIdentity`.
    pub source_cell_id: CellIdentity,
    /// Source `Generation`.
    pub source_generation: Generation,
    /// Target `CellIdentity`.
    pub target_cell_id: CellIdentity,
    /// Target `Generation` (`source_generation + 1`).
    pub target_generation: Generation,
    /// Whether Target should advance its `CellEpoch` before caching rebound
    /// names so a subsequent `ExclusiveDeviceLease` migration satisfies
    /// `target_epoch > source_exclusivity_epoch` (`[LEASE-DEVICE-001]`) without
    /// fencing the newly inserted Target name-cache entries.
    pub ensure_target_epoch_above: Option<CellEpoch>,
    /// Capabilities to rebind onto `(target_cell_id, target_generation)`.
    pub items: Vec<CapabilityRebindItemSpec>,
    /// Timestamp recorded in [`CapabilityRebindProof`].
    pub rebound_at_ms: u64,
}

/// Verified outcome of Step 5 (`CapabilitiesRebound`) on Target [`CellHost`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TargetCapabilityRebindReport {
    /// Step 5 proof for [`ProcessMigrationCoordinator::record_capability_rebind`].
    pub proof: CapabilityRebindProof,
    /// Delegated [`CapabilityRecord`]s committed on Target's `CapabilityAuthority`.
    pub delegated_records: Vec<(CapabilityRecord, CapabilityIssueReceipt)>,
    /// Target's `CellFence` after any pre-regrant epoch bump.
    pub target_fence: CellFence,
}

/// `QuotaLease` migration specification (`[LEASE-MIGRATE-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QuotaLeaseMigrationSpec {
    /// Active `QuotaLeaseId` on Source [`CellHost`].
    pub source_lease_id: QuotaLeaseId,
    /// Enforcement gateway identifier bound into [`FenceBarrierAck::for_quota`].
    pub gateway_id: [u8; 16],
    /// Optional explicit override for Source's [`FenceBarrierAck`] (when `None`,
    /// Source constructs [`FenceBarrierAck::for_quota`] over the drained lease;
    /// `Some` allows testing fail-closed rejection of tampered barrier ACKs).
    pub override_fence_ack: Option<FenceBarrierAck>,
    /// Fresh `QuotaLeaseId` to grant on Target [`CellHost`] (`!= source_lease_id`).
    pub target_lease_id: QuotaLeaseId,
    /// Requested face value on Target (`None` transfers the full unspent
    /// `returned` remainder of Source's closed lease; `Some(v)` requires
    /// `v <= returned`).
    pub target_face_value: Option<u64>,
    /// Optional TTL deadline on Target's re-granted `QuotaLease`.
    pub target_expires_at: Option<LeaseInstant>,
    /// When `true`, transitions Target's newly granted `QuotaLease` from
    /// `Issued` to `Active`.
    pub activate_on_target: bool,
}

/// Optional `CapacityLease` migration specification (`[LEASE-CAPACITY-002]`,
/// `[LEASE-MIGRATE-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapacityLeaseMigrationSpec {
    /// Source `CapacityLeaseId` to fence and reconcile to `RETURNED`.
    pub source_lease_id: CapacityLeaseId,
    /// Idempotent `ReceiptId` for Source's [`CapacityReconciliationReceipt`].
    pub reconcile_receipt_id: ReceiptId,
    /// Enforcement gateway identifier bound into [`FenceBarrierAck::for_capacity`].
    pub gateway_id: [u8; 16],
    /// Fresh `CapacityLeaseId` to grant on Target (`!= source_lease_id`).
    pub target_lease_id: CapacityLeaseId,
    /// Requested capacity on Target (`None` transfers full `source.amount()`;
    /// `Some(a)` requires `a <= source.amount()`).
    pub target_amount: Option<u64>,
}

/// Optional `ExclusiveDeviceLease` migration specification (`[LEASE-DEVICE-001]`,
/// `[LEASE-MIGRATE-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceLeaseMigrationSpec {
    /// Source `ExclusiveDeviceLeaseId` to fence, reset/zeroize, and reconcile
    /// to `RETURNED` (`device_head_free() == true`).
    pub source_lease_id: ExclusiveDeviceLeaseId,
    /// Idempotent `ReceiptId` for Source's [`DeviceResetReceipt`].
    pub reset_receipt_id: ReceiptId,
    /// Idempotent `ReceiptId` for Source's [`DeviceReconciliationReceipt`].
    pub reconcile_receipt_id: ReceiptId,
    /// Enforcement gateway identifier bound into [`FenceBarrierAck::for_device`].
    pub gateway_id: [u8; 16],
    /// Fresh `ExclusiveDeviceLeaseId` to grant on Target (`!= source_lease_id`).
    pub target_lease_id: ExclusiveDeviceLeaseId,
    /// When `true`, automatically advances Target's `CellEpoch` if needed so
    /// `target_exclusivity_epoch > source_exclusivity_epoch` (`[LEASE-DEVICE-001]`).
    pub advance_target_epoch_if_needed: bool,
}

/// Complete multi-family lease migration specification for Step 6 (`LeasesRegranted`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LeaseMigrationBundleSpec {
    /// Mandatory `QuotaLease` close/regrant specification.
    pub quota: QuotaLeaseMigrationSpec,
    /// Optional `CapacityLease` fence/reconcile/regrant specification.
    pub capacity: Option<CapacityLeaseMigrationSpec>,
    /// Optional `ExclusiveDeviceLease` fence/reset/reconcile/regrant specification.
    pub device: Option<DeviceLeaseMigrationSpec>,
    /// Timestamp recorded in [`LeaseCloseRegrantProof`].
    pub regranted_at_ms: u64,
}

/// Transport-constructible summary of a closed Source `QuotaLease`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceClosedQuotaSummary {
    /// Closed `QuotaLeaseId`.
    pub lease_id: QuotaLeaseId,
    /// Committed `QuotaLeaseState` (`Closed`).
    pub state: QuotaLeaseState,
    /// Issued `face_value`.
    pub face_value: u64,
    /// Settled `spent` high-water.
    pub spent: u64,
    /// Unspent `returned` remainder refunded on Source.
    pub returned: u64,
    /// Verified `FenceBarrierAck` digest (`32` bytes).
    pub close_digest: [u8; 32],
}

/// Transport-constructible summary of a reconciled/returned Source `CapacityLease`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceReturnedCapacitySummary {
    /// Returned `CapacityLeaseId`.
    pub capacity_lease_id: CapacityLeaseId,
    /// Committed `CapacityLeaseState` (`Returned`).
    pub state: CapacityLeaseState,
    /// Reconciled `amount` returned to Source's pool.
    pub amount: u64,
    /// Verified `FenceBarrierAck` digest (`32` bytes).
    pub close_digest: [u8; 32],
}

/// Transport-constructible summary of a reconciled/returned Source
/// `ExclusiveDeviceLease`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceReturnedDeviceSummary {
    /// Returned `ExclusiveDeviceLeaseId`.
    pub device_lease_id: ExclusiveDeviceLeaseId,
    /// Committed `ExclusiveDeviceLeaseState` (`Returned`).
    pub state: ExclusiveDeviceLeaseState,
    /// Source `exclusivity_epoch` (`Target` must grant at strictly higher epoch).
    pub exclusivity_epoch: CellEpoch,
    /// Source `DeviceLeaseHead` `reset_generation` after reconciliation.
    pub head_reset_generation: Generation,
    /// Verified `FenceBarrierAck` digest (`32` bytes).
    pub close_digest: [u8; 32],
}

/// Source-side Step 6 close/fence outcome proving all Source leases reached
/// `CLOSED` / `RETURNED` before Target grants anything (`[LEASE-MIGRATE-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceLeaseCloseReport {
    /// Closed Source `QuotaLease` summary.
    pub closed_quota: SourceClosedQuotaSummary,
    /// Optional reconciled Source `CapacityLease` summary.
    pub returned_capacity: Option<SourceReturnedCapacitySummary>,
    /// Optional reconciled Source `ExclusiveDeviceLease` summary.
    pub returned_device: Option<SourceReturnedDeviceSummary>,
}

/// Target-side Step 6 regrant request, executed strictly after
/// [`SourceLeaseCloseReport`] completes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TargetLeaseRegrantRequest {
    /// Migrating `ProcessId`.
    pub process_id: ProcessId,
    /// Source `CellIdentity`.
    pub source_cell_id: CellIdentity,
    /// Source `Generation`.
    pub source_generation: Generation,
    /// Target `CellIdentity`.
    pub target_cell_id: CellIdentity,
    /// Target `Generation` (`source_generation + 1`).
    pub target_generation: Generation,
    /// Lease migration specification.
    pub spec: LeaseMigrationBundleSpec,
    /// Verified Source close/return report.
    pub source_closed: SourceLeaseCloseReport,
}

/// Target-side Step 6 regrant outcome.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TargetLeaseRegrantReport {
    /// Step 6 proof for [`ProcessMigrationCoordinator::record_lease_close_regrant`].
    pub proof: LeaseCloseRegrantProof,
    /// Newly granted (and optionally activated) Target `QuotaLeaseGrant`.
    pub target_quota: QuotaLeaseGrant,
    /// Optional newly granted Target `CapacityLeaseGrant`.
    pub target_capacity: Option<CapacityLeaseGrant>,
    /// Optional newly granted Target `ExclusiveDeviceLeaseGrant`.
    pub target_device: Option<ExclusiveDeviceLeaseGrant>,
}

/// Post-fence quarantine report produced on Source [`CellHost`] when migration
/// aborts after Step 4 (`SourceFenced`) and before Step 7 (`TargetRestored`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourcePostFenceQuarantineReport {
    /// Source `QuotaLeaseState` after post-fence quarantine (`Quarantined` if it
    /// was still unsettled, `Closed` if Step 6 source close had already settled it).
    pub quota_state: QuotaLeaseState,
    /// Optional Source `CapacityLeaseState` after post-fence quarantine.
    pub capacity_state: Option<CapacityLeaseState>,
    /// Optional Source `ExclusiveDeviceLeaseState` after post-fence quarantine.
    pub device_state: Option<ExclusiveDeviceLeaseState>,
}

/// Target-side Step 7 (`TargetRestored`) request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TargetProcessRestoreRequest {
    /// Migrating `ProcessId`.
    pub process_id: ProcessId,
    /// Target `CellIdentity`.
    pub target_cell_id: CellIdentity,
    /// Expected fenced generation (`source_generation`).
    pub source_generation: Generation,
    /// Expected 32-byte `FencingToken` of `source_generation` on Source.
    pub source_fencing_token: FencingToken,
    /// Expected restored generation (`source_generation + 1`).
    pub target_generation: Generation,
    /// Lineage prefix allowing Target's `ProcessAuthority` to verify and advance
    /// `process_id` to `target_generation`.
    pub lineage: ProcessLineageSpec,
    /// Optional existing active `IsolationDomainId` on Target; when `None`,
    /// Target creates or replays a dedicated `IsolationDomain` using
    /// `target_domain_policy_digest` and `target_domain_idempotency_key`.
    pub existing_target_domain_id: Option<IsolationDomainId>,
    /// Policy digest for creating/replaying Target's `IsolationDomain` when
    /// `existing_target_domain_id` is `None`.
    pub target_domain_policy_digest: [u8; 32],
    /// Idempotency key for creating/replaying Target's `IsolationDomain`.
    pub target_domain_idempotency_key: IdempotencyKey,
    /// Idempotency key for [`nlos_process::ProcessAuthority::restore_process`]
    /// at `target_generation`.
    pub restore_idempotency_key: IdempotencyKey,
    /// Primary 32-byte checkpoint or artifact image digest verified in Step 3.
    pub restored_image_or_checkpoint_digest: [u8; 32],
    /// Timestamp recorded on the restored binding and supervisor registration.
    pub restored_at_ms: u64,
}

/// Verified outcome of Step 7 (`TargetRestored`) on Target [`CellHost`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TargetProcessRestoreReport {
    /// Authoritative [`ProcessBindingRecord`] at `target_generation = source_generation + 1`.
    pub restored_binding: ProcessBindingRecord,
    /// Step 7 proof for [`ProcessMigrationCoordinator::record_target_restore`].
    pub proof: TargetRestoreProof,
    /// Target OS PID registered in Target's [`nlos_process::SupervisorPidRegistry`].
    pub registered_target_os_pid: u32,
}

/// Verified outcome of Step 8 (`Completed`) federation checkpoint recording on
/// Target [`CellHost`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TargetReceiptCheckpointReport {
    /// Canonical [`CheckpointPrefix`] committing the [`ProcessMigrationReceipt`].
    pub checkpoint_prefix: CheckpointPrefix,
    /// Structured [`CheckpointDigest`] (`v1:sha256:...`) over Target's fence and
    /// `checkpoint_prefix`.
    pub checkpoint_digest: CheckpointDigest,
    /// Recorded immutable [`CheckpointRecord`] in the shared [`CellDirectory`].
    pub checkpoint_record: CheckpointRecord,
}

/// Top-level end-to-end request for [`drive_cell_host_process_migration`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CellHostProcessMigrationRequest {
    /// Unique 16-byte migration identifier.
    pub migration_id: [u8; 16],
    /// `ProcessId` to migrate from `source_cell_id` to `target_cell_id`.
    pub process_id: ProcessId,
    /// Federated Source `CellIdentity`.
    pub source_cell_id: CellIdentity,
    /// Federated Target `CellIdentity`.
    pub target_cell_id: CellIdentity,
    /// Current active generation on Source (`>= 1`).
    pub source_generation: Generation,
    /// Process lineage descriptor (`genesis` + any prior restore hops).
    pub lineage: ProcessLineageSpec,
    /// Attached driver sessions evaluated under `[DIST-MIGRATE-002]`.
    pub attached_sessions: Vec<DriverSessionAttachment>,
    /// Policy governing `PinnedNonMigratable` driver sessions (`[DIST-MIGRATE-002]`).
    pub pinned_session_policy: PinnedSessionPolicy,
    /// Optional custom single-line reason for [`MigrationIntent`] (defaults to
    /// [`process_migration_intent_reason`] when `None`).
    pub custom_intent_reason: Option<String>,
    /// Timestamp for Step 1 (`Initiated`).
    pub initiated_at_ms: u64,
    /// Step 2 quiesce parameters (`cancel_scope`, `active_fiber_count`,
    /// `inflight_operations`, `quiesced_at_ms`).
    pub quiesce: SourceQuiesceSpec,
    /// Step 3 checkpoint or artifact restart image specification.
    pub checkpoint: CheckpointSourceSpec,
    /// Deterministic seed used if Step 3 syncs an artifact bundle to Target.
    pub artifact_sync_seed: u8,
    /// Step 4 source fence parameters.
    pub source_fence: SourceFenceSpec,
    /// Step 5 capability rebind items and timestamp.
    pub capabilities: Vec<CapabilityRebindItemSpec>,
    /// Timestamp for Step 5 (`CapabilitiesRebound`).
    pub capabilities_rebound_at_ms: u64,
    /// Step 6 multi-family lease close/regrant specification.
    pub leases: LeaseMigrationBundleSpec,
    /// Step 7 target restore parameters.
    pub target_restore: TargetRestoreSpec,
    /// Timestamp for Step 8 (`Completed`) and post-fence abort fallback.
    pub completed_at_ms: u64,
}

/// Caller-supplied Step 2 quiesce fields inside [`CellHostProcessMigrationRequest`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceQuiesceSpec {
    /// Optional cancellation scope `(scope_id, generation)` to cancel on Source.
    pub cancel_scope: Option<(CancellationScopeId, Generation)>,
    /// Active runtime fibers drained/cancelled on Source.
    pub active_fiber_count: u32,
    /// In-flight driver operations `(handle, no_effect_receipt_id)` to cancel.
    pub inflight_operations: Vec<(OperationHandle, ReceiptId)>,
    /// Quiesce timestamp.
    pub quiesced_at_ms: u64,
}

/// Caller-supplied Step 4 source fence fields inside [`CellHostProcessMigrationRequest`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceFenceSpec {
    /// `CapabilityNameCache` paths on Source to invalidate through `source_generation`.
    pub invalidate_name_paths: Vec<NamePath>,
    /// Idempotency key for marking `(process_id, source_generation)` terminated.
    pub terminate_idempotency_key: IdempotencyKey,
    /// Non-zero probe `ExecutionFiberId` used to prove stale-generation dispatch refusal.
    pub stale_dispatch_probe_fiber_id: ExecutionFiberId,
    /// Idempotency key for the stale-generation dispatch probe.
    pub stale_dispatch_probe_key: IdempotencyKey,
    /// Fence timestamp.
    pub fenced_at_ms: u64,
}

/// Caller-supplied Step 7 target restore fields inside [`CellHostProcessMigrationRequest`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TargetRestoreSpec {
    /// Optional pre-existing active `IsolationDomainId` on Target (`None`
    /// creates/replays one using `domain_policy_digest` + `domain_idempotency_key`).
    pub existing_target_domain_id: Option<IsolationDomainId>,
    /// Policy digest for Target's `IsolationDomain`.
    pub domain_policy_digest: [u8; 32],
    /// Idempotency key for Target's `IsolationDomain`.
    pub domain_idempotency_key: IdempotencyKey,
    /// Idempotency key for `ProcessAuthority::restore_process` on Target.
    pub restore_idempotency_key: IdempotencyKey,
    /// Restore timestamp.
    pub restored_at_ms: u64,
}

/// Complete end-to-end outcome of [`drive_cell_host_process_migration`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CellHostProcessMigrationOutcome {
    /// Immutable [`MigrationIntentRecord`] appended to the shared [`CellDirectory`].
    pub migration_intent_record: MigrationIntentRecord,
    /// Verified Source fencing report from Step 4.
    pub source_fence_report: SourceFenceReport,
    /// Verified Target capability rebind report from Step 5.
    pub capability_rebind_report: TargetCapabilityRebindReport,
    /// Verified Source lease close/return report from Step 6.
    pub source_lease_close_report: SourceLeaseCloseReport,
    /// Verified Target lease regrant report from Step 6.
    pub target_lease_regrant_report: TargetLeaseRegrantReport,
    /// Optional capability revocation receipts committed on Source after rebind.
    pub source_revocation_receipts: Vec<CapabilityRevocationReceipt>,
    /// Verified Target process restore report (`target_generation = source_generation + 1`)
    /// from Step 7.
    pub target_restore_report: TargetProcessRestoreReport,
    /// Sealed Step 8 [`ProcessMigrationReceipt`] (`verify_digest() == true`).
    pub receipt: ProcessMigrationReceipt,
    /// Target's federation [`CheckpointRecord`] + [`CheckpointPrefix`] committing
    /// `receipt.receipt_digest`.
    pub target_checkpoint_report: TargetReceiptCheckpointReport,
    /// Completed [`ProcessMigrationCoordinator`] at [`ProcessMigrationStep::Completed`].
    pub coordinator: ProcessMigrationCoordinator,
}

/// Fail-closed errors surfaced by [`drive_cell_host_process_migration`] and its
/// [`CellHost`] endpoint implementations.
#[derive(Debug)]
pub enum ProcessMigrationDriveError {
    /// Underlying [`SliceKError`] from a [`CellHost`] authority operation.
    Slice(SliceKError),
    /// Federation placement precheck failed before migration execution.
    Placement(&'static str),
    /// Step-level refusal from [`ProcessMigrationCoordinator`].
    Protocol {
        /// Step at which the coordinator refused.
        step: ProcessMigrationStep,
        /// Underlying protocol error (`[DIST-MIGRATE-001/002]`).
        source: ProcessMigrationError,
    },
    /// `[DIST-ART-001]` / [`nlos_artifact::ArtifactStore`] checkpoint or
    /// restart-image verification failed before `SourceFenced`.
    CheckpointVerification(String),
    /// Source fencing failed to prove stale-generation heartbeat or dispatch
    /// refusal.
    SourceFenceVerification(&'static str),
    /// Capability delegation / attenuation failed on Target or revocation failed
    /// on Source.
    Capability(CapabilityAuthorityError),
    /// `QuotaLease` reconciliation or barrier verification failed.
    QuotaLease(String),
    /// `CapacityLease` reconciliation failed.
    CapacityReconcile(CapacityReconcileError),
    /// `ExclusiveDeviceLease` reconciliation or exclusivity-epoch check failed.
    DeviceReconcile(String),
    /// Target `QuotaLease` or `CapacityLease` regrant exceeded Source's closed/returned
    /// remainder (`[LEASE-MIGRATE-001]`).
    LeaseConservationViolated {
        /// Static description of the violated family.
        family: &'static str,
        /// Requested target amount.
        requested: u64,
        /// Proven returned/unspent amount from Source.
        available_from_source: u64,
    },
    /// Transport / IPC channel error when driving across separate OS processes.
    Transport(String),
    /// Pre-fence failure after Step 1 (`Initiated`) aborted cleanly into
    /// [`MigrationAbortOutcome::AbortedPreFence`] without fencing Source.
    AbortedPreFence {
        /// Step where the pre-fence failure occurred.
        failed_step: ProcessMigrationStep,
        /// Underlying failure cause.
        cause: Box<ProcessMigrationDriveError>,
        /// Coordinator abort outcome (`AbortedPreFence`).
        abort_outcome: MigrationAbortOutcome,
        /// Coordinator at [`ProcessMigrationStep::Aborted`].
        coordinator: Box<ProcessMigrationCoordinator>,
    },
    /// Post-fence failure after Step 4 (`SourceFenced`) and before Step 7
    /// (`TargetRestored`) transitioned the plan into
    /// [`MigrationAbortOutcome::QuarantinedPostFence`] and quarantined unsettled
    /// Source leases so dual-active execution is impossible.
    QuarantinedPostFence {
        /// Step where the post-fence failure occurred (`CapabilitiesRebound`,
        /// `LeasesRegranted`, or `TargetRestored`).
        failed_step: ProcessMigrationStep,
        /// Underlying failure cause.
        cause: Box<ProcessMigrationDriveError>,
        /// Coordinator quarantine outcome (`QuarantinedPostFence`).
        abort_outcome: MigrationAbortOutcome,
        /// Coordinator at [`ProcessMigrationStep::Quarantined`].
        coordinator: Box<ProcessMigrationCoordinator>,
        /// Quarantine states enforced on unsettled Source leases.
        source_quarantine: Option<SourcePostFenceQuarantineReport>,
    },
}

impl fmt::Display for ProcessMigrationDriveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Slice(err) => write!(formatter, "cell-host slice error: {err}"),
            Self::Placement(reason) => {
                write!(formatter, "federation placement check refused: {reason}")
            }
            Self::Protocol { step, source } => {
                write!(
                    formatter,
                    "migration protocol refused at {step:?}: {source}"
                )
            }
            Self::CheckpointVerification(reason) => {
                write!(
                    formatter,
                    "checkpoint/artifact image verification failed: {reason}"
                )
            }
            Self::SourceFenceVerification(reason) => {
                write!(formatter, "source fence verification failed: {reason}")
            }
            Self::Capability(err) => write!(formatter, "capability rebind refused: {err}"),
            Self::QuotaLease(reason) => {
                write!(formatter, "quota lease migration refused: {reason}")
            }
            Self::CapacityReconcile(err) => {
                write!(formatter, "capacity lease reconciliation refused: {err}")
            }
            Self::DeviceReconcile(reason) => {
                write!(formatter, "device lease migration refused: {reason}")
            }
            Self::LeaseConservationViolated {
                family,
                requested,
                available_from_source,
            } => write!(
                formatter,
                "{family} migration conservation violated: requested {requested} > source returned {available_from_source}"
            ),
            Self::Transport(reason) => {
                write!(formatter, "cross-cell migration transport: {reason}")
            }
            Self::AbortedPreFence {
                failed_step, cause, ..
            } => write!(
                formatter,
                "migration aborted pre-fence at {failed_step:?}: {cause}"
            ),
            Self::QuarantinedPostFence {
                failed_step, cause, ..
            } => write!(
                formatter,
                "migration quarantined post-fence at {failed_step:?}: {cause}"
            ),
        }
    }
}

impl Error for ProcessMigrationDriveError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Slice(err) => Some(err),
            Self::Protocol { source, .. } => Some(source),
            Self::Capability(err) => Some(err),
            Self::CapacityReconcile(err) => Some(err),
            Self::AbortedPreFence { cause, .. } | Self::QuarantinedPostFence { cause, .. } => {
                Some(cause)
            }
            _ => None,
        }
    }
}

impl From<SliceKError> for ProcessMigrationDriveError {
    fn from(error: SliceKError) -> Self {
        Self::Slice(error)
    }
}

impl From<CapabilityAuthorityError> for ProcessMigrationDriveError {
    fn from(error: CapabilityAuthorityError) -> Self {
        Self::Capability(error)
    }
}

impl From<CapacityReconcileError> for ProcessMigrationDriveError {
    fn from(error: CapacityReconcileError) -> Self {
        Self::CapacityReconcile(error)
    }
}

/// Source [`CellHost`] endpoint face for cross-Cell process migration.
pub trait SourceCellMigrationEndpoint {
    /// Inspects Source federation registration, active process binding, and
    /// supervisor registration before Step 1 (`Initiated`).
    ///
    /// # Errors
    ///
    /// Fails closed if the host is not federated, the process is absent or
    /// terminal, or the federation directory cannot be read.
    fn inspect_source_placement(
        &self,
        process_id: ProcessId,
        expected_target: CellIdentity,
    ) -> Result<SourceCellPlacementSnapshot, ProcessMigrationDriveError>;

    /// Appends an immutable [`MigrationIntent`] to the shared [`CellDirectory`].
    ///
    /// # Errors
    ///
    /// Fails closed if the host is not federated or the intent write refuses.
    fn record_migration_intent(
        &mut self,
        intent: &MigrationIntent,
    ) -> Result<MigrationIntentRecord, ProcessMigrationDriveError>;

    /// Executes Step 2 (`Quiesced`) on Source: cancels/drains runtime fibers
    /// and in-flight operations, returning [`QuiesceProof`].
    ///
    /// # Errors
    ///
    /// Fails closed if scope cancellation or operation cancellation refuses.
    fn quiesce_source_process(
        &mut self,
        request: &SourceQuiesceRequest,
    ) -> Result<QuiesceProof, ProcessMigrationDriveError>;

    /// Executes Source-side Step 3 (`Checkpointed`): captures state checkpoint
    /// digest or exports the restart-image [`ReplicaRevisionBundle`].
    ///
    /// # Errors
    ///
    /// Fails closed if the artifact or cache read/write fails.
    #[allow(clippy::too_many_lines)]
    fn capture_source_checkpoint(
        &mut self,
        request: &SourceCheckpointRequest,
    ) -> Result<SourceCheckpointBundle, ProcessMigrationDriveError>;

    /// Executes Step 4 (`SourceFenced`): unregisters `(process_id, source_generation)`
    /// from Source's supervisor, terminates the binding, invalidates
    /// `CapabilityNameCache` paths, escalates detector silence to `Dead`, and
    /// verifies that stale-generation heartbeat and dispatch are refused.
    ///
    /// # Errors
    ///
    /// Fails closed if any fencing or refusal verification step fails.
    #[allow(clippy::too_many_lines)]
    fn fence_source_process(
        &mut self,
        request: &SourceFenceRequest,
    ) -> Result<SourceFenceReport, ProcessMigrationDriveError>;

    /// Executes Source-side Step 6 (`LeasesRegranted`): drains/closes Source's
    /// `QuotaLease` (`ACTIVE -> CLOSING -> CLOSED` with verified `FenceBarrierAck`)
    /// and fences/reconciles optional `CapacityLease` and `ExclusiveDeviceLease`
    /// to `RETURNED`.
    ///
    /// # Errors
    ///
    /// Fails closed if any Source lease is quarantined, fails its barrier ACK,
    /// or refuses reconciliation.
    fn close_or_fence_source_leases(
        &mut self,
        spec: &LeaseMigrationBundleSpec,
    ) -> Result<SourceLeaseCloseReport, ProcessMigrationDriveError>;

    /// Revokes old capability handles on Source's `CapabilityAuthority` when
    /// requested after Target capability rebind.
    ///
    /// # Errors
    ///
    /// Fails closed if any signed revocation is rejected.
    fn revoke_source_capabilities(
        &mut self,
        revocations: &[SignedRevokeCapabilityRequest],
    ) -> Result<Vec<CapabilityRevocationReceipt>, ProcessMigrationDriveError>;

    /// Quarantines any unsettled Source leases (`QuotaLease`, `CapacityLease`,
    /// `ExclusiveDeviceLease`) upon a post-fence abort (`QuarantinedPostFence`)
    /// so unsettled resources remain locked/quarantined rather than leaked or
    /// dual-allocated.
    ///
    /// # Errors
    ///
    /// Propagates any underlying authority error while quarantining.
    fn quarantine_unsettled_source_leases(
        &mut self,
        spec: &LeaseMigrationBundleSpec,
    ) -> Result<SourcePostFenceQuarantineReport, ProcessMigrationDriveError>;
}

/// Target [`CellHost`] endpoint face for cross-Cell process migration.
pub trait TargetCellMigrationEndpoint {
    /// Inspects Target federation registration and verifies both Source and
    /// Target Cells are registered in Target's [`CellDirectory`].
    ///
    /// # Errors
    ///
    /// Fails closed if Target is not federated or [`CellDirectory`] cannot read.
    fn inspect_target_placement(
        &self,
        expected_source: CellIdentity,
    ) -> Result<TargetCellPlacementSnapshot, ProcessMigrationDriveError>;

    /// Verifies Step 3 (`Checkpointed`) on Target: validates byte-exact cache
    /// transfer for `TransparentCheckpointRestore` or verifies the committed
    /// `ArtifactStore` revision/blob digest (syncing from Source first if
    /// requested) for `DowngradedRestartFromArtifact`.
    ///
    /// # Errors
    ///
    /// Fails closed on missing artifact, digest mismatch, sync conflict, or
    /// storage error.
    fn verify_checkpoint_on_target(
        &mut self,
        request: &TargetCheckpointVerifyRequest,
    ) -> Result<(), ProcessMigrationDriveError>;

    /// Executes Step 5 (`CapabilitiesRebound`) on Target via signed
    /// `CapabilityAuthority::delegate_signed` (zero authority amplification)
    /// and caches rebound handles in Target's `CapabilityNameCache` at
    /// `target_generation`.
    ///
    /// # Errors
    ///
    /// Fails closed on any rights/scope/purpose/validity/call-limit/depth
    /// amplification, invalid signature, or name-cache fence error.
    fn rebind_capabilities_on_target(
        &mut self,
        request: &TargetCapabilityRebindRequest,
    ) -> Result<TargetCapabilityRebindReport, ProcessMigrationDriveError>;

    /// Executes Target-side Step 6 (`LeasesRegranted`): enforces conservation
    /// against [`SourceLeaseCloseReport`] and grants Target leases.
    ///
    /// # Errors
    ///
    /// Fails closed if requested amounts exceed Source's returned amounts, if
    /// Target's device exclusivity epoch was not bumped (`[LEASE-DEVICE-001]`),
    /// or if Target's lease pools refuse the grant.
    #[allow(clippy::too_many_lines)]
    fn regrant_leases_on_target(
        &mut self,
        request: &TargetLeaseRegrantRequest,
    ) -> Result<TargetLeaseRegrantReport, ProcessMigrationDriveError>;

    /// Executes Step 7 (`TargetRestored`) on Target: seeds process lineage up
    /// to `source_generation`, restores `process_id` at
    /// `target_generation = source_generation + 1` with a fresh fencing token,
    /// registers Target's OS PID in Target's supervisor registry, and records
    /// liveness heartbeat evidence.
    ///
    /// # Errors
    ///
    /// Fails closed on process fence conflict, stale domain, or supervisor error.
    #[allow(clippy::too_many_lines)]
    fn restore_process_on_target(
        &mut self,
        request: &TargetProcessRestoreRequest,
    ) -> Result<TargetProcessRestoreReport, ProcessMigrationDriveError>;

    /// Executes Step 8 (`Completed`) federation checkpointing on Target:
    /// records a structured `ReconciliationCheckpoint` committing
    /// [`ProcessMigrationReceipt::receipt_digest`] and verifies it via
    /// [`nlos_cell::CheckpointFact::verify_digest`].
    ///
    /// # Errors
    ///
    /// Fails closed if Target is not federated, the checkpoint write fails, or
    /// readback digest verification fails.
    fn record_receipt_checkpoint_on_target(
        &mut self,
        receipt: &ProcessMigrationReceipt,
    ) -> Result<TargetReceiptCheckpointReport, ProcessMigrationDriveError>;
}

/// Direct [`SourceCellMigrationEndpoint`] implementation backed by a live
/// federated Source [`CellHost`] and optional [`TokioRuntimeAdapter`].
pub struct LocalCellHostSourceEndpoint<'a> {
    host: &'a mut CellHost,
    adapter: Option<&'a TokioRuntimeAdapter>,
}

impl<'a> LocalCellHostSourceEndpoint<'a> {
    /// Wraps a federated Source [`CellHost`] without a live [`TokioRuntimeAdapter`].
    #[must_use]
    pub fn new(host: &'a mut CellHost) -> Self {
        Self {
            host,
            adapter: None,
        }
    }

    /// Wraps a federated Source [`CellHost`] paired with its [`TokioRuntimeAdapter`]
    /// so Step 2 quiesce and Step 4 fiber batch-cancel drive the live runtime.
    #[must_use]
    pub fn with_adapter(host: &'a mut CellHost, adapter: &'a TokioRuntimeAdapter) -> Self {
        Self {
            host,
            adapter: Some(adapter),
        }
    }

    /// Borrows the underlying Source [`CellHost`].
    #[must_use]
    pub const fn host(&self) -> &CellHost {
        self.host
    }

    /// Mutably borrows the underlying Source [`CellHost`].
    #[must_use]
    pub fn host_mut(&mut self) -> &mut CellHost {
        self.host
    }
}

impl SourceCellMigrationEndpoint for LocalCellHostSourceEndpoint<'_> {
    fn inspect_source_placement(
        &self,
        process_id: ProcessId,
        expected_target: CellIdentity,
    ) -> Result<SourceCellPlacementSnapshot, ProcessMigrationDriveError> {
        let directory = self
            .host
            .cell_directory()
            .ok_or(ProcessMigrationDriveError::Placement(
                "source CellHost is not federated",
            ))?;
        let cell_id = self.host.fence().identity();
        let source_found = directory
            .find(cell_id)
            .map_err(SliceKError::from)?
            .is_some();
        let target_found = directory
            .find(expected_target)
            .map_err(SliceKError::from)?
            .is_some();
        let active_binding = self
            .host
            .runtime()
            .process
            .inspect_active_process_binding(process_id)
            .map_err(SliceKError::from)?;
        let supervisor_registered = self
            .host
            .supervisor_pids()
            .lookup(process_id)
            .is_ok_and(|entry| entry.process_generation == active_binding.process_generation);
        let fence = self.host.fence();
        Ok(SourceCellPlacementSnapshot {
            cell_id,
            node_boot_generation: fence.node_boot_generation(),
            epoch: fence.epoch(),
            fencing_token: fence.fencing_token(),
            both_cells_registered_in_directory: source_found && target_found,
            active_binding,
            supervisor_registered,
        })
    }

    fn record_migration_intent(
        &mut self,
        intent: &MigrationIntent,
    ) -> Result<MigrationIntentRecord, ProcessMigrationDriveError> {
        self.host
            .record_migration_intent(intent)?
            .ok_or(ProcessMigrationDriveError::Placement(
                "source CellHost cannot record MigrationIntent without federation root",
            ))
    }

    fn quiesce_source_process(
        &mut self,
        request: &SourceQuiesceRequest,
    ) -> Result<QuiesceProof, ProcessMigrationDriveError> {
        if let (Some(adapter), Some((scope_id, scope_gen))) = (self.adapter, request.cancel_scope) {
            RuntimeAdapter::cancel_scope(adapter, scope_id, scope_gen)
                .map_err(SliceKError::from)?;
        }
        let mut cancelled_ops = 0u32;
        for &(op_handle, cancel_receipt_id) in &request.inflight_operations {
            self.host
                .runtime()
                .operations
                .request_cancel(op_handle, cancel_receipt_id)
                .map_err(SliceKError::from)?;
            cancelled_ops = cancelled_ops.saturating_add(1);
        }
        Ok(QuiesceProof {
            process_id: request.process_id,
            source_cell_id: *request.source_cell_id.as_bytes(),
            source_generation: request.source_generation.get(),
            quiesced_fibers: request.active_fiber_count,
            cancelled_inflight_ops: cancelled_ops,
            inflight_effects_drained: true,
            quiesced_at_ms: request.quiesced_at_ms,
        })
    }

    #[allow(clippy::too_many_lines)]
    fn capture_source_checkpoint(
        &mut self,
        request: &SourceCheckpointRequest,
    ) -> Result<SourceCheckpointBundle, ProcessMigrationDriveError> {
        let binding = self
            .host
            .runtime()
            .process
            .inspect_active_process_binding(request.process_id)
            .map_err(SliceKError::from)?;
        if binding.process_generation != request.source_generation {
            return Err(ProcessMigrationDriveError::Placement(
                "source process generation drifted before checkpoint capture",
            ));
        }

        match (&request.migration_mode, &request.spec) {
            (
                MigrationMode::TransparentCheckpointRestore,
                CheckpointSourceSpec::TransparentState {
                    state_payload,
                    cache_key,
                    checkpointed_at_ms,
                },
            ) => {
                let state_checkpoint_digest = compute_transparent_checkpoint_digest(
                    request.migration_id,
                    request.process_id,
                    request.source_generation,
                    binding.process_fencing_token,
                    state_payload,
                );
                let transparent_cache_transfer = if let Some(key) = cache_key {
                    self.host
                        .runtime()
                        .artifacts
                        .put_cache_blob(key, state_payload, *checkpointed_at_ms)
                        .map_err(SliceKError::from)?;
                    Some((key.clone(), state_payload.clone(), *checkpointed_at_ms))
                } else {
                    None
                };
                Ok(SourceCheckpointBundle {
                    proof: ProcessCheckpointProof {
                        process_id: request.process_id,
                        source_generation: request.source_generation.get(),
                        payload: CheckpointPayloadKind::FullStateCheckpoint {
                            state_checkpoint_digest,
                            migrated_session_digests: request.migratable_sessions.clone(),
                        },
                        checkpointed_at_ms: *checkpointed_at_ms,
                    },
                    transparent_cache_transfer,
                    exported_artifact_bundle: None,
                })
            }
            (
                MigrationMode::DowngradedRestartFromArtifact {
                    artifact_image_digest,
                    ..
                },
                CheckpointSourceSpec::ArtifactRestartImage {
                    artifact_id,
                    revision,
                    sync_from_source_if_missing,
                    checkpointed_at_ms,
                },
            ) => {
                let exported_artifact_bundle = if *sync_from_source_if_missing {
                    let bundle = export_replica_revision_bundle(
                        &self.host.runtime().artifacts,
                        *artifact_id,
                        *revision,
                        *request.source_cell_id.as_bytes(),
                        false,
                        *checkpointed_at_ms,
                    )
                    .map_err(|err| {
                        ProcessMigrationDriveError::CheckpointVerification(format!(
                            "source artifact export failed: {err}"
                        ))
                    })?;
                    if bundle.remote_head.digest.into_bytes() != *artifact_image_digest {
                        return Err(ProcessMigrationDriveError::CheckpointVerification(
                            "source artifact revision digest disagrees with MigrationMode::DowngradedRestartFromArtifact digest"
                                .to_string(),
                        ));
                    }
                    Some(bundle)
                } else {
                    None
                };
                Ok(SourceCheckpointBundle {
                    proof: ProcessCheckpointProof {
                        process_id: request.process_id,
                        source_generation: request.source_generation.get(),
                        payload: CheckpointPayloadKind::ArtifactRestartImage {
                            artifact_image_digest: *artifact_image_digest,
                        },
                        checkpointed_at_ms: *checkpointed_at_ms,
                    },
                    transparent_cache_transfer: None,
                    exported_artifact_bundle,
                })
            }
            _ => Err(ProcessMigrationDriveError::CheckpointVerification(
                "CheckpointSourceSpec variant does not match resolved MigrationMode".to_string(),
            )),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn fence_source_process(
        &mut self,
        request: &SourceFenceRequest,
    ) -> Result<SourceFenceReport, ProcessMigrationDriveError> {
        let binding = self
            .host
            .runtime()
            .process
            .inspect_active_process_binding(request.process_id)
            .map_err(SliceKError::from)?;
        if binding.process_generation != request.source_generation {
            return Err(ProcessMigrationDriveError::SourceFenceVerification(
                "source process generation mismatch at fence step",
            ));
        }
        let source_fencing_token = binding.process_fencing_token;

        // 1. Unregister `(process_id, source_generation)` from Source's SupervisorPidRegistry.
        let unregistered = self
            .host
            .supervisor_pids()
            .unregister(request.process_id, request.source_generation)
            .map_err(SliceKError::from)?;
        if !unregistered {
            return Err(ProcessMigrationDriveError::SourceFenceVerification(
                "source process was not registered in source SupervisorPidRegistry",
            ));
        }

        // 2. Mark `(process_id, source_generation)` terminal on Source's ProcessAuthority
        //    and batch-cancel any registered fiber incarnations.
        self.host
            .runtime()
            .process
            .mark_process_terminated(MarkProcessTerminatedRequest {
                process_id: request.process_id,
                expected_process_generation: request.source_generation,
                expected_process_fencing_token: source_fencing_token,
                idempotency_key: request.terminate_idempotency_key,
                marked_at_ms: request.fenced_at_ms,
            })
            .map_err(SliceKError::from)?;

        if let Some(adapter) = self.adapter {
            adapter
                .cancel_process_fibers(
                    &self.host.runtime().process,
                    PropagateCancelToFibersRequest {
                        process_id: request.process_id,
                        expected_process_generation: request.source_generation,
                        expected_process_fencing_token: source_fencing_token,
                        lifecycle_state: ProcessLifecycleState::Terminated,
                        idempotency_key: request.terminate_idempotency_key,
                        cancelled_at_ms: request.fenced_at_ms,
                    },
                )
                .map_err(SliceKError::from)?;
        } else {
            self.host
                .runtime()
                .process
                .propagate_cancel_to_fibers(PropagateCancelToFibersRequest {
                    process_id: request.process_id,
                    expected_process_generation: request.source_generation,
                    expected_process_fencing_token: source_fencing_token,
                    lifecycle_state: ProcessLifecycleState::Terminated,
                    idempotency_key: request.terminate_idempotency_key,
                    cancelled_at_ms: request.fenced_at_ms,
                })
                .map_err(SliceKError::from)?;
        }

        // 3. Invalidate all declared CapabilityNameCache paths through `source_generation`.
        let mut invalidated_name_paths = 0usize;
        for path in &request.invalidate_name_paths {
            self.host
                .invalidate_cached_name(path.clone(), request.source_generation)?;
            if self.host.cached_name(path).is_some() {
                return Err(ProcessMigrationDriveError::SourceFenceVerification(
                    "source CapabilityNameCache still served invalidated path",
                ));
            }
            invalidated_name_paths += 1;
        }

        // 4. Escalate Source FailureDetector silence to `Dead` so raw heartbeats
        //    for the fenced incarnation on Source are permanently refused.
        let subject = MonitoredSubject::Process(request.process_id);
        if self.host.failure_detector().liveness(subject).is_some() {
            let dead_after = self.host.failure_detector().config().dead_after().get();
            for _ in 0..=dead_after {
                let view = self.host.failure_detector().liveness(subject);
                if view.is_some_and(|v| v.liveness == Liveness::Dead) {
                    break;
                }
                self.host.sweep_liveness()?;
            }
        }

        // 5. Explicitly verify that Source CellHost now refuses BOTH stale-generation
        //    heartbeat AND stale-generation fiber dispatch!
        let fenced_hb_probe = self
            .host
            .record_fenced_process_heartbeat(request.process_id, request.source_generation);
        let raw_hb_probe = self.host.record_process_heartbeat(request.process_id);
        let stale_heartbeat_refused = fenced_hb_probe.is_err() && raw_hb_probe.is_err();
        if !stale_heartbeat_refused {
            return Err(ProcessMigrationDriveError::SourceFenceVerification(
                "source CellHost accepted a stale-generation process heartbeat after fencing",
            ));
        }

        let dispatch_probe = self.host.runtime().process.register_fiber_incarnation(
            RegisterFiberIncarnationRequest {
                process_id: request.process_id,
                expected_process_generation: request.source_generation,
                expected_process_fencing_token: source_fencing_token,
                binding: request.stale_dispatch_probe_fiber_id,
                idempotency_key: request.stale_dispatch_probe_key,
                registered_at_ms: request.fenced_at_ms,
            },
        );
        let stale_dispatch_refused = matches!(
            dispatch_probe,
            Err(ProcessAuthorityError::ProcessBindingTerminal(
                ProcessLifecycleState::Terminated
            ))
        );
        if !stale_dispatch_refused {
            return Err(ProcessMigrationDriveError::SourceFenceVerification(
                "source CellHost did not refuse stale-generation fiber dispatch after fencing",
            ));
        }

        let fence = self.host.fence();
        Ok(SourceFenceReport {
            proof: SourceProcessFenceProof {
                process_id: request.process_id,
                source_cell_id: *fence.identity().as_bytes(),
                source_cell_epoch: fence.epoch().get(),
                fenced_generation: request.source_generation.get(),
                source_fencing_token,
                supervisor_unregistered: true,
                fenced_at_ms: request.fenced_at_ms,
            },
            stale_heartbeat_refused,
            stale_dispatch_refused,
            invalidated_name_paths,
        })
    }

    fn close_or_fence_source_leases(
        &mut self,
        spec: &LeaseMigrationBundleSpec,
    ) -> Result<SourceLeaseCloseReport, ProcessMigrationDriveError> {
        // 1. QuotaLease: drain/freeze (`ACTIVE -> CLOSING`), verify `FenceBarrierAck`,
        //    and close (`CLOSING -> CLOSED`, returning unspent remainder only after close).
        let current_quota = self.host.query_quota_lease(spec.quota.source_lease_id)?;
        match current_quota.state() {
            QuotaLeaseState::Quarantined => {
                return Err(ProcessMigrationDriveError::QuotaLease(
                    "source QuotaLease is QUARANTINED and cannot migrate without reconciliation"
                        .to_string(),
                ));
            }
            QuotaLeaseState::Issued => {
                self.host.activate_quota_lease(spec.quota.source_lease_id)?;
                self.host.begin_quota_close(spec.quota.source_lease_id)?;
            }
            QuotaLeaseState::Active => {
                self.host.begin_quota_close(spec.quota.source_lease_id)?;
            }
            QuotaLeaseState::Closing | QuotaLeaseState::Closed => {}
            other => {
                return Err(ProcessMigrationDriveError::QuotaLease(format!(
                    "source QuotaLease in non-migratable state {other:?}"
                )));
            }
        }
        let closing_quota = self.host.query_quota_lease(spec.quota.source_lease_id)?;
        let quota_ack = spec.quota.override_fence_ack.unwrap_or_else(|| {
            FenceBarrierAck::for_quota(&closing_quota, spec.quota.gateway_id, closing_quota.spent())
        });
        verify_quota_ack_matches(&closing_quota, &quota_ack)?;
        let closed_quota = if closing_quota.state() == QuotaLeaseState::Closing {
            self.host.ack_quota_close(spec.quota.source_lease_id)?
        } else {
            closing_quota
        };

        let closed_quota_summary = SourceClosedQuotaSummary {
            lease_id: closed_quota.lease_id(),
            state: closed_quota.state(),
            face_value: closed_quota.face_value(),
            spent: closed_quota.spent(),
            returned: closed_quota.returned(),
            close_digest: quota_ack.ack_digest(),
        };

        // 2. Optional CapacityLease: fence (`-> FENCED`) and reconcile with
        //    `CleanupAttestation` (`holder_processes_terminated = true`, proven by Step 4)
        //    + `FenceBarrierAck::for_capacity` (`-> RETURNED`).
        let returned_capacity = if let Some(cap_spec) = spec.capacity {
            let current_cap = self.host.query_capacity_lease(cap_spec.source_lease_id)?;
            let fenced_cap = match current_cap.state() {
                CapacityLeaseState::GlobalReserved
                | CapacityLeaseState::TargetPrepared
                | CapacityLeaseState::Active
                | CapacityLeaseState::Returning => {
                    self.host.fence_capacity_lease(cap_spec.source_lease_id)?
                }
                CapacityLeaseState::Fenced => current_cap,
                CapacityLeaseState::Returned => {
                    let ack = FenceBarrierAck::for_capacity(&current_cap, cap_spec.gateway_id);
                    return finish_device_close(
                        self.host,
                        spec,
                        closed_quota_summary,
                        Some(SourceReturnedCapacitySummary {
                            capacity_lease_id: current_cap.capacity_lease_id(),
                            state: current_cap.state(),
                            amount: current_cap.amount(),
                            close_digest: ack.ack_digest(),
                        }),
                    );
                }
                CapacityLeaseState::Quarantined => {
                    return Err(ProcessMigrationDriveError::QuotaLease(
                        "source CapacityLease is QUARANTINED and cannot migrate without reconciliation"
                            .to_string(),
                    ));
                }
            };
            let cleanup = CleanupAttestation::for_lease(&fenced_cap, fenced_cap.amount(), true);
            let cap_ack = FenceBarrierAck::for_capacity(&fenced_cap, cap_spec.gateway_id);
            let cap_receipt = CapacityReconciliationReceipt::for_lease(
                cap_spec.reconcile_receipt_id,
                &fenced_cap,
                cleanup,
                cap_ack,
            );
            let returned = self.host.reconcile_capacity_lease(cap_receipt)?;
            Some(SourceReturnedCapacitySummary {
                capacity_lease_id: returned.capacity_lease_id(),
                state: returned.state(),
                amount: returned.amount(),
                close_digest: cap_ack.ack_digest(),
            })
        } else {
            None
        };

        finish_device_close(self.host, spec, closed_quota_summary, returned_capacity)
    }

    fn revoke_source_capabilities(
        &mut self,
        revocations: &[SignedRevokeCapabilityRequest],
    ) -> Result<Vec<CapabilityRevocationReceipt>, ProcessMigrationDriveError> {
        let mut receipts = Vec::with_capacity(revocations.len());
        for revocation in revocations {
            let decision = self
                .host
                .runtime()
                .capability()
                .revoke_signed(&self.host.runtime().identity, *revocation)?;
            receipts.push(decision.receipt());
        }
        Ok(receipts)
    }

    fn quarantine_unsettled_source_leases(
        &mut self,
        spec: &LeaseMigrationBundleSpec,
    ) -> Result<SourcePostFenceQuarantineReport, ProcessMigrationDriveError> {
        let capacity_state = if let Some(cap_spec) = spec.capacity {
            let current = self.host.query_capacity_lease(cap_spec.source_lease_id)?;
            let settled = if current.state() == CapacityLeaseState::Returned {
                current
            } else {
                self.host
                    .quarantine_capacity_lease(cap_spec.source_lease_id)?
            };
            Some(settled.state())
        } else {
            None
        };

        let device_state = if let Some(dev_spec) = spec.device {
            let current = self.host.query_device_lease(dev_spec.source_lease_id)?;
            let settled = if current.state() == ExclusiveDeviceLeaseState::Returned {
                current
            } else {
                self.host
                    .quarantine_device_lease(dev_spec.source_lease_id)?
            };
            Some(settled.state())
        } else {
            None
        };

        let current_quota = self.host.query_quota_lease(spec.quota.source_lease_id)?;
        let quota_state = match current_quota.state() {
            QuotaLeaseState::Closed | QuotaLeaseState::Cancelled | QuotaLeaseState::Quarantined => {
                current_quota.state()
            }
            QuotaLeaseState::Issued
            | QuotaLeaseState::Active
            | QuotaLeaseState::Closing
            | QuotaLeaseState::Fenced => {
                // `LEASE-LOSS-001`: advancing Source Cell's epoch atomically moves
                // unsettled QuotaLeases into `Quarantined` without returning funds.
                self.host.advance_cell_epoch()?;
                self.host
                    .query_quota_lease(spec.quota.source_lease_id)?
                    .state()
            }
        };

        Ok(SourcePostFenceQuarantineReport {
            quota_state,
            capacity_state,
            device_state,
        })
    }
}

fn finish_device_close(
    host: &mut CellHost,
    spec: &LeaseMigrationBundleSpec,
    closed_quota: SourceClosedQuotaSummary,
    returned_capacity: Option<SourceReturnedCapacitySummary>,
) -> Result<SourceLeaseCloseReport, ProcessMigrationDriveError> {
    let returned_device = if let Some(dev_spec) = spec.device {
        let current_dev = host.query_device_lease(dev_spec.source_lease_id)?;
        let head_reset_gen = host.device_reset_generation();
        let fenced_dev = match current_dev.state() {
            ExclusiveDeviceLeaseState::DeviceReserved | ExclusiveDeviceLeaseState::Resetting => {
                host.fence_device_lease(dev_spec.source_lease_id)?
            }
            ExclusiveDeviceLeaseState::Fencing => current_dev,
            ExclusiveDeviceLeaseState::Returned => {
                let ack = FenceBarrierAck::for_device(&current_dev, dev_spec.gateway_id);
                return Ok(SourceLeaseCloseReport {
                    closed_quota,
                    returned_capacity,
                    returned_device: Some(SourceReturnedDeviceSummary {
                        device_lease_id: current_dev.device_lease_id(),
                        state: current_dev.state(),
                        exclusivity_epoch: current_dev.exclusivity_epoch(),
                        head_reset_generation: head_reset_gen,
                        close_digest: ack.ack_digest(),
                    }),
                });
            }
            ExclusiveDeviceLeaseState::Quarantined => {
                return Err(ProcessMigrationDriveError::DeviceReconcile(
                    "source ExclusiveDeviceLease is QUARANTINED and cannot migrate without reconciliation"
                        .to_string(),
                ));
            }
        };
        let reset_receipt = DeviceResetReceipt::new(
            dev_spec.reset_receipt_id,
            fenced_dev.device_lease_id(),
            fenced_dev.holder_node(),
            fenced_dev.holder_node_boot_generation(),
            fenced_dev.exclusivity_epoch(),
            fenced_dev.fencing_token(),
            head_reset_gen,
        );
        let dev_ack = FenceBarrierAck::for_device(&fenced_dev, dev_spec.gateway_id);
        let dev_reconcile = DeviceReconciliationReceipt::for_lease(
            dev_spec.reconcile_receipt_id,
            &fenced_dev,
            true,
            true,
            true,
            reset_receipt,
            dev_ack,
        );
        let returned = host
            .reconcile_device_lease(dev_reconcile)
            .map_err(|err| ProcessMigrationDriveError::DeviceReconcile(err.to_string()))?;
        if !host.device_head_free() {
            return Err(ProcessMigrationDriveError::DeviceReconcile(
                "source DeviceLeaseHead did not return to FREE after reconciliation".to_string(),
            ));
        }
        Some(SourceReturnedDeviceSummary {
            device_lease_id: returned.device_lease_id(),
            state: returned.state(),
            exclusivity_epoch: returned.exclusivity_epoch(),
            head_reset_generation: host.device_reset_generation(),
            close_digest: dev_ack.ack_digest(),
        })
    } else {
        None
    };

    Ok(SourceLeaseCloseReport {
        closed_quota,
        returned_capacity,
        returned_device,
    })
}

fn verify_quota_ack_matches(
    lease: &QuotaLeaseGrant,
    ack: &FenceBarrierAck,
) -> Result<(), ProcessMigrationDriveError> {
    if ack.lease_id() != *lease.lease_id().as_bytes()
        || ack.fence_scope() != lease.fence_scope()
        || ack.epoch() != lease.epoch()
        || ack.fencing_token() != lease.fencing_token()
        || ack.observed_high_water() != lease.spent()
        || !ack.verify_digest()
    {
        return Err(ProcessMigrationDriveError::QuotaLease(
            "source QuotaLease FenceBarrierAck verification failed".to_string(),
        ));
    }
    Ok(())
}

/// Direct [`TargetCellMigrationEndpoint`] implementation backed by a live
/// federated Target [`CellHost`].
pub struct LocalCellHostTargetEndpoint<'a> {
    host: &'a mut CellHost,
}

impl<'a> LocalCellHostTargetEndpoint<'a> {
    /// Wraps a federated Target [`CellHost`].
    #[must_use]
    pub fn new(host: &'a mut CellHost) -> Self {
        Self { host }
    }

    /// Borrows the underlying Target [`CellHost`].
    #[must_use]
    pub const fn host(&self) -> &CellHost {
        self.host
    }

    /// Mutably borrows the underlying Target [`CellHost`].
    #[must_use]
    pub fn host_mut(&mut self) -> &mut CellHost {
        self.host
    }
}

impl TargetCellMigrationEndpoint for LocalCellHostTargetEndpoint<'_> {
    fn inspect_target_placement(
        &self,
        expected_source: CellIdentity,
    ) -> Result<TargetCellPlacementSnapshot, ProcessMigrationDriveError> {
        let directory = self
            .host
            .cell_directory()
            .ok_or(ProcessMigrationDriveError::Placement(
                "target CellHost is not federated",
            ))?;
        let cell_id = self.host.fence().identity();
        let source_found = directory
            .find(expected_source)
            .map_err(SliceKError::from)?
            .is_some();
        let target_found = directory
            .find(cell_id)
            .map_err(SliceKError::from)?
            .is_some();
        Ok(TargetCellPlacementSnapshot {
            cell_id,
            fence: self.host.fence(),
            both_cells_registered_in_directory: source_found && target_found,
        })
    }

    fn verify_checkpoint_on_target(
        &mut self,
        request: &TargetCheckpointVerifyRequest,
    ) -> Result<(), ProcessMigrationDriveError> {
        match (&request.migration_mode, &request.spec) {
            (
                MigrationMode::TransparentCheckpointRestore,
                CheckpointSourceSpec::TransparentState { .. },
            ) => {
                if let Some((cache_key, payload, created_at_ms)) =
                    &request.source_bundle.transparent_cache_transfer
                {
                    let put_digest = self
                        .host
                        .runtime()
                        .artifacts
                        .put_cache_blob(cache_key, payload, *created_at_ms)
                        .map_err(SliceKError::from)?;
                    let read_back = self
                        .host
                        .runtime()
                        .artifacts
                        .get_cache_blob(cache_key)
                        .map_err(SliceKError::from)?
                        .ok_or_else(|| {
                            ProcessMigrationDriveError::CheckpointVerification(
                                "target cache blob readback missed immediately after commit"
                                    .to_string(),
                            )
                        })?;
                    if read_back != *payload || ContentDigest::of_bytes(&read_back) != put_digest {
                        return Err(ProcessMigrationDriveError::CheckpointVerification(
                            "target cache blob digest mismatch".to_string(),
                        ));
                    }
                }
                Ok(())
            }
            (
                MigrationMode::DowngradedRestartFromArtifact {
                    artifact_image_digest,
                    ..
                },
                CheckpointSourceSpec::ArtifactRestartImage {
                    artifact_id,
                    revision,
                    checkpointed_at_ms,
                    ..
                },
            ) => {
                // If Source exported a `[DIST-ART-001]` ReplicaRevisionBundle and Target
                // does not yet hold `(artifact_id, revision)`, stage & publish it on Target!
                let already_present = self
                    .host
                    .runtime()
                    .artifacts
                    .get_revision(*artifact_id, *revision, *checkpointed_at_ms)
                    .is_ok();
                if !already_present
                    && let Some(bundle) = &request.source_bundle.exported_artifact_bundle
                {
                    sync_bundle_into_target_artifact_store(
                        &self.host.runtime().artifacts,
                        bundle,
                        request.sync_seed,
                        *checkpointed_at_ms,
                    )?;
                }

                // Read and re-verify the committed artifact bytes directly from Target's
                // real `ArtifactStore` (`[DIST-MIGRATE-002]`).
                let verified_bytes = self
                    .host
                    .runtime()
                    .artifacts
                    .get_revision(*artifact_id, *revision, *checkpointed_at_ms)
                    .map_err(|err| {
                        ProcessMigrationDriveError::CheckpointVerification(format!(
                            "target ArtifactStore failed to read restart image ({artifact_id:?} rev {revision}): {err}"
                        ))
                    })?;
                let computed = ContentDigest::of_bytes(&verified_bytes).into_bytes();
                if computed != *artifact_image_digest {
                    return Err(ProcessMigrationDriveError::CheckpointVerification(
                        "target ArtifactStore revision digest does not match expected artifact_image_digest"
                            .to_string(),
                    ));
                }
                Ok(())
            }
            _ => Err(ProcessMigrationDriveError::CheckpointVerification(
                "TargetCheckpointVerifyRequest mode and spec mismatch".to_string(),
            )),
        }
    }

    fn rebind_capabilities_on_target(
        &mut self,
        request: &TargetCapabilityRebindRequest,
    ) -> Result<TargetCapabilityRebindReport, ProcessMigrationDriveError> {
        if let Some(min_source_epoch) = request.ensure_target_epoch_above {
            while self.host.fence().epoch() <= min_source_epoch {
                self.host.advance_cell_epoch()?;
            }
        }

        let mut rebound_entries = Vec::with_capacity(request.items.len());
        let mut delegated_records = Vec::with_capacity(request.items.len());
        for item in &request.items {
            for principal_req in &item.bootstrap_principals {
                self.host
                    .runtime()
                    .identity
                    .bootstrap_principal(*principal_req)
                    .map_err(SliceKError::from)?;
            }
            if let Some(root_req) = item.seed_parent_root {
                self.host
                    .runtime()
                    .capability()
                    .issue_root_signed(&self.host.runtime().identity, root_req)?;
            }
            let decision = self
                .host
                .runtime()
                .capability()
                .delegate_signed(&self.host.runtime().identity, item.signed_delegation)?;
            let record = decision.record();
            let receipt = decision.receipt();
            if let Some(path) = &item.target_name_path {
                self.host.insert_cached_name(
                    path.clone(),
                    record.handle.capability_id,
                    request.target_generation,
                )?;
            }
            let attenuation_digest = compute_rebound_capability_attenuation_digest(
                &record,
                *request.source_cell_id.as_bytes(),
                request.source_generation.get(),
                *request.target_cell_id.as_bytes(),
                request.target_generation.get(),
            );
            rebound_entries.push(ReboundCapabilityEntry {
                capability_id: *record.handle.capability_id.as_bytes(),
                source_cell_id: *request.source_cell_id.as_bytes(),
                source_generation: request.source_generation.get(),
                target_cell_id: *request.target_cell_id.as_bytes(),
                target_generation: request.target_generation.get(),
                attenuation_digest,
            });
            delegated_records.push((record, receipt));
        }

        Ok(TargetCapabilityRebindReport {
            proof: CapabilityRebindProof {
                process_id: request.process_id,
                rebound_capabilities: rebound_entries,
                rebound_at_ms: request.rebound_at_ms,
            },
            delegated_records,
            target_fence: self.host.fence(),
        })
    }

    #[allow(clippy::too_many_lines)]
    fn regrant_leases_on_target(
        &mut self,
        request: &TargetLeaseRegrantRequest,
    ) -> Result<TargetLeaseRegrantReport, ProcessMigrationDriveError> {
        let closed_quota = request.source_closed.closed_quota;
        if closed_quota.state != QuotaLeaseState::Closed {
            return Err(ProcessMigrationDriveError::QuotaLease(
                "cannot regrant target QuotaLease before source QuotaLease is CLOSED".to_string(),
            ));
        }
        let unspent_returned = closed_quota.returned;
        let target_face = request
            .spec
            .quota
            .target_face_value
            .unwrap_or(unspent_returned);
        if target_face > unspent_returned {
            return Err(ProcessMigrationDriveError::LeaseConservationViolated {
                family: "QuotaLease",
                requested: target_face,
                available_from_source: unspent_returned,
            });
        }

        // If an ExclusiveDeviceLease is migrating, verify Target's exclusivity_epoch
        // strictly exceeds Source's (`[LEASE-DEVICE-001]`) BEFORE granting any Target leases.
        if let (Some(dev_spec), Some(returned_dev)) =
            (request.spec.device, request.source_closed.returned_device)
        {
            if returned_dev.state != ExclusiveDeviceLeaseState::Returned {
                return Err(ProcessMigrationDriveError::DeviceReconcile(
                    "cannot regrant target ExclusiveDeviceLease before source is RETURNED"
                        .to_string(),
                ));
            }
            if dev_spec.advance_target_epoch_if_needed {
                while self.host.fence().epoch() <= returned_dev.exclusivity_epoch {
                    self.host.advance_cell_epoch()?;
                }
            }
            if self.host.fence().epoch() <= returned_dev.exclusivity_epoch {
                return Err(ProcessMigrationDriveError::DeviceReconcile(format!(
                    "target exclusivity_epoch {} must exceed source exclusivity_epoch {}",
                    self.host.fence().epoch().get(),
                    returned_dev.exclusivity_epoch.get()
                )));
            }
        }

        let granted_quota = self.host.grant_quota_lease(
            request.spec.quota.target_lease_id,
            target_face,
            request.spec.quota.target_expires_at,
        )?;
        let target_quota = if request.spec.quota.activate_on_target {
            self.host
                .activate_quota_lease(request.spec.quota.target_lease_id)?
        } else {
            granted_quota
        };

        let mut migrated_leases = vec![MigratedLeaseEntry {
            source_lease_id: *request.spec.quota.source_lease_id.as_bytes(),
            source_cell_id: *request.source_cell_id.as_bytes(),
            source_closed_or_fenced: true,
            source_close_receipt_digest: closed_quota.close_digest,
            target_lease_id: *request.spec.quota.target_lease_id.as_bytes(),
            target_cell_id: *request.target_cell_id.as_bytes(),
            target_grant_epoch: target_quota.epoch().get(),
        }];

        let target_capacity = match (
            request.spec.capacity,
            request.source_closed.returned_capacity,
        ) {
            (Some(cap_spec), Some(returned_cap)) => {
                if returned_cap.state != CapacityLeaseState::Returned {
                    return Err(ProcessMigrationDriveError::QuotaLease(
                        "cannot regrant target CapacityLease before source is RETURNED".to_string(),
                    ));
                }
                let requested_amount = cap_spec.target_amount.unwrap_or(returned_cap.amount);
                if requested_amount > returned_cap.amount {
                    return Err(ProcessMigrationDriveError::LeaseConservationViolated {
                        family: "CapacityLease",
                        requested: requested_amount,
                        available_from_source: returned_cap.amount,
                    });
                }
                let granted_cap = self
                    .host
                    .grant_capacity_lease(cap_spec.target_lease_id, requested_amount)?;
                migrated_leases.push(MigratedLeaseEntry {
                    source_lease_id: *cap_spec.source_lease_id.as_bytes(),
                    source_cell_id: *request.source_cell_id.as_bytes(),
                    source_closed_or_fenced: true,
                    source_close_receipt_digest: returned_cap.close_digest,
                    target_lease_id: *cap_spec.target_lease_id.as_bytes(),
                    target_cell_id: *request.target_cell_id.as_bytes(),
                    target_grant_epoch: granted_cap.capacity_epoch().get(),
                });
                Some(granted_cap)
            }
            (None, None) => None,
            _ => {
                return Err(ProcessMigrationDriveError::QuotaLease(
                    "CapacityLease spec and source close report presence mismatch".to_string(),
                ));
            }
        };

        let target_device = match (request.spec.device, request.source_closed.returned_device) {
            (Some(dev_spec), Some(returned_dev)) => {
                let granted_dev = self.host.grant_device_lease(dev_spec.target_lease_id)?;
                migrated_leases.push(MigratedLeaseEntry {
                    source_lease_id: *dev_spec.source_lease_id.as_bytes(),
                    source_cell_id: *request.source_cell_id.as_bytes(),
                    source_closed_or_fenced: true,
                    source_close_receipt_digest: returned_dev.close_digest,
                    target_lease_id: *dev_spec.target_lease_id.as_bytes(),
                    target_cell_id: *request.target_cell_id.as_bytes(),
                    target_grant_epoch: granted_dev.exclusivity_epoch().get(),
                });
                Some(granted_dev)
            }
            (None, None) => None,
            _ => {
                return Err(ProcessMigrationDriveError::DeviceReconcile(
                    "ExclusiveDeviceLease spec and source close report presence mismatch"
                        .to_string(),
                ));
            }
        };

        Ok(TargetLeaseRegrantReport {
            proof: LeaseCloseRegrantProof {
                process_id: request.process_id,
                source_generation: request.source_generation.get(),
                target_generation: request.target_generation.get(),
                migrated_leases,
                regranted_at_ms: request.spec.regranted_at_ms,
            },
            target_quota,
            target_capacity,
            target_device,
        })
    }

    #[allow(clippy::too_many_lines)]
    fn restore_process_on_target(
        &mut self,
        request: &TargetProcessRestoreRequest,
    ) -> Result<TargetProcessRestoreReport, ProcessMigrationDriveError> {
        let domain = match request.existing_target_domain_id {
            Some(domain_id) => self
                .host
                .runtime()
                .process
                .inspect_isolation_domain(domain_id)
                .map_err(SliceKError::from)?,
            None => self
                .host
                .runtime()
                .process
                .create_isolation_domain(CreateIsolationDomainRequest {
                    policy_digest: request.target_domain_policy_digest,
                    idempotency_key: request.target_domain_idempotency_key,
                    created_at_ms: request.restored_at_ms,
                })
                .map_err(SliceKError::from)?
                .record()
                .clone(),
        };

        // Determine Target's current head generation for `process_id` (if any),
        // then idempotently seed missing lineage hops up to `source_generation`.
        let current_target_gen = match self
            .host
            .runtime()
            .process
            .inspect_process_terminal(request.process_id)
        {
            Err(ProcessAuthorityError::ProcessNotFound(_)) => None,
            Ok(Some(terminal)) => Some(terminal.process_generation),
            Ok(None) => Some(
                self.host
                    .runtime()
                    .process
                    .inspect_active_process_binding(request.process_id)
                    .map_err(SliceKError::from)?
                    .process_generation,
            ),
            Err(other) => return Err(SliceKError::from(other).into()),
        };

        let mut head_gen = if let Some(gen_val) = current_target_gen {
            gen_val
        } else {
            let genesis = &request.lineage.genesis;
            let seeded = self
                .host
                .runtime()
                .process
                .register_delegated_process(RegisterDelegatedProcessRequest {
                    task_id: genesis.task_id,
                    task_attempt_id: genesis.task_attempt_id,
                    attempt_generation: genesis.attempt_generation,
                    isolation_domain_id: domain.isolation_domain_id,
                    isolation_domain_generation: domain.generation,
                    isolation_domain_fencing_token: domain.fencing_token,
                    idempotency_key: genesis.idempotency_key,
                    created_at_ms: genesis.created_at_ms,
                })
                .map_err(SliceKError::from)?
                .record()
                .clone();
            if seeded.process_id != request.process_id {
                return Err(ProcessMigrationDriveError::Placement(
                    "ProcessLineageSpec genesis derived a different ProcessId than requested",
                ));
            }
            seeded.process_generation
        };

        for hop in &request.lineage.prior_restores {
            if hop.expected_generation >= head_gen {
                let stepped = self
                    .host
                    .runtime()
                    .process
                    .restore_process(RestoreProcessRequest {
                        process_id: request.process_id,
                        expected_process_generation: hop.expected_generation,
                        expected_process_fencing_token: hop.expected_fencing_token,
                        isolation_domain_id: domain.isolation_domain_id,
                        isolation_domain_generation: domain.generation,
                        isolation_domain_fencing_token: domain.fencing_token,
                        idempotency_key: hop.idempotency_key,
                        restored_at_ms: hop.restored_at_ms,
                    })
                    .map_err(SliceKError::from)?
                    .record()
                    .clone();
                head_gen = stepped.process_generation;
            }
        }

        // Now atomically restore `process_id` to `target_generation = source_generation + 1`
        // under Target's active `IsolationDomain` with a fresh fencing token!
        let restored_binding = self
            .host
            .runtime()
            .process
            .restore_process(RestoreProcessRequest {
                process_id: request.process_id,
                expected_process_generation: request.source_generation,
                expected_process_fencing_token: request.source_fencing_token,
                isolation_domain_id: domain.isolation_domain_id,
                isolation_domain_generation: domain.generation,
                isolation_domain_fencing_token: domain.fencing_token,
                idempotency_key: request.restore_idempotency_key,
                restored_at_ms: request.restored_at_ms,
            })
            .map_err(SliceKError::from)?
            .record()
            .clone();

        if restored_binding.process_generation != request.target_generation {
            return Err(ProcessMigrationDriveError::Placement(
                "Target restore_process did not produce expected target_generation",
            ));
        }

        let target_os_pid = self.host.os_process_id();
        self.host
            .supervisor_pids()
            .register(RegisterSupervisorPidRequest {
                process_id: restored_binding.process_id,
                process_generation: restored_binding.process_generation,
                os_pid: target_os_pid,
                registered_at_ms: request.restored_at_ms,
            })
            .map_err(SliceKError::from)?;
        self.host.record_fenced_process_heartbeat(
            restored_binding.process_id,
            restored_binding.process_generation,
        )?;

        let proof = TargetRestoreProof::from_restored_binding(
            &restored_binding,
            *request.target_cell_id.as_bytes(),
            request.restored_image_or_checkpoint_digest,
        );
        Ok(TargetProcessRestoreReport {
            restored_binding,
            proof,
            registered_target_os_pid: target_os_pid,
        })
    }

    fn record_receipt_checkpoint_on_target(
        &mut self,
        receipt: &ProcessMigrationReceipt,
    ) -> Result<TargetReceiptCheckpointReport, ProcessMigrationDriveError> {
        let prefix = process_migration_receipt_checkpoint_prefix(receipt);
        let fence = self.host.fence();
        let digest = CheckpointDigest::from_prefix(
            fence.node_boot_generation(),
            fence.epoch(),
            fence.fencing_token(),
            &prefix,
        );
        let digest_text = digest.to_text();
        let record = self
            .host
            .record_reconciliation_checkpoint(&digest_text)?
            .ok_or(ProcessMigrationDriveError::Placement(
                "target CellHost cannot record ReconciliationCheckpoint without federation root",
            ))?;
        if record.fact().verify_digest(&prefix) != CheckpointDigestVerification::Verified {
            return Err(ProcessMigrationDriveError::Placement(
                "target ReconciliationCheckpoint digest verification failed after write",
            ));
        }
        Ok(TargetReceiptCheckpointReport {
            checkpoint_prefix: prefix,
            checkpoint_digest: digest,
            checkpoint_record: record,
        })
    }
}

/// Orchestrates all eight steps of [`ProcessMigrationCoordinator`] end-to-end
/// across a federated `source` [`CellHost`] endpoint and `target` [`CellHost`]
/// endpoint (`[DIST-MIGRATE-001]`, `[DIST-MIGRATE-002]`, `[LEASE-MIGRATE-001]`).
///
/// # Failure Semantics
///
/// - **Pre-admission (`[DIST-MIGRATE-002]` `RefuseMigration` or placement mismatch)**:
///   fails closed before touching Source's process binding, supervisor PID
///   mapping, `CapabilityNameCache`, or leases.
/// - **Pre-fence failure (Steps 2–4 before `record_source_fence`)**: aborts the
///   plan as [`MigrationAbortOutcome::AbortedPreFence`].
/// - **Post-fence failure (Steps 5–7 after `SourceFenced`)**: quarantines any
///   unsettled Source leases via
///   [`SourceCellMigrationEndpoint::quarantine_unsettled_source_leases`] and
///   transitions [`ProcessMigrationCoordinator`] into
///   [`MigrationAbortOutcome::QuarantinedPostFence`] (`ProcessMigrationStep::Quarantined`),
///   preventing dual-active execution across Source and Target.
///
/// # Errors
///
/// Returns [`ProcessMigrationDriveError`] on any fail-closed refusal.
#[allow(clippy::too_many_lines)]
pub fn drive_cell_host_process_migration(
    source: &mut impl SourceCellMigrationEndpoint,
    target: &mut impl TargetCellMigrationEndpoint,
    request: CellHostProcessMigrationRequest,
) -> Result<CellHostProcessMigrationOutcome, ProcessMigrationDriveError> {
    if request.source_cell_id == request.target_cell_id {
        return Err(ProcessMigrationDriveError::Placement(
            "source_cell_id and target_cell_id must be distinct",
        ));
    }
    let target_generation =
        request
            .source_generation
            .checked_next()
            .ok_or(ProcessMigrationDriveError::Placement(
                "source_generation overflowed u64",
            ))?;

    // Step 1: Federation Placement Check + MigrationIntent + Coordinator Initiate.
    let source_placement =
        source.inspect_source_placement(request.process_id, request.target_cell_id)?;
    if source_placement.cell_id != request.source_cell_id {
        return Err(ProcessMigrationDriveError::Placement(
            "source CellHost identity does not match request.source_cell_id",
        ));
    }
    if !source_placement.both_cells_registered_in_directory {
        return Err(ProcessMigrationDriveError::Placement(
            "both Source and Target CellHosts must be registered in Source's CellDirectory",
        ));
    }
    if source_placement.active_binding.process_generation != request.source_generation {
        return Err(ProcessMigrationDriveError::Placement(
            "source active process generation does not match request.source_generation",
        ));
    }
    if !source_placement.supervisor_registered {
        return Err(ProcessMigrationDriveError::Placement(
            "source process generation is not registered in Source's SupervisorPidRegistry",
        ));
    }

    let target_placement = target.inspect_target_placement(request.source_cell_id)?;
    if target_placement.cell_id != request.target_cell_id {
        return Err(ProcessMigrationDriveError::Placement(
            "target CellHost identity does not match request.target_cell_id",
        ));
    }
    if !target_placement.both_cells_registered_in_directory {
        return Err(ProcessMigrationDriveError::Placement(
            "both Source and Target CellHosts must be registered in Target's CellDirectory",
        ));
    }

    let mut coordinator = ProcessMigrationCoordinator::initiate(InitiateProcessMigrationRequest {
        migration_id: request.migration_id,
        process_id: request.process_id,
        source_cell_id: *request.source_cell_id.as_bytes(),
        target_cell_id: *request.target_cell_id.as_bytes(),
        source_generation: request.source_generation.get(),
        target_generation: target_generation.get(),
        attached_sessions: request.attached_sessions,
        pinned_session_policy: request.pinned_session_policy,
        initiated_at_ms: request.initiated_at_ms,
    })
    .map_err(|source_err| ProcessMigrationDriveError::Protocol {
        step: ProcessMigrationStep::Initiated,
        source: source_err,
    })?;

    let reason = request.custom_intent_reason.unwrap_or_else(|| {
        process_migration_intent_reason(
            request.migration_id,
            request.process_id,
            request.source_generation,
            target_generation,
        )
    });
    let intent = MigrationIntent::new(
        request.source_cell_id,
        request.target_cell_id,
        MigrationObject::from_bytes(*request.process_id.as_bytes()),
        request.source_generation,
        &reason,
    )
    .map_err(SliceKError::from)?;
    let migration_intent_record = source.record_migration_intent(&intent)?;

    // Step 2: Source Quiesce (`Quiesced`).
    let quiesce_proof = match source.quiesce_source_process(&SourceQuiesceRequest {
        process_id: request.process_id,
        source_cell_id: request.source_cell_id,
        source_generation: request.source_generation,
        cancel_scope: request.quiesce.cancel_scope,
        active_fiber_count: request.quiesce.active_fiber_count,
        inflight_operations: request.quiesce.inflight_operations,
        quiesced_at_ms: request.quiesce.quiesced_at_ms,
    }) {
        Ok(proof) => proof,
        Err(cause) => {
            return Err(abort_pre_fence(
                coordinator,
                ProcessMigrationStep::Quiesced,
                cause,
                request.completed_at_ms,
            ));
        }
    };
    if let Err(err) = coordinator.record_quiesce(quiesce_proof) {
        return Err(abort_pre_fence(
            coordinator,
            ProcessMigrationStep::Quiesced,
            ProcessMigrationDriveError::Protocol {
                step: ProcessMigrationStep::Quiesced,
                source: err,
            },
            request.completed_at_ms,
        ));
    }

    // Step 3: Checkpoint / Artifact Restart Image (`Checkpointed`).
    let migration_mode = coordinator.migration_mode().clone();
    let migratable_sessions = coordinator.plan().migratable_sessions.clone();
    let source_checkpoint_bundle =
        match source.capture_source_checkpoint(&SourceCheckpointRequest {
            migration_id: request.migration_id,
            process_id: request.process_id,
            source_cell_id: request.source_cell_id,
            source_generation: request.source_generation,
            migration_mode: migration_mode.clone(),
            migratable_sessions,
            spec: request.checkpoint.clone(),
        }) {
            Ok(bundle) => bundle,
            Err(cause) => {
                return Err(abort_pre_fence(
                    coordinator,
                    ProcessMigrationStep::Checkpointed,
                    cause,
                    request.completed_at_ms,
                ));
            }
        };
    let checkpoint_proof = source_checkpoint_bundle.proof.clone();
    let primary_checkpoint_digest = checkpoint_proof.payload.primary_digest();
    if let Err(cause) = target.verify_checkpoint_on_target(&TargetCheckpointVerifyRequest {
        migration_mode,
        spec: request.checkpoint,
        source_bundle: source_checkpoint_bundle,
        sync_seed: request.artifact_sync_seed,
    }) {
        return Err(abort_pre_fence(
            coordinator,
            ProcessMigrationStep::Checkpointed,
            cause,
            request.completed_at_ms,
        ));
    }
    if let Err(err) = coordinator.record_checkpoint(checkpoint_proof) {
        return Err(abort_pre_fence(
            coordinator,
            ProcessMigrationStep::Checkpointed,
            ProcessMigrationDriveError::Protocol {
                step: ProcessMigrationStep::Checkpointed,
                source: err,
            },
            request.completed_at_ms,
        ));
    }

    // Step 4: Source Fencing (`SourceFenced`).
    let source_fence_report = match source.fence_source_process(&SourceFenceRequest {
        process_id: request.process_id,
        source_cell_id: request.source_cell_id,
        source_generation: request.source_generation,
        invalidate_name_paths: request.source_fence.invalidate_name_paths,
        terminate_idempotency_key: request.source_fence.terminate_idempotency_key,
        stale_dispatch_probe_fiber_id: request.source_fence.stale_dispatch_probe_fiber_id,
        stale_dispatch_probe_key: request.source_fence.stale_dispatch_probe_key,
        fenced_at_ms: request.source_fence.fenced_at_ms,
    }) {
        Ok(report) => report,
        Err(cause) => {
            return Err(abort_pre_fence(
                coordinator,
                ProcessMigrationStep::SourceFenced,
                cause,
                request.completed_at_ms,
            ));
        }
    };
    let source_fencing_token = source_fence_report.proof.source_fencing_token;
    if let Err(err) = coordinator.record_source_fence(source_fence_report.proof.clone()) {
        return Err(abort_pre_fence(
            coordinator,
            ProcessMigrationStep::SourceFenced,
            ProcessMigrationDriveError::Protocol {
                step: ProcessMigrationStep::SourceFenced,
                source: err,
            },
            request.completed_at_ms,
        ));
    }

    // =========================================================================
    // POST-FENCE BARRIER: Source `(process_id, source_generation)` is now fenced!
    // Any failure in Steps 5, 6, or 7 MUST quarantine unsettled Source leases
    // and transition `coordinator` to `QuarantinedPostFence`.
    // =========================================================================

    // If an ExclusiveDeviceLease will migrate with `advance_target_epoch_if_needed`,
    // ensure Target's epoch advances BEFORE Step 5 inserts into Target's NameCache!
    let ensure_target_epoch_above = request.leases.device.and_then(|dev| {
        dev.advance_target_epoch_if_needed
            .then_some(source_placement.epoch)
    });

    let source_revocations: Vec<SignedRevokeCapabilityRequest> = request
        .capabilities
        .iter()
        .filter_map(|item| item.revoke_on_source)
        .collect();

    // Step 5: Capability Rebind (`CapabilitiesRebound`).
    let capability_rebind_report =
        match target.rebind_capabilities_on_target(&TargetCapabilityRebindRequest {
            process_id: request.process_id,
            source_cell_id: request.source_cell_id,
            source_generation: request.source_generation,
            target_cell_id: request.target_cell_id,
            target_generation,
            ensure_target_epoch_above,
            items: request.capabilities,
            rebound_at_ms: request.capabilities_rebound_at_ms,
        }) {
            Ok(report) => report,
            Err(cause) => {
                return Err(abort_post_fence(
                    source,
                    &request.leases,
                    coordinator,
                    ProcessMigrationStep::CapabilitiesRebound,
                    cause,
                    request.completed_at_ms,
                ));
            }
        };
    if let Err(err) = coordinator.record_capability_rebind(capability_rebind_report.proof.clone()) {
        return Err(abort_post_fence(
            source,
            &request.leases,
            coordinator,
            ProcessMigrationStep::CapabilitiesRebound,
            ProcessMigrationDriveError::Protocol {
                step: ProcessMigrationStep::CapabilitiesRebound,
                source: err,
            },
            request.completed_at_ms,
        ));
    }

    let source_revocation_receipts = if source_revocations.is_empty() {
        Vec::new()
    } else {
        match source.revoke_source_capabilities(&source_revocations) {
            Ok(receipts) => receipts,
            Err(cause) => {
                return Err(abort_post_fence(
                    source,
                    &request.leases,
                    coordinator,
                    ProcessMigrationStep::CapabilitiesRebound,
                    cause,
                    request.completed_at_ms,
                ));
            }
        }
    };

    // Step 6: Lease Close on Source -> Regrant on Target (`LeasesRegranted`,
    // `[LEASE-MIGRATE-001]`).
    let source_lease_close_report = match source.close_or_fence_source_leases(&request.leases) {
        Ok(report) => report,
        Err(cause) => {
            return Err(abort_post_fence(
                source,
                &request.leases,
                coordinator,
                ProcessMigrationStep::LeasesRegranted,
                cause,
                request.completed_at_ms,
            ));
        }
    };
    let target_lease_regrant_report =
        match target.regrant_leases_on_target(&TargetLeaseRegrantRequest {
            process_id: request.process_id,
            source_cell_id: request.source_cell_id,
            source_generation: request.source_generation,
            target_cell_id: request.target_cell_id,
            target_generation,
            spec: request.leases,
            source_closed: source_lease_close_report,
        }) {
            Ok(report) => report,
            Err(cause) => {
                return Err(abort_post_fence(
                    source,
                    &request.leases,
                    coordinator,
                    ProcessMigrationStep::LeasesRegranted,
                    cause,
                    request.completed_at_ms,
                ));
            }
        };
    if let Err(err) =
        coordinator.record_lease_close_regrant(target_lease_regrant_report.proof.clone())
    {
        return Err(abort_post_fence(
            source,
            &request.leases,
            coordinator,
            ProcessMigrationStep::LeasesRegranted,
            ProcessMigrationDriveError::Protocol {
                step: ProcessMigrationStep::LeasesRegranted,
                source: err,
            },
            request.completed_at_ms,
        ));
    }

    // Step 7: Target Restore & New Generation (`TargetRestored`).
    let target_restore_report =
        match target.restore_process_on_target(&TargetProcessRestoreRequest {
            process_id: request.process_id,
            target_cell_id: request.target_cell_id,
            source_generation: request.source_generation,
            source_fencing_token,
            target_generation,
            lineage: request.lineage,
            existing_target_domain_id: request.target_restore.existing_target_domain_id,
            target_domain_policy_digest: request.target_restore.domain_policy_digest,
            target_domain_idempotency_key: request.target_restore.domain_idempotency_key,
            restore_idempotency_key: request.target_restore.restore_idempotency_key,
            restored_image_or_checkpoint_digest: primary_checkpoint_digest,
            restored_at_ms: request.target_restore.restored_at_ms,
        }) {
            Ok(report) => report,
            Err(cause) => {
                return Err(abort_post_fence(
                    source,
                    &request.leases,
                    coordinator,
                    ProcessMigrationStep::TargetRestored,
                    cause,
                    request.completed_at_ms,
                ));
            }
        };
    if let Err(err) = coordinator.record_target_restore(target_restore_report.proof.clone()) {
        return Err(abort_post_fence(
            source,
            &request.leases,
            coordinator,
            ProcessMigrationStep::TargetRestored,
            ProcessMigrationDriveError::Protocol {
                step: ProcessMigrationStep::TargetRestored,
                source: err,
            },
            request.completed_at_ms,
        ));
    }

    // Step 8: Seal `ProcessMigrationReceipt` & Record Target `ReconciliationCheckpoint`.
    let receipt = coordinator
        .complete(request.completed_at_ms)
        .map_err(|err| ProcessMigrationDriveError::Protocol {
            step: ProcessMigrationStep::Completed,
            source: err,
        })?;
    if !receipt.verify_digest() {
        return Err(ProcessMigrationDriveError::Placement(
            "sealed ProcessMigrationReceipt failed self-digest verification",
        ));
    }
    let target_checkpoint_report = target.record_receipt_checkpoint_on_target(&receipt)?;

    Ok(CellHostProcessMigrationOutcome {
        migration_intent_record,
        source_fence_report,
        capability_rebind_report,
        source_lease_close_report,
        target_lease_regrant_report,
        source_revocation_receipts,
        target_restore_report,
        receipt,
        target_checkpoint_report,
        coordinator,
    })
}

fn abort_pre_fence(
    mut coordinator: ProcessMigrationCoordinator,
    failed_step: ProcessMigrationStep,
    cause: ProcessMigrationDriveError,
    aborted_at_ms: u64,
) -> ProcessMigrationDriveError {
    let reason = format!("pre-fence failure at {failed_step:?}: {cause}");
    match coordinator.abort_before_target_restore(reason, aborted_at_ms) {
        Ok(abort_outcome) => ProcessMigrationDriveError::AbortedPreFence {
            failed_step,
            cause: Box::new(cause),
            abort_outcome,
            coordinator: Box::new(coordinator),
        },
        Err(proto_err) => ProcessMigrationDriveError::Protocol {
            step: failed_step,
            source: proto_err,
        },
    }
}

fn abort_post_fence(
    source: &mut impl SourceCellMigrationEndpoint,
    leases: &LeaseMigrationBundleSpec,
    mut coordinator: ProcessMigrationCoordinator,
    failed_step: ProcessMigrationStep,
    cause: ProcessMigrationDriveError,
    aborted_at_ms: u64,
) -> ProcessMigrationDriveError {
    let source_quarantine = source.quarantine_unsettled_source_leases(leases).ok();
    let reason = format!("post-fence failure at {failed_step:?}: {cause}");
    match coordinator.abort_before_target_restore(reason, aborted_at_ms) {
        Ok(abort_outcome) => ProcessMigrationDriveError::QuarantinedPostFence {
            failed_step,
            cause: Box::new(cause),
            abort_outcome,
            coordinator: Box::new(coordinator),
            source_quarantine,
        },
        Err(proto_err) => ProcessMigrationDriveError::Protocol {
            step: failed_step,
            source: proto_err,
        },
    }
}

fn sync_bundle_into_target_artifact_store(
    target_store: &nlos_artifact::ArtifactStore,
    bundle: &ReplicaRevisionBundle,
    sync_seed: u8,
    synced_at_ms: u64,
) -> Result<(), ProcessMigrationDriveError> {
    if target_store.inspect_artifact(bundle.artifact_id).is_err() {
        target_store
            .create_artifact(CreateArtifactSpec {
                artifact_id: bundle.artifact_id,
                idempotency_key: crate::seeded_key(sync_seed, 201),
                content_type: bundle.content_type.clone(),
                application_id: None,
                owner: None,
                created_at_ms: synced_at_ms,
            })
            .map_err(SliceKError::from)?;
    }

    // If `bundle` has ancestor revisions (`1..bundle.remote_head.revision`) that
    // are not yet in `target_store` and `bundle` itself is revision 1, we use
    // `[DIST-ART-001]` `sync_artifact_replica_head`; if `revision > 1` on a fresh
    // store without intermediate payloads, fail closed through `sync_artifact_replica_head`.
    let mut registry = ArtifactSyncPolicyRegistry::new();
    registry
        .register(ArtifactSyncPolicy {
            content_type: bundle.content_type.clone(),
            merge_strategy: ArtifactMergeStrategy::ImmutableContentAddressed,
            conflict_strategy: ArtifactConflictStrategy::FailClosedConflict,
            replica_authority: ArtifactReplicaAuthority::SymmetricPeer,
            deletion_semantics: ArtifactDeletionSemantics::Forbidden,
        })
        .map_err(|err| {
            ProcessMigrationDriveError::CheckpointVerification(format!(
                "artifact sync policy registration failed: {err}"
            ))
        })?;

    let outcome = sync_artifact_replica_head(
        target_store,
        &registry,
        SyncArtifactReplicaRequest {
            bundle,
            task_id: TaskId::from_bytes([sync_seed.wrapping_add(202); 16]),
            permit_id: CommitPermitId::from_bytes([sync_seed.wrapping_add(203); 16]),
            write_set_root: ContentDigest::of_bytes(&bundle.bytes),
            idempotency_key: crate::seeded_key(sync_seed, 204),
            synced_at_ms,
        },
    )
    .map_err(|err| {
        ProcessMigrationDriveError::CheckpointVerification(format!(
            "artifact replica sync failed: {err}"
        ))
    })?;

    match outcome {
        ArtifactReplicaSyncOutcome::Synced { .. }
        | ArtifactReplicaSyncOutcome::NoOpAlreadySynced { .. } => Ok(()),
        ArtifactReplicaSyncOutcome::RefusedConflict(conflict)
        | ArtifactReplicaSyncOutcome::QuarantinedConflict(conflict) => {
            Err(ProcessMigrationDriveError::CheckpointVerification(format!(
                "target artifact sync refused divergent conflict {:?}",
                conflict.conflict_digest
            )))
        }
    }
}

/// Convenience helper to commit a restart-image revision directly into a
/// [`CellHost`]'s `ArtifactStore` (`create_artifact` + `put_revision`) and
/// return its [`ContentDigest`].
///
/// # Errors
///
/// Propagates [`SliceKError`] if artifact creation or revision commit fails.
pub fn publish_restart_image_artifact(
    host: &CellHost,
    artifact_id: ArtifactId,
    seed: u8,
    content_type: &str,
    image_bytes: &[u8],
    created_at_ms: u64,
) -> Result<(u64, ContentDigest), SliceKError> {
    host.runtime()
        .artifacts
        .create_artifact(CreateArtifactSpec {
            artifact_id,
            idempotency_key: crate::seeded_key(seed, 180),
            content_type: content_type.to_string(),
            application_id: None,
            owner: None,
            created_at_ms,
        })?;
    let decision = host.runtime().artifacts.put_revision(PutRevisionRequest {
        artifact_id,
        expected_head_revision: 0,
        bytes: image_bytes,
        created_at_ms,
        provenance: crate::package::provenance_triple(seed),
    })?;
    let record = decision.record();
    Ok((record.revision, record.digest))
}

/// Convenience helper to verify whether a [`CapabilityHandle`] is currently
/// active on `host`'s `CapabilityAuthority` at `at_ms`.
///
/// # Errors
///
/// Propagates [`CapabilityAuthorityError`].
pub fn inspect_host_active_capability(
    host: &CellHost,
    handle: CapabilityHandle,
    at_ms: u64,
) -> Result<CapabilityRecord, CapabilityAuthorityError> {
    host.runtime().capability().inspect_active(handle, at_ms)
}

fn append_hex(out: &mut String, bytes: &[u8]) {
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
}
