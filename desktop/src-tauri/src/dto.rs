//! 前端 DTO:后端把 `ControlReceipt` 投影为可序列化的 JSON 形态。
//! `receipt_hex` 恒为 [`nlos_system_control::control::receipt_to_hex`] 的输出,
//! 与 CLI 的 `RECEIPT <hex>` 行共用同一等价契约(B-TASK-006L 三入口字节一致)。

use nlos_schema::sabi::v1::{RetryDirective, SabiErrorCode, SabiFailure};
use nlos_system_control::control::{
    ControlOutcome, ControlReceipt, ProcessInspection, RecoveryInspection, ResourceInspection,
    SemanticRecoveryInspection, receipt_to_hex,
};

/// 一条 escalated 告警(artifact/semantic 共用形态)。
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryAlertDto {
    pub plan_id_hex: String,
    pub total_failures: u64,
    pub acknowledged_receipt_id_hex: Option<String>,
}

/// 只读命令结果;mutation 结果在 W32-A 不可达,显式单独形态而非伪造失败。
#[derive(Clone, Debug, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OutcomeDto {
    Inspected {
        worker_state: String,
        completed_cycles: u64,
        durable_retrying: u64,
        durable_escalated: u64,
        durable_unacknowledged_escalated: u64,
        durable_resolved: u64,
        alerts: Vec<RecoveryAlertDto>,
    },
    SemanticInspected {
        total_inspected: u64,
        total_finalized: u64,
        consecutive_failed_cycles: u64,
        domain_faulted: bool,
        durable_retrying: u64,
        durable_escalated: u64,
        durable_unacknowledged_escalated: u64,
        durable_resolved: u64,
        alerts: Vec<RecoveryAlertDto>,
    },
    ProcessInspected {
        process_id_hex: String,
        process_generation: u64,
        agent_instance_id_hex: String,
        task_id_hex: String,
        task_attempt_id_hex: String,
        isolation_domain_id_hex: String,
    },
    ResourceInspected {
        reservation_id_hex: String,
        account_id_hex: String,
        upper_bound: u64,
        usage_high_water: u64,
        consumption_count: u32,
    },
    MetricsExported {
        openmetrics_text: String,
    },
    Failure {
        code: String,
        retry: String,
        safe_message: String,
    },
    UnexpectedMutation {
        receipt_id_hex: String,
    },
}

/// 一条派发命令的 SABI Receipt 投影。
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReceiptDto {
    pub control_command_id_hex: String,
    pub correlation_id_hex: String,
    pub receipt_hex: String,
    pub outcome: OutcomeDto,
}

/// 会话连接配置(环境变量或 GUI 会话内设置)。
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigDto {
    pub socket_path: Option<String>,
    pub principal_hex: Option<String>,
    pub key_file: Option<String>,
    pub cli_socket: Option<String>,
    pub cli_path: Option<String>,
    pub source: ConfigSourceDto,
    pub platform_supported: bool,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigSourceDto {
    Env,
    Session,
    #[default]
    Unset,
}

/// 一致性自检结果:GUI(认证入口)与真实 CLI(plain 入口)的 receipt hex 比对。
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParityDto {
    pub operation: String,
    pub gui_receipt_hex: String,
    pub cli_receipt_hex: Option<String>,
    pub cli_exit_code: Option<i32>,
    pub matched: bool,
    pub cli_stderr: Option<String>,
}

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn enum_name(pending: i32) -> String {
    // 数值未知时保留数值本身,绝不静默丢失错误面。
    SabiErrorCode::try_from(pending)
        .map(|code| code.as_str_name().to_owned())
        .unwrap_or_else(|_| format!("UNKNOWN({pending})"))
}

fn retry_name(pending: i32) -> String {
    RetryDirective::try_from(pending)
        .map(|retry| retry.as_str_name().to_owned())
        .unwrap_or_else(|_| format!("UNKNOWN({pending})"))
}

fn failure_dto(failure: &SabiFailure) -> OutcomeDto {
    OutcomeDto::Failure {
        code: enum_name(failure.code),
        retry: retry_name(failure.retry),
        safe_message: failure.safe_message.clone(),
    }
}

fn artifact_alerts(inspection: &RecoveryInspection) -> Vec<RecoveryAlertDto> {
    inspection
        .alerts
        .iter()
        .map(|alert| RecoveryAlertDto {
            plan_id_hex: hex(&alert.plan_id),
            total_failures: alert.total_failures,
            acknowledged_receipt_id_hex: alert.acknowledged_receipt_id.clone().map(|id| hex(&id)),
        })
        .collect()
}

fn semantic_alerts(inspection: &SemanticRecoveryInspection) -> Vec<RecoveryAlertDto> {
    inspection
        .alerts
        .iter()
        .map(|alert| RecoveryAlertDto {
            plan_id_hex: hex(&alert.plan_id),
            total_failures: alert.total_failures,
            acknowledged_receipt_id_hex: alert.acknowledged_receipt_id.clone().map(|id| hex(&id)),
        })
        .collect()
}

fn inspected_dto(inspection: &RecoveryInspection) -> OutcomeDto {
    OutcomeDto::Inspected {
        worker_state: format!("{:?}", inspection.worker_state),
        completed_cycles: inspection.completed_cycles,
        durable_retrying: inspection.durable_retrying,
        durable_escalated: inspection.durable_escalated,
        durable_unacknowledged_escalated: inspection.durable_unacknowledged_escalated,
        durable_resolved: inspection.durable_resolved,
        alerts: artifact_alerts(inspection),
    }
}

fn semantic_inspected_dto(inspection: &SemanticRecoveryInspection) -> OutcomeDto {
    OutcomeDto::SemanticInspected {
        total_inspected: inspection.total_inspected,
        total_finalized: inspection.total_finalized,
        consecutive_failed_cycles: inspection.consecutive_failed_cycles,
        domain_faulted: inspection.domain_faulted,
        durable_retrying: inspection.durable_retrying,
        durable_escalated: inspection.durable_escalated,
        durable_unacknowledged_escalated: inspection.durable_unacknowledged_escalated,
        durable_resolved: inspection.durable_resolved,
        alerts: semantic_alerts(inspection),
    }
}

fn process_inspected_dto(inspection: &ProcessInspection) -> OutcomeDto {
    OutcomeDto::ProcessInspected {
        process_id_hex: hex(&inspection.process_id),
        process_generation: inspection.process_generation,
        agent_instance_id_hex: hex(&inspection.agent_instance_id),
        task_id_hex: hex(&inspection.task_id),
        task_attempt_id_hex: hex(&inspection.task_attempt_id),
        isolation_domain_id_hex: hex(&inspection.isolation_domain_id),
    }
}

fn resource_inspected_dto(inspection: &ResourceInspection) -> OutcomeDto {
    OutcomeDto::ResourceInspected {
        reservation_id_hex: hex(&inspection.reservation_id),
        account_id_hex: hex(&inspection.account_id),
        upper_bound: inspection.upper_bound,
        usage_high_water: inspection.usage_high_water,
        consumption_count: inspection.consumption_count,
    }
}

/// [`ControlReceipt`] → [`ReceiptDto`] 的单一投影点。
#[must_use]
pub fn receipt_dto(receipt: &ControlReceipt) -> ReceiptDto {
    let outcome = match receipt.outcome.as_ref() {
        Ok(ControlOutcome::Inspected(inspection)) => inspected_dto(inspection),
        Ok(ControlOutcome::SemanticInspected(inspection)) => semantic_inspected_dto(inspection),
        Ok(ControlOutcome::ProcessInspected(inspection)) => process_inspected_dto(inspection),
        Ok(ControlOutcome::ResourceInspected(inspection)) => resource_inspected_dto(inspection),
        Ok(ControlOutcome::MetricsExported(export)) => OutcomeDto::MetricsExported {
            openmetrics_text: export.openmetrics_text.clone(),
        },
        // W32-A 只派发读命令;mutation 回执不可达,但类型必须穷尽且不得
        // 伪造失败——单独形态透出。
        Ok(ControlOutcome::Acknowledged { receipt_id })
        | Ok(ControlOutcome::Resumed { receipt_id })
        | Ok(ControlOutcome::OperationPaused { receipt_id })
        | Ok(ControlOutcome::OperationResumed { receipt_id })
        | Ok(ControlOutcome::OperationCancelled { receipt_id }) => OutcomeDto::UnexpectedMutation {
            receipt_id_hex: hex(receipt_id),
        },
        Err(failure) => failure_dto(failure),
    };
    ReceiptDto {
        control_command_id_hex: hex(&receipt.control_command_id),
        correlation_id_hex: hex(&receipt.correlation_id),
        receipt_hex: receipt_to_hex(receipt),
        outcome,
    }
}
