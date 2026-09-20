//! `system-control-cli` — minimal typed control CLI over a real local IPC
//! endpoint.
//!
//! The binary is a thin shell around [`nlos_system_control::control`]: it
//! parses one bounded [`ControlCommand`], dispatches it through
//! [`dispatch_over_socket`] to the `SystemControl` service's real Unix
//! socket, and prints the resulting [`ControlReceipt`]. It owns no control
//! logic of its own — the in-process path and this CLI share the same
//! command compilation, transport, handler, and receipt projection.
//!
//! # Usage
//!
//! ```text
//! system-control-cli <SOCKET> inspect-health
//! system-control-cli <SOCKET> inspect-semantic-health
//! system-control-cli <SOCKET> inspect-resource-health
//! system-control-cli <SOCKET> export-metrics
//! system-control-cli <SOCKET> export-semantic-metrics
//! system-control-cli <SOCKET> export-resource-metrics
//! system-control-cli <SOCKET> inspect-task <PLAN_ID_HEX_32>
//! system-control-cli <SOCKET> inspect-process <PROCESS_ID_HEX_32>
//! system-control-cli <SOCKET> inspect-resource <RESERVATION_ID_HEX_32>
//! system-control-cli <SOCKET> ack-recovery-alert <COMMAND_ID_HEX_32> <PLAN_ID_HEX_32> <EXPECTED_FAILURES> <REASON>
//! system-control-cli <SOCKET> ack-semantic-recovery-alert <COMMAND_ID_HEX_32> <PLAN_ID_HEX_32> <EXPECTED_FAILURES> <REASON>
//! system-control-cli <SOCKET> resume-semantic-recovery <COMMAND_ID_HEX_32> <PLAN_ID_HEX_32> <EXPECTED_FAILURES> <REASON>
//! system-control-cli <SOCKET> ack-resource-recovery-alert <COMMAND_ID_HEX_32> <PLAN_ID_HEX_32> <EXPECTED_FAILURES> <REASON>
//! system-control-cli <SOCKET> resume-resource-recovery <COMMAND_ID_HEX_32> <PLAN_ID_HEX_32> <EXPECTED_FAILURES> <REASON>
//! system-control-cli <SOCKET> pause-operation <COMMAND_ID_HEX_32> <TARGET_ID_HEX_32> <EXPECTED_REVISION> <REASON>
//! system-control-cli <SOCKET> resume-operation <COMMAND_ID_HEX_32> <TARGET_ID_HEX_32> <EXPECTED_REVISION> <REASON>
//! system-control-cli <SOCKET> cancel-operation <COMMAND_ID_HEX_32> <TARGET_ID_HEX_32> <EXPECTED_REVISION> <REASON>
//! system-control-cli <SOCKET> kill-operation <COMMAND_ID_HEX_32> <TARGET_ID_HEX_32> <EXPECTED_REVISION> <REASON>
//! system-control-cli <SOCKET> throttle-operation <COMMAND_ID_HEX_32> <TARGET_ID_HEX_32> <PERCENT_1_TO_100> <EXPECTED_REVISION> <REASON>
//! system-control-cli <SOCKET> reclaim-operation <COMMAND_ID_HEX_32> <TARGET_ID_HEX_32> <EXPECTED_REVISION> <REASON>
//! ```
//!
//! # Output and exit contract
//!
//! The first stdout line is always `RECEIPT <hex>` — the deterministic
//! [`ControlReceipt::to_bytes`] encoding — followed by one human-readable
//! summary line. Exit codes: `0` success receipt, `1` typed failure receipt
//! (the sanitized `SabiFailure`), `2` usage or transport error.
//!
//! # Authorization posture
//!
//! Local trust domain only: identities are fixed placeholders
//! (`LOCAL_ISSUER_PRINCIPAL_ID`) until ADR-0011 lands; the service-side
//! authorizer remains the policy boundary.

use std::process::ExitCode;

#[cfg(unix)]
use nlos_system_control::control::{
    ControlCommand, ControlError, ControlOutcome, ControlReceipt, parse_hex_id, receipt_to_hex,
};

#[cfg(unix)]
const USAGE: &str = "usage: system-control-cli <SOCKET> inspect-health \
| inspect-semantic-health \
| inspect-resource-health \
| export-metrics \
| export-semantic-metrics \
| export-resource-metrics \
| inspect-task <PLAN_ID_HEX_32> \
| inspect-process <PROCESS_ID_HEX_32> \
| inspect-resource <RESERVATION_ID_HEX_32> \
 | ack-recovery-alert <COMMAND_ID_HEX_32> <PLAN_ID_HEX_32> <EXPECTED_FAILURES> <REASON> \
 | ack-semantic-recovery-alert <COMMAND_ID_HEX_32> <PLAN_ID_HEX_32> <EXPECTED_FAILURES> <REASON> \
 | resume-semantic-recovery <COMMAND_ID_HEX_32> <PLAN_ID_HEX_32> <EXPECTED_FAILURES> <REASON> \
 | ack-resource-recovery-alert <COMMAND_ID_HEX_32> <PLAN_ID_HEX_32> <EXPECTED_FAILURES> <REASON> \
 | resume-resource-recovery <COMMAND_ID_HEX_32> <PLAN_ID_HEX_32> <EXPECTED_FAILURES> <REASON> \
 | pause-operation <COMMAND_ID_HEX_32> <TARGET_ID_HEX_32> <EXPECTED_REVISION> <REASON> \
 | resume-operation <COMMAND_ID_HEX_32> <TARGET_ID_HEX_32> <EXPECTED_REVISION> <REASON> \
 | cancel-operation <COMMAND_ID_HEX_32> <TARGET_ID_HEX_32> <EXPECTED_REVISION> <REASON> \
 | kill-operation <COMMAND_ID_HEX_32> <TARGET_ID_HEX_32> <EXPECTED_REVISION> <REASON> \
 | throttle-operation <COMMAND_ID_HEX_32> <TARGET_ID_HEX_32> <PERCENT_1_TO_100> <EXPECTED_REVISION> <REASON> \
 | reclaim-operation <COMMAND_ID_HEX_32> <TARGET_ID_HEX_32> <EXPECTED_REVISION> <REASON>";

#[cfg(unix)]
fn parse_u64(value: &str) -> Result<u64, ControlError> {
    value
        .parse::<u64>()
        .map_err(|_| ControlError::InvalidCommand("expected failure count must be a u64"))
}

#[cfg(unix)]
fn parse_throttle_percent(value: &str) -> Result<u64, ControlError> {
    let percent = parse_u64(value).map_err(|_| {
        ControlError::InvalidCommand("throttle percent must be a plain decimal u64")
    })?;
    if !(1..=100).contains(&percent) {
        return Err(ControlError::InvalidCommand(
            "throttle percent must be a whole percent from 1 to 100",
        ));
    }
    Ok(percent)
}

#[cfg(unix)]
#[allow(clippy::too_many_lines)] // The flat subcommand table stays in one auditable dispatch.
fn parsed_command(arguments: &[String]) -> Result<ControlCommand, ControlError> {
    let Some(operation) = arguments.first().map(String::as_str) else {
        return Err(ControlError::InvalidCommand("missing control operation"));
    };
    match operation {
        "inspect-health" if arguments.len() == 1 => Ok(ControlCommand::InspectHealth),
        "inspect-semantic-health" if arguments.len() == 1 => {
            Ok(ControlCommand::InspectSemanticHealth)
        }
        "inspect-resource-health" if arguments.len() == 1 => {
            Ok(ControlCommand::InspectResourceHealth)
        }
        "export-metrics" if arguments.len() == 1 => Ok(ControlCommand::ExportMetrics),
        "export-semantic-metrics" if arguments.len() == 1 => {
            Ok(ControlCommand::ExportSemanticMetrics)
        }
        "export-resource-metrics" if arguments.len() == 1 => {
            Ok(ControlCommand::ExportResourceMetrics)
        }
        "inspect-task" if arguments.len() == 2 => Ok(ControlCommand::InspectTask {
            plan_id: parse_hex_id(&arguments[1])?,
        }),
        "inspect-process" if arguments.len() == 2 => Ok(ControlCommand::InspectProcess {
            process_id: parse_hex_id(&arguments[1])?,
        }),
        "inspect-resource" if arguments.len() == 2 => Ok(ControlCommand::InspectResource {
            reservation_id: parse_hex_id(&arguments[1])?,
        }),
        "ack-recovery-alert" if arguments.len() == 5 => {
            Ok(ControlCommand::AcknowledgeRecoveryAlert {
                control_command_id: parse_hex_id(&arguments[1])?,
                plan_id: parse_hex_id(&arguments[2])?,
                expected_total_failures: parse_u64(&arguments[3])?,
                reason: arguments[4].clone(),
            })
        }
        "ack-semantic-recovery-alert" if arguments.len() == 5 => {
            Ok(ControlCommand::AcknowledgeSemanticRecoveryAlert {
                control_command_id: parse_hex_id(&arguments[1])?,
                plan_id: parse_hex_id(&arguments[2])?,
                expected_total_failures: parse_u64(&arguments[3])?,
                reason: arguments[4].clone(),
            })
        }
        "resume-semantic-recovery" if arguments.len() == 5 => {
            Ok(ControlCommand::ResumeSemanticRecovery {
                control_command_id: parse_hex_id(&arguments[1])?,
                plan_id: parse_hex_id(&arguments[2])?,
                expected_total_failures: parse_u64(&arguments[3])?,
                reason: arguments[4].clone(),
            })
        }
        "ack-resource-recovery-alert" if arguments.len() == 5 => {
            Ok(ControlCommand::AcknowledgeResourceRecoveryAlert {
                control_command_id: parse_hex_id(&arguments[1])?,
                plan_id: parse_hex_id(&arguments[2])?,
                expected_total_failures: parse_u64(&arguments[3])?,
                reason: arguments[4].clone(),
            })
        }
        "resume-resource-recovery" if arguments.len() == 5 => {
            Ok(ControlCommand::ResumeResourceRecovery {
                control_command_id: parse_hex_id(&arguments[1])?,
                plan_id: parse_hex_id(&arguments[2])?,
                expected_total_failures: parse_u64(&arguments[3])?,
                reason: arguments[4].clone(),
            })
        }
        "pause-operation" if arguments.len() == 5 => Ok(ControlCommand::PauseOperation {
            control_command_id: parse_hex_id(&arguments[1])?,
            target_id: parse_hex_id(&arguments[2])?,
            expected_generation_or_revision: parse_u64(&arguments[3])?,
            reason: arguments[4].clone(),
        }),
        "resume-operation" if arguments.len() == 5 => Ok(ControlCommand::ResumeOperation {
            control_command_id: parse_hex_id(&arguments[1])?,
            target_id: parse_hex_id(&arguments[2])?,
            expected_generation_or_revision: parse_u64(&arguments[3])?,
            reason: arguments[4].clone(),
        }),
        "cancel-operation" if arguments.len() == 5 => Ok(ControlCommand::CancelOperation {
            control_command_id: parse_hex_id(&arguments[1])?,
            target_id: parse_hex_id(&arguments[2])?,
            expected_generation_or_revision: parse_u64(&arguments[3])?,
            reason: arguments[4].clone(),
        }),
        "kill-operation" if arguments.len() == 5 => Ok(ControlCommand::KillOperation {
            control_command_id: parse_hex_id(&arguments[1])?,
            target_id: parse_hex_id(&arguments[2])?,
            expected_generation_or_revision: parse_u64(&arguments[3])?,
            reason: arguments[4].clone(),
        }),
        "throttle-operation" if arguments.len() == 6 => Ok(ControlCommand::ThrottleOperation {
            control_command_id: parse_hex_id(&arguments[1])?,
            target_id: parse_hex_id(&arguments[2])?,
            throttle_percent: parse_throttle_percent(&arguments[3])?,
            expected_generation_or_revision: parse_u64(&arguments[4])?,
            reason: arguments[5].clone(),
        }),
        "reclaim-operation" if arguments.len() == 5 => Ok(ControlCommand::ReclaimOperation {
            control_command_id: parse_hex_id(&arguments[1])?,
            target_id: parse_hex_id(&arguments[2])?,
            expected_generation_or_revision: parse_u64(&arguments[3])?,
            reason: arguments[4].clone(),
        }),
        _ => Err(ControlError::InvalidCommand("unknown operation or arity")),
    }
}

#[cfg(unix)]
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

#[cfg(unix)]
fn summary(receipt: &ControlReceipt) -> String {
    match receipt.outcome.as_ref() {
        Ok(ControlOutcome::Inspected(inspection)) => format!(
            "outcome=inspected worker_state={:?} completed_cycles={} \
             durable_retrying={} durable_escalated={} \
             durable_unacknowledged_escalated={} durable_resolved={} alerts={}",
            inspection.worker_state,
            inspection.completed_cycles,
            inspection.durable_retrying,
            inspection.durable_escalated,
            inspection.durable_unacknowledged_escalated,
            inspection.durable_resolved,
            inspection.alerts.len(),
        ),
        Ok(ControlOutcome::SemanticInspected(inspection)) => format!(
            "outcome=semantic_inspected total_inspected={} total_finalized={} \
             consecutive_failed_cycles={} domain_faulted={} \
             durable_retrying={} durable_escalated={} \
             durable_unacknowledged_escalated={} durable_resolved={} alerts={}",
            inspection.total_inspected,
            inspection.total_finalized,
            inspection.consecutive_failed_cycles,
            inspection.domain_faulted,
            inspection.durable_retrying,
            inspection.durable_escalated,
            inspection.durable_unacknowledged_escalated,
            inspection.durable_resolved,
            inspection.alerts.len(),
        ),
        Ok(ControlOutcome::ResourceRecoveryInspected(inspection)) => format!(
            "outcome=resource_recovery_inspected total_inspected={} total_finalized={} \
             consecutive_failed_cycles={} domain_faulted={} \
             durable_retrying={} durable_escalated={} \
             durable_unacknowledged_escalated={} durable_resolved={} alerts={}",
            inspection.total_inspected,
            inspection.total_finalized,
            inspection.consecutive_failed_cycles,
            inspection.domain_faulted,
            inspection.durable_retrying,
            inspection.durable_escalated,
            inspection.durable_unacknowledged_escalated,
            inspection.durable_resolved,
            inspection.alerts.len(),
        ),
        Ok(ControlOutcome::ProcessInspected(inspection)) => format!(
            "outcome=process_inspected process_id={} generation={} task_id={}",
            hex(&inspection.process_id),
            inspection.process_generation,
            hex(&inspection.task_id),
        ),
        Ok(ControlOutcome::ResourceInspected(inspection)) => format!(
            "outcome=resource_inspected reservation_id={} account_id={} upper_bound={} \
             usage_high_water={} consumptions={}",
            hex(&inspection.reservation_id),
            hex(&inspection.account_id),
            inspection.upper_bound,
            inspection.usage_high_water,
            inspection.consumption_count,
        ),
        Ok(ControlOutcome::MetricsExported(export)) => format!(
            "outcome=metrics_exported bytes={}",
            export.openmetrics_text.len(),
        ),
        Ok(ControlOutcome::Acknowledged { receipt_id }) => {
            format!("outcome=acknowledged receipt_id={}", hex(receipt_id))
        }
        Ok(ControlOutcome::Resumed { receipt_id }) => {
            format!("outcome=resumed receipt_id={}", hex(receipt_id))
        }
        Ok(ControlOutcome::OperationPaused { receipt_id }) => {
            format!("outcome=operation_paused receipt_id={}", hex(receipt_id))
        }
        Ok(ControlOutcome::OperationResumed { receipt_id }) => {
            format!("outcome=operation_resumed receipt_id={}", hex(receipt_id))
        }
        Ok(ControlOutcome::OperationCancelled { receipt_id }) => {
            format!("outcome=operation_cancelled receipt_id={}", hex(receipt_id))
        }
        Ok(ControlOutcome::OperationKilled { receipt_id }) => {
            format!("outcome=operation_killed receipt_id={}", hex(receipt_id))
        }
        Ok(ControlOutcome::OperationThrottled { receipt_id }) => {
            format!("outcome=operation_throttled receipt_id={}", hex(receipt_id))
        }
        Ok(ControlOutcome::OperationReclaimed { receipt_id }) => {
            format!("outcome=operation_reclaimed receipt_id={}", hex(receipt_id))
        }
        Err(failure) => format!(
            "outcome=failure code={} retry={} message={}",
            failure.code, failure.retry, failure.safe_message,
        ),
    }
}

#[cfg(unix)]
async fn run() -> Result<ExitCode, ControlError> {
    use nlos_system_control::control::dispatch_over_socket;
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let Some(socket) = arguments.first().cloned() else {
        eprintln!("{USAGE}");
        return Ok(ExitCode::from(2));
    };
    let command = parsed_command(&arguments[1..]).inspect_err(|_| eprintln!("{USAGE}"))?;
    let receipt = dispatch_over_socket(&socket, &command, None, None).await?;
    println!("RECEIPT {}", receipt_to_hex(&receipt));
    println!("{}", summary(&receipt));
    if receipt.outcome.is_ok() {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::from(1))
    }
}

#[cfg(unix)]
#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => code,
        Err(error) => {
            eprintln!("system-control-cli: {error}");
            ExitCode::from(2)
        }
    }
}

#[cfg(not(unix))]
fn main() -> ExitCode {
    eprintln!(
        "system-control-cli: the minimal prefix ships Unix socket dispatch only; \
         the Windows named-pipe CLI adapter is not part of this prefix"
    );
    ExitCode::from(2)
}
