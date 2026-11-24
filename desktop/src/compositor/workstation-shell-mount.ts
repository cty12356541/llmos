// ==========================================================================
// NLOS Industrial Workstation Shell Surface Mounter (`workstation-shell-mount.ts`)
// Authority: ADR-0025 Rev 2 + Stage D Desktop Visual & UX Spec
// Connects all first-party industrial workstation surfaces into:
//   - Z2 Tiling Window Layer (3-Pane Default: Upper-Left 12-Domain Control &
//     Resource Center + Upper-Right 5-Tab DAG / Store / Settings / Search / Probes Workbench)
//   - Z1 Bottom Docked Task Space (Bidirectionally synced with TaskSpaceController)
//   - Z3 System Flyouts Slot (CONNECTIVITY -> ClusterPostureTray,
//     NOTIFICATIONS -> NotificationCenter, CONTROL_CENTER -> SystemControlCenterFlyout,
//     SPOTLIGHT_SEARCH -> SpotlightView)
// Strict Discipline:
//   - ANTI-AI-01..07 compliant (0 emojis, 0 internal jargon in L0/L1, 0/4/6px geometry)
//   - First-party trusted surfaces only; untrusted third-party apps remain in sandboxed <iframe>
// ==========================================================================

import {
  createBulletMicroBarElement,
  createStatusIndicatorElement,
  renderReceiptAuditBarHtml,
  type DisclosureLevel,
  type WorkstationLocale,
  type WorkstationStatusState,
} from "../ui";
import {
  ALL_CONTROL_CENTER_DOMAINS,
  renderControlCenterWorkstationHtml,
  renderSystemControlCenterFlyoutHtml,
  type ControlCenterDomainId,
  type SystemControlCenterController,
  type SystemControlCenterState,
} from "../views/control-center";
import {
  createNotificationCenterElement,
  type NotificationCenterController,
} from "../views/notifications";
import {
  createClusterPostureTrayElement,
  type ClusterPostureController,
} from "../views/offline-cluster";
import type {
  SettingsDomainLayer,
  WorkstationSettingsSuiteController,
} from "../views/settings";
import {
  renderSpotlightViewHtml,
  type SpotlightCandidateCategory,
  type SpotlightController,
} from "../views/spotlight";
import {
  createStoreCatalogController,
  renderStoreCatalogHtml,
  STORE_CHAIN_STATUS_VERIFIED,
  STORE_SOURCE_LIVE_REGISTRY,
  type StoreCatalogController,
  type StoreCatalogEntryDto,
  type StoreCatalogSnapshotDto,
  type StorePermissionDiffDto,
  type StoreReleaseTransparencyDto,
  type StoreSensitiveActionEntryDto,
  type StoreSensitiveActionInput,
} from "../views/store-catalog";
import {
  renderTaskAttemptSwimlanesHtml,
  renderTaskPlanDagCanvasHtml,
  type TaskSpaceSurfaceController,
  type TaskSpaceWorkstationRow,
} from "../views/task-space";
import type {
  WorkstationCompositor,
  WorkstationTaskRowInput,
} from "./workstation-compositor";

export type WorkbenchTabId = "dag" | "store" | "settings" | "search" | "probes";

export const WORKSTATION_CONTROL_WINDOW_ID = "win-system-control-center";
export const WORKSTATION_WORKBENCH_WINDOW_ID = "win-workstation-workbench";

const SYSTEM_CONTROL_APP_ID_HEX = "01".repeat(16);
const SYSTEM_WORKBENCH_APP_ID_HEX = "02".repeat(16);

const DOMAIN_FILTER_LABELS: Readonly<
  Record<ControlCenterDomainId, Readonly<Record<WorkstationLocale, string>>>
> = {
  "system-device": {
    "zh-CN": "系统与硬件设备",
    "en-US": "System & Hardware",
  },
  workspace: {
    "zh-CN": "工作区与租户",
    "en-US": "Workspace & Tenant",
  },
  application: {
    "zh-CN": "已安装应用",
    "en-US": "Installed Applications",
  },
  task: {
    "zh-CN": "任务生命周期",
    "en-US": "Task Lifecycle",
  },
  "task-group": {
    "zh-CN": "任务编排组",
    "en-US": "Task Groups",
  },
  "task-node": {
    "zh-CN": "执行步骤节点",
    "en-US": "Execution Step Nodes",
  },
  "task-attempt": {
    "zh-CN": "执行尝试记录",
    "en-US": "Execution Runs",
  },
  "execution-unit": {
    "zh-CN": "进程与并发流",
    "en-US": "Processes & Execution Units",
  },
  "resource-group": {
    "zh-CN": "资源保障池",
    "en-US": "Resource Pools",
  },
  "topic-channel": {
    "zh-CN": "事件通道与队列",
    "en-US": "Event Topics & Queues",
  },
  operation: {
    "zh-CN": "外部操作与外设",
    "en-US": "External Operations & Devices",
  },
  cell: {
    "zh-CN": "计算节点与分布式租约",
    "en-US": "Compute Nodes & Distributed Leases",
  },
};

interface ShellCopy {
  readonly controlWindowTitle: string;
  readonly workbenchWindowTitle: string;
  readonly domainFilterAll: string;
  readonly tabDag: string;
  readonly tabStore: string;
  readonly tabSettings: string;
  readonly tabSearch: string;
  readonly tabProbes: string;
  readonly taskSwitchLabel: string;
  readonly stagePipelineHeading: string;
  readonly stageMetricApp: string;
  readonly stageMetricCompletion: string;
  readonly stageMetricUndo: string;
  readonly storeSourceLive: string;
  readonly storeSourceShowcase: string;
  readonly storeRefreshBtn: string;
  readonly searchInputPlaceholder: string;
  readonly searchBtnLabel: string;
}

const SHELL_COPY: Readonly<Record<WorkstationLocale, ShellCopy>> = {
  "zh-CN": {
    controlWindowTitle: "系统控制与资源中心 (12 域显控)",
    workbenchWindowTitle: "工作站编排与生态治理台",
    domainFilterAll: "全部域 (12)",
    tabDag: "任务编排图谱",
    tabStore: "应用商店目录",
    tabSettings: "系统设置与密钥链",
    tabSearch: "全局只读检索",
    tabProbes: "内核诊断与模型池",
    taskSwitchLabel: "当前检视任务：",
    stagePipelineHeading: "四阶段正交执行拓扑与依赖流水线",
    stageMetricApp: "归属应用套件",
    stageMetricCompletion: "阶段目标完成度",
    stageMetricUndo: "操作可逆与恢复保障",
    storeSourceLive: "实时本地商店目录",
    storeSourceShowcase: "内置参考生态样板包 (4)",
    storeRefreshBtn: "刷新目录",
    searchInputPlaceholder: "输入关键词检索任务、已装应用或工作区制品…",
    searchBtnLabel: "检索",
  },
  "en-US": {
    controlWindowTitle: "System Control & Resource Center (12 Domains)",
    workbenchWindowTitle: "Workstation Orchestration & Governance Bench",
    domainFilterAll: "All Domains (12)",
    tabDag: "Task Graph & Lanes",
    tabStore: "App Store Catalog",
    tabSettings: "Settings & Keychain",
    tabSearch: "Global Read-Only Search",
    tabProbes: "Kernel Probes & Model Pool",
    taskSwitchLabel: "Active Task:",
    stagePipelineHeading: "4-Stage Orthogonal Execution Pipeline & Dependency Flow",
    stageMetricApp: "Owning Application",
    stageMetricCompletion: "Target Completion",
    stageMetricUndo: "Reversibility & Safeguard",
    storeSourceLive: "Live Local Store Registry",
    storeSourceShowcase: "Reference Ecosystem Packages (4)",
    storeRefreshBtn: "Refresh",
    searchInputPlaceholder: "Search tasks, installed applications, or workspace artifacts…",
    searchBtnLabel: "Search",
  },
};

/* ==========================================================================
   Built-in Stage E Reference Ecosystem Showcase Fixtures (for Store Demo)
   ========================================================================== */

const SHOWCASE_STORE_ENTRIES: readonly StoreCatalogEntryDto[] = [
  {
    packageIdHex: "a1".repeat(16),
    packageVersion: 1_099_511_627_776, // 1.0.0
    applicationIdsHex: ["31".repeat(16)],
    publisherIdHex: "41".repeat(16),
    publisherTier: "CERTIFIED_PUBLISHER",
    track: "OFFICIAL",
    channel: "STABLE",
    activeRecallSeverity: null,
    componentCount: 3,
    permissionCount: 2,
    anyNetworkEgress: false,
    anyIrreversibleEffect: false,
    publisherKeyFingerprintHex: "9f86d081884c7d65",
    submittedAtMs: 1_761_900_000_000,
  },
  {
    packageIdHex: "b2".repeat(16),
    packageVersion: 1_099_511_627_777, // 1.0.1
    applicationIdsHex: ["32".repeat(16)],
    publisherIdHex: "42".repeat(16),
    publisherTier: "CERTIFIED_PUBLISHER",
    track: "OFFICIAL",
    channel: "STABLE",
    activeRecallSeverity: null,
    componentCount: 4,
    permissionCount: 3,
    anyNetworkEgress: true,
    anyIrreversibleEffect: false,
    publisherKeyFingerprintHex: "7c4a8d09ca3762af",
    submittedAtMs: 1_761_920_000_000,
  },
  {
    packageIdHex: "c3".repeat(16),
    packageVersion: 1_099_511_627_776,
    applicationIdsHex: ["33".repeat(16)],
    publisherIdHex: "43".repeat(16),
    publisherTier: "PLATFORM_CORE",
    track: "OFFICIAL",
    channel: "STABLE",
    activeRecallSeverity: null,
    componentCount: 2,
    permissionCount: 2,
    anyNetworkEgress: true,
    anyIrreversibleEffect: false,
    publisherKeyFingerprintHex: "e3b0c44298fc1c14",
    submittedAtMs: 1_761_940_000_000,
  },
  {
    packageIdHex: "d4".repeat(16),
    packageVersion: 1_099_511_627_776,
    applicationIdsHex: ["34".repeat(16)],
    publisherIdHex: "44".repeat(16),
    publisherTier: "COMMUNITY_SANDBOXED",
    track: "COMMUNITY",
    channel: "CANDIDATE",
    activeRecallSeverity: null,
    componentCount: 2,
    permissionCount: 2,
    anyNetworkEgress: true,
    anyIrreversibleEffect: true,
    publisherKeyFingerprintHex: "5e884898da280471",
    submittedAtMs: 1_761_960_000_000,
  },
];

function buildShowcaseStoreController(
  initialLocale: WorkstationLocale,
  initialDisclosure: DisclosureLevel,
  openZ4Challenge?: Parameters<typeof createStoreCatalogController>[0]["openZ4Challenge"],
): StoreCatalogController {
  const snapshot: StoreCatalogSnapshotDto = {
    source: STORE_SOURCE_LIVE_REGISTRY,
    offlineNoticeZh: null,
    offlineNoticeEn: null,
    storeRevocationEpoch: 3,
    officialStableReleases: SHOWCASE_STORE_ENTRIES.slice(0, 3),
    communityCandidates: SHOWCASE_STORE_ENTRIES.slice(3),
  };

  const buildTransparency = (pkgHex: string): StoreReleaseTransparencyDto => {
    const entry =
      SHOWCASE_STORE_ENTRIES.find((e) => e.packageIdHex === pkgHex) ??
      SHOWCASE_STORE_ENTRIES[0]!;
    return {
      packageIdHex: entry.packageIdHex,
      packageVersion: entry.packageVersion,
      applicationIdsHex: entry.applicationIdsHex,
      publisherIdHex: entry.publisherIdHex,
      publisherTier: entry.publisherTier,
      track: entry.track,
      channel: entry.channel,
      permissions: [
        {
          capabilityScope: "artifact.workspace.read",
          humanPurpose: "读取当前工作区文档与结构化归档制品",
          effectClass: "READ_ONLY",
          networkEgressDeclared: false,
          sandboxProfile: "wasm-dual-layer-strict",
        },
        {
          capabilityScope: "task.plan.propose",
          humanPurpose: "向工作区提交多步执行计划提案并生成核验回执",
          effectClass: entry.anyIrreversibleEffect ? "IRREVERSIBLE" : "COMPENSATABLE",
          networkEgressDeclared: entry.anyNetworkEgress,
          sandboxProfile: "wasm-dual-layer-strict",
        },
      ],
      anyNetworkEgress: entry.anyNetworkEgress,
      anyIrreversibleEffect: entry.anyIrreversibleEffect,
      effectLegend: [
        { effectClass: "READ_ONLY", labelZh: "只读", labelEn: "Read-only" },
        { effectClass: "REVERSIBLE", labelZh: "可逆", labelEn: "Reversible" },
        { effectClass: "COMPENSATABLE", labelZh: "可补偿", labelEn: "Compensatable" },
        { effectClass: "IRREVERSIBLE", labelZh: "不可逆", labelEn: "Irreversible" },
      ],
      trustChain: {
        publisherIdHex: entry.publisherIdHex,
        trustTier: entry.publisherTier,
        tierLabelZh:
          entry.publisherTier === "PLATFORM_CORE"
            ? "平台核心签名"
            : entry.publisherTier === "CERTIFIED_PUBLISHER"
              ? "已认证发布方"
              : "社区双层沙箱",
        tierLabelEn:
          entry.publisherTier === "PLATFORM_CORE"
            ? "Platform Core"
            : entry.publisherTier === "CERTIFIED_PUBLISHER"
              ? "Certified Publisher"
              : "Community Sandboxed",
        chainStatus: STORE_CHAIN_STATUS_VERIFIED,
        chainStatusNoticeZh: "三级 Ed25519 发布证书链与单调吊销纪元已在本机复验通过。",
        chainStatusNoticeEn:
          "3-tier Ed25519 publisher certificate chain and monotonic revocation epoch verified locally.",
        leafKeyFingerprintHex: entry.publisherKeyFingerprintHex,
        revocationEpoch: 3,
        crlFreshUntilMs: 8_000_000_000_000,
        signatureReverified: true,
      },
    };
  };

  const diff: StorePermissionDiffDto = {
    packageIdHex: SHOWCASE_STORE_ENTRIES[0]!.packageIdHex,
    oldVersion: 1_099_511_627_776,
    newVersion: 1_099_511_627_777,
    addedCapabilities: [
      {
        capabilityScope: "channel.external.sync",
        humanPurpose: "向受控外部端点同步结构化勘测摘要",
        effectClass: "COMPENSATABLE",
        networkEgressDeclared: true,
      },
    ],
    removedCapabilityScopes: [],
    newlyEnabledNetworkEgress: ["channel.external.sync"],
    escalatedEffects: [],
    escalatedToIrreversible: [],
    purposeChanges: [],
    sandboxProfileChanges: [],
    requiresZ4Reconsent: true,
    plainSummaryZh:
      "升级版本新增 1 项受控外部同步权限，安装或升级前需经系统安全确认屏显式授权。",
    plainSummaryEn:
      "New version adds 1 controlled external sync permission; explicit confirmation on the system security screen is required.",
  };

  return createStoreCatalogController({
    client: {
      loadSnapshot: async () => snapshot,
      loadTransparency: async (pkgHex) => buildTransparency(pkgHex),
      loadPermissionDiff: async () => diff,
      prepareSensitiveAction: async (
        input: StoreSensitiveActionInput,
      ): Promise<StoreSensitiveActionEntryDto> => {
        const isIrrev = input.actionKind === "IRREVERSIBLE_UPGRADE";
        return {
          actionKind: input.actionKind,
          plainActionZh: `对受控生态应用包 ${input.packageIdHex.slice(0, 8)} 执行 ${input.actionKind} 操作`,
          plainActionEn: `Execute ${input.actionKind} on governed package ${input.packageIdHex.slice(0, 8)}`,
          reversibilityClass: isIrrev ? "IRREVERSIBLE" : "COMPENSATABLE_ROLLBACK",
          z4Request: {
            requestingApp: {
              displayName: `生态样板应用 (${input.packageIdHex.slice(0, 8)})`,
              publisherName: "NLOS Verified Ecosystem Publisher",
              signatureVerified: true,
              applicationIdHex: "31".repeat(16),
              installationIdHex: "21".repeat(16),
              principalIdHex: "11".repeat(16),
              installationGeneration: 1,
              manifestDigestHex: "f0".repeat(32),
            },
            actionScope: {
              plainSummaryZh: `从商店目录执行 ${input.actionKind} 并绑定双层沙箱边界`,
              plainSummaryEn: `Execute ${input.actionKind} from store catalog with dual-layer sandbox ceiling`,
              capabilityRights: ["artifact.workspace.read", "task.plan.propose"],
              controlScope: "application",
              planDigestHex: "e0".repeat(32),
            },
            resourceCeiling: {
              plainCeilingZh: "内存上限 512 MiB · 零原始凭据暴露",
              plainCeilingEn: "Memory ceiling 512 MiB · Zero raw credential exposure",
              memoryCeilingMib: 512,
              tokenBudgetCeiling: 50_000,
              networkEgressAllowed: false,
              maxDurationMs: 60_000,
            },
            reversibilityAndRisk: {
              reversibilityClass: isIrrev ? "IRREVERSIBLE" : "COMPENSATABLE_ROLLBACK",
              plainConsequenceZh: isIrrev
                ? "不可逆数据迁移：确认前已验证预备份回执"
                : "支持快照回滚与零残留卸载",
              plainConsequenceEn: isIrrev
                ? "Irreversible migration: pre-upgrade backup verified"
                : "Supports snapshot rollback and zero-residue uninstall",
              multiTargetCount: 1,
              partialFailureDisclosureZh: null,
              partialFailureDisclosureEn: null,
              blastRadiusRows: [
                {
                  symbol: "+",
                  category: "CAPABILITY_BOUNDARY",
                  scopeLabel: "artifact.workspace.read",
                  targetLabel: input.packageIdHex.slice(0, 8),
                  reason: "授予工作区制品只读与计划提案权限（双层沙箱隔离）",
                  targetIdHex: input.packageIdHex,
                },
              ],
              requiresDataLossReceipt: isIrrev,
            },
            allowedGrantScopes: ["once", "session"],
          },
        };
      },
    },
    initialLocale,
    initialDisclosure,
    openZ4Challenge,
  });
}

function mapTaskSpaceRowToCompositorTask(
  row: TaskSpaceWorkstationRow,
  locale: WorkstationLocale,
): WorkstationTaskRowInput {
  const st: string = row.summary.taskState;
  let status: WorkstationStatusState = "RUNNING";
  let isTerminal = false;
  let pendingApproval = false;
  if (st === "COMPLETED") {
    status = "COMPLETED";
    isTerminal = true;
  } else if (st === "FAILED") {
    status = "FAILED";
    isTerminal = true;
  } else if (st === "CANCELLED") {
    status = "CANCELLED";
    isTerminal = true;
  } else if (st === "SUSPENDED") {
    status = "SUSPENDED";
  } else if (st === "WAITING_CONSENT") {
    status = "WAITING_CONSENT";
    pendingApproval = true;
  } else if (st === "WAITING_INPUT") {
    status = "WAITING_INPUT";
    pendingApproval = true;
  } else if (st === "QUARANTINED") {
    status = "QUARANTINED";
  }

  return {
    taskId: row.summary.taskId,
    title: locale === "en-US" ? row.goalTitleEn : row.goalTitleZh,
    status,
    actualUsage: row.summary.usageUnits,
    effectiveGrantedOrLeaseCap: row.summary.effectiveUnits,
    requestedCeiling: row.summary.requestedUnits,
    resourceUnit: "%",
    planIdHex: row.summary.planId,
    readBackingIdHex: row.summary.taskId,
    expectedRevision: row.summary.planRevision,
    canPause: status === "RUNNING",
    canResume: status === "SUSPENDED",
    canCancel: !isTerminal,
    isTerminal,
    pendingApproval,
    summaryNote:
      locale === "en-US" ? row.applicationNameEn : row.applicationNameZh,
  };
}

export interface MountIndustrialWorkstationSurfacesOptions {
  readonly doc?: Document;
  readonly compositor: WorkstationCompositor;
  readonly controlCenterController: SystemControlCenterController;
  readonly taskSpaceController: TaskSpaceSurfaceController;
  readonly storeCatalogController: StoreCatalogController;
  readonly notificationController: NotificationCenterController;
  readonly spotlightController: SpotlightController;
  readonly clusterPostureController: ClusterPostureController;
  readonly settingsSuiteController: WorkstationSettingsSuiteController;
  readonly legacyDiagnosticsElement?: HTMLElement | null;
  readonly openZ4FromStore?: NonNullable<
    Parameters<typeof createStoreCatalogController>[0]["openZ4Challenge"]
  >;
}

export interface IndustrialWorkstationShellMountHandle {
  selectWorkbenchTab(tab: WorkbenchTabId): void;
  getActiveWorkbenchTab(): WorkbenchTabId;
  setDomainFilter(domain: ControlCenterDomainId | "all"): void;
  focusOrRestoreControlWindow(): void;
  focusOrRestoreWorkbenchWindow(tab?: WorkbenchTabId): void;
}

export function mountIndustrialWorkstationSurfaces(
  options: MountIndustrialWorkstationSurfacesOptions,
): IndustrialWorkstationShellMountHandle {
  const {
    compositor,
    controlCenterController,
    taskSpaceController,
    storeCatalogController,
    notificationController,
    spotlightController,
    clusterPostureController,
    settingsSuiteController,
  } = options;

  let activeDomainFilter: ControlCenterDomainId | "all" = "all";
  let activeWorkbenchTab: WorkbenchTabId = "dag";
  let activeSettingsLayer: SettingsDomainLayer = "user";
  let storeMode: "live" | "showcase" = "live";
  let controlFeedbackText: string | null = null;
  const collapsedNodeIds = new Set<string>();
  let liveRefreshControlPane = null as (() => void) | null;
  let liveRefreshWorkbenchPane = null as (() => void) | null;

  const initialSnap = compositor.getSnapshot();

  // Built-in showcase store controller so users can always inspect & test 4 Stage E packages
  const showcaseStoreController = buildShowcaseStoreController(
    initialSnap.locale,
    initialSnap.disclosure,
    options.openZ4FromStore,
  );
  void showcaseStoreController.refresh().catch(() => undefined);
  void storeCatalogController.refresh().catch(() => undefined);

  // Seed Z1 bottom task space from TaskSpaceSurfaceController when compositor has no initial tasks
  if (initialSnap.tasks.length === 0) {
    const taskRows = taskSpaceController.getRows();
    if (taskRows.length > 0) {
      compositor.setTasks(
        taskRows.map((r) => mapTaskSpaceRowToCompositorTask(r, initialSnap.locale)),
      );
    }
  }

  /* ==========================================================================
     1. Upper-Left Z2 Window: 12-Domain Control & Resource Center
     ========================================================================== */

  const buildControlCenterSurface = (ctx: {
    readonly doc: Document;
    readonly locale: WorkstationLocale;
    readonly disclosure: DisclosureLevel;
  }): HTMLElement => {
    const copy = SHELL_COPY[ctx.locale];
    const root = ctx.doc.createElement("div");
    root.className = "nlos-workstation-control-pane";
    root.style.display = "flex";
    root.style.flexDirection = "column";
    root.style.height = "100%";

    const filterBar = ctx.doc.createElement("div");
    filterBar.className = "nlos-domain-filter-bar";
    filterBar.style.display = "flex";
    filterBar.style.flexWrap = "wrap";
    filterBar.style.gap = "4px";
    filterBar.style.padding = "6px 10px";
    filterBar.style.borderBottom = "1px solid var(--nlos-border-hairline)";
    filterBar.style.backgroundColor = "var(--nlos-bg-chrome)";

    const feedbackStrip = ctx.doc.createElement("div");
    feedbackStrip.className = "nlos-action-feedback-strip";
    feedbackStrip.hidden = true;
    feedbackStrip.setAttribute("hidden", "");

    const makeFilterBtn = (
      id: ControlCenterDomainId | "all",
      label: string,
    ): HTMLElement => {
      const btn = ctx.doc.createElement("button");
      btn.type = "button";
      const isSelected = activeDomainFilter === id;
      btn.className = isSelected ? "nlos-btn nlos-btn--primary" : "nlos-btn";
      btn.setAttribute("data-domain-filter", id);
      btn.setAttribute("aria-pressed", isSelected ? "true" : "false");
      btn.textContent = label;
      btn.addEventListener("click", () => {
        activeDomainFilter = id;
        renderBody();
      });
      return btn;
    };

    const renderFilterButtons = (): void => {
      filterBar.replaceChildren();
      filterBar.appendChild(makeFilterBtn("all", copy.domainFilterAll));
      for (const domainId of ALL_CONTROL_CENTER_DOMAINS) {
        filterBar.appendChild(
          makeFilterBtn(domainId, DOMAIN_FILTER_LABELS[domainId][ctx.locale]),
        );
      }
    };

    const renderFeedback = (): void => {
      if (controlFeedbackText) {
        feedbackStrip.hidden = false;
        feedbackStrip.removeAttribute("hidden");
        feedbackStrip.textContent = controlFeedbackText;
      } else {
        feedbackStrip.hidden = true;
        feedbackStrip.setAttribute("hidden", "");
        feedbackStrip.textContent = "";
      }
    };

    const bodyHost = ctx.doc.createElement("div");
    bodyHost.className = "nlos-control-workstation-host";
    bodyHost.style.flex = "1 1 auto";
    bodyHost.style.overflow = "auto";
    bodyHost.style.padding = "8px 10px";

    const renderBody = (): void => {
      renderFilterButtons();
      renderFeedback();
      const currentState: SystemControlCenterState =
        controlCenterController.getState();
      bodyHost.innerHTML = renderControlCenterWorkstationHtml(currentState, {
        locale: ctx.locale,
        disclosure: ctx.disclosure,
        collapsedNodeIds,
        domainFilter: activeDomainFilter,
        rowLimit: 60,
      });
    };

    // Interactive event delegation on the 12-Domain Summary Grid, Ownership Tree & Cell Panel
    bodyHost.addEventListener("click", (ev: MouseEvent) => {
      const target =
        ev.target && typeof (ev.target as Element).closest === "function"
          ? (ev.target as Element)
          : null;
      if (!target) {
        return;
      }

      // 1) Action button click inside ownership tree row
      const verbBtn = target.closest("[data-verb][data-node-id]");
      if (verbBtn) {
        ev.stopPropagation();
        const verb = verbBtn.getAttribute("data-verb") ?? "";
        const nodeId = verbBtn.getAttribute("data-node-id") ?? "";
        const requiresArming =
          verbBtn.getAttribute("data-requires-arming") === "true";

        if (requiresArming) {
          compositor.openZ4Modal({
            promptId: `z4-ctrl-${nodeId}-${verb}`,
            title:
              ctx.locale === "en-US"
                ? `Confirm High-Impact Control: ${verb}`
                : `确认高影响控制动作：${verbBtn.textContent ?? verb}`,
            summary:
              ctx.locale === "en-US"
                ? `Target node ${nodeId} · Action requires explicit operator confirmation`
                : `目标对象 ${nodeId} · 该动作会改变运行边界，需显式授权执行`,
            onConfirm: () => {
              controlFeedbackText =
                ctx.locale === "en-US"
                  ? `Authorized and executed '${verb}' on ${nodeId}`
                  : `已通过安全确认并执行动作：${verbBtn.textContent ?? verb}`;
              renderBody();
            },
          });
          return;
        }

        if (verb === "SwitchPerformanceProfile") {
          const cur = controlCenterController.getState().performanceProfile;
          const next =
            cur === "BALANCED"
              ? "HIGH_PERFORMANCE"
              : cur === "HIGH_PERFORMANCE"
                ? "SILENT_ECO"
                : "BALANCED";
          controlCenterController.setPerformanceProfile(next);
        } else if (verb === "PauseBackgroundTier") {
          const cur = controlCenterController.getState().backgroundTierPaused;
          controlCenterController.setBackgroundTierPaused(!cur);
        }
        controlFeedbackText =
          ctx.locale === "en-US"
            ? `Executed '${verbBtn.textContent ?? verb}' on ${nodeId}`
            : `已执行控制动作：${verbBtn.textContent ?? verb}`;
        renderBody();
        return;
      }

      // 2) Domain summary card click -> toggle domain filter
      const summaryCard = target.closest("[data-summary-domain]");
      if (summaryCard) {
        const domain = summaryCard.getAttribute(
          "data-summary-domain",
        ) as ControlCenterDomainId | null;
        if (domain) {
          activeDomainFilter = activeDomainFilter === domain ? "all" : domain;
          renderBody();
        }
        return;
      }

      // 3) Ownership tree row click -> toggle collapse of children
      const treeRow = target.closest(".nlos-ownership-tree__row[data-node-id]");
      if (treeRow) {
        const nodeId = treeRow.getAttribute("data-node-id");
        if (nodeId) {
          if (collapsedNodeIds.has(nodeId)) {
            collapsedNodeIds.delete(nodeId);
          } else {
            collapsedNodeIds.add(nodeId);
          }
          renderBody();
        }
      }
    });

    liveRefreshControlPane = renderBody;
    renderBody();
    root.appendChild(filterBar);
    root.appendChild(feedbackStrip);
    root.appendChild(bodyHost);
    return root;
  };

  /* ==========================================================================
     2. Upper-Right Z2 Window: 5-Tab Orchestration & Governance Workbench
     ========================================================================== */

  const buildCleanOrthogonalDagElement = (ctx: {
    readonly doc: Document;
    readonly locale: WorkstationLocale;
    readonly disclosure: DisclosureLevel;
  }): HTMLElement => {
    const copy = SHELL_COPY[ctx.locale];
    const rows = taskSpaceController.getRows();
    const activeTaskId =
      compositor.getSnapshot().selectedTaskId ??
      taskSpaceController.getSelectedTaskId() ??
      rows[0]?.summary.taskId ??
      null;
    const selectedRow =
      rows.find((r) => r.summary.taskId === activeTaskId) ?? rows[0]!;

    const container = ctx.doc.createElement("div");
    container.className = "nlos-task-space-mount nlos-dag-workbench-view";

    // Top task switcher toolbar
    const toolbar = ctx.doc.createElement("div");
    toolbar.className = "nlos-workbench-toolbar";

    const leftGroup = ctx.doc.createElement("div");
    leftGroup.style.display = "flex";
    leftGroup.style.alignItems = "center";
    leftGroup.style.gap = "8px";
    leftGroup.style.flexWrap = "wrap";

    const labelSpan = ctx.doc.createElement("span");
    labelSpan.textContent = copy.taskSwitchLabel;
    leftGroup.appendChild(labelSpan);

    const selectEl = ctx.doc.createElement("select");
    selectEl.className = "nlos-workbench-select";
    selectEl.setAttribute("aria-label", copy.taskSwitchLabel);
    for (const r of rows) {
      const opt = ctx.doc.createElement("option");
      opt.setAttribute("value", r.summary.taskId);
      if (r.summary.taskId === selectedRow.summary.taskId) {
        opt.setAttribute("selected", "selected");
      }
      const titleText = ctx.locale === "en-US" ? r.goalTitleEn : r.goalTitleZh;
      opt.textContent = `${titleText} (${r.phaseFold.currentPhaseIndex}/${r.phaseFold.totalPhases})`;
      selectEl.appendChild(opt);
    }
    selectEl.addEventListener("change", (ev) => {
      const val = (ev.target as HTMLSelectElement | null)?.value;
      if (val) {
        compositor.disclosureModel.selectEntity(val, "task");
        liveRefreshWorkbenchPane?.();
      }
    });
    leftGroup.appendChild(selectEl);

    leftGroup.appendChild(
      createStatusIndicatorElement({
        state: selectedRow.summary.taskState,
        doc: ctx.doc,
        locale: ctx.locale,
        disclosure: ctx.disclosure,
      }),
    );

    toolbar.appendChild(leftGroup);
    container.appendChild(toolbar);

    // Stage Pipeline Card (Clean Zero-Jargon 4-Stage Orthogonal Flow in L0/L1/L2)
    const pipelineCard = ctx.doc.createElement("section");
    pipelineCard.className = "nlos-stage-pipeline-card";

    const pipeHeader = ctx.doc.createElement("div");
    pipeHeader.className = "nlos-stage-pipeline-header";
    const pipeTitle = ctx.doc.createElement("strong");
    pipeTitle.textContent =
      ctx.locale === "en-US"
        ? `${copy.stagePipelineHeading} — ${selectedRow.goalTitleEn}`
        : `${copy.stagePipelineHeading} — ${selectedRow.goalTitleZh}`;
    pipeHeader.appendChild(pipeTitle);

    pipeHeader.appendChild(
      createBulletMicroBarElement({
        actualUsage: selectedRow.summary.usageUnits,
        effectiveGrantedOrLeaseCap: selectedRow.summary.effectiveUnits,
        requestedCeiling: selectedRow.summary.requestedUnits,
        unit: "%",
        doc: ctx.doc,
        locale: ctx.locale,
        disclosure: ctx.disclosure,
      }),
    );
    pipelineCard.appendChild(pipeHeader);

    const stageLabelsZh = [
      "工作区变更与资源预留核对",
      "依赖契约与安全边界校验",
      "制品生成与并发执行收敛",
      "结果汇总与可验证凭证归档",
    ];
    const stageLabelsEn = [
      "Workspace & Resource Reservation Check",
      "Dependency Contract & Boundary Audit",
      "Artifact Generation & Parallel Convergence",
      "Outcome Summary & Receipt Archival",
    ];

    const nodesGrid = ctx.doc.createElement("div");
    nodesGrid.className = "nlos-stage-nodes-grid";
    const curPhase = selectedRow.phaseFold.currentPhaseIndex;

    for (let idx = 0; idx < 4; idx += 1) {
      const stepNum = idx + 1;
      const isDone =
        selectedRow.summary.taskState === "COMPLETED" || stepNum < curPhase;
      const isActive =
        selectedRow.summary.taskState !== "COMPLETED" && stepNum === curPhase;
      const box = ctx.doc.createElement("div");
      box.className = "nlos-stage-node-box";
      box.setAttribute(
        "data-stage-status",
        isDone ? "DONE" : isActive ? "ACTIVE" : "PENDING",
      );

      const stepTag = ctx.doc.createElement("span");
      stepTag.className = "nlos-stage-node-step";
      stepTag.textContent =
        ctx.locale === "en-US"
          ? `Stage ${stepNum} / 4 · ${isDone ? "Completed" : isActive ? "In Progress" : "Queued"}`
          : `阶段 ${stepNum} / 4 · ${isDone ? "已完成" : isActive ? "执行中" : "排队待执行"}`;

      const stepTitle = ctx.doc.createElement("span");
      stepTitle.className = "nlos-stage-node-title";
      stepTitle.textContent =
        ctx.locale === "en-US" ? stageLabelsEn[idx]! : stageLabelsZh[idx]!;

      box.appendChild(stepTag);
      box.appendChild(stepTitle);
      nodesGrid.appendChild(box);
    }
    pipelineCard.appendChild(nodesGrid);

    // 3 Metric Tiles: Owning Application, Target Completion, Reversibility Safeguard
    const metricsGrid = ctx.doc.createElement("div");
    metricsGrid.className = "nlos-stage-metrics-grid";

    const makeTile = (label: string, val: string): HTMLElement => {
      const tile = ctx.doc.createElement("div");
      tile.className = "nlos-stage-metric-tile";
      const l = ctx.doc.createElement("span");
      l.className = "nlos-stage-metric-label";
      l.textContent = label;
      const v = ctx.doc.createElement("span");
      v.className = "nlos-stage-metric-value";
      v.textContent = val;
      tile.appendChild(l);
      tile.appendChild(v);
      return tile;
    };

    metricsGrid.appendChild(
      makeTile(
        copy.stageMetricApp,
        ctx.locale === "en-US"
          ? selectedRow.applicationNameEn
          : selectedRow.applicationNameZh,
      ),
    );
    metricsGrid.appendChild(
      makeTile(
        copy.stageMetricCompletion,
        ctx.locale === "en-US"
          ? `${selectedRow.summary.succeededTargets} Succeeded / ${selectedRow.summary.failedTargets} Failed`
          : `${selectedRow.summary.succeededTargets} 项成功 / ${selectedRow.summary.failedTargets} 项失败`,
      ),
    );
    metricsGrid.appendChild(
      makeTile(
        copy.stageMetricUndo,
        selectedRow.summary.undoClass === "REVERSIBLE"
          ? ctx.locale === "en-US"
            ? "Lossless Undo Available"
            : "支持无损一键撤销"
          : selectedRow.summary.undoClass === "COMPENSATABLE"
            ? ctx.locale === "en-US"
              ? "Compensatable Snapshot Rollback"
              : "支持快照补偿回滚"
            : ctx.locale === "en-US"
              ? "Irreversible External Effect (Receipt Audited)"
              : "不可逆外部动作（已锁定操作凭证）",
      ),
    );
    pipelineCard.appendChild(metricsGrid);
    container.appendChild(pipelineCard);

    // In L3 Deep Diagnostics mode, append the full Dagster Orthogonal SVG DAG Editor,
    // Temporal Attempt Racing Swimlanes, and Cryptographic Receipt Audit Bar!
    if (ctx.disclosure === "L3") {
      const l3Wrap = ctx.doc.createElement("div");
      l3Wrap.className = "nlos-l3-only";
      const dagHtml = renderTaskPlanDagCanvasHtml({
        snapshot: selectedRow.l3PlanDag,
        locale: ctx.locale,
      });
      const lanesHtml = renderTaskAttemptSwimlanesHtml({
        snapshot: selectedRow.l3AttemptLanes,
        locale: ctx.locale,
      });
      const receiptHtml = renderReceiptAuditBarHtml({
        disclosure: "L3",
        controlCommandIdHex: selectedRow.summary.reservationId,
        receiptHex: selectedRow.summary.receiptHex,
        correlationIdHex: selectedRow.l3PlanDag.planDigestHex,
        cellId: selectedRow.summary.cellId,
        leaseEpoch: selectedRow.summary.planRevision,
        nodeBootGeneration: selectedRow.summary.installationGeneration,
        locale: ctx.locale,
      });
      l3Wrap.innerHTML = dagHtml + lanesHtml + receiptHtml;
      container.appendChild(l3Wrap);
    }

    return container;
  };

  const buildStoreCatalogElement = (ctx: {
    readonly doc: Document;
    readonly locale: WorkstationLocale;
    readonly disclosure: DisclosureLevel;
  }): HTMLElement => {
    const copy = SHELL_COPY[ctx.locale];
    const outer = ctx.doc.createElement("div");
    outer.className = "store-catalog-workbench";

    // Automatically fall back to Showcase mode if Live registry is empty/unavailable on first view
    const liveViewState = storeCatalogController.getSnapshot();
    const liveEmpty =
      liveViewState.snapshot === null ||
      (liveViewState.snapshot.officialStableReleases.length === 0 &&
        liveViewState.snapshot.communityCandidates.length === 0);
    const activeStoreCtrl =
      storeMode === "showcase" || liveEmpty
        ? showcaseStoreController
        : storeCatalogController;

    const toolbar = ctx.doc.createElement("div");
    toolbar.className = "nlos-workbench-toolbar";

    const modeGroup = ctx.doc.createElement("div");
    modeGroup.style.display = "flex";
    modeGroup.style.gap = "4px";

    const showcaseBtn = ctx.doc.createElement("button");
    showcaseBtn.type = "button";
    const usingShowcase = activeStoreCtrl === showcaseStoreController;
    showcaseBtn.className = usingShowcase ? "nlos-btn nlos-btn--primary" : "nlos-btn";
    showcaseBtn.setAttribute("data-store-mode", "showcase");
    showcaseBtn.textContent = copy.storeSourceShowcase;
    showcaseBtn.addEventListener("click", () => {
      storeMode = "showcase";
      void showcaseStoreController.refresh().then(() => {
        liveRefreshWorkbenchPane?.();
      });
    });

    const liveBtn = ctx.doc.createElement("button");
    liveBtn.type = "button";
    liveBtn.className = !usingShowcase ? "nlos-btn nlos-btn--primary" : "nlos-btn";
    liveBtn.setAttribute("data-store-mode", "live");
    liveBtn.textContent = copy.storeSourceLive;
    liveBtn.addEventListener("click", () => {
      storeMode = "live";
      void storeCatalogController.refresh().then(() => {
        liveRefreshWorkbenchPane?.();
      });
    });

    const refreshBtn = ctx.doc.createElement("button");
    refreshBtn.type = "button";
    refreshBtn.className = "nlos-btn";
    refreshBtn.textContent = copy.storeRefreshBtn;
    refreshBtn.addEventListener("click", () => {
      void activeStoreCtrl.refresh().then(() => {
        liveRefreshWorkbenchPane?.();
      });
    });

    modeGroup.appendChild(showcaseBtn);
    modeGroup.appendChild(liveBtn);
    toolbar.appendChild(modeGroup);
    toolbar.appendChild(refreshBtn);
    outer.appendChild(toolbar);

    const host = ctx.doc.createElement("div");
    host.className = "store-catalog-host";
    host.innerHTML = renderStoreCatalogHtml(activeStoreCtrl.getSnapshot());
    host.addEventListener("click", (event: MouseEvent) => {
      const target =
        event.target && typeof (event.target as Element).closest === "function"
          ? (event.target as Element)
          : null;
      if (target === null) {
        return;
      }
      const actionBtn = target.closest("[data-store-action-kind]");
      if (actionBtn !== null) {
        if (actionBtn.hasAttribute("disabled")) {
          return;
        }
        const kind = actionBtn.getAttribute("data-store-action-kind");
        if (kind !== null && kind.length > 0) {
          void activeStoreCtrl.requestSensitiveAction(kind).catch(() => undefined);
        }
        return;
      }
      const row = target.closest(".nlos-store-row[data-store-pkg][data-store-version]");
      if (row !== null) {
        const pkg = row.getAttribute("data-store-pkg");
        const ver = Number(row.getAttribute("data-store-version"));
        if (pkg !== null && pkg.length > 0 && Number.isFinite(ver)) {
          void activeStoreCtrl
            .selectRelease(pkg, ver)
            .then(() => {
              host.innerHTML = renderStoreCatalogHtml(activeStoreCtrl.getSnapshot());
            })
            .catch(() => undefined);
        }
      }
    });

    outer.appendChild(host);
    return outer;
  };

  const buildSettingsInteractiveElement = (ctx: {
    readonly doc: Document;
    readonly locale: WorkstationLocale;
    readonly disclosure: DisclosureLevel;
  }): HTMLElement => {
    const settingsWrap = ctx.doc.createElement("div");
    settingsWrap.className = "nlos-settings-host";

    const renderSettings = (): void => {
      settingsWrap.innerHTML = settingsSuiteController.renderSuiteHtml({
        activeLayer: activeSettingsLayer,
        locale: ctx.locale,
        disclosure: ctx.disclosure,
      });
    };

    settingsWrap.addEventListener("click", (ev: MouseEvent) => {
      const target =
        ev.target && typeof (ev.target as Element).closest === "function"
          ? (ev.target as Element)
          : null;
      if (!target) {
        return;
      }

      const layerBtn = target.closest("[data-settings-layer]");
      if (layerBtn) {
        const nextLayer = layerBtn.getAttribute(
          "data-settings-layer",
        ) as SettingsDomainLayer | null;
        if (nextLayer) {
          activeSettingsLayer = nextLayer;
          renderSettings();
        }
        return;
      }

      const approveBtn = target.closest('[data-action="approve-proposal"][data-content-type]');
      if (approveBtn) {
        const ct = approveBtn.getAttribute("data-content-type");
        if (ct) {
          const currentItem = settingsSuiteController.associations
            .listAssociations()
            .find((item) => item.contentTypeOrUri === ct);
          if (currentItem && currentItem.pendingProposal) {
            settingsSuiteController.associations.bindUserDefaultAssociation({
              contentTypeOrUri: ct,
              applicationId: currentItem.pendingProposal.proposedApplicationId,
              installationId: currentItem.pendingProposal.proposedInstallationId,
              handlerLabels: currentItem.pendingProposal.proposedLabels,
              expectedRevision: currentItem.revision,
            });
          }
          renderSettings();
        }
        return;
      }

      const revokeAssocBtn = target.closest('[data-action="revoke-association"][data-content-type]');
      if (revokeAssocBtn) {
        const ct = revokeAssocBtn.getAttribute("data-content-type");
        if (ct) {
          const rev = Number(revokeAssocBtn.getAttribute("data-revision") ?? "1");
          settingsSuiteController.associations.revokeAssociationToSystemChooser(
            ct,
            rev,
          );
          renderSettings();
        }
        return;
      }

      const revokeKeyBtn = target.closest('[data-action="revoke-keychain-ref"][data-keychain-ref]');
      if (revokeKeyBtn) {
        const refId = revokeKeyBtn.getAttribute("data-keychain-ref");
        if (refId) {
          const rev = Number(revokeKeyBtn.getAttribute("data-revision") ?? "1");
          settingsSuiteController.keychainVault.revokeKeychainReference(
            refId,
            rev,
          );
          renderSettings();
        }
      }
    });

    renderSettings();
    return settingsWrap;
  };

  const buildInteractiveSpotlightElement = (ctx: {
    readonly doc: Document;
    readonly locale: WorkstationLocale;
    readonly disclosure: DisclosureLevel;
  }): HTMLElement => {
    const copy = SHELL_COPY[ctx.locale];
    spotlightController.setLocale(ctx.locale);
    spotlightController.setDisclosureLevel(ctx.disclosure);

    const outer = ctx.doc.createElement("div");
    outer.className = "nlos-spotlight-workbench";

    const searchRow = ctx.doc.createElement("div");
    searchRow.className = "nlos-search-bar-row";

    const input = ctx.doc.createElement("input");
    input.setAttribute("type", "search");
    input.className = "nlos-text-input";
    input.setAttribute("placeholder", copy.searchInputPlaceholder);
    input.setAttribute("aria-label", copy.searchInputPlaceholder);

    const searchBtn = ctx.doc.createElement("button");
    searchBtn.type = "button";
    searchBtn.className = "nlos-btn nlos-btn--primary";
    searchBtn.textContent = copy.searchBtnLabel;

    const resultsHost = ctx.doc.createElement("div");
    resultsHost.className = "nlos-spotlight-host";

    const renderResults = (queryText?: string, category?: SpotlightCandidateCategory): void => {
      if (queryText !== undefined) {
        spotlightController.querySpotlightCandidates({
          queryText,
          categoryFilter: category,
          limit: 20,
        });
      }
      resultsHost.innerHTML = renderSpotlightViewHtml(spotlightController.getState());
    };

    searchBtn.addEventListener("click", () => {
      const val = (input as unknown as { value?: string }).value ?? "";
      renderResults(val);
    });

    input.addEventListener("input", (ev) => {
      const val = (ev.target as { value?: string } | null)?.value ?? "";
      renderResults(val);
    });

    renderResults();
    searchRow.appendChild(input);
    searchRow.appendChild(searchBtn);
    outer.appendChild(searchRow);
    outer.appendChild(resultsHost);
    return outer;
  };

  const buildWorkbenchSurface = (ctx: {
    readonly doc: Document;
    readonly locale: WorkstationLocale;
    readonly disclosure: DisclosureLevel;
  }): HTMLElement => {
    const copy = SHELL_COPY[ctx.locale];
    const root = ctx.doc.createElement("div");
    root.className = "nlos-workstation-workbench";
    root.style.display = "flex";
    root.style.flexDirection = "column";
    root.style.height = "100%";

    const tabBar = ctx.doc.createElement("nav");
    tabBar.className = "nlos-workbench-tabs";
    tabBar.setAttribute("role", "tablist");
    tabBar.style.display = "flex";
    tabBar.style.flexWrap = "wrap";
    tabBar.style.gap = "4px";
    tabBar.style.padding = "6px 10px";
    tabBar.style.borderBottom = "1px solid var(--nlos-border-hairline)";
    tabBar.style.backgroundColor = "var(--nlos-bg-chrome)";

    const tabs: Array<{ id: WorkbenchTabId; label: string }> = [
      { id: "dag", label: copy.tabDag },
      { id: "store", label: copy.tabStore },
      { id: "settings", label: copy.tabSettings },
      { id: "search", label: copy.tabSearch },
    ];
    if (options.legacyDiagnosticsElement) {
      tabs.push({ id: "probes", label: copy.tabProbes });
    }

    const contentHost = ctx.doc.createElement("div");
    contentHost.className = "nlos-workbench-body";
    contentHost.style.flex = "1 1 auto";
    contentHost.style.overflow = "auto";
    contentHost.style.padding = "8px 10px";

    const renderActiveTabContent = (): void => {
      if (activeWorkbenchTab === "dag") {
        contentHost.replaceChildren(buildCleanOrthogonalDagElement(ctx));
      } else if (activeWorkbenchTab === "store") {
        contentHost.replaceChildren(buildStoreCatalogElement(ctx));
      } else if (activeWorkbenchTab === "settings") {
        contentHost.replaceChildren(buildSettingsInteractiveElement(ctx));
      } else if (activeWorkbenchTab === "probes" && options.legacyDiagnosticsElement) {
        contentHost.replaceChildren(options.legacyDiagnosticsElement);
      } else {
        contentHost.replaceChildren(buildInteractiveSpotlightElement(ctx));
      }
    };

    const renderTabButtons = (): void => {
      tabBar.replaceChildren();
      for (const tab of tabs) {
        const btn = ctx.doc.createElement("button");
        btn.type = "button";
        const isSelected = activeWorkbenchTab === tab.id;
        btn.className = isSelected ? "nlos-btn nlos-btn--primary" : "nlos-btn";
        btn.setAttribute("role", "tab");
        btn.setAttribute("data-workbench-tab", tab.id);
        btn.setAttribute("aria-selected", isSelected ? "true" : "false");
        btn.textContent = tab.label;
        btn.addEventListener("click", () => {
          selectWorkbenchTab(tab.id);
        });
        tabBar.appendChild(btn);
      }
    };

    const renderAll = (): void => {
      renderTabButtons();
      renderActiveTabContent();
    };

    liveRefreshWorkbenchPane = renderAll;
    renderAll();
    root.appendChild(tabBar);
    root.appendChild(contentHost);
    return root;
  };

  // Register the 2 default first-party industrial workstation windows into Z2
  const initialCopy = SHELL_COPY[initialSnap.locale];

  compositor.windowManager.registerWindow({
    windowId: WORKSTATION_CONTROL_WINDOW_ID,
    surfaceId: "surf-system-control-center",
    applicationIdHex: SYSTEM_CONTROL_APP_ID_HEX,
    title: initialCopy.controlWindowTitle,
    appIcon: "layout-3-pane",
    surfaceGeneration: 1,
    latestInstallationGeneration: 1,
    isFloating: false,
    presentImmediately: true,
    trustedSurfaceFactory: buildControlCenterSurface,
  });

  compositor.windowManager.registerWindow({
    windowId: WORKSTATION_WORKBENCH_WINDOW_ID,
    surfaceId: "surf-workstation-workbench",
    applicationIdHex: SYSTEM_WORKBENCH_APP_ID_HEX,
    title: initialCopy.workbenchWindowTitle,
    appIcon: "layout-2-col",
    surfaceGeneration: 1,
    latestInstallationGeneration: 1,
    isFloating: false,
    presentImmediately: true,
    trustedSurfaceFactory: buildWorkbenchSurface,
  });

  compositor.windowManager.focusWindow(WORKSTATION_CONTROL_WINDOW_ID);

  const focusOrRestoreControlWindow = (): void => {
    const wmSnap = compositor.windowManager.getSnapshot();
    const existing = wmSnap.windows.find(
      (w) => w.windowId === WORKSTATION_CONTROL_WINDOW_ID && w.windowState !== "CLOSED",
    );
    if (!existing) {
      const loc = compositor.getSnapshot().locale;
      compositor.windowManager.registerWindow({
        windowId: WORKSTATION_CONTROL_WINDOW_ID,
        surfaceId: "surf-system-control-center",
        applicationIdHex: SYSTEM_CONTROL_APP_ID_HEX,
        title: SHELL_COPY[loc].controlWindowTitle,
        appIcon: "layout-3-pane",
        surfaceGeneration: 1,
        latestInstallationGeneration: 1,
        isFloating: false,
        presentImmediately: true,
        trustedSurfaceFactory: buildControlCenterSurface,
      });
    } else if (existing.windowState === "MINIMIZED") {
      compositor.windowManager.restoreWindow(WORKSTATION_CONTROL_WINDOW_ID);
    } else {
      compositor.windowManager.focusWindow(WORKSTATION_CONTROL_WINDOW_ID);
    }
  };

  const focusOrRestoreWorkbenchWindow = (tab?: WorkbenchTabId): void => {
    if (tab) {
      activeWorkbenchTab = tab;
    }
    const wmSnap = compositor.windowManager.getSnapshot();
    const existing = wmSnap.windows.find(
      (w) => w.windowId === WORKSTATION_WORKBENCH_WINDOW_ID && w.windowState !== "CLOSED",
    );
    if (!existing) {
      const loc = compositor.getSnapshot().locale;
      compositor.windowManager.registerWindow({
        windowId: WORKSTATION_WORKBENCH_WINDOW_ID,
        surfaceId: "surf-workstation-workbench",
        applicationIdHex: SYSTEM_WORKBENCH_APP_ID_HEX,
        title: SHELL_COPY[loc].workbenchWindowTitle,
        appIcon: "layout-2-col",
        surfaceGeneration: 1,
        latestInstallationGeneration: 1,
        isFloating: false,
        presentImmediately: true,
        trustedSurfaceFactory: buildWorkbenchSurface,
      });
    } else if (existing.windowState === "MINIMIZED") {
      compositor.windowManager.restoreWindow(WORKSTATION_WORKBENCH_WINDOW_ID);
    } else {
      compositor.windowManager.focusWindow(WORKSTATION_WORKBENCH_WINDOW_ID);
    }
  };

  // Z3 System Flyout Synchronization (CONNECTIVITY / NOTIFICATIONS / CONTROL_CENTER / SPOTLIGHT_SEARCH)
  let syncingFlyout = false;
  let lastFlyoutSignature = "";

  const buildZ3FlyoutDom = (
    activeFlyoutId: string,
    locale: WorkstationLocale,
    disclosure: DisclosureLevel,
  ): HTMLElement => {
    const doc = options.doc ?? document;
    const wrapper = doc.createElement("div");
    wrapper.className = "nlos-z3-flyout-host";
    wrapper.setAttribute("data-active-z3-flyout", activeFlyoutId);
    wrapper.style.position = "absolute";
    wrapper.style.right = "12px";
    wrapper.style.bottom = "44px";
    wrapper.style.maxWidth = "440px";
    wrapper.style.maxHeight = "calc(100vh - 72px)";
    wrapper.style.overflow = "auto";

    if (activeFlyoutId === "CONNECTIVITY") {
      clusterPostureController.setLocale(locale);
      clusterPostureController.setDisclosureLevel(disclosure);
      wrapper.appendChild(
        createClusterPostureTrayElement(clusterPostureController, doc),
      );
    } else if (activeFlyoutId === "NOTIFICATIONS") {
      notificationController.setLocale(locale);
      notificationController.setDisclosureLevel(disclosure);
      wrapper.appendChild(
        createNotificationCenterElement(notificationController, doc),
      );
    } else if (activeFlyoutId === "SPOTLIGHT_SEARCH") {
      wrapper.appendChild(
        buildInteractiveSpotlightElement({ doc, locale, disclosure }),
      );
    } else if (activeFlyoutId === "CONTROL_CENTER") {
      const flyoutBox = doc.createElement("div");
      flyoutBox.className = "nlos-control-center-flyout-mount";
      flyoutBox.innerHTML = renderSystemControlCenterFlyoutHtml(
        controlCenterController.getState(),
        {
          locale,
          disclosure,
        },
      );
      flyoutBox.addEventListener("click", (ev: MouseEvent) => {
        const target =
          ev.target && typeof (ev.target as Element).closest === "function"
            ? (ev.target as Element)
            : null;
        const actionEl = target?.closest("[data-action]");
        const action = actionEl?.getAttribute("data-action");
        if (action === "lock-session") {
          compositor.setZ3FlyoutOpen("CONTROL_CENTER", false);
          compositor.lockSession();
        } else if (
          action === "open-task-manager" ||
          action === "open-resource-monitor"
        ) {
          compositor.setZ3FlyoutOpen("CONTROL_CENTER", false);
          focusOrRestoreControlWindow();
        }
      });
      wrapper.appendChild(flyoutBox);
    }
    return wrapper;
  };

  compositor.subscribe((snap) => {
    if (syncingFlyout) {
      return;
    }
    syncingFlyout = true;
    try {
      const copy = SHELL_COPY[snap.locale];
      compositor.windowManager.setWindowTitle(
        WORKSTATION_CONTROL_WINDOW_ID,
        copy.controlWindowTitle,
      );
      compositor.windowManager.setWindowTitle(
        WORKSTATION_WORKBENCH_WINDOW_ID,
        copy.workbenchWindowTitle,
      );

      showcaseStoreController.setLocale(snap.locale);
      showcaseStoreController.setDisclosure(snap.disclosure);

      const unread = notificationController.getState().currentPage.totalUnreadCount;
      compositor.setUnreadNotificationCount(unread);

      const activeFlyoutId = snap.openZ3FlyoutIds[0] ?? "";
      const nextSignature = `${activeFlyoutId}|${snap.locale}|${snap.disclosure}|${controlCenterController.getState().revision}`;
      if (nextSignature !== lastFlyoutSignature) {
        lastFlyoutSignature = nextSignature;
        if (activeFlyoutId.length > 0) {
          const flyoutEl = buildZ3FlyoutDom(
            activeFlyoutId,
            snap.locale,
            snap.disclosure,
          );
          compositor.setSystemFlyoutElement(flyoutEl);
        } else {
          compositor.setSystemFlyoutElement(null);
        }
      }
    } finally {
      syncingFlyout = false;
    }
  });

  storeCatalogController.subscribe(() => {
    if (activeWorkbenchTab === "store") {
      liveRefreshWorkbenchPane?.();
    }
  });

  showcaseStoreController.subscribe(() => {
    if (activeWorkbenchTab === "store") {
      liveRefreshWorkbenchPane?.();
    }
  });

  const selectWorkbenchTab = (tab: WorkbenchTabId): void => {
    activeWorkbenchTab = tab;
    if (tab === "store") {
      void storeCatalogController.refresh().catch(() => undefined);
      void showcaseStoreController.refresh().catch(() => undefined);
    }
    focusOrRestoreWorkbenchWindow(tab);
    liveRefreshWorkbenchPane?.();
  };

  const setDomainFilter = (domain: ControlCenterDomainId | "all"): void => {
    activeDomainFilter = domain;
    focusOrRestoreControlWindow();
    liveRefreshControlPane?.();
  };

  return {
    selectWorkbenchTab,
    getActiveWorkbenchTab: () => activeWorkbenchTab,
    setDomainFilter,
    focusOrRestoreControlWindow,
    focusOrRestoreWorkbenchWindow,
  };
}
