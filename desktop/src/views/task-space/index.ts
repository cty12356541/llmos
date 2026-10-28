// Stage D Task Space (`z1-task-space`) — `L1` Commitment Table, `L2` Non-Modal Attribution Drawer,
// and `L3` Dagster Orthogonal `TaskPlan` DAG Override Editor & Temporal `TaskAttempt` Swimlanes.
// Authority: Stage D Visual & UX Spec §2A, §3.2 (`[ROAD-D-001]`, `[TASK-SPACE-001]`, `[TASK-VIS-001]`,
// `[TASK-REOPEN-001]`, `[RCPT-UNDO-001]`, `[RCPT-PARTIAL-001]`, `[PLAN-CHANGE-001]`, `[PLAN-OVERRIDE-001]`,
// `[PLAN-LAZY-001]`, `[TASK-SPEC-001]`, `[TASK-PROVIDER-001]`, `[TASK-AGENT-001]`, `[TASK-ISO-005]`),
// ADR-0025, `docs/management/stage-d-progress.md` §D.3.2 (`TASK_SPACE_GAPS` 1-3 closure).

import {
  inspectApplication,
  inspectExecutionFiber,
  inspectHealth,
  inspectOperation,
  inspectProcess,
  inspectResourceCost,
  inspectResourceHealth,
  inspectSemanticHealth,
  inspectTask,
  inspectTaskGroup,
  inspectTaskNode,
  inspectTopic,
} from "../../ipc";
import { computeAllowedVerbs } from "../../store/action-pruner";
import {
  createDisclosureModel,
  createTaskSpaceLazyCursorCoordinator,
  type DisclosureKeydownEvent,
  type DisclosureLevel,
  type DisclosureModel,
  type TaskSpaceDisclosurePullPlan,
  type TaskSpaceLazyCursorCoordinator,
} from "../../store/disclosure-model";
import type { ReceiptDto } from "../../types";
import { renderBulletMicroBarHtml } from "../../ui/bullet-micro-bar";
import {
  filterBreadcrumbsForDisclosure,
  renderInspectorDrawerHtml,
  type ScopeBreadcrumbItem,
} from "../../ui/inspector-drawer";
import { renderReceiptAuditBarHtml } from "../../ui/receipt-audit-bar";
import {
  CANONICAL_TASK_STATES,
  pruneVerbsAgainstStatutoryLock,
  renderStatutoryLockBadgeHtml,
  renderStatusIndicatorHtml,
  type CanonicalTaskState,
  type WorkstationLocale,
  type WorkstationStatusState,
} from "../../ui/status-indicator";
import {
  attemptManualCommitOverride,
  executeTaskAttemptControlAction,
  renderTaskAttemptSwimlanesHtml,
  type TaskAttemptControlOutcome,
  type TaskAttemptControlRejection,
  type TaskAttemptSwimlanesSnapshot,
} from "./attempt-lanes";
import {
  createTaskPlanDagCanvasController,
  formatLazySupernodeBadgeText,
  renderTaskPlanDagCanvasHtml,
  type PlanOverrideClassificationOutcome,
  type PlanOverrideOperation,
  type PlanOverridePreviewResult,
  type PlanOverrideRejection,
  type PlanOverrideSubmissionOutcome,
  type TaskPlanDagCanvasController,
  type TaskPlanDagSnapshot,
  type Z4ReauthorizationGrant,
} from "./dag-canvas";
import { el, fieldRow, labeledInput, showError } from "./dom";
import type { DomainScan, RecoveryDomain, TaskRow } from "./model";
import { rebuildRows, scanFromReceipt } from "./model";
import { receiptFooter, renderOutcome, renderReceipt } from "./receipts";

export * from "./attempt-lanes";
export * from "./dag-canvas";

export type TaskUndoClass = "REVERSIBLE" | "COMPENSATABLE" | "IRREVERSIBLE";
export type InspectionLossReason = "NOT_FOUND" | "STALE_GENERATION";

export interface TaskSummaryProjection {
  readonly taskId: string;
  readonly planId: string;
  readonly applicationId: string;
  readonly installationGeneration: number;
  readonly planRevision: number;
  readonly taskState: CanonicalTaskState;
  readonly undoClass: TaskUndoClass;
  readonly reservationId: string;
  readonly cellId: string;
  readonly requestedUnits: number;
  readonly effectiveUnits: number;
  readonly usageUnits: number;
  readonly succeededTargets: number;
  readonly failedTargets: number;
  readonly receiptHex: string;
}

/**
 * Stage D §D.3.2 Closure of `TASK_SPACE_GAPS` (1–3):
 * All three historical Stage B/C read-only Task Space gaps have been closed by `IPC-D1..D3`
 * (`listTasks`, `listTaskGroups`, `listTaskNodes`, `listTaskAttempts`, `listExecutionFibers`,
 * `listOperations`, `TaskSummaryProjection` carrying `applicationId` / `installationGeneration` /
 * `planRevision`, and `NOT_FOUND` vs `STALE_GENERATION` carrying `currentGeneration`).
 */
export const TASK_SPACE_GAPS: ReadonlyArray<{
  readonly fact: string;
  readonly detail: string;
}> = [];

export interface ClosedTaskSpaceGapRecord {
  readonly id: "GAP-1" | "GAP-2" | "GAP-3";
  readonly fact: string;
  readonly closedByIpcSurface: string;
  readonly verificationDetail: string;
}

export const CLOSED_TASK_SPACE_GAPS: readonly ClosedTaskSpaceGapRecord[] = [
  {
    id: "GAP-1",
    fact: "任务/TaskGroup 全量游标枚举面 (`list_tasks`, `list_task_groups`, `list_task_nodes`, `list_task_attempts`)",
    closedByIpcSurface:
      "IPC-D1/D2: listTasks / listTaskGroups / listTaskNodes / listTaskAttempts (limit <= 64)",
    verificationDetail:
      "TaskSpace L1/L2/L3 按需游标拉取全部 16 种权威 TaskState 任务行与子图，不再仅靠三域 escalated 告警行。",
  },
  {
    id: "GAP-2",
    fact: "W29-A 关联字段投影 (`applicationId`, `installationGeneration`, `planRevision`, `undoClass`, `reservationId`, `cellId`)",
    closedByIpcSurface:
      "IPC-D1: TaskSummaryProjection (applicationId, installationGeneration, planRevision, taskState, undoClass, reservationId, cellId)",
    verificationDetail:
      "L1 呈现所属应用与 Nomad 3-水位微条/撤销能力，L3 呈现完整 applicationId + installationGeneration + planRevision 元组。",
  },
  {
    id: "GAP-3",
    fact: "ExecutionFiber / DurableOperation 枚举面与 `NOT_FOUND` vs `STALE_GENERATION` 区分",
    closedByIpcSurface:
      "IPC-D1/D2: listExecutionFibers / listOperations + InspectionLossReason (NOT_FOUND vs STALE_GENERATION + currentGeneration)",
    verificationDetail:
      "L3 泳道按父 TaskAttempt 枚举 ExecutionFiber 与 DurableOperation，并在代数漂移时精确报告 STALE_GENERATION (含 currentGeneration)。",
  },
];

export interface TaskTargetPartialItem {
  readonly targetId: string;
  readonly targetLabelZh: string;
  readonly targetLabelEn: string;
  readonly succeeded: boolean;
  readonly failureReasonZh?: string;
  readonly failureReasonEn?: string;
}

export interface TaskPhaseMilestoneSummary {
  readonly currentPhaseIndex: number;
  readonly totalPhases: number;
  readonly milestoneTitleZh: string;
  readonly milestoneTitleEn: string;
  readonly foldedToolStepCount: number;
}

export interface TaskL2AttributionDetail {
  readonly waitingReasonZh: string;
  readonly waitingReasonEn: string;
  readonly stepProgressItems: readonly {
    readonly stepId: string;
    readonly titleZh: string;
    readonly titleEn: string;
    readonly state: WorkstationStatusState;
    readonly dependencyTitlesZh: readonly string[];
    readonly dependencyTitlesEn: readonly string[];
    readonly collapsedUnmaterializedCount?: number;
  }[];
  readonly resourceBreakdown: readonly {
    readonly resourceLabelZh: string;
    readonly resourceLabelEn: string;
    readonly usageUnits: number;
    readonly effectiveUnits: number;
    readonly requestedUnits: number;
  }[];
  readonly partialTargets: readonly TaskTargetPartialItem[];
}

export interface TaskSpaceWorkstationRow {
  readonly summary: TaskSummaryProjection;
  readonly goalTitleZh: string;
  readonly goalTitleEn: string;
  readonly applicationNameZh: string;
  readonly applicationNameEn: string;
  readonly groupNameZh: string;
  readonly groupNameEn: string;
  readonly phaseFold: TaskPhaseMilestoneSummary;
  readonly l2Attribution: TaskL2AttributionDetail;
  readonly l3PlanDag: TaskPlanDagSnapshot;
  readonly l3AttemptLanes: TaskAttemptSwimlanesSnapshot;
  readonly latestAttemptIndex: number;
  readonly parentTaskId: string | null;
  readonly layerLossDiagnostic?: {
    readonly reason: InspectionLossReason;
    readonly currentGeneration: number | null;
  } | null;
}

export type TerminalTaskReopenMode = "retry" | "refine";

export interface TerminalTaskReopenOutcome {
  readonly ok: boolean;
  readonly mode: TerminalTaskReopenMode;
  readonly sourceTaskId: string;
  readonly resultingTaskId: string;
  readonly spawnedAttemptId: string | null;
  readonly newInstallationGeneration: number;
  readonly newPlanRevision: number;
  readonly retryFenceEpoch: number;
  readonly preservedHistoryTaskIds: readonly string[];
  readonly receiptHex: string;
  readonly errorCode?: "TASK_NOT_FOUND" | "TASK_NOT_TERMINAL_OR_PARTIAL";
  readonly messageZh: string;
  readonly messageEn: string;
}

const TERMINAL_OR_REOPENABLE_STATES: ReadonlySet<CanonicalTaskState> =
  new Set<CanonicalTaskState>([
    "FAILED",
    "CANCELLED",
    "DISPUTED",
    "COMPLETED",
    "PARTIAL",
  ]);

function escapeHtml(value: string): string {
  return value
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

function deterministicHex(seed: string): string {
  let h1 = 0x811c9dc5;
  let h2 = 0x01000193;
  for (let i = 0; i < seed.length; i += 1) {
    const ch = seed.charCodeAt(i);
    h1 ^= ch;
    h1 = Math.imul(h1, 0x01000193) >>> 0;
    h2 ^= (ch + i) & 0xff;
    h2 = Math.imul(h2, 0x85ebca6b) >>> 0;
  }
  const p1 = h1.toString(16).padStart(8, "0");
  const p2 = h2.toString(16).padStart(8, "0");
  return `${p1}${p2}${p1}${p2}${p1}${p2}${p1}${p2}`;
}

/**
 * Formats Temporal-style folded phase milestone text for `L1` task rows (`[TASK-VIS-001]`),
 * folding noisy tool/reasoning sub-steps into a clean phase summary.
 */
export function renderTemporalPhaseSummaryText(
  phase: TaskPhaseMilestoneSummary,
  locale: WorkstationLocale = "zh-CN",
): string {
  if (locale === "en-US") {
    return `Phase ${phase.currentPhaseIndex}/${phase.totalPhases} · ${phase.milestoneTitleEn} (${phase.foldedToolStepCount} sub-steps folded)`;
  }
  return `阶段 ${phase.currentPhaseIndex}/${phase.totalPhases} · ${phase.milestoneTitleZh}（已折叠 ${phase.foldedToolStepCount} 个子步骤）`;
}

/**
 * Formats the `L3` layer inspection loss banner distinguishing `NOT_FOUND` vs `STALE_GENERATION`
 * (`GAP-3` closure).
 */
export function formatLayerInspectionDiagnosticBanner(
  reason: InspectionLossReason,
  currentGeneration: number | null,
  locale: WorkstationLocale = "zh-CN",
): string {
  if (reason === "STALE_GENERATION") {
    const genLabel =
      currentGeneration !== null ? String(currentGeneration) : "unknown";
    return locale === "en-US"
      ? `STALE_GENERATION · Requested generation is superseded (current_generation=${genLabel})`
      : `STALE_GENERATION · 请求代数已过期（最新 current_generation=${genLabel}）`;
  }
  return locale === "en-US"
    ? "NOT_FOUND · Target authority row does not exist in store"
    : "NOT_FOUND · 权威存储中不存在目标对象行";
}

/**
 * Implements `[TASK-REOPEN-001]` terminal task reopen semantics:
 * - `mode === "retry"`: spawns a new `TaskAttempt` (`latestAttemptIndex + 1`, incremented `retryFenceEpoch`)
 *   under the same `Task` while keeping all prior attempts and receipts immutable.
 * - `mode === "refine"`: spawns a new `Task` generation (`installationGeneration + 1`, `planRevision + 1`)
 *   linked via `parentTaskId` while preserving the original task's historical receipt chain.
 * Never overwrites history in-place.
 */
export function reopenTerminalTaskInModel(
  rows: readonly TaskSpaceWorkstationRow[],
  taskId: string,
  mode: TerminalTaskReopenMode,
  refinementGoalZh?: string,
  refinementGoalEn?: string,
): {
  readonly outcome: TerminalTaskReopenOutcome;
  readonly updatedRows: readonly TaskSpaceWorkstationRow[];
} {
  const target = rows.find((r) => r.summary.taskId === taskId);
  if (target === undefined) {
    return {
      outcome: {
        ok: false,
        mode,
        sourceTaskId: taskId,
        resultingTaskId: taskId,
        spawnedAttemptId: null,
        newInstallationGeneration: 0,
        newPlanRevision: 0,
        retryFenceEpoch: 0,
        preservedHistoryTaskIds: rows.map((r) => r.summary.taskId),
        receiptHex: "",
        errorCode: "TASK_NOT_FOUND",
        messageZh: `未找到任务 ${taskId}`,
        messageEn: `Task ${taskId} was not found`,
      },
      updatedRows: rows,
    };
  }

  const hasPartialFailure = target.summary.failedTargets > 0;
  if (
    !TERMINAL_OR_REOPENABLE_STATES.has(target.summary.taskState) &&
    !hasPartialFailure
  ) {
    return {
      outcome: {
        ok: false,
        mode,
        sourceTaskId: taskId,
        resultingTaskId: taskId,
        spawnedAttemptId: null,
        newInstallationGeneration: target.summary.installationGeneration,
        newPlanRevision: target.summary.planRevision,
        retryFenceEpoch: 0,
        preservedHistoryTaskIds: rows.map((r) => r.summary.taskId),
        receiptHex: "",
        errorCode: "TASK_NOT_TERMINAL_OR_PARTIAL",
        messageZh: "仅终态或含部分失败的任务可执行重试或微调重开",
        messageEn:
          "Only terminal or partially failed tasks can be reopened via retry or refine",
      },
      updatedRows: rows,
    };
  }

  if (mode === "retry") {
    const nextAttemptIdx = target.latestAttemptIndex + 1;
    const spawnedAttemptId = `${target.summary.taskId}-attempt-${nextAttemptIdx}`;
    const prevMaxFence = target.l3AttemptLanes.attempts.reduce(
      (max, a) => Math.max(max, a.retryFenceEpoch),
      0,
    );
    const nextRetryFenceEpoch = prevMaxFence + 1;
    const receiptHex = deterministicHex(
      `rcpt:reopen:retry:${target.summary.taskId}:${spawnedAttemptId}:fence${nextRetryFenceEpoch}`,
    );

    const newAttempt = {
      attemptId: spawnedAttemptId,
      taskNodeId: target.l3AttemptLanes.taskNodeId,
      attemptIndex: nextAttemptIdx,
      label: `Attempt #${nextAttemptIdx} (Retry)`,
      state: "ACTIVE" as const,
      inputSnapshotDigestHex: deterministicHex(
        `input:${spawnedAttemptId}:fence${nextRetryFenceEpoch}`,
      ),
      isolationDomainIdHex: deterministicHex(`iso:${spawnedAttemptId}`).slice(
        0,
        32,
      ),
      isolationEnforcementProfile: "STRICT_SANDBOX" as const,
      providerIsolationUnverified: false,
      startedAtOffsetMs: 120,
      durationMs: 45,
      casWinner: false,
      superseded: false,
      retryFenceEpoch: nextRetryFenceEpoch,
      fibers: [
        {
          fiberId: `${spawnedAttemptId}-fiber-1`,
          attemptId: spawnedAttemptId,
          processId: `proc-${spawnedAttemptId}`,
          label: "Retry Worker Fiber",
          sharedThreadLike: false,
          timings: {
            activeCpuMs: 25,
            schedulerWaitMs: 5,
            externalWaitMs: 10,
            backpressureWaitMs: 5,
            suspendedMs: 0,
          },
        },
      ],
      operations: [],
      cellPlacement: {
        cellIdHex: target.summary.cellId,
        leaseEpoch: nextRetryFenceEpoch,
        nodeBootGeneration: 1,
        placementMode: "LOCAL_SINGLE_NODE" as const,
        fencingTokenHex: deterministicHex(`fence:${spawnedAttemptId}`).slice(
          0,
          32,
        ),
      },
      receiptHex,
    };

    const updatedRow: TaskSpaceWorkstationRow = {
      ...target,
      latestAttemptIndex: nextAttemptIdx,
      summary: {
        ...target.summary,
        taskState: "RUNNING",
        receiptHex,
      },
      l3AttemptLanes: {
        ...target.l3AttemptLanes,
        taskHeadCasEpoch: target.l3AttemptLanes.taskHeadCasEpoch + 1,
        attempts: [...target.l3AttemptLanes.attempts, newAttempt],
      },
    };

    const updatedRows = rows.map((r) =>
      r.summary.taskId === taskId ? updatedRow : r,
    );

    return {
      outcome: {
        ok: true,
        mode: "retry",
        sourceTaskId: taskId,
        resultingTaskId: taskId,
        spawnedAttemptId,
        newInstallationGeneration: target.summary.installationGeneration,
        newPlanRevision: target.summary.planRevision,
        retryFenceEpoch: nextRetryFenceEpoch,
        preservedHistoryTaskIds: updatedRows.map((r) => r.summary.taskId),
        receiptHex,
        messageZh: `已派生新执行尝试 (#${nextAttemptIdx})，历史尝试记录完整保留`,
        messageEn: `Spawned new retry attempt (#${nextAttemptIdx}) while preserving prior attempt history`,
      },
      updatedRows,
    };
  }

  // mode === "refine": spawn a new Task generation while keeping original row intact in history
  const nextGen = target.summary.installationGeneration + 1;
  const nextPlanRev = target.summary.planRevision + 1;
  const newTaskId = `${target.summary.taskId}-gen${nextGen}`;
  const newPlanId = `${target.summary.planId}-gen${nextGen}`;
  const receiptHex = deterministicHex(
    `rcpt:reopen:refine:${target.summary.taskId}->${newTaskId}:gen${nextGen}:r${nextPlanRev}`,
  );

  const refinedRow: TaskSpaceWorkstationRow = {
    ...target,
    parentTaskId: target.summary.taskId,
    latestAttemptIndex: 1,
    goalTitleZh:
      refinementGoalZh ?? `${target.goalTitleZh}（微调重开 #${nextGen}）`,
    goalTitleEn:
      refinementGoalEn ?? `${target.goalTitleEn} (Refined Gen #${nextGen})`,
    summary: {
      ...target.summary,
      taskId: newTaskId,
      planId: newPlanId,
      installationGeneration: nextGen,
      planRevision: nextPlanRev,
      taskState: "AUTHORIZED",
      failedTargets: 0,
      receiptHex,
    },
    l3PlanDag: {
      ...target.l3PlanDag,
      taskId: newTaskId,
      planId: newPlanId,
      planRevision: nextPlanRev,
      installationGeneration: nextGen,
      planDigestHex: deterministicHex(`plan_digest:${newPlanId}:r${nextPlanRev}`),
    },
    l3AttemptLanes: {
      ...target.l3AttemptLanes,
      taskId: newTaskId,
      planRevision: nextPlanRev,
      taskHeadCasEpoch: target.l3AttemptLanes.taskHeadCasEpoch + 1,
    },
  };

  const updatedRows = [...rows, refinedRow];
  return {
    outcome: {
      ok: true,
      mode: "refine",
      sourceTaskId: taskId,
      resultingTaskId: newTaskId,
      spawnedAttemptId: null,
      newInstallationGeneration: nextGen,
      newPlanRevision: nextPlanRev,
      retryFenceEpoch: target.l3AttemptLanes.taskHeadCasEpoch + 1,
      preservedHistoryTaskIds: updatedRows.map((r) => r.summary.taskId),
      receiptHex,
      messageZh: `已派生新任务世代 (${newTaskId}，第 ${nextGen} 代)，原任务历史完整保留`,
      messageEn: `Spawned new task generation (${newTaskId}, gen ${nextGen}) while preserving prior history`,
    },
    updatedRows,
  };
}

/**
 * Builds the default Stage D fixture dataset covering all 16 canonical `TaskState` values,
 * `[RCPT-UNDO-001]` (`REVERSIBLE`, `COMPENSATABLE`, `IRREVERSIBLE`), `[RCPT-PARTIAL-001]` partial failures,
 * `[PLAN-LAZY-001]` `+N 待物化步骤 · 0 进程` super-nodes, and `L3` `TaskAttempt` / `ExecutionFiber` lanes.
 */
export function createDefaultTaskSpaceWorkstationRows(): readonly TaskSpaceWorkstationRow[] {
  return CANONICAL_TASK_STATES.map((state, index) => {
    const num = index + 1;
    const taskId = `task-${num.toString().padStart(2, "0")}`;
    const planId = `plan-${num.toString().padStart(2, "0")}`;
    const undoClass: TaskUndoClass =
      index % 3 === 0
        ? "IRREVERSIBLE"
        : index % 3 === 1
          ? "COMPENSATABLE"
          : "REVERSIBLE";
    const hasPartial =
      state === "PARTIAL" || state === "FAILED" || state === "DISPUTED" || index === 2;
    const succeededTargets = hasPartial ? 4 : 5;
    const failedTargets = hasPartial ? 1 : 0;
    const requestedUnits = 100;
    const effectiveUnits = 80;
    const usageUnits = Math.min(95, 25 + ((index * 7) % 65));
    const receiptHex = deterministicHex(`rcpt:${taskId}:${state}`);

    const partialTargets: TaskTargetPartialItem[] = [
      {
        targetId: `${taskId}-target-1`,
        targetLabelZh: "主工作区配置文档",
        targetLabelEn: "Primary Workspace Config",
        succeeded: true,
      },
      {
        targetId: `${taskId}-target-2`,
        targetLabelZh: "构建产物索引表",
        targetLabelEn: "Build Artifact Index",
        succeeded: !hasPartial,
        failureReasonZh: hasPartial ? "目标文件被外部进程独占锁定" : undefined,
        failureReasonEn: hasPartial
          ? "Target file locked exclusively by external process"
          : undefined,
      },
    ];

    const dagSnapshot: TaskPlanDagSnapshot = {
      taskId,
      planId,
      planRevision: 3,
      installationGeneration: 2,
      planDigestHex: deterministicHex(`digest:${planId}:r3`),
      authorizedCapabilities: [
        "cap.fs.workspace.read",
        "cap.fs.workspace.write",
      ],
      authorizedBudgetCeilingUnits: 120,
      groups: [
        {
          groupId: `${planId}-grp-ingest`,
          title: "数据采集与校验组",
          topology: "DAG",
          schedulingOrder: 1,
          failurePolicy: "CONTINUE_ON_PARTIAL",
          cancellationScope: "GROUP_BOUNDED",
          runtimeExpanded: index % 2 === 0,
          attributedBudgetUnits: 60,
        },
        {
          groupId: `${planId}-grp-commit`,
          title: "归档与提交组",
          topology: "SEQUENTIAL",
          schedulingOrder: 2,
          failurePolicy: "FAIL_FAST",
          cancellationScope: "PLAN_WIDE",
          runtimeExpanded: false,
          attributedBudgetUnits: 40,
        },
      ],
      nodes: [
        {
          nodeId: `${planId}-node-completed`,
          groupId: `${planId}-grp-ingest`,
          title: "读取工作区基线清单",
          state: "COMPLETED",
          dependencyNodeIds: [],
          capabilityId: "cap.fs.workspace.read",
          requestedBudgetUnits: 20,
          effectiveBudgetUnits: 20,
          usageBudgetUnits: 18,
          residencyState: "HOT_MATERIALIZED",
          runtimeExpanded: false,
          assignedAttemptCount: 1,
        },
        {
          nodeId: `${planId}-node-active`,
          groupId: `${planId}-grp-ingest`,
          title: "执行差异归因分析",
          state: "ACTIVE",
          dependencyNodeIds: [`${planId}-node-completed`],
          capabilityId: "cap.fs.workspace.read",
          requestedBudgetUnits: 30,
          effectiveBudgetUnits: 25,
          usageBudgetUnits: 22,
          residencyState: "HOT_MATERIALIZED",
          runtimeExpanded: true,
          assignedAttemptCount: 2,
        },
        {
          nodeId: `${planId}-node-pending-a`,
          groupId: `${planId}-grp-commit`,
          title: "生成结构化阶段报告",
          state: "DECLARED",
          dependencyNodeIds: [`${planId}-node-active`],
          capabilityId: "cap.fs.workspace.write",
          requestedBudgetUnits: 25,
          effectiveBudgetUnits: 20,
          usageBudgetUnits: 0,
          residencyState: "HOT_MATERIALIZED",
          runtimeExpanded: false,
          assignedAttemptCount: 0,
        },
        {
          nodeId: `${planId}-node-pending-b`,
          groupId: `${planId}-grp-commit`,
          title: "写入归档回执摘要",
          state: "ELIGIBLE",
          dependencyNodeIds: [`${planId}-node-pending-a`],
          capabilityId: "cap.fs.workspace.write",
          requestedBudgetUnits: 20,
          effectiveBudgetUnits: 15,
          usageBudgetUnits: 0,
          residencyState: "HOT_MATERIALIZED",
          runtimeExpanded: false,
          assignedAttemptCount: 0,
        },
      ],
      lazySupernodes: [
        {
          supernodeId: `${planId}-supernode-cold`,
          groupId: `${planId}-grp-commit`,
          titleZh: "后续批量归档子图",
          titleEn: "Batch Archive Tail Subgraph",
          dependencyNodeIds: [`${planId}-node-pending-b`],
          unmaterializedStepCount: 18,
          allocatedProcessCount: 0,
          residencyState: "METADATA_ONLY",
          expanded: false,
          materializedNodes: [],
          nextCursorHex: null,
        },
      ],
    };

    const attemptLanes: TaskAttemptSwimlanesSnapshot = {
      taskId,
      taskNodeId: `${planId}-node-active`,
      planRevision: 3,
      winningAttemptId: `${taskId}-attempt-2`,
      taskHeadCasEpoch: 7,
      attempts: [
        {
          attemptId: `${taskId}-attempt-1`,
          taskNodeId: `${planId}-node-active`,
          attemptIndex: 1,
          label: "Attempt #1 (Speculative Candidate)",
          state: "SUPERSEDED",
          inputSnapshotDigestHex: deterministicHex(`snap:${taskId}:a1`),
          isolationDomainIdHex: deterministicHex(`iso:${taskId}:a1`).slice(
            0,
            32,
          ),
          isolationEnforcementProfile: "PROVIDER_EXTERNAL",
          providerIsolationUnverified: true,
          startedAtOffsetMs: 0,
          durationMs: 180,
          casWinner: false,
          superseded: true,
          retryFenceEpoch: 1,
          fibers: [
            {
              fiberId: `${taskId}-a1-fiber-1`,
              attemptId: `${taskId}-attempt-1`,
              processId: `proc-shared-${num}`,
              label: "External Provider Adapter Worker",
              sharedThreadLike: true,
              timings: {
                activeCpuMs: 40,
                schedulerWaitMs: 25,
                externalWaitMs: 95,
                backpressureWaitMs: 20,
                suspendedMs: 10,
              },
            },
          ],
          operations: [
            {
              operationId: `${taskId}-op-1`,
              attemptId: `${taskId}-attempt-1`,
              targetResourceUri: "workspace://staging/report-draft.json",
              effectState: "EFFECT_UNKNOWN",
              idempotencyKeyHex: deterministicHex(`idem:${taskId}:op1`).slice(
                0,
                32,
              ),
              fencingTokenEpoch: 1,
              receiptHex: deterministicHex(`rcpt:${taskId}:op1`),
            },
          ],
          cellPlacement: {
            cellIdHex: `cell-local-${num.toString().padStart(2, "0")}`,
            leaseEpoch: 4,
            nodeBootGeneration: 2,
            placementMode: "LOCAL_SINGLE_NODE",
            fencingTokenHex: deterministicHex(`fence:${taskId}:a1`).slice(
              0,
              32,
            ),
          },
          receiptHex: deterministicHex(`rcpt:${taskId}:attempt1`),
        },
        {
          attemptId: `${taskId}-attempt-2`,
          taskNodeId: `${planId}-node-active`,
          attemptIndex: 2,
          label: "Attempt #2 (Committed Winner)",
          state: "COMPLETED",
          inputSnapshotDigestHex: deterministicHex(`snap:${taskId}:a2`),
          isolationDomainIdHex: deterministicHex(`iso:${taskId}:a2`).slice(
            0,
            32,
          ),
          isolationEnforcementProfile: "STRICT_SANDBOX",
          providerIsolationUnverified: false,
          startedAtOffsetMs: 20,
          durationMs: 110,
          casWinner: true,
          superseded: false,
          retryFenceEpoch: 2,
          fibers: [
            {
              fiberId: `${taskId}-a2-fiber-1`,
              attemptId: `${taskId}-attempt-2`,
              processId: `proc-sandbox-${num}`,
              label: "Sandbox Verification Worker",
              sharedThreadLike: false,
              timings: {
                activeCpuMs: 65,
                schedulerWaitMs: 15,
                externalWaitMs: 20,
                backpressureWaitMs: 10,
                suspendedMs: 0,
              },
            },
          ],
          operations: [
            {
              operationId: `${taskId}-op-2`,
              attemptId: `${taskId}-attempt-2`,
              targetResourceUri: "workspace://canonical/report-final.json",
              effectState: "COMMITTED",
              idempotencyKeyHex: deterministicHex(`idem:${taskId}:op2`).slice(
                0,
                32,
              ),
              fencingTokenEpoch: 2,
              receiptHex: deterministicHex(`rcpt:${taskId}:op2`),
            },
          ],
          cellPlacement: {
            cellIdHex: `cell-local-${num.toString().padStart(2, "0")}`,
            leaseEpoch: 5,
            nodeBootGeneration: 2,
            placementMode: "LOCAL_SINGLE_NODE",
            fencingTokenHex: deterministicHex(`fence:${taskId}:a2`).slice(
              0,
              32,
            ),
          },
          receiptHex: deterministicHex(`rcpt:${taskId}:attempt2`),
        },
      ],
    };

    const waitingReasonZh =
      state === "WAITING_CONSENT"
        ? "等待操作员授权确认文件写入范围"
        : state === "WAITING_RESOURCE"
          ? "等待内存与处理器并发容量空出"
          : state === "WAITING_INPUT"
            ? "等待补充任务目标参数"
            : state === "WAITING_NETWORK"
              ? "等待外部网络连接恢复（本地工作站正常可用）"
              : "正在按既定计划推进";

    const waitingReasonEn =
      state === "WAITING_CONSENT"
        ? "Waiting for operator authorization on workspace write scope"
        : state === "WAITING_RESOURCE"
          ? "Waiting for memory and processor concurrency headroom"
          : state === "WAITING_INPUT"
            ? "Waiting for operator parameter input"
            : state === "WAITING_NETWORK"
              ? "Waiting for network connectivity (local workstation ready)"
              : "Proceeding according to schedule";

    return {
      summary: {
        taskId,
        planId,
        applicationId: `app.workstation.suite.${num.toString().padStart(2, "0")}`,
        installationGeneration: 2,
        planRevision: 3,
        taskState: state,
        undoClass,
        reservationId: `resv-${num.toString().padStart(2, "0")}`,
        cellId: `cell-local-${num.toString().padStart(2, "0")}`,
        requestedUnits,
        effectiveUnits,
        usageUnits,
        succeededTargets,
        failedTargets,
        receiptHex,
      },
      goalTitleZh: `工程归档与依赖校验任务 #${num}`,
      goalTitleEn: `Workspace Archive & Dependency Verification #${num}`,
      applicationNameZh: "工作站构建与归档套件",
      applicationNameEn: "Workstation Build & Archive Suite",
      groupNameZh: "数据采集与校验组",
      groupNameEn: "Ingest & Verification Group",
      phaseFold: {
        currentPhaseIndex: Math.min(4, (index % 4) + 1),
        totalPhases: 4,
        milestoneTitleZh: "核对工作区变更与资源预算",
        milestoneTitleEn: "Verifying workspace diff and resource budget",
        foldedToolStepCount: 6 + (index % 5),
      },
      l2Attribution: {
        waitingReasonZh,
        waitingReasonEn,
        stepProgressItems: [
          {
            stepId: `${planId}-node-completed`,
            titleZh: "读取工作区基线清单",
            titleEn: "Read Workspace Baseline Manifest",
            state: "COMPLETED",
            dependencyTitlesZh: [],
            dependencyTitlesEn: [],
          },
          {
            stepId: `${planId}-node-active`,
            titleZh: "执行差异归因分析",
            titleEn: "Execute Diff Attribution Analysis",
            state: "ACTIVE",
            dependencyTitlesZh: ["读取工作区基线清单"],
            dependencyTitlesEn: ["Read Workspace Baseline Manifest"],
          },
          {
            stepId: `${planId}-node-pending-a`,
            titleZh: "生成结构化阶段报告",
            titleEn: "Generate Structured Stage Report",
            state: "DECLARED",
            dependencyTitlesZh: ["执行差异归因分析"],
            dependencyTitlesEn: ["Execute Diff Attribution Analysis"],
            collapsedUnmaterializedCount: 18,
          },
        ],
        resourceBreakdown: [
          {
            resourceLabelZh: "处理器时间预算",
            resourceLabelEn: "Processor Time Budget",
            usageUnits,
            effectiveUnits,
            requestedUnits,
          },
          {
            resourceLabelZh: "工作区内存水线",
            resourceLabelEn: "Workspace Memory Watermark",
            usageUnits: Math.max(10, usageUnits - 12),
            effectiveUnits: 75,
            requestedUnits: 90,
          },
        ],
        partialTargets,
      },
      l3PlanDag: dagSnapshot,
      l3AttemptLanes: attemptLanes,
      latestAttemptIndex: 2,
      parentTaskId: null,
      layerLossDiagnostic:
        index === 15
          ? {
              reason: "STALE_GENERATION",
              currentGeneration: 4,
            }
          : null,
    };
  });
}

function renderUndoColumnHtml(
  row: TaskSpaceWorkstationRow,
  disclosure: DisclosureLevel,
  locale: WorkstationLocale,
): string {
  const undoClass = row.summary.undoClass;
  if (undoClass === "IRREVERSIBLE") {
    const prunedVerbs = pruneVerbsAgainstStatutoryLock(
      ["Undo", "ViewEffectProof"],
      "IRREVERSIBLE_NO_UNDO",
    );
    const hasViewProof = prunedVerbs.includes("ViewEffectProof");
    return (
      `<div class="nlos-task-row__undo-cell" data-undo-class="IRREVERSIBLE" data-undo-omitted="true">` +
      (hasViewProof
        ? `<button type="button" class="nlos-task-row__btn" data-task-action="view-effect-proof" data-task-id="${escapeHtml(row.summary.taskId)}">${locale === "en-US" ? "View Effect Proof" : "查看操作与效果凭证"}</button>`
        : "") +
      renderStatutoryLockBadgeHtml({
        kind: "IRREVERSIBLE_NO_UNDO",
        locale,
        disclosure,
      }) +
      `</div>`
    );
  }

  if (undoClass === "COMPENSATABLE") {
    return (
      `<div class="nlos-task-row__undo-cell" data-undo-class="COMPENSATABLE" data-undo-omitted="false">` +
      `<button type="button" class="nlos-task-row__btn" data-task-action="compensate" data-task-id="${escapeHtml(row.summary.taskId)}">${locale === "en-US" ? "Compensate" : "补偿回滚"}</button>` +
      `</div>`
    );
  }

  return (
    `<div class="nlos-task-row__undo-cell" data-undo-class="REVERSIBLE" data-undo-omitted="false">` +
    `<button type="button" class="nlos-task-row__btn" data-task-action="undo" data-task-id="${escapeHtml(row.summary.taskId)}">${locale === "en-US" ? "Undo" : "撤销"}</button>` +
    `</div>`
  );
}

function localizeVerbLabel(verb: string, locale: WorkstationLocale): string {
  if (locale === "en-US") {
    return verb;
  }
  switch (verb) {
    case "Pause":
      return "暂停";
    case "Resume":
      return "继续";
    case "Cancel":
      return "取消";
    case "Retry":
      return "重试";
    case "Refine":
      return "微调重开";
    case "Undo":
      return "撤销";
    case "ViewEffectProof":
      return "查看操作凭证";
    case "ReviewConsent":
      return "审阅授权";
    case "ProvideInput":
      return "补充说明";
    case "SubmitProposal":
      return "提交执行";
    case "ReviewDispute":
      return "核对差异";
    default:
      return verb;
  }
}

function renderL2AttributionDrawerForRow(
  row: TaskSpaceWorkstationRow,
  disclosure: DisclosureLevel,
  locale: WorkstationLocale,
): string {
  const drawerDisclosure = disclosure === "L3" ? "L3" : "L2";
  const breadcrumbs: ScopeBreadcrumbItem[] = [
    {
      kind: "Application",
      id: row.summary.applicationId,
      label: locale === "en-US" ? row.applicationNameEn : row.applicationNameZh,
    },
    {
      kind: "Task",
      id: row.summary.taskId,
      label: locale === "en-US" ? row.goalTitleEn : row.goalTitleZh,
    },
    {
      kind: "TaskGroup",
      id: `${row.summary.planId}-grp-ingest`,
      label: locale === "en-US" ? row.groupNameEn : row.groupNameZh,
    },
    {
      kind: "TaskNode",
      id: `${row.summary.planId}-node-active`,
      label:
        locale === "en-US"
          ? "Execute Diff Attribution Analysis"
          : "执行差异归因分析",
    },
    {
      kind: "TaskAttempt",
      id: `${row.summary.taskId}-attempt-2`,
      label: "Attempt #2",
    },
    {
      kind: "Fiber",
      id: `${row.summary.taskId}-a2-fiber-1`,
      label: "Worker #1",
    },
  ];

  const waitingReason =
    locale === "en-US"
      ? row.l2Attribution.waitingReasonEn
      : row.l2Attribution.waitingReasonZh;

  const stepsHtml =
    `<div class="nlos-task-l2-step-list">` +
    row.l2Attribution.stepProgressItems
      .map((step) => {
        const title = locale === "en-US" ? step.titleEn : step.titleZh;
        const deps =
          locale === "en-US"
            ? step.dependencyTitlesEn
            : step.dependencyTitlesZh;
        const depsLabel =
          deps.length > 0
            ? locale === "en-US"
              ? `Depends on: ${deps.join(", ")}`
              : `前置依赖：${deps.join("、")}`
            : locale === "en-US"
              ? "Root step"
              : "起始步骤";
        const lazySuffix =
          step.collapsedUnmaterializedCount !== undefined &&
          step.collapsedUnmaterializedCount > 0
            ? `<span class="nlos-task-l2-step-lazy" data-tabular-nums="true">${escapeHtml(
                formatLazySupernodeBadgeText(
                  step.collapsedUnmaterializedCount,
                  locale,
                ),
              )}</span>`
            : "";
        return (
          `<div class="nlos-task-l2-step-item" data-step-id="${escapeHtml(step.stepId)}" data-step-state="${step.state}">` +
          renderStatusIndicatorHtml({
            state: step.state,
            locale,
            disclosure: drawerDisclosure,
          }) +
          `<span class="nlos-task-l2-step-title">${escapeHtml(title)}</span>` +
          `<span class="nlos-task-l2-step-deps">${escapeHtml(depsLabel)}</span>` +
          lazySuffix +
          `</div>`
        );
      })
      .join("") +
    `</div>`;

  const resourcesHtml = row.l2Attribution.resourceBreakdown
    .map((res) => {
      const label =
        locale === "en-US" ? res.resourceLabelEn : res.resourceLabelZh;
      const barHtml = renderBulletMicroBarHtml({
        actualUsage: res.usageUnits,
        effectiveGrantedOrLeaseCap: res.effectiveUnits,
        requestedCeiling: res.requestedUnits,
        unit: "%",
        locale,
        disclosure: drawerDisclosure,
      });
      return (
        `<div class="nlos-task-l2-resource-item">` +
        `<div class="nlos-task-l2-resource-label">${escapeHtml(label)}</div>` +
        barHtml +
        `</div>`
      );
    })
    .join("");

  const partialHtml =
    `<div class="nlos-task-l2-partial-table" data-partial-breakdown="true">` +
    row.l2Attribution.partialTargets
      .map((target) => {
        const label =
          locale === "en-US" ? target.targetLabelEn : target.targetLabelZh;
        const statusText = target.succeeded
          ? locale === "en-US"
            ? "Succeeded"
            : "已完成"
          : locale === "en-US"
            ? `Failed: ${target.failureReasonEn ?? "Target error"}`
            : `失败：${target.failureReasonZh ?? "目标异常"}`;
        const retryBtn = !target.succeeded
          ? `<button type="button" class="nlos-task-row__btn" data-task-action="retry-failed-target" data-task-id="${escapeHtml(row.summary.taskId)}" data-target-id="${escapeHtml(target.targetId)}">${locale === "en-US" ? "Retry Failed Target" : "仅重试失败目标"}</button>`
          : "";
        return (
          `<div class="nlos-task-l2-partial-row" data-target-id="${escapeHtml(target.targetId)}" data-target-succeeded="${target.succeeded ? "true" : "false"}">` +
          `<span class="nlos-task-l2-partial-label">${escapeHtml(label)}</span>` +
          `<span class="nlos-task-l2-partial-status">${escapeHtml(statusText)}</span>` +
          retryBtn +
          `</div>`
        );
      })
      .join("") +
    `</div>`;

  const drawerShellHtml = renderInspectorDrawerHtml({
    open: disclosure === "L2" || disclosure === "L3",
    placement: "right",
    disclosure: drawerDisclosure,
    locale,
    breadcrumbs: filterBreadcrumbsForDisclosure(breadcrumbs, drawerDisclosure),
  });

  const sectionsHtml =
    `<section class="nlos-task-l2-section" data-section-id="l2-waiting-reason">` +
    `<h4 class="nlos-task-l2-section__title">${locale === "en-US" ? "Current Execution & Waiting Attribution" : "当前执行进展与等待归因"}</h4>` +
    `<p class="nlos-task-l2-waiting-text">${escapeHtml(waitingReason)}</p>` +
    `</section>` +
    `<section class="nlos-task-l2-section" data-section-id="l2-step-dependencies">` +
    `<h4 class="nlos-task-l2-section__title">${locale === "en-US" ? "Key Step Dependency Progress" : "关键步骤依赖进展"}</h4>` +
    stepsHtml +
    `</section>` +
    `<section class="nlos-task-l2-section" data-section-id="l2-resource-bottleneck">` +
    `<h4 class="nlos-task-l2-section__title">${locale === "en-US" ? "Resource Usage / Effective / Requested Breakdown" : "资源使用量 / 生效上限 / 申请上限分解"}</h4>` +
    resourcesHtml +
    `</section>` +
    `<section class="nlos-task-l2-section" data-section-id="l2-partial-targets">` +
    `<h4 class="nlos-task-l2-section__title">${locale === "en-US" ? "Per-Target Completion & Partial Retry" : "分目标完成状态与局部重试"}</h4>` +
    partialHtml +
    `</section>`;

  return drawerShellHtml.replace(
    `<div class="nlos-inspector-drawer__body"></div>`,
    `<div class="nlos-inspector-drawer__body">${sectionsHtml}</div>`,
  );
}

export interface RenderTaskSpaceSurfaceOptions {
  readonly rows: readonly TaskSpaceWorkstationRow[];
  readonly selectedTaskId: string | null;
  readonly disclosureLevel: DisclosureLevel;
  readonly locale?: WorkstationLocale;
  readonly activeDagPreview?: PlanOverridePreviewResult | null;
  readonly activeDagRejection?: PlanOverrideRejection | null;
  readonly activeAttemptRejection?: TaskAttemptControlRejection | null;
}

/**
 * Pure HTML renderer for the Stage D `TaskSpace` surface (`L1` commitment table,
 * `L2` non-modal single-row Headlamp attribution drawer, and `L3` Dagster orthogonal DAG
 * override editor + Temporal `TaskAttempt` racing swimlanes).
 */
export function renderTaskSpaceSurfaceHtml(
  options: RenderTaskSpaceSurfaceOptions,
): string {
  const locale = options.locale ?? "zh-CN";
  const level = options.disclosureLevel;
  const selectedRow =
    options.selectedTaskId !== null
      ? (options.rows.find((r) => r.summary.taskId === options.selectedTaskId) ??
        null)
      : null;

  const rowsHtml = options.rows
    .map((row) => {
      const isSelected = row.summary.taskId === options.selectedTaskId;
      const statusHtml = renderStatusIndicatorHtml({
        state: row.summary.taskState,
        locale,
        disclosure: level,
      });

      const goalTitle =
        locale === "en-US" ? row.goalTitleEn : row.goalTitleZh;
      const phaseSummary = renderTemporalPhaseSummaryText(
        row.phaseFold,
        locale,
      );
      const appName =
        locale === "en-US" ? row.applicationNameEn : row.applicationNameZh;

      const bulletHtml = renderBulletMicroBarHtml({
        actualUsage: row.summary.usageUnits,
        effectiveGrantedOrLeaseCap: row.summary.effectiveUnits,
        requestedCeiling: row.summary.requestedUnits,
        unit: "%",
        locale,
        disclosure: level,
      });

      const partialText =
        locale === "en-US"
          ? `${row.summary.succeededTargets} Succeeded / ${row.summary.failedTargets} Failed`
          : `${row.summary.succeededTargets} 成功 / ${row.summary.failedTargets} 失败`;

      const prunerOutcome = computeAllowedVerbs({
        domain: "task",
        state: row.summary.taskState,
        callerCapabilities: ["control:write"],
        readBacking: {
          targetIdHex: deterministicHex(row.summary.taskId).slice(0, 32),
          expectedGenerationOrRevision: row.summary.planRevision,
        },
        undoClass:
          row.summary.undoClass === "IRREVERSIBLE"
            ? "irreversible"
            : row.summary.undoClass === "COMPENSATABLE"
              ? "compensatable"
              : "reversible",
        locale,
        disclosure: level,
      });

      const stateActionsHtml = prunerOutcome.allowedVerbs
        .map((verb) => {
          const label = localizeVerbLabel(verb, locale);
          return `<button type="button" class="nlos-task-row__btn" data-task-control-verb="${escapeHtml(verb)}" data-task-id="${escapeHtml(row.summary.taskId)}">${escapeHtml(label)}</button>`;
        })
        .join("");

      const canReopen =
        TERMINAL_OR_REOPENABLE_STATES.has(row.summary.taskState) ||
        row.summary.failedTargets > 0;
      const reopenButtonsHtml = canReopen
        ? `<button type="button" class="nlos-task-row__btn" data-task-reopen="retry" data-task-id="${escapeHtml(row.summary.taskId)}">${locale === "en-US" ? "Retry" : "重试"}</button>` +
          `<button type="button" class="nlos-task-row__btn" data-task-reopen="refine" data-task-id="${escapeHtml(row.summary.taskId)}">${locale === "en-US" ? "Refine & Reopen" : "微调重开"}</button>`
        : "";

      const l3IdentityMetaHtml =
        level === "L3"
          ? `<div class="nlos-task-row__l3-meta" data-mono="true" data-tabular-nums="true">app=${escapeHtml(row.summary.applicationId)} · gen=${row.summary.installationGeneration} · rev=r${row.summary.planRevision} · cell=${escapeHtml(row.summary.cellId)}</div>`
          : "";

      return (
        `<tr class="nlos-task-row" data-task-id="${escapeHtml(row.summary.taskId)}" data-plan-id="${escapeHtml(row.summary.planId)}" data-task-state="${row.summary.taskState}" data-selected="${isSelected ? "true" : "false"}" tabindex="0">` +
        `<td class="nlos-task-row__status-col">${statusHtml}</td>` +
        `<td class="nlos-task-row__goal-col">` +
        `<div class="nlos-task-row__goal-cell">` +
        `<span class="nlos-task-row__goal-title">${escapeHtml(goalTitle)}</span>` +
        `<span class="nlos-task-row__phase-fold" data-phase-folded="true">${escapeHtml(phaseSummary)}</span>` +
        l3IdentityMetaHtml +
        `</div>` +
        `</td>` +
        `<td class="nlos-task-row__app-col">${escapeHtml(appName)}</td>` +
        `<td class="nlos-task-row__resource-col">${bulletHtml}</td>` +
        `<td class="nlos-task-row__partial-col"><span class="nlos-task-row__partial-badge" data-partial-failed="${row.summary.failedTargets}" data-tabular-nums="true">${escapeHtml(partialText)}</span></td>` +
        `<td class="nlos-task-row__undo-col">${renderUndoColumnHtml(row, level, locale)}</td>` +
        `<td class="nlos-task-row__actions-col"><div class="nlos-task-row__actions">${stateActionsHtml}${reopenButtonsHtml}</div></td>` +
        `</tr>`
      );
    })
    .join("");

  const l2DrawerHtml =
    (level === "L2" || level === "L3") && selectedRow !== null
      ? `<div class="nlos-task-l2-drawer" data-selected-task-id="${escapeHtml(selectedRow.summary.taskId)}">${renderL2AttributionDrawerForRow(selectedRow, level, locale)}</div>`
      : "";

  let l3DiagnosticsHtml = "";
  if (level === "L3" && selectedRow !== null) {
    const lossBannerHtml = selectedRow.layerLossDiagnostic
      ? `<div class="nlos-task-space__loss-banner" role="status" data-loss-reason="${selectedRow.layerLossDiagnostic.reason}" data-current-generation="${selectedRow.layerLossDiagnostic.currentGeneration ?? ""}">` +
        `${escapeHtml(
          formatLayerInspectionDiagnosticBanner(
            selectedRow.layerLossDiagnostic.reason,
            selectedRow.layerLossDiagnostic.currentGeneration,
            locale,
          ),
        )}` +
        `</div>`
      : "";

    const dagHtml = renderTaskPlanDagCanvasHtml({
      snapshot: selectedRow.l3PlanDag,
      locale,
      activePreview: options.activeDagPreview,
      activeRejection: options.activeDagRejection,
    });

    const swimlanesHtml = renderTaskAttemptSwimlanesHtml({
      snapshot: selectedRow.l3AttemptLanes,
      locale,
      activeRejection: options.activeAttemptRejection,
    });

    const receiptBarHtml = renderReceiptAuditBarHtml({
      disclosure: "L3",
      controlCommandIdHex: selectedRow.summary.reservationId,
      receiptHex: selectedRow.summary.receiptHex,
      correlationIdHex: selectedRow.l3PlanDag.planDigestHex,
      cellId: selectedRow.summary.cellId,
      leaseEpoch: selectedRow.summary.planRevision,
      nodeBootGeneration: selectedRow.summary.installationGeneration,
      locale,
    });

    l3DiagnosticsHtml =
      `<section class="nlos-task-space__l3-forensics" data-l3-forensics="true" data-task-id="${escapeHtml(selectedRow.summary.taskId)}">` +
      lossBannerHtml +
      dagHtml +
      swimlanesHtml +
      receiptBarHtml +
      `</section>`;
  }

  const levelBadgeText =
    level === "L1"
      ? locale === "en-US"
        ? "Commitment View"
        : "任务承诺视图"
      : level === "L2"
        ? locale === "en-US"
          ? "Attribution Inspector"
          : "单行归因抽屉"
        : locale === "en-US"
          ? "L3 Kernel & DAG Forensics (Ctrl+Shift+E)"
          : "L3 内核取证与 DAG 重编排 (Ctrl+Shift+E)";

  return (
    `<section class="nlos-task-space" data-surface-id="z1-task-space" data-disclosure-level="${level}" data-selected-task-id="${escapeHtml(options.selectedTaskId ?? "")}">` +
    `<header class="nlos-task-space__toolbar">` +
    `<div class="nlos-task-space__toolbar-title">${locale === "en-US" ? "Task Space" : "任务空间"}</div>` +
    `<div class="nlos-task-space__toolbar-meta">` +
    `<span class="nlos-task-space__disclosure-pill" data-disclosure-indicator="${level}">${escapeHtml(levelBadgeText)}</span>` +
    `<span data-tabular-nums="true">${locale === "en-US" ? `${options.rows.length} Tasks` : `共 ${options.rows.length} 项任务`}</span>` +
    `</div>` +
    `</header>` +
    `<div class="nlos-task-space__workspace-split">` +
    `<div class="nlos-task-space__table-region">` +
    `<table class="nlos-task-table" data-task-commitment-table="true">` +
    `<thead><tr>` +
    `<th>${locale === "en-US" ? "Status" : "状态"}</th>` +
    `<th>${locale === "en-US" ? "Task Goal & Phase Progress" : "任务目标与阶段进展"}</th>` +
    `<th>${locale === "en-US" ? "Application" : "所属应用"}</th>` +
    `<th>${locale === "en-US" ? "Resource Watermarks" : "资源水位（使用/生效/申请）"}</th>` +
    `<th>${locale === "en-US" ? "Target Completion" : "目标完成度"}</th>` +
    `<th>${locale === "en-US" ? "Reversibility" : "可逆与凭证"}</th>` +
    `<th>${locale === "en-US" ? "Actions" : "操作"}</th>` +
    `</tr></thead>` +
    `<tbody>${rowsHtml}</tbody>` +
    `</table>` +
    `</div>` +
    l2DrawerHtml +
    `</div>` +
    l3DiagnosticsHtml +
    `</section>`
  );
}

export interface TaskSpaceSurfaceController {
  getDisclosureModel(): DisclosureModel;
  getCursorCoordinator(): TaskSpaceLazyCursorCoordinator;
  getRows(): readonly TaskSpaceWorkstationRow[];
  getSelectedTaskId(): string | null;
  getSelectedRow(): TaskSpaceWorkstationRow | null;
  getPullPlan(): TaskSpaceDisclosurePullPlan;
  getDagController(taskId?: string): TaskPlanDagCanvasController | null;
  selectTaskRow(taskId: string): TaskSpaceDisclosurePullPlan;
  closeAttributionDrawer(): boolean;
  toggleDiagnosticL3(): DisclosureLevel;
  handleKeydown(event: DisclosureKeydownEvent): boolean;
  expandSupernodeOnDemand(
    supernodeId: string,
    requestedLimit?: number,
  ): {
    readonly expanded: boolean;
    readonly effectiveLimit: number;
    readonly materializedCount: number;
  };
  previewPlanOverride(
    operations: readonly PlanOverrideOperation[],
    expectedPlanRevision?: number,
  ): PlanOverrideClassificationOutcome;
  submitPlanOverride(
    operations: readonly PlanOverrideOperation[],
    options?: {
      readonly expectedPlanRevision?: number;
      readonly z4Grant?: Z4ReauthorizationGrant | null;
    },
  ): PlanOverrideSubmissionOutcome;
  executeAttemptAction(
    attemptId: string,
    verb: string,
  ): TaskAttemptControlOutcome;
  attemptForbiddenManualCommit(attemptId: string): TaskAttemptControlRejection;
  reopenTerminalTask(
    taskId: string,
    mode: TerminalTaskReopenMode,
    refinementGoalZh?: string,
    refinementGoalEn?: string,
  ): TerminalTaskReopenOutcome;
  renderHtml(locale?: WorkstationLocale): string;
}

export function createTaskSpaceSurfaceController(options?: {
  readonly initialRows?: readonly TaskSpaceWorkstationRow[];
  readonly disclosureModel?: DisclosureModel;
  readonly locale?: WorkstationLocale;
}): TaskSpaceSurfaceController {
  let rows: readonly TaskSpaceWorkstationRow[] =
    options?.initialRows ?? createDefaultTaskSpaceWorkstationRows();
  const disclosureModel =
    options?.disclosureModel ?? createDisclosureModel({ initialLevel: "L1" });
  const cursorCoordinator = createTaskSpaceLazyCursorCoordinator(disclosureModel);
  const dagControllers = new Map<string, TaskPlanDagCanvasController>();
  let activeDagPreview: PlanOverridePreviewResult | null = null;
  let activeDagRejection: PlanOverrideRejection | null = null;
  let activeAttemptRejection: TaskAttemptControlRejection | null = null;

  function ensureDagController(
    row: TaskSpaceWorkstationRow,
  ): TaskPlanDagCanvasController {
    let ctrl = dagControllers.get(row.summary.taskId);
    if (ctrl === undefined) {
      ctrl = createTaskPlanDagCanvasController(
        row.l3PlanDag,
        cursorCoordinator,
      );
      dagControllers.set(row.summary.taskId, ctrl);
    }
    return ctrl;
  }

  function syncRowFromDagController(taskId: string): void {
    const ctrl = dagControllers.get(taskId);
    if (ctrl === undefined) {
      return;
    }
    const updatedDag = ctrl.getSnapshot();
    rows = rows.map((r) =>
      r.summary.taskId === taskId
        ? {
            ...r,
            summary: {
              ...r.summary,
              planRevision: updatedDag.planRevision,
            },
            l3PlanDag: updatedDag,
          }
        : r,
    );
  }

  cursorCoordinator.requestSlicePull("list_tasks", {
    requestedLimit: disclosureModel.getVisibleRowWindow().limit,
  });

  return {
    getDisclosureModel(): DisclosureModel {
      return disclosureModel;
    },

    getCursorCoordinator(): TaskSpaceLazyCursorCoordinator {
      return cursorCoordinator;
    },

    getRows(): readonly TaskSpaceWorkstationRow[] {
      return rows;
    },

    getSelectedTaskId(): string | null {
      return disclosureModel.getSelectedEntityId();
    },

    getSelectedRow(): TaskSpaceWorkstationRow | null {
      const selectedId = disclosureModel.getSelectedEntityId();
      if (selectedId === null) {
        return null;
      }
      return rows.find((r) => r.summary.taskId === selectedId) ?? null;
    },

    getPullPlan(): TaskSpaceDisclosurePullPlan {
      return cursorCoordinator.computePullPlan();
    },

    getDagController(taskId?: string): TaskPlanDagCanvasController | null {
      const targetId = taskId ?? disclosureModel.getSelectedEntityId();
      if (targetId === null) {
        return null;
      }
      const row = rows.find((r) => r.summary.taskId === targetId);
      if (row === undefined) {
        return null;
      }
      return ensureDagController(row);
    },

    selectTaskRow(taskId: string): TaskSpaceDisclosurePullPlan {
      const currentLevel = disclosureModel.getLevel();
      if (currentLevel === "L3") {
        disclosureModel.selectEntity(taskId, "task-space");
        disclosureModel.expandL3Parent(taskId);
      } else {
        disclosureModel.toggleDrawerL2(taskId);
      }
      const newLevel = disclosureModel.getLevel();
      if (newLevel === "L2" || newLevel === "L3") {
        cursorCoordinator.requestSlicePull("list_task_nodes", {
          targetEntityId: taskId,
          requestedLimit: disclosureModel.getVisibleRowWindow().limit,
        });
        cursorCoordinator.requestSlicePull("inspect_resource_cost", {
          targetEntityId: taskId,
          requestedLimit: 1,
        });
      }
      if (newLevel === "L3") {
        cursorCoordinator.requestSlicePull("list_task_groups", {
          targetEntityId: taskId,
        });
        cursorCoordinator.requestSlicePull("list_task_attempts", {
          targetEntityId: taskId,
        });
        cursorCoordinator.requestSlicePull("list_execution_fibers", {
          targetEntityId: taskId,
        });
        cursorCoordinator.requestSlicePull("list_operations", {
          targetEntityId: taskId,
        });
      }
      return cursorCoordinator.computePullPlan();
    },

    closeAttributionDrawer(): boolean {
      return disclosureModel.closeDrawerOnEscape();
    },

    toggleDiagnosticL3(): DisclosureLevel {
      let selectedId = disclosureModel.getSelectedEntityId();
      if (selectedId === null && rows.length > 0) {
        selectedId = rows[0]!.summary.taskId;
        disclosureModel.selectEntity(selectedId, "task-space");
      }
      const nextLevel = disclosureModel.toggleDiagnosticL3();
      if (nextLevel === "L3" && selectedId !== null) {
        disclosureModel.expandL3Parent(selectedId);
        cursorCoordinator.requestSlicePull("list_task_groups", {
          targetEntityId: selectedId,
        });
        cursorCoordinator.requestSlicePull("list_task_attempts", {
          targetEntityId: selectedId,
        });
        cursorCoordinator.requestSlicePull("list_execution_fibers", {
          targetEntityId: selectedId,
        });
        cursorCoordinator.requestSlicePull("list_operations", {
          targetEntityId: selectedId,
        });
      }
      return nextLevel;
    },

    handleKeydown(event: DisclosureKeydownEvent): boolean {
      const prevLevel = disclosureModel.getLevel();
      const handled = disclosureModel.handleDisclosureKeydown(event);
      const nextLevel = disclosureModel.getLevel();
      const selectedId = disclosureModel.getSelectedEntityId();
      if (handled && prevLevel !== nextLevel && selectedId !== null) {
        if (nextLevel === "L2") {
          cursorCoordinator.requestSlicePull("list_task_nodes", {
            targetEntityId: selectedId,
          });
          cursorCoordinator.requestSlicePull("inspect_resource_cost", {
            targetEntityId: selectedId,
            requestedLimit: 1,
          });
        } else if (nextLevel === "L3") {
          disclosureModel.expandL3Parent(selectedId);
          cursorCoordinator.requestSlicePull("list_task_groups", {
            targetEntityId: selectedId,
          });
          cursorCoordinator.requestSlicePull("list_task_attempts", {
            targetEntityId: selectedId,
          });
          cursorCoordinator.requestSlicePull("list_execution_fibers", {
            targetEntityId: selectedId,
          });
          cursorCoordinator.requestSlicePull("list_operations", {
            targetEntityId: selectedId,
          });
        }
      }
      return handled;
    },

    expandSupernodeOnDemand(supernodeId: string, requestedLimit?: number) {
      const selectedRow = this.getSelectedRow() ?? rows[0] ?? null;
      if (selectedRow === null) {
        return { expanded: false, effectiveLimit: 0, materializedCount: 0 };
      }
      const dagCtrl = ensureDagController(selectedRow);
      const res = dagCtrl.expandLazySupernode(supernodeId, requestedLimit);
      syncRowFromDagController(selectedRow.summary.taskId);
      return {
        expanded: res.expanded,
        effectiveLimit: res.effectiveLimit,
        materializedCount: res.materializedNodes.length,
      };
    },

    previewPlanOverride(
      operations: readonly PlanOverrideOperation[],
      expectedPlanRevision?: number,
    ): PlanOverrideClassificationOutcome {
      const selectedRow = this.getSelectedRow() ?? rows[0]!;
      const dagCtrl = ensureDagController(selectedRow);
      const outcome = dagCtrl.previewOverride(operations, expectedPlanRevision);
      if (outcome.ok) {
        activeDagPreview = outcome;
        activeDagRejection = null;
      } else {
        activeDagRejection = outcome;
      }
      return outcome;
    },

    submitPlanOverride(
      operations: readonly PlanOverrideOperation[],
      submitOptions?: {
        readonly expectedPlanRevision?: number;
        readonly z4Grant?: Z4ReauthorizationGrant | null;
      },
    ): PlanOverrideSubmissionOutcome {
      const selectedRow = this.getSelectedRow() ?? rows[0]!;
      const dagCtrl = ensureDagController(selectedRow);
      const outcome = dagCtrl.submitOverride(operations, submitOptions);
      if (outcome.ok) {
        syncRowFromDagController(selectedRow.summary.taskId);
        activeDagRejection = null;
      } else {
        activeDagRejection = outcome;
      }
      return outcome;
    },

    executeAttemptAction(
      attemptId: string,
      verb: string,
    ): TaskAttemptControlOutcome {
      const selectedRow = this.getSelectedRow() ?? rows[0]!;
      const outcome = executeTaskAttemptControlAction(
        selectedRow.l3AttemptLanes,
        attemptId,
        verb,
      );
      if (outcome.ok) {
        rows = rows.map((r) =>
          r.summary.taskId === selectedRow.summary.taskId
            ? { ...r, l3AttemptLanes: outcome.updatedSnapshot }
            : r,
        );
        activeAttemptRejection = null;
      } else {
        activeAttemptRejection = outcome;
      }
      return outcome;
    },

    attemptForbiddenManualCommit(attemptId: string): TaskAttemptControlRejection {
      const selectedRow = this.getSelectedRow() ?? rows[0]!;
      const rejection = attemptManualCommitOverride(
        selectedRow.l3AttemptLanes,
        attemptId,
      );
      activeAttemptRejection = rejection;
      return rejection;
    },

    reopenTerminalTask(
      taskId: string,
      mode: TerminalTaskReopenMode,
      refinementGoalZh?: string,
      refinementGoalEn?: string,
    ): TerminalTaskReopenOutcome {
      const res = reopenTerminalTaskInModel(
        rows,
        taskId,
        mode,
        refinementGoalZh,
        refinementGoalEn,
      );
      rows = res.updatedRows;
      return res.outcome;
    },

    renderHtml(locale: WorkstationLocale = options?.locale ?? "zh-CN"): string {
      return renderTaskSpaceSurfaceHtml({
        rows,
        selectedTaskId: disclosureModel.getSelectedEntityId(),
        disclosureLevel: disclosureModel.getLevel(),
        locale,
        activeDagPreview,
        activeDagRejection,
        activeAttemptRejection,
      });
    },
  };
}

export interface CompositorTaskSpaceHostLike {
  getDisclosureModel?(): DisclosureModel;
  focusLandmark?(landmarkId: "z1-task-space"): void;
}

export function bindTaskSpaceToCompositor(
  compositor: CompositorTaskSpaceHostLike,
  controller?: TaskSpaceSurfaceController,
): TaskSpaceSurfaceController {
  const sharedDisclosure =
    typeof compositor.getDisclosureModel === "function"
      ? compositor.getDisclosureModel()
      : undefined;
  const activeController =
    controller ??
    createTaskSpaceSurfaceController({
      disclosureModel: sharedDisclosure,
    });
  if (typeof compositor.focusLandmark === "function") {
    compositor.focusLandmark("z1-task-space");
  }
  return activeController;
}

export function createTaskSpaceSurfaceElement(
  doc: Document,
  controller: TaskSpaceSurfaceController = createTaskSpaceSurfaceController(),
  locale: WorkstationLocale = "zh-CN",
): HTMLElement {
  const container = doc.createElement("div");
  container.className = "nlos-task-space-mount";

  const render = (): void => {
    container.innerHTML = controller.renderHtml(locale);
    const rows = container.querySelectorAll<HTMLElement>(".nlos-task-row");
    rows.forEach((tr) => {
      const taskId = tr.getAttribute("data-task-id");
      if (taskId) {
        tr.addEventListener("click", () => {
          controller.selectTaskRow(taskId);
          render();
        });
      }
    });
  };

  render();
  return container;
}

// ============================================================================
// Backward-Compatible Legacy + Stage D Combined `taskSpaceView()` & `buildTaskSpaceView()`
// ============================================================================

const HEX32 = /^[0-9a-fA-F]{32}$/;

const DOMAIN_LABELS: Readonly<Record<RecoveryDomain, string>> = {
  artifact: "artifact",
  semantic: "semantic",
  resource: "resource",
};

const TASK_MANAGER_TABS: ReadonlyArray<{ id: string; label: string; hint: string }> = [
  { id: "health", label: "恢复总览", hint: "artifact 域巡检(InspectHealth)" },
  { id: "semantic", label: "语义恢复", hint: "semantic 域巡检" },
  { id: "task", label: "任务查询", hint: "单计划 InspectTask" },
  { id: "process", label: "进程查询", hint: "进程绑定 InspectProcess" },
  { id: "resource", label: "资源查询", hint: "资源预留 InspectResource" },
  { id: "parity", label: "一致性自检", hint: "GUI-CLI receipt 字节比对" },
];

function openSidebarTab(viewId: string): boolean {
  const sidebar = document.querySelector(".sidebar");
  if (sidebar === null) {
    return false;
  }
  for (const child of sidebar.children) {
    if (
      child instanceof HTMLElement &&
      child.classList.contains("tab") &&
      child.dataset.viewId === viewId
    ) {
      child.click();
      return true;
    }
  }
  return false;
}

function runReceiptAction(
  container: HTMLElement,
  action: () => Promise<ReceiptDto>,
): void {
  container.replaceChildren(el("p", { className: "muted", text: "派发中……" }));
  action()
    .then((receipt) => {
      container.replaceChildren(renderReceipt(receipt));
    })
    .catch((error: unknown) => showError(container, error));
}

function domainBadge(row: TaskRow): string {
  const parts: string[] = [];
  for (const domain of ["artifact", "semantic", "resource"] as const) {
    const alert = row.domains.get(domain);
    if (alert === undefined) {
      continue;
    }
    const ack = alert.acknowledgedReceiptIdHex === null ? "未确认" : "已确认";
    parts.push(`${DOMAIN_LABELS[domain]}:${alert.totalFailures}(${ack})`);
  }
  return parts.length > 0 ? parts.join(" / ") : "—(三域台账无 escalated 行)";
}

function scanSummaryRows(scans: readonly DomainScan[]): HTMLElement {
  const wrap = el("div");
  for (const scan of scans) {
    if (scan.counts === null) {
      wrap.append(
        fieldRow(`${DOMAIN_LABELS[scan.domain]} 域巡检`, "(类型化失败,无聚合事实)"),
      );
      continue;
    }
    wrap.append(
      fieldRow(
        `${DOMAIN_LABELS[scan.domain]} 域(retrying/escalated/unack/resolved)`,
        `${scan.counts.durableRetrying} / ${scan.counts.durableEscalated} / ${scan.counts.durableUnacknowledgedEscalated} / ${scan.counts.durableResolved}`,
      ),
    );
  }
  return wrap;
}

interface TaskSpaceState {
  scans: DomainScan[];
  rows: Map<string, TaskRow>;
  followed: Set<string>;
  selected: string | null;
}

interface TaskDetailHooks {
  initialReceipt: ReceiptDto | null;
  onQuery: () => void;
}

function taskDetailPane(
  state: TaskSpaceState,
  hooks: TaskDetailHooks,
): { node: HTMLElement; taskResult: HTMLElement | null } {
  const wrap = el("div");
  const selected = state.selected === null ? null : state.rows.get(state.selected) ?? null;
  if (selected === null) {
    wrap.append(
      el("p", { className: "muted", text: "在上方列表点选一个任务(plan_id)查看详情。" }),
    );
    return { node: wrap, taskResult: null };
  }

  const header = el("section", { className: "card" });
  header.append(el("h3", { text: `任务详情:${selected.planIdHex}` }));
  header.append(
    fieldRow("来源", selected.source === "scan" ? "三域巡检发现(escalated)" : "手动关注(InspectTask 验证)"),
  );
  for (const domain of ["artifact", "semantic", "resource"] as const) {
    const alert = selected.domains.get(domain);
    header.append(
      fieldRow(
        `${DOMAIN_LABELS[domain]} 域告警关联`,
        alert === undefined
          ? "—(最新扫描无此计划)"
          : `total_failures=${alert.totalFailures},acknowledged=${alert.acknowledgedReceiptIdHex ?? "未确认"}`,
      ),
    );
  }
  const navRow = el("p", { className: "muted", text: "跳转 Task Manager 视图:" });
  const navStatus = el("span", { className: "muted" });
  for (const tab of TASK_MANAGER_TABS) {
    const button = el("button", { text: tab.label });
    button.title = tab.hint;
    button.addEventListener("click", () => {
      if (openSidebarTab(tab.id)) {
        navStatus.textContent = "";
        return;
      }
      navStatus.textContent =
        `未找到侧栏 tab「${tab.label}」(data-view-id=${tab.id});主壳注册表可能已变更,详见 console。`;
      console.warn(`[task-space] openSidebarTab: no sidebar tab with data-view-id=${tab.id}`);
    });
    navRow.append(button);
  }
  navRow.append(navStatus);
  header.append(navRow);
  wrap.append(header);

  const taskReceipt = el("section", { className: "card" });
  taskReceipt.append(el("h3", { text: "任务回执(InspectTask,经认证 IPC)" }));
  const taskResult = el("div");
  const refresh = el("button", { text: "查询/刷新任务回执" });
  refresh.addEventListener("click", hooks.onQuery);
  taskReceipt.append(refresh, taskResult);
  if (hooks.initialReceipt !== null) {
    taskResult.append(renderReceipt(hooks.initialReceipt));
  } else {
    taskResult.append(
      el("p", {
        className: "muted",
        text: "尚未查询:选中任务或完成扫描后自动派发,亦可点击上方按钮手动查询。",
      }),
    );
  }
  wrap.append(taskReceipt);

  const entities = el("section", { className: "card" });
  entities.append(el("h3", { text: "关联实体查询(按已知 id,只读)" }));
  const process = labeledInput("进程 id(32 hex)", "32 hex 字符 process_id", "");
  const processResult = el("div");
  const processButton = el("button", { text: "查询进程绑定" });
  processButton.addEventListener("click", () => {
    const id = process.input.value.trim();
    if (!HEX32.test(id)) {
      processResult.replaceChildren(
        el("p", { className: "muted", text: "请先输入 32 位 hex 的 process_id。" }),
      );
      return;
    }
    runReceiptAction(processResult, () => inspectProcess(id.toLowerCase()));
  });
  const reservation = labeledInput("资源预留 id(32 hex)", "32 hex 字符 reservation_id", "");
  const reservationResult = el("div");
  const reservationButton = el("button", { text: "查询预留成本" });
  reservationButton.addEventListener("click", () => {
    const id = reservation.input.value.trim();
    if (!HEX32.test(id)) {
      reservationResult.replaceChildren(
        el("p", { className: "muted", text: "请先输入 32 位 hex 的 reservation_id。" }),
      );
      return;
    }
    runReceiptAction(reservationResult, () => inspectResourceCost(id.toLowerCase()));
  });
  const application = labeledInput("应用包 id(32 hex)", "32 hex 字符 package_id", "");
  const applicationResult = el("div");
  const applicationButton = el("button", { text: "查询应用头" });
  applicationButton.addEventListener("click", () => {
    const id = application.input.value.trim();
    if (!HEX32.test(id)) {
      applicationResult.replaceChildren(
        el("p", { className: "muted", text: "请先输入 32 位 hex 的 package_id。" }),
      );
      return;
    }
    runReceiptAction(applicationResult, () => inspectApplication(id.toLowerCase()));
  });
  entities.append(
    process.row,
    processButton,
    processResult,
    reservation.row,
    reservationButton,
    reservationResult,
    application.row,
    applicationButton,
    applicationResult,
  );
  wrap.append(entities);
  return { node: wrap, taskResult };
}

function planNodesCard(): HTMLElement {
  const panel = el("section", { className: "card" });
  panel.append(el("h3", { text: "五层只读 inspect(W39-D / §28.4 Task Space)" }));
  const group = labeledInput("TaskGroup id(32 hex)", "group_id", "");
  const groupResult = el("div");
  const groupButton = el("button", { text: "InspectTaskGroup" });
  groupButton.addEventListener("click", () => {
    const id = group.input.value.trim().toLowerCase();
    if (!HEX32.test(id)) {
      groupResult.replaceChildren(el("p", { className: "muted", text: "请先输入 32 位 hex group_id。" }));
      return;
    }
    runReceiptAction(groupResult, () => inspectTaskGroup(id));
  });

  const plan = labeledInput("TaskNode plan_id(32 hex)", "plan_id", "");
  const node = labeledInput("TaskNode node_id(32 hex)", "node_id", "");
  const nodeResult = el("div");
  const nodeButton = el("button", { text: "InspectTaskNode" });
  nodeButton.addEventListener("click", () => {
    const planId = plan.input.value.trim().toLowerCase();
    const nodeId = node.input.value.trim().toLowerCase();
    if (!HEX32.test(planId) || !HEX32.test(nodeId)) {
      nodeResult.replaceChildren(
        el("p", { className: "muted", text: "请先输入 32 位 hex 的 plan_id 与 node_id。" }),
      );
      return;
    }
    runReceiptAction(nodeResult, () => inspectTaskNode(planId, nodeId));
  });

  const fiber = labeledInput("ExecutionFiber id(32 hex)", "fiber_id", "");
  const fiberGen = labeledInput("fiber generation(>0)", "generation", "1");
  const fiberResult = el("div");
  const fiberButton = el("button", { text: "InspectExecutionFiber" });
  fiberButton.addEventListener("click", () => {
    const id = fiber.input.value.trim().toLowerCase();
    const generation = Number.parseInt(fiberGen.input.value.trim(), 10);
    if (!HEX32.test(id) || !Number.isFinite(generation) || generation < 1) {
      fiberResult.replaceChildren(
        el("p", { className: "muted", text: "请输入 32 hex fiber_id 与非零 generation。" }),
      );
      return;
    }
    runReceiptAction(fiberResult, () => inspectExecutionFiber(id, generation));
  });

  const topic = labeledInput("Topic id(32 hex)", "topic_id", "");
  const topicResult = el("div");
  const topicButton = el("button", { text: "InspectTopic" });
  topicButton.addEventListener("click", () => {
    const id = topic.input.value.trim().toLowerCase();
    if (!HEX32.test(id)) {
      topicResult.replaceChildren(el("p", { className: "muted", text: "请先输入 32 位 hex topic_id。" }));
      return;
    }
    runReceiptAction(topicResult, () => inspectTopic(id));
  });

  const operation = labeledInput("DurableOperation id(32 hex)", "operation_id", "");
  const opGen = labeledInput("operation generation(>0)", "generation", "1");
  const opResult = el("div");
  const opButton = el("button", { text: "InspectOperation" });
  opButton.addEventListener("click", () => {
    const id = operation.input.value.trim().toLowerCase();
    const generation = Number.parseInt(opGen.input.value.trim(), 10);
    if (!HEX32.test(id) || !Number.isFinite(generation) || generation < 1) {
      opResult.replaceChildren(
        el("p", { className: "muted", text: "请输入 32 hex operation_id 与非零 generation。" }),
      );
      return;
    }
    runReceiptAction(opResult, () => inspectOperation(id, generation));
  });

  panel.append(
    group.row,
    groupButton,
    groupResult,
    plan.row,
    node.row,
    nodeButton,
    nodeResult,
    fiber.row,
    fiberGen.row,
    fiberButton,
    fiberResult,
    topic.row,
    topicButton,
    topicResult,
    operation.row,
    opGen.row,
    opButton,
    opResult,
  );
  return panel;
}

function closedGapRegisterCard(): HTMLElement {
  const panel = el("section", { className: "card" });
  panel.append(el("h3", { text: "TASK_SPACE_GAPS 闭环核验登记 (W72-D-TASK-DAG)" }));
  const table = el("table", { className: "data-table" });
  const header = el("tr");
  header.append(
    el("th", { text: "编号" }),
    el("th", { text: "历史缺口事实" }),
    el("th", { text: "闭环 IPC 面" }),
    el("th", { text: "核验结论" }),
  );
  const head = el("thead");
  head.append(header);
  const body = el("tbody");
  for (const item of CLOSED_TASK_SPACE_GAPS) {
    const tr = el("tr");
    tr.append(
      el("td", { text: item.id }),
      el("td", { text: item.fact }),
      el("td", { text: item.closedByIpcSurface }),
      el("td", { text: item.verificationDetail }),
    );
    body.append(tr);
  }
  table.append(head, body);
  panel.append(table);
  return panel;
}

export function taskSpaceView(): HTMLElement {
  const state: TaskSpaceState = {
    scans: [],
    rows: new Map(),
    followed: new Set(),
    selected: null,
  };

  const wrap = el("div");

  if (typeof document !== "undefined") {
    const stageDSurface = createTaskSpaceSurfaceElement(document);
    wrap.append(stageDSurface);
  }

  const listCard = el("section", { className: "card" });
  listCard.append(el("h3", { text: "任务列表(三域恢复台账扫描 + 手动关注)" }));
  const status = el("p", { className: "muted", text: "尚未扫描;点击刷新派发三条只读巡检命令。" });
  const summary = el("div");
  const scanFooter = el("div");
  const tableBox = el("div");
  const refresh = el("button", { text: "刷新(三域巡检,经认证 IPC)" });

  const follow = labeledInput("关注任务 plan_id(32 hex)", "32 hex 字符 plan_id", "");
  const followResult = el("div");
  const followButton = el("button", { text: "关注(InspectTask 验证)" });

  const detailBox = el("div");

  let detailSeq = 0;
  let detailInFlightFor: string | null = null;
  let currentTaskResult: HTMLElement | null = null;
  let detailReceipt: ReceiptDto | null = null;
  let detailReceiptFor: string | null = null;

  const refreshSelectedDetail = (options?: { force?: boolean }): void => {
    const selectedId = state.selected;
    if (selectedId === null) {
      return;
    }
    if (!options?.force && detailInFlightFor === selectedId) {
      return;
    }
    detailInFlightFor = selectedId;
    const seq = ++detailSeq;
    if (currentTaskResult !== null) {
      currentTaskResult.replaceChildren(
        el("p", { className: "muted", text: "派发中(InspectTask,经认证 IPC)……" }),
      );
    }
    inspectTask(selectedId)
      .then((receipt) => {
        if (seq !== detailSeq) {
          return;
        }
        detailReceipt = receipt;
        detailReceiptFor = selectedId;
        if (state.selected === selectedId && currentTaskResult !== null) {
          currentTaskResult.replaceChildren(renderReceipt(receipt));
        }
      })
      .catch((error: unknown) => {
        if (seq !== detailSeq) {
          return;
        }
        if (state.selected === selectedId && currentTaskResult !== null) {
          showError(currentTaskResult, error);
        }
      })
      .finally(() => {
        if (seq === detailSeq) {
          detailInFlightFor = null;
        }
      });
  };

  const renderTable = (): void => {
    tableBox.replaceChildren();
    const selectedId = state.selected;
    const cachedReceipt =
      selectedId !== null && detailReceiptFor === selectedId ? detailReceipt : null;
    const detail = taskDetailPane(state, {
      initialReceipt: cachedReceipt,
      onQuery: () => refreshSelectedDetail({ force: true }),
    });
    detailBox.replaceChildren(detail.node);
    currentTaskResult = detail.taskResult;
    if (state.rows.size === 0) {
      tableBox.append(
        el("p", { className: "muted", text: "(空)三域台账当前无 escalated 计划行,且无手动关注任务。" }),
      );
      return;
    }
    const table = el("table", { className: "data-table" });
    const header = el("tr");
    header.append(
      el("th", { text: "plan_id" }),
      el("th", { text: "来源" }),
      el("th", { text: "域告警关联(total_failures/确认态)" }),
      el("th", { text: "操作" }),
    );
    const head = el("thead");
    head.append(header);
    const body = el("tbody");
    for (const row of [...state.rows.values()].sort((a, b) => a.planIdHex.localeCompare(b.planIdHex))) {
      const tr = el("tr");
      if (row.planIdHex === state.selected) {
        tr.style.fontWeight = "600";
      }
      tr.append(
        el("td", { text: row.planIdHex }),
        el("td", { text: row.source === "scan" ? "巡检发现" : "手动关注" }),
        el("td", { text: domainBadge(row) }),
      );
      const actions = el("td");
      const detailBtn = el("button", { text: "详情" });
      detailBtn.addEventListener("click", () => {
        state.selected = row.planIdHex;
        renderTable();
        refreshSelectedDetail();
      });
      actions.append(detailBtn);
      if (row.source === "follow") {
        const unfollow = el("button", { text: "取消关注" });
        unfollow.addEventListener("click", () => {
          state.followed.delete(row.planIdHex);
          state.rows = rebuildRows(state.followed, state.scans);
          if (state.selected === row.planIdHex) {
            state.selected = null;
          }
          renderTable();
        });
        actions.append(unfollow);
      }
      tr.append(actions);
      body.append(tr);
    }
    table.append(head, body);
    tableBox.append(table);
  };

  const renderScanFooter = (): void => {
    scanFooter.replaceChildren();
    if (state.scans.length === 0) {
      return;
    }
    const footer = el("section", { className: "card receipt-footer" });
    footer.append(el("h4", { text: "扫描回执(与 CLI `RECEIPT` 行同一等价契约)" }));
    for (const scan of state.scans) {
      const row = fieldRow(`${DOMAIN_LABELS[scan.domain]} 域巡检 receipt`, scan.receiptHex);
      row.classList.add("mono");
      footer.append(row);
    }
    scanFooter.append(footer);
  };

  const refreshScan = (): void => {
    status.textContent = "扫描中(三条只读派发)……";
    const domains: ReadonlyArray<{ id: RecoveryDomain; dispatch: () => Promise<ReceiptDto> }> = [
      { id: "artifact", dispatch: inspectHealth },
      { id: "semantic", dispatch: inspectSemanticHealth },
      { id: "resource", dispatch: inspectResourceHealth },
    ];
    Promise.all(
      domains.map((domain) =>
        domain
          .dispatch()
          .then(
            (receipt) => scanFromReceipt(domain.id, receipt),
            (error: unknown) => ({ domain: domain.id, error }) as const,
          ),
      ),
    )
      .then((results) => {
        state.scans = [];
        const failures: string[] = [];
        for (const result of results) {
          if ("error" in result) {
            failures.push(DOMAIN_LABELS[result.domain]);
          } else {
            state.scans.push(result);
          }
        }
        state.rows = rebuildRows(state.followed, state.scans);
        summary.replaceChildren(scanSummaryRows(state.scans));
        renderScanFooter();
        renderTable();
        refreshSelectedDetail();
        status.textContent =
          failures.length > 0
            ? `最近扫描:${new Date().toLocaleTimeString()}(${failures.join("/")} 域派发失败,见「一致性自检」排查)`
            : `最近扫描:${new Date().toLocaleTimeString()}`;
      })
      .catch((error: unknown) => showError(status.parentElement ?? listCard, error));
  };

  refresh.addEventListener("click", refreshScan);

  followButton.addEventListener("click", () => {
    const id = follow.input.value.trim().toLowerCase();
    if (!HEX32.test(id)) {
      followResult.replaceChildren(
        el("p", { className: "muted", text: "请先输入 32 位 hex 的 plan_id。" }),
      );
      return;
    }
    followResult.replaceChildren(el("p", { className: "muted", text: "验证中(InspectTask)……" }));
    inspectTask(id)
      .then((receipt) => {
        if (receipt.outcome.kind !== "inspected") {
          followResult.replaceChildren(renderOutcome(receipt.outcome), receiptFooter(receipt));
          return;
        }
        state.followed.add(id);
        state.selected = id;
        state.rows = rebuildRows(state.followed, state.scans);
        detailSeq += 1;
        detailInFlightFor = null;
        detailReceipt = receipt;
        detailReceiptFor = id;
        renderTable();
        followResult.replaceChildren(
          el("p", { className: "muted", text: `已关注 ${id}(存在于恢复快照);详情见下方。` }),
          renderReceipt(receipt),
        );
      })
      .catch((error: unknown) => showError(followResult, error));
  });

  listCard.append(status, refresh, summary, tableBox, follow.row, followButton, followResult, scanFooter);
  wrap.append(listCard, detailBox, planNodesCard(), closedGapRegisterCard());
  renderTable();
  return wrap;
}

export function buildTaskSpaceView(): HTMLElement {
  return taskSpaceView();
}
