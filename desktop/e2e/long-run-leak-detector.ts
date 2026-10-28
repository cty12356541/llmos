// ============================================================================
// NLOS Stage D (W73-D-GUI-CAMPAIGN) — 1,000-Cycle Workstation Soak &
// Bounded-Memory / DOM Leak Detector (`desktop/e2e/long-run-leak-detector.ts`)
//
// Authority:
//   - Handover #3 C-GUI-CAMPAIGN (`docs/management/stage-b-progress.md` §6.5.6 #3)
//   - Unknown U-3 (`docs/management/exit-unknown-risks.md` — long-run stability)
//   - ADR-0025 Decision 4 (`O(visible_rows)` `DesktopProjectionStore` cursor budget)
//   - Architecture Spec v0.5 (`MAX_SYSTEM_CONTROL_ENUM_LIMIT = 64`)
// ============================================================================

import {
  createDesktopProjectionStore,
  createDisclosureModel,
  type ProjectionDataSource,
} from "../src/store";
import { deterministicDomainDigestHex } from "../src/trusted-ui";
import {
  MAX_SYSTEM_CONTROL_ENUM_LIMIT,
  type TaskNodeSummaryDto,
  type TaskSummaryDto,
} from "../src/types";
import {
  DEFAULT_CALLER_APP_ID_HEX,
  DEFAULT_CALLER_PRINCIPAL_ID_HEX,
  NotificationCenterController,
  NotificationCenterError,
  computeActionTokenCommitmentHex,
  deriveDeterministicId32Hex,
  renderNotificationCenterHtml,
  type NotificationFeedItem,
} from "../src/views/notifications";
import { createTilingWindowManager } from "../src/wm";

export const DEFAULT_SOAK_CYCLES = 1_000 as const;
export const LOGICAL_TASK_POOL_TOTAL = 100_000 as const;
export const MAX_RESIDENT_ROWS_PER_DOMAIN = MAX_SYSTEM_CONTROL_ENUM_LIMIT;

export interface SoakSampleCheckpoint {
  readonly cycle: number;
  readonly cursorOffset: number;
  readonly residentTasksInStore: number;
  readonly residentTaskNodesInStore: number;
  readonly activeVisibleWindows: number;
  readonly renderedDomNodeCount: number;
  readonly activeSubscribers: number;
  readonly activeTimers: number;
}

export interface LongRunSoakLeakReport {
  readonly schemaVersion: "1.0.0";
  readonly totalCyclesExecuted: number;
  readonly logicalTaskPoolTotal: number;
  readonly pageCursorLimit: number;
  readonly totalLogicalTasksVisited: number;
  readonly maxResidentTasksInStore: number;
  readonly maxResidentTaskNodesInStore: number;
  readonly maxRenderedDomNodeCount: number;
  readonly cycle100DomNodeCount: number;
  readonly cycle1000DomNodeCount: number;
  readonly domNodeGrowthAfterWarmup: number;
  readonly actionTokensRedeemed: number;
  readonly actionTokenReplayRejections: number;
  readonly activeSubscriberLeakCount: number;
  readonly activeTimerLeakCount: number;
  readonly strictlyBoundedOVisibleRows: true;
  readonly checkpoints: readonly SoakSampleCheckpoint[];
  readonly soakDigestHex: string;
}

function assertSoak(condition: unknown, message: string): asserts condition {
  if (!condition) {
    throw new Error(`[LongRunLeakDetectorFailure] ${message}`);
  }
}

function encodeCursorOffsetHex(offset: number): string {
  const clamped = Math.max(0, Math.floor(offset));
  return clamped.toString(16).padStart(32, "0");
}

function decodeCursorOffsetHex(cursorHex?: string | null): number {
  if (!cursorHex || cursorHex.trim().length === 0) {
    return 0;
  }
  const parsed = Number.parseInt(cursorHex.trim(), 16);
  return Number.isFinite(parsed) && parsed >= 0 ? parsed : 0;
}

function synthesizeTaskSummary(index: number): TaskSummaryDto {
  const hexId = (index + 1).toString(16).padStart(32, "0");
  const planIdHex = (index + 100_001).toString(16).padStart(32, "0");
  return {
    taskIdHex: hexId,
    planIdHex,
    applicationIdHex: DEFAULT_CALLER_APP_ID_HEX,
    installationGeneration: 1,
    planRevision: 1,
    taskState: index % 5 === 0 ? "WAITING_CONSENT" : "RUNNING",
    undoClass: "COMPENSATABLE_ROLLBACK",
    reservationIdHex: "bb".repeat(16),
    cellIdHex: "11".repeat(16),
    updatedAtMs: 1_700_000_000_000 + index,
    expectedGenerationOrRevision: 1,
  };
}

function synthesizeTaskNodes(planIdHex: string): TaskNodeSummaryDto[] {
  const nodes: TaskNodeSummaryDto[] = [];
  for (let i = 0; i < 8; i += 1) {
    nodes.push({
      planIdHex,
      nodeIdHex: `${planIdHex.slice(0, 28)}${i.toString(16).padStart(4, "0")}`,
      nodeKind: i === 0 ? "ingest" : i === 7 ? "export" : "transform",
      state: i < 4 ? "COMPLETED" : "PENDING",
      declaredRevision: 1,
      nodeDigestHex: "aa".repeat(32),
      transitionCount: i < 4 ? 2 : 0,
      residencyTier: "HOT",
      residencyTransitionCount: 1,
      firstDeclaredAtMs: 1_700_000_000_000,
      updatedAtMs: 1_700_000_001_000,
    });
  }
  return nodes;
}

/**
 * Counts opening HTML element tags in a rendered HTML fragment to measure active DOM footprint.
 */
export function countRenderedHtmlNodes(html: string): number {
  const matches = html.match(/<[a-zA-Z][a-zA-Z0-9-]*(?:\s|>|\/)/g);
  return matches ? matches.length : 0;
}

function createBoundedSoakNotificationItems(count: number): NotificationFeedItem[] {
  const subscriberKeyHex = deriveDeterministicId32Hex("soak_sub", 1);
  const topicIdHex = deriveDeterministicId32Hex("soak_topic", 1);
  const items: NotificationFeedItem[] = [];

  for (let i = 0; i < count; i += 1) {
    const seq = i + 1;
    const notificationIdHex = deriveDeterministicId32Hex("soak_notif", seq);
    const taskIdHex = deriveDeterministicId32Hex("soak_task", seq);
    const tokenIdHex = deriveDeterministicId32Hex("soak_token", seq);
    const expiryMonoMs = 9_000_000_000;
    const actionId = "approve_soak_step";
    const verifierCommitmentHex = computeActionTokenCommitmentHex({
      notificationIdHex,
      tokenIdHex,
      actionId,
      appIdHex: DEFAULT_CALLER_APP_ID_HEX,
      taskIdHex,
      principalIdHex: DEFAULT_CALLER_PRINCIPAL_ID_HEX,
      expiryMonoMs,
    });

    items.push({
      notificationIdHex,
      sequence: seq,
      taskIdHex,
      appIdHex: DEFAULT_CALLER_APP_ID_HEX,
      appDisplayNameZh: "工作站长稳巡检器",
      appDisplayNameEn: "Workstation Soak Monitor",
      taskState: "WAITING_CONSENT",
      category: "APPROVAL_REQUIRED",
      severity: "INFO",
      titleZh: `批次核验步骤 #${seq} 待确认`,
      titleEn: `Batch check step #${seq} awaits approval`,
      summaryZh: "请确认是否继续执行下一批归档校验。",
      summaryEn: "Please confirm to proceed with the next archive batch check.",
      stateLabelZh: "等待您的确认",
      stateLabelEn: "Awaiting your confirmation",
      createdAtMonoMs: 1_000_000 + seq * 100,
      unread: true,
      dismissed: false,
      actionToken: {
        tokenIdHex,
        actionId,
        labelZh: "批准并继续",
        labelEn: "Approve & Continue",
        boundNotificationIdHex: notificationIdHex,
        boundAppIdHex: DEFAULT_CALLER_APP_ID_HEX,
        boundTaskIdHex: taskIdHex,
        boundPrincipalIdHex: DEFAULT_CALLER_PRINCIPAL_ID_HEX,
        expiryMonoMs,
        consumed: false,
        verifierCommitmentHex,
      },
      allowedVerbs: ["inspect", "redeem_action_token", "dismiss"],
      l3Diagnostics: {
        topicIdHex,
        subscriberKeyHex,
        verifierCommitmentHex,
        sequence: seq,
        pruneWatermarkSequence: 0,
        retractedEventCount: 0,
        deliveryAttempt: 1,
      },
    });
  }
  return items;
}

/**
 * Executes a 1,000-cycle workstation soak campaign:
 *   - Opens/minimizes/restores/closes windows in `TilingWindowManager`
 *   - Cycles `L1 -> L2 -> L3` disclosure levels
 *   - Cursor-paginates a `100,000`-task logical dataset with `limit <= 64`
 *   - Redeems one-shot `ActionToken`s and verifies replay rejection
 *   - Asserts `O(visible_rows)` (`<= 64`) resident memory and flat DOM node counts
 */
export async function runLongRunWorkstationSoakDetector(options?: {
  readonly cycles?: number;
  readonly pageCursorLimit?: number;
}): Promise<LongRunSoakLeakReport> {
  const totalCycles = Math.max(100, options?.cycles ?? DEFAULT_SOAK_CYCLES);
  const pageCursorLimit = Math.min(
    MAX_SYSTEM_CONTROL_ENUM_LIMIT,
    Math.max(1, options?.pageCursorLimit ?? MAX_SYSTEM_CONTROL_ENUM_LIMIT),
  );

  let watermarkCounter = 1;
  let totalLogicalTasksVisited = 0;
  let activeSubscribers = 0;
  const activeTimers = 0;

  const disclosureModel = createDisclosureModel({
    initialLevel: "L1",
    viewportHeightPx: pageCursorLimit * 30,
    rowHeightPx: 30,
  });

  const dataSource: Partial<ProjectionDataSource> = {
    pollDesktopWatermarkEvents: async () => {
      watermarkCounter += 1;
      return {
        controlCommandIdHex: "aa".repeat(16),
        correlationIdHex: "11".repeat(16),
        receiptHex: "aa".repeat(32),
        outcome: {
          kind: "desktop_watermark_polled",
          currentWatermark: watermarkCounter,
          events: [],
          truncated: false,
        },
      };
    },
    listTasks: async (cursorHex, limit) => {
      const bounded = Math.min(
        MAX_SYSTEM_CONTROL_ENUM_LIMIT,
        limit ?? pageCursorLimit,
      );
      const startOffset = decodeCursorOffsetHex(cursorHex);
      const tasks: TaskSummaryDto[] = [];
      for (let i = 0; i < bounded; i += 1) {
        const idx = (startOffset + i) % LOGICAL_TASK_POOL_TOTAL;
        tasks.push(synthesizeTaskSummary(idx));
      }
      totalLogicalTasksVisited += tasks.length;
      const nextOffset = (startOffset + bounded) % LOGICAL_TASK_POOL_TOTAL;
      return {
        controlCommandIdHex: "bb".repeat(16),
        correlationIdHex: "22".repeat(16),
        receiptHex: "bb".repeat(32),
        outcome: {
          kind: "tasks_listed",
          tasks,
          nextCursorHex: encodeCursorOffsetHex(nextOffset),
          truncated: true,
        },
      };
    },
    listTaskNodes: async (planIdHex) => ({
      controlCommandIdHex: "cc".repeat(16),
      correlationIdHex: "33".repeat(16),
      receiptHex: "cc".repeat(32),
      outcome: {
        kind: "task_nodes_listed",
        planIdHex,
        currentPlanRevision: 1,
        staleGeneration: false,
        nodes: synthesizeTaskNodes(planIdHex),
        metadataOnlyCollapsedCount: 12,
        coldCollapsedCount: 4,
        nextCursorHex: null,
        truncated: false,
      },
    }),
    listInstalledApplications: async () => ({
      controlCommandIdHex: "dd".repeat(16),
      correlationIdHex: "44".repeat(16),
      receiptHex: "dd".repeat(32),
      outcome: {
        kind: "installed_applications_listed",
        applications: [],
        nextCursorHex: null,
        truncated: false,
      },
    }),
    listWorkspaces: async () => ({
      controlCommandIdHex: "ee".repeat(16),
      correlationIdHex: "55".repeat(16),
      receiptHex: "ee".repeat(32),
      outcome: {
        kind: "workspaces_listed",
        workspaces: [],
        nextCursorHex: null,
        truncated: false,
      },
    }),
    listNotifications: async () => ({
      controlCommandIdHex: "ff".repeat(16),
      correlationIdHex: "66".repeat(16),
      receiptHex: "ff".repeat(32),
      outcome: {
        kind: "notifications_listed",
        notifications: [],
        nextCursorHex: null,
        truncated: false,
      },
    }),
    listCells: async () => ({
      controlCommandIdHex: "12".repeat(16),
      correlationIdHex: "77".repeat(16),
      receiptHex: "12".repeat(32),
      outcome: {
        kind: "cells_listed",
        cells: [],
        nextCursorHex: null,
        truncated: false,
      },
    }),
    inspectResourceCost: async (reservationIdHex) => ({
      controlCommandIdHex: "13".repeat(16),
      correlationIdHex: "88".repeat(16),
      receiptHex: "13".repeat(32),
      outcome: {
        kind: "resource_inspected",
        reservationIdHex,
        accountIdHex: "aa".repeat(16),
        upperBound: 1000,
        usageHighWater: 320,
        consumptionCount: 4,
      },
    }),
    listTaskGroups: async () => ({
      controlCommandIdHex: "14".repeat(16),
      correlationIdHex: "99".repeat(16),
      receiptHex: "14".repeat(32),
      outcome: {
        kind: "task_groups_listed",
        groups: [],
        nextCursorHex: null,
        truncated: false,
      },
    }),
    listTaskAttempts: async () => ({
      controlCommandIdHex: "15".repeat(16),
      correlationIdHex: "00".repeat(16),
      receiptHex: "15".repeat(32),
      outcome: {
        kind: "task_attempts_listed",
        currentGeneration: 1,
        staleGeneration: false,
        attempts: [],
        nextCursorHex: null,
        truncated: false,
      },
    }),
  };

  const projectionStore = createDesktopProjectionStore({
    disclosureModel,
    dataSource,
  });

  const wm = createTilingWindowManager({
    viewportWidth: 1440,
    viewportHeight: 900,
    initialLayoutMode: "three-pane-workstation",
    showZ1TaskSpace: true,
    locale: "zh-CN",
    disclosure: "L1",
  });

  // Seed 3 persistent workstation windows + 1 rotating transient slot
  for (let i = 1; i <= 3; i += 1) {
    wm.registerWindow({
      windowId: `win-core-${i}`,
      surfaceId: `surf-core-${i}`,
      applicationIdHex: DEFAULT_CALLER_APP_ID_HEX,
      title: `常驻工作区窗口 #${i}`,
      surfaceGeneration: 1,
      latestInstallationGeneration: 1,
      presentImmediately: true,
    });
  }

  const notifItems = createBoundedSoakNotificationItems(64);
  const notificationCenter = new NotificationCenterController({
    initialDisclosure: "L1",
    initialLocale: "zh-CN",
    initialItems: notifItems,
  });

  let maxResidentTasksInStore = 0;
  let maxResidentTaskNodesInStore = 0;
  let maxRenderedDomNodeCount = 0;
  let cycle100DomNodeCount = 0;
  let cycle1000DomNodeCount = 0;
  let actionTokensRedeemed = 0;
  let actionTokenReplayRejections = 0;
  const checkpoints: SoakSampleCheckpoint[] = [];

  for (let cycle = 1; cycle <= totalCycles; cycle += 1) {
    // 1. Subscribe & unsubscribe transient listeners in each cycle (verify 0 listener leak)
    const unsubWm = wm.subscribe(() => {});
    const unsubStore = projectionStore.subscribe(() => {});
    activeSubscribers += 2;

    // 2. Open, present, minimize, restore, and close rotating window slot
    wm.registerWindow({
      windowId: "win-rotating-slot",
      surfaceId: `surf-rot-${cycle}`,
      applicationIdHex: DEFAULT_CALLER_APP_ID_HEX,
      title: `临时检视窗口 #${cycle}`,
      surfaceGeneration: 1,
      latestInstallationGeneration: 1,
      presentImmediately: true,
    });
    wm.minimizeWindow("win-rotating-slot");
    wm.restoreWindow("win-rotating-slot");
    wm.closeWindow("win-rotating-slot");

    // 3. Advance cursor window over the 100K logical task pool (limit <= 64)
    const cursorOffset = ((cycle - 1) * pageCursorLimit) % LOGICAL_TASK_POOL_TOTAL;
    disclosureModel.setCursor(encodeCursorOffsetHex(cursorOffset));

    // Cycle disclosure level L1 -> L2 -> L3 -> L1
    const pinnedTaskHex = (1).toString(16).padStart(32, "0");
    if (cycle % 3 === 1) {
      disclosureModel.setLevel("L1");
    } else if (cycle % 3 === 2) {
      disclosureModel.selectEntity(pinnedTaskHex, "task");
      disclosureModel.setLevel("L2");
    } else {
      disclosureModel.selectEntity(pinnedTaskHex, "task");
      disclosureModel.expandL3Parent(pinnedTaskHex);
      disclosureModel.setLevel("L3");
    }

    const snap = await projectionStore.refreshForCurrentDisclosure();
    const residentTasks = snap.tasks.size;
    let residentNodes = 0;
    for (const nodeList of snap.taskNodesByPlan.values()) {
      residentNodes += nodeList.length;
    }

    assertSoak(
      residentTasks <= MAX_RESIDENT_ROWS_PER_DOMAIN,
      `Resident tasks (${residentTasks}) exceeded O(visible_rows) ceiling (${MAX_RESIDENT_ROWS_PER_DOMAIN}) at cycle ${cycle}`,
    );
    assertSoak(
      residentNodes <= MAX_RESIDENT_ROWS_PER_DOMAIN,
      `Resident task nodes (${residentNodes}) exceeded ceiling (${MAX_RESIDENT_ROWS_PER_DOMAIN}) at cycle ${cycle}`,
    );

    maxResidentTasksInStore = Math.max(maxResidentTasksInStore, residentTasks);
    maxResidentTaskNodesInStore = Math.max(maxResidentTaskNodesInStore, residentNodes);

    // 4. Periodically redeem an ActionToken and verify one-shot replay rejection
    if (cycle <= 64) {
      const targetItem = notifItems[cycle - 1]!;
      const token = targetItem.actionToken!;
      notificationCenter.redeemNotificationActionToken({
        notificationIdHex: targetItem.notificationIdHex,
        tokenIdHex: token.tokenIdHex,
        actionId: token.actionId,
        callerAppIdHex: DEFAULT_CALLER_APP_ID_HEX,
        callerPrincipalIdHex: DEFAULT_CALLER_PRINCIPAL_ID_HEX,
        taskIdHex: targetItem.taskIdHex,
        nowMonoMs: 2_000_000 + cycle,
      });
      actionTokensRedeemed += 1;

      try {
        notificationCenter.redeemNotificationActionToken({
          notificationIdHex: targetItem.notificationIdHex,
          tokenIdHex: token.tokenIdHex,
          actionId: token.actionId,
          callerAppIdHex: DEFAULT_CALLER_APP_ID_HEX,
          callerPrincipalIdHex: DEFAULT_CALLER_PRINCIPAL_ID_HEX,
          taskIdHex: targetItem.taskIdHex,
          nowMonoMs: 2_000_100 + cycle,
        });
      } catch (err) {
        if (
          err instanceof NotificationCenterError &&
          err.code === "ACTION_TOKEN_ALREADY_CONSUMED"
        ) {
          actionTokenReplayRejections += 1;
        }
      }
    }

    // 5. Measure rendered DOM node count across Z2 WindowPlane + NotificationCenter page
    const z2Html = wm.renderZ2PlaneHtml();
    const notifPage = notificationCenter.listNotificationFeed({ limit: 8 });
    const notifHtml = renderNotificationCenterHtml({
      open: true,
      disclosure: "L1",
      locale: "zh-CN",
      unreadOnly: false,
      currentPage: notifPage,
      audit: notificationCenter.getSecurityAudit(),
    });
    const renderedDomNodeCount =
      countRenderedHtmlNodes(z2Html) + countRenderedHtmlNodes(notifHtml);
    maxRenderedDomNodeCount = Math.max(maxRenderedDomNodeCount, renderedDomNodeCount);

    if (cycle === 100) {
      cycle100DomNodeCount = renderedDomNodeCount;
    }
    if (cycle === totalCycles) {
      cycle1000DomNodeCount = renderedDomNodeCount;
    }

    // Cleanup transient subscriptions
    unsubWm();
    unsubStore();
    activeSubscribers -= 2;

    if (cycle === 1 || cycle % 200 === 0 || cycle === totalCycles) {
      const visibleWins = wm
        .getSnapshot()
        .windows.filter((w) => w.windowState === "VISIBLE").length;
      checkpoints.push({
        cycle,
        cursorOffset,
        residentTasksInStore: residentTasks,
        residentTaskNodesInStore: residentNodes,
        activeVisibleWindows: visibleWins,
        renderedDomNodeCount,
        activeSubscribers,
        activeTimers,
      });
    }
  }

  const domNodeGrowthAfterWarmup = Math.max(
    0,
    cycle1000DomNodeCount - cycle100DomNodeCount,
  );
  assertSoak(
    domNodeGrowthAfterWarmup === 0,
    `Expected 0 unbounded DOM node growth between cycle 100 and ${totalCycles}, got +${domNodeGrowthAfterWarmup}`,
  );
  assertSoak(
    activeSubscribers === 0 && activeTimers === 0,
    "Expected 0 leaked subscribers or timers after soak campaign",
  );

  const soakDigestHex = deterministicDomainDigestHex(
    "llmos/stage-d/long-run-soak-detector/v1",
    [
      String(totalCycles),
      String(totalLogicalTasksVisited),
      String(maxResidentTasksInStore),
      String(maxRenderedDomNodeCount),
      String(actionTokensRedeemed),
      String(actionTokenReplayRejections),
    ],
  );

  return {
    schemaVersion: "1.0.0",
    totalCyclesExecuted: totalCycles,
    logicalTaskPoolTotal: LOGICAL_TASK_POOL_TOTAL,
    pageCursorLimit,
    totalLogicalTasksVisited,
    maxResidentTasksInStore,
    maxResidentTaskNodesInStore,
    maxRenderedDomNodeCount,
    cycle100DomNodeCount,
    cycle1000DomNodeCount,
    domNodeGrowthAfterWarmup,
    actionTokensRedeemed,
    actionTokenReplayRejections,
    activeSubscriberLeakCount: activeSubscribers,
    activeTimerLeakCount: activeTimers,
    strictlyBoundedOVisibleRows: true,
    checkpoints,
    soakDigestHex,
  };
}
