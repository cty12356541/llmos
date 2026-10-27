//! Zero-configuration, zero-socket `N = 1` single-host passthrough mode for
//! `ClusterProfile::SingleHostDesktop` (`ADR-0026` §6.3, `[NLOS-GOAL-001]`).
//!
//! # Single-Host Desktop Guarantees
//!
//! - **0 TCP/UDP sockets** (`active_socket_count() == 0`).
//! - **0 background election or heartbeat timers**
//!   (`active_background_timer_count() == 0`).
//! - **0 TLS certificates or mTLS handshakes**
//!   (`requires_tls_certificates() == false`).
//! - **1 local `SQLite WAL + synchronous=FULL` transaction per proposal**,
//!   producing an authentic [`ConsensusCommitReceipt`] with monotonic
//!   `consensus_commit_index: NonZeroU64` and deterministic
//!   `ControlPlaneStateMachine` root digest.

use std::path::Path;
use std::sync::{Mutex, PoisonError};

use nlos_cell::CellIdentity;
use nlos_net::{ClusterProfile, SocketActivityAccounting};
use nlos_types::ControlDomainId;

use crate::ConsensusError;
use crate::core::{
    ConsensusCore, ConsensusInput, ConsensusRole, ConsensusTerm, ConsensusTimingConfig, LogIndex,
    SnapshotMetadata, ValidatedVoterRoster, VoterReplica,
};
use crate::state_machine::{ControlPlaneProposal, ControlPlaneStateMachine};
use crate::store::{ConsensusCommitReceipt, ConsensusStoreError, SqliteConsensusStore};

#[derive(Debug)]
struct SingleHostInner {
    store: SqliteConsensusStore,
    core: ConsensusCore,
    state_machine: ControlPlaneStateMachine,
    latest_durable_snapshot: Option<(SnapshotMetadata, Vec<u8>)>,
}

/// Zero-overhead `N = 1` control-plane consensus passthrough engine
/// (`ClusterProfile::SingleHostDesktop`, `ADR-0026` §6.3).
#[derive(Debug)]
pub struct SingleHostPassthroughNode {
    local_cell: CellIdentity,
    local_domain: ControlDomainId,
    socket_accounting: SocketActivityAccounting,
    inner: Mutex<SingleHostInner>,
}

impl SingleHostPassthroughNode {
    /// Opens or recovers an `N = 1` `SingleHostDesktop` passthrough node backed
    /// exclusively by a local `SQLite WAL + synchronous=FULL` directory.
    ///
    /// Binds **0** network sockets, spawns **0** background timers, and
    /// requires **0** TLS certificates.
    ///
    /// # Errors
    ///
    /// Fails closed if `SQLite WAL + FULL` durability is unavailable or if
    /// existing on-disk state fails `[DUR-FAILCLOSED-001]` integrity checks.
    pub fn open(
        store_dir: impl AsRef<Path>,
        local_cell: CellIdentity,
        local_domain: ControlDomainId,
    ) -> Result<Self, ConsensusError> {
        let roster = ValidatedVoterRoster::new(vec![VoterReplica {
            cell_identity: local_cell,
            control_domain_id: local_domain,
        }])?;
        let (mut store, mut recovered) = SqliteConsensusStore::open_or_recover(store_dir)?;

        if recovered.core_state.current_term == 0 {
            store.persist_hard_state(1, Some(local_cell))?;
            recovered.core_state.current_term = 1;
            recovered.core_state.voted_for = Some(local_cell);
        }

        let latest_durable_snapshot = recovered.core_state.snapshot.clone();
        let core = ConsensusCore::rehydrate(
            local_cell,
            roster,
            ConsensusTimingConfig::default(),
            recovered.core_state,
        )?;
        debug_assert_eq!(core.role(), ConsensusRole::Leader);

        Ok(Self {
            local_cell,
            local_domain,
            socket_accounting: SocketActivityAccounting::new(),
            inner: Mutex::new(SingleHostInner {
                store,
                core,
                state_machine: recovered.state_machine,
                latest_durable_snapshot,
            }),
        })
    }

    /// Always returns [`ClusterProfile::SingleHostDesktop`].
    #[must_use]
    pub const fn cluster_profile(&self) -> ClusterProfile {
        ClusterProfile::SingleHostDesktop
    }

    /// Local nominal `CellIdentity`.
    #[must_use]
    pub const fn local_cell(&self) -> CellIdentity {
        self.local_cell
    }

    /// Local failure/control domain (`[TASK-SCALE-002]`).
    #[must_use]
    pub const fn local_domain(&self) -> ControlDomainId {
        self.local_domain
    }

    /// Active bound listener + open peer socket count (**always `0`** in `N = 1`).
    #[must_use]
    pub fn active_socket_count(&self) -> usize {
        self.socket_accounting
            .bound_listener_count()
            .saturating_add(self.socket_accounting.active_connection_count())
    }

    /// Active background timer/task count (**always `0`** in `N = 1`).
    #[must_use]
    pub const fn active_background_timer_count(&self) -> usize {
        0
    }

    /// Whether TLS X.509 certificates are required (**always `false`** in `N = 1`).
    #[must_use]
    pub const fn requires_tls_certificates(&self) -> bool {
        false
    }

    /// Commits `proposal` in **1** local `SQLite WAL + synchronous=FULL`
    /// transaction and returns its [`ConsensusCommitReceipt`].
    ///
    /// # Errors
    ///
    /// Fails closed on any `SQLite` durability or integrity error.
    pub fn propose(
        &self,
        proposal: ControlPlaneProposal,
    ) -> Result<ConsensusCommitReceipt, ConsensusError> {
        let mut guard = self.lock_inner();
        let effects = guard.core.step(ConsensusInput::Propose {
            proposal,
            expected_leader_term: None,
        });
        let SingleHostInner {
            store,
            state_machine,
            ..
        } = &mut *guard;
        let mut receipts = store.apply_durable_effects(&effects, state_machine)?;
        let (_, receipt) = receipts.pop().ok_or_else(|| {
            ConsensusError::Store(ConsensusStoreError::IntegrityCorrupted(
                "single-host proposal produced no ConsensusCommitReceipt".to_owned(),
            ))
        })?;
        Ok(receipt)
    }

    /// Decodes a wire proposal frame under `[CONS-SCOPE-001]` and commits it if
    /// permitted, rejecting any data-plane tag (`Task` / `Fiber` / `Topic` /
    /// `SemanticEvent` / `Artifact`) before touching `SQLite`.
    ///
    /// # Errors
    ///
    /// Returns [`ConsensusError::ScopeViolation`] on forbidden data-plane or
    /// malformed wire payloads.
    pub fn propose_wire(
        &self,
        wire_frame: &[u8],
    ) -> Result<ConsensusCommitReceipt, ConsensusError> {
        let proposal = ControlPlaneProposal::decode_wire(wire_frame)?;
        self.propose(proposal)
    }

    /// Executes an immediate linearizable read against the local `N = 1`
    /// state machine with zero network RTTs.
    pub fn read_state<R>(
        &self,
        reader: impl FnOnce(&ControlPlaneStateMachine, LogIndex, ConsensusTerm) -> R,
    ) -> R {
        let guard = self.lock_inner();
        reader(
            &guard.state_machine,
            guard.core.consensus_commit_index(),
            guard.core.current_term(),
        )
    }

    /// Current durable `consensus_commit_index`.
    #[must_use]
    pub fn consensus_commit_index(&self) -> LogIndex {
        self.lock_inner().core.consensus_commit_index()
    }

    /// Current durable `current_term` (`1` in single-host passthrough).
    #[must_use]
    pub fn current_term(&self) -> ConsensusTerm {
        self.lock_inner().core.current_term()
    }

    /// Current deterministic `ControlPlaneStateMachine` root digest.
    #[must_use]
    pub fn state_root_digest(&self) -> [u8; 32] {
        self.lock_inner().state_machine.state_root_digest()
    }

    /// Number of uncompacted log entries currently retained in `consensus_log`.
    #[must_use]
    pub fn uncompacted_log_len(&self) -> usize {
        self.lock_inner().core.uncompacted_log().len()
    }

    /// Creates a durable `[DUR-SNAP-001]` snapshot at `last_applied_index`
    /// without compacting `consensus_log` yet.
    ///
    /// # Errors
    ///
    /// Returns [`ConsensusError`] if no entries have been committed yet or if
    /// snapshot persistence fails.
    pub fn create_snapshot_only(&self) -> Result<SnapshotMetadata, ConsensusError> {
        let mut guard = self.lock_inner();
        let included_index = guard.state_machine.last_applied_index();
        let included_term = guard.state_machine.last_applied_term();
        let bytes = guard.state_machine.encode_snapshot();
        let meta = guard
            .store
            .create_snapshot(included_index, included_term, &bytes)?;
        guard.latest_durable_snapshot = Some((meta, bytes));
        Ok(meta)
    }

    /// Compacts `consensus_log` up to `up_to_index` (`<= snapshot.included_index`).
    ///
    /// # Errors
    ///
    /// Fails closed if `up_to_index` is not covered by a durable snapshot.
    pub fn compact_log_up_to(&self, up_to_index: LogIndex) -> Result<usize, ConsensusError> {
        let mut guard = self.lock_inner();
        let deleted = guard.store.compact_log_up_to(up_to_index)?;
        if let Some((meta, bytes)) = guard.latest_durable_snapshot.clone() {
            guard.core.record_local_snapshot(meta, bytes, up_to_index);
        }
        Ok(deleted)
    }

    /// Creates a durable `[DUR-SNAP-001]` snapshot at `last_applied_index` and
    /// immediately compacts covered `consensus_log` entries.
    ///
    /// # Errors
    ///
    /// Fails closed if `last_applied_index == 0` or persistence fails.
    pub fn create_snapshot_and_compact(&self) -> Result<SnapshotMetadata, ConsensusError> {
        let mut guard = self.lock_inner();
        let included_index = guard.state_machine.last_applied_index();
        let included_term = guard.state_machine.last_applied_term();
        let bytes = guard.state_machine.encode_snapshot();
        let meta = guard
            .store
            .create_snapshot(included_index, included_term, &bytes)?;
        guard.store.compact_log_up_to(included_index)?;
        guard.latest_durable_snapshot = Some((meta, bytes.clone()));
        guard
            .core
            .record_local_snapshot(meta, bytes, included_index);
        Ok(meta)
    }

    /// Forces a `TRUNCATE` WAL checkpoint on the underlying `SQLite` database.
    ///
    /// # Errors
    ///
    /// Returns [`ConsensusError::Store`] if checkpointing fails.
    pub fn checkpoint_wal_truncate(&self) -> Result<(), ConsensusError> {
        self.lock_inner()
            .store
            .checkpoint_wal_truncate()
            .map_err(ConsensusError::Store)
    }

    fn lock_inner(&self) -> std::sync::MutexGuard<'_, SingleHostInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
