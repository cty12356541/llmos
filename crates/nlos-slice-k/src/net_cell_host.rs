//! Networked `CellHost` assembly (`ADR-0026`, `W71-C-SYNC-MIGRATE`) — binds one
//! local [`CellHost`] rooted at an isolated host directory (zero shared
//! filesystem paths across physical hosts) together with `nlos-net` TLS 1.3
//! mTLS transport, `nlos-consensus` (`SingleHostPassthroughNode` for `N = 1`
//! with `0` sockets/timers/TLS vs `MultiHostQuorumNode` for `N >= 3`),
//! `[LEASE-PROTO-001]` [`RemoteLeaseProtocolServer`], `[SEM-CHECKPOINT-001]`
//! [`SemanticNetSyncServer`], `[DIST-ART-001]` [`ArtifactNetTransferServer`],
//! and Stage D `W72` `SystemControlCenter` [`CellTopologyInspector`] /
//! [`CellLeaseInspector`] read-only projections.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::future::Future;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use nlos_artifact::net_transfer::ArtifactNetTransferServer;
use nlos_artifact::{ArtifactError, ArtifactStore, ArtifactSyncPolicy, ArtifactSyncPolicyRegistry};
use nlos_cell::{
    CellEpoch, CellFence, CellIdentity, CellRegistrationEntry, ControlPlaneLinkState,
    MigrationIntent, MigrationObject,
};
use nlos_consensus::{
    ConsensusCommitReceipt, ConsensusError, ConsensusRole, ConsensusTerm, ControlPlaneProposal,
    LogIndex, MultiHostQuorumNode, SingleHostPassthroughNode,
};
use nlos_identity::BootstrapPrincipalRequest;
use nlos_lease::clock_horizon::{ClockSkewPolicy, CrossHostLeaseFamily};
use nlos_lease::remote_proto::{LEASE_PROTO_SCHEMA_VERSION, RemoteLeaseProtocolServer};
use nlos_lease::{
    CapacityLeaseGrant, CapacityLeaseState, ExclusiveDeviceLeaseGrant, ExclusiveDeviceLeaseState,
    LeaseInstant, QuotaLeaseGrant, QuotaLeaseState,
};
use nlos_net::{
    BulkHandlerCompletion, BulkStreamHandler, BulkStreamReader, ClusterProfile, EndpointHintRecord,
    EndpointResolver, EndpointRoutingVersion, EndpointStaleError, EndpointUpdateDecision,
    IncomingBulkMeta, IncomingRpcRequest, MonotonicEndpointResolver, MtlsCellTransport,
    MtlsEndpointListener, NetInitError, NetTransportConfig, NodeSecurityDeps, RpcHandlerReply,
    RpcRequestHandler, RpcWireStatus, RunningMtlsListener, SocketActivityAccounting,
};
use nlos_schema::MAX_SYSTEM_CONTROL_ENUM_LIMIT;
use nlos_schema::sabi::v1::{
    CellLeaseKind, CellLeaseLockState, CellReachabilityState, PartitionAutonomyGateState,
    RetryDirective, SabiErrorCode, SabiFailure,
};
use nlos_semantic::net_sync::{
    SemanticNetSyncError, SemanticNetSyncServer, SemanticSyncProvider,
    verify_cross_domain_item_provenance,
};
use nlos_semantic::{
    CrossDomainConvergenceReport, CrossDomainEventItem, SignedSemanticVectorCheckpoint,
    verify_signed_vector_checkpoint,
};
use nlos_system_control::cell_inspector::{
    CellLeaseInspector, CellLeasePage, CellLeaseRecord, CellPage, CellSummaryRecord,
    CellTopologyInspector, ProcessMigrationControlRequest,
};
use nlos_types::{
    CapacityLeaseId, ControlDomainId, ExclusiveDeviceLeaseId, Generation, ProcessId, QuotaLeaseId,
    ReceiptId,
};
use sha2::{Digest, Sha256};

use crate::cell_host::{CellHost, CellHostConfig};
use crate::error::SliceKError;

/// Default non-zero `trust_snapshot_digest` used by [`NetCellHost`]'s semantic
/// checkpoint/provenance sync responder when none is explicitly configured.
pub const DEFAULT_NET_CELL_TRUST_SNAPSHOT_DIGEST: [u8; 32] = [0x71; 32];

/// Subdirectory inside each isolated `<host_root>` holding this host's own
/// private federation mirror (`CellDirectory`), never shared across hosts.
pub const PRIVATE_FEDERATION_MIRROR_DIR: &str = "local-cell-directory";

/// Subdirectory inside `<host_root>` holding the `N = 1` consensus SQLite WAL.
pub const PRIVATE_CONSENSUS_DIR: &str = "consensus";

/// Pluggable RPC sub-dispatcher registered on [`NetCellHost`]'s multiplexed
/// mTLS service listener (used by `net_migration_drive` for `NLOSMG01` and
/// `NLOSTB01` frames).
pub trait SubProtocolRpcHandler: Send + Sync {
    /// Returns `true` when `payload` carries this sub-protocol's wire magic.
    fn matches_frame(&self, payload: &[u8]) -> bool;

    /// Asynchronously processes an authenticated mTLS RPC frame.
    fn handle_frame(
        &self,
        request: IncomingRpcRequest,
    ) -> Pin<Box<dyn Future<Output = RpcHandlerReply> + Send + 'static>>;
}

/// Unified control-plane consensus handle supporting zero-overhead `N = 1`
/// desktop passthrough (`0` sockets, `0` timers, `0` TLS state) and `N >= 3`
/// multi-host Raft quorum (`ADR-0026` §2.1 & §6.3).
#[derive(Debug)]
pub enum NetConsensusBackend {
    /// `ClusterProfile::SingleHostDesktop` (`N = 1`, `0` sockets/timers/TLS).
    SingleHostPassthrough(Box<SingleHostPassthroughNode>),
    /// `ClusterProfile::MultiHostCluster` (`N >= 3` distinct `ControlDomainId`s).
    MultiHostQuorum(Arc<MultiHostQuorumNode>),
}

impl NetConsensusBackend {
    /// Active [`ClusterProfile`] of this consensus backend.
    #[must_use]
    pub fn cluster_profile(&self) -> ClusterProfile {
        match self {
            Self::SingleHostPassthrough(node) => node.cluster_profile(),
            Self::MultiHostQuorum(node) => node.cluster_profile(),
        }
    }

    /// Current Raft [`ConsensusRole`].
    #[must_use]
    pub fn role(&self) -> ConsensusRole {
        match self {
            Self::SingleHostPassthrough(_) => ConsensusRole::Leader,
            Self::MultiHostQuorum(node) => node.role(),
        }
    }

    /// Current durable `current_term`.
    #[must_use]
    pub fn current_term(&self) -> ConsensusTerm {
        match self {
            Self::SingleHostPassthrough(node) => node.read_state(|_, _, term| term),
            Self::MultiHostQuorum(node) => node.current_term(),
        }
    }

    /// Current majority-committed `consensus_commit_index`.
    #[must_use]
    pub fn consensus_commit_index(&self) -> LogIndex {
        match self {
            Self::SingleHostPassthrough(node) => node.consensus_commit_index(),
            Self::MultiHostQuorum(node) => node.consensus_commit_index(),
        }
    }

    /// Deterministic `ControlPlaneStateMachine` root digest.
    #[must_use]
    pub fn state_root_digest(&self) -> [u8; 32] {
        match self {
            Self::SingleHostPassthrough(node) => node.read_state(|sm, _, _| sm.state_root_digest()),
            Self::MultiHostQuorum(node) => node.state_root_digest(),
        }
    }

    /// Active socket count held by the consensus backend (`0` in `N = 1`).
    #[must_use]
    pub fn active_socket_count(&self) -> usize {
        match self {
            Self::SingleHostPassthrough(node) => node.active_socket_count(),
            Self::MultiHostQuorum(node) => node.active_socket_count(),
        }
    }

    /// Active background timer count held by the consensus backend (`0` in `N = 1`).
    #[must_use]
    pub const fn active_background_timer_count(&self) -> usize {
        match self {
            Self::SingleHostPassthrough(_) => 0,
            Self::MultiHostQuorum(_) => 2,
        }
    }

    /// Whether TLS certificates are required by this backend (`false` in `N = 1`).
    #[must_use]
    pub const fn requires_tls_certificates(&self) -> bool {
        match self {
            Self::SingleHostPassthrough(_) => false,
            Self::MultiHostQuorum(_) => true,
        }
    }

    /// Proposes a `[CONS-SCOPE-001]` command and awaits majority durable commit.
    ///
    /// # Errors
    ///
    /// Propagates [`ConsensusError`] (`NotLeader`, `StaleLeaderTerm`, `ProposalTimeout`,
    /// or `SQLite` durability failure).
    pub async fn propose(
        &self,
        proposal: ControlPlaneProposal,
        timeout_budget: Duration,
    ) -> Result<ConsensusCommitReceipt, ConsensusError> {
        match self {
            Self::SingleHostPassthrough(node) => node.propose(proposal),
            Self::MultiHostQuorum(node) => node.propose(proposal, timeout_budget).await,
        }
    }
}

/// Fail-closed initialization and runtime errors of [`NetCellHost`].
#[derive(Debug)]
pub enum NetCellHostError {
    /// Local [`CellHost`] or authority slice operation failed.
    Slice(Box<SliceKError>),
    /// Shared `ArtifactStore` open failed.
    Artifact(Box<ArtifactError>),
    /// `nlos-net` mTLS listener or transport initialization failed.
    NetInit(Box<NetInitError>),
    /// `nlos-consensus` initialization or commit failed.
    Consensus(Box<ConsensusError>),
    /// Endpoint routing hint registration failed monotonic watermark check.
    EndpointStale(Box<EndpointStaleError>),
    /// Attempted a networked operation on an `N = 1` `SingleHostDesktop` host.
    NotNetworked(&'static str),
}

impl fmt::Display for NetCellHostError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Slice(err) => write!(formatter, "CellHost slice error: {err}"),
            Self::Artifact(err) => write!(formatter, "ArtifactStore open error: {err}"),
            Self::NetInit(err) => write!(formatter, "mTLS transport init error: {err}"),
            Self::Consensus(err) => write!(formatter, "consensus backend error: {err}"),
            Self::EndpointStale(err) => write!(formatter, "stale endpoint routing hint: {err}"),
            Self::NotNetworked(op) => {
                write!(
                    formatter,
                    "operation `{op}` requires MultiHostCluster profile"
                )
            }
        }
    }
}

impl Error for NetCellHostError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Slice(err) => Some(err.as_ref()),
            Self::Artifact(err) => Some(err.as_ref()),
            Self::NetInit(err) => Some(err.as_ref()),
            Self::Consensus(err) => Some(err.as_ref()),
            Self::EndpointStale(err) => Some(err.as_ref()),
            Self::NotNetworked(_) => None,
        }
    }
}

impl From<SliceKError> for NetCellHostError {
    fn from(error: SliceKError) -> Self {
        Self::Slice(Box::new(error))
    }
}

impl From<ArtifactError> for NetCellHostError {
    fn from(error: ArtifactError) -> Self {
        Self::Artifact(Box::new(error))
    }
}

impl From<NetInitError> for NetCellHostError {
    fn from(error: NetInitError) -> Self {
        Self::NetInit(Box::new(error))
    }
}

impl From<ConsensusError> for NetCellHostError {
    fn from(error: ConsensusError) -> Self {
        Self::Consensus(Box::new(error))
    }
}

impl From<EndpointStaleError> for NetCellHostError {
    fn from(error: EndpointStaleError) -> Self {
        Self::EndpointStale(Box::new(error))
    }
}

/// [`SemanticSyncProvider`] adapter over [`NetCellHost`]'s locked [`CellHost`].
pub struct NetCellSemanticSyncBridge {
    host: Arc<Mutex<CellHost>>,
    trust_snapshot_digest: [u8; 32],
    min_signed_at_ms: u64,
}

impl fmt::Debug for NetCellSemanticSyncBridge {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NetCellSemanticSyncBridge")
            .field("trust_snapshot_digest", &self.trust_snapshot_digest)
            .field("min_signed_at_ms", &self.min_signed_at_ms)
            .finish_non_exhaustive()
    }
}

impl NetCellSemanticSyncBridge {
    /// Creates a bridge binding `host` and a non-zero `trust_snapshot_digest`.
    #[must_use]
    pub const fn new(host: Arc<Mutex<CellHost>>, trust_snapshot_digest: [u8; 32]) -> Self {
        Self {
            host,
            trust_snapshot_digest,
            min_signed_at_ms: 0,
        }
    }

    /// Clamps checkpoint `signed_at_ms` at or above `min_signed_at_ms` (e.g.
    /// the host's authoritative `AuthorityClock` floor at `NetCellHost` boot).
    #[must_use]
    pub const fn with_min_signed_at_ms(mut self, min_signed_at_ms: u64) -> Self {
        self.min_signed_at_ms = min_signed_at_ms;
        self
    }

    /// Configured non-zero `trust_snapshot_digest`.
    #[must_use]
    pub const fn trust_snapshot_digest(&self) -> [u8; 32] {
        self.trust_snapshot_digest
    }
}

impl SemanticSyncProvider for NetCellSemanticSyncBridge {
    fn local_cell_identity(&self) -> CellIdentity {
        self.host
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .fence()
            .identity()
    }

    fn local_cell_epoch(&self) -> CellEpoch {
        self.host
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .fence()
            .epoch()
    }

    fn export_local_checkpoint(
        &self,
        signed_at_ms: u64,
    ) -> Result<SignedSemanticVectorCheckpoint, SemanticNetSyncError> {
        let guard = self.host.lock().unwrap_or_else(PoisonError::into_inner);
        let runtime = guard.runtime();
        Ok(runtime.semantic().export_signed_vector_checkpoint(
            &runtime.identity,
            runtime.semantic_writer(),
            self.trust_snapshot_digest,
            signed_at_ms.max(self.min_signed_at_ms),
        )?)
    }

    fn verify_remote_checkpoint(
        &self,
        checkpoint: &SignedSemanticVectorCheckpoint,
    ) -> Result<(), SemanticNetSyncError> {
        let guard = self.host.lock().unwrap_or_else(PoisonError::into_inner);
        verify_signed_vector_checkpoint(&guard.runtime().identity, checkpoint)?;
        Ok(())
    }

    fn export_local_cross_domain_events(
        &self,
    ) -> Result<Vec<CrossDomainEventItem>, SemanticNetSyncError> {
        let guard = self.host.lock().unwrap_or_else(PoisonError::into_inner);
        Ok(guard
            .runtime()
            .semantic()
            .export_cross_domain_event_set(self.trust_snapshot_digest)?)
    }

    fn import_remote_cross_domain_events(
        &self,
        remote_items: &[CrossDomainEventItem],
    ) -> Result<CrossDomainConvergenceReport, SemanticNetSyncError> {
        let guard = self.host.lock().unwrap_or_else(PoisonError::into_inner);
        let runtime = guard.runtime();
        for item in remote_items {
            verify_cross_domain_item_provenance(&runtime.identity, item)?;
        }
        Ok(runtime.semantic().import_cross_domain_event_set(
            &runtime.identity,
            runtime.semantic_writer(),
            self.trust_snapshot_digest,
            remote_items,
        )?)
    }
}

/// Metadata tracked per local 3-family lease so [`CellLeaseInspector`] projects
/// live holder TTL, skew horizon, and lock state (`Active` / `Expiring` /
/// `Quarantined`) directly from [`CellHost`]'s authoritative grantors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrackedCellLeaseEntry {
    /// Local `QuotaLease` entry.
    Quota {
        /// Lease identifier.
        lease_id: QuotaLeaseId,
        /// Holder identifier (`ProcessId` or `CellIdentity` bytes).
        holder_id: [u8; 16],
        /// Remaining holder TTL budget in milliseconds at inspection reference.
        nominal_remaining_ttl_ms: u64,
        /// Clock skew horizon (`2 * max_clock_skew_ms + safety_margin_ms`).
        clock_skew_horizon_ms: u64,
    },
    /// Local `CapacityLease` entry.
    Capacity {
        /// Lease identifier.
        lease_id: CapacityLeaseId,
        /// Holder identifier.
        holder_id: [u8; 16],
        /// Remaining holder TTL budget in milliseconds.
        nominal_remaining_ttl_ms: u64,
        /// Clock skew horizon (`2 * max_clock_skew_ms + safety_margin_ms`).
        clock_skew_horizon_ms: u64,
    },
    /// Local `ExclusiveDeviceLease` entry.
    ExclusiveDevice {
        /// Lease identifier.
        lease_id: ExclusiveDeviceLeaseId,
        /// Holder identifier.
        holder_id: [u8; 16],
        /// Remaining holder TTL budget in milliseconds.
        nominal_remaining_ttl_ms: u64,
        /// Clock skew horizon (`2 * max_clock_skew_ms + safety_margin_ms`).
        clock_skew_horizon_ms: u64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RecordedControlMigration {
    idempotency_key: [u8; 16],
    process_id: [u8; 16],
    destination_cell_id: [u8; 16],
    expected_generation: u64,
    receipt_id: ReceiptId,
}

#[derive(Default)]
struct InspectorTrackerState {
    cell_label: String,
    tracked_leases: BTreeMap<[u8; 16], TrackedCellLeaseEntry>,
    tracked_remote_horizons: Vec<(CrossHostLeaseFamily, [u8; 16])>,
    recorded_migrations: Vec<RecordedControlMigration>,
}

/// Shared read-only & controlled-migration inspector handle over a live
/// [`NetCellHost`], implementing [`CellTopologyInspector`] and
/// [`CellLeaseInspector`] (`IPC-D7`, Stage D `W72` `SystemControlCenter` seam).
#[derive(Clone)]
pub struct NetCellHostInspector {
    host: Arc<Mutex<CellHost>>,
    profile: ClusterProfile,
    skew_policy: ClockSkewPolicy,
    tracker: Arc<Mutex<InspectorTrackerState>>,
    remote_lease_server: Option<Arc<RemoteLeaseProtocolServer>>,
}

impl fmt::Debug for NetCellHostInspector {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NetCellHostInspector")
            .field("profile", &self.profile)
            .field("skew_policy", &self.skew_policy)
            .finish_non_exhaustive()
    }
}

impl NetCellHostInspector {
    /// Default clock skew horizon (`2 * max_clock_skew_ms + safety_margin_ms`).
    #[must_use]
    pub const fn default_clock_skew_horizon_ms(&self) -> u64 {
        self.skew_policy
            .max_clock_skew_ms
            .get()
            .saturating_mul(2)
            .saturating_add(self.skew_policy.safety_margin_ms.get())
    }

    /// Indexes a remote cross-host horizon `(family, lease_id)` in this inspector.
    pub fn track_remote_horizon(&self, family: CrossHostLeaseFamily, lease_id: [u8; 16]) {
        let mut tracker = self.tracker.lock().unwrap_or_else(PoisonError::into_inner);
        if !tracker
            .tracked_remote_horizons
            .contains(&(family, lease_id))
        {
            tracker.tracked_remote_horizons.push((family, lease_id));
        }
    }

    /// Indexes a local `QuotaLease` in this inspector's projection map.
    pub fn track_quota_lease(
        &self,
        lease_id: QuotaLeaseId,
        holder_id: [u8; 16],
        nominal_remaining_ttl_ms: u64,
    ) {
        let horizon = self.default_clock_skew_horizon_ms();
        self.tracker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .tracked_leases
            .insert(
                *lease_id.as_bytes(),
                TrackedCellLeaseEntry::Quota {
                    lease_id,
                    holder_id,
                    nominal_remaining_ttl_ms,
                    clock_skew_horizon_ms: horizon,
                },
            );
    }

    /// Indexes a local `CapacityLease` in this inspector's projection map.
    pub fn track_capacity_lease(
        &self,
        lease_id: CapacityLeaseId,
        holder_id: [u8; 16],
        nominal_remaining_ttl_ms: u64,
    ) {
        let horizon = self.default_clock_skew_horizon_ms();
        self.tracker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .tracked_leases
            .insert(
                *lease_id.as_bytes(),
                TrackedCellLeaseEntry::Capacity {
                    lease_id,
                    holder_id,
                    nominal_remaining_ttl_ms,
                    clock_skew_horizon_ms: horizon,
                },
            );
    }

    /// Indexes a local `ExclusiveDeviceLease` in this inspector's projection map.
    pub fn track_device_lease(
        &self,
        lease_id: ExclusiveDeviceLeaseId,
        holder_id: [u8; 16],
        nominal_remaining_ttl_ms: u64,
    ) {
        let horizon = self.default_clock_skew_horizon_ms();
        self.tracker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .tracked_leases
            .insert(
                *lease_id.as_bytes(),
                TrackedCellLeaseEntry::ExclusiveDevice {
                    lease_id,
                    holder_id,
                    nominal_remaining_ttl_ms,
                    clock_skew_horizon_ms: horizon,
                },
            );
    }

    fn collect_local_leases(
        &self,
        guard: &CellHost,
        local_cell_id: [u8; 16],
    ) -> Vec<CellLeaseRecord> {
        let mut records = BTreeMap::<[u8; 16], CellLeaseRecord>::new();
        let remote_keys = {
            let tracker = self.tracker.lock().unwrap_or_else(PoisonError::into_inner);
            for (&raw_id, entry) in &tracker.tracked_leases {
                if let Some(projected) = project_tracked_lease(guard, local_cell_id, *entry) {
                    records.insert(raw_id, projected);
                }
            }
            tracker.tracked_remote_horizons.clone()
        };

        if let Some(server) = &self.remote_lease_server
            && !remote_keys.is_empty()
        {
            let grantor_epoch = guard.fence().epoch().get();
            server.with_coordinator(|coord| {
                for (family, lease_id) in remote_keys {
                    let Some(horizon) = coord.horizon_record(family, lease_id) else {
                        continue;
                    };
                    let lock_state = match family {
                        CrossHostLeaseFamily::Quota => coord
                            .quota()
                            .query(QuotaLeaseId::from_bytes(lease_id))
                            .ok()
                            .map_or(CellLeaseLockState::Active, |g| match g.state() {
                                QuotaLeaseState::Quarantined | QuotaLeaseState::Fenced => {
                                    CellLeaseLockState::Quarantined
                                }
                                _ => CellLeaseLockState::Active,
                            }),
                        CrossHostLeaseFamily::Capacity => coord
                            .capacity()
                            .query(CapacityLeaseId::from_bytes(lease_id))
                            .ok()
                            .map_or(CellLeaseLockState::Active, |g| match g.state() {
                                CapacityLeaseState::Quarantined | CapacityLeaseState::Fenced => {
                                    CellLeaseLockState::Quarantined
                                }
                                _ => CellLeaseLockState::Active,
                            }),
                        CrossHostLeaseFamily::ExclusiveDevice => CellLeaseLockState::Active,
                    };
                    records.entry(lease_id).or_insert(CellLeaseRecord {
                        lease_id,
                        cell_id: *horizon.holder_cell.as_bytes(),
                        kind: match horizon.family {
                            CrossHostLeaseFamily::Quota => CellLeaseKind::Quota,
                            CrossHostLeaseFamily::Capacity => CellLeaseKind::Capacity,
                            CrossHostLeaseFamily::ExclusiveDevice => CellLeaseKind::ExclusiveDevice,
                        },
                        lock_state,
                        holder_id: *horizon.holder_cell.as_bytes(),
                        remaining_holder_ttl_ms: horizon.ttl_grant_ms,
                        lease_epoch: grantor_epoch,
                        clock_skew_horizon_ms: self.default_clock_skew_horizon_ms(),
                    });
                }
            });
        }

        records.into_values().collect()
    }

    fn build_topology_summaries(&self, guard: &CellHost) -> Vec<CellSummaryRecord> {
        let fence = guard.fence();
        let local_id = *fence.identity().as_bytes();
        let local_leases = self.collect_local_leases(guard, local_id);
        let any_quarantined = local_leases
            .iter()
            .any(|l| l.lock_state == CellLeaseLockState::Quarantined);
        let active_lease_count = u64::try_from(
            local_leases
                .iter()
                .filter(|l| l.lock_state != CellLeaseLockState::Quarantined)
                .count(),
        )
        .unwrap_or(u64::MAX);

        let link_state = guard.connectivity_state();
        let reachability = match (self.profile, link_state) {
            (ClusterProfile::SingleHostDesktop, _) => CellReachabilityState::OnlineLocal,
            (ClusterProfile::MultiHostCluster { .. }, ControlPlaneLinkState::Connected { .. }) => {
                CellReachabilityState::OnlineRemote
            }
            (
                ClusterProfile::MultiHostCluster { .. },
                ControlPlaneLinkState::Partitioned { .. }
                | ControlPlaneLinkState::Reconnecting { .. },
            ) => CellReachabilityState::Partitioned,
        };
        let partition_autonomy_gate = match link_state {
            ControlPlaneLinkState::Connected { .. } if any_quarantined => {
                PartitionAutonomyGateState::Quarantined
            }
            ControlPlaneLinkState::Connected { .. } => PartitionAutonomyGateState::FullAutonomy,
            ControlPlaneLinkState::Partitioned { .. } => {
                PartitionAutonomyGateState::LocalReadAndBoundedLease
            }
            ControlPlaneLinkState::Reconnecting { .. } => {
                PartitionAutonomyGateState::FencedReadOnly
            }
        };

        let label = self
            .tracker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .cell_label
            .clone();

        let mut by_id = BTreeMap::<[u8; 16], CellSummaryRecord>::new();
        by_id.insert(
            local_id,
            CellSummaryRecord {
                cell_id: local_id,
                cell_label: label,
                reachability,
                partition_autonomy_gate,
                current_epoch: fence.epoch().get(),
                active_lease_count,
                expected_generation_or_revision: fence.node_boot_generation().get(),
            },
        );

        if let Some(dir) = guard.cell_directory()
            && let Ok(snap) = dir.snapshot()
        {
            for entry in snap.into_entries() {
                let peer_id = *entry.identity().as_bytes();
                if peer_id == local_id {
                    continue;
                }
                let peer_reachability = if link_state.is_connected() {
                    CellReachabilityState::OnlineRemote
                } else {
                    CellReachabilityState::Partitioned
                };
                let peer_gate = if link_state.is_connected() {
                    PartitionAutonomyGateState::FullAutonomy
                } else {
                    PartitionAutonomyGateState::LocalReadAndBoundedLease
                };
                by_id.insert(
                    peer_id,
                    CellSummaryRecord {
                        cell_id: peer_id,
                        cell_label: format!("peer-cell-{}", crate::short_hex(&peer_id)),
                        reachability: peer_reachability,
                        partition_autonomy_gate: peer_gate,
                        current_epoch: entry.epoch().get(),
                        active_lease_count: 0,
                        expected_generation_or_revision: entry.node_boot_generation().get(),
                    },
                );
            }
        }

        by_id.into_values().collect()
    }
}

#[allow(clippy::too_many_lines)]
fn project_tracked_lease(
    host: &CellHost,
    cell_id: [u8; 16],
    entry: TrackedCellLeaseEntry,
) -> Option<CellLeaseRecord> {
    match entry {
        TrackedCellLeaseEntry::Quota {
            lease_id,
            holder_id,
            nominal_remaining_ttl_ms,
            clock_skew_horizon_ms,
        } => {
            let grant = host.query_quota_lease(lease_id).ok()?;
            let (lock_state, ttl_ms) = match grant.state() {
                QuotaLeaseState::Closed | QuotaLeaseState::Cancelled => return None,
                QuotaLeaseState::Quarantined | QuotaLeaseState::Fenced => {
                    (CellLeaseLockState::Quarantined, 0)
                }
                QuotaLeaseState::Issued | QuotaLeaseState::Active | QuotaLeaseState::Closing => {
                    let lock = if nominal_remaining_ttl_ms <= clock_skew_horizon_ms {
                        CellLeaseLockState::Expiring
                    } else {
                        CellLeaseLockState::Active
                    };
                    (lock, nominal_remaining_ttl_ms)
                }
            };
            Some(CellLeaseRecord {
                lease_id: *lease_id.as_bytes(),
                cell_id,
                kind: CellLeaseKind::Quota,
                lock_state,
                holder_id,
                remaining_holder_ttl_ms: ttl_ms,
                lease_epoch: grant.epoch().get(),
                clock_skew_horizon_ms,
            })
        }
        TrackedCellLeaseEntry::Capacity {
            lease_id,
            holder_id,
            nominal_remaining_ttl_ms,
            clock_skew_horizon_ms,
        } => {
            let grant = host.query_capacity_lease(lease_id).ok()?;
            let (lock_state, ttl_ms) = match grant.state() {
                CapacityLeaseState::Returned => return None,
                CapacityLeaseState::Quarantined | CapacityLeaseState::Fenced => {
                    (CellLeaseLockState::Quarantined, 0)
                }
                CapacityLeaseState::GlobalReserved
                | CapacityLeaseState::TargetPrepared
                | CapacityLeaseState::Active
                | CapacityLeaseState::Returning => {
                    let lock = if nominal_remaining_ttl_ms <= clock_skew_horizon_ms {
                        CellLeaseLockState::Expiring
                    } else {
                        CellLeaseLockState::Active
                    };
                    (lock, nominal_remaining_ttl_ms)
                }
            };
            Some(CellLeaseRecord {
                lease_id: *lease_id.as_bytes(),
                cell_id,
                kind: CellLeaseKind::Capacity,
                lock_state,
                holder_id,
                remaining_holder_ttl_ms: ttl_ms,
                lease_epoch: grant.capacity_epoch().get(),
                clock_skew_horizon_ms,
            })
        }
        TrackedCellLeaseEntry::ExclusiveDevice {
            lease_id,
            holder_id,
            nominal_remaining_ttl_ms,
            clock_skew_horizon_ms,
        } => {
            let grant = host.query_device_lease(lease_id).ok()?;
            let (lock_state, ttl_ms) = match grant.state() {
                ExclusiveDeviceLeaseState::Returned => return None,
                ExclusiveDeviceLeaseState::Quarantined | ExclusiveDeviceLeaseState::Fencing => {
                    (CellLeaseLockState::Quarantined, 0)
                }
                ExclusiveDeviceLeaseState::DeviceReserved
                | ExclusiveDeviceLeaseState::Resetting => {
                    let lock = if nominal_remaining_ttl_ms <= clock_skew_horizon_ms {
                        CellLeaseLockState::Expiring
                    } else {
                        CellLeaseLockState::Active
                    };
                    (lock, nominal_remaining_ttl_ms)
                }
            };
            Some(CellLeaseRecord {
                lease_id: *lease_id.as_bytes(),
                cell_id,
                kind: CellLeaseKind::ExclusiveDevice,
                lock_state,
                holder_id,
                remaining_holder_ttl_ms: ttl_ms,
                lease_epoch: grant.exclusivity_epoch().get(),
                clock_skew_horizon_ms,
            })
        }
    }
}

fn derive_migration_control_receipt_id(request: &ProcessMigrationControlRequest) -> ReceiptId {
    let mut hasher = Sha256::new();
    hasher.update(b"llmos/net-cell-host/process-migration-control/v1");
    hasher.update(request.process_id);
    hasher.update(request.destination_cell_id);
    hasher.update(request.expected_generation_or_revision.to_be_bytes());
    hasher.update(request.idempotency_key);
    let digest: [u8; 32] = hasher.finalize().into();
    let mut id = [0_u8; 16];
    id.copy_from_slice(&digest[..16]);
    ReceiptId::from_bytes(id)
}

impl CellTopologyInspector for NetCellHostInspector {
    fn list_cells(&self, cursor: Option<[u8; 16]>, limit: usize) -> Result<CellPage, SabiFailure> {
        let clamped = limit.clamp(1, MAX_SYSTEM_CONTROL_ENUM_LIMIT);
        let guard = self.host.lock().unwrap_or_else(PoisonError::into_inner);
        let summaries = self.build_topology_summaries(&guard);
        drop(guard);

        let filtered: Vec<CellSummaryRecord> = summaries
            .into_iter()
            .filter(|cell| cursor.is_none_or(|after| cell.cell_id > after))
            .collect();
        let truncated = filtered.len() > clamped;
        let cells: Vec<CellSummaryRecord> = filtered.into_iter().take(clamped).collect();
        let next_cursor = if truncated {
            cells.last().map(|last| last.cell_id)
        } else {
            None
        };
        Ok(CellPage {
            cells,
            next_cursor,
            truncated,
        })
    }

    #[allow(clippy::too_many_lines)]
    fn request_process_migration(
        &self,
        request: ProcessMigrationControlRequest,
    ) -> Result<ReceiptId, SabiFailure> {
        {
            let tracker = self.tracker.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(existing) = tracker
                .recorded_migrations
                .iter()
                .find(|m| m.idempotency_key == request.idempotency_key)
            {
                if existing.process_id == request.process_id
                    && existing.destination_cell_id == request.destination_cell_id
                    && existing.expected_generation == request.expected_generation_or_revision
                {
                    return Ok(existing.receipt_id);
                }
                return Err(SabiFailure {
                    code: SabiErrorCode::Conflict.into(),
                    retry: RetryDirective::DoNotRetry.into(),
                    safe_message: "idempotency key reused with divergent migration parameters"
                        .to_owned(),
                });
            }
        }

        if matches!(self.profile, ClusterProfile::SingleHostDesktop) {
            return Err(SabiFailure {
                code: SabiErrorCode::NotSupported.into(),
                retry: RetryDirective::DoNotRetry.into(),
                safe_message: "SingleHostDesktop (N=1) cell host has no remote migration peers"
                    .to_owned(),
            });
        }

        let guard = self.host.lock().unwrap_or_else(PoisonError::into_inner);
        let summaries = self.build_topology_summaries(&guard);
        let local_id = *guard.fence().identity().as_bytes();
        if request.destination_cell_id == local_id {
            return Err(SabiFailure {
                code: SabiErrorCode::InvalidArgument.into(),
                retry: RetryDirective::DoNotRetry.into(),
                safe_message: "destination_cell_id must be distinct from source cell".to_owned(),
            });
        }
        let Some(dest) = summaries
            .iter()
            .find(|c| c.cell_id == request.destination_cell_id)
        else {
            return Err(SabiFailure {
                code: SabiErrorCode::NotFound.into(),
                retry: RetryDirective::DoNotRetry.into(),
                safe_message: "destination cell is not registered in local CellDirectory"
                    .to_owned(),
            });
        };

        if matches!(
            dest.reachability,
            CellReachabilityState::Partitioned | CellReachabilityState::Offline
        ) || matches!(
            dest.partition_autonomy_gate,
            PartitionAutonomyGateState::FencedReadOnly | PartitionAutonomyGateState::Quarantined
        ) || !guard.connectivity_state().is_connected()
        {
            return Err(SabiFailure {
                code: SabiErrorCode::Fenced.into(),
                retry: RetryDirective::DoNotRetry.into(),
                safe_message: "partition autonomy gate forbids cross-cell migration while partitioned or fenced"
                    .to_owned(),
            });
        }

        let Some(gen_nz) = std::num::NonZeroU64::new(request.expected_generation_or_revision)
        else {
            return Err(SabiFailure {
                code: SabiErrorCode::InvalidArgument.into(),
                retry: RetryDirective::DoNotRetry.into(),
                safe_message: "expected_generation_or_revision must be non-zero".to_owned(),
            });
        };
        let target_cell = CellIdentity::from_domain(nlos_types::SchedulerDomainId::from_bytes(
            request.destination_cell_id,
        ));
        let intent = MigrationIntent::new(
            guard.fence().identity(),
            target_cell,
            MigrationObject::from_bytes(request.process_id),
            Generation::new(gen_nz),
            "system-control request_process_migration",
        )
        .map_err(|_| SabiFailure {
            code: SabiErrorCode::InvalidArgument.into(),
            retry: RetryDirective::DoNotRetry.into(),
            safe_message: "invalid MigrationIntent parameters".to_owned(),
        })?;
        guard
            .record_migration_intent(&intent)
            .map_err(|_| SabiFailure {
                code: SabiErrorCode::Durability.into(),
                retry: RetryDirective::RetrySameIdempotencyKey.into(),
                safe_message: "failed to persist MigrationIntent".to_owned(),
            })?;
        drop(guard);

        let receipt_id = derive_migration_control_receipt_id(&request);
        self.tracker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .recorded_migrations
            .push(RecordedControlMigration {
                idempotency_key: request.idempotency_key,
                process_id: request.process_id,
                destination_cell_id: request.destination_cell_id,
                expected_generation: request.expected_generation_or_revision,
                receipt_id,
            });
        Ok(receipt_id)
    }
}

impl CellLeaseInspector for NetCellHostInspector {
    fn inspect_cell_leases(
        &self,
        cell_id: Option<[u8; 16]>,
        limit: usize,
    ) -> Result<CellLeasePage, SabiFailure> {
        let clamped = limit.clamp(1, MAX_SYSTEM_CONTROL_ENUM_LIMIT);
        let guard = self.host.lock().unwrap_or_else(PoisonError::into_inner);
        let local_id = *guard.fence().identity().as_bytes();
        let resolved_cell_id = cell_id.unwrap_or(local_id);
        let summaries = self.build_topology_summaries(&guard);
        if !summaries.iter().any(|c| c.cell_id == resolved_cell_id) {
            return Err(SabiFailure {
                code: SabiErrorCode::NotFound.into(),
                retry: RetryDirective::DoNotRetry.into(),
                safe_message: "requested cell was not found".to_owned(),
            });
        }
        let mut all_leases = self.collect_local_leases(&guard, local_id);
        drop(guard);

        all_leases.retain(|lease| lease.cell_id == resolved_cell_id);
        all_leases.sort_by_key(|lease| lease.lease_id);
        let truncated = all_leases.len() > clamped;
        all_leases.truncate(clamped);
        Ok(CellLeasePage {
            cell_id: resolved_cell_id,
            leases: all_leases,
            truncated,
        })
    }
}

/// Multiplexed service handler mounted on [`NetCellHost`]'s
/// [`MtlsEndpointListener`], routing `NLOSSY01` (semantic sync),
/// `NLOSAT01`/`NLOSAB01` (chunked artifact transfer), `[LEASE-PROTO-001]`
/// remote lease envelopes, and registered sub-protocol RPCs (`NLOSMG01`
/// migration & `NLOSTB01` takeover barrier).
pub struct NetCellHostServiceDispatcher {
    host: Arc<Mutex<CellHost>>,
    semantic_server: SemanticNetSyncServer<NetCellSemanticSyncBridge>,
    artifact_server: Arc<ArtifactNetTransferServer>,
    remote_lease_server: Option<Arc<RemoteLeaseProtocolServer>>,
    sub_handlers: Mutex<Vec<Arc<dyn SubProtocolRpcHandler>>>,
    network_cut_active: Arc<AtomicBool>,
}

impl fmt::Debug for NetCellHostServiceDispatcher {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NetCellHostServiceDispatcher")
            .field(
                "network_cut_active",
                &self.network_cut_active.load(Ordering::Acquire),
            )
            .finish_non_exhaustive()
    }
}

impl NetCellHostServiceDispatcher {
    fn register_sub_handler(&self, handler: Arc<dyn SubProtocolRpcHandler>) {
        self.sub_handlers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(handler);
    }

    fn current_epoch(&self) -> CellEpoch {
        self.host
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .fence()
            .epoch()
    }

    fn match_sub_handler(&self, payload: &[u8]) -> Option<Arc<dyn SubProtocolRpcHandler>> {
        let extra = self
            .sub_handlers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        extra.iter().find(|h| h.matches_frame(payload)).cloned()
    }

    fn dispatch_rpc_sync_builtin(&self, request: &IncomingRpcRequest) -> RpcHandlerReply {
        let observed_epoch = self.current_epoch();
        if SemanticNetSyncServer::<NetCellSemanticSyncBridge>::is_semantic_sync_frame(
            &request.payload,
        ) {
            return self.semantic_server.handle_incoming_rpc(request);
        }

        if ArtifactNetTransferServer::is_artifact_rpc_frame(&request.payload) {
            return self.artifact_server.handle_incoming_rpc(request);
        }

        if request.payload.len() >= 2
            && request.payload[..2] == LEASE_PROTO_SCHEMA_VERSION.to_be_bytes()
            && let Some(lease_srv) = &self.remote_lease_server
        {
            return lease_srv.process_incoming_sync(request);
        }

        RpcHandlerReply {
            observed_epoch,
            idempotency_replayed: false,
            status: RpcWireStatus::PreExecutionUnavailable,
            payload: b"net-cell-host: unrecognized RPC wire magic".to_vec(),
        }
    }
}

impl RpcRequestHandler for NetCellHostServiceDispatcher {
    fn handle_rpc(
        &self,
        request: IncomingRpcRequest,
    ) -> Pin<Box<dyn Future<Output = RpcHandlerReply> + Send + 'static>> {
        let observed_epoch = self.current_epoch();
        if self.network_cut_active.load(Ordering::Acquire) {
            return Box::pin(async move {
                RpcHandlerReply {
                    observed_epoch,
                    idempotency_replayed: false,
                    status: RpcWireStatus::PreExecutionUnavailable,
                    payload: b"net-cell-host: simulated network link cut".to_vec(),
                }
            });
        }
        if let Some(sub) = self.match_sub_handler(&request.payload) {
            return sub.handle_frame(request);
        }
        let reply = self.dispatch_rpc_sync_builtin(&request);
        Box::pin(async move { reply })
    }
}

impl BulkStreamHandler for NetCellHostServiceDispatcher {
    fn handle_bulk_stream(
        &self,
        meta: IncomingBulkMeta,
        reader: BulkStreamReader,
    ) -> Pin<Box<dyn Future<Output = Result<BulkHandlerCompletion, &'static str>> + Send + 'static>>
    {
        if self.network_cut_active.load(Ordering::Acquire) {
            return Box::pin(async move { Err("net-cell-host: simulated network link cut") });
        }
        let artifact_server = Arc::clone(&self.artifact_server);
        Box::pin(async move { artifact_server.handle_incoming_bulk(&meta, reader).await })
    }
}

/// Initialization parameters for binding a multi-host networked [`NetCellHost`].
pub struct NetCellHostNetworkConfig {
    /// Local [`CellHostConfig`].
    pub cell_config: CellHostConfig,
    /// Human-readable label projected by [`CellTopologyInspector`].
    pub cell_label: String,
    /// Bind address hint for the host service mTLS listener (e.g. `127.0.0.1:0`).
    pub service_bind_addr: SocketAddr,
    /// TLS 1.3 + 5-axis `CellFence` security dependencies for the service listener.
    pub listener_security: NodeSecurityDeps,
    /// TLS 1.3 + 5-axis `CellFence` security dependencies for outbound client dials.
    pub client_security: NodeSecurityDeps,
    /// Transport timeouts and multiplexing configuration.
    pub transport_config: NetTransportConfig,
    /// Clock skew horizon policy projected to [`CellLeaseInspector`].
    pub skew_policy: ClockSkewPolicy,
    /// Non-zero `trust_snapshot_digest` used for semantic checkpoint/event sync.
    pub trust_snapshot_digest: [u8; 32],
    /// Pre-registered `[DIST-ART-001]` artifact sync policies on this host.
    pub artifact_policies: Vec<ArtifactSyncPolicy>,
    /// Optional `[LEASE-PROTO-001]` [`RemoteLeaseProtocolServer`].
    pub remote_lease_server: Option<Arc<RemoteLeaseProtocolServer>>,
    /// Control-plane consensus backend (`SingleHostPassthrough` or `MultiHostQuorum`).
    pub consensus: NetConsensusBackend,
}

impl fmt::Debug for NetCellHostNetworkConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NetCellHostNetworkConfig")
            .field("cell_config", &self.cell_config)
            .field("cell_label", &self.cell_label)
            .field("service_bind_addr", &self.service_bind_addr)
            .field("skew_policy", &self.skew_policy)
            .finish_non_exhaustive()
    }
}

struct ActiveNetworkRuntime {
    service_addr: SocketAddr,
    resolver: Arc<MonotonicEndpointResolver>,
    dispatcher: Arc<NetCellHostServiceDispatcher>,
    semantic_bridge: Arc<NetCellSemanticSyncBridge>,
    artifact_server: Arc<ArtifactNetTransferServer>,
    migration_server: Arc<crate::net_migration_drive::NetMigrationRpcServer>,
    takeover_server: Arc<crate::net_migration_drive::NetTakeoverRpcServer>,
    listener_slot: tokio::sync::Mutex<Option<RunningMtlsListener>>,
}

/// Networked `CellHost` assembly (`ADR-0026`, `W71-C-SYNC-MIGRATE`).
///
/// Wraps one isolated local [`CellHost`] (`<root>`, zero shared filesystem
/// paths across hosts) with `nlos-net` mTLS transport, `nlos-consensus`,
/// semantic delta sync, resumable chunked artifact streaming, and Stage D
/// [`CellTopologyInspector`] / [`CellLeaseInspector`] projections.
pub struct NetCellHost {
    root: PathBuf,
    host: Arc<Mutex<CellHost>>,
    consensus: NetConsensusBackend,
    service_accounting: SocketActivityAccounting,
    transport: Arc<MtlsCellTransport>,
    inspector: NetCellHostInspector,
    network_cut_active: Arc<AtomicBool>,
    network_runtime: Option<ActiveNetworkRuntime>,
}

impl fmt::Debug for NetCellHost {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("NetCellHost")
            .field("root", &self.root)
            .field("profile", &self.cluster_profile())
            .field("service_addr", &self.service_addr())
            .finish_non_exhaustive()
    }
}

impl NetCellHost {
    /// Opens an `N = 1` `SingleHostDesktop` [`NetCellHost`] rooted at `root`.
    ///
    /// Enforces `ADR-0026` §6.3 (`[NLOS-GOAL-001]`):
    /// - **0** bound TCP/UDP sockets (`active_socket_count() == 0`)
    /// - **0** background consensus/network timers (`active_background_timer_count() == 0`)
    /// - **0** TLS certificates or handshakes (`requires_tls_certificates() == false`)
    ///
    /// # Errors
    ///
    /// Fails closed if `CellHost` or `SingleHostPassthroughNode` fails to open.
    pub fn open_single_host_desktop(
        root: impl AsRef<Path>,
        cell_config: CellHostConfig,
        control_domain: ControlDomainId,
        skew_policy: ClockSkewPolicy,
    ) -> Result<Self, NetCellHostError> {
        let root_path = root.as_ref().to_path_buf();
        let private_fed_dir = root_path.join(PRIVATE_FEDERATION_MIRROR_DIR);
        let cell_host =
            CellHost::open_federated_durable(&root_path, cell_config, &private_fed_dir)?;
        let local_cell = cell_host.fence().identity();
        let consensus_node = SingleHostPassthroughNode::open(
            root_path.join(PRIVATE_CONSENSUS_DIR),
            local_cell,
            control_domain,
        )?;
        let service_accounting = SocketActivityAccounting::new();
        let transport = Arc::new(MtlsCellTransport::new_single_host_desktop(
            service_accounting.clone(),
        ));
        let host = Arc::new(Mutex::new(cell_host));
        let tracker = Arc::new(Mutex::new(InspectorTrackerState {
            cell_label: "local-desktop-cell".to_owned(),
            tracked_leases: BTreeMap::new(),
            recorded_migrations: Vec::new(),
            tracked_remote_horizons: Vec::new(),
        }));
        let inspector = NetCellHostInspector {
            host: Arc::clone(&host),
            profile: ClusterProfile::SingleHostDesktop,
            skew_policy,
            tracker,
            remote_lease_server: None,
        };
        Ok(Self {
            root: root_path,
            host,
            consensus: NetConsensusBackend::SingleHostPassthrough(Box::new(consensus_node)),
            service_accounting,
            transport,
            inspector,
            network_cut_active: Arc::new(AtomicBool::new(false)),
            network_runtime: None,
        })
    }

    /// Opens an isolated [`CellHost`] at `root` (with a strictly private
    /// `<root>/local-cell-directory` mirror — zero shared filesystem paths) and
    /// binds its multiplexed TLS 1.3 mTLS service listener + client transport.
    ///
    /// # Errors
    ///
    /// Fails closed if `CellHost`, `ArtifactStore`, `MtlsEndpointListener`, or
    /// `MtlsCellTransport` initialization fails.
    #[allow(clippy::too_many_lines)]
    pub async fn bind_networked(
        root: impl AsRef<Path>,
        config: NetCellHostNetworkConfig,
    ) -> Result<Self, NetCellHostError> {
        let root_path = root.as_ref().to_path_buf();
        let private_fed_dir = root_path.join(PRIVATE_FEDERATION_MIRROR_DIR);
        let cell_host =
            CellHost::open_federated_durable(&root_path, config.cell_config, &private_fed_dir)?;
        let local_cell = cell_host.fence().identity();
        let host_wall_ms = cell_host
            .runtime()
            .clock
            .wall_now(nlos_clock::NowRequest {
                idempotency_key: nlos_types::IdempotencyKey::from_bytes(*b"netcell-sem-sync"),
            })
            .map_err(SliceKError::from)?
            .reading()
            .as_u64();
        let initial_retention = cell_host.artifact_sync_retention().clone();
        let host = Arc::new(Mutex::new(cell_host));

        let shared_artifact_store = Arc::new(ArtifactStore::open(root_path.join("artifacts"))?);
        let mut policy_reg = ArtifactSyncPolicyRegistry::new();
        for policy in config.artifact_policies {
            let _ = policy_reg.register(policy);
        }
        let policy_registry = Arc::new(Mutex::new(policy_reg));
        let retention_set = Arc::new(Mutex::new(initial_retention));

        let host_for_epoch = Arc::clone(&host);
        let epoch_source: Arc<dyn Fn() -> CellEpoch + Send + Sync> = Arc::new(move || {
            host_for_epoch
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .fence()
                .epoch()
        });

        let artifact_server = Arc::new(ArtifactNetTransferServer::with_shared_state(
            local_cell,
            epoch_source,
            shared_artifact_store,
            policy_registry,
            retention_set,
        ));
        let semantic_bridge = Arc::new(
            NetCellSemanticSyncBridge::new(Arc::clone(&host), config.trust_snapshot_digest)
                .with_min_signed_at_ms(host_wall_ms),
        );
        let semantic_server =
            SemanticNetSyncServer::new(Arc::clone(&semantic_bridge), host_wall_ms);
        let network_cut_active = Arc::new(AtomicBool::new(false));

        let dispatcher = Arc::new(NetCellHostServiceDispatcher {
            host: Arc::clone(&host),
            semantic_server,
            artifact_server: Arc::clone(&artifact_server),
            remote_lease_server: config.remote_lease_server.clone(),
            sub_handlers: Mutex::new(Vec::new()),
            network_cut_active: Arc::clone(&network_cut_active),
        });

        let bind_profile = ClusterProfile::MultiHostCluster {
            local_cell,
            bind_hint: config.service_bind_addr,
        };
        let service_accounting = SocketActivityAccounting::new();
        let rpc_dyn: Arc<dyn RpcRequestHandler> = Arc::clone(&dispatcher) as _;
        let bulk_dyn: Arc<dyn BulkStreamHandler> = Arc::clone(&dispatcher) as _;
        let listener = MtlsEndpointListener::bind(
            bind_profile,
            config.listener_security,
            config.transport_config,
            service_accounting.clone(),
            Some(Arc::clone(&rpc_dyn)),
            Some(Arc::clone(&bulk_dyn)),
        )
        .await?;
        let service_addr = listener.local_addr();
        let running_listener = listener.spawn_accept_loop();

        let effective_profile = ClusterProfile::MultiHostCluster {
            local_cell,
            bind_hint: service_addr,
        };
        let resolver = Arc::new(MonotonicEndpointResolver::new());
        let transport = Arc::new(MtlsCellTransport::new_multi_host(
            effective_profile,
            Arc::clone(&resolver) as _,
            config.client_security,
            config.transport_config,
            service_accounting.clone(),
            Some(rpc_dyn),
            Some(bulk_dyn),
        )?);

        let tracker = Arc::new(Mutex::new(InspectorTrackerState {
            cell_label: config.cell_label,
            tracked_leases: BTreeMap::new(),
            recorded_migrations: Vec::new(),
            tracked_remote_horizons: Vec::new(),
        }));
        let inspector = NetCellHostInspector {
            host: Arc::clone(&host),
            profile: effective_profile,
            skew_policy: config.skew_policy,
            tracker,
            remote_lease_server: config.remote_lease_server,
        };

        let (migration_server, takeover_server) =
            crate::net_migration_drive::build_default_net_host_sub_handlers(
                Arc::clone(&host),
                Arc::clone(&transport),
                Arc::clone(&resolver),
                Arc::clone(&semantic_bridge),
                inspector.clone(),
                &config.consensus,
            );
        dispatcher.register_sub_handler(Arc::clone(&migration_server) as _);
        dispatcher.register_sub_handler(Arc::clone(&takeover_server) as _);

        let host_out = Self {
            root: root_path,
            host,
            consensus: config.consensus,
            service_accounting,
            transport,
            inspector,
            network_cut_active,
            network_runtime: Some(ActiveNetworkRuntime {
                service_addr,
                resolver,
                dispatcher,
                semantic_bridge,
                artifact_server,
                migration_server,
                takeover_server,
                listener_slot: tokio::sync::Mutex::new(Some(running_listener)),
            }),
        };

        Ok(host_out)
    }

    /// Isolated filesystem root owned exclusively by this [`NetCellHost`].
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Nominal [`CellIdentity`] of this host.
    #[must_use]
    pub fn local_cell(&self) -> CellIdentity {
        self.with_cell_host(|h| h.fence().identity())
    }

    /// Current authoritative [`CellFence`] of this host.
    #[must_use]
    pub fn current_fence(&self) -> CellFence {
        self.with_cell_host(CellHost::fence)
    }

    /// Active [`ClusterProfile`] (`SingleHostDesktop` vs `MultiHostCluster`).
    #[must_use]
    pub fn cluster_profile(&self) -> ClusterProfile {
        match &self.network_runtime {
            Some(rt) => ClusterProfile::MultiHostCluster {
                local_cell: self.local_cell(),
                bind_hint: rt.service_addr,
            },
            None => ClusterProfile::SingleHostDesktop,
        }
    }

    /// Ephemeral bound mTLS service `SocketAddr`, or `None` in `N = 1` desktop mode.
    #[must_use]
    pub fn service_addr(&self) -> Option<SocketAddr> {
        self.network_runtime.as_ref().map(|rt| rt.service_addr)
    }

    /// Total active listener + connection sockets across service transport and
    /// consensus backend (`0` in `N = 1` `SingleHostDesktop`).
    #[must_use]
    pub fn active_socket_count(&self) -> usize {
        let svc = self
            .service_accounting
            .bound_listener_count()
            .saturating_add(self.service_accounting.active_connection_count());
        svc.saturating_add(self.consensus.active_socket_count())
    }

    /// Active background timer count (`0` in `N = 1` `SingleHostDesktop`).
    #[must_use]
    pub const fn active_background_timer_count(&self) -> usize {
        let net_timers = if self.network_runtime.is_some() { 1 } else { 0 };
        net_timers + self.consensus.active_background_timer_count()
    }

    /// Whether TLS X.509 certificates are required (`false` in `N = 1`).
    #[must_use]
    pub const fn requires_tls_certificates(&self) -> bool {
        self.network_runtime.is_some() || self.consensus.requires_tls_certificates()
    }

    /// Shared handle to the wrapped [`CellHost`].
    #[must_use]
    pub const fn cell_host_arc(&self) -> &Arc<Mutex<CellHost>> {
        &self.host
    }

    /// Executes `f` under locked `&CellHost` access.
    pub fn with_cell_host<R>(&self, f: impl FnOnce(&CellHost) -> R) -> R {
        let guard = self.host.lock().unwrap_or_else(PoisonError::into_inner);
        f(&guard)
    }

    /// Executes `f` under locked `&mut CellHost` access.
    pub fn with_cell_host_mut<R>(&self, f: impl FnOnce(&mut CellHost) -> R) -> R {
        let mut guard = self.host.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut guard)
    }

    /// Control-plane consensus backend (`SingleHostPassthrough` or `MultiHostQuorum`).
    #[must_use]
    pub const fn consensus(&self) -> &NetConsensusBackend {
        &self.consensus
    }

    /// Outbound/multiplexed `nlos-net` [`MtlsCellTransport`].
    #[must_use]
    pub const fn transport(&self) -> &Arc<MtlsCellTransport> {
        &self.transport
    }

    /// Cloneable Stage D [`CellTopologyInspector`] + [`CellLeaseInspector`] handle.
    #[must_use]
    pub fn inspector(&self) -> NetCellHostInspector {
        self.inspector.clone()
    }

    /// Semantic sync provider bridge when running in networked mode.
    #[must_use]
    pub fn semantic_sync_bridge(&self) -> Option<&Arc<NetCellSemanticSyncBridge>> {
        self.network_runtime.as_ref().map(|rt| &rt.semantic_bridge)
    }

    /// Chunked artifact transfer server when running in networked mode.
    #[must_use]
    pub fn artifact_transfer_server(&self) -> Option<&Arc<ArtifactNetTransferServer>> {
        self.network_runtime.as_ref().map(|rt| &rt.artifact_server)
    }

    /// Cross-socket `NLOSMG01` process migration RPC server when running in networked mode.
    #[must_use]
    pub fn migration_rpc_server(
        &self,
    ) -> Option<&Arc<crate::net_migration_drive::NetMigrationRpcServer>> {
        self.network_runtime.as_ref().map(|rt| &rt.migration_server)
    }

    /// Cross-socket `NLOSTB01` quorum takeover & barrier RPC server when running in networked mode.
    #[must_use]
    pub fn takeover_rpc_server(
        &self,
    ) -> Option<&Arc<crate::net_migration_drive::NetTakeoverRpcServer>> {
        self.network_runtime.as_ref().map(|rt| &rt.takeover_server)
    }

    /// Registers an `[DIST-ART-001]` [`ArtifactSyncPolicy`] on this host's
    /// [`ArtifactNetTransferServer`].
    ///
    /// # Errors
    ///
    /// Returns [`NetCellHostError::NotNetworked`] in `N = 1` mode.
    pub fn register_artifact_sync_policy(
        &self,
        policy: ArtifactSyncPolicy,
    ) -> Result<(), NetCellHostError> {
        let Some(rt) = &self.network_runtime else {
            return Err(NetCellHostError::NotNetworked(
                "register_artifact_sync_policy",
            ));
        };
        let _ = rt
            .artifact_server
            .policy_registry()
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .register(policy);
        Ok(())
    }

    /// Registers an additional sub-protocol RPC handler on the service listener.
    pub fn register_sub_protocol_handler(&self, handler: Arc<dyn SubProtocolRpcHandler>) {
        if let Some(rt) = &self.network_runtime {
            rt.dispatcher.register_sub_handler(handler);
        }
    }

    /// Upserts a monotonic ephemeral `SocketAddr` routing hint for `peer_cell`
    /// (`[DIST-NAME-001]`).
    ///
    /// # Errors
    ///
    /// Fails closed on `SingleHostDesktop` or stale routing version.
    pub fn upsert_peer_endpoint_hint(
        &self,
        peer_cell: CellIdentity,
        addr: SocketAddr,
        version: EndpointRoutingVersion,
    ) -> Result<EndpointUpdateDecision, NetCellHostError> {
        let Some(rt) = &self.network_runtime else {
            return Err(NetCellHostError::NotNetworked("upsert_peer_endpoint_hint"));
        };
        Ok(rt
            .resolver
            .upsert_hint(EndpointHintRecord::new(peer_cell, addr, version, 1))?)
    }

    /// Mirrors a verified peer [`CellRegistrationEntry`] into this host's own
    /// private `<root>/local-cell-directory` (`CellDirectory`), preserving zero
    /// shared filesystem paths across hosts.
    ///
    /// # Errors
    ///
    /// Fails closed if publishing into the local `CellDirectory` fails.
    pub fn record_peer_registration_locally(
        &self,
        entry: &CellRegistrationEntry,
    ) -> Result<CellRegistrationEntry, NetCellHostError> {
        self.with_cell_host(|h| {
            let dir = h.cell_directory().ok_or(SliceKError::CellHost(
                "NetCellHost local CellDirectory missing",
            ))?;
            Ok(dir.publish_registration(entry).map_err(SliceKError::from)?)
        })
    }

    /// Exports this host's current [`CellRegistrationEntry`] refreshed from its
    /// authoritative [`CellFence`].
    ///
    /// # Errors
    ///
    /// Propagates any local `CellDirectory` error.
    pub fn export_local_registration(&self) -> Result<CellRegistrationEntry, NetCellHostError> {
        self.with_cell_host_mut(|h| {
            let refreshed = h.refresh_cell_registration()?.ok_or(SliceKError::CellHost(
                "NetCellHost local CellDirectory missing",
            ))?;
            Ok(refreshed)
        })
    }

    /// Exports this host's semantic writer public-key [`BootstrapPrincipalRequest`].
    ///
    /// # Errors
    ///
    /// Propagates any identity inspection error.
    pub fn export_semantic_writer_principal(
        &self,
    ) -> Result<BootstrapPrincipalRequest, NetCellHostError> {
        Ok(self.with_cell_host(CellHost::semantic_writer_bootstrap_request)?)
    }

    /// Registers a peer public-key [`BootstrapPrincipalRequest`] in this host's
    /// local `IdentityAuthority`.
    ///
    /// # Errors
    ///
    /// Propagates any `IdentityAuthority` bootstrap failure.
    pub fn register_peer_principal(
        &self,
        request: BootstrapPrincipalRequest,
    ) -> Result<(), NetCellHostError> {
        self.with_cell_host(|h| {
            let _ = h
                .runtime()
                .identity
                .bootstrap_principal(request)
                .map_err(SliceKError::from)?;
            Ok(())
        })
    }

    /// Registers a local `QuotaLease` in the host's live [`CellLeaseInspector`]
    /// projection index.
    pub fn track_local_quota_lease(
        &self,
        lease_id: QuotaLeaseId,
        holder_id: [u8; 16],
        nominal_remaining_ttl_ms: u64,
    ) {
        let horizon = self.inspector.default_clock_skew_horizon_ms();
        self.inspector
            .tracker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .tracked_leases
            .insert(
                *lease_id.as_bytes(),
                TrackedCellLeaseEntry::Quota {
                    lease_id,
                    holder_id,
                    nominal_remaining_ttl_ms,
                    clock_skew_horizon_ms: horizon,
                },
            );
    }

    /// Registers a local `CapacityLease` in the host's live [`CellLeaseInspector`]
    /// projection index.
    pub fn track_local_capacity_lease(
        &self,
        lease_id: CapacityLeaseId,
        holder_id: [u8; 16],
        nominal_remaining_ttl_ms: u64,
    ) {
        let horizon = self.inspector.default_clock_skew_horizon_ms();
        self.inspector
            .tracker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .tracked_leases
            .insert(
                *lease_id.as_bytes(),
                TrackedCellLeaseEntry::Capacity {
                    lease_id,
                    holder_id,
                    nominal_remaining_ttl_ms,
                    clock_skew_horizon_ms: horizon,
                },
            );
    }

    /// Registers a local `ExclusiveDeviceLease` in the host's live
    /// [`CellLeaseInspector`] projection index.
    pub fn track_local_device_lease(
        &self,
        lease_id: ExclusiveDeviceLeaseId,
        holder_id: [u8; 16],
        nominal_remaining_ttl_ms: u64,
    ) {
        let horizon = self.inspector.default_clock_skew_horizon_ms();
        self.inspector
            .tracker
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .tracked_leases
            .insert(
                *lease_id.as_bytes(),
                TrackedCellLeaseEntry::ExclusiveDevice {
                    lease_id,
                    holder_id,
                    nominal_remaining_ttl_ms,
                    clock_skew_horizon_ms: horizon,
                },
            );
    }

    /// Convenience helper granting (and optionally activating) a local
    /// `QuotaLease` on [`CellHost`] while indexing it for [`CellLeaseInspector`].
    ///
    /// # Errors
    ///
    /// Propagates any [`CellHost`] quota grant/activation error.
    pub fn grant_and_track_quota_lease(
        &self,
        lease_id: QuotaLeaseId,
        face_value: u64,
        expires_at: Option<LeaseInstant>,
        holder_process: ProcessId,
        nominal_remaining_ttl_ms: u64,
        activate: bool,
    ) -> Result<QuotaLeaseGrant, NetCellHostError> {
        let grant = self.with_cell_host_mut(|h| {
            let issued = h.grant_quota_lease(lease_id, face_value, expires_at)?;
            if activate {
                h.activate_quota_lease(lease_id)
            } else {
                Ok(issued)
            }
        })?;
        self.track_local_quota_lease(
            lease_id,
            *holder_process.as_bytes(),
            nominal_remaining_ttl_ms,
        );
        Ok(grant)
    }

    /// Convenience helper granting a local `CapacityLease` on [`CellHost`] while
    /// indexing it for [`CellLeaseInspector`].
    ///
    /// # Errors
    ///
    /// Propagates any [`CellHost`] capacity grant error.
    pub fn grant_and_track_capacity_lease(
        &self,
        lease_id: CapacityLeaseId,
        amount: u64,
        holder_process: ProcessId,
        nominal_remaining_ttl_ms: u64,
    ) -> Result<CapacityLeaseGrant, NetCellHostError> {
        let grant = self.with_cell_host_mut(|h| h.grant_capacity_lease(lease_id, amount))?;
        self.track_local_capacity_lease(
            lease_id,
            *holder_process.as_bytes(),
            nominal_remaining_ttl_ms,
        );
        Ok(grant)
    }

    /// Convenience helper granting a local `ExclusiveDeviceLease` on [`CellHost`]
    /// while indexing it for [`CellLeaseInspector`].
    ///
    /// # Errors
    ///
    /// Propagates any [`CellHost`] device grant error.
    pub fn grant_and_track_device_lease(
        &self,
        lease_id: ExclusiveDeviceLeaseId,
        holder_process: ProcessId,
        nominal_remaining_ttl_ms: u64,
    ) -> Result<ExclusiveDeviceLeaseGrant, NetCellHostError> {
        let grant = self.with_cell_host_mut(|h| h.grant_device_lease(lease_id))?;
        self.track_local_device_lease(
            lease_id,
            *holder_process.as_bytes(),
            nominal_remaining_ttl_ms,
        );
        Ok(grant)
    }

    /// Toggles simulated network-link severance on this host's service listener,
    /// closing accepted sessions when `cut == true` so post-`SourceFenced`
    /// network-cut safety can be deterministically verified.
    pub async fn set_network_cut(&self, cut: bool) {
        self.network_cut_active.store(cut, Ordering::Release);
        if cut
            && let Some(rt) = &self.network_runtime
            && let Some(listener) = rt.listener_slot.lock().await.as_ref()
        {
            for session in listener.accepted_sessions() {
                session.close();
            }
        }
    }

    /// Gracefully shuts down the service mTLS listener and any `MultiHostQuorumNode`
    /// workers before dropping `SQLite` database handles.
    pub async fn shutdown(self) {
        if let Some(rt) = self.network_runtime
            && let Some(running) = rt.listener_slot.lock().await.take()
        {
            running.shutdown().await;
        }
        if let NetConsensusBackend::MultiHostQuorum(node) = &self.consensus {
            node.shutdown().await;
        }
    }
}

impl CellTopologyInspector for NetCellHost {
    fn list_cells(&self, cursor: Option<[u8; 16]>, limit: usize) -> Result<CellPage, SabiFailure> {
        self.inspector.list_cells(cursor, limit)
    }

    #[allow(clippy::too_many_lines)]
    fn request_process_migration(
        &self,
        request: ProcessMigrationControlRequest,
    ) -> Result<ReceiptId, SabiFailure> {
        self.inspector.request_process_migration(request)
    }
}

impl CellLeaseInspector for NetCellHost {
    fn inspect_cell_leases(
        &self,
        cell_id: Option<[u8; 16]>,
        limit: usize,
    ) -> Result<CellLeasePage, SabiFailure> {
        self.inspector.inspect_cell_leases(cell_id, limit)
    }
}

/// Helper wrapper encoding an application error string into a non-Ok
/// [`RpcHandlerReply`] when needed by sub-protocol handlers.
#[must_use]
pub fn invalid_sub_rpc_reply(observed_epoch: CellEpoch, reason: &str) -> RpcHandlerReply {
    RpcHandlerReply {
        observed_epoch,
        idempotency_replayed: false,
        status: RpcWireStatus::PreExecutionUnavailable,
        payload: reason.as_bytes().to_vec(),
    }
}
