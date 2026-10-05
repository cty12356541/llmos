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

use std::path::Path;
use std::sync::Arc;

use nlos_cell::{
    CacheHit, CapabilityNameCache, CellAuthority, CellEpoch, CellFence, CellFencingToken,
    CellIdentity, EpochAdvanceReport, EpochInvalidation, FailureDetector, FailureDetectorConfig,
    FailureDetectorError, HeartbeatOutcome, InsertOutcome, InvalidationOutcome, MonitoredSubject,
    NameCacheError, NamePath, SweepReport,
};
use nlos_clock::NowRequest;
use nlos_driver_mock::{CacheHealth, MockProvider, ProviderCache};
use nlos_lease::{
    CapacityLeaseGrant, CapacityLeaseGrantor, DeviceResetReceipt, ExclusiveDeviceLeaseGrant,
    ExclusiveDeviceLeaseGrantor, LeaseInstant, QuotaLeaseGrant, QuotaLeaseGrantor,
};
use nlos_process::{MarkProcessTerminatedRequest, ProcessBindingRecord, ProcessTerminalRecord};
use nlos_runtime_tokio::PumpHealth;
use nlos_types::{
    CapabilityId, CapacityLeaseId, DeviceId, ExclusiveDeviceLeaseId, Generation, IdempotencyKey,
    ProcessId, QuotaLeaseId, SchedulerDomainId, TaskAttemptId, TaskId,
};

use crate::error::{SliceKError, SliceKResult};
use crate::runtime::{SliceKRuntime, seeded_key};

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
    /// runtime's operation store.
    ///
    /// # Errors
    ///
    /// Fails closed with the first refusal: a runtime authority open error,
    /// or [`SliceKError::Cell`] when this OS process already holds a Cell,
    /// the cell data directory is unusable, or its identity is bound to a
    /// different Cell.
    pub fn open(root: impl AsRef<Path>, config: CellHostConfig) -> SliceKResult<Self> {
        let runtime = SliceKRuntime::open(&root)?;
        let authority =
            CellAuthority::claim_with_data_dir(config.domain, runtime.root().join("cell"))?;
        let fence = authority.fence();
        let detector = FailureDetector::new(&fence, config.detector);
        let cache = CapabilityNameCache::new(&fence);
        let authority = authority.into_shared();
        let quota = QuotaLeaseGrantor::open(Arc::clone(&authority), config.quota_available);
        let capacity = CapacityLeaseGrantor::open(Arc::clone(&authority), config.capacity_pool);
        let device = ExclusiveDeviceLeaseGrantor::open(authority, config.device);
        let gateway =
            ProviderCache::new(Arc::new(MockProvider::new(Arc::clone(&runtime.operations))));
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
        })
    }

    /// The wrapped slice runtime — the assembled home of the process
    /// supervisor face, artifact cache, and outbox-pump lane (the pump
    /// starts and stops through the runtime's own lifecycle methods).
    #[must_use]
    pub fn runtime(&self) -> &SliceKRuntime {
        &self.runtime
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

    /// Whether the device lease sub-ledger's `DeviceLeaseHead` is still
    /// FREE (W49).
    #[must_use]
    pub const fn device_head_free(&self) -> bool {
        self.device.is_free()
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
        Ok(CellEpochAdvance {
            fence: new_fence,
            detector: detector_report,
            name_cache: name_cache_report,
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

    /// Spawns (or replays) the delegated process of one attempt through the
    /// wrapped runtime's supervisor face, and reports the registration to
    /// the failure detector as liveness evidence — the minimal heartbeat
    /// wiring: *registration observed = heartbeat*. A replayed
    /// registration is a fresh registration call and therefore fresh
    /// evidence.
    ///
    /// # Errors
    ///
    /// Propagates the process authority's fail-closed refusals and clock
    /// errors; the heartbeat itself can refuse only on logical-clock
    /// regression (typed [`SliceKError::FailureDetector`]).
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
        self.record_process_heartbeat(binding.process_id)?;
        Ok(binding)
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
    /// face. **No heartbeat is recorded** — a terminal observation is not
    /// liveness evidence; silence starts (or continues) and the detector's
    /// own sweep escalates it. Replaying the same marking is idempotent and
    /// equally heartbeat-free.
    ///
    /// # Errors
    ///
    /// Propagates the process authority's fail-closed refusals (stale
    /// fence, idempotency conflict) and clock errors.
    pub fn terminate_monitored_process(
        &mut self,
        binding: &ProcessBindingRecord,
        seed: u8,
    ) -> SliceKResult<ProcessTerminalRecord> {
        let marked_at_ms = self
            .runtime
            .wall_now_ms(seeded_key(seed, TERMINATE_WALL_KEY_OFFSET))?;
        Ok(self
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
            .clone())
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
