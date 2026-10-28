/**
 * Stage D (`D-NOTIFY-SEARCH-OFFLINE` / `W72-D-NOTIFY-SEARCH`)
 * Offline Autonomy & Cluster Posture Tray (`[DUI-OFFLINE-001]`, `IPC-D7` `ListCells` + `InspectCellLeases`).
 *
 * Aligns with:
 * - `06-架构设计总纲-v0.5.md` §16.3.4 (`IPC-D7` `ListCells` / `InspectCellLeases`), §26.3, §27.3
 * - `ADR-0025` Decision 2 (`L0..L3` progressive disclosure & action pruning),
 *   Decision 3 (Nomad `72-96px × 6px` solid triple-watermark micro-bar, zero blur filters),
 *   Decision 4 (Statutory Lock #3: `QUARANTINED_LEASE_FENCE`)
 * - `2026-10-26-stage-d-desktop-visual-and-ux-design.md` §0.1 (`ANTI-AI-01..07`), §2A.2, §3.5
 */

import type { WorkstationCompositor } from "../../compositor/workstation-compositor";
import { computeAllowedVerbs } from "../../store/action-pruner";
import { renderBulletMicroBarHtml } from "../../ui/bullet-micro-bar";
import { renderIconSvg } from "../../ui/icons";
import {
  type DisclosureLevel,
  renderStatutoryLockBadgeHtml,
  type WorkstationLocale,
} from "../../ui/status-indicator";
import {
  computeDeterministicDigest64Hex,
  deriveDeterministicId32Hex,
} from "../notifications/notification-center";
import {
  LOCAL_PRIMARY_CELL_ID_HEX,
  QUARANTINED_CELL_ID_HEX,
  REMOTE_WORKER_CELL_ID_HEX,
  type SpotlightController,
} from "../spotlight/spotlight-view";

export const Z3_CLUSTER_TRAY_Z_INDEX = 400 as const;

export type ClusterConnectivityMode =
  | "CONNECTED"
  | "PARTITIONED"
  | "RECONNECTING"
  | "LOCAL_SINGLE_NODE";

export type CellNodePosture =
  | "ACTIVE_LOCAL_PRIMARY"
  | "REACHABLE_REMOTE_WORKER"
  | "PARTITIONED_REMOTE_WORKER"
  | "QUARANTINED_FENCED";

export type OfflineSurfaceKind =
  | "LAUNCHER"
  | "LOCAL_ARTIFACTS"
  | "TASK_HISTORY_AND_LOCAL_TASKS"
  | "SETTINGS"
  | "LOCAL_SEARCH"
  | "NOTIFICATION_HISTORY"
  | "REMOTE_CLUSTER_DISPATCH";

export const ALL_LOCAL_AUTONOMOUS_SURFACES: readonly OfflineSurfaceKind[] = [
  "LAUNCHER",
  "LOCAL_ARTIFACTS",
  "TASK_HISTORY_AND_LOCAL_TASKS",
  "SETTINGS",
  "LOCAL_SEARCH",
  "NOTIFICATION_HISTORY",
];

export type OfflineActionAvailabilityStatus =
  | "AVAILABLE_ONLINE"
  | "AVAILABLE_LOCAL_AUTONOMY"
  | "WAITING_NETWORK"
  | "UNAVAILABLE_REMOTE_OFFLINE"
  | "BLOCKED_QUARANTINED_FENCE";

export type OfflineClusterErrorCode =
  | "QUARANTINED_LEASE_FENCE_LOCKED"
  | "REMOTE_DEPENDENCY_OFFLINE"
  | "INVALID_ARGUMENT";

export class OfflineClusterError extends Error {
  public readonly code: OfflineClusterErrorCode;

  public constructor(code: OfflineClusterErrorCode, message: string) {
    super(`[${code}] ${message}`);
    this.name = "OfflineClusterError";
    this.code = code;
  }
}

export interface LocalAutonomyProofBadge {
  readonly proofIdHex: string;
  readonly localCellIdHex: string;
  readonly autonomyEpoch: number;
  readonly offlineCapableSurfaces: readonly string[];
  readonly labelZh: string;
  readonly labelEn: string;
  readonly signatureDigestHex: string;
}

export interface ClusterCellL3Diagnostics {
  readonly cellIdHex: string;
  readonly leaseIdHex: string;
  readonly epoch: number;
  readonly clockSkewMs: number;
  readonly lastHeartbeatMonoMs: number;
  readonly fencingTokenHex: string;
}

export interface ClusterCellSummary {
  readonly cellIdHex: string;
  readonly displayNameZh: string;
  readonly displayNameEn: string;
  posture: CellNodePosture;
  statusLabelZh: string;
  statusLabelEn: string;
  readonly isLocalPrimary: boolean;
  reachable: boolean;
  readonly leaseElapsedMs: number;
  readonly leaseSoftWatermarkMs: number;
  readonly leaseHardCeilingMs: number;
  readonly quarantinedFenceLocked: boolean;
  allowedVerbs: readonly string[];
  l3Diagnostics: ClusterCellL3Diagnostics;
}

export interface ClusterL3Diagnostics {
  readonly partitionAutonomyGateState: ClusterConnectivityMode;
  readonly clusterEpoch: number;
  readonly activeLeaseCount: number;
  readonly quarantinedCellCount: number;
  readonly offlineRemoteActionBlocks: number;
  readonly quarantinedTtlUnlockRejections: number;
}

export interface OfflineClusterPostureSnapshot {
  readonly connectivityMode: ClusterConnectivityMode;
  readonly localAutonomyActive: true;
  readonly localAutonomyProof: LocalAutonomyProofBadge;
  readonly badgeLabelZh: string;
  readonly badgeLabelEn: string;
  readonly statusCopyZh: string;
  readonly statusCopyEn: string;
  readonly cells: readonly ClusterCellSummary[];
  readonly l3Diagnostics: ClusterL3Diagnostics;
}

export interface EvaluateOfflineActionAvailabilityRequest {
  readonly surface: OfflineSurfaceKind;
  readonly actionVerb: string;
  readonly requiresRemoteProvider: boolean;
  readonly targetCellIdHex?: string;
}

export interface OfflineActionAvailabilityDecision {
  readonly surface: OfflineSurfaceKind;
  readonly actionVerb: string;
  readonly status: OfflineActionAvailabilityStatus;
  readonly interactive: boolean;
  readonly autonomyProof?: LocalAutonomyProofBadge;
  readonly statutoryLockKind?: "QUARANTINED_LEASE_FENCE";
  readonly reasonZh: string;
  readonly reasonEn: string;
  readonly l3DiagnosticTag: string;
}

export interface OfflineClusterSecurityAuditCounters {
  quarantinedLeaseTtlUnlockRejections: number;
  offlineRemoteActionBlocks: number;
}

const MODE_BADGE_ZH: Readonly<Record<ClusterConnectivityMode, string>> = {
  CONNECTED: "在线协同",
  PARTITIONED: "离线自治",
  RECONNECTING: "正在重连同步",
  LOCAL_SINGLE_NODE: "本机独立运行",
};

const MODE_BADGE_EN: Readonly<Record<ClusterConnectivityMode, string>> = {
  CONNECTED: "Online Synced",
  PARTITIONED: "Offline Autonomy",
  RECONNECTING: "Reconnecting",
  LOCAL_SINGLE_NODE: "Local Standalone",
};

const MODE_SUMMARY_ZH: Readonly<Record<ClusterConnectivityMode, string>> = {
  CONNECTED: "本机与远端协同节点连接正常，所有服务均可使用。",
  PARTITIONED:
    "远端网络暂时中断 · 本机应用启动、本地制品、任务历史、系统设置、本机搜索与通知中心 100% 正常可用。",
  RECONNECTING:
    "正在重新连接远端协同节点 · 本机功能保持连续可用，云端协同操作将在同步完成后恢复。",
  LOCAL_SINGLE_NODE:
    "当前以本机独立模式运行 · 所有本机工作台功能、历史记录与本地检索均完全可用。",
};

const MODE_SUMMARY_EN: Readonly<Record<ClusterConnectivityMode, string>> = {
  CONNECTED: "Local workstation and remote nodes are connected; all services are available.",
  PARTITIONED:
    "Remote network interrupted · Launcher, local artifacts, task history, settings, local search, and notifications remain 100% available.",
  RECONNECTING:
    "Reconnecting to remote nodes · Local workstation functions remain continuously available while cloud actions resume after sync.",
  LOCAL_SINGLE_NODE:
    "Running in local standalone mode · All local workstation views, history, and local search are 100% available.",
};

const POSTURE_LABEL_ZH: Readonly<Record<CellNodePosture, string>> = {
  ACTIVE_LOCAL_PRIMARY: "本机工作站（自治就绪）",
  REACHABLE_REMOTE_WORKER: "远端协同节点（在线）",
  PARTITIONED_REMOTE_WORKER: "远端协同节点（离线等待网络）",
  QUARANTINED_FENCED: "受保护隔离节点（安全锁定）",
};

const POSTURE_LABEL_EN: Readonly<Record<CellNodePosture, string>> = {
  ACTIVE_LOCAL_PRIMARY: "Local Workstation (Autonomy Ready)",
  REACHABLE_REMOTE_WORKER: "Remote Node (Online)",
  PARTITIONED_REMOTE_WORKER: "Remote Node (Offline — Waiting for Network)",
  QUARANTINED_FENCED: "Isolated Node (Safety Fence Locked)",
};

const SURFACE_LABELS_ZH: Readonly<Record<OfflineSurfaceKind, string>> = {
  LAUNCHER: "应用启动台",
  LOCAL_ARTIFACTS: "本地制品",
  TASK_HISTORY_AND_LOCAL_TASKS: "任务与历史",
  SETTINGS: "系统设置",
  LOCAL_SEARCH: "本机检索",
  NOTIFICATION_HISTORY: "通知中心",
  REMOTE_CLUSTER_DISPATCH: "远端协同派发",
};

const SURFACE_LABELS_EN: Readonly<Record<OfflineSurfaceKind, string>> = {
  LAUNCHER: "Launcher",
  LOCAL_ARTIFACTS: "Local Artifacts",
  TASK_HISTORY_AND_LOCAL_TASKS: "Tasks & History",
  SETTINGS: "Settings",
  LOCAL_SEARCH: "Local Search",
  NOTIFICATION_HISTORY: "Notifications",
  REMOTE_CLUSTER_DISPATCH: "Remote Dispatch",
};

function escapeHtml(raw: string): string {
  return raw
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

function isValidHex32(raw: string): boolean {
  return /^[0-9a-fA-F]{32}$/.test(raw.trim());
}

export function createDefaultClusterFixture(): {
  localAutonomyProof: LocalAutonomyProofBadge;
  cells: ClusterCellSummary[];
} {
  const proofIdHex = deriveDeterministicId32Hex("autonomy_proof", 1);
  const autonomyEpoch = 42;
  const signatureDigestHex = computeDeterministicDigest64Hex(
    "LLMOS_STAGE_D_OFFLINE_AUTONOMY_PROOF_V1",
    [proofIdHex, LOCAL_PRIMARY_CELL_ID_HEX, String(autonomyEpoch)],
  );

  const localAutonomyProof: LocalAutonomyProofBadge = {
    proofIdHex,
    localCellIdHex: LOCAL_PRIMARY_CELL_ID_HEX,
    autonomyEpoch,
    offlineCapableSurfaces: [
      "launcher",
      "artifacts",
      "tasks",
      "settings",
      "search",
      "notifications",
    ],
    labelZh: "本机自治已就绪",
    labelEn: "Local Autonomy Ready",
    signatureDigestHex,
  };

  const healthyVerbs = computeAllowedVerbs({
    domain: "cell",
    state: "ACTIVE",
    callerCapabilities: ["control:write", "cell:migrate"],
    readBacking: {
      targetIdHex: LOCAL_PRIMARY_CELL_ID_HEX,
      expectedGenerationOrRevision: autonomyEpoch,
    },
    statutoryLocks: [],
  }).allowedVerbs;

  const quarantinedVerbs = computeAllowedVerbs({
    domain: "cell",
    state: "QUARANTINED",
    callerCapabilities: ["control:write"],
    statutoryLocks: ["QUARANTINED_LEASE_FENCE"],
  }).allowedVerbs;

  const cells: ClusterCellSummary[] = [
    {
      cellIdHex: LOCAL_PRIMARY_CELL_ID_HEX,
      displayNameZh: "本机主工作站",
      displayNameEn: "Primary Local Workstation",
      posture: "ACTIVE_LOCAL_PRIMARY",
      statusLabelZh: POSTURE_LABEL_ZH.ACTIVE_LOCAL_PRIMARY,
      statusLabelEn: POSTURE_LABEL_EN.ACTIVE_LOCAL_PRIMARY,
      isLocalPrimary: true,
      reachable: true,
      leaseElapsedMs: 12_000,
      leaseSoftWatermarkMs: 45_000,
      leaseHardCeilingMs: 60_000,
      quarantinedFenceLocked: false,
      allowedVerbs: healthyVerbs,
      l3Diagnostics: {
        cellIdHex: LOCAL_PRIMARY_CELL_ID_HEX,
        leaseIdHex: deriveDeterministicId32Hex("lease", 1),
        epoch: autonomyEpoch,
        clockSkewMs: 0,
        lastHeartbeatMonoMs: 1_700_000_240_000,
        fencingTokenHex: computeDeterministicDigest64Hex("FENCE", ["local_primary"]),
      },
    },
    {
      cellIdHex: REMOTE_WORKER_CELL_ID_HEX,
      displayNameZh: "远端构建加速节点 A",
      displayNameEn: "Remote Build Accelerator A",
      posture: "REACHABLE_REMOTE_WORKER",
      statusLabelZh: POSTURE_LABEL_ZH.REACHABLE_REMOTE_WORKER,
      statusLabelEn: POSTURE_LABEL_EN.REACHABLE_REMOTE_WORKER,
      isLocalPrimary: false,
      reachable: true,
      leaseElapsedMs: 28_000,
      leaseSoftWatermarkMs: 45_000,
      leaseHardCeilingMs: 60_000,
      quarantinedFenceLocked: false,
      allowedVerbs: healthyVerbs,
      l3Diagnostics: {
        cellIdHex: REMOTE_WORKER_CELL_ID_HEX,
        leaseIdHex: deriveDeterministicId32Hex("lease", 2),
        epoch: autonomyEpoch,
        clockSkewMs: 14,
        lastHeartbeatMonoMs: 1_700_000_238_000,
        fencingTokenHex: computeDeterministicDigest64Hex("FENCE", ["remote_worker_a"]),
      },
    },
    {
      cellIdHex: QUARANTINED_CELL_ID_HEX,
      displayNameZh: "受保护隔离归档节点 B",
      displayNameEn: "Isolated Archive Node B",
      posture: "QUARANTINED_FENCED",
      statusLabelZh: POSTURE_LABEL_ZH.QUARANTINED_FENCED,
      statusLabelEn: POSTURE_LABEL_EN.QUARANTINED_FENCED,
      isLocalPrimary: false,
      reachable: false,
      leaseElapsedMs: 60_000,
      leaseSoftWatermarkMs: 45_000,
      leaseHardCeilingMs: 60_000,
      quarantinedFenceLocked: true,
      allowedVerbs: quarantinedVerbs,
      l3Diagnostics: {
        cellIdHex: QUARANTINED_CELL_ID_HEX,
        leaseIdHex: deriveDeterministicId32Hex("lease", 3),
        epoch: autonomyEpoch - 1,
        clockSkewMs: 480,
        lastHeartbeatMonoMs: 1_700_000_120_000,
        fencingTokenHex: computeDeterministicDigest64Hex("FENCE", ["quarantined_b"]),
      },
    },
  ];

  return { localAutonomyProof, cells };
}

export interface ClusterPostureTrayViewState {
  readonly open: boolean;
  readonly disclosure: DisclosureLevel;
  readonly locale: WorkstationLocale;
  readonly snapshot: OfflineClusterPostureSnapshot;
  readonly audit: Readonly<OfflineClusterSecurityAuditCounters>;
}

export interface ClusterPostureControllerOptions {
  readonly initialDisclosure?: DisclosureLevel;
  readonly initialLocale?: WorkstationLocale;
  readonly initialMode?: ClusterConnectivityMode;
  readonly compositor?: WorkstationCompositor;
  readonly spotlightController?: SpotlightController;
}

export class ClusterPostureController {
  private connectivityMode: ClusterConnectivityMode;
  private clusterEpoch = 42;
  private readonly localAutonomyProof: LocalAutonomyProofBadge;
  private readonly cells: ClusterCellSummary[];
  private open = false;
  private disclosure: DisclosureLevel;
  private locale: WorkstationLocale;
  private compositor?: WorkstationCompositor;
  private spotlightController?: SpotlightController;
  private readonly audit: OfflineClusterSecurityAuditCounters = {
    quarantinedLeaseTtlUnlockRejections: 0,
    offlineRemoteActionBlocks: 0,
  };

  public constructor(options: ClusterPostureControllerOptions = {}) {
    this.disclosure = options.initialDisclosure ?? "L1";
    this.locale = options.initialLocale ?? "zh-CN";
    this.connectivityMode = options.initialMode ?? "CONNECTED";
    this.compositor = options.compositor;
    this.spotlightController = options.spotlightController;

    const fixture = createDefaultClusterFixture();
    this.localAutonomyProof = fixture.localAutonomyProof;
    this.cells = fixture.cells;

    if (this.connectivityMode !== "CONNECTED") {
      this.setClusterConnectivityMode(this.connectivityMode, 1_700_000_250_000);
    } else {
      this.syncExternalListeners();
    }
  }

  public bindCompositor(compositor: WorkstationCompositor): void {
    this.compositor = compositor;
    this.syncExternalListeners();
  }

  public bindSpotlightController(spotlightController: SpotlightController): void {
    this.spotlightController = spotlightController;
    this.syncExternalListeners();
  }

  public setOpen(open: boolean): void {
    this.open = open;
    if (this.compositor !== undefined) {
      this.compositor.setZ3FlyoutOpen("cluster-posture", open);
    }
  }

  public isOpen(): boolean {
    return this.open;
  }

  public setDisclosureLevel(level: DisclosureLevel): void {
    this.disclosure = level;
  }

  public setLocale(locale: WorkstationLocale): void {
    this.locale = locale;
  }

  public inspectOfflineClusterPosture(): OfflineClusterPostureSnapshot {
    const activeLeaseCount = this.cells.filter(
      (c) => c.reachable && !c.quarantinedFenceLocked,
    ).length;
    const quarantinedCellCount = this.cells.filter((c) => c.quarantinedFenceLocked).length;

    return {
      connectivityMode: this.connectivityMode,
      localAutonomyActive: true,
      localAutonomyProof: { ...this.localAutonomyProof },
      badgeLabelZh: MODE_BADGE_ZH[this.connectivityMode],
      badgeLabelEn: MODE_BADGE_EN[this.connectivityMode],
      statusCopyZh: MODE_SUMMARY_ZH[this.connectivityMode],
      statusCopyEn: MODE_SUMMARY_EN[this.connectivityMode],
      cells: this.cells.map((c) => ({
        ...c,
        l3Diagnostics: { ...c.l3Diagnostics },
      })),
      l3Diagnostics: {
        partitionAutonomyGateState: this.connectivityMode,
        clusterEpoch: this.clusterEpoch,
        activeLeaseCount,
        quarantinedCellCount,
        offlineRemoteActionBlocks: this.audit.offlineRemoteActionBlocks,
        quarantinedTtlUnlockRejections: this.audit.quarantinedLeaseTtlUnlockRejections,
      },
    };
  }

  public setClusterConnectivityMode(
    mode: ClusterConnectivityMode,
    nowMonoMs = 1_700_000_300_000,
  ): OfflineClusterPostureSnapshot {
    this.connectivityMode = mode;
    this.clusterEpoch += 1;

    for (const cell of this.cells) {
      if (cell.isLocalPrimary) {
        cell.posture = "ACTIVE_LOCAL_PRIMARY";
        cell.reachable = true;
        cell.statusLabelZh = POSTURE_LABEL_ZH.ACTIVE_LOCAL_PRIMARY;
        cell.statusLabelEn = POSTURE_LABEL_EN.ACTIVE_LOCAL_PRIMARY;
        cell.l3Diagnostics = {
          ...cell.l3Diagnostics,
          epoch: this.clusterEpoch,
          lastHeartbeatMonoMs: nowMonoMs,
        };
      } else if (cell.quarantinedFenceLocked) {
        // Statutory Lock #3: QUARANTINED cell remains fenced regardless of connectivity mode
        cell.posture = "QUARANTINED_FENCED";
        cell.reachable = false;
        cell.statusLabelZh = POSTURE_LABEL_ZH.QUARANTINED_FENCED;
        cell.statusLabelEn = POSTURE_LABEL_EN.QUARANTINED_FENCED;
      } else if (mode === "CONNECTED") {
        cell.posture = "REACHABLE_REMOTE_WORKER";
        cell.reachable = true;
        cell.statusLabelZh = POSTURE_LABEL_ZH.REACHABLE_REMOTE_WORKER;
        cell.statusLabelEn = POSTURE_LABEL_EN.REACHABLE_REMOTE_WORKER;
        cell.l3Diagnostics = {
          ...cell.l3Diagnostics,
          epoch: this.clusterEpoch,
          lastHeartbeatMonoMs: nowMonoMs,
        };
      } else {
        cell.posture = "PARTITIONED_REMOTE_WORKER";
        cell.reachable = false;
        cell.statusLabelZh = POSTURE_LABEL_ZH.PARTITIONED_REMOTE_WORKER;
        cell.statusLabelEn = POSTURE_LABEL_EN.PARTITIONED_REMOTE_WORKER;
      }
    }

    this.syncExternalListeners();
    return this.inspectOfflineClusterPosture();
  }

  /**
   * Evaluates `[DUI-OFFLINE-001]` action availability:
   * - Local workstation surfaces (`Launcher`, `LocalArtifacts`, `TaskHistoryAndLocalTasks`,
   *   `Settings`, `LocalSearch`, `NotificationHistory`) remain 100% interactive in all modes
   *   with a signed `LocalAutonomyProofBadge`.
   * - Actions targeting a `QUARANTINED` cell fail closed with `BLOCKED_QUARANTINED_FENCE`.
   * - Remote-dependent actions in offline/partitioned modes degrade deterministically to
   *   `WAITING_NETWORK` or `UNAVAILABLE_REMOTE_OFFLINE`.
   */
  public evaluateOfflineActionAvailability(
    request: EvaluateOfflineActionAvailabilityRequest,
  ): OfflineActionAvailabilityDecision {
    if (request.targetCellIdHex !== undefined && !isValidHex32(request.targetCellIdHex)) {
      throw new OfflineClusterError(
        "INVALID_ARGUMENT",
        `targetCellIdHex must be 32 hex chars, got '${request.targetCellIdHex}'`,
      );
    }

    if (request.targetCellIdHex !== undefined) {
      const targetCell = this.cells.find(
        (c) =>
          c.cellIdHex.toLowerCase() === request.targetCellIdHex!.trim().toLowerCase(),
      );
      if (targetCell !== undefined) {
        if (targetCell.quarantinedFenceLocked) {
          this.audit.offlineRemoteActionBlocks += 1;
          return {
            surface: request.surface,
            actionVerb: request.actionVerb,
            status: "BLOCKED_QUARANTINED_FENCE",
            interactive: false,
            statutoryLockKind: "QUARANTINED_LEASE_FENCE",
            reasonZh:
              "目标节点处于安全隔离保护态，为防止数据冲突已暂停向该节点派发操作。",
            reasonEn:
              "Target node is under safety isolation; operations to this node are paused to prevent data conflict.",
            l3DiagnosticTag: `QUARANTINED_LEASE_FENCE · cell=${targetCell.cellIdHex} · epoch=${targetCell.l3Diagnostics.epoch} · lease=${targetCell.l3Diagnostics.leaseIdHex}`,
          };
        }

        if (!targetCell.isLocalPrimary && !targetCell.reachable) {
          this.audit.offlineRemoteActionBlocks += 1;
          const status: OfflineActionAvailabilityStatus =
            this.connectivityMode === "LOCAL_SINGLE_NODE"
              ? "UNAVAILABLE_REMOTE_OFFLINE"
              : "WAITING_NETWORK";
          return {
            surface: request.surface,
            actionVerb: request.actionVerb,
            status,
            interactive: false,
            reasonZh:
              "此操作需要连接远端协同节点（当前离线，等待网络恢复后可继续）。",
            reasonEn:
              "This action requires a remote node connection (currently offline; will resume when network returns).",
            l3DiagnosticTag: `REMOTE_CELL_UNREACHABLE · mode=${this.connectivityMode} · cell=${targetCell.cellIdHex} · epoch=${targetCell.l3Diagnostics.epoch}`,
          };
        }
      }
    }

    if (
      request.requiresRemoteProvider ||
      request.surface === "REMOTE_CLUSTER_DISPATCH"
    ) {
      if (this.connectivityMode !== "CONNECTED") {
        this.audit.offlineRemoteActionBlocks += 1;
        const status: OfflineActionAvailabilityStatus =
          this.connectivityMode === "LOCAL_SINGLE_NODE"
            ? "UNAVAILABLE_REMOTE_OFFLINE"
            : "WAITING_NETWORK";
        return {
          surface: request.surface,
          actionVerb: request.actionVerb,
          status,
          interactive: false,
          reasonZh:
            "此操作需要连接云端模型或远端服务（当前处于本机离线模式，本机功能不受影响）。",
          reasonEn:
            "This action requires a remote model or cloud service (currently in local offline mode; local workstation features remain unaffected).",
          l3DiagnosticTag: `REMOTE_PROVIDER_OFFLINE · gate=${this.connectivityMode} · cluster_epoch=${this.clusterEpoch}`,
        };
      }

      return {
        surface: request.surface,
        actionVerb: request.actionVerb,
        status: "AVAILABLE_ONLINE",
        interactive: true,
        autonomyProof: { ...this.localAutonomyProof },
        reasonZh: "远端协同服务在线可用。",
        reasonEn: "Remote collaborative service is online and available.",
        l3DiagnosticTag: `ONLINE_CONNECTED · cluster_epoch=${this.clusterEpoch}`,
      };
    }

    const status: OfflineActionAvailabilityStatus =
      this.connectivityMode === "CONNECTED"
        ? "AVAILABLE_ONLINE"
        : "AVAILABLE_LOCAL_AUTONOMY";

    return {
      surface: request.surface,
      actionVerb: request.actionVerb,
      status,
      interactive: true,
      autonomyProof: { ...this.localAutonomyProof },
      reasonZh: "本机自治保障：无需外部网络即可完整使用此功能。",
      reasonEn:
        "Local autonomy guaranteed: full functionality available without external network.",
      l3DiagnosticTag: `LOCAL_AUTONOMY_PROOF · proof=${this.localAutonomyProof.proofIdHex} · cell=${this.localAutonomyProof.localCellIdHex} · epoch=${this.localAutonomyProof.autonomyEpoch}`,
    };
  }

  /**
   * Statutory Lock #3 (`QUARANTINED_LEASE_FENCE`):
   * Rejects any attempt to unlock a `QUARANTINED` node lease by local TTL expiration alone.
   */
  public attemptUnlockQuarantinedLeaseByTtl(
    cellIdHex: string,
    elapsedLocalTtlMs: number,
  ): void {
    if (!isValidHex32(cellIdHex)) {
      throw new OfflineClusterError(
        "INVALID_ARGUMENT",
        `cellIdHex must be 32 hex chars, got '${cellIdHex}'`,
      );
    }
    const cell = this.cells.find(
      (c) => c.cellIdHex.toLowerCase() === cellIdHex.trim().toLowerCase(),
    );
    if (cell === undefined) {
      throw new OfflineClusterError("INVALID_ARGUMENT", `cell '${cellIdHex}' not found`);
    }
    if (cell.quarantinedFenceLocked) {
      this.audit.quarantinedLeaseTtlUnlockRejections += 1;
      throw new OfflineClusterError(
        "QUARANTINED_LEASE_FENCE_LOCKED",
        `Statutory Lock #3 violation: cell '${cell.cellIdHex}' is fenced in QUARANTINED state (epoch=${cell.l3Diagnostics.epoch}, lease=${cell.l3Diagnostics.leaseIdHex}); local TTL expiration (${elapsedLocalTtlMs} ms >= ceiling ${cell.leaseHardCeilingMs} ms) cannot unlock fence without authoritative epoch progression`,
      );
    }
  }

  public getSecurityAudit(): Readonly<OfflineClusterSecurityAuditCounters> {
    return { ...this.audit };
  }

  public getState(): ClusterPostureTrayViewState {
    return {
      open: this.open,
      disclosure: this.disclosure,
      locale: this.locale,
      snapshot: this.inspectOfflineClusterPosture(),
      audit: this.getSecurityAudit(),
    };
  }

  private syncExternalListeners(): void {
    if (this.compositor !== undefined) {
      const kind =
        this.connectivityMode === "CONNECTED"
          ? "cluster-online"
          : this.connectivityMode === "RECONNECTING"
            ? "cluster-degraded"
            : this.connectivityMode === "PARTITIONED"
              ? "offline-local-ready"
              : "local-ready";
      this.compositor.setConnectivity(kind, {
        onlineNodes: this.cells.filter((c) => c.reachable).length,
        totalNodes: this.cells.length,
        l3TopologyTag: `${this.connectivityMode} · epoch=${this.clusterEpoch}`,
      });
    }
    if (this.spotlightController !== undefined) {
      for (const cell of this.cells) {
        if (!cell.isLocalPrimary) {
          this.spotlightController.setShardReachability(
            cell.cellIdHex,
            cell.reachable && !cell.quarantinedFenceLocked,
          );
        }
      }
    }
  }
}

export function renderClusterPostureTrayHtml(state: ClusterPostureTrayViewState): string {
  const isEn = state.locale === "en-US";
  const isL3 = state.disclosure === "L3";
  const snap = state.snapshot;
  const headerTitle = isEn ? "Workstation Autonomy & Nodes" : "本机自治与协同节点态势";
  const badgeLabel = isEn ? snap.badgeLabelEn : snap.badgeLabelZh;
  const summaryCopy = isEn ? snap.statusCopyEn : snap.statusCopyZh;
  const proofLabel = isEn
    ? snap.localAutonomyProof.labelEn
    : snap.localAutonomyProof.labelZh;

  const surfacePillsHtml = ALL_LOCAL_AUTONOMOUS_SURFACES.map((surface) => {
    const label = isEn ? SURFACE_LABELS_EN[surface] : SURFACE_LABELS_ZH[surface];
    return `<span class="nlos-cluster-tray__surface-pill" data-surface="${surface}" data-interactive="true">${renderIconSvg("check", { size: 14 })}<span>${escapeHtml(label)}</span></span>`;
  }).join("");

  const nodesHtml = snap.cells
    .map((cell) => {
      const name = isEn ? cell.displayNameEn : cell.displayNameZh;
      const statusText = isEn ? cell.statusLabelEn : cell.statusLabelZh;
      const quarantinedClass = cell.quarantinedFenceLocked
        ? " nlos-cluster-node--quarantined"
        : "";

      const microBarHtml = renderBulletMicroBarHtml({
        actualUsage: cell.leaseElapsedMs,
        effectiveGrantedOrLeaseCap: cell.leaseSoftWatermarkMs,
        requestedCeiling: cell.leaseHardCeilingMs,
        widthPx: 88,
        unit: "ms",
        disclosure: state.disclosure,
        locale: state.locale,
      });

      const fenceBadgeHtml = cell.quarantinedFenceLocked
        ? renderStatutoryLockBadgeHtml({
            kind: "QUARANTINED_LEASE_FENCE",
            disclosure: state.disclosure,
            locale: state.locale,
          })
        : "";

      const l3CellHtml = isL3
        ? `<div class="nlos-cluster-node__l3 nlos-l3-only" data-l3-diagnostics="true">` +
          `<span>cell=${escapeHtml(cell.l3Diagnostics.cellIdHex.slice(0, 12))}</span>` +
          `<span>epoch=${cell.l3Diagnostics.epoch}</span>` +
          `<span>lease=${escapeHtml(cell.l3Diagnostics.leaseIdHex.slice(0, 12))}</span>` +
          `<span>skew=${cell.l3Diagnostics.clockSkewMs}ms</span>` +
          `<span>fence=${escapeHtml(cell.l3Diagnostics.fencingTokenHex.slice(0, 12))}</span>` +
          `</div>`
        : "";

      const leaseLabel = isEn ? "Node activity window" : "节点连接状态窗";

      return (
        `<article class="nlos-cluster-node${quarantinedClass}" data-cell-id="${escapeHtml(cell.cellIdHex)}" data-reachable="${String(cell.reachable)}" data-quarantined="${String(cell.quarantinedFenceLocked)}">` +
        `<div class="nlos-cluster-node__row">` +
        `<h4 class="nlos-cluster-node__name">${escapeHtml(name)}</h4>` +
        `${fenceBadgeHtml}` +
        `</div>` +
        `<span class="nlos-cluster-node__status">${escapeHtml(statusText)}</span>` +
        `<div class="nlos-cluster-node__lease-row">` +
        `<span>${escapeHtml(leaseLabel)}</span>` +
        `${microBarHtml}` +
        `</div>` +
        `${l3CellHtml}` +
        `</article>`
      );
    })
    .join("");

  const footerHtml = isL3
    ? `<span class="nlos-l3-only">gate=${escapeHtml(snap.l3Diagnostics.partitionAutonomyGateState)} · cluster_epoch=${snap.l3Diagnostics.clusterEpoch} · active_leases=${snap.l3Diagnostics.activeLeaseCount} · quarantined=${snap.l3Diagnostics.quarantinedCellCount} · proof=${escapeHtml(snap.localAutonomyProof.proofIdHex.slice(0, 12))}</span>`
    : `<span>${isEn ? "Local workstation functions operate independently of external network" : "本机核心功能独立运行，不依赖外部网络"}</span>`;

  return (
    `<section class="nlos-cluster-tray" role="region" aria-label="${escapeHtml(headerTitle)}" data-z-layer="Z3" data-z-index="${Z3_CLUSTER_TRAY_Z_INDEX}" data-disclosure="${state.disclosure}" data-connectivity-mode="${snap.connectivityMode}">` +
    `<header class="nlos-cluster-tray__header">` +
    `<h3 class="nlos-cluster-tray__title">${escapeHtml(headerTitle)}</h3>` +
    `<span class="nlos-cluster-tray__posture-badge">${escapeHtml(badgeLabel)}</span>` +
    `</header>` +
    `<div class="nlos-cluster-tray__summary">` +
    `<p class="nlos-cluster-tray__summary-copy">${escapeHtml(summaryCopy)}</p>` +
    `<div class="nlos-cluster-tray__autonomy-proof" data-autonomy-active="${String(snap.localAutonomyActive)}">` +
    `${renderIconSvg("shield-seal", { size: 14 })}` +
    `<span>${escapeHtml(proofLabel)}</span>` +
    `</div>` +
    `</div>` +
    `<div class="nlos-cluster-tray__surfaces" aria-label="${isEn ? "Offline capable local surfaces" : "本机无网可用功能"}">` +
    `${surfacePillsHtml}` +
    `</div>` +
    `<div class="nlos-cluster-tray__nodes">` +
    `${nodesHtml}` +
    `</div>` +
    `<footer class="nlos-cluster-tray__footer">` +
    `${footerHtml}` +
    `</footer>` +
    `</section>`
  );
}

export function createClusterPostureTrayElement(
  controller: ClusterPostureController,
  doc: Document = document,
): HTMLElement {
  const wrapper = doc.createElement("div");
  wrapper.className = "nlos-cluster-tray-host";
  wrapper.innerHTML = renderClusterPostureTrayHtml(controller.getState());
  return wrapper;
}
