import "./style.css";

import {
  exportMetrics,
  exportSemanticMetrics,
  getConfig,
  inspectHealth,
  inspectProcess,
  inspectResource,
  inspectSemanticHealth,
  inspectTask,
  parityCheck,
  setConfig,
} from "./ipc";
import type { ConfigDto, OutcomeDto, ReceiptDto } from "./types";

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
  register("parity", "一致性自检", parityView());
  register("config", "连接配置", configView(config));
  if (sidebar.firstElementChild instanceof HTMLElement) {
    sidebar.firstElementChild.classList.add("active");
  }
  layout.append(sidebar, main);
  app.replaceChildren(layout);
}

void bootstrap();
