import { invoke } from "@tauri-apps/api/core";

import {
  MAX_DESKTOP_WATERMARK_EVENT_LIMIT,
  MAX_SYSTEM_CONTROL_ENUM_LIMIT,
  SEARCH_NON_AUTHORITATIVE_RESTRICTIONS_ALL,
  type AnyControlActionInput,
  type CompositorInputDispatchInput,
  type CompositorSnapshotDto,
  type ConfigDto,
  type ControlActionInput,
  type ControlPlaneFactsDto,
  type ControlScopeDto,
  type DesktopFailure,
  type ExtendedOutcomeDto,
  type FactCheckDto,
  type InputRouteResultDto,
  type OutcomeDto,
  type ParityDto,
  type PreviewActionDto,
  type ReceiptDto,
  type RectPxDto,
  type SettingsDomainKindDto,
  type StageDControlActionInput,
  type StageDEnumOutcomeDto,
  type StageDMutationOutcomeDto,
  type SurfacesPresentationDto,
  type TypedReceiptDto,
  type WorkspaceLayoutModeKindDto,
} from "./types";

function failClosedControlError(message: string): DesktopFailure {
  return { code: "CONTROL", message };
}

/** 有界游标分页页宽规整(`1..=64`)。非法非正数以 CONFIG 失败关闭,超上限钳制到 64。 */
export function clampEnumLimit(limit = MAX_SYSTEM_CONTROL_ENUM_LIMIT): number {
  if (!Number.isInteger(limit) || limit <= 0) {
    throw {
      code: "CONFIG",
      message: "enumeration limit must be a positive integer (1..=64)",
    } satisfies DesktopFailure;
  }
  return Math.min(limit, MAX_SYSTEM_CONTROL_ENUM_LIMIT);
}

/** 水位线增量事件页宽规整(`1..=128`)。 */
export function clampWatermarkLimit(
  limit = MAX_DESKTOP_WATERMARK_EVENT_LIMIT,
): number {
  if (!Number.isInteger(limit) || limit <= 0) {
    throw {
      code: "CONFIG",
      message: "watermark event limit must be a positive integer (1..=128)",
    } satisfies DesktopFailure;
  }
  return Math.min(limit, MAX_DESKTOP_WATERMARK_EVENT_LIMIT);
}

function assertNonAuthoritativeSearchInspect(receipt: ReceiptDto): ReceiptDto {
  if (
    receipt.outcome.kind === "search_inspected" &&
    receipt.outcome.nonAuthoritativeRestrictionsBits !==
      SEARCH_NON_AUTHORITATIVE_RESTRICTIONS_ALL
  ) {
    throw failClosedControlError(
      `search inspection violated 0x1F non-authoritative restriction bits: 0x${receipt.outcome.nonAuthoritativeRestrictionsBits.toString(16)}`,
    );
  }
  return receipt;
}

function assertNonAuthoritativeSearchCandidates<
  O extends ExtendedOutcomeDto,
>(receipt: TypedReceiptDto<O>): TypedReceiptDto<O> {
  if (receipt.outcome.kind === "search_candidates_queried") {
    if (
      receipt.outcome.nonAuthoritativeRestrictionsBits !==
      SEARCH_NON_AUTHORITATIVE_RESTRICTIONS_ALL
    ) {
      throw failClosedControlError(
        `search candidate summary violated 0x1F non-authoritative restriction bits: 0x${receipt.outcome.nonAuthoritativeRestrictionsBits.toString(16)}`,
      );
    }
    for (const candidate of receipt.outcome.candidates) {
      if (
        candidate.nonAuthoritativeRestrictionsBits !==
        SEARCH_NON_AUTHORITATIVE_RESTRICTIONS_ALL
      ) {
        throw failClosedControlError(
          `search candidate ${candidate.candidateIdHex} violated 0x1F non-authoritative restriction bits: 0x${candidate.nonAuthoritativeRestrictionsBits.toString(16)}`,
        );
      }
    }
  }
  return receipt;
}

export async function getConfig(): Promise<ConfigDto> {
  return invoke("get_config");
}

export interface SetConfigInput {
  socketPath?: string | null;
  principalHex?: string | null;
  keyFile?: string | null;
  cliSocket?: string | null;
  cliPath?: string | null;
  resourceRoot?: string | null;
  applicationRoot?: string | null;
}

export async function setConfig(input: SetConfigInput): Promise<ConfigDto> {
  return invoke("set_config", { input });
}

export async function inspectHealth(): Promise<ReceiptDto> {
  return invoke("inspect_health");
}

export async function inspectSemanticHealth(): Promise<ReceiptDto> {
  return invoke("inspect_semantic_health");
}

export async function inspectResourceHealth(): Promise<ReceiptDto> {
  return invoke("inspect_resource_health");
}

export async function exportMetrics(): Promise<ReceiptDto> {
  return invoke("export_metrics");
}

export async function exportSemanticMetrics(): Promise<ReceiptDto> {
  return invoke("export_semantic_metrics");
}

/** W32-E 资源监控:resource 域(G8)OpenMetrics 导出(既有只读命令的 GUI 接线)。 */
export async function exportResourceMetrics(): Promise<ReceiptDto> {
  return invoke("export_resource_metrics");
}

export async function inspectTask(planIdHex: string): Promise<ReceiptDto> {
  return invoke("inspect_task", { planIdHex });
}

/** W39-D:Task Space 五层 inspect(既有 SABI v1.5 ControlCommand 的 GUI 接线)。 */
export async function inspectTaskGroup(groupIdHex: string): Promise<ReceiptDto> {
  return invoke("inspect_task_group", { groupIdHex });
}

export async function inspectTaskNode(
  planIdHex: string,
  nodeIdHex: string,
): Promise<ReceiptDto> {
  return invoke("inspect_task_node", { planIdHex, nodeIdHex });
}

export async function inspectExecutionFiber(
  fiberIdHex: string,
  generation: number,
): Promise<ReceiptDto> {
  return invoke("inspect_execution_fiber", { fiberIdHex, generation });
}

export async function inspectTopic(topicIdHex: string): Promise<ReceiptDto> {
  return invoke("inspect_topic", { topicIdHex });
}

export async function inspectOperation(
  operationIdHex: string,
  generation: number,
): Promise<ReceiptDto> {
  return invoke("inspect_operation", { operationIdHex, generation });
}

export async function inspectProcess(processIdHex: string): Promise<ReceiptDto> {
  return invoke("inspect_process", { processIdHex });
}

/** InspectApplication:application_root 接线时组装应用头;未接线为类型化 NOT_FOUND。 */
export async function inspectApplication(packageIdHex: string): Promise<ReceiptDto> {
  return invoke("inspect_application", { packageIdHex });
}

export async function inspectResource(reservationIdHex: string): Promise<ReceiptDto> {
  return invoke("inspect_resource", { reservationIdHex });
}

/** W66 + Stage D (`IPC-D4`): 非权威搜索域 inspect,客户端与后端双重强制 `0x1F` 五禁掩码 fail-closed。 */
export async function inspectSearch(scopeIdHex: string): Promise<ReceiptDto> {
  const receipt = await invoke<ReceiptDto>("inspect_search", { scopeIdHex });
  return assertNonAuthoritativeSearchInspect(receipt);
}

/** W66 + Stage D (`IPC-D6`): 通知订阅与有界离线历史 inspect。 */
export async function inspectNotification(
  notificationIdHex: string,
): Promise<ReceiptDto> {
  return invoke("inspect_notification", { notificationIdHex });
}

/** W32-D:预算/成本查询(会话 resource_root 接线时组装真实有界成本事实)。 */
export async function inspectResourceCost(reservationIdHex: string): Promise<ReceiptDto> {
  return invoke("inspect_resource_cost", { reservationIdHex });
}

/** W32-D 一致性自检:同一 reservation 两次独立认证派发,渲染事实 vs 直接复检。 */
export async function costFactCheck(reservationIdHex: string): Promise<FactCheckDto> {
  return invoke("cost_fact_check", { reservationIdHex });
}

/** W32-D 控制面授权事实(客户端路径常量)。 */
export async function controlPlaneFacts(): Promise<ControlPlaneFactsDto> {
  return invoke("control_plane_facts");
}

/** W32-F:按包身份呈现应用声明的可呈现表面(本地应用权威直读)。 */
export async function presentSurfaces(packageIdHex: string): Promise<SurfacesPresentationDto> {
  return invoke("present_surfaces", { packageIdHex });
}

// =========================================================================
// Stage D (`IPC-D1..D8` & `[UI-PARITY-001]`) 十二域有界分页、水位线、预演与对账
// =========================================================================

export type StageDReadReceipt<K extends StageDEnumOutcomeDto["kind"]> =
  TypedReceiptDto<
    | Extract<StageDEnumOutcomeDto, { kind: K }>
    | Extract<StageDEnumOutcomeDto, { kind: "stale_generation" }>
    | Extract<OutcomeDto, { kind: "failure" }>
  >;

export async function listTasks(
  cursorHex?: string | null,
  limit = MAX_SYSTEM_CONTROL_ENUM_LIMIT,
): Promise<StageDReadReceipt<"tasks_listed">> {
  return invoke("list_tasks", {
    cursorHex: cursorHex ?? null,
    limit: clampEnumLimit(limit),
  });
}

export async function listTaskGroups(
  taskIdHex?: string | null,
  cursorHex?: string | null,
  limit = MAX_SYSTEM_CONTROL_ENUM_LIMIT,
): Promise<StageDReadReceipt<"task_groups_listed">> {
  return invoke("list_task_groups", {
    taskIdHex: taskIdHex ?? null,
    cursorHex: cursorHex ?? null,
    limit: clampEnumLimit(limit),
  });
}

export async function listTaskNodes(
  planIdHex: string,
  expectedPlanRevision = 0,
  cursorHex?: string | null,
  limit = MAX_SYSTEM_CONTROL_ENUM_LIMIT,
): Promise<StageDReadReceipt<"task_nodes_listed">> {
  return invoke("list_task_nodes", {
    planIdHex,
    expectedPlanRevision,
    cursorHex: cursorHex ?? null,
    limit: clampEnumLimit(limit),
  });
}

export async function listTaskAttempts(
  scopeFilterIdHex?: string | null,
  expectedGeneration = 0,
  cursorHex?: string | null,
  limit = MAX_SYSTEM_CONTROL_ENUM_LIMIT,
): Promise<StageDReadReceipt<"task_attempts_listed">> {
  return invoke("list_task_attempts", {
    scopeFilterIdHex: scopeFilterIdHex ?? null,
    expectedGeneration,
    cursorHex: cursorHex ?? null,
    limit: clampEnumLimit(limit),
  });
}

export async function listExecutionFibers(
  processIdHex?: string | null,
  expectedGeneration = 0,
  cursorHex?: string | null,
  limit = MAX_SYSTEM_CONTROL_ENUM_LIMIT,
): Promise<StageDReadReceipt<"execution_fibers_listed">> {
  return invoke("list_execution_fibers", {
    processIdHex: processIdHex ?? null,
    expectedGeneration,
    cursorHex: cursorHex ?? null,
    limit: clampEnumLimit(limit),
  });
}

export async function listOperations(
  fiberIdHex?: string | null,
  expectedGeneration = 0,
  cursorHex?: string | null,
  limit = MAX_SYSTEM_CONTROL_ENUM_LIMIT,
): Promise<StageDReadReceipt<"operations_listed">> {
  return invoke("list_operations", {
    fiberIdHex: fiberIdHex ?? null,
    expectedGeneration,
    cursorHex: cursorHex ?? null,
    limit: clampEnumLimit(limit),
  });
}

export async function listInstalledApplications(
  cursorHex?: string | null,
  limit = MAX_SYSTEM_CONTROL_ENUM_LIMIT,
): Promise<StageDReadReceipt<"installed_applications_listed">> {
  return invoke("list_installed_applications", {
    cursorHex: cursorHex ?? null,
    limit: clampEnumLimit(limit),
  });
}

export async function querySearchCandidates(
  scopeIdHex: string,
  query: string,
  cursorOffset = 0,
  limit = MAX_SYSTEM_CONTROL_ENUM_LIMIT,
): Promise<StageDReadReceipt<"search_candidates_queried">> {
  const receipt = await invoke<StageDReadReceipt<"search_candidates_queried">>(
    "query_search_candidates",
    {
      scopeIdHex,
      query,
      cursorOffset,
      limit: clampEnumLimit(limit),
    },
  );
  return assertNonAuthoritativeSearchCandidates(receipt);
}

export async function pollDesktopWatermarkEvents(
  sinceWatermark = 0,
  limit = MAX_DESKTOP_WATERMARK_EVENT_LIMIT,
): Promise<StageDReadReceipt<"desktop_watermark_polled">> {
  return invoke("poll_desktop_watermark_events", {
    sinceWatermark,
    limit: clampWatermarkLimit(limit),
  });
}

export async function inspectSettingsDomain(
  domainKind: SettingsDomainKindDto,
  scopeTargetIdHex?: string | null,
  limit = MAX_SYSTEM_CONTROL_ENUM_LIMIT,
): Promise<StageDReadReceipt<"settings_domain_inspected">> {
  return invoke("inspect_settings_domain", {
    domainKind,
    scopeTargetIdHex: scopeTargetIdHex ?? null,
    limit: clampEnumLimit(limit),
  });
}

export async function listContentTypeAssociations(
  cursorHex?: string | null,
  limit = MAX_SYSTEM_CONTROL_ENUM_LIMIT,
): Promise<StageDReadReceipt<"content_type_associations_listed">> {
  return invoke("list_content_type_associations", {
    cursorHex: cursorHex ?? null,
    limit: clampEnumLimit(limit),
  });
}

export async function listKeychainReferences(
  cursorHex?: string | null,
  limit = MAX_SYSTEM_CONTROL_ENUM_LIMIT,
): Promise<StageDReadReceipt<"keychain_references_listed">> {
  return invoke("list_keychain_references", {
    cursorHex: cursorHex ?? null,
    limit: clampEnumLimit(limit),
  });
}

export async function listChooserGrants(
  cursorHex?: string | null,
  limit = MAX_SYSTEM_CONTROL_ENUM_LIMIT,
): Promise<StageDReadReceipt<"chooser_grants_listed">> {
  return invoke("list_chooser_grants", {
    cursorHex: cursorHex ?? null,
    limit: clampEnumLimit(limit),
  });
}

export async function listWorkspaces(
  cursorHex?: string | null,
  limit = MAX_SYSTEM_CONTROL_ENUM_LIMIT,
): Promise<StageDReadReceipt<"workspaces_listed">> {
  return invoke("list_workspaces", {
    cursorHex: cursorHex ?? null,
    limit: clampEnumLimit(limit),
  });
}

export async function inspectWorkspace(
  workspaceIdHex: string,
  expectedRevision = 0,
): Promise<StageDReadReceipt<"workspace_inspected">> {
  return invoke("inspect_workspace", {
    workspaceIdHex,
    expectedRevision,
  });
}

export async function listProcesses(
  cursorHex?: string | null,
  limit = MAX_SYSTEM_CONTROL_ENUM_LIMIT,
): Promise<StageDReadReceipt<"processes_listed">> {
  return invoke("list_processes", {
    cursorHex: cursorHex ?? null,
    limit: clampEnumLimit(limit),
  });
}

export async function listResourceReservations(
  cursorHex?: string | null,
  limit = MAX_SYSTEM_CONTROL_ENUM_LIMIT,
): Promise<StageDReadReceipt<"resource_reservations_listed">> {
  return invoke("list_resource_reservations", {
    cursorHex: cursorHex ?? null,
    limit: clampEnumLimit(limit),
  });
}

export async function listTopics(
  cursorHex?: string | null,
  limit = MAX_SYSTEM_CONTROL_ENUM_LIMIT,
): Promise<StageDReadReceipt<"topics_listed">> {
  return invoke("list_topics", {
    cursorHex: cursorHex ?? null,
    limit: clampEnumLimit(limit),
  });
}

export async function listNotifications(
  cursorHex?: string | null,
  limit = MAX_SYSTEM_CONTROL_ENUM_LIMIT,
): Promise<StageDReadReceipt<"notifications_listed">> {
  return invoke("list_notifications", {
    cursorHex: cursorHex ?? null,
    limit: clampEnumLimit(limit),
  });
}

export async function listCells(
  cursorHex?: string | null,
  limit = MAX_SYSTEM_CONTROL_ENUM_LIMIT,
): Promise<StageDReadReceipt<"cells_listed">> {
  return invoke("list_cells", {
    cursorHex: cursorHex ?? null,
    limit: clampEnumLimit(limit),
  });
}

export async function inspectCellLeases(
  cellIdHex?: string | null,
  limit = MAX_SYSTEM_CONTROL_ENUM_LIMIT,
): Promise<StageDReadReceipt<"cell_leases_inspected">> {
  return invoke("inspect_cell_leases", {
    cellIdHex: cellIdHex ?? null,
    limit: clampEnumLimit(limit),
  });
}

export async function previewControlBlastRadius(
  targetScope: ControlScopeDto,
  targetIdHex: string,
  previewAction: PreviewActionDto,
  secondaryIdHex?: string | null,
  limit = MAX_SYSTEM_CONTROL_ENUM_LIMIT,
): Promise<StageDReadReceipt<"blast_radius_previewed">> {
  return invoke("preview_control_blast_radius", {
    targetScope,
    targetIdHex,
    secondaryIdHex: secondaryIdHex ?? null,
    previewAction,
    limit: clampEnumLimit(limit),
  });
}

export async function queryControlCommandStatus(
  targetCommandIdHex: string,
): Promise<StageDReadReceipt<"command_status_queried">> {
  return invoke("query_control_command_status", { targetCommandIdHex });
}

export const getControlCommandStatus = queryControlCommandStatus;

export async function parityCheck(
  operation: string,
  targetHex?: string | null,
): Promise<ParityDto> {
  return invoke("parity_check", { operation, targetHex: targetHex ?? null });
}

function normalizeControlAction(
  action: AnyControlActionInput,
): AnyControlActionInput {
  switch (action.action) {
    case "pause-operation":
      return action.scope !== undefined
        ? {
            action: "scoped-operation",
            scope: action.scope,
            verb: "pause",
            targetIdHex: action.targetIdHex,
            expectedRevision: action.expectedRevision,
            reason: action.reason,
          }
        : action;
    case "resume-operation":
      return action.scope !== undefined
        ? {
            action: "scoped-operation",
            scope: action.scope,
            verb: "resume",
            targetIdHex: action.targetIdHex,
            expectedRevision: action.expectedRevision,
            reason: action.reason,
          }
        : action;
    case "cancel-operation":
      return action.scope !== undefined
        ? {
            action: "scoped-operation",
            scope: action.scope,
            verb: "cancel",
            targetIdHex: action.targetIdHex,
            expectedRevision: action.expectedRevision,
            reason: action.reason,
          }
        : action;
    case "kill-operation":
      return action.scope !== undefined
        ? {
            action: "scoped-operation",
            scope: action.scope,
            verb: "kill",
            targetIdHex: action.targetIdHex,
            expectedRevision: action.expectedRevision,
            reason: action.reason,
          }
        : action;
    case "throttle-operation":
      return action.scope !== undefined
        ? {
            action: "scoped-operation",
            scope: action.scope,
            verb: "throttle",
            targetIdHex: action.targetIdHex,
            expectedRevision: action.expectedRevision,
            throttlePercent: action.throttlePercent,
            reason: action.reason,
          }
        : action;
    case "reclaim-operation":
      return action.scope !== undefined
        ? {
            action: "scoped-operation",
            scope: action.scope,
            verb: "reclaim",
            targetIdHex: action.targetIdHex,
            expectedRevision: action.expectedRevision,
            reason: action.reason,
          }
        : action;
    default:
      return action;
  }
}

/** W32-B + Stage D 写路径唯一入口:授权动作 → 真实 ControlCommand → 认证 IPC。 */
export async function submitControl(
  action: ControlActionInput,
): Promise<ReceiptDto>;
export async function submitControl(
  action: StageDControlActionInput,
): Promise<TypedReceiptDto<OutcomeDto | StageDMutationOutcomeDto>>;
export async function submitControl(
  action: AnyControlActionInput,
): Promise<TypedReceiptDto<OutcomeDto | StageDMutationOutcomeDto>> {
  return invoke("submit_control", { action: normalizeControlAction(action) });
}

/** 写路径一致性探针(pause-operation):同命令 GUI 认证入口 vs CLI plain 入口。 */
export async function parityCheckWrite(
  commandIdHex: string,
  targetHex: string,
  expectedRevision: number,
  reason: string,
): Promise<ParityDto> {
  return invoke("parity_check_write", {
    commandIdHex,
    targetHex,
    expectedRevision,
    reason,
  });
}

// =========================================================================
// Stage D (`D-COMPOSITOR-WM`) 五平面合成器宿主状态与焦点防伪路由 IPC 接线
// =========================================================================

export type CompositorSessionTransitionKind =
  | "unlock"
  | "lock"
  | "suspend"
  | "resume"
  | "switch_user"
  | "close";

export type CompositorWorkspaceActionKind =
  | "create"
  | "create_and_activate"
  | "activate"
  | "archive"
  | "close";

export type CompositorWindowActionKind =
  | "focus"
  | "minimize"
  | "restore"
  | "float"
  | "tile"
  | "toggle_floating"
  | "move_floating"
  | "begin_close"
  | "close"
  | "reload_stale";

export interface CompositorSetLayoutOptions {
  layoutMode: WorkspaceLayoutModeKindDto;
  viewportWidth?: number | null;
  viewportHeight?: number | null;
  topHeightRatioPermille?: number | null;
  splitWidthRatioPermille?: number | null;
  z1Visible?: boolean | null;
}

export interface CompositorLaunchApplicationInput {
  applicationIdHex?: string | null;
  installationIdHex?: string | null;
  rejectedPackageIdHex?: string | null;
  rejectedProcessIdHex?: string | null;
}

export async function compositorGetSnapshot(
  includeZ4Trusted = false,
): Promise<CompositorSnapshotDto> {
  return invoke("compositor_get_snapshot", { includeZ4Trusted });
}

export async function compositorSessionTransition(
  transition: CompositorSessionTransitionKind,
  targetPrincipalHex?: string | null,
  nowMonoMs?: number | null,
): Promise<CompositorSnapshotDto> {
  return invoke("compositor_session_transition", {
    transition,
    targetPrincipalHex: targetPrincipalHex ?? null,
    nowMonoMs: nowMonoMs ?? null,
  });
}

export async function compositorWorkspaceAction(
  action: CompositorWorkspaceActionKind,
  workspaceIdHex: string,
): Promise<CompositorSnapshotDto> {
  return invoke("compositor_workspace_action", { action, workspaceIdHex });
}

export async function compositorSetLayout(
  options: CompositorSetLayoutOptions,
): Promise<CompositorSnapshotDto> {
  const payload = {
    layoutMode: options.layoutMode,
    viewportWidth: options.viewportWidth ?? null,
    viewportHeight: options.viewportHeight ?? null,
    topHeightRatioPermille: options.topHeightRatioPermille ?? null,
    splitWidthRatioPermille: options.splitWidthRatioPermille ?? null,
    z1Visible: options.z1Visible ?? null,
  };
  return invoke("compositor_set_layout", {
    ...payload,
    options: payload,
    input: payload,
  });
}

export async function compositorLaunchApplication(
  input: CompositorLaunchApplicationInput,
): Promise<CompositorSnapshotDto> {
  const payload = {
    applicationIdHex: input.applicationIdHex ?? null,
    installationIdHex: input.installationIdHex ?? null,
    rejectedPackageIdHex: input.rejectedPackageIdHex ?? null,
    rejectedProcessIdHex: input.rejectedProcessIdHex ?? null,
  };
  return invoke("compositor_launch_application", {
    ...payload,
    input: payload,
  });
}

export async function compositorWindowAction(
  windowIdHex: string,
  action: CompositorWindowActionKind,
  rect?: RectPxDto | null,
  newInstallationIdHex?: string | null,
): Promise<CompositorSnapshotDto> {
  return invoke("compositor_window_action", {
    windowIdHex,
    action,
    rect: rect ?? null,
    newInstallationIdHex: newInstallationIdHex ?? null,
  });
}

export async function compositorSetZ3Overlay(
  overlayId: string,
  kind: "flyout" | "palette",
  active: boolean,
  rect?: RectPxDto | null,
): Promise<CompositorSnapshotDto> {
  return invoke("compositor_set_z3_overlay", {
    overlayId,
    kind,
    active,
    rect: rect ?? null,
  });
}

export async function compositorSetZ4Modal(
  active: boolean,
  nowMonoMs: number,
): Promise<CompositorSnapshotDto> {
  return invoke("compositor_set_z4_modal", { active, nowMonoMs });
}

export async function compositorRouteInput(
  input: CompositorInputDispatchInput,
): Promise<InputRouteResultDto> {
  const winHex = input.targetWindowIdHex ?? input.windowIdHex ?? null;
  const surfHex = input.targetSurfaceIdHex ?? input.surfaceIdHex ?? null;
  const sessionGen = input.eventSessionGeneration ?? input.sessionGeneration;
  const normalized: CompositorInputDispatchInput = {
    source: input.source,
    originKind: input.originKind,
    originSurfaceIdHex: input.originSurfaceIdHex ?? null,
    originApplicationIdHex: input.originApplicationIdHex ?? null,
    targetKind: input.targetKind,
    targetWindowIdHex: winHex,
    windowIdHex: winHex,
    targetSurfaceIdHex: surfHex,
    surfaceIdHex: surfHex,
    targetSurfaceGeneration: input.targetSurfaceGeneration ?? null,
    shortcut: input.shortcut ?? null,
    confirm: input.confirm ?? null,
    key: input.key ?? null,
    sessionGeneration: sessionGen,
    eventSessionGeneration: sessionGen,
    nowMonoMs: input.nowMonoMs,
  };
  return invoke("compositor_route_input", {
    input: normalized,
  });
}
