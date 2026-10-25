//! Concrete three-tier `Global -> Cell -> Worker` hierarchical scheduler,
//! `Context`-affinity placement router, safe work-stealing engine, and reverse
//! backpressure propagator (`§25.2.2`).
//!
//! Enforces:
//! - `[SCHED-HIER-001]` per-`Worker` local state for normal `enqueue`,
//!   `dequeue_next`, `wake_item`, and `complete_in_flight` hot paths without
//!   acquiring a global lock or scanning a global `AgentInstance` registry;
//! - `[SCHED-QUEUE-001]` bounded per-`ResourceGroup` WFQ / Deficit Round Robin
//!   queues with starvation aging, parent hard in-flight quotas, cancellation
//!   epoch fences, and full [`QueueItemInspection`];
//! - `[SCHED-STEAL-001]` atomic single-owner peer work stealing under canonical
//!   `worker_id` lock ordering that refuses non-migratable items, fenced
//!   `IsolationDomain`s, mismatched capabilities, and active `EffectPermit`,
//!   `Reservation`, or `ExclusiveDeviceLease` state;
//! - `[SCHED-BACKPRESSURE-001]` reverse `Worker -> Cell -> Global` pressure
//!   propagation shrinking `materialization_window`, returning
//!   [`SchedulerAdmission::WaitingResource`], and shedding cross-`Cell`
//!   placement from overloaded `Cell`s onto healthy `Cell`s.

use std::collections::{BTreeSet, HashMap};
use std::error::Error;
use std::fmt;
use std::sync::atomic::{AtomicU16, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use nlos_runtime::{
    AdmissionDirective, BackpressureSignal, DeadlineClass, DomainPressureLevel,
    QueueItemInspection, ResourceGroupQueuePolicy, SaturationSource, SchedulerAdmission,
    SchedulerDomainId, StealEligibility, StealRejectionReason, ThrottleReason, WorkStealEnvelope,
    WorkerStealCapabilities, evaluate_steal_eligibility,
};
use nlos_types::ExecutionFiberId;
use tokio::sync::Notify;

/// Work item submitted to a hierarchical `SchedulerDomain`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScheduledWorkItem {
    /// Stable work-item identity.
    pub item_id: [u8; 16],
    /// Optional `ExecutionFiberId` associated with this runnable activation.
    pub fiber_id: Option<ExecutionFiberId>,
    /// Owning `ResourceGroup` identifier.
    pub resource_group_id: [u8; 16],
    /// Caller-requested scheduling priority prior to policy clamping or aging.
    pub requested_priority: u8,
    /// Latency/deadline scheduling class.
    pub deadline_class: DeadlineClass,
    /// Monotonic cancellation epoch bound to this item (`0` if unfenced).
    pub cancel_epoch: u64,
    /// Monotonic enqueue timestamp in milliseconds.
    pub enqueued_at_ms: u64,
    /// Whether the item is currently runnable (`false` while parked awaiting a
    /// local `wake_item` handoff).
    pub runnable: bool,
    /// Security, lease, authority, and `Context` affinity envelope.
    pub steal_envelope: WorkStealEnvelope,
}

/// Work item popped for execution from a [`WorkerSchedulerDomain`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DispatchedWorkItem {
    /// The scheduled work item transferred to active execution.
    pub item: ScheduledWorkItem,
    /// `Worker` domain that dispatched the item.
    pub worker_domain: SchedulerDomainId,
    /// Starvation-aged effective priority at dispatch time.
    pub effective_priority: u8,
    /// Elapsed queue wait duration in milliseconds at dispatch time.
    pub starvation_age_ms: u64,
}

/// Capacity and window configuration for one [`WorkerSchedulerDomain`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerDomainConfig {
    /// Maximum concurrent in-flight items executing on this `Worker`.
    pub max_concurrency: usize,
    /// Baseline local materialization window contribution under `Nominal` load.
    pub base_materialization_window: usize,
}

/// Error returned when a cross-`Worker` steal invocation violates topology
/// preconditions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StealError {
    /// A `Worker` attempted to steal from itself.
    SelfSteal,
    /// Work stealing across distinct `Cell`s is forbidden; cross-`Cell`
    /// placement or takeover must traverse `Global` / `Cell` authorities.
    CrossCellStealForbidden,
    /// Requested `Worker` identity is not registered in the `Cell`.
    UnknownWorker([u8; 16]),
}

impl fmt::Display for StealError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SelfSteal => formatter.write_str("worker cannot steal from itself"),
            Self::CrossCellStealForbidden => {
                formatter.write_str("work stealing across distinct cells is forbidden")
            }
            Self::UnknownWorker(_) => formatter.write_str("worker is not registered in cell"),
        }
    }
}

impl Error for StealError {}

/// Structured outcome of a [`WorkerSchedulerDomain::steal_from_peer_worker`]
/// attempt (`[SCHED-STEAL-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StealReport {
    /// `Worker` domain attempting to steal work.
    pub thief_domain: SchedulerDomainId,
    /// Sibling `Worker` domain inspected as victim.
    pub victim_domain: SchedulerDomainId,
    /// Identity of the single work item atomically transferred, if any.
    pub stolen_item_id: Option<[u8; 16]>,
    /// Whether the stolen item matched a warm `Context` digest on the thief.
    pub context_locality_hit: bool,
    /// Ineligible items skipped and left intact in the victim queue along with
    /// their `[SCHED-STEAL-001]` rejection reasons.
    pub skipped_ineligible: Vec<([u8; 16], StealRejectionReason)>,
}

impl StealReport {
    #[must_use]
    const fn empty(thief_domain: SchedulerDomainId, victim_domain: SchedulerDomainId) -> Self {
        Self {
            thief_domain,
            victim_domain,
            stolen_item_id: None,
            context_locality_hit: false,
            skipped_ineligible: Vec::new(),
        }
    }
}

/// Lock-free per-`ResourceGroup` quota and cancellation fence shared across
/// sibling `Worker`s of a single `Cell` (`[SCHED-QUEUE-001]`).
struct ResourceGroupQuotaState {
    policy: ResourceGroupQueuePolicy,
    in_flight: AtomicUsize,
    fenced_cancel_epoch: AtomicU64,
}

impl ResourceGroupQuotaState {
    fn new(policy: ResourceGroupQueuePolicy) -> Self {
        Self {
            policy,
            in_flight: AtomicUsize::new(0),
            fenced_cancel_epoch: AtomicU64::new(0),
        }
    }

    fn current_in_flight(&self) -> usize {
        self.in_flight.load(Ordering::Acquire)
    }

    fn try_acquire_in_flight(&self) -> bool {
        let limit = self.policy.parent_hard_quota_in_flight;
        self.in_flight
            .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                if current < limit {
                    Some(current + 1)
                } else {
                    None
                }
            })
            .is_ok()
    }

    fn release_in_flight(&self) {
        let _ = self
            .in_flight
            .try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                Some(current.saturating_sub(1))
            });
    }

    fn fenced_cancel_epoch(&self) -> u64 {
        self.fenced_cancel_epoch.load(Ordering::Acquire)
    }

    fn advance_cancel_epoch(&self, epoch: u64) {
        self.fenced_cancel_epoch.fetch_max(epoch, Ordering::AcqRel);
    }
}

#[derive(Clone, Copy, Debug)]
struct QueuedEntry {
    item: ScheduledWorkItem,
    enqueue_seq: u64,
}

struct WorkerResourceGroupQueue {
    shared: Arc<ResourceGroupQuotaState>,
    items: Vec<QueuedEntry>,
    remaining_quantum: u32,
}

impl WorkerResourceGroupQueue {
    fn new(shared: Arc<ResourceGroupQuotaState>) -> Self {
        Self {
            shared,
            items: Vec::new(),
            remaining_quantum: 0,
        }
    }

    fn is_item_fenced(&self, item: &ScheduledWorkItem) -> bool {
        let fenced = self.shared.fenced_cancel_epoch();
        fenced > 0 && item.cancel_epoch <= fenced
    }

    fn purge_fenced(&mut self) -> usize {
        let fenced = self.shared.fenced_cancel_epoch();
        if fenced == 0 {
            return 0;
        }
        let before = self.items.len();
        self.items.retain(|entry| entry.item.cancel_epoch > fenced);
        before - self.items.len()
    }

    fn entry_score(&self, entry: &QueuedEntry, now_ms: u64) -> (u8, u8, u64, u64) {
        let age = now_ms.saturating_sub(entry.item.enqueued_at_ms);
        let effective = self
            .shared
            .policy
            .compute_effective_priority(entry.item.requested_priority, age);
        let urgency = entry.item.deadline_class.urgency_rank();
        // Invert `enqueue_seq` so standard tuple comparison breaks ties FIFO.
        (effective, urgency, age, u64::MAX - entry.enqueue_seq)
    }

    fn best_runnable_index(&self, now_ms: u64) -> Option<usize> {
        let fenced = self.shared.fenced_cancel_epoch();
        self.items
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                entry.item.runnable && (fenced == 0 || entry.item.cancel_epoch > fenced)
            })
            .max_by_key(|(_, entry)| self.entry_score(entry, now_ms))
            .map(|(idx, _)| idx)
    }

    fn runnable_count(&self) -> usize {
        let fenced = self.shared.fenced_cancel_epoch();
        self.items
            .iter()
            .filter(|entry| {
                entry.item.runnable && (fenced == 0 || entry.item.cancel_epoch > fenced)
            })
            .count()
    }
}

struct WorkerLocalState {
    group_order: Vec<[u8; 16]>,
    groups: HashMap<[u8; 16], WorkerResourceGroupQueue>,
    rr_cursor: usize,
    next_enqueue_seq: u64,
    worker_in_flight: usize,
    capabilities: WorkerStealCapabilities,
    saturation_sources: BTreeSet<SaturationSource>,
    critical_overload: bool,
    pressure_epoch: u64,
    current_signal: BackpressureSignal,
    published_signature: (DomainPressureLevel, Option<SaturationSource>, u8),
}

impl WorkerLocalState {
    fn total_queued_and_capacity(&self) -> (usize, usize) {
        let mut queued = 0usize;
        let mut capacity = 0usize;
        for group in self.groups.values() {
            queued = queued.saturating_add(group.items.len());
            capacity = capacity.saturating_add(group.shared.policy.max_queue_depth);
        }
        (queued, capacity)
    }

    fn total_runnable(&self) -> usize {
        self.groups
            .values()
            .map(WorkerResourceGroupQueue::runnable_count)
            .sum()
    }

    fn refresh_pressure(
        &mut self,
        domain_id: SchedulerDomainId,
        config: WorkerDomainConfig,
        force_publish: bool,
    ) -> Option<BackpressureSignal> {
        let (queued, queue_cap) = self.total_queued_and_capacity();
        let queue_bps = ratio_bps(queued, queue_cap);
        let concurrency_bps = ratio_bps(self.worker_in_flight, config.max_concurrency);
        let utilization_bps = queue_bps.max(concurrency_bps);

        let queue_level = if utilization_bps >= 9_500 {
            DomainPressureLevel::Critical
        } else if utilization_bps >= 8_000 {
            DomainPressureLevel::Saturated
        } else if utilization_bps >= 5_000 {
            DomainPressureLevel::Elevated
        } else {
            DomainPressureLevel::Nominal
        };

        let source_level = if self.critical_overload {
            DomainPressureLevel::Critical
        } else if !self.saturation_sources.is_empty() {
            DomainPressureLevel::Saturated
        } else {
            DomainPressureLevel::Nominal
        };

        let level = queue_level.max(source_level);
        let active_sources: Vec<SaturationSource> =
            self.saturation_sources.iter().copied().collect();
        let primary_source = active_sources.first().copied();
        let mask = sources_mask(&active_sources);
        let signature = (level, primary_source, mask);

        let window = BackpressureSignal::compute_recommended_window(
            config.base_materialization_window,
            level,
        );
        let directive = BackpressureSignal::directive_for_level(level);

        let transitioned = signature != self.published_signature || force_publish;
        if transitioned {
            self.pressure_epoch = self.pressure_epoch.saturating_add(1);
            self.published_signature = signature;
        }

        let signal = BackpressureSignal {
            origin_domain: domain_id,
            propagated_via: None,
            level,
            primary_source,
            active_sources,
            queue_utilization_bps: utilization_bps,
            recommended_materialization_window_limit: window,
            admission_directive: directive,
            epoch: self.pressure_epoch,
        };
        self.current_signal = signal.clone();

        if transitioned { Some(signal) } else { None }
    }
}

#[derive(Clone, Copy)]
struct StealCandidate {
    group_id: [u8; 16],
    item_idx: usize,
    context_locality_hit: bool,
    score: (u8, u8, u64, u64),
}

fn select_steal_candidate(
    thief_guard: &WorkerLocalState,
    victim_guard: &WorkerLocalState,
    now_ms: u64,
) -> (
    Option<StealCandidate>,
    Vec<([u8; 16], StealRejectionReason)>,
) {
    let mut skipped_ineligible: Vec<([u8; 16], StealRejectionReason)> = Vec::new();
    let mut best_candidate: Option<StealCandidate> = None;

    for group_id in &victim_guard.group_order {
        let Some(victim_group) = victim_guard.groups.get(group_id) else {
            continue;
        };
        let Some(thief_group) = thief_guard.groups.get(group_id) else {
            continue;
        };

        if thief_group.items.len() >= thief_group.shared.policy.max_queue_depth
            || thief_group.shared.current_in_flight()
                >= thief_group.shared.policy.parent_hard_quota_in_flight
        {
            continue;
        }

        for (idx, entry) in victim_group.items.iter().enumerate() {
            if !entry.item.runnable || victim_group.is_item_fenced(&entry.item) {
                continue;
            }
            match evaluate_steal_eligibility(&entry.item.steal_envelope, &thief_guard.capabilities)
            {
                StealEligibility::Eligible {
                    context_locality_hit,
                } => {
                    let score = victim_group.entry_score(entry, now_ms);
                    let replace = best_candidate.is_none_or(|prev| {
                        (context_locality_hit, score) > (prev.context_locality_hit, prev.score)
                    });
                    if replace {
                        best_candidate = Some(StealCandidate {
                            group_id: *group_id,
                            item_idx: idx,
                            context_locality_hit,
                            score,
                        });
                    }
                }
                StealEligibility::Rejected(reason) => {
                    skipped_ineligible.push((entry.item.item_id, reason));
                }
            }
        }
    }

    (best_candidate, skipped_ineligible)
}

type PressureSink = Arc<dyn Fn(BackpressureSignal) + Send + Sync>;

/// Per-`Worker` scheduler domain owning isolated local runnable queues
/// (`[SCHED-HIER-001]`, `[SCHED-QUEUE-001]`, `[SCHED-STEAL-001]`).
///
/// Normal `enqueue`, `dequeue_next`, `wake_item`, and `complete_in_flight`
/// operations lock only this `Worker`'s local state and never touch a global
/// lock or global `AgentInstance` table.
pub struct WorkerSchedulerDomain {
    domain_id: SchedulerDomainId,
    cell_id: [u8; 16],
    worker_id: [u8; 16],
    config: WorkerDomainConfig,
    local_state: Mutex<WorkerLocalState>,
    wake_notify: Notify,
    utilization_bps_atomic: AtomicU16,
    pressure_rank_atomic: AtomicU16,
    cell_materialized_counter: Option<Arc<AtomicUsize>>,
    parent_pressure_sink: Option<PressureSink>,
    local_enqueues: AtomicU64,
    local_dispatches: AtomicU64,
    local_wakes: AtomicU64,
    stolen_in_count: AtomicU64,
    stolen_out_count: AtomicU64,
}

impl WorkerSchedulerDomain {
    /// Creates a standalone [`WorkerSchedulerDomain`] for `(cell_id, worker_id)`.
    #[must_use]
    pub fn new(
        cell_id: [u8; 16],
        worker_id: [u8; 16],
        config: WorkerDomainConfig,
        capabilities: WorkerStealCapabilities,
    ) -> Self {
        Self::new_attached(cell_id, worker_id, config, capabilities, None, None)
    }

    fn new_attached(
        cell_id: [u8; 16],
        worker_id: [u8; 16],
        config: WorkerDomainConfig,
        capabilities: WorkerStealCapabilities,
        cell_materialized_counter: Option<Arc<AtomicUsize>>,
        parent_pressure_sink: Option<PressureSink>,
    ) -> Self {
        let domain_id = SchedulerDomainId::worker(cell_id, worker_id);
        let initial_signal =
            BackpressureSignal::nominal(domain_id, config.base_materialization_window);
        Self {
            domain_id,
            cell_id,
            worker_id,
            config,
            local_state: Mutex::new(WorkerLocalState {
                group_order: Vec::new(),
                groups: HashMap::new(),
                rr_cursor: 0,
                next_enqueue_seq: 0,
                worker_in_flight: 0,
                capabilities,
                saturation_sources: BTreeSet::new(),
                critical_overload: false,
                pressure_epoch: 0,
                current_signal: initial_signal,
                published_signature: (DomainPressureLevel::Nominal, None, 0),
            }),
            wake_notify: Notify::new(),
            utilization_bps_atomic: AtomicU16::new(0),
            pressure_rank_atomic: AtomicU16::new(pressure_level_rank(DomainPressureLevel::Nominal)),
            cell_materialized_counter,
            parent_pressure_sink,
            local_enqueues: AtomicU64::new(0),
            local_dispatches: AtomicU64::new(0),
            local_wakes: AtomicU64::new(0),
            stolen_in_count: AtomicU64::new(0),
            stolen_out_count: AtomicU64::new(0),
        }
    }

    /// Returns this `Worker`'s hierarchical [`SchedulerDomainId`].
    #[must_use]
    pub const fn domain_id(&self) -> SchedulerDomainId {
        self.domain_id
    }

    /// Returns the raw `worker_id` bytes.
    #[must_use]
    pub const fn worker_id(&self) -> [u8; 16] {
        self.worker_id
    }

    /// Returns the parent `cell_id` bytes.
    #[must_use]
    pub const fn cell_id(&self) -> [u8; 16] {
        self.cell_id
    }

    /// Registers a bounded `ResourceGroup` queue policy on this standalone `Worker`.
    pub fn register_resource_group(&self, policy: ResourceGroupQueuePolicy) {
        self.register_shared_resource_group(Arc::new(ResourceGroupQuotaState::new(policy)));
    }

    fn register_shared_resource_group(&self, shared: Arc<ResourceGroupQuotaState>) {
        let mut state = lock_unpoisoned(&self.local_state);
        let group_id = shared.policy.resource_group_id;
        if !state.groups.contains_key(&group_id) {
            state.group_order.push(group_id);
            state
                .groups
                .insert(group_id, WorkerResourceGroupQueue::new(shared));
        }
    }

    /// Records a warm `Context` digest on this `Worker` for affinity routing
    /// and locality-aware stealing.
    pub fn register_warm_context(&self, context_digest: [u8; 32]) {
        let mut state = lock_unpoisoned(&self.local_state);
        if !state
            .capabilities
            .warm_context_digests
            .contains(&context_digest)
        {
            state.capabilities.warm_context_digests.push(context_digest);
        }
    }

    /// Evicts a warm `Context` digest from this `Worker`.
    pub fn evict_warm_context(&self, context_digest: [u8; 32]) {
        let mut state = lock_unpoisoned(&self.local_state);
        state
            .capabilities
            .warm_context_digests
            .retain(|digest| *digest != context_digest);
    }

    /// Marks an `IsolationDomain` as fenced on this `Worker` (`[SCHED-STEAL-001]`).
    pub fn fence_isolation_domain(&self, isolation_domain_id: [u8; 16]) {
        let mut state = lock_unpoisoned(&self.local_state);
        state
            .capabilities
            .active_isolation_domains
            .retain(|id| *id != isolation_domain_id);
        if !state
            .capabilities
            .fenced_isolation_domains
            .contains(&isolation_domain_id)
        {
            state
                .capabilities
                .fenced_isolation_domains
                .push(isolation_domain_id);
        }
    }

    /// Reports or clears a downstream [`SaturationSource`] on this `Worker`,
    /// propagating any resulting [`BackpressureSignal`] upward (`Worker -> Cell -> Global`).
    pub fn set_saturation_source(&self, source: SaturationSource, active: bool) {
        let outbound = {
            let mut state = lock_unpoisoned(&self.local_state);
            let changed = if active {
                state.saturation_sources.insert(source)
            } else {
                state.saturation_sources.remove(&source)
            };
            let outbound = state.refresh_pressure(self.domain_id, self.config, changed);
            self.sync_pressure_atomics(&state.current_signal);
            outbound
        };
        self.emit_pressure(outbound);
    }

    /// Sets or clears an explicit `Critical` overload condition on this `Worker`.
    pub fn set_critical_overload(&self, critical: bool) {
        let outbound = {
            let mut state = lock_unpoisoned(&self.local_state);
            let changed = state.critical_overload != critical;
            state.critical_overload = critical;
            let outbound = state.refresh_pressure(self.domain_id, self.config, changed);
            self.sync_pressure_atomics(&state.current_signal);
            outbound
        };
        self.emit_pressure(outbound);
    }

    /// Enqueues a work item into this `Worker`'s local bounded `ResourceGroup`
    /// queue (`[SCHED-HIER-001]`, `[SCHED-QUEUE-001]`).
    pub fn enqueue(&self, item: ScheduledWorkItem, now_ms: u64) -> SchedulerAdmission {
        let _ = now_ms;
        let purged_cancelled: usize;
        let (admission, notify_runnable, outbound_signal) = {
            let mut state = lock_unpoisoned(&self.local_state);
            let Some(group) = state.groups.get_mut(&item.resource_group_id) else {
                return SchedulerAdmission::RejectedUnknownResourceGroup {
                    domain: self.domain_id,
                    resource_group_id: item.resource_group_id,
                };
            };

            purged_cancelled = group.purge_fenced();

            let fenced_epoch = group.shared.fenced_cancel_epoch();
            if fenced_epoch > 0 && item.cancel_epoch <= fenced_epoch {
                self.decrement_cell_materialized(purged_cancelled);
                return SchedulerAdmission::RejectedCancelled {
                    cancel_epoch: item.cancel_epoch,
                    fenced_epoch,
                };
            }

            let max_depth = group.shared.policy.max_queue_depth;
            if group.items.len() >= max_depth {
                let outbound = state.refresh_pressure(self.domain_id, self.config, false);
                self.sync_pressure_atomics(&state.current_signal);
                self.decrement_cell_materialized(purged_cancelled);
                drop(state);
                self.emit_pressure(outbound);
                return SchedulerAdmission::RejectedQueueFull {
                    domain: self.domain_id,
                    resource_group_id: item.resource_group_id,
                    max_queue_depth: max_depth,
                };
            }

            let effective_priority = group
                .shared
                .policy
                .compute_effective_priority(item.requested_priority, 0);
            let seq = state.next_enqueue_seq;
            state.next_enqueue_seq = state.next_enqueue_seq.saturating_add(1);
            if let Some(group) = state.groups.get_mut(&item.resource_group_id) {
                group.items.push(QueuedEntry {
                    item,
                    enqueue_seq: seq,
                });
            }

            let context_hit = item
                .steal_envelope
                .preferred_context_digest
                .is_some_and(|digest| state.capabilities.warm_context_digests.contains(&digest));
            let outbound = state.refresh_pressure(self.domain_id, self.config, false);
            let window_limit = state
                .current_signal
                .recommended_materialization_window_limit;
            self.sync_pressure_atomics(&state.current_signal);

            (
                SchedulerAdmission::Admitted {
                    assigned_domain: self.domain_id,
                    effective_priority,
                    context_affinity_hit: context_hit,
                    materialization_window_limit: window_limit,
                },
                item.runnable,
                outbound,
            )
        };

        self.decrement_cell_materialized(purged_cancelled);
        self.increment_cell_materialized(1);
        self.local_enqueues.fetch_add(1, Ordering::Relaxed);
        if notify_runnable {
            self.wake_notify.notify_one();
        }
        self.emit_pressure(outbound_signal);
        admission
    }

    /// Wakes a parked local item on this `Worker` without scanning global state
    /// (`[SCHED-HIER-001]`).
    pub fn wake_item(&self, item_id: [u8; 16]) -> bool {
        let woken = {
            let mut state = lock_unpoisoned(&self.local_state);
            let mut found = false;
            for group in state.groups.values_mut() {
                let fenced = group.shared.fenced_cancel_epoch();
                for entry in &mut group.items {
                    if entry.item.item_id == item_id {
                        if fenced > 0 && entry.item.cancel_epoch <= fenced {
                            return false;
                        }
                        entry.item.runnable = true;
                        found = true;
                        break;
                    }
                }
                if found {
                    break;
                }
            }
            found
        };

        if woken {
            self.local_wakes.fetch_add(1, Ordering::Relaxed);
            self.wake_notify.notify_one();
        }
        woken
    }

    /// Waits asynchronously until this `Worker` receives a runnable wake signal.
    pub async fn wait_for_runnable(&self) {
        let notified = self.wake_notify.notified();
        tokio::pin!(notified);
        if self.runnable_queue_len() > 0 {
            return;
        }
        notified.await;
    }

    /// Advances the cancellation epoch fence for `resource_group_id` and purges
    /// all queued items on this `Worker` with `cancel_epoch <= new_fenced_epoch`
    /// (`[SCHED-QUEUE-001]`).
    pub fn advance_cancel_epoch(
        &self,
        resource_group_id: [u8; 16],
        new_fenced_epoch: u64,
    ) -> usize {
        let (purged, outbound) = {
            let mut state = lock_unpoisoned(&self.local_state);
            let purged = if let Some(group) = state.groups.get_mut(&resource_group_id) {
                group.shared.advance_cancel_epoch(new_fenced_epoch);
                group.purge_fenced()
            } else {
                0
            };
            let outbound = state.refresh_pressure(self.domain_id, self.config, false);
            self.sync_pressure_atomics(&state.current_signal);
            (purged, outbound)
        };
        self.decrement_cell_materialized(purged);
        self.emit_pressure(outbound);
        purged
    }

    /// Pops the next runnable item for execution using Deficit Round Robin
    /// across `ResourceGroup`s and starvation-aged priority within the chosen
    /// group, enforcing both `Worker` concurrency and parent `ResourceGroup`
    /// hard in-flight quotas (`[SCHED-HIER-001]`, `[SCHED-QUEUE-001]`).
    pub fn dequeue_next(&self, now_ms: u64) -> Option<DispatchedWorkItem> {
        let mut purged_total = 0usize;
        let (dispatched, outbound) = {
            let mut state = lock_unpoisoned(&self.local_state);
            for group in state.groups.values_mut() {
                purged_total = purged_total.saturating_add(group.purge_fenced());
            }

            if state.worker_in_flight >= self.config.max_concurrency || state.group_order.is_empty()
            {
                let outbound = state.refresh_pressure(self.domain_id, self.config, false);
                self.sync_pressure_atomics(&state.current_signal);
                drop(state);
                self.decrement_cell_materialized(purged_total);
                self.emit_pressure(outbound);
                return None;
            }

            let num_groups = state.group_order.len();
            let mut chosen: Option<DispatchedWorkItem> = None;

            for _ in 0..(num_groups.saturating_mul(2)) {
                let cursor = state.rr_cursor % num_groups;
                let group_id = state.group_order[cursor];

                let Some(group) = state.groups.get_mut(&group_id) else {
                    state.rr_cursor = (cursor + 1) % num_groups;
                    continue;
                };

                let Some(best_idx) = group.best_runnable_index(now_ms) else {
                    group.remaining_quantum = 0;
                    state.rr_cursor = (cursor + 1) % num_groups;
                    continue;
                };

                if !group.shared.try_acquire_in_flight() {
                    group.remaining_quantum = 0;
                    state.rr_cursor = (cursor + 1) % num_groups;
                    continue;
                }

                if group.remaining_quantum == 0 {
                    group.remaining_quantum = group.shared.policy.weight.max(1);
                }

                let entry = group.items.remove(best_idx);
                let age = now_ms.saturating_sub(entry.item.enqueued_at_ms);
                let effective = group
                    .shared
                    .policy
                    .compute_effective_priority(entry.item.requested_priority, age);
                group.remaining_quantum = group.remaining_quantum.saturating_sub(1);
                let exhausted = group.remaining_quantum == 0;

                state.worker_in_flight = state.worker_in_flight.saturating_add(1);
                if exhausted {
                    state.rr_cursor = (cursor + 1) % num_groups;
                }

                chosen = Some(DispatchedWorkItem {
                    item: entry.item,
                    worker_domain: self.domain_id,
                    effective_priority: effective,
                    starvation_age_ms: age,
                });
                break;
            }

            let outbound = state.refresh_pressure(self.domain_id, self.config, false);
            self.sync_pressure_atomics(&state.current_signal);
            (chosen, outbound)
        };

        self.decrement_cell_materialized(purged_total);
        if dispatched.is_some() {
            self.local_dispatches.fetch_add(1, Ordering::Relaxed);
        }
        self.emit_pressure(outbound);
        dispatched
    }

    /// Releases the in-flight execution slot and `Cell` materialization count
    /// for a finished [`DispatchedWorkItem`].
    pub fn complete_dispatched(&self, dispatched: &DispatchedWorkItem) {
        self.complete_in_flight(dispatched.item.resource_group_id);
    }

    /// Releases one in-flight slot for `resource_group_id` on this `Worker`.
    pub fn complete_in_flight(&self, resource_group_id: [u8; 16]) {
        let outbound = {
            let mut state = lock_unpoisoned(&self.local_state);
            if let Some(group) = state.groups.get(&resource_group_id) {
                group.shared.release_in_flight();
            }
            state.worker_in_flight = state.worker_in_flight.saturating_sub(1);
            let outbound = state.refresh_pressure(self.domain_id, self.config, false);
            self.sync_pressure_atomics(&state.current_signal);
            outbound
        };
        self.decrement_cell_materialized(1);
        self.emit_pressure(outbound);
    }

    /// Inspects metadata, aged priority, dispatch position bounds, and active
    /// throttle reason for a queued item (`[SCHED-QUEUE-001]`).
    #[must_use]
    pub fn inspect_queue_item(
        &self,
        item_id: [u8; 16],
        now_ms: u64,
    ) -> Option<QueueItemInspection> {
        let state = lock_unpoisoned(&self.local_state);
        let num_groups = state.group_order.len();
        let active_cursor_group = if num_groups > 0 {
            Some(state.group_order[state.rr_cursor % num_groups])
        } else {
            None
        };

        let mut other_eligible_runnable = 0usize;
        let mut target_group_id: Option<[u8; 16]> = None;

        for (gid, group) in &state.groups {
            if group
                .items
                .iter()
                .any(|entry| entry.item.item_id == item_id)
            {
                target_group_id = Some(*gid);
            }
        }
        let target_gid = target_group_id?;

        for (gid, group) in &state.groups {
            if *gid != target_gid
                && group.shared.current_in_flight()
                    < group.shared.policy.parent_hard_quota_in_flight
            {
                other_eligible_runnable =
                    other_eligible_runnable.saturating_add(group.runnable_count());
            }
        }

        let group = state.groups.get(&target_gid)?;
        let target_entry = group
            .items
            .iter()
            .find(|entry| entry.item.item_id == item_id)?;

        let target_score = group.entry_score(target_entry, now_ms);
        let local_ahead = group
            .items
            .iter()
            .filter(|entry| {
                entry.item.item_id != item_id
                    && entry.item.runnable
                    && !group.is_item_fenced(&entry.item)
                    && group.entry_score(entry, now_ms) > target_score
            })
            .count();

        let starvation_age_ms = now_ms.saturating_sub(target_entry.item.enqueued_at_ms);
        let effective_priority = group
            .shared
            .policy
            .compute_effective_priority(target_entry.item.requested_priority, starvation_age_ms);

        let throttle_reason = if group.is_item_fenced(&target_entry.item) {
            Some(ThrottleReason::CancellationFenced)
        } else if group.shared.current_in_flight()
            >= group.shared.policy.parent_hard_quota_in_flight
        {
            Some(ThrottleReason::ParentHardQuotaReached)
        } else if state.worker_in_flight >= self.config.max_concurrency {
            Some(ThrottleReason::WorkerConcurrencyLimit)
        } else if let Some(source) = state.current_signal.primary_source {
            Some(ThrottleReason::DownstreamBackpressure(source))
        } else if other_eligible_runnable > 0 && active_cursor_group != Some(target_gid) {
            Some(ThrottleReason::ResourceGroupFairShareDeficit)
        } else {
            None
        };

        let optimistic_pos = local_ahead;
        let pessimistic_pos = local_ahead.saturating_add(other_eligible_runnable);

        Some(QueueItemInspection {
            item_id,
            requested_priority: target_entry.item.requested_priority,
            effective_priority,
            deadline_class: target_entry.item.deadline_class,
            weight: group.shared.policy.weight,
            queue_position_range: (optimistic_pos, pessimistic_pos),
            throttle_reason,
            starvation_age_ms,
            resource_group_id: target_entry.item.resource_group_id,
            cancel_epoch: target_entry.item.cancel_epoch,
        })
    }

    /// Atomically steals at most one eligible runnable item from sibling
    /// `peer_victim` in the same `Cell` (`[SCHED-STEAL-001]`).
    ///
    /// Acquires the two `Worker` locks in canonical ascending `worker_id` order
    /// to prevent AB-BA deadlocks under concurrent symmetric stealing. Any item
    /// rejected by [`evaluate_steal_eligibility`] is recorded in
    /// [`StealReport::skipped_ineligible`] and left intact in `peer_victim`.
    ///
    /// # Errors
    ///
    /// Returns [`StealError::SelfSteal`] if `self` and `peer_victim` share the
    /// same `worker_id`, or [`StealError::CrossCellStealForbidden`] if they
    /// belong to different `Cell`s.
    pub fn steal_from_peer_worker(
        &self,
        peer_victim: &Self,
        now_ms: u64,
    ) -> Result<StealReport, StealError> {
        if self.cell_id != peer_victim.cell_id {
            return Err(StealError::CrossCellStealForbidden);
        }
        if self.worker_id == peer_victim.worker_id {
            return Err(StealError::SelfSteal);
        }

        let mut victim_purged = 0usize;
        let mut thief_purged = 0usize;

        let (report, thief_outbound, victim_outbound) = {
            let (mut thief_guard, mut victim_guard) = lock_worker_pair(self, peer_victim);

            for group in victim_guard.groups.values_mut() {
                victim_purged = victim_purged.saturating_add(group.purge_fenced());
            }
            for group in thief_guard.groups.values_mut() {
                thief_purged = thief_purged.saturating_add(group.purge_fenced());
            }

            if thief_guard.worker_in_flight >= self.config.max_concurrency
                || thief_guard.current_signal.level >= DomainPressureLevel::Saturated
            {
                let t_out = thief_guard.refresh_pressure(self.domain_id, self.config, false);
                let v_out =
                    victim_guard.refresh_pressure(peer_victim.domain_id, peer_victim.config, false);
                self.sync_pressure_atomics(&thief_guard.current_signal);
                peer_victim.sync_pressure_atomics(&victim_guard.current_signal);
                drop(thief_guard);
                drop(victim_guard);
                peer_victim.decrement_cell_materialized(victim_purged);
                self.decrement_cell_materialized(thief_purged);
                self.emit_pressure(t_out);
                peer_victim.emit_pressure(v_out);
                return Ok(StealReport::empty(self.domain_id, peer_victim.domain_id));
            }

            let (best_candidate, skipped_ineligible) =
                select_steal_candidate(&thief_guard, &victim_guard, now_ms);

            let mut stolen_item_id = None;
            let mut locality_hit = false;

            if let Some(candidate) = best_candidate
                && let Some(victim_group) = victim_guard.groups.get_mut(&candidate.group_id)
            {
                let transferred = victim_group.items.remove(candidate.item_idx);
                let seq = thief_guard.next_enqueue_seq;
                thief_guard.next_enqueue_seq = thief_guard.next_enqueue_seq.saturating_add(1);
                if let Some(thief_group) = thief_guard.groups.get_mut(&candidate.group_id) {
                    stolen_item_id = Some(transferred.item.item_id);
                    locality_hit = candidate.context_locality_hit;
                    thief_group.items.push(QueuedEntry {
                        item: transferred.item,
                        enqueue_seq: seq,
                    });
                }
            }

            let t_out = thief_guard.refresh_pressure(self.domain_id, self.config, false);
            let v_out =
                victim_guard.refresh_pressure(peer_victim.domain_id, peer_victim.config, false);
            self.sync_pressure_atomics(&thief_guard.current_signal);
            peer_victim.sync_pressure_atomics(&victim_guard.current_signal);

            (
                StealReport {
                    thief_domain: self.domain_id,
                    victim_domain: peer_victim.domain_id,
                    stolen_item_id,
                    context_locality_hit: locality_hit,
                    skipped_ineligible,
                },
                t_out,
                v_out,
            )
        };

        peer_victim.decrement_cell_materialized(victim_purged);
        self.decrement_cell_materialized(thief_purged);

        if report.stolen_item_id.is_some() {
            self.stolen_in_count.fetch_add(1, Ordering::Relaxed);
            peer_victim.stolen_out_count.fetch_add(1, Ordering::Relaxed);
            self.wake_notify.notify_one();
        }

        self.emit_pressure(thief_outbound);
        peer_victim.emit_pressure(victim_outbound);

        Ok(report)
    }

    /// Returns the `Worker`'s current [`BackpressureSignal`] snapshot.
    #[must_use]
    pub fn current_backpressure_signal(&self) -> BackpressureSignal {
        lock_unpoisoned(&self.local_state).current_signal.clone()
    }

    /// Lock-free read of the `Worker`'s current [`DomainPressureLevel`].
    #[must_use]
    pub fn pressure_level(&self) -> DomainPressureLevel {
        rank_to_pressure_level(self.pressure_rank_atomic.load(Ordering::Acquire))
    }

    /// Lock-free read of the `Worker`'s queue/slot utilization in basis points.
    #[must_use]
    pub fn utilization_bps(&self) -> u16 {
        self.utilization_bps_atomic.load(Ordering::Acquire)
    }

    /// Returns the total runnable item count currently queued on this `Worker`.
    #[must_use]
    pub fn runnable_queue_len(&self) -> usize {
        lock_unpoisoned(&self.local_state).total_runnable()
    }

    /// Returns the total queued item count (runnable + parked) on this `Worker`.
    #[must_use]
    pub fn total_queued_len(&self) -> usize {
        lock_unpoisoned(&self.local_state)
            .total_queued_and_capacity()
            .0
    }

    /// Returns the number of items currently executing in flight on this `Worker`.
    #[must_use]
    pub fn in_flight_count(&self) -> usize {
        lock_unpoisoned(&self.local_state).worker_in_flight
    }

    /// Monotonic count of local `enqueue` admissions on this `Worker`.
    #[must_use]
    pub fn local_enqueues(&self) -> u64 {
        self.local_enqueues.load(Ordering::Acquire)
    }

    /// Monotonic count of local `dequeue_next` dispatches on this `Worker`.
    #[must_use]
    pub fn local_dispatches(&self) -> u64 {
        self.local_dispatches.load(Ordering::Acquire)
    }

    /// Monotonic count of local `wake_item` transitions on this `Worker`.
    #[must_use]
    pub fn local_wakes(&self) -> u64 {
        self.local_wakes.load(Ordering::Acquire)
    }

    /// Monotonic count of items stolen into this `Worker`.
    #[must_use]
    pub fn stolen_in_count(&self) -> u64 {
        self.stolen_in_count.load(Ordering::Acquire)
    }

    /// Monotonic count of items stolen out of this `Worker`.
    #[must_use]
    pub fn stolen_out_count(&self) -> u64 {
        self.stolen_out_count.load(Ordering::Acquire)
    }

    fn placement_score_for(&self, item: &ScheduledWorkItem) -> Option<(bool, usize)> {
        let state = lock_unpoisoned(&self.local_state);
        if state.current_signal.level >= DomainPressureLevel::Saturated {
            return None;
        }
        let group = state.groups.get(&item.resource_group_id)?;
        if group.items.len() >= group.shared.policy.max_queue_depth {
            return None;
        }
        if !worker_supports_initial_placement(&item.steal_envelope, &state.capabilities) {
            return None;
        }
        let warm_hit = item
            .steal_envelope
            .preferred_context_digest
            .is_some_and(|digest| state.capabilities.warm_context_digests.contains(&digest));
        let (queued, _) = state.total_queued_and_capacity();
        let load = queued.saturating_add(state.worker_in_flight);
        Some((warm_hit, load))
    }

    fn has_warm_context(&self, digest: [u8; 32]) -> bool {
        lock_unpoisoned(&self.local_state)
            .capabilities
            .warm_context_digests
            .contains(&digest)
    }

    fn sync_pressure_atomics(&self, signal: &BackpressureSignal) {
        self.utilization_bps_atomic
            .store(signal.queue_utilization_bps, Ordering::Release);
        self.pressure_rank_atomic
            .store(pressure_level_rank(signal.level), Ordering::Release);
    }

    fn emit_pressure(&self, outbound: Option<BackpressureSignal>) {
        if let (Some(signal), Some(sink)) = (outbound, &self.parent_pressure_sink) {
            sink(signal);
        }
    }

    fn increment_cell_materialized(&self, delta: usize) {
        if delta > 0
            && let Some(counter) = &self.cell_materialized_counter
        {
            counter.fetch_add(delta, Ordering::AcqRel);
        }
    }

    fn decrement_cell_materialized(&self, delta: usize) {
        if delta > 0
            && let Some(counter) = &self.cell_materialized_counter
        {
            let _ = counter.try_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                Some(current.saturating_sub(delta))
            });
        }
    }
}

fn lock_worker_pair<'a>(
    thief: &'a WorkerSchedulerDomain,
    victim: &'a WorkerSchedulerDomain,
) -> (
    MutexGuard<'a, WorkerLocalState>,
    MutexGuard<'a, WorkerLocalState>,
) {
    if thief.worker_id < victim.worker_id {
        let first = lock_unpoisoned(&thief.local_state);
        let second = lock_unpoisoned(&victim.local_state);
        (first, second)
    } else {
        let first = lock_unpoisoned(&victim.local_state);
        let second = lock_unpoisoned(&thief.local_state);
        (second, first)
    }
}

/// Checks whether a `Worker` satisfies security, tenant, authority, `Device`,
/// and `IsolationDomain` compatibility for initial `Cell` placement (distinct
/// from cross-`Worker` stealing, which additionally forbids non-migratable
/// items and active permits/leases).
fn worker_supports_initial_placement(
    envelope: &WorkStealEnvelope,
    capabilities: &WorkerStealCapabilities,
) -> bool {
    if envelope.isolation_fenced
        || capabilities
            .fenced_isolation_domains
            .contains(&envelope.isolation_domain_id)
    {
        return false;
    }
    if !capabilities
        .active_isolation_domains
        .contains(&envelope.isolation_domain_id)
    {
        return false;
    }
    if envelope.authority_term == 0
        || !capabilities
            .trusted_task_authorities
            .iter()
            .any(|(authority_id, min_term)| {
                *authority_id == envelope.task_authority_id && envelope.authority_term >= *min_term
            })
    {
        return false;
    }
    if !capabilities
        .allowed_namespaces
        .contains(&envelope.namespace_root)
    {
        return false;
    }
    if let Some(secret_scope) = envelope.secret_scope_id
        && !capabilities.allowed_secret_scopes.contains(&secret_scope)
    {
        return false;
    }
    if let Some(tenant) = envelope.provider_tenant_id
        && !capabilities.allowed_provider_tenants.contains(&tenant)
    {
        return false;
    }
    if let Some(device) = envelope.device_affinity
        && !capabilities.attached_devices.contains(&device)
    {
        return false;
    }
    true
}

struct CellState {
    worker_order: Vec<[u8; 16]>,
    workers: HashMap<[u8; 16], Arc<WorkerSchedulerDomain>>,
    resource_groups: HashMap<[u8; 16], Arc<ResourceGroupQuotaState>>,
    worker_signals: HashMap<[u8; 16], BackpressureSignal>,
    cell_signal: BackpressureSignal,
    signal_epoch: u64,
}

impl CellState {
    fn recompute_cell_signal(
        &mut self,
        cell_domain: SchedulerDomainId,
        base_window: usize,
    ) -> BackpressureSignal {
        let mut worst_level = DomainPressureLevel::Nominal;
        let mut worst_origin = cell_domain;
        let mut sources = BTreeSet::new();
        let mut max_bps = 0u16;

        for worker_id in &self.worker_order {
            if let Some(sig) = self.worker_signals.get(worker_id) {
                if sig.level > worst_level {
                    worst_level = sig.level;
                    worst_origin = sig.origin_domain;
                }
                max_bps = max_bps.max(sig.queue_utilization_bps);
                for src in &sig.active_sources {
                    sources.insert(*src);
                }
            }
        }

        // If every registered Worker in a non-empty Cell is Saturated or
        // Critical, the Cell itself is Saturated/Critical; if only a subset of
        // Workers is Saturated while healthy Workers remain, raise Cell to at
        // least Elevated and contract the Cell window proportionally.
        let total_workers = self.worker_order.len();
        let saturated_workers = self
            .worker_order
            .iter()
            .filter(|wid| {
                self.worker_signals
                    .get(*wid)
                    .is_some_and(|sig| sig.level >= DomainPressureLevel::Saturated)
            })
            .count();

        let effective_level = if total_workers == 0 {
            DomainPressureLevel::Nominal
        } else if saturated_workers == total_workers {
            worst_level
        } else if saturated_workers > 0 {
            DomainPressureLevel::Elevated
        } else {
            worst_level
        };

        let window =
            if total_workers > 0 && saturated_workers > 0 && saturated_workers < total_workers {
                let healthy = total_workers - saturated_workers;
                let scaled = (base_window.saturating_mul(healthy)) / total_workers;
                scaled.max(1)
            } else {
                BackpressureSignal::compute_recommended_window(base_window, effective_level)
            };

        self.signal_epoch = self.signal_epoch.saturating_add(1);
        let active_sources: Vec<SaturationSource> = sources.into_iter().collect();
        let primary_source = active_sources.first().copied();

        let signal = BackpressureSignal {
            origin_domain: if effective_level == DomainPressureLevel::Nominal {
                cell_domain
            } else {
                worst_origin
            },
            propagated_via: Some(cell_domain),
            level: effective_level,
            primary_source,
            active_sources,
            queue_utilization_bps: max_bps,
            recommended_materialization_window_limit: window,
            admission_directive: BackpressureSignal::directive_for_level(effective_level),
            epoch: self.signal_epoch,
        };
        self.cell_signal = signal.clone();
        signal
    }
}

/// `Cell`-tier scheduler domain managing `Worker` placement, `Context` affinity
/// routing, intra-`Cell` work stealing, and `Worker -> Cell` backpressure
/// aggregation (`§25.2.2`).
pub struct CellSchedulerDomain {
    domain_id: SchedulerDomainId,
    global_id: [u8; 16],
    cell_id: [u8; 16],
    base_materialization_window: usize,
    materialized_instances: Arc<AtomicUsize>,
    placement_lock_acquisitions: AtomicU64,
    state: Mutex<CellState>,
    parent_pressure_sink: Option<PressureSink>,
}

impl CellSchedulerDomain {
    /// Creates a [`CellSchedulerDomain`] under `global_id`.
    #[must_use]
    pub fn new(
        global_id: [u8; 16],
        cell_id: [u8; 16],
        base_materialization_window: usize,
    ) -> Arc<Self> {
        Self::new_attached(global_id, cell_id, base_materialization_window, None)
    }

    fn new_attached(
        global_id: [u8; 16],
        cell_id: [u8; 16],
        base_materialization_window: usize,
        parent_pressure_sink: Option<PressureSink>,
    ) -> Arc<Self> {
        let domain_id = SchedulerDomainId::cell(global_id, cell_id);
        Arc::new(Self {
            domain_id,
            global_id,
            cell_id,
            base_materialization_window,
            materialized_instances: Arc::new(AtomicUsize::new(0)),
            placement_lock_acquisitions: AtomicU64::new(0),
            state: Mutex::new(CellState {
                worker_order: Vec::new(),
                workers: HashMap::new(),
                resource_groups: HashMap::new(),
                worker_signals: HashMap::new(),
                cell_signal: BackpressureSignal::nominal(domain_id, base_materialization_window),
                signal_epoch: 0,
            }),
            parent_pressure_sink,
        })
    }

    /// Returns this `Cell`'s hierarchical [`SchedulerDomainId`].
    #[must_use]
    pub const fn domain_id(&self) -> SchedulerDomainId {
        self.domain_id
    }

    /// Returns the raw `cell_id` bytes.
    #[must_use]
    pub const fn cell_id(&self) -> [u8; 16] {
        self.cell_id
    }

    /// Returns the parent `global_id` bytes.
    #[must_use]
    pub const fn global_id(&self) -> [u8; 16] {
        self.global_id
    }

    /// Registers a `Cell`-wide `ResourceGroup` policy and attaches its shared
    /// hard-quota / cancellation-fence state to all `Worker`s in this `Cell`.
    pub fn register_resource_group(&self, policy: ResourceGroupQueuePolicy) {
        let shared = Arc::new(ResourceGroupQuotaState::new(policy));
        let workers: Vec<Arc<WorkerSchedulerDomain>> = {
            let mut state = lock_unpoisoned(&self.state);
            state
                .resource_groups
                .insert(policy.resource_group_id, Arc::clone(&shared));
            state.workers.values().cloned().collect()
        };
        for worker in workers {
            worker.register_shared_resource_group(Arc::clone(&shared));
        }
    }

    /// Registers a new [`WorkerSchedulerDomain`] inside this `Cell` and wires
    /// reverse `Worker -> Cell` pressure propagation.
    pub fn add_worker(
        self: &Arc<Self>,
        worker_id: [u8; 16],
        config: WorkerDomainConfig,
        capabilities: WorkerStealCapabilities,
    ) -> Arc<WorkerSchedulerDomain> {
        let cell_weak = Arc::downgrade(self);
        let sink: PressureSink = Arc::new(move |worker_signal| {
            if let Some(cell) = cell_weak.upgrade() {
                cell.on_worker_pressure(worker_id, worker_signal);
            }
        });

        let worker = Arc::new(WorkerSchedulerDomain::new_attached(
            self.cell_id,
            worker_id,
            config,
            capabilities,
            Some(Arc::clone(&self.materialized_instances)),
            Some(sink),
        ));

        let (shared_groups, cell_signal) = {
            let mut state = lock_unpoisoned(&self.state);
            let groups: Vec<Arc<ResourceGroupQuotaState>> =
                state.resource_groups.values().cloned().collect();
            if !state.workers.contains_key(&worker_id) {
                state.worker_order.push(worker_id);
            }
            state.worker_signals.insert(
                worker_id,
                BackpressureSignal::nominal(worker.domain_id(), config.base_materialization_window),
            );
            state.workers.insert(worker_id, Arc::clone(&worker));
            let recomputed =
                state.recompute_cell_signal(self.domain_id, self.base_materialization_window);
            (groups, recomputed)
        };

        for shared in shared_groups {
            worker.register_shared_resource_group(shared);
        }

        if let Some(parent_sink) = &self.parent_pressure_sink {
            parent_sink(cell_signal);
        }

        worker
    }

    /// Looks up a `Worker` registered in this `Cell`.
    #[must_use]
    pub fn worker(&self, worker_id: [u8; 16]) -> Option<Arc<WorkerSchedulerDomain>> {
        lock_unpoisoned(&self.state)
            .workers
            .get(&worker_id)
            .cloned()
    }

    /// Advances the `Cell`-wide cancellation fence for `resource_group_id` and
    /// purges fenced items across all `Worker`s in this `Cell`.
    pub fn advance_cancel_epoch(
        &self,
        resource_group_id: [u8; 16],
        new_fenced_epoch: u64,
    ) -> usize {
        let workers: Vec<Arc<WorkerSchedulerDomain>> = {
            let state = lock_unpoisoned(&self.state);
            if let Some(shared) = state.resource_groups.get(&resource_group_id) {
                shared.advance_cancel_epoch(new_fenced_epoch);
            }
            state.workers.values().cloned().collect()
        };
        workers
            .into_iter()
            .map(|worker| worker.advance_cancel_epoch(resource_group_id, new_fenced_epoch))
            .sum()
    }

    /// Places a runnable item onto the optimal `Worker` in this `Cell`:
    /// enforces `[SCHED-BACKPRESSURE-001]` `materialization_window` and
    /// `WaitingResource` gating, prefers a healthy `Worker` caching the item's
    /// `preferred_context_digest`, and falls back cleanly to sibling healthy
    /// `Worker`s when the warm `Worker` saturates.
    pub fn place_runnable(&self, item: ScheduledWorkItem, now_ms: u64) -> SchedulerAdmission {
        self.placement_lock_acquisitions
            .fetch_add(1, Ordering::Relaxed);
        let (cell_signal, workers) = {
            let state = lock_unpoisoned(&self.state);
            let ordered: Vec<Arc<WorkerSchedulerDomain>> = state
                .worker_order
                .iter()
                .filter_map(|wid| state.workers.get(wid).cloned())
                .collect();
            (state.cell_signal.clone(), ordered)
        };

        let window_limit = cell_signal.recommended_materialization_window_limit;
        let active_materialized = self.active_materialized_count();

        if cell_signal.level == DomainPressureLevel::Critical
            || (item.deadline_class != DeadlineClass::Immediate
                && (cell_signal.admission_directive == AdmissionDirective::WaitingResource
                    || active_materialized >= window_limit))
            || (cell_signal.admission_directive == AdmissionDirective::ThrottleBackground
                && item.deadline_class == DeadlineClass::BackgroundBatch)
        {
            return SchedulerAdmission::WaitingResource {
                reporting_domain: self.domain_id,
                level: cell_signal.level,
                source: cell_signal.primary_source,
                materialization_window_limit: window_limit,
            };
        }

        // Rank compatible healthy Workers: warm Context hit first, then lowest load.
        let mut candidates: Vec<(bool, usize, Arc<WorkerSchedulerDomain>)> = workers
            .into_iter()
            .filter_map(|worker| {
                let (warm_hit, load) = worker.placement_score_for(&item)?;
                Some((warm_hit, load, worker))
            })
            .collect();

        candidates.sort_by(|left, right| {
            right
                .0
                .cmp(&left.0)
                .then_with(|| left.1.cmp(&right.1))
                .then_with(|| left.2.worker_id().cmp(&right.2.worker_id()))
        });

        for (_, _, worker) in candidates {
            match worker.enqueue(item, now_ms) {
                SchedulerAdmission::Admitted {
                    assigned_domain,
                    effective_priority,
                    context_affinity_hit,
                    ..
                } => {
                    return SchedulerAdmission::Admitted {
                        assigned_domain,
                        effective_priority,
                        context_affinity_hit,
                        materialization_window_limit: window_limit,
                    };
                }
                SchedulerAdmission::RejectedCancelled {
                    cancel_epoch,
                    fenced_epoch,
                } => {
                    return SchedulerAdmission::RejectedCancelled {
                        cancel_epoch,
                        fenced_epoch,
                    };
                }
                SchedulerAdmission::RejectedUnknownResourceGroup {
                    domain,
                    resource_group_id,
                } => {
                    return SchedulerAdmission::RejectedUnknownResourceGroup {
                        domain,
                        resource_group_id,
                    };
                }
                SchedulerAdmission::RejectedQueueFull { .. }
                | SchedulerAdmission::WaitingResource { .. } => {}
            }
        }

        let refreshed = self.current_backpressure_signal();
        SchedulerAdmission::WaitingResource {
            reporting_domain: self.domain_id,
            level: refreshed.level.max(DomainPressureLevel::Saturated),
            source: refreshed.primary_source,
            materialization_window_limit: refreshed.recommended_materialization_window_limit,
        }
    }

    /// Drives safe intra-`Cell` work stealing for `thief_worker_id` across
    /// overloaded sibling `Worker`s ordered by descending runnable backlog.
    ///
    /// # Errors
    ///
    /// Returns [`StealError::UnknownWorker`] if `thief_worker_id` is not
    /// registered in this `Cell`.
    pub fn steal_from_overloaded_peers(
        &self,
        thief_worker_id: [u8; 16],
        now_ms: u64,
    ) -> Result<StealReport, StealError> {
        let (thief, mut peers) = {
            let state = lock_unpoisoned(&self.state);
            let thief = state
                .workers
                .get(&thief_worker_id)
                .cloned()
                .ok_or(StealError::UnknownWorker(thief_worker_id))?;
            let peers: Vec<(usize, Arc<WorkerSchedulerDomain>)> = state
                .worker_order
                .iter()
                .filter(|wid| **wid != thief_worker_id)
                .filter_map(|wid| state.workers.get(wid).cloned())
                .map(|peer| (peer.runnable_queue_len(), peer))
                .filter(|(backlog, _)| *backlog > 0)
                .collect();
            (thief, peers)
        };

        peers.sort_by(|left, right| {
            right
                .0
                .cmp(&left.0)
                .then_with(|| left.1.worker_id().cmp(&right.1.worker_id()))
        });

        let mut aggregated_skipped = Vec::new();
        let mut last_victim = thief.domain_id();

        for (_, victim) in peers {
            last_victim = victim.domain_id();
            let report = thief.steal_from_peer_worker(&victim, now_ms)?;
            if report.stolen_item_id.is_some() {
                let mut combined = aggregated_skipped;
                combined.extend(report.skipped_ineligible);
                return Ok(StealReport {
                    skipped_ineligible: combined,
                    ..report
                });
            }
            aggregated_skipped.extend(report.skipped_ineligible);
        }

        Ok(StealReport {
            thief_domain: thief.domain_id(),
            victim_domain: last_victim,
            stolen_item_id: None,
            context_locality_hit: false,
            skipped_ineligible: aggregated_skipped,
        })
    }

    /// Returns whether any healthy `Worker` in this `Cell` holds `digest` warm.
    #[must_use]
    pub fn has_healthy_warm_context(&self, digest: [u8; 32]) -> bool {
        let workers: Vec<Arc<WorkerSchedulerDomain>> = lock_unpoisoned(&self.state)
            .workers
            .values()
            .cloned()
            .collect();
        workers.into_iter().any(|worker| {
            worker.pressure_level() < DomainPressureLevel::Saturated
                && worker.has_warm_context(digest)
        })
    }

    /// Returns the `Cell`'s current aggregated [`BackpressureSignal`].
    #[must_use]
    pub fn current_backpressure_signal(&self) -> BackpressureSignal {
        lock_unpoisoned(&self.state).cell_signal.clone()
    }

    /// Returns the `Cell`'s current effective `materialization_window` ceiling.
    #[must_use]
    pub fn effective_materialization_window(&self) -> usize {
        self.current_backpressure_signal()
            .recommended_materialization_window_limit
    }

    /// Returns the current number of materialized items queued or in flight
    /// across this `Cell`.
    #[must_use]
    pub fn active_materialized_count(&self) -> usize {
        self.materialized_instances.load(Ordering::Acquire)
    }

    /// Monotonic count of `Cell`-level placement lock acquisitions.
    #[must_use]
    pub fn placement_lock_acquisitions(&self) -> u64 {
        self.placement_lock_acquisitions.load(Ordering::Acquire)
    }

    fn on_worker_pressure(&self, worker_id: [u8; 16], signal: BackpressureSignal) {
        let cell_signal = {
            let mut state = lock_unpoisoned(&self.state);
            if let Some(existing) = state.worker_signals.get(&worker_id)
                && signal.epoch < existing.epoch
            {
                return;
            }
            state.worker_signals.insert(worker_id, signal);
            state.recompute_cell_signal(self.domain_id, self.base_materialization_window)
        };
        if let Some(parent_sink) = &self.parent_pressure_sink {
            parent_sink(cell_signal);
        }
    }
}

struct GlobalState {
    cell_order: Vec<[u8; 16]>,
    cells: HashMap<[u8; 16], Arc<CellSchedulerDomain>>,
    cell_signals: HashMap<[u8; 16], BackpressureSignal>,
    global_signal: BackpressureSignal,
    signal_epoch: u64,
}

impl GlobalState {
    fn recompute_global_signal(
        &mut self,
        global_domain: SchedulerDomainId,
        base_window: usize,
    ) -> BackpressureSignal {
        let total_cells = self.cell_order.len();
        let mut worst_level = DomainPressureLevel::Nominal;
        let mut worst_origin = global_domain;
        let mut sources = BTreeSet::new();
        let mut max_bps = 0u16;
        let mut saturated_cells = 0usize;
        let mut sum_cell_windows = 0usize;
        let mut sum_cell_bases = 0usize;

        for cell_id in &self.cell_order {
            if let Some(cell) = self.cells.get(cell_id) {
                sum_cell_bases = sum_cell_bases.saturating_add(cell.base_materialization_window);
            }
            if let Some(sig) = self.cell_signals.get(cell_id) {
                if sig.level > worst_level {
                    worst_level = sig.level;
                    worst_origin = sig.origin_domain;
                }
                if sig.level >= DomainPressureLevel::Saturated {
                    saturated_cells = saturated_cells.saturating_add(1);
                }
                max_bps = max_bps.max(sig.queue_utilization_bps);
                sum_cell_windows =
                    sum_cell_windows.saturating_add(sig.recommended_materialization_window_limit);
                for src in &sig.active_sources {
                    sources.insert(*src);
                }
            }
        }

        let effective_level = if total_cells == 0 {
            DomainPressureLevel::Nominal
        } else if saturated_cells == total_cells {
            worst_level
        } else if saturated_cells > 0 {
            DomainPressureLevel::Elevated
        } else {
            worst_level
        };

        let window = if total_cells == 0 || sum_cell_bases == 0 {
            BackpressureSignal::compute_recommended_window(base_window, effective_level)
        } else if effective_level == DomainPressureLevel::Critical {
            0
        } else {
            let scaled = (base_window.saturating_mul(sum_cell_windows)) / sum_cell_bases;
            if base_window > 0 && scaled == 0 {
                1
            } else {
                scaled.min(base_window)
            }
        };

        self.signal_epoch = self.signal_epoch.saturating_add(1);
        let active_sources: Vec<SaturationSource> = sources.into_iter().collect();
        let primary_source = active_sources.first().copied();

        let signal = BackpressureSignal {
            origin_domain: if effective_level == DomainPressureLevel::Nominal {
                global_domain
            } else {
                worst_origin
            },
            propagated_via: Some(global_domain),
            level: effective_level,
            primary_source,
            active_sources,
            queue_utilization_bps: max_bps,
            recommended_materialization_window_limit: window,
            admission_directive: BackpressureSignal::directive_for_level(effective_level),
            epoch: self.signal_epoch,
        };
        self.global_signal = signal.clone();
        signal
    }
}

/// Top-level `Global` control scheduler domain managing cross-`Cell` placement,
/// peer `Cell` pressure summaries, and `Worker -> Cell -> Global` reverse
/// backpressure contraction (`[SCHED-HIER-001]`, `[SCHED-BACKPRESSURE-001]`).
pub struct GlobalSchedulerDomain {
    domain_id: SchedulerDomainId,
    raw_id: [u8; 16],
    base_materialization_window: usize,
    control_lock_acquisitions: AtomicU64,
    state: Mutex<GlobalState>,
}

impl GlobalSchedulerDomain {
    /// Creates a new [`GlobalSchedulerDomain`].
    #[must_use]
    pub fn new(domain_id: [u8; 16], base_materialization_window: usize) -> Arc<Self> {
        let hierarchical_id = SchedulerDomainId::global(domain_id);
        Arc::new(Self {
            domain_id: hierarchical_id,
            raw_id: domain_id,
            base_materialization_window,
            control_lock_acquisitions: AtomicU64::new(0),
            state: Mutex::new(GlobalState {
                cell_order: Vec::new(),
                cells: HashMap::new(),
                cell_signals: HashMap::new(),
                global_signal: BackpressureSignal::nominal(
                    hierarchical_id,
                    base_materialization_window,
                ),
                signal_epoch: 0,
            }),
        })
    }

    /// Returns this `Global` domain's [`SchedulerDomainId`].
    #[must_use]
    pub const fn domain_id(&self) -> SchedulerDomainId {
        self.domain_id
    }

    /// Registers a new [`CellSchedulerDomain`] under this `Global` domain and
    /// wires reverse `Cell -> Global` backpressure propagation.
    pub fn add_cell(
        self: &Arc<Self>,
        cell_id: [u8; 16],
        cell_base_materialization_window: usize,
    ) -> Arc<CellSchedulerDomain> {
        let global_weak = Arc::downgrade(self);
        let sink: PressureSink = Arc::new(move |cell_signal| {
            if let Some(global) = global_weak.upgrade() {
                global.on_cell_pressure(cell_id, cell_signal);
            }
        });

        let cell = CellSchedulerDomain::new_attached(
            self.raw_id,
            cell_id,
            cell_base_materialization_window,
            Some(sink),
        );

        self.control_lock_acquisitions
            .fetch_add(1, Ordering::Relaxed);
        let mut state = lock_unpoisoned(&self.state);
        if !state.cells.contains_key(&cell_id) {
            state.cell_order.push(cell_id);
        }
        state.cell_signals.insert(
            cell_id,
            BackpressureSignal::nominal(cell.domain_id(), cell_base_materialization_window),
        );
        state.cells.insert(cell_id, Arc::clone(&cell));
        state.recompute_global_signal(self.domain_id, self.base_materialization_window);
        cell
    }

    /// Looks up a registered [`CellSchedulerDomain`].
    #[must_use]
    pub fn cell(&self, cell_id: [u8; 16]) -> Option<Arc<CellSchedulerDomain>> {
        self.control_lock_acquisitions
            .fetch_add(1, Ordering::Relaxed);
        lock_unpoisoned(&self.state).cells.get(&cell_id).cloned()
    }

    /// Inspects the latest [`BackpressureSignal`] summary received from `cell_id`.
    #[must_use]
    pub fn inspect_cell_pressure(&self, cell_id: [u8; 16]) -> Option<BackpressureSignal> {
        self.control_lock_acquisitions
            .fetch_add(1, Ordering::Relaxed);
        lock_unpoisoned(&self.state)
            .cell_signals
            .get(&cell_id)
            .cloned()
    }

    /// Places a runnable item across registered `Cell`s, consulting peer `Cell`
    /// pressure summaries so overloaded `Cell`s shed placement onto healthy
    /// `Cell`s and returning [`SchedulerAdmission::WaitingResource`] when all
    /// compatible `Cell`s are saturated (`[SCHED-BACKPRESSURE-001]`).
    pub fn place_across_cells(
        &self,
        item: ScheduledWorkItem,
        preferred_cell_id: Option<[u8; 16]>,
        now_ms: u64,
    ) -> SchedulerAdmission {
        self.control_lock_acquisitions
            .fetch_add(1, Ordering::Relaxed);
        let (global_signal, cell_entries) = {
            let state = lock_unpoisoned(&self.state);
            let entries: Vec<([u8; 16], BackpressureSignal, Arc<CellSchedulerDomain>)> = state
                .cell_order
                .iter()
                .filter_map(|cid| {
                    let cell = state.cells.get(cid).cloned()?;
                    let sig = state.cell_signals.get(cid).cloned()?;
                    Some((*cid, sig, cell))
                })
                .collect();
            (state.global_signal.clone(), entries)
        };

        let global_window = global_signal.recommended_materialization_window_limit;
        let total_active: usize = cell_entries
            .iter()
            .map(|(_, _, cell)| cell.active_materialized_count())
            .sum();

        if global_signal.level == DomainPressureLevel::Critical
            || (item.deadline_class != DeadlineClass::Immediate
                && (global_signal.admission_directive == AdmissionDirective::WaitingResource
                    || total_active >= global_window))
        {
            return SchedulerAdmission::WaitingResource {
                reporting_domain: self.domain_id,
                level: global_signal.level,
                source: global_signal.primary_source,
                materialization_window_limit: global_window,
            };
        }

        let healthy_cells = rank_healthy_cells(cell_entries, &item, preferred_cell_id);

        for candidate in healthy_cells {
            match candidate.cell.place_runnable(item, now_ms) {
                SchedulerAdmission::Admitted {
                    assigned_domain,
                    effective_priority,
                    context_affinity_hit,
                    materialization_window_limit,
                } => {
                    return SchedulerAdmission::Admitted {
                        assigned_domain,
                        effective_priority,
                        context_affinity_hit,
                        materialization_window_limit: materialization_window_limit
                            .min(global_window),
                    };
                }
                SchedulerAdmission::RejectedCancelled {
                    cancel_epoch,
                    fenced_epoch,
                } => {
                    return SchedulerAdmission::RejectedCancelled {
                        cancel_epoch,
                        fenced_epoch,
                    };
                }
                SchedulerAdmission::WaitingResource { .. }
                | SchedulerAdmission::RejectedQueueFull { .. }
                | SchedulerAdmission::RejectedUnknownResourceGroup { .. } => {}
            }
        }

        let refreshed = self.current_backpressure_signal();
        SchedulerAdmission::WaitingResource {
            reporting_domain: self.domain_id,
            level: refreshed.level.max(DomainPressureLevel::Saturated),
            source: refreshed.primary_source,
            materialization_window_limit: refreshed.recommended_materialization_window_limit,
        }
    }

    /// Returns the `Global` domain's current [`BackpressureSignal`].
    #[must_use]
    pub fn current_backpressure_signal(&self) -> BackpressureSignal {
        self.control_lock_acquisitions
            .fetch_add(1, Ordering::Relaxed);
        lock_unpoisoned(&self.state).global_signal.clone()
    }

    /// Returns the `Global` effective `materialization_window` ceiling.
    #[must_use]
    pub fn effective_materialization_window(&self) -> usize {
        self.current_backpressure_signal()
            .recommended_materialization_window_limit
    }

    /// Monotonic count of `Global` control-plane lock acquisitions (used to
    /// prove zero global lock acquisitions during `Worker` local hot-path runs).
    #[must_use]
    pub fn control_lock_acquisitions(&self) -> u64 {
        self.control_lock_acquisitions.load(Ordering::Acquire)
    }

    fn on_cell_pressure(&self, cell_id: [u8; 16], signal: BackpressureSignal) {
        self.control_lock_acquisitions
            .fetch_add(1, Ordering::Relaxed);
        let mut state = lock_unpoisoned(&self.state);
        if let Some(existing) = state.cell_signals.get(&cell_id)
            && signal.epoch < existing.epoch
        {
            return;
        }
        state.cell_signals.insert(cell_id, signal);
        state.recompute_global_signal(self.domain_id, self.base_materialization_window);
    }
}

/// Unified three-tier `Global -> Cell -> Worker` scheduler handle (`§25.2.2`).
#[derive(Clone)]
pub struct ThreeTierScheduler {
    global: Arc<GlobalSchedulerDomain>,
}

impl ThreeTierScheduler {
    /// Creates a new [`ThreeTierScheduler`] rooted at `global_domain_id`.
    #[must_use]
    pub fn new(global_domain_id: [u8; 16], base_materialization_window: usize) -> Self {
        Self {
            global: GlobalSchedulerDomain::new(global_domain_id, base_materialization_window),
        }
    }

    /// Returns the root [`GlobalSchedulerDomain`].
    #[must_use]
    pub fn global(&self) -> &Arc<GlobalSchedulerDomain> {
        &self.global
    }

    /// Registers a new [`CellSchedulerDomain`] under the root `Global` domain.
    #[must_use]
    pub fn add_cell(
        &self,
        cell_id: [u8; 16],
        cell_base_materialization_window: usize,
    ) -> Arc<CellSchedulerDomain> {
        self.global
            .add_cell(cell_id, cell_base_materialization_window)
    }

    /// Looks up a registered [`CellSchedulerDomain`].
    #[must_use]
    pub fn cell(&self, cell_id: [u8; 16]) -> Option<Arc<CellSchedulerDomain>> {
        self.global.cell(cell_id)
    }

    /// Looks up a registered [`WorkerSchedulerDomain`] inside `cell_id`.
    #[must_use]
    pub fn worker(
        &self,
        cell_id: [u8; 16],
        worker_id: [u8; 16],
    ) -> Option<Arc<WorkerSchedulerDomain>> {
        self.global.cell(cell_id)?.worker(worker_id)
    }

    /// Places a runnable item across the `Global -> Cell -> Worker` hierarchy.
    #[must_use]
    pub fn place_across_cells(
        &self,
        item: ScheduledWorkItem,
        preferred_cell_id: Option<[u8; 16]>,
        now_ms: u64,
    ) -> SchedulerAdmission {
        self.global
            .place_across_cells(item, preferred_cell_id, now_ms)
    }
}

struct RankedCellCandidate {
    is_preferred: bool,
    has_warm_context: bool,
    level: DomainPressureLevel,
    utilization_bps: u16,
    cell_id: [u8; 16],
    cell: Arc<CellSchedulerDomain>,
}

fn rank_healthy_cells(
    cell_entries: Vec<([u8; 16], BackpressureSignal, Arc<CellSchedulerDomain>)>,
    item: &ScheduledWorkItem,
    preferred_cell_id: Option<[u8; 16]>,
) -> Vec<RankedCellCandidate> {
    let mut healthy_cells: Vec<RankedCellCandidate> = cell_entries
        .into_iter()
        .filter(|(_, sig, _)| sig.level < DomainPressureLevel::Saturated)
        .map(|(cid, sig, cell)| {
            let is_preferred = preferred_cell_id == Some(cid);
            let has_warm_context = item
                .steal_envelope
                .preferred_context_digest
                .is_some_and(|digest| cell.has_healthy_warm_context(digest));
            RankedCellCandidate {
                is_preferred,
                has_warm_context,
                level: sig.level,
                utilization_bps: sig.queue_utilization_bps,
                cell_id: cid,
                cell,
            }
        })
        .collect();

    healthy_cells.sort_by(|left, right| {
        right
            .is_preferred
            .cmp(&left.is_preferred)
            .then_with(|| right.has_warm_context.cmp(&left.has_warm_context))
            .then_with(|| left.level.cmp(&right.level))
            .then_with(|| left.utilization_bps.cmp(&right.utilization_bps))
            .then_with(|| left.cell_id.cmp(&right.cell_id))
    });
    healthy_cells
}

fn ratio_bps(numerator: usize, denominator: usize) -> u16 {
    if denominator == 0 {
        return 0;
    }
    let scaled = numerator.saturating_mul(10_000) / denominator;
    u16::try_from(scaled.min(10_000)).unwrap_or(10_000)
}

const fn source_bit(source: SaturationSource) -> u8 {
    match source {
        SaturationSource::ModelSlot => 1 << 0,
        SaturationSource::TopicBacklog => 1 << 1,
        SaturationSource::ContextResidency => 1 << 2,
        SaturationSource::StorageIo => 1 << 3,
        SaturationSource::DriverPool => 1 << 4,
    }
}

fn sources_mask(sources: &[SaturationSource]) -> u8 {
    let mut mask = 0u8;
    for source in sources {
        mask |= source_bit(*source);
    }
    mask
}

const fn pressure_level_rank(level: DomainPressureLevel) -> u16 {
    match level {
        DomainPressureLevel::Nominal => 0,
        DomainPressureLevel::Elevated => 1,
        DomainPressureLevel::Saturated => 2,
        DomainPressureLevel::Critical => 3,
    }
}

const fn rank_to_pressure_level(rank: u16) -> DomainPressureLevel {
    match rank {
        0 => DomainPressureLevel::Nominal,
        1 => DomainPressureLevel::Elevated,
        2 => DomainPressureLevel::Saturated,
        _ => DomainPressureLevel::Critical,
    }
}

fn lock_unpoisoned<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
