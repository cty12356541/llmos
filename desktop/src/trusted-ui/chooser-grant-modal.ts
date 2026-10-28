// System ChooserGrant Scoped Capability Broker & Modal (Open / Save / Share).
// Authority: 06-Architecture Spec v0.5 §24.3, §25.4; ADR-0025 Decision 1;
// Stage D Visual & UX Spec §3.5 ([TUI-CHOOSER-001], [DUI-CHOOSER-001]).

import { renderIconSvg } from "../ui/icons";
import type { DisclosureLevel, WorkstationLocale } from "../ui/status-indicator";
import { renderVerificationSealFallbackHtml } from "../ui/verification-seal";
import {
  TrustedUiRejectionError,
  Z4_ARMING_WINDOW_MS,
  Z4_SCRIM_RGBA,
  Z4_SECURITY_Z_INDEX,
  Z4_TOP_SECURITY_BAR_PX,
  deterministicDomainDigestHex,
  escapeHtml,
  type TrustedSessionLifecycle,
  type TrustedUiCallerOrigin,
  type TrustedUiInputOrigin,
  type TrustedUiInputSource,
} from "./z4-security-dialog";

export type ChooserMode = "OPEN" | "SAVE" | "SHARE";

export type ChooserAttenuatedRights =
  | "READ_ONLY"
  | "READ_WRITE_SINGLE"
  | "EXPORT_SAVE";

export interface TcbVaultEntryInput {
  readonly vaultItemIdHex: string;
  readonly artifactIdHex: string;
  readonly hostBackingPath: string;
  readonly logicalCollectionLabel: Readonly<Record<WorkstationLocale, string>>;
  readonly leafDisplayName: string;
  readonly contentType: string;
  readonly sizeBytes: number;
}

export interface TcbChooserCatalogRow {
  readonly vaultItemIdHex: string;
  readonly logicalCollectionLabel: Readonly<Record<WorkstationLocale, string>>;
  readonly leafDisplayName: string;
  readonly contentType: string;
  readonly sizeBytes: number;
}

export interface ScopedChooserHandleForApp {
  readonly scopedHandleIdHex: string;
  readonly artifactIdHex: string;
  readonly displayName: string;
  readonly contentType: string;
  readonly sizeBytes: number;
}

/** Strict capability token returned to the requesting Application — zero host bare paths or sibling items. */
export interface ChooserGrantForApp {
  readonly grantIdHex: string;
  readonly requestingApplicationIdHex: string;
  readonly mode: ChooserMode;
  readonly attenuatedRights: ChooserAttenuatedRights;
  readonly scopedHandles: readonly ScopedChooserHandleForApp[];
  readonly sessionGeneration: number;
  readonly issuedAtMonoMs: number;
  readonly expiresAtMonoMs: number;
  readonly grantDigestHex: string;
}

export interface ExercisedChooserHandle {
  readonly grantIdHex: string;
  readonly scopedHandleIdHex: string;
  readonly artifactIdHex: string;
  readonly grantedRight: ChooserAttenuatedRights;
  readonly displayName: string;
  readonly contentType: string;
  readonly sizeBytes: number;
}

export interface TcbChooserSessionSnapshot {
  readonly chooserSessionIdHex: string;
  readonly requestingApplicationIdHex: string;
  readonly requestingAppDisplayName: string;
  readonly mode: ChooserMode;
  readonly requestedRights: ChooserAttenuatedRights;
  readonly zIndex: number;
  readonly securitySealHint: string;
  readonly sessionGeneration: number;
  readonly presentedAtMonoMs: number;
  readonly armedFromMonoMs: number;
  readonly armingWindowMs: number;
  readonly catalogRows: readonly TcbChooserCatalogRow[];
  readonly selectedVaultItemIds: readonly string[];
  readonly exportSaveLeafName: string;
}

export interface ChooserBrokerAuditCounters {
  chooserGrantsIssued: number;
  chooserGrantsRevoked: number;
  rejectedBoundaryViolations: number;
  rejectedClickjackingAttempts: number;
  rejectedUntrustedConfirmSpoofs: number;
}

interface InternalVaultRecord {
  readonly vaultItemIdHex: string;
  readonly artifactIdHex: string;
  readonly privateHostBackingPath: string;
  readonly logicalCollectionLabel: Readonly<Record<WorkstationLocale, string>>;
  readonly leafDisplayName: string;
  readonly contentType: string;
  readonly sizeBytes: number;
}

interface InternalIssuedGrant {
  readonly dto: ChooserGrantForApp;
  revoked: boolean;
  readonly handleToVaultItem: ReadonlyMap<string, string | null>;
}

export function sanitizeLeafDisplayName(raw: string): string {
  const trimmed = raw.trim();
  if (
    trimmed.length === 0 ||
    trimmed === "." ||
    trimmed === ".." ||
    trimmed.includes("..") ||
    trimmed.includes("/") ||
    trimmed.includes("\\") ||
    trimmed.includes(":") ||
    trimmed.includes("\0")
  ) {
    throw new TrustedUiRejectionError(
      "CHOOSER_INVALID_EXPORT_NAME",
      `Chooser leaf filename must not contain host path separators or traversal: ${trimmed}`,
    );
  }
  return trimmed;
}

export function modeAllowsAttenuatedRights(
  mode: ChooserMode,
  rights: ChooserAttenuatedRights,
): boolean {
  switch (mode) {
    case "OPEN":
      return rights === "READ_ONLY" || rights === "READ_WRITE_SINGLE";
    case "SAVE":
      return rights === "EXPORT_SAVE" || rights === "READ_WRITE_SINGLE";
    case "SHARE":
      return rights === "READ_ONLY";
  }
}

export function attenuatedRightsSatisfy(
  held: ChooserAttenuatedRights,
  required: ChooserAttenuatedRights,
): boolean {
  if (held === "READ_ONLY") {
    return required === "READ_ONLY";
  }
  if (held === "READ_WRITE_SINGLE") {
    return required === "READ_ONLY" || required === "READ_WRITE_SINGLE";
  }
  return required === "EXPORT_SAVE";
}

interface ChooserCopy {
  readonly titles: Readonly<Record<ChooserMode, string>>;
  readonly rightsDescriptions: Readonly<Record<ChooserAttenuatedRights, string>>;
  readonly sealLabel: string;
  readonly requestingAppPrefix: string;
  readonly colCollection: string;
  readonly colName: string;
  readonly colType: string;
  readonly colSize: string;
  readonly exportNameLabel: string;
  readonly cancelLabel: string;
  readonly confirmLabels: Readonly<Record<ChooserMode, string>>;
  readonly armingPrefix: string;
  readonly zeroPathNotice: string;
}

const CHOOSER_COPY: Readonly<Record<WorkstationLocale, ChooserCopy>> = {
  "zh-CN": {
    titles: {
      OPEN: "系统安全文件选择 · 打开受控对象",
      SAVE: "系统安全文件选择 · 导出保存目标",
      SHARE: "系统安全文件选择 · 单次只读共享",
    },
    rightsDescriptions: {
      READ_ONLY: "仅授予所选条目的单次只读访问句柄",
      READ_WRITE_SINGLE: "仅授予所选单个条目的读写访问句柄",
      EXPORT_SAVE: "仅授予新建导出目标的写入句柄",
    },
    sealLabel: "个人防伪核验短语",
    requestingAppPrefix: "申请应用",
    colCollection: "所属分类",
    colName: "对象名称",
    colType: "媒体类型",
    colSize: "字节大小",
    exportNameLabel: "导出文件名",
    cancelLabel: "取消 (Esc)",
    confirmLabels: {
      OPEN: "授权打开所选项",
      SAVE: "授权保存至此名称",
      SHARE: "授权只读共享",
    },
    armingPrefix: "防误触保护中",
    zeroPathNotice: "应用仅接收不透明受限句柄，不会获知您的计算机真实文件夹路径或未选中的其他文件。",
  },
  "en-US": {
    titles: {
      OPEN: "System Safe Chooser · Open Controlled Item",
      SAVE: "System Safe Chooser · Save Export Target",
      SHARE: "System Safe Chooser · Share Read-Only",
    },
    rightsDescriptions: {
      READ_ONLY: "Grants read-only handle to selected item(s) only",
      READ_WRITE_SINGLE: "Grants read-write handle to the selected item only",
      EXPORT_SAVE: "Grants write handle to the single export target only",
    },
    sealLabel: "Personal Verification Phrase",
    requestingAppPrefix: "Requesting App",
    colCollection: "Collection",
    colName: "Item Name",
    colType: "Media Type",
    colSize: "Size (Bytes)",
    exportNameLabel: "Export Filename",
    cancelLabel: "Cancel (Esc)",
    confirmLabels: {
      OPEN: "Grant Access to Selected",
      SAVE: "Grant Export Save",
      SHARE: "Grant Read-Only Share",
    },
    armingPrefix: "Clickguard active",
    zeroPathNotice: "The app receives only an opaque scoped handle and never sees raw folder paths or unselected files.",
  },
};

export function renderChooserGrantModalHtml(
  session: TcbChooserSessionSnapshot,
  options?: {
    readonly locale?: WorkstationLocale;
    readonly disclosure?: DisclosureLevel;
    readonly nowMonoMs?: number;
  },
): string {
  const locale: WorkstationLocale = options?.locale ?? "zh-CN";
  const disclosure: DisclosureLevel = options?.disclosure ?? "L2";
  const nowMonoMs = options?.nowMonoMs ?? session.armedFromMonoMs;
  const copy = CHOOSER_COPY[locale];

  const elapsedMs = Math.max(0, nowMonoMs - session.armedFromMonoMs);
  const armingRemainingMs = Math.max(0, session.armingWindowMs - elapsedMs);
  const hasValidSelection =
    session.mode === "SAVE"
      ? session.exportSaveLeafName.trim().length > 0
      : session.selectedVaultItemIds.length > 0;
  const confirmDisabled = armingRemainingMs > 0 || !hasValidSelection;

  const l3Meta =
    disclosure === "L3"
      ? `<div class="nlos-mono nlos-l3-only" data-chooser-l3="contract" style="margin-top: 6px;">` +
        `ApplicationId=${escapeHtml(session.requestingApplicationIdHex)} ` +
        `mode=${escapeHtml(session.mode)} ` +
        `rights=${escapeHtml(session.requestedRights)} ` +
        `session_gen=${session.sessionGeneration}` +
        `</div>`
      : "";

  const bodyHtml =
    session.mode === "SAVE"
      ? `<div style="margin-top: 8px;">` +
        `<label style="display: block; margin-bottom: 4px; color: var(--nlos-text-secondary);">${escapeHtml(copy.exportNameLabel)}</label>` +
        `<input class="nlos-input" type="text" data-chooser-export-input="true" value="${escapeHtml(session.exportSaveLeafName)}" style="width: 100%;" />` +
        `</div>`
      : `<table class="nlos-table" data-chooser-catalog-table="true" style="margin-top: 8px;">` +
        `<thead><tr>` +
        `<th>${escapeHtml(copy.colCollection)}</th>` +
        `<th>${escapeHtml(copy.colName)}</th>` +
        `<th>${escapeHtml(copy.colType)}</th>` +
        `<th class="nlos-tabular">${escapeHtml(copy.colSize)}</th>` +
        `</tr></thead><tbody>` +
        session.catalogRows
          .map((row) => {
            const selected = session.selectedVaultItemIds.includes(
              row.vaultItemIdHex,
            );
            return (
              `<tr class="nlos-table-row" aria-selected="${selected ? "true" : "false"}" data-vault-item-id="${escapeHtml(row.vaultItemIdHex)}">` +
              `<td>${escapeHtml(row.logicalCollectionLabel[locale])}</td>` +
              `<td><strong>${escapeHtml(row.leafDisplayName)}</strong></td>` +
              `<td class="nlos-mono">${escapeHtml(row.contentType)}</td>` +
              `<td class="nlos-tabular">${row.sizeBytes}</td>` +
              `</tr>`
            );
          })
          .join("") +
        `</tbody></table>`;

  const confirmText =
    armingRemainingMs > 0
      ? `${copy.armingPrefix} (${armingRemainingMs}ms)`
      : copy.confirmLabels[session.mode];

  return (
    `<div class="nlos-scrim-z4" data-trusted-plane="chooser-modal" ` +
    `data-z-index="${Z4_SECURITY_Z_INDEX}" data-nlos-disclosure="${escapeHtml(disclosure)}" ` +
    `style="position: fixed; inset: 0; background-color: ${Z4_SCRIM_RGBA}; z-index: ${Z4_SECURITY_Z_INDEX}; display: flex; align-items: center; justify-content: center;">` +
    `<div class="nlos-surface-modal-z4" role="dialog" aria-modal="true" ` +
    `data-chooser-session-id="${escapeHtml(session.chooserSessionIdHex)}" ` +
    `style="border-radius: 6px; border-top-width: ${Z4_TOP_SECURITY_BAR_PX}px; padding: 14px 16px;">` +
    `<header class="nlos-hairline-bottom" style="display: flex; align-items: center; justify-content: space-between; padding-bottom: 10px; margin-bottom: 8px; gap: 12px;">` +
    `<div style="display: flex; align-items: center; gap: 8px; font-weight: 600;">` +
    `${renderIconSvg("shield-seal", { size: 16, strokeWidth: 1.5 })}` +
    `<span>${escapeHtml(copy.titles[session.mode])}</span>` +
    `</div>` +
    `<div style="display: inline-flex; align-items: center; gap: 8px;">` +
    `${renderVerificationSealFallbackHtml({ ariaLabel: copy.sealLabel })}` +
    `<div class="nlos-surface-recessed nlos-tabular" data-chooser-seal-hint="true" style="padding: 2px 8px; border-radius: 2px; font-size: 12px;">` +
    `<span style="color: var(--nlos-text-secondary); margin-right: 6px;">${escapeHtml(copy.sealLabel)}:</span>` +
    `<strong>${escapeHtml(session.securitySealHint)}</strong>` +
    `</div>` +
    `</div>` +
    `</header>` +
    `<div style="font-size: 12px; color: var(--nlos-text-secondary);">` +
    `${escapeHtml(copy.requestingAppPrefix)}: <strong style="color: var(--nlos-text-primary);">${escapeHtml(session.requestingAppDisplayName)}</strong> · ` +
    `${escapeHtml(copy.rightsDescriptions[session.requestedRights])}` +
    `</div>` +
    `<div class="nlos-surface-recessed" style="padding: 6px 8px; margin-top: 6px; border-radius: 4px; font-size: 12px;">` +
    `${escapeHtml(copy.zeroPathNotice)}` +
    `</div>` +
    `${l3Meta}` +
    `${bodyHtml}` +
    `<footer class="nlos-hairline-top" style="display: flex; align-items: center; justify-content: flex-end; gap: 8px; padding-top: 10px; margin-top: 12px;">` +
    `<button type="button" class="nlos-btn" data-chooser-action="cancel">${escapeHtml(copy.cancelLabel)}</button>` +
    `<button type="button" class="nlos-btn nlos-btn--primary" data-chooser-action="confirm"${confirmDisabled ? ' disabled="disabled"' : ""}>${escapeHtml(confirmText)}</button>` +
    `</footer>` +
    `</div>` +
    `</div>`
  );
}

/** Stateful TCB ChooserGrantBroker enforcing zero host bare path leakage to Applications. */
export class ChooserGrantModalBroker {
  private sessionState: TrustedSessionLifecycle;
  private sessionGeneration: number;
  private securitySealHint: string;
  private readonly vault = new Map<string, InternalVaultRecord>();
  private activeSession: TcbChooserSessionSnapshot | null = null;
  private readonly grants = new Map<string, InternalIssuedGrant>();
  private nextNonce = 0;

  public readonly audit: ChooserBrokerAuditCounters = {
    chooserGrantsIssued: 0,
    chooserGrantsRevoked: 0,
    rejectedBoundaryViolations: 0,
    rejectedClickjackingAttempts: 0,
    rejectedUntrustedConfirmSpoofs: 0,
  };

  public constructor(options?: {
    readonly initialSessionState?: TrustedSessionLifecycle;
    readonly initialSessionGeneration?: number;
    readonly initialSecuritySealHint?: string;
  }) {
    this.sessionState = options?.initialSessionState ?? "ACTIVE";
    this.sessionGeneration = options?.initialSessionGeneration ?? 1;
    this.securitySealHint = options?.initialSecuritySealHint ?? "远山青松-4821";
    this.seedDefaultWorkstationVault();
  }

  public registerVaultEntry(entry: TcbVaultEntryInput): void {
    const safeLeaf = sanitizeLeafDisplayName(entry.leafDisplayName);
    this.vault.set(entry.vaultItemIdHex, {
      vaultItemIdHex: entry.vaultItemIdHex,
      artifactIdHex: entry.artifactIdHex,
      privateHostBackingPath: entry.hostBackingPath,
      logicalCollectionLabel: entry.logicalCollectionLabel,
      leafDisplayName: safeLeaf,
      contentType: entry.contentType,
      sizeBytes: entry.sizeBytes,
    });
  }

  public tcbOnlyResolveHostPath(
    grantIdHex: string,
    scopedHandleIdHex: string,
  ): string | null {
    const grant = this.grants.get(grantIdHex);
    if (!grant) {
      return null;
    }
    const vaultId = grant.handleToVaultItem.get(scopedHandleIdHex) ?? null;
    if (vaultId === null) {
      return null;
    }
    return this.vault.get(vaultId)?.privateHostBackingPath ?? null;
  }

  public syncSession(
    nextState: TrustedSessionLifecycle,
    nextSessionGeneration: number,
    nextSealHint?: string,
  ): void {
    const invalidate =
      nextState !== "ACTIVE" || nextSessionGeneration !== this.sessionGeneration;
    this.sessionState = nextState;
    this.sessionGeneration = nextSessionGeneration;
    if (nextSealHint !== undefined && nextSealHint.trim().length > 0) {
      this.securitySealHint = nextSealHint.trim();
    }
    if (invalidate) {
      this.activeSession = null;
      for (const grant of this.grants.values()) {
        if (!grant.revoked) {
          grant.revoked = true;
          this.audit.chooserGrantsRevoked += 1;
        }
      }
    }
  }

  public getActiveSession(caller: TrustedUiCallerOrigin): TcbChooserSessionSnapshot | null {
    if (caller.kind !== "tcb_trusted_broker") {
      this.audit.rejectedBoundaryViolations += 1;
      throw new TrustedUiRejectionError(
        "UNTRUSTED_Z4_FORGE_ATTEMPT",
        "Untrusted surface cannot inspect TCB chooser catalog or seal hint",
      );
    }
    return this.activeSession;
  }

  public openChooser(options: {
    readonly caller: TrustedUiCallerOrigin;
    readonly requestingApplicationIdHex: string;
    readonly requestingAppDisplayName: string;
    readonly mode: ChooserMode;
    readonly requestedRights: ChooserAttenuatedRights;
    readonly nowMonoMs: number;
  }): TcbChooserSessionSnapshot {
    if (options.caller.kind !== "tcb_trusted_broker") {
      this.audit.rejectedBoundaryViolations += 1;
      throw new TrustedUiRejectionError(
        "UNTRUSTED_Z4_FORGE_ATTEMPT",
        "Untrusted surface cannot directly open or enumerate TCB chooser catalog",
      );
    }
    if (this.sessionState !== "ACTIVE") {
      throw new TrustedUiRejectionError(
        "SESSION_NOT_ACTIVE_OR_STALE_GENERATION",
        `Cannot open chooser while session is ${this.sessionState}`,
      );
    }
    if (!modeAllowsAttenuatedRights(options.mode, options.requestedRights)) {
      this.audit.rejectedBoundaryViolations += 1;
      throw new TrustedUiRejectionError(
        "CHOOSER_RIGHTS_EXCEED_MODE_CEILING",
        `Chooser mode ${options.mode} forbids rights ${options.requestedRights}`,
      );
    }

    this.nextNonce += 1;
    const sidDigest = deterministicDomainDigestHex(
      "llmos/stage-d/ts-chooser-session/v1",
      [
        String(this.nextNonce),
        options.requestingApplicationIdHex,
        options.mode,
        String(options.nowMonoMs),
      ],
    );
    const catalogRows: TcbChooserCatalogRow[] = [...this.vault.values()].map(
      (entry) => ({
        vaultItemIdHex: entry.vaultItemIdHex,
        logicalCollectionLabel: entry.logicalCollectionLabel,
        leafDisplayName: entry.leafDisplayName,
        contentType: entry.contentType,
        sizeBytes: entry.sizeBytes,
      }),
    );

    const snapshot: TcbChooserSessionSnapshot = {
      chooserSessionIdHex: sidDigest.slice(0, 32),
      requestingApplicationIdHex: options.requestingApplicationIdHex,
      requestingAppDisplayName: options.requestingAppDisplayName,
      mode: options.mode,
      requestedRights: options.requestedRights,
      zIndex: Z4_SECURITY_Z_INDEX,
      securitySealHint: this.securitySealHint,
      sessionGeneration: this.sessionGeneration,
      presentedAtMonoMs: options.nowMonoMs,
      armedFromMonoMs: options.nowMonoMs,
      armingWindowMs: Z4_ARMING_WINDOW_MS,
      catalogRows,
      selectedVaultItemIds: [],
      exportSaveLeafName: "",
    };
    this.activeSession = snapshot;
    return snapshot;
  }

  public selectItems(
    caller: TrustedUiCallerOrigin,
    vaultItemIds: readonly string[],
    exportSaveLeafName?: string,
  ): TcbChooserSessionSnapshot {
    if (caller.kind !== "tcb_trusted_broker" || this.activeSession === null) {
      this.audit.rejectedBoundaryViolations += 1;
      throw new TrustedUiRejectionError(
        "UNTRUSTED_Z4_FORGE_ATTEMPT",
        "Only TCB chooser UI may mutate item selection",
      );
    }
    const updated: TcbChooserSessionSnapshot = {
      ...this.activeSession,
      selectedVaultItemIds: [...vaultItemIds],
      exportSaveLeafName:
        exportSaveLeafName ?? this.activeSession.exportSaveLeafName,
    };
    this.activeSession = updated;
    return updated;
  }

  public cancelChooser(origin: TrustedUiInputOrigin): void {
    if (origin !== "trusted_shell") {
      this.audit.rejectedUntrustedConfirmSpoofs += 1;
      throw new TrustedUiRejectionError(
        "UNTRUSTED_INPUT_ORIGIN",
        "Untrusted surface cannot dismiss system chooser",
      );
    }
    this.activeSession = null;
  }

  public confirmChooserGrant(options: {
    readonly origin: TrustedUiInputOrigin;
    readonly source: TrustedUiInputSource;
    readonly chooserSessionIdHex: string;
    readonly selectedVaultItemIds?: readonly string[];
    readonly exportSaveLeafName?: string;
    readonly exportContentType?: string;
    readonly ttlMs: number;
    readonly nowMonoMs: number;
  }): ChooserGrantForApp {
    if (
      options.origin !== "trusted_shell" ||
      options.source === "AUTOMATION"
    ) {
      this.audit.rejectedUntrustedConfirmSpoofs += 1;
      throw new TrustedUiRejectionError(
        "UNTRUSTED_INPUT_ORIGIN",
        "Untrusted origin or automation forbidden from confirming ChooserGrant",
      );
    }
    if (
      this.activeSession === null ||
      this.activeSession.chooserSessionIdHex !== options.chooserSessionIdHex
    ) {
      throw new TrustedUiRejectionError(
        "CHALLENGE_NOT_FOUND_OR_EXPIRED",
        `Chooser session ${options.chooserSessionIdHex} is not active`,
      );
    }
    if (
      this.sessionState !== "ACTIVE" ||
      this.sessionGeneration !== this.activeSession.sessionGeneration
    ) {
      this.activeSession = null;
      throw new TrustedUiRejectionError(
        "SESSION_NOT_ACTIVE_OR_STALE_GENERATION",
        "Session changed before chooser grant confirmation",
      );
    }

    const elapsed = options.nowMonoMs - this.activeSession.armedFromMonoMs;
    if (elapsed < Z4_ARMING_WINDOW_MS) {
      this.audit.rejectedClickjackingAttempts += 1;
      throw new TrustedUiRejectionError(
        "CLICKJACKING_ARMING_ACTIVE",
        `Chooser confirm rejected within ${Z4_ARMING_WINDOW_MS}ms arming window (elapsed ${elapsed}ms)`,
      );
    }

    this.nextNonce += 1;
    const grantIdHex = deterministicDomainDigestHex(
      "llmos/stage-d/ts-chooser-grant-id/v1",
      [
        String(this.nextNonce),
        this.activeSession.chooserSessionIdHex,
        this.activeSession.requestingApplicationIdHex,
        String(options.nowMonoMs),
      ],
    ).slice(0, 32);

    const handles: ScopedChooserHandleForApp[] = [];
    const handleMap = new Map<string, string | null>();
    const selectedIds =
      options.selectedVaultItemIds ?? this.activeSession.selectedVaultItemIds;

    if (this.activeSession.mode === "SAVE") {
      const rawExport =
        options.exportSaveLeafName ?? this.activeSession.exportSaveLeafName;
      const safeLeaf = sanitizeLeafDisplayName(rawExport);
      const artifactIdHex = deterministicDomainDigestHex(
        "llmos/stage-d/ts-chooser-save-artifact/v1",
        [grantIdHex, safeLeaf],
      );
      const scopedHandleIdHex = deterministicDomainDigestHex(
        "llmos/stage-d/ts-chooser-handle/v1",
        [grantIdHex, artifactIdHex, "0"],
      ).slice(0, 32);
      handleMap.set(scopedHandleIdHex, null);
      handles.push({
        scopedHandleIdHex,
        artifactIdHex,
        displayName: safeLeaf,
        contentType: options.exportContentType ?? "application/octet-stream",
        sizeBytes: 0,
      });
    } else {
      if (selectedIds.length === 0) {
        throw new TrustedUiRejectionError(
          "INVALID_ARGUMENT",
          "Chooser confirmation requires at least one selected item",
        );
      }
      selectedIds.forEach((vaultId, index) => {
        const entry = this.vault.get(vaultId);
        if (!entry) {
          this.audit.rejectedBoundaryViolations += 1;
          throw new TrustedUiRejectionError(
            "CHOOSER_ITEM_NOT_IN_VAULT",
            `Selected vault item ${vaultId} does not exist`,
          );
        }
        const scopedHandleIdHex = deterministicDomainDigestHex(
          "llmos/stage-d/ts-chooser-handle/v1",
          [grantIdHex, entry.artifactIdHex, String(index)],
        ).slice(0, 32);
        handleMap.set(scopedHandleIdHex, vaultId);
        handles.push({
          scopedHandleIdHex,
          artifactIdHex: entry.artifactIdHex,
          displayName: entry.leafDisplayName,
          contentType: entry.contentType,
          sizeBytes: entry.sizeBytes,
        });
      });
    }

    const boundedTtl = Math.max(1_000, Math.min(options.ttlMs, 86_400_000));
    const expiresAtMonoMs = options.nowMonoMs + boundedTtl;
    const grantDigestHex = deterministicDomainDigestHex(
      "llmos/stage-d/ts-chooser-grant/v1",
      [
        grantIdHex,
        this.activeSession.requestingApplicationIdHex,
        this.activeSession.mode,
        this.activeSession.requestedRights,
        handles.map((h) => `${h.scopedHandleIdHex}:${h.artifactIdHex}`).join(";"),
        String(this.activeSession.sessionGeneration),
        String(options.nowMonoMs),
        String(expiresAtMonoMs),
      ],
    );

    const dto: ChooserGrantForApp = {
      grantIdHex,
      requestingApplicationIdHex:
        this.activeSession.requestingApplicationIdHex,
      mode: this.activeSession.mode,
      attenuatedRights: this.activeSession.requestedRights,
      scopedHandles: handles,
      sessionGeneration: this.activeSession.sessionGeneration,
      issuedAtMonoMs: options.nowMonoMs,
      expiresAtMonoMs,
      grantDigestHex,
    };

    this.grants.set(grantIdHex, {
      dto,
      revoked: false,
      handleToVaultItem: handleMap,
    });
    this.activeSession = null;
    this.audit.chooserGrantsIssued += 1;
    return dto;
  }

  public exerciseGrant(options: {
    readonly grantIdHex: string;
    readonly callerApplicationIdHex: string;
    readonly scopedHandleIdHex: string;
    readonly requiredRight: ChooserAttenuatedRights;
    readonly nowMonoMs: number;
  }): ExercisedChooserHandle {
    const rec = this.grants.get(options.grantIdHex);
    if (!rec) {
      throw new TrustedUiRejectionError(
        "CHOOSER_GRANT_NOT_FOUND",
        `ChooserGrant ${options.grantIdHex} not found`,
      );
    }
    if (
      rec.revoked ||
      this.sessionState !== "ACTIVE" ||
      rec.dto.sessionGeneration !== this.sessionGeneration
    ) {
      this.audit.rejectedBoundaryViolations += 1;
      throw new TrustedUiRejectionError(
        "CHOOSER_GRANT_REVOKED",
        `ChooserGrant ${options.grantIdHex} is revoked or belongs to stale session`,
      );
    }
    if (options.nowMonoMs >= rec.dto.expiresAtMonoMs) {
      this.audit.rejectedBoundaryViolations += 1;
      throw new TrustedUiRejectionError(
        "CHOOSER_GRANT_EXPIRED",
        `ChooserGrant ${options.grantIdHex} expired at ${rec.dto.expiresAtMonoMs}ms`,
      );
    }
    if (rec.dto.requestingApplicationIdHex !== options.callerApplicationIdHex) {
      this.audit.rejectedBoundaryViolations += 1;
      throw new TrustedUiRejectionError(
        "CHOOSER_GRANT_APPLICATION_MISMATCH",
        `ChooserGrant ${options.grantIdHex} rejected foreign caller ${options.callerApplicationIdHex}`,
      );
    }
    if (!attenuatedRightsSatisfy(rec.dto.attenuatedRights, options.requiredRight)) {
      this.audit.rejectedBoundaryViolations += 1;
      throw new TrustedUiRejectionError(
        "CHOOSER_GRANT_INSUFFICIENT_RIGHTS",
        `ChooserGrant ${options.grantIdHex} holds ${rec.dto.attenuatedRights}, insufficient for ${options.requiredRight}`,
      );
    }

    const handle = rec.dto.scopedHandles.find(
      (item) => item.scopedHandleIdHex === options.scopedHandleIdHex,
    );
    if (!handle) {
      this.audit.rejectedBoundaryViolations += 1;
      throw new TrustedUiRejectionError(
        "CHOOSER_GRANT_HANDLE_OUT_OF_SCOPE",
        `Handle ${options.scopedHandleIdHex} is outside ChooserGrant ${options.grantIdHex}`,
      );
    }

    return {
      grantIdHex: rec.dto.grantIdHex,
      scopedHandleIdHex: handle.scopedHandleIdHex,
      artifactIdHex: handle.artifactIdHex,
      grantedRight: options.requiredRight,
      displayName: handle.displayName,
      contentType: handle.contentType,
      sizeBytes: handle.sizeBytes,
    };
  }

  public revokeGrant(grantIdHex: string): void {
    const rec = this.grants.get(grantIdHex);
    if (!rec) {
      throw new TrustedUiRejectionError(
        "CHOOSER_GRANT_NOT_FOUND",
        `ChooserGrant ${grantIdHex} not found`,
      );
    }
    if (!rec.revoked) {
      rec.revoked = true;
      this.audit.chooserGrantsRevoked += 1;
    }
  }

  private seedDefaultWorkstationVault(): void {
    const defaults: readonly TcbVaultEntryInput[] = [
      {
        vaultItemIdHex: "11".repeat(16),
        artifactIdHex: deterministicDomainDigestHex("artifact", ["q3-financial-ledger"]),
        hostBackingPath: "C:\\Users\\operator\\Documents\\Finance\\q3-financial-ledger.csv",
        logicalCollectionLabel: { "zh-CN": "工作文档", "en-US": "Work Documents" },
        leafDisplayName: "q3-financial-ledger.csv",
        contentType: "text/csv",
        sizeBytes: 48120,
      },
      {
        vaultItemIdHex: "22".repeat(16),
        artifactIdHex: deterministicDomainDigestHex("artifact", ["design-review-brief"]),
        hostBackingPath: "C:\\Users\\operator\\Documents\\Specs\\design-review-brief.md",
        logicalCollectionLabel: { "zh-CN": "工作文档", "en-US": "Work Documents" },
        leafDisplayName: "design-review-brief.md",
        contentType: "text/markdown",
        sizeBytes: 12480,
      },
      {
        vaultItemIdHex: "33".repeat(16),
        artifactIdHex: deterministicDomainDigestHex("artifact", ["unselected-ssh-key"]),
        hostBackingPath: "/home/operator/.ssh/id_ed25519",
        logicalCollectionLabel: { "zh-CN": "私密凭据库", "en-US": "Private Credentials" },
        leafDisplayName: "id_ed25519",
        contentType: "application/octet-stream",
        sizeBytes: 411,
      },
    ];
    for (const item of defaults) {
      this.registerVaultEntry(item);
    }
  }
}

export function createChooserGrantBroker(options?: {
  readonly initialSessionState?: TrustedSessionLifecycle;
  readonly initialSessionGeneration?: number;
  readonly initialSecuritySealHint?: string;
}): ChooserGrantModalBroker {
  return new ChooserGrantModalBroker(options);
}
