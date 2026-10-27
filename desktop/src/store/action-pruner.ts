// Headlamp-Style Capability & State-Machine Guarded Action Pruner ([UI-PARITY-001]).
// Authority: ADR-0025 Decision 4, Stage D Visual & UX Spec §2A.3, §2A.4.
// Enforces the Three Laws of Display-Control Symmetry across all 12 NLOS resource domains:
//   Law 1 (No Blind Control): Mutating verbs require authoritative 32-hex ID + CAS revision/generation.
//   Law 2 (No Orphan Display): Every record yields allowed verbs OR an explicit zero-jargon read-only reason.
//   Law 3 (Statutory Non-Tamperable Locks): Physically omits forbidden verbs on the 5+1 statutory locks.

import { SEARCH_NON_AUTHORITATIVE_RESTRICTIONS_ALL } from "../types";
import {
  ALL_STATUTORY_LOCK_KINDS,
  getStatutoryLockDescriptor,
  pruneVerbsAgainstStatutoryLock,
  type DisclosureLevel,
  type StatutoryLockKind,
  type WorkstationLocale,
} from "../ui/status-indicator";

export {
  ALL_STATUTORY_LOCK_KINDS,
  getStatutoryLockDescriptor,
  pruneVerbsAgainstStatutoryLock,
  type StatutoryLockKind,
};

export type ActionPrunerDomain =
  | "system-device"
  | "workspace"
  | "application"
  | "task"
  | "task-group"
  | "task-node"
  | "task-attempt"
  | "process"
  | "execution-fiber"
  | "resource-group"
  | "topic"
  | "operation"
  | "cell"
  | "search"
  | "notification";

export const ALL_ACTION_PRUNER_DOMAINS: readonly ActionPrunerDomain[] = [
  "system-device",
  "workspace",
  "application",
  "task",
  "task-group",
  "task-node",
  "task-attempt",
  "process",
  "execution-fiber",
  "resource-group",
  "topic",
  "operation",
  "cell",
  "search",
  "notification",
];

export interface ReadBackingToken {
  readonly targetIdHex: string;
  readonly expectedGenerationOrRevision: number;
}

export interface ActionPrunerInput {
  readonly domain: ActionPrunerDomain;
  readonly state: string;
  readonly callerCapabilities: ReadonlySet<string> | readonly string[];
  readonly readBacking?: ReadBackingToken | null;
  readonly undoClass?: "reversible" | "compensatable" | "irreversible";
  readonly searchRestrictionsBits?: number;
  readonly actionTokenConsumed?: boolean;
  readonly hasActiveTasksForUninstall?: boolean;
  readonly statutoryLocks?: StatutoryLockKind | readonly StatutoryLockKind[];
  readonly locale?: WorkstationLocale;
  readonly disclosure?: DisclosureLevel;
}

export interface ActionPrunerResult {
  readonly allowedVerbs: readonly string[];
  readonly statutoryLock: StatutoryLockKind | null;
  readonly readOnlyReason: string | null;
  readonly requiresTwoStepArmingVerbs: readonly string[];
  readonly requiresBlastRadiusPreviewVerbs: readonly string[];
}

const VALID_32_HEX_RE = /^[0-9a-fA-F]{32}$/;

/** Read-only navigation / inspection verbs that do not mutate authoritative state. */
const NON_MUTATING_VERBS: ReadonlySet<string> = new Set([
  "JumpToAuthoritativeObject",
  "ViewEffectProof",
  "InspectFenceStatus",
]);

/** High-consequence destructive or cross-cell verbs requiring 5s two-step arming + 350ms guard. */
const TWO_STEP_ARMING_VERBS: ReadonlySet<string> = new Set([
  "Kill",
  "Purge",
  "DisableApplication",
  "UninstallApplication",
  "ResetDevice",
  "RequestProcessMigration",
]);

/** Cascading or structural verbs requiring `+ / ~ / -` blast-radius diff preview ([CTRL-HIER-001]). */
const BLAST_RADIUS_PREVIEW_VERBS: ReadonlySet<string> = new Set([
  "Pause",
  "Cancel",
  "Kill",
  "Purge",
  "DisableApplication",
  "UninstallApplication",
  "ResetDevice",
  "RequestProcessMigration",
  "ArchiveWorkspace",
  "Seal",
  "DeleteNode",
  "ReplaceNode",
  "RewireEdge",
  "MergeNode",
  "Throttle",
  "Reclaim",
  "Close",
]);

/** Exclusive statutory locks whose whitelist strictly bounds every candidate verb. */
const EXCLUSIVE_STATUTORY_LOCKS: readonly StatutoryLockKind[] = [
  "SEARCH_0X1F_READ_ONLY",
  "QUARANTINED_LEASE_FENCE",
  "NODE_COMPLETED_OR_ACTIVE_LOCKED",
  "OPERATION_EFFECT_UNKNOWN_NO_MANUAL_SUCCESS",
  "TASK_ATTEMPT_NO_MANUAL_COMMIT",
];

type ReadOnlyReasonKey =
  | "NO_CALLER_CAPABILITY"
  | "MISSING_READ_BACKING"
  | "INSUFFICIENT_WRITE_CAPABILITY"
  | "SEARCH_RESTRICTIONS_VIOLATION"
  | "SESSION_LOCKED_OR_CLOSED"
  | "WORKSPACE_CLOSED_OR_ARCHIVED"
  | "APP_ACTIVE_TASKS_BLOCK_UNINSTALL"
  | "APP_STALE_GENERATION"
  | "APP_UNINSTALLED"
  | "TASK_FINALIZING"
  | "TASK_ISOLATED"
  | "GROUP_TERMINAL"
  | "NODE_TERMINAL"
  | "PROCESS_TERMINATED_OR_FENCED"
  | "FIBER_FINISHED"
  | "RESOURCE_SETTLED"
  | "TOPIC_CLOSED"
  | "OPERATION_FINISHED"
  | "CELL_OFFLINE_LOCAL_READY"
  | "NOTIFICATION_TOKEN_CONSUMED"
  | "STATE_READ_ONLY";

const LOCALIZED_REASONS: Readonly<
  Record<ReadOnlyReasonKey, Readonly<Record<WorkstationLocale, string>>>
> = {
  NO_CALLER_CAPABILITY: {
    "zh-CN": "当前会话未授予此项查看或管理权限",
    "en-US": "Current session does not hold permission for this item",
  },
  MISSING_READ_BACKING: {
    "zh-CN": "需先读取最新记录状态后才能执行修改操作",
    "en-US": "Refresh the latest record status before performing changes",
  },
  INSUFFICIENT_WRITE_CAPABILITY: {
    "zh-CN": "当前权限仅支持只读查阅，不包含修改权限",
    "en-US": "Read-only access; current permissions do not allow modifications",
  },
  SEARCH_RESTRICTIONS_VIOLATION: {
    "zh-CN": "搜索结果安全校验未通过，已停止展示操作入口",
    "en-US": "Search preview failed safety validation; actions are disabled",
  },
  SESSION_LOCKED_OR_CLOSED: {
    "zh-CN": "桌面会话已锁定或关闭，请先解锁后操作",
    "en-US": "Desktop session is locked or closed; unlock before acting",
  },
  WORKSPACE_CLOSED_OR_ARCHIVED: {
    "zh-CN": "该工作区已归档或关闭，仅支持只读查阅历史内容",
    "en-US": "Workspace is archived or closed; historical items are read-only",
  },
  APP_ACTIVE_TASKS_BLOCK_UNINSTALL: {
    "zh-CN": "该应用仍有正在运行的后台任务，完成后方可卸载",
    "en-US": "Application has active background tasks; finish them before uninstalling",
  },
  APP_STALE_GENERATION: {
    "zh-CN": "应用已更新至新版本，旧窗口已停止接收输入",
    "en-US": "Application updated to a newer version; old window input is disabled",
  },
  APP_UNINSTALLED: {
    "zh-CN": "应用已卸载，历史记录仅供查阅",
    "en-US": "Application is uninstalled; record is read-only",
  },
  TASK_FINALIZING: {
    "zh-CN": "任务正在完成收尾处理，暂时锁定更改",
    "en-US": "Task is finalizing; changes are temporarily locked",
  },
  TASK_ISOLATED: {
    "zh-CN": "任务处于受控隔离状态，等待安全核实完成",
    "en-US": "Task is isolated under safety review and is read-only",
  },
  GROUP_TERMINAL: {
    "zh-CN": "任务分组已结束，仅供只读查阅",
    "en-US": "Task group has concluded and is read-only",
  },
  NODE_TERMINAL: {
    "zh-CN": "该步骤已结束，流程结构只读锁定",
    "en-US": "Step has concluded; workflow structure is locked read-only",
  },
  PROCESS_TERMINATED_OR_FENCED: {
    "zh-CN": "运行实例已结束或处于安全隔离态，不可恢复执行",
    "en-US": "Process has exited or is isolated and cannot be resumed",
  },
  FIBER_FINISHED: {
    "zh-CN": "执行流已结束，仅供查阅耗时详情",
    "en-US": "Execution unit has finished; timing details are read-only",
  },
  RESOURCE_SETTLED: {
    "zh-CN": "资源使用记录已结清，仅供查阅",
    "en-US": "Resource usage record is settled and read-only",
  },
  TOPIC_CLOSED: {
    "zh-CN": "消息通道已关闭，不可再次写入",
    "en-US": "Message channel is closed and read-only",
  },
  OPERATION_FINISHED: {
    "zh-CN": "外部操作已完结，记录只读",
    "en-US": "External operation has completed and is read-only",
  },
  CELL_OFFLINE_LOCAL_READY: {
    "zh-CN": "远端节点等待网络连接（本地功能正常可用）",
    "en-US": "Remote node waiting for network (local workstation ready)",
  },
  NOTIFICATION_TOKEN_CONSUMED: {
    "zh-CN": "通知快捷操作已执行或已归档，不可重复触发",
    "en-US": "Notification quick action already used or archived; cannot replay",
  },
  STATE_READ_ONLY: {
    "zh-CN": "当前记录状态仅供只读查阅",
    "en-US": "Record is read-only in its current state",
  },
};

/** Validates Law 1 (`[UI-PARITY-001]` No Blind Control): 32-hex target ID + non-negative integer CAS token. */
export function isValidReadBackingToken(
  readBacking: ReadBackingToken | null | undefined,
): readBacking is ReadBackingToken {
  if (readBacking === undefined || readBacking === null) {
    return false;
  }
  if (typeof readBacking.targetIdHex !== "string") {
    return false;
  }
  const trimmed = readBacking.targetIdHex.trim();
  if (!VALID_32_HEX_RE.test(trimmed)) {
    return false;
  }
  return (
    Number.isInteger(readBacking.expectedGenerationOrRevision) &&
    readBacking.expectedGenerationOrRevision >= 0
  );
}

function normalizeCapabilities(
  raw: ReadonlySet<string> | readonly string[],
): ReadonlySet<string> {
  const out = new Set<string>();
  for (const cap of raw) {
    const trimmed = cap.trim();
    // Domain 1 statutory rule ([CTRL-PRIORITY-001]): never allow self-granting SYSTEM_CRITICAL.
    if (trimmed.length > 0 && trimmed !== "SYSTEM_CRITICAL") {
      out.add(trimmed);
    }
  }
  return out;
}

function normalizeExplicitLocks(
  locks: StatutoryLockKind | readonly StatutoryLockKind[] | undefined,
): Set<StatutoryLockKind> {
  if (locks === undefined) {
    return new Set();
  }
  if (typeof locks === "string") {
    return new Set([locks]);
  }
  return new Set(locks);
}

function dedupeVerbs(verbs: readonly string[]): string[] {
  const seen = new Set<string>();
  const out: string[] = [];
  for (const v of verbs) {
    if (!seen.has(v)) {
      seen.add(v);
      out.push(v);
    }
  }
  return out;
}

function formatStatutoryReason(
  lockKind: StatutoryLockKind,
  locale: WorkstationLocale,
  disclosure: DisclosureLevel,
): string {
  const desc = getStatutoryLockDescriptor(lockKind);
  const base = desc.labels[locale];
  if (disclosure === "L3") {
    return `${base} [${desc.l3DiagnosticTag}]`;
  }
  return base;
}

/**
 * Computes exact allowed `ControlCommand` verbs, statutory non-tamperable lock badge,
 * localized zero-jargon read-only reason, two-step arming subset, and blast-radius preview subset.
 */
export function computeAllowedVerbs(input: ActionPrunerInput): ActionPrunerResult {
  const locale: WorkstationLocale = input.locale ?? "zh-CN";
  const disclosure: DisclosureLevel = input.disclosure ?? "L1";
  const normState = input.state.trim().toUpperCase();
  const caps = normalizeCapabilities(input.callerCapabilities);
  const hasAnyCap = caps.size > 0;
  const canWrite = caps.has("control:write");
  const canDestructive = caps.has("control:destructive");
  const canManageApp = caps.has("application:manage");
  const canEditPlan = caps.has("plan:edit");
  const canMigrateCell = caps.has("cell:migrate");
  const hasReadBacking = isValidReadBackingToken(input.readBacking);

  const activeLocks = normalizeExplicitLocks(input.statutoryLocks);

  // Auto-infer statutory non-tamperable locks from domain, state, undoClass, and 0x1F mask.
  if (input.domain === "search") {
    activeLocks.add("SEARCH_0X1F_READ_ONLY");
  }
  if (input.undoClass === "irreversible") {
    activeLocks.add("IRREVERSIBLE_NO_UNDO");
  }
  if (
    input.domain === "task-node" &&
    (normState === "COMPLETED" || normState === "ACTIVE")
  ) {
    activeLocks.add("NODE_COMPLETED_OR_ACTIVE_LOCKED");
  }
  if (input.domain === "task-attempt") {
    activeLocks.add("TASK_ATTEMPT_NO_MANUAL_COMMIT");
  }
  if (input.domain === "operation" && normState === "EFFECT_UNKNOWN") {
    activeLocks.add("OPERATION_EFFECT_UNKNOWN_NO_MANUAL_SUCCESS");
  }
  if (input.domain === "cell" && normState === "QUARANTINED") {
    activeLocks.add("QUARANTINED_LEASE_FENCE");
  }

  // Primary statutory lock (exclusive locks take precedence over additive IRREVERSIBLE_NO_UNDO).
  let primaryStatutoryLock: StatutoryLockKind | null = null;
  for (const kind of EXCLUSIVE_STATUTORY_LOCKS) {
    if (activeLocks.has(kind)) {
      primaryStatutoryLock = kind;
      break;
    }
  }
  if (primaryStatutoryLock === null && activeLocks.has("IRREVERSIBLE_NO_UNDO")) {
    primaryStatutoryLock = "IRREVERSIBLE_NO_UNDO";
  }

  // Fail-closed check for Search 0x1F non-authoritative restriction bits ([CONS-CTRL-001]).
  if (
    activeLocks.has("SEARCH_0X1F_READ_ONLY") &&
    input.searchRestrictionsBits !== undefined &&
    input.searchRestrictionsBits !== SEARCH_NON_AUTHORITATIVE_RESTRICTIONS_ALL
  ) {
    return {
      allowedVerbs: [],
      statutoryLock: "SEARCH_0X1F_READ_ONLY",
      readOnlyReason: LOCALIZED_REASONS.SEARCH_RESTRICTIONS_VIOLATION[locale],
      requiresTwoStepArmingVerbs: [],
      requiresBlastRadiusPreviewVerbs: [],
    };
  }

  if (!hasAnyCap) {
    return {
      allowedVerbs: [],
      statutoryLock: primaryStatutoryLock,
      readOnlyReason:
        primaryStatutoryLock !== null
          ? formatStatutoryReason(primaryStatutoryLock, locale, disclosure)
          : LOCALIZED_REASONS.NO_CALLER_CAPABILITY[locale],
      requiresTwoStepArmingVerbs: [],
      requiresBlastRadiusPreviewVerbs: [],
    };
  }

  // Step 1: Compute domain + state + capability candidate verbs BEFORE readBacking & statutory filters.
  const candidateMutating: string[] = [];
  const candidateReadOnly: string[] = [];
  let domainReasonKey: ReadOnlyReasonKey = "STATE_READ_ONLY";
  let stateWouldAllowWritesWithCapability = false;

  switch (input.domain) {
    case "system-device": {
      if (normState === "LOCKED" || normState === "CLOSED") {
        domainReasonKey = "SESSION_LOCKED_OR_CLOSED";
      } else {
        stateWouldAllowWritesWithCapability = true;
        if (canWrite) {
          if (
            normState === "ESCALATED" ||
            normState === "FAULTED" ||
            normState === "BACKINGOFF" ||
            normState === "RECOVERING" ||
            normState === "ALERTING"
          ) {
            candidateMutating.push("AcknowledgeRecoveryAlert", "ResumeRecovery");
          }
          candidateMutating.push(
            "SwitchPerformanceProfile",
            "PauseBackgroundTier",
            "RevokeDevicePermission",
          );
        }
        if (canDestructive) {
          candidateMutating.push("ResetDevice");
        }
      }
      break;
    }

    case "workspace": {
      if (normState === "CLOSED") {
        domainReasonKey = "WORKSPACE_CLOSED_OR_ARCHIVED";
      } else if (normState === "ARCHIVED") {
        domainReasonKey = "WORKSPACE_CLOSED_OR_ARCHIVED";
        stateWouldAllowWritesWithCapability = true;
        if (canWrite) {
          candidateMutating.push("ActivateWorkspace");
        }
      } else {
        stateWouldAllowWritesWithCapability = true;
        if (canWrite) {
          candidateMutating.push(
            "ActivateWorkspace",
            "UpdateWorkspaceLimit",
            "UpdateWorkspaceSettings",
            "ArchiveWorkspace",
          );
        }
      }
      break;
    }

    case "application": {
      if (normState === "STALE_GENERATION") {
        domainReasonKey = "APP_STALE_GENERATION";
      } else if (normState === "UNINSTALLED") {
        domainReasonKey = "APP_UNINSTALLED";
      } else if (normState === "DISABLED") {
        stateWouldAllowWritesWithCapability = true;
        if (canWrite || canManageApp) {
          candidateMutating.push("Activate");
        }
        if (canManageApp || canDestructive) {
          if (!input.hasActiveTasksForUninstall) {
            candidateMutating.push("UninstallApplication");
          } else {
            domainReasonKey = "APP_ACTIVE_TASKS_BLOCK_UNINSTALL";
          }
        }
      } else {
        // INSTALLED / ACTIVE / READY
        stateWouldAllowWritesWithCapability = true;
        if (canWrite || canManageApp) {
          candidateMutating.push(
            "Launch",
            "Activate",
            "Terminate",
            "ToggleBackground",
          );
        }
        if (canManageApp || canDestructive) {
          candidateMutating.push("DisableApplication");
          if (!input.hasActiveTasksForUninstall) {
            candidateMutating.push("UninstallApplication");
          } else {
            domainReasonKey = "APP_ACTIVE_TASKS_BLOCK_UNINSTALL";
          }
        }
      }
      break;
    }

    case "task": {
      if (normState === "COMPLETING" || normState === "CANCELLING") {
        domainReasonKey = "TASK_FINALIZING";
      } else if (normState === "QUARANTINED") {
        domainReasonKey = "TASK_ISOLATED";
      } else if (normState === "WAITING_CONSENT") {
        stateWouldAllowWritesWithCapability = true;
        if (canWrite) {
          candidateMutating.push("ReviewConsent", "Cancel");
        }
      } else if (normState === "WAITING_INPUT") {
        stateWouldAllowWritesWithCapability = true;
        if (canWrite) {
          candidateMutating.push("ProvideInput", "Pause", "Cancel");
        }
      } else if (normState === "DRAFT" || normState === "PROPOSED") {
        stateWouldAllowWritesWithCapability = true;
        if (canWrite) {
          candidateMutating.push("SubmitProposal", "Cancel");
        }
      } else if (normState === "SUSPENDED") {
        stateWouldAllowWritesWithCapability = true;
        if (canWrite) {
          candidateMutating.push("Resume", "Cancel");
        }
      } else if (normState === "DISPUTED") {
        stateWouldAllowWritesWithCapability = true;
        if (canWrite) {
          candidateMutating.push("ReviewDispute", "Retry", "Cancel");
        }
      } else if (normState === "PARTIAL") {
        stateWouldAllowWritesWithCapability = true;
        if (canWrite) {
          candidateMutating.push("Retry", "Refine");
          if (
            input.undoClass === "reversible" ||
            input.undoClass === "compensatable"
          ) {
            candidateMutating.push("Undo");
          }
        }
        if (input.undoClass !== undefined) {
          candidateReadOnly.push("ViewEffectProof");
        }
      } else if (normState === "FAILED" || normState === "CANCELLED") {
        stateWouldAllowWritesWithCapability = true;
        if (canWrite) {
          candidateMutating.push("Retry", "Refine");
        }
      } else if (normState === "COMPLETED") {
        stateWouldAllowWritesWithCapability = true;
        if (canWrite) {
          candidateMutating.push("Refine");
          if (
            input.undoClass === "reversible" ||
            input.undoClass === "compensatable"
          ) {
            candidateMutating.push("Undo");
          }
        }
        candidateReadOnly.push("ViewEffectProof");
      } else {
        // AUTHORIZED / RUNNING / WAITING_RESOURCE / WAITING_NETWORK / RECOVERING
        stateWouldAllowWritesWithCapability = true;
        if (canWrite) {
          candidateMutating.push("Pause", "Cancel");
        }
      }
      break;
    }

    case "task-group": {
      if (normState === "COMPLETED" || normState === "CANCELLED") {
        domainReasonKey = "GROUP_TERMINAL";
      } else if (normState === "SEALED") {
        stateWouldAllowWritesWithCapability = true;
        if (canWrite) {
          candidateMutating.push("ReopenNewGeneration");
        }
      } else if (normState === "SUSPENDED" || normState === "PAUSED") {
        stateWouldAllowWritesWithCapability = true;
        if (canWrite) {
          candidateMutating.push("Resume", "Seal", "SetFanoutLimit");
        }
      } else {
        stateWouldAllowWritesWithCapability = true;
        if (canWrite) {
          candidateMutating.push("Pause", "Seal", "SetFanoutLimit");
        }
      }
      break;
    }

    case "task-node": {
      if (normState === "COMPLETED" || normState === "ACTIVE") {
        // Handled by NODE_COMPLETED_OR_ACTIVE_LOCKED statutory lock.
        domainReasonKey = "NODE_TERMINAL";
      } else if (
        normState === "FAILED" ||
        normState === "CANCELLED" ||
        normState === "SKIPPED"
      ) {
        domainReasonKey = "NODE_TERMINAL";
      } else {
        // DECLARED / BLOCKED_DEPENDENCY / ELIGIBLE
        stateWouldAllowWritesWithCapability = true;
        if (canWrite || canEditPlan) {
          candidateMutating.push("PauseNode", "Materialize", "Evict");
        }
        if (canEditPlan) {
          candidateMutating.push(
            "DeleteNode",
            "ReplaceNode",
            "RewireEdge",
            "MergeNode",
          );
        }
      }
      break;
    }

    case "task-attempt": {
      stateWouldAllowWritesWithCapability = true;
      if (canWrite) {
        if (
          normState === "DISPUTED" ||
          normState === "PARTIAL"
        ) {
          candidateMutating.push("Supersede", "RequestReverify");
        } else if (
          normState === "COMPLETED" ||
          normState === "COMMITTED" ||
          normState === "FAILED" ||
          normState === "SUPERSEDED" ||
          normState === "CANCELLED"
        ) {
          candidateMutating.push("RequestReverify");
        } else {
          candidateMutating.push("Cancel", "Supersede", "RequestReverify");
        }
      }
      break;
    }

    case "process": {
      if (
        normState === "FENCED" ||
        normState === "TERMINATED" ||
        normState === "EXITED" ||
        normState === "CLOSED"
      ) {
        domainReasonKey = "PROCESS_TERMINATED_OR_FENCED";
      } else if (normState === "SUSPENDED" || normState === "PAUSED") {
        stateWouldAllowWritesWithCapability = true;
        if (canWrite) {
          candidateMutating.push("Resume");
        }
        if (canDestructive) {
          candidateMutating.push("Kill");
        }
      } else {
        stateWouldAllowWritesWithCapability = true;
        if (canWrite) {
          candidateMutating.push("Suspend", "UpdateQos");
        }
        if (canDestructive) {
          candidateMutating.push("Kill");
        }
      }
      break;
    }

    case "execution-fiber": {
      if (
        normState === "COMPLETED" ||
        normState === "CANCELLED" ||
        normState === "FAILED" ||
        normState === "TERMINATED"
      ) {
        domainReasonKey = "FIBER_FINISHED";
      } else {
        stateWouldAllowWritesWithCapability = true;
        if (canWrite) {
          candidateMutating.push("Cancel");
        }
      }
      break;
    }

    case "resource-group": {
      if (
        normState === "SETTLED" ||
        normState === "CLOSED" ||
        normState === "RELEASED"
      ) {
        domainReasonKey = "RESOURCE_SETTLED";
      } else {
        stateWouldAllowWritesWithCapability = true;
        if (canWrite) {
          candidateMutating.push(
            "Throttle",
            "Reclaim",
            "Evict",
            "AdjustResourceCeiling",
          );
        }
      }
      break;
    }

    case "topic": {
      if (normState === "CLOSED") {
        domainReasonKey = "TOPIC_CLOSED";
      } else if (normState === "PAUSED" || normState === "SUSPENDED") {
        stateWouldAllowWritesWithCapability = true;
        if (canWrite) {
          candidateMutating.push("Resume", "Throttle", "Drain", "Close");
        }
        if (canDestructive) {
          candidateMutating.push("Purge");
        }
      } else {
        stateWouldAllowWritesWithCapability = true;
        if (canWrite) {
          candidateMutating.push("Pause", "Throttle", "Drain", "Close");
        }
        if (canDestructive) {
          candidateMutating.push("Purge");
        }
      }
      break;
    }

    case "operation": {
      if (normState === "EFFECT_UNKNOWN") {
        stateWouldAllowWritesWithCapability = true;
        if (canWrite) {
          candidateMutating.push("Reconcile", "Compensate");
        }
      } else if (
        normState === "SUCCEEDED" ||
        normState === "COMPLETED" ||
        normState === "CANCELLED" ||
        normState === "COMPENSATED"
      ) {
        domainReasonKey = "OPERATION_FINISHED";
        if (
          canWrite &&
          (input.undoClass === "reversible" || input.undoClass === "compensatable")
        ) {
          stateWouldAllowWritesWithCapability = true;
          candidateMutating.push("Undo");
        }
        if (input.undoClass !== undefined) {
          candidateReadOnly.push("ViewEffectProof");
        }
      } else if (normState === "PAUSED" || normState === "SUSPENDED") {
        stateWouldAllowWritesWithCapability = true;
        if (canWrite) {
          candidateMutating.push("Resume", "Cancel");
        }
        if (canDestructive) {
          candidateMutating.push("ResetDevice");
        }
      } else if (normState === "FAILED" || normState === "RETRYABLE") {
        stateWouldAllowWritesWithCapability = true;
        if (canWrite) {
          candidateMutating.push("RetryIfSafe", "Compensate");
        }
        if (canDestructive) {
          candidateMutating.push("ResetDevice");
        }
      } else {
        stateWouldAllowWritesWithCapability = true;
        if (canWrite) {
          candidateMutating.push("Pause", "Cancel");
        }
        if (canDestructive) {
          candidateMutating.push("ResetDevice");
        }
      }
      break;
    }

    case "cell": {
      if (normState === "QUARANTINED") {
        candidateReadOnly.push("InspectFenceStatus");
      } else if (
        normState === "OFFLINE" ||
        normState === "UNREACHABLE" ||
        normState === "PARTITIONED"
      ) {
        domainReasonKey = "CELL_OFFLINE_LOCAL_READY";
      } else {
        stateWouldAllowWritesWithCapability = true;
        if (canMigrateCell) {
          candidateMutating.push("RequestProcessMigration");
        }
        if (canWrite) {
          candidateMutating.push("AuditPartitionAutonomy");
        }
      }
      break;
    }

    case "search": {
      candidateReadOnly.push("JumpToAuthoritativeObject");
      break;
    }

    case "notification": {
      if (
        input.actionTokenConsumed === true ||
        normState === "CONSUMED" ||
        normState === "EXPIRED" ||
        normState === "ARCHIVED"
      ) {
        domainReasonKey = "NOTIFICATION_TOKEN_CONSUMED";
      } else {
        stateWouldAllowWritesWithCapability = true;
        if (canWrite) {
          if (normState !== "READ") {
            candidateMutating.push("ExecuteNotificationAction");
          }
          candidateMutating.push("AcknowledgeNotification");
        }
      }
      break;
    }
  }

  // Step 2: Enforce Law 1 (No Blind Control) — prune ALL mutating verbs if readBacking is missing/invalid.
  const prunedDueToMissingReadBacking =
    candidateMutating.length > 0 && !hasReadBacking;
  const survivingMutating = hasReadBacking ? candidateMutating : [];

  // Step 3: Apply IRREVERSIBLE_NO_UNDO (additive lock: physically omits Undo, ensures ViewEffectProof).
  if (activeLocks.has("IRREVERSIBLE_NO_UNDO")) {
    const receiptCandidates = pruneVerbsAgainstStatutoryLock(
      ["Undo", "ViewEffectProof"],
      "IRREVERSIBLE_NO_UNDO",
    );
    candidateReadOnly.push(...receiptCandidates);
  }

  let mergedVerbs = dedupeVerbs([
    ...survivingMutating.filter(
      (verb) => !(activeLocks.has("IRREVERSIBLE_NO_UNDO") && verb === "Undo"),
    ),
    ...candidateReadOnly,
  ]);

  // Step 4: Enforce exclusive statutory non-tamperable locks ([UI-PARITY-001] Law 3).
  for (const exclusiveLock of EXCLUSIVE_STATUTORY_LOCKS) {
    if (activeLocks.has(exclusiveLock)) {
      if (exclusiveLock === "SEARCH_0X1F_READ_ONLY" && mergedVerbs.length === 0) {
        mergedVerbs = ["JumpToAuthoritativeObject"];
      } else if (
        exclusiveLock === "QUARANTINED_LEASE_FENCE" &&
        mergedVerbs.length === 0
      ) {
        mergedVerbs = ["InspectFenceStatus"];
      }
      mergedVerbs = [...pruneVerbsAgainstStatutoryLock(mergedVerbs, exclusiveLock)];
    }
  }

  // Step 5: Compute write verb presence & Law 2 (No Orphan Display) readOnlyReason.
  const hasMutatingVerb = mergedVerbs.some((v) => !NON_MUTATING_VERBS.has(v));
  let readOnlyReason: string | null = null;

  if (!hasMutatingVerb) {
    if (primaryStatutoryLock !== null && !prunedDueToMissingReadBacking) {
      readOnlyReason = formatStatutoryReason(
        primaryStatutoryLock,
        locale,
        disclosure,
      );
    } else if (prunedDueToMissingReadBacking) {
      readOnlyReason = LOCALIZED_REASONS.MISSING_READ_BACKING[locale];
    } else if (
      stateWouldAllowWritesWithCapability &&
      candidateMutating.length === 0 &&
      domainReasonKey === "STATE_READ_ONLY"
    ) {
      readOnlyReason = LOCALIZED_REASONS.INSUFFICIENT_WRITE_CAPABILITY[locale];
    } else if (primaryStatutoryLock !== null) {
      readOnlyReason = formatStatutoryReason(
        primaryStatutoryLock,
        locale,
        disclosure,
      );
    } else {
      readOnlyReason = LOCALIZED_REASONS[domainReasonKey][locale];
    }
  }

  const requiresTwoStepArmingVerbs = mergedVerbs.filter((v) =>
    TWO_STEP_ARMING_VERBS.has(v),
  );
  const requiresBlastRadiusPreviewVerbs = mergedVerbs.filter((v) =>
    BLAST_RADIUS_PREVIEW_VERBS.has(v),
  );

  return {
    allowedVerbs: mergedVerbs,
    statutoryLock: primaryStatutoryLock,
    readOnlyReason,
    requiresTwoStepArmingVerbs,
    requiresBlastRadiusPreviewVerbs,
  };
}
