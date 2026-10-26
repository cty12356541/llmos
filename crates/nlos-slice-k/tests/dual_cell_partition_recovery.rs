//! `W65` (`C-PARTITION`): Dual-live-OS-process partition autonomy, bounded-loss
//! lease reconciliation, signed vector-checkpoint semantic convergence, and
//! content-type-policy artifact replica convergence integration tests
//! (`[DIST-LOCAL-001]`, `[DIST-FAIL-001]`, `[LEASE-LOSS-001]`,
//! `[LEASE-FENCE-001]`, `[LEASE-RECON-001]`, `[ROAD-C-001]`,
//! `[DIST-EVENT-001]`, `[DIST-AUDIT-001]`, `[SEM-CHECKPOINT-001]`,
//! `[DIST-ART-001]`).
//!
//! Per `ADR-0018` (one OS process = one Cell), each top-level `#[test]` spawns
//! **two live child OS processes** (`child_partition_cell_worker`) that remain
//! alive simultaneously before, throughout, and after the simulated
//! control-plane partition cut — never terminated via `kill` to fake a
//! partition. Both live child OS processes advance independent heartbeats in
//! the shared `CellDirectory` (`ADR-0019`), execute bounded local autonomy
//! spends against their local `CellHost` authorities, and converge across an
//! atomic file-mailbox IPC [`CellPartitionEndpoint`] driven by
//! [`drive_partition_reconnection_convergence`].

#![allow(clippy::similar_names)]

use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use nlos_artifact::{
    ArtifactConflictReceipt, ArtifactConflictStrategy, ArtifactDeletionSemantics,
    ArtifactMergeStrategy, ArtifactReplicaAuthority, ArtifactSyncPolicy, ArtifactTransferPackage,
    ContentDigest, CreateArtifactSpec, HeadState, ProvenanceSourceTriple, PutRevisionRequest,
};
use nlos_cell::{
    CellDirectory, CellEpoch, CellFence, CellFencingToken, CellIdentity,
    CheckpointDigestVerification, CheckpointPrefix, ControlPlaneLinkPhase, ControlPlaneLinkState,
    FailureDetectorConfig, LocalAutonomyProof, PartitionViolation, PartitionedActionKind,
    RemoteCallContract, RemoteCallOutcome, RemoteRetryDirective, RemoteRetryRefusal,
};
use nlos_identity::{BootstrapPrincipalRequest, KeyPurpose};
use nlos_lease::{
    CapacityLeaseState, CleanupAttestation, DeviceResetReceipt, ExclusiveDeviceLeaseState,
    LeaseInstant, QuotaLeaseState,
};
use nlos_operation::{CompletionOutcome, OperationState};
use nlos_semantic::{
    CrossDomainEventItem, DomainSemanticWaterMark, SemanticVectorCheckpoint,
    SignedSemanticVectorCheckpoint, VectorCheckpointRelation,
};
use nlos_slice_k::{
    ArtifactImportCommitContext, ArtifactPartitionConvergenceSpec, CapacityPartitionSettlementSpec,
    CellHost, CellHostConfig, CellLeaseReconciliationSpec, CellPartitionEndpoint,
    CellPartitionSpendError, DevicePartitionSettlementSpec, LocalCellHostPartitionEndpoint,
    OperationReceiptFact, PartitionArtifactImportSummary, PartitionEpochAdvanceSnapshot,
    PartitionRecordedCheckpointSummary, PartitionRecoveryError, PartitionRecoveryRequest,
    PartitionSettledCapacitySummary, PartitionSettledDeviceSummary,
    QuotaLeaseReconciliationSummary, QuotaPartitionSettlementSpec, SemanticConvergenceStepSnapshot,
    SemanticPartitionConvergenceSpec, SliceKError, decode_artifact_transfer_package,
    decode_cell_fence, decode_cross_domain_event_items, decode_partition_recovery_checkpoint_entry,
    decode_signed_vector_checkpoint, drive_partition_reconnection_convergence,
    encode_artifact_transfer_package, encode_cell_fence, encode_cross_domain_event_items,
    encode_signed_vector_checkpoint, partition_recovery_checkpoint_prefix,
};
use nlos_types::{
    ApplicationId, ArtifactId, CallbackId, CapacityLeaseId, CommitPermitId, DeviceId,
    ExclusiveDeviceLeaseId, Generation, IdempotencyKey, OperationId, PackageId, QuotaLeaseId,
    ReceiptId, SchedulerDomainId, SemanticEventId, TaskId,
};

const ENV_WORKER_ROLE: &str = "LLMOS_W65_PARTITION_WORKER_ROLE";
const ENV_CELL_ROOT: &str = "LLMOS_W65_CELL_ROOT";
const ENV_FEDERATION_ROOT: &str = "LLMOS_W65_FEDERATION_ROOT";
const ENV_MAILBOX_DIR: &str = "LLMOS_W65_MAILBOX_DIR";

const CELL_ALPHA_DOMAIN: [u8; 16] = [0xA5; 16];
const CELL_BETA_DOMAIN: [u8; 16] = [0xB6; 16];
const DEVICE_ALPHA_BYTES: [u8; 16] = [0xD1; 16];
const DEVICE_BETA_BYTES: [u8; 16] = [0xD2; 16];
const GATEWAY_ID: [u8; 16] = [0xE7; 16];

const INITIAL_QUOTA_POOL: u64 = 1_000;
const INITIAL_CAPACITY_POOL: u64 = 700;

// Worker IPC opcodes
const OP_PING_AND_HEARTBEAT: u8 = 1;
const OP_CURRENT_FENCE: u8 = 2;
const OP_CONNECTIVITY_STATE: u8 = 3;
const OP_ENTER_PARTITION: u8 = 4;
const OP_BEGIN_RECONNECTION: u8 = 5;
const OP_COMPLETE_RECONNECTION: u8 = 6;
const OP_ADVANCE_EPOCH: u8 = 7;
const OP_QUOTA_AVAILABLE: u8 = 8;
const OP_GRANT_LEASES: u8 = 9;
const OP_LOCAL_AUTONOMY_SPEND: u8 = 10;
const OP_CHECK_CONTROL_PLANE_ACTION: u8 = 11;
const OP_SETTLE_QUOTA_LEASE: u8 = 12;
const OP_SETTLE_CAPACITY_LEASE: u8 = 13;
const OP_SETTLE_DEVICE_LEASE: u8 = 14;
const OP_EXPORT_WRITER_PRINCIPAL: u8 = 15;
const OP_REGISTER_PEER_PRINCIPAL: u8 = 16;
const OP_EXPORT_SIGNED_VECTOR_CKPT: u8 = 17;
const OP_VERIFY_PEER_VECTOR_CKPT: u8 = 18;
const OP_EXPORT_XDOM_EVENTS: u8 = 19;
const OP_CONVERGE_XDOM_EVENTS: u8 = 20;
const OP_APPEND_SEMANTIC_RECEIPT: u8 = 21;
const OP_PUBLISH_LOCAL_ARTIFACT: u8 = 22;
const OP_EXPORT_ARTIFACT_PKG: u8 = 23;
const OP_IMPORT_ARTIFACT_PKG: u8 = 24;
const OP_INSPECT_LOCAL_ARTIFACT: u8 = 25;
const OP_RECORD_CHECKPOINT: u8 = 26;
const OP_INJECT_STALE_EPOCH_PROBES: u8 = 27;
const OP_SHUTDOWN: u8 = 255;

const RESP_OK: u8 = 0;
const RESP_ERR: u8 = 1;

// Typed Spend Refusal Codes over Worker Test RPC
const SPEND_ERR_CAP_UNPROVABLE: u8 = 1;
const SPEND_ERR_CAP_EXPIRED: u8 = 2;
const SPEND_ERR_LEASE_EXPIRED: u8 = 3;
const SPEND_ERR_INSUFFICIENT_BALANCE: u8 = 4;
const SPEND_ERR_FENCE_MISMATCH: u8 = 5;
const SPEND_ERR_OTHER: u8 = 9;

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
        let root = std::env::temp_dir().join(format!(
            "nlos-w65-partition-{tag}-{}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("create w65 temp directory");
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

fn write_atomic_file(target: &Path, bytes: &[u8]) {
    let tmp = target.with_extension("tmp");
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
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(err) => {
                assert!(
                    Instant::now() < deadline,
                    "failed reading IPC frame {}: {err}",
                    target.display()
                );
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }
}

// =============================================================================
// Compact Deterministic Binary Wire Framing for Test RPCs
// =============================================================================

struct FrameWriter {
    buf: Vec<u8>,
}

impl FrameWriter {
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

    fn put_usize(&mut self, v: usize) {
        self.put_u64(u64::try_from(v).unwrap_or(u64::MAX));
    }

    fn put_fixed(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    fn put_bytes(&mut self, bytes: &[u8]) {
        self.put_usize(bytes.len());
        self.buf.extend_from_slice(bytes);
    }

    fn put_str(&mut self, s: &str) {
        self.put_bytes(s.as_bytes());
    }

    fn put_opt_u64(&mut self, v: Option<u64>) {
        match v {
            None => self.put_u8(0),
            Some(x) => {
                self.put_u8(1);
                self.put_u64(x);
            }
        }
    }

    fn put_head(&mut self, head: HeadState) {
        self.put_u64(head.revision);
        self.put_fixed(head.digest.as_bytes());
    }

    fn put_opt_head(&mut self, head: Option<HeadState>) {
        match head {
            None => self.put_u8(0),
            Some(h) => {
                self.put_u8(1);
                self.put_head(h);
            }
        }
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
        let code = match state {
            QuotaLeaseState::Issued => 0,
            QuotaLeaseState::Active => 1,
            QuotaLeaseState::Closing => 2,
            QuotaLeaseState::Closed => 3,
            QuotaLeaseState::Cancelled => 4,
            QuotaLeaseState::Fenced => 5,
            QuotaLeaseState::Quarantined => 6,
        };
        self.put_u8(code);
    }

    fn put_cap_state(&mut self, state: CapacityLeaseState) {
        let code = match state {
            CapacityLeaseState::GlobalReserved => 0,
            CapacityLeaseState::TargetPrepared => 1,
            CapacityLeaseState::Active => 2,
            CapacityLeaseState::Returning => 3,
            CapacityLeaseState::Returned => 4,
            CapacityLeaseState::Fenced => 5,
            CapacityLeaseState::Quarantined => 6,
        };
        self.put_u8(code);
    }

    fn put_dev_state(&mut self, state: ExclusiveDeviceLeaseState) {
        let code = match state {
            ExclusiveDeviceLeaseState::DeviceReserved => 0,
            ExclusiveDeviceLeaseState::Resetting => 1,
            ExclusiveDeviceLeaseState::Fencing => 2,
            ExclusiveDeviceLeaseState::Quarantined => 3,
            ExclusiveDeviceLeaseState::Returned => 4,
        };
        self.put_u8(code);
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
        self.put_usize(ckpt.domain_watermarks.len());
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

    fn put_sync_policy(&mut self, p: &ArtifactSyncPolicy) {
        self.put_str(&p.content_type);
        self.put_u8(match p.merge_strategy {
            ArtifactMergeStrategy::ImmutableContentAddressed => 0,
            ArtifactMergeStrategy::FastForwardLineageOnly => 1,
            ArtifactMergeStrategy::AuthoritativeReplicaWins => 2,
        });
        self.put_u8(match p.conflict_strategy {
            ArtifactConflictStrategy::FailClosedConflict => 0,
            ArtifactConflictStrategy::QuarantineDivergentBranch => 1,
            ArtifactConflictStrategy::DeferToAuthoritativeReplica => 2,
        });
        match p.replica_authority {
            ArtifactReplicaAuthority::SymmetricPeer => self.put_u8(0),
            ArtifactReplicaAuthority::SingleAuthoritativeCell {
                authoritative_cell_id,
            } => {
                self.put_u8(1);
                self.put_fixed(&authoritative_cell_id);
            }
        }
        self.put_u8(match p.deletion_semantics {
            ArtifactDeletionSemantics::Forbidden => 0,
            ArtifactDeletionSemantics::TombstoneRevisionRequired => 1,
            ArtifactDeletionSemantics::GcAfterRetentionBarrier => 2,
        });
    }

    fn put_artifact_import_summary(&mut self, s: &PartitionArtifactImportSummary) {
        match s {
            PartitionArtifactImportSummary::ImportedFastForward {
                artifact_id,
                prior_local_head,
                new_head,
                imported_revisions,
            } => {
                self.put_u8(0);
                self.put_fixed(artifact_id.as_bytes());
                self.put_opt_head(*prior_local_head);
                self.put_head(*new_head);
                self.put_usize(imported_revisions.len());
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
                self.put_u8(match conflict_receipt.strategy {
                    ArtifactConflictStrategy::FailClosedConflict => 0,
                    ArtifactConflictStrategy::QuarantineDivergentBranch => 1,
                    ArtifactConflictStrategy::DeferToAuthoritativeReplica => 2,
                });
                self.put_fixed(conflict_receipt.conflict_digest.as_bytes());
                self.put_bool(*preserved_both_blobs);
                self.put_usize(pinned_digests.len());
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
}

struct FrameReader<'a> {
    rest: &'a [u8],
}

impl<'a> FrameReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { rest: bytes }
    }

    fn take_u8(&mut self) -> u8 {
        let (&b, tail) = self.rest.split_first().expect("frame u8");
        self.rest = tail;
        b
    }

    fn take_bool(&mut self) -> bool {
        self.take_u8() != 0
    }

    fn take_fixed<const N: usize>(&mut self) -> [u8; N] {
        let (head, tail) = self.rest.split_at(N);
        self.rest = tail;
        head.try_into().expect("fixed slice")
    }

    fn take_u16(&mut self) -> u16 {
        u16::from_be_bytes(self.take_fixed::<2>())
    }

    fn take_u32(&mut self) -> u32 {
        u32::from_be_bytes(self.take_fixed::<4>())
    }

    fn take_u64(&mut self) -> u64 {
        u64::from_be_bytes(self.take_fixed::<8>())
    }

    fn take_usize(&mut self) -> usize {
        usize::try_from(self.take_u64()).expect("usize frame")
    }

    fn take_vec(&mut self) -> Vec<u8> {
        let len = self.take_usize();
        let (head, tail) = self.rest.split_at(len);
        self.rest = tail;
        head.to_vec()
    }

    fn take_string(&mut self) -> String {
        String::from_utf8(self.take_vec()).expect("utf8 string")
    }

    fn take_opt_u64(&mut self) -> Option<u64> {
        match self.take_u8() {
            0 => None,
            _ => Some(self.take_u64()),
        }
    }

    fn take_fence(&mut self) -> CellFence {
        decode_cell_fence(&self.take_fixed::<40>()).expect("valid cell fence")
    }

    fn take_head(&mut self) -> HeadState {
        HeadState {
            revision: self.take_u64(),
            digest: ContentDigest::from_bytes(self.take_fixed::<32>()),
        }
    }

    fn take_opt_head(&mut self) -> Option<HeadState> {
        match self.take_u8() {
            0 => None,
            _ => Some(self.take_head()),
        }
    }

    fn take_link_state(&mut self) -> ControlPlaneLinkState {
        match self.take_u8() {
            0 => ControlPlaneLinkState::Connected {
                verified_fence: self.take_fence(),
                last_contact_tick: self.take_u64(),
            },
            1 => ControlPlaneLinkState::Partitioned {
                frozen_fence: self.take_fence(),
                partitioned_since_tick: self.take_u64(),
                reason_code: self.take_u16(),
            },
            2 => ControlPlaneLinkState::Reconnecting {
                prior_frozen_fence: self.take_fence(),
                candidate_fence: self.take_fence(),
                started_tick: self.take_u64(),
            },
            other => panic!("invalid link state tag: {other}"),
        }
    }

    fn take_quota_state(&mut self) -> QuotaLeaseState {
        match self.take_u8() {
            0 => QuotaLeaseState::Issued,
            1 => QuotaLeaseState::Active,
            2 => QuotaLeaseState::Closing,
            3 => QuotaLeaseState::Closed,
            4 => QuotaLeaseState::Cancelled,
            5 => QuotaLeaseState::Fenced,
            6 => QuotaLeaseState::Quarantined,
            other => panic!("invalid quota state tag: {other}"),
        }
    }

    fn take_cap_state(&mut self) -> CapacityLeaseState {
        match self.take_u8() {
            0 => CapacityLeaseState::GlobalReserved,
            1 => CapacityLeaseState::TargetPrepared,
            2 => CapacityLeaseState::Active,
            3 => CapacityLeaseState::Returning,
            4 => CapacityLeaseState::Returned,
            5 => CapacityLeaseState::Fenced,
            6 => CapacityLeaseState::Quarantined,
            other => panic!("invalid capacity state tag: {other}"),
        }
    }

    fn take_dev_state(&mut self) -> ExclusiveDeviceLeaseState {
        match self.take_u8() {
            0 => ExclusiveDeviceLeaseState::DeviceReserved,
            1 => ExclusiveDeviceLeaseState::Resetting,
            2 => ExclusiveDeviceLeaseState::Fencing,
            3 => ExclusiveDeviceLeaseState::Quarantined,
            4 => ExclusiveDeviceLeaseState::Returned,
            other => panic!("invalid device state tag: {other}"),
        }
    }

    fn take_bootstrap_principal(&mut self) -> BootstrapPrincipalRequest {
        let principal_profile_digest = self.take_fixed::<32>();
        let control_domain_policy_digest = self.take_fixed::<32>();
        let public_key = self.take_fixed::<32>();
        let key_purpose = match self.take_u8() {
            1 => KeyPurpose::SemanticSigning,
            2 => KeyPurpose::BarrierObservationSigning,
            other => panic!("invalid key purpose tag: {other}"),
        };
        let key_valid_from_ms = self.take_u64();
        let key_valid_until_ms = self.take_u64();
        let idempotency_key = IdempotencyKey::from_bytes(self.take_fixed::<16>());
        let created_at_ms = self.take_u64();
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

    fn take_vector_checkpoint(&mut self) -> SemanticVectorCheckpoint {
        let checkpoint_id = self.take_fixed::<16>();
        let count = self.take_usize();
        let mut domain_watermarks = Vec::with_capacity(count);
        for _ in 0..count {
            domain_watermarks.push(DomainSemanticWaterMark {
                authority_id: self.take_fixed::<16>(),
                event_count: self.take_u64(),
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

    fn take_sync_policy(&mut self) -> ArtifactSyncPolicy {
        let content_type = self.take_string();
        let merge_strategy = match self.take_u8() {
            0 => ArtifactMergeStrategy::ImmutableContentAddressed,
            1 => ArtifactMergeStrategy::FastForwardLineageOnly,
            2 => ArtifactMergeStrategy::AuthoritativeReplicaWins,
            other => panic!("invalid merge strategy: {other}"),
        };
        let conflict_strategy = match self.take_u8() {
            0 => ArtifactConflictStrategy::FailClosedConflict,
            1 => ArtifactConflictStrategy::QuarantineDivergentBranch,
            2 => ArtifactConflictStrategy::DeferToAuthoritativeReplica,
            other => panic!("invalid conflict strategy: {other}"),
        };
        let replica_authority = match self.take_u8() {
            0 => ArtifactReplicaAuthority::SymmetricPeer,
            1 => ArtifactReplicaAuthority::SingleAuthoritativeCell {
                authoritative_cell_id: self.take_fixed::<16>(),
            },
            other => panic!("invalid replica authority: {other}"),
        };
        let deletion_semantics = match self.take_u8() {
            0 => ArtifactDeletionSemantics::Forbidden,
            1 => ArtifactDeletionSemantics::TombstoneRevisionRequired,
            2 => ArtifactDeletionSemantics::GcAfterRetentionBarrier,
            other => panic!("invalid deletion semantics: {other}"),
        };
        ArtifactSyncPolicy {
            content_type,
            merge_strategy,
            conflict_strategy,
            replica_authority,
            deletion_semantics,
        }
    }

    fn take_artifact_import_summary(&mut self) -> PartitionArtifactImportSummary {
        match self.take_u8() {
            0 => {
                let artifact_id = ArtifactId::from_bytes(self.take_fixed::<16>());
                let prior_local_head = self.take_opt_head();
                let new_head = self.take_head();
                let count = self.take_usize();
                let mut imported_revisions = Vec::with_capacity(count);
                for _ in 0..count {
                    imported_revisions.push(self.take_u64());
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
                head: self.take_head(),
            },
            2 => {
                let artifact_id = ArtifactId::from_bytes(self.take_fixed::<16>());
                let content_type = self.take_string();
                let local_head = self.take_head();
                let remote_head = self.take_head();
                let origin_cell_id = self.take_fixed::<16>();
                let strategy = match self.take_u8() {
                    0 => ArtifactConflictStrategy::FailClosedConflict,
                    1 => ArtifactConflictStrategy::QuarantineDivergentBranch,
                    2 => ArtifactConflictStrategy::DeferToAuthoritativeReplica,
                    other => panic!("invalid conflict strategy: {other}"),
                };
                let conflict_digest = ContentDigest::from_bytes(self.take_fixed::<32>());
                let preserved_both_blobs = self.take_bool();
                let count = self.take_usize();
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
                tombstone_head: self.take_head(),
            },
            other => panic!("invalid artifact import summary tag: {other}"),
        }
    }
}

// =============================================================================
// Child OS Process Worker (`ADR-0018`: One OS Process = One Live CellHost)
// =============================================================================

fn detector_config() -> FailureDetectorConfig {
    FailureDetectorConfig::new(
        NonZeroU64::new(3).expect("suspect threshold"),
        NonZeroU64::new(5).expect("dead threshold"),
    )
    .expect("valid detector threshold order")
}

#[test]
#[ignore = "spawned as live child OS process by dual_cell_partition_recovery coordinator tests"]
fn child_partition_cell_worker() {
    // Batch `--ignored`/`--include-ignored` runs (nightly scale-probe job)
    // reach this entry without env; only the parent spawn makes it meaningful.
    let (Ok(role), Ok(cell_root), Ok(federation_root), Ok(mailbox_dir)) = (
        std::env::var(ENV_WORKER_ROLE),
        std::env::var(ENV_CELL_ROOT),
        std::env::var(ENV_FEDERATION_ROOT),
        std::env::var(ENV_MAILBOX_DIR),
    ) else {
        eprintln!(
            "skipped: {ENV_WORKER_ROLE}/{ENV_CELL_ROOT}/{ENV_FEDERATION_ROOT}/{ENV_MAILBOX_DIR} unset (harness entry, parent spawn only)"
        );
        return;
    };
    let cell_root = PathBuf::from(cell_root);
    let federation_root = PathBuf::from(federation_root);
    let mailbox_dir = PathBuf::from(mailbox_dir);

    let (domain_bytes, device_bytes) = match role.as_str() {
        "alpha" => (CELL_ALPHA_DOMAIN, DEVICE_ALPHA_BYTES),
        "beta" => (CELL_BETA_DOMAIN, DEVICE_BETA_BYTES),
        other => panic!("unexpected worker role: {other}"),
    };

    let config = CellHostConfig::new(
        SchedulerDomainId::from_bytes(domain_bytes),
        INITIAL_QUOTA_POOL,
        INITIAL_CAPACITY_POOL,
        DeviceId::from_bytes(device_bytes),
        detector_config(),
    );

    let mut host = CellHost::open_federated(&cell_root, config, &federation_root)
        .expect("open federated host");

    // Publish ready banner with our real OS PID + initial fence.
    let mut ready = FrameWriter::new();
    ready.put_u32(std::process::id());
    ready.put_fixed(&encode_cell_fence(host.fence()));
    write_atomic_file(&mailbox_dir.join("ready.bin"), &ready.into_vec());

    let mut seq: u64 = 1;
    loop {
        let req_path = mailbox_dir.join(format!("req-{seq:06}.bin"));
        let resp_path = mailbox_dir.join(format!("resp-{seq:06}.bin"));
        let req_bytes = wait_for_file_and_consume(&req_path, Duration::from_secs(60));
        let mut reader = FrameReader::new(&req_bytes);
        let opcode = reader.take_u8();
        if opcode == OP_SHUTDOWN {
            let mut ok = FrameWriter::new();
            ok.put_u8(RESP_OK);
            ok.put_u32(std::process::id());
            write_atomic_file(&resp_path, &ok.into_vec());
            drop(host);
            return;
        }
        let response = handle_worker_command(&mut host, opcode, &mut reader);
        write_atomic_file(&resp_path, &response);
        seq += 1;
    }
}

#[allow(clippy::too_many_lines)]
fn handle_worker_command(host: &mut CellHost, opcode: u8, reader: &mut FrameReader<'_>) -> Vec<u8> {
    let mut out = FrameWriter::new();
    match opcode {
        OP_PING_AND_HEARTBEAT => {
            let entry = host
                .refresh_cell_registration()
                .expect("refresh registration")
                .expect("federated registration present");
            out.put_u8(RESP_OK);
            out.put_u32(std::process::id());
            out.put_fixed(&encode_cell_fence(host.fence()));
            out.put_link_state(host.connectivity_state());
            out.put_u64(entry.heartbeat());
            out.put_u64(host.quota_available());
            out.put_u64(host.capacity_pool_remaining());
            out.put_bool(host.device_head_free());
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
            let tick = reader.take_u64();
            let reason_code = reader.take_u16();
            match host.enter_control_plane_partition(tick, reason_code) {
                Ok(state) => {
                    out.put_u8(RESP_OK);
                    out.put_link_state(state);
                }
                Err(err) => {
                    out.put_u8(RESP_ERR);
                    out.put_str(&err.to_string());
                }
            }
        }
        OP_BEGIN_RECONNECTION => {
            let tick = reader.take_u64();
            let mut ep = LocalCellHostPartitionEndpoint::new(host);
            match ep.begin_reconnection(tick) {
                Ok(state) => {
                    out.put_u8(RESP_OK);
                    out.put_link_state(state);
                }
                Err(err) => {
                    out.put_u8(RESP_ERR);
                    out.put_str(&err.to_string());
                }
            }
        }
        OP_COMPLETE_RECONNECTION => {
            let tick = reader.take_u64();
            let mut ep = LocalCellHostPartitionEndpoint::new(host);
            match ep.complete_reconnection(tick) {
                Ok(state) => {
                    out.put_u8(RESP_OK);
                    out.put_link_state(state);
                }
                Err(err) => {
                    out.put_u8(RESP_ERR);
                    out.put_str(&err.to_string());
                }
            }
        }
        OP_ADVANCE_EPOCH => {
            let mut ep = LocalCellHostPartitionEndpoint::new(host);
            match ep.advance_cell_epoch() {
                Ok(snap) => {
                    out.put_u8(RESP_OK);
                    out.put_fixed(&encode_cell_fence(snap.fence));
                    out.put_usize(snap.entries_fenced);
                }
                Err(err) => {
                    out.put_u8(RESP_ERR);
                    out.put_str(&err.to_string());
                }
            }
        }
        OP_QUOTA_AVAILABLE => {
            out.put_u8(RESP_OK);
            out.put_u64(host.quota_available());
        }
        OP_GRANT_LEASES => {
            let quota_id = QuotaLeaseId::from_bytes(reader.take_fixed::<16>());
            let quota_face = reader.take_u64();
            let expires_at = reader.take_opt_u64().map(LeaseInstant::new);
            let initial_pre_cut_spend = reader.take_u64();
            let has_cap = reader.take_bool();
            let cap_spec = if has_cap {
                Some((
                    CapacityLeaseId::from_bytes(reader.take_fixed::<16>()),
                    reader.take_u64(),
                ))
            } else {
                None
            };
            let has_dev = reader.take_bool();
            let dev_id = if has_dev {
                Some(ExclusiveDeviceLeaseId::from_bytes(
                    reader.take_fixed::<16>(),
                ))
            } else {
                None
            };

            let _ = host
                .grant_quota_lease(quota_id, quota_face, expires_at)
                .expect("grant quota lease on worker");
            if initial_pre_cut_spend > 0 {
                let _ = host
                    .activate_quota_lease(quota_id)
                    .expect("activate quota lease");
                let _ = host
                    .report_quota_usage(quota_id, initial_pre_cut_spend)
                    .expect("report pre-cut usage");
            }
            if let Some((cid, amount)) = cap_spec {
                let _ = host
                    .grant_capacity_lease(cid, amount)
                    .expect("grant capacity lease on worker");
            }
            if let Some(did) = dev_id {
                let _ = host
                    .grant_device_lease(did)
                    .expect("grant device lease on worker");
            }
            out.put_u8(RESP_OK);
            out.put_u64(host.quota_available());
            out.put_u64(host.capacity_pool_remaining());
            out.put_bool(host.device_head_free());
        }
        OP_LOCAL_AUTONOMY_SPEND => {
            let now_tick = reader.take_u64();
            let lease_id = QuotaLeaseId::from_bytes(reader.take_fixed::<16>());
            let spend_delta = reader.take_u64();
            let cap_expires = reader.take_u64();
            let cap_revoked = reader.take_bool();
            let cap_verified = reader.take_bool();
            match host.spend_quota_lease_with_local_autonomy(
                now_tick,
                lease_id,
                spend_delta,
                cap_expires,
                cap_revoked,
                cap_verified,
            ) {
                Ok((receipt, grant)) => {
                    out.put_u8(RESP_OK);
                    out.put_fixed(&receipt.receipt_digest());
                    out.put_u64(grant.spent());
                    out.put_u64(grant.remaining());
                }
                Err(err) => {
                    out.put_u8(RESP_ERR);
                    let code = match err {
                        CellPartitionSpendError::Partition(
                            PartitionViolation::CapabilityNotLocallyProvable { .. },
                        ) => SPEND_ERR_CAP_UNPROVABLE,
                        CellPartitionSpendError::Partition(
                            PartitionViolation::CapabilityExpiredDuringPartition { .. },
                        ) => SPEND_ERR_CAP_EXPIRED,
                        CellPartitionSpendError::Partition(
                            PartitionViolation::QuotaLeaseExpiredDuringPartition { .. },
                        ) => SPEND_ERR_LEASE_EXPIRED,
                        CellPartitionSpendError::Partition(
                            PartitionViolation::QuotaFrozenInsufficientRemaining { .. },
                        ) => SPEND_ERR_INSUFFICIENT_BALANCE,
                        CellPartitionSpendError::Partition(
                            PartitionViolation::FenceMismatchDuringPartition { .. },
                        ) => SPEND_ERR_FENCE_MISMATCH,
                        _ => SPEND_ERR_OTHER,
                    };
                    out.put_u8(code);
                    out.put_str(&err.to_string());
                }
            }
        }
        OP_CHECK_CONTROL_PLANE_ACTION => {
            let now_tick = reader.take_u64();
            let kind_tag = reader.take_u8();
            let action = match kind_tag {
                0 => PartitionedActionKind::RequestQuotaExpansionOrRenewal,
                1 => PartitionedActionKind::CrossCellCanonicalPublishWithoutLocalAuthority,
                _ => PartitionedActionKind::UncachedRemoteAuthorityMutation,
            };
            match host.admit_partitioned_action(now_tick, &action) {
                Ok(_) => out.put_u8(RESP_OK),
                Err(PartitionViolation::ControlPlaneRequiredDuringPartition {
                    action: rejected_name,
                    link_phase,
                }) => {
                    out.put_u8(RESP_ERR);
                    out.put_str(rejected_name);
                    out.put_u8(match link_phase {
                        ControlPlaneLinkPhase::Connected => 0,
                        ControlPlaneLinkPhase::Partitioned => 1,
                        ControlPlaneLinkPhase::Reconnecting => 2,
                    });
                }
                Err(other) => panic!("unexpected violation variant: {other}"),
            }
        }
        OP_SETTLE_QUOTA_LEASE => {
            let spec = QuotaPartitionSettlementSpec {
                lease_id: QuotaLeaseId::from_bytes(reader.take_fixed::<16>()),
                receipt_id: ReceiptId::from_bytes(reader.take_fixed::<16>()),
                reconcile_seq: reader.take_u64(),
                gateway_id: reader.take_fixed::<16>(),
                attested_spent: reader.take_u64(),
                unattested_inflight_loss: reader.take_u64(),
                max_unreported_inflight_window: reader.take_u64(),
            };
            let mut ep = LocalCellHostPartitionEndpoint::new(host);
            match ep.settle_quarantined_quota_lease(spec) {
                Ok((summary, delta_refund)) => {
                    out.put_u8(RESP_OK);
                    out.put_fixed(summary.lease_id.as_bytes());
                    out.put_fixed(summary.receipt_id.as_bytes());
                    out.put_u64(summary.prior_epoch.get());
                    out.put_u64(summary.face_value);
                    out.put_u64(summary.attested_spent);
                    out.put_u64(summary.refunded_to_available);
                    out.put_u64(summary.permanently_quarantined_loss);
                    out.put_u64(summary.max_unreported_inflight_window);
                    out.put_quota_state(summary.post_reconcile_state);
                    out.put_fixed(&summary.fence_barrier_ack_digest);
                    out.put_u64(delta_refund);
                }
                Err(err) => {
                    out.put_u8(RESP_ERR);
                    out.put_str(&err.to_string());
                }
            }
        }
        OP_SETTLE_CAPACITY_LEASE => {
            let cap_id = CapacityLeaseId::from_bytes(reader.take_fixed::<16>());
            let receipt_id = ReceiptId::from_bytes(reader.take_fixed::<16>());
            let gateway_id = reader.take_fixed::<16>();
            let target_node =
                CellIdentity::from_domain(SchedulerDomainId::from_bytes(reader.take_fixed::<16>()));
            let target_boot =
                Generation::new(NonZeroU64::new(reader.take_u64()).expect("boot gen"));
            let cap_epoch = CellEpoch::from_u64(reader.take_u64()).expect("cap epoch");
            let cap_token = CellFencingToken::from_u64(reader.take_u64()).expect("cap token");
            let cleaned_amount = reader.take_u64();
            let holder_terminated = reader.take_bool();
            let attestation_digest = reader.take_fixed::<32>();
            let spec = CapacityPartitionSettlementSpec {
                capacity_lease_id: cap_id,
                receipt_id,
                gateway_id,
                cleanup_attestation: CleanupAttestation::new(
                    cap_id,
                    target_node,
                    target_boot,
                    cap_epoch,
                    cap_token,
                    cleaned_amount,
                    holder_terminated,
                    attestation_digest,
                ),
            };
            let mut ep = LocalCellHostPartitionEndpoint::new(host);
            match ep.quarantine_and_reconcile_capacity_lease(spec) {
                Ok(summary) => {
                    out.put_u8(RESP_OK);
                    out.put_fixed(summary.capacity_lease_id.as_bytes());
                    out.put_u64(summary.amount);
                    out.put_cap_state(summary.final_state);
                    out.put_fixed(&summary.fence_ack_digest);
                }
                Err(err) => {
                    out.put_u8(RESP_ERR);
                    out.put_str(&err.to_string());
                }
            }
        }
        OP_SETTLE_DEVICE_LEASE => {
            let dev_lease_id = ExclusiveDeviceLeaseId::from_bytes(reader.take_fixed::<16>());
            let receipt_id = ReceiptId::from_bytes(reader.take_fixed::<16>());
            let gateway_id = reader.take_fixed::<16>();
            let reset_receipt_id = ReceiptId::from_bytes(reader.take_fixed::<16>());
            let holder_node =
                CellIdentity::from_domain(SchedulerDomainId::from_bytes(reader.take_fixed::<16>()));
            let holder_boot =
                Generation::new(NonZeroU64::new(reader.take_u64()).expect("holder boot"));
            let excl_epoch = CellEpoch::from_u64(reader.take_u64()).expect("excl epoch");
            let excl_token = CellFencingToken::from_u64(reader.take_u64()).expect("excl token");
            let reset_gen = Generation::new(NonZeroU64::new(reader.take_u64()).expect("reset gen"));
            let spec = DevicePartitionSettlementSpec {
                device_lease_id: dev_lease_id,
                receipt_id,
                gateway_id,
                reset_receipt: DeviceResetReceipt::new(
                    reset_receipt_id,
                    dev_lease_id,
                    holder_node,
                    holder_boot,
                    excl_epoch,
                    excl_token,
                    reset_gen,
                ),
            };
            let mut ep = LocalCellHostPartitionEndpoint::new(host);
            match ep.quarantine_and_reconcile_device_lease(spec) {
                Ok(summary) => {
                    out.put_u8(RESP_OK);
                    out.put_fixed(summary.device_lease_id.as_bytes());
                    out.put_fixed(summary.device_id.as_bytes());
                    out.put_u64(summary.exclusivity_epoch.get());
                    out.put_u64(summary.reset_generation.get());
                    out.put_dev_state(summary.final_state);
                    out.put_fixed(&summary.fence_ack_digest);
                }
                Err(err) => {
                    out.put_u8(RESP_ERR);
                    out.put_str(&err.to_string());
                }
            }
        }
        OP_EXPORT_WRITER_PRINCIPAL => {
            let mut ep = LocalCellHostPartitionEndpoint::new(host);
            let req = ep
                .export_semantic_writer_principal()
                .expect("export writer principal");
            out.put_u8(RESP_OK);
            out.put_bootstrap_principal(&req);
        }
        OP_REGISTER_PEER_PRINCIPAL => {
            let req = reader.take_bootstrap_principal();
            let mut ep = LocalCellHostPartitionEndpoint::new(host);
            match ep.register_peer_principal(req) {
                Ok(()) => out.put_u8(RESP_OK),
                Err(err) => {
                    out.put_u8(RESP_ERR);
                    out.put_str(&err.to_string());
                }
            }
        }
        OP_EXPORT_SIGNED_VECTOR_CKPT => {
            let trust = reader.take_fixed::<32>();
            let signed_at_ms = reader.take_u64();
            let mut ep = LocalCellHostPartitionEndpoint::new(host);
            match ep.export_signed_vector_checkpoint(trust, signed_at_ms) {
                Ok(ckpt) => {
                    out.put_u8(RESP_OK);
                    out.put_bytes(&encode_signed_vector_checkpoint(&ckpt));
                }
                Err(err) => {
                    out.put_u8(RESP_ERR);
                    out.put_str(&err.to_string());
                }
            }
        }
        OP_VERIFY_PEER_VECTOR_CKPT => {
            let raw = reader.take_vec();
            let ckpt = decode_signed_vector_checkpoint(&raw).expect("decode vector ckpt");
            let mut ep = LocalCellHostPartitionEndpoint::new(host);
            match ep.verify_peer_vector_checkpoint(&ckpt) {
                Ok(()) => out.put_u8(RESP_OK),
                Err(err) => {
                    out.put_u8(RESP_ERR);
                    out.put_str(&err.to_string());
                }
            }
        }
        OP_EXPORT_XDOM_EVENTS => {
            let trust = reader.take_fixed::<32>();
            let mut ep = LocalCellHostPartitionEndpoint::new(host);
            match ep.export_cross_domain_events(trust) {
                Ok(items) => {
                    out.put_u8(RESP_OK);
                    out.put_bytes(&encode_cross_domain_event_items(&items));
                }
                Err(err) => {
                    out.put_u8(RESP_ERR);
                    out.put_str(&err.to_string());
                }
            }
        }
        OP_CONVERGE_XDOM_EVENTS => {
            let trust = reader.take_fixed::<32>();
            let raw = reader.take_vec();
            let items = decode_cross_domain_event_items(&raw).expect("decode xdom events");
            let mut ep = LocalCellHostPartitionEndpoint::new(host);
            match ep.converge_cross_domain_events(trust, &items) {
                Ok(snap) => {
                    out.put_u8(RESP_OK);
                    out.put_usize(snap.imported_event_ids.len());
                    for id in &snap.imported_event_ids {
                        out.put_fixed(id.as_bytes());
                    }
                    out.put_usize(snap.skipped_existing_event_ids.len());
                    for id in &snap.skipped_existing_event_ids {
                        out.put_fixed(id.as_bytes());
                    }
                    out.put_u64(snap.audit_record_count);
                    out.put_vector_checkpoint(&snap.vector_checkpoint);
                }
                Err(err) => {
                    out.put_u8(RESP_ERR);
                    out.put_str(&err.to_string());
                }
            }
        }
        OP_APPEND_SEMANTIC_RECEIPT => {
            let seed_tag = reader.take_u8();
            let receipt_id = ReceiptId::from_bytes([seed_tag; 16]);
            let fact = OperationReceiptFact {
                application_id: ApplicationId::from_bytes([0x30_u8.wrapping_add(seed_tag); 16]),
                package_id: PackageId::from_bytes([0x40_u8.wrapping_add(seed_tag); 16]),
                package_version: 1,
                installation_generation: Generation::INITIAL,
                entry_name: "partitioned-local-op",
                artifact_id: ArtifactId::from_bytes([0x50_u8.wrapping_add(seed_tag); 16]),
                payload_revision: 1,
                payload_digest: [seed_tag; 32],
                payload_size_bytes: 64,
                operation_id: OperationId::from_bytes([0x60_u8.wrapping_add(seed_tag); 16]),
                operation_generation: Generation::INITIAL,
                callback_id: CallbackId::from_bytes([0x70_u8.wrapping_add(seed_tag); 16]),
                outcome: CompletionOutcome::Completed { receipt_id },
                terminal_state: OperationState::Completed { receipt_id },
            };
            let runtime = host.runtime();
            let appended = runtime
                .semantic_writer()
                .append_operation_receipt(runtime, &fact)
                .expect("append partitioned operation receipt");
            out.put_u8(RESP_OK);
            out.put_fixed(appended.event_id.as_bytes());
            out.put_u64(appended.log_seq);
            out.put_bool(appended.replayed);
        }
        OP_PUBLISH_LOCAL_ARTIFACT => {
            let artifact_id = ArtifactId::from_bytes(reader.take_fixed::<16>());
            let create_new = reader.take_bool();
            let content_type = reader.take_string();
            let idemp_seed = reader.take_u8();
            let expected_head_revision = reader.take_u64();
            let payload = reader.take_vec();
            let created_at_ms = reader.take_u64();

            let store = &host.runtime().artifacts;
            if create_new {
                store
                    .create_artifact(CreateArtifactSpec {
                        artifact_id,
                        idempotency_key: IdempotencyKey::from_bytes([idemp_seed; 16]),
                        content_type,
                        application_id: None,
                        owner: None,
                        created_at_ms,
                    })
                    .expect("create local artifact");
            }
            let rev = store
                .put_revision(PutRevisionRequest {
                    artifact_id,
                    expected_head_revision,
                    bytes: &payload,
                    created_at_ms,
                    provenance: ProvenanceSourceTriple {
                        source_a: *host.fence().identity().as_bytes(),
                        source_b: *artifact_id.as_bytes(),
                        source_digest: ContentDigest::of_bytes(&payload),
                    },
                })
                .expect("put local artifact revision")
                .record()
                .clone();
            let head = HeadState {
                revision: rev.revision,
                digest: rev.digest,
            };
            out.put_u8(RESP_OK);
            out.put_head(head);
        }
        OP_EXPORT_ARTIFACT_PKG => {
            let artifact_id = ArtifactId::from_bytes(reader.take_fixed::<16>());
            let now_ms = reader.take_u64();
            let mut ep = LocalCellHostPartitionEndpoint::new(host);
            match ep.export_artifact_package(artifact_id, now_ms) {
                Ok(pkg) => {
                    out.put_u8(RESP_OK);
                    out.put_bytes(&encode_artifact_transfer_package(&pkg));
                }
                Err(err) => {
                    out.put_u8(RESP_ERR);
                    out.put_str(&err.to_string());
                }
            }
        }
        OP_IMPORT_ARTIFACT_PKG => {
            let policy_count = reader.take_usize();
            let mut policies = Vec::with_capacity(policy_count);
            for _ in 0..policy_count {
                policies.push(reader.take_sync_policy());
            }
            let raw_pkg = reader.take_vec();
            let pkg = decode_artifact_transfer_package(&raw_pkg).expect("decode artifact pkg");
            let commit_ctx = ArtifactImportCommitContext {
                task_id: TaskId::from_bytes(reader.take_fixed::<16>()),
                permit_id: CommitPermitId::from_bytes(reader.take_fixed::<16>()),
                write_set_root: ContentDigest::from_bytes(reader.take_fixed::<32>()),
                base_idempotency_key: IdempotencyKey::from_bytes(reader.take_fixed::<16>()),
            };
            let idempotency_key = IdempotencyKey::from_bytes(reader.take_fixed::<16>());
            let synced_at_ms = reader.take_u64();
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
                Err(err) => {
                    out.put_u8(RESP_ERR);
                    out.put_str(&err.to_string());
                }
            }
        }
        OP_INSPECT_LOCAL_ARTIFACT => {
            let artifact_id = ArtifactId::from_bytes(reader.take_fixed::<16>());
            let read_now_ms = reader.take_u64();
            let check_retained_digest = reader.take_bool();
            let retained_digest = if check_retained_digest {
                Some(ContentDigest::from_bytes(reader.take_fixed::<32>()))
            } else {
                None
            };
            let store = &host.runtime().artifacts;
            let head = store
                .resolve_head(artifact_id, read_now_ms)
                .expect("resolve artifact head")
                .expect("artifact has published head");
            let head_bytes = store
                .get_revision(artifact_id, head.revision, read_now_ms)
                .expect("read canonical head bytes");
            out.put_u8(RESP_OK);
            out.put_head(head);
            out.put_bytes(&head_bytes);
            if let Some(digest) = retained_digest {
                let retained_bytes = nlos_artifact::read_retained_sync_blob(
                    store,
                    host.artifact_sync_retention(),
                    digest,
                    read_now_ms,
                )
                .expect("read retained competing branch blob");
                out.put_bool(true);
                out.put_bytes(&retained_bytes);
            } else {
                out.put_bool(false);
            }
        }
        OP_RECORD_CHECKPOINT => {
            let entry_count = reader.take_usize();
            let mut entries = Vec::with_capacity(entry_count);
            for _ in 0..entry_count {
                entries.push(reader.take_vec());
            }
            let prefix = CheckpointPrefix::from_entries(entries);
            let mut ep = LocalCellHostPartitionEndpoint::new(host);
            match ep.refresh_and_record_checkpoint(&prefix) {
                Ok(None) => {
                    out.put_u8(RESP_OK);
                    out.put_bool(false);
                }
                Ok(Some(ckpt)) => {
                    out.put_u8(RESP_OK);
                    out.put_bool(true);
                    out.put_fixed(ckpt.cell.as_bytes());
                    out.put_u64(ckpt.epoch.get());
                    out.put_u64(ckpt.fencing_token.get());
                    out.put_u64(ckpt.heartbeat);
                    out.put_u64(ckpt.sequence);
                    out.put_u32(ckpt.os_process_id);
                    out.put_fixed(&ckpt.digest_bytes);
                    out.put_bool(ckpt.digest_verified);
                }
                Err(err) => {
                    out.put_u8(RESP_ERR);
                    out.put_str(&err.to_string());
                }
            }
        }
        OP_INJECT_STALE_EPOCH_PROBES => {
            let stale_fence = reader.take_fence();
            let existing_quota_id = QuotaLeaseId::from_bytes(reader.take_fixed::<16>());
            let now_tick = reader.take_u64();

            let avail_before = host.quota_available();
            let cap_before = host.capacity_pool_remaining();

            let stale_quota_res = host.grant_quota_lease_against(
                &stale_fence,
                QuotaLeaseId::from_bytes([0xE1; 16]),
                25,
                None,
            );
            let stale_quota_rejected = matches!(
                stale_quota_res,
                Err(SliceKError::Lease(
                    nlos_lease::QuotaLeaseGrantError::StaleEpoch { .. }
                ))
            );

            let stale_cap_res = host.grant_capacity_lease_against(
                &stale_fence,
                CapacityLeaseId::from_bytes([0xE2; 16]),
                25,
            );
            let stale_cap_rejected = matches!(
                stale_cap_res,
                Err(SliceKError::CapacityLeaseGrant(
                    nlos_lease::CapacityLeaseGrantError::StaleEpoch { .. }
                ))
            );

            let stale_usage_res =
                host.report_quota_usage_against(&stale_fence, existing_quota_id, 999);
            let stale_usage_rejected = matches!(
                stale_usage_res,
                Err(SliceKError::LeaseLedger(
                    nlos_lease::QuotaLeaseLedgerError::Fence(
                        nlos_lease::QuotaLeaseGrantError::StaleEpoch { .. }
                    )
                ))
            );

            let proof = LocalAutonomyProof::new(
                now_tick + 1_000,
                false,
                true,
                host.fence().epoch(),
                None,
                0,
                0,
            );
            let stale_gate_res = host.admit_partitioned_action_against(
                now_tick,
                &stale_fence,
                &PartitionedActionKind::LocalPureComputeOrAuthorizedSpend(proof),
            );
            let stale_gate_rejected = matches!(
                stale_gate_res,
                Err(PartitionViolation::FenceMismatchDuringPartition { .. })
            );

            assert_eq!(host.quota_available(), avail_before);
            assert_eq!(host.capacity_pool_remaining(), cap_before);

            // Prove fresh epoch-2 grants succeed cleanly right afterward.
            let fresh_grant = host
                .grant_quota_lease(QuotaLeaseId::from_bytes([0xE4; 16]), 20, None)
                .expect("fresh epoch-2 quota grant succeeds");

            out.put_u8(RESP_OK);
            out.put_bool(
                stale_quota_rejected
                    && stale_cap_rejected
                    && stale_usage_rejected
                    && stale_gate_rejected,
            );
            out.put_u64(fresh_grant.epoch().get());
            out.put_u64(host.quota_available());
        }
        other => panic!("unknown worker opcode: {other}"),
    }
    out.into_vec()
}

// =============================================================================
// Coordinator-Side Dual Live OS Process Handle & `CellPartitionEndpoint`
// =============================================================================

#[derive(Clone, Copy, Debug)]
struct LiveHeartbeatSnapshot {
    os_pid: u32,
    fence: CellFence,
    link_state: ControlPlaneLinkState,
    heartbeat: u64,
    quota_available: u64,
    capacity_remaining: u64,
    device_head_free: bool,
}

struct LiveCellProcess {
    role: &'static str,
    child: Option<Child>,
    os_pid: u32,
    initial_fence: CellFence,
    mailbox_dir: TempDir,
    cell_dir: TempDir,
    next_seq: u64,
}

impl LiveCellProcess {
    fn spawn(role: &'static str, federation_root: &Path) -> Self {
        let cell_dir = TempDir::new(&format!("cell-{role}"));
        let mailbox_dir = TempDir::new(&format!("mbox-{role}"));
        let exe = std::env::current_exe().expect("test runner binary path");
        let child = Command::new(exe)
            .arg("--exact")
            .arg("child_partition_cell_worker")
            .arg("--ignored")
            .arg("--nocapture")
            .env(ENV_WORKER_ROLE, role)
            .env(ENV_CELL_ROOT, cell_dir.path())
            .env(ENV_FEDERATION_ROOT, federation_root)
            .env(ENV_MAILBOX_DIR, mailbox_dir.path())
            .spawn()
            .expect("spawn live child CellHost OS process");

        let ready_bytes = wait_for_file_and_consume(
            &mailbox_dir.path().join("ready.bin"),
            Duration::from_secs(30),
        );
        let mut reader = FrameReader::new(&ready_bytes);
        let os_pid = reader.take_u32();
        let initial_fence = reader.take_fence();

        Self {
            role,
            child: Some(child),
            os_pid,
            initial_fence,
            mailbox_dir,
            cell_dir,
            next_seq: 1,
        }
    }

    fn send_rpc(&mut self, opcode: u8, build: impl FnOnce(&mut FrameWriter)) -> Vec<u8> {
        let seq = self.next_seq;
        self.next_seq += 1;
        let mut req = FrameWriter::new();
        req.put_u8(opcode);
        build(&mut req);
        let req_path = self.mailbox_dir.path().join(format!("req-{seq:06}.bin"));
        let resp_path = self.mailbox_dir.path().join(format!("resp-{seq:06}.bin"));
        write_atomic_file(&req_path, &req.into_vec());
        wait_for_file_and_consume(&resp_path, Duration::from_secs(30))
    }

    fn ping_and_advance_heartbeat(&mut self) -> LiveHeartbeatSnapshot {
        if let Some(child) = self.child.as_mut() {
            assert!(
                child.try_wait().expect("try_wait child").is_none(),
                "live Cell worker '{}' exited prematurely",
                self.role
            );
        }
        let resp = self.send_rpc(OP_PING_AND_HEARTBEAT, |_| {});
        let mut r = FrameReader::new(&resp);
        assert_eq!(r.take_u8(), RESP_OK);
        LiveHeartbeatSnapshot {
            os_pid: r.take_u32(),
            fence: r.take_fence(),
            link_state: r.take_link_state(),
            heartbeat: r.take_u64(),
            quota_available: r.take_u64(),
            capacity_remaining: r.take_u64(),
            device_head_free: r.take_bool(),
        }
    }

    fn grant_leases(
        &mut self,
        quota_id: QuotaLeaseId,
        quota_face: u64,
        expires_at_tick: Option<u64>,
        initial_pre_cut_spend: u64,
        capacity: Option<(CapacityLeaseId, u64)>,
        device: Option<ExclusiveDeviceLeaseId>,
    ) -> (u64, u64, bool) {
        let resp = self.send_rpc(OP_GRANT_LEASES, |w| {
            w.put_fixed(quota_id.as_bytes());
            w.put_u64(quota_face);
            w.put_opt_u64(expires_at_tick);
            w.put_u64(initial_pre_cut_spend);
            match capacity {
                None => w.put_bool(false),
                Some((cid, amt)) => {
                    w.put_bool(true);
                    w.put_fixed(cid.as_bytes());
                    w.put_u64(amt);
                }
            }
            match device {
                None => w.put_bool(false),
                Some(did) => {
                    w.put_bool(true);
                    w.put_fixed(did.as_bytes());
                }
            }
        });
        let mut r = FrameReader::new(&resp);
        assert_eq!(r.take_u8(), RESP_OK);
        (r.take_u64(), r.take_u64(), r.take_bool())
    }

    fn enter_partition(
        &mut self,
        detected_at_tick: u64,
        reason_code: u16,
    ) -> ControlPlaneLinkState {
        let resp = self.send_rpc(OP_ENTER_PARTITION, |w| {
            w.put_u64(detected_at_tick);
            w.put_u16(reason_code);
        });
        let mut r = FrameReader::new(&resp);
        assert_eq!(r.take_u8(), RESP_OK);
        r.take_link_state()
    }

    fn spend_quota_during_partition(
        &mut self,
        now_tick: u64,
        lease_id: QuotaLeaseId,
        spend_delta: u64,
        cap_expires_at: u64,
        cap_revoked: bool,
        cap_verified: bool,
    ) -> Result<([u8; 32], u64, u64), (u8, String)> {
        let resp = self.send_rpc(OP_LOCAL_AUTONOMY_SPEND, |w| {
            w.put_u64(now_tick);
            w.put_fixed(lease_id.as_bytes());
            w.put_u64(spend_delta);
            w.put_u64(cap_expires_at);
            w.put_bool(cap_revoked);
            w.put_bool(cap_verified);
        });
        let mut r = FrameReader::new(&resp);
        if r.take_u8() == RESP_OK {
            Ok((r.take_fixed::<32>(), r.take_u64(), r.take_u64()))
        } else {
            Err((r.take_u8(), r.take_string()))
        }
    }

    fn check_control_plane_action_blocked(
        &mut self,
        now_tick: u64,
        kind_tag: u8,
    ) -> (String, ControlPlaneLinkPhase) {
        let resp = self.send_rpc(OP_CHECK_CONTROL_PLANE_ACTION, |w| {
            w.put_u64(now_tick);
            w.put_u8(kind_tag);
        });
        let mut r = FrameReader::new(&resp);
        assert_eq!(
            r.take_u8(),
            RESP_ERR,
            "control-plane action must be blocked during partition"
        );
        let name = r.take_string();
        let phase = match r.take_u8() {
            0 => ControlPlaneLinkPhase::Connected,
            1 => ControlPlaneLinkPhase::Partitioned,
            _ => ControlPlaneLinkPhase::Reconnecting,
        };
        (name, phase)
    }

    fn append_partitioned_semantic_receipt(
        &mut self,
        seed_tag: u8,
    ) -> (SemanticEventId, u64, bool) {
        let resp = self.send_rpc(OP_APPEND_SEMANTIC_RECEIPT, |w| w.put_u8(seed_tag));
        let mut r = FrameReader::new(&resp);
        assert_eq!(r.take_u8(), RESP_OK);
        (
            SemanticEventId::from_bytes(r.take_fixed::<32>()),
            r.take_u64(),
            r.take_bool(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn publish_local_artifact(
        &mut self,
        artifact_id: ArtifactId,
        create_new: bool,
        content_type: &str,
        idemp_seed: u8,
        expected_head_revision: u64,
        payload: &[u8],
        created_at_ms: u64,
    ) -> HeadState {
        let resp = self.send_rpc(OP_PUBLISH_LOCAL_ARTIFACT, |w| {
            w.put_fixed(artifact_id.as_bytes());
            w.put_bool(create_new);
            w.put_str(content_type);
            w.put_u8(idemp_seed);
            w.put_u64(expected_head_revision);
            w.put_bytes(payload);
            w.put_u64(created_at_ms);
        });
        let mut r = FrameReader::new(&resp);
        assert_eq!(r.take_u8(), RESP_OK);
        r.take_head()
    }

    fn inspect_local_artifact(
        &mut self,
        artifact_id: ArtifactId,
        read_now_ms: u64,
        retained_peer_digest: Option<ContentDigest>,
    ) -> (HeadState, Vec<u8>, Option<Vec<u8>>) {
        let resp = self.send_rpc(OP_INSPECT_LOCAL_ARTIFACT, |w| {
            w.put_fixed(artifact_id.as_bytes());
            w.put_u64(read_now_ms);
            match retained_peer_digest {
                None => w.put_bool(false),
                Some(d) => {
                    w.put_bool(true);
                    w.put_fixed(d.as_bytes());
                }
            }
        });
        let mut r = FrameReader::new(&resp);
        assert_eq!(r.take_u8(), RESP_OK);
        let head = r.take_head();
        let head_bytes = r.take_vec();
        let retained = if r.take_bool() {
            Some(r.take_vec())
        } else {
            None
        };
        (head, head_bytes, retained)
    }

    fn inject_stale_epoch_probes(
        &mut self,
        stale_fence: CellFence,
        existing_quota_id: QuotaLeaseId,
        now_tick: u64,
    ) -> (bool, u64, u64) {
        let resp = self.send_rpc(OP_INJECT_STALE_EPOCH_PROBES, |w| {
            w.put_fixed(&encode_cell_fence(stale_fence));
            w.put_fixed(existing_quota_id.as_bytes());
            w.put_u64(now_tick);
        });
        let mut r = FrameReader::new(&resp);
        assert_eq!(r.take_u8(), RESP_OK);
        (r.take_bool(), r.take_u64(), r.take_u64())
    }

    fn shutdown(&mut self) {
        if let Some(mut child) = self.child.take() {
            let resp = self.send_rpc(OP_SHUTDOWN, |_| {});
            let mut r = FrameReader::new(&resp);
            assert_eq!(r.take_u8(), RESP_OK);
            assert_eq!(r.take_u32(), self.os_pid);
            let status = child.wait().expect("wait for clean worker exit");
            assert!(
                status.success(),
                "worker '{}' ({}) exited with non-zero status: {status}",
                self.role,
                self.os_pid
            );
            let _ = &self.cell_dir;
        }
    }
}

impl Drop for LiveCellProcess {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

struct DualLiveCellCluster {
    federation_dir: TempDir,
    alpha: LiveCellProcess,
    beta: LiveCellProcess,
}

impl DualLiveCellCluster {
    fn spawn() -> Self {
        let federation_dir = TempDir::new("federation");
        let alpha = LiveCellProcess::spawn("alpha", federation_dir.path());
        let beta = LiveCellProcess::spawn("beta", federation_dir.path());

        let coordinator_pid = std::process::id();
        assert_ne!(
            alpha.os_pid, beta.os_pid,
            "Cells must run in distinct OS processes"
        );
        assert_ne!(alpha.os_pid, coordinator_pid);
        assert_ne!(beta.os_pid, coordinator_pid);

        Self {
            federation_dir,
            alpha,
            beta,
        }
    }

    fn federation_root(&self) -> &Path {
        self.federation_dir.path()
    }

    fn shutdown(mut self) {
        self.alpha.shutdown();
        self.beta.shutdown();
    }
}

struct RemoteIpcCellPartitionEndpoint<'a> {
    worker: &'a mut LiveCellProcess,
}

impl<'a> RemoteIpcCellPartitionEndpoint<'a> {
    fn new(worker: &'a mut LiveCellProcess) -> Self {
        Self { worker }
    }
}

impl CellPartitionEndpoint for RemoteIpcCellPartitionEndpoint<'_> {
    fn current_fence(&mut self) -> Result<CellFence, PartitionRecoveryError> {
        let resp = self.worker.send_rpc(OP_CURRENT_FENCE, |_| {});
        let mut r = FrameReader::new(&resp);
        if r.take_u8() != RESP_OK {
            return Err(PartitionRecoveryError::EndpointTransport(r.take_string()));
        }
        Ok(r.take_fence())
    }

    fn connectivity_state(&mut self) -> Result<ControlPlaneLinkState, PartitionRecoveryError> {
        let resp = self.worker.send_rpc(OP_CONNECTIVITY_STATE, |_| {});
        let mut r = FrameReader::new(&resp);
        if r.take_u8() != RESP_OK {
            return Err(PartitionRecoveryError::EndpointTransport(r.take_string()));
        }
        Ok(r.take_link_state())
    }

    fn advance_cell_epoch(
        &mut self,
    ) -> Result<PartitionEpochAdvanceSnapshot, PartitionRecoveryError> {
        let resp = self.worker.send_rpc(OP_ADVANCE_EPOCH, |_| {});
        let mut r = FrameReader::new(&resp);
        if r.take_u8() != RESP_OK {
            return Err(PartitionRecoveryError::EndpointTransport(r.take_string()));
        }
        Ok(PartitionEpochAdvanceSnapshot {
            fence: r.take_fence(),
            entries_fenced: r.take_usize(),
        })
    }

    fn begin_reconnection(
        &mut self,
        now_tick: u64,
    ) -> Result<ControlPlaneLinkState, PartitionRecoveryError> {
        let resp = self
            .worker
            .send_rpc(OP_BEGIN_RECONNECTION, |w| w.put_u64(now_tick));
        let mut r = FrameReader::new(&resp);
        if r.take_u8() != RESP_OK {
            return Err(PartitionRecoveryError::EndpointTransport(r.take_string()));
        }
        Ok(r.take_link_state())
    }

    fn complete_reconnection(
        &mut self,
        now_tick: u64,
    ) -> Result<ControlPlaneLinkState, PartitionRecoveryError> {
        let resp = self
            .worker
            .send_rpc(OP_COMPLETE_RECONNECTION, |w| w.put_u64(now_tick));
        let mut r = FrameReader::new(&resp);
        if r.take_u8() != RESP_OK {
            return Err(PartitionRecoveryError::EndpointTransport(r.take_string()));
        }
        Ok(r.take_link_state())
    }

    fn quota_available(&mut self) -> Result<u64, PartitionRecoveryError> {
        let resp = self.worker.send_rpc(OP_QUOTA_AVAILABLE, |_| {});
        let mut r = FrameReader::new(&resp);
        if r.take_u8() != RESP_OK {
            return Err(PartitionRecoveryError::EndpointTransport(r.take_string()));
        }
        Ok(r.take_u64())
    }

    fn settle_quarantined_quota_lease(
        &mut self,
        spec: QuotaPartitionSettlementSpec,
    ) -> Result<(QuotaLeaseReconciliationSummary, u64), PartitionRecoveryError> {
        let resp = self.worker.send_rpc(OP_SETTLE_QUOTA_LEASE, |w| {
            w.put_fixed(spec.lease_id.as_bytes());
            w.put_fixed(spec.receipt_id.as_bytes());
            w.put_u64(spec.reconcile_seq);
            w.put_fixed(&spec.gateway_id);
            w.put_u64(spec.attested_spent);
            w.put_u64(spec.unattested_inflight_loss);
            w.put_u64(spec.max_unreported_inflight_window);
        });
        let mut r = FrameReader::new(&resp);
        if r.take_u8() != RESP_OK {
            return Err(PartitionRecoveryError::EndpointTransport(r.take_string()));
        }
        let summary = QuotaLeaseReconciliationSummary {
            lease_id: QuotaLeaseId::from_bytes(r.take_fixed::<16>()),
            receipt_id: ReceiptId::from_bytes(r.take_fixed::<16>()),
            prior_epoch: CellEpoch::from_u64(r.take_u64()).expect("prior epoch"),
            face_value: r.take_u64(),
            attested_spent: r.take_u64(),
            refunded_to_available: r.take_u64(),
            permanently_quarantined_loss: r.take_u64(),
            max_unreported_inflight_window: r.take_u64(),
            post_reconcile_state: r.take_quota_state(),
            fence_barrier_ack_digest: r.take_fixed::<32>(),
        };
        let delta_refund = r.take_u64();
        Ok((summary, delta_refund))
    }

    fn quarantine_and_reconcile_capacity_lease(
        &mut self,
        spec: CapacityPartitionSettlementSpec,
    ) -> Result<PartitionSettledCapacitySummary, PartitionRecoveryError> {
        let att = &spec.cleanup_attestation;
        let resp = self.worker.send_rpc(OP_SETTLE_CAPACITY_LEASE, |w| {
            w.put_fixed(spec.capacity_lease_id.as_bytes());
            w.put_fixed(spec.receipt_id.as_bytes());
            w.put_fixed(&spec.gateway_id);
            w.put_fixed(att.target_node().as_bytes());
            w.put_u64(att.target_node_boot_generation().get());
            w.put_u64(att.capacity_epoch().get());
            w.put_u64(att.fencing_token().get());
            w.put_u64(att.cleaned_amount());
            w.put_bool(att.holder_processes_terminated());
            w.put_fixed(&att.attestation_digest());
        });
        let mut r = FrameReader::new(&resp);
        if r.take_u8() != RESP_OK {
            return Err(PartitionRecoveryError::EndpointTransport(r.take_string()));
        }
        Ok(PartitionSettledCapacitySummary {
            capacity_lease_id: CapacityLeaseId::from_bytes(r.take_fixed::<16>()),
            amount: r.take_u64(),
            final_state: r.take_cap_state(),
            fence_ack_digest: r.take_fixed::<32>(),
        })
    }

    fn quarantine_and_reconcile_device_lease(
        &mut self,
        spec: DevicePartitionSettlementSpec,
    ) -> Result<PartitionSettledDeviceSummary, PartitionRecoveryError> {
        let rst = spec.reset_receipt;
        let resp = self.worker.send_rpc(OP_SETTLE_DEVICE_LEASE, |w| {
            w.put_fixed(spec.device_lease_id.as_bytes());
            w.put_fixed(spec.receipt_id.as_bytes());
            w.put_fixed(&spec.gateway_id);
            w.put_fixed(rst.receipt_id().as_bytes());
            w.put_fixed(rst.holder_node().as_bytes());
            w.put_u64(rst.holder_node_boot_generation().get());
            w.put_u64(rst.exclusivity_epoch().get());
            w.put_u64(rst.fencing_token().get());
            w.put_u64(rst.reset_generation().get());
        });
        let mut r = FrameReader::new(&resp);
        if r.take_u8() != RESP_OK {
            return Err(PartitionRecoveryError::EndpointTransport(r.take_string()));
        }
        Ok(PartitionSettledDeviceSummary {
            device_lease_id: ExclusiveDeviceLeaseId::from_bytes(r.take_fixed::<16>()),
            device_id: DeviceId::from_bytes(r.take_fixed::<16>()),
            exclusivity_epoch: CellEpoch::from_u64(r.take_u64()).expect("exclusivity epoch"),
            reset_generation: Generation::new(
                NonZeroU64::new(r.take_u64()).expect("reset generation"),
            ),
            final_state: r.take_dev_state(),
            fence_ack_digest: r.take_fixed::<32>(),
        })
    }

    fn export_semantic_writer_principal(
        &mut self,
    ) -> Result<BootstrapPrincipalRequest, PartitionRecoveryError> {
        let resp = self.worker.send_rpc(OP_EXPORT_WRITER_PRINCIPAL, |_| {});
        let mut r = FrameReader::new(&resp);
        if r.take_u8() != RESP_OK {
            return Err(PartitionRecoveryError::EndpointTransport(r.take_string()));
        }
        Ok(r.take_bootstrap_principal())
    }

    fn register_peer_principal(
        &mut self,
        request: BootstrapPrincipalRequest,
    ) -> Result<(), PartitionRecoveryError> {
        let resp = self.worker.send_rpc(OP_REGISTER_PEER_PRINCIPAL, |w| {
            w.put_bootstrap_principal(&request);
        });
        let mut r = FrameReader::new(&resp);
        if r.take_u8() != RESP_OK {
            return Err(PartitionRecoveryError::EndpointTransport(r.take_string()));
        }
        Ok(())
    }

    fn export_signed_vector_checkpoint(
        &mut self,
        trust_snapshot_digest: [u8; 32],
        signed_at_ms: u64,
    ) -> Result<SignedSemanticVectorCheckpoint, PartitionRecoveryError> {
        let resp = self.worker.send_rpc(OP_EXPORT_SIGNED_VECTOR_CKPT, |w| {
            w.put_fixed(&trust_snapshot_digest);
            w.put_u64(signed_at_ms);
        });
        let mut r = FrameReader::new(&resp);
        if r.take_u8() != RESP_OK {
            return Err(PartitionRecoveryError::EndpointTransport(r.take_string()));
        }
        let raw = r.take_vec();
        decode_signed_vector_checkpoint(&raw).ok_or_else(|| {
            PartitionRecoveryError::EndpointTransport(
                "corrupt signed vector checkpoint frame".into(),
            )
        })
    }

    fn verify_peer_vector_checkpoint(
        &mut self,
        checkpoint: &SignedSemanticVectorCheckpoint,
    ) -> Result<(), PartitionRecoveryError> {
        let encoded = encode_signed_vector_checkpoint(checkpoint);
        let resp = self
            .worker
            .send_rpc(OP_VERIFY_PEER_VECTOR_CKPT, |w| w.put_bytes(&encoded));
        let mut r = FrameReader::new(&resp);
        if r.take_u8() != RESP_OK {
            return Err(PartitionRecoveryError::EndpointTransport(r.take_string()));
        }
        Ok(())
    }

    fn export_cross_domain_events(
        &mut self,
        trust_snapshot_digest: [u8; 32],
    ) -> Result<Vec<CrossDomainEventItem>, PartitionRecoveryError> {
        let resp = self.worker.send_rpc(OP_EXPORT_XDOM_EVENTS, |w| {
            w.put_fixed(&trust_snapshot_digest);
        });
        let mut r = FrameReader::new(&resp);
        if r.take_u8() != RESP_OK {
            return Err(PartitionRecoveryError::EndpointTransport(r.take_string()));
        }
        let raw = r.take_vec();
        decode_cross_domain_event_items(&raw).ok_or_else(|| {
            PartitionRecoveryError::EndpointTransport("corrupt cross-domain event frame".into())
        })
    }

    fn converge_cross_domain_events(
        &mut self,
        local_trust_snapshot_digest: [u8; 32],
        remote_items: &[CrossDomainEventItem],
    ) -> Result<SemanticConvergenceStepSnapshot, PartitionRecoveryError> {
        let encoded = encode_cross_domain_event_items(remote_items);
        let resp = self.worker.send_rpc(OP_CONVERGE_XDOM_EVENTS, |w| {
            w.put_fixed(&local_trust_snapshot_digest);
            w.put_bytes(&encoded);
        });
        let mut r = FrameReader::new(&resp);
        if r.take_u8() != RESP_OK {
            return Err(PartitionRecoveryError::EndpointTransport(r.take_string()));
        }
        let imp_len = r.take_usize();
        let mut imported_event_ids = Vec::with_capacity(imp_len);
        for _ in 0..imp_len {
            imported_event_ids.push(SemanticEventId::from_bytes(r.take_fixed::<32>()));
        }
        let skip_len = r.take_usize();
        let mut skipped_existing_event_ids = Vec::with_capacity(skip_len);
        for _ in 0..skip_len {
            skipped_existing_event_ids.push(SemanticEventId::from_bytes(r.take_fixed::<32>()));
        }
        let audit_record_count = r.take_u64();
        let vector_checkpoint = r.take_vector_checkpoint();
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
        let resp = self.worker.send_rpc(OP_EXPORT_ARTIFACT_PKG, |w| {
            w.put_fixed(artifact_id.as_bytes());
            w.put_u64(now_ms);
        });
        let mut r = FrameReader::new(&resp);
        if r.take_u8() != RESP_OK {
            return Err(PartitionRecoveryError::EndpointTransport(r.take_string()));
        }
        let raw = r.take_vec();
        decode_artifact_transfer_package(&raw).ok_or_else(|| {
            PartitionRecoveryError::EndpointTransport("corrupt artifact package frame".into())
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
        let raw_pkg = encode_artifact_transfer_package(package);
        let resp = self.worker.send_rpc(OP_IMPORT_ARTIFACT_PKG, |w| {
            w.put_usize(policies.len());
            for p in policies {
                w.put_sync_policy(p);
            }
            w.put_bytes(&raw_pkg);
            w.put_fixed(commit_ctx.task_id.as_bytes());
            w.put_fixed(commit_ctx.permit_id.as_bytes());
            w.put_fixed(commit_ctx.write_set_root.as_bytes());
            w.put_fixed(commit_ctx.base_idempotency_key.as_bytes());
            w.put_fixed(idempotency_key.as_bytes());
            w.put_u64(synced_at_ms);
        });
        let mut r = FrameReader::new(&resp);
        if r.take_u8() != RESP_OK {
            return Err(PartitionRecoveryError::EndpointTransport(r.take_string()));
        }
        Ok(r.take_artifact_import_summary())
    }

    fn refresh_and_record_checkpoint(
        &mut self,
        prefix: &CheckpointPrefix,
    ) -> Result<Option<PartitionRecordedCheckpointSummary>, PartitionRecoveryError> {
        let resp = self.worker.send_rpc(OP_RECORD_CHECKPOINT, |w| {
            w.put_usize(prefix.entries().len());
            for entry in prefix.entries() {
                w.put_bytes(entry);
            }
        });
        let mut r = FrameReader::new(&resp);
        if r.take_u8() != RESP_OK {
            return Err(PartitionRecoveryError::EndpointTransport(r.take_string()));
        }
        if !r.take_bool() {
            return Ok(None);
        }
        Ok(Some(PartitionRecordedCheckpointSummary {
            cell: CellIdentity::from_domain(SchedulerDomainId::from_bytes(r.take_fixed::<16>())),
            epoch: CellEpoch::from_u64(r.take_u64()).expect("ckpt epoch"),
            fencing_token: CellFencingToken::from_u64(r.take_u64()).expect("ckpt token"),
            heartbeat: r.take_u64(),
            sequence: r.take_u64(),
            os_process_id: r.take_u32(),
            digest_bytes: r.take_fixed::<32>(),
            digest_verified: r.take_bool(),
        }))
    }
}

fn default_commit_ctx(seed: u8) -> ArtifactImportCommitContext {
    ArtifactImportCommitContext {
        task_id: TaskId::from_bytes([0x60_u8.wrapping_add(seed); 16]),
        permit_id: CommitPermitId::from_bytes([0x70_u8.wrapping_add(seed); 16]),
        write_set_root: ContentDigest::from_bytes([0x80_u8.wrapping_add(seed); 32]),
        base_idempotency_key: IdempotencyKey::from_bytes([0x90_u8.wrapping_add(seed); 16]),
    }
}

fn now_wall_ms() -> u64 {
    u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("wall clock after unix epoch")
            .as_millis(),
    )
    .expect("wall ms fits u64")
    .saturating_add(1_000)
}

// =============================================================================
// Scenario 1: Dual-Live Partition Cut, Bounded Local Autonomy, Control-Plane
// Blocking, and Explicit Remote Failure Classification (`[DIST-LOCAL-001]`,
// `[DIST-FAIL-001]`)
// =============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn dual_live_os_processes_sustain_local_autonomy_block_control_plane_and_classify_remote_failures()
{
    let mut cluster = DualLiveCellCluster::spawn();

    let lease_alpha = QuotaLeaseId::from_bytes([0x11; 16]);
    let lease_beta = QuotaLeaseId::from_bytes([0x22; 16]);

    // Pre-partition lease issuance while control plane is `Connected`.
    let (avail_a, _, _) = cluster
        .alpha
        .grant_leases(lease_alpha, 200, Some(500), 0, None, None);
    let (avail_b, _, _) = cluster
        .beta
        .grant_leases(lease_beta, 150, Some(500), 0, None, None);
    assert_eq!(avail_a, 800);
    assert_eq!(avail_b, 850);

    // Cut control-plane link on BOTH live OS processes simultaneously (NO process kill!).
    let state_a = cluster.alpha.enter_partition(100, 503);
    let state_b = cluster.beta.enter_partition(100, 503);
    assert_eq!(state_a.phase(), ControlPlaneLinkPhase::Partitioned);
    assert_eq!(state_b.phase(), ControlPlaneLinkPhase::Partitioned);

    // Prove BOTH child OS processes remain alive with distinct PIDs and advancing heartbeats!
    let hb_a1 = cluster.alpha.ping_and_advance_heartbeat();
    let hb_b1 = cluster.beta.ping_and_advance_heartbeat();
    assert_eq!(hb_a1.os_pid, cluster.alpha.os_pid);
    assert_eq!(hb_b1.os_pid, cluster.beta.os_pid);
    assert_ne!(hb_a1.os_pid, hb_b1.os_pid);
    assert_eq!(hb_a1.heartbeat, 2);
    assert_eq!(hb_b1.heartbeat, 2);

    // `[DIST-LOCAL-001]`: Authorized local spends within unexpired Capability + QuotaLease succeed!
    let (_, spent_a1, rem_a1) = cluster
        .alpha
        .spend_quota_during_partition(120, lease_alpha, 60, 600, false, true)
        .expect("alpha local spend #1");
    assert_eq!((spent_a1, rem_a1), (60, 140));
    let (_, spent_a2, rem_a2) = cluster
        .alpha
        .spend_quota_during_partition(140, lease_alpha, 40, 600, false, true)
        .expect("alpha local spend #2");
    assert_eq!((spent_a2, rem_a2), (100, 100));

    let (_, spent_b1, rem_b1) = cluster
        .beta
        .spend_quota_during_partition(130, lease_beta, 55, 600, false, true)
        .expect("beta local spend #1");
    assert_eq!((spent_b1, rem_b1), (55, 95));

    // Heartbeats keep advancing while partitioned.
    let hb_a2 = cluster.alpha.ping_and_advance_heartbeat();
    let hb_b2 = cluster.beta.ping_and_advance_heartbeat();
    assert_eq!(hb_a2.heartbeat, 3);
    assert_eq!(hb_b2.heartbeat, 3);

    // Fail-closed local overdraw, revocation, capability expiry (`now_tick > valid_until`),
    // and lease expiry (`now_tick >= expires_at`)!
    let (overdraw_code, _) = cluster
        .alpha
        .spend_quota_during_partition(150, lease_alpha, 101, 600, false, true)
        .expect_err("overdraw must fail closed");
    assert_eq!(overdraw_code, SPEND_ERR_INSUFFICIENT_BALANCE);

    let (revoked_code, _) = cluster
        .alpha
        .spend_quota_during_partition(160, lease_alpha, 10, 600, true, true)
        .expect_err("revoked capability must fail closed");
    assert_eq!(revoked_code, SPEND_ERR_CAP_UNPROVABLE);

    let (cap_exp_code, _) = cluster
        .beta
        .spend_quota_during_partition(601, lease_beta, 10, 600, false, true)
        .expect_err("expired capability must fail closed");
    assert_eq!(cap_exp_code, SPEND_ERR_CAP_EXPIRED);

    let (lease_exp_code, _) = cluster
        .beta
        .spend_quota_during_partition(500, lease_beta, 10, 900, false, true)
        .expect_err("expired quota lease must fail closed");
    assert_eq!(lease_exp_code, SPEND_ERR_LEASE_EXPIRED);

    // All three control-plane action classes are blocked fail-closed during partition!
    for (tag, expected_action) in [
        (0_u8, "request_quota_expansion_or_renewal"),
        (1_u8, "cross_cell_canonical_publish_without_local_authority"),
        (2_u8, "uncached_remote_authority_mutation"),
    ] {
        let (rejected_name, phase) = cluster.alpha.check_control_plane_action_blocked(610, tag);
        assert_eq!(rejected_name, expected_action);
        assert_eq!(phase, ControlPlaneLinkPhase::Partitioned);
    }

    // `[DIST-FAIL-001]`: Cross-Cell remote calls classify into explicit `Unavailable` vs
    // `DeadlineExceededUncertain` — never silent hang or blind side-effecting retry!
    let idemp_key = IdempotencyKey::from_bytes([0xF1; 16]);
    let side_effecting_contract = RemoteCallContract::new([0xC1; 16], idemp_key, 250, 2, 0, true);

    let unavailable: RemoteCallOutcome<()> =
        side_effecting_contract.classify_pre_dispatch_unavailable();
    assert!(matches!(
        unavailable,
        RemoteCallOutcome::Unavailable {
            retry_directive: RemoteRetryDirective::SafeToRetryWithSameKey,
            ..
        }
    ));
    let retried_contract = side_effecting_contract
        .next_retry_for_outcome(&unavailable, 350)
        .expect("pre-dispatch Unavailable permits retry with identical idempotency_key");
    assert_eq!(retried_contract.idempotency_key, idemp_key);
    assert_eq!(retried_contract.attempt_number, 1);

    let uncertain: RemoteCallOutcome<()> = side_effecting_contract.classify_inflight_timeout(280);
    assert!(matches!(
        uncertain,
        RemoteCallOutcome::DeadlineExceededUncertain {
            retry_directive: RemoteRetryDirective::ReconcileBeforeRetry,
            ..
        }
    ));
    assert!(uncertain.is_uncertain());
    assert!(uncertain.requires_reconciliation());
    assert_eq!(
        side_effecting_contract.next_retry_for_outcome(&uncertain, 450),
        Err(
            RemoteRetryRefusal::UncertainSideEffectRequiresReconciliation {
                idempotency_key: idemp_key,
            }
        )
    );

    cluster.shutdown();
}

// =============================================================================
// Scenario 2: Partition Heal With Full Spend Attestation — Zero Loss Across
// All Three Lease Families + Verified Federated Checkpoints
// =============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn partition_heal_with_full_gateway_attestation_achieves_zero_loss_across_three_lease_families() {
    let mut cluster = DualLiveCellCluster::spawn();

    let quota_a = QuotaLeaseId::from_bytes([0x21; 16]);
    let cap_lease_a = CapacityLeaseId::from_bytes([0x22; 16]);
    let dev_lease_a = ExclusiveDeviceLeaseId::from_bytes([0x23; 16]);
    let quota_b = QuotaLeaseId::from_bytes([0x24; 16]);

    let (avail_a, cap_rem_a, dev_free_a) = cluster.alpha.grant_leases(
        quota_a,
        200,
        Some(1_000),
        0,
        Some((cap_lease_a, 300)),
        Some(dev_lease_a),
    );
    assert_eq!((avail_a, cap_rem_a, dev_free_a), (800, 400, false));

    let (avail_b, _, _) = cluster
        .beta
        .grant_leases(quota_b, 150, Some(1_000), 0, None, None);
    assert_eq!(avail_b, 850);

    let fence_a_e1 = cluster.alpha.initial_fence;

    // Enter partition; both live OS processes spend locally and advance heartbeats.
    cluster.alpha.enter_partition(100, 503);
    cluster.beta.enter_partition(100, 503);

    cluster
        .alpha
        .spend_quota_during_partition(120, quota_a, 75, 900, false, true)
        .expect("alpha spends 75 locally");
    cluster
        .beta
        .spend_quota_during_partition(125, quota_b, 50, 900, false, true)
        .expect("beta spends 50 locally");

    assert_eq!(cluster.alpha.ping_and_advance_heartbeat().heartbeat, 2);
    assert_eq!(cluster.beta.ping_and_advance_heartbeat().heartbeat, 2);

    let att_digest = CleanupAttestation::compute_digest(
        cap_lease_a,
        fence_a_e1.identity(),
        fence_a_e1.node_boot_generation(),
        fence_a_e1.epoch(),
        fence_a_e1.fencing_token(),
        300,
        true,
    );
    let cleanup_attestation = CleanupAttestation::new(
        cap_lease_a,
        fence_a_e1.identity(),
        fence_a_e1.node_boot_generation(),
        fence_a_e1.epoch(),
        fence_a_e1.fencing_token(),
        300,
        true,
        att_digest,
    );
    let reset_receipt = DeviceResetReceipt::new(
        ReceiptId::from_bytes([0x44; 16]),
        dev_lease_a,
        fence_a_e1.identity(),
        fence_a_e1.node_boot_generation(),
        fence_a_e1.epoch(),
        fence_a_e1.fencing_token(),
        Generation::INITIAL,
    );

    // Heal partition with full gateway spend attestation (`unattested_inflight_loss = 0`).
    let request = PartitionRecoveryRequest {
        recovery_id: [0x52; 16],
        reconnection_tick: 200,
        completion_tick: 210,
        synced_at_ms: now_wall_ms(),
        trust_snapshot_digest_a: [0x71; 32],
        trust_snapshot_digest_b: [0x71; 32],
        cell_a_leases: CellLeaseReconciliationSpec {
            quota_leases: vec![QuotaPartitionSettlementSpec {
                lease_id: quota_a,
                receipt_id: ReceiptId::from_bytes([0x41; 16]),
                reconcile_seq: 1,
                gateway_id: GATEWAY_ID,
                attested_spent: 75,
                unattested_inflight_loss: 0,
                max_unreported_inflight_window: 40,
            }],
            capacity_leases: vec![CapacityPartitionSettlementSpec {
                capacity_lease_id: cap_lease_a,
                receipt_id: ReceiptId::from_bytes([0x42; 16]),
                gateway_id: GATEWAY_ID,
                cleanup_attestation,
            }],
            device_leases: vec![DevicePartitionSettlementSpec {
                device_lease_id: dev_lease_a,
                receipt_id: ReceiptId::from_bytes([0x43; 16]),
                gateway_id: GATEWAY_ID,
                reset_receipt,
            }],
        },
        cell_b_leases: CellLeaseReconciliationSpec {
            quota_leases: vec![QuotaPartitionSettlementSpec {
                lease_id: quota_b,
                receipt_id: ReceiptId::from_bytes([0x45; 16]),
                reconcile_seq: 1,
                gateway_id: GATEWAY_ID,
                attested_spent: 50,
                unattested_inflight_loss: 0,
                max_unreported_inflight_window: 30,
            }],
            capacity_leases: vec![],
            device_leases: vec![],
        },
        semantic_convergence: SemanticPartitionConvergenceSpec::default(),
        artifact_convergence: ArtifactPartitionConvergenceSpec {
            policies: vec![],
            cell_a_exports: vec![],
            cell_b_exports: vec![],
            cell_a_import_context: default_commit_ctx(1),
            cell_b_import_context: default_commit_ctx(2),
        },
        record_federation_checkpoints: true,
    };

    let outcome = {
        let mut ep_a = RemoteIpcCellPartitionEndpoint::new(&mut cluster.alpha);
        let mut ep_b = RemoteIpcCellPartitionEndpoint::new(&mut cluster.beta);
        drive_partition_reconnection_convergence(&mut ep_a, &mut ep_b, &request)
            .expect("zero-loss partition heal")
    };

    assert!(outcome.receipt.verify_digest());
    assert!(outcome.cell_a_bounded_loss.verify_invariants());
    assert!(outcome.cell_b_bounded_loss.verify_invariants());
    assert_eq!(outcome.cell_a_bounded_loss.reconciled_spent, 75);
    assert_eq!(outcome.cell_a_bounded_loss.refunded_available, 125);
    assert_eq!(outcome.cell_a_bounded_loss.permanently_quarantined_loss, 0);
    assert_eq!(
        outcome.cell_a_bounded_loss.pool_available_after_reconcile,
        925
    );
    assert_eq!(
        outcome.cell_a_bounded_loss.quota_summaries[0].post_reconcile_state,
        QuotaLeaseState::Closed
    );

    assert_eq!(outcome.cell_b_bounded_loss.reconciled_spent, 50);
    assert_eq!(outcome.cell_b_bounded_loss.refunded_available, 100);
    assert_eq!(outcome.cell_b_bounded_loss.permanently_quarantined_loss, 0);
    assert_eq!(
        outcome.cell_b_bounded_loss.pool_available_after_reconcile,
        950
    );

    // Verify all three lease pools & `Connected` state on live workers.
    let post_a = cluster.alpha.ping_and_advance_heartbeat();
    let post_b = cluster.beta.ping_and_advance_heartbeat();
    assert_eq!(post_a.link_state.phase(), ControlPlaneLinkPhase::Connected);
    assert_eq!(post_b.link_state.phase(), ControlPlaneLinkPhase::Connected);
    assert_eq!(post_a.fence.epoch().get(), 2);
    assert_eq!(post_b.fence.epoch().get(), 2);
    assert_eq!(post_a.quota_available, 925);
    assert_eq!(post_a.capacity_remaining, 700);
    assert!(post_a.device_head_free);
    assert_eq!(post_b.quota_available, 950);

    // Independently open shared `CellDirectory` in coordinator and verify both `PARTRECV` checkpoints!
    let expected_prefix = partition_recovery_checkpoint_prefix(&outcome.receipt);
    let decoded_entry = decode_partition_recovery_checkpoint_entry(&expected_prefix.entries()[0])
        .expect("decode PARTRECV entry");
    assert_eq!(decoded_entry.receipt_digest, outcome.receipt.receipt_digest);
    assert_eq!(decoded_entry.cell_a_epoch, 2);
    assert_eq!(decoded_entry.cell_b_epoch, 2);

    let directory = CellDirectory::open(cluster.federation_root()).expect("open CellDirectory");
    for cell_id in [post_a.fence.identity(), post_b.fence.identity()] {
        let log = directory.checkpoints(cell_id).expect("list checkpoints");
        assert_eq!(log.corrupt(), 0);
        let records = log.into_records();
        assert_eq!(records.len(), 1);
        assert_eq!(
            records[0].fact().verify_digest(&expected_prefix),
            CheckpointDigestVerification::Verified
        );
    }

    cluster.shutdown();
}

// =============================================================================
// Scenario 3: Partition Heal With Unattested In-Flight Window — `[ROAD-C-001]`
// Bounded Quota Loss Proof & Zero Oversell
// =============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn partition_heal_with_unattested_inflight_window_quarantines_bounded_loss_without_oversell() {
    let mut cluster = DualLiveCellCluster::spawn();

    let quota_a = QuotaLeaseId::from_bytes([0x31; 16]);
    let quota_b = QuotaLeaseId::from_bytes([0x32; 16]);

    // Pre-cut attested usage: Alpha reported 60 out of 200; Beta reported 50 out of 180.
    let (avail_a, _, _) = cluster
        .alpha
        .grant_leases(quota_a, 200, Some(1_000), 60, None, None);
    let (avail_b, _, _) = cluster
        .beta
        .grant_leases(quota_b, 180, Some(1_000), 50, None, None);
    assert_eq!(avail_a, 800);
    assert_eq!(avail_b, 820);

    // Cut partition; during partition Alpha spends an additional 30 locally (total 90) and
    // Beta spends 25 locally (total 75) while both OS processes stay live!
    cluster.alpha.enter_partition(100, 503);
    cluster.beta.enter_partition(100, 503);

    let (_, spent_a, rem_a) = cluster
        .alpha
        .spend_quota_during_partition(130, quota_a, 30, 900, false, true)
        .expect("alpha local in-flight spend during partition");
    assert_eq!((spent_a, rem_a), (90, 110));

    let (_, spent_b, rem_b) = cluster
        .beta
        .spend_quota_during_partition(135, quota_b, 25, 900, false, true)
        .expect("beta local in-flight spend during partition");
    assert_eq!((spent_b, rem_b), (75, 105));

    // Verify fail-closed rejection if `unattested_inflight_loss > max_unreported_inflight_window`!
    let mut recovery_req = PartitionRecoveryRequest {
        recovery_id: [0x60; 16],
        reconnection_tick: 200,
        completion_tick: 210,
        synced_at_ms: now_wall_ms(),
        trust_snapshot_digest_a: [0x72; 32],
        trust_snapshot_digest_b: [0x72; 32],
        cell_a_leases: CellLeaseReconciliationSpec {
            quota_leases: vec![QuotaPartitionSettlementSpec {
                lease_id: quota_a,
                receipt_id: ReceiptId::from_bytes([0x51; 16]),
                reconcile_seq: 1,
                gateway_id: GATEWAY_ID,
                attested_spent: 90,
                unattested_inflight_loss: 45, // > max_unreported_inflight_window (40)!
                max_unreported_inflight_window: 40,
            }],
            capacity_leases: vec![],
            device_leases: vec![],
        },
        cell_b_leases: CellLeaseReconciliationSpec {
            quota_leases: vec![QuotaPartitionSettlementSpec {
                lease_id: quota_b,
                receipt_id: ReceiptId::from_bytes([0x52; 16]),
                reconcile_seq: 1,
                gateway_id: GATEWAY_ID,
                attested_spent: 75,
                unattested_inflight_loss: 20,
                max_unreported_inflight_window: 30,
            }],
            capacity_leases: vec![],
            device_leases: vec![],
        },
        semantic_convergence: SemanticPartitionConvergenceSpec::default(),
        artifact_convergence: ArtifactPartitionConvergenceSpec {
            policies: vec![],
            cell_a_exports: vec![],
            cell_b_exports: vec![],
            cell_a_import_context: default_commit_ctx(3),
            cell_b_import_context: default_commit_ctx(4),
        },
        record_federation_checkpoints: true,
    };

    {
        let mut ep_a = RemoteIpcCellPartitionEndpoint::new(&mut cluster.alpha);
        let over_bound =
            ep_a.settle_quarantined_quota_lease(recovery_req.cell_a_leases.quota_leases[0]);
        assert!(matches!(
            over_bound,
            Err(PartitionRecoveryError::EndpointTransport(_))
        ));
    }

    // Heal with bounded unattested in-flight loss (`25 <= 40` on Alpha, `20 <= 30` on Beta).
    recovery_req.cell_a_leases.quota_leases[0].unattested_inflight_loss = 25;
    let outcome = {
        let mut ep_a = RemoteIpcCellPartitionEndpoint::new(&mut cluster.alpha);
        let mut ep_b = RemoteIpcCellPartitionEndpoint::new(&mut cluster.beta);
        drive_partition_reconnection_convergence(&mut ep_a, &mut ep_b, &recovery_req)
            .expect("bounded-loss partition heal")
    };

    // `[ROAD-C-001]` + `[LEASE-LOSS-001]`:
    // Alpha: face=200, spent=90, unattested_quarantined_loss=25, refunded=85 -> AVAILABLE = 800 + 85 = 885 (NEVER 910!)
    // Beta:  face=180, spent=75, unattested_quarantined_loss=20, refunded=85 -> AVAILABLE = 820 + 85 = 905 (NEVER 925!)
    assert!(outcome.cell_a_bounded_loss.verify_invariants());
    assert!(outcome.cell_b_bounded_loss.verify_invariants());

    assert_eq!(outcome.cell_a_bounded_loss.total_granted_face_value, 200);
    assert_eq!(outcome.cell_a_bounded_loss.reconciled_spent, 90);
    assert_eq!(outcome.cell_a_bounded_loss.permanently_quarantined_loss, 25);
    assert_eq!(outcome.cell_a_bounded_loss.refunded_available, 85);
    assert_eq!(
        outcome.cell_a_bounded_loss.pool_available_after_reconcile,
        885
    );
    assert!(
        outcome.cell_a_bounded_loss.permanently_quarantined_loss
            <= outcome.cell_a_bounded_loss.total_granted_face_value
                - outcome.cell_a_bounded_loss.reconciled_spent
    );
    assert!(
        outcome.cell_a_bounded_loss.permanently_quarantined_loss
            <= outcome.cell_a_bounded_loss.max_unreported_inflight_window
    );
    assert_eq!(
        outcome.cell_a_bounded_loss.quota_summaries[0].post_reconcile_state,
        QuotaLeaseState::Quarantined
    );

    assert_eq!(outcome.cell_b_bounded_loss.total_granted_face_value, 180);
    assert_eq!(outcome.cell_b_bounded_loss.reconciled_spent, 75);
    assert_eq!(outcome.cell_b_bounded_loss.permanently_quarantined_loss, 20);
    assert_eq!(outcome.cell_b_bounded_loss.refunded_available, 85);
    assert_eq!(
        outcome.cell_b_bounded_loss.pool_available_after_reconcile,
        905
    );
    assert_eq!(
        outcome.cell_b_bounded_loss.quota_summaries[0].post_reconcile_state,
        QuotaLeaseState::Quarantined
    );

    assert_eq!(outcome.receipt.cell_a_permanently_quarantined_loss, 25);
    assert_eq!(outcome.receipt.cell_b_permanently_quarantined_loss, 20);

    let post_a = cluster.alpha.ping_and_advance_heartbeat();
    let post_b = cluster.beta.ping_and_advance_heartbeat();
    assert_eq!(post_a.quota_available, 885);
    assert_eq!(post_b.quota_available, 905);

    cluster.shutdown();
}

// =============================================================================
// Scenario 4: Partitioned Semantic Admissions & Idempotent Replay Convergence
// (`[DIST-EVENT-001]`, `[DIST-AUDIT-001]`, `[SEM-CHECKPOINT-001]`)
// =============================================================================

#[test]
fn partitioned_semantic_admissions_converge_to_identical_vector_checkpoint_with_idempotent_replay()
{
    let mut cluster = DualLiveCellCluster::spawn();

    // Enter partition; both live OS processes independently admit signed semantic assertions!
    cluster.alpha.enter_partition(100, 503);
    cluster.beta.enter_partition(100, 503);

    let (ev_a1, seq_a1, rep_a1) = cluster.alpha.append_partitioned_semantic_receipt(0x11);
    let (ev_a2, seq_a2, rep_a2) = cluster.alpha.append_partitioned_semantic_receipt(0x12);
    assert_eq!((seq_a1, seq_a2, rep_a1, rep_a2), (1, 2, false, false));

    let (ev_b1, seq_b1, rep_b1) = cluster.beta.append_partitioned_semantic_receipt(0x21);
    let (ev_b2, seq_b2, rep_b2) = cluster.beta.append_partitioned_semantic_receipt(0x22);
    assert_eq!((seq_b1, seq_b2, rep_b1, rep_b2), (1, 2, false, false));

    // Confirm both processes are alive during partition.
    assert_eq!(cluster.alpha.ping_and_advance_heartbeat().heartbeat, 2);
    assert_eq!(cluster.beta.ping_and_advance_heartbeat().heartbeat, 2);

    let request = PartitionRecoveryRequest {
        recovery_id: [0x64; 16],
        reconnection_tick: 200,
        completion_tick: 210,
        synced_at_ms: now_wall_ms(),
        trust_snapshot_digest_a: [0x88; 32],
        trust_snapshot_digest_b: [0x88; 32],
        cell_a_leases: CellLeaseReconciliationSpec::default(),
        cell_b_leases: CellLeaseReconciliationSpec::default(),
        semantic_convergence: SemanticPartitionConvergenceSpec {
            additional_principals_for_a: vec![],
            additional_principals_for_b: vec![],
            verify_idempotent_second_replay: true,
        },
        artifact_convergence: ArtifactPartitionConvergenceSpec {
            policies: vec![],
            cell_a_exports: vec![],
            cell_b_exports: vec![],
            cell_a_import_context: default_commit_ctx(5),
            cell_b_import_context: default_commit_ctx(6),
        },
        record_federation_checkpoints: true,
    };

    let outcome = {
        let mut ep_a = RemoteIpcCellPartitionEndpoint::new(&mut cluster.alpha);
        let mut ep_b = RemoteIpcCellPartitionEndpoint::new(&mut cluster.beta);
        drive_partition_reconnection_convergence(&mut ep_a, &mut ep_b, &request)
            .expect("semantic convergence across dual live processes")
    };

    let sem = &outcome.semantic_convergence;
    assert_eq!(
        sem.pre_sync_diff_on_a.relation,
        VectorCheckpointRelation::Concurrent
    );
    assert_eq!(
        sem.pre_sync_diff_on_b.relation,
        VectorCheckpointRelation::Concurrent
    );
    assert_eq!(sem.cell_a_convergence.imported_event_ids.len(), 2);
    assert!(sem.cell_a_convergence.imported_event_ids.contains(&ev_b1));
    assert!(sem.cell_a_convergence.imported_event_ids.contains(&ev_b2));
    assert_eq!(sem.cell_b_convergence.imported_event_ids.len(), 2);
    assert!(sem.cell_b_convergence.imported_event_ids.contains(&ev_a1));
    assert!(sem.cell_b_convergence.imported_event_ids.contains(&ev_a2));

    assert!(sem.replay_idempotency_verified);
    assert_eq!(sem.converged_event_count, 4);
    assert_eq!(
        sem.post_sync_checkpoint_a.covered_event_ids_root,
        sem.post_sync_checkpoint_b.covered_event_ids_root
    );
    assert_eq!(
        sem.post_sync_checkpoint_a.domain_watermarks,
        sem.post_sync_checkpoint_b.domain_watermarks
    );
    assert_eq!(
        sem.post_sync_checkpoint_a.checkpoint_digest,
        sem.post_sync_checkpoint_b.checkpoint_digest
    );

    // Explicit third replay over IPC imports 0 duplicate rows and skips all 4!
    {
        let mut ep_a = RemoteIpcCellPartitionEndpoint::new(&mut cluster.alpha);
        let mut ep_b = RemoteIpcCellPartitionEndpoint::new(&mut cluster.beta);
        let exported_b = ep_b
            .export_cross_domain_events([0x88; 32])
            .expect("export b");
        assert_eq!(exported_b.len(), 4);
        let third_replay = ep_a
            .converge_cross_domain_events([0x88; 32], &exported_b)
            .expect("third idempotent replay");
        assert_eq!(third_replay.imported_event_ids.len(), 0);
        assert_eq!(third_replay.skipped_existing_event_ids.len(), 4);
    }

    cluster.shutdown();
}

// =============================================================================
// Scenario 5: Partitioned Artifact Revisions — Disjoint Fast-Forward &
// Divergent `SymmetricPeer` Conflict Isolation (`[DIST-ART-001]`)
// =============================================================================

#[test]
#[allow(clippy::too_many_lines)]
fn partitioned_artifact_updates_fast_forward_disjoint_heads_and_isolate_divergent_branches_symmetrically()
 {
    let mut cluster = DualLiveCellCluster::spawn();

    let content_type = "text/markdown";
    let art_shared = ArtifactId::from_bytes([0xA0; 16]);
    let art_only_a = ArtifactId::from_bytes([0xA1; 16]);
    let art_only_b = ArtifactId::from_bytes([0xB1; 16]);

    let shared_base_v1 = b"shared-document-ancestor-v1";
    let head_shared_a1 = cluster.alpha.publish_local_artifact(
        art_shared,
        true,
        content_type,
        1,
        0,
        shared_base_v1,
        1_000,
    );
    let head_shared_b1 = cluster.beta.publish_local_artifact(
        art_shared,
        true,
        content_type,
        1,
        0,
        shared_base_v1,
        1_000,
    );
    assert_eq!(head_shared_a1, head_shared_b1);

    // Cut control-plane link; both live OS processes independently author artifacts during partition!
    cluster.alpha.enter_partition(100, 503);
    cluster.beta.enter_partition(100, 503);

    // Disjoint artifact on Alpha (`v1` -> `v2`)
    let _ = cluster.alpha.publish_local_artifact(
        art_only_a,
        true,
        content_type,
        2,
        0,
        b"alpha-disjoint-v1",
        2_000,
    );
    let head_only_a_v2 = cluster.alpha.publish_local_artifact(
        art_only_a,
        false,
        content_type,
        3,
        1,
        b"alpha-disjoint-v2",
        2_100,
    );

    // Disjoint artifact on Beta (`v1`)
    let head_only_b_v1 = cluster.beta.publish_local_artifact(
        art_only_b,
        true,
        content_type,
        4,
        0,
        b"beta-disjoint-v1",
        2_050,
    );

    // Concurrent conflicting `v2` edits on `art_shared` on Alpha vs Beta!
    let divergent_bytes_a = b"alpha-concurrent-divergent-edit-v2";
    let divergent_bytes_b = b"beta-concurrent-divergent-edit-v2";
    let head_div_a = cluster.alpha.publish_local_artifact(
        art_shared,
        false,
        content_type,
        5,
        1,
        divergent_bytes_a,
        2_200,
    );
    let head_div_b = cluster.beta.publish_local_artifact(
        art_shared,
        false,
        content_type,
        6,
        1,
        divergent_bytes_b,
        2_250,
    );
    assert_ne!(head_div_a.digest, head_div_b.digest);

    let policy = ArtifactSyncPolicy {
        content_type: content_type.to_string(),
        merge_strategy: ArtifactMergeStrategy::FastForwardLineageOnly,
        conflict_strategy: ArtifactConflictStrategy::QuarantineDivergentBranch,
        replica_authority: ArtifactReplicaAuthority::SymmetricPeer,
        deletion_semantics: ArtifactDeletionSemantics::TombstoneRevisionRequired,
    };

    let synced_at_ms = now_wall_ms();
    let request = PartitionRecoveryRequest {
        recovery_id: [0x65; 16],
        reconnection_tick: 200,
        completion_tick: 210,
        synced_at_ms,
        trust_snapshot_digest_a: [0x89; 32],
        trust_snapshot_digest_b: [0x89; 32],
        cell_a_leases: CellLeaseReconciliationSpec::default(),
        cell_b_leases: CellLeaseReconciliationSpec::default(),
        semantic_convergence: SemanticPartitionConvergenceSpec::default(),
        artifact_convergence: ArtifactPartitionConvergenceSpec {
            policies: vec![policy],
            cell_a_exports: vec![art_only_a, art_shared],
            cell_b_exports: vec![art_only_b, art_shared],
            cell_a_import_context: default_commit_ctx(7),
            cell_b_import_context: default_commit_ctx(8),
        },
        record_federation_checkpoints: true,
    };

    let outcome = {
        let mut ep_a = RemoteIpcCellPartitionEndpoint::new(&mut cluster.alpha);
        let mut ep_b = RemoteIpcCellPartitionEndpoint::new(&mut cluster.beta);
        drive_partition_reconnection_convergence(&mut ep_a, &mut ep_b, &request)
            .expect("artifact convergence & symmetric conflict isolation")
    };

    let art = &outcome.artifact_convergence;
    assert_eq!(art.conflict_receipts_on_a.len(), 1);
    assert_eq!(art.conflict_receipts_on_b.len(), 1);
    assert_eq!(
        art.conflict_receipts_on_a[0].conflict_digest,
        art.conflict_receipts_on_b[0].conflict_digest,
        "symmetric divergent conflict receipts MUST compute identical conflict_digest"
    );
    assert_eq!(art.canonical_conflict_digests.len(), 1);
    assert_eq!(
        outcome.receipt.artifact_conflict_digests,
        art.canonical_conflict_digests
    );

    let inspect_ms = synced_at_ms + 500;
    // Disjoint artifacts fast-forwarded onto the opposite Cell!
    let (beta_imported_a_head, beta_imported_a_bytes, _) = cluster
        .beta
        .inspect_local_artifact(art_only_a, inspect_ms, None);
    assert_eq!(beta_imported_a_head, head_only_a_v2);
    assert_eq!(beta_imported_a_bytes, b"alpha-disjoint-v2");

    let (alpha_imported_b_head, alpha_imported_b_bytes, _) = cluster
        .alpha
        .inspect_local_artifact(art_only_b, inspect_ms, None);
    assert_eq!(alpha_imported_b_head, head_only_b_v1);
    assert_eq!(alpha_imported_b_bytes, b"beta-disjoint-v1");

    // Divergent `art_shared` preserved local head on each Cell (ZERO silent last-writer-wins!)
    // AND retained the competing peer's branch blob on disk!
    let (alpha_shared_head, alpha_shared_bytes, alpha_retained_peer) = cluster
        .alpha
        .inspect_local_artifact(art_shared, inspect_ms, Some(head_div_b.digest));
    assert_eq!(alpha_shared_head, head_div_a);
    assert_eq!(alpha_shared_bytes, divergent_bytes_a);
    assert_eq!(
        alpha_retained_peer.as_deref(),
        Some(divergent_bytes_b.as_slice())
    );

    let (beta_shared_head, beta_shared_bytes, beta_retained_peer) = cluster
        .beta
        .inspect_local_artifact(art_shared, inspect_ms, Some(head_div_a.digest));
    assert_eq!(beta_shared_head, head_div_b);
    assert_eq!(beta_shared_bytes, divergent_bytes_b);
    assert_eq!(
        beta_retained_peer.as_deref(),
        Some(divergent_bytes_a.as_slice())
    );

    cluster.shutdown();
}

// =============================================================================
// Scenario 6: Post-Reconnection Stale-Epoch Fence & Message Injection
// Fail-Closed Across Dual Live Processes (`[LEASE-FENCE-001]`)
// =============================================================================

#[test]
fn post_reconnection_stale_epoch_messages_and_fences_fail_closed_across_dual_live_processes() {
    let mut cluster = DualLiveCellCluster::spawn();

    let stale_fence_alpha = cluster.alpha.initial_fence;
    let stale_fence_beta = cluster.beta.initial_fence;
    assert_eq!(stale_fence_alpha.epoch().get(), 1);
    assert_eq!(stale_fence_beta.epoch().get(), 1);

    let quota_a = QuotaLeaseId::from_bytes([0x61; 16]);
    let quota_b = QuotaLeaseId::from_bytes([0x62; 16]);
    cluster
        .alpha
        .grant_leases(quota_a, 100, Some(1_000), 30, None, None);
    cluster
        .beta
        .grant_leases(quota_b, 100, Some(1_000), 40, None, None);

    cluster.alpha.enter_partition(100, 503);
    cluster.beta.enter_partition(100, 503);

    let request = PartitionRecoveryRequest {
        recovery_id: [0x66; 16],
        reconnection_tick: 200,
        completion_tick: 210,
        synced_at_ms: now_wall_ms(),
        trust_snapshot_digest_a: [0x90; 32],
        trust_snapshot_digest_b: [0x90; 32],
        cell_a_leases: CellLeaseReconciliationSpec {
            quota_leases: vec![QuotaPartitionSettlementSpec {
                lease_id: quota_a,
                receipt_id: ReceiptId::from_bytes([0x71; 16]),
                reconcile_seq: 1,
                gateway_id: GATEWAY_ID,
                attested_spent: 30,
                unattested_inflight_loss: 0,
                max_unreported_inflight_window: 20,
            }],
            capacity_leases: vec![],
            device_leases: vec![],
        },
        cell_b_leases: CellLeaseReconciliationSpec {
            quota_leases: vec![QuotaPartitionSettlementSpec {
                lease_id: quota_b,
                receipt_id: ReceiptId::from_bytes([0x72; 16]),
                reconcile_seq: 1,
                gateway_id: GATEWAY_ID,
                attested_spent: 40,
                unattested_inflight_loss: 0,
                max_unreported_inflight_window: 20,
            }],
            capacity_leases: vec![],
            device_leases: vec![],
        },
        semantic_convergence: SemanticPartitionConvergenceSpec::default(),
        artifact_convergence: ArtifactPartitionConvergenceSpec {
            policies: vec![],
            cell_a_exports: vec![],
            cell_b_exports: vec![],
            cell_a_import_context: default_commit_ctx(9),
            cell_b_import_context: default_commit_ctx(10),
        },
        record_federation_checkpoints: true,
    };

    let outcome = {
        let mut ep_a = RemoteIpcCellPartitionEndpoint::new(&mut cluster.alpha);
        let mut ep_b = RemoteIpcCellPartitionEndpoint::new(&mut cluster.beta);
        drive_partition_reconnection_convergence(&mut ep_a, &mut ep_b, &request)
            .expect("partition heal advances both Cells to epoch 2")
    };
    assert_eq!(outcome.cell_a_epoch_advance.fence.epoch().get(), 2);
    assert_eq!(outcome.cell_b_epoch_advance.fence.epoch().get(), 2);

    // Inject delayed pre-heal epoch-1 fences into BOTH live OS processes!
    // Every stale quota grant, stale capacity grant, stale usage report, and stale gate admission
    // MUST fail closed with zero state mutation, while fresh epoch-2 grants succeed!
    let (all_rejected_a, fresh_epoch_a, post_avail_a) =
        cluster
            .alpha
            .inject_stale_epoch_probes(stale_fence_alpha, quota_a, 300);
    assert!(all_rejected_a);
    assert_eq!(fresh_epoch_a, 2);
    // Alpha had 970 after refund (1000 - 30), then granted 20 at epoch 2 -> 950.
    assert_eq!(post_avail_a, 950);

    let (all_rejected_b, fresh_epoch_b, post_avail_b) =
        cluster
            .beta
            .inject_stale_epoch_probes(stale_fence_beta, quota_b, 300);
    assert!(all_rejected_b);
    assert_eq!(fresh_epoch_b, 2);
    // Beta had 960 after refund (1000 - 40), then granted 20 at epoch 2 -> 940.
    assert_eq!(post_avail_b, 940);

    cluster.shutdown();
}
