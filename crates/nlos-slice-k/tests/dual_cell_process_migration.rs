//! `W64` (`C-MIGRATE-002`): Dual-Cell end-to-end process migration integration
//! tests across two federated [`CellHost`] instances running in separate OS
//! processes (`ADR-0018`: one OS process = one Cell).
//!
//! Each top-level `#[test]` spawns two real child OS processes from the current
//! test binary (`child_source_cell_host_worker` and
//! `child_target_cell_host_driver`), sharing a filesystem [`CellDirectory`]
//! federation root (`ADR-0019`) and driving
//! [`drive_cell_host_process_migration`] over an atomic file-backed IPC
//! [`SourceCellMigrationEndpoint`].

use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use ed25519_dalek::{Signer, SigningKey};
use nlos_artifact::sync::ReplicaRevisionBundle;
use nlos_artifact::{ContentDigest, HeadState};
use nlos_capability::{
    CapabilityAuthorityError, CapabilityHandle, CapabilityRecord, CapabilityRevocationReceipt,
    CapabilityRights, CapabilityTarget, DelegateCapabilityRequest, IssueRootCapabilityRequest,
    RevokeCapabilityRequest, SignedDelegateCapabilityRequest, SignedIssueRootCapabilityRequest,
    SignedRevokeCapabilityRequest, delegate_command_message, issue_root_command_message,
    revoke_command_message,
};
use nlos_cell::{
    CellDirectory, CellEpoch, CellFencingToken, CellIdentity, CheckpointDigestVerification,
    FailureDetectorConfig, FailureDetectorError, InsertOutcome, Liveness, MigrationIntent,
    MigrationIntentRecord, MigrationObject, MonitoredSubject, NameCacheError, NamePath,
};
use nlos_identity::{BootstrapPrincipalRequest, KeyPurpose};
use nlos_lease::{CapacityLeaseState, ExclusiveDeviceLeaseState, QuotaLeaseState};
use nlos_operation::{OperationHandle, OperationSpec, OperationState};
use nlos_process::{
    CheckpointPayloadKind, DriverSessionAttachment, MigrationAbortOutcome, MigrationMode,
    PinnedSessionPolicy, ProcessAuthorityError, ProcessBindingRecord, ProcessCheckpointProof,
    ProcessLifecycleState, ProcessMigrationError, ProcessMigrationStep, QuiesceProof,
    RegisterFiberIncarnationRequest, SourceProcessFenceProof,
};
use nlos_runtime::{FiberExit, FiberHandle, FiberSpec, RuntimeAdapter};
use nlos_runtime_tokio::{TokioRuntimeAdapter, TokioRuntimeConfig};
use nlos_slice_k::{
    CapabilityRebindItemSpec, CapacityLeaseMigrationSpec, CellHost, CellHostConfig,
    CellHostProcessMigrationRequest, CheckpointSourceSpec, DeviceLeaseMigrationSpec,
    LeaseMigrationBundleSpec, LocalCellHostSourceEndpoint, LocalCellHostTargetEndpoint,
    PROCESS_MIGRATION_INTENT_TAG, ProcessLineageGenesis, ProcessLineageSpec,
    ProcessMigrationDriveError, QuotaLeaseMigrationSpec, SliceKError, SourceCellMigrationEndpoint,
    SourceCellPlacementSnapshot, SourceCheckpointBundle, SourceCheckpointRequest,
    SourceClosedQuotaSummary, SourceFenceReport, SourceFenceRequest, SourceFenceSpec,
    SourceLeaseCloseReport, SourcePostFenceQuarantineReport, SourceQuiesceRequest,
    SourceQuiesceSpec, SourceReturnedCapacitySummary, SourceReturnedDeviceSummary,
    TargetRestoreSpec, decode_process_migration_receipt_checkpoint_entry,
    drive_cell_host_process_migration, inspect_host_active_capability,
    process_migration_intent_reason, publish_restart_image_artifact, seeded_key,
};
use nlos_types::{
    AgentInstanceId, ArtifactId, CallbackId, CancellationScopeId, CapacityLeaseId, DeviceId,
    ExclusiveDeviceLeaseId, ExecutionFiberId, Generation, IdempotencyKey, IsolationDomainId, KeyId,
    NamespaceId, OperationId, PrincipalId, ProcessId, QuotaLeaseId, ReceiptId, ResourceGroupId,
    SchedulerDomainId, TaskAttemptId, TaskId,
};

const ENV_SCENARIO: &str = "LLMOS_DUAL_CELL_SCENARIO";
const ENV_FEDERATION_ROOT: &str = "LLMOS_FEDERATION_ROOT";
const ENV_CELL_ROOT: &str = "LLMOS_CELL_ROOT";
const ENV_IPC_DIR: &str = "LLMOS_IPC_DIR";

const CMD_INSPECT_PLACEMENT: u8 = 1;
const CMD_RECORD_INTENT: u8 = 2;
const CMD_QUIESCE: u8 = 3;
const CMD_CAPTURE_CHECKPOINT: u8 = 4;
const CMD_FENCE_SOURCE: u8 = 5;
const CMD_CLOSE_LEASES: u8 = 6;
const CMD_REVOKE_CAPS: u8 = 7;
const CMD_QUARANTINE_LEASES: u8 = 8;
const CMD_FINISH_VERIFY: u8 = 9;
const CMD_ABORT_WORKER: u8 = 10;

const SOURCE_DOMAIN_BYTES: [u8; 16] = [0xA1; 16];
const TARGET_DOMAIN_BYTES: [u8; 16] = [0xB2; 16];
const SHARED_DEVICE_BYTES: [u8; 16] = [0xD4; 16];

const TASK_BYTES: [u8; 16] = [0x31; 16];
const ATTEMPT_BYTES: [u8; 16] = [0x32; 16];
const MIGRATION_ID_BYTES: [u8; 16] = [0x44; 16];
const SCOPE_BYTES: [u8; 16] = [0x51; 16];
const FIBER_11_BYTES: [u8; 16] = [0x11; 16];
const FIBER_77_BYTES: [u8; 16] = [0x77; 16];
const FIBER_99_BYTES: [u8; 16] = [0x99; 16];
const OP_BYTES: [u8; 16] = [0x61; 16];
const CALLBACK_BYTES: [u8; 16] = [0x62; 16];
const CANCEL_RECEIPT_BYTES: [u8; 16] = [0x63; 16];
const SOURCE_QUOTA_BYTES: [u8; 16] = [0x71; 16];
const TARGET_QUOTA_BYTES: [u8; 16] = [0x72; 16];
const SOURCE_CAP_LEASE_BYTES: [u8; 16] = [0x73; 16];
const TARGET_CAP_LEASE_BYTES: [u8; 16] = [0x74; 16];
const SOURCE_DEV_LEASE_BYTES: [u8; 16] = [0x75; 16];
const TARGET_DEV_LEASE_BYTES: [u8; 16] = [0x76; 16];
const ARTIFACT_ID_BYTES: [u8; 16] = [0x81; 16];
const ARTIFACT_CONTENT_TYPE: &str = "application/vnd.llmos.process-restart-image+v1";
const ARTIFACT_IMAGE_BYTES: &[u8] = b"llmos-restart-image-elf-state-v1";
const TRANSPARENT_STATE_BYTES: &[u8] = b"llmos-transparent-process-checkpoint-v1";
const TRANSPARENT_CACHE_KEY: &str = "proc-checkpoint-v1";
const CAP_NAME_PATH: &str = "/w64/migrated/capability";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ScenarioId {
    TransparentHappyPath,
    MultiFamilyLeases,
    SourceFencingRefusal,
    PinnedRefuse,
    PinnedDowngradeArtifact,
    PostFenceCapAmplificationQuarantine,
    PostFenceLeaseConservationQuarantine,
}

impl ScenarioId {
    const fn as_str(self) -> &'static str {
        match self {
            Self::TransparentHappyPath => "transparent_happy_path",
            Self::MultiFamilyLeases => "multi_family_leases",
            Self::SourceFencingRefusal => "source_fencing_refusal",
            Self::PinnedRefuse => "pinned_refuse",
            Self::PinnedDowngradeArtifact => "pinned_downgrade_artifact",
            Self::PostFenceCapAmplificationQuarantine => "post_fence_cap_amplification_quarantine",
            Self::PostFenceLeaseConservationQuarantine => {
                "post_fence_lease_conservation_quarantine"
            }
        }
    }

    fn from_str(raw: &str) -> Self {
        match raw {
            "transparent_happy_path" => Self::TransparentHappyPath,
            "multi_family_leases" => Self::MultiFamilyLeases,
            "source_fencing_refusal" => Self::SourceFencingRefusal,
            "pinned_refuse" => Self::PinnedRefuse,
            "pinned_downgrade_artifact" => Self::PinnedDowngradeArtifact,
            "post_fence_cap_amplification_quarantine" => Self::PostFenceCapAmplificationQuarantine,
            "post_fence_lease_conservation_quarantine" => {
                Self::PostFenceLeaseConservationQuarantine
            }
            other => panic!("unknown dual-cell scenario: {other}"),
        }
    }
}

struct TempDir {
    root: PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let seq = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "nlos-w64-migrate-{name}-{}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("create temp dir");
        Self { root }
    }

    fn root(&self) -> &Path {
        &self.root
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn detector_config() -> FailureDetectorConfig {
    FailureDetectorConfig::new(
        NonZeroU64::new(3).expect("suspect"),
        NonZeroU64::new(5).expect("dead"),
    )
    .expect("detector thresholds")
}

fn source_cell_config() -> CellHostConfig {
    CellHostConfig::new(
        SchedulerDomainId::from_bytes(SOURCE_DOMAIN_BYTES),
        1000,
        500,
        DeviceId::from_bytes(SHARED_DEVICE_BYTES),
        detector_config(),
    )
}

fn target_cell_config() -> CellHostConfig {
    CellHostConfig::new(
        SchedulerDomainId::from_bytes(TARGET_DOMAIN_BYTES),
        1000,
        500,
        DeviceId::from_bytes(SHARED_DEVICE_BYTES),
        detector_config(),
    )
}

fn write_atomic_bytes(path: &Path, bytes: &[u8]) {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes).expect("write tmp ipc file");
    std::fs::rename(&tmp, path).expect("rename ipc file");
}

fn wait_for_file(path: &Path) -> Vec<u8> {
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        if path.exists()
            && let Ok(bytes) = std::fs::read(path)
        {
            return bytes;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for IPC file {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

// ============================================================================
// Compact deterministic binary codec for IPC between Source and Target OS procs
// ============================================================================

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

    fn put_u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_be_bytes());
    }

    fn put_u64(&mut self, v: u64) {
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
        self.put_u64(v.len() as u64);
        self.buf.extend_from_slice(v);
    }

    fn put_str(&mut self, v: &str) {
        self.put_bytes(v.as_bytes());
    }

    fn put_opt<T>(&mut self, opt: Option<T>, mut f: impl FnMut(&mut Self, T)) {
        match opt {
            None => self.put_u8(0),
            Some(val) => {
                self.put_u8(1);
                f(self, val);
            }
        }
    }

    fn put_binding(&mut self, b: &ProcessBindingRecord) {
        self.put_arr16(b.process_id.as_bytes());
        self.put_u64(b.process_generation.get());
        self.put_arr32(&b.process_fencing_token);
        self.put_arr16(b.agent_instance_id.as_bytes());
        self.put_u64(b.agent_instance_generation.get());
        self.put_arr16(b.task_id.as_bytes());
        self.put_arr16(b.task_attempt_id.as_bytes());
        self.put_u64(b.attempt_generation.get());
        self.put_arr16(b.isolation_domain_id.as_bytes());
        self.put_u64(b.isolation_domain_generation.get());
        self.put_arr32(&b.isolation_domain_fencing_token);
        self.put_opt(b.prior_process_generation, |w, g| w.put_u64(g.get()));
        self.put_arr16(b.idempotency_key.as_bytes());
        self.put_u64(b.created_at_ms);
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
                self.put_u64(dropped_pinned_sessions.len() as u64);
                for s in dropped_pinned_sessions {
                    self.put_arr16(s);
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
                self.put_arr16(artifact_id.as_bytes());
                self.put_u64(*revision);
                self.put_bool(*sync_from_source_if_missing);
                self.put_u64(*checkpointed_at_ms);
            }
        }
    }

    fn put_head_state(&mut self, head: HeadState) {
        self.put_u64(head.revision);
        self.put_arr32(head.digest.as_bytes());
    }

    fn put_lease_bundle_spec(&mut self, spec: &LeaseMigrationBundleSpec) {
        assert!(
            spec.quota.override_fence_ack.is_none(),
            "IPC test codec expects default FenceBarrierAck"
        );
        assert!(
            spec.quota.target_expires_at.is_none(),
            "IPC test codec expects None target_expires_at"
        );
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

    fn put_quota_state(&mut self, s: QuotaLeaseState) {
        let code = match s {
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

    fn put_cap_state(&mut self, s: CapacityLeaseState) {
        let code = match s {
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

    fn put_dev_state(&mut self, s: ExclusiveDeviceLeaseState) {
        let code = match s {
            ExclusiveDeviceLeaseState::DeviceReserved => 0,
            ExclusiveDeviceLeaseState::Resetting => 1,
            ExclusiveDeviceLeaseState::Fencing => 2,
            ExclusiveDeviceLeaseState::Quarantined => 3,
            ExclusiveDeviceLeaseState::Returned => 4,
        };
        self.put_u8(code);
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

    fn get_u8(&mut self) -> u8 {
        self.take(1)[0]
    }

    fn get_bool(&mut self) -> bool {
        self.get_u8() != 0
    }

    fn get_u32(&mut self) -> u32 {
        u32::from_be_bytes(self.take(4).try_into().expect("u32"))
    }

    fn get_u64(&mut self) -> u64 {
        u64::from_be_bytes(self.take(8).try_into().expect("u64"))
    }

    fn get_gen(&mut self) -> Generation {
        Generation::new(NonZeroU64::new(self.get_u64()).expect("nonzero generation"))
    }

    fn get_arr16(&mut self) -> [u8; 16] {
        self.take(16).try_into().expect("arr16")
    }

    fn get_arr32(&mut self) -> [u8; 32] {
        self.take(32).try_into().expect("arr32")
    }

    fn get_arr64(&mut self) -> [u8; 64] {
        self.take(64).try_into().expect("arr64")
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
            other => panic!("invalid option tag: {other}"),
        }
    }

    fn get_cell_id(&mut self) -> CellIdentity {
        CellIdentity::from_domain(SchedulerDomainId::from_bytes(self.get_arr16()))
    }

    fn get_binding(&mut self) -> ProcessBindingRecord {
        ProcessBindingRecord {
            process_id: ProcessId::from_bytes(self.get_arr16()),
            process_generation: self.get_gen(),
            process_fencing_token: self.get_arr32(),
            agent_instance_id: AgentInstanceId::from_bytes(self.get_arr16()),
            agent_instance_generation: self.get_gen(),
            task_id: TaskId::from_bytes(self.get_arr16()),
            task_attempt_id: TaskAttemptId::from_bytes(self.get_arr16()),
            attempt_generation: self.get_gen(),
            isolation_domain_id: IsolationDomainId::from_bytes(self.get_arr16()),
            isolation_domain_generation: self.get_gen(),
            isolation_domain_fencing_token: self.get_arr32(),
            prior_process_generation: self.get_opt(Self::get_gen),
            idempotency_key: IdempotencyKey::from_bytes(self.get_arr16()),
            created_at_ms: self.get_u64(),
        }
    }

    fn get_migration_mode(&mut self) -> MigrationMode {
        match self.get_u8() {
            0 => MigrationMode::TransparentCheckpointRestore,
            1 => {
                let artifact_image_digest = self.get_arr32();
                let count = usize::try_from(self.get_u64()).expect("count");
                let mut dropped_pinned_sessions = Vec::with_capacity(count);
                for _ in 0..count {
                    dropped_pinned_sessions.push(self.get_arr16());
                }
                MigrationMode::DowngradedRestartFromArtifact {
                    artifact_image_digest,
                    dropped_pinned_sessions,
                }
            }
            other => panic!("invalid MigrationMode tag: {other}"),
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
                artifact_id: ArtifactId::from_bytes(self.get_arr16()),
                revision: self.get_u64(),
                sync_from_source_if_missing: self.get_bool(),
                checkpointed_at_ms: self.get_u64(),
            },
            other => panic!("invalid CheckpointSourceSpec tag: {other}"),
        }
    }

    fn get_head_state(&mut self) -> HeadState {
        HeadState {
            revision: self.get_u64(),
            digest: ContentDigest::from_bytes(self.get_arr32()),
        }
    }

    fn get_lease_bundle_spec(&mut self) -> LeaseMigrationBundleSpec {
        let quota = QuotaLeaseMigrationSpec {
            source_lease_id: QuotaLeaseId::from_bytes(self.get_arr16()),
            gateway_id: self.get_arr16(),
            override_fence_ack: None,
            target_lease_id: QuotaLeaseId::from_bytes(self.get_arr16()),
            target_face_value: self.get_opt(Self::get_u64),
            target_expires_at: None,
            activate_on_target: self.get_bool(),
        };
        let capacity = self.get_opt(|r| CapacityLeaseMigrationSpec {
            source_lease_id: CapacityLeaseId::from_bytes(r.get_arr16()),
            reconcile_receipt_id: ReceiptId::from_bytes(r.get_arr16()),
            gateway_id: r.get_arr16(),
            target_lease_id: CapacityLeaseId::from_bytes(r.get_arr16()),
            target_amount: r.get_opt(Self::get_u64),
        });
        let device = self.get_opt(|r| DeviceLeaseMigrationSpec {
            source_lease_id: ExclusiveDeviceLeaseId::from_bytes(r.get_arr16()),
            reset_receipt_id: ReceiptId::from_bytes(r.get_arr16()),
            reconcile_receipt_id: ReceiptId::from_bytes(r.get_arr16()),
            gateway_id: r.get_arr16(),
            target_lease_id: ExclusiveDeviceLeaseId::from_bytes(r.get_arr16()),
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

    fn get_quota_state(&mut self) -> QuotaLeaseState {
        match self.get_u8() {
            0 => QuotaLeaseState::Issued,
            1 => QuotaLeaseState::Active,
            2 => QuotaLeaseState::Closing,
            3 => QuotaLeaseState::Closed,
            4 => QuotaLeaseState::Cancelled,
            5 => QuotaLeaseState::Fenced,
            6 => QuotaLeaseState::Quarantined,
            other => panic!("invalid QuotaLeaseState tag: {other}"),
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
            other => panic!("invalid CapacityLeaseState tag: {other}"),
        }
    }

    fn get_dev_state(&mut self) -> ExclusiveDeviceLeaseState {
        match self.get_u8() {
            0 => ExclusiveDeviceLeaseState::DeviceReserved,
            1 => ExclusiveDeviceLeaseState::Resetting,
            2 => ExclusiveDeviceLeaseState::Fencing,
            3 => ExclusiveDeviceLeaseState::Quarantined,
            4 => ExclusiveDeviceLeaseState::Returned,
            other => panic!("invalid ExclusiveDeviceLeaseState tag: {other}"),
        }
    }
}

#[derive(Clone, Debug)]
struct SourceReadyPayload {
    source_os_pid: u32,
    source_cell_id: CellIdentity,
    spawned_binding: ProcessBindingRecord,
    artifact_image_digest: Option<[u8; 32]>,
}

impl SourceReadyPayload {
    fn encode(&self) -> Vec<u8> {
        let mut w = WireWriter::new();
        w.put_u32(self.source_os_pid);
        w.put_arr16(self.source_cell_id.as_bytes());
        w.put_binding(&self.spawned_binding);
        w.put_opt(self.artifact_image_digest, |w, d| w.put_arr32(&d));
        w.into_vec()
    }

    fn decode(bytes: &[u8]) -> Self {
        let mut r = WireReader::new(bytes);
        Self {
            source_os_pid: r.get_u32(),
            source_cell_id: r.get_cell_id(),
            spawned_binding: r.get_binding(),
            artifact_image_digest: r.get_opt(WireReader::get_arr32),
        }
    }
}

// ============================================================================
// Deterministic Capability Signing Fixtures Shared Across Source and Target
// ============================================================================

struct CapabilityFixtures {
    root_record: CapabilityRecord,
}

fn seed_source_capabilities(
    host: &CellHost,
    amplify_rights_on_delegate: bool,
    base_ms: u64,
) -> CapabilityFixtures {
    let issuer_sk = SigningKey::from_bytes(&[0x11; 32]);
    let holder_sk = SigningKey::from_bytes(&[0x22; 32]);

    let issuer_bootstrap = BootstrapPrincipalRequest {
        principal_profile_digest: [0x12; 32],
        control_domain_policy_digest: [0x13; 32],
        public_key: issuer_sk.verifying_key().to_bytes(),
        key_purpose: KeyPurpose::SemanticSigning,
        key_valid_from_ms: 0,
        key_valid_until_ms: base_ms + 86_400_000,
        idempotency_key: seeded_key(0x90, 1),
        created_at_ms: base_ms + 100,
    };
    let holder_bootstrap = BootstrapPrincipalRequest {
        principal_profile_digest: [0x23; 32],
        control_domain_policy_digest: [0x24; 32],
        public_key: holder_sk.verifying_key().to_bytes(),
        key_purpose: KeyPurpose::SemanticSigning,
        key_valid_from_ms: 0,
        key_valid_until_ms: base_ms + 86_400_000,
        idempotency_key: seeded_key(0x90, 2),
        created_at_ms: base_ms + 100,
    };

    let issuer_binding = host
        .runtime()
        .identity
        .bootstrap_principal(issuer_bootstrap)
        .expect("bootstrap issuer")
        .binding();
    let holder_binding = host
        .runtime()
        .identity
        .bootstrap_principal(holder_bootstrap)
        .expect("bootstrap holder")
        .binding();

    let parent_rights = if amplify_rights_on_delegate {
        CapabilityRights::DELEGATE
    } else {
        CapabilityRights::DELEGATE.union(CapabilityRights::SEMANTIC_APPEND)
    };
    let root_cmd = IssueRootCapabilityRequest {
        issuer_key_id: issuer_binding.key_id,
        holder_key_id: holder_binding.key_id,
        target: CapabilityTarget::Namespace(NamespaceId::from_bytes([0x99; 16])),
        rights: parent_rights,
        purpose_digest: Some([0xAA; 32]),
        valid_from_ms: base_ms + 100,
        valid_until_ms: base_ms + 500_000,
        delegation_depth_remaining: 2,
        call_limit: Some(10),
        idempotency_key: seeded_key(0x90, 3),
        issued_at_ms: base_ms + 150,
    };
    let root_sig = issuer_sk
        .sign(&issue_root_command_message(root_cmd))
        .to_bytes();
    let signed_root = SignedIssueRootCapabilityRequest {
        command: root_cmd,
        signer: issuer_binding.principal_id,
        signature: root_sig,
    };
    let root_record = host
        .runtime()
        .capability()
        .issue_root_signed(&host.runtime().identity, signed_root)
        .expect("issue root cap")
        .record();

    CapabilityFixtures { root_record }
}

// ============================================================================
// IPC SourceCellMigrationEndpoint Client (Runs inside Target OS Process)
// ============================================================================

struct IpcSourceCellEndpoint {
    ipc_dir: PathBuf,
    federation_root: PathBuf,
    seq: u64,
    finished: bool,
}

impl IpcSourceCellEndpoint {
    fn new(ipc_dir: PathBuf, federation_root: PathBuf) -> Self {
        Self {
            ipc_dir,
            federation_root,
            seq: 0,
            finished: false,
        }
    }

    fn call_rpc(&mut self, cmd: u8, payload: &[u8]) -> Result<Vec<u8>, String> {
        self.seq += 1;
        let mut req = Vec::with_capacity(1 + payload.len());
        req.push(cmd);
        req.extend_from_slice(payload);

        let req_path = self.ipc_dir.join(format!("req_{:06}.bin", self.seq));
        let resp_path = self.ipc_dir.join(format!("resp_{:06}.bin", self.seq));
        write_atomic_bytes(&req_path, &req);

        let raw_resp = wait_for_file(&resp_path);
        let mut r = WireReader::new(&raw_resp);
        if r.get_bool() {
            Ok(raw_resp[1..].to_vec())
        } else {
            Err(r.get_str())
        }
    }

    fn finish_and_verify_source(&mut self) {
        self.finished = true;
        self.call_rpc(CMD_FINISH_VERIFY, &[])
            .expect("source worker post-migration verification");
    }
}

impl Drop for IpcSourceCellEndpoint {
    fn drop(&mut self) {
        if !self.finished {
            let abort_path = self.ipc_dir.join("req_999999.bin");
            let _ = std::fs::write(abort_path, [CMD_ABORT_WORKER]);
        }
    }
}

impl SourceCellMigrationEndpoint for IpcSourceCellEndpoint {
    fn inspect_source_placement(
        &self,
        process_id: ProcessId,
        expected_target: CellIdentity,
    ) -> Result<SourceCellPlacementSnapshot, ProcessMigrationDriveError> {
        static PROBE_SEQ: AtomicU64 = AtomicU64::new(10_000);
        let probe_id = PROBE_SEQ.fetch_add(1, Ordering::Relaxed);
        let mut w = WireWriter::new();
        w.put_u8(CMD_INSPECT_PLACEMENT);
        w.put_arr16(process_id.as_bytes());
        w.put_arr16(expected_target.as_bytes());

        let req_path = self.ipc_dir.join(format!("req_{probe_id:06}.bin"));
        let resp_path = self.ipc_dir.join(format!("resp_{probe_id:06}.bin"));
        write_atomic_bytes(&req_path, &w.into_vec());

        let raw_resp = wait_for_file(&resp_path);
        let mut r = WireReader::new(&raw_resp);
        if !r.get_bool() {
            return Err(ProcessMigrationDriveError::Transport(r.get_str()));
        }
        Ok(SourceCellPlacementSnapshot {
            cell_id: r.get_cell_id(),
            node_boot_generation: r.get_gen(),
            epoch: CellEpoch::from_u64(r.get_u64()).expect("epoch"),
            fencing_token: CellFencingToken::from_u64(r.get_u64()).expect("fencing token"),
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
        w.put_arr16(intent.source().as_bytes());
        w.put_arr16(intent.target().as_bytes());
        w.put_arr16(intent.object().as_bytes());
        w.put_u64(intent.generation().get());
        w.put_str(intent.reason());

        let body = self
            .call_rpc(CMD_RECORD_INTENT, &w.into_vec())
            .map_err(ProcessMigrationDriveError::Transport)?;
        let mut r = WireReader::new(&body);
        let os_pid = r.get_u32();
        let sequence = r.get_u64();

        let dir = CellDirectory::open(&self.federation_root)
            .map_err(|e| ProcessMigrationDriveError::Transport(e.to_string()))?;
        let intents = dir
            .migration_intents()
            .map_err(|e| ProcessMigrationDriveError::Transport(e.to_string()))?
            .into_records();
        intents
            .into_iter()
            .find(|rec| rec.os_process_id() == os_pid && rec.sequence() == sequence)
            .ok_or_else(|| {
                ProcessMigrationDriveError::Transport(
                    "recorded MigrationIntentRecord not found in shared CellDirectory".into(),
                )
            })
    }

    fn quiesce_source_process(
        &mut self,
        request: &SourceQuiesceRequest,
    ) -> Result<QuiesceProof, ProcessMigrationDriveError> {
        let mut w = WireWriter::new();
        w.put_arr16(request.process_id.as_bytes());
        w.put_arr16(request.source_cell_id.as_bytes());
        w.put_u64(request.source_generation.get());
        w.put_opt(request.cancel_scope, |w, (sid, sgen)| {
            w.put_arr16(sid.as_bytes());
            w.put_u64(sgen.get());
        });
        w.put_u32(request.active_fiber_count);
        w.put_u64(request.inflight_operations.len() as u64);
        for (op_handle, receipt_id) in &request.inflight_operations {
            w.put_arr16(op_handle.operation_id.as_bytes());
            w.put_u64(op_handle.generation.get());
            w.put_arr16(receipt_id.as_bytes());
        }
        w.put_u64(request.quiesced_at_ms);

        let body = self
            .call_rpc(CMD_QUIESCE, &w.into_vec())
            .map_err(ProcessMigrationDriveError::Transport)?;
        let mut r = WireReader::new(&body);
        Ok(QuiesceProof {
            process_id: ProcessId::from_bytes(r.get_arr16()),
            source_cell_id: r.get_arr16(),
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
        w.put_arr16(&request.migration_id);
        w.put_arr16(request.process_id.as_bytes());
        w.put_arr16(request.source_cell_id.as_bytes());
        w.put_u64(request.source_generation.get());
        w.put_migration_mode(&request.migration_mode);
        w.put_u64(request.migratable_sessions.len() as u64);
        for (sid, sdig) in &request.migratable_sessions {
            w.put_arr16(sid);
            w.put_arr32(sdig);
        }
        w.put_checkpoint_spec(&request.spec);

        let body = self
            .call_rpc(CMD_CAPTURE_CHECKPOINT, &w.into_vec())
            .map_err(ProcessMigrationDriveError::Transport)?;
        let mut r = WireReader::new(&body);
        let process_id = ProcessId::from_bytes(r.get_arr16());
        let source_generation = r.get_u64();
        let payload = match r.get_u8() {
            0 => {
                let state_checkpoint_digest = r.get_arr32();
                let count = usize::try_from(r.get_u64()).expect("session count");
                let mut migrated_session_digests = Vec::with_capacity(count);
                for _ in 0..count {
                    migrated_session_digests.push((r.get_arr16(), r.get_arr32()));
                }
                CheckpointPayloadKind::FullStateCheckpoint {
                    state_checkpoint_digest,
                    migrated_session_digests,
                }
            }
            1 => CheckpointPayloadKind::ArtifactRestartImage {
                artifact_image_digest: r.get_arr32(),
            },
            other => panic!("invalid CheckpointPayloadKind tag: {other}"),
        };
        let checkpointed_at_ms = r.get_u64();
        let transparent_cache_transfer = r.get_opt(|r| (r.get_str(), r.get_bytes(), r.get_u64()));
        let exported_artifact_bundle = r.get_opt(|r| {
            let artifact_id = ArtifactId::from_bytes(r.get_arr16());
            let content_type = r.get_str();
            let remote_head = r.get_head_state();
            let parent_head = r.get_opt(WireReader::get_head_state);
            let anc_count = usize::try_from(r.get_u64()).expect("anc_count");
            let mut ancestor_lineage = Vec::with_capacity(anc_count);
            for _ in 0..anc_count {
                ancestor_lineage.push(r.get_head_state());
            }
            let origin_cell_id = r.get_arr16();
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
        w.put_arr16(request.process_id.as_bytes());
        w.put_arr16(request.source_cell_id.as_bytes());
        w.put_u64(request.source_generation.get());
        w.put_u64(request.invalidate_name_paths.len() as u64);
        for np in &request.invalidate_name_paths {
            w.put_str(np.as_str());
        }
        w.put_arr16(request.terminate_idempotency_key.as_bytes());
        w.put_arr16(request.stale_dispatch_probe_fiber_id.as_bytes());
        w.put_arr16(request.stale_dispatch_probe_key.as_bytes());
        w.put_u64(request.fenced_at_ms);

        let body = self
            .call_rpc(CMD_FENCE_SOURCE, &w.into_vec())
            .map_err(ProcessMigrationDriveError::Transport)?;
        let mut r = WireReader::new(&body);
        Ok(SourceFenceReport {
            proof: SourceProcessFenceProof {
                process_id: ProcessId::from_bytes(r.get_arr16()),
                source_cell_id: r.get_arr16(),
                source_cell_epoch: r.get_u64(),
                fenced_generation: r.get_u64(),
                source_fencing_token: r.get_arr32(),
                supervisor_unregistered: r.get_bool(),
                fenced_at_ms: r.get_u64(),
            },
            stale_heartbeat_refused: r.get_bool(),
            stale_dispatch_refused: r.get_bool(),
            invalidated_name_paths: usize::try_from(r.get_u64()).expect("invalidated_name_paths"),
        })
    }

    fn close_or_fence_source_leases(
        &mut self,
        spec: &LeaseMigrationBundleSpec,
    ) -> Result<SourceLeaseCloseReport, ProcessMigrationDriveError> {
        let mut w = WireWriter::new();
        w.put_lease_bundle_spec(spec);

        let body = self
            .call_rpc(CMD_CLOSE_LEASES, &w.into_vec())
            .map_err(ProcessMigrationDriveError::Transport)?;
        let mut r = WireReader::new(&body);
        let closed_quota = SourceClosedQuotaSummary {
            lease_id: QuotaLeaseId::from_bytes(r.get_arr16()),
            state: r.get_quota_state(),
            face_value: r.get_u64(),
            spent: r.get_u64(),
            returned: r.get_u64(),
            close_digest: r.get_arr32(),
        };
        let returned_capacity = r.get_opt(|r| SourceReturnedCapacitySummary {
            capacity_lease_id: CapacityLeaseId::from_bytes(r.get_arr16()),
            state: r.get_cap_state(),
            amount: r.get_u64(),
            close_digest: r.get_arr32(),
        });
        let returned_device = r.get_opt(|r| SourceReturnedDeviceSummary {
            device_lease_id: ExclusiveDeviceLeaseId::from_bytes(r.get_arr16()),
            state: r.get_dev_state(),
            exclusivity_epoch: CellEpoch::from_u64(r.get_u64()).expect("epoch"),
            head_reset_generation: r.get_gen(),
            close_digest: r.get_arr32(),
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
            w.put_arr16(req.command.handle.capability_id.as_bytes());
            w.put_u64(req.command.handle.generation.get());
            w.put_arr16(req.command.revoker_key_id.as_bytes());
            w.put_arr16(req.command.idempotency_key.as_bytes());
            w.put_u64(req.command.revoked_at_ms);
            w.put_arr16(req.signer.as_bytes());
            w.put_arr64(&req.signature);
        }

        let body = self
            .call_rpc(CMD_REVOKE_CAPS, &w.into_vec())
            .map_err(ProcessMigrationDriveError::Transport)?;
        let mut r = WireReader::new(&body);
        let count = usize::try_from(r.get_u64()).expect("receipt count");
        let mut receipts = Vec::with_capacity(count);
        for _ in 0..count {
            receipts.push(CapabilityRevocationReceipt {
                receipt_id: ReceiptId::from_bytes(r.get_arr16()),
                capability_id: nlos_types::CapabilityId::from_bytes(r.get_arr16()),
                prior_generation: r.get_gen(),
                resulting_generation: r.get_gen(),
                revoker: PrincipalId::from_bytes(r.get_arr16()),
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
            .call_rpc(CMD_QUARANTINE_LEASES, &w.into_vec())
            .map_err(ProcessMigrationDriveError::Transport)?;
        let mut r = WireReader::new(&body);
        Ok(SourcePostFenceQuarantineReport {
            quota_state: r.get_quota_state(),
            capacity_state: r.get_opt(WireReader::get_cap_state),
            device_state: r.get_opt(WireReader::get_dev_state),
        })
    }
}

// ============================================================================
// Source Child OS Process Worker (`ADR-0018`: Process 1 = Source CellHost)
// ============================================================================

#[allow(clippy::too_many_lines)]
fn handle_source_rpc(
    cmd: u8,
    payload: &[u8],
    endpoint: &mut LocalCellHostSourceEndpoint<'_>,
) -> Result<Vec<u8>, String> {
    let mut r = WireReader::new(payload);
    let mut out = WireWriter::new();

    match cmd {
        CMD_INSPECT_PLACEMENT => {
            let process_id = ProcessId::from_bytes(r.get_arr16());
            let expected_target = r.get_cell_id();
            let snap = endpoint
                .inspect_source_placement(process_id, expected_target)
                .map_err(|e| e.to_string())?;
            out.put_arr16(snap.cell_id.as_bytes());
            out.put_u64(snap.node_boot_generation.get());
            out.put_u64(snap.epoch.get());
            out.put_u64(snap.fencing_token.get());
            out.put_bool(snap.both_cells_registered_in_directory);
            out.put_binding(&snap.active_binding);
            out.put_bool(snap.supervisor_registered);
            Ok(out.into_vec())
        }
        CMD_RECORD_INTENT => {
            let source = r.get_cell_id();
            let target = r.get_cell_id();
            let object = MigrationObject::from_bytes(r.get_arr16());
            let generation = r.get_gen();
            let reason = r.get_str();
            let intent = MigrationIntent::new(source, target, object, generation, &reason)
                .map_err(|e| e.to_string())?;
            let rec = endpoint
                .record_migration_intent(&intent)
                .map_err(|e| e.to_string())?;
            out.put_u32(rec.os_process_id());
            out.put_u64(rec.sequence());
            Ok(out.into_vec())
        }
        CMD_QUIESCE => {
            let process_id = ProcessId::from_bytes(r.get_arr16());
            let source_cell_id = r.get_cell_id();
            let source_generation = r.get_gen();
            let cancel_scope =
                r.get_opt(|r| (CancellationScopeId::from_bytes(r.get_arr16()), r.get_gen()));
            let active_fiber_count = r.get_u32();
            let op_count = usize::try_from(r.get_u64()).expect("op_count");
            let mut inflight_operations = Vec::with_capacity(op_count);
            for _ in 0..op_count {
                let op_handle = OperationHandle {
                    operation_id: OperationId::from_bytes(r.get_arr16()),
                    generation: r.get_gen(),
                };
                let receipt_id = ReceiptId::from_bytes(r.get_arr16());
                inflight_operations.push((op_handle, receipt_id));
            }
            let quiesced_at_ms = r.get_u64();
            let proof = endpoint
                .quiesce_source_process(&SourceQuiesceRequest {
                    process_id,
                    source_cell_id,
                    source_generation,
                    cancel_scope,
                    active_fiber_count,
                    inflight_operations,
                    quiesced_at_ms,
                })
                .map_err(|e| e.to_string())?;
            out.put_arr16(proof.process_id.as_bytes());
            out.put_arr16(&proof.source_cell_id);
            out.put_u64(proof.source_generation);
            out.put_u32(proof.quiesced_fibers);
            out.put_u32(proof.cancelled_inflight_ops);
            out.put_bool(proof.inflight_effects_drained);
            out.put_u64(proof.quiesced_at_ms);
            Ok(out.into_vec())
        }
        CMD_CAPTURE_CHECKPOINT => {
            let migration_id = r.get_arr16();
            let process_id = ProcessId::from_bytes(r.get_arr16());
            let source_cell_id = r.get_cell_id();
            let source_generation = r.get_gen();
            let migration_mode = r.get_migration_mode();
            let sess_count = usize::try_from(r.get_u64()).expect("sess_count");
            let mut migratable_sessions = Vec::with_capacity(sess_count);
            for _ in 0..sess_count {
                migratable_sessions.push((r.get_arr16(), r.get_arr32()));
            }
            let spec = r.get_checkpoint_spec();
            let bundle = endpoint
                .capture_source_checkpoint(&SourceCheckpointRequest {
                    migration_id,
                    process_id,
                    source_cell_id,
                    source_generation,
                    migration_mode,
                    migratable_sessions,
                    spec,
                })
                .map_err(|e| e.to_string())?;
            out.put_arr16(bundle.proof.process_id.as_bytes());
            out.put_u64(bundle.proof.source_generation);
            match &bundle.proof.payload {
                CheckpointPayloadKind::FullStateCheckpoint {
                    state_checkpoint_digest,
                    migrated_session_digests,
                } => {
                    out.put_u8(0);
                    out.put_arr32(state_checkpoint_digest);
                    out.put_u64(migrated_session_digests.len() as u64);
                    for (sid, sdig) in migrated_session_digests {
                        out.put_arr16(sid);
                        out.put_arr32(sdig);
                    }
                }
                CheckpointPayloadKind::ArtifactRestartImage {
                    artifact_image_digest,
                } => {
                    out.put_u8(1);
                    out.put_arr32(artifact_image_digest);
                }
            }
            out.put_u64(bundle.proof.checkpointed_at_ms);
            out.put_opt(
                bundle.transparent_cache_transfer.as_ref(),
                |w, (k, b, ts)| {
                    w.put_str(k);
                    w.put_bytes(b);
                    w.put_u64(*ts);
                },
            );
            out.put_opt(bundle.exported_artifact_bundle.as_ref(), |w, b| {
                w.put_arr16(b.artifact_id.as_bytes());
                w.put_str(&b.content_type);
                w.put_head_state(b.remote_head);
                w.put_opt(b.parent_head, WireWriter::put_head_state);
                w.put_u64(b.ancestor_lineage.len() as u64);
                for anc in &b.ancestor_lineage {
                    w.put_head_state(*anc);
                }
                w.put_arr16(&b.origin_cell_id);
                w.put_bytes(&b.bytes);
                w.put_bool(b.is_tombstone);
            });
            Ok(out.into_vec())
        }
        CMD_FENCE_SOURCE => {
            let process_id = ProcessId::from_bytes(r.get_arr16());
            let source_cell_id = r.get_cell_id();
            let source_generation = r.get_gen();
            let path_count = usize::try_from(r.get_u64()).expect("path_count");
            let mut invalidate_name_paths = Vec::with_capacity(path_count);
            for _ in 0..path_count {
                invalidate_name_paths.push(NamePath::new(&r.get_str()).expect("NamePath"));
            }
            let terminate_idempotency_key = IdempotencyKey::from_bytes(r.get_arr16());
            let stale_dispatch_probe_fiber_id = ExecutionFiberId::from_bytes(r.get_arr16());
            let stale_dispatch_probe_key = IdempotencyKey::from_bytes(r.get_arr16());
            let fenced_at_ms = r.get_u64();

            let report = endpoint
                .fence_source_process(&SourceFenceRequest {
                    process_id,
                    source_cell_id,
                    source_generation,
                    invalidate_name_paths,
                    terminate_idempotency_key,
                    stale_dispatch_probe_fiber_id,
                    stale_dispatch_probe_key,
                    fenced_at_ms,
                })
                .map_err(|e| e.to_string())?;
            out.put_arr16(report.proof.process_id.as_bytes());
            out.put_arr16(&report.proof.source_cell_id);
            out.put_u64(report.proof.source_cell_epoch);
            out.put_u64(report.proof.fenced_generation);
            out.put_arr32(&report.proof.source_fencing_token);
            out.put_bool(report.proof.supervisor_unregistered);
            out.put_u64(report.proof.fenced_at_ms);
            out.put_bool(report.stale_heartbeat_refused);
            out.put_bool(report.stale_dispatch_refused);
            out.put_u64(report.invalidated_name_paths as u64);
            Ok(out.into_vec())
        }
        CMD_CLOSE_LEASES => {
            let spec = r.get_lease_bundle_spec();
            let closed = endpoint
                .close_or_fence_source_leases(&spec)
                .map_err(|e| e.to_string())?;
            out.put_arr16(closed.closed_quota.lease_id.as_bytes());
            out.put_quota_state(closed.closed_quota.state);
            out.put_u64(closed.closed_quota.face_value);
            out.put_u64(closed.closed_quota.spent);
            out.put_u64(closed.closed_quota.returned);
            out.put_arr32(&closed.closed_quota.close_digest);
            out.put_opt(closed.returned_capacity, |w, cap| {
                w.put_arr16(cap.capacity_lease_id.as_bytes());
                w.put_cap_state(cap.state);
                w.put_u64(cap.amount);
                w.put_arr32(&cap.close_digest);
            });
            out.put_opt(closed.returned_device, |w, dev| {
                w.put_arr16(dev.device_lease_id.as_bytes());
                w.put_dev_state(dev.state);
                w.put_u64(dev.exclusivity_epoch.get());
                w.put_u64(dev.head_reset_generation.get());
                w.put_arr32(&dev.close_digest);
            });
            Ok(out.into_vec())
        }
        CMD_REVOKE_CAPS => {
            let count = usize::try_from(r.get_u64()).expect("revocation count");
            let mut reqs = Vec::with_capacity(count);
            for _ in 0..count {
                let handle = CapabilityHandle {
                    capability_id: nlos_types::CapabilityId::from_bytes(r.get_arr16()),
                    generation: r.get_gen(),
                };
                let revoker_key_id = KeyId::from_bytes(r.get_arr16());
                let idempotency_key = IdempotencyKey::from_bytes(r.get_arr16());
                let revoked_at_ms = r.get_u64();
                let signer = PrincipalId::from_bytes(r.get_arr16());
                let signature = r.get_arr64();
                reqs.push(SignedRevokeCapabilityRequest {
                    command: RevokeCapabilityRequest {
                        handle,
                        revoker_key_id,
                        idempotency_key,
                        revoked_at_ms,
                    },
                    signer,
                    signature,
                });
            }
            let receipts = endpoint
                .revoke_source_capabilities(&reqs)
                .map_err(|e| e.to_string())?;
            out.put_u64(receipts.len() as u64);
            for rec in receipts {
                out.put_arr16(rec.receipt_id.as_bytes());
                out.put_arr16(rec.capability_id.as_bytes());
                out.put_u64(rec.prior_generation.get());
                out.put_u64(rec.resulting_generation.get());
                out.put_arr16(rec.revoker.as_bytes());
                out.put_u64(rec.revoked_at_ms);
            }
            Ok(out.into_vec())
        }
        CMD_QUARANTINE_LEASES => {
            let spec = r.get_lease_bundle_spec();
            let q = endpoint
                .quarantine_unsettled_source_leases(&spec)
                .map_err(|e| e.to_string())?;
            out.put_quota_state(q.quota_state);
            out.put_opt(q.capacity_state, WireWriter::put_cap_state);
            out.put_opt(q.device_state, WireWriter::put_dev_state);
            Ok(out.into_vec())
        }
        other => Err(format!("unknown IPC command {other}")),
    }
}

#[allow(clippy::too_many_lines)]
fn verify_source_post_conditions(
    scenario: ScenarioId,
    source_host: &mut CellHost,
    binding: &ProcessBindingRecord,
    cap_fixtures: &CapabilityFixtures,
) {
    let base_ms = binding.created_at_ms + 1_000;
    let cap_path = NamePath::new(CAP_NAME_PATH).expect("cap path");
    let source_quota_id = QuotaLeaseId::from_bytes(SOURCE_QUOTA_BYTES);
    let source_cap_id = CapacityLeaseId::from_bytes(SOURCE_CAP_LEASE_BYTES);
    let source_dev_id = ExclusiveDeviceLeaseId::from_bytes(SOURCE_DEV_LEASE_BYTES);

    match scenario {
        ScenarioId::TransparentHappyPath | ScenarioId::PinnedDowngradeArtifact => {
            assert!(matches!(
                source_host
                    .runtime()
                    .process
                    .inspect_active_process_binding(binding.process_id),
                Err(ProcessAuthorityError::ProcessBindingTerminal(
                    ProcessLifecycleState::Terminated
                ))
            ));
            assert!(
                source_host
                    .supervisor_pids()
                    .lookup(binding.process_id)
                    .is_err()
            );
            assert_eq!(source_host.cached_name(&cap_path), None);
            let q = source_host
                .query_quota_lease(source_quota_id)
                .expect("query source quota");
            assert_eq!(q.state(), QuotaLeaseState::Closed);
            assert_eq!(q.returned(), 65);
            assert_eq!(source_host.quota_available(), 965);
            let op_snap = source_host
                .runtime()
                .operations
                .inspect(OperationHandle {
                    operation_id: OperationId::from_bytes(OP_BYTES),
                    generation: Generation::INITIAL,
                })
                .expect("inflight op snapshot");
            assert_eq!(op_snap.state, OperationState::CancelRequested);
            assert!(matches!(
                inspect_host_active_capability(
                    source_host,
                    cap_fixtures.root_record.handle,
                    base_ms + 310
                ),
                Err(CapabilityAuthorityError::GenerationFenceConflict)
            ));
            let gen_two_handle = CapabilityHandle {
                capability_id: cap_fixtures.root_record.handle.capability_id,
                generation: Generation::new(NonZeroU64::new(2).expect("gen 2")),
            };
            assert!(matches!(
                inspect_host_active_capability(source_host, gen_two_handle, base_ms + 310),
                Err(CapabilityAuthorityError::CapabilityRevoked)
            ));
        }
        ScenarioId::MultiFamilyLeases => {
            let q = source_host
                .query_quota_lease(source_quota_id)
                .expect("query quota");
            assert_eq!(q.state(), QuotaLeaseState::Closed);
            assert_eq!(q.returned(), 150);
            assert_eq!(source_host.quota_available(), 950);

            let c = source_host
                .query_capacity_lease(source_cap_id)
                .expect("query capacity");
            assert_eq!(c.state(), CapacityLeaseState::Returned);
            assert_eq!(source_host.capacity_pool_remaining(), 500);

            let d = source_host
                .query_device_lease(source_dev_id)
                .expect("query device");
            assert_eq!(d.state(), ExclusiveDeviceLeaseState::Returned);
            assert!(source_host.device_head_free());
            assert_eq!(source_host.device_reset_generation().get(), 2);
        }
        ScenarioId::SourceFencingRefusal => {
            assert!(matches!(
                source_host.record_process_heartbeat(binding.process_id),
                Err(SliceKError::FailureDetector(
                    FailureDetectorError::SubjectAlreadyDead { .. }
                ))
            ));
            assert!(matches!(
                source_host
                    .record_fenced_process_heartbeat(binding.process_id, Generation::INITIAL),
                Err(SliceKError::Process(
                    ProcessAuthorityError::ProcessBindingTerminal(
                        ProcessLifecycleState::Terminated
                    )
                ))
            ));
            let stale_fiber = source_host.runtime().process.register_fiber_incarnation(
                RegisterFiberIncarnationRequest {
                    process_id: binding.process_id,
                    expected_process_generation: Generation::INITIAL,
                    expected_process_fencing_token: binding.process_fencing_token,
                    binding: ExecutionFiberId::from_bytes(FIBER_99_BYTES),
                    idempotency_key: seeded_key(0xEE, 1),
                    registered_at_ms: base_ms + 999,
                },
            );
            assert!(matches!(
                stale_fiber,
                Err(ProcessAuthorityError::ProcessBindingTerminal(
                    ProcessLifecycleState::Terminated
                ))
            ));
            assert!(
                source_host
                    .supervisor_pids()
                    .lookup(binding.process_id)
                    .is_err()
            );
            assert_eq!(source_host.cached_name(&cap_path), None);
            assert!(matches!(
                source_host.insert_cached_name(
                    cap_path,
                    cap_fixtures.root_record.handle.capability_id,
                    Generation::INITIAL
                ),
                Err(SliceKError::NameCache(
                    NameCacheError::StaleGeneration { .. }
                ))
            ));
            assert_eq!(
                source_host
                    .failure_detector()
                    .liveness(MonitoredSubject::Process(binding.process_id))
                    .expect("detector view")
                    .liveness,
                Liveness::Dead
            );
        }
        ScenarioId::PinnedRefuse => {
            let active = source_host
                .runtime()
                .process
                .inspect_active_process_binding(binding.process_id)
                .expect("still active on source");
            assert_eq!(active.process_generation, Generation::INITIAL);
            let sup = source_host
                .supervisor_pids()
                .lookup(binding.process_id)
                .expect("still registered in source supervisor");
            assert_eq!(sup.process_generation, Generation::INITIAL);
            assert_eq!(sup.os_pid, source_host.os_process_id());
            assert!(source_host.cached_name(&cap_path).is_some());
            let q = source_host
                .query_quota_lease(source_quota_id)
                .expect("source quota");
            assert_eq!(q.state(), QuotaLeaseState::Active);
            assert_eq!(source_host.quota_available(), 900);
        }
        ScenarioId::PostFenceCapAmplificationQuarantine => {
            assert!(matches!(
                source_host
                    .runtime()
                    .process
                    .inspect_active_process_binding(binding.process_id),
                Err(ProcessAuthorityError::ProcessBindingTerminal(
                    ProcessLifecycleState::Terminated
                ))
            ));
            assert_eq!(
                source_host
                    .query_quota_lease(source_quota_id)
                    .expect("quota")
                    .state(),
                QuotaLeaseState::Quarantined
            );
            assert_eq!(source_host.quota_available(), 900);

            assert_eq!(
                source_host
                    .query_capacity_lease(source_cap_id)
                    .expect("cap lease")
                    .state(),
                CapacityLeaseState::Quarantined
            );
            assert_eq!(source_host.capacity_pool_remaining(), 380);

            assert_eq!(
                source_host
                    .query_device_lease(source_dev_id)
                    .expect("dev lease")
                    .state(),
                ExclusiveDeviceLeaseState::Quarantined
            );
            assert!(!source_host.device_head_free());
        }
        ScenarioId::PostFenceLeaseConservationQuarantine => {
            assert!(matches!(
                source_host
                    .runtime()
                    .process
                    .inspect_active_process_binding(binding.process_id),
                Err(ProcessAuthorityError::ProcessBindingTerminal(
                    ProcessLifecycleState::Terminated
                ))
            ));
            assert_eq!(
                source_host
                    .query_quota_lease(source_quota_id)
                    .expect("quota")
                    .state(),
                QuotaLeaseState::Closed
            );
            assert_eq!(source_host.quota_available(), 965);
        }
    }
}

#[test]
#[ignore = "subprocess worker invoked by dual_cell_process_migration tests"]
#[allow(clippy::too_many_lines)]
fn child_source_cell_host_worker() {
    let scenario = ScenarioId::from_str(&std::env::var(ENV_SCENARIO).expect("ENV_SCENARIO"));
    let federation_root = PathBuf::from(std::env::var(ENV_FEDERATION_ROOT).expect("FED_ROOT"));
    let cell_root = PathBuf::from(std::env::var(ENV_CELL_ROOT).expect("CELL_ROOT"));
    let ipc_dir = PathBuf::from(std::env::var(ENV_IPC_DIR).expect("IPC_DIR"));

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("tokio runtime");
    let adapter =
        TokioRuntimeAdapter::new(rt.handle().clone(), TokioRuntimeConfig::default()).expect("adap");

    let mut source_host =
        CellHost::open_federated(&cell_root, source_cell_config(), &federation_root)
            .expect("open federated source CellHost");

    let binding = source_host
        .spawn_monitored_process(
            0xA5,
            TaskId::from_bytes(TASK_BYTES),
            TaskAttemptId::from_bytes(ATTEMPT_BYTES),
            Generation::INITIAL,
        )
        .expect("spawn monitored process on source");
    let base_ms = binding.created_at_ms + 1_000;

    // Register durable fiber incarnation + live Tokio fiber + in-flight operation.
    source_host
        .runtime()
        .process
        .register_fiber_incarnation(RegisterFiberIncarnationRequest {
            process_id: binding.process_id,
            expected_process_generation: Generation::INITIAL,
            expected_process_fencing_token: binding.process_fencing_token,
            binding: ExecutionFiberId::from_bytes(FIBER_11_BYTES),
            idempotency_key: seeded_key(0xA5, 200),
            registered_at_ms: binding.created_at_ms + 10,
        })
        .expect("register durable fiber incarnation");

    let scope_id = CancellationScopeId::from_bytes(SCOPE_BYTES);
    let _fiber_handle = RuntimeAdapter::spawn_fiber(
        &adapter,
        FiberSpec {
            fiber_id: ExecutionFiberId::from_bytes(FIBER_11_BYTES),
            fiber_generation: Generation::INITIAL,
            agent_instance_id: binding.agent_instance_id,
            agent_generation: binding.agent_instance_generation,
            process_id: binding.process_id,
            process_generation: binding.process_generation,
            task_attempt_id: Some(binding.task_attempt_id),
            cancellation_scope_id: scope_id,
            cancellation_generation: Generation::INITIAL,
            resource_group_id: ResourceGroupId::from_bytes([0x55; 16]),
            scheduler_domain_id: SchedulerDomainId::from_bytes(SOURCE_DOMAIN_BYTES),
            deadline: None,
        },
        Box::pin(async {
            tokio::time::sleep(Duration::from_secs(60)).await;
            FiberExit::Cancelled
        }),
    )
    .expect("spawn runtime fiber on source");

    let op_handle = source_host
        .runtime()
        .operations
        .register(OperationSpec {
            operation_id: OperationId::from_bytes(OP_BYTES),
            generation: Generation::INITIAL,
            owner_fiber: FiberHandle {
                fiber_id: ExecutionFiberId::from_bytes(FIBER_11_BYTES),
                generation: Generation::INITIAL,
            },
            cancellation_scope_id: scope_id,
            cancellation_generation: Generation::INITIAL,
        })
        .expect("register inflight op")
        .handle();
    let _ticket = source_host
        .runtime()
        .operations
        .dispatch(op_handle, CallbackId::from_bytes(CALLBACK_BYTES))
        .expect("dispatch inflight op");

    // Bootstrap capabilities & cache NamePath on Source.
    let amplify = scenario == ScenarioId::PostFenceCapAmplificationQuarantine;
    let cap_fixtures = seed_source_capabilities(&source_host, amplify, base_ms);
    let cap_path = NamePath::new(CAP_NAME_PATH).expect("cap path");
    assert_eq!(
        source_host
            .insert_cached_name(
                cap_path,
                cap_fixtures.root_record.handle.capability_id,
                Generation::INITIAL
            )
            .expect("cache cap on source"),
        InsertOutcome::Inserted
    );

    // Setup scenario-specific leases on Source.
    let source_quota_id = QuotaLeaseId::from_bytes(SOURCE_QUOTA_BYTES);
    match scenario {
        ScenarioId::MultiFamilyLeases => {
            source_host
                .grant_quota_lease(source_quota_id, 200, None)
                .expect("grant quota 200");
            source_host
                .activate_quota_lease(source_quota_id)
                .expect("activate quota");
            source_host
                .report_quota_usage(source_quota_id, 50)
                .expect("spend 50");
            source_host
                .grant_capacity_lease(CapacityLeaseId::from_bytes(SOURCE_CAP_LEASE_BYTES), 120)
                .expect("grant capacity 120");
            source_host
                .grant_device_lease(ExclusiveDeviceLeaseId::from_bytes(SOURCE_DEV_LEASE_BYTES))
                .expect("grant exclusive device");
        }
        ScenarioId::PostFenceCapAmplificationQuarantine => {
            source_host
                .grant_quota_lease(source_quota_id, 100, None)
                .expect("grant quota 100");
            source_host
                .activate_quota_lease(source_quota_id)
                .expect("activate quota");
            source_host
                .grant_capacity_lease(CapacityLeaseId::from_bytes(SOURCE_CAP_LEASE_BYTES), 120)
                .expect("grant capacity 120");
            source_host
                .grant_device_lease(ExclusiveDeviceLeaseId::from_bytes(SOURCE_DEV_LEASE_BYTES))
                .expect("grant exclusive device");
        }
        _ => {
            source_host
                .grant_quota_lease(source_quota_id, 100, None)
                .expect("grant quota 100");
            source_host
                .activate_quota_lease(source_quota_id)
                .expect("activate quota");
            source_host
                .report_quota_usage(source_quota_id, 35)
                .expect("spend 35");
        }
    }

    // Commit restart-image artifact on Source when testing DowngradeToRestartFromArtifact.
    let artifact_image_digest = if scenario == ScenarioId::PinnedDowngradeArtifact {
        let (_rev, digest) = publish_restart_image_artifact(
            &source_host,
            ArtifactId::from_bytes(ARTIFACT_ID_BYTES),
            0x88,
            ARTIFACT_CONTENT_TYPE,
            ARTIFACT_IMAGE_BYTES,
            base_ms + 25,
        )
        .expect("publish restart image artifact on source");
        Some(digest.into_bytes())
    } else {
        None
    };

    // Publish `source_ready.bin` for the Target driver process.
    let ready = SourceReadyPayload {
        source_os_pid: source_host.os_process_id(),
        source_cell_id: source_host.fence().identity(),
        spawned_binding: binding.clone(),
        artifact_image_digest,
    };
    write_atomic_bytes(&ipc_dir.join("source_ready.bin"), &ready.encode());

    // Service incoming step RPCs until `CMD_FINISH_VERIFY`.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut processed = std::collections::HashSet::<PathBuf>::new();
    let mut finished = false;
    while !finished {
        assert!(
            Instant::now() < deadline,
            "source worker timed out waiting for CMD_FINISH_VERIFY"
        );
        let mut req_files: Vec<PathBuf> = std::fs::read_dir(&ipc_dir)
            .expect("read ipc_dir")
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| {
                p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                    n.starts_with("req_")
                        && std::path::Path::new(n)
                            .extension()
                            .is_some_and(|ext| ext.eq_ignore_ascii_case("bin"))
                }) && !processed.contains(p)
            })
            .collect();
        req_files.sort();

        if req_files.is_empty() {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        }

        for req_path in req_files {
            let Ok(raw_req) = std::fs::read(&req_path) else {
                continue;
            };
            processed.insert(req_path.clone());
            let file_stem = req_path
                .file_name()
                .and_then(|s| s.to_str())
                .expect("req filename")
                .replacen("req_", "resp_", 1);
            let resp_path = ipc_dir.join(file_stem);

            let cmd = raw_req[0];
            if cmd == CMD_ABORT_WORKER {
                finished = true;
                break;
            }
            if cmd == CMD_FINISH_VERIFY {
                let check = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    verify_source_post_conditions(
                        scenario,
                        &mut source_host,
                        &binding,
                        &cap_fixtures,
                    );
                }));
                match check {
                    Ok(()) => {
                        let mut ok = WireWriter::new();
                        ok.put_bool(true);
                        write_atomic_bytes(&resp_path, &ok.into_vec());
                        finished = true;
                        break;
                    }
                    Err(payload) => {
                        let mut err = WireWriter::new();
                        err.put_bool(false);
                        err.put_str("verify_source_post_conditions panicked");
                        write_atomic_bytes(&resp_path, &err.into_vec());
                        std::panic::resume_unwind(payload);
                    }
                }
            }

            let mut local_ep =
                LocalCellHostSourceEndpoint::with_adapter(&mut source_host, &adapter);
            match handle_source_rpc(cmd, &raw_req[1..], &mut local_ep) {
                Ok(body) => {
                    let mut resp = Vec::with_capacity(1 + body.len());
                    resp.push(1u8);
                    resp.extend_from_slice(&body);
                    write_atomic_bytes(&resp_path, &resp);
                }
                Err(err_msg) => {
                    let mut resp = WireWriter::new();
                    resp.put_bool(false);
                    resp.put_str(&err_msg);
                    write_atomic_bytes(&resp_path, &resp.into_vec());
                }
            }
        }
    }

    drop(source_host);
    drop(adapter);
    rt.shutdown_background();
}

// ============================================================================
// Target Child OS Process Driver (`ADR-0018`: Process 2 = Target CellHost)
// ============================================================================

#[allow(clippy::too_many_lines)]
fn build_migration_request(
    scenario: ScenarioId,
    ready: &SourceReadyPayload,
    target_cell_id: CellIdentity,
) -> CellHostProcessMigrationRequest {
    let base_ms = ready.spawned_binding.created_at_ms + 1_000;
    let amplify = scenario == ScenarioId::PostFenceCapAmplificationQuarantine;
    let issuer_sk = SigningKey::from_bytes(&[0x11; 32]);
    let holder_sk = SigningKey::from_bytes(&[0x22; 32]);
    let issuer_bootstrap = BootstrapPrincipalRequest {
        principal_profile_digest: [0x12; 32],
        control_domain_policy_digest: [0x13; 32],
        public_key: issuer_sk.verifying_key().to_bytes(),
        key_purpose: KeyPurpose::SemanticSigning,
        key_valid_from_ms: 0,
        key_valid_until_ms: base_ms + 86_400_000,
        idempotency_key: seeded_key(0x90, 1),
        created_at_ms: base_ms + 100,
    };
    let holder_bootstrap = BootstrapPrincipalRequest {
        principal_profile_digest: [0x23; 32],
        control_domain_policy_digest: [0x24; 32],
        public_key: holder_sk.verifying_key().to_bytes(),
        key_purpose: KeyPurpose::SemanticSigning,
        key_valid_from_ms: 0,
        key_valid_until_ms: base_ms + 86_400_000,
        idempotency_key: seeded_key(0x90, 2),
        created_at_ms: base_ms + 100,
    };

    // Derive deterministic IDs in a disposable scratch store so Target's durable
    // authorities stay completely untouched prior to Step 5 (`CapabilitiesRebound`).
    let scratch = TempDir::new("cap-derive-scratch");
    let scratch_id = nlos_identity::IdentityAuthority::open(scratch.root()).expect("scratch id");
    let scratch_cap =
        nlos_capability::CapabilityAuthority::open(scratch.root()).expect("scratch cap");
    let issuer_binding = scratch_id
        .bootstrap_principal(issuer_bootstrap)
        .expect("scratch issuer")
        .binding();
    let holder_binding = scratch_id
        .bootstrap_principal(holder_bootstrap)
        .expect("scratch holder")
        .binding();
    let parent_rights = if amplify {
        CapabilityRights::DELEGATE
    } else {
        CapabilityRights::DELEGATE.union(CapabilityRights::SEMANTIC_APPEND)
    };
    let root_cmd = IssueRootCapabilityRequest {
        issuer_key_id: issuer_binding.key_id,
        holder_key_id: holder_binding.key_id,
        target: CapabilityTarget::Namespace(NamespaceId::from_bytes([0x99; 16])),
        rights: parent_rights,
        purpose_digest: Some([0xAA; 32]),
        valid_from_ms: base_ms + 100,
        valid_until_ms: base_ms + 500_000,
        delegation_depth_remaining: 2,
        call_limit: Some(10),
        idempotency_key: seeded_key(0x90, 3),
        issued_at_ms: base_ms + 150,
    };
    let signed_root = SignedIssueRootCapabilityRequest {
        command: root_cmd,
        signer: issuer_binding.principal_id,
        signature: issuer_sk
            .sign(&issue_root_command_message(root_cmd))
            .to_bytes(),
    };
    let root_record = scratch_cap
        .issue_root_signed(&scratch_id, signed_root)
        .expect("scratch root")
        .record();
    drop(scratch_cap);
    drop(scratch_id);
    drop(scratch);

    let child_rights = if amplify {
        CapabilityRights::DELEGATE.union(CapabilityRights::SEMANTIC_APPEND)
    } else {
        CapabilityRights::SEMANTIC_APPEND
    };
    let delegate_cmd = DelegateCapabilityRequest {
        parent: root_record.handle,
        delegator_key_id: holder_binding.key_id,
        recipient_key_id: holder_binding.key_id,
        target: CapabilityTarget::Namespace(NamespaceId::from_bytes([0x99; 16])),
        rights: child_rights,
        purpose_digest: Some([0xAA; 32]),
        valid_from_ms: base_ms + 200,
        valid_until_ms: base_ms + 400_000,
        delegation_depth_remaining: 1,
        call_limit: Some(5),
        idempotency_key: seeded_key(0x90, 4),
        delegated_at_ms: base_ms + 250,
    };
    let signed_delegation = SignedDelegateCapabilityRequest {
        command: delegate_cmd,
        signer: holder_binding.principal_id,
        signature: holder_sk
            .sign(&delegate_command_message(delegate_cmd))
            .to_bytes(),
    };
    let revoke_cmd = RevokeCapabilityRequest {
        handle: root_record.handle,
        revoker_key_id: issuer_binding.key_id,
        idempotency_key: seeded_key(0x90, 5),
        revoked_at_ms: base_ms + 300,
    };
    let signed_revoke = SignedRevokeCapabilityRequest {
        command: revoke_cmd,
        signer: issuer_binding.principal_id,
        signature: issuer_sk
            .sign(&revoke_command_message(revoke_cmd))
            .to_bytes(),
    };

    let (attached_sessions, pinned_session_policy, checkpoint) = match scenario {
        ScenarioId::PinnedRefuse => (
            vec![DriverSessionAttachment::PinnedNonMigratable {
                session_id: [0x77; 16],
                pin_reason: "nvme-vfio-bar0".to_string(),
                artifact_restart_image_digest: None,
            }],
            PinnedSessionPolicy::RefuseMigration,
            CheckpointSourceSpec::TransparentState {
                state_payload: TRANSPARENT_STATE_BYTES.to_vec(),
                cache_key: Some(TRANSPARENT_CACHE_KEY.to_string()),
                checkpointed_at_ms: base_ms + 220,
            },
        ),
        ScenarioId::PinnedDowngradeArtifact => (
            vec![
                DriverSessionAttachment::Migratable {
                    session_id: [0x87; 16],
                    state_digest: [0x86; 32],
                },
                DriverSessionAttachment::PinnedNonMigratable {
                    session_id: [0x88; 16],
                    pin_reason: "gpu-cuda-ctx".to_string(),
                    artifact_restart_image_digest: Some(
                        ready
                            .artifact_image_digest
                            .expect("artifact digest from source"),
                    ),
                },
            ],
            PinnedSessionPolicy::DowngradeToRestartFromArtifact,
            CheckpointSourceSpec::ArtifactRestartImage {
                artifact_id: ArtifactId::from_bytes(ARTIFACT_ID_BYTES),
                revision: 1,
                sync_from_source_if_missing: true,
                checkpointed_at_ms: base_ms + 220,
            },
        ),
        _ => (
            vec![DriverSessionAttachment::Migratable {
                session_id: [0x45; 16],
                state_digest: [0x46; 32],
            }],
            PinnedSessionPolicy::RefuseMigration,
            CheckpointSourceSpec::TransparentState {
                state_payload: TRANSPARENT_STATE_BYTES.to_vec(),
                cache_key: Some(TRANSPARENT_CACHE_KEY.to_string()),
                checkpointed_at_ms: base_ms + 220,
            },
        ),
    };

    let target_face_value = if scenario == ScenarioId::PostFenceLeaseConservationQuarantine {
        Some(80)
    } else {
        None
    };

    let include_cap_and_dev = matches!(
        scenario,
        ScenarioId::MultiFamilyLeases | ScenarioId::PostFenceCapAmplificationQuarantine
    );
    let leases = LeaseMigrationBundleSpec {
        quota: QuotaLeaseMigrationSpec {
            source_lease_id: QuotaLeaseId::from_bytes(SOURCE_QUOTA_BYTES),
            gateway_id: [0xC1; 16],
            override_fence_ack: None,
            target_lease_id: QuotaLeaseId::from_bytes(TARGET_QUOTA_BYTES),
            target_face_value,
            target_expires_at: None,
            activate_on_target: true,
        },
        capacity: include_cap_and_dev.then_some(CapacityLeaseMigrationSpec {
            source_lease_id: CapacityLeaseId::from_bytes(SOURCE_CAP_LEASE_BYTES),
            reconcile_receipt_id: ReceiptId::from_bytes([0xC2; 16]),
            gateway_id: [0xC3; 16],
            target_lease_id: CapacityLeaseId::from_bytes(TARGET_CAP_LEASE_BYTES),
            target_amount: None,
        }),
        device: include_cap_and_dev.then_some(DeviceLeaseMigrationSpec {
            source_lease_id: ExclusiveDeviceLeaseId::from_bytes(SOURCE_DEV_LEASE_BYTES),
            reset_receipt_id: ReceiptId::from_bytes([0xC4; 16]),
            reconcile_receipt_id: ReceiptId::from_bytes([0xC5; 16]),
            gateway_id: [0xC6; 16],
            target_lease_id: ExclusiveDeviceLeaseId::from_bytes(TARGET_DEV_LEASE_BYTES),
            advance_target_epoch_if_needed: true,
        }),
        regranted_at_ms: base_ms + 260,
    };

    let cap_path = NamePath::new(CAP_NAME_PATH).expect("cap path");
    CellHostProcessMigrationRequest {
        migration_id: MIGRATION_ID_BYTES,
        process_id: ready.spawned_binding.process_id,
        source_cell_id: ready.source_cell_id,
        target_cell_id,
        source_generation: Generation::INITIAL,
        lineage: ProcessLineageSpec {
            genesis: ProcessLineageGenesis::from_spawned_binding(0xA5, &ready.spawned_binding),
            prior_restores: Vec::new(),
        },
        attached_sessions,
        pinned_session_policy,
        custom_intent_reason: None,
        initiated_at_ms: base_ms + 200,
        quiesce: SourceQuiesceSpec {
            cancel_scope: Some((
                CancellationScopeId::from_bytes(SCOPE_BYTES),
                Generation::INITIAL,
            )),
            active_fiber_count: 1,
            inflight_operations: vec![(
                OperationHandle {
                    operation_id: OperationId::from_bytes(OP_BYTES),
                    generation: Generation::INITIAL,
                },
                ReceiptId::from_bytes(CANCEL_RECEIPT_BYTES),
            )],
            quiesced_at_ms: base_ms + 210,
        },
        checkpoint,
        artifact_sync_seed: 0xD1,
        source_fence: SourceFenceSpec {
            invalidate_name_paths: vec![cap_path.clone()],
            terminate_idempotency_key: seeded_key(0xA5, 222),
            stale_dispatch_probe_fiber_id: ExecutionFiberId::from_bytes(FIBER_77_BYTES),
            stale_dispatch_probe_key: seeded_key(0xA5, 223),
            fenced_at_ms: base_ms + 230,
        },
        capabilities: vec![CapabilityRebindItemSpec {
            bootstrap_principals: vec![issuer_bootstrap, holder_bootstrap],
            seed_parent_root: Some(signed_root),
            signed_delegation,
            target_name_path: Some(cap_path),
            revoke_on_source: Some(signed_revoke),
        }],
        capabilities_rebound_at_ms: base_ms + 240,
        leases,
        target_restore: TargetRestoreSpec {
            existing_target_domain_id: None,
            domain_policy_digest: [0xE1; 32],
            domain_idempotency_key: seeded_key(0xB6, 1),
            restore_idempotency_key: seeded_key(0xB6, 2),
            restored_at_ms: base_ms + 270,
        },
        completed_at_ms: base_ms + 280,
    }
}

#[test]
#[ignore = "subprocess driver invoked by dual_cell_process_migration tests"]
#[allow(clippy::too_many_lines)]
fn child_target_cell_host_driver() {
    let scenario = ScenarioId::from_str(&std::env::var(ENV_SCENARIO).expect("ENV_SCENARIO"));
    let federation_root = PathBuf::from(std::env::var(ENV_FEDERATION_ROOT).expect("FED_ROOT"));
    let cell_root = PathBuf::from(std::env::var(ENV_CELL_ROOT).expect("CELL_ROOT"));
    let ipc_dir = PathBuf::from(std::env::var(ENV_IPC_DIR).expect("IPC_DIR"));

    let mut target_host =
        CellHost::open_federated(&cell_root, target_cell_config(), &federation_root)
            .expect("open federated target CellHost");
    let target_cell_id = target_host.fence().identity();

    let ready_bytes = wait_for_file(&ipc_dir.join("source_ready.bin"));
    let ready = SourceReadyPayload::decode(&ready_bytes);
    assert_ne!(
        ready.source_os_pid,
        target_host.os_process_id(),
        "Source and Target CellHost MUST run in distinct OS processes (ADR-0018)"
    );

    let request = build_migration_request(scenario, &ready, target_cell_id);
    let mut ipc_source = IpcSourceCellEndpoint::new(ipc_dir, federation_root);
    let drive_result = {
        let mut local_target = LocalCellHostTargetEndpoint::new(&mut target_host);
        drive_cell_host_process_migration(&mut ipc_source, &mut local_target, request)
    };

    let gen_two = Generation::new(NonZeroU64::new(2).expect("gen 2"));
    let cap_path = NamePath::new(CAP_NAME_PATH).expect("cap path");
    let target_quota_id = QuotaLeaseId::from_bytes(TARGET_QUOTA_BYTES);
    let target_cap_id = CapacityLeaseId::from_bytes(TARGET_CAP_LEASE_BYTES);
    let target_dev_id = ExclusiveDeviceLeaseId::from_bytes(TARGET_DEV_LEASE_BYTES);

    match scenario {
        ScenarioId::TransparentHappyPath => {
            let outcome = drive_result.expect("transparent dual-Cell migration must succeed");
            assert_eq!(
                outcome.coordinator.current_step(),
                ProcessMigrationStep::Completed
            );
            assert!(outcome.receipt.verify_digest());
            assert_eq!(outcome.receipt.source_generation, 1);
            assert_eq!(outcome.receipt.target_generation, 2);
            assert_eq!(
                outcome.receipt.migration_mode,
                MigrationMode::TransparentCheckpointRestore
            );
            assert!(outcome.source_fence_report.stale_heartbeat_refused);
            assert!(outcome.source_fence_report.stale_dispatch_refused);
            assert_eq!(outcome.source_revocation_receipts.len(), 1);

            let active = target_host
                .runtime()
                .process
                .inspect_active_process_binding(ready.spawned_binding.process_id)
                .expect("restored active on target");
            assert_eq!(active.process_generation, gen_two);
            assert_ne!(
                active.process_fencing_token,
                ready.spawned_binding.process_fencing_token
            );
            let sup = target_host
                .supervisor_pids()
                .lookup(ready.spawned_binding.process_id)
                .expect("target supervisor entry");
            assert_eq!(sup.process_generation, gen_two);
            assert_eq!(sup.os_pid, target_host.os_process_id());

            let cached_bytes = target_host
                .runtime()
                .artifacts
                .get_cache_blob(TRANSPARENT_CACHE_KEY)
                .expect("read target cache")
                .expect("cache present on target");
            assert_eq!(cached_bytes, TRANSPARENT_STATE_BYTES);
            let name_hit = target_host
                .cached_name(&cap_path)
                .expect("target name cache hit");
            assert_eq!(name_hit.cached.capability_generation, gen_two);
            assert_eq!(
                name_hit.cached.capability,
                outcome.capability_rebind_report.delegated_records[0]
                    .0
                    .handle
                    .capability_id
            );
            let q = target_host
                .query_quota_lease(target_quota_id)
                .expect("target quota");
            assert_eq!(q.state(), QuotaLeaseState::Active);
            assert_eq!(q.face_value(), 65);
            assert_eq!(target_host.quota_available(), 935);

            assert_eq!(
                outcome
                    .target_checkpoint_report
                    .checkpoint_record
                    .fact()
                    .verify_digest(&outcome.target_checkpoint_report.checkpoint_prefix),
                CheckpointDigestVerification::Verified
            );
            let decoded = decode_process_migration_receipt_checkpoint_entry(
                &outcome.target_checkpoint_report.checkpoint_prefix.entries()[0],
            )
            .expect("decode PMIGRCPT");
            assert_eq!(decoded.mode_code, 0);
            assert_eq!(decoded.receipt_digest, outcome.receipt.receipt_digest);
        }
        ScenarioId::MultiFamilyLeases => {
            let outcome = drive_result.expect("multi-family lease migration must succeed");
            assert!(outcome.receipt.verify_digest());
            assert_eq!(outcome.receipt.lease_regrant_proof.migrated_leases.len(), 3);

            let q = target_host
                .query_quota_lease(target_quota_id)
                .expect("target quota");
            assert_eq!(q.state(), QuotaLeaseState::Active);
            assert_eq!(q.face_value(), 150);
            assert_eq!(target_host.quota_available(), 850);

            let c = target_host
                .query_capacity_lease(target_cap_id)
                .expect("target capacity");
            assert_eq!(c.amount(), 120);
            assert_eq!(target_host.capacity_pool_remaining(), 380);

            let d = target_host
                .query_device_lease(target_dev_id)
                .expect("target device");
            assert!(!target_host.device_head_free());
            assert!(
                d.exclusivity_epoch()
                    > outcome
                        .source_lease_close_report
                        .returned_device
                        .expect("source device")
                        .exclusivity_epoch
            );
        }
        ScenarioId::SourceFencingRefusal => {
            let outcome = drive_result.expect("migration for source fencing test must succeed");
            assert!(outcome.receipt.verify_digest());
            target_host
                .record_fenced_process_heartbeat(ready.spawned_binding.process_id, gen_two)
                .expect("target gen 2 fenced heartbeat must succeed");
            assert!(target_host.cached_name(&cap_path).is_some());
        }
        ScenarioId::PinnedRefuse => {
            let err = drive_result.expect_err("PinnedNonMigratable + RefuseMigration must fail");
            assert!(
                matches!(
                    err,
                    ProcessMigrationDriveError::Protocol {
                        step: ProcessMigrationStep::Initiated,
                        source: ProcessMigrationError::PinnedSessionNonMigratable { .. },
                    }
                ),
                "unexpected error: {err:?}"
            );
            assert!(matches!(
                target_host
                    .runtime()
                    .process
                    .inspect_active_process_binding(ready.spawned_binding.process_id),
                Err(ProcessAuthorityError::ProcessNotFound(_))
            ));
            assert_eq!(target_host.quota_available(), 1000);
        }
        ScenarioId::PinnedDowngradeArtifact => {
            let outcome =
                drive_result.expect("DowngradeToRestartFromArtifact with synced image must pass");
            assert!(outcome.receipt.verify_digest());
            let expected_digest = ready.artifact_image_digest.expect("artifact digest");
            assert_eq!(
                outcome.receipt.migration_mode,
                MigrationMode::DowngradedRestartFromArtifact {
                    artifact_image_digest: expected_digest,
                    dropped_pinned_sessions: vec![[0x88; 16]],
                }
            );
            let synced_bytes = target_host
                .runtime()
                .artifacts
                .get_revision(
                    ArtifactId::from_bytes(ARTIFACT_ID_BYTES),
                    1,
                    ready.spawned_binding.created_at_ms + 1_250,
                )
                .expect("read synced artifact revision on target");
            assert_eq!(synced_bytes, ARTIFACT_IMAGE_BYTES);
            let decoded = decode_process_migration_receipt_checkpoint_entry(
                &outcome.target_checkpoint_report.checkpoint_prefix.entries()[0],
            )
            .expect("decode PMIGRCPT");
            assert_eq!(decoded.mode_code, 1);
            assert_eq!(decoded.receipt_digest, outcome.receipt.receipt_digest);
        }
        ScenarioId::PostFenceCapAmplificationQuarantine => {
            let err = drive_result.expect_err("rights amplification at Step 5 must fail closed");
            match err {
                ProcessMigrationDriveError::QuarantinedPostFence {
                    failed_step,
                    cause,
                    abort_outcome,
                    coordinator,
                    source_quarantine,
                } => {
                    assert_eq!(failed_step, ProcessMigrationStep::CapabilitiesRebound);
                    assert!(matches!(
                        *cause,
                        ProcessMigrationDriveError::Capability(
                            CapabilityAuthorityError::RightsAmplification
                        )
                    ));
                    assert!(matches!(
                        abort_outcome,
                        MigrationAbortOutcome::QuarantinedPostFence { .. }
                    ));
                    assert_eq!(
                        coordinator.current_step(),
                        ProcessMigrationStep::Quarantined
                    );
                    let sq = source_quarantine.expect("source quarantine report");
                    assert_eq!(sq.quota_state, QuotaLeaseState::Quarantined);
                    assert_eq!(sq.capacity_state, Some(CapacityLeaseState::Quarantined));
                    assert_eq!(
                        sq.device_state,
                        Some(ExclusiveDeviceLeaseState::Quarantined)
                    );
                }
                other => panic!("expected QuarantinedPostFence, got {other:?}"),
            }
            assert!(matches!(
                target_host
                    .runtime()
                    .process
                    .inspect_active_process_binding(ready.spawned_binding.process_id),
                Err(ProcessAuthorityError::ProcessNotFound(_))
            ));
            assert_eq!(target_host.quota_available(), 1000);
            assert_eq!(target_host.capacity_pool_remaining(), 500);
            assert!(target_host.device_head_free());
        }
        ScenarioId::PostFenceLeaseConservationQuarantine => {
            let err = drive_result.expect_err("quota conservation violation (>65) must fail");
            match err {
                ProcessMigrationDriveError::QuarantinedPostFence {
                    failed_step,
                    cause,
                    coordinator,
                    ..
                } => {
                    assert_eq!(failed_step, ProcessMigrationStep::LeasesRegranted);
                    assert!(matches!(
                        *cause,
                        ProcessMigrationDriveError::LeaseConservationViolated {
                            family: "QuotaLease",
                            requested: 80,
                            available_from_source: 65,
                        }
                    ));
                    assert_eq!(
                        coordinator.current_step(),
                        ProcessMigrationStep::Quarantined
                    );
                }
                other => panic!("expected QuarantinedPostFence, got {other:?}"),
            }
            assert!(matches!(
                target_host
                    .runtime()
                    .process
                    .inspect_active_process_binding(ready.spawned_binding.process_id),
                Err(ProcessAuthorityError::ProcessNotFound(_))
            ));
            assert_eq!(target_host.quota_available(), 1000);
        }
    }

    ipc_source.finish_and_verify_source();
    drop(target_host);
}

// ============================================================================
// Outer Dual-Process Orchestrator + Independent Federation Directory Verifier
// ============================================================================

fn run_dual_cell_scenario(scenario: ScenarioId) {
    let temp = TempDir::new(scenario.as_str());
    let federation_root = temp.root().join("federation");
    let source_root = temp.root().join("source-cell");
    let target_root = temp.root().join("target-cell");
    let ipc_dir = temp.root().join("ipc");
    std::fs::create_dir_all(&federation_root).expect("mkdir federation");
    std::fs::create_dir_all(&source_root).expect("mkdir source");
    std::fs::create_dir_all(&target_root).expect("mkdir target");
    std::fs::create_dir_all(&ipc_dir).expect("mkdir ipc");

    let exe = std::env::current_exe().expect("current test binary");

    let source_proc = Command::new(&exe)
        .args([
            "--exact",
            "child_source_cell_host_worker",
            "--ignored",
            "--nocapture",
        ])
        .env(ENV_SCENARIO, scenario.as_str())
        .env(ENV_FEDERATION_ROOT, &federation_root)
        .env(ENV_CELL_ROOT, &source_root)
        .env(ENV_IPC_DIR, &ipc_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn source CellHost child process");

    let target_proc = Command::new(&exe)
        .args([
            "--exact",
            "child_target_cell_host_driver",
            "--ignored",
            "--nocapture",
        ])
        .env(ENV_SCENARIO, scenario.as_str())
        .env(ENV_FEDERATION_ROOT, &federation_root)
        .env(ENV_CELL_ROOT, &target_root)
        .env(ENV_IPC_DIR, &ipc_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn target CellHost child process");

    let target_out = target_proc.wait_with_output().expect("wait target child");
    let source_out = source_proc.wait_with_output().expect("wait source child");

    assert!(
        source_out.status.success() && target_out.status.success(),
        "dual-Cell scenario {:?} failed!\n--- SOURCE ({}) ---\nstdout:\n{}\nstderr:\n{}\n--- TARGET ({}) ---\nstdout:\n{}\nstderr:\n{}",
        scenario,
        source_out.status,
        String::from_utf8_lossy(&source_out.stdout),
        String::from_utf8_lossy(&source_out.stderr),
        target_out.status,
        String::from_utf8_lossy(&target_out.stdout),
        String::from_utf8_lossy(&target_out.stderr),
    );

    verify_federation_directory_from_outer_process(scenario, &federation_root, &ipc_dir);
}

fn verify_federation_directory_from_outer_process(
    scenario: ScenarioId,
    federation_root: &Path,
    ipc_dir: &Path,
) {
    let directory = CellDirectory::open(federation_root).expect("open federation directory");
    let snapshot = directory.snapshot().expect("directory snapshot");
    assert_eq!(snapshot.corrupt(), 0);
    let entries = snapshot.into_entries();
    assert_eq!(entries.len(), 2, "both Source and Target Cells registered");
    assert_ne!(
        entries[0].os_process_id(),
        entries[1].os_process_id(),
        "federated registrations must come from two distinct OS PIDs"
    );
    assert_ne!(entries[0].os_process_id(), std::process::id());
    assert_ne!(entries[1].os_process_id(), std::process::id());

    let ready = SourceReadyPayload::decode(&wait_for_file(&ipc_dir.join("source_ready.bin")));
    let intent_log = directory.migration_intents().expect("migration intents");
    assert_eq!(intent_log.corrupt(), 0);
    let intents = intent_log.into_records();

    let target_cell_id =
        CellIdentity::from_domain(SchedulerDomainId::from_bytes(TARGET_DOMAIN_BYTES));
    let ckpt_log = directory
        .checkpoints(target_cell_id)
        .expect("target checkpoints");
    assert_eq!(ckpt_log.corrupt(), 0);

    match scenario {
        ScenarioId::PinnedRefuse => {
            assert!(
                intents.is_empty(),
                "Step 1 refusal must not append a MigrationIntent"
            );
        }
        ScenarioId::PostFenceCapAmplificationQuarantine
        | ScenarioId::PostFenceLeaseConservationQuarantine => {
            assert_eq!(intents.len(), 1);
            assert!(
                intents[0]
                    .intent()
                    .reason()
                    .starts_with(PROCESS_MIGRATION_INTENT_TAG)
            );
        }
        ScenarioId::TransparentHappyPath
        | ScenarioId::MultiFamilyLeases
        | ScenarioId::SourceFencingRefusal
        | ScenarioId::PinnedDowngradeArtifact => {
            assert_eq!(intents.len(), 1);
            let expected_reason = process_migration_intent_reason(
                MIGRATION_ID_BYTES,
                ready.spawned_binding.process_id,
                Generation::INITIAL,
                Generation::new(NonZeroU64::new(2).expect("gen 2")),
            );
            assert_eq!(intents[0].intent().reason(), expected_reason);
        }
    }
}

#[test]
fn transparent_process_migration_completes_all_8_steps_and_seals_checkpoint_across_dual_cell_hosts()
{
    run_dual_cell_scenario(ScenarioId::TransparentHappyPath);
}

#[test]
fn multi_family_lease_migration_conserves_quota_capacity_and_exclusive_device_without_double_allocation()
 {
    run_dual_cell_scenario(ScenarioId::MultiFamilyLeases);
}

#[test]
fn source_fencing_refuses_stale_heartbeat_dispatch_supervisor_and_name_cache_on_source_cell_host() {
    run_dual_cell_scenario(ScenarioId::SourceFencingRefusal);
}

#[test]
fn dist_migrate_002_refuse_migration_rejects_pinned_session_and_leaves_source_cell_host_untouched()
{
    run_dual_cell_scenario(ScenarioId::PinnedRefuse);
}

#[test]
fn dist_migrate_002_downgrade_to_restart_from_artifact_syncs_and_verifies_real_artifact_store_digest()
 {
    run_dual_cell_scenario(ScenarioId::PinnedDowngradeArtifact);
}

#[test]
fn post_fence_capability_amplification_failure_quarantines_migration_and_unsettled_source_leases() {
    run_dual_cell_scenario(ScenarioId::PostFenceCapAmplificationQuarantine);
}

#[test]
fn post_fence_lease_conservation_violation_quarantines_migration_without_target_restore() {
    run_dual_cell_scenario(ScenarioId::PostFenceLeaseConservationQuarantine);
}
