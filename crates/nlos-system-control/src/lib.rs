//! Typed `SystemControl` adapter for TaskAuthority-owned Artifact recovery.
//!
//! The adapter owns no canonical state. It validates the common SABI context,
//! delegates authorization, reads worker/TaskAuthority facts, and turns one
//! acknowledgement `ControlCommand` into the immutable `TaskAuthority` Receipt.

use std::error::Error;
use std::fmt;
use std::time::Duration;

use nlos_commit_coordinator::{
    RecoveryFailureAuthority as WorkerFailureAuthority, RecoveryWorkerHealth, RecoveryWorkerState,
    TaskAuthorityCommitRecoveryWorker,
};
use nlos_schema::sabi;
use nlos_schema::sabi::v1::{
    ArtifactRecoveryAlertStatus, ArtifactRecoveryMetrics, ArtifactRecoveryOperationsSnapshot,
    ControlCommandLifecycleState, Envelope, ReceiptReference, RecoveryFailureSummary,
    ResourceRecoveryAlertStatus, ResourceRecoveryMetrics, ResourceRecoveryOperationsSnapshot,
    RetryDirective, SabiErrorCode, SabiFailure, SabiRequestContext, SabiResponseContext,
    SemanticRecoveryAlertStatus, SemanticRecoveryMetrics, SemanticRecoveryOperationsSnapshot,
    SystemControlView, envelope,
};
use nlos_schema::{
    CommonSemanticsError, CompatibilityError, MAX_SYSTEM_CONTROL_FAILURES, MethodSemantics,
    REQUEST_ID_BYTES, decode_get_system_control_request, decode_submit_control_command_request,
    encode_artifact_recovery_operations_snapshot, encode_control_command_result,
    encode_resource_recovery_operations_snapshot, encode_semantic_recovery_operations_snapshot,
    system_control_schema_identity, validate_sabi_request_context,
};
use nlos_task::{
    ArtifactCommitPlanId, ArtifactRecoveryAlertAcknowledgeRequest, ArtifactRecoveryFailureSource,
    GroupMemberType, GroupState, MembershipState, ResourceCommitPlanId,
    ResourceRecoveryAlertAcknowledgeRequest, ResourceRecoveryFailureSource,
    ResourceRecoveryResumeRequest, SemanticCommitPlanId, SemanticRecoveryAlertAcknowledgeRequest,
    SemanticRecoveryFailureSource, SemanticRecoveryResumeRequest, SqliteTaskAuthority, TaskGroupId,
    TaskStoreError, semantic_recovery_resume_reference,
};
use nlos_types::{IdempotencyKey, PrincipalId, ReceiptId};

pub const SYSTEM_CONTROL_SERVICE: &str = "system_control";
pub const GET_METHOD: &str = "get";
pub const SUBMIT_METHOD: &str = "submit";

/// Minimal typed control-plane prefix ([§25.3] of the architecture master
/// plan). Every [`control::ControlCommand`] — whether dispatched in-process
/// or through the `system-control-cli` binary — compiles to the same SABI
/// envelope and is answered by the same [`RecoverySystemControl`] handler
/// path, so GUI/NL/CLI/API parity degrades to one code path by construction.
///
/// [§25.3]: <https://github.com/cty12356541/llmos/blob/main/docs/design/06-架构设计总纲-v0.5.md>
pub mod control;

/// Restricted-grammar natural-language control prefix ([§1.3] of the
/// architecture master plan): a whitelist compiler front-end that turns
/// simple English/Chinese imperative sentences into the same
/// [`control::ControlCommand`] the CLI and structured API dispatch. It is a
/// compiler, not a privileged path — no fuzzy matching, no free-form NLU,
/// and out-of-grammar input is a typed rejection naming the legal forms.
///
/// [§1.3]: <https://github.com/cty12356541/llmos/blob/main/docs/design/06-架构设计总纲-v0.5.md>
pub mod nl;

/// Optional [`control::ProcessInspector`] adapter backed by
/// [`nlos_process::ProcessAuthority`] (`process` feature).
#[cfg(feature = "process")]
pub mod process_inspector;

/// Optional [`control::ResourceInspector`] adapter backed by
/// [`nlos_resource::ResourceAuthority`] (`resource` feature).
#[cfg(feature = "resource")]
pub mod resource_inspector;

/// Optional [`control::ApplicationInspector`] adapter backed by
/// [`nlos_application::ApplicationAuthority`] (`application` feature,
/// W38-A11 C-APP-CONTROL GET 后片).
#[cfg(feature = "application")]
pub mod application_inspector;

/// Optional [`TaskNodeInspectSource`] adapter backed by the durable
/// [`nlos_plan::SqlitePlanAuthority`] (`plan` feature, W32-G).
#[cfg(feature = "plan")]
pub mod plan_inspector;

/// Optional [`ExecutionFiberInspectSource`] adapter backed by the
/// [`nlos_runtime_tokio::TokioRuntimeAdapter`] snapshot surface (`runtime`
/// feature, W32-G).
#[cfg(feature = "runtime")]
pub mod fiber_inspector;

/// Optional [`TopicInspectSource`] adapter backed by the durable
/// [`nlos_topic::TopicAuthority`] rows (`topic` feature, W32-G).
#[cfg(feature = "topic")]
pub mod topic_inspector;

/// Optional [`OperationInspectSource`] adapter backed by the durable
/// [`nlos_store::SqliteOperationStore`] state-machine rows (`store`
/// feature, W32-G).
#[cfg(feature = "store")]
pub mod operation_inspector;

/// Optional kill-arm [`OperationCommandExecutor`] backed by the durable
/// [`nlos_process::ProcessAuthority`] platform-kill path and the
/// [`nlos_process::SupervisorPidRegistry`] (`process` feature, W29-D).
#[cfg(feature = "process")]
pub mod process_kill_executor;

/// Optional throttle-arm [`OperationCommandExecutor`] backed by
/// [`nlos_resource::ResourceAuthority`] demand rows (`resource` feature,
/// W29-D).
#[cfg(feature = "resource")]
pub mod resource_throttle_executor;

/// Reclaim-arm [`OperationCommandExecutor`] driving the nlos-task
/// working-set reclaim entry (W29-D; `nlos-task` is a core dependency, so
/// this module needs no feature gate).
pub mod working_set_reclaim_executor;

/// Optional [`ApplicationCommandExecutor`] adapter backed by the real
/// [`nlos_application::ApplicationAuthority`] lifecycle transitions
/// (`application` feature, W35-P11 移交#11 前片).
#[cfg(feature = "application")]
pub mod application_lifecycle_executor;

/// Domain-separated `ReceiptId` derivation shared by the authority-backed
/// operation executors.
mod executor_receipt;

/// Deterministic OpenMetrics text exposition (`text/plain; version=0.0.4`)
/// for the recovery metrics catalog: the first concrete backend for
/// [`RecoveryMetricsSink`], with no scraping transport. See the module
/// documentation for the renderer contract and remaining B-TASK-006M scope.
pub mod openmetrics;

/// ADR-0011 opt-in authenticated control-plane entry points (Unix, `cli`
/// feature). Strictly additive: the local trust-domain paths above keep
/// their exact semantics; this module adds one authenticated serve variant
/// and one authenticated dispatch client.
#[cfg(all(unix, feature = "cli"))]
pub mod auth;

/// Discriminates the operation-level arms routed through
/// [`RecoverySystemControl::execute_operation_control`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OperationArm {
    Pause,
    Resume,
    Cancel,
    Kill,
    Throttle { throttle_percent: u64 },
    Reclaim,
}

/// Discriminates the application-lifecycle arms routed through
/// [`RecoverySystemControl::execute_application_control`] (W35-P11).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ApplicationArm {
    Disable,
    Uninstall,
}

/// Policy boundary used by every `SystemControl` entry point. Implementations
/// are expected to validate the supplied capability handles against their
/// authority; mere handle presence is not authorization.
pub trait SystemControlAuthorizer {
    /// Authorizes one read-only recovery operations snapshot.
    ///
    /// # Errors
    ///
    /// Returns a static policy class safe for the local service log.
    fn authorize_get(
        &self,
        context: &SabiRequestContext,
        request: &sabi::v1::GetSystemControlRequest,
    ) -> Result<(), &'static str>;

    /// Authorizes one state-changing `ControlCommand`.
    ///
    /// # Errors
    ///
    /// Returns a static policy class safe for the local service log.
    fn authorize_submit(
        &self,
        context: &SabiRequestContext,
        command: &sabi::v1::ControlCommand,
    ) -> Result<(), &'static str>;
}

pub trait RecoveryHealthSource {
    fn recovery_health(&self) -> RecoveryWorkerHealth;
}

/// One operation-level control request as handed to the
/// [`OperationCommandExecutor`] seam: the 16-byte operational target, its
/// explicit CAS expectation, the issuing principal, the §25.3 idempotency
/// identity, and the handler's wall-clock reading.
pub struct OperationControlRequest {
    pub target_id: [u8; 16],
    pub expected_generation_or_revision: u64,
    pub issuer_principal_id: [u8; 16],
    pub idempotency_key: [u8; 16],
    pub requested_at_ms: i64,
}

/// Pluggable execution seam for the operation-level `ControlCommand` arms.
/// W28-D landed the pause/resume/cancel command surface; W29-D adds the
/// kill/throttle/reclaim arms and wires the first real executors (see
/// [`crate::process_kill_executor`], [`crate::resource_throttle_executor`],
/// and [`crate::working_set_reclaim_executor`]). The command surface — wire
/// arms, envelope compilation, authorization, idempotency binding, typed
/// receipts — stays owned by the `SystemControl.submit` handler;
/// implementations of this trait own the actual authority transitions. The
/// default [`UnwiredOperationCommandExecutor`] refuses fail-closed,
/// mirroring the unwired inspector stubs. `Send + Sync` is part of the
/// contract: handlers are held across async IPC service loops.
pub trait OperationCommandExecutor: Send + Sync {
    /// Pauses the operational target under the explicit CAS expectation.
    ///
    /// # Errors
    ///
    /// Returns a bounded, crossing-safe [`SabiFailure`] when the backing
    /// executor rejects the transition (absent target, CAS mismatch, state
    /// mismatch, or an unwired backend).
    fn pause_operation(&self, request: OperationControlRequest) -> Result<ReceiptId, SabiFailure>;
    /// Resumes the operational target; same contract as
    /// [`Self::pause_operation`].
    ///
    /// # Errors
    ///
    /// Returns a bounded [`SabiFailure`] for a rejected transition.
    fn resume_operation(&self, request: OperationControlRequest) -> Result<ReceiptId, SabiFailure>;
    /// Cancels the operational target; same contract as
    /// [`Self::pause_operation`].
    ///
    /// # Errors
    ///
    /// Returns a bounded [`SabiFailure`] for a rejected transition.
    fn cancel_operation(&self, request: OperationControlRequest) -> Result<ReceiptId, SabiFailure>;
    /// Kills the operational target (W29-D). For the process authority
    /// executor the target is a `ProcessId` and the CAS expectation is its
    /// process generation; the receipt id is derived from the authority's
    /// durable platform-kill receipt.
    ///
    /// # Errors
    ///
    /// Returns a bounded [`SabiFailure`] when the backing authority rejects
    /// the kill (absent process, generation CAS mismatch, terminal binding,
    /// absent supervisor pid mapping, or an unwired backend).
    fn kill_operation(&self, request: OperationControlRequest) -> Result<ReceiptId, SabiFailure>;
    /// Throttles the operational target down to `throttle_percent` percent
    /// of its current declared demand (W29-D; whole percent `1..=100`,
    /// already enforced before the wire). The resource demand executor
    /// reads the target's reservation demand and quote capacity from the
    /// `ResourceAuthority` and derives the receipt id from the
    /// authority-driven before/after adjustment.
    ///
    /// # Errors
    ///
    /// Returns a bounded [`SabiFailure`] when the backing authority rejects
    /// the adjustment (absent reservation or quote, revision CAS mismatch,
    /// or an unwired backend).
    fn throttle_operation(
        &self,
        request: OperationControlRequest,
        throttle_percent: u64,
    ) -> Result<ReceiptId, SabiFailure>;
    /// Reclaims the operational target's working set (W29-D). The
    /// working-set executor drives the nlos-task reclaim
    /// advisory→plan→execute entry against the observed occupancy and
    /// derives the receipt id from its typed outcome.
    ///
    /// # Errors
    ///
    /// Returns a bounded [`SabiFailure`] when there is no reclaim advisory
    /// (the working set is below the soft threshold) or the backend is
    /// unwired.
    fn reclaim_operation(&self, request: OperationControlRequest)
    -> Result<ReceiptId, SabiFailure>;
}

/// Default stub used when no operation-level execution backend is wired.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UnwiredOperationCommandExecutor;

impl OperationCommandExecutor for UnwiredOperationCommandExecutor {
    fn pause_operation(&self, _: OperationControlRequest) -> Result<ReceiptId, SabiFailure> {
        Err(unwired_operation_failure())
    }

    fn resume_operation(&self, _: OperationControlRequest) -> Result<ReceiptId, SabiFailure> {
        Err(unwired_operation_failure())
    }

    fn cancel_operation(&self, _: OperationControlRequest) -> Result<ReceiptId, SabiFailure> {
        Err(unwired_operation_failure())
    }

    fn kill_operation(&self, _: OperationControlRequest) -> Result<ReceiptId, SabiFailure> {
        Err(unwired_operation_failure())
    }

    fn throttle_operation(
        &self,
        _: OperationControlRequest,
        _: u64,
    ) -> Result<ReceiptId, SabiFailure> {
        Err(unwired_operation_failure())
    }

    fn reclaim_operation(&self, _: OperationControlRequest) -> Result<ReceiptId, SabiFailure> {
        Err(unwired_operation_failure())
    }
}

fn unwired_operation_failure() -> SabiFailure {
    SabiFailure {
        code: SabiErrorCode::NotFound.into(),
        retry: RetryDirective::DoNotRetry.into(),
        safe_message: "operation control execution backend is not wired".to_owned(),
    }
}

/// One application-lifecycle control request as handed to the
/// [`ApplicationCommandExecutor`] seam (W35-P11, 移交#11 前片): the 16-byte
/// package identity (the application singleton is authority-derived from
/// it), its explicit CAS expectation — the application's current
/// installation generation — the issuing principal, the §25.3 idempotency
/// identity, and the handler's wall-clock reading.
pub struct ApplicationControlRequest {
    pub package_id: [u8; 16],
    pub expected_generation_or_revision: u64,
    pub issuer_principal_id: [u8; 16],
    pub idempotency_key: [u8; 16],
    pub requested_at_ms: i64,
}

/// Pluggable execution seam for the application-lifecycle
/// `ControlCommand` arms (W35-P11). The command surface — wire arms,
/// envelope compilation, authorization, idempotency binding, typed
/// receipts — stays owned by the `SystemControl.submit` handler;
/// implementations of this trait own the actual authority transitions.
/// The real adapter ([`crate::application_lifecycle_executor`], `application`
/// feature) drives the `nlos-application` authority; the default
/// [`UnwiredApplicationCommandExecutor`] refuses fail-closed, mirroring the
/// operation executor stub. `Send + Sync` is part of the contract: handlers
/// are held across async IPC service loops.
pub trait ApplicationCommandExecutor: Send + Sync {
    /// Disables the installed application under the package identity and
    /// explicit installation-generation CAS expectation. Disable has no
    /// activity gate: the `installed → disabled` transition is reversible
    /// by rollback.
    ///
    /// # Errors
    ///
    /// Returns a bounded, crossing-safe [`SabiFailure`] when the backing
    /// authority rejects the transition (absent application, generation CAS
    /// mismatch, terminal state, or an unwired backend).
    fn disable_application(
        &self,
        request: ApplicationControlRequest,
    ) -> Result<ReceiptId, SabiFailure>;
    /// Uninstalls the installed or disabled application; same addressing
    /// contract, but the real executor runs the W27-D task-activity gate —
    /// the `SqliteTaskAuthority` live query — before the terminal
    /// transition commits.
    ///
    /// # Errors
    ///
    /// Returns a bounded [`SabiFailure`] when the backing authority rejects
    /// the transition (absent application, generation CAS mismatch,
    /// outstanding task activity, terminal state, or an unwired backend).
    fn uninstall_application(
        &self,
        request: ApplicationControlRequest,
    ) -> Result<ReceiptId, SabiFailure>;
}

/// Default stub used when no application-lifecycle execution backend is
/// wired.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UnwiredApplicationCommandExecutor;

impl ApplicationCommandExecutor for UnwiredApplicationCommandExecutor {
    fn disable_application(&self, _: ApplicationControlRequest) -> Result<ReceiptId, SabiFailure> {
        Err(unwired_application_failure())
    }

    fn uninstall_application(
        &self,
        _: ApplicationControlRequest,
    ) -> Result<ReceiptId, SabiFailure> {
        Err(unwired_application_failure())
    }
}

fn unwired_application_failure() -> SabiFailure {
    SabiFailure {
        code: SabiErrorCode::NotFound.into(),
        retry: RetryDirective::DoNotRetry.into(),
        safe_message: "application control execution backend is not wired".to_owned(),
    }
}

fn unwired_layer_failure() -> SabiFailure {
    SabiFailure {
        code: SabiErrorCode::NotFound.into(),
        retry: RetryDirective::DoNotRetry.into(),
        safe_message: "layer inspection backend is not wired".to_owned(),
    }
}

/// Pluggable read-only `TaskNode` inspection seam (W32-G, B5-3). The handler
/// owns only the envelope, authorization, and projection; hosts wire an
/// adapter over the plan authority (see [`crate::plan_inspector`] with the
/// `plan` feature) or leave the default [`UnwiredTaskNodeInspectSource`] in
/// place. `Send + Sync` is part of the contract: handlers are held across
/// async IPC service loops.
pub trait TaskNodeInspectSource: Send + Sync {
    /// Returns one bounded plan-node snapshot or a sanitized failure.
    ///
    /// # Errors
    ///
    /// Returns [`SabiFailure`] when the backing authority rejects the read
    /// (absent node, absent plan, or an unwired backend).
    fn inspect_task_node(
        &self,
        plan_id: [u8; 16],
        node_id: [u8; 16],
    ) -> Result<control::TaskNodeInspection, SabiFailure>;
}

/// Default stub used when no `TaskNode` inspection backend is wired.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UnwiredTaskNodeInspectSource;

impl TaskNodeInspectSource for UnwiredTaskNodeInspectSource {
    fn inspect_task_node(
        &self,
        _: [u8; 16],
        _: [u8; 16],
    ) -> Result<control::TaskNodeInspection, SabiFailure> {
        Err(unwired_layer_failure())
    }
}

/// Pluggable read-only execution-fiber inspection seam (W32-G, B5-3) over
/// the runtime snapshot surface (see [`crate::fiber_inspector`] with the
/// `runtime` feature).
pub trait ExecutionFiberInspectSource: Send + Sync {
    /// Returns one bounded fiber snapshot — state, lifecycle phase, and
    /// whole-millisecond usage meters — or a sanitized failure.
    ///
    /// # Errors
    ///
    /// Returns [`SabiFailure`] when the runtime rejects the handle (unknown
    /// or reaped fiber, or an unwired backend).
    fn inspect_execution_fiber(
        &self,
        fiber_id: [u8; 16],
        generation: u64,
    ) -> Result<control::ExecutionFiberInspection, SabiFailure>;
}

/// Default stub used when no execution-fiber inspection backend is wired.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UnwiredExecutionFiberInspectSource;

impl ExecutionFiberInspectSource for UnwiredExecutionFiberInspectSource {
    fn inspect_execution_fiber(
        &self,
        _: [u8; 16],
        _: u64,
    ) -> Result<control::ExecutionFiberInspection, SabiFailure> {
        Err(unwired_layer_failure())
    }
}

/// Pluggable read-only topic inspection seam (W32-G, B5-3) over the durable
/// topic rows (see [`crate::topic_inspector`] with the `topic` feature).
pub trait TopicInspectSource: Send + Sync {
    /// Returns one bounded durable topic snapshot or a sanitized failure.
    ///
    /// # Errors
    ///
    /// Returns [`SabiFailure`] when the backing authority rejects the read
    /// (absent topic or an unwired backend).
    fn inspect_topic(&self, topic_id: [u8; 16]) -> Result<control::TopicInspection, SabiFailure>;
}

/// Default stub used when no topic inspection backend is wired.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UnwiredTopicInspectSource;

impl TopicInspectSource for UnwiredTopicInspectSource {
    fn inspect_topic(&self, _: [u8; 16]) -> Result<control::TopicInspection, SabiFailure> {
        Err(unwired_layer_failure())
    }
}

/// Pluggable read-only durable-operation inspection seam (W32-G, B5-3) over
/// the operation store's state-machine rows (see [`crate::operation_inspector`]
/// with the `store` feature).
pub trait OperationInspectSource: Send + Sync {
    /// Returns one bounded durable operation snapshot — state, cancel epoch,
    /// owner fiber handle, and terminal outcome receipt — or a sanitized
    /// failure.
    ///
    /// # Errors
    ///
    /// Returns [`SabiFailure`] when the backing store rejects the read
    /// (absent or stale-generation row or an unwired backend).
    fn inspect_operation(
        &self,
        operation_id: [u8; 16],
        generation: u64,
    ) -> Result<control::DurableOperationInspection, SabiFailure>;
}

/// Default stub used when no durable-operation inspection backend is wired.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct UnwiredOperationInspectSource;

impl OperationInspectSource for UnwiredOperationInspectSource {
    fn inspect_operation(
        &self,
        _: [u8; 16],
        _: u64,
    ) -> Result<control::DurableOperationInspection, SabiFailure> {
        Err(unwired_layer_failure())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryCounter {
    CompletedCycles,
    InspectedPlans,
    FinalizedPlans,
    SemanticPlansInspected,
    SemanticPlansFinalized,
    ResourcePlansInspected,
    ResourcePlansFinalized,
}

impl RecoveryCounter {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::CompletedCycles => "nlos_artifact_recovery_cycles_total",
            Self::InspectedPlans => "nlos_artifact_recovery_plans_inspected_total",
            Self::FinalizedPlans => "nlos_artifact_recovery_plans_finalized_total",
            Self::SemanticPlansInspected => "nlos_semantic_recovery_plans_inspected_total",
            Self::SemanticPlansFinalized => "nlos_semantic_recovery_plans_finalized_total",
            Self::ResourcePlansInspected => "nlos_resource_recovery_plans_inspected_total",
            Self::ResourcePlansFinalized => "nlos_resource_recovery_plans_finalized_total",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryGauge {
    ConsecutiveFailedCycles,
    RetryDelayMilliseconds,
    DurableRetrying,
    DurableEscalated,
    DurableUnacknowledgedEscalated,
    DurableResolved,
    ArtifactDomainFaulted,
    SemanticConsecutiveFailedCycles,
    SemanticDurableRetrying,
    SemanticDurableEscalated,
    SemanticDurableUnacknowledgedEscalated,
    SemanticDurableResolved,
    SemanticDomainFaulted,
    ResourceConsecutiveFailedCycles,
    ResourceDurableRetrying,
    ResourceDurableEscalated,
    ResourceDurableUnacknowledgedEscalated,
    ResourceDurableResolved,
    ResourceDomainFaulted,
}

impl RecoveryGauge {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::ConsecutiveFailedCycles => "nlos_artifact_recovery_consecutive_failed_cycles",
            Self::RetryDelayMilliseconds => "nlos_artifact_recovery_retry_delay_milliseconds",
            Self::DurableRetrying => "nlos_artifact_recovery_durable_retrying",
            Self::DurableEscalated => "nlos_artifact_recovery_durable_escalated",
            Self::DurableUnacknowledgedEscalated => {
                "nlos_artifact_recovery_durable_unacknowledged_escalated"
            }
            Self::DurableResolved => "nlos_artifact_recovery_durable_resolved",
            Self::ArtifactDomainFaulted => "nlos_artifact_recovery_domain_faulted",
            Self::SemanticConsecutiveFailedCycles => {
                "nlos_semantic_recovery_consecutive_failed_cycles"
            }
            Self::SemanticDurableRetrying => "nlos_semantic_recovery_durable_retrying",
            Self::SemanticDurableEscalated => "nlos_semantic_recovery_durable_escalated",
            Self::SemanticDurableUnacknowledgedEscalated => {
                "nlos_semantic_recovery_durable_unacknowledged_escalated"
            }
            Self::SemanticDurableResolved => "nlos_semantic_recovery_durable_resolved",
            Self::SemanticDomainFaulted => "nlos_semantic_recovery_domain_faulted",
            Self::ResourceConsecutiveFailedCycles => {
                "nlos_resource_recovery_consecutive_failed_cycles"
            }
            Self::ResourceDurableRetrying => "nlos_resource_recovery_durable_retrying",
            Self::ResourceDurableEscalated => "nlos_resource_recovery_durable_escalated",
            Self::ResourceDurableUnacknowledgedEscalated => {
                "nlos_resource_recovery_durable_unacknowledged_escalated"
            }
            Self::ResourceDurableResolved => "nlos_resource_recovery_durable_resolved",
            Self::ResourceDomainFaulted => "nlos_resource_recovery_domain_faulted",
        }
    }
}

/// Backend-neutral exporter boundary. Metric names and kinds are fixed here;
/// a host adapter may render them as `OpenMetrics`, ETW, signposts, or another
/// platform facility without changing the authority model.
pub trait RecoveryMetricsSink {
    type Error;

    /// Records the current worker lifecycle.
    ///
    /// # Errors
    ///
    /// Returns a backend-specific export error.
    fn record_worker_state(&mut self, state: RecoveryWorkerState) -> Result<(), Self::Error>;
    /// Sets one monotonic counter to its authoritative total.
    ///
    /// # Errors
    ///
    /// Returns a backend-specific export error.
    fn set_counter_total(
        &mut self,
        counter: RecoveryCounter,
        value: u64,
    ) -> Result<(), Self::Error>;
    /// Sets one point-in-time gauge.
    ///
    /// # Errors
    ///
    /// Returns a backend-specific export error.
    fn set_gauge(&mut self, gauge: RecoveryGauge, value: u64) -> Result<(), Self::Error>;
}

#[derive(Debug)]
pub enum RecoveryMetricsExportError<E> {
    Task(TaskStoreError),
    Sink(E),
}

impl RecoveryHealthSource for TaskAuthorityCommitRecoveryWorker {
    fn recovery_health(&self) -> RecoveryWorkerHealth {
        self.health()
    }
}

#[derive(Debug)]
pub enum SystemControlError {
    Schema(CompatibilityError),
    Common(CommonSemanticsError),
    Task(TaskStoreError),
    UnknownMethod,
    AuthorizationDenied(&'static str),
    CallerIssuerMismatch,
    CommandIdempotencyMismatch,
    InvalidRecoveryAlert,
    /// The ADR-0011 authenticated exchange carried an unbounded correlation
    /// id, so the clock-anchored wall reading cannot be pinned to it.
    /// Never produced by the local trust-domain paths.
    UnboundedCorrelation,
    /// The `AuthorityClock` refused to serve the wall reading for an
    /// ADR-0011 authenticated exchange (fail-closed; no time is guessed).
    /// Never produced by the local trust-domain paths.
    ClockWallUnavailable,
    /// No [`OperationCommandExecutor`] is wired, so an operation-level
    /// command arm refuses fail-closed (W29-D wires the real executors).
    OperationControlExecutionUnwired,
    /// The wired [`OperationCommandExecutor`] rejected the transition with
    /// an already-bounded, crossing-safe failure; [`Self::to_sabi_failure`]
    /// forwards it verbatim.
    OperationExecution(SabiFailure),
    /// No [`ApplicationCommandExecutor`] is wired, so an
    /// application-lifecycle command arm refuses fail-closed (W35-P11
    /// wires the real executor behind the `application` feature).
    ApplicationControlExecutionUnwired,
    /// The wired [`ApplicationCommandExecutor`] rejected the transition
    /// with an already-bounded, crossing-safe failure;
    /// [`Self::to_sabi_failure`] forwards it verbatim.
    ApplicationExecution(SabiFailure),
    /// No backing source is wired for one W32-G per-layer inspect view, so
    /// the read refuses fail-closed.
    LayerInspectionUnwired,
    /// A wired per-layer inspection source rejected the read with an
    /// already-bounded, crossing-safe failure; [`Self::to_sabi_failure`]
    /// forwards it verbatim.
    LayerInspection(SabiFailure),
}

impl fmt::Display for SystemControlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Schema(error) => write!(formatter, "invalid SystemControl schema: {error}"),
            Self::Common(error) => write!(formatter, "invalid SystemControl context: {error}"),
            Self::Task(error) => write!(formatter, "TaskAuthority rejected SystemControl: {error}"),
            Self::UnknownMethod => formatter.write_str("unknown SystemControl service or method"),
            Self::AuthorizationDenied(reason) => {
                write!(formatter, "SystemControl authorization denied: {reason}")
            }
            Self::CallerIssuerMismatch => {
                formatter.write_str("ControlCommand issuer does not match authenticated caller")
            }
            Self::CommandIdempotencyMismatch => {
                formatter.write_str("ControlCommand ID does not match the request idempotency key")
            }
            Self::InvalidRecoveryAlert => {
                formatter.write_str("TaskAuthority returned an invalid recovery alert")
            }
            Self::UnboundedCorrelation => formatter
                .write_str("authenticated control exchange requires a bounded correlation id"),
            Self::ClockWallUnavailable => {
                formatter.write_str("authority clock refused to serve a wall reading")
            }
            Self::OperationControlExecutionUnwired => {
                formatter.write_str("operation control execution backend is not wired")
            }
            Self::OperationExecution(failure) => write!(
                formatter,
                "operation control execution rejected the command: {}",
                failure.safe_message
            ),
            Self::ApplicationControlExecutionUnwired => {
                formatter.write_str("application control execution backend is not wired")
            }
            Self::ApplicationExecution(failure) => write!(
                formatter,
                "application control execution rejected the command: {}",
                failure.safe_message
            ),
            Self::LayerInspectionUnwired => {
                formatter.write_str("layer inspection backend is not wired")
            }
            Self::LayerInspection(failure) => write!(
                formatter,
                "layer inspection source rejected the read: {}",
                failure.safe_message
            ),
        }
    }
}

impl Error for SystemControlError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Schema(error) => Some(error),
            Self::Common(error) => Some(error),
            Self::Task(error) => Some(error),
            Self::UnknownMethod
            | Self::AuthorizationDenied(_)
            | Self::CallerIssuerMismatch
            | Self::CommandIdempotencyMismatch
            | Self::InvalidRecoveryAlert
            | Self::UnboundedCorrelation
            | Self::ClockWallUnavailable
            | Self::OperationControlExecutionUnwired
            | Self::OperationExecution(_)
            | Self::ApplicationControlExecutionUnwired
            | Self::ApplicationExecution(_)
            | Self::LayerInspectionUnwired
            | Self::LayerInspection(_) => None,
        }
    }
}

impl From<CompatibilityError> for SystemControlError {
    fn from(error: CompatibilityError) -> Self {
        Self::Schema(error)
    }
}

impl From<CommonSemanticsError> for SystemControlError {
    fn from(error: CommonSemanticsError) -> Self {
        Self::Common(error)
    }
}

impl From<TaskStoreError> for SystemControlError {
    fn from(error: TaskStoreError) -> Self {
        Self::Task(error)
    }
}

impl SystemControlError {
    /// Maps one local rejection to the bounded common SABI failure class.
    ///
    /// The mapping deliberately never includes the source error's display
    /// text. `SQLite` messages, static authority reasons, and durable-record
    /// details are local diagnostics and must not cross the `SystemControl`
    /// boundary. A failed acknowledgement carries no receipt evidence, so a
    /// storage error may be retried with the original idempotency key while a
    /// contract or state error is terminal for that request.
    ///
    /// | Source | Code | Retry |
    /// |---|---|---|
    /// | schema/common contract | `INVALID_ARGUMENT` | `DO_NOT_RETRY` |
    /// | expired deadline | `DEADLINE` | `DO_NOT_RETRY` |
    /// | authorization/caller binding | `RIGHTS` | `DO_NOT_RETRY` |
    /// | command/idempotency binding | `CONFLICT` | `DO_NOT_RETRY` |
    /// | recovery object absent | `NOT_FOUND` | `DO_NOT_RETRY` |
    /// | recovery CAS/replay conflict | `CONFLICT` | `DO_NOT_RETRY` |
    /// | recovery lifecycle mismatch | `STATE` | `DO_NOT_RETRY` |
    /// | SQLite storage failure | `DURABILITY` | `RETRY_SAME_IDEMPOTENCY_KEY` |
    /// | unavailable durability/corrupt local state | `DURABILITY`/`DRIVER` | `DO_NOT_RETRY` |
    /// | unknown method | `NOT_SUPPORTED` | `DO_NOT_RETRY` |
    /// | operation executor unwired | `NOT_FOUND` | `DO_NOT_RETRY` |
    /// | executor rejection | bounded passthrough | bounded passthrough |
    /// | application executor unwired | `NOT_FOUND` | `DO_NOT_RETRY` |
    /// | application executor rejection | bounded passthrough | bounded passthrough |
    #[must_use]
    pub fn to_sabi_failure(&self) -> SabiFailure {
        let (code, retry, safe_message) = match self {
            Self::Schema(_) => (
                SabiErrorCode::InvalidArgument,
                RetryDirective::DoNotRetry,
                "request violates the SystemControl payload contract",
            ),
            Self::Common(CommonSemanticsError::DeadlineExpired) => (
                SabiErrorCode::Deadline,
                RetryDirective::DoNotRetry,
                "call deadline has expired",
            ),
            Self::Common(_) => (
                SabiErrorCode::InvalidArgument,
                RetryDirective::DoNotRetry,
                "request violates the common SABI contract",
            ),
            Self::AuthorizationDenied(_) | Self::CallerIssuerMismatch => (
                SabiErrorCode::Rights,
                RetryDirective::DoNotRetry,
                "SystemControl authorization denied",
            ),
            Self::CommandIdempotencyMismatch => (
                SabiErrorCode::Conflict,
                RetryDirective::DoNotRetry,
                "command identity conflicts with the idempotency key",
            ),
            Self::UnknownMethod => (
                SabiErrorCode::NotSupported,
                RetryDirective::DoNotRetry,
                "unknown SystemControl service or method",
            ),
            Self::InvalidRecoveryAlert => (
                SabiErrorCode::Driver,
                RetryDirective::DoNotRetry,
                "local recovery authority returned an invalid alert",
            ),
            Self::UnboundedCorrelation => (
                SabiErrorCode::InvalidArgument,
                RetryDirective::DoNotRetry,
                "authenticated control exchange requires a bounded correlation id",
            ),
            Self::ClockWallUnavailable => (
                SabiErrorCode::Driver,
                RetryDirective::DoNotRetry,
                "authority clock refused a wall reading; do not retry",
            ),
            Self::OperationControlExecutionUnwired => (
                SabiErrorCode::NotFound,
                RetryDirective::DoNotRetry,
                "operation control execution backend is not wired",
            ),
            Self::ApplicationControlExecutionUnwired => (
                SabiErrorCode::NotFound,
                RetryDirective::DoNotRetry,
                "application control execution backend is not wired",
            ),
            Self::LayerInspectionUnwired => (
                SabiErrorCode::NotFound,
                RetryDirective::DoNotRetry,
                "layer inspection backend is not wired",
            ),
            // The executor or inspection source already produced a bounded,
            // crossing-safe failure; forward its class, retry directive, and
            // message verbatim.
            Self::OperationExecution(failure)
            | Self::ApplicationExecution(failure)
            | Self::LayerInspection(failure) => {
                return failure.clone();
            }
            Self::Task(error) => task_store_failure(error),
        };
        SabiFailure {
            code: code.into(),
            retry: retry.into(),
            safe_message: safe_message.to_owned(),
        }
    }
}

#[allow(clippy::too_many_lines)]
fn task_store_failure(error: &TaskStoreError) -> (SabiErrorCode, RetryDirective, &'static str) {
    use TaskStoreError as Task;

    match error {
        Task::Sqlite(_) => (
            SabiErrorCode::Durability,
            RetryDirective::RetrySameIdempotencyKey,
            "task authority storage failure; retry with the same idempotency key",
        ),
        Task::DurabilityUnavailable { .. } => (
            SabiErrorCode::Durability,
            RetryDirective::DoNotRetry,
            "task authority durability configuration is unavailable",
        ),
        Task::TaskNotFound
        | Task::AttemptNotFound
        | Task::PermitNotFound
        | Task::ReceiptNotFound
        | Task::SnapshotReceiptNotFound
        | Task::ArtifactCommitPlanNotFound
        | Task::SemanticCommitPlanNotFound
        | Task::ResourceCommitPlanNotFound
        | Task::ArtifactRecoveryNotFound
        | Task::EffectSlotNotFound
        | Task::EffectPermitNotFound
        | Task::GroupNotFound
        | Task::GroupMemberNotFound
        | Task::ParticipantRegistryNotFound
        | Task::TaskWriteSetNotFound => (
            SabiErrorCode::NotFound,
            RetryDirective::DoNotRetry,
            "requested recovery authority object was not found",
        ),
        Task::ArtifactRecoveryCasMismatch { .. }
        | Task::SemanticRecoveryCasMismatch { .. }
        | Task::ResourceRecoveryCasMismatch { .. }
        | Task::IdempotencyConflict
        | Task::SnapshotConflict
        | Task::ArtifactPublicationConflict { .. }
        | Task::SemanticPublicationConflict { .. }
        | Task::TaskWriteSetConflict { .. }
        | Task::TaskWriteSetReadConflict
        | Task::TaskWriteSetSemanticReadConflict
        | Task::TaskWriteSetResourceReservationConflict
        | Task::HistoryConflict
        | Task::ParticipantRegistryCasMismatch
        | Task::ParticipantEndpointConflict
        | Task::StaleMembershipGeneration { .. } => (
            SabiErrorCode::Conflict,
            RetryDirective::DoNotRetry,
            "request conflicts with durable recovery state",
        ),
        Task::ArtifactCommitPlanNotReady { .. }
        | Task::SemanticCommitPlanNotReady { .. }
        | Task::InvalidArtifactRecoveryState { .. }
        | Task::InvalidSemanticRecoveryState { .. }
        | Task::InvalidResourceRecoveryState { .. }
        | Task::AuthorityLeaseHeld
        | Task::AuthorityLeaseExpired
        | Task::AuthorityLeaseRequired
        | Task::AuthorityLeaseBindingMismatch
        | Task::GroupPublicationInFlight
        | Task::InvalidAttemptState { .. }
        | Task::TaskCancelled
        | Task::NotPermitHolder
        | Task::PermitNotIssued
        | Task::StaleTaskHead
        | Task::FenceRegression
        | Task::PermitEpochMismatch
        | Task::CancellationCommitted { .. }
        | Task::InvalidEffectSlotState { .. }
        | Task::DispatchTokenConsumed
        | Task::OutstandingEffectSlots { .. }
        | Task::Quarantined
        | Task::AdoptionScopeViolation
        | Task::EffectAlreadyClosed
        | Task::RequiredEffectUnsatisfied { .. }
        | Task::InvalidReconcileState { .. }
        | Task::PermitHasEffects { .. }
        | Task::GroupSealed
        | Task::GroupNotOpen { .. }
        | Task::InvalidGroupState { .. }
        | Task::GroupQuarantinedChild
        | Task::ParticipantRegistryFrozen { .. }
        | Task::ParticipantRegistryBindingMissing
        | Task::ParticipantRegistryBindingMismatch
        | Task::BarrierObservationUnsigned => (
            SabiErrorCode::State,
            RetryDirective::DoNotRetry,
            "recovery authority state rejects this request",
        ),
        Task::InvalidArtifactRecoveryPolicy { .. }
        | Task::InvalidSemanticRecoveryPolicy { .. }
        | Task::InvalidResourceRecoveryPolicy { .. }
        | Task::InvalidAuthorityLease { .. }
        | Task::InvalidSnapshotReceipt { .. }
        | Task::InvalidArtifactPublicationPlan { .. }
        | Task::InvalidSemanticPublicationPlan { .. }
        | Task::InvalidGeneration
        | Task::InvalidEffectSet { .. }
        | Task::DispatchTokenMismatch
        | Task::ConditionNotBound
        | Task::GroupCycle
        | Task::GroupDepthExceeded
        | Task::GroupFanoutExceeded
        | Task::UnsupportedGroupMode
        | Task::InvalidGroupSpec { .. } => (
            SabiErrorCode::InvalidArgument,
            RetryDirective::DoNotRetry,
            "request violates the recovery authority contract",
        ),
        Task::AuthorityLeaseFenced => (
            SabiErrorCode::Fenced,
            RetryDirective::DoNotRetry,
            "task authority lease is fenced",
        ),
        Task::CorruptRecord(_) | Task::UnsupportedSchema(_) | Task::LockPoisoned => (
            SabiErrorCode::Driver,
            RetryDirective::DoNotRetry,
            "local task authority state is invalid",
        ),
        _ => (
            SabiErrorCode::Driver,
            RetryDirective::DoNotRetry,
            "local task authority defect; do not retry",
        ),
    }
}

pub struct RecoverySystemControl<'a, H, A> {
    tasks: &'a SqliteTaskAuthority,
    health: &'a H,
    authorizer: &'a A,
    operation_executor: Option<&'a dyn OperationCommandExecutor>,
    application_executor: Option<&'a dyn ApplicationCommandExecutor>,
    task_node_source: Option<&'a dyn TaskNodeInspectSource>,
    fiber_source: Option<&'a dyn ExecutionFiberInspectSource>,
    topic_source: Option<&'a dyn TopicInspectSource>,
    operation_source: Option<&'a dyn OperationInspectSource>,
}

impl<'a, H, A> RecoverySystemControl<'a, H, A>
where
    H: RecoveryHealthSource,
    A: SystemControlAuthorizer,
{
    #[must_use]
    pub const fn new(tasks: &'a SqliteTaskAuthority, health: &'a H, authorizer: &'a A) -> Self {
        Self {
            tasks,
            health,
            authorizer,
            operation_executor: None,
            application_executor: None,
            task_node_source: None,
            fiber_source: None,
            topic_source: None,
            operation_source: None,
        }
    }

    /// Wires the pluggable operation-level execution seam
    /// ([`OperationCommandExecutor`]). Without it the pause/resume/cancel
    /// arms refuse fail-closed with a typed `NOT_FOUND` failure.
    #[must_use]
    pub const fn with_operation_executor(
        mut self,
        executor: &'a dyn OperationCommandExecutor,
    ) -> Self {
        self.operation_executor = Some(executor);
        self
    }

    /// Wires the pluggable application-lifecycle execution seam
    /// ([`ApplicationCommandExecutor`], W35-P11). Without it the
    /// disable/uninstall arms refuse fail-closed with a typed `NOT_FOUND`
    /// failure.
    #[must_use]
    pub const fn with_application_executor(
        mut self,
        executor: &'a dyn ApplicationCommandExecutor,
    ) -> Self {
        self.application_executor = Some(executor);
        self
    }

    /// Wires the pluggable `TaskNode` inspection seam (W32-G, B5-3). Without
    /// it the `TASK_NODE` view refuses fail-closed with a typed `NOT_FOUND`
    /// failure.
    #[must_use]
    pub const fn with_task_node_source(mut self, source: &'a dyn TaskNodeInspectSource) -> Self {
        self.task_node_source = Some(source);
        self
    }

    /// Wires the pluggable execution-fiber inspection seam (W32-G, B5-3).
    #[must_use]
    pub const fn with_execution_fiber_source(
        mut self,
        source: &'a dyn ExecutionFiberInspectSource,
    ) -> Self {
        self.fiber_source = Some(source);
        self
    }

    /// Wires the pluggable topic inspection seam (W32-G, B5-3).
    #[must_use]
    pub const fn with_topic_source(mut self, source: &'a dyn TopicInspectSource) -> Self {
        self.topic_source = Some(source);
        self
    }

    /// Wires the pluggable durable-operation inspection seam (W32-G, B5-3).
    #[must_use]
    pub const fn with_operation_source(mut self, source: &'a dyn OperationInspectSource) -> Self {
        self.operation_source = Some(source);
        self
    }

    /// Handles one validated-envelope-shaped request without introducing a
    /// transport-specific RPC. The returned Envelope retains the request ID.
    ///
    /// # Errors
    ///
    /// Returns typed schema/common-context/policy/authority errors. A failed
    /// request never manufactures a success Receipt.
    pub fn handle(
        &self,
        request: &Envelope,
        now_monotonic_ns: u64,
        now_wall_ms: i64,
    ) -> Result<Envelope, SystemControlError> {
        if request.service != SYSTEM_CONTROL_SERVICE {
            return Err(SystemControlError::UnknownMethod);
        }
        match request.method.as_str() {
            GET_METHOD => self.handle_get(request, now_monotonic_ns),
            SUBMIT_METHOD => self.handle_submit(request, now_monotonic_ns, now_wall_ms),
            _ => Err(SystemControlError::UnknownMethod),
        }
    }

    /// Handles one request for a local IPC adapter and always returns a typed
    /// response envelope. Handler errors are converted with
    /// [`failure_envelope`] before framing; transport I/O failures remain the
    /// caller's responsibility. Use [`Self::handle`] when the caller needs
    /// to inspect the local error instead.
    #[must_use]
    pub fn handle_for_ipc(
        &self,
        request: &Envelope,
        now_monotonic_ns: u64,
        now_wall_ms: i64,
    ) -> Envelope {
        match self.handle(request, now_monotonic_ns, now_wall_ms) {
            Ok(response) => response,
            Err(error) => failure_envelope(request, &error),
        }
    }

    /// Exports one authoritative tri-domain metrics snapshot through a
    /// backend-neutral sink: the artifact catalog first, then the semantic
    /// catalog, then the resource catalog. Durable gauges are read from the
    /// live `TaskAuthority` summaries of all three recovery ledgers; every
    /// other value comes from a single worker health generation. Diagnostic
    /// strings and per-plan identities are not metrics.
    ///
    /// # Errors
    ///
    /// Returns a `TaskAuthority` read failure or the first sink error.
    #[allow(clippy::too_many_lines)] // The tri-domain catalog stays flat in one auditable export order.
    pub fn export_metrics<S: RecoveryMetricsSink>(
        &self,
        sink: &mut S,
    ) -> Result<(), RecoveryMetricsExportError<S::Error>> {
        let mut health = self.health.recovery_health();
        let artifact = self
            .tasks
            .summarize_artifact_recovery()
            .map_err(RecoveryMetricsExportError::Task)?;
        health.durable_retrying = artifact.retrying;
        health.durable_escalated = artifact.escalated;
        health.durable_unacknowledged_escalated = artifact.unacknowledged_escalated;
        health.durable_resolved = artifact.resolved;
        let semantic = self
            .tasks
            .summarize_semantic_recovery()
            .map_err(RecoveryMetricsExportError::Task)?;
        health.semantic_durable_retrying = semantic.retrying;
        health.semantic_durable_escalated = semantic.escalated;
        health.semantic_durable_unacknowledged_escalated = semantic.unacknowledged_escalated;
        health.semantic_durable_resolved = semantic.resolved;
        let resource = self
            .tasks
            .summarize_resource_recovery()
            .map_err(RecoveryMetricsExportError::Task)?;
        health.resource_durable_retrying = resource.retrying;
        health.resource_durable_escalated = resource.escalated;
        health.resource_durable_unacknowledged_escalated = resource.unacknowledged_escalated;
        health.resource_durable_resolved = resource.resolved;
        sink.record_worker_state(health.state)
            .map_err(RecoveryMetricsExportError::Sink)?;
        for (counter, value) in [
            (RecoveryCounter::CompletedCycles, health.completed_cycles),
            (RecoveryCounter::InspectedPlans, health.total_inspected),
            (RecoveryCounter::FinalizedPlans, health.total_finalized),
            (
                RecoveryCounter::SemanticPlansInspected,
                health.semantic_total_inspected,
            ),
            (
                RecoveryCounter::SemanticPlansFinalized,
                health.semantic_total_finalized,
            ),
            (
                RecoveryCounter::ResourcePlansInspected,
                health.resource_total_inspected,
            ),
            (
                RecoveryCounter::ResourcePlansFinalized,
                health.resource_total_finalized,
            ),
        ] {
            sink.set_counter_total(counter, value)
                .map_err(RecoveryMetricsExportError::Sink)?;
        }
        for (gauge, value) in [
            (
                RecoveryGauge::ConsecutiveFailedCycles,
                u64::try_from(health.consecutive_failed_cycles).unwrap_or(u64::MAX),
            ),
            (
                RecoveryGauge::RetryDelayMilliseconds,
                health.retry_delay.map_or(0, duration_ms),
            ),
            (RecoveryGauge::DurableRetrying, health.durable_retrying),
            (RecoveryGauge::DurableEscalated, health.durable_escalated),
            (
                RecoveryGauge::DurableUnacknowledgedEscalated,
                health.durable_unacknowledged_escalated,
            ),
            (RecoveryGauge::DurableResolved, health.durable_resolved),
            (
                RecoveryGauge::ArtifactDomainFaulted,
                u64::from(health.artifact_domain_faulted),
            ),
            (
                RecoveryGauge::SemanticConsecutiveFailedCycles,
                u64::try_from(health.semantic_consecutive_failed_cycles).unwrap_or(u64::MAX),
            ),
            (
                RecoveryGauge::SemanticDurableRetrying,
                health.semantic_durable_retrying,
            ),
            (
                RecoveryGauge::SemanticDurableEscalated,
                health.semantic_durable_escalated,
            ),
            (
                RecoveryGauge::SemanticDurableUnacknowledgedEscalated,
                health.semantic_durable_unacknowledged_escalated,
            ),
            (
                RecoveryGauge::SemanticDurableResolved,
                health.semantic_durable_resolved,
            ),
            (
                RecoveryGauge::SemanticDomainFaulted,
                u64::from(health.semantic_domain_faulted),
            ),
            (
                RecoveryGauge::ResourceConsecutiveFailedCycles,
                u64::try_from(health.resource_consecutive_failed_cycles).unwrap_or(u64::MAX),
            ),
            (
                RecoveryGauge::ResourceDurableRetrying,
                health.resource_durable_retrying,
            ),
            (
                RecoveryGauge::ResourceDurableEscalated,
                health.resource_durable_escalated,
            ),
            (
                RecoveryGauge::ResourceDurableUnacknowledgedEscalated,
                health.resource_durable_unacknowledged_escalated,
            ),
            (
                RecoveryGauge::ResourceDurableResolved,
                health.resource_durable_resolved,
            ),
            (
                RecoveryGauge::ResourceDomainFaulted,
                u64::from(health.resource_domain_faulted),
            ),
        ] {
            sink.set_gauge(gauge, value)
                .map_err(RecoveryMetricsExportError::Sink)?;
        }
        Ok(())
    }

    /// Routes one operation-level arm to the pluggable executor seam. The
    /// authorization, caller/issuer binding, and idempotency checks have
    /// already run on the shared submit path; this half only owns the state
    /// transition and its receipt id.
    fn execute_operation_control(
        &self,
        arm: OperationArm,
        command: &sabi::v1::ControlCommand,
        caller: &nlos_schema::sabi::v1::CallerIdentity,
        context: &SabiRequestContext,
        now_wall_ms: i64,
    ) -> Result<ReceiptId, SystemControlError> {
        let Some(executor) = self.operation_executor else {
            return Err(SystemControlError::OperationControlExecutionUnwired);
        };
        let request = OperationControlRequest {
            target_id: fixed16(&command.target_id)?,
            expected_generation_or_revision: command.expected_generation_or_revision,
            issuer_principal_id: fixed16(&caller.principal_id)?,
            idempotency_key: fixed16(&context.idempotency_key)?,
            requested_at_ms: now_wall_ms,
        };
        let execution = match arm {
            OperationArm::Pause => executor.pause_operation(request),
            OperationArm::Resume => executor.resume_operation(request),
            OperationArm::Cancel => executor.cancel_operation(request),
            OperationArm::Kill => executor.kill_operation(request),
            OperationArm::Throttle { throttle_percent } => {
                executor.throttle_operation(request, throttle_percent)
            }
            OperationArm::Reclaim => executor.reclaim_operation(request),
        };
        execution.map_err(SystemControlError::OperationExecution)
    }

    /// Routes one application-lifecycle arm to the pluggable executor seam
    /// (W35-P11). The authorization, caller/issuer binding, and
    /// idempotency checks have already run on the shared submit path; this
    /// half only owns the authority transition and its receipt id.
    fn execute_application_control(
        &self,
        arm: ApplicationArm,
        command: &sabi::v1::ControlCommand,
        caller: &nlos_schema::sabi::v1::CallerIdentity,
        context: &SabiRequestContext,
        now_wall_ms: i64,
    ) -> Result<ReceiptId, SystemControlError> {
        let Some(executor) = self.application_executor else {
            return Err(SystemControlError::ApplicationControlExecutionUnwired);
        };
        let request = ApplicationControlRequest {
            package_id: fixed16(&command.target_id)?,
            expected_generation_or_revision: command.expected_generation_or_revision,
            issuer_principal_id: fixed16(&caller.principal_id)?,
            idempotency_key: fixed16(&context.idempotency_key)?,
            requested_at_ms: now_wall_ms,
        };
        let execution = match arm {
            ApplicationArm::Disable => executor.disable_application(request),
            ApplicationArm::Uninstall => executor.uninstall_application(request),
        };
        execution.map_err(SystemControlError::ApplicationExecution)
    }

    fn handle_get(
        &self,
        request: &Envelope,
        now_monotonic_ns: u64,
    ) -> Result<Envelope, SystemControlError> {
        let context =
            validate_sabi_request_context(request, MethodSemantics::QUERY, now_monotonic_ns)?;
        let payload = decode_get_system_control_request(&request.payload)?;
        self.authorizer
            .authorize_get(context, &payload)
            .map_err(SystemControlError::AuthorizationDenied)?;
        if payload.view == i32::from(SystemControlView::SemanticCommitRecovery) {
            return self.handle_get_semantic(request, context, payload.alert_limit);
        }
        if payload.view == i32::from(SystemControlView::ResourceCommitRecovery) {
            return self.handle_get_resource(request, context, payload.alert_limit);
        }
        if payload.view == i32::from(SystemControlView::TaskGroup) {
            return self.handle_get_task_group(request, context, &payload);
        }
        if payload.view == i32::from(SystemControlView::TaskNode) {
            return self.handle_get_task_node(request, context, &payload);
        }
        if payload.view == i32::from(SystemControlView::ExecutionFiber) {
            return self.handle_get_execution_fiber(request, context, &payload);
        }
        if payload.view == i32::from(SystemControlView::Topic) {
            return self.handle_get_topic(request, context, &payload);
        }
        if payload.view == i32::from(SystemControlView::Operation) {
            return self.handle_get_operation(request, context, &payload);
        }
        let requested = usize::try_from(payload.alert_limit).unwrap_or(usize::MAX);
        let alerts = self
            .tasks
            .list_artifact_recovery_alerts(requested.saturating_add(1))?;
        let alerts_truncated = alerts.len() > requested;
        let alerts = alerts
            .into_iter()
            .take(requested)
            .map(|alert| {
                let recovery = alert.recovery;
                Ok(ArtifactRecoveryAlertStatus {
                    plan_id: recovery.plan_id.as_bytes().to_vec(),
                    total_failures: recovery.total_failures,
                    last_failure_authority: recovery_failure_authority(recovery.last_source).into(),
                    first_failed_at_ms: recovery.first_failed_at_ms,
                    last_failed_at_ms: recovery.last_failed_at_ms,
                    escalated_at_ms: recovery
                        .escalated_at_ms
                        .ok_or(SystemControlError::InvalidRecoveryAlert)?,
                    acknowledgement_receipt: alert.acknowledgement.map(|receipt| {
                        ReceiptReference {
                            receipt_id: receipt.receipt_id.into_bytes().to_vec(),
                        }
                    }),
                })
            })
            .collect::<Result<Vec<_>, SystemControlError>>()?;
        let health = self.authoritative_health()?;
        let snapshot = ArtifactRecoveryOperationsSnapshot {
            schema: Some(system_control_schema_identity()),
            metrics: Some(metrics(health)),
            alerts,
            alerts_truncated,
        };
        Ok(response_envelope(
            request,
            context.correlation_id.clone(),
            encode_artifact_recovery_operations_snapshot(&snapshot)?,
            Vec::new(),
        ))
    }

    /// Semantic-domain `get`: the W26 ledger API pins a zero-argument alert
    /// listing (no query limit), so the bounded snapshot truncates the full
    /// escalated list at the requested `alert_limit` here.
    fn handle_get_semantic(
        &self,
        request: &Envelope,
        context: &SabiRequestContext,
        alert_limit: u32,
    ) -> Result<Envelope, SystemControlError> {
        let requested = usize::try_from(alert_limit).unwrap_or(usize::MAX);
        let escalated = self.tasks.list_semantic_recovery_alerts()?;
        let alerts_truncated = escalated.len() > requested;
        let alerts = escalated
            .into_iter()
            .take(requested)
            .map(|alert| {
                let recovery = alert.recovery;
                Ok(SemanticRecoveryAlertStatus {
                    plan_id: recovery.plan_id.as_bytes().to_vec(),
                    total_failures: recovery.total_failures,
                    last_failure_authority: semantic_failure_authority(recovery.last_source).into(),
                    first_failed_at_ms: recovery.first_failed_at_ms,
                    last_failed_at_ms: recovery.last_failed_at_ms,
                    escalated_at_ms: recovery
                        .escalated_at_ms
                        .ok_or(SystemControlError::InvalidRecoveryAlert)?,
                    acknowledgement_receipt: alert.acknowledgement.map(|receipt| {
                        ReceiptReference {
                            receipt_id: receipt.receipt_id.into_bytes().to_vec(),
                        }
                    }),
                })
            })
            .collect::<Result<Vec<_>, SystemControlError>>()?;
        let health = self.authoritative_semantic_health()?;
        let snapshot = SemanticRecoveryOperationsSnapshot {
            schema: Some(system_control_schema_identity()),
            metrics: Some(semantic_metrics(&health)),
            alerts,
            alerts_truncated,
        };
        Ok(response_envelope(
            request,
            context.correlation_id.clone(),
            encode_semantic_recovery_operations_snapshot(&snapshot)?,
            Vec::new(),
        ))
    }

    /// Resource-domain `get` (W28-C-3b, ADR-0017 G8): mirrors the semantic
    /// view — the v43 ledger API pins the same zero-argument alert listing,
    /// so the bounded snapshot truncates at the requested `alert_limit`.
    fn handle_get_resource(
        &self,
        request: &Envelope,
        context: &SabiRequestContext,
        alert_limit: u32,
    ) -> Result<Envelope, SystemControlError> {
        let requested = usize::try_from(alert_limit).unwrap_or(usize::MAX);
        let escalated = self.tasks.list_resource_recovery_alerts()?;
        let alerts_truncated = escalated.len() > requested;
        let alerts = escalated
            .into_iter()
            .take(requested)
            .map(|alert| {
                let recovery = alert.recovery;
                Ok(ResourceRecoveryAlertStatus {
                    plan_id: recovery.plan_id.as_bytes().to_vec(),
                    total_failures: recovery.total_failures,
                    last_failure_authority: resource_failure_authority(recovery.last_source).into(),
                    first_failed_at_ms: recovery.first_failed_at_ms,
                    last_failed_at_ms: recovery.last_failed_at_ms,
                    escalated_at_ms: recovery
                        .escalated_at_ms
                        .ok_or(SystemControlError::InvalidRecoveryAlert)?,
                    acknowledgement_receipt: alert.acknowledgement.map(|receipt| {
                        ReceiptReference {
                            receipt_id: receipt.receipt_id.into_bytes().to_vec(),
                        }
                    }),
                })
            })
            .collect::<Result<Vec<_>, SystemControlError>>()?;
        let health = self.authoritative_resource_health()?;
        let snapshot = ResourceRecoveryOperationsSnapshot {
            schema: Some(system_control_schema_identity()),
            metrics: Some(resource_metrics(&health)),
            alerts,
            alerts_truncated,
        };
        Ok(response_envelope(
            request,
            context.correlation_id.clone(),
            encode_resource_recovery_operations_snapshot(&snapshot)?,
            Vec::new(),
        ))
    }

    /// `TaskGroup` view (W32-G, B5-3): reads the durable group and its
    /// bounded member list straight from the `TaskAuthority` the handler
    /// already owns; `alert_limit` bounds the member projection.
    fn handle_get_task_group(
        &self,
        request: &Envelope,
        context: &SabiRequestContext,
        payload: &sabi::v1::GetSystemControlRequest,
    ) -> Result<Envelope, SystemControlError> {
        let group_id = TaskGroupId::from_bytes(fixed16(&payload.target_id)?);
        let record = self.tasks.inspect_group(group_id)?;
        let requested = usize::try_from(payload.alert_limit).unwrap_or(usize::MAX);
        let members = self.tasks.list_group_members(group_id)?;
        let members_truncated = members.len() > requested;
        let members = members
            .into_iter()
            .take(requested)
            .map(|member| sabi::v1::TaskGroupMemberStatus {
                member_type: task_group_member_type(member.member_type).into(),
                member_id: member.member_id.to_vec(),
                membership_state: task_group_membership_state(member.membership_state).into(),
                membership_generation: member.membership_generation,
                admission_receipt: Some(ReceiptReference {
                    receipt_id: member.admission_receipt_id.into_bytes().to_vec(),
                }),
                removal_receipt: member
                    .removal_receipt_id
                    .map(|receipt_id| ReceiptReference {
                        receipt_id: receipt_id.into_bytes().to_vec(),
                    }),
            })
            .collect();
        let snapshot = sabi::v1::TaskGroupOperationsSnapshot {
            schema: Some(system_control_schema_identity()),
            group: Some(sabi::v1::TaskGroupStatus {
                group_id: record.group_id.into_bytes().to_vec(),
                task_id: record.task_id.into_bytes().to_vec(),
                parent_group_id: record
                    .parent_group_id
                    .map_or_else(Vec::new, |parent| parent.into_bytes().to_vec()),
                state: task_group_state(record.state).into(),
                membership_generation: record.membership_generation,
                state_seq: record.state_seq,
                depth: record.depth,
                cancel_epoch: record.cancel_epoch,
                created_at_ms: record.created_at_ms,
                updated_at_ms: record.updated_at_ms,
            }),
            members,
            members_truncated,
        };
        Ok(response_envelope(
            request,
            context.correlation_id.clone(),
            nlos_schema::encode_task_group_operations_snapshot(&snapshot)?,
            Vec::new(),
        ))
    }

    /// `TaskNode` view (W32-G, B5-3): bounded plan-authority read through the
    /// pluggable [`TaskNodeInspectSource`] seam.
    fn handle_get_task_node(
        &self,
        request: &Envelope,
        context: &SabiRequestContext,
        payload: &sabi::v1::GetSystemControlRequest,
    ) -> Result<Envelope, SystemControlError> {
        let Some(source) = self.task_node_source else {
            return Err(SystemControlError::LayerInspectionUnwired);
        };
        let inspection = source
            .inspect_task_node(fixed16(&payload.plan_id)?, fixed16(&payload.target_id)?)
            .map_err(SystemControlError::LayerInspection)?;
        let snapshot = sabi::v1::TaskNodeOperationsSnapshot {
            schema: Some(system_control_schema_identity()),
            node: Some(sabi::v1::TaskNodeStatus {
                plan_id: inspection.plan_id.to_vec(),
                node_id: inspection.node_id.to_vec(),
                kind: inspection.kind.into(),
                state: inspection.state.into(),
                declared_revision: inspection.declared_revision,
                node_digest: inspection.node_digest,
                transition_count: inspection.transition_count,
                residency_tier: inspection.residency_tier.into(),
                residency_transition_count: inspection.residency_transition_count,
                first_declared_at_ms: inspection.first_declared_at_ms,
                updated_at_ms: inspection.updated_at_ms,
            }),
        };
        Ok(response_envelope(
            request,
            context.correlation_id.clone(),
            nlos_schema::encode_task_node_operations_snapshot(&snapshot)?,
            Vec::new(),
        ))
    }

    /// `ExecutionFiber` view (W32-G, B5-3): runtime snapshot read through the
    /// pluggable [`ExecutionFiberInspectSource`] seam.
    fn handle_get_execution_fiber(
        &self,
        request: &Envelope,
        context: &SabiRequestContext,
        payload: &sabi::v1::GetSystemControlRequest,
    ) -> Result<Envelope, SystemControlError> {
        let Some(source) = self.fiber_source else {
            return Err(SystemControlError::LayerInspectionUnwired);
        };
        let inspection = source
            .inspect_execution_fiber(fixed16(&payload.target_id)?, payload.target_generation)
            .map_err(SystemControlError::LayerInspection)?;
        let snapshot = sabi::v1::ExecutionFiberOperationsSnapshot {
            schema: Some(system_control_schema_identity()),
            fiber: Some(sabi::v1::ExecutionFiberStatus {
                fiber_id: inspection.fiber_id.to_vec(),
                generation: inspection.generation,
                state: inspection.state.into(),
                lifecycle_phase: inspection.lifecycle_phase.into(),
                active_cpu_ms: inspection.active_cpu_ms,
                elapsed_wall_ms: inspection.elapsed_wall_ms,
                scheduler_wait_ms: inspection.scheduler_wait_ms,
                external_wait_ms: inspection.external_wait_ms,
                backpressure_wait_ms: inspection.backpressure_wait_ms,
                suspended_ms: inspection.suspended_ms,
            }),
        };
        Ok(response_envelope(
            request,
            context.correlation_id.clone(),
            nlos_schema::encode_execution_fiber_operations_snapshot(&snapshot)?,
            Vec::new(),
        ))
    }

    /// Topic view (W32-G, B5-3): durable topic row read through the
    /// pluggable [`TopicInspectSource`] seam.
    fn handle_get_topic(
        &self,
        request: &Envelope,
        context: &SabiRequestContext,
        payload: &sabi::v1::GetSystemControlRequest,
    ) -> Result<Envelope, SystemControlError> {
        let Some(source) = self.topic_source else {
            return Err(SystemControlError::LayerInspectionUnwired);
        };
        let inspection = source
            .inspect_topic(fixed16(&payload.target_id)?)
            .map_err(SystemControlError::LayerInspection)?;
        let snapshot = sabi::v1::TopicOperationsSnapshot {
            schema: Some(system_control_schema_identity()),
            topic: Some(sabi::v1::TopicStatus {
                topic_id: inspection.topic_id.to_vec(),
                channel_id: inspection.channel_id.to_vec(),
                channel_generation: inspection.channel_generation,
                name: inspection.name,
                active_subscriptions: inspection.active_subscriptions,
                policy_digest: inspection.policy_digest,
                created_at_ms: inspection.created_at_ms,
            }),
        };
        Ok(response_envelope(
            request,
            context.correlation_id.clone(),
            nlos_schema::encode_topic_operations_snapshot(&snapshot)?,
            Vec::new(),
        ))
    }

    /// Operation view (W32-G, B5-3): durable state-machine row read through
    /// the pluggable [`OperationInspectSource`] seam.
    fn handle_get_operation(
        &self,
        request: &Envelope,
        context: &SabiRequestContext,
        payload: &sabi::v1::GetSystemControlRequest,
    ) -> Result<Envelope, SystemControlError> {
        let Some(source) = self.operation_source else {
            return Err(SystemControlError::LayerInspectionUnwired);
        };
        let inspection = source
            .inspect_operation(fixed16(&payload.target_id)?, payload.target_generation)
            .map_err(SystemControlError::LayerInspection)?;
        let snapshot = sabi::v1::DurableOperationSnapshot {
            schema: Some(system_control_schema_identity()),
            operation: Some(sabi::v1::DurableOperationStatus {
                operation_id: inspection.operation_id.to_vec(),
                generation: inspection.generation,
                state: inspection.state.into(),
                cancel_epoch: inspection.cancel_epoch,
                owner_fiber_id: inspection.owner_fiber_id.to_vec(),
                owner_fiber_generation: inspection.owner_fiber_generation,
                outcome_receipt: inspection
                    .outcome_receipt_id
                    .map(|receipt_id| ReceiptReference { receipt_id }),
            }),
        };
        Ok(response_envelope(
            request,
            context.correlation_id.clone(),
            nlos_schema::encode_durable_operation_snapshot(&snapshot)?,
            Vec::new(),
        ))
    }

    fn authoritative_health(&self) -> Result<RecoveryWorkerHealth, TaskStoreError> {
        let durable = self.tasks.summarize_artifact_recovery()?;
        let mut health = self.health.recovery_health();
        health.durable_retrying = durable.retrying;
        health.durable_escalated = durable.escalated;
        health.durable_unacknowledged_escalated = durable.unacknowledged_escalated;
        health.durable_resolved = durable.resolved;
        Ok(health)
    }

    fn authoritative_semantic_health(&self) -> Result<RecoveryWorkerHealth, TaskStoreError> {
        let durable = self.tasks.summarize_semantic_recovery()?;
        let mut health = self.health.recovery_health();
        health.semantic_durable_retrying = durable.retrying;
        health.semantic_durable_escalated = durable.escalated;
        health.semantic_durable_unacknowledged_escalated = durable.unacknowledged_escalated;
        health.semantic_durable_resolved = durable.resolved;
        Ok(health)
    }

    fn authoritative_resource_health(&self) -> Result<RecoveryWorkerHealth, TaskStoreError> {
        let durable = self.tasks.summarize_resource_recovery()?;
        let mut health = self.health.recovery_health();
        health.resource_durable_retrying = durable.retrying;
        health.resource_durable_escalated = durable.escalated;
        health.resource_durable_unacknowledged_escalated = durable.unacknowledged_escalated;
        health.resource_durable_resolved = durable.resolved;
        Ok(health)
    }

    #[allow(clippy::too_many_lines)] // The thirteen submit arms stay flat in one auditable dispatch.
    fn handle_submit(
        &self,
        request: &Envelope,
        now_monotonic_ns: u64,
        now_wall_ms: i64,
    ) -> Result<Envelope, SystemControlError> {
        let context =
            validate_sabi_request_context(request, MethodSemantics::MUTATION, now_monotonic_ns)?;
        let payload = decode_submit_control_command_request(&request.payload)?;
        let command = payload
            .command
            .as_ref()
            .ok_or(CompatibilityError::MissingSystemControlCommand)?;
        let caller = context
            .caller
            .as_ref()
            .ok_or(CommonSemanticsError::MissingCallerIdentity)?;
        if command.issuer_principal_id != caller.principal_id {
            return Err(SystemControlError::CallerIssuerMismatch);
        }
        if command.control_command_id != context.idempotency_key {
            return Err(SystemControlError::CommandIdempotencyMismatch);
        }
        self.authorizer
            .authorize_submit(context, command)
            .map_err(SystemControlError::AuthorizationDenied)?;
        let receipt_id = match command.command {
            Some(sabi::v1::control_command::Command::AcknowledgeArtifactRecoveryAlert(_)) => {
                self.tasks
                    .acknowledge_artifact_recovery_alert(ArtifactRecoveryAlertAcknowledgeRequest {
                        plan_id: ArtifactCommitPlanId::from_bytes(fixed16(&command.target_id)?),
                        expected_total_failures: command.expected_generation_or_revision,
                        principal_id: PrincipalId::from_bytes(fixed16(&caller.principal_id)?),
                        idempotency_key: IdempotencyKey::from_bytes(fixed16(
                            &context.idempotency_key,
                        )?),
                        acknowledged_at_ms: now_wall_ms,
                    })?
                    .receipt()
                    .receipt_id
            }
            Some(sabi::v1::control_command::Command::AcknowledgeSemanticRecoveryAlert(_)) => {
                self.tasks
                    .acknowledge_semantic_recovery_alert(SemanticRecoveryAlertAcknowledgeRequest {
                        plan_id: SemanticCommitPlanId::from_bytes(fixed16(&command.target_id)?),
                        expected_total_failures: command.expected_generation_or_revision,
                        principal_id: PrincipalId::from_bytes(fixed16(&caller.principal_id)?),
                        idempotency_key: IdempotencyKey::from_bytes(fixed16(
                            &context.idempotency_key,
                        )?),
                        acknowledged_at_ms: now_wall_ms,
                    })?
                    .receipt()
                    .receipt_id
            }
            Some(sabi::v1::control_command::Command::ResumeSemanticRecovery(_)) => {
                let resumed =
                    self.tasks
                        .resume_semantic_recovery(SemanticRecoveryResumeRequest {
                            plan_id: SemanticCommitPlanId::from_bytes(fixed16(&command.target_id)?),
                            expected_total_failures: command.expected_generation_or_revision,
                            resumed_at_ms: now_wall_ms,
                        })?;
                semantic_recovery_resume_reference(resumed.plan_id, resumed.total_failures)
            }
            Some(sabi::v1::control_command::Command::AcknowledgeResourceRecoveryAlert(_)) => {
                self.tasks
                    .acknowledge_resource_recovery_alert(ResourceRecoveryAlertAcknowledgeRequest {
                        plan_id: ResourceCommitPlanId::from_bytes(fixed16(&command.target_id)?),
                        expected_total_failures: command.expected_generation_or_revision,
                        principal_id: PrincipalId::from_bytes(fixed16(&caller.principal_id)?),
                        idempotency_key: IdempotencyKey::from_bytes(fixed16(
                            &context.idempotency_key,
                        )?),
                        acknowledged_at_ms: now_wall_ms,
                    })?
                    .receipt()
                    .receipt_id
            }
            Some(sabi::v1::control_command::Command::ResumeResourceRecovery(_)) => {
                let resumed =
                    self.tasks
                        .resume_resource_recovery(ResourceRecoveryResumeRequest {
                            plan_id: ResourceCommitPlanId::from_bytes(fixed16(&command.target_id)?),
                            expected_total_failures: command.expected_generation_or_revision,
                            resumed_at_ms: now_wall_ms,
                        })?;
                resource_recovery_resume_reference(resumed.plan_id, resumed.total_failures)
            }
            Some(sabi::v1::control_command::Command::PauseOperation(_)) => self
                .execute_operation_control(
                    OperationArm::Pause,
                    command,
                    caller,
                    context,
                    now_wall_ms,
                )?,
            Some(sabi::v1::control_command::Command::ResumeOperation(_)) => self
                .execute_operation_control(
                    OperationArm::Resume,
                    command,
                    caller,
                    context,
                    now_wall_ms,
                )?,
            Some(sabi::v1::control_command::Command::CancelOperation(_)) => self
                .execute_operation_control(
                    OperationArm::Cancel,
                    command,
                    caller,
                    context,
                    now_wall_ms,
                )?,
            Some(sabi::v1::control_command::Command::KillOperation(_)) => self
                .execute_operation_control(
                    OperationArm::Kill,
                    command,
                    caller,
                    context,
                    now_wall_ms,
                )?,
            Some(sabi::v1::control_command::Command::ThrottleOperation(throttle)) => self
                .execute_operation_control(
                    OperationArm::Throttle {
                        throttle_percent: throttle.throttle_percent,
                    },
                    command,
                    caller,
                    context,
                    now_wall_ms,
                )?,
            Some(sabi::v1::control_command::Command::ReclaimOperation(_)) => self
                .execute_operation_control(
                    OperationArm::Reclaim,
                    command,
                    caller,
                    context,
                    now_wall_ms,
                )?,
            Some(sabi::v1::control_command::Command::DisableApplication(_)) => self
                .execute_application_control(
                    ApplicationArm::Disable,
                    command,
                    caller,
                    context,
                    now_wall_ms,
                )?,
            Some(sabi::v1::control_command::Command::UninstallApplication(_)) => self
                .execute_application_control(
                    ApplicationArm::Uninstall,
                    command,
                    caller,
                    context,
                    now_wall_ms,
                )?,
            // The shared decoder already rejects a payload without a known
            // command arm, so an un-routed command cannot reach this point.
            None => return Err(CompatibilityError::MissingSystemControlCommand.into()),
        };
        let receipt = ReceiptReference {
            receipt_id: receipt_id.into_bytes().to_vec(),
        };
        let result = sabi::v1::ControlCommandResult {
            schema: Some(system_control_schema_identity()),
            control_command_id: command.control_command_id.clone(),
            state: ControlCommandLifecycleState::Completed.into(),
            receipt: Some(receipt.clone()),
        };
        Ok(response_envelope(
            request,
            context.correlation_id.clone(),
            encode_control_command_result(&result)?,
            vec![receipt],
        ))
    }
}

fn metrics(health: RecoveryWorkerHealth) -> ArtifactRecoveryMetrics {
    ArtifactRecoveryMetrics {
        worker_state: worker_state(health.state).into(),
        completed_cycles: health.completed_cycles,
        total_inspected: health.total_inspected,
        total_finalized: health.total_finalized,
        consecutive_failed_cycles: u64::try_from(health.consecutive_failed_cycles)
            .unwrap_or(u64::MAX),
        retry_delay_ms: health.retry_delay.map(duration_ms),
        durable_retrying: health.durable_retrying,
        durable_escalated: health.durable_escalated,
        durable_unacknowledged_escalated: health.durable_unacknowledged_escalated,
        durable_resolved: health.durable_resolved,
        domain_faulted: health.artifact_domain_faulted,
        last_failures: health
            .last_failures
            .into_iter()
            .take(MAX_SYSTEM_CONTROL_FAILURES)
            .map(|failure| RecoveryFailureSummary {
                plan_id: failure
                    .plan_id
                    .map_or_else(Vec::new, |plan_id| plan_id.as_bytes().to_vec()),
                authority: worker_failure_authority(failure.authority).into(),
            })
            .collect(),
    }
}

fn semantic_metrics(health: &RecoveryWorkerHealth) -> SemanticRecoveryMetrics {
    SemanticRecoveryMetrics {
        total_inspected: health.semantic_total_inspected,
        total_finalized: health.semantic_total_finalized,
        consecutive_failed_cycles: u64::try_from(health.semantic_consecutive_failed_cycles)
            .unwrap_or(u64::MAX),
        durable_retrying: health.semantic_durable_retrying,
        durable_escalated: health.semantic_durable_escalated,
        durable_unacknowledged_escalated: health.semantic_durable_unacknowledged_escalated,
        durable_resolved: health.semantic_durable_resolved,
        domain_faulted: health.semantic_domain_faulted,
    }
}

fn resource_metrics(health: &RecoveryWorkerHealth) -> ResourceRecoveryMetrics {
    ResourceRecoveryMetrics {
        total_inspected: health.resource_total_inspected,
        total_finalized: health.resource_total_finalized,
        consecutive_failed_cycles: u64::try_from(health.resource_consecutive_failed_cycles)
            .unwrap_or(u64::MAX),
        durable_retrying: health.resource_durable_retrying,
        durable_escalated: health.resource_durable_escalated,
        durable_unacknowledged_escalated: health.resource_durable_unacknowledged_escalated,
        durable_resolved: health.resource_durable_resolved,
        domain_faulted: health.resource_domain_faulted,
    }
}

/// Deterministic 16-byte reference naming one resource recovery resume
/// outcome (`Escalated`→`Retrying` at one `total_failures` revision),
/// mirroring `nlos_task::semantic_recovery_resume_reference` verbatim under
/// a resource-domain label. `nlos-task` (read-only in the W28-C-3b lane)
/// exports no public resource twin yet, so this crate owns the formula; it
/// is stable across idempotent replays of the same resume command and
/// domain-separated from the alert acknowledgement derivation
/// (`llmos/task-resource-recovery-alert-ack/v1`, owned by nlos-task).
#[must_use]
pub fn resource_recovery_resume_reference(
    plan_id: ResourceCommitPlanId,
    total_failures: u64,
) -> ReceiptId {
    executor_receipt::derive_executor_receipt_id(
        b"llmos/task-resource-recovery-resume/v1\0",
        &[plan_id.as_bytes(), &total_failures.to_be_bytes()],
    )
}

const fn worker_state(state: RecoveryWorkerState) -> sabi::v1::RecoveryWorkerLifecycleState {
    match state {
        RecoveryWorkerState::Starting => sabi::v1::RecoveryWorkerLifecycleState::Starting,
        RecoveryWorkerState::Running => sabi::v1::RecoveryWorkerLifecycleState::Running,
        RecoveryWorkerState::BackingOff => sabi::v1::RecoveryWorkerLifecycleState::BackingOff,
        RecoveryWorkerState::Faulted => sabi::v1::RecoveryWorkerLifecycleState::Faulted,
        RecoveryWorkerState::Stopped => sabi::v1::RecoveryWorkerLifecycleState::Stopped,
    }
}

const fn task_group_state(state: GroupState) -> sabi::v1::TaskGroupLifecycleState {
    use nlos_task::GroupState as Source;
    match state {
        Source::Open => sabi::v1::TaskGroupLifecycleState::Open,
        Source::Sealed => sabi::v1::TaskGroupLifecycleState::Sealed,
        Source::CancelRequested => sabi::v1::TaskGroupLifecycleState::CancelRequested,
        Source::Cancelling => sabi::v1::TaskGroupLifecycleState::Cancelling,
        Source::Quiescing => sabi::v1::TaskGroupLifecycleState::Quiescing,
        Source::Completed => sabi::v1::TaskGroupLifecycleState::Completed,
        Source::Failed => sabi::v1::TaskGroupLifecycleState::Failed,
        Source::Partial => sabi::v1::TaskGroupLifecycleState::Partial,
        Source::Cancelled => sabi::v1::TaskGroupLifecycleState::Cancelled,
        Source::Uncertain => sabi::v1::TaskGroupLifecycleState::Uncertain,
        Source::Recovering => sabi::v1::TaskGroupLifecycleState::Recovering,
        Source::Quarantined => sabi::v1::TaskGroupLifecycleState::Quarantined,
        Source::EffectUnknown => sabi::v1::TaskGroupLifecycleState::EffectUnknown,
    }
}

const fn task_group_member_type(member_type: GroupMemberType) -> sabi::v1::TaskGroupMemberType {
    match member_type {
        GroupMemberType::ChildGroup => sabi::v1::TaskGroupMemberType::ChildGroup,
        GroupMemberType::TaskAttempt => sabi::v1::TaskGroupMemberType::TaskAttempt,
        GroupMemberType::AgentInstance => sabi::v1::TaskGroupMemberType::AgentInstance,
    }
}

const fn task_group_membership_state(
    membership: MembershipState,
) -> sabi::v1::TaskGroupMembershipState {
    match membership {
        MembershipState::Active => sabi::v1::TaskGroupMembershipState::Active,
        MembershipState::Removed => sabi::v1::TaskGroupMembershipState::Removed,
    }
}

const fn worker_failure_authority(
    authority: WorkerFailureAuthority,
) -> sabi::v1::RecoveryFailureAuthority {
    match authority {
        WorkerFailureAuthority::Task => sabi::v1::RecoveryFailureAuthority::Task,
        WorkerFailureAuthority::Artifact => sabi::v1::RecoveryFailureAuthority::Artifact,
        WorkerFailureAuthority::Coordinator => sabi::v1::RecoveryFailureAuthority::Coordinator,
        WorkerFailureAuthority::Worker => sabi::v1::RecoveryFailureAuthority::Worker,
    }
}

const fn recovery_failure_authority(
    authority: ArtifactRecoveryFailureSource,
) -> sabi::v1::RecoveryFailureAuthority {
    match authority {
        ArtifactRecoveryFailureSource::TaskAuthority => sabi::v1::RecoveryFailureAuthority::Task,
        ArtifactRecoveryFailureSource::ArtifactAuthority => {
            sabi::v1::RecoveryFailureAuthority::Artifact
        }
        ArtifactRecoveryFailureSource::Coordinator => {
            sabi::v1::RecoveryFailureAuthority::Coordinator
        }
    }
}

const fn semantic_failure_authority(
    authority: SemanticRecoveryFailureSource,
) -> sabi::v1::RecoveryFailureAuthority {
    match authority {
        SemanticRecoveryFailureSource::TaskAuthority => sabi::v1::RecoveryFailureAuthority::Task,
        SemanticRecoveryFailureSource::SemanticAuthority => {
            sabi::v1::RecoveryFailureAuthority::Semantic
        }
        SemanticRecoveryFailureSource::Coordinator => {
            sabi::v1::RecoveryFailureAuthority::Coordinator
        }
    }
}

const fn resource_failure_authority(
    authority: ResourceRecoveryFailureSource,
) -> sabi::v1::RecoveryFailureAuthority {
    match authority {
        ResourceRecoveryFailureSource::TaskAuthority => sabi::v1::RecoveryFailureAuthority::Task,
        ResourceRecoveryFailureSource::ResourceAuthority => {
            sabi::v1::RecoveryFailureAuthority::Resource
        }
        ResourceRecoveryFailureSource::Coordinator => {
            sabi::v1::RecoveryFailureAuthority::Coordinator
        }
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

fn fixed16(bytes: &[u8]) -> Result<[u8; 16], SystemControlError> {
    bytes
        .try_into()
        .map_err(|_| SystemControlError::InvalidRecoveryAlert)
}

/// Builds a typed failure envelope for one rejected request.
///
/// The request ID and service/method are retained for transport correlation,
/// while the payload and all Operation/Receipt evidence are cleared. A
/// malformed correlation ID cannot be echoed into a response, so a valid
/// request ID is preferred and an all-zero bounded correlation is used only
/// when both request identifiers are malformed.
#[must_use]
pub fn failure_envelope(request: &Envelope, error: &SystemControlError) -> Envelope {
    let correlation_id = match request.common_context.as_ref() {
        Some(envelope::CommonContext::RequestContext(context))
            if context.correlation_id.len() == REQUEST_ID_BYTES =>
        {
            context.correlation_id.clone()
        }
        _ if request.request_id.len() == REQUEST_ID_BYTES => request.request_id.clone(),
        _ => vec![0; REQUEST_ID_BYTES],
    };
    let mut response = request.clone();
    response.payload.clear();
    response.common_context = Some(envelope::CommonContext::ResponseContext(
        SabiResponseContext {
            correlation_id,
            operation: None,
            receipts: Vec::new(),
            failure: Some(error.to_sabi_failure()),
        },
    ));
    response
}

fn response_envelope(
    request: &Envelope,
    correlation_id: Vec<u8>,
    payload: Vec<u8>,
    receipts: Vec<ReceiptReference>,
) -> Envelope {
    let mut response = request.clone();
    response.payload = payload;
    response.common_context = Some(envelope::CommonContext::ResponseContext(
        SabiResponseContext {
            correlation_id,
            operation: None,
            receipts,
            failure: None,
        },
    ));
    response
}
