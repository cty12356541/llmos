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
    SabiRequestContext, SabiResponseContext, envelope,
};
use nlos_schema::{
    CommonSemanticsError, CompatibilityError, MAX_SYSTEM_CONTROL_FAILURES, MethodSemantics,
    decode_get_system_control_request, decode_submit_control_command_request,
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
        let durable = self.tasks.summarize_artifact_recovery()?;
        let mut health = self.health.recovery_health();
        health.durable_retrying = durable.retrying;
        health.durable_escalated = durable.escalated;
        health.durable_unacknowledged_escalated = durable.unacknowledged_escalated;
        health.durable_resolved = durable.resolved;
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
