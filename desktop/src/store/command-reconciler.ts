// ControlCommand Outcome Reconciler & Anti-Blind-Retry Guard ([CTRL-RECOVERY-001]).
// Authority: ADR-0025 Decision 2 & Decision 4, v0.5 §25.3 [CTRL-RECOVERY-001], [RCPT-PARTIAL-001].
// Prevents duplicate side effects when transport/IPC drops mid-flight by fencing target keys
// in `UNKNOWN_NEEDS_RECONCILE` until `getControlCommandStatus` resolves the authoritative outcome.

import {
  queryControlCommandStatus,
  submitControl,
  type StageDReadReceipt,
} from "../ipc";
import type {
  AnyControlActionInput,
  ControlScopeDto,
  OutcomeDto,
  StageDMutationOutcomeDto,
  TypedReceiptDto,
} from "../types";

export const BLIND_RETRY_FORBIDDEN_ON_UNKNOWN =
  "BLIND_RETRY_FORBIDDEN_ON_UNKNOWN" as const;

export const SAFE_TO_ISSUE_FRESH_COMMAND =
  "SAFE_TO_ISSUE_FRESH_COMMAND" as const;

export type TrackedCommandState =
  | "IN_FLIGHT"
  | "SUCCEEDED"
  | "REJECTED"
  | "UNKNOWN_NEEDS_RECONCILE";

export type ReconcileResolution =
  | "SUCCEEDED"
  | "REJECTED"
  | typeof SAFE_TO_ISSUE_FRESH_COMMAND
  | "UNKNOWN_NEEDS_RECONCILE";

export class BlindRetryForbiddenError extends Error {
  public readonly code = BLIND_RETRY_FORBIDDEN_ON_UNKNOWN;
  public readonly targetKey: string;
  public readonly blockingCommandIdHex: string;

  public constructor(targetKey: string, blockingCommandIdHex: string) {
    super(
      `${BLIND_RETRY_FORBIDDEN_ON_UNKNOWN}: target '${targetKey}' has unresolved command '${blockingCommandIdHex}' that must be reconciled via getControlCommandStatus before retrying`,
    );
    this.name = "BlindRetryForbiddenError";
    this.targetKey = targetKey;
    this.blockingCommandIdHex = blockingCommandIdHex;
  }
}

export interface TrackedCommandEntry<TAction = AnyControlActionInput> {
  readonly controlCommandIdHex: string;
  readonly targetKey: string;
  readonly action: TAction | null;
  readonly state: TrackedCommandState;
  readonly reconcileResolution: ReconcileResolution | null;
  readonly safeToIssueFreshCommand: boolean;
  readonly receiptHex: string | null;
  readonly receiptIdHex: string | null;
  readonly failureCode: string | null;
  readonly failureMessage: string | null;
  readonly createdAtMs: number;
  readonly updatedAtMs: number;
}

export interface AuthoritativeCommandStatusProbe {
  readonly state: string;
  readonly controlCommandIdHex?: string;
  readonly scope?: string;
  readonly targetIdHex?: string;
  readonly receiptIdHex?: string | null;
  readonly failureCode?: string | null;
  readonly failureRetry?: string | null;
  readonly failureSafeMessage?: string | null;
}

export type CommandStatusQueryResponse =
  | StageDReadReceipt<"command_status_queried">
  | AuthoritativeCommandStatusProbe;

export type DispatchCommandTransport<
  TAction = AnyControlActionInput,
  TOutcome = OutcomeDto | StageDMutationOutcomeDto,
> = (
  action: TAction,
  meta: {
    readonly controlCommandIdHex: string;
    readonly targetKey: string;
  },
) => Promise<TypedReceiptDto<TOutcome>>;

export type GetControlCommandStatusTransport = (
  controlCommandIdHex: string,
) => Promise<CommandStatusQueryResponse>;

export interface CommandReconcilerOptions<
  TAction = AnyControlActionInput,
  TOutcome = OutcomeDto | StageDMutationOutcomeDto,
> {
  readonly dispatchCommand?: DispatchCommandTransport<TAction, TOutcome>;
  readonly getControlCommandStatus?: GetControlCommandStatusTransport;
  readonly nowMs?: () => number;
}

export interface DispatchRequest<TAction = AnyControlActionInput> {
  readonly action: TAction;
  readonly targetKey?: string;
  readonly controlCommandIdHex?: string;
}

export type CommandDispatchResult<
  TOutcome = OutcomeDto | StageDMutationOutcomeDto,
> =
  | {
      readonly ok: true;
      readonly state: "SUCCEEDED";
      readonly controlCommandIdHex: string;
      readonly targetKey: string;
      readonly receipt: TypedReceiptDto<TOutcome>;
    }
  | {
      readonly ok: false;
      readonly state: "REJECTED";
      readonly code: string;
      readonly message: string;
      readonly controlCommandIdHex: string;
      readonly targetKey: string;
      readonly receipt?: TypedReceiptDto<TOutcome>;
    }
  | {
      readonly ok: false;
      readonly state: "UNKNOWN_NEEDS_RECONCILE";
      readonly code: "TRANSPORT_OUTCOME_UNKNOWN";
      readonly message: string;
      readonly controlCommandIdHex: string;
      readonly targetKey: string;
    }
  | {
      readonly ok: false;
      readonly state: "UNKNOWN_NEEDS_RECONCILE";
      readonly code: typeof BLIND_RETRY_FORBIDDEN_ON_UNKNOWN;
      readonly message: string;
      readonly controlCommandIdHex: string;
      readonly blockingCommandIdHex: string;
      readonly targetKey: string;
    };

export type BlindRetryAttemptResult =
  | {
      readonly ok: false;
      readonly accepted: false;
      readonly code: typeof BLIND_RETRY_FORBIDDEN_ON_UNKNOWN;
      readonly state: "UNKNOWN_NEEDS_RECONCILE";
      readonly targetKey: string;
      readonly blockingCommandIdHex: string;
      readonly message: string;
    }
  | {
      readonly ok: true;
      readonly accepted: true;
      readonly code: typeof SAFE_TO_ISSUE_FRESH_COMMAND;
      readonly targetKey: string;
    };

export interface CommandReconcileResult {
  readonly controlCommandIdHex: string;
  readonly targetKey: string;
  readonly state: TrackedCommandState;
  readonly resolution: ReconcileResolution;
  readonly safeToIssueFreshCommand: boolean;
  readonly receiptIdHex: string | null;
  readonly failureCode: string | null;
  readonly failureMessage: string | null;
}

function formatCounterAs32Hex(seq: number): string {
  const hex = Math.max(1, Math.trunc(seq)).toString(16);
  return hex.padStart(32, "0").slice(-32);
}

export function deriveLogicalTargetKey(action: unknown): string {
  if (typeof action !== "object" || action === null) {
    return "scope:unknown";
  }
  const rec = action as Record<string, unknown>;
  const actionKind = typeof rec.action === "string" ? rec.action : "command";
  const scope = typeof rec.scope === "string" ? rec.scope : "default";
  const target =
    typeof rec.targetIdHex === "string"
      ? rec.targetIdHex
      : typeof rec.planIdHex === "string"
        ? rec.planIdHex
        : typeof rec.processIdHex === "string"
          ? rec.processIdHex
          : typeof rec.topicIdHex === "string"
            ? rec.topicIdHex
            : typeof rec.attemptIdHex === "string"
              ? rec.attemptIdHex
              : typeof rec.operationIdHex === "string"
                ? rec.operationIdHex
                : typeof rec.candidateIdHex === "string"
                  ? rec.candidateIdHex
                  : typeof rec.scopeTargetIdHex === "string"
                    ? rec.scopeTargetIdHex
                    : "global";
  const verb = typeof rec.verb === "string" ? `:${rec.verb}` : "";
  return `${scope}:${actionKind}${verb}:${target}`;
}

function isOutcomeFailure(
  outcome: unknown,
): outcome is Extract<OutcomeDto, { kind: "failure" }> {
  return (
    typeof outcome === "object" &&
    outcome !== null &&
    (outcome as { kind?: unknown }).kind === "failure"
  );
}

function isTransportAmbiguousFailure(
  failure: Extract<OutcomeDto, { kind: "failure" }>,
): boolean {
  const code = failure.code.trim().toUpperCase();
  const retry = failure.retry.trim().toUpperCase();
  return (
    code === "IPC" ||
    code === "TIMEOUT" ||
    code === "DEADLINE_EXCEEDED" ||
    code === "EFFECT_UNKNOWN" ||
    retry === "RECONCILE_FIRST"
  );
}

function extractErrorMessage(err: unknown): string {
  if (err instanceof Error) {
    return err.message;
  }
  if (typeof err === "object" && err !== null) {
    const rec = err as { code?: unknown; message?: unknown };
    if (typeof rec.message === "string") {
      return typeof rec.code === "string"
        ? `${rec.code}: ${rec.message}`
        : rec.message;
    }
  }
  return String(err);
}

function isFullStatusReceipt(
  res: CommandStatusQueryResponse,
): res is StageDReadReceipt<"command_status_queried"> {
  return (
    typeof res === "object" &&
    res !== null &&
    "outcome" in res &&
    typeof (res as { outcome?: unknown }).outcome === "object" &&
    (res as { outcome?: unknown }).outcome !== null
  );
}

export class ControlCommandReconciler<
  TAction = AnyControlActionInput,
  TOutcome = OutcomeDto | StageDMutationOutcomeDto,
> {
  private readonly dispatchTransport: DispatchCommandTransport<TAction, TOutcome>;
  private readonly statusTransport: GetControlCommandStatusTransport;
  private readonly nowMs: () => number;
  private readonly entries = new Map<string, TrackedCommandEntry<TAction>>();
  private readonly aliasToCanonicalId = new Map<string, string>();
  private readonly unknownLockByTargetKey = new Map<string, string>();
  private sequenceCounter = 0;

  public constructor(
    options: CommandReconcilerOptions<TAction, TOutcome> = {},
  ) {
    const defaultSubmit = submitControl as unknown as (
      action: AnyControlActionInput,
    ) => Promise<TypedReceiptDto<OutcomeDto | StageDMutationOutcomeDto>>;
    this.dispatchTransport =
      options.dispatchCommand ??
      ((async (action: TAction) =>
        (await defaultSubmit(
          action as unknown as AnyControlActionInput,
        )) as unknown as TypedReceiptDto<TOutcome>) as DispatchCommandTransport<
        TAction,
        TOutcome
      >);
    this.statusTransport =
      options.getControlCommandStatus ?? queryControlCommandStatus;
    this.nowMs = options.nowMs ?? (() => Date.now());
  }

  private resolveCanonicalId(idHex: string): string {
    return this.aliasToCanonicalId.get(idHex) ?? idHex;
  }

  public getEntry(
    controlCommandIdHex: string,
  ): TrackedCommandEntry<TAction> | undefined {
    const canonical = this.resolveCanonicalId(controlCommandIdHex);
    return this.entries.get(canonical);
  }

  public listEntries(): readonly TrackedCommandEntry<TAction>[] {
    return [...this.entries.values()];
  }

  public getBlockingUnknownCommandId(targetKey: string): string | null {
    const cmdId = this.unknownLockByTargetKey.get(targetKey);
    if (cmdId === undefined) {
      return null;
    }
    const entry = this.entries.get(cmdId);
    if (entry === undefined || entry.state !== "UNKNOWN_NEEDS_RECONCILE") {
      this.unknownLockByTargetKey.delete(targetKey);
      return null;
    }
    return cmdId;
  }

  public isTargetBlockedByUnknown(targetKey: string): boolean {
    return this.getBlockingUnknownCommandId(targetKey) !== null;
  }

  public assertCanIssueForTarget(targetKey: string): void {
    const blockingId = this.getBlockingUnknownCommandId(targetKey);
    if (blockingId !== null) {
      throw new BlindRetryForbiddenError(targetKey, blockingId);
    }
  }

  public markUnknownNeedsReconcile(
    controlCommandIdHex: string,
    targetKey: string,
    failureMessage = "Transport disconnected before receipt confirmation",
    action: TAction | null = null,
  ): TrackedCommandEntry<TAction> {
    const now = this.nowMs();
    const existing = this.getEntry(controlCommandIdHex);
    const entry: TrackedCommandEntry<TAction> = {
      controlCommandIdHex,
      targetKey,
      action: action ?? existing?.action ?? null,
      state: "UNKNOWN_NEEDS_RECONCILE",
      reconcileResolution: "UNKNOWN_NEEDS_RECONCILE",
      safeToIssueFreshCommand: false,
      receiptHex: existing?.receiptHex ?? null,
      receiptIdHex: existing?.receiptIdHex ?? null,
      failureCode: "TRANSPORT_OUTCOME_UNKNOWN",
      failureMessage,
      createdAtMs: existing?.createdAtMs ?? now,
      updatedAtMs: now,
    };
    this.entries.set(controlCommandIdHex, entry);
    this.unknownLockByTargetKey.set(targetKey, controlCommandIdHex);
    return entry;
  }

  /**
   * Explicit blind-retry probe: refuses with `BLIND_RETRY_FORBIDDEN_ON_UNKNOWN`
   * whenever `targetKey` has an unresolved `UNKNOWN_NEEDS_RECONCILE` command,
   * NEVER invoking `dispatchCommand`.
   */
  public retryBlindly(targetKey: string): BlindRetryAttemptResult {
    const blockingId = this.getBlockingUnknownCommandId(targetKey);
    if (blockingId !== null) {
      return {
        ok: false,
        accepted: false,
        code: BLIND_RETRY_FORBIDDEN_ON_UNKNOWN,
        state: "UNKNOWN_NEEDS_RECONCILE",
        targetKey,
        blockingCommandIdHex: blockingId,
        message: `${BLIND_RETRY_FORBIDDEN_ON_UNKNOWN}: reconcile command ${blockingId} via getControlCommandStatus before retrying '${targetKey}'`,
      };
    }
    return {
      ok: true,
      accepted: true,
      code: SAFE_TO_ISSUE_FRESH_COMMAND,
      targetKey,
    };
  }

  public async dispatch(
    request: DispatchRequest<TAction>,
  ): Promise<CommandDispatchResult<TOutcome>> {
    const targetKey =
      request.targetKey ?? deriveLogicalTargetKey(request.action);
    const blockingId = this.getBlockingUnknownCommandId(targetKey);
    if (blockingId !== null) {
      return {
        ok: false,
        state: "UNKNOWN_NEEDS_RECONCILE",
        code: BLIND_RETRY_FORBIDDEN_ON_UNKNOWN,
        controlCommandIdHex: blockingId,
        blockingCommandIdHex: blockingId,
        targetKey,
        message: `${BLIND_RETRY_FORBIDDEN_ON_UNKNOWN}: target '${targetKey}' is fenced by unresolved command '${blockingId}'`,
      };
    }

    this.sequenceCounter += 1;
    const initialCmdId =
      request.controlCommandIdHex ?? formatCounterAs32Hex(this.sequenceCounter);
    const startedAt = this.nowMs();

    this.entries.set(initialCmdId, {
      controlCommandIdHex: initialCmdId,
      targetKey,
      action: request.action,
      state: "IN_FLIGHT",
      reconcileResolution: null,
      safeToIssueFreshCommand: false,
      receiptHex: null,
      receiptIdHex: null,
      failureCode: null,
      failureMessage: null,
      createdAtMs: startedAt,
      updatedAtMs: startedAt,
    });

    try {
      const receipt = await this.dispatchTransport(request.action, {
        controlCommandIdHex: initialCmdId,
        targetKey,
      });

      const canonicalId =
        typeof receipt.controlCommandIdHex === "string" &&
        receipt.controlCommandIdHex.length > 0
          ? receipt.controlCommandIdHex
          : initialCmdId;

      if (canonicalId !== initialCmdId) {
        this.entries.delete(initialCmdId);
        this.aliasToCanonicalId.set(initialCmdId, canonicalId);
      }

      const finishedAt = this.nowMs();

      if (isOutcomeFailure(receipt.outcome)) {
        if (isTransportAmbiguousFailure(receipt.outcome)) {
          this.markUnknownNeedsReconcile(
            canonicalId,
            targetKey,
            receipt.outcome.safeMessage,
            request.action,
          );
          return {
            ok: false,
            state: "UNKNOWN_NEEDS_RECONCILE",
            code: "TRANSPORT_OUTCOME_UNKNOWN",
            message: receipt.outcome.safeMessage,
            controlCommandIdHex: canonicalId,
            targetKey,
          };
        }

        const rejectedEntry: TrackedCommandEntry<TAction> = {
          controlCommandIdHex: canonicalId,
          targetKey,
          action: request.action,
          state: "REJECTED",
          reconcileResolution: "REJECTED",
          safeToIssueFreshCommand: true,
          receiptHex: receipt.receiptHex,
          receiptIdHex: null,
          failureCode: receipt.outcome.code,
          failureMessage: receipt.outcome.safeMessage,
          createdAtMs: startedAt,
          updatedAtMs: finishedAt,
        };
        this.entries.set(canonicalId, rejectedEntry);
        return {
          ok: false,
          state: "REJECTED",
          code: receipt.outcome.code,
          message: receipt.outcome.safeMessage,
          controlCommandIdHex: canonicalId,
          targetKey,
          receipt,
        };
      }

      const receiptIdHex =
        typeof (receipt.outcome as { receiptIdHex?: unknown }).receiptIdHex ===
        "string"
          ? ((receipt.outcome as { receiptIdHex: string }).receiptIdHex ?? null)
          : null;

      const succeededEntry: TrackedCommandEntry<TAction> = {
        controlCommandIdHex: canonicalId,
        targetKey,
        action: request.action,
        state: "SUCCEEDED",
        reconcileResolution: "SUCCEEDED",
        safeToIssueFreshCommand: true,
        receiptHex: receipt.receiptHex,
        receiptIdHex,
        failureCode: null,
        failureMessage: null,
        createdAtMs: startedAt,
        updatedAtMs: finishedAt,
      };
      this.entries.set(canonicalId, succeededEntry);
      return {
        ok: true,
        state: "SUCCEEDED",
        controlCommandIdHex: canonicalId,
        targetKey,
        receipt,
      };
    } catch (err: unknown) {
      const msg = extractErrorMessage(err);
      this.markUnknownNeedsReconcile(
        initialCmdId,
        targetKey,
        msg,
        request.action,
      );
      return {
        ok: false,
        state: "UNKNOWN_NEEDS_RECONCILE",
        code: "TRANSPORT_OUTCOME_UNKNOWN",
        message: msg,
        controlCommandIdHex: initialCmdId,
        targetKey,
      };
    }
  }

  /**
   * Queries authoritative `getControlCommandStatus(controlCommandIdHex)` to reconcile
   * an `UNKNOWN_NEEDS_RECONCILE` command:
   * - `"succeeded" | "committed"` -> `"SUCCEEDED"` (clears unknown lock)
   * - `"failed" | "rejected"` -> `"REJECTED"` (clears unknown lock)
   * - `"not_found"` -> clears unknown lock and marks `"SAFE_TO_ISSUE_FRESH_COMMAND"`
   * - `"pending" | "in_flight"` -> remains `"UNKNOWN_NEEDS_RECONCILE"` (lock stays active)
   */
  public async reconcile(
    controlCommandIdHex: string,
  ): Promise<CommandReconcileResult> {
    const canonicalId = this.resolveCanonicalId(controlCommandIdHex);
    const existing = this.entries.get(canonicalId);
    const targetKey = existing?.targetKey ?? `reconcile:${canonicalId}`;

    let rawState = "unknown";
    let receiptIdHex: string | null = existing?.receiptIdHex ?? null;
    let failureCode: string | null = null;
    let failureMessage: string | null = null;

    try {
      const response = await this.statusTransport(canonicalId);
      if (isFullStatusReceipt(response)) {
        const outcome = response.outcome;
        if (outcome.kind === "command_status_queried") {
          rawState = outcome.state.trim().toLowerCase();
          receiptIdHex = outcome.receiptIdHex ?? receiptIdHex;
          failureCode = outcome.failureCode;
          failureMessage = outcome.failureSafeMessage;
        } else if (outcome.kind === "failure") {
          if (outcome.code.trim().toUpperCase() === "NOT_FOUND") {
            rawState = "not_found";
          } else {
            rawState = "pending";
            failureCode = outcome.code;
            failureMessage = outcome.safeMessage;
          }
        } else {
          rawState = "pending";
        }
      } else {
        rawState = response.state.trim().toLowerCase();
        receiptIdHex = response.receiptIdHex ?? receiptIdHex;
        failureCode = response.failureCode ?? null;
        failureMessage = response.failureSafeMessage ?? null;
      }
    } catch (err: unknown) {
      const msg = extractErrorMessage(err);
      this.markUnknownNeedsReconcile(
        canonicalId,
        targetKey,
        msg,
        existing?.action ?? null,
      );
      return {
        controlCommandIdHex: canonicalId,
        targetKey,
        state: "UNKNOWN_NEEDS_RECONCILE",
        resolution: "UNKNOWN_NEEDS_RECONCILE",
        safeToIssueFreshCommand: false,
        receiptIdHex,
        failureCode: "TRANSPORT_OUTCOME_UNKNOWN",
        failureMessage: msg,
      };
    }

    const updatedAt = this.nowMs();
    const clearTargetLock = (): void => {
      if (this.unknownLockByTargetKey.get(targetKey) === canonicalId) {
        this.unknownLockByTargetKey.delete(targetKey);
      }
    };

    if (rawState === "succeeded" || rawState === "committed") {
      clearTargetLock();
      const next: TrackedCommandEntry<TAction> = {
        controlCommandIdHex: canonicalId,
        targetKey,
        action: existing?.action ?? null,
        state: "SUCCEEDED",
        reconcileResolution: "SUCCEEDED",
        safeToIssueFreshCommand: true,
        receiptHex: existing?.receiptHex ?? null,
        receiptIdHex,
        failureCode: null,
        failureMessage: null,
        createdAtMs: existing?.createdAtMs ?? updatedAt,
        updatedAtMs: updatedAt,
      };
      this.entries.set(canonicalId, next);
      return {
        controlCommandIdHex: canonicalId,
        targetKey,
        state: "SUCCEEDED",
        resolution: "SUCCEEDED",
        safeToIssueFreshCommand: true,
        receiptIdHex,
        failureCode: null,
        failureMessage: null,
      };
    }

    if (rawState === "failed" || rawState === "rejected") {
      clearTargetLock();
      const next: TrackedCommandEntry<TAction> = {
        controlCommandIdHex: canonicalId,
        targetKey,
        action: existing?.action ?? null,
        state: "REJECTED",
        reconcileResolution: "REJECTED",
        safeToIssueFreshCommand: true,
        receiptHex: existing?.receiptHex ?? null,
        receiptIdHex,
        failureCode: failureCode ?? "COMMAND_REJECTED",
        failureMessage: failureMessage ?? "Authoritative status reported rejected",
        createdAtMs: existing?.createdAtMs ?? updatedAt,
        updatedAtMs: updatedAt,
      };
      this.entries.set(canonicalId, next);
      return {
        controlCommandIdHex: canonicalId,
        targetKey,
        state: "REJECTED",
        resolution: "REJECTED",
        safeToIssueFreshCommand: true,
        receiptIdHex,
        failureCode: next.failureCode,
        failureMessage: next.failureMessage,
      };
    }

    if (rawState === "not_found") {
      clearTargetLock();
      const next: TrackedCommandEntry<TAction> = {
        controlCommandIdHex: canonicalId,
        targetKey,
        action: existing?.action ?? null,
        state: "REJECTED",
        reconcileResolution: SAFE_TO_ISSUE_FRESH_COMMAND,
        safeToIssueFreshCommand: true,
        receiptHex: existing?.receiptHex ?? null,
        receiptIdHex: null,
        failureCode: "NOT_FOUND",
        failureMessage:
          "Command never reached durable authority; safe to issue fresh command",
        createdAtMs: existing?.createdAtMs ?? updatedAt,
        updatedAtMs: updatedAt,
      };
      this.entries.set(canonicalId, next);
      return {
        controlCommandIdHex: canonicalId,
        targetKey,
        state: "REJECTED",
        resolution: SAFE_TO_ISSUE_FRESH_COMMAND,
        safeToIssueFreshCommand: true,
        receiptIdHex: null,
        failureCode: "NOT_FOUND",
        failureMessage: next.failureMessage,
      };
    }

    // "pending" | "in_flight" | any non-terminal state remains UNKNOWN_NEEDS_RECONCILE.
    this.markUnknownNeedsReconcile(
      canonicalId,
      targetKey,
      failureMessage ?? `Command remains ${rawState} on authority`,
      existing?.action ?? null,
    );
    return {
      controlCommandIdHex: canonicalId,
      targetKey,
      state: "UNKNOWN_NEEDS_RECONCILE",
      resolution: "UNKNOWN_NEEDS_RECONCILE",
      safeToIssueFreshCommand: false,
      receiptIdHex,
      failureCode,
      failureMessage,
    };
  }

  public clearAll(): void {
    this.entries.clear();
    this.aliasToCanonicalId.clear();
    this.unknownLockByTargetKey.clear();
  }
}

export function createCommandReconciler<
  TAction = AnyControlActionInput,
  TOutcome = OutcomeDto | StageDMutationOutcomeDto,
>(
  options?: CommandReconcilerOptions<TAction, TOutcome>,
): ControlCommandReconciler<TAction, TOutcome> {
  return new ControlCommandReconciler<TAction, TOutcome>(options);
}

/* ==========================================================================
   Batch Per-Target Outcome Summarizer ([RCPT-PARTIAL-001] & [CTRL-RECOVERY-001])
   ========================================================================== */

export type BatchTargetOutcomeStatus =
  | "succeeded"
  | "rejected"
  | "pending"
  | "unknown";

export interface BatchTargetOutcomeEntry {
  readonly targetIdHex: string;
  readonly targetLabel?: string;
  readonly scope?: ControlScopeDto | string;
  readonly state: TrackedCommandState | BatchTargetOutcomeStatus | string;
  readonly controlCommandIdHex?: string | null;
  readonly receiptIdHex?: string | null;
  readonly reason?: string | null;
}

export interface BatchTargetOutcomeRow {
  readonly targetIdHex: string;
  readonly targetLabel: string;
  readonly scope: string;
  readonly status: BatchTargetOutcomeStatus;
  readonly controlCommandIdHex: string | null;
  readonly receiptIdHex: string | null;
  readonly reason: string | null;
  readonly requiresReconcileBeforeRetry: boolean;
}

export interface BatchTargetOutcomeSummary {
  readonly total: number;
  readonly succeeded: number;
  readonly rejected: number;
  readonly pending: number;
  readonly unknown: number;
  readonly isPartial: boolean;
  readonly hasUnknownRequiringReconcile: boolean;
  readonly rows: readonly BatchTargetOutcomeRow[];
}

function classifyBatchTargetStatus(raw: string): BatchTargetOutcomeStatus {
  const norm = raw.trim().toLowerCase();
  if (norm === "succeeded" || norm === "committed" || norm === "completed") {
    return "succeeded";
  }
  if (norm === "rejected" || norm === "failed" || norm === "cancelled") {
    return "rejected";
  }
  if (norm === "in_flight" || norm === "pending" || norm === "running") {
    return "pending";
  }
  return "unknown";
}

export function summarizeBatchTargetOutcomes(
  entries: readonly BatchTargetOutcomeEntry[],
): BatchTargetOutcomeSummary {
  let succeeded = 0;
  let rejected = 0;
  let pending = 0;
  let unknown = 0;
  const rows: BatchTargetOutcomeRow[] = [];

  for (const item of entries) {
    const status = classifyBatchTargetStatus(item.state);
    if (status === "succeeded") {
      succeeded += 1;
    } else if (status === "rejected") {
      rejected += 1;
    } else if (status === "pending") {
      pending += 1;
    } else {
      unknown += 1;
    }

    rows.push({
      targetIdHex: item.targetIdHex,
      targetLabel: item.targetLabel ?? item.targetIdHex,
      scope: item.scope ?? "task",
      status,
      controlCommandIdHex: item.controlCommandIdHex ?? null,
      receiptIdHex: item.receiptIdHex ?? null,
      reason: item.reason ?? null,
      requiresReconcileBeforeRetry: status === "unknown",
    });
  }

  const total = rows.length;
  const isPartial = succeeded > 0 && succeeded < total;

  return {
    total,
    succeeded,
    rejected,
    pending,
    unknown,
    isPartial,
    hasUnknownRequiringReconcile: unknown > 0,
    rows,
  };
}
