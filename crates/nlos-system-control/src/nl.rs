//! Restricted-grammar natural-language control prefix (§1.3 of the
//! architecture master plan).
//!
//! Natural language enters the system here as a *compiler front-end*, never
//! as a privileged path: [`parse_nl_command`] matches a fixed bilingual
//! whitelist of imperative forms and compiles them into the same
//! [`ControlCommand`] the CLI and the structured API dispatch. There is no
//! fuzzy matching, no free-form NLU, and no second control semantics — a
//! parsed command crosses [`crate::control::build_request_envelope`], one
//! local IPC transport, and the one `SystemControl` handler exactly like
//! every other surface (§25.3 `[CTRL-PARITY-001]`, `[PHIL-CTRL-001]`,
//! `[NLOS-NL-001]`/`[NLOS-NL-002]`).
//!
//! Grammar (whitelist, strict word order; ASCII case-insensitive English;
//! arbitrary whitespace between tokens tolerated):
//!
//! ```text
//! inspect health                             | 查看健康
//! export metrics                             | 导出指标
//! inspect task <32-hex>                      | 查看任务 <32位十六进制>
//! acknowledge alert <32-hex> expecting <n>   | 确认告警 <32位十六进制> 期望 <n>
//! ```
//!
//! Derivation rules for the acknowledgement form: the `<32-hex>` argument is
//! the escalated alert's plan id; the §25.3 `control_command_id` (the
//! idempotency identity) is derived deterministically from that same plan
//! id, so one plan has exactly one acknowledgement identity and repeating
//! the sentence replays the original receipt instead of double-applying;
//! `<n>` is the explicit CAS expectation (`expected_total_failures`) — the
//! sentence must carry it because silently guessing a compare-and-swap
//! value for a state-changing command would violate `[NL-AMBIG-001]`; the
//! audit reason is the fixed [`NL_ACK_REASON`], so receipts record the
//! issuing surface without embedding the raw sentence.
//!
//! Anything outside the whitelist — unknown verbs, wrong arity, malformed
//! identifiers, non-decimal counts — fails with a typed
//! [`ControlError::InvalidCommand`] whose message names the violated bound
//! or the legal forms.

use crate::control::{ControlCommand, ControlError, parse_hex_id};

/// Fixed bounded audit reason compiled into every natural-language
/// acknowledgement. It names the issuing surface without embedding the raw
/// sentence (raw natural language MUST NOT cross the control boundary,
/// `[NLOS-NL-002]`).
pub const NL_ACK_REASON: &str =
    "acknowledged through the restricted natural-language control prefix";

/// Legal grammar, named verbatim in every rejection message.
const GRAMMAR_HELP: &str = "valid forms: \"inspect health\" | \"export metrics\" | \
\"inspect task <32-hex>\" | \"acknowledge alert <32-hex> expecting <count>\" | \"查看健康\" | \
\"导出指标\" | \"查看任务 <32位十六进制>\" | \"确认告警 <32位十六进制> 期望 <次数>\"";

/// Compiles one restricted-grammar English or Chinese imperative sentence
/// into a [`ControlCommand`] for the existing dispatch paths. Pure function:
/// no clock, no I/O, no state.
///
/// # Errors
///
/// Returns [`ControlError::InvalidCommand`] for every out-of-grammar input
/// (empty input, unknown verb phrase, wrong word order or arity, malformed
/// 32-hex identifier, non-decimal or overflowing count). The message names
/// the specific violated bound where one exists and the legal grammar
/// otherwise; no fuzzy or probabilistic interpretation is ever attempted.
pub fn parse_nl_command(input: &str) -> Result<ControlCommand, ControlError> {
    let tokens: Vec<&str> = input.split_whitespace().collect();
    match tokens.as_slice() {
        [head, second]
            if head.eq_ignore_ascii_case("inspect") && second.eq_ignore_ascii_case("health") =>
        {
            Ok(ControlCommand::InspectHealth)
        }
        [head, second]
            if head.eq_ignore_ascii_case("export") && second.eq_ignore_ascii_case("metrics") =>
        {
            Ok(ControlCommand::ExportMetrics)
        }
        [head, ..] if head.eq_ignore_ascii_case("export") => Err(ControlError::InvalidCommand(
            "\"export\" expects \"metrics\"",
        )),
        [head, second, plan]
            if head.eq_ignore_ascii_case("inspect") && second.eq_ignore_ascii_case("task") =>
        {
            Ok(ControlCommand::InspectTask {
                plan_id: parse_hex_id(plan)?,
            })
        }
        [head, ..] if head.eq_ignore_ascii_case("inspect") => Err(ControlError::InvalidCommand(
            "\"inspect\" expects \"health\" or \"task <32-hex>\"",
        )),
        [head, second, plan, third, count]
            if head.eq_ignore_ascii_case("acknowledge")
                && second.eq_ignore_ascii_case("alert")
                && third.eq_ignore_ascii_case("expecting") =>
        {
            acknowledge(plan, parse_count(count)?)
        }
        [head, ..] if head.eq_ignore_ascii_case("acknowledge") => {
            Err(ControlError::InvalidCommand(
                "\"acknowledge alert\" expects \"<32-hex> expecting <count>\"",
            ))
        }
        ["查看健康"] => Ok(ControlCommand::InspectHealth),
        ["导出指标"] => Ok(ControlCommand::ExportMetrics),
        ["查看任务", plan] => Ok(ControlCommand::InspectTask {
            plan_id: parse_hex_id(plan)?,
        }),
        ["确认告警", plan, "期望", count] => acknowledge(plan, parse_count(count)?),
        _ => Err(ControlError::InvalidCommand(GRAMMAR_HELP)),
    }
}

/// Compiles the acknowledgement form with the deterministic derivation
/// rules documented at the module level.
fn acknowledge(
    plan_hex: &str,
    expected_total_failures: u64,
) -> Result<ControlCommand, ControlError> {
    let plan_id = parse_hex_id(plan_hex)?;
    Ok(ControlCommand::AcknowledgeRecoveryAlert {
        control_command_id: plan_id,
        plan_id,
        expected_total_failures,
        reason: NL_ACK_REASON.to_owned(),
    })
}

/// Parses the plain decimal CAS expectation. Digits only: no sign, no
/// separator, no overflow past the 64-bit bound.
fn parse_count(token: &str) -> Result<u64, ControlError> {
    if !token.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ControlError::InvalidCommand(
            "acknowledgement CAS expectation must be a plain decimal count (digits only)",
        ));
    }
    token.parse::<u64>().map_err(|_| {
        ControlError::InvalidCommand("acknowledgement CAS expectation exceeds the 64-bit bound")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAN_HEX_LOWER: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f90";
    const PLAN_HEX_UPPER: &str = "A1B2C3D4E5F60718293A4B5C6D7E8F90";

    fn plan_id() -> [u8; 16] {
        [
            0xa1, 0xb2, 0xc3, 0xd4, 0xe5, 0xf6, 0x07, 0x18, 0x29, 0x3a, 0x4b, 0x5c, 0x6d, 0x7e,
            0x8f, 0x90,
        ]
    }

    #[test]
    fn english_export_metrics_forms_parse() {
        assert_eq!(
            parse_nl_command("export metrics").unwrap(),
            ControlCommand::ExportMetrics
        );
        assert_eq!(
            parse_nl_command("EXPORT METRICS").unwrap(),
            ControlCommand::ExportMetrics
        );
        assert_eq!(
            parse_nl_command("Export\tMetrics").unwrap(),
            ControlCommand::ExportMetrics
        );
        assert_eq!(
            parse_nl_command("  export   metrics  \n").unwrap(),
            ControlCommand::ExportMetrics
        );
    }

    #[test]
    fn chinese_export_metrics_form_parses() {
        assert_eq!(
            parse_nl_command("导出指标").unwrap(),
            ControlCommand::ExportMetrics
        );
        assert_eq!(
            parse_nl_command("  导出指标  ").unwrap(),
            ControlCommand::ExportMetrics
        );
    }

    #[test]
    fn english_health_forms_parse() {
        assert_eq!(
            parse_nl_command("inspect health").unwrap(),
            ControlCommand::InspectHealth
        );
        assert_eq!(
            parse_nl_command("INSPECT HEALTH").unwrap(),
            ControlCommand::InspectHealth
        );
        assert_eq!(
            parse_nl_command("Inspect\tHealth").unwrap(),
            ControlCommand::InspectHealth
        );
        assert_eq!(
            parse_nl_command("  inspect   health  \n").unwrap(),
            ControlCommand::InspectHealth
        );
    }

    #[test]
    fn chinese_health_form_parses() {
        assert_eq!(
            parse_nl_command("查看健康").unwrap(),
            ControlCommand::InspectHealth
        );
        assert_eq!(
            parse_nl_command("  查看健康  ").unwrap(),
            ControlCommand::InspectHealth
        );
    }

    #[test]
    fn english_task_forms_parse() {
        assert_eq!(
            parse_nl_command("inspect task a1b2c3d4e5f60718293a4b5c6d7e8f90").unwrap(),
            ControlCommand::InspectTask { plan_id: plan_id() }
        );
        assert_eq!(
            parse_nl_command("INSPECT TASK A1B2C3D4E5F60718293A4B5C6D7E8F90").unwrap(),
            ControlCommand::InspectTask { plan_id: plan_id() }
        );
        assert_eq!(
            parse_nl_command("inspect\t task \t A1B2C3D4E5F60718293A4B5C6D7E8F90").unwrap(),
            ControlCommand::InspectTask { plan_id: plan_id() }
        );
    }

    #[test]
    fn chinese_task_form_parses() {
        assert_eq!(
            parse_nl_command("查看任务 a1b2c3d4e5f60718293a4b5c6d7e8f90").unwrap(),
            ControlCommand::InspectTask { plan_id: plan_id() }
        );
        assert_eq!(
            parse_nl_command("  查看任务  A1B2C3D4E5F60718293A4B5C6D7E8F90 ").unwrap(),
            ControlCommand::InspectTask { plan_id: plan_id() }
        );
    }

    #[test]
    fn english_acknowledgement_parses_with_derived_identity() {
        let command =
            parse_nl_command("acknowledge alert a1b2c3d4e5f60718293a4b5c6d7e8f90 expecting 3")
                .unwrap();
        assert_eq!(
            command,
            ControlCommand::AcknowledgeRecoveryAlert {
                control_command_id: plan_id(),
                plan_id: plan_id(),
                expected_total_failures: 3,
                reason: NL_ACK_REASON.to_owned(),
            }
        );
        assert_eq!(
            parse_nl_command(
                "  Acknowledge\tAlert  A1B2C3D4E5F60718293A4B5C6D7E8F90 \t EXPECTING  3 "
            )
            .unwrap(),
            command
        );
    }

    #[test]
    fn chinese_acknowledgement_parses_with_derived_identity() {
        let command = parse_nl_command("确认告警 a1b2c3d4e5f60718293a4b5c6d7e8f90 期望 7").unwrap();
        assert_eq!(
            command,
            ControlCommand::AcknowledgeRecoveryAlert {
                control_command_id: plan_id(),
                plan_id: plan_id(),
                expected_total_failures: 7,
                reason: NL_ACK_REASON.to_owned(),
            }
        );
        assert_eq!(
            parse_nl_command("  确认告警  A1B2C3D4E5F60718293A4B5C6D7E8F90   期望  7 ").unwrap(),
            command
        );
    }

    #[test]
    fn count_zero_is_a_valid_literal() {
        // The parser is literal: policy about a zero CAS expectation belongs
        // to the authorizer and the TaskAuthority, not to this compiler.
        assert_eq!(
            parse_nl_command("acknowledge alert a1b2c3d4e5f60718293a4b5c6d7e8f90 expecting 0")
                .unwrap(),
            ControlCommand::AcknowledgeRecoveryAlert {
                control_command_id: plan_id(),
                plan_id: plan_id(),
                expected_total_failures: 0,
                reason: NL_ACK_REASON.to_owned(),
            }
        );
    }

    #[test]
    fn out_of_grammar_inputs_fail_typed_with_a_reason() {
        let long_count = "9".repeat(21);
        for input in [
            "",
            "   ",
            "show health",
            "显示健康",
            "export",
            "export metric",
            "export metrics now",
            "导出",
            "导出指标了",
            "inspect",
            "inspect task",
            "inspect health now",
            "inspect tasks a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "inspect task 1234",
            "inspect task zz313233343536373839303132333435",
            "acknowledge alert",
            format!("acknowledge alert {PLAN_HEX_LOWER}").as_str(),
            format!("acknowledge alert {PLAN_HEX_LOWER} expecting").as_str(),
            format!("acknowledge alert {PLAN_HEX_LOWER} expecting -1").as_str(),
            format!("acknowledge alert {PLAN_HEX_LOWER} expecting +1").as_str(),
            format!("acknowledge alert {PLAN_HEX_LOWER} expecting 1.5").as_str(),
            format!("acknowledge alert {PLAN_HEX_LOWER} expecting {long_count}").as_str(),
            format!("acknowledge alert {PLAN_HEX_LOWER} expecting 1 extra").as_str(),
            "查看健康了",
            "查看任务",
            format!("确认告警 {PLAN_HEX_UPPER} 期望").as_str(),
            format!("确认告警 {PLAN_HEX_UPPER} 期望 一").as_str(),
        ] {
            match parse_nl_command(input) {
                Err(ControlError::InvalidCommand(reason)) => {
                    assert!(
                        !reason.is_empty(),
                        "rejection for {input:?} carries no reason"
                    );
                }
                other => panic!("expected typed rejection for {input:?}, got {other:?}"),
            }
        }
    }
}
