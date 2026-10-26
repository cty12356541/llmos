//! Cell host assembly (W48) — the §26.1 seven-piece Cell, assembled in this
//! crate per the cell-assembly wiring map.
//!
//! **Assembly form:** [`CellHost`] *wraps* one [`SliceKRuntime`] by value
//! plus the three Cell-local pieces the runtime does not own. Wrapping (not
//! extending [`SliceKRuntime`], not running side-by-side) is the
//! minimal-intrusion shape: the runtime already opens the durable
//! process/artifact/operation authorities and owns the outbox-pump lane, and
//! the host adds exactly the pieces v0.5 §26.1 places in the Cell —
//!
//! 1. process supervisor face — the runtime's `ProcessAuthority` plus the
//!    heartbeat wiring of this module (registration observed = liveness
//!    evidence, terminal = silence);
//! 2. resource-lease sub-ledgers — all three `nlos-lease` families
//!    (`QuotaLeaseGrantor`, `CapacityLeaseGrantor`,
//!    `ExclusiveDeviceLeaseGrantor`) over one *shared* authority
//!    ([`CellAuthority::into_shared`], ADR-0021); the quota grantor remains
//!    the single initiator of epoch advances;
//! 3. driver gateway — one [`ProviderCache`] over a [`MockProvider`] bound
//!    to the runtime's durable operation store (the consumer-side gateway
//!    entry with degradation/recovery);
//! 4. durable outbox — the runtime's existing `OutboxPump` lane, started
//!    and stopped through [`CellHost::runtime`] unchanged;
//! 5. artifact cache — the runtime's existing `ArtifactStore`;
//! 6. failure detector — [`FailureDetector`], built from the authority's
//!    opening fence;
//! 7. capability/name cache — [`CapabilityNameCache`], built from the same
//!    fence.
//!
//! **Shared authority, single epoch writer (ADR-0021):** the claim slot of
//! `nlos-cell` is process-unique and never released (ADR-0018: one OS
//! process is one Cell), and that slot — not Rust ownership — is what makes
//! the [`CellAuthority`] unique. All three grantor families therefore
//! co-hold one shared authority `Arc`-wise inside the claiming process
//! (W46's by-value API kept exactly one grantor per process; ADR-0021
//! retired that shape so the capacity and device families assemble here
//! too). Sharing mints readers, not writers: the quota family stays the
//! only epoch-advance entry (`advance_epoch_and_quarantine`,
//! `LEASE-LOSS-001`) — a runtime invariant pinned by tests, since the
//! compiler no longer proves single-writer through ownership.
//!
//! **Epoch broadcast chain (the wiring map's key invariant):** epoch
//! advances flow one way — [`CellHost::advance_cell_epoch`] drives the
//! quota grantor's `advance_epoch_and_quarantine`, takes the new
//! [`CellFence`], and broadcasts `on_epoch_advanced` in a fixed order
//! (failure detector first, then the name cache). A refusal mid-chain is
//! returned as the typed incomplete state [`CellEpochBroadcastIncomplete`]
//! (which consumer had already received the fence, and the refusal) — an
//! advanced-but-partially-broadcast epoch is never silently swallowed.
//!
//! **Federation face (W50-L2 + W51-L1, opt-in):**
//! [`CellHost::open_federated`] additionally publishes this Cell into one
//! shared [`CellDirectory`] (cross-Cell name/service discovery) and exposes
//! migration-intent registration and reconciliation-checkpoint registration
//! on the same root — the three §26.1 control-plane mechanism faces of
//! ADR-0019 decision 5. Default opening ([`CellHost::open`]) is unchanged:
//! no federation root, no registration, no behavior delta. The published
//! registration carries the host's *last published* fence (refreshed by
//! [`CellHost::refresh_cell_registration`]); it is a discovery snapshot,
//! not a fencing oracle, and no cross-Cell commit, intent execution, or
//! checkpoint verification/coordination is implied or performed here.

use std::path::Path;
use std::sync::Arc;

use nlos_artifact::ReplicaSyncRetentionSet;
use nlos_cell::{
    CacheHit, CapabilityNameCache, CellAuthority, CellDirectory, CellEpoch, CellFence,
    CellFencingToken, CellIdentity, CellRegistrationEntry, CheckpointDigest, CheckpointFact,
    CheckpointPrefix, CheckpointRecord, ControlPlaneLinkState, EpochAdvanceReport,
    EpochInvalidation, FailureDetector, FailureDetectorConfig, FailureDetectorError,
    HeartbeatOutcome, InsertOutcome, InvalidationOutcome, LocalAutonomyProof, MigrationIntent,
    MigrationIntentRecord, MonitoredSubject, NameCacheError, NamePath, PartitionAutonomyGate,
    PartitionAutonomyReceipt, PartitionViolation, PartitionedActionKind, SweepReport,
};
use nlos_clock::NowRequest;
use nlos_driver_mock::{CacheHealth, MockProvider, ProviderCache};
use nlos_identity::{BootstrapPrincipalRequest, KeyPurpose};
use nlos_lease::{
    CapacityLeaseGrant, CapacityLeaseGrantor, CapacityReconcileError,
    CapacityReconciliationReceipt, DeviceReconcileError, DeviceReconciliationReceipt,
    DeviceResetReceipt, ExclusiveDeviceLeaseGrant, ExclusiveDeviceLeaseGrantor, LeaseInstant,
    QuotaLeaseGrant, QuotaLeaseGrantor, QuotaLeaseLedgerError, QuotaLeaseState,
    QuotaReconcileError, QuotaReconciliationReceipt, QuotaWalLossError, SqliteLeaseStore,
};
use nlos_notify::{
    AckNotificationRequest, DeliveredTaskNotification, IssueActionTokenDecision,
    IssueNotificationActionTokenRequest, NotificationAckDecision, NotificationActionRedeemReceipt,
    NotificationHistoryPage, NotificationId, NotificationPublishDecision, NotificationService,
    NotificationSubscribeDecision, RedeemNotificationActionRequest, TaskNotificationEnvelope,
};
use nlos_process::{
    MarkProcessTerminatedRequest, ProcessAuthorityError, ProcessBindingRecord,
    ProcessTerminalRecord, RegisterSupervisorPidRequest, SupervisorPidRegistry,
    SupervisorPidRegistryError,
};
use nlos_runtime_tokio::PumpHealth;
use nlos_search::{
    AssertionSelector, CellSearchShardOutcome, FederatedSearchReport, NonAuthoritativeCandidateHit,
    PaginatedAssertionSelector, SearchPage, VerifiedCheckpointCandidate,
};
use nlos_semantic::SignedSemanticVectorCheckpoint;
use nlos_topic::{SubscriberKey, TopicId};
use nlos_types::{
    CapabilityId, CapacityLeaseId, DeviceId, ExclusiveDeviceLeaseId, Generation, IdempotencyKey,
    ProcessId, QuotaLeaseId, SchedulerDomainId, TaskAttemptId, TaskId,
};
use nlos_wait::WaitAuthority;
use sha2::{Digest, Sha256};

use crate::error::{SliceKError, SliceKResult};
use crate::runtime::{SearchServiceGuard, SliceKRuntime, seeded_key};
use crate::service_faces::{
    NotificationTopicBinding, NotificationTopicConfig, SubscribedNotificationTopic,
    export_cell_search_shard, federated_search_across_cell_hosts,
};

/// Key tag of every durable tick the cell host takes for heartbeat and
/// sweep evidence (`cellbeat ‖ sequence`), disjoint from the slice fixture's
/// `seeded_key` space so the two lanes never collide on one clock store.
const CELL_BEAT_KEY_TAG: &[u8; 8] = b"cellbeat";

/// The seed-offset pair of the terminal-marking face (idempotency key,
/// wall-reading key), kept in the slice's fixture-key convention.
const TERMINATE_KEY_OFFSET: u8 = 120;
const TERMINATE_WALL_KEY_OFFSET: u8 = 121;

/// Assembly inputs of one [`CellHost`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CellHostConfig {
    /// Stable Cell identity (the Cell layer of `SchedulerDomainId`; opaque
    /// bytes, no host/path/pid encoding).
    pub domain: SchedulerDomainId,
    /// Initial AVAILABLE pool of the quota lease sub-ledger.
    pub quota_available: u64,
    /// Initial source pool of the capacity lease sub-ledger (W49).
    pub capacity_pool: u64,
    /// Device whose exclusive-lease head the device sub-ledger manages
    /// (W49; one head per host in this slice).
    pub device: DeviceId,
    /// Escalation thresholds of the failure detector, in durable-clock
    /// logical ticks.
    pub detector: FailureDetectorConfig,
}

impl CellHostConfig {
    /// Builds the assembly inputs. The detector config is explicit (no
    /// defaults: thresholds are an operational decision); so are the two
    /// lease pools and the managed device.
    #[must_use]
    pub const fn new(
        domain: SchedulerDomainId,
        quota_available: u64,
        capacity_pool: u64,
        device: DeviceId,
        detector: FailureDetectorConfig,
    ) -> Self {
        Self {
            domain,
            quota_available,
            capacity_pool,
            device,
            detector,
        }
    }
}

/// The Cell as an assembly: one [`SliceKRuntime`] by value (process
/// supervisor face, driver gateway, durable outbox lane, artifact cache)
/// plus the Cell-local pieces the runtime does not own — the three lease
/// sub-ledgers over one shared [`CellAuthority`] (ADR-0021; the quota
/// grantor is the epoch-advance entry), the failure detector, and the
/// capability/name cache.
///
/// Not cloneable and single-claim per OS process (the `nlos-cell` claim
/// slot); a second [`CellHost::open`] in one process fails typed.
pub struct CellHost {
    /// The wrapped slice runtime: durable process/artifact/operation
    /// authorities, clock, and the outbox-pump lane.
    runtime: SliceKRuntime,
    /// The quota lease sub-ledger — the epoch-advance entry of the
    /// assembly. The authority it holds is the shared one all three
    /// families co-hold (ADR-0021).
    quota: QuotaLeaseGrantor,
    /// The capacity lease sub-ledger over the same shared authority (W49).
    capacity: CapacityLeaseGrantor,
    /// The exclusive device lease sub-ledger over the same shared authority
    /// (W49).
    device: ExclusiveDeviceLeaseGrantor,
    /// The cell-local failure detector (snapshot consumer of the fence).
    detector: FailureDetector,
    /// The cell-local capability/name cache (snapshot consumer of the
    /// fence).
    cache: CapabilityNameCache,
    /// The cell-local driver gateway entry over the runtime's durable
    /// operation store.
    gateway: ProviderCache,
    /// The fence this host last assembled or advanced to. Authoritative by
    /// construction: the only writer of the authority's epoch is
    /// [`Self::advance_cell_epoch`], which updates this snapshot in the
    /// same call that advances the grantor.
    fence: CellFence,
    /// Monotone counter feeding the durable-clock tick keys of heartbeat
    /// and sweep evidence.
    beat_sequence: u64,
    /// Host OS process identifier claimed by this Cell (`ADR-0018`).
    os_process_id: u32,
    /// Generation-fenced `ProcessId` → host OS pid supervisor registry (`W64`).
    supervisor_pids: SupervisorPidRegistry,
    /// Cell-local partition autonomy gate (`[DIST-LOCAL-001]`, `W65`).
    partition_gate: PartitionAutonomyGate,
    /// Replica sync conflict/tombstone retention root set (`[DIST-ART-001]`, `W65`).
    artifact_sync_retention: ReplicaSyncRetentionSet,
    /// The opt-in federation face: shared-root [`CellDirectory`] plus this
    /// host's monotone registration-heartbeat counter. `None` under the
    /// default (non-federated) opening — zero behavior delta there.
    federation: Option<FederationFace>,
}

/// The opt-in federation face of one host: the shared-root directory
/// handle, this host's heartbeat counter for its own registration file,
/// and the publishing process's pid (observational, republished verbatim).
#[derive(Debug)]
struct FederationFace {
    directory: CellDirectory,
    heartbeat: u64,
    os_process_id: u32,
}

impl CellHost {
    /// Opens the assembled Cell under one root directory.
    ///
    /// Opening order: the slice runtime first (its authority opens are the
    /// crash-recovery path), then the process-unique Cell claim against
    /// `<root>/cell` (so restarts bump `node_boot_generation`), then the
    /// detector and name cache from the claiming fence, then the claimed
    /// authority turned shared ([`CellAuthority::into_shared`], ADR-0021)
    /// and taken by all three lease grantors, then the gateway over the
    /// runtime's operation store. No federation root is touched: default
    /// opening is exactly the pre-W50 behavior.
    ///
    /// # Errors
    ///
    /// Fails closed with the first refusal: a runtime authority open error,
    /// or [`SliceKError::Cell`] when this OS process already holds a Cell,
    /// the cell data directory is unusable, or its identity is bound to a
    /// different Cell.
    pub fn open(root: impl AsRef<Path>, config: CellHostConfig) -> SliceKResult<Self> {
        Self::open_with_federation(root.as_ref(), config, None, false)
    }

    /// Opens the assembled Cell with all three lease sub-ledgers backed by a
    /// shared `SQLite WAL + FULL` [`SqliteLeaseStore`] at
    /// `<root>/leases.sqlite3` (`W62` / `W66`), so committed `QuotaLease`,
    /// `CapacityLease`, and `ExclusiveDeviceLease` states rehydrate across hard
    /// OS process crashes and restarts.
    ///
    /// # Errors
    ///
    /// As [`Self::open`], plus [`SliceKError::LeaseStore`] if the durable lease
    /// database cannot be opened or fails conservation/integrity verification.
    pub fn open_durable(root: impl AsRef<Path>, config: CellHostConfig) -> SliceKResult<Self> {
        Self::open_with_federation(root.as_ref(), config, None, true)
    }

    /// Opens the assembled Cell under one root directory **and** publishes
    /// its registration (heartbeat 1) into the shared federation root —
    /// the opt-in mechanism face of W50-L2 (ADR-0019 decision 5): cross-
    /// Cell name/service discovery plus migration-intent registration on
    /// one [`CellDirectory`].
    ///
    /// The published record carries the opening fence; later refreshes
    /// ([`Self::refresh_cell_registration`]) republish the host's
    /// last-advanced fence at a monotone heartbeat. Discovery freshness
    /// and intent reading flow through [`Self::cell_directory`]. Nothing
    /// here executes a migration or commits anything across Cells.
    ///
    /// # Errors
    ///
    /// As [`Self::open`], plus [`SliceKError::Federation`] when the shared
    /// root cannot be opened or the first registration cannot be published.
    pub fn open_federated(
        root: impl AsRef<Path>,
        config: CellHostConfig,
        federation_root: impl AsRef<Path>,
    ) -> SliceKResult<Self> {
        Self::open_with_federation(root.as_ref(), config, Some(federation_root.as_ref()), false)
    }

    /// Opens a federated Cell backed by durable `<root>/leases.sqlite3` storage
    /// (`[ROAD-C-001]` / `W66` crash-restart + reconnection convergence path).
    ///
    /// # Errors
    ///
    /// As [`Self::open_federated`], plus [`SliceKError::LeaseStore`] on durable
    /// lease store open or rehydration errors.
    pub fn open_federated_durable(
        root: impl AsRef<Path>,
        config: CellHostConfig,
        federation_root: impl AsRef<Path>,
    ) -> SliceKResult<Self> {
        Self::open_with_federation(root.as_ref(), config, Some(federation_root.as_ref()), true)
    }

    fn open_with_federation(
        root: &Path,
        config: CellHostConfig,
        federation_root: Option<&Path>,
        durable_leases: bool,
    ) -> SliceKResult<Self> {
        let runtime = SliceKRuntime::open(root)?;
        let authority =
            CellAuthority::claim_with_data_dir(config.domain, runtime.root().join("cell"))?;
        let os_process_id = authority.os_process_id();
        let authority = authority.into_shared();
        let (quota, capacity, device) = if durable_leases {
            let store = Arc::new(SqliteLeaseStore::open(
                runtime.root().join("leases.sqlite3"),
            )?);
            let q = QuotaLeaseGrantor::open_with_store(
                Arc::clone(&store),
                Arc::clone(&authority),
                config.quota_available,
            )?;
            let c = CapacityLeaseGrantor::open_with_store(
                Arc::clone(&store),
                Arc::clone(&authority),
                config.capacity_pool,
            )?;
            let d = ExclusiveDeviceLeaseGrantor::open_with_store(
                store,
                Arc::clone(&authority),
                config.device,
            )?;
            (q, c, d)
        } else {
            let q = QuotaLeaseGrantor::open(Arc::clone(&authority), config.quota_available);
            let c = CapacityLeaseGrantor::open(Arc::clone(&authority), config.capacity_pool);
            let d = ExclusiveDeviceLeaseGrantor::open(Arc::clone(&authority), config.device);
            (q, c, d)
        };
        let fence = authority.fence();
        let detector = FailureDetector::new(&fence, config.detector);
        let cache = CapabilityNameCache::new(&fence);
        let gateway =
            ProviderCache::new(Arc::new(MockProvider::new(Arc::clone(&runtime.operations))));
        let federation = match federation_root {
            Some(root) => {
                let directory = CellDirectory::open(root)?;
                let entry = federation_registration(&fence, os_process_id, 1)?;
                directory.publish_registration(&entry)?;
                Some(FederationFace {
                    directory,
                    heartbeat: 1,
                    os_process_id,
                })
            }
            None => None,
        };
        Ok(Self {
            runtime,
            quota,
            capacity,
            device,
            detector,
            cache,
            gateway,
            fence,
            beat_sequence: 0,
            os_process_id,
            supervisor_pids: SupervisorPidRegistry::new(),
            partition_gate: PartitionAutonomyGate::new_connected(fence, 0),
            artifact_sync_retention: ReplicaSyncRetentionSet::new(),
            federation,
        })
    }

    /// The shared-root federation directory when this host was opened
    /// federated (discovery reads, intent reads and records); `None` under
    /// the default opening.
    #[must_use]
    pub const fn cell_directory(&self) -> Option<&CellDirectory> {
        match &self.federation {
            Some(face) => Some(&face.directory),
            None => None,
        }
    }

    /// Heartbeats this Cell's registration in the federation root: bumps
    /// the monotone counter and republishes the host's current fence whole
    /// (write-temp + rename, so readers never observe a torn record).
    /// Returns the stamped entry, or `None` when this host is not
    /// federated (the call is then a no-op, not an error).
    ///
    /// # Errors
    ///
    /// Fails with [`SliceKError::Federation`] when the publish refuses
    /// (stale heartbeat, unusable root) and [`SliceKError::CellHost`] on
    /// counter exhaustion.
    pub fn refresh_cell_registration(&mut self) -> SliceKResult<Option<CellRegistrationEntry>> {
        let Some(face) = &mut self.federation else {
            return Ok(None);
        };
        let heartbeat = face.heartbeat.checked_add(1).ok_or(SliceKError::CellHost(
            "federation registration heartbeat exhausted",
        ))?;
        face.heartbeat = heartbeat;
        let entry = federation_registration(&self.fence, face.os_process_id, heartbeat)?;
        Ok(Some(face.directory.publish_registration(&entry)?))
    }

    /// Appends one migration intent to the federation root's intent log —
    /// registration only, never execution (`C-MIGRATE` owns that). Returns
    /// the recorded form, or `None` when this host is not federated.
    ///
    /// # Errors
    ///
    /// Fails with [`SliceKError::Federation`] when the intent is invalid
    /// or the append refuses.
    pub fn record_migration_intent(
        &self,
        intent: &MigrationIntent,
    ) -> SliceKResult<Option<MigrationIntentRecord>> {
        match &self.federation {
            Some(face) => Ok(Some(face.directory.record_migration_intent(intent)?)),
            None => Ok(None),
        }
    }

    /// Appends one reconciliation/audit checkpoint of this Cell to the
    /// federation root's checkpoint log (W51-L1, the third §26.1 mechanism
    /// face of ADR-0019 decision 5): the host's current fence snapshot
    /// (boot generation, epoch, fencing token) plus the caller's one-line
    /// digest of what claims reconciled. Registration only — verifying or
    /// executing the fact, and any cross-Cell reconciliation coordination,
    /// stay deferred until ADR-0019's finalization. Returns the recorded
    /// form, or `None` when this host is not federated (the call is then a
    /// no-op, not an error).
    ///
    /// # Errors
    ///
    /// Fails with [`SliceKError::Federation`] when the digest is invalid
    /// or the append refuses.
    pub fn record_reconciliation_checkpoint(
        &self,
        digest: &str,
    ) -> SliceKResult<Option<CheckpointRecord>> {
        let Some(face) = &self.federation else {
            return Ok(None);
        };
        let fence = self.fence;
        let checkpoint_fact = CheckpointFact::new(
            fence.node_boot_generation(),
            fence.epoch(),
            fence.fencing_token(),
            digest,
        )?;
        Ok(Some(
            face.directory
                .record_checkpoint(fence.identity(), &checkpoint_fact)?,
        ))
    }

    /// Appends one structured [`CheckpointDigest`] derived from `prefix` and
    /// this host's current fence snapshot to the federation checkpoint log
    /// (`CS0-B` / `W65`). Returns `None` when this host is not federated.
    ///
    /// # Errors
    ///
    /// Fails with [`SliceKError::Federation`] when the checkpoint append refuses.
    pub fn record_structured_reconciliation_checkpoint(
        &self,
        prefix: &CheckpointPrefix,
    ) -> SliceKResult<Option<(CheckpointDigest, CheckpointRecord)>> {
        let fence = self.fence;
        let digest = CheckpointDigest::from_prefix(
            fence.node_boot_generation(),
            fence.epoch(),
            fence.fencing_token(),
            prefix,
        );
        let recorded = self.record_reconciliation_checkpoint(&digest.to_text())?;
        Ok(recorded.map(|rec| (digest, rec)))
    }

    /// The wrapped slice runtime — the assembled home of the process
    /// supervisor face, artifact cache, and outbox-pump lane (the pump
    /// starts and stops through the runtime's own lifecycle methods).
    #[must_use]
    pub fn runtime(&self) -> &SliceKRuntime {
        &self.runtime
    }

    /// Durable [`WaitAuthority`] assembled inside this Cell's runtime (`W66`).
    #[must_use]
    pub fn waits(&self) -> &Arc<WaitAuthority> {
        self.runtime.waits()
    }

    /// Managed [`NotificationService`] assembled inside this Cell's runtime
    /// (`W66`, unfreezing `nlos-notify`).
    #[must_use]
    pub fn notify(&self) -> &NotificationService {
        self.runtime.notify()
    }

    /// Read-only, non-authoritative [`SearchService`] guard assembled over this
    /// Cell's [`SemanticAuthority`](nlos_semantic::SemanticAuthority) (`W66`,
    /// unfreezing `nlos-search`). Derefs directly to [`SearchService`].
    pub fn search(&self) -> SearchServiceGuard<'_> {
        self.runtime.search()
    }

    /// Idempotently provisions (or replays) a notification channel + topic
    /// binding on this Cell (`[TASK-NOTIFY-001]`).
    ///
    /// # Errors
    ///
    /// Propagates any clock, channel, or topic authority refusal.
    pub fn ensure_notification_topic(
        &self,
        config: &NotificationTopicConfig,
    ) -> SliceKResult<NotificationTopicBinding> {
        self.runtime.ensure_notification_topic(config)
    }

    /// Subscribes `subscriber_key` to `topic_id` through this Cell's
    /// [`NotificationService`].
    ///
    /// # Errors
    ///
    /// Propagates any [`NotificationService`] or [`nlos_topic::TopicAuthority`] refusal.
    pub fn subscribe_notification_topic(
        &self,
        topic_id: TopicId,
        subscriber_key: SubscriberKey,
        subscribed_at_ms: u64,
    ) -> SliceKResult<NotificationSubscribeDecision> {
        self.runtime
            .subscribe_notification_topic(topic_id, subscriber_key, subscribed_at_ms)
    }

    /// Idempotently provisions `config` and subscribes `subscriber_key` on
    /// this Cell (`[TASK-NOTIFY-001]`).
    ///
    /// # Errors
    ///
    /// Propagates any clock, channel, topic, or notification-service error.
    pub fn bootstrap_and_subscribe_notification_topic(
        &self,
        config: &NotificationTopicConfig,
        subscriber_key: SubscriberKey,
        subscribed_at_ms: u64,
    ) -> SliceKResult<SubscribedNotificationTopic> {
        self.runtime.bootstrap_and_subscribe_notification_topic(
            config,
            subscriber_key,
            subscribed_at_ms,
        )
    }

    /// Publishes a canonical `[TASK-NOTIFY-001]` [`TaskNotificationEnvelope`]
    /// transition on this Cell and records its `[DUI-OFFLINE-001]` offline
    /// history reference.
    ///
    /// # Errors
    ///
    /// Propagates any [`NotificationService`] refusal.
    pub fn publish_task_notification(
        &self,
        topic_id: TopicId,
        envelope: &TaskNotificationEnvelope,
        idempotency_key: IdempotencyKey,
    ) -> SliceKResult<NotificationPublishDecision> {
        self.runtime
            .publish_task_notification(topic_id, envelope, idempotency_key)
    }

    /// Polls unacknowledged `[TASK-NOTIFY-001]` task transition notifications
    /// for `notification_id` on this Cell.
    ///
    /// # Errors
    ///
    /// Propagates any [`NotificationService`] refusal or malformed wire error.
    pub fn poll_task_notifications(
        &self,
        notification_id: NotificationId,
        limit: usize,
    ) -> SliceKResult<Vec<DeliveredTaskNotification>> {
        self.runtime.poll_task_notifications(notification_id, limit)
    }

    /// Advances the subscriber's token-authenticated delivery cursor on this
    /// Cell.
    ///
    /// # Errors
    ///
    /// Propagates any [`NotificationService`] refusal.
    pub fn ack_notification(
        &self,
        request: AckNotificationRequest,
    ) -> SliceKResult<NotificationAckDecision> {
        self.runtime.ack_notification(request)
    }

    /// Issues a `[DUI-NOTIFY-001]` cryptographically bound one-shot
    /// `ActionToken` on this Cell.
    ///
    /// # Errors
    ///
    /// Propagates any [`NotificationService`] refusal.
    pub fn issue_notification_action_token(
        &self,
        request: IssueNotificationActionTokenRequest,
    ) -> SliceKResult<IssueActionTokenDecision> {
        self.runtime.issue_notification_action_token(request)
    }

    /// Redeems a `[DUI-NOTIFY-001]` bound one-shot `ActionToken` on this Cell.
    ///
    /// # Errors
    ///
    /// Fails closed on cross-application spoofing, binding mismatch, invalid
    /// proof, expiry, revocation, or duplicate redemption.
    pub fn redeem_notification_action_token(
        &self,
        request: RedeemNotificationActionRequest,
    ) -> SliceKResult<NotificationActionRedeemReceipt> {
        self.runtime.redeem_notification_action_token(request)
    }

    /// Queries a cursor-paginated `[DUI-OFFLINE-001]` offline notification
    /// history page on this Cell strictly after `after_sequence`.
    ///
    /// # Errors
    ///
    /// Fails closed on zero or out-of-bounds `limit` or `SQLite` errors.
    pub fn query_offline_notification_history(
        &self,
        topic_id: TopicId,
        after_sequence: u64,
        limit: usize,
    ) -> SliceKResult<NotificationHistoryPage> {
        self.runtime
            .query_offline_notification_history(topic_id, after_sequence, limit)
    }

    /// Exports a [`SignedSemanticVectorCheckpoint`] signed by this Cell's
    /// semantic writer (`[SEM-CHECKPOINT-001]`).
    ///
    /// # Errors
    ///
    /// Propagates any [`nlos_semantic::SemanticAuthority`] checkpoint signing error.
    pub fn export_signed_vector_checkpoint(
        &self,
        trust_snapshot_digest: [u8; 32],
        signed_at_ms: u64,
    ) -> SliceKResult<SignedSemanticVectorCheckpoint> {
        self.runtime
            .export_signed_vector_checkpoint(trust_snapshot_digest, signed_at_ms)
    }

    /// Queries a cursor-paginated page of non-authoritative candidates over
    /// this Cell's [`SemanticAuthority`](nlos_semantic::SemanticAuthority),
    /// preserving [`SearchUseRestrictions::NON_AUTHORITATIVE`](nlos_search::SearchUseRestrictions::NON_AUTHORITATIVE)
    /// (`[CONS-DECL-001]`, `[CONS-CTRL-001]`).
    ///
    /// # Errors
    ///
    /// Fails closed on invalid limits, decode corruption, or authority
    /// trust-view errors.
    pub fn query_non_authoritative_candidates(
        &self,
        paginated: &PaginatedAssertionSelector,
    ) -> SliceKResult<SearchPage<NonAuthoritativeCandidateHit>> {
        self.runtime.query_non_authoritative_candidates(paginated)
    }

    /// Queries checkpoint-bounded non-authoritative candidates on this Cell
    /// (`require_domain_present = true`, `[SEM-CHECKPOINT-001]`,
    /// `[TASK-SNAPSHOT-003]`, `[CONS-CTRL-001]`).
    ///
    /// # Errors
    ///
    /// Fails closed on invalid checkpoint signatures/digests, missing or
    /// mismatched domain watermarks, or `selector.limit == 0`.
    pub fn query_at_signed_checkpoint(
        &self,
        checkpoint: &SignedSemanticVectorCheckpoint,
        selector: &AssertionSelector,
        after_log_seq: Option<u64>,
    ) -> SliceKResult<SearchPage<NonAuthoritativeCandidateHit>> {
        self.runtime
            .query_at_signed_checkpoint(checkpoint, selector, after_log_seq)
    }

    /// Queries checkpoint-bounded non-authoritative candidates on this Cell
    /// with explicit `require_domain_present` policy (`[SEM-CHECKPOINT-001]`).
    ///
    /// # Errors
    ///
    /// Fails closed on invalid checkpoint signatures/digests, watermark
    /// mismatches, or missing watermark when `require_domain_present == true`.
    pub fn query_at_signed_checkpoint_with_options(
        &self,
        checkpoint: &SignedSemanticVectorCheckpoint,
        selector: &AssertionSelector,
        after_log_seq: Option<u64>,
        require_domain_present: bool,
    ) -> SliceKResult<SearchPage<NonAuthoritativeCandidateHit>> {
        self.runtime.query_at_signed_checkpoint_with_options(
            checkpoint,
            selector,
            after_log_seq,
            require_domain_present,
        )
    }

    /// Re-verifies a discovered [`NonAuthoritativeCandidateHit`] against
    /// `checkpoint` and this Cell's live [`SemanticAuthority`](nlos_semantic::SemanticAuthority)
    /// (`[TASK-SNAPSHOT-003]`, `[CONS-CTRL-001]`).
    ///
    /// # Errors
    ///
    /// Fails closed on tampered `use_restrictions`, invalid checkpoint
    /// signature, watermark mismatch, authority record mismatch, durable
    /// retraction, or post-checkpoint `log_seq`.
    pub fn verify_candidate_against_checkpoint(
        &self,
        candidate: &NonAuthoritativeCandidateHit,
        checkpoint: &SignedSemanticVectorCheckpoint,
    ) -> SliceKResult<VerifiedCheckpointCandidate> {
        self.runtime
            .verify_candidate_against_checkpoint(candidate, checkpoint)
    }

    /// Exports this Cell's non-authoritative [`CellSearchShardOutcome`] keyed
    /// by `self.fence().identity()` (`[SEM-CHECKPOINT-001]`,
    /// `[CONS-CTRL-001]`).
    ///
    /// # Errors
    ///
    /// Propagates any search or checkpoint verification error.
    pub fn export_search_shard(
        &self,
        selector: &AssertionSelector,
        checkpoint: Option<&SignedSemanticVectorCheckpoint>,
    ) -> SliceKResult<CellSearchShardOutcome> {
        export_cell_search_shard(self, selector, checkpoint)
    }

    /// Executes federated non-authoritative candidate aggregation across `self`
    /// and `peer` (`[SEM-CHECKPOINT-001]`, `[DIST-FAIL-001]`,
    /// `[CONS-DECL-001]`, `[CONS-CTRL-001]`).
    ///
    /// # Errors
    ///
    /// Propagates any shard query or federated aggregation refusal.
    pub fn federated_search_with_peer(
        &self,
        peer: &Self,
        selector: &AssertionSelector,
        max_results: usize,
        allow_partial_on_remote_failure: bool,
    ) -> SliceKResult<FederatedSearchReport> {
        federated_search_across_cell_hosts(
            self,
            peer,
            selector,
            max_results,
            allow_partial_on_remote_failure,
        )
    }

    /// The fence this host last assembled or advanced to. The
    /// authority-holding grantor is the only epoch writer, and this host is
    /// its only driver, so the snapshot cannot go stale except in the
    /// documented incomplete-broadcast state carried by
    /// [`SliceKError::CellBroadcast`].
    #[must_use]
    pub const fn fence(&self) -> CellFence {
        self.fence
    }

    /// The cell-local driver gateway entry (the minimal gateway face of the
    /// seven-piece set): a [`ProviderCache`] over a [`MockProvider`] bound
    /// to this runtime's durable operation store, with the cache's
    /// degradation/recovery semantics.
    #[must_use]
    pub fn gateway(&self) -> &ProviderCache {
        &self.gateway
    }

    /// Read-only handle on the cell-local failure detector: liveness views,
    /// dead-judgment history, epoch, and config.
    #[must_use]
    pub const fn failure_detector(&self) -> &FailureDetector {
        &self.detector
    }

    /// Remaining AVAILABLE of the quota lease sub-ledger.
    #[must_use]
    pub const fn quota_available(&self) -> u64 {
        self.quota.available()
    }

    /// Grants a `QuotaLease` against the host's current fence (the
    /// single-holder presentation).
    ///
    /// # Errors
    ///
    /// Propagates [`nlos_lease::QuotaLeaseGrantError`] typed: a stale or
    /// mismatched fence, insufficient AVAILABLE, or a conflicting replay of
    /// the same lease id.
    pub fn grant_quota_lease(
        &mut self,
        lease_id: QuotaLeaseId,
        face_value: u64,
        expires_at: Option<LeaseInstant>,
    ) -> SliceKResult<QuotaLeaseGrant> {
        let fence = self.fence;
        self.grant_quota_lease_against(&fence, lease_id, face_value, expires_at)
    }

    /// Grants a `QuotaLease` against an explicit fence presentation — the
    /// fail-closed probe face: a fence older than the authority's current
    /// epoch is rejected typed, which is how a stale holder discovers an
    /// epoch advanced underneath it.
    ///
    /// # Errors
    ///
    /// As [`Self::grant_quota_lease`].
    pub fn grant_quota_lease_against(
        &mut self,
        presented: &CellFence,
        lease_id: QuotaLeaseId,
        face_value: u64,
        expires_at: Option<LeaseInstant>,
    ) -> SliceKResult<QuotaLeaseGrant> {
        Ok(self
            .quota
            .grant(presented, lease_id, face_value, expires_at)?)
    }

    /// Reads the committed quota lease snapshot.
    ///
    /// # Errors
    ///
    /// Fails with [`SliceKError::LeaseLedger`] when the id was never
    /// issued.
    pub fn query_quota_lease(&self, lease_id: QuotaLeaseId) -> SliceKResult<QuotaLeaseGrant> {
        Ok(self.quota.query(lease_id)?)
    }

    /// Transitions `ISSUED → ACTIVE` on the quota lease sub-ledger against
    /// the host's current fence (`W64`).
    ///
    /// # Errors
    ///
    /// Propagates [`nlos_lease::QuotaLeaseLedgerError`] typed.
    pub fn activate_quota_lease(
        &mut self,
        lease_id: QuotaLeaseId,
    ) -> SliceKResult<QuotaLeaseGrant> {
        let fence = self.fence;
        Ok(self.quota.activate(&fence, lease_id)?)
    }

    /// Reports monotonic usage high-water on an `ACTIVE` `QuotaLease` against
    /// the host's current fence (`LEASE-SPEND-001`, `LEASE-REPORT-001`, `W64`).
    ///
    /// # Errors
    ///
    /// Propagates [`nlos_lease::QuotaLeaseLedgerError`] typed.
    pub fn report_quota_usage(
        &mut self,
        lease_id: QuotaLeaseId,
        high_water: u64,
    ) -> SliceKResult<QuotaLeaseGrant> {
        let fence = self.fence;
        Ok(self.quota.report_usage(&fence, lease_id, high_water)?)
    }

    /// Starts `ACTIVE → CLOSING` drain on a `QuotaLease` against the host's
    /// current fence without returning funds to `AVAILABLE` (`LEASE-CLOSE-001`,
    /// `W64`).
    ///
    /// # Errors
    ///
    /// Propagates [`nlos_lease::QuotaLeaseLedgerError`] typed.
    pub fn begin_quota_close(&mut self, lease_id: QuotaLeaseId) -> SliceKResult<QuotaLeaseGrant> {
        let fence = self.fence;
        Ok(self.quota.begin_close(&fence, lease_id)?)
    }

    /// Completes `CLOSING → CLOSED` on a `QuotaLease` against the host's
    /// current fence, returning the unspent remainder once (`LEASE-CLOSE-001`,
    /// `W64`).
    ///
    /// # Errors
    ///
    /// Propagates [`nlos_lease::QuotaLeaseLedgerError`] typed.
    pub fn ack_quota_close(&mut self, lease_id: QuotaLeaseId) -> SliceKResult<QuotaLeaseGrant> {
        let fence = self.fence;
        Ok(self.quota.ack_close(&fence, lease_id)?)
    }

    /// Reconciles a `QUARANTINED` or `FENCED` `QuotaLease` via a verified
    /// [`QuotaReconciliationReceipt`] (`[LEASE-RECON-001]`, `W64`).
    ///
    /// # Errors
    ///
    /// Propagates [`QuotaReconcileError`] typed.
    pub fn reconcile_quota_lease(
        &mut self,
        receipt: QuotaReconciliationReceipt,
    ) -> Result<QuotaLeaseGrant, QuotaReconcileError> {
        let fence = self.fence;
        self.quota.reconcile(&fence, receipt)
    }

    /// Reports monotonic usage high-water against an explicit presented fence
    /// (`LEASE-SPEND-001`, `W65`) — used to verify fail-closed stale-epoch
    /// spend rejection after partition reconnection.
    ///
    /// # Errors
    ///
    /// Propagates [`QuotaLeaseLedgerError`] typed (including
    /// [`nlos_cell::CellAdmitError::StaleEpoch`]).
    pub fn report_quota_usage_against(
        &mut self,
        presented: &CellFence,
        lease_id: QuotaLeaseId,
        high_water: u64,
    ) -> SliceKResult<QuotaLeaseGrant> {
        Ok(self.quota.report_usage(presented, lease_id, high_water)?)
    }

    /// Sweeps `QuotaLease`s whose TTL deadline has expired at `now`, moving
    /// them into `FENCED` (`LEASE-TTL-001`, `W65`).
    pub fn sweep_expired_quota_leases(&mut self, now: LeaseInstant) -> Vec<QuotaLeaseId> {
        self.quota.sweep_expired(now)
    }

    /// Explicitly rejects any attempt to recover a WAL-lost `QuotaLease` by
    /// guessing `remaining` from `face_value` (`[LEASE-WAL-001]`, `W66`).
    ///
    /// # Errors
    ///
    /// Always fails closed with [`QuotaWalLossError::GuessedRemainingForbidden`]
    /// (or [`QuotaWalLossError::UnknownLease`] if the lease id does not exist).
    pub fn refuse_guessed_remaining_on_wal_loss(
        &self,
        lease_id: QuotaLeaseId,
        guessed_remaining: u64,
    ) -> Result<QuotaLeaseGrant, QuotaWalLossError> {
        self.quota
            .refuse_guessed_remaining_on_wal_loss(lease_id, guessed_remaining)
    }

    /// Fences all unsettled `QuotaLease`s on `lost_boot_generation` into
    /// `QUARANTINED` without returning balance to `AVAILABLE`, advances the
    /// Cell epoch/fencing token, and broadcasts the new fence across this
    /// host's detector and capability/name cache (`[LEASE-WAL-001]`, `W66`).
    ///
    /// # Errors
    ///
    /// Fails closed with [`SliceKError::QuotaWalLoss`] if
    /// `reconnected_boot_generation <= lost_boot_generation` or epoch space is
    /// exhausted, or [`SliceKError::CellBroadcast`] on mid-chain broadcast
    /// refusal.
    pub fn fence_boot_generation_on_wal_loss(
        &mut self,
        lost_boot_generation: Generation,
        reconnected_boot_generation: Generation,
    ) -> SliceKResult<(CellEpochAdvance, Vec<QuotaLeaseId>)> {
        let (new_fence, quarantined_ids) = self
            .quota
            .fence_boot_generation_on_wal_loss(lost_boot_generation, reconnected_boot_generation)?;
        let detector_report = match self.detector.on_epoch_advanced(&new_fence) {
            Ok(report) => report,
            Err(refusal) => {
                return Err(SliceKError::CellBroadcast(Box::new(
                    CellEpochBroadcastIncomplete {
                        advanced_to: new_fence,
                        detector: None,
                        name_cache: None,
                        refusal: CellBroadcastRefusal::Detector(refusal),
                    },
                )));
            }
        };
        let name_cache_report = match self.cache.on_epoch_advanced(&new_fence) {
            Ok(report) => report,
            Err(refusal) => {
                return Err(SliceKError::CellBroadcast(Box::new(
                    CellEpochBroadcastIncomplete {
                        advanced_to: new_fence,
                        detector: Some(detector_report),
                        name_cache: None,
                        refusal: CellBroadcastRefusal::Cache(refusal),
                    },
                )));
            }
        };
        self.fence = new_fence;
        if self.partition_gate.state().is_connected() {
            let tick = self.partition_gate.last_observed_tick();
            let _ = self
                .partition_gate
                .record_control_plane_contact(new_fence, tick);
        }
        Ok((
            CellEpochAdvance {
                fence: new_fence,
                detector: detector_report,
                name_cache: name_cache_report,
            },
            quarantined_ids,
        ))
    }

    /// Committed [`QuotaReconciliationReceipt`] records for `lease_id` (`W65`).
    #[must_use]
    pub fn quota_reconciliation_receipts(
        &self,
        lease_id: QuotaLeaseId,
    ) -> &[QuotaReconciliationReceipt] {
        self.quota.reconciliation_receipts(lease_id)
    }

    /// Committed [`CapacityReconciliationReceipt`] for `capacity_lease_id`
    /// (`W65`).
    #[must_use]
    pub fn capacity_reconciliation_receipt(
        &self,
        capacity_lease_id: CapacityLeaseId,
    ) -> Option<CapacityReconciliationReceipt> {
        self.capacity.reconciliation_receipt(capacity_lease_id)
    }

    /// Committed [`DeviceReconciliationReceipt`] for `device_lease_id` (`W65`).
    #[must_use]
    pub fn device_reconciliation_receipt(
        &self,
        device_lease_id: ExclusiveDeviceLeaseId,
    ) -> Option<DeviceReconciliationReceipt> {
        self.device.reconciliation_receipt(device_lease_id)
    }

    /// Remaining source pool of the capacity lease sub-ledger (W49).
    #[must_use]
    pub const fn capacity_pool_remaining(&self) -> u64 {
        self.capacity.pool_remaining()
    }

    /// Grants a `CapacityLease` against the host's current fence (the
    /// assembly's single-claim presentation; W49).
    ///
    /// # Errors
    ///
    /// Propagates [`nlos_lease::CapacityLeaseGrantError`] typed: a stale or
    /// mismatched fence, an insufficient source pool, or a conflicting
    /// replay of the same capacity lease id.
    pub fn grant_capacity_lease(
        &mut self,
        capacity_lease_id: CapacityLeaseId,
        amount: u64,
    ) -> SliceKResult<CapacityLeaseGrant> {
        let fence = self.fence;
        self.grant_capacity_lease_against(&fence, capacity_lease_id, amount)
    }

    /// Grants a `CapacityLease` against an explicit fence presentation —
    /// the fail-closed probe face (W49), symmetric to
    /// [`Self::grant_quota_lease_against`].
    ///
    /// # Errors
    ///
    /// As [`Self::grant_capacity_lease`].
    pub fn grant_capacity_lease_against(
        &mut self,
        presented: &CellFence,
        capacity_lease_id: CapacityLeaseId,
        amount: u64,
    ) -> SliceKResult<CapacityLeaseGrant> {
        Ok(self.capacity.grant(presented, capacity_lease_id, amount)?)
    }

    /// Reads the committed capacity lease snapshot (W49).
    ///
    /// # Errors
    ///
    /// Fails with [`SliceKError::CapacityLeaseReturn`] when the id was
    /// never issued.
    pub fn query_capacity_lease(
        &self,
        capacity_lease_id: CapacityLeaseId,
    ) -> SliceKResult<CapacityLeaseGrant> {
        Ok(self.capacity.query(capacity_lease_id)?)
    }

    /// `GLOBAL_RESERVED` / `TARGET_PREPARED` / `ACTIVE` → `RETURNING`
    /// against the host's current fence (W49): the return leg starts; no
    /// refund happens here.
    ///
    /// # Errors
    ///
    /// Propagates [`nlos_lease::CapacityLeaseReturnError`] typed: a stale
    /// or mismatched fence, an unknown lease id, or an already-returned
    /// lease.
    pub fn begin_capacity_return(
        &mut self,
        capacity_lease_id: CapacityLeaseId,
    ) -> SliceKResult<CapacityLeaseGrant> {
        let fence = self.fence;
        Ok(self.capacity.begin_return(&fence, capacity_lease_id)?)
    }

    /// `RETURNING` → `RETURNED` against the host's current fence (W49):
    /// refunds the full `amount` to the source pool once — capacity is
    /// returned whole, not consumed.
    ///
    /// # Errors
    ///
    /// Propagates [`nlos_lease::CapacityLeaseReturnError`] typed: a stale
    /// or mismatched fence, an unknown lease id, or a lease that has not
    /// begun its return.
    pub fn ack_capacity_return(
        &mut self,
        capacity_lease_id: CapacityLeaseId,
    ) -> SliceKResult<CapacityLeaseGrant> {
        let fence = self.fence;
        Ok(self.capacity.ack_return(&fence, capacity_lease_id)?)
    }

    /// Fences a `CapacityLease` (`GLOBAL_RESERVED` / `TARGET_PREPARED` /
    /// `ACTIVE` / `RETURNING` → `FENCED`) against the host's current fence
    /// without returning capacity (`[LEASE-CAPACITY-002]`, `W64`).
    ///
    /// # Errors
    ///
    /// Propagates [`nlos_lease::CapacityLeaseReturnError`] typed.
    pub fn fence_capacity_lease(
        &mut self,
        capacity_lease_id: CapacityLeaseId,
    ) -> SliceKResult<CapacityLeaseGrant> {
        let fence = self.fence;
        Ok(self.capacity.fence_lease(&fence, capacity_lease_id)?)
    }

    /// Quarantines an unsettled `CapacityLease` (`→ QUARANTINED`) against the
    /// host's current fence without returning capacity (`[LEASE-CAPACITY-002]`,
    /// `W64`).
    ///
    /// # Errors
    ///
    /// Propagates [`nlos_lease::CapacityLeaseReturnError`] typed.
    pub fn quarantine_capacity_lease(
        &mut self,
        capacity_lease_id: CapacityLeaseId,
    ) -> SliceKResult<CapacityLeaseGrant> {
        let fence = self.fence;
        Ok(self.capacity.quarantine_lease(&fence, capacity_lease_id)?)
    }

    /// Reconciles a `FENCED`, `QUARANTINED`, or `RETURNING` `CapacityLease`
    /// via a verified [`CapacityReconciliationReceipt`] (`[LEASE-CAPACITY-002]`,
    /// `W64`).
    ///
    /// # Errors
    ///
    /// Propagates [`CapacityReconcileError`] typed.
    pub fn reconcile_capacity_lease(
        &mut self,
        receipt: CapacityReconciliationReceipt,
    ) -> Result<CapacityLeaseGrant, CapacityReconcileError> {
        let fence = self.fence;
        self.capacity.reconcile(&fence, receipt)
    }

    /// Whether the device lease sub-ledger's `DeviceLeaseHead` is still
    /// FREE (W49).
    #[must_use]
    pub const fn device_head_free(&self) -> bool {
        self.device.is_free()
    }

    /// Device identity managed by this host's exclusive device sub-ledger (`W64`).
    #[must_use]
    pub const fn device_id(&self) -> DeviceId {
        self.device.device_id()
    }

    /// Current `reset_generation` held by the `DeviceLeaseHead` (`W64`).
    #[must_use]
    pub const fn device_reset_generation(&self) -> Generation {
        self.device.reset_generation()
    }

    /// Grants an `ExclusiveDeviceLease` against the host's current fence
    /// (W49): claims the FREE head before the lease is issued.
    ///
    /// # Errors
    ///
    /// Propagates [`nlos_lease::ExclusiveDeviceLeaseGrantError`] typed: a
    /// stale or mismatched fence, or a head that is not FREE.
    pub fn grant_device_lease(
        &mut self,
        device_lease_id: ExclusiveDeviceLeaseId,
    ) -> SliceKResult<ExclusiveDeviceLeaseGrant> {
        let fence = self.fence;
        self.grant_device_lease_against(&fence, device_lease_id)
    }

    /// Grants an `ExclusiveDeviceLease` against an explicit fence
    /// presentation — the fail-closed probe face (W49), symmetric to
    /// [`Self::grant_quota_lease_against`].
    ///
    /// # Errors
    ///
    /// As [`Self::grant_device_lease`].
    pub fn grant_device_lease_against(
        &mut self,
        presented: &CellFence,
        device_lease_id: ExclusiveDeviceLeaseId,
    ) -> SliceKResult<ExclusiveDeviceLeaseGrant> {
        Ok(self.device.grant(presented, device_lease_id)?)
    }

    /// Reads the committed device lease snapshot (W49).
    ///
    /// # Errors
    ///
    /// Fails with [`SliceKError::DeviceLeaseReturn`] when the id was never
    /// issued.
    pub fn query_device_lease(
        &self,
        device_lease_id: ExclusiveDeviceLeaseId,
    ) -> SliceKResult<ExclusiveDeviceLeaseGrant> {
        Ok(self.device.query(device_lease_id)?)
    }

    /// `DEVICE_RESERVED` → `RESETTING` against the host's current fence
    /// (W49): the holder declares reset+zeroization intent; the head is
    /// not FREE yet.
    ///
    /// # Errors
    ///
    /// Propagates [`nlos_lease::ExclusiveDeviceLeaseReturnError`] typed: a
    /// stale or mismatched fence, an unknown lease id, or an
    /// already-returned lease.
    pub fn declare_device_reset(
        &mut self,
        device_lease_id: ExclusiveDeviceLeaseId,
    ) -> SliceKResult<ExclusiveDeviceLeaseGrant> {
        let fence = self.fence;
        Ok(self.device.declare_reset(&fence, device_lease_id)?)
    }

    /// Submits the reset+zeroization receipt (`RESETTING` → `RETURNED`)
    /// against the host's current fence (W49): on accept the receipt joins
    /// the immutable log, the head returns to FREE, and the head reset
    /// generation bumps. The receipt's own axes must bind the committed
    /// lease — including the exclusivity epoch the lease was issued under,
    /// which may precede the current Cell epoch.
    ///
    /// # Errors
    ///
    /// Propagates [`nlos_lease::ExclusiveDeviceLeaseReturnError`] typed: a
    /// stale or mismatched fence, an unknown lease id, a receipt that does
    /// not bind the lease or head fence, or a conflicting replay of the
    /// same idempotency key.
    pub fn submit_device_reset_receipt(
        &mut self,
        receipt: DeviceResetReceipt,
    ) -> SliceKResult<ExclusiveDeviceLeaseGrant> {
        let fence = self.fence;
        Ok(self.device.submit_reset_receipt(&fence, receipt)?)
    }

    /// Fences an `ExclusiveDeviceLease` (`DEVICE_RESERVED` / `RESETTING` →
    /// `FENCING`) against the host's current fence while keeping the head
    /// occupied (`[LEASE-DEVICE-001]`, `W64`).
    ///
    /// # Errors
    ///
    /// Propagates [`nlos_lease::ExclusiveDeviceLeaseReturnError`] typed.
    pub fn fence_device_lease(
        &mut self,
        device_lease_id: ExclusiveDeviceLeaseId,
    ) -> SliceKResult<ExclusiveDeviceLeaseGrant> {
        let fence = self.fence;
        Ok(self.device.fence_lease(&fence, device_lease_id)?)
    }

    /// Quarantines an unsettled `ExclusiveDeviceLease` (`→ QUARANTINED`)
    /// against the host's current fence while keeping the head occupied
    /// (`[LEASE-DEVICE-001]`, `W64`).
    ///
    /// # Errors
    ///
    /// Propagates [`nlos_lease::ExclusiveDeviceLeaseReturnError`] typed.
    pub fn quarantine_device_lease(
        &mut self,
        device_lease_id: ExclusiveDeviceLeaseId,
    ) -> SliceKResult<ExclusiveDeviceLeaseGrant> {
        let fence = self.fence;
        Ok(self.device.quarantine_lease(&fence, device_lease_id)?)
    }

    /// Reconciles a `QUARANTINED`, `FENCING`, or `RESETTING`
    /// `ExclusiveDeviceLease` via a verified [`DeviceReconciliationReceipt`],
    /// clearing the head back to `FREE` and bumping `reset_generation`
    /// (`[LEASE-DEVICE-001]`, `W64`).
    ///
    /// # Errors
    ///
    /// Propagates [`DeviceReconcileError`] typed.
    pub fn reconcile_device_lease(
        &mut self,
        receipt: DeviceReconciliationReceipt,
    ) -> Result<ExclusiveDeviceLeaseGrant, DeviceReconcileError> {
        let fence = self.fence;
        self.device.reconcile(&fence, receipt)
    }

    /// The explicit epoch-advance entry — the single-initiator broadcast
    /// chain of the wiring map.
    ///
    /// The capacity and device grantors co-hold the same shared authority
    /// (ADR-0021) and therefore observe the new epoch immediately, but
    /// neither can advance it: this method, driving the quota grantor,
    /// stays the only epoch writer — the single-entry discipline the
    /// sharing face turned into a runtime invariant.
    ///
    /// 1. the quota grantor advances the Cell epoch (fencing token with
    ///    it) and quarantines every unreconciled lease of the previous
    ///    epoch (`LEASE-LOSS-001`; their face value is not returned to
    ///    AVAILABLE);
    /// 2. the new fence is broadcast to the failure detector
    ///    (`on_epoch_advanced`: pending suspicions void, heartbeat evidence
    ///    does not cross the boundary, dead subjects re-enter as fresh
    ///    incarnations);
    /// 3. then to the capability/name cache (`on_epoch_advanced`: every
    ///    entry cached before the boundary becomes invisible; per-path
    ///    generation high-waters survive).
    ///
    /// Only when both broadcasts accept does the host's fence snapshot
    /// advance. A refusal mid-chain returns the typed incomplete state
    /// [`SliceKError::CellBroadcast`] — the epoch *has* advanced in the
    /// grantor, and the error names which consumer already received it; the
    /// state converges only by manual adjudication (this host deliberately
    /// offers no guess-forward path).
    ///
    /// # Errors
    ///
    /// Fails with [`SliceKError::Cell`] when the epoch/token space is
    /// exhausted (nothing advanced), and
    /// [`SliceKError::CellBroadcast`] on a mid-chain broadcast refusal.
    pub fn advance_cell_epoch(&mut self) -> SliceKResult<CellEpochAdvance> {
        let new_fence = self.quota.advance_epoch_and_quarantine()?;
        let detector_report = match self.detector.on_epoch_advanced(&new_fence) {
            Ok(report) => report,
            Err(refusal) => {
                return Err(SliceKError::CellBroadcast(Box::new(
                    CellEpochBroadcastIncomplete {
                        advanced_to: new_fence,
                        detector: None,
                        name_cache: None,
                        refusal: CellBroadcastRefusal::Detector(refusal),
                    },
                )));
            }
        };
        let name_cache_report = match self.cache.on_epoch_advanced(&new_fence) {
            Ok(report) => report,
            Err(refusal) => {
                return Err(SliceKError::CellBroadcast(Box::new(
                    CellEpochBroadcastIncomplete {
                        advanced_to: new_fence,
                        detector: Some(detector_report),
                        name_cache: None,
                        refusal: CellBroadcastRefusal::Cache(refusal),
                    },
                )));
            }
        };
        self.fence = new_fence;
        if self.partition_gate.state().is_connected() {
            let tick = self.partition_gate.last_observed_tick();
            let _ = self
                .partition_gate
                .record_control_plane_contact(new_fence, tick);
        }
        Ok(CellEpochAdvance {
            fence: new_fence,
            detector: detector_report,
            name_cache: name_cache_report,
        })
    }

    /// Current [`ControlPlaneLinkState`] tracked by this host's
    /// [`PartitionAutonomyGate`] (`[DIST-LOCAL-001]`, `W65`).
    #[must_use]
    pub const fn connectivity_state(&self) -> ControlPlaneLinkState {
        self.partition_gate.state()
    }

    /// Read-only handle to this host's [`PartitionAutonomyGate`] (`W65`).
    #[must_use]
    pub const fn partition_autonomy_gate(&self) -> &PartitionAutonomyGate {
        &self.partition_gate
    }

    /// Replica sync conflict/tombstone retention roots held by this host
    /// (`[DIST-ART-001]`, `W65`).
    #[must_use]
    pub const fn artifact_sync_retention(&self) -> &ReplicaSyncRetentionSet {
        &self.artifact_sync_retention
    }

    /// Mutable handle to this host's [`ReplicaSyncRetentionSet`] (`W65`).
    pub const fn artifact_sync_retention_mut(&mut self) -> &mut ReplicaSyncRetentionSet {
        &mut self.artifact_sync_retention
    }

    /// Transitions this host's control-plane link into
    /// [`ControlPlaneLinkState::Partitioned`] at its current [`CellFence`]
    /// (`[DIST-LOCAL-001]`, `W65`).
    ///
    /// # Errors
    ///
    /// Propagates [`PartitionViolation`] on tick regression or invalid state.
    pub fn enter_control_plane_partition(
        &mut self,
        now_tick: u64,
        reason_code: u16,
    ) -> Result<ControlPlaneLinkState, PartitionViolation> {
        let fence = self.fence;
        self.partition_gate
            .enter_partition(fence, now_tick, reason_code)
    }

    /// Transitions this host's control-plane link from `Partitioned` into
    /// [`ControlPlaneLinkState::Reconnecting`] using the host's current
    /// advanced [`CellFence`] (`[LEASE-LOSS-001]`, `W65`).
    ///
    /// Callers MUST invoke [`Self::advance_cell_epoch`] first so `self.fence`
    /// has strictly advanced its `CellEpoch` and `CellFencingToken`.
    ///
    /// # Errors
    ///
    /// Propagates [`PartitionViolation`] if not partitioned, if `now_tick`
    /// regresses, or if `self.fence` has not advanced past `frozen_fence`.
    pub fn begin_control_plane_reconnection(
        &mut self,
        now_tick: u64,
    ) -> Result<ControlPlaneLinkState, PartitionViolation> {
        let candidate_fence = self.fence;
        self.partition_gate
            .begin_reconnection(candidate_fence, now_tick)
    }

    /// Completes reconnection convergence from `Reconnecting` back into
    /// [`ControlPlaneLinkState::Connected`] at the host's current [`CellFence`]
    /// (`W65`).
    ///
    /// # Errors
    ///
    /// Propagates [`PartitionViolation`] on invalid state or tick regression.
    pub fn complete_control_plane_reconnection(
        &mut self,
        now_tick: u64,
    ) -> Result<ControlPlaneLinkState, PartitionViolation> {
        let verified_fence = self.fence;
        self.partition_gate
            .complete_reconnection(verified_fence, now_tick)
    }

    /// Evaluates [`PartitionAutonomyGate::admit_action`] against this host's
    /// current [`CellFence`] (`[DIST-LOCAL-001]`, `W65`).
    ///
    /// # Errors
    ///
    /// Propagates [`PartitionViolation`] fail-closed.
    pub fn admit_partitioned_action(
        &self,
        now_tick: u64,
        action: &PartitionedActionKind,
    ) -> Result<PartitionAutonomyReceipt, PartitionViolation> {
        let fence = self.fence;
        self.partition_gate.admit_action(now_tick, &fence, action)
    }

    /// Probe variant of [`Self::admit_partitioned_action`] accepting an
    /// explicit presented [`CellFence`] (`W65`).
    ///
    /// # Errors
    ///
    /// Propagates [`PartitionViolation`] fail-closed.
    pub fn admit_partitioned_action_against(
        &self,
        now_tick: u64,
        presented: &CellFence,
        action: &PartitionedActionKind,
    ) -> Result<PartitionAutonomyReceipt, PartitionViolation> {
        self.partition_gate
            .admit_action(now_tick, presented, action)
    }

    /// Evaluates local autonomy admission for a local `QuotaLease` spend (or
    /// `0`-spend pure compute) and, upon admission, commits the monotonic spend
    /// delta into the local `QuotaLease` sub-ledger (`[DIST-LOCAL-001]`,
    /// `[LEASE-SPEND-001]`, `W65`).
    ///
    /// # Errors
    ///
    /// Fails closed before any ledger mutation when the lease does not exist,
    /// the capability/lease proof fails [`PartitionAutonomyGate`], or the
    /// ledger rejects the usage update.
    pub fn spend_quota_lease_with_local_autonomy(
        &mut self,
        now_tick: u64,
        lease_id: QuotaLeaseId,
        spend_delta: u64,
        capability_valid_until_tick: u64,
        capability_revoked: bool,
        capability_locally_verified: bool,
    ) -> Result<(PartitionAutonomyReceipt, QuotaLeaseGrant), CellPartitionSpendError> {
        let existing = self.quota.query(lease_id)?;
        let proof = LocalAutonomyProof::new(
            capability_valid_until_tick,
            capability_revoked,
            capability_locally_verified,
            existing.epoch(),
            existing.expires_at().map(LeaseInstant::get),
            existing.remaining(),
            spend_delta,
        );
        let receipt = self.admit_partitioned_action(
            now_tick,
            &PartitionedActionKind::LocalPureComputeOrAuthorizedSpend(proof),
        )?;
        let fence = self.fence;
        let active = if existing.state() == QuotaLeaseState::Issued {
            self.quota.activate(&fence, lease_id)?
        } else {
            existing
        };
        let updated = if spend_delta == 0 {
            active
        } else {
            let next_high_water = active.spent().checked_add(spend_delta).ok_or(
                QuotaLeaseLedgerError::UsageExceedsFace {
                    reported: u64::MAX,
                    face_value: active.face_value(),
                },
            )?;
            self.quota.report_usage(&fence, lease_id, next_high_water)?
        };
        Ok((receipt, updated))
    }

    /// Exports the public-key [`BootstrapPrincipalRequest`] describing this
    /// host's dedicated [`crate::SemanticWriter`] principal so a peer Cell's
    /// `IdentityAuthority` can cryptographically verify signed vector
    /// checkpoints and cross-domain semantic events produced by this host
    /// without ever exporting private key material (`[DIST-EVENT-001]`, `W65`).
    ///
    /// # Errors
    ///
    /// Propagates [`SliceKError::Identity`] if the writer key binding cannot be
    /// inspected.
    pub fn semantic_writer_bootstrap_request(&self) -> SliceKResult<BootstrapPrincipalRequest> {
        const WRITER_IDENTITY_DOMAIN: &[u8] = b"llmos/slice-k/semantic-writer/identity/v1";
        let binding = self
            .runtime
            .identity
            .inspect_current_binding(self.runtime.semantic_writer().key_id())?;
        let public_key = binding.public_key;

        let mut profile_hasher = Sha256::new();
        profile_hasher.update(WRITER_IDENTITY_DOMAIN);
        profile_hasher.update(b"profile");
        profile_hasher.update(public_key);
        let principal_profile_digest: [u8; 32] = profile_hasher.finalize().into();

        let mut policy_hasher = Sha256::new();
        policy_hasher.update(WRITER_IDENTITY_DOMAIN);
        policy_hasher.update(b"policy");
        policy_hasher.update(public_key);
        let control_domain_policy_digest: [u8; 32] = policy_hasher.finalize().into();

        let mut bootstrap_hasher = Sha256::new();
        bootstrap_hasher.update(WRITER_IDENTITY_DOMAIN);
        bootstrap_hasher.update(b"bootstrap");
        bootstrap_hasher.update(public_key);
        let bootstrap_digest: [u8; 32] = bootstrap_hasher.finalize().into();
        let mut id_bytes = [0_u8; 16];
        id_bytes.copy_from_slice(&bootstrap_digest[..16]);

        Ok(BootstrapPrincipalRequest {
            principal_profile_digest,
            control_domain_policy_digest,
            public_key,
            key_purpose: KeyPurpose::SemanticSigning,
            key_valid_from_ms: binding.key_valid_from_ms,
            key_valid_until_ms: binding.key_valid_until_ms,
            idempotency_key: IdempotencyKey::from_bytes(id_bytes),
            created_at_ms: binding.key_valid_from_ms,
        })
    }

    /// Caches a capability handle for `path` in the cell-local name cache.
    ///
    /// # Errors
    ///
    /// Propagates [`NameCacheError`] typed: a generation at or below the
    /// path's invalidation high-water, a stale entry, or a same-generation
    /// capability conflict.
    pub fn insert_cached_name(
        &mut self,
        path: NamePath,
        capability: CapabilityId,
        capability_generation: Generation,
    ) -> SliceKResult<InsertOutcome> {
        Ok(self.cache.insert(path, capability, capability_generation)?)
    }

    /// Serves the cached handle for `path`, if a live entry survives both
    /// fences (generation high-water and current epoch). A miss is `None`
    /// regardless of cause.
    #[must_use]
    pub fn cached_name(&self, path: &NamePath) -> Option<CacheHit> {
        self.cache.get(path)
    }

    /// Advances the invalidation high-water of `path` (monotone,
    /// irreversible; survives epoch boundaries).
    ///
    /// # Errors
    ///
    /// Fails with [`NameCacheError::StaleInvalidation`] when `through` is
    /// below the path's current high-water.
    pub fn invalidate_cached_name(
        &mut self,
        path: NamePath,
        through: Generation,
    ) -> SliceKResult<InvalidationOutcome> {
        Ok(self.cache.invalidate(path, through)?)
    }

    /// Host OS process identifier claimed by this [`CellHost`] (`ADR-0018`).
    #[must_use]
    pub const fn os_process_id(&self) -> u32 {
        self.os_process_id
    }

    /// Generation-fenced `ProcessId` → host OS pid supervisor registry (`W64`).
    #[must_use]
    pub const fn supervisor_pids(&self) -> &SupervisorPidRegistry {
        &self.supervisor_pids
    }

    /// Spawns (or replays) the delegated process of one attempt through the
    /// wrapped runtime's supervisor face, registers `(process_id, generation)`
    /// in the host's [`SupervisorPidRegistry`], and reports the registration
    /// to the failure detector as liveness evidence — the minimal heartbeat
    /// wiring: *registration observed = heartbeat*. A replayed registration is
    /// a fresh registration call and therefore fresh evidence.
    ///
    /// # Errors
    ///
    /// Propagates the process authority's fail-closed refusals, supervisor pid
    /// registry fence errors, and clock errors; the heartbeat itself can
    /// refuse only on logical-clock regression (typed
    /// [`SliceKError::FailureDetector`]).
    pub fn spawn_monitored_process(
        &mut self,
        seed: u8,
        task_id: TaskId,
        task_attempt_id: TaskAttemptId,
        attempt_generation: Generation,
    ) -> SliceKResult<ProcessBindingRecord> {
        let binding =
            self.runtime
                .materialize_process(seed, task_id, task_attempt_id, attempt_generation)?;
        self.supervisor_pids
            .register(RegisterSupervisorPidRequest {
                process_id: binding.process_id,
                process_generation: binding.process_generation,
                os_pid: self.os_process_id,
                registered_at_ms: binding.created_at_ms,
            })?;
        self.record_process_heartbeat(binding.process_id)?;
        Ok(binding)
    }

    /// Records one generation-fenced heartbeat for `(process_id, expected_generation)`
    /// (`W64`): verifies that `(process_id, expected_generation)` is both the
    /// active binding in `ProcessAuthority` and currently registered in
    /// [`Self::supervisor_pids`] before recording detector liveness evidence.
    ///
    /// # Errors
    ///
    /// Fails closed if the binding is terminal, missing, at a different
    /// generation, unregistered in [`Self::supervisor_pids`], or already
    /// judged dead by the [`FailureDetector`].
    pub fn record_fenced_process_heartbeat(
        &mut self,
        process_id: ProcessId,
        expected_generation: Generation,
    ) -> SliceKResult<HeartbeatOutcome> {
        let active = self
            .runtime
            .process
            .inspect_active_process_binding(process_id)?;
        if active.process_generation != expected_generation {
            return Err(SliceKError::Process(
                ProcessAuthorityError::StaleProcessBinding,
            ));
        }
        let entry = self.supervisor_pids.lookup(process_id)?;
        if entry.process_generation != expected_generation {
            return Err(SliceKError::SupervisorPid(
                SupervisorPidRegistryError::StaleProcessGeneration {
                    process_id,
                    current: entry.process_generation,
                    presented: expected_generation,
                },
            ));
        }
        self.record_process_heartbeat(process_id)
    }

    /// Records one heartbeat for a process subject at a fresh durable-clock
    /// tick — the liveness-evidence entry point of the detector wiring.
    ///
    /// # Errors
    ///
    /// Fails with [`SliceKError::FailureDetector`] when the subject's
    /// current incarnation is already judged dead (a dead incarnation never
    /// revives in place), and with clock errors when no tick can be taken.
    pub fn record_process_heartbeat(
        &mut self,
        process_id: ProcessId,
    ) -> SliceKResult<HeartbeatOutcome> {
        let now = self.next_beat_tick()?;
        Ok(self
            .detector
            .record_heartbeat(MonitoredSubject::Process(process_id), now)?)
    }

    /// Runs one detector escalation sweep at a fresh durable-clock tick.
    ///
    /// # Errors
    ///
    /// Fails with [`SliceKError::FailureDetector`] only on a logical-clock
    /// regression (impossible through this host — its ticks are the durable
    /// clock's monotone readings), and with clock errors when no tick can
    /// be taken.
    pub fn sweep_liveness(&mut self) -> SliceKResult<SweepReport> {
        let now = self.next_beat_tick()?;
        Ok(self.detector.sweep(now)?)
    }

    /// Marks the process binding cleanly terminated through the supervisor
    /// face and unregisters `(process_id, generation)` from
    /// [`Self::supervisor_pids`]. **No heartbeat is recorded** — a terminal
    /// observation is not liveness evidence; silence starts (or continues) and
    /// the detector's own sweep escalates it. Replaying the same marking is
    /// idempotent and equally heartbeat-free.
    ///
    /// # Errors
    ///
    /// Propagates the process authority's fail-closed refusals (stale
    /// fence, idempotency conflict), supervisor pid registry fence errors,
    /// and clock errors.
    pub fn terminate_monitored_process(
        &mut self,
        binding: &ProcessBindingRecord,
        seed: u8,
    ) -> SliceKResult<ProcessTerminalRecord> {
        let marked_at_ms = self
            .runtime
            .wall_now_ms(seeded_key(seed, TERMINATE_WALL_KEY_OFFSET))?;
        let record = self
            .runtime
            .process
            .mark_process_terminated(MarkProcessTerminatedRequest {
                process_id: binding.process_id,
                expected_process_generation: binding.process_generation,
                expected_process_fencing_token: binding.process_fencing_token,
                idempotency_key: seeded_key(seed, TERMINATE_KEY_OFFSET),
                marked_at_ms,
            })?
            .record()
            .clone();
        let _ = self
            .supervisor_pids
            .unregister(binding.process_id, binding.process_generation)?;
        Ok(record)
    }

    /// The seven-piece assembly inspect: one live fact per piece, straight
    /// from the assembled authorities (the same facts a CLI/NL surface
    /// would render).
    ///
    /// # Errors
    ///
    /// Fails closed when a piece's read refuses (process-authority listing,
    /// pump health lane, artifact read probe).
    pub fn inspect_assembly(&self) -> SliceKResult<CellAssemblyInspect> {
        let bindings = self.runtime.process.list_active_process_bindings()?;
        // Read-only liveness probe of the artifact cache face: a cache-blob
        // read answers `Ok(None)` on a healthy store (a miss, never an
        // error), so this is the piece's read path responding.
        let artifact_store_responds = self
            .runtime
            .artifacts
            .get_cache_blob("cell-host-inspect-probe")
            .is_ok();
        Ok(CellAssemblyInspect {
            cell_identity: self.fence.identity(),
            node_boot_generation: self.fence.node_boot_generation(),
            epoch: self.fence.epoch(),
            fencing_token: self.fence.fencing_token(),
            quota_available: self.quota.available(),
            capacity_pool_remaining: self.capacity.pool_remaining(),
            device_head_free: self.device.is_free(),
            active_process_bindings: bindings.len(),
            gateway_health: self.gateway.health(),
            outbox_pump: self.runtime.pump_health(),
            artifact_store_responds,
            detector_epoch: self.detector.epoch(),
            detector_subjects: self.detector.subject_count(),
            detector_dead_judgments: self.detector.dead_judgments().len(),
            name_cache_epoch: self.cache.epoch(),
            name_cache_paths: self.cache.path_count(),
        })
    }

    /// Takes the next durable-clock tick for heartbeat/sweep evidence, at a
    /// never-reused idempotency key so every observation advances the
    /// monotone watermark by exactly one (the detector's clock-regression
    /// gate is therefore structurally satisfied).
    fn next_beat_tick(&mut self) -> SliceKResult<u64> {
        let sequence = self
            .beat_sequence
            .checked_add(1)
            .ok_or(SliceKError::CellHost(
                "durable heartbeat tick sequence exhausted",
            ))?;
        self.beat_sequence = sequence;
        let decision = self.runtime.clock.now(NowRequest {
            idempotency_key: beat_tick_key(sequence),
        })?;
        Ok(decision.reading().as_u64())
    }
}

/// The tick key of heartbeat/sweep evidence number `sequence`
/// (`cellbeat ‖ big-endian sequence`).
fn beat_tick_key(sequence: u64) -> IdempotencyKey {
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(CELL_BEAT_KEY_TAG);
    bytes[8..].copy_from_slice(&sequence.to_be_bytes());
    IdempotencyKey::from_bytes(bytes)
}

/// The one-line assembly digest this host publishes as its registration
/// description (the "pipeline 描述摘要" of the mechanism face; this slice
/// has no socket face).
const FEDERATION_DESCRIPTION: &str = "slice-k CellHost seven-piece assembly";

/// Builds this host's registration entry from a fence plus the monotone
/// heartbeat counter.
fn federation_registration(
    fence: &CellFence,
    os_process_id: u32,
    heartbeat: u64,
) -> SliceKResult<CellRegistrationEntry> {
    Ok(CellRegistrationEntry::new(
        fence.identity(),
        fence.node_boot_generation(),
        fence.epoch(),
        fence.fencing_token(),
        os_process_id,
        FEDERATION_DESCRIPTION,
    )?
    .with_heartbeat(heartbeat))
}

impl std::fmt::Debug for CellHost {
    /// The wrapped runtime and gateway are not `Debug`; the host prints its
    /// fence and evidence counter, marking the rest non-exhaustive.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("CellHost")
            .field("fence", &self.fence)
            .field("beat_sequence", &self.beat_sequence)
            .finish_non_exhaustive()
    }
}

/// Receipt of one accepted epoch advance: the new fence plus both
/// consumers' broadcast reports, in the order they were delivered.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CellEpochAdvance {
    /// The fence the authority advanced to (epoch and fencing token both
    /// moved).
    pub fence: CellFence,
    /// The failure detector's report: voided suspicions, subjects
    /// re-tracked after death, and the epoch boundary.
    pub detector: EpochAdvanceReport,
    /// The name cache's report: how many live entries the boundary fenced.
    pub name_cache: EpochInvalidation,
}

/// The typed incomplete state of a mid-chain broadcast refusal: the epoch
/// *has* advanced (the grantor's authority holds `advanced_to`), and these
/// fields name exactly which snapshot consumer had already accepted the
/// broadcast before the refusal. Nothing here is silently swallowed; the
/// state converges only by manual adjudication.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CellEpochBroadcastIncomplete {
    /// The fence the authority advanced to.
    pub advanced_to: CellFence,
    /// The detector's report when it accepted the broadcast before the
    /// refusal; `None` when the detector itself refused.
    pub detector: Option<EpochAdvanceReport>,
    /// Always `None` today — the name cache is broadcast last, so its
    /// report is present exactly when nothing refused.
    pub name_cache: Option<EpochInvalidation>,
    /// Which consumer refused, and why.
    pub refusal: CellBroadcastRefusal,
}

/// Which snapshot consumer refused an epoch-advanced broadcast, carrying
/// its typed refusal verbatim.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CellBroadcastRefusal {
    /// The failure detector's `on_epoch_advanced` refused.
    Detector(FailureDetectorError),
    /// The capability/name cache's `on_epoch_advanced` refused.
    Cache(NameCacheError),
}

impl std::fmt::Display for CellBroadcastRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Detector(error) => write!(formatter, "failure detector: {error}"),
            Self::Cache(error) => write!(formatter, "name cache: {error}"),
        }
    }
}

/// The seven-piece assembly facts, one per piece (see
/// [`CellHost::inspect_assembly`]). The three epoch axes (`epoch`,
/// `detector_epoch`, `name_cache_epoch`) are separate fields on purpose:
/// after a mid-chain [`SliceKError::CellBroadcast`] they are the drift the
/// manual adjudication needs to see.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CellAssemblyInspect {
    /// Stable identity of the assembled Cell.
    pub cell_identity: CellIdentity,
    /// Boot generation of this process claim against the cell data
    /// directory.
    pub node_boot_generation: Generation,
    /// Current epoch of the host's fence snapshot.
    pub epoch: CellEpoch,
    /// Current fencing token of the host's fence snapshot.
    pub fencing_token: CellFencingToken,
    /// Remaining AVAILABLE of the quota lease sub-ledger.
    pub quota_available: u64,
    /// Remaining source pool of the capacity lease sub-ledger (W49).
    pub capacity_pool_remaining: u64,
    /// Whether the exclusive device lease sub-ledger's head is still FREE
    /// (W49).
    pub device_head_free: bool,
    /// Active process bindings under the supervisor face.
    pub active_process_bindings: usize,
    /// Health of the driver gateway entry.
    pub gateway_health: CacheHealth,
    /// Health of the durable-outbox pump lane: `None` while no pump runs.
    pub outbox_pump: Option<PumpHealth>,
    /// Whether the artifact cache's read path answered (cache-blob probe:
    /// a miss is `Ok(None)`, never an error).
    pub artifact_store_responds: bool,
    /// Epoch the failure detector currently runs in.
    pub detector_epoch: CellEpoch,
    /// Subjects the failure detector currently tracks.
    pub detector_subjects: usize,
    /// Closed dead judgments in the detector's history.
    pub detector_dead_judgments: usize,
    /// Epoch the name cache currently serves.
    pub name_cache_epoch: CellEpoch,
    /// Paths the name cache tracks (live or tombstoned).
    pub name_cache_paths: usize,
}

impl CellAssemblyInspect {
    /// Stable `key=value` lines for demo/CLI inspect (grep-friendly), one
    /// line per piece of the seven-piece set.
    #[must_use]
    pub fn report_lines(&self) -> Vec<String> {
        let gateway = match self.gateway_health {
            CacheHealth::Healthy => "healthy".to_string(),
            CacheHealth::Degraded { .. } => "degraded".to_string(),
        };
        let pump = match &self.outbox_pump {
            Some(health) => format!("{:?}", health.state),
            None => "idle".to_string(),
        };
        vec![
            format!(
                "cell={} boot_generation={}",
                crate::short_hex(self.cell_identity.as_bytes()),
                self.node_boot_generation.get()
            ),
            format!(
                "fence epoch={} token={}",
                self.epoch.get(),
                self.fencing_token.get()
            ),
            format!("lease_quota_available={}", self.quota_available),
            format!("lease_capacity_pool={}", self.capacity_pool_remaining),
            format!(
                "lease_device_head={}",
                if self.device_head_free {
                    "free"
                } else {
                    "reserved"
                }
            ),
            format!("process_bindings={}", self.active_process_bindings),
            format!("gateway={gateway}"),
            format!("outbox_pump={pump}"),
            format!("artifact_store={}", self.artifact_store_responds),
            format!(
                "detector epoch={} subjects={} dead={}",
                self.detector_epoch.get(),
                self.detector_subjects,
                self.detector_dead_judgments
            ),
            format!(
                "name_cache epoch={} paths={}",
                self.name_cache_epoch.get(),
                self.name_cache_paths
            ),
        ]
    }
}

/// Fail-closed error returned by
/// [`CellHost::spend_quota_lease_with_local_autonomy`] (`[DIST-LOCAL-001]`,
/// `[LEASE-SPEND-001]`, `W65`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CellPartitionSpendError {
    /// Local autonomy gate rejected the action before any ledger mutation.
    Partition(PartitionViolation),
    /// Quota lease sub-ledger rejected the query, activation, or usage report.
    Ledger(QuotaLeaseLedgerError),
}

impl From<PartitionViolation> for CellPartitionSpendError {
    fn from(error: PartitionViolation) -> Self {
        Self::Partition(error)
    }
}

impl From<QuotaLeaseLedgerError> for CellPartitionSpendError {
    fn from(error: QuotaLeaseLedgerError) -> Self {
        Self::Ledger(error)
    }
}

impl std::fmt::Display for CellPartitionSpendError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Partition(error) => write!(formatter, "partition autonomy gate refused: {error}"),
            Self::Ledger(error) => write!(formatter, "QuotaLease ledger refused: {error}"),
        }
    }
}

impl std::error::Error for CellPartitionSpendError {}
