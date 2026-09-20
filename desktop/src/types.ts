// 后端 serde DTO 的前端镜像(desktop/src-tauri/src/dto.rs 为唯一权威)。

export interface DesktopFailure {
  code: string;
  message: string;
}

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
  | { kind: "metrics_exported"; openmetricsText: string }
  | { kind: "failure"; code: string; retry: string; safeMessage: string }
  | { kind: "acknowledged"; receiptIdHex: string }
  | { kind: "resumed"; receiptIdHex: string }
  | { kind: "operation_paused"; receiptIdHex: string }
  | { kind: "operation_resumed"; receiptIdHex: string }
  | { kind: "operation_cancelled"; receiptIdHex: string }
  | { kind: "operation_killed"; receiptIdHex: string }
  | { kind: "operation_throttled"; receiptIdHex: string }
  | { kind: "operation_reclaimed"; receiptIdHex: string };

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
]);

/** W32-B 授权动作(serde tag 与 CLI operation 名一致;desktop/src-tauri/src/ipc.rs 为权威)。 */
export type ControlActionInput =
  | { action: "ack-recovery-alert"; planIdHex: string; expectedTotalFailures: number; reason: string }
  | { action: "ack-semantic-recovery-alert"; planIdHex: string; expectedTotalFailures: number; reason: string }
  | { action: "resume-semantic-recovery"; planIdHex: string; expectedTotalFailures: number; reason: string }
  | { action: "ack-resource-recovery-alert"; planIdHex: string; expectedTotalFailures: number; reason: string }
  | { action: "resume-resource-recovery"; planIdHex: string; expectedTotalFailures: number; reason: string }
  | { action: "pause-operation"; targetIdHex: string; expectedRevision: number; reason: string }
  | { action: "resume-operation"; targetIdHex: string; expectedRevision: number; reason: string }
  | { action: "cancel-operation"; targetIdHex: string; expectedRevision: number; reason: string }
  | { action: "kill-operation"; targetIdHex: string; expectedRevision: number; reason: string }
  | { action: "throttle-operation"; targetIdHex: string; expectedRevision: number; throttlePercent: number; reason: string }
  | { action: "reclaim-operation"; targetIdHex: string; expectedRevision: number; reason: string };

export interface ReceiptDto {
  controlCommandIdHex: string;
  correlationIdHex: string;
  /** ControlReceipt::to_bytes 的 hex——与 CLI `RECEIPT <hex>` 同一等价契约。 */
  receiptHex: string;
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
  source: "env" | "session" | "unset";
  platformSupported: boolean;
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
