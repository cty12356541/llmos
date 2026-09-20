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
//! inspect health | check health | show health | status health | health check
//!   | health status | inspect system health
//!   | 查看健康 | 查看 健康 | 查看系统健康 | 查看 系统 健康 | 系统状态
//!   | 检查健康 | 检查 健康 | 健康状态 | 健康 状态
//! export metrics | show metrics | get metrics | metrics
//!   | 导出指标 | 导出 指标 | 指标
//! inspect task <32-hex> | check task <32-hex> | show task <32-hex>
//!   | get task <32-hex> | status task <32-hex> | task status <32-hex>
//!   | 查看任务 <32位十六进制> | 查看 任务 <32位十六进制>
//!   | 检查任务 <32位十六进制> | 检查 任务 <32位十六进制>
//!   | 任务状态 <32位十六进制> | 任务 状态 <32位十六进制>
//! inspect process <32-hex> | check process <32-hex> | show process <32-hex>
//!   | get process <32-hex> | status process <32-hex>
//!   | 检查进程 <32位十六进制> | 查看进程 <32位十六进制> | 查看 进程 <32位十六进制>
//! inspect resource <32-hex> | check resource <32-hex> | show resource <32-hex>
//!   | get resource <32-hex> | status resource <32-hex> | resource status <32-hex>
//!   | 查看资源 <32位十六进制> | 查看 资源 <32位十六进制>
//!   | 检查资源 <32位十六进制> | 检查 资源 <32位十六进制>
//!   | 资源状态 <32位十六进制> | 资源 状态 <32位十六进制>
//! acknowledge alert <32-hex> expecting <n>
//!   | ack alert <32-hex> expecting <n> | confirm alert <32-hex> expecting <n>
//!   | 确认告警 <32位十六进制> 期望 <n> | 确认 告警 <32位十六进制> 期望 <n>
//! pause operation <32-hex> expecting <n>
//!   | halt operation <32-hex> expecting <n> | suspend operation <32-hex> expecting <n>
//!   | 暂停操作 <32位十六进制> 期望 <n> | 暂停 操作 <32位十六进制> 期望 <n>
//! resume operation <32-hex> expecting <n>
//!   | 恢复操作 <32位十六进制> 期望 <n> | 恢复 操作 <32位十六进制> 期望 <n>
//! cancel operation <32-hex> expecting <n> | abort operation <32-hex> expecting <n>
//!   | 取消操作 <32位十六进制> 期望 <n> | 取消 操作 <32位十六进制> 期望 <n>
//! kill operation <32-hex> expecting <n> | terminate operation <32-hex> expecting <n>
//!   | 终止操作 <32位十六进制> 期望 <n> | 终止 操作 <32位十六进制> 期望 <n>
//! throttle operation <32-hex> to <n> percent expecting <n>
//!   | 限流操作 <32位十六进制> 到 <n> 百分比 期望 <n>
//!   | 限流 操作 <32位十六进制> 到 <n> 百分比 期望 <n>
//! reclaim operation <32-hex> expecting <n>
//!   | 回收操作 <32位十六进制> 期望 <n> | 回收 操作 <32位十六进制> 期望 <n>
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
//! The operation-level pause/resume/cancel forms follow the same rules
//! verbatim: the `<32-hex>` argument is the operational target id, the
//! command identity derives from that target (one target, one pause
//! identity, replay-safe), `<n>` is the explicit
//! `expected_generation_or_revision` CAS expectation, and the audit reason
//! is the fixed per-verb [`NL_PAUSE_REASON`]/[`NL_RESUME_REASON`]/
//! [`NL_CANCEL_REASON`].
//!
//! The W29-D kill/throttle/reclaim forms follow the same derivation rules:
//! kill/reclaim mirror pause verbatim; throttle adds the explicit whole
//! percent level (`to <n> percent`, `1..=100`) between the target and the
//! CAS expectation, because silently guessing a throttle level would
//! violate `[NL-AMBIG-001]` the same way a guessed CAS value would. The
//! audit reasons are the fixed per-verb [`NL_KILL_REASON`]/
//! [`NL_THROTTLE_REASON`]/[`NL_RECLAIM_REASON`].
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

/// See [`NL_ACK_REASON`]; per-verb audit reasons for the operation-level
/// forms.
pub const NL_PAUSE_REASON: &str = "paused through the restricted natural-language control prefix";
/// See [`NL_ACK_REASON`].
pub const NL_RESUME_REASON: &str = "resumed through the restricted natural-language control prefix";
/// See [`NL_ACK_REASON`].
pub const NL_CANCEL_REASON: &str =
    "cancelled through the restricted natural-language control prefix";
/// See [`NL_ACK_REASON`]; W29-D kill/throttle/reclaim forms.
pub const NL_KILL_REASON: &str = "killed through the restricted natural-language control prefix";
/// See [`NL_ACK_REASON`].
pub const NL_THROTTLE_REASON: &str =
    "throttled through the restricted natural-language control prefix";
/// See [`NL_ACK_REASON`].
pub const NL_RECLAIM_REASON: &str =
    "reclaimed through the restricted natural-language control prefix";

/// Legal grammar, named verbatim in every rejection message.
const GRAMMAR_HELP: &str = "valid forms: \"inspect health\" | \"export metrics\" | \
\"inspect task <32-hex>\" | \"inspect process <32-hex>\" | \"inspect resource <32-hex>\" | \
\"acknowledge alert <32-hex> expecting <count>\" | \
\"pause|resume|cancel|kill|reclaim operation <32-hex> expecting <count>\" | \
\"throttle operation <32-hex> to <percent> expecting <count>\" | \"查看健康\" | \
\"导出指标\" | \"查看任务 <32位十六进制>\" | \"检查进程 <32位十六进制>\" | \
\"查看资源 <32位十六进制>\" | \
\"确认告警 <32位十六进制> 期望 <次数>\" | \
\"暂停|恢复|取消|终止|回收操作 <32位十六进制> 期望 <次数>\" | \
\"限流操作 <32位十六进制> 到 <百分比> 期望 <次数>\"";

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
    if let Some(result) = try_parse_inspect_resource(&tokens) {
        return result;
    }
    if let Some(result) = try_parse_acknowledgement(&tokens) {
        return result;
    }
    if let Some(result) = try_parse_operation_control(&tokens) {
        return result;
    }
    Err(ControlError::InvalidCommand(GRAMMAR_HELP))
}

fn is_read_verb(token: &str) -> bool {
    token.eq_ignore_ascii_case("inspect")
        || token.eq_ignore_ascii_case("check")
        || token.eq_ignore_ascii_case("show")
        || token.eq_ignore_ascii_case("status")
        || token.eq_ignore_ascii_case("get")
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

fn is_pause_verb(token: &str) -> bool {
    token.eq_ignore_ascii_case("pause")
        || token.eq_ignore_ascii_case("halt")
        || token.eq_ignore_ascii_case("suspend")
}

fn is_operation_resume_verb(token: &str) -> bool {
    token.eq_ignore_ascii_case("resume")
}

fn is_cancel_verb(token: &str) -> bool {
    token.eq_ignore_ascii_case("cancel") || token.eq_ignore_ascii_case("abort")
}

fn is_kill_verb(token: &str) -> bool {
    token.eq_ignore_ascii_case("kill") || token.eq_ignore_ascii_case("terminate")
}

fn is_throttle_verb(token: &str) -> bool {
    token.eq_ignore_ascii_case("throttle")
}

fn is_reclaim_verb(token: &str) -> bool {
    token.eq_ignore_ascii_case("reclaim")
}

fn try_parse_inspect_health(tokens: &[&str]) -> Option<Result<ControlCommand, ControlError>> {
    match tokens {
        [head, second] if is_read_verb(head) && second.eq_ignore_ascii_case("health") => {
            Some(Ok(ControlCommand::InspectHealth))
        }
        [first, second]
            if first.eq_ignore_ascii_case("health")
                && (second.eq_ignore_ascii_case("check")
                    || second.eq_ignore_ascii_case("status")) =>
        {
            Some(Ok(ControlCommand::InspectHealth))
        }
        [head, second, third]
            if head.eq_ignore_ascii_case("inspect")
                && second.eq_ignore_ascii_case("system")
                && third.eq_ignore_ascii_case("health") =>
        {
            Some(Ok(ControlCommand::InspectHealth))
        }
        ["查看健康" | "查看系统健康" | "系统状态" | "检查健康" | "健康状态"]
        | ["查看" | "检查", "健康"]
        | ["健康", "状态"]
        | ["查看", "系统", "健康"] => Some(Ok(ControlCommand::InspectHealth)),
        [head, second, ..]
            if (is_read_verb(head) && second.eq_ignore_ascii_case("task"))
                || (*head == "查看" && *second == "任务")
                || (*head == "检查" && *second == "任务") =>
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
        [head, second, ..]
            if (is_read_verb(head) && second.eq_ignore_ascii_case("resource"))
                || (*head == "查看" && *second == "资源")
                || (*head == "检查" && *second == "资源") =>
        {
            None
        }
        [head, second, ..] if is_read_verb(head) && second.eq_ignore_ascii_case("metrics") => None,
        [head, ..] if is_read_verb(head) || *head == "查看" || *head == "检查" => {
            Some(Err(ControlError::InvalidCommand(
                "\"inspect\" expects \"health\", \"task <32-hex>\", \"process <32-hex>\", \
                 or \"resource <32-hex>\"",
            )))
        }
        _ => None,
    }
}

fn try_parse_export_metrics(tokens: &[&str]) -> Option<Result<ControlCommand, ControlError>> {
    match tokens {
        [head] if head.eq_ignore_ascii_case("metrics") || *head == "指标" => {
            Some(Ok(ControlCommand::ExportMetrics))
        }
        [head, second] if is_metrics_verb(head) && second.eq_ignore_ascii_case("metrics") => {
            Some(Ok(ControlCommand::ExportMetrics))
        }
        ["导出指标"] | ["导出", "指标"] => Some(Ok(ControlCommand::ExportMetrics)),
        [head, second, ..]
            if (head.eq_ignore_ascii_case("show") || head.eq_ignore_ascii_case("get"))
                && (second.eq_ignore_ascii_case("task")
                    || second.eq_ignore_ascii_case("process")
                    || second.eq_ignore_ascii_case("resource")) =>
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
        ["查看任务" | "检查任务", plan] | ["查看" | "检查", "任务", plan] => {
            Some(parse_hex_id(plan).map(|plan_id| ControlCommand::InspectTask { plan_id }))
        }
        [first, second, plan]
            if first.eq_ignore_ascii_case("task") && second.eq_ignore_ascii_case("status") =>
        {
            Some(parse_hex_id(plan).map(|plan_id| ControlCommand::InspectTask { plan_id }))
        }
        ["任务状态", plan] | ["任务", "状态", plan] => {
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

fn try_parse_inspect_resource(tokens: &[&str]) -> Option<Result<ControlCommand, ControlError>> {
    match tokens {
        [head, second, reservation_id]
            if is_read_verb(head) && second.eq_ignore_ascii_case("resource") =>
        {
            Some(
                parse_hex_id(reservation_id)
                    .map(|reservation_id| ControlCommand::InspectResource { reservation_id }),
            )
        }
        ["查看资源" | "检查资源", reservation_id] | ["查看" | "检查", "资源", reservation_id] => {
            Some(
                parse_hex_id(reservation_id)
                    .map(|reservation_id| ControlCommand::InspectResource { reservation_id }),
            )
        }
        [first, second, reservation_id]
            if first.eq_ignore_ascii_case("resource") && second.eq_ignore_ascii_case("status") =>
        {
            Some(
                parse_hex_id(reservation_id)
                    .map(|reservation_id| ControlCommand::InspectResource { reservation_id }),
            )
        }
        ["资源状态", reservation_id] | ["资源", "状态", reservation_id] => Some(
            parse_hex_id(reservation_id)
                .map(|reservation_id| ControlCommand::InspectResource { reservation_id }),
        ),
        [head, second, ..]
            if (is_read_verb(head) && second.eq_ignore_ascii_case("resource"))
                || *second == "资源" =>
        {
            Some(Err(ControlError::InvalidCommand(
                "\"inspect resource\" expects \"<32-hex>\"",
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

/// Compiles the operation-level pause form: the command identity derives
/// from the target id (replay-safe) and the CAS expectation is explicit.
fn pause_operation_command(
    target_hex: &str,
    expected_generation_or_revision: u64,
) -> Result<ControlCommand, ControlError> {
    let target_id = parse_hex_id(target_hex)?;
    Ok(ControlCommand::PauseOperation {
        control_command_id: target_id,
        target_id,
        expected_generation_or_revision,
        reason: NL_PAUSE_REASON.to_owned(),
    })
}

fn resume_operation_command(
    target_hex: &str,
    expected_generation_or_revision: u64,
) -> Result<ControlCommand, ControlError> {
    let target_id = parse_hex_id(target_hex)?;
    Ok(ControlCommand::ResumeOperation {
        control_command_id: target_id,
        target_id,
        expected_generation_or_revision,
        reason: NL_RESUME_REASON.to_owned(),
    })
}

fn cancel_operation_command(
    target_hex: &str,
    expected_generation_or_revision: u64,
) -> Result<ControlCommand, ControlError> {
    let target_id = parse_hex_id(target_hex)?;
    Ok(ControlCommand::CancelOperation {
        control_command_id: target_id,
        target_id,
        expected_generation_or_revision,
        reason: NL_CANCEL_REASON.to_owned(),
    })
}

fn kill_operation_command(
    target_hex: &str,
    expected_generation_or_revision: u64,
) -> Result<ControlCommand, ControlError> {
    let target_id = parse_hex_id(target_hex)?;
    Ok(ControlCommand::KillOperation {
        control_command_id: target_id,
        target_id,
        expected_generation_or_revision,
        reason: NL_KILL_REASON.to_owned(),
    })
}

fn throttle_operation_command(
    target_hex: &str,
    throttle_percent: u64,
    expected_generation_or_revision: u64,
) -> Result<ControlCommand, ControlError> {
    let target_id = parse_hex_id(target_hex)?;
    Ok(ControlCommand::ThrottleOperation {
        control_command_id: target_id,
        target_id,
        expected_generation_or_revision,
        throttle_percent,
        reason: NL_THROTTLE_REASON.to_owned(),
    })
}

fn reclaim_operation_command(
    target_hex: &str,
    expected_generation_or_revision: u64,
) -> Result<ControlCommand, ControlError> {
    let target_id = parse_hex_id(target_hex)?;
    Ok(ControlCommand::ReclaimOperation {
        control_command_id: target_id,
        target_id,
        expected_generation_or_revision,
        reason: NL_RECLAIM_REASON.to_owned(),
    })
}

fn try_parse_operation_control(tokens: &[&str]) -> Option<Result<ControlCommand, ControlError>> {
    match tokens {
        [head, second, target, third, count]
            if is_pause_verb(head)
                && second.eq_ignore_ascii_case("operation")
                && third.eq_ignore_ascii_case("expecting") =>
        {
            Some(parse_count(count).and_then(|n| pause_operation_command(target, n)))
        }
        [head, second, target, third, count]
            if is_operation_resume_verb(head)
                && second.eq_ignore_ascii_case("operation")
                && third.eq_ignore_ascii_case("expecting") =>
        {
            Some(parse_count(count).and_then(|n| resume_operation_command(target, n)))
        }
        [head, second, target, third, count]
            if is_cancel_verb(head)
                && second.eq_ignore_ascii_case("operation")
                && third.eq_ignore_ascii_case("expecting") =>
        {
            Some(parse_count(count).and_then(|n| cancel_operation_command(target, n)))
        }
        ["暂停操作", target, "期望", count] | ["暂停", "操作", target, "期望", count] => {
            Some(parse_count(count).and_then(|n| pause_operation_command(target, n)))
        }
        ["恢复操作", target, "期望", count] | ["恢复", "操作", target, "期望", count] => {
            Some(parse_count(count).and_then(|n| resume_operation_command(target, n)))
        }
        ["取消操作", target, "期望", count] | ["取消", "操作", target, "期望", count] => {
            Some(parse_count(count).and_then(|n| cancel_operation_command(target, n)))
        }
        [head, second, target, third, count]
            if is_kill_verb(head)
                && second.eq_ignore_ascii_case("operation")
                && third.eq_ignore_ascii_case("expecting") =>
        {
            Some(parse_count(count).and_then(|n| kill_operation_command(target, n)))
        }
        ["终止操作", target, "期望", count] | ["终止", "操作", target, "期望", count] => {
            Some(parse_count(count).and_then(|n| kill_operation_command(target, n)))
        }
        [head, second, target, third, percent, fourth, fifth, count]
            if is_throttle_verb(head)
                && second.eq_ignore_ascii_case("operation")
                && third.eq_ignore_ascii_case("to")
                && fourth.eq_ignore_ascii_case("percent")
                && fifth.eq_ignore_ascii_case("expecting") =>
        {
            Some(
                parse_percent(percent)
                    .and_then(|level| parse_count(count).map(|n| (level, n)))
                    .and_then(|(level, n)| throttle_operation_command(target, level, n)),
            )
        }
        ["限流操作", target, "到", percent, "百分比", "期望", count]
        | [
            "限流",
            "操作",
            target,
            "到",
            percent,
            "百分比",
            "期望",
            count,
        ] => Some(
            parse_percent(percent)
                .and_then(|level| parse_count(count).map(|n| (level, n)))
                .and_then(|(level, n)| throttle_operation_command(target, level, n)),
        ),
        [head, second, target, third, count]
            if is_reclaim_verb(head)
                && second.eq_ignore_ascii_case("operation")
                && third.eq_ignore_ascii_case("expecting") =>
        {
            Some(parse_count(count).and_then(|n| reclaim_operation_command(target, n)))
        }
        ["回收操作", target, "期望", count] | ["回收", "操作", target, "期望", count] => {
            Some(parse_count(count).and_then(|n| reclaim_operation_command(target, n)))
        }
        [head, ..]
            if is_pause_verb(head)
                || is_operation_resume_verb(head)
                || is_cancel_verb(head)
                || is_kill_verb(head)
                || is_throttle_verb(head)
                || is_reclaim_verb(head) =>
        {
            Some(Err(ControlError::InvalidCommand(
                "\"pause|resume|cancel|kill|reclaim operation\" expects \"<32-hex> expecting \
                 <count>\"; \"throttle operation\" expects \"<32-hex> to <percent> expecting \
                 <count>\"",
            )))
        }
        _ => None,
    }
}

/// Parses the plain decimal CAS expectation. Digits only: no sign, no
/// separator, no overflow past the 64-bit bound.
fn parse_count(token: &str) -> Result<u64, ControlError> {
    if !token.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ControlError::InvalidCommand(
            "CAS expectation must be a plain decimal count (digits only)",
        ));
    }
    token
        .parse::<u64>()
        .map_err(|_| ControlError::InvalidCommand("CAS expectation exceeds the 64-bit bound"))
}

/// Parses the throttle level: a plain decimal whole percent `1..=100`.
fn parse_percent(token: &str) -> Result<u64, ControlError> {
    if !token.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ControlError::InvalidCommand(
            "throttle percent must be a plain decimal percent (digits only)",
        ));
    }
    let percent = token
        .parse::<u64>()
        .map_err(|_| ControlError::InvalidCommand("throttle percent exceeds the 64-bit bound"))?;
    if !(1..=100).contains(&percent) {
        return Err(ControlError::InvalidCommand(
            "throttle percent must be a whole percent from 1 to 100",
        ));
    }
    Ok(percent)
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
            "metrics",
            "METRICS",
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
        for sentence in ["导出指标", "  导出指标  ", "导出 指标", "指标"] {
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
            "status health",
            "health check",
            "HEALTH CHECK",
            "health status",
            "HEALTH STATUS",
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
        for sentence in [
            "查看健康",
            "  查看健康  ",
            "查看 健康",
            "查看系统健康",
            "查看 系统 健康",
            "系统状态",
            "检查健康",
            "  检查健康  ",
            "检查 健康",
            "健康状态",
            "  健康状态  ",
            "健康 状态",
        ] {
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
            "get task a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "status task a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "task status a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "TASK STATUS A1B2C3D4E5F60718293A4B5C6D7E8F90",
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
            "检查任务 a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "检查 任务 a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "任务状态 a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "任务 状态 a1b2c3d4e5f60718293a4b5c6d7e8f90",
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
            "get process a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "status process a1b2c3d4e5f60718293a4b5c6d7e8f90",
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
    fn english_resource_forms_parse() {
        for sentence in [
            "inspect resource a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "INSPECT RESOURCE A1B2C3D4E5F60718293A4B5C6D7E8F90",
            "check resource a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "show resource A1B2C3D4E5F60718293A4B5C6D7E8F90",
            "get resource a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "status resource a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "resource status a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "RESOURCE STATUS A1B2C3D4E5F60718293A4B5C6D7E8F90",
        ] {
            assert_eq!(
                parse_nl_command(sentence).unwrap(),
                ControlCommand::InspectResource {
                    reservation_id: plan_id()
                },
                "sentence: {sentence:?}"
            );
        }
    }

    #[test]
    fn chinese_resource_form_parses() {
        for sentence in [
            "查看资源 a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "  查看资源  A1B2C3D4E5F60718293A4B5C6D7E8F90 ",
            "查看 资源 a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "检查资源 a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "检查 资源 a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "资源状态 a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "资源 状态 a1b2c3d4e5f60718293a4b5c6d7e8f90",
        ] {
            assert_eq!(
                parse_nl_command(sentence).unwrap(),
                ControlCommand::InspectResource {
                    reservation_id: plan_id()
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
    fn english_operation_control_forms_parse_with_derived_identity() {
        let pause =
            parse_nl_command("pause operation a1b2c3d4e5f60718293a4b5c6d7e8f90 expecting 4")
                .unwrap();
        assert_eq!(
            pause,
            ControlCommand::PauseOperation {
                control_command_id: plan_id(),
                target_id: plan_id(),
                expected_generation_or_revision: 4,
                reason: NL_PAUSE_REASON.to_owned(),
            }
        );
        for sentence in [
            "  HALT  Operation \t A1B2C3D4E5F60718293A4B5C6D7E8F90 \t EXPECTING  4 ",
            "suspend operation a1b2c3d4e5f60718293a4b5c6d7e8f90 expecting 4",
        ] {
            assert_eq!(
                parse_nl_command(sentence).unwrap(),
                pause,
                "sentence: {sentence:?}"
            );
        }
        let resume =
            parse_nl_command("resume operation a1b2c3d4e5f60718293a4b5c6d7e8f90 expecting 5")
                .unwrap();
        assert_eq!(
            resume,
            ControlCommand::ResumeOperation {
                control_command_id: plan_id(),
                target_id: plan_id(),
                expected_generation_or_revision: 5,
                reason: NL_RESUME_REASON.to_owned(),
            }
        );
        let cancel =
            parse_nl_command("cancel operation a1b2c3d4e5f60718293a4b5c6d7e8f90 expecting 6")
                .unwrap();
        assert_eq!(
            cancel,
            ControlCommand::CancelOperation {
                control_command_id: plan_id(),
                target_id: plan_id(),
                expected_generation_or_revision: 6,
                reason: NL_CANCEL_REASON.to_owned(),
            }
        );
        assert_eq!(
            parse_nl_command("abort operation a1b2c3d4e5f60718293a4b5c6d7e8f90 expecting 6")
                .unwrap(),
            cancel
        );
    }

    #[test]
    fn chinese_operation_control_forms_parse_with_derived_identity() {
        assert_eq!(
            parse_nl_command("暂停操作 a1b2c3d4e5f60718293a4b5c6d7e8f90 期望 4").unwrap(),
            ControlCommand::PauseOperation {
                control_command_id: plan_id(),
                target_id: plan_id(),
                expected_generation_or_revision: 4,
                reason: NL_PAUSE_REASON.to_owned(),
            }
        );
        assert_eq!(
            parse_nl_command("暂停 操作 A1B2C3D4E5F60718293A4B5C6D7E8F90 期望 4").unwrap(),
            ControlCommand::PauseOperation {
                control_command_id: plan_id(),
                target_id: plan_id(),
                expected_generation_or_revision: 4,
                reason: NL_PAUSE_REASON.to_owned(),
            }
        );
        assert_eq!(
            parse_nl_command("恢复操作 a1b2c3d4e5f60718293a4b5c6d7e8f90 期望 5").unwrap(),
            ControlCommand::ResumeOperation {
                control_command_id: plan_id(),
                target_id: plan_id(),
                expected_generation_or_revision: 5,
                reason: NL_RESUME_REASON.to_owned(),
            }
        );
        assert_eq!(
            parse_nl_command("恢复 操作 a1b2c3d4e5f60718293a4b5c6d7e8f90 期望 5").unwrap(),
            ControlCommand::ResumeOperation {
                control_command_id: plan_id(),
                target_id: plan_id(),
                expected_generation_or_revision: 5,
                reason: NL_RESUME_REASON.to_owned(),
            }
        );
        assert_eq!(
            parse_nl_command("取消操作 a1b2c3d4e5f60718293a4b5c6d7e8f90 期望 6").unwrap(),
            ControlCommand::CancelOperation {
                control_command_id: plan_id(),
                target_id: plan_id(),
                expected_generation_or_revision: 6,
                reason: NL_CANCEL_REASON.to_owned(),
            }
        );
        assert_eq!(
            parse_nl_command("取消 操作 a1b2c3d4e5f60718293a4b5c6d7e8f90 期望 6").unwrap(),
            ControlCommand::CancelOperation {
                control_command_id: plan_id(),
                target_id: plan_id(),
                expected_generation_or_revision: 6,
                reason: NL_CANCEL_REASON.to_owned(),
            }
        );
    }

    #[test]
    fn english_w29d_operation_control_forms_parse_with_derived_identity() {
        let kill = parse_nl_command("kill operation a1b2c3d4e5f60718293a4b5c6d7e8f90 expecting 7")
            .unwrap();
        assert_eq!(
            kill,
            ControlCommand::KillOperation {
                control_command_id: plan_id(),
                target_id: plan_id(),
                expected_generation_or_revision: 7,
                reason: NL_KILL_REASON.to_owned(),
            }
        );
        assert_eq!(
            parse_nl_command("terminate operation a1b2c3d4e5f60718293a4b5c6d7e8f90 expecting 7")
                .unwrap(),
            kill
        );
        let throttle = parse_nl_command(
            "throttle operation a1b2c3d4e5f60718293a4b5c6d7e8f90 to 50 percent expecting 8",
        )
        .unwrap();
        assert_eq!(
            throttle,
            ControlCommand::ThrottleOperation {
                control_command_id: plan_id(),
                target_id: plan_id(),
                expected_generation_or_revision: 8,
                throttle_percent: 50,
                reason: NL_THROTTLE_REASON.to_owned(),
            }
        );
        for sentence in [
            "  THROTTLE  Operation \t A1B2C3D4E5F60718293A4B5C6D7E8F90 \t TO \t 1 \t PERCENT  EXPECTING  8 ",
            "throttle operation a1b2c3d4e5f60718293a4b5c6d7e8f90 to 100 percent expecting 8",
        ] {
            assert_eq!(
                parse_nl_command(sentence).unwrap(),
                ControlCommand::ThrottleOperation {
                    control_command_id: plan_id(),
                    target_id: plan_id(),
                    expected_generation_or_revision: 8,
                    throttle_percent: if sentence.contains("100") { 100 } else { 1 },
                    reason: NL_THROTTLE_REASON.to_owned(),
                },
                "sentence: {sentence:?}"
            );
        }
        let reclaim =
            parse_nl_command("reclaim operation a1b2c3d4e5f60718293a4b5c6d7e8f90 expecting 9")
                .unwrap();
        assert_eq!(
            reclaim,
            ControlCommand::ReclaimOperation {
                control_command_id: plan_id(),
                target_id: plan_id(),
                expected_generation_or_revision: 9,
                reason: NL_RECLAIM_REASON.to_owned(),
            }
        );
    }

    #[test]
    fn chinese_w29d_operation_control_forms_parse_with_derived_identity() {
        assert_eq!(
            parse_nl_command("终止操作 a1b2c3d4e5f60718293a4b5c6d7e8f90 期望 7").unwrap(),
            ControlCommand::KillOperation {
                control_command_id: plan_id(),
                target_id: plan_id(),
                expected_generation_or_revision: 7,
                reason: NL_KILL_REASON.to_owned(),
            }
        );
        assert_eq!(
            parse_nl_command("终止 操作 A1B2C3D4E5F60718293A4B5C6D7E8F90 期望 7").unwrap(),
            ControlCommand::KillOperation {
                control_command_id: plan_id(),
                target_id: plan_id(),
                expected_generation_or_revision: 7,
                reason: NL_KILL_REASON.to_owned(),
            }
        );
        assert_eq!(
            parse_nl_command("限流操作 a1b2c3d4e5f60718293a4b5c6d7e8f90 到 50 百分比 期望 8")
                .unwrap(),
            ControlCommand::ThrottleOperation {
                control_command_id: plan_id(),
                target_id: plan_id(),
                expected_generation_or_revision: 8,
                throttle_percent: 50,
                reason: NL_THROTTLE_REASON.to_owned(),
            }
        );
        assert_eq!(
            parse_nl_command("限流 操作 a1b2c3d4e5f60718293a4b5c6d7e8f90 到 25 百分比 期望 8")
                .unwrap(),
            ControlCommand::ThrottleOperation {
                control_command_id: plan_id(),
                target_id: plan_id(),
                expected_generation_or_revision: 8,
                throttle_percent: 25,
                reason: NL_THROTTLE_REASON.to_owned(),
            }
        );
        assert_eq!(
            parse_nl_command("回收操作 a1b2c3d4e5f60718293a4b5c6d7e8f90 期望 9").unwrap(),
            ControlCommand::ReclaimOperation {
                control_command_id: plan_id(),
                target_id: plan_id(),
                expected_generation_or_revision: 9,
                reason: NL_RECLAIM_REASON.to_owned(),
            }
        );
        assert_eq!(
            parse_nl_command("回收 操作 a1b2c3d4e5f60718293a4b5c6d7e8f90 期望 9").unwrap(),
            ControlCommand::ReclaimOperation {
                control_command_id: plan_id(),
                target_id: plan_id(),
                expected_generation_or_revision: 9,
                reason: NL_RECLAIM_REASON.to_owned(),
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
            "health check now",
            "health status now",
            "status health now",
            "status",
            "health",
            "inspect system health now",
            "查看 系统",
            "系统状态了",
            "健康状态了",
            "show health now",
            "显示健康",
            "export",
            "export metric",
            "export metrics now",
            "metrics now",
            "show metric",
            "get metric",
            "导出",
            "导出指标了",
            "指标了",
            "inspect",
            "inspect task",
            "inspect health now",
            "inspect tasks a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "inspect task 1234",
            "inspect task zz313233343536373839303132333435",
            "task status",
            "task status now",
            "任务状态",
            "任务 状态",
            "任务状态了",
            "inspect process",
            "inspect processes a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "inspect process 1234",
            "inspect resource",
            "inspect resources a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "inspect resource 1234",
            "resource status",
            "resource status now",
            "资源状态",
            "资源 状态",
            "资源状态了",
            "查看资源",
            "查看 资源",
            "检查资源",
            "检查 资源",
            "检查进程",
            "查看 进程",
            "acknowledge alert",
            format!("cancel alert {PLAN_HEX_LOWER} expecting 1").as_str(),
            format!("acknowledge alert {PLAN_HEX_LOWER}").as_str(),
            format!("acknowledge alert {PLAN_HEX_LOWER} expecting").as_str(),
            format!("acknowledge alert {PLAN_HEX_LOWER} expecting -1").as_str(),
            format!("acknowledge alert {PLAN_HEX_LOWER} expecting +1").as_str(),
            format!("acknowledge alert {PLAN_HEX_LOWER} expecting 1.5").as_str(),
            format!("acknowledge alert {PLAN_HEX_LOWER} expecting {long_count}").as_str(),
            format!("acknowledge alert {PLAN_HEX_LOWER} expecting 1 extra").as_str(),
            format!("ack alert {PLAN_HEX_LOWER}").as_str(),
            format!("confirm alert {PLAN_HEX_LOWER} expecting").as_str(),
            "pause",
            "pause operation",
            format!("pause operation {PLAN_HEX_LOWER}").as_str(),
            format!("pause operation {PLAN_HEX_LOWER} expecting").as_str(),
            format!("pause operation {PLAN_HEX_LOWER} expecting -2").as_str(),
            format!("pause task {PLAN_HEX_LOWER} expecting 1").as_str(),
            "resume operation",
            "suspend",
            format!("cancel operation {PLAN_HEX_LOWER} expecting 1 extra").as_str(),
            format!("abort alert {PLAN_HEX_LOWER} expecting 1").as_str(),
            "暂停操作",
            format!("暂停操作 {PLAN_HEX_LOWER}").as_str(),
            format!("暂停操作 {PLAN_HEX_LOWER} 期望").as_str(),
            format!("暂停 操作 {PLAN_HEX_LOWER} 期望 一").as_str(),
            "恢复操作",
            format!("取消操作 {PLAN_HEX_LOWER} 期望了 6").as_str(),
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

    #[test]
    fn w29d_out_of_grammar_inputs_fail_typed_with_a_reason() {
        for input in [
            "kill",
            "kill operation",
            format!("kill operation {PLAN_HEX_LOWER}").as_str(),
            format!("kill operation {PLAN_HEX_LOWER} expecting").as_str(),
            format!("kill operation {PLAN_HEX_LOWER} expecting -3").as_str(),
            format!("kill task {PLAN_HEX_LOWER} expecting 1").as_str(),
            "terminate",
            format!("terminate alert {PLAN_HEX_LOWER} expecting 1").as_str(),
            "throttle",
            "throttle operation",
            format!("throttle operation {PLAN_HEX_LOWER} expecting 4").as_str(),
            format!("throttle operation {PLAN_HEX_LOWER} to 50 expecting 4").as_str(),
            format!("throttle operation {PLAN_HEX_LOWER} to 50 percent").as_str(),
            format!("throttle operation {PLAN_HEX_LOWER} to 0 percent expecting 4").as_str(),
            format!("throttle operation {PLAN_HEX_LOWER} to 101 percent expecting 4").as_str(),
            format!("throttle operation {PLAN_HEX_LOWER} to -5 percent expecting 4").as_str(),
            format!("throttle task {PLAN_HEX_LOWER} to 50 percent expecting 4").as_str(),
            "reclaim",
            "reclaim operation",
            format!("reclaim operation {PLAN_HEX_LOWER}").as_str(),
            format!("reclaim operation {PLAN_HEX_LOWER} expecting -1").as_str(),
            format!("reclaim task {PLAN_HEX_LOWER} expecting 1").as_str(),
            "终止操作",
            format!("终止操作 {PLAN_HEX_LOWER}").as_str(),
            "限流操作",
            format!("限流操作 {PLAN_HEX_LOWER} 期望 4").as_str(),
            format!("限流操作 {PLAN_HEX_LOWER} 到 50 百分比 期望").as_str(),
            format!("限流操作 {PLAN_HEX_LOWER} 到 0 百分比 期望 4").as_str(),
            format!("限流操作 {PLAN_HEX_LOWER} 到 101 百分比 期望 4").as_str(),
            "回收操作",
            format!("回收操作 {PLAN_HEX_LOWER} 期望").as_str(),
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
