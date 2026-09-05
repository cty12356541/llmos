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
//! arbitrary whitespace between tokens tolerated). Canonical forms are listed
//! first; additional EN/ZH synonym variants compile to the same
//! [`ControlCommand`] and are enumerated in the module tests.
//!
//! ```text
//! inspect health | check health | show health | inspect system health
//!   | 查看健康 | 查看系统健康 | 查看 系统 健康
//! export metrics | show metrics | get metrics
//!   | 导出指标 | 导出 指标
//! inspect task <32-hex> | check task <32-hex> | show task <32-hex>
//!   | 查看任务 <32位十六进制> | 查看 任务 <32位十六进制>
//! inspect process <32-hex> | check process <32-hex> | show process <32-hex>
//!   | 检查进程 <32位十六进制> | 查看进程 <32位十六进制> | 查看 进程 <32位十六进制>
//! acknowledge alert <32-hex> expecting <n>
//!   | ack alert <32-hex> expecting <n> | confirm alert <32-hex> expecting <n>
//!   | 确认告警 <32位十六进制> 期望 <n> | 确认 告警 <32位十六进制> 期望 <n>
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
\"inspect task <32-hex>\" | \"inspect process <32-hex>\" | \
\"acknowledge alert <32-hex> expecting <count>\" | \"查看健康\" | \
\"导出指标\" | \"查看任务 <32位十六进制>\" | \"检查进程 <32位十六进制>\" | \
\"确认告警 <32位十六进制> 期望 <次数>\"";

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
    if let Some(result) = try_parse_inspect_health(&tokens) {
        return result;
    }
    if let Some(result) = try_parse_export_metrics(&tokens) {
        return result;
    }
    if let Some(result) = try_parse_inspect_task(&tokens) {
        return result;
    }
    if let Some(result) = try_parse_inspect_process(&tokens) {
        return result;
    }
    if let Some(result) = try_parse_acknowledgement(&tokens) {
        return result;
    }
    Err(ControlError::InvalidCommand(GRAMMAR_HELP))
}

fn is_read_verb(token: &str) -> bool {
    token.eq_ignore_ascii_case("inspect")
        || token.eq_ignore_ascii_case("check")
        || token.eq_ignore_ascii_case("show")
}

fn is_metrics_verb(token: &str) -> bool {
    token.eq_ignore_ascii_case("export")
        || token.eq_ignore_ascii_case("show")
        || token.eq_ignore_ascii_case("get")
}

fn is_ack_verb(token: &str) -> bool {
    token.eq_ignore_ascii_case("acknowledge")
        || token.eq_ignore_ascii_case("ack")
        || token.eq_ignore_ascii_case("confirm")
}

fn try_parse_inspect_health(tokens: &[&str]) -> Option<Result<ControlCommand, ControlError>> {
    match tokens {
        [head, second] if is_read_verb(head) && second.eq_ignore_ascii_case("health") => {
            Some(Ok(ControlCommand::InspectHealth))
        }
        [head, second, third]
            if head.eq_ignore_ascii_case("inspect")
                && second.eq_ignore_ascii_case("system")
                && third.eq_ignore_ascii_case("health") =>
        {
            Some(Ok(ControlCommand::InspectHealth))
        }
        ["查看健康" | "查看系统健康"] | ["查看", "系统", "健康"] => {
            Some(Ok(ControlCommand::InspectHealth))
        }
        [head, second, ..]
            if (is_read_verb(head) && second.eq_ignore_ascii_case("task"))
                || (*head == "查看" && *second == "任务") =>
        {
            None
        }
        [head, second, ..]
            if (is_read_verb(head) && second.eq_ignore_ascii_case("process"))
                || (*head == "检查" && *second == "进程")
                || (*head == "查看" && *second == "进程") =>
        {
            None
        }
        [head, second, ..] if is_read_verb(head) && second.eq_ignore_ascii_case("metrics") => None,
        [head, ..] if is_read_verb(head) || *head == "查看" || *head == "检查" => {
            Some(Err(ControlError::InvalidCommand(
                "\"inspect\" expects \"health\", \"task <32-hex>\", or \"process <32-hex>\"",
            )))
        }
        _ => None,
    }
}

fn try_parse_export_metrics(tokens: &[&str]) -> Option<Result<ControlCommand, ControlError>> {
    match tokens {
        [head, second] if is_metrics_verb(head) && second.eq_ignore_ascii_case("metrics") => {
            Some(Ok(ControlCommand::ExportMetrics))
        }
        ["导出指标"] | ["导出", "指标"] => Some(Ok(ControlCommand::ExportMetrics)),
        [head, second, ..]
            if (head.eq_ignore_ascii_case("show") || head.eq_ignore_ascii_case("get"))
                && (second.eq_ignore_ascii_case("task")
                    || second.eq_ignore_ascii_case("process")) =>
        {
            None
        }
        [head, ..] if head.eq_ignore_ascii_case("export") => Some(Err(
            ControlError::InvalidCommand("\"export\" expects \"metrics\""),
        )),
        [head, ..] if head.eq_ignore_ascii_case("get") || head.eq_ignore_ascii_case("show") => {
            Some(Err(ControlError::InvalidCommand(
                "\"export\" expects \"metrics\"",
            )))
        }
        ["导出", ..] => Some(Err(ControlError::InvalidCommand(
            "\"export\" expects \"metrics\"",
        ))),
        _ => None,
    }
}

fn try_parse_inspect_task(tokens: &[&str]) -> Option<Result<ControlCommand, ControlError>> {
    match tokens {
        [head, second, plan] if is_read_verb(head) && second.eq_ignore_ascii_case("task") => {
            Some(parse_hex_id(plan).map(|plan_id| ControlCommand::InspectTask { plan_id }))
        }
        ["查看任务", plan] | ["查看", "任务", plan] => {
            Some(parse_hex_id(plan).map(|plan_id| ControlCommand::InspectTask { plan_id }))
        }
        _ => None,
    }
}

fn try_parse_inspect_process(tokens: &[&str]) -> Option<Result<ControlCommand, ControlError>> {
    match tokens {
        [head, second, process_id]
            if is_read_verb(head) && second.eq_ignore_ascii_case("process") =>
        {
            Some(
                parse_hex_id(process_id)
                    .map(|process_id| ControlCommand::InspectProcess { process_id }),
            )
        }
        ["检查进程" | "查看进程", process_id] | ["查看", "进程", process_id] => Some(
            parse_hex_id(process_id)
                .map(|process_id| ControlCommand::InspectProcess { process_id }),
        ),
        [head, second, ..]
            if (is_read_verb(head) && second.eq_ignore_ascii_case("process"))
                || *second == "进程" =>
        {
            Some(Err(ControlError::InvalidCommand(
                "\"inspect process\" expects \"<32-hex>\"",
            )))
        }
        _ => None,
    }
}

fn try_parse_acknowledgement(tokens: &[&str]) -> Option<Result<ControlCommand, ControlError>> {
    match tokens {
        [head, second, plan, third, count]
            if is_ack_verb(head)
                && second.eq_ignore_ascii_case("alert")
                && third.eq_ignore_ascii_case("expecting") =>
        {
            Some(parse_count(count).and_then(|n| acknowledge(plan, n)))
        }
        ["确认告警", plan, "期望", count] | ["确认", "告警", plan, "期望", count] => {
            Some(parse_count(count).and_then(|n| acknowledge(plan, n)))
        }
        [head, ..] if is_ack_verb(head) => Some(Err(ControlError::InvalidCommand(
            "\"acknowledge alert\" expects \"<32-hex> expecting <count>\"",
        ))),
        _ => None,
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
        for sentence in [
            "export metrics",
            "EXPORT METRICS",
            "Export\tMetrics",
            "  export   metrics  \n",
            "show metrics",
            "get metrics",
        ] {
            assert_eq!(
                parse_nl_command(sentence).unwrap(),
                ControlCommand::ExportMetrics,
                "sentence: {sentence:?}"
            );
        }
    }

    #[test]
    fn chinese_export_metrics_form_parses() {
        for sentence in ["导出指标", "  导出指标  ", "导出 指标"] {
            assert_eq!(
                parse_nl_command(sentence).unwrap(),
                ControlCommand::ExportMetrics,
                "sentence: {sentence:?}"
            );
        }
    }

    #[test]
    fn english_health_forms_parse() {
        for sentence in [
            "inspect health",
            "INSPECT HEALTH",
            "Inspect\tHealth",
            "  inspect   health  \n",
            "check health",
            "show health",
            "inspect system health",
            "Inspect System Health",
        ] {
            assert_eq!(
                parse_nl_command(sentence).unwrap(),
                ControlCommand::InspectHealth,
                "sentence: {sentence:?}"
            );
        }
    }

    #[test]
    fn chinese_health_form_parses() {
        for sentence in ["查看健康", "  查看健康  ", "查看系统健康", "查看 系统 健康"]
        {
            assert_eq!(
                parse_nl_command(sentence).unwrap(),
                ControlCommand::InspectHealth,
                "sentence: {sentence:?}"
            );
        }
    }

    #[test]
    fn english_task_forms_parse() {
        for sentence in [
            "inspect task a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "INSPECT TASK A1B2C3D4E5F60718293A4B5C6D7E8F90",
            "inspect\t task \t A1B2C3D4E5F60718293A4B5C6D7E8F90",
            "check task a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "show task A1B2C3D4E5F60718293A4B5C6D7E8F90",
        ] {
            assert_eq!(
                parse_nl_command(sentence).unwrap(),
                ControlCommand::InspectTask { plan_id: plan_id() },
                "sentence: {sentence:?}"
            );
        }
    }

    #[test]
    fn chinese_task_form_parses() {
        for sentence in [
            "查看任务 a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "  查看任务  A1B2C3D4E5F60718293A4B5C6D7E8F90 ",
            "查看 任务 a1b2c3d4e5f60718293a4b5c6d7e8f90",
        ] {
            assert_eq!(
                parse_nl_command(sentence).unwrap(),
                ControlCommand::InspectTask { plan_id: plan_id() },
                "sentence: {sentence:?}"
            );
        }
    }

    #[test]
    fn english_process_forms_parse() {
        for sentence in [
            "inspect process a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "INSPECT PROCESS A1B2C3D4E5F60718293A4B5C6D7E8F90",
            "check process a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "show process A1B2C3D4E5F60718293A4B5C6D7E8F90",
        ] {
            assert_eq!(
                parse_nl_command(sentence).unwrap(),
                ControlCommand::InspectProcess {
                    process_id: plan_id()
                },
                "sentence: {sentence:?}"
            );
        }
    }

    #[test]
    fn chinese_process_form_parses() {
        for sentence in [
            "检查进程 a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "  检查进程  A1B2C3D4E5F60718293A4B5C6D7E8F90 ",
            "查看进程 a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "查看 进程 a1b2c3d4e5f60718293a4b5c6d7e8f90",
        ] {
            assert_eq!(
                parse_nl_command(sentence).unwrap(),
                ControlCommand::InspectProcess {
                    process_id: plan_id()
                },
                "sentence: {sentence:?}"
            );
        }
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
        for sentence in [
            "  Acknowledge\tAlert  A1B2C3D4E5F60718293A4B5C6D7E8F90 \t EXPECTING  3 ",
            "ack alert a1b2c3d4e5f60718293a4b5c6d7e8f90 expecting 3",
            "confirm alert a1b2c3d4e5f60718293a4b5c6d7e8f90 expecting 3",
        ] {
            assert_eq!(
                parse_nl_command(sentence).unwrap(),
                command,
                "sentence: {sentence:?}"
            );
        }
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
        for sentence in [
            "  确认告警  A1B2C3D4E5F60718293A4B5C6D7E8F90   期望  7 ",
            "确认 告警 a1b2c3d4e5f60718293a4b5c6d7e8f90 期望 7",
        ] {
            assert_eq!(
                parse_nl_command(sentence).unwrap(),
                command,
                "sentence: {sentence:?}"
            );
        }
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
            "show health now",
            "check healthy",
            "inspect system health now",
            "查看 健康",
            "查看 系统",
            "show health now",
            "显示健康",
            "export",
            "export metric",
            "export metrics now",
            "show metric",
            "get metric",
            "导出",
            "导出指标了",
            "inspect",
            "inspect task",
            "inspect health now",
            "inspect tasks a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "inspect task 1234",
            "inspect task zz313233343536373839303132333435",
            "inspect process",
            "inspect processes a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "inspect process 1234",
            "检查进程",
            "查看 进程",
            "acknowledge alert",
            format!("acknowledge alert {PLAN_HEX_LOWER}").as_str(),
            format!("acknowledge alert {PLAN_HEX_LOWER} expecting").as_str(),
            format!("acknowledge alert {PLAN_HEX_LOWER} expecting -1").as_str(),
            format!("acknowledge alert {PLAN_HEX_LOWER} expecting +1").as_str(),
            format!("acknowledge alert {PLAN_HEX_LOWER} expecting 1.5").as_str(),
            format!("acknowledge alert {PLAN_HEX_LOWER} expecting {long_count}").as_str(),
            format!("acknowledge alert {PLAN_HEX_LOWER} expecting 1 extra").as_str(),
            format!("ack alert {PLAN_HEX_LOWER}").as_str(),
            format!("confirm alert {PLAN_HEX_LOWER} expecting").as_str(),
            "查看健康了",
            "查看任务",
            "查看 任务",
            format!("确认告警 {PLAN_HEX_UPPER} 期望").as_str(),
            format!("确认告警 {PLAN_HEX_UPPER} 期望 一").as_str(),
            format!("确认 告警 {PLAN_HEX_UPPER} 期望").as_str(),
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
