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
  | { kind: "failure"; code: string; retry: string; safeMessage: string };

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
  source: "env" | "session" | "unset";
  platformSupported: boolean;
}

export interface ParityDto {
  operation: string;
  guiReceiptHex: string;
  cliReceiptHex: string | null;
  cliExitCode: number | null;
  matched: boolean;
  cliStderr: string | null;
}
