// L0 -> L1 -> L2 -> L3 Progressive Disclosure State Machine & O(visible_rows) Query Fence.
// Authority: ADR-0025 Decision 4 (W68-R2), Stage D Visual & UX Spec §2A.2, §2.4.
// Replaces binary Standard <-> Diagnostic view-mode cliff with zero-context-loss (< 16ms)
// in-place disclosure transitions and strict per-tier IPC fanout bounds.

import { MAX_SYSTEM_CONTROL_ENUM_LIMIT } from "../types";
import type { DisclosureLevel } from "../ui/status-indicator";

export type { DisclosureLevel };

export const ALL_DISCLOSURE_LEVELS: readonly DisclosureLevel[] = [
  "L0",
  "L1",
  "L2",
  "L3",
];

export const DEFAULT_DISCLOSURE_ROW_HEIGHT_PX = 30 as const;
export const DEFAULT_VISIBLE_VIEWPORT_HEIGHT_PX = 600 as const;

export type ProjectionQueryTier = "L0" | "L1" | "L2" | "L3";

export type ProjectionQueryKind =
  | "poll_watermark"
  | "health_posture"
  | "list_tasks"
  | "list_installed_applications"
  | "list_workspaces"
  | "list_notifications"
  | "list_cells"
  | "list_task_nodes"
  | "inspect_resource_cost"
  | "list_task_groups"
  | "list_task_attempts"
  | "list_processes"
  | "list_execution_fibers"
  | "list_operations"
  | "list_resource_reservations"
  | "list_topics"
  | "inspect_cell_leases";

const QUERY_TIER_BY_KIND: Readonly<Record<ProjectionQueryKind, ProjectionQueryTier>> = {
  poll_watermark: "L0",
  health_posture: "L0",
  list_tasks: "L1",
  list_installed_applications: "L1",
  list_workspaces: "L1",
  list_notifications: "L1",
  list_cells: "L1",
  list_task_nodes: "L2",
  inspect_resource_cost: "L2",
  list_task_groups: "L3",
  list_task_attempts: "L3",
  list_processes: "L3",
  list_execution_fibers: "L3",
  list_operations: "L3",
  list_resource_reservations: "L3",
  list_topics: "L3",
  inspect_cell_leases: "L3",
};

export const ALL_PROJECTION_QUERY_KINDS: readonly ProjectionQueryKind[] = Object.keys(
  QUERY_TIER_BY_KIND,
) as ProjectionQueryKind[];

export function getProjectionQueryTier(queryKind: ProjectionQueryKind): ProjectionQueryTier {
  return QUERY_TIER_BY_KIND[queryKind];
}

export interface DisclosureQueryGateContext {
  readonly targetEntityId?: string | null;
  readonly selectedEntityId?: string | null;
  readonly expandedParentIds?: ReadonlySet<string>;
}

/**
 * Computes the bounded enumeration cursor page width strictly clamped to `1..=64`
 * (`MAX_SYSTEM_CONTROL_ENUM_LIMIT`) for `O(visible_rows)` viewport lazy-fetching.
 */
export function computeVisibleCursorLimit(
  visibleViewportHeightPx: number,
  rowHeightPx: number = DEFAULT_DISCLOSURE_ROW_HEIGHT_PX,
): number {
  const effectiveRowHeight =
    Number.isFinite(rowHeightPx) && rowHeightPx > 0
      ? rowHeightPx
      : DEFAULT_DISCLOSURE_ROW_HEIGHT_PX;
  if (!Number.isFinite(visibleViewportHeightPx) || visibleViewportHeightPx <= 0) {
    return 1;
  }
  const rawCapacity = Math.ceil(visibleViewportHeightPx / effectiveRowHeight);
  return Math.max(1, Math.min(MAX_SYSTEM_CONTROL_ENUM_LIMIT, rawCapacity));
}

function isNonEmptyEntityId(value: string | null | undefined): value is string {
  return typeof value === "string" && value.trim().length > 0;
}

/**
 * Pure `O(visible_rows)` progressive disclosure IPC fence:
 * - `L0`: ONLY `"poll_watermark"` and `"health_posture"` allowed; zero list fanout.
 * - `L1`: `L0` + bounded `L1` page lists; strictly `false` for all `L2` and `L3` queries.
 * - `L2`: `L1` + single-selected-object `L2` queries (`targetEntityId === selectedEntityId`);
 *   strictly `false` for all `L3` queries.
 * - `L3`: `L2` + `L3` diagnostic queries scoped to `expandedParentIds` when provided.
 */
export function isQueryAllowedAtDisclosure(
  level: DisclosureLevel,
  queryKind: ProjectionQueryKind,
  context?: DisclosureQueryGateContext,
): boolean {
  const tier = QUERY_TIER_BY_KIND[queryKind];

  if (tier === "L0") {
    return true;
  }

  if (level === "L0") {
    return false;
  }

  if (tier === "L1") {
    return true;
  }

  if (level === "L1") {
    return false;
  }

  if (tier === "L2") {
    const selected = context?.selectedEntityId;
    const target = context?.targetEntityId;
    if (isNonEmptyEntityId(selected) && isNonEmptyEntityId(target) && target === selected) {
      return true;
    }
    if (
      level === "L3" &&
      isNonEmptyEntityId(target) &&
      context?.expandedParentIds !== undefined &&
      context.expandedParentIds.has(target)
    ) {
      return true;
    }
    return false;
  }

  // tier === "L3"
  if (level !== "L3") {
    return false;
  }

  const expandedParents = context?.expandedParentIds;
  if (expandedParents === undefined) {
    return true;
  }

  const target = context?.targetEntityId;
  if (isNonEmptyEntityId(target)) {
    return expandedParents.has(target);
  }

  return expandedParents.size > 0;
}

export interface DisclosureRootElementLike {
  setAttribute(name: string, value: string): void;
  getAttribute?(name: string): string | null;
}

export interface VisibleRowWindow {
  readonly cursorHex: string | null;
  readonly limit: number;
  readonly viewportHeightPx: number;
  readonly rowHeightPx: number;
}

export type NonDiagnosticDisclosureLevel = Exclude<DisclosureLevel, "L3">;

export interface DisclosureSnapshot {
  readonly level: DisclosureLevel;
  readonly preDiagnosticLevel: NonDiagnosticDisclosureLevel;
  readonly selectedEntityId: string | null;
  readonly selectedDomain: string | null;
  readonly visibleRowWindow: VisibleRowWindow;
  readonly expandedL3ParentIds: ReadonlySet<string>;
  readonly lastTransitionDurationMs: number;
}

export interface DisclosureKeydownEvent {
  readonly key: string;
  readonly ctrlKey?: boolean;
  readonly metaKey?: boolean;
  readonly shiftKey?: boolean;
  readonly altKey?: boolean;
  readonly targetIsTextInput?: boolean;
  readonly focusedRowEntityId?: string | null;
}

export interface DisclosureModelOptions {
  readonly initialLevel?: DisclosureLevel;
  readonly initialPreDiagnosticLevel?: NonDiagnosticDisclosureLevel;
  readonly rootElement?: DisclosureRootElementLike | null;
  readonly selectedEntityId?: string | null;
  readonly selectedDomain?: string | null;
  readonly viewportHeightPx?: number;
  readonly rowHeightPx?: number;
  readonly cursorHex?: string | null;
  readonly expandedL3ParentIds?: Iterable<string>;
  readonly nowMs?: () => number;
}

export type DisclosureSubscriber = (snapshot: DisclosureSnapshot) => void;

function defaultNowMs(): number {
  if (typeof performance !== "undefined" && typeof performance.now === "function") {
    return performance.now();
  }
  return Date.now();
}

function isDisclosureLevelValue(candidate: string): candidate is DisclosureLevel {
  return (
    candidate === "L0" ||
    candidate === "L1" ||
    candidate === "L2" ||
    candidate === "L3"
  );
}

export class DisclosureModel {
  private level: DisclosureLevel;
  private preDiagnosticLevel: NonDiagnosticDisclosureLevel;
  private rootElement: DisclosureRootElementLike | null;
  private selectedEntityId: string | null;
  private selectedDomain: string | null;
  private visibleRowWindow: VisibleRowWindow;
  private expandedL3ParentIds: Set<string>;
  private lastTransitionDurationMs = 0;
  private readonly nowMs: () => number;
  private readonly subscribers = new Set<DisclosureSubscriber>();

  public constructor(options: DisclosureModelOptions = {}) {
    const initial = options.initialLevel ?? "L1";
    this.level = initial;
    this.preDiagnosticLevel =
      options.initialPreDiagnosticLevel ?? (initial === "L3" ? "L1" : initial);
    this.rootElement = options.rootElement ?? null;
    this.selectedEntityId = options.selectedEntityId ?? null;
    this.selectedDomain = options.selectedDomain ?? null;
    const viewportHeightPx =
      options.viewportHeightPx ?? DEFAULT_VISIBLE_VIEWPORT_HEIGHT_PX;
    const rowHeightPx = options.rowHeightPx ?? DEFAULT_DISCLOSURE_ROW_HEIGHT_PX;
    this.visibleRowWindow = {
      cursorHex: options.cursorHex ?? null,
      limit: computeVisibleCursorLimit(viewportHeightPx, rowHeightPx),
      viewportHeightPx,
      rowHeightPx,
    };
    this.expandedL3ParentIds = new Set(options.expandedL3ParentIds ?? []);
    this.nowMs = options.nowMs ?? defaultNowMs;
    this.syncRootAttribute();
  }

  private syncRootAttribute(): void {
    if (this.rootElement !== null) {
      this.rootElement.setAttribute("data-nlos-disclosure", this.level);
    }
  }

  private commitMutation(startedAtMs: number): DisclosureSnapshot {
    this.syncRootAttribute();
    const endedAtMs = this.nowMs();
    this.lastTransitionDurationMs = Math.max(0, endedAtMs - startedAtMs);
    const snap = this.getSnapshot();
    for (const cb of this.subscribers) {
      cb(snap);
    }
    return snap;
  }

  public getLevel(): DisclosureLevel {
    return this.level;
  }

  public getPreDiagnosticLevel(): NonDiagnosticDisclosureLevel {
    return this.preDiagnosticLevel;
  }

  public getSelectedEntityId(): string | null {
    return this.selectedEntityId;
  }

  public getSelectedDomain(): string | null {
    return this.selectedDomain;
  }

  public getVisibleRowWindow(): VisibleRowWindow {
    return { ...this.visibleRowWindow };
  }

  public getExpandedL3ParentIds(): ReadonlySet<string> {
    return new Set(this.expandedL3ParentIds);
  }

  public getSnapshot(): DisclosureSnapshot {
    return {
      level: this.level,
      preDiagnosticLevel: this.preDiagnosticLevel,
      selectedEntityId: this.selectedEntityId,
      selectedDomain: this.selectedDomain,
      visibleRowWindow: { ...this.visibleRowWindow },
      expandedL3ParentIds: new Set(this.expandedL3ParentIds),
      lastTransitionDurationMs: this.lastTransitionDurationMs,
    };
  }

  public bindRootElement(rootElement: DisclosureRootElementLike | null): void {
    this.rootElement = rootElement;
    this.syncRootAttribute();
  }

  public subscribe(listener: DisclosureSubscriber): () => void {
    this.subscribers.add(listener);
    return () => {
      this.subscribers.delete(listener);
    };
  }

  public setLevel(nextLevel: DisclosureLevel): DisclosureSnapshot {
    const startedAtMs = this.nowMs();
    if (nextLevel === "L3" && this.level !== "L3") {
      this.preDiagnosticLevel = this.level;
    } else if (nextLevel !== "L3") {
      this.preDiagnosticLevel = nextLevel;
    }
    this.level = nextLevel;
    return this.commitMutation(startedAtMs);
  }

  public selectEntity(
    entityId: string | null,
    domain?: string | null,
  ): DisclosureSnapshot {
    const startedAtMs = this.nowMs();
    this.selectedEntityId = entityId;
    if (domain !== undefined) {
      this.selectedDomain = domain;
    }
    return this.commitMutation(startedAtMs);
  }

  public setVisibleViewport(
    viewportHeightPx: number,
    rowHeightPx: number = this.visibleRowWindow.rowHeightPx,
    cursorHex: string | null = this.visibleRowWindow.cursorHex,
  ): DisclosureSnapshot {
    const startedAtMs = this.nowMs();
    this.visibleRowWindow = {
      cursorHex,
      limit: computeVisibleCursorLimit(viewportHeightPx, rowHeightPx),
      viewportHeightPx,
      rowHeightPx,
    };
    return this.commitMutation(startedAtMs);
  }

  public setCursor(cursorHex: string | null): DisclosureSnapshot {
    const startedAtMs = this.nowMs();
    this.visibleRowWindow = {
      ...this.visibleRowWindow,
      cursorHex,
    };
    return this.commitMutation(startedAtMs);
  }

  public expandL3Parent(parentId: string): DisclosureSnapshot {
    const startedAtMs = this.nowMs();
    if (isNonEmptyEntityId(parentId)) {
      this.expandedL3ParentIds.add(parentId);
    }
    return this.commitMutation(startedAtMs);
  }

  public collapseL3Parent(parentId: string): DisclosureSnapshot {
    const startedAtMs = this.nowMs();
    this.expandedL3ParentIds.delete(parentId);
    return this.commitMutation(startedAtMs);
  }

  public toggleL3Parent(parentId: string): DisclosureSnapshot {
    if (this.expandedL3ParentIds.has(parentId)) {
      return this.collapseL3Parent(parentId);
    }
    return this.expandL3Parent(parentId);
  }

  public clearExpandedL3Parents(): DisclosureSnapshot {
    const startedAtMs = this.nowMs();
    this.expandedL3ParentIds.clear();
    return this.commitMutation(startedAtMs);
  }

  /**
   * `Space` / `Enter` non-modal `L2` inspector drawer transition:
   * - From `L0` or `L1`: enters `L2` (updating `selectedEntityId` when provided).
   * - From `L2` with the same entity (or `entityId === undefined`): returns to `L1`.
   * - From `L2` with a different entity: updates `selectedEntityId` in-place in `L2`.
   * - From `L3`: updates `selectedEntityId` (if provided) and focuses `L2` drawer.
   */
  public toggleDrawerL2(entityId?: string | null): DisclosureSnapshot {
    const startedAtMs = this.nowMs();
    if (this.level === "L2") {
      if (entityId === undefined || entityId === this.selectedEntityId) {
        this.level = "L1";
        this.preDiagnosticLevel = "L1";
      } else {
        this.selectedEntityId = entityId;
        this.preDiagnosticLevel = "L2";
      }
      return this.commitMutation(startedAtMs);
    }

    if (entityId !== undefined) {
      this.selectedEntityId = entityId;
    }
    this.level = "L2";
    this.preDiagnosticLevel = "L2";
    return this.commitMutation(startedAtMs);
  }

  /**
   * `Escape` key handler for non-modal `L2` drawer: transitions `L2 -> L1`
   * while preserving `selectedEntityId` and cursor state.
   */
  public closeDrawerOnEscape(): boolean {
    if (this.level !== "L2") {
      return false;
    }
    const startedAtMs = this.nowMs();
    this.level = "L1";
    this.preDiagnosticLevel = "L1";
    this.commitMutation(startedAtMs);
    return true;
  }

  /**
   * `Ctrl/Cmd + Shift + E` in-place toggle to `"L3"` and back to the remembered
   * pre-`L3` level (`L0 | L1 | L2`, default `L1`), preserving `selectedEntityId`,
   * `selectedDomain`, `visibleRowWindow`, and `expandedL3ParentIds`.
   */
  public toggleDiagnosticL3(): DisclosureLevel {
    const startedAtMs = this.nowMs();
    if (this.level !== "L3") {
      this.preDiagnosticLevel = this.level;
      this.level = "L3";
    } else {
      this.level = this.preDiagnosticLevel;
    }
    this.commitMutation(startedAtMs);
    return this.level;
  }

  public handleDisclosureKeydown(event: DisclosureKeydownEvent): boolean {
    const modCtrlOrMeta = Boolean(event.ctrlKey) || Boolean(event.metaKey);
    const shift = Boolean(event.shiftKey);
    const alt = Boolean(event.altKey);

    if (modCtrlOrMeta && shift && !alt && (event.key === "E" || event.key === "e")) {
      this.toggleDiagnosticL3();
      return true;
    }

    if (!modCtrlOrMeta && !shift && !alt && event.key === "Escape") {
      return this.closeDrawerOnEscape();
    }

    if (event.targetIsTextInput) {
      return false;
    }

    if (
      !modCtrlOrMeta &&
      !shift &&
      !alt &&
      (event.key === " " || event.key === "Spacebar" || event.key === "Enter")
    ) {
      if (event.focusedRowEntityId === null && this.level !== "L2") {
        return false;
      }
      if (event.focusedRowEntityId !== undefined && event.focusedRowEntityId !== null) {
        this.toggleDrawerL2(event.focusedRowEntityId);
      } else {
        this.toggleDrawerL2();
      }
      return true;
    }

    return false;
  }

  public isQueryAllowedAtDisclosure(
    queryKind: ProjectionQueryKind,
    context?: DisclosureQueryGateContext,
  ): boolean;
  public isQueryAllowedAtDisclosure(
    level: DisclosureLevel,
    queryKind: ProjectionQueryKind,
    context?: DisclosureQueryGateContext,
  ): boolean;
  public isQueryAllowedAtDisclosure(
    first: DisclosureLevel | ProjectionQueryKind,
    second?: ProjectionQueryKind | DisclosureQueryGateContext,
    third?: DisclosureQueryGateContext,
  ): boolean {
    if (isDisclosureLevelValue(first)) {
      const queryKind = second as ProjectionQueryKind;
      return isQueryAllowedAtDisclosure(first, queryKind, {
        targetEntityId: third?.targetEntityId,
        selectedEntityId:
          third?.selectedEntityId !== undefined
            ? third.selectedEntityId
            : this.selectedEntityId,
        expandedParentIds: third?.expandedParentIds ?? this.expandedL3ParentIds,
      });
    }
    const queryKind = first;
    const ctx = second as DisclosureQueryGateContext | undefined;
    return isQueryAllowedAtDisclosure(this.level, queryKind, {
      targetEntityId: ctx?.targetEntityId,
      selectedEntityId:
        ctx?.selectedEntityId !== undefined
          ? ctx.selectedEntityId
          : this.selectedEntityId,
      expandedParentIds: ctx?.expandedParentIds ?? this.expandedL3ParentIds,
    });
  }
}

export function createDisclosureModel(
  options?: DisclosureModelOptions,
): DisclosureModel {
  return new DisclosureModel(options);
}

export function handleDisclosureKeydown(
  model: DisclosureModel,
  event: DisclosureKeydownEvent,
): boolean {
  return model.handleDisclosureKeydown(event);
}

// ============================================================================
// Task Space Row-Level & Canvas-Level Lazy-Pulling Cursor Coordinator (W72)
// Authority: Stage D Visual & UX Spec §2A.2, §2.4, §3.2 (`[PLAN-LAZY-001]`)
// ============================================================================

export interface TaskSpaceCursorSlice {
  readonly queryKind: ProjectionQueryKind;
  readonly tier: ProjectionQueryTier;
  readonly parentEntityId: string | null;
  readonly supernodeId: string | null;
  readonly cursorHex: string | null;
  readonly limit: number;
  readonly expandLazySupernodes: boolean;
}

export interface TaskSpaceCanvasSupernodeCursor {
  readonly taskId: string;
  readonly supernodeId: string;
  readonly cursorHex: string | null;
  readonly limit: number;
  readonly expanded: boolean;
}

export interface TaskSpaceDisclosurePullPlan {
  readonly level: DisclosureLevel;
  readonly selectedTaskId: string | null;
  readonly visibleRowLimit: number;
  readonly allowedSlices: readonly TaskSpaceCursorSlice[];
  readonly blockedTiers: readonly ProjectionQueryTier[];
  readonly zeroL2PrefetchGuaranteed: boolean;
  readonly zeroL3PrefetchGuaranteed: boolean;
}

export interface TaskSpacePullAuditEntry {
  readonly seq: number;
  readonly level: DisclosureLevel;
  readonly queryKind: ProjectionQueryKind;
  readonly tier: ProjectionQueryTier;
  readonly targetEntityId: string | null;
  readonly supernodeId: string | null;
  readonly requestedLimit: number;
  readonly effectiveLimit: number;
  readonly allowed: boolean;
  readonly reason:
    | "ALLOWED_WATERMARK_L0"
    | "ALLOWED_VISIBLE_ROW_WINDOW_L1"
    | "ALLOWED_SINGLE_ROW_ATTRIBUTION_L2"
    | "ALLOWED_DIAGNOSTIC_SLICE_L3"
    | "ALLOWED_SUPERNODE_ON_DEMAND_L3"
    | "BLOCKED_TIER_AT_CURRENT_DISCLOSURE"
    | "BLOCKED_UNSELECTED_ROW_AT_L2"
    | "BLOCKED_UNEXPANDED_PARENT_AT_L3"
    | "BLOCKED_SUPERNODE_EXPANSION_OUTSIDE_L3";
}

export function clampTaskSpaceCursorLimit(requestedLimit?: number): number {
  if (
    requestedLimit === undefined ||
    !Number.isFinite(requestedLimit) ||
    requestedLimit <= 0
  ) {
    return MAX_SYSTEM_CONTROL_ENUM_LIMIT;
  }
  return Math.max(
    1,
    Math.min(MAX_SYSTEM_CONTROL_ENUM_LIMIT, Math.floor(requestedLimit)),
  );
}

const L3_TASK_SPACE_QUERY_KINDS: readonly ProjectionQueryKind[] = [
  "list_task_groups",
  "list_task_nodes",
  "list_task_attempts",
  "list_execution_fibers",
  "list_operations",
  "inspect_cell_leases",
];

export class TaskSpaceLazyCursorCoordinator {
  private readonly model: DisclosureModel;
  private readonly rowCursors = new Map<string, string | null>();
  private readonly supernodeCursors = new Map<string, TaskSpaceCanvasSupernodeCursor>();
  private readonly auditLog: TaskSpacePullAuditEntry[] = [];
  private nextAuditSeq = 1;

  public constructor(model?: DisclosureModel) {
    this.model = model ?? new DisclosureModel({ initialLevel: "L1" });
  }

  public getDisclosureModel(): DisclosureModel {
    return this.model;
  }

  public getAuditLog(): readonly TaskSpacePullAuditEntry[] {
    return [...this.auditLog];
  }

  public clearAuditLog(): void {
    this.auditLog.length = 0;
  }

  public setEntityCursor(
    queryKind: ProjectionQueryKind,
    entityId: string | null,
    cursorHex: string | null,
  ): void {
    const key = `${queryKind}:${entityId ?? "*"}`;
    this.rowCursors.set(key, cursorHex);
  }

  public getEntityCursor(
    queryKind: ProjectionQueryKind,
    entityId: string | null,
  ): string | null {
    const key = `${queryKind}:${entityId ?? "*"}`;
    return this.rowCursors.get(key) ?? null;
  }

  private recordAudit(
    entry: Omit<TaskSpacePullAuditEntry, "seq">,
  ): TaskSpacePullAuditEntry {
    const full: TaskSpacePullAuditEntry = {
      seq: this.nextAuditSeq++,
      ...entry,
    };
    this.auditLog.push(full);
    return full;
  }

  /**
   * Computes the deterministic `L0 -> L1 -> L2 -> L3` pull plan for `TaskSpace`:
   * - `L0`: watermark posture only (`0` list slices).
   * - `L1`: `list_tasks` bounded by `visibleRowWindow.limit <= 64`; strictly `0` `L2` and `0` `L3` slices.
   * - `L2`: `L1` + single selected task's `list_task_nodes` (collapsed super-nodes NOT expanded) and
   *   `inspect_resource_cost`; strictly `0` `L3` slices.
   * - `L3`: `L2` + parent-scoped `L3` slices (`list_task_groups`, `list_task_attempts`,
   *   `list_execution_fibers`, `list_operations`, `inspect_cell_leases`) and any explicitly expanded
   *   `[PLAN-LAZY-001]` canvas super-nodes (`limit <= 64`).
   */
  public computePullPlan(): TaskSpaceDisclosurePullPlan {
    const snapshot = this.model.getSnapshot();
    const level = snapshot.level;
    const selectedTaskId = snapshot.selectedEntityId;
    const visibleLimit = clampTaskSpaceCursorLimit(snapshot.visibleRowWindow.limit);
    const slices: TaskSpaceCursorSlice[] = [];

    if (level === "L0") {
      slices.push({
        queryKind: "poll_watermark",
        tier: "L0",
        parentEntityId: null,
        supernodeId: null,
        cursorHex: null,
        limit: 1,
        expandLazySupernodes: false,
      });
      return {
        level,
        selectedTaskId,
        visibleRowLimit: visibleLimit,
        allowedSlices: slices,
        blockedTiers: ["L1", "L2", "L3"],
        zeroL2PrefetchGuaranteed: true,
        zeroL3PrefetchGuaranteed: true,
      };
    }

    // L1, L2, L3 always include the bounded visible L1 task commitment window
    slices.push({
      queryKind: "list_tasks",
      tier: "L1",
      parentEntityId: null,
      supernodeId: null,
      cursorHex: snapshot.visibleRowWindow.cursorHex,
      limit: visibleLimit,
      expandLazySupernodes: false,
    });

    if (level === "L1") {
      return {
        level,
        selectedTaskId,
        visibleRowLimit: visibleLimit,
        allowedSlices: slices,
        blockedTiers: ["L2", "L3"],
        zeroL2PrefetchGuaranteed: true,
        zeroL3PrefetchGuaranteed: true,
      };
    }

    // L2 or L3: pull single selected row's L2 attribution summary if a row is selected
    if (isNonEmptyEntityId(selectedTaskId)) {
      slices.push(
        {
          queryKind: "list_task_nodes",
          tier: "L2",
          parentEntityId: selectedTaskId,
          supernodeId: null,
          cursorHex: this.getEntityCursor("list_task_nodes", selectedTaskId),
          limit: visibleLimit,
          expandLazySupernodes: false,
        },
        {
          queryKind: "inspect_resource_cost",
          tier: "L2",
          parentEntityId: selectedTaskId,
          supernodeId: null,
          cursorHex: null,
          limit: 1,
          expandLazySupernodes: false,
        },
      );
    }

    if (level === "L2") {
      return {
        level,
        selectedTaskId,
        visibleRowLimit: visibleLimit,
        allowedSlices: slices,
        blockedTiers: ["L3"],
        zeroL2PrefetchGuaranteed: false,
        zeroL3PrefetchGuaranteed: true,
      };
    }

    // L3: pull parent-scoped L3 slices for selectedTaskId and expandedL3ParentIds
    const activeParents = new Set<string>(snapshot.expandedL3ParentIds);
    if (isNonEmptyEntityId(selectedTaskId)) {
      activeParents.add(selectedTaskId);
    }

    for (const parentId of activeParents) {
      for (const queryKind of L3_TASK_SPACE_QUERY_KINDS) {
        if (queryKind === "list_task_nodes" && parentId === selectedTaskId) {
          continue;
        }
        slices.push({
          queryKind,
          tier: getProjectionQueryTier(queryKind),
          parentEntityId: parentId,
          supernodeId: null,
          cursorHex: this.getEntityCursor(queryKind, parentId),
          limit: clampTaskSpaceCursorLimit(MAX_SYSTEM_CONTROL_ENUM_LIMIT),
          expandLazySupernodes: false,
        });
      }
    }

    for (const superCursor of this.supernodeCursors.values()) {
      if (superCursor.expanded && activeParents.has(superCursor.taskId)) {
        slices.push({
          queryKind: "list_task_nodes",
          tier: "L2",
          parentEntityId: superCursor.taskId,
          supernodeId: superCursor.supernodeId,
          cursorHex: superCursor.cursorHex,
          limit: clampTaskSpaceCursorLimit(superCursor.limit),
          expandLazySupernodes: true,
        });
      }
    }

    return {
      level,
      selectedTaskId,
      visibleRowLimit: visibleLimit,
      allowedSlices: slices,
      blockedTiers: [],
      zeroL2PrefetchGuaranteed: false,
      zeroL3PrefetchGuaranteed: false,
    };
  }

  /**
   * Validates and records a row-level or canvas-level query pull against the active disclosure fence.
   */
  public requestSlicePull(
    queryKind: ProjectionQueryKind,
    options?: {
      readonly targetEntityId?: string | null;
      readonly supernodeId?: string | null;
      readonly requestedLimit?: number;
      readonly cursorHex?: string | null;
    },
  ): TaskSpacePullAuditEntry {
    const level = this.model.getLevel();
    const tier = getProjectionQueryTier(queryKind);
    const targetEntityId = options?.targetEntityId ?? null;
    const supernodeId = options?.supernodeId ?? null;
    const requestedLimit = options?.requestedLimit ?? MAX_SYSTEM_CONTROL_ENUM_LIMIT;
    const effectiveLimit = clampTaskSpaceCursorLimit(requestedLimit);

    if (isNonEmptyEntityId(supernodeId) && level !== "L3") {
      return this.recordAudit({
        level,
        queryKind,
        tier,
        targetEntityId,
        supernodeId,
        requestedLimit,
        effectiveLimit,
        allowed: false,
        reason: "BLOCKED_SUPERNODE_EXPANSION_OUTSIDE_L3",
      });
    }

    const allowed = this.model.isQueryAllowedAtDisclosure(queryKind, {
      targetEntityId,
    });

    if (!allowed) {
      let reason: TaskSpacePullAuditEntry["reason"] =
        "BLOCKED_TIER_AT_CURRENT_DISCLOSURE";
      if (level === "L2" && tier === "L2") {
        reason = "BLOCKED_UNSELECTED_ROW_AT_L2";
      } else if (level === "L3" && (tier === "L2" || tier === "L3")) {
        reason = "BLOCKED_UNEXPANDED_PARENT_AT_L3";
      }
      return this.recordAudit({
        level,
        queryKind,
        tier,
        targetEntityId,
        supernodeId,
        requestedLimit,
        effectiveLimit,
        allowed: false,
        reason,
      });
    }

    if (options?.cursorHex !== undefined) {
      this.setEntityCursor(queryKind, targetEntityId, options.cursorHex);
    }

    const reason: TaskSpacePullAuditEntry["reason"] = isNonEmptyEntityId(supernodeId)
      ? "ALLOWED_SUPERNODE_ON_DEMAND_L3"
      : tier === "L0"
        ? "ALLOWED_WATERMARK_L0"
        : tier === "L1"
          ? "ALLOWED_VISIBLE_ROW_WINDOW_L1"
          : tier === "L2"
            ? "ALLOWED_SINGLE_ROW_ATTRIBUTION_L2"
            : "ALLOWED_DIAGNOSTIC_SLICE_L3";

    return this.recordAudit({
      level,
      queryKind,
      tier,
      targetEntityId,
      supernodeId,
      requestedLimit,
      effectiveLimit,
      allowed: true,
      reason,
    });
  }

  /**
   * On-demand `[PLAN-LAZY-001]` canvas super-node expansion (`+N 待物化步骤 · 0 进程`).
   * Strictly gated to `L3` DAG canvas and clamped to `limit <= 64`.
   */
  public expandCanvasSupernode(
    taskId: string,
    supernodeId: string,
    requestedLimit: number = MAX_SYSTEM_CONTROL_ENUM_LIMIT,
    cursorHex: string | null = null,
  ): {
    readonly allowed: boolean;
    readonly cursor: TaskSpaceCanvasSupernodeCursor | null;
    readonly audit: TaskSpacePullAuditEntry;
  } {
    const effectiveLimit = clampTaskSpaceCursorLimit(requestedLimit);
    const level = this.model.getLevel();
    if (level !== "L3" || !isNonEmptyEntityId(taskId) || !isNonEmptyEntityId(supernodeId)) {
      const audit = this.recordAudit({
        level,
        queryKind: "list_task_nodes",
        tier: "L2",
        targetEntityId: taskId,
        supernodeId,
        requestedLimit,
        effectiveLimit,
        allowed: false,
        reason: "BLOCKED_SUPERNODE_EXPANSION_OUTSIDE_L3",
      });
      return { allowed: false, cursor: null, audit };
    }

    this.model.expandL3Parent(taskId);
    const key = `${taskId}::${supernodeId}`;
    const cursor: TaskSpaceCanvasSupernodeCursor = {
      taskId,
      supernodeId,
      cursorHex,
      limit: effectiveLimit,
      expanded: true,
    };
    this.supernodeCursors.set(key, cursor);
    const audit = this.requestSlicePull("list_task_nodes", {
      targetEntityId: taskId,
      supernodeId,
      requestedLimit: effectiveLimit,
      cursorHex,
    });
    return { allowed: audit.allowed, cursor, audit };
  }

  public collapseCanvasSupernode(taskId: string, supernodeId: string): void {
    const key = `${taskId}::${supernodeId}`;
    const existing = this.supernodeCursors.get(key);
    if (existing !== undefined) {
      this.supernodeCursors.set(key, {
        ...existing,
        expanded: false,
      });
    }
  }

  public getCanvasSupernodeCursor(
    taskId: string,
    supernodeId: string,
  ): TaskSpaceCanvasSupernodeCursor | null {
    return this.supernodeCursors.get(`${taskId}::${supernodeId}`) ?? null;
  }
}

export function createTaskSpaceLazyCursorCoordinator(
  model?: DisclosureModel,
): TaskSpaceLazyCursorCoordinator {
  return new TaskSpaceLazyCursorCoordinator(model);
}

