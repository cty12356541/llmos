// Non-Occludable System Security Screen (Plane 900, Solid Opaque Surface, Zero Blur Filter).
// Authority: 06-Architecture Spec v0.5 §24.1-24.2, §25.2-25.4; ADR-0025 Decisions 1, 3, 4;
// Stage D Visual & UX Spec §1.3, §2A.1, §2A.3, §3.5.

import {
  computeBlastRadiusSummary,
  createBlastRadiusDiffElement,
  renderBlastRadiusDiffHtml,
  type BlastOperationKind,
  type BlastRadiusRow,
} from "../ui/blast-radius-diff";
import { createIconElement, renderIconSvg } from "../ui/icons";
import {
  createStatutoryLockBadgeElement,
  pruneVerbsAgainstStatutoryLock,
  renderStatutoryLockBadgeHtml,
  type DisclosureLevel,
  type WorkstationLocale,
} from "../ui/status-indicator";
import {
  SEAL_HEIGHT_PX,
  SEAL_WIDTH_PX,
  createVerificationSeal,
  renderVerificationSealFallbackHtml,
  type VerificationSealController,
  type VerificationSealOptions,
} from "../ui/verification-seal";

export const Z4_SECURITY_Z_INDEX = 900;
export const Z2_SURFACE_Z_CEILING = 299;
export const Z4_SCRIM_RGBA = "rgba(10, 11, 13, 0.78)";
export const Z4_DIALOG_RADIUS_PX = 6;
export const Z4_TOP_SECURITY_BAR_PX = 2;
export const Z4_SEAL_HOUSING_WIDTH_PX = SEAL_WIDTH_PX;
export const Z4_SEAL_HOUSING_HEIGHT_PX = SEAL_HEIGHT_PX;
export const Z4_ARMING_WINDOW_MS = 350;
export const Z4_IRREVERSIBLE_CRIMSON_HEX = "#DC2626";

export type TrustedSessionLifecycle = "LOCKED" | "ACTIVE" | "SUSPENDED" | "CLOSED";

export type TrustedUiInputOrigin = "trusted_shell" | "untrusted_application_surface";

export type TrustedUiInputSource =
  | "KEYBOARD"
  | "POINTER"
  | "VOICE"
  | "DRAG_DROP"
  | "AUTOMATION";

export type TrustedUiCallerOrigin =
  | { readonly kind: "tcb_trusted_broker" }
  | {
      readonly kind: "untrusted_z2_surface";
      readonly surfaceIdHex: string;
      readonly applicationIdHex: string;
      readonly requestedZIndex?: number;
    };

export type ReversibilityClass =
  | "LOSSLESS_UNDO"
  | "COMPENSATABLE_ROLLBACK"
  | "IRREVERSIBLE";

export type ConsentGrantScope =
  | "SINGLE_ACTION"
  | "CURRENT_TASK"
  | "CURRENT_SESSION";

export type Z4ChallengeState =
  | "PENDING"
  | "APPROVED"
  | "CANCELLED_BY_USER"
  | "INVALIDATED_BY_SESSION_FENCE";

export type TrustedUiRejectionCode =
  | "UNTRUSTED_Z4_FORGE_ATTEMPT"
  | "Z4_PLANE_OCCLUSION_REJECTED"
  | "UNTRUSTED_SEAL_READ_DENIED"
  | "CLICKJACKING_ARMING_ACTIVE"
  | "UNTRUSTED_INPUT_ORIGIN"
  | "SESSION_NOT_ACTIVE_OR_STALE_GENERATION"
  | "CHALLENGE_NOT_FOUND_OR_EXPIRED"
  | "CHALLENGE_ALREADY_RESOLVED"
  | "IRREVERSIBLE_HIGH_CONTRAST_ACK_REQUIRED"
  | "IRREVERSIBLE_STATUTORY_LOCK_VIOLATION"
  | "CHOOSER_RIGHTS_EXCEED_MODE_CEILING"
  | "CHOOSER_INVALID_EXPORT_NAME"
  | "CHOOSER_ITEM_NOT_IN_VAULT"
  | "CHOOSER_GRANT_NOT_FOUND"
  | "CHOOSER_GRANT_REVOKED"
  | "CHOOSER_GRANT_EXPIRED"
  | "CHOOSER_GRANT_APPLICATION_MISMATCH"
  | "CHOOSER_GRANT_HANDLE_OUT_OF_SCOPE"
  | "CHOOSER_GRANT_INSUFFICIENT_RIGHTS"
  | "MISSING_USER_GESTURE"
  | "INVALID_OR_EXPIRED_USER_GESTURE"
  | "AUTOMATION_GESTURE_FORBIDDEN"
  | "TRANSFER_NOT_FOUND"
  | "TRANSFER_EXPIRED"
  | "TRANSFER_CONSENT_MISMATCH_OR_DENIED"
  | "INVALID_ARGUMENT";

export class TrustedUiRejectionError extends Error {
  public readonly code: TrustedUiRejectionCode;

  public constructor(code: TrustedUiRejectionCode, message: string) {
    super(`[${code}] ${message}`);
    this.name = "TrustedUiRejectionError";
    this.code = code;
  }
}

export interface Z4RequestingAppRow {
  readonly displayName: string;
  readonly publisherName: string;
  readonly signatureVerified: boolean;
  readonly applicationIdHex: string;
  readonly installationIdHex: string;
  readonly principalIdHex: string;
  readonly installationGeneration: number;
  readonly manifestDigestHex: string;
}

export interface Z4ActionScopeRow {
  readonly plainSummary: Readonly<Record<WorkstationLocale, string>>;
  readonly capabilityRights: readonly string[];
  readonly controlScope: string;
  readonly planDigestHex: string;
}

export interface Z4ResourceCeilingRow {
  readonly plainCeiling: Readonly<Record<WorkstationLocale, string>>;
  readonly memoryCeilingMib: number;
  readonly tokenBudgetCeiling: number;
  readonly networkEgressAllowed: boolean;
  readonly maxDurationMs: number;
}

export interface Z4ReversibilityRiskRow {
  readonly reversibilityClass: ReversibilityClass;
  readonly plainConsequence: Readonly<Record<WorkstationLocale, string>>;
  readonly multiTargetCount: number;
  readonly partialFailureDisclosure?: Readonly<Record<WorkstationLocale, string>>;
  readonly blastRadiusRows?: readonly BlastRadiusRow[];
  readonly blastOperationKind?: BlastOperationKind;
  readonly requiresDataLossReceipt?: boolean;
}

export interface Z4ConsentChallengeSpec {
  readonly requestingApp: Z4RequestingAppRow;
  readonly actionScope: Z4ActionScopeRow;
  readonly resourceCeiling: Z4ResourceCeilingRow;
  readonly reversibilityAndRisk: Z4ReversibilityRiskRow;
  readonly allowedGrantScopes?: readonly ConsentGrantScope[];
}

export interface Z4TrustedChallengeSnapshot {
  readonly challengeIdHex: string;
  readonly state: Z4ChallengeState;
  readonly zIndex: number;
  readonly scrimRgba: string;
  readonly zeroBlurFilter: true;
  readonly dialogRadiusPx: number;
  readonly topSecurityBarPx: number;
  readonly sealWidthPx: number;
  readonly sealHeightPx: number;
  readonly securitySealHint: string;
  readonly sessionGeneration: number;
  readonly presentedAtMonoMs: number;
  readonly armedFromMonoMs: number;
  readonly armingWindowMs: number;
  readonly requestingApp: Z4RequestingAppRow;
  readonly actionScope: Z4ActionScopeRow;
  readonly resourceCeiling: Z4ResourceCeilingRow;
  readonly reversibilityAndRisk: Required<
    Pick<
      Z4ReversibilityRiskRow,
      "reversibilityClass" | "plainConsequence" | "multiTargetCount" | "requiresDataLossReceipt"
    >
  > & {
    readonly partialFailureDisclosure: Readonly<Record<WorkstationLocale, string>> | null;
    readonly blastRadiusRows: readonly BlastRadiusRow[];
    readonly blastOperationKind: BlastOperationKind;
  };
  readonly undoAffordanceAllowed: boolean;
  readonly requiresHighContrastAck: boolean;
  readonly irreversibleAckChecked: boolean;
  readonly selectedGrantScope: ConsentGrantScope;
  readonly allowedGrantScopes: readonly ConsentGrantScope[];
  readonly candidateReceiptVerbs: readonly string[];
}

/** Public posture visible to lower planes — strictly omits `securitySealHint`. */
export interface LowerPlaneTrustedPosture {
  readonly z4ModalActive: boolean;
  readonly z4ZIndex: number;
  readonly z2ZCeiling: number;
  readonly activeChallengeIdHex: string | null;
  readonly sessionGeneration: number;
}

export interface Z4ConsentDecisionReceipt {
  readonly receiptIdHex: string;
  readonly challengeIdHex: string;
  readonly state: Z4ChallengeState;
  readonly chosenGrantScope: ConsentGrantScope | null;
  readonly reversibilityClass: ReversibilityClass;
  readonly undoAffordanceAllowed: boolean;
  readonly requiresDataLossReceipt: boolean;
  readonly decidedAtMonoMs: number;
  readonly sessionGeneration: number;
  readonly decisionDigestHex: string;
  readonly allowedFollowUpVerbs: readonly string[];
}

export interface Z4SecurityAuditCounters {
  z4ChallengesOpened: number;
  z4ChallengesApproved: number;
  z4ChallengesCancelled: number;
  z4ChallengesInvalidatedBySession: number;
  rejectedUntrustedZ4ForgeAttempts: number;
  rejectedZ4OcclusionAttempts: number;
  rejectedUntrustedSealReads: number;
  rejectedClickjackingArmingAttempts: number;
  rejectedUntrustedConfirmSpoofs: number;
  rejectedIrreversibleAckMissing: number;
  rejectedIrreversibleUndoAttempts: number;
}

export interface Z4SecurityDialogOptions {
  readonly initialSessionState?: TrustedSessionLifecycle;
  readonly initialSessionGeneration?: number;
  readonly initialSecuritySealHint?: string;
  readonly locale?: WorkstationLocale;
  readonly disclosure?: DisclosureLevel;
  readonly sealOptions?: VerificationSealOptions;
  readonly doc?: Document;
}

export interface Z4DecideInput {
  readonly origin: TrustedUiInputOrigin;
  readonly source: TrustedUiInputSource;
  readonly confirm: boolean;
  readonly key?: string;
  readonly nowMonoMs: number;
  readonly grantScope?: ConsentGrantScope;
  readonly irreversibleHighContrastAck?: boolean;
}

export interface Z4RenderOptions {
  readonly locale?: WorkstationLocale;
  readonly disclosure?: DisclosureLevel;
  readonly nowMonoMs?: number;
  readonly doc?: Document;
}

export function escapeHtml(raw: string): string {
  return raw
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

/** Deterministic 256-bit digest formatted as 64 lowercase hex characters (pure TS, zero Node/WebCrypto dependency). */
export function deterministicDomainDigestHex(domain: string, parts: readonly string[]): string {
  const k = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
  ];
  const utf8 = encodeUtf8LengthPrefixed(domain, parts);
  const bitLen = utf8.length * 8;
  const paddedLen = Math.ceil((utf8.length + 9) / 64) * 64;
  const padded = new Uint8Array(paddedLen);
  padded.set(utf8);
  padded[utf8.length] = 0x80;
  const hiBits = Math.floor(bitLen / 0x100000000);
  const loBits = bitLen >>> 0;
  const view = new DataView(padded.buffer);
  view.setUint32(paddedLen - 8, hiBits, false);
  view.setUint32(paddedLen - 4, loBits, false);

  const state = new Uint32Array([
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
    0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
  ]);
  const w = new Uint32Array(64);
  const rotr = (x: number, n: number): number => ((x >>> n) | (x << (32 - n))) >>> 0;

  for (let offset = 0; offset < paddedLen; offset += 64) {
    for (let i = 0; i < 16; i += 1) {
      w[i] = view.getUint32(offset + i * 4, false);
    }
    for (let i = 16; i < 64; i += 1) {
      const w15 = w[i - 15]!;
      const w2 = w[i - 2]!;
      const s0 = (rotr(w15, 7) ^ rotr(w15, 18) ^ (w15 >>> 3)) >>> 0;
      const s1 = (rotr(w2, 17) ^ rotr(w2, 19) ^ (w2 >>> 10)) >>> 0;
      w[i] = (w[i - 16]! + s0 + w[i - 7]! + s1) >>> 0;
    }

    let a = state[0]!;
    let b = state[1]!;
    let c = state[2]!;
    let d = state[3]!;
    let e = state[4]!;
    let f = state[5]!;
    let g = state[6]!;
    let h = state[7]!;

    for (let i = 0; i < 64; i += 1) {
      const s1 = (rotr(e, 6) ^ rotr(e, 11) ^ rotr(e, 25)) >>> 0;
      const ch = ((e & f) ^ (~e & g)) >>> 0;
      const temp1 = (h + s1 + ch + k[i]! + w[i]!) >>> 0;
      const s0 = (rotr(a, 2) ^ rotr(a, 13) ^ rotr(a, 22)) >>> 0;
      const maj = ((a & b) ^ (a & c) ^ (b & c)) >>> 0;
      const temp2 = (s0 + maj) >>> 0;

      h = g;
      g = f;
      f = e;
      e = (d + temp1) >>> 0;
      d = c;
      c = b;
      b = a;
      a = (temp1 + temp2) >>> 0;
    }

    state[0] = (state[0]! + a) >>> 0;
    state[1] = (state[1]! + b) >>> 0;
    state[2] = (state[2]! + c) >>> 0;
    state[3] = (state[3]! + d) >>> 0;
    state[4] = (state[4]! + e) >>> 0;
    state[5] = (state[5]! + f) >>> 0;
    state[6] = (state[6]! + g) >>> 0;
    state[7] = (state[7]! + h) >>> 0;
  }

  let hexOut = "";
  for (let i = 0; i < 8; i += 1) {
    hexOut += state[i]!.toString(16).padStart(8, "0");
  }
  return hexOut;
}

function encodeUtf8LengthPrefixed(domain: string, parts: readonly string[]): Uint8Array {
  const encoder = new TextEncoder();
  const chunks: Uint8Array[] = [encoder.encode(domain), ...parts.map((p) => encoder.encode(p))];
  let total = 0;
  for (const chunk of chunks) {
    total += 8 + chunk.length;
  }
  const out = new Uint8Array(total);
  const view = new DataView(out.buffer);
  let cursor = 0;
  for (const chunk of chunks) {
    view.setUint32(cursor, 0, false);
    view.setUint32(cursor + 4, chunk.length >>> 0, false);
    out.set(chunk, cursor + 8);
    cursor += 8 + chunk.length;
  }
  return out;
}

interface Z4CopyPack {
  readonly headerTitle: string;
  readonly sealLabel: string;
  readonly verifiedPublisher: string;
  readonly unverifiedPublisher: string;
  readonly rowAppLabel: string;
  readonly rowScopeLabel: string;
  readonly rowBudgetLabel: string;
  readonly rowRiskLabel: string;
  readonly reversibilityLabels: Readonly<Record<ReversibilityClass, string>>;
  readonly grantScopeHeading: string;
  readonly grantScopeLabels: Readonly<Record<ConsentGrantScope, string>>;
  readonly irreversibleAckPrompt: string;
  readonly armingWaitPrefix: string;
  readonly cancelEscapeLabel: string;
  readonly approveLabel: string;
  readonly approveIrreversibleLabel: string;
}

const Z4_COPY: Readonly<Record<WorkstationLocale, Z4CopyPack>> = {
  "zh-CN": {
    headerTitle: "系统安全授权核验",
    sealLabel: "个人防伪核验短语",
    verifiedPublisher: "已核验签名来源",
    unverifiedPublisher: "未签名来源",
    rowAppLabel: "请求应用",
    rowScopeLabel: "请求操作范围",
    rowBudgetLabel: "资源与费用上限",
    rowRiskLabel: "可撤回性与风险等级",
    reversibilityLabels: {
      LOSSLESS_UNDO: "支持无损撤回",
      COMPENSATABLE_ROLLBACK: "支持补偿回退（保留审计记录）",
      IRREVERSIBLE: "不可撤回操作 · 执行后无法恢复",
    },
    grantScopeHeading: "授权生效范围",
    grantScopeLabels: {
      SINGLE_ACTION: "仅本次操作有效",
      CURRENT_TASK: "当前任务期间有效",
      CURRENT_SESSION: "当前登录会话内有效",
    },
    irreversibleAckPrompt: "我已知晓此操作不可撤回，执行后无撤销入口",
    armingWaitPrefix: "防误触保护中",
    cancelEscapeLabel: "拒绝并关闭 (Esc)",
    approveLabel: "批准执行",
    approveIrreversibleLabel: "确认执行不可撤回操作",
  },
  "en-US": {
    headerTitle: "System Security Verification",
    sealLabel: "Personal Verification Phrase",
    verifiedPublisher: "Verified Signature",
    unverifiedPublisher: "Unsigned Source",
    rowAppLabel: "Requesting App",
    rowScopeLabel: "Requested Action",
    rowBudgetLabel: "Resource & Cost Cap",
    rowRiskLabel: "Reversibility & Risk",
    reversibilityLabels: {
      LOSSLESS_UNDO: "Lossless undo available",
      COMPENSATABLE_ROLLBACK: "Compensating rollback available (audited)",
      IRREVERSIBLE: "Irreversible action · Cannot be undone after execution",
    },
    grantScopeHeading: "Approval Scope",
    grantScopeLabels: {
      SINGLE_ACTION: "This action only",
      CURRENT_TASK: "For current task",
      CURRENT_SESSION: "For current login session",
    },
    irreversibleAckPrompt: "I understand this action cannot be undone and exposes no Undo button",
    armingWaitPrefix: "Clickguard active",
    cancelEscapeLabel: "Reject & Close (Esc)",
    approveLabel: "Approve Action",
    approveIrreversibleLabel: "Confirm Irreversible Action",
  },
};

function computeCandidateReceiptVerbs(rev: ReversibilityClass): readonly string[] {
  const rawCandidates = ["Undo", "ViewEffectProof"];
  if (rev === "IRREVERSIBLE") {
    return pruneVerbsAgainstStatutoryLock(rawCandidates, "IRREVERSIBLE_NO_UNDO");
  }
  return rawCandidates;
}

/** Pure HTML renderer for the solid system security screen. */
export function renderZ4SecurityDialogHtml(
  snapshot: Z4TrustedChallengeSnapshot,
  options?: Z4RenderOptions,
): string {
  const locale: WorkstationLocale = options?.locale ?? "zh-CN";
  const disclosure: DisclosureLevel = options?.disclosure ?? "L2";
  const nowMonoMs = options?.nowMonoMs ?? snapshot.armedFromMonoMs;
  const copy = Z4_COPY[locale];

  const isIrreversible =
    snapshot.reversibilityAndRisk.reversibilityClass === "IRREVERSIBLE";
  const elapsedMs = Math.max(0, nowMonoMs - snapshot.armedFromMonoMs);
  const armingRemainingMs = Math.max(0, snapshot.armingWindowMs - elapsedMs);
  const confirmDisabled =
    armingRemainingMs > 0 || (isIrreversible && !snapshot.irreversibleAckChecked);

  const badgeText = snapshot.requestingApp.signatureVerified
    ? copy.verifiedPublisher
    : copy.unverifiedPublisher;
  const revLabel =
    copy.reversibilityLabels[snapshot.reversibilityAndRisk.reversibilityClass];

  const l3AppMeta =
    disclosure === "L3"
      ? `<div class="nlos-mono nlos-l3-only" data-z4-l3="app-ids">` +
        `ApplicationId=${escapeHtml(snapshot.requestingApp.applicationIdHex)} ` +
        `InstallationId=${escapeHtml(snapshot.requestingApp.installationIdHex)} ` +
        `PrincipalId=${escapeHtml(snapshot.requestingApp.principalIdHex)} ` +
        `gen=${snapshot.requestingApp.installationGeneration} ` +
        `manifest_digest=${escapeHtml(snapshot.requestingApp.manifestDigestHex)}` +
        `</div>`
      : "";

  const l3ScopeMeta =
    disclosure === "L3"
      ? `<div class="nlos-mono nlos-l3-only" data-z4-l3="scope-contract">` +
        `CapabilityRights=[${escapeHtml(snapshot.actionScope.capabilityRights.join(", "))}] ` +
        `ControlScope=${escapeHtml(snapshot.actionScope.controlScope)} ` +
        `plan_digest=${escapeHtml(snapshot.actionScope.planDigestHex)}` +
        `</div>`
      : "";

  const l3BudgetMeta =
    disclosure === "L3"
      ? `<div class="nlos-mono nlos-l3-only" data-z4-l3="budget-contract">` +
        `memory_mib=${snapshot.resourceCeiling.memoryCeilingMib} ` +
        `tokens=${snapshot.resourceCeiling.tokenBudgetCeiling} ` +
        `egress=${String(snapshot.resourceCeiling.networkEgressAllowed)} ` +
        `max_duration_ms=${snapshot.resourceCeiling.maxDurationMs}` +
        `</div>`
      : "";

  const l3RiskMeta =
    disclosure === "L3"
      ? `<div class="nlos-mono nlos-l3-only" data-z4-l3="risk-contract">` +
        `reversibility=${escapeHtml(snapshot.reversibilityAndRisk.reversibilityClass)} ` +
        `multi_target_count=${snapshot.reversibilityAndRisk.multiTargetCount}` +
        `</div>`
      : "";

  const partialFailureHtml =
    snapshot.reversibilityAndRisk.partialFailureDisclosure !== null
      ? `<div class="nlos-surface-recessed" data-z4-partial-failure="true" ` +
        `style="padding: 6px 8px; margin-top: 6px; border-radius: 4px;">` +
        `${renderIconSvg("warning", { size: 14, strokeWidth: 1.5 })} ` +
        `<span>${escapeHtml(snapshot.reversibilityAndRisk.partialFailureDisclosure[locale])}</span>` +
        `</div>`
      : "";

  const statutoryLockHtml = isIrreversible
    ? `<div style="margin-top: 6px;">` +
      renderStatutoryLockBadgeHtml({
        kind: "IRREVERSIBLE_NO_UNDO",
        locale,
        disclosure,
      }) +
      `</div>`
    : "";

  const blastDiffHtml =
    snapshot.reversibilityAndRisk.blastRadiusRows.length > 0
      ? `<div data-z4-blast-preview="true" style="margin-top: 8px;">` +
        renderBlastRadiusDiffHtml({
          rows: snapshot.reversibilityAndRisk.blastRadiusRows,
          operationKind: snapshot.reversibilityAndRisk.blastOperationKind,
          requiresDataLossReceipt:
            snapshot.reversibilityAndRisk.requiresDataLossReceipt,
          locale,
          disclosure,
        }) +
        `</div>`
      : "";

  const scopeRadiosHtml = snapshot.allowedGrantScopes
    .map((scope) => {
      const checkedAttr =
        scope === snapshot.selectedGrantScope ? ' checked="checked"' : "";
      return (
        `<label class="nlos-z4-scope-option" data-grant-scope="${escapeHtml(scope)}" ` +
        `style="display: inline-flex; align-items: center; gap: 6px; margin-right: 12px;">` +
        `<input type="radio" name="nlos-z4-grant-scope" value="${escapeHtml(scope)}"${checkedAttr} />` +
        `<span>${escapeHtml(copy.grantScopeLabels[scope])}</span>` +
        `</label>`
      );
    })
    .join("");

  const irreversibleAckHtml = isIrreversible
    ? `<div class="nlos-surface-recessed" data-z4-irreversible-ack-box="true" ` +
      `style="padding: 8px 10px; margin-top: 10px; border: 1px solid ${Z4_IRREVERSIBLE_CRIMSON_HEX}; border-radius: 4px;">` +
      `<label style="display: flex; align-items: center; gap: 8px; color: var(--nlos-signal-danger-text); font-weight: 600;">` +
      `<input type="checkbox" data-z4-irreversible-ack="true"${snapshot.irreversibleAckChecked ? ' checked="checked"' : ""} />` +
      `<span>${escapeHtml(copy.irreversibleAckPrompt)}</span>` +
      `</label>` +
      `</div>`
    : "";

  const approveBtnText =
    armingRemainingMs > 0
      ? `${copy.armingWaitPrefix} (${armingRemainingMs}ms)`
      : isIrreversible
        ? copy.approveIrreversibleLabel
        : copy.approveLabel;

  const approveBtnClass = isIrreversible
    ? "nlos-btn nlos-btn--danger"
    : "nlos-btn nlos-btn--primary";

  return (
    `<div class="nlos-scrim-z4" data-trusted-plane="security-screen" ` +
    `data-z-index="${Z4_SECURITY_Z_INDEX}" data-nlos-disclosure="${escapeHtml(disclosure)}" ` +
    `style="position: fixed; inset: 0; background-color: ${Z4_SCRIM_RGBA}; z-index: ${Z4_SECURITY_Z_INDEX}; display: flex; align-items: center; justify-content: center;">` +
    `<div class="nlos-surface-modal-z4" role="alertdialog" aria-modal="true" ` +
    `data-challenge-id="${escapeHtml(snapshot.challengeIdHex)}" ` +
    `data-reversibility-tone="${isIrreversible ? "danger" : "standard"}" ` +
    `style="border-radius: 6px; border-top-width: ${Z4_TOP_SECURITY_BAR_PX}px; padding: 14px 16px;">` +
    `<header class="nlos-hairline-bottom" style="display: flex; align-items: center; justify-content: space-between; padding-bottom: 10px; margin-bottom: 10px; gap: 12px;">` +
    `<div style="display: flex; align-items: center; gap: 8px; font-weight: 600;">` +
    `${renderIconSvg("shield-seal", { size: 16, strokeWidth: 1.5 })}` +
    `<span>${escapeHtml(copy.headerTitle)}</span>` +
    `</div>` +
    `<div data-z4-seal-cluster="true" style="display: inline-flex; align-items: center; gap: 8px;">` +
    `${renderVerificationSealFallbackHtml({ ariaLabel: copy.sealLabel })}` +
    `<div class="nlos-surface-recessed nlos-tabular" data-z4-seal-hint="true" ` +
    `style="padding: 2px 8px; border-radius: 2px; font-size: 12px;">` +
    `<span style="color: var(--nlos-text-secondary); margin-right: 6px;">${escapeHtml(copy.sealLabel)}:</span>` +
    `<strong>${escapeHtml(snapshot.securitySealHint)}</strong>` +
    `</div>` +
    `</div>` +
    `</header>` +
    `<table class="nlos-table" data-z4-kv-spec-table="true">` +
    `<tbody>` +
    `<tr class="nlos-table-row" data-z4-kv-row="requesting-app">` +
    `<th scope="row" style="width: 148px;">${escapeHtml(copy.rowAppLabel)}</th>` +
    `<td>` +
    `<div><strong>${escapeHtml(snapshot.requestingApp.displayName)}</strong> ` +
    `<span style="color: var(--nlos-text-secondary);">(${escapeHtml(snapshot.requestingApp.publisherName)} · ${escapeHtml(badgeText)})</span></div>` +
    `${l3AppMeta}` +
    `</td>` +
    `</tr>` +
    `<tr class="nlos-table-row" data-z4-kv-row="action-scope">` +
    `<th scope="row">${escapeHtml(copy.rowScopeLabel)}</th>` +
    `<td>` +
    `<div>${escapeHtml(snapshot.actionScope.plainSummary[locale])}</div>` +
    `${l3ScopeMeta}` +
    `</td>` +
    `</tr>` +
    `<tr class="nlos-table-row" data-z4-kv-row="resource-ceiling">` +
    `<th scope="row">${escapeHtml(copy.rowBudgetLabel)}</th>` +
    `<td>` +
    `<div>${escapeHtml(snapshot.resourceCeiling.plainCeiling[locale])}</div>` +
    `${l3BudgetMeta}` +
    `</td>` +
    `</tr>` +
    `<tr class="nlos-table-row" data-z4-kv-row="reversibility-risk">` +
    `<th scope="row">${escapeHtml(copy.rowRiskLabel)}</th>` +
    `<td>` +
    `<div style="${isIrreversible ? `color: ${Z4_IRREVERSIBLE_CRIMSON_HEX}; font-weight: 600;` : ""}">` +
    `${isIrreversible ? renderIconSvg("danger", { size: 14, strokeWidth: 1.5 }) + " " : ""}` +
    `${escapeHtml(revLabel)}` +
    `</div>` +
    `<div style="margin-top: 2px; color: var(--nlos-text-secondary);">${escapeHtml(snapshot.reversibilityAndRisk.plainConsequence[locale])}</div>` +
    `${statutoryLockHtml}` +
    `${partialFailureHtml}` +
    `${l3RiskMeta}` +
    `</td>` +
    `</tr>` +
    `</tbody>` +
    `</table>` +
    `${blastDiffHtml}` +
    `<div data-z4-grant-scopes="true" style="margin-top: 10px;">` +
    `<div style="font-size: 12px; color: var(--nlos-text-secondary); margin-bottom: 4px;">${escapeHtml(copy.grantScopeHeading)}</div>` +
    `<div>${scopeRadiosHtml}</div>` +
    `</div>` +
    `${irreversibleAckHtml}` +
    `<footer class="nlos-hairline-top" style="display: flex; align-items: center; justify-content: flex-end; gap: 8px; padding-top: 10px; margin-top: 12px;">` +
    `<button type="button" class="nlos-btn" data-z4-action="cancel">${escapeHtml(copy.cancelEscapeLabel)}</button>` +
    `<button type="button" class="${approveBtnClass}" data-z4-action="approve"${confirmDisabled ? ' disabled="disabled"' : ""}>${escapeHtml(approveBtnText)}</button>` +
    `</footer>` +
    `</div>` +
    `</div>`
  );
}

/** Stateful TCB controller for the Z4 System Security Screen. */
export class Z4SecurityDialogController {
  private sessionState: TrustedSessionLifecycle;
  private sessionGeneration: number;
  private securitySealHint: string;
  private locale: WorkstationLocale;
  private disclosure: DisclosureLevel;
  private readonly doc: Document | undefined;
  private readonly sealOptions: VerificationSealOptions | undefined;
  private activeSeal: VerificationSealController | null = null;
  private activeChallenge: Z4TrustedChallengeSnapshot | null = null;
  private readonly challenges = new Map<string, Z4TrustedChallengeSnapshot>();
  private readonly receipts = new Map<string, Z4ConsentDecisionReceipt>();
  private nextNonce = 0;

  public readonly audit: Z4SecurityAuditCounters = {
    z4ChallengesOpened: 0,
    z4ChallengesApproved: 0,
    z4ChallengesCancelled: 0,
    z4ChallengesInvalidatedBySession: 0,
    rejectedUntrustedZ4ForgeAttempts: 0,
    rejectedZ4OcclusionAttempts: 0,
    rejectedUntrustedSealReads: 0,
    rejectedClickjackingArmingAttempts: 0,
    rejectedUntrustedConfirmSpoofs: 0,
    rejectedIrreversibleAckMissing: 0,
    rejectedIrreversibleUndoAttempts: 0,
  };

  public constructor(options?: Z4SecurityDialogOptions) {
    this.sessionState = options?.initialSessionState ?? "ACTIVE";
    this.sessionGeneration = options?.initialSessionGeneration ?? 1;
    this.securitySealHint = options?.initialSecuritySealHint ?? "远山青松-4821";
    this.locale = options?.locale ?? "zh-CN";
    this.disclosure = options?.disclosure ?? "L2";
    this.doc = options?.doc;
    this.sealOptions = options?.sealOptions;
  }

  public setLocaleAndDisclosure(
    locale: WorkstationLocale,
    disclosure: DisclosureLevel,
  ): void {
    this.locale = locale;
    this.disclosure = disclosure;
  }

  public guardSurfaceZIndex(
    caller: TrustedUiCallerOrigin,
    requestedZIndex: number,
  ): number {
    if (caller.kind === "tcb_trusted_broker") {
      return requestedZIndex;
    }
    if (requestedZIndex > Z2_SURFACE_Z_CEILING) {
      this.audit.rejectedZ4OcclusionAttempts += 1;
      throw new TrustedUiRejectionError(
        "Z4_PLANE_OCCLUSION_REJECTED",
        `Untrusted surface ${caller.surfaceIdHex} requested z-index ${requestedZIndex} above ceiling ${Z2_SURFACE_Z_CEILING}`,
      );
    }
    return requestedZIndex;
  }

  public syncSession(
    nextState: TrustedSessionLifecycle,
    nextSessionGeneration: number,
    nextSecuritySealHint?: string,
  ): void {
    const stateInvalidates = nextState !== "ACTIVE";
    const genChanged = nextSessionGeneration !== this.sessionGeneration;
    this.sessionState = nextState;
    this.sessionGeneration = nextSessionGeneration;
    if (nextSecuritySealHint !== undefined && nextSecuritySealHint.trim().length > 0) {
      this.securitySealHint = nextSecuritySealHint.trim();
    }

    if ((stateInvalidates || genChanged) && this.activeChallenge !== null) {
      const invalidated: Z4TrustedChallengeSnapshot = {
        ...this.activeChallenge,
        state: "INVALIDATED_BY_SESSION_FENCE",
      };
      this.challenges.set(invalidated.challengeIdHex, invalidated);
      this.activeChallenge = null;
      this.audit.z4ChallengesInvalidatedBySession += 1;
      this.stopSealLoop();
    }
  }

  public getPublicPosture(): LowerPlaneTrustedPosture {
    return {
      z4ModalActive: this.activeChallenge !== null,
      z4ZIndex: Z4_SECURITY_Z_INDEX,
      z2ZCeiling: Z2_SURFACE_Z_CEILING,
      activeChallengeIdHex: this.activeChallenge?.challengeIdHex ?? null,
      sessionGeneration: this.sessionGeneration,
    };
  }

  public inspectTrustedChallenge(
    caller: TrustedUiCallerOrigin,
    challengeIdHex?: string,
  ): Z4TrustedChallengeSnapshot {
    if (caller.kind !== "tcb_trusted_broker") {
      this.audit.rejectedUntrustedSealReads += 1;
      throw new TrustedUiRejectionError(
        "UNTRUSTED_SEAL_READ_DENIED",
        "Private security_seal_hint projection is strictly confined to TCB plane 900",
      );
    }
    const targetId = challengeIdHex ?? this.activeChallenge?.challengeIdHex;
    if (!targetId) {
      throw new TrustedUiRejectionError(
        "CHALLENGE_NOT_FOUND_OR_EXPIRED",
        "No active security challenge",
      );
    }
    const snap = this.challenges.get(targetId);
    if (!snap) {
      throw new TrustedUiRejectionError(
        "CHALLENGE_NOT_FOUND_OR_EXPIRED",
        `Security challenge ${targetId} not found`,
      );
    }
    return snap;
  }

  public isSealAnimationRunning(): boolean {
    return this.activeSeal?.isRunning() ?? false;
  }

  public openChallenge(
    caller: TrustedUiCallerOrigin,
    spec: Z4ConsentChallengeSpec,
    nowMonoMs: number,
  ): Z4TrustedChallengeSnapshot {
    if (caller.kind === "untrusted_z2_surface") {
      if (
        caller.requestedZIndex !== undefined &&
        caller.requestedZIndex > Z2_SURFACE_Z_CEILING
      ) {
        this.audit.rejectedZ4OcclusionAttempts += 1;
        throw new TrustedUiRejectionError(
          "Z4_PLANE_OCCLUSION_REJECTED",
          `Untrusted surface ${caller.surfaceIdHex} attempted z-index ${caller.requestedZIndex} occlusion`,
        );
      }
      this.audit.rejectedUntrustedZ4ForgeAttempts += 1;
      throw new TrustedUiRejectionError(
        "UNTRUSTED_Z4_FORGE_ATTEMPT",
        `Untrusted surface ${caller.surfaceIdHex} forbidden from forging system security screen`,
      );
    }

    if (this.sessionState !== "ACTIVE") {
      throw new TrustedUiRejectionError(
        "SESSION_NOT_ACTIVE_OR_STALE_GENERATION",
        `Cannot open security challenge while session is ${this.sessionState}`,
      );
    }

    this.nextNonce += 1;
    const challengeDigest = deterministicDomainDigestHex(
      "llmos/stage-d/ts-z4-challenge/v1",
      [
        String(this.nextNonce),
        String(this.sessionGeneration),
        String(nowMonoMs),
        spec.requestingApp.applicationIdHex,
        spec.actionScope.planDigestHex,
      ],
    );
    const challengeIdHex = challengeDigest.slice(0, 32);

    const blastRows = spec.reversibilityAndRisk.blastRadiusRows
      ? [...spec.reversibilityAndRisk.blastRadiusRows]
      : [];
    const blastKind: BlastOperationKind =
      spec.reversibilityAndRisk.blastOperationKind ?? "generic-cascade";
    const blastSummary = computeBlastRadiusSummary({
      rows: blastRows,
      operationKind: blastKind,
      requiresDataLossReceipt: spec.reversibilityAndRisk.requiresDataLossReceipt,
      locale: this.locale,
      disclosure: this.disclosure,
    });

    const partialDisclosure: Readonly<Record<WorkstationLocale, string>> | null =
      spec.reversibilityAndRisk.multiTargetCount > 1
        ? (spec.reversibilityAndRisk.partialFailureDisclosure ?? {
            "zh-CN": `此操作涉及 ${spec.reversibilityAndRisk.multiTargetCount} 个目标；若中途部分失败，成功项与失败项将分别列出供逐项核查。`,
            "en-US": `Affects ${spec.reversibilityAndRisk.multiTargetCount} targets; if partial failure occurs, succeeded and failed items are listed separately.`,
          })
        : null;

    const revClass = spec.reversibilityAndRisk.reversibilityClass;
    const allowedScopes: readonly ConsentGrantScope[] =
      spec.allowedGrantScopes && spec.allowedGrantScopes.length > 0
        ? [...spec.allowedGrantScopes]
        : ["SINGLE_ACTION"];

    if (this.activeChallenge !== null) {
      const superseded: Z4TrustedChallengeSnapshot = {
        ...this.activeChallenge,
        state: "CANCELLED_BY_USER",
      };
      this.challenges.set(superseded.challengeIdHex, superseded);
      this.stopSealLoop();
    }

    const snapshot: Z4TrustedChallengeSnapshot = {
      challengeIdHex,
      state: "PENDING",
      zIndex: Z4_SECURITY_Z_INDEX,
      scrimRgba: Z4_SCRIM_RGBA,
      zeroBlurFilter: true,
      dialogRadiusPx: Z4_DIALOG_RADIUS_PX,
      topSecurityBarPx: Z4_TOP_SECURITY_BAR_PX,
      sealWidthPx: Z4_SEAL_HOUSING_WIDTH_PX,
      sealHeightPx: Z4_SEAL_HOUSING_HEIGHT_PX,
      securitySealHint: this.securitySealHint,
      sessionGeneration: this.sessionGeneration,
      presentedAtMonoMs: nowMonoMs,
      armedFromMonoMs: nowMonoMs,
      armingWindowMs: Z4_ARMING_WINDOW_MS,
      requestingApp: spec.requestingApp,
      actionScope: spec.actionScope,
      resourceCeiling: spec.resourceCeiling,
      reversibilityAndRisk: {
        reversibilityClass: revClass,
        plainConsequence: spec.reversibilityAndRisk.plainConsequence,
        multiTargetCount: spec.reversibilityAndRisk.multiTargetCount,
        requiresDataLossReceipt: blastSummary.requiresDataLossReceipt,
        partialFailureDisclosure: partialDisclosure,
        blastRadiusRows: blastRows,
        blastOperationKind: blastKind,
      },
      undoAffordanceAllowed: revClass !== "IRREVERSIBLE",
      requiresHighContrastAck: revClass === "IRREVERSIBLE",
      irreversibleAckChecked: false,
      selectedGrantScope: allowedScopes[0]!,
      allowedGrantScopes: allowedScopes,
      candidateReceiptVerbs: computeCandidateReceiptVerbs(revClass),
    };

    this.activeChallenge = snapshot;
    this.challenges.set(challengeIdHex, snapshot);
    this.audit.z4ChallengesOpened += 1;

    if (this.doc !== undefined) {
      this.activeSeal = createVerificationSeal({
        ...this.sealOptions,
        doc: this.doc,
        autoStart: true,
      });
    }

    return snapshot;
  }

  public notifyFocusRegained(
    caller: TrustedUiCallerOrigin,
    nowMonoMs: number,
  ): number {
    if (caller.kind !== "tcb_trusted_broker") {
      this.audit.rejectedUntrustedZ4ForgeAttempts += 1;
      throw new TrustedUiRejectionError(
        "UNTRUSTED_Z4_FORGE_ATTEMPT",
        "Only TCB broker may reset focus-regain arming clock",
      );
    }
    if (this.activeChallenge === null) {
      throw new TrustedUiRejectionError(
        "CHALLENGE_NOT_FOUND_OR_EXPIRED",
        "No active security challenge to re-arm",
      );
    }
    const nextArmed = Math.max(this.activeChallenge.armedFromMonoMs, nowMonoMs);
    const updated: Z4TrustedChallengeSnapshot = {
      ...this.activeChallenge,
      armedFromMonoMs: nextArmed,
    };
    this.activeChallenge = updated;
    this.challenges.set(updated.challengeIdHex, updated);
    return nextArmed;
  }

  public setIrreversibleAckChecked(checked: boolean): void {
    if (this.activeChallenge === null) {
      return;
    }
    const updated: Z4TrustedChallengeSnapshot = {
      ...this.activeChallenge,
      irreversibleAckChecked: checked,
    };
    this.activeChallenge = updated;
    this.challenges.set(updated.challengeIdHex, updated);
  }

  public setSelectedGrantScope(scope: ConsentGrantScope): void {
    if (
      this.activeChallenge === null ||
      !this.activeChallenge.allowedGrantScopes.includes(scope)
    ) {
      return;
    }
    const updated: Z4TrustedChallengeSnapshot = {
      ...this.activeChallenge,
      selectedGrantScope: scope,
    };
    this.activeChallenge = updated;
    this.challenges.set(updated.challengeIdHex, updated);
  }

  public decide(
    challengeIdHex: string,
    input: Z4DecideInput,
  ): Z4ConsentDecisionReceipt {
    const isEscapeCancel =
      !input.confirm ||
      (input.key !== undefined &&
        (input.key.toLowerCase() === "escape" || input.key.toLowerCase() === "esc"));

    if (
      input.origin !== "trusted_shell" ||
      (!isEscapeCancel && input.source === "AUTOMATION")
    ) {
      this.audit.rejectedUntrustedConfirmSpoofs += 1;
      throw new TrustedUiRejectionError(
        "UNTRUSTED_INPUT_ORIGIN",
        "Untrusted application surface or automated channel cannot decide system security screen",
      );
    }

    const snap = this.challenges.get(challengeIdHex);
    if (!snap) {
      throw new TrustedUiRejectionError(
        "CHALLENGE_NOT_FOUND_OR_EXPIRED",
        `Security challenge ${challengeIdHex} not found`,
      );
    }
    if (snap.state === "INVALIDATED_BY_SESSION_FENCE") {
      throw new TrustedUiRejectionError(
        "SESSION_NOT_ACTIVE_OR_STALE_GENERATION",
        `Security challenge ${challengeIdHex} invalidated by session lock or user switch`,
      );
    }
    if (snap.state !== "PENDING") {
      throw new TrustedUiRejectionError(
        "CHALLENGE_ALREADY_RESOLVED",
        `Security challenge ${challengeIdHex} already resolved as ${snap.state}`,
      );
    }

    const revClass = snap.reversibilityAndRisk.reversibilityClass;

    if (isEscapeCancel) {
      const cancelledSnap: Z4TrustedChallengeSnapshot = {
        ...snap,
        state: "CANCELLED_BY_USER",
      };
      this.challenges.set(challengeIdHex, cancelledSnap);
      if (this.activeChallenge?.challengeIdHex === challengeIdHex) {
        this.activeChallenge = null;
        this.stopSealLoop();
      }
      this.audit.z4ChallengesCancelled += 1;
      const receipt = this.mintDecisionReceipt(
        cancelledSnap,
        "CANCELLED_BY_USER",
        null,
        input.nowMonoMs,
      );
      this.receipts.set(receipt.receiptIdHex, receipt);
      return receipt;
    }

    if (
      this.sessionState !== "ACTIVE" ||
      this.sessionGeneration !== snap.sessionGeneration
    ) {
      this.syncSession(this.sessionState, this.sessionGeneration);
      throw new TrustedUiRejectionError(
        "SESSION_NOT_ACTIVE_OR_STALE_GENERATION",
        "Session locked or generation advanced before approval",
      );
    }

    const elapsed = input.nowMonoMs - snap.armedFromMonoMs;
    if (elapsed < Z4_ARMING_WINDOW_MS) {
      this.audit.rejectedClickjackingArmingAttempts += 1;
      throw new TrustedUiRejectionError(
        "CLICKJACKING_ARMING_ACTIVE",
        `Confirm click rejected within ${Z4_ARMING_WINDOW_MS}ms arming window (elapsed ${elapsed}ms)`,
      );
    }

    const ackSatisfied =
      input.irreversibleHighContrastAck ?? snap.irreversibleAckChecked;
    if (revClass === "IRREVERSIBLE" && !ackSatisfied) {
      this.audit.rejectedIrreversibleAckMissing += 1;
      throw new TrustedUiRejectionError(
        "IRREVERSIBLE_HIGH_CONTRAST_ACK_REQUIRED",
        "Irreversible action requires explicit high-contrast user confirmation",
      );
    }

    const chosenScope = input.grantScope ?? snap.selectedGrantScope;
    if (!snap.allowedGrantScopes.includes(chosenScope)) {
      throw new TrustedUiRejectionError(
        "INVALID_ARGUMENT",
        `Grant scope ${chosenScope} is not permitted for challenge ${challengeIdHex}`,
      );
    }

    const approvedSnap: Z4TrustedChallengeSnapshot = {
      ...snap,
      state: "APPROVED",
      irreversibleAckChecked: ackSatisfied,
      selectedGrantScope: chosenScope,
    };
    this.challenges.set(challengeIdHex, approvedSnap);
    if (this.activeChallenge?.challengeIdHex === challengeIdHex) {
      this.activeChallenge = null;
      this.stopSealLoop();
    }
    this.audit.z4ChallengesApproved += 1;

    const receipt = this.mintDecisionReceipt(
      approvedSnap,
      "APPROVED",
      chosenScope,
      input.nowMonoMs,
    );
    this.receipts.set(receipt.receiptIdHex, receipt);
    return receipt;
  }

  public requestReceiptUndo(receiptIdHex: string): string {
    const receipt = this.receipts.get(receiptIdHex);
    if (!receipt) {
      throw new TrustedUiRejectionError(
        "CHALLENGE_NOT_FOUND_OR_EXPIRED",
        `Receipt ${receiptIdHex} not found`,
      );
    }
    if (receipt.reversibilityClass === "IRREVERSIBLE") {
      this.audit.rejectedIrreversibleUndoAttempts += 1;
      throw new TrustedUiRejectionError(
        "IRREVERSIBLE_STATUTORY_LOCK_VIOLATION",
        "Statutory lock forbids Undo on irreversible receipts",
      );
    }
    return receipt.reversibilityClass === "LOSSLESS_UNDO"
      ? "LOSSLESS_UNDO_DISPATCHED"
      : "COMPENSATABLE_ROLLBACK_DISPATCHED";
  }

  public renderActiveHtml(nowMonoMs?: number): string {
    if (this.activeChallenge === null) {
      return "";
    }
    return renderZ4SecurityDialogHtml(this.activeChallenge, {
      locale: this.locale,
      disclosure: this.disclosure,
      nowMonoMs: nowMonoMs ?? this.activeChallenge.armedFromMonoMs,
    });
  }

  /** Builds a live DOM tree for the active Z4 challenge, embedding VerificationSeal and BlastRadiusDiff primitives. */
  public renderActiveDom(options?: Z4RenderOptions): HTMLElement | null {
    if (this.activeChallenge === null) {
      return null;
    }
    const doc = options?.doc ?? this.doc ?? document;
    const locale = options?.locale ?? this.locale;
    const disclosure = options?.disclosure ?? this.disclosure;
    const snap = this.activeChallenge;
    const nowMonoMs = options?.nowMonoMs ?? snap.armedFromMonoMs;
    const copy = Z4_COPY[locale];
    const isIrreversible =
      snap.reversibilityAndRisk.reversibilityClass === "IRREVERSIBLE";

    const scrim = doc.createElement("div");
    scrim.className = "nlos-scrim-z4";
    scrim.setAttribute("data-trusted-plane", "security-screen");
    scrim.setAttribute("data-z-index", String(Z4_SECURITY_Z_INDEX));
    scrim.setAttribute("data-nlos-disclosure", disclosure);

    const dialog = doc.createElement("div");
    dialog.className = "nlos-surface-modal-z4";
    dialog.setAttribute("role", "alertdialog");
    dialog.setAttribute("aria-modal", "true");
    dialog.setAttribute("data-challenge-id", snap.challengeIdHex);

    const header = doc.createElement("header");
    header.className = "nlos-hairline-bottom";
    const titleWrap = doc.createElement("div");
    titleWrap.appendChild(
      createIconElement("shield-seal", { size: 16, strokeWidth: 1.5, doc }),
    );
    const titleSpan = doc.createElement("span");
    titleSpan.textContent = copy.headerTitle;
    titleWrap.appendChild(titleSpan);
    header.appendChild(titleWrap);

    const sealCluster = doc.createElement("div");
    sealCluster.setAttribute("data-z4-seal-cluster", "true");
    const sealCtrl =
      this.activeSeal ??
      createVerificationSeal({
        ...this.sealOptions,
        doc,
        ariaLabel: copy.sealLabel,
        autoStart: true,
      });
    this.activeSeal = sealCtrl;
    sealCluster.appendChild(sealCtrl.element);

    const hintBox = doc.createElement("div");
    hintBox.className = "nlos-surface-recessed nlos-tabular";
    hintBox.setAttribute("data-z4-seal-hint", "true");
    hintBox.textContent = `${copy.sealLabel}: ${snap.securitySealHint}`;
    sealCluster.appendChild(hintBox);
    header.appendChild(sealCluster);
    dialog.appendChild(header);

    const table = doc.createElement("table");
    table.className = "nlos-table";
    table.setAttribute("data-z4-kv-spec-table", "true");
    const tbody = doc.createElement("tbody");

    const appendKvRow = (
      rowKey: string,
      label: string,
      primaryText: string,
      l3Text: string | null,
    ): HTMLTableCellElement => {
      const tr = doc.createElement("tr");
      tr.className = "nlos-table-row";
      tr.setAttribute("data-z4-kv-row", rowKey);
      const th = doc.createElement("th");
      th.setAttribute("scope", "row");
      th.textContent = label;
      const td = doc.createElement("td");
      const valDiv = doc.createElement("div");
      valDiv.textContent = primaryText;
      td.appendChild(valDiv);
      if (disclosure === "L3" && l3Text !== null) {
        const l3Div = doc.createElement("div");
        l3Div.className = "nlos-mono nlos-l3-only";
        l3Div.textContent = l3Text;
        td.appendChild(l3Div);
      }
      tr.appendChild(th);
      tr.appendChild(td);
      tbody.appendChild(tr);
      return td;
    };

    const pubStatus = snap.requestingApp.signatureVerified
      ? copy.verifiedPublisher
      : copy.unverifiedPublisher;
    appendKvRow(
      "requesting-app",
      copy.rowAppLabel,
      `${snap.requestingApp.displayName} (${snap.requestingApp.publisherName} · ${pubStatus})`,
      `ApplicationId=${snap.requestingApp.applicationIdHex} InstallationId=${snap.requestingApp.installationIdHex} PrincipalId=${snap.requestingApp.principalIdHex} gen=${snap.requestingApp.installationGeneration}`,
    );
    appendKvRow(
      "action-scope",
      copy.rowScopeLabel,
      snap.actionScope.plainSummary[locale],
      `CapabilityRights=[${snap.actionScope.capabilityRights.join(", ")}] ControlScope=${snap.actionScope.controlScope} plan_digest=${snap.actionScope.planDigestHex}`,
    );
    appendKvRow(
      "resource-ceiling",
      copy.rowBudgetLabel,
      snap.resourceCeiling.plainCeiling[locale],
      `memory_mib=${snap.resourceCeiling.memoryCeilingMib} tokens=${snap.resourceCeiling.tokenBudgetCeiling} egress=${String(snap.resourceCeiling.networkEgressAllowed)} max_duration_ms=${snap.resourceCeiling.maxDurationMs}`,
    );

    const riskTd = appendKvRow(
      "reversibility-risk",
      copy.rowRiskLabel,
      `${copy.reversibilityLabels[snap.reversibilityAndRisk.reversibilityClass]} — ${snap.reversibilityAndRisk.plainConsequence[locale]}`,
      `reversibility=${snap.reversibilityAndRisk.reversibilityClass} multi_target_count=${snap.reversibilityAndRisk.multiTargetCount}`,
    );

    if (isIrreversible) {
      riskTd.appendChild(
        createStatutoryLockBadgeElement({
          kind: "IRREVERSIBLE_NO_UNDO",
          locale,
          disclosure,
          doc,
        }),
      );
    }

    if (snap.reversibilityAndRisk.partialFailureDisclosure !== null) {
      const partialDiv = doc.createElement("div");
      partialDiv.className = "nlos-surface-recessed";
      partialDiv.setAttribute("data-z4-partial-failure", "true");
      partialDiv.textContent =
        snap.reversibilityAndRisk.partialFailureDisclosure[locale];
      riskTd.appendChild(partialDiv);
    }

    table.appendChild(tbody);
    dialog.appendChild(table);

    if (snap.reversibilityAndRisk.blastRadiusRows.length > 0) {
      dialog.appendChild(
        createBlastRadiusDiffElement({
          rows: snap.reversibilityAndRisk.blastRadiusRows,
          operationKind: snap.reversibilityAndRisk.blastOperationKind,
          requiresDataLossReceipt:
            snap.reversibilityAndRisk.requiresDataLossReceipt,
          locale,
          disclosure,
          doc,
        }),
      );
    }

    const footer = doc.createElement("footer");
    footer.className = "nlos-hairline-top";

    const cancelBtn = doc.createElement("button");
    cancelBtn.setAttribute("type", "button");
    cancelBtn.className = "nlos-btn";
    cancelBtn.setAttribute("data-z4-action", "cancel");
    cancelBtn.textContent = copy.cancelEscapeLabel;
    footer.appendChild(cancelBtn);

    const elapsedMs = Math.max(0, nowMonoMs - snap.armedFromMonoMs);
    const armingRemainingMs = Math.max(0, snap.armingWindowMs - elapsedMs);
    const confirmDisabled =
      armingRemainingMs > 0 || (isIrreversible && !snap.irreversibleAckChecked);

    const approveBtn = doc.createElement("button");
    approveBtn.setAttribute("type", "button");
    approveBtn.className = isIrreversible
      ? "nlos-btn nlos-btn--danger"
      : "nlos-btn nlos-btn--primary";
    approveBtn.setAttribute("data-z4-action", "approve");
    if (confirmDisabled) {
      approveBtn.setAttribute("disabled", "disabled");
    }
    approveBtn.textContent =
      armingRemainingMs > 0
        ? `${copy.armingWaitPrefix} (${armingRemainingMs}ms)`
        : isIrreversible
          ? copy.approveIrreversibleLabel
          : copy.approveLabel;
    footer.appendChild(approveBtn);

    dialog.appendChild(footer);
    scrim.appendChild(dialog);
    return scrim;
  }

  private stopSealLoop(): void {
    if (this.activeSeal !== null) {
      this.activeSeal.unmount();
      this.activeSeal = null;
    }
  }

  private mintDecisionReceipt(
    snap: Z4TrustedChallengeSnapshot,
    state: Z4ChallengeState,
    chosenGrantScope: ConsentGrantScope | null,
    decidedAtMonoMs: number,
  ): Z4ConsentDecisionReceipt {
    const revClass = snap.reversibilityAndRisk.reversibilityClass;
    const digest = deterministicDomainDigestHex(
      "llmos/stage-d/ts-z4-receipt/v1",
      [
        snap.challengeIdHex,
        state,
        chosenGrantScope ?? "NONE",
        revClass,
        String(decidedAtMonoMs),
        String(snap.sessionGeneration),
      ],
    );
    return {
      receiptIdHex: digest.slice(0, 32),
      challengeIdHex: snap.challengeIdHex,
      state,
      chosenGrantScope,
      reversibilityClass: revClass,
      undoAffordanceAllowed: revClass !== "IRREVERSIBLE",
      requiresDataLossReceipt:
        snap.reversibilityAndRisk.requiresDataLossReceipt,
      decidedAtMonoMs,
      sessionGeneration: snap.sessionGeneration,
      decisionDigestHex: digest,
      allowedFollowUpVerbs: snap.candidateReceiptVerbs,
    };
  }
}

export function createZ4SecurityDialog(
  options?: Z4SecurityDialogOptions,
): Z4SecurityDialogController {
  return new Z4SecurityDialogController(options);
}
