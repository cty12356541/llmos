// Stage D Task Space (`L3`) — Temporal Proportional `TaskAttempt` Racing Swimlanes
// & `ExecutionFiber` Phase Duration Waterfall.
// Authority: Stage D Visual & UX Spec §3.2 (`[TASK-SPEC-001]`, `[TASK-PROVIDER-001]`,
// `[TASK-AGENT-001]`, `[TASK-ISO-005]`, `[CTRL-GRANULAR-001]`, `[RCPT-OUTCOME-001]`),
// Statutory Lock #4 (`TASK_ATTEMPT_NO_MANUAL_COMMIT`), Statutory Lock #5 (`OPERATION_EFFECT_UNKNOWN_NO_MANUAL_SUCCESS`).

import { renderReceiptAuditBarHtml } from "../../ui/receipt-audit-bar";
import {
  pruneVerbsAgainstStatutoryLock,
  renderStatutoryLockBadgeHtml,
  renderStatusIndicatorHtml,
  type StatutoryLockKind,
  type WorkstationLocale,
  type WorkstationStatusState,
} from "../../ui/status-indicator";
import {
  computeAttemptRaceModel,
  computeFiberWaterfallModel,
  renderAttemptRaceHtml,
  renderFiberWaterfallHtml,
  type FiberWaterfallInput,
  type TaskAttemptOutcome,
  type TaskAttemptTimingInput,
} from "../../ui/waterfall-bar";

export type AllowedTaskAttemptControlVerb =
  | "cancel_attempt"
  | "supersede_attempt"
  | "request_reverify";

export type ForbiddenTaskAttemptControlVerb =
  | "manual_commit"
  | "force_commit"
  | "mark_winner"
  | "override_commit";

export type TaskAttemptControlVerb =
  | AllowedTaskAttemptControlVerb
  | ForbiddenTaskAttemptControlVerb;

export const ALLOWED_TASK_ATTEMPT_CONTROL_VERBS: readonly AllowedTaskAttemptControlVerb[] = [
  "cancel_attempt",
  "supersede_attempt",
  "request_reverify",
];

export type IsolationEnforcementProfile =
  | "STRICT_SANDBOX"
  | "SHARED_PROCESS_SUPERVISOR"
  | "PROVIDER_EXTERNAL";

export interface FiberPhaseTimings {
  readonly activeCpuMs: number;
  readonly schedulerWaitMs: number;
  readonly externalWaitMs: number;
  readonly backpressureWaitMs: number;
  readonly suspendedMs: number;
}

export interface ExecutionFiberLaneProjection {
  readonly fiberId: string;
  readonly attemptId: string;
  readonly processId: string;
  readonly label: string;
  readonly sharedThreadLike: boolean;
  readonly timings: FiberPhaseTimings;
}

export type DurableOperationEffectState =
  | "COMMITTED"
  | "ROLLED_BACK"
  | "EFFECT_UNKNOWN"
  | "PENDING";

export interface DurableOperationForensicProjection {
  readonly operationId: string;
  readonly attemptId: string;
  readonly targetResourceUri: string;
  readonly effectState: DurableOperationEffectState;
  readonly idempotencyKeyHex: string;
  readonly fencingTokenEpoch: number;
  readonly receiptHex: string;
}

export interface CellPlacementForensicProjection {
  readonly cellIdHex: string;
  readonly leaseEpoch: number;
  readonly nodeBootGeneration: number;
  readonly placementMode: "LOCAL_SINGLE_NODE" | "MULTI_CELL_DISTRIBUTED";
  readonly fencingTokenHex: string;
}

export interface TaskAttemptSwimlaneProjection {
  readonly attemptId: string;
  readonly taskNodeId: string;
  readonly attemptIndex: number;
  readonly label: string;
  readonly state: WorkstationStatusState;
  readonly inputSnapshotDigestHex: string;
  readonly isolationDomainIdHex: string;
  readonly isolationEnforcementProfile: IsolationEnforcementProfile;
  readonly providerIsolationUnverified: boolean;
  readonly startedAtOffsetMs: number;
  readonly durationMs: number;
  readonly casWinner: boolean;
  readonly superseded: boolean;
  readonly retryFenceEpoch: number;
  readonly fibers: readonly ExecutionFiberLaneProjection[];
  readonly operations: readonly DurableOperationForensicProjection[];
  readonly cellPlacement: CellPlacementForensicProjection;
  readonly receiptHex: string;
}

export interface TaskAttemptSwimlanesSnapshot {
  readonly taskId: string;
  readonly taskNodeId: string;
  readonly planRevision: number;
  readonly winningAttemptId: string | null;
  readonly taskHeadCasEpoch: number;
  readonly attempts: readonly TaskAttemptSwimlaneProjection[];
}

export interface TaskAttemptControlRejection {
  readonly ok: false;
  readonly errorCode: "FORBIDDEN_MANUAL_ATTEMPT_COMMIT" | "ATTEMPT_NOT_FOUND";
  readonly statutoryLock: StatutoryLockKind | null;
  readonly attemptId: string;
  readonly rejectedVerb: string;
  readonly messageZh: string;
  readonly messageEn: string;
}

export interface TaskAttemptControlSuccess {
  readonly ok: true;
  readonly attemptId: string;
  readonly executedVerb: AllowedTaskAttemptControlVerb;
  readonly updatedSnapshot: TaskAttemptSwimlanesSnapshot;
  readonly receiptHex: string;
}

export type TaskAttemptControlOutcome =
  | TaskAttemptControlSuccess
  | TaskAttemptControlRejection;

function escapeHtml(value: string): string {
  return value
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

function deterministicAttemptControlReceiptHex(
  attemptId: string,
  verb: string,
  epoch: number,
): string {
  const seed = `attempt_ctrl:${attemptId}:${verb}:epoch${epoch}`;
  let h1 = 0x811c9dc5;
  let h2 = 0x01000193;
  for (let i = 0; i < seed.length; i += 1) {
    const ch = seed.charCodeAt(i);
    h1 ^= ch;
    h1 = Math.imul(h1, 0x01000193) >>> 0;
    h2 ^= (ch + i) & 0xff;
    h2 = Math.imul(h2, 0x85ebca6b) >>> 0;
  }
  const p1 = h1.toString(16).padStart(8, "0");
  const p2 = h2.toString(16).padStart(8, "0");
  return `${p1}${p2}${p1}${p2}${p1}${p2}${p1}${p2}`;
}

/**
 * Statutory Lock #4 (`TASK_ATTEMPT_NO_MANUAL_COMMIT`):
 * Prunes any manual commit verbs (`commit`, `force_commit`, `mark_winner`, `override_commit`, `manual_commit`)
 * from `TaskAttempt` action toolbars.
 */
export function pruneTaskAttemptControlVerbs(
  candidateVerbs: readonly string[],
): readonly AllowedTaskAttemptControlVerb[] {
  const mappedToCanonical = candidateVerbs.map((v) => {
    const lower = v.trim().toLowerCase();
    if (lower === "cancel_attempt" || lower === "cancel") {
      return "Cancel";
    }
    if (lower === "supersede_attempt" || lower === "supersede") {
      return "Supersede";
    }
    if (lower === "request_reverify" || lower === "requestreverify") {
      return "RequestReverify";
    }
    if (
      lower === "manual_commit" ||
      lower === "commit" ||
      lower === "force_commit" ||
      lower === "mark_winner" ||
      lower === "override_commit"
    ) {
      return "ManualCommit";
    }
    return v;
  });
  const pruned = pruneVerbsAgainstStatutoryLock(
    mappedToCanonical,
    "TASK_ATTEMPT_NO_MANUAL_COMMIT",
  );
  const out: AllowedTaskAttemptControlVerb[] = [];
  for (const item of pruned) {
    if (item === "Cancel") {
      out.push("cancel_attempt");
    } else if (item === "Supersede") {
      out.push("supersede_attempt");
    } else if (item === "RequestReverify") {
      out.push("request_reverify");
    }
  }
  return out;
}

/**
 * Statutory Lock #4 (`TASK_ATTEMPT_NO_MANUAL_COMMIT` / `FORBIDDEN_MANUAL_ATTEMPT_COMMIT`):
 * Deterministically rejects any attempt to manually mark a `TaskAttempt` as committed/winner,
 * while permitting `cancel_attempt`, `supersede_attempt`, and `request_reverify`.
 */
export function executeTaskAttemptControlAction(
  snapshot: TaskAttemptSwimlanesSnapshot,
  attemptId: string,
  verb: TaskAttemptControlVerb | string,
): TaskAttemptControlOutcome {
  const normalized = verb.trim().toLowerCase();
  if (
    normalized === "manual_commit" ||
    normalized === "commit" ||
    normalized === "force_commit" ||
    normalized === "mark_winner" ||
    normalized === "override_commit"
  ) {
    return {
      ok: false,
      errorCode: "FORBIDDEN_MANUAL_ATTEMPT_COMMIT",
      statutoryLock: "TASK_ATTEMPT_NO_MANUAL_COMMIT",
      attemptId,
      rejectedVerb: verb,
      messageZh:
        "胜出提交由内核 TaskHead CAS 原子裁决，禁止手动标记提交 (FORBIDDEN_MANUAL_ATTEMPT_COMMIT)",
      messageEn:
        "Winner commit is adjudicated atomically by kernel TaskHead CAS; manual commit is forbidden (FORBIDDEN_MANUAL_ATTEMPT_COMMIT)",
    };
  }

  const target = snapshot.attempts.find((a) => a.attemptId === attemptId);
  if (target === undefined) {
    return {
      ok: false,
      errorCode: "ATTEMPT_NOT_FOUND",
      statutoryLock: null,
      attemptId,
      rejectedVerb: verb,
      messageZh: `未找到执行尝试 ${attemptId}`,
      messageEn: `TaskAttempt ${attemptId} was not found`,
    };
  }

  if (
    normalized !== "cancel_attempt" &&
    normalized !== "supersede_attempt" &&
    normalized !== "request_reverify"
  ) {
    return {
      ok: false,
      errorCode: "FORBIDDEN_MANUAL_ATTEMPT_COMMIT",
      statutoryLock: "TASK_ATTEMPT_NO_MANUAL_COMMIT",
      attemptId,
      rejectedVerb: verb,
      messageZh: `不允许对 TaskAttempt 执行未授权动作 ${verb}`,
      messageEn: `Unauthorized TaskAttempt control verb ${verb}`,
    };
  }

  const allowedVerb = normalized as AllowedTaskAttemptControlVerb;
  const updatedAttempts: TaskAttemptSwimlaneProjection[] = snapshot.attempts.map(
    (a): TaskAttemptSwimlaneProjection => {
      if (a.attemptId !== attemptId) {
        return a;
      }
      if (allowedVerb === "cancel_attempt") {
        return {
          ...a,
          state: "CANCELLED",
          retryFenceEpoch: a.retryFenceEpoch + 1,
        };
      }
      if (allowedVerb === "supersede_attempt") {
        return {
          ...a,
          state: "SUPERSEDED",
          superseded: true,
          casWinner: false,
          retryFenceEpoch: a.retryFenceEpoch + 1,
        };
      }
      return {
        ...a,
        state: "RECOVERING",
      };
    },
  );

  const updatedSnapshot: TaskAttemptSwimlanesSnapshot = {
    ...snapshot,
    taskHeadCasEpoch: snapshot.taskHeadCasEpoch + 1,
    attempts: updatedAttempts,
  };

  return {
    ok: true,
    attemptId,
    executedVerb: allowedVerb,
    updatedSnapshot,
    receiptHex: deterministicAttemptControlReceiptHex(
      attemptId,
      allowedVerb,
      updatedSnapshot.taskHeadCasEpoch,
    ),
  };
}

export function attemptManualCommitOverride(
  snapshot: TaskAttemptSwimlanesSnapshot,
  attemptId: string,
): TaskAttemptControlRejection {
  const outcome = executeTaskAttemptControlAction(
    snapshot,
    attemptId,
    "manual_commit",
  );
  if (outcome.ok) {
    throw new Error("Unreachable: manual_commit must always be rejected");
  }
  return outcome;
}

function mapAttemptToRaceOutcome(
  attempt: TaskAttemptSwimlaneProjection,
): TaskAttemptOutcome {
  if (attempt.casWinner) {
    return "CAS_WINNER";
  }
  if (attempt.superseded || attempt.state === "SUPERSEDED") {
    return "SUPERSEDED";
  }
  if (attempt.state === "FAILED") {
    return "FAILED";
  }
  if (attempt.state === "CANCELLED" || attempt.state === "CANCELLING") {
    return "CANCELLED";
  }
  return "RUNNING";
}

function buildTimingInputs(
  snapshot: TaskAttemptSwimlanesSnapshot,
): readonly TaskAttemptTimingInput[] {
  return snapshot.attempts.map((a) => ({
    attemptNumber: a.attemptIndex,
    attemptIdHex: a.attemptId,
    startMs: a.startedAtOffsetMs,
    endMs: a.startedAtOffsetMs + a.durationMs,
    outcome: mapAttemptToRaceOutcome(a),
    isolationDomainIdHex: a.isolationDomainIdHex,
    retryFenceEpoch: a.retryFenceEpoch,
    providerIsolationUnverified: a.providerIsolationUnverified,
  }));
}

export interface ComputedTaskAttemptSwimlanesModel {
  readonly snapshot: TaskAttemptSwimlanesSnapshot;
  readonly raceModel: ReturnType<typeof computeAttemptRaceModel>;
  readonly fiberModelsByAttemptId: ReadonlyMap<
    string,
    readonly ReturnType<typeof computeFiberWaterfallModel>[]
  >;
}

export function computeTaskAttemptSwimlanesModel(
  snapshot: TaskAttemptSwimlanesSnapshot,
  locale: WorkstationLocale = "zh-CN",
): ComputedTaskAttemptSwimlanesModel {
  const timingInputs = buildTimingInputs(snapshot);
  const raceModel = computeAttemptRaceModel(timingInputs);
  const fiberModelsByAttemptId = new Map<
    string,
    readonly ReturnType<typeof computeFiberWaterfallModel>[]
  >();

  for (const attempt of snapshot.attempts) {
    const fiberModels = attempt.fibers.map((f) =>
      computeFiberWaterfallModel({
        activeCpuMs: f.timings.activeCpuMs,
        schedulerWaitMs: f.timings.schedulerWaitMs,
        externalWaitMs: f.timings.externalWaitMs,
        backpressureWaitMs: f.timings.backpressureWaitMs,
        suspendedMs: f.timings.suspendedMs,
        sharedProcessFaultDomain: f.sharedThreadLike,
        disclosure: "L3",
        locale,
      }),
    );
    fiberModelsByAttemptId.set(attempt.attemptId, fiberModels);
  }

  return {
    snapshot,
    raceModel,
    fiberModelsByAttemptId,
  };
}

export interface RenderTaskAttemptSwimlanesOptions {
  readonly snapshot: TaskAttemptSwimlanesSnapshot;
  readonly locale?: WorkstationLocale;
  readonly activeRejection?: TaskAttemptControlRejection | null;
}

export function renderTaskAttemptSwimlanesHtml(
  options: RenderTaskAttemptSwimlanesOptions,
): string {
  const locale = options.locale ?? "zh-CN";
  const timingInputs = buildTimingInputs(options.snapshot);
  const raceHtml = renderAttemptRaceHtml({
    attempts: timingInputs,
    disclosure: "L3",
    locale,
  });
  const attemptLockBadgeHtml = renderStatutoryLockBadgeHtml({
    kind: "TASK_ATTEMPT_NO_MANUAL_COMMIT",
    locale,
    disclosure: "L3",
  });

  const attemptCardsHtml = options.snapshot.attempts
    .map((attempt) => {
      const statusHtml = renderStatusIndicatorHtml({
        state: attempt.state,
        locale,
        disclosure: "L3",
      });

      const winnerBadge = attempt.casWinner
        ? `<span class="nlos-attempt-card__badge" data-badge-kind="CAS_WINNER">CAS_WINNER</span>`
        : attempt.superseded
          ? `<span class="nlos-attempt-card__badge" data-badge-kind="SUPERSEDED">SUPERSEDED</span>`
          : "";

      const providerBadge = attempt.providerIsolationUnverified
        ? `<span class="nlos-attempt-card__badge" data-badge-kind="PROVIDER_ISOLATION_UNVERIFIED">PROVIDER_ISOLATION_UNVERIFIED</span>`
        : "";

      const fibersHtml = attempt.fibers
        .map((rawFiber) => {
          const fiberInput: FiberWaterfallInput = {
            activeCpuMs: rawFiber.timings.activeCpuMs,
            schedulerWaitMs: rawFiber.timings.schedulerWaitMs,
            externalWaitMs: rawFiber.timings.externalWaitMs,
            backpressureWaitMs: rawFiber.timings.backpressureWaitMs,
            suspendedMs: rawFiber.timings.suspendedMs,
            sharedProcessFaultDomain: rawFiber.sharedThreadLike,
            disclosure: "L3",
            locale,
          };
          return (
            `<div class="nlos-attempt-card__fiber" data-fiber-id="${escapeHtml(rawFiber.fiberId)}" data-process-id="${escapeHtml(rawFiber.processId)}" data-shared-thread-like="${rawFiber.sharedThreadLike ? "true" : "false"}">` +
            renderFiberWaterfallHtml(fiberInput) +
            `</div>`
          );
        })
        .join("");

      const opsHtml = attempt.operations
        .map((op) => {
          const effectUnknownLockHtml =
            op.effectState === "EFFECT_UNKNOWN"
              ? renderStatutoryLockBadgeHtml({
                  kind: "OPERATION_EFFECT_UNKNOWN_NO_MANUAL_SUCCESS",
                  locale,
                  disclosure: "L3",
                })
              : "";
          return (
            `<div class="nlos-attempt-card__operation" data-operation-id="${escapeHtml(op.operationId)}" data-effect-state="${op.effectState}">` +
            `<span class="nlos-attempt-card__op-id" data-mono="true">${escapeHtml(op.operationId)}</span>` +
            `<span class="nlos-attempt-card__op-uri" data-mono="true">${escapeHtml(op.targetResourceUri)}</span>` +
            `<span class="nlos-attempt-card__op-state" data-mono="true">${escapeHtml(op.effectState)}</span>` +
            `<span class="nlos-attempt-card__op-fence" data-mono="true" data-tabular-nums="true">fence_epoch=${op.fencingTokenEpoch}</span>` +
            effectUnknownLockHtml +
            `</div>`
          );
        })
        .join("");

      const allowedVerbs = pruneTaskAttemptControlVerbs([
        ...ALLOWED_TASK_ATTEMPT_CONTROL_VERBS,
        "manual_commit",
        "force_commit",
      ]);

      const controlsHtml = allowedVerbs
        .map((verb) => {
          const label =
            verb === "cancel_attempt"
              ? locale === "en-US"
                ? "Cancel"
                : "取消尝试 (Cancel)"
              : verb === "supersede_attempt"
                ? locale === "en-US"
                  ? "Supersede"
                  : "标记淘汰 (Supersede)"
                : locale === "en-US"
                  ? "RequestReverify"
                  : "请求重验 (RequestReverify)";
          return `<button type="button" class="nlos-attempt-card__btn" data-attempt-control-verb="${verb}" data-attempt-id="${escapeHtml(attempt.attemptId)}">${escapeHtml(label)}</button>`;
        })
        .join("");

      const receiptBarHtml = renderReceiptAuditBarHtml({
        disclosure: "L3",
        controlCommandIdHex: attempt.isolationDomainIdHex,
        receiptHex: attempt.receiptHex,
        correlationIdHex: attempt.inputSnapshotDigestHex,
        cellId: attempt.cellPlacement.cellIdHex,
        leaseEpoch: attempt.cellPlacement.leaseEpoch,
        nodeBootGeneration: attempt.cellPlacement.nodeBootGeneration,
        locale,
      });

      return (
        `<article class="nlos-attempt-card" data-attempt-id="${escapeHtml(attempt.attemptId)}" data-cas-winner="${attempt.casWinner ? "true" : "false"}" data-superseded="${attempt.superseded ? "true" : "false"}" data-provider-isolation-unverified="${attempt.providerIsolationUnverified ? "true" : "false"}" data-statutory-lock="TASK_ATTEMPT_NO_MANUAL_COMMIT">` +
        `<header class="nlos-attempt-card__header">` +
        `<div class="nlos-attempt-card__identity">` +
        statusHtml +
        `<span class="nlos-attempt-card__title">${escapeHtml(attempt.label)}</span>` +
        `<span class="nlos-attempt-card__id" data-mono="true">${escapeHtml(attempt.attemptId)}</span>` +
        winnerBadge +
        providerBadge +
        `</div>` +
        `<div class="nlos-attempt-card__meta" data-mono="true" data-tabular-nums="true">` +
        `<span>input_snapshot=${escapeHtml(attempt.inputSnapshotDigestHex.slice(0, 12))}…</span>` +
        `<span>iso_domain=${escapeHtml(attempt.isolationDomainIdHex.slice(0, 12))}… (${escapeHtml(attempt.isolationEnforcementProfile)})</span>` +
        `<span>retry_fence_epoch=${attempt.retryFenceEpoch}</span>` +
        `<span>cell=${escapeHtml(attempt.cellPlacement.cellIdHex)} (${escapeHtml(attempt.cellPlacement.placementMode)}, lease=${attempt.cellPlacement.leaseEpoch}, boot_gen=${attempt.cellPlacement.nodeBootGeneration})</span>` +
        `</div>` +
        `</header>` +
        `<div class="nlos-attempt-card__controls" data-manual-commit-omitted="true">` +
        controlsHtml +
        attemptLockBadgeHtml +
        `</div>` +
        `<div class="nlos-attempt-card__fibers">${fibersHtml}</div>` +
        `<div class="nlos-attempt-card__operations">${opsHtml}</div>` +
        receiptBarHtml +
        `</article>`
      );
    })
    .join("");

  const rejectionHtml = options.activeRejection
    ? `<div class="nlos-attempt-lanes__rejection" role="alert" data-error-code="${options.activeRejection.errorCode}"${options.activeRejection.statutoryLock ? ` data-statutory-lock="${options.activeRejection.statutoryLock}"` : ""}>` +
      `${escapeHtml(
        locale === "en-US"
          ? options.activeRejection.messageEn
          : options.activeRejection.messageZh,
      )}` +
      `</div>`
    : "";

  return (
    `<section class="nlos-attempt-swimlanes" data-surface-id="task-attempt-swimlanes" data-task-id="${escapeHtml(options.snapshot.taskId)}" data-task-node-id="${escapeHtml(options.snapshot.taskNodeId)}" data-cas-epoch="${options.snapshot.taskHeadCasEpoch}">` +
    `<header class="nlos-attempt-swimlanes__header">` +
    `<span class="nlos-attempt-swimlanes__title">${locale === "en-US" ? "TaskAttempt Racing Swimlanes & ExecutionFiber Phase Waterfall" : "TaskAttempt 竞速泳道与 ExecutionFiber 四段耗时瀑布"}</span>` +
    `<span class="nlos-attempt-swimlanes__cas-meta" data-mono="true" data-tabular-nums="true">TaskHead CAS epoch=${options.snapshot.taskHeadCasEpoch} · winner=${escapeHtml(options.snapshot.winningAttemptId ?? "NONE")}</span>` +
    `</header>` +
    rejectionHtml +
    raceHtml +
    `<div class="nlos-attempt-swimlanes__cards">${attemptCardsHtml}</div>` +
    `</section>`
  );
}

export function createTaskAttemptSwimlanesElement(
  doc: Document,
  options: RenderTaskAttemptSwimlanesOptions,
): HTMLElement {
  const wrapper = doc.createElement("div");
  wrapper.innerHTML = renderTaskAttemptSwimlanesHtml(options);
  return (wrapper.firstElementChild as HTMLElement | null) ?? wrapper;
}
