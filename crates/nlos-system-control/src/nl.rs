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
//! inspect resource recovery | check resource recovery | show resource recovery
//!   | get resource recovery | status resource recovery | resource recovery status
//!   | resource recovery check
//!   | 查看资源恢复 | 查看 资源 恢复 | 检查资源恢复 | 检查 资源 恢复
//!   | 资源恢复状态 | 资源恢复 状态
//! export resource metrics | show resource metrics | get resource metrics
//!   | resource metrics
//!   | 导出资源指标 | 导出 资源 指标 | 资源指标
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
//! inspect task group <32-hex> | check task group <32-hex> | show task group <32-hex>
//!   | get task group <32-hex> | status task group <32-hex>
//!   | task group status <32-hex>
//!   | 查看任务组 <32位十六进制> | 查看 任务组 <32位十六进制> | 检查任务组 <32位十六进制>
//!   | 任务组状态 <32位十六进制> | 任务组 状态 <32位十六进制>
//! inspect task node <32-hex> <32-hex> | check task node <32-hex> <32-hex>
//!   | show task node <32-hex> <32-hex> | get task node <32-hex> <32-hex>
//!   | status task node <32-hex> <32-hex> | task node status <32-hex> <32-hex>
//!   | 查看任务节点 <32位十六进制> <32位十六进制> | 查看 任务节点 <32位十六进制> <32位十六进制>
//!   | 检查任务节点 <32位十六进制> <32位十六进制>
//!   | 任务节点状态 <32位十六进制> <32位十六进制>
//! inspect fiber <32-hex> generation <n> | check fiber <32-hex> generation <n>
//!   | show fiber <32-hex> generation <n> | get fiber <32-hex> generation <n>
//!   | status fiber <32-hex> generation <n> | fiber status <32-hex> generation <n>
//!   | 查看纤程 <32位十六进制> 世代 <n> | 查看 纤程 <32位十六进制> 世代 <n>
//!   | 纤程状态 <32位十六进制> 世代 <n>
//! inspect topic <32-hex> | check topic <32-hex> | show topic <32-hex>
//!   | get topic <32-hex> | status topic <32-hex> | topic status <32-hex>
//!   | 查看主题 <32位十六进制> | 查看 主题 <32位十六进制>
//!   | 主题状态 <32位十六进制> | 主题 状态 <32位十六进制>
//! inspect operation <32-hex> generation <n> | check operation <32-hex> generation <n>
//!   | show operation <32-hex> generation <n> | get operation <32-hex> generation <n>
//!   | status operation <32-hex> generation <n> | operation status <32-hex> generation <n>
//!   | 查看操作 <32位十六进制> 世代 <n> | 查看 操作 <32位十六进制> 世代 <n>
//!   | 操作状态 <32位十六进制> 世代 <n>
//! acknowledge alert <32-hex> expecting <n>
//!   | ack alert <32-hex> expecting <n> | confirm alert <32-hex> expecting <n>
//!   | 确认告警 <32位十六进制> 期望 <n> | 确认 告警 <32位十六进制> 期望 <n>
//! acknowledge resource alert <32-hex> expecting <n>
//!   | ack resource alert <32-hex> expecting <n>
//!   | confirm resource alert <32-hex> expecting <n>
//!   | 确认资源告警 <32位十六进制> 期望 <n> | 确认 资源 告警 <32位十六进制> 期望 <n>
//! resume resource recovery <32-hex> expecting <n>
//!   | 恢复资源恢复 <32位十六进制> 期望 <n> | 恢复 资源恢复 <32位十六进制> 期望 <n>
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
//! disable application <32-hex> expecting <n>
//!   | 禁用应用 <32位十六进制> 期望 <n> | 禁用 应用 <32位十六进制> 期望 <n>
//! uninstall application <32-hex> expecting <n>
//!   | 卸载应用 <32位十六进制> 期望 <n> | 卸载 应用 <32位十六进制> 期望 <n>
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
//! The W35-P11 application-lifecycle forms follow the same derivation
//! rules: the `<32-hex>` argument is the package identity (the application
//! singleton is authority-derived from it), the command identity derives
//! from that package id (one package, one disable/uninstall identity,
//! replay-safe), `<n>` is the explicit application installation generation
//! — the CAS expectation — and the audit reasons are the fixed
//! [`NL_DISABLE_REASON`]/[`NL_UNINSTALL_REASON`]. There is no application
//! inspect form in this slice.
//!
//! The W28-C-3b resource recovery forms follow the same rules verbatim: the
//! acknowledgement targets the escalated resource plan (command identity
//! derives from the plan id, replay-safe), the resume form requeues the
//! escalated resource plan under its explicit CAS expectation, and the audit
//! reasons are the fixed [`NL_RESOURCE_ACK_REASON`]/
//! [`NL_RESOURCE_RESUME_REASON`]. The read forms carry no argument — the
//! domain health and metrics projections are aggregate snapshots.
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
/// See [`NL_ACK_REASON`]; W28-C-3b resource recovery forms.
pub const NL_RESOURCE_ACK_REASON: &str =
    "resource alert acknowledged through the restricted natural-language control prefix";
/// See [`NL_ACK_REASON`].
pub const NL_RESOURCE_RESUME_REASON: &str =
    "resource recovery resumed through the restricted natural-language control prefix";
/// See [`NL_ACK_REASON`]; W35-P11 application-lifecycle forms.
pub const NL_DISABLE_REASON: &str =
    "application disabled through the restricted natural-language control prefix";
/// See [`NL_ACK_REASON`].
pub const NL_UNINSTALL_REASON: &str =
    "application uninstalled through the restricted natural-language control prefix";

/// Legal grammar, named verbatim in every rejection message.
const GRAMMAR_HELP: &str = "valid forms: \"inspect health\" | \"export metrics\" | \
\"inspect resource recovery\" | \"export resource metrics\" | \
\"inspect task <32-hex>\" | \"inspect process <32-hex>\" | \"inspect resource <32-hex>\" | \
\"inspect task group <32-hex>\" | \"inspect task node <32-hex> <32-hex>\" | \
\"inspect fiber <32-hex> generation <count>\" | \"inspect topic <32-hex>\" | \
\"inspect operation <32-hex> generation <count>\" | \
\"acknowledge alert <32-hex> expecting <count>\" | \
\"acknowledge resource alert <32-hex> expecting <count>\" | \
\"resume resource recovery <32-hex> expecting <count>\" | \
\"pause|resume|cancel|kill|reclaim operation <32-hex> expecting <count>\" | \
\"throttle operation <32-hex> to <percent> expecting <count>\" | \
\"disable|uninstall application <32-hex> expecting <count>\" | \"查看健康\" | \
\"导出指标\" | \"查看资源恢复\" | \"导出资源指标\" | \
\"查看任务 <32位十六进制>\" | \"检查进程 <32位十六进制>\" | \
\"查看资源 <32位十六进制>\" | \
\"确认告警 <32位十六进制> 期望 <次数>\" | \
\"确认资源告警 <32位十六进制> 期望 <次数>\" | \
\"恢复资源恢复 <32位十六进制> 期望 <次数>\" | \
\"暂停|恢复|取消|终止|回收操作 <32位十六进制> 期望 <次数>\" | \
\"限流操作 <32位十六进制> 到 <百分比> 期望 <次数>\" | \
\"禁用|卸载应用 <32位十六进制> 期望 <次数>\"";

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
    if let Some(result) = try_parse_resource_recovery(&tokens) {
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
    if let Some(result) = try_parse_layer_inspects(&tokens) {
        return result;
    }
    if let Some(result) = try_parse_acknowledgement(&tokens) {
        return result;
    }
    if let Some(result) = try_parse_operation_control(&tokens) {
        return result;
    }
    if let Some(result) = try_parse_application_control(&tokens) {
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

fn is_disable_application_verb(token: &str) -> bool {
    token.eq_ignore_ascii_case("disable")
}

fn is_uninstall_application_verb(token: &str) -> bool {
    token.eq_ignore_ascii_case("uninstall")
}

fn is_resource_recovery_resume_verb(token: &str) -> bool {
    token.eq_ignore_ascii_case("resume")
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
        [head, second, ..]
            if (is_read_verb(head)
                && (second.eq_ignore_ascii_case("fiber")
                    || second.eq_ignore_ascii_case("topic")
                    || second.eq_ignore_ascii_case("operation")))
                || (*head == "查看"
                    && (*second == "纤程"
                        || *second == "主题"
                        || *second == "操作"
                        || *second == "任务组"
                        || *second == "任务节点"))
                || (*head == "检查" && (*second == "任务组" || *second == "任务节点")) =>
        {
            None
        }
        [head, second, ..] if is_read_verb(head) && second.eq_ignore_ascii_case("metrics") => None,
        [head, ..] if is_read_verb(head) || *head == "查看" || *head == "检查" => {
            Some(Err(ControlError::InvalidCommand(
                "\"inspect\" expects \"health\", \"resource recovery\", \"task <32-hex>\", \
                 \"process <32-hex>\", or \"resource <32-hex>\"",
            )))
        }
        _ => None,
    }
}

/// Compiles the W28-C-3b resource recovery forms: the two aggregate read
/// projections (domain health, domain metrics) and the two escalated-plan
/// mutations (acknowledge, resume), with the same deterministic identity
/// derivation rules as the artifact and operation-level forms.
fn try_parse_resource_recovery(tokens: &[&str]) -> Option<Result<ControlCommand, ControlError>> {
    match tokens {
        [head, second, third]
            if is_read_verb(head)
                && second.eq_ignore_ascii_case("resource")
                && third.eq_ignore_ascii_case("recovery") =>
        {
            Some(Ok(ControlCommand::InspectResourceHealth))
        }
        [first, second, third]
            if first.eq_ignore_ascii_case("resource")
                && second.eq_ignore_ascii_case("recovery")
                && (third.eq_ignore_ascii_case("status")
                    || third.eq_ignore_ascii_case("check")
                    || third.eq_ignore_ascii_case("health")) =>
        {
            Some(Ok(ControlCommand::InspectResourceHealth))
        }
        ["查看资源恢复" | "检查资源恢复" | "资源恢复状态"]
        | ["查看" | "检查", "资源", "恢复"]
        | ["资源恢复", "状态"] => Some(Ok(ControlCommand::InspectResourceHealth)),
        [head, second, third]
            if is_metrics_verb(head)
                && second.eq_ignore_ascii_case("resource")
                && third.eq_ignore_ascii_case("metrics") =>
        {
            Some(Ok(ControlCommand::ExportResourceMetrics))
        }
        [first, second]
            if first.eq_ignore_ascii_case("resource") && second.eq_ignore_ascii_case("metrics") =>
        {
            Some(Ok(ControlCommand::ExportResourceMetrics))
        }
        ["导出资源指标" | "资源指标"] | ["导出", "资源", "指标"] => {
            Some(Ok(ControlCommand::ExportResourceMetrics))
        }
        [head, second, third, plan, fourth, count]
            if is_ack_verb(head)
                && second.eq_ignore_ascii_case("resource")
                && third.eq_ignore_ascii_case("alert")
                && fourth.eq_ignore_ascii_case("expecting") =>
        {
            Some(parse_count(count).and_then(|n| acknowledge_resource_alert(plan, n)))
        }
        ["确认资源告警", plan, "期望", count] => {
            Some(parse_count(count).and_then(|n| acknowledge_resource_alert(plan, n)))
        }
        ["确认", "资源", "告警", plan, "期望", count] => {
            Some(parse_count(count).and_then(|n| acknowledge_resource_alert(plan, n)))
        }
        [head, second, third, plan, fourth, count]
            if is_resource_recovery_resume_verb(head)
                && second.eq_ignore_ascii_case("resource")
                && third.eq_ignore_ascii_case("recovery")
                && fourth.eq_ignore_ascii_case("expecting") =>
        {
            Some(parse_count(count).and_then(|n| resume_resource_recovery_command(plan, n)))
        }
        ["恢复资源恢复", plan, "期望", count] | ["恢复", "资源恢复", plan, "期望", count] => {
            Some(parse_count(count).and_then(|n| resume_resource_recovery_command(plan, n)))
        }
        [head, second, ..] if is_ack_verb(head) && second.eq_ignore_ascii_case("resource") => {
            Some(Err(ControlError::InvalidCommand(
                "\"acknowledge resource alert\" expects \"<32-hex> expecting <count>\"",
            )))
        }
        ["确认资源告警", ..] | ["确认", "资源", ..] => Some(Err(
            ControlError::InvalidCommand("\"确认资源告警\" 期望 \"<32位十六进制> 期望 <次数>\""),
        )),
        [head, second, ..]
            if is_resource_recovery_resume_verb(head)
                && second.eq_ignore_ascii_case("resource") =>
        {
            Some(Err(ControlError::InvalidCommand(
                "\"resume resource recovery\" expects \"<32-hex> expecting <count>\"",
            )))
        }
        ["恢复资源恢复", ..] | ["恢复", "资源恢复", ..] => Some(Err(
            ControlError::InvalidCommand("\"恢复资源恢复\" 期望 \"<32位十六进制> 期望 <次数>\""),
        )),
        _ => None,
    }
}

/// Compiles the resource acknowledgement with the deterministic derivation
/// rules documented at the module level (identity = plan id, replay-safe).
fn acknowledge_resource_alert(
    plan_hex: &str,
    expected_total_failures: u64,
) -> Result<ControlCommand, ControlError> {
    let plan_id = parse_hex_id(plan_hex)?;
    Ok(ControlCommand::AcknowledgeResourceRecoveryAlert {
        control_command_id: plan_id,
        plan_id,
        expected_total_failures,
        reason: NL_RESOURCE_ACK_REASON.to_owned(),
    })
}

fn resume_resource_recovery_command(
    plan_hex: &str,
    expected_total_failures: u64,
) -> Result<ControlCommand, ControlError> {
    let plan_id = parse_hex_id(plan_hex)?;
    Ok(ControlCommand::ResumeResourceRecovery {
        control_command_id: plan_id,
        plan_id,
        expected_total_failures,
        reason: NL_RESOURCE_RESUME_REASON.to_owned(),
    })
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

/// Compiles the W32-G per-layer read forms (B5-3): `TaskGroup`, `TaskNode`,
/// `ExecutionFiber`, `Topic`, and durable `Operation` inspections. The command
/// identity derives from the addressed target (one target, one inspect
/// identity, replay-safe); the fiber and operation forms carry the handle
/// generation explicitly because silently guessing a handle generation
/// would violate `[NL-AMBIG-001]` exactly like a guessed CAS value.
fn try_parse_layer_inspects(tokens: &[&str]) -> Option<Result<ControlCommand, ControlError>> {
    try_parse_layer_inspects_en(tokens)
        .or_else(|| try_parse_layer_status_forms(tokens))
        .or_else(|| try_parse_layer_inspects_zh(tokens))
}

fn try_parse_layer_inspects_en(tokens: &[&str]) -> Option<Result<ControlCommand, ControlError>> {
    match tokens {
        [head, second, third, group]
            if is_read_verb(head)
                && second.eq_ignore_ascii_case("task")
                && third.eq_ignore_ascii_case("group") =>
        {
            Some(parse_hex_id(group).map(|group_id| ControlCommand::InspectTaskGroup { group_id }))
        }
        [head, second, third, plan, node]
            if is_read_verb(head)
                && second.eq_ignore_ascii_case("task")
                && third.eq_ignore_ascii_case("node") =>
        {
            Some(
                parse_hex_id(plan)
                    .and_then(|plan_id| parse_hex_id(node).map(|node_id| (plan_id, node_id)))
                    .map(|(plan_id, node_id)| ControlCommand::InspectTaskNode { plan_id, node_id }),
            )
        }
        [head, second, fiber, third, generation]
            if is_read_verb(head)
                && second.eq_ignore_ascii_case("fiber")
                && third.eq_ignore_ascii_case("generation") =>
        {
            Some(
                parse_hex_id(fiber)
                    .and_then(|fiber_id| {
                        parse_generation(generation).map(|generation| (fiber_id, generation))
                    })
                    .map(
                        |(fiber_id, generation)| ControlCommand::InspectExecutionFiber {
                            fiber_id,
                            generation,
                        },
                    ),
            )
        }
        [head, second, topic] if is_read_verb(head) && second.eq_ignore_ascii_case("topic") => {
            Some(parse_hex_id(topic).map(|topic_id| ControlCommand::InspectTopic { topic_id }))
        }
        [head, second, operation, third, generation]
            if is_read_verb(head)
                && second.eq_ignore_ascii_case("operation")
                && third.eq_ignore_ascii_case("generation") =>
        {
            Some(
                parse_hex_id(operation)
                    .and_then(|operation_id| {
                        parse_generation(generation).map(|generation| (operation_id, generation))
                    })
                    .map(
                        |(operation_id, generation)| ControlCommand::InspectOperation {
                            operation_id,
                            generation,
                        },
                    ),
            )
        }
        [head, second, ..]
            if is_read_verb(head)
                && (second.eq_ignore_ascii_case("fiber")
                    || second.eq_ignore_ascii_case("topic")
                    || second.eq_ignore_ascii_case("operation")) =>
        {
            Some(Err(ControlError::InvalidCommand(
                "\"inspect fiber|operation\" expects \"<32-hex> generation <count>\"; \
                 \"inspect topic\" expects \"<32-hex>\"",
            )))
        }
        _ => None,
    }
}

fn try_parse_layer_status_forms(tokens: &[&str]) -> Option<Result<ControlCommand, ControlError>> {
    match tokens {
        [first, second, third, group]
            if first.eq_ignore_ascii_case("task")
                && second.eq_ignore_ascii_case("group")
                && third.eq_ignore_ascii_case("status") =>
        {
            Some(parse_hex_id(group).map(|group_id| ControlCommand::InspectTaskGroup { group_id }))
        }
        [first, second, third, plan, node]
            if first.eq_ignore_ascii_case("task")
                && second.eq_ignore_ascii_case("node")
                && third.eq_ignore_ascii_case("status") =>
        {
            Some(
                parse_hex_id(plan)
                    .and_then(|plan_id| parse_hex_id(node).map(|node_id| (plan_id, node_id)))
                    .map(|(plan_id, node_id)| ControlCommand::InspectTaskNode { plan_id, node_id }),
            )
        }
        [first, second, fiber, third, generation]
            if first.eq_ignore_ascii_case("fiber")
                && second.eq_ignore_ascii_case("status")
                && third.eq_ignore_ascii_case("generation") =>
        {
            Some(
                parse_hex_id(fiber)
                    .and_then(|fiber_id| {
                        parse_generation(generation).map(|generation| (fiber_id, generation))
                    })
                    .map(
                        |(fiber_id, generation)| ControlCommand::InspectExecutionFiber {
                            fiber_id,
                            generation,
                        },
                    ),
            )
        }
        [first, second, topic]
            if first.eq_ignore_ascii_case("topic") && second.eq_ignore_ascii_case("status") =>
        {
            Some(parse_hex_id(topic).map(|topic_id| ControlCommand::InspectTopic { topic_id }))
        }
        [first, second, operation, third, generation]
            if first.eq_ignore_ascii_case("operation")
                && second.eq_ignore_ascii_case("status")
                && third.eq_ignore_ascii_case("generation") =>
        {
            Some(
                parse_hex_id(operation)
                    .and_then(|operation_id| {
                        parse_generation(generation).map(|generation| (operation_id, generation))
                    })
                    .map(
                        |(operation_id, generation)| ControlCommand::InspectOperation {
                            operation_id,
                            generation,
                        },
                    ),
            )
        }
        _ => None,
    }
}

fn try_parse_layer_inspects_zh(tokens: &[&str]) -> Option<Result<ControlCommand, ControlError>> {
    match tokens {
        ["查看任务组" | "检查任务组", group] | ["查看" | "检查", "任务组", group] => {
            Some(parse_hex_id(group).map(|group_id| ControlCommand::InspectTaskGroup { group_id }))
        }
        ["任务组状态", group] | ["任务组", "状态", group] => {
            Some(parse_hex_id(group).map(|group_id| ControlCommand::InspectTaskGroup { group_id }))
        }
        ["查看任务节点" | "检查任务节点", plan, node]
        | ["查看" | "检查", "任务节点", plan, node] => Some(
            parse_hex_id(plan)
                .and_then(|plan_id| parse_hex_id(node).map(|node_id| (plan_id, node_id)))
                .map(|(plan_id, node_id)| ControlCommand::InspectTaskNode { plan_id, node_id }),
        ),
        ["任务节点状态", plan, node] | ["任务节点", "状态", plan, node] => Some(
            parse_hex_id(plan)
                .and_then(|plan_id| parse_hex_id(node).map(|node_id| (plan_id, node_id)))
                .map(|(plan_id, node_id)| ControlCommand::InspectTaskNode { plan_id, node_id }),
        ),
        ["查看纤程", fiber, "世代", generation] | ["查看", "纤程", fiber, "世代", generation] => {
            Some(
                parse_hex_id(fiber)
                    .and_then(|fiber_id| {
                        parse_generation(generation).map(|generation| (fiber_id, generation))
                    })
                    .map(
                        |(fiber_id, generation)| ControlCommand::InspectExecutionFiber {
                            fiber_id,
                            generation,
                        },
                    ),
            )
        }
        ["纤程状态", fiber, "世代", generation] | ["纤程", "状态", fiber, "世代", generation] => {
            Some(
                parse_hex_id(fiber)
                    .and_then(|fiber_id| {
                        parse_generation(generation).map(|generation| (fiber_id, generation))
                    })
                    .map(
                        |(fiber_id, generation)| ControlCommand::InspectExecutionFiber {
                            fiber_id,
                            generation,
                        },
                    ),
            )
        }
        ["查看主题", topic] | ["查看", "主题", topic] => {
            Some(parse_hex_id(topic).map(|topic_id| ControlCommand::InspectTopic { topic_id }))
        }
        ["主题状态", topic] | ["主题", "状态", topic] => {
            Some(parse_hex_id(topic).map(|topic_id| ControlCommand::InspectTopic { topic_id }))
        }
        ["查看操作", operation, "世代", generation]
        | ["查看", "操作", operation, "世代", generation] => Some(
            parse_hex_id(operation)
                .and_then(|operation_id| {
                    parse_generation(generation).map(|generation| (operation_id, generation))
                })
                .map(
                    |(operation_id, generation)| ControlCommand::InspectOperation {
                        operation_id,
                        generation,
                    },
                ),
        ),
        ["操作状态", operation, "世代", generation]
        | ["操作", "状态", operation, "世代", generation] => Some(
            parse_hex_id(operation)
                .and_then(|operation_id| {
                    parse_generation(generation).map(|generation| (operation_id, generation))
                })
                .map(
                    |(operation_id, generation)| ControlCommand::InspectOperation {
                        operation_id,
                        generation,
                    },
                ),
        ),
        ["查看纤程", ..] | ["查看", "纤程", ..] => Some(Err(ControlError::InvalidCommand(
            "\"查看纤程\" 期望 \"<32位十六进制> 世代 <次数>\"",
        ))),
        ["查看操作", ..] | ["查看", "操作", ..] => Some(Err(ControlError::InvalidCommand(
            "\"查看操作\" 期望 \"<32位十六进制> 世代 <次数>\"",
        ))),
        ["查看任务组" | "检查任务组", ..] | ["查看" | "检查", "任务组", ..] => {
            Some(Err(ControlError::InvalidCommand(
                "\"查看任务组\" 期望 \"<32位十六进制>\"",
            )))
        }
        ["查看任务节点" | "检查任务节点", ..] | ["查看" | "检查", "任务节点", ..] => {
            Some(Err(ControlError::InvalidCommand(
                "\"查看任务节点\" 期望 \"<32位十六进制> <32位十六进制>\"",
            )))
        }
        _ => None,
    }
}

/// Parses the fiber/operation handle generation: a positive plain decimal
/// (the runtime and operation authorities never resolve generation zero).
fn parse_generation(token: &str) -> Result<u64, ControlError> {
    if !token.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ControlError::InvalidCommand(
            "handle generation must be a plain decimal count (digits only)",
        ));
    }
    let generation = token
        .parse::<u64>()
        .map_err(|_| ControlError::InvalidCommand("handle generation exceeds the 64-bit bound"))?;
    if generation == 0 {
        return Err(ControlError::InvalidCommand(
            "handle generation must be a positive generation",
        ));
    }
    Ok(generation)
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

/// Compiles the W35-P11 application-lifecycle forms (移交#11 前片):
/// `disable application` and `uninstall application`, each addressing the
/// 16-byte package identity under an explicit installation-generation CAS
/// expectation. The command identity derives from the package id (one
/// package, one disable/uninstall identity, replay-safe); silently guessing
/// a CAS value would violate `[NL-AMBIG-001]`, so the expectation is a
/// required token.
fn try_parse_application_control(tokens: &[&str]) -> Option<Result<ControlCommand, ControlError>> {
    match tokens {
        [head, second, package, third, count]
            if is_disable_application_verb(head)
                && second.eq_ignore_ascii_case("application")
                && third.eq_ignore_ascii_case("expecting") =>
        {
            Some(parse_count(count).and_then(|n| disable_application_command(package, n)))
        }
        ["禁用应用", package, "期望", count] | ["禁用", "应用", package, "期望", count] => {
            Some(parse_count(count).and_then(|n| disable_application_command(package, n)))
        }
        [head, second, package, third, count]
            if is_uninstall_application_verb(head)
                && second.eq_ignore_ascii_case("application")
                && third.eq_ignore_ascii_case("expecting") =>
        {
            Some(parse_count(count).and_then(|n| uninstall_application_command(package, n)))
        }
        ["卸载应用", package, "期望", count] | ["卸载", "应用", package, "期望", count] => {
            Some(parse_count(count).and_then(|n| uninstall_application_command(package, n)))
        }
        ["禁用应用", ..] | ["禁用", "应用", ..] => Some(Err(ControlError::InvalidCommand(
            "\"禁用应用\" 期望 \"<32位十六进制> 期望 <次数>\"",
        ))),
        ["卸载应用", ..] | ["卸载", "应用", ..] => Some(Err(ControlError::InvalidCommand(
            "\"卸载应用\" 期望 \"<32位十六进制> 期望 <次数>\"",
        ))),
        [head, ..] if is_disable_application_verb(head) || is_uninstall_application_verb(head) => {
            Some(Err(ControlError::InvalidCommand(
                "\"disable|uninstall application\" expects \"<32-hex> expecting <count>\"",
            )))
        }
        _ => None,
    }
}

fn disable_application_command(
    package_hex: &str,
    expected_generation_or_revision: u64,
) -> Result<ControlCommand, ControlError> {
    let package_id = parse_hex_id(package_hex)?;
    Ok(ControlCommand::DisableApplication {
        control_command_id: package_id,
        package_id,
        expected_generation_or_revision,
        reason: NL_DISABLE_REASON.to_owned(),
    })
}

fn uninstall_application_command(
    package_hex: &str,
    expected_generation_or_revision: u64,
) -> Result<ControlCommand, ControlError> {
    let package_id = parse_hex_id(package_hex)?;
    Ok(ControlCommand::UninstallApplication {
        control_command_id: package_id,
        package_id,
        expected_generation_or_revision,
        reason: NL_UNINSTALL_REASON.to_owned(),
    })
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
    fn english_application_lifecycle_forms_parse_with_derived_identity() {
        assert_eq!(
            parse_nl_command("disable application a1b2c3d4e5f60718293a4b5c6d7e8f90 expecting 4")
                .unwrap(),
            ControlCommand::DisableApplication {
                control_command_id: plan_id(),
                package_id: plan_id(),
                expected_generation_or_revision: 4,
                reason: NL_DISABLE_REASON.to_owned(),
            }
        );
        assert_eq!(
            parse_nl_command("DISABLE APPLICATION A1B2C3D4E5F60718293A4B5C6D7E8F90 EXPECTING 4")
                .unwrap(),
            ControlCommand::DisableApplication {
                control_command_id: plan_id(),
                package_id: plan_id(),
                expected_generation_or_revision: 4,
                reason: NL_DISABLE_REASON.to_owned(),
            }
        );
        assert_eq!(
            parse_nl_command("uninstall application a1b2c3d4e5f60718293a4b5c6d7e8f90 expecting 4")
                .unwrap(),
            ControlCommand::UninstallApplication {
                control_command_id: plan_id(),
                package_id: plan_id(),
                expected_generation_or_revision: 4,
                reason: NL_UNINSTALL_REASON.to_owned(),
            }
        );
    }

    #[test]
    fn chinese_application_lifecycle_forms_parse_with_derived_identity() {
        assert_eq!(
            parse_nl_command("禁用应用 a1b2c3d4e5f60718293a4b5c6d7e8f90 期望 4").unwrap(),
            ControlCommand::DisableApplication {
                control_command_id: plan_id(),
                package_id: plan_id(),
                expected_generation_or_revision: 4,
                reason: NL_DISABLE_REASON.to_owned(),
            }
        );
        assert_eq!(
            parse_nl_command("禁用 应用 A1B2C3D4E5F60718293A4B5C6D7E8F90 期望 4").unwrap(),
            ControlCommand::DisableApplication {
                control_command_id: plan_id(),
                package_id: plan_id(),
                expected_generation_or_revision: 4,
                reason: NL_DISABLE_REASON.to_owned(),
            }
        );
        assert_eq!(
            parse_nl_command("卸载应用 a1b2c3d4e5f60718293a4b5c6d7e8f90 期望 4").unwrap(),
            ControlCommand::UninstallApplication {
                control_command_id: plan_id(),
                package_id: plan_id(),
                expected_generation_or_revision: 4,
                reason: NL_UNINSTALL_REASON.to_owned(),
            }
        );
        assert_eq!(
            parse_nl_command("卸载 应用 a1b2c3d4e5f60718293a4b5c6d7e8f90 期望 4").unwrap(),
            ControlCommand::UninstallApplication {
                control_command_id: plan_id(),
                package_id: plan_id(),
                expected_generation_or_revision: 4,
                reason: NL_UNINSTALL_REASON.to_owned(),
            }
        );
    }

    #[test]
    fn application_lifecycle_near_misses_are_typed_rejections() {
        for sentence in [
            "disable application a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "disable application a1b2c3d4e5f60718293a4b5c6d7e8f90 expecting",
            "disable application a1b2c3d4e5f60718293a4b5c6d7e8f9 expecting 4",
            "disable application a1b2c3d4e5f60718293a4b5c6d7e8f90 expecting -4",
            "disable operation a1b2c3d4e5f60718293a4b5c6d7e8f90 expecting 4",
            "uninstall task a1b2c3d4e5f60718293a4b5c6d7e8f90 expecting 4",
            "uninstall application a1b2c3d4e5f60718293a4b5c6d7e8f90 expecting 0x4",
            "禁用应用 a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "卸载应用",
            "卸载 应用 a1b2c3d4e5f60718293a4b5c6d7e8f90 期望",
            "inspect application a1b2c3d4e5f60718293a4b5c6d7e8f90",
        ] {
            assert!(
                matches!(
                    parse_nl_command(sentence),
                    Err(ControlError::InvalidCommand(_))
                ),
                "expected typed NL rejection for {sentence:?}"
            );
        }
    }

    #[test]
    fn english_resource_recovery_read_forms_parse() {
        for sentence in [
            "inspect resource recovery",
            "INSPECT RESOURCE RECOVERY",
            "Inspect\tResource\tRecovery",
            "check resource recovery",
            "show resource recovery",
            "get resource recovery",
            "status resource recovery",
            "resource recovery status",
            "RESOURCE RECOVERY STATUS",
            "resource recovery check",
            "resource recovery health",
        ] {
            assert_eq!(
                parse_nl_command(sentence).unwrap(),
                ControlCommand::InspectResourceHealth,
                "sentence: {sentence:?}"
            );
        }
        for sentence in [
            "export resource metrics",
            "EXPORT RESOURCE METRICS",
            "show resource metrics",
            "get resource metrics",
            "resource metrics",
            "RESOURCE METRICS",
        ] {
            assert_eq!(
                parse_nl_command(sentence).unwrap(),
                ControlCommand::ExportResourceMetrics,
                "sentence: {sentence:?}"
            );
        }
    }

    #[test]
    fn chinese_resource_recovery_read_forms_parse() {
        for sentence in [
            "查看资源恢复",
            "  查看资源恢复  ",
            "查看 资源 恢复",
            "检查资源恢复",
            "检查 资源 恢复",
            "资源恢复状态",
            "资源恢复 状态",
        ] {
            assert_eq!(
                parse_nl_command(sentence).unwrap(),
                ControlCommand::InspectResourceHealth,
                "sentence: {sentence:?}"
            );
        }
        for sentence in ["导出资源指标", "导出 资源 指标", "资源指标"] {
            assert_eq!(
                parse_nl_command(sentence).unwrap(),
                ControlCommand::ExportResourceMetrics,
                "sentence: {sentence:?}"
            );
        }
    }

    #[test]
    fn english_resource_recovery_mutations_parse_with_derived_identity() {
        let acknowledge = parse_nl_command(
            "acknowledge resource alert a1b2c3d4e5f60718293a4b5c6d7e8f90 expecting 8",
        )
        .unwrap();
        assert_eq!(
            acknowledge,
            ControlCommand::AcknowledgeResourceRecoveryAlert {
                control_command_id: plan_id(),
                plan_id: plan_id(),
                expected_total_failures: 8,
                reason: NL_RESOURCE_ACK_REASON.to_owned(),
            }
        );
        for sentence in [
            "  ACK  Resource  Alert  A1B2C3D4E5F60718293A4B5C6D7E8F90 \t EXPECTING  8 ",
            "ack resource alert a1b2c3d4e5f60718293a4b5c6d7e8f90 expecting 8",
            "confirm resource alert a1b2c3d4e5f60718293a4b5c6d7e8f90 expecting 8",
        ] {
            assert_eq!(
                parse_nl_command(sentence).unwrap(),
                acknowledge,
                "sentence: {sentence:?}"
            );
        }
        let resume = parse_nl_command(
            "resume resource recovery a1b2c3d4e5f60718293a4b5c6d7e8f90 expecting 8",
        )
        .unwrap();
        assert_eq!(
            resume,
            ControlCommand::ResumeResourceRecovery {
                control_command_id: plan_id(),
                plan_id: plan_id(),
                expected_total_failures: 8,
                reason: NL_RESOURCE_RESUME_REASON.to_owned(),
            }
        );
        assert_eq!(
            parse_nl_command(
                "  RESUME   Resource  Recovery  A1B2C3D4E5F60718293A4B5C6D7E8F90  EXPECTING  8 "
            )
            .unwrap(),
            resume
        );
    }

    #[test]
    fn chinese_resource_recovery_mutations_parse_with_derived_identity() {
        assert_eq!(
            parse_nl_command("确认资源告警 a1b2c3d4e5f60718293a4b5c6d7e8f90 期望 8").unwrap(),
            ControlCommand::AcknowledgeResourceRecoveryAlert {
                control_command_id: plan_id(),
                plan_id: plan_id(),
                expected_total_failures: 8,
                reason: NL_RESOURCE_ACK_REASON.to_owned(),
            }
        );
        assert_eq!(
            parse_nl_command("确认 资源 告警 A1B2C3D4E5F60718293A4B5C6D7E8F90 期望 8").unwrap(),
            ControlCommand::AcknowledgeResourceRecoveryAlert {
                control_command_id: plan_id(),
                plan_id: plan_id(),
                expected_total_failures: 8,
                reason: NL_RESOURCE_ACK_REASON.to_owned(),
            }
        );
        assert_eq!(
            parse_nl_command("恢复资源恢复 a1b2c3d4e5f60718293a4b5c6d7e8f90 期望 8").unwrap(),
            ControlCommand::ResumeResourceRecovery {
                control_command_id: plan_id(),
                plan_id: plan_id(),
                expected_total_failures: 8,
                reason: NL_RESOURCE_RESUME_REASON.to_owned(),
            }
        );
        assert_eq!(
            parse_nl_command("恢复 资源恢复 A1B2C3D4E5F60718293A4B5C6D7E8F90 期望 8").unwrap(),
            ControlCommand::ResumeResourceRecovery {
                control_command_id: plan_id(),
                plan_id: plan_id(),
                expected_total_failures: 8,
                reason: NL_RESOURCE_RESUME_REASON.to_owned(),
            }
        );
    }

    #[test]
    fn resource_recovery_out_of_grammar_inputs_fail_typed_with_a_reason() {
        for input in [
            "inspect resource recovery now",
            "resource recovery",
            "resource recovery status now",
            "查看 资源",
            "检查资源恢复了",
            "资源恢复状态了",
            "export resource metric",
            "resource metrics now",
            "导出资源指标了",
            "acknowledge resource alert",
            "ack resource alert a1b2c3d4e5f60718293a4b5c6d7e8f90 expecting",
            format!("acknowledge resource alert {PLAN_HEX_LOWER} expecting -1").as_str(),
            format!("acknowledge resource alert {PLAN_HEX_LOWER} expecting 1 extra").as_str(),
            format!("ack resource alert {PLAN_HEX_LOWER} expecting 1.5").as_str(),
            "resume resource recovery",
            format!("resume resource recovery {PLAN_HEX_LOWER}").as_str(),
            format!("resume resource recovery {PLAN_HEX_LOWER} expecting").as_str(),
            format!("resume resource {PLAN_HEX_LOWER} expecting 8").as_str(),
            format!("resume resource alert {PLAN_HEX_LOWER} expecting 8").as_str(),
            "确认资源告警",
            format!("确认资源告警 {PLAN_HEX_LOWER} 期望").as_str(),
            format!("确认 资源 告警 {PLAN_HEX_LOWER} 期望 一").as_str(),
            "恢复资源恢复",
            format!("恢复资源恢复 {PLAN_HEX_LOWER} 期望").as_str(),
            format!("恢复 资源恢复 {PLAN_HEX_LOWER} 期望 一").as_str(),
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

    #[test]
    fn english_w32g_layer_forms_parse() {
        for sentence in [
            "inspect task group a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "CHECK TASK GROUP A1B2C3D4E5F60718293A4B5C6D7E8F90",
            "show task group a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "get task group a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "status task group a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "task group status a1b2c3d4e5f60718293a4b5c6d7e8f90",
        ] {
            assert_eq!(
                parse_nl_command(sentence).unwrap(),
                ControlCommand::InspectTaskGroup {
                    group_id: plan_id()
                },
                "sentence: {sentence:?}"
            );
        }
    }

    #[test]
    fn english_w32g_two_id_and_generation_forms_parse() {
        let node_hex = "31".repeat(16);
        assert_eq!(
            parse_nl_command(&format!(
                "inspect task node a1b2c3d4e5f60718293a4b5c6d7e8f90 {node_hex}"
            ))
            .unwrap(),
            ControlCommand::InspectTaskNode {
                plan_id: plan_id(),
                node_id: [0x31; 16],
            }
        );
        assert_eq!(
            parse_nl_command(&format!(
                "task node status a1b2c3d4e5f60718293a4b5c6d7e8f90 {node_hex}"
            ))
            .unwrap(),
            ControlCommand::InspectTaskNode {
                plan_id: plan_id(),
                node_id: [0x31; 16],
            }
        );
        assert_eq!(
            parse_nl_command("inspect fiber a1b2c3d4e5f60718293a4b5c6d7e8f90 generation 2")
                .unwrap(),
            ControlCommand::InspectExecutionFiber {
                fiber_id: plan_id(),
                generation: 2,
            }
        );
        assert_eq!(
            parse_nl_command("fiber status a1b2c3d4e5f60718293a4b5c6d7e8f90 generation 3").unwrap(),
            ControlCommand::InspectExecutionFiber {
                fiber_id: plan_id(),
                generation: 3,
            }
        );
        assert_eq!(
            parse_nl_command("inspect topic a1b2c3d4e5f60718293a4b5c6d7e8f90").unwrap(),
            ControlCommand::InspectTopic {
                topic_id: plan_id()
            }
        );
        assert_eq!(
            parse_nl_command("topic status a1b2c3d4e5f60718293a4b5c6d7e8f90").unwrap(),
            ControlCommand::InspectTopic {
                topic_id: plan_id()
            }
        );
        assert_eq!(
            parse_nl_command("inspect operation a1b2c3d4e5f60718293a4b5c6d7e8f90 generation 4")
                .unwrap(),
            ControlCommand::InspectOperation {
                operation_id: plan_id(),
                generation: 4,
            }
        );
        assert_eq!(
            parse_nl_command("operation status a1b2c3d4e5f60718293a4b5c6d7e8f90 generation 5")
                .unwrap(),
            ControlCommand::InspectOperation {
                operation_id: plan_id(),
                generation: 5,
            }
        );
    }

    #[test]
    fn chinese_w32g_layer_forms_parse() {
        assert_eq!(
            parse_nl_command("查看任务组 a1b2c3d4e5f60718293a4b5c6d7e8f90").unwrap(),
            ControlCommand::InspectTaskGroup {
                group_id: plan_id()
            }
        );
        assert_eq!(
            parse_nl_command("查看 任务组 a1b2c3d4e5f60718293a4b5c6d7e8f90").unwrap(),
            ControlCommand::InspectTaskGroup {
                group_id: plan_id()
            }
        );
        assert_eq!(
            parse_nl_command("任务组状态 a1b2c3d4e5f60718293a4b5c6d7e8f90").unwrap(),
            ControlCommand::InspectTaskGroup {
                group_id: plan_id()
            }
        );
        assert_eq!(
            parse_nl_command(
                "查看任务节点 a1b2c3d4e5f60718293a4b5c6d7e8f90 31313131313131313131313131313131"
            )
            .unwrap(),
            ControlCommand::InspectTaskNode {
                plan_id: plan_id(),
                node_id: [0x31; 16],
            }
        );
        assert_eq!(
            parse_nl_command(
                "查看 任务节点 a1b2c3d4e5f60718293a4b5c6d7e8f90 31313131313131313131313131313131"
            )
            .unwrap(),
            ControlCommand::InspectTaskNode {
                plan_id: plan_id(),
                node_id: [0x31; 16],
            }
        );
        assert_eq!(
            parse_nl_command("查看纤程 a1b2c3d4e5f60718293a4b5c6d7e8f90 世代 2").unwrap(),
            ControlCommand::InspectExecutionFiber {
                fiber_id: plan_id(),
                generation: 2,
            }
        );
        assert_eq!(
            parse_nl_command("查看 纤程 a1b2c3d4e5f60718293a4b5c6d7e8f90 世代 2").unwrap(),
            ControlCommand::InspectExecutionFiber {
                fiber_id: plan_id(),
                generation: 2,
            }
        );
        assert_eq!(
            parse_nl_command("查看主题 a1b2c3d4e5f60718293a4b5c6d7e8f90").unwrap(),
            ControlCommand::InspectTopic {
                topic_id: plan_id()
            }
        );
        assert_eq!(
            parse_nl_command("主题状态 a1b2c3d4e5f60718293a4b5c6d7e8f90").unwrap(),
            ControlCommand::InspectTopic {
                topic_id: plan_id()
            }
        );
        assert_eq!(
            parse_nl_command("查看操作 a1b2c3d4e5f60718293a4b5c6d7e8f90 世代 4").unwrap(),
            ControlCommand::InspectOperation {
                operation_id: plan_id(),
                generation: 4,
            }
        );
        assert_eq!(
            parse_nl_command("操作状态 a1b2c3d4e5f60718293a4b5c6d7e8f90 世代 4").unwrap(),
            ControlCommand::InspectOperation {
                operation_id: plan_id(),
                generation: 4,
            }
        );
    }

    #[test]
    fn w32g_layer_forms_reject_bad_arity_and_generation() {
        for input in [
            "inspect task group",
            "inspect task group now",
            "inspect task node a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "inspect fiber a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "inspect fiber a1b2c3d4e5f60718293a4b5c6d7e8f90 generation",
            "inspect fiber a1b2c3d4e5f60718293a4b5c6d7e8f90 generation 0",
            "inspect fiber a1b2c3d4e5f60718293a4b5c6d7e8f90 generation -1",
            "inspect topic",
            "inspect operation a1b2c3d4e5f60718293a4b5c6d7e8f90 generation 0",
            "查看任务组",
            "查看纤程 a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "查看纤程 a1b2c3d4e5f60718293a4b5c6d7e8f90 世代 0",
            "查看主题",
            "查看操作 a1b2c3d4e5f60718293a4b5c6d7e8f90 世代 0",
        ] {
            assert!(
                matches!(
                    parse_nl_command(input),
                    Err(ControlError::InvalidCommand(_))
                ),
                "sentence: {input:?}"
            );
        }
    }

    #[test]
    fn w32g_layer_forms_leave_existing_task_grammar_untouched() {
        assert_eq!(
            parse_nl_command("inspect task a1b2c3d4e5f60718293a4b5c6d7e8f90").unwrap(),
            ControlCommand::InspectTask { plan_id: plan_id() }
        );
        assert!(matches!(
            parse_nl_command("task status now"),
            Err(ControlError::InvalidCommand(_))
        ));
    }
}
