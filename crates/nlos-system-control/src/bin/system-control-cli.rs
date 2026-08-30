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
//! system-control-cli <SOCKET> inspect-task <PLAN_ID_HEX_32>
//! system-control-cli <SOCKET> ack-recovery-alert <COMMAND_ID_HEX_32> <PLAN_ID_HEX_32> <EXPECTED_FAILURES> <REASON>
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

use nlos_system_control::control::{
    ControlCommand, ControlError, ControlOutcome, ControlReceipt, parse_hex_id, receipt_to_hex,
};

const USAGE: &str = "usage: system-control-cli <SOCKET> inspect-health \
| inspect-task <PLAN_ID_HEX_32> \
| ack-recovery-alert <COMMAND_ID_HEX_32> <PLAN_ID_HEX_32> <EXPECTED_FAILURES> <REASON>";

fn parse_u64(value: &str) -> Result<u64, ControlError> {
    value
        .parse::<u64>()
        .map_err(|_| ControlError::InvalidCommand("expected failure count must be a u64"))
}

fn parsed_command(arguments: &[String]) -> Result<ControlCommand, ControlError> {
    let Some(operation) = arguments.first().map(String::as_str) else {
        return Err(ControlError::InvalidCommand("missing control operation"));
    };
    match operation {
        "inspect-health" if arguments.len() == 1 => Ok(ControlCommand::InspectHealth),
        "inspect-task" if arguments.len() == 2 => Ok(ControlCommand::InspectTask {
            plan_id: parse_hex_id(&arguments[1])?,
        }),
        "ack-recovery-alert" if arguments.len() == 5 => {
            Ok(ControlCommand::AcknowledgeRecoveryAlert {
                control_command_id: parse_hex_id(&arguments[1])?,
                plan_id: parse_hex_id(&arguments[2])?,
                expected_total_failures: parse_u64(&arguments[3])?,
                reason: arguments[4].clone(),
            })
        }
        _ => Err(ControlError::InvalidCommand("unknown operation or arity")),
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

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
        Ok(ControlOutcome::Acknowledged { receipt_id }) => {
            format!("outcome=acknowledged receipt_id={}", hex(receipt_id))
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
    let receipt = dispatch_over_socket(&socket, &command).await?;
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
