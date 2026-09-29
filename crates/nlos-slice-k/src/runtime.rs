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
use nlos_store::SqliteOperationStore;
use nlos_task::{AttemptRecord, PermitRecord, SqliteTaskAuthority, TaskRecord};
use nlos_types::{
    CommitPermitId, Generation, IdempotencyKey, InstallationId, PackageId, ProcessId,
    TaskAttemptId, TaskId,
};

use crate::error::{SliceKError, SliceKResult};
use crate::pump::{PumpLane, ReconcileRefusalSnapshot, stop_pump_bounded};

/// Poison-tolerant guard over the runtime's pump slot.
type PumpGuard<'a> = MutexGuard<'a, Option<OutboxPump>>;

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
    pub tasks: SqliteTaskAuthority,
    /// Authority clock (durable monotonic tick + wall high-water).
    pub clock: AuthorityClock,
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
    /// The durable-Outbox pump lane: `None` until
    /// [`SliceKRuntime::start_pump`] binds a pump to a runtime adapter.
    /// Guarded by a `Mutex` so `Drop` and explicit stops can take the pump
    /// out from behind an `Arc`-shared runtime.
    pump: Mutex<Option<OutboxPump>>,
    /// Refusal surface of the fail-closed reconcile sink (see
    /// [`crate::pump`]). Created once per runtime and shared with every
    /// pump generation, so the counters survive pump restarts.
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
        let tasks = SqliteTaskAuthority::open(root.join("tasks.sqlite3"))?;
        let clock = AuthorityClock::open(root.join("clock"))?;
        let operations = Arc::new(SqliteOperationStore::open(root.join("operations.sqlite3"))?);
        let capability = CapabilityAuthority::open(root.join("capability"))?;
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
            pump: Mutex::new(None),
            pump_lane: PumpLane::new(),
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
    /// fail-closed sink documented in [`crate::pump`] — refused with a
    /// typed error, retried with backoff, never acknowledged away, and
    /// counted on [`Self::reconcile_refusals`]. The pump uses the landed
    /// default tuning (25ms fallback poll, 16-failure threshold).
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
            reconcile_sink: self.pump_lane.sink(),
            config: ConsumerConfig { batch_limit: 8 },
        };
        let started = OutboxPump::start(consumer, PumpConfig::default()).map_err(
            |error: OutboxPumpStartError| match error {
                OutboxPumpStartError::Spawn(io) => SliceKError::Io(io),
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

    /// The fail-closed reconcile lane's refusal surface: how many
    /// `ReconcileEffect` entries this runtime refused (they stay durable in
    /// the outbox) and the most recent typed reason.
    #[must_use]
    pub fn reconcile_refusals(&self) -> ReconcileRefusalSnapshot {
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
    /// hang on a stuck consumer lane. With no pump running this is a
    /// no-op — dropping a runtime that never started one has no threads to
    /// reap.
    fn drop(&mut self) {
        if let Some(pump) = self
            .pump
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            stop_pump_bounded(pump);
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
