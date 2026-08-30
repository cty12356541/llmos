//! Minimal typed control-plane prefix over the existing `SystemControl`
//! handler path (§24 Intent→Action→Receipt, §25.3 of the architecture master
//! plan).
//!
//! Every [`ControlCommand`] — whether dispatched in-process or through the
//! `system-control-cli` binary — is compiled to the same SABI envelope by
//! [`build_request_envelope`], crosses one local IPC transport, and is
//! answered by the same [`RecoverySystemControl::handle_for_ipc`] entry
//! point used by every other caller. There is no second control path;
//! [`ControlReceipt`] is only the typed projection of that single handler
//! response, and its failures reuse the bounded [`SabiFailure`] envelope.
//!
//! Identity posture: until the ADR-0011 authentication line lands, the CLI
//! prefix runs inside the local trust domain and presents fixed placeholder
//! identities ([`LOCAL_ISSUER_PRINCIPAL_ID`]). The injected
//! [`SystemControlAuthorizer`] on the service side remains the policy
//! boundary; presenting a handle is not authorization. The explicit opt-in
//! authenticated alternative lives in [`crate::auth`].

use std::error::Error;
use std::fmt;

use nlos_schema::sabi::v1::{
    AcknowledgeArtifactRecoveryAlertCommand, ArtifactRecoveryOperationsSnapshot, CallerIdentity,
    CapabilityHandle, ControlCommandSource, ControlScope, Envelope, GetSystemControlRequest,
    ReceiptReference, SabiErrorCode, SabiFailure, SabiRequestContext, SubmitControlCommandRequest,
    SystemControlView, control_command, envelope,
};
use nlos_schema::{
    CompatibilityError, REQUEST_ID_BYTES, SABI_ENVELOPE_SCHEMA,
    decode_artifact_recovery_operations_snapshot, decode_control_command_result,
    encode_get_system_control_request, encode_submit_control_command_request,
    system_control_schema_identity,
};

use crate::{
    GET_METHOD, RecoveryHealthSource, RecoverySystemControl, SUBMIT_METHOD, SystemControlAuthorizer,
};

/// Capability handle presented by the local control prefix. The service-side
/// authorizer still decides policy; this slot only carries the request side
/// of the local trust domain until ADR-0011 replaces it with real
/// authentication.
pub const CONTROL_CAPABILITY_SLOT: u64 = 9;
/// See [`CONTROL_CAPABILITY_SLOT`].
pub const CONTROL_CAPABILITY_GENERATION: u64 = 1;
/// Fixed local trust-domain issuer principal (placeholder, ADR-0011 pending).
pub const LOCAL_ISSUER_PRINCIPAL_ID: [u8; 16] = [0x31; 16];
/// Bounded upper bound for recovery alerts requested by one inspection.
pub const INSPECT_ALERT_LIMIT: u32 = 8;

const LOCAL_APPLICATION_ID: [u8; 16] = [0x32; 16];
const LOCAL_PROCESS_ID: [u8; 16] = [0x33; 16];
const LOCAL_REQUEST_ID: [u8; 16] = [0x35; 16];
const LOCAL_PROCESS_GENERATION: u64 = 1;
const INSPECT_HEALTH_COMMAND_ID: [u8; 16] = [0xC0; 16];
const INSPECT_CORRELATION_ID: [u8; 16] = [0x34; 16];

/// Bounded control operations for the minimal prefix (§25.3). The read
/// variants reuse the `get` snapshot; the mutation variant is the one real
/// control capability the `SystemControl` handler already owns — acknowledging
/// an escalated artifact-commit recovery alert through the `TaskAuthority` CAS.
/// No unimplemented control semantics are introduced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlCommand {
    /// Inspect aggregate recovery health (worker lifecycle plus durable
    /// retrying/escalated/resolved gauges).
    InspectHealth,
    /// Inspect one recovery plan/task by its 16-byte plan id; the receipt
    /// reports only that plan's alert.
    InspectTask { plan_id: [u8; 16] },
    /// Acknowledge one escalated recovery alert. `control_command_id` is the
    /// idempotency identity (§25.3) and is bound to the request idempotency
    /// key by the handler; `expected_total_failures` is the CAS expectation.
    AcknowledgeRecoveryAlert {
        control_command_id: [u8; 16],
        plan_id: [u8; 16],
        expected_total_failures: u64,
        reason: String,
    },
}

impl ControlCommand {
    /// §25.3 command identity: explicit for mutations, the target id for a
    /// scoped read, and a fixed constant for the aggregate read.
    #[must_use]
    pub const fn control_command_id(&self) -> [u8; 16] {
        match self {
            Self::InspectHealth => INSPECT_HEALTH_COMMAND_ID,
            Self::InspectTask { plan_id } => *plan_id,
            Self::AcknowledgeRecoveryAlert {
                control_command_id, ..
            } => *control_command_id,
        }
    }

    const fn correlation_id(&self) -> [u8; 16] {
        match self {
            Self::InspectHealth => INSPECT_CORRELATION_ID,
            Self::InspectTask { plan_id } => *plan_id,
            Self::AcknowledgeRecoveryAlert {
                control_command_id, ..
            } => *control_command_id,
        }
    }
}

/// Typed facts carried by a successful inspection.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryInspection {
    /// Recovery worker lifecycle (worker-local view, never a durable fact).
    pub worker_state: RecoveryWorkerLifecycle,
    pub completed_cycles: u64,
    pub durable_retrying: u64,
    pub durable_escalated: u64,
    pub durable_unacknowledged_escalated: u64,
    pub durable_resolved: u64,
    /// Bounded alert list exactly as the handler returned it.
    pub alerts: Vec<RecoveryAlertInsight>,
}

/// Worker lifecycle as observed by the recovery worker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecoveryWorkerLifecycle {
    Starting,
    Running,
    BackingOff,
    Faulted,
    Stopped,
}

fn decode_worker_lifecycle(encoded: i32) -> Result<RecoveryWorkerLifecycle, ControlError> {
    use nlos_schema::sabi::v1::RecoveryWorkerLifecycleState;

    let Ok(state) = RecoveryWorkerLifecycleState::try_from(encoded) else {
        return Err(ControlError::UnexpectedResponse(
            "unknown recovery worker lifecycle state",
        ));
    };
    Ok(match state {
        RecoveryWorkerLifecycleState::Starting => RecoveryWorkerLifecycle::Starting,
        RecoveryWorkerLifecycleState::Running => RecoveryWorkerLifecycle::Running,
        RecoveryWorkerLifecycleState::BackingOff => RecoveryWorkerLifecycle::BackingOff,
        RecoveryWorkerLifecycleState::Faulted => RecoveryWorkerLifecycle::Faulted,
        RecoveryWorkerLifecycleState::Stopped => RecoveryWorkerLifecycle::Stopped,
        RecoveryWorkerLifecycleState::Unspecified => {
            return Err(ControlError::UnexpectedResponse(
                "unspecified recovery worker lifecycle state",
            ));
        }
    })
}

fn encode_worker_lifecycle(lifecycle: RecoveryWorkerLifecycle) -> i32 {
    use nlos_schema::sabi::v1::RecoveryWorkerLifecycleState;

    let state = match lifecycle {
        RecoveryWorkerLifecycle::Starting => RecoveryWorkerLifecycleState::Starting,
        RecoveryWorkerLifecycle::Running => RecoveryWorkerLifecycleState::Running,
        RecoveryWorkerLifecycle::BackingOff => RecoveryWorkerLifecycleState::BackingOff,
        RecoveryWorkerLifecycle::Faulted => RecoveryWorkerLifecycleState::Faulted,
        RecoveryWorkerLifecycle::Stopped => RecoveryWorkerLifecycleState::Stopped,
    };
    i32::from(state)
}

/// One recovery alert as reported by the authoritative snapshot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryAlertInsight {
    pub plan_id: Vec<u8>,
    pub total_failures: u64,
    pub acknowledged_receipt_id: Option<Vec<u8>>,
}

/// Terminal outcome of one dispatched command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlOutcome {
    /// Read-only inspection completed.
    Inspected(RecoveryInspection),
    /// Mutation accepted by the `TaskAuthority`; authoritative receipt id.
    Acknowledged { receipt_id: Vec<u8> },
}

/// Typed receipt for one dispatched [`ControlCommand`] (§24.3 posture in
/// miniature): it records only facts observable inside the controlled
/// boundary — the echoed command identity, transport correlation, and either
/// bounded success facts or the sanitized [`SabiFailure`] produced by the
/// handler. A receipt never upgrades a rejection into success.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlReceipt {
    /// Echoed §25.3 command identity.
    pub control_command_id: [u8; 16],
    /// Correlation retained from the handler response context.
    pub correlation_id: Vec<u8>,
    /// Bounded success facts or the sanitized typed failure.
    pub outcome: Result<ControlOutcome, SabiFailure>,
}

/// Errors raised on the caller side of the control prefix. Handler
/// rejections are not errors here — they surface as
/// [`ControlReceipt::outcome`] failures — so this type only covers contract
/// and transport defects.
#[derive(Debug)]
pub enum ControlError {
    /// Payload encoding/decoding violated the frozen schema contract.
    Schema(CompatibilityError),
    /// Command arguments violate the bounded command contract.
    InvalidCommand(&'static str),
    /// Handler response lacked the expected typed shape.
    UnexpectedResponse(&'static str),
    /// Local IPC transport failure (requires the `cli` feature).
    #[cfg(feature = "cli")]
    Ipc(nlos_ipc::IpcError),
    /// ADR-0011 handshake refusal on the authenticated dispatch path
    /// (Unix + `cli` only, via [`crate::auth`]).
    #[cfg(all(unix, feature = "cli"))]
    Handshake(nlos_ipc::handshake::HandshakeError),
}

impl fmt::Display for ControlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Schema(error) => write!(formatter, "control payload contract: {error}"),
            Self::InvalidCommand(reason) => write!(formatter, "invalid control command: {reason}"),
            Self::UnexpectedResponse(reason) => {
                write!(formatter, "unexpected control response: {reason}")
            }
            #[cfg(feature = "cli")]
            Self::Ipc(error) => write!(formatter, "control transport: {error}"),
            #[cfg(all(unix, feature = "cli"))]
            Self::Handshake(error) => write!(formatter, "control handshake refused: {error}"),
        }
    }
}

impl Error for ControlError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Schema(error) => Some(error),
            Self::InvalidCommand(_) | Self::UnexpectedResponse(_) => None,
            #[cfg(feature = "cli")]
            Self::Ipc(error) => Some(error),
            #[cfg(all(unix, feature = "cli"))]
            Self::Handshake(error) => Some(error),
        }
    }
}

impl From<CompatibilityError> for ControlError {
    fn from(error: CompatibilityError) -> Self {
        Self::Schema(error)
    }
}

/// Compiles one [`ControlCommand`] into the same SABI request envelope the
/// structured API would send. This is the single compilation point shared by
/// the in-process dispatcher and the CLI binary (§25.3 `[CTRL-PARITY-001]`).
///
/// # Errors
///
/// Returns [`ControlError::Schema`] when payload encoding fails and
/// [`ControlError::InvalidCommand`] when the bounded reason contract is
/// violated before encoding.
pub fn build_request_envelope(command: &ControlCommand) -> Result<Envelope, ControlError> {
    let (method, payload) = match command {
        ControlCommand::InspectHealth | ControlCommand::InspectTask { .. } => (
            GET_METHOD,
            encode_get_system_control_request(&GetSystemControlRequest {
                schema: Some(system_control_schema_identity()),
                view: SystemControlView::ArtifactCommitRecovery.into(),
                alert_limit: INSPECT_ALERT_LIMIT,
            })?,
        ),
        ControlCommand::AcknowledgeRecoveryAlert {
            control_command_id,
            plan_id,
            expected_total_failures,
            reason,
        } => {
            if reason.is_empty() {
                return Err(ControlError::InvalidCommand(
                    "acknowledgement requires a non-empty bounded reason",
                ));
            }
            (
                SUBMIT_METHOD,
                encode_submit_control_command_request(&SubmitControlCommandRequest {
                    schema: Some(system_control_schema_identity()),
                    command: Some(sabi_wire_command(
                        *control_command_id,
                        *plan_id,
                        *expected_total_failures,
                        reason,
                    )),
                })?,
            )
        }
    };
    Ok(Envelope {
        schema: Some(nlos_schema::sabi::v1::SchemaIdentity {
            name: SABI_ENVELOPE_SCHEMA.to_owned(),
            major: 1,
            minor: 1,
            critical_extension_ids: Vec::new(),
            non_critical_extension_ids: Vec::new(),
        }),
        request_id: LOCAL_REQUEST_ID.to_vec(),
        service: crate::SYSTEM_CONTROL_SERVICE.to_owned(),
        method: method.to_owned(),
        common_context: Some(envelope::CommonContext::RequestContext(request_context(
            command,
        ))),
        payload,
    })
}

fn sabi_wire_command(
    control_command_id: [u8; 16],
    plan_id: [u8; 16],
    expected_total_failures: u64,
    reason: &str,
) -> nlos_schema::sabi::v1::ControlCommand {
    nlos_schema::sabi::v1::ControlCommand {
        control_command_id: control_command_id.to_vec(),
        issuer_principal_id: LOCAL_ISSUER_PRINCIPAL_ID.to_vec(),
        source: ControlCommandSource::Cli.into(),
        scope: ControlScope::Operation.into(),
        target_id: plan_id.to_vec(),
        expected_generation_or_revision: expected_total_failures,
        command: Some(control_command::Command::AcknowledgeArtifactRecoveryAlert(
            AcknowledgeArtifactRecoveryAlertCommand {},
        )),
        reason: reason.to_owned(),
    }
}

fn request_context(command: &ControlCommand) -> SabiRequestContext {
    let idempotency_key = match command {
        ControlCommand::AcknowledgeRecoveryAlert {
            control_command_id, ..
        } => control_command_id.to_vec(),
        _ => Vec::new(),
    };
    SabiRequestContext {
        caller: Some(CallerIdentity {
            principal_id: LOCAL_ISSUER_PRINCIPAL_ID.to_vec(),
            application_id: LOCAL_APPLICATION_ID.to_vec(),
            process_id: LOCAL_PROCESS_ID.to_vec(),
            process_generation: LOCAL_PROCESS_GENERATION,
        }),
        activity_context: Vec::new(),
        task_execution_binding: None,
        correlation_id: command.correlation_id().to_vec(),
        idempotency_key,
        deadline_monotonic_ns: 0,
        capability_handles: vec![CapabilityHandle {
            slot: CONTROL_CAPABILITY_SLOT,
            generation: CONTROL_CAPABILITY_GENERATION,
        }],
        reservation_handle: None,
        proposal_or_input_digest_sha256: Vec::new(),
    }
}

/// Dispatches one command in-process through the shared
/// [`RecoverySystemControl::handle_for_ipc`] handler path and projects the
/// response into a [`ControlReceipt`]. The CLI crosses the same handler via
/// [`dispatch_over_socket`], so both paths answer from one implementation.
///
/// # Errors
///
/// Returns [`ControlError`] when envelope compilation or receipt projection
/// fails; handler rejections are typed receipt failures instead.
pub fn dispatch_in_process<H, A>(
    control: &RecoverySystemControl<'_, H, A>,
    command: &ControlCommand,
    now_monotonic_ns: u64,
    now_wall_ms: i64,
) -> Result<ControlReceipt, ControlError>
where
    H: RecoveryHealthSource,
    A: SystemControlAuthorizer,
{
    let request = build_request_envelope(command)?;
    let response = control.handle_for_ipc(&request, now_monotonic_ns, now_wall_ms);
    ControlReceipt::compose(command, &response)
}

/// Dispatches one command to a real local IPC endpoint and projects the
/// response into a [`ControlReceipt`]. The service side is expected to run
/// the same [`RecoverySystemControl::handle_for_ipc`] handler; this function
/// only adds the transport hop. Unix socket endpoints only — the Windows
/// named-pipe CLI adapter is not part of this minimal prefix.
///
/// # Errors
///
/// Returns [`ControlError`] for transport, contract, or projection failures;
/// handler rejections are typed receipt failures instead.
#[cfg(all(unix, feature = "cli"))]
pub async fn dispatch_over_socket(
    socket: impl AsRef<std::path::Path>,
    command: &ControlCommand,
) -> Result<ControlReceipt, ControlError> {
    use nlos_ipc::{LocalRpcClient, TransportConfig};
    use nlos_schema::sabi::v1::ExchangeRequest;

    let request = build_request_envelope(command)?;
    let config = TransportConfig::default();
    let (stream, _) = nlos_ipc::unix::connect(socket, config)
        .await
        .map_err(ControlError::Ipc)?;
    let client = LocalRpcClient::new(stream, config);
    let response = client
        .exchange_validated(ExchangeRequest {
            envelope: Some(request),
        })
        .await
        .map_err(ControlError::Ipc)?;
    ControlReceipt::compose(command, response.envelope())
}

fn decoded_snapshot(
    response: &Envelope,
) -> Result<ArtifactRecoveryOperationsSnapshot, ControlError> {
    let snapshot = decode_artifact_recovery_operations_snapshot(&response.payload)?;
    if snapshot.alerts.len() > usize::try_from(INSPECT_ALERT_LIMIT).unwrap_or(usize::MAX) {
        return Err(ControlError::UnexpectedResponse(
            "snapshot exceeded the requested alert bound",
        ));
    }
    Ok(snapshot)
}

fn decoded_inspection(response: &Envelope) -> Result<RecoveryInspection, ControlError> {
    let snapshot = decoded_snapshot(response)?;
    let metrics = snapshot.metrics.ok_or(ControlError::Schema(
        CompatibilityError::MissingSystemControlMetrics,
    ))?;
    let worker_state = decode_worker_lifecycle(metrics.worker_state)?;
    Ok(RecoveryInspection {
        worker_state,
        completed_cycles: metrics.completed_cycles,
        durable_retrying: metrics.durable_retrying,
        durable_escalated: metrics.durable_escalated,
        durable_unacknowledged_escalated: metrics.durable_unacknowledged_escalated,
        durable_resolved: metrics.durable_resolved,
        alerts: snapshot
            .alerts
            .into_iter()
            .map(|alert| RecoveryAlertInsight {
                plan_id: alert.plan_id,
                total_failures: alert.total_failures,
                acknowledged_receipt_id: alert
                    .acknowledgement_receipt
                    .map(|receipt| receipt.receipt_id),
            })
            .collect(),
    })
}

fn not_found_failure(message: &'static str) -> SabiFailure {
    SabiFailure {
        code: SabiErrorCode::NotFound.into(),
        retry: nlos_schema::sabi::v1::RetryDirective::DoNotRetry.into(),
        safe_message: message.to_owned(),
    }
}

impl ControlReceipt {
    /// Projects one handler response envelope into the typed receipt. This
    /// is the single projection point for both dispatch paths; it never
    /// manufactures success evidence — an envelope without a usable response
    /// context or payload is a typed error, never an empty receipt.
    ///
    /// # Errors
    ///
    /// Returns [`ControlError`] when the response shape does not match the
    /// command or the frozen payload contract.
    pub fn compose(command: &ControlCommand, response: &Envelope) -> Result<Self, ControlError> {
        let Some(envelope::CommonContext::ResponseContext(context)) =
            response.common_context.as_ref()
        else {
            return Err(ControlError::UnexpectedResponse(
                "handler returned no typed response context",
            ));
        };
        let correlation_id = context.correlation_id.clone();
        let outcome = if let Some(failure) = context.failure.as_ref() {
            Err(failure.clone())
        } else {
            match command {
                ControlCommand::InspectHealth => {
                    Ok(ControlOutcome::Inspected(decoded_inspection(response)?))
                }
                ControlCommand::InspectTask { plan_id } => {
                    let mut inspected = decoded_inspection(response)?;
                    inspected
                        .alerts
                        .retain(|alert| alert.plan_id.as_slice() == plan_id.as_slice());
                    if inspected.alerts.is_empty() {
                        Err(not_found_failure(
                            "requested recovery task was not found in the operations snapshot",
                        ))
                    } else {
                        Ok(ControlOutcome::Inspected(inspected))
                    }
                }
                ControlCommand::AcknowledgeRecoveryAlert { .. } => {
                    let result = decode_control_command_result(&response.payload)?;
                    if result.control_command_id != command.control_command_id().to_vec() {
                        return Err(ControlError::UnexpectedResponse(
                            "result echoed a foreign control command id",
                        ));
                    }
                    let receipt_id = result
                        .receipt
                        .map(|ReceiptReference { receipt_id }| receipt_id)
                        .ok_or(ControlError::UnexpectedResponse(
                            "completed command carried no receipt reference",
                        ))?;
                    Ok(ControlOutcome::Acknowledged { receipt_id })
                }
            }
        };
        Ok(Self {
            control_command_id: command.control_command_id(),
            correlation_id,
            outcome,
        })
    }

    /// Deterministic bounded byte encoding used for the equivalence contract:
    /// two receipts are "the same receipt" exactly when these bytes match.
    /// Fixed-width little-endian integers, `u32` length prefixes, no schema
    /// drift surface.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&self.control_command_id);
        push_bytes(&mut bytes, &self.correlation_id);
        match self.outcome.as_ref() {
            Err(failure) => {
                bytes.push(0);
                bytes.extend_from_slice(&failure.code.to_le_bytes());
                bytes.extend_from_slice(&failure.retry.to_le_bytes());
                push_bytes(&mut bytes, failure.safe_message.as_bytes());
            }
            Ok(ControlOutcome::Inspected(inspection)) => {
                bytes.push(1);
                bytes.extend_from_slice(
                    &encode_worker_lifecycle(inspection.worker_state).to_le_bytes(),
                );
                bytes.extend_from_slice(&inspection.completed_cycles.to_le_bytes());
                bytes.extend_from_slice(&inspection.durable_retrying.to_le_bytes());
                bytes.extend_from_slice(&inspection.durable_escalated.to_le_bytes());
                bytes.extend_from_slice(&inspection.durable_unacknowledged_escalated.to_le_bytes());
                bytes.extend_from_slice(&inspection.durable_resolved.to_le_bytes());
                bytes.extend_from_slice(
                    &u32::try_from(inspection.alerts.len())
                        .unwrap_or(u32::MAX)
                        .to_le_bytes(),
                );
                for alert in &inspection.alerts {
                    push_bytes(&mut bytes, &alert.plan_id);
                    bytes.extend_from_slice(&alert.total_failures.to_le_bytes());
                    match alert.acknowledged_receipt_id.as_ref() {
                        Some(receipt_id) => {
                            bytes.push(1);
                            push_bytes(&mut bytes, receipt_id);
                        }
                        None => bytes.push(0),
                    }
                }
            }
            Ok(ControlOutcome::Acknowledged { receipt_id }) => {
                bytes.push(2);
                push_bytes(&mut bytes, receipt_id);
            }
        }
        bytes
    }
}

fn push_bytes(bytes: &mut Vec<u8>, value: &[u8]) {
    let length = u32::try_from(value.len()).unwrap_or(u32::MAX);
    bytes.extend_from_slice(&length.to_le_bytes());
    bytes.extend_from_slice(value);
}

/// Hex-encodes [`ControlReceipt::to_bytes`] for single-line CLI output.
#[must_use]
pub fn receipt_to_hex(receipt: &ControlReceipt) -> String {
    use std::fmt::Write as _;

    let bytes = receipt.to_bytes();
    let mut hex = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// Parses a fixed-length 16-byte hex identifier (`32` hex chars), fail-closed
/// on length or charset violations.
///
/// # Errors
///
/// Returns a static description of the first violated bound.
pub fn parse_hex_id(value: &str) -> Result<[u8; REQUEST_ID_BYTES], ControlError> {
    fn nibble(character: u8) -> Option<u8> {
        match character {
            b'0'..=b'9' => Some(character - b'0'),
            b'a'..=b'f' => Some(character - b'a' + 10),
            b'A'..=b'F' => Some(character - b'A' + 10),
            _ => None,
        }
    }
    let raw = value.as_bytes();
    if raw.len() != REQUEST_ID_BYTES * 2 {
        return Err(ControlError::InvalidCommand(
            "identifier must be exactly 32 hex characters",
        ));
    }
    let mut decoded = [0; REQUEST_ID_BYTES];
    for (target, pair) in decoded.iter_mut().zip(raw.as_chunks::<2>().0) {
        let high = nibble(pair[0]).ok_or(ControlError::InvalidCommand(
            "identifier must contain only hex digits",
        ))?;
        let low = nibble(pair[1]).ok_or(ControlError::InvalidCommand(
            "identifier must contain only hex digits",
        ))?;
        *target = (high << 4) | low;
    }
    Ok(decoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex_id_is_fail_closed() {
        assert!(matches!(
            parse_hex_id("31"),
            Err(ControlError::InvalidCommand(_))
        ));
        assert!(matches!(
            parse_hex_id("zz313233343536373839303132333435"),
            Err(ControlError::InvalidCommand(_))
        ));
        assert_eq!(parse_hex_id(&"31".repeat(16)).unwrap(), [0x31; 16]);
    }

    #[test]
    fn command_ids_are_deterministic_per_variant() {
        assert_eq!(
            ControlCommand::InspectHealth.control_command_id(),
            INSPECT_HEALTH_COMMAND_ID
        );
        assert_eq!(
            ControlCommand::InspectTask {
                plan_id: [0x22; 16]
            }
            .control_command_id(),
            [0x22; 16]
        );
        assert_eq!(
            ControlCommand::AcknowledgeRecoveryAlert {
                control_command_id: [0x41; 16],
                plan_id: [0x22; 16],
                expected_total_failures: 1,
                reason: "inspected recovery evidence".to_owned(),
            }
            .control_command_id(),
            [0x41; 16]
        );
    }

    #[test]
    fn acknowledgement_rejects_empty_reason_before_the_wire() {
        assert!(matches!(
            build_request_envelope(&ControlCommand::AcknowledgeRecoveryAlert {
                control_command_id: [0x41; 16],
                plan_id: [0x22; 16],
                expected_total_failures: 1,
                reason: String::new(),
            }),
            Err(ControlError::InvalidCommand(_))
        ));
    }

    #[test]
    fn inspect_envelopes_bind_the_local_trust_domain_context() {
        let envelope = build_request_envelope(&ControlCommand::InspectHealth).unwrap();
        assert_eq!(envelope.service, crate::SYSTEM_CONTROL_SERVICE);
        assert_eq!(envelope.method, GET_METHOD);
        assert_eq!(envelope.request_id.len(), REQUEST_ID_BYTES);
        let Some(envelope::CommonContext::RequestContext(context)) = envelope.common_context else {
            panic!("request context expected");
        };
        assert_eq!(
            context.caller.unwrap().principal_id,
            LOCAL_ISSUER_PRINCIPAL_ID
        );
        assert_eq!(
            context.capability_handles,
            vec![CapabilityHandle {
                slot: CONTROL_CAPABILITY_SLOT,
                generation: CONTROL_CAPABILITY_GENERATION,
            }]
        );
    }

    #[test]
    fn mutation_envelopes_bind_command_id_to_idempotency_key() {
        let command = ControlCommand::AcknowledgeRecoveryAlert {
            control_command_id: [0x41; 16],
            plan_id: [0x22; 16],
            expected_total_failures: 1,
            reason: "inspected recovery evidence".to_owned(),
        };
        let envelope = build_request_envelope(&command).unwrap();
        assert_eq!(envelope.method, SUBMIT_METHOD);
        let Some(envelope::CommonContext::RequestContext(context)) = envelope.common_context else {
            panic!("request context expected");
        };
        assert_eq!(context.idempotency_key, vec![0x41; 16]);
    }
}
