//! `[ROAD-C-001]` Stage-C H6 Combined Distributed Fault-Injection Exit Gate
//! (`W66`, unfreezing `nlos-notify` and `nlos-search` in [`SliceKRuntime`] and
//! [`CellHost`]).
//!
//! Exercises simultaneous / sequential distributed fault modes across real live
//! OS child processes (`ADR-0018`: one OS process = one Cell) over durable
//! `SQLite WAL + FULL` authorities and a shared filesystem [`CellDirectory`]
//! federation root (`ADR-0019`):
//! 1. Control-plane partition (`[DIST-LOCAL-001]`, `[DIST-FAIL-001]`) while both
//!    Cell OS processes stay alive and advance independent heartbeats;
//! 2. Hard OS process crash (`child.kill()` + `child.wait()`) and cold restart
//!    over durable `<cell_root>` (`node_boot_generation` `1 -> 2`, durable
//!    `SqliteLeaseStore`, `ProcessAuthority`, `NotificationService` v2, and
//!    `SemanticAuthority` v9 rehydration);
//! 3. Four-phase partition reconnection convergence with bounded-loss quota
//!    reconciliation (`[LEASE-LOSS-001]`, `[LEASE-RECON-001]`, `[LEASE-WAL-001]`,
//!    `[DIST-EVENT-001]`, `[DIST-ART-001]`);
//! 4. Cross-Cell `TaskAuthority` takeover (`[DIST-TASK-001]`, `[DIST-TASK-002]`,
//!    `[DIST-TASK-004]`) and 8-step `CellHost` process migration
//!    (`[DIST-MIGRATE-001]`, `[DIST-MIGRATE-002]`, `[LEASE-MIGRATE-001]`),
//!    including post-`SourceFenced` crash/quarantine without dual-active
//!    processes;
//! 5. Duplicate message, `ActionToken`, semantic event, artifact package,
//!    federated search shard, and stale-epoch/term replay across reconnection
//!    (`[TASK-NOTIFY-001]`, `[DUI-NOTIFY-001]`, `[DUI-OFFLINE-001]`,
//!    `[SEM-CHECKPOINT-001]`, `[TASK-SNAPSHOT-003]`, `[CONS-DECL-001]`,
//!    `[CONS-CTRL-001]`).

#![allow(clippy::similar_names)]

use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer, SigningKey};
use nlos_artifact::sync::ReplicaRevisionBundle;
use nlos_artifact::{
    ArtifactConflictReceipt, ArtifactConflictStrategy, ArtifactDeletionSemantics,
    ArtifactMergeStrategy, ArtifactReplicaAuthority, ArtifactSyncPolicy, ArtifactTransferPackage,
    ContentDigest, CreateArtifactSpec, HeadState, PutRevisionRequest,
};
use nlos_capability::{
    CapabilityRevocationReceipt, CapabilityRights, CapabilityTarget, DelegateCapabilityRequest,
    IssueRootCapabilityRequest, RevokeCapabilityRequest, SignedDelegateCapabilityRequest,
    SignedIssueRootCapabilityRequest, SignedRevokeCapabilityRequest, delegate_command_message,
    issue_root_command_message, revoke_command_message,
};
use nlos_cell::{
    CellDirectory, CellEpoch, CellFence, CellFencingToken, CellIdentity,
    CheckpointDigestVerification, CheckpointPrefix, ControlPlaneLinkPhase, ControlPlaneLinkState,
    FailureDetectorConfig, MigrationIntent, MigrationIntentRecord, NamePath, PartitionViolation,
    PartitionedActionKind, RemoteCallContract, RemoteCallOutcome,
};
use nlos_identity::{BootstrapPrincipalRequest, IdentityAuthority, KeyPurpose};
use nlos_lease::{
    CapacityLeaseState, CleanupAttestation, DeviceResetReceipt, ExclusiveDeviceLeaseState,
    FenceBarrierAck, LeaseInstant, QuotaLeaseState, QuotaReconciliationReceipt, QuotaWalLossError,
};
use nlos_notify::{
    AckNotificationRequest, ActionTokenId, IssueActionTokenDecision,
    IssueNotificationActionTokenRequest, NotificationId, NotificationPublishDecision, NotifyError,
    RedeemNotificationActionRequest, RegisterDeliveryWaitRequest, TaskNotificationEnvelope,
    TaskTransitionKind,
};
use nlos_operation::{CompletionOutcome, OperationHandle, OperationSpec, OperationState};
use nlos_process::{
    CheckpointPayloadKind, DriverSessionAttachment, MigrationAbortOutcome, MigrationMode,
    PinnedSessionPolicy, ProcessBindingRecord, ProcessCheckpointProof, ProcessMigrationStep,
    QuiesceProof, RegisterFiberIncarnationRequest, SourceProcessFenceProof,
};
use nlos_runtime::{FiberExit, FiberHandle, FiberSpec, RuntimeAdapter};
use nlos_runtime_tokio::{TokioRuntimeAdapter, TokioRuntimeConfig};
use nlos_search::{
    AssertionHit, AssertionSelector, CellSearchShardOutcome, ControlConsumerKind,
    PaginatedAssertionSelector, RemoteSearchFailureReason, SearchAchievedConsistency, SearchError,
    SearchUseRestrictions,
};
use nlos_semantic::{
    AssertionMode, CrossDomainEventItem, DomainSemanticWaterMark, SemanticVectorCheckpoint,
    SignedSemanticVectorCheckpoint,
};
use nlos_slice_k::{
    ArtifactImportCommitContext, ArtifactPartitionConvergenceSpec, BarrierObservationDemand,
    BarrierObservationSource, CapabilityRebindItemSpec, CapacityLeaseMigrationSpec,
    CapacityPartitionSettlementSpec, CellHost, CellHostConfig, CellHostProcessMigrationRequest,
    CellLeaseReconciliationSpec, CellPartitionEndpoint, CellPartitionSpendError,
    CheckpointSourceSpec, CrossCellTakeoverRequest, DeviceLeaseMigrationSpec,
    DevicePartitionSettlementSpec, LeaseMigrationBundleSpec, LocalCellHostPartitionEndpoint,
    LocalCellHostSourceEndpoint, LocalCellHostTargetEndpoint, NotificationTopicConfig,
    OperationReceiptFact, PartitionArtifactImportSummary, PartitionEpochAdvanceSnapshot,
    PartitionRecordedCheckpointSummary, PartitionRecoveryError, PartitionRecoveryRequest,
    PartitionSettledCapacitySummary, PartitionSettledDeviceSummary, ProcessLineageGenesis,
    ProcessLineageSpec, ProcessMigrationDriveError, ProductionTakeoverOptions,
    QuotaLeaseMigrationSpec, QuotaLeaseReconciliationSummary, QuotaPartitionSettlementSpec,
    SemanticConvergenceStepSnapshot, SemanticPartitionConvergenceSpec, SignedBarrierObservation,
    SliceKError, SliceKRuntime, SourceCellMigrationEndpoint, SourceCellPlacementSnapshot,
    SourceCheckpointBundle, SourceCheckpointRequest, SourceClosedQuotaSummary, SourceFenceReport,
    SourceFenceRequest, SourceFenceSpec, SourceLeaseCloseReport, SourcePostFenceQuarantineReport,
    SourceQuiesceRequest, SourceQuiesceSpec, SourceReturnedCapacitySummary,
    SourceReturnedDeviceSummary, TakeoverDriveError, TakeoverFederationContext, TargetRestoreSpec,
    aggregate_federated_cell_candidates, decode_artifact_transfer_package, decode_cell_fence,
    decode_cross_domain_event_items, decode_partition_recovery_checkpoint_entry,
    decode_process_migration_receipt_checkpoint_entry, decode_signed_vector_checkpoint,
    drive_cell_host_process_migration, drive_partition_reconnection_convergence,
    drive_takeover_with_federation_binding, encode_artifact_transfer_package, encode_cell_fence,
    encode_cross_domain_event_items, encode_signed_vector_checkpoint,
    partition_recovery_checkpoint_prefix, provenance_triple, publish_restart_image_artifact,
    remote_call_outcome_to_search_shard, seeded_key,
};
use nlos_task::{
    AuthorityAssignmentState, AuthorityLeaseDecision, AuthorityLeaseFinalizeRequest,
    AuthorityLeasePermitRequest, AuthorityLeaseRecord, AuthorityLeaseRequest,
    AuthorityLeaseTakeoverFenceRequest, AuthorityTakeoverReceiptState, BarrierObservationSignature,
    FinalizeRequest, FinalizeRequestV3, ParticipantRecord, ParticipantRegistryState,
    ParticipantType, PermitDecision, PermitRequest, SnapshotBundle, SqliteTaskAuthority,
    TaskStoreError, barrier_observation_signature_message, empty_effect_history_root,
};
use nlos_topic::{SubscriberKey, TopicId};
use nlos_types::{
    AgentInstanceId, ApplicationId, ArtifactId, CallbackId, CancellationScopeId, CapacityLeaseId,
    CommitPermitId, DeviceId, ExclusiveDeviceLeaseId, ExecutionFiberId, Generation, IdempotencyKey,
    IsolationDomainId, NamespaceId, OperationId, PackageId, PrincipalId, ProcessId, QuotaLeaseId,
    ReceiptId, ResourceAccountId, ResourceGroupId, SchedulerDomainId, SemanticEventId,
    TaskAttemptId, TaskId, TaskParticipantId, TaskSnapshotId,
};
use nlos_wait::{BindingId, WaitState};
use sha2::{Digest, Sha256};

const ENV_WORKER_DOMAIN_HEX: &str = "LLMOS_ROADC001_DOMAIN_HEX";
const ENV_CELL_ROOT: &str = "LLMOS_ROADC001_CELL_ROOT";
const ENV_FEDERATION_ROOT: &str = "LLMOS_ROADC001_FEDERATION_ROOT";
const ENV_MAILBOX_DIR: &str = "LLMOS_ROADC001_MAILBOX_DIR";

const CELL_A_DOMAIN: [u8; 16] = [0xA1; 16];
const CELL_B_DOMAIN: [u8; 16] = [0xB2; 16];
const SHARED_DEVICE_BYTES: [u8; 16] = [0xD4; 16];
const GATEWAY_ID: [u8; 16] = [0xE7; 16];

const INITIAL_QUOTA_POOL: u64 = 1_000;
const INITIAL_CAPACITY_POOL: u64 = 600;

const FIBER_77_BYTES: [u8; 16] = [0x77; 16];
const CANCEL_RECEIPT_BYTES: [u8; 16] = [0x63; 16];
const RESTART_ARTIFACT_CONTENT_TYPE: &str = "application/vnd.llmos.process-restart-image+v1";
const RESTART_ARTIFACT_BYTES: &[u8] = b"llmos-road-c001-restart-image-v1";
const TRANSPARENT_STATE_BYTES: &[u8] = b"llmos-road-c001-transparent-state-v1";
const TRANSPARENT_CACHE_KEY: &str = "road-c001-proc-ckpt-v1";
const CAP_NAME_PATH: &str = "/road-c001/migrated/capability";
const BARRIER_SIGNER_SEED: u8 = 0xA9;

// Worker opcodes
const OP_PING_AND_HEARTBEAT: u8 = 1;
const OP_CURRENT_FENCE: u8 = 2;
const OP_CONNECTIVITY_STATE: u8 = 3;
const OP_ENTER_PARTITION: u8 = 4;
const OP_BEGIN_RECONNECTION: u8 = 5;
const OP_COMPLETE_RECONNECTION: u8 = 6;
const OP_ADVANCE_EPOCH: u8 = 7;
const OP_QUOTA_AVAILABLE: u8 = 8;
const OP_GRANT_LEASES: u8 = 9;
const OP_INSPECT_LEASES: u8 = 10;
const OP_LOCAL_AUTONOMY_SPEND: u8 = 11;
const OP_CHECK_CONTROL_PLANE_ACTION: u8 = 12;
const OP_SETTLE_QUOTA_LEASE: u8 = 13;
const OP_SETTLE_CAPACITY_LEASE: u8 = 14;
const OP_SETTLE_DEVICE_LEASE: u8 = 15;
const OP_EXPORT_WRITER_PRINCIPAL: u8 = 16;
const OP_REGISTER_PEER_PRINCIPAL: u8 = 17;
const OP_EXPORT_SIGNED_VECTOR_CKPT: u8 = 18;
const OP_VERIFY_PEER_VECTOR_CKPT: u8 = 19;
const OP_EXPORT_XDOM_EVENTS: u8 = 20;
const OP_CONVERGE_XDOM_EVENTS: u8 = 21;
const OP_APPEND_SEMANTIC_RECEIPT: u8 = 22;
const OP_PUBLISH_LOCAL_ARTIFACT: u8 = 23;
const OP_EXPORT_ARTIFACT_PKG: u8 = 24;
const OP_IMPORT_ARTIFACT_PKG: u8 = 25;
const OP_INSPECT_LOCAL_ARTIFACT: u8 = 26;
const OP_RECORD_CHECKPOINT: u8 = 27;
const OP_SWEEP_AND_WAL_LOSS_PROBES: u8 = 28;
const OP_RECONCILE_QUOTA_DIRECT: u8 = 29;
const OP_NOTIFY_PUBLISH_AND_POLL: u8 = 30;
const OP_ISSUE_ACTION_TOKEN: u8 = 31;
const OP_REDEEM_ACTION_TOKEN: u8 = 32;
const OP_QUERY_OFFLINE_HISTORY: u8 = 33;
const OP_QUERY_CHECKPOINT_SEARCH: u8 = 34;
const OP_EXPORT_SEARCH_SHARD: u8 = 35;
const OP_SEED_TASK_TERM1: u8 = 36;
const OP_SIGN_BARRIER_OBSERVATION: u8 = 37;
const OP_DRIVE_TASK_TAKEOVER: u8 = 38;
const OP_PROBE_STALE_TASK_AUTHORITY: u8 = 39;
const OP_SEED_MIGRATABLE_PROCESS: u8 = 40;
const OP_INSPECT_PROCESS_STATE: u8 = 41;
const OP_DRIVE_PROCESS_MIGRATION: u8 = 42;

const CMD_SRC_INSPECT_PLACEMENT: u8 = 60;
const CMD_SRC_RECORD_INTENT: u8 = 61;
const CMD_SRC_QUIESCE: u8 = 62;
const CMD_SRC_CAPTURE_CHECKPOINT: u8 = 63;
const CMD_SRC_FENCE_SOURCE: u8 = 64;
const CMD_SRC_CLOSE_LEASES: u8 = 65;
const CMD_SRC_REVOKE_CAPS: u8 = 66;
const CMD_SRC_QUARANTINE_LEASES: u8 = 67;

const OP_SHUTDOWN: u8 = 255;

const RESP_OK: u8 = 0;
const RESP_ERR: u8 = 1;

const NOTIFY_ERR_ALREADY_REDEEMED: u8 = 1;
const NOTIFY_ERR_CROSS_APP_SPOOF: u8 = 2;
const NOTIFY_ERR_BINDING_MISMATCH: u8 = 3;
const NOTIFY_ERR_PROOF_INVALID: u8 = 4;
const NOTIFY_ERR_OTHER: u8 = 9;

// =============================================================================
// Temporary Workspace & Atomic File Mailbox Helpers
// =============================================================================

struct TempDir {
    root: PathBuf,
}

impl TempDir {
    fn new(tag: &str) -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        let seq = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("nlos-road-c001-{tag}-{}-{seq}", std::process::id()));
        std::fs::create_dir_all(&root).expect("create road-c001 temp directory");
        Self { root }
    }

    fn path(&self) -> &Path {
        &self.root
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn hex16(bytes: &[u8; 16]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(32);
    for b in bytes {
        write!(&mut s, "{b:02x}").expect("hex byte");
    }
    s
}

fn parse_hex16(raw: &str) -> [u8; 16] {
    assert_eq!(raw.len(), 32, "expected 32 hex chars");
    let bytes = raw.as_bytes();
    let nibble = |b: u8| match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        other => panic!("invalid hex char: {}", char::from(other)),
    };
    let mut out = [0_u8; 16];
    for (idx, slot) in out.iter_mut().enumerate() {
        *slot = (nibble(bytes[2 * idx]) << 4) | nibble(bytes[2 * idx + 1]);
    }
    out
}

fn detector_config() -> FailureDetectorConfig {
    FailureDetectorConfig::new(
        NonZeroU64::new(3).expect("suspect threshold"),
        NonZeroU64::new(5).expect("dead threshold"),
    )
    .expect("detector config")
}

fn cell_config(domain: [u8; 16]) -> CellHostConfig {
    CellHostConfig::new(
        SchedulerDomainId::from_bytes(domain),
        INITIAL_QUOTA_POOL,
        INITIAL_CAPACITY_POOL,
        DeviceId::from_bytes(SHARED_DEVICE_BYTES),
        detector_config(),
    )
}

fn write_atomic_file(target: &Path, bytes: &[u8]) {
    static TMP_SEQ: AtomicU64 = AtomicU64::new(1);
    let n = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = target.with_extension(format!("tmp.{}.{n}", std::process::id()));
    std::fs::write(&tmp, bytes).expect("write temp ipc frame");
    std::fs::rename(&tmp, target).expect("rename temp ipc frame into place");
}

fn wait_for_file_and_consume(target: &Path, timeout: Duration) -> Vec<u8> {
    let deadline = Instant::now() + timeout;
    loop {
        match std::fs::read(target) {
            Ok(bytes) => {
                let _ = std::fs::remove_file(target);
                return bytes;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting for IPC frame {}",
                    target.display()
                );
                std::thread::sleep(Duration::from_millis(3));
            }
            Err(err) => {
                assert!(
                    Instant::now() < deadline,
                    "failed reading IPC frame {}: {err}",
                    target.display()
                );
                std::thread::sleep(Duration::from_millis(3));
            }
        }
    }
}

// =============================================================================
// Compact Deterministic Binary Wire Framing
// =============================================================================

#[derive(Clone)]
struct WireWriter {
    buf: Vec<u8>,
}

impl WireWriter {
    fn new() -> Self {
        Self {
            buf: Vec::with_capacity(256),
        }
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

    fn put_u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    fn put_u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    fn put_u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    fn put_fixed(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    fn put_bytes(&mut self, bytes: &[u8]) {
        self.put_u64(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
        self.buf.extend_from_slice(bytes);
    }

    fn put_str(&mut self, s: &str) {
        self.put_bytes(s.as_bytes());
    }

    fn put_opt<T>(&mut self, v: Option<T>, f: impl FnOnce(&mut Self, T)) {
        match v {
            None => self.put_u8(0),
            Some(val) => {
                self.put_u8(1);
                f(self, val);
            }
        }
    }

    fn put_head(&mut self, head: HeadState) {
        self.put_u64(head.revision);
        self.put_fixed(head.digest.as_bytes());
    }

    fn put_link_state(&mut self, state: ControlPlaneLinkState) {
        match state {
            ControlPlaneLinkState::Connected {
                verified_fence,
                last_contact_tick,
            } => {
                self.put_u8(0);
                self.put_fixed(&encode_cell_fence(verified_fence));
                self.put_u64(last_contact_tick);
            }
            ControlPlaneLinkState::Partitioned {
                frozen_fence,
                partitioned_since_tick,
                reason_code,
            } => {
                self.put_u8(1);
                self.put_fixed(&encode_cell_fence(frozen_fence));
                self.put_u64(partitioned_since_tick);
                self.put_u16(reason_code);
            }
            ControlPlaneLinkState::Reconnecting {
                prior_frozen_fence,
                candidate_fence,
                started_tick,
            } => {
                self.put_u8(2);
                self.put_fixed(&encode_cell_fence(prior_frozen_fence));
                self.put_fixed(&encode_cell_fence(candidate_fence));
                self.put_u64(started_tick);
            }
        }
    }

    fn put_quota_state(&mut self, state: QuotaLeaseState) {
        let tag = match state {
            QuotaLeaseState::Issued => 0,
            QuotaLeaseState::Active => 1,
            QuotaLeaseState::Closing => 2,
            QuotaLeaseState::Closed => 3,
            QuotaLeaseState::Cancelled => 4,
            QuotaLeaseState::Fenced => 5,
            QuotaLeaseState::Quarantined => 6,
        };
        self.put_u8(tag);
    }

    fn put_cap_state(&mut self, state: CapacityLeaseState) {
        let tag = match state {
            CapacityLeaseState::GlobalReserved => 0,
            CapacityLeaseState::TargetPrepared => 1,
            CapacityLeaseState::Active => 2,
            CapacityLeaseState::Returning => 3,
            CapacityLeaseState::Returned => 4,
            CapacityLeaseState::Fenced => 5,
            CapacityLeaseState::Quarantined => 6,
        };
        self.put_u8(tag);
    }

    fn put_dev_state(&mut self, state: ExclusiveDeviceLeaseState) {
        let tag = match state {
            ExclusiveDeviceLeaseState::DeviceReserved => 0,
            ExclusiveDeviceLeaseState::Resetting => 1,
            ExclusiveDeviceLeaseState::Fencing => 2,
            ExclusiveDeviceLeaseState::Quarantined => 3,
            ExclusiveDeviceLeaseState::Returned => 4,
        };
        self.put_u8(tag);
    }

    fn put_bootstrap_principal(&mut self, req: &BootstrapPrincipalRequest) {
        self.put_fixed(&req.principal_profile_digest);
        self.put_fixed(&req.control_domain_policy_digest);
        self.put_fixed(&req.public_key);
        let purpose = match req.key_purpose {
            KeyPurpose::SemanticSigning => 1,
            KeyPurpose::BarrierObservationSigning => 2,
        };
        self.put_u8(purpose);
        self.put_u64(req.key_valid_from_ms);
        self.put_u64(req.key_valid_until_ms);
        self.put_fixed(req.idempotency_key.as_bytes());
        self.put_u64(req.created_at_ms);
    }

    fn put_vector_checkpoint(&mut self, ckpt: &SemanticVectorCheckpoint) {
        self.put_fixed(&ckpt.checkpoint_id);
        self.put_u64(ckpt.domain_watermarks.len() as u64);
        for wm in &ckpt.domain_watermarks {
            self.put_fixed(&wm.authority_id);
            self.put_u64(wm.event_count);
            self.put_fixed(&wm.event_id_merkle_root);
            self.put_fixed(&wm.latest_admission_receipt_digest);
            self.put_fixed(&wm.trust_snapshot_digest);
        }
        self.put_fixed(&ckpt.covered_event_ids_root);
        self.put_fixed(&ckpt.checkpoint_digest);
    }

    fn put_sync_policy(&mut self, policy: &ArtifactSyncPolicy) {
        self.put_str(&policy.content_type);
        let merge = match policy.merge_strategy {
            ArtifactMergeStrategy::ImmutableContentAddressed => 0,
            ArtifactMergeStrategy::FastForwardLineageOnly => 1,
            ArtifactMergeStrategy::AuthoritativeReplicaWins => 2,
        };
        self.put_u8(merge);
        let conflict = match policy.conflict_strategy {
            ArtifactConflictStrategy::FailClosedConflict => 0,
            ArtifactConflictStrategy::QuarantineDivergentBranch => 1,
            ArtifactConflictStrategy::DeferToAuthoritativeReplica => 2,
        };
        self.put_u8(conflict);
        match policy.replica_authority {
            ArtifactReplicaAuthority::SymmetricPeer => self.put_u8(0),
            ArtifactReplicaAuthority::SingleAuthoritativeCell {
                authoritative_cell_id,
            } => {
                self.put_u8(1);
                self.put_fixed(&authoritative_cell_id);
            }
        }
        let del = match policy.deletion_semantics {
            ArtifactDeletionSemantics::Forbidden => 0,
            ArtifactDeletionSemantics::TombstoneRevisionRequired => 1,
            ArtifactDeletionSemantics::GcAfterRetentionBarrier => 2,
        };
        self.put_u8(del);
    }

    fn put_artifact_import_summary(&mut self, summary: &PartitionArtifactImportSummary) {
        match summary {
            PartitionArtifactImportSummary::ImportedFastForward {
                artifact_id,
                prior_local_head,
                new_head,
                imported_revisions,
            } => {
                self.put_u8(0);
                self.put_fixed(artifact_id.as_bytes());
                self.put_opt(*prior_local_head, Self::put_head);
                self.put_head(*new_head);
                self.put_u64(imported_revisions.len() as u64);
                for rev in imported_revisions {
                    self.put_u64(*rev);
                }
            }
            PartitionArtifactImportSummary::NoOpConverged { artifact_id, head } => {
                self.put_u8(1);
                self.put_fixed(artifact_id.as_bytes());
                self.put_head(*head);
            }
            PartitionArtifactImportSummary::ConflictIsolated {
                conflict_receipt,
                preserved_both_blobs,
                pinned_digests,
            } => {
                self.put_u8(2);
                self.put_fixed(conflict_receipt.artifact_id.as_bytes());
                self.put_str(&conflict_receipt.content_type);
                self.put_head(conflict_receipt.local_head);
                self.put_head(conflict_receipt.remote_head);
                self.put_fixed(&conflict_receipt.origin_cell_id);
                let strat = match conflict_receipt.strategy {
                    ArtifactConflictStrategy::FailClosedConflict => 0,
                    ArtifactConflictStrategy::QuarantineDivergentBranch => 1,
                    ArtifactConflictStrategy::DeferToAuthoritativeReplica => 2,
                };
                self.put_u8(strat);
                self.put_fixed(conflict_receipt.conflict_digest.as_bytes());
                self.put_bool(*preserved_both_blobs);
                self.put_u64(pinned_digests.len() as u64);
                for d in pinned_digests {
                    self.put_fixed(d.as_bytes());
                }
            }
            PartitionArtifactImportSummary::TombstoneApplied {
                artifact_id,
                tombstone_head,
            } => {
                self.put_u8(3);
                self.put_fixed(artifact_id.as_bytes());
                self.put_head(*tombstone_head);
            }
        }
    }

    fn put_binding(&mut self, b: &ProcessBindingRecord) {
        self.put_fixed(b.process_id.as_bytes());
        self.put_u64(b.process_generation.get());
        self.put_fixed(&b.process_fencing_token);
        self.put_fixed(b.agent_instance_id.as_bytes());
        self.put_u64(b.agent_instance_generation.get());
        self.put_fixed(b.task_id.as_bytes());
        self.put_fixed(b.task_attempt_id.as_bytes());
        self.put_u64(b.attempt_generation.get());
        self.put_fixed(b.isolation_domain_id.as_bytes());
        self.put_u64(b.isolation_domain_generation.get());
        self.put_fixed(&b.isolation_domain_fencing_token);
        self.put_opt(b.prior_process_generation, |w, g| w.put_u64(g.get()));
        self.put_fixed(b.idempotency_key.as_bytes());
        self.put_u64(b.created_at_ms);
    }

    fn put_migration_mode(&mut self, m: &MigrationMode) {
        match m {
            MigrationMode::TransparentCheckpointRestore => self.put_u8(0),
            MigrationMode::DowngradedRestartFromArtifact {
                artifact_image_digest,
                dropped_pinned_sessions,
            } => {
                self.put_u8(1);
                self.put_fixed(artifact_image_digest);
                self.put_u64(dropped_pinned_sessions.len() as u64);
                for sid in dropped_pinned_sessions {
                    self.put_fixed(sid);
                }
            }
        }
    }

    fn put_checkpoint_spec(&mut self, spec: &CheckpointSourceSpec) {
        match spec {
            CheckpointSourceSpec::TransparentState {
                state_payload,
                cache_key,
                checkpointed_at_ms,
            } => {
                self.put_u8(0);
                self.put_bytes(state_payload);
                self.put_opt(cache_key.as_deref(), Self::put_str);
                self.put_u64(*checkpointed_at_ms);
            }
            CheckpointSourceSpec::ArtifactRestartImage {
                artifact_id,
                revision,
                sync_from_source_if_missing,
                checkpointed_at_ms,
            } => {
                self.put_u8(1);
                self.put_fixed(artifact_id.as_bytes());
                self.put_u64(*revision);
                self.put_bool(*sync_from_source_if_missing);
                self.put_u64(*checkpointed_at_ms);
            }
        }
    }

    fn put_lease_bundle_spec(&mut self, spec: &LeaseMigrationBundleSpec) {
        self.put_fixed(spec.quota.source_lease_id.as_bytes());
        self.put_fixed(&spec.quota.gateway_id);
        self.put_fixed(spec.quota.target_lease_id.as_bytes());
        self.put_opt(spec.quota.target_face_value, Self::put_u64);
        self.put_bool(spec.quota.activate_on_target);
        self.put_opt(spec.capacity, |w, cap| {
            w.put_fixed(cap.source_lease_id.as_bytes());
            w.put_fixed(cap.reconcile_receipt_id.as_bytes());
            w.put_fixed(&cap.gateway_id);
            w.put_fixed(cap.target_lease_id.as_bytes());
            w.put_opt(cap.target_amount, Self::put_u64);
        });
        self.put_opt(spec.device, |w, dev| {
            w.put_fixed(dev.source_lease_id.as_bytes());
            w.put_fixed(dev.reset_receipt_id.as_bytes());
            w.put_fixed(dev.reconcile_receipt_id.as_bytes());
            w.put_fixed(&dev.gateway_id);
            w.put_fixed(dev.target_lease_id.as_bytes());
            w.put_bool(dev.advance_target_epoch_if_needed);
        });
        self.put_u64(spec.regranted_at_ms);
    }

    fn put_assertion_hit(&mut self, hit: &AssertionHit) {
        self.put_fixed(hit.event_id.as_bytes());
        self.put_u64(hit.log_seq);
        match hit.scope {
            CapabilityTarget::Namespace(ns) => {
                self.put_u8(1);
                self.put_fixed(ns.as_bytes());
            }
            CapabilityTarget::Task(tid) => {
                self.put_u8(2);
                self.put_fixed(tid.as_bytes());
            }
        }
        self.put_fixed(hit.issuer.as_bytes());
        let mode = match hit.assertion_mode {
            AssertionMode::FactFromTool => 1,
            AssertionMode::Inference => 2,
            AssertionMode::Speculation => 3,
            AssertionMode::Directive => 4,
        };
        self.put_u8(mode);
        self.put_fixed(&hit.content_digest);
        self.put_str(&hit.content_media_type);
        self.put_u64(hit.admitted_at_ms);
        self.put_bool(hit.retracted);
    }
}

struct WireReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> WireReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn take(&mut self, len: usize) -> &'a [u8] {
        let slice = &self.bytes[self.pos..self.pos + len];
        self.pos += len;
        slice
    }

    fn take_fixed<const N: usize>(&mut self) -> [u8; N] {
        self.take(N).try_into().expect("fixed slice")
    }

    fn get_u8(&mut self) -> u8 {
        self.take(1)[0]
    }

    fn get_bool(&mut self) -> bool {
        self.get_u8() != 0
    }

    fn get_u16(&mut self) -> u16 {
        u16::from_be_bytes(self.take_fixed::<2>())
    }

    fn get_u32(&mut self) -> u32 {
        u32::from_be_bytes(self.take_fixed::<4>())
    }

    fn get_u64(&mut self) -> u64 {
        u64::from_be_bytes(self.take_fixed::<8>())
    }

    fn get_gen(&mut self) -> Generation {
        Generation::new(NonZeroU64::new(self.get_u64()).expect("nonzero generation"))
    }

    fn get_bytes(&mut self) -> Vec<u8> {
        let len = usize::try_from(self.get_u64()).expect("vec len");
        self.take(len).to_vec()
    }

    fn get_str(&mut self) -> String {
        String::from_utf8(self.get_bytes()).expect("utf8")
    }

    fn get_opt<T>(&mut self, mut f: impl FnMut(&mut Self) -> T) -> Option<T> {
        match self.get_u8() {
            0 => None,
            1 => Some(f(self)),
            other => panic!("invalid option tag {other}"),
        }
    }

    fn get_cell_id(&mut self) -> CellIdentity {
        CellIdentity::from_domain(SchedulerDomainId::from_bytes(self.take_fixed::<16>()))
    }

    fn get_head(&mut self) -> HeadState {
        HeadState {
            revision: self.get_u64(),
            digest: ContentDigest::from_bytes(self.take_fixed::<32>()),
        }
    }

    fn get_link_state(&mut self) -> ControlPlaneLinkState {
        match self.get_u8() {
            0 => ControlPlaneLinkState::Connected {
                verified_fence: decode_cell_fence(&self.take_fixed::<40>()).expect("fence"),
                last_contact_tick: self.get_u64(),
            },
            1 => ControlPlaneLinkState::Partitioned {
                frozen_fence: decode_cell_fence(&self.take_fixed::<40>()).expect("fence"),
                partitioned_since_tick: self.get_u64(),
                reason_code: self.get_u16(),
            },
            2 => ControlPlaneLinkState::Reconnecting {
                prior_frozen_fence: decode_cell_fence(&self.take_fixed::<40>()).expect("fence"),
                candidate_fence: decode_cell_fence(&self.take_fixed::<40>()).expect("fence"),
                started_tick: self.get_u64(),
            },
            other => panic!("unknown connectivity state {other}"),
        }
    }

    fn get_quota_state(&mut self) -> QuotaLeaseState {
        match self.get_u8() {
            0 => QuotaLeaseState::Issued,
            1 => QuotaLeaseState::Active,
            2 => QuotaLeaseState::Closing,
            3 => QuotaLeaseState::Closed,
            4 => QuotaLeaseState::Cancelled,
            5 => QuotaLeaseState::Fenced,
            6 => QuotaLeaseState::Quarantined,
            other => panic!("invalid quota state {other}"),
        }
    }

    fn get_cap_state(&mut self) -> CapacityLeaseState {
        match self.get_u8() {
            0 => CapacityLeaseState::GlobalReserved,
            1 => CapacityLeaseState::TargetPrepared,
            2 => CapacityLeaseState::Active,
            3 => CapacityLeaseState::Returning,
            4 => CapacityLeaseState::Returned,
            5 => CapacityLeaseState::Fenced,
            6 => CapacityLeaseState::Quarantined,
            other => panic!("invalid cap state {other}"),
        }
    }

    fn get_dev_state(&mut self) -> ExclusiveDeviceLeaseState {
        match self.get_u8() {
            0 => ExclusiveDeviceLeaseState::DeviceReserved,
            1 => ExclusiveDeviceLeaseState::Resetting,
            2 => ExclusiveDeviceLeaseState::Fencing,
            3 => ExclusiveDeviceLeaseState::Quarantined,
            4 => ExclusiveDeviceLeaseState::Returned,
            other => panic!("invalid dev state {other}"),
        }
    }

    fn get_bootstrap_principal(&mut self) -> BootstrapPrincipalRequest {
        let principal_profile_digest = self.take_fixed::<32>();
        let control_domain_policy_digest = self.take_fixed::<32>();
        let public_key = self.take_fixed::<32>();
        let key_purpose = match self.get_u8() {
            1 => KeyPurpose::SemanticSigning,
            2 => KeyPurpose::BarrierObservationSigning,
            other => panic!("invalid key purpose {other}"),
        };
        let key_valid_from_ms = self.get_u64();
        let key_valid_until_ms = self.get_u64();
        let idempotency_key = IdempotencyKey::from_bytes(self.take_fixed::<16>());
        let created_at_ms = self.get_u64();
        BootstrapPrincipalRequest {
            principal_profile_digest,
            control_domain_policy_digest,
            public_key,
            key_purpose,
            key_valid_from_ms,
            key_valid_until_ms,
            idempotency_key,
            created_at_ms,
        }
    }

    fn get_vector_checkpoint(&mut self) -> SemanticVectorCheckpoint {
        let checkpoint_id = self.take_fixed::<16>();
        let count = usize::try_from(self.get_u64()).expect("count");
        let mut domain_watermarks = Vec::with_capacity(count);
        for _ in 0..count {
            domain_watermarks.push(DomainSemanticWaterMark {
                authority_id: self.take_fixed::<16>(),
                event_count: self.get_u64(),
                event_id_merkle_root: self.take_fixed::<32>(),
                latest_admission_receipt_digest: self.take_fixed::<32>(),
                trust_snapshot_digest: self.take_fixed::<32>(),
            });
        }
        let covered_event_ids_root = self.take_fixed::<32>();
        let checkpoint_digest = self.take_fixed::<32>();
        SemanticVectorCheckpoint {
            checkpoint_id,
            domain_watermarks,
            covered_event_ids_root,
            checkpoint_digest,
        }
    }

    fn get_sync_policy(&mut self) -> ArtifactSyncPolicy {
        let content_type = self.get_str();
        let merge_strategy = match self.get_u8() {
            0 => ArtifactMergeStrategy::ImmutableContentAddressed,
            1 => ArtifactMergeStrategy::FastForwardLineageOnly,
            2 => ArtifactMergeStrategy::AuthoritativeReplicaWins,
            other => panic!("invalid merge tag {other}"),
        };
        let conflict_strategy = match self.get_u8() {
            0 => ArtifactConflictStrategy::FailClosedConflict,
            1 => ArtifactConflictStrategy::QuarantineDivergentBranch,
            2 => ArtifactConflictStrategy::DeferToAuthoritativeReplica,
            other => panic!("invalid conflict tag {other}"),
        };
        let replica_authority = match self.get_u8() {
            0 => ArtifactReplicaAuthority::SymmetricPeer,
            1 => ArtifactReplicaAuthority::SingleAuthoritativeCell {
                authoritative_cell_id: self.take_fixed::<16>(),
            },
            other => panic!("invalid replica authority tag {other}"),
        };
        let deletion_semantics = match self.get_u8() {
            0 => ArtifactDeletionSemantics::Forbidden,
            1 => ArtifactDeletionSemantics::TombstoneRevisionRequired,
            2 => ArtifactDeletionSemantics::GcAfterRetentionBarrier,
            other => panic!("invalid deletion tag {other}"),
        };
        ArtifactSyncPolicy {
            content_type,
            merge_strategy,
            conflict_strategy,
            replica_authority,
            deletion_semantics,
        }
    }

    fn get_artifact_import_summary(&mut self) -> PartitionArtifactImportSummary {
        match self.get_u8() {
            0 => {
                let artifact_id = ArtifactId::from_bytes(self.take_fixed::<16>());
                let prior_local_head = self.get_opt(Self::get_head);
                let new_head = self.get_head();
                let count = usize::try_from(self.get_u64()).expect("count");
                let mut imported_revisions = Vec::with_capacity(count);
                for _ in 0..count {
                    imported_revisions.push(self.get_u64());
                }
                PartitionArtifactImportSummary::ImportedFastForward {
                    artifact_id,
                    prior_local_head,
                    new_head,
                    imported_revisions,
                }
            }
            1 => PartitionArtifactImportSummary::NoOpConverged {
                artifact_id: ArtifactId::from_bytes(self.take_fixed::<16>()),
                head: self.get_head(),
            },
            2 => {
                let artifact_id = ArtifactId::from_bytes(self.take_fixed::<16>());
                let content_type = self.get_str();
                let local_head = self.get_head();
                let remote_head = self.get_head();
                let origin_cell_id = self.take_fixed::<16>();
                let strategy = match self.get_u8() {
                    0 => ArtifactConflictStrategy::FailClosedConflict,
                    1 => ArtifactConflictStrategy::QuarantineDivergentBranch,
                    2 => ArtifactConflictStrategy::DeferToAuthoritativeReplica,
                    other => panic!("invalid conflict strategy {other}"),
                };
                let conflict_digest = ContentDigest::from_bytes(self.take_fixed::<32>());
                let preserved_both_blobs = self.get_bool();
                let count = usize::try_from(self.get_u64()).expect("count");
                let mut pinned_digests = Vec::with_capacity(count);
                for _ in 0..count {
                    pinned_digests.push(ContentDigest::from_bytes(self.take_fixed::<32>()));
                }
                PartitionArtifactImportSummary::ConflictIsolated {
                    conflict_receipt: ArtifactConflictReceipt {
                        artifact_id,
                        content_type,
                        local_head,
                        remote_head,
                        origin_cell_id,
                        strategy,
                        conflict_digest,
                    },
                    preserved_both_blobs,
                    pinned_digests,
                }
            }
            3 => PartitionArtifactImportSummary::TombstoneApplied {
                artifact_id: ArtifactId::from_bytes(self.take_fixed::<16>()),
                tombstone_head: self.get_head(),
            },
            other => panic!("invalid artifact import summary {other}"),
        }
    }

    fn get_binding(&mut self) -> ProcessBindingRecord {
        ProcessBindingRecord {
            process_id: ProcessId::from_bytes(self.take_fixed::<16>()),
            process_generation: self.get_gen(),
            process_fencing_token: self.take_fixed::<32>(),
            agent_instance_id: AgentInstanceId::from_bytes(self.take_fixed::<16>()),
            agent_instance_generation: self.get_gen(),
            task_id: TaskId::from_bytes(self.take_fixed::<16>()),
            task_attempt_id: TaskAttemptId::from_bytes(self.take_fixed::<16>()),
            attempt_generation: self.get_gen(),
            isolation_domain_id: IsolationDomainId::from_bytes(self.take_fixed::<16>()),
            isolation_domain_generation: self.get_gen(),
            isolation_domain_fencing_token: self.take_fixed::<32>(),
            prior_process_generation: self.get_opt(Self::get_gen),
            idempotency_key: IdempotencyKey::from_bytes(self.take_fixed::<16>()),
            created_at_ms: self.get_u64(),
        }
    }

    fn get_migration_mode(&mut self) -> MigrationMode {
        match self.get_u8() {
            0 => MigrationMode::TransparentCheckpointRestore,
            1 => {
                let artifact_image_digest = self.take_fixed::<32>();
                let count = usize::try_from(self.get_u64()).expect("count");
                let mut dropped_pinned_sessions = Vec::with_capacity(count);
                for _ in 0..count {
                    dropped_pinned_sessions.push(self.take_fixed::<16>());
                }
                MigrationMode::DowngradedRestartFromArtifact {
                    artifact_image_digest,
                    dropped_pinned_sessions,
                }
            }
            other => panic!("invalid migration mode {other}"),
        }
    }

    fn get_checkpoint_spec(&mut self) -> CheckpointSourceSpec {
        match self.get_u8() {
            0 => CheckpointSourceSpec::TransparentState {
                state_payload: self.get_bytes(),
                cache_key: self.get_opt(Self::get_str),
                checkpointed_at_ms: self.get_u64(),
            },
            1 => CheckpointSourceSpec::ArtifactRestartImage {
                artifact_id: ArtifactId::from_bytes(self.take_fixed::<16>()),
                revision: self.get_u64(),
                sync_from_source_if_missing: self.get_bool(),
                checkpointed_at_ms: self.get_u64(),
            },
            other => panic!("invalid checkpoint spec {other}"),
        }
    }

    fn get_lease_bundle_spec(&mut self) -> LeaseMigrationBundleSpec {
        let quota = QuotaLeaseMigrationSpec {
            source_lease_id: QuotaLeaseId::from_bytes(self.take_fixed::<16>()),
            gateway_id: self.take_fixed::<16>(),
            override_fence_ack: None,
            target_lease_id: QuotaLeaseId::from_bytes(self.take_fixed::<16>()),
            target_face_value: self.get_opt(Self::get_u64),
            target_expires_at: None,
            activate_on_target: self.get_bool(),
        };
        let capacity = self.get_opt(|r| CapacityLeaseMigrationSpec {
            source_lease_id: CapacityLeaseId::from_bytes(r.take_fixed::<16>()),
            reconcile_receipt_id: ReceiptId::from_bytes(r.take_fixed::<16>()),
            gateway_id: r.take_fixed::<16>(),
            target_lease_id: CapacityLeaseId::from_bytes(r.take_fixed::<16>()),
            target_amount: r.get_opt(Self::get_u64),
        });
        let device = self.get_opt(|r| DeviceLeaseMigrationSpec {
            source_lease_id: ExclusiveDeviceLeaseId::from_bytes(r.take_fixed::<16>()),
            reset_receipt_id: ReceiptId::from_bytes(r.take_fixed::<16>()),
            reconcile_receipt_id: ReceiptId::from_bytes(r.take_fixed::<16>()),
            gateway_id: r.take_fixed::<16>(),
            target_lease_id: ExclusiveDeviceLeaseId::from_bytes(r.take_fixed::<16>()),
            advance_target_epoch_if_needed: r.get_bool(),
        });
        let regranted_at_ms = self.get_u64();
        LeaseMigrationBundleSpec {
            quota,
            capacity,
            device,
            regranted_at_ms,
        }
    }

    fn get_assertion_hit(&mut self) -> AssertionHit {
        let event_id = SemanticEventId::from_bytes(self.take_fixed::<32>());
        let log_seq = self.get_u64();
        let scope = match self.get_u8() {
            1 => CapabilityTarget::Namespace(NamespaceId::from_bytes(self.take_fixed::<16>())),
            2 => CapabilityTarget::Task(TaskId::from_bytes(self.take_fixed::<16>())),
            other => panic!("invalid CapabilityTarget tag {other}"),
        };
        let issuer = PrincipalId::from_bytes(self.take_fixed::<16>());
        let assertion_mode = match self.get_u8() {
            1 => AssertionMode::FactFromTool,
            2 => AssertionMode::Inference,
            3 => AssertionMode::Speculation,
            4 => AssertionMode::Directive,
            other => panic!("invalid AssertionMode tag {other}"),
        };
        let content_digest = self.take_fixed::<32>();
        let content_media_type = self.get_str();
        let admitted_at_ms = self.get_u64();
        let retracted = self.get_bool();
        AssertionHit {
            event_id,
            log_seq,
            scope,
            issuer,
            assertion_mode,
            content_digest,
            content_media_type,
            admitted_at_ms,
            retracted,
        }
    }
}

// =============================================================================
// Mailbox RPC Client & Child OS Process Worker Handle
// =============================================================================

static GLOBAL_RPC_SEQ: AtomicU64 = AtomicU64::new(1);

fn send_mailbox_rpc(mailbox_dir: &Path, opcode: u8, payload: &[u8]) -> Vec<u8> {
    let seq = GLOBAL_RPC_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut req = Vec::with_capacity(1 + payload.len());
    req.push(opcode);
    req.extend_from_slice(payload);
    let req_path = mailbox_dir.join(format!("req_{seq:06}.bin"));
    let resp_path = mailbox_dir.join(format!("resp_{seq:06}.bin"));
    write_atomic_file(&req_path, &req);
    wait_for_file_and_consume(&resp_path, Duration::from_secs(30))
}

struct CellWorkerHandle {
    child: Option<Child>,
    mailbox_dir: PathBuf,
    os_pid: u32,
    initial_boot_generation: u64,
}

impl CellWorkerHandle {
    fn spawn(
        domain: [u8; 16],
        cell_root: &Path,
        federation_root: &Path,
        mailbox_dir: &Path,
    ) -> Self {
        std::fs::create_dir_all(mailbox_dir).expect("create worker mailbox dir");
        let exe = std::env::current_exe().expect("current test executable");
        let mut child = Command::new(exe)
            .arg("--exact")
            .arg("road_c001_cell_worker")
            .arg("--nocapture")
            .arg("--ignored")
            .env(ENV_WORKER_DOMAIN_HEX, hex16(&domain))
            .env(ENV_CELL_ROOT, cell_root)
            .env(ENV_FEDERATION_ROOT, federation_root)
            .env(ENV_MAILBOX_DIR, mailbox_dir)
            .spawn()
            .expect("spawn road_c001_cell_worker child OS process");

        let ready_path = mailbox_dir.join("worker_ready.bin");
        let deadline = Instant::now() + Duration::from_secs(30);
        let ready_bytes = loop {
            if let Ok(bytes) = std::fs::read(&ready_path) {
                let _ = std::fs::remove_file(&ready_path);
                break bytes;
            }
            if let Some(status) = child.try_wait().expect("try_wait child") {
                panic!("worker exited before publishing worker_ready.bin: {status:?}");
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for worker_ready.bin in {}",
                mailbox_dir.display()
            );
            std::thread::sleep(Duration::from_millis(5));
        };
        let mut r = WireReader::new(&ready_bytes);
        let os_pid = r.get_u32();
        let initial_boot_generation = r.get_u64();
        assert_eq!(os_pid, child.id());
        Self {
            child: Some(child),
            mailbox_dir: mailbox_dir.to_path_buf(),
            os_pid,
            initial_boot_generation,
        }
    }

    fn assert_alive(&mut self) {
        let child = self.child.as_mut().expect("worker handle still active");
        if let Some(status) = child.try_wait().expect("try_wait worker") {
            panic!(
                "expected worker pid={} to remain alive, got {status:?}",
                self.os_pid
            );
        }
    }

    fn rpc(&mut self, opcode: u8, payload: &[u8]) -> Vec<u8> {
        self.assert_alive();
        send_mailbox_rpc(&self.mailbox_dir, opcode, payload)
    }

    fn ping_and_heartbeat(&mut self) -> (u32, u64, u64) {
        let resp = self.rpc(OP_PING_AND_HEARTBEAT, &[]);
        let mut r = WireReader::new(&resp);
        assert_eq!(r.get_u8(), RESP_OK);
        (r.get_u32(), r.get_u64(), r.get_u64())
    }

    fn hard_kill_and_wait(&mut self) -> std::process::ExitStatus {
        let mut child = self
            .child
            .take()
            .expect("worker handle must own live child to kill");
        child.kill().expect("hard-kill child OS process");
        let status = child.wait().expect("wait on hard-killed child OS process");
        assert!(
            !status.success(),
            "hard-killed child OS process must not exit cleanly with 0: {status:?}"
        );
        status
    }

    fn shutdown_cleanly(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = send_mailbox_rpc(&self.mailbox_dir, OP_SHUTDOWN, &[]);
            let status = child.wait().expect("wait for clean worker exit");
            assert!(status.success(), "clean worker shutdown failed: {status:?}");
        }
    }
}

impl Drop for CellWorkerHandle {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
}

// =============================================================================
// IPC Endpoint Proxies (`CellPartitionEndpoint`, `BarrierObservationSource`,
// `SourceCellMigrationEndpoint`)
// =============================================================================

struct IpcPartitionEndpoint<'a> {
    worker: &'a mut CellWorkerHandle,
}

impl CellPartitionEndpoint for IpcPartitionEndpoint<'_> {
    fn current_fence(&mut self) -> Result<CellFence, PartitionRecoveryError> {
        let resp = self.worker.rpc(OP_CURRENT_FENCE, &[]);
        let mut r = WireReader::new(&resp);
        if r.get_u8() != RESP_OK {
            return Err(PartitionRecoveryError::EndpointTransport(r.get_str()));
        }
        decode_cell_fence(&r.take_fixed::<40>()).ok_or_else(|| {
            PartitionRecoveryError::EndpointTransport("failed to decode CellFence".to_string())
        })
    }

    fn connectivity_state(&mut self) -> Result<ControlPlaneLinkState, PartitionRecoveryError> {
        let resp = self.worker.rpc(OP_CONNECTIVITY_STATE, &[]);
        let mut r = WireReader::new(&resp);
        if r.get_u8() != RESP_OK {
            return Err(PartitionRecoveryError::EndpointTransport(r.get_str()));
        }
        Ok(r.get_link_state())
    }

    fn advance_cell_epoch(
        &mut self,
    ) -> Result<PartitionEpochAdvanceSnapshot, PartitionRecoveryError> {
        let resp = self.worker.rpc(OP_ADVANCE_EPOCH, &[]);
        let mut r = WireReader::new(&resp);
        if r.get_u8() != RESP_OK {
            return Err(PartitionRecoveryError::EndpointTransport(r.get_str()));
        }
        Ok(PartitionEpochAdvanceSnapshot {
            fence: decode_cell_fence(&r.take_fixed::<40>()).ok_or_else(|| {
                PartitionRecoveryError::EndpointTransport(
                    "failed to decode advanced CellFence".to_string(),
                )
            })?,
            entries_fenced: usize::try_from(r.get_u64()).expect("usize"),
        })
    }

    fn begin_reconnection(
        &mut self,
        now_tick: u64,
    ) -> Result<ControlPlaneLinkState, PartitionRecoveryError> {
        let mut w = WireWriter::new();
        w.put_u64(now_tick);
        let resp = self.worker.rpc(OP_BEGIN_RECONNECTION, &w.into_vec());
        let mut r = WireReader::new(&resp);
        if r.get_u8() != RESP_OK {
            return Err(PartitionRecoveryError::EndpointTransport(r.get_str()));
        }
        Ok(r.get_link_state())
    }

    fn complete_reconnection(
        &mut self,
        now_tick: u64,
    ) -> Result<ControlPlaneLinkState, PartitionRecoveryError> {
        let mut w = WireWriter::new();
        w.put_u64(now_tick);
        let resp = self.worker.rpc(OP_COMPLETE_RECONNECTION, &w.into_vec());
        let mut r = WireReader::new(&resp);
        if r.get_u8() != RESP_OK {
            return Err(PartitionRecoveryError::EndpointTransport(r.get_str()));
        }
        Ok(r.get_link_state())
    }

    fn quota_available(&mut self) -> Result<u64, PartitionRecoveryError> {
        let resp = self.worker.rpc(OP_QUOTA_AVAILABLE, &[]);
        let mut r = WireReader::new(&resp);
        if r.get_u8() != RESP_OK {
            return Err(PartitionRecoveryError::EndpointTransport(r.get_str()));
        }
        Ok(r.get_u64())
    }

    fn settle_quarantined_quota_lease(
        &mut self,
        spec: QuotaPartitionSettlementSpec,
    ) -> Result<(QuotaLeaseReconciliationSummary, u64), PartitionRecoveryError> {
        let mut w = WireWriter::new();
        w.put_fixed(spec.lease_id.as_bytes());
        w.put_fixed(spec.receipt_id.as_bytes());
        w.put_u64(spec.reconcile_seq);
        w.put_fixed(&spec.gateway_id);
        w.put_u64(spec.attested_spent);
        w.put_u64(spec.unattested_inflight_loss);
        w.put_u64(spec.max_unreported_inflight_window);
        let resp = self.worker.rpc(OP_SETTLE_QUOTA_LEASE, &w.into_vec());
        let mut r = WireReader::new(&resp);
        if r.get_u8() != RESP_OK {
            let tag = r.get_u8();
            if tag == 1 {
                return Err(PartitionRecoveryError::BoundedLossViolation(
                    "unattested_inflight_loss exceeds max_unreported_inflight_window",
                ));
            }
            return Err(PartitionRecoveryError::EndpointTransport(r.get_str()));
        }
        Ok((
            QuotaLeaseReconciliationSummary {
                lease_id: QuotaLeaseId::from_bytes(r.take_fixed::<16>()),
                receipt_id: ReceiptId::from_bytes(r.take_fixed::<16>()),
                prior_epoch: CellEpoch::from_u64(r.get_u64()).expect("epoch"),
                face_value: r.get_u64(),
                attested_spent: r.get_u64(),
                refunded_to_available: r.get_u64(),
                permanently_quarantined_loss: r.get_u64(),
                max_unreported_inflight_window: r.get_u64(),
                post_reconcile_state: r.get_quota_state(),
                fence_barrier_ack_digest: r.take_fixed::<32>(),
            },
            r.get_u64(),
        ))
    }

    fn quarantine_and_reconcile_capacity_lease(
        &mut self,
        spec: CapacityPartitionSettlementSpec,
    ) -> Result<PartitionSettledCapacitySummary, PartitionRecoveryError> {
        let mut w = WireWriter::new();
        w.put_fixed(spec.capacity_lease_id.as_bytes());
        w.put_fixed(spec.receipt_id.as_bytes());
        w.put_fixed(&spec.gateway_id);
        w.put_fixed(spec.cleanup_attestation.target_node().as_bytes());
        w.put_u64(spec.cleanup_attestation.target_node_boot_generation().get());
        w.put_u64(spec.cleanup_attestation.capacity_epoch().get());
        w.put_u64(spec.cleanup_attestation.fencing_token().get());
        w.put_u64(spec.cleanup_attestation.cleaned_amount());
        w.put_bool(spec.cleanup_attestation.holder_processes_terminated());
        w.put_fixed(&spec.cleanup_attestation.attestation_digest());
        let resp = self.worker.rpc(OP_SETTLE_CAPACITY_LEASE, &w.into_vec());
        let mut r = WireReader::new(&resp);
        if r.get_u8() != RESP_OK {
            return Err(PartitionRecoveryError::EndpointTransport(r.get_str()));
        }
        Ok(PartitionSettledCapacitySummary {
            capacity_lease_id: CapacityLeaseId::from_bytes(r.take_fixed::<16>()),
            amount: r.get_u64(),
            final_state: r.get_cap_state(),
            fence_ack_digest: r.take_fixed::<32>(),
        })
    }

    fn quarantine_and_reconcile_device_lease(
        &mut self,
        spec: DevicePartitionSettlementSpec,
    ) -> Result<PartitionSettledDeviceSummary, PartitionRecoveryError> {
        let mut w = WireWriter::new();
        w.put_fixed(spec.device_lease_id.as_bytes());
        w.put_fixed(spec.receipt_id.as_bytes());
        w.put_fixed(&spec.gateway_id);
        w.put_fixed(spec.reset_receipt.receipt_id().as_bytes());
        w.put_fixed(spec.reset_receipt.holder_node().as_bytes());
        w.put_u64(spec.reset_receipt.holder_node_boot_generation().get());
        w.put_u64(spec.reset_receipt.exclusivity_epoch().get());
        w.put_u64(spec.reset_receipt.fencing_token().get());
        w.put_u64(spec.reset_receipt.reset_generation().get());
        let resp = self.worker.rpc(OP_SETTLE_DEVICE_LEASE, &w.into_vec());
        let mut r = WireReader::new(&resp);
        if r.get_u8() != RESP_OK {
            return Err(PartitionRecoveryError::EndpointTransport(r.get_str()));
        }
        Ok(PartitionSettledDeviceSummary {
            device_lease_id: ExclusiveDeviceLeaseId::from_bytes(r.take_fixed::<16>()),
            device_id: DeviceId::from_bytes(r.take_fixed::<16>()),
            exclusivity_epoch: CellEpoch::from_u64(r.get_u64()).expect("epoch"),
            reset_generation: r.get_gen(),
            final_state: r.get_dev_state(),
            fence_ack_digest: r.take_fixed::<32>(),
        })
    }

    fn export_semantic_writer_principal(
        &mut self,
    ) -> Result<BootstrapPrincipalRequest, PartitionRecoveryError> {
        let resp = self.worker.rpc(OP_EXPORT_WRITER_PRINCIPAL, &[]);
        let mut r = WireReader::new(&resp);
        if r.get_u8() != RESP_OK {
            return Err(PartitionRecoveryError::EndpointTransport(r.get_str()));
        }
        Ok(r.get_bootstrap_principal())
    }

    fn register_peer_principal(
        &mut self,
        request: BootstrapPrincipalRequest,
    ) -> Result<(), PartitionRecoveryError> {
        let mut w = WireWriter::new();
        w.put_bootstrap_principal(&request);
        let resp = self.worker.rpc(OP_REGISTER_PEER_PRINCIPAL, &w.into_vec());
        let mut r = WireReader::new(&resp);
        if r.get_u8() != RESP_OK {
            return Err(PartitionRecoveryError::EndpointTransport(r.get_str()));
        }
        Ok(())
    }

    fn export_signed_vector_checkpoint(
        &mut self,
        trust_snapshot_digest: [u8; 32],
        signed_at_ms: u64,
    ) -> Result<SignedSemanticVectorCheckpoint, PartitionRecoveryError> {
        let mut w = WireWriter::new();
        w.put_fixed(&trust_snapshot_digest);
        w.put_u64(signed_at_ms);
        let resp = self.worker.rpc(OP_EXPORT_SIGNED_VECTOR_CKPT, &w.into_vec());
        let mut r = WireReader::new(&resp);
        if r.get_u8() != RESP_OK {
            return Err(PartitionRecoveryError::EndpointTransport(r.get_str()));
        }
        decode_signed_vector_checkpoint(&r.get_bytes()).ok_or_else(|| {
            PartitionRecoveryError::EndpointTransport(
                "failed to decode SignedSemanticVectorCheckpoint".to_string(),
            )
        })
    }

    fn verify_peer_vector_checkpoint(
        &mut self,
        checkpoint: &SignedSemanticVectorCheckpoint,
    ) -> Result<(), PartitionRecoveryError> {
        let mut w = WireWriter::new();
        w.put_bytes(&encode_signed_vector_checkpoint(checkpoint));
        let resp = self.worker.rpc(OP_VERIFY_PEER_VECTOR_CKPT, &w.into_vec());
        let mut r = WireReader::new(&resp);
        if r.get_u8() != RESP_OK {
            return Err(PartitionRecoveryError::EndpointTransport(r.get_str()));
        }
        Ok(())
    }

    fn export_cross_domain_events(
        &mut self,
        trust_snapshot_digest: [u8; 32],
    ) -> Result<Vec<CrossDomainEventItem>, PartitionRecoveryError> {
        let mut w = WireWriter::new();
        w.put_fixed(&trust_snapshot_digest);
        let resp = self.worker.rpc(OP_EXPORT_XDOM_EVENTS, &w.into_vec());
        let mut r = WireReader::new(&resp);
        if r.get_u8() != RESP_OK {
            return Err(PartitionRecoveryError::EndpointTransport(r.get_str()));
        }
        decode_cross_domain_event_items(&r.get_bytes()).ok_or_else(|| {
            PartitionRecoveryError::EndpointTransport(
                "failed to decode CrossDomainEventItem list".to_string(),
            )
        })
    }

    fn converge_cross_domain_events(
        &mut self,
        local_trust_snapshot_digest: [u8; 32],
        remote_items: &[CrossDomainEventItem],
    ) -> Result<SemanticConvergenceStepSnapshot, PartitionRecoveryError> {
        let mut w = WireWriter::new();
        w.put_fixed(&local_trust_snapshot_digest);
        w.put_bytes(&encode_cross_domain_event_items(remote_items));
        let resp = self.worker.rpc(OP_CONVERGE_XDOM_EVENTS, &w.into_vec());
        let mut r = WireReader::new(&resp);
        if r.get_u8() != RESP_OK {
            return Err(PartitionRecoveryError::EndpointTransport(r.get_str()));
        }
        let imp_len = usize::try_from(r.get_u64()).expect("imp len");
        let mut imported_event_ids = Vec::with_capacity(imp_len);
        for _ in 0..imp_len {
            imported_event_ids.push(SemanticEventId::from_bytes(r.take_fixed::<32>()));
        }
        let skip_len = usize::try_from(r.get_u64()).expect("skip len");
        let mut skipped_existing_event_ids = Vec::with_capacity(skip_len);
        for _ in 0..skip_len {
            skipped_existing_event_ids.push(SemanticEventId::from_bytes(r.take_fixed::<32>()));
        }
        let audit_record_count = r.get_u64();
        let vector_checkpoint = r.get_vector_checkpoint();
        Ok(SemanticConvergenceStepSnapshot {
            imported_event_ids,
            skipped_existing_event_ids,
            audit_record_count,
            vector_checkpoint,
        })
    }

    fn export_artifact_package(
        &mut self,
        artifact_id: ArtifactId,
        now_ms: u64,
    ) -> Result<ArtifactTransferPackage, PartitionRecoveryError> {
        let mut w = WireWriter::new();
        w.put_fixed(artifact_id.as_bytes());
        w.put_u64(now_ms);
        let resp = self.worker.rpc(OP_EXPORT_ARTIFACT_PKG, &w.into_vec());
        let mut r = WireReader::new(&resp);
        if r.get_u8() != RESP_OK {
            return Err(PartitionRecoveryError::EndpointTransport(r.get_str()));
        }
        decode_artifact_transfer_package(&r.get_bytes()).ok_or_else(|| {
            PartitionRecoveryError::EndpointTransport(
                "failed to decode ArtifactTransferPackage".to_string(),
            )
        })
    }

    fn import_artifact_package(
        &mut self,
        policies: &[ArtifactSyncPolicy],
        package: &ArtifactTransferPackage,
        commit_ctx: ArtifactImportCommitContext,
        idempotency_key: IdempotencyKey,
        synced_at_ms: u64,
    ) -> Result<PartitionArtifactImportSummary, PartitionRecoveryError> {
        let mut w = WireWriter::new();
        w.put_u64(policies.len() as u64);
        for p in policies {
            w.put_sync_policy(p);
        }
        w.put_bytes(&encode_artifact_transfer_package(package));
        w.put_fixed(commit_ctx.task_id.as_bytes());
        w.put_fixed(commit_ctx.permit_id.as_bytes());
        w.put_fixed(commit_ctx.write_set_root.as_bytes());
        w.put_fixed(commit_ctx.base_idempotency_key.as_bytes());
        w.put_fixed(idempotency_key.as_bytes());
        w.put_u64(synced_at_ms);
        let resp = self.worker.rpc(OP_IMPORT_ARTIFACT_PKG, &w.into_vec());
        let mut r = WireReader::new(&resp);
        if r.get_u8() != RESP_OK {
            return Err(PartitionRecoveryError::EndpointTransport(r.get_str()));
        }
        Ok(r.get_artifact_import_summary())
    }

    fn refresh_and_record_checkpoint(
        &mut self,
        prefix: &CheckpointPrefix,
    ) -> Result<Option<PartitionRecordedCheckpointSummary>, PartitionRecoveryError> {
        let mut w = WireWriter::new();
        w.put_u64(prefix.entries().len() as u64);
        for entry in prefix.entries() {
            w.put_bytes(entry);
        }
        let resp = self.worker.rpc(OP_RECORD_CHECKPOINT, &w.into_vec());
        let mut r = WireReader::new(&resp);
        if r.get_u8() != RESP_OK {
            return Err(PartitionRecoveryError::EndpointTransport(r.get_str()));
        }
        if !r.get_bool() {
            return Ok(None);
        }
        Ok(Some(PartitionRecordedCheckpointSummary {
            cell: r.get_cell_id(),
            epoch: CellEpoch::from_u64(r.get_u64()).expect("epoch"),
            fencing_token: CellFencingToken::from_u64(r.get_u64()).expect("token"),
            heartbeat: r.get_u64(),
            sequence: r.get_u64(),
            os_process_id: r.get_u32(),
            digest_bytes: r.take_fixed::<32>(),
            digest_verified: r.get_bool(),
        }))
    }
}

struct IpcBarrierSource {
    peer_mailbox_dir: PathBuf,
}

impl BarrierObservationSource for IpcBarrierSource {
    fn observe_barrier(
        &mut self,
        demand: BarrierObservationDemand,
    ) -> Result<SignedBarrierObservation, TakeoverDriveError> {
        let mut w = WireWriter::new();
        w.put_fixed(demand.takeover_receipt_id.as_bytes());
        let ptype = match demand.participant.participant_type {
            ParticipantType::TaskStore => 1,
            ParticipantType::ArtifactHead => 2,
            ParticipantType::SemanticAdmission => 3,
            ParticipantType::ChannelTopic => 4,
            ParticipantType::DriverGateway => 5,
            ParticipantType::ResourceLedger => 6,
            ParticipantType::ProcessBinding => 7,
            ParticipantType::OperationBinding => 8,
        };
        w.put_u8(ptype);
        w.put_fixed(demand.participant.participant_id.as_bytes());
        w.put_u64(demand.participant.participant_generation.get());
        w.put_fixed(demand.participant.admission_receipt_id.as_bytes());
        w.put_fixed(&demand.fence_set_root);

        let resp = send_mailbox_rpc(
            &self.peer_mailbox_dir,
            OP_SIGN_BARRIER_OBSERVATION,
            &w.into_vec(),
        );
        let mut r = WireReader::new(&resp);
        if r.get_u8() != RESP_OK {
            return Err(TakeoverDriveError::BarrierSource(r.get_str()));
        }
        Ok(SignedBarrierObservation {
            remote_receipt_id: ReceiptId::from_bytes(r.take_fixed::<16>()),
            barrier_digest: r.take_fixed::<32>(),
            signature: BarrierObservationSignature {
                issuer: PrincipalId::from_bytes(r.take_fixed::<16>()),
                control_domain_id: nlos_types::ControlDomainId::from_bytes(r.take_fixed::<16>()),
                key_id: nlos_types::KeyId::from_bytes(r.take_fixed::<16>()),
                signature: r.take_fixed::<64>(),
            },
        })
    }
}

struct IpcSourceCellEndpoint {
    source_mailbox_dir: PathBuf,
    federation_root: PathBuf,
}

impl IpcSourceCellEndpoint {
    fn call_source(&self, opcode: u8, payload: &[u8]) -> Result<Vec<u8>, String> {
        let raw = send_mailbox_rpc(&self.source_mailbox_dir, opcode, payload);
        let mut r = WireReader::new(&raw);
        if r.get_u8() == RESP_OK {
            Ok(raw[1..].to_vec())
        } else {
            Err(r.get_str())
        }
    }
}

impl SourceCellMigrationEndpoint for IpcSourceCellEndpoint {
    fn inspect_source_placement(
        &self,
        process_id: ProcessId,
        expected_target: CellIdentity,
    ) -> Result<SourceCellPlacementSnapshot, ProcessMigrationDriveError> {
        let mut w = WireWriter::new();
        w.put_fixed(process_id.as_bytes());
        w.put_fixed(expected_target.as_bytes());
        let body = self
            .call_source(CMD_SRC_INSPECT_PLACEMENT, &w.into_vec())
            .map_err(ProcessMigrationDriveError::Transport)?;
        let mut r = WireReader::new(&body);
        Ok(SourceCellPlacementSnapshot {
            cell_id: r.get_cell_id(),
            node_boot_generation: r.get_gen(),
            epoch: CellEpoch::from_u64(r.get_u64()).expect("epoch"),
            fencing_token: CellFencingToken::from_u64(r.get_u64()).expect("token"),
            both_cells_registered_in_directory: r.get_bool(),
            active_binding: r.get_binding(),
            supervisor_registered: r.get_bool(),
        })
    }

    fn record_migration_intent(
        &mut self,
        intent: &MigrationIntent,
    ) -> Result<MigrationIntentRecord, ProcessMigrationDriveError> {
        let mut w = WireWriter::new();
        w.put_fixed(intent.source().as_bytes());
        w.put_fixed(intent.target().as_bytes());
        w.put_fixed(intent.object().as_bytes());
        w.put_u64(intent.generation().get());
        w.put_str(intent.reason());
        let body = self
            .call_source(CMD_SRC_RECORD_INTENT, &w.into_vec())
            .map_err(ProcessMigrationDriveError::Transport)?;
        let mut r = WireReader::new(&body);
        let os_pid = r.get_u32();
        let sequence = r.get_u64();
        let dir = CellDirectory::open(&self.federation_root)
            .map_err(|e| ProcessMigrationDriveError::Transport(e.to_string()))?;
        dir.migration_intents()
            .map_err(|e| ProcessMigrationDriveError::Transport(e.to_string()))?
            .into_records()
            .into_iter()
            .find(|rec| rec.os_process_id() == os_pid && rec.sequence() == sequence)
            .ok_or_else(|| {
                ProcessMigrationDriveError::Transport("recorded MigrationIntent not found".into())
            })
    }

    fn quiesce_source_process(
        &mut self,
        request: &SourceQuiesceRequest,
    ) -> Result<QuiesceProof, ProcessMigrationDriveError> {
        let mut w = WireWriter::new();
        w.put_fixed(request.process_id.as_bytes());
        w.put_fixed(request.source_cell_id.as_bytes());
        w.put_u64(request.source_generation.get());
        w.put_opt(request.cancel_scope, |w, (sid, sgen)| {
            w.put_fixed(sid.as_bytes());
            w.put_u64(sgen.get());
        });
        w.put_u32(request.active_fiber_count);
        w.put_u64(request.inflight_operations.len() as u64);
        for (op, rid) in &request.inflight_operations {
            w.put_fixed(op.operation_id.as_bytes());
            w.put_u64(op.generation.get());
            w.put_fixed(rid.as_bytes());
        }
        w.put_u64(request.quiesced_at_ms);
        let body = self
            .call_source(CMD_SRC_QUIESCE, &w.into_vec())
            .map_err(ProcessMigrationDriveError::Transport)?;
        let mut r = WireReader::new(&body);
        Ok(QuiesceProof {
            process_id: ProcessId::from_bytes(r.take_fixed::<16>()),
            source_cell_id: r.take_fixed::<16>(),
            source_generation: r.get_u64(),
            quiesced_fibers: r.get_u32(),
            cancelled_inflight_ops: r.get_u32(),
            inflight_effects_drained: r.get_bool(),
            quiesced_at_ms: r.get_u64(),
        })
    }

    fn capture_source_checkpoint(
        &mut self,
        request: &SourceCheckpointRequest,
    ) -> Result<SourceCheckpointBundle, ProcessMigrationDriveError> {
        let mut w = WireWriter::new();
        w.put_fixed(&request.migration_id);
        w.put_fixed(request.process_id.as_bytes());
        w.put_fixed(request.source_cell_id.as_bytes());
        w.put_u64(request.source_generation.get());
        w.put_migration_mode(&request.migration_mode);
        w.put_u64(request.migratable_sessions.len() as u64);
        for (sid, sdig) in &request.migratable_sessions {
            w.put_fixed(sid);
            w.put_fixed(sdig);
        }
        w.put_checkpoint_spec(&request.spec);
        let body = self
            .call_source(CMD_SRC_CAPTURE_CHECKPOINT, &w.into_vec())
            .map_err(ProcessMigrationDriveError::Transport)?;
        let mut r = WireReader::new(&body);
        let process_id = ProcessId::from_bytes(r.take_fixed::<16>());
        let source_generation = r.get_u64();
        let payload = match r.get_u8() {
            0 => {
                let state_checkpoint_digest = r.take_fixed::<32>();
                let count = usize::try_from(r.get_u64()).expect("count");
                let mut migrated_session_digests = Vec::with_capacity(count);
                for _ in 0..count {
                    migrated_session_digests.push((r.take_fixed::<16>(), r.take_fixed::<32>()));
                }
                CheckpointPayloadKind::FullStateCheckpoint {
                    state_checkpoint_digest,
                    migrated_session_digests,
                }
            }
            1 => CheckpointPayloadKind::ArtifactRestartImage {
                artifact_image_digest: r.take_fixed::<32>(),
            },
            other => panic!("invalid checkpoint payload tag {other}"),
        };
        let checkpointed_at_ms = r.get_u64();
        let transparent_cache_transfer = r.get_opt(|r| (r.get_str(), r.get_bytes(), r.get_u64()));
        let exported_artifact_bundle = r.get_opt(|r| {
            let artifact_id = ArtifactId::from_bytes(r.take_fixed::<16>());
            let content_type = r.get_str();
            let remote_head = r.get_head();
            let parent_head = r.get_opt(WireReader::get_head);
            let anc_len = usize::try_from(r.get_u64()).expect("anc_len");
            let mut ancestor_lineage = Vec::with_capacity(anc_len);
            for _ in 0..anc_len {
                ancestor_lineage.push(r.get_head());
            }
            let origin_cell_id = r.take_fixed::<16>();
            let bytes = r.get_bytes();
            let is_tombstone = r.get_bool();
            ReplicaRevisionBundle {
                artifact_id,
                content_type,
                remote_head,
                parent_head,
                ancestor_lineage,
                origin_cell_id,
                bytes,
                is_tombstone,
            }
        });
        Ok(SourceCheckpointBundle {
            proof: ProcessCheckpointProof {
                process_id,
                source_generation,
                payload,
                checkpointed_at_ms,
            },
            transparent_cache_transfer,
            exported_artifact_bundle,
        })
    }

    fn fence_source_process(
        &mut self,
        request: &SourceFenceRequest,
    ) -> Result<SourceFenceReport, ProcessMigrationDriveError> {
        let mut w = WireWriter::new();
        w.put_fixed(request.process_id.as_bytes());
        w.put_fixed(request.source_cell_id.as_bytes());
        w.put_u64(request.source_generation.get());
        w.put_u64(request.invalidate_name_paths.len() as u64);
        for np in &request.invalidate_name_paths {
            w.put_str(np.as_str());
        }
        w.put_fixed(request.terminate_idempotency_key.as_bytes());
        w.put_fixed(request.stale_dispatch_probe_fiber_id.as_bytes());
        w.put_fixed(request.stale_dispatch_probe_key.as_bytes());
        w.put_u64(request.fenced_at_ms);
        let body = self
            .call_source(CMD_SRC_FENCE_SOURCE, &w.into_vec())
            .map_err(ProcessMigrationDriveError::Transport)?;
        let mut r = WireReader::new(&body);
        Ok(SourceFenceReport {
            proof: SourceProcessFenceProof {
                process_id: ProcessId::from_bytes(r.take_fixed::<16>()),
                source_cell_id: r.take_fixed::<16>(),
                source_cell_epoch: r.get_u64(),
                fenced_generation: r.get_u64(),
                source_fencing_token: r.take_fixed::<32>(),
                supervisor_unregistered: r.get_bool(),
                fenced_at_ms: r.get_u64(),
            },
            stale_heartbeat_refused: r.get_bool(),
            stale_dispatch_refused: r.get_bool(),
            invalidated_name_paths: usize::try_from(r.get_u64()).expect("usize"),
        })
    }

    fn close_or_fence_source_leases(
        &mut self,
        spec: &LeaseMigrationBundleSpec,
    ) -> Result<SourceLeaseCloseReport, ProcessMigrationDriveError> {
        let mut w = WireWriter::new();
        w.put_lease_bundle_spec(spec);
        let body = self
            .call_source(CMD_SRC_CLOSE_LEASES, &w.into_vec())
            .map_err(ProcessMigrationDriveError::Transport)?;
        let mut r = WireReader::new(&body);
        let closed_quota = SourceClosedQuotaSummary {
            lease_id: QuotaLeaseId::from_bytes(r.take_fixed::<16>()),
            state: r.get_quota_state(),
            face_value: r.get_u64(),
            spent: r.get_u64(),
            returned: r.get_u64(),
            close_digest: r.take_fixed::<32>(),
        };
        let returned_capacity = r.get_opt(|r| SourceReturnedCapacitySummary {
            capacity_lease_id: CapacityLeaseId::from_bytes(r.take_fixed::<16>()),
            state: r.get_cap_state(),
            amount: r.get_u64(),
            close_digest: r.take_fixed::<32>(),
        });
        let returned_device = r.get_opt(|r| SourceReturnedDeviceSummary {
            device_lease_id: ExclusiveDeviceLeaseId::from_bytes(r.take_fixed::<16>()),
            state: r.get_dev_state(),
            exclusivity_epoch: CellEpoch::from_u64(r.get_u64()).expect("epoch"),
            head_reset_generation: r.get_gen(),
            close_digest: r.take_fixed::<32>(),
        });
        Ok(SourceLeaseCloseReport {
            closed_quota,
            returned_capacity,
            returned_device,
        })
    }

    fn revoke_source_capabilities(
        &mut self,
        revocations: &[SignedRevokeCapabilityRequest],
    ) -> Result<Vec<CapabilityRevocationReceipt>, ProcessMigrationDriveError> {
        let mut w = WireWriter::new();
        w.put_u64(revocations.len() as u64);
        for req in revocations {
            w.put_fixed(req.command.handle.capability_id.as_bytes());
            w.put_u64(req.command.handle.generation.get());
            w.put_fixed(req.command.revoker_key_id.as_bytes());
            w.put_fixed(req.command.idempotency_key.as_bytes());
            w.put_u64(req.command.revoked_at_ms);
            w.put_fixed(req.signer.as_bytes());
            w.put_fixed(&req.signature);
        }
        let body = self
            .call_source(CMD_SRC_REVOKE_CAPS, &w.into_vec())
            .map_err(ProcessMigrationDriveError::Transport)?;
        let mut r = WireReader::new(&body);
        let count = usize::try_from(r.get_u64()).expect("count");
        let mut receipts = Vec::with_capacity(count);
        for _ in 0..count {
            receipts.push(CapabilityRevocationReceipt {
                receipt_id: ReceiptId::from_bytes(r.take_fixed::<16>()),
                capability_id: nlos_types::CapabilityId::from_bytes(r.take_fixed::<16>()),
                prior_generation: r.get_gen(),
                resulting_generation: r.get_gen(),
                revoker: PrincipalId::from_bytes(r.take_fixed::<16>()),
                revoked_at_ms: r.get_u64(),
            });
        }
        Ok(receipts)
    }

    fn quarantine_unsettled_source_leases(
        &mut self,
        spec: &LeaseMigrationBundleSpec,
    ) -> Result<SourcePostFenceQuarantineReport, ProcessMigrationDriveError> {
        let mut w = WireWriter::new();
        w.put_lease_bundle_spec(spec);
        let body = self
            .call_source(CMD_SRC_QUARANTINE_LEASES, &w.into_vec())
            .map_err(ProcessMigrationDriveError::Transport)?;
        let mut r = WireReader::new(&body);
        Ok(SourcePostFenceQuarantineReport {
            quota_state: r.get_quota_state(),
            capacity_state: r.get_opt(WireReader::get_cap_state),
            device_state: r.get_opt(WireReader::get_dev_state),
        })
    }
}

// =============================================================================
// Fixture Builders: Capability Rebind, Migratable Process, & Barrier Signer
// =============================================================================

fn make_capability_bootstrap(seed: u8) -> (SigningKey, BootstrapPrincipalRequest) {
    let signing_key = SigningKey::from_bytes(&[seed; 32]);
    let req = BootstrapPrincipalRequest {
        principal_profile_digest: [seed.wrapping_add(1); 32],
        control_domain_policy_digest: [seed.wrapping_add(2); 32],
        public_key: signing_key.verifying_key().to_bytes(),
        key_purpose: KeyPurpose::SemanticSigning,
        key_valid_from_ms: 0,
        key_valid_until_ms: 10_000_000,
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(3); 16]),
        created_at_ms: 0,
    };
    (signing_key, req)
}

#[allow(clippy::too_many_lines)]
fn seed_source_capabilities_on_host(
    host: &mut CellHost,
    task_id: TaskId,
    source_generation: Generation,
    seed_base: u8,
) {
    let (issuer_sk, issuer_boot) = make_capability_bootstrap(seed_base);
    let (holder_sk, holder_boot) = make_capability_bootstrap(seed_base.wrapping_add(0x10));
    let (_target_sk, target_boot) = make_capability_bootstrap(seed_base.wrapping_add(0x20));

    let issuer_binding = host
        .runtime()
        .identity
        .bootstrap_principal(issuer_boot)
        .expect("bootstrap capability issuer")
        .binding();
    let holder_binding = host
        .runtime()
        .identity
        .bootstrap_principal(holder_boot)
        .expect("bootstrap source capability holder")
        .binding();
    let target_binding = host
        .runtime()
        .identity
        .bootstrap_principal(target_boot)
        .expect("bootstrap target capability recipient")
        .binding();

    let root_rights = CapabilityRights::SEMANTIC_APPEND
        .union(CapabilityRights::SEMANTIC_RETRACT)
        .union(CapabilityRights::DELEGATE);
    let root_cmd = IssueRootCapabilityRequest {
        issuer_key_id: issuer_binding.key_id,
        holder_key_id: holder_binding.key_id,
        target: CapabilityTarget::Task(task_id),
        rights: root_rights,
        purpose_digest: Some([0x5A; 32]),
        valid_from_ms: 100,
        valid_until_ms: 5_000_000,
        delegation_depth_remaining: 3,
        call_limit: Some(100),
        idempotency_key: seeded_key(seed_base, 140),
        issued_at_ms: 200,
    };
    let root_sig = issuer_sk
        .sign(&issue_root_command_message(root_cmd))
        .to_bytes();
    let signed_root = SignedIssueRootCapabilityRequest {
        command: root_cmd,
        signer: issuer_binding.principal_id,
        signature: root_sig,
    };
    let source_root_record = host
        .runtime()
        .capability()
        .issue_root_signed(&host.runtime().identity, signed_root)
        .expect("issue root capability on source")
        .record();

    // Delegate once on Source for the active source process binding, and cache
    // under CAP_NAME_PATH at `source_generation`.
    let src_child_cmd = DelegateCapabilityRequest {
        parent: source_root_record.handle,
        delegator_key_id: holder_binding.key_id,
        recipient_key_id: holder_binding.key_id,
        target: CapabilityTarget::Task(task_id),
        rights: CapabilityRights::SEMANTIC_APPEND.union(CapabilityRights::DELEGATE),
        purpose_digest: Some([0x5A; 32]),
        valid_from_ms: 150,
        valid_until_ms: 4_000_000,
        delegation_depth_remaining: 2,
        call_limit: Some(80),
        idempotency_key: seeded_key(seed_base, 141),
        delegated_at_ms: 220,
    };
    let src_child_sig = holder_sk
        .sign(&delegate_command_message(src_child_cmd))
        .to_bytes();
    let source_child_record = host
        .runtime()
        .capability()
        .delegate_signed(
            &host.runtime().identity,
            SignedDelegateCapabilityRequest {
                command: src_child_cmd,
                signer: holder_binding.principal_id,
                signature: src_child_sig,
            },
        )
        .expect("delegate source child capability")
        .record();

    let name_path = cap_name_path_for(seed_base);
    host.insert_cached_name(
        name_path.clone(),
        source_child_record.handle.capability_id,
        source_generation,
    )
    .expect("cache capability on source");

    // Prepare attenuated delegation for Target (strictly narrower rights,
    // validity, call_limit, and depth: zero authority amplification) plus
    // revocation of `source_child_record` on Source after Step 5 rebind.
    let target_del_cmd = DelegateCapabilityRequest {
        parent: source_root_record.handle,
        delegator_key_id: holder_binding.key_id,
        recipient_key_id: target_binding.key_id,
        target: CapabilityTarget::Task(task_id),
        rights: CapabilityRights::SEMANTIC_APPEND,
        purpose_digest: Some([0x5A; 32]),
        valid_from_ms: 200,
        valid_until_ms: 3_000_000,
        delegation_depth_remaining: 1,
        call_limit: Some(40),
        idempotency_key: seeded_key(seed_base, 142),
        delegated_at_ms: 250,
    };
    let target_del_sig = holder_sk
        .sign(&delegate_command_message(target_del_cmd))
        .to_bytes();
    let signed_delegation = SignedDelegateCapabilityRequest {
        command: target_del_cmd,
        signer: holder_binding.principal_id,
        signature: target_del_sig,
    };

    let revoke_cmd = RevokeCapabilityRequest {
        handle: source_child_record.handle,
        revoker_key_id: holder_binding.key_id,
        idempotency_key: seeded_key(seed_base, 143),
        revoked_at_ms: 260,
    };
    let revoke_sig = holder_sk
        .sign(&revoke_command_message(revoke_cmd))
        .to_bytes();
    let signed_revoke = SignedRevokeCapabilityRequest {
        command: revoke_cmd,
        signer: holder_binding.principal_id,
        signature: revoke_sig,
    };

    let rebind_item = CapabilityRebindItemSpec {
        bootstrap_principals: vec![issuer_boot, holder_boot, target_boot],
        seed_parent_root: Some(signed_root),
        signed_delegation,
        target_name_path: Some(name_path),
        revoke_on_source: Some(signed_revoke),
    };
    assert_eq!(source_root_record.delegation_depth_remaining, 3);
    assert_eq!(source_child_record.handle.generation, Generation::INITIAL);
    assert!(rebind_item.revoke_on_source.is_some());
}

fn cap_name_path_for(seed_base: u8) -> NamePath {
    NamePath::new(&format!("{CAP_NAME_PATH}-{seed_base:02x}")).expect("valid cap name path")
}

fn participant_wire_code(participant_type: ParticipantType) -> u8 {
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

fn deterministic_barrier_receipt_id(
    takeover_receipt_id: ReceiptId,
    participant: &ParticipantRecord,
) -> ReceiptId {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/road-c001/barrier-receipt/v1");
    hasher.update(takeover_receipt_id.as_bytes());
    hasher.update([participant_wire_code(participant.participant_type)]);
    hasher.update(participant.participant_id.as_bytes());
    hasher.update(participant.participant_generation.get().to_be_bytes());
    hasher.update(participant.admission_receipt_id.as_bytes());
    let digest: [u8; 32] = hasher.finalize().into();
    ReceiptId::from_bytes(digest[..16].try_into().expect("16-byte receipt id"))
}

fn deterministic_barrier_digest(
    takeover_receipt_id: ReceiptId,
    participant: &ParticipantRecord,
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/road-c001/barrier-digest/v1");
    hasher.update(takeover_receipt_id.as_bytes());
    hasher.update([participant_wire_code(participant.participant_type)]);
    hasher.update(participant.participant_id.as_bytes());
    hasher.update(participant.participant_generation.get().to_be_bytes());
    hasher.update(participant.admission_receipt_id.as_bytes());
    hasher.finalize().into()
}

struct WorkerMigrationSeedState {
    crash_after_source_fence: bool,
}

// =============================================================================
// Source-Side Process Migration RPC Handler (Executed Inside Source Cell OS PID)
// =============================================================================

#[allow(clippy::too_many_lines)]
fn handle_source_migration_rpc(
    opcode: u8,
    payload: &[u8],
    host: &mut CellHost,
    adapter: &TokioRuntimeAdapter,
    mig_state: Option<&WorkerMigrationSeedState>,
) -> Vec<u8> {
    let mut r = WireReader::new(payload);
    let mut out = WireWriter::new();
    let mut ep = LocalCellHostSourceEndpoint::with_adapter(host, adapter);

    match opcode {
        CMD_SRC_INSPECT_PLACEMENT => {
            let process_id = ProcessId::from_bytes(r.take_fixed::<16>());
            let expected_target = r.get_cell_id();
            match ep.inspect_source_placement(process_id, expected_target) {
                Ok(snap) => {
                    out.put_u8(RESP_OK);
                    out.put_fixed(snap.cell_id.as_bytes());
                    out.put_u64(snap.node_boot_generation.get());
                    out.put_u64(snap.epoch.get());
                    out.put_u64(snap.fencing_token.get());
                    out.put_bool(snap.both_cells_registered_in_directory);
                    out.put_binding(&snap.active_binding);
                    out.put_bool(snap.supervisor_registered);
                }
                Err(e) => {
                    out.put_u8(RESP_ERR);
                    out.put_str(&e.to_string());
                }
            }
        }
        CMD_SRC_RECORD_INTENT => {
            let src = r.get_cell_id();
            let dst = r.get_cell_id();
            let obj = nlos_cell::MigrationObject::from_bytes(r.take_fixed::<16>());
            let generation = r.get_gen();
            let reason = r.get_str();
            let intent = MigrationIntent::new(src, dst, obj, generation, &reason)
                .expect("valid migration intent");
            match ep.record_migration_intent(&intent) {
                Ok(rec) => {
                    out.put_u8(RESP_OK);
                    out.put_u32(rec.os_process_id());
                    out.put_u64(rec.sequence());
                }
                Err(e) => {
                    out.put_u8(RESP_ERR);
                    out.put_str(&e.to_string());
                }
            }
        }
        CMD_SRC_QUIESCE => {
            let process_id = ProcessId::from_bytes(r.take_fixed::<16>());
            let source_cell_id = r.get_cell_id();
            let source_generation = r.get_gen();
            let cancel_scope = r.get_opt(|r| {
                (
                    CancellationScopeId::from_bytes(r.take_fixed::<16>()),
                    r.get_gen(),
                )
            });
            let active_fiber_count = r.get_u32();
            let ops_len = usize::try_from(r.get_u64()).expect("ops len");
            let mut inflight_operations = Vec::with_capacity(ops_len);
            for _ in 0..ops_len {
                let op_id = OperationId::from_bytes(r.take_fixed::<16>());
                let op_gen = r.get_gen();
                let rid = ReceiptId::from_bytes(r.take_fixed::<16>());
                inflight_operations.push((
                    OperationHandle {
                        operation_id: op_id,
                        generation: op_gen,
                    },
                    rid,
                ));
            }
            let quiesced_at_ms = r.get_u64();
            let req = SourceQuiesceRequest {
                process_id,
                source_cell_id,
                source_generation,
                cancel_scope,
                active_fiber_count,
                inflight_operations,
                quiesced_at_ms,
            };
            match ep.quiesce_source_process(&req) {
                Ok(proof) => {
                    out.put_u8(RESP_OK);
                    out.put_fixed(proof.process_id.as_bytes());
                    out.put_fixed(&proof.source_cell_id);
                    out.put_u64(proof.source_generation);
                    out.put_u32(proof.quiesced_fibers);
                    out.put_u32(proof.cancelled_inflight_ops);
                    out.put_bool(proof.inflight_effects_drained);
                    out.put_u64(proof.quiesced_at_ms);
                }
                Err(e) => {
                    out.put_u8(RESP_ERR);
                    out.put_str(&e.to_string());
                }
            }
        }
        CMD_SRC_CAPTURE_CHECKPOINT => {
            let migration_id = r.take_fixed::<16>();
            let process_id = ProcessId::from_bytes(r.take_fixed::<16>());
            let source_cell_id = r.get_cell_id();
            let source_generation = r.get_gen();
            let migration_mode = r.get_migration_mode();
            let sess_len = usize::try_from(r.get_u64()).expect("sess len");
            let mut migratable_sessions = Vec::with_capacity(sess_len);
            for _ in 0..sess_len {
                migratable_sessions.push((r.take_fixed::<16>(), r.take_fixed::<32>()));
            }
            let spec = r.get_checkpoint_spec();
            let req = SourceCheckpointRequest {
                migration_id,
                process_id,
                source_cell_id,
                source_generation,
                migration_mode,
                migratable_sessions,
                spec,
            };
            match ep.capture_source_checkpoint(&req) {
                Ok(bundle) => {
                    out.put_u8(RESP_OK);
                    out.put_fixed(bundle.proof.process_id.as_bytes());
                    out.put_u64(bundle.proof.source_generation);
                    match &bundle.proof.payload {
                        CheckpointPayloadKind::FullStateCheckpoint {
                            state_checkpoint_digest,
                            migrated_session_digests,
                        } => {
                            out.put_u8(0);
                            out.put_fixed(state_checkpoint_digest);
                            out.put_u64(migrated_session_digests.len() as u64);
                            for (sid, sdig) in migrated_session_digests {
                                out.put_fixed(sid);
                                out.put_fixed(sdig);
                            }
                        }
                        CheckpointPayloadKind::ArtifactRestartImage {
                            artifact_image_digest,
                        } => {
                            out.put_u8(1);
                            out.put_fixed(artifact_image_digest);
                        }
                    }
                    out.put_u64(bundle.proof.checkpointed_at_ms);
                    out.put_opt(bundle.transparent_cache_transfer, |w, (k, b, ts)| {
                        w.put_str(&k);
                        w.put_bytes(&b);
                        w.put_u64(ts);
                    });
                    out.put_opt(bundle.exported_artifact_bundle, |w, b| {
                        w.put_fixed(b.artifact_id.as_bytes());
                        w.put_str(&b.content_type);
                        w.put_head(b.remote_head);
                        w.put_opt(b.parent_head, WireWriter::put_head);
                        w.put_u64(b.ancestor_lineage.len() as u64);
                        for h in &b.ancestor_lineage {
                            w.put_head(*h);
                        }
                        w.put_fixed(&b.origin_cell_id);
                        w.put_bytes(&b.bytes);
                        w.put_bool(b.is_tombstone);
                    });
                }
                Err(e) => {
                    out.put_u8(RESP_ERR);
                    out.put_str(&e.to_string());
                }
            }
        }
        CMD_SRC_FENCE_SOURCE => {
            let process_id = ProcessId::from_bytes(r.take_fixed::<16>());
            let source_cell_id = r.get_cell_id();
            let source_generation = r.get_gen();
            let np_len = usize::try_from(r.get_u64()).expect("np len");
            let mut invalidate_name_paths = Vec::with_capacity(np_len);
            for _ in 0..np_len {
                invalidate_name_paths.push(NamePath::new(&r.get_str()).expect("name path"));
            }
            let terminate_idempotency_key = IdempotencyKey::from_bytes(r.take_fixed::<16>());
            let stale_dispatch_probe_fiber_id = ExecutionFiberId::from_bytes(r.take_fixed::<16>());
            let stale_dispatch_probe_key = IdempotencyKey::from_bytes(r.take_fixed::<16>());
            let fenced_at_ms = r.get_u64();
            let req = SourceFenceRequest {
                process_id,
                source_cell_id,
                source_generation,
                invalidate_name_paths,
                terminate_idempotency_key,
                stale_dispatch_probe_fiber_id,
                stale_dispatch_probe_key,
                fenced_at_ms,
            };
            match ep.fence_source_process(&req) {
                Ok(report) => {
                    out.put_u8(RESP_OK);
                    out.put_fixed(report.proof.process_id.as_bytes());
                    out.put_fixed(&report.proof.source_cell_id);
                    out.put_u64(report.proof.source_cell_epoch);
                    out.put_u64(report.proof.fenced_generation);
                    out.put_fixed(&report.proof.source_fencing_token);
                    out.put_bool(report.proof.supervisor_unregistered);
                    out.put_u64(report.proof.fenced_at_ms);
                    out.put_bool(report.stale_heartbeat_refused);
                    out.put_bool(report.stale_dispatch_refused);
                    out.put_u64(report.invalidated_name_paths as u64);
                }
                Err(e) => {
                    out.put_u8(RESP_ERR);
                    out.put_str(&e.to_string());
                }
            }
        }
        CMD_SRC_CLOSE_LEASES => {
            if mig_state.is_some_and(|st| st.crash_after_source_fence) {
                out.put_u8(RESP_ERR);
                out.put_str(
                    "injected post-SourceFenced crash/disconnect before Step 6 lease close",
                );
                return out.into_vec();
            }
            let spec = r.get_lease_bundle_spec();
            match ep.close_or_fence_source_leases(&spec) {
                Ok(report) => {
                    out.put_u8(RESP_OK);
                    out.put_fixed(report.closed_quota.lease_id.as_bytes());
                    out.put_quota_state(report.closed_quota.state);
                    out.put_u64(report.closed_quota.face_value);
                    out.put_u64(report.closed_quota.spent);
                    out.put_u64(report.closed_quota.returned);
                    out.put_fixed(&report.closed_quota.close_digest);
                    out.put_opt(report.returned_capacity, |w, cap| {
                        w.put_fixed(cap.capacity_lease_id.as_bytes());
                        w.put_cap_state(cap.state);
                        w.put_u64(cap.amount);
                        w.put_fixed(&cap.close_digest);
                    });
                    out.put_opt(report.returned_device, |w, dev| {
                        w.put_fixed(dev.device_lease_id.as_bytes());
                        w.put_dev_state(dev.state);
                        w.put_u64(dev.exclusivity_epoch.get());
                        w.put_u64(dev.head_reset_generation.get());
                        w.put_fixed(&dev.close_digest);
                    });
                }
                Err(e) => {
                    out.put_u8(RESP_ERR);
                    out.put_str(&e.to_string());
                }
            }
        }
        CMD_SRC_REVOKE_CAPS => {
            let count = usize::try_from(r.get_u64()).expect("count");
            let mut revocations = Vec::with_capacity(count);
            for _ in 0..count {
                let capability_id = nlos_types::CapabilityId::from_bytes(r.take_fixed::<16>());
                let generation = r.get_gen();
                let revoker_key_id = nlos_types::KeyId::from_bytes(r.take_fixed::<16>());
                let idempotency_key = IdempotencyKey::from_bytes(r.take_fixed::<16>());
                let revoked_at_ms = r.get_u64();
                let signer = PrincipalId::from_bytes(r.take_fixed::<16>());
                let signature = r.take_fixed::<64>();
                revocations.push(SignedRevokeCapabilityRequest {
                    command: RevokeCapabilityRequest {
                        handle: nlos_capability::CapabilityHandle {
                            capability_id,
                            generation,
                        },
                        revoker_key_id,
                        idempotency_key,
                        revoked_at_ms,
                    },
                    signer,
                    signature,
                });
            }
            match ep.revoke_source_capabilities(&revocations) {
                Ok(receipts) => {
                    out.put_u8(RESP_OK);
                    out.put_u64(receipts.len() as u64);
                    for rcpt in receipts {
                        out.put_fixed(rcpt.receipt_id.as_bytes());
                        out.put_fixed(rcpt.capability_id.as_bytes());
                        out.put_u64(rcpt.prior_generation.get());
                        out.put_u64(rcpt.resulting_generation.get());
                        out.put_fixed(rcpt.revoker.as_bytes());
                        out.put_u64(rcpt.revoked_at_ms);
                    }
                }
                Err(e) => {
                    out.put_u8(RESP_ERR);
                    out.put_str(&e.to_string());
                }
            }
        }
        CMD_SRC_QUARANTINE_LEASES => {
            let spec = r.get_lease_bundle_spec();
            match ep.quarantine_unsettled_source_leases(&spec) {
                Ok(report) => {
                    out.put_u8(RESP_OK);
                    out.put_quota_state(report.quota_state);
                    out.put_opt(report.capacity_state, WireWriter::put_cap_state);
                    out.put_opt(report.device_state, WireWriter::put_dev_state);
                }
                Err(e) => {
                    out.put_u8(RESP_ERR);
                    out.put_str(&e.to_string());
                }
            }
        }
        other => panic!("unexpected source migration opcode {other}"),
    }
    out.into_vec()
}

// =============================================================================
// Worker Opcode Handler & Child OS Process Entrypoint (`road_c001_cell_worker`)
// =============================================================================

struct WorkerTaskTerm1State {
    lease_one: AuthorityLeaseRecord,
    registry_binding: nlos_task::ParticipantRegistryBinding,
}

#[allow(clippy::too_many_lines)]
fn handle_worker_opcode(
    opcode: u8,
    payload: &[u8],
    host: &mut CellHost,
    adapter: &TokioRuntimeAdapter,
    live_fibers: &mut Vec<FiberHandle>,
    mig_state: &mut Option<WorkerMigrationSeedState>,
    task_term1: &mut Option<WorkerTaskTerm1State>,
) -> Vec<u8> {
    if (CMD_SRC_INSPECT_PLACEMENT..=CMD_SRC_QUARANTINE_LEASES).contains(&opcode) {
        return handle_source_migration_rpc(opcode, payload, host, adapter, mig_state.as_ref());
    }

    let mut r = WireReader::new(payload);
    let mut out = WireWriter::new();

    match opcode {
        OP_PING_AND_HEARTBEAT => {
            let hb = host
                .refresh_cell_registration()
                .expect("refresh federation registration")
                .map_or(0, |e| e.heartbeat());
            out.put_u8(RESP_OK);
            out.put_u32(std::process::id());
            out.put_u64(host.fence().node_boot_generation().get());
            out.put_u64(hb);
        }
        OP_CURRENT_FENCE => {
            out.put_u8(RESP_OK);
            out.put_fixed(&encode_cell_fence(host.fence()));
        }
        OP_CONNECTIVITY_STATE => {
            out.put_u8(RESP_OK);
            out.put_link_state(host.connectivity_state());
        }
        OP_ENTER_PARTITION => {
            let now_tick = r.get_u64();
            let reason_code = r.get_u16();
            match host.enter_control_plane_partition(now_tick, reason_code) {
                Ok(state) => {
                    out.put_u8(RESP_OK);
                    out.put_link_state(state);
                }
                Err(e) => {
                    out.put_u8(RESP_ERR);
                    out.put_str(&e.to_string());
                }
            }
        }
        OP_BEGIN_RECONNECTION => {
            let now_tick = r.get_u64();
            let mut ep = LocalCellHostPartitionEndpoint::new(host);
            match ep.begin_reconnection(now_tick) {
                Ok(state) => {
                    out.put_u8(RESP_OK);
                    out.put_link_state(state);
                }
                Err(e) => {
                    out.put_u8(RESP_ERR);
                    out.put_str(&e.to_string());
                }
            }
        }
        OP_COMPLETE_RECONNECTION => {
            let now_tick = r.get_u64();
            let mut ep = LocalCellHostPartitionEndpoint::new(host);
            match ep.complete_reconnection(now_tick) {
                Ok(state) => {
                    out.put_u8(RESP_OK);
                    out.put_link_state(state);
                }
                Err(e) => {
                    out.put_u8(RESP_ERR);
                    out.put_str(&e.to_string());
                }
            }
        }
        OP_ADVANCE_EPOCH => {
            let mut ep = LocalCellHostPartitionEndpoint::new(host);
            match ep.advance_cell_epoch() {
                Ok(snap) => {
                    out.put_u8(RESP_OK);
                    out.put_fixed(&encode_cell_fence(snap.fence));
                    out.put_u64(snap.entries_fenced as u64);
                }
                Err(e) => {
                    out.put_u8(RESP_ERR);
                    out.put_str(&e.to_string());
                }
            }
        }
        OP_QUOTA_AVAILABLE => {
            out.put_u8(RESP_OK);
            out.put_u64(host.quota_available());
        }
        OP_GRANT_LEASES => {
            let quota_id = QuotaLeaseId::from_bytes(r.take_fixed::<16>());
            let quota_face = r.get_u64();
            let quota_ttl = r.get_opt(|r| LeaseInstant::new(r.get_u64()));
            let activate = r.get_bool();
            let cap_spec = r.get_opt(|r| {
                (
                    CapacityLeaseId::from_bytes(r.take_fixed::<16>()),
                    r.get_u64(),
                )
            });
            let dev_spec = r.get_opt(|r| ExclusiveDeviceLeaseId::from_bytes(r.take_fixed::<16>()));

            let q = host
                .grant_quota_lease(quota_id, quota_face, quota_ttl)
                .expect("grant quota lease");
            let q = if activate {
                host.activate_quota_lease(quota_id)
                    .expect("activate quota lease")
            } else {
                q
            };
            if let Some((cid, amt)) = cap_spec {
                host.grant_capacity_lease(cid, amt)
                    .expect("grant capacity lease");
            }
            if let Some(did) = dev_spec {
                host.grant_device_lease(did).expect("grant device lease");
            }
            out.put_u8(RESP_OK);
            out.put_u64(host.quota_available());
            out.put_u64(host.capacity_pool_remaining());
            out.put_bool(host.device_head_free());
            out.put_quota_state(q.state());
            out.put_u64(host.device_reset_generation().get());
        }
        OP_INSPECT_LEASES => {
            let quota_id = QuotaLeaseId::from_bytes(r.take_fixed::<16>());
            let cap_id = r.get_opt(|r| CapacityLeaseId::from_bytes(r.take_fixed::<16>()));
            let dev_id = r.get_opt(|r| ExclusiveDeviceLeaseId::from_bytes(r.take_fixed::<16>()));
            let q = host.query_quota_lease(quota_id).expect("query quota lease");
            out.put_u8(RESP_OK);
            out.put_u64(host.quota_available());
            out.put_u64(host.capacity_pool_remaining());
            out.put_bool(host.device_head_free());
            out.put_quota_state(q.state());
            out.put_u64(q.face_value());
            out.put_u64(q.spent());
            out.put_u64(q.returned());
            out.put_u64(q.remaining());
            out.put_u64(q.epoch().get());
            out.put_u64(q.node_boot_generation().get());
            out.put_opt(cap_id, |w, cid| {
                let c = host.query_capacity_lease(cid).expect("query cap lease");
                w.put_cap_state(c.state());
                w.put_u64(c.amount());
                w.put_u64(c.target_node_boot_generation().get());
                w.put_u64(c.capacity_epoch().get());
                w.put_u64(c.fencing_token().get());
            });
            out.put_opt(dev_id, |w, did| {
                let d = host.query_device_lease(did).expect("query dev lease");
                w.put_dev_state(d.state());
                w.put_u64(d.holder_node_boot_generation().get());
                w.put_u64(d.exclusivity_epoch().get());
                w.put_u64(d.fencing_token().get());
                w.put_u64(d.reset_generation().get());
            });
        }
        OP_LOCAL_AUTONOMY_SPEND => {
            let now_tick = r.get_u64();
            let lease_id = QuotaLeaseId::from_bytes(r.take_fixed::<16>());
            let spend_delta = r.get_u64();
            let cap_valid_until = r.get_u64();
            let cap_revoked = r.get_bool();
            let cap_verified = r.get_bool();
            match host.spend_quota_lease_with_local_autonomy(
                now_tick,
                lease_id,
                spend_delta,
                cap_valid_until,
                cap_revoked,
                cap_verified,
            ) {
                Ok((_rcpt, grant)) => {
                    out.put_u8(RESP_OK);
                    out.put_u64(grant.spent());
                    out.put_u64(grant.remaining());
                    out.put_quota_state(grant.state());
                }
                Err(CellPartitionSpendError::Partition(pv)) => {
                    out.put_u8(RESP_ERR);
                    out.put_u8(1);
                    out.put_str(&format!("{pv:?}"));
                }
                Err(CellPartitionSpendError::Ledger(le)) => {
                    out.put_u8(RESP_ERR);
                    out.put_u8(2);
                    out.put_str(&format!("{le:?}"));
                }
            }
        }
        OP_CHECK_CONTROL_PLANE_ACTION => {
            let now_tick = r.get_u64();
            let kind = match r.get_u8() {
                0 | 3 => PartitionedActionKind::RequestQuotaExpansionOrRenewal,
                1 => PartitionedActionKind::CrossCellCanonicalPublishWithoutLocalAuthority,
                2 => PartitionedActionKind::UncachedRemoteAuthorityMutation,
                other => panic!("unknown control-plane action kind {other}"),
            };
            match host.admit_partitioned_action(now_tick, &kind) {
                Ok(_rcpt) => {
                    out.put_u8(RESP_OK);
                }
                Err(pv) => {
                    out.put_u8(RESP_ERR);
                    let is_ctrl_refusal = matches!(
                        pv,
                        PartitionViolation::ControlPlaneRequiredDuringPartition {
                            link_phase: ControlPlaneLinkPhase::Partitioned,
                            ..
                        }
                    );
                    out.put_bool(is_ctrl_refusal);
                    out.put_str(&format!("{pv:?}"));
                }
            }
        }
        OP_SETTLE_QUOTA_LEASE => {
            let spec = QuotaPartitionSettlementSpec {
                lease_id: QuotaLeaseId::from_bytes(r.take_fixed::<16>()),
                receipt_id: ReceiptId::from_bytes(r.take_fixed::<16>()),
                reconcile_seq: r.get_u64(),
                gateway_id: r.take_fixed::<16>(),
                attested_spent: r.get_u64(),
                unattested_inflight_loss: r.get_u64(),
                max_unreported_inflight_window: r.get_u64(),
            };
            if spec.unattested_inflight_loss > spec.max_unreported_inflight_window {
                out.put_u8(RESP_ERR);
                out.put_u8(1);
                return out.into_vec();
            }
            let mut ep = LocalCellHostPartitionEndpoint::new(host);
            match ep.settle_quarantined_quota_lease(spec) {
                Ok((sum, delta)) => {
                    out.put_u8(RESP_OK);
                    out.put_fixed(sum.lease_id.as_bytes());
                    out.put_fixed(sum.receipt_id.as_bytes());
                    out.put_u64(sum.prior_epoch.get());
                    out.put_u64(sum.face_value);
                    out.put_u64(sum.attested_spent);
                    out.put_u64(sum.refunded_to_available);
                    out.put_u64(sum.permanently_quarantined_loss);
                    out.put_u64(sum.max_unreported_inflight_window);
                    out.put_quota_state(sum.post_reconcile_state);
                    out.put_fixed(&sum.fence_barrier_ack_digest);
                    out.put_u64(delta);
                }
                Err(PartitionRecoveryError::BoundedLossViolation(_)) => {
                    out.put_u8(RESP_ERR);
                    out.put_u8(1);
                }
                Err(e) => {
                    out.put_u8(RESP_ERR);
                    out.put_u8(0);
                    out.put_str(&e.to_string());
                }
            }
        }
        OP_SETTLE_CAPACITY_LEASE => {
            let capacity_lease_id = CapacityLeaseId::from_bytes(r.take_fixed::<16>());
            let receipt_id = ReceiptId::from_bytes(r.take_fixed::<16>());
            let gateway_id = r.take_fixed::<16>();
            let target_node = r.get_cell_id();
            let target_node_boot_generation = r.get_gen();
            let capacity_epoch = CellEpoch::from_u64(r.get_u64()).expect("epoch");
            let fencing_token = CellFencingToken::from_u64(r.get_u64()).expect("token");
            let cleaned_amount = r.get_u64();
            let holder_processes_terminated = r.get_bool();
            let attestation_digest = r.take_fixed::<32>();
            let cleanup_attestation = CleanupAttestation::new(
                capacity_lease_id,
                target_node,
                target_node_boot_generation,
                capacity_epoch,
                fencing_token,
                cleaned_amount,
                holder_processes_terminated,
                attestation_digest,
            );
            let mut ep = LocalCellHostPartitionEndpoint::new(host);
            match ep.quarantine_and_reconcile_capacity_lease(CapacityPartitionSettlementSpec {
                capacity_lease_id,
                receipt_id,
                gateway_id,
                cleanup_attestation,
            }) {
                Ok(sum) => {
                    out.put_u8(RESP_OK);
                    out.put_fixed(sum.capacity_lease_id.as_bytes());
                    out.put_u64(sum.amount);
                    out.put_cap_state(sum.final_state);
                    out.put_fixed(&sum.fence_ack_digest);
                }
                Err(e) => {
                    out.put_u8(RESP_ERR);
                    out.put_str(&e.to_string());
                }
            }
        }
        OP_SETTLE_DEVICE_LEASE => {
            let device_lease_id = ExclusiveDeviceLeaseId::from_bytes(r.take_fixed::<16>());
            let receipt_id = ReceiptId::from_bytes(r.take_fixed::<16>());
            let gateway_id = r.take_fixed::<16>();
            let reset_rcpt_id = ReceiptId::from_bytes(r.take_fixed::<16>());
            let holder_node = r.get_cell_id();
            let holder_node_boot_generation = r.get_gen();
            let exclusivity_epoch = CellEpoch::from_u64(r.get_u64()).expect("epoch");
            let fencing_token = CellFencingToken::from_u64(r.get_u64()).expect("token");
            let reset_generation = r.get_gen();
            let reset_receipt = DeviceResetReceipt::new(
                reset_rcpt_id,
                device_lease_id,
                holder_node,
                holder_node_boot_generation,
                exclusivity_epoch,
                fencing_token,
                reset_generation,
            );
            let mut ep = LocalCellHostPartitionEndpoint::new(host);
            match ep.quarantine_and_reconcile_device_lease(DevicePartitionSettlementSpec {
                device_lease_id,
                receipt_id,
                gateway_id,
                reset_receipt,
            }) {
                Ok(sum) => {
                    out.put_u8(RESP_OK);
                    out.put_fixed(sum.device_lease_id.as_bytes());
                    out.put_fixed(sum.device_id.as_bytes());
                    out.put_u64(sum.exclusivity_epoch.get());
                    out.put_u64(sum.reset_generation.get());
                    out.put_dev_state(sum.final_state);
                    out.put_fixed(&sum.fence_ack_digest);
                }
                Err(e) => {
                    out.put_u8(RESP_ERR);
                    out.put_str(&e.to_string());
                }
            }
        }
        OP_EXPORT_WRITER_PRINCIPAL => {
            let req = host
                .semantic_writer_bootstrap_request()
                .expect("export writer principal");
            out.put_u8(RESP_OK);
            out.put_bootstrap_principal(&req);
        }
        OP_REGISTER_PEER_PRINCIPAL => {
            let req = r.get_bootstrap_principal();
            let mut ep = LocalCellHostPartitionEndpoint::new(host);
            match ep.register_peer_principal(req) {
                Ok(()) => out.put_u8(RESP_OK),
                Err(e) => {
                    out.put_u8(RESP_ERR);
                    out.put_str(&e.to_string());
                }
            }
        }
        OP_EXPORT_SIGNED_VECTOR_CKPT => {
            let trust = r.take_fixed::<32>();
            let signed_at_ms = r.get_u64();
            match host.export_signed_vector_checkpoint(trust, signed_at_ms) {
                Ok(ckpt) => {
                    out.put_u8(RESP_OK);
                    out.put_bytes(&encode_signed_vector_checkpoint(&ckpt));
                }
                Err(e) => {
                    out.put_u8(RESP_ERR);
                    out.put_str(&e.to_string());
                }
            }
        }
        OP_VERIFY_PEER_VECTOR_CKPT => {
            let ckpt = decode_signed_vector_checkpoint(&r.get_bytes()).expect("decode ckpt");
            let mut ep = LocalCellHostPartitionEndpoint::new(host);
            match ep.verify_peer_vector_checkpoint(&ckpt) {
                Ok(()) => out.put_u8(RESP_OK),
                Err(e) => {
                    out.put_u8(RESP_ERR);
                    out.put_str(&e.to_string());
                }
            }
        }
        OP_EXPORT_XDOM_EVENTS => {
            let trust = r.take_fixed::<32>();
            let mut ep = LocalCellHostPartitionEndpoint::new(host);
            match ep.export_cross_domain_events(trust) {
                Ok(items) => {
                    out.put_u8(RESP_OK);
                    out.put_bytes(&encode_cross_domain_event_items(&items));
                }
                Err(e) => {
                    out.put_u8(RESP_ERR);
                    out.put_str(&e.to_string());
                }
            }
        }
        OP_CONVERGE_XDOM_EVENTS => {
            let local_trust = r.take_fixed::<32>();
            let items = decode_cross_domain_event_items(&r.get_bytes()).expect("decode xdom items");
            let mut ep = LocalCellHostPartitionEndpoint::new(host);
            match ep.converge_cross_domain_events(local_trust, &items) {
                Ok(snap) => {
                    out.put_u8(RESP_OK);
                    out.put_u64(snap.imported_event_ids.len() as u64);
                    for eid in &snap.imported_event_ids {
                        out.put_fixed(eid.as_bytes());
                    }
                    out.put_u64(snap.skipped_existing_event_ids.len() as u64);
                    for eid in &snap.skipped_existing_event_ids {
                        out.put_fixed(eid.as_bytes());
                    }
                    out.put_u64(snap.audit_record_count);
                    out.put_vector_checkpoint(&snap.vector_checkpoint);
                }
                Err(e) => {
                    out.put_u8(RESP_ERR);
                    out.put_str(&e.to_string());
                }
            }
        }
        OP_APPEND_SEMANTIC_RECEIPT => {
            let seed = r.get_u8();
            let app_id = ApplicationId::from_bytes(r.take_fixed::<16>());
            let _task_id = TaskId::from_bytes(r.take_fixed::<16>());
            let _attempt_id = TaskAttemptId::from_bytes(r.take_fixed::<16>());
            let op_id = OperationId::from_bytes(r.take_fixed::<16>());
            let artifact_id = ArtifactId::from_bytes(r.take_fixed::<16>());
            let revision = r.get_u64();
            let digest = ContentDigest::from_bytes(r.take_fixed::<32>());
            let _appended_at_ms = r.get_u64();
            let rcpt_id = ReceiptId::from_bytes([seed.wrapping_add(7); 16]);
            let appended = host
                .runtime()
                .semantic_writer()
                .append_operation_receipt(
                    host.runtime(),
                    &OperationReceiptFact {
                        application_id: app_id,
                        package_id: PackageId::from_bytes([seed.wrapping_add(8); 16]),
                        package_version: 1,
                        installation_generation: Generation::INITIAL,
                        entry_name: "bin/road-c001-worker",
                        artifact_id,
                        payload_revision: revision,
                        payload_digest: *digest.as_bytes(),
                        payload_size_bytes: 64,
                        operation_id: op_id,
                        operation_generation: Generation::INITIAL,
                        callback_id: CallbackId::from_bytes([seed.wrapping_add(9); 16]),
                        outcome: CompletionOutcome::Completed {
                            receipt_id: rcpt_id,
                        },
                        terminal_state: OperationState::Completed {
                            receipt_id: rcpt_id,
                        },
                    },
                )
                .expect("append semantic operation receipt fact");
            out.put_u8(RESP_OK);
            out.put_fixed(appended.event_id.as_bytes());
        }
        OP_PUBLISH_LOCAL_ARTIFACT => {
            let artifact_id = ArtifactId::from_bytes(r.take_fixed::<16>());
            let seed = r.get_u8();
            let content_type = r.get_str();
            let create_new = r.get_bool();
            let expected_head_revision = r.get_u64();
            let bytes = r.get_bytes();
            let created_at_ms = r.get_u64();
            if create_new {
                host.runtime()
                    .artifacts
                    .create_artifact(CreateArtifactSpec {
                        artifact_id,
                        idempotency_key: seeded_key(seed, 181),
                        content_type,
                        application_id: None,
                        owner: None,
                        created_at_ms,
                    })
                    .expect("create local artifact");
            }
            let decision = host
                .runtime()
                .artifacts
                .put_revision(PutRevisionRequest {
                    artifact_id,
                    expected_head_revision,
                    bytes: &bytes,
                    created_at_ms,
                    provenance: provenance_triple(seed),
                })
                .expect("put artifact revision");
            let rec = decision.record();
            out.put_u8(RESP_OK);
            out.put_u64(rec.revision);
            out.put_fixed(rec.digest.as_bytes());
        }
        OP_EXPORT_ARTIFACT_PKG => {
            let artifact_id = ArtifactId::from_bytes(r.take_fixed::<16>());
            let now_ms = r.get_u64();
            let mut ep = LocalCellHostPartitionEndpoint::new(host);
            match ep.export_artifact_package(artifact_id, now_ms) {
                Ok(pkg) => {
                    out.put_u8(RESP_OK);
                    out.put_bytes(&encode_artifact_transfer_package(&pkg));
                }
                Err(e) => {
                    out.put_u8(RESP_ERR);
                    out.put_str(&e.to_string());
                }
            }
        }
        OP_IMPORT_ARTIFACT_PKG => {
            let pol_len = usize::try_from(r.get_u64()).expect("pol len");
            let mut policies = Vec::with_capacity(pol_len);
            for _ in 0..pol_len {
                policies.push(r.get_sync_policy());
            }
            let pkg = decode_artifact_transfer_package(&r.get_bytes()).expect("decode pkg");
            let commit_ctx = ArtifactImportCommitContext {
                task_id: TaskId::from_bytes(r.take_fixed::<16>()),
                permit_id: CommitPermitId::from_bytes(r.take_fixed::<16>()),
                write_set_root: ContentDigest::from_bytes(r.take_fixed::<32>()),
                base_idempotency_key: IdempotencyKey::from_bytes(r.take_fixed::<16>()),
            };
            let idempotency_key = IdempotencyKey::from_bytes(r.take_fixed::<16>());
            let synced_at_ms = r.get_u64();
            let mut ep = LocalCellHostPartitionEndpoint::new(host);
            match ep.import_artifact_package(
                &policies,
                &pkg,
                commit_ctx,
                idempotency_key,
                synced_at_ms,
            ) {
                Ok(summary) => {
                    out.put_u8(RESP_OK);
                    out.put_artifact_import_summary(&summary);
                }
                Err(e) => {
                    out.put_u8(RESP_ERR);
                    out.put_str(&e.to_string());
                }
            }
        }
        OP_INSPECT_LOCAL_ARTIFACT => {
            let artifact_id = ArtifactId::from_bytes(r.take_fixed::<16>());
            let head = host
                .runtime()
                .artifacts
                .resolve_head(artifact_id, u64::MAX)
                .expect("resolve artifact head");
            out.put_u8(RESP_OK);
            out.put_opt(head, WireWriter::put_head);
        }
        OP_RECORD_CHECKPOINT => {
            let count = usize::try_from(r.get_u64()).expect("count");
            let mut entries = Vec::with_capacity(count);
            for _ in 0..count {
                entries.push(r.get_bytes());
            }
            let prefix = CheckpointPrefix::from_entries(entries);
            let mut ep = LocalCellHostPartitionEndpoint::new(host);
            match ep.refresh_and_record_checkpoint(&prefix) {
                Ok(None) => {
                    out.put_u8(RESP_OK);
                    out.put_bool(false);
                }
                Ok(Some(rec)) => {
                    out.put_u8(RESP_OK);
                    out.put_bool(true);
                    out.put_fixed(rec.cell.as_bytes());
                    out.put_u64(rec.epoch.get());
                    out.put_u64(rec.fencing_token.get());
                    out.put_u64(rec.heartbeat);
                    out.put_u64(rec.sequence);
                    out.put_u32(rec.os_process_id);
                    out.put_fixed(&rec.digest_bytes);
                    out.put_bool(rec.digest_verified);
                }
                Err(e) => {
                    out.put_u8(RESP_ERR);
                    out.put_str(&e.to_string());
                }
            }
        }
        OP_SWEEP_AND_WAL_LOSS_PROBES => {
            let now_tick = r.get_u64();
            let wal_probe_lease = QuotaLeaseId::from_bytes(r.take_fixed::<16>());
            let guessed_remaining = r.get_u64();
            let do_wal_boot_fence = r.get_bool();
            let lost_boot_gen = r.get_u64();
            let reconnected_boot_gen = r.get_u64();
            let stale_epoch_probe = r.get_opt(|r| {
                (
                    decode_cell_fence(&r.take_fixed::<40>()).expect("stale fence"),
                    QuotaLeaseId::from_bytes(r.take_fixed::<16>()),
                    r.get_u64(),
                )
            });

            let swept = host.sweep_expired_quota_leases(LeaseInstant::new(now_tick));
            let guessed_refused = matches!(
                host.refuse_guessed_remaining_on_wal_loss(wal_probe_lease, guessed_remaining),
                Err(QuotaWalLossError::GuessedRemainingForbidden { .. })
            );
            let (wal_fenced_epoch, wal_quarantined_count) = if do_wal_boot_fence {
                let (adv, ids) = host
                    .fence_boot_generation_on_wal_loss(
                        Generation::new(NonZeroU64::new(lost_boot_gen).expect("gen")),
                        Generation::new(NonZeroU64::new(reconnected_boot_gen).expect("gen")),
                    )
                    .expect("fence boot generation on WAL loss");
                (adv.fence.epoch().get(), ids.len() as u64)
            } else {
                (host.fence().epoch().get(), 0)
            };
            let stale_spend_refused = stale_epoch_probe.is_some_and(|(stale_fence, lid, hw)| {
                matches!(
                    host.report_quota_usage_against(&stale_fence, lid, hw),
                    Err(SliceKError::LeaseLedger(_))
                )
            });

            out.put_u8(RESP_OK);
            out.put_u64(swept.len() as u64);
            for lid in swept {
                out.put_fixed(lid.as_bytes());
            }
            out.put_bool(guessed_refused);
            out.put_u64(wal_fenced_epoch);
            out.put_u64(wal_quarantined_count);
            out.put_bool(stale_spend_refused);
        }
        OP_RECONCILE_QUOTA_DIRECT => {
            let lease_id = QuotaLeaseId::from_bytes(r.take_fixed::<16>());
            let receipt_id = ReceiptId::from_bytes(r.take_fixed::<16>());
            let reconcile_seq = r.get_u64();
            let gateway_id = r.take_fixed::<16>();
            let settled_spent = r.get_u64();
            let returned_to_available = r.get_u64();
            let remaining_quarantined = r.get_u64();
            let tamper_ack = r.get_bool();

            let lease = host.query_quota_lease(lease_id).expect("query lease");
            let mut ack = FenceBarrierAck::for_quota(&lease, gateway_id, settled_spent);
            if tamper_ack {
                // Build an ACK bound to a forged gateway/high-water to prove fail-closed rejection.
                ack =
                    FenceBarrierAck::for_quota(&lease, [0xFF; 16], settled_spent.saturating_add(7));
            }
            let rcpt = QuotaReconciliationReceipt::new(
                receipt_id,
                lease_id,
                reconcile_seq,
                lease.cell(),
                lease.node_boot_generation(),
                lease.epoch(),
                lease.fencing_token(),
                lease.face_value(),
                settled_spent,
                settled_spent,
                returned_to_available,
                remaining_quarantined,
                ack,
            );
            match host.reconcile_quota_lease(rcpt) {
                Ok(settled) => {
                    out.put_u8(RESP_OK);
                    out.put_u64(host.quota_available());
                    out.put_quota_state(settled.state());
                    out.put_u64(settled.spent());
                    out.put_u64(settled.returned());
                    out.put_u64(settled.remaining());
                }
                Err(e) => {
                    out.put_u8(RESP_ERR);
                    out.put_str(&format!("{e:?}"));
                }
            }
        }
        OP_NOTIFY_PUBLISH_AND_POLL => {
            let seed = r.get_u8();
            let topic_name = r.get_str();
            let subscriber_bytes = r.take_fixed::<16>();
            let subscribed_at_ms = r.get_u64();
            let app_id = ApplicationId::from_bytes(r.take_fixed::<16>());
            let task_id = TaskId::from_bytes(r.take_fixed::<16>());
            let _attempt_id = r.get_opt(|r| TaskAttemptId::from_bytes(r.take_fixed::<16>()));
            let transition = match r.get_u8() {
                0 => TaskTransitionKind::Proposed,
                1 => TaskTransitionKind::Running,
                2 => TaskTransitionKind::WaitingConsent,
                3 => TaskTransitionKind::Completed,
                4 => TaskTransitionKind::Failed,
                5 => TaskTransitionKind::Cancelled,
                other => panic!("unknown TaskTransitionKind {other}"),
            };
            let summary_digest = r.take_fixed::<32>();
            let occurred_at_ms = r.get_u64();
            let publish_key = IdempotencyKey::from_bytes(r.take_fixed::<16>());
            let ack_up_to = r.get_opt(WireReader::get_u64);

            let sub = host
                .bootstrap_and_subscribe_notification_topic(
                    &NotificationTopicConfig::named(
                        topic_name.into_bytes(),
                        ResourceAccountId::from_bytes([seed; 16]),
                    ),
                    SubscriberKey::from_bytes(subscriber_bytes),
                    subscribed_at_ms,
                )
                .expect("bootstrap and subscribe notification topic");
            let env = TaskNotificationEnvelope {
                task_id,
                application_id: app_id,
                task_generation: Generation::INITIAL,
                transition,
                occurred_at_ms,
                summary_digest,
                requires_user_action: matches!(transition, TaskTransitionKind::WaitingConsent),
            };
            let decision = host
                .publish_task_notification(sub.topic_id(), &env, publish_key)
                .expect("publish task notification");
            let (is_replayed, pub_seq) = match decision {
                NotificationPublishDecision::Published(rcpt) => {
                    (false, rcpt.publication.channel_sequence)
                }
                NotificationPublishDecision::Replayed(rcpt) => {
                    (true, rcpt.publication.channel_sequence)
                }
            };
            let polled = host
                .poll_task_notifications(sub.notification_id(), 32)
                .expect("poll task notifications");
            if let Some(seq) = ack_up_to {
                host.ack_notification(AckNotificationRequest {
                    notification_id: sub.notification_id(),
                    up_to_sequence: seq,
                    acked_at_ms: occurred_at_ms + 1,
                })
                .expect("ack notification");
            }
            out.put_u8(RESP_OK);
            out.put_fixed(sub.topic_id().as_bytes());
            out.put_fixed(sub.notification_id().as_bytes());
            out.put_bool(is_replayed);
            out.put_u64(pub_seq);
            out.put_u64(polled.len() as u64);
            for item in polled {
                out.put_u64(item.sequence);
                out.put_fixed(item.envelope.task_id.as_bytes());
                out.put_fixed(&item.envelope.summary_digest);
            }
        }
        OP_ISSUE_ACTION_TOKEN => {
            let _topic_id = TopicId::from_bytes(r.take_fixed::<16>());
            let notification_id = NotificationId::from_bytes(r.take_fixed::<16>());
            let delivery_sequence = r.get_u64();
            let app_id = ApplicationId::from_bytes(r.take_fixed::<16>());
            let task_id = TaskId::from_bytes(r.take_fixed::<16>());
            let _attempt_id = r.get_opt(|r| TaskAttemptId::from_bytes(r.take_fixed::<16>()));
            let _capability_handle = r.take_fixed::<16>();
            let secret_nonce = r.take_fixed::<32>();
            let _valid_from_ms = r.get_u64();
            let expires_at_ms = r.get_u64();
            let idempotency_key = IdempotencyKey::from_bytes(r.take_fixed::<16>());
            let issued_at_ms = r.get_u64();

            let decision = host
                .issue_notification_action_token(IssueNotificationActionTokenRequest {
                    notification_id,
                    application_id: app_id,
                    task_id,
                    delivery_sequence,
                    action_code: 1,
                    secret_nonce,
                    issued_at_ms,
                    expires_at_ms,
                    idempotency_key,
                })
                .expect("issue action token");
            let (replayed, issued) = match decision {
                IssueActionTokenDecision::Issued(tok) => (false, tok),
                IssueActionTokenDecision::Replayed(tok) => (true, tok),
            };
            out.put_u8(RESP_OK);
            out.put_bool(replayed);
            out.put_fixed(issued.token_id.as_bytes());
            out.put_fixed(&issued.secret_proof);
        }
        OP_REDEEM_ACTION_TOKEN => {
            let token_id = ActionTokenId::from_bytes(r.take_fixed::<16>());
            let presented_proof = r.take_fixed::<32>();
            let caller_notification_id = NotificationId::from_bytes(r.take_fixed::<16>());
            let app_id = ApplicationId::from_bytes(r.take_fixed::<16>());
            let task_id = TaskId::from_bytes(r.take_fixed::<16>());
            let action_code = r.get_u16();
            let redeemed_at_ms = r.get_u64();

            match host.redeem_notification_action_token(RedeemNotificationActionRequest {
                token_id,
                presented_proof,
                caller_notification_id,
                caller_application_id: app_id,
                caller_task_id: task_id,
                action_code,
                redeemed_at_ms,
            }) {
                Ok(rcpt) => {
                    out.put_u8(RESP_OK);
                    out.put_fixed(rcpt.receipt_id.as_bytes());
                    out.put_u64(rcpt.delivery_sequence);
                }
                Err(SliceKError::Notify(ne)) => {
                    out.put_u8(RESP_ERR);
                    let code = match ne {
                        NotifyError::ActionTokenAlreadyRedeemed { .. } => {
                            NOTIFY_ERR_ALREADY_REDEEMED
                        }
                        NotifyError::ActionCrossApplicationSpoof { .. } => {
                            NOTIFY_ERR_CROSS_APP_SPOOF
                        }
                        NotifyError::ActionBindingMismatch(_) => NOTIFY_ERR_BINDING_MISMATCH,
                        NotifyError::ActionTokenProofInvalid(_) => NOTIFY_ERR_PROOF_INVALID,
                        _ => NOTIFY_ERR_OTHER,
                    };
                    out.put_u8(code);
                    out.put_str(&format!("{ne:?}"));
                }
                Err(other) => {
                    out.put_u8(RESP_ERR);
                    out.put_u8(NOTIFY_ERR_OTHER);
                    out.put_str(&other.to_string());
                }
            }
        }
        OP_QUERY_OFFLINE_HISTORY => {
            let topic_id = TopicId::from_bytes(r.take_fixed::<16>());
            let after_sequence = r.get_u64();
            let limit = usize::try_from(r.get_u64()).expect("limit");
            let page = host
                .query_offline_notification_history(topic_id, after_sequence, limit)
                .expect("query offline notification history");
            out.put_u8(RESP_OK);
            out.put_u64(page.entries.len() as u64);
            for entry in &page.entries {
                let meta = entry.task_metadata.expect("task history metadata");
                out.put_u64(entry.channel_sequence);
                out.put_fixed(meta.task_id.as_bytes());
                out.put_fixed(&meta.summary_digest);
            }
            out.put_opt(page.next_after_sequence, WireWriter::put_u64);
            out.put_bool(page.has_more);
        }
        OP_QUERY_CHECKPOINT_SEARCH => {
            let ckpt = decode_signed_vector_checkpoint(&r.get_bytes()).expect("decode ckpt");
            let limit = usize::try_from(r.get_u64()).expect("limit");
            let verify_first_candidate = r.get_bool();
            let tamper_non_auth_probe = r.get_bool();
            let selector = AssertionSelector::new(limit);
            match host.query_at_signed_checkpoint(&ckpt, &selector, None) {
                Ok(page) => {
                    let mut verified_ok = false;
                    let mut tampered_rejected = false;
                    let mut control_rejected = false;
                    if let Some(first) = page.items.first() {
                        assert_eq!(
                            first.use_restrictions,
                            SearchUseRestrictions::NON_AUTHORITATIVE
                        );
                        control_rejected = matches!(
                            first.enforce_not_control_authoritative(
                                ControlConsumerKind::TaskHeadCommit
                            ),
                            Err(SearchError::ControlAuthoritativeUseForbidden(
                                ControlConsumerKind::TaskHeadCommit
                            ))
                        );
                        if verify_first_candidate {
                            let verified = host
                                .verify_candidate_against_checkpoint(first, &ckpt)
                                .expect("verify candidate against checkpoint");
                            verified_ok = verified.event_id == first.hit.event_id
                                && verified.checkpoint_digest == ckpt.checkpoint_digest;
                        }
                        if tamper_non_auth_probe {
                            let mut forged = first.clone();
                            forged.use_restrictions = SearchUseRestrictions {
                                forbid_capability_decision: false,
                                ..SearchUseRestrictions::NON_AUTHORITATIVE
                            };
                            tampered_rejected = matches!(
                                host.verify_candidate_against_checkpoint(&forged, &ckpt),
                                Err(SliceKError::Search(
                                    SearchError::ControlAuthoritativeUseForbidden(
                                        ControlConsumerKind::CandidateDiscovery
                                    )
                                ))
                            );
                        }
                    }
                    out.put_u8(RESP_OK);
                    out.put_u64(page.items.len() as u64);
                    for item in &page.items {
                        out.put_assertion_hit(&item.hit);
                    }
                    out.put_bool(verified_ok);
                    out.put_bool(tampered_rejected);
                    out.put_bool(control_rejected);
                }
                Err(e) => {
                    out.put_u8(RESP_ERR);
                    out.put_str(&e.to_string());
                }
            }
        }
        OP_EXPORT_SEARCH_SHARD => {
            let ckpt_opt = r.get_opt(|r| {
                decode_signed_vector_checkpoint(&r.get_bytes()).expect("decode shard ckpt")
            });
            let limit = usize::try_from(r.get_u64()).expect("limit");
            let selector = AssertionSelector::new(limit);
            let shard = host
                .export_search_shard(&selector, ckpt_opt.as_ref())
                .expect("export search shard");
            out.put_u8(RESP_OK);
            match shard {
                CellSearchShardOutcome::Available {
                    cell_domain_id,
                    hits,
                    checkpoint_digest,
                } => {
                    out.put_fixed(&cell_domain_id);
                    out.put_opt(checkpoint_digest, |w, d| w.put_fixed(&d));
                    out.put_u64(hits.len() as u64);
                    for h in &hits {
                        out.put_assertion_hit(h);
                    }
                }
                CellSearchShardOutcome::Failed { reason, .. } => {
                    panic!("expected local Available search shard, got Failed({reason:?})");
                }
            }
        }
        OP_SEED_TASK_TERM1 => {
            let shared_task_db = PathBuf::from(r.get_str());
            let shared_id_dir = PathBuf::from(r.get_str());
            let task_id = TaskId::from_bytes(r.take_fixed::<16>());
            let lease_at_ms = i64::try_from(r.get_u64()).expect("lease_at_ms fits i64");
            let lease_ttl_ms = i64::try_from(r.get_u64()).expect("lease_ttl_ms fits i64");

            let authority =
                SqliteTaskAuthority::open(&shared_task_db).expect("open shared task db");
            let mut holder_bytes = [0_u8; 16];
            holder_bytes[..4].copy_from_slice(&std::process::id().to_be_bytes());
            holder_bytes[15] = 0xA1;
            let holder_id = ProcessId::from_bytes(holder_bytes);

            let lease_one = match authority
                .acquire_authority_lease(AuthorityLeaseRequest {
                    holder_id,
                    idempotency_key: IdempotencyKey::from_bytes([0xC1; 16]),
                    requested_at_ms: lease_at_ms,
                    ttl_ms: lease_ttl_ms,
                })
                .expect("acquire term-1 authority lease")
            {
                AuthorityLeaseDecision::Acquired(rec) => rec,
                other => panic!("expected Acquired term-1 lease, got {other:?}"),
            };
            assert_eq!(lease_one.term, 1);

            authority
                .register_task(nlos_task::TaskSpec {
                    application_id: None,
                    plan_revision: None,
                    task_id,
                    task_generation: Generation::INITIAL,
                    registered_at_ms: lease_at_ms + 1,
                })
                .expect("register task on term-1");
            let attempt = nlos_task::AttemptSpec {
                task_id,
                attempt_id: TaskAttemptId::from_bytes([0xC2; 16]),
                attempt_generation: Generation::INITIAL,
                snapshot: SnapshotBundle {
                    snapshot_id: TaskSnapshotId::from_bytes([0xC3; 16]),
                    snapshot_digest: [0xC4; 32],
                    expected_head_commit_seq: 0,
                    effect_history_root: empty_effect_history_root(),
                    retry_fence_epoch: 0,
                },
                cancellation_scope_id: CancellationScopeId::from_bytes([0xC5; 16]),
                cancellation_generation: Generation::INITIAL,
                idempotency_key: IdempotencyKey::from_bytes([0xC6; 16]),
                registered_at_ms: lease_at_ms + 2,
            };
            authority
                .register_attempt(attempt)
                .expect("register attempt");
            let permit = match authority
                .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
                    permit: PermitRequest {
                        task_id,
                        attempt_id: attempt.attempt_id,
                        attempt_generation: attempt.attempt_generation,
                        write_set_root: [0xC7; 32],
                        planned_effects: Vec::new(),
                        idempotency_key: IdempotencyKey::from_bytes([0xC8; 16]),
                        valid_until_ms: 50_000,
                        requested_at_ms: lease_at_ms + 3,
                    },
                    lease: lease_one,
                })
                .expect("issue term-1 permit")
            {
                PermitDecision::Issued(p) => *p,
                other => panic!("expected Issued permit, got {other:?}"),
            };
            let registry_binding = permit
                .participant_registry_binding
                .expect("permit carries participant registry binding");
            authority
                .finalize_commit_v3_with_authority_lease(AuthorityLeaseFinalizeRequest {
                    finalize: FinalizeRequestV3 {
                        base: FinalizeRequest {
                            task_id,
                            attempt_id: attempt.attempt_id,
                            attempt_generation: attempt.attempt_generation,
                            permit_id: permit.permit_id,
                            new_effect_history_root: [0; 32],
                            new_retry_fence_epoch: 0,
                            finalized_at_ms: lease_at_ms + 4,
                        },
                        required_satisfaction: Vec::new(),
                        fenced_participant_digest: [0; 32],
                    },
                    lease: lease_one,
                })
                .expect("finalize term-1 commit");

            // Bootstrap Cell A's BarrierObservationSigning principal in the shared identity store.
            let shared_identity =
                IdentityAuthority::open(&shared_id_dir).expect("open shared identity root");
            let sk = SigningKey::from_bytes(&[BARRIER_SIGNER_SEED; 32]);
            let binding = shared_identity
                .bootstrap_principal(BootstrapPrincipalRequest {
                    principal_profile_digest: [BARRIER_SIGNER_SEED.wrapping_add(1); 32],
                    control_domain_policy_digest: [BARRIER_SIGNER_SEED.wrapping_add(2); 32],
                    public_key: sk.verifying_key().to_bytes(),
                    key_purpose: KeyPurpose::BarrierObservationSigning,
                    key_valid_from_ms: 0,
                    key_valid_until_ms: 10_000_000,
                    idempotency_key: IdempotencyKey::from_bytes(
                        [BARRIER_SIGNER_SEED.wrapping_add(3); 16],
                    ),
                    created_at_ms: 0,
                })
                .expect("bootstrap barrier signer")
                .binding();

            // Persist signer metadata into shared_id_dir/barrier_signer.bin so a cold-restarted
            // Cell A OS process can continue signing barrier observations with the same identity!
            let mut meta = WireWriter::new();
            meta.put_fixed(binding.principal_id.as_bytes());
            meta.put_fixed(binding.control_domain_id.as_bytes());
            meta.put_fixed(binding.key_id.as_bytes());
            write_atomic_file(&shared_id_dir.join("barrier_signer.bin"), &meta.into_vec());

            *task_term1 = Some(WorkerTaskTerm1State {
                lease_one,
                registry_binding,
            });

            out.put_u8(RESP_OK);
            out.put_u64(lease_one.term as u64);
            out.put_fixed(&lease_one.fencing_token);
            out.put_fixed(holder_id.as_bytes());
        }
        OP_SIGN_BARRIER_OBSERVATION => {
            let takeover_receipt_id = ReceiptId::from_bytes(r.take_fixed::<16>());
            let ptype = match r.get_u8() {
                1 => ParticipantType::TaskStore,
                2 => ParticipantType::ArtifactHead,
                3 => ParticipantType::SemanticAdmission,
                4 => ParticipantType::ChannelTopic,
                5 => ParticipantType::DriverGateway,
                6 => ParticipantType::ResourceLedger,
                7 => ParticipantType::ProcessBinding,
                8 => ParticipantType::OperationBinding,
                other => panic!("unknown ParticipantType {other}"),
            };
            let participant = ParticipantRecord {
                participant_type: ptype,
                participant_id: TaskParticipantId::from_bytes(r.take_fixed::<16>()),
                participant_generation: r.get_gen(),
                admission_receipt_id: ReceiptId::from_bytes(r.take_fixed::<16>()),
            };
            let fence_set_root = r.take_fixed::<32>();

            // Re-bootstrap idempotently against a local/shared identity or derive from seed:
            let sk = SigningKey::from_bytes(&[BARRIER_SIGNER_SEED; 32]);
            // Note: we read the signer's PrincipalId/ControlDomainId/KeyId from the deterministic
            // bootstrap on a scratch/shared identity view or host's identity if same request:
            // `IdentityAuthority::bootstrap_principal` derives deterministic IDs from the request!
            let binding = host
                .runtime()
                .identity
                .bootstrap_principal(BootstrapPrincipalRequest {
                    principal_profile_digest: [BARRIER_SIGNER_SEED.wrapping_add(1); 32],
                    control_domain_policy_digest: [BARRIER_SIGNER_SEED.wrapping_add(2); 32],
                    public_key: sk.verifying_key().to_bytes(),
                    key_purpose: KeyPurpose::BarrierObservationSigning,
                    key_valid_from_ms: 0,
                    key_valid_until_ms: 10_000_000,
                    idempotency_key: IdempotencyKey::from_bytes(
                        [BARRIER_SIGNER_SEED.wrapping_add(3); 16],
                    ),
                    created_at_ms: 0,
                })
                .expect("deterministic barrier signer binding")
                .binding();

            let remote_receipt_id =
                deterministic_barrier_receipt_id(takeover_receipt_id, &participant);
            let barrier_digest = deterministic_barrier_digest(takeover_receipt_id, &participant);
            let msg = barrier_observation_signature_message(
                takeover_receipt_id,
                &participant,
                remote_receipt_id,
                barrier_digest,
                fence_set_root,
            );
            let sig = sk.sign(&msg).to_bytes();

            out.put_u8(RESP_OK);
            out.put_fixed(remote_receipt_id.as_bytes());
            out.put_fixed(&barrier_digest);
            out.put_fixed(binding.principal_id.as_bytes());
            out.put_fixed(binding.control_domain_id.as_bytes());
            out.put_fixed(binding.key_id.as_bytes());
            out.put_fixed(&sig);
        }
        OP_DRIVE_TASK_TAKEOVER => {
            let shared_task_db = PathBuf::from(r.get_str());
            let shared_id_dir = PathBuf::from(r.get_str());
            let peer_mailbox_dir = PathBuf::from(r.get_str());
            let source_cell = r.get_cell_id();
            let task_id = TaskId::from_bytes(r.take_fixed::<16>());
            let clock_start_ms = i64::try_from(r.get_u64()).expect("clock_start_ms fits i64");

            let authority =
                SqliteTaskAuthority::open(&shared_task_db).expect("open shared task db");
            let shared_identity =
                IdentityAuthority::open(&shared_id_dir).expect("open shared identity root");
            let mut barrier_source = IpcBarrierSource { peer_mailbox_dir };

            let mut holder_bytes = [0_u8; 16];
            holder_bytes[..4].copy_from_slice(&std::process::id().to_be_bytes());
            holder_bytes[15] = 0xB2;
            let successor_holder_id = ProcessId::from_bytes(holder_bytes);

            let mut current_clock = clock_start_ms;
            let mut clock_fn = || {
                let v = current_clock;
                current_clock += 10;
                v
            };

            let fence = host.fence();
            let directory = host.cell_directory().expect("Cell B is federated");
            let fed_ctx = TakeoverFederationContext {
                directory,
                source_cell,
                target_cell: fence.identity(),
                checkpoint_boot_generation: fence.node_boot_generation(),
                checkpoint_epoch: fence.epoch(),
                checkpoint_fencing_token: fence.fencing_token(),
            };

            let (outcome, fed_binding) = drive_takeover_with_federation_binding(
                CrossCellTakeoverRequest {
                    store: &authority,
                    identity: &shared_identity,
                    barrier_source: &mut barrier_source,
                    task_id,
                    successor_holder_id,
                    lease_idempotency_key: IdempotencyKey::from_bytes([0xD8; 16]),
                    lease_ttl_ms: 100_000,
                    now_ms: &mut clock_fn,
                },
                ProductionTakeoverOptions::default(),
                fed_ctx,
            )
            .expect("drive cross-cell task takeover with federation binding");

            assert_eq!(outcome.successor_lease.term, 2);
            assert_eq!(
                outcome.takeover_receipt_completed.barrier_state,
                AuthorityTakeoverReceiptState::Complete
            );
            assert_eq!(
                outcome.active_assignment.state,
                AuthorityAssignmentState::Active
            );
            assert_eq!(
                outcome.reopened_registry.state,
                ParticipantRegistryState::Open
            );
            assert_eq!(
                fed_binding
                    .checkpoint_record
                    .fact()
                    .verify_digest(&fed_binding.checkpoint_prefix),
                CheckpointDigestVerification::Verified
            );

            // Second replay of the exact takeover receipt must fail closed (`TakeoverAlreadyComplete`
            // or `SuccessorIsIncumbentHolder`).
            let replay_res = drive_takeover_with_federation_binding(
                CrossCellTakeoverRequest {
                    store: &authority,
                    identity: &shared_identity,
                    barrier_source: &mut barrier_source,
                    task_id,
                    successor_holder_id,
                    lease_idempotency_key: IdempotencyKey::from_bytes([0xD9; 16]),
                    lease_ttl_ms: 100_000,
                    now_ms: &mut clock_fn,
                },
                ProductionTakeoverOptions::default(),
                fed_ctx,
            );
            let duplicate_takeover_refused = matches!(
                replay_res,
                Err(TakeoverDriveError::SuccessorIsIncumbentHolder
                    | TakeoverDriveError::TakeoverAlreadyComplete)
            );

            out.put_u8(RESP_OK);
            out.put_u64(outcome.successor_lease.term);
            out.put_fixed(&outcome.successor_lease.fencing_token);
            out.put_u64(outcome.reopened_registry.generation);
            out.put_u64(fed_binding.checkpoint_record.sequence());
            out.put_bool(duplicate_takeover_refused);
        }
        OP_PROBE_STALE_TASK_AUTHORITY => {
            let shared_task_db = PathBuf::from(r.get_str());
            let task_id = TaskId::from_bytes(r.take_fixed::<16>());
            let probe_at_ms = i64::try_from(r.get_u64()).expect("probe_at_ms fits i64");
            let st = task_term1.as_ref().expect("term-1 state must be seeded");

            let authority =
                SqliteTaskAuthority::open(&shared_task_db).expect("open shared task db");
            let prepare_fenced = matches!(
                authority.prepare_authority_takeover_fence(AuthorityLeaseTakeoverFenceRequest {
                    task_id,
                    expected_registry_binding: st.registry_binding,
                    lease: st.lease_one,
                    requested_at_ms: probe_at_ms,
                }),
                Err(TaskStoreError::AuthorityLeaseFenced)
            );

            let head = authority.inspect_task(task_id).expect("inspect task");
            let stale_attempt = nlos_task::AttemptSpec {
                task_id,
                attempt_id: TaskAttemptId::from_bytes([0xE1; 16]),
                attempt_generation: Generation::INITIAL,
                snapshot: SnapshotBundle {
                    snapshot_id: TaskSnapshotId::from_bytes([0xE2; 16]),
                    snapshot_digest: [0xE3; 32],
                    expected_head_commit_seq: head.head_commit_seq,
                    effect_history_root: head.head_effect_history_root,
                    retry_fence_epoch: head.retry_fence_epoch,
                },
                cancellation_scope_id: CancellationScopeId::from_bytes([0xE4; 16]),
                cancellation_generation: Generation::INITIAL,
                idempotency_key: IdempotencyKey::from_bytes([0xE5; 16]),
                registered_at_ms: probe_at_ms + 1,
            };
            authority
                .register_attempt(stale_attempt)
                .expect("register attempt for stale permit probe");
            let permit_fenced = matches!(
                authority.request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
                    permit: PermitRequest {
                        task_id,
                        attempt_id: stale_attempt.attempt_id,
                        attempt_generation: stale_attempt.attempt_generation,
                        write_set_root: [0xE6; 32],
                        planned_effects: Vec::new(),
                        idempotency_key: IdempotencyKey::from_bytes([0xE7; 16]),
                        valid_until_ms: 100_000,
                        requested_at_ms: probe_at_ms + 2,
                    },
                    lease: st.lease_one,
                }),
                Err(TaskStoreError::AuthorityLeaseFenced)
            );

            out.put_u8(RESP_OK);
            out.put_bool(prepare_fenced);
            out.put_bool(permit_fenced);
        }
        OP_SEED_MIGRATABLE_PROCESS => {
            let seed = r.get_u8();
            let with_device_lease = r.get_bool();
            let with_pinned_restart_image = r.get_bool();
            let crash_after_source_fence = r.get_bool();

            let task_id = TaskId::from_bytes([seed.wrapping_add(0x01); 16]);
            let attempt_id = TaskAttemptId::from_bytes([seed.wrapping_add(0x02); 16]);
            let binding = host
                .spawn_monitored_process(seed, task_id, attempt_id, Generation::INITIAL)
                .expect("spawn monitored source process");

            let scope_id = CancellationScopeId::from_bytes([seed.wrapping_add(0x03); 16]);
            let scope_gen = Generation::INITIAL;
            let fiber_id = ExecutionFiberId::from_bytes([seed.wrapping_add(0x04); 16]);
            let spawned_fiber = RuntimeAdapter::spawn_fiber(
                adapter,
                FiberSpec {
                    fiber_id,
                    fiber_generation: Generation::INITIAL,
                    agent_instance_id: binding.agent_instance_id,
                    agent_generation: binding.agent_instance_generation,
                    process_id: binding.process_id,
                    process_generation: binding.process_generation,
                    task_attempt_id: Some(attempt_id),
                    cancellation_scope_id: scope_id,
                    cancellation_generation: scope_gen,
                    resource_group_id: ResourceGroupId::from_bytes([0x71; 16]),
                    scheduler_domain_id: SchedulerDomainId::from_bytes(
                        *host.fence().identity().as_bytes(),
                    ),
                    deadline: None,
                },
                Box::pin(std::future::pending::<FiberExit>()),
            )
            .expect("spawn live source fiber");
            live_fibers.push(spawned_fiber);

            host.runtime()
                .process
                .register_fiber_incarnation(RegisterFiberIncarnationRequest {
                    process_id: binding.process_id,
                    expected_process_generation: binding.process_generation,
                    expected_process_fencing_token: binding.process_fencing_token,
                    binding: fiber_id,
                    idempotency_key: seeded_key(seed, 151),
                    registered_at_ms: binding.created_at_ms + 5,
                })
                .expect("register fiber incarnation on ProcessAuthority");

            // Register and dispatch one in-flight async driver operation to drain during Step 2.
            let op_handle = OperationHandle {
                operation_id: OperationId::from_bytes([seed.wrapping_add(0x05); 16]),
                generation: Generation::INITIAL,
            };
            host.runtime()
                .operations
                .register(OperationSpec {
                    operation_id: op_handle.operation_id,
                    generation: op_handle.generation,
                    owner_fiber: spawned_fiber,
                    cancellation_scope_id: scope_id,
                    cancellation_generation: scope_gen,
                })
                .expect("register inflight operation");
            let _ticket = host
                .runtime()
                .operations
                .dispatch(
                    op_handle,
                    CallbackId::from_bytes([seed.wrapping_add(0x06); 16]),
                )
                .expect("dispatch inflight operation");

            let source_quota_id = QuotaLeaseId::from_bytes([seed.wrapping_add(0x30); 16]);
            host.grant_quota_lease(source_quota_id, 300, None)
                .expect("grant source process quota lease");
            host.activate_quota_lease(source_quota_id)
                .expect("activate source process quota lease");
            host.report_quota_usage(source_quota_id, 80)
                .expect("spend 80 on source process quota lease");

            let source_cap_id = CapacityLeaseId::from_bytes([seed.wrapping_add(0x31); 16]);
            host.grant_capacity_lease(source_cap_id, 150)
                .expect("grant source process capacity lease");

            let source_dev_id = if with_device_lease {
                let did = ExclusiveDeviceLeaseId::from_bytes([seed.wrapping_add(0x32); 16]);
                host.grant_device_lease(did)
                    .expect("grant source process device lease");
                Some(did)
            } else {
                None
            };

            seed_source_capabilities_on_host(
                host,
                task_id,
                binding.process_generation,
                seed.wrapping_add(0x40),
            );

            let restart_artifact = if with_pinned_restart_image {
                let art_id = ArtifactId::from_bytes([seed.wrapping_add(0x07); 16]);
                let (rev, dig) = publish_restart_image_artifact(
                    host,
                    art_id,
                    seed,
                    RESTART_ARTIFACT_CONTENT_TYPE,
                    RESTART_ARTIFACT_BYTES,
                    binding.created_at_ms + 10,
                )
                .expect("publish restart image artifact");
                Some((art_id, rev, *dig.as_bytes()))
            } else {
                None
            };

            out.put_u8(RESP_OK);
            out.put_binding(&binding);
            out.put_fixed(source_quota_id.as_bytes());
            out.put_fixed(source_cap_id.as_bytes());
            out.put_opt(source_dev_id, |w, d| w.put_fixed(d.as_bytes()));
            out.put_opt(restart_artifact, |w, (aid, rev, dig)| {
                w.put_fixed(aid.as_bytes());
                w.put_u64(rev);
                w.put_fixed(&dig);
            });

            *mig_state = Some(WorkerMigrationSeedState {
                crash_after_source_fence,
            });
        }
        OP_INSPECT_PROCESS_STATE => {
            let process_id = ProcessId::from_bytes(r.take_fixed::<16>());
            let seed = r.get_u8();
            let active = host
                .runtime()
                .process
                .inspect_active_process_binding(process_id);
            let sup_registered = host.supervisor_pids().lookup(process_id).ok();
            let op_state = host
                .runtime()
                .operations
                .inspect(OperationHandle {
                    operation_id: OperationId::from_bytes([seed.wrapping_add(0x05); 16]),
                    generation: Generation::INITIAL,
                })
                .ok()
                .map(|rec| rec.state);
            let cached_cap = host
                .cached_name(&cap_name_path_for(seed.wrapping_add(0x40)))
                .map(|hit| (hit.cached.capability, hit.cached.capability_generation));

            out.put_u8(RESP_OK);
            match active {
                Ok(b) => {
                    out.put_bool(true);
                    out.put_binding(&b);
                }
                Err(_) => {
                    out.put_bool(false);
                }
            }
            out.put_opt(sup_registered, |w, entry| {
                w.put_u32(entry.os_pid);
                w.put_u64(entry.process_generation.get());
            });
            out.put_opt(op_state, |w, st| {
                let code = match st {
                    OperationState::Registered => 0,
                    OperationState::Dispatched => 1,
                    OperationState::CancelRequested => 2,
                    OperationState::Completed { .. } => 3,
                    OperationState::Failed { .. } => 4,
                    OperationState::CancelledBeforeEffect { .. } => 5,
                    OperationState::PartialEffect { .. } => 6,
                    OperationState::EffectUnknown { .. } => 7,
                };
                w.put_u8(code);
            });
            out.put_opt(cached_cap, |w, (cid, cgen)| {
                w.put_fixed(cid.as_bytes());
                w.put_u64(cgen.get());
            });
        }
        OP_DRIVE_PROCESS_MIGRATION => {
            let source_mailbox_dir = PathBuf::from(r.get_str());
            let federation_root = PathBuf::from(r.get_str());
            let source_cell_id = r.get_cell_id();
            let seed = r.get_u8();
            let binding = r.get_binding();
            let source_quota_id = QuotaLeaseId::from_bytes(r.take_fixed::<16>());
            let target_quota_id = QuotaLeaseId::from_bytes(r.take_fixed::<16>());
            let source_cap_id = CapacityLeaseId::from_bytes(r.take_fixed::<16>());
            let target_cap_id = CapacityLeaseId::from_bytes(r.take_fixed::<16>());
            let dev_pair = r.get_opt(|r| {
                (
                    ExclusiveDeviceLeaseId::from_bytes(r.take_fixed::<16>()),
                    ExclusiveDeviceLeaseId::from_bytes(r.take_fixed::<16>()),
                )
            });
            let restart_image_opt = r.get_opt(|r| {
                (
                    ArtifactId::from_bytes(r.take_fixed::<16>()),
                    r.get_u64(),
                    r.take_fixed::<32>(),
                )
            });
            let expect_post_fence_quarantine = r.get_bool();

            // Reconstruct the deterministic capability rebind item matching `seed_source_capabilities_on_host`!
            let cap_seed_base = seed.wrapping_add(0x40);
            let (issuer_sk, issuer_boot) = make_capability_bootstrap(cap_seed_base);
            let (holder_sk, holder_boot) =
                make_capability_bootstrap(cap_seed_base.wrapping_add(0x10));
            let (_target_sk, target_boot) =
                make_capability_bootstrap(cap_seed_base.wrapping_add(0x20));

            let issuer_binding = host
                .runtime()
                .identity
                .bootstrap_principal(issuer_boot)
                .expect("bootstrap issuer on target")
                .binding();
            let holder_binding = host
                .runtime()
                .identity
                .bootstrap_principal(holder_boot)
                .expect("bootstrap holder on target")
                .binding();
            let target_binding = host
                .runtime()
                .identity
                .bootstrap_principal(target_boot)
                .expect("bootstrap recipient on target")
                .binding();

            let root_rights = CapabilityRights::SEMANTIC_APPEND
                .union(CapabilityRights::SEMANTIC_RETRACT)
                .union(CapabilityRights::DELEGATE);
            let root_cmd = IssueRootCapabilityRequest {
                issuer_key_id: issuer_binding.key_id,
                holder_key_id: holder_binding.key_id,
                target: CapabilityTarget::Task(binding.task_id),
                rights: root_rights,
                purpose_digest: Some([0x5A; 32]),
                valid_from_ms: 100,
                valid_until_ms: 5_000_000,
                delegation_depth_remaining: 3,
                call_limit: Some(100),
                idempotency_key: seeded_key(cap_seed_base, 140),
                issued_at_ms: 200,
            };
            let root_sig = issuer_sk
                .sign(&issue_root_command_message(root_cmd))
                .to_bytes();
            let signed_root = SignedIssueRootCapabilityRequest {
                command: root_cmd,
                signer: issuer_binding.principal_id,
                signature: root_sig,
            };
            // Pre-compute deterministic root & source child handles via local replay so the
            // delegation parent and source revocation handles match Source byte-for-byte!
            let root_rec = host
                .runtime()
                .capability()
                .issue_root_signed(&host.runtime().identity, signed_root)
                .expect("pre-seed parent root on target")
                .record();

            let src_child_cmd = DelegateCapabilityRequest {
                parent: root_rec.handle,
                delegator_key_id: holder_binding.key_id,
                recipient_key_id: holder_binding.key_id,
                target: CapabilityTarget::Task(binding.task_id),
                rights: CapabilityRights::SEMANTIC_APPEND.union(CapabilityRights::DELEGATE),
                purpose_digest: Some([0x5A; 32]),
                valid_from_ms: 150,
                valid_until_ms: 4_000_000,
                delegation_depth_remaining: 2,
                call_limit: Some(80),
                idempotency_key: seeded_key(cap_seed_base, 141),
                delegated_at_ms: 220,
            };
            let src_child_sig = holder_sk
                .sign(&delegate_command_message(src_child_cmd))
                .to_bytes();
            // Compute the deterministic capability_id of `src_child_cmd` by delegating on a temp store:
            let tmp_cap_dir = TempDir::new("cap-derive");
            let tmp_cap =
                nlos_capability::CapabilityAuthority::open(tmp_cap_dir.path()).expect("tmp cap");
            tmp_cap
                .issue_root_signed(&host.runtime().identity, signed_root)
                .expect("tmp root");
            let src_child_handle = tmp_cap
                .delegate_signed(
                    &host.runtime().identity,
                    SignedDelegateCapabilityRequest {
                        command: src_child_cmd,
                        signer: holder_binding.principal_id,
                        signature: src_child_sig,
                    },
                )
                .expect("tmp child")
                .record()
                .handle;

            let target_del_cmd = DelegateCapabilityRequest {
                parent: root_rec.handle,
                delegator_key_id: holder_binding.key_id,
                recipient_key_id: target_binding.key_id,
                target: CapabilityTarget::Task(binding.task_id),
                rights: CapabilityRights::SEMANTIC_APPEND,
                purpose_digest: Some([0x5A; 32]),
                valid_from_ms: 200,
                valid_until_ms: 3_000_000,
                delegation_depth_remaining: 1,
                call_limit: Some(40),
                idempotency_key: seeded_key(cap_seed_base, 142),
                delegated_at_ms: 250,
            };
            let target_del_sig = holder_sk
                .sign(&delegate_command_message(target_del_cmd))
                .to_bytes();
            let revoke_cmd = RevokeCapabilityRequest {
                handle: src_child_handle,
                revoker_key_id: holder_binding.key_id,
                idempotency_key: seeded_key(cap_seed_base, 143),
                revoked_at_ms: 260,
            };
            let revoke_sig = holder_sk
                .sign(&revoke_command_message(revoke_cmd))
                .to_bytes();

            let cap_item = CapabilityRebindItemSpec {
                bootstrap_principals: vec![issuer_boot, holder_boot, target_boot],
                seed_parent_root: Some(signed_root),
                signed_delegation: SignedDelegateCapabilityRequest {
                    command: target_del_cmd,
                    signer: holder_binding.principal_id,
                    signature: target_del_sig,
                },
                target_name_path: Some(cap_name_path_for(cap_seed_base)),
                revoke_on_source: Some(SignedRevokeCapabilityRequest {
                    command: revoke_cmd,
                    signer: holder_binding.principal_id,
                    signature: revoke_sig,
                }),
            };

            let mig_t0 = binding.created_at_ms + 100;
            let (attached_sessions, pinned_session_policy, checkpoint) = match restart_image_opt {
                None => (
                    vec![DriverSessionAttachment::Migratable {
                        session_id: [0x91; 16],
                        state_digest: [0x92; 32],
                    }],
                    PinnedSessionPolicy::RefuseMigration,
                    CheckpointSourceSpec::TransparentState {
                        state_payload: TRANSPARENT_STATE_BYTES.to_vec(),
                        cache_key: Some(TRANSPARENT_CACHE_KEY.to_string()),
                        checkpointed_at_ms: mig_t0 + 100,
                    },
                ),
                Some((art_id, rev, dig)) => (
                    vec![DriverSessionAttachment::PinnedNonMigratable {
                        session_id: [0x93; 16],
                        pin_reason: "gpu-vram-pinned".to_string(),
                        artifact_restart_image_digest: Some(dig),
                    }],
                    PinnedSessionPolicy::DowngradeToRestartFromArtifact,
                    CheckpointSourceSpec::ArtifactRestartImage {
                        artifact_id: art_id,
                        revision: rev,
                        sync_from_source_if_missing: true,
                        checkpointed_at_ms: mig_t0 + 100,
                    },
                ),
            };

            let req = CellHostProcessMigrationRequest {
                migration_id: [seed.wrapping_add(0x06); 16],
                process_id: binding.process_id,
                source_cell_id,
                target_cell_id: host.fence().identity(),
                source_generation: binding.process_generation,
                lineage: ProcessLineageSpec {
                    genesis: ProcessLineageGenesis::from_spawned_binding(seed, &binding),
                    prior_restores: Vec::new(),
                },
                attached_sessions,
                pinned_session_policy,
                custom_intent_reason: None,
                initiated_at_ms: mig_t0,
                quiesce: SourceQuiesceSpec {
                    cancel_scope: Some((
                        CancellationScopeId::from_bytes([seed.wrapping_add(0x03); 16]),
                        Generation::INITIAL,
                    )),
                    active_fiber_count: 1,
                    inflight_operations: vec![(
                        OperationHandle {
                            operation_id: OperationId::from_bytes([seed.wrapping_add(0x05); 16]),
                            generation: Generation::INITIAL,
                        },
                        ReceiptId::from_bytes(CANCEL_RECEIPT_BYTES),
                    )],
                    quiesced_at_ms: mig_t0 + 50,
                },
                checkpoint,
                artifact_sync_seed: seed.wrapping_add(7),
                source_fence: SourceFenceSpec {
                    invalidate_name_paths: vec![cap_name_path_for(cap_seed_base)],
                    terminate_idempotency_key: seeded_key(seed, 160),
                    stale_dispatch_probe_fiber_id: ExecutionFiberId::from_bytes(FIBER_77_BYTES),
                    stale_dispatch_probe_key: seeded_key(seed, 161),
                    fenced_at_ms: mig_t0 + 200,
                },
                capabilities: vec![cap_item],
                capabilities_rebound_at_ms: mig_t0 + 300,
                leases: LeaseMigrationBundleSpec {
                    quota: QuotaLeaseMigrationSpec {
                        source_lease_id: source_quota_id,
                        gateway_id: GATEWAY_ID,
                        override_fence_ack: None,
                        target_lease_id: target_quota_id,
                        target_face_value: None,
                        target_expires_at: None,
                        activate_on_target: true,
                    },
                    capacity: Some(CapacityLeaseMigrationSpec {
                        source_lease_id: source_cap_id,
                        reconcile_receipt_id: ReceiptId::from_bytes([seed.wrapping_add(0x71); 16]),
                        gateway_id: GATEWAY_ID,
                        target_lease_id: target_cap_id,
                        target_amount: None,
                    }),
                    device: dev_pair.map(|(sdev, tdev)| DeviceLeaseMigrationSpec {
                        source_lease_id: sdev,
                        reset_receipt_id: ReceiptId::from_bytes([seed.wrapping_add(0x72); 16]),
                        reconcile_receipt_id: ReceiptId::from_bytes([seed.wrapping_add(0x73); 16]),
                        gateway_id: GATEWAY_ID,
                        target_lease_id: tdev,
                        advance_target_epoch_if_needed: true,
                    }),
                    regranted_at_ms: mig_t0 + 400,
                },
                target_restore: TargetRestoreSpec {
                    existing_target_domain_id: None,
                    domain_policy_digest: [0x88; 32],
                    domain_idempotency_key: seeded_key(seed, 170),
                    restore_idempotency_key: seeded_key(seed, 171),
                    restored_at_ms: mig_t0 + 500,
                },
                completed_at_ms: mig_t0 + 600,
            };

            let mut src_ep = IpcSourceCellEndpoint {
                source_mailbox_dir,
                federation_root,
            };
            let mut dst_ep = LocalCellHostTargetEndpoint::new(host);

            match drive_cell_host_process_migration(&mut src_ep, &mut dst_ep, req) {
                Ok(outcome) => {
                    assert!(!expect_post_fence_quarantine);
                    assert!(outcome.receipt.verify_digest());
                    assert_eq!(
                        outcome.coordinator.current_step(),
                        ProcessMigrationStep::Completed
                    );
                    let decoded = decode_process_migration_receipt_checkpoint_entry(
                        &outcome.target_checkpoint_report.checkpoint_prefix.entries()[0],
                    )
                    .expect("decode PMIGRCPT entry");
                    assert_eq!(decoded.receipt_digest, outcome.receipt.receipt_digest);

                    out.put_u8(RESP_OK);
                    out.put_u64(
                        outcome
                            .target_restore_report
                            .restored_binding
                            .process_generation
                            .get(),
                    );
                    out.put_u32(outcome.target_restore_report.registered_target_os_pid);
                    out.put_u64(outcome.source_lease_close_report.closed_quota.returned);
                    out.put_u64(
                        outcome
                            .target_lease_regrant_report
                            .target_quota
                            .face_value(),
                    );
                    out.put_fixed(&outcome.receipt.receipt_digest);
                    out.put_u64(
                        outcome
                            .target_checkpoint_report
                            .checkpoint_record
                            .sequence(),
                    );
                }
                Err(ProcessMigrationDriveError::QuarantinedPostFence {
                    failed_step,
                    abort_outcome,
                    coordinator,
                    source_quarantine,
                    ..
                }) if expect_post_fence_quarantine => {
                    assert_eq!(failed_step, ProcessMigrationStep::LeasesRegranted);
                    assert!(matches!(
                        abort_outcome,
                        MigrationAbortOutcome::QuarantinedPostFence { .. }
                    ));
                    assert_eq!(
                        coordinator.current_step(),
                        ProcessMigrationStep::Quarantined
                    );
                    let sq = source_quarantine.expect("source quarantine report present");
                    out.put_u8(RESP_OK);
                    out.put_quota_state(sq.quota_state);
                    out.put_opt(sq.capacity_state, WireWriter::put_cap_state);
                    out.put_opt(sq.device_state, WireWriter::put_dev_state);
                }
                Err(e) => {
                    out.put_u8(RESP_ERR);
                    out.put_str(&e.to_string());
                }
            }
        }
        other => panic!("unknown worker opcode {other}"),
    }
    out.into_vec()
}

#[test]
#[ignore = "child OS process Cell worker spawned by ROAD-C-001 H6 gate tests"]
fn road_c001_cell_worker() {
    let (Ok(domain_hex), Ok(cell_root), Ok(federation_root), Ok(mailbox_dir)) = (
        std::env::var(ENV_WORKER_DOMAIN_HEX),
        std::env::var(ENV_CELL_ROOT),
        std::env::var(ENV_FEDERATION_ROOT),
        std::env::var(ENV_MAILBOX_DIR),
    ) else {
        eprintln!("skipped: road_c001_cell_worker env unset");
        return;
    };

    let domain = parse_hex16(&domain_hex);
    let mailbox_path = PathBuf::from(&mailbox_dir);
    let tokio_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build worker tokio runtime");
    let mut host = CellHost::open_federated_durable(
        Path::new(&cell_root),
        cell_config(domain),
        Path::new(&federation_root),
    )
    .expect("open durable federated CellHost in worker child OS process");
    let adapter =
        TokioRuntimeAdapter::new(tokio_rt.handle().clone(), TokioRuntimeConfig::default())
            .expect("create TokioRuntimeAdapter in worker child OS process");
    let mut live_fibers: Vec<FiberHandle> = Vec::new();
    let mut mig_state: Option<WorkerMigrationSeedState> = None;
    let mut task_term1: Option<WorkerTaskTerm1State> = None;

    let mut ready = WireWriter::new();
    ready.put_u32(std::process::id());
    ready.put_u64(host.fence().node_boot_generation().get());
    write_atomic_file(&mailbox_path.join("worker_ready.bin"), &ready.into_vec());

    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        assert!(
            Instant::now() < deadline,
            "road_c001_cell_worker timed out waiting for shutdown"
        );
        let mut req_files: Vec<PathBuf> = std::fs::read_dir(&mailbox_path)
            .expect("read worker mailbox")
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|path| {
                path.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                    n.starts_with("req_")
                        && Path::new(n)
                            .extension()
                            .is_some_and(|ext| ext.eq_ignore_ascii_case("bin"))
                })
            })
            .collect();
        if req_files.is_empty() {
            std::thread::sleep(Duration::from_millis(3));
            continue;
        }
        req_files.sort();
        for req_path in req_files {
            let Ok(frame) = std::fs::read(&req_path) else {
                continue;
            };
            let _ = std::fs::remove_file(&req_path);
            let file_name = req_path
                .file_name()
                .and_then(|n| n.to_str())
                .expect("req filename");
            let resp_name = file_name.replacen("req_", "resp_", 1);
            let resp_path = mailbox_path.join(resp_name);

            let opcode = frame[0];
            if opcode == OP_SHUTDOWN {
                let mut bye = WireWriter::new();
                bye.put_u8(RESP_OK);
                write_atomic_file(&resp_path, &bye.into_vec());
                return;
            }
            let resp = handle_worker_opcode(
                opcode,
                &frame[1..],
                &mut host,
                &adapter,
                &mut live_fibers,
                &mut mig_state,
                &mut task_term1,
            );
            write_atomic_file(&resp_path, &resp);
        }
    }
}

// =============================================================================
// Parent Test Orchestrator Helpers
// =============================================================================

fn wall_now_unix_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after UNIX_EPOCH")
            .as_millis(),
    )
    .expect("wall clock fits u64 ms")
}

#[allow(clippy::too_many_arguments)]
fn rpc_grant_leases(
    worker: &mut CellWorkerHandle,
    quota_id: QuotaLeaseId,
    quota_face: u64,
    quota_ttl: Option<u64>,
    activate: bool,
    cap_spec: Option<(CapacityLeaseId, u64)>,
    dev_spec: Option<ExclusiveDeviceLeaseId>,
) -> (u64, u64, bool, QuotaLeaseState, u64) {
    let mut w = WireWriter::new();
    w.put_fixed(quota_id.as_bytes());
    w.put_u64(quota_face);
    w.put_opt(quota_ttl, WireWriter::put_u64);
    w.put_bool(activate);
    w.put_opt(cap_spec, |w, (cid, amt)| {
        w.put_fixed(cid.as_bytes());
        w.put_u64(amt);
    });
    w.put_opt(dev_spec, |w, did| w.put_fixed(did.as_bytes()));
    let resp = worker.rpc(OP_GRANT_LEASES, &w.into_vec());
    let mut r = WireReader::new(&resp);
    assert_eq!(r.get_u8(), RESP_OK);
    (
        r.get_u64(),
        r.get_u64(),
        r.get_bool(),
        r.get_quota_state(),
        r.get_u64(),
    )
}

struct LeaseInspectSnapshot {
    quota_available: u64,
    cap_pool_remaining: u64,
    device_head_free: bool,
    quota_state: QuotaLeaseState,
    quota_face: u64,
    quota_spent: u64,
    quota_returned: u64,
    quota_remaining: u64,
    quota_epoch: u64,
    quota_boot_gen: u64,
    cap_info: Option<(CapacityLeaseState, u64, u64, u64, u64)>,
    dev_info: Option<(ExclusiveDeviceLeaseState, u64, u64, u64, u64)>,
}

fn rpc_inspect_leases(
    worker: &mut CellWorkerHandle,
    quota_id: QuotaLeaseId,
    cap_id: Option<CapacityLeaseId>,
    dev_id: Option<ExclusiveDeviceLeaseId>,
) -> LeaseInspectSnapshot {
    let mut w = WireWriter::new();
    w.put_fixed(quota_id.as_bytes());
    w.put_opt(cap_id, |w, c| w.put_fixed(c.as_bytes()));
    w.put_opt(dev_id, |w, d| w.put_fixed(d.as_bytes()));
    let resp = worker.rpc(OP_INSPECT_LEASES, &w.into_vec());
    let mut r = WireReader::new(&resp);
    assert_eq!(r.get_u8(), RESP_OK);
    LeaseInspectSnapshot {
        quota_available: r.get_u64(),
        cap_pool_remaining: r.get_u64(),
        device_head_free: r.get_bool(),
        quota_state: r.get_quota_state(),
        quota_face: r.get_u64(),
        quota_spent: r.get_u64(),
        quota_returned: r.get_u64(),
        quota_remaining: r.get_u64(),
        quota_epoch: r.get_u64(),
        quota_boot_gen: r.get_u64(),
        cap_info: r.get_opt(|r| {
            (
                r.get_cap_state(),
                r.get_u64(),
                r.get_u64(),
                r.get_u64(),
                r.get_u64(),
            )
        }),
        dev_info: r.get_opt(|r| {
            (
                r.get_dev_state(),
                r.get_u64(),
                r.get_u64(),
                r.get_u64(),
                r.get_u64(),
            )
        }),
    }
}

fn rpc_enter_partition(
    worker: &mut CellWorkerHandle,
    now_tick: u64,
    reason_code: u16,
) -> ControlPlaneLinkState {
    let mut w = WireWriter::new();
    w.put_u64(now_tick);
    w.put_u16(reason_code);
    let resp = worker.rpc(OP_ENTER_PARTITION, &w.into_vec());
    let mut r = WireReader::new(&resp);
    assert_eq!(r.get_u8(), RESP_OK);
    r.get_link_state()
}

fn rpc_local_autonomy_spend(
    worker: &mut CellWorkerHandle,
    now_tick: u64,
    lease_id: QuotaLeaseId,
    spend_delta: u64,
    cap_valid_until: u64,
    cap_revoked: bool,
    cap_verified: bool,
) -> Result<(u64, u64, QuotaLeaseState), (u8, String)> {
    let mut w = WireWriter::new();
    w.put_u64(now_tick);
    w.put_fixed(lease_id.as_bytes());
    w.put_u64(spend_delta);
    w.put_u64(cap_valid_until);
    w.put_bool(cap_revoked);
    w.put_bool(cap_verified);
    let resp = worker.rpc(OP_LOCAL_AUTONOMY_SPEND, &w.into_vec());
    let mut r = WireReader::new(&resp);
    if r.get_u8() == RESP_OK {
        Ok((r.get_u64(), r.get_u64(), r.get_quota_state()))
    } else {
        Err((r.get_u8(), r.get_str()))
    }
}

fn rpc_check_control_plane_action(
    worker: &mut CellWorkerHandle,
    now_tick: u64,
    action_tag: u8,
) -> Result<(), (bool, String)> {
    let mut w = WireWriter::new();
    w.put_u64(now_tick);
    w.put_u8(action_tag);
    let resp = worker.rpc(OP_CHECK_CONTROL_PLANE_ACTION, &w.into_vec());
    let mut r = WireReader::new(&resp);
    if r.get_u8() == RESP_OK {
        Ok(())
    } else {
        Err((r.get_bool(), r.get_str()))
    }
}

fn rpc_append_semantic_fact(
    worker: &mut CellWorkerHandle,
    seed: u8,
    revision: u64,
    appended_at_ms: u64,
) -> SemanticEventId {
    let mut w = WireWriter::new();
    w.put_u8(seed);
    w.put_fixed(&[seed.wrapping_add(1); 16]);
    w.put_fixed(&[seed.wrapping_add(2); 16]);
    w.put_fixed(&[seed.wrapping_add(3); 16]);
    w.put_fixed(&[seed.wrapping_add(4); 16]);
    w.put_fixed(&[seed.wrapping_add(5); 16]);
    w.put_u64(revision);
    w.put_fixed(&[seed.wrapping_add(6); 32]);
    w.put_u64(appended_at_ms);
    let resp = worker.rpc(OP_APPEND_SEMANTIC_RECEIPT, &w.into_vec());
    let mut r = WireReader::new(&resp);
    assert_eq!(r.get_u8(), RESP_OK);
    SemanticEventId::from_bytes(r.take_fixed::<32>())
}

#[allow(clippy::too_many_arguments)]
fn rpc_publish_local_artifact(
    worker: &mut CellWorkerHandle,
    artifact_id: ArtifactId,
    seed: u8,
    content_type: &str,
    create_new: bool,
    expected_head_revision: u64,
    bytes: &[u8],
    created_at_ms: u64,
) -> (u64, ContentDigest) {
    let mut w = WireWriter::new();
    w.put_fixed(artifact_id.as_bytes());
    w.put_u8(seed);
    w.put_str(content_type);
    w.put_bool(create_new);
    w.put_u64(expected_head_revision);
    w.put_bytes(bytes);
    w.put_u64(created_at_ms);
    let resp = worker.rpc(OP_PUBLISH_LOCAL_ARTIFACT, &w.into_vec());
    let mut r = WireReader::new(&resp);
    assert_eq!(r.get_u8(), RESP_OK);
    (r.get_u64(), ContentDigest::from_bytes(r.take_fixed::<32>()))
}

fn rpc_export_search_shard(
    worker: &mut CellWorkerHandle,
    ckpt: Option<&SignedSemanticVectorCheckpoint>,
    limit: u64,
) -> CellSearchShardOutcome {
    let mut w = WireWriter::new();
    w.put_opt(ckpt, |w, c| {
        w.put_bytes(&encode_signed_vector_checkpoint(c));
    });
    w.put_u64(limit);
    let resp = worker.rpc(OP_EXPORT_SEARCH_SHARD, &w.into_vec());
    let mut r = WireReader::new(&resp);
    assert_eq!(r.get_u8(), RESP_OK);
    let cell_domain_id = r.take_fixed::<16>();
    let checkpoint_digest = r.get_opt(WireReader::take_fixed::<32>);
    let count = usize::try_from(r.get_u64()).expect("count");
    let mut hits = Vec::with_capacity(count);
    for _ in 0..count {
        hits.push(r.get_assertion_hit());
    }
    CellSearchShardOutcome::Available {
        cell_domain_id,
        hits,
        checkpoint_digest,
    }
}

// =============================================================================
// H6 Test 1: Full Compound Fault-Injection Gate (`[ROAD-C-001]`)
// =============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn road_c001_compound_partition_crash_restart_migration_and_duplicate_replay_converges() {
    let dir = TempDir::new("compound-h6");
    let cell_a_root = dir.path().join("cell_a");
    let cell_b_root = dir.path().join("cell_b");
    let federation_root = dir.path().join("federation");
    let mailbox_a1 = dir.path().join("mailbox_a1");
    let mailbox_a2 = dir.path().join("mailbox_a2");
    let mailbox_b = dir.path().join("mailbox_b");
    let shared_task_db = dir.path().join("shared_task_authority.sqlite3");
    let shared_identity_dir = dir.path().join("shared_identity");
    std::fs::create_dir_all(&shared_identity_dir).expect("create shared identity dir");

    // 1. Spawn two real independent OS processes (`ADR-0018`: 1 OS process = 1 Cell).
    let mut worker_a =
        CellWorkerHandle::spawn(CELL_A_DOMAIN, &cell_a_root, &federation_root, &mailbox_a1);
    let mut worker_b =
        CellWorkerHandle::spawn(CELL_B_DOMAIN, &cell_b_root, &federation_root, &mailbox_b);
    assert_ne!(worker_a.os_pid, worker_b.os_pid);
    assert_eq!(worker_a.initial_boot_generation, 1);
    assert_eq!(worker_b.initial_boot_generation, 1);

    let cell_a_id = CellIdentity::from_domain(SchedulerDomainId::from_bytes(CELL_A_DOMAIN));
    let cell_b_id = CellIdentity::from_domain(SchedulerDomainId::from_bytes(CELL_B_DOMAIN));

    // Grant initial multi-family leases on both Cells before partition.
    let quota_a = QuotaLeaseId::from_bytes([0x11; 16]);
    let cap_a = CapacityLeaseId::from_bytes([0x12; 16]);
    let dev_a = ExclusiveDeviceLeaseId::from_bytes([0x13; 16]);
    let (avail_a0, cap_rem_a0, dev_free_a0, qstate_a0, _) = rpc_grant_leases(
        &mut worker_a,
        quota_a,
        400,
        Some(5_000),
        true,
        Some((cap_a, 200)),
        Some(dev_a),
    );
    assert_eq!(avail_a0, INITIAL_QUOTA_POOL - 400);
    assert_eq!(cap_rem_a0, INITIAL_CAPACITY_POOL - 200);
    assert!(!dev_free_a0);
    assert_eq!(qstate_a0, QuotaLeaseState::Active);

    let quota_b = QuotaLeaseId::from_bytes([0x21; 16]);
    let cap_b = CapacityLeaseId::from_bytes([0x22; 16]);
    let dev_b = ExclusiveDeviceLeaseId::from_bytes([0x23; 16]);
    let (avail_b0, cap_rem_b0, dev_free_b0, qstate_b0, _) = rpc_grant_leases(
        &mut worker_b,
        quota_b,
        300,
        Some(5_000),
        true,
        Some((cap_b, 150)),
        Some(dev_b),
    );
    assert_eq!(avail_b0, INITIAL_QUOTA_POOL - 300);
    assert_eq!(cap_rem_b0, INITIAL_CAPACITY_POOL - 150);
    assert!(!dev_free_b0);
    assert_eq!(qstate_b0, QuotaLeaseState::Active);

    // Seed shared base revision 1 of a symmetric-peer artifact on BOTH Cells before partition,
    // plus a fast-forward artifact created only on Cell A during partition.
    let conflict_art_id = ArtifactId::from_bytes([0x71; 16]);
    let ff_art_id = ArtifactId::from_bytes([0x72; 16]);
    let sync_content_type = "application/vnd.llmos.road-c001-doc+v1";
    let base_bytes = b"shared-base-revision-1";
    let (rev_a1, dig_a1) = rpc_publish_local_artifact(
        &mut worker_a,
        conflict_art_id,
        0x31,
        sync_content_type,
        true,
        0,
        base_bytes,
        1_000,
    );
    let (rev_b1, dig_b1) = rpc_publish_local_artifact(
        &mut worker_b,
        conflict_art_id,
        0x31,
        sync_content_type,
        true,
        0,
        base_bytes,
        1_000,
    );
    assert_eq!((rev_a1, dig_a1), (rev_b1, dig_b1));

    // Seed pre-crash task notification + one-shot ActionToken on Cell A.
    let notify_app = ApplicationId::from_bytes([0x41; 16]);
    let notify_task = TaskId::from_bytes([0x42; 16]);
    let notify_attempt = TaskAttemptId::from_bytes([0x43; 16]);
    let notify_pub_key = IdempotencyKey::from_bytes([0x44; 16]);
    let (topic_a, notif_id_a) = {
        let mut w = WireWriter::new();
        w.put_u8(0x40);
        w.put_str("road.c001.tasks");
        w.put_fixed(&[0x45; 16]);
        w.put_u64(1_050);
        w.put_fixed(notify_app.as_bytes());
        w.put_fixed(notify_task.as_bytes());
        w.put_opt(Some(notify_attempt), |w, a| w.put_fixed(a.as_bytes()));
        w.put_u8(2); // WaitingApproval
        w.put_fixed(&[0x46; 32]);
        w.put_u64(1_060);
        w.put_fixed(notify_pub_key.as_bytes());
        w.put_opt::<u64>(None, WireWriter::put_u64);
        let resp = worker_a.rpc(OP_NOTIFY_PUBLISH_AND_POLL, &w.into_vec());
        let mut r = WireReader::new(&resp);
        assert_eq!(r.get_u8(), RESP_OK);
        let tid = TopicId::from_bytes(r.take_fixed::<16>());
        let nid = NotificationId::from_bytes(r.take_fixed::<16>());
        assert!(!r.get_bool()); // newly published
        assert_eq!(r.get_u64(), 1);
        assert_eq!(r.get_u64(), 1);
        (tid, nid)
    };
    let (token_id_a, token_secret_a) = {
        let mut w = WireWriter::new();
        w.put_fixed(topic_a.as_bytes());
        w.put_fixed(notif_id_a.as_bytes());
        w.put_u64(1);
        w.put_fixed(notify_app.as_bytes());
        w.put_fixed(notify_task.as_bytes());
        w.put_opt(Some(notify_attempt), |w, a| w.put_fixed(a.as_bytes()));
        w.put_fixed(&[0x47; 16]);
        w.put_fixed(&[0x48; 32]);
        w.put_u64(1_000);
        w.put_u64(50_000);
        w.put_fixed(&[0x49; 16]);
        w.put_u64(1_070);
        let resp = worker_a.rpc(OP_ISSUE_ACTION_TOKEN, &w.into_vec());
        let mut r = WireReader::new(&resp);
        assert_eq!(r.get_u8(), RESP_OK);
        assert!(!r.get_bool());
        (r.take_fixed::<16>(), r.take_fixed::<32>())
    };

    // 2. Enter control-plane partition on BOTH live Cell OS processes (`[DIST-LOCAL-001]`).
    let link_a = rpc_enter_partition(&mut worker_a, 100, 0x501);
    let link_b = rpc_enter_partition(&mut worker_b, 100, 0x501);
    assert!(matches!(link_a, ControlPlaneLinkState::Partitioned { .. }));
    assert!(matches!(link_b, ControlPlaneLinkState::Partitioned { .. }));

    let (pid_a1, bgen_a1, hb_a1) = worker_a.ping_and_heartbeat();
    let (pid_b1, bgen_b1, hb_b1) = worker_b.ping_and_heartbeat();
    assert_eq!((pid_a1, bgen_a1), (worker_a.os_pid, 1));
    assert_eq!((pid_b1, bgen_b1), (worker_b.os_pid, 1));
    assert!(hb_a1 >= 2 && hb_b1 >= 2);

    // Local autonomy spend succeeds on both partitioned Cells while control-plane actions fail closed.
    let (spent_a, rem_a, _) =
        rpc_local_autonomy_spend(&mut worker_a, 110, quota_a, 120, 10_000, false, true)
            .expect("Cell A local autonomy spend during partition");
    assert_eq!((spent_a, rem_a), (120, 280));
    let (spent_b, rem_b, _) =
        rpc_local_autonomy_spend(&mut worker_b, 110, quota_b, 90, 10_000, false, true)
            .expect("Cell B local autonomy spend during partition");
    assert_eq!((spent_b, rem_b), (90, 210));

    for tag in 0..=3 {
        let (is_ctrl_refusal, _) = rpc_check_control_plane_action(&mut worker_a, 115, tag)
            .expect_err("control-plane action must fail closed during partition");
        assert!(is_ctrl_refusal);
    }

    // Partition-local semantic appends & divergent artifact edits on each Cell.
    let sem_eid_a = rpc_append_semantic_fact(&mut worker_a, 0x61, 1, 1_200);
    let sem_eid_b = rpc_append_semantic_fact(&mut worker_b, 0x68, 1, 1_210);
    assert_ne!(sem_eid_a, sem_eid_b);

    let _ = rpc_publish_local_artifact(
        &mut worker_a,
        conflict_art_id,
        0x32,
        sync_content_type,
        false,
        1,
        b"cell-a-divergent-branch-rev2",
        1_250,
    );
    let _ = rpc_publish_local_artifact(
        &mut worker_b,
        conflict_art_id,
        0x33,
        sync_content_type,
        false,
        1,
        b"cell-b-divergent-branch-rev2",
        1_260,
    );
    let _ = rpc_publish_local_artifact(
        &mut worker_a,
        ff_art_id,
        0x34,
        sync_content_type,
        true,
        0,
        b"cell-a-fast-forward-artifact",
        1_270,
    );

    // Verify federated search failure/degradation contract (`[DIST-FAIL-001]`, `[CONS-DECL-001]`)
    // when Cell B cannot reach partitioned Cell A!
    let local_shard_b = rpc_export_search_shard(&mut worker_b, None, 16);
    let remote_contract = RemoteCallContract::new(
        [0x88; 16],
        IdempotencyKey::from_bytes([0x89; 16]),
        50,
        2,
        0,
        false,
    );
    let remote_timeout: RemoteCallOutcome<CellSearchShardOutcome> =
        remote_contract.classify_inflight_timeout(55);
    let failed_shard_a = remote_call_outcome_to_search_shard(*cell_a_id.as_bytes(), remote_timeout);
    assert!(matches!(
        failed_shard_a,
        CellSearchShardOutcome::Failed {
            reason: RemoteSearchFailureReason::DeadlineExceeded { budget_ms: 50 },
            retryable: true,
            ..
        }
    ));
    let strict_err = aggregate_federated_cell_candidates(
        &[local_shard_b.clone(), failed_shard_a.clone()],
        16,
        false,
    )
    .expect_err("strict federated search must fail closed on remote partition timeout");
    assert!(matches!(
        strict_err,
        SliceKError::Search(SearchError::RemoteShardFailed(_))
    ));

    let degraded_report =
        aggregate_federated_cell_candidates(&[local_shard_b, failed_shard_a], 16, true)
            .expect("explicitly degraded federated search succeeds with partial disclosure");
    assert_eq!(degraded_report.failed_shards.len(), 1);
    assert_eq!(
        degraded_report.achieved_consistency,
        SearchAchievedConsistency::FederatedPartialEventual
    );

    // 3. Hard-crash Cell A (`child.kill()` + `child.wait()`) while Cell B remains alive!
    let kill_status = worker_a.hard_kill_and_wait();
    assert!(!kill_status.success());
    worker_b.assert_alive();

    // Cold-restart Cell A on the exact same `<cell_a_root>` and `<federation_root>`.
    let mut worker_a2 =
        CellWorkerHandle::spawn(CELL_A_DOMAIN, &cell_a_root, &federation_root, &mailbox_a2);
    assert_ne!(worker_a2.os_pid, pid_a1);
    assert_eq!(worker_a2.initial_boot_generation, 2);

    // Verify durable rehydration on restarted Cell A (`leases.sqlite3` zero silent refund).
    let snap_a_rehydrated = rpc_inspect_leases(&mut worker_a2, quota_a, Some(cap_a), Some(dev_a));
    assert_eq!(snap_a_rehydrated.quota_available, INITIAL_QUOTA_POOL - 400);
    assert_eq!(snap_a_rehydrated.quota_face, 400);
    assert_eq!(snap_a_rehydrated.quota_spent, 120);
    assert_eq!(snap_a_rehydrated.quota_returned, 0);
    assert_eq!(snap_a_rehydrated.quota_remaining, 280);
    assert_eq!(snap_a_rehydrated.quota_epoch, 1);
    assert_eq!(snap_a_rehydrated.quota_boot_gen, 1);
    assert_eq!(
        snap_a_rehydrated.cap_pool_remaining,
        INITIAL_CAPACITY_POOL - 200
    );
    assert!(!snap_a_rehydrated.device_head_free);

    let snap_b_pre_heal = rpc_inspect_leases(&mut worker_b, quota_b, Some(cap_b), Some(dev_b));

    // Place restarted Cell A into `Partitioned` so both endpoints execute the 4-phase
    // reconnection convergence protocol symmetrically.
    let _ = rpc_enter_partition(&mut worker_a2, 180, 0x502);

    // 4. Drive 4-phase partition reconnection convergence across BOTH live OS processes!
    let cap_info_a = snap_a_rehydrated.cap_info.expect("cap_a info");
    let dev_info_a = snap_a_rehydrated.dev_info.expect("dev_a info");
    let cap_attest_a = CleanupAttestation::new(
        cap_a,
        cell_a_id,
        Generation::new(NonZeroU64::new(cap_info_a.2).expect("gen")),
        CellEpoch::from_u64(cap_info_a.3).expect("epoch"),
        CellFencingToken::from_u64(cap_info_a.4).expect("token"),
        cap_info_a.1,
        true,
        CleanupAttestation::compute_digest(
            cap_a,
            cell_a_id,
            Generation::new(NonZeroU64::new(cap_info_a.2).expect("gen")),
            CellEpoch::from_u64(cap_info_a.3).expect("epoch"),
            CellFencingToken::from_u64(cap_info_a.4).expect("token"),
            cap_info_a.1,
            true,
        ),
    );
    let dev_reset_a = DeviceResetReceipt::new(
        ReceiptId::from_bytes([0x91; 16]),
        dev_a,
        cell_a_id,
        Generation::new(NonZeroU64::new(dev_info_a.1).expect("gen")),
        CellEpoch::from_u64(dev_info_a.2).expect("epoch"),
        CellFencingToken::from_u64(dev_info_a.3).expect("token"),
        Generation::new(NonZeroU64::new(dev_info_a.4).expect("gen")),
    );

    let cap_info_b = snap_b_pre_heal.cap_info.expect("cap_b info");
    let dev_info_b = snap_b_pre_heal.dev_info.expect("dev_b info");
    let cap_attest_b = CleanupAttestation::new(
        cap_b,
        cell_b_id,
        Generation::new(NonZeroU64::new(cap_info_b.2).expect("gen")),
        CellEpoch::from_u64(cap_info_b.3).expect("epoch"),
        CellFencingToken::from_u64(cap_info_b.4).expect("token"),
        cap_info_b.1,
        true,
        CleanupAttestation::compute_digest(
            cap_b,
            cell_b_id,
            Generation::new(NonZeroU64::new(cap_info_b.2).expect("gen")),
            CellEpoch::from_u64(cap_info_b.3).expect("epoch"),
            CellFencingToken::from_u64(cap_info_b.4).expect("token"),
            cap_info_b.1,
            true,
        ),
    );
    let dev_reset_b = DeviceResetReceipt::new(
        ReceiptId::from_bytes([0x92; 16]),
        dev_b,
        cell_b_id,
        Generation::new(NonZeroU64::new(dev_info_b.1).expect("gen")),
        CellEpoch::from_u64(dev_info_b.2).expect("epoch"),
        CellFencingToken::from_u64(dev_info_b.3).expect("token"),
        Generation::new(NonZeroU64::new(dev_info_b.4).expect("gen")),
    );

    let sync_policy = ArtifactSyncPolicy {
        content_type: sync_content_type.to_string(),
        merge_strategy: ArtifactMergeStrategy::FastForwardLineageOnly,
        conflict_strategy: ArtifactConflictStrategy::QuarantineDivergentBranch,
        replica_authority: ArtifactReplicaAuthority::SymmetricPeer,
        deletion_semantics: ArtifactDeletionSemantics::TombstoneRevisionRequired,
    };

    let heal_now_ms = wall_now_unix_ms() + 5_000;
    let outcome = {
        let mut ep_a = IpcPartitionEndpoint {
            worker: &mut worker_a2,
        };
        let mut ep_b = IpcPartitionEndpoint {
            worker: &mut worker_b,
        };
        drive_partition_reconnection_convergence(
            &mut ep_a,
            &mut ep_b,
            &PartitionRecoveryRequest {
                recovery_id: [0xEE; 16],
                reconnection_tick: 200,
                completion_tick: 260,
                synced_at_ms: heal_now_ms,
                trust_snapshot_digest_a: [0xAB; 32],
                trust_snapshot_digest_b: [0xAB; 32],
                cell_a_leases: CellLeaseReconciliationSpec {
                    // Cell A crashed with 120 reported + 30 unattested in-flight loss (<= window 50),
                    // refunding 250 to AVAILABLE!
                    quota_leases: vec![QuotaPartitionSettlementSpec {
                        lease_id: quota_a,
                        receipt_id: ReceiptId::from_bytes([0x93; 16]),
                        reconcile_seq: 1,
                        gateway_id: GATEWAY_ID,
                        attested_spent: 120,
                        unattested_inflight_loss: 30,
                        max_unreported_inflight_window: 50,
                    }],
                    capacity_leases: vec![CapacityPartitionSettlementSpec {
                        capacity_lease_id: cap_a,
                        receipt_id: ReceiptId::from_bytes([0x94; 16]),
                        gateway_id: GATEWAY_ID,
                        cleanup_attestation: cap_attest_a,
                    }],
                    device_leases: vec![DevicePartitionSettlementSpec {
                        device_lease_id: dev_a,
                        receipt_id: ReceiptId::from_bytes([0x95; 16]),
                        gateway_id: GATEWAY_ID,
                        reset_receipt: dev_reset_a,
                    }],
                },
                cell_b_leases: CellLeaseReconciliationSpec {
                    quota_leases: vec![QuotaPartitionSettlementSpec {
                        lease_id: quota_b,
                        receipt_id: ReceiptId::from_bytes([0x96; 16]),
                        reconcile_seq: 1,
                        gateway_id: GATEWAY_ID,
                        attested_spent: 90,
                        unattested_inflight_loss: 10,
                        max_unreported_inflight_window: 25,
                    }],
                    capacity_leases: vec![CapacityPartitionSettlementSpec {
                        capacity_lease_id: cap_b,
                        receipt_id: ReceiptId::from_bytes([0x97; 16]),
                        gateway_id: GATEWAY_ID,
                        cleanup_attestation: cap_attest_b,
                    }],
                    device_leases: vec![DevicePartitionSettlementSpec {
                        device_lease_id: dev_b,
                        receipt_id: ReceiptId::from_bytes([0x98; 16]),
                        gateway_id: GATEWAY_ID,
                        reset_receipt: dev_reset_b,
                    }],
                },
                semantic_convergence: SemanticPartitionConvergenceSpec {
                    additional_principals_for_a: Vec::new(),
                    additional_principals_for_b: Vec::new(),
                    verify_idempotent_second_replay: true,
                },
                artifact_convergence: ArtifactPartitionConvergenceSpec {
                    policies: vec![sync_policy],
                    cell_a_exports: vec![conflict_art_id, ff_art_id],
                    cell_b_exports: vec![conflict_art_id],
                    cell_a_import_context: ArtifactImportCommitContext::default(),
                    cell_b_import_context: ArtifactImportCommitContext {
                        task_id: TaskId::from_bytes([0xA4; 16]),
                        permit_id: CommitPermitId::from_bytes([0xA5; 16]),
                        write_set_root: ContentDigest::from_bytes([0xA6; 32]),
                        base_idempotency_key: IdempotencyKey::from_bytes([0xA7; 16]),
                    },
                },
                record_federation_checkpoints: true,
            },
        )
        .expect("drive 4-phase partition reconnection convergence across live OS processes")
    };

    assert!(outcome.receipt.verify_digest());
    assert!(outcome.cell_a_bounded_loss.verify_invariants());
    assert!(outcome.cell_b_bounded_loss.verify_invariants());
    assert_eq!(outcome.cell_a_bounded_loss.permanently_quarantined_loss, 30);
    assert_eq!(outcome.cell_a_bounded_loss.refunded_available, 250);
    assert_eq!(
        outcome.cell_a_bounded_loss.pool_available_after_reconcile,
        INITIAL_QUOTA_POOL - 150
    );
    assert_eq!(outcome.cell_b_bounded_loss.permanently_quarantined_loss, 10);
    assert_eq!(outcome.cell_b_bounded_loss.refunded_available, 200);
    assert_eq!(
        outcome.cell_b_bounded_loss.pool_available_after_reconcile,
        INITIAL_QUOTA_POOL - 100
    );
    assert!(outcome.semantic_convergence.replay_idempotency_verified);
    assert_eq!(
        outcome
            .semantic_convergence
            .post_sync_checkpoint_a
            .checkpoint_digest,
        outcome
            .semantic_convergence
            .post_sync_checkpoint_b
            .checkpoint_digest
    );
    assert_eq!(outcome.semantic_convergence.converged_event_count, 2);
    assert_eq!(
        outcome
            .artifact_convergence
            .canonical_conflict_digests
            .len(),
        1
    );
    let ckpt_a = outcome
        .cell_a_checkpoint
        .expect("cell A PARTRECV checkpoint");
    let ckpt_b = outcome
        .cell_b_checkpoint
        .expect("cell B PARTRECV checkpoint");
    assert!(ckpt_a.digest_verified && ckpt_b.digest_verified);
    let prefix = partition_recovery_checkpoint_prefix(&outcome.receipt);
    let decoded_partrecv =
        decode_partition_recovery_checkpoint_entry(&prefix.entries()[0]).expect("decode PARTRECV");
    assert_eq!(
        decoded_partrecv.receipt_digest,
        outcome.receipt.receipt_digest
    );

    // Verify fast-forward artifact `ff_art_id` is now readable on Cell B after convergence!
    {
        let mut w = WireWriter::new();
        w.put_fixed(ff_art_id.as_bytes());
        let resp = worker_b.rpc(OP_INSPECT_LOCAL_ARTIFACT, &w.into_vec());
        let mut r = WireReader::new(&resp);
        assert_eq!(r.get_u8(), RESP_OK);
        let head = r
            .get_opt(WireReader::get_head)
            .expect("ff_art_id synced to Cell B");
        assert_eq!(head.revision, 1);
    }

    // Verify post-heal federated search across BOTH converged Cells (`CheckpointBoundedSnapshot`)
    // and checkpoint-pinned candidate verification (`[SEM-CHECKPOINT-001]`, `[CONS-CTRL-001]`).
    let signed_ckpt_a = {
        let mut ep_a = IpcPartitionEndpoint {
            worker: &mut worker_a2,
        };
        ep_a.export_signed_vector_checkpoint([0xAA; 32], heal_now_ms + 100)
            .expect("export signed post-heal vector checkpoint on Cell A")
    };
    let signed_ckpt_b = {
        let mut ep_b = IpcPartitionEndpoint {
            worker: &mut worker_b,
        };
        ep_b.export_signed_vector_checkpoint([0xAB; 32], heal_now_ms + 200)
            .expect("export signed post-heal vector checkpoint on Cell B")
    };
    {
        let mut w = WireWriter::new();
        w.put_bytes(&encode_signed_vector_checkpoint(&signed_ckpt_b));
        w.put_u64(16);
        w.put_bool(true);
        w.put_bool(true);
        let resp = worker_b.rpc(OP_QUERY_CHECKPOINT_SEARCH, &w.into_vec());
        let mut r = WireReader::new(&resp);
        assert_eq!(r.get_u8(), RESP_OK);
        let count = r.get_u64();
        // `query_at_signed_checkpoint` scans Cell B's own `local_domain_id` watermark cut (`sem_eid_b`).
        assert_eq!(count, 1);
        let hit_b = r.get_assertion_hit();
        assert_eq!(hit_b.event_id, sem_eid_b);
        assert!(r.get_bool()); // verified_ok
        assert!(r.get_bool()); // tampered_rejected
        assert!(r.get_bool()); // control_rejected
    }
    // Unbounded Cell B shard scan sees BOTH converged cross-domain events (`sem_eid_a` + `sem_eid_b`),
    // and checkpoint-pinned federated aggregation across Cell A + Cell B also converges to 2 unique hits!
    if let CellSearchShardOutcome::Available { hits, .. } =
        rpc_export_search_shard(&mut worker_b, None, 16)
    {
        assert_eq!(hits.len(), 2);
    } else {
        panic!("expected Available local search shard on Cell B");
    }
    let shard_a_post = rpc_export_search_shard(&mut worker_a2, Some(&signed_ckpt_a), 16);
    let shard_b_post = rpc_export_search_shard(&mut worker_b, Some(&signed_ckpt_b), 16);
    let fed_converged =
        aggregate_federated_cell_candidates(&[shard_a_post, shard_b_post], 16, false)
            .expect("converged federated search across both live Cells");
    assert_eq!(fed_converged.failed_shards.len(), 0);
    assert_eq!(
        fed_converged.achieved_consistency,
        SearchAchievedConsistency::CheckpointBoundedSnapshot
    );
    // Deduplicated by SemanticEventId across both converged shards -> exactly 2 unique hits!
    assert_eq!(fed_converged.candidates.len(), 2);

    // Verify duplicate notification publish & one-shot ActionToken redemption on restarted Cell A!
    {
        let mut w = WireWriter::new();
        w.put_u8(0x40);
        w.put_str("road.c001.tasks");
        w.put_fixed(&[0x45; 16]);
        w.put_u64(1_050);
        w.put_fixed(notify_app.as_bytes());
        w.put_fixed(notify_task.as_bytes());
        w.put_opt(Some(notify_attempt), |w, a| w.put_fixed(a.as_bytes()));
        w.put_u8(2);
        w.put_fixed(&[0x46; 32]);
        w.put_u64(1_060);
        w.put_fixed(notify_pub_key.as_bytes()); // exact same idempotency key replayed post-crash!
        w.put_opt(Some(1_u64), WireWriter::put_u64);
        let resp = worker_a2.rpc(OP_NOTIFY_PUBLISH_AND_POLL, &w.into_vec());
        let mut r = WireReader::new(&resp);
        assert_eq!(r.get_u8(), RESP_OK);
        let _ = r.take_fixed::<16>();
        let _ = r.take_fixed::<16>();
        assert!(r.get_bool()); // Replayed!
        assert_eq!(r.get_u64(), 1);
    }
    {
        let mut w = WireWriter::new();
        w.put_fixed(&token_id_a);
        w.put_fixed(&token_secret_a);
        w.put_fixed(notif_id_a.as_bytes());
        w.put_fixed(notify_app.as_bytes());
        w.put_fixed(notify_task.as_bytes());
        w.put_u16(1);
        w.put_u64(2_400);
        let resp1 = worker_a2.rpc(OP_REDEEM_ACTION_TOKEN, &w.into_vec());
        let mut r1 = WireReader::new(&resp1);
        assert_eq!(r1.get_u8(), RESP_OK);

        // Replay of the exact same ActionToken must fail closed (`ActionTokenAlreadyRedeemed`)!
        let mut w2 = WireWriter::new();
        w2.put_fixed(&token_id_a);
        w2.put_fixed(&token_secret_a);
        w2.put_fixed(notif_id_a.as_bytes());
        w2.put_fixed(notify_app.as_bytes());
        w2.put_fixed(notify_task.as_bytes());
        w2.put_u16(1);
        w2.put_u64(2_405);
        let resp2 = worker_a2.rpc(OP_REDEEM_ACTION_TOKEN, &w2.into_vec());
        let mut r2 = WireReader::new(&resp2);
        assert_eq!(r2.get_u8(), RESP_ERR);
        assert_eq!(r2.get_u8(), NOTIFY_ERR_ALREADY_REDEEMED);
    }

    // 5. Cross-Cell `TaskAuthority` Takeover (`[DIST-TASK-001/002/004]`) from Cell A -> Cell B!
    let takeover_task_id = TaskId::from_bytes([0xD1; 16]);
    {
        let mut w = WireWriter::new();
        w.put_str(shared_task_db.to_str().expect("utf8 path"));
        w.put_str(shared_identity_dir.to_str().expect("utf8 path"));
        w.put_fixed(takeover_task_id.as_bytes());
        w.put_u64(100);
        w.put_u64(100); // expires at 200
        let resp = worker_a2.rpc(OP_SEED_TASK_TERM1, &w.into_vec());
        let mut r = WireReader::new(&resp);
        assert_eq!(r.get_u8(), RESP_OK);
        assert_eq!(r.get_u64(), 1); // term 1
    }
    {
        let mut w = WireWriter::new();
        w.put_str(shared_task_db.to_str().expect("utf8 path"));
        w.put_str(shared_identity_dir.to_str().expect("utf8 path"));
        w.put_str(mailbox_a2.to_str().expect("utf8 path"));
        w.put_fixed(cell_a_id.as_bytes());
        w.put_fixed(takeover_task_id.as_bytes());
        w.put_u64(250); // past term-1 expiry (200)
        let resp = worker_b.rpc(OP_DRIVE_TASK_TAKEOVER, &w.into_vec());
        let mut r = WireReader::new(&resp);
        assert_eq!(r.get_u8(), RESP_OK);
        assert_eq!(r.get_u64(), 2); // term 2 acquired on Cell B!
        let _fencing_token = r.take_fixed::<32>();
        assert_eq!(r.get_u64(), 2); // reopened participant registry generation 2
        let _ckpt_seq = r.get_u64();
        assert!(r.get_bool()); // duplicate takeover replay refused!
    }
    {
        // Live Cell A proves its stale term-1 lease is fenced on both prepare and permit!
        let mut w = WireWriter::new();
        w.put_str(shared_task_db.to_str().expect("utf8 path"));
        w.put_fixed(takeover_task_id.as_bytes());
        w.put_u64(400);
        let resp = worker_a2.rpc(OP_PROBE_STALE_TASK_AUTHORITY, &w.into_vec());
        let mut r = WireReader::new(&resp);
        assert_eq!(r.get_u8(), RESP_OK);
        assert!(r.get_bool()); // prepare_fenced
        assert!(r.get_bool()); // permit_fenced
    }

    // 6. Cross-Cell 8-Step `CellHost` Process Migration (`[DIST-MIGRATE-001/002]`,
    // `[LEASE-MIGRATE-001]`) from Cell A -> Cell B (transparent checkpoint + 3-family leases)
    // AND Cell B -> Cell A (pinned-session downgraded restart-from-artifact)!
    let (proc_binding_a, src_q_a, src_c_a, src_d_a) = {
        let mut w = WireWriter::new();
        w.put_u8(0x18);
        w.put_bool(true); // with_device_lease (device head was freed during partition recovery!)
        w.put_bool(false); // transparent checkpoint mode
        w.put_bool(false); // no post-fence crash
        let resp = worker_a2.rpc(OP_SEED_MIGRATABLE_PROCESS, &w.into_vec());
        let mut r = WireReader::new(&resp);
        assert_eq!(r.get_u8(), RESP_OK);
        let proc_bind = r.get_binding();
        let quota_id = QuotaLeaseId::from_bytes(r.take_fixed::<16>());
        let cap_id = CapacityLeaseId::from_bytes(r.take_fixed::<16>());
        let dev_id = r
            .get_opt(|r| ExclusiveDeviceLeaseId::from_bytes(r.take_fixed::<16>()))
            .expect("device lease present");
        (proc_bind, quota_id, cap_id, dev_id)
    };
    {
        let mut w = WireWriter::new();
        w.put_str(mailbox_a2.to_str().expect("utf8"));
        w.put_str(federation_root.to_str().expect("utf8"));
        w.put_fixed(cell_a_id.as_bytes());
        w.put_u8(0x18);
        w.put_binding(&proc_binding_a);
        w.put_fixed(src_q_a.as_bytes());
        w.put_fixed(&[0xB4; 16]);
        w.put_fixed(src_c_a.as_bytes());
        w.put_fixed(&[0xB5; 16]);
        w.put_opt(
            Some((src_d_a, ExclusiveDeviceLeaseId::from_bytes([0xB6; 16]))),
            |w, (sd, td)| {
                w.put_fixed(sd.as_bytes());
                w.put_fixed(td.as_bytes());
            },
        );
        w.put_opt::<(ArtifactId, u64, [u8; 32])>(None, |_, _| {});
        w.put_bool(false);

        let resp = worker_b.rpc(OP_DRIVE_PROCESS_MIGRATION, &w.into_vec());
        let mut r = WireReader::new(&resp);
        let tag = r.get_u8();
        assert_eq!(
            tag,
            RESP_OK,
            "Hop 1 Cell A -> Cell B migration failed: {}",
            r.get_str()
        );
        assert_eq!(r.get_u64(), 2); // target_generation == 2!
        assert_eq!(r.get_u32(), worker_b.os_pid);
        let returned_from_src = r.get_u64();
        let target_face = r.get_u64();
        assert_eq!(returned_from_src, 220); // 300 - 80 spent
        assert_eq!(target_face, 220); // exact conservation (`[LEASE-MIGRATE-001]`)
    }
    // Verify Source `worker_a2` has fenced/unregistered generation 1 while Target `worker_b`
    // owns active generation 2 (zero dual-active process)!
    {
        let mut w = WireWriter::new();
        w.put_fixed(proc_binding_a.process_id.as_bytes());
        w.put_u8(0x18);
        let resp_src = worker_a2.rpc(OP_INSPECT_PROCESS_STATE, &w.clone().into_vec());
        let mut rs = WireReader::new(&resp_src);
        assert_eq!(rs.get_u8(), RESP_OK);
        assert!(!rs.get_bool()); // inactive on Source!
        assert!(rs.get_opt(|r| (r.get_u32(), r.get_u64())).is_none()); // unregistered from supervisor!
        assert_eq!(rs.get_opt(WireReader::get_u8), Some(2)); // OperationState::CancelRequested!
        assert!(
            rs.get_opt(|r| (r.take_fixed::<16>(), r.get_u64()))
                .is_none()
        ); // NameCache invalidated!

        let resp_dst = worker_b.rpc(OP_INSPECT_PROCESS_STATE, &w.into_vec());
        let mut rd = WireReader::new(&resp_dst);
        assert_eq!(rd.get_u8(), RESP_OK);
        assert!(rd.get_bool()); // active on Target!
        let restored = rd.get_binding();
        assert_eq!(restored.process_generation.get(), 2);
        let sup = rd
            .get_opt(|r| (r.get_u32(), r.get_u64()))
            .expect("registered in Target supervisor");
        assert_eq!(sup, (worker_b.os_pid, 2));
        let _ = rd.get_opt(WireReader::get_u8);
        assert!(
            rd.get_opt(|r| (r.take_fixed::<16>(), r.get_u64()))
                .is_some()
        ); // NameCache rebound!
    }

    // Second migration direction (`Cell B -> Cell A`): pinned-session downgraded
    // `restart-from-artifact` (`[DIST-MIGRATE-002]`) completing all 8 steps!
    let (proc_binding_b, src_q_b, src_c_b, restart_img_b) = {
        let mut w = WireWriter::new();
        w.put_u8(0x38);
        w.put_bool(false); // device head on B is held by migrated process A; use quota+capacity
        w.put_bool(true); // pinned-session restart-from-artifact mode!
        w.put_bool(false);
        let resp = worker_b.rpc(OP_SEED_MIGRATABLE_PROCESS, &w.into_vec());
        let mut r = WireReader::new(&resp);
        assert_eq!(r.get_u8(), RESP_OK);
        let proc_bind = r.get_binding();
        let quota_id = QuotaLeaseId::from_bytes(r.take_fixed::<16>());
        let cap_id = CapacityLeaseId::from_bytes(r.take_fixed::<16>());
        let _ = r.get_opt(|r| ExclusiveDeviceLeaseId::from_bytes(r.take_fixed::<16>()));
        let img = r
            .get_opt(|r| {
                (
                    ArtifactId::from_bytes(r.take_fixed::<16>()),
                    r.get_u64(),
                    r.take_fixed::<32>(),
                )
            })
            .expect("restart image artifact on Cell B");
        (proc_bind, quota_id, cap_id, img)
    };
    {
        let mut w = WireWriter::new();
        w.put_str(mailbox_b.to_str().expect("utf8"));
        w.put_str(federation_root.to_str().expect("utf8"));
        w.put_fixed(cell_b_id.as_bytes());
        w.put_u8(0x38);
        w.put_binding(&proc_binding_b);
        w.put_fixed(src_q_b.as_bytes());
        w.put_fixed(&[0xB7; 16]);
        w.put_fixed(src_c_b.as_bytes());
        w.put_fixed(&[0xB8; 16]);
        w.put_opt::<(ExclusiveDeviceLeaseId, ExclusiveDeviceLeaseId)>(None, |_, _| {});
        w.put_opt(Some(restart_img_b), |w, (aid, rev, dig)| {
            w.put_fixed(aid.as_bytes());
            w.put_u64(rev);
            w.put_fixed(&dig);
        });
        w.put_bool(false);

        let resp = worker_a2.rpc(OP_DRIVE_PROCESS_MIGRATION, &w.into_vec());
        let mut r = WireReader::new(&resp);
        let tag = r.get_u8();
        assert_eq!(
            tag,
            RESP_OK,
            "Hop 2 Cell B -> Cell A migration failed: {}",
            r.get_str()
        );
        assert_eq!(r.get_u64(), 2);
        assert_eq!(r.get_u32(), worker_a2.os_pid);
        assert_eq!(r.get_u64(), 220);
        assert_eq!(r.get_u64(), 220);
    }

    worker_a2.shutdown_cleanly();
    worker_b.shutdown_cleanly();
}

// =============================================================================
// H6 Test 2: Quota Freeze, WAL Loss Refusal, & Exact Bounded-Loss Proof
// =============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn road_c001_quota_freeze_and_exact_bounded_loss_under_partition_and_crash() {
    let dir = TempDir::new("bounded-loss-h6");
    let cell_a_root = dir.path().join("cell_a");
    let cell_b_root = dir.path().join("cell_b");
    let federation_root = dir.path().join("federation");
    let mailbox_a1 = dir.path().join("mailbox_a1");
    let mailbox_a2 = dir.path().join("mailbox_a2");
    let mailbox_b = dir.path().join("mailbox_b");

    let mut worker_a =
        CellWorkerHandle::spawn(CELL_A_DOMAIN, &cell_a_root, &federation_root, &mailbox_a1);
    let mut worker_b =
        CellWorkerHandle::spawn(CELL_B_DOMAIN, &cell_b_root, &federation_root, &mailbox_b);

    // Grant 3 QuotaLeases on Cell A:
    // - `lease_active`: 300 face, TTL 5_000 (spends 110 during partition)
    // - `lease_short_ttl`: 150 face, TTL 120 (expires during partition -> swept to FENCED -> QUARANTINED)
    // - `lease_wal_lost`: 100 face, no TTL (proves [LEASE-WAL-001] refusal of guessed remaining)
    let lease_active = QuotaLeaseId::from_bytes([0x01; 16]);
    let lease_short_ttl = QuotaLeaseId::from_bytes([0x02; 16]);
    let lease_wal_lost = QuotaLeaseId::from_bytes([0x03; 16]);
    let _ = rpc_grant_leases(
        &mut worker_a,
        lease_active,
        300,
        Some(5_000),
        true,
        None,
        None,
    );
    let _ = rpc_grant_leases(
        &mut worker_a,
        lease_short_ttl,
        150,
        Some(120),
        true,
        None,
        None,
    );
    let _ = rpc_grant_leases(&mut worker_a, lease_wal_lost, 100, None, true, None, None);

    let lease_b = QuotaLeaseId::from_bytes([0x04; 16]);
    let _ = rpc_grant_leases(&mut worker_b, lease_b, 200, Some(5_000), true, None, None);

    // Capture Cell A's pre-partition fence so we can probe stale-epoch spend after heal.
    let pre_partition_fence_a = {
        let mut ep_a = IpcPartitionEndpoint {
            worker: &mut worker_a,
        };
        ep_a.current_fence().expect("current fence")
    };

    let _ = rpc_enter_partition(&mut worker_a, 100, 0x601);
    let _ = rpc_enter_partition(&mut worker_b, 100, 0x601);

    // Valid local autonomy spend of 110 on `lease_active`.
    let (spent, rem, _) =
        rpc_local_autonomy_spend(&mut worker_a, 110, lease_active, 110, 10_000, false, true)
            .expect("valid local spend");
    assert_eq!((spent, rem), (110, 190));

    // Expired capability, revoked capability, unverified capability, and spend > remaining
    // all fail closed under `[DIST-LOCAL-001]` without mutating ledger spend!
    assert!(
        rpc_local_autonomy_spend(&mut worker_a, 115, lease_active, 10, 112, false, true).is_err()
    );
    assert!(
        rpc_local_autonomy_spend(&mut worker_a, 115, lease_active, 10, 10_000, true, true).is_err()
    );
    assert!(
        rpc_local_autonomy_spend(&mut worker_a, 115, lease_active, 10, 10_000, false, false)
            .is_err()
    );
    assert!(
        rpc_local_autonomy_spend(&mut worker_a, 115, lease_active, 250, 10_000, false, true)
            .is_err()
    );

    // Sweep TTL at tick 130 -> `lease_short_ttl` transitions to `FENCED` (`[LEASE-TTL-001]`).
    {
        let mut w = WireWriter::new();
        w.put_u64(130);
        w.put_fixed(lease_wal_lost.as_bytes());
        w.put_u64(100);
        w.put_bool(false);
        w.put_u64(1);
        w.put_u64(2);
        w.put_opt::<([u8; 40], QuotaLeaseId, u64)>(None, |_, _| {});
        let resp = worker_a.rpc(OP_SWEEP_AND_WAL_LOSS_PROBES, &w.into_vec());
        let mut r = WireReader::new(&resp);
        assert_eq!(r.get_u8(), RESP_OK);
        assert_eq!(r.get_u64(), 1); // 1 lease swept (`lease_short_ttl`)
        assert_eq!(
            QuotaLeaseId::from_bytes(r.take_fixed::<16>()),
            lease_short_ttl
        );
        assert!(r.get_bool()); // guessed remaining refused!
    }

    // Hard crash Cell A and cold restart into boot generation 2!
    let _ = worker_a.hard_kill_and_wait();
    let mut worker_a2 =
        CellWorkerHandle::spawn(CELL_A_DOMAIN, &cell_a_root, &federation_root, &mailbox_a2);
    assert_eq!(worker_a2.initial_boot_generation, 2);

    // Verify `[LEASE-WAL-001]`:
    // 1) `refuse_guessed_remaining_on_wal_loss` fails closed (`GuessedRemainingForbidden`);
    // 2) `fence_boot_generation_on_wal_loss(1 -> 2)` fences all 3 unsettled boot-gen-1 leases into
    //    `QUARANTINED` with ZERO silent refund to `AVAILABLE` (`1000 - 550 == 450`)!
    {
        let mut w = WireWriter::new();
        w.put_u64(140);
        w.put_fixed(lease_wal_lost.as_bytes());
        w.put_u64(100);
        w.put_bool(true); // execute fence_boot_generation_on_wal_loss(1, 2)!
        w.put_u64(1);
        w.put_u64(2);
        w.put_opt(
            Some((encode_cell_fence(pre_partition_fence_a), lease_active, 120)),
            |w, (f, lid, hw)| {
                w.put_fixed(&f);
                w.put_fixed(lid.as_bytes());
                w.put_u64(hw);
            },
        );
        let resp = worker_a2.rpc(OP_SWEEP_AND_WAL_LOSS_PROBES, &w.into_vec());
        let mut r = WireReader::new(&resp);
        assert_eq!(r.get_u8(), RESP_OK);
        assert_eq!(r.get_u64(), 0);
        assert!(r.get_bool()); // guessed remaining refused!
        assert_eq!(r.get_u64(), 2); // epoch advanced to 2!
        assert_eq!(r.get_u64(), 3); // all 3 boot-gen-1 leases quarantined!
        assert!(r.get_bool()); // stale pre-partition epoch spend refused!
    }
    let post_wal_fence = rpc_inspect_leases(&mut worker_a2, lease_active, None, None);
    assert_eq!(post_wal_fence.quota_available, INITIAL_QUOTA_POOL - 550);
    assert_eq!(post_wal_fence.quota_state, QuotaLeaseState::Quarantined);

    // Forged `FenceBarrierAck` on `lease_wal_lost` MUST fail closed (`InvalidFenceBarrierAck`).
    {
        let mut w = WireWriter::new();
        w.put_fixed(lease_wal_lost.as_bytes());
        w.put_fixed(&[0xA0; 16]);
        w.put_u64(1);
        w.put_fixed(&GATEWAY_ID);
        w.put_u64(20);
        w.put_u64(50);
        w.put_u64(30);
        w.put_bool(true); // tamper_ack = true!
        let resp = worker_a2.rpc(OP_RECONCILE_QUOTA_DIRECT, &w.into_vec());
        let mut r = WireReader::new(&resp);
        assert_eq!(r.get_u8(), RESP_ERR);
        assert!(r.get_str().contains("InvalidFenceBarrierAck"));
    }

    // Enter partition on `worker_a2` and prove that `drive_partition_reconnection_convergence`
    // FAILS CLOSED (`BoundedLossViolation`) if `unattested_inflight_loss > max_unreported_inflight_window`!
    let _ = rpc_enter_partition(&mut worker_a2, 190, 0x602);
    {
        let mut ep_a = IpcPartitionEndpoint {
            worker: &mut worker_a2,
        };
        let mut ep_b = IpcPartitionEndpoint {
            worker: &mut worker_b,
        };
        let bad_res = drive_partition_reconnection_convergence(
            &mut ep_a,
            &mut ep_b,
            &PartitionRecoveryRequest {
                recovery_id: [0xEF; 16],
                reconnection_tick: 200,
                completion_tick: 250,
                synced_at_ms: 2_500,
                trust_snapshot_digest_a: [0xAC; 32],
                trust_snapshot_digest_b: [0xAC; 32],
                cell_a_leases: CellLeaseReconciliationSpec {
                    quota_leases: vec![QuotaPartitionSettlementSpec {
                        lease_id: lease_wal_lost,
                        receipt_id: ReceiptId::from_bytes([0xA1; 16]),
                        reconcile_seq: 1,
                        gateway_id: GATEWAY_ID,
                        attested_spent: 20,
                        unattested_inflight_loss: 60, // > window 40!
                        max_unreported_inflight_window: 40,
                    }],
                    capacity_leases: Vec::new(),
                    device_leases: Vec::new(),
                },
                cell_b_leases: CellLeaseReconciliationSpec::default(),
                semantic_convergence: SemanticPartitionConvergenceSpec {
                    additional_principals_for_a: Vec::new(),
                    additional_principals_for_b: Vec::new(),
                    verify_idempotent_second_replay: false,
                },
                artifact_convergence: ArtifactPartitionConvergenceSpec {
                    policies: Vec::new(),
                    cell_a_exports: Vec::new(),
                    cell_b_exports: Vec::new(),
                    cell_a_import_context: ArtifactImportCommitContext::default(),
                    cell_b_import_context: ArtifactImportCommitContext::default(),
                },
                record_federation_checkpoints: false,
            },
        );
        assert!(matches!(
            bad_res,
            Err(PartitionRecoveryError::BoundedLossViolation(_))
        ));
    }

    // Now reconcile all 3 quarantined leases on `worker_a2` with valid bounded-loss receipts
    // and verify idempotent duplicate replay produces ZERO double refund!
    {
        let mut w = WireWriter::new();
        w.put_fixed(lease_active.as_bytes());
        w.put_fixed(&[0xB1; 16]);
        w.put_u64(1);
        w.put_fixed(&GATEWAY_ID);
        w.put_u64(110);
        w.put_u64(170);
        w.put_u64(20);
        w.put_bool(false);
        let resp = worker_a2.rpc(OP_RECONCILE_QUOTA_DIRECT, &w.clone().into_vec());
        let mut r = WireReader::new(&resp);
        assert_eq!(r.get_u8(), RESP_OK);
        let avail_after_first = r.get_u64();
        assert_eq!(avail_after_first, 450 + 170);

        // Exact duplicate replay of the same `QuotaReconciliationReceipt` -> idempotent, zero extra refund!
        let resp_dup = worker_a2.rpc(OP_RECONCILE_QUOTA_DIRECT, &w.into_vec());
        let mut r_dup = WireReader::new(&resp_dup);
        assert_eq!(r_dup.get_u8(), RESP_OK);
        assert_eq!(r_dup.get_u64(), avail_after_first);
    }

    worker_a2.shutdown_cleanly();
    worker_b.shutdown_cleanly();
}

// =============================================================================
// H6 Test 3: Post-`SourceFenced` Migration Crash Quarantines Without Dual-Active
// =============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn road_c001_migration_crash_after_source_fenced_quarantines_without_dual_active_process() {
    let dir = TempDir::new("post-fence-quarantine-h6");
    let cell_a_root = dir.path().join("cell_a");
    let cell_b_root = dir.path().join("cell_b");
    let federation_root = dir.path().join("federation");
    let mailbox_a1 = dir.path().join("mailbox_a1");
    let mailbox_a2 = dir.path().join("mailbox_a2");
    let mailbox_b = dir.path().join("mailbox_b");

    let mut worker_a =
        CellWorkerHandle::spawn(CELL_A_DOMAIN, &cell_a_root, &federation_root, &mailbox_a1);
    let mut worker_b =
        CellWorkerHandle::spawn(CELL_B_DOMAIN, &cell_b_root, &federation_root, &mailbox_b);
    let cell_a_id = CellIdentity::from_domain(SchedulerDomainId::from_bytes(CELL_A_DOMAIN));

    // Seed migratable process + pinned-session restart image + 3-family leases on Cell A,
    // armed with `crash_after_source_fence = true`!
    let (binding, src_q, src_c, src_d, restart_img) = {
        let mut w = WireWriter::new();
        w.put_u8(0x55);
        w.put_bool(true); // with_device_lease
        w.put_bool(true); // downgraded restart-from-artifact mode (`[DIST-MIGRATE-002]`)
        w.put_bool(true); // crash_after_source_fence!
        let resp = worker_a.rpc(OP_SEED_MIGRATABLE_PROCESS, &w.into_vec());
        let mut r = WireReader::new(&resp);
        assert_eq!(r.get_u8(), RESP_OK);
        let proc_bind = r.get_binding();
        let quota_id = QuotaLeaseId::from_bytes(r.take_fixed::<16>());
        let cap_id = CapacityLeaseId::from_bytes(r.take_fixed::<16>());
        let dev_id = r
            .get_opt(|r| ExclusiveDeviceLeaseId::from_bytes(r.take_fixed::<16>()))
            .expect("device lease");
        let img = r
            .get_opt(|r| {
                (
                    ArtifactId::from_bytes(r.take_fixed::<16>()),
                    r.get_u64(),
                    r.take_fixed::<32>(),
                )
            })
            .expect("restart image");
        (proc_bind, quota_id, cap_id, dev_id, img)
    };

    // Drive migration from Target `worker_b`: Steps 1-5 complete (`SourceFenced` committed on A!),
    // Step 6 fails due to injected post-fence crash/disconnect, and `abort_post_fence` quarantines
    // both the coordinator (`QuarantinedPostFence`) and all 3 unsettled Source leases!
    {
        let mut w = WireWriter::new();
        w.put_str(mailbox_a1.to_str().expect("utf8"));
        w.put_str(federation_root.to_str().expect("utf8"));
        w.put_fixed(cell_a_id.as_bytes());
        w.put_u8(0x55);
        w.put_binding(&binding);
        w.put_fixed(src_q.as_bytes());
        w.put_fixed(&[0xC1; 16]);
        w.put_fixed(src_c.as_bytes());
        w.put_fixed(&[0xC2; 16]);
        w.put_opt(
            Some((src_d, ExclusiveDeviceLeaseId::from_bytes([0xC3; 16]))),
            |w, (sd, td)| {
                w.put_fixed(sd.as_bytes());
                w.put_fixed(td.as_bytes());
            },
        );
        w.put_opt(Some(restart_img), |w, (aid, rev, dig)| {
            w.put_fixed(aid.as_bytes());
            w.put_u64(rev);
            w.put_fixed(&dig);
        });
        w.put_bool(true); // expect_post_fence_quarantine!

        let resp = worker_b.rpc(OP_DRIVE_PROCESS_MIGRATION, &w.into_vec());
        let mut r = WireReader::new(&resp);
        let tag = r.get_u8();
        assert_eq!(
            tag,
            RESP_OK,
            "post-fence quarantine migration RPC failed: {}",
            r.get_str()
        );
        assert_eq!(r.get_quota_state(), QuotaLeaseState::Quarantined);
        assert_eq!(
            r.get_opt(WireReader::get_cap_state),
            Some(CapacityLeaseState::Quarantined)
        );
        assert_eq!(
            r.get_opt(WireReader::get_dev_state),
            Some(ExclusiveDeviceLeaseState::Quarantined)
        );
    }

    // Hard-kill Source `worker_a`, cold-restart `worker_a2` (`boot_gen = 2`), and verify
    // NEITHER Source NOR Target has an active process binding (zero dual-active process),
    // and all 3 Source leases remain durably `Quarantined`!
    let _ = worker_a.hard_kill_and_wait();
    let mut worker_a2 =
        CellWorkerHandle::spawn(CELL_A_DOMAIN, &cell_a_root, &federation_root, &mailbox_a2);

    let mut w = WireWriter::new();
    w.put_fixed(binding.process_id.as_bytes());
    w.put_u8(0x55);
    let resp_a = worker_a2.rpc(OP_INSPECT_PROCESS_STATE, &w.clone().into_vec());
    let mut ra = WireReader::new(&resp_a);
    assert_eq!(ra.get_u8(), RESP_OK);
    assert!(!ra.get_bool()); // NOT active on restarted Source!

    let resp_b = worker_b.rpc(OP_INSPECT_PROCESS_STATE, &w.into_vec());
    let mut rb = WireReader::new(&resp_b);
    assert_eq!(rb.get_u8(), RESP_OK);
    assert!(!rb.get_bool()); // NOT active on Target!

    let lease_snap_a = rpc_inspect_leases(&mut worker_a2, src_q, Some(src_c), Some(src_d));
    assert_eq!(lease_snap_a.quota_state, QuotaLeaseState::Quarantined);
    assert_eq!(
        lease_snap_a.cap_info.expect("cap").0,
        CapacityLeaseState::Quarantined
    );
    assert_eq!(
        lease_snap_a.dev_info.expect("dev").0,
        ExclusiveDeviceLeaseState::Quarantined
    );

    worker_a2.shutdown_cleanly();
    worker_b.shutdown_cleanly();
}

// =============================================================================
// H6 Test 4: Duplicate Messages, Offline History, & One-Shot ActionTokens
// =============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn road_c001_duplicate_messages_and_action_tokens_across_reconnection_are_idempotent_and_one_shot()
{
    let dir = TempDir::new("notify-action-token-h6");
    let cell_a_root = dir.path().join("cell_a");
    let cell_b_root = dir.path().join("cell_b");
    let federation_root = dir.path().join("federation");
    let mailbox_a1 = dir.path().join("mailbox_a1");
    let mailbox_a2 = dir.path().join("mailbox_a2");
    let mailbox_b = dir.path().join("mailbox_b");

    let mut worker_a =
        CellWorkerHandle::spawn(CELL_A_DOMAIN, &cell_a_root, &federation_root, &mailbox_a1);
    let mut worker_b =
        CellWorkerHandle::spawn(CELL_B_DOMAIN, &cell_b_root, &federation_root, &mailbox_b);

    let app_id = ApplicationId::from_bytes([0x71; 16]);
    let spoof_app_id = ApplicationId::from_bytes([0xEE; 16]);
    let task_id = TaskId::from_bytes([0x72; 16]);
    let wrong_task_id = TaskId::from_bytes([0xEF; 16]);
    let attempt_id = TaskAttemptId::from_bytes([0x73; 16]);

    // Publish 3 distinct transitions (`Proposed`, `Running`, `WaitingConsent`), replaying #2 twice!
    let mut topic_id = TopicId::from_bytes([0; 16]);
    let mut notif_id = NotificationId::from_bytes([0; 16]);
    for (idx, (trans_code, key_byte, occurred_at_ms)) in [
        (0_u8, 0x81_u8, 1_100_u64),
        (1, 0x82, 1_110),
        (1, 0x82, 1_110),
        (2, 0x83, 1_120),
    ]
    .into_iter()
    .enumerate()
    {
        let mut w = WireWriter::new();
        w.put_u8(0x70);
        w.put_str("road.c001.dup.notify");
        w.put_fixed(&[0x74; 16]);
        w.put_u64(1_000);
        w.put_fixed(app_id.as_bytes());
        w.put_fixed(task_id.as_bytes());
        w.put_opt(Some(attempt_id), |w, a| w.put_fixed(a.as_bytes()));
        w.put_u8(trans_code);
        w.put_fixed(&[key_byte; 32]);
        w.put_u64(occurred_at_ms);
        w.put_fixed(&[key_byte; 16]);
        w.put_opt::<u64>(None, WireWriter::put_u64);
        let resp = worker_a.rpc(OP_NOTIFY_PUBLISH_AND_POLL, &w.into_vec());
        let mut r = WireReader::new(&resp);
        assert_eq!(r.get_u8(), RESP_OK);
        topic_id = TopicId::from_bytes(r.take_fixed::<16>());
        notif_id = NotificationId::from_bytes(r.take_fixed::<16>());
        let is_replayed = r.get_bool();
        let seq = r.get_u64();
        if idx == 2 {
            assert!(is_replayed);
            assert_eq!(seq, 2);
        } else {
            assert!(!is_replayed);
        }
    }

    // Issue ActionToken for sequence 3, and verify idempotent replay of `issue_notification_action_token`!
    let issue_payload = {
        let mut w = WireWriter::new();
        w.put_fixed(topic_id.as_bytes());
        w.put_fixed(notif_id.as_bytes());
        w.put_u64(3);
        w.put_fixed(app_id.as_bytes());
        w.put_fixed(task_id.as_bytes());
        w.put_opt(Some(attempt_id), |w, a| w.put_fixed(a.as_bytes()));
        w.put_fixed(&[0x75; 16]);
        w.put_fixed(&[0x76; 32]);
        w.put_u64(1_000);
        w.put_u64(100_000);
        w.put_fixed(&[0x77; 16]);
        w.put_u64(1_200);
        w.into_vec()
    };
    let resp_issue1 = worker_a.rpc(OP_ISSUE_ACTION_TOKEN, &issue_payload);
    let mut ri1 = WireReader::new(&resp_issue1);
    assert_eq!(ri1.get_u8(), RESP_OK);
    assert!(!ri1.get_bool());
    let token_id = ri1.take_fixed::<16>();
    let secret = ri1.take_fixed::<32>();

    let resp_issue2 = worker_a.rpc(OP_ISSUE_ACTION_TOKEN, &issue_payload);
    let mut ri2 = WireReader::new(&resp_issue2);
    assert_eq!(ri2.get_u8(), RESP_OK);
    assert!(ri2.get_bool()); // Replayed!
    assert_eq!(ri2.take_fixed::<16>(), token_id);
    assert_eq!(ri2.take_fixed::<32>(), secret);

    // Hard-kill and cold-restart Cell A, then publish transition #4 (`Completed`), query offline
    // history with cursor pagination, and test all fail-closed + one-shot ActionToken paths!
    let _ = worker_a.hard_kill_and_wait();
    let mut worker_a2 =
        CellWorkerHandle::spawn(CELL_A_DOMAIN, &cell_a_root, &federation_root, &mailbox_a2);

    {
        let mut w = WireWriter::new();
        w.put_u8(0x70);
        w.put_str("road.c001.dup.notify");
        w.put_fixed(&[0x74; 16]);
        w.put_u64(1_000);
        w.put_fixed(app_id.as_bytes());
        w.put_fixed(task_id.as_bytes());
        w.put_opt(Some(attempt_id), |w, a| w.put_fixed(a.as_bytes()));
        w.put_u8(3); // Completed
        w.put_fixed(&[0x84; 32]);
        w.put_u64(1_300);
        w.put_fixed(&[0x84; 16]);
        w.put_opt(Some(4_u64), WireWriter::put_u64);
        let resp = worker_a2.rpc(OP_NOTIFY_PUBLISH_AND_POLL, &w.into_vec());
        let mut r = WireReader::new(&resp);
        assert_eq!(r.get_u8(), RESP_OK);
        let _ = r.take_fixed::<16>();
        let _ = r.take_fixed::<16>();
        assert!(!r.get_bool());
        assert_eq!(r.get_u64(), 4);
    }

    // Query `[DUI-OFFLINE-001]` cursor-paginated offline history across 2 pages of size 2:
    // sequences [1, 2] then [3, 4] with zero gaps and zero duplicates!
    {
        let mut w1 = WireWriter::new();
        w1.put_fixed(topic_id.as_bytes());
        w1.put_u64(0);
        w1.put_u64(2);
        let resp1 = worker_a2.rpc(OP_QUERY_OFFLINE_HISTORY, &w1.into_vec());
        let mut r1 = WireReader::new(&resp1);
        assert_eq!(r1.get_u8(), RESP_OK);
        assert_eq!(r1.get_u64(), 2);
        assert_eq!(r1.get_u64(), 1);
        let _ = r1.take_fixed::<16>();
        let _ = r1.take_fixed::<32>();
        assert_eq!(r1.get_u64(), 2);
        let _ = r1.take_fixed::<16>();
        let _ = r1.take_fixed::<32>();
        let cursor = r1.get_opt(WireReader::get_u64).expect("next cursor");
        assert_eq!(cursor, 2);
        assert!(r1.get_bool()); // has_more == true!

        let mut w2 = WireWriter::new();
        w2.put_fixed(topic_id.as_bytes());
        w2.put_u64(cursor);
        w2.put_u64(2);
        let resp2 = worker_a2.rpc(OP_QUERY_OFFLINE_HISTORY, &w2.into_vec());
        let mut r2 = WireReader::new(&resp2);
        assert_eq!(r2.get_u8(), RESP_OK);
        assert_eq!(r2.get_u64(), 2);
        assert_eq!(r2.get_u64(), 3);
        let _ = r2.take_fixed::<16>();
        let _ = r2.take_fixed::<32>();
        assert_eq!(r2.get_u64(), 4);
        let _ = r2.take_fixed::<16>();
        let _ = r2.take_fixed::<32>();
        let _ = r2.get_opt(WireReader::get_u64);
        assert!(!r2.get_bool()); // has_more == false!
    }

    // 1) Cross-app spoofing refused (`NOTIFY_ERR_CROSS_APP_SPOOF`) without burning token!
    {
        let mut w = WireWriter::new();
        w.put_fixed(&token_id);
        w.put_fixed(&secret);
        w.put_fixed(notif_id.as_bytes());
        w.put_fixed(spoof_app_id.as_bytes());
        w.put_fixed(task_id.as_bytes());
        w.put_u16(1);
        w.put_u64(1_400);
        let resp = worker_a2.rpc(OP_REDEEM_ACTION_TOKEN, &w.into_vec());
        let mut r = WireReader::new(&resp);
        assert_eq!(r.get_u8(), RESP_ERR);
        assert_eq!(r.get_u8(), NOTIFY_ERR_CROSS_APP_SPOOF);
    }
    // 2) Wrong task binding refused (`NOTIFY_ERR_BINDING_MISMATCH`) without burning token!
    {
        let mut w = WireWriter::new();
        w.put_fixed(&token_id);
        w.put_fixed(&secret);
        w.put_fixed(notif_id.as_bytes());
        w.put_fixed(app_id.as_bytes());
        w.put_fixed(wrong_task_id.as_bytes());
        w.put_u16(1);
        w.put_u64(1_410);
        let resp = worker_a2.rpc(OP_REDEEM_ACTION_TOKEN, &w.into_vec());
        let mut r = WireReader::new(&resp);
        assert_eq!(r.get_u8(), RESP_ERR);
        assert_eq!(r.get_u8(), NOTIFY_ERR_BINDING_MISMATCH);
    }
    // 3) Forged secret refused (`NOTIFY_ERR_PROOF_INVALID`) without burning token!
    {
        let mut w = WireWriter::new();
        w.put_fixed(&token_id);
        w.put_fixed(&[0xFF; 32]);
        w.put_fixed(notif_id.as_bytes());
        w.put_fixed(app_id.as_bytes());
        w.put_fixed(task_id.as_bytes());
        w.put_u16(1);
        w.put_u64(1_420);
        let resp = worker_a2.rpc(OP_REDEEM_ACTION_TOKEN, &w.into_vec());
        let mut r = WireReader::new(&resp);
        assert_eq!(r.get_u8(), RESP_ERR);
        assert_eq!(r.get_u8(), NOTIFY_ERR_PROOF_INVALID);
    }
    // 4) Valid presentation succeeds ONCE, and immediate duplicate presentation fails closed!
    let valid_redeem = {
        let mut w = WireWriter::new();
        w.put_fixed(&token_id);
        w.put_fixed(&secret);
        w.put_fixed(notif_id.as_bytes());
        w.put_fixed(app_id.as_bytes());
        w.put_fixed(task_id.as_bytes());
        w.put_u16(1);
        w.put_u64(1_430);
        w.into_vec()
    };
    let resp_ok = worker_a2.rpc(OP_REDEEM_ACTION_TOKEN, &valid_redeem);
    assert_eq!(WireReader::new(&resp_ok).get_u8(), RESP_OK);

    let resp_dup = worker_a2.rpc(OP_REDEEM_ACTION_TOKEN, &valid_redeem);
    let mut r_dup = WireReader::new(&resp_dup);
    assert_eq!(r_dup.get_u8(), RESP_ERR);
    assert_eq!(r_dup.get_u8(), NOTIFY_ERR_ALREADY_REDEEMED);

    worker_a2.shutdown_cleanly();
    worker_b.shutdown_cleanly();
}

// =============================================================================
// H6 Test 5: Direct `SliceKRuntime` & `CellHost` Live Service Wiring Proof
// =============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn slice_k_runtime_and_cell_host_wire_live_search_and_notify_surfaces() {
    let dir = TempDir::new("runtime-services-wiring");
    let rt_root = dir.path().join("slice_runtime");

    // 1. Open `SliceKRuntime`, run the longitudinal happy chain, append a real
    //    `OperationReceiptFact` into `SemanticAuthority`, and verify live
    //    `WaitAuthority` + `NotificationService` + `SearchService` over the
    //    exact same runtime!
    let (wait_id, notif_id, topic_id, event_id, ckpt) = {
        let tokio_rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("build test tokio runtime");
        let runtime = Arc::new(SliceKRuntime::open(&rt_root).expect("open SliceKRuntime"));
        let adapter =
            TokioRuntimeAdapter::new(tokio_rt.handle().clone(), TokioRuntimeConfig::default())
                .expect("create TokioRuntimeAdapter");
        let happy = tokio_rt
            .block_on(nlos_slice_k::run_happy_chain(&runtime, &adapter, 0x10))
            .expect("run happy chain");

        let receipt_id = ReceiptId::from_bytes([0x10_u8.wrapping_add(46); 16]);
        let appended = runtime
            .semantic_writer()
            .append_operation_receipt(
                &runtime,
                &OperationReceiptFact {
                    application_id: happy.application_id,
                    package_id: happy.package.package_id,
                    package_version: 1,
                    installation_generation: Generation::INITIAL,
                    entry_name: "bin/happy-chain",
                    artifact_id: happy.package.payload_artifact,
                    payload_revision: 1,
                    payload_digest: *happy.package.payload_digest.as_bytes(),
                    payload_size_bytes: 256,
                    operation_id: happy.outcome.operation.operation_id,
                    operation_generation: happy.outcome.operation.generation,
                    callback_id: CallbackId::from_bytes([0x10_u8.wrapping_add(45); 16]),
                    outcome: CompletionOutcome::Completed { receipt_id },
                    terminal_state: OperationState::Completed { receipt_id },
                },
            )
            .expect("append happy-chain semantic operation receipt");

        let sub = runtime
            .bootstrap_and_subscribe_notification_topic(
                &NotificationTopicConfig::for_application(happy.application_id),
                SubscriberKey::from_bytes([0x11; 16]),
                2_000,
            )
            .expect("bootstrap notification topic");

        // Register a live `WaitAuthority` delivery wait for sequence 1 BEFORE publishing!
        let wait_reg = runtime
            .notify()
            .register_delivery_wait(RegisterDeliveryWaitRequest {
                notification_id: sub.notification_id(),
                target_sequence: 1,
                binding: BindingId::from_bytes([0x14; 16]),
                idempotency_key: IdempotencyKey::from_bytes([0x15; 16]),
                registered_at_ms: 2_010,
            })
            .expect("register delivery wait");
        let wait_id = wait_reg.record().wait_id;
        assert_eq!(
            runtime
                .waits()
                .inspect_wait(wait_id)
                .expect("inspect pending")
                .state,
            WaitState::Pending
        );

        let pub_decision = runtime
            .publish_task_notification(
                sub.topic_id(),
                &TaskNotificationEnvelope {
                    task_id: happy.task_id,
                    application_id: happy.application_id,
                    task_generation: Generation::INITIAL,
                    transition: TaskTransitionKind::Completed,
                    occurred_at_ms: 2_020,
                    summary_digest: *happy.package.payload_digest.as_bytes(),
                    requires_user_action: false,
                },
                IdempotencyKey::from_bytes([0x16; 16]),
            )
            .expect("publish task notification");
        assert!(matches!(
            pub_decision,
            NotificationPublishDecision::Published(_)
        ));

        // `WaitAuthority` immediately transitions `Pending -> Woken`!
        assert_eq!(
            runtime
                .waits()
                .inspect_wait(wait_id)
                .expect("inspect woken")
                .state,
            WaitState::Woken
        );

        // `SearchService` discovers the committed happy-chain semantic receipt fact as
        // `NON_AUTHORITATIVE` and verifies it against a signed vector checkpoint!
        let cand_page = runtime
            .query_non_authoritative_candidates(&PaginatedAssertionSelector::new(
                AssertionSelector::new(16),
            ))
            .expect("query non-authoritative candidates");
        assert_eq!(cand_page.items.len(), 1);
        let hit = &cand_page.items[0];
        assert_eq!(
            hit.use_restrictions,
            SearchUseRestrictions::NON_AUTHORITATIVE
        );
        assert_eq!(hit.hit.event_id, appended.event_id);

        let ckpt_now_ms = runtime
            .wall_now_ms(seeded_key(0x10, 99))
            .expect("wall clock")
            + 5_000;
        let ckpt = runtime
            .export_signed_vector_checkpoint([0x99; 32], ckpt_now_ms)
            .expect("export signed vector checkpoint");
        let ckpt_page = runtime
            .query_at_signed_checkpoint(&ckpt, &AssertionSelector::new(16), None)
            .expect("query at signed checkpoint");
        assert_eq!(ckpt_page.items.len(), 1);
        let verified = runtime
            .verify_candidate_against_checkpoint(&ckpt_page.items[0], &ckpt)
            .expect("verify candidate against checkpoint");
        assert_eq!(verified.event_id, appended.event_id);

        (
            wait_id,
            sub.notification_id(),
            sub.topic_id(),
            appended.event_id,
            ckpt,
        )
    };

    // 2. Reopen `SliceKRuntime` on the exact same root and confirm `<root>/waits`,
    //    `<root>/notify`, and `<root>/semantic` all persisted durably (`WAL + FULL`)!
    assert!(rt_root.join("waits").exists());
    assert!(rt_root.join("notify").exists());
    assert!(rt_root.join("semantic").exists());
    {
        let reopened = SliceKRuntime::open(&rt_root).expect("reopen SliceKRuntime");
        assert_eq!(
            reopened
                .waits()
                .inspect_wait(wait_id)
                .expect("rehydrated wait")
                .state,
            WaitState::Woken
        );
        let polled = reopened
            .poll_task_notifications(notif_id, 8)
            .expect("poll rehydrated task notifications");
        assert_eq!(polled.len(), 1);
        let hist = reopened
            .query_offline_notification_history(topic_id, 0, 8)
            .expect("query rehydrated offline history");
        assert_eq!(hist.entries.len(), 1);
        let ckpt_page = reopened
            .query_at_signed_checkpoint(&ckpt, &AssertionSelector::new(16), None)
            .expect("query rehydrated checkpoint search");
        assert_eq!(ckpt_page.items.len(), 1);
        assert_eq!(ckpt_page.items[0].hit.event_id, event_id);
    }

    // 3. Also verify `CellHost` live service surfaces in a dedicated child OS process!
    let cell_root = dir.path().join("cell_host_service");
    let fed_root = dir.path().join("cell_host_fed");
    let mbx_root = dir.path().join("cell_host_mbx");
    let mut cell_worker = CellWorkerHandle::spawn([0xC5; 16], &cell_root, &fed_root, &mbx_root);
    let cell_eid = rpc_append_semantic_fact(&mut cell_worker, 0x91, 1, 3_000);
    let cell_shard = rpc_export_search_shard(&mut cell_worker, None, 16);
    let cell_fed_report = aggregate_federated_cell_candidates(&[cell_shard], 16, false)
        .expect("federated report over CellHost shard");
    assert_eq!(cell_fed_report.candidates.len(), 1);
    assert_eq!(cell_fed_report.candidates[0].event_id, cell_eid);
    cell_worker.shutdown_cleanly();
}
