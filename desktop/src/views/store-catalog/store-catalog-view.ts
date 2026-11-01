// NLOS Stage E W80-E6C Desktop Store Transparency View: governed catalog browsing,
// publisher certificate-chain transparency, permission transparency inspection, and
// Z4 security-confirmation handoff entries for install / irreversible upgrade /
// sandbox downgrade.
//
// Authority: ADR-0025 (Graphite/Paper solid industrial workstation; zero blur, zero
// emoji), ADR-0027 section 2.3 (store governance transparency), stage-e-progress.md
// E6. All catalog state is projected from the Rust bridge (nlos-store-registry /
// nlos-publisher-trust data plane); this view never fabricates catalog state and
// never renders its own confirmation screen — sensitive actions only produce an
// entry that hands off to the existing trusted-UI Z4 flow.

import { renderIconSvg } from "../../ui/icons";
import type { DisclosureLevel, WorkstationLocale } from "../../ui";
import type { Z4ConsentChallengeSpec } from "../../trusted-ui";
import { invoke as tauriInvoke } from "@tauri-apps/api/core";

// ---------------------------------------------------------------------------
// Stable bridge constants (mirrored from desktop/src-tauri/src/store_catalog.rs)
// ---------------------------------------------------------------------------

export const STORE_SOURCE_LIVE_REGISTRY = "LIVE_REGISTRY" as const;
export const STORE_SOURCE_OFFLINE_UNAVAILABLE = "OFFLINE_UNAVAILABLE" as const;
export const STORE_CHAIN_STATUS_VERIFIED = "VERIFIED" as const;
export const STORE_CHAIN_STATUS_TRUST_DB_UNAVAILABLE = "TRUST_DB_UNAVAILABLE" as const;
export const STORE_CHAIN_STATUS_VERIFICATION_FAILED = "VERIFICATION_FAILED" as const;
export const STORE_ACTION_INSTALL = "INSTALL" as const;
export const STORE_ACTION_IRREVERSIBLE_UPGRADE = "IRREVERSIBLE_UPGRADE" as const;
export const STORE_ACTION_DOWNGRADE_SANDBOX = "DOWNGRADE_SANDBOX" as const;

// ---------------------------------------------------------------------------
// Wire DTO types (camelCase serde shapes of the Rust bridge)
// ---------------------------------------------------------------------------

export type StoreReversibilityClass = "LOSSLESS_UNDO" | "COMPENSATABLE_ROLLBACK" | "IRREVERSIBLE";

export interface StoreCatalogEntryDto {
  readonly packageIdHex: string;
  readonly packageVersion: number;
  readonly applicationIdsHex: readonly string[];
  readonly publisherIdHex: string;
  readonly publisherTier: string;
  readonly track: "OFFICIAL" | "COMMUNITY" | string;
  readonly channel: "STABLE" | "CANDIDATE" | "RECALLED" | string;
  readonly activeRecallSeverity: string | null;
  readonly componentCount: number;
  readonly permissionCount: number;
  readonly anyNetworkEgress: boolean;
  readonly anyIrreversibleEffect: boolean;
  readonly publisherKeyFingerprintHex: string;
  readonly submittedAtMs: number;
}

export interface StoreCatalogSnapshotDto {
  readonly source: string;
  readonly offlineNoticeZh: string | null;
  readonly offlineNoticeEn: string | null;
  readonly storeRevocationEpoch: number;
  readonly officialStableReleases: readonly StoreCatalogEntryDto[];
  readonly communityCandidates: readonly StoreCatalogEntryDto[];
}

export interface DeclaredPermissionDto {
  readonly capabilityScope: string;
  readonly humanPurpose: string;
  readonly effectClass: "READ_ONLY" | "REVERSIBLE" | "COMPENSATABLE" | "IRREVERSIBLE" | string;
  readonly networkEgressDeclared: boolean;
  readonly sandboxProfile: string;
}

export interface EffectClassLabelDto {
  readonly effectClass: string;
  readonly labelZh: string;
  readonly labelEn: string;
}

export interface PublisherTrustChainDto {
  readonly publisherIdHex: string;
  readonly trustTier: string;
  readonly tierLabelZh: string;
  readonly tierLabelEn: string;
  readonly chainStatus: string;
  readonly chainStatusNoticeZh: string;
  readonly chainStatusNoticeEn: string;
  readonly leafKeyFingerprintHex: string;
  readonly revocationEpoch: number;
  readonly crlFreshUntilMs: number;
  readonly signatureReverified: boolean;
}

export interface StoreReleaseTransparencyDto {
  readonly packageIdHex: string;
  readonly packageVersion: number;
  readonly applicationIdsHex: readonly string[];
  readonly publisherIdHex: string;
  readonly publisherTier: string;
  readonly track: string;
  readonly channel: string;
  readonly permissions: readonly DeclaredPermissionDto[];
  readonly anyNetworkEgress: boolean;
  readonly anyIrreversibleEffect: boolean;
  readonly effectLegend: readonly EffectClassLabelDto[];
  readonly trustChain: PublisherTrustChainDto;
}

export interface AddedCapabilityDto {
  readonly capabilityScope: string;
  readonly humanPurpose: string;
  readonly effectClass: string;
  readonly networkEgressDeclared: boolean;
}

export interface EffectEscalationDto {
  readonly capabilityScope: string;
  readonly oldEffect: string;
  readonly newEffect: string;
}

export interface PurposeChangeDto {
  readonly capabilityScope: string;
  readonly oldPurpose: string;
  readonly newPurpose: string;
}

export interface SandboxProfileChangeDto {
  readonly capabilityScope: string;
  readonly oldProfile: string;
  readonly newProfile: string;
}

export interface StorePermissionDiffDto {
  readonly packageIdHex: string;
  readonly oldVersion: number;
  readonly newVersion: number;
  readonly addedCapabilities: readonly AddedCapabilityDto[];
  readonly removedCapabilityScopes: readonly string[];
  readonly newlyEnabledNetworkEgress: readonly string[];
  readonly escalatedEffects: readonly EffectEscalationDto[];
  readonly escalatedToIrreversible: readonly EffectEscalationDto[];
  readonly purposeChanges: readonly PurposeChangeDto[];
  readonly sandboxProfileChanges: readonly SandboxProfileChangeDto[];
  readonly requiresZ4Reconsent: boolean;
  readonly plainSummaryZh: string;
  readonly plainSummaryEn: string;
}

export interface StoreSensitiveActionInput {
  readonly actionKind: string;
  readonly packageIdHex: string;
  readonly packageVersion: number;
  readonly oldVersion: number | null;
  readonly nowMs: number | null;
}

export interface StoreZ4BlastRadiusRowDto {
  readonly symbol: "+" | "~" | "-" | "=" | string;
  readonly category: string;
  readonly scopeLabel: string;
  readonly targetLabel: string;
  readonly reason: string;
  readonly targetIdHex: string | null;
}

export interface StoreSensitiveActionEntryDto {
  readonly actionKind: string;
  readonly plainActionZh: string;
  readonly plainActionEn: string;
  readonly reversibilityClass: StoreReversibilityClass;
  readonly z4Request: {
    readonly requestingApp: {
      readonly displayName: string;
      readonly publisherName: string;
      readonly signatureVerified: boolean;
      readonly applicationIdHex: string;
      readonly installationIdHex: string;
      readonly principalIdHex: string;
      readonly installationGeneration: number;
      readonly manifestDigestHex: string;
    };
    readonly actionScope: {
      readonly plainSummaryZh: string;
      readonly plainSummaryEn: string;
      readonly capabilityRights: readonly string[];
      readonly controlScope: string;
      readonly planDigestHex: string;
    };
    readonly resourceCeiling: {
      readonly plainCeilingZh: string;
      readonly plainCeilingEn: string;
      readonly memoryCeilingMib: number;
      readonly tokenBudgetCeiling: number;
      readonly networkEgressAllowed: boolean;
      readonly maxDurationMs: number;
    };
    readonly reversibilityAndRisk: {
      readonly reversibilityClass: StoreReversibilityClass;
      readonly plainConsequenceZh: string;
      readonly plainConsequenceEn: string;
      readonly multiTargetCount: number;
      readonly partialFailureDisclosureZh: string | null;
      readonly partialFailureDisclosureEn: string | null;
      readonly blastRadiusRows: readonly StoreZ4BlastRadiusRowDto[] | null;
      readonly requiresDataLossReceipt: boolean | null;
    };
    readonly allowedGrantScopes: readonly string[] | null;
  };
}

// ---------------------------------------------------------------------------
// Bilingual copy (zh-CN / en-US key parity; L1/L2 zero jargon, zero emoji)
// ---------------------------------------------------------------------------

export interface StoreCatalogCopy {
  readonly viewAriaLabel: string;
  readonly headingOfficial: string;
  readonly headingCommunity: string;
  readonly offlineHeading: string;
  readonly refreshLabel: string;
  readonly colPackage: string;
  readonly colVersion: string;
  readonly colPublisher: string;
  readonly colChannel: string;
  readonly colPermissions: string;
  readonly colEgress: string;
  readonly colIrreversible: string;
  readonly channelStable: string;
  readonly channelCandidate: string;
  readonly channelRecalled: string;
  readonly egressAllowed: string;
  readonly egressNone: string;
  readonly inspectorTitle: string;
  readonly inspectorPermissions: string;
  readonly inspectorLegend: string;
  readonly chainTitle: string;
  readonly chainVerifiedBadge: string;
  readonly chainDegradedBadge: string;
  readonly chainFailedBadge: string;
  readonly chainEpochKey: string;
  readonly chainFreshKey: string;
  readonly chainFingerprintKey: string;
  readonly diffTitle: string;
  readonly diffReconsentHeader: string;
  readonly diffIdentical: string;
  readonly diffAddedKey: string;
  readonly diffEgressKey: string;
  readonly diffEscalatedKey: string;
  readonly installEntryLabel: string;
  readonly upgradeEntryLabel: string;
  readonly downgradeEntryLabel: string;
  readonly actionBlockedNotice: string;
  readonly actionErrorNotice: string;
  readonly actionHandoffNotice: string;
  readonly l3PanelTitle: string;
  readonly l3PublisherKey: string;
  readonly l3AppsKey: string;
  readonly l3SandboxKey: string;
  readonly emptyCatalog: string;
}

export const STORE_COPY: Readonly<Record<WorkstationLocale, StoreCatalogCopy>> = {
  "zh-CN": {
    viewAriaLabel: "应用商店透明目录",
    headingOfficial: "官方稳定目录",
    headingCommunity: "社区沙箱候选",
    offlineHeading: "商店目录离线",
    refreshLabel: "刷新目录",
    colPackage: "应用包",
    colVersion: "版本",
    colPublisher: "发布方",
    colChannel: "渠道",
    colPermissions: "权限数",
    colEgress: "网络出站",
    colIrreversible: "不可逆",
    channelStable: "稳定",
    channelCandidate: "候选",
    channelRecalled: "已撤回",
    egressAllowed: "有出站",
    egressNone: "无出站",
    inspectorTitle: "权限透明检视",
    inspectorPermissions: "声明的能力与用途",
    inspectorLegend: "副作用等级说明",
    chainTitle: "发布者证书链",
    chainVerifiedBadge: "证书链已复验",
    chainDegradedBadge: "信任库不可用",
    chainFailedBadge: "证书链复验未通过",
    chainEpochKey: "撤回纪元",
    chainFreshKey: "名单新鲜期至",
    chainFingerprintKey: "叶密钥指纹",
    diffTitle: "跨版本权限差异",
    diffReconsentHeader: "新版本请求更强权限,须重新确认后才能升级",
    diffIdentical: "两个版本的权限声明完全一致。",
    diffAddedKey: "新增能力",
    diffEgressKey: "新开网络出站",
    diffEscalatedKey: "副作用升级",
    installEntryLabel: "安装(进入安全确认)",
    upgradeEntryLabel: "升级(进入安全确认)",
    downgradeEntryLabel: "降档沙箱(进入安全确认)",
    actionBlockedNotice: "证书链未在本机复验通过,安装与升级入口已停用。",
    actionErrorNotice: "安全入口请求被拒绝,请稍后重试或刷新目录。",
    actionHandoffNotice: "已转入系统安全确认屏,请在那里完成核对与授权。",
    l3PanelTitle: "深度核验信息",
    l3PublisherKey: "发布者标识",
    l3AppsKey: "应用标识",
    l3SandboxKey: "沙箱姿态",
    emptyCatalog: "当前目录为空。",
  },
  "en-US": {
    viewAriaLabel: "Store transparency catalog",
    headingOfficial: "Official stable catalog",
    headingCommunity: "Community sandbox candidates",
    offlineHeading: "Store catalog offline",
    refreshLabel: "Refresh catalog",
    colPackage: "Package",
    colVersion: "Version",
    colPublisher: "Publisher",
    colChannel: "Channel",
    colPermissions: "Permissions",
    colEgress: "Network egress",
    colIrreversible: "Irreversible",
    channelStable: "Stable",
    channelCandidate: "Candidate",
    channelRecalled: "Recalled",
    egressAllowed: "Egress on",
    egressNone: "No egress",
    inspectorTitle: "Permission transparency inspector",
    inspectorPermissions: "Declared capabilities and purposes",
    inspectorLegend: "Effect class legend",
    chainTitle: "Publisher certificate chain",
    chainVerifiedBadge: "Chain re-verified",
    chainDegradedBadge: "Trust store unavailable",
    chainFailedBadge: "Chain verification failed",
    chainEpochKey: "Revocation epoch",
    chainFreshKey: "List fresh until",
    chainFingerprintKey: "Leaf key fingerprint",
    diffTitle: "Cross-version permission diff",
    diffReconsentHeader: "The new version requests stronger permissions; re-confirmation is required before upgrading",
    diffIdentical: "Permission declarations are identical across both versions.",
    diffAddedKey: "Added capabilities",
    diffEgressKey: "Newly enabled network egress",
    diffEscalatedKey: "Effect escalation",
    installEntryLabel: "Install (security check)",
    upgradeEntryLabel: "Upgrade (security check)",
    downgradeEntryLabel: "Sandbox downgrade (security check)",
    actionBlockedNotice: "The certificate chain has not been re-verified on this machine; install and upgrade entries are paused.",
    actionErrorNotice: "The security entry request was refused. Retry or refresh the catalog.",
    actionHandoffNotice: "Handed off to the system security confirmation screen to finish review and approval.",
    l3PanelTitle: "Deep verification details",
    l3PublisherKey: "Publisher id",
    l3AppsKey: "Application ids",
    l3SandboxKey: "Sandbox profile",
    emptyCatalog: "The catalog is currently empty.",
  },
};

// ---------------------------------------------------------------------------
// Controller
// ---------------------------------------------------------------------------

export interface StoreCatalogDataClient {
  loadSnapshot(nowMs?: number | null): Promise<StoreCatalogSnapshotDto>;
  loadTransparency(
    packageIdHex: string,
    packageVersion: number,
    nowMs?: number | null,
  ): Promise<StoreReleaseTransparencyDto>;
  loadPermissionDiff(
    packageIdHex: string,
    oldVersion: number,
    newVersion: number,
  ): Promise<StorePermissionDiffDto>;
  prepareSensitiveAction(input: StoreSensitiveActionInput): Promise<StoreSensitiveActionEntryDto>;
}

export interface StoreCatalogActionNotice {
  readonly kind: "blocked" | "handoff" | "error";
  readonly textZh: string;
  readonly textEn: string;
}

export interface StoreCatalogViewState {
  readonly locale: WorkstationLocale;
  readonly disclosure: DisclosureLevel;
  readonly snapshot: StoreCatalogSnapshotDto | null;
  readonly loadPending: boolean;
  readonly loadError: string | null;
  readonly selected: StoreReleaseTransparencyDto | null;
  readonly selectedDiff: StorePermissionDiffDto | null;
  readonly actionPending: boolean;
  readonly lastActionNotice: StoreCatalogActionNotice | null;
  readonly z4HandoffCount: number;
}

export interface StoreCatalogControllerOptions {
  readonly client: StoreCatalogDataClient;
  readonly initialLocale?: WorkstationLocale;
  readonly initialDisclosure?: DisclosureLevel;
  readonly openZ4Challenge?: (spec: Z4ConsentChallengeSpec) => void;
  readonly nowMs?: () => number;
}

export type StoreCatalogListener = (state: StoreCatalogViewState) => void;

function copyFor(locale: WorkstationLocale): StoreCatalogCopy {
  return STORE_COPY[locale];
}

export class StoreCatalogController {
  private readonly client: StoreCatalogDataClient;
  private readonly openZ4Challenge: ((spec: Z4ConsentChallengeSpec) => void) | undefined;
  private readonly clock: () => number;
  private state: StoreCatalogViewState;
  private readonly listeners: StoreCatalogListener[] = [];

  public constructor(options: StoreCatalogControllerOptions) {
    this.client = options.client;
    this.openZ4Challenge = options.openZ4Challenge;
    this.clock = options.nowMs ?? (() => Date.now());
    this.state = {
      locale: options.initialLocale ?? "zh-CN",
      disclosure: options.initialDisclosure ?? "L1",
      snapshot: null,
      loadPending: false,
      loadError: null,
      selected: null,
      selectedDiff: null,
      actionPending: false,
      lastActionNotice: null,
      z4HandoffCount: 0,
    };
  }

  public getSnapshot(): StoreCatalogViewState {
    return this.state;
  }

  public subscribe(listener: StoreCatalogListener): () => void {
    this.listeners.push(listener);
    return () => {
      const idx = this.listeners.indexOf(listener);
      if (idx >= 0) {
        this.listeners.splice(idx, 1);
      }
    };
  }

  private patch(update: Partial<StoreCatalogViewState>): void {
    this.state = { ...this.state, ...update };
    for (const listener of [...this.listeners]) {
      listener(this.state);
    }
  }

  public setLocale(locale: WorkstationLocale): void {
    this.patch({ locale });
  }

  public setDisclosure(disclosure: DisclosureLevel): void {
    this.patch({ disclosure });
  }

  public async refresh(): Promise<void> {
    this.patch({ loadPending: true });
    try {
      const snapshot = await this.client.loadSnapshot(this.clock());
      this.patch({ snapshot, loadPending: false, loadError: null });
    } catch (error) {
      const reason = error instanceof Error ? error.message : "unknown";
      this.patch({ loadPending: false, loadError: `catalog load failed: ${reason}` });
    }
  }

  public async selectRelease(packageIdHex: string, packageVersion: number): Promise<void> {
    try {
      const selected = await this.client.loadTransparency(
        packageIdHex,
        packageVersion,
        this.clock(),
      );
      this.patch({ selected, selectedDiff: null, lastActionNotice: null });
    } catch (error) {
      const reason = error instanceof Error ? error.message : "unknown";
      this.patch({ loadError: `transparency load failed: ${reason}` });
    }
  }

  public async inspectDiff(
    packageIdHex: string,
    oldVersion: number,
    newVersion: number,
  ): Promise<void> {
    try {
      const selectedDiff = await this.client.loadPermissionDiff(
        packageIdHex,
        oldVersion,
        newVersion,
      );
      this.patch({ selectedDiff });
    } catch (error) {
      const reason = error instanceof Error ? error.message : "unknown";
      this.patch({ loadError: `diff load failed: ${reason}` });
    }
  }

  /** 最近一次列出版本中小于当前版本的上一版(供不可逆升级差异比对)。 */
  private resolvePreviousVersion(): number | null {
    const selected = this.state.selected;
    const snapshot = this.state.snapshot;
    if (!selected || !snapshot) {
      return null;
    }
    const versions = [
      ...snapshot.officialStableReleases,
      ...snapshot.communityCandidates,
    ]
      .filter((entry) => entry.packageIdHex === selected.packageIdHex)
      .map((entry) => entry.packageVersion)
      .filter((version) => version < selected.packageVersion);
    if (versions.length === 0) {
      return null;
    }
    return Math.max(...versions);
  }

  public async requestSensitiveAction(actionKind: string): Promise<void> {
    const selected = this.state.selected;
    if (!selected) {
      return;
    }
    const zh = copyFor("zh-CN");
    const en = copyFor("en-US");
    if (selected.trustChain.chainStatus !== STORE_CHAIN_STATUS_VERIFIED) {
      this.patch({
        lastActionNotice: {
          kind: "blocked",
          textZh: zh.actionBlockedNotice,
          textEn: en.actionBlockedNotice,
        },
      });
      return;
    }
    const oldVersion =
      actionKind === STORE_ACTION_IRREVERSIBLE_UPGRADE
        ? (this.resolvePreviousVersion() ?? null)
        : null;
    if (actionKind === STORE_ACTION_IRREVERSIBLE_UPGRADE && oldVersion === null) {
      this.patch({
        lastActionNotice: {
          kind: "blocked",
          textZh: "未找到可比对的上一版本,升级入口暂不可用。",
          textEn: "No previous version is available to compare; the upgrade entry is paused.",
        },
      });
      return;
    }
    this.patch({ actionPending: true });
    try {
      const entry = await this.client.prepareSensitiveAction({
        actionKind,
        packageIdHex: selected.packageIdHex,
        packageVersion: selected.packageVersion,
        oldVersion,
        nowMs: this.clock(),
      });
      const spec = toZ4ConsentChallengeSpec(entry);
      this.patch({
        actionPending: false,
        z4HandoffCount: this.state.z4HandoffCount + 1,
        lastActionNotice: {
          kind: "handoff",
          textZh: zh.actionHandoffNotice,
          textEn: en.actionHandoffNotice,
        },
      });
      this.openZ4Challenge?.(spec);
    } catch {
      this.patch({
        actionPending: false,
        lastActionNotice: {
          kind: "error",
          textZh: zh.actionErrorNotice,
          textEn: en.actionErrorNotice,
        },
      });
    }
  }
}

export function createStoreCatalogController(
  options: StoreCatalogControllerOptions,
): StoreCatalogController {
  return new StoreCatalogController(options);
}

// ---------------------------------------------------------------------------
// Z4 handoff adapter (wire DTO -> existing frontend Z4ConsentChallengeSpec)
// ---------------------------------------------------------------------------

function localePaired(zh: string, en: string): Readonly<Record<WorkstationLocale, string>> {
  return { "zh-CN": zh, "en-US": en };
}

export function toZ4ConsentChallengeSpec(
  entry: StoreSensitiveActionEntryDto,
): Z4ConsentChallengeSpec {
  const req = entry.z4Request;
  return {
    requestingApp: { ...req.requestingApp },
    actionScope: {
      plainSummary: localePaired(
        req.actionScope.plainSummaryZh,
        req.actionScope.plainSummaryEn,
      ),
      capabilityRights: [...req.actionScope.capabilityRights],
      controlScope: req.actionScope.controlScope,
      planDigestHex: req.actionScope.planDigestHex,
    },
    resourceCeiling: {
      plainCeiling: localePaired(
        req.resourceCeiling.plainCeilingZh,
        req.resourceCeiling.plainCeilingEn,
      ),
      memoryCeilingMib: req.resourceCeiling.memoryCeilingMib,
      tokenBudgetCeiling: req.resourceCeiling.tokenBudgetCeiling,
      networkEgressAllowed: req.resourceCeiling.networkEgressAllowed,
      maxDurationMs: req.resourceCeiling.maxDurationMs,
    },
    reversibilityAndRisk: {
      reversibilityClass: req.reversibilityAndRisk.reversibilityClass,
      plainConsequence: localePaired(
        req.reversibilityAndRisk.plainConsequenceZh,
        req.reversibilityAndRisk.plainConsequenceEn,
      ),
      multiTargetCount: req.reversibilityAndRisk.multiTargetCount,
      partialFailureDisclosure:
        req.reversibilityAndRisk.partialFailureDisclosureZh !== null &&
        req.reversibilityAndRisk.partialFailureDisclosureEn !== null
          ? localePaired(
              req.reversibilityAndRisk.partialFailureDisclosureZh,
              req.reversibilityAndRisk.partialFailureDisclosureEn,
            )
          : undefined,
      blastRadiusRows: (req.reversibilityAndRisk.blastRadiusRows ?? []).map((row) => ({
        symbol: row.symbol as "+" | "~" | "-" | "=",
        category: (row.symbol === "+" ? "created" : "reverified") as
          | "created"
          | "reverified",
        scopeLabel: row.scopeLabel,
        targetLabel: row.targetLabel,
        reason: row.reason,
        targetIdHex: row.targetIdHex ?? undefined,
      })),
      requiresDataLossReceipt: req.reversibilityAndRisk.requiresDataLossReceipt ?? false,
    },
    allowedGrantScopes: (req.allowedGrantScopes ?? []) as Z4ConsentChallengeSpec["allowedGrantScopes"],
  };
}

// ---------------------------------------------------------------------------
// Pure HTML renderers (solid opaque Graphite/Paper surfaces, zero blur)
// ---------------------------------------------------------------------------

function escapeHtml(raw: string): string {
  return raw
    .replaceAll("&", "&amp;")
    .replaceAll("<", "&lt;")
    .replaceAll(">", "&gt;")
    .replaceAll('"', "&quot;")
    .replaceAll("'", "&#39;");
}

function channelLabel(copy: StoreCatalogCopy, channel: string): string {
  if (channel === "STABLE") {
    return copy.channelStable;
  }
  if (channel === "CANDIDATE") {
    return copy.channelCandidate;
  }
  if (channel === "RECALLED") {
    return copy.channelRecalled;
  }
  return channel;
}

function tierLabel(
  locale: WorkstationLocale,
  entry: { publisherTier: string },
): string {
  if (entry.publisherTier === "PLATFORM_CORE") {
    return locale === "zh-CN" ? "平台核心发布方" : "Platform core publisher";
  }
  if (entry.publisherTier === "CERTIFIED_PUBLISHER") {
    return locale === "zh-CN" ? "已认证发布方" : "Certified publisher";
  }
  if (entry.publisherTier === "COMMUNITY_SANDBOXED") {
    return locale === "zh-CN" ? "社区沙箱发布方" : "Community sandboxed publisher";
  }
  return entry.publisherTier;
}

function shortId(hexId: string): string {
  return hexId.slice(0, 8);
}

function renderCatalogTableHtml(
  copy: StoreCatalogCopy,
  locale: WorkstationLocale,
  disclosure: DisclosureLevel,
  heading: string,
  rows: readonly StoreCatalogEntryDto[],
): string {
  const body = rows
    .map((entry) => {
      const egress = entry.anyNetworkEgress;
      const irreversible = entry.anyIrreversibleEffect;
      const l3Fingerprint =
        disclosure === "L3"
          ? `<td class="nlos-store-mono">${escapeHtml(entry.publisherKeyFingerprintHex)}</td>`
          : "";
      return [
        `<tr class="nlos-store-row" data-store-pkg="${escapeHtml(entry.packageIdHex)}"`,
        ` data-store-version="${entry.packageVersion}"`,
        ` data-store-tier="${escapeHtml(entry.publisherTier)}"`,
        ` data-store-egress="${egress}"`,
        ` data-store-irreversible="${irreversible}">`,
        `<td class="nlos-store-mono" title="${escapeHtml(entry.packageIdHex)}">${escapeHtml(shortId(entry.packageIdHex))}</td>`,
        `<td class="nlos-store-version">${entry.packageVersion}</td>`,
        `<td>${escapeHtml(tierLabel(locale, entry))}</td>`,
        `<td>${escapeHtml(channelLabel(copy, entry.channel))}</td>`,
        `<td class="nlos-store-version">${entry.permissionCount}</td>`,
        `<td><span class="nlos-store-dot" data-on="${egress}"></span>${escapeHtml(egress ? copy.egressAllowed : copy.egressNone)}</td>`,
        `<td><span class="nlos-store-dot" data-on="${irreversible}"></span></td>`,
        l3Fingerprint,
        `</tr>`,
      ].join("");
    })
    .join("");
  const l3Header = disclosure === "L3" ? `<th>${escapeHtml(copy.chainFingerprintKey)}</th>` : "";
  const emptyRow =
    rows.length === 0
      ? `<tr><td class="nlos-store-empty" colspan="8">${escapeHtml(copy.emptyCatalog)}</td></tr>`
      : "";
  return [
    `<section class="nlos-store-table-block" aria-label="${escapeHtml(heading)}">`,
    `<h3 class="nlos-store-block-title">${escapeHtml(heading)}</h3>`,
    `<table class="nlos-store-table">`,
    "<thead><tr>",
    `<th>${escapeHtml(copy.colPackage)}</th>`,
    `<th>${escapeHtml(copy.colVersion)}</th>`,
    `<th>${escapeHtml(copy.colPublisher)}</th>`,
    `<th>${escapeHtml(copy.colChannel)}</th>`,
    `<th>${escapeHtml(copy.colPermissions)}</th>`,
    `<th>${escapeHtml(copy.colEgress)}</th>`,
    `<th>${escapeHtml(copy.colIrreversible)}</th>`,
    l3Header,
    "</tr></thead>",
    `<tbody>${body}${emptyRow}</tbody>`,
    "</table></section>",
  ].join("");
}

function renderChainPanelHtml(
  copy: StoreCatalogCopy,
  locale: WorkstationLocale,
  chain: PublisherTrustChainDto,
): string {
  const statusClass =
    chain.chainStatus === STORE_CHAIN_STATUS_VERIFIED
      ? "nlos-store-chain-verified"
      : chain.chainStatus === STORE_CHAIN_STATUS_TRUST_DB_UNAVAILABLE
        ? "nlos-store-chain-degraded"
        : "nlos-store-chain-failed";
  const badge =
    chain.chainStatus === STORE_CHAIN_STATUS_VERIFIED
      ? copy.chainVerifiedBadge
      : chain.chainStatus === STORE_CHAIN_STATUS_TRUST_DB_UNAVAILABLE
        ? copy.chainDegradedBadge
        : copy.chainFailedBadge;
  const notice =
    locale === "zh-CN" ? chain.chainStatusNoticeZh : chain.chainStatusNoticeEn;
  return [
    `<section class="nlos-store-chain ${statusClass}" data-store-chain="${escapeHtml(chain.chainStatus)}" aria-label="${escapeHtml(copy.chainTitle)}">`,
    `<header class="nlos-store-chain-head">`,
    `<span class="nlos-store-chain-glyph">${renderIconSvg("shield-seal", { size: 14, strokeWidth: 1.5 })}</span>`,
    `<h3 class="nlos-store-block-title">${escapeHtml(copy.chainTitle)}</h3>`,
    `<span class="nlos-store-chain-badge">${renderIconSvg(chain.chainStatus === STORE_CHAIN_STATUS_VERIFIED ? "check" : "warning", { size: 14, strokeWidth: 1.5 })}${escapeHtml(badge)}</span>`,
    `</header>`,
    `<p class="nlos-store-chain-notice">${escapeHtml(notice)}</p>`,
    `<dl class="nlos-store-kv">`,
    `<div><dt>${escapeHtml(copy.colPublisher)}</dt><dd>${escapeHtml(locale === "zh-CN" ? chain.tierLabelZh : chain.tierLabelEn)}</dd></div>`,
    `<div><dt>${escapeHtml(copy.chainEpochKey)}</dt><dd class="nlos-store-version">${chain.revocationEpoch}</dd></div>`,
    `<div><dt>${escapeHtml(copy.chainFingerprintKey)}</dt><dd class="nlos-store-mono">${escapeHtml(chain.leafKeyFingerprintHex)}</dd></div>`,
    `</dl>`,
    `</section>`,
  ].join("");
}

function renderInspectorHtml(
  copy: StoreCatalogCopy,
  locale: WorkstationLocale,
  disclosure: DisclosureLevel,
  selected: StoreReleaseTransparencyDto,
): string {
  const permissionRows = selected.permissions
    .map(
      (perm) =>
        `<tr data-store-effect="${escapeHtml(perm.effectClass)}">` +
        `<td class="nlos-store-mono">${escapeHtml(perm.capabilityScope)}</td>` +
        `<td>${escapeHtml(perm.humanPurpose)}</td>` +
        `<td><span class="nlos-store-effect-badge" data-store-effect="${escapeHtml(perm.effectClass)}">${escapeHtml(perm.effectClass)}</span></td>` +
        `<td>${perm.networkEgressDeclared ? escapeHtml(copy.egressAllowed) : escapeHtml(copy.egressNone)}</td>` +
        (disclosure === "L3"
          ? `<td class="nlos-store-mono">${escapeHtml(perm.sandboxProfile)}</td>`
          : "") +
        `</tr>`,
    )
    .join("");
  const legend = selected.effectLegend
    .map(
      (item) =>
        `<span class="nlos-store-legend-item">${escapeHtml(locale === "zh-CN" ? item.labelZh : item.labelEn)} (${escapeHtml(item.effectClass)})</span>`,
    )
    .join("");
  const chainPanel = renderChainPanelHtml(copy, locale, selected.trustChain);
  const actionGate = selected.trustChain.chainStatus === STORE_CHAIN_STATUS_VERIFIED;
  const downgradeOnly = selected.trustChain.trustTier === "COMMUNITY_SANDBOXED";
  const actionButtons = [
    `<div class="nlos-store-actions">`,
    `<button type="button" class="nlos-store-action-btn" data-store-action-kind="INSTALL" data-undo-class="${escapeHtml(selected.anyIrreversibleEffect ? "IRREVERSIBLE" : "COMPENSATABLE_ROLLBACK")}" ${actionGate ? "" : "disabled"}>${renderIconSvg("lock", { size: 14, strokeWidth: 1.5 })}${escapeHtml(downgradeOnly ? copy.downgradeEntryLabel : copy.installEntryLabel)}</button>`,
    `</div>`,
  ].join("");
  const l3Panel =
    disclosure === "L3"
      ? [
          `<section class="nlos-store-l3">`,
          `<h4>${renderIconSvg("search", { size: 14, strokeWidth: 1.5 })}${escapeHtml(copy.l3PanelTitle)}</h4>`,
          `<dl class="nlos-store-kv">`,
          `<div><dt>${escapeHtml(copy.l3PublisherKey)}</dt><dd class="nlos-store-mono">${escapeHtml(selected.publisherIdHex)}</dd></div>`,
          `<div><dt>${escapeHtml(copy.l3AppsKey)}</dt><dd class="nlos-store-mono">${escapeHtml(selected.applicationIdsHex.join(" "))}</dd></div>`,
          `<div><dt>${escapeHtml(copy.chainFreshKey)}</dt><dd class="nlos-store-version">${selected.trustChain.crlFreshUntilMs}</dd></div>`,
          `</dl>`,
          `</section>`,
        ].join("")
      : "";
  return [
    `<section class="nlos-store-inspector" aria-label="${escapeHtml(copy.inspectorTitle)}">`,
    `<h3 class="nlos-store-block-title">${renderIconSvg("shield-seal", { size: 14, strokeWidth: 1.5 })}${escapeHtml(copy.inspectorTitle)}</h3>`,
    chainPanel,
    `<h4>${escapeHtml(copy.inspectorPermissions)}</h4>`,
    `<table class="nlos-store-table">`,
    `<thead><tr><th></th><th></th><th>${escapeHtml(copy.inspectorLegend)}</th><th>${escapeHtml(copy.colEgress)}</th>${disclosure === "L3" ? `<th>${escapeHtml(copy.l3SandboxKey)}</th>` : ""}</tr></thead>`,
    `<tbody>${permissionRows}</tbody>`,
    `</table>`,
    `<div class="nlos-store-legend">${legend}</div>`,
    actionButtons,
    l3Panel,
    `</section>`,
  ].join("");
}

function renderDiffPanelHtml(
  copy: StoreCatalogCopy,
  locale: WorkstationLocale,
  diff: StorePermissionDiffDto,
): string {
  const summary = locale === "zh-CN" ? diff.plainSummaryZh : diff.plainSummaryEn;
  const rows: string[] = [];
  for (const added of diff.addedCapabilities) {
    rows.push(
      `<tr><td class="nlos-store-blast">+</td><td class="nlos-store-mono">${escapeHtml(added.capabilityScope)}</td><td>${escapeHtml(added.humanPurpose)}</td><td>${escapeHtml(added.effectClass)}</td></tr>`,
    );
  }
  for (const scope of diff.newlyEnabledNetworkEgress) {
    rows.push(
      `<tr><td class="nlos-store-blast">~</td><td class="nlos-store-mono">${escapeHtml(scope)}</td><td>${escapeHtml(copy.diffEgressKey)}</td><td>NETWORK</td></tr>`,
    );
  }
  for (const esc of diff.escalatedEffects) {
    rows.push(
      `<tr><td class="nlos-store-blast">~</td><td class="nlos-store-mono">${escapeHtml(esc.capabilityScope)}</td><td>${escapeHtml(copy.diffEscalatedKey)}</td><td>${escapeHtml(esc.oldEffect)} → ${escapeHtml(esc.newEffect)}</td></tr>`,
    );
  }
  const banner = diff.requiresZ4Reconsent
    ? `<div class="nlos-store-diff-reconsent" role="alert">${renderIconSvg("warning", { size: 14, strokeWidth: 1.5 })}<div><strong>${escapeHtml(copy.diffReconsentHeader)}</strong><p>${escapeHtml(summary)}</p></div></div>`
    : `<div class="nlos-store-diff-quiet">${renderIconSvg("check", { size: 14, strokeWidth: 1.5 })}${escapeHtml(copy.diffIdentical)}</div>`;
  return [
    `<section class="nlos-store-diff" aria-label="${escapeHtml(copy.diffTitle)}">`,
    `<h3 class="nlos-store-block-title">${escapeHtml(copy.diffTitle)}</h3>`,
    banner,
    `<table class="nlos-store-table"><tbody>${rows.join("")}</tbody></table>`,
    `</section>`,
  ].join("");
}

export function renderStoreCatalogHtml(state: StoreCatalogViewState): string {
  const copy = copyFor(state.locale);
  const snapshot = state.snapshot;
  const posture = snapshot?.source ?? "INITIAL";
  const parts: string[] = [
    `<section class="nlos-store-view" data-store-posture="${escapeHtml(posture)}" aria-label="${escapeHtml(copy.viewAriaLabel)}">`,
  ];
  if (snapshot && snapshot.source === STORE_SOURCE_OFFLINE_UNAVAILABLE) {
    const notice =
      state.locale === "zh-CN"
        ? (snapshot.offlineNoticeZh ?? "")
        : (snapshot.offlineNoticeEn ?? "");
    parts.push(
      `<div class="nlos-store-offline" role="status"><strong>${escapeHtml(copy.offlineHeading)}</strong><p>${escapeHtml(notice)}</p></div>`,
    );
  }
  if (snapshot) {
    parts.push(
      renderCatalogTableHtml(
        copy,
        state.locale,
        state.disclosure,
        copy.headingOfficial,
        snapshot.officialStableReleases,
      ),
    );
    parts.push(
      renderCatalogTableHtml(
        copy,
        state.locale,
        state.disclosure,
        copy.headingCommunity,
        snapshot.communityCandidates,
      ),
    );
  }
  if (state.selected) {
    parts.push(renderInspectorHtml(copy, state.locale, state.disclosure, state.selected));
  }
  if (state.selectedDiff) {
    parts.push(renderDiffPanelHtml(copy, state.locale, state.selectedDiff));
  }
  if (state.lastActionNotice) {
    const text =
      state.locale === "zh-CN"
        ? state.lastActionNotice.textZh
        : state.lastActionNotice.textEn;
    parts.push(
      `<div class="nlos-store-notice" data-store-notice-kind="${state.lastActionNotice.kind}" role="status">${escapeHtml(text)}</div>`,
    );
  }
  parts.push("</section>");
  return parts.join("");
}

// ---------------------------------------------------------------------------
// Tauri client (W80-INT wiring seam; command names frozen on the Rust side)
// ---------------------------------------------------------------------------

type TauriInvoke = (cmd: string, args?: Record<string, unknown>) => Promise<unknown>;

export function createTauriStoreCatalogClient(
  invokeFn?: TauriInvoke,
): StoreCatalogDataClient {
  const invokeImpl: TauriInvoke = invokeFn ?? ((cmd, args) => tauriInvoke(cmd, args));
  return {
    loadSnapshot: (nowMs) =>
      invokeImpl("store_catalog_snapshot", {
        storeRootOverride: null,
        trustDbOverride: null,
        nowMs: nowMs ?? null,
      }) as Promise<StoreCatalogSnapshotDto>,
    loadTransparency: (packageIdHex, packageVersion, nowMs) =>
      invokeImpl("store_release_transparency", {
        packageIdHex,
        packageVersion,
        nowMs: nowMs ?? null,
        storeRootOverride: null,
        trustDbOverride: null,
      }) as Promise<StoreReleaseTransparencyDto>,
    loadPermissionDiff: (packageIdHex, oldVersion, newVersion) =>
      invokeImpl("store_permission_diff", {
        packageIdHex,
        oldVersion,
        newVersion,
        storeRootOverride: null,
        trustDbOverride: null,
      }) as Promise<StorePermissionDiffDto>,
    prepareSensitiveAction: (input) =>
      invokeImpl("store_prepare_sensitive_action", {
        input: {
          actionKind: input.actionKind,
          packageIdHex: input.packageIdHex,
          packageVersion: input.packageVersion,
          oldVersion: input.oldVersion ?? null,
          nowMs: input.nowMs ?? null,
        },
        storeRootOverride: null,
        trustDbOverride: null,
      }) as Promise<StoreSensitiveActionEntryDto>,
  };
}
