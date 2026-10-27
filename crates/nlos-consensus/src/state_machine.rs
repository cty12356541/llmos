//! Strict `[CONS-SCOPE-001]` control-plane command guard and deterministic
//! replicated state machine (`ADR-0026` §2.1, §3.2, `[DUR-SNAP-001]`).
//!
//! # Normative Scope Boundary (`[CONS-SCOPE-001]`)
//!
//! Only the **four** low-frequency control-plane metadata families may enter
//! `nlos-consensus`:
//! 1. [`ControlPlaneCommand::CellDirectoryMutation`] — cluster membership view,
//!    ephemeral `SocketAddr` routing hint versioned by
//!    `(node_boot_generation, epoch, heartbeat_seq)`, and Principal/Package
//!    trust-policy epoch.
//! 2. [`ControlPlaneCommand::RootLeasePoolMutation`] — root `QuotaLease` /
//!    `CapacityLease` / `ExclusiveDeviceLease` pool allocation, epoch
//!    advancement, and global fencing registration.
//! 3. [`ControlPlaneCommand::TaskAuthorityShardMutation`] —
//!    `TaskAuthorityAssignment` and `TaskAuthorityTakeoverReceipt` shard
//!    ownership + `authority_term` CAS binding, stamping authentic
//!    majority-committed `consensus_commit_index: NonZeroU64`.
//! 4. [`ControlPlaneCommand::ControlPlaneAuditAnchor`] — `MigrationIntent` and
//!    `ReconciliationCheckpoint` control-plane digest anchors.
//!
//! High-frequency data-plane `Task` / `Fiber` / `Topic` / `SemanticEvent` /
//! `Artifact` payloads are rejected at both compile time (via the sealed
//! [`ControlPlaneScopePermitted`] marker trait) and wire-decoding time (via
//! [`RejectDataPlanePayload`]).

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::num::NonZeroU64;

use nlos_cell::{CellEpoch, CellFencingToken, CellIdentity};
use nlos_types::{
    CapacityLeaseId, ControlDomainId, DeviceId, ExclusiveDeviceLeaseId, Generation, IdempotencyKey,
    PrincipalId, QuotaLeaseId, ReceiptId, SchedulerDomainId, TaskAuthorityAssignmentId, TaskId,
};
use sha2::{Digest, Sha256};

const WIRE_MAGIC: &[u8; 8] = b"NLOSCP01";
const SNAPSHOT_MAGIC: &[u8; 8] = b"NLOSSN01";
const PROPOSAL_DIGEST_DOMAIN: &[u8] = b"llmos/consensus/proposal/v1";
const STATE_ROOT_DOMAIN: &[u8] = b"llmos/consensus/state-root/v1";
const SNAPSHOT_CHECKSUM_DOMAIN: &[u8] = b"llmos/consensus/state-snapshot/v1";

mod sealed {
    pub trait SealedControlPlanePayload {}
}

/// Sealed compile-time marker implemented **exclusively** for the four
/// normative low-frequency control-plane command families (`[CONS-SCOPE-001]`).
pub trait ControlPlaneScopePermitted: sealed::SealedControlPlanePayload {}

/// Numeric wire and `SQLite` `CHECK`-constraint code for the 4 permitted
/// control-plane command families (`1..=4`).
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
#[repr(u8)]
pub enum CommandFamilyCode {
    /// `CellDirectory` membership, routing hint, and trust epoch (`1`).
    CellDirectory = 1,
    /// Root `Quota / Capacity / ExclusiveDevice` lease pool & fencing (`2`).
    RootLeasePool = 2,
    /// `TaskAuthorityAssignment` & `TaskAuthorityTakeoverReceipt` shard CAS (`3`).
    TaskAuthorityShard = 3,
    /// `MigrationIntent` & `ReconciliationCheckpoint` digest anchors (`4`).
    ControlPlaneAudit = 4,
}

impl CommandFamilyCode {
    /// Converts this family code into its `SQLite` `INTEGER` representation.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// Parses a raw wire / `SQLite` byte tag, rejecting data-plane or unknown
    /// tags fail-closed (`[CONS-SCOPE-001]`).
    ///
    /// # Errors
    ///
    /// Returns [`RejectDataPlanePayload`] for any tag outside `1..=4`.
    pub const fn from_wire_tag(tag: u8) -> Result<Self, RejectDataPlanePayload> {
        match tag {
            1 => Ok(Self::CellDirectory),
            2 => Ok(Self::RootLeasePool),
            3 => Ok(Self::TaskAuthorityShard),
            4 => Ok(Self::ControlPlaneAudit),
            0x81 => Err(RejectDataPlanePayload {
                kind: ForbiddenDataPlaneKind::TaskExecutionMutation,
                wire_tag: tag,
                reason: "[CONS-SCOPE-001] high-frequency Task/CommitPermit data-plane mutation is forbidden in control-plane consensus",
            }),
            0x82 => Err(RejectDataPlanePayload {
                kind: ForbiddenDataPlaneKind::FiberScheduleEvent,
                wire_tag: tag,
                reason: "[CONS-SCOPE-001] Fiber/Worker scheduling hot-path payload is forbidden in control-plane consensus",
            }),
            0x83 => Err(RejectDataPlanePayload {
                kind: ForbiddenDataPlaneKind::TopicMessagePublish,
                wire_tag: tag,
                reason: "[CONS-SCOPE-001] Topic/Channel message ordering is forbidden in control-plane consensus",
            }),
            0x84 => Err(RejectDataPlanePayload {
                kind: ForbiddenDataPlaneKind::SemanticEventAppend,
                wire_tag: tag,
                reason: "[CONS-SCOPE-001] SemanticEvent data-plane append is forbidden in control-plane consensus",
            }),
            0x85 => Err(RejectDataPlanePayload {
                kind: ForbiddenDataPlaneKind::ArtifactBulkPayload,
                wire_tag: tag,
                reason: "[CONS-SCOPE-001] Artifact content/revision data-plane payload is forbidden in control-plane consensus",
            }),
            other => Err(RejectDataPlanePayload {
                kind: ForbiddenDataPlaneKind::UnknownNonControlFamily(other),
                wire_tag: other,
                reason: "[CONS-SCOPE-001] non-control-plane wire tag is forbidden in nlos-consensus",
            }),
        }
    }
}

/// High-frequency data-plane payload classes explicitly barred from entering
/// `nlos-consensus` (`[CONS-SCOPE-001]`, `ADR-0026` §2.1 & §5 #5).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ForbiddenDataPlaneKind {
    /// Local Cell `TaskControlRecord` / `TaskHead` / `CommitPermit` mutation (`0x81`).
    TaskExecutionMutation,
    /// `ExecutionFiber` / worker runnable-queue scheduling transition (`0x82`).
    FiberScheduleEvent,
    /// `Topic` / `Channel` message publication (`0x83`).
    TopicMessagePublish,
    /// `SemanticEvent` append / CRDT projection (`0x84`).
    SemanticEventAppend,
    /// `Artifact` revision or chunk payload (`0x85`).
    ArtifactBulkPayload,
    /// Any other unrecognized non-control-plane family tag.
    UnknownNonControlFamily(u8),
}

impl ForbiddenDataPlaneKind {
    /// Reserved wire tag used to detect and reject forbidden data-plane probes.
    #[must_use]
    pub const fn wire_tag(self) -> u8 {
        match self {
            Self::TaskExecutionMutation => 0x81,
            Self::FiberScheduleEvent => 0x82,
            Self::TopicMessagePublish => 0x83,
            Self::SemanticEventAppend => 0x84,
            Self::ArtifactBulkPayload => 0x85,
            Self::UnknownNonControlFamily(tag) => tag,
        }
    }
}

/// Explicit `[CONS-SCOPE-001]` refusal emitted whenever a caller or wire frame
/// attempts to inject data-plane traffic into `nlos-consensus`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RejectDataPlanePayload {
    /// Classified forbidden data-plane payload category.
    pub kind: ForbiddenDataPlaneKind,
    /// Offending raw family tag observed on the wire.
    pub wire_tag: u8,
    /// Static normative explanation citing `[CONS-SCOPE-001]`.
    pub reason: &'static str,
}

impl fmt::Display for RejectDataPlanePayload {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} (kind={:?}, wire_tag=0x{:02x})",
            self.reason, self.kind, self.wire_tag
        )
    }
}

impl Error for RejectDataPlanePayload {}

/// Wire-decoding and scope-admission errors for [`ControlPlaneProposal`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScopeViolationError {
    /// Data-plane or non-control-plane payload rejected under `[CONS-SCOPE-001]`.
    RejectDataPlanePayload(RejectDataPlanePayload),
    /// Frame header did not start with `NLOSCP01`.
    MalformedWireHeader,
    /// Frame ended prematurely while decoding fields.
    TruncatedPayload,
    /// Unexpected trailing bytes remained after decoding canonical command fields.
    TrailingBytes,
    /// Zero or out-of-range nominal field (e.g. `NonZeroU64` / variant code).
    InvalidNominalField(&'static str),
    /// Trailing `SHA-256` integrity tag on wire frame did not match payload.
    PayloadChecksumMismatch,
}

impl fmt::Display for ScopeViolationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RejectDataPlanePayload(reject) => write!(formatter, "{reject}"),
            Self::MalformedWireHeader => {
                formatter.write_str("invalid ControlPlaneProposal magic header")
            }
            Self::TruncatedPayload => formatter.write_str("truncated ControlPlaneProposal payload"),
            Self::TrailingBytes => {
                formatter.write_str("unexpected trailing bytes in ControlPlaneProposal")
            }
            Self::InvalidNominalField(field) => {
                write!(
                    formatter,
                    "invalid nominal field in ControlPlaneProposal: {field}"
                )
            }
            Self::PayloadChecksumMismatch => {
                formatter.write_str("ControlPlaneProposal wire SHA-256 checksum mismatch")
            }
        }
    }
}

impl Error for ScopeViolationError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::RejectDataPlanePayload(reject) => Some(reject),
            _ => None,
        }
    }
}

impl From<RejectDataPlanePayload> for ScopeViolationError {
    fn from(value: RejectDataPlanePayload) -> Self {
        Self::RejectDataPlanePayload(value)
    }
}

/// Family 1 (`ADR-0026` §2.1 #1): `CellDirectory` membership, ephemeral routing
/// hints, and Principal/Package trust epoch mutations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CellDirectoryCommand {
    /// Registers or updates a Cell's membership entry and ephemeral routing
    /// hint guarded by `(node_boot_generation, epoch, heartbeat_seq)`.
    UpsertCellEndpoint {
        /// Nominal 16-byte `CellIdentity` (`[DIST-NAME-001]`).
        cell: CellIdentity,
        /// Failure domain owning `cell` (`[TASK-SCALE-002]`).
        control_domain_id: ControlDomainId,
        /// Authenticated node `PrincipalId`.
        principal_id: PrincipalId,
        /// Monotonic OS boot generation.
        node_boot_generation: Generation,
        /// Monotonic `CellEpoch`.
        epoch: CellEpoch,
        /// Monotonic heartbeat/routing sequence within `(node_boot_generation, epoch)`.
        heartbeat_seq: u64,
        /// Optional ephemeral `IP:port` routing hint (`[DIST-NAME-001]`).
        endpoint_hint: Option<SocketAddr>,
    },
    /// Advances the cluster-wide Principal/Package trust-policy epoch.
    AdvanceTrustPolicyEpoch {
        /// Strictly increasing trust epoch (`> current`).
        new_trust_epoch: u64,
        /// Domain-separated digest of the frozen trust-policy bundle.
        policy_digest: [u8; 32],
    },
    /// Marks a Cell fenced/decommissioned at `fenced_epoch`.
    DecommissionCell {
        /// Target `CellIdentity`.
        cell: CellIdentity,
        /// Fencing epoch at which `cell` was decommissioned.
        fenced_epoch: CellEpoch,
    },
}

impl sealed::SealedControlPlanePayload for CellDirectoryCommand {}
impl ControlPlaneScopePermitted for CellDirectoryCommand {}

/// Replicated disposition of a root `QuotaLease` (`06-架构设计总纲-v0.5.md` §12).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ReplicatedQuotaLeaseDisposition {
    /// Active / issued to holder Cell.
    Active = 1,
    /// Cleanly closed with verified holder `LeaseCloseReceipt` (`[LEASE-CLOSE-001]`).
    Closed = 2,
    /// Fenced and quarantined on holder loss (`[LEASE-LOSS-001]`; never re-issued
    /// without barrier/reconciliation proof).
    Quarantined = 3,
}

impl ReplicatedQuotaLeaseDisposition {
    const fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::Active),
            2 => Some(Self::Closed),
            3 => Some(Self::Quarantined),
            _ => None,
        }
    }
}

/// Replicated state of a root `DeviceLeaseHead` (`[LEASE-DEVICE-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ReplicatedDeviceHeadState {
    /// Free for CAS acquisition with verified reset/zeroization proof.
    Free = 1,
    /// Exclusively held by `holder_node` in `exclusivity_epoch`.
    Active = 2,
    /// Quarantined pending fence + reset + zeroization attestation.
    Quarantined = 3,
}

impl ReplicatedDeviceHeadState {
    const fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::Free),
            2 => Some(Self::Active),
            3 => Some(Self::Quarantined),
            _ => None,
        }
    }
}

/// Family 2 (`ADR-0026` §2.1 #2): Root `Quota / Capacity / ExclusiveDevice`
/// lease pool allocation, epoch bump, and global fencing registration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RootLeasePoolCommand {
    /// Initializes a root quota budget pool with `initial_available`.
    RegisterQuotaPool {
        /// Nominal 16-byte budget root identifier.
        budget_root: [u8; 16],
        /// Total available units seeded into the root pool.
        initial_available: u64,
    },
    /// Deducts `face_value` from `budget_root` (`[LEASE-GRANT-001]`) and records
    /// a `QuotaLease` with monotonic `fencing_token` in `fence_scope`.
    AllocateQuotaLease {
        /// Lease identifier.
        lease_id: QuotaLeaseId,
        /// Source budget root pool.
        budget_root: [u8; 16],
        /// Holder `CellIdentity`.
        holder_cell: CellIdentity,
        /// Holder boot generation.
        node_boot_generation: Generation,
        /// Prepaid face value deducted upfront.
        face_value: u64,
        /// Lease epoch.
        epoch: CellEpoch,
        /// Monotonic fencing token within `fence_scope` (`[LEASE-FENCE-001]`).
        fencing_token: CellFencingToken,
        /// 16-byte fencing scope identifier.
        fence_scope: [u8; 16],
    },
    /// Settles or quarantines an existing `QuotaLease` (`[LEASE-CLOSE-001]`,
    /// `[LEASE-LOSS-001]`).
    SettleOrQuarantineQuotaLease {
        /// Target `QuotaLeaseId`.
        lease_id: QuotaLeaseId,
        /// Target disposition (`Closed` or `Quarantined`).
        disposition: ReplicatedQuotaLeaseDisposition,
        /// Verified unused balance returned to `budget_root` (only when `Closed`).
        returned_amount: u64,
        /// Unreconciled balance frozen in `QUARANTINED` (`[LEASE-LOSS-001]`).
        quarantined_amount: u64,
        /// Advanced fence epoch.
        new_epoch: CellEpoch,
        /// Strictly advanced fencing token in the lease's `fence_scope`.
        new_fencing_token: CellFencingToken,
    },
    /// Allocates committed capacity from a root capacity pool (`[LEASE-CAPACITY-001]`).
    AllocateCapacityLease {
        /// Capacity lease identifier.
        capacity_lease_id: CapacityLeaseId,
        /// 16-byte capacity pool identifier.
        pool_id: [u8; 16],
        /// Total capacity ceiling of `pool_id` if initializing for the first time.
        pool_total_capacity: u64,
        /// Target Cell receiving capacity.
        target_cell: CellIdentity,
        /// Target Cell boot generation.
        target_boot_generation: Generation,
        /// Reserved capacity units.
        amount: u64,
        /// Monotonic capacity epoch.
        capacity_epoch: NonZeroU64,
        /// Monotonic fencing token.
        fencing_token: CellFencingToken,
    },
    /// Linearizes `DeviceLeaseHead` transitions (`[LEASE-DEVICE-001]`).
    TransitionExclusiveDeviceHead {
        /// Target physical/logical device identifier.
        device_id: DeviceId,
        /// Associated `ExclusiveDeviceLeaseId`.
        device_lease_id: ExclusiveDeviceLeaseId,
        /// Monotonic exclusivity epoch.
        exclusivity_epoch: NonZeroU64,
        /// Holder Cell when `new_state == Active`.
        holder_cell: Option<CellIdentity>,
        /// Monotonic fencing token.
        fencing_token: CellFencingToken,
        /// Next `DeviceLeaseHead` state (`Free`, `Active`, or `Quarantined`).
        new_state: ReplicatedDeviceHeadState,
        /// Whether a verified `DeviceResetReceipt` + zeroization proof accompanied
        /// a transition back to `Free`.
        has_reset_zeroization_proof: bool,
    },
}

impl sealed::SealedControlPlanePayload for RootLeasePoolCommand {}
impl ControlPlaneScopePermitted for RootLeasePoolCommand {}

/// Family 3 (`ADR-0026` §2.1 #3): `TaskAuthorityAssignment` and
/// `TaskAuthorityTakeoverReceipt` shard ownership + `authority_term` CAS,
/// producing authentic majority-committed `consensus_commit_index: NonZeroU64`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskAuthorityShardCommand {
    /// Binds initial shard ownership for `(task_id, task_generation)` at
    /// `authority_term`.
    AssignInitialAuthority {
        /// Deterministic `TaskAuthorityAssignmentId`.
        assignment_id: TaskAuthorityAssignmentId,
        /// Subject `TaskId`.
        task_id: TaskId,
        /// Subject `Task` generation.
        task_generation: Generation,
        /// 16-byte authority shard identifier.
        authority_shard: [u8; 16],
        /// Authoritative home `CellIdentity`.
        home_cell: CellIdentity,
        /// Initial non-zero `authority_term`.
        authority_term: NonZeroU64,
        /// Control epoch paired with `authority_term`.
        control_epoch: u64,
        /// Initial participant-registry generation.
        participant_registry_generation: u64,
        /// Initial participant-registry Merkle/digest root.
        participant_registry_root: [u8; 32],
    },
    /// Atomically fences `old_assignment_id` at `frozen_old_authority_term` and
    /// activates `new_assignment_id` at strictly higher `new_authority_term`
    /// (`[DIST-TASK-002]`), stamping the Raft majority `consensus_commit_index`.
    RecordCompletedTakeover {
        /// `TaskAuthorityTakeoverReceipt` identifier.
        takeover_receipt_id: ReceiptId,
        /// Subject `TaskId`.
        task_id: TaskId,
        /// Subject `Task` generation.
        task_generation: Generation,
        /// Fenced predecessor assignment id.
        old_assignment_id: TaskAuthorityAssignmentId,
        /// Activated successor assignment id.
        new_assignment_id: TaskAuthorityAssignmentId,
        /// Expected predecessor `authority_term` (CAS guard).
        frozen_old_authority_term: NonZeroU64,
        /// Strictly greater successor `authority_term`.
        new_authority_term: NonZeroU64,
        /// Successor home `CellIdentity`.
        new_home_cell: CellIdentity,
        /// Successor control epoch.
        new_control_epoch: u64,
        /// Successor participant-registry generation.
        new_participant_registry_generation: u64,
        /// Successor participant-registry root.
        new_participant_registry_root: [u8; 32],
        /// Exact fenced participant set root covered by barrier receipts.
        exact_fence_set_root: [u8; 32],
        /// Optional quarantined effect root carried across takeover.
        outstanding_effect_quarantine_root: Option<[u8; 32]>,
    },
}

impl sealed::SealedControlPlanePayload for TaskAuthorityShardCommand {}
impl ControlPlaneScopePermitted for TaskAuthorityShardCommand {}

/// Family 4 (`ADR-0026` §2.1 #4): `MigrationIntent` and
/// `ReconciliationCheckpoint` control-plane audit digest anchors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlPlaneAuditCommand {
    /// Anchors a cross-Cell `MigrationIntent` digest in the control-plane log.
    AnchorMigrationIntent {
        /// 16-byte migration intent identifier.
        intent_id: [u8; 16],
        /// Source `CellIdentity`.
        source_cell: CellIdentity,
        /// Target `CellIdentity`.
        target_cell: CellIdentity,
        /// Authoritative `CellEpoch` of the migration intent.
        epoch: CellEpoch,
        /// Canonical SHA-256 digest of the `MigrationIntentRecord`.
        intent_digest: [u8; 32],
    },
    /// Anchors a per-Cell monotonic `ReconciliationCheckpoint` digest
    /// (`ADR-0026` §2.3 #4).
    AnchorReconciliationCheckpoint {
        /// Emitting `CellIdentity`.
        cell: CellIdentity,
        /// Cell OS boot generation.
        node_boot_generation: Generation,
        /// Cell control-plane epoch.
        epoch: CellEpoch,
        /// Monotonic per-Cell checkpoint sequence (`>= 1`).
        checkpoint_seq: NonZeroU64,
        /// Domain-separated `CheckpointDigest` (`SHA-256`).
        checkpoint_digest: [u8; 32],
    },
}

impl sealed::SealedControlPlanePayload for ControlPlaneAuditCommand {}
impl ControlPlaneScopePermitted for ControlPlaneAuditCommand {}

/// Closed normative sum of all commands permitted inside `nlos-consensus`
/// (`[CONS-SCOPE-001]`, `ADR-0026` §2.1).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlPlaneCommand {
    /// Family 1: `CellDirectory` membership, routing hint, and trust epoch.
    CellDirectoryMutation(CellDirectoryCommand),
    /// Family 2: Root `Quota / Capacity / ExclusiveDevice` lease pool & fencing.
    RootLeasePoolMutation(RootLeasePoolCommand),
    /// Family 3: `TaskAuthorityAssignment` & `TaskAuthorityTakeoverReceipt` CAS.
    TaskAuthorityShardMutation(TaskAuthorityShardCommand),
    /// Family 4: `MigrationIntent` & `ReconciliationCheckpoint` digest anchors.
    ControlPlaneAuditAnchor(ControlPlaneAuditCommand),
}

impl sealed::SealedControlPlanePayload for ControlPlaneCommand {}
impl ControlPlaneScopePermitted for ControlPlaneCommand {}

impl ControlPlaneCommand {
    /// Returns the `1..=4` [`CommandFamilyCode`] for this command.
    #[must_use]
    pub const fn family_code(&self) -> CommandFamilyCode {
        match self {
            Self::CellDirectoryMutation(_) => CommandFamilyCode::CellDirectory,
            Self::RootLeasePoolMutation(_) => CommandFamilyCode::RootLeasePool,
            Self::TaskAuthorityShardMutation(_) => CommandFamilyCode::TaskAuthorityShard,
            Self::ControlPlaneAuditAnchor(_) => CommandFamilyCode::ControlPlaneAudit,
        }
    }
}

/// Idempotent client proposal wrapping a [`ControlPlaneCommand`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ControlPlaneProposal {
    /// Caller-supplied 16-byte idempotency key (`[DUR-SNAP-001]`).
    pub idempotency_key: IdempotencyKey,
    /// Strictly scoped control-plane metadata mutation (`[CONS-SCOPE-001]`).
    pub command: ControlPlaneCommand,
}

impl ControlPlaneProposal {
    /// Constructs a scoped [`ControlPlaneProposal`].
    #[must_use]
    pub const fn new(idempotency_key: IdempotencyKey, command: ControlPlaneCommand) -> Self {
        Self {
            idempotency_key,
            command,
        }
    }

    /// Computes the canonical domain-separated `SHA-256` digest of this proposal.
    #[must_use]
    pub fn proposal_digest(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(PROPOSAL_DIGEST_DOMAIN);
        hasher.update([self.command.family_code().as_u8()]);
        hasher.update(self.idempotency_key.as_bytes());
        let body = self.encode_command_body();
        hasher.update((body.len() as u64).to_be_bytes());
        hasher.update(&body);
        hasher.finalize().into()
    }

    /// Encodes only the inner command variant body (paired with
    /// `(family_code, idempotency_key)` in `SQLite` `consensus_log`).
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn encode_command_body(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(128);
        match self.command {
            ControlPlaneCommand::CellDirectoryMutation(cmd) => match cmd {
                CellDirectoryCommand::UpsertCellEndpoint {
                    cell,
                    control_domain_id,
                    principal_id,
                    node_boot_generation,
                    epoch,
                    heartbeat_seq,
                    endpoint_hint,
                } => {
                    out.push(1);
                    out.extend_from_slice(cell.as_bytes());
                    out.extend_from_slice(control_domain_id.as_bytes());
                    out.extend_from_slice(principal_id.as_bytes());
                    out.extend_from_slice(&node_boot_generation.get().to_be_bytes());
                    out.extend_from_slice(&epoch.get().to_be_bytes());
                    out.extend_from_slice(&heartbeat_seq.to_be_bytes());
                    encode_socket_addr_opt(&mut out, endpoint_hint);
                }
                CellDirectoryCommand::AdvanceTrustPolicyEpoch {
                    new_trust_epoch,
                    policy_digest,
                } => {
                    out.push(2);
                    out.extend_from_slice(&new_trust_epoch.to_be_bytes());
                    out.extend_from_slice(&policy_digest);
                }
                CellDirectoryCommand::DecommissionCell { cell, fenced_epoch } => {
                    out.push(3);
                    out.extend_from_slice(cell.as_bytes());
                    out.extend_from_slice(&fenced_epoch.get().to_be_bytes());
                }
            },
            ControlPlaneCommand::RootLeasePoolMutation(cmd) => match cmd {
                RootLeasePoolCommand::RegisterQuotaPool {
                    budget_root,
                    initial_available,
                } => {
                    out.push(1);
                    out.extend_from_slice(&budget_root);
                    out.extend_from_slice(&initial_available.to_be_bytes());
                }
                RootLeasePoolCommand::AllocateQuotaLease {
                    lease_id,
                    budget_root,
                    holder_cell,
                    node_boot_generation,
                    face_value,
                    epoch,
                    fencing_token,
                    fence_scope,
                } => {
                    out.push(2);
                    out.extend_from_slice(lease_id.as_bytes());
                    out.extend_from_slice(&budget_root);
                    out.extend_from_slice(holder_cell.as_bytes());
                    out.extend_from_slice(&node_boot_generation.get().to_be_bytes());
                    out.extend_from_slice(&face_value.to_be_bytes());
                    out.extend_from_slice(&epoch.get().to_be_bytes());
                    out.extend_from_slice(&fencing_token.get().to_be_bytes());
                    out.extend_from_slice(&fence_scope);
                }
                RootLeasePoolCommand::SettleOrQuarantineQuotaLease {
                    lease_id,
                    disposition,
                    returned_amount,
                    quarantined_amount,
                    new_epoch,
                    new_fencing_token,
                } => {
                    out.push(3);
                    out.extend_from_slice(lease_id.as_bytes());
                    out.push(disposition as u8);
                    out.extend_from_slice(&returned_amount.to_be_bytes());
                    out.extend_from_slice(&quarantined_amount.to_be_bytes());
                    out.extend_from_slice(&new_epoch.get().to_be_bytes());
                    out.extend_from_slice(&new_fencing_token.get().to_be_bytes());
                }
                RootLeasePoolCommand::AllocateCapacityLease {
                    capacity_lease_id,
                    pool_id,
                    pool_total_capacity,
                    target_cell,
                    target_boot_generation,
                    amount,
                    capacity_epoch,
                    fencing_token,
                } => {
                    out.push(4);
                    out.extend_from_slice(capacity_lease_id.as_bytes());
                    out.extend_from_slice(&pool_id);
                    out.extend_from_slice(&pool_total_capacity.to_be_bytes());
                    out.extend_from_slice(target_cell.as_bytes());
                    out.extend_from_slice(&target_boot_generation.get().to_be_bytes());
                    out.extend_from_slice(&amount.to_be_bytes());
                    out.extend_from_slice(&capacity_epoch.get().to_be_bytes());
                    out.extend_from_slice(&fencing_token.get().to_be_bytes());
                }
                RootLeasePoolCommand::TransitionExclusiveDeviceHead {
                    device_id,
                    device_lease_id,
                    exclusivity_epoch,
                    holder_cell,
                    fencing_token,
                    new_state,
                    has_reset_zeroization_proof,
                } => {
                    out.push(5);
                    out.extend_from_slice(device_id.as_bytes());
                    out.extend_from_slice(device_lease_id.as_bytes());
                    out.extend_from_slice(&exclusivity_epoch.get().to_be_bytes());
                    match holder_cell {
                        Some(cell) => {
                            out.push(1);
                            out.extend_from_slice(cell.as_bytes());
                        }
                        None => out.push(0),
                    }
                    out.extend_from_slice(&fencing_token.get().to_be_bytes());
                    out.push(new_state as u8);
                    out.push(u8::from(has_reset_zeroization_proof));
                }
            },
            ControlPlaneCommand::TaskAuthorityShardMutation(cmd) => match cmd {
                TaskAuthorityShardCommand::AssignInitialAuthority {
                    assignment_id,
                    task_id,
                    task_generation,
                    authority_shard,
                    home_cell,
                    authority_term,
                    control_epoch,
                    participant_registry_generation,
                    participant_registry_root,
                } => {
                    out.push(1);
                    out.extend_from_slice(assignment_id.as_bytes());
                    out.extend_from_slice(task_id.as_bytes());
                    out.extend_from_slice(&task_generation.get().to_be_bytes());
                    out.extend_from_slice(&authority_shard);
                    out.extend_from_slice(home_cell.as_bytes());
                    out.extend_from_slice(&authority_term.get().to_be_bytes());
                    out.extend_from_slice(&control_epoch.to_be_bytes());
                    out.extend_from_slice(&participant_registry_generation.to_be_bytes());
                    out.extend_from_slice(&participant_registry_root);
                }
                TaskAuthorityShardCommand::RecordCompletedTakeover {
                    takeover_receipt_id,
                    task_id,
                    task_generation,
                    old_assignment_id,
                    new_assignment_id,
                    frozen_old_authority_term,
                    new_authority_term,
                    new_home_cell,
                    new_control_epoch,
                    new_participant_registry_generation,
                    new_participant_registry_root,
                    exact_fence_set_root,
                    outstanding_effect_quarantine_root,
                } => {
                    out.push(2);
                    out.extend_from_slice(takeover_receipt_id.as_bytes());
                    out.extend_from_slice(task_id.as_bytes());
                    out.extend_from_slice(&task_generation.get().to_be_bytes());
                    out.extend_from_slice(old_assignment_id.as_bytes());
                    out.extend_from_slice(new_assignment_id.as_bytes());
                    out.extend_from_slice(&frozen_old_authority_term.get().to_be_bytes());
                    out.extend_from_slice(&new_authority_term.get().to_be_bytes());
                    out.extend_from_slice(new_home_cell.as_bytes());
                    out.extend_from_slice(&new_control_epoch.to_be_bytes());
                    out.extend_from_slice(&new_participant_registry_generation.to_be_bytes());
                    out.extend_from_slice(&new_participant_registry_root);
                    out.extend_from_slice(&exact_fence_set_root);
                    match outstanding_effect_quarantine_root {
                        Some(root) => {
                            out.push(1);
                            out.extend_from_slice(&root);
                        }
                        None => out.push(0),
                    }
                }
            },
            ControlPlaneCommand::ControlPlaneAuditAnchor(cmd) => match cmd {
                ControlPlaneAuditCommand::AnchorMigrationIntent {
                    intent_id,
                    source_cell,
                    target_cell,
                    epoch,
                    intent_digest,
                } => {
                    out.push(1);
                    out.extend_from_slice(&intent_id);
                    out.extend_from_slice(source_cell.as_bytes());
                    out.extend_from_slice(target_cell.as_bytes());
                    out.extend_from_slice(&epoch.get().to_be_bytes());
                    out.extend_from_slice(&intent_digest);
                }
                ControlPlaneAuditCommand::AnchorReconciliationCheckpoint {
                    cell,
                    node_boot_generation,
                    epoch,
                    checkpoint_seq,
                    checkpoint_digest,
                } => {
                    out.push(2);
                    out.extend_from_slice(cell.as_bytes());
                    out.extend_from_slice(&node_boot_generation.get().to_be_bytes());
                    out.extend_from_slice(&epoch.get().to_be_bytes());
                    out.extend_from_slice(&checkpoint_seq.get().to_be_bytes());
                    out.extend_from_slice(&checkpoint_digest);
                }
            },
        }
        out
    }

    /// Decodes a [`ControlPlaneProposal`] from `(family_code, idempotency_key, body)`.
    ///
    /// # Errors
    ///
    /// Fails closed on truncated, malformed, or trailing-byte payloads.
    #[allow(clippy::too_many_lines)]
    pub fn decode_command_body(
        family_code: CommandFamilyCode,
        idempotency_key: IdempotencyKey,
        body: &[u8],
    ) -> Result<Self, ScopeViolationError> {
        let mut cursor = ByteCursor::new(body);
        let sub_tag = cursor.read_u8()?;
        let command = match family_code {
            CommandFamilyCode::CellDirectory => match sub_tag {
                1 => {
                    let cell = cursor.read_cell_id()?;
                    let control_domain_id = ControlDomainId::from_bytes(cursor.read_16()?);
                    let principal_id = PrincipalId::from_bytes(cursor.read_16()?);
                    let node_boot_generation = cursor.read_generation("node_boot_generation")?;
                    let epoch = cursor.read_cell_epoch("epoch")?;
                    let heartbeat_seq = cursor.read_u64()?;
                    let endpoint_hint = cursor.read_socket_addr_opt()?;
                    ControlPlaneCommand::CellDirectoryMutation(
                        CellDirectoryCommand::UpsertCellEndpoint {
                            cell,
                            control_domain_id,
                            principal_id,
                            node_boot_generation,
                            epoch,
                            heartbeat_seq,
                            endpoint_hint,
                        },
                    )
                }
                2 => {
                    let new_trust_epoch = cursor.read_u64()?;
                    let policy_digest = cursor.read_32()?;
                    ControlPlaneCommand::CellDirectoryMutation(
                        CellDirectoryCommand::AdvanceTrustPolicyEpoch {
                            new_trust_epoch,
                            policy_digest,
                        },
                    )
                }
                3 => {
                    let cell = cursor.read_cell_id()?;
                    let fenced_epoch = cursor.read_cell_epoch("fenced_epoch")?;
                    ControlPlaneCommand::CellDirectoryMutation(
                        CellDirectoryCommand::DecommissionCell { cell, fenced_epoch },
                    )
                }
                _ => {
                    return Err(ScopeViolationError::InvalidNominalField(
                        "CellDirectory sub-tag",
                    ));
                }
            },
            CommandFamilyCode::RootLeasePool => match sub_tag {
                1 => {
                    let budget_root = cursor.read_16()?;
                    let initial_available = cursor.read_u64()?;
                    ControlPlaneCommand::RootLeasePoolMutation(
                        RootLeasePoolCommand::RegisterQuotaPool {
                            budget_root,
                            initial_available,
                        },
                    )
                }
                2 => {
                    let lease_id = QuotaLeaseId::from_bytes(cursor.read_16()?);
                    let budget_root = cursor.read_16()?;
                    let holder_cell = cursor.read_cell_id()?;
                    let node_boot_generation = cursor.read_generation("node_boot_generation")?;
                    let face_value = cursor.read_u64()?;
                    let epoch = cursor.read_cell_epoch("epoch")?;
                    let fencing_token = cursor.read_fencing_token("fencing_token")?;
                    let fence_scope = cursor.read_16()?;
                    ControlPlaneCommand::RootLeasePoolMutation(
                        RootLeasePoolCommand::AllocateQuotaLease {
                            lease_id,
                            budget_root,
                            holder_cell,
                            node_boot_generation,
                            face_value,
                            epoch,
                            fencing_token,
                            fence_scope,
                        },
                    )
                }
                3 => {
                    let lease_id = QuotaLeaseId::from_bytes(cursor.read_16()?);
                    let disp_raw = cursor.read_u8()?;
                    let disposition = ReplicatedQuotaLeaseDisposition::from_u8(disp_raw).ok_or(
                        ScopeViolationError::InvalidNominalField("QuotaLeaseDisposition"),
                    )?;
                    let returned_amount = cursor.read_u64()?;
                    let quarantined_amount = cursor.read_u64()?;
                    let new_epoch = cursor.read_cell_epoch("new_epoch")?;
                    let new_fencing_token = cursor.read_fencing_token("new_fencing_token")?;
                    ControlPlaneCommand::RootLeasePoolMutation(
                        RootLeasePoolCommand::SettleOrQuarantineQuotaLease {
                            lease_id,
                            disposition,
                            returned_amount,
                            quarantined_amount,
                            new_epoch,
                            new_fencing_token,
                        },
                    )
                }
                4 => {
                    let capacity_lease_id = CapacityLeaseId::from_bytes(cursor.read_16()?);
                    let pool_id = cursor.read_16()?;
                    let pool_total_capacity = cursor.read_u64()?;
                    let target_cell = cursor.read_cell_id()?;
                    let target_boot_generation =
                        cursor.read_generation("target_boot_generation")?;
                    let amount = cursor.read_u64()?;
                    let capacity_epoch = cursor.read_nonzero_u64("capacity_epoch")?;
                    let fencing_token = cursor.read_fencing_token("fencing_token")?;
                    ControlPlaneCommand::RootLeasePoolMutation(
                        RootLeasePoolCommand::AllocateCapacityLease {
                            capacity_lease_id,
                            pool_id,
                            pool_total_capacity,
                            target_cell,
                            target_boot_generation,
                            amount,
                            capacity_epoch,
                            fencing_token,
                        },
                    )
                }
                5 => {
                    let device_id = DeviceId::from_bytes(cursor.read_16()?);
                    let device_lease_id = ExclusiveDeviceLeaseId::from_bytes(cursor.read_16()?);
                    let exclusivity_epoch = cursor.read_nonzero_u64("exclusivity_epoch")?;
                    let holder_cell = match cursor.read_u8()? {
                        0 => None,
                        1 => Some(cursor.read_cell_id()?),
                        _ => {
                            return Err(ScopeViolationError::InvalidNominalField(
                                "holder_cell option tag",
                            ));
                        }
                    };
                    let fencing_token = cursor.read_fencing_token("fencing_token")?;
                    let new_state = ReplicatedDeviceHeadState::from_u8(cursor.read_u8()?).ok_or(
                        ScopeViolationError::InvalidNominalField("ReplicatedDeviceHeadState"),
                    )?;
                    let has_reset_zeroization_proof = match cursor.read_u8()? {
                        0 => false,
                        1 => true,
                        _ => {
                            return Err(ScopeViolationError::InvalidNominalField(
                                "has_reset_zeroization_proof bool",
                            ));
                        }
                    };
                    ControlPlaneCommand::RootLeasePoolMutation(
                        RootLeasePoolCommand::TransitionExclusiveDeviceHead {
                            device_id,
                            device_lease_id,
                            exclusivity_epoch,
                            holder_cell,
                            fencing_token,
                            new_state,
                            has_reset_zeroization_proof,
                        },
                    )
                }
                _ => {
                    return Err(ScopeViolationError::InvalidNominalField(
                        "RootLeasePool sub-tag",
                    ));
                }
            },
            CommandFamilyCode::TaskAuthorityShard => match sub_tag {
                1 => {
                    let assignment_id = TaskAuthorityAssignmentId::from_bytes(cursor.read_16()?);
                    let task_id = TaskId::from_bytes(cursor.read_16()?);
                    let task_generation = cursor.read_generation("task_generation")?;
                    let authority_shard = cursor.read_16()?;
                    let home_cell = cursor.read_cell_id()?;
                    let authority_term = cursor.read_nonzero_u64("authority_term")?;
                    let control_epoch = cursor.read_u64()?;
                    let participant_registry_generation = cursor.read_u64()?;
                    let participant_registry_root = cursor.read_32()?;
                    ControlPlaneCommand::TaskAuthorityShardMutation(
                        TaskAuthorityShardCommand::AssignInitialAuthority {
                            assignment_id,
                            task_id,
                            task_generation,
                            authority_shard,
                            home_cell,
                            authority_term,
                            control_epoch,
                            participant_registry_generation,
                            participant_registry_root,
                        },
                    )
                }
                2 => {
                    let takeover_receipt_id = ReceiptId::from_bytes(cursor.read_16()?);
                    let task_id = TaskId::from_bytes(cursor.read_16()?);
                    let task_generation = cursor.read_generation("task_generation")?;
                    let old_assignment_id =
                        TaskAuthorityAssignmentId::from_bytes(cursor.read_16()?);
                    let new_assignment_id =
                        TaskAuthorityAssignmentId::from_bytes(cursor.read_16()?);
                    let frozen_old_authority_term =
                        cursor.read_nonzero_u64("frozen_old_authority_term")?;
                    let new_authority_term = cursor.read_nonzero_u64("new_authority_term")?;
                    let new_home_cell = cursor.read_cell_id()?;
                    let new_control_epoch = cursor.read_u64()?;
                    let new_participant_registry_generation = cursor.read_u64()?;
                    let new_participant_registry_root = cursor.read_32()?;
                    let exact_fence_set_root = cursor.read_32()?;
                    let outstanding_effect_quarantine_root = match cursor.read_u8()? {
                        0 => None,
                        1 => Some(cursor.read_32()?),
                        _ => {
                            return Err(ScopeViolationError::InvalidNominalField(
                                "outstanding_effect_quarantine_root option tag",
                            ));
                        }
                    };
                    ControlPlaneCommand::TaskAuthorityShardMutation(
                        TaskAuthorityShardCommand::RecordCompletedTakeover {
                            takeover_receipt_id,
                            task_id,
                            task_generation,
                            old_assignment_id,
                            new_assignment_id,
                            frozen_old_authority_term,
                            new_authority_term,
                            new_home_cell,
                            new_control_epoch,
                            new_participant_registry_generation,
                            new_participant_registry_root,
                            exact_fence_set_root,
                            outstanding_effect_quarantine_root,
                        },
                    )
                }
                _ => {
                    return Err(ScopeViolationError::InvalidNominalField(
                        "TaskAuthorityShard sub-tag",
                    ));
                }
            },
            CommandFamilyCode::ControlPlaneAudit => match sub_tag {
                1 => {
                    let intent_id = cursor.read_16()?;
                    let source_cell = cursor.read_cell_id()?;
                    let target_cell = cursor.read_cell_id()?;
                    let epoch = cursor.read_cell_epoch("epoch")?;
                    let intent_digest = cursor.read_32()?;
                    ControlPlaneCommand::ControlPlaneAuditAnchor(
                        ControlPlaneAuditCommand::AnchorMigrationIntent {
                            intent_id,
                            source_cell,
                            target_cell,
                            epoch,
                            intent_digest,
                        },
                    )
                }
                2 => {
                    let cell = cursor.read_cell_id()?;
                    let node_boot_generation = cursor.read_generation("node_boot_generation")?;
                    let epoch = cursor.read_cell_epoch("epoch")?;
                    let checkpoint_seq = cursor.read_nonzero_u64("checkpoint_seq")?;
                    let checkpoint_digest = cursor.read_32()?;
                    ControlPlaneCommand::ControlPlaneAuditAnchor(
                        ControlPlaneAuditCommand::AnchorReconciliationCheckpoint {
                            cell,
                            node_boot_generation,
                            epoch,
                            checkpoint_seq,
                            checkpoint_digest,
                        },
                    )
                }
                _ => {
                    return Err(ScopeViolationError::InvalidNominalField(
                        "ControlPlaneAudit sub-tag",
                    ));
                }
            },
        };

        if !cursor.is_empty() {
            return Err(ScopeViolationError::TrailingBytes);
        }
        Ok(Self {
            idempotency_key,
            command,
        })
    }

    /// Encodes this proposal as a self-verifying wire frame
    /// (`NLOSCP01 || family_tag || idempotency_key || body_len || body || sha256`).
    ///
    /// # Panics
    ///
    /// Panics only if the serialized body exceeds `u32::MAX` (impossible for
    /// fixed-size control-plane command variants).
    #[must_use]
    pub fn encode_wire(&self) -> Vec<u8> {
        let body = self.encode_command_body();
        let mut frame = Vec::with_capacity(8 + 1 + 16 + 4 + body.len() + 32);
        frame.extend_from_slice(WIRE_MAGIC);
        frame.push(self.command.family_code().as_u8());
        frame.extend_from_slice(self.idempotency_key.as_bytes());
        let len_u32 = u32::try_from(body.len()).expect("control-plane command body <= u32::MAX");
        frame.extend_from_slice(&len_u32.to_be_bytes());
        frame.extend_from_slice(&body);
        let checksum: [u8; 32] = Sha256::digest(&frame).into();
        frame.extend_from_slice(&checksum);
        frame
    }

    /// Decodes and scope-checks a wire proposal frame, immediately rejecting
    /// any forbidden data-plane tag (`Task` / `Fiber` / `Topic` /
    /// `SemanticEvent` / `Artifact`) with
    /// [`ScopeViolationError::RejectDataPlanePayload`] (`[CONS-SCOPE-001]`).
    ///
    /// # Errors
    ///
    /// Fails closed on magic mismatch, checksum mismatch, data-plane family
    /// tags, or malformed command fields.
    ///
    /// # Panics
    ///
    /// Never panics: fixed-width header slices are length-checked upfront.
    pub fn decode_wire(bytes: &[u8]) -> Result<Self, ScopeViolationError> {
        if bytes.len() < 8 + 1 + 16 + 4 + 32 {
            return Err(ScopeViolationError::TruncatedPayload);
        }
        if &bytes[0..8] != WIRE_MAGIC {
            return Err(ScopeViolationError::MalformedWireHeader);
        }
        // Check `[CONS-SCOPE-001]` family tag FIRST so data-plane attempts
        // always surface `RejectDataPlanePayload`.
        let raw_family_tag = bytes[8];
        let family_code = CommandFamilyCode::from_wire_tag(raw_family_tag)?;

        let payload_end = bytes.len() - 32;
        let expected_checksum: [u8; 32] = Sha256::digest(&bytes[..payload_end]).into();
        if bytes[payload_end..] != expected_checksum {
            return Err(ScopeViolationError::PayloadChecksumMismatch);
        }

        let idem_bytes: [u8; 16] = bytes[9..25].try_into().expect("16-byte slice");
        let idempotency_key = IdempotencyKey::from_bytes(idem_bytes);
        let body_len =
            usize::try_from(u32::from_be_bytes(bytes[25..29].try_into().expect("4B"))).unwrap_or(0);
        if 29 + body_len != payload_end {
            return Err(ScopeViolationError::TrailingBytes);
        }
        Self::decode_command_body(family_code, idempotency_key, &bytes[29..payload_end])
    }
}

/// Builds a wire-framed data-plane probe carrying a forbidden
/// [`ForbiddenDataPlaneKind`] tag for adversarial `[CONS-SCOPE-001]` rejection
/// testing.
#[must_use]
pub fn encode_forbidden_data_plane_wire_probe(
    kind: ForbiddenDataPlaneKind,
    idempotency_key: IdempotencyKey,
    raw_body: &[u8],
) -> Vec<u8> {
    let mut frame = Vec::with_capacity(8 + 1 + 16 + 4 + raw_body.len() + 32);
    frame.extend_from_slice(WIRE_MAGIC);
    frame.push(kind.wire_tag());
    frame.extend_from_slice(idempotency_key.as_bytes());
    let len_u32 = u32::try_from(raw_body.len()).unwrap_or(u32::MAX);
    frame.extend_from_slice(&len_u32.to_be_bytes());
    frame.extend_from_slice(raw_body);
    let checksum: [u8; 32] = Sha256::digest(&frame).into();
    frame.extend_from_slice(&checksum);
    frame
}

// ============================================================================
// Replicated Control-Plane Materialized Records & Deterministic State Machine
// ============================================================================

/// Replicated `CellDirectory` entry stored in [`ControlPlaneStateMachine`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReplicatedCellDirectoryEntry {
    /// Nominal `CellIdentity`.
    pub cell: CellIdentity,
    /// Owning failure domain (`[TASK-SCALE-002]`).
    pub control_domain_id: ControlDomainId,
    /// Node `PrincipalId`.
    pub principal_id: PrincipalId,
    /// Monotonic boot generation.
    pub node_boot_generation: Generation,
    /// Monotonic `CellEpoch`.
    pub epoch: CellEpoch,
    /// Monotonic routing sequence within `(node_boot_generation, epoch)`.
    pub heartbeat_seq: u64,
    /// Optional ephemeral `SocketAddr` routing hint (`[DIST-NAME-001]`).
    pub endpoint_hint: Option<SocketAddr>,
    /// Whether this Cell has been decommissioned/fenced.
    pub decommissioned: bool,
    /// Consensus commit index that last updated this record.
    pub updated_at_commit_index: NonZeroU64,
}

/// Replicated root `QuotaLease` entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReplicatedQuotaLeaseEntry {
    /// Lease identifier.
    pub lease_id: QuotaLeaseId,
    /// Root budget pool identifier.
    pub budget_root: [u8; 16],
    /// Holder Cell.
    pub holder_cell: CellIdentity,
    /// Holder boot generation.
    pub node_boot_generation: Generation,
    /// Deducted face value.
    pub face_value: u64,
    /// Returned amount upon verified close.
    pub returned_amount: u64,
    /// Quarantined amount upon holder loss (`[LEASE-LOSS-001]`).
    pub quarantined_amount: u64,
    /// Current lease epoch.
    pub epoch: CellEpoch,
    /// Current fencing token.
    pub fencing_token: CellFencingToken,
    /// Fencing scope identifier.
    pub fence_scope: [u8; 16],
    /// Current disposition (`Active`, `Closed`, `Quarantined`).
    pub disposition: ReplicatedQuotaLeaseDisposition,
    /// Consensus commit index of latest state transition.
    pub committed_at_index: NonZeroU64,
}

/// Replicated root `CapacityLease` entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReplicatedCapacityLeaseEntry {
    /// Capacity lease identifier.
    pub capacity_lease_id: CapacityLeaseId,
    /// Pool identifier.
    pub pool_id: [u8; 16],
    /// Target Cell.
    pub target_cell: CellIdentity,
    /// Target boot generation.
    pub target_boot_generation: Generation,
    /// Committed amount.
    pub amount: u64,
    /// Capacity epoch.
    pub capacity_epoch: NonZeroU64,
    /// Fencing token.
    pub fencing_token: CellFencingToken,
    /// Consensus commit index.
    pub committed_at_index: NonZeroU64,
}

/// Replicated `DeviceLeaseHead` state (`[LEASE-DEVICE-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReplicatedDeviceHeadEntry {
    /// Device identifier.
    pub device_id: DeviceId,
    /// Current/last `ExclusiveDeviceLeaseId`.
    pub device_lease_id: ExclusiveDeviceLeaseId,
    /// Monotonic exclusivity epoch.
    pub exclusivity_epoch: NonZeroU64,
    /// Current holder Cell when `state == Active`.
    pub holder_cell: Option<CellIdentity>,
    /// Monotonic fencing token.
    pub fencing_token: CellFencingToken,
    /// Head state (`Free`, `Active`, `Quarantined`).
    pub state: ReplicatedDeviceHeadState,
    /// Consensus commit index.
    pub committed_at_index: NonZeroU64,
}

/// Majority-committed `TaskAuthorityAssignment` projection (`06-架构设计总纲-v0.5.md`
/// §26.1, `ADR-0026` §2.1 #3).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommittedTaskAuthorityAssignment {
    /// Assignment identifier.
    pub assignment_id: TaskAuthorityAssignmentId,
    /// Subject `TaskId`.
    pub task_id: TaskId,
    /// Subject `Task` generation.
    pub task_generation: Generation,
    /// 16-byte authority shard identifier.
    pub authority_shard: [u8; 16],
    /// Authoritative home `CellIdentity`.
    pub home_cell: CellIdentity,
    /// Authoritative `authority_term`.
    pub authority_term: NonZeroU64,
    /// Control epoch.
    pub control_epoch: u64,
    /// Participant-registry generation.
    pub participant_registry_generation: u64,
    /// Participant-registry root digest.
    pub participant_registry_root: [u8; 32],
    /// Authentic majority-committed Raft log index (`[DUR-ACK-002]`).
    pub consensus_commit_index: NonZeroU64,
    /// Consensus election term in which this assignment committed.
    pub consensus_term: NonZeroU64,
}

/// Majority-committed `TaskAuthorityTakeoverReceipt` projection
/// (`06-架构设计总纲-v0.5.md` §26.1, `ADR-0026` §2.1 #3).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommittedTaskAuthorityTakeover {
    /// Takeover receipt identifier.
    pub takeover_receipt_id: ReceiptId,
    /// Subject `TaskId`.
    pub task_id: TaskId,
    /// Subject `Task` generation.
    pub task_generation: Generation,
    /// Fenced predecessor assignment id.
    pub old_assignment_id: TaskAuthorityAssignmentId,
    /// Activated successor assignment id.
    pub new_assignment_id: TaskAuthorityAssignmentId,
    /// Frozen predecessor `authority_term`.
    pub frozen_old_authority_term: NonZeroU64,
    /// Activated successor `authority_term`.
    pub new_authority_term: NonZeroU64,
    /// New authoritative home `CellIdentity`.
    pub new_home_cell: CellIdentity,
    /// New control epoch.
    pub new_control_epoch: u64,
    /// Exact fenced participant set root.
    pub exact_fence_set_root: [u8; 32],
    /// Optional quarantined effect root.
    pub outstanding_effect_quarantine_root: Option<[u8; 32]>,
    /// Authentic majority-committed Raft log index (`[DUR-ACK-002]`).
    pub consensus_commit_index: NonZeroU64,
    /// Consensus election term in which this takeover receipt committed.
    pub consensus_term: NonZeroU64,
}

/// Replicated `MigrationIntent` audit anchor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReplicatedMigrationAnchor {
    /// Intent identifier.
    pub intent_id: [u8; 16],
    /// Source Cell.
    pub source_cell: CellIdentity,
    /// Target Cell.
    pub target_cell: CellIdentity,
    /// Migration epoch.
    pub epoch: CellEpoch,
    /// Canonical `MigrationIntent` digest.
    pub intent_digest: [u8; 32],
    /// Consensus commit index.
    pub consensus_commit_index: NonZeroU64,
}

/// Replicated `ReconciliationCheckpoint` audit anchor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReplicatedCheckpointAnchor {
    /// Emitting Cell.
    pub cell: CellIdentity,
    /// Boot generation.
    pub node_boot_generation: Generation,
    /// Control epoch.
    pub epoch: CellEpoch,
    /// Monotonic checkpoint sequence.
    pub checkpoint_seq: NonZeroU64,
    /// Canonical `CheckpointDigest`.
    pub checkpoint_digest: [u8; 32],
    /// Consensus commit index.
    pub consensus_commit_index: NonZeroU64,
}

/// Deterministic domain rejection recorded when a committed control-plane
/// command fails a monotonic CAS or balance invariant (without diverging
/// replicas).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub enum ControlPlaneDomainRejection {
    /// `IdempotencyKey` was reused with a different proposal payload.
    IdempotencyKeyPayloadConflict = 1,
    /// Stale `(node_boot_generation, epoch, heartbeat_seq)` on `CellDirectory`.
    StaleCellRoutingVersion = 2,
    /// Non-monotonic trust-policy epoch.
    StaleTrustPolicyEpoch = 3,
    /// Duplicate or insufficient root quota/capacity pool balance (`[LEASE-GRANT-001]`).
    InsufficientOrInvalidPoolBalance = 4,
    /// Non-monotonic `fencing_token` in `fence_scope` (`[LEASE-FENCE-001]`).
    NonMonotonicFencingToken = 5,
    /// Invalid `QuotaLease` settlement conservation or missing lease.
    InvalidLeaseSettlement = 6,
    /// `ExclusiveDeviceLease` CAS conflict or missing reset/zeroization proof
    /// (`[LEASE-DEVICE-001]`).
    DeviceExclusivityViolation = 7,
    /// `TaskAuthorityAssignment` already exists or takeover CAS mismatched
    /// (`[DIST-TASK-002]`).
    TaskAuthorityCasConflict = 8,
    /// Duplicate or non-monotonic audit anchor.
    AuditAnchorConflict = 9,
}

impl ControlPlaneDomainRejection {
    const fn from_u8(v: u8) -> Option<Self> {
        match v {
            1 => Some(Self::IdempotencyKeyPayloadConflict),
            2 => Some(Self::StaleCellRoutingVersion),
            3 => Some(Self::StaleTrustPolicyEpoch),
            4 => Some(Self::InsufficientOrInvalidPoolBalance),
            5 => Some(Self::NonMonotonicFencingToken),
            6 => Some(Self::InvalidLeaseSettlement),
            7 => Some(Self::DeviceExclusivityViolation),
            8 => Some(Self::TaskAuthorityCasConflict),
            9 => Some(Self::AuditAnchorConflict),
            _ => None,
        }
    }
}

/// Outcome of applying a committed [`ControlPlaneProposal`] to
/// [`ControlPlaneStateMachine`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AppliedProposalOutcome {
    /// Mutation succeeded at `consensus_commit_index`.
    Applied {
        /// Majority-committed Raft log index (`>= 1`).
        consensus_commit_index: NonZeroU64,
        /// Whether this outcome was served from the idempotent dedup table.
        idempotent_replay: bool,
    },
    /// Proposal was deterministically rejected by a state-machine CAS/invariant
    /// without mutating canonical tables.
    DomainRejected {
        /// Majority-committed log index at which rejection was finalized.
        consensus_commit_index: NonZeroU64,
        /// Rejection reason code.
        reason: ControlPlaneDomainRejection,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DedupRecord {
    proposal_digest: [u8; 32],
    committed_index: NonZeroU64,
    rejection: Option<ControlPlaneDomainRejection>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct QuotaPoolBalance {
    available: u64,
    leased_active: u64,
    quarantined: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CapacityPoolBalance {
    total_capacity: u64,
    committed: u64,
}

/// Snapshot serialization / integrity verification errors (`[DUR-SNAP-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SnapshotCodecError {
    /// Missing or invalid `NLOSSN01` snapshot magic header.
    InvalidMagic,
    /// Snapshot byte slice ended unexpectedly.
    Truncated,
    /// Trailing bytes remained after decoding snapshot tables.
    TrailingBytes,
    /// Invalid field value inside snapshot.
    CorruptField(&'static str),
    /// Trailing `SHA-256` checksum or recomputed `state_root_digest` mismatched.
    ChecksumMismatch,
}

impl fmt::Display for SnapshotCodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidMagic => {
                formatter.write_str("invalid ControlPlaneStateMachine snapshot magic")
            }
            Self::Truncated => formatter.write_str("truncated ControlPlaneStateMachine snapshot"),
            Self::TrailingBytes => {
                formatter.write_str("trailing bytes in ControlPlaneStateMachine snapshot")
            }
            Self::CorruptField(f) => write!(formatter, "corrupt snapshot field: {f}"),
            Self::ChecksumMismatch => {
                formatter.write_str("snapshot SHA-256 or state_root_digest mismatch")
            }
        }
    }
}

impl Error for SnapshotCodecError {}

/// Deterministic replicated state machine for the four `[CONS-SCOPE-001]`
/// control-plane metadata families (`ADR-0026` §2.1, `[DUR-SNAP-001]`).
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ControlPlaneStateMachine {
    last_applied_index: u64,
    last_applied_term: u64,
    authority_receipt_high_water: u64,
    trust_policy_epoch: u64,
    trust_policy_digest: [u8; 32],
    cells: BTreeMap<CellIdentity, ReplicatedCellDirectoryEntry>,
    quota_pools: BTreeMap<[u8; 16], QuotaPoolBalance>,
    quota_leases: BTreeMap<QuotaLeaseId, ReplicatedQuotaLeaseEntry>,
    capacity_pools: BTreeMap<[u8; 16], CapacityPoolBalance>,
    capacity_leases: BTreeMap<CapacityLeaseId, ReplicatedCapacityLeaseEntry>,
    device_heads: BTreeMap<DeviceId, ReplicatedDeviceHeadEntry>,
    fence_scope_high_water: BTreeMap<[u8; 16], CellFencingToken>,
    task_shards: BTreeMap<(TaskId, Generation), CommittedTaskAuthorityAssignment>,
    takeover_receipts: BTreeMap<ReceiptId, CommittedTaskAuthorityTakeover>,
    migration_anchors: BTreeMap<[u8; 16], ReplicatedMigrationAnchor>,
    checkpoint_anchors: BTreeMap<(CellIdentity, u64), ReplicatedCheckpointAnchor>,
    dedup_table: BTreeMap<IdempotencyKey, DedupRecord>,
}

impl ControlPlaneStateMachine {
    /// Creates an empty genesis control-plane state machine (`last_applied_index == 0`).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Highest committed log index applied (`last_lsn` in `[DUR-SNAP-001]`).
    #[must_use]
    pub const fn last_applied_index(&self) -> u64 {
        self.last_applied_index
    }

    /// Term of [`Self::last_applied_index`].
    #[must_use]
    pub const fn last_applied_term(&self) -> u64 {
        self.last_applied_term
    }

    /// High-water count of applied authority/takeover/lease receipts (`[DUR-SNAP-001]`).
    #[must_use]
    pub const fn authority_receipt_high_water(&self) -> u64 {
        self.authority_receipt_high_water
    }

    /// Current cluster-wide trust-policy epoch.
    #[must_use]
    pub const fn trust_policy_epoch(&self) -> u64 {
        self.trust_policy_epoch
    }

    /// Looks up a replicated `CellDirectory` entry.
    #[must_use]
    pub fn cell_entry(&self, cell: CellIdentity) -> Option<&ReplicatedCellDirectoryEntry> {
        self.cells.get(&cell)
    }

    /// Available balance in `budget_root`, if registered.
    #[must_use]
    pub fn quota_pool_available(&self, budget_root: &[u8; 16]) -> Option<u64> {
        self.quota_pools.get(budget_root).map(|p| p.available)
    }

    /// Quarantined balance in `budget_root`, if registered.
    #[must_use]
    pub fn quota_pool_quarantined(&self, budget_root: &[u8; 16]) -> Option<u64> {
        self.quota_pools.get(budget_root).map(|p| p.quarantined)
    }

    /// Looks up a replicated `QuotaLease`.
    #[must_use]
    pub fn quota_lease(&self, lease_id: QuotaLeaseId) -> Option<&ReplicatedQuotaLeaseEntry> {
        self.quota_leases.get(&lease_id)
    }

    /// Looks up a replicated `DeviceLeaseHead`.
    #[must_use]
    pub fn device_head(&self, device_id: DeviceId) -> Option<&ReplicatedDeviceHeadEntry> {
        self.device_heads.get(&device_id)
    }

    /// Looks up the active majority-committed `TaskAuthorityAssignment` for
    /// `(task_id, task_generation)`.
    #[must_use]
    pub fn task_authority_assignment(
        &self,
        task_id: TaskId,
        task_generation: Generation,
    ) -> Option<&CommittedTaskAuthorityAssignment> {
        self.task_shards.get(&(task_id, task_generation))
    }

    /// Looks up a majority-committed `TaskAuthorityTakeoverReceipt` by id.
    #[must_use]
    pub fn takeover_receipt(
        &self,
        receipt_id: ReceiptId,
    ) -> Option<&CommittedTaskAuthorityTakeover> {
        self.takeover_receipts.get(&receipt_id)
    }

    /// Looks up a committed `MigrationIntent` anchor.
    #[must_use]
    pub fn migration_anchor(&self, intent_id: &[u8; 16]) -> Option<&ReplicatedMigrationAnchor> {
        self.migration_anchors.get(intent_id)
    }

    /// Looks up a committed `ReconciliationCheckpoint` anchor.
    #[must_use]
    pub fn checkpoint_anchor(
        &self,
        cell: CellIdentity,
        checkpoint_seq: u64,
    ) -> Option<&ReplicatedCheckpointAnchor> {
        self.checkpoint_anchors.get(&(cell, checkpoint_seq))
    }

    /// Deterministically applies a committed log entry at `(log_index, term)`.
    ///
    /// # Panics
    ///
    /// Panics if `log_index.get() <= self.last_applied_index` (violating
    /// strictly monotonic log application order).
    #[allow(clippy::too_many_lines)]
    pub fn apply_committed_entry(
        &mut self,
        log_index: NonZeroU64,
        term: NonZeroU64,
        proposal: &ControlPlaneProposal,
    ) -> AppliedProposalOutcome {
        assert!(
            log_index.get() > self.last_applied_index,
            "committed entries must be applied in strictly increasing log_index order"
        );
        self.last_applied_index = log_index.get();
        self.last_applied_term = term.get();

        let prop_digest = proposal.proposal_digest();
        if let Some(&existing) = self.dedup_table.get(&proposal.idempotency_key) {
            if existing.proposal_digest == prop_digest {
                return match existing.rejection {
                    None => AppliedProposalOutcome::Applied {
                        consensus_commit_index: existing.committed_index,
                        idempotent_replay: true,
                    },
                    Some(reason) => AppliedProposalOutcome::DomainRejected {
                        consensus_commit_index: existing.committed_index,
                        reason,
                    },
                };
            }
            return AppliedProposalOutcome::DomainRejected {
                consensus_commit_index: log_index,
                reason: ControlPlaneDomainRejection::IdempotencyKeyPayloadConflict,
            };
        }

        let rejection = self.apply_inner(log_index, term, &proposal.command);
        self.dedup_table.insert(
            proposal.idempotency_key,
            DedupRecord {
                proposal_digest: prop_digest,
                committed_index: log_index,
                rejection,
            },
        );

        match rejection {
            None => {
                self.authority_receipt_high_water =
                    self.authority_receipt_high_water.saturating_add(1);
                AppliedProposalOutcome::Applied {
                    consensus_commit_index: log_index,
                    idempotent_replay: false,
                }
            }
            Some(reason) => AppliedProposalOutcome::DomainRejected {
                consensus_commit_index: log_index,
                reason,
            },
        }
    }

    #[allow(clippy::too_many_lines)]
    fn apply_inner(
        &mut self,
        log_index: NonZeroU64,
        term: NonZeroU64,
        command: &ControlPlaneCommand,
    ) -> Option<ControlPlaneDomainRejection> {
        match *command {
            ControlPlaneCommand::CellDirectoryMutation(cmd) => match cmd {
                CellDirectoryCommand::UpsertCellEndpoint {
                    cell,
                    control_domain_id,
                    principal_id,
                    node_boot_generation,
                    epoch,
                    heartbeat_seq,
                    endpoint_hint,
                } => {
                    if let Some(prev) = self.cells.get(&cell) {
                        let prev_ver = (prev.node_boot_generation, prev.epoch, prev.heartbeat_seq);
                        let next_ver = (node_boot_generation, epoch, heartbeat_seq);
                        if prev.decommissioned || next_ver <= prev_ver {
                            return Some(ControlPlaneDomainRejection::StaleCellRoutingVersion);
                        }
                    }
                    self.cells.insert(
                        cell,
                        ReplicatedCellDirectoryEntry {
                            cell,
                            control_domain_id,
                            principal_id,
                            node_boot_generation,
                            epoch,
                            heartbeat_seq,
                            endpoint_hint,
                            decommissioned: false,
                            updated_at_commit_index: log_index,
                        },
                    );
                    None
                }
                CellDirectoryCommand::AdvanceTrustPolicyEpoch {
                    new_trust_epoch,
                    policy_digest,
                } => {
                    if new_trust_epoch <= self.trust_policy_epoch {
                        return Some(ControlPlaneDomainRejection::StaleTrustPolicyEpoch);
                    }
                    self.trust_policy_epoch = new_trust_epoch;
                    self.trust_policy_digest = policy_digest;
                    None
                }
                CellDirectoryCommand::DecommissionCell { cell, fenced_epoch } => {
                    let Some(entry) = self.cells.get_mut(&cell) else {
                        return Some(ControlPlaneDomainRejection::StaleCellRoutingVersion);
                    };
                    if fenced_epoch < entry.epoch {
                        return Some(ControlPlaneDomainRejection::StaleCellRoutingVersion);
                    }
                    entry.epoch = fenced_epoch;
                    entry.endpoint_hint = None;
                    entry.decommissioned = true;
                    entry.updated_at_commit_index = log_index;
                    None
                }
            },
            ControlPlaneCommand::RootLeasePoolMutation(cmd) => match cmd {
                RootLeasePoolCommand::RegisterQuotaPool {
                    budget_root,
                    initial_available,
                } => {
                    if self.quota_pools.contains_key(&budget_root) {
                        return Some(ControlPlaneDomainRejection::InsufficientOrInvalidPoolBalance);
                    }
                    self.quota_pools.insert(
                        budget_root,
                        QuotaPoolBalance {
                            available: initial_available,
                            leased_active: 0,
                            quarantined: 0,
                        },
                    );
                    None
                }
                RootLeasePoolCommand::AllocateQuotaLease {
                    lease_id,
                    budget_root,
                    holder_cell,
                    node_boot_generation,
                    face_value,
                    epoch,
                    fencing_token,
                    fence_scope,
                } => {
                    if self.quota_leases.contains_key(&lease_id) || face_value == 0 {
                        return Some(ControlPlaneDomainRejection::InsufficientOrInvalidPoolBalance);
                    }
                    if let Some(&prev_tok) = self.fence_scope_high_water.get(&fence_scope)
                        && fencing_token <= prev_tok
                    {
                        return Some(ControlPlaneDomainRejection::NonMonotonicFencingToken);
                    }
                    let Some(pool) = self.quota_pools.get_mut(&budget_root) else {
                        return Some(ControlPlaneDomainRejection::InsufficientOrInvalidPoolBalance);
                    };
                    if pool.available < face_value {
                        return Some(ControlPlaneDomainRejection::InsufficientOrInvalidPoolBalance);
                    }
                    // `[LEASE-GRANT-001]`: deduct full face_value before issuing lease.
                    pool.available -= face_value;
                    pool.leased_active = pool.leased_active.saturating_add(face_value);
                    self.fence_scope_high_water
                        .insert(fence_scope, fencing_token);
                    self.quota_leases.insert(
                        lease_id,
                        ReplicatedQuotaLeaseEntry {
                            lease_id,
                            budget_root,
                            holder_cell,
                            node_boot_generation,
                            face_value,
                            returned_amount: 0,
                            quarantined_amount: 0,
                            epoch,
                            fencing_token,
                            fence_scope,
                            disposition: ReplicatedQuotaLeaseDisposition::Active,
                            committed_at_index: log_index,
                        },
                    );
                    None
                }
                RootLeasePoolCommand::SettleOrQuarantineQuotaLease {
                    lease_id,
                    disposition,
                    returned_amount,
                    quarantined_amount,
                    new_epoch,
                    new_fencing_token,
                } => {
                    let Some(existing) = self.quota_leases.get(&lease_id).copied() else {
                        return Some(ControlPlaneDomainRejection::InvalidLeaseSettlement);
                    };
                    if existing.disposition != ReplicatedQuotaLeaseDisposition::Active
                        || disposition == ReplicatedQuotaLeaseDisposition::Active
                    {
                        return Some(ControlPlaneDomainRejection::InvalidLeaseSettlement);
                    }
                    if new_epoch < existing.epoch {
                        return Some(ControlPlaneDomainRejection::InvalidLeaseSettlement);
                    }
                    if let Some(&prev_tok) = self.fence_scope_high_water.get(&existing.fence_scope)
                        && new_fencing_token <= prev_tok
                    {
                        return Some(ControlPlaneDomainRejection::NonMonotonicFencingToken);
                    }
                    let Some(total_accounted) = returned_amount.checked_add(quarantined_amount)
                    else {
                        return Some(ControlPlaneDomainRejection::InvalidLeaseSettlement);
                    };
                    if total_accounted > existing.face_value {
                        return Some(ControlPlaneDomainRejection::InvalidLeaseSettlement);
                    }
                    // `[LEASE-LOSS-001]`: Quarantined disposition MUST NOT return
                    // unreconciled funds back into `available`.
                    if disposition == ReplicatedQuotaLeaseDisposition::Quarantined
                        && returned_amount != 0
                    {
                        return Some(ControlPlaneDomainRejection::InvalidLeaseSettlement);
                    }
                    let pool = self
                        .quota_pools
                        .get_mut(&existing.budget_root)
                        .expect("registered budget_root");
                    pool.leased_active = pool.leased_active.saturating_sub(existing.face_value);
                    pool.available = pool.available.saturating_add(returned_amount);
                    pool.quarantined = pool.quarantined.saturating_add(quarantined_amount);
                    self.fence_scope_high_water
                        .insert(existing.fence_scope, new_fencing_token);
                    let lease_mut = self.quota_leases.get_mut(&lease_id).expect("present");
                    lease_mut.disposition = disposition;
                    lease_mut.returned_amount = returned_amount;
                    lease_mut.quarantined_amount = quarantined_amount;
                    lease_mut.epoch = new_epoch;
                    lease_mut.fencing_token = new_fencing_token;
                    lease_mut.committed_at_index = log_index;
                    None
                }
                RootLeasePoolCommand::AllocateCapacityLease {
                    capacity_lease_id,
                    pool_id,
                    pool_total_capacity,
                    target_cell,
                    target_boot_generation,
                    amount,
                    capacity_epoch,
                    fencing_token,
                } => {
                    if self.capacity_leases.contains_key(&capacity_lease_id) || amount == 0 {
                        return Some(ControlPlaneDomainRejection::InsufficientOrInvalidPoolBalance);
                    }
                    let pool = self
                        .capacity_pools
                        .entry(pool_id)
                        .or_insert(CapacityPoolBalance {
                            total_capacity: pool_total_capacity,
                            committed: 0,
                        });
                    if pool.total_capacity != pool_total_capacity {
                        return Some(ControlPlaneDomainRejection::InsufficientOrInvalidPoolBalance);
                    }
                    let Some(new_committed) = pool.committed.checked_add(amount) else {
                        return Some(ControlPlaneDomainRejection::InsufficientOrInvalidPoolBalance);
                    };
                    if new_committed > pool.total_capacity {
                        return Some(ControlPlaneDomainRejection::InsufficientOrInvalidPoolBalance);
                    }
                    pool.committed = new_committed;
                    self.capacity_leases.insert(
                        capacity_lease_id,
                        ReplicatedCapacityLeaseEntry {
                            capacity_lease_id,
                            pool_id,
                            target_cell,
                            target_boot_generation,
                            amount,
                            capacity_epoch,
                            fencing_token,
                            committed_at_index: log_index,
                        },
                    );
                    None
                }
                RootLeasePoolCommand::TransitionExclusiveDeviceHead {
                    device_id,
                    device_lease_id,
                    exclusivity_epoch,
                    holder_cell,
                    fencing_token,
                    new_state,
                    has_reset_zeroization_proof,
                } => {
                    if new_state == ReplicatedDeviceHeadState::Active && holder_cell.is_none() {
                        return Some(ControlPlaneDomainRejection::DeviceExclusivityViolation);
                    }
                    if new_state != ReplicatedDeviceHeadState::Active && holder_cell.is_some() {
                        return Some(ControlPlaneDomainRejection::DeviceExclusivityViolation);
                    }
                    if let Some(prev) = self.device_heads.get(&device_id) {
                        if exclusivity_epoch <= prev.exclusivity_epoch
                            || fencing_token <= prev.fencing_token
                        {
                            return Some(ControlPlaneDomainRejection::DeviceExclusivityViolation);
                        }
                        // `[LEASE-DEVICE-001]`: only `Free` may transition to
                        // `Active`; returning to `Free` from `Active` or
                        // `Quarantined` requires reset + zeroization attestation!
                        match (prev.state, new_state) {
                            (
                                ReplicatedDeviceHeadState::Free,
                                ReplicatedDeviceHeadState::Active,
                            )
                            | (
                                ReplicatedDeviceHeadState::Active,
                                ReplicatedDeviceHeadState::Quarantined,
                            ) => {}
                            (
                                ReplicatedDeviceHeadState::Active
                                | ReplicatedDeviceHeadState::Quarantined,
                                ReplicatedDeviceHeadState::Free,
                            ) => {
                                if !has_reset_zeroization_proof {
                                    return Some(
                                        ControlPlaneDomainRejection::DeviceExclusivityViolation,
                                    );
                                }
                            }
                            _ => {
                                return Some(
                                    ControlPlaneDomainRejection::DeviceExclusivityViolation,
                                );
                            }
                        }
                    } else if new_state != ReplicatedDeviceHeadState::Active {
                        return Some(ControlPlaneDomainRejection::DeviceExclusivityViolation);
                    }

                    self.device_heads.insert(
                        device_id,
                        ReplicatedDeviceHeadEntry {
                            device_id,
                            device_lease_id,
                            exclusivity_epoch,
                            holder_cell,
                            fencing_token,
                            state: new_state,
                            committed_at_index: log_index,
                        },
                    );
                    None
                }
            },
            ControlPlaneCommand::TaskAuthorityShardMutation(cmd) => match cmd {
                TaskAuthorityShardCommand::AssignInitialAuthority {
                    assignment_id,
                    task_id,
                    task_generation,
                    authority_shard,
                    home_cell,
                    authority_term,
                    control_epoch,
                    participant_registry_generation,
                    participant_registry_root,
                } => {
                    if self.task_shards.contains_key(&(task_id, task_generation)) {
                        return Some(ControlPlaneDomainRejection::TaskAuthorityCasConflict);
                    }
                    self.task_shards.insert(
                        (task_id, task_generation),
                        CommittedTaskAuthorityAssignment {
                            assignment_id,
                            task_id,
                            task_generation,
                            authority_shard,
                            home_cell,
                            authority_term,
                            control_epoch,
                            participant_registry_generation,
                            participant_registry_root,
                            consensus_commit_index: log_index,
                            consensus_term: term,
                        },
                    );
                    None
                }
                TaskAuthorityShardCommand::RecordCompletedTakeover {
                    takeover_receipt_id,
                    task_id,
                    task_generation,
                    old_assignment_id,
                    new_assignment_id,
                    frozen_old_authority_term,
                    new_authority_term,
                    new_home_cell,
                    new_control_epoch,
                    new_participant_registry_generation,
                    new_participant_registry_root,
                    exact_fence_set_root,
                    outstanding_effect_quarantine_root,
                } => {
                    if self.takeover_receipts.contains_key(&takeover_receipt_id)
                        || new_authority_term <= frozen_old_authority_term
                        || old_assignment_id == new_assignment_id
                    {
                        return Some(ControlPlaneDomainRejection::TaskAuthorityCasConflict);
                    }
                    let Some(current) = self.task_shards.get(&(task_id, task_generation)).copied()
                    else {
                        return Some(ControlPlaneDomainRejection::TaskAuthorityCasConflict);
                    };
                    if current.assignment_id != old_assignment_id
                        || current.authority_term != frozen_old_authority_term
                        || new_control_epoch <= current.control_epoch
                        || new_participant_registry_generation
                            <= current.participant_registry_generation
                    {
                        return Some(ControlPlaneDomainRejection::TaskAuthorityCasConflict);
                    }

                    self.task_shards.insert(
                        (task_id, task_generation),
                        CommittedTaskAuthorityAssignment {
                            assignment_id: new_assignment_id,
                            task_id,
                            task_generation,
                            authority_shard: current.authority_shard,
                            home_cell: new_home_cell,
                            authority_term: new_authority_term,
                            control_epoch: new_control_epoch,
                            participant_registry_generation: new_participant_registry_generation,
                            participant_registry_root: new_participant_registry_root,
                            consensus_commit_index: log_index,
                            consensus_term: term,
                        },
                    );
                    self.takeover_receipts.insert(
                        takeover_receipt_id,
                        CommittedTaskAuthorityTakeover {
                            takeover_receipt_id,
                            task_id,
                            task_generation,
                            old_assignment_id,
                            new_assignment_id,
                            frozen_old_authority_term,
                            new_authority_term,
                            new_home_cell,
                            new_control_epoch,
                            exact_fence_set_root,
                            outstanding_effect_quarantine_root,
                            consensus_commit_index: log_index,
                            consensus_term: term,
                        },
                    );
                    None
                }
            },
            ControlPlaneCommand::ControlPlaneAuditAnchor(cmd) => match cmd {
                ControlPlaneAuditCommand::AnchorMigrationIntent {
                    intent_id,
                    source_cell,
                    target_cell,
                    epoch,
                    intent_digest,
                } => {
                    if self.migration_anchors.contains_key(&intent_id) || source_cell == target_cell
                    {
                        return Some(ControlPlaneDomainRejection::AuditAnchorConflict);
                    }
                    self.migration_anchors.insert(
                        intent_id,
                        ReplicatedMigrationAnchor {
                            intent_id,
                            source_cell,
                            target_cell,
                            epoch,
                            intent_digest,
                            consensus_commit_index: log_index,
                        },
                    );
                    None
                }
                ControlPlaneAuditCommand::AnchorReconciliationCheckpoint {
                    cell,
                    node_boot_generation,
                    epoch,
                    checkpoint_seq,
                    checkpoint_digest,
                } => {
                    let seq_u64 = checkpoint_seq.get();
                    if self.checkpoint_anchors.contains_key(&(cell, seq_u64)) {
                        return Some(ControlPlaneDomainRejection::AuditAnchorConflict);
                    }
                    if seq_u64 > 1
                        && let Some(prev) = self.checkpoint_anchors.get(&(cell, seq_u64 - 1))
                        && (node_boot_generation, epoch) < (prev.node_boot_generation, prev.epoch)
                    {
                        return Some(ControlPlaneDomainRejection::AuditAnchorConflict);
                    }
                    self.checkpoint_anchors.insert(
                        (cell, seq_u64),
                        ReplicatedCheckpointAnchor {
                            cell,
                            node_boot_generation,
                            epoch,
                            checkpoint_seq,
                            checkpoint_digest,
                            consensus_commit_index: log_index,
                        },
                    );
                    None
                }
            },
        }
    }

    /// Computes the canonical deterministic `SHA-256` state root over all four
    /// control-plane tables, dedup/result map, and high-water marks.
    #[must_use]
    pub fn state_root_digest(&self) -> [u8; 32] {
        let mut hasher = Sha256::new();
        hasher.update(STATE_ROOT_DOMAIN);
        let canonical = self.encode_canonical_state_body();
        hasher.update((canonical.len() as u64).to_be_bytes());
        hasher.update(&canonical);
        hasher.finalize().into()
    }

    /// Serializes the full [`ControlPlaneStateMachine`] (including `[DUR-SNAP-001]`
    /// `last_applied_index`, `state_root_digest`, `dedup_table`, and
    /// `authority_receipt_high_water`) into a self-verifying snapshot blob.
    ///
    /// # Panics
    ///
    /// Panics only if the serialized state exceeds `u32::MAX` bytes.
    #[must_use]
    pub fn encode_snapshot(&self) -> Vec<u8> {
        let body = self.encode_canonical_state_body();
        let state_root = self.state_root_digest();
        let mut out = Vec::with_capacity(8 + 32 + 4 + body.len() + 32);
        out.extend_from_slice(SNAPSHOT_MAGIC);
        out.extend_from_slice(&state_root);
        let body_len = u32::try_from(body.len()).expect("snapshot body <= u32::MAX");
        out.extend_from_slice(&body_len.to_be_bytes());
        out.extend_from_slice(&body);

        let mut checksum_hasher = Sha256::new();
        checksum_hasher.update(SNAPSHOT_CHECKSUM_DOMAIN);
        checksum_hasher.update(&out);
        let checksum: [u8; 32] = checksum_hasher.finalize().into();
        out.extend_from_slice(&checksum);
        out
    }

    /// Reconstructs and integrity-verifies a [`ControlPlaneStateMachine`] from
    /// snapshot bytes (`[DUR-SNAP-001]`, `[DUR-FAILCLOSED-001]`).
    ///
    /// # Errors
    ///
    /// Fails closed on magic, framing, checksum, or `state_root_digest` mismatch.
    ///
    /// # Panics
    ///
    /// Never panics: fixed-width header slices are length-checked upfront.
    #[allow(clippy::too_many_lines)]
    pub fn decode_snapshot(bytes: &[u8]) -> Result<Self, SnapshotCodecError> {
        if bytes.len() < 8 + 32 + 4 + 32 {
            return Err(SnapshotCodecError::Truncated);
        }
        if &bytes[0..8] != SNAPSHOT_MAGIC {
            return Err(SnapshotCodecError::InvalidMagic);
        }
        let payload_end = bytes.len() - 32;
        let mut checksum_hasher = Sha256::new();
        checksum_hasher.update(SNAPSHOT_CHECKSUM_DOMAIN);
        checksum_hasher.update(&bytes[..payload_end]);
        let expected_checksum: [u8; 32] = checksum_hasher.finalize().into();
        if bytes[payload_end..] != expected_checksum {
            return Err(SnapshotCodecError::ChecksumMismatch);
        }

        let declared_root: [u8; 32] = bytes[8..40].try_into().expect("32B");
        let body_len =
            usize::try_from(u32::from_be_bytes(bytes[40..44].try_into().expect("4B"))).unwrap_or(0);
        if 44 + body_len != payload_end {
            return Err(SnapshotCodecError::TrailingBytes);
        }

        let mut c = ByteCursor::new(&bytes[44..payload_end]);
        let map_err = |_| SnapshotCodecError::Truncated;
        let last_applied_index = c.read_u64().map_err(map_err)?;
        let last_applied_term = c.read_u64().map_err(map_err)?;
        let authority_receipt_high_water = c.read_u64().map_err(map_err)?;
        let trust_policy_epoch = c.read_u64().map_err(map_err)?;
        let trust_policy_digest = c.read_32().map_err(map_err)?;

        let cell_count = c.read_len()?;
        let mut cells = BTreeMap::new();
        for _ in 0..cell_count {
            let cell = c
                .read_cell_id()
                .map_err(|_| SnapshotCodecError::CorruptField("cell"))?;
            let control_domain_id = ControlDomainId::from_bytes(c.read_16().map_err(map_err)?);
            let principal_id = PrincipalId::from_bytes(c.read_16().map_err(map_err)?);
            let node_boot_generation = c
                .read_generation("boot_gen")
                .map_err(|_| SnapshotCodecError::CorruptField("boot_gen"))?;
            let epoch = c
                .read_cell_epoch("epoch")
                .map_err(|_| SnapshotCodecError::CorruptField("epoch"))?;
            let heartbeat_seq = c.read_u64().map_err(map_err)?;
            let endpoint_hint = c
                .read_socket_addr_opt()
                .map_err(|_| SnapshotCodecError::CorruptField("endpoint_hint"))?;
            let decommissioned = c.read_bool("decommissioned")?;
            let updated_at_commit_index = c
                .read_nonzero_u64("updated_at_commit_index")
                .map_err(|_| SnapshotCodecError::CorruptField("updated_at_commit_index"))?;
            cells.insert(
                cell,
                ReplicatedCellDirectoryEntry {
                    cell,
                    control_domain_id,
                    principal_id,
                    node_boot_generation,
                    epoch,
                    heartbeat_seq,
                    endpoint_hint,
                    decommissioned,
                    updated_at_commit_index,
                },
            );
        }

        let quota_pool_count = c.read_len()?;
        let mut quota_pools = BTreeMap::new();
        for _ in 0..quota_pool_count {
            let root = c.read_16().map_err(map_err)?;
            let available = c.read_u64().map_err(map_err)?;
            let leased_active = c.read_u64().map_err(map_err)?;
            let quarantined = c.read_u64().map_err(map_err)?;
            quota_pools.insert(
                root,
                QuotaPoolBalance {
                    available,
                    leased_active,
                    quarantined,
                },
            );
        }

        let quota_lease_count = c.read_len()?;
        let mut quota_leases = BTreeMap::new();
        for _ in 0..quota_lease_count {
            let lease_id = QuotaLeaseId::from_bytes(c.read_16().map_err(map_err)?);
            let budget_root = c.read_16().map_err(map_err)?;
            let holder_cell = c.read_cell_id().map_err(map_err)?;
            let node_boot_generation = c
                .read_generation("boot_gen")
                .map_err(|_| SnapshotCodecError::CorruptField("ql.boot_gen"))?;
            let face_value = c.read_u64().map_err(map_err)?;
            let returned_amount = c.read_u64().map_err(map_err)?;
            let quarantined_amount = c.read_u64().map_err(map_err)?;
            let epoch = c
                .read_cell_epoch("ql.epoch")
                .map_err(|_| SnapshotCodecError::CorruptField("ql.epoch"))?;
            let fencing_token = c
                .read_fencing_token("ql.tok")
                .map_err(|_| SnapshotCodecError::CorruptField("ql.tok"))?;
            let fence_scope = c.read_16().map_err(map_err)?;
            let disposition =
                ReplicatedQuotaLeaseDisposition::from_u8(c.read_u8().map_err(map_err)?)
                    .ok_or(SnapshotCodecError::CorruptField("ql.disposition"))?;
            let committed_at_index = c
                .read_nonzero_u64("ql.idx")
                .map_err(|_| SnapshotCodecError::CorruptField("ql.idx"))?;
            quota_leases.insert(
                lease_id,
                ReplicatedQuotaLeaseEntry {
                    lease_id,
                    budget_root,
                    holder_cell,
                    node_boot_generation,
                    face_value,
                    returned_amount,
                    quarantined_amount,
                    epoch,
                    fencing_token,
                    fence_scope,
                    disposition,
                    committed_at_index,
                },
            );
        }

        let capacity_pool_count = c.read_len()?;
        let mut capacity_pools = BTreeMap::new();
        for _ in 0..capacity_pool_count {
            let pool_id = c.read_16().map_err(map_err)?;
            let total_capacity = c.read_u64().map_err(map_err)?;
            let committed = c.read_u64().map_err(map_err)?;
            capacity_pools.insert(
                pool_id,
                CapacityPoolBalance {
                    total_capacity,
                    committed,
                },
            );
        }

        let capacity_lease_count = c.read_len()?;
        let mut capacity_leases = BTreeMap::new();
        for _ in 0..capacity_lease_count {
            let capacity_lease_id = CapacityLeaseId::from_bytes(c.read_16().map_err(map_err)?);
            let pool_id = c.read_16().map_err(map_err)?;
            let target_cell = c.read_cell_id().map_err(map_err)?;
            let target_boot_generation = c
                .read_generation("cl.boot")
                .map_err(|_| SnapshotCodecError::CorruptField("cl.boot"))?;
            let amount = c.read_u64().map_err(map_err)?;
            let capacity_epoch = c
                .read_nonzero_u64("cl.epoch")
                .map_err(|_| SnapshotCodecError::CorruptField("cl.epoch"))?;
            let fencing_token = c
                .read_fencing_token("cl.tok")
                .map_err(|_| SnapshotCodecError::CorruptField("cl.tok"))?;
            let committed_at_index = c
                .read_nonzero_u64("cl.idx")
                .map_err(|_| SnapshotCodecError::CorruptField("cl.idx"))?;
            capacity_leases.insert(
                capacity_lease_id,
                ReplicatedCapacityLeaseEntry {
                    capacity_lease_id,
                    pool_id,
                    target_cell,
                    target_boot_generation,
                    amount,
                    capacity_epoch,
                    fencing_token,
                    committed_at_index,
                },
            );
        }

        let device_head_count = c.read_len()?;
        let mut device_heads = BTreeMap::new();
        for _ in 0..device_head_count {
            let device_id = DeviceId::from_bytes(c.read_16().map_err(map_err)?);
            let device_lease_id = ExclusiveDeviceLeaseId::from_bytes(c.read_16().map_err(map_err)?);
            let exclusivity_epoch = c
                .read_nonzero_u64("dh.epoch")
                .map_err(|_| SnapshotCodecError::CorruptField("dh.epoch"))?;
            let holder_cell = match c.read_u8().map_err(map_err)? {
                0 => None,
                1 => Some(c.read_cell_id().map_err(map_err)?),
                _ => return Err(SnapshotCodecError::CorruptField("dh.holder_cell")),
            };
            let fencing_token = c
                .read_fencing_token("dh.tok")
                .map_err(|_| SnapshotCodecError::CorruptField("dh.tok"))?;
            let state = ReplicatedDeviceHeadState::from_u8(c.read_u8().map_err(map_err)?)
                .ok_or(SnapshotCodecError::CorruptField("dh.state"))?;
            let committed_at_index = c
                .read_nonzero_u64("dh.idx")
                .map_err(|_| SnapshotCodecError::CorruptField("dh.idx"))?;
            device_heads.insert(
                device_id,
                ReplicatedDeviceHeadEntry {
                    device_id,
                    device_lease_id,
                    exclusivity_epoch,
                    holder_cell,
                    fencing_token,
                    state,
                    committed_at_index,
                },
            );
        }

        let fence_scope_count = c.read_len()?;
        let mut fence_scope_high_water = BTreeMap::new();
        for _ in 0..fence_scope_count {
            let scope = c.read_16().map_err(map_err)?;
            let tok = c
                .read_fencing_token("fs.tok")
                .map_err(|_| SnapshotCodecError::CorruptField("fs.tok"))?;
            fence_scope_high_water.insert(scope, tok);
        }

        let task_shard_count = c.read_len()?;
        let mut task_shards = BTreeMap::new();
        for _ in 0..task_shard_count {
            let assignment_id =
                TaskAuthorityAssignmentId::from_bytes(c.read_16().map_err(map_err)?);
            let task_id = TaskId::from_bytes(c.read_16().map_err(map_err)?);
            let task_generation = c
                .read_generation("ts.gen")
                .map_err(|_| SnapshotCodecError::CorruptField("ts.gen"))?;
            let authority_shard = c.read_16().map_err(map_err)?;
            let home_cell = c.read_cell_id().map_err(map_err)?;
            let authority_term = c
                .read_nonzero_u64("ts.term")
                .map_err(|_| SnapshotCodecError::CorruptField("ts.term"))?;
            let control_epoch = c.read_u64().map_err(map_err)?;
            let participant_registry_generation = c.read_u64().map_err(map_err)?;
            let participant_registry_root = c.read_32().map_err(map_err)?;
            let consensus_commit_index = c
                .read_nonzero_u64("ts.cidx")
                .map_err(|_| SnapshotCodecError::CorruptField("ts.cidx"))?;
            let consensus_term = c
                .read_nonzero_u64("ts.cterm")
                .map_err(|_| SnapshotCodecError::CorruptField("ts.cterm"))?;
            task_shards.insert(
                (task_id, task_generation),
                CommittedTaskAuthorityAssignment {
                    assignment_id,
                    task_id,
                    task_generation,
                    authority_shard,
                    home_cell,
                    authority_term,
                    control_epoch,
                    participant_registry_generation,
                    participant_registry_root,
                    consensus_commit_index,
                    consensus_term,
                },
            );
        }

        let takeover_receipt_count = c.read_len()?;
        let mut takeover_receipts = BTreeMap::new();
        for _ in 0..takeover_receipt_count {
            let takeover_receipt_id = ReceiptId::from_bytes(c.read_16().map_err(map_err)?);
            let task_id = TaskId::from_bytes(c.read_16().map_err(map_err)?);
            let task_generation = c
                .read_generation("tr.gen")
                .map_err(|_| SnapshotCodecError::CorruptField("tr.gen"))?;
            let old_assignment_id =
                TaskAuthorityAssignmentId::from_bytes(c.read_16().map_err(map_err)?);
            let new_assignment_id =
                TaskAuthorityAssignmentId::from_bytes(c.read_16().map_err(map_err)?);
            let frozen_old_authority_term = c
                .read_nonzero_u64("tr.old_term")
                .map_err(|_| SnapshotCodecError::CorruptField("tr.old_term"))?;
            let new_authority_term = c
                .read_nonzero_u64("tr.new_term")
                .map_err(|_| SnapshotCodecError::CorruptField("tr.new_term"))?;
            let new_home_cell = c.read_cell_id().map_err(map_err)?;
            let new_control_epoch = c.read_u64().map_err(map_err)?;
            let exact_fence_set_root = c.read_32().map_err(map_err)?;
            let outstanding_effect_quarantine_root = match c.read_u8().map_err(map_err)? {
                0 => None,
                1 => Some(c.read_32().map_err(map_err)?),
                _ => return Err(SnapshotCodecError::CorruptField("tr.quarantine_opt")),
            };
            let consensus_commit_index = c
                .read_nonzero_u64("tr.cidx")
                .map_err(|_| SnapshotCodecError::CorruptField("tr.cidx"))?;
            let consensus_term = c
                .read_nonzero_u64("tr.cterm")
                .map_err(|_| SnapshotCodecError::CorruptField("tr.cterm"))?;
            takeover_receipts.insert(
                takeover_receipt_id,
                CommittedTaskAuthorityTakeover {
                    takeover_receipt_id,
                    task_id,
                    task_generation,
                    old_assignment_id,
                    new_assignment_id,
                    frozen_old_authority_term,
                    new_authority_term,
                    new_home_cell,
                    new_control_epoch,
                    exact_fence_set_root,
                    outstanding_effect_quarantine_root,
                    consensus_commit_index,
                    consensus_term,
                },
            );
        }

        let migration_anchor_count = c.read_len()?;
        let mut migration_anchors = BTreeMap::new();
        for _ in 0..migration_anchor_count {
            let intent_id = c.read_16().map_err(map_err)?;
            let source_cell = c.read_cell_id().map_err(map_err)?;
            let target_cell = c.read_cell_id().map_err(map_err)?;
            let epoch = c
                .read_cell_epoch("ma.epoch")
                .map_err(|_| SnapshotCodecError::CorruptField("ma.epoch"))?;
            let intent_digest = c.read_32().map_err(map_err)?;
            let consensus_commit_index = c
                .read_nonzero_u64("ma.cidx")
                .map_err(|_| SnapshotCodecError::CorruptField("ma.cidx"))?;
            migration_anchors.insert(
                intent_id,
                ReplicatedMigrationAnchor {
                    intent_id,
                    source_cell,
                    target_cell,
                    epoch,
                    intent_digest,
                    consensus_commit_index,
                },
            );
        }

        let checkpoint_anchor_count = c.read_len()?;
        let mut checkpoint_anchors = BTreeMap::new();
        for _ in 0..checkpoint_anchor_count {
            let cell = c.read_cell_id().map_err(map_err)?;
            let node_boot_generation = c
                .read_generation("ca.boot")
                .map_err(|_| SnapshotCodecError::CorruptField("ca.boot"))?;
            let epoch = c
                .read_cell_epoch("ca.epoch")
                .map_err(|_| SnapshotCodecError::CorruptField("ca.epoch"))?;
            let checkpoint_seq = c
                .read_nonzero_u64("ca.seq")
                .map_err(|_| SnapshotCodecError::CorruptField("ca.seq"))?;
            let checkpoint_digest = c.read_32().map_err(map_err)?;
            let consensus_commit_index = c
                .read_nonzero_u64("ca.cidx")
                .map_err(|_| SnapshotCodecError::CorruptField("ca.cidx"))?;
            checkpoint_anchors.insert(
                (cell, checkpoint_seq.get()),
                ReplicatedCheckpointAnchor {
                    cell,
                    node_boot_generation,
                    epoch,
                    checkpoint_seq,
                    checkpoint_digest,
                    consensus_commit_index,
                },
            );
        }

        let dedup_entry_count = c.read_len()?;
        let mut dedup_table = BTreeMap::new();
        for _ in 0..dedup_entry_count {
            let key = IdempotencyKey::from_bytes(c.read_16().map_err(map_err)?);
            let proposal_digest = c.read_32().map_err(map_err)?;
            let committed_index = c
                .read_nonzero_u64("dd.idx")
                .map_err(|_| SnapshotCodecError::CorruptField("dd.idx"))?;
            let rejection = match c.read_u8().map_err(map_err)? {
                0 => None,
                code => Some(
                    ControlPlaneDomainRejection::from_u8(code)
                        .ok_or(SnapshotCodecError::CorruptField("dd.rejection"))?,
                ),
            };
            dedup_table.insert(
                key,
                DedupRecord {
                    proposal_digest,
                    committed_index,
                    rejection,
                },
            );
        }

        if !c.is_empty() {
            return Err(SnapshotCodecError::TrailingBytes);
        }

        let reconstructed = Self {
            last_applied_index,
            last_applied_term,
            authority_receipt_high_water,
            trust_policy_epoch,
            trust_policy_digest,
            cells,
            quota_pools,
            quota_leases,
            capacity_pools,
            capacity_leases,
            device_heads,
            fence_scope_high_water,
            task_shards,
            takeover_receipts,
            migration_anchors,
            checkpoint_anchors,
            dedup_table,
        };
        if reconstructed.state_root_digest() != declared_root {
            return Err(SnapshotCodecError::ChecksumMismatch);
        }
        Ok(reconstructed)
    }

    #[allow(clippy::too_many_lines)]
    fn encode_canonical_state_body(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(512);
        out.extend_from_slice(&self.last_applied_index.to_be_bytes());
        out.extend_from_slice(&self.last_applied_term.to_be_bytes());
        out.extend_from_slice(&self.authority_receipt_high_water.to_be_bytes());
        out.extend_from_slice(&self.trust_policy_epoch.to_be_bytes());
        out.extend_from_slice(&self.trust_policy_digest);

        encode_len(&mut out, self.cells.len());
        for entry in self.cells.values() {
            out.extend_from_slice(entry.cell.as_bytes());
            out.extend_from_slice(entry.control_domain_id.as_bytes());
            out.extend_from_slice(entry.principal_id.as_bytes());
            out.extend_from_slice(&entry.node_boot_generation.get().to_be_bytes());
            out.extend_from_slice(&entry.epoch.get().to_be_bytes());
            out.extend_from_slice(&entry.heartbeat_seq.to_be_bytes());
            encode_socket_addr_opt(&mut out, entry.endpoint_hint);
            out.push(u8::from(entry.decommissioned));
            out.extend_from_slice(&entry.updated_at_commit_index.get().to_be_bytes());
        }

        encode_len(&mut out, self.quota_pools.len());
        for (root, pool) in &self.quota_pools {
            out.extend_from_slice(root);
            out.extend_from_slice(&pool.available.to_be_bytes());
            out.extend_from_slice(&pool.leased_active.to_be_bytes());
            out.extend_from_slice(&pool.quarantined.to_be_bytes());
        }

        encode_len(&mut out, self.quota_leases.len());
        for ql in self.quota_leases.values() {
            out.extend_from_slice(ql.lease_id.as_bytes());
            out.extend_from_slice(&ql.budget_root);
            out.extend_from_slice(ql.holder_cell.as_bytes());
            out.extend_from_slice(&ql.node_boot_generation.get().to_be_bytes());
            out.extend_from_slice(&ql.face_value.to_be_bytes());
            out.extend_from_slice(&ql.returned_amount.to_be_bytes());
            out.extend_from_slice(&ql.quarantined_amount.to_be_bytes());
            out.extend_from_slice(&ql.epoch.get().to_be_bytes());
            out.extend_from_slice(&ql.fencing_token.get().to_be_bytes());
            out.extend_from_slice(&ql.fence_scope);
            out.push(ql.disposition as u8);
            out.extend_from_slice(&ql.committed_at_index.get().to_be_bytes());
        }

        encode_len(&mut out, self.capacity_pools.len());
        for (pool_id, pool) in &self.capacity_pools {
            out.extend_from_slice(pool_id);
            out.extend_from_slice(&pool.total_capacity.to_be_bytes());
            out.extend_from_slice(&pool.committed.to_be_bytes());
        }

        encode_len(&mut out, self.capacity_leases.len());
        for cl in self.capacity_leases.values() {
            out.extend_from_slice(cl.capacity_lease_id.as_bytes());
            out.extend_from_slice(&cl.pool_id);
            out.extend_from_slice(cl.target_cell.as_bytes());
            out.extend_from_slice(&cl.target_boot_generation.get().to_be_bytes());
            out.extend_from_slice(&cl.amount.to_be_bytes());
            out.extend_from_slice(&cl.capacity_epoch.get().to_be_bytes());
            out.extend_from_slice(&cl.fencing_token.get().to_be_bytes());
            out.extend_from_slice(&cl.committed_at_index.get().to_be_bytes());
        }

        encode_len(&mut out, self.device_heads.len());
        for dh in self.device_heads.values() {
            out.extend_from_slice(dh.device_id.as_bytes());
            out.extend_from_slice(dh.device_lease_id.as_bytes());
            out.extend_from_slice(&dh.exclusivity_epoch.get().to_be_bytes());
            match dh.holder_cell {
                Some(cell) => {
                    out.push(1);
                    out.extend_from_slice(cell.as_bytes());
                }
                None => out.push(0),
            }
            out.extend_from_slice(&dh.fencing_token.get().to_be_bytes());
            out.push(dh.state as u8);
            out.extend_from_slice(&dh.committed_at_index.get().to_be_bytes());
        }

        encode_len(&mut out, self.fence_scope_high_water.len());
        for (scope, tok) in &self.fence_scope_high_water {
            out.extend_from_slice(scope);
            out.extend_from_slice(&tok.get().to_be_bytes());
        }

        encode_len(&mut out, self.task_shards.len());
        for ts in self.task_shards.values() {
            out.extend_from_slice(ts.assignment_id.as_bytes());
            out.extend_from_slice(ts.task_id.as_bytes());
            out.extend_from_slice(&ts.task_generation.get().to_be_bytes());
            out.extend_from_slice(&ts.authority_shard);
            out.extend_from_slice(ts.home_cell.as_bytes());
            out.extend_from_slice(&ts.authority_term.get().to_be_bytes());
            out.extend_from_slice(&ts.control_epoch.to_be_bytes());
            out.extend_from_slice(&ts.participant_registry_generation.to_be_bytes());
            out.extend_from_slice(&ts.participant_registry_root);
            out.extend_from_slice(&ts.consensus_commit_index.get().to_be_bytes());
            out.extend_from_slice(&ts.consensus_term.get().to_be_bytes());
        }

        encode_len(&mut out, self.takeover_receipts.len());
        for tr in self.takeover_receipts.values() {
            out.extend_from_slice(tr.takeover_receipt_id.as_bytes());
            out.extend_from_slice(tr.task_id.as_bytes());
            out.extend_from_slice(&tr.task_generation.get().to_be_bytes());
            out.extend_from_slice(tr.old_assignment_id.as_bytes());
            out.extend_from_slice(tr.new_assignment_id.as_bytes());
            out.extend_from_slice(&tr.frozen_old_authority_term.get().to_be_bytes());
            out.extend_from_slice(&tr.new_authority_term.get().to_be_bytes());
            out.extend_from_slice(tr.new_home_cell.as_bytes());
            out.extend_from_slice(&tr.new_control_epoch.to_be_bytes());
            out.extend_from_slice(&tr.exact_fence_set_root);
            match tr.outstanding_effect_quarantine_root {
                Some(root) => {
                    out.push(1);
                    out.extend_from_slice(&root);
                }
                None => out.push(0),
            }
            out.extend_from_slice(&tr.consensus_commit_index.get().to_be_bytes());
            out.extend_from_slice(&tr.consensus_term.get().to_be_bytes());
        }

        encode_len(&mut out, self.migration_anchors.len());
        for ma in self.migration_anchors.values() {
            out.extend_from_slice(&ma.intent_id);
            out.extend_from_slice(ma.source_cell.as_bytes());
            out.extend_from_slice(ma.target_cell.as_bytes());
            out.extend_from_slice(&ma.epoch.get().to_be_bytes());
            out.extend_from_slice(&ma.intent_digest);
            out.extend_from_slice(&ma.consensus_commit_index.get().to_be_bytes());
        }

        encode_len(&mut out, self.checkpoint_anchors.len());
        for ca in self.checkpoint_anchors.values() {
            out.extend_from_slice(ca.cell.as_bytes());
            out.extend_from_slice(&ca.node_boot_generation.get().to_be_bytes());
            out.extend_from_slice(&ca.epoch.get().to_be_bytes());
            out.extend_from_slice(&ca.checkpoint_seq.get().to_be_bytes());
            out.extend_from_slice(&ca.checkpoint_digest);
            out.extend_from_slice(&ca.consensus_commit_index.get().to_be_bytes());
        }

        encode_len(&mut out, self.dedup_table.len());
        for (key, rec) in &self.dedup_table {
            out.extend_from_slice(key.as_bytes());
            out.extend_from_slice(&rec.proposal_digest);
            out.extend_from_slice(&rec.committed_index.get().to_be_bytes());
            out.push(rec.rejection.map_or(0, |r| r as u8));
        }

        out
    }
}

fn encode_len(out: &mut Vec<u8>, len: usize) {
    let len_u32 = u32::try_from(len).expect("collection len <= u32::MAX");
    out.extend_from_slice(&len_u32.to_be_bytes());
}

fn encode_socket_addr_opt(out: &mut Vec<u8>, addr: Option<SocketAddr>) {
    match addr {
        None => out.push(0),
        Some(SocketAddr::V4(v4)) => {
            out.push(4);
            out.extend_from_slice(&v4.ip().octets());
            out.extend_from_slice(&v4.port().to_be_bytes());
        }
        Some(SocketAddr::V6(v6)) => {
            out.push(6);
            out.extend_from_slice(&v6.ip().octets());
            out.extend_from_slice(&v6.port().to_be_bytes());
        }
    }
}

struct ByteCursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> ByteCursor<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    const fn is_empty(&self) -> bool {
        self.pos >= self.bytes.len()
    }

    fn read_slice(&mut self, len: usize) -> Result<&'a [u8], ScopeViolationError> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or(ScopeViolationError::TruncatedPayload)?;
        if end > self.bytes.len() {
            return Err(ScopeViolationError::TruncatedPayload);
        }
        let slice = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn read_u8(&mut self) -> Result<u8, ScopeViolationError> {
        Ok(self.read_slice(1)?[0])
    }

    fn read_bool(&mut self, field: &'static str) -> Result<bool, SnapshotCodecError> {
        match self.read_u8().map_err(|_| SnapshotCodecError::Truncated)? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(SnapshotCodecError::CorruptField(field)),
        }
    }

    fn read_len(&mut self) -> Result<usize, SnapshotCodecError> {
        let raw = self
            .read_slice(4)
            .map_err(|_| SnapshotCodecError::Truncated)?;
        let len = u32::from_be_bytes(raw.try_into().expect("4B"));
        usize::try_from(len).map_err(|_| SnapshotCodecError::CorruptField("len"))
    }

    fn read_u64(&mut self) -> Result<u64, ScopeViolationError> {
        let s = self.read_slice(8)?;
        Ok(u64::from_be_bytes(s.try_into().expect("8B")))
    }

    fn read_nonzero_u64(&mut self, field: &'static str) -> Result<NonZeroU64, ScopeViolationError> {
        NonZeroU64::new(self.read_u64()?).ok_or(ScopeViolationError::InvalidNominalField(field))
    }

    fn read_generation(&mut self, field: &'static str) -> Result<Generation, ScopeViolationError> {
        Ok(Generation::new(self.read_nonzero_u64(field)?))
    }

    fn read_cell_epoch(&mut self, field: &'static str) -> Result<CellEpoch, ScopeViolationError> {
        CellEpoch::from_u64(self.read_u64()?).ok_or(ScopeViolationError::InvalidNominalField(field))
    }

    fn read_fencing_token(
        &mut self,
        field: &'static str,
    ) -> Result<CellFencingToken, ScopeViolationError> {
        CellFencingToken::from_u64(self.read_u64()?)
            .ok_or(ScopeViolationError::InvalidNominalField(field))
    }

    fn read_16(&mut self) -> Result<[u8; 16], ScopeViolationError> {
        let s = self.read_slice(16)?;
        Ok(s.try_into().expect("16B"))
    }

    fn read_32(&mut self) -> Result<[u8; 32], ScopeViolationError> {
        let s = self.read_slice(32)?;
        Ok(s.try_into().expect("32B"))
    }

    fn read_cell_id(&mut self) -> Result<CellIdentity, ScopeViolationError> {
        Ok(CellIdentity::from_domain(SchedulerDomainId::from_bytes(
            self.read_16()?,
        )))
    }

    fn read_socket_addr_opt(&mut self) -> Result<Option<SocketAddr>, ScopeViolationError> {
        match self.read_u8()? {
            0 => Ok(None),
            4 => {
                let ip: [u8; 4] = self.read_slice(4)?.try_into().expect("4B");
                let port = u16::from_be_bytes(self.read_slice(2)?.try_into().expect("2B"));
                Ok(Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::from(ip)), port)))
            }
            6 => {
                let ip: [u8; 16] = self.read_16()?;
                let port = u16::from_be_bytes(self.read_slice(2)?.try_into().expect("2B"));
                Ok(Some(SocketAddr::new(IpAddr::V6(Ipv6Addr::from(ip)), port)))
            }
            _ => Err(ScopeViolationError::InvalidNominalField(
                "SocketAddr family tag",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_cell(tag: u8) -> CellIdentity {
        CellIdentity::from_domain(SchedulerDomainId::from_bytes([tag; 16]))
    }

    #[test]
    fn wire_guard_rejects_all_forbidden_data_plane_families() {
        let key = IdempotencyKey::from_bytes([0x11; 16]);
        let forbidden = [
            ForbiddenDataPlaneKind::TaskExecutionMutation,
            ForbiddenDataPlaneKind::FiberScheduleEvent,
            ForbiddenDataPlaneKind::TopicMessagePublish,
            ForbiddenDataPlaneKind::SemanticEventAppend,
            ForbiddenDataPlaneKind::ArtifactBulkPayload,
            ForbiddenDataPlaneKind::UnknownNonControlFamily(0xFE),
        ];
        for kind in forbidden {
            let probe = encode_forbidden_data_plane_wire_probe(kind, key, b"data-plane-payload");
            let err = ControlPlaneProposal::decode_wire(&probe)
                .expect_err("data-plane payload must fail closed");
            assert!(
                matches!(err, ScopeViolationError::RejectDataPlanePayload(r) if r.kind == kind),
                "unexpected error for {kind:?}: {err:?}"
            );
        }
    }

    #[test]
    fn all_four_control_plane_families_roundtrip_and_snapshot_deterministically() {
        let mut sm = ControlPlaneStateMachine::new();
        let cell_a = sample_cell(0xA1);
        let cell_b = sample_cell(0xB2);

        let proposals = [
            ControlPlaneProposal::new(
                IdempotencyKey::from_bytes([1; 16]),
                ControlPlaneCommand::CellDirectoryMutation(
                    CellDirectoryCommand::UpsertCellEndpoint {
                        cell: cell_a,
                        control_domain_id: ControlDomainId::from_bytes([0x10; 16]),
                        principal_id: PrincipalId::from_bytes([0x20; 16]),
                        node_boot_generation: Generation::INITIAL,
                        epoch: CellEpoch::INITIAL,
                        heartbeat_seq: 1,
                        endpoint_hint: Some("127.0.0.1:19001".parse().expect("addr")),
                    },
                ),
            ),
            ControlPlaneProposal::new(
                IdempotencyKey::from_bytes([2; 16]),
                ControlPlaneCommand::RootLeasePoolMutation(
                    RootLeasePoolCommand::RegisterQuotaPool {
                        budget_root: [0x55; 16],
                        initial_available: 1_000,
                    },
                ),
            ),
            ControlPlaneProposal::new(
                IdempotencyKey::from_bytes([3; 16]),
                ControlPlaneCommand::TaskAuthorityShardMutation(
                    TaskAuthorityShardCommand::AssignInitialAuthority {
                        assignment_id: TaskAuthorityAssignmentId::from_bytes([0x71; 16]),
                        task_id: TaskId::from_bytes([0x81; 16]),
                        task_generation: Generation::INITIAL,
                        authority_shard: [0x91; 16],
                        home_cell: cell_a,
                        authority_term: NonZeroU64::new(1).expect("nz"),
                        control_epoch: 1,
                        participant_registry_generation: 1,
                        participant_registry_root: [0xAA; 32],
                    },
                ),
            ),
            ControlPlaneProposal::new(
                IdempotencyKey::from_bytes([4; 16]),
                ControlPlaneCommand::ControlPlaneAuditAnchor(
                    ControlPlaneAuditCommand::AnchorMigrationIntent {
                        intent_id: [0xC1; 16],
                        source_cell: cell_a,
                        target_cell: cell_b,
                        epoch: CellEpoch::INITIAL,
                        intent_digest: [0xDD; 32],
                    },
                ),
            ),
        ];

        for (idx, prop) in proposals.iter().enumerate() {
            let wire = prop.encode_wire();
            let decoded = ControlPlaneProposal::decode_wire(&wire).expect("decode valid");
            assert_eq!(&decoded, prop);
            let log_idx = NonZeroU64::new((idx as u64) + 1).expect("nz");
            let outcome = sm.apply_committed_entry(log_idx, NonZeroU64::MIN, &decoded);
            assert_eq!(
                outcome,
                AppliedProposalOutcome::Applied {
                    consensus_commit_index: log_idx,
                    idempotent_replay: false,
                }
            );
        }

        let snap = sm.encode_snapshot();
        let restored = ControlPlaneStateMachine::decode_snapshot(&snap).expect("restore snapshot");
        assert_eq!(restored, sm);
        assert_eq!(restored.state_root_digest(), sm.state_root_digest());
    }
}
