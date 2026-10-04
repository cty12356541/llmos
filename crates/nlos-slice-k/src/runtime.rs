//! The [`SliceKRuntime`] assembler: one constructor that opens and holds
//! every landed authority of the first longitudinal slice over one root
//! directory, plus the in-process inspect view the demo prints.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use nlos_application::{
    ApplicationAuthority, ApplicationStatus, ApplicationView, BackgroundTaskRegistrationReceipt,
    InstallationReceipt, ProcessBindingReceipt,
};
use nlos_artifact::{ArtifactStore, HeadState};
use nlos_capability::CapabilityAuthority;
use nlos_channel::ChannelAuthority;
use nlos_clock::{AuthorityClock, NowRequest};
use nlos_commit_coordinator::ArtifactCommitCoordinator;
use nlos_identity::IdentityAuthority;
use nlos_operation::{OperationHandle, OperationSnapshot};
use nlos_outbox::{ConsumerConfig, OutboxConsumer};
use nlos_process::{ProcessAuthority, ProcessBindingRecord};
use nlos_runtime_tokio::{
    OutboxPump, OutboxPumpStartError, PumpConfig, PumpHealth, PumpState, StoreOutboxSource,
    TokioRuntimeAdapter,
};
use nlos_semantic::SemanticAuthority;
use nlos_store::SqliteOperationStore;
use nlos_task::{AttemptRecord, PermitRecord, SqliteTaskAuthority, TaskRecord};
use nlos_topic::TopicAuthority;
use nlos_types::{
    ApplicationId, CommitPermitId, Generation, IdempotencyKey, InstallationId, PackageId,
    ProcessId, TaskAttemptId, TaskId,
};

use crate::error::{SliceKError, SliceKResult};
use crate::pump::{PumpLane, ReconcileLaneSnapshot, stop_pump_bounded};
use crate::semantic_stream::{
    SemanticStreamBinding, SemanticStreamConfig, SemanticStreamDeps, SemanticStreamHealth,
    SemanticStreamPump, SemanticStreamPumpStartError, SemanticStreamState,
    bootstrap_semantic_stream, stop_semantic_stream_bounded,
};
use crate::semantic_writer::{SemanticWriter, SemanticWriterBudgetRaise, WriterRootRegistry};

/// Poison-tolerant guard over the runtime's pump slot.
type PumpGuard<'a> = MutexGuard<'a, Option<OutboxPump>>;

/// Poison-tolerant guard over the runtime's semantic stream pump slot.
type SemanticStreamGuard<'a> = MutexGuard<'a, Option<SemanticStreamPump>>;

/// One assembler holding every authority of the first longitudinal slice.
///
/// Opening is the only thing this type "invents": fixed sub-path names for
/// the landed authority stores under one root. Every authority keeps its own
/// database, schema, and durability guarantees; reopening the same root
/// after a crash is the recovery path (drop + reopen + converge).
///
/// Fields are public by design: the slice composes the landed public APIs
/// directly and adds no wrapper semantics beyond ordering.
pub struct SliceKRuntime {
    root: PathBuf,
    /// Principal/key authority (bootstrap, signature verification readback).
    pub identity: IdentityAuthority,
    /// Process/AgentInstance/IsolationDomain binding authority (durable
    /// generation/fence; B-PROCESS-001). Fibers spawn only under a binding
    /// this authority registered.
    pub process: ProcessAuthority,
    /// Content-addressed artifact authority (revisions, signed packages,
    /// staged publication).
    pub artifacts: ArtifactStore,
    /// Application/installation authority (verify-then-install).
    pub applications: ApplicationAuthority,
    /// Task authority (tasks, attempts, permits, commit plans, receipts).
    /// Held behind an `Arc` so the pump's reconcile sink can route late
    /// Operation outcomes into the same durable authority without opening a
    /// second connection to the same database.
    pub tasks: Arc<SqliteTaskAuthority>,
    /// Authority clock (durable monotonic tick + wall high-water). Shared
    /// behind an `Arc` with the pump's reconcile sink, whose per-entry
    /// idempotency keys take replay-stable wall readings.
    pub clock: Arc<AuthorityClock>,
    /// Durable operation store (driver operations owned by fibers). Shared
    /// behind an `Arc` so the payload-execution lane can bind the same
    /// durable authority into the `nlos-driver-mock` provider face
    /// ([`MockProvider::new`](nlos_driver_mock::MockProvider::new)) without
    /// opening a second connection to the same database.
    pub operations: Arc<SqliteOperationStore>,
    /// Capability authority (root issuance, delegation, semantic admission).
    /// Assembled with read-only exposure: the slice holds and opens it so
    /// `authorize_semantic` is production-reachable through this runtime,
    /// but adds no wrapper semantics.
    capability: CapabilityAuthority,
    /// Semantic assertion authority (`<root>/semantic/semantic-authority.db`,
    /// the daemon W46-L3 path style), opened read-exposed: the production
    /// write side lives in [`crate::semantic_writer`], which appends through
    /// this authority's admission gates. Held behind an `Arc` so the
    /// semantic stream pump lane can drain the admission outbox through the
    /// same connection without opening a second one.
    semantic: Arc<SemanticAuthority>,
    /// Durable system Channel endpoint authority (`<root>/channel`), the
    /// daemon W46 path style — opened for the semantic notification stream
    /// lane (see [`crate::semantic_stream`]) and shared with the topic
    /// authority below.
    pub channel: Arc<ChannelAuthority>,
    /// Durable Topic service-layer authority (`<root>/topics`) over the
    /// system channel — the semantic notification stream's fanout owner
    /// (see [`crate::semantic_stream`]).
    pub topics: Arc<TopicAuthority>,
    /// The dedicated semantic writer principal this runtime bootstrapped
    /// (key file `<root>/keys/semantic-writer.key`, see
    /// [`crate::semantic_writer`]). Held so the payload lane's terminal
    /// receipts reach the semantic ledger without any caller-side setup.
    semantic_writer: SemanticWriter,
    /// The durable raise registry (W53-B, file
    /// `<root>/semantic-writer-roots`): the current root-capability handle
    /// per (writer key, application) that
    /// [`SliceKRuntime::raise_semantic_writer_budget`] wrote and every
    /// active-root resolution reads. Guarded by a `Mutex` so a raise
    /// serializes its revoke→issue→register orchestration against bridge
    /// resolutions within this runtime.
    writer_roots: Mutex<WriterRootRegistry>,
    /// The semantic notification stream pump lane (see
    /// [`crate::semantic_stream`]): `None` until
    /// [`SliceKRuntime::start_semantic_stream`] bootstraps the well-known
    /// system channel/topic/subscription and binds a pump to it. Guarded by
    /// a `Mutex` so `Drop` and explicit stops can take the pump out from
    /// behind an `Arc`-shared runtime.
    semantic_stream: Mutex<Option<SemanticStreamPump>>,
    /// The durable-Outbox pump lane: `None` until
    /// [`SliceKRuntime::start_pump`] binds a pump to a runtime adapter.
    /// Guarded by a `Mutex` so `Drop` and explicit stops can take the pump
    /// out from behind an `Arc`-shared runtime.
    pump: Mutex<Option<OutboxPump>>,
    /// Health surface of the reconcile lane (see [`crate::pump`]): routed
    /// and refused counts of the task-routing sink. Created once per runtime
    /// and shared with every pump generation, so the counters survive pump
    /// restarts.
    pump_lane: PumpLane,
}

impl SliceKRuntime {
    /// Opens (or creates after a crash) every authority of the slice under
    /// one root directory.
    ///
    /// # Errors
    ///
    /// Fails closed with the first authority open error; each authority
    /// validates its own WAL/FULL durability and schema version.
    pub fn open(root: impl AsRef<Path>) -> SliceKResult<Self> {
        let root = root.as_ref().to_path_buf();
        std::fs::create_dir_all(&root)?;
        let identity = IdentityAuthority::open(root.join("identity"))?;
        let process = ProcessAuthority::open(root.join("process"))?;
        let artifacts = ArtifactStore::open(root.join("artifacts"))?;
        let applications = ApplicationAuthority::open(root.join("applications"))?;
        let tasks = Arc::new(SqliteTaskAuthority::open(root.join("tasks.sqlite3"))?);
        let clock = Arc::new(AuthorityClock::open(root.join("clock"))?);
        let operations = Arc::new(SqliteOperationStore::open(root.join("operations.sqlite3"))?);
        let capability = CapabilityAuthority::open(root.join("capability"))?;
        let channel = Arc::new(ChannelAuthority::open(root.join("channel"))?);
        let topics = Arc::new(TopicAuthority::open(
            root.join("topics"),
            Arc::clone(&channel),
        )?);
        let semantic = Arc::new(SemanticAuthority::open(root.join("semantic"))?);
        let semantic_writer = SemanticWriter::assemble(
            &identity,
            &process,
            &crate::semantic_writer::load_runtime_writer_key(&clock, &root.join("keys"))?,
        )?;
        let writer_roots = WriterRootRegistry::load(root.join("semantic-writer-roots"))?;
        Ok(Self {
            root,
            identity,
            process,
            artifacts,
            applications,
            tasks,
            clock,
            operations,
            capability,
            semantic,
            channel,
            topics,
            semantic_writer,
            writer_roots: Mutex::new(writer_roots),
            pump: Mutex::new(None),
            pump_lane: PumpLane::new(),
            semantic_stream: Mutex::new(None),
        })
    }

    /// The durable root every authority was opened under; reopening this
    /// path after a drop is the crash-recovery entry.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// One authoritative wall reading (ms since Unix epoch) under a fresh
    /// idempotency key. The slice takes every timestamp from the clock
    /// authority, never from `SystemTime` directly.
    ///
    /// # Errors
    ///
    /// Propagates [`nlos_clock::AuthorityClockError`].
    pub fn wall_now_ms(&self, key: IdempotencyKey) -> SliceKResult<u64> {
        let decision = self.clock.wall_now(NowRequest {
            idempotency_key: key,
        })?;
        Ok(decision.reading().as_u64())
    }

    /// [`Self::wall_now_ms`] narrowed into the `i64` timestamp domain the
    /// task authority uses.
    ///
    /// # Errors
    ///
    /// Propagates clock errors and the (astronomically remote)
    /// [`SliceKError::TimestampOverflow`].
    pub fn wall_now_i64(&self, key: IdempotencyKey) -> SliceKResult<i64> {
        let ms = self.wall_now_ms(key)?;
        i64::try_from(ms).map_err(|_| SliceKError::TimestampOverflow(ms))
    }

    /// The cross-authority verify-then-commit coordinator bound to this
    /// runtime's task and artifact authorities.
    #[must_use]
    pub fn coordinator(&self) -> ArtifactCommitCoordinator<'_> {
        ArtifactCommitCoordinator::new(&self.tasks, &self.artifacts)
    }

    /// Read-only handle on the capability authority this runtime opened
    /// (`<root>/capability/capability-authority.db`), making
    /// `authorize_semantic` production-reachable through the slice.
    #[must_use]
    pub fn capability(&self) -> &CapabilityAuthority {
        &self.capability
    }

    /// Read-only handle on the semantic authority this runtime opened
    /// (`<root>/semantic/semantic-authority.db`): event, receipt, outbox,
    /// and trust-view reads for everything the write bridge admitted.
    #[must_use]
    pub fn semantic(&self) -> &SemanticAuthority {
        &self.semantic
    }

    /// The runtime's dedicated semantic writer principal (see
    /// [`crate::semantic_writer`]): the only production writer of semantic
    /// assertions in this slice.
    #[must_use]
    pub fn semantic_writer(&self) -> &SemanticWriter {
        &self.semantic_writer
    }

    /// The remaining semantic-write budget of one application's root
    /// capability (W51): `call_limit_remaining` read through the runtime's
    /// writer — the per-application quota every bridged admission charges
    /// exactly once (see [`crate::semantic_writer`]). The read-only face
    /// for CLI/daemon presentation; an exhausted budget surfaces as `Some(0)`.
    ///
    /// Since W53-B the read resolves the application's *current active
    /// root* (the durable raise registry first, the writer's
    /// deterministic, idempotent original-issuance replay as fallback):
    /// after [`Self::raise_semantic_writer_budget`] the reported budget is
    /// the new root's — a fresh `new_limit` that subsequent admissions
    /// draw down (the authority meters per capability, so a raised root
    /// never inherits the old root's spent charges). Before any raise, the
    /// first observation of an application still materializes exactly the
    /// one replay-stable original capability row and no other state.
    ///
    /// # Errors
    ///
    /// Propagates capability-authority refusals typed. A no-active-root
    /// state (the raise window of
    /// [`Self::raise_semantic_writer_budget`], or an out-of-band
    /// revocation of the original root) fails closed with the authority's
    /// generation-fence refusal instead of reporting a number.
    pub fn semantic_writer_budget(
        &self,
        application_id: ApplicationId,
    ) -> SliceKResult<Option<u64>> {
        let capability = self
            .semantic_writer
            .active_application_capability(self, application_id)?;
        Ok(self.capability.call_limit_remaining(capability.handle)?)
    }

    /// Raises (or lowers — an operations decision, no monotonicity is
    /// enforced) the application's per-writer semantic-write budget to
    /// exactly `new_limit` (`>= 1`), closing the W51 exhaustion residual
    /// (W53-B). No single transaction spans the needed authority effects,
    /// so this is an orchestrated sequence of the capability authority's
    /// own signed commands, in a fixed order:
    ///
    /// 1. resolve the (writer, application)'s current active root (the
    ///    raise registry, falling back to the untouched original
    ///    issuance);
    /// 2. when that root's `call_limit` already equals `new_limit`, this
    ///    call is the typed no-op replay (`issued == false`) — nothing is
    ///    revoked or issued;
    /// 3. otherwise revoke the current root through `revoke_signed`
    ///    (writer-signed; the writer is both issuer and holder), which
    ///    advances its generation and leaves the old handle
    ///    generation-fenced;
    /// 4. issue a fresh root through `issue_root_signed` — the identical
    ///    target namespace, rights, purpose, and validity window, with
    ///    `call_limit = new_limit`;
    /// 5. durably register the new root in `<root>/semantic-writer-roots`
    ///    (atomic temp-file replace), which every bridge admission, budget
    ///    read, and later raise resolves through.
    ///
    /// The new root is a fresh budget: the authority meters consumption
    /// per capability, so the raised root starts at `new_limit` remaining
    /// and does not inherit the revoked root's spent charges. The raise's
    /// authority time and command idempotency keys derive from
    /// `(writer key, application, "raise", new_limit)` on the durable
    /// clock, so replaying the same raise replays the same revocation and
    /// issuance receipts and converges without double effects; a raise to
    /// a *different* limit after a completed one is simply another raise.
    ///
    /// **Window semantics (honest, not atomic):** if the process dies — or
    /// the issue or registry persist refuses — after step 3's revoke
    /// committed but before step 5 registered a live new root, the
    /// application has *no active root*: bridge admissions and
    /// [`Self::semantic_writer_budget`] fail closed with the capability
    /// authority's generation-fence refusal. Any later raise converges the
    /// state (with nothing active there is nothing to revoke; the issue
    /// proceeds and the registry is written) — including a raise to a
    /// different limit. A root issued but never registered (a crash
    /// between steps 4 and 5) stays live but unreachable through this
    /// runtime: same writer, same namespace, same rights, and only the
    /// registry-named root is ever consumed.
    ///
    /// # Errors
    ///
    /// Fails typed on `new_limit == 0`, clock or capability-authority
    /// refusals (signature, identity, revocation authorization), or a
    /// registry persistence failure — see the window semantics above for
    /// the states those failures can leave behind.
    pub fn raise_semantic_writer_budget(
        &self,
        application_id: ApplicationId,
        new_limit: u64,
    ) -> SliceKResult<SemanticWriterBudgetRaise> {
        self.semantic_writer
            .raise_application_capability(self, application_id, new_limit)
    }

    /// The raise registry lock shared by the writer's resolution and raise
    /// orchestration (see [`Self::writer_roots`]); poison-tolerant like
    /// every other runtime mutex.
    pub(crate) fn lock_writer_roots(&self) -> std::sync::MutexGuard<'_, WriterRootRegistry> {
        self.writer_roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Starts the durable-Outbox pump for this runtime, bound to `adapter`'s
    /// wake lane.
    ///
    /// Why an explicit start instead of `open`: the wake sink must be the
    /// [`TokioWakeSink`](nlos_runtime_tokio::TokioWakeSink) of the very
    /// adapter that hosts the owner fibers — `open` is synchronous and
    /// tokio-context-free while every existing lane builds its own adapter
    /// from `Handle::current()`, so injection is the only wiring that
    /// routes wakes to the right fiber registry instead of acking them as
    /// `FiberGone`.
    ///
    /// Routing: `WakeFiber` entries go to the adapter's wake sink (the
    /// closed durable-wake loop); `ReconcileEffect` entries go to the
    /// task-routing sink documented in [`crate::pump`] — offered to this
    /// runtime's task authority as real late-outcome routing, with the
    /// typed no-route decision and every routing failure kept fail-closed
    /// (not acknowledged, retried with backoff, counted per lane on
    /// [`Self::reconcile_lane`]). A persistently refused head entry (W57-B
    /// poison head) is parked one-way after
    /// [`DEFAULT_PARK_THRESHOLD`](crate::pump::DEFAULT_PARK_THRESHOLD)
    /// consecutive same-sequence failures: the queue head unlocks, and the
    /// parked entry stays durable and unacknowledged for manual
    /// adjudication — parking is explicit operational debt with no
    /// automatic un-park. The pump uses the landed default tuning (25ms
    /// fallback poll, 16-failure pump-fault threshold).
    ///
    /// Starting again while a pump is `Running` fails closed — a second
    /// lane silently stealing the pump would ack wakes for its fibers as
    /// `FiberGone` on the first lane's registry. A `Faulted`/`Stopped`
    /// leftover is joined first and replaced (fault recovery).
    ///
    /// # Errors
    ///
    /// Returns [`SliceKError::Pump`] when a pump is already running, and
    /// [`SliceKError::Io`] when the OS refuses the pump thread.
    pub fn start_pump(&self, adapter: &TokioRuntimeAdapter) -> SliceKResult<()> {
        let mut pump = self.lock_pump();
        if let Some(existing) = pump.as_ref() {
            if existing.health().state == PumpState::Running {
                return Err(SliceKError::Pump(
                    "outbox pump already running; stop_pump() before starting another",
                ));
            }
            // Not running: join the dead thread so only one pump generation
            // ever owns the outbox.
            if let Some(dead) = pump.take() {
                dead.stop();
            }
        }
        let consumer = OutboxConsumer {
            source: StoreOutboxSource::new(Arc::clone(&self.operations)),
            wake_sink: adapter.wake_sink(),
            reconcile_sink: self.pump_lane.sink(
                Arc::clone(&self.operations),
                Arc::clone(&self.tasks),
                Arc::clone(&self.clock),
            ),
            config: ConsumerConfig { batch_limit: 8 },
        };
        // `PumpConfig::default()` keeps a non-zero poll interval, so the
        // InvalidConfig arm is unreachable here — but mapping it keeps the
        // pump's start surface exhaustive instead of a wildcard.
        let started = OutboxPump::start(consumer, PumpConfig::default()).map_err(
            |error: OutboxPumpStartError| match error {
                OutboxPumpStartError::Spawn(io) => SliceKError::Io(io),
                OutboxPumpStartError::InvalidConfig(reason) => SliceKError::Pump(reason),
            },
        )?;
        *pump = Some(started);
        Ok(())
    }

    /// Bounded, non-blocking delivery hint into a running pump. `false`
    /// when no pump runs (or a hint is already pending); the 25ms fallback
    /// poll bounds delivery either way, so callers may ignore the result.
    #[must_use]
    pub fn hint_pump(&self) -> bool {
        self.lock_pump().as_ref().is_some_and(OutboxPump::hint)
    }

    /// Current pump health, or `None` while no pump is running.
    #[must_use]
    pub fn pump_health(&self) -> Option<PumpHealth> {
        self.lock_pump().as_ref().map(OutboxPump::health)
    }

    /// The reconcile lane's health surface: how many `ReconcileEffect`
    /// entries routed into the task authority, how many were refused per
    /// fail-closed lane (typed no-route vs routing failure) with the most
    /// recent typed reason, and the W57-B dead-letter state — how many
    /// entries this lane parked after repeated same-sequence failures and
    /// the durable reason of the most recent park. Refused and parked
    /// entries stay durable in the outbox; parked ones are visible through
    /// [`SqliteOperationStore::inspect_parked_outbox`] on
    /// [`Self::operations`] and require manual adjudication (there is no
    /// automatic un-park).
    #[must_use]
    pub fn reconcile_lane(&self) -> ReconcileLaneSnapshot {
        self.pump_lane.snapshot()
    }

    /// Stops the pump and joins its thread. Idempotent: a runtime without
    /// a running pump is a no-op. Unacknowledged outbox entries stay
    /// durable for a future pump, exactly as the at-least-once contract
    /// requires.
    pub fn stop_pump(&self) {
        if let Some(pump) = self.lock_pump().take() {
            stop_pump_bounded(pump);
        }
    }

    fn lock_pump(&self) -> PumpGuard<'_> {
        self.pump
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Idempotently bootstraps the semantic notification stream's
    /// well-known system channel, system topic, and system subscription
    /// (see [`crate::semantic_stream`]) under the default
    /// [`SemanticStreamConfig`]. Repeating the call replays the durable
    /// binding; the durable-identity fields of the config are frozen per
    /// root.
    ///
    /// # Errors
    ///
    /// Propagates [`SliceKError::Channel`], [`SliceKError::Topic`], and
    /// [`SliceKError::Clock`] refusals typed.
    pub fn bootstrap_semantic_stream(&self) -> SliceKResult<SemanticStreamBinding> {
        bootstrap_semantic_stream(
            &self.channel,
            &self.topics,
            &self.clock,
            &SemanticStreamConfig::default(),
        )
    }

    /// Starts the semantic notification stream pump with the default
    /// [`SemanticStreamConfig`] (see [`crate::semantic_stream`]): one
    /// dedicated OS thread that drains the pending semantic admission
    /// outbox prefix into 73-byte topic envelopes, acknowledges the outbox,
    /// and follows/compacts the system subscriber cursor so the bounded
    /// channel capacity is released.
    ///
    /// Why a separate lifecycle from [`Self::start_pump`]: the durable
    /// Outbox pump is bound to a caller's tokio adapter because its wake
    /// lane routes into that adapter's fiber registry; the semantic stream
    /// has no wake sink to route — its failure domain, tuning, and stop
    /// semantics are its own, so coupling the two lanes would let one
    /// lane's fault take the other down.
    ///
    /// Starting again while a stream pump is running fails closed; a
    /// `Faulted`/`Stopped` leftover is joined first and replaced.
    ///
    /// # Errors
    ///
    /// Returns [`SliceKError::Channel`]/[`SliceKError::Topic`]/
    /// [`SliceKError::Clock`] when the bootstrap refuses,
    /// [`SliceKError::Pump`] when a stream pump already runs or the config
    /// is unusable, and [`SliceKError::Io`] when the OS refuses the thread.
    pub fn start_semantic_stream(&self) -> SliceKResult<SemanticStreamBinding> {
        self.start_semantic_stream_with(SemanticStreamConfig::default())
    }

    /// [`Self::start_semantic_stream`] under an explicit (possibly
    /// non-default) [`SemanticStreamConfig`] — the lane's documented tuning
    /// and durable-policy surface. The config's durable-identity fields are
    /// frozen per root: a different value against an already-bootstrapped
    /// root fails closed with the authorities' typed idempotency conflict.
    ///
    /// # Errors
    ///
    /// As [`Self::start_semantic_stream`].
    pub fn start_semantic_stream_with(
        &self,
        config: SemanticStreamConfig,
    ) -> SliceKResult<SemanticStreamBinding> {
        let mut stream = self.lock_semantic_stream();
        if let Some(existing) = stream.as_ref() {
            if existing.health().state == SemanticStreamState::Running {
                return Err(SliceKError::Pump(
                    "semantic stream pump already running; stop_semantic_stream() before starting another",
                ));
            }
            // Not running: join the dead thread so only one stream pump
            // generation ever owns the lane.
            if let Some(dead) = stream.take() {
                stop_semantic_stream_bounded(dead);
            }
        }
        let binding = bootstrap_semantic_stream(&self.channel, &self.topics, &self.clock, &config)?;
        let started = SemanticStreamPump::start(
            SemanticStreamDeps {
                semantic: Arc::clone(&self.semantic),
                topics: Arc::clone(&self.topics),
                clock: Arc::clone(&self.clock),
                binding: binding.clone(),
            },
            config,
        )
        .map_err(|error: SemanticStreamPumpStartError| match error {
            SemanticStreamPumpStartError::Spawn(io) => SliceKError::Io(io),
            SemanticStreamPumpStartError::InvalidConfig(reason) => SliceKError::Pump(reason),
        })?;
        *stream = Some(started);
        Ok(binding)
    }

    /// Bounded, non-blocking wake-up hint into a running semantic stream
    /// pump. `false` when no pump runs (or a hint is already pending); the
    /// fallback poll interval bounds delivery either way, so callers may
    /// ignore the result.
    #[must_use]
    pub fn hint_semantic_stream(&self) -> bool {
        self.lock_semantic_stream()
            .as_ref()
            .is_some_and(SemanticStreamPump::hint)
    }

    /// Current semantic stream pump health, or `None` while no pump runs.
    #[must_use]
    pub fn semantic_stream_health(&self) -> Option<SemanticStreamHealth> {
        self.lock_semantic_stream()
            .as_ref()
            .map(SemanticStreamPump::health)
    }

    /// Stops the semantic stream pump and joins its thread. Idempotent: a
    /// runtime without a running stream pump is a no-op. Unacknowledged
    /// outbox rows stay durable for a future pump, exactly as the
    /// at-least-once contract requires.
    pub fn stop_semantic_stream(&self) {
        if let Some(pump) = self.lock_semantic_stream().take() {
            stop_semantic_stream_bounded(pump);
        }
    }

    fn lock_semantic_stream(&self) -> SemanticStreamGuard<'_> {
        self.semantic_stream
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Drains every pending artifact commit plan to its terminal
    /// `TaskCommitReceipt` (the crash-recovery convergence entry).
    ///
    /// # Errors
    ///
    /// Propagates [`nlos_commit_coordinator::CoordinatorError`].
    pub fn converge_pending(
        &self,
        scan_limit: usize,
        now_ms: i64,
    ) -> SliceKResult<Vec<nlos_task::ArtifactTaskCommitReceipt>> {
        Ok(self.coordinator().converge_pending(scan_limit, now_ms)?)
    }

    /// In-process inspect of one assembled chain — the same facts a CLI/NL
    /// inspect surface would render, read straight from the authorities.
    ///
    /// # Errors
    ///
    /// Propagates authority read errors; optional rows that do not exist
    /// (application never installed, permit not requested) read as `None`,
    /// not errors.
    pub fn inspect_chain(&self, query: ChainQuery) -> SliceKResult<ChainInspect> {
        let application = self.applications.inspect_application(query.package_id)?;
        // The application row names the principal whose semantic-write
        // budget the inspect surfaces (W51): resolved only when the
        // application exists, through the same idempotent replay the
        // bridge performs.
        let semantic_writer_budget = application
            .as_ref()
            .map(|view| self.semantic_writer_budget(view.application_id))
            .transpose()?
            .flatten();
        let installation = query
            .installation_id
            .map(|installation_id| self.applications.inspect_installation(installation_id))
            .transpose()?;
        let process = query
            .process_id
            .map(|process_id| self.process.inspect_active_process_binding(process_id))
            .transpose()?;
        let task = self.tasks.inspect_task(query.task_id)?;
        let attempt = self
            .tasks
            .inspect_attempt(query.task_id, query.attempt_id)?;
        let permit = query
            .permit_id
            .map(|permit_id| self.tasks.inspect_permit(query.task_id, permit_id))
            .transpose()?;
        let artifact_head = self
            .artifacts
            .resolve_head(query.artifact_id, u64::MAX)
            .map_err(SliceKError::from)?;
        let operation = query
            .operation
            .map(|handle| self.operations.inspect(handle))
            .transpose()?;
        Ok(ChainInspect {
            application,
            semantic_writer_budget,
            installation,
            process,
            task,
            attempt,
            permit,
            artifact_head,
            operation,
        })
    }

    /// Read-only inspect of one application's durable background-task
    /// registrations and process bindings — the same facts a CLI/NL surface
    /// would render for ROAD-B-002 registration state, aggregated straight
    /// from the application authority.
    ///
    /// # Errors
    ///
    /// Propagates application-authority read errors. An unknown package
    /// returns empty lists, not an error.
    pub fn inspect_application_registrations(
        &self,
        package_id: PackageId,
    ) -> SliceKResult<ApplicationRegistrationInspect> {
        Ok(ApplicationRegistrationInspect {
            background_tasks: self.applications.inspect_background_tasks(package_id)?,
            process_bindings: self.applications.inspect_process_bindings(package_id)?,
        })
    }
}

impl Drop for SliceKRuntime {
    /// Stops a still-running pump with a bounded join (see
    /// [`crate::pump`]): the stop flag and wake-up hint are delivered
    /// first, the join itself waits at most a deadline so teardown cannot
    /// hang on a stuck consumer lane. The semantic stream pump lane gets
    /// the same bounded stop. With neither pump running this is a no-op —
    /// dropping a runtime that never started one has no threads to reap.
    fn drop(&mut self) {
        if let Some(pump) = self
            .pump
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            stop_pump_bounded(pump);
        }
        if let Some(stream) = self
            .semantic_stream
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            stop_semantic_stream_bounded(stream);
        }
    }
}

/// Identifiers naming one assembled chain to [`SliceKRuntime::inspect_chain`].
#[derive(Clone, Copy, Debug)]
pub struct ChainQuery {
    pub package_id: PackageId,
    /// `None` before the install step.
    pub installation_id: Option<InstallationId>,
    pub task_id: TaskId,
    pub attempt_id: TaskAttemptId,
    /// `None` before the process binding was materialized.
    pub process_id: Option<ProcessId>,
    /// `None` before permit issuance.
    pub permit_id: Option<CommitPermitId>,
    pub artifact_id: nlos_types::ArtifactId,
    /// `None` before the fiber registered its driver operation.
    pub operation: Option<OperationHandle>,
}

/// The durable facts one chain inspect observes, straight from the
/// authorities (no slice-side cache).
#[derive(Clone, Debug)]
pub struct ChainInspect {
    pub application: Option<ApplicationView>,
    /// Remaining semantic-write budget of the inspected application's
    /// writer root capability (`call_limit_remaining`, W51): `None` only
    /// while the application itself is absent (never installed).
    pub semantic_writer_budget: Option<u64>,
    pub installation: Option<InstallationReceipt>,
    /// The authority-current process binding, readback-validated.
    pub process: Option<ProcessBindingRecord>,
    pub task: TaskRecord,
    pub attempt: AttemptRecord,
    pub permit: Option<PermitRecord>,
    pub artifact_head: Option<HeadState>,
    pub operation: Option<OperationSnapshot>,
}

/// Durable background-task and process-binding registrations of one
/// application, read straight from the application authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplicationRegistrationInspect {
    pub background_tasks: Vec<BackgroundTaskRegistrationReceipt>,
    pub process_bindings: Vec<ProcessBindingReceipt>,
}

impl ApplicationRegistrationInspect {
    /// Stable `key=value` lines for demo/CLI inspect (grep-friendly).
    #[must_use]
    pub fn report_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        lines.push(format!("background_tasks={}", self.background_tasks.len()));
        for receipt in &self.background_tasks {
            lines.push(format!(
                "background_task={} generation={} principal={}",
                crate::short_hex(receipt.task_id.as_bytes()),
                receipt.application_generation.get(),
                crate::short_hex(receipt.registrant_principal.as_bytes()),
            ));
        }
        lines.push(format!("process_bindings={}", self.process_bindings.len()));
        for receipt in &self.process_bindings {
            lines.push(format!(
                "process_binding={} generation={} principal={}",
                crate::short_hex(receipt.process_id.as_bytes()),
                receipt.application_generation.get(),
                crate::short_hex(receipt.registrant_principal.as_bytes()),
            ));
        }
        lines
    }
}

impl ChainInspect {
    /// Stable `key=value` lines for the demo's inspect step (one fact per
    /// line, grep-friendly, authority-sourced).
    #[must_use]
    pub fn report_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        let application_state = match &self.application {
            Some(view) => {
                let status = match view.status {
                    ApplicationStatus::Installed => "installed",
                    ApplicationStatus::Disabled => "disabled",
                    ApplicationStatus::Uninstalled => "uninstalled",
                };
                format!(
                    "{status} generation={} manifest={}",
                    view.current_installation_generation.get(),
                    crate::short_hex(view.package_manifest_digest.as_bytes())
                )
            }
            None => "absent".to_string(),
        };
        lines.push(format!("application={application_state}"));
        let budget = match self.semantic_writer_budget {
            Some(units) => units.to_string(),
            None => "absent".to_string(),
        };
        lines.push(format!("semantic_writer_budget={budget}"));
        let installation = match &self.installation {
            Some(receipt) => crate::short_hex(receipt.installation_id.as_bytes()),
            None => "absent".to_string(),
        };
        lines.push(format!("installation={installation}"));
        let process = match &self.process {
            Some(binding) => format!(
                "{} generation={} agent={}",
                crate::short_hex(binding.process_id.as_bytes()),
                binding.process_generation.get(),
                crate::short_hex(binding.agent_instance_id.as_bytes()),
            ),
            None => "absent".to_string(),
        };
        lines.push(format!("process={process}"));
        lines.push(format!(
            "task={} head_commit_seq={} cancel_epoch={}",
            crate::short_hex(self.task.task_id.as_bytes()),
            self.task.head_commit_seq,
            self.task.cancel_epoch
        ));
        lines.push(format!(
            "attempt={} state={:?}",
            crate::short_hex(self.attempt.attempt_id.as_bytes()),
            self.attempt.state
        ));
        let permit = match &self.permit {
            Some(permit) => crate::short_hex(permit.permit_id.as_bytes()),
            None => "absent".to_string(),
        };
        lines.push(format!("permit={permit}"));
        let head = match &self.artifact_head {
            Some(head) => format!(
                "revision={} digest={}",
                head.revision,
                crate::short_hex(head.digest.as_bytes())
            ),
            None => "absent".to_string(),
        };
        lines.push(format!("artifact_head={head}"));
        let operation = match &self.operation {
            Some(snapshot) => format!(
                "id={} generation={} state={:?}",
                crate::short_hex(snapshot.handle.operation_id.as_bytes()),
                snapshot.handle.generation.get(),
                snapshot.state
            ),
            None => "absent".to_string(),
        };
        lines.push(format!("operation={operation}"));
        lines
    }
}

/// Fresh idempotency key from one seed byte + offset. The slice-fixture
/// convention: every key of a scenario is `[seed + offset; 16]`, so
/// scenarios sharing one store never collide and every value is
/// reproducible.
#[must_use]
pub fn seeded_key(seed: u8, offset: u8) -> IdempotencyKey {
    IdempotencyKey::from_bytes([seed.wrapping_add(offset); 16])
}

/// `Generation::INITIAL` spelled once for the fixture tables.
#[must_use]
pub const fn initial_generation() -> Generation {
    Generation::INITIAL
}

/// Short hex form (first 8 bytes) used by every demo receipt line.
#[must_use]
pub fn short_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let end = bytes.len().min(8);
    bytes[..end].iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}
