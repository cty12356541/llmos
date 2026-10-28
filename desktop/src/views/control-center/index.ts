// ==========================================================================
// NLOS Stage D — Unified 12-Domain SystemControlCenter, TaskManagerSurface
// & ResourceMonitorSurface (`desktop/src/views/control-center/index.ts`)
// ==========================================================================

import {
  type DisclosureLevel,
  renderStatusIndicatorHtml,
  type WorkstationLocale,
} from "../../ui/status-indicator";
import {
  createSeededCellTopologies,
  evaluateProcessMigration,
  type ProcessMigrationOutcome,
  type ProcessMigrationRequest,
  renderCellLeasePanelHtml,
  type StageCCellTopologyRecord,
} from "./cell-lease-panel";
import {
  type ControlCenterDomainId,
  computeControlDomainSummaries,
  createSeededOwnershipTreeSpecs,
  type EnrichedOwnershipTreeNode,
  enrichOwnershipTree,
  type OwnershipTreeNodeSpec,
  renderDomainSummaryGridHtml,
  renderOwnershipTreeHtml,
  windowOwnershipTreeRows,
} from "./ownership-tree";

export * from "./ownership-tree";
export * from "./cell-lease-panel";
export * from "./blast-radius-modal";

export type WorkstationPerformanceProfile =
  | "SILENT_ECO"
  | "BALANCED"
  | "HIGH_PERFORMANCE";

export type HardwarePermissionDomain =
  | "MICROPHONE"
  | "CAMERA"
  | "SCREEN_CAPTURE"
  | "LOCATION";

export type HardwarePermissionPosture =
  | "AUTHORIZED_ACTIVE"
  | "PROMPT_ON_USE"
  | "REVOKED_KILL_SWITCH";

export interface HardwarePermissionEntry {
  readonly domain: HardwarePermissionDomain;
  readonly labels: Readonly<Record<WorkstationLocale, string>>;
  readonly posture: HardwarePermissionPosture;
  readonly activeHolderCount: number;
}

export interface RecoveryDomainAlertEntry {
  readonly alertId: string;
  readonly names: Readonly<Record<WorkstationLocale, string>>;
  readonly state: "HEALTHY" | "BACKING_OFF" | "ALERTING";
  readonly acknowledged: boolean;
  readonly resumable: boolean;
}

export interface SystemControlCenterState {
  readonly revision: number;
  readonly sessionLocked: boolean;
  readonly sessionGeneration: number;
  readonly activeWorkspaceId: string;
  readonly performanceProfile: WorkstationPerformanceProfile;
  readonly backgroundTierPaused: boolean;
  readonly hardwarePermissions: readonly HardwarePermissionEntry[];
  readonly recoveryAlerts: readonly RecoveryDomainAlertEntry[];
  readonly treeSpecs: readonly OwnershipTreeNodeSpec[];
  readonly cells: readonly StageCCellTopologyRecord[];
}

export interface SystemControlCenterController {
  getState(): SystemControlCenterState;
  getEnrichedTree(options?: {
    readonly locale?: WorkstationLocale;
    readonly disclosure?: DisclosureLevel;
  }): readonly EnrichedOwnershipTreeNode[];
  setPerformanceProfile(profile: WorkstationPerformanceProfile): void;
  setBackgroundTierPaused(paused: boolean): void;
  setHardwarePermissionPosture(
    domain: HardwarePermissionDomain,
    posture: HardwarePermissionPosture,
  ): void;
  acknowledgeRecoveryAlert(alertId: string): void;
  resumeRecoveryDomain(alertId: string): void;
  switchWorkspace(workspaceId: string): void;
  lockDesktopSession(): { readonly sessionGeneration: number };
  unlockDesktopSession(): { readonly sessionGeneration: number };
  requestProcessMigration(
    request: ProcessMigrationRequest,
    locale?: WorkstationLocale,
  ): ProcessMigrationOutcome;
}

const PROFILE_LABELS: Readonly<
  Record<
    WorkstationPerformanceProfile,
    Readonly<Record<WorkstationLocale, string>>
  >
> = {
  SILENT_ECO: { "zh-CN": "静音节能模式", "en-US": "Silent Eco" },
  BALANCED: { "zh-CN": "均衡工作站模式", "en-US": "Balanced Workstation" },
  HIGH_PERFORMANCE: { "zh-CN": "全速计算模式", "en-US": "High Performance" },
};

function escapeHtml(raw: string): string {
  return raw
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

export function createSeededControlCenterState(): SystemControlCenterState {
  return {
    revision: 1,
    sessionLocked: false,
    sessionGeneration: 100,
    activeWorkspaceId: "ws-prod-studio",
    performanceProfile: "BALANCED",
    backgroundTierPaused: false,
    hardwarePermissions: [
      {
        domain: "MICROPHONE",
        labels: { "zh-CN": "麦克风采集", "en-US": "Microphone Capture" },
        posture: "AUTHORIZED_ACTIVE",
        activeHolderCount: 1,
      },
      {
        domain: "CAMERA",
        labels: { "zh-CN": "摄像头输入", "en-US": "Camera Input" },
        posture: "PROMPT_ON_USE",
        activeHolderCount: 0,
      },
      {
        domain: "SCREEN_CAPTURE",
        labels: { "zh-CN": "屏幕录制共享", "en-US": "Screen Recording" },
        posture: "AUTHORIZED_ACTIVE",
        activeHolderCount: 1,
      },
      {
        domain: "LOCATION",
        labels: { "zh-CN": "高精度定位", "en-US": "Precise Location" },
        posture: "REVOKED_KILL_SWITCH",
        activeHolderCount: 0,
      },
    ],
    recoveryAlerts: [
      {
        alertId: "rec-runtime-supervision",
        names: {
          "zh-CN": "运行时进程监督域",
          "en-US": "Runtime Process Supervision",
        },
        state: "BACKING_OFF",
        acknowledged: false,
        resumable: true,
      },
      {
        alertId: "rec-storage-journal",
        names: {
          "zh-CN": "对象存储日志域",
          "en-US": "Object Store WAL Journal",
        },
        state: "HEALTHY",
        acknowledged: true,
        resumable: false,
      },
      {
        alertId: "rec-channel-backpressure",
        names: {
          "zh-CN": "消息通道背压恢复域",
          "en-US": "Channel Backpressure Recovery",
        },
        state: "ALERTING",
        acknowledged: false,
        resumable: true,
      },
    ],
    treeSpecs: createSeededOwnershipTreeSpecs(),
    cells: createSeededCellTopologies(),
  };
}

export function createSystemControlCenterController(
  initialState?: SystemControlCenterState,
): SystemControlCenterController {
  let state: SystemControlCenterState =
    initialState ?? createSeededControlCenterState();

  return {
    getState: () => state,
    getEnrichedTree: (opts) =>
      enrichOwnershipTree(state.treeSpecs, {
        locale: opts?.locale ?? "zh-CN",
        disclosure: opts?.disclosure ?? "L1",
      }),
    setPerformanceProfile: (profile) => {
      state = {
        ...state,
        revision: state.revision + 1,
        performanceProfile: profile,
      };
    },
    setBackgroundTierPaused: (paused) => {
      state = {
        ...state,
        revision: state.revision + 1,
        backgroundTierPaused: paused,
      };
    },
    setHardwarePermissionPosture: (domain, posture) => {
      state = {
        ...state,
        revision: state.revision + 1,
        hardwarePermissions: state.hardwarePermissions.map((p) =>
          p.domain === domain
            ? {
                ...p,
                posture,
                activeHolderCount:
                  posture === "REVOKED_KILL_SWITCH" ? 0 : p.activeHolderCount,
              }
            : p,
        ),
      };
    },
    acknowledgeRecoveryAlert: (alertId) => {
      state = {
        ...state,
        revision: state.revision + 1,
        recoveryAlerts: state.recoveryAlerts.map((a) =>
          a.alertId === alertId ? { ...a, acknowledged: true } : a,
        ),
      };
    },
    resumeRecoveryDomain: (alertId) => {
      state = {
        ...state,
        revision: state.revision + 1,
        recoveryAlerts: state.recoveryAlerts.map((a) =>
          a.alertId === alertId
            ? { ...a, state: "HEALTHY", acknowledged: true, resumable: false }
            : a,
        ),
      };
    },
    switchWorkspace: (workspaceId) => {
      state = {
        ...state,
        revision: state.revision + 1,
        activeWorkspaceId: workspaceId,
      };
    },
    lockDesktopSession: () => {
      const nextGen = state.sessionGeneration + 1;
      state = {
        ...state,
        revision: state.revision + 1,
        sessionLocked: true,
        sessionGeneration: nextGen,
      };
      return { sessionGeneration: nextGen };
    },
    unlockDesktopSession: () => {
      const nextGen = state.sessionGeneration + 1;
      state = {
        ...state,
        revision: state.revision + 1,
        sessionLocked: false,
        sessionGeneration: nextGen,
      };
      return { sessionGeneration: nextGen };
    },
    requestProcessMigration: (request, locale = "zh-CN") => {
      const outcome = evaluateProcessMigration(state.cells, request, locale);
      if (outcome.ok) {
        state = {
          ...state,
          revision: state.revision + 1,
          cells: outcome.updatedCells,
          treeSpecs: state.treeSpecs.map((spec) =>
            spec.nodeId === request.processNodeId
              ? { ...spec, linkedCellId: request.targetCellId }
              : spec,
          ),
        };
      }
      return outcome;
    },
  };
}

/**
 * Renders the `360px` bottom-right `SystemControlCenter` tray flyout HTML (`§2A.1`).
 */
export function renderSystemControlCenterFlyoutHtml(
  state: SystemControlCenterState,
  options?: {
    readonly locale?: WorkstationLocale;
    readonly disclosure?: DisclosureLevel;
  },
): string {
  const locale: WorkstationLocale = options?.locale ?? "zh-CN";
  const disclosure: DisclosureLevel = options?.disclosure ?? "L1";
  const enriched = enrichOwnershipTree(state.treeSpecs, { locale, disclosure });
  const sharedCount = enriched.filter(
    (n) => n.faultDomainPosture === "SHARED_THREAD_LIKE",
  ).length;

  const title =
    locale === "zh-CN" ? "系统控制中心" : "System Control Center";
  const profileLabel = PROFILE_LABELS[state.performanceProfile][locale];
  const bgTierText = state.backgroundTierPaused
    ? locale === "zh-CN"
      ? "后台任务层已暂停"
      : "Background Tier Paused"
    : locale === "zh-CN"
      ? "后台任务层运行中"
      : "Background Tier Active";

  const sharedSummaryText =
    disclosure === "L3"
      ? locale === "zh-CN"
        ? `同进程共享故障域单元: ${sharedCount} [SHARED_THREAD_LIKE]`
        : `Shared-Process Fate Units: ${sharedCount} [SHARED_THREAD_LIKE]`
      : locale === "zh-CN"
        ? `同进程共享故障域单元: ${sharedCount}`
        : `Shared-Process Fate Units: ${sharedCount}`;

  const permsHtml = state.hardwarePermissions
    .map((perm) => {
      const postureText =
        perm.posture === "REVOKED_KILL_SWITCH"
          ? locale === "zh-CN"
            ? "硬件总开关已切断"
            : "Kill-Switch Off"
          : perm.posture === "PROMPT_ON_USE"
            ? locale === "zh-CN"
              ? "每次使用时询问"
              : "Prompt on Use"
            : locale === "zh-CN"
              ? `已授权 (${perm.activeHolderCount})`
              : `Authorized (${perm.activeHolderCount})`;

      return (
        `<div class="nlos-control-center-flyout__perm-item" data-perm-domain="${escapeHtml(
          perm.domain,
        )}" data-perm-posture="${escapeHtml(perm.posture)}">` +
        `<span>${escapeHtml(perm.labels[locale])}</span> · ` +
        `<span>${escapeHtml(postureText)}</span>` +
        `</div>`
      );
    })
    .join("");

  const alertsHtml = state.recoveryAlerts
    .map((alert) => {
      const statusState =
        alert.state === "HEALTHY"
          ? "ACTIVE"
          : alert.state === "BACKING_OFF"
            ? "RECOVERING"
            : "DISPUTED";

      return (
        `<div class="nlos-control-center-flyout__alert-item" data-alert-id="${escapeHtml(
          alert.alertId,
        )}">` +
        `<span>${escapeHtml(alert.names[locale])}</span> ` +
        renderStatusIndicatorHtml({
          state: statusState,
          locale,
          disclosure,
        }) +
        `</div>`
      );
    })
    .join("");

  return (
    `<aside class="nlos-control-center-flyout" role="dialog" aria-label="${escapeHtml(
      title,
    )}" data-session-locked="${state.sessionLocked ? "true" : "false"}">` +
    `<header class="nlos-control-center-flyout__header">` +
    `<h2 class="nlos-control-center-flyout__title">${escapeHtml(title)}</h2>` +
    `<button type="button" class="nlos-control-center-btn" data-action="lock-session">${escapeHtml(
      locale === "zh-CN" ? "锁定工作站" : "Lock Workstation",
    )}</button>` +
    `</header>` +
    `<section class="nlos-control-center-flyout__section">` +
    `<div class="nlos-control-center-flyout__profile-row">` +
    `<strong>${escapeHtml(profileLabel)}</strong> · <span>${escapeHtml(
      bgTierText,
    )}</span> · <span>${escapeHtml(sharedSummaryText)}</span>` +
    `</div>` +
    `</section>` +
    `<section class="nlos-control-center-flyout__section">` +
    `<div class="nlos-control-center-flyout__permission-grid">${permsHtml}</div>` +
    `</section>` +
    `<section class="nlos-control-center-flyout__section">` +
    `<div class="nlos-control-center-flyout__recovery-list">${alertsHtml}</div>` +
    `</section>` +
    `<footer class="nlos-control-center-flyout__section">` +
    `<button type="button" class="nlos-control-center-btn" data-action="open-task-manager">${escapeHtml(
      locale === "zh-CN" ? "打开任务管理器" : "Open Task Manager",
    )}</button> ` +
    `<button type="button" class="nlos-control-center-btn" data-action="open-resource-monitor">${escapeHtml(
      locale === "zh-CN" ? "打开资源监控器" : "Open Resource Monitor",
    )}</button>` +
    `</footer>` +
    `</aside>`
  );
}

/**
 * Renders the full 12-Domain `TaskManagerSurface` & `ResourceMonitorSurface` workstation view HTML.
 */
export function renderControlCenterWorkstationHtml(
  state: SystemControlCenterState,
  options?: {
    readonly locale?: WorkstationLocale;
    readonly disclosure?: DisclosureLevel;
    readonly collapsedNodeIds?: ReadonlySet<string>;
    readonly domainFilter?: ControlCenterDomainId | "all";
    readonly rowLimit?: number;
  },
): string {
  const locale: WorkstationLocale = options?.locale ?? "zh-CN";
  const disclosure: DisclosureLevel = options?.disclosure ?? "L1";
  const enriched = enrichOwnershipTree(state.treeSpecs, { locale, disclosure });
  const summaries = computeControlDomainSummaries(enriched);
  const windowed = windowOwnershipTreeRows(enriched, {
    collapsedNodeIds: options?.collapsedNodeIds,
    domainFilter: options?.domainFilter,
    limit: options?.rowLimit ?? 50,
  });

  const summaryGridHtml = renderDomainSummaryGridHtml(summaries, {
    locale,
    disclosure,
  });
  const treeHtml = renderOwnershipTreeHtml(windowed.visibleRows, {
    locale,
    disclosure,
  });
  const cellPanelHtml = renderCellLeasePanelHtml(state.cells, {
    locale,
    disclosure,
  });

  return (
    `<main class="nlos-control-workstation" data-revision="${state.revision}">` +
    summaryGridHtml +
    treeHtml +
    cellPanelHtml +
    `</main>`
  );
}
