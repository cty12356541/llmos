//! Runtime-independent contracts for the three-tier `Global -> Cell -> Worker`
//! hierarchical `SchedulerDomain` (`§25.2.2`).
//!
//! Covers:
//! - `[SCHED-HIER-001]` hierarchical domain identity and lineage inspection;
//! - `[SCHED-QUEUE-001]` bounded per-`ResourceGroup` queue metadata, deadline
//!   classes, starvation aging, cancellation fencing, and queue inspection;
//! - `[SCHED-STEAL-001]` deterministic work-stealing eligibility and `Context`
//!   affinity validation that strictly forbids duplicating active
//!   `EffectPermit`, `Reservation`, or `ExclusiveDeviceLease` state;
//! - `[SCHED-BACKPRESSURE-001]` reverse `Worker -> Cell -> Global` pressure
//!   signals, `materialization_window` contraction, and `WaitingResource`
//!   admission directives.

use nlos_types::SchedulerDomainId as NominalSchedulerDomainId;

/// Execution tier of a [`SchedulerDomainId`] (`§25.2.2`).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum SchedulerTier {
    /// Global control scheduler: tenant fairness, `Application`/`Task`
    /// priority, `Cell` placement, and global quota leases.
    Global,
    /// `Cell` scheduler: local capacity, `TaskAuthority`, bulkheads,
    /// `TaskGroup` queues, and cross-`Worker` placement.
    Cell,
    /// `Worker` scheduler: runnable `Process`/`AgentInstance`/`ExecutionFiber`
    /// queues, `Context` affinity, and local backpressure.
    Worker,
}

/// Three-tier hierarchical scheduler domain identity (`[SCHED-HIER-001]`).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum SchedulerDomainId {
    /// Top-level global scheduler domain.
    Global { domain_id: [u8; 16] },
    /// `Cell`-scoped scheduler domain bound to a parent `Global` domain.
    Cell {
        global_id: [u8; 16],
        cell_id: [u8; 16],
    },
    /// `Worker`-scoped scheduler domain bound to a parent `Cell` domain.
    Worker {
        cell_id: [u8; 16],
        worker_id: [u8; 16],
    },
}

impl SchedulerDomainId {
    /// Constructs a `Global` scheduler domain identifier.
    #[must_use]
    pub const fn global(domain_id: [u8; 16]) -> Self {
        Self::Global { domain_id }
    }

    /// Constructs a `Cell` scheduler domain identifier under `global_id`.
    #[must_use]
    pub const fn cell(global_id: [u8; 16], cell_id: [u8; 16]) -> Self {
        Self::Cell { global_id, cell_id }
    }

    /// Constructs a `Worker` scheduler domain identifier under `cell_id`.
    #[must_use]
    pub const fn worker(cell_id: [u8; 16], worker_id: [u8; 16]) -> Self {
        Self::Worker { cell_id, worker_id }
    }

    /// Returns the [`SchedulerTier`] of this domain.
    #[must_use]
    pub const fn tier(&self) -> SchedulerTier {
        match self {
            Self::Global { .. } => SchedulerTier::Global,
            Self::Cell { .. } => SchedulerTier::Cell,
            Self::Worker { .. } => SchedulerTier::Worker,
        }
    }

    /// Returns `true` if this domain is at the [`SchedulerTier::Global`] tier.
    #[must_use]
    pub const fn is_global(&self) -> bool {
        matches!(self, Self::Global { .. })
    }

    /// Returns `true` if this domain is at the [`SchedulerTier::Cell`] tier.
    #[must_use]
    pub const fn is_cell(&self) -> bool {
        matches!(self, Self::Cell { .. })
    }

    /// Returns `true` if this domain is at the [`SchedulerTier::Worker`] tier.
    #[must_use]
    pub const fn is_worker(&self) -> bool {
        matches!(self, Self::Worker { .. })
    }

    /// Returns the owning or self `cell_id` for `Cell` and `Worker` domains.
    #[must_use]
    pub const fn cell_id(&self) -> Option<[u8; 16]> {
        match self {
            Self::Global { .. } => None,
            Self::Cell { cell_id, .. } | Self::Worker { cell_id, .. } => Some(*cell_id),
        }
    }

    /// Returns the directly encoded `global_id` for `Global` and `Cell` domains.
    #[must_use]
    pub const fn global_id(&self) -> Option<[u8; 16]> {
        match self {
            Self::Global { domain_id } => Some(*domain_id),
            Self::Cell { global_id, .. } => Some(*global_id),
            Self::Worker { .. } => None,
        }
    }

    /// Returns the `worker_id` when this domain is a `Worker`.
    #[must_use]
    pub const fn worker_id(&self) -> Option<[u8; 16]> {
        match self {
            Self::Worker { worker_id, .. } => Some(*worker_id),
            Self::Global { .. } | Self::Cell { .. } => None,
        }
    }

    /// Returns `true` when this domain is the `Cell` `target_cell_id` or a
    /// `Worker` directly parented to `target_cell_id`.
    #[must_use]
    pub fn belongs_to_cell(&self, target_cell_id: [u8; 16]) -> bool {
        match self {
            Self::Global { .. } => false,
            Self::Cell { cell_id, .. } | Self::Worker { cell_id, .. } => *cell_id == target_cell_id,
        }
    }

    /// Returns `true` when this domain directly encodes lineage to
    /// `target_global_id` (`Global` self-match or `Cell` parent match).
    ///
    /// Because a bare `Worker` identifier carries `(cell_id, worker_id)`
    /// without duplicating `global_id`, use [`Self::belongs_to_global_via`] to
    /// verify two-hop `Worker -> Cell -> Global` lineage against a `Cell`
    /// descriptor.
    #[must_use]
    pub fn belongs_to_global(&self, target_global_id: [u8; 16]) -> bool {
        match self {
            Self::Global { domain_id } => *domain_id == target_global_id,
            Self::Cell { global_id, .. } => *global_id == target_global_id,
            Self::Worker { .. } => false,
        }
    }

    /// Verifies lineage to `target_global_id`, resolving `Worker` membership
    /// through `parent_cell`.
    #[must_use]
    pub fn belongs_to_global_via(&self, target_global_id: [u8; 16], parent_cell: &Self) -> bool {
        match self {
            Self::Global { .. } | Self::Cell { .. } => self.belongs_to_global(target_global_id),
            Self::Worker { cell_id, .. } => match parent_cell {
                Self::Cell {
                    global_id,
                    cell_id: parent_cell_id,
                } => *cell_id == *parent_cell_id && *global_id == target_global_id,
                Self::Global { .. } | Self::Worker { .. } => false,
            },
        }
    }

    /// Converts the leaf identifier of this hierarchical domain into the
    /// nominal [`nlos_types::SchedulerDomainId`] used on [`crate::FiberSpec`].
    #[must_use]
    pub const fn leaf_nominal_id(&self) -> NominalSchedulerDomainId {
        match self {
            Self::Global { domain_id } => NominalSchedulerDomainId::from_bytes(*domain_id),
            Self::Cell { cell_id, .. } => NominalSchedulerDomainId::from_bytes(*cell_id),
            Self::Worker { worker_id, .. } => NominalSchedulerDomainId::from_bytes(*worker_id),
        }
    }
}

/// Latency and deadline scheduling class (`[SCHED-QUEUE-001]`).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum DeadlineClass {
    /// System-critical or immediate interactive deadline.
    Immediate,
    /// User-facing interactive turn with tight latency expectations.
    Interactive,
    /// Standard asynchronous task execution.
    Standard,
    /// Opportunistic background or batch work subject to early throttling.
    BackgroundBatch,
}

impl DeadlineClass {
    /// Higher numeric rank indicates stricter latency urgency.
    #[must_use]
    pub const fn urgency_rank(self) -> u8 {
        match self {
            Self::Immediate => 3,
            Self::Interactive => 2,
            Self::Standard => 1,
            Self::BackgroundBatch => 0,
        }
    }
}

/// Source of downstream resource saturation (`[SCHED-BACKPRESSURE-001]`).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum SaturationSource {
    /// Model inference concurrency slots are saturated.
    ModelSlot,
    /// Event `Topic` or `Channel` consumer backlog reached high watermark.
    TopicBacklog,
    /// `Context` / KV-cache working-set residency reached memory limits.
    ContextResidency,
    /// Durable storage I/O bandwidth or queue depth is saturated.
    StorageIo,
    /// External tool or device `Driver` connection pool is saturated.
    DriverPool,
}

/// Explicit reason a queued item is currently throttled from immediate dispatch
/// (`[SCHED-QUEUE-001]`).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ThrottleReason {
    /// The owning `ResourceGroup` reached its parent hard in-flight quota.
    ParentHardQuotaReached,
    /// Another `ResourceGroup` currently holds the WFQ/DRR fair-share turn.
    ResourceGroupFairShareDeficit,
    /// Downstream saturation is applying backpressure on dispatch.
    DownstreamBackpressure(SaturationSource),
    /// The `Cell` or `Global` materialization window has contracted to zero
    /// remaining headroom.
    MaterializationWindowClosed,
    /// The item's `cancel_epoch` was fenced by a newer cancellation epoch.
    CancellationFenced,
    /// The local `Worker` reached its active concurrency slot limit.
    WorkerConcurrencyLimit,
}

/// Bounded queue and WFQ/aging policy for one `ResourceGroup` inside a
/// `SchedulerDomain` (`[SCHED-QUEUE-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResourceGroupQueuePolicy {
    /// Stable `ResourceGroup` identifier bytes.
    pub resource_group_id: [u8; 16],
    /// Maximum queued items permitted per `Worker` queue for this group.
    pub max_queue_depth: usize,
    /// Parent hard ceiling on concurrent in-flight items across the `Cell`.
    pub parent_hard_quota_in_flight: usize,
    /// Relative WFQ / Deficit Round Robin quantum weight (minimum effective 1).
    pub weight: u32,
    /// Ceiling applied to caller-supplied `requested_priority` at admission
    /// (`[CTRL-PRIORITY-001]`).
    pub max_priority_cap: u8,
    /// Wait duration in milliseconds per +1 starvation-aging priority boost
    /// (`0` disables aging).
    pub starvation_aging_step_ms: u64,
}

impl ResourceGroupQueuePolicy {
    /// Computes the starvation-aged effective priority at `starvation_age_ms`.
    #[must_use]
    pub fn compute_effective_priority(&self, requested_priority: u8, starvation_age_ms: u64) -> u8 {
        let base = requested_priority.min(self.max_priority_cap);
        if self.starvation_aging_step_ms == 0 {
            return base;
        }
        let steps = starvation_age_ms / self.starvation_aging_step_ms;
        let boost = u8::try_from(steps).unwrap_or(u8::MAX);
        base.saturating_add(boost)
    }
}

/// Observable metadata snapshot for a queued item (`[SCHED-QUEUE-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QueueItemInspection {
    /// Stable work-item identity.
    pub item_id: [u8; 16],
    /// Caller-requested priority prior to policy clamping or aging.
    pub requested_priority: u8,
    /// Current effective priority after policy cap and starvation aging.
    pub effective_priority: u8,
    /// Latency/deadline scheduling class.
    pub deadline_class: DeadlineClass,
    /// Owning `ResourceGroup` WFQ/DRR weight.
    pub weight: u32,
    /// Inclusive `(optimistic, pessimistic)` zero-based dispatch position range
    /// across the `Worker`'s `ResourceGroup` queues.
    pub queue_position_range: (usize, usize),
    /// Active throttle cause if the item cannot immediately dispatch.
    pub throttle_reason: Option<ThrottleReason>,
    /// Elapsed wait time in milliseconds since enqueue.
    pub starvation_age_ms: u64,
    /// Owning `ResourceGroup` identifier.
    pub resource_group_id: [u8; 16],
    /// Item cancellation epoch checked against the group's cancellation fence.
    pub cancel_epoch: u64,
}

/// Optional `Context` residency and `Device` locality hints (`[SCHED-STEAL-001]`).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ContextAffinitySpec {
    /// Digest of a warm `Context` / KV-prefix cache preferred for placement.
    pub preferred_context_digest: Option<[u8; 32]>,
    /// Optional `Device` attachment required to run the item.
    pub device_affinity: Option<[u8; 16]>,
}

impl ContextAffinitySpec {
    /// Empty affinity specification.
    #[must_use]
    pub const fn none() -> Self {
        Self {
            preferred_context_digest: None,
            device_affinity: None,
        }
    }

    /// Affinity specification preferring a warm `Context` digest.
    #[must_use]
    pub const fn with_context(digest: [u8; 32]) -> Self {
        Self {
            preferred_context_digest: Some(digest),
            device_affinity: None,
        }
    }

    /// Affinity specification requiring a specific attached `Device`.
    #[must_use]
    pub const fn with_device(device_id: [u8; 16]) -> Self {
        Self {
            preferred_context_digest: None,
            device_affinity: Some(device_id),
        }
    }
}

/// Security, lease, authority, and locality envelope checked before cross-`Worker`
/// migration or work stealing (`[SCHED-STEAL-001]`).
#[allow(clippy::struct_excessive_bools)] // Canonical [SCHED-STEAL-001] safety flags are independent boolean invariants.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkStealEnvelope {
    /// Whether the `TaskNode`/`TaskAttempt` permits cross-`Worker` migration.
    pub migratable: bool,
    /// Required `Namespace` root digest.
    pub namespace_root: [u8; 32],
    /// Optional secret scope required to unseal credentials on the `Worker`.
    pub secret_scope_id: Option<[u8; 16]>,
    /// Optional warm `Context` digest preferred by this item.
    pub preferred_context_digest: Option<[u8; 32]>,
    /// Optional hardware/driver `Device` affinity requirement.
    pub device_affinity: Option<[u8; 16]>,
    /// True if the item holds an active `ExclusiveDeviceLease` (never stealable).
    pub has_active_device_lease: bool,
    /// True if the item holds an active `EffectPermit` (never stealable).
    pub has_active_effect_permit: bool,
    /// True if the item holds an active `Reservation` (never stealable).
    pub has_active_reservation: bool,
    /// Optional model/tool provider tenant boundary.
    pub provider_tenant_id: Option<[u8; 16]>,
    /// Owning `TaskAuthority` identifier.
    pub task_authority_id: [u8; 16],
    /// Monotonic `TaskAuthority` term/epoch (`0` is invalid).
    pub authority_term: u64,
    /// `IsolationDomain` hosting the item.
    pub isolation_domain_id: [u8; 16],
    /// Whether the item's `IsolationDomain` has been fenced.
    pub isolation_fenced: bool,
}

impl WorkStealEnvelope {
    /// Extracts the [`ContextAffinitySpec`] projection of this envelope.
    #[must_use]
    pub const fn context_affinity(&self) -> ContextAffinitySpec {
        ContextAffinitySpec {
            preferred_context_digest: self.preferred_context_digest,
            device_affinity: self.device_affinity,
        }
    }
}

/// Target `Worker` security, lease, authority, and locality capabilities checked
/// by [`evaluate_steal_eligibility`] (`[SCHED-STEAL-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkerStealCapabilities {
    /// Mounted `Namespace` roots accessible on the target `Worker`.
    pub allowed_namespaces: Vec<[u8; 32]>,
    /// Secret scopes provisioned on the target `Worker`.
    pub allowed_secret_scopes: Vec<[u8; 16]>,
    /// Hardware/virtual `Device`s attached to the target `Worker`.
    pub attached_devices: Vec<[u8; 16]>,
    /// Provider tenant boundaries authorized on the target `Worker`.
    pub allowed_provider_tenants: Vec<[u8; 16]>,
    /// Trusted `(task_authority_id, min_valid_authority_term)` pairs in this `Cell`.
    pub trusted_task_authorities: Vec<([u8; 16], u64)>,
    /// Unfenced `IsolationDomain`s admitted on the target `Worker`.
    pub active_isolation_domains: Vec<[u8; 16]>,
    /// `IsolationDomain`s known by the target `Worker` to be fenced.
    pub fenced_isolation_domains: Vec<[u8; 16]>,
    /// Warm `Context` digests currently resident on the target `Worker`.
    pub warm_context_digests: Vec<[u8; 32]>,
    /// When `true`, refuses stealing items with `preferred_context_digest` if
    /// the target `Worker` lacks that warm `Context`.
    pub require_warm_context_on_steal: bool,
}

/// Deterministic `[SCHED-STEAL-001]` rejection cause.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum StealRejectionReason {
    /// `migratable == false`.
    NotMigratable,
    /// Stealing an active `EffectPermit` would risk duplicate side effects.
    ActiveEffectPermit,
    /// Stealing an active `Reservation` would split or duplicate budget custody.
    ActiveReservation,
    /// Stealing an active `ExclusiveDeviceLease` would violate device exclusivity.
    ActiveExclusiveDeviceLease,
    /// The item's `IsolationDomain` is fenced.
    IsolationDomainFenced,
    /// The target `Worker` does not host the item's `IsolationDomain`.
    IsolationDomainNotPermitted,
    /// The target `Worker` does not recognize `task_authority_id` or `authority_term` is stale.
    TaskAuthorityMismatch,
    /// The target `Worker` does not mount `namespace_root`.
    NamespaceMismatch,
    /// The target `Worker` lacks access to `secret_scope_id`.
    SecretScopeMismatch,
    /// The target `Worker` is not authorized for `provider_tenant_id`.
    ProviderTenantMismatch,
    /// The target `Worker` lacks the required `device_affinity`.
    DeviceAffinityUnsatisfied,
    /// Policy required warm `Context` residency on steal and target missed.
    ContextLocalityUnavailable,
}

/// Outcome of [`evaluate_steal_eligibility`] (`[SCHED-STEAL-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StealEligibility {
    /// Item may safely migrate to the target `Worker` without duplicating
    /// non-copyable leases or permits.
    Eligible { context_locality_hit: bool },
    /// Item must remain on its current `Worker`.
    Rejected(StealRejectionReason),
}

impl StealEligibility {
    /// Returns `true` when the item is eligible for migration/stealing.
    #[must_use]
    pub const fn is_eligible(&self) -> bool {
        matches!(self, Self::Eligible { .. })
    }

    /// Returns the [`StealRejectionReason`] if ineligible.
    #[must_use]
    pub const fn rejection_reason(&self) -> Option<StealRejectionReason> {
        match self {
            Self::Eligible { .. } => None,
            Self::Rejected(reason) => Some(*reason),
        }
    }
}

/// Pure deterministic `[SCHED-STEAL-001]` validator checking whether `item`
/// may be stolen by or migrated onto a `Worker` advertising
/// `target_worker_capabilities`.
#[must_use]
pub fn evaluate_steal_eligibility(
    item: &WorkStealEnvelope,
    target_worker_capabilities: &WorkerStealCapabilities,
) -> StealEligibility {
    if !item.migratable {
        return StealEligibility::Rejected(StealRejectionReason::NotMigratable);
    }
    if item.has_active_effect_permit {
        return StealEligibility::Rejected(StealRejectionReason::ActiveEffectPermit);
    }
    if item.has_active_reservation {
        return StealEligibility::Rejected(StealRejectionReason::ActiveReservation);
    }
    if item.has_active_device_lease {
        return StealEligibility::Rejected(StealRejectionReason::ActiveExclusiveDeviceLease);
    }
    if item.isolation_fenced
        || target_worker_capabilities
            .fenced_isolation_domains
            .contains(&item.isolation_domain_id)
    {
        return StealEligibility::Rejected(StealRejectionReason::IsolationDomainFenced);
    }
    if !target_worker_capabilities
        .active_isolation_domains
        .contains(&item.isolation_domain_id)
    {
        return StealEligibility::Rejected(StealRejectionReason::IsolationDomainNotPermitted);
    }
    if item.authority_term == 0
        || !target_worker_capabilities
            .trusted_task_authorities
            .iter()
            .any(|(authority_id, min_term)| {
                *authority_id == item.task_authority_id && item.authority_term >= *min_term
            })
    {
        return StealEligibility::Rejected(StealRejectionReason::TaskAuthorityMismatch);
    }
    if !target_worker_capabilities
        .allowed_namespaces
        .contains(&item.namespace_root)
    {
        return StealEligibility::Rejected(StealRejectionReason::NamespaceMismatch);
    }
    if let Some(secret_scope) = item.secret_scope_id
        && !target_worker_capabilities
            .allowed_secret_scopes
            .contains(&secret_scope)
    {
        return StealEligibility::Rejected(StealRejectionReason::SecretScopeMismatch);
    }
    if let Some(tenant) = item.provider_tenant_id
        && !target_worker_capabilities
            .allowed_provider_tenants
            .contains(&tenant)
    {
        return StealEligibility::Rejected(StealRejectionReason::ProviderTenantMismatch);
    }
    if let Some(device) = item.device_affinity
        && !target_worker_capabilities
            .attached_devices
            .contains(&device)
    {
        return StealEligibility::Rejected(StealRejectionReason::DeviceAffinityUnsatisfied);
    }

    let context_locality_hit = item.preferred_context_digest.is_some_and(|digest| {
        target_worker_capabilities
            .warm_context_digests
            .contains(&digest)
    });
    if target_worker_capabilities.require_warm_context_on_steal
        && item.preferred_context_digest.is_some()
        && !context_locality_hit
    {
        return StealEligibility::Rejected(StealRejectionReason::ContextLocalityUnavailable);
    }

    StealEligibility::Eligible {
        context_locality_hit,
    }
}

/// Severity of saturation in a `SchedulerDomain` (`[SCHED-BACKPRESSURE-001]`).
#[derive(Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum DomainPressureLevel {
    /// Normal operating headroom; full materialization window available.
    #[default]
    Nominal,
    /// Rising utilization; shrink materialization window and throttle batch work.
    Elevated,
    /// Downstream resource or runnable queue saturated; halt non-critical
    /// materialization and return `WaitingResource`.
    Saturated,
    /// Emergency overload; close materialization window to zero for new births.
    Critical,
}

/// Planner / admission directive carried by [`BackpressureSignal`]
/// (`[SCHED-BACKPRESSURE-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdmissionDirective {
    /// Normal admission permitted up to the active materialization window.
    Accept,
    /// Throttle `BackgroundBatch` admissions when window headroom is tight.
    ThrottleBackground,
    /// Transition non-critical `TaskNode`s to `WAITING_RESOURCE` instead of
    /// spawning unbounded waiting `AgentInstance`s.
    WaitingResource,
}

/// Reverse backpressure signal propagating `Worker -> Cell -> Global`
/// (`[SCHED-BACKPRESSURE-001]`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BackpressureSignal {
    /// Domain that originated this pressure measurement.
    pub origin_domain: SchedulerDomainId,
    /// Intermediate domain (`Cell` or `Global`) that aggregated and forwarded
    /// this signal upward.
    pub propagated_via: Option<SchedulerDomainId>,
    /// Current pressure severity.
    pub level: DomainPressureLevel,
    /// Dominant saturation bottleneck, if any.
    pub primary_source: Option<SaturationSource>,
    /// Complete sorted set of active saturation sources.
    pub active_sources: Vec<SaturationSource>,
    /// Aggregate queue utilization in basis points (`0..=10_000`).
    pub queue_utilization_bps: u16,
    /// Recommended ceiling for the `Cell` / Planner `materialization_window`.
    pub recommended_materialization_window_limit: usize,
    /// Recommended admission disposition for new `TaskNode` materializations.
    pub admission_directive: AdmissionDirective,
    /// Monotonic signal epoch within the reporting domain.
    pub epoch: u64,
}

impl BackpressureSignal {
    /// Computes the recommended `materialization_window` limit for `level`
    /// relative to `base_window`.
    #[must_use]
    pub const fn compute_recommended_window(
        base_window: usize,
        level: DomainPressureLevel,
    ) -> usize {
        if base_window == 0 {
            return 0;
        }
        match level {
            DomainPressureLevel::Nominal => base_window,
            DomainPressureLevel::Elevated => {
                let shrunk = (base_window.saturating_mul(3)) / 4;
                if shrunk == 0 { 1 } else { shrunk }
            }
            DomainPressureLevel::Saturated => {
                let shrunk = base_window / 4;
                if shrunk == 0 { 1 } else { shrunk }
            }
            DomainPressureLevel::Critical => 0,
        }
    }

    /// Maps a [`DomainPressureLevel`] to its canonical [`AdmissionDirective`].
    #[must_use]
    pub const fn directive_for_level(level: DomainPressureLevel) -> AdmissionDirective {
        match level {
            DomainPressureLevel::Nominal => AdmissionDirective::Accept,
            DomainPressureLevel::Elevated => AdmissionDirective::ThrottleBackground,
            DomainPressureLevel::Saturated | DomainPressureLevel::Critical => {
                AdmissionDirective::WaitingResource
            }
        }
    }

    /// Constructs a nominal baseline signal for `domain`.
    #[must_use]
    pub const fn nominal(domain: SchedulerDomainId, base_window: usize) -> Self {
        Self {
            origin_domain: domain,
            propagated_via: None,
            level: DomainPressureLevel::Nominal,
            primary_source: None,
            active_sources: Vec::new(),
            queue_utilization_bps: 0,
            recommended_materialization_window_limit: base_window,
            admission_directive: AdmissionDirective::Accept,
            epoch: 0,
        }
    }

    /// Returns a signal forwarded upward through `parent_domain`, recomputing
    /// `recommended_materialization_window_limit` against `parent_base_window`.
    #[must_use]
    pub fn propagate_to_parent(
        &self,
        parent_domain: SchedulerDomainId,
        parent_base_window: usize,
        epoch: u64,
    ) -> Self {
        Self {
            origin_domain: self.origin_domain,
            propagated_via: Some(parent_domain),
            level: self.level,
            primary_source: self.primary_source,
            active_sources: self.active_sources.clone(),
            queue_utilization_bps: self.queue_utilization_bps,
            recommended_materialization_window_limit: Self::compute_recommended_window(
                parent_base_window,
                self.level,
            ),
            admission_directive: Self::directive_for_level(self.level),
            epoch,
        }
    }
}

/// Admission outcome returned when placing or enqueueing work into a
/// `SchedulerDomain` (`[SCHED-QUEUE-001]`, `[SCHED-BACKPRESSURE-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchedulerAdmission {
    /// Item was admitted and placed onto `assigned_domain`.
    Admitted {
        assigned_domain: SchedulerDomainId,
        effective_priority: u8,
        context_affinity_hit: bool,
        materialization_window_limit: usize,
    },
    /// Downstream backpressure or a contracted `materialization_window`
    /// requires the caller/Planner to hold the node in `WAITING_RESOURCE`
    /// rather than queueing unbounded waiting `AgentInstance`s.
    WaitingResource {
        reporting_domain: SchedulerDomainId,
        level: DomainPressureLevel,
        source: Option<SaturationSource>,
        materialization_window_limit: usize,
    },
    /// Rejected because the item's `cancel_epoch` is already fenced.
    RejectedCancelled {
        cancel_epoch: u64,
        fenced_epoch: u64,
    },
    /// Rejected because the bounded per-`ResourceGroup` queue is full.
    RejectedQueueFull {
        domain: SchedulerDomainId,
        resource_group_id: [u8; 16],
        max_queue_depth: usize,
    },
    /// Rejected because the `ResourceGroup` is not registered on the domain.
    RejectedUnknownResourceGroup {
        domain: SchedulerDomainId,
        resource_group_id: [u8; 16],
    },
}

impl SchedulerAdmission {
    /// Returns `true` if the item was admitted onto a domain queue.
    #[must_use]
    pub const fn is_admitted(&self) -> bool {
        matches!(self, Self::Admitted { .. })
    }

    /// Returns `true` if backpressure directed the caller to `WAITING_RESOURCE`.
    #[must_use]
    pub const fn is_waiting_resource(&self) -> bool {
        matches!(self, Self::WaitingResource { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AdmissionDirective, BackpressureSignal, DomainPressureLevel, SchedulerDomainId,
        SchedulerTier, StealEligibility, StealRejectionReason, WorkStealEnvelope,
        WorkerStealCapabilities, evaluate_steal_eligibility,
    };

    fn sample_envelope() -> WorkStealEnvelope {
        WorkStealEnvelope {
            migratable: true,
            namespace_root: [0x11; 32],
            secret_scope_id: Some([0x22; 16]),
            preferred_context_digest: Some([0x33; 32]),
            device_affinity: Some([0x44; 16]),
            has_active_device_lease: false,
            has_active_effect_permit: false,
            has_active_reservation: false,
            provider_tenant_id: Some([0x55; 16]),
            task_authority_id: [0x66; 16],
            authority_term: 3,
            isolation_domain_id: [0x77; 16],
            isolation_fenced: false,
        }
    }

    fn matching_capabilities() -> WorkerStealCapabilities {
        WorkerStealCapabilities {
            allowed_namespaces: vec![[0x11; 32]],
            allowed_secret_scopes: vec![[0x22; 16]],
            attached_devices: vec![[0x44; 16]],
            allowed_provider_tenants: vec![[0x55; 16]],
            trusted_task_authorities: vec![([0x66; 16], 2)],
            active_isolation_domains: vec![[0x77; 16]],
            fenced_isolation_domains: Vec::new(),
            warm_context_digests: vec![[0x33; 32]],
            require_warm_context_on_steal: false,
        }
    }

    #[test]
    fn domain_lineage_and_tier_inspection() {
        let global = SchedulerDomainId::global([0xAA; 16]);
        let cell = SchedulerDomainId::cell([0xAA; 16], [0xBB; 16]);
        let worker = SchedulerDomainId::worker([0xBB; 16], [0xCC; 16]);

        assert_eq!(global.tier(), SchedulerTier::Global);
        assert_eq!(cell.tier(), SchedulerTier::Cell);
        assert_eq!(worker.tier(), SchedulerTier::Worker);

        assert!(cell.belongs_to_global([0xAA; 16]));
        assert!(!cell.belongs_to_global([0x99; 16]));
        assert!(cell.belongs_to_cell([0xBB; 16]));
        assert!(worker.belongs_to_cell([0xBB; 16]));
        assert!(!worker.belongs_to_cell([0x99; 16]));
        assert!(!worker.belongs_to_global([0xAA; 16]));
        assert!(worker.belongs_to_global_via([0xAA; 16], &cell));
    }

    #[test]
    fn steal_validator_enforces_every_sched_steal_001_fence() {
        let caps = matching_capabilities();
        let ok = sample_envelope();
        assert_eq!(
            evaluate_steal_eligibility(&ok, &caps),
            StealEligibility::Eligible {
                context_locality_hit: true
            }
        );

        let mut non_migratable = ok;
        non_migratable.migratable = false;
        assert_eq!(
            evaluate_steal_eligibility(&non_migratable, &caps),
            StealEligibility::Rejected(StealRejectionReason::NotMigratable)
        );

        let mut effect = ok;
        effect.has_active_effect_permit = true;
        assert_eq!(
            evaluate_steal_eligibility(&effect, &caps),
            StealEligibility::Rejected(StealRejectionReason::ActiveEffectPermit)
        );

        let mut reservation = ok;
        reservation.has_active_reservation = true;
        assert_eq!(
            evaluate_steal_eligibility(&reservation, &caps),
            StealEligibility::Rejected(StealRejectionReason::ActiveReservation)
        );

        let mut lease = ok;
        lease.has_active_device_lease = true;
        assert_eq!(
            evaluate_steal_eligibility(&lease, &caps),
            StealEligibility::Rejected(StealRejectionReason::ActiveExclusiveDeviceLease)
        );

        let mut fenced = ok;
        fenced.isolation_fenced = true;
        assert_eq!(
            evaluate_steal_eligibility(&fenced, &caps),
            StealEligibility::Rejected(StealRejectionReason::IsolationDomainFenced)
        );
    }

    #[test]
    fn backpressure_window_contraction_is_monotonic() {
        assert_eq!(
            BackpressureSignal::compute_recommended_window(16, DomainPressureLevel::Nominal),
            16
        );
        assert_eq!(
            BackpressureSignal::compute_recommended_window(16, DomainPressureLevel::Elevated),
            12
        );
        assert_eq!(
            BackpressureSignal::compute_recommended_window(16, DomainPressureLevel::Saturated),
            4
        );
        assert_eq!(
            BackpressureSignal::compute_recommended_window(16, DomainPressureLevel::Critical),
            0
        );
        assert_eq!(
            BackpressureSignal::directive_for_level(DomainPressureLevel::Saturated),
            AdmissionDirective::WaitingResource
        );
    }
}
