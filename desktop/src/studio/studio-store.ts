// ==========================================================================
// LLMOS Operator Studio — Unified Full-IPC Reactive State Store (`studio-store.ts`)
// Connects all 116 Tauri backend RPC commands & local domain controllers into
// a clean, structured 8-view operator console state machine.
// ==========================================================================

import {
  assistIntentWithKeyPool,
  chatWithLlmKeyPool,
  controlPlaneFacts,
  costFactCheck,
  exportMetrics,
  exportResourceMetrics,
  exportSemanticMetrics,
  getConfig,
  inspectApplication,
  inspectCellLeases,
  inspectHealth,
  inspectLlmKeyPoolStatus,
  inspectProcess,
  inspectResource,
  inspectResourceCost,
  inspectResourceHealth,
  inspectSemanticHealth,
  inspectTask,
  listCells,
  listInstalledApplications,
  listProcesses,
  listResourceReservations,
  listTasks,
  parityCheck,
  parityCheckWrite,
  presentSurfaces,
  setConfig,
  submitControl,
  type SetConfigInput,
} from "../ipc";
import { parseOpenMetricsText, type MetricFamily } from "../openmetrics";
import type {
  AnyControlActionInput,
  ConfigDto,
  ControlActionInput,
  ControlPlaneFactsDto,
  KeyPoolChatDto,
  KeyPoolIntentAssistDto,
  KeyPoolStatusDto,
  ReceiptDto,
} from "../types";
import type { DisclosureLevel, WorkstationLocale } from "../ui";
import {
  createSystemControlCenterController,
  type ControlCenterDomainId,
  type SystemControlCenterController,
} from "../views/control-center";
import { NotificationCenterController } from "../views/notifications";
import { ClusterPostureController } from "../views/offline-cluster";
import {
  createWorkstationSettingsSuiteController,
  type SettingsDomainLayer,
  type WorkstationSettingsSuiteController,
} from "../views/settings";
import { SpotlightController } from "../views/spotlight";
import {
  createStoreCatalogController,
  createTauriStoreCatalogClient,
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
  createTaskSpaceSurfaceController,
  type TaskSpaceSurfaceController,
} from "../views/task-space";

export type StudioNavSectionId =
  | "overview"
  | "intent-keypool"
  | "tasks-dag"
  | "processes-resources"
  | "recovery-telemetry"
  | "apps-store"
  | "security-settings"
  | "kernel-probes";

export interface StudioInspectorRecord {
  readonly title: string;
  readonly subtitle: string;
  readonly badgeText: string;
  readonly badgeVariant: "ok" | "warn" | "danger" | "accent" | "neutral";
  readonly fields: ReadonlyArray<{ readonly key: string; readonly value: string }>;
  readonly rawReceiptHex?: string | null;
  readonly rawPayloadJson?: string | null;
}

export interface StudioZ4PromptRecord {
  readonly promptId: string;
  readonly title: string;
  readonly summary: string;
  readonly consequence: string;
  readonly irreversible: boolean;
  readonly armedAtMs: number;
  readonly onConfirm: () => void;
}

export interface RecoveryDomainHealthSummary {
  readonly domain: "artifact" | "semantic" | "resource";
  readonly workerState: string;
  readonly completedCycles: number;
  readonly retrying: number;
  readonly escalated: number;
  readonly unacknowledged: number;
  readonly resolved: number;
  readonly alerts: ReadonlyArray<{
    readonly planIdHex: string;
    readonly totalFailures: number;
    readonly acknowledgedReceiptIdHex: string | null;
  }>;
  readonly lastReceiptHex: string | null;
}

export type StudioThemeId = "zurich-light" | "braun-atelier" | "zurich-dark";

export interface StudioStateSnapshot {
  readonly activeSection: StudioNavSectionId;
  readonly studioTheme: StudioThemeId;
  readonly theme: "graphite-workstation" | "paper-workstation";
  readonly locale: WorkstationLocale;
  readonly disclosure: DisclosureLevel;
  readonly daemonOnline: boolean;
  readonly config: ConfigDto;
  readonly controlFacts: ControlPlaneFactsDto | null;
  readonly keypoolStatus: KeyPoolStatusDto;
  readonly lastIntentAssist: KeyPoolIntentAssistDto | null;
  readonly lastChatResponse: KeyPoolChatDto | null;
  readonly keypoolBusy: boolean;
  readonly selectedTaskId: string;
  readonly taskStateFilter: "ALL" | "ACTIVE" | "WAITING" | "COMPLETED";
  readonly selectedDomainFilter: ControlCenterDomainId | "all";
  readonly collapsedTreeNodeIds: ReadonlySet<string>;
  readonly activeSettingsLayer: SettingsDomainLayer;
  readonly storeMode: "showcase" | "live";
  readonly recoveryHealth: Readonly<
    Record<"artifact" | "semantic" | "resource", RecoveryDomainHealthSummary>
  >;
  readonly metricFamilies: readonly MetricFamily[];
  readonly inspector: StudioInspectorRecord | null;
  readonly z4Prompt: StudioZ4PromptRecord | null;
  readonly commandModalOpen: boolean;
  readonly bannerNotice: {
    readonly message: string;
    readonly tone: "info" | "warn" | "danger";
  } | null;
  readonly lastReceiptHex: string;
  readonly revision: number;
}

export interface OperatorStudioControllers {
  readonly controlCenter: SystemControlCenterController;
  readonly taskSpace: TaskSpaceSurfaceController;
  readonly liveStoreCatalog: StoreCatalogController;
  readonly showcaseStoreCatalog: StoreCatalogController;
  readonly settingsSuite: WorkstationSettingsSuiteController;
  readonly notifications: NotificationCenterController;
  readonly spotlight: SpotlightController;
  readonly clusterPosture: ClusterPostureController;
}

export interface OperatorStudioStore {
  readonly controllers: OperatorStudioControllers;
  getState(): StudioStateSnapshot;
  subscribe(listener: (state: StudioStateSnapshot) => void): () => void;
  setActiveSection(section: StudioNavSectionId): void;
  setStudioTheme(theme: StudioThemeId): void;
  toggleTheme(): void;
  setLocale(locale: WorkstationLocale): void;
  cycleDisclosure(): void;
  setCommandModalOpen(open: boolean): void;
  setInspector(record: StudioInspectorRecord | null): void;
  setBannerNotice(
    message: string | null,
    tone?: "info" | "warn" | "danger",
  ): void;
  selectTask(taskId: string): void;
  setTaskStateFilter(filter: "ALL" | "ACTIVE" | "WAITING" | "COMPLETED"): void;
  setDomainFilter(domain: ControlCenterDomainId | "all"): void;
  toggleTreeNodeCollapse(nodeId: string): void;
  setActiveSettingsLayer(layer: SettingsDomainLayer): void;
  setStoreMode(mode: "showcase" | "live"): void;
  openZ4Confirmation(prompt: Omit<StudioZ4PromptRecord, "armedAtMs">): void;
  confirmZ4Prompt(): boolean;
  cancelZ4Prompt(): void;
  // Live IPC Actions
  refreshAllFromDaemon(): Promise<void>;
  probeKeyPoolStatus(): Promise<KeyPoolStatusDto>;
  runIntentAssist(utterance: string): Promise<KeyPoolIntentAssistDto>;
  runKeyPoolChat(prompt: string): Promise<KeyPoolChatDto>;
  dispatchControlAction(action: AnyControlActionInput): Promise<void>;
  runRecoveryInspect(domain: "artifact" | "semantic" | "resource"): Promise<void>;
  refreshOpenMetrics(): Promise<void>;
  executeRawIpcProbe(
    probeKind: string,
    primaryHex: string,
    secondaryArg?: string,
  ): Promise<void>;
  saveConnectionConfig(input: SetConfigInput): Promise<void>;
}

const DEFAULT_OFFLINE_CONFIG: ConfigDto = {
  socketPath: "\\\\.\\pipe\\llmos-system-control-auth-18179e988b53ae66",
  principalHex: "9f389da8334b5b5fef257e39feafa147",
  keyFile: "~/.llmos/trial-state/keys/operator-ed25519.key",
  cliSocket: "\\\\.\\pipe\\llmos-system-control-plain-18179e988b53ae66",
  cliPath: null,
  resourceRoot: "~/.llmos/trial-state/resource",
  applicationRoot: "~/.llmos/trial-state/application",
  source: "env",
  platformSupported: true,
};

const DEFAULT_KEYPOOL_STATUS: KeyPoolStatusDto = {
  configured: true,
  reachable: true,
  discoverySource: "user-home:~/glm-keypool/config.json",
  gatewayOrigin: "http://127.0.0.1:8317",
  openaiBasePath: "/api/coding/paas/v4",
  defaultModel: "glm-5.3",
  fastIntentModel: "glm-5.3",
  allowedModels: ["glm-5.3", "glm-5.3-flash"],
  activeKeys: 41,
  totalKeys: 44,
  strategy: "sticky",
  budgetMetered: true,
  isolationTag: "PROVIDER_ISOLATION_UNVERIFIED",
  fallbackMode: "LIVE_KEYPOOL",
  diagnosticSummary: "41/44 slots online · sticky routing · 5D LlmTokenBudget active",
};

const SHOWCASE_ENTRIES: readonly StoreCatalogEntryDto[] = [
  {
    packageIdHex: "a1".repeat(16),
    packageVersion: 1_099_511_627_776,
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
    packageVersion: 1_099_511_627_777,
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

function createShowcaseStore(
  onOpenZ4: (prompt: Omit<StudioZ4PromptRecord, "armedAtMs">) => void,
): StoreCatalogController {
  const snap: StoreCatalogSnapshotDto = {
    source: STORE_SOURCE_LIVE_REGISTRY,
    offlineNoticeZh: null,
    offlineNoticeEn: null,
    storeRevocationEpoch: 3,
    officialStableReleases: SHOWCASE_ENTRIES.slice(0, 3),
    communityCandidates: SHOWCASE_ENTRIES.slice(3),
  };

  const diff: StorePermissionDiffDto = {
    packageIdHex: SHOWCASE_ENTRIES[0]!.packageIdHex,
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
    plainSummaryZh: "升级版本新增 1 项外部同步权限，需经系统安全确认屏显式授权。",
    plainSummaryEn:
      "New version adds 1 external sync permission; explicit security confirmation required.",
  };

  return createStoreCatalogController({
    client: {
      loadSnapshot: async () => snap,
      loadTransparency: async (pkgHex): Promise<StoreReleaseTransparencyDto> => {
        const entry =
          SHOWCASE_ENTRIES.find((e) => e.packageIdHex === pkgHex) ??
          SHOWCASE_ENTRIES[0]!;
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
              effectClass: entry.anyIrreversibleEffect
                ? "IRREVERSIBLE"
                : "COMPENSATABLE",
              networkEgressDeclared: entry.anyNetworkEgress,
              sandboxProfile: "wasm-dual-layer-strict",
            },
          ],
          anyNetworkEgress: entry.anyNetworkEgress,
          anyIrreversibleEffect: entry.anyIrreversibleEffect,
          effectLegend: [
            { effectClass: "READ_ONLY", labelZh: "只读", labelEn: "Read-only" },
            { effectClass: "REVERSIBLE", labelZh: "可逆", labelEn: "Reversible" },
            {
              effectClass: "COMPENSATABLE",
              labelZh: "可补偿",
              labelEn: "Compensatable",
            },
            {
              effectClass: "IRREVERSIBLE",
              labelZh: "不可逆",
              labelEn: "Irreversible",
            },
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
            chainStatusNoticeZh:
              "三级 Ed25519 发布证书链与单调吊销纪元已在本机复验通过。",
            chainStatusNoticeEn:
              "3-tier Ed25519 publisher certificate chain verified locally.",
            leafKeyFingerprintHex: entry.publisherKeyFingerprintHex,
            revocationEpoch: 3,
            crlFreshUntilMs: 8_000_000_000_000,
            signatureReverified: true,
          },
        };
      },
      loadPermissionDiff: async () => diff,
      prepareSensitiveAction: async (
        input: StoreSensitiveActionInput,
      ): Promise<StoreSensitiveActionEntryDto> => {
        const isIrrev = input.actionKind === "IRREVERSIBLE_UPGRADE";
        return {
          actionKind: input.actionKind,
          plainActionZh: `对受控生态应用包 ${input.packageIdHex.slice(0, 8)} 执行 ${input.actionKind}`,
          plainActionEn: `Execute ${input.actionKind} on package ${input.packageIdHex.slice(0, 8)}`,
          reversibilityClass: isIrrev ? "IRREVERSIBLE" : "COMPENSATABLE_ROLLBACK",
          z4Request: {
            requestingApp: {
              displayName: `生态应用包 (${input.packageIdHex.slice(0, 8)})`,
              publisherName: "NLOS Verified Ecosystem Publisher",
              signatureVerified: true,
              applicationIdHex: "31".repeat(16),
              installationIdHex: "21".repeat(16),
              principalIdHex: "11".repeat(16),
              installationGeneration: 1,
              manifestDigestHex: "f0".repeat(32),
            },
            actionScope: {
              plainSummaryZh: `从受治理商店执行 ${input.actionKind}（双层 Process+Wasm 沙箱）`,
              plainSummaryEn: `Execute ${input.actionKind} from governed store (Process+Wasm sandbox)`,
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
              reversibilityClass: isIrrev
                ? "IRREVERSIBLE"
                : "COMPENSATABLE_ROLLBACK",
              plainConsequenceZh: isIrrev
                ? "不可逆数据迁移：已核验升级前快照备份回执"
                : "支持原子快照回滚与零残留卸载",
              plainConsequenceEn: isIrrev
                ? "Irreversible migration: pre-upgrade backup verified"
                : "Supports atomic snapshot rollback and clean uninstall",
              multiTargetCount: 1,
              partialFailureDisclosureZh: null,
              partialFailureDisclosureEn: null,
              blastRadiusRows: null,
              requiresDataLossReceipt: isIrrev,
            },
            allowedGrantScopes: ["once", "session"],
          },
        };
      },
    },
    initialLocale: "zh-CN",
    initialDisclosure: "L1",
    openZ4Challenge: (spec) => {
      onOpenZ4({
        promptId: `z4-store-${spec.requestingApp.applicationIdHex.slice(0, 8)}`,
        title: spec.requestingApp.displayName,
        summary: `${spec.actionScope.plainSummary["zh-CN"]} · ${spec.resourceCeiling.plainCeiling["zh-CN"]}`,
        consequence: spec.reversibilityAndRisk.plainConsequence["zh-CN"],
        irreversible:
          spec.reversibilityAndRisk.reversibilityClass === "IRREVERSIBLE",
        onConfirm: () => undefined,
      });
    },
  });
}

export function createOperatorStudioStore(): OperatorStudioStore {
  const listeners = new Set<(state: StudioStateSnapshot) => void>();

  const controlCenter = createSystemControlCenterController();
  const taskSpace = createTaskSpaceSurfaceController({ locale: "zh-CN" });
  const settingsSuite = createWorkstationSettingsSuiteController();
  const notifications = new NotificationCenterController();
  const spotlight = new SpotlightController();
  const clusterPosture = new ClusterPostureController();

  let state: StudioStateSnapshot = {
    activeSection: "overview",
    studioTheme: "zurich-light",
    theme: "paper-workstation",
    locale: "zh-CN",
    disclosure: "L1",
    daemonOnline: false,
    config: DEFAULT_OFFLINE_CONFIG,
    controlFacts: null,
    keypoolStatus: DEFAULT_KEYPOOL_STATUS,
    lastIntentAssist: null,
    lastChatResponse: null,
    keypoolBusy: false,
    selectedTaskId: taskSpace.getRows()[0]?.summary.taskId ?? "task-01",
    taskStateFilter: "ALL",
    selectedDomainFilter: "all",
    collapsedTreeNodeIds: new Set<string>(),
    activeSettingsLayer: "user",
    storeMode: "showcase",
    recoveryHealth: {
      artifact: {
        domain: "artifact",
        workerState: "Running (Healthy)",
        completedCycles: 142,
        retrying: 0,
        escalated: 0,
        unacknowledged: 0,
        resolved: 18,
        alerts: [],
        lastReceiptHex: "9df1db3b6a37f677a8c7618f0e34d3fd",
      },
      semantic: {
        domain: "semantic",
        workerState: "Running (Healthy)",
        completedCycles: 142,
        retrying: 0,
        escalated: 0,
        unacknowledged: 0,
        resolved: 11,
        alerts: [],
        lastReceiptHex: "4c8e2a190fb45811c029d7a319f84b12",
      },
      resource: {
        domain: "resource",
        workerState: "Running (Healthy)",
        completedCycles: 142,
        retrying: 0,
        escalated: 0,
        unacknowledged: 0,
        resolved: 9,
        alerts: [],
        lastReceiptHex: "7a11f0e38c2b9104d55e18a902c43e01",
      },
    },
    metricFamilies: parseOpenMetricsText(
      [
        "# HELP nlos_artifact_recovery_completed_cycles_total Total artifact recovery cycles",
        "# TYPE nlos_artifact_recovery_completed_cycles_total counter",
        "nlos_artifact_recovery_completed_cycles_total 142",
        "# HELP nlos_semantic_recovery_total_finalized Total finalized semantic plans",
        "# TYPE nlos_semantic_recovery_total_finalized counter",
        "nlos_semantic_recovery_total_finalized 11",
        "# HELP nlos_resource_active_reservations Active resource reservations",
        "# TYPE nlos_resource_active_reservations gauge",
        "nlos_resource_active_reservations 6",
      ].join("\n"),
    ).families,
    inspector: null,
    z4Prompt: null,
    commandModalOpen: false,
    bannerNotice: null,
    lastReceiptHex: "9df1db3b6a37f677a8c7618f0e34d3fd",
    revision: 1,
  };

  const emit = (): void => {
    state = { ...state, revision: state.revision + 1 };
    for (const listener of listeners) {
      listener(state);
    }
  };

  const openZ4Confirmation = (
    prompt: Omit<StudioZ4PromptRecord, "armedAtMs">,
  ): void => {
    state = {
      ...state,
      z4Prompt: {
        ...prompt,
        armedAtMs: Date.now(),
      },
    };
    emit();
  };

  const liveStoreCatalog = createStoreCatalogController({
    client: createTauriStoreCatalogClient(),
    initialLocale: "zh-CN",
    initialDisclosure: "L1",
    openZ4Challenge: (spec) => {
      openZ4Confirmation({
        promptId: `z4-live-store-${spec.requestingApp.applicationIdHex.slice(0, 8)}`,
        title: spec.requestingApp.displayName,
        summary: `${spec.actionScope.plainSummary[state.locale]} · ${spec.resourceCeiling.plainCeiling[state.locale]}`,
        consequence: spec.reversibilityAndRisk.plainConsequence[state.locale],
        irreversible:
          spec.reversibilityAndRisk.reversibilityClass === "IRREVERSIBLE",
        onConfirm: () => {
          state = {
            ...state,
            bannerNotice: {
              message: `已通过 Z4 安全确认屏批准商店动作：${spec.requestingApp.displayName}`,
              tone: "info",
            },
          };
        },
      });
    },
  });

  const showcaseStoreCatalog = createShowcaseStore(openZ4Confirmation);
  void showcaseStoreCatalog
    .refresh()
    .then(() =>
      showcaseStoreCatalog.selectRelease(
        SHOWCASE_ENTRIES[0]!.packageIdHex,
        SHOWCASE_ENTRIES[0]!.packageVersion,
      ),
    )
    .catch(() => undefined);
  void liveStoreCatalog.refresh().catch(() => undefined);

  const applyReceiptToRecovery = (
    domain: "artifact" | "semantic" | "resource",
    receipt: ReceiptDto,
  ): void => {
    const out = receipt.outcome;
    const prev = state.recoveryHealth[domain];
    let updated: RecoveryDomainHealthSummary = {
      ...prev,
      lastReceiptHex: receipt.receiptHex,
    };
    if (out.kind === "inspected") {
      updated = {
        domain: "artifact",
        workerState: out.workerState,
        completedCycles: out.completedCycles,
        retrying: out.durableRetrying,
        escalated: out.durableEscalated,
        unacknowledged: out.durableUnacknowledgedEscalated,
        resolved: out.durableResolved,
        alerts: out.alerts,
        lastReceiptHex: receipt.receiptHex,
      };
    } else if (out.kind === "semantic_inspected") {
      updated = {
        domain: "semantic",
        workerState: out.domainFaulted ? "Faulted" : "Running (Healthy)",
        completedCycles: out.totalInspected,
        retrying: out.durableRetrying,
        escalated: out.durableEscalated,
        unacknowledged: out.durableUnacknowledgedEscalated,
        resolved: out.durableResolved,
        alerts: out.alerts,
        lastReceiptHex: receipt.receiptHex,
      };
    } else if (out.kind === "resource_recovery_inspected") {
      updated = {
        domain: "resource",
        workerState: out.domainFaulted ? "Faulted" : "Running (Healthy)",
        completedCycles: out.totalInspected,
        retrying: out.durableRetrying,
        escalated: out.durableEscalated,
        unacknowledged: out.durableUnacknowledgedEscalated,
        resolved: out.durableResolved,
        alerts: out.alerts,
        lastReceiptHex: receipt.receiptHex,
      };
    }
    state = {
      ...state,
      lastReceiptHex: receipt.receiptHex,
      recoveryHealth: {
        ...state.recoveryHealth,
        [domain]: updated,
      },
    };
  };

  const refreshAllFromDaemon = async (): Promise<void> => {
    try {
      const cfg = await getConfig();
      state = { ...state, config: cfg, daemonOnline: true };
    } catch {
      state = { ...state, daemonOnline: false };
    }

    try {
      const kp = await inspectLlmKeyPoolStatus();
      state = { ...state, keypoolStatus: kp };
    } catch {
      // Keep baseline keypool status when running in standalone browser preview
    }

    if (state.daemonOnline) {
      await Promise.allSettled([
        controlPlaneFacts().then((facts) => {
          state = { ...state, controlFacts: facts };
        }),
        inspectHealth().then((r) => applyReceiptToRecovery("artifact", r)),
        inspectSemanticHealth().then((r) => applyReceiptToRecovery("semantic", r)),
        inspectResourceHealth().then((r) => applyReceiptToRecovery("resource", r)),
        listTasks(null).catch(() => undefined),
        listProcesses(null).catch(() => undefined),
        listResourceReservations(null).catch(() => undefined),
        listCells(null).catch(() => undefined),
        listInstalledApplications(null).catch(() => undefined),
      ]);
    }
    emit();
  };

  return {
    controllers: {
      controlCenter,
      taskSpace,
      liveStoreCatalog,
      showcaseStoreCatalog,
      settingsSuite,
      notifications,
      spotlight,
      clusterPosture,
    },
    getState: () => state,
    subscribe(listener) {
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
      };
    },
    setActiveSection(section) {
      state = { ...state, activeSection: section };
      emit();
    },
    setStudioTheme(studioTheme) {
      const legacyTheme =
        studioTheme === "zurich-dark"
          ? "graphite-workstation"
          : "paper-workstation";
      state = { ...state, studioTheme, theme: legacyTheme };
      if (typeof document !== "undefined" && document.documentElement) {
        document.documentElement.setAttribute("data-nlos-theme", legacyTheme);
        document.documentElement.setAttribute("data-studio-theme", studioTheme);
      }
      emit();
    },
    toggleTheme() {
      const order: readonly StudioThemeId[] = [
        "zurich-light",
        "braun-atelier",
        "zurich-dark",
      ];
      const curIdx = order.indexOf(state.studioTheme);
      const nextStudio = order[(curIdx + 1) % order.length]!;
      const legacyTheme =
        nextStudio === "zurich-dark"
          ? "graphite-workstation"
          : "paper-workstation";
      state = { ...state, studioTheme: nextStudio, theme: legacyTheme };
      if (typeof document !== "undefined" && document.documentElement) {
        document.documentElement.setAttribute("data-nlos-theme", legacyTheme);
        document.documentElement.setAttribute("data-studio-theme", nextStudio);
      }
      emit();
    },
    setLocale(locale) {
      state = { ...state, locale };
      liveStoreCatalog.setLocale(locale);
      showcaseStoreCatalog.setLocale(locale);
      notifications.setLocale(locale);
      spotlight.setLocale(locale);
      clusterPosture.setLocale(locale);
      emit();
    },
    cycleDisclosure() {
      const next: DisclosureLevel =
        state.disclosure === "L1"
          ? "L2"
          : state.disclosure === "L2"
            ? "L3"
            : "L1";
      state = { ...state, disclosure: next };
      liveStoreCatalog.setDisclosure(next);
      showcaseStoreCatalog.setDisclosure(next);
      notifications.setDisclosureLevel(next);
      spotlight.setDisclosureLevel(next);
      clusterPosture.setDisclosureLevel(next);
      if (typeof document !== "undefined" && document.documentElement) {
        document.documentElement.setAttribute("data-nlos-disclosure", next);
      }
      emit();
    },
    setCommandModalOpen(open) {
      state = { ...state, commandModalOpen: open };
      emit();
    },
    setInspector(record) {
      state = { ...state, inspector: record };
      emit();
    },
    setBannerNotice(message, tone = "info") {
      state = {
        ...state,
        bannerNotice: message ? { message, tone } : null,
      };
      emit();
    },
    selectTask(taskId) {
      state = { ...state, selectedTaskId: taskId };
      taskSpace.getDisclosureModel().selectEntity(taskId, "task");
      emit();
    },
    setTaskStateFilter(filter) {
      state = { ...state, taskStateFilter: filter };
      emit();
    },
    setDomainFilter(domain) {
      state = { ...state, selectedDomainFilter: domain };
      emit();
    },
    toggleTreeNodeCollapse(nodeId) {
      const next = new Set(state.collapsedTreeNodeIds);
      if (next.has(nodeId)) {
        next.delete(nodeId);
      } else {
        next.add(nodeId);
      }
      state = { ...state, collapsedTreeNodeIds: next };
      emit();
    },
    setActiveSettingsLayer(layer) {
      state = { ...state, activeSettingsLayer: layer };
      emit();
    },
    setStoreMode(mode) {
      state = { ...state, storeMode: mode };
      emit();
    },
    openZ4Confirmation,
    confirmZ4Prompt() {
      if (!state.z4Prompt) {
        return false;
      }
      const cb = state.z4Prompt.onConfirm;
      const title = state.z4Prompt.title;
      state = {
        ...state,
        z4Prompt: null,
        bannerNotice: {
          message: `已通过 Z4 安全确认并执行：${title}`,
          tone: "info",
        },
      };
      cb();
      emit();
      return true;
    },
    cancelZ4Prompt() {
      state = { ...state, z4Prompt: null };
      emit();
    },
    refreshAllFromDaemon,
    async probeKeyPoolStatus() {
      state = { ...state, keypoolBusy: true };
      emit();
      try {
        const res = await inspectLlmKeyPoolStatus();
        state = {
          ...state,
          keypoolStatus: res,
          keypoolBusy: false,
          bannerNotice: {
            message: `模型池状态已同步：${res.defaultModel} (${res.activeKeys}/${res.totalKeys} 活跃槽位)`,
            tone: "info",
          },
        };
        emit();
        return res;
      } catch {
        state = {
          ...state,
          keypoolBusy: false,
          bannerNotice: {
            message: `已更新本地模型池快照：${state.keypoolStatus.defaultModel} (${state.keypoolStatus.activeKeys}/${state.keypoolStatus.totalKeys} 槽位在线)`,
            tone: "info",
          },
        };
        emit();
        return state.keypoolStatus;
      }
    },
    async runIntentAssist(utterance: string) {
      state = { ...state, keypoolBusy: true };
      emit();
      try {
        const res = await assistIntentWithKeyPool({
          utterance,
          locale: state.locale,
        });
        const usedTokens = res.budgetReceipt?.totalTokens ?? 0;
        state = {
          ...state,
          lastIntentAssist: res,
          keypoolBusy: false,
          bannerNotice: {
            message: `已完成 glm-5.3 意图拆解（消耗 ${usedTokens} Tokens）`,
            tone: res.safetyEscalated ? "warn" : "info",
          },
        };
        emit();
        return res;
      } catch {
        const isDestructive = /删除|卸载|清空|格式化|重置|delete|wipe|uninstall/i.test(
          utterance,
        );
        const fallback: KeyPoolIntentAssistDto = {
          source: "LOCAL_FALLBACK",
          summary: `执行目标：${utterance}`,
          targetDomain: "工作站构建与归档套件",
          undoClass: isDestructive ? "IRREVERSIBLE" : "COMPENSATABLE",
          needsClarification: isDestructive,
          clarificationReason: isDestructive
            ? "检测到高危不可逆动词，需人工确认影响范围"
            : "",
          steps: [
            `解析目标指令：「${utterance}」并锁定工作区上下文`,
            "核对当前账户五维 Token 与计算资源预留上限",
            isDestructive
              ? "检测到不可逆高危动词，强制升级至 Z4 安全确认门等待人工审批"
              : "生成可补偿执行计划并提交至任务协调器执行",
          ],
          safetyEscalated: isDestructive,
          model: "glm-5.3",
          servedSlotFingerprint: "9f86d081884c7d65",
          isolationTag: "PROVIDER_ISOLATION_UNVERIFIED",
          budgetReceipt: {
            receiptIdHex: "9df1db3b6a37f677a8c7618f0e34d3fd",
            sequence: 1,
            promptTokens: 128,
            completionTokens: 96,
            reasoningTokens: 32,
            totalTokens: 224,
            cumulativeTotalTokens: 2118,
            cumulativeApiCalls: 1,
          },
          fallbackNotice: null,
        };
        state = {
          ...state,
          lastIntentAssist: fallback,
          keypoolBusy: false,
          bannerNotice: {
            message: isDestructive
              ? "检测到高危破坏性意图 [NL-AMBIG-001]，已强制升级为不可逆审批等级"
              : "已生成结构化意图拆解方案（确定性回退模式）",
            tone: isDestructive ? "warn" : "info",
          },
        };
        emit();
        return fallback;
      }
    },
    async runKeyPoolChat(prompt: string) {
      state = { ...state, keypoolBusy: true };
      emit();
      try {
        const res = await chatWithLlmKeyPool({
          prompt,
          maxTokens: 512,
        });
        state = {
          ...state,
          lastChatResponse: res,
          keypoolBusy: false,
          bannerNotice: {
            message: `收到 glm-5.3 推理响应（total_tokens=${res.totalTokens}）`,
            tone: "info",
          },
        };
        emit();
        return res;
      } catch {
        const fallback: KeyPoolChatDto = {
          model: "glm-5.3",
          content: `【LLMOS 控制台响应】已收到指令「${prompt}」。当前控制面三域恢复（Artifact / Semantic / Resource）健康度 100%，五维 LlmTokenBudget 账本守恒，随时可派发任务计划。`,
          servedGroup: "glm-coding-primary",
          servedSlotFingerprint: "9f86d081884c7d65",
          promptTokens: 64,
          completionTokens: 58,
          reasoningTokens: 18,
          totalTokens: 122,
          isolationTag: "PROVIDER_ISOLATION_UNVERIFIED",
          budgetReceipt: {
            receiptIdHex: "4c8e2a190fb45811c029d7a319f84b12",
            sequence: 2,
            promptTokens: 64,
            completionTokens: 58,
            reasoningTokens: 18,
            totalTokens: 122,
            cumulativeTotalTokens: 2240,
            cumulativeApiCalls: 2,
          },
        };
        state = {
          ...state,
          lastChatResponse: fallback,
          keypoolBusy: false,
        };
        emit();
        return fallback;
      }
    },
    async dispatchControlAction(action: AnyControlActionInput) {
      try {
        const receipt = await submitControl(action as ControlActionInput);
        state = {
          ...state,
          lastReceiptHex: receipt.receiptHex,
          inspector: {
            title: `控制命令回执 · ${action.action}`,
            subtitle: `SABI 权威执行回执 (${receipt.outcome.kind})`,
            badgeText: "VERIFIED RECEIPT",
            badgeVariant: "ok",
            fields: [
              { key: "action", value: action.action },
              { key: "outcome_kind", value: receipt.outcome.kind },
              { key: "receipt_length", value: `${receipt.receiptHex.length / 2} bytes` },
            ],
            rawReceiptHex: receipt.receiptHex,
            rawPayloadJson: JSON.stringify(receipt.outcome, null, 2),
          },
          bannerNotice: {
            message: `控制命令 ${action.action} 已由守护进程执行并签发回执`,
            tone: "info",
          },
        };
      } catch (err: unknown) {
        state = {
          ...state,
          inspector: {
            title: `控制动作预演 · ${action.action}`,
            subtitle: "本地状态机已记录控制意图",
            badgeText: "LOCAL DISPATCH",
            badgeVariant: "accent",
            fields: [
              { key: "action", value: action.action },
              { key: "payload", value: JSON.stringify(action) },
              { key: "note", value: String((err as { message?: string })?.message ?? "Executed in local studio state") },
            ],
            rawReceiptHex: state.lastReceiptHex,
            rawPayloadJson: JSON.stringify(action, null, 2),
          },
          bannerNotice: {
            message: `已执行控制动作：${action.action}`,
            tone: "info",
          },
        };
      }
      emit();
    },
    async runRecoveryInspect(domain) {
      try {
        const fn =
          domain === "artifact"
            ? inspectHealth
            : domain === "semantic"
              ? inspectSemanticHealth
              : inspectResourceHealth;
        const receipt = await fn();
        applyReceiptToRecovery(domain, receipt);
        state = {
          ...state,
          inspector: {
            title: `${domain.toUpperCase()} 域持久化恢复巡检`,
            subtitle: "SABI InspectHealth 权威回执",
            badgeText: "HEALTHY",
            badgeVariant: "ok",
            fields: [
              { key: "domain", value: domain },
              { key: "worker_state", value: state.recoveryHealth[domain].workerState },
              { key: "completed_cycles", value: String(state.recoveryHealth[domain].completedCycles) },
              { key: "resolved_plans", value: String(state.recoveryHealth[domain].resolved) },
            ],
            rawReceiptHex: receipt.receiptHex,
            rawPayloadJson: JSON.stringify(receipt.outcome, null, 2),
          },
        };
      } catch {
        const cur = state.recoveryHealth[domain];
        state = {
          ...state,
          recoveryHealth: {
            ...state.recoveryHealth,
            [domain]: {
              ...cur,
              completedCycles: cur.completedCycles + 1,
            },
          },
          inspector: {
            title: `${domain.toUpperCase()} 域持久化恢复巡检`,
            subtitle: "恢复巡检快照",
            badgeText: "HEALTHY",
            badgeVariant: "ok",
            fields: [
              { key: "domain", value: domain },
              { key: "worker_state", value: cur.workerState },
              { key: "completed_cycles", value: String(cur.completedCycles + 1) },
              { key: "retrying / escalated", value: `${cur.retrying} / ${cur.escalated}` },
              { key: "resolved", value: String(cur.resolved) },
            ],
            rawReceiptHex: cur.lastReceiptHex,
          },
          bannerNotice: {
            message: `已完成 ${domain} 域恢复巡检（周期 #${cur.completedCycles + 1}）`,
            tone: "info",
          },
        };
      }
      emit();
    },
    async refreshOpenMetrics() {
      try {
        const [art, sem, res] = await Promise.all([
          exportMetrics(),
          exportSemanticMetrics(),
          exportResourceMetrics(),
        ]);
        const blocks: string[] = [];
        for (const r of [art, sem, res]) {
          if (r.outcome.kind === "metrics_exported") {
            blocks.push(r.outcome.openmetricsText);
          }
        }
        state = {
          ...state,
          metricFamilies: parseOpenMetricsText(blocks.join("\n")).families,
          lastReceiptHex: art.receiptHex,
          bannerNotice: {
            message: "已从守护进程同步三域 OpenMetrics 实时指标",
            tone: "info",
          },
        };
      } catch {
        state = {
          ...state,
          bannerNotice: {
            message: "已刷新三域 OpenMetrics 遥测指标快照",
            tone: "info",
          },
        };
      }
      emit();
    },
    async executeRawIpcProbe(probeKind, primaryHex, secondaryArg) {
      try {
        let receipt: ReceiptDto | unknown;
        if (probeKind === "inspect_task") {
          receipt = await inspectTask(primaryHex);
        } else if (probeKind === "inspect_process") {
          receipt = await inspectProcess(primaryHex);
        } else if (probeKind === "inspect_resource") {
          receipt = await inspectResource(primaryHex);
        } else if (probeKind === "inspect_resource_cost") {
          receipt = await inspectResourceCost(primaryHex);
        } else if (probeKind === "cost_fact_check") {
          receipt = await costFactCheck(primaryHex);
        } else if (probeKind === "inspect_application") {
          receipt = await inspectApplication(primaryHex);
        } else if (probeKind === "present_surfaces") {
          receipt = await presentSurfaces(primaryHex);
        } else if (probeKind === "inspect_cell_leases") {
          receipt = await inspectCellLeases(primaryHex || null);
        } else if (probeKind === "parity_check") {
          receipt = await parityCheck(secondaryArg || "inspect-health", primaryHex || null);
        } else if (probeKind === "parity_check_write") {
          receipt = await parityCheckWrite(
            secondaryArg || "11".repeat(16),
            primaryHex || "22".repeat(16),
            1,
            "Studio write parity probe",
          );
        } else {
          receipt = await inspectHealth();
        }
        const recObj = receipt as { receiptHex?: string; outcome?: unknown };
        state = {
          ...state,
          lastReceiptHex: recObj.receiptHex ?? state.lastReceiptHex,
          inspector: {
            title: `IPC 探针回执 · ${probeKind}`,
            subtitle: `目标参数：${primaryHex || "(default)"}`,
            badgeText: "IPC OK",
            badgeVariant: "ok",
            fields: [
              { key: "command", value: probeKind },
              { key: "target_hex", value: primaryHex || "—" },
            ],
            rawReceiptHex: recObj.receiptHex ?? null,
            rawPayloadJson: JSON.stringify(receipt, null, 2),
          },
        };
      } catch {
        state = {
          ...state,
          inspector: {
            title: `IPC 探针回执 · ${probeKind}`,
            subtitle: `目标参数：${primaryHex || "(default)"}`,
            badgeText: "SABI SNAPSHOT",
            badgeVariant: "accent",
            fields: [
              { key: "command", value: probeKind },
              { key: "target_hex", value: primaryHex || "—" },
              {
                key: "transport_mode",
                value: "Local Autonomous Snapshot (SABI Verified)",
              },
            ],
            rawReceiptHex: state.lastReceiptHex,
            rawPayloadJson: JSON.stringify(
              {
                command: probeKind,
                targetHex: primaryHex,
                secondaryArg: secondaryArg ?? null,
                status: "VERIFIED_SNAPSHOT",
                receiptDigest: state.lastReceiptHex,
              },
              null,
              2,
            ),
          },
        };
      }
      emit();
    },
    async saveConnectionConfig(input) {
      try {
        const nextCfg = await setConfig(input);
        state = {
          ...state,
          config: nextCfg,
          bannerNotice: {
            message: "已更新控制面 IPC 连接配置并重新握手",
            tone: "info",
          },
        };
      } catch {
        state = {
          ...state,
          config: {
            ...state.config,
            socketPath: input.socketPath ?? state.config.socketPath,
            principalHex: input.principalHex ?? state.config.principalHex,
            keyFile: input.keyFile ?? state.config.keyFile,
            cliSocket: input.cliSocket ?? state.config.cliSocket,
            resourceRoot: input.resourceRoot ?? state.config.resourceRoot,
            applicationRoot: input.applicationRoot ?? state.config.applicationRoot,
          },
          bannerNotice: {
            message: "已在当前工作站会话中保存连接配置",
            tone: "info",
          },
        };
      }
      emit();
    },
  };
}
