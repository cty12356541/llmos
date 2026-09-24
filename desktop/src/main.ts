import "./style.css";

import {
  controlPlaneFacts,
  costFactCheck,
  exportMetrics,
  exportResourceMetrics,
  exportSemanticMetrics,
  getConfig,
  inspectHealth,
  inspectProcess,
  inspectResource,
  inspectResourceCost,
  inspectResourceHealth,
  inspectSemanticHealth,
  inspectTask,
  parityCheck,
  parityCheckWrite,
  presentSurfaces,
  setConfig,
  submitControl,
} from "./ipc";
import type { MetricFamily, RecoveryDomainId } from "./openmetrics";
import { familyDomain, parseOpenMetricsText } from "./openmetrics";
import type {
  ConfigDto,
  ControlActionInput,
  FactCheckDto,
  OutcomeDto,
  PresentedSurfaceDto,
  ReceiptDto,
  SurfacesPresentationDto,
} from "./types";
import { MUTATION_OUTCOME_KINDS } from "./types";
import { taskSpaceView } from "./views/task-space";

function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  options?: { className?: string; text?: string },
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  if (options?.className !== undefined) {
    node.className = options.className;
  }
  if (options?.text !== undefined) {
    node.textContent = options.text;
  }
  return node;
}

function fieldRow(label: string, value: string): HTMLElement {
  const row = el("div", { className: "field-row" });
  row.append(el("span", { className: "field-label", text: label }));
  row.append(el("span", { className: "field-value", text: value }));
  return row;
}

interface AlertShape {
  planIdHex: string;
  totalFailures: number;
  acknowledgedReceiptIdHex: string | null;
}

function alertsTable(alerts: AlertShape[]): HTMLElement {
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

const MUTATION_TITLES: Record<string, string> = {
  acknowledged: "告警已确认(acknowledged)",
  resumed: "恢复重试已恢复(resumed)",
  operation_paused: "操作已暂停(operation_paused)",
  operation_resumed: "操作已恢复(operation_resumed)",
  operation_cancelled: "操作已取消(operation_cancelled)",
  operation_killed: "操作已终止(operation_killed)",
  operation_throttled: "操作已限流(operation_throttled)",
  operation_reclaimed: "工作集已回收(operation_reclaimed)",
  application_disabled: "应用已禁用(application_disabled)",
  application_uninstalled: "应用已卸载(application_uninstalled)",
};

function renderOutcome(outcome: OutcomeDto): HTMLElement {
  const card = el("section", { className: "card outcome" });
  switch (outcome.kind) {
    case "inspected": {
      card.append(el("h3", { text: "恢复巡检(artifact 域)" }));
      card.append(
        fieldRow("worker_state", outcome.workerState),
        fieldRow("completed_cycles", String(outcome.completedCycles)),
        fieldRow("durable_retrying", String(outcome.durableRetrying)),
        fieldRow("durable_escalated", String(outcome.durableEscalated)),
        fieldRow(
          "durable_unacknowledged_escalated",
          String(outcome.durableUnacknowledgedEscalated),
        ),
        fieldRow("durable_resolved", String(outcome.durableResolved)),
      );
      card.append(el("h4", { text: "escalated 告警" }));
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
        fieldRow(
          "durable_unacknowledged_escalated",
          String(outcome.durableUnacknowledgedEscalated),
        ),
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
        fieldRow(
          "durable_unacknowledged_escalated",
          String(outcome.durableUnacknowledgedEscalated),
        ),
        fieldRow("durable_resolved", String(outcome.durableResolved)),
      );
      card.append(el("h4", { text: "escalated 资源告警" }));
      card.append(alertsTable(outcome.alerts));
      break;
    }
    case "process_inspected": {
      card.append(el("h3", { text: "进程绑定快照" }));
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
      card.append(el("h3", { text: "资源预留成本快照" }));
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
    case "application_inspected": {
      card.append(el("h3", { text: "Application 层 inspect" }));
      card.append(
        fieldRow("package_id", outcome.packageIdHex),
        fieldRow("application_id", outcome.applicationIdHex),
        fieldRow("package_manifest_digest", outcome.packageManifestDigestHex),
        fieldRow("installation_generation", String(outcome.currentInstallationGeneration)),
        fieldRow("status", String(outcome.status)),
        fieldRow("created_at_ms", String(outcome.createdAtMs)),
        fieldRow("updated_at_ms", String(outcome.updatedAtMs)),
      );
      break;
    }
    case "task_group_inspected":
    case "task_node_inspected":
    case "execution_fiber_inspected":
    case "topic_inspected":
    case "durable_operation_inspected": {
      card.append(el("h3", { text: `层级 inspect(${outcome.kind})` }));
      card.append(
        el("p", {
          className: "muted",
          text: "完整字段渲染见「任务空间」视图;主壳此处仅登记形态可达。",
        }),
      );
      break;
    }
    case "metrics_exported": {
      card.append(el("h3", { text: "OpenMetrics 导出" }));
      const pre = el("pre", { className: "metrics", text: outcome.openmetricsText });
      card.append(pre);
      break;
    }
    case "failure": {
      card.append(el("h3", { text: "类型化失败(SabiFailure)" }));
      card.append(
        fieldRow("code", outcome.code),
        fieldRow("retry", outcome.retry),
        fieldRow("safe_message", outcome.safeMessage),
      );
      break;
    }
    case "acknowledged":
    case "resumed":
    case "operation_paused":
    case "operation_resumed":
    case "operation_cancelled":
    case "operation_killed":
    case "operation_throttled":
    case "operation_reclaimed":
    case "application_disabled":
    case "application_uninstalled": {
      card.append(el("h3", { text: MUTATION_TITLES[outcome.kind] ?? outcome.kind }));
      card.append(fieldRow("receipt_reference", outcome.receiptIdHex));
      break;
    }
  }
  if (outcome.kind === "failure") {
    card.classList.add("error");
  } else if (MUTATION_OUTCOME_KINDS.has(outcome.kind)) {
    card.classList.add("ok");
  }
  return card;
}

function receiptFooter(receipt: ReceiptDto): HTMLElement {
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

function renderReceipt(receipt: ReceiptDto): HTMLElement {
  const wrap = el("div");
  wrap.append(renderOutcome(receipt.outcome));
  wrap.append(receiptFooter(receipt));
  return wrap;
}

function showError(container: HTMLElement, error: unknown): void {
  container.replaceChildren();
  const card = el("section", { className: "card error" });
  card.append(el("h3", { text: "命令失败" }));
  let code = "UNKNOWN";
  let message = String(error);
  if (typeof error === "object" && error !== null && "code" in error && "message" in error) {
    const typed = error as { code?: unknown; message?: unknown };
    if (typeof typed.code === "string") {
      code = typed.code;
    }
    if (typeof typed.message === "string") {
      message = typed.message;
    }
  }
  card.append(fieldRow("code", code), fieldRow("message", message));
  container.append(card);
}

async function runReceiptAction(
  container: HTMLElement,
  action: () => Promise<ReceiptDto>,
): Promise<void> {
  container.replaceChildren(el("p", { className: "muted", text: "派发中……" }));
  try {
    const receipt = await action();
    container.replaceChildren(renderReceipt(receipt));
  } catch (error) {
    showError(container, error);
  }
}

function labeledInput(
  label: string,
  placeholder: string,
  value: string,
): { row: HTMLElement; input: HTMLInputElement } {
  const row = el("div", { className: "field-row" });
  row.append(el("span", { className: "field-label", text: label }));
  const input = el("input");
  input.type = "text";
  input.placeholder = placeholder;
  input.value = value;
  input.spellcheck = false;
  row.append(input);
  return { row, input };
}

interface TargetQueryView {
  label: string;
  placeholder: string;
  button: string;
  dispatch: (id: string) => Promise<ReceiptDto>;
}

function targetQueryView(view: TargetQueryView): HTMLElement {
  const panel = el("section", { className: "card" });
  panel.append(el("h3", { text: view.label }));
  const { row, input } = labeledInput(view.label, view.placeholder, "");
  panel.append(row);
  const hint = el("p", {
    className: "muted",
    text: "16 字节标识,32 个 hex 字符;后端经认证入口派发同一 get 命令。",
  });
  const result = el("div");
  const button = el("button", { text: view.button });
  button.addEventListener("click", () => {
    const id = input.value.trim();
    if (id.length === 0) {
      result.replaceChildren(
        el("p", { className: "muted", text: "请先输入 32 位 hex 标识。" }),
      );
      return;
    }
    void runReceiptAction(result, () => view.dispatch(id));
  });
  panel.append(button, hint, result);
  return panel;
}

const HEX32 = /^[0-9a-fA-F]{32}$/;

function bytesToHex(bytes: Uint8Array): string {
  return Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
}

function freshCommandIdHex(): string {
  const bytes = new Uint8Array(16);
  crypto.getRandomValues(bytes);
  return bytesToHex(bytes);
}

function labeledNumberInput(
  label: string,
  value: string,
  options?: { min?: number; max?: number },
): { row: HTMLElement; input: HTMLInputElement } {
  const { row, input } = labeledInput(label, "", value);
  input.type = "number";
  input.inputMode = "numeric";
  if (options?.min !== undefined) {
    input.min = String(options.min);
  }
  if (options?.max !== undefined) {
    input.max = String(options.max);
  }
  return { row, input };
}

interface RecoveryDomain {
  id: "artifact" | "semantic" | "resource";
  label: string;
  inspect: () => Promise<ReceiptDto>;
  ackAction: string;
  resumeAction: string | null;
}

const RECOVERY_DOMAINS: RecoveryDomain[] = [
  {
    id: "artifact",
    label: "artifact 域",
    inspect: inspectHealth,
    ackAction: "ack-recovery-alert",
    resumeAction: null,
  },
  {
    id: "semantic",
    label: "semantic 域",
    inspect: inspectSemanticHealth,
    ackAction: "ack-semantic-recovery-alert",
    resumeAction: "resume-semantic-recovery",
  },
  {
    id: "resource",
    label: "resource 域(G8)",
    inspect: inspectResourceHealth,
    ackAction: "ack-resource-recovery-alert",
    resumeAction: "resume-resource-recovery",
  },
];

function recoveryAlertsCard(): HTMLElement {
  const panel = el("section", { className: "card" });
  panel.append(el("h3", { text: "恢复告警动作(ack / resume)" }));
  panel.append(
    el("p", {
      className: "muted",
      text: "先巡检读取 escalated 告警(CAS 预期 = 行内 total_failures),再按行下发确认/恢复;每次提交经认证入口携带新的 §25.3 命令身份。",
    }),
  );
  const domainRow = el("div", { className: "field-row" });
  domainRow.append(el("span", { className: "field-label", text: "恢复域" }));
  const domainSelect = el("select");
  for (const domain of RECOVERY_DOMAINS) {
    const option = el("option", { text: domain.label });
    option.value = domain.id;
    domainSelect.append(option);
  }
  domainRow.append(domainSelect);
  panel.append(domainRow);

  const reason = labeledInput("reason(操作理由)", "gui acknowledge/resume", "");
  panel.append(reason.row);

  const load = el("button", { text: "巡检读取告警(经认证 IPC)" });
  const alertsBox = el("div");
  const result = el("div");
  panel.append(load, alertsBox, result);

  const currentDomain = (): RecoveryDomain =>
    RECOVERY_DOMAINS.find((domain) => domain.id === domainSelect.value) ?? RECOVERY_DOMAINS[0];

  const dispatchAction = (
    action: ControlActionInput,
  ): void => {
    const trimmed = reason.input.value.trim();
    if (trimmed.length === 0) {
      result.replaceChildren(
        el("p", { className: "muted", text: "请先填写 reason(控制动作需要非空操作理由)。" }),
      );
      return;
    }
    void runReceiptAction(result, () => submitControl({ ...action, reason: trimmed }));
  };

  load.addEventListener("click", () => {
    const domain = currentDomain();
    alertsBox.replaceChildren(el("p", { className: "muted", text: "巡检中……" }));
    domain
      .inspect()
      .then((receipt) => {
        if (
          receipt.outcome.kind !== "inspected" &&
          receipt.outcome.kind !== "semantic_inspected" &&
          receipt.outcome.kind !== "resource_recovery_inspected"
        ) {
          alertsBox.replaceChildren(renderReceipt(receipt));
          return;
        }
        const alerts = receipt.outcome.alerts;
        if (alerts.length === 0) {
          alertsBox.replaceChildren(
            el("p", { className: "muted", text: `${domain.label}当前无 escalated 告警。` }),
          );
          return;
        }
        const table = el("table", { className: "data-table" });
        const header = el("tr");
        header.append(
          el("th", { text: "plan_id" }),
          el("th", { text: "total_failures(CAS 预期)" }),
          el("th", { text: "acknowledged" }),
          el("th", { text: "动作" }),
        );
        const head = el("thead");
        head.append(header);
        const body = el("tbody");
        for (const alert of alerts) {
          const row = el("tr");
          row.append(
            el("td", { text: alert.planIdHex }),
            el("td", { text: String(alert.totalFailures) }),
            el("td", { text: alert.acknowledgedReceiptIdHex ?? "—" }),
          );
          const actions = el("td");
          const ack = el("button", { text: "确认告警(ack)" });
          ack.addEventListener("click", () => {
            dispatchAction({
              action: domain.ackAction,
              planIdHex: alert.planIdHex,
              expectedTotalFailures: alert.totalFailures,
              reason: "",
            } as ControlActionInput);
          });
          actions.append(ack);
          if (domain.resumeAction !== null) {
            const resume = el("button", { text: "恢复重试(resume)" });
            resume.addEventListener("click", () => {
              dispatchAction({
                action: domain.resumeAction,
                planIdHex: alert.planIdHex,
                expectedTotalFailures: alert.totalFailures,
                reason: "",
              } as ControlActionInput);
            });
            actions.append(resume);
          }
          row.append(actions);
          body.append(row);
        }
        table.append(head, body);
        alertsBox.replaceChildren(table);
      })
      .catch((error: unknown) => showError(alertsBox, error));
  });
  return panel;
}

const OPERATION_ACTIONS = [
  { id: "pause-operation", label: "暂停(pause)", destructive: false },
  { id: "resume-operation", label: "恢复(resume)", destructive: false },
  { id: "cancel-operation", label: "取消(cancel)", destructive: false },
  { id: "kill-operation", label: "终止进程(kill)", destructive: true },
  { id: "throttle-operation", label: "限流(throttle)", destructive: false },
  { id: "reclaim-operation", label: "回收工作集(reclaim)", destructive: false },
] as const;

const CONFIRM_WINDOW_MS = 5000;

function operationControlCard(): HTMLElement {
  const panel = el("section", { className: "card" });
  panel.append(el("h3", { text: "操作控制(pause / resume / cancel / kill / throttle / reclaim)" }));
  panel.append(
    el("p", {
      className: "muted",
      text: "操作目标为 16 字节 id(32 hex);CAS 预期 = 目标当前 generation/revision(进程终止即 process generation,可先经进程查询读取)。kill 为不可逆动作,需两步确认。",
    }),
  );

  const actionRow = el("div", { className: "field-row" });
  actionRow.append(el("span", { className: "field-label", text: "动作" }));
  const actionSelect = el("select");
  for (const action of OPERATION_ACTIONS) {
    const option = el("option", { text: action.label });
    option.value = action.id;
    actionSelect.append(option);
  }
  actionRow.append(actionSelect);
  panel.append(actionRow);

  const target = labeledInput("目标 id(32 hex)", "进程/预留等工作目标 id", "");
  const revision = labeledNumberInput("CAS 预期(generation/revision)", "0", { min: 0 });
  const percent = labeledNumberInput("throttle 百分比(1-100)", "50", { min: 1, max: 100 });
  percent.row.classList.add("hidden");
  const reason = labeledInput("reason(操作理由)", "gui operation control", "");
  panel.append(target.row, revision.row, percent.row, reason.row);

  const result = el("div");
  const confirmHint = el("p", { className: "confirm-hint hidden" });
  confirmHint.textContent = "kill-operation 不可逆:再次点击按钮才真正下发(5 秒内有效)。";
  const dispatch = el("button", { text: "下发(经认证 IPC)" });
  panel.append(dispatch, confirmHint, result);

  const isThrottle = (): boolean => actionSelect.value === "throttle-operation";
  const isKill = (): boolean => actionSelect.value === "kill-operation";
  const refreshFields = (): void => {
    percent.row.classList.toggle("hidden", !isThrottle());
  };
  actionSelect.addEventListener("change", () => {
    refreshFields();
    disarm();
  });

  let armed = false;
  let disarmTimer = 0;
  const disarm = (): void => {
    armed = false;
    window.clearTimeout(disarmTimer);
    dispatch.classList.remove("danger");
    dispatch.textContent = "下发(经认证 IPC)";
    confirmHint.classList.add("hidden");
  };
  const arm = (): void => {
    armed = true;
    dispatch.classList.add("danger");
    dispatch.textContent = "再次点击确认下发 kill";
    confirmHint.classList.remove("hidden");
    disarmTimer = window.setTimeout(disarm, CONFIRM_WINDOW_MS);
  };

  const inputsValid = (): ControlActionInput | string => {
    const targetHex = target.input.value.trim();
    if (!HEX32.test(targetHex)) {
      return "目标 id 必须是 32 个 hex 字符。";
    }
    const expected = Number(revision.input.value);
    if (!Number.isInteger(expected) || expected < 0) {
      return "CAS 预期必须是 ≥0 的整数。";
    }
    const trimmedReason = reason.input.value.trim();
    if (trimmedReason.length === 0) {
      return "请先填写 reason(控制动作需要非空操作理由)。";
    }
    switch (actionSelect.value) {
      case "pause-operation":
      case "resume-operation":
      case "cancel-operation":
      case "kill-operation":
      case "reclaim-operation":
        return {
          action: actionSelect.value,
          targetIdHex: targetHex.toLowerCase(),
          expectedRevision: expected,
          reason: trimmedReason,
        };
      case "throttle-operation": {
        const throttlePercent = Number(percent.input.value);
        if (!Number.isInteger(throttlePercent) || throttlePercent < 1 || throttlePercent > 100) {
          return "throttle 百分比必须是 1..=100 的整数。";
        }
        return {
          action: "throttle-operation",
          targetIdHex: targetHex.toLowerCase(),
          expectedRevision: expected,
          throttlePercent,
          reason: trimmedReason,
        };
      }
      default:
        return "未知动作。";
    }
  };

  dispatch.addEventListener("click", () => {
    if (isKill() && !armed) {
      arm();
      return;
    }
    disarm();
    const action = inputsValid();
    if (typeof action === "string") {
      result.replaceChildren(el("p", { className: "muted", text: action }));
      return;
    }
    void runReceiptAction(result, () => submitControl(action));
  });
  return panel;
}

/** W32-D:无 IPC inspect 面的权威事实缺口登记(本视图不渲染,只声明缺席)。
 * 每一条都对应 crates/ 中的真实权威数据,但控制面没有暴露它的命令/视图。 */
const IPC_SURFACE_GAPS: ReadonlyArray<{ fact: string; detail: string }> = [
  {
    fact: "能力签发/衰减/撤销账本",
    detail: "nlos-capability 的 CapabilityRecord(issuer/holder/rights/target/有效期/衰减深度)与撤销回执——ControlCommand 无对应 arm,proto 无视图",
  },
  {
    fact: "能力调用限额与消耗",
    detail: "call_limit_remaining 与 capability_consumption_rows(剩余额度/消费回执)——无 IPC inspect 面",
  },
  {
    fact: "资源报价事实",
    detail: "QuoteRecord 的 demand_capacity/pricing_version/valid_until——只有 upper_bound 进入有界成本回执投影",
  },
  {
    fact: "预留状态机与多维需求",
    detail: "Reserved/Active/Quarantined/Finalized 状态与 cpu_shares/memory_mib/io_weight 需求——不在 ResourceInspection 五个有界字段内",
  },
  {
    fact: "账户预算余额",
    detail: "resource_accounts 的 initial/available credit——无 inspect 面",
  },
  {
    fact: "结清明细回执",
    detail: "FinalizationReceipt.refund_credit 与逐条 ConsumptionReceipt(只有高水位与条数进入投影)——无 IPC 面",
  },
];

function authorizationFactsCard(config: ConfigDto): HTMLElement {
  const panel = el("section", { className: "card" });
  panel.append(el("h3", { text: "控制面授权事实(本会话可见)" }));
  panel.append(
    fieldRow("会话 principal(ADR-0011 认证身份)", config.principalHex ?? "(未配置)"),
    fieldRow("资源权威接线(resource_root)", config.resourceRoot ?? "(未配置——成本查询保持未接线形态)"),
  );
  const factsBox = el("div");
  controlPlaneFacts()
    .then((facts) => {
      factsBox.append(
        fieldRow("SystemControl 服务", facts.service),
        fieldRow(
          "控制能力句柄(客户端每条派发携带)",
          `slot=${facts.capabilitySlot} generation=${facts.capabilityGeneration}`,
        ),
      );
    })
    .catch((error: unknown) => showError(factsBox, error));
  panel.append(factsBox);
  panel.append(
    el("p", {
      className: "muted",
      text: "以上是客户端路径事实:每条派发信封携带该固定能力句柄,服务端授权检查拒绝时回执为类型化 RIGHTS 失败。逐 principal 的能力签发/衰减/撤销账本无 IPC inspect 面(见下方缺口登记),本视图不渲染任何未暴露的权威数据。",
    }),
  );

  const verify = el("button", { text: "验证控制面授权(真实 dispatch)" });
  const verifyResult = el("div");
  verify.addEventListener("click", () => {
    void runReceiptAction(verifyResult, inspectResourceHealth);
  });
  panel.append(verify, verifyResult);
  return panel;
}

function budgetCostCard(): HTMLElement {
  const panel = el("section", { className: "card" });
  panel.append(el("h3", { text: "预算/成本可见性(InspectResource 有界成本事实)" }));
  panel.append(
    el("p", {
      className: "muted",
      text: "输入 reservation_id(32 hex)经认证入口派发 InspectResource;会话配置 resource_root 指向本地资源权威根目录时,由真实 ResourceAuthorityInspector 组装 upper_bound/usage_high_water/consumption_count 等有界事实;未配置时回执为诚实的类型化 NOT_FOUND(未接线,与 CLI 字节一致),不伪造数据。",
    }),
  );
  const { row, input } = labeledInput("reservation id(32 hex)", "32 hex 字符 reservation_id", "");
  panel.append(row);
  const result = el("div");
  const button = el("button", { text: "查询成本(经认证 IPC)" });
  button.addEventListener("click", () => {
    const id = input.value.trim();
    if (!HEX32.test(id)) {
      result.replaceChildren(
        el("p", { className: "muted", text: "请先输入 32 位 hex 的 reservation_id。" }),
      );
      return;
    }
    void runReceiptAction(result, () => inspectResourceCost(id.toLowerCase()));
  });
  panel.append(button, result);
  return panel;
}

function costFactCheckCard(): HTMLElement {
  const panel = el("section", { className: "card" });
  panel.append(el("h3", { text: "成本事实自检(渲染事实 vs 直接复检)" }));
  panel.append(
    el("p", {
      className: "muted",
      text: "同一 reservation 两次独立经认证入口派发 InspectResource,逐字段比较有界成本事实并比对 receipt hex。已结清(FINALIZED)预留事实不可变,两次必须一致;未接线形态两侧同为类型化 NOT_FOUND,亦如实可比。",
    }),
  );
  const { row, input } = labeledInput("reservation id(32 hex)", "32 hex 字符 reservation_id", "");
  panel.append(row);
  const result = el("div");
  const button = el("button", { text: "运行成本自检" });
  button.addEventListener("click", () => {
    const id = input.value.trim();
    if (!HEX32.test(id)) {
      result.replaceChildren(
        el("p", { className: "muted", text: "请先输入 32 位 hex 的 reservation_id。" }),
      );
      return;
    }
    result.replaceChildren(el("p", { className: "muted", text: "比对中……" }));
    costFactCheck(id.toLowerCase())
      .then((check: FactCheckDto) => {
        result.replaceChildren(renderFactCheck(check));
      })
      .catch((error: unknown) => showError(result, error));
  });
  panel.append(button, result);
  return panel;
}

function renderFactCheck(check: FactCheckDto): HTMLElement {
  const card = el("section", { className: check.matched ? "card ok" : "card error" });
  card.append(
    el("h3", { text: check.matched ? "一致(matched)" : "不一致(mismatch)" }),
    fieldRow("reservation_id", check.reservationIdHex),
    fieldRow("receipt_hex 一致", String(check.receiptHexMatched)),
  );
  const firstRow = fieldRow("第一次派发 receipt", check.firstReceiptHex);
  firstRow.classList.add("mono");
  const secondRow = fieldRow("复检 receipt", check.secondReceiptHex);
  secondRow.classList.add("mono");
  card.append(firstRow, secondRow);
  const table = el("table", { className: "data-table" });
  const header = el("tr");
  header.append(
    el("th", { text: "字段" }),
    el("th", { text: "渲染值" }),
    el("th", { text: "复检值" }),
    el("th", { text: "一致" }),
  );
  const head = el("thead");
  head.append(header);
  const body = el("tbody");
  for (const row of check.rows) {
    const tr = el("tr");
    tr.append(
      el("td", { text: row.field }),
      el("td", { text: row.first }),
      el("td", { text: row.second }),
      el("td", { text: row.matched ? "✓" : "✗" }),
    );
    body.append(tr);
  }
  table.append(head, body);
  card.append(table);
  return card;
}

function gapRegisterCard(): HTMLElement {
  const panel = el("section", { className: "card" });
  panel.append(el("h3", { text: "缺口登记:无 IPC inspect 面的权威事实(本视图不渲染)" }));
  panel.append(
    el("p", {
      className: "muted",
      text: "以下事实存在于本地权威(crate 已持久化),但控制面没有暴露它们的 inspect 命令/视图——按诚实纪律不发明数据,登记待后续车道补 IPC 面(证据 b-gui-001 §W32-D)。",
    }),
  );
  const table = el("table", { className: "data-table" });
  const header = el("tr");
  header.append(el("th", { text: "权威事实" }), el("th", { text: "缺口说明" }));
  const head = el("thead");
  head.append(header);
  const body = el("tbody");
  for (const gap of IPC_SURFACE_GAPS) {
    const tr = el("tr");
    tr.append(el("td", { text: gap.fact }), el("td", { text: gap.detail }));
    body.append(tr);
  }
  table.append(head, body);
  panel.append(table);
  return panel;
}

function permissionView(config: ConfigDto): HTMLElement {
  const wrap = el("div");
  wrap.append(
    authorizationFactsCard(config),
    budgetCostCard(),
    costFactCheckCard(),
    gapRegisterCard(),
  );
  return wrap;
}

/** W32-F:呈现边界登记(声明→呈现最小链之外的面,不发明)。 */
const SURFACE_PRESENTATION_GAPS: ReadonlyArray<{ fact: string; detail: string }> = [
  {
    fact: "载荷内容渲染",
    detail:
      "entry_name 引用的 manifest entry 载荷字节(artifact store 物化内容)未进入呈现——本视图渲染声明元数据 + 内容占位,不伪造内容",
  },
  {
    fact: "表面生命周期管理",
    detail:
      "open/close 终态已由桌面 Rust `window_lifecycle`(REGISTERED→CREATED→PRESENTED↔HIDDEN→CLOSED)落地;本视图仍只有 durable 声明与呈现过滤,未接线 GUI 开合控件、stale 表面隔离执行器",
  },
  {
    fact: "焦点/输入路由与几何",
    detail: "[DUI-WINDOW-001]/[DUI-INPUT-001] 的 focus/input route、accessibility tree、窗口几何/多窗口编排——均未建模",
  },
  {
    fact: "呈现经 ControlCommand 的 IPC 面",
    detail:
      "表面呈现是本地应用权威直读视图(application_root 接线,与 W32-D 成本查询同机制);Surface 域的 SABI ControlCommand 面(register/create/present/…)不在本波次",
  },
];

function surfaceWindowCard(surface: PresentedSurfaceDto, index: number): HTMLElement {
  const window = el("section", { className: "card surface-window" });
  const titleBar = el("div", { className: "surface-titlebar" });
  const badge = el("span", {
    className: `surface-kind ${surface.kind}`,
    text: surface.kind === "window" ? "窗口 window" : "面板 panel",
  });
  titleBar.append(badge, el("strong", { text: surface.title }), el("span", { className: "muted", text: `#${index + 1}` }));
  window.append(titleBar);
  const body = el("div", { className: "surface-body" });
  body.append(
    fieldRow("surface_id", surface.surfaceIdHex),
    fieldRow("kind", surface.kind),
    fieldRow("title", surface.title),
    fieldRow("entry_name(声明的内容引用)", surface.entryName ?? "(无——元数据-only 声明)"),
    fieldRow("registration_key", surface.registrationKeyHex),
    fieldRow("registered_at_ms", String(surface.registeredAtMs)),
  );
  const placeholder = el("p", {
    className: "muted surface-placeholder",
    text: "表面内容占位:本最小链呈现声明的元数据;entry 载荷渲染属后续车道(缺口登记)。",
  });
  body.append(placeholder);
  window.append(body);
  return window;
}

function renderSurfacesPresentation(presentation: SurfacesPresentationDto): HTMLElement {
  const wrap = el("div");
  const summary = el("section", { className: "card" });
  summary.append(el("h3", { text: "应用表面呈现(本地应用权威直读)" }));
  summary.append(
    fieldRow("application_id", presentation.applicationIdHex),
    fieldRow("package_id", presentation.packageIdHex),
    fieldRow("application_generation", String(presentation.applicationGeneration)),
    fieldRow("status", presentation.status),
    fieldRow("package_manifest_digest", presentation.packageManifestDigestHex),
    fieldRow(
      "可呈现表面数",
      String(presentation.presentableSurfaces.length),
    ),
  );
  if (presentation.status !== "installed") {
    summary.append(
      el("p", {
        className: "muted",
        text: `应用状态为 ${presentation.status}:无可呈现表面(非 installed 状态不呈现,如实显示空集)。`,
      }),
    );
  } else if (presentation.presentableSurfaces.length === 0) {
    summary.append(
      el("p", {
        className: "muted",
        text: "当前代际没有已登记的表面声明:该安装未声明 UI Surface(或声明登记在更早代际,stale 不呈现)。",
      }),
    );
  }
  wrap.append(summary);
  presentation.presentableSurfaces.forEach((surface, index) => {
    wrap.append(surfaceWindowCard(surface, index));
  });
  return wrap;
}

function surfaceGapRegisterCard(): HTMLElement {
  const panel = el("section", { className: "card" });
  panel.append(el("h3", { text: "呈现边界登记:声明→呈现最小链之外的面(本视图不发明)" }));
  const table = el("table", { className: "data-table" });
  const header = el("tr");
  header.append(el("th", { text: "缺口" }), el("th", { text: "说明" }));
  const head = el("thead");
  head.append(header);
  const body = el("tbody");
  for (const gap of SURFACE_PRESENTATION_GAPS) {
    const tr = el("tr");
    tr.append(el("td", { text: gap.fact }), el("td", { text: gap.detail }));
    body.append(tr);
  }
  table.append(head, body);
  panel.append(table);
  return panel;
}

function appSurfacesView(): HTMLElement {
  const wrap = el("div");
  const panel = el("section", { className: "card" });
  panel.append(el("h3", { text: "应用表面(声明 → 窗口呈现最小链)" }));
  panel.append(
    el("p", {
      className: "muted",
      text: "输入包 id(32 hex),经本地应用权威(application_root 接线)读回该应用声明的 UI Surface:只呈现 durable 登记的声明事实(surface id/kind/title/entry 引用),当前安装代际的表面以窗口/面板卡呈现;stale 代际与未安装状态如实为空。登记面由应用侧(sample-app-driver 或样板驱动)经公共 ApplicationAuthority API 完成。",
    }),
  );
  const { row, input } = labeledInput("包 id(32 hex)", "32 hex 字符 package_id", "");
  panel.append(row);
  const result = el("div");
  const button = el("button", { text: "呈现(经本地应用权威)" });
  button.addEventListener("click", () => {
    const id = input.value.trim();
    if (!HEX32.test(id)) {
      result.replaceChildren(
        el("p", { className: "muted", text: "请先输入 32 位 hex 的 package_id。" }),
      );
      return;
    }
    result.replaceChildren(el("p", { className: "muted", text: "读回中……" }));
    presentSurfaces(id.toLowerCase())
      .then((presentation) => {
        result.replaceChildren(renderSurfacesPresentation(presentation));
      })
      .catch((error: unknown) => showError(result, error));
  });
  panel.append(button, result);
  wrap.append(panel, surfaceGapRegisterCard());
  return wrap;
}

function controlView(): HTMLElement {
  const wrap = el("div");
  wrap.append(recoveryAlertsCard(), operationControlCard());
  return wrap;
}

function parityView(): HTMLElement {
  const panel = el("section", { className: "card" });
  panel.append(el("h3", { text: "GUI ↔ CLI 一致性自检" }));
  panel.append(
    el("p", {
      className: "muted",
      text: "GUI 经 ADR-0011 认证入口派发一次读命令;同一命令再由真实 system-control-cli 二进制经 plain socket 派发,比对两侧 RECEIPT hex(与 B-TASK-006L 三入口字节一致契约同源)。",
    }),
  );
  const operationRow = el("div", { className: "field-row" });
  operationRow.append(el("span", { className: "field-label", text: "命令" }));
  const select = el("select");
  for (const operation of [
    "inspect-health",
    "inspect-semantic-health",
    "inspect-resource-health",
    "export-metrics",
    "export-semantic-metrics",
    "export-resource-metrics",
    "inspect-task",
    "inspect-process",
    "inspect-resource",
  ]) {
    const option = el("option", { text: operation });
    option.value = operation;
    select.append(option);
  }
  operationRow.append(select);
  panel.append(operationRow);
  const target = labeledInput("目标 id(hex)", "32 hex 字符(仅 inspect-task/process/resource)", "");
  target.row.classList.add("hidden");
  const needsTarget = ["inspect-task", "inspect-process", "inspect-resource"];
  target.row.classList.add("hidden");
  select.addEventListener("change", () => {
    target.row.classList.toggle("hidden", !needsTarget.includes(select.value));
  });
  panel.append(target.row);
  const result = el("div");
  const button = el("button", { text: "运行自检" });
  button.addEventListener("click", () => {
    const operation = select.value;
    const targetHex = target.input.value.trim();
    result.replaceChildren(el("p", { className: "muted", text: "比对中……" }));
    parityCheck(operation, targetHex.length > 0 ? targetHex : null)
      .then((parity) => {
        const card = el("section", { className: parity.matched ? "card ok" : "card error" });
        card.append(
          el("h3", {
            text: parity.matched ? "一致(matched)" : "不一致(mismatch)",
          }),
          fieldRow("operation", parity.operation),
          fieldRow("gui_receipt", parity.guiReceiptHex),
          fieldRow("cli_receipt", parity.cliReceiptHex ?? "(CLI 未产出)"),
          fieldRow("cli_exit_code", parity.cliExitCode === null ? "—" : String(parity.cliExitCode)),
        );
        const stderrRow = fieldRow("cli_stderr", parity.cliStderr ?? "—");
        stderrRow.classList.add("mono");
        card.append(stderrRow);
        result.replaceChildren(card);
      })
      .catch((error: unknown) => showError(result, error));
  });
  panel.append(button, result);
  return panel;
}

function parityWriteCard(): HTMLElement {
  const panel = el("section", { className: "card" });
  panel.append(el("h3", { text: "写路径自检(pause-operation 探针,W32-B)" }));
  panel.append(
    el("p", {
      className: "muted",
      text: "同一条 pause-operation 命令(每次运行生成新 §25.3 命令 id,两侧字节同一):GUI 经认证入口、真实 CLI 经 plain 入口各派发一次,比对 RECEIPT hex。开发夹具未接线操作执行器,两侧同为确定性类型化 NOT_FOUND 失败回执;已接线宿主上第二次派发可能按 idempotency/CAS 纪律回 CONFLICT,mismatch 如实显示。完整三形态矩阵钉死属 W32-C。",
    }),
  );
  const commandId = labeledInput("命令 id(32 hex,每次运行自动新生成)", "32 hex 字符", "");
  commandId.input.readOnly = true;
  const target = labeledInput("目标 id(32 hex)", "41".repeat(16), "41".repeat(16));
  const revision = labeledNumberInput("CAS 预期(revision)", "1", { min: 0 });
  const reason = labeledInput("reason", "desktop-parity-probe", "desktop-parity-probe");
  panel.append(commandId.row, target.row, revision.row, reason.row);
  const result = el("div");
  const button = el("button", { text: "运行写路径自检" });
  button.addEventListener("click", () => {
    const commandIdHex = freshCommandIdHex();
    commandId.input.value = commandIdHex;
    const targetHex = target.input.value.trim();
    const expected = Number(revision.input.value);
    const trimmedReason = reason.input.value.trim();
    if (!HEX32.test(targetHex) || !Number.isInteger(expected) || expected < 0 || trimmedReason.length === 0) {
      result.replaceChildren(
        el("p", { className: "muted", text: "目标 id 需 32 hex、CAS 预期需 ≥0 整数、reason 非空。" }),
      );
      return;
    }
    result.replaceChildren(el("p", { className: "muted", text: "比对中……" }));
    parityCheckWrite(commandIdHex, targetHex.toLowerCase(), expected, trimmedReason)
      .then((parity) => {
        const card = el("section", { className: parity.matched ? "card ok" : "card error" });
        card.append(
          el("h3", {
            text: parity.matched ? "一致(matched)" : "不一致(mismatch)",
          }),
          fieldRow("operation", parity.operation),
          fieldRow("command_id", commandIdHex),
          fieldRow("gui_receipt", parity.guiReceiptHex),
          fieldRow("cli_receipt", parity.cliReceiptHex ?? "(CLI 未产出)"),
          fieldRow(
            "cli_exit_code",
            parity.cliExitCode === null ? "—" : String(parity.cliExitCode),
          ),
        );
        const stderrRow = fieldRow("cli_stderr", parity.cliStderr ?? "—");
        stderrRow.classList.add("mono");
        card.append(stderrRow);
        result.replaceChildren(card);
      })
      .catch((error: unknown) => showError(result, error));
  });
  panel.append(button, result);
  return panel;
}

function configView(initial: ConfigDto): HTMLElement {
  const panel = el("section", { className: "card" });
  panel.append(el("h3", { text: "连接配置(会话级;来自环境变量可覆盖)" }));
  panel.append(
    el("p", {
      className: "muted",
      text: "socket=认证 SystemControl Unix socket;principal=32 hex;key_file=64 hex Ed25519 种子文件路径(0600);cli_socket/cli_path 仅一致性自检使用。",
    }),
  );
  const socket = labeledInput("认证 socket", initial.socketPath ?? "", initial.socketPath ?? "");
  const principal = labeledInput(
    "principal(32 hex)",
    initial.principalHex ?? "",
    initial.principalHex ?? "",
  );
  const keyFile = labeledInput("key_file", initial.keyFile ?? "", initial.keyFile ?? "");
  const cliSocket = labeledInput("cli_socket", initial.cliSocket ?? "", initial.cliSocket ?? "");
  const cliPath = labeledInput(
    "cli_path",
    initial.cliPath ?? "../../target/debug/system-control-cli",
    initial.cliPath ?? "",
  );
  const resourceRoot = labeledInput(
    "resource_root(本地资源权威根目录;预算/成本可见性)",
    initial.resourceRoot ?? "",
    initial.resourceRoot ?? "",
  );
  const applicationRoot = labeledInput(
    "application_root(本地应用权威根目录;UI Surface 呈现)",
    initial.applicationRoot ?? "",
    initial.applicationRoot ?? "",
  );
  const status = el("p", { className: "muted", text: `配置来源:${initial.source}` });
  const save = el("button", { text: "保存会话配置" });
  save.addEventListener("click", () => {
    setConfig({
      socketPath: socket.input.value.trim() || null,
      principalHex: principal.input.value.trim() || null,
      keyFile: keyFile.input.value.trim() || null,
      cliSocket: cliSocket.input.value.trim() || null,
      cliPath: cliPath.input.value.trim() || null,
      resourceRoot: resourceRoot.input.value.trim() || null,
      applicationRoot: applicationRoot.input.value.trim() || null,
    })
      .then((saved) => {
        status.textContent = `配置来源:${saved.source}(已保存)`;
      })
      .catch((error: unknown) => showError(status.parentElement ?? panel, error));
  });
  panel.append(
    socket.row,
    principal.row,
    keyFile.row,
    cliSocket.row,
    cliPath.row,
    resourceRoot.row,
    applicationRoot.row,
    save,
    status,
  );
  return panel;
}

function healthView(title: string, action: () => Promise<ReceiptDto>): HTMLElement {
  const panel = el("section", { className: "card" });
  panel.append(el("h3", { text: title }));
  const result = el("div");
  const button = el("button", { text: "刷新(经认证 IPC)" });
  button.addEventListener("click", () => {
    void runReceiptAction(result, action);
  });
  panel.append(button, result);
  return panel;
}

function metricsView(): HTMLElement {
  const panel = el("section", { className: "card" });
  panel.append(el("h3", { text: "OpenMetrics 导出" }));
  const result = el("div");
  const artifact = el("button", { text: "artifact 域导出" });
  artifact.addEventListener("click", () => {
    void runReceiptAction(result, exportMetrics);
  });
  const semantic = el("button", { text: "semantic 域导出" });
  semantic.addEventListener("click", () => {
    void runReceiptAction(result, exportSemanticMetrics);
  });
  panel.append(artifact, semantic, result);
  return panel;
}

/** W32-E:指标面缺口登记(诚实边界——存在与否都明说,不发明数据)。 */
const METRICS_SURFACE_GAPS: ReadonlyArray<{ fact: string; detail: string }> = [
  {
    fact: "scrape/流式指标端点",
    detail: "B-TASK-006M 未竟项:无 HTTP scrape/订阅端点——本监控的「刷新」即重新经认证入口派发只读导出命令(拉模型,每次都是完整真实 dispatch)",
  },
  {
    fact: "宿主级资源用量指标",
    detail: "进程 CPU/内存/IO、预留实时用量等无任何既有导出面;指标目录只覆盖恢复目录(三域 26 族 + worker 生命周期),本视图不发明",
  },
  {
    fact: "resource 域 GUI 导出接线(已关闭,留档)",
    detail: "W32-A 只接了 artifact/semantic 两域导出命令;W32-E 补 resource 域只读接线(既有 ControlCommand::ExportResourceMetrics,无新控制路径)",
  },
];

const MONITOR_REFRESH_MS = 5000;

interface MonitorDomain {
  id: RecoveryDomainId;
  label: string;
  export: () => Promise<ReceiptDto>;
}

const MONITOR_DOMAINS: MonitorDomain[] = [
  {
    id: "artifact",
    label: "artifact 域(恢复工作器生命周期 + 计数/gauge)",
    export: exportMetrics,
  },
  {
    id: "semantic",
    label: "semantic 域(W27-A 语义恢复目录)",
    export: exportSemanticMetrics,
  },
  {
    id: "resource",
    label: "resource 域(G8 资源恢复目录)",
    export: exportResourceMetrics,
  },
];

function formatLabels(labels: Readonly<Record<string, string>>): string {
  const entries = Object.entries(labels);
  if (entries.length === 0) {
    return "—";
  }
  return entries.map(([key, value]) => `${key}="${value}"`).join(", ");
}

function metricsFamiliesTable(families: MetricFamily[]): HTMLElement {
  const table = el("table", { className: "data-table" });
  const header = el("tr");
  header.append(
    el("th", { text: "指标族" }),
    el("th", { text: "类型" }),
    el("th", { text: "标签" }),
    el("th", { text: "值" }),
  );
  const head = el("thead");
  head.append(header);
  const body = el("tbody");
  for (const family of families) {
    if (family.samples.length === 0) {
      const row = el("tr");
      const cell = el("td", { text: `${family.name}(无样本)` });
      cell.colSpan = 4;
      row.append(cell);
      body.append(row);
      continue;
    }
    family.samples.forEach((sample, index) => {
      const row = el("tr");
      if (index === 0) {
        const nameCell = el("td", { text: family.name });
        nameCell.rowSpan = family.samples.length;
        const kindCell = el("td", { text: family.kind });
        kindCell.rowSpan = family.samples.length;
        row.append(nameCell, kindCell);
      }
      row.append(
        el("td", { text: formatLabels(sample.labels) }),
        el("td", { text: sample.value }),
      );
      body.append(row);
    });
  }
  table.append(head, body);
  return table;
}

function renderMonitorDomainCard(domain: MonitorDomain, receipt: ReceiptDto): HTMLElement {
  const card = el("section", { className: "card" });
  card.append(el("h3", { text: domain.label }));
  if (receipt.outcome.kind !== "metrics_exported") {
    card.append(renderOutcome(receipt.outcome));
    card.append(receiptFooter(receipt));
    return card;
  }
  const parsed = parseOpenMetricsText(receipt.outcome.openmetricsText);
  const own: MetricFamily[] = [];
  const foreign: MetricFamily[] = [];
  for (const family of parsed.families) {
    if (familyDomain(family.name) === domain.id) {
      own.push(family);
    } else {
      foreign.push(family);
    }
  }
  card.append(
    el("p", {
      className: "muted",
      text: `本域 ${own.length} 个指标族(共解析 ${parsed.families.length} 族);取数时间 ${new Date().toLocaleTimeString()}`,
    }),
  );
  card.append(metricsFamiliesTable(own));
  if (foreign.length > 0) {
    card.append(el("h4", { text: "非本域前缀的族(按导出文本如实显示)" }));
    card.append(metricsFamiliesTable(foreign));
  }
  if (parsed.unparsedLines.length > 0) {
    card.append(el("h4", { text: "未识别行(如实显示,不静默丢弃)" }));
    card.append(el("pre", { className: "metrics", text: parsed.unparsedLines.join("\n") }));
  }
  const details = el("details");
  details.append(el("summary", { text: "原始 OpenMetrics 文本(receipt 载荷)" }));
  details.append(el("pre", { className: "metrics", text: receipt.outcome.openmetricsText }));
  card.append(details);
  card.append(receiptFooter(receipt));
  return card;
}

function metricsGapRegisterCard(): HTMLElement {
  const panel = el("section", { className: "card" });
  panel.append(el("h3", { text: "指标面缺口登记(消费边界)" }));
  panel.append(
    el("p", {
      className: "muted",
      text: "本视图消费的 OpenMetrics 面经 SABI IPC 真实可达(三条既有只读导出命令,回执携带文本);以下缺口如实登记,不渲染任何未导出的数据(证据 b-gui-001 §W32-E)。",
    }),
  );
  const table = el("table", { className: "data-table" });
  const header = el("tr");
  header.append(el("th", { text: "缺口" }), el("th", { text: "说明" }));
  const head = el("thead");
  head.append(header);
  const body = el("tbody");
  for (const gap of METRICS_SURFACE_GAPS) {
    const tr = el("tr");
    tr.append(el("td", { text: gap.fact }), el("td", { text: gap.detail }));
    body.append(tr);
  }
  table.append(head, body);
  panel.append(table);
  return panel;
}

function resourceMonitorView(): HTMLElement {
  const wrap = el("div");
  const panel = el("section", { className: "card" });
  panel.append(el("h3", { text: "Resource Monitor(OpenMetrics 消费,只读)" }));
  panel.append(
    el("p", {
      className: "muted",
      text: "消费既有指标面:三个恢复域各经认证入口派发一条既有只读导出命令(ExportMetrics / ExportSemanticMetrics / ExportResourceMetrics),回执携带 OpenMetrics 文本,前端结构化渲染计数/gauge;无任何新控制路径。计数器值为 u64 十进制文本,按原样显示不做数值换算。",
    }),
  );
  const refresh = el("button", { text: "刷新(三域,经认证 IPC)" });
  const autoLabel = el("label", { text: " 自动刷新(5 秒)" });
  const auto = el("input");
  auto.type = "checkbox";
  autoLabel.prepend(auto);
  const status = el("p", { className: "muted", text: "尚未取数;点击刷新或勾选自动刷新。" });
  const domainsBox = el("div");
  const boxes = new Map<RecoveryDomainId, HTMLElement>();
  for (const domain of MONITOR_DOMAINS) {
    const box = el("div");
    boxes.set(domain.id, box);
    domainsBox.append(box);
  }
  panel.append(refresh, autoLabel, status, domainsBox);

  const fetchAll = (): void => {
    status.textContent = "刷新中……";
    let pending = MONITOR_DOMAINS.length;
    for (const domain of MONITOR_DOMAINS) {
      const box = boxes.get(domain.id);
      if (box === undefined) {
        continue;
      }
      domain
        .export()
        .then((receipt) => {
          box.replaceChildren(renderMonitorDomainCard(domain, receipt));
        })
        .catch((error: unknown) => showError(box, error))
        .finally(() => {
          pending -= 1;
          if (pending === 0) {
            status.textContent = `最近刷新:${new Date().toLocaleTimeString()}`;
          }
        });
    }
  };

  let timer = 0;
  refresh.addEventListener("click", fetchAll);
  auto.addEventListener("change", () => {
    if (auto.checked) {
      fetchAll();
      timer = window.setInterval(fetchAll, MONITOR_REFRESH_MS);
    } else {
      window.clearInterval(timer);
      timer = 0;
    }
  });

  wrap.append(panel, metricsGapRegisterCard());
  return wrap;
}

async function bootstrap(): Promise<void> {
  const app = document.querySelector("#app");
  if (app === null) {
    return;
  }
  const config = await getConfig();
  const layout = el("div", { className: "layout" });
  const sidebar = el("nav", { className: "sidebar" });
  const main = el("main", { className: "main" });
  const views = new Map<string, HTMLElement>();
  const register = (id: string, label: string, node: HTMLElement): void => {
    views.set(id, node);
    const tab = el("button", { className: "tab", text: label });
    tab.addEventListener("click", () => {
      for (const [otherId, otherNode] of views) {
        otherNode.classList.toggle("active", otherId === id);
      }
      for (const child of sidebar.children) {
        if (child instanceof HTMLElement) {
          child.classList.toggle("active", child === tab);
        }
      }
    });
    sidebar.append(tab);
    node.classList.add("view", id === "health" ? "active" : "");
    main.append(node);
  };
  register("health", "恢复总览", healthView("恢复总览(InspectHealth)", inspectHealth));
  register(
    "semantic",
    "语义恢复",
    healthView("语义恢复(InspectSemanticHealth)", inspectSemanticHealth),
  );
  register(
    "task",
    "任务查询",
    targetQueryView({
      label: "任务(恢复计划)id",
      placeholder: "32 hex 字符 plan_id",
      button: "查询任务",
      dispatch: inspectTask,
    }),
  );
  register("task-space", "任务空间", taskSpaceView());
  register(
    "process",
    "进程查询",
    targetQueryView({
      label: "进程 id",
      placeholder: "32 hex 字符 process_id",
      button: "查询进程",
      dispatch: inspectProcess,
    }),
  );
  register(
    "resource",
    "资源查询",
    targetQueryView({
      label: "资源预留 id",
      placeholder: "32 hex 字符 reservation_id",
      button: "查询资源",
      dispatch: inspectResource,
    }),
  );
  register("metrics", "指标导出", metricsView());
  register("monitor", "资源监控", resourceMonitorView());
  register("surfaces", "应用表面", appSurfacesView());
  register("control", "控制动作", controlView());
  register("permission", "权限/预算", permissionView(config));
  const parity = el("div");
  parity.append(parityView(), parityWriteCard());
  register("parity", "一致性自检", parity);
  register("config", "连接配置", configView(config));
  if (sidebar.firstElementChild instanceof HTMLElement) {
    sidebar.firstElementChild.classList.add("active");
  }
  layout.append(sidebar, main);
  app.replaceChildren(layout);
}

void bootstrap();
