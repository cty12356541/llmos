// Task Space 最小只读视图(W33-F,X-2;决策点 2:完整桌面归阶段 D)。
//
// 边界(进度单 §6.5.4 决策点 2):Task Space = 复用 Task Manager 读侧的
// 只读任务列表/详情——全部数据经既有认证只读命令(与 CLI 同一回执字节),
// 零新后端命令、零 mutation。文件边界:本目录是本车道在 desktop/ 的
// 唯一写集,主壳只加一行注册(main.ts import + register)。
//
// 数据路径(全部为 Task Manager 既有读面):
// - 任务列表:InspectHealth/InspectSemanticHealth/InspectResourceHealth
//   三域巡检回执的 escalated 告警行(plan_id 键)+ 会话内 follow 集合;
// - 任务详情:InspectTask(单计划告警回执)、InspectProcess/InspectResourceCost
//   (关联实体快照,操作者按已知 id 查询);
// - 回执页脚恒显 receipt_hex,与 `system-control-cli <plain_socket> …`
//   的 `RECEIPT` 行同一等价契约(house parity 纪律)。

import {
  inspectExecutionFiber,
  inspectHealth,
  inspectOperation,
  inspectProcess,
  inspectResourceCost,
  inspectSemanticHealth,
  inspectResourceHealth,
  inspectTask,
  inspectTaskGroup,
  inspectTaskNode,
  inspectTopic,
} from "../../ipc";
import type { ReceiptDto } from "../../types";
import { el, fieldRow, labeledInput, showError } from "./dom";
import { renderReceipt, renderOutcome, receiptFooter } from "./receipts";
import type { DomainScan, RecoveryDomain, TaskRow } from "./model";
import { rebuildRows, scanFromReceipt } from "./model";

const HEX32 = /^[0-9a-fA-F]{32}$/;

const DOMAIN_LABELS: Readonly<Record<RecoveryDomain, string>> = {
  artifact: "artifact",
  semantic: "semantic",
  resource: "resource",
};

/** 无 IPC 面的事实缺口(诚实边界:不发明数据,静态登记,供后续车道补面)。 */
const TASK_SPACE_GAPS: ReadonlyArray<{ fact: string; detail: string }> = [
  {
    fact: "任务/TaskGroup 全量枚举面",
    detail:
      "无 IPC 列表命令——恢复巡检只投影 escalated 告警行;本列表 = 三域 escalated 计划行 + 手动关注 plan_id(InspectTask 验证存在),全量枚举待后续 IPC 面(§28.4 完整 Task Space)",
  },
  {
    fact: "W29-A 关联字段(application_id / plan_revision)",
    detail:
      "TaskSpec v44 已在 tasks 表落三列(nlos-task schema),但现有可达读面(InspectTask 告警投影)不携带这两字段——无 IPC 投影,本视图不发明;待 inspect 面扩列后在此呈现",
  },
  {
    fact: "fiber / operation 列表面",
    detail:
      "上游 W32-G 已登记:runtime 无 fiber id 枚举 API、store 行缺失与 stale generation 不可区分——Task Space 的层级浏览同样受此约束;按已知 id 的单点 inspect 已由 W39-D 接通",
  },
];

/** 详情卡里提供的 Task Manager 视图跳转(侧栏既有 tab,零主壳改动)。 */
const TASK_MANAGER_TABS: ReadonlyArray<{ label: string; hint: string }> = [
  { label: "恢复总览", hint: "artifact 域巡检(InspectHealth)" },
  { label: "语义恢复", hint: "semantic 域巡检" },
  { label: "任务查询", hint: "单计划 InspectTask" },
  { label: "进程查询", hint: "进程绑定 InspectProcess" },
  { label: "资源查询", hint: "资源预留 InspectResource" },
  { label: "一致性自检", hint: "GUI↔CLI receipt 字节比对" },
];

function openSidebarTab(label: string): boolean {
  const sidebar = document.querySelector(".sidebar");
  if (sidebar === null) {
    return false;
  }
  for (const child of sidebar.children) {
    if (child instanceof HTMLElement && child.classList.contains("tab") && child.textContent === label) {
      child.click();
      return true;
    }
  }
  return false;
}

function runReceiptAction(
  container: HTMLElement,
  action: () => Promise<ReceiptDto>,
): void {
  container.replaceChildren(el("p", { className: "muted", text: "派发中……" }));
  action()
    .then((receipt) => {
      container.replaceChildren(renderReceipt(receipt));
    })
    .catch((error: unknown) => showError(container, error));
}

function domainBadge(row: TaskRow): string {
  const parts: string[] = [];
  for (const domain of ["artifact", "semantic", "resource"] as const) {
    const alert = row.domains.get(domain);
    if (alert === undefined) {
      continue;
    }
    const ack = alert.acknowledgedReceiptIdHex === null ? "未确认" : "已确认";
    parts.push(`${DOMAIN_LABELS[domain]}:${alert.totalFailures}(${ack})`);
  }
  return parts.length > 0 ? parts.join(" / ") : "—(三域台账无 escalated 行)";
}

function scanSummaryRows(scans: readonly DomainScan[]): HTMLElement {
  const wrap = el("div");
  for (const scan of scans) {
    if (scan.counts === null) {
      wrap.append(
        fieldRow(`${DOMAIN_LABELS[scan.domain]} 域巡检`, "(类型化失败,无聚合事实)"),
      );
      continue;
    }
    wrap.append(
      fieldRow(
        `${DOMAIN_LABELS[scan.domain]} 域(retrying/escalated/unack/resolved)`,
        `${scan.counts.durableRetrying} / ${scan.counts.durableEscalated} / ${scan.counts.durableUnacknowledgedEscalated} / ${scan.counts.durableResolved}`,
      ),
    );
  }
  return wrap;
}

interface TaskSpaceState {
  scans: DomainScan[];
  rows: Map<string, TaskRow>;
  followed: Set<string>;
  selected: string | null;
}

function taskDetailPane(state: TaskSpaceState): { node: HTMLElement; taskResult: HTMLElement | null } {
  const wrap = el("div");
  const selected = state.selected === null ? null : state.rows.get(state.selected) ?? null;
  if (selected === null) {
    wrap.append(
      el("p", { className: "muted", text: "在上方列表点选一个任务(plan_id)查看详情。" }),
    );
    return { node: wrap, taskResult: null };
  }

  const header = el("section", { className: "card" });
  header.append(el("h3", { text: `任务详情:${selected.planIdHex}` }));
  header.append(
    fieldRow("来源", selected.source === "scan" ? "三域巡检发现(escalated)" : "手动关注(InspectTask 验证)"),
  );
  for (const domain of ["artifact", "semantic", "resource"] as const) {
    const alert = selected.domains.get(domain);
    header.append(
      fieldRow(
        `${DOMAIN_LABELS[domain]} 域告警关联`,
        alert === undefined
          ? "—(最新扫描无此计划)"
          : `total_failures=${alert.totalFailures},acknowledged=${alert.acknowledgedReceiptIdHex ?? "未确认"}`,
      ),
    );
  }
  const navRow = el("p", { className: "muted", text: "跳转 Task Manager 视图:" });
  for (const tab of TASK_MANAGER_TABS) {
    const button = el("button", { text: tab.label });
    button.title = tab.hint;
    button.addEventListener("click", () => {
      openSidebarTab(tab.label);
    });
    navRow.append(button);
  }
  header.append(navRow);
  wrap.append(header);

  const taskReceipt = el("section", { className: "card" });
  taskReceipt.append(el("h3", { text: "任务回执(InspectTask,经认证 IPC)" }));
  const taskResult = el("div");
  const refresh = el("button", { text: "查询/刷新任务回执" });
  refresh.addEventListener("click", () => {
    runReceiptAction(taskResult, () => inspectTask(selected.planIdHex));
  });
  taskReceipt.append(refresh, taskResult);
  wrap.append(taskReceipt);

  const entities = el("section", { className: "card" });
  entities.append(el("h3", { text: "关联实体查询(按已知 id,只读)" }));
  entities.append(
    el("p", {
      className: "muted",
      text: "任务↔进程/预留的绑定枚举无 IPC 面(见缺口登记);按操作者已知的 process_id / reservation_id 查询关联快照。",
    }),
  );
  const process = labeledInput("进程 id(32 hex)", "32 hex 字符 process_id", "");
  const processResult = el("div");
  const processButton = el("button", { text: "查询进程绑定" });
  processButton.addEventListener("click", () => {
    const id = process.input.value.trim();
    if (!HEX32.test(id)) {
      processResult.replaceChildren(
        el("p", { className: "muted", text: "请先输入 32 位 hex 的 process_id。" }),
      );
      return;
    }
    runReceiptAction(processResult, () => inspectProcess(id.toLowerCase()));
  });
  const reservation = labeledInput("资源预留 id(32 hex)", "32 hex 字符 reservation_id", "");
  const reservationResult = el("div");
  const reservationButton = el("button", { text: "查询预留成本" });
  reservationButton.addEventListener("click", () => {
    const id = reservation.input.value.trim();
    if (!HEX32.test(id)) {
      reservationResult.replaceChildren(
        el("p", { className: "muted", text: "请先输入 32 位 hex 的 reservation_id。" }),
      );
      return;
    }
    runReceiptAction(reservationResult, () => inspectResourceCost(id.toLowerCase()));
  });
  entities.append(process.row, processButton, processResult, reservation.row, reservationButton, reservationResult);
  wrap.append(entities);
  return { node: wrap, taskResult };
}

function planNodesCard(): HTMLElement {
  const panel = el("section", { className: "card" });
  panel.append(el("h3", { text: "五层只读 inspect(W39-D / §28.4 Task Space)" }));
  panel.append(
    el("p", {
      className: "muted",
      text: "按操作者已知 id 派发既有 SABI v1.5 ControlCommand(经认证 IPC);列表面仍缺。夹具未接 layer inspector 时回执为类型化 NOT_FOUND(与 CLI 同字节)。generation 必须非零。",
    }),
  );

  const group = labeledInput("TaskGroup id(32 hex)", "group_id", "");
  const groupResult = el("div");
  const groupButton = el("button", { text: "InspectTaskGroup" });
  groupButton.addEventListener("click", () => {
    const id = group.input.value.trim().toLowerCase();
    if (!HEX32.test(id)) {
      groupResult.replaceChildren(el("p", { className: "muted", text: "请先输入 32 位 hex group_id。" }));
      return;
    }
    runReceiptAction(groupResult, () => inspectTaskGroup(id));
  });

  const plan = labeledInput("TaskNode plan_id(32 hex)", "plan_id", "");
  const node = labeledInput("TaskNode node_id(32 hex)", "node_id", "");
  const nodeResult = el("div");
  const nodeButton = el("button", { text: "InspectTaskNode" });
  nodeButton.addEventListener("click", () => {
    const planId = plan.input.value.trim().toLowerCase();
    const nodeId = node.input.value.trim().toLowerCase();
    if (!HEX32.test(planId) || !HEX32.test(nodeId)) {
      nodeResult.replaceChildren(
        el("p", { className: "muted", text: "请先输入 32 位 hex 的 plan_id 与 node_id。" }),
      );
      return;
    }
    runReceiptAction(nodeResult, () => inspectTaskNode(planId, nodeId));
  });

  const fiber = labeledInput("ExecutionFiber id(32 hex)", "fiber_id", "");
  const fiberGen = labeledInput("fiber generation(>0)", "generation", "1");
  const fiberResult = el("div");
  const fiberButton = el("button", { text: "InspectExecutionFiber" });
  fiberButton.addEventListener("click", () => {
    const id = fiber.input.value.trim().toLowerCase();
    const generation = Number.parseInt(fiberGen.input.value.trim(), 10);
    if (!HEX32.test(id) || !Number.isFinite(generation) || generation < 1) {
      fiberResult.replaceChildren(
        el("p", { className: "muted", text: "请输入 32 hex fiber_id 与非零 generation。" }),
      );
      return;
    }
    runReceiptAction(fiberResult, () => inspectExecutionFiber(id, generation));
  });

  const topic = labeledInput("Topic id(32 hex)", "topic_id", "");
  const topicResult = el("div");
  const topicButton = el("button", { text: "InspectTopic" });
  topicButton.addEventListener("click", () => {
    const id = topic.input.value.trim().toLowerCase();
    if (!HEX32.test(id)) {
      topicResult.replaceChildren(el("p", { className: "muted", text: "请先输入 32 位 hex topic_id。" }));
      return;
    }
    runReceiptAction(topicResult, () => inspectTopic(id));
  });

  const operation = labeledInput("DurableOperation id(32 hex)", "operation_id", "");
  const opGen = labeledInput("operation generation(>0)", "generation", "1");
  const opResult = el("div");
  const opButton = el("button", { text: "InspectOperation" });
  opButton.addEventListener("click", () => {
    const id = operation.input.value.trim().toLowerCase();
    const generation = Number.parseInt(opGen.input.value.trim(), 10);
    if (!HEX32.test(id) || !Number.isFinite(generation) || generation < 1) {
      opResult.replaceChildren(
        el("p", { className: "muted", text: "请输入 32 hex operation_id 与非零 generation。" }),
      );
      return;
    }
    runReceiptAction(opResult, () => inspectOperation(id, generation));
  });

  panel.append(
    group.row,
    groupButton,
    groupResult,
    plan.row,
    node.row,
    nodeButton,
    nodeResult,
    fiber.row,
    fiberGen.row,
    fiberButton,
    fiberResult,
    topic.row,
    topicButton,
    topicResult,
    operation.row,
    opGen.row,
    opButton,
    opResult,
  );
  return panel;
}

function gapRegisterCard(): HTMLElement {
  const panel = el("section", { className: "card" });
  panel.append(el("h3", { text: "缺口登记:无 IPC 面的任务事实(本视图不渲染)" }));
  panel.append(
    el("p", {
      className: "muted",
      text: "以下事实存在于权威(crate 已持久化)或上游 IPC 面,但本壳可达读面不含——按诚实纪律不发明数据,登记待后续车道补面(证据 b-gui-001 §W33-F)。",
    }),
  );
  const table = el("table", { className: "data-table" });
  const header = el("tr");
  header.append(el("th", { text: "任务事实" }), el("th", { text: "缺口说明" }));
  const head = el("thead");
  head.append(header);
  const body = el("tbody");
  for (const gap of TASK_SPACE_GAPS) {
    const tr = el("tr");
    tr.append(el("td", { text: gap.fact }), el("td", { text: gap.detail }));
    body.append(tr);
  }
  table.append(head, body);
  panel.append(table);
  return panel;
}

export function taskSpaceView(): HTMLElement {
  const state: TaskSpaceState = {
    scans: [],
    rows: new Map(),
    followed: new Set(),
    selected: null,
  };

  const wrap = el("div");

  const intro = el("section", { className: "card" });
  intro.append(el("h3", { text: "Task Space(最小只读任务空间,X-2)" }));
  intro.append(
    el("p", {
      className: "muted",
      text: "决策点 2 边界:只读视图,复用 Task Manager 读侧——列表/详情全部来自既有认证只读命令的同一批回执(与 CLI 字节一致,页脚恒显 receipt_hex);无任何 mutation 派发路径。完整桌面归阶段 D。",
    }),
  );
  wrap.append(intro);

  const listCard = el("section", { className: "card" });
  listCard.append(el("h3", { text: "任务列表(三域恢复台账扫描 + 手动关注)" }));
  const status = el("p", { className: "muted", text: "尚未扫描;点击刷新派发三条只读巡检命令。" });
  const summary = el("div");
  const scanFooter = el("div");
  const tableBox = el("div");
  const refresh = el("button", { text: "刷新(三域巡检,经认证 IPC)" });

  const follow = labeledInput("关注任务 plan_id(32 hex)", "32 hex 字符 plan_id", "");
  const followResult = el("div");
  const followButton = el("button", { text: "关注(InspectTask 验证)" });

  const detailBox = el("div");

  const renderTable = (): void => {
    tableBox.replaceChildren();
    const detail = taskDetailPane(state);
    detailBox.replaceChildren(detail.node);
    const selectedId = state.selected;
    if (detail.taskResult !== null && selectedId !== null) {
      runReceiptAction(detail.taskResult, () => inspectTask(selectedId));
    }
    if (state.rows.size === 0) {
      tableBox.append(
        el("p", { className: "muted", text: "(空)三域台账当前无 escalated 计划行,且无手动关注任务。" }),
      );
      return;
    }
    const table = el("table", { className: "data-table" });
    const header = el("tr");
    header.append(
      el("th", { text: "plan_id" }),
      el("th", { text: "来源" }),
      el("th", { text: "域告警关联(total_failures/确认态)" }),
      el("th", { text: "操作" }),
    );
    const head = el("thead");
    head.append(header);
    const body = el("tbody");
    for (const row of [...state.rows.values()].sort((a, b) => a.planIdHex.localeCompare(b.planIdHex))) {
      const tr = el("tr");
      if (row.planIdHex === state.selected) {
        tr.style.fontWeight = "600";
      }
      tr.append(
        el("td", { text: row.planIdHex }),
        el("td", { text: row.source === "scan" ? "巡检发现" : "手动关注" }),
        el("td", { text: domainBadge(row) }),
      );
      const actions = el("td");
      const detail = el("button", { text: "详情" });
      detail.addEventListener("click", () => {
        state.selected = row.planIdHex;
        renderTable();
      });
      actions.append(detail);
      if (row.source === "follow") {
        const unfollow = el("button", { text: "取消关注" });
        unfollow.addEventListener("click", () => {
          state.followed.delete(row.planIdHex);
          state.rows = rebuildRows(state.followed, state.scans);
          if (state.selected === row.planIdHex) {
            state.selected = null;
          }
          renderTable();
        });
        actions.append(unfollow);
      }
      tr.append(actions);
      body.append(tr);
    }
    table.append(head, body);
    tableBox.append(table);
  };

  const renderScanFooter = (): void => {
    scanFooter.replaceChildren();
    if (state.scans.length === 0) {
      return;
    }
    const footer = el("section", { className: "card receipt-footer" });
    footer.append(el("h4", { text: "扫描回执(与 CLI `RECEIPT` 行同一等价契约)" }));
    for (const scan of state.scans) {
      const row = fieldRow(`${DOMAIN_LABELS[scan.domain]} 域巡检 receipt`, scan.receiptHex);
      row.classList.add("mono");
      footer.append(row);
    }
    scanFooter.append(footer);
  };

  const refreshScan = (): void => {
    status.textContent = "扫描中(三条只读派发)……";
    const domains: ReadonlyArray<{ id: RecoveryDomain; dispatch: () => Promise<ReceiptDto> }> = [
      { id: "artifact", dispatch: inspectHealth },
      { id: "semantic", dispatch: inspectSemanticHealth },
      { id: "resource", dispatch: inspectResourceHealth },
    ];
    Promise.all(
      domains.map((domain) =>
        domain
          .dispatch()
          .then(
            (receipt) => scanFromReceipt(domain.id, receipt),
            (error: unknown) => ({ domain: domain.id, error }) as const,
          ),
      ),
    )
      .then((results) => {
        state.scans = [];
        const failures: string[] = [];
        for (const result of results) {
          if ("error" in result) {
            failures.push(DOMAIN_LABELS[result.domain]);
          } else {
            state.scans.push(result);
          }
        }
        state.rows = rebuildRows(state.followed, state.scans);
        summary.replaceChildren(scanSummaryRows(state.scans));
        renderScanFooter();
        renderTable();
        status.textContent =
          failures.length > 0
            ? `最近扫描:${new Date().toLocaleTimeString()}(${failures.join("/")} 域派发失败,见「一致性自检」排查)`
            : `最近扫描:${new Date().toLocaleTimeString()}`;
      })
      .catch((error: unknown) => showError(status.parentElement ?? listCard, error));
  };

  refresh.addEventListener("click", refreshScan);

  followButton.addEventListener("click", () => {
    const id = follow.input.value.trim().toLowerCase();
    if (!HEX32.test(id)) {
      followResult.replaceChildren(
        el("p", { className: "muted", text: "请先输入 32 位 hex 的 plan_id。" }),
      );
      return;
    }
    followResult.replaceChildren(el("p", { className: "muted", text: "验证中(InspectTask)……" }));
    inspectTask(id)
      .then((receipt) => {
        if (receipt.outcome.kind !== "inspected") {
          followResult.replaceChildren(renderOutcome(receipt.outcome), receiptFooter(receipt));
          return;
        }
        state.followed.add(id);
        state.selected = id;
        state.rows = rebuildRows(state.followed, state.scans);
        renderTable();
        followResult.replaceChildren(
          el("p", { className: "muted", text: `已关注 ${id}(存在于恢复快照);详情见下方。` }),
          renderReceipt(receipt),
        );
      })
      .catch((error: unknown) => showError(followResult, error));
  });

  listCard.append(status, refresh, summary, tableBox, follow.row, followButton, followResult, scanFooter);
  wrap.append(listCard, detailBox, planNodesCard(), gapRegisterCard());
  renderTable();
  return wrap;
}
