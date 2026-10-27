// SOTA-4 (HashiCorp Nomad): 72-96px x 6px Solid Triple-Watermark Bullet Micro-Bar.
// Authority: Stage D Desktop Visual & UX Spec §2A.1 (SOTA-4), §3.2.B.4, ADR-0025 Decision 4.
// Simultaneously encodes:
//   1. Inner solid fill: Actual Usage
//   2. Middle 1.5px vertical tick: Effective Granted Limit or Local Cell Lease Cap
//   3. Outer 1px hairline recessed trough: Requested Ceiling

import type { DisclosureLevel, WorkstationLocale } from "./status-indicator";

export const BULLET_BAR_MIN_WIDTH_PX = 72;
export const BULLET_BAR_MAX_WIDTH_PX = 96;
export const BULLET_BAR_DEFAULT_WIDTH_PX = 84;
export const BULLET_BAR_HEIGHT_PX = 6;

export type BulletWatermarkState =
  | "zero"
  | "nominal"
  | "throttled-or-shrunk"
  | "over-effective-cap";

export interface BulletMicroBarInput {
  readonly actualUsage: number;
  readonly effectiveGrantedOrLeaseCap: number;
  readonly requestedCeiling: number;
  readonly unit?: string;
  readonly widthPx?: number;
  readonly locale?: WorkstationLocale;
  readonly disclosure?: DisclosureLevel;
  readonly doc?: Document;
}

export interface BulletMicroBarModel {
  readonly actualUsage: number;
  readonly effectiveCap: number;
  readonly requestedCeiling: number;
  readonly scaleMax: number;
  readonly actualPct: number;
  readonly effectivePct: number;
  readonly widthPx: number;
  readonly state: BulletWatermarkState;
  readonly readoutText: string;
  readonly ariaLabel: string;
}

function sanitizeNonNegative(value: number): number {
  if (!Number.isFinite(value) || value <= 0) {
    return 0;
  }
  return value;
}

function clampWidthPx(widthPx?: number): number {
  if (widthPx === undefined || !Number.isFinite(widthPx)) {
    return BULLET_BAR_DEFAULT_WIDTH_PX;
  }
  return Math.min(BULLET_BAR_MAX_WIDTH_PX, Math.max(BULLET_BAR_MIN_WIDTH_PX, Math.round(widthPx)));
}

function formatCompactNumber(value: number): string {
  if (Number.isInteger(value)) {
    return String(value);
  }
  return Number(value.toFixed(2)).toString();
}

function roundPct(pct: number): number {
  const clamped = Math.min(100, Math.max(0, pct));
  return Math.round(clamped * 100) / 100;
}

function escapeHtml(raw: string): string {
  return raw
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

/** Deterministic Nomad triple-watermark model handling zero, throttled, and shrunk-lease edge cases. */
export function computeBulletMicroBarModel(input: BulletMicroBarInput): BulletMicroBarModel {
  const actual = sanitizeNonNegative(input.actualUsage);
  const effective = sanitizeNonNegative(input.effectiveGrantedOrLeaseCap);
  const requested = sanitizeNonNegative(input.requestedCeiling);
  const widthPx = clampWidthPx(input.widthPx);
  const locale: WorkstationLocale = input.locale ?? "zh-CN";
  const disclosure: DisclosureLevel = input.disclosure ?? "L1";
  const unitSuffix = input.unit && input.unit.trim().length > 0 ? ` ${input.unit.trim()}` : "";

  const scaleMax = Math.max(requested, effective, actual);

  let state: BulletWatermarkState;
  let actualPct = 0;
  let effectivePct = 0;

  if (scaleMax === 0) {
    state = "zero";
  } else {
    actualPct = roundPct((actual / scaleMax) * 100);
    effectivePct = roundPct((effective / scaleMax) * 100);
    if (actual > effective) {
      state = "over-effective-cap";
    } else if (effective < requested) {
      state = "throttled-or-shrunk";
    } else {
      state = "nominal";
    }
  }

  const actualStr = formatCompactNumber(actual);
  const effectiveStr = formatCompactNumber(effective);
  const requestedStr = formatCompactNumber(requested);
  const readoutText = `${actualStr}/${effectiveStr}/${requestedStr}${unitSuffix}`;

  // Zero internal jargon in L0/L1/L2 (never emit "Quota" or raw protocol tokens).
  let ariaLabel: string;
  if (locale === "zh-CN") {
    ariaLabel = `已使用 ${actualStr}，当前可用上限 ${effectiveStr}，申请上限 ${requestedStr}${unitSuffix}`;
  } else {
    ariaLabel = `Used ${actualStr}, available limit ${effectiveStr}, requested ceiling ${requestedStr}${unitSuffix}`;
  }
  if (disclosure === "L3") {
    ariaLabel += ` [watermark=${state}]`;
  }

  return {
    actualUsage: actual,
    effectiveCap: effective,
    requestedCeiling: requested,
    scaleMax,
    actualPct,
    effectivePct,
    widthPx,
    state,
    readoutText,
    ariaLabel,
  };
}

export function renderBulletMicroBarHtml(input: BulletMicroBarInput): string {
  const model = computeBulletMicroBarModel(input);
  const tickLeft = model.effectivePct >= 100 ? "calc(100% - 1.5px)" : `${model.effectivePct}%`;
  return (
    `<span class="nlos-bullet-bar nlos-bullet-bar--${model.state}" ` +
    `data-watermark-state="${model.state}" role="meter" ` +
    `aria-valuemin="0" aria-valuemax="${model.scaleMax}" aria-valuenow="${model.actualUsage}" ` +
    `aria-label="${escapeHtml(model.ariaLabel)}">` +
    `<span class="nlos-bullet-bar__trough" style="width:${model.widthPx}px" aria-hidden="true">` +
    `<span class="nlos-bullet-bar__fill" style="width:${model.actualPct}%"></span>` +
    `<span class="nlos-bullet-bar__cap-tick" style="left:${tickLeft}"></span>` +
    `</span>` +
    `<span class="nlos-bullet-bar__readout">${escapeHtml(model.readoutText)}</span>` +
    `</span>`
  );
}

export function createBulletMicroBarElement(input: BulletMicroBarInput): HTMLElement {
  const doc = input.doc ?? document;
  const model = computeBulletMicroBarModel(input);
  const tickLeft = model.effectivePct >= 100 ? "calc(100% - 1.5px)" : `${model.effectivePct}%`;

  const root = doc.createElement("span");
  root.className = `nlos-bullet-bar nlos-bullet-bar--${model.state}`;
  root.setAttribute("data-watermark-state", model.state);
  root.setAttribute("role", "meter");
  root.setAttribute("aria-valuemin", "0");
  root.setAttribute("aria-valuemax", String(model.scaleMax));
  root.setAttribute("aria-valuenow", String(model.actualUsage));
  root.setAttribute("aria-label", model.ariaLabel);

  const trough = doc.createElement("span");
  trough.className = "nlos-bullet-bar__trough";
  trough.style.width = `${model.widthPx}px`;
  trough.setAttribute("aria-hidden", "true");

  const fill = doc.createElement("span");
  fill.className = "nlos-bullet-bar__fill";
  fill.style.width = `${model.actualPct}%`;
  trough.appendChild(fill);

  const tick = doc.createElement("span");
  tick.className = "nlos-bullet-bar__cap-tick";
  tick.style.left = tickLeft;
  trough.appendChild(tick);

  const readout = doc.createElement("span");
  readout.className = "nlos-bullet-bar__readout";
  readout.textContent = model.readoutText;

  root.appendChild(trough);
  root.appendChild(readout);
  return root;
}
