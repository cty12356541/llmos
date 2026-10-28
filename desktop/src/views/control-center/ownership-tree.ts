// ==========================================================================
// NLOS Stage D — 12-Domain & 10-Layer System Informer Ownership Tree
// (`desktop/src/views/control-center/ownership-tree.ts`)
//
// Covers:
// - `[DUI-CTRL-001]` 12-Domain System Control Center & System Informer 10-layer tree
// - `[TASK-AGENT-001]` / `[TASK-ISO-005]` `SHARED_THREAD_LIKE` fault-domain propagation
// - `[UI-PARITY-001]` CNCF Headlamp action pruning & 5+1 statutory non-tamperable locks
// - `O(visible_rows)` cursor-windowed tree projection & Nomad triple-watermark bars
// ==========================================================================

import {
  type ActionPrunerDomain,
  type ActionPrunerResult,
  computeAllowedVerbs,
  type ReadBackingToken,
} from "../../store/action-pruner";
import { renderBulletMicroBarHtml } from "../../ui/bullet-micro-bar";
import {
  type DisclosureLevel,
  renderStatusIndicatorHtml,
  renderStatutoryLockBadgeHtml,
  type StatutoryLockKind,
  type WorkstationLocale,
  type WorkstationStatusState,
} from "../../ui/status-indicator";

/**
 * The 12 authoritative System Control Center domains (`[DUI-CTRL-001]`).
 */
export type ControlCenterDomainId =
  | "system-device"
  | "workspace"
  | "application"
  | "task"
  | "task-group"
  | "task-node"
  | "task-attempt"
  | "execution-unit"
  | "resource-group"
  | "topic-channel"
  | "operation"
  | "cell";

export const ALL_CONTROL_CENTER_DOMAINS: readonly ControlCenterDomainId[] = [
  "system-device",
  "workspace",
  "application",
  "task",
  "task-group",
  "task-node",
  "task-attempt",
  "execution-unit",
  "resource-group",
  "topic-channel",
  "operation",
  "cell",
] as const;

/**
 * System Informer 10-layer parent-child ownership hierarchy (`§2A.1`).
 */
export type OwnershipHierarchyLayerId =
  | "L1_SYSTEM_DEVICE"
  | "L2_WORKSPACE_TENANT"
  | "L3_APPLICATION"
  | "L4_TASK"
  | "L5_TASK_GROUP"
  | "L6_TASK_NODE"
  | "L7_TASK_ATTEMPT"
  | "L8_PROCESS"
  | "L9_AGENT_INSTANCE"
  | "L10_EXECUTION_FIBER";

export const ALL_OWNERSHIP_LAYERS: readonly OwnershipHierarchyLayerId[] = [
  "L1_SYSTEM_DEVICE",
  "L2_WORKSPACE_TENANT",
  "L3_APPLICATION",
  "L4_TASK",
  "L5_TASK_GROUP",
  "L6_TASK_NODE",
  "L7_TASK_ATTEMPT",
  "L8_PROCESS",
  "L9_AGENT_INSTANCE",
  "L10_EXECUTION_FIBER",
] as const;

export type FaultDomainPosture = "ISOLATED_PROCESS" | "SHARED_THREAD_LIKE";

export type ResourcePressurePosture =
  | "normal"
  | "throttled"
  | "oom-risk"
  | "exhausted";

export interface NomadWatermarkTelemetry {
  readonly guaranteedPct: number;
  readonly allocatedPct: number;
  readonly ceilingPct: number;
  readonly liveUsagePct: number;
  readonly pressureState: ResourcePressurePosture;
  readonly cpuMillicores: number;
  readonly memoryMib: number;
}

export interface OwnershipTreeNodeSpec {
  readonly nodeId: string;
  readonly parentNodeId: string | null;
  readonly domain: ControlCenterDomainId;
  readonly prunerDomain?: ActionPrunerDomain;
  readonly layer: OwnershipHierarchyLayerId;
  readonly depth: number;
  readonly labels: Readonly<Record<WorkstationLocale, string>>;
  readonly state: string;
  readonly hostPid?: number;
  readonly watermark: NomadWatermarkTelemetry;
  readonly undoClass?: "reversible" | "compensatable" | "irreversible";
  readonly searchRestrictionsBits?: number;
  readonly statutoryLocks?: readonly StatutoryLockKind[];
  readonly hasActiveTasksForUninstall?: boolean;
  readonly linkedCellId?: string;
  readonly linkedResourceGroupId?: string;
  readonly linkedTopicId?: string;
  readonly linkedOperationId?: string;
  readonly l3DiagnosticTag: string;
}

export interface EnrichedOwnershipTreeNode extends OwnershipTreeNodeSpec {
  readonly faultDomainPosture: FaultDomainPosture;
  readonly sharedFatePeerIds: readonly string[];
  readonly prunerResult: ActionPrunerResult;
}

export interface ControlDomainSummaryCard {
  readonly domain: ControlCenterDomainId;
  readonly titles: Readonly<Record<WorkstationLocale, string>>;
  readonly totalCount: number;
  readonly activeCount: number;
  readonly attentionCount: number;
  readonly lockedCount: number;
  readonly watermark: NomadWatermarkTelemetry;
}

const DEFAULT_READ_BACKING_HEX = "0123456789abcdef0123456789abcdef";

const DOMAIN_TITLES: Readonly<
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

const LAYER_LABELS_HUMAN: Readonly<
  Record<OwnershipHierarchyLayerId, Readonly<Record<WorkstationLocale, string>>>
> = {
  L1_SYSTEM_DEVICE: { "zh-CN": "系统设备层", "en-US": "System Host" },
  L2_WORKSPACE_TENANT: { "zh-CN": "工作区层", "en-US": "Workspace" },
  L3_APPLICATION: { "zh-CN": "应用套件层", "en-US": "Application" },
  L4_TASK: { "zh-CN": "用户任务层", "en-US": "User Task" },
  L5_TASK_GROUP: { "zh-CN": "任务编排组", "en-US": "Task Group" },
  L6_TASK_NODE: { "zh-CN": "执行步骤", "en-US": "Step Node" },
  L7_TASK_ATTEMPT: { "zh-CN": "单次执行记录", "en-US": "Execution Run" },
  L8_PROCESS: { "zh-CN": "系统进程", "en-US": "OS Process" },
  L9_AGENT_INSTANCE: { "zh-CN": "协作代理实例", "en-US": "Agent Worker" },
  L10_EXECUTION_FIBER: { "zh-CN": "轻量并发流", "en-US": "Concurrent Stream" },
};

const LAYER_LABELS_L3_RAW: Readonly<Record<OwnershipHierarchyLayerId, string>> = {
  L1_SYSTEM_DEVICE: "System/Device",
  L2_WORKSPACE_TENANT: "Workspace/Tenant",
  L3_APPLICATION: "Application",
  L4_TASK: "Task",
  L5_TASK_GROUP: "TaskGroup",
  L6_TASK_NODE: "TaskNode",
  L7_TASK_ATTEMPT: "TaskAttempt",
  L8_PROCESS: "Process",
  L9_AGENT_INSTANCE: "AgentInstance",
  L10_EXECUTION_FIBER: "ExecutionFiber",
};

const VERB_LABELS: Readonly<
  Record<string, Readonly<Record<WorkstationLocale, string>>>
> = {
  SwitchPerformanceProfile: { "zh-CN": "切换性能档位", "en-US": "Switch Profile" },
  PauseBackgroundTier: { "zh-CN": "暂停后台任务层", "en-US": "Pause Background" },
  RevokeDevicePermission: { "zh-CN": "收回硬件权限", "en-US": "Revoke Hardware" },
  AcknowledgeRecoveryAlert: { "zh-CN": "确认恢复告警", "en-US": "Ack Alert" },
  ResumeRecovery: { "zh-CN": "恢复域运行", "en-US": "Resume Recovery" },
  ResetDevice: { "zh-CN": "重置设备", "en-US": "Reset Device" },
  ActivateWorkspace: { "zh-CN": "切换至此工作区", "en-US": "Activate Workspace" },
  UpdateWorkspaceLimit: { "zh-CN": "调整资源上限", "en-US": "Adjust Limit" },
  UpdateWorkspaceSettings: { "zh-CN": "更新工作区配置", "en-US": "Workspace Settings" },
  ArchiveWorkspace: { "zh-CN": "归档工作区", "en-US": "Archive Workspace" },
  Launch: { "zh-CN": "启动应用", "en-US": "Launch" },
  Activate: { "zh-CN": "置前窗口", "en-US": "Bring to Front" },
  Terminate: { "zh-CN": "终止应用", "en-US": "Terminate" },
  ToggleBackground: { "zh-CN": "切换后台运行", "en-US": "Toggle Background" },
  DisableApplication: { "zh-CN": "停用应用", "en-US": "Disable App" },
  UninstallApplication: { "zh-CN": "卸载应用", "en-US": "Uninstall App" },
  Pause: { "zh-CN": "暂停", "en-US": "Pause" },
  Resume: { "zh-CN": "继续", "en-US": "Resume" },
  Cancel: { "zh-CN": "取消", "en-US": "Cancel" },
  Retry: { "zh-CN": "重试", "en-US": "Retry" },
  Refine: { "zh-CN": "调整并重跑", "en-US": "Refine" },
  Undo: { "zh-CN": "撤销操作", "en-US": "Undo" },
  ViewEffectProof: { "zh-CN": "查看执行凭证", "en-US": "View Proof" },
  Seal: { "zh-CN": "封存任务组", "en-US": "Seal Group" },
  SetFanoutLimit: { "zh-CN": "限制并发度", "en-US": "Set Concurrency" },
  Supersede: { "zh-CN": "发起替代运行", "en-US": "Supersede Run" },
  RequestReverify: { "zh-CN": "重新运行验证", "en-US": "Re-verify" },
  Throttle: { "zh-CN": "限制速率", "en-US": "Throttle" },
  Freeze: { "zh-CN": "冻结进程", "en-US": "Freeze" },
  Kill: { "zh-CN": "强制结束", "en-US": "Force Stop" },
  Restart: { "zh-CN": "重启单元", "en-US": "Restart" },
  RequestProcessMigration: { "zh-CN": "受控迁移进程", "en-US": "Migrate Process" },
  ReweightShare: { "zh-CN": "调整资源权重", "en-US": "Reweight Share" },
  SetHardCap: { "zh-CN": "设置硬上限", "en-US": "Set Hard Cap" },
  FreezeContext: { "zh-CN": "冻结资源池", "en-US": "Freeze Pool" },
  ReclaimIdlers: { "zh-CN": "回收空闲资源", "en-US": "Reclaim Idle" },
  PauseConsumer: { "zh-CN": "暂停消费", "en-US": "Pause Consumer" },
  ResumeConsumer: { "zh-CN": "恢复消费", "en-US": "Resume Consumer" },
  Drain: { "zh-CN": "排空通道队列", "en-US": "Drain Queue" },
  Purge: { "zh-CN": "彻底清空积压消息", "en-US": "Purge Backlog" },
  RevokePublisher: { "zh-CN": "收回发布权限", "en-US": "Revoke Publisher" },
  Reconcile: { "zh-CN": "触发状态对账", "en-US": "Reconcile Outcome" },
  Compensate: { "zh-CN": "执行补偿回滚", "en-US": "Run Compensation" },
  InspectFenceStatus: { "zh-CN": "查看安全围栏状态", "en-US": "Inspect Safety Fence" },
  JumpToAuthoritativeObject: { "zh-CN": "跳转至源对象", "en-US": "Open Source Object" },
};

function escapeHtml(raw: string): string {
  return raw
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

function mapDomainToActionPrunerDomain(
  spec: OwnershipTreeNodeSpec,
): ActionPrunerDomain {
  if (spec.prunerDomain) {
    return spec.prunerDomain;
  }
  switch (spec.domain) {
    case "execution-unit":
      return spec.layer === "L10_EXECUTION_FIBER"
        ? "execution-fiber"
        : "process";
    case "topic-channel":
      return "topic";
    default:
      return spec.domain;
  }
}

export function mapNodeStateToWorkstationStatus(
  rawState: string,
): WorkstationStatusState {
  const upper = rawState.trim().toUpperCase();
  switch (upper) {
    case "ACTIVE":
      return "ACTIVE";
    case "RUNNING":
      return "RUNNING";
    case "COMPLETED":
      return "COMPLETED";
    case "PENDING":
    case "DECLARED":
      return "DECLARED";
    case "PAUSED":
    case "SUSPENDED":
    case "THROTTLED":
    case "FROZEN":
      return "SUSPENDED";
    case "EFFECT_UNKNOWN":
    case "DISPUTED":
      return "DISPUTED";
    case "QUARANTINED":
      return "QUARANTINED";
    default:
      return "AUTHORIZED";
  }
}

export function formatVerbLabel(
  verb: string,
  locale: WorkstationLocale,
  disclosure: DisclosureLevel,
): string {
  const human = VERB_LABELS[verb]?.[locale] ?? verb;
  if (disclosure === "L3") {
    return `${human} (${verb})`;
  }
  return human;
}

export function formatLayerBadgeLabel(
  layer: OwnershipHierarchyLayerId,
  locale: WorkstationLocale,
  disclosure: DisclosureLevel,
): string {
  const human = LAYER_LABELS_HUMAN[layer][locale];
  if (disclosure === "L3") {
    return `${human} [${LAYER_LABELS_L3_RAW[layer]}]`;
  }
  return human;
}

/**
 * Enriches the 10-layer ownership tree with `SHARED_THREAD_LIKE` fate-sharing group
 * detection (`[TASK-AGENT-001]`, `[TASK-ISO-005]`) and CNCF Headlamp action pruning (`[UI-PARITY-001]`).
 */
export function enrichOwnershipTree(
  specs: readonly OwnershipTreeNodeSpec[],
  options?: {
    readonly callerCapabilities?: readonly string[];
    readonly readBacking?: ReadBackingToken | null;
    readonly locale?: WorkstationLocale;
    readonly disclosure?: DisclosureLevel;
  },
): readonly EnrichedOwnershipTreeNode[] {
  const locale: WorkstationLocale = options?.locale ?? "zh-CN";
  const disclosure: DisclosureLevel = options?.disclosure ?? "L1";
  const capabilities: readonly string[] = options?.callerCapabilities ?? [
    "control:read",
    "control:write",
    "control:destructive",
    "application:manage",
    "plan:edit",
    "cell:migrate",
  ];
  const readBacking: ReadBackingToken | null =
    options?.readBacking !== undefined
      ? options.readBacking
      : {
          targetIdHex: DEFAULT_READ_BACKING_HEX,
          expectedGenerationOrRevision: 1,
        };

  // Detect shared OS ProcessId across L9_AGENT_INSTANCE and L10_EXECUTION_FIBER
  const pidToSubUnits = new Map<number, string[]>();
  for (const spec of specs) {
    if (
      spec.hostPid !== undefined &&
      (spec.layer === "L9_AGENT_INSTANCE" || spec.layer === "L10_EXECUTION_FIBER")
    ) {
      const list = pidToSubUnits.get(spec.hostPid) ?? [];
      list.push(spec.nodeId);
      pidToSubUnits.set(spec.hostPid, list);
    }
  }

  const sharedPids = new Set<number>();
  for (const [pid, ids] of pidToSubUnits.entries()) {
    if (ids.length > 1) {
      sharedPids.add(pid);
    }
  }

  return specs.map((spec) => {
    const isSharedPid =
      spec.hostPid !== undefined && sharedPids.has(spec.hostPid);
    const allUnitsForPid =
      spec.hostPid !== undefined
        ? specs
            .filter((s) => s.hostPid === spec.hostPid && s.nodeId !== spec.nodeId)
            .map((s) => s.nodeId)
        : [];

    const prunerDomain = mapDomainToActionPrunerDomain(spec);
    const prunerResult = computeAllowedVerbs({
      domain: prunerDomain,
      state: spec.state,
      callerCapabilities: capabilities,
      readBacking,
      undoClass: spec.undoClass,
      statutoryLocks: spec.statutoryLocks,
      searchRestrictionsBits: spec.searchRestrictionsBits,
      hasActiveTasksForUninstall: spec.hasActiveTasksForUninstall,
      locale,
      disclosure,
    });

    return {
      ...spec,
      faultDomainPosture: isSharedPid ? "SHARED_THREAD_LIKE" : "ISOLATED_PROCESS",
      sharedFatePeerIds: isSharedPid ? allUnitsForPid : [],
      prunerResult,
    };
  });
}

/**
 * Windowed cursor projection (`O(visible_rows)`) respecting collapsed parent nodes and disclosure limits.
 */
export function windowOwnershipTreeRows(
  nodes: readonly EnrichedOwnershipTreeNode[],
  options?: {
    readonly collapsedNodeIds?: ReadonlySet<string>;
    readonly domainFilter?: ControlCenterDomainId | "all";
    readonly cursorOffset?: number;
    readonly limit?: number;
  },
): {
  readonly visibleRows: readonly EnrichedOwnershipTreeNode[];
  readonly totalMatchingCount: number;
  readonly hiddenCount: number;
} {
  const collapsed = options?.collapsedNodeIds ?? new Set<string>();
  const domainFilter = options?.domainFilter ?? "all";
  const offset = Math.max(0, options?.cursorOffset ?? 0);
  const limit = Math.max(1, options?.limit ?? 50);

  const nodeMap = new Map<string, EnrichedOwnershipTreeNode>();
  for (const n of nodes) {
    nodeMap.set(n.nodeId, n);
  }

  const isHiddenByCollapsedAncestor = (node: EnrichedOwnershipTreeNode): boolean => {
    let currParentId = node.parentNodeId;
    let guard = 0;
    while (currParentId !== null && guard < 16) {
      if (collapsed.has(currParentId)) {
        return true;
      }
      currParentId = nodeMap.get(currParentId)?.parentNodeId ?? null;
      guard += 1;
    }
    return false;
  };

  const filtered = nodes.filter((n) => {
    if (isHiddenByCollapsedAncestor(n)) {
      return false;
    }
    if (domainFilter !== "all" && n.domain !== domainFilter) {
      return false;
    }
    return true;
  });

  const visibleRows = filtered.slice(offset, offset + limit);
  return {
    visibleRows,
    totalMatchingCount: filtered.length,
    hiddenCount: Math.max(0, filtered.length - visibleRows.length),
  };
}

/**
 * Computes the 12-Domain summary cards from the enriched ownership tree.
 */
export function computeControlDomainSummaries(
  nodes: readonly EnrichedOwnershipTreeNode[],
): readonly ControlDomainSummaryCard[] {
  return ALL_CONTROL_CENTER_DOMAINS.map((domain) => {
    const domainNodes = nodes.filter((n) => n.domain === domain);
    const activeCount = domainNodes.filter((n) => {
      const s = n.state.toUpperCase();
      return s === "ACTIVE" || s === "RUNNING";
    }).length;
    const attentionCount = domainNodes.filter(
      (n) =>
        n.faultDomainPosture === "SHARED_THREAD_LIKE" ||
        n.watermark.pressureState !== "normal" ||
        n.state.toUpperCase() === "EFFECT_UNKNOWN" ||
        n.state.toUpperCase() === "QUARANTINED",
    ).length;
    const lockedCount = domainNodes.filter(
      (n) => n.prunerResult.statutoryLock !== null,
    ).length;

    const maxLive = domainNodes.reduce(
      (acc, n) => Math.max(acc, n.watermark.liveUsagePct),
      20,
    );
    const cpuSum = domainNodes.reduce((acc, n) => acc + n.watermark.cpuMillicores, 0);
    const memSum = domainNodes.reduce((acc, n) => acc + n.watermark.memoryMib, 0);

    return {
      domain,
      titles: DOMAIN_TITLES[domain],
      totalCount: domainNodes.length,
      activeCount,
      attentionCount,
      lockedCount,
      watermark: {
        guaranteedPct: 20,
        allocatedPct: 55,
        ceilingPct: 90,
        liveUsagePct: maxLive,
        pressureState:
          maxLive >= 80 ? "oom-risk" : maxLive >= 60 ? "throttled" : "normal",
        cpuMillicores: cpuSum,
        memoryMib: memSum,
      },
    };
  });
}

/**
 * Renders the 12-Domain summary cards grid HTML.
 */
export function renderDomainSummaryGridHtml(
  summaries: readonly ControlDomainSummaryCard[],
  options?: {
    readonly locale?: WorkstationLocale;
    readonly disclosure?: DisclosureLevel;
  },
): string {
  const locale: WorkstationLocale = options?.locale ?? "zh-CN";
  const disclosure: DisclosureLevel = options?.disclosure ?? "L1";

  const cardsHtml = summaries
    .map((card) => {
      const title = card.titles[locale];
      const domainTag =
        disclosure === "L3"
          ? ` <span class="nlos-l3-tag">[${escapeHtml(card.domain)}]</span>`
          : "";
      const barHtml = renderBulletMicroBarHtml({
        actualUsage: card.watermark.liveUsagePct,
        effectiveGrantedOrLeaseCap: card.watermark.allocatedPct,
        requestedCeiling: card.watermark.ceilingPct,
        unit: "%",
        locale,
        disclosure,
      });
      const statsText =
        locale === "zh-CN"
          ? `对象 ${card.totalCount} · 运行 ${card.activeCount} · 关注 ${card.attentionCount}`
          : `Total ${card.totalCount} · Active ${card.activeCount} · Alerts ${card.attentionCount}`;

      return (
        `<div class="nlos-domain-summary-card" data-domain="${escapeHtml(card.domain)}">` +
        `<div class="nlos-domain-summary-card__title">${escapeHtml(title)}${domainTag}</div>` +
        `<div class="nlos-domain-summary-card__stats">${escapeHtml(statsText)}</div>` +
        barHtml +
        `</div>`
      );
    })
    .join("");

  return `<section class="nlos-domain-summary-grid" role="region" aria-label="${
    locale === "zh-CN" ? "十二域系统控制概览" : "12-Domain System Control Overview"
  }">${cardsHtml}</section>`;
}

/**
 * Renders the System Informer 10-layer parent-child ownership tree HTML.
 */
export function renderOwnershipTreeHtml(
  nodes: readonly EnrichedOwnershipTreeNode[],
  options?: {
    readonly locale?: WorkstationLocale;
    readonly disclosure?: DisclosureLevel;
    readonly selectedNodeId?: string | null;
  },
): string {
  const locale: WorkstationLocale = options?.locale ?? "zh-CN";
  const disclosure: DisclosureLevel = options?.disclosure ?? "L1";
  const selectedId = options?.selectedNodeId ?? null;

  const rowsHtml = nodes
    .map((node) => {
      const indentPx = node.depth * 14;
      const layerBadge = formatLayerBadgeLabel(node.layer, locale, disclosure);
      const statusHtml = renderStatusIndicatorHtml({
        state: mapNodeStateToWorkstationStatus(node.state),
        locale,
        disclosure,
      });

      let faultBadgeHtml = "";
      if (node.faultDomainPosture === "SHARED_THREAD_LIKE") {
        const badgeText =
          disclosure === "L3"
            ? locale === "zh-CN"
              ? `同进程共享故障域 [SHARED_THREAD_LIKE · PID ${node.hostPid ?? 0} · ${node.l3DiagnosticTag}]`
              : `Shared-Process Fate Group [SHARED_THREAD_LIKE · PID ${node.hostPid ?? 0} · ${node.l3DiagnosticTag}]`
            : locale === "zh-CN"
              ? `同进程共享故障域 (PID ${node.hostPid ?? 0})`
              : `Shared-Process Fate Group (PID ${node.hostPid ?? 0})`;
        faultBadgeHtml = `<span class="nlos-fault-domain-badge" data-fault-domain="SHARED_THREAD_LIKE">${escapeHtml(
          badgeText,
        )}</span>`;
      }

      const lockBadgeHtml =
        node.prunerResult.statutoryLock !== null
          ? renderStatutoryLockBadgeHtml({
              kind: node.prunerResult.statutoryLock,
              locale,
              disclosure,
            })
          : "";

      const barHtml = renderBulletMicroBarHtml({
        actualUsage: node.watermark.liveUsagePct,
        effectiveGrantedOrLeaseCap: node.watermark.allocatedPct,
        requestedCeiling: node.watermark.ceilingPct,
        unit: "%",
        locale,
        disclosure,
      });

      const actionsHtml = node.prunerResult.allowedVerbs
        .map((verb) => {
          const isArming =
            node.prunerResult.requiresTwoStepArmingVerbs.includes(verb);
          const btnClass = isArming
            ? "nlos-control-center-btn nlos-control-center-btn--danger"
            : "nlos-control-center-btn";
          return `<button type="button" class="${btnClass}" data-node-id="${escapeHtml(
            node.nodeId,
          )}" data-verb="${escapeHtml(verb)}" data-requires-arming="${
            isArming ? "true" : "false"
          }">${escapeHtml(formatVerbLabel(verb, locale, disclosure))}</button>`;
        })
        .join("");

      const readOnlyNoteHtml =
        node.prunerResult.readOnlyReason !== null
          ? `<span class="nlos-ownership-tree__readonly-reason">${escapeHtml(
              node.prunerResult.readOnlyReason,
            )}</span>`
          : "";

      const l3DiagHtml =
        disclosure === "L3"
          ? `<span class="nlos-ownership-tree__l3-id">[${escapeHtml(
              node.nodeId,
            )} · ${escapeHtml(node.l3DiagnosticTag)}]</span>`
          : "";

      const selectedClass =
        selectedId === node.nodeId ? " nlos-ownership-tree__row--selected" : "";

      return (
        `<div class="nlos-ownership-tree__row${selectedClass}" role="treeitem" aria-level="${
          node.depth + 1
        }" data-node-id="${escapeHtml(node.nodeId)}" data-domain="${escapeHtml(
          node.domain,
        )}" data-layer="${escapeHtml(node.layer)}" data-depth="${node.depth}">` +
        `<div class="nlos-ownership-tree__indent" style="padding-left: ${indentPx}px;">` +
        `<span class="nlos-ownership-tree__layer-tag">${escapeHtml(layerBadge)}</span>` +
        `<strong class="nlos-ownership-tree__label">${escapeHtml(
          node.labels[locale],
        )}</strong>` +
        l3DiagHtml +
        faultBadgeHtml +
        `</div>` +
        `<div class="nlos-ownership-tree__status">${statusHtml}</div>` +
        `<div class="nlos-ownership-tree__telemetry">${barHtml}</div>` +
        `<div class="nlos-ownership-tree__actions">${lockBadgeHtml}${actionsHtml}${readOnlyNoteHtml}</div>` +
        `</div>`
      );
    })
    .join("");

  return `<div class="nlos-ownership-tree" role="tree" aria-label="${
    locale === "zh-CN" ? "十层父子归属资源树" : "10-Layer Ownership & Resource Tree"
  }">${rowsHtml}</div>`;
}

export function createOwnershipTreeElement(
  nodes: readonly EnrichedOwnershipTreeNode[],
  options?: {
    readonly locale?: WorkstationLocale;
    readonly disclosure?: DisclosureLevel;
    readonly selectedNodeId?: string | null;
  },
): HTMLElement {
  const wrapper = document.createElement("div");
  wrapper.innerHTML = renderOwnershipTreeHtml(nodes, options);
  return (wrapper.firstElementChild as HTMLElement) ?? wrapper;
}

/**
 * Default deterministic 10-layer / 12-domain workstation fixture for `TaskManagerSurface`
 * and `ResourceMonitorSurface`.
 */
export function createSeededOwnershipTreeSpecs(): readonly OwnershipTreeNodeSpec[] {
  return [
    {
      nodeId: "node-sys-workstation-01",
      parentNodeId: null,
      domain: "system-device",
      layer: "L1_SYSTEM_DEVICE",
      depth: 0,
      labels: {
        "zh-CN": "本地工作站主控节点",
        "en-US": "Primary Workstation Host",
      },
      state: "ACTIVE",
      watermark: {
        guaranteedPct: 25,
        allocatedPct: 55,
        ceilingPct: 90,
        liveUsagePct: 48,
        pressureState: "normal",
        cpuMillicores: 3200,
        memoryMib: 6144,
      },
      l3DiagnosticTag: "SYS-DEV-001",
    },
    {
      nodeId: "node-ws-prod-studio",
      parentNodeId: "node-sys-workstation-01",
      domain: "workspace",
      layer: "L2_WORKSPACE_TENANT",
      depth: 1,
      labels: {
        "zh-CN": "生产研发工作区",
        "en-US": "Production Engineering Workspace",
      },
      state: "ACTIVE",
      watermark: {
        guaranteedPct: 20,
        allocatedPct: 50,
        ceilingPct: 85,
        liveUsagePct: 44,
        pressureState: "normal",
        cpuMillicores: 2800,
        memoryMib: 5120,
      },
      l3DiagnosticTag: "WS-STUDIO-001",
    },
    {
      nodeId: "node-app-code-analyzer",
      parentNodeId: "node-ws-prod-studio",
      domain: "application",
      layer: "L3_APPLICATION",
      depth: 2,
      labels: {
        "zh-CN": "静态审计与构建套件",
        "en-US": "Static Audit & Build Suite",
      },
      state: "ACTIVE",
      watermark: {
        guaranteedPct: 20,
        allocatedPct: 48,
        ceilingPct: 80,
        liveUsagePct: 42,
        pressureState: "normal",
        cpuMillicores: 2100,
        memoryMib: 3584,
      },
      l3DiagnosticTag: "APP-AUDIT-001",
    },
    {
      nodeId: "node-task-release-verify",
      parentNodeId: "node-app-code-analyzer",
      domain: "task",
      layer: "L4_TASK",
      depth: 3,
      labels: {
        "zh-CN": "版本发布合规验证任务",
        "en-US": "Release Compliance Verification Task",
      },
      state: "RUNNING",
      undoClass: "reversible",
      watermark: {
        guaranteedPct: 20,
        allocatedPct: 45,
        ceilingPct: 80,
        liveUsagePct: 40,
        pressureState: "normal",
        cpuMillicores: 1600,
        memoryMib: 2560,
      },
      l3DiagnosticTag: "TASK-VERIFY-001",
    },
    {
      nodeId: "node-task-external-publish",
      parentNodeId: "node-app-code-analyzer",
      domain: "task",
      layer: "L4_TASK",
      depth: 3,
      labels: {
        "zh-CN": "外部镜像签名发布任务",
        "en-US": "External Image Signed Publish Task",
      },
      state: "COMPLETED",
      undoClass: "irreversible",
      watermark: {
        guaranteedPct: 10,
        allocatedPct: 20,
        ceilingPct: 40,
        liveUsagePct: 12,
        pressureState: "normal",
        cpuMillicores: 200,
        memoryMib: 512,
      },
      l3DiagnosticTag: "RCPT-UNDO-001",
    },
    {
      nodeId: "node-group-static-gates",
      parentNodeId: "node-task-release-verify",
      domain: "task-group",
      layer: "L5_TASK_GROUP",
      depth: 4,
      labels: {
        "zh-CN": "多车道静态门禁任务组",
        "en-US": "Multi-Lane Static Gate Group",
      },
      state: "RUNNING",
      watermark: {
        guaranteedPct: 20,
        allocatedPct: 45,
        ceilingPct: 80,
        liveUsagePct: 39,
        pressureState: "normal",
        cpuMillicores: 1400,
        memoryMib: 2048,
      },
      l3DiagnosticTag: "TASK-GRP-001",
    },
    {
      nodeId: "node-tasknode-ast-scan",
      parentNodeId: "node-group-static-gates",
      domain: "task-node",
      layer: "L6_TASK_NODE",
      depth: 5,
      labels: {
        "zh-CN": "语法树与依赖扫描步骤",
        "en-US": "AST & Dependency Scan Step",
      },
      state: "ACTIVE",
      watermark: {
        guaranteedPct: 20,
        allocatedPct: 45,
        ceilingPct: 80,
        liveUsagePct: 38,
        pressureState: "normal",
        cpuMillicores: 1200,
        memoryMib: 1792,
      },
      l3DiagnosticTag: "TASK-GRAPH-002",
    },
    {
      nodeId: "node-attempt-ast-01",
      parentNodeId: "node-tasknode-ast-scan",
      domain: "task-attempt",
      layer: "L7_TASK_ATTEMPT",
      depth: 6,
      labels: {
        "zh-CN": "扫描执行运行记录 #1",
        "en-US": "Scan Execution Run #1",
      },
      state: "RUNNING",
      watermark: {
        guaranteedPct: 20,
        allocatedPct: 45,
        ceilingPct: 80,
        liveUsagePct: 37,
        pressureState: "normal",
        cpuMillicores: 1100,
        memoryMib: 1536,
      },
      l3DiagnosticTag: "TASK-COMMIT-001",
    },
    {
      nodeId: "node-proc-worker-4820",
      parentNodeId: "node-attempt-ast-01",
      domain: "execution-unit",
      layer: "L8_PROCESS",
      depth: 7,
      labels: {
        "zh-CN": "分析器工作进程 (PID 4820)",
        "en-US": "Analyzer Worker Process (PID 4820)",
      },
      state: "RUNNING",
      hostPid: 4820,
      linkedCellId: "cell-local-primary",
      watermark: {
        guaranteedPct: 25,
        allocatedPct: 60,
        ceilingPct: 88,
        liveUsagePct: 64,
        pressureState: "throttled",
        cpuMillicores: 950,
        memoryMib: 1280,
      },
      l3DiagnosticTag: "EXEC-PROC-4820",
    },
    {
      nodeId: "node-agent-scanner-a",
      parentNodeId: "node-proc-worker-4820",
      domain: "execution-unit",
      layer: "L9_AGENT_INSTANCE",
      depth: 8,
      labels: {
        "zh-CN": "代码扫描协作实例 A",
        "en-US": "Code Scanner Worker A",
      },
      state: "RUNNING",
      hostPid: 4820,
      watermark: {
        guaranteedPct: 15,
        allocatedPct: 35,
        ceilingPct: 70,
        liveUsagePct: 34,
        pressureState: "normal",
        cpuMillicores: 500,
        memoryMib: 640,
      },
      l3DiagnosticTag: "TASK-AGENT-001",
    },
    {
      nodeId: "node-agent-scanner-b",
      parentNodeId: "node-proc-worker-4820",
      domain: "execution-unit",
      layer: "L9_AGENT_INSTANCE",
      depth: 8,
      labels: {
        "zh-CN": "许可证核验协作实例 B",
        "en-US": "License Verifier Worker B",
      },
      state: "RUNNING",
      hostPid: 4820,
      watermark: {
        guaranteedPct: 15,
        allocatedPct: 30,
        ceilingPct: 65,
        liveUsagePct: 30,
        pressureState: "normal",
        cpuMillicores: 450,
        memoryMib: 640,
      },
      l3DiagnosticTag: "TASK-ISO-005",
    },
    {
      nodeId: "node-fiber-ast-parser-01",
      parentNodeId: "node-agent-scanner-a",
      domain: "execution-unit",
      layer: "L10_EXECUTION_FIBER",
      depth: 9,
      labels: {
        "zh-CN": "语法树解析并发流 #01",
        "en-US": "AST Parser Stream #01",
      },
      state: "RUNNING",
      hostPid: 4820,
      watermark: {
        guaranteedPct: 10,
        allocatedPct: 25,
        ceilingPct: 50,
        liveUsagePct: 22,
        pressureState: "normal",
        cpuMillicores: 300,
        memoryMib: 384,
      },
      l3DiagnosticTag: "FIBER-AST-01",
    },
    {
      nodeId: "node-resource-group-main",
      parentNodeId: "node-ws-prod-studio",
      domain: "resource-group",
      layer: "L3_APPLICATION",
      depth: 2,
      labels: {
        "zh-CN": "构建计算资源保障池",
        "en-US": "Build Compute Resource Pool",
      },
      state: "ACTIVE",
      watermark: {
        guaranteedPct: 30,
        allocatedPct: 65,
        ceilingPct: 92,
        liveUsagePct: 58,
        pressureState: "normal",
        cpuMillicores: 2400,
        memoryMib: 4096,
      },
      l3DiagnosticTag: "RES-POOL-001",
    },
    {
      nodeId: "node-topic-telemetry",
      parentNodeId: "node-ws-prod-studio",
      domain: "topic-channel",
      layer: "L3_APPLICATION",
      depth: 2,
      labels: {
        "zh-CN": "编译诊断事件通道",
        "en-US": "Build Diagnostic Event Topic",
      },
      state: "ACTIVE",
      watermark: {
        guaranteedPct: 15,
        allocatedPct: 40,
        ceilingPct: 85,
        liveUsagePct: 52,
        pressureState: "normal",
        cpuMillicores: 320,
        memoryMib: 512,
      },
      l3DiagnosticTag: "TOPIC-DIAG-001",
    },
    {
      nodeId: "node-op-ext-webhook",
      parentNodeId: "node-attempt-ast-01",
      domain: "operation",
      layer: "L8_PROCESS",
      depth: 7,
      labels: {
        "zh-CN": "外部合规网关回执操作",
        "en-US": "External Compliance Webhook Operation",
      },
      state: "EFFECT_UNKNOWN",
      watermark: {
        guaranteedPct: 10,
        allocatedPct: 25,
        ceilingPct: 60,
        liveUsagePct: 18,
        pressureState: "normal",
        cpuMillicores: 120,
        memoryMib: 192,
      },
      l3DiagnosticTag: "TASK-EFFECT-003",
    },
    {
      nodeId: "node-cell-local-primary",
      parentNodeId: "node-sys-workstation-01",
      domain: "cell",
      layer: "L2_WORKSPACE_TENANT",
      depth: 1,
      labels: {
        "zh-CN": "本地主控计算节点",
        "en-US": "Primary Local Compute Node",
      },
      state: "ACTIVE",
      watermark: {
        guaranteedPct: 25,
        allocatedPct: 55,
        ceilingPct: 90,
        liveUsagePct: 46,
        pressureState: "normal",
        cpuMillicores: 2200,
        memoryMib: 4096,
      },
      l3DiagnosticTag: "CELL-PRIMARY-001",
    },
    {
      nodeId: "node-cell-quarantined-edge",
      parentNodeId: "node-sys-workstation-01",
      domain: "cell",
      layer: "L2_WORKSPACE_TENANT",
      depth: 1,
      labels: {
        "zh-CN": "隔离观察计算节点",
        "en-US": "Isolated Edge Compute Node",
      },
      state: "QUARANTINED",
      watermark: {
        guaranteedPct: 20,
        allocatedPct: 75,
        ceilingPct: 95,
        liveUsagePct: 82,
        pressureState: "oom-risk",
        cpuMillicores: 800,
        memoryMib: 1536,
      },
      l3DiagnosticTag: "LEASE-TTL-001",
    },
    {
      nodeId: "node-search-0x1f-candidate",
      parentNodeId: "node-ws-prod-studio",
      domain: "task",
      prunerDomain: "search",
      layer: "L4_TASK",
      depth: 3,
      labels: {
        "zh-CN": "搜索候选：历史发布审计记录",
        "en-US": "Search Result: Historical Release Audit",
      },
      state: "COMPLETED",
      searchRestrictionsBits: 0x1f,
      watermark: {
        guaranteedPct: 5,
        allocatedPct: 10,
        ceilingPct: 20,
        liveUsagePct: 5,
        pressureState: "normal",
        cpuMillicores: 50,
        memoryMib: 64,
      },
      l3DiagnosticTag: "CONS-CTRL-001",
    },
  ];
}
