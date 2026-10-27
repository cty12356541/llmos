// SOTA-6 (Terraform Plan Preview): Structured + / ~ / - / = Cascade Blast-Radius Diff
// Preview Table, Topic Purge Data-Loss Receipt Banner, and 5s Two-Step Arming Helper.
// Authority: Stage D Desktop Visual & UX Spec §2A.1 (SOTA-6), §3.2.C.4, §3.4.1, §3.7.2.

import { createIconElement, renderIconSvg } from "./icons";
import type { DisclosureLevel, WorkstationLocale } from "./status-indicator";

export const ARMING_WINDOW_MS = 5000;

export type BlastDiffSymbol = "+" | "~" | "-" | "=";

export type BlastDiffCategory =
  | "created"
  | "reverified"
  | "paused"
  | "throttled"
  | "fenced"
  | "terminated"
  | "evicted"
  | "purged"
  | "exempted";

export type BlastOperationKind =
  | "generic-cascade"
  | "plan-reauthorize"
  | "process-kill"
  | "topic-purge"
  | "application-disable"
  | "application-uninstall";

export interface BlastRadiusRow {
  readonly symbol: BlastDiffSymbol;
  readonly category: BlastDiffCategory;
  readonly scopeLabel: string;
  readonly targetLabel: string;
  readonly reason: string;
  readonly targetIdHex?: string;
}

export interface BlastRadiusSummary {
  readonly addedCount: number;
  readonly modifiedCount: number;
  readonly removedCount: number;
  readonly exemptedCount: number;
  readonly affectedTotal: number;
  readonly requiresDataLossReceipt: boolean;
  readonly summaryText: string;
  readonly dataLossBannerText: string | null;
}

export interface BlastRadiusDiffOptions {
  readonly rows: readonly BlastRadiusRow[];
  readonly operationKind?: BlastOperationKind;
  readonly requiresDataLossReceipt?: boolean;
  readonly locale?: WorkstationLocale;
  readonly disclosure?: DisclosureLevel;
  readonly doc?: Document;
}

function escapeHtml(raw: string): string {
  return raw
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

function symbolCssModifier(sym: BlastDiffSymbol): "add" | "mod" | "del" | "exempt" {
  switch (sym) {
    case "+":
      return "add";
    case "~":
      return "mod";
    case "-":
      return "del";
    case "=":
      return "exempt";
  }
}

export function computeBlastRadiusSummary(
  options: BlastRadiusDiffOptions,
): BlastRadiusSummary {
  let addedCount = 0;
  let modifiedCount = 0;
  let removedCount = 0;
  let exemptedCount = 0;
  let hasPurgedRow = false;

  for (const row of options.rows) {
    switch (row.symbol) {
      case "+":
        addedCount += 1;
        break;
      case "~":
        modifiedCount += 1;
        break;
      case "-":
        removedCount += 1;
        break;
      case "=":
        exemptedCount += 1;
        break;
    }
    if (row.category === "purged") {
      hasPurgedRow = true;
    }
  }

  const affectedTotal = addedCount + modifiedCount + removedCount;
  const requiresDataLossReceipt =
    Boolean(options.requiresDataLossReceipt) ||
    options.operationKind === "topic-purge" ||
    hasPurgedRow;

  const locale: WorkstationLocale = options.locale ?? "zh-CN";
  const disclosure: DisclosureLevel = options.disclosure ?? "L2";

  const summaryText =
    locale === "zh-CN"
      ? `变更影响预览：+${addedCount} 新增/重验 · ~${modifiedCount} 暂停/限流 · -${removedCount} 终止/清空 · =${exemptedCount} 豁免不受影响`
      : `Blast Radius: +${addedCount} create/reverify · ~${modifiedCount} pause/fence · -${removedCount} terminate/purge · =${exemptedCount} exempted`;

  let dataLossBannerText: string | null = null;
  if (requiresDataLossReceipt) {
    const baseText =
      locale === "zh-CN"
        ? "高危数据清空警告：清空通道积压将永久丢弃未消费消息，执行后必须生成不可篡改的数据损失审计凭证"
        : "Data-Loss Warning: Purging topic backlog permanently discards unconsumed messages and emits an auditable Data-Loss Receipt";
    dataLossBannerText =
      disclosure === "L3" ? `${baseText} [Data-Loss Receipt · v0.5 §25.3]` : baseText;
  }

  return {
    addedCount,
    modifiedCount,
    removedCount,
    exemptedCount,
    affectedTotal,
    requiresDataLossReceipt,
    summaryText,
    dataLossBannerText,
  };
}

export function renderBlastRadiusDiffHtml(options: BlastRadiusDiffOptions): string {
  const locale: WorkstationLocale = options.locale ?? "zh-CN";
  const disclosure: DisclosureLevel = options.disclosure ?? "L2";
  const summary = computeBlastRadiusSummary(options);

  const bannerHtml =
    summary.dataLossBannerText !== null
      ? `<div class="nlos-blast-diff__dataloss" role="alert" data-dataloss-receipt="required">` +
        `${renderIconSvg("danger", { size: 16, strokeWidth: 1.5 })}` +
        `<span>${escapeHtml(summary.dataLossBannerText)}</span></div>`
      : "";

  const colOp = locale === "zh-CN" ? "变更" : "Op";
  const colTarget = locale === "zh-CN" ? "受影响对象" : "Target";
  const colReason = locale === "zh-CN" ? "级联效果说明" : "Effect";

  const rowsHtml = options.rows
    .map((row) => {
      const mod = symbolCssModifier(row.symbol);
      const l3Hex =
        disclosure === "L3" && row.targetIdHex
          ? ` <code class="nlos-status-enum">${escapeHtml(row.targetIdHex.slice(0, 12))}</code>`
          : "";
      return (
        `<tr class="nlos-table-row" data-diff-op="${row.symbol}" data-diff-category="${row.category}">` +
        `<td><span class="nlos-blast-diff__op nlos-blast-diff__op--${mod}">${escapeHtml(row.symbol)}</span></td>` +
        `<td>${escapeHtml(row.scopeLabel)}: ${escapeHtml(row.targetLabel)}${l3Hex}</td>` +
        `<td>${escapeHtml(row.reason)}</td>` +
        `</tr>`
      );
    })
    .join("");

  return (
    `<section class="nlos-blast-diff" aria-label="Blast Radius Preview">` +
    `<div class="nlos-blast-diff__summary">` +
    `<span>${escapeHtml(summary.summaryText)}</span>` +
    `</div>` +
    `${bannerHtml}` +
    `<table class="nlos-table">` +
    `<thead><tr><th>${escapeHtml(colOp)}</th><th>${escapeHtml(colTarget)}</th><th>${escapeHtml(colReason)}</th></tr></thead>` +
    `<tbody>${rowsHtml}</tbody>` +
    `</table>` +
    `</section>`
  );
}

export function createBlastRadiusDiffElement(
  options: BlastRadiusDiffOptions,
): HTMLElement {
  const doc = options.doc ?? document;
  const locale: WorkstationLocale = options.locale ?? "zh-CN";
  const disclosure: DisclosureLevel = options.disclosure ?? "L2";
  const summary = computeBlastRadiusSummary(options);

  const section = doc.createElement("section");
  section.className = "nlos-blast-diff";
  section.setAttribute("aria-label", "Blast Radius Preview");

  const summaryBar = doc.createElement("div");
  summaryBar.className = "nlos-blast-diff__summary";
  const summarySpan = doc.createElement("span");
  summarySpan.textContent = summary.summaryText;
  summaryBar.appendChild(summarySpan);
  section.appendChild(summaryBar);

  if (summary.dataLossBannerText !== null) {
    const alertBar = doc.createElement("div");
    alertBar.className = "nlos-blast-diff__dataloss";
    alertBar.setAttribute("role", "alert");
    alertBar.setAttribute("data-dataloss-receipt", "required");
    alertBar.appendChild(createIconElement("danger", { size: 16, strokeWidth: 1.5, doc }));
    const alertText = doc.createElement("span");
    alertText.textContent = summary.dataLossBannerText;
    alertBar.appendChild(alertText);
    section.appendChild(alertBar);
  }

  const table = doc.createElement("table");
  table.className = "nlos-table";

  const thead = doc.createElement("thead");
  const headTr = doc.createElement("tr");
  const headers =
    locale === "zh-CN"
      ? ["变更", "受影响对象", "级联效果说明"]
      : ["Op", "Target", "Effect"];
  for (const h of headers) {
    const th = doc.createElement("th");
    th.textContent = h;
    headTr.appendChild(th);
  }
  thead.appendChild(headTr);
  table.appendChild(thead);

  const tbody = doc.createElement("tbody");
  for (const row of options.rows) {
    const tr = doc.createElement("tr");
    tr.className = "nlos-table-row";
    tr.setAttribute("data-diff-op", row.symbol);
    tr.setAttribute("data-diff-category", row.category);

    const tdOp = doc.createElement("td");
    const opBadge = doc.createElement("span");
    opBadge.className = `nlos-blast-diff__op nlos-blast-diff__op--${symbolCssModifier(row.symbol)}`;
    opBadge.textContent = row.symbol;
    tdOp.appendChild(opBadge);

    const tdTarget = doc.createElement("td");
    tdTarget.appendChild(doc.createTextNode(`${row.scopeLabel}: ${row.targetLabel}`));
    if (disclosure === "L3" && row.targetIdHex) {
      tdTarget.appendChild(doc.createTextNode(" "));
      const hexCode = doc.createElement("code");
      hexCode.className = "nlos-status-enum";
      hexCode.textContent = row.targetIdHex.slice(0, 12);
      tdTarget.appendChild(hexCode);
    }

    const tdReason = doc.createElement("td");
    tdReason.textContent = row.reason;

    tr.appendChild(tdOp);
    tr.appendChild(tdTarget);
    tr.appendChild(tdReason);
    tbody.appendChild(tr);
  }
  table.appendChild(tbody);
  section.appendChild(table);

  return section;
}

/* ==========================================================================
   5-Second Two-Step Arming Countdown Helper ([CTRL-SAFETY-001])
   ========================================================================== */

export type TwoStepArmingState = "idle" | "armed" | "confirmed";
export type TwoStepPressResult = "armed" | "guard-blocked" | "confirmed";

export interface TwoStepArmingSnapshot {
  readonly state: TwoStepArmingState;
  readonly remainingMs: number;
  readonly remainingSecondsLabel: string;
}

export interface TwoStepArmingOptions {
  readonly windowMs?: number;
  readonly minGuardMs?: number;
  readonly nowMs?: () => number;
  readonly onConfirm?: () => void;
  readonly onStateChange?: (snapshot: TwoStepArmingSnapshot) => void;
}

export interface TwoStepArmingController {
  press(): TwoStepPressResult;
  cancel(): void;
  reset(): void;
  getSnapshot(): TwoStepArmingSnapshot;
}

export function formatArmingSeconds(remainingMs: number): string {
  const clamped = Math.max(0, remainingMs);
  const secs = Math.ceil(clamped / 100) / 10;
  return `${secs.toFixed(1)}s`;
}

export function createTwoStepArmingController(
  options?: TwoStepArmingOptions,
): TwoStepArmingController {
  const windowMs = Math.max(500, options?.windowMs ?? ARMING_WINDOW_MS);
  const minGuardMs = Math.max(0, options?.minGuardMs ?? 0);
  const nowMs = options?.nowMs ?? (() => Date.now());

  let state: TwoStepArmingState = "idle";
  let armedAtMs: number | null = null;

  const computeSnapshot = (t: number): TwoStepArmingSnapshot => {
    if (state === "armed" && armedAtMs !== null) {
      const elapsed = t - armedAtMs;
      if (elapsed >= windowMs) {
        state = "idle";
        armedAtMs = null;
        return { state: "idle", remainingMs: 0, remainingSecondsLabel: "0.0s" };
      }
      const remainingMs = Math.max(0, windowMs - elapsed);
      return {
        state: "armed",
        remainingMs,
        remainingSecondsLabel: formatArmingSeconds(remainingMs),
      };
    }
    return {
      state,
      remainingMs: 0,
      remainingSecondsLabel: "0.0s",
    };
  };

  const emit = (t: number): void => {
    options?.onStateChange?.(computeSnapshot(t));
  };

  const press = (): TwoStepPressResult => {
    const t = nowMs();
    const current = computeSnapshot(t);

    if (current.state !== "armed" || armedAtMs === null) {
      state = "armed";
      armedAtMs = t;
      emit(t);
      return "armed";
    }

    const elapsed = t - armedAtMs;
    if (elapsed < minGuardMs) {
      return "guard-blocked";
    }

    state = "confirmed";
    armedAtMs = null;
    emit(t);
    options?.onConfirm?.();
    return "confirmed";
  };

  const cancel = (): void => {
    state = "idle";
    armedAtMs = null;
    emit(nowMs());
  };

  return {
    press,
    cancel,
    reset: cancel,
    getSnapshot: () => computeSnapshot(nowMs()),
  };
}
