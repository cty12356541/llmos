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
    RetryDirective, SabiErrorCode, SabiFailure, SabiRequestContext, SabiResponseContext, envelope,
};
use nlos_schema::{
    CommonSemanticsError, CompatibilityError, MAX_SYSTEM_CONTROL_FAILURES, MethodSemantics,
    REQUEST_ID_BYTES, decode_get_system_control_request, decode_submit_control_command_request,
    encode_artifact_recovery_operations_snapshot, encode_control_command_result,
    system_control_schema_identity, validate_sabi_request_context,
};
use nlos_task::{
    ArtifactCommitPlanId, ArtifactRecoveryAlertAcknowledgeRequest, ArtifactRecoveryFailureSource,
    SqliteTaskAuthority, TaskStoreError,
};
use nlos_types::{IdempotencyKey, PrincipalId};

pub const SYSTEM_CONTROL_SERVICE: &str = "system_control";
pub const GET_METHOD: &str = "get";
pub const SUBMIT_METHOD: &str = "submit";

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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RecoveryCounter {
    CompletedCycles,
    InspectedPlans,
    FinalizedPlans,
}

impl RecoveryCounter {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::CompletedCycles => "nlos_artifact_recovery_cycles_total",
            Self::InspectedPlans => "nlos_artifact_recovery_plans_inspected_total",
            Self::FinalizedPlans => "nlos_artifact_recovery_plans_finalized_total",
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
            | Self::InvalidRecoveryAlert => None,
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
        }
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

    /// Exports one authoritative metrics snapshot through a backend-neutral
    /// sink. Diagnostic strings and per-plan identities are not metrics.
    ///
    /// # Errors
    ///
    /// Returns a `TaskAuthority` read failure or the first sink error.
    pub fn export_metrics<S: RecoveryMetricsSink>(
        &self,
        sink: &mut S,
    ) -> Result<(), RecoveryMetricsExportError<S::Error>> {
        let health = self
            .authoritative_health()
            .map_err(RecoveryMetricsExportError::Task)?;
        sink.record_worker_state(health.state)
            .map_err(RecoveryMetricsExportError::Sink)?;
        for (counter, value) in [
            (RecoveryCounter::CompletedCycles, health.completed_cycles),
            (RecoveryCounter::InspectedPlans, health.total_inspected),
            (RecoveryCounter::FinalizedPlans, health.total_finalized),
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
        ] {
            sink.set_gauge(gauge, value)
                .map_err(RecoveryMetricsExportError::Sink)?;
        }
        Ok(())
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

    fn authoritative_health(&self) -> Result<RecoveryWorkerHealth, TaskStoreError> {
        let durable = self.tasks.summarize_artifact_recovery()?;
        let mut health = self.health.recovery_health();
        health.durable_retrying = durable.retrying;
        health.durable_escalated = durable.escalated;
        health.durable_unacknowledged_escalated = durable.unacknowledged_escalated;
        health.durable_resolved = durable.resolved;
        Ok(health)
    }

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
        let decision = self.tasks.acknowledge_artifact_recovery_alert(
            ArtifactRecoveryAlertAcknowledgeRequest {
                plan_id: ArtifactCommitPlanId::from_bytes(fixed16(&command.target_id)?),
                expected_total_failures: command.expected_generation_or_revision,
                principal_id: PrincipalId::from_bytes(fixed16(&caller.principal_id)?),
                idempotency_key: IdempotencyKey::from_bytes(fixed16(&context.idempotency_key)?),
                acknowledged_at_ms: now_wall_ms,
            },
        )?;
        let receipt = ReceiptReference {
            receipt_id: decision.receipt().receipt_id.into_bytes().to_vec(),
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

const fn worker_state(state: RecoveryWorkerState) -> sabi::v1::RecoveryWorkerLifecycleState {
    match state {
        RecoveryWorkerState::Starting => sabi::v1::RecoveryWorkerLifecycleState::Starting,
        RecoveryWorkerState::Running => sabi::v1::RecoveryWorkerLifecycleState::Running,
        RecoveryWorkerState::BackingOff => sabi::v1::RecoveryWorkerLifecycleState::BackingOff,
        RecoveryWorkerState::Faulted => sabi::v1::RecoveryWorkerLifecycleState::Faulted,
        RecoveryWorkerState::Stopped => sabi::v1::RecoveryWorkerLifecycleState::Stopped,
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
