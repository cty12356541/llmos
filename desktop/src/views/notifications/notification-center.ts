/**
 * Stage D (`D-NOTIFY-SEARCH-OFFLINE` / `W72-D-NOTIFY-SEARCH`)
 * Notification Center (`Z3 = 400`) & Cryptographic One-Shot `ActionToken` Redemption.
 *
 * Aligns with:
 * - `06-架构设计总纲-v0.5.md` §16.3.4 (`IPC-D8` `ListNotifications` / `InspectNotification`), §26.1–§26.4
 * - `ADR-0025` Decision 1 (`Z3 = 400` flyout), Decision 2 (`L0..L3` progressive disclosure & action pruning),
 *   Decision 3 (Solid `Graphite / Paper` surfaces, zero blur filters)
 * - `2026-10-26-stage-d-desktop-visual-and-ux-design.md` §0.1 (`ANTI-AI-01..07`), §3.5
 */

import type { WorkstationCompositor } from "../../compositor/workstation-compositor";
import { computeAllowedVerbs } from "../../store/action-pruner";
import { renderIconSvg } from "../../ui/icons";
import {
  CANONICAL_TASK_STATES,
  type CanonicalTaskState,
  type DisclosureLevel,
  renderStatusIndicatorHtml,
  type WorkstationLocale,
} from "../../ui/status-indicator";

export const Z3_NOTIFICATION_CENTER_Z_INDEX = 400 as const;
export const MAX_NOTIFICATION_PAGE_LIMIT = 64 as const;

export type NotificationCategory =
  | "TASK_TRANSITION"
  | "APPROVAL_REQUIRED"
  | "TASK_MILESTONE"
  | "ANOMALY_ALERT"
  | "SYSTEM_POSTURE";

export type NotificationSeverity = "INFO" | "WARNING" | "CRITICAL";

export type NotificationErrorCode =
  | "ACTION_TOKEN_ALREADY_CONSUMED"
  | "ACTION_TOKEN_EXPIRED"
  | "ACTION_TOKEN_APP_MISMATCH"
  | "ACTION_TOKEN_PRINCIPAL_MISMATCH"
  | "ACTION_TOKEN_TASK_MISMATCH"
  | "ACTION_TOKEN_INVALID_PROOF"
  | "ACTION_TOKEN_NOT_FOUND"
  | "INVALID_ARGUMENT";

export class NotificationCenterError extends Error {
  public readonly code: NotificationErrorCode;

  public constructor(code: NotificationErrorCode, message: string) {
    super(`[${code}] ${message}`);
    this.name = "NotificationCenterError";
    this.code = code;
  }
}

export interface NotificationActionTokenView {
  readonly tokenIdHex: string;
  readonly actionId: string;
  readonly labelZh: string;
  readonly labelEn: string;
  readonly boundNotificationIdHex: string;
  readonly boundAppIdHex: string;
  readonly boundTaskIdHex: string;
  readonly boundPrincipalIdHex: string;
  readonly expiryMonoMs: number;
  consumed: boolean;
  consumedAtMonoMs?: number;
  redemptionReceiptDigestHex?: string;
  readonly verifierCommitmentHex: string;
}

export interface NotificationL3Diagnostics {
  readonly topicIdHex: string;
  readonly subscriberKeyHex: string;
  readonly verifierCommitmentHex: string;
  readonly sequence: number;
  readonly pruneWatermarkSequence: number;
  readonly retractedEventCount: number;
  readonly deliveryAttempt: number;
}

export interface NotificationFeedItem {
  readonly notificationIdHex: string;
  readonly sequence: number;
  readonly taskIdHex: string;
  readonly appIdHex: string;
  readonly appDisplayNameZh: string;
  readonly appDisplayNameEn: string;
  taskState: CanonicalTaskState;
  readonly category: NotificationCategory;
  readonly severity: NotificationSeverity;
  readonly titleZh: string;
  readonly titleEn: string;
  readonly summaryZh: string;
  readonly summaryEn: string;
  stateLabelZh: string;
  stateLabelEn: string;
  readonly createdAtMonoMs: number;
  unread: boolean;
  dismissed: boolean;
  actionToken?: NotificationActionTokenView;
  allowedVerbs: readonly string[];
  readonly l3Diagnostics: NotificationL3Diagnostics;
}

export interface ListNotificationFeedRequest {
  readonly cursorSequence?: number;
  readonly limit?: number;
  readonly unreadOnly?: boolean;
  readonly categoryFilter?: NotificationCategory;
  readonly includeDismissed?: boolean;
}

export interface NotificationFeedPage {
  readonly items: readonly NotificationFeedItem[];
  readonly nextCursorSequence?: number;
  readonly hasMore: boolean;
  readonly totalUnreadCount: number;
  readonly totalActiveCount: number;
  readonly ackWatermarkSequence: number;
  readonly pruneWatermarkSequence: number;
  readonly retractedEventCount: number;
}

export interface RedeemNotificationActionTokenRequest {
  readonly notificationIdHex: string;
  readonly tokenIdHex: string;
  readonly actionId: string;
  readonly callerAppIdHex: string;
  readonly callerPrincipalIdHex: string;
  readonly taskIdHex: string;
  readonly nowMonoMs: number;
  readonly presentedCommitmentHex?: string;
}

export interface RedeemNotificationActionTokenReceipt {
  readonly notificationIdHex: string;
  readonly tokenIdHex: string;
  readonly actionId: string;
  readonly taskIdHex: string;
  readonly resultingTaskState: CanonicalTaskState;
  readonly redemptionReceiptDigestHex: string;
  readonly redeemedAtMonoMs: number;
  readonly remainingAllowedVerbs: readonly string[];
  readonly updatedUnreadCount: number;
}

export interface DismissNotificationItemRequest {
  readonly notificationIdHex: string;
  readonly nowMonoMs: number;
}

export interface DismissNotificationItemReceipt {
  readonly notificationIdHex: string;
  readonly dismissed: boolean;
  readonly ackWatermarkSequence: number;
  readonly updatedUnreadCount: number;
}

export interface NotificationSecurityAuditCounters {
  actionTokenRedemptionsSucceeded: number;
  actionTokenReplayRejections: number;
  actionTokenExpiredRejections: number;
  actionTokenAppMismatchRejections: number;
  actionTokenPrincipalMismatchRejections: number;
  actionTokenTaskMismatchRejections: number;
  actionTokenInvalidProofRejections: number;
}

/**
 * Self-contained deterministic 256-bit hex digest helper (8x32-bit FNV/Sip-style mixing)
 * for synchronous browser/Node commitment verification without async WebCrypto overhead.
 */
export function computeDeterministicDigest64Hex(domain: string, parts: readonly string[]): string {
  const seeds = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a,
    0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
  ];
  const payload = `${domain}|${parts.map((p) => `${p.length}:${p}`).join("|")}`;
  for (let i = 0; i < payload.length; i += 1) {
    const code = payload.charCodeAt(i);
    for (let s = 0; s < 8; s += 1) {
      let v = seeds[s]! ^ (code + i * 31 + s * 131);
      v = Math.imul(v ^ (v >>> 16), 0x85ebca6b);
      v = Math.imul(v ^ (v >>> 13), 0xc2b2ae35);
      seeds[s] = (v ^ (v >>> 16)) >>> 0;
    }
  }
  return seeds.map((w) => w.toString(16).padStart(8, "0")).join("");
}

export function deriveDeterministicId32Hex(tag: string, index: number): string {
  return computeDeterministicDigest64Hex("LLMOS_STAGE_D_W72_ID32", [tag, String(index)]).slice(
    0,
    32,
  );
}

export function computeActionTokenCommitmentHex(params: {
  readonly notificationIdHex: string;
  readonly tokenIdHex: string;
  readonly actionId: string;
  readonly appIdHex: string;
  readonly taskIdHex: string;
  readonly principalIdHex: string;
  readonly expiryMonoMs: number;
}): string {
  return computeDeterministicDigest64Hex("LLMOS_STAGE_D_NOTIFY_ACTION_TOKEN_COMMIT_V1", [
    params.notificationIdHex.toLowerCase(),
    params.tokenIdHex.toLowerCase(),
    params.actionId,
    params.appIdHex.toLowerCase(),
    params.taskIdHex.toLowerCase(),
    params.principalIdHex.toLowerCase(),
    String(params.expiryMonoMs),
  ]);
}

function escapeHtml(raw: string): string {
  return raw
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

function isValidHex32(raw: string): boolean {
  return /^[0-9a-fA-F]{32}$/.test(raw.trim());
}

const STATE_LABELS_ZH: Readonly<Record<CanonicalTaskState, string>> = {
  PROPOSED: "方案草稿就绪",
  WAITING_CONSENT: "等待您的确认",
  AUTHORIZED: "已授权准备执行",
  RUNNING: "正在执行",
  WAITING_INPUT: "需要补充输入",
  WAITING_RESOURCE: "等待本机资源空闲",
  WAITING_NETWORK: "等待网络连接恢复",
  SUSPENDED: "已暂停",
  RECOVERING: "正在恢复重连",
  CANCELLING: "正在停止任务",
  COMPLETING: "正在完成收尾",
  COMPLETED: "已完成",
  PARTIAL: "部分完成",
  DISPUTED: "结果待复核",
  FAILED: "执行受阻",
  CANCELLED: "已取消",
};

const STATE_LABELS_EN: Readonly<Record<CanonicalTaskState, string>> = {
  PROPOSED: "Proposal ready",
  WAITING_CONSENT: "Awaiting your confirmation",
  AUTHORIZED: "Authorized to run",
  RUNNING: "Running",
  WAITING_INPUT: "Needs your input",
  WAITING_RESOURCE: "Waiting for local resources",
  WAITING_NETWORK: "Waiting for network",
  SUSPENDED: "Paused",
  RECOVERING: "Recovering state",
  CANCELLING: "Stopping task",
  COMPLETING: "Finalizing output",
  COMPLETED: "Completed",
  PARTIAL: "Partially completed",
  DISPUTED: "Needs review",
  FAILED: "Blocked by error",
  CANCELLED: "Cancelled",
};

interface FixtureEntrySpec {
  readonly state: CanonicalTaskState;
  readonly category: NotificationCategory;
  readonly severity: NotificationSeverity;
  readonly titleZh: string;
  readonly titleEn: string;
  readonly summaryZh: string;
  readonly summaryEn: string;
  readonly actionSpec?: {
    readonly actionId: string;
    readonly labelZh: string;
    readonly labelEn: string;
  };
}

export const DEFAULT_CALLER_PRINCIPAL_ID_HEX = "a1a2a3a4a5a6a7a8b1b2b3b4b5b6b7b8" as const;
export const DEFAULT_CALLER_APP_ID_HEX = "c1c2c3c4c5c6c7c8d1d2d3d4d5d6d7d8" as const;

export function createDefaultNotificationFixture(): NotificationFeedItem[] {
  const subscriberKeyHex = deriveDeterministicId32Hex("subscriber", 1);
  const specs: readonly FixtureEntrySpec[] = [
    {
      state: "PROPOSED",
      category: "TASK_TRANSITION",
      severity: "INFO",
      titleZh: "季度财务报表整理方案已生成",
      titleEn: "Quarterly financial report proposal generated",
      summaryZh: "已为您准备好报表汇总方案，可随时查看或提交运行。",
      summaryEn: "Your report aggregation proposal is ready to review or run.",
      actionSpec: { actionId: "open_proposed_task", labelZh: "查看方案", labelEn: "Open Proposal" },
    },
    {
      state: "WAITING_CONSENT",
      category: "APPROVAL_REQUIRED",
      severity: "WARNING",
      titleZh: "生产环境配置导出需要您的确认",
      titleEn: "Production config export awaits your confirmation",
      summaryZh: "导出操作将生成包含环境参数的归档文件，请确认是否继续执行。",
      summaryEn:
        "Exporting will generate an archive with environment parameters; please confirm to proceed.",
      actionSpec: {
        actionId: "approve_task_step",
        labelZh: "批准并继续",
        labelEn: "Approve & Continue",
      },
    },
    {
      state: "AUTHORIZED",
      category: "TASK_TRANSITION",
      severity: "INFO",
      titleZh: "代码仓依赖升级方案已获授权",
      titleEn: "Repository dependency upgrade authorized",
      summaryZh: "权限校验已通过，正在为您启动分步升级步骤。",
      summaryEn: "Authorization passed; starting safe step-by-step upgrade.",
      actionSpec: {
        actionId: "inspect_authorized_task",
        labelZh: "查看步骤",
        labelEn: "View Steps",
      },
    },
    {
      state: "RUNNING",
      category: "TASK_TRANSITION",
      severity: "INFO",
      titleZh: "正在执行多语言文档校验",
      titleEn: "Running multilingual documentation verification",
      summaryZh: "已完成 68% 的文档章节交叉核对，预计剩余 20 秒。",
      summaryEn: "Completed 68% of document section cross-checks; about 20 seconds remaining.",
      actionSpec: {
        actionId: "inspect_running_task",
        labelZh: "查看实时进度",
        labelEn: "View Live Progress",
      },
    },
    {
      state: "WAITING_INPUT",
      category: "APPROVAL_REQUIRED",
      severity: "WARNING",
      titleZh: "报表导出路径需要补充选择",
      titleEn: "Report export destination needs your selection",
      summaryZh: "请选择保存导出文件的本地目标文件夹以继续完成任务。",
      summaryEn: "Please choose a local destination folder for the exported file to continue.",
      actionSpec: {
        actionId: "provide_human_input",
        labelZh: "选择目标文件夹",
        labelEn: "Choose Folder",
      },
    },
    {
      state: "WAITING_RESOURCE",
      category: "TASK_TRANSITION",
      severity: "INFO",
      titleZh: "夜间全量索引构建排队等待空闲算力",
      titleEn: "Nightly full index build waiting for idle capacity",
      summaryZh: "任务已排入本机空闲队列，将在当前高负载任务完成后自动启动。",
      summaryEn: "Queued for local idle capacity; will start automatically once current load drops.",
      actionSpec: { actionId: "run_scheduled_now", labelZh: "立即启动", labelEn: "Start Now" },
    },
    {
      state: "WAITING_NETWORK",
      category: "SYSTEM_POSTURE",
      severity: "WARNING",
      titleZh: "云端文献同步正在等待网络恢复",
      titleEn: "Cloud reference sync waiting for network",
      summaryZh: "远端连接恢复后将自动继续同步，本机已有文献可正常查阅。",
      summaryEn: "Sync will resume automatically when network returns; local files remain available.",
    },
    {
      state: "SUSPENDED",
      category: "TASK_TRANSITION",
      severity: "INFO",
      titleZh: "大文件归档压缩已暂停",
      titleEn: "Large archive compression paused",
      summaryZh: "任务已按您的指令安全暂停，保留当前处理进度。",
      summaryEn: "Task is safely paused at your request with current progress preserved.",
      actionSpec: { actionId: "resume_paused_task", labelZh: "继续执行", labelEn: "Resume Task" },
    },
    {
      state: "RECOVERING",
      category: "ANOMALY_ALERT",
      severity: "WARNING",
      titleZh: "智能摘要服务已自动切换至本机恢复模式",
      titleEn: "Smart summary switched to local recovery mode",
      summaryZh: "正在从上次保存点恢复任务进度，无需您重新提交。",
      summaryEn: "Restoring task progress from last saved step; no resubmission required.",
      actionSpec: {
        actionId: "acknowledge_recovery_mode",
        labelZh: "知悉并继续",
        labelEn: "Keep Running",
      },
    },
    {
      state: "CANCELLING",
      category: "TASK_TRANSITION",
      severity: "INFO",
      titleZh: "正在安全停止批量重命名任务",
      titleEn: "Safely stopping batch rename task",
      summaryZh: "正在回收临时句柄并还原未完成的文件名变更。",
      summaryEn: "Releasing temporary handles and restoring incomplete filename changes.",
    },
    {
      state: "COMPLETING",
      category: "TASK_MILESTONE",
      severity: "INFO",
      titleZh: "正在生成最终桌面视觉巡检摘要",
      titleEn: "Generating final desktop visual audit summary",
      summaryZh: "主体校验已完成，正在写入本地核验报告文件。",
      summaryEn: "Core checks finished; writing local verification report file.",
    },
    {
      state: "COMPLETED",
      category: "TASK_MILESTONE",
      severity: "INFO",
      titleZh: "桌面视觉规范一致性巡检已完成",
      titleEn: "Desktop visual specification audit completed",
      summaryZh: "全部检查项均已通过，已生成可随时查阅的本地核验报告。",
      summaryEn: "All checks passed and a local verification report is ready for inspection.",
      actionSpec: {
        actionId: "open_completed_artifact",
        labelZh: "打开核验报告",
        labelEn: "Open Report",
      },
    },
    {
      state: "PARTIAL",
      category: "TASK_MILESTONE",
      severity: "WARNING",
      titleZh: "跨目录批量整理已部分完成",
      titleEn: "Batch folder organization partially completed",
      summaryZh: "已完成 14 个目录的归档整理，有 2 个受保护文件被安全跳过。",
      summaryEn: "Archived 14 folders; 2 protected files were safely skipped.",
      actionSpec: {
        actionId: "review_partial_items",
        labelZh: "查看跳过项",
        labelEn: "Review Skipped Items",
      },
    },
    {
      state: "DISPUTED",
      category: "ANOMALY_ALERT",
      severity: "WARNING",
      titleZh: "外部账单核对发现差异待复核",
      titleEn: "External invoice reconciliation found differences for review",
      summaryZh: "有 1 笔远端回执金额与本地记录不一致，已为您保留对比详情。",
      summaryEn: "One remote receipt differs from local records; comparison details are ready.",
      actionSpec: {
        actionId: "review_disputed_record",
        labelZh: "查看对比详情",
        labelEn: "Review Details",
      },
    },
    {
      state: "FAILED",
      category: "ANOMALY_ALERT",
      severity: "CRITICAL",
      titleZh: "外部数据源同步连接受阻",
      titleEn: "External data source sync blocked",
      summaryZh: "无法连接远端数据接口，本机已有数据未受影响，可稍后重试。",
      summaryEn: "Could not reach remote data endpoint; local data is intact and ready to retry.",
      actionSpec: { actionId: "retry_failed_step", labelZh: "重试同步", labelEn: "Retry Sync" },
    },
    {
      state: "CANCELLED",
      category: "TASK_TRANSITION",
      severity: "INFO",
      titleZh: "临时日志导出任务已取消",
      titleEn: "Temporary log export cancelled",
      summaryZh: "任务已按请求停止，未产生任何残留临时文件。",
      summaryEn: "Task stopped as requested without leaving any temporary files.",
    },
  ];

  return specs.map((spec, idx) => {
    const sequence = idx + 1;
    const notificationIdHex = deriveDeterministicId32Hex("notification", sequence);
    const taskIdHex = deriveDeterministicId32Hex("task", sequence);
    const topicIdHex = deriveDeterministicId32Hex("topic", sequence);
    const createdAtMonoMs = 1_700_000_000_000 + sequence * 15_000;
    const expiryMonoMs = createdAtMonoMs + 3_600_000;

    let actionToken: NotificationActionTokenView | undefined;
    let verifierCommitmentHex: string;

    if (spec.actionSpec !== undefined) {
      const tokenIdHex = deriveDeterministicId32Hex("action_token", sequence);
      verifierCommitmentHex = computeActionTokenCommitmentHex({
        notificationIdHex,
        tokenIdHex,
        actionId: spec.actionSpec.actionId,
        appIdHex: DEFAULT_CALLER_APP_ID_HEX,
        taskIdHex,
        principalIdHex: DEFAULT_CALLER_PRINCIPAL_ID_HEX,
        expiryMonoMs,
      });
      actionToken = {
        tokenIdHex,
        actionId: spec.actionSpec.actionId,
        labelZh: spec.actionSpec.labelZh,
        labelEn: spec.actionSpec.labelEn,
        boundNotificationIdHex: notificationIdHex,
        boundAppIdHex: DEFAULT_CALLER_APP_ID_HEX,
        boundTaskIdHex: taskIdHex,
        boundPrincipalIdHex: DEFAULT_CALLER_PRINCIPAL_ID_HEX,
        expiryMonoMs,
        consumed: false,
        verifierCommitmentHex,
      };
    } else {
      verifierCommitmentHex = computeActionTokenCommitmentHex({
        notificationIdHex,
        tokenIdHex: "00000000000000000000000000000000",
        actionId: "inspect_only",
        appIdHex: DEFAULT_CALLER_APP_ID_HEX,
        taskIdHex,
        principalIdHex: DEFAULT_CALLER_PRINCIPAL_ID_HEX,
        expiryMonoMs,
      });
    }

    const tokenConsumed = actionToken === undefined ? true : actionToken.consumed;
    const pruned = computeAllowedVerbs({
      domain: "notification",
      state: tokenConsumed ? "CONSUMED" : "UNREAD",
      callerCapabilities: ["control:write"],
      readBacking: {
        targetIdHex: notificationIdHex,
        expectedGenerationOrRevision: sequence,
      },
      actionTokenConsumed: tokenConsumed,
    });
    const allowedVerbs = tokenConsumed
      ? ["inspect", "dismiss", ...pruned.allowedVerbs]
      : ["inspect", "redeem_action_token", "dismiss", ...pruned.allowedVerbs];

    return {
      notificationIdHex,
      sequence,
      taskIdHex,
      appIdHex: DEFAULT_CALLER_APP_ID_HEX,
      appDisplayNameZh: "工作台协同中心",
      appDisplayNameEn: "Workspace Coordinator",
      taskState: spec.state,
      category: spec.category,
      severity: spec.severity,
      titleZh: spec.titleZh,
      titleEn: spec.titleEn,
      summaryZh: spec.summaryZh,
      summaryEn: spec.summaryEn,
      stateLabelZh: STATE_LABELS_ZH[spec.state],
      stateLabelEn: STATE_LABELS_EN[spec.state],
      createdAtMonoMs,
      unread: sequence >= 11,
      dismissed: false,
      actionToken,
      allowedVerbs,
      l3Diagnostics: {
        topicIdHex,
        subscriberKeyHex,
        verifierCommitmentHex,
        sequence,
        pruneWatermarkSequence: 0,
        retractedEventCount: 2,
        deliveryAttempt: 1,
      },
    };
  });
}

export interface NotificationCenterViewState {
  readonly open: boolean;
  readonly disclosure: DisclosureLevel;
  readonly locale: WorkstationLocale;
  readonly unreadOnly: boolean;
  readonly categoryFilter?: NotificationCategory;
  readonly currentPage: NotificationFeedPage;
  readonly audit: Readonly<NotificationSecurityAuditCounters>;
}

export interface NotificationCenterControllerOptions {
  readonly initialDisclosure?: DisclosureLevel;
  readonly initialLocale?: WorkstationLocale;
  readonly initialItems?: readonly NotificationFeedItem[];
  readonly compositor?: WorkstationCompositor;
}

export class NotificationCenterController {
  private items: NotificationFeedItem[];
  private ackWatermarkSequence = 10;
  private pruneWatermarkSequence = 0;
  private retractedEventCount = 2;
  private open = false;
  private disclosure: DisclosureLevel;
  private locale: WorkstationLocale;
  private unreadOnly = false;
  private categoryFilter?: NotificationCategory;
  private pageLimit = 16;
  private cursorSequence?: number;
  private compositor?: WorkstationCompositor;
  private readonly audit: NotificationSecurityAuditCounters = {
    actionTokenRedemptionsSucceeded: 0,
    actionTokenReplayRejections: 0,
    actionTokenExpiredRejections: 0,
    actionTokenAppMismatchRejections: 0,
    actionTokenPrincipalMismatchRejections: 0,
    actionTokenTaskMismatchRejections: 0,
    actionTokenInvalidProofRejections: 0,
  };

  public constructor(options: NotificationCenterControllerOptions = {}) {
    this.disclosure = options.initialDisclosure ?? "L1";
    this.locale = options.initialLocale ?? "zh-CN";
    this.items =
      options.initialItems !== undefined
        ? options.initialItems.map((i) => ({
            ...i,
            actionToken: i.actionToken !== undefined ? { ...i.actionToken } : undefined,
          }))
        : createDefaultNotificationFixture();
    this.compositor = options.compositor;
    this.syncCompositorBadge();
  }

  public bindCompositor(compositor: WorkstationCompositor): void {
    this.compositor = compositor;
    this.syncCompositorBadge();
  }

  public setOpen(open: boolean): void {
    this.open = open;
    if (this.compositor !== undefined) {
      this.compositor.setZ3FlyoutOpen("notification-center", open);
    }
  }

  public isOpen(): boolean {
    return this.open;
  }

  public setDisclosureLevel(level: DisclosureLevel): void {
    this.disclosure = level;
  }

  public getDisclosureLevel(): DisclosureLevel {
    return this.disclosure;
  }

  public setLocale(locale: WorkstationLocale): void {
    this.locale = locale;
  }

  public getLocale(): WorkstationLocale {
    return this.locale;
  }

  public setFilter(options: {
    readonly unreadOnly?: boolean;
    readonly categoryFilter?: NotificationCategory;
    readonly limit?: number;
    readonly cursorSequence?: number;
  }): void {
    if (options.unreadOnly !== undefined) {
      this.unreadOnly = options.unreadOnly;
    }
    this.categoryFilter = options.categoryFilter;
    if (options.limit !== undefined) {
      this.pageLimit = options.limit;
    }
    this.cursorSequence = options.cursorSequence;
  }

  public listNotificationFeed(
    request: ListNotificationFeedRequest = {},
  ): NotificationFeedPage {
    const rawLimit = request.limit ?? this.pageLimit;
    if (rawLimit < 1) {
      throw new NotificationCenterError("INVALID_ARGUMENT", "limit must be >= 1");
    }
    const clampedLimit = Math.min(rawLimit, MAX_NOTIFICATION_PAGE_LIMIT);
    const includeDismissed = request.includeDismissed ?? false;
    const unreadOnly = request.unreadOnly ?? this.unreadOnly;
    const categoryFilter = request.categoryFilter ?? this.categoryFilter;
    const cursorSequence = request.cursorSequence ?? this.cursorSequence;

    const totalUnreadCount = this.items.filter((i) => i.unread && !i.dismissed).length;
    const totalActiveCount = this.items.filter((i) => !i.dismissed).length;

    const filtered = this.items
      .filter((item) => {
        if (!includeDismissed && item.dismissed) {
          return false;
        }
        if (unreadOnly && !item.unread) {
          return false;
        }
        if (categoryFilter !== undefined && item.category !== categoryFilter) {
          return false;
        }
        if (cursorSequence !== undefined && item.sequence >= cursorSequence) {
          return false;
        }
        return true;
      })
      .sort((a, b) => b.sequence - a.sequence);

    const hasMore = filtered.length > clampedLimit;
    const pageItems = filtered.slice(0, clampedLimit);
    const nextCursorSequence =
      hasMore && pageItems.length > 0
        ? pageItems[pageItems.length - 1]!.sequence
        : undefined;

    return {
      items: pageItems,
      nextCursorSequence,
      hasMore,
      totalUnreadCount,
      totalActiveCount,
      ackWatermarkSequence: this.ackWatermarkSequence,
      pruneWatermarkSequence: this.pruneWatermarkSequence,
      retractedEventCount: this.retractedEventCount,
    };
  }

  public inspectNotificationItem(notificationIdHex: string): NotificationFeedItem {
    if (!isValidHex32(notificationIdHex)) {
      throw new NotificationCenterError(
        "INVALID_ARGUMENT",
        `notificationIdHex must be 32 hex chars, got '${notificationIdHex}'`,
      );
    }
    const found = this.items.find(
      (i) => i.notificationIdHex.toLowerCase() === notificationIdHex.trim().toLowerCase(),
    );
    if (found === undefined) {
      throw new NotificationCenterError(
        "ACTION_TOKEN_NOT_FOUND",
        `notification '${notificationIdHex}' not found`,
      );
    }
    return found;
  }

  public redeemNotificationActionToken(
    request: RedeemNotificationActionTokenRequest,
  ): RedeemNotificationActionTokenReceipt {
    if (
      !isValidHex32(request.notificationIdHex) ||
      !isValidHex32(request.tokenIdHex) ||
      !isValidHex32(request.callerAppIdHex) ||
      !isValidHex32(request.callerPrincipalIdHex) ||
      !isValidHex32(request.taskIdHex)
    ) {
      throw new NotificationCenterError(
        "INVALID_ARGUMENT",
        "All identifier fields in RedeemNotificationActionTokenRequest must be 32 hex chars",
      );
    }

    const item = this.items.find(
      (i) =>
        i.notificationIdHex.toLowerCase() === request.notificationIdHex.trim().toLowerCase(),
    );
    if (item === undefined || item.actionToken === undefined) {
      throw new NotificationCenterError(
        "ACTION_TOKEN_NOT_FOUND",
        `Notification '${request.notificationIdHex}' or its inline ActionToken was not found`,
      );
    }

    const token = item.actionToken;

    if (
      token.tokenIdHex.toLowerCase() !== request.tokenIdHex.trim().toLowerCase() ||
      token.actionId !== request.actionId.trim()
    ) {
      this.audit.actionTokenInvalidProofRejections += 1;
      throw new NotificationCenterError(
        "ACTION_TOKEN_INVALID_PROOF",
        "tokenIdHex or actionId does not match bound ActionToken",
      );
    }

    if (token.consumed) {
      this.audit.actionTokenReplayRejections += 1;
      throw new NotificationCenterError(
        "ACTION_TOKEN_ALREADY_CONSUMED",
        `ActionToken '${token.tokenIdHex}' has already been redeemed (one-shot replay rejected)`,
      );
    }

    if (token.boundAppIdHex.toLowerCase() !== request.callerAppIdHex.trim().toLowerCase()) {
      this.audit.actionTokenAppMismatchRejections += 1;
      throw new NotificationCenterError(
        "ACTION_TOKEN_APP_MISMATCH",
        `callerAppIdHex '${request.callerAppIdHex}' mismatches boundAppIdHex '${token.boundAppIdHex}'`,
      );
    }

    if (
      token.boundPrincipalIdHex.toLowerCase() !==
      request.callerPrincipalIdHex.trim().toLowerCase()
    ) {
      this.audit.actionTokenPrincipalMismatchRejections += 1;
      throw new NotificationCenterError(
        "ACTION_TOKEN_PRINCIPAL_MISMATCH",
        `callerPrincipalIdHex '${request.callerPrincipalIdHex}' mismatches boundPrincipalIdHex '${token.boundPrincipalIdHex}'`,
      );
    }

    if (token.boundTaskIdHex.toLowerCase() !== request.taskIdHex.trim().toLowerCase()) {
      this.audit.actionTokenTaskMismatchRejections += 1;
      throw new NotificationCenterError(
        "ACTION_TOKEN_TASK_MISMATCH",
        `taskIdHex '${request.taskIdHex}' mismatches boundTaskIdHex '${token.boundTaskIdHex}'`,
      );
    }

    if (request.nowMonoMs > token.expiryMonoMs) {
      this.audit.actionTokenExpiredRejections += 1;
      throw new NotificationCenterError(
        "ACTION_TOKEN_EXPIRED",
        `ActionToken '${token.tokenIdHex}' expired at ${token.expiryMonoMs} ms (now=${request.nowMonoMs} ms)`,
      );
    }

    const expectedCommitment = computeActionTokenCommitmentHex({
      notificationIdHex: token.boundNotificationIdHex,
      tokenIdHex: token.tokenIdHex,
      actionId: token.actionId,
      appIdHex: token.boundAppIdHex,
      taskIdHex: token.boundTaskIdHex,
      principalIdHex: token.boundPrincipalIdHex,
      expiryMonoMs: token.expiryMonoMs,
    });

    if (expectedCommitment !== token.verifierCommitmentHex.toLowerCase()) {
      this.audit.actionTokenInvalidProofRejections += 1;
      throw new NotificationCenterError(
        "ACTION_TOKEN_INVALID_PROOF",
        "ActionToken cryptographic commitment verification failed",
      );
    }

    if (
      request.presentedCommitmentHex !== undefined &&
      request.presentedCommitmentHex.trim().toLowerCase() !== expectedCommitment
    ) {
      this.audit.actionTokenInvalidProofRejections += 1;
      throw new NotificationCenterError(
        "ACTION_TOKEN_INVALID_PROOF",
        "presentedCommitmentHex does not match authoritative ActionToken commitment",
      );
    }

    // Atomic one-shot redemption
    const receiptDigestHex = computeDeterministicDigest64Hex(
      "LLMOS_STAGE_D_NOTIFY_ACTION_TOKEN_RECEIPT_V1",
      [
        token.boundNotificationIdHex,
        token.tokenIdHex,
        token.actionId,
        String(request.nowMonoMs),
      ],
    );

    token.consumed = true;
    token.consumedAtMonoMs = request.nowMonoMs;
    token.redemptionReceiptDigestHex = receiptDigestHex;
    item.unread = false;

    const resultingState: CanonicalTaskState =
      item.taskState === "WAITING_CONSENT" ||
      item.taskState === "WAITING_INPUT" ||
      item.taskState === "SUSPENDED" ||
      item.taskState === "WAITING_RESOURCE"
        ? "RUNNING"
        : item.taskState === "FAILED"
          ? "AUTHORIZED"
          : item.taskState;

    item.taskState = resultingState;
    item.stateLabelZh = STATE_LABELS_ZH[resultingState];
    item.stateLabelEn = STATE_LABELS_EN[resultingState];

    const prunedAfter = computeAllowedVerbs({
      domain: "notification",
      state: "CONSUMED",
      callerCapabilities: ["control:write"],
      readBacking: {
        targetIdHex: item.notificationIdHex,
        expectedGenerationOrRevision: item.sequence,
      },
      actionTokenConsumed: true,
    });
    item.allowedVerbs = ["inspect", "dismiss", ...prunedAfter.allowedVerbs];

    this.audit.actionTokenRedemptionsSucceeded += 1;
    this.syncCompositorBadge();

    const updatedUnreadCount = this.items.filter((i) => i.unread && !i.dismissed).length;

    return {
      notificationIdHex: item.notificationIdHex,
      tokenIdHex: token.tokenIdHex,
      actionId: token.actionId,
      taskIdHex: item.taskIdHex,
      resultingTaskState: resultingState,
      redemptionReceiptDigestHex: receiptDigestHex,
      redeemedAtMonoMs: request.nowMonoMs,
      remainingAllowedVerbs: item.allowedVerbs,
      updatedUnreadCount,
    };
  }

  public dismissNotificationItem(
    request: DismissNotificationItemRequest,
  ): DismissNotificationItemReceipt {
    const item = this.inspectNotificationItem(request.notificationIdHex);
    item.unread = false;
    item.dismissed = true;
    if (item.sequence > this.ackWatermarkSequence) {
      this.ackWatermarkSequence = item.sequence;
    }
    this.syncCompositorBadge();
    const updatedUnreadCount = this.items.filter((i) => i.unread && !i.dismissed).length;
    return {
      notificationIdHex: item.notificationIdHex,
      dismissed: true,
      ackWatermarkSequence: this.ackWatermarkSequence,
      updatedUnreadCount,
    };
  }

  public getSecurityAudit(): Readonly<NotificationSecurityAuditCounters> {
    return { ...this.audit };
  }

  public getState(): NotificationCenterViewState {
    return {
      open: this.open,
      disclosure: this.disclosure,
      locale: this.locale,
      unreadOnly: this.unreadOnly,
      categoryFilter: this.categoryFilter,
      currentPage: this.listNotificationFeed(),
      audit: this.getSecurityAudit(),
    };
  }

  public static verifyAll16CanonicalStatesPresent(
    items: readonly NotificationFeedItem[],
  ): boolean {
    const set = new Set(items.map((i) => i.taskState));
    return CANONICAL_TASK_STATES.every((state) => set.has(state));
  }

  private syncCompositorBadge(): void {
    if (this.compositor !== undefined) {
      const unread = this.items.filter((i) => i.unread && !i.dismissed).length;
      this.compositor.setUnreadNotificationCount(unread);
    }
  }
}

export function renderNotificationCenterHtml(state: NotificationCenterViewState): string {
  const isEn = state.locale === "en-US";
  const isL3 = state.disclosure === "L3";
  const headerTitle = isEn ? "Notification Center" : "通知中心";
  const allTabLabel = isEn ? "All" : "全部";
  const unreadTabLabel = isEn ? "Unread Only" : "仅看未读";
  const approvalTabLabel = isEn ? "Needs Confirmation" : "待确认";
  const anomalyTabLabel = isEn ? "Alerts" : "异常提醒";
  const dismissLabel = isEn ? "Dismiss" : "忽略";
  const consumedLabel = isEn ? "Action Completed" : "已处理";
  const loadMoreLabel = isEn ? "Load Earlier" : "加载更早通知";

  const cardsHtml = state.currentPage.items
    .map((item) => {
      const title = isEn ? item.titleEn : item.titleZh;
      const summary = isEn ? item.summaryEn : item.summaryZh;
      const appName = isEn ? item.appDisplayNameEn : item.appDisplayNameZh;
      const statusBadgeHtml = renderStatusIndicatorHtml({
        state: item.taskState,
        disclosure: state.disclosure,
        locale: state.locale,
      });

      const unreadClass = item.unread ? " nlos-notification-card--unread" : "";
      const criticalClass =
        item.severity === "CRITICAL" ? " nlos-notification-card--critical" : "";

      let actionTokenHtml = "";
      if (item.actionToken !== undefined) {
        const tok = item.actionToken;
        const btnLabel = tok.consumed
          ? consumedLabel
          : isEn
            ? tok.labelEn
            : tok.labelZh;
        const consumedClass = tok.consumed
          ? " nlos-notification-card__token-btn--consumed"
          : "";
        const disabledAttr = tok.consumed ? ' disabled="disabled" aria-disabled="true"' : "";
        const iconName = tok.consumed ? "check" : "play";
        actionTokenHtml = `<button type="button" class="nlos-notification-card__token-btn${consumedClass}" data-action="redeem-token" data-notification-id="${escapeHtml(item.notificationIdHex)}" data-token-id="${escapeHtml(tok.tokenIdHex)}" data-action-id="${escapeHtml(tok.actionId)}"${disabledAttr}>${renderIconSvg(iconName, { size: 14 })}<span>${escapeHtml(btnLabel)}</span></button>`;
      }

      const l3Html = isL3
        ? `<div class="nlos-notification-card__l3 nlos-l3-only" data-l3-diagnostics="true">` +
          `<span>seq=${item.sequence}</span>` +
          `<span>topic=${escapeHtml(item.l3Diagnostics.topicIdHex.slice(0, 12))}</span>` +
          `<span>sub=${escapeHtml(item.l3Diagnostics.subscriberKeyHex.slice(0, 12))}</span>` +
          `<span>commit=${escapeHtml(item.l3Diagnostics.verifierCommitmentHex.slice(0, 16))}</span>` +
          `<span>prune_wm=${item.l3Diagnostics.pruneWatermarkSequence}</span>` +
          `<span>retracted=${item.l3Diagnostics.retractedEventCount}</span>` +
          `</div>`
        : "";

      return (
        `<article class="nlos-notification-card${unreadClass}${criticalClass}" data-notification-id="${escapeHtml(item.notificationIdHex)}" data-task-state="${escapeHtml(item.taskState)}">` +
        `<div class="nlos-notification-card__top">` +
        `<span class="nlos-notification-card__app">${escapeHtml(appName)}</span>` +
        `${statusBadgeHtml}` +
        `</div>` +
        `<h4 class="nlos-notification-card__title">${escapeHtml(title)}</h4>` +
        `<p class="nlos-notification-card__summary">${escapeHtml(summary)}</p>` +
        `<div class="nlos-notification-card__actions">` +
        `${actionTokenHtml}` +
        `<button type="button" class="nlos-notification-card__dismiss-btn" data-action="dismiss-notification" data-notification-id="${escapeHtml(item.notificationIdHex)}">${renderIconSvg("close", { size: 14 })}<span>${escapeHtml(dismissLabel)}</span></button>` +
        `</div>` +
        `${l3Html}` +
        `</article>`
      );
    })
    .join("");

  const footerL3Html = isL3
    ? `<span class="nlos-l3-only">ack_wm=${state.currentPage.ackWatermarkSequence} · prune_wm=${state.currentPage.pruneWatermarkSequence} · retracted=${state.currentPage.retractedEventCount}</span>`
    : `<span>${isEn ? `${state.currentPage.totalActiveCount} active notifications` : `共 ${state.currentPage.totalActiveCount} 条通知`}</span>`;

  const moreBtnHtml = state.currentPage.hasMore
    ? `<button type="button" class="nlos-notification-center__more-btn" data-action="load-more" data-next-cursor="${state.currentPage.nextCursorSequence ?? ""}">${escapeHtml(loadMoreLabel)}</button>`
    : "";

  return (
    `<section class="nlos-notification-center" role="region" aria-label="${escapeHtml(headerTitle)}" data-z-layer="Z3" data-z-index="${Z3_NOTIFICATION_CENTER_Z_INDEX}" data-disclosure="${state.disclosure}">` +
    `<header class="nlos-notification-center__header">` +
    `<div class="nlos-notification-center__title-group">` +
    `${renderIconSvg("notification-tray", { size: 16 })}` +
    `<h3 class="nlos-notification-center__title">${escapeHtml(headerTitle)}</h3>` +
    `<span class="nlos-notification-center__unread-pill" data-unread-count="${state.currentPage.totalUnreadCount}">${state.currentPage.totalUnreadCount}</span>` +
    `</div>` +
    `</header>` +
    `<nav class="nlos-notification-center__filter-bar" aria-label="${isEn ? "Notification filters" : "通知筛选"}">` +
    `<button type="button" class="nlos-notification-center__tab${!state.unreadOnly && state.categoryFilter === undefined ? " nlos-notification-center__tab--active" : ""}" data-filter="all">${escapeHtml(allTabLabel)}</button>` +
    `<button type="button" class="nlos-notification-center__tab${state.unreadOnly ? " nlos-notification-center__tab--active" : ""}" data-filter="unread">${escapeHtml(unreadTabLabel)}</button>` +
    `<button type="button" class="nlos-notification-center__tab${state.categoryFilter === "APPROVAL_REQUIRED" ? " nlos-notification-center__tab--active" : ""}" data-filter="approval">${escapeHtml(approvalTabLabel)}</button>` +
    `<button type="button" class="nlos-notification-center__tab${state.categoryFilter === "ANOMALY_ALERT" ? " nlos-notification-center__tab--active" : ""}" data-filter="anomaly">${escapeHtml(anomalyTabLabel)}</button>` +
    `</nav>` +
    `<div class="nlos-notification-center__list" role="feed">` +
    `${cardsHtml}` +
    `</div>` +
    `<footer class="nlos-notification-center__footer">` +
    `${footerL3Html}` +
    `${moreBtnHtml}` +
    `</footer>` +
    `</section>`
  );
}

export function createNotificationCenterElement(
  controller: NotificationCenterController,
  doc: Document = document,
): HTMLElement {
  const wrapper = doc.createElement("div");
  wrapper.className = "nlos-notification-center-host";
  const render = (): void => {
    wrapper.innerHTML = renderNotificationCenterHtml(controller.getState());
  };
  render();
  return wrapper;
}
