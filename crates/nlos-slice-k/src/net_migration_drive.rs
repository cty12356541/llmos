//! Cross-socket TLS 1.3 mTLS 8-step process migration (`[DIST-MIGRATE-001/002]`,
//! `[LEASE-MIGRATE-001]`), post-`SourceFenced` network-cut reconciliation
//! (`[DIST-FAIL-001]`), and `N = 3` Raft quorum-backed 6-endpoint `DIST-TASK-002`
//! authority takeover (`ADR-0026`, `W71-C-SYNC-MIGRATE`).
//!
//! All communication between [`NetCellHost`] instances executes strictly over
//! `nlos-net` TLS 1.3 mTLS sockets (`NLOSMG01` migration RPCs, `NLOSTB01`
//! takeover & quorum attestation RPCs, `NLOSSY01` semantic vector-checkpoint
//! delta sync, and `NLOSAT01`/`NLOSAB01` resumable chunked artifact streaming),
//! with **zero** shared filesystem paths between hosts.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::fs;
use std::future::Future;
use std::net::SocketAddr;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use ed25519_dalek::{Signer, SigningKey};
use nlos_artifact::{
    ArtifactNetTransferClient, ArtifactTransferExecutionSummary, ExportArtifactTransferRequest,
    export_artifact_transfer_package,
};
use nlos_capability::{
    CapabilityHandle, CapabilityIssueReceipt, CapabilityRecord, CapabilityRevocationReceipt,
    CapabilityRights, CapabilityTarget, DelegateCapabilityRequest, IssueRootCapabilityRequest,
    SignedDelegateCapabilityRequest, SignedIssueRootCapabilityRequest,
};
use nlos_cell::{
    CellEpoch, CellFence, CellFencingToken, CellIdentity, CellRegistrationEntry, CheckpointDigest,
    CheckpointDigestVerification, CheckpointFact, CheckpointPrefix, FederationError,
    MigrationIntent, MigrationIntentRecord, MigrationObject, NamePath, RemoteCallContract,
    RemoteCallOutcome,
};
use nlos_consensus::{
    AppliedProposalOutcome, ConsensusCommitReceipt, ConsensusError, ConsensusRole,
    ControlPlaneAuditCommand, ControlPlaneCommand, ControlPlaneDomainRejection,
    ControlPlaneProposal, MultiHostQuorumNode, ProposalRejectReason, TaskAuthorityShardCommand,
};
use nlos_identity::{BootstrapPrincipalRequest, Ed25519Signature, KeyPurpose};
use nlos_lease::{
    CapacityLeaseState, CapacityReconciliationReceipt, CleanupAttestation,
    DeviceReconciliationReceipt, DeviceResetReceipt, ExclusiveDeviceLeaseState, FenceBarrierAck,
    QuotaLeaseState, QuotaReconciliationReceipt,
};
use nlos_net::{
    CellRpcTransport, EndpointHintRecord, EndpointResolver, EndpointRoutingVersion,
    IncomingRpcRequest, MonotonicEndpointResolver, MtlsCellTransport, PendingReconciliationTracker,
    RpcHandlerReply, RpcWireStatus, SocketFailureKind,
};
use nlos_process::{
    CapabilityRebindProof, CheckpointPayloadKind, InitiateProcessMigrationRequest,
    LeaseCloseRegrantProof, MigratedLeaseEntry, MigrationMode, ProcessAuthorityError,
    ProcessBindingRecord, ProcessCheckpointProof, ProcessLifecycleState,
    ProcessMigrationCoordinator, ProcessMigrationReceipt, ProcessMigrationStep,
    ReboundCapabilityEntry, TargetRestoreProof,
};
use nlos_runtime_tokio::{TokioRuntimeAdapter, TokioRuntimeConfig};
use nlos_semantic::net_sync::{SemanticNetSyncClient, SemanticNetSyncConfig};
use nlos_takeover_control::{
    CommittedControlIndexGate, ConsensusBackingMode, ConsensusIndexGateError, ControlCommitSubject,
    QuorumCommitAttestation, VerifiedControlCommitProof, VoterCommitAttestation,
    quorum_commit_attestation_digest,
};
use nlos_task::{
    AttemptSpec, AuthorityAssignmentRecord, AuthorityAssignmentState, AuthorityLeaseCloseRequest,
    AuthorityLeaseDecision, AuthorityLeasePermitRequest, AuthorityLeaseRecord,
    AuthorityLeaseRequest, AuthorityLeaseTakeoverFenceRequest,
    AuthoritySuccessorRegistryReopenRequest, AuthorityTakeoverBarrierCoverageState,
    AuthorityTakeoverBarrierReceiptRequest, AuthorityTakeoverReceiptState,
    BarrierObservationSignature, ClosePermitDecision, ClosePermitRequest,
    CompleteAuthorityTakeoverRequest, DistTaskOperationAdmissionRequest, ParticipantRecord,
    ParticipantRegistryBinding, ParticipantRegistryState, ParticipantType, PermitClosureOutcome,
    PermitClosureRegistryReopenRequest, PermitDecision, PermitRequest, SnapshotBundle,
    SqliteTaskAuthority, TakeoverEndpointBarrierAuditReport, TaskSpec, TaskStoreError,
    barrier_observation_signature_message, empty_effect_history_root,
};
use nlos_types::{
    AgentInstanceId, ArtifactId, CancellationScopeId, CapabilityId, CapacityLeaseId,
    CommitPermitId, ControlDomainId, DeviceId, ExclusiveDeviceLeaseId, Generation, IdempotencyKey,
    IsolationDomainId, KeyId, NamespaceId, PrincipalId, ProcessId, QuotaLeaseId, ReceiptId,
    SchedulerDomainId, TaskAttemptId, TaskAuthorityAssignmentId, TaskId, TaskParticipantId,
    TaskSnapshotId,
};
use sha2::{Digest, Sha256};

use crate::cell_host::CellHost;
use crate::error::SliceKError;
use crate::net_cell_host::{
    NetCellHost, NetCellHostInspector, NetCellSemanticSyncBridge, NetConsensusBackend,
    SubProtocolRpcHandler,
};
use crate::process_migration_drive::{
    CapabilityRebindItemSpec, CapacityLeaseMigrationSpec, CellHostProcessMigrationRequest,
    CheckpointSourceSpec, DeviceLeaseMigrationSpec, LeaseMigrationBundleSpec,
    LocalCellHostSourceEndpoint, LocalCellHostTargetEndpoint, ProcessLineageGenesis,
    ProcessLineageRestoreHop, ProcessLineageSpec, ProcessMigrationDriveError,
    QuotaLeaseMigrationSpec, SourceCellMigrationEndpoint, SourceCheckpointBundle,
    SourceCheckpointRequest, SourceClosedQuotaSummary, SourceFenceReport, SourceFenceRequest,
    SourceLeaseCloseReport, SourcePostFenceQuarantineReport, SourceQuiesceRequest,
    SourceReturnedCapacitySummary, SourceReturnedDeviceSummary, TargetCapabilityRebindReport,
    TargetCapabilityRebindRequest, TargetCellMigrationEndpoint, TargetCellPlacementSnapshot,
    TargetCheckpointVerifyRequest, TargetLeaseRegrantRequest, TargetProcessRestoreReport,
    TargetProcessRestoreRequest, process_migration_intent_reason,
};
use crate::runtime::seeded_key;
use crate::takeover_drive::{
    BarrierObservationDemand, BarrierObservationSource, CrossCellTakeoverOutcome,
    SignedBarrierObservation, TakeoverDriveError, TakeoverFederationBindingOutcome,
    TakeoverFederationContext, bind_takeover_to_federation,
};

/// 8-byte wire magic for cross-socket process migration RPC frames.
pub const NET_MIGRATION_RPC_MAGIC: &[u8; 8] = b"NLOSMG01";

/// 8-byte wire magic for cross-socket quorum takeover & barrier RPC frames.
pub const NET_TAKEOVER_RPC_MAGIC: &[u8; 8] = b"NLOSTB01";

/// Default chunk payload size (`16 KiB <= 64 KiB`) used when streaming restart
/// artifacts during cross-socket process migration.
pub const DEFAULT_MIGRATION_ARTIFACT_CHUNK_BYTES: u32 = 16 * 1024;

const STEP_INSPECT_TARGET: u8 = 1;
const STEP_PULL_SEMANTIC: u8 = 2;
const STEP_VERIFY_CHECKPOINT: u8 = 3;
const STEP_REBIND_CAPABILITIES: u8 = 4;
const STEP_REGRANT_LEASES: u8 = 5;
const STEP_RESTORE_PROCESS: u8 = 6;
const STEP_RECORD_RECEIPT_CKPT: u8 = 7;
const STEP_INSPECT_STATE: u8 = 8;

/// Builds the default `NLOSMG01` and `NLOSTB01` sub-protocol RPC servers mounted
/// on every networked [`NetCellHost`].
#[must_use]
pub fn build_default_net_host_sub_handlers(
    host: Arc<Mutex<CellHost>>,
    transport: Arc<MtlsCellTransport>,
    resolver: Arc<MonotonicEndpointResolver>,
    semantic_bridge: Arc<NetCellSemanticSyncBridge>,
    inspector: NetCellHostInspector,
    consensus: &NetConsensusBackend,
) -> (Arc<NetMigrationRpcServer>, Arc<NetTakeoverRpcServer>) {
    let quorum_node = match consensus {
        NetConsensusBackend::SingleHostPassthrough(_) => None,
        NetConsensusBackend::MultiHostQuorum(node) => Some(Arc::clone(node)),
    };
    let migration_server = Arc::new(NetMigrationRpcServer::new(
        Arc::clone(&host),
        transport,
        resolver,
        semantic_bridge,
        inspector,
    ));
    let takeover_server = Arc::new(NetTakeoverRpcServer::new(host, quorum_node));
    (migration_server, takeover_server)
}

/// Summary of semantic vector-checkpoint delta pull & 3-part provenance merge
/// executed during cross-socket process migration (`[SEM-CHECKPOINT-001]`,
/// `[DIST-AUDIT-001]`, `[DIST-EVENT-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CrossSocketSemanticSyncSummary {
    /// Number of bounded event delta batches pulled over `NLOSSY01`.
    pub batches_pulled: u32,
    /// Total `CrossDomainEventItem`s transferred over the wire.
    pub pulled_event_count: u32,
    /// Events newly imported into Target's `SemanticAuthority`.
    pub imported_events: u32,
    /// Post-sync Target checkpoint digest.
    pub target_checkpoint_digest: [u8; 32],
    /// Verified Source checkpoint digest.
    pub source_checkpoint_digest: [u8; 32],
}

/// Transport-constructible summary of Step 8 federation checkpointing on Target
/// (`[DIST-MIGRATE-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteTargetCheckpointSummary {
    /// Target `CellIdentity`.
    pub target_cell_id: CellIdentity,
    /// Canonical `PMIGRCPT` [`CheckpointPrefix`] committing `receipt.receipt_digest`.
    pub checkpoint_prefix: CheckpointPrefix,
    /// Structured [`CheckpointDigest`] over Target's fence and `checkpoint_prefix`.
    pub checkpoint_digest: CheckpointDigest,
    /// Reconstructible [`CheckpointFact`] recorded in Target's private `CellDirectory`.
    pub checkpoint_fact: CheckpointFact,
    /// Timestamp recorded on Target's checkpoint file.
    pub recorded_ms: u64,
    /// Target OS PID recorded on Target's checkpoint file.
    pub os_process_id: u32,
    /// Sequence recorded on Target's checkpoint file.
    pub sequence: u64,
}

impl RemoteTargetCheckpointSummary {
    /// Re-verifies `checkpoint_fact` against `checkpoint_prefix` fail-closed.
    #[must_use]
    pub fn verify_digest(&self) -> bool {
        self.checkpoint_fact.verify_digest(&self.checkpoint_prefix)
            == CheckpointDigestVerification::Verified
    }
}

/// Read-only snapshot of a migrating process on Target used for post-cut safety
/// assertions and `[DIST-FAIL-001]` reconcile-before-retry inspection.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TargetMigrationInspectionSnapshot {
    /// Target `CellIdentity`.
    pub target_cell_id: CellIdentity,
    /// Active generation on Target if `process_id` has already been restored.
    pub active_generation: Option<Generation>,
    /// Lifecycle state on Target if `process_id` exists in Target's `ProcessAuthority`.
    pub lifecycle_state: Option<ProcessLifecycleState>,
    /// Whether `(process_id, active_generation)` is registered in Target's supervisor.
    pub supervisor_registered: bool,
    /// Recorded `PMIGRCPT` `receipt_digest` in Target's `CellDirectory` if Step 8 completed.
    pub recorded_receipt_digest: Option<[u8; 32]>,
}

/// Tunable transport options for [`drive_cross_socket_process_migration`].
pub struct CrossSocketProcessMigrationOptions {
    /// Chunk payload slice size (`1..=64 KiB`) for `ArtifactNetTransferClient`.
    pub artifact_chunk_payload_bytes: u32,
    /// Whether Step 3 also pulls & merges Source's semantic provenance trail onto Target.
    pub sync_semantic_provenance: bool,
    /// Bounded batch size for semantic delta pulls.
    pub semantic_max_batch_events: u32,
    /// Per-RPC deadline in ticks/milliseconds.
    pub rpc_deadline_ms: u64,
    /// Optional deterministic hook invoked immediately after Step 4 (`SourceFenced`)
    /// commits on Source and BEFORE Step 5 (`CapabilitiesRebound`) dials Target.
    pub after_source_fenced_hook: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
}

impl fmt::Debug for CrossSocketProcessMigrationOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CrossSocketProcessMigrationOptions")
            .field(
                "artifact_chunk_payload_bytes",
                &self.artifact_chunk_payload_bytes,
            )
            .field("sync_semantic_provenance", &self.sync_semantic_provenance)
            .field("semantic_max_batch_events", &self.semantic_max_batch_events)
            .field("rpc_deadline_ms", &self.rpc_deadline_ms)
            .field(
                "has_after_source_fenced_hook",
                &self.after_source_fenced_hook.is_some(),
            )
            .finish()
    }
}

impl Default for CrossSocketProcessMigrationOptions {
    fn default() -> Self {
        Self {
            artifact_chunk_payload_bytes: DEFAULT_MIGRATION_ARTIFACT_CHUNK_BYTES,
            sync_semantic_provenance: true,
            semantic_max_batch_events: 32,
            rpc_deadline_ms: 5_000,
            after_source_fenced_hook: None,
        }
    }
}

/// Transport-constructible summary of Step 6 (`LeasesRegranted`) on Target
/// (`[LEASE-MIGRATE-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteTargetLeaseRegrantSummary {
    /// Step 6 proof recorded in [`ProcessMigrationCoordinator`].
    pub proof: LeaseCloseRegrantProof,
    /// Newly granted Target `QuotaLeaseId`.
    pub target_quota_lease_id: QuotaLeaseId,
    /// Face value of Target's newly granted `QuotaLease`.
    pub target_quota_face_value: u64,
    /// Grant epoch on Target's `QuotaLease`.
    pub target_quota_epoch: CellEpoch,
    /// Whether Target's `QuotaLease` is in `Active` state.
    pub target_quota_active: bool,
    /// Optional newly granted Target `(CapacityLeaseId, amount, epoch)`.
    pub target_capacity: Option<(CapacityLeaseId, u64, CellEpoch)>,
    /// Optional newly granted Target `(ExclusiveDeviceLeaseId, DeviceId, exclusivity_epoch, reset_generation)`.
    pub target_device: Option<(ExclusiveDeviceLeaseId, DeviceId, CellEpoch, Generation)>,
}

/// Complete end-to-end outcome of [`drive_cross_socket_process_migration`] or
/// [`reconcile_post_fence_interrupted_migration`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CrossSocketProcessMigrationOutcome {
    /// Immutable [`MigrationIntentRecord`] appended to Source's local `CellDirectory`.
    pub migration_intent_record: MigrationIntentRecord,
    /// Optional chunked artifact streaming summary when Step 3 streamed a restart image.
    pub artifact_transfer_summary: Option<ArtifactTransferExecutionSummary>,
    /// Optional semantic vector-checkpoint delta sync summary from Step 3.
    pub semantic_sync_summary: Option<CrossSocketSemanticSyncSummary>,
    /// Verified Source fencing report from Step 4.
    pub source_fence_report: SourceFenceReport,
    /// Verified Target capability rebind report from Step 5.
    pub capability_rebind_report: TargetCapabilityRebindReport,
    /// Verified Source lease close/return report from Step 6.
    pub source_lease_close_report: SourceLeaseCloseReport,
    /// Verified Target lease regrant summary from Step 6.
    pub target_lease_regrant_summary: RemoteTargetLeaseRegrantSummary,
    /// Optional capability revocation receipts committed on Source after Step 5.
    pub source_revocation_receipts: Vec<CapabilityRevocationReceipt>,
    /// Verified Target process restore report (`target_generation == source_generation + 1`).
    pub target_restore_report: TargetProcessRestoreReport,
    /// Sealed Step 8 [`ProcessMigrationReceipt`] (`verify_digest() == true`).
    pub receipt: ProcessMigrationReceipt,
    /// Target's Step 8 federation checkpoint summary (`verify_digest() == true`).
    pub target_checkpoint_summary: RemoteTargetCheckpointSummary,
    /// Consensus commit receipt anchoring the migration audit record on Source.
    pub consensus_audit_commit: ConsensusCommitReceipt,
    /// Completed [`ProcessMigrationCoordinator`] at [`ProcessMigrationStep::Completed`].
    pub coordinator: ProcessMigrationCoordinator,
}

/// Captures the quarantined, zero-dual-live state after an mTLS disconnect
/// interrupts migration AFTER Step 4 (`SourceFenced`) and before `Completed`,
/// allowing deterministic reconnection reconciliation via
/// [`reconcile_post_fence_interrupted_migration`].
#[derive(Debug)]
pub struct PostFenceInterruptedMigration {
    /// Original migration request (retaining all deterministic step parameters).
    pub request: CellHostProcessMigrationRequest,
    /// Recorded Step 1 `MigrationIntentRecord` on Source.
    pub migration_intent_record: MigrationIntentRecord,
    /// Source epoch prior to post-fence quarantine.
    pub pre_fence_source_epoch: CellEpoch,
    /// Optional Step 3 chunked artifact transfer summary already completed pre-fence.
    pub artifact_transfer_summary: Option<ArtifactTransferExecutionSummary>,
    /// Optional Step 3 semantic sync summary already completed pre-fence.
    pub semantic_sync_summary: Option<CrossSocketSemanticSyncSummary>,
    /// Step 2 proof already verified before the cut.
    pub quiesce_proof: nlos_process::QuiesceProof,
    /// Step 3 proof already verified before the cut.
    pub checkpoint_proof: ProcessCheckpointProof,
    /// Step 4 report proving Source `(process_id, source_generation)` is durably fenced.
    pub source_fence_report: SourceFenceReport,
    /// Step at which the mTLS network cut occurred (`CapabilitiesRebound`,
    /// `LeasesRegranted`, `TargetRestored`, or `Completed`).
    pub interrupted_step: ProcessMigrationStep,
    /// Deterministic `IdempotencyKey` of the interrupted remote mutation.
    pub interrupted_idempotency_key: IdempotencyKey,
    /// Quarantine states enforced on Source's unsettled 3-family leases.
    pub source_quarantine: SourcePostFenceQuarantineReport,
    /// `[DIST-FAIL-001]` tracker holding the uncertain remote mutation until reconciled.
    pub pending_tracker: PendingReconciliationTracker,
    /// Coordinator transitioned to [`ProcessMigrationStep::Quarantined`].
    pub quarantined_coordinator: ProcessMigrationCoordinator,
    /// Underlying transport disconnect details.
    pub transport_reason: String,
    /// Per-RPC deadline to reuse on reconnection.
    pub rpc_deadline_ms: u64,
}

/// Fail-closed error taxonomy for cross-socket process migration.
#[derive(Debug)]
pub enum CrossSocketMigrationError {
    /// Underlying process-migration protocol, capability, or lease error.
    Drive(Box<ProcessMigrationDriveError>),
    /// Chunked `ArtifactTransferPackage` streaming failed pre-fence.
    ArtifactTransfer(String),
    /// Signed vector-checkpoint semantic delta sync failed pre-fence.
    SemanticSync(String),
    /// `nlos-consensus` audit anchor commit failed.
    Consensus(ConsensusError),
    /// Network cut or uncertain outcome occurred AFTER `SourceFenced`; Source is
    /// fenced, unsettled leases are `Quarantined`, and Target has NOT prematurely
    /// activated execution. Reconnect and pass to [`reconcile_post_fence_interrupted_migration`].
    PostFenceNetworkCut(Box<PostFenceInterruptedMigration>),
}

impl fmt::Display for CrossSocketMigrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Drive(err) => write!(formatter, "{err}"),
            Self::ArtifactTransfer(msg) => {
                write!(formatter, "cross-socket artifact transfer failed: {msg}")
            }
            Self::SemanticSync(msg) => {
                write!(formatter, "cross-socket semantic sync failed: {msg}")
            }
            Self::Consensus(err) => write!(formatter, "consensus commit failed: {err}"),
            Self::PostFenceNetworkCut(interrupted) => write!(
                formatter,
                "post-SourceFenced mTLS network cut at {:?} (zero dual-live; leases quarantined): {}",
                interrupted.interrupted_step, interrupted.transport_reason
            ),
        }
    }
}

impl Error for CrossSocketMigrationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Drive(err) => Some(err),
            Self::Consensus(err) => Some(err),
            Self::ArtifactTransfer(_) | Self::SemanticSync(_) | Self::PostFenceNetworkCut(_) => {
                None
            }
        }
    }
}

impl From<ProcessMigrationDriveError> for CrossSocketMigrationError {
    fn from(value: ProcessMigrationDriveError) -> Self {
        Self::Drive(Box::new(value))
    }
}

impl From<ConsensusError> for CrossSocketMigrationError {
    fn from(value: ConsensusError) -> Self {
        Self::Consensus(value)
    }
}

fn derive_migration_rpc_key(migration_id: [u8; 16], step_code: u8) -> IdempotencyKey {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/net-migration-rpc-idempotency/v1");
    hasher.update(migration_id);
    hasher.update([step_code]);
    let digest: [u8; 32] = hasher.finalize().into();
    let mut raw = [0_u8; 16];
    raw.copy_from_slice(&digest[..16]);
    IdempotencyKey::from_bytes(raw)
}

fn build_rpc_contract(
    idempotency_key: IdempotencyKey,
    deadline_ms: u64,
    side_effecting: bool,
) -> RemoteCallContract {
    RemoteCallContract::new(
        idempotency_key.into_bytes(),
        idempotency_key,
        deadline_ms.max(1),
        3,
        0,
        side_effecting,
    )
}

// ============================================================================
// Binary Wire Codec for `NLOSMG01` & `NLOSTB01`
// ============================================================================

struct WireWriter {
    buf: Vec<u8>,
}

impl WireWriter {
    #[allow(clippy::trivially_copy_pass_by_ref)]
    fn new(magic: &[u8; 8], op_tag: u8) -> Self {
        let mut buf = Vec::with_capacity(256);
        buf.extend_from_slice(magic);
        buf.push(op_tag);
        Self { buf }
    }

    fn into_vec(self) -> Vec<u8> {
        self.buf
    }

    fn put_u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    fn put_bool(&mut self, v: bool) {
        self.put_u8(u8::from(v));
    }

    fn put_u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    fn put_u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    fn put_i64(&mut self, v: i64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    fn put_arr16(&mut self, v: &[u8; 16]) {
        self.buf.extend_from_slice(v);
    }

    fn put_arr32(&mut self, v: &[u8; 32]) {
        self.buf.extend_from_slice(v);
    }

    fn put_arr64(&mut self, v: &[u8; 64]) {
        self.buf.extend_from_slice(v);
    }

    fn put_bytes(&mut self, v: &[u8]) {
        let len = u64::try_from(v.len()).unwrap_or(0);
        self.put_u64(len);
        self.buf.extend_from_slice(v);
    }

    fn put_str(&mut self, s: &str) {
        self.put_bytes(s.as_bytes());
    }

    fn put_opt<T>(&mut self, opt: Option<T>, f: impl FnOnce(&mut Self, T)) {
        match opt {
            Some(v) => {
                self.put_u8(1);
                f(self, v);
            }
            None => self.put_u8(0),
        }
    }

    fn put_cell(&mut self, cell: CellIdentity) {
        self.put_arr16(cell.as_bytes());
    }

    fn put_gen(&mut self, g: Generation) {
        self.put_u64(g.get());
    }

    fn put_epoch(&mut self, e: CellEpoch) {
        self.put_u64(e.get());
    }

    fn put_fence(&mut self, fence: CellFence) {
        self.put_cell(fence.identity());
        self.put_gen(fence.node_boot_generation());
        self.put_epoch(fence.epoch());
        self.put_u64(fence.fencing_token().get());
    }

    fn put_registration(&mut self, entry: &CellRegistrationEntry) {
        self.put_cell(entry.identity());
        self.put_gen(entry.node_boot_generation());
        self.put_epoch(entry.epoch());
        self.put_u64(entry.fencing_token().get());
        self.put_u32(entry.os_process_id());
        self.put_str(entry.description());
        self.put_u64(entry.heartbeat());
    }

    fn put_bootstrap_principal(&mut self, b: &BootstrapPrincipalRequest) {
        self.put_arr32(&b.principal_profile_digest);
        self.put_arr32(&b.control_domain_policy_digest);
        self.put_arr32(&b.public_key);
        let purpose_code = match b.key_purpose {
            KeyPurpose::SemanticSigning => 1_u8,
            KeyPurpose::BarrierObservationSigning => 2_u8,
        };
        self.put_u8(purpose_code);
        self.put_u64(b.key_valid_from_ms);
        self.put_u64(b.key_valid_until_ms);
        self.put_arr16(b.idempotency_key.as_bytes());
        self.put_u64(b.created_at_ms);
    }

    fn put_cap_target(&mut self, target: CapabilityTarget) {
        match target {
            CapabilityTarget::Namespace(ns) => {
                self.put_u8(1);
                self.put_arr16(&ns.into_bytes());
            }
            CapabilityTarget::Task(tid) => {
                self.put_u8(2);
                self.put_arr16(tid.as_bytes());
            }
        }
    }

    fn put_cap_handle(&mut self, handle: CapabilityHandle) {
        self.put_arr16(handle.capability_id.as_bytes());
        self.put_gen(handle.generation);
    }

    fn put_signed_root_cap(&mut self, req: &SignedIssueRootCapabilityRequest) {
        self.put_arr16(req.command.issuer_key_id.as_bytes());
        self.put_arr16(req.command.holder_key_id.as_bytes());
        self.put_cap_target(req.command.target);
        self.put_u64(req.command.rights.bits());
        self.put_opt(req.command.purpose_digest, |w, d| w.put_arr32(&d));
        self.put_u64(req.command.valid_from_ms);
        self.put_u64(req.command.valid_until_ms);
        self.put_u8(req.command.delegation_depth_remaining);
        self.put_opt(req.command.call_limit, Self::put_u64);
        self.put_arr16(req.command.idempotency_key.as_bytes());
        self.put_u64(req.command.issued_at_ms);
        self.put_arr16(req.signer.as_bytes());
        self.put_arr64(&req.signature);
    }

    fn put_signed_delegate_cap(&mut self, req: &SignedDelegateCapabilityRequest) {
        self.put_cap_handle(req.command.parent);
        self.put_arr16(req.command.delegator_key_id.as_bytes());
        self.put_arr16(req.command.recipient_key_id.as_bytes());
        self.put_cap_target(req.command.target);
        self.put_u64(req.command.rights.bits());
        self.put_opt(req.command.purpose_digest, |w, d| w.put_arr32(&d));
        self.put_u64(req.command.valid_from_ms);
        self.put_u64(req.command.valid_until_ms);
        self.put_u8(req.command.delegation_depth_remaining);
        self.put_opt(req.command.call_limit, Self::put_u64);
        self.put_arr16(req.command.idempotency_key.as_bytes());
        self.put_u64(req.command.delegated_at_ms);
        self.put_arr16(req.signer.as_bytes());
        self.put_arr64(&req.signature);
    }

    fn put_cap_record(&mut self, r: &CapabilityRecord) {
        self.put_cap_handle(r.handle);
        self.put_arr16(r.issuer.as_bytes());
        self.put_arr16(r.issuer_control_domain.as_bytes());
        self.put_arr16(r.holder.as_bytes());
        self.put_arr16(r.holder_control_domain.as_bytes());
        self.put_cap_target(r.target);
        self.put_u64(r.rights.bits());
        self.put_opt(r.purpose_digest, |w, d| w.put_arr32(&d));
        self.put_u64(r.valid_from_ms);
        self.put_u64(r.valid_until_ms);
        self.put_u8(r.delegation_depth_remaining);
        self.put_opt(r.call_limit, Self::put_u64);
        self.put_opt(r.parent, Self::put_cap_handle);
        self.put_opt(r.revoked_at_ms, Self::put_u64);
    }

    fn put_cap_issue_receipt(&mut self, r: &CapabilityIssueReceipt) {
        self.put_arr16(r.receipt_id.as_bytes());
        self.put_arr16(r.capability_id.as_bytes());
        self.put_gen(r.generation);
        self.put_opt(r.parent, Self::put_cap_handle);
        self.put_u64(r.issued_at_ms);
    }

    fn put_migration_mode(&mut self, mode: &MigrationMode) {
        match mode {
            MigrationMode::TransparentCheckpointRestore => self.put_u8(0),
            MigrationMode::DowngradedRestartFromArtifact {
                artifact_image_digest,
                dropped_pinned_sessions,
            } => {
                self.put_u8(1);
                self.put_arr32(artifact_image_digest);
                self.put_u64(u64::try_from(dropped_pinned_sessions.len()).unwrap_or(0));
                for sess in dropped_pinned_sessions {
                    self.put_arr16(sess);
                }
            }
        }
    }

    fn put_checkpoint_proof(&mut self, proof: &ProcessCheckpointProof) {
        self.put_arr16(proof.process_id.as_bytes());
        self.put_u64(proof.source_generation);
        match &proof.payload {
            CheckpointPayloadKind::FullStateCheckpoint {
                state_checkpoint_digest,
                migrated_session_digests,
            } => {
                self.put_u8(0);
                self.put_arr32(state_checkpoint_digest);
                self.put_u64(u64::try_from(migrated_session_digests.len()).unwrap_or(0));
                for (sid, sdig) in migrated_session_digests {
                    self.put_arr16(sid);
                    self.put_arr32(sdig);
                }
            }
            CheckpointPayloadKind::ArtifactRestartImage {
                artifact_image_digest,
            } => {
                self.put_u8(1);
                self.put_arr32(artifact_image_digest);
            }
        }
        self.put_u64(proof.checkpointed_at_ms);
    }

    fn put_process_binding(&mut self, b: &ProcessBindingRecord) {
        self.put_arr16(b.process_id.as_bytes());
        self.put_gen(b.process_generation);
        self.put_arr32(&b.process_fencing_token);
        self.put_arr16(b.agent_instance_id.as_bytes());
        self.put_gen(b.agent_instance_generation);
        self.put_arr16(b.task_id.as_bytes());
        self.put_arr16(b.task_attempt_id.as_bytes());
        self.put_gen(b.attempt_generation);
        self.put_arr16(b.isolation_domain_id.as_bytes());
        self.put_gen(b.isolation_domain_generation);
        self.put_arr32(&b.isolation_domain_fencing_token);
        self.put_opt(b.prior_process_generation, Self::put_gen);
        self.put_arr16(b.idempotency_key.as_bytes());
        self.put_u64(b.created_at_ms);
    }

    fn put_lease_bundle_spec(&mut self, spec: &LeaseMigrationBundleSpec) {
        self.put_arr16(spec.quota.source_lease_id.as_bytes());
        self.put_arr16(&spec.quota.gateway_id);
        self.put_arr16(spec.quota.target_lease_id.as_bytes());
        self.put_opt(spec.quota.target_face_value, Self::put_u64);
        self.put_bool(spec.quota.activate_on_target);

        self.put_opt(spec.capacity, |w, cap| {
            w.put_arr16(cap.source_lease_id.as_bytes());
            w.put_arr16(cap.reconcile_receipt_id.as_bytes());
            w.put_arr16(&cap.gateway_id);
            w.put_arr16(cap.target_lease_id.as_bytes());
            w.put_opt(cap.target_amount, Self::put_u64);
        });
        self.put_opt(spec.device, |w, dev| {
            w.put_arr16(dev.source_lease_id.as_bytes());
            w.put_arr16(dev.reset_receipt_id.as_bytes());
            w.put_arr16(dev.reconcile_receipt_id.as_bytes());
            w.put_arr16(&dev.gateway_id);
            w.put_arr16(dev.target_lease_id.as_bytes());
            w.put_bool(dev.advance_target_epoch_if_needed);
        });
        self.put_u64(spec.regranted_at_ms);
    }

    fn put_source_closed_leases(&mut self, r: &SourceLeaseCloseReport) {
        self.put_arr16(r.closed_quota.lease_id.as_bytes());
        self.put_u64(r.closed_quota.face_value);
        self.put_u64(r.closed_quota.spent);
        self.put_u64(r.closed_quota.returned);
        self.put_arr32(&r.closed_quota.close_digest);

        self.put_opt(r.returned_capacity, |w, cap| {
            w.put_arr16(cap.capacity_lease_id.as_bytes());
            w.put_u64(cap.amount);
            w.put_arr32(&cap.close_digest);
        });
        self.put_opt(r.returned_device, |w, dev| {
            w.put_arr16(dev.device_lease_id.as_bytes());
            w.put_epoch(dev.exclusivity_epoch);
            w.put_gen(dev.head_reset_generation);
            w.put_arr32(&dev.close_digest);
        });
    }

    fn put_migration_receipt(&mut self, r: &ProcessMigrationReceipt) {
        self.put_arr16(&r.migration_id);
        self.put_arr16(r.process_id.as_bytes());
        self.put_arr16(&r.source_cell_id);
        self.put_arr16(&r.target_cell_id);
        self.put_u64(r.source_generation);
        self.put_u64(r.target_generation);
        self.put_migration_mode(&r.migration_mode);

        // QuiesceProof
        self.put_arr16(r.quiesce_proof.process_id.as_bytes());
        self.put_arr16(&r.quiesce_proof.source_cell_id);
        self.put_u64(r.quiesce_proof.source_generation);
        self.put_u32(r.quiesce_proof.quiesced_fibers);
        self.put_u32(r.quiesce_proof.cancelled_inflight_ops);
        self.put_bool(r.quiesce_proof.inflight_effects_drained);
        self.put_u64(r.quiesce_proof.quiesced_at_ms);

        // CheckpointProof
        self.put_checkpoint_proof(&r.checkpoint_proof);

        // SourceProcessFenceProof
        self.put_arr16(r.source_fence_proof.process_id.as_bytes());
        self.put_arr16(&r.source_fence_proof.source_cell_id);
        self.put_u64(r.source_fence_proof.source_cell_epoch);
        self.put_u64(r.source_fence_proof.fenced_generation);
        self.put_arr32(&r.source_fence_proof.source_fencing_token);
        self.put_bool(r.source_fence_proof.supervisor_unregistered);
        self.put_u64(r.source_fence_proof.fenced_at_ms);

        // CapabilityRebindProof
        self.put_arr16(r.capability_rebind_proof.process_id.as_bytes());
        self.put_u64(
            u64::try_from(r.capability_rebind_proof.rebound_capabilities.len()).unwrap_or(0),
        );
        for entry in &r.capability_rebind_proof.rebound_capabilities {
            self.put_arr16(&entry.capability_id);
            self.put_arr16(&entry.source_cell_id);
            self.put_u64(entry.source_generation);
            self.put_arr16(&entry.target_cell_id);
            self.put_u64(entry.target_generation);
            self.put_arr32(&entry.attenuation_digest);
        }
        self.put_u64(r.capability_rebind_proof.rebound_at_ms);

        // LeaseCloseRegrantProof
        self.put_arr16(r.lease_regrant_proof.process_id.as_bytes());
        self.put_u64(r.lease_regrant_proof.source_generation);
        self.put_u64(r.lease_regrant_proof.target_generation);
        self.put_u64(u64::try_from(r.lease_regrant_proof.migrated_leases.len()).unwrap_or(0));
        for entry in &r.lease_regrant_proof.migrated_leases {
            self.put_arr16(&entry.source_lease_id);
            self.put_arr16(&entry.source_cell_id);
            self.put_bool(entry.source_closed_or_fenced);
            self.put_arr32(&entry.source_close_receipt_digest);
            self.put_arr16(&entry.target_lease_id);
            self.put_arr16(&entry.target_cell_id);
            self.put_u64(entry.target_grant_epoch);
        }
        self.put_u64(r.lease_regrant_proof.regranted_at_ms);

        // TargetRestoreProof
        self.put_arr16(r.target_restore_proof.process_id.as_bytes());
        self.put_arr16(&r.target_restore_proof.target_cell_id);
        self.put_u64(r.target_restore_proof.restored_generation);
        self.put_arr32(&r.target_restore_proof.target_fencing_token);
        self.put_arr16(r.target_restore_proof.target_isolation_domain_id.as_bytes());
        self.put_arr32(&r.target_restore_proof.restored_image_or_checkpoint_digest);
        self.put_u64(r.target_restore_proof.restored_at_ms);

        self.put_u64(r.completed_at_ms);
        self.put_arr32(&r.receipt_digest);
    }

    fn put_participant(&mut self, p: ParticipantRecord) {
        self.put_u8(participant_type_to_u8(p.participant_type));
        self.put_arr16(p.participant_id.as_bytes());
        self.put_gen(p.participant_generation);
        self.put_arr16(p.admission_receipt_id.as_bytes());
    }

    fn put_authority_lease(&mut self, lease: AuthorityLeaseRecord) {
        self.put_arr16(lease.authority_id.as_bytes());
        self.put_arr16(lease.holder_id.as_bytes());
        self.put_u64(lease.term);
        self.put_u64(lease.lease_epoch);
        self.put_arr32(&lease.fencing_token);
        self.put_i64(lease.requested_at_ms);
        self.put_i64(lease.expires_at_ms);
        self.put_i64(lease.ttl_ms);
        self.put_arr16(lease.idempotency_key.as_bytes());
    }

    fn put_control_commit_subject(&mut self, subject: &ControlCommitSubject) {
        match *subject {
            ControlCommitSubject::TaskAuthorityAssignment {
                assignment_id,
                task_id,
                task_generation,
                authority_term,
                control_epoch,
                participant_registry_generation,
                participant_registry_root,
            } => {
                self.put_u8(1);
                self.put_arr16(assignment_id.as_bytes());
                self.put_arr16(task_id.as_bytes());
                self.put_gen(task_generation);
                self.put_u64(authority_term.get());
                self.put_u64(control_epoch);
                self.put_u64(participant_registry_generation);
                self.put_arr32(&participant_registry_root);
            }
            ControlCommitSubject::TaskAuthorityTakeoverReceipt {
                takeover_receipt_id,
                task_id,
                task_generation,
                old_assignment_id,
                new_assignment_id,
                frozen_old_authority_term,
                new_authority_term,
                new_control_epoch,
                exact_fence_set_root,
                outstanding_effect_quarantine_root,
            } => {
                self.put_u8(2);
                self.put_arr16(takeover_receipt_id.as_bytes());
                self.put_arr16(task_id.as_bytes());
                self.put_gen(task_generation);
                self.put_arr16(old_assignment_id.as_bytes());
                self.put_arr16(new_assignment_id.as_bytes());
                self.put_u64(frozen_old_authority_term.get());
                self.put_u64(new_authority_term.get());
                self.put_u64(new_control_epoch);
                self.put_arr32(&exact_fence_set_root);
                self.put_opt(outstanding_effect_quarantine_root, |w, r| w.put_arr32(&r));
            }
        }
    }
}

struct WireReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> WireReader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], &'static str> {
        let end = self.pos.checked_add(len).ok_or("wire length overflow")?;
        if end > self.bytes.len() {
            return Err("truncated wire frame");
        }
        let out = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(out)
    }

    fn finish(&self) -> Result<(), &'static str> {
        if self.pos == self.bytes.len() {
            Ok(())
        } else {
            Err("unexpected trailing wire bytes")
        }
    }

    fn get_u8(&mut self) -> Result<u8, &'static str> {
        Ok(self.take(1)?[0])
    }

    fn get_bool(&mut self) -> Result<bool, &'static str> {
        match self.get_u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err("invalid bool wire byte"),
        }
    }

    fn get_u32(&mut self) -> Result<u32, &'static str> {
        let b: [u8; 4] = self.take(4)?.try_into().map_err(|_| "u32")?;
        Ok(u32::from_be_bytes(b))
    }

    fn get_u64(&mut self) -> Result<u64, &'static str> {
        let b: [u8; 8] = self.take(8)?.try_into().map_err(|_| "u64")?;
        Ok(u64::from_be_bytes(b))
    }

    fn get_i64(&mut self) -> Result<i64, &'static str> {
        let b: [u8; 8] = self.take(8)?.try_into().map_err(|_| "i64")?;
        Ok(i64::from_be_bytes(b))
    }

    fn get_nz_u64(&mut self) -> Result<NonZeroU64, &'static str> {
        NonZeroU64::new(self.get_u64()?).ok_or("expected non-zero u64")
    }

    fn get_arr16(&mut self) -> Result<[u8; 16], &'static str> {
        self.take(16)?.try_into().map_err(|_| "arr16")
    }

    fn get_arr32(&mut self) -> Result<[u8; 32], &'static str> {
        self.take(32)?.try_into().map_err(|_| "arr32")
    }

    fn get_arr64(&mut self) -> Result<[u8; 64], &'static str> {
        self.take(64)?.try_into().map_err(|_| "arr64")
    }

    fn get_bytes(&mut self) -> Result<Vec<u8>, &'static str> {
        let len = usize::try_from(self.get_u64()?).map_err(|_| "vec len overflow")?;
        if len > 64 * 1024 * 1024 {
            return Err("wire vec exceeds 64 MiB safety cap");
        }
        Ok(self.take(len)?.to_vec())
    }

    fn get_str(&mut self) -> Result<String, &'static str> {
        String::from_utf8(self.get_bytes()?).map_err(|_| "invalid utf-8 wire string")
    }

    fn get_opt<T>(
        &mut self,
        f: impl FnOnce(&mut Self) -> Result<T, &'static str>,
    ) -> Result<Option<T>, &'static str> {
        match self.get_u8()? {
            0 => Ok(None),
            1 => Ok(Some(f(self)?)),
            _ => Err("invalid option tag"),
        }
    }

    fn get_cell(&mut self) -> Result<CellIdentity, &'static str> {
        Ok(CellIdentity::from_domain(SchedulerDomainId::from_bytes(
            self.get_arr16()?,
        )))
    }

    fn get_gen(&mut self) -> Result<Generation, &'static str> {
        Ok(Generation::new(self.get_nz_u64()?))
    }

    fn get_epoch(&mut self) -> Result<CellEpoch, &'static str> {
        CellEpoch::from_u64(self.get_nz_u64()?.get()).ok_or("invalid CellEpoch")
    }

    fn get_fence(&mut self) -> Result<CellFence, &'static str> {
        let cell = self.get_cell()?;
        let boot_gen = self.get_gen()?;
        let epoch = self.get_epoch()?;
        let token = CellFencingToken::from_u64(self.get_nz_u64()?.get())
            .ok_or("invalid CellFencingToken")?;
        Ok(CellFence::present(cell, boot_gen, epoch, token))
    }

    fn get_registration(&mut self) -> Result<CellRegistrationEntry, &'static str> {
        let cell = self.get_cell()?;
        let boot_gen = self.get_gen()?;
        let epoch = self.get_epoch()?;
        let token = CellFencingToken::from_u64(self.get_nz_u64()?.get())
            .ok_or("invalid CellFencingToken")?;
        let os_pid = self.get_u32()?;
        let desc = self.get_str()?;
        let heartbeat = self.get_u64()?;
        let entry = CellRegistrationEntry::new(cell, boot_gen, epoch, token, os_pid, &desc)
            .map_err(|_| "invalid CellRegistrationEntry")?
            .with_heartbeat(heartbeat);
        Ok(entry)
    }

    fn get_bootstrap_principal(&mut self) -> Result<BootstrapPrincipalRequest, &'static str> {
        let principal_profile_digest = self.get_arr32()?;
        let control_domain_policy_digest = self.get_arr32()?;
        let public_key = self.get_arr32()?;
        let key_purpose = match self.get_u8()? {
            1 => KeyPurpose::SemanticSigning,
            2 => KeyPurpose::BarrierObservationSigning,
            _ => return Err("invalid KeyPurpose tag"),
        };
        let key_valid_from_ms = self.get_u64()?;
        let key_valid_until_ms = self.get_u64()?;
        let idempotency_key = IdempotencyKey::from_bytes(self.get_arr16()?);
        let created_at_ms = self.get_u64()?;
        Ok(BootstrapPrincipalRequest {
            principal_profile_digest,
            control_domain_policy_digest,
            public_key,
            key_purpose,
            key_valid_from_ms,
            key_valid_until_ms,
            idempotency_key,
            created_at_ms,
        })
    }

    fn get_cap_target(&mut self) -> Result<CapabilityTarget, &'static str> {
        match self.get_u8()? {
            1 => Ok(CapabilityTarget::Namespace(NamespaceId::from_bytes(
                self.get_arr16()?,
            ))),
            2 => Ok(CapabilityTarget::Task(TaskId::from_bytes(
                self.get_arr16()?,
            ))),
            _ => Err("invalid CapabilityTarget tag"),
        }
    }

    fn get_cap_handle(&mut self) -> Result<CapabilityHandle, &'static str> {
        Ok(CapabilityHandle {
            capability_id: CapabilityId::from_bytes(self.get_arr16()?),
            generation: self.get_gen()?,
        })
    }

    fn get_signed_root_cap(&mut self) -> Result<SignedIssueRootCapabilityRequest, &'static str> {
        let issuer_key_id = KeyId::from_bytes(self.get_arr16()?);
        let holder_key_id = KeyId::from_bytes(self.get_arr16()?);
        let target = self.get_cap_target()?;
        let rights =
            CapabilityRights::from_bits(self.get_u64()?).ok_or("invalid CapabilityRights bits")?;
        let purpose_digest = self.get_opt(Self::get_arr32)?;
        let valid_from_ms = self.get_u64()?;
        let valid_until_ms = self.get_u64()?;
        let delegation_depth_remaining = self.get_u8()?;
        let call_limit = self.get_opt(Self::get_u64)?;
        let idempotency_key = IdempotencyKey::from_bytes(self.get_arr16()?);
        let issued_at_ms = self.get_u64()?;
        let signer = PrincipalId::from_bytes(self.get_arr16()?);
        let signature = self.get_arr64()?;
        Ok(SignedIssueRootCapabilityRequest {
            command: IssueRootCapabilityRequest {
                issuer_key_id,
                holder_key_id,
                target,
                rights,
                purpose_digest,
                valid_from_ms,
                valid_until_ms,
                delegation_depth_remaining,
                call_limit,
                idempotency_key,
                issued_at_ms,
            },
            signer,
            signature,
        })
    }

    fn get_signed_delegate_cap(&mut self) -> Result<SignedDelegateCapabilityRequest, &'static str> {
        let parent = self.get_cap_handle()?;
        let delegator_key_id = KeyId::from_bytes(self.get_arr16()?);
        let recipient_key_id = KeyId::from_bytes(self.get_arr16()?);
        let target = self.get_cap_target()?;
        let rights =
            CapabilityRights::from_bits(self.get_u64()?).ok_or("invalid CapabilityRights bits")?;
        let purpose_digest = self.get_opt(Self::get_arr32)?;
        let valid_from_ms = self.get_u64()?;
        let valid_until_ms = self.get_u64()?;
        let delegation_depth_remaining = self.get_u8()?;
        let call_limit = self.get_opt(Self::get_u64)?;
        let idempotency_key = IdempotencyKey::from_bytes(self.get_arr16()?);
        let delegated_at_ms = self.get_u64()?;
        let signer = PrincipalId::from_bytes(self.get_arr16()?);
        let signature = self.get_arr64()?;
        Ok(SignedDelegateCapabilityRequest {
            command: DelegateCapabilityRequest {
                parent,
                delegator_key_id,
                recipient_key_id,
                target,
                rights,
                purpose_digest,
                valid_from_ms,
                valid_until_ms,
                delegation_depth_remaining,
                call_limit,
                idempotency_key,
                delegated_at_ms,
            },
            signer,
            signature,
        })
    }

    fn get_cap_record(&mut self) -> Result<CapabilityRecord, &'static str> {
        let handle = self.get_cap_handle()?;
        let issuer = PrincipalId::from_bytes(self.get_arr16()?);
        let issuer_control_domain = ControlDomainId::from_bytes(self.get_arr16()?);
        let holder = PrincipalId::from_bytes(self.get_arr16()?);
        let holder_control_domain = ControlDomainId::from_bytes(self.get_arr16()?);
        let target = self.get_cap_target()?;
        let rights =
            CapabilityRights::from_bits(self.get_u64()?).ok_or("invalid CapabilityRights bits")?;
        let purpose_digest = self.get_opt(Self::get_arr32)?;
        let valid_from_ms = self.get_u64()?;
        let valid_until_ms = self.get_u64()?;
        let delegation_depth_remaining = self.get_u8()?;
        let call_limit = self.get_opt(Self::get_u64)?;
        let parent = self.get_opt(Self::get_cap_handle)?;
        let revoked_at_ms = self.get_opt(Self::get_u64)?;
        Ok(CapabilityRecord {
            handle,
            issuer,
            issuer_control_domain,
            holder,
            holder_control_domain,
            target,
            rights,
            purpose_digest,
            valid_from_ms,
            valid_until_ms,
            delegation_depth_remaining,
            call_limit,
            parent,
            revoked_at_ms,
        })
    }

    fn get_cap_issue_receipt(&mut self) -> Result<CapabilityIssueReceipt, &'static str> {
        Ok(CapabilityIssueReceipt {
            receipt_id: ReceiptId::from_bytes(self.get_arr16()?),
            capability_id: CapabilityId::from_bytes(self.get_arr16()?),
            generation: self.get_gen()?,
            parent: self.get_opt(Self::get_cap_handle)?,
            issued_at_ms: self.get_u64()?,
        })
    }

    fn get_migration_mode(&mut self) -> Result<MigrationMode, &'static str> {
        match self.get_u8()? {
            0 => Ok(MigrationMode::TransparentCheckpointRestore),
            1 => {
                let artifact_image_digest = self.get_arr32()?;
                let n = usize::try_from(self.get_u64()?).map_err(|_| "count")?;
                let mut dropped_pinned_sessions = Vec::with_capacity(n.min(256));
                for _ in 0..n {
                    dropped_pinned_sessions.push(self.get_arr16()?);
                }
                Ok(MigrationMode::DowngradedRestartFromArtifact {
                    artifact_image_digest,
                    dropped_pinned_sessions,
                })
            }
            _ => Err("invalid MigrationMode tag"),
        }
    }

    fn get_checkpoint_proof(&mut self) -> Result<ProcessCheckpointProof, &'static str> {
        let process_id = ProcessId::from_bytes(self.get_arr16()?);
        let source_generation = self.get_u64()?;
        let payload = match self.get_u8()? {
            0 => {
                let state_checkpoint_digest = self.get_arr32()?;
                let n = usize::try_from(self.get_u64()?).map_err(|_| "count")?;
                let mut migrated_session_digests = Vec::with_capacity(n.min(256));
                for _ in 0..n {
                    migrated_session_digests.push((self.get_arr16()?, self.get_arr32()?));
                }
                CheckpointPayloadKind::FullStateCheckpoint {
                    state_checkpoint_digest,
                    migrated_session_digests,
                }
            }
            1 => CheckpointPayloadKind::ArtifactRestartImage {
                artifact_image_digest: self.get_arr32()?,
            },
            _ => return Err("invalid CheckpointPayloadKind tag"),
        };
        let checkpointed_at_ms = self.get_u64()?;
        Ok(ProcessCheckpointProof {
            process_id,
            source_generation,
            payload,
            checkpointed_at_ms,
        })
    }

    fn get_process_binding(&mut self) -> Result<ProcessBindingRecord, &'static str> {
        Ok(ProcessBindingRecord {
            process_id: ProcessId::from_bytes(self.get_arr16()?),
            process_generation: self.get_gen()?,
            process_fencing_token: self.get_arr32()?,
            agent_instance_id: AgentInstanceId::from_bytes(self.get_arr16()?),
            agent_instance_generation: self.get_gen()?,
            task_id: TaskId::from_bytes(self.get_arr16()?),
            task_attempt_id: TaskAttemptId::from_bytes(self.get_arr16()?),
            attempt_generation: self.get_gen()?,
            isolation_domain_id: IsolationDomainId::from_bytes(self.get_arr16()?),
            isolation_domain_generation: self.get_gen()?,
            isolation_domain_fencing_token: self.get_arr32()?,
            prior_process_generation: self.get_opt(Self::get_gen)?,
            idempotency_key: IdempotencyKey::from_bytes(self.get_arr16()?),
            created_at_ms: self.get_u64()?,
        })
    }

    fn get_lease_bundle_spec(&mut self) -> Result<LeaseMigrationBundleSpec, &'static str> {
        let quota = QuotaLeaseMigrationSpec {
            source_lease_id: QuotaLeaseId::from_bytes(self.get_arr16()?),
            gateway_id: self.get_arr16()?,
            override_fence_ack: None,
            target_lease_id: QuotaLeaseId::from_bytes(self.get_arr16()?),
            target_face_value: self.get_opt(Self::get_u64)?,
            target_expires_at: None,
            activate_on_target: self.get_bool()?,
        };
        let capacity = self.get_opt(|r| {
            Ok(CapacityLeaseMigrationSpec {
                source_lease_id: CapacityLeaseId::from_bytes(r.get_arr16()?),
                reconcile_receipt_id: ReceiptId::from_bytes(r.get_arr16()?),
                gateway_id: r.get_arr16()?,
                target_lease_id: CapacityLeaseId::from_bytes(r.get_arr16()?),
                target_amount: r.get_opt(Self::get_u64)?,
            })
        })?;
        let device = self.get_opt(|r| {
            Ok(DeviceLeaseMigrationSpec {
                source_lease_id: ExclusiveDeviceLeaseId::from_bytes(r.get_arr16()?),
                reset_receipt_id: ReceiptId::from_bytes(r.get_arr16()?),
                reconcile_receipt_id: ReceiptId::from_bytes(r.get_arr16()?),
                gateway_id: r.get_arr16()?,
                target_lease_id: ExclusiveDeviceLeaseId::from_bytes(r.get_arr16()?),
                advance_target_epoch_if_needed: r.get_bool()?,
            })
        })?;
        let regranted_at_ms = self.get_u64()?;
        Ok(LeaseMigrationBundleSpec {
            quota,
            capacity,
            device,
            regranted_at_ms,
        })
    }

    fn get_source_closed_leases(&mut self) -> Result<SourceLeaseCloseReport, &'static str> {
        let closed_quota = SourceClosedQuotaSummary {
            lease_id: QuotaLeaseId::from_bytes(self.get_arr16()?),
            state: QuotaLeaseState::Closed,
            face_value: self.get_u64()?,
            spent: self.get_u64()?,
            returned: self.get_u64()?,
            close_digest: self.get_arr32()?,
        };
        let returned_capacity = self.get_opt(|r| {
            Ok(SourceReturnedCapacitySummary {
                capacity_lease_id: CapacityLeaseId::from_bytes(r.get_arr16()?),
                state: CapacityLeaseState::Returned,
                amount: r.get_u64()?,
                close_digest: r.get_arr32()?,
            })
        })?;
        let returned_device = self.get_opt(|r| {
            Ok(SourceReturnedDeviceSummary {
                device_lease_id: ExclusiveDeviceLeaseId::from_bytes(r.get_arr16()?),
                state: ExclusiveDeviceLeaseState::Returned,
                exclusivity_epoch: r.get_epoch()?,
                head_reset_generation: r.get_gen()?,
                close_digest: r.get_arr32()?,
            })
        })?;
        Ok(SourceLeaseCloseReport {
            closed_quota,
            returned_capacity,
            returned_device,
        })
    }

    fn get_migration_receipt(&mut self) -> Result<ProcessMigrationReceipt, &'static str> {
        let migration_id = self.get_arr16()?;
        let process_id = ProcessId::from_bytes(self.get_arr16()?);
        let source_cell_id = self.get_arr16()?;
        let target_cell_id = self.get_arr16()?;
        let source_generation = self.get_u64()?;
        let target_generation = self.get_u64()?;
        let migration_mode = self.get_migration_mode()?;

        let quiesce_proof = nlos_process::QuiesceProof {
            process_id: ProcessId::from_bytes(self.get_arr16()?),
            source_cell_id: self.get_arr16()?,
            source_generation: self.get_u64()?,
            quiesced_fibers: self.get_u32()?,
            cancelled_inflight_ops: self.get_u32()?,
            inflight_effects_drained: self.get_bool()?,
            quiesced_at_ms: self.get_u64()?,
        };
        let checkpoint_proof = self.get_checkpoint_proof()?;
        let source_fence_proof = nlos_process::SourceProcessFenceProof {
            process_id: ProcessId::from_bytes(self.get_arr16()?),
            source_cell_id: self.get_arr16()?,
            source_cell_epoch: self.get_u64()?,
            fenced_generation: self.get_u64()?,
            source_fencing_token: self.get_arr32()?,
            supervisor_unregistered: self.get_bool()?,
            fenced_at_ms: self.get_u64()?,
        };

        let cap_pid = ProcessId::from_bytes(self.get_arr16()?);
        let cap_len = usize::try_from(self.get_u64()?).map_err(|_| "cap count")?;
        let mut rebound_capabilities = Vec::with_capacity(cap_len.min(256));
        for _ in 0..cap_len {
            rebound_capabilities.push(ReboundCapabilityEntry {
                capability_id: self.get_arr16()?,
                source_cell_id: self.get_arr16()?,
                source_generation: self.get_u64()?,
                target_cell_id: self.get_arr16()?,
                target_generation: self.get_u64()?,
                attenuation_digest: self.get_arr32()?,
            });
        }
        let capability_rebind_proof = CapabilityRebindProof {
            process_id: cap_pid,
            rebound_capabilities,
            rebound_at_ms: self.get_u64()?,
        };

        let lease_pid = ProcessId::from_bytes(self.get_arr16()?);
        let lease_source_gen = self.get_u64()?;
        let lease_target_gen = self.get_u64()?;
        let lease_len = usize::try_from(self.get_u64()?).map_err(|_| "lease count")?;
        let mut migrated_leases = Vec::with_capacity(lease_len.min(64));
        for _ in 0..lease_len {
            migrated_leases.push(MigratedLeaseEntry {
                source_lease_id: self.get_arr16()?,
                source_cell_id: self.get_arr16()?,
                source_closed_or_fenced: self.get_bool()?,
                source_close_receipt_digest: self.get_arr32()?,
                target_lease_id: self.get_arr16()?,
                target_cell_id: self.get_arr16()?,
                target_grant_epoch: self.get_u64()?,
            });
        }
        let lease_regrant_proof = LeaseCloseRegrantProof {
            process_id: lease_pid,
            source_generation: lease_source_gen,
            target_generation: lease_target_gen,
            migrated_leases,
            regranted_at_ms: self.get_u64()?,
        };

        let target_restore_proof = TargetRestoreProof {
            process_id: ProcessId::from_bytes(self.get_arr16()?),
            target_cell_id: self.get_arr16()?,
            restored_generation: self.get_u64()?,
            target_fencing_token: self.get_arr32()?,
            target_isolation_domain_id: IsolationDomainId::from_bytes(self.get_arr16()?),
            restored_image_or_checkpoint_digest: self.get_arr32()?,
            restored_at_ms: self.get_u64()?,
        };

        let completed_at_ms = self.get_u64()?;
        let receipt_digest = self.get_arr32()?;
        Ok(ProcessMigrationReceipt {
            migration_id,
            process_id,
            source_cell_id,
            target_cell_id,
            source_generation,
            target_generation,
            migration_mode,
            quiesce_proof,
            checkpoint_proof,
            source_fence_proof,
            capability_rebind_proof,
            lease_regrant_proof,
            target_restore_proof,
            completed_at_ms,
            receipt_digest,
        })
    }

    fn get_participant(&mut self) -> Result<ParticipantRecord, &'static str> {
        let participant_type = participant_type_from_u8(self.get_u8()?)?;
        Ok(ParticipantRecord {
            participant_type,
            participant_id: TaskParticipantId::from_bytes(self.get_arr16()?),
            participant_generation: self.get_gen()?,
            admission_receipt_id: ReceiptId::from_bytes(self.get_arr16()?),
        })
    }

    fn get_authority_lease(&mut self) -> Result<AuthorityLeaseRecord, &'static str> {
        Ok(AuthorityLeaseRecord {
            authority_id: TaskParticipantId::from_bytes(self.get_arr16()?),
            holder_id: ProcessId::from_bytes(self.get_arr16()?),
            term: self.get_u64()?,
            lease_epoch: self.get_u64()?,
            fencing_token: self.get_arr32()?,
            requested_at_ms: self.get_i64()?,
            expires_at_ms: self.get_i64()?,
            ttl_ms: self.get_i64()?,
            idempotency_key: IdempotencyKey::from_bytes(self.get_arr16()?),
        })
    }

    fn get_control_commit_subject(&mut self) -> Result<ControlCommitSubject, &'static str> {
        match self.get_u8()? {
            1 => Ok(ControlCommitSubject::TaskAuthorityAssignment {
                assignment_id: TaskAuthorityAssignmentId::from_bytes(self.get_arr16()?),
                task_id: TaskId::from_bytes(self.get_arr16()?),
                task_generation: self.get_gen()?,
                authority_term: self.get_nz_u64()?,
                control_epoch: self.get_u64()?,
                participant_registry_generation: self.get_u64()?,
                participant_registry_root: self.get_arr32()?,
            }),
            2 => Ok(ControlCommitSubject::TaskAuthorityTakeoverReceipt {
                takeover_receipt_id: ReceiptId::from_bytes(self.get_arr16()?),
                task_id: TaskId::from_bytes(self.get_arr16()?),
                task_generation: self.get_gen()?,
                old_assignment_id: TaskAuthorityAssignmentId::from_bytes(self.get_arr16()?),
                new_assignment_id: TaskAuthorityAssignmentId::from_bytes(self.get_arr16()?),
                frozen_old_authority_term: self.get_nz_u64()?,
                new_authority_term: self.get_nz_u64()?,
                new_control_epoch: self.get_u64()?,
                exact_fence_set_root: self.get_arr32()?,
                outstanding_effect_quarantine_root: self.get_opt(Self::get_arr32)?,
            }),
            _ => Err("invalid ControlCommitSubject tag"),
        }
    }
}

const fn participant_type_to_u8(participant_type: ParticipantType) -> u8 {
    match participant_type {
        ParticipantType::TaskStore => 1,
        ParticipantType::ArtifactHead => 2,
        ParticipantType::SemanticAdmission => 3,
        ParticipantType::ChannelTopic => 4,
        ParticipantType::DriverGateway => 5,
        ParticipantType::ResourceLedger => 6,
        ParticipantType::ProcessBinding => 7,
        ParticipantType::OperationBinding => 8,
    }
}

const fn participant_type_from_u8(code: u8) -> Result<ParticipantType, &'static str> {
    match code {
        1 => Ok(ParticipantType::TaskStore),
        2 => Ok(ParticipantType::ArtifactHead),
        3 => Ok(ParticipantType::SemanticAdmission),
        4 => Ok(ParticipantType::ChannelTopic),
        5 => Ok(ParticipantType::DriverGateway),
        6 => Ok(ParticipantType::ResourceLedger),
        7 => Ok(ParticipantType::ProcessBinding),
        8 => Ok(ParticipantType::OperationBinding),
        _ => Err("invalid ParticipantType code"),
    }
}

fn encode_ok_reply(observed_epoch: CellEpoch, replayed: bool, body: &[u8]) -> RpcHandlerReply {
    let mut out = Vec::with_capacity(9 + body.len());
    out.extend_from_slice(NET_MIGRATION_RPC_MAGIC);
    out.push(1);
    out.extend_from_slice(body);
    RpcHandlerReply {
        observed_epoch,
        idempotency_replayed: replayed,
        status: RpcWireStatus::Ok,
        payload: out,
    }
}

fn encode_tb_ok_reply(observed_epoch: CellEpoch, replayed: bool, body: &[u8]) -> RpcHandlerReply {
    let mut out = Vec::with_capacity(9 + body.len());
    out.extend_from_slice(NET_TAKEOVER_RPC_MAGIC);
    out.push(1);
    out.extend_from_slice(body);
    RpcHandlerReply {
        observed_epoch,
        idempotency_replayed: replayed,
        status: RpcWireStatus::Ok,
        payload: out,
    }
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn encode_err_reply(magic: &[u8; 8], observed_epoch: CellEpoch, detail: &str) -> RpcHandlerReply {
    let mut w = WireWriter::new(magic, 0);
    w.put_str(detail);
    RpcHandlerReply {
        observed_epoch,
        idempotency_replayed: false,
        status: RpcWireStatus::Ok,
        payload: w.into_vec(),
    }
}

#[allow(clippy::trivially_copy_pass_by_ref)]
fn decode_app_reply<'a>(magic: &[u8; 8], raw: &'a [u8]) -> Result<&'a [u8], String> {
    if raw.len() < 9 || &raw[..8] != magic {
        return Err("malformed sub-protocol RPC response magic".to_owned());
    }
    match raw[8] {
        1 => Ok(&raw[9..]),
        0 => {
            let mut r = WireReader::new(&raw[9..]);
            let msg = r
                .get_str()
                .unwrap_or_else(|_| "malformed remote error detail".to_owned());
            Err(msg)
        }
        other => Err(format!("unknown sub-protocol response tag {other}")),
    }
}

// ============================================================================
// `NLOSMG01`: Target `NetMigrationRpcServer` & Peer Bootstrap Exchange
// ============================================================================

const MG_OP_EXCHANGE_BOOTSTRAP: u8 = 1;
const MG_OP_INSPECT_PLACEMENT: u8 = 2;
const MG_OP_VERIFY_CHECKPOINT: u8 = 3;
const MG_OP_PULL_SEMANTIC_DELTA: u8 = 4;
const MG_OP_REBIND_CAPABILITIES: u8 = 5;
const MG_OP_REGRANT_LEASES: u8 = 6;
const MG_OP_RESTORE_PROCESS: u8 = 7;
const MG_OP_RECORD_RECEIPT_CKPT: u8 = 8;
const MG_OP_INSPECT_MIGRATION_STATE: u8 = 9;

type MigrationReplayCache = BTreeMap<(CellIdentity, IdempotencyKey), Vec<u8>>;

/// Authenticated mTLS `NLOSMG01` server handling peer bootstrap discovery and
/// Target-side steps of [`drive_cross_socket_process_migration`].
#[derive(Clone)]
pub struct NetMigrationRpcServer {
    host: Arc<Mutex<CellHost>>,
    transport: Arc<MtlsCellTransport>,
    resolver: Arc<MonotonicEndpointResolver>,
    semantic_bridge: Arc<NetCellSemanticSyncBridge>,
    inspector: NetCellHostInspector,
    replay_cache: Arc<Mutex<MigrationReplayCache>>,
}

impl fmt::Debug for NetMigrationRpcServer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NetMigrationRpcServer")
            .finish_non_exhaustive()
    }
}

impl NetMigrationRpcServer {
    /// Creates a new `NLOSMG01` RPC server.
    #[must_use]
    pub fn new(
        host: Arc<Mutex<CellHost>>,
        transport: Arc<MtlsCellTransport>,
        resolver: Arc<MonotonicEndpointResolver>,
        semantic_bridge: Arc<NetCellSemanticSyncBridge>,
        inspector: NetCellHostInspector,
    ) -> Self {
        Self {
            host,
            transport,
            resolver,
            semantic_bridge,
            inspector,
            replay_cache: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    fn current_epoch(&self) -> CellEpoch {
        self.host
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .fence()
            .epoch()
    }

    #[allow(clippy::too_many_lines)]
    async fn dispatch(&self, request: IncomingRpcRequest) -> RpcHandlerReply {
        let observed_epoch = self.current_epoch();
        if request.payload.len() < 9 || &request.payload[..8] != NET_MIGRATION_RPC_MAGIC {
            return encode_err_reply(
                NET_MIGRATION_RPC_MAGIC,
                observed_epoch,
                "invalid NLOSMG01 header",
            );
        }
        let op = request.payload[8];
        let mut r = WireReader::new(&request.payload[9..]);
        let Ok(caller_cell) = r.get_cell() else {
            return encode_err_reply(
                NET_MIGRATION_RPC_MAGIC,
                observed_epoch,
                "missing caller_cell in NLOSMG01 frame",
            );
        };
        let authenticated_peer = request.peer_session.peer_claims.cell_identity;
        if caller_cell != authenticated_peer {
            return encode_err_reply(
                NET_MIGRATION_RPC_MAGIC,
                observed_epoch,
                "caller_cell mismatches TLS 1.3 peer_claims.cell_identity",
            );
        }

        if request.side_effecting
            && let Some(cached_body) = self
                .replay_cache
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .get(&(authenticated_peer, request.idempotency_key))
                .cloned()
        {
            return encode_ok_reply(self.current_epoch(), true, &cached_body);
        }

        let result: Result<Vec<u8>, String> = match op {
            MG_OP_EXCHANGE_BOOTSTRAP => self.exec_exchange_bootstrap(caller_cell, &mut r),
            MG_OP_INSPECT_PLACEMENT => self.exec_inspect_placement(&mut r),
            MG_OP_VERIFY_CHECKPOINT => self.exec_verify_checkpoint(&mut r),
            MG_OP_PULL_SEMANTIC_DELTA => self.exec_pull_semantic_delta(caller_cell, &mut r).await,
            MG_OP_REBIND_CAPABILITIES => self.exec_rebind_capabilities(&mut r),
            MG_OP_REGRANT_LEASES => self.exec_regrant_leases(&mut r),
            MG_OP_RESTORE_PROCESS => self.exec_restore_process(&mut r),
            MG_OP_RECORD_RECEIPT_CKPT => self.exec_record_receipt_checkpoint(&mut r),
            MG_OP_INSPECT_MIGRATION_STATE => self.exec_inspect_migration_state(&mut r),
            other => Err(format!("unknown NLOSMG01 op tag {other}")),
        };

        match result {
            Ok(body) => {
                let reply = encode_ok_reply(self.current_epoch(), false, &body);
                if request.side_effecting {
                    self.replay_cache
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .insert((authenticated_peer, request.idempotency_key), body);
                }
                reply
            }
            Err(err) => encode_err_reply(NET_MIGRATION_RPC_MAGIC, self.current_epoch(), &err),
        }
    }

    fn exec_exchange_bootstrap(
        &self,
        caller_cell: CellIdentity,
        r: &mut WireReader<'_>,
    ) -> Result<Vec<u8>, String> {
        let caller_addr_opt = r
            .get_opt(|rd| {
                let s = rd.get_str()?;
                s.parse::<SocketAddr>()
                    .map_err(|_| "invalid caller SocketAddr")
            })
            .map_err(str::to_owned)?;
        let caller_boot_gen = r.get_gen().map_err(str::to_owned)?;
        let caller_epoch = r.get_epoch().map_err(str::to_owned)?;
        let caller_seq = r.get_u64().map_err(str::to_owned)?;
        let peer_reg = r.get_registration().map_err(str::to_owned)?;
        let peer_principal = r.get_bootstrap_principal().map_err(str::to_owned)?;
        r.finish().map_err(str::to_owned)?;

        if let Some(addr) = caller_addr_opt {
            let _ = self.resolver.upsert_hint(EndpointHintRecord::new(
                caller_cell,
                addr,
                EndpointRoutingVersion::new(caller_boot_gen, caller_epoch, caller_seq),
                1,
            ));
        }

        let mut guard = self.host.lock().unwrap_or_else(PoisonError::into_inner);
        {
            let dir = guard
                .cell_directory()
                .ok_or_else(|| "Target local CellDirectory missing".to_owned())?;
            dir.publish_registration(&peer_reg)
                .map_err(|e| format!("publish peer registration failed: {e}"))?;
        }
        let _ = guard
            .runtime()
            .identity
            .bootstrap_principal(peer_principal)
            .map_err(|e| format!("bootstrap peer principal failed: {e}"))?;
        let local_reg = guard
            .refresh_cell_registration()
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "Target local CellDirectory missing".to_owned())?;
        let local_principal = guard
            .semantic_writer_bootstrap_request()
            .map_err(|e| e.to_string())?;
        let local_cell = guard.fence().identity();
        drop(guard);

        let mut w = WireWriter::new(b"NLOSBGLK", 0);
        w.buf.clear();
        w.put_cell(local_cell);
        w.put_registration(&local_reg);
        w.put_bootstrap_principal(&local_principal);
        Ok(w.into_vec())
    }

    fn exec_inspect_placement(&self, r: &mut WireReader<'_>) -> Result<Vec<u8>, String> {
        let expected_source = r.get_cell().map_err(str::to_owned)?;
        r.finish().map_err(str::to_owned)?;

        let mut guard = self.host.lock().unwrap_or_else(PoisonError::into_inner);
        let ep = LocalCellHostTargetEndpoint::new(&mut guard);
        let snap = ep
            .inspect_target_placement(expected_source)
            .map_err(|e| e.to_string())?;
        drop(guard);

        let mut w = WireWriter::new(b"NLOSBGLK", 0);
        w.buf.clear();
        w.put_cell(snap.cell_id);
        w.put_fence(snap.fence);
        w.put_bool(snap.both_cells_registered_in_directory);
        Ok(w.into_vec())
    }

    fn exec_verify_checkpoint(&self, r: &mut WireReader<'_>) -> Result<Vec<u8>, String> {
        let migration_mode = r.get_migration_mode().map_err(str::to_owned)?;
        let spec = match r.get_u8().map_err(str::to_owned)? {
            0 => CheckpointSourceSpec::TransparentState {
                state_payload: r.get_bytes().map_err(str::to_owned)?,
                cache_key: r.get_opt(WireReader::get_str).map_err(str::to_owned)?,
                checkpointed_at_ms: r.get_u64().map_err(str::to_owned)?,
            },
            1 => CheckpointSourceSpec::ArtifactRestartImage {
                artifact_id: ArtifactId::from_bytes(r.get_arr16().map_err(str::to_owned)?),
                revision: r.get_u64().map_err(str::to_owned)?,
                sync_from_source_if_missing: r.get_bool().map_err(str::to_owned)?,
                checkpointed_at_ms: r.get_u64().map_err(str::to_owned)?,
            },
            _ => return Err("invalid CheckpointSourceSpec tag".to_owned()),
        };
        let proof = r.get_checkpoint_proof().map_err(str::to_owned)?;
        let transparent_cache_transfer = r
            .get_opt(|rd| Ok((rd.get_str()?, rd.get_bytes()?, rd.get_u64()?)))
            .map_err(str::to_owned)?;
        let sync_seed = r.get_u8().map_err(str::to_owned)?;
        r.finish().map_err(str::to_owned)?;

        let req = TargetCheckpointVerifyRequest {
            migration_mode,
            spec,
            source_bundle: SourceCheckpointBundle {
                proof,
                transparent_cache_transfer,
                exported_artifact_bundle: None,
            },
            sync_seed,
        };
        let mut guard = self.host.lock().unwrap_or_else(PoisonError::into_inner);
        let mut ep = LocalCellHostTargetEndpoint::new(&mut guard);
        ep.verify_checkpoint_on_target(&req)
            .map_err(|e| e.to_string())?;
        Ok(Vec::new())
    }

    async fn exec_pull_semantic_delta(
        &self,
        source_cell: CellIdentity,
        r: &mut WireReader<'_>,
    ) -> Result<Vec<u8>, String> {
        let base_idempotency_key =
            IdempotencyKey::from_bytes(r.get_arr16().map_err(str::to_owned)?);
        let max_batch_events = r.get_u32().map_err(str::to_owned)?;
        let rpc_deadline_ms = r.get_u64().map_err(str::to_owned)?;
        let signed_at_ms = r.get_u64().map_err(str::to_owned)?;
        r.finish().map_err(str::to_owned)?;

        let client =
            SemanticNetSyncClient::new(self.transport.as_ref(), self.semantic_bridge.as_ref());
        let report = client
            .pull_and_merge_from_peer(
                source_cell,
                SemanticNetSyncConfig {
                    max_batch_events: usize::try_from(max_batch_events).unwrap_or(32),
                    rpc_deadline_ms,
                    base_idempotency_key,
                    signed_at_ms,
                },
            )
            .await
            .map_err(|e| format!("SemanticNetSyncClient pull_and_merge_from_peer failed: {e}"))?;

        let imported = report.convergence.as_ref().map_or(0_u32, |c| {
            u32::try_from(c.imported_event_ids.len()).unwrap_or(u32::MAX)
        });
        let mut w = WireWriter::new(b"NLOSBGLK", 0);
        w.buf.clear();
        w.put_u32(u32::try_from(report.batches_pulled).unwrap_or(u32::MAX));
        w.put_u32(u32::try_from(report.pulled_event_count).unwrap_or(u32::MAX));
        w.put_u32(imported);
        w.put_arr32(&report.post_sync_local_checkpoint.checkpoint_digest);
        w.put_arr32(&report.remote_checkpoint.checkpoint_digest);
        Ok(w.into_vec())
    }

    fn exec_rebind_capabilities(&self, r: &mut WireReader<'_>) -> Result<Vec<u8>, String> {
        let process_id = ProcessId::from_bytes(r.get_arr16().map_err(str::to_owned)?);
        let source_cell_id = r.get_cell().map_err(str::to_owned)?;
        let source_generation = r.get_gen().map_err(str::to_owned)?;
        let target_cell_id = r.get_cell().map_err(str::to_owned)?;
        let target_generation = r.get_gen().map_err(str::to_owned)?;
        let ensure_target_epoch_above = r.get_opt(WireReader::get_epoch).map_err(str::to_owned)?;
        let item_count = usize::try_from(r.get_u64().map_err(str::to_owned)?)
            .map_err(|_| "item count overflow".to_owned())?;
        let mut items = Vec::with_capacity(item_count.min(64));
        for _ in 0..item_count {
            let bp_count = usize::try_from(r.get_u64().map_err(str::to_owned)?)
                .map_err(|_| "bp count overflow".to_owned())?;
            let mut bootstrap_principals = Vec::with_capacity(bp_count.min(32));
            for _ in 0..bp_count {
                bootstrap_principals.push(r.get_bootstrap_principal().map_err(str::to_owned)?);
            }
            let seed_parent_root = r
                .get_opt(WireReader::get_signed_root_cap)
                .map_err(str::to_owned)?;
            let signed_delegation = r.get_signed_delegate_cap().map_err(str::to_owned)?;
            let target_name_path = r
                .get_opt(|rd| {
                    let s = rd.get_str()?;
                    NamePath::new(&s).map_err(|_| "invalid NamePath")
                })
                .map_err(str::to_owned)?;
            items.push(CapabilityRebindItemSpec {
                bootstrap_principals,
                seed_parent_root,
                signed_delegation,
                target_name_path,
                revoke_on_source: None,
            });
        }
        let rebound_at_ms = r.get_u64().map_err(str::to_owned)?;
        r.finish().map_err(str::to_owned)?;

        let req = TargetCapabilityRebindRequest {
            process_id,
            source_cell_id,
            source_generation,
            target_cell_id,
            target_generation,
            ensure_target_epoch_above,
            items,
            rebound_at_ms,
        };
        let mut guard = self.host.lock().unwrap_or_else(PoisonError::into_inner);
        let mut ep = LocalCellHostTargetEndpoint::new(&mut guard);
        let report = ep
            .rebind_capabilities_on_target(&req)
            .map_err(|e| e.to_string())?;
        drop(guard);

        let mut w = WireWriter::new(b"NLOSBGLK", 0);
        w.buf.clear();
        w.put_arr16(report.proof.process_id.as_bytes());
        w.put_u64(u64::try_from(report.proof.rebound_capabilities.len()).unwrap_or(0));
        for entry in &report.proof.rebound_capabilities {
            w.put_arr16(&entry.capability_id);
            w.put_arr16(&entry.source_cell_id);
            w.put_u64(entry.source_generation);
            w.put_arr16(&entry.target_cell_id);
            w.put_u64(entry.target_generation);
            w.put_arr32(&entry.attenuation_digest);
        }
        w.put_u64(report.proof.rebound_at_ms);
        w.put_u64(u64::try_from(report.delegated_records.len()).unwrap_or(0));
        for (rec, rcpt) in &report.delegated_records {
            w.put_cap_record(rec);
            w.put_cap_issue_receipt(rcpt);
        }
        w.put_fence(report.target_fence);
        Ok(w.into_vec())
    }

    fn exec_regrant_leases(&self, r: &mut WireReader<'_>) -> Result<Vec<u8>, String> {
        let process_id = ProcessId::from_bytes(r.get_arr16().map_err(str::to_owned)?);
        let source_cell_id = r.get_cell().map_err(str::to_owned)?;
        let source_generation = r.get_gen().map_err(str::to_owned)?;
        let target_cell_id = r.get_cell().map_err(str::to_owned)?;
        let target_generation = r.get_gen().map_err(str::to_owned)?;
        let spec = r.get_lease_bundle_spec().map_err(str::to_owned)?;
        let source_closed = r.get_source_closed_leases().map_err(str::to_owned)?;
        r.finish().map_err(str::to_owned)?;

        let req = TargetLeaseRegrantRequest {
            process_id,
            source_cell_id,
            source_generation,
            target_cell_id,
            target_generation,
            spec,
            source_closed,
        };
        let mut guard = self.host.lock().unwrap_or_else(PoisonError::into_inner);
        let mut ep = LocalCellHostTargetEndpoint::new(&mut guard);
        let report = ep
            .regrant_leases_on_target(&req)
            .map_err(|e| e.to_string())?;
        drop(guard);

        let holder_bytes = *process_id.as_bytes();
        self.inspector
            .track_quota_lease(report.target_quota.lease_id(), holder_bytes, 30_000);
        if let Some(cap) = &report.target_capacity {
            self.inspector
                .track_capacity_lease(cap.capacity_lease_id(), holder_bytes, 30_000);
        }
        if let Some(dev) = &report.target_device {
            self.inspector
                .track_device_lease(dev.device_lease_id(), holder_bytes, 30_000);
        }

        let mut w = WireWriter::new(b"NLOSBGLK", 0);
        w.buf.clear();
        w.put_arr16(report.proof.process_id.as_bytes());
        w.put_u64(report.proof.source_generation);
        w.put_u64(report.proof.target_generation);
        w.put_u64(u64::try_from(report.proof.migrated_leases.len()).unwrap_or(0));
        for entry in &report.proof.migrated_leases {
            w.put_arr16(&entry.source_lease_id);
            w.put_arr16(&entry.source_cell_id);
            w.put_bool(entry.source_closed_or_fenced);
            w.put_arr32(&entry.source_close_receipt_digest);
            w.put_arr16(&entry.target_lease_id);
            w.put_arr16(&entry.target_cell_id);
            w.put_u64(entry.target_grant_epoch);
        }
        w.put_u64(report.proof.regranted_at_ms);

        // Also return the verified Target lease IDs & epochs so callers can inspect them.
        w.put_arr16(report.target_quota.lease_id().as_bytes());
        w.put_u64(report.target_quota.face_value());
        w.put_epoch(report.target_quota.epoch());
        w.put_bool(report.target_quota.state() == QuotaLeaseState::Active);
        w.put_opt(report.target_capacity.as_ref(), |w, c| {
            w.put_arr16(c.capacity_lease_id().as_bytes());
            w.put_u64(c.amount());
            w.put_epoch(c.capacity_epoch());
        });
        w.put_opt(report.target_device.as_ref(), |w, d| {
            w.put_arr16(d.device_lease_id().as_bytes());
            w.put_arr16(d.device_id().as_bytes());
            w.put_epoch(d.exclusivity_epoch());
            w.put_gen(d.reset_generation());
        });
        Ok(w.into_vec())
    }

    fn exec_restore_process(&self, r: &mut WireReader<'_>) -> Result<Vec<u8>, String> {
        let process_id = ProcessId::from_bytes(r.get_arr16().map_err(str::to_owned)?);
        let target_cell_id = r.get_cell().map_err(str::to_owned)?;
        let source_generation = r.get_gen().map_err(str::to_owned)?;
        let source_fencing_token = r.get_arr32().map_err(str::to_owned)?;
        let target_generation = r.get_gen().map_err(str::to_owned)?;
        let genesis = ProcessLineageGenesis {
            task_id: TaskId::from_bytes(r.get_arr16().map_err(str::to_owned)?),
            task_attempt_id: TaskAttemptId::from_bytes(r.get_arr16().map_err(str::to_owned)?),
            attempt_generation: r.get_gen().map_err(str::to_owned)?,
            idempotency_key: IdempotencyKey::from_bytes(r.get_arr16().map_err(str::to_owned)?),
            created_at_ms: r.get_u64().map_err(str::to_owned)?,
        };
        let hop_count = usize::try_from(r.get_u64().map_err(str::to_owned)?)
            .map_err(|_| "hop count overflow".to_owned())?;
        let mut prior_restores = Vec::with_capacity(hop_count.min(64));
        for _ in 0..hop_count {
            prior_restores.push(ProcessLineageRestoreHop {
                expected_generation: r.get_gen().map_err(str::to_owned)?,
                expected_fencing_token: r.get_arr32().map_err(str::to_owned)?,
                idempotency_key: IdempotencyKey::from_bytes(r.get_arr16().map_err(str::to_owned)?),
                restored_at_ms: r.get_u64().map_err(str::to_owned)?,
            });
        }
        let existing_target_domain_id = r
            .get_opt(|rd| Ok(IsolationDomainId::from_bytes(rd.get_arr16()?)))
            .map_err(str::to_owned)?;
        let target_domain_policy_digest = r.get_arr32().map_err(str::to_owned)?;
        let target_domain_idempotency_key =
            IdempotencyKey::from_bytes(r.get_arr16().map_err(str::to_owned)?);
        let restore_idempotency_key =
            IdempotencyKey::from_bytes(r.get_arr16().map_err(str::to_owned)?);
        let restored_image_or_checkpoint_digest = r.get_arr32().map_err(str::to_owned)?;
        let restored_at_ms = r.get_u64().map_err(str::to_owned)?;
        r.finish().map_err(str::to_owned)?;

        let req = TargetProcessRestoreRequest {
            process_id,
            target_cell_id,
            source_generation,
            source_fencing_token,
            target_generation,
            lineage: ProcessLineageSpec {
                genesis,
                prior_restores,
            },
            existing_target_domain_id,
            target_domain_policy_digest,
            target_domain_idempotency_key,
            restore_idempotency_key,
            restored_image_or_checkpoint_digest,
            restored_at_ms,
        };
        let mut guard = self.host.lock().unwrap_or_else(PoisonError::into_inner);
        let mut ep = LocalCellHostTargetEndpoint::new(&mut guard);
        let report = ep
            .restore_process_on_target(&req)
            .map_err(|e| e.to_string())?;
        drop(guard);

        let mut w = WireWriter::new(b"NLOSBGLK", 0);
        w.buf.clear();
        w.put_process_binding(&report.restored_binding);
        w.put_arr16(report.proof.process_id.as_bytes());
        w.put_arr16(&report.proof.target_cell_id);
        w.put_u64(report.proof.restored_generation);
        w.put_arr32(&report.proof.target_fencing_token);
        w.put_arr16(report.proof.target_isolation_domain_id.as_bytes());
        w.put_arr32(&report.proof.restored_image_or_checkpoint_digest);
        w.put_u64(report.proof.restored_at_ms);
        w.put_u32(report.registered_target_os_pid);
        Ok(w.into_vec())
    }

    fn exec_record_receipt_checkpoint(&self, r: &mut WireReader<'_>) -> Result<Vec<u8>, String> {
        let receipt = r.get_migration_receipt().map_err(str::to_owned)?;
        r.finish().map_err(str::to_owned)?;

        let mut guard = self.host.lock().unwrap_or_else(PoisonError::into_inner);
        let mut ep = LocalCellHostTargetEndpoint::new(&mut guard);
        let report = ep
            .record_receipt_checkpoint_on_target(&receipt)
            .map_err(|e| e.to_string())?;
        let target_cell_id = guard.fence().identity();
        drop(guard);

        let fact = report.checkpoint_record.fact();
        let mut w = WireWriter::new(b"NLOSBGLK", 0);
        w.buf.clear();
        w.put_cell(target_cell_id);
        let entries: Vec<&[u8]> = report
            .checkpoint_prefix
            .entries()
            .iter()
            .map(Vec::as_slice)
            .collect();
        w.put_u64(u64::try_from(entries.len()).unwrap_or(0));
        for e in entries {
            w.put_bytes(e);
        }
        w.put_gen(fact.node_boot_generation());
        w.put_epoch(fact.epoch());
        w.put_u64(fact.fencing_token().get());
        w.put_str(fact.digest());
        w.put_u64(report.checkpoint_record.recorded_ms());
        w.put_u32(report.checkpoint_record.os_process_id());
        w.put_u64(report.checkpoint_record.sequence());
        Ok(w.into_vec())
    }

    fn exec_inspect_migration_state(&self, r: &mut WireReader<'_>) -> Result<Vec<u8>, String> {
        let process_id = ProcessId::from_bytes(r.get_arr16().map_err(str::to_owned)?);
        r.finish().map_err(str::to_owned)?;

        let guard = self.host.lock().unwrap_or_else(PoisonError::into_inner);
        let target_cell_id = guard.fence().identity();
        let (active_generation, lifecycle_state) = match guard
            .runtime()
            .process
            .inspect_active_process_binding(process_id)
        {
            Ok(binding) => (
                Some(binding.process_generation),
                Some(ProcessLifecycleState::Active),
            ),
            Err(ProcessAuthorityError::ProcessBindingTerminal(st)) => (None, Some(st)),
            Err(ProcessAuthorityError::ProcessNotFound(_)) => (None, None),
            Err(other) => return Err(format!("inspect_active_process_binding failed: {other}")),
        };
        let supervisor_registered = active_generation.is_some_and(|active_gen| {
            guard
                .supervisor_pids()
                .lookup(process_id)
                .is_ok_and(|sup| sup.process_generation == active_gen)
        });
        let recorded_receipt_digest = guard.cell_directory().and_then(|dir| {
            let log = dir.checkpoints(target_cell_id).ok()?;
            for ckpt in log.into_records().into_iter().rev() {
                if let Some(hex_body) = ckpt.fact().digest().strip_prefix("v1:sha256:")
                    && hex_body.len() == 64
                {
                    let mut decoded = [0_u8; 32];
                    let mut ok = true;
                    for (idx, pair) in hex_body.as_bytes().as_chunks::<2>().0.iter().enumerate() {
                        let s = std::str::from_utf8(pair).ok()?;
                        if let Ok(b) = u8::from_str_radix(s, 16) {
                            decoded[idx] = b;
                        } else {
                            ok = false;
                            break;
                        }
                    }
                    if ok {
                        return Some(decoded);
                    }
                }
            }
            None
        });
        drop(guard);

        let mut w = WireWriter::new(b"NLOSBGLK", 0);
        w.buf.clear();
        w.put_cell(target_cell_id);
        w.put_opt(active_generation, WireWriter::put_gen);
        w.put_opt(lifecycle_state, |w, st| {
            let code = match st {
                ProcessLifecycleState::Active => 1_u8,
                ProcessLifecycleState::Terminated => 2_u8,
                ProcessLifecycleState::Crashed => 3_u8,
            };
            w.put_u8(code);
        });
        w.put_bool(supervisor_registered);
        w.put_opt(recorded_receipt_digest, |w, d| w.put_arr32(&d));
        Ok(w.into_vec())
    }
}

impl SubProtocolRpcHandler for NetMigrationRpcServer {
    fn matches_frame(&self, payload: &[u8]) -> bool {
        payload.len() >= 8 && &payload[..8] == NET_MIGRATION_RPC_MAGIC
    }

    fn handle_frame(
        &self,
        request: IncomingRpcRequest,
    ) -> Pin<Box<dyn Future<Output = RpcHandlerReply> + Send + 'static>> {
        let server = self.clone();
        Box::pin(async move { server.dispatch(request).await })
    }
}

// ============================================================================
// Client Orchestration: Bootstrap, 8-Step Migration & Post-Fence Reconciliation
// ============================================================================

enum RemoteStepCallError {
    NetworkCutOrUncertain(String),
    ApplicationRefused(String),
}

async fn send_migration_rpc(
    source_host: &NetCellHost,
    target_cell: CellIdentity,
    contract: &RemoteCallContract,
    payload: Vec<u8>,
) -> Result<Vec<u8>, RemoteStepCallError> {
    let outcome = source_host
        .transport()
        .call_cell_rpc(target_cell, contract, payload)
        .await
        .map_err(|refusal| RemoteStepCallError::NetworkCutOrUncertain(refusal.to_string()))?;

    match outcome {
        RemoteCallOutcome::Succeeded { value, .. } => {
            let body = decode_app_reply(NET_MIGRATION_RPC_MAGIC, &value.payload)
                .map_err(RemoteStepCallError::ApplicationRefused)?;
            Ok(body.to_vec())
        }
        RemoteCallOutcome::Unavailable { .. }
        | RemoteCallOutcome::DeadlineExceededUncertain { .. }
        | RemoteCallOutcome::Partial { .. } => Err(RemoteStepCallError::NetworkCutOrUncertain(
            format!("{outcome:?}"),
        )),
    }
}

/// Mutually bootstraps two isolated [`NetCellHost`] instances over TLS 1.3 mTLS
/// (`NLOSMG01`), registering bidirectional ephemeral routing hints, local
/// `CellDirectory` registration mirrors, and semantic writer principals with
/// **zero** shared filesystem paths.
///
/// # Errors
///
/// Fails closed if local export, mTLS transport, 5-axis peer verification, or
/// local registration/principal import fails.
pub async fn exchange_peer_bootstrap_over_mtls(
    local_host: &NetCellHost,
    peer_cell: CellIdentity,
    peer_addr: SocketAddr,
    routing_version: EndpointRoutingVersion,
    idempotency_key: IdempotencyKey,
) -> Result<CellRegistrationEntry, String> {
    local_host
        .upsert_peer_endpoint_hint(peer_cell, peer_addr, routing_version)
        .map_err(|e| e.to_string())?;
    let local_reg = local_host
        .export_local_registration()
        .map_err(|e| e.to_string())?;
    let local_principal = local_host
        .export_semantic_writer_principal()
        .map_err(|e| e.to_string())?;
    let local_fence = local_host.current_fence();

    let mut w = WireWriter::new(NET_MIGRATION_RPC_MAGIC, MG_OP_EXCHANGE_BOOTSTRAP);
    w.put_cell(local_fence.identity());
    w.put_opt(local_host.service_addr(), |wr, addr| {
        wr.put_str(&addr.to_string());
    });
    w.put_gen(local_fence.node_boot_generation());
    w.put_epoch(local_fence.epoch());
    w.put_u64(routing_version.heartbeat_seq);
    w.put_registration(&local_reg);
    w.put_bootstrap_principal(&local_principal);

    let contract = build_rpc_contract(idempotency_key, 5_000, true);
    let body = send_migration_rpc(local_host, peer_cell, &contract, w.into_vec())
        .await
        .map_err(|e| match e {
            RemoteStepCallError::NetworkCutOrUncertain(s)
            | RemoteStepCallError::ApplicationRefused(s) => s,
        })?;
    let mut r = WireReader::new(&body);
    let responder_cell = r.get_cell().map_err(str::to_owned)?;
    if responder_cell != peer_cell {
        return Err("responder_cell mismatch during mTLS peer bootstrap".to_owned());
    }
    let peer_reg = r.get_registration().map_err(str::to_owned)?;
    let peer_principal = r.get_bootstrap_principal().map_err(str::to_owned)?;
    r.finish().map_err(str::to_owned)?;

    local_host
        .record_peer_registration_locally(&peer_reg)
        .map_err(|e| e.to_string())?;
    local_host
        .register_peer_principal(peer_principal)
        .map_err(|e| e.to_string())?;
    Ok(peer_reg)
}

/// Queries Target's authoritative process migration state over mTLS (`side_effecting = false`).
///
/// # Errors
///
/// Fails closed if mTLS transport is unavailable or the wire frame is malformed.
pub async fn inspect_remote_target_migration_state(
    source_host: &NetCellHost,
    target_cell: CellIdentity,
    process_id: ProcessId,
    idempotency_key: IdempotencyKey,
    deadline_ms: u64,
) -> Result<TargetMigrationInspectionSnapshot, String> {
    let mut w = WireWriter::new(NET_MIGRATION_RPC_MAGIC, MG_OP_INSPECT_MIGRATION_STATE);
    w.put_cell(source_host.local_cell());
    w.put_arr16(process_id.as_bytes());
    let contract = build_rpc_contract(idempotency_key, deadline_ms, false);
    let body = send_migration_rpc(source_host, target_cell, &contract, w.into_vec())
        .await
        .map_err(|e| match e {
            RemoteStepCallError::NetworkCutOrUncertain(s)
            | RemoteStepCallError::ApplicationRefused(s) => s,
        })?;
    let mut r = WireReader::new(&body);
    let target_cell_id = r.get_cell().map_err(str::to_owned)?;
    let active_generation = r.get_opt(WireReader::get_gen).map_err(str::to_owned)?;
    let lifecycle_state = r
        .get_opt(|rd| match rd.get_u8()? {
            1 => Ok(ProcessLifecycleState::Active),
            2 => Ok(ProcessLifecycleState::Terminated),
            3 => Ok(ProcessLifecycleState::Crashed),
            _ => Err("invalid ProcessLifecycleState code"),
        })
        .map_err(str::to_owned)?;
    let supervisor_registered = r.get_bool().map_err(str::to_owned)?;
    let recorded_receipt_digest = r.get_opt(WireReader::get_arr32).map_err(str::to_owned)?;
    r.finish().map_err(str::to_owned)?;
    Ok(TargetMigrationInspectionSnapshot {
        target_cell_id,
        active_generation,
        lifecycle_state,
        supervisor_registered,
        recorded_receipt_digest,
    })
}

async fn call_remote_inspect_target_placement(
    source_host: &NetCellHost,
    target_cell: CellIdentity,
    migration_id: [u8; 16],
    deadline_ms: u64,
) -> Result<TargetCellPlacementSnapshot, ProcessMigrationDriveError> {
    let mut w = WireWriter::new(NET_MIGRATION_RPC_MAGIC, MG_OP_INSPECT_PLACEMENT);
    w.put_cell(source_host.local_cell());
    w.put_cell(source_host.local_cell());
    let contract = build_rpc_contract(
        derive_migration_rpc_key(migration_id, STEP_INSPECT_TARGET),
        deadline_ms,
        false,
    );
    let body = send_migration_rpc(source_host, target_cell, &contract, w.into_vec())
        .await
        .map_err(|e| match e {
            RemoteStepCallError::NetworkCutOrUncertain(s) => {
                ProcessMigrationDriveError::Transport(s)
            }
            RemoteStepCallError::ApplicationRefused(s) => ProcessMigrationDriveError::Transport(s),
        })?;
    let mut r = WireReader::new(&body);
    let cell_id = r
        .get_cell()
        .map_err(|s| ProcessMigrationDriveError::Transport(s.to_owned()))?;
    let fence = r
        .get_fence()
        .map_err(|s| ProcessMigrationDriveError::Transport(s.to_owned()))?;
    let both_cells_registered_in_directory = r
        .get_bool()
        .map_err(|s| ProcessMigrationDriveError::Transport(s.to_owned()))?;
    r.finish()
        .map_err(|s| ProcessMigrationDriveError::Transport(s.to_owned()))?;
    Ok(TargetCellPlacementSnapshot {
        cell_id,
        fence,
        both_cells_registered_in_directory,
    })
}

async fn call_remote_pull_semantic_delta(
    source_host: &NetCellHost,
    target_cell: CellIdentity,
    migration_id: [u8; 16],
    max_batch_events: u32,
    deadline_ms: u64,
    signed_at_ms: u64,
) -> Result<CrossSocketSemanticSyncSummary, CrossSocketMigrationError> {
    let rpc_key = derive_migration_rpc_key(migration_id, STEP_PULL_SEMANTIC);
    let base_sync_key = derive_migration_rpc_key(migration_id, 0x91);
    let mut w = WireWriter::new(NET_MIGRATION_RPC_MAGIC, MG_OP_PULL_SEMANTIC_DELTA);
    w.put_cell(source_host.local_cell());
    w.put_arr16(base_sync_key.as_bytes());
    w.put_u32(max_batch_events.max(1));
    w.put_u64(deadline_ms);
    w.put_u64(signed_at_ms);

    let contract = build_rpc_contract(rpc_key, deadline_ms, true);
    let body = send_migration_rpc(source_host, target_cell, &contract, w.into_vec())
        .await
        .map_err(|e| match e {
            RemoteStepCallError::NetworkCutOrUncertain(s)
            | RemoteStepCallError::ApplicationRefused(s) => {
                CrossSocketMigrationError::SemanticSync(s)
            }
        })?;
    let mut r = WireReader::new(&body);
    let summary = CrossSocketSemanticSyncSummary {
        batches_pulled: r
            .get_u32()
            .map_err(|s| CrossSocketMigrationError::SemanticSync(s.to_owned()))?,
        pulled_event_count: r
            .get_u32()
            .map_err(|s| CrossSocketMigrationError::SemanticSync(s.to_owned()))?,
        imported_events: r
            .get_u32()
            .map_err(|s| CrossSocketMigrationError::SemanticSync(s.to_owned()))?,
        target_checkpoint_digest: r
            .get_arr32()
            .map_err(|s| CrossSocketMigrationError::SemanticSync(s.to_owned()))?,
        source_checkpoint_digest: r
            .get_arr32()
            .map_err(|s| CrossSocketMigrationError::SemanticSync(s.to_owned()))?,
    };
    r.finish()
        .map_err(|s| CrossSocketMigrationError::SemanticSync(s.to_owned()))?;
    Ok(summary)
}

#[allow(clippy::too_many_arguments)]
async fn call_remote_verify_checkpoint(
    source_host: &NetCellHost,
    target_cell: CellIdentity,
    migration_id: [u8; 16],
    migration_mode: &MigrationMode,
    spec: &CheckpointSourceSpec,
    bundle: &SourceCheckpointBundle,
    sync_seed: u8,
    deadline_ms: u64,
) -> Result<(), ProcessMigrationDriveError> {
    let mut w = WireWriter::new(NET_MIGRATION_RPC_MAGIC, MG_OP_VERIFY_CHECKPOINT);
    w.put_cell(source_host.local_cell());
    w.put_migration_mode(migration_mode);
    match spec {
        CheckpointSourceSpec::TransparentState {
            state_payload,
            cache_key,
            checkpointed_at_ms,
        } => {
            w.put_u8(0);
            w.put_bytes(state_payload);
            w.put_opt(cache_key.as_deref(), WireWriter::put_str);
            w.put_u64(*checkpointed_at_ms);
        }
        CheckpointSourceSpec::ArtifactRestartImage {
            artifact_id,
            revision,
            checkpointed_at_ms,
            ..
        } => {
            // Restart artifact was already streamed over chunked `ArtifactNetTransferClient`;
            // Target verifies `(artifact_id, revision)` directly from Target's `ArtifactStore`.
            w.put_u8(1);
            w.put_arr16(artifact_id.as_bytes());
            w.put_u64(*revision);
            w.put_bool(false);
            w.put_u64(*checkpointed_at_ms);
        }
    }
    w.put_checkpoint_proof(&bundle.proof);
    w.put_opt(
        bundle.transparent_cache_transfer.as_ref(),
        |wr, (k, b, ts)| {
            wr.put_str(k);
            wr.put_bytes(b);
            wr.put_u64(*ts);
        },
    );
    w.put_u8(sync_seed);

    let contract = build_rpc_contract(
        derive_migration_rpc_key(migration_id, STEP_VERIFY_CHECKPOINT),
        deadline_ms,
        true,
    );
    send_migration_rpc(source_host, target_cell, &contract, w.into_vec())
        .await
        .map_err(|e| match e {
            RemoteStepCallError::NetworkCutOrUncertain(s) => {
                ProcessMigrationDriveError::Transport(s)
            }
            RemoteStepCallError::ApplicationRefused(s) => {
                ProcessMigrationDriveError::CheckpointVerification(s)
            }
        })?;
    Ok(())
}

async fn call_remote_rebind_capabilities(
    source_host: &NetCellHost,
    target_cell: CellIdentity,
    idempotency_key: IdempotencyKey,
    req: &TargetCapabilityRebindRequest,
    deadline_ms: u64,
) -> Result<TargetCapabilityRebindReport, RemoteStepCallError> {
    let mut w = WireWriter::new(NET_MIGRATION_RPC_MAGIC, MG_OP_REBIND_CAPABILITIES);
    w.put_cell(source_host.local_cell());
    w.put_arr16(req.process_id.as_bytes());
    w.put_cell(req.source_cell_id);
    w.put_gen(req.source_generation);
    w.put_cell(req.target_cell_id);
    w.put_gen(req.target_generation);
    w.put_opt(req.ensure_target_epoch_above, WireWriter::put_epoch);
    w.put_u64(u64::try_from(req.items.len()).unwrap_or(0));
    for item in &req.items {
        w.put_u64(u64::try_from(item.bootstrap_principals.len()).unwrap_or(0));
        for bp in &item.bootstrap_principals {
            w.put_bootstrap_principal(bp);
        }
        w.put_opt(
            item.seed_parent_root.as_ref(),
            WireWriter::put_signed_root_cap,
        );
        w.put_signed_delegate_cap(&item.signed_delegation);
        w.put_opt(item.target_name_path.as_ref(), |wr, np| {
            wr.put_str(np.as_str());
        });
    }
    w.put_u64(req.rebound_at_ms);

    let contract = build_rpc_contract(idempotency_key, deadline_ms, true);
    let body = send_migration_rpc(source_host, target_cell, &contract, w.into_vec()).await?;
    let mut r = WireReader::new(&body);
    let process_id = ProcessId::from_bytes(
        r.get_arr16()
            .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?,
    );
    let reb_len = usize::try_from(
        r.get_u64()
            .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?,
    )
    .unwrap_or(0);
    let mut rebound_capabilities = Vec::with_capacity(reb_len.min(64));
    for _ in 0..reb_len {
        rebound_capabilities.push(ReboundCapabilityEntry {
            capability_id: r
                .get_arr16()
                .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?,
            source_cell_id: r
                .get_arr16()
                .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?,
            source_generation: r
                .get_u64()
                .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?,
            target_cell_id: r
                .get_arr16()
                .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?,
            target_generation: r
                .get_u64()
                .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?,
            attenuation_digest: r
                .get_arr32()
                .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?,
        });
    }
    let rebound_at_ms = r
        .get_u64()
        .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?;
    let del_len = usize::try_from(
        r.get_u64()
            .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?,
    )
    .unwrap_or(0);
    let mut delegated_records = Vec::with_capacity(del_len.min(64));
    for _ in 0..del_len {
        let rec = r
            .get_cap_record()
            .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?;
        let rcpt = r
            .get_cap_issue_receipt()
            .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?;
        delegated_records.push((rec, rcpt));
    }
    let target_fence = r
        .get_fence()
        .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?;
    r.finish()
        .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?;

    Ok(TargetCapabilityRebindReport {
        proof: CapabilityRebindProof {
            process_id,
            rebound_capabilities,
            rebound_at_ms,
        },
        delegated_records,
        target_fence,
    })
}

#[allow(clippy::too_many_lines)]
async fn call_remote_regrant_leases(
    source_host: &NetCellHost,
    target_cell: CellIdentity,
    idempotency_key: IdempotencyKey,
    req: &TargetLeaseRegrantRequest,
    deadline_ms: u64,
) -> Result<RemoteTargetLeaseRegrantSummary, RemoteStepCallError> {
    let mut w = WireWriter::new(NET_MIGRATION_RPC_MAGIC, MG_OP_REGRANT_LEASES);
    w.put_cell(source_host.local_cell());
    w.put_arr16(req.process_id.as_bytes());
    w.put_cell(req.source_cell_id);
    w.put_gen(req.source_generation);
    w.put_cell(req.target_cell_id);
    w.put_gen(req.target_generation);
    w.put_lease_bundle_spec(&req.spec);
    w.put_source_closed_leases(&req.source_closed);

    let contract = build_rpc_contract(idempotency_key, deadline_ms, true);
    let body = send_migration_rpc(source_host, target_cell, &contract, w.into_vec()).await?;
    let mut r = WireReader::new(&body);
    let process_id = ProcessId::from_bytes(
        r.get_arr16()
            .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?,
    );
    let source_generation = r
        .get_u64()
        .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?;
    let target_generation = r
        .get_u64()
        .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?;
    let mig_len = usize::try_from(
        r.get_u64()
            .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?,
    )
    .unwrap_or(0);
    let mut migrated_leases = Vec::with_capacity(mig_len.min(32));
    for _ in 0..mig_len {
        migrated_leases.push(MigratedLeaseEntry {
            source_lease_id: r
                .get_arr16()
                .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?,
            source_cell_id: r
                .get_arr16()
                .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?,
            source_closed_or_fenced: r
                .get_bool()
                .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?,
            source_close_receipt_digest: r
                .get_arr32()
                .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?,
            target_lease_id: r
                .get_arr16()
                .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?,
            target_cell_id: r
                .get_arr16()
                .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?,
            target_grant_epoch: r
                .get_u64()
                .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?,
        });
    }
    let regranted_at_ms = r
        .get_u64()
        .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?;

    let target_quota_lease_id = QuotaLeaseId::from_bytes(
        r.get_arr16()
            .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?,
    );
    let target_quota_face_value = r
        .get_u64()
        .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?;
    let target_quota_epoch = r
        .get_epoch()
        .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?;
    let target_quota_active = r
        .get_bool()
        .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?;
    let target_capacity = r
        .get_opt(|rd| {
            Ok((
                CapacityLeaseId::from_bytes(rd.get_arr16()?),
                rd.get_u64()?,
                rd.get_epoch()?,
            ))
        })
        .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?;
    let target_device = r
        .get_opt(|rd| {
            Ok((
                ExclusiveDeviceLeaseId::from_bytes(rd.get_arr16()?),
                DeviceId::from_bytes(rd.get_arr16()?),
                rd.get_epoch()?,
                rd.get_gen()?,
            ))
        })
        .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?;
    r.finish()
        .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?;

    Ok(RemoteTargetLeaseRegrantSummary {
        proof: LeaseCloseRegrantProof {
            process_id,
            source_generation,
            target_generation,
            migrated_leases,
            regranted_at_ms,
        },
        target_quota_lease_id,
        target_quota_face_value,
        target_quota_epoch,
        target_quota_active,
        target_capacity,
        target_device,
    })
}

async fn call_remote_restore_process(
    source_host: &NetCellHost,
    target_cell: CellIdentity,
    idempotency_key: IdempotencyKey,
    req: &TargetProcessRestoreRequest,
    deadline_ms: u64,
) -> Result<TargetProcessRestoreReport, RemoteStepCallError> {
    let mut w = WireWriter::new(NET_MIGRATION_RPC_MAGIC, MG_OP_RESTORE_PROCESS);
    w.put_cell(source_host.local_cell());
    w.put_arr16(req.process_id.as_bytes());
    w.put_cell(req.target_cell_id);
    w.put_gen(req.source_generation);
    w.put_arr32(&req.source_fencing_token);
    w.put_gen(req.target_generation);
    w.put_arr16(req.lineage.genesis.task_id.as_bytes());
    w.put_arr16(req.lineage.genesis.task_attempt_id.as_bytes());
    w.put_gen(req.lineage.genesis.attempt_generation);
    w.put_arr16(req.lineage.genesis.idempotency_key.as_bytes());
    w.put_u64(req.lineage.genesis.created_at_ms);
    w.put_u64(u64::try_from(req.lineage.prior_restores.len()).unwrap_or(0));
    for hop in &req.lineage.prior_restores {
        w.put_gen(hop.expected_generation);
        w.put_arr32(&hop.expected_fencing_token);
        w.put_arr16(hop.idempotency_key.as_bytes());
        w.put_u64(hop.restored_at_ms);
    }
    w.put_opt(req.existing_target_domain_id, |wr, dom| {
        wr.put_arr16(dom.as_bytes());
    });
    w.put_arr32(&req.target_domain_policy_digest);
    w.put_arr16(req.target_domain_idempotency_key.as_bytes());
    w.put_arr16(req.restore_idempotency_key.as_bytes());
    w.put_arr32(&req.restored_image_or_checkpoint_digest);
    w.put_u64(req.restored_at_ms);

    let contract = build_rpc_contract(idempotency_key, deadline_ms, true);
    let body = send_migration_rpc(source_host, target_cell, &contract, w.into_vec()).await?;
    let mut r = WireReader::new(&body);
    let restored_binding = r
        .get_process_binding()
        .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?;
    let proof = TargetRestoreProof {
        process_id: ProcessId::from_bytes(
            r.get_arr16()
                .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?,
        ),
        target_cell_id: r
            .get_arr16()
            .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?,
        restored_generation: r
            .get_u64()
            .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?,
        target_fencing_token: r
            .get_arr32()
            .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?,
        target_isolation_domain_id: IsolationDomainId::from_bytes(
            r.get_arr16()
                .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?,
        ),
        restored_image_or_checkpoint_digest: r
            .get_arr32()
            .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?,
        restored_at_ms: r
            .get_u64()
            .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?,
    };
    let registered_target_os_pid = r
        .get_u32()
        .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?;
    r.finish()
        .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?;
    Ok(TargetProcessRestoreReport {
        restored_binding,
        proof,
        registered_target_os_pid,
    })
}

async fn call_remote_record_receipt_checkpoint(
    source_host: &NetCellHost,
    target_cell: CellIdentity,
    idempotency_key: IdempotencyKey,
    receipt: &ProcessMigrationReceipt,
    deadline_ms: u64,
) -> Result<RemoteTargetCheckpointSummary, RemoteStepCallError> {
    let mut w = WireWriter::new(NET_MIGRATION_RPC_MAGIC, MG_OP_RECORD_RECEIPT_CKPT);
    w.put_cell(source_host.local_cell());
    w.put_migration_receipt(receipt);

    let contract = build_rpc_contract(idempotency_key, deadline_ms, true);
    let body = send_migration_rpc(source_host, target_cell, &contract, w.into_vec()).await?;
    let mut r = WireReader::new(&body);
    let target_cell_id = r
        .get_cell()
        .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?;
    let entry_count = usize::try_from(
        r.get_u64()
            .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?,
    )
    .unwrap_or(0);
    let mut entries = Vec::with_capacity(entry_count.min(32));
    for _ in 0..entry_count {
        entries.push(
            r.get_bytes()
                .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?,
        );
    }
    let checkpoint_prefix = CheckpointPrefix::from_entries(entries);
    let boot_gen = r
        .get_gen()
        .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?;
    let epoch = r
        .get_epoch()
        .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?;
    let token = CellFencingToken::from_u64(
        r.get_nz_u64()
            .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?
            .get(),
    )
    .ok_or_else(|| {
        RemoteStepCallError::ApplicationRefused("invalid CellFencingToken".to_owned())
    })?;
    let digest_str = r
        .get_str()
        .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?;
    let recorded_ms = r
        .get_u64()
        .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?;
    let os_process_id = r
        .get_u32()
        .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?;
    let sequence = r
        .get_u64()
        .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?;
    r.finish()
        .map_err(|s| RemoteStepCallError::ApplicationRefused(s.to_owned()))?;

    let checkpoint_fact = CheckpointFact::new(boot_gen, epoch, token, &digest_str)
        .map_err(|e| RemoteStepCallError::ApplicationRefused(e.to_string()))?;
    let checkpoint_digest =
        CheckpointDigest::from_prefix(boot_gen, epoch, token, &checkpoint_prefix);
    let summary = RemoteTargetCheckpointSummary {
        target_cell_id,
        checkpoint_prefix,
        checkpoint_digest,
        checkpoint_fact,
        recorded_ms,
        os_process_id,
        sequence,
    };
    if !summary.verify_digest() {
        return Err(RemoteStepCallError::ApplicationRefused(
            "Target CheckpointFact digest verification failed on Source".to_owned(),
        ));
    }
    Ok(summary)
}

fn build_pre_fence_abort(
    mut coordinator: ProcessMigrationCoordinator,
    failed_step: ProcessMigrationStep,
    cause: ProcessMigrationDriveError,
    aborted_at_ms: u64,
) -> CrossSocketMigrationError {
    let reason = format!("pre-fence failure at {failed_step:?}: {cause}");
    let err = match coordinator.abort_before_target_restore(reason, aborted_at_ms) {
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
    };
    CrossSocketMigrationError::Drive(Box::new(err))
}

fn build_post_fence_app_abort(
    source_host: &NetCellHost,
    leases: &LeaseMigrationBundleSpec,
    mut coordinator: ProcessMigrationCoordinator,
    failed_step: ProcessMigrationStep,
    cause: ProcessMigrationDriveError,
    aborted_at_ms: u64,
) -> CrossSocketMigrationError {
    let source_quarantine = source_host
        .with_cell_host_mut(|h| {
            let mut ep = LocalCellHostSourceEndpoint::new(h);
            ep.quarantine_unsettled_source_leases(leases)
        })
        .ok();
    let reason = format!("post-fence failure at {failed_step:?}: {cause}");
    let err = match coordinator.abort_before_target_restore(reason, aborted_at_ms) {
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
    };
    CrossSocketMigrationError::Drive(Box::new(err))
}

#[allow(clippy::too_many_arguments)]
fn build_post_fence_network_cut(
    source_host: &NetCellHost,
    request: CellHostProcessMigrationRequest,
    migration_intent_record: MigrationIntentRecord,
    pre_fence_source_epoch: CellEpoch,
    artifact_transfer_summary: Option<ArtifactTransferExecutionSummary>,
    semantic_sync_summary: Option<CrossSocketSemanticSyncSummary>,
    quiesce_proof: nlos_process::QuiesceProof,
    checkpoint_proof: ProcessCheckpointProof,
    source_fence_report: SourceFenceReport,
    mut coordinator: ProcessMigrationCoordinator,
    interrupted_step: ProcessMigrationStep,
    interrupted_idempotency_key: IdempotencyKey,
    transport_reason: String,
    rpc_deadline_ms: u64,
) -> CrossSocketMigrationError {
    let source_quarantine = source_host
        .with_cell_host_mut(|h| {
            let mut ep = LocalCellHostSourceEndpoint::new(h);
            ep.quarantine_unsettled_source_leases(&request.leases)
        })
        .unwrap_or(SourcePostFenceQuarantineReport {
            quota_state: QuotaLeaseState::Quarantined,
            capacity_state: request
                .leases
                .capacity
                .map(|_| CapacityLeaseState::Quarantined),
            device_state: request
                .leases
                .device
                .map(|_| ExclusiveDeviceLeaseState::Quarantined),
        });
    let _ =
        coordinator.abort_before_target_restore(transport_reason.clone(), request.completed_at_ms);
    let pending_tracker = PendingReconciliationTracker::new();
    pending_tracker.record_uncertain_side_effect(
        interrupted_idempotency_key,
        SocketFailureKind::ConnectionReset,
    );
    CrossSocketMigrationError::PostFenceNetworkCut(Box::new(PostFenceInterruptedMigration {
        request,
        migration_intent_record,
        pre_fence_source_epoch,
        artifact_transfer_summary,
        semantic_sync_summary,
        quiesce_proof,
        checkpoint_proof,
        source_fence_report,
        interrupted_step,
        interrupted_idempotency_key,
        source_quarantine,
        pending_tracker,
        quarantined_coordinator: coordinator,
        transport_reason,
        rpc_deadline_ms,
    }))
}

async fn anchor_migration_intent_in_consensus(
    source_host: &NetCellHost,
    migration_id: [u8; 16],
    intent_record: &MigrationIntentRecord,
    epoch: CellEpoch,
) -> Result<ConsensusCommitReceipt, ConsensusError> {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/net-migration-intent-anchor/v1");
    hasher.update(intent_record.intent().source().as_bytes());
    hasher.update(intent_record.intent().target().as_bytes());
    hasher.update(intent_record.intent().generation().get().to_be_bytes());
    hasher.update(intent_record.intent().reason().as_bytes());
    let intent_digest: [u8; 32] = hasher.finalize().into();

    let proposal = ControlPlaneProposal::new(
        derive_migration_rpc_key(migration_id, 0xA1),
        ControlPlaneCommand::ControlPlaneAuditAnchor(
            ControlPlaneAuditCommand::AnchorMigrationIntent {
                intent_id: migration_id,
                source_cell: intent_record.intent().source(),
                target_cell: intent_record.intent().target(),
                epoch,
                intent_digest,
            },
        ),
    );
    source_host
        .consensus()
        .propose(proposal, Duration::from_secs(5))
        .await
}

/// Drives all 8 ordered steps of `[DIST-MIGRATE-001/002]` (`Initiated -> Quiesced ->
/// Checkpointed -> SourceFenced -> CapabilitiesRebound -> LeasesRegranted ->
/// TargetRestored -> Completed`) from `source_host` to remote `target_cell_id`
/// over real TLS 1.3 mTLS sockets, streaming restart-image artifacts in bounded
/// `<= 64 KiB` chunks via [`ArtifactNetTransferClient`] and syncing semantic
/// provenance trails via [`SemanticNetSyncClient`], with **zero** shared
/// filesystem paths.
///
/// # Errors
///
/// - Pre-fence failures (`RefuseMigration`, digest mismatch, placement failure)
///   abort cleanly without fencing Source.
/// - Post-`SourceFenced` mTLS disconnects immediately quarantine unsettled Source
///   leases and return [`CrossSocketMigrationError::PostFenceNetworkCut`] with
///   **zero dual-live execution**; call [`reconcile_post_fence_interrupted_migration`]
///   after healing the mTLS link.
#[allow(clippy::too_many_lines)]
pub async fn drive_cross_socket_process_migration(
    source_host: &NetCellHost,
    source_runtime_adapter: Option<&TokioRuntimeAdapter>,
    request: CellHostProcessMigrationRequest,
    mut options: CrossSocketProcessMigrationOptions,
) -> Result<CrossSocketProcessMigrationOutcome, CrossSocketMigrationError> {
    if request.source_cell_id == request.target_cell_id {
        return Err(ProcessMigrationDriveError::Placement(
            "source_cell_id and target_cell_id must be distinct federated Cells",
        )
        .into());
    }
    let target_generation =
        request
            .source_generation
            .checked_next()
            .ok_or(ProcessMigrationDriveError::Placement(
                "source_generation overflowed u64",
            ))?;

    // Step 1: Placement precheck (local Source + remote mTLS Target) + Initiate.
    let source_placement = source_host.with_cell_host_mut(|h| {
        let ep = LocalCellHostSourceEndpoint::new(h);
        ep.inspect_source_placement(request.process_id, request.target_cell_id)
    })?;
    if source_placement.cell_id != request.source_cell_id {
        return Err(ProcessMigrationDriveError::Placement(
            "source NetCellHost identity does not match request.source_cell_id",
        )
        .into());
    }
    if !source_placement.both_cells_registered_in_directory {
        return Err(ProcessMigrationDriveError::Placement(
            "both Source and Target Cells must be registered in Source's local CellDirectory",
        )
        .into());
    }
    if source_placement.active_binding.process_generation != request.source_generation {
        return Err(ProcessMigrationDriveError::Placement(
            "source active process generation does not match request.source_generation",
        )
        .into());
    }
    if !source_placement.supervisor_registered {
        return Err(ProcessMigrationDriveError::Placement(
            "source process generation is not registered in Source's SupervisorPidRegistry",
        )
        .into());
    }

    let target_placement = call_remote_inspect_target_placement(
        source_host,
        request.target_cell_id,
        request.migration_id,
        options.rpc_deadline_ms,
    )
    .await?;
    if target_placement.cell_id != request.target_cell_id
        || !target_placement.both_cells_registered_in_directory
    {
        return Err(ProcessMigrationDriveError::Placement(
            "Target placement precheck failed over mTLS",
        )
        .into());
    }

    let mut coordinator = ProcessMigrationCoordinator::initiate(InitiateProcessMigrationRequest {
        migration_id: request.migration_id,
        process_id: request.process_id,
        source_cell_id: *request.source_cell_id.as_bytes(),
        target_cell_id: *request.target_cell_id.as_bytes(),
        source_generation: request.source_generation.get(),
        target_generation: target_generation.get(),
        attached_sessions: request.attached_sessions.clone(),
        pinned_session_policy: request.pinned_session_policy,
        initiated_at_ms: request.initiated_at_ms,
    })
    .map_err(|source_err| ProcessMigrationDriveError::Protocol {
        step: ProcessMigrationStep::Initiated,
        source: source_err,
    })?;

    let reason = request.custom_intent_reason.clone().unwrap_or_else(|| {
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
    .map_err(|e| ProcessMigrationDriveError::Slice(SliceKError::from(e)))?;
    let migration_intent_record = source_host.with_cell_host_mut(|h| {
        let mut ep = LocalCellHostSourceEndpoint::new(h);
        ep.record_migration_intent(&intent)
    })?;

    // Step 2: Quiesce on Source (`Quiesced`).
    let fallback_adapter;
    let adapter_ref = if let Some(ext) = source_runtime_adapter {
        ext
    } else {
        fallback_adapter = TokioRuntimeAdapter::new(
            tokio::runtime::Handle::current(),
            TokioRuntimeConfig::default(),
        )
        .map_err(|e| ProcessMigrationDriveError::Slice(SliceKError::from(e)))?;
        &fallback_adapter
    };
    let quiesce_proof = match source_host.with_cell_host_mut(|h| {
        let mut ep = LocalCellHostSourceEndpoint::with_adapter(h, adapter_ref);
        ep.quiesce_source_process(&SourceQuiesceRequest {
            process_id: request.process_id,
            source_cell_id: request.source_cell_id,
            source_generation: request.source_generation,
            cancel_scope: request.quiesce.cancel_scope,
            active_fiber_count: request.quiesce.active_fiber_count,
            inflight_operations: request.quiesce.inflight_operations.clone(),
            quiesced_at_ms: request.quiesce.quiesced_at_ms,
        })
    }) {
        Ok(p) => p,
        Err(cause) => {
            return Err(build_pre_fence_abort(
                coordinator,
                ProcessMigrationStep::Quiesced,
                cause,
                request.completed_at_ms,
            ));
        }
    };
    if let Err(err) = coordinator.record_quiesce(quiesce_proof.clone()) {
        return Err(build_pre_fence_abort(
            coordinator,
            ProcessMigrationStep::Quiesced,
            ProcessMigrationDriveError::Protocol {
                step: ProcessMigrationStep::Quiesced,
                source: err,
            },
            request.completed_at_ms,
        ));
    }

    // Step 3: Capture Checkpoint + Chunked Artifact Transfer + Semantic Delta Sync + Target Verify.
    let migration_mode = coordinator.migration_mode().clone();
    let migratable_sessions = coordinator.plan().migratable_sessions.clone();
    let mut local_checkpoint_spec = request.checkpoint.clone();
    let mut stream_restart_artifact = None;
    if let CheckpointSourceSpec::ArtifactRestartImage {
        artifact_id,
        revision,
        ref mut sync_from_source_if_missing,
        checkpointed_at_ms,
    } = local_checkpoint_spec
        && *sync_from_source_if_missing
    {
        *sync_from_source_if_missing = false;
        stream_restart_artifact = Some((artifact_id, revision, checkpointed_at_ms));
    }

    let source_bundle = match source_host.with_cell_host_mut(|h| {
        let mut ep = LocalCellHostSourceEndpoint::new(h);
        ep.capture_source_checkpoint(&SourceCheckpointRequest {
            migration_id: request.migration_id,
            process_id: request.process_id,
            source_cell_id: request.source_cell_id,
            source_generation: request.source_generation,
            migration_mode: migration_mode.clone(),
            migratable_sessions,
            spec: local_checkpoint_spec,
        })
    }) {
        Ok(b) => b,
        Err(cause) => {
            return Err(build_pre_fence_abort(
                coordinator,
                ProcessMigrationStep::Checkpointed,
                cause,
                request.completed_at_ms,
            ));
        }
    };

    let artifact_transfer_summary = if let Some((artifact_id, revision, checkpointed_at_ms)) =
        stream_restart_artifact
    {
        let package = source_host
            .with_cell_host(|h| {
                export_artifact_transfer_package(
                    &h.runtime().artifacts,
                    ExportArtifactTransferRequest {
                        artifact_id,
                        target_revision: Some(revision),
                        origin_cell_id: *request.source_cell_id.as_bytes(),
                        producer_epoch: source_placement.epoch.get(),
                        tombstone: None,
                        now_ms: checkpointed_at_ms,
                    },
                )
            })
            .map_err(|e| CrossSocketMigrationError::ArtifactTransfer(e.to_string()))?;

        let art_client = ArtifactNetTransferClient::new(
            request.source_cell_id,
            source_host.transport().as_ref(),
        )
        .with_chunk_payload_bytes(options.artifact_chunk_payload_bytes);
        let permit_id =
            CommitPermitId::from_bytes(seeded_key(request.artifact_sync_seed, 201).into_bytes());
        let (summary, _sync_report) = art_client
            .transfer_package_resumable(
                request.target_cell_id,
                &package,
                None,
                request.lineage.genesis.task_id,
                permit_id,
                package.bundle.remote_head.digest,
                checkpointed_at_ms,
                request.artifact_sync_seed,
            )
            .await
            .map_err(|e| CrossSocketMigrationError::ArtifactTransfer(e.to_string()))?;
        Some(summary)
    } else {
        None
    };

    let semantic_sync_summary = if options.sync_semantic_provenance {
        Some(
            call_remote_pull_semantic_delta(
                source_host,
                request.target_cell_id,
                request.migration_id,
                options.semantic_max_batch_events,
                options.rpc_deadline_ms,
                request.initiated_at_ms,
            )
            .await?,
        )
    } else {
        None
    };

    if let Err(cause) = call_remote_verify_checkpoint(
        source_host,
        request.target_cell_id,
        request.migration_id,
        &migration_mode,
        &request.checkpoint,
        &source_bundle,
        request.artifact_sync_seed,
        options.rpc_deadline_ms,
    )
    .await
    {
        return Err(build_pre_fence_abort(
            coordinator,
            ProcessMigrationStep::Checkpointed,
            cause,
            request.completed_at_ms,
        ));
    }
    let checkpoint_proof = source_bundle.proof;
    let primary_checkpoint_digest = checkpoint_proof.payload.primary_digest();
    if let Err(err) = coordinator.record_checkpoint(checkpoint_proof.clone()) {
        return Err(build_pre_fence_abort(
            coordinator,
            ProcessMigrationStep::Checkpointed,
            ProcessMigrationDriveError::Protocol {
                step: ProcessMigrationStep::Checkpointed,
                source: err,
            },
            request.completed_at_ms,
        ));
    }

    // Step 4: Fence Source Process (`SourceFenced`).
    let source_fence_report = match source_host.with_cell_host_mut(|h| {
        let mut ep = LocalCellHostSourceEndpoint::new(h);
        ep.fence_source_process(&SourceFenceRequest {
            process_id: request.process_id,
            source_cell_id: request.source_cell_id,
            source_generation: request.source_generation,
            invalidate_name_paths: request.source_fence.invalidate_name_paths.clone(),
            terminate_idempotency_key: request.source_fence.terminate_idempotency_key,
            stale_dispatch_probe_fiber_id: request.source_fence.stale_dispatch_probe_fiber_id,
            stale_dispatch_probe_key: request.source_fence.stale_dispatch_probe_key,
            fenced_at_ms: request.source_fence.fenced_at_ms,
        })
    }) {
        Ok(r) => r,
        Err(cause) => {
            return Err(build_pre_fence_abort(
                coordinator,
                ProcessMigrationStep::SourceFenced,
                cause,
                request.completed_at_ms,
            ));
        }
    };
    let source_fencing_token = source_fence_report.proof.source_fencing_token;
    if let Err(err) = coordinator.record_source_fence(source_fence_report.proof.clone()) {
        return Err(build_pre_fence_abort(
            coordinator,
            ProcessMigrationStep::SourceFenced,
            ProcessMigrationDriveError::Protocol {
                step: ProcessMigrationStep::SourceFenced,
                source: err,
            },
            request.completed_at_ms,
        ));
    }

    if let Some(hook) = options.after_source_fenced_hook.take() {
        hook.await;
    }

    // =========================================================================
    // POST-FENCE BARRIER: Source `(process_id, source_generation)` is now fenced!
    // =========================================================================

    execute_post_fence_steps(
        source_host,
        request,
        target_generation,
        source_placement.epoch,
        primary_checkpoint_digest,
        source_fencing_token,
        migration_intent_record,
        artifact_transfer_summary,
        semantic_sync_summary,
        quiesce_proof,
        checkpoint_proof,
        source_fence_report,
        coordinator,
        options.rpc_deadline_ms,
    )
    .await
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn execute_post_fence_steps(
    source_host: &NetCellHost,
    request: CellHostProcessMigrationRequest,
    target_generation: Generation,
    pre_fence_source_epoch: CellEpoch,
    primary_checkpoint_digest: [u8; 32],
    source_fencing_token: [u8; 32],
    migration_intent_record: MigrationIntentRecord,
    artifact_transfer_summary: Option<ArtifactTransferExecutionSummary>,
    semantic_sync_summary: Option<CrossSocketSemanticSyncSummary>,
    quiesce_proof: nlos_process::QuiesceProof,
    checkpoint_proof: ProcessCheckpointProof,
    source_fence_report: SourceFenceReport,
    mut coordinator: ProcessMigrationCoordinator,
    rpc_deadline_ms: u64,
) -> Result<CrossSocketProcessMigrationOutcome, CrossSocketMigrationError> {
    let ensure_target_epoch_above = request.leases.device.and_then(|dev| {
        dev.advance_target_epoch_if_needed
            .then_some(pre_fence_source_epoch)
    });

    // Step 5: Capability Rebind on Target (`CapabilitiesRebound`).
    let step5_key = derive_migration_rpc_key(request.migration_id, STEP_REBIND_CAPABILITIES);
    let cap_req = TargetCapabilityRebindRequest {
        process_id: request.process_id,
        source_cell_id: request.source_cell_id,
        source_generation: request.source_generation,
        target_cell_id: request.target_cell_id,
        target_generation,
        ensure_target_epoch_above,
        items: request.capabilities.clone(),
        rebound_at_ms: request.capabilities_rebound_at_ms,
    };
    let capability_rebind_report = match call_remote_rebind_capabilities(
        source_host,
        request.target_cell_id,
        step5_key,
        &cap_req,
        rpc_deadline_ms,
    )
    .await
    {
        Ok(r) => r,
        Err(RemoteStepCallError::NetworkCutOrUncertain(reason)) => {
            return Err(build_post_fence_network_cut(
                source_host,
                request,
                migration_intent_record,
                pre_fence_source_epoch,
                artifact_transfer_summary,
                semantic_sync_summary,
                quiesce_proof,
                checkpoint_proof,
                source_fence_report,
                coordinator,
                ProcessMigrationStep::CapabilitiesRebound,
                step5_key,
                reason,
                rpc_deadline_ms,
            ));
        }
        Err(RemoteStepCallError::ApplicationRefused(reason)) => {
            return Err(build_post_fence_app_abort(
                source_host,
                &request.leases,
                coordinator,
                ProcessMigrationStep::CapabilitiesRebound,
                ProcessMigrationDriveError::Transport(reason),
                request.completed_at_ms,
            ));
        }
    };
    coordinator
        .record_capability_rebind(capability_rebind_report.proof.clone())
        .map_err(|source| ProcessMigrationDriveError::Protocol {
            step: ProcessMigrationStep::CapabilitiesRebound,
            source,
        })?;

    let source_revocations: Vec<_> = request
        .capabilities
        .iter()
        .filter_map(|i| i.revoke_on_source)
        .collect();
    let source_revocation_receipts = if source_revocations.is_empty() {
        Vec::new()
    } else {
        source_host.with_cell_host_mut(|h| {
            let mut ep = LocalCellHostSourceEndpoint::new(h);
            ep.revoke_source_capabilities(&source_revocations)
        })?
    };

    // Step 6: Close/Settle Source Leases BEFORE Regranting on Target (`[LEASE-MIGRATE-001]`).
    let source_lease_close_report =
        settle_or_close_source_leases_for_step6(source_host, &request.leases)?;
    let step6_key = derive_migration_rpc_key(request.migration_id, STEP_REGRANT_LEASES);
    let regrant_req = TargetLeaseRegrantRequest {
        process_id: request.process_id,
        source_cell_id: request.source_cell_id,
        source_generation: request.source_generation,
        target_cell_id: request.target_cell_id,
        target_generation,
        spec: request.leases,
        source_closed: source_lease_close_report,
    };
    let target_lease_regrant_summary = match call_remote_regrant_leases(
        source_host,
        request.target_cell_id,
        step6_key,
        &regrant_req,
        rpc_deadline_ms,
    )
    .await
    {
        Ok(r) => r,
        Err(RemoteStepCallError::NetworkCutOrUncertain(reason)) => {
            return Err(build_post_fence_network_cut(
                source_host,
                request,
                migration_intent_record,
                pre_fence_source_epoch,
                artifact_transfer_summary,
                semantic_sync_summary,
                quiesce_proof,
                checkpoint_proof,
                source_fence_report,
                coordinator,
                ProcessMigrationStep::LeasesRegranted,
                step6_key,
                reason,
                rpc_deadline_ms,
            ));
        }
        Err(RemoteStepCallError::ApplicationRefused(reason)) => {
            return Err(build_post_fence_app_abort(
                source_host,
                &request.leases,
                coordinator,
                ProcessMigrationStep::LeasesRegranted,
                ProcessMigrationDriveError::Transport(reason),
                request.completed_at_ms,
            ));
        }
    };
    coordinator
        .record_lease_close_regrant(target_lease_regrant_summary.proof.clone())
        .map_err(|source| ProcessMigrationDriveError::Protocol {
            step: ProcessMigrationStep::LeasesRegranted,
            source,
        })?;

    // Step 7: Restore Process on Target at `target_generation == source_generation + 1`.
    let step7_key = derive_migration_rpc_key(request.migration_id, STEP_RESTORE_PROCESS);
    let restore_req = TargetProcessRestoreRequest {
        process_id: request.process_id,
        target_cell_id: request.target_cell_id,
        source_generation: request.source_generation,
        source_fencing_token,
        target_generation,
        lineage: request.lineage.clone(),
        existing_target_domain_id: request.target_restore.existing_target_domain_id,
        target_domain_policy_digest: request.target_restore.domain_policy_digest,
        target_domain_idempotency_key: request.target_restore.domain_idempotency_key,
        restore_idempotency_key: request.target_restore.restore_idempotency_key,
        restored_image_or_checkpoint_digest: primary_checkpoint_digest,
        restored_at_ms: request.target_restore.restored_at_ms,
    };
    let target_restore_report = match call_remote_restore_process(
        source_host,
        request.target_cell_id,
        step7_key,
        &restore_req,
        rpc_deadline_ms,
    )
    .await
    {
        Ok(r) => r,
        Err(RemoteStepCallError::NetworkCutOrUncertain(reason)) => {
            return Err(build_post_fence_network_cut(
                source_host,
                request,
                migration_intent_record,
                pre_fence_source_epoch,
                artifact_transfer_summary,
                semantic_sync_summary,
                quiesce_proof,
                checkpoint_proof,
                source_fence_report,
                coordinator,
                ProcessMigrationStep::TargetRestored,
                step7_key,
                reason,
                rpc_deadline_ms,
            ));
        }
        Err(RemoteStepCallError::ApplicationRefused(reason)) => {
            return Err(build_post_fence_app_abort(
                source_host,
                &request.leases,
                coordinator,
                ProcessMigrationStep::TargetRestored,
                ProcessMigrationDriveError::Transport(reason),
                request.completed_at_ms,
            ));
        }
    };
    coordinator
        .record_target_restore(target_restore_report.proof.clone())
        .map_err(|source| ProcessMigrationDriveError::Protocol {
            step: ProcessMigrationStep::TargetRestored,
            source,
        })?;

    // Step 8: Complete Receipt + Seal Checkpoint on Target + Anchor in Consensus.
    let receipt = coordinator
        .complete(request.completed_at_ms)
        .map_err(|source| ProcessMigrationDriveError::Protocol {
            step: ProcessMigrationStep::Completed,
            source,
        })?;
    let step8_key = derive_migration_rpc_key(request.migration_id, STEP_RECORD_RECEIPT_CKPT);
    let target_checkpoint_summary = match call_remote_record_receipt_checkpoint(
        source_host,
        request.target_cell_id,
        step8_key,
        &receipt,
        rpc_deadline_ms,
    )
    .await
    {
        Ok(s) => s,
        Err(RemoteStepCallError::NetworkCutOrUncertain(reason)) => {
            return Err(build_post_fence_network_cut(
                source_host,
                request,
                migration_intent_record,
                pre_fence_source_epoch,
                artifact_transfer_summary,
                semantic_sync_summary,
                quiesce_proof,
                checkpoint_proof,
                source_fence_report,
                coordinator,
                ProcessMigrationStep::Completed,
                step8_key,
                reason,
                rpc_deadline_ms,
            ));
        }
        Err(RemoteStepCallError::ApplicationRefused(reason)) => {
            return Err(ProcessMigrationDriveError::Transport(reason).into());
        }
    };

    let consensus_audit_commit = anchor_migration_intent_in_consensus(
        source_host,
        request.migration_id,
        &migration_intent_record,
        source_host.current_fence().epoch(),
    )
    .await?;

    Ok(CrossSocketProcessMigrationOutcome {
        migration_intent_record,
        artifact_transfer_summary,
        semantic_sync_summary,
        source_fence_report,
        capability_rebind_report,
        source_lease_close_report,
        target_lease_regrant_summary,
        source_revocation_receipts,
        target_restore_report,
        receipt,
        target_checkpoint_summary,
        consensus_audit_commit,
        coordinator,
    })
}

/// Settles Source's 3-family leases for Step 6 whether they are currently in
/// normal `Active`/`DeviceReserved` state OR were transitioned to `Quarantined`
/// during a post-`SourceFenced` network cut (or already `Closed`/`Returned` on
/// an idempotent reconnection replay).
#[allow(clippy::too_many_lines)]
fn settle_or_close_source_leases_for_step6(
    source_host: &NetCellHost,
    spec: &LeaseMigrationBundleSpec,
) -> Result<SourceLeaseCloseReport, ProcessMigrationDriveError> {
    source_host.with_cell_host_mut(|host| {
        let quota_grant = host.query_quota_lease(spec.quota.source_lease_id)?;
        let closed_quota = match quota_grant.state() {
            QuotaLeaseState::Closed => {
                let ack = FenceBarrierAck::for_quota(
                    &quota_grant,
                    spec.quota.gateway_id,
                    quota_grant.spent(),
                );
                SourceClosedQuotaSummary {
                    lease_id: quota_grant.lease_id(),
                    state: quota_grant.state(),
                    face_value: quota_grant.face_value(),
                    spent: quota_grant.spent(),
                    returned: quota_grant.returned(),
                    close_digest: ack.ack_digest(),
                }
            }
            QuotaLeaseState::Quarantined => {
                let ack = FenceBarrierAck::for_quota(
                    &quota_grant,
                    spec.quota.gateway_id,
                    quota_grant.spent(),
                );
                let close_digest = ack.ack_digest();
                let returned_to_avail =
                    quota_grant.face_value().saturating_sub(quota_grant.spent());
                let receipt = QuotaReconciliationReceipt::for_lease(
                    ReceiptId::from_bytes(
                        derive_migration_rpc_key(*spec.quota.source_lease_id.as_bytes(), 0xB1)
                            .into_bytes(),
                    ),
                    1_u64,
                    &quota_grant,
                    quota_grant.spent(),
                    returned_to_avail,
                    0,
                    ack,
                );
                let settled = host
                    .reconcile_quota_lease(receipt)
                    .map_err(|e| ProcessMigrationDriveError::QuotaLease(e.to_string()))?;
                SourceClosedQuotaSummary {
                    lease_id: settled.lease_id(),
                    state: settled.state(),
                    face_value: settled.face_value(),
                    spent: settled.spent(),
                    returned: settled.returned(),
                    close_digest,
                }
            }
            _ => {
                let mut ep = LocalCellHostSourceEndpoint::new(host);
                return ep.close_or_fence_source_leases(spec);
            }
        };

        let returned_capacity = if let Some(cap_spec) = spec.capacity {
            let cap_grant = host.query_capacity_lease(cap_spec.source_lease_id)?;
            let summary = match cap_grant.state() {
                CapacityLeaseState::Returned => {
                    let ack = FenceBarrierAck::for_capacity(&cap_grant, cap_spec.gateway_id);
                    SourceReturnedCapacitySummary {
                        capacity_lease_id: cap_grant.capacity_lease_id(),
                        state: cap_grant.state(),
                        amount: cap_grant.amount(),
                        close_digest: ack.ack_digest(),
                    }
                }
                CapacityLeaseState::Quarantined | CapacityLeaseState::Fenced => {
                    let ack = FenceBarrierAck::for_capacity(&cap_grant, cap_spec.gateway_id);
                    let close_digest = ack.ack_digest();
                    let receipt = CapacityReconciliationReceipt::for_lease(
                        cap_spec.reconcile_receipt_id,
                        &cap_grant,
                        CleanupAttestation::for_lease(&cap_grant, cap_grant.amount(), true),
                        ack,
                    );
                    let settled = host.reconcile_capacity_lease(receipt)?;
                    SourceReturnedCapacitySummary {
                        capacity_lease_id: settled.capacity_lease_id(),
                        state: settled.state(),
                        amount: settled.amount(),
                        close_digest,
                    }
                }
                _ => {
                    let fenced = host.quarantine_capacity_lease(cap_spec.source_lease_id)?;
                    let ack = FenceBarrierAck::for_capacity(&fenced, cap_spec.gateway_id);
                    let close_digest = ack.ack_digest();
                    let receipt = CapacityReconciliationReceipt::for_lease(
                        cap_spec.reconcile_receipt_id,
                        &fenced,
                        CleanupAttestation::for_lease(&fenced, fenced.amount(), true),
                        ack,
                    );
                    let settled = host.reconcile_capacity_lease(receipt)?;
                    SourceReturnedCapacitySummary {
                        capacity_lease_id: settled.capacity_lease_id(),
                        state: settled.state(),
                        amount: settled.amount(),
                        close_digest,
                    }
                }
            };
            Some(summary)
        } else {
            None
        };

        let returned_device = if let Some(dev_spec) = spec.device {
            let dev_grant = host.query_device_lease(dev_spec.source_lease_id)?;
            let summary = if dev_grant.state() == ExclusiveDeviceLeaseState::Returned {
                let ack = FenceBarrierAck::for_device(&dev_grant, dev_spec.gateway_id);
                SourceReturnedDeviceSummary {
                    device_lease_id: dev_grant.device_lease_id(),
                    state: dev_grant.state(),
                    exclusivity_epoch: dev_grant.exclusivity_epoch(),
                    head_reset_generation: host.device_reset_generation(),
                    close_digest: ack.ack_digest(),
                }
            } else {
                let quarantined = if matches!(
                    dev_grant.state(),
                    ExclusiveDeviceLeaseState::Quarantined | ExclusiveDeviceLeaseState::Fencing
                ) {
                    dev_grant
                } else {
                    host.quarantine_device_lease(dev_spec.source_lease_id)?
                };
                let head_reset_gen = host.device_reset_generation();
                let ack = FenceBarrierAck::for_device(&quarantined, dev_spec.gateway_id);
                let close_digest = ack.ack_digest();
                let reset_receipt = DeviceResetReceipt::new(
                    dev_spec.reset_receipt_id,
                    quarantined.device_lease_id(),
                    quarantined.holder_node(),
                    quarantined.holder_node_boot_generation(),
                    quarantined.exclusivity_epoch(),
                    quarantined.fencing_token(),
                    head_reset_gen,
                );
                let receipt = DeviceReconciliationReceipt::for_lease(
                    dev_spec.reconcile_receipt_id,
                    &quarantined,
                    true,
                    true,
                    true,
                    reset_receipt,
                    ack,
                );
                let settled = host
                    .reconcile_device_lease(receipt)
                    .map_err(|e| ProcessMigrationDriveError::DeviceReconcile(e.to_string()))?;
                SourceReturnedDeviceSummary {
                    device_lease_id: settled.device_lease_id(),
                    state: settled.state(),
                    exclusivity_epoch: settled.exclusivity_epoch(),
                    head_reset_generation: host.device_reset_generation(),
                    close_digest,
                }
            };
            Some(summary)
        } else {
            None
        };

        Ok(SourceLeaseCloseReport {
            closed_quota,
            returned_capacity,
            returned_device,
        })
    })
}

/// Reconciles and deterministically completes a migration that suffered an mTLS
/// network cut AFTER Step 4 (`SourceFenced`) (`[DIST-FAIL-001]`).
///
/// 1. Queries Target's authoritative migration state (`InspectTargetMigrationState`,
///    `side_effecting = false`) and clears `PendingReconciliationTracker`.
/// 2. Anchors `MigrationIntent` in `source_host.consensus()` (`consensus_commit_index`).
/// 3. Reconciles Source's `Quarantined` 3-family leases to `Closed` / `Returned`.
/// 4. Replays Steps 5–8 with the **exact same** deterministic `(migration_id, step)`
///    `IdempotencyKey`s so already-applied Target mutations replay idempotently
///    and remaining steps converge to `Completed` without dual-live execution.
///
/// # Errors
///
/// Fails closed if Target remains unreachable, consensus commit fails, or any
/// lease/capability/restore invariant fails.
pub async fn reconcile_post_fence_interrupted_migration(
    source_host: &NetCellHost,
    interrupted: &mut PostFenceInterruptedMigration,
) -> Result<
    (
        TargetMigrationInspectionSnapshot,
        CrossSocketProcessMigrationOutcome,
    ),
    CrossSocketMigrationError,
> {
    let inspect_key =
        derive_migration_rpc_key(interrupted.request.migration_id, STEP_INSPECT_STATE);
    let pre_reconcile_target_snapshot = inspect_remote_target_migration_state(
        source_host,
        interrupted.request.target_cell_id,
        interrupted.request.process_id,
        inspect_key,
        interrupted.rpc_deadline_ms,
    )
    .await
    .map_err(ProcessMigrationDriveError::Transport)?;

    let _ = interrupted
        .pending_tracker
        .mark_reconciled(interrupted.interrupted_idempotency_key);

    let target_generation = interrupted.request.source_generation.checked_next().ok_or(
        ProcessMigrationDriveError::Placement("source_generation overflowed u64"),
    )?;

    // Rebuild fresh coordinator through Step 4 (`SourceFenced`) using the verified
    // pre-cut proofs so Steps 5..=8 produce the canonical `ProcessMigrationReceipt`.
    let mut coordinator = ProcessMigrationCoordinator::initiate(InitiateProcessMigrationRequest {
        migration_id: interrupted.request.migration_id,
        process_id: interrupted.request.process_id,
        source_cell_id: *interrupted.request.source_cell_id.as_bytes(),
        target_cell_id: *interrupted.request.target_cell_id.as_bytes(),
        source_generation: interrupted.request.source_generation.get(),
        target_generation: target_generation.get(),
        attached_sessions: interrupted.request.attached_sessions.clone(),
        pinned_session_policy: interrupted.request.pinned_session_policy,
        initiated_at_ms: interrupted.request.initiated_at_ms,
    })
    .map_err(|source| ProcessMigrationDriveError::Protocol {
        step: ProcessMigrationStep::Initiated,
        source,
    })?;
    coordinator
        .record_quiesce(interrupted.quiesce_proof.clone())
        .map_err(|source| ProcessMigrationDriveError::Protocol {
            step: ProcessMigrationStep::Quiesced,
            source,
        })?;
    coordinator
        .record_checkpoint(interrupted.checkpoint_proof.clone())
        .map_err(|source| ProcessMigrationDriveError::Protocol {
            step: ProcessMigrationStep::Checkpointed,
            source,
        })?;
    coordinator
        .record_source_fence(interrupted.source_fence_report.proof.clone())
        .map_err(|source| ProcessMigrationDriveError::Protocol {
            step: ProcessMigrationStep::SourceFenced,
            source,
        })?;

    let primary_checkpoint_digest = interrupted.checkpoint_proof.payload.primary_digest();
    let source_fencing_token = interrupted.source_fence_report.proof.source_fencing_token;

    let outcome = execute_post_fence_steps(
        source_host,
        interrupted.request.clone(),
        target_generation,
        interrupted.pre_fence_source_epoch,
        primary_checkpoint_digest,
        source_fencing_token,
        interrupted.migration_intent_record.clone(),
        interrupted.artifact_transfer_summary,
        interrupted.semantic_sync_summary,
        interrupted.quiesce_proof.clone(),
        interrupted.checkpoint_proof.clone(),
        interrupted.source_fence_report.clone(),
        coordinator,
        interrupted.rpc_deadline_ms,
    )
    .await?;

    Ok((pre_reconcile_target_snapshot, outcome))
}

// ============================================================================
// Part 3: `NLOSTB01` `NetTakeoverRpcServer` & `N = 3` Quorum-Backed
// 6-Endpoint `DIST-TASK-002` Authority Takeover (`ADR-0026`)
// ============================================================================

const TB_OP_EXPORT_TASK_SHARD: u8 = 1;
const TB_OP_OBSERVE_BARRIER: u8 = 2;
const TB_OP_PROPOSE_CONTROL_CMD: u8 = 3;
const TB_OP_SIGN_QUORUM_COMMIT: u8 = 4;
const TB_OP_PROBE_STALE_INCUMBENT_FENCED: u8 = 5;
const TB_OP_GET_VOTER_BOOTSTRAP: u8 = 6;

const ASSIGNMENT_ID_DOMAIN: &[u8] = b"llmos/task-authority-assignment/v1";
const TAKEOVER_BARRIER_REMOTE_RECEIPT_DOMAIN: &[u8] =
    b"llmos/w71-net-takeover/remote-barrier-receipt/v1";
const TAKEOVER_BARRIER_DIGEST_DOMAIN: &[u8] = b"llmos/w71-net-takeover/barrier-digest/v1";

/// Deterministic `TaskAuthorityAssignmentId` derivation matching `nlos_task`
/// (`SHA-256("llmos/task-authority-assignment/v1" || ...)[..16]`).
#[must_use]
pub fn derive_deterministic_assignment_id(
    task_id: TaskId,
    task_generation: Generation,
    authority_id: TaskParticipantId,
    authority_term: u64,
    registry_binding: ParticipantRegistryBinding,
) -> TaskAuthorityAssignmentId {
    let mut hasher = Sha256::new();
    hasher.update(ASSIGNMENT_ID_DOMAIN);
    hasher.update(task_id.as_bytes());
    hasher.update(task_generation.get().to_be_bytes());
    hasher.update(authority_id.as_bytes());
    hasher.update(authority_term.to_be_bytes());
    hasher.update(registry_binding.generation.to_be_bytes());
    hasher.update(registry_binding.root);
    let digest: [u8; 32] = hasher.finalize().into();
    let mut id_bytes = [0u8; 16];
    id_bytes.copy_from_slice(&digest[..16]);
    TaskAuthorityAssignmentId::from_bytes(id_bytes)
}

/// Deterministic endpoint-durable `ReceiptId` minted by an incumbent host for a
/// 6-endpoint takeover barrier observation (`[DIST-TASK-002]`).
#[must_use]
pub fn derive_net_barrier_remote_receipt_id(
    takeover_receipt_id: ReceiptId,
    participant: &ParticipantRecord,
) -> ReceiptId {
    let mut hasher = Sha256::new();
    hasher.update(TAKEOVER_BARRIER_REMOTE_RECEIPT_DOMAIN);
    hasher.update(takeover_receipt_id.as_bytes());
    hasher.update([participant_type_to_u8(participant.participant_type)]);
    hasher.update(participant.participant_id.as_bytes());
    hasher.update(participant.participant_generation.get().to_be_bytes());
    hasher.update(participant.admission_receipt_id.as_bytes());
    let digest: [u8; 32] = hasher.finalize().into();
    let mut id_bytes = [0u8; 16];
    id_bytes.copy_from_slice(&digest[..16]);
    ReceiptId::from_bytes(id_bytes)
}

/// Deterministic endpoint-authored `barrier_digest` bound into a signed
/// takeover barrier observation (`[DIST-TASK-002]`).
#[must_use]
pub fn derive_net_barrier_digest(
    takeover_receipt_id: ReceiptId,
    participant: &ParticipantRecord,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(TAKEOVER_BARRIER_DIGEST_DOMAIN);
    hasher.update(takeover_receipt_id.as_bytes());
    hasher.update([participant_type_to_u8(participant.participant_type)]);
    hasher.update(participant.participant_id.as_bytes());
    hasher.update(participant.participant_generation.get().to_be_bytes());
    hasher.update(participant.admission_receipt_id.as_bytes());
    hasher.finalize().into()
}

/// Voter Ed25519 signing material registered on a [`NetTakeoverRpcServer`] to
/// issue `[DUR-ACK-002]` / `[TASK-SCALE-002]` [`VoterCommitAttestation`]s over mTLS.
#[derive(Clone, Debug)]
pub struct VoterAttestationSigner {
    /// Voter's `PrincipalId` (`KeyPurpose::SemanticSigning`).
    pub voter_principal: PrincipalId,
    /// Voter's independent failure/control domain (`[TASK-SCALE-002]`).
    pub voter_control_domain: ControlDomainId,
    /// Active `KeyPurpose::SemanticSigning` key identifier.
    pub voter_key_id: KeyId,
    /// Private 32-byte Ed25519 seed (never transmitted over the wire).
    pub signing_key_bytes: [u8; 32],
    /// Public `BootstrapPrincipalRequest` allowing remote verifiers to enroll
    /// this voter's public key in their private `IdentityAuthority`.
    pub bootstrap_request: BootstrapPrincipalRequest,
}

impl VoterAttestationSigner {
    /// Bootstraps a deterministic `KeyPurpose::SemanticSigning` voter principal
    /// inside `host`'s private `IdentityAuthority` and returns the ready signer.
    ///
    /// Distinct `seed` values produce distinct `(PrincipalId, ControlDomainId)`
    /// pairs satisfying `[TASK-SCALE-002]`.
    ///
    /// # Errors
    ///
    /// Fails closed if `IdentityAuthority::bootstrap_principal` rejects the request.
    pub fn bootstrap_on_host(host: &NetCellHost, seed: u8) -> Result<Self, SliceKError> {
        let signing_key_bytes = [seed; 32];
        let signing_key = SigningKey::from_bytes(&signing_key_bytes);
        let bootstrap_request = BootstrapPrincipalRequest {
            principal_profile_digest: [seed.wrapping_add(0x11); 32],
            control_domain_policy_digest: [seed.wrapping_add(0x22); 32],
            public_key: signing_key.verifying_key().to_bytes(),
            key_purpose: KeyPurpose::SemanticSigning,
            key_valid_from_ms: 0,
            key_valid_until_ms: 10_000_000,
            idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(0x33); 16]),
            created_at_ms: 0,
        };
        let binding = host.with_cell_host(|ch| {
            ch.runtime()
                .identity
                .bootstrap_principal(bootstrap_request)
                .map_err(SliceKError::Identity)
                .map(nlos_identity::BootstrapDecision::binding)
        })?;
        let signer = Self {
            voter_principal: binding.principal_id,
            voter_control_domain: binding.control_domain_id,
            voter_key_id: binding.key_id,
            signing_key_bytes,
            bootstrap_request,
        };
        if let Some(srv) = host.takeover_rpc_server() {
            srv.set_voter_attestation_signer(signer.clone());
        }
        Ok(signer)
    }
}

/// Private incumbent shard configuration served by [`NetTakeoverRpcServer`] over
/// mTLS (`0` shared filesystem paths).
#[derive(Clone, Debug)]
pub struct IncumbentTaskShardConfig {
    /// Path to the incumbent host's private SQLite task-authority shard file.
    pub task_db_path: PathBuf,
    /// `BarrierObservationSigning` principal identity.
    pub barrier_principal: PrincipalId,
    /// Control domain bound to `barrier_key_id`.
    pub barrier_control_domain: ControlDomainId,
    /// `BarrierObservationSigning` key identifier.
    pub barrier_key_id: KeyId,
    /// Private 32-byte Ed25519 signing seed (never leaves incumbent host).
    pub barrier_signing_key_bytes: [u8; 32],
    /// Public `BootstrapPrincipalRequest` allowing the successor host to verify
    /// barrier signatures inside its own private `IdentityAuthority`.
    pub barrier_bootstrap_request: BootstrapPrincipalRequest,
}

/// Materialized incumbent 6-endpoint task setup produced by
/// [`setup_six_endpoint_incumbent_task_on_host`].
#[derive(Clone, Debug)]
pub struct SixEndpointIncumbentSetup {
    /// Subject `TaskId`.
    pub task_id: TaskId,
    /// Subject `Task` generation (`Generation::INITIAL`).
    pub task_generation: Generation,
    /// Incumbent `term = 1` authority lease (held on `Host 1`).
    pub incumbent_lease: AuthorityLeaseRecord,
    /// Active `term = 1` `AuthorityAssignmentRecord` covering all 6 canonical
    /// §26.1 endpoint participant types.
    pub initial_assignment: AuthorityAssignmentRecord,
    /// Open `ParticipantRegistryBinding` containing all 6 canonical §26.1
    /// participant types (`TaskStore`, `ArtifactHead`, `SemanticAdmission`,
    /// `ChannelTopic`, `DriverGateway`, `ResourceLedger`).
    pub registry_binding: ParticipantRegistryBinding,
    /// All 6 admitted canonical §26.1 participant records.
    pub canonical_participants: Vec<ParticipantRecord>,
    /// Private shard config registered on `Host 1`'s [`NetTakeoverRpcServer`].
    pub shard_config: IncumbentTaskShardConfig,
}

/// Proof returned by `TB_OP_PROBE_STALE_INCUMBENT_FENCED` confirming that the
/// surviving old-term host's local `SqliteTaskAuthority` rejects stale `term = 1`
/// mutations with `TaskStoreError::AuthorityLeaseFenced`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StaleIncumbentFenceProbeReport {
    /// `true` iff `prepare_authority_takeover_fence` under `stale_lease` failed
    /// closed with `TaskStoreError::AuthorityLeaseFenced`.
    pub stale_prepare_fenced: bool,
    /// `true` iff `request_commit_permit_with_authority_lease` under `stale_lease`
    /// failed closed with `TaskStoreError::AuthorityLeaseFenced`.
    pub stale_permit_fenced: bool,
    /// Current durable lease term observed on the incumbent host (`>= 2`).
    pub observed_durable_term: u64,
}

/// Initializes a 6-endpoint (§26.1 `TaskStore`, `ArtifactHead`,
/// `SemanticAdmission`, `ChannelTopic`, `DriverGateway`, `ResourceLedger`)
/// lease-bound `Task` inside `incumbent_host`'s private `shard_dir` and
/// registers [`IncumbentTaskShardConfig`] on `incumbent_host.takeover_rpc_server()`.
///
/// The SQLite connection is cleanly closed before returning so the `.sqlite3`
/// file in `shard_dir` has zero uncheckpointed WAL frames and zero open locks.
///
/// # Errors
///
/// Fails closed if any `SqliteTaskAuthority` or `IdentityAuthority` gate fails.
#[allow(clippy::too_many_lines)]
pub fn setup_six_endpoint_incumbent_task_on_host(
    incumbent_host: &NetCellHost,
    shard_dir: &Path,
    task_id: TaskId,
    incumbent_holder_id: ProcessId,
    seed: u8,
    lease_requested_at_ms: i64,
    lease_ttl_ms: i64,
) -> Result<SixEndpointIncumbentSetup, SliceKError> {
    fs::create_dir_all(shard_dir).map_err(SliceKError::Io)?;
    let task_db_path = shard_dir.join("incumbent-tasks.sqlite3");

    let signer_seed = seed.wrapping_add(0x61);
    let signing_key_bytes = [signer_seed; 32];
    let signing_key = SigningKey::from_bytes(&signing_key_bytes);
    let barrier_bootstrap_request = BootstrapPrincipalRequest {
        principal_profile_digest: [signer_seed.wrapping_add(1); 32],
        control_domain_policy_digest: [signer_seed.wrapping_add(2); 32],
        public_key: signing_key.verifying_key().to_bytes(),
        key_purpose: KeyPurpose::BarrierObservationSigning,
        key_valid_from_ms: 0,
        key_valid_until_ms: 10_000_000,
        idempotency_key: IdempotencyKey::from_bytes([signer_seed.wrapping_add(3); 16]),
        created_at_ms: 0,
    };
    let barrier_binding = incumbent_host.with_cell_host(|ch| {
        ch.runtime()
            .identity
            .bootstrap_principal(barrier_bootstrap_request)
            .map_err(SliceKError::Identity)
            .map(nlos_identity::BootstrapDecision::binding)
    })?;

    let (incumbent_lease, initial_assignment, registry_binding, canonical_participants) = {
        let store = SqliteTaskAuthority::open(&task_db_path).map_err(SliceKError::Task)?;
        let (AuthorityLeaseDecision::Acquired(incumbent_lease)
        | AuthorityLeaseDecision::Replayed(incumbent_lease)) = store
            .acquire_authority_lease(AuthorityLeaseRequest {
                holder_id: incumbent_holder_id,
                idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(1); 16]),
                requested_at_ms: lease_requested_at_ms,
                ttl_ms: lease_ttl_ms,
            })
            .map_err(SliceKError::Task)?
        else {
            return Err(SliceKError::CellHost(
                "expected fresh term-1 lease acquire on incumbent host",
            ));
        };

        store
            .register_task(TaskSpec {
                application_id: None,
                plan_revision: None,
                task_id,
                task_generation: Generation::INITIAL,
                registered_at_ms: lease_requested_at_ms + 1,
            })
            .map_err(SliceKError::Task)?;

        let attempt = AttemptSpec {
            task_id,
            attempt_id: TaskAttemptId::from_bytes([seed.wrapping_add(2); 16]),
            attempt_generation: Generation::INITIAL,
            snapshot: SnapshotBundle {
                snapshot_id: TaskSnapshotId::from_bytes([seed.wrapping_add(3); 16]),
                snapshot_digest: [seed.wrapping_add(4); 32],
                expected_head_commit_seq: 0,
                effect_history_root: empty_effect_history_root(),
                retry_fence_epoch: 0,
            },
            cancellation_scope_id: CancellationScopeId::from_bytes([seed.wrapping_add(5); 16]),
            cancellation_generation: Generation::INITIAL,
            idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(6); 16]),
            registered_at_ms: lease_requested_at_ms + 2,
        };
        store.register_attempt(attempt).map_err(SliceKError::Task)?;

        let permit = match store
            .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
                permit: PermitRequest {
                    task_id,
                    attempt_id: attempt.attempt_id,
                    attempt_generation: attempt.attempt_generation,
                    write_set_root: [seed.wrapping_add(7); 32],
                    planned_effects: Vec::new(),
                    idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(8); 16]),
                    valid_until_ms: lease_requested_at_ms + 50_000,
                    requested_at_ms: lease_requested_at_ms + 3,
                },
                lease: incumbent_lease,
            })
            .map_err(SliceKError::Task)?
        {
            PermitDecision::Issued(permit) => *permit,
            _ => {
                return Err(SliceKError::CellHost(
                    "expected Issued commit permit for baseline registry",
                ));
            }
        };

        let ClosePermitDecision::Closed(closure_receipt) = store
            .close_permit_with_authority_lease(AuthorityLeaseCloseRequest {
                close: ClosePermitRequest {
                    task_id,
                    attempt_id: attempt.attempt_id,
                    attempt_generation: attempt.attempt_generation,
                    permit_id: permit.permit_id,
                    outcome: PermitClosureOutcome::CancelledBeforeEffect,
                    fenced_participant_digest: [0u8; 32],
                    closed_at_ms: lease_requested_at_ms + 4,
                },
                lease: incumbent_lease,
            })
            .map_err(SliceKError::Task)?
        else {
            return Err(SliceKError::CellHost(
                "expected Closed permit receipt before reopening registry",
            ));
        };

        let reopen = store
            .reopen_participant_registry_after_permit_closure(PermitClosureRegistryReopenRequest {
                task_id,
                closure_receipt_id: closure_receipt.receipt_id,
                lease: Some(incumbent_lease),
                reopened_at_ms: lease_requested_at_ms + 5,
            })
            .map_err(SliceKError::Task)?;

        let mut expected_registry_binding = reopen.reopened_registry_binding;
        let mut expected_control_epoch = reopen.control_epoch;

        // `TaskStore` is already registered in generation 1 and carried into
        // generation 2; admit the remaining 5 canonical §26.1 endpoint types.
        let extra_types = [
            ParticipantType::ArtifactHead,
            ParticipantType::SemanticAdmission,
            ParticipantType::ChannelTopic,
            ParticipantType::DriverGateway,
            ParticipantType::ResourceLedger,
        ];
        let mut admitted_extra = Vec::with_capacity(extra_types.len());
        for (idx, ptype) in extra_types.into_iter().enumerate() {
            let offset = u8::try_from(idx + 1).unwrap_or(1);
            let participant = ParticipantRecord {
                participant_type: ptype,
                participant_id: TaskParticipantId::from_bytes(
                    [seed.wrapping_add(0x20).wrapping_add(offset); 16],
                ),
                participant_generation: Generation::INITIAL,
                admission_receipt_id: ReceiptId::from_bytes(
                    [seed.wrapping_add(0x40).wrapping_add(offset); 16],
                ),
            };
            let step_ms = lease_requested_at_ms + 6 + i64::try_from(idx).unwrap_or(0);
            let adm = store
                .admit_verified_participant_operation(DistTaskOperationAdmissionRequest {
                    task_id,
                    expected_registry_binding,
                    lease: incumbent_lease,
                    expected_authority_term: incumbent_lease.term,
                    expected_control_epoch,
                    participant,
                    admitted_at_ms: step_ms,
                })
                .map_err(SliceKError::Task)?;
            expected_registry_binding = adm.participant_registry_binding;
            expected_control_epoch = adm.control_epoch;
            admitted_extra.push(adm.participant);
        }

        let initial_assignment = store
            .inspect_authority_assignment(task_id)
            .map_err(SliceKError::Task)?;
        let task_store_participant = ParticipantRecord {
            participant_type: ParticipantType::TaskStore,
            participant_id: incumbent_lease.authority_id,
            participant_generation: Generation::INITIAL,
            admission_receipt_id: ReceiptId::from_bytes(permit.permit_id.into_bytes()),
        };
        let mut all_participants = Vec::with_capacity(6);
        all_participants.push(task_store_participant);
        all_participants.extend(admitted_extra);

        (
            incumbent_lease,
            initial_assignment,
            expected_registry_binding,
            all_participants,
        )
    };

    let shard_config = IncumbentTaskShardConfig {
        task_db_path,
        barrier_principal: barrier_binding.principal_id,
        barrier_control_domain: barrier_binding.control_domain_id,
        barrier_key_id: barrier_binding.key_id,
        barrier_signing_key_bytes: signing_key_bytes,
        barrier_bootstrap_request,
    };
    if let Some(srv) = incumbent_host.takeover_rpc_server() {
        srv.register_incumbent_task_shard(task_id, shard_config.clone());
    }

    Ok(SixEndpointIncumbentSetup {
        task_id,
        task_generation: Generation::INITIAL,
        incumbent_lease,
        initial_assignment,
        registry_binding,
        canonical_participants,
        shard_config,
    })
}

/// Networked `NLOSTB01` RPC server handling cross-socket task-shard export,
/// signed 6-endpoint takeover barrier observations, Raft leader-forwarded
/// proposals, `[DUR-ACK-002]` voter commit attestations, and stale-incumbent
/// fencing verification.
#[derive(Clone)]
pub struct NetTakeoverRpcServer {
    host: Arc<Mutex<CellHost>>,
    quorum_node: Option<Arc<MultiHostQuorumNode>>,
    shards: Arc<Mutex<BTreeMap<TaskId, IncumbentTaskShardConfig>>>,
    voter_signer: Arc<Mutex<Option<VoterAttestationSigner>>>,
}

impl fmt::Debug for NetTakeoverRpcServer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NetTakeoverRpcServer")
            .field("has_quorum_node", &self.quorum_node.is_some())
            .finish_non_exhaustive()
    }
}

impl NetTakeoverRpcServer {
    /// Creates a new `NLOSTB01` server bound to `host` and optional `quorum_node`.
    #[must_use]
    pub fn new(host: Arc<Mutex<CellHost>>, quorum_node: Option<Arc<MultiHostQuorumNode>>) -> Self {
        Self {
            host,
            quorum_node,
            shards: Arc::new(Mutex::new(BTreeMap::new())),
            voter_signer: Arc::new(Mutex::new(None)),
        }
    }

    /// Registers a private incumbent task shard for cross-socket export and
    /// signed barrier observation.
    pub fn register_incumbent_task_shard(&self, task_id: TaskId, config: IncumbentTaskShardConfig) {
        self.shards
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(task_id, config);
    }

    /// Registers this host's `KeyPurpose::SemanticSigning` voter attestation
    /// signer for `CommittedControlIndexGate` quorum proofs.
    pub fn set_voter_attestation_signer(&self, signer: VoterAttestationSigner) {
        *self
            .voter_signer
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(signer);
    }

    fn observed_epoch(&self) -> CellEpoch {
        self.host
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .fence()
            .epoch()
    }

    fn handle_export_task_shard(
        &self,
        mut r: WireReader<'_>,
        auth_peer: CellIdentity,
    ) -> Result<Vec<u8>, String> {
        const MAX_SHARD_EXPORT_PAGE_BYTES: usize = 48 * 1024;

        let claimed_caller = r.get_cell().map_err(str::to_owned)?;
        let task_id = TaskId::from_bytes(r.get_arr16().map_err(str::to_owned)?);
        let offset = r.get_u64().map_err(str::to_owned)?;
        let max_chunk_bytes = r.get_u32().map_err(str::to_owned)?;
        r.finish().map_err(str::to_owned)?;
        if claimed_caller != auth_peer {
            return Err(
                "5-axis mTLS caller identity mismatch on TB_OP_EXPORT_TASK_SHARD".to_owned(),
            );
        }
        let chunk_limit = usize::try_from(max_chunk_bytes)
            .unwrap_or(MAX_SHARD_EXPORT_PAGE_BYTES)
            .clamp(1024, MAX_SHARD_EXPORT_PAGE_BYTES);

        // Hold the shard map guard while opening & cleanly closing SQLite so
        // WAL frames are checkpointed into `.sqlite3` before `fs::read`.
        let guard = self.shards.lock().unwrap_or_else(PoisonError::into_inner);
        let cfg = guard
            .get(&task_id)
            .ok_or_else(|| format!("unknown incumbent task shard {task_id:?}"))?;
        if offset == 0 {
            let store = SqliteTaskAuthority::open(&cfg.task_db_path)
                .map_err(|err| format!("open incumbent shard for WAL checkpoint: {err}"))?;
            let _ = store
                .inspect_task(task_id)
                .map_err(|err| format!("verify incumbent task before export: {err}"))?;
        }
        let sqlite_bytes = fs::read(&cfg.task_db_path)
            .map_err(|err| format!("read checkpointed incumbent shard bytes: {err}"))?;
        let bootstrap_req = cfg.barrier_bootstrap_request;
        drop(guard);

        let total_len = u64::try_from(sqlite_bytes.len()).unwrap_or(u64::MAX);
        let start = usize::try_from(offset)
            .unwrap_or(sqlite_bytes.len())
            .min(sqlite_bytes.len());
        let end = start.saturating_add(chunk_limit).min(sqlite_bytes.len());
        let chunk = &sqlite_bytes[start..end];
        let has_more = end < sqlite_bytes.len();

        let mut body_writer = WireWriter {
            buf: Vec::with_capacity(chunk.len() + 256),
        };
        body_writer.put_u64(total_len);
        body_writer.put_bytes(chunk);
        body_writer.put_bool(has_more);
        if offset == 0 {
            body_writer.put_bool(true);
            body_writer.put_bootstrap_principal(&bootstrap_req);
        } else {
            body_writer.put_bool(false);
        }
        Ok(body_writer.into_vec())
    }

    fn handle_observe_barrier(
        &self,
        mut r: WireReader<'_>,
        auth_peer: CellIdentity,
    ) -> Result<Vec<u8>, String> {
        let claimed_caller = r.get_cell().map_err(str::to_owned)?;
        let task_id = TaskId::from_bytes(r.get_arr16().map_err(str::to_owned)?);
        let takeover_receipt_id = ReceiptId::from_bytes(r.get_arr16().map_err(str::to_owned)?);
        let participant = r.get_participant().map_err(str::to_owned)?;
        let fence_set_root = r.get_arr32().map_err(str::to_owned)?;
        let successor_holder_id = ProcessId::from_bytes(r.get_arr16().map_err(str::to_owned)?);
        let successor_lease_idem =
            IdempotencyKey::from_bytes(r.get_arr16().map_err(str::to_owned)?);
        let successor_requested_at_ms = r.get_i64().map_err(str::to_owned)?;
        let successor_ttl_ms = r.get_i64().map_err(str::to_owned)?;
        let expected_registry_binding = ParticipantRegistryBinding {
            generation: r.get_u64().map_err(str::to_owned)?,
            root: r.get_arr32().map_err(str::to_owned)?,
        };
        r.finish().map_err(str::to_owned)?;
        if claimed_caller != auth_peer {
            return Err("5-axis mTLS caller identity mismatch on TB_OP_OBSERVE_BARRIER".to_owned());
        }

        // Under the shard mutex, durably advance the incumbent host's OWN private
        // `SqliteTaskAuthority` to the successor fence lease & `FrozenForTakeover`
        // state, verifying that the local 6-endpoint manifest produces the exact
        // same `fence_set_root` BEFORE signing the barrier observation!
        let guard = self.shards.lock().unwrap_or_else(PoisonError::into_inner);
        let cfg = guard
            .get(&task_id)
            .cloned()
            .ok_or_else(|| format!("unknown incumbent task shard {task_id:?}"))?;
        {
            let store = SqliteTaskAuthority::open(&cfg.task_db_path)
                .map_err(|err| format!("open incumbent shard for barrier fence: {err}"))?;
            let wall_ms = u64::try_from(successor_requested_at_ms)
                .map_err(|_| "negative successor_requested_at_ms".to_owned())?;
            let fence_lease = store
                .acquire_authority_lease_anchored(
                    AuthorityLeaseRequest {
                        holder_id: successor_holder_id,
                        idempotency_key: successor_lease_idem,
                        requested_at_ms: successor_requested_at_ms,
                        ttl_ms: successor_ttl_ms,
                    },
                    wall_ms,
                )
                .map_err(|err| format!("incumbent local lease fence transition failed: {err}"))?
                .record();

            let current_reg = store
                .inspect_participant_registry(task_id)
                .map_err(|err| format!("inspect incumbent registry: {err}"))?;
            if current_reg.state != ParticipantRegistryState::FrozenForTakeover {
                store
                    .prepare_authority_takeover_fence(AuthorityLeaseTakeoverFenceRequest {
                        task_id,
                        expected_registry_binding,
                        lease: fence_lease,
                        requested_at_ms: successor_requested_at_ms + 1,
                    })
                    .map_err(|err| {
                        format!("incumbent local prepare_authority_takeover_fence: {err}")
                    })?;
            }
            let local_fence = store
                .inspect_authority_takeover_fence_receipt(task_id, expected_registry_binding)
                .map_err(|err| format!("inspect incumbent fence receipt: {err}"))?;
            if local_fence.exact_fence_set_root != Some(fence_set_root) {
                return Err(
                    "incumbent exact_fence_set_root mismatch against demand fence_set_root"
                        .to_owned(),
                );
            }
            let members = store
                .inspect_authority_takeover_fence_members(task_id, expected_registry_binding)
                .map_err(|err| format!("inspect incumbent fence members: {err}"))?;
            if !members.iter().any(|m| m.participant == participant) {
                return Err(format!(
                    "participant {:?} is not in incumbent frozen fence manifest",
                    participant.participant_type
                ));
            }
        }
        drop(guard);

        let remote_receipt_id =
            derive_net_barrier_remote_receipt_id(takeover_receipt_id, &participant);
        let barrier_digest = derive_net_barrier_digest(takeover_receipt_id, &participant);
        let msg = barrier_observation_signature_message(
            takeover_receipt_id,
            &participant,
            remote_receipt_id,
            barrier_digest,
            fence_set_root,
        );
        let signing_key = SigningKey::from_bytes(&cfg.barrier_signing_key_bytes);
        let sig_bytes: Ed25519Signature = signing_key.sign(&msg).to_bytes();

        let mut body = WireWriter {
            buf: Vec::with_capacity(192),
        };
        body.put_arr16(remote_receipt_id.as_bytes());
        body.put_arr32(&barrier_digest);
        body.put_arr16(cfg.barrier_principal.as_bytes());
        body.put_arr16(cfg.barrier_control_domain.as_bytes());
        body.put_arr16(cfg.barrier_key_id.as_bytes());
        body.put_arr64(&sig_bytes);
        Ok(body.into_vec())
    }

    async fn handle_propose_control_cmd(&self, proposal_wire: Vec<u8>) -> Result<Vec<u8>, String> {
        let Some(ref node) = self.quorum_node else {
            return Err("MultiHostQuorumNode is not mounted on this host".to_owned());
        };
        let proposal = ControlPlaneProposal::decode_wire(&proposal_wire)
            .map_err(|err| format!("invalid ControlPlaneProposal wire: {err}"))?;

        let mut body = WireWriter {
            buf: Vec::with_capacity(128),
        };
        match node.propose(proposal, Duration::from_secs(5)).await {
            Ok(receipt) => {
                body.put_u8(1); // Committed
                body.put_u64(receipt.consensus_commit_index.get());
                body.put_u64(receipt.term.get());
                body.put_arr32(&receipt.state_root_digest);
                match receipt.outcome {
                    AppliedProposalOutcome::Applied {
                        consensus_commit_index,
                        idempotent_replay,
                    } => {
                        body.put_u8(1);
                        body.put_u64(consensus_commit_index.get());
                        body.put_bool(idempotent_replay);
                    }
                    AppliedProposalOutcome::DomainRejected {
                        consensus_commit_index,
                        reason,
                    } => {
                        body.put_u8(2);
                        body.put_u64(consensus_commit_index.get());
                        body.put_u8(reason as u8);
                    }
                }
                Ok(body.into_vec())
            }
            Err(ConsensusError::ProposalRejected(
                ProposalRejectReason::NotLeader { known_leader, .. }
                | ProposalRejectReason::StaleLeaderTerm { known_leader, .. },
            )) => {
                body.put_u8(2); // Redirect / NotLeader
                body.put_opt(known_leader, WireWriter::put_cell);
                Ok(body.into_vec())
            }
            Err(other) => Err(format!("quorum propose failed: {other}")),
        }
    }

    async fn handle_sign_quorum_commit(
        &self,
        target_commit_index: NonZeroU64,
        target_term: NonZeroU64,
        expected_state_root: [u8; 32],
        subject: ControlCommitSubject,
    ) -> Result<Vec<u8>, String> {
        let Some(ref node) = self.quorum_node else {
            return Err("MultiHostQuorumNode is not mounted on this host".to_owned());
        };
        let signer = self
            .voter_signer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or_else(|| "VoterAttestationSigner is not configured on this host".to_owned())?;

        let deadline = Instant::now() + Duration::from_secs(4);
        loop {
            let local_idx = node.consensus_commit_index();
            if local_idx >= target_commit_index.get() {
                if local_idx == target_commit_index.get() {
                    let local_root = node.state_root_digest();
                    if local_root != expected_state_root {
                        return Err(
                            "voter state_root_digest mismatch at target_commit_index".to_owned()
                        );
                    }
                }
                break;
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "voter timed out waiting for consensus_commit_index {} (local={local_idx})",
                    target_commit_index.get()
                ));
            }
            tokio::time::sleep(Duration::from_millis(15)).await;
        }

        let digest = quorum_commit_attestation_digest(
            target_commit_index,
            target_term,
            &expected_state_root,
            &subject,
        );
        let signing_key = SigningKey::from_bytes(&signer.signing_key_bytes);
        let sig_bytes: Ed25519Signature = signing_key.sign(&digest).to_bytes();

        let mut body = WireWriter {
            buf: Vec::with_capacity(128),
        };
        body.put_arr16(signer.voter_principal.as_bytes());
        body.put_arr16(signer.voter_control_domain.as_bytes());
        body.put_arr16(signer.voter_key_id.as_bytes());
        body.put_arr64(&sig_bytes);
        Ok(body.into_vec())
    }

    fn handle_probe_stale_incumbent_fenced(
        &self,
        mut r: WireReader<'_>,
        auth_peer: CellIdentity,
    ) -> Result<Vec<u8>, String> {
        let claimed_caller = r.get_cell().map_err(str::to_owned)?;
        let task_id = TaskId::from_bytes(r.get_arr16().map_err(str::to_owned)?);
        let stale_lease = r.get_authority_lease().map_err(str::to_owned)?;
        let expected_registry_binding = ParticipantRegistryBinding {
            generation: r.get_u64().map_err(str::to_owned)?,
            root: r.get_arr32().map_err(str::to_owned)?,
        };
        let probe_at_ms = r.get_i64().map_err(str::to_owned)?;
        r.finish().map_err(str::to_owned)?;
        if claimed_caller != auth_peer {
            return Err(
                "5-axis mTLS caller identity mismatch on TB_OP_PROBE_STALE_INCUMBENT_FENCED"
                    .to_owned(),
            );
        }

        let guard = self.shards.lock().unwrap_or_else(PoisonError::into_inner);
        let cfg = self.lookup_shard_in(&guard, task_id)?;
        let (stale_prepare_fenced, stale_permit_fenced, observed_durable_term) = {
            let store = SqliteTaskAuthority::open(&cfg.task_db_path)
                .map_err(|err| format!("open incumbent shard for stale probe: {err}"))?;
            let durable_lease = store
                .inspect_authority_lease()
                .map_err(|err| format!("inspect incumbent lease: {err}"))?;

            let stale_prepare_fenced = matches!(
                store.prepare_authority_takeover_fence(AuthorityLeaseTakeoverFenceRequest {
                    task_id,
                    expected_registry_binding,
                    lease: stale_lease,
                    requested_at_ms: probe_at_ms,
                }),
                Err(TaskStoreError::AuthorityLeaseFenced)
            );

            let head = store
                .inspect_task(task_id)
                .map_err(|err| format!("inspect task head for stale probe: {err}"))?;
            let probe_attempt = AttemptSpec {
                task_id,
                attempt_id: TaskAttemptId::from_bytes([0xE7; 16]),
                attempt_generation: Generation::INITIAL,
                snapshot: SnapshotBundle {
                    snapshot_id: TaskSnapshotId::from_bytes([0xE8; 16]),
                    snapshot_digest: [0xE9; 32],
                    expected_head_commit_seq: head.head_commit_seq,
                    effect_history_root: head.head_effect_history_root,
                    retry_fence_epoch: head.retry_fence_epoch,
                },
                cancellation_scope_id: CancellationScopeId::from_bytes([0xEA; 16]),
                cancellation_generation: Generation::INITIAL,
                idempotency_key: IdempotencyKey::from_bytes([0xEB; 16]),
                registered_at_ms: probe_at_ms + 1,
            };
            let _ = store.register_attempt(probe_attempt);
            let stale_permit_fenced = matches!(
                store.request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
                    permit: PermitRequest {
                        task_id,
                        attempt_id: probe_attempt.attempt_id,
                        attempt_generation: probe_attempt.attempt_generation,
                        write_set_root: [0xEC; 32],
                        planned_effects: Vec::new(),
                        idempotency_key: IdempotencyKey::from_bytes([0xED; 16]),
                        valid_until_ms: probe_at_ms + 50_000,
                        requested_at_ms: probe_at_ms + 2,
                    },
                    lease: stale_lease,
                }),
                Err(TaskStoreError::AuthorityLeaseFenced)
            );

            (
                stale_prepare_fenced,
                stale_permit_fenced,
                durable_lease.term,
            )
        };
        drop(guard);

        let mut body = WireWriter {
            buf: Vec::with_capacity(16),
        };
        body.put_bool(stale_prepare_fenced);
        body.put_bool(stale_permit_fenced);
        body.put_u64(observed_durable_term);
        Ok(body.into_vec())
    }

    fn handle_get_voter_bootstrap(
        &self,
        mut r: WireReader<'_>,
        auth_peer: CellIdentity,
    ) -> Result<Vec<u8>, String> {
        let claimed_caller = r.get_cell().map_err(str::to_owned)?;
        r.finish().map_err(str::to_owned)?;
        if claimed_caller != auth_peer {
            return Err(
                "5-axis mTLS caller identity mismatch on TB_OP_GET_VOTER_BOOTSTRAP".to_owned(),
            );
        }
        let signer = self
            .voter_signer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .ok_or_else(|| "VoterAttestationSigner is not configured on this host".to_owned())?;
        let mut body = WireWriter {
            buf: Vec::with_capacity(160),
        };
        body.put_bootstrap_principal(&signer.bootstrap_request);
        Ok(body.into_vec())
    }

    fn lookup_shard_in<'a>(
        &self,
        guard: &'a BTreeMap<TaskId, IncumbentTaskShardConfig>,
        task_id: TaskId,
    ) -> Result<&'a IncumbentTaskShardConfig, String> {
        let _ = self;
        guard
            .get(&task_id)
            .ok_or_else(|| format!("unknown incumbent task shard {task_id:?}"))
    }
}

impl SubProtocolRpcHandler for NetTakeoverRpcServer {
    fn matches_frame(&self, payload: &[u8]) -> bool {
        payload.len() >= 9 && &payload[..8] == NET_TAKEOVER_RPC_MAGIC
    }

    fn handle_frame(
        &self,
        request: IncomingRpcRequest,
    ) -> Pin<Box<dyn Future<Output = RpcHandlerReply> + Send + 'static>> {
        let server = self.clone();
        Box::pin(async move {
            let observed_epoch = server.observed_epoch();
            if request.payload.len() < 9 || &request.payload[..8] != NET_TAKEOVER_RPC_MAGIC {
                return encode_err_reply(
                    NET_TAKEOVER_RPC_MAGIC,
                    observed_epoch,
                    "invalid NLOSTB01 header",
                );
            }
            let auth_peer = request.peer_session.peer_claims.cell_identity;
            let op = request.payload[8];
            let body_slice = &request.payload[9..];

            let result = match op {
                TB_OP_EXPORT_TASK_SHARD => {
                    server.handle_export_task_shard(WireReader::new(body_slice), auth_peer)
                }
                TB_OP_OBSERVE_BARRIER => {
                    server.handle_observe_barrier(WireReader::new(body_slice), auth_peer)
                }
                TB_OP_PROPOSE_CONTROL_CMD => {
                    let parsed = (|| -> Result<Vec<u8>, String> {
                        let mut r = WireReader::new(body_slice);
                        let claimed_caller = r.get_cell().map_err(str::to_owned)?;
                        let wire = r.get_bytes().map_err(str::to_owned)?;
                        r.finish().map_err(str::to_owned)?;
                        if claimed_caller != auth_peer {
                            return Err(
                                "5-axis mTLS caller identity mismatch on TB_OP_PROPOSE_CONTROL_CMD"
                                    .to_owned(),
                            );
                        }
                        Ok(wire)
                    })();
                    match parsed {
                        Ok(wire) => server.handle_propose_control_cmd(wire).await,
                        Err(e) => Err(e),
                    }
                }
                TB_OP_SIGN_QUORUM_COMMIT => {
                    let parsed = (|| -> Result<(NonZeroU64, NonZeroU64, [u8; 32], ControlCommitSubject), String> {
                        let mut r = WireReader::new(body_slice);
                        let claimed_caller = r.get_cell().map_err(str::to_owned)?;
                        let idx = r.get_nz_u64().map_err(str::to_owned)?;
                        let term = r.get_nz_u64().map_err(str::to_owned)?;
                        let root = r.get_arr32().map_err(str::to_owned)?;
                        let subj = r.get_control_commit_subject().map_err(str::to_owned)?;
                        r.finish().map_err(str::to_owned)?;
                        if claimed_caller != auth_peer {
                            return Err(
                                "5-axis mTLS caller identity mismatch on TB_OP_SIGN_QUORUM_COMMIT"
                                    .to_owned(),
                            );
                        }
                        Ok((idx, term, root, subj))
                    })();
                    match parsed {
                        Ok((idx, term, root, subj)) => {
                            server
                                .handle_sign_quorum_commit(idx, term, root, subj)
                                .await
                        }
                        Err(e) => Err(e),
                    }
                }
                TB_OP_PROBE_STALE_INCUMBENT_FENCED => server
                    .handle_probe_stale_incumbent_fenced(WireReader::new(body_slice), auth_peer),
                TB_OP_GET_VOTER_BOOTSTRAP => {
                    server.handle_get_voter_bootstrap(WireReader::new(body_slice), auth_peer)
                }
                other => Err(format!("unknown NLOSTB01 op {other}")),
            };

            match result {
                Ok(bytes) => encode_tb_ok_reply(observed_epoch, false, &bytes),
                Err(detail) => encode_err_reply(NET_TAKEOVER_RPC_MAGIC, observed_epoch, &detail),
            }
        })
    }
}

async fn call_tb_rpc(
    caller_host: &NetCellHost,
    target_cell: CellIdentity,
    op_tag: u8,
    req_seq: u64,
    payload: Vec<u8>,
) -> Result<Vec<u8>, String> {
    let transport = caller_host.transport();
    let caller_cell = caller_host.local_cell();
    let mut corr = [0u8; 16];
    corr[0] = b'T';
    corr[1] = op_tag;
    corr[2..8].copy_from_slice(&caller_cell.as_bytes()[..6]);
    corr[8..16].copy_from_slice(&req_seq.to_be_bytes());
    let contract =
        RemoteCallContract::new(corr, IdempotencyKey::from_bytes(corr), 5_000, 0, 0, false);
    match transport
        .call_cell_rpc(target_cell, &contract, payload)
        .await
    {
        Ok(RemoteCallOutcome::Succeeded { value, .. }) => {
            let body = decode_app_reply(NET_TAKEOVER_RPC_MAGIC, &value.payload)?;
            Ok(body.to_vec())
        }
        Ok(other) => Err(format!("NLOSTB01 transport non-success outcome: {other:?}")),
        Err(err) => Err(format!("NLOSTB01 transport error: {err}")),
    }
}

/// Cross-socket TLS 1.3 mTLS implementation of [`BarrierObservationSource`]
/// collecting signed 6-endpoint takeover barrier observations from the
/// incumbent [`NetCellHost`] with zero shared filesystem access.
pub struct MtlsBarrierObservationClient<'a> {
    caller_host: &'a NetCellHost,
    incumbent_cell: CellIdentity,
    task_id: TaskId,
    successor_holder_id: ProcessId,
    successor_lease_idempotency_key: IdempotencyKey,
    successor_requested_at_ms: i64,
    successor_ttl_ms: i64,
    expected_registry_binding: ParticipantRegistryBinding,
    next_seq: u64,
}

impl<'a> MtlsBarrierObservationClient<'a> {
    /// Constructs a new mTLS barrier observation client targeting `incumbent_cell`.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        caller_host: &'a NetCellHost,
        incumbent_cell: CellIdentity,
        task_id: TaskId,
        successor_holder_id: ProcessId,
        successor_lease_idempotency_key: IdempotencyKey,
        successor_requested_at_ms: i64,
        successor_ttl_ms: i64,
        expected_registry_binding: ParticipantRegistryBinding,
    ) -> Self {
        Self {
            caller_host,
            incumbent_cell,
            task_id,
            successor_holder_id,
            successor_lease_idempotency_key,
            successor_requested_at_ms,
            successor_ttl_ms,
            expected_registry_binding,
            next_seq: 1,
        }
    }

    /// Asynchronously requests a signed endpoint barrier observation over mTLS.
    ///
    /// # Errors
    ///
    /// Returns [`TakeoverDriveError::BarrierSource`] if the mTLS call or wire
    /// decode fails.
    pub async fn observe_barrier_async(
        &mut self,
        demand: BarrierObservationDemand,
    ) -> Result<SignedBarrierObservation, TakeoverDriveError> {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);

        let mut w = WireWriter::new(NET_TAKEOVER_RPC_MAGIC, TB_OP_OBSERVE_BARRIER);
        w.put_cell(self.caller_host.local_cell());
        w.put_arr16(self.task_id.as_bytes());
        w.put_arr16(demand.takeover_receipt_id.as_bytes());
        w.put_participant(demand.participant);
        w.put_arr32(&demand.fence_set_root);
        w.put_arr16(self.successor_holder_id.as_bytes());
        w.put_arr16(self.successor_lease_idempotency_key.as_bytes());
        w.put_i64(self.successor_requested_at_ms);
        w.put_i64(self.successor_ttl_ms);
        w.put_u64(self.expected_registry_binding.generation);
        w.put_arr32(&self.expected_registry_binding.root);

        let raw = call_tb_rpc(
            self.caller_host,
            self.incumbent_cell,
            TB_OP_OBSERVE_BARRIER,
            seq,
            w.into_vec(),
        )
        .await
        .map_err(TakeoverDriveError::BarrierSource)?;

        let mut r = WireReader::new(&raw);
        let remote_receipt_id = ReceiptId::from_bytes(
            r.get_arr16()
                .map_err(|e| TakeoverDriveError::BarrierSource(e.to_owned()))?,
        );
        let barrier_digest = r
            .get_arr32()
            .map_err(|e| TakeoverDriveError::BarrierSource(e.to_owned()))?;
        let issuer = PrincipalId::from_bytes(
            r.get_arr16()
                .map_err(|e| TakeoverDriveError::BarrierSource(e.to_owned()))?,
        );
        let control_domain_id = ControlDomainId::from_bytes(
            r.get_arr16()
                .map_err(|e| TakeoverDriveError::BarrierSource(e.to_owned()))?,
        );
        let key_id = KeyId::from_bytes(
            r.get_arr16()
                .map_err(|e| TakeoverDriveError::BarrierSource(e.to_owned()))?,
        );
        let signature = r
            .get_arr64()
            .map_err(|e| TakeoverDriveError::BarrierSource(e.to_owned()))?;
        r.finish()
            .map_err(|e| TakeoverDriveError::BarrierSource(e.to_owned()))?;

        Ok(SignedBarrierObservation {
            remote_receipt_id,
            barrier_digest,
            signature: BarrierObservationSignature {
                issuer,
                control_domain_id,
                key_id,
                signature,
            },
        })
    }
}

impl BarrierObservationSource for MtlsBarrierObservationClient<'_> {
    fn observe_barrier(
        &mut self,
        demand: BarrierObservationDemand,
    ) -> Result<SignedBarrierObservation, TakeoverDriveError> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(self.observe_barrier_async(demand))
        })
    }
}

/// Fail-closed errors surfaced by [`drive_quorum_backed_six_endpoint_takeover`].
#[derive(Debug)]
pub enum QuorumTakeoverDriveError {
    /// Underlying `TakeoverDriveError`.
    Takeover(TakeoverDriveError),
    /// `CommittedControlIndexGate` rejected a quorum attestation or commit index.
    ConsensusGate(ConsensusIndexGateError),
    /// Underlying `SliceKError`.
    Host(SliceKError),
    /// Raft quorum proposal or mTLS RPC failed.
    QuorumRpc(String),
}

impl fmt::Display for QuorumTakeoverDriveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Takeover(err) => write!(formatter, "takeover drive failed: {err}"),
            Self::ConsensusGate(err) => {
                write!(formatter, "CommittedControlIndexGate rejected proof: {err}")
            }
            Self::Host(err) => write!(formatter, "host error: {err}"),
            Self::QuorumRpc(msg) => write!(formatter, "quorum/mTLS RPC failed: {msg}"),
        }
    }
}

impl Error for QuorumTakeoverDriveError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Takeover(err) => Some(err),
            Self::ConsensusGate(err) => Some(err),
            Self::Host(err) => Some(err),
            Self::QuorumRpc(_) => None,
        }
    }
}

impl From<TakeoverDriveError> for QuorumTakeoverDriveError {
    fn from(value: TakeoverDriveError) -> Self {
        Self::Takeover(value)
    }
}

impl From<ConsensusIndexGateError> for QuorumTakeoverDriveError {
    fn from(value: ConsensusIndexGateError) -> Self {
        Self::ConsensusGate(value)
    }
}

impl From<SliceKError> for QuorumTakeoverDriveError {
    fn from(value: SliceKError) -> Self {
        Self::Host(value)
    }
}

/// Inputs for [`drive_quorum_backed_six_endpoint_takeover`] across an `N >= 3`
/// `MultiHostQuorumNode` cluster over TLS 1.3 mTLS (`0` shared filesystem paths).
#[derive(Clone, Debug)]
pub struct QuorumSixEndpointTakeoverRequest {
    /// Subject `TaskId`.
    pub task_id: TaskId,
    /// Incumbent `CellIdentity` (`Host 1`).
    pub incumbent_cell: CellIdentity,
    /// Incumbent `term = 1` lease to probe for post-takeover fencing on `Host 1`.
    pub incumbent_stale_lease: AuthorityLeaseRecord,
    /// Initial `term = 1` `AuthorityAssignmentRecord` established on `Host 1`.
    pub initial_assignment: AuthorityAssignmentRecord,
    /// Complete voter roster `CellIdentity` list (`N >= 3`, distinct `ControlDomainId`s).
    pub voter_cells: Vec<CellIdentity>,
    /// Private directory on the successor host (`Host 2`) where the exported
    /// shard snapshot will be materialized.
    pub successor_private_shard_dir: PathBuf,
    /// Successor lease holder `ProcessId` on `Host 2`.
    pub successor_holder_id: ProcessId,
    /// Idempotency key for the `term = 2` takeover lease.
    pub successor_lease_idempotency_key: IdempotencyKey,
    /// Logical timestamp (`ms`) after `incumbent_stale_lease.expires_at_ms`.
    pub takeover_start_ms: i64,
    /// Successor lease TTL (`ms`).
    pub successor_lease_ttl_ms: i64,
    /// Logical timestamp (`ms`) for probing stale-lease rejection on `Host 1`.
    pub stale_probe_at_ms: i64,
}

/// Complete durable & consensus-verified outcome of
/// [`drive_quorum_backed_six_endpoint_takeover`].
#[derive(Clone, Debug)]
pub struct QuorumSixEndpointTakeoverOutcome {
    /// Majority Raft commit receipt for `AssignInitialAuthority`.
    pub initial_consensus_receipt: ConsensusCommitReceipt,
    /// `CommittedControlIndexGate` proof for `AssignInitialAuthority`.
    pub initial_commit_proof: VerifiedControlCommitProof,
    /// Majority Raft commit receipt for `RecordCompletedTakeover`.
    pub takeover_consensus_receipt: ConsensusCommitReceipt,
    /// `CommittedControlIndexGate` proof for `TaskAuthorityTakeoverReceipt`.
    pub takeover_commit_proof: VerifiedControlCommitProof,
    /// 6-endpoint §26.1 barrier coverage audit report (`all_six_canonical_types_present
    /// && all_union_members_signed_and_observed`).
    pub six_endpoint_audit: TakeoverEndpointBarrierAuditReport,
    /// Full `CrossCellTakeoverOutcome` materialized in `Host 2`'s private shard.
    pub takeover_outcome: CrossCellTakeoverOutcome,
    /// Federation `MigrationIntent` + `CheckpointFact` binding recorded in
    /// `Host 2`'s private `CellDirectory`.
    pub federation_binding: TakeoverFederationBindingOutcome,
    /// Majority Raft commit receipt anchoring `Host 2`'s takeover `CheckpointFact`.
    pub checkpoint_anchor_receipt: ConsensusCommitReceipt,
    /// Proof from the surviving `Host 1` over mTLS that its stale `term = 1`
    /// lease is durably fenced (`AuthorityLeaseFenced`) at both prepare & commit gates.
    pub stale_incumbent_probe: StaleIncumbentFenceProbeReport,
}

/// Proposes `proposal` to the `N >= 3` Raft cluster, routing automatically to
/// the current Raft leader over mTLS (`TB_OP_PROPOSE_CONTROL_CMD`) when
/// `caller_host` is a Follower.
///
/// # Errors
///
/// Fails closed if no leader elects/commits within the bounded retry budget or
/// if the state machine rejects the proposal (`DomainRejected`).
#[allow(clippy::too_many_lines)]
pub async fn propose_control_command_over_quorum(
    caller_host: &NetCellHost,
    voter_cells: &[CellIdentity],
    proposal: ControlPlaneProposal,
) -> Result<ConsensusCommitReceipt, QuorumTakeoverDriveError> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seq = 1u64;
    let mut candidate_leader: Option<CellIdentity> = match caller_host.consensus() {
        NetConsensusBackend::MultiHostQuorum(node) => {
            if node.role() == ConsensusRole::Leader {
                Some(caller_host.local_cell())
            } else {
                node.known_leader()
            }
        }
        NetConsensusBackend::SingleHostPassthrough(_) => {
            return Err(QuorumTakeoverDriveError::QuorumRpc(
                "MultiHostQuorum backend required for quorum proposal".to_owned(),
            ));
        }
    };

    while Instant::now() < deadline {
        let mut try_order = Vec::with_capacity(voter_cells.len() + 1);
        if let Some(leader) = candidate_leader {
            try_order.push(leader);
        }
        for &c in voter_cells {
            if !try_order.contains(&c) {
                try_order.push(c);
            }
        }

        for target in try_order {
            if target == caller_host.local_cell() {
                if let NetConsensusBackend::MultiHostQuorum(node) = caller_host.consensus() {
                    match node.propose(proposal, Duration::from_secs(3)).await {
                        Ok(receipt) => return validate_applied_receipt(receipt),
                        Err(ConsensusError::ProposalRejected(
                            ProposalRejectReason::NotLeader { known_leader, .. }
                            | ProposalRejectReason::StaleLeaderTerm { known_leader, .. },
                        )) => {
                            candidate_leader = known_leader;
                        }
                        Err(_) => {}
                    }
                }
            } else {
                seq = seq.saturating_add(1);
                let mut w = WireWriter::new(NET_TAKEOVER_RPC_MAGIC, TB_OP_PROPOSE_CONTROL_CMD);
                w.put_cell(caller_host.local_cell());
                w.put_bytes(&proposal.encode_wire());
                if let Ok(raw) = call_tb_rpc(
                    caller_host,
                    target,
                    TB_OP_PROPOSE_CONTROL_CMD,
                    seq,
                    w.into_vec(),
                )
                .await
                {
                    let mut r = WireReader::new(&raw);
                    match r.get_u8() {
                        Ok(1) => {
                            let idx = r
                                .get_nz_u64()
                                .map_err(|e| QuorumTakeoverDriveError::QuorumRpc(e.to_owned()))?;
                            let term = r
                                .get_nz_u64()
                                .map_err(|e| QuorumTakeoverDriveError::QuorumRpc(e.to_owned()))?;
                            let state_root_digest = r
                                .get_arr32()
                                .map_err(|e| QuorumTakeoverDriveError::QuorumRpc(e.to_owned()))?;
                            let outcome = match r
                                .get_u8()
                                .map_err(|e| QuorumTakeoverDriveError::QuorumRpc(e.to_owned()))?
                            {
                                1 => AppliedProposalOutcome::Applied {
                                    consensus_commit_index: r.get_nz_u64().map_err(|e| {
                                        QuorumTakeoverDriveError::QuorumRpc(e.to_owned())
                                    })?,
                                    idempotent_replay: r.get_bool().map_err(|e| {
                                        QuorumTakeoverDriveError::QuorumRpc(e.to_owned())
                                    })?,
                                },
                                2 => {
                                    let cidx = r.get_nz_u64().map_err(|e| {
                                        QuorumTakeoverDriveError::QuorumRpc(e.to_owned())
                                    })?;
                                    let code = r.get_u8().map_err(|e| {
                                        QuorumTakeoverDriveError::QuorumRpc(e.to_owned())
                                    })?;
                                    let reason =
                                        decode_domain_rejection(code).ok_or_else(|| {
                                            QuorumTakeoverDriveError::QuorumRpc(
                                                "unknown ControlPlaneDomainRejection code"
                                                    .to_owned(),
                                            )
                                        })?;
                                    AppliedProposalOutcome::DomainRejected {
                                        consensus_commit_index: cidx,
                                        reason,
                                    }
                                }
                                _ => {
                                    return Err(QuorumTakeoverDriveError::QuorumRpc(
                                        "unknown AppliedProposalOutcome tag".to_owned(),
                                    ));
                                }
                            };
                            r.finish()
                                .map_err(|e| QuorumTakeoverDriveError::QuorumRpc(e.to_owned()))?;
                            return validate_applied_receipt(ConsensusCommitReceipt {
                                consensus_commit_index: idx,
                                term,
                                state_root_digest,
                                outcome,
                            });
                        }
                        Ok(2) => {
                            if let Ok(kl) = r.get_opt(WireReader::get_cell) {
                                candidate_leader = kl;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
    }

    Err(QuorumTakeoverDriveError::QuorumRpc(
        "timed out waiting for Raft quorum leader commit".to_owned(),
    ))
}

fn validate_applied_receipt(
    receipt: ConsensusCommitReceipt,
) -> Result<ConsensusCommitReceipt, QuorumTakeoverDriveError> {
    match receipt.outcome {
        AppliedProposalOutcome::Applied { .. } => Ok(receipt),
        AppliedProposalOutcome::DomainRejected { reason, .. } => {
            Err(QuorumTakeoverDriveError::QuorumRpc(format!(
                "control-plane state machine rejected proposal: {reason:?}"
            )))
        }
    }
}

const fn decode_domain_rejection(code: u8) -> Option<ControlPlaneDomainRejection> {
    match code {
        1 => Some(ControlPlaneDomainRejection::IdempotencyKeyPayloadConflict),
        2 => Some(ControlPlaneDomainRejection::StaleCellRoutingVersion),
        3 => Some(ControlPlaneDomainRejection::StaleTrustPolicyEpoch),
        4 => Some(ControlPlaneDomainRejection::InsufficientOrInvalidPoolBalance),
        5 => Some(ControlPlaneDomainRejection::NonMonotonicFencingToken),
        6 => Some(ControlPlaneDomainRejection::InvalidLeaseSettlement),
        7 => Some(ControlPlaneDomainRejection::DeviceExclusivityViolation),
        8 => Some(ControlPlaneDomainRejection::TaskAuthorityCasConflict),
        9 => Some(ControlPlaneDomainRejection::AuditAnchorConflict),
        _ => None,
    }
}

/// Collects `>= floor(N / 2) + 1` voter Ed25519 commit attestations across
/// independent `ControlDomainId`s over mTLS (`TB_OP_SIGN_QUORUM_COMMIT`).
///
/// # Errors
///
/// Fails closed if fewer than a strict majority of voters return valid
/// attestations.
pub async fn collect_quorum_commit_attestation_over_mtls(
    caller_host: &NetCellHost,
    voter_cells: &[CellIdentity],
    receipt: ConsensusCommitReceipt,
    subject: &ControlCommitSubject,
    verified_at_ms: u64,
) -> Result<QuorumCommitAttestation, QuorumTakeoverDriveError> {
    let required_quorum = (voter_cells.len() / 2) + 1;
    let mut voter_attestations = Vec::with_capacity(voter_cells.len());
    let mut seq = 100u64;

    for &voter_cell in voter_cells {
        if voter_cell == caller_host.local_cell() {
            if let Some(srv) = caller_host.takeover_rpc_server()
                && let Ok(raw) = srv
                    .handle_sign_quorum_commit(
                        receipt.consensus_commit_index,
                        receipt.term,
                        receipt.state_root_digest,
                        *subject,
                    )
                    .await
                && let Ok(att) = decode_voter_attestation_wire(&raw)
            {
                voter_attestations.push(att);
            }
        } else {
            seq = seq.saturating_add(1);
            let mut w = WireWriter::new(NET_TAKEOVER_RPC_MAGIC, TB_OP_SIGN_QUORUM_COMMIT);
            w.put_cell(caller_host.local_cell());
            w.put_u64(receipt.consensus_commit_index.get());
            w.put_u64(receipt.term.get());
            w.put_arr32(&receipt.state_root_digest);
            w.put_control_commit_subject(subject);
            if let Ok(raw) = call_tb_rpc(
                caller_host,
                voter_cell,
                TB_OP_SIGN_QUORUM_COMMIT,
                seq,
                w.into_vec(),
            )
            .await
                && let Ok(att) = decode_voter_attestation_wire(&raw)
            {
                voter_attestations.push(att);
            }
        }
    }

    if voter_attestations.len() < required_quorum {
        return Err(QuorumTakeoverDriveError::QuorumRpc(format!(
            "collected {} voter attestations, need quorum {required_quorum}",
            voter_attestations.len()
        )));
    }

    Ok(QuorumCommitAttestation {
        consensus_commit_index: receipt.consensus_commit_index,
        consensus_term: receipt.term,
        state_root_digest: receipt.state_root_digest,
        verified_at_ms,
        voter_attestations,
    })
}

fn decode_voter_attestation_wire(raw: &[u8]) -> Result<VoterCommitAttestation, &'static str> {
    let mut r = WireReader::new(raw);
    let voter_principal = PrincipalId::from_bytes(r.get_arr16()?);
    let voter_control_domain = ControlDomainId::from_bytes(r.get_arr16()?);
    let voter_key_id = KeyId::from_bytes(r.get_arr16()?);
    let signature = r.get_arr64()?;
    r.finish()?;
    Ok(VoterCommitAttestation {
        voter_principal,
        voter_control_domain,
        voter_key_id,
        signature,
    })
}

/// Drives an end-to-end `N = 3` Raft quorum-backed `DIST-TASK-002` 6-endpoint
/// authority takeover from `incumbent_cell` (`Host 1`) onto `successor_host`
/// (`Host 2`) over TLS 1.3 mTLS with **zero** shared filesystem paths:
///
/// 1. Enrolls all `N >= 3` voter public keys over mTLS (`TB_OP_GET_VOTER_BOOTSTRAP`)
///    into `successor_host`'s private `IdentityAuthority` and constructs a
///    `CommittedControlIndexGate` in `ConsensusBackingMode::multi_host_quorum`
///    enforcing `[TASK-SCALE-002]` failure-domain independence.
/// 2. Commits `TaskAuthorityShardCommand::AssignInitialAuthority` in the `N = 3`
///    Raft cluster, collects majority voter attestations over mTLS, and admits
///    `initial_assignment` through `CommittedControlIndexGate`.
/// 3. Exports the checkpointed SQLite shard & barrier signer public bootstrap
///    from `incumbent_cell` over mTLS (`TB_OP_EXPORT_TASK_SHARD`) into
///    `successor_host`'s private `successor_private_shard_dir`.
/// 4. Acquires the `term = 2` takeover lease, freezes the 6-endpoint registry
///    (`FrozenForTakeover`), collects all 6 signed canonical §26.1 endpoint
///    barrier observations over mTLS (`MtlsBarrierObservationClient`), and
///    verifies `verify_six_endpoint_takeover_barrier_coverage`.
/// 5. Commits `TaskAuthorityShardCommand::RecordCompletedTakeover` in the `N = 3`
///    Raft cluster, binds the authentic majority `consensus_commit_index` onto
///    `TaskAuthorityTakeoverReceipt`, completes takeover, collects majority
///    voter attestations over mTLS, and admits the completed takeover receipt
///    through `CommittedControlIndexGate`.
/// 6. Reopens the successor participant registry (`DIST-TASK-004`), binds
///    `MigrationIntent` + `CheckpointFact` in `successor_host`'s private
///    `CellDirectory`, anchors the checkpoint in `nlos-consensus`, and probes
///    the surviving `incumbent_cell` over mTLS (`TB_OP_PROBE_STALE_INCUMBENT_FENCED`)
///    to prove old-term `lease_one` is durably fenced (`AuthorityLeaseFenced`).
///
/// # Errors
///
/// Fails closed on any Raft quorum, `CommittedControlIndexGate`, mTLS barrier,
/// 6-endpoint coverage, `SqliteTaskAuthority`, or old-term fencing error.
#[allow(clippy::too_many_lines)]
pub async fn drive_quorum_backed_six_endpoint_takeover(
    successor_host: &NetCellHost,
    request: QuorumSixEndpointTakeoverRequest,
) -> Result<QuorumSixEndpointTakeoverOutcome, QuorumTakeoverDriveError> {
    // 1. Enroll all voter public keys in `successor_host`'s private IdentityAuthority
    //    over mTLS and initialize `CommittedControlIndexGate` (`[TASK-SCALE-002]`).
    let mut voter_roster = Vec::with_capacity(request.voter_cells.len());
    for (idx, &voter_cell) in request.voter_cells.iter().enumerate() {
        let bootstrap_req = if voter_cell == successor_host.local_cell() {
            successor_host
                .takeover_rpc_server()
                .and_then(|srv| {
                    srv.voter_signer
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .as_ref()
                        .map(|s| s.bootstrap_request)
                })
                .ok_or_else(|| {
                    QuorumTakeoverDriveError::QuorumRpc(
                        "local VoterAttestationSigner missing on successor_host".to_owned(),
                    )
                })?
        } else {
            let mut w = WireWriter::new(NET_TAKEOVER_RPC_MAGIC, TB_OP_GET_VOTER_BOOTSTRAP);
            w.put_cell(successor_host.local_cell());
            let raw = call_tb_rpc(
                successor_host,
                voter_cell,
                TB_OP_GET_VOTER_BOOTSTRAP,
                200 + u64::try_from(idx).unwrap_or(0),
                w.into_vec(),
            )
            .await
            .map_err(QuorumTakeoverDriveError::QuorumRpc)?;
            let mut r = WireReader::new(&raw);
            let req = r
                .get_bootstrap_principal()
                .map_err(|e| QuorumTakeoverDriveError::QuorumRpc(e.to_owned()))?;
            r.finish()
                .map_err(|e| QuorumTakeoverDriveError::QuorumRpc(e.to_owned()))?;
            req
        };

        let binding = successor_host.with_cell_host(|ch| {
            ch.runtime()
                .identity
                .bootstrap_principal(bootstrap_req)
                .map_err(SliceKError::Identity)
                .map(nlos_identity::BootstrapDecision::binding)
        })?;
        voter_roster.push((binding.principal_id, binding.control_domain_id));
    }

    let backing_mode = ConsensusBackingMode::multi_host_quorum(&voter_roster)?;
    let mut index_gate = CommittedControlIndexGate::new(backing_mode);

    // 2. Commit `AssignInitialAuthority` in the `N = 3` Raft quorum and admit via
    //    `CommittedControlIndexGate`.
    let old_term_nz = NonZeroU64::new(request.initial_assignment.authority_lease_binding.term)
        .ok_or(ConsensusIndexGateError::ZeroAuthorityTerm)?;
    let mut assign_idem = [0u8; 16];
    assign_idem[0] = 0xA1;
    assign_idem[1..16].copy_from_slice(&request.task_id.as_bytes()[1..16]);

    let initial_proposal = ControlPlaneProposal::new(
        IdempotencyKey::from_bytes(assign_idem),
        ControlPlaneCommand::TaskAuthorityShardMutation(
            TaskAuthorityShardCommand::AssignInitialAuthority {
                assignment_id: request.initial_assignment.assignment_id,
                task_id: request.task_id,
                task_generation: request.initial_assignment.task_generation,
                authority_shard: *request.task_id.as_bytes(),
                home_cell: request.incumbent_cell,
                authority_term: old_term_nz,
                control_epoch: request.initial_assignment.control_epoch,
                participant_registry_generation: request
                    .initial_assignment
                    .participant_registry_binding
                    .generation,
                participant_registry_root: request
                    .initial_assignment
                    .participant_registry_binding
                    .root,
            },
        ),
    );
    let initial_consensus_receipt =
        propose_control_command_over_quorum(successor_host, &request.voter_cells, initial_proposal)
            .await?;

    let initial_subject = ControlCommitSubject::from_assignment(&request.initial_assignment)?;
    let verified_at_u64 = u64::try_from(request.takeover_start_ms).unwrap_or(1_000);
    let initial_attestation = collect_quorum_commit_attestation_over_mtls(
        successor_host,
        &request.voter_cells,
        initial_consensus_receipt,
        &initial_subject,
        verified_at_u64,
    )
    .await?;
    let initial_commit_proof = successor_host
        .with_cell_host(|ch| {
            index_gate.admit_assignment(
                &ch.runtime().identity,
                &request.initial_assignment,
                initial_consensus_receipt.consensus_commit_index,
                &initial_attestation,
            )
        })
        .map_err(QuorumTakeoverDriveError::ConsensusGate)?;

    // 3. Export incumbent shard bytes & barrier signer bootstrap in bounded
    //    `<= 48 KiB` pages over mTLS into `successor_host`'s isolated private directory.
    let (sqlite_bytes, barrier_bootstrap_req) = {
        const SHARD_EXPORT_PAGE_BYTES: u32 = 48 * 1024;
        let mut assembled = Vec::new();
        let mut expected_total: Option<usize> = None;
        let mut boot_opt: Option<BootstrapPrincipalRequest> = None;
        let mut offset = 0u64;
        let mut page_seq = 0u64;

        loop {
            let mut export_w = WireWriter::new(NET_TAKEOVER_RPC_MAGIC, TB_OP_EXPORT_TASK_SHARD);
            export_w.put_cell(successor_host.local_cell());
            export_w.put_arr16(request.task_id.as_bytes());
            export_w.put_u64(offset);
            export_w.put_u32(SHARD_EXPORT_PAGE_BYTES);
            let export_raw = call_tb_rpc(
                successor_host,
                request.incumbent_cell,
                TB_OP_EXPORT_TASK_SHARD,
                300 + page_seq,
                export_w.into_vec(),
            )
            .await
            .map_err(QuorumTakeoverDriveError::QuorumRpc)?;

            let mut r = WireReader::new(&export_raw);
            let total_len_u64 = r
                .get_u64()
                .map_err(|e| QuorumTakeoverDriveError::QuorumRpc(e.to_owned()))?;
            let total_len = usize::try_from(total_len_u64).map_err(|_| {
                QuorumTakeoverDriveError::QuorumRpc("shard size overflow".to_owned())
            })?;
            let chunk = r
                .get_bytes()
                .map_err(|e| QuorumTakeoverDriveError::QuorumRpc(e.to_owned()))?;
            let has_more = r
                .get_bool()
                .map_err(|e| QuorumTakeoverDriveError::QuorumRpc(e.to_owned()))?;
            let has_boot = r
                .get_bool()
                .map_err(|e| QuorumTakeoverDriveError::QuorumRpc(e.to_owned()))?;
            if has_boot {
                boot_opt = Some(
                    r.get_bootstrap_principal()
                        .map_err(|e| QuorumTakeoverDriveError::QuorumRpc(e.to_owned()))?,
                );
            }
            r.finish()
                .map_err(|e| QuorumTakeoverDriveError::QuorumRpc(e.to_owned()))?;

            if let Some(prev_total) = expected_total {
                if prev_total != total_len {
                    return Err(QuorumTakeoverDriveError::QuorumRpc(
                        "incumbent task shard length mutated mid-export".to_owned(),
                    ));
                }
            } else {
                expected_total = Some(total_len);
                assembled.reserve(total_len);
            }
            if chunk.is_empty() && has_more {
                return Err(QuorumTakeoverDriveError::QuorumRpc(
                    "empty page returned with has_more=true during shard export".to_owned(),
                ));
            }
            assembled.extend_from_slice(&chunk);
            offset = u64::try_from(assembled.len()).unwrap_or(u64::MAX);
            page_seq = page_seq.saturating_add(1);
            if !has_more {
                break;
            }
        }

        if Some(assembled.len()) != expected_total {
            return Err(QuorumTakeoverDriveError::QuorumRpc(
                "assembled task shard byte length mismatched expected total".to_owned(),
            ));
        }
        let boot = boot_opt.ok_or_else(|| {
            QuorumTakeoverDriveError::QuorumRpc(
                "missing barrier signer bootstrap on task shard export".to_owned(),
            )
        })?;
        (assembled, boot)
    };

    fs::create_dir_all(&request.successor_private_shard_dir)
        .map_err(|err| QuorumTakeoverDriveError::Host(SliceKError::Io(err)))?;
    let successor_db_path = request
        .successor_private_shard_dir
        .join("successor-tasks.sqlite3");
    fs::write(&successor_db_path, &sqlite_bytes)
        .map_err(|err| QuorumTakeoverDriveError::Host(SliceKError::Io(err)))?;

    successor_host.with_cell_host(|ch| {
        ch.runtime()
            .identity
            .bootstrap_principal(barrier_bootstrap_req)
            .map_err(SliceKError::Identity)
    })?;

    // 4. Open `successor_host`'s private `SqliteTaskAuthority`, advance lease to
    //    `term = 2`, freeze the 6-endpoint registry, and collect all 6 signed
    //    barrier observations over mTLS from `Host 1`.
    let store = SqliteTaskAuthority::open(&successor_db_path).map_err(SliceKError::Task)?;
    let mut clock_ms = request.takeover_start_ms;
    let mut next_clock = || {
        let now = clock_ms;
        clock_ms = clock_ms.saturating_add(10);
        now
    };

    let lease_now_ms = next_clock();
    let wall_now_ms = u64::try_from(lease_now_ms).map_err(|_| TakeoverDriveError::NegativeClock)?;
    let successor_lease = match store
        .acquire_authority_lease_anchored(
            AuthorityLeaseRequest {
                holder_id: request.successor_holder_id,
                idempotency_key: request.successor_lease_idempotency_key,
                requested_at_ms: lease_now_ms,
                ttl_ms: request.successor_lease_ttl_ms,
            },
            wall_now_ms,
        )
        .map_err(TakeoverDriveError::Store)?
    {
        AuthorityLeaseDecision::TakenOver(rec) | AuthorityLeaseDecision::Replayed(rec) => rec,
        AuthorityLeaseDecision::Acquired(_) => {
            return Err(TakeoverDriveError::NoIncumbentAuthorityLease.into());
        }
        AuthorityLeaseDecision::Renewed(_) => {
            return Err(TakeoverDriveError::SuccessorIsIncumbentHolder.into());
        }
    };

    let old_assignment = store
        .inspect_authority_assignment(request.task_id)
        .map_err(TakeoverDriveError::Store)?;
    if old_assignment.state != AuthorityAssignmentState::Active {
        return Err(TakeoverDriveError::OldAssignmentNotActive {
            state: old_assignment.state,
        }
        .into());
    }

    let fence_now_ms = next_clock();
    let frozen_registry = store
        .prepare_authority_takeover_fence_with_receipt_extensions(
            AuthorityLeaseTakeoverFenceRequest {
                task_id: request.task_id,
                expected_registry_binding: old_assignment.participant_registry_binding,
                lease: successor_lease,
                requested_at_ms: fence_now_ms,
            },
            None,
            None,
        )
        .map_err(TakeoverDriveError::Store)?;
    let frozen_binding = ParticipantRegistryBinding {
        generation: frozen_registry.generation,
        root: frozen_registry.root,
    };
    let fence_receipt = store
        .inspect_authority_takeover_fence_receipt(request.task_id, frozen_binding)
        .map_err(TakeoverDriveError::Store)?;
    let exact_fence_set_root = fence_receipt
        .exact_fence_set_root
        .ok_or(TakeoverDriveError::FenceManifestUnavailable)?;
    let takeover_receipt_pending = store
        .inspect_authority_takeover_receipt(request.task_id, fence_receipt.receipt_id)
        .map_err(TakeoverDriveError::Store)?;
    let fence_members = store
        .inspect_authority_takeover_fence_members(request.task_id, frozen_binding)
        .map_err(TakeoverDriveError::Store)?;

    let mut barrier_client = MtlsBarrierObservationClient::new(
        successor_host,
        request.incumbent_cell,
        request.task_id,
        request.successor_holder_id,
        request.successor_lease_idempotency_key,
        lease_now_ms,
        request.successor_lease_ttl_ms,
        frozen_binding,
    );

    let mut observations = Vec::with_capacity(fence_members.len());
    for member in &fence_members {
        let observed_at_ms = next_clock();
        let obs = barrier_client
            .observe_barrier_async(BarrierObservationDemand {
                takeover_receipt_id: takeover_receipt_pending.receipt_id,
                participant: member.participant,
                fence_set_root: exact_fence_set_root,
            })
            .await?;
        let recorded = successor_host
            .with_cell_host(|ch| {
                store.record_authority_takeover_barrier_receipt_signed(
                    &ch.runtime().identity,
                    AuthorityTakeoverBarrierReceiptRequest {
                        takeover_receipt_id: takeover_receipt_pending.receipt_id,
                        participant: member.participant,
                        remote_receipt_id: obs.remote_receipt_id,
                        barrier_digest: obs.barrier_digest,
                        observed_at_ms,
                    },
                    obs.signature,
                )
            })
            .map_err(TakeoverDriveError::Store)?;
        observations.push(recorded);
    }

    let coverage = store
        .inspect_authority_takeover_barrier_coverage(takeover_receipt_pending.receipt_id)
        .map_err(TakeoverDriveError::Store)?;
    if coverage.state != AuthorityTakeoverBarrierCoverageState::LocallyCovered {
        return Err(TakeoverDriveError::BarrierCoverageIncomplete {
            state: coverage.state,
            missing: coverage.missing_participants.len(),
        }
        .into());
    }
    let six_endpoint_audit = store
        .verify_six_endpoint_takeover_barrier_coverage(takeover_receipt_pending.receipt_id)
        .map_err(TakeoverDriveError::Store)?;

    // 5. Commit `RecordCompletedTakeover` in the `N = 3` Raft quorum BEFORE
    //    completing takeover locally so `consensus_commit_index` is authentic!
    let predicted_new_assignment_id = derive_deterministic_assignment_id(
        request.task_id,
        takeover_receipt_pending.task_generation,
        successor_lease.authority_id,
        successor_lease.term,
        frozen_binding,
    );
    let new_term_nz =
        NonZeroU64::new(successor_lease.term).ok_or(ConsensusIndexGateError::ZeroAuthorityTerm)?;
    let mut takeover_idem = [0u8; 16];
    takeover_idem[0] = 0xA2;
    takeover_idem[1..16].copy_from_slice(&takeover_receipt_pending.receipt_id.as_bytes()[1..16]);

    let takeover_proposal = ControlPlaneProposal::new(
        IdempotencyKey::from_bytes(takeover_idem),
        ControlPlaneCommand::TaskAuthorityShardMutation(
            TaskAuthorityShardCommand::RecordCompletedTakeover {
                takeover_receipt_id: takeover_receipt_pending.receipt_id,
                task_id: request.task_id,
                task_generation: takeover_receipt_pending.task_generation,
                old_assignment_id: old_assignment.assignment_id,
                new_assignment_id: predicted_new_assignment_id,
                frozen_old_authority_term: old_term_nz,
                new_authority_term: new_term_nz,
                new_home_cell: successor_host.local_cell(),
                new_control_epoch: takeover_receipt_pending.new_control_epoch,
                new_participant_registry_generation: frozen_binding.generation.saturating_add(1),
                new_participant_registry_root: frozen_binding.root,
                exact_fence_set_root,
                outstanding_effect_quarantine_root: takeover_receipt_pending
                    .outstanding_effect_quarantine_root,
            },
        ),
    );
    let takeover_consensus_receipt = propose_control_command_over_quorum(
        successor_host,
        &request.voter_cells,
        takeover_proposal,
    )
    .await?;

    let _ = store
        .record_authority_takeover_receipt_extensions(
            takeover_receipt_pending.receipt_id,
            takeover_receipt_pending.outstanding_effect_quarantine_root,
            Some(takeover_consensus_receipt.consensus_commit_index.get()),
        )
        .map_err(TakeoverDriveError::Store)?;

    let completed_at_ms = next_clock();
    let completion = store
        .complete_authority_takeover(CompleteAuthorityTakeoverRequest {
            takeover_receipt_id: takeover_receipt_pending.receipt_id,
            lease: successor_lease,
            completed_at_ms,
        })
        .map_err(TakeoverDriveError::Store)?;
    if completion.new_assignment_id != predicted_new_assignment_id
        || completion.barrier_state != AuthorityTakeoverReceiptState::Complete
    {
        return Err(TakeoverDriveError::PostCondition(
            "completed new_assignment_id drifted from deterministic prediction",
        )
        .into());
    }

    let takeover_receipt_completed = store
        .inspect_authority_takeover_receipt(request.task_id, fence_receipt.receipt_id)
        .map_err(TakeoverDriveError::Store)?;
    let takeover_subject =
        ControlCommitSubject::from_takeover_receipt(&takeover_receipt_completed)?;
    let takeover_attestation = collect_quorum_commit_attestation_over_mtls(
        successor_host,
        &request.voter_cells,
        takeover_consensus_receipt,
        &takeover_subject,
        u64::try_from(completed_at_ms).unwrap_or(verified_at_u64 + 100),
    )
    .await?;
    let takeover_commit_proof = successor_host
        .with_cell_host(|ch| {
            index_gate.admit_takeover_receipt(
                &ch.runtime().identity,
                &takeover_receipt_completed,
                &takeover_attestation,
            )
        })
        .map_err(QuorumTakeoverDriveError::ConsensusGate)?;

    // 6. Reopen successor registry (`DIST-TASK-004`), bind federation records
    //    in `successor_host`'s private `CellDirectory`, anchor checkpoint in
    //    `nlos-consensus`, and probe `Host 1` over mTLS to prove old-term fencing.
    let reopened_at_ms = next_clock();
    let reopen = store
        .reopen_successor_registry(AuthoritySuccessorRegistryReopenRequest {
            takeover_receipt_id: takeover_receipt_pending.receipt_id,
            lease: successor_lease,
            reopened_at_ms,
        })
        .map_err(TakeoverDriveError::Store)?;
    let active_assignment = store
        .inspect_authority_assignment(request.task_id)
        .map_err(TakeoverDriveError::Store)?;
    let reopened_registry = store
        .inspect_participant_registry(request.task_id)
        .map_err(TakeoverDriveError::Store)?;

    let takeover_outcome = CrossCellTakeoverOutcome {
        successor_lease,
        old_assignment,
        frozen_registry,
        verified_registry_signer: None,
        fence_receipt,
        fence_members,
        takeover_receipt_pending,
        observations,
        completion,
        takeover_receipt_completed,
        reopen,
        active_assignment,
        reopened_registry,
    };

    let successor_fence = successor_host.current_fence();
    let federation_binding = successor_host.with_cell_host_mut(|ch| {
        let _ = ch.refresh_cell_registration();
        let dir = ch.cell_directory().ok_or(TakeoverDriveError::Federation(
            FederationError::DirectoryUnavailable,
        ))?;
        bind_takeover_to_federation(
            TakeoverFederationContext {
                directory: dir,
                source_cell: request.incumbent_cell,
                target_cell: successor_fence.identity(),
                checkpoint_boot_generation: successor_fence.node_boot_generation(),
                checkpoint_epoch: successor_fence.epoch(),
                checkpoint_fencing_token: successor_fence.fencing_token(),
            },
            &takeover_outcome,
        )
    })?;

    let mut ckpt_idem = [0u8; 16];
    ckpt_idem[0] = 0xA3;
    ckpt_idem[1..16].copy_from_slice(&takeover_receipt_pending.receipt_id.as_bytes()[1..16]);
    let ckpt_seq =
        NonZeroU64::new(federation_binding.checkpoint_record.sequence()).unwrap_or(NonZeroU64::MIN);
    let ckpt_proposal = ControlPlaneProposal::new(
        IdempotencyKey::from_bytes(ckpt_idem),
        ControlPlaneCommand::ControlPlaneAuditAnchor(
            ControlPlaneAuditCommand::AnchorReconciliationCheckpoint {
                cell: successor_fence.identity(),
                node_boot_generation: successor_fence.node_boot_generation(),
                epoch: successor_fence.epoch(),
                checkpoint_seq: ckpt_seq,
                checkpoint_digest: *federation_binding.checkpoint_digest.as_bytes(),
            },
        ),
    );
    let checkpoint_anchor_receipt =
        propose_control_command_over_quorum(successor_host, &request.voter_cells, ckpt_proposal)
            .await?;

    let mut probe_w = WireWriter::new(NET_TAKEOVER_RPC_MAGIC, TB_OP_PROBE_STALE_INCUMBENT_FENCED);
    probe_w.put_cell(successor_host.local_cell());
    probe_w.put_arr16(request.task_id.as_bytes());
    probe_w.put_authority_lease(request.incumbent_stale_lease);
    probe_w.put_u64(frozen_binding.generation);
    probe_w.put_arr32(&frozen_binding.root);
    probe_w.put_i64(request.stale_probe_at_ms);
    let probe_raw = call_tb_rpc(
        successor_host,
        request.incumbent_cell,
        TB_OP_PROBE_STALE_INCUMBENT_FENCED,
        400,
        probe_w.into_vec(),
    )
    .await
    .map_err(QuorumTakeoverDriveError::QuorumRpc)?;
    let stale_incumbent_probe = {
        let mut r = WireReader::new(&probe_raw);
        let stale_prepare_fenced = r
            .get_bool()
            .map_err(|e| QuorumTakeoverDriveError::QuorumRpc(e.to_owned()))?;
        let stale_permit_fenced = r
            .get_bool()
            .map_err(|e| QuorumTakeoverDriveError::QuorumRpc(e.to_owned()))?;
        let observed_durable_term = r
            .get_u64()
            .map_err(|e| QuorumTakeoverDriveError::QuorumRpc(e.to_owned()))?;
        r.finish()
            .map_err(|e| QuorumTakeoverDriveError::QuorumRpc(e.to_owned()))?;
        StaleIncumbentFenceProbeReport {
            stale_prepare_fenced,
            stale_permit_fenced,
            observed_durable_term,
        }
    };
    if !stale_incumbent_probe.stale_prepare_fenced || !stale_incumbent_probe.stale_permit_fenced {
        return Err(QuorumTakeoverDriveError::QuorumRpc(format!(
            "surviving incumbent host was NOT fenced: {stale_incumbent_probe:?}"
        )));
    }

    Ok(QuorumSixEndpointTakeoverOutcome {
        initial_consensus_receipt,
        initial_commit_proof,
        takeover_consensus_receipt,
        takeover_commit_proof,
        six_endpoint_audit,
        takeover_outcome,
        federation_binding,
        checkpoint_anchor_receipt,
        stale_incumbent_probe,
    })
}
