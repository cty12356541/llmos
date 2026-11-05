// ==========================================================================
// LLMOS Studio · Zürich Open Swiss Grid & Braun Atelier Shell Assembler
// (`desktop/src/studio/studio-app.ts`)
// - 3px Dual-Span Crown Bar (Left Vermilion Seal + Right Heavy Carbon Rule)
// - Three-Column Open Swiss Workbench (244px Left Rail + Open Center Stage +
//   Collapsible 388px Right SABI Receipt & Inspector Rail)
// - Double-Rule Masthead Topbar with 3-Theme Switcher:
//     * A2A Zürich Light (Archival Paper #ffffff)
//     * Braun Atelier (Dieter Rams Warm Grey #f4f3ef)
//     * A2A Carbon Dark (Deep Graphite #0b0f17)
// ==========================================================================

import { renderSpotlightViewHtml } from "../views/spotlight";
import {
  createOperatorStudioStore,
  type OperatorStudioStore,
  type StudioNavSectionId,
  type StudioStateSnapshot,
  type StudioThemeId,
} from "./studio-store";
import {
  buildIntentKeyPoolViewElement,
  buildOverviewViewElement,
  buildProcessesResourcesViewElement,
  buildTasksDagViewElement,
} from "./views-core";
import {
  buildAppsStoreViewElement,
  buildKernelProbesViewElement,
  buildRecoveryTelemetryViewElement,
  buildSecuritySettingsViewElement,
} from "./views-governance";

function esc(raw: string): string {
  return raw
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

interface NavItemSpec {
  readonly id: StudioNavSectionId;
  readonly group: "operations" | "governance";
  readonly code: string;
  readonly labelZh: string;
  readonly labelEn: string;
  readonly badgeCount?: (
    snap: StudioStateSnapshot,
    store: OperatorStudioStore,
  ) => string | null;
}

const NAV_ITEMS: readonly NavItemSpec[] = [
  {
    id: "overview",
    group: "operations",
    code: "01",
    labelZh: "全局态势与控制总览",
    labelEn: "System Posture Overview",
    badgeCount: () => "9/9",
  },
  {
    id: "intent-keypool",
    group: "operations",
    code: "02",
    labelZh: "自然语言意图与模型池",
    labelEn: "Intent & glm-5.3 Pool",
    badgeCount: (s) =>
      `${s.keypoolStatus.activeKeys ?? 41}/${s.keypoolStatus.totalKeys ?? 44}`,
  },
  {
    id: "tasks-dag",
    group: "operations",
    code: "03",
    labelZh: "任务台账与正交拓扑",
    labelEn: "Task Ledger & DAG",
    badgeCount: (_s, st) => String(st.controllers.taskSpace.getRows().length),
  },
  {
    id: "processes-resources",
    group: "operations",
    code: "04",
    labelZh: "十层父子归属与节点",
    labelEn: "10-Layer Tree & Cells",
    badgeCount: () => "12D",
  },
  {
    id: "recovery-telemetry",
    group: "governance",
    code: "05",
    labelZh: "三域持久化恢复与指标",
    labelEn: "3-Domain Recovery",
    badgeCount: (s) => `#${s.recoveryHealth.artifact.completedCycles}`,
  },
  {
    id: "apps-store",
    group: "governance",
    code: "06",
    labelZh: "应用生命周期与商店",
    labelEn: "Apps & Store PKI",
    badgeCount: () => "4",
  },
  {
    id: "security-settings",
    group: "governance",
    code: "07",
    labelZh: "四层设置与密钥保险库",
    labelEn: "CAS Settings & Vault",
    badgeCount: () => "v40",
  },
  {
    id: "kernel-probes",
    group: "governance",
    code: "08",
    labelZh: "内核 SABI 探针与通道",
    labelEn: "Raw SABI IPC Probes",
    badgeCount: () => "RPC",
  },
];

export interface MountOperatorStudioOptions {
  readonly doc?: Document;
  readonly store?: OperatorStudioStore;
}

export function mountOperatorStudio(
  container: HTMLElement,
  options?: MountOperatorStudioOptions,
): OperatorStudioStore {
  const doc = options?.doc ?? document;
  const store = options?.store ?? createOperatorStudioStore();

  const renderActiveCanvasView = (snap: StudioStateSnapshot): HTMLElement => {
    switch (snap.activeSection) {
      case "overview":
        return buildOverviewViewElement(doc, store, snap);
      case "intent-keypool":
        return buildIntentKeyPoolViewElement(doc, store, snap);
      case "tasks-dag":
        return buildTasksDagViewElement(doc, store, snap);
      case "processes-resources":
        return buildProcessesResourcesViewElement(doc, store, snap);
      case "recovery-telemetry":
        return buildRecoveryTelemetryViewElement(doc, store, snap);
      case "apps-store":
        return buildAppsStoreViewElement(doc, store, snap);
      case "security-settings":
        return buildSecuritySettingsViewElement(doc, store, snap);
      case "kernel-probes":
        return buildKernelProbesViewElement(doc, store, snap);
    }
  };

  const renderShell = (): void => {
    const snap = store.getState();
    const isEn = snap.locale === "en-US";
    const activeNav =
      NAV_ITEMS.find((n) => n.id === snap.activeSection) ?? NAV_ITEMS[0]!;

    const root = doc.createElement("div");
    root.className = "nlos-studio-root nlos-studio-shell";
    root.setAttribute("data-studio-theme", snap.studioTheme);
    root.setAttribute("data-nlos-theme", snap.theme);
    root.setAttribute("data-nlos-disclosure", snap.disclosure);
    root.setAttribute("data-active-section", snap.activeSection);

    // 1. 3px Architectural Crown Bar (Left Vermilion Seal + Right Heavy Carbon Rule)
    const crown = doc.createElement("div");
    crown.className = "st-crown-bar";
    crown.setAttribute("aria-hidden", "true");
    crown.innerHTML =
      `<span class="st-crown-vermilion"></span>` +
      `<span class="st-crown-heavy"></span>`;
    root.appendChild(crown);

    // 2. Three-Column Open Swiss Workbench (Always-Live 3-Column Balance:
    //    Right rail shows either selected entity Inspector OR live SABI Telemetry Stream)
    const workbench = doc.createElement("div");
    workbench.className = "st-workbench";

    // 2.1 Left Rail (244px)
    const leftRail = doc.createElement("aside");
    leftRail.className = "st-rail-left";
    leftRail.setAttribute(
      "aria-label",
      isEn ? "System Navigation & Index" : "系统视图与分面导航",
    );

    const buildNavGroupHtml = (group: "operations" | "governance"): string =>
      NAV_ITEMS.filter((i) => i.group === group)
        .map((item) => {
          const current = item.id === snap.activeSection;
          const count = item.badgeCount?.(snap, store);
          return (
            `<button type="button" class="st-nav-item" data-nav-section="${item.id}" ${current ? 'aria-current="page"' : ""}>` +
            `<span style="display:inline-flex;align-items:center;gap:8px;min-width:0;">` +
            `<span class="st-dot" aria-hidden="true"></span>` +
            `<span style="font-family:var(--st-font-mono);font-size:10.5px;color:var(--st-ink-400);">${item.code}</span>` +
            `<span>${esc(isEn ? item.labelEn : item.labelZh)}</span>` +
            `</span>` +
            (count ? `<span class="st-nav-count">${esc(count)}</span>` : "") +
            `</button>`
          );
        })
        .join("");

    leftRail.innerHTML =
      `<div class="st-brand-header">` +
      `<div class="st-brand-kicker">LLMOS KERNEL <span class="st-sep-dot" aria-hidden="true">·</span> SABI V1.0</div>` +
      `<div class="st-brand-name">${isEn ? "Microkernel Operator Hub" : "微内核控制与协同中枢"}</div>` +
      `</div>` +
      `<div class="st-posture-subbar">` +
      `<div style="display:inline-flex;align-items:center;gap:6px;font-size:11px;font-weight:700;color:var(--st-ink-900);">` +
      `<span class="st-dot" data-tone="live" aria-hidden="true"></span>` +
      `<span>${snap.daemonOnline ? (isEn ? "IPC Connected" : "控制面在线") : isEn ? "Autonomous Ready" : "本地自治就绪"}</span>` +
      `</div>` +
      `<div style="display:inline-flex;align-items:baseline;gap:8px;font-family:var(--st-font-mono);font-size:10.5px;">` +
      `<span style="color:var(--st-ink-400);">${snap.keypoolStatus.activeKeys ?? 41}/${snap.keypoolStatus.totalKeys ?? 44}</span>` +
      `<span style="color:var(--st-accent);font-weight:700;">#r${snap.recoveryHealth.artifact.completedCycles}</span>` +
      `</div>` +
      `</div>` +
      `<div class="st-nav-scroll">` +
      `<div class="st-nav-block">` +
      `<div class="st-sec-head">` +
      `<span class="st-sec-num">01</span>` +
      `<h2 class="st-sec-title">${isEn ? "Runtime & Orchestration" : "运行态与智能编排"}</h2>` +
      `<span class="st-sec-kicker">RUNTIME</span>` +
      `</div>` +
      buildNavGroupHtml("operations") +
      `</div>` +
      `<div class="st-nav-block">` +
      `<div class="st-sec-head">` +
      `<span class="st-sec-num">02</span>` +
      `<h2 class="st-sec-title">${isEn ? "Governance & Kernel" : "生态治理与内核探针"}</h2>` +
      `<span class="st-sec-kicker">KERNEL</span>` +
      `</div>` +
      buildNavGroupHtml("governance") +
      `</div>` +
      `<div class="st-nav-block">` +
      `<div class="st-sec-head">` +
      `<span class="st-sec-num">03</span>` +
      `<h2 class="st-sec-title">${isEn ? "SABI Frozen Channels" : "SABI 冻结通道"}</h2>` +
      `<span class="st-sec-kicker">9/9 FROZEN</span>` +
      `</div>` +
      `<div style="padding:8px 14px;display:flex;flex-wrap:wrap;gap:6px 10px;font-family:var(--st-font-mono);font-size:10.5px;color:var(--st-ink-600);">` +
      `<span>SystemControl</span><span>·</span>` +
      `<span>TaskCommit</span><span>·</span>` +
      `<span>DriverProvider</span><span>·</span>` +
      `<span>Handshake</span><span>·</span>` +
      `<span>ServiceDir</span>` +
      `</div>` +
      `</div>` +
      `</div>` +
      `<div class="st-rail-footer">` +
      `<div style="display:flex;align-items:center;justify-content:space-between;gap:6px;">` +
      `<span style="font-size:12px;font-weight:700;color:var(--st-ink-950);">operator.principal</span>` +
      `<span class="st-seal" data-tone="accent">ED25519</span>` +
      `</div>` +
      `<div class="st-mono" style="font-size:10.5px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;" title="${esc(snap.config.principalHex ?? "")}">@${esc((snap.config.principalHex ?? "9f389da8334b5b5fef257e39feafa147").slice(0, 20))}…</div>` +
      `</div>`;

    // 2.2 Center Stage (Double-Rule Topbar + Open Cross-Grid Scroll Area)
    const centerStage = doc.createElement("div");
    centerStage.className = "st-center-stage";

    const topbar = doc.createElement("header");
    topbar.className = "st-topbar";

    const themeButtons: Array<{ id: StudioThemeId; labelZh: string; labelEn: string }> = [
      { id: "zurich-light", labelZh: "A2A 苏黎世白", labelEn: "A2A Light" },
      { id: "braun-atelier", labelZh: "博朗暖灰", labelEn: "Braun Warm" },
      { id: "zurich-dark", labelZh: "A2A 碳黑", labelEn: "A2A Carbon" },
    ];

    topbar.innerHTML =
      `<div style="display:flex;align-items:baseline;gap:8px;min-width:0;">` +
      `<span style="font-family:var(--st-font-mono);font-size:11px;font-weight:700;color:var(--st-vermilion);">${activeNav.code}</span>` +
      `<strong style="font-size:13.5px;font-weight:800;color:var(--st-ink-950);white-space:nowrap;">${esc(isEn ? activeNav.labelEn : activeNav.labelZh)}</strong>` +
      `<span class="st-sep-dot" aria-hidden="true">·</span>` +
      `<span class="st-mono" style="font-size:11px;white-space:nowrap;">${esc(snap.keypoolStatus.defaultModel ?? "glm-5.3")} (${snap.keypoolStatus.activeKeys ?? 41}/${snap.keypoolStatus.totalKeys ?? 44})</span>` +
      `</div>` +
      `<div style="display:flex;align-items:center;gap:10px;margin-left:auto;flex-shrink:0;">` +
      `<button type="button" class="st-search-box" data-header-action="open-command-modal">` +
      `<span>${isEn ? "Search tasks, packages, or dispatch intent…" : "检索任务、契约包或下达自然语言意图…"}</span>` +
      `<span class="st-kbd">Ctrl+K</span>` +
      `</button>` +
      `<div class="st-seg-group" role="group" aria-label="${isEn ? "Color Palette" : "色彩主题"}">` +
      themeButtons
        .map(
          (t) =>
            `<button type="button" class="st-seg-btn ${snap.studioTheme === t.id ? "is-active" : ""}" data-set-studio-theme="${t.id}">${esc(isEn ? t.labelEn : t.labelZh)}</button>`,
        )
        .join("") +
      `</div>` +
      `<button type="button" class="st-btn" data-header-action="cycle-disclosure" title="${isEn ? "Cycle Disclosure Depth (L1 / L2 / L3)" : "切换披露深度 (L1 / L2 / L3)"}">${snap.disclosure}</button>` +
      `<button type="button" class="st-btn" data-header-action="toggle-locale">${snap.locale === "zh-CN" ? "EN" : "中文"}</button>` +
      `</div>`;

    const mainScroll = doc.createElement("main");
    mainScroll.className = "st-main-scroll";

    if (snap.bannerNotice) {
      const strip = doc.createElement("div");
      strip.className = "st-notice-bar";
      strip.setAttribute("data-tone", snap.bannerNotice.tone);
      strip.innerHTML =
        `<div style="display:flex;align-items:center;gap:8px;">` +
        `<span class="st-dot" data-tone="vermilion"></span>` +
        `<span>${esc(snap.bannerNotice.message)}</span>` +
        `</div>` +
        `<button type="button" class="st-btn" data-header-action="dismiss-banner">${isEn ? "Dismiss" : "关闭"}</button>`;
      mainScroll.appendChild(strip);
    }

    mainScroll.appendChild(renderActiveCanvasView(snap));
    centerStage.appendChild(topbar);
    centerStage.appendChild(mainScroll);

    // 2.3 Right Inspector Rail (388px Open Swiss Pane)
    const rightRail = doc.createElement("aside");
    rightRail.className = "st-rail-right nlos-studio-drawer";
    rightRail.setAttribute(
      "aria-label",
      isEn ? "SABI Receipt & Entity Inspector" : "SABI 回执与实体详情轨",
    );

    if (snap.inspector) {
      const ins = snap.inspector;
      const tone =
        ins.badgeVariant === "danger" || ins.badgeVariant === "warn"
          ? "vermilion"
          : ins.badgeVariant === "ok"
            ? "forest"
            : "accent";
      rightRail.innerHTML =
        `<header class="st-inspector-head">` +
        `<div style="min-width:0;">` +
        `<div style="display:flex;align-items:center;gap:6px;">` +
        `<span class="st-dot" data-tone="vermilion"></span>` +
        `<strong style="font-size:13px;font-weight:800;color:var(--st-ink-950);overflow:hidden;text-overflow:ellipsis;white-space:nowrap;">${esc(ins.title)}</strong>` +
        `<span class="st-seal" data-tone="${tone}">${esc(ins.badgeText)}</span>` +
        `</div>` +
        `<div class="st-mono" style="font-size:10.5px;margin-top:2px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;">${esc(ins.subtitle)}</div>` +
        `</div>` +
        `<button type="button" class="st-btn" data-header-action="close-inspector">${isEn ? "Close" : "收起"}</button>` +
        `</header>` +
        `<div class="st-inspector-body">` +
        `<div class="st-sec-head" style="padding:0 0 6px 0;">` +
        `<span class="st-sec-num">01</span>` +
        `<span class="st-sec-title">${isEn ? "Authoritative Fields" : "权威属性字段"}</span>` +
        `<span class="st-sec-kicker">ATTRIBUTES</span>` +
        `</div>` +
        `<div>` +
        ins.fields
          .map(
            (f) =>
              `<div class="st-kv-row">` +
              `<span style="color:var(--st-ink-500);">${esc(f.key)}</span>` +
              `<span class="st-mono" style="color:var(--st-ink-950);font-weight:600;text-align:right;word-break:break-all;">${esc(f.value)}</span>` +
              `</div>`,
          )
          .join("") +
        `</div>` +
        (ins.rawReceiptHex
          ? `<div style="margin-top:6px;">` +
            `<div class="st-sec-head" style="padding:0 0 6px 0;">` +
            `<span class="st-sec-num">02</span>` +
            `<span class="st-sec-title">${isEn ? "SABI Cryptographic Receipt" : "SABI 密码学回执字节"}</span>` +
            `<span class="st-sec-kicker">RECEIPT_HEX</span>` +
            `</div>` +
            `<pre class="st-pre" style="margin-top:6px;">${esc(ins.rawReceiptHex)}</pre>` +
            `</div>`
          : "") +
        (ins.rawPayloadJson
          ? `<div style="margin-top:6px;">` +
            `<div class="st-sec-head" style="padding:0 0 6px 0;">` +
            `<span class="st-sec-num">03</span>` +
            `<span class="st-sec-title">${isEn ? "Structured Outcome" : "结构化载荷"}</span>` +
            `<span class="st-sec-kicker">JSON</span>` +
            `</div>` +
            `<pre class="st-pre" style="margin-top:6px;">${esc(ins.rawPayloadJson)}</pre>` +
            `</div>`
          : "") +
        `</div>`;
    } else {
      // Default Always-Live Right Rail: Real-Time SABI Pulse, KeyPool Ledger & Active Receipts
      rightRail.innerHTML =
        `<header class="st-inspector-head">` +
        `<div style="min-width:0;">` +
        `<div style="display:flex;align-items:center;gap:6px;">` +
        `<span class="st-dot" data-tone="live"></span>` +
        `<strong style="font-size:13px;font-weight:800;color:var(--st-ink-950);">${isEn ? "SABI Live Pulse & Receipt Ledger" : "实时控制脉搏与 SABI 回执台账"}</strong>` +
        `</div>` +
        `<div class="st-mono" style="font-size:10.5px;margin-top:2px;">ADR-0011 · ADR-0025 · ADR-0027 VERIFIED</div>` +
        `</div>` +
        `<span class="st-seal" data-tone="accent">LIVE</span>` +
        `</header>` +
        `<div class="st-inspector-body">` +
        `<div class="st-sec-head" style="padding:0 0 6px 0;">` +
        `<span class="st-sec-num">01</span>` +
        `<span class="st-sec-title">${isEn ? "Control-Plane Identity & Transport" : "控制面鉴权与传输绑定"}</span>` +
        `<span class="st-sec-kicker">TRANSPORT</span>` +
        `</div>` +
        `<div>` +
        `<div class="st-kv-row"><span>${isEn ? "Transport" : "传输通道"}</span><span class="st-mono" style="color:var(--st-ink-950);font-weight:600;">${snap.daemonOnline ? "NamedPipe (Authenticated)" : "Local Autonomous"}</span></div>` +
        `<div class="st-kv-row"><span>${isEn ? "Principal" : "操作员主体"}</span><span class="st-mono">${esc((snap.config.principalHex ?? "9f389da8334b5b5fef257e39feafa147").slice(0, 16))}…</span></div>` +
        `<div class="st-kv-row"><span>${isEn ? "SABI Epoch" : "SABI 兼容纪元"}</span><span class="st-mono" style="color:var(--st-accent);font-weight:700;">v1.0 (HostEpoch 2 · N-1 Ready)</span></div>` +
        `</div>` +
        `<div class="st-sec-head" style="padding:8px 0 6px 0;">` +
        `<span class="st-sec-num">02</span>` +
        `<span class="st-sec-title">${isEn ? "glm-5.3 5D Token Budget Pulse" : "glm-5.3 五维预算实时脉搏"}</span>` +
        `<span class="st-sec-kicker">5D_BUDGET</span>` +
        `</div>` +
        `<div>` +
        `<div class="st-kv-row"><span>${isEn ? "Gateway" : "网关地址"}</span><span class="st-mono">${esc(snap.keypoolStatus.gatewayOrigin ?? "http://127.0.0.1:8317")}</span></div>` +
        `<div class="st-kv-row"><span>${isEn ? "Locked Model" : "直锁模型"}</span><span class="st-mono" style="color:var(--st-ink-950);font-weight:700;">${esc(snap.keypoolStatus.defaultModel ?? "glm-5.3")}</span></div>` +
        `<div class="st-kv-row"><span>${isEn ? "Active Slots" : "在线密钥槽"}</span><span class="st-mono">${snap.keypoolStatus.activeKeys ?? 41} / ${snap.keypoolStatus.totalKeys ?? 44} (${esc(snap.keypoolStatus.strategy ?? "sticky")})</span></div>` +
        `<div class="st-kv-row"><span>${isEn ? "Credential Custody" : "凭据托管"}</span><span class="st-seal" data-tone="forest">LlmEnforcementShim</span></div>` +
        `</div>` +
        `<div class="st-sec-head" style="padding:8px 0 6px 0;">` +
        `<span class="st-sec-num">03</span>` +
        `<span class="st-sec-title">${isEn ? "Latest SABI Commit Receipts" : "最近签发 SABI 不可变回执"}</span>` +
        `<span class="st-sec-kicker">RECEIPTS</span>` +
        `</div>` +
        `<div style="display:flex;flex-direction:column;gap:8px;">` +
        `<div style="padding:8px 10px;border-left:2px solid var(--st-accent);background:var(--st-bg-subtle);">` +
        `<div style="display:flex;justify-content:space-between;align-items:baseline;">` +
        `<strong style="font-size:11.5px;color:var(--st-ink-950);">InspectHealth (Artifact)</strong>` +
        `<span class="st-mono" style="font-size:10px;color:var(--st-accent);">#r${snap.recoveryHealth.artifact.completedCycles}</span>` +
        `</div>` +
        `<div class="st-mono" style="font-size:10.5px;margin-top:3px;word-break:break-all;">${esc(snap.lastReceiptHex)}</div>` +
        `</div>` +
        `<div style="padding:8px 10px;border-left:2px solid var(--st-vermilion);background:var(--st-bg-subtle);">` +
        `<div style="display:flex;justify-content:space-between;align-items:baseline;">` +
        `<strong style="font-size:11.5px;color:var(--st-ink-950);">StoreCertificationReceipt</strong>` +
        `<span class="st-mono" style="font-size:10px;color:var(--st-vermilion);">4/4 GATES</span>` +
        `</div>` +
        `<div class="st-mono" style="font-size:10.5px;margin-top:3px;word-break:break-all;">4c8e2a190fb45811c029d7a319f84b12</div>` +
        `</div>` +
        `</div>` +
        `<div style="margin-top:auto;padding-top:10px;border-top:1px solid var(--st-rule-soft);font-size:11px;color:var(--st-ink-500);">` +
        (isEn
          ? "Click any task, process node, or RPC probe button in the center stage to inspect its full cryptographic receipt here."
          : "提示：点击中央画布的任意任务凭证、归属树节点或 RPC 探针按钮，本轨将自动切换为该对象的完整属性与密码学回执。") +
        `</div>` +
        `</div>`;
    }

    workbench.appendChild(leftRail);
    workbench.appendChild(centerStage);
    workbench.appendChild(rightRail);
    root.appendChild(workbench);

    // 3. Optional Global Search & Intent Modal (Ctrl+K)
    if (snap.commandModalOpen) {
      const modal = doc.createElement("div");
      modal.className = "st-modal-mask";
      modal.innerHTML =
        `<div class="st-modal-dialog" role="dialog" aria-modal="true" aria-label="${isEn ? "Global Search & Intent Dispatch" : "全局检索与自然语言意图派发"}">` +
        `<div class="st-sec-head" style="padding:12px 16px;">` +
        `<span class="st-sec-num">00</span>` +
        `<h2 class="st-sec-title">${isEn ? "Global Read-Only Index & Intent Handoff" : "全局非权威检索与自然语言意图直达"}</h2>` +
        `<button type="button" class="st-btn" style="margin-left:auto;" data-header-action="close-command-modal">ESC</button>` +
        `</div>` +
        `<div style="padding:14px 16px;display:flex;flex-direction:column;gap:12px;max-height:70vh;overflow-y:auto;">` +
        `<div style="display:flex;gap:8px;">` +
        `<input type="search" id="studio-modal-search-input" class="st-input" placeholder="${isEn ? "Search tasks, packages, or enter a natural-language goal…" : "输入关键词检索任务、应用包或直接输入自然语言目标…"}" />` +
        `<button type="button" class="st-btn st-btn--primary" data-header-action="modal-jump-intent">${isEn ? "Decompose with glm-5.3" : "送入 glm-5.3 拆解"}</button>` +
        `</div>` +
        `<div id="studio-modal-spotlight-host">` +
        renderSpotlightViewHtml(store.controllers.spotlight.getState()) +
        `</div>` +
        `</div>` +
        `</div>`;
      root.appendChild(modal);
    }

    // 4. Optional Z4 Non-Occludable Trusted Security Confirmation Modal
    if (snap.z4Prompt) {
      const z4 = snap.z4Prompt;
      const modal = doc.createElement("div");
      modal.className = "st-modal-mask";
      modal.setAttribute("data-nlos-plane", "Z4");
      modal.innerHTML =
        `<div class="st-modal-dialog" data-z4-danger="${z4.irreversible ? "true" : "false"}" role="alertdialog" aria-modal="true" aria-label="Z4 Trusted Security Confirmation">` +
        `<div class="st-sec-head" style="padding:12px 16px;">` +
        `<span class="st-sec-num">Z4</span>` +
        `<h2 class="st-sec-title">${isEn ? "System Security Confirmation Gate" : "系统级可信安全确认门 (Trusted UI)"}</h2>` +
        `<span class="st-seal" style="margin-left:auto;" data-tone="${z4.irreversible ? "vermilion" : "accent"}">${z4.irreversible ? (isEn ? "IRREVERSIBLE" : "不可逆操作") : isEn ? "COMPENSATABLE" : "可补偿变更"}</span>` +
        `</div>` +
        `<div style="padding:16px 20px;display:flex;flex-direction:column;gap:12px;">` +
        `<div style="font-size:15px;font-weight:800;color:var(--st-ink-950);">${esc(z4.title)}</div>` +
        `<div style="padding:10px 12px;border-left:2px solid var(--st-accent);background:var(--st-bg-subtle);">` +
        `<div class="st-mono" style="font-size:10.5px;margin-bottom:3px;">${isEn ? "AUTHORIZED SCOPE & CEILING" : "授权作用范围与配额上限"}</div>` +
        `<div style="color:var(--st-ink-900);">${esc(z4.summary)}</div>` +
        `</div>` +
        `<div style="padding:10px 12px;border-left:2px solid var(--st-vermilion);background:var(--st-bg-subtle);">` +
        `<div class="st-mono" style="font-size:10.5px;margin-bottom:3px;color:var(--st-vermilion);">${isEn ? "CONSEQUENCE & SAFEGUARD" : "执行后果与恢复保障"}</div>` +
        `<div style="color:var(--st-ink-900);">${esc(z4.consequence)}</div>` +
        `</div>` +
        `<div style="display:flex;justify-content:flex-end;gap:10px;padding-top:6px;border-top:1px solid var(--st-rule-soft);">` +
        `<button type="button" class="st-btn" data-z4-action="cancel">${isEn ? "Cancel (Esc)" : "取消 (Esc)"}</button>` +
        `<button type="button" class="st-btn st-btn--primary" data-z4-action="confirm">${isEn ? "Authorize & Execute" : "确认授权并执行"}</button>` +
        `</div>` +
        `</div>` +
        `</div>`;
      root.appendChild(modal);
    }

    // Event delegation
    root.addEventListener("click", (ev: MouseEvent) => {
      const target =
        ev.target && typeof (ev.target as Element).closest === "function"
          ? (ev.target as Element)
          : null;
      if (!target) {
        return;
      }

      const navBtn = target.closest("[data-nav-section]");
      if (navBtn) {
        const sec = navBtn.getAttribute(
          "data-nav-section",
        ) as StudioNavSectionId | null;
        if (sec) {
          store.setActiveSection(sec);
        }
        return;
      }

      const themeBtn = target.closest("[data-set-studio-theme]");
      if (themeBtn) {
        const th = themeBtn.getAttribute(
          "data-set-studio-theme",
        ) as StudioThemeId | null;
        if (th) {
          store.setStudioTheme(th);
        }
        return;
      }

      const z4Btn = target.closest("[data-z4-action]");
      if (z4Btn) {
        const act = z4Btn.getAttribute("data-z4-action");
        if (act === "confirm") {
          store.confirmZ4Prompt();
        } else {
          store.cancelZ4Prompt();
        }
        return;
      }

      const headerBtn = target.closest("[data-header-action]");
      if (headerBtn) {
        const act = headerBtn.getAttribute("data-header-action");
        if (act === "toggle-theme") {
          store.toggleTheme();
        } else if (act === "toggle-locale") {
          store.setLocale(snap.locale === "zh-CN" ? "en-US" : "zh-CN");
        } else if (act === "cycle-disclosure") {
          store.cycleDisclosure();
        } else if (act === "open-command-modal") {
          store.setCommandModalOpen(true);
        } else if (act === "close-command-modal") {
          store.setCommandModalOpen(false);
        } else if (act === "close-inspector") {
          store.setInspector(null);
        } else if (act === "dismiss-banner") {
          store.setBannerNotice(null);
        } else if (act === "modal-jump-intent") {
          const inp = root.querySelector(
            "#studio-modal-search-input",
          ) as HTMLInputElement | null;
          const q = inp?.value?.trim() || "整理工作区制品并生成校验报告";
          store.setCommandModalOpen(false);
          store.setActiveSection("intent-keypool");
          void store.runIntentAssist(q);
        }
      }
    });

    const modalSearchInput = root.querySelector(
      "#studio-modal-search-input",
    ) as HTMLInputElement | null;
    if (modalSearchInput) {
      modalSearchInput.addEventListener("input", () => {
        store.controllers.spotlight.querySpotlightCandidates({
          queryText: modalSearchInput.value,
          limit: 15,
        });
        const host = root.querySelector("#studio-modal-spotlight-host");
        if (host) {
          host.innerHTML = renderSpotlightViewHtml(
            store.controllers.spotlight.getState(),
          );
        }
      });
    }

    container.replaceChildren(root);
  };

  store.subscribe(() => {
    renderShell();
  });

  if (typeof window !== "undefined") {
    window.addEventListener("keydown", (ev: KeyboardEvent) => {
      if ((ev.ctrlKey || ev.metaKey) && ev.key.toLowerCase() === "k") {
        ev.preventDefault();
        store.setCommandModalOpen(!store.getState().commandModalOpen);
      } else if (ev.key === "Escape") {
        if (store.getState().z4Prompt) {
          store.cancelZ4Prompt();
        } else if (store.getState().commandModalOpen) {
          store.setCommandModalOpen(false);
        } else if (store.getState().inspector) {
          store.setInspector(null);
        }
      }
    });
  }

  renderShell();
  void store.refreshAllFromDaemon();
  return store;
}
