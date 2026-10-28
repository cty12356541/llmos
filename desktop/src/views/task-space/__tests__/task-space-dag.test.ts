// Stage D W72-D-TASK-DAG (`D-TASK-SPACE-DAG`) — Comprehensive Verification Suite.
// Covers:
//   1. `TASK_SPACE_GAPS` (1–3) closure verification.
//   2. `O(visible_rows)` `L0 -> L1 -> L2 -> L3` lazy-pulling cursor coordinator (`limit <= 64`).
//   3. `L1` 16-state commitment table, Nomad 3-watermark bullet micro-bar, `[RCPT-UNDO-001]`
//      `IRREVERSIBLE` statutory lock, `[RCPT-PARTIAL-001]` partial failure badge, `[TASK-REOPEN-001]`
//      terminal retry/refine, and Temporal folded phase milestones.
//   4. `L2` non-modal single-row Headlamp Attribution Drawer (`Space`/`Enter`/row-click, `Escape`).
//   5. `L3` Dagster-style Left-to-Right (`LR`) orthogonal `TaskPlan` DAG canvas, `[PLAN-LAZY-001]`
//      `+N 待物化步骤 · 0 进程` super-node on-demand expansion, Statutory Lock #3
//      (`NODE_COMPLETED_OR_ACTIVE_LOCKED`), and Path A vs Path B + `Z4` re-authorization diff preview.
//   6. `L3` Temporal `TaskAttempt` racing swimlanes, Statutory Lock #4 (`FORBIDDEN_MANUAL_ATTEMPT_COMMIT`),
//      `ExecutionFiber` phase duration waterfall, `SHARED_THREAD_LIKE` fault domain badge,
//      `NOT_FOUND` vs `STALE_GENERATION`, and `ReceiptAuditBar`.
//   7. `ANTI-AI-01..07` static CSS/source audit & `L1`/`L2` zero-jargon gate (`zh-CN` & `en-US`).

import {
  clampTaskSpaceCursorLimit,
  createDisclosureModel,
  createTaskSpaceLazyCursorCoordinator,
} from "../../../store/disclosure-model";
import { CANONICAL_TASK_STATES } from "../../../ui/status-indicator";
import {
  ALLOWED_TASK_ATTEMPT_CONTROL_VERBS,
  CLOSED_TASK_SPACE_GAPS,
  TASK_SPACE_GAPS,
  attemptManualCommitOverride,
  bindTaskSpaceToCompositor,
  classifyPlanOverrideOperations,
  computeTaskAttemptSwimlanesModel,
  createDefaultTaskSpaceWorkstationRows,
  createTaskSpaceSurfaceController,
  executeTaskAttemptControlAction,
  formatLayerInspectionDiagnosticBanner,
  formatLazySupernodeBadgeText,
  isTaskNodeEditableForOverride,
  pruneTaskAttemptControlVerbs,
  renderTaskSpaceSurfaceHtml,
  reopenTerminalTaskInModel,
  submitPlanOverrideOperations,
} from "../index";

interface FsPromisesLike {
  readFile(path: string, encoding: "utf8"): Promise<string>;
}

interface PathModuleLike {
  resolve(...segments: string[]): string;
}

const dynamicImport = new Function(
  "specifier",
  "return import(specifier);",
) as (specifier: string) => Promise<unknown>;

function assert(condition: boolean, message: string): void {
  if (!condition) {
    throw new Error(`Assertion failed: ${message}`);
  }
}

function assertEqual<T>(actual: T, expected: T, message: string): void {
  if (actual !== expected) {
    throw new Error(
      `${message}: expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`,
    );
  }
}

function extractVisibleTextAndAriaLabels(html: string): string {
  const ariaMatches: string[] = [];
  const ariaRegex = /aria-label="([^"]*)"/g;
  let match: RegExpExecArray | null = ariaRegex.exec(html);
  while (match !== null) {
    if (match[1]) {
      ariaMatches.push(match[1]);
    }
    match = ariaRegex.exec(html);
  }
  const textOnly = html
    .replace(/<script[\s\S]*?<\/script>/gi, " ")
    .replace(/<style[\s\S]*?<\/style>/gi, " ")
    .replace(/<[^>]+>/g, " ")
    .replace(/\s+/g, " ")
    .trim();
  return `${textOnly} ${ariaMatches.join(" ")}`;
}

// ============================================================================
// Suite 1: Closure of `TASK_SPACE_GAPS` (1–3)
// ============================================================================
function testTaskSpaceGapsClosure(): void {
  assertEqual(
    TASK_SPACE_GAPS.length,
    0,
    "TASK_SPACE_GAPS must be empty after IPC-D1..D3 closure",
  );
  assertEqual(
    CLOSED_TASK_SPACE_GAPS.length,
    3,
    "CLOSED_TASK_SPACE_GAPS must document all 3 historical gaps",
  );
  assertEqual(CLOSED_TASK_SPACE_GAPS[0]?.id, "GAP-1", "Gap 1 ID");
  assertEqual(CLOSED_TASK_SPACE_GAPS[1]?.id, "GAP-2", "Gap 2 ID");
  assertEqual(CLOSED_TASK_SPACE_GAPS[2]?.id, "GAP-3", "Gap 3 ID");
}

// ============================================================================
// Suite 2: `O(visible_rows)` `L0 -> L1 -> L2 -> L3` Lazy Cursor Coordinator
// ============================================================================
function testLazyCursorCoordinator(): void {
  assertEqual(
    clampTaskSpaceCursorLimit(500),
    64,
    "Cursor limit must clamp to <= 64 (MAX_SYSTEM_CONTROL_ENUM_LIMIT)",
  );
  assertEqual(clampTaskSpaceCursorLimit(0), 64, "Invalid 0 limit defaults to 64");
  assertEqual(clampTaskSpaceCursorLimit(20), 20, "Valid limit preserved");

  const model = createDisclosureModel({
    initialLevel: "L1",
    viewportHeightPx: 450,
    rowHeightPx: 30,
  });
  const coordinator = createTaskSpaceLazyCursorCoordinator(model);

  // L1 pull plan
  const l1Plan = coordinator.computePullPlan();
  assertEqual(l1Plan.level, "L1", "Initial level is L1");
  assertEqual(l1Plan.visibleRowLimit, 15, "450px / 30px = 15 visible rows");
  assertEqual(l1Plan.zeroL2PrefetchGuaranteed, true, "Zero L2 prefetch at L1");
  assertEqual(l1Plan.zeroL3PrefetchGuaranteed, true, "Zero L3 prefetch at L1");
  assertEqual(l1Plan.allowedSlices.length, 1, "Only list_tasks slice allowed at L1");
  assertEqual(l1Plan.allowedSlices[0]?.queryKind, "list_tasks", "L1 slice is list_tasks");

  // Attempting L2 or L3 pull at L1 is blocked
  const blockedL2AtL1 = coordinator.requestSlicePull("list_task_nodes", {
    targetEntityId: "task-01",
  });
  assertEqual(blockedL2AtL1.allowed, false, "L2 query blocked at L1");
  assertEqual(
    blockedL2AtL1.reason,
    "BLOCKED_TIER_AT_CURRENT_DISCLOSURE",
    "Blocked reason at L1",
  );

  const blockedSupernodeAtL1 = coordinator.expandCanvasSupernode(
    "task-01",
    "super-1",
    100,
  );
  assertEqual(
    blockedSupernodeAtL1.allowed,
    false,
    "Supernode expansion blocked at L1",
  );

  // Transition L1 -> L2 for single row "task-01"
  model.toggleDrawerL2("task-01");
  const l2Plan = coordinator.computePullPlan();
  assertEqual(l2Plan.level, "L2", "Level is L2");
  assertEqual(l2Plan.selectedTaskId, "task-01", "Selected task is task-01");
  assertEqual(l2Plan.zeroL3PrefetchGuaranteed, true, "Zero L3 prefetch at L2");
  assert(
    l2Plan.allowedSlices.some(
      (s) =>
        s.queryKind === "list_task_nodes" &&
        s.parentEntityId === "task-01" &&
        s.expandLazySupernodes === false,
    ),
    "L2 pulls list_task_nodes for selected task without expanding lazy supernodes",
  );

  // Pulling L2 for an unselected row ("task-02") at L2 is blocked
  const unselectedAtL2 = coordinator.requestSlicePull("list_task_nodes", {
    targetEntityId: "task-02",
  });
  assertEqual(unselectedAtL2.allowed, false, "Unselected row L2 pull blocked");
  assertEqual(
    unselectedAtL2.reason,
    "BLOCKED_UNSELECTED_ROW_AT_L2",
    "Blocked unselected row reason",
  );

  // Pulling L3 at L2 is blocked
  const blockedL3AtL2 = coordinator.requestSlicePull("list_task_attempts", {
    targetEntityId: "task-01",
  });
  assertEqual(blockedL3AtL2.allowed, false, "L3 query blocked at L2");

  // Supernode expansion at L2 is still blocked (strictly L3 DAG canvas only)
  const blockedSupernodeAtL2 = coordinator.expandCanvasSupernode(
    "task-01",
    "super-1",
    32,
  );
  assertEqual(
    blockedSupernodeAtL2.allowed,
    false,
    "Supernode expansion blocked at L2",
  );

  // Transition L2 -> L3 via Ctrl/Cmd+Shift+E
  const handled = model.handleDisclosureKeydown({
    key: "E",
    ctrlKey: true,
    shiftKey: true,
  });
  assertEqual(handled, true, "Ctrl+Shift+E handled");
  assertEqual(model.getLevel(), "L3", "Level is L3");
  assert(
    model.getSnapshot().lastTransitionDurationMs < 16,
    "In-place L3 transition completes in < 16ms",
  );

  // On-demand supernode expansion at L3 succeeds and clamps limit <= 64
  const expandedSuper = coordinator.expandCanvasSupernode(
    "task-01",
    "super-1",
    128,
    "0011223344556677",
  );
  assertEqual(expandedSuper.allowed, true, "Supernode expansion allowed at L3");
  assertEqual(
    expandedSuper.cursor?.limit,
    64,
    "Supernode expansion limit clamped to 64",
  );

  const l3Plan = coordinator.computePullPlan();
  assertEqual(l3Plan.level, "L3", "Pull plan at L3");
  assert(
    l3Plan.allowedSlices.some((s) => s.queryKind === "list_task_attempts"),
    "L3 pull plan includes list_task_attempts",
  );
  assert(
    l3Plan.allowedSlices.some((s) => s.queryKind === "list_execution_fibers"),
    "L3 pull plan includes list_execution_fibers",
  );
  assert(
    l3Plan.allowedSlices.some(
      (s) => s.supernodeId === "super-1" && s.expandLazySupernodes === true,
    ),
    "L3 pull plan includes expanded supernode slice",
  );
}

// ============================================================================
// Suite 3: `L1` Task Commitment Table, Statutory Lock #6, & `[TASK-REOPEN-001]`
// ============================================================================
function testL1CommitmentTableAndReopen(): void {
  const controller = createTaskSpaceSurfaceController();
  const rows = controller.getRows();
  assertEqual(
    rows.length,
    CANONICAL_TASK_STATES.length,
    "Default workstation rows cover all 16 canonical TaskStates",
  );

  const l1Html = controller.renderHtml("zh-CN");
  for (const state of CANONICAL_TASK_STATES) {
    assert(
      l1Html.includes(`data-task-state="${state}"`),
      `L1 table renders row for canonical state ${state}`,
    );
  }
  assert(
    l1Html.includes(`width="6" height="6"`),
    "L1 status column uses 6x6px geometric SVG glyphs",
  );
  assert(
    l1Html.includes(`data-phase-folded="true"`),
    "L1 goal column includes Temporal folded phase milestone summary",
  );
  assert(
    l1Html.includes(`role="meter"`),
    "L1 resource column renders Nomad 3-watermark bullet micro-bars",
  );
  assert(
    l1Html.includes(`data-undo-class="IRREVERSIBLE" data-undo-omitted="true"`),
    "IRREVERSIBLE rows mark data-undo-omitted=true",
  );
  assert(
    l1Html.includes(`data-task-action="view-effect-proof"`),
    "IRREVERSIBLE rows expose View Effect Proof button instead of Undo",
  );
  assert(
    l1Html.includes(`data-statutory-lock="IRREVERSIBLE_NO_UNDO"`),
    "IRREVERSIBLE rows render statutory lock badge",
  );

  // Verify [TASK-REOPEN-001]: Retry spawns new TaskAttempt; Refine spawns new Task generation
  const failedRow = rows.find((r) => r.summary.taskState === "FAILED")!;
  const initialAttempts = failedRow.l3AttemptLanes.attempts.length;
  const retryOutcome = controller.reopenTerminalTask(
    failedRow.summary.taskId,
    "retry",
  );
  assertEqual(retryOutcome.ok, true, "Retry on FAILED task succeeds");
  assertEqual(retryOutcome.mode, "retry", "Mode is retry");
  assert(
    retryOutcome.spawnedAttemptId !== null,
    "Retry spawns a new TaskAttempt ID",
  );
  const updatedFailedRow = controller
    .getRows()
    .find((r) => r.summary.taskId === failedRow.summary.taskId)!;
  assertEqual(
    updatedFailedRow.l3AttemptLanes.attempts.length,
    initialAttempts + 1,
    "Prior attempts are preserved and new attempt is appended",
  );

  const cancelledRow = rows.find((r) => r.summary.taskState === "CANCELLED")!;
  const beforeRowCount = controller.getRows().length;
  const refineOutcome = controller.reopenTerminalTask(
    cancelledRow.summary.taskId,
    "refine",
    "微调重开归档任务",
    "Refined Archive Task",
  );
  assertEqual(refineOutcome.ok, true, "Refine on CANCELLED task succeeds");
  assertEqual(
    refineOutcome.newInstallationGeneration,
    cancelledRow.summary.installationGeneration + 1,
    "Refine increments installationGeneration",
  );
  assertEqual(
    refineOutcome.newPlanRevision,
    cancelledRow.summary.planRevision + 1,
    "Refine increments planRevision",
  );
  assertEqual(
    controller.getRows().length,
    beforeRowCount + 1,
    "Refine appends a new Task generation without overwriting history",
  );
  assert(
    refineOutcome.preservedHistoryTaskIds.includes(cancelledRow.summary.taskId),
    "Original task ID remains in preserved history",
  );

  // Non-terminal task without partial failures cannot be reopened
  const runningRow = rows.find(
    (r) => r.summary.taskState === "RUNNING" && r.summary.failedTargets === 0,
  )!;
  const invalidReopen = reopenTerminalTaskInModel(
    rows,
    runningRow.summary.taskId,
    "retry",
  );
  assertEqual(
    invalidReopen.outcome.ok,
    false,
    "Healthy RUNNING task rejects terminal reopen",
  );
  assertEqual(
    invalidReopen.outcome.errorCode,
    "TASK_NOT_TERMINAL_OR_PARTIAL",
    "Error code is TASK_NOT_TERMINAL_OR_PARTIAL",
  );
}

// ============================================================================
// Suite 4: `L2` Non-Modal Single-Row Attribution Drawer (`Headlamp Inspector`)
// ============================================================================
function testL2SingleRowAttributionDrawer(): void {
  const controller = createTaskSpaceSurfaceController();
  const pullPlan = controller.selectTaskRow("task-02");
  assertEqual(pullPlan.level, "L2", "Clicking a row enters L2");
  assertEqual(pullPlan.selectedTaskId, "task-02", "Selected task is task-02");
  assertEqual(
    pullPlan.zeroL3PrefetchGuaranteed,
    true,
    "L2 drawer never prefetches L3 payloads",
  );

  const l2Html = controller.renderHtml("zh-CN");
  assert(
    l2Html.includes(`class="nlos-task-l2-drawer"`),
    "L2 renders non-modal right-hand attribution drawer",
  );
  assert(
    l2Html.includes(`data-drawer-disclosure="L2"`),
    "Inspector drawer is in L2 mode",
  );
  assert(
    l2Html.includes(`data-scope-kind="Application"`),
    "L2 breadcrumb includes Application",
  );
  assert(
    l2Html.includes(`data-scope-kind="Task"`),
    "L2 breadcrumb includes Task",
  );
  assert(
    !l2Html.includes(`data-scope-kind="TaskAttempt"`),
    "L2 breadcrumb hides L3-only TaskAttempt scope",
  );
  assert(
    !l2Html.includes(`data-scope-kind="Fiber"`),
    "L2 breadcrumb hides L3-only Fiber scope",
  );
  assert(
    l2Html.includes(`data-partial-breakdown="true"`),
    "L2 drawer renders per-target partial breakdown",
  );

  // Escape closes drawer back to L1 while preserving selectedTaskId
  const closed = controller.closeAttributionDrawer();
  assertEqual(closed, true, "closeAttributionDrawer returns true at L2");
  assertEqual(
    controller.getDisclosureModel().getLevel(),
    "L1",
    "Level returns to L1 on Escape",
  );
  assertEqual(
    controller.getSelectedTaskId(),
    "task-02",
    "Selected task ID is preserved on Escape",
  );
}

// ============================================================================
// Suite 5: `L3` Dagster Orthogonal `LR` DAG Canvas, `[PLAN-LAZY-001]`,
// Statutory Lock #3, and 2-Path Override Classifier
// ============================================================================
function testL3DagCanvasAndOverrides(): void {
  const controller = createTaskSpaceSurfaceController();
  controller.selectTaskRow("task-01");
  const nextLevel = controller.toggleDiagnosticL3();
  assertEqual(nextLevel, "L3", "Toggled into L3 diagnostic mode");

  const dagCtrl = controller.getDagController("task-01")!;
  const layout = dagCtrl.getLayout();
  assertEqual(layout.direction, "LR", "DAG layout is Left-to-Right (LR)");
  assert(
    layout.columnCount >= 4,
    "DAG layout assigns topological columns left-to-right",
  );
  assert(layout.edges.length >= 4, "DAG layout computes orthogonal edges");
  for (const edge of layout.edges) {
    assert(
      edge.svgPathD.includes(" H ") && edge.svgPathD.includes(" V "),
      `Edge ${edge.edgeId} uses orthogonal horizontal/vertical routing`,
    );
  }

  // Verify [PLAN-LAZY-001] collapsed supernode
  const superBox = layout.nodeBoxes.find((b) => b.kind === "LAZY_SUPERNODE")!;
  assertEqual(
    superBox.badgeTextZh,
    formatLazySupernodeBadgeText(18, "zh-CN"),
    "Collapsed supernode badge shows +18 待物化步骤 · 0 进程",
  );
  assertEqual(
    superBox.badgeTextEn,
    formatLazySupernodeBadgeText(18, "en-US"),
    "Collapsed supernode English badge shows +18 Unmaterialized Steps · 0 Processes",
  );

  // Expand supernode on demand at L3 with limit clamped <= 64
  const expandRes = controller.expandSupernodeOnDemand(superBox.id, 99);
  assertEqual(expandRes.expanded, true, "Supernode expands on demand at L3");
  assertEqual(
    expandRes.effectiveLimit,
    64,
    "Supernode expansion limit clamped to 64",
  );
  assertEqual(
    expandRes.materializedCount,
    18,
    "Materialized 18 child steps from supernode",
  );

  // Evict supernode back to METADATA_ONLY
  const evicted = dagCtrl.evictSupernodeToMetadataOnly(superBox.id);
  assertEqual(evicted, true, "Supernode evicted back to METADATA_ONLY");

  // Statutory Lock #3: COMPLETED and ACTIVE nodes are strictly read-only
  assertEqual(
    isTaskNodeEditableForOverride("COMPLETED"),
    false,
    "COMPLETED node is not editable",
  );
  assertEqual(
    isTaskNodeEditableForOverride("ACTIVE"),
    false,
    "ACTIVE node is not editable",
  );
  assertEqual(
    isTaskNodeEditableForOverride("DECLARED"),
    true,
    "DECLARED node is editable",
  );

  const rejectedCompleted = controller.previewPlanOverride([
    {
      kind: "remove_node",
      targetNodeId: "plan-01-node-completed",
    },
  ]);
  assertEqual(
    rejectedCompleted.ok,
    false,
    "Removing a COMPLETED node must be rejected",
  );
  if (!rejectedCompleted.ok) {
    assertEqual(
      rejectedCompleted.errorCode,
      "READ_ONLY_EXECUTED_TASK_NODE",
      "Rejection code is READ_ONLY_EXECUTED_TASK_NODE",
    );
    assertEqual(
      rejectedCompleted.statutoryLock,
      "NODE_COMPLETED_OR_ACTIVE_LOCKED",
      "Statutory Lock #3 attached",
    );
  }

  const rejectedActive = controller.submitPlanOverride([
    {
      kind: "replace_node",
      targetNodeId: "plan-01-node-active",
      replacementNodeId: "plan-01-node-evil",
      replacementTitle: "非法替换运行中节点",
      capabilityId: "cap.fs.workspace.read",
      requestedBudgetUnits: 10,
    },
  ]);
  assertEqual(
    rejectedActive.ok,
    false,
    "Replacing an ACTIVE node must be rejected",
  );
  if (!rejectedActive.ok) {
    assertEqual(
      rejectedActive.statutoryLock,
      "NODE_COMPLETED_OR_ACTIVE_LOCKED",
      "ACTIVE node rejection carries NODE_COMPLETED_OR_ACTIVE_LOCKED",
    );
  }

  // Path A (BOUNDED_CONTROL_COMMAND): pause_node & reorder_nodes on unexecuted nodes
  const pathAPreview = controller.previewPlanOverride([
    {
      kind: "pause_node",
      targetNodeId: "plan-01-node-pending-a",
      reason: "Operator temporary hold",
    },
    {
      kind: "reorder_nodes",
      nodeId: "plan-01-node-pending-b",
      newDependencyNodeIds: ["plan-01-node-active"],
      withinAuthorizedEnvelope: true,
    },
  ]);
  assertEqual(pathAPreview.ok, true, "Path A preview succeeds");
  if (pathAPreview.ok) {
    assertEqual(
      pathAPreview.pathKind,
      "BOUNDED_CONTROL_COMMAND",
      "Classified as Path A BOUNDED_CONTROL_COMMAND",
    );
    assertEqual(
      pathAPreview.requiresZ4Reauthorization,
      false,
      "Path A does not require Z4 reauthorization",
    );
    assertEqual(
      pathAPreview.compiledControlCommands.length,
      2,
      "Path A compiles 2 typed ControlCommands",
    );
  }

  const pathACommit = controller.submitPlanOverride([
    {
      kind: "pause_node",
      targetNodeId: "plan-01-node-pending-a",
      reason: "Operator temporary hold",
    },
  ]);
  assertEqual(pathACommit.ok, true, "Path A commit succeeds without Z4 token");
  if (pathACommit.ok) {
    assertEqual(
      pathACommit.updatedPlan.planRevision,
      4,
      "Plan revision increments from 3 to 4",
    );
    assert(pathACommit.receiptHex.length > 0, "Path A emits receipt_hex");
  }

  // Stale generation check on plan revision
  const staleOutcome = classifyPlanOverrideOperations(
    dagCtrl.getSnapshot(),
    3, // stale! current is 4
    [{ kind: "pause_node", targetNodeId: "plan-01-node-pending-b" }],
  );
  assertEqual(staleOutcome.ok, false, "Stale plan revision rejected");
  if (!staleOutcome.ok) {
    assertEqual(
      staleOutcome.errorCode,
      "STALE_GENERATION",
      "Error code is STALE_GENERATION",
    );
    assertEqual(
      staleOutcome.currentGeneration,
      4,
      "Carries currentGeneration = 4",
    );
  }

  // Path B (RECOMPILE_AND_Z4_REAUTHORIZE): add_node / replace_node / merge_nodes / budget expansion
  const pathBLinearOps = [
    {
      kind: "add_node" as const,
      nodeId: "plan-01-node-added",
      groupId: "plan-01-grp-commit",
      title: "跨应用安全审计同步",
      dependencyNodeIds: ["plan-01-node-pending-b"],
      capabilityId: "cap.net.audit.export",
      requestedBudgetUnits: 50,
      crossApplicationTarget: "app.security.auditor",
    },
    {
      kind: "merge_nodes" as const,
      primaryNodeId: "plan-01-node-pending-a",
      secondaryNodeId: "plan-01-node-pending-b",
      mergedTitle: "合并阶段报告与归档摘要",
      crossScopeOrCapability: true,
    },
  ];

  const pathBPreview = controller.previewPlanOverride(pathBLinearOps);
  assertEqual(pathBPreview.ok, true, "Path B preview succeeds");
  if (pathBPreview.ok) {
    assertEqual(
      pathBPreview.pathKind,
      "RECOMPILE_AND_Z4_REAUTHORIZE",
      "Classified as Path B RECOMPILE_AND_Z4_REAUTHORIZE",
    );
    assertEqual(
      pathBPreview.requiresZ4Reauthorization,
      true,
      "Path B requires Z4 re-authorization",
    );
    assert(
      pathBPreview.blastRadiusDiff.rows.length >= 3,
      "Path B generates Terraform-style + / ~ / - BlastRadiusRow diff",
    );

    // Submitting Path B without Z4 grant is deterministically rejected
    const missingZ4 = controller.submitPlanOverride(pathBLinearOps);
    assertEqual(
      missingZ4.ok,
      false,
      "Path B submission without Z4 grant must be rejected",
    );
    if (!missingZ4.ok) {
      assertEqual(
        missingZ4.errorCode,
        "Z4_REAUTHORIZATION_REQUIRED",
        "Error code is Z4_REAUTHORIZATION_REQUIRED",
      );
    }

    // Submitting Path B with valid Z4 grant succeeds
    const z4Grant = dagCtrl.issueZ4GrantForPreview(pathBPreview, 9);
    const authorizedCommit = submitPlanOverrideOperations(
      dagCtrl.getSnapshot(),
      pathBPreview.basePlanRevision,
      pathBLinearOps,
      z4Grant,
    );
    assertEqual(
      authorizedCommit.ok,
      true,
      "Path B submission with valid Z4 grant succeeds",
    );
    if (authorizedCommit.ok) {
      assertEqual(
        authorizedCommit.updatedPlan.planRevision,
        5,
        "Plan revision advances to 5",
      );
      assertEqual(
        authorizedCommit.z4GrantTokenHex,
        z4Grant.grantTokenHex,
        "Commit receipt records Z4 grant token",
      );
    }
  }
}

// ============================================================================
// Suite 6: `L3` `TaskAttempt` Racing Swimlanes, Statutory Lock #4, & Fiber Waterfall
// ============================================================================
function testL3AttemptSwimlanesAndFiberWaterfall(): void {
  const controller = createTaskSpaceSurfaceController();
  controller.selectTaskRow("task-16");
  controller.toggleDiagnosticL3();

  const selectedRow = controller.getSelectedRow()!;
  const swimlanesModel = computeTaskAttemptSwimlanesModel(
    selectedRow.l3AttemptLanes,
  );
  assertEqual(
    swimlanesModel.raceModel.spans.length,
    2,
    "2 racing TaskAttempts computed",
  );
  assertEqual(
    swimlanesModel.raceModel.statutoryManualCommitForbidden,
    true,
    "Statutory manual commit lock enforced on race model",
  );

  // Statutory Lock #4: Manual commit on TaskAttempt is strictly forbidden
  const prunedVerbs = pruneTaskAttemptControlVerbs([
    ...ALLOWED_TASK_ATTEMPT_CONTROL_VERBS,
    "manual_commit",
    "force_commit",
    "mark_winner",
  ]);
  assertEqual(
    prunedVerbs.length,
    3,
    "Only cancel_attempt, supersede_attempt, request_reverify survive pruning",
  );

  const manualCommitRejection = controller.attemptForbiddenManualCommit(
    "task-16-attempt-1",
  );
  assertEqual(
    manualCommitRejection.ok,
    false,
    "Manual commit override rejected",
  );
  assertEqual(
    manualCommitRejection.errorCode,
    "FORBIDDEN_MANUAL_ATTEMPT_COMMIT",
    "Error code is FORBIDDEN_MANUAL_ATTEMPT_COMMIT",
  );
  assertEqual(
    manualCommitRejection.statutoryLock,
    "TASK_ATTEMPT_NO_MANUAL_COMMIT",
    "Statutory Lock #4 attached",
  );

  const directRejection = attemptManualCommitOverride(
    selectedRow.l3AttemptLanes,
    "task-16-attempt-2",
  );
  assertEqual(
    directRejection.errorCode,
    "FORBIDDEN_MANUAL_ATTEMPT_COMMIT",
    "Direct attemptManualCommitOverride rejected",
  );

  // Allowed verb (request_reverify) succeeds
  const allowedOutcome = executeTaskAttemptControlAction(
    selectedRow.l3AttemptLanes,
    "task-16-attempt-1",
    "request_reverify",
  );
  assertEqual(allowedOutcome.ok, true, "request_reverify succeeds");

  const l3Html = controller.renderHtml("zh-CN");
  assert(
    l3Html.includes(`data-badge-kind="CAS_WINNER"`),
    "L3 renders CAS_WINNER badge",
  );
  assert(
    l3Html.includes(`data-badge-kind="SUPERSEDED"`),
    "L3 renders SUPERSEDED badge",
  );
  assert(
    l3Html.includes(`data-badge-kind="PROVIDER_ISOLATION_UNVERIFIED"`),
    "L3 renders PROVIDER_ISOLATION_UNVERIFIED badge",
  );
  assert(
    l3Html.includes(`data-fault-domain="SHARED_THREAD_LIKE"`),
    "L3 renders SHARED_THREAD_LIKE fault domain badge for shared-process fibers",
  );
  assert(
    l3Html.includes(`data-statutory-lock="OPERATION_EFFECT_UNKNOWN_NO_MANUAL_SUCCESS"`),
    "L3 renders Statutory Lock #5 on EFFECT_UNKNOWN operations",
  );
  assert(
    l3Html.includes(`data-loss-reason="STALE_GENERATION"`),
    "L3 renders STALE_GENERATION diagnostic banner with current_generation",
  );
  assert(
    formatLayerInspectionDiagnosticBanner("NOT_FOUND", null, "en-US").includes(
      "NOT_FOUND",
    ),
    "NOT_FOUND banner distinct from STALE_GENERATION",
  );
  assert(
    l3Html.includes(`data-component="receipt-audit-bar"`),
    "L3 renders cryptographic ReceiptAuditBar",
  );

  // Compositor binding check
  let focusedLandmark: string | null = null;
  const boundCtrl = bindTaskSpaceToCompositor(
    {
      getDisclosureModel: () => controller.getDisclosureModel(),
      focusLandmark: (id) => {
        focusedLandmark = id;
      },
    },
    controller,
  );
  assertEqual(boundCtrl, controller, "Bound controller returned");
  assertEqual(
    focusedLandmark,
    "z1-task-space",
    "Compositor focused z1-task-space landmark",
  );
}

// ============================================================================
// Suite 7: `ANTI-AI-01..07` Static Audit & `L1`/`L2` Zero-Jargon Gate
// ============================================================================
async function testAntiAiAndZeroJargonGates(): Promise<void> {
  const fsPromises = (await dynamicImport("node:fs/promises")) as FsPromisesLike;
  const pathMod = (await dynamicImport("node:path")) as PathModuleLike;

  const filesToAudit = [
    pathMod.resolve("src/store/disclosure-model.ts"),
    pathMod.resolve("src/views/task-space/dag-canvas.ts"),
    pathMod.resolve("src/views/task-space/attempt-lanes.ts"),
    pathMod.resolve("src/views/task-space/task-space.css"),
    pathMod.resolve("src/views/task-space/index.ts"),
  ];

  const emojiRegex = /\p{Extended_Pictographic}/u;
  const allowedRadii = new Set(["0", "0px", "2px", "4px", "6px"]);

  for (const filePath of filesToAudit) {
    const content = await fsPromises.readFile(filePath, "utf8");
    assert(
      !content.includes("backdrop-filter"),
      `Zero backdrop-filter allowed in ${filePath}`,
    );
    assert(
      !/#030710|#7fc8ff/i.test(content),
      `Zero legacy neon colors allowed in ${filePath}`,
    );
    assert(
      !emojiRegex.test(content),
      `Zero emoji characters allowed in ${filePath}`,
    );

    if (filePath.endsWith(".css")) {
      const radiusRegex = /border-radius\s*:\s*([^;]+);/gi;
      let match: RegExpExecArray | null = radiusRegex.exec(content);
      while (match !== null) {
        const val = (match[1] ?? "").trim();
        assert(
          allowedRadii.has(val),
          `Disallowed border-radius "${val}" in ${filePath}`,
        );
        match = radiusRegex.exec(content);
      }
    }
  }

  // Zero internal jargon gate at L1 and L2 across both zh-CN and en-US
  const forbiddenJargonTokens = [
    "Z1",
    "Z2",
    "Z3",
    "Z4",
    "TypedIntent",
    "0x1F",
    "Quota",
    "TaskAttempt",
    "Fiber",
    "ExecutionFiber",
    "Cell",
    "receipt_hex",
    "fencing_token",
    "plan_digest",
    "control_command_id",
    "CAS_WINNER",
    "SHARED_THREAD_LIKE",
    "LOCAL_SINGLE_NODE",
  ];

  const rows = createDefaultTaskSpaceWorkstationRows();
  for (const locale of ["zh-CN", "en-US"] as const) {
    for (const level of ["L1", "L2"] as const) {
      const html = renderTaskSpaceSurfaceHtml({
        rows,
        selectedTaskId: "task-01",
        disclosureLevel: level,
        locale,
      });
      const visibleText = extractVisibleTextAndAriaLabels(html);
      for (const token of forbiddenJargonTokens) {
        const tokenRegex = new RegExp(`\\b${token}\\b`, "i");
        assert(
          !tokenRegex.test(visibleText),
          `Forbidden internal jargon "${token}" leaked into ${level} visible text (${locale})`,
        );
      }
    }
  }
}

async function runAllTests(): Promise<void> {
  testTaskSpaceGapsClosure();
  testLazyCursorCoordinator();
  testL1CommitmentTableAndReopen();
  testL2SingleRowAttributionDrawer();
  testL3DagCanvasAndOverrides();
  testL3AttemptSwimlanesAndFiberWaterfall();
  await testAntiAiAndZeroJargonGates();
  console.log(
    "[W72-D-TASK-DAG] All 7 Task Space L1->L3, DAG Override, Attempt Swimlanes & Anti-AI suites PASSED.",
  );
}

void runAllTests().catch((err: unknown) => {
  console.error("[W72-D-TASK-DAG] Test failure:", err);
  const proc = (globalThis as { process?: { exitCode?: number } }).process;
  if (proc) {
    proc.exitCode = 1;
  }
  throw err;
});
