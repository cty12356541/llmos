// Stage D Task Space (`L3`) — Dagster-Style Orthogonal `TaskPlan` DAG Canvas & Override Editor.
// Authority: Stage D Visual & UX Spec §3.2 (`[PLAN-CHANGE-001]`, `[PLAN-OVERRIDE-001]`,
// `[CTRL-PLAN-001]`, `[PLAN-LAZY-001]`, `[TASK-STREAM-001]`, `[TASK-EVOLVE-001]`),
// ADR-0025, Statutory Lock #3 (`NODE_COMPLETED_OR_ACTIVE_LOCKED`).

import {
  clampTaskSpaceCursorLimit,
  type TaskSpaceLazyCursorCoordinator,
} from "../../store/disclosure-model";
import {
  computeBlastRadiusSummary,
  renderBlastRadiusDiffHtml,
  type BlastRadiusDiffOptions,
  type BlastRadiusRow,
} from "../../ui/blast-radius-diff";
import { renderIconSvg } from "../../ui/icons";
import {
  pruneVerbsAgainstStatutoryLock,
  renderStatutoryLockBadgeHtml,
  renderStatusIndicatorHtml,
  type StatutoryLockKind,
  type WorkstationLocale,
  type WorkstationStatusState,
} from "../../ui/status-indicator";

export type TaskGroupExecutionTopology = "SEQUENTIAL" | "DAG" | "STREAMING" | "SERVICE";

export type TaskNodeResidencyState = "HOT_MATERIALIZED" | "METADATA_ONLY" | "COLD";

export type PlanOverrideVerb =
  | "add_node"
  | "remove_node"
  | "pause_node"
  | "merge_nodes"
  | "reorder_nodes"
  | "replace_node";

export const ALL_PLAN_OVERRIDE_VERBS: readonly PlanOverrideVerb[] = [
  "add_node",
  "remove_node",
  "pause_node",
  "merge_nodes",
  "reorder_nodes",
  "replace_node",
];

export interface TaskGroupDagProjection {
  readonly groupId: string;
  readonly title: string;
  readonly topology: TaskGroupExecutionTopology;
  readonly schedulingOrder: number;
  readonly failurePolicy: "FAIL_FAST" | "CONTINUE_ON_PARTIAL" | "ISOLATE_BRANCH";
  readonly cancellationScope: "GROUP_BOUNDED" | "PLAN_WIDE";
  readonly runtimeExpanded?: boolean;
  readonly attributedBudgetUnits: number;
}

export interface TaskNodeDagProjection {
  readonly nodeId: string;
  readonly groupId: string;
  readonly title: string;
  readonly state: WorkstationStatusState;
  readonly dependencyNodeIds: readonly string[];
  readonly capabilityId: string;
  readonly requestedBudgetUnits: number;
  readonly effectiveBudgetUnits: number;
  readonly usageBudgetUnits: number;
  readonly residencyState: TaskNodeResidencyState;
  readonly runtimeExpanded?: boolean;
  readonly crossApplicationTarget?: string | null;
  readonly assignedAttemptCount: number;
}

export interface TaskPlanLazySupernode {
  readonly supernodeId: string;
  readonly groupId: string;
  readonly titleZh: string;
  readonly titleEn: string;
  readonly dependencyNodeIds: readonly string[];
  readonly unmaterializedStepCount: number;
  readonly allocatedProcessCount: 0;
  readonly residencyState: "METADATA_ONLY" | "COLD";
  readonly expanded: boolean;
  readonly materializedNodes: readonly TaskNodeDagProjection[];
  readonly nextCursorHex: string | null;
}

export interface TaskPlanDagSnapshot {
  readonly taskId: string;
  readonly planId: string;
  readonly planRevision: number;
  readonly installationGeneration: number;
  readonly planDigestHex: string;
  readonly authorizedCapabilities: readonly string[];
  readonly authorizedBudgetCeilingUnits: number;
  readonly groups: readonly TaskGroupDagProjection[];
  readonly nodes: readonly TaskNodeDagProjection[];
  readonly lazySupernodes: readonly TaskPlanLazySupernode[];
}

export interface OrthogonalDagNodeBox {
  readonly id: string;
  readonly kind: "TASK_NODE" | "LAZY_SUPERNODE";
  readonly groupId: string;
  readonly title: string;
  readonly state: WorkstationStatusState;
  readonly columnIndex: number;
  readonly rowIndex: number;
  readonly xPx: number;
  readonly yPx: number;
  readonly widthPx: number;
  readonly heightPx: number;
  readonly editableForOverride: boolean;
  readonly statutoryLock: StatutoryLockKind | null;
  readonly allowedOverrideVerbs: readonly PlanOverrideVerb[];
  readonly residencyState: TaskNodeResidencyState;
  readonly runtimeExpanded: boolean;
  readonly unmaterializedStepCount: number;
  readonly badgeTextZh: string | null;
  readonly badgeTextEn: string | null;
}

export interface OrthogonalDagEdge {
  readonly edgeId: string;
  readonly fromNodeId: string;
  readonly toNodeId: string;
  readonly points: readonly { readonly xPx: number; readonly yPx: number }[];
  readonly svgPathD: string;
}

export interface OrthogonalDagLayout {
  readonly direction: "LR";
  readonly totalWidthPx: number;
  readonly totalHeightPx: number;
  readonly columnCount: number;
  readonly nodeBoxes: readonly OrthogonalDagNodeBox[];
  readonly edges: readonly OrthogonalDagEdge[];
}

export type PlanOverrideOperation =
  | {
      readonly kind: "add_node";
      readonly nodeId: string;
      readonly groupId: string;
      readonly title: string;
      readonly dependencyNodeIds: readonly string[];
      readonly capabilityId: string;
      readonly requestedBudgetUnits: number;
      readonly crossApplicationTarget?: string | null;
    }
  | {
      readonly kind: "remove_node";
      readonly targetNodeId: string;
      readonly altersAcceptanceOrSideEffects?: boolean;
    }
  | {
      readonly kind: "pause_node";
      readonly targetNodeId: string;
      readonly reason?: string;
    }
  | {
      readonly kind: "merge_nodes";
      readonly primaryNodeId: string;
      readonly secondaryNodeId: string;
      readonly mergedTitle: string;
      readonly crossScopeOrCapability?: boolean;
    }
  | {
      readonly kind: "reorder_nodes";
      readonly nodeId: string;
      readonly newDependencyNodeIds: readonly string[];
      readonly withinAuthorizedEnvelope?: boolean;
    }
  | {
      readonly kind: "replace_node";
      readonly targetNodeId: string;
      readonly replacementNodeId: string;
      readonly replacementTitle: string;
      readonly capabilityId: string;
      readonly requestedBudgetUnits: number;
      readonly crossApplicationTarget?: string | null;
    };

export type PlanOverridePathKind =
  | "BOUNDED_CONTROL_COMMAND"
  | "RECOMPILE_AND_Z4_REAUTHORIZE";

export type PlanOverrideErrorCode =
  | "READ_ONLY_EXECUTED_TASK_NODE"
  | "NOT_FOUND"
  | "STALE_GENERATION"
  | "Z4_REAUTHORIZATION_REQUIRED"
  | "EMPTY_OVERRIDE_BATCH"
  | "CYCLE_DETECTED";

export interface PlanOverrideRejection {
  readonly ok: false;
  readonly errorCode: PlanOverrideErrorCode;
  readonly statutoryLock: StatutoryLockKind | null;
  readonly rejectedNodeId: string | null;
  readonly rejectedNodeState: WorkstationStatusState | null;
  readonly currentGeneration: number | null;
  readonly messageZh: string;
  readonly messageEn: string;
}

export interface CompiledPlanControlCommand {
  readonly commandIdHex: string;
  readonly scopeKind: "TaskNode" | "TaskPlan";
  readonly targetId: string;
  readonly action: "PAUSE_TASK_NODE" | "REORDER_TASK_PLAN_NODES";
  readonly expectedPlanRevision: number;
  readonly idempotencyKeyHex: string;
}

export interface Z4ReauthorizationGrant {
  readonly grantTokenHex: string;
  readonly planId: string;
  readonly expectedPlanRevision: number;
  readonly approvedPlanDigestHex: string;
  readonly authorizerPolicyEpoch: number;
}

export interface BlastRadiusDiffModel {
  readonly title: string;
  readonly planRevision: number;
  readonly rows: readonly BlastRadiusRow[];
  readonly requiresApproval: boolean;
  readonly approvalReason?: string;
}

export interface PlanOverridePreviewResult {
  readonly ok: true;
  readonly pathKind: PlanOverridePathKind;
  readonly requiresZ4Reauthorization: boolean;
  readonly basePlanRevision: number;
  readonly candidatePlanRevision: number;
  readonly basePlanDigestHex: string;
  readonly candidatePlanDigestHex: string;
  readonly compiledControlCommands: readonly CompiledPlanControlCommand[];
  readonly blastRadiusDiff: BlastRadiusDiffModel;
  readonly summaryZh: string;
  readonly summaryEn: string;
}

export type PlanOverrideClassificationOutcome =
  | PlanOverridePreviewResult
  | PlanOverrideRejection;

export interface PlanOverrideCommitReceipt {
  readonly ok: true;
  readonly pathKind: PlanOverridePathKind;
  readonly updatedPlan: TaskPlanDagSnapshot;
  readonly receiptHex: string;
  readonly auditIdHex: string;
  readonly compiledControlCommands: readonly CompiledPlanControlCommand[];
  readonly z4GrantTokenHex: string | null;
}

export type PlanOverrideSubmissionOutcome =
  | PlanOverrideCommitReceipt
  | PlanOverrideRejection;

const LOCKED_EXECUTED_STATES: ReadonlySet<WorkstationStatusState> =
  new Set<WorkstationStatusState>([
    "ACTIVE",
    "RUNNING",
    "COMPLETED",
    "COMPLETING",
    "CANCELLING",
  ]);

const EDITABLE_UNEXECUTED_STATES: ReadonlySet<WorkstationStatusState> =
  new Set<WorkstationStatusState>([
    "DECLARED",
    "DRAFT",
    "PROPOSED",
    "BLOCKED_DEPENDENCY",
    "WAITING_CONSENT",
    "WAITING_INPUT",
    "WAITING_RESOURCE",
    "WAITING_NETWORK",
    "ELIGIBLE",
    "SUSPENDED",
  ]);

/**
 * Statutory Lock #3 (`NODE_COMPLETED_OR_ACTIVE_LOCKED`):
 * `COMPLETED` and `ACTIVE` (or `RUNNING`) `TaskNode`s are strictly read-only in the DAG editor.
 */
export function isTaskNodeEditableForOverride(state: WorkstationStatusState): boolean {
  if (LOCKED_EXECUTED_STATES.has(state)) {
    return false;
  }
  return EDITABLE_UNEXECUTED_STATES.has(state);
}

export function getTaskNodeStatutoryLock(
  state: WorkstationStatusState,
): StatutoryLockKind | null {
  if (LOCKED_EXECUTED_STATES.has(state)) {
    return "NODE_COMPLETED_OR_ACTIVE_LOCKED";
  }
  return null;
}

export function formatLazySupernodeBadgeText(
  unmaterializedStepCount: number,
  locale: WorkstationLocale = "zh-CN",
): string {
  const safeCount = Math.max(0, Math.floor(unmaterializedStepCount));
  return locale === "en-US"
    ? `+${safeCount} Unmaterialized Steps · 0 Processes`
    : `+${safeCount} 待物化步骤 · 0 进程`;
}

function deterministicHexDigest(seed: string): string {
  let h1 = 0x811c9dc5;
  let h2 = 0x01000193;
  for (let i = 0; i < seed.length; i += 1) {
    const ch = seed.charCodeAt(i);
    h1 ^= ch;
    h1 = Math.imul(h1, 0x01000193) >>> 0;
    h2 ^= (ch + i) & 0xff;
    h2 = Math.imul(h2, 0x85ebca6b) >>> 0;
  }
  const part1 = h1.toString(16).padStart(8, "0");
  const part2 = h2.toString(16).padStart(8, "0");
  const part3 = ((h1 ^ h2) >>> 0).toString(16).padStart(8, "0");
  const part4 = (Math.imul(h1, 31) >>> 0).toString(16).padStart(8, "0");
  return `${part1}${part2}${part3}${part4}${part1}${part2}${part3}${part4}`;
}

function escapeHtml(value: string): string {
  return value
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

const DAG_NODE_WIDTH_PX = 224 as const;
const DAG_NODE_HEIGHT_PX = 68 as const;
const DAG_COLUMN_GAP_PX = 76 as const;
const DAG_ROW_GAP_PX = 24 as const;
const DAG_PADDING_X_PX = 24 as const;
const DAG_PADDING_Y_PX = 24 as const;

/**
 * Computes a deterministic Dagster-style Left-to-Right (`LR`) orthogonal DAG layout.
 * Unmaterialized super-nodes remain collapsed single boxes unless `supernode.expanded === true`.
 */
export function computeOrthogonalDagLayout(
  snapshot: TaskPlanDagSnapshot,
): OrthogonalDagLayout {
  interface RawVertex {
    readonly id: string;
    readonly kind: "TASK_NODE" | "LAZY_SUPERNODE";
    readonly groupId: string;
    readonly title: string;
    readonly state: WorkstationStatusState;
    readonly dependencyIds: readonly string[];
    readonly residencyState: TaskNodeResidencyState;
    readonly runtimeExpanded: boolean;
    readonly unmaterializedStepCount: number;
    readonly badgeTextZh: string | null;
    readonly badgeTextEn: string | null;
  }

  const vertices: RawVertex[] = [];
  const vertexById = new Map<string, RawVertex>();

  for (const node of snapshot.nodes) {
    const v: RawVertex = {
      id: node.nodeId,
      kind: "TASK_NODE",
      groupId: node.groupId,
      title: node.title,
      state: node.state,
      dependencyIds: node.dependencyNodeIds,
      residencyState: node.residencyState,
      runtimeExpanded: Boolean(node.runtimeExpanded),
      unmaterializedStepCount: 0,
      badgeTextZh: node.runtimeExpanded ? "运行时展开子步骤" : null,
      badgeTextEn: node.runtimeExpanded ? "RuntimeExpanded" : null,
    };
    vertices.push(v);
    vertexById.set(v.id, v);
  }

  for (const supernode of snapshot.lazySupernodes) {
    if (supernode.expanded && supernode.materializedNodes.length > 0) {
      for (const child of supernode.materializedNodes) {
        if (vertexById.has(child.nodeId)) {
          continue;
        }
        const v: RawVertex = {
          id: child.nodeId,
          kind: "TASK_NODE",
          groupId: child.groupId,
          title: child.title,
          state: child.state,
          dependencyIds: child.dependencyNodeIds,
          residencyState: child.residencyState,
          runtimeExpanded: Boolean(child.runtimeExpanded),
          unmaterializedStepCount: 0,
          badgeTextZh: child.runtimeExpanded ? "运行时展开子步骤" : null,
          badgeTextEn: child.runtimeExpanded ? "RuntimeExpanded" : null,
        };
        vertices.push(v);
        vertexById.set(v.id, v);
      }
    } else {
      const v: RawVertex = {
        id: supernode.supernodeId,
        kind: "LAZY_SUPERNODE",
        groupId: supernode.groupId,
        title: supernode.titleZh,
        state: "DECLARED",
        dependencyIds: supernode.dependencyNodeIds,
        residencyState: supernode.residencyState,
        runtimeExpanded: false,
        unmaterializedStepCount: supernode.unmaterializedStepCount,
        badgeTextZh: formatLazySupernodeBadgeText(
          supernode.unmaterializedStepCount,
          "zh-CN",
        ),
        badgeTextEn: formatLazySupernodeBadgeText(
          supernode.unmaterializedStepCount,
          "en-US",
        ),
      };
      vertices.push(v);
      vertexById.set(v.id, v);
    }
  }

  const columnMemo = new Map<string, number>();
  const visiting = new Set<string>();

  function resolveColumn(id: string): number {
    const cached = columnMemo.get(id);
    if (cached !== undefined) {
      return cached;
    }
    if (visiting.has(id)) {
      return 0;
    }
    visiting.add(id);
    const v = vertexById.get(id);
    let col = 0;
    if (v !== undefined) {
      for (const depId of v.dependencyIds) {
        if (vertexById.has(depId)) {
          col = Math.max(col, resolveColumn(depId) + 1);
        }
      }
    }
    visiting.delete(id);
    columnMemo.set(id, col);
    return col;
  }

  const columns = new Map<number, RawVertex[]>();
  let maxColumn = 0;
  for (const v of vertices) {
    const col = resolveColumn(v.id);
    maxColumn = Math.max(maxColumn, col);
    const bucket = columns.get(col) ?? [];
    bucket.push(v);
    columns.set(col, bucket);
  }

  const nodeBoxes: OrthogonalDagNodeBox[] = [];
  const boxById = new Map<string, OrthogonalDagNodeBox>();
  let maxRowCount = 1;

  for (let col = 0; col <= maxColumn; col += 1) {
    const bucket = columns.get(col) ?? [];
    maxRowCount = Math.max(maxRowCount, bucket.length);
    for (let row = 0; row < bucket.length; row += 1) {
      const v = bucket[row]!;
      const statutoryLock = getTaskNodeStatutoryLock(v.state);
      const editableForOverride =
        v.kind === "TASK_NODE" && isTaskNodeEditableForOverride(v.state);
      // Enforce Statutory Lock #3 on node-level verbs
      const prunedLockedVerbs = pruneVerbsAgainstStatutoryLock(
        ["DeleteNode", "ReplaceNode", "RewireEdge", "MergeNode"],
        statutoryLock,
      );
      const allowedOverrideVerbs: readonly PlanOverrideVerb[] =
        editableForOverride && prunedLockedVerbs.length > 0
          ? ALL_PLAN_OVERRIDE_VERBS
          : [];
      const xPx =
        DAG_PADDING_X_PX + col * (DAG_NODE_WIDTH_PX + DAG_COLUMN_GAP_PX);
      const yPx =
        DAG_PADDING_Y_PX + row * (DAG_NODE_HEIGHT_PX + DAG_ROW_GAP_PX);
      const box: OrthogonalDagNodeBox = {
        id: v.id,
        kind: v.kind,
        groupId: v.groupId,
        title: v.title,
        state: v.state,
        columnIndex: col,
        rowIndex: row,
        xPx,
        yPx,
        widthPx: DAG_NODE_WIDTH_PX,
        heightPx: DAG_NODE_HEIGHT_PX,
        editableForOverride,
        statutoryLock,
        allowedOverrideVerbs,
        residencyState: v.residencyState,
        runtimeExpanded: v.runtimeExpanded,
        unmaterializedStepCount: v.unmaterializedStepCount,
        badgeTextZh: v.badgeTextZh,
        badgeTextEn: v.badgeTextEn,
      };
      nodeBoxes.push(box);
      boxById.set(box.id, box);
    }
  }

  const edges: OrthogonalDagEdge[] = [];
  for (const v of vertices) {
    const targetBox = boxById.get(v.id);
    if (targetBox === undefined) {
      continue;
    }
    for (const depId of v.dependencyIds) {
      const sourceBox = boxById.get(depId);
      if (sourceBox === undefined) {
        continue;
      }
      const startX = sourceBox.xPx + sourceBox.widthPx;
      const startY = sourceBox.yPx + Math.round(sourceBox.heightPx / 2);
      const endX = targetBox.xPx;
      const endY = targetBox.yPx + Math.round(targetBox.heightPx / 2);
      const midX = Math.round((startX + endX) / 2);
      const points = [
        { xPx: startX, yPx: startY },
        { xPx: midX, yPx: startY },
        { xPx: midX, yPx: endY },
        { xPx: endX, yPx: endY },
      ];
      const svgPathD = `M ${startX} ${startY} H ${midX} V ${endY} H ${endX}`;
      edges.push({
        edgeId: `${depId}->${v.id}`,
        fromNodeId: depId,
        toNodeId: v.id,
        points,
        svgPathD,
      });
    }
  }

  const columnCount = vertices.length === 0 ? 0 : maxColumn + 1;
  const totalWidthPx =
    columnCount === 0
      ? DAG_PADDING_X_PX * 2
      : DAG_PADDING_X_PX * 2 +
        columnCount * DAG_NODE_WIDTH_PX +
        Math.max(0, columnCount - 1) * DAG_COLUMN_GAP_PX;
  const totalHeightPx =
    DAG_PADDING_Y_PX * 2 +
    maxRowCount * DAG_NODE_HEIGHT_PX +
    Math.max(0, maxRowCount - 1) * DAG_ROW_GAP_PX;

  return {
    direction: "LR",
    totalWidthPx,
    totalHeightPx,
    columnCount,
    nodeBoxes,
    edges,
  };
}

function getReferencedExistingNodeIds(op: PlanOverrideOperation): readonly string[] {
  switch (op.kind) {
    case "add_node":
      return [];
    case "remove_node":
      return [op.targetNodeId];
    case "pause_node":
      return [op.targetNodeId];
    case "merge_nodes":
      return [op.primaryNodeId, op.secondaryNodeId];
    case "reorder_nodes":
      return [op.nodeId];
    case "replace_node":
      return [op.targetNodeId];
  }
}

/**
 * Classifies and previews a batch of interactive `TaskPlan` DAG override operations:
 * - Rejects edits on `COMPLETED` or `ACTIVE` nodes (`READ_ONLY_EXECUTED_TASK_NODE` /
 *   Statutory Lock #3 `NODE_COMPLETED_OR_ACTIVE_LOCKED`).
 * - Rejects stale plan revisions (`STALE_GENERATION` carrying `currentGeneration: snapshot.planRevision`).
 * - Routes bounded `pause_node` / `reorder_nodes` within authorized budget/capabilities to
 *   **Path A (`BOUNDED_CONTROL_COMMAND`)**.
 * - Routes `add_node`, `replace_node`, `remove_node`, `merge_nodes`, or capability/budget/cross-app
 *   expansions to **Path B (`RECOMPILE_AND_Z4_REAUTHORIZE`)**, generating a Terraform-style
 *   `+ / ~ / -` `BlastRadiusDiffModel`.
 */
export function classifyPlanOverrideOperations(
  snapshot: TaskPlanDagSnapshot,
  expectedPlanRevision: number,
  operations: readonly PlanOverrideOperation[],
): PlanOverrideClassificationOutcome {
  if (expectedPlanRevision !== snapshot.planRevision) {
    return {
      ok: false,
      errorCode: "STALE_GENERATION",
      statutoryLock: null,
      rejectedNodeId: null,
      rejectedNodeState: null,
      currentGeneration: snapshot.planRevision,
      messageZh: `计划版本已过期（期望 r${expectedPlanRevision}，当前 r${snapshot.planRevision}），请先刷新最新投影`,
      messageEn: `Plan revision is stale (expected r${expectedPlanRevision}, current r${snapshot.planRevision}); refresh before applying overrides`,
    };
  }

  if (operations.length === 0) {
    return {
      ok: false,
      errorCode: "EMPTY_OVERRIDE_BATCH",
      statutoryLock: null,
      rejectedNodeId: null,
      rejectedNodeState: null,
      currentGeneration: snapshot.planRevision,
      messageZh: "未提供任何计划覆盖操作",
      messageEn: "No plan override operations were provided",
    };
  }

  const nodeMap = new Map<string, TaskNodeDagProjection>();
  for (const node of snapshot.nodes) {
    nodeMap.set(node.nodeId, node);
  }
  for (const supernode of snapshot.lazySupernodes) {
    for (const child of supernode.materializedNodes) {
      nodeMap.set(child.nodeId, child);
    }
  }

  for (const op of operations) {
    const targetIds = getReferencedExistingNodeIds(op);
    for (const targetId of targetIds) {
      const existing = nodeMap.get(targetId);
      if (existing === undefined) {
        return {
          ok: false,
          errorCode: "NOT_FOUND",
          statutoryLock: null,
          rejectedNodeId: targetId,
          rejectedNodeState: null,
          currentGeneration: snapshot.planRevision,
          messageZh: `未找到目标步骤节点 ${targetId}`,
          messageEn: `Target step node ${targetId} was not found`,
        };
      }
      if (!isTaskNodeEditableForOverride(existing.state)) {
        return {
          ok: false,
          errorCode: "READ_ONLY_EXECUTED_TASK_NODE",
          statutoryLock: "NODE_COMPLETED_OR_ACTIVE_LOCKED",
          rejectedNodeId: existing.nodeId,
          rejectedNodeState: existing.state,
          currentGeneration: snapshot.planRevision,
          messageZh: "已执行或正在执行的步骤禁止修改结构",
          messageEn: "Completed or active step is locked against structural edits",
        };
      }
    }
  }

  const authorizedCaps = new Set(snapshot.authorizedCapabilities);
  let currentTotalRequestedBudget = snapshot.nodes.reduce(
    (acc, n) => acc + n.requestedBudgetUnits,
    0,
  );
  let requiresRecompileAndZ4 = false;
  const diffRows: BlastRadiusRow[] = [];
  const compiledControlCommands: CompiledPlanControlCommand[] = [];

  for (let idx = 0; idx < operations.length; idx += 1) {
    const op = operations[idx]!;
    switch (op.kind) {
      case "pause_node": {
        const target = nodeMap.get(op.targetNodeId)!;
        compiledControlCommands.push({
          commandIdHex: deterministicHexDigest(
            `cmd:pause:${snapshot.planId}:${snapshot.planRevision}:${op.targetNodeId}:${idx}`,
          ),
          scopeKind: "TaskNode",
          targetId: op.targetNodeId,
          action: "PAUSE_TASK_NODE",
          expectedPlanRevision,
          idempotencyKeyHex: deterministicHexDigest(
            `idem:pause:${snapshot.planId}:${expectedPlanRevision}:${op.targetNodeId}`,
          ),
        });
        diffRows.push({
          symbol: "~",
          category: "paused",
          scopeLabel: "TaskNode",
          targetLabel: `${target.title} (${op.targetNodeId})`,
          reason: op.reason ?? "Bounded step pause within authorized plan envelope",
          targetIdHex: deterministicHexDigest(op.targetNodeId).slice(0, 32),
        });
        break;
      }

      case "reorder_nodes": {
        const target = nodeMap.get(op.nodeId)!;
        const withinEnvelope = op.withinAuthorizedEnvelope !== false;
        if (!withinEnvelope) {
          requiresRecompileAndZ4 = true;
        } else {
          compiledControlCommands.push({
            commandIdHex: deterministicHexDigest(
              `cmd:reorder:${snapshot.planId}:${snapshot.planRevision}:${op.nodeId}:${idx}`,
            ),
            scopeKind: "TaskPlan",
            targetId: snapshot.planId,
            action: "REORDER_TASK_PLAN_NODES",
            expectedPlanRevision,
            idempotencyKeyHex: deterministicHexDigest(
              `idem:reorder:${snapshot.planId}:${expectedPlanRevision}:${op.nodeId}:${op.newDependencyNodeIds.join(",")}`,
            ),
          });
        }
        diffRows.push({
          symbol: "~",
          category: "reverified",
          scopeLabel: "TaskNode",
          targetLabel: `${target.title} (${op.nodeId})`,
          reason: withinEnvelope
            ? `Reorder dependencies [${target.dependencyNodeIds.join(", ")}] -> [${op.newDependencyNodeIds.join(", ")}]`
            : `Cross-boundary dependency reorder requiring Z4 re-authorization`,
          targetIdHex: deterministicHexDigest(op.nodeId).slice(0, 32),
        });
        break;
      }

      case "add_node": {
        requiresRecompileAndZ4 = true;
        currentTotalRequestedBudget += op.requestedBudgetUnits;
        diffRows.push({
          symbol: "+",
          category: "created",
          scopeLabel: "TaskNode",
          targetLabel: `${op.title} (${op.nodeId})`,
          reason: `Add step in group ${op.groupId} (cap=${op.capabilityId}, budget=${op.requestedBudgetUnits})`,
          targetIdHex: deterministicHexDigest(op.nodeId).slice(0, 32),
        });
        if (!authorizedCaps.has(op.capabilityId)) {
          diffRows.push({
            symbol: "+",
            category: "reverified",
            scopeLabel: "Capability",
            targetLabel: op.capabilityId,
            reason: "New capability required by added TaskNode",
          });
        }
        if (op.crossApplicationTarget) {
          diffRows.push({
            symbol: "+",
            category: "reverified",
            scopeLabel: "CrossApplication",
            targetLabel: op.crossApplicationTarget,
            reason: "Cross-application collaboration target introduced",
          });
        }
        break;
      }

      case "remove_node": {
        const target = nodeMap.get(op.targetNodeId)!;
        if (op.altersAcceptanceOrSideEffects !== false) {
          requiresRecompileAndZ4 = true;
        }
        currentTotalRequestedBudget = Math.max(
          0,
          currentTotalRequestedBudget - target.requestedBudgetUnits,
        );
        diffRows.push({
          symbol: "-",
          category: "terminated",
          scopeLabel: "TaskNode",
          targetLabel: `${target.title} (${op.targetNodeId})`,
          reason: "Removed unexecuted step from TaskPlan DAG",
          targetIdHex: deterministicHexDigest(op.targetNodeId).slice(0, 32),
        });
        break;
      }

      case "merge_nodes": {
        const primary = nodeMap.get(op.primaryNodeId)!;
        const secondary = nodeMap.get(op.secondaryNodeId)!;
        if (
          op.crossScopeOrCapability !== false ||
          primary.capabilityId !== secondary.capabilityId ||
          primary.groupId !== secondary.groupId
        ) {
          requiresRecompileAndZ4 = true;
        }
        diffRows.push({
          symbol: "~",
          category: "reverified",
          scopeLabel: "TaskNode",
          targetLabel: `${op.mergedTitle} (${op.primaryNodeId} + ${op.secondaryNodeId})`,
          reason: `Merged ${primary.title} and ${secondary.title} in group ${primary.groupId}`,
          targetIdHex: deterministicHexDigest(op.primaryNodeId).slice(0, 32),
        });
        break;
      }

      case "replace_node": {
        const target = nodeMap.get(op.targetNodeId)!;
        requiresRecompileAndZ4 = true;
        currentTotalRequestedBudget =
          currentTotalRequestedBudget -
          target.requestedBudgetUnits +
          op.requestedBudgetUnits;
        diffRows.push({
          symbol: "~",
          category: "reverified",
          scopeLabel: "TaskNode",
          targetLabel: `${target.title} -> ${op.replacementTitle} (${op.replacementNodeId})`,
          reason: `Replace ${target.nodeId} with ${op.replacementNodeId} (cap=${op.capabilityId}, budget=${op.requestedBudgetUnits})`,
          targetIdHex: deterministicHexDigest(op.replacementNodeId).slice(0, 32),
        });
        if (!authorizedCaps.has(op.capabilityId)) {
          diffRows.push({
            symbol: "+",
            category: "reverified",
            scopeLabel: "Capability",
            targetLabel: op.capabilityId,
            reason: "New capability required by replacement TaskNode",
          });
        }
        if (op.crossApplicationTarget) {
          diffRows.push({
            symbol: "+",
            category: "reverified",
            scopeLabel: "CrossApplication",
            targetLabel: op.crossApplicationTarget,
            reason: "Cross-application collaboration target introduced",
          });
        }
        break;
      }
    }
  }

  if (currentTotalRequestedBudget > snapshot.authorizedBudgetCeilingUnits) {
    requiresRecompileAndZ4 = true;
    diffRows.push({
      symbol: "~",
      category: "throttled",
      scopeLabel: "BudgetEnvelope",
      targetLabel: "TaskPlan Authorized Budget Ceiling",
      reason: `Budget ceiling expands from ${snapshot.authorizedBudgetCeilingUnits} to ${currentTotalRequestedBudget} units`,
    });
  }

  const pathKind: PlanOverridePathKind = requiresRecompileAndZ4
    ? "RECOMPILE_AND_Z4_REAUTHORIZE"
    : "BOUNDED_CONTROL_COMMAND";
  const candidatePlanRevision = snapshot.planRevision + 1;
  const candidatePlanDigestHex = deterministicHexDigest(
    `${snapshot.planId}:r${candidatePlanRevision}:${snapshot.planDigestHex}:${JSON.stringify(operations)}`,
  );

  const blastRadiusDiff: BlastRadiusDiffModel = {
    title:
      pathKind === "RECOMPILE_AND_Z4_REAUTHORIZE"
        ? `TaskPlan Override Recompile Diff (r${snapshot.planRevision} -> r${candidatePlanRevision})`
        : `Bounded TaskPlan Control Diff (r${snapshot.planRevision} -> r${candidatePlanRevision})`,
    planRevision: candidatePlanRevision,
    rows: diffRows,
    requiresApproval: requiresRecompileAndZ4,
    approvalReason: requiresRecompileAndZ4
      ? "Structural step addition, replacement, capability expansion, or budget increase requires AdmissionPlan recompile and Z4 policy re-authorization."
      : undefined,
  };

  return {
    ok: true,
    pathKind,
    requiresZ4Reauthorization: requiresRecompileAndZ4,
    basePlanRevision: snapshot.planRevision,
    candidatePlanRevision,
    basePlanDigestHex: snapshot.planDigestHex,
    candidatePlanDigestHex,
    compiledControlCommands: requiresRecompileAndZ4 ? [] : compiledControlCommands,
    blastRadiusDiff,
    summaryZh: requiresRecompileAndZ4
      ? `路径 B：需要重编译 AdmissionPlan 并经策略引擎二次授权（新摘要 ${candidatePlanDigestHex.slice(0, 12)}…）`
      : `路径 A：有界控制命令（${compiledControlCommands.length} 条 ControlCommand，无需二次授权）`,
    summaryEn: requiresRecompileAndZ4
      ? `Path B: Recompile AdmissionPlan & require Z4 re-authorization (digest ${candidatePlanDigestHex.slice(0, 12)}…)`
      : `Path A: Bounded ControlCommand (${compiledControlCommands.length} command(s), within authorized envelope)`,
  };
}

function applyOperationsToSnapshot(
  snapshot: TaskPlanDagSnapshot,
  operations: readonly PlanOverrideOperation[],
  candidatePlanRevision: number,
  candidatePlanDigestHex: string,
): TaskPlanDagSnapshot {
  let nodes = [...snapshot.nodes];
  const authorizedCapabilities = new Set(snapshot.authorizedCapabilities);
  let authorizedBudgetCeilingUnits = snapshot.authorizedBudgetCeilingUnits;

  for (const op of operations) {
    switch (op.kind) {
      case "pause_node": {
        nodes = nodes.map((n) =>
          n.nodeId === op.targetNodeId ? { ...n, state: "SUSPENDED" } : n,
        );
        break;
      }
      case "reorder_nodes": {
        nodes = nodes.map((n) =>
          n.nodeId === op.nodeId
            ? { ...n, dependencyNodeIds: [...op.newDependencyNodeIds] }
            : n,
        );
        break;
      }
      case "add_node": {
        authorizedCapabilities.add(op.capabilityId);
        nodes.push({
          nodeId: op.nodeId,
          groupId: op.groupId,
          title: op.title,
          state: "DECLARED",
          dependencyNodeIds: [...op.dependencyNodeIds],
          capabilityId: op.capabilityId,
          requestedBudgetUnits: op.requestedBudgetUnits,
          effectiveBudgetUnits: op.requestedBudgetUnits,
          usageBudgetUnits: 0,
          residencyState: "HOT_MATERIALIZED",
          runtimeExpanded: false,
          crossApplicationTarget: op.crossApplicationTarget ?? null,
          assignedAttemptCount: 0,
        });
        break;
      }
      case "remove_node": {
        nodes = nodes
          .filter((n) => n.nodeId !== op.targetNodeId)
          .map((n) => ({
            ...n,
            dependencyNodeIds: n.dependencyNodeIds.filter(
              (dep) => dep !== op.targetNodeId,
            ),
          }));
        break;
      }
      case "merge_nodes": {
        const primary = nodes.find((n) => n.nodeId === op.primaryNodeId);
        const secondary = nodes.find((n) => n.nodeId === op.secondaryNodeId);
        if (primary && secondary) {
          const mergedDeps = Array.from(
            new Set([
              ...primary.dependencyNodeIds,
              ...secondary.dependencyNodeIds,
            ]),
          ).filter((d) => d !== op.primaryNodeId && d !== op.secondaryNodeId);
          nodes = nodes
            .filter((n) => n.nodeId !== op.secondaryNodeId)
            .map((n) => {
              if (n.nodeId === op.primaryNodeId) {
                return {
                  ...n,
                  title: op.mergedTitle,
                  dependencyNodeIds: mergedDeps,
                  requestedBudgetUnits:
                    primary.requestedBudgetUnits + secondary.requestedBudgetUnits,
                  effectiveBudgetUnits:
                    primary.effectiveBudgetUnits + secondary.effectiveBudgetUnits,
                };
              }
              return {
                ...n,
                dependencyNodeIds: n.dependencyNodeIds.map((dep) =>
                  dep === op.secondaryNodeId ? op.primaryNodeId : dep,
                ),
              };
            });
        }
        break;
      }
      case "replace_node": {
        authorizedCapabilities.add(op.capabilityId);
        nodes = nodes.map((n) => {
          if (n.nodeId === op.targetNodeId) {
            return {
              ...n,
              nodeId: op.replacementNodeId,
              title: op.replacementTitle,
              capabilityId: op.capabilityId,
              requestedBudgetUnits: op.requestedBudgetUnits,
              effectiveBudgetUnits: op.requestedBudgetUnits,
              crossApplicationTarget: op.crossApplicationTarget ?? null,
            };
          }
          return {
            ...n,
            dependencyNodeIds: n.dependencyNodeIds.map((dep) =>
              dep === op.targetNodeId ? op.replacementNodeId : dep,
            ),
          };
        });
        break;
      }
    }
  }

  const totalBudget = nodes.reduce((acc, n) => acc + n.requestedBudgetUnits, 0);
  if (totalBudget > authorizedBudgetCeilingUnits) {
    authorizedBudgetCeilingUnits = totalBudget;
  }

  return {
    ...snapshot,
    planRevision: candidatePlanRevision,
    planDigestHex: candidatePlanDigestHex,
    authorizedCapabilities: Array.from(authorizedCapabilities),
    authorizedBudgetCeilingUnits,
    nodes,
  };
}

/**
 * Compiles and commits a batch of `TaskPlan` override operations:
 * - Enforces Statutory Lock #3 (`NODE_COMPLETED_OR_ACTIVE_LOCKED`).
 * - Enforces `expectedPlanRevision` CAS (`STALE_GENERATION`).
 * - Enforces Path B `Z4ReauthorizationGrant` matching `(planId, expectedPlanRevision, candidatePlanDigestHex)`
 *   (`Z4_REAUTHORIZATION_REQUIRED`).
 */
export function submitPlanOverrideOperations(
  snapshot: TaskPlanDagSnapshot,
  expectedPlanRevision: number,
  operations: readonly PlanOverrideOperation[],
  z4Grant?: Z4ReauthorizationGrant | null,
): PlanOverrideSubmissionOutcome {
  const preview = classifyPlanOverrideOperations(
    snapshot,
    expectedPlanRevision,
    operations,
  );
  if (!preview.ok) {
    return preview;
  }

  if (preview.requiresZ4Reauthorization) {
    const validGrant =
      z4Grant !== undefined &&
      z4Grant !== null &&
      z4Grant.grantTokenHex.trim().length > 0 &&
      z4Grant.planId === snapshot.planId &&
      z4Grant.expectedPlanRevision === expectedPlanRevision &&
      z4Grant.approvedPlanDigestHex === preview.candidatePlanDigestHex;

    if (!validGrant) {
      return {
        ok: false,
        errorCode: "Z4_REAUTHORIZATION_REQUIRED",
        statutoryLock: null,
        rejectedNodeId: null,
        rejectedNodeState: null,
        currentGeneration: snapshot.planRevision,
        messageZh:
          "结构性计划重编排或预算/权限扩权必须先通过策略引擎（Z4）二次授权，禁止绕过 AdmissionPlan",
        messageEn:
          "Structural plan override or capability/budget expansion requires Z4 policy re-authorization and cannot bypass AdmissionPlan",
      };
    }
  }

  const updatedPlan = applyOperationsToSnapshot(
    snapshot,
    operations,
    preview.candidatePlanRevision,
    preview.candidatePlanDigestHex,
  );

  const receiptHex = deterministicHexDigest(
    `rcpt:plan_override:${snapshot.planId}:r${preview.candidatePlanRevision}:${preview.candidatePlanDigestHex}`,
  );
  const auditIdHex = deterministicHexDigest(
    `audit:plan_override:${snapshot.taskId}:r${preview.candidatePlanRevision}`,
  ).slice(0, 32);

  return {
    ok: true,
    pathKind: preview.pathKind,
    updatedPlan,
    receiptHex,
    auditIdHex,
    compiledControlCommands: preview.compiledControlCommands,
    z4GrantTokenHex: z4Grant?.grantTokenHex ?? null,
  };
}

export interface TaskPlanDagCanvasController {
  getSnapshot(): TaskPlanDagSnapshot;
  getLayout(): OrthogonalDagLayout;
  getLastPreview(): PlanOverridePreviewResult | null;
  getLastRejection(): PlanOverrideRejection | null;
  getLastReceiptHex(): string | null;
  expandLazySupernode(
    supernodeId: string,
    requestedLimit?: number,
    cursorHex?: string | null,
  ): {
    readonly expanded: boolean;
    readonly materializedNodes: readonly TaskNodeDagProjection[];
    readonly effectiveLimit: number;
  };
  evictSupernodeToMetadataOnly(supernodeId: string): boolean;
  previewOverride(
    operations: readonly PlanOverrideOperation[],
    expectedPlanRevision?: number,
  ): PlanOverrideClassificationOutcome;
  submitOverride(
    operations: readonly PlanOverrideOperation[],
    options?: {
      readonly expectedPlanRevision?: number;
      readonly z4Grant?: Z4ReauthorizationGrant | null;
    },
  ): PlanOverrideSubmissionOutcome;
  issueZ4GrantForPreview(
    preview: PlanOverridePreviewResult,
    policyEpoch?: number,
  ): Z4ReauthorizationGrant;
}

export function createTaskPlanDagCanvasController(
  initialSnapshot: TaskPlanDagSnapshot,
  cursorCoordinator?: TaskSpaceLazyCursorCoordinator,
): TaskPlanDagCanvasController {
  let snapshot: TaskPlanDagSnapshot = {
    ...initialSnapshot,
    groups: [...initialSnapshot.groups],
    nodes: [...initialSnapshot.nodes],
    lazySupernodes: initialSnapshot.lazySupernodes.map((s) => ({
      ...s,
      materializedNodes: [...s.materializedNodes],
    })),
  };
  let lastPreview: PlanOverridePreviewResult | null = null;
  let lastRejection: PlanOverrideRejection | null = null;
  let lastReceiptHex: string | null = null;

  return {
    getSnapshot(): TaskPlanDagSnapshot {
      return snapshot;
    },

    getLayout(): OrthogonalDagLayout {
      return computeOrthogonalDagLayout(snapshot);
    },

    getLastPreview(): PlanOverridePreviewResult | null {
      return lastPreview;
    },

    getLastRejection(): PlanOverrideRejection | null {
      return lastRejection;
    },

    getLastReceiptHex(): string | null {
      return lastReceiptHex;
    },

    expandLazySupernode(
      supernodeId: string,
      requestedLimit?: number,
      cursorHex: string | null = null,
    ) {
      const effectiveLimit = clampTaskSpaceCursorLimit(requestedLimit);
      if (cursorCoordinator !== undefined) {
        const gate = cursorCoordinator.expandCanvasSupernode(
          snapshot.taskId,
          supernodeId,
          effectiveLimit,
          cursorHex,
        );
        if (!gate.allowed) {
          return {
            expanded: false,
            materializedNodes: [],
            effectiveLimit,
          };
        }
      }

      let materializedResult: readonly TaskNodeDagProjection[] = [];
      let found = false;

      snapshot = {
        ...snapshot,
        lazySupernodes: snapshot.lazySupernodes.map((supernode) => {
          if (supernode.supernodeId !== supernodeId) {
            return supernode;
          }
          found = true;
          const countToMaterialize = Math.min(
            effectiveLimit,
            supernode.unmaterializedStepCount,
          );
          const generatedNodes: TaskNodeDagProjection[] =
            supernode.materializedNodes.length > 0
              ? supernode.materializedNodes.slice(0, effectiveLimit)
              : Array.from({ length: countToMaterialize }, (_, idx) => ({
                  nodeId: `${supernode.supernodeId}-step-${idx + 1}`,
                  groupId: supernode.groupId,
                  title: `${supernode.titleZh} #${idx + 1}`,
                  state: "DECLARED" as const,
                  dependencyNodeIds:
                    idx === 0
                      ? [...supernode.dependencyNodeIds]
                      : [`${supernode.supernodeId}-step-${idx}`],
                  capabilityId:
                    snapshot.authorizedCapabilities[0] ?? "cap.fs.workspace.read",
                  requestedBudgetUnits: 5,
                  effectiveBudgetUnits: 5,
                  usageBudgetUnits: 0,
                  residencyState: "HOT_MATERIALIZED" as const,
                  runtimeExpanded: false,
                  assignedAttemptCount: 0,
                }));
          materializedResult = generatedNodes;
          return {
            ...supernode,
            expanded: true,
            materializedNodes: generatedNodes,
            nextCursorHex:
              supernode.unmaterializedStepCount > countToMaterialize
                ? deterministicHexDigest(
                    `${supernodeId}:cursor:${countToMaterialize}`,
                  ).slice(0, 16)
                : null,
          };
        }),
      };

      return {
        expanded: found,
        materializedNodes: materializedResult,
        effectiveLimit,
      };
    },

    evictSupernodeToMetadataOnly(supernodeId: string): boolean {
      let updated = false;
      if (cursorCoordinator !== undefined) {
        cursorCoordinator.collapseCanvasSupernode(snapshot.taskId, supernodeId);
      }
      snapshot = {
        ...snapshot,
        lazySupernodes: snapshot.lazySupernodes.map((s) => {
          if (s.supernodeId !== supernodeId) {
            return s;
          }
          updated = true;
          return {
            ...s,
            expanded: false,
            residencyState: "METADATA_ONLY",
          };
        }),
      };
      return updated;
    },

    previewOverride(
      operations: readonly PlanOverrideOperation[],
      expectedPlanRevision: number = snapshot.planRevision,
    ): PlanOverrideClassificationOutcome {
      const outcome = classifyPlanOverrideOperations(
        snapshot,
        expectedPlanRevision,
        operations,
      );
      if (outcome.ok) {
        lastPreview = outcome;
        lastRejection = null;
      } else {
        lastRejection = outcome;
      }
      return outcome;
    },

    submitOverride(
      operations: readonly PlanOverrideOperation[],
      options?: {
        readonly expectedPlanRevision?: number;
        readonly z4Grant?: Z4ReauthorizationGrant | null;
      },
    ): PlanOverrideSubmissionOutcome {
      const expectedRev = options?.expectedPlanRevision ?? snapshot.planRevision;
      const outcome = submitPlanOverrideOperations(
        snapshot,
        expectedRev,
        operations,
        options?.z4Grant,
      );
      if (outcome.ok) {
        snapshot = outcome.updatedPlan;
        lastReceiptHex = outcome.receiptHex;
        lastRejection = null;
      } else {
        lastRejection = outcome;
      }
      return outcome;
    },

    issueZ4GrantForPreview(
      preview: PlanOverridePreviewResult,
      policyEpoch = 1,
    ): Z4ReauthorizationGrant {
      return {
        grantTokenHex: deterministicHexDigest(
          `z4_grant:${snapshot.planId}:r${preview.basePlanRevision}:${preview.candidatePlanDigestHex}:epoch${policyEpoch}`,
        ),
        planId: snapshot.planId,
        expectedPlanRevision: preview.basePlanRevision,
        approvedPlanDigestHex: preview.candidatePlanDigestHex,
        authorizerPolicyEpoch: policyEpoch,
      };
    },
  };
}

export interface RenderTaskPlanDagCanvasOptions {
  readonly snapshot: TaskPlanDagSnapshot;
  readonly locale?: WorkstationLocale;
  readonly activePreview?: PlanOverridePreviewResult | null;
  readonly activeRejection?: PlanOverrideRejection | null;
  readonly selectedNodeId?: string | null;
}

/**
 * Renders the `L3` Dagster-style Left-to-Right (`LR`) orthogonal `TaskPlan` DAG canvas HTML.
 */
export function renderTaskPlanDagCanvasHtml(
  options: RenderTaskPlanDagCanvasOptions,
): string {
  const locale = options.locale ?? "zh-CN";
  const snapshot = options.snapshot;
  const layout = computeOrthogonalDagLayout(snapshot);

  const groupsHtml = snapshot.groups
    .map(
      (g) =>
        `<div class="nlos-dag-group-chip" data-group-id="${escapeHtml(g.groupId)}" data-topology="${escapeHtml(g.topology)}">` +
        `<span class="nlos-dag-group-chip__title">${escapeHtml(g.title)}</span>` +
        `<span class="nlos-dag-group-chip__meta" data-tabular-nums="true">${escapeHtml(g.topology)} · #${g.schedulingOrder} · ${escapeHtml(g.failurePolicy)}</span>` +
        (g.runtimeExpanded
          ? `<span class="nlos-dag-runtime-expanded-badge" data-runtime-expanded="true">RuntimeExpanded</span>`
          : "") +
        `</div>`,
    )
    .join("");

  const edgesSvgHtml = layout.edges
    .map(
      (edge) =>
        `<path class="nlos-dag-edge-path" data-edge-id="${escapeHtml(edge.edgeId)}" data-from-node="${escapeHtml(edge.fromNodeId)}" data-to-node="${escapeHtml(edge.toNodeId)}" d="${escapeHtml(edge.svgPathD)}" fill="none" stroke="currentColor" stroke-width="1.5" />`,
    )
    .join("");

  const nodesHtml = layout.nodeBoxes
    .map((box) => {
      const selectedAttr =
        options.selectedNodeId === box.id ? ` data-selected="true"` : "";
      const statusHtml = renderStatusIndicatorHtml({
        state: box.state,
        locale,
        disclosure: "L3",
      });

      const statutoryBadgeHtml =
        box.statutoryLock !== null
          ? renderStatutoryLockBadgeHtml({
              kind: box.statutoryLock,
              locale,
              disclosure: "L3",
            })
          : "";

      const lazyBadgeText =
        locale === "en-US" ? box.badgeTextEn : box.badgeTextZh;
      const lazyBadgeHtml =
        lazyBadgeText !== null
          ? `<span class="nlos-dag-node__badge" data-supernode-badge="${box.kind === "LAZY_SUPERNODE" ? "true" : "false"}">${escapeHtml(lazyBadgeText)}</span>`
          : "";

      const verbsHtml =
        box.kind === "LAZY_SUPERNODE"
          ? `<div class="nlos-dag-node__actions">` +
            `<button type="button" class="nlos-dag-node__btn" data-dag-action="expand-supernode" data-supernode-id="${escapeHtml(box.id)}">${locale === "en-US" ? "Expand (<=64)" : "按需展开 (<=64)"}</button>` +
            `<button type="button" class="nlos-dag-node__btn" data-dag-action="materialize-supernode" data-supernode-id="${escapeHtml(box.id)}">${locale === "en-US" ? "Materialize" : "显式物化"}</button>` +
            `</div>`
          : box.editableForOverride
            ? `<div class="nlos-dag-node__actions" data-override-toolbar="true">` +
              box.allowedOverrideVerbs
                .map(
                  (verb) =>
                    `<button type="button" class="nlos-dag-node__btn" data-dag-override-verb="${verb}" data-node-id="${escapeHtml(box.id)}">${escapeHtml(verb)}</button>`,
                )
                .join("") +
              `</div>`
            : `<div class="nlos-dag-node__locked-toolbar" data-statutory-lock="NODE_COMPLETED_OR_ACTIVE_LOCKED">${statutoryBadgeHtml}</div>`;

      return (
        `<article class="nlos-dag-node" data-node-id="${escapeHtml(box.id)}" data-node-kind="${box.kind}" data-node-state="${box.state}" data-node-editable="${box.editableForOverride ? "true" : "false"}" data-residency="${box.residencyState}"${box.statutoryLock ? ` data-statutory-lock="${box.statutoryLock}"` : ""}${selectedAttr} style="left:${box.xPx}px;top:${box.yPx}px;width:${box.widthPx}px;height:${box.heightPx}px;">` +
        `<header class="nlos-dag-node__header">` +
        statusHtml +
        `<span class="nlos-dag-node__title">${escapeHtml(box.title)}</span>` +
        `<span class="nlos-dag-node__id" data-mono="true">${escapeHtml(box.id)}</span>` +
        `</header>` +
        `<div class="nlos-dag-node__meta">` +
        lazyBadgeHtml +
        `<span class="nlos-dag-node__residency" data-mono="true">${escapeHtml(box.residencyState)}</span>` +
        `</div>` +
        verbsHtml +
        `</article>`
      );
    })
    .join("");

  const blastDiffOptions: BlastRadiusDiffOptions | null = options.activePreview
    ? {
        rows: options.activePreview.blastRadiusDiff.rows,
        operationKind: "plan-reauthorize",
        locale,
        disclosure: "L3",
      }
    : null;

  const previewHtml =
    options.activePreview && blastDiffOptions
      ? `<section class="nlos-dag-override-preview" data-override-path="${options.activePreview.pathKind}" data-requires-z4="${options.activePreview.requiresZ4Reauthorization ? "true" : "false"}">` +
        `<div class="nlos-dag-override-preview__summary">${escapeHtml(
          locale === "en-US"
            ? options.activePreview.summaryEn
            : options.activePreview.summaryZh,
        )}</div>` +
        renderBlastRadiusDiffHtml(blastDiffOptions) +
        `</section>`
      : "";

  const rejectionHtml = options.activeRejection
    ? `<section class="nlos-dag-override-rejection" role="alert" data-error-code="${options.activeRejection.errorCode}"${options.activeRejection.statutoryLock ? ` data-statutory-lock="${options.activeRejection.statutoryLock}"` : ""}>` +
      `<span class="nlos-dag-override-rejection__icon">${renderIconSvg("lock", { size: 14 })}</span>` +
      `<span class="nlos-dag-override-rejection__msg">${escapeHtml(
        locale === "en-US"
          ? options.activeRejection.messageEn
          : options.activeRejection.messageZh,
      )}</span>` +
      `</section>`
    : "";

  const summaryCounts = blastDiffOptions
    ? computeBlastRadiusSummary(blastDiffOptions)
    : null;

  return (
    `<section class="nlos-task-dag-canvas" data-surface-id="task-plan-dag-canvas" data-dag-direction="${layout.direction}" data-plan-id="${escapeHtml(snapshot.planId)}" data-plan-revision="${snapshot.planRevision}" data-plan-digest="${escapeHtml(snapshot.planDigestHex)}">` +
    `<header class="nlos-task-dag-canvas__header">` +
    `<div class="nlos-task-dag-canvas__title-group">` +
    `<span class="nlos-task-dag-canvas__icon">${renderIconSvg("tree-expand", { size: 14 })}</span>` +
    `<span class="nlos-task-dag-canvas__title">${locale === "en-US" ? "TaskPlan Orthogonal DAG Editor (LR)" : "TaskPlan 正交依赖图与重编排画布 (LR)"}</span>` +
    `<span class="nlos-task-dag-canvas__revision" data-mono="true" data-tabular-nums="true">plan_rev=r${snapshot.planRevision} · gen=${snapshot.installationGeneration} · digest=${escapeHtml(snapshot.planDigestHex.slice(0, 12))}…</span>` +
    `</div>` +
    (summaryCounts
      ? `<div class="nlos-task-dag-canvas__diff-pill" data-tabular-nums="true">+${summaryCounts.addedCount} / ~${summaryCounts.modifiedCount} / -${summaryCounts.removedCount}</div>`
      : "") +
    `</header>` +
    `<div class="nlos-task-dag-canvas__groups">${groupsHtml}</div>` +
    `<div class="nlos-task-dag-canvas__viewport" style="width:${layout.totalWidthPx}px;height:${layout.totalHeightPx}px;">` +
    `<svg class="nlos-task-dag-canvas__edges" width="${layout.totalWidthPx}" height="${layout.totalHeightPx}" viewBox="0 0 ${layout.totalWidthPx} ${layout.totalHeightPx}" aria-hidden="true">${edgesSvgHtml}</svg>` +
    `<div class="nlos-task-dag-canvas__nodes">${nodesHtml}</div>` +
    `</div>` +
    rejectionHtml +
    previewHtml +
    `</section>`
  );
}

export function createTaskPlanDagCanvasElement(
  doc: Document,
  options: RenderTaskPlanDagCanvasOptions,
): HTMLElement {
  const wrapper = doc.createElement("div");
  wrapper.innerHTML = renderTaskPlanDagCanvasHtml(options);
  return (wrapper.firstElementChild as HTMLElement | null) ?? wrapper;
}
