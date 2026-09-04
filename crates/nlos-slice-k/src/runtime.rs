//! The [`SliceKRuntime`] assembler: one constructor that opens and holds
//! every landed authority of the first longitudinal slice over one root
//! directory, plus the in-process inspect view the demo prints.

use std::path::{Path, PathBuf};

use nlos_application::{
    ApplicationAuthority, ApplicationStatus, ApplicationView, InstallationReceipt,
};
use nlos_artifact::{ArtifactStore, HeadState};
use nlos_clock::{AuthorityClock, NowRequest};
use nlos_commit_coordinator::ArtifactCommitCoordinator;
use nlos_identity::IdentityAuthority;
use nlos_operation::{OperationHandle, OperationSnapshot};
use nlos_process::{ProcessAuthority, ProcessBindingRecord};
use nlos_store::SqliteOperationStore;
use nlos_task::{AttemptRecord, PermitRecord, SqliteTaskAuthority, TaskRecord};
use nlos_types::{
    CommitPermitId, Generation, IdempotencyKey, InstallationId, PackageId, ProcessId,
    TaskAttemptId, TaskId,
};

use crate::error::{SliceKError, SliceKResult};

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
    /// Durable operation store (driver operations owned by fibers).
    pub operations: SqliteOperationStore,
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
        let operations = SqliteOperationStore::open(root.join("operations.sqlite3"))?;
        Ok(Self {
            root,
            identity,
            process,
            artifacts,
            applications,
            tasks,
            clock,
            operations,
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
