// Full 16-state Task / Node semantic status indicator & Statutory Lock Badges.
// Authority: Stage D Desktop Visual & UX Spec §1.2, §2A.3, ADR-0025 Decision 3 & 4.
// Strict discipline:
//   - 6x6px geometric SVG glyphs + monochrome ink + strict bipolar signals (#D97706 / #DC2626)
//   - 100% zero internal jargon in L0/L1/L2 (raw enum names & invariant IDs strictly L3-only)

import { createIconElement, renderIconSvg } from "./icons";

const SVG_NS = "http://www.w3.org/2000/svg";

export type DisclosureLevel = "L0" | "L1" | "L2" | "L3";
export type WorkstationLocale = "zh-CN" | "en-US";

/** Canonical 16 Task lifecycle states (crates/nlos-notify TaskTransitionKind 1..=16). */
export type CanonicalTaskState =
  | "PROPOSED"
  | "WAITING_CONSENT"
  | "AUTHORIZED"
  | "RUNNING"
  | "WAITING_INPUT"
  | "WAITING_RESOURCE"
  | "WAITING_NETWORK"
  | "SUSPENDED"
  | "RECOVERING"
  | "CANCELLING"
  | "COMPLETING"
  | "COMPLETED"
  | "PARTIAL"
  | "DISPUTED"
  | "FAILED"
  | "CANCELLED";

/** Extended workstation status domain covering pre-submit draft, quarantine, and TaskNode states. */
export type WorkstationStatusState =
  | CanonicalTaskState
  | "DRAFT"
  | "QUARANTINED"
  | "DECLARED"
  | "BLOCKED_DEPENDENCY"
  | "ELIGIBLE"
  | "ACTIVE"
  | "SKIPPED"
  | "SUPERSEDED";

export type StatusGlyphShape =
  | "hollow-ring"
  | "solid-diamond"
  | "hollow-diamond"
  | "solid-dot"
  | "half-square"
  | "dashed-ring"
  | "pause-bars"
  | "slashed-circle"
  | "closed-arc"
  | "solid-square"
  | "solid-triangle"
  | "left-half-circle"
  | "check-polyline"
  | "cross-mark";

/** Monochrome tones ("neutral" | "active" | "muted") + strict bipolar signals ("action" | "danger"). */
export type StatusSignalTone = "neutral" | "active" | "muted" | "action" | "danger";

export interface StatusDescriptor {
  readonly state: WorkstationStatusState;
  readonly glyph: StatusGlyphShape;
  readonly tone: StatusSignalTone;
  readonly signalToken:
    | "--nlos-indicator-neutral"
    | "--nlos-indicator-active"
    | "--nlos-text-muted"
    | "--nlos-signal-action"
    | "--nlos-signal-danger";
  readonly labels: Readonly<Record<WorkstationLocale, string>>;
}

export const CANONICAL_TASK_STATES: readonly CanonicalTaskState[] = [
  "PROPOSED",
  "WAITING_CONSENT",
  "AUTHORIZED",
  "RUNNING",
  "WAITING_INPUT",
  "WAITING_RESOURCE",
  "WAITING_NETWORK",
  "SUSPENDED",
  "RECOVERING",
  "CANCELLING",
  "COMPLETING",
  "COMPLETED",
  "PARTIAL",
  "DISPUTED",
  "FAILED",
  "CANCELLED",
];

const STATUS_DESCRIPTORS: Readonly<Record<WorkstationStatusState, StatusDescriptor>> = {
  DRAFT: {
    state: "DRAFT",
    glyph: "hollow-ring",
    tone: "neutral",
    signalToken: "--nlos-indicator-neutral",
    labels: { "zh-CN": "草稿待提交", "en-US": "Draft" },
  },
  PROPOSED: {
    state: "PROPOSED",
    glyph: "hollow-ring",
    tone: "neutral",
    signalToken: "--nlos-indicator-neutral",
    labels: { "zh-CN": "草稿待提交", "en-US": "Draft" },
  },
  WAITING_CONSENT: {
    state: "WAITING_CONSENT",
    glyph: "solid-diamond",
    tone: "action",
    signalToken: "--nlos-signal-action",
    labels: { "zh-CN": "待您确认权限或费用上限", "en-US": "Approval Required" },
  },
  WAITING_INPUT: {
    state: "WAITING_INPUT",
    glyph: "hollow-diamond",
    tone: "action",
    signalToken: "--nlos-signal-action",
    labels: { "zh-CN": "待补充说明", "en-US": "Input Needed" },
  },
  AUTHORIZED: {
    state: "AUTHORIZED",
    glyph: "solid-dot",
    tone: "active",
    signalToken: "--nlos-indicator-active",
    labels: { "zh-CN": "正在运行", "en-US": "Running" },
  },
  RUNNING: {
    state: "RUNNING",
    glyph: "solid-dot",
    tone: "active",
    signalToken: "--nlos-indicator-active",
    labels: { "zh-CN": "正在运行", "en-US": "Running" },
  },
  COMPLETING: {
    state: "COMPLETING",
    glyph: "solid-dot",
    tone: "active",
    signalToken: "--nlos-indicator-active",
    labels: { "zh-CN": "正在完成收尾", "en-US": "Completing" },
  },
  WAITING_RESOURCE: {
    state: "WAITING_RESOURCE",
    glyph: "half-square",
    tone: "neutral",
    signalToken: "--nlos-indicator-neutral",
    labels: { "zh-CN": "排队等待计算资源", "en-US": "Queued for Resources" },
  },
  WAITING_NETWORK: {
    state: "WAITING_NETWORK",
    glyph: "dashed-ring",
    tone: "neutral",
    signalToken: "--nlos-indicator-neutral",
    labels: {
      "zh-CN": "等待网络连接（本地正常可用）",
      "en-US": "Waiting for Network (Local Ready)",
    },
  },
  SUSPENDED: {
    state: "SUSPENDED",
    glyph: "pause-bars",
    tone: "muted",
    signalToken: "--nlos-text-muted",
    labels: { "zh-CN": "已暂停", "en-US": "Paused" },
  },
  CANCELLING: {
    state: "CANCELLING",
    glyph: "slashed-circle",
    tone: "muted",
    signalToken: "--nlos-text-muted",
    labels: { "zh-CN": "正在取消", "en-US": "Cancelling" },
  },
  CANCELLED: {
    state: "CANCELLED",
    glyph: "slashed-circle",
    tone: "muted",
    signalToken: "--nlos-text-muted",
    labels: { "zh-CN": "已取消", "en-US": "Cancelled" },
  },
  RECOVERING: {
    state: "RECOVERING",
    glyph: "closed-arc",
    tone: "neutral",
    signalToken: "--nlos-indicator-neutral",
    labels: { "zh-CN": "正在对账恢复", "en-US": "Recovering" },
  },
  QUARANTINED: {
    state: "QUARANTINED",
    glyph: "solid-square",
    tone: "danger",
    signalToken: "--nlos-signal-danger",
    labels: { "zh-CN": "已受控隔离", "en-US": "Isolated" },
  },
  DISPUTED: {
    state: "DISPUTED",
    glyph: "solid-triangle",
    tone: "action",
    signalToken: "--nlos-signal-action",
    labels: { "zh-CN": "结果不一致，待您核对", "en-US": "Needs Verification" },
  },
  PARTIAL: {
    state: "PARTIAL",
    glyph: "left-half-circle",
    tone: "active",
    signalToken: "--nlos-indicator-active",
    labels: { "zh-CN": "部分完成（含未完成项）", "en-US": "Partially Completed" },
  },
  COMPLETED: {
    state: "COMPLETED",
    glyph: "check-polyline",
    tone: "active",
    signalToken: "--nlos-indicator-active",
    labels: { "zh-CN": "已完成", "en-US": "Completed" },
  },
  FAILED: {
    state: "FAILED",
    glyph: "cross-mark",
    tone: "danger",
    signalToken: "--nlos-signal-danger",
    labels: { "zh-CN": "未能完成", "en-US": "Failed" },
  },
  DECLARED: {
    state: "DECLARED",
    glyph: "hollow-ring",
    tone: "neutral",
    signalToken: "--nlos-indicator-neutral",
    labels: { "zh-CN": "待执行步骤", "en-US": "Pending Step" },
  },
  BLOCKED_DEPENDENCY: {
    state: "BLOCKED_DEPENDENCY",
    glyph: "half-square",
    tone: "neutral",
    signalToken: "--nlos-indicator-neutral",
    labels: { "zh-CN": "等待前置步骤完成", "en-US": "Waiting on Prerequisite" },
  },
  ELIGIBLE: {
    state: "ELIGIBLE",
    glyph: "hollow-ring",
    tone: "neutral",
    signalToken: "--nlos-indicator-neutral",
    labels: { "zh-CN": "已就绪待启动", "en-US": "Ready to Run" },
  },
  ACTIVE: {
    state: "ACTIVE",
    glyph: "solid-dot",
    tone: "active",
    signalToken: "--nlos-indicator-active",
    labels: { "zh-CN": "正在执行", "en-US": "Active" },
  },
  SKIPPED: {
    state: "SKIPPED",
    glyph: "slashed-circle",
    tone: "muted",
    signalToken: "--nlos-text-muted",
    labels: { "zh-CN": "已跳过", "en-US": "Skipped" },
  },
  SUPERSEDED: {
    state: "SUPERSEDED",
    glyph: "slashed-circle",
    tone: "muted",
    signalToken: "--nlos-text-muted",
    labels: { "zh-CN": "已由新尝试替代", "en-US": "Replaced by Newer Run" },
  },
};

export const ALL_WORKSTATION_STATUS_STATES: readonly WorkstationStatusState[] =
  Object.keys(STATUS_DESCRIPTORS) as WorkstationStatusState[];

function escapeHtml(raw: string): string {
  return raw
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

export function getStatusDescriptor(state: WorkstationStatusState): StatusDescriptor {
  return STATUS_DESCRIPTORS[state];
}

function renderStatusGlyphInnerSvg(glyph: StatusGlyphShape): string {
  switch (glyph) {
    case "hollow-ring":
      return `<circle cx="3" cy="3" r="2.2" fill="none" stroke="currentColor" stroke-width="1.2" />`;
    case "solid-diamond":
      return `<polygon points="3,0.4 5.6,3 3,5.6 0.4,3" fill="currentColor" />`;
    case "hollow-diamond":
      return `<polygon points="3,0.6 5.4,3 3,5.4 0.6,3" fill="none" stroke="currentColor" stroke-width="1.2" />`;
    case "solid-dot":
      return `<circle cx="3" cy="3" r="2.5" fill="currentColor" />`;
    case "half-square":
      return (
        `<rect x="0.7" y="0.7" width="4.6" height="4.6" fill="none" stroke="currentColor" stroke-width="1.1" />` +
        `<rect x="0.7" y="0.7" width="2.3" height="4.6" fill="currentColor" />`
      );
    case "dashed-ring":
      return `<circle cx="3" cy="3" r="2.2" fill="none" stroke="currentColor" stroke-width="1.2" stroke-dasharray="1.4 1.1" />`;
    case "pause-bars":
      return (
        `<line x1="1.9" y1="0.8" x2="1.9" y2="5.2" stroke="currentColor" stroke-width="1.3" />` +
        `<line x1="4.1" y1="0.8" x2="4.1" y2="5.2" stroke="currentColor" stroke-width="1.3" />`
      );
    case "slashed-circle":
      return (
        `<circle cx="3" cy="3" r="2.2" fill="none" stroke="currentColor" stroke-width="1.1" />` +
        `<line x1="1.4" y1="4.6" x2="4.6" y2="1.4" stroke="currentColor" stroke-width="1.1" />`
      );
    case "closed-arc":
      return `<path d="M5.1 3A2.1 2.1 0 1 1 3 0.9" fill="none" stroke="currentColor" stroke-width="1.25" />`;
    case "solid-square":
      return `<rect x="0.7" y="0.7" width="4.6" height="4.6" fill="currentColor" />`;
    case "solid-triangle":
      return `<polygon points="3,0.5 5.6,5.3 0.4,5.3" fill="currentColor" />`;
    case "left-half-circle":
      return (
        `<circle cx="3" cy="3" r="2.2" fill="none" stroke="currentColor" stroke-width="1.1" />` +
        `<path d="M3 0.8A2.2 2.2 0 0 0 3 5.2Z" fill="currentColor" />`
      );
    case "check-polyline":
      return `<polyline points="0.7,3.2 2.3,4.8 5.3,1.2" fill="none" stroke="currentColor" stroke-width="1.5" />`;
    case "cross-mark":
      return (
        `<line x1="1" y1="1" x2="5" y2="5" stroke="currentColor" stroke-width="1.5" />` +
        `<line x1="5" y1="1" x2="1" y2="5" stroke="currentColor" stroke-width="1.5" />`
      );
  }
}

function populateGlyphSvgElement(doc: Document, svg: SVGSVGElement, glyph: StatusGlyphShape): void {
  const addEl = (tag: string, attrs: Readonly<Record<string, string>>): void => {
    const node = doc.createElementNS(SVG_NS, tag);
    for (const [k, v] of Object.entries(attrs)) {
      node.setAttribute(k, v);
    }
    svg.appendChild(node);
  };

  switch (glyph) {
    case "hollow-ring":
      addEl("circle", { cx: "3", cy: "3", r: "2.2", fill: "none", stroke: "currentColor", "stroke-width": "1.2" });
      break;
    case "solid-diamond":
      addEl("polygon", { points: "3,0.4 5.6,3 3,5.6 0.4,3", fill: "currentColor" });
      break;
    case "hollow-diamond":
      addEl("polygon", { points: "3,0.6 5.4,3 3,5.4 0.6,3", fill: "none", stroke: "currentColor", "stroke-width": "1.2" });
      break;
    case "solid-dot":
      addEl("circle", { cx: "3", cy: "3", r: "2.5", fill: "currentColor" });
      break;
    case "half-square":
      addEl("rect", { x: "0.7", y: "0.7", width: "4.6", height: "4.6", fill: "none", stroke: "currentColor", "stroke-width": "1.1" });
      addEl("rect", { x: "0.7", y: "0.7", width: "2.3", height: "4.6", fill: "currentColor" });
      break;
    case "dashed-ring":
      addEl("circle", { cx: "3", cy: "3", r: "2.2", fill: "none", stroke: "currentColor", "stroke-width": "1.2", "stroke-dasharray": "1.4 1.1" });
      break;
    case "pause-bars":
      addEl("line", { x1: "1.9", y1: "0.8", x2: "1.9", y2: "5.2", stroke: "currentColor", "stroke-width": "1.3" });
      addEl("line", { x1: "4.1", y1: "0.8", x2: "4.1", y2: "5.2", stroke: "currentColor", "stroke-width": "1.3" });
      break;
    case "slashed-circle":
      addEl("circle", { cx: "3", cy: "3", r: "2.2", fill: "none", stroke: "currentColor", "stroke-width": "1.1" });
      addEl("line", { x1: "1.4", y1: "4.6", x2: "4.6", y2: "1.4", stroke: "currentColor", "stroke-width": "1.1" });
      break;
    case "closed-arc":
      addEl("path", { d: "M5.1 3A2.1 2.1 0 1 1 3 0.9", fill: "none", stroke: "currentColor", "stroke-width": "1.25" });
      break;
    case "solid-square":
      addEl("rect", { x: "0.7", y: "0.7", width: "4.6", height: "4.6", fill: "currentColor" });
      break;
    case "solid-triangle":
      addEl("polygon", { points: "3,0.5 5.6,5.3 0.4,5.3", fill: "currentColor" });
      break;
    case "left-half-circle":
      addEl("circle", { cx: "3", cy: "3", r: "2.2", fill: "none", stroke: "currentColor", "stroke-width": "1.1" });
      addEl("path", { d: "M3 0.8A2.2 2.2 0 0 0 3 5.2Z", fill: "currentColor" });
      break;
    case "check-polyline":
      addEl("polyline", { points: "0.7,3.2 2.3,4.8 5.3,1.2", fill: "none", stroke: "currentColor", "stroke-width": "1.5" });
      break;
    case "cross-mark":
      addEl("line", { x1: "1", y1: "1", x2: "5", y2: "5", stroke: "currentColor", "stroke-width": "1.5" });
      addEl("line", { x1: "5", y1: "1", x2: "1", y2: "5", stroke: "currentColor", "stroke-width": "1.5" });
      break;
  }
}

export interface StatusIndicatorOptions {
  readonly state: WorkstationStatusState;
  readonly locale?: WorkstationLocale;
  readonly disclosure?: DisclosureLevel;
  readonly doc?: Document;
}

export function renderStatusIndicatorHtml(options: StatusIndicatorOptions): string {
  const locale: WorkstationLocale = options.locale ?? "zh-CN";
  const disclosure: DisclosureLevel = options.disclosure ?? "L1";
  const desc = getStatusDescriptor(options.state);
  const label = desc.labels[locale];
  const glyphSvg =
    `<svg xmlns="${SVG_NS}" class="nlos-status-glyph" data-glyph="${desc.glyph}" ` +
    `width="6" height="6" viewBox="0 0 6 6" aria-hidden="true">` +
    `${renderStatusGlyphInnerSvg(desc.glyph)}</svg>`;
  const enumBadge =
    disclosure === "L3"
      ? `<code class="nlos-status-enum">${escapeHtml(desc.state)}</code>`
      : "";
  return (
    `<span class="nlos-status-cell nlos-status-cell--${desc.tone}" data-tone="${desc.tone}">` +
    `${glyphSvg}<span class="nlos-status-label">${escapeHtml(label)}</span>${enumBadge}</span>`
  );
}

export function createStatusIndicatorElement(options: StatusIndicatorOptions): HTMLElement {
  const doc = options.doc ?? document;
  const locale: WorkstationLocale = options.locale ?? "zh-CN";
  const disclosure: DisclosureLevel = options.disclosure ?? "L1";
  const desc = getStatusDescriptor(options.state);

  const root = doc.createElement("span");
  root.className = `nlos-status-cell nlos-status-cell--${desc.tone}`;
  root.setAttribute("data-tone", desc.tone);

  const svg = doc.createElementNS(SVG_NS, "svg");
  svg.setAttribute("class", "nlos-status-glyph");
  svg.setAttribute("data-glyph", desc.glyph);
  svg.setAttribute("width", "6");
  svg.setAttribute("height", "6");
  svg.setAttribute("viewBox", "0 0 6 6");
  svg.setAttribute("aria-hidden", "true");
  populateGlyphSvgElement(doc, svg, desc.glyph);
  root.appendChild(svg);

  const labelSpan = doc.createElement("span");
  labelSpan.className = "nlos-status-label";
  labelSpan.textContent = desc.labels[locale];
  root.appendChild(labelSpan);

  if (disclosure === "L3") {
    const enumCode = doc.createElement("code");
    enumCode.className = "nlos-status-enum";
    enumCode.textContent = desc.state;
    root.appendChild(enumCode);
  }

  return root;
}

/* ==========================================================================
   Statutory Non-Tamperable Lock Badges ([UI-PARITY-001] Third Law)
   ========================================================================== */

export type StatutoryLockKind =
  | "SEARCH_0X1F_READ_ONLY"
  | "IRREVERSIBLE_NO_UNDO"
  | "NODE_COMPLETED_OR_ACTIVE_LOCKED"
  | "TASK_ATTEMPT_NO_MANUAL_COMMIT"
  | "OPERATION_EFFECT_UNKNOWN_NO_MANUAL_SUCCESS"
  | "QUARANTINED_LEASE_FENCE";

export interface StatutoryLockDescriptor {
  readonly kind: StatutoryLockKind;
  readonly allowedVerbs: readonly string[];
  readonly forbiddenVerbs: readonly string[];
  /** Zero-jargon everyday explanation safe for L0/L1/L2. */
  readonly labels: Readonly<Record<WorkstationLocale, string>>;
  /** Raw technical invariant & protocol detail rendered strictly in L3. */
  readonly l3DiagnosticTag: string;
}

const STATUTORY_LOCKS: Readonly<Record<StatutoryLockKind, StatutoryLockDescriptor>> = {
  SEARCH_0X1F_READ_ONLY: {
    kind: "SEARCH_0X1F_READ_ONLY",
    allowedVerbs: ["JumpToAuthoritativeObject"],
    forbiddenVerbs: ["Delete", "Charge", "Pause", "Cancel", "Authorize", "Commit"],
    labels: {
      "zh-CN": "候选搜索预览只读 · 打开原记录核实后可操作",
      "en-US": "Read-only search preview · Open original record to act",
    },
    l3DiagnosticTag: "NON_AUTHORITATIVE mask=0x1F [CONS-CTRL-001]",
  },
  IRREVERSIBLE_NO_UNDO: {
    kind: "IRREVERSIBLE_NO_UNDO",
    allowedVerbs: ["ViewEffectProof"],
    forbiddenVerbs: ["Undo"],
    labels: {
      "zh-CN": "不可逆外部操作 · 无撤销入口，可查看操作凭证",
      "en-US": "Irreversible external action · Undo unavailable, view proof",
    },
    l3DiagnosticTag: "IRREVERSIBLE ActionReceipt [RCPT-UNDO-001]",
  },
  NODE_COMPLETED_OR_ACTIVE_LOCKED: {
    kind: "NODE_COMPLETED_OR_ACTIVE_LOCKED",
    allowedVerbs: [],
    forbiddenVerbs: ["DeleteNode", "ReplaceNode", "RewireEdge", "MergeNode"],
    labels: {
      "zh-CN": "已执行或正在执行的步骤禁止修改结构",
      "en-US": "Completed or active step is locked against structural edits",
    },
    l3DiagnosticTag: "TaskNode(COMPLETED|ACTIVE) read-only [PLAN-CHANGE-001]",
  },
  TASK_ATTEMPT_NO_MANUAL_COMMIT: {
    kind: "TASK_ATTEMPT_NO_MANUAL_COMMIT",
    allowedVerbs: ["Cancel", "Supersede", "RequestReverify"],
    forbiddenVerbs: ["ManualCommit", "ForceMarkCommitted"],
    labels: {
      "zh-CN": "系统自动仲裁最终提交 · 不可手动标记完成",
      "en-US": "System-arbitrated commit · Manual completion disabled",
    },
    l3DiagnosticTag: "TaskAttempt CAS-only commit [TASK-COMMIT-001]",
  },
  OPERATION_EFFECT_UNKNOWN_NO_MANUAL_SUCCESS: {
    kind: "OPERATION_EFFECT_UNKNOWN_NO_MANUAL_SUCCESS",
    allowedVerbs: ["Reconcile", "Compensate"],
    forbiddenVerbs: ["MarkSucceeded", "MarkNotExecuted"],
    labels: {
      "zh-CN": "外部执行结果待对账核实 · 不可手动改判成功",
      "en-US": "External outcome pending reconciliation · Manual override disabled",
    },
    l3DiagnosticTag: "Operation(EFFECT_UNKNOWN) [TASK-EFFECT-003]",
  },
  QUARANTINED_LEASE_FENCE: {
    kind: "QUARANTINED_LEASE_FENCE",
    allowedVerbs: ["InspectFenceStatus"],
    forbiddenVerbs: ["UnlockByLocalTtl", "ForceResumeQuarantined"],
    labels: {
      "zh-CN": "安全围栏隔离中 · 待收齐关闭确认后解锁",
      "en-US": "Isolated by safety fence · Unlocks after closure confirmation",
    },
    l3DiagnosticTag: "QUARANTINED lease fence [LEASE-TTL-001]",
  },
};

export const ALL_STATUTORY_LOCK_KINDS: readonly StatutoryLockKind[] =
  Object.keys(STATUTORY_LOCKS) as StatutoryLockKind[];

export function getStatutoryLockDescriptor(kind: StatutoryLockKind): StatutoryLockDescriptor {
  return STATUTORY_LOCKS[kind];
}

/** Headlamp-style pre-render verb pruning against statutory non-tamperable locks. */
export function pruneVerbsAgainstStatutoryLock(
  candidateVerbs: readonly string[],
  lockKind: StatutoryLockKind | null,
): readonly string[] {
  if (lockKind === null) {
    return [...candidateVerbs];
  }
  const desc = STATUTORY_LOCKS[lockKind];
  const allowedSet = new Set(desc.allowedVerbs);
  const forbiddenSet = new Set(desc.forbiddenVerbs);
  return candidateVerbs.filter((v) => !forbiddenSet.has(v) && (allowedSet.size === 0 ? false : allowedSet.has(v)));
}

export interface StatutoryLockBadgeOptions {
  readonly kind: StatutoryLockKind;
  readonly locale?: WorkstationLocale;
  readonly disclosure?: DisclosureLevel;
  readonly doc?: Document;
}

export function renderStatutoryLockBadgeHtml(options: StatutoryLockBadgeOptions): string {
  const locale: WorkstationLocale = options.locale ?? "zh-CN";
  const disclosure: DisclosureLevel = options.disclosure ?? "L1";
  const desc = getStatutoryLockDescriptor(options.kind);
  const iconHtml = renderIconSvg("lock", { size: 14, strokeWidth: 1.25 });
  const l3Suffix =
    disclosure === "L3"
      ? ` <code class="nlos-status-enum">${escapeHtml(desc.l3DiagnosticTag)}</code>`
      : "";
  return (
    `<span class="nlos-statutory-lock" data-statutory-lock="${escapeHtml(desc.kind)}">` +
    `${iconHtml}<span>${escapeHtml(desc.labels[locale])}</span>${l3Suffix}</span>`
  );
}

export function createStatutoryLockBadgeElement(options: StatutoryLockBadgeOptions): HTMLElement {
  const doc = options.doc ?? document;
  const locale: WorkstationLocale = options.locale ?? "zh-CN";
  const disclosure: DisclosureLevel = options.disclosure ?? "L1";
  const desc = getStatutoryLockDescriptor(options.kind);

  const badge = doc.createElement("span");
  badge.className = "nlos-statutory-lock";
  badge.setAttribute("data-statutory-lock", desc.kind);
  badge.appendChild(createIconElement("lock", { size: 14, strokeWidth: 1.25, doc }));

  const text = doc.createElement("span");
  text.textContent = desc.labels[locale];
  badge.appendChild(text);

  if (disclosure === "L3") {
    const code = doc.createElement("code");
    code.className = "nlos-status-enum";
    code.textContent = desc.l3DiagnosticTag;
    badge.appendChild(code);
  }

  return badge;
}
