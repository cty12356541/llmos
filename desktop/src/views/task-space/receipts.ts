// Task Space(W33-F)的 Receipt 渲染层:SABI Receipt 只读展示,与主壳
// (desktop/src/main.ts)同形。对 OutcomeDto 全形态穷尽匹配——特别覆盖
// W32-G 五层 inspect 形态(task_group/task_node/execution_fiber/topic/
// durable_operation inspected,types.ts 既有导出):它们是 Task Space
// 详情面的数据丰富度来源;本壳命令层尚未接线其派发(见 gap 登记),
// 渲染形态先行就绪,接线落地即直显。

import type { OutcomeDto, ReceiptDto } from "../../types";
import { el, fieldRow } from "./dom";

interface AlertShape {
  planIdHex: string;
  totalFailures: number;
  acknowledgedReceiptIdHex: string | null;
}

function alertsTable(alerts: readonly AlertShape[]): HTMLElement {
  const table = el("table", { className: "data-table" });
  const header = el("tr");
  header.append(
    el("th", { text: "plan_id" }),
    el("th", { text: "total_failures" }),
    el("th", { text: "acknowledged_receipt" }),
  );
  const head = el("thead");
  head.append(header);
  const body = el("tbody");
  if (alerts.length === 0) {
    const row = el("tr");
    const cell = el("td", { text: "(无告警)" });
    cell.colSpan = 3;
    row.append(cell);
    body.append(row);
  }
  for (const alert of alerts) {
    const row = el("tr");
    row.append(
      el("td", { text: alert.planIdHex }),
      el("td", { text: String(alert.totalFailures) }),
      el("td", { text: alert.acknowledgedReceiptIdHex ?? "—" }),
    );
    body.append(row);
  }
  table.append(head, body);
  return table;
}

/** 类型化失败与读侧巡检的视觉区分(红边);mutation 成功不出现在本视图。 */
function failureCard(code: string, retry: string, safeMessage: string): HTMLElement {
  const card = el("section", { className: "card error" });
  card.append(el("h3", { text: "类型化失败(SabiFailure)" }));
  card.append(
    fieldRow("code", code),
    fieldRow("retry", retry),
    fieldRow("safe_message", safeMessage),
  );
  return card;
}

/** 只读 outcome 卡片(穷尽,无通配臂;mutation 形态显式拒绝——本视图无写路径)。 */
export function renderOutcome(outcome: OutcomeDto): HTMLElement {
  const card = el("section", { className: "card outcome" });
  switch (outcome.kind) {
    case "inspected": {
      card.append(el("h3", { text: "恢复巡检(artifact 域,单计划过滤)" }));
      card.append(
        fieldRow("worker_state", outcome.workerState),
        fieldRow("completed_cycles", String(outcome.completedCycles)),
        fieldRow("durable_retrying", String(outcome.durableRetrying)),
        fieldRow("durable_escalated", String(outcome.durableEscalated)),
        fieldRow("durable_unacknowledged_escalated", String(outcome.durableUnacknowledgedEscalated)),
        fieldRow("durable_resolved", String(outcome.durableResolved)),
      );
      card.append(el("h4", { text: "本计划告警行" }));
      card.append(alertsTable(outcome.alerts));
      break;
    }
    case "semantic_inspected": {
      card.append(el("h3", { text: "语义恢复巡检(semantic 域)" }));
      card.append(
        fieldRow("total_inspected", String(outcome.totalInspected)),
        fieldRow("total_finalized", String(outcome.totalFinalized)),
        fieldRow("consecutive_failed_cycles", String(outcome.consecutiveFailedCycles)),
        fieldRow("domain_faulted", String(outcome.domainFaulted)),
        fieldRow("durable_retrying", String(outcome.durableRetrying)),
        fieldRow("durable_escalated", String(outcome.durableEscalated)),
        fieldRow("durable_unacknowledged_escalated", String(outcome.durableUnacknowledgedEscalated)),
        fieldRow("durable_resolved", String(outcome.durableResolved)),
      );
      card.append(el("h4", { text: "escalated 语义告警" }));
      card.append(alertsTable(outcome.alerts));
      break;
    }
    case "resource_recovery_inspected": {
      card.append(el("h3", { text: "资源恢复巡检(resource 域)" }));
      card.append(
        fieldRow("total_inspected", String(outcome.totalInspected)),
        fieldRow("total_finalized", String(outcome.totalFinalized)),
        fieldRow("consecutive_failed_cycles", String(outcome.consecutiveFailedCycles)),
        fieldRow("domain_faulted", String(outcome.domainFaulted)),
        fieldRow("durable_retrying", String(outcome.durableRetrying)),
        fieldRow("durable_escalated", String(outcome.durableEscalated)),
        fieldRow("durable_unacknowledged_escalated", String(outcome.durableUnacknowledgedEscalated)),
        fieldRow("durable_resolved", String(outcome.durableResolved)),
      );
      card.append(el("h4", { text: "escalated 资源告警" }));
      card.append(alertsTable(outcome.alerts));
      break;
    }
    case "process_inspected": {
      card.append(el("h3", { text: "进程绑定快照(关联实体)" }));
      card.append(
        fieldRow("process_id", outcome.processIdHex),
        fieldRow("process_generation", String(outcome.processGeneration)),
        fieldRow("agent_instance_id", outcome.agentInstanceIdHex),
        fieldRow("task_id", outcome.taskIdHex),
        fieldRow("task_attempt_id", outcome.taskAttemptIdHex),
        fieldRow("isolation_domain_id", outcome.isolationDomainIdHex),
      );
      break;
    }
    case "resource_inspected": {
      card.append(el("h3", { text: "资源预留成本快照(关联实体)" }));
      card.append(
        fieldRow("reservation_id", outcome.reservationIdHex),
        fieldRow("account_id", outcome.accountIdHex),
        fieldRow("upper_bound(预留预算上限)", String(outcome.upperBound)),
        fieldRow("usage_high_water(结转用量)", String(outcome.usageHighWater)),
        fieldRow("consumption_count(消费回执数)", String(outcome.consumptionCount)),
        fieldRow(
          "结余(派生 = upper_bound − usage_high_water)",
          String(outcome.upperBound - outcome.usageHighWater),
        ),
      );
      break;
    }
    case "task_group_inspected": {
      card.append(el("h3", { text: "TaskGroup 层 inspect(W32-G,SABI v1.5)" }));
      card.append(
        fieldRow("group_id", outcome.groupIdHex),
        fieldRow("task_id", outcome.taskIdHex),
        fieldRow("parent_group_id", outcome.parentGroupIdHex ?? "(根组)"),
        fieldRow("state", outcome.state),
        fieldRow("membership_generation", String(outcome.membershipGeneration)),
        fieldRow("state_seq", String(outcome.stateSeq)),
        fieldRow("depth", String(outcome.depth)),
        fieldRow("cancel_epoch", String(outcome.cancelEpoch)),
        fieldRow("created_at_ms", String(outcome.createdAtMs)),
        fieldRow("updated_at_ms", String(outcome.updatedAtMs)),
        fieldRow("member_count", String(outcome.memberCount)),
        fieldRow("members_truncated", String(outcome.membersTruncated)),
      );
      break;
    }
    case "task_node_inspected": {
      card.append(el("h3", { text: "计划节点 TaskNode inspect(W32-G,SABI v1.5)" }));
      card.append(
        fieldRow("plan_id", outcome.planIdHex),
        fieldRow("node_id", outcome.nodeIdHex),
        fieldRow("node_kind", outcome.nodeKind),
        fieldRow("state", outcome.state),
        fieldRow("declared_revision(计划 revision 引用)", String(outcome.declaredRevision)),
        fieldRow("node_digest", outcome.nodeDigestHex),
        fieldRow("transition_count", String(outcome.transitionCount)),
        fieldRow("residency_tier", outcome.residencyTier),
        fieldRow("residency_transition_count", String(outcome.residencyTransitionCount)),
        fieldRow("first_declared_at_ms", String(outcome.firstDeclaredAtMs)),
        fieldRow("updated_at_ms", String(outcome.updatedAtMs)),
      );
      break;
    }
    case "execution_fiber_inspected": {
      card.append(el("h3", { text: "执行纤程 ExecutionFiber inspect(W32-G,SABI v1.5)" }));
      card.append(
        fieldRow("fiber_id", outcome.fiberIdHex),
        fieldRow("generation", String(outcome.generation)),
        fieldRow("state", outcome.state),
        fieldRow("lifecycle_phase", outcome.lifecyclePhase),
        fieldRow("active_cpu_ms", String(outcome.activeCpuMs)),
        fieldRow("elapsed_wall_ms", String(outcome.elapsedWallMs)),
        fieldRow("scheduler_wait_ms", String(outcome.schedulerWaitMs)),
        fieldRow("external_wait_ms", String(outcome.externalWaitMs)),
        fieldRow("backpressure_wait_ms", String(outcome.backpressureWaitMs)),
        fieldRow("suspended_ms", String(outcome.suspendedMs)),
      );
      break;
    }
    case "topic_inspected": {
      card.append(el("h3", { text: "持久主题 Topic inspect(W32-G,SABI v1.5)" }));
      card.append(
        fieldRow("topic_id", outcome.topicIdHex),
        fieldRow("channel_id", outcome.channelIdHex),
        fieldRow("channel_generation", String(outcome.channelGeneration)),
        fieldRow("name(hex)", outcome.nameHex),
        fieldRow("active_subscriptions", String(outcome.activeSubscriptions)),
        fieldRow("policy_digest", outcome.policyDigestHex),
        fieldRow("created_at_ms", String(outcome.createdAtMs)),
      );
      break;
    }
    case "durable_operation_inspected": {
      card.append(el("h3", { text: "持久操作 DurableOperation inspect(W32-G,SABI v1.5)" }));
      card.append(
        fieldRow("operation_id", outcome.operationIdHex),
        fieldRow("generation", String(outcome.generation)),
        fieldRow("state", outcome.state),
        fieldRow("cancel_epoch", String(outcome.cancelEpoch)),
        fieldRow("owner_fiber_id", outcome.ownerFiberIdHex),
        fieldRow("owner_fiber_generation", String(outcome.ownerFiberGeneration)),
        fieldRow("outcome_receipt", outcome.outcomeReceiptIdHex ?? "(未终态)"),
      );
      break;
    }
    case "metrics_exported": {
      card.append(el("h3", { text: "OpenMetrics 导出" }));
      card.append(el("pre", { className: "metrics", text: outcome.openmetricsText }));
      break;
    }
    case "failure": {
      return failureCard(outcome.code, outcome.retry, outcome.safeMessage);
    }
    case "acknowledged":
    case "resumed":
    case "operation_paused":
    case "operation_resumed":
    case "operation_cancelled":
    case "operation_killed":
    case "operation_throttled":
    case "operation_reclaimed": {
      // 本视图只读:Task Space 不派发任何控制动作,mutation 回执不应出现;
      // 穷尽臂显式拒绝而不是静默渲染,保住「零写入」边界可见性。
      card.append(
        el("h3", { text: "mutation 回执(只读视图拒绝渲染)" }),
        el("p", { className: "muted", text: `outcome=${outcome.kind}——Task Space 无写路径;该形态不应到达本视图。` }),
      );
      break;
    }
  }
  return card;
}

/** 回执页脚:命令身份 + transport 关联 + receipt 字节 hex(CLI `RECEIPT` 同一契约)。 */
export function receiptFooter(receipt: ReceiptDto): HTMLElement {
  const footer = el("section", { className: "card receipt-footer" });
  footer.append(
    fieldRow("control_command_id", receipt.controlCommandIdHex),
    fieldRow("correlation_id", receipt.correlationIdHex),
  );
  const receiptRow = fieldRow("receipt (to_bytes hex)", receipt.receiptHex);
  receiptRow.classList.add("mono");
  footer.append(receiptRow);
  return footer;
}

export function renderReceipt(receipt: ReceiptDto): HTMLElement {
  const wrap = el("div");
  wrap.append(renderOutcome(receipt.outcome));
  wrap.append(receiptFooter(receipt));
  return wrap;
}
