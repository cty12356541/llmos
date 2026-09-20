import "./style.css";

import {
  exportMetrics,
  exportSemanticMetrics,
  getConfig,
  inspectHealth,
  inspectProcess,
  inspectResource,
  inspectResourceHealth,
  inspectSemanticHealth,
  inspectTask,
  parityCheck,
  parityCheckWrite,
  setConfig,
  submitControl,
} from "./ipc";
import type {
  ConfigDto,
  ControlActionInput,
  OutcomeDto,
  ReceiptDto,
} from "./types";
import { MUTATION_OUTCOME_KINDS } from "./types";

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
        fieldRow("upper_bound", String(outcome.upperBound)),
        fieldRow("usage_high_water", String(outcome.usageHighWater)),
        fieldRow("consumption_count", String(outcome.consumptionCount)),
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
    case "operation_reclaimed": {
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

function renderReceipt(receipt: ReceiptDto): HTMLElement {
  const wrap = el("div");
  wrap.append(renderOutcome(receipt.outcome));
  const footer = el("section", { className: "card receipt-footer" });
  footer.append(
    fieldRow("control_command_id", receipt.controlCommandIdHex),
    fieldRow("correlation_id", receipt.correlationIdHex),
  );
  const receiptRow = fieldRow("receipt (to_bytes hex)", receipt.receiptHex);
  receiptRow.classList.add("mono");
  footer.append(receiptRow);
  wrap.append(footer);
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
    "export-metrics",
    "export-semantic-metrics",
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
  const status = el("p", { className: "muted", text: `配置来源:${initial.source}` });
  const save = el("button", { text: "保存会话配置" });
  save.addEventListener("click", () => {
    setConfig({
      socketPath: socket.input.value.trim() || null,
      principalHex: principal.input.value.trim() || null,
      keyFile: keyFile.input.value.trim() || null,
      cliSocket: cliSocket.input.value.trim() || null,
      cliPath: cliPath.input.value.trim() || null,
    })
      .then((saved) => {
        status.textContent = `配置来源:${saved.source}(已保存)`;
      })
      .catch((error: unknown) => showError(status.parentElement ?? panel, error));
  });
  panel.append(socket.row, principal.row, keyFile.row, cliSocket.row, cliPath.row, save, status);
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
  register("control", "控制动作", controlView());
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
