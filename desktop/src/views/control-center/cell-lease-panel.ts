// ==========================================================================
// NLOS Stage D — Stage C Cell Topology, 3-Family Lease Inspector & Migration
// (`desktop/src/views/control-center/cell-lease-panel.ts`)
//
// Covers:
// - `[SCHED-STEAL-001]` Controlled `RequestProcessMigration` across compute nodes
// - `[LEASE-TTL-001]` Stage C 3-family lease inspection & `QUARANTINED_LEASE_FENCE`
// - Nomad triple-watermark bars & strict `L0/L1/L2` zero-jargon discipline
// ==========================================================================

import { renderBulletMicroBarHtml } from "../../ui/bullet-micro-bar";
import {
  type DisclosureLevel,
  renderStatusIndicatorHtml,
  renderStatutoryLockBadgeHtml,
  type WorkstationLocale,
  type WorkstationStatusState,
} from "../../ui/status-indicator";
import { type NomadWatermarkTelemetry } from "./ownership-tree";

export type CellConnectivityPosture =
  | "LOCAL_SINGLE_NODE"
  | "HEALTHY_CLUSTER_MEMBER"
  | "DEGRADED_HEARTBEAT"
  | "PARTITIONED"
  | "FENCED_READ_ONLY"
  | "QUARANTINED"
  | "OFFLINE";

export type PartitionAutonomyGateState =
  | "OPEN"
  | "LOCAL_AUTONOMY_ONLY"
  | "FENCED_READ_ONLY"
  | "QUARANTINED_HOLD";

export type StageCLeaseFamily =
  | "EXECUTION_LEASE"
  | "RESOURCE_SUBPOOL_LEASE"
  | "CAPABILITY_EPOCH_LEASE";

export interface StageCCellLeaseRecord {
  readonly leaseId: string;
  readonly cellId: string;
  readonly family: StageCLeaseFamily;
  readonly holderNodeId: string;
  readonly holderLabels: Readonly<Record<WorkstationLocale, string>>;
  readonly leaseEpoch: number;
  readonly ttlRemainingMs: number;
  readonly clockSkewHorizonMs: number;
  readonly quarantined: boolean;
  readonly hasClosureReceipt: boolean;
  readonly hasFenceReceipt: boolean;
  readonly watermark: NomadWatermarkTelemetry;
}

export interface StageCCellTopologyRecord {
  readonly cellId: string;
  readonly names: Readonly<Record<WorkstationLocale, string>>;
  readonly posture: CellConnectivityPosture;
  readonly autonomyGate: PartitionAutonomyGateState;
  readonly currentEpoch: number;
  readonly clockSkewHorizonMs: number;
  readonly acceptsMigrationTarget: boolean;
  readonly watermark: NomadWatermarkTelemetry;
  readonly leases: readonly StageCCellLeaseRecord[];
}

export interface ProcessMigrationRequest {
  readonly processNodeId: string;
  readonly sourceCellId: string;
  readonly targetCellId: string;
  readonly expectedEpoch: number;
  readonly readBackingToken: string;
  readonly closureReceiptHex?: string;
  readonly fenceReceiptHex?: string;
}

export interface ProcessMigrationOutcome {
  readonly ok: boolean;
  readonly errorCode?:
    | "READ_BACKING_REQUIRED"
    | "MIGRATION_SAME_NODE"
    | "CAS_EPOCH_CONFLICT"
    | "QUARANTINED_LEASE_FENCE"
    | "MIGRATION_TARGET_UNAVAILABLE";
  readonly message: string;
  readonly newEpoch?: number;
  readonly migratedLeaseIds: readonly string[];
  readonly receiptId?: string;
  readonly updatedCells: readonly StageCCellTopologyRecord[];
}

const POSTURE_LABELS: Readonly<
  Record<
    CellConnectivityPosture,
    {
      readonly zh: string;
      readonly en: string;
      readonly statusState: WorkstationStatusState;
    }
  >
> = {
  LOCAL_SINGLE_NODE: {
    zh: "单机主控在线",
    en: "Primary Standalone Online",
    statusState: "ACTIVE",
  },
  HEALTHY_CLUSTER_MEMBER: {
    zh: "协同节点健康",
    en: "Cluster Member Healthy",
    statusState: "RUNNING",
  },
  DEGRADED_HEARTBEAT: {
    zh: "心跳延迟降级",
    en: "Heartbeat Degraded",
    statusState: "RECOVERING",
  },
  PARTITIONED: {
    zh: "网络分区自治",
    en: "Network Partitioned",
    statusState: "WAITING_NETWORK",
  },
  FENCED_READ_ONLY: {
    zh: "围栏只读保护",
    en: "Fenced Read-Only",
    statusState: "SUSPENDED",
  },
  QUARANTINED: {
    zh: "安全隔离观察",
    en: "Isolated Hold",
    statusState: "QUARANTINED",
  },
  OFFLINE: {
    zh: "节点离线",
    en: "Node Offline",
    statusState: "CANCELLED",
  },
};

const GATE_LABELS: Readonly<
  Record<PartitionAutonomyGateState, Readonly<Record<WorkstationLocale, string>>>
> = {
  OPEN: {
    "zh-CN": "协同调度开放",
    "en-US": "Coordination Open",
  },
  LOCAL_AUTONOMY_ONLY: {
    "zh-CN": "仅限本地自治执行",
    "en-US": "Local Autonomy Only",
  },
  FENCED_READ_ONLY: {
    "zh-CN": "写入围栏只读",
    "en-US": "Write-Fenced Read-Only",
  },
  QUARANTINED_HOLD: {
    "zh-CN": "隔离待核验围栏",
    "en-US": "Isolated Hold Fence",
  },
};

const LEASE_FAMILY_LABELS: Readonly<
  Record<
    StageCLeaseFamily,
    {
      readonly zh: string;
      readonly en: string;
      readonly l3RawTag: string;
    }
  >
> = {
  EXECUTION_LEASE: {
    zh: "执行资源保障租约",
    en: "Execution Resource Lease",
    l3RawTag: "ExecutionLease / Quota",
  },
  RESOURCE_SUBPOOL_LEASE: {
    zh: "并发容量子池租约",
    en: "Concurrency Subpool Lease",
    l3RawTag: "ResourceSubpoolLease / Capacity",
  },
  CAPABILITY_EPOCH_LEASE: {
    zh: "独占外设纪元租约",
    en: "Exclusive Device Lease",
    l3RawTag: "CapabilityEpochLease / ExclusiveDevice",
  },
};

function escapeHtml(raw: string): string {
  return raw
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

/**
 * Evaluates and applies a controlled `RequestProcessMigration` (`[SCHED-STEAL-001]`, `[LEASE-TTL-001]`).
 */
export function evaluateProcessMigration(
  cells: readonly StageCCellTopologyRecord[],
  request: ProcessMigrationRequest,
  locale: WorkstationLocale = "zh-CN",
): ProcessMigrationOutcome {
  if (!request.readBackingToken || request.readBackingToken.trim().length === 0) {
    return {
      ok: false,
      errorCode: "READ_BACKING_REQUIRED",
      message:
        locale === "zh-CN"
          ? "缺少最新状态读取凭证，请先刷新节点视图"
          : "Missing state read token; refresh the node view first",
      migratedLeaseIds: [],
      updatedCells: cells,
    };
  }

  if (request.sourceCellId === request.targetCellId) {
    return {
      ok: false,
      errorCode: "MIGRATION_SAME_NODE",
      message:
        locale === "zh-CN"
          ? "源计算节点与目标计算节点不能相同"
          : "Source and target compute nodes must be different",
      migratedLeaseIds: [],
      updatedCells: cells,
    };
  }

  const source = cells.find((c) => c.cellId === request.sourceCellId);
  const target = cells.find((c) => c.cellId === request.targetCellId);
  if (!source || !target) {
    return {
      ok: false,
      errorCode: "MIGRATION_TARGET_UNAVAILABLE",
      message:
        locale === "zh-CN"
          ? "未找到指定的源节点或目标节点"
          : "Specified source or target compute node was not found",
      migratedLeaseIds: [],
      updatedCells: cells,
    };
  }

  if (source.currentEpoch !== request.expectedEpoch) {
    return {
      ok: false,
      errorCode: "CAS_EPOCH_CONFLICT",
      message:
        locale === "zh-CN"
          ? `节点版本已变更为 ${source.currentEpoch}，请刷新后重试`
          : `Node generation changed to ${source.currentEpoch}; refresh and retry`,
      migratedLeaseIds: [],
      updatedCells: cells,
    };
  }

  const hasQuarantinedLease =
    source.posture === "QUARANTINED" ||
    source.leases.some(
      (l) => l.holderNodeId === request.processNodeId && l.quarantined,
    );
  if (
    hasQuarantinedLease &&
    (!request.closureReceiptHex || !request.fenceReceiptHex)
  ) {
    return {
      ok: false,
      errorCode: "QUARANTINED_LEASE_FENCE",
      message:
        locale === "zh-CN"
          ? "隔离区租约必须提供闭环与围栏凭证后才允许迁移或释放"
          : "Isolated lease requires closure and fence receipts before migration or release",
      migratedLeaseIds: [],
      updatedCells: cells,
    };
  }

  if (
    !target.acceptsMigrationTarget ||
    target.posture === "PARTITIONED" ||
    target.posture === "FENCED_READ_ONLY" ||
    target.posture === "QUARANTINED" ||
    target.posture === "OFFLINE"
  ) {
    return {
      ok: false,
      errorCode: "MIGRATION_TARGET_UNAVAILABLE",
      message:
        locale === "zh-CN"
          ? "目标计算节点当前处于隔离、只读或离线状态，无法接收迁移进程"
          : "Target compute node is isolated, fenced, or offline and cannot accept migration",
      migratedLeaseIds: [],
      updatedCells: cells,
    };
  }

  const newEpoch = source.currentEpoch + 1;
  const movingLeases = source.leases
    .filter((l) => l.holderNodeId === request.processNodeId)
    .map((l) => ({
      ...l,
      cellId: target.cellId,
      leaseEpoch: newEpoch,
    }));
  const migratedLeaseIds = movingLeases.map((l) => l.leaseId);

  const updatedCells = cells.map((c) => {
    if (c.cellId === source.cellId) {
      return {
        ...c,
        currentEpoch: newEpoch,
        leases: c.leases.filter((l) => l.holderNodeId !== request.processNodeId),
      };
    }
    if (c.cellId === target.cellId) {
      return {
        ...c,
        currentEpoch: newEpoch,
        leases: [...c.leases, ...movingLeases],
      };
    }
    return c;
  });

  return {
    ok: true,
    message:
      locale === "zh-CN"
        ? "进程与关联资源租约已完成受控迁移"
        : "Process and bound resource leases completed controlled migration",
    newEpoch,
    migratedLeaseIds,
    receiptId: `rcpt_mig_${newEpoch}`,
    updatedCells,
  };
}

/**
 * Renders the Stage C `Cell` topology & 3-family lease inspection panel HTML (`§2A.2`).
 */
export function renderCellLeasePanelHtml(
  cells: readonly StageCCellTopologyRecord[],
  options?: {
    readonly locale?: WorkstationLocale;
    readonly disclosure?: DisclosureLevel;
  },
): string {
  const locale: WorkstationLocale = options?.locale ?? "zh-CN";
  const disclosure: DisclosureLevel = options?.disclosure ?? "L1";

  const cardsHtml = cells
    .map((cell) => {
      const postureDesc = POSTURE_LABELS[cell.posture];
      const postureText =
        disclosure === "L3"
          ? `${locale === "zh-CN" ? postureDesc.zh : postureDesc.en} [Cell:${cell.cellId} · ${cell.posture}]`
          : locale === "zh-CN"
            ? postureDesc.zh
            : postureDesc.en;
      const statusHtml = renderStatusIndicatorHtml({
        state: postureDesc.statusState,
        locale,
        disclosure,
      });

      const gateLabelBase = GATE_LABELS[cell.autonomyGate][locale];
      const gateLabel =
        disclosure === "L3"
          ? `${gateLabelBase} [PartitionAutonomyGate:${cell.autonomyGate} · epoch=${cell.currentEpoch}]`
          : gateLabelBase;

      const cellBarHtml = renderBulletMicroBarHtml({
        actualUsage: cell.watermark.liveUsagePct,
        effectiveGrantedOrLeaseCap: cell.watermark.allocatedPct,
        requestedCeiling: cell.watermark.ceilingPct,
        unit: "%",
        locale,
        disclosure,
      });

      const leasesHtml = cell.leases
        .map((lease) => {
          const fam = LEASE_FAMILY_LABELS[lease.family];
          const famLabel =
            disclosure === "L3"
              ? `${locale === "zh-CN" ? fam.zh : fam.en} [${fam.l3RawTag} · lease_epoch=${lease.leaseEpoch}]`
              : locale === "zh-CN"
                ? fam.zh
                : fam.en;

          const ttlSec = (lease.ttlRemainingMs / 1000).toFixed(1);
          const ttlLabel =
            locale === "zh-CN"
              ? `剩余有效期 ${ttlSec}s · 时钟容差 ±${lease.clockSkewHorizonMs}ms`
              : `Remaining ${ttlSec}s · Clock Window ±${lease.clockSkewHorizonMs}ms`;

          const lockHtml = lease.quarantined
            ? renderStatutoryLockBadgeHtml({
                kind: "QUARANTINED_LEASE_FENCE",
                locale,
                disclosure,
              })
            : "";

          const leaseBarHtml = renderBulletMicroBarHtml({
            actualUsage: lease.watermark.liveUsagePct,
            effectiveGrantedOrLeaseCap: lease.watermark.allocatedPct,
            requestedCeiling: lease.watermark.ceilingPct,
            unit: "%",
            locale,
            disclosure,
          });

          return (
            `<div class="nlos-cell-lease-row" data-lease-id="${escapeHtml(
              lease.leaseId,
            )}" data-lease-family="${escapeHtml(lease.family)}">` +
            `<div class="nlos-cell-lease-row__family">${escapeHtml(famLabel)}</div>` +
            `<div class="nlos-cell-lease-row__holder">${escapeHtml(
              lease.holderLabels[locale],
            )}</div>` +
            `<div class="nlos-cell-lease-row__ttl">${escapeHtml(ttlLabel)} ${lockHtml}</div>` +
            `<div class="nlos-cell-lease-row__bar">${leaseBarHtml}</div>` +
            `</div>`
          );
        })
        .join("");

      return (
        `<article class="nlos-cell-lease-panel__card" data-cell-id="${escapeHtml(
          cell.cellId,
        )}" data-posture="${escapeHtml(cell.posture)}">` +
        `<header class="nlos-cell-lease-panel__header">` +
        `<strong>${escapeHtml(cell.names[locale])}</strong> ` +
        `<span>${escapeHtml(postureText)}</span> ` +
        statusHtml +
        ` <span class="nlos-cell-lease-panel__gate">${escapeHtml(gateLabel)}</span>` +
        `</header>` +
        `<div class="nlos-cell-lease-panel__summary-bar">${cellBarHtml}</div>` +
        `<div class="nlos-cell-lease-panel__leases">${leasesHtml}</div>` +
        `</article>`
      );
    })
    .join("");

  return `<section class="nlos-cell-lease-panel" role="region" aria-label="${
    locale === "zh-CN"
      ? "计算节点与三族资源租约巡检面板"
      : "Compute Nodes & 3-Family Resource Lease Inspector"
  }">${cardsHtml}</section>`;
}

export function createSeededCellTopologies(): readonly StageCCellTopologyRecord[] {
  return [
    {
      cellId: "cell-local-primary",
      names: {
        "zh-CN": "主控工作站计算节点 #01",
        "en-US": "Primary Workstation Compute Node #01",
      },
      posture: "LOCAL_SINGLE_NODE",
      autonomyGate: "OPEN",
      currentEpoch: 42,
      clockSkewHorizonMs: 15,
      acceptsMigrationTarget: true,
      watermark: {
        guaranteedPct: 25,
        allocatedPct: 55,
        ceilingPct: 90,
        liveUsagePct: 48,
        pressureState: "normal",
        cpuMillicores: 2200,
        memoryMib: 4096,
      },
      leases: [
        {
          leaseId: "lease-quota-exec-001",
          cellId: "cell-local-primary",
          family: "EXECUTION_LEASE",
          holderNodeId: "node-proc-worker-4820",
          holderLabels: {
            "zh-CN": "分析器工作进程 (PID 4820)",
            "en-US": "Analyzer Worker Process (PID 4820)",
          },
          leaseEpoch: 42,
          ttlRemainingMs: 28500,
          clockSkewHorizonMs: 15,
          quarantined: false,
          hasClosureReceipt: false,
          hasFenceReceipt: false,
          watermark: {
            guaranteedPct: 25,
            allocatedPct: 60,
            ceilingPct: 88,
            liveUsagePct: 56,
            pressureState: "normal",
            cpuMillicores: 950,
            memoryMib: 1280,
          },
        },
        {
          leaseId: "lease-capacity-subpool-002",
          cellId: "cell-local-primary",
          family: "RESOURCE_SUBPOOL_LEASE",
          holderNodeId: "node-resource-group-main",
          holderLabels: {
            "zh-CN": "构建计算资源保障池",
            "en-US": "Build Compute Resource Pool",
          },
          leaseEpoch: 42,
          ttlRemainingMs: 45000,
          clockSkewHorizonMs: 15,
          quarantined: false,
          hasClosureReceipt: false,
          hasFenceReceipt: false,
          watermark: {
            guaranteedPct: 30,
            allocatedPct: 65,
            ceilingPct: 92,
            liveUsagePct: 58,
            pressureState: "normal",
            cpuMillicores: 1200,
            memoryMib: 2048,
          },
        },
        {
          leaseId: "lease-device-epoch-003",
          cellId: "cell-local-primary",
          family: "CAPABILITY_EPOCH_LEASE",
          holderNodeId: "node-sys-workstation-01",
          holderLabels: {
            "zh-CN": "独占音频硬件采集通道",
            "en-US": "Exclusive Audio Hardware Channel",
          },
          leaseEpoch: 42,
          ttlRemainingMs: 19200,
          clockSkewHorizonMs: 15,
          quarantined: false,
          hasClosureReceipt: false,
          hasFenceReceipt: false,
          watermark: {
            guaranteedPct: 10,
            allocatedPct: 25,
            ceilingPct: 50,
            liveUsagePct: 20,
            pressureState: "normal",
            cpuMillicores: 150,
            memoryMib: 256,
          },
        },
      ],
    },
    {
      cellId: "cell-secondary-compute",
      names: {
        "zh-CN": "辅助加速计算节点 #02",
        "en-US": "Secondary Compute Accelerator Node #02",
      },
      posture: "HEALTHY_CLUSTER_MEMBER",
      autonomyGate: "OPEN",
      currentEpoch: 42,
      clockSkewHorizonMs: 22,
      acceptsMigrationTarget: true,
      watermark: {
        guaranteedPct: 20,
        allocatedPct: 40,
        ceilingPct: 85,
        liveUsagePct: 31,
        pressureState: "normal",
        cpuMillicores: 1100,
        memoryMib: 2048,
      },
      leases: [],
    },
    {
      cellId: "cell-quarantined-edge",
      names: {
        "zh-CN": "隔离观察计算节点 #03",
        "en-US": "Isolated Edge Compute Node #03",
      },
      posture: "QUARANTINED",
      autonomyGate: "QUARANTINED_HOLD",
      currentEpoch: 41,
      clockSkewHorizonMs: 480,
      acceptsMigrationTarget: false,
      watermark: {
        guaranteedPct: 20,
        allocatedPct: 75,
        ceilingPct: 95,
        liveUsagePct: 84,
        pressureState: "oom-risk",
        cpuMillicores: 800,
        memoryMib: 1536,
      },
      leases: [
        {
          leaseId: "lease-quarantined-009",
          cellId: "cell-quarantined-edge",
          family: "CAPABILITY_EPOCH_LEASE",
          holderNodeId: "node-cell-quarantined-edge",
          holderLabels: {
            "zh-CN": "受隔离外设控制句柄",
            "en-US": "Isolated Peripheral Handle",
          },
          leaseEpoch: 41,
          ttlRemainingMs: 0,
          clockSkewHorizonMs: 480,
          quarantined: true,
          hasClosureReceipt: false,
          hasFenceReceipt: false,
          watermark: {
            guaranteedPct: 20,
            allocatedPct: 80,
            ceilingPct: 95,
            liveUsagePct: 86,
            pressureState: "oom-risk",
            cpuMillicores: 400,
            memoryMib: 768,
          },
        },
      ],
    },
  ];
}
