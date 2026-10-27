// Single Normalized In-Memory Desktop Projection Store (`DesktopProjectionStore`).
// Authority: ADR-0025 Decision 4, Stage D Visual & UX Spec §2A.2, §3.2, §3.5, [DUI-SESSION-001].
// Strictly couples every read/list IPC call to `DisclosureModel` (`O(visible_rows)` cursor budget):
//   - L0: watermark posture only (zero L1/L2/L3 list fanout)
//   - L1: bounded visible page queries (limit <= visibleLimit <= 64; zero L2/L3 queries)
//   - L2: L1 + single-selected-entity TaskNodes & ResourceCost (zero N+1, zero L3 queries)
//   - L3: L2 + parent-scoped diagnostic enumerations for expanded parent IDs (limit <= 64)

import {
  clampEnumLimit,
  clampWatermarkLimit,
  inspectCellLeases,
  inspectHealth,
  inspectResourceCost,
  listCells,
  listExecutionFibers,
  listInstalledApplications,
  listNotifications,
  listOperations,
  listProcesses,
  listResourceReservations,
  listTaskAttempts,
  listTaskGroups,
  listTaskNodes,
  listTasks,
  listTopics,
  listWorkspaces,
  pollDesktopWatermarkEvents,
  type StageDReadReceipt,
} from "../ipc";
import {
  MAX_DESKTOP_WATERMARK_EVENT_LIMIT,
  MAX_SYSTEM_CONTROL_ENUM_LIMIT,
  type CellLeaseDto,
  type CellSummaryDto,
  type DurableOperationSummaryDto,
  type ExecutionFiberSummaryDto,
  type InstalledApplicationSummaryDto,
  type NotificationSummaryDto,
  type OutcomeDto,
  type ProcessSummaryDto,
  type ReceiptDto,
  type ResourceReservationSummaryDto,
  type TaskAttemptSummaryDto,
  type TaskGroupSummaryDto,
  type TaskNodeSummaryDto,
  type TaskSummaryDto,
  type TopicSummaryDto,
  type WorkspaceDto,
} from "../types";
import {
  createDisclosureModel,
  getProjectionQueryTier,
  type DisclosureModel,
  type DisclosureQueryGateContext,
  type DisclosureSnapshot,
  type ProjectionQueryKind,
  type ProjectionQueryTier,
} from "./disclosure-model";

export type WorkstationConnectivityPosture =
  | "local_ready"
  | "offline_local_ready"
  | "cluster_online"
  | "cluster_degraded";

export interface L0AmbientPosture {
  readonly activeTaskCount: number;
  readonly pendingApprovalCount: number;
  readonly unreadNotificationCount: number;
  readonly connectivity: WorkstationConnectivityPosture;
  readonly totalCells: number;
  readonly reachableCells: number;
}

export interface ResourceCostFactSnapshot {
  readonly reservationIdHex: string;
  readonly accountIdHex: string;
  readonly upperBound: number;
  readonly usageHighWater: number;
  readonly consumptionCount: number;
}

export interface ProjectionQueryTelemetry {
  readonly L0: number;
  readonly L1: number;
  readonly L2: number;
  readonly L3: number;
  readonly blockedByDisclosureFence: number;
}

export interface DesktopProjectionSnapshot {
  readonly disclosure: DisclosureSnapshot;
  readonly watermark: number;
  readonly l0Posture: L0AmbientPosture;
  readonly tasks: ReadonlyMap<string, TaskSummaryDto>;
  readonly installedApplications: ReadonlyMap<string, InstalledApplicationSummaryDto>;
  readonly workspaces: ReadonlyMap<string, WorkspaceDto>;
  readonly notifications: ReadonlyMap<string, NotificationSummaryDto>;
  readonly cells: ReadonlyMap<string, CellSummaryDto>;
  readonly taskNodesByPlan: ReadonlyMap<string, readonly TaskNodeSummaryDto[]>;
  readonly taskGroupsByTask: ReadonlyMap<string, readonly TaskGroupSummaryDto[]>;
  readonly taskAttemptsByTask: ReadonlyMap<string, readonly TaskAttemptSummaryDto[]>;
  readonly processes: ReadonlyMap<string, ProcessSummaryDto>;
  readonly executionFibersByProcess: ReadonlyMap<
    string,
    readonly ExecutionFiberSummaryDto[]
  >;
  readonly operationsByFiber: ReadonlyMap<
    string,
    readonly DurableOperationSummaryDto[]
  >;
  readonly resourceCostsByReservation: ReadonlyMap<string, ResourceCostFactSnapshot>;
  readonly resourceReservations: ReadonlyMap<string, ResourceReservationSummaryDto>;
  readonly topics: ReadonlyMap<string, TopicSummaryDto>;
  readonly cellLeasesByCell: ReadonlyMap<string, readonly CellLeaseDto[]>;
  readonly queryTelemetry: ProjectionQueryTelemetry;
}

export interface ProjectionDataSource {
  readonly pollDesktopWatermarkEvents: (
    sinceWatermark?: number,
    limit?: number,
  ) => Promise<StageDReadReceipt<"desktop_watermark_polled">>;
  readonly inspectHealth: () => Promise<ReceiptDto>;
  readonly listTasks: (
    cursorHex?: string | null,
    limit?: number,
  ) => Promise<StageDReadReceipt<"tasks_listed">>;
  readonly listInstalledApplications: (
    cursorHex?: string | null,
    limit?: number,
  ) => Promise<StageDReadReceipt<"installed_applications_listed">>;
  readonly listWorkspaces: (
    cursorHex?: string | null,
    limit?: number,
  ) => Promise<StageDReadReceipt<"workspaces_listed">>;
  readonly listNotifications: (
    cursorHex?: string | null,
    limit?: number,
  ) => Promise<StageDReadReceipt<"notifications_listed">>;
  readonly listCells: (
    cursorHex?: string | null,
    limit?: number,
  ) => Promise<StageDReadReceipt<"cells_listed">>;
  readonly listTaskNodes: (
    planIdHex: string,
    expectedPlanRevision?: number,
    cursorHex?: string | null,
    limit?: number,
  ) => Promise<StageDReadReceipt<"task_nodes_listed">>;
  readonly inspectResourceCost: (reservationIdHex: string) => Promise<ReceiptDto>;
  readonly listTaskGroups: (
    taskIdHex?: string | null,
    cursorHex?: string | null,
    limit?: number,
  ) => Promise<StageDReadReceipt<"task_groups_listed">>;
  readonly listTaskAttempts: (
    scopeFilterIdHex?: string | null,
    expectedGeneration?: number,
    cursorHex?: string | null,
    limit?: number,
  ) => Promise<StageDReadReceipt<"task_attempts_listed">>;
  readonly listProcesses: (
    cursorHex?: string | null,
    limit?: number,
  ) => Promise<StageDReadReceipt<"processes_listed">>;
  readonly listExecutionFibers: (
    processIdHex?: string | null,
    expectedGeneration?: number,
    cursorHex?: string | null,
    limit?: number,
  ) => Promise<StageDReadReceipt<"execution_fibers_listed">>;
  readonly listOperations: (
    fiberIdHex?: string | null,
    expectedGeneration?: number,
    cursorHex?: string | null,
    limit?: number,
  ) => Promise<StageDReadReceipt<"operations_listed">>;
  readonly listResourceReservations: (
    cursorHex?: string | null,
    limit?: number,
  ) => Promise<StageDReadReceipt<"resource_reservations_listed">>;
  readonly listTopics: (
    cursorHex?: string | null,
    limit?: number,
  ) => Promise<StageDReadReceipt<"topics_listed">>;
  readonly inspectCellLeases: (
    cellIdHex?: string | null,
    limit?: number,
  ) => Promise<StageDReadReceipt<"cell_leases_inspected">>;
}

type RawEnumLimitArg = typeof MAX_SYSTEM_CONTROL_ENUM_LIMIT | undefined;
type RawWatermarkLimitArg = typeof MAX_DESKTOP_WATERMARK_EVENT_LIMIT | undefined;

const DEFAULT_IPC_DATA_SOURCE: ProjectionDataSource = {
  pollDesktopWatermarkEvents: (sinceWatermark, limit) =>
    pollDesktopWatermarkEvents(sinceWatermark, limit as RawWatermarkLimitArg),
  inspectHealth,
  listTasks: (cursorHex, limit) =>
    listTasks(cursorHex, limit as RawEnumLimitArg),
  listInstalledApplications: (cursorHex, limit) =>
    listInstalledApplications(cursorHex, limit as RawEnumLimitArg),
  listWorkspaces: (cursorHex, limit) =>
    listWorkspaces(cursorHex, limit as RawEnumLimitArg),
  listNotifications: (cursorHex, limit) =>
    listNotifications(cursorHex, limit as RawEnumLimitArg),
  listCells: (cursorHex, limit) =>
    listCells(cursorHex, limit as RawEnumLimitArg),
  listTaskNodes: (planIdHex, expectedPlanRevision, cursorHex, limit) =>
    listTaskNodes(
      planIdHex,
      expectedPlanRevision,
      cursorHex,
      limit as RawEnumLimitArg,
    ),
  inspectResourceCost,
  listTaskGroups: (taskIdHex, cursorHex, limit) =>
    listTaskGroups(taskIdHex, cursorHex, limit as RawEnumLimitArg),
  listTaskAttempts: (scopeFilterIdHex, expectedGeneration, cursorHex, limit) =>
    listTaskAttempts(
      scopeFilterIdHex,
      expectedGeneration,
      cursorHex,
      limit as RawEnumLimitArg,
    ),
  listProcesses: (cursorHex, limit) =>
    listProcesses(cursorHex, limit as RawEnumLimitArg),
  listExecutionFibers: (processIdHex, expectedGeneration, cursorHex, limit) =>
    listExecutionFibers(
      processIdHex,
      expectedGeneration,
      cursorHex,
      limit as RawEnumLimitArg,
    ),
  listOperations: (fiberIdHex, expectedGeneration, cursorHex, limit) =>
    listOperations(
      fiberIdHex,
      expectedGeneration,
      cursorHex,
      limit as RawEnumLimitArg,
    ),
  listResourceReservations: (cursorHex, limit) =>
    listResourceReservations(cursorHex, limit as RawEnumLimitArg),
  listTopics: (cursorHex, limit) =>
    listTopics(cursorHex, limit as RawEnumLimitArg),
  inspectCellLeases: (cellIdHex, limit) =>
    inspectCellLeases(cellIdHex, limit as RawEnumLimitArg),
};

export interface TieredFetchRequestOptions<T> extends DisclosureQueryGateContext {
  readonly queryKind: ProjectionQueryKind;
  readonly execute: () => Promise<T>;
}

export type TieredFetchOutcome<T> =
  | {
      readonly allowed: true;
      readonly executed: true;
      readonly blockedByDisclosureFence: false;
      readonly tier: ProjectionQueryTier;
      readonly value: T;
    }
  | {
      readonly allowed: false;
      readonly executed: false;
      readonly blockedByDisclosureFence: true;
      readonly tier: ProjectionQueryTier;
      readonly value: null;
    };

export interface DesktopProjectionStoreOptions {
  readonly disclosureModel?: DisclosureModel;
  readonly dataSource?: Partial<ProjectionDataSource>;
}

export type ProjectionStoreSubscriber = (
  snapshot: DesktopProjectionSnapshot,
) => void;

const TERMINAL_TASK_STATES: ReadonlySet<string> = new Set([
  "COMPLETED",
  "FAILED",
  "CANCELLED",
]);

const APPROVAL_NEEDED_STATES: ReadonlySet<string> = new Set([
  "WAITING_CONSENT",
  "WAITING_INPUT",
  "DISPUTED",
]);

function isCellReachable(cell: CellSummaryDto): boolean {
  const norm = cell.reachability.trim().toUpperCase();
  return (
    norm === "ONLINE" ||
    norm === "REACHABLE" ||
    norm === "LOCAL" ||
    norm === "LOCAL_READY" ||
    norm === "ACTIVE" ||
    norm === "HEALTHY"
  );
}

function createInitialL0Posture(): L0AmbientPosture {
  return {
    activeTaskCount: 0,
    pendingApprovalCount: 0,
    unreadNotificationCount: 0,
    connectivity: "local_ready",
    totalCells: 0,
    reachableCells: 0,
  };
}

export class DesktopProjectionStore {
  public readonly disclosureModel: DisclosureModel;
  private readonly dataSource: ProjectionDataSource;

  private watermark = 0;
  private l0Posture: L0AmbientPosture = createInitialL0Posture();
  private offlineFallbackActive = false;

  private readonly tasksMap = new Map<string, TaskSummaryDto>();
  private readonly installedApplicationsMap = new Map<
    string,
    InstalledApplicationSummaryDto
  >();
  private readonly workspacesMap = new Map<string, WorkspaceDto>();
  private readonly notificationsMap = new Map<string, NotificationSummaryDto>();
  private readonly cellsMap = new Map<string, CellSummaryDto>();
  private readonly taskNodesByPlanMap = new Map<
    string,
    readonly TaskNodeSummaryDto[]
  >();
  private readonly taskGroupsByTaskMap = new Map<
    string,
    readonly TaskGroupSummaryDto[]
  >();
  private readonly taskAttemptsByTaskMap = new Map<
    string,
    readonly TaskAttemptSummaryDto[]
  >();
  private readonly processesMap = new Map<string, ProcessSummaryDto>();
  private readonly executionFibersByProcessMap = new Map<
    string,
    readonly ExecutionFiberSummaryDto[]
  >();
  private readonly knownFibersById = new Map<string, ExecutionFiberSummaryDto>();
  private readonly operationsByFiberMap = new Map<
    string,
    readonly DurableOperationSummaryDto[]
  >();
  private readonly resourceCostsByReservationMap = new Map<
    string,
    ResourceCostFactSnapshot
  >();
  private readonly resourceReservationsMap = new Map<
    string,
    ResourceReservationSummaryDto
  >();
  private readonly topicsMap = new Map<string, TopicSummaryDto>();
  private readonly cellLeasesByCellMap = new Map<
    string,
    readonly CellLeaseDto[]
  >();

  private telemetryCounts: {
    L0: number;
    L1: number;
    L2: number;
    L3: number;
    blockedByDisclosureFence: number;
  } = {
    L0: 0,
    L1: 0,
    L2: 0,
    L3: 0,
    blockedByDisclosureFence: 0,
  };

  private readonly subscribers = new Set<ProjectionStoreSubscriber>();

  public constructor(options: DesktopProjectionStoreOptions = {}) {
    this.disclosureModel = options.disclosureModel ?? createDisclosureModel();
    this.dataSource = {
      ...DEFAULT_IPC_DATA_SOURCE,
      ...(options.dataSource ?? {}),
    };
  }

  private emit(): DesktopProjectionSnapshot {
    const snap = this.getSnapshot();
    for (const cb of this.subscribers) {
      cb(snap);
    }
    return snap;
  }

  private recomputeL0PostureFromNormalized(): void {
    let activeTaskCount = 0;
    let pendingApprovalCount = 0;
    for (const task of this.tasksMap.values()) {
      const st = task.taskState.trim().toUpperCase();
      if (!TERMINAL_TASK_STATES.has(st)) {
        activeTaskCount += 1;
      }
      if (APPROVAL_NEEDED_STATES.has(st)) {
        pendingApprovalCount += 1;
      }
    }

    let unreadNotificationCount = 0;
    for (const notif of this.notificationsMap.values()) {
      const pendingEntries = Math.max(
        0,
        notif.nextSequence - notif.pruneWatermarkSequence,
      );
      if (pendingEntries > 0 || notif.historyEntryCount > 0) {
        unreadNotificationCount += 1;
      }
    }

    const totalCells = this.cellsMap.size;
    let reachableCells = 0;
    for (const cell of this.cellsMap.values()) {
      if (isCellReachable(cell)) {
        reachableCells += 1;
      }
    }

    let connectivity: WorkstationConnectivityPosture = "local_ready";
    if (this.offlineFallbackActive) {
      connectivity = "offline_local_ready";
    } else if (totalCells > 1) {
      if (reachableCells === totalCells) {
        connectivity = "cluster_online";
      } else if (reachableCells === 0) {
        connectivity = "offline_local_ready";
      } else {
        connectivity = "cluster_degraded";
      }
    } else if (totalCells === 1 && reachableCells === 0) {
      connectivity = "offline_local_ready";
    }

    this.l0Posture = {
      activeTaskCount,
      pendingApprovalCount,
      unreadNotificationCount,
      connectivity,
      totalCells,
      reachableCells,
    };
  }

  public subscribe(listener: ProjectionStoreSubscriber): () => void {
    this.subscribers.add(listener);
    return () => {
      this.subscribers.delete(listener);
    };
  }

  public getWatermark(): number {
    return this.watermark;
  }

  public getL0Posture(): L0AmbientPosture {
    return { ...this.l0Posture };
  }

  public getQueryTelemetry(): ProjectionQueryTelemetry {
    return { ...this.telemetryCounts };
  }

  public resetQueryTelemetry(): void {
    this.telemetryCounts = {
      L0: 0,
      L1: 0,
      L2: 0,
      L3: 0,
      blockedByDisclosureFence: 0,
    };
  }

  public getSnapshot(): DesktopProjectionSnapshot {
    return {
      disclosure: this.disclosureModel.getSnapshot(),
      watermark: this.watermark,
      l0Posture: { ...this.l0Posture },
      tasks: new Map(this.tasksMap),
      installedApplications: new Map(this.installedApplicationsMap),
      workspaces: new Map(this.workspacesMap),
      notifications: new Map(this.notificationsMap),
      cells: new Map(this.cellsMap),
      taskNodesByPlan: new Map(this.taskNodesByPlanMap),
      taskGroupsByTask: new Map(this.taskGroupsByTaskMap),
      taskAttemptsByTask: new Map(this.taskAttemptsByTaskMap),
      processes: new Map(this.processesMap),
      executionFibersByProcess: new Map(this.executionFibersByProcessMap),
      operationsByFiber: new Map(this.operationsByFiberMap),
      resourceCostsByReservation: new Map(this.resourceCostsByReservationMap),
      resourceReservations: new Map(this.resourceReservationsMap),
      topics: new Map(this.topicsMap),
      cellLeasesByCell: new Map(this.cellLeasesByCellMap),
      queryTelemetry: { ...this.telemetryCounts },
    };
  }

  /**
   * Executes an IPC query strictly through the `DisclosureModel` tier fence.
   * When the current disclosure level forbids `queryKind`, increments
   * `queryTelemetry.blockedByDisclosureFence` and skips network/IPC execution.
   */
  public async requestTieredFetch<T>(
    options: TieredFetchRequestOptions<T>,
  ): Promise<TieredFetchOutcome<T>>;
  public async requestTieredFetch<T>(
    queryKind: ProjectionQueryKind,
    execute: () => Promise<T>,
    context?: DisclosureQueryGateContext,
  ): Promise<TieredFetchOutcome<T>>;
  public async requestTieredFetch<T>(
    first: ProjectionQueryKind | TieredFetchRequestOptions<T>,
    second?: () => Promise<T>,
    third?: DisclosureQueryGateContext,
  ): Promise<TieredFetchOutcome<T>> {
    const queryKind = typeof first === "string" ? first : first.queryKind;
    const execute = typeof first === "string" ? second! : first.execute;
    const context: DisclosureQueryGateContext | undefined =
      typeof first === "string"
        ? third
        : {
            targetEntityId: first.targetEntityId,
            selectedEntityId: first.selectedEntityId,
            expandedParentIds: first.expandedParentIds,
          };

    const tier = getProjectionQueryTier(queryKind);
    const allowed = this.disclosureModel.isQueryAllowedAtDisclosure(
      queryKind,
      context,
    );

    if (!allowed) {
      this.telemetryCounts.blockedByDisclosureFence += 1;
      return {
        allowed: false,
        executed: false,
        blockedByDisclosureFence: true,
        tier,
        value: null,
      };
    }

    this.telemetryCounts[tier] += 1;
    const value = await execute();
    return {
      allowed: true,
      executed: true,
      blockedByDisclosureFence: false,
      tier,
      value,
    };
  }

  /**
   * Refreshes `L0` ambient watermark posture (`limit <= 128`), incrementing `queryTelemetry.L0`.
   * Preserves `[DUI-OFFLINE-001]` local-first resilience on transient transport disconnection.
   */
  public async refreshL0Posture(
    limit: number = MAX_DESKTOP_WATERMARK_EVENT_LIMIT,
  ): Promise<DesktopProjectionSnapshot> {
    const boundedLimit = clampWatermarkLimit(limit as RawWatermarkLimitArg);
    try {
      const res = await this.requestTieredFetch("poll_watermark", () =>
        this.dataSource.pollDesktopWatermarkEvents(this.watermark, boundedLimit),
      );
      if (res.executed && res.value.outcome.kind === "desktop_watermark_polled") {
        this.offlineFallbackActive = false;
        this.watermark = Math.max(
          this.watermark,
          res.value.outcome.currentWatermark,
        );
        if (this.tasksMap.size === 0 && res.value.outcome.events.length > 0) {
          const uniqueTaskEvents = new Set<string>();
          const uniqueNotifEvents = new Set<string>();
          for (const ev of res.value.outcome.events) {
            const scope = ev.domainScope.trim().toLowerCase();
            if (scope === "task") {
              uniqueTaskEvents.add(ev.entityIdHex);
            } else if (scope === "notification") {
              uniqueNotifEvents.add(ev.entityIdHex);
            }
          }
          this.l0Posture = {
            ...this.l0Posture,
            activeTaskCount: Math.max(
              this.l0Posture.activeTaskCount,
              uniqueTaskEvents.size,
            ),
            unreadNotificationCount: Math.max(
              this.l0Posture.unreadNotificationCount,
              uniqueNotifEvents.size,
            ),
            connectivity: "local_ready",
          };
        } else {
          this.recomputeL0PostureFromNormalized();
        }
      }
    } catch {
      this.offlineFallbackActive = true;
      this.l0Posture = {
        ...this.l0Posture,
        connectivity: "offline_local_ready",
      };
    }
    return this.emit();
  }

  /**
   * Refreshes normalized projections strictly bounded by `disclosureModel`:
   * - `L0`: ONLY `L0` watermark posture (`L1 === 0 && L2 === 0 && L3 === 0`)
   * - `L1`: `L0` + bounded `L1` page lists (`limit <= visibleLimit <= 64`, `0` `L2`, `0` `L3`)
   * - `L2`: `L1` + single-selected-entity `listTaskNodes` & `inspectResourceCost` (`0` `L3`)
   * - `L3`: `L2` + `L3` enumerations strictly for `expandedL3ParentIds` (`limit <= 64`)
   */
  public async refreshForCurrentDisclosure(): Promise<DesktopProjectionSnapshot> {
    await this.refreshL0Posture();

    const disclosureSnap = this.disclosureModel.getSnapshot();
    const level = disclosureSnap.level;
    if (level === "L0") {
      return this.getSnapshot();
    }

    const visibleLimit = clampEnumLimit(
      disclosureSnap.visibleRowWindow.limit as RawEnumLimitArg,
    );
    const cursorHex = disclosureSnap.visibleRowWindow.cursorHex;

    // L1 bounded page queries (strictly <= visibleLimit <= 64)
    const [tasksRes, appsRes, workspacesRes, notificationsRes, cellsRes] =
      await Promise.all([
        this.requestTieredFetch("list_tasks", () =>
          this.dataSource.listTasks(cursorHex, visibleLimit),
        ),
        this.requestTieredFetch("list_installed_applications", () =>
          this.dataSource.listInstalledApplications(cursorHex, visibleLimit),
        ),
        this.requestTieredFetch("list_workspaces", () =>
          this.dataSource.listWorkspaces(cursorHex, visibleLimit),
        ),
        this.requestTieredFetch("list_notifications", () =>
          this.dataSource.listNotifications(cursorHex, visibleLimit),
        ),
        this.requestTieredFetch("list_cells", () =>
          this.dataSource.listCells(cursorHex, visibleLimit),
        ),
      ]);

    if (tasksRes.executed && tasksRes.value.outcome.kind === "tasks_listed") {
      this.tasksMap.clear();
      for (const task of tasksRes.value.outcome.tasks) {
        this.tasksMap.set(task.taskIdHex, task);
      }
    }

    if (
      appsRes.executed &&
      appsRes.value.outcome.kind === "installed_applications_listed"
    ) {
      this.installedApplicationsMap.clear();
      for (const app of appsRes.value.outcome.applications) {
        this.installedApplicationsMap.set(app.applicationIdHex, app);
      }
    }

    if (
      workspacesRes.executed &&
      workspacesRes.value.outcome.kind === "workspaces_listed"
    ) {
      this.workspacesMap.clear();
      for (const ws of workspacesRes.value.outcome.workspaces) {
        this.workspacesMap.set(ws.workspaceIdHex, ws);
      }
    }

    if (
      notificationsRes.executed &&
      notificationsRes.value.outcome.kind === "notifications_listed"
    ) {
      this.notificationsMap.clear();
      for (const notif of notificationsRes.value.outcome.notifications) {
        this.notificationsMap.set(notif.notificationIdHex, notif);
      }
    }

    if (cellsRes.executed && cellsRes.value.outcome.kind === "cells_listed") {
      this.cellsMap.clear();
      for (const cell of cellsRes.value.outcome.cells) {
        this.cellsMap.set(cell.cellIdHex, cell);
      }
    }

    this.recomputeL0PostureFromNormalized();

    if (level === "L1") {
      return this.emit();
    }

    // L2 single-selected-object queries (zero N+1 fanout across unselected tasks)
    const selectedId = disclosureSnap.selectedEntityId;
    if (selectedId !== null && selectedId.trim().length > 0) {
      const selectedTask = this.tasksMap.get(selectedId);
      const planIdHex = selectedTask?.planIdHex ?? selectedId;
      const planRevision = selectedTask?.planRevision ?? 0;
      const l2GateCtx: DisclosureQueryGateContext = {
        targetEntityId: selectedId,
        selectedEntityId: selectedId,
      };

      const nodesPromise = this.requestTieredFetch(
        "list_task_nodes",
        () =>
          this.dataSource.listTaskNodes(
            planIdHex,
            planRevision,
            null,
            visibleLimit,
          ),
        l2GateCtx,
      );

      const reservationIdHex =
        selectedTask === undefined ? selectedId : selectedTask.reservationIdHex;

      const costPromise =
        reservationIdHex !== null
          ? this.requestTieredFetch(
              "inspect_resource_cost",
              () => this.dataSource.inspectResourceCost(reservationIdHex),
              l2GateCtx,
            )
          : Promise.resolve(null);

      const [nodesRes, costRes] = await Promise.all([nodesPromise, costPromise]);

      if (
        nodesRes.executed &&
        nodesRes.value.outcome.kind === "task_nodes_listed"
      ) {
        this.taskNodesByPlanMap.set(
          nodesRes.value.outcome.planIdHex,
          nodesRes.value.outcome.nodes,
        );
      }

      if (
        costRes !== null &&
        costRes.executed &&
        costRes.value.outcome.kind === "resource_inspected"
      ) {
        const out: Extract<OutcomeDto, { kind: "resource_inspected" }> =
          costRes.value.outcome;
        this.resourceCostsByReservationMap.set(out.reservationIdHex, {
          reservationIdHex: out.reservationIdHex,
          accountIdHex: out.accountIdHex,
          upperBound: out.upperBound,
          usageHighWater: out.usageHighWater,
          consumptionCount: out.consumptionCount,
        });
      }
    }

    if (level === "L2") {
      return this.emit();
    }

    // L3 diagnostic queries strictly scoped to expandedL3ParentIds (limit <= visibleLimit <= 64)
    const expandedParents = disclosureSnap.expandedL3ParentIds;
    for (const parentId of expandedParents) {
      const l3Ctx: DisclosureQueryGateContext = {
        targetEntityId: parentId,
        selectedEntityId: disclosureSnap.selectedEntityId,
        expandedParentIds: expandedParents,
      };

      let matchedDomain = false;

      const task = this.tasksMap.get(parentId);
      if (task !== undefined) {
        matchedDomain = true;
        const [groupsRes, attemptsRes] = await Promise.all([
          this.requestTieredFetch(
            "list_task_groups",
            () => this.dataSource.listTaskGroups(parentId, null, visibleLimit),
            l3Ctx,
          ),
          this.requestTieredFetch(
            "list_task_attempts",
            () =>
              this.dataSource.listTaskAttempts(
                parentId,
                task.expectedGenerationOrRevision,
                null,
                visibleLimit,
              ),
            l3Ctx,
          ),
        ]);
        if (
          groupsRes.executed &&
          groupsRes.value.outcome.kind === "task_groups_listed"
        ) {
          this.taskGroupsByTaskMap.set(parentId, groupsRes.value.outcome.groups);
        }
        if (
          attemptsRes.executed &&
          attemptsRes.value.outcome.kind === "task_attempts_listed"
        ) {
          this.taskAttemptsByTaskMap.set(
            parentId,
            attemptsRes.value.outcome.attempts,
          );
        }
      }

      const proc = this.processesMap.get(parentId);
      if (proc !== undefined) {
        matchedDomain = true;
        const fibersRes = await this.requestTieredFetch(
          "list_execution_fibers",
          () =>
            this.dataSource.listExecutionFibers(
              parentId,
              proc.expectedGenerationOrRevision,
              null,
              visibleLimit,
            ),
          l3Ctx,
        );
        if (
          fibersRes.executed &&
          fibersRes.value.outcome.kind === "execution_fibers_listed"
        ) {
          this.executionFibersByProcessMap.set(
            parentId,
            fibersRes.value.outcome.fibers,
          );
          for (const fiber of fibersRes.value.outcome.fibers) {
            this.knownFibersById.set(fiber.fiberIdHex, fiber);
          }
        }
      }

      const fiber = this.knownFibersById.get(parentId);
      if (fiber !== undefined) {
        matchedDomain = true;
        const opsRes = await this.requestTieredFetch(
          "list_operations",
          () =>
            this.dataSource.listOperations(
              parentId,
              fiber.generation,
              null,
              visibleLimit,
            ),
          l3Ctx,
        );
        if (
          opsRes.executed &&
          opsRes.value.outcome.kind === "operations_listed"
        ) {
          this.operationsByFiberMap.set(
            parentId,
            opsRes.value.outcome.operations,
          );
        }
      }

      const cell = this.cellsMap.get(parentId);
      if (cell !== undefined) {
        matchedDomain = true;
        const leasesRes = await this.requestTieredFetch(
          "inspect_cell_leases",
          () => this.dataSource.inspectCellLeases(parentId, visibleLimit),
          l3Ctx,
        );
        if (
          leasesRes.executed &&
          leasesRes.value.outcome.kind === "cell_leases_inspected"
        ) {
          this.cellLeasesByCellMap.set(parentId, leasesRes.value.outcome.leases);
        }
      }

      if (parentId === "domain:processes" || parentId === "processes") {
        matchedDomain = true;
        const procsRes = await this.requestTieredFetch(
          "list_processes",
          () => this.dataSource.listProcesses(null, visibleLimit),
          l3Ctx,
        );
        if (
          procsRes.executed &&
          procsRes.value.outcome.kind === "processes_listed"
        ) {
          this.processesMap.clear();
          for (const p of procsRes.value.outcome.processes) {
            this.processesMap.set(p.processIdHex, p);
          }
        }
      } else if (parentId === "domain:resources" || parentId === "resources") {
        matchedDomain = true;
        const resvRes = await this.requestTieredFetch(
          "list_resource_reservations",
          () => this.dataSource.listResourceReservations(null, visibleLimit),
          l3Ctx,
        );
        if (
          resvRes.executed &&
          resvRes.value.outcome.kind === "resource_reservations_listed"
        ) {
          this.resourceReservationsMap.clear();
          for (const r of resvRes.value.outcome.reservations) {
            this.resourceReservationsMap.set(r.reservationIdHex, r);
          }
        }
      } else if (parentId === "domain:topics" || parentId === "topics") {
        matchedDomain = true;
        const topicsRes = await this.requestTieredFetch(
          "list_topics",
          () => this.dataSource.listTopics(null, visibleLimit),
          l3Ctx,
        );
        if (
          topicsRes.executed &&
          topicsRes.value.outcome.kind === "topics_listed"
        ) {
          this.topicsMap.clear();
          for (const t of topicsRes.value.outcome.topics) {
            this.topicsMap.set(t.topicIdHex, t);
          }
        }
      }

      if (!matchedDomain) {
        const attemptsRes = await this.requestTieredFetch(
          "list_task_attempts",
          () =>
            this.dataSource.listTaskAttempts(parentId, 0, null, visibleLimit),
          l3Ctx,
        );
        if (
          attemptsRes.executed &&
          attemptsRes.value.outcome.kind === "task_attempts_listed"
        ) {
          this.taskAttemptsByTaskMap.set(
            parentId,
            attemptsRes.value.outcome.attempts,
          );
        }
      }
    }

    return this.emit();
  }

  /**
   * `[DUI-SESSION-001]` Session lock / switch-user sensitive cache purge:
   * Wipes all user/workspace/task/application/notification/diagnostic entities in memory,
   * resets watermark to `0`, collapses `L2/L3` scopes, and drops disclosure to `L0`.
   */
  public clearSensitiveCacheOnSessionLock(): DesktopProjectionSnapshot {
    this.watermark = 0;
    this.offlineFallbackActive = false;
    this.l0Posture = createInitialL0Posture();

    this.tasksMap.clear();
    this.installedApplicationsMap.clear();
    this.workspacesMap.clear();
    this.notificationsMap.clear();
    this.cellsMap.clear();
    this.taskNodesByPlanMap.clear();
    this.taskGroupsByTaskMap.clear();
    this.taskAttemptsByTaskMap.clear();
    this.processesMap.clear();
    this.executionFibersByProcessMap.clear();
    this.knownFibersById.clear();
    this.operationsByFiberMap.clear();
    this.resourceCostsByReservationMap.clear();
    this.resourceReservationsMap.clear();
    this.topicsMap.clear();
    this.cellLeasesByCellMap.clear();

    this.disclosureModel.clearExpandedL3Parents();
    this.disclosureModel.selectEntity(null, null);
    this.disclosureModel.setLevel("L0");

    return this.emit();
  }
}

export function createDesktopProjectionStore(
  options?: DesktopProjectionStoreOptions,
): DesktopProjectionStore {
  return new DesktopProjectionStore(options);
}
