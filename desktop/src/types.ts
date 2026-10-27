// 后端 serde DTO 的前端镜像(desktop/src-tauri/src/dto.rs 为唯一权威)。

export interface DesktopFailure {
  code: string;
  message: string;
}

/** `[CONS-CTRL-001]` / `[UI-PARITY-001]` 非权威搜索五禁全集掩码(`0x1F`)。 */
export const SEARCH_NON_AUTHORITATIVE_RESTRICTIONS_ALL = 0x1f as const;

/** SABI `SystemControl` 有界分页单次最大页宽(`limit <= 64`)。 */
export const MAX_SYSTEM_CONTROL_ENUM_LIMIT = 64 as const;

/** SABI `PollDesktopWatermarkEvents` 有界水位线事件单次最大页宽(`limit <= 128`)。 */
export const MAX_DESKTOP_WATERMARK_EVENT_LIMIT = 128 as const;

/** `IPC-D8`: 显式一等控制作用域枚举(`ControlScope` 1..=17)。 */
export type ControlScopeDto =
  | "system"
  | "operation"
  | "workspace"
  | "application"
  | "task"
  | "task-group"
  | "task-node"
  | "task-attempt"
  | "process"
  | "agent-instance"
  | "execution-fiber"
  | "resource-group"
  | "context"
  | "topic"
  | "channel"
  | "device"
  | "cell";

export type SettingsDomainKindDto =
  | "system"
  | "user"
  | "workspace"
  | "application";

export type TaskUndoClassDto =
  | "reversible"
  | "compensatable"
  | "irreversible";

export type PlanNodeStateDto =
  | "declared"
  | "eligible"
  | "active"
  | "completed"
  | "failed"
  | "cancelled"
  | "skipped";

export type ScopedControlVerbDto =
  | "pause"
  | "resume"
  | "cancel"
  | "kill"
  | "throttle"
  | "reclaim"
  | "disable-application"
  | "uninstall-application";

export type PreviewActionDto =
  | ScopedControlVerbDto
  | "migrate"
  | "purge-topic";

export interface RecoveryAlertDto {
  planIdHex: string;
  totalFailures: number;
  acknowledgedReceiptIdHex: string | null;
}

export type OutcomeDto =
  | {
      kind: "inspected";
      workerState: string;
      completedCycles: number;
      durableRetrying: number;
      durableEscalated: number;
      durableUnacknowledgedEscalated: number;
      durableResolved: number;
      alerts: RecoveryAlertDto[];
    }
  | {
      kind: "semantic_inspected";
      totalInspected: number;
      totalFinalized: number;
      consecutiveFailedCycles: number;
      domainFaulted: boolean;
      durableRetrying: number;
      durableEscalated: number;
      durableUnacknowledgedEscalated: number;
      durableResolved: number;
      alerts: RecoveryAlertDto[];
    }
  | {
      kind: "resource_recovery_inspected";
      totalInspected: number;
      totalFinalized: number;
      consecutiveFailedCycles: number;
      domainFaulted: boolean;
      durableRetrying: number;
      durableEscalated: number;
      durableUnacknowledgedEscalated: number;
      durableResolved: number;
      alerts: RecoveryAlertDto[];
    }
  | {
      kind: "process_inspected";
      processIdHex: string;
      processGeneration: number;
      agentInstanceIdHex: string;
      taskIdHex: string;
      taskAttemptIdHex: string;
      isolationDomainIdHex: string;
    }
  | {
      kind: "resource_inspected";
      reservationIdHex: string;
      accountIdHex: string;
      upperBound: number;
      usageHighWater: number;
      consumptionCount: number;
    }
  | {
      kind: "application_inspected";
      packageIdHex: string;
      applicationIdHex: string;
      packageManifestDigestHex: string;
      currentInstallationGeneration: number;
      status: number;
      createdAtMs: number;
      updatedAtMs: number;
    }
  | {
      kind: "search_inspected";
      scopeIdHex: string;
      consistencyClass: number;
      degradationPolicy: number;
      nonAuthoritativeRestrictionsBits: number;
      admittedEventCount: number;
      activeAssertionCount: number;
      retractedEventCount: number;
      truncated: boolean;
    }
  | {
      kind: "notification_inspected";
      notificationIdHex: string;
      topicIdHex: string;
      subscriberKeyHex: string;
      nextSequence: number;
      historyEntryCount: number;
      historyTruncated: boolean;
      pruneWatermarkSequence: number;
    }
  | {
      kind: "task_group_inspected";
      groupIdHex: string;
      taskIdHex: string;
      parentGroupIdHex: string | null;
      state: string;
      membershipGeneration: number;
      stateSeq: number;
      depth: number;
      cancelEpoch: number;
      createdAtMs: number;
      updatedAtMs: number;
      memberCount: number;
      membersTruncated: boolean;
    }
  | {
      kind: "task_node_inspected";
      planIdHex: string;
      nodeIdHex: string;
      nodeKind: string;
      state: string;
      declaredRevision: number;
      nodeDigestHex: string;
      transitionCount: number;
      residencyTier: string;
      residencyTransitionCount: number;
      firstDeclaredAtMs: number;
      updatedAtMs: number;
    }
  | {
      kind: "execution_fiber_inspected";
      fiberIdHex: string;
      generation: number;
      state: string;
      lifecyclePhase: string;
      activeCpuMs: number;
      elapsedWallMs: number;
      schedulerWaitMs: number;
      externalWaitMs: number;
      backpressureWaitMs: number;
      suspendedMs: number;
    }
  | {
      kind: "topic_inspected";
      topicIdHex: string;
      channelIdHex: string;
      channelGeneration: number;
      nameHex: string;
      activeSubscriptions: number;
      policyDigestHex: string;
      createdAtMs: number;
    }
  | {
      kind: "durable_operation_inspected";
      operationIdHex: string;
      generation: number;
      state: string;
      cancelEpoch: number;
      ownerFiberIdHex: string;
      ownerFiberGeneration: number;
      outcomeReceiptIdHex: string | null;
    }
  | { kind: "metrics_exported"; openmetricsText: string }
  | { kind: "failure"; code: string; retry: string; safeMessage: string }
  | { kind: "acknowledged"; receiptIdHex: string }
  | { kind: "resumed"; receiptIdHex: string }
  | { kind: "operation_paused"; receiptIdHex: string }
  | { kind: "operation_resumed"; receiptIdHex: string }
  | { kind: "operation_cancelled"; receiptIdHex: string }
  | { kind: "operation_killed"; receiptIdHex: string }
  | { kind: "operation_throttled"; receiptIdHex: string }
  | { kind: "operation_reclaimed"; receiptIdHex: string }
  | { kind: "application_disabled"; receiptIdHex: string }
  | { kind: "application_uninstalled"; receiptIdHex: string };

/** Stage D (`IPC-D1..D8`) 十二域有界枚举、水位线、爆炸半径预演与命令状态对账 DTO。 */
export interface TaskSummaryDto {
  taskIdHex: string;
  planIdHex: string;
  applicationIdHex: string | null;
  installationGeneration: number;
  planRevision: number;
  taskState: string;
  undoClass: string;
  reservationIdHex: string | null;
  cellIdHex: string | null;
  updatedAtMs: number;
  expectedGenerationOrRevision: number;
}

export interface TaskGroupSummaryDto {
  groupIdHex: string;
  taskIdHex: string;
  parentGroupIdHex: string | null;
  state: string;
  membershipGeneration: number;
  stateSeq: number;
  depth: number;
  cancelEpoch: number;
  createdAtMs: number;
  updatedAtMs: number;
}

export interface TaskNodeSummaryDto {
  planIdHex: string;
  nodeIdHex: string;
  nodeKind: string;
  state: string;
  declaredRevision: number;
  nodeDigestHex: string;
  transitionCount: number;
  residencyTier: string;
  residencyTransitionCount: number;
  firstDeclaredAtMs: number;
  updatedAtMs: number;
}

export interface TaskAttemptSummaryDto {
  attemptIdHex: string;
  taskIdHex: string;
  nodeIdHex: string | null;
  state: string;
  isolationDomainIdHex: string;
  providerIsolationUnverified: boolean;
  casWinner: boolean;
  expectedGenerationOrRevision: number;
  startedAtMs: number;
  updatedAtMs: number;
}

export interface ExecutionFiberSummaryDto {
  fiberIdHex: string;
  generation: number;
  state: string;
  lifecyclePhase: string;
  activeCpuMs: number;
  elapsedWallMs: number;
  schedulerWaitMs: number;
  externalWaitMs: number;
  backpressureWaitMs: number;
  suspendedMs: number;
  processIdHex: string | null;
  sharedThreadLike: boolean;
}

export interface DurableOperationSummaryDto {
  operationIdHex: string;
  generation: number;
  state: string;
  cancelEpoch: number;
  ownerFiberIdHex: string;
  ownerFiberGeneration: number;
  outcomeReceiptIdHex: string | null;
}

export interface InstalledApplicationSummaryDto {
  applicationIdHex: string;
  installationIdHex: string;
  packageIdHex: string;
  packageManifestDigestHex: string;
  currentInstallationGeneration: number;
  status: number;
  createdAtMs: number;
  updatedAtMs: number;
}

export interface SearchCandidateDto {
  candidateIdHex: string;
  objectIdHex: string;
  scopeIdHex: string;
  titleSnippet: string;
  rankScoreMilli: number;
  nonAuthoritativeRestrictionsBits: number;
}

export interface DesktopWatermarkEventDto {
  watermark: number;
  domainScope: string;
  entityIdHex: string;
  newGenerationOrRevision: number;
  stateDigestHex: string;
  occurredAtMs: number;
}

export interface SettingsEntryDto {
  key: string;
  valueCborHex: string;
  revision: number;
}

export interface ContentTypeAssociationDto {
  associationIdHex: string;
  contentType: string;
  applicationIdHex: string;
  expectedGenerationOrRevision: number;
}

export interface KeychainReferenceDto {
  keychainRefIdHex: string;
  ownerPrincipalIdHex: string;
  label: string;
  publicFingerprintHex: string;
  expectedGenerationOrRevision: number;
  hardwareBacked: boolean;
}

export interface ChooserGrantDto {
  grantIdHex: string;
  applicationIdHex: string;
  chooserKind: number;
  resourceHandleHex: string;
  expectedGenerationOrRevision: number;
  expiresAtMs: number;
}

export interface WorkspaceDto {
  workspaceIdHex: string;
  name: string;
  layoutPreset: number;
  activeSurfaceCount: number;
  focusedTaskIdHex: string | null;
  expectedGenerationOrRevision: number;
  updatedAtMs: number;
}

export interface ProcessSummaryDto {
  processIdHex: string;
  expectedGenerationOrRevision: number;
  agentInstanceIdHex: string;
  taskIdHex: string;
  taskAttemptIdHex: string;
  isolationDomainIdHex: string;
  cellIdHex: string | null;
  lifecycleState: number;
}

export interface ResourceReservationSummaryDto {
  reservationIdHex: string;
  accountIdHex: string;
  actualUsage: number;
  effectiveGranted: number;
  requestedCeiling: number;
  contextResidencyTier: string;
  expectedGenerationOrRevision: number;
}

export interface TopicSummaryDto {
  topicIdHex: string;
  channelIdHex: string;
  channelGeneration: number;
  nameHex: string;
  activeSubscriptions: number;
  policyDigestHex: string;
  createdAtMs: number;
  expectedGenerationOrRevision: number;
  retainedMessages: number;
}

export interface NotificationSummaryDto {
  notificationIdHex: string;
  topicIdHex: string;
  subscriberKeyHex: string;
  nextSequence: number;
  historyEntryCount: number;
  historyTruncated: boolean;
  pruneWatermarkSequence: number;
  expectedGenerationOrRevision: number;
}

export interface CellSummaryDto {
  cellIdHex: string;
  cellLabel: string;
  reachability: string;
  partitionAutonomyGate: string;
  currentEpoch: number;
  activeLeaseCount: number;
  expectedGenerationOrRevision: number;
}

export interface CellLeaseDto {
  leaseIdHex: string;
  cellIdHex: string;
  kind: string;
  lockState: string;
  holderIdHex: string;
  remainingHolderTtlMs: number;
  leaseEpoch: number;
  clockSkewHorizonMs: number;
}

export interface ControlBlastRadiusImpactDto {
  sign: string;
  affectedScope: string;
  affectedIdHex: string;
  currentGenerationOrRevision: number;
  summaryCode: string;
}

export type StageDEnumOutcomeDto =
  | {
      kind: "stale_generation";
      view: string;
      targetIdHex: string | null;
      expectedGeneration: number;
      currentGeneration: number;
    }
  | {
      kind: "tasks_listed";
      tasks: TaskSummaryDto[];
      nextCursorHex: string | null;
      truncated: boolean;
    }
  | {
      kind: "task_groups_listed";
      groups: TaskGroupSummaryDto[];
      nextCursorHex: string | null;
      truncated: boolean;
    }
  | {
      kind: "task_nodes_listed";
      planIdHex: string;
      currentPlanRevision: number;
      staleGeneration: boolean;
      nodes: TaskNodeSummaryDto[];
      metadataOnlyCollapsedCount: number;
      coldCollapsedCount: number;
      nextCursorHex: string | null;
      truncated: boolean;
    }
  | {
      kind: "task_attempts_listed";
      currentGeneration: number;
      staleGeneration: boolean;
      attempts: TaskAttemptSummaryDto[];
      nextCursorHex: string | null;
      truncated: boolean;
    }
  | {
      kind: "execution_fibers_listed";
      currentGeneration: number;
      staleGeneration: boolean;
      fibers: ExecutionFiberSummaryDto[];
      nextCursorHex: string | null;
      truncated: boolean;
    }
  | {
      kind: "operations_listed";
      currentGeneration: number;
      staleGeneration: boolean;
      operations: DurableOperationSummaryDto[];
      nextCursorHex: string | null;
      truncated: boolean;
    }
  | {
      kind: "installed_applications_listed";
      applications: InstalledApplicationSummaryDto[];
      nextCursorHex: string | null;
      truncated: boolean;
    }
  | {
      kind: "search_candidates_queried";
      scopeIdHex: string;
      nonAuthoritativeRestrictionsBits: number;
      achievedConsistency: number;
      failedShards: number[];
      candidates: SearchCandidateDto[];
      nextCursorHex: string | null;
      truncated: boolean;
    }
  | {
      kind: "desktop_watermark_polled";
      currentWatermark: number;
      events: DesktopWatermarkEventDto[];
      truncated: boolean;
    }
  | {
      kind: "settings_domain_inspected";
      domainKind: string;
      scopeTargetIdHex: string | null;
      domainRevision: number;
      entries: SettingsEntryDto[];
    }
  | {
      kind: "content_type_associations_listed";
      associations: ContentTypeAssociationDto[];
      nextCursorHex: string | null;
      truncated: boolean;
    }
  | {
      kind: "keychain_references_listed";
      references: KeychainReferenceDto[];
      nextCursorHex: string | null;
      truncated: boolean;
    }
  | {
      kind: "chooser_grants_listed";
      grants: ChooserGrantDto[];
      nextCursorHex: string | null;
      truncated: boolean;
    }
  | {
      kind: "workspaces_listed";
      workspaces: WorkspaceDto[];
      nextCursorHex: string | null;
      truncated: boolean;
    }
  | ({ kind: "workspace_inspected" } & WorkspaceDto)
  | {
      kind: "processes_listed";
      processes: ProcessSummaryDto[];
      nextCursorHex: string | null;
      truncated: boolean;
    }
  | {
      kind: "resource_reservations_listed";
      reservations: ResourceReservationSummaryDto[];
      nextCursorHex: string | null;
      truncated: boolean;
    }
  | {
      kind: "topics_listed";
      topics: TopicSummaryDto[];
      nextCursorHex: string | null;
      truncated: boolean;
    }
  | {
      kind: "notifications_listed";
      notifications: NotificationSummaryDto[];
      nextCursorHex: string | null;
      truncated: boolean;
    }
  | {
      kind: "cells_listed";
      cells: CellSummaryDto[];
      nextCursorHex: string | null;
      truncated: boolean;
    }
  | {
      kind: "cell_leases_inspected";
      cellIdHex: string;
      leases: CellLeaseDto[];
      truncated: boolean;
    }
  | {
      kind: "blast_radius_previewed";
      targetScope: string;
      targetIdHex: string;
      previewAction: number;
      statutoryLockBlocked: boolean;
      statutoryLockCode: string;
      impacts: ControlBlastRadiusImpactDto[];
      truncated: boolean;
    }
  | {
      kind: "command_status_queried";
      controlCommandIdHex: string;
      state: string;
      scope: string;
      targetIdHex: string;
      receiptIdHex: string | null;
      failureCode: string | null;
      failureRetry: string | null;
      failureSafeMessage: string | null;
    };

export type StageDMutationOutcomeDto =
  | { kind: "process_migration_requested"; receiptIdHex: string }
  | { kind: "topic_purged"; receiptIdHex: string }
  | { kind: "settings_domain_updated"; receiptIdHex: string }
  | { kind: "content_type_association_set"; receiptIdHex: string }
  | { kind: "chooser_grant_issued"; receiptIdHex: string }
  | { kind: "receipt_undone"; receiptIdHex: string };

export type ExtendedOutcomeDto =
  | OutcomeDto
  | StageDEnumOutcomeDto
  | StageDMutationOutcomeDto;

/** mutation 成功形态(渲染样式与读侧巡检区分)。 */
export const MUTATION_OUTCOME_KINDS: ReadonlySet<OutcomeDto["kind"]> = new Set([
  "acknowledged",
  "resumed",
  "operation_paused",
  "operation_resumed",
  "operation_cancelled",
  "operation_killed",
  "operation_throttled",
  "operation_reclaimed",
  "application_disabled",
  "application_uninstalled",
]);

export const STAGE_D_MUTATION_OUTCOME_KINDS: ReadonlySet<
  StageDMutationOutcomeDto["kind"]
> = new Set([
  "process_migration_requested",
  "topic_purged",
  "settings_domain_updated",
  "content_type_association_set",
  "chooser_grant_issued",
  "receipt_undone",
]);

/** W32-B + Stage D 应用生命周期与显式 ControlScope 授权动作(desktop/src-tauri/src/ipc.rs 为权威)。 */
export type ControlActionInput =
  | {
      action: "ack-recovery-alert";
      planIdHex: string;
      expectedTotalFailures: number;
      reason: string;
    }
  | {
      action: "ack-semantic-recovery-alert";
      planIdHex: string;
      expectedTotalFailures: number;
      reason: string;
    }
  | {
      action: "resume-semantic-recovery";
      planIdHex: string;
      expectedTotalFailures: number;
      reason: string;
    }
  | {
      action: "ack-resource-recovery-alert";
      planIdHex: string;
      expectedTotalFailures: number;
      reason: string;
    }
  | {
      action: "resume-resource-recovery";
      planIdHex: string;
      expectedTotalFailures: number;
      reason: string;
    }
  | {
      action: "pause-operation";
      targetIdHex: string;
      expectedRevision: number;
      scope?: ControlScopeDto;
      reason: string;
    }
  | {
      action: "resume-operation";
      targetIdHex: string;
      expectedRevision: number;
      scope?: ControlScopeDto;
      reason: string;
    }
  | {
      action: "cancel-operation";
      targetIdHex: string;
      expectedRevision: number;
      scope?: ControlScopeDto;
      reason: string;
    }
  | {
      action: "kill-operation";
      targetIdHex: string;
      expectedRevision: number;
      scope?: ControlScopeDto;
      reason: string;
    }
  | {
      action: "throttle-operation";
      targetIdHex: string;
      expectedRevision: number;
      throttlePercent: number;
      scope?: ControlScopeDto;
      reason: string;
    }
  | {
      action: "reclaim-operation";
      targetIdHex: string;
      expectedRevision: number;
      scope?: ControlScopeDto;
      reason: string;
    }
  | {
      action: "disable-application";
      targetIdHex: string;
      expectedRevision: number;
      scope?: ControlScopeDto;
      reason: string;
    }
  | {
      action: "uninstall-application";
      targetIdHex: string;
      expectedRevision: number;
      scope?: ControlScopeDto;
      reason: string;
    }
  | {
      action: "scoped-operation";
      scope: ControlScopeDto;
      verb: ScopedControlVerbDto;
      targetIdHex: string;
      expectedRevision: number;
      throttlePercent?: number;
      reason: string;
    };

/** Stage D 跨十二域写路径动作与五大法定不可篡改锁探针。 */
export type StageDControlActionInput =
  | {
      action: "request-process-migration";
      processIdHex: string;
      destinationCellIdHex: string;
      expectedRevision: number;
      reason: string;
    }
  | {
      action: "purge-topic";
      topicIdHex: string;
      expectedRevision: number;
      acknowledgeDataLoss?: boolean;
      reason: string;
    }
  | {
      action: "update-settings-domain";
      domainKind: SettingsDomainKindDto;
      scope?: ControlScopeDto;
      scopeTargetIdHex?: string | null;
      expectedDomainRevision: number;
      key: string;
      valueCborHex: string;
      reason: string;
    }
  | {
      action: "set-content-type-association";
      associationIdHex: string;
      contentType: string;
      applicationIdHex: string;
      expectedRevision: number;
      reason: string;
    }
  | {
      action: "issue-chooser-grant";
      grantIdHex: string;
      applicationIdHex: string;
      chooserKind: number;
      resourceHandleHex: string;
      expectedRevision: number;
      reason: string;
    }
  | {
      action: "undo-receipt";
      scope?: ControlScopeDto;
      targetIdHex: string;
      undoClass: TaskUndoClassDto;
      expectedRevision: number;
      reason: string;
    }
  | {
      action: "mutate-search-candidate";
      candidateIdHex: string;
      scopeIdHex: string;
      restrictionsBits?: number;
      expectedRevision: number;
      reason: string;
    }
  | {
      action: "mutate-task-node-topology";
      planIdHex: string;
      nodeIdHex: string;
      observedState: PlanNodeStateDto;
      expectedPlanRevision: number;
      reason: string;
    }
  | {
      action: "override-task-attempt-commit";
      attemptIdHex: string;
      expectedGeneration: number;
      reason: string;
    }
  | {
      action: "override-effect-unknown-operation";
      operationIdHex: string;
      markSucceeded?: boolean;
      expectedGeneration: number;
      reason: string;
    };

export type AnyControlActionInput =
  | ControlActionInput
  | StageDControlActionInput;

export interface TypedReceiptDto<O = ExtendedOutcomeDto> {
  controlCommandIdHex: string;
  correlationIdHex: string;
  /** ControlReceipt::to_bytes 的 hex——与 CLI `RECEIPT <hex>` 同一等价契约。 */
  receiptHex: string;
  outcome: O;
}

export interface ReceiptDto extends TypedReceiptDto<OutcomeDto> {
  outcome: OutcomeDto;
}

export interface ConfigDto {
  socketPath: string | null;
  principalHex: string | null;
  keyFile: string | null;
  cliSocket: string | null;
  cliPath: string | null;
  /** W32-D:本地资源权威根目录(预算/成本可见性的接线;null=未接线)。 */
  resourceRoot: string | null;
  /** W32-F:本地应用权威根目录(UI Surface 呈现的接线;null=未接线)。 */
  applicationRoot: string | null;
  source: "env" | "session" | "unset";
  platformSupported: boolean;
}

/** W32-F:一条可呈现表面(durable 声明事实的投影,desktop/src-tauri/src/surfaces.rs 为权威)。 */
export interface PresentedSurfaceDto {
  surfaceIdHex: string;
  kind: "window" | "panel";
  title: string;
  entryName: string | null;
  registrationKeyHex: string;
  registeredAtMs: number;
}

/** 当前代际进程绑定(inspect_process_bindings 回执投影)。 */
export interface PresentedProcessBindingDto {
  processIdHex: string;
  registrantPrincipalHex: string;
  applicationGeneration: number;
  registrationKeyHex: string;
  registeredAtMs: number;
}

/** 当前代际后台任务登记(inspect_background_tasks 回执投影)。 */
export interface PresentedBackgroundTaskDto {
  taskIdHex: string;
  registrantPrincipalHex: string;
  applicationGeneration: number;
  registrationKeyHex: string;
  registeredAtMs: number;
}

/** W32-F:一个应用的呈现事实(当前 durable 状态 + 当前代际可呈现表面集)。 */
export interface SurfacesPresentationDto {
  applicationIdHex: string;
  packageIdHex: string;
  applicationGeneration: number;
  status: "installed" | "disabled" | "uninstalled";
  packageManifestDigestHex: string;
  presentableSurfaces: PresentedSurfaceDto[];
  processBindings: PresentedProcessBindingDto[];
  backgroundTasks: PresentedBackgroundTaskDto[];
}

/** W32-D 控制面授权事实(客户端路径常量,非 inspect 数据)。 */
export interface ControlPlaneFactsDto {
  service: string;
  capabilitySlot: number;
  capabilityGeneration: number;
}

/** W32-D 一致性自检的一行事实比对(渲染值 vs 复检值)。 */
export interface FactRowDto {
  field: string;
  first: string;
  second: string;
  matched: boolean;
}

/** W32-D 一致性自检结果:同一 reservation 两次独立认证派发的比对。 */
export interface FactCheckDto {
  reservationIdHex: string;
  matched: boolean;
  receiptHexMatched: boolean;
  firstReceiptHex: string;
  secondReceiptHex: string;
  rows: FactRowDto[];
}

export interface ParityDto {
  operation: string;
  guiReceiptHex: string;
  cliReceiptHex: string | null;
  cliExitCode: number | null;
  matched: boolean;
  cliStderr: string | null;
}

// =========================================================================
// Stage D (`D-COMPOSITOR-WM`) 五平面桌面合成器、窗口管理与焦点输入防伪路由 DTO
// =========================================================================

export interface RectPxDto {
  x: number;
  y: number;
  width: number;
  height: number;
}

export type DesktopSessionStateDto =
  | "LOCKED"
  | "ACTIVE"
  | "SUSPENDED"
  | "CLOSED";

export type WorkspaceLifecycleStateDto =
  | "CREATED"
  | "ACTIVE"
  | "ARCHIVED"
  | "CLOSED";

export type WindowLifecycleStateDto =
  | "CREATED"
  | "VISIBLE"
  | "MINIMIZED"
  | "CLOSING"
  | "CLOSED";

export type SurfaceLifecycleStateDto =
  | "REGISTERED"
  | "CREATED"
  | "PRESENTED"
  | "HIDDEN"
  | "CLOSED";

export type WorkspaceLayoutModeKindDto =
  | "three-pane-workstation"
  | "two-column-split"
  | "quadrants"
  | "workspace-maximize"
  | "floating";

export type WorkstationThemeDto =
  | "graphite-workstation"
  | "paper-workstation"
  | "system";

export type InputSourceKindDto =
  | "keyboard"
  | "pointer"
  | "voice"
  | "drag_drop"
  | "automation";

export interface UserProfilePublicDto {
  principalIdHex: string;
  displayName: string;
  locale: "zh-CN" | "en-US";
  theme: WorkstationThemeDto;
  dataContainerScopeIdHex: string;
}

export interface Z4TrustedSealContextDto {
  principalIdHex: string;
  displayName: string;
  securitySealHint: string;
  sessionGeneration: number;
  z4ArmedAtMonoMs: number | null;
  armingWindowMs: number;
}

export interface SessionWorkspaceDto {
  workspaceIdHex: string;
  ownerPrincipalIdHex: string;
  name: string;
  state: WorkspaceLifecycleStateDto;
  revision: number;
  layoutMode: WorkspaceLayoutModeKindDto;
}

export interface WindowAccessibilityNodeDto {
  a11yNodeId: string;
  a11yRole: string;
  a11yLabel: string;
  ownerApplicationIdHex: string;
  installationGeneration: number;
  staleReadonly: boolean;
  hidden: boolean;
}

export interface CompositorWindowDto {
  windowIdHex: string;
  surfaceIdHex: string;
  title: string;
  entryName: string | null;
  ownerApplicationIdHex: string;
  installationIdHex: string;
  packageIdHex: string;
  workspaceIdHex: string;
  sessionGeneration: number;
  surfaceGeneration: number;
  currentInstallationGeneration: number;
  staleGeneration: boolean;
  windowLifecycle: WindowLifecycleStateDto;
  surfaceLifecycle: SurfaceLifecycleStateDto;
  layoutSlot: string;
  isFloating: boolean;
  zIndex: number;
  borderRadiusPx: number;
  outerFrameRect: RectPxDto;
  innerContentClipRect: RectPxDto;
  occludedBySystemPlane: boolean;
  a11y: WindowAccessibilityNodeDto;
}

export interface CompositorZ3OverlayDto {
  overlayId: string;
  kind: "flyout" | "palette";
  rect: RectPxDto;
  zIndex: number;
}

export interface InputAuditCountersDto {
  acceptedEvents: number;
  rejectedStaleSurfaceEvents: number;
  rejectedLockedSessionEvents: number;
  rejectedZ4SeveredEvents: number;
  rejectedZ4ArmingGuardEvents: number;
  rejectedUntrustedSpoofEvents: number;
}

export interface FocusAndInputRouteDto {
  focusedWindowIdHex: string | null;
  focusedSurfaceIdHex: string | null;
  focusedSurfaceGeneration: number | null;
  sessionGeneration: number;
  z4ModalLock: boolean;
  z4ArmedAtMonoMs: number | null;
  counters: InputAuditCountersDto;
}

export interface CompositorSnapshotDto {
  sessionState: DesktopSessionStateDto;
  sessionGeneration: number;
  activeUser: UserProfilePublicDto;
  z4TrustedContext: Z4TrustedSealContextDto | null;
  activeWorkspace: SessionWorkspaceDto;
  workspaces: SessionWorkspaceDto[];
  viewportRect: RectPxDto;
  stageRect: RectPxDto;
  taskbarRect: RectPxDto;
  z1TaskSpaceRect: RectPxDto | null;
  z1TaskSpaceVisible: boolean;
  layoutMode: WorkspaceLayoutModeKindDto;
  topHeightRatioPermille: number;
  splitWidthRatioPermille: number;
  windows: CompositorWindowDto[];
  activeZ3Overlays: CompositorZ3OverlayDto[];
  focusAndInputRoute: FocusAndInputRouteDto;
}

export interface CompositorInputDispatchInput {
  source: InputSourceKindDto;
  originKind: "trusted_shell" | "untrusted_surface" | "untrusted_application_surface";
  originSurfaceIdHex?: string | null;
  originApplicationIdHex?: string | null;
  targetKind:
    | "z2_surface"
    | "z3_shortcut"
    | "z3_system_shortcut"
    | "z4_decision"
    | "z4_security_action"
    | "z4_unlock"
    | "z4_session_unlock";
  targetWindowIdHex?: string | null;
  windowIdHex?: string | null;
  targetSurfaceIdHex?: string | null;
  surfaceIdHex?: string | null;
  targetSurfaceGeneration?: number | null;
  shortcut?: string | null;
  confirm?: boolean | null;
  key?: string | null;
  sessionGeneration: number;
  eventSessionGeneration?: number;
  nowMonoMs: number;
}

export interface InputRouteResultDto {
  accepted: boolean;
  routedPlane: string | null;
  targetWindowIdHex: string | null;
  targetSurfaceIdHex: string | null;
  rejectionCode: string | null;
  rejectionMessage: string | null;
  route: FocusAndInputRouteDto;
}
