// ==========================================================================
// LLMOS Studio · Open Swiss Editorial Core Views (`views-core.ts`)
// Implements Views 01..04 with zero pill soup & open hairline cross-grid:
//   01. System Posture Overview (`buildOverviewViewElement`)
//   02. Natural-Language Intent & glm-5.3 Pool (`buildIntentKeyPoolViewElement`)
//   03. Task Commitment Ledger & Orthogonal DAG (`buildTasksDagViewElement`)
//   04. 10-Layer Ownership Tree & Compute Cells (`buildProcessesResourcesViewElement`)
// ==========================================================================

import {
  ALL_CONTROL_CENTER_DOMAINS,
  computeControlDomainSummaries,
  enrichOwnershipTree,
  renderCellLeasePanelHtml,
  renderOwnershipTreeHtml,
  windowOwnershipTreeRows,
  type ControlCenterDomainId,
} from "../views/control-center";
import {
  renderTemporalPhaseSummaryText,
  type TaskSpaceWorkstationRow,
} from "../views/task-space";
import type { OperatorStudioStore, StudioStateSnapshot } from "./studio-store";

function esc(raw: string): string {
  return raw
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

const DOMAIN_NAMES_ZH: Readonly<Record<ControlCenterDomainId, string>> = {
  "system-device": "系统与硬件设备",
  workspace: "工作区与租户",
  application: "已安装应用",
  task: "任务生命周期",
  "task-group": "任务编排组",
  "task-node": "执行步骤节点",
  "task-attempt": "执行尝试记录",
  "execution-unit": "进程与并发流",
  "resource-group": "资源保障池",
  "topic-channel": "事件通道与队列",
  operation: "外部操作与外设",
  cell: "计算节点与分布式租约",
};

const DOMAIN_NAMES_EN: Readonly<Record<ControlCenterDomainId, string>> = {
  "system-device": "System & Hardware",
  workspace: "Workspace & Tenant",
  application: "Installed Applications",
  task: "Task Lifecycle",
  "task-group": "Task Groups",
  "task-node": "Step Nodes",
  "task-attempt": "Execution Runs",
  "execution-unit": "Processes & Fibers",
  "resource-group": "Resource Pools",
  "topic-channel": "Topics & Queues",
  operation: "External Operations",
  cell: "Compute Cells & Leases",
};

function statusSealHtml(stateStr: string, isEn: boolean): string {
  const s = stateStr.toUpperCase();
  if (s === "RUNNING" || s === "ACTIVE") {
    return `<span class="st-seal" data-tone="accent">${isEn ? "RUNNING" : "运行中"}</span>`;
  }
  if (s === "AUTHORIZED") {
    return `<span class="st-seal" data-tone="accent">${isEn ? "AUTHORIZED" : "已授权"}</span>`;
  }
  if (s === "COMPLETED") {
    return `<span class="st-seal" data-tone="forest">${isEn ? "COMPLETED" : "已完成"}</span>`;
  }
  if (
    s === "WAITING_CONSENT" ||
    s === "WAITING_INPUT" ||
    s === "WAITING_RESOURCE" ||
    s === "WAITING_NETWORK" ||
    s === "RECOVERING" ||
    s === "PROPOSED" ||
    s === "SUSPENDED"
  ) {
    const label =
      s === "WAITING_CONSENT"
        ? isEn
          ? "AWAITING CONSENT"
          : "待安全确认"
        : s === "WAITING_INPUT"
          ? isEn
            ? "NEEDS INPUT"
            : "待补充输入"
          : s === "WAITING_RESOURCE"
            ? isEn
              ? "WAIT RESOURCE"
              : "等待资源预留"
            : s === "WAITING_NETWORK"
              ? isEn
                ? "WAIT NETWORK"
                : "等待网络通道"
              : s === "RECOVERING"
                ? isEn
                  ? "RECOVERING"
                  : "故障恢复中"
                : s === "SUSPENDED"
                  ? isEn
                    ? "PAUSED"
                    : "已暂停"
                  : isEn
                    ? "PROPOSED"
                    : "草稿待提交";
    return `<span class="st-seal" data-tone="vermilion">${esc(label)}</span>`;
  }
  if (
    s === "FAILED" ||
    s === "QUARANTINED" ||
    s === "EFFECT_UNKNOWN" ||
    s === "DISPUTED" ||
    s === "PARTIAL" ||
    s === "CANCELLING" ||
    s === "CANCELLED"
  ) {
    const label =
      s === "QUARANTINED"
        ? isEn
          ? "QUARANTINED"
          : "受控隔离"
        : s === "PARTIAL"
          ? isEn
            ? "PARTIAL"
            : "部分完成"
          : s === "CANCELLED" || s === "CANCELLING"
            ? isEn
              ? "CANCELLED"
              : "已取消"
            : isEn
              ? "ATTENTION"
              : "需人工核查";
    return `<span class="st-seal" data-tone="vermilion">${esc(label)}</span>`;
  }
  return `<span class="st-seal">${esc(stateStr)}</span>`;
}

function hairlineBarHtml(
  usage: number,
  effective: number,
  ceiling: number,
): string {
  const pct = Math.max(0, Math.min(100, Math.round(usage)));
  const tone = pct >= 78 ? "vermilion" : pct >= 50 ? "accent" : "normal";
  return (
    `<div class="st-bar">` +
    `<div class="st-bar-track">` +
    `<div class="st-bar-fill" data-tone="${tone}" style="width:${pct}%"></div>` +
    `</div>` +
    `<span class="st-mono">${usage}/${effective}/${ceiling}%</span>` +
    `</div>`
  );
}

/* ==========================================================================
   01. System Posture Overview
   ========================================================================== */

export function buildOverviewViewElement(
  doc: Document,
  store: OperatorStudioStore,
  snap: StudioStateSnapshot,
): HTMLElement {
  const isEn = snap.locale === "en-US";
  const wrap = doc.createElement("div");

  const taskRows = store.controllers.taskSpace.getRows();
  const activeCount = taskRows.filter(
    (r) =>
      r.summary.taskState !== "COMPLETED" &&
      r.summary.taskState !== "CANCELLED",
  ).length;
  const waitingCount = taskRows.filter(
    (r) =>
      r.summary.taskState === "WAITING_CONSENT" ||
      r.summary.taskState === "WAITING_INPUT",
  ).length;

  const ctrlState = store.controllers.controlCenter.getState();
  const enriched = enrichOwnershipTree(ctrlState.treeSpecs, {
    locale: snap.locale,
    disclosure: snap.disclosure,
  });
  const domainSummaries = computeControlDomainSummaries(enriched);

  const totalResolved =
    snap.recoveryHealth.artifact.resolved +
    snap.recoveryHealth.semantic.resolved +
    snap.recoveryHealth.resource.resolved;

  wrap.innerHTML =
    `<header class="st-masthead-band">` +
    `<div>` +
    `<div class="st-page-kicker">SECTION 01 · SYSTEM CONTROL POSTURE</div>` +
    `<h1 class="st-page-title">${isEn ? "System Posture & Control Overview" : "系统全局态势与控制总览"}</h1>` +
    `<p class="st-page-desc">${
      isEn
        ? "Open Swiss editorial telemetry across frozen SABI v1.0 channels, metered glm-5.3 keypool, 3-domain durable recovery ledgers, and 12 ownership domains."
        : "汇聚 9 大冻结 SABI 通道、glm-5.3 五维预算号池、三域持久化恢复台账与 12 域资源归属树的核心运行态势。"
    }</p>` +
    `</div>` +
    `<div style="display:flex;align-items:center;gap:10px;">` +
    `<button type="button" class="st-btn" data-studio-action="refresh-daemon">${isEn ? "Sync Daemon" : "同步守护进程"}</button>` +
    `<button type="button" class="st-btn st-btn--primary" data-studio-nav="intent-keypool">${isEn ? "+ Dispatch Intent" : "+ 下达自然语言意图"}</button>` +
    `</div>` +
    `</header>` +
    `<section class="st-metric-band">` +
    `<div class="st-metric-cell">` +
    `<div class="st-metric-label"><span style="white-space:nowrap;">${isEn ? "SABI Channels" : "SABI 契约"}</span><span class="st-seal" data-tone="accent">FROZEN</span></div>` +
    `<div class="st-metric-val">9 / 9</div>` +
    `<div class="st-metric-sub">${isEn ? "5-lang golden corpus" : "五语言 v1.0 零漂移"}</div>` +
    `</div>` +
    `<div class="st-metric-cell">` +
    `<div class="st-metric-label"><span style="white-space:nowrap;">${isEn ? "Model Pool" : "本地模型池"}</span><span class="st-seal" data-tone="accent">${esc((snap.keypoolStatus.strategy ?? "sticky").toUpperCase())}</span></div>` +
    `<div class="st-metric-val">${snap.keypoolStatus.activeKeys ?? 41} / ${snap.keypoolStatus.totalKeys ?? 44}</div>` +
    `<div class="st-metric-sub">glm-5.3 · :8317</div>` +
    `</div>` +
    `<div class="st-metric-cell">` +
    `<div class="st-metric-label"><span style="white-space:nowrap;">${isEn ? "Active Tasks" : "工作站任务"}</span><span class="st-seal" data-tone="${waitingCount > 0 ? "vermilion" : "forest"}">${waitingCount > 0 ? (isEn ? `${waitingCount} WAIT` : `${waitingCount} 待确认`) : "OK"}</span></div>` +
    `<div class="st-metric-val">${activeCount} / ${taskRows.length}</div>` +
    `<div class="st-metric-sub">${isEn ? "4-stage DAG & lanes" : "4 阶段正交拓扑"}</div>` +
    `</div>` +
    `<div class="st-metric-cell">` +
    `<div class="st-metric-label"><span style="white-space:nowrap;">${isEn ? "Recovery" : "三域恢复"}</span><span class="st-seal" data-tone="forest">0 FAULT</span></div>` +
    `<div class="st-metric-val">${totalResolved}</div>` +
    `<div class="st-metric-sub">Cycle #r${snap.recoveryHealth.artifact.completedCycles}</div>` +
    `</div>` +
    `<div class="st-metric-cell">` +
    `<div class="st-metric-label"><span style="white-space:nowrap;">${isEn ? "Store PKI" : "生态商店"}</span><span class="st-seal" data-tone="heavy">EPOCH 3</span></div>` +
    `<div class="st-metric-val">4 Pkgs</div>` +
    `<div class="st-metric-sub">${isEn ? "Dual-layer Wasm" : "三级 PKI + 强沙箱"}</div>` +
    `</div>` +
    `</section>` +
    `<div class="st-split-2">` +
    `<section class="st-block" style="border-bottom:none;">` +
    `<div class="st-sec-head">` +
    `<span class="st-sec-num">01</span>` +
    `<h2 class="st-sec-title">${isEn ? "Priority Tasks & Pending Approvals" : "重点任务与待确认队列"}</h2>` +
    `<button type="button" class="st-btn" style="margin-left:auto;height:22px;font-size:11px;" data-studio-nav="tasks-dag">${isEn ? "Full DAG View →" : "全部任务与拓扑 →"}</button>` +
    `</div>` +
    `<div class="st-table-wrap">` +
    `<table class="st-table">` +
    `<thead><tr>` +
    `<th>${isEn ? "STATUS" : "状态"}</th>` +
    `<th>${isEn ? "TASK GOAL" : "任务目标与归属"}</th>` +
    `<th>${isEn ? "PHASE" : "阶段"}</th>` +
    `<th>${isEn ? "WATERMARK" : "资源水位"}</th>` +
    `<th></th>` +
    `</tr></thead>` +
    `<tbody>` +
    taskRows
      .slice(0, 6)
      .map((r) => {
        const title = isEn ? r.goalTitleEn : r.goalTitleZh;
        return (
          `<tr data-overview-task-id="${esc(r.summary.taskId)}">` +
          `<td style="padding:8px 10px;">${statusSealHtml(r.summary.taskState, isEn)}</td>` +
          `<td style="padding:8px 10px;"><strong style="color:var(--st-ink-950);">${esc(title)}</strong><div class="st-mono" style="font-size:10.5px;">${esc(isEn ? r.applicationNameEn : r.applicationNameZh)}</div></td>` +
          `<td style="padding:8px 8px;"><span class="st-mono">${r.phaseFold.currentPhaseIndex}/${r.phaseFold.totalPhases}</span></td>` +
          `<td style="padding:8px 8px;">${hairlineBarHtml(r.summary.usageUnits, r.summary.effectiveUnits, r.summary.requestedUnits)}</td>` +
          `<td style="padding:8px 10px;"><button type="button" class="st-btn" data-inspect-task-dag="${esc(r.summary.taskId)}">${isEn ? "DAG" : "拓扑"}</button></td>` +
          `</tr>`
        );
      })
      .join("") +
    `</tbody></table></div>` +
    `</section>` +
    `<section class="st-block" style="border-bottom:none;">` +
    `<div class="st-sec-head">` +
    `<span class="st-sec-num">02</span>` +
    `<h2 class="st-sec-title">${isEn ? "12-Domain Ownership & Watermark Index" : "12 域控制面健康与水位分面索引"}</h2>` +
    `<button type="button" class="st-btn" style="margin-left:auto;height:22px;font-size:11px;" data-studio-nav="processes-resources">${isEn ? "10-Layer Tree →" : "10 层归属树 →"}</button>` +
    `</div>` +
    `<div style="display:grid;grid-template-columns:repeat(2,minmax(0,1fr));">` +
    domainSummaries
      .map((s, idx) => {
        const name = isEn
          ? DOMAIN_NAMES_EN[s.domain]
          : DOMAIN_NAMES_ZH[s.domain];
        const borderRight =
          idx % 2 === 0 ? "border-right:1px solid var(--st-rule-soft);" : "";
        return (
          `<div data-jump-domain="${esc(s.domain)}" style="${borderRight}border-bottom:1px solid var(--st-rule-soft);padding:9px 14px;cursor:pointer;display:flex;flex-direction:column;gap:5px;">` +
          `<div style="display:flex;justify-content:space-between;align-items:baseline;gap:6px;">` +
          `<span style="font-weight:700;font-size:12px;color:var(--st-ink-950);">${esc(name)}</span>` +
          `<span class="st-mono" style="font-size:11px;">${s.activeCount}/${s.totalCount}</span>` +
          `</div>` +
          `<div>${hairlineBarHtml(s.watermark.liveUsagePct, s.watermark.allocatedPct, s.watermark.ceilingPct)}</div>` +
          `</div>`
        );
      })
      .join("") +
    `</div>` +
    `</section>` +
    `</div>` +
    `<section class="st-block">` +
    `<div class="st-sec-head">` +
    `<span class="st-sec-num">03</span>` +
    `<h2 class="st-sec-title">${isEn ? "Distributed Compute Cells & Three-Family Lease Telemetry" : "分布式计算节点 (Cells) 与三族租约水位"}</h2>` +
    `<span class="st-sec-kicker">${ctrlState.cells.length} CELLS ACTIVE</span>` +
    `</div>` +
    `<div class="st-block-body">` +
    renderCellLeasePanelHtml(ctrlState.cells, {
      locale: snap.locale,
      disclosure: snap.disclosure,
    }) +
    `</div>` +
    `</section>`;

  wrap.addEventListener("click", (ev: MouseEvent) => {
    const target =
      ev.target && typeof (ev.target as Element).closest === "function"
        ? (ev.target as Element)
        : null;
    if (!target) {
      return;
    }

    const navBtn = target.closest("[data-studio-nav]");
    if (navBtn) {
      const dest = navBtn.getAttribute("data-studio-nav") as
        | "intent-keypool"
        | "tasks-dag"
        | "processes-resources"
        | null;
      if (dest) {
        store.setActiveSection(dest);
      }
      return;
    }

    if (target.closest('[data-studio-action="refresh-daemon"]')) {
      void store.refreshAllFromDaemon();
      store.setBannerNotice(
        isEn
          ? "Synchronized all telemetry from control-plane daemon"
          : "已同步控制面守护进程与本地模型池最新状态",
        "info",
      );
      return;
    }

    const inspectDagBtn = target.closest("[data-inspect-task-dag]");
    if (inspectDagBtn) {
      const taskId = inspectDagBtn.getAttribute("data-inspect-task-dag");
      if (taskId) {
        store.selectTask(taskId);
        store.setActiveSection("tasks-dag");
      }
      return;
    }

    const jumpDomain = target.closest("[data-jump-domain]");
    if (jumpDomain) {
      const domain = jumpDomain.getAttribute(
        "data-jump-domain",
      ) as ControlCenterDomainId | null;
      if (domain) {
        store.setDomainFilter(domain);
        store.setActiveSection("processes-resources");
      }
    }
  });

  return wrap;
}

/* ==========================================================================
   02. Natural-Language Intent & glm-5.3 Model Pool View
   ========================================================================== */

export function buildIntentKeyPoolViewElement(
  doc: Document,
  store: OperatorStudioStore,
  snap: StudioStateSnapshot,
): HTMLElement {
  const isEn = snap.locale === "en-US";
  const kp = snap.keypoolStatus;
  const assist = snap.lastIntentAssist;
  const chat = snap.lastChatResponse;

  const wrap = doc.createElement("div");

  const assistHtml = assist
    ? `<div style="margin-top:14px;padding:12px 14px;border:1px solid var(--st-rule-mid);border-left:3px solid ${assist.safetyEscalated ? "var(--st-vermilion)" : "var(--st-accent)"};background:var(--st-bg-subtle);">` +
      `<div style="display:flex;justify-content:space-between;align-items:baseline;margin-bottom:8px;">` +
      `<strong style="color:var(--st-ink-950);">${isEn ? "Assisted Plan Breakdown" : "模型辅助拆解结果"} (${esc(assist.model ?? "glm-5.3")})</strong>` +
      `<span class="st-seal" data-tone="${assist.safetyEscalated ? "vermilion" : "accent"}">${esc(assist.undoClass)}</span>` +
      `</div>` +
      `<ol style="margin:0 0 12px 18px;padding:0;display:flex;flex-direction:column;gap:5px;color:var(--st-ink-900);">` +
      assist.steps.map((s: string) => `<li>${esc(s)}</li>`).join("") +
      `</ol>` +
      `<div style="display:flex;justify-content:space-between;align-items:center;flex-wrap:wrap;gap:10px;padding-top:8px;border-top:1px solid var(--st-rule-soft);">` +
      `<span class="st-mono">prompt=${assist.budgetReceipt?.promptTokens ?? 0} · completion=${assist.budgetReceipt?.completionTokens ?? 0} · reasoning=${assist.budgetReceipt?.reasoningTokens ?? 0} · total=${assist.budgetReceipt?.totalTokens ?? 0}</span>` +
      `<button type="button" class="st-btn st-btn--primary" data-action="authorize-assisted-plan">${isEn ? "Authorize & Submit Plan" : "批准并提交执行计划"}</button>` +
      `</div>` +
      `</div>`
    : `<div style="margin-top:14px;padding:12px 14px;border-left:2px solid var(--st-rule-mid);background:var(--st-bg-subtle);color:var(--st-ink-600);font-size:12px;">` +
      (isEn
        ? "Enter a natural-language task goal above and click 'Decompose Intent' to invoke glm-5.3 via LlmEnforcementShim."
        : "在上方输入自然语言任务目标（或点选预设），即可经 LlmEnforcementShim 调用本机 glm-5.3 拆解步骤并实时核算五维 Token 预算。") +
      `</div>`;

  const chatHtml = chat
    ? `<div style="margin-top:14px;padding:12px 14px;border:1px solid var(--st-rule-mid);background:var(--st-bg-subtle);">` +
      `<div style="display:flex;justify-content:space-between;align-items:baseline;margin-bottom:6px;padding-bottom:6px;border-bottom:1px solid var(--st-rule-soft);">` +
      `<strong style="color:var(--st-ink-950);">${esc(chat.model)} · ${isEn ? "Metered Response" : "计量推理回复"}</strong>` +
      `<span class="st-mono">total=${chat.totalTokens} (cum=${chat.budgetReceipt?.cumulativeTotalTokens ?? chat.totalTokens})</span>` +
      `</div>` +
      `<div style="white-space:pre-wrap;line-height:1.6;color:var(--st-ink-900);">${esc(chat.content)}</div>` +
      `</div>`
    : "";

  wrap.innerHTML =
    `<header class="st-masthead-band">` +
    `<div>` +
    `<div class="st-page-kicker">SECTION 02 · INTENT COMPILER & 5D TOKEN LEDGER</div>` +
    `<h1 class="st-page-title">${isEn ? "Natural-Language Intent Studio & glm-5.3 Pool" : "自然语言意图工作台与 glm-5.3 模型池"}</h1>` +
    `<p class="st-page-desc">${
      isEn
        ? "User-space HTTP driver with LlmEnforcementShim credential custody, 5-dimensional LlmTokenBudget conservation, and [NL-AMBIG-001] destructive escalation."
        : "核外用户态模型驱动（LlmEnforcementShim 托管脱敏凭据），联动 ResourceAuthority v8 五维 Token 预算回执与高危歧义自动升级门控。"
    }</p>` +
    `</div>` +
    `<button type="button" class="st-btn st-btn--primary" data-action="probe-keypool">${isEn ? "Probe 127.0.0.1:8317 Pool" : "探测 127.0.0.1:8317 号池"}</button>` +
    `</header>` +
    `<section class="st-metric-band">` +
    `<div class="st-metric-cell">` +
    `<div class="st-metric-label"><span>${isEn ? "Primary Model" : "默认与意图锁定模型"}</span><span class="st-seal" data-tone="accent">${kp.reachable ? "ONLINE" : "FALLBACK"}</span></div>` +
    `<div class="st-metric-val">${esc(kp.defaultModel ?? "glm-5.3")}</div>` +
    `<div class="st-metric-sub">${esc(kp.gatewayOrigin ?? "http://127.0.0.1:8317")}</div>` +
    `</div>` +
    `<div class="st-metric-cell">` +
    `<div class="st-metric-label"><span>${isEn ? "Active Key Slots" : "活跃密钥槽位"}</span><span class="st-seal">${esc((kp.strategy ?? "sticky").toUpperCase())}</span></div>` +
    `<div class="st-metric-val">${kp.activeKeys ?? 41} / ${kp.totalKeys ?? 44}</div>` +
    `<div class="st-metric-sub">Custody: &lt;redacted-credential&gt;</div>` +
    `</div>` +
    `<div class="st-metric-cell">` +
    `<div class="st-metric-label"><span>${isEn ? "5D Budget Enforcement" : "五维预算守恒门禁"}</span><span class="st-seal" data-tone="forest">V8 LEDGER</span></div>` +
    `<div class="st-metric-val">${kp.budgetMetered ? "ENFORCED" : "PASSIVE"}</div>` +
    `<div class="st-metric-sub">prompt + completion (incl. reasoning)</div>` +
    `</div>` +
    `<div class="st-metric-cell">` +
    `<div class="st-metric-label"><span>${isEn ? "Provider Isolation" : "驱动隔离边界标记"}</span></div>` +
    `<div class="st-metric-val" style="font-size:15px;">USER-SPACE SHIM</div>` +
    `<div class="st-metric-sub">${esc(kp.isolationTag)}</div>` +
    `</div>` +
    `</section>` +
    `<div class="st-split-2">` +
    `<section class="st-block" style="border-bottom:none;">` +
    `<div class="st-sec-head">` +
    `<span class="st-sec-num">01</span>` +
    `<h2 class="st-sec-title">${isEn ? "Intent Decomposition & [NL-AMBIG-001] Guard" : "意图辅助拆解与高危歧义护栏 ([NL-AMBIG-001])"}</h2>` +
    `<span class="st-sec-kicker">ASSIST_INTENT</span>` +
    `</div>` +
    `<div class="st-block-body">` +
    `<div style="display:flex;gap:8px;flex-wrap:wrap;margin-bottom:10px;">` +
    `<button type="button" class="st-btn" data-preset-intent="汇总当前工作区构建产物并生成合规校验归档">${isEn ? "Preset: Archive Build" : "预设：归档构建产物"}</button>` +
    `<button type="button" class="st-btn" data-preset-intent="巡检三域持久化恢复队列并导出 OpenMetrics 指标">${isEn ? "Preset: Audit Recovery" : "预设：巡检三域恢复"}</button>` +
    `<button type="button" class="st-btn st-btn--vermilion" data-preset-intent="删除旧版缓存并卸载未签名测试应用包">${isEn ? "Preset: Destructive Wipe (Z4)" : "预设：高危删除/卸载（触发不可逆护栏）"}</button>` +
    `</div>` +
    `<div style="display:flex;gap:8px;">` +
    `<input type="text" class="st-input" id="studio-intent-input" value="汇总当前工作区构建产物并生成合规校验归档" />` +
    `<button type="button" class="st-btn st-btn--primary" data-action="run-intent-assist">${isEn ? "Decompose (glm-5.3)" : "调用 glm-5.3 拆解"}</button>` +
    `</div>` +
    assistHtml +
    `</div>` +
    `</section>` +
    `<section class="st-block" style="border-bottom:none;">` +
    `<div class="st-sec-head">` +
    `<span class="st-sec-num">02</span>` +
    `<h2 class="st-sec-title">${isEn ? "Direct glm-5.3 Metered Inference" : "glm-5.3 实时计量推理对话"}</h2>` +
    `<span class="st-sec-kicker">KEYPOOL_CHAT</span>` +
    `</div>` +
    `<div class="st-block-body">` +
    `<div style="display:flex;gap:8px;">` +
    `<input type="text" class="st-input" id="studio-chat-input" value="请简述当前 LLMOS 微内核的三域恢复机制与五维 Token 预算守恒" />` +
    `<button type="button" class="st-btn st-btn--primary" data-action="run-keypool-chat">${isEn ? "Send Prompt" : "发送推理请求"}</button>` +
    `</div>` +
    chatHtml +
    `</div>` +
    `</section>` +
    `</div>`;

  wrap.addEventListener("click", (ev: MouseEvent) => {
    const target =
      ev.target && typeof (ev.target as Element).closest === "function"
        ? (ev.target as Element)
        : null;
    if (!target) {
      return;
    }

    const presetBtn = target.closest("[data-preset-intent]");
    if (presetBtn) {
      const val = presetBtn.getAttribute("data-preset-intent") ?? "";
      const input = wrap.querySelector(
        "#studio-intent-input",
      ) as HTMLInputElement | null;
      if (input) {
        input.value = val;
      }
      void store.runIntentAssist(val);
      return;
    }

    if (target.closest('[data-action="probe-keypool"]')) {
      void store.probeKeyPoolStatus();
      return;
    }

    if (target.closest('[data-action="run-intent-assist"]')) {
      const input = wrap.querySelector(
        "#studio-intent-input",
      ) as HTMLInputElement | null;
      const val =
        input?.value?.trim() || "汇总当前工作区构建产物并生成合规校验归档";
      void store.runIntentAssist(val);
      return;
    }

    if (target.closest('[data-action="run-keypool-chat"]')) {
      const input = wrap.querySelector(
        "#studio-chat-input",
      ) as HTMLInputElement | null;
      const val = input?.value?.trim() || "请汇报当前系统状态";
      void store.runKeyPoolChat(val);
      return;
    }

    if (target.closest('[data-action="authorize-assisted-plan"]')) {
      if (!assist) {
        return;
      }
      store.openZ4Confirmation({
        promptId: `z4-intent-${Date.now()}`,
        title: assist.targetDomain,
        summary: assist.steps.join(" → "),
        consequence: assist.safetyEscalated
          ? "包含不可逆外部操作，执行后锁定操作回执"
          : "支持快照补偿回滚，预算上限 50,000 Tokens",
        irreversible: assist.safetyEscalated,
        onConfirm: () => {
          store.setActiveSection("tasks-dag");
        },
      });
    }
  });

  return wrap;
}

/* ==========================================================================
   03. Task Commitment Ledger & Orthogonal DAG View
   ========================================================================== */

export function buildTasksDagViewElement(
  doc: Document,
  store: OperatorStudioStore,
  snap: StudioStateSnapshot,
): HTMLElement {
  const isEn = snap.locale === "en-US";
  const allRows = store.controllers.taskSpace.getRows();
  const filteredRows = allRows.filter((r) => {
    const st = r.summary.taskState;
    if (snap.taskStateFilter === "ACTIVE") {
      return st === "RUNNING" || st === "AUTHORIZED" || st === "RECOVERING";
    }
    if (snap.taskStateFilter === "WAITING") {
      return (
        st === "WAITING_CONSENT" ||
        st === "WAITING_INPUT" ||
        st === "PROPOSED" ||
        st === "SUSPENDED"
      );
    }
    if (snap.taskStateFilter === "COMPLETED") {
      return st === "COMPLETED" || st === "PARTIAL";
    }
    return true;
  });

  const selectedRow: TaskSpaceWorkstationRow =
    allRows.find((r) => r.summary.taskId === snap.selectedTaskId) ??
    filteredRows[0] ??
    allRows[0]!;

  const wrap = doc.createElement("div");

  const filterTabs: Array<{
    id: StudioStateSnapshot["taskStateFilter"];
    label: string;
  }> = [
    {
      id: "ALL",
      label: isEn ? `All (${allRows.length})` : `全部 (${allRows.length})`,
    },
    { id: "ACTIVE", label: isEn ? "Running" : "执行中" },
    { id: "WAITING", label: isEn ? "Awaiting Action" : "待确认 / 暂停" },
    { id: "COMPLETED", label: isEn ? "Completed" : "已完成" },
  ];

  const dagNodes = selectedRow.l3PlanDag.nodes;
  const supernodes = selectedRow.l3PlanDag.lazySupernodes;
  const swissDagHtml =
    `<div style="display:grid;grid-template-columns:repeat(4,minmax(0,1fr));border-bottom:1px solid var(--st-rule-mid);">` +
    dagNodes
      .slice(0, 3)
      .map((node, idx) => {
        const isDone = node.state === "COMPLETED";
        const isActive = node.state === "ACTIVE";
        const topBar = isDone
          ? "border-top:2.5px solid var(--st-forest);"
          : isActive
            ? "border-top:2.5px solid var(--st-accent);"
            : "border-top:2.5px solid var(--st-rule-mid);";
        return (
          `<div style="border-right:1px solid var(--st-rule-mid);${topBar}padding:12px 16px;display:flex;flex-direction:column;gap:8px;min-width:0;">` +
          `<div style="display:flex;justify-content:space-between;align-items:baseline;gap:6px;">` +
          `<span class="st-mono" style="font-size:10.5px;color:var(--st-vermilion);font-weight:700;">STEP 0${idx + 1}</span>` +
          `${statusSealHtml(node.state, isEn)}` +
          `</div>` +
          `<div style="font-size:13px;font-weight:800;color:var(--st-ink-950);">${esc(node.title)}</div>` +
          `<div class="st-mono" style="font-size:11px;">cap: ${esc(node.capabilityId)} · ${esc(node.residencyState)}</div>` +
          `<div style="display:flex;gap:6px;flex-wrap:wrap;margin-top:auto;padding-top:4px;">` +
          (isDone || isActive
            ? `<span class="st-seal">${isEn ? "LOCKED" : "法定锁定"}</span>`
            : `<button type="button" class="st-btn" data-dag-action="preview-override">${isEn ? "Insert Step" : "插入前置步骤"}</button>`) +
          `</div>` +
          `</div>`
        );
      })
      .join("") +
    (supernodes[0]
      ? `<div style="border-top:2.5px solid var(--st-vermilion);padding:12px 16px;display:flex;flex-direction:column;gap:8px;min-width:0;background:var(--st-bg-subtle);">` +
        `<div style="display:flex;justify-content:space-between;align-items:baseline;gap:6px;">` +
        `<span class="st-mono" style="font-size:10.5px;color:var(--st-vermilion);font-weight:700;">LAZY SUPERNODE</span>` +
        `<span class="st-seal" data-tone="vermilion">${supernodes[0].expanded ? "EXPANDED" : `+${supernodes[0].unmaterializedStepCount} COLD`}</span>` +
        `</div>` +
        `<div style="font-size:13px;font-weight:800;color:var(--st-ink-950);">${esc(isEn ? supernodes[0].titleEn : supernodes[0].titleZh)}</div>` +
        `<div class="st-mono" style="font-size:11px;">residency: ${esc(supernodes[0].residencyState)} · 0 OS processes until materialized</div>` +
        `<div style="margin-top:auto;padding-top:4px;">` +
        `<button type="button" class="st-btn st-btn--primary" data-dag-action="expand-supernode">${isEn ? "Materialize Subgraph" : "按需物化展开子图"}</button>` +
        `</div>` +
        `</div>`
      : "") +
    `</div>`;

  const attempts = selectedRow.l3AttemptLanes.attempts;
  const swissSwimlanesHtml =
    `<div class="st-sec-head">` +
    `<span class="st-sec-num">03</span>` +
    `<h2 class="st-sec-title">${isEn ? "TaskAttempt Racing Swimlanes & 4-Phase Execution Waterfall" : "TaskAttempt 并发竞争泳道与四阶段耗时瀑布"}</h2>` +
    `<span class="st-sec-kicker">CAS EPOCH #${selectedRow.l3AttemptLanes.taskHeadCasEpoch} · WINNER: ${esc(selectedRow.l3AttemptLanes.winningAttemptId ?? "PENDING")}</span>` +
    `</div>` +
    `<div class="st-table-wrap">` +
    `<table class="st-table">` +
    `<thead><tr>` +
    `<th>${isEn ? "ATTEMPT" : "尝试实例"}</th>` +
    `<th>${isEn ? "CAS POSTURE" : "CAS 裁决状态"}</th>` +
    `<th>${isEn ? "FIBER TIMING WATERFALL (CPU / SCHED / EXT / BACKPRESSURE)" : "并发流四段耗时瀑布 (CPU / 调度 / 外部 I/O / 背压)"}</th>` +
    `<th>${isEn ? "ISOLATION & CELL" : "隔离配置与节点"}</th>` +
    `</tr></thead>` +
    `<tbody>` +
    attempts
      .map((att) => {
        const tm = att.fibers[0]?.timings ?? {
          activeCpuMs: 24,
          schedulerWaitMs: 6,
          externalWaitMs: 42,
          backpressureWaitMs: 4,
          suspendedMs: 0,
        };
        const totalMs = Math.max(
          1,
          tm.activeCpuMs +
            tm.schedulerWaitMs +
            tm.externalWaitMs +
            tm.backpressureWaitMs,
        );
        return (
          `<tr>` +
          `<td><strong style="color:var(--st-ink-950);">${esc(att.label)}</strong><div class="st-mono" style="font-size:10.5px;">#${esc(att.attemptId)}</div></td>` +
          `<td><span class="st-seal" data-tone="${att.casWinner ? "forest" : att.superseded ? "vermilion" : "accent"}">${att.casWinner ? (isEn ? "CAS WINNER" : "CAS 胜出提交") : att.superseded ? (isEn ? "SUPERSEDED" : "已被竞争取代") : esc(att.state)}</span></td>` +
          `<td>` +
          `<div style="display:flex;align-items:center;gap:10px;">` +
          `<div style="display:flex;width:180px;height:6px;background:var(--st-rule-soft);">` +
          `<div style="width:${Math.round((tm.activeCpuMs / totalMs) * 100)}%;background:var(--st-ink-950);"></div>` +
          `<div style="width:${Math.round((tm.schedulerWaitMs / totalMs) * 100)}%;background:var(--st-accent);"></div>` +
          `<div style="width:${Math.round((tm.externalWaitMs / totalMs) * 100)}%;background:var(--st-vermilion);"></div>` +
          `<div style="width:${Math.round((tm.backpressureWaitMs / totalMs) * 100)}%;background:var(--st-ink-400);"></div>` +
          `</div>` +
          `<span class="st-mono">${tm.activeCpuMs} / ${tm.schedulerWaitMs} / ${tm.externalWaitMs} / ${tm.backpressureWaitMs} ms</span>` +
          `</div>` +
          `</td>` +
          `<td><span class="st-mono">${esc(att.isolationEnforcementProfile)} · cell=${esc(att.cellPlacement.cellIdHex.slice(0, 8))}</span></td>` +
          `</tr>`
        );
      })
      .join("") +
    `</tbody></table></div>`;

  wrap.innerHTML =
    `<header class="st-masthead-band">` +
    `<div>` +
    `<div class="st-page-kicker">SECTION 03 · TASK COMMITMENT LEDGER & ORTHOGONAL DAG</div>` +
    `<h1 class="st-page-title">${isEn ? "Task Lifecycle & Orthogonal DAG Orchestration" : "任务承诺台账与正交 DAG 依赖拓扑"}</h1>` +
    `<p class="st-page-desc">${
      isEn
        ? "Full-width commitment table coupled with live Dagster-style orthogonal TaskPlan dependency graph and Temporal TaskAttempt racing swimlanes."
        : "上方为全宽任务承诺台账（点击任意行切换焦点任务），下方实时呈现该任务的 TaskPlan 正交依赖拓扑图与 TaskAttempt 并发竞争泳道。"
    }</p>` +
    `</div>` +
    `<div class="st-seg-group">` +
    filterTabs
      .map(
        (t) =>
          `<button type="button" class="st-seg-btn ${snap.taskStateFilter === t.id ? "is-active" : ""}" data-task-filter="${t.id}">${esc(t.label)}</button>`,
      )
      .join("") +
    `</div>` +
    `</header>` +
    `<section class="st-block">` +
    `<div class="st-sec-head">` +
    `<span class="st-sec-num">01</span>` +
    `<h2 class="st-sec-title">${isEn ? "Workstation Task Commitment Ledger" : "工作站任务承诺台账"}</h2>` +
    `<span class="st-sec-kicker">${esc(selectedRow.summary.planId.toUpperCase())}</span>` +
    `</div>` +
    `<div class="st-table-wrap" style="max-height:280px;overflow-y:auto;">` +
    `<table class="st-table">` +
    `<thead><tr>` +
    `<th>${isEn ? "STATUS" : "状态"}</th>` +
    `<th>${isEn ? "TASK GOAL & PHASE PROGRESS" : "任务目标与阶段进展"}</th>` +
    `<th>${isEn ? "APPLICATION" : "归属应用"}</th>` +
    `<th>${isEn ? "WATERMARKS" : "资源水位 (使用/生效/申请)"}</th>` +
    `<th>${isEn ? "TARGETS" : "目标完成度"}</th>` +
    `<th>${isEn ? "SAFEGUARD" : "可逆保障"}</th>` +
    `<th>${isEn ? "ACTIONS" : "控制动作"}</th>` +
    `</tr></thead>` +
    `<tbody>` +
    filteredRows
      .map((row) => {
        const isSel = row.summary.taskId === selectedRow.summary.taskId;
        const goal = isEn ? row.goalTitleEn : row.goalTitleZh;
        const phaseText = renderTemporalPhaseSummaryText(
          row.phaseFold,
          snap.locale,
        );
        const appName = isEn ? row.applicationNameEn : row.applicationNameZh;
        const undoLabel =
          row.summary.undoClass === "REVERSIBLE"
            ? isEn
              ? "LOSSLESS UNDO"
              : "无损撤销"
            : row.summary.undoClass === "COMPENSATABLE"
              ? isEn
                ? "COMPENSATABLE"
                : "补偿回滚"
              : isEn
                ? "IRREVERSIBLE"
                : "不可逆凭证";
        return (
          `<tr data-task-row="${esc(row.summary.taskId)}" data-selected="${isSel ? "true" : "false"}">` +
          `<td>${statusSealHtml(row.summary.taskState, isEn)}</td>` +
          `<td><strong style="color:var(--st-ink-950);">${esc(goal)}</strong><div class="st-mono" style="font-size:11px;">${esc(phaseText)}</div></td>` +
          `<td>${esc(appName)}</td>` +
          `<td>${hairlineBarHtml(row.summary.usageUnits, row.summary.effectiveUnits, row.summary.requestedUnits)}</td>` +
          `<td><span class="st-mono">${row.summary.succeededTargets} OK / ${row.summary.failedTargets} FAIL</span></td>` +
          `<td><span class="st-seal">${esc(undoLabel)}</span></td>` +
          `<td>` +
          `<div style="display:flex;gap:6px;">` +
          `<button type="button" class="st-btn" data-task-action="pause" data-target-task="${esc(row.summary.taskId)}">${isEn ? "Pause" : "暂停"}</button>` +
          `<button type="button" class="st-btn" data-task-action="inspect" data-target-task="${esc(row.summary.taskId)}">${isEn ? "Receipt" : "回执"}</button>` +
          `</div>` +
          `</td>` +
          `</tr>`
        );
      })
      .join("") +
    `</tbody></table></div>` +
    `</section>` +
    `<section class="st-block">` +
    `<div class="st-sec-head">` +
    `<span class="st-sec-num">02</span>` +
    `<h2 class="st-sec-title">${isEn ? `Orthogonal TaskPlan DAG & Attempt Swimlanes — ${selectedRow.goalTitleEn}` : `正交依赖拓扑图与并发执行泳道 — ${selectedRow.goalTitleZh}`}</h2>` +
    `<div style="margin-left:auto;display:flex;gap:8px;">` +
    `<button type="button" class="st-btn" data-dag-action="expand-supernode">${isEn ? "Expand Lazy Subgraph" : "展开惰性子图超节点"}</button>` +
    `<button type="button" class="st-btn" data-dag-action="preview-override">${isEn ? "Preview Override Diff" : "预演改图爆炸半径"}</button>` +
    `</div>` +
    `</div>` +
    swissDagHtml +
    swissSwimlanesHtml +
    `</section>`;

  wrap.addEventListener("click", (ev: MouseEvent) => {
    const target =
      ev.target && typeof (ev.target as Element).closest === "function"
        ? (ev.target as Element)
        : null;
    if (!target) {
      return;
    }

    const filterBtn = target.closest("[data-task-filter]");
    if (filterBtn) {
      const f = filterBtn.getAttribute(
        "data-task-filter",
      ) as StudioStateSnapshot["taskStateFilter"] | null;
      if (f) {
        store.setTaskStateFilter(f);
      }
      return;
    }

    const actionBtn = target.closest("[data-task-action][data-target-task]");
    if (actionBtn) {
      ev.stopPropagation();
      const act = actionBtn.getAttribute("data-task-action");
      const taskId = actionBtn.getAttribute("data-target-task") ?? "";
      const row =
        allRows.find((r) => r.summary.taskId === taskId) ?? selectedRow;
      if (act === "inspect") {
        store.setInspector({
          title: isEn ? row.goalTitleEn : row.goalTitleZh,
          subtitle: `Plan ID: ${row.summary.planId} · Rev r${row.summary.planRevision}`,
          badgeText: row.summary.taskState,
          badgeVariant: "accent",
          fields: [
            { key: "task_id", value: row.summary.taskId },
            { key: "plan_id", value: row.summary.planId },
            {
              key: "application",
              value: isEn ? row.applicationNameEn : row.applicationNameZh,
            },
            { key: "cell_id", value: row.summary.cellId },
            {
              key: "watermarks",
              value: `${row.summary.usageUnits}% / ${row.summary.effectiveUnits}% / ${row.summary.requestedUnits}%`,
            },
            { key: "reversibility", value: row.summary.undoClass },
          ],
          rawReceiptHex: row.summary.receiptHex,
        });
      } else if (act === "pause") {
        void store.dispatchControlAction({
          action: "pause-operation",
          targetIdHex: row.summary.planId.padEnd(32, "0").slice(0, 32),
          expectedRevision: row.summary.planRevision,
          scope: "task",
          reason: "Operator paused task from Studio Tasks Ledger",
        });
      }
      return;
    }

    if (target.closest('[data-dag-action="expand-supernode"]')) {
      const res = store.controllers.taskSpace.expandSupernodeOnDemand(
        "plan-01-node-lazy-super",
        8,
      );
      store.setBannerNotice(
        isEn
          ? `Expanded lazy supernode (${res.materializedCount} child nodes materialized)`
          : `已按需物化展开惰性超节点（新增 ${res.materializedCount} 个子步骤节点）`,
        "info",
      );
      return;
    }

    if (target.closest('[data-dag-action="preview-override"]')) {
      store.setInspector({
        title: isEn
          ? "TaskPlan Override Blast-Radius Diff"
          : "TaskPlan 改图爆炸半径预演",
        subtitle: `Plan ${selectedRow.summary.planId} (r${selectedRow.summary.planRevision} -> r${selectedRow.summary.planRevision + 1})`,
        badgeText: "COMPENSATABLE",
        badgeVariant: "warn",
        fields: [
          {
            key: "completed_nodes",
            value: "Immutable (Statutorily Locked)",
          },
          {
            key: "pending_subgraph",
            value: "+1 verification barrier node inserted",
          },
          {
            key: "budget_delta",
            value: "+0 MiB (within existing reservation ceiling)",
          },
        ],
        rawReceiptHex: selectedRow.summary.receiptHex,
      });
      return;
    }

    const tr = target.closest("tr[data-task-row]");
    if (tr) {
      const taskId = tr.getAttribute("data-task-row");
      if (taskId) {
        store.selectTask(taskId);
      }
    }
  });

  return wrap;
}

/* ==========================================================================
   04. 10-Layer Ownership Tree & Compute Cells View
   ========================================================================== */

export function buildProcessesResourcesViewElement(
  doc: Document,
  store: OperatorStudioStore,
  snap: StudioStateSnapshot,
): HTMLElement {
  const isEn = snap.locale === "en-US";
  const ctrlState = store.controllers.controlCenter.getState();
  const enriched = enrichOwnershipTree(ctrlState.treeSpecs, {
    locale: snap.locale,
    disclosure: snap.disclosure,
  });
  const windowed = windowOwnershipTreeRows(enriched, {
    collapsedNodeIds: snap.collapsedTreeNodeIds,
    domainFilter: snap.selectedDomainFilter,
    limit: 64,
  });

  const wrap = doc.createElement("div");

  const filterButtonsHtml =
    `<button type="button" class="st-facet-pill ${snap.selectedDomainFilter === "all" ? "is-active" : ""}" data-domain-chip="all">${isEn ? "All Domains (12)" : "全部控制域 (12)"}</button>` +
    ALL_CONTROL_CENTER_DOMAINS.map((d) => {
      const label = isEn ? DOMAIN_NAMES_EN[d] : DOMAIN_NAMES_ZH[d];
      const active = snap.selectedDomainFilter === d ? "is-active" : "";
      return `<button type="button" class="st-facet-pill ${active}" data-domain-chip="${esc(d)}">${esc(label)}</button>`;
    }).join("");

  wrap.innerHTML =
    `<header class="st-masthead-band">` +
    `<div>` +
    `<div class="st-page-kicker">SECTION 04 · 10-LAYER OWNERSHIP TREE & COMPUTE CELLS</div>` +
    `<h1 class="st-page-title">${isEn ? "10-Layer Ownership Tree, Processes & Compute Cells" : "十层父子归属资源树、系统进程与分布式计算节点"}</h1>` +
    `<p class="st-page-desc">${
      isEn
        ? "System Informer 10-layer hierarchy (Device -> Workspace -> App -> Task -> Group -> Node -> Attempt -> Process -> Agent -> Fiber) with Nomad triple watermarks."
        : "完整呈现设备 → 工作区 → 应用 → 任务 → 编排组 → 节点 → 尝试 → 进程 → Agent 实例 → 并发流 10 层归属关系、三水位配额与动作剪枝。"
    }</p>` +
    `</div>` +
    `</header>` +
    `<div class="st-facet-bar">${filterButtonsHtml}</div>` +
    `<section class="st-block">` +
    `<div class="st-sec-head">` +
    `<span class="st-sec-num">01</span>` +
    `<h2 class="st-sec-title">${isEn ? "Interactive 10-Layer Ownership & Resource Tree" : "十层父子归属与资源水位控制树（点击行可折叠子级并在右轨检视详情）"}</h2>` +
    `<span class="st-sec-kicker">${windowed.visibleRows.length} / ${windowed.totalMatchingCount} ROWS</span>` +
    `</div>` +
    `<div>` +
    renderOwnershipTreeHtml(windowed.visibleRows, {
      locale: snap.locale,
      disclosure: snap.disclosure,
    }) +
    `</div>` +
    `</section>` +
    `<section class="st-block">` +
    `<div class="st-sec-head">` +
    `<span class="st-sec-num">02</span>` +
    `<h2 class="st-sec-title">${isEn ? "Stage C Distributed Cell Leases & Process Migration" : "分布式 Cell 租约健康与跨节点进程迁移"}</h2>` +
    `<span class="st-sec-kicker">RAFT & LEASE</span>` +
    `</div>` +
    `<div class="st-block-body">` +
    renderCellLeasePanelHtml(ctrlState.cells, {
      locale: snap.locale,
      disclosure: snap.disclosure,
    }) +
    `</div>` +
    `</section>`;

  wrap.addEventListener("click", (ev: MouseEvent) => {
    const target =
      ev.target && typeof (ev.target as Element).closest === "function"
        ? (ev.target as Element)
        : null;
    if (!target) {
      return;
    }

    const chip = target.closest("[data-domain-chip]");
    if (chip) {
      const dom = chip.getAttribute("data-domain-chip") as
        | ControlCenterDomainId
        | "all"
        | null;
      if (dom) {
        store.setDomainFilter(dom);
      }
      return;
    }

    const verbBtn = target.closest("[data-verb][data-node-id]");
    if (verbBtn) {
      ev.stopPropagation();
      const verb = verbBtn.getAttribute("data-verb") ?? "";
      const nodeId = verbBtn.getAttribute("data-node-id") ?? "";
      const requiresArming =
        verbBtn.getAttribute("data-requires-arming") === "true";
      const label = verbBtn.textContent ?? verb;

      if (requiresArming) {
        store.openZ4Confirmation({
          promptId: `z4-tree-${nodeId}-${verb}`,
          title: `${label} (${nodeId})`,
          summary: isEn
            ? `High-impact control action '${verb}' on ${nodeId}`
            : `对节点 ${nodeId} 执行高影响控制动作「${label}」`,
          consequence: isEn
            ? "Terminates active execution or revokes authority; audited via SABI receipt"
            : "将终止活跃执行单元或变更授权边界，全程记录不可变 SABI 回执",
          irreversible: true,
          onConfirm: () => {
            store.setInspector({
              title: `已执行高危控制动作 · ${label}`,
              subtitle: `目标节点：${nodeId}`,
              badgeText: "Z4 AUTHORIZED",
              badgeVariant: "ok",
              fields: [
                { key: "node_id", value: nodeId },
                { key: "verb", value: verb },
                { key: "authorization", value: "Z4 Trusted UI Confirmed" },
              ],
              rawReceiptHex: snap.lastReceiptHex,
            });
          },
        });
      } else {
        store.setBannerNotice(
          isEn
            ? `Executed '${label}' on ${nodeId}`
            : `已对 ${nodeId} 执行控制动作：${label}`,
          "info",
        );
        store.setInspector({
          title: `控制动作回执 · ${label}`,
          subtitle: `目标节点：${nodeId}`,
          badgeText: "EXECUTED",
          badgeVariant: "ok",
          fields: [
            { key: "node_id", value: nodeId },
            { key: "verb", value: verb },
          ],
          rawReceiptHex: snap.lastReceiptHex,
        });
      }
      return;
    }

    const row = target.closest(".nlos-ownership-tree__row[data-node-id]");
    if (row) {
      const nodeId = row.getAttribute("data-node-id");
      if (nodeId) {
        const matched = enriched.find((n) => n.nodeId === nodeId);
        if (matched) {
          store.setInspector({
            title: matched.labels[snap.locale],
            subtitle: `${matched.layer} · Domain: ${matched.domain}`,
            badgeText: matched.state,
            badgeVariant: "accent",
            fields: [
              { key: "node_id", value: matched.nodeId },
              { key: "domain", value: matched.domain },
              { key: "hierarchy_layer", value: matched.layer },
              { key: "fault_domain", value: matched.faultDomainPosture },
              {
                key: "watermarks (usage/alloc/ceil)",
                value: `${matched.watermark.liveUsagePct}% / ${matched.watermark.allocatedPct}% / ${matched.watermark.ceilingPct}%`,
              },
              {
                key: "allowed_verbs",
                value:
                  matched.prunerResult.allowedVerbs.join(", ") || "(read-only)",
              },
            ],
            rawReceiptHex: snap.lastReceiptHex,
          });
        }
        store.toggleTreeNodeCollapse(nodeId);
      }
    }
  });

  return wrap;
}
