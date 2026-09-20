//! 前端 DTO:后端把 `ControlReceipt` 投影为可序列化的 JSON 形态。
//! `receipt_hex` 恒为 [`nlos_system_control::control::receipt_to_hex`] 的输出,
//! 与 CLI 的 `RECEIPT <hex>` 行共用同一等价契约(B-TASK-006L 三入口字节一致)。

use nlos_schema::sabi::v1::{RetryDirective, SabiErrorCode, SabiFailure};
use nlos_system_control::control::{
    ControlOutcome, ControlReceipt, ProcessInspection, RecoveryInspection, ResourceInspection,
    ResourceRecoveryInspection, SemanticRecoveryInspection, receipt_to_hex,
};

/// 一条 escalated 告警(artifact/semantic 共用形态)。
#[derive(Clone, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryAlertDto {
    pub plan_id_hex: String,
    pub total_failures: u64,
    pub acknowledged_receipt_id_hex: Option<String>,
}

/// 一条派发命令的回执结果:只读巡检形态 + W32-B 写入形态 + 类型化失败。
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
    ResourceRecoveryInspected {
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
    /// W32-B 写入半:mutation 回执的第一类形态——各携带权威 receipt 引用
    /// (§24.3),由「控制动作」视图按动作类型化渲染。
    Acknowledged {
        receipt_id_hex: String,
    },
    Resumed {
        receipt_id_hex: String,
    },
    OperationPaused {
        receipt_id_hex: String,
    },
    OperationResumed {
        receipt_id_hex: String,
    },
    OperationCancelled {
        receipt_id_hex: String,
    },
    OperationKilled {
        receipt_id_hex: String,
    },
    OperationThrottled {
        receipt_id_hex: String,
    },
    OperationReclaimed {
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
    pub resource_root: Option<String>,
    pub source: ConfigSourceDto,
    pub platform_supported: bool,
}

/// W32-D 控制面授权事实:客户端路径常量(非 inspect 数据)。
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ControlPlaneFactsDto {
    pub service: String,
    pub capability_slot: u64,
    pub capability_generation: u64,
}

/// W32-D 一致性自检的一行事实比对(渲染值 vs 复检值)。
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FactRowDto {
    pub field: String,
    pub first: String,
    pub second: String,
    pub matched: bool,
}

/// W32-D 一致性自检结果:同一 reservation 两次独立认证派发的有界成本
/// 事实逐字段比对 + receipt hex 比对。
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FactCheckDto {
    pub reservation_id_hex: String,
    pub matched: bool,
    pub receipt_hex_matched: bool,
    pub first_receipt_hex: String,
    pub second_receipt_hex: String,
    pub rows: Vec<FactRowDto>,
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

fn resource_recovery_alerts(inspection: &ResourceRecoveryInspection) -> Vec<RecoveryAlertDto> {
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

fn resource_recovery_inspected_dto(inspection: &ResourceRecoveryInspection) -> OutcomeDto {
    OutcomeDto::ResourceRecoveryInspected {
        total_inspected: inspection.total_inspected,
        total_finalized: inspection.total_finalized,
        consecutive_failed_cycles: inspection.consecutive_failed_cycles,
        domain_faulted: inspection.domain_faulted,
        durable_retrying: inspection.durable_retrying,
        durable_escalated: inspection.durable_escalated,
        durable_unacknowledged_escalated: inspection.durable_unacknowledged_escalated,
        durable_resolved: inspection.durable_resolved,
        alerts: resource_recovery_alerts(inspection),
    }
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

/// [`ControlReceipt`] → [`ReceiptDto`] 的单一投影点(穷尽匹配,无通配臂:
/// 新的 outcome 变体必须落成显式 DTO 形态,不得静默丢进失败)。
#[must_use]
pub fn receipt_dto(receipt: &ControlReceipt) -> ReceiptDto {
    let outcome = match receipt.outcome.as_ref() {
        Ok(ControlOutcome::Inspected(inspection)) => inspected_dto(inspection),
        Ok(ControlOutcome::SemanticInspected(inspection)) => semantic_inspected_dto(inspection),
        Ok(ControlOutcome::ResourceRecoveryInspected(inspection)) => {
            resource_recovery_inspected_dto(inspection)
        }
        Ok(ControlOutcome::ProcessInspected(inspection)) => process_inspected_dto(inspection),
        Ok(ControlOutcome::ResourceInspected(inspection)) => resource_inspected_dto(inspection),
        Ok(ControlOutcome::MetricsExported(export)) => OutcomeDto::MetricsExported {
            openmetrics_text: export.openmetrics_text.clone(),
        },
        Ok(ControlOutcome::Acknowledged { receipt_id }) => OutcomeDto::Acknowledged {
            receipt_id_hex: hex(receipt_id),
        },
        Ok(ControlOutcome::Resumed { receipt_id }) => OutcomeDto::Resumed {
            receipt_id_hex: hex(receipt_id),
        },
        Ok(ControlOutcome::OperationPaused { receipt_id }) => OutcomeDto::OperationPaused {
            receipt_id_hex: hex(receipt_id),
        },
        Ok(ControlOutcome::OperationResumed { receipt_id }) => OutcomeDto::OperationResumed {
            receipt_id_hex: hex(receipt_id),
        },
        Ok(ControlOutcome::OperationCancelled { receipt_id }) => OutcomeDto::OperationCancelled {
            receipt_id_hex: hex(receipt_id),
        },
        Ok(ControlOutcome::OperationKilled { receipt_id }) => OutcomeDto::OperationKilled {
            receipt_id_hex: hex(receipt_id),
        },
        Ok(ControlOutcome::OperationThrottled { receipt_id }) => OutcomeDto::OperationThrottled {
            receipt_id_hex: hex(receipt_id),
        },
        Ok(ControlOutcome::OperationReclaimed { receipt_id }) => OutcomeDto::OperationReclaimed {
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

fn fact_row(field: &str, first: &str, second: &str) -> FactRowDto {
    FactRowDto {
        field: field.to_owned(),
        first: first.to_owned(),
        second: second.to_owned(),
        matched: first == second,
    }
}

/// 非 ResourceInspected 形态(如未接线的类型化失败)的一行摘要标签:
/// 保留失败码,绝不把失败伪装成事实。
fn outcome_label(outcome: &OutcomeDto) -> String {
    match outcome {
        OutcomeDto::ResourceInspected { .. } => "resource_inspected".to_owned(),
        OutcomeDto::Failure { code, .. } => format!("failure/{code}"),
        OutcomeDto::Inspected { .. } => "inspected".to_owned(),
        OutcomeDto::SemanticInspected { .. } => "semantic_inspected".to_owned(),
        OutcomeDto::ResourceRecoveryInspected { .. } => "resource_recovery_inspected".to_owned(),
        OutcomeDto::ProcessInspected { .. } => "process_inspected".to_owned(),
        OutcomeDto::MetricsExported { .. } => "metrics_exported".to_owned(),
        OutcomeDto::Acknowledged { .. } => "acknowledged".to_owned(),
        OutcomeDto::Resumed { .. } => "resumed".to_owned(),
        OutcomeDto::OperationPaused { .. } => "operation_paused".to_owned(),
        OutcomeDto::OperationResumed { .. } => "operation_resumed".to_owned(),
        OutcomeDto::OperationCancelled { .. } => "operation_cancelled".to_owned(),
        OutcomeDto::OperationKilled { .. } => "operation_killed".to_owned(),
        OutcomeDto::OperationThrottled { .. } => "operation_throttled".to_owned(),
        OutcomeDto::OperationReclaimed { .. } => "operation_reclaimed".to_owned(),
    }
}

/// 两次独立派发的回执 → [`FactCheckDto`](渲染事实 vs 直接复检)。
/// 双 ResourceInspected 时逐字段比较五个有界事实;否则比较形态标签
/// (未接线形态两侧同为 `failure/NOT_FOUND` 亦如实可比)。
#[must_use]
pub fn fact_check_dto(
    reservation_id_hex: &str,
    first: &ReceiptDto,
    second: &ReceiptDto,
) -> FactCheckDto {
    let rows = match (&first.outcome, &second.outcome) {
        (
            OutcomeDto::ResourceInspected {
                reservation_id_hex: first_id,
                account_id_hex: first_account,
                upper_bound: first_bound,
                usage_high_water: first_water,
                consumption_count: first_count,
            },
            OutcomeDto::ResourceInspected {
                reservation_id_hex: second_id,
                account_id_hex: second_account,
                upper_bound: second_bound,
                usage_high_water: second_water,
                consumption_count: second_count,
            },
        ) => vec![
            fact_row("reservation_id", first_id, second_id),
            fact_row("account_id", first_account, second_account),
            fact_row(
                "upper_bound",
                &first_bound.to_string(),
                &second_bound.to_string(),
            ),
            fact_row(
                "usage_high_water",
                &first_water.to_string(),
                &second_water.to_string(),
            ),
            fact_row(
                "consumption_count",
                &first_count.to_string(),
                &second_count.to_string(),
            ),
        ],
        (first_outcome, second_outcome) => {
            vec![fact_row(
                "outcome",
                &outcome_label(first_outcome),
                &outcome_label(second_outcome),
            )]
        }
    };
    let receipt_hex_matched = first.receipt_hex == second.receipt_hex;
    let matched = receipt_hex_matched && rows.iter().all(|row| row.matched);
    FactCheckDto {
        reservation_id_hex: reservation_id_hex.to_owned(),
        matched,
        receipt_hex_matched,
        first_receipt_hex: first.receipt_hex.clone(),
        second_receipt_hex: second.receipt_hex.clone(),
        rows,
    }
}
