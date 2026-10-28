/**
 * Stage D (`D-NOTIFY-SEARCH-OFFLINE` / `W72-D-NOTIFY-SEARCH`)
 * `Ctrl+K` Global Spotlight Search & Statutory Lock #1 (`0x1F` Read-Only Non-Authoritative Guarantee).
 *
 * Aligns with:
 * - `06-架构设计总纲-v0.5.md` §16.3.1 (`IPC-D4` `QueryIndex` / `InspectSearchScope`),
 *   §16.3.4 (`IPC-D8`), §27.1–§27.3
 * - `ADR-0025` Decision 1 (`Z3 = 400` command/search surface), Decision 2 (`L0..L3` progressive disclosure
 *   & action pruning), Decision 4 (Statutory Lock #1: `0x1F` read-only search non-authoritative lock)
 * - `2026-10-26-stage-d-desktop-visual-and-ux-design.md` §0.1 (`ANTI-AI-01..07`), §2A.4, §3.6
 */

import type { WorkstationCompositor } from "../../compositor/workstation-compositor";
import { computeAllowedVerbs } from "../../store/action-pruner";
import { renderIconSvg } from "../../ui/icons";
import {
  type CanonicalTaskState,
  type DisclosureLevel,
  renderStatutoryLockBadgeHtml,
  type WorkstationLocale,
} from "../../ui/status-indicator";
import {
  computeDeterministicDigest64Hex,
  DEFAULT_CALLER_APP_ID_HEX,
  deriveDeterministicId32Hex,
} from "../notifications/notification-center";

export const Z3_SPOTLIGHT_Z_INDEX = 400 as const;
export const MAX_SPOTLIGHT_CANDIDATE_LIMIT = 32 as const;

export const SPOTLIGHT_FORBID_CAPABILITY_GRANT = 1 << 0; // 0x01
export const SPOTLIGHT_FORBID_LEDGER_DEDUCTION = 1 << 1; // 0x02
export const SPOTLIGHT_FORBID_TASK_COMMIT = 1 << 2; // 0x04
export const SPOTLIGHT_FORBID_APP_UPDATE = 1 << 3; // 0x08
export const SPOTLIGHT_FORBID_IRREVERSIBLE_ACTION = 1 << 4; // 0x10

export const SPOTLIGHT_STATUTORY_MASK_0X1F: 31 = (
  SPOTLIGHT_FORBID_CAPABILITY_GRANT |
  SPOTLIGHT_FORBID_LEDGER_DEDUCTION |
  SPOTLIGHT_FORBID_TASK_COMMIT |
  SPOTLIGHT_FORBID_APP_UPDATE |
  SPOTLIGHT_FORBID_IRREVERSIBLE_ACTION
) as 31; // 0x1F = 31

export type SpotlightCandidateCategory =
  | "TASK"
  | "ARTIFACT"
  | "APP"
  | "SETTING"
  | "NOTIFICATION";

export type SearchForbiddenControlGate =
  | "CAPABILITY_GRANT"
  | "RESOURCE_LEDGER_DEDUCTION"
  | "TASK_COMMIT"
  | "APPLICATION_UPDATE"
  | "IRREVERSIBLE_ACTION";

export const ALL_SEARCH_FORBIDDEN_GATES: readonly SearchForbiddenControlGate[] = [
  "CAPABILITY_GRANT",
  "RESOURCE_LEDGER_DEDUCTION",
  "TASK_COMMIT",
  "APPLICATION_UPDATE",
  "IRREVERSIBLE_ACTION",
];

export type SpotlightErrorCode =
  | "SEARCH_RESULT_NON_AUTHORITATIVE_0X1F"
  | "SEARCH_CANDIDATE_NOT_FOUND"
  | "AUTHORITY_RECORD_RETRACTED"
  | "STALE_GENERATION"
  | "INVALID_ARGUMENT";

export class SpotlightError extends Error {
  public readonly code: SpotlightErrorCode;

  public constructor(code: SpotlightErrorCode, message: string) {
    super(`[${code}] ${message}`);
    this.name = "SpotlightError";
    this.code = code;
  }
}

export interface SpotlightShardStatus {
  readonly cellIdHex: string;
  readonly nodeLabelZh: string;
  readonly nodeLabelEn: string;
  readonly isLocalPrimary: boolean;
  reachable: boolean;
  readonly checkpointGeneration: number;
}

export interface SpotlightCandidateL3Diagnostics {
  readonly authorityTag: "NON_AUTHORITATIVE · mask=0x1F";
  readonly restrictionsBitmaskHex: "0x1F";
  readonly sourceCellIdHex: string;
  readonly checkpointIdHex: string;
  readonly checkpointDigestHex: string;
  readonly snapshotGeneration: number;
  readonly scorePermille: number;
}

export interface SpotlightCandidateItem {
  readonly candidateIdHex: string;
  readonly targetIdHex: string;
  readonly category: SpotlightCandidateCategory;
  readonly titleZh: string;
  readonly titleEn: string;
  readonly snippetZh: string;
  readonly snippetEn: string;
  readonly candidateHintZh: string;
  readonly candidateHintEn: string;
  readonly taskState?: CanonicalTaskState;
  readonly authorityGrade: "NON_AUTHORITATIVE";
  readonly restrictionsBitmask: 31;
  readonly allowedVerbs: readonly string[];
  readonly snapshotGeneration: number;
  readonly l3Diagnostics: SpotlightCandidateL3Diagnostics;
}

export interface QuerySpotlightCandidatesRequest {
  readonly queryText?: string;
  readonly categoryFilter?: SpotlightCandidateCategory;
  readonly limit?: number;
}

export interface SpotlightQueryResponse {
  readonly normalizedQuery: string;
  readonly candidates: readonly SpotlightCandidateItem[];
  readonly envelopeAuthorityGrade: "NON_AUTHORITATIVE";
  readonly envelopeRestrictionsBitmask: 31;
  readonly hasDegradedShards: boolean;
  readonly scopeNoticeZh: string;
  readonly scopeNoticeEn: string;
  readonly shardStatuses: readonly SpotlightShardStatus[];
  readonly l3ConsistencyLevel: string;
  readonly l3CheckpointDigestHex: string;
  readonly l3RetractedFilteredCount: number;
}

export interface AttemptMutationFromSearchHitRequest {
  readonly candidateIdHex: string;
  readonly attemptedGate: SearchForbiddenControlGate;
  readonly callerSuppliedMask?: number;
}

export interface VerifySpotlightCandidateAuthorityRequest {
  readonly candidateIdHex: string;
  readonly expectedTargetIdHex: string;
  readonly snapshotGeneration: number;
}

export interface SpotlightAuthorityVerificationReceipt {
  readonly candidateIdHex: string;
  readonly targetIdHex: string;
  readonly category: SpotlightCandidateCategory;
  readonly verified: boolean;
  readonly authoritativeStoreKind: string;
  readonly targetViewId: string;
  readonly authoritativeGeneration: number;
  readonly verificationDigestHex: string;
  readonly unlockedAuthoritativeVerbs: readonly string[];
  readonly messageZh: string;
  readonly messageEn: string;
}

export interface SpotlightSecurityAuditCounters {
  search0x1fMutationRejections: number;
  searchForbidCapabilityGrantHits: number;
  searchForbidLedgerDeductionHits: number;
  searchForbidTaskCommitHits: number;
  searchForbidAppUpdateHits: number;
  searchForbidIrreversibleActionHits: number;
  searchAuthorityRetractedRejections: number;
  searchStaleGenerationRejections: number;
  searchAuthorityVerificationsSucceeded: number;
}

export const LOCAL_PRIMARY_CELL_ID_HEX = "11111111111111112222222222222222" as const;
export const REMOTE_WORKER_CELL_ID_HEX = "33333333333333334444444444444444" as const;
export const QUARANTINED_CELL_ID_HEX = "55555555555555556666666666666666" as const;

const CATEGORY_LABELS_ZH: Readonly<Record<SpotlightCandidateCategory, string>> = {
  TASK: "任务",
  ARTIFACT: "本地制品与文档",
  APP: "受管应用",
  SETTING: "系统设置",
  NOTIFICATION: "通知历史",
};

const CATEGORY_LABELS_EN: Readonly<Record<SpotlightCandidateCategory, string>> = {
  TASK: "Tasks",
  ARTIFACT: "Local Artifacts",
  APP: "Managed Apps",
  SETTING: "System Settings",
  NOTIFICATION: "Notification History",
};

const STORE_KIND_BY_CATEGORY: Readonly<Record<SpotlightCandidateCategory, string>> = {
  TASK: "AUTHORITATIVE_TASK_STORE",
  ARTIFACT: "AUTHORITATIVE_ARTIFACT_STORE",
  APP: "AUTHORITATIVE_APP_REGISTRY",
  SETTING: "AUTHORITATIVE_SETTINGS_STORE",
  NOTIFICATION: "AUTHORITATIVE_NOTIFICATION_STORE",
};

const TARGET_VIEW_BY_CATEGORY: Readonly<Record<SpotlightCandidateCategory, string>> = {
  TASK: "task-control-drawer",
  ARTIFACT: "artifact-inspector-view",
  APP: "launcher-workspace",
  SETTING: "settings-governance-view",
  NOTIFICATION: "notification-center-flyout",
};

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

interface AuthoritativeRecordState {
  readonly targetIdHex: string;
  readonly category: SpotlightCandidateCategory;
  authoritativeGeneration: number;
  retracted: boolean;
  readonly unlockedVerbs: readonly string[];
}

export function createDefaultSpotlightFixture(): {
  candidates: SpotlightCandidateItem[];
  shards: SpotlightShardStatus[];
} {
  const checkpointIdHex = deriveDeterministicId32Hex("search_checkpoint", 1);
  const checkpointDigestHex = computeDeterministicDigest64Hex(
    "LLMOS_STAGE_D_SPOTLIGHT_CHECKPOINT_V1",
    [checkpointIdHex, LOCAL_PRIMARY_CELL_ID_HEX],
  );

  const prunedSearch = computeAllowedVerbs({
    domain: "search",
    state: "CANDIDATE",
    callerCapabilities: ["control:write"],
    searchRestrictionsBits: SPOTLIGHT_STATUTORY_MASK_0X1F,
    statutoryLocks: ["SEARCH_0X1F_READ_ONLY"],
  });
  const searchVerbs =
    prunedSearch.statutoryLock === "SEARCH_0X1F_READ_ONLY"
      ? (["inspect", "jump_to_authority"] as const)
      : prunedSearch.allowedVerbs;

  const rawSpecs: ReadonlyArray<{
    readonly index: number;
    readonly category: SpotlightCandidateCategory;
    readonly targetIdHex: string;
    readonly titleZh: string;
    readonly titleEn: string;
    readonly snippetZh: string;
    readonly snippetEn: string;
    readonly taskState?: CanonicalTaskState;
    readonly sourceCellIdHex: string;
    readonly scorePermille: number;
  }> = [
    {
      index: 1,
      category: "TASK",
      targetIdHex: deriveDeterministicId32Hex("task", 2),
      titleZh: "生产环境配置导出任务",
      titleEn: "Production Config Export Task",
      snippetZh: "状态：等待您的确认 · 包含环境参数归档校验",
      snippetEn: "Status: Awaiting your confirmation · Includes environment archive check",
      taskState: "WAITING_CONSENT",
      sourceCellIdHex: LOCAL_PRIMARY_CELL_ID_HEX,
      scorePermille: 980,
    },
    {
      index: 2,
      category: "ARTIFACT",
      targetIdHex: deriveDeterministicId32Hex("artifact", 1),
      titleZh: "Stage D 桌面视觉一致性核验报告.md",
      titleEn: "Stage D Desktop Visual Audit Report.md",
      snippetZh: "本地制品 · Graphite 与 Paper 双主题对比度及零毛玻璃核验记录",
      snippetEn: "Local artifact · Graphite & Paper contrast and zero-glass verification log",
      taskState: "COMPLETED",
      sourceCellIdHex: LOCAL_PRIMARY_CELL_ID_HEX,
      scorePermille: 955,
    },
    {
      index: 3,
      category: "APP",
      targetIdHex: DEFAULT_CALLER_APP_ID_HEX,
      titleZh: "工作台制品检视器",
      titleEn: "Workspace Artifact Inspector",
      snippetZh: "受管桌面应用 · 支持离线查阅本地证据包、日志与制品快照",
      snippetEn: "Managed desktop app · Offline inspection of local evidence bundles and snapshots",
      sourceCellIdHex: LOCAL_PRIMARY_CELL_ID_HEX,
      scorePermille: 920,
    },
    {
      index: 4,
      category: "SETTING",
      targetIdHex: deriveDeterministicId32Hex("setting", 1),
      titleZh: "主题外观与高对比度护眼设置",
      titleEn: "Theme Appearance & High-Contrast Settings",
      snippetZh: "系统设置 · 切换 Graphite 暗色、Paper 亮色与高对比度模式",
      snippetEn: "System settings · Switch between Graphite dark, Paper light, and high-contrast",
      sourceCellIdHex: LOCAL_PRIMARY_CELL_ID_HEX,
      scorePermille: 890,
    },
    {
      index: 5,
      category: "NOTIFICATION",
      targetIdHex: deriveDeterministicId32Hex("notification", 9),
      titleZh: "智能摘要服务已自动切换至本机恢复模式",
      titleEn: "Smart Summary Switched to Local Recovery Mode",
      snippetZh: "通知历史 · 远端网络不可达时由本机模型连续提供服务",
      snippetEn: "Notification history · Local model provides continuous service when offline",
      taskState: "RECOVERING",
      sourceCellIdHex: LOCAL_PRIMARY_CELL_ID_HEX,
      scorePermille: 860,
    },
    {
      index: 6,
      category: "ARTIFACT",
      targetIdHex: deriveDeterministicId32Hex("artifact", 2),
      titleZh: "跨节点联邦构建缓存清单",
      titleEn: "Federated Node Build Cache Manifest",
      snippetZh: "协同节点索引快照 · 点击后将向来源存储重核验最新版本",
      snippetEn: "Federated node index snapshot · Clicking re-verifies latest version with source",
      taskState: "RUNNING",
      sourceCellIdHex: REMOTE_WORKER_CELL_ID_HEX,
      scorePermille: 830,
    },
  ];

  const candidates: SpotlightCandidateItem[] = rawSpecs.map((spec) => ({
    candidateIdHex: deriveDeterministicId32Hex("spotlight_candidate", spec.index),
    targetIdHex: spec.targetIdHex,
    category: spec.category,
    titleZh: spec.titleZh,
    titleEn: spec.titleEn,
    snippetZh: spec.snippetZh,
    snippetEn: spec.snippetEn,
    candidateHintZh: "搜索候选（点击跳转核验）",
    candidateHintEn: "Search candidate — click to verify source",
    taskState: spec.taskState,
    authorityGrade: "NON_AUTHORITATIVE",
    restrictionsBitmask: SPOTLIGHT_STATUTORY_MASK_0X1F,
    allowedVerbs: searchVerbs,
    snapshotGeneration: 1,
    l3Diagnostics: {
      authorityTag: "NON_AUTHORITATIVE · mask=0x1F",
      restrictionsBitmaskHex: "0x1F",
      sourceCellIdHex: spec.sourceCellIdHex,
      checkpointIdHex,
      checkpointDigestHex,
      snapshotGeneration: 1,
      scorePermille: spec.scorePermille,
    },
  }));

  const shards: SpotlightShardStatus[] = [
    {
      cellIdHex: LOCAL_PRIMARY_CELL_ID_HEX,
      nodeLabelZh: "本机主工作站",
      nodeLabelEn: "Primary Local Workstation",
      isLocalPrimary: true,
      reachable: true,
      checkpointGeneration: 42,
    },
    {
      cellIdHex: REMOTE_WORKER_CELL_ID_HEX,
      nodeLabelZh: "远端构建加速节点 A",
      nodeLabelEn: "Remote Build Accelerator A",
      isLocalPrimary: false,
      reachable: true,
      checkpointGeneration: 42,
    },
    {
      cellIdHex: QUARANTINED_CELL_ID_HEX,
      nodeLabelZh: "受保护隔离归档节点 B",
      nodeLabelEn: "Isolated Archive Node B",
      isLocalPrimary: false,
      reachable: false,
      checkpointGeneration: 41,
    },
  ];

  return { candidates, shards };
}

export interface SpotlightViewState {
  readonly open: boolean;
  readonly disclosure: DisclosureLevel;
  readonly locale: WorkstationLocale;
  readonly queryText: string;
  readonly categoryFilter?: SpotlightCandidateCategory;
  readonly response: SpotlightQueryResponse;
  readonly lastVerificationReceipt?: SpotlightAuthorityVerificationReceipt;
  readonly audit: Readonly<SpotlightSecurityAuditCounters>;
}

export interface SpotlightControllerOptions {
  readonly initialDisclosure?: DisclosureLevel;
  readonly initialLocale?: WorkstationLocale;
  readonly compositor?: WorkstationCompositor;
}

export class SpotlightController {
  private readonly candidates: SpotlightCandidateItem[];
  private readonly shards: SpotlightShardStatus[];
  private readonly authoritativeRecords = new Map<string, AuthoritativeRecordState>();
  private retractedFilteredCount = 2;
  private open = false;
  private disclosure: DisclosureLevel;
  private locale: WorkstationLocale;
  private queryText = "";
  private categoryFilter?: SpotlightCandidateCategory;
  private lastVerificationReceipt?: SpotlightAuthorityVerificationReceipt;
  private compositor?: WorkstationCompositor;
  private readonly audit: SpotlightSecurityAuditCounters = {
    search0x1fMutationRejections: 0,
    searchForbidCapabilityGrantHits: 0,
    searchForbidLedgerDeductionHits: 0,
    searchForbidTaskCommitHits: 0,
    searchForbidAppUpdateHits: 0,
    searchForbidIrreversibleActionHits: 0,
    searchAuthorityRetractedRejections: 0,
    searchStaleGenerationRejections: 0,
    searchAuthorityVerificationsSucceeded: 0,
  };

  public constructor(options: SpotlightControllerOptions = {}) {
    this.disclosure = options.initialDisclosure ?? "L1";
    this.locale = options.initialLocale ?? "zh-CN";
    this.compositor = options.compositor;
    const fixture = createDefaultSpotlightFixture();
    this.candidates = fixture.candidates;
    this.shards = fixture.shards;

    for (const cand of this.candidates) {
      this.authoritativeRecords.set(cand.targetIdHex.toLowerCase(), {
        targetIdHex: cand.targetIdHex,
        category: cand.category,
        authoritativeGeneration: 1,
        retracted: false,
        unlockedVerbs: ["inspect", "open_authoritative_view", "export"],
      });
    }
  }

  public bindCompositor(compositor: WorkstationCompositor): void {
    this.compositor = compositor;
  }

  public setOpen(open: boolean): void {
    this.open = open;
    if (this.compositor !== undefined) {
      this.compositor.setZ3FlyoutOpen("command-palette", open);
    }
  }

  public isOpen(): boolean {
    return this.open;
  }

  public setDisclosureLevel(level: DisclosureLevel): void {
    this.disclosure = level;
  }

  public setLocale(locale: WorkstationLocale): void {
    this.locale = locale;
  }

  public setShardReachability(cellIdHex: string, reachable: boolean): void {
    const shard = this.shards.find(
      (s) => s.cellIdHex.toLowerCase() === cellIdHex.trim().toLowerCase(),
    );
    if (shard !== undefined && !shard.isLocalPrimary) {
      shard.reachable = reachable;
    }
  }

  public querySpotlightCandidates(
    request: QuerySpotlightCandidatesRequest = {},
  ): SpotlightQueryResponse {
    const rawLimit = request.limit ?? 12;
    if (rawLimit < 1) {
      throw new SpotlightError("INVALID_ARGUMENT", "Spotlight query limit must be >= 1");
    }
    const clampedLimit = Math.min(rawLimit, MAX_SPOTLIGHT_CANDIDATE_LIMIT);
    if (request.queryText !== undefined) {
      this.queryText = request.queryText;
    }
    if ("categoryFilter" in request) {
      this.categoryFilter = request.categoryFilter;
    }

    const normalizedQuery = this.queryText.trim().toLowerCase();
    const reachableSet = new Set(
      this.shards.filter((s) => s.reachable).map((s) => s.cellIdHex.toLowerCase()),
    );
    const hasDegradedShards = this.shards.some((s) => !s.reachable);

    const prunedSearch = computeAllowedVerbs({
      domain: "search",
      state: "CANDIDATE",
      callerCapabilities: ["control:write"],
      searchRestrictionsBits: SPOTLIGHT_STATUTORY_MASK_0X1F,
      statutoryLocks: ["SEARCH_0X1F_READ_ONLY"],
    });
    const prunedSearchVerbs =
      prunedSearch.statutoryLock === "SEARCH_0X1F_READ_ONLY"
        ? (["inspect", "jump_to_authority"] as const)
        : prunedSearch.allowedVerbs;

    const matched = this.candidates
      .filter((cand) => {
        if (!reachableSet.has(cand.l3Diagnostics.sourceCellIdHex.toLowerCase())) {
          return false;
        }
        if (this.categoryFilter !== undefined && cand.category !== this.categoryFilter) {
          return false;
        }
        if (normalizedQuery.length > 0) {
          const hayZh = `${cand.titleZh} ${cand.snippetZh}`.toLowerCase();
          const hayEn = `${cand.titleEn} ${cand.snippetEn}`.toLowerCase();
          if (!hayZh.includes(normalizedQuery) && !hayEn.includes(normalizedQuery)) {
            return false;
          }
        }
        return true;
      })
      .slice(0, clampedLimit)
      .map((cand): SpotlightCandidateItem => ({
        ...cand,
        authorityGrade: "NON_AUTHORITATIVE",
        restrictionsBitmask: SPOTLIGHT_STATUTORY_MASK_0X1F,
        allowedVerbs: prunedSearchVerbs,
        l3Diagnostics: {
          ...cand.l3Diagnostics,
          authorityTag: "NON_AUTHORITATIVE · mask=0x1F",
          restrictionsBitmaskHex: "0x1F",
        },
      }));

    const scopeNoticeZh = hasDegradedShards
      ? "部分远端或隔离节点未参与检索，当前显示本机与在线节点候选（点击候选将核验最新状态）。"
      : "本机与协同节点索引已同步（搜索结果仅供预览，点击后将跳转并核验最新状态）。";
    const scopeNoticeEn = hasDegradedShards
      ? "Some remote or isolated nodes are excluded; showing local and online candidates (click to verify latest status)."
      : "Local and federated index synced (search hits are preview-only; clicking jumps and verifies latest status).";

    const l3ConsistencyLevel = hasDegradedShards
      ? "BOUNDED_STALENESS_LOCAL_AUTONOMY"
      : "FEDERATED_CHECKPOINT_SYNCED";

    const l3CheckpointDigestHex =
      this.candidates[0]?.l3Diagnostics.checkpointDigestHex ?? "0".repeat(64);

    return {
      normalizedQuery,
      candidates: matched,
      envelopeAuthorityGrade: "NON_AUTHORITATIVE",
      envelopeRestrictionsBitmask: SPOTLIGHT_STATUTORY_MASK_0X1F,
      hasDegradedShards,
      scopeNoticeZh,
      scopeNoticeEn,
      shardStatuses: this.shards.map((s) => ({ ...s })),
      l3ConsistencyLevel,
      l3CheckpointDigestHex,
      l3RetractedFilteredCount: this.retractedFilteredCount,
    };
  }

  /**
   * Statutory Lock #1 (`0x1F` Read-Only Non-Authoritative Guarantee):
   * Any attempt to directly mutate state from a search hit is unconditionally rejected,
   * even if the caller passes a tampered `callerSuppliedMask: 0x00`.
   */
  public attemptMutationFromSearchHit(request: AttemptMutationFromSearchHitRequest): never {
    this.audit.search0x1fMutationRejections += 1;
    switch (request.attemptedGate) {
      case "CAPABILITY_GRANT":
        this.audit.searchForbidCapabilityGrantHits += 1;
        break;
      case "RESOURCE_LEDGER_DEDUCTION":
        this.audit.searchForbidLedgerDeductionHits += 1;
        break;
      case "TASK_COMMIT":
        this.audit.searchForbidTaskCommitHits += 1;
        break;
      case "APPLICATION_UPDATE":
        this.audit.searchForbidAppUpdateHits += 1;
        break;
      case "IRREVERSIBLE_ACTION":
        this.audit.searchForbidIrreversibleActionHits += 1;
        break;
    }

    throw new SpotlightError(
      "SEARCH_RESULT_NON_AUTHORITATIVE_0X1F",
      `Statutory Lock #1 violation: search candidate '${request.candidateIdHex}' carries NON_AUTHORITATIVE 0x1F lock (gate=${request.attemptedGate}, callerSuppliedMask=${String(request.callerSuppliedMask)}); direct mutation is forbidden — jump to authoritative store first`,
    );
  }

  /**
   * Authoritative jump & re-verification (`verify_spotlight_candidate_authority`).
   * Re-checks the owning store before opening the target view or unlocking authoritative verbs.
   */
  public verifySpotlightCandidateAuthority(
    request: VerifySpotlightCandidateAuthorityRequest,
  ): SpotlightAuthorityVerificationReceipt {
    if (
      !isValidHex32(request.candidateIdHex) ||
      !isValidHex32(request.expectedTargetIdHex)
    ) {
      throw new SpotlightError(
        "INVALID_ARGUMENT",
        "candidateIdHex and expectedTargetIdHex must be 32 hex chars",
      );
    }

    const candidate = this.candidates.find(
      (c) => c.candidateIdHex.toLowerCase() === request.candidateIdHex.trim().toLowerCase(),
    );
    if (candidate === undefined) {
      throw new SpotlightError(
        "SEARCH_CANDIDATE_NOT_FOUND",
        `Spotlight candidate '${request.candidateIdHex}' not found`,
      );
    }

    if (
      candidate.targetIdHex.toLowerCase() !== request.expectedTargetIdHex.trim().toLowerCase()
    ) {
      throw new SpotlightError(
        "INVALID_ARGUMENT",
        `expectedTargetIdHex '${request.expectedTargetIdHex}' mismatches candidate targetIdHex '${candidate.targetIdHex}'`,
      );
    }

    const authRecord = this.authoritativeRecords.get(candidate.targetIdHex.toLowerCase());
    if (authRecord === undefined) {
      throw new SpotlightError(
        "SEARCH_CANDIDATE_NOT_FOUND",
        `Authoritative object '${candidate.targetIdHex}' not found in store`,
      );
    }

    if (authRecord.retracted) {
      this.audit.searchAuthorityRetractedRejections += 1;
      throw new SpotlightError(
        "AUTHORITY_RECORD_RETRACTED",
        `Authoritative record '${candidate.targetIdHex}' has been retracted since search index snapshot`,
      );
    }

    if (request.snapshotGeneration < authRecord.authoritativeGeneration) {
      this.audit.searchStaleGenerationRejections += 1;
      throw new SpotlightError(
        "STALE_GENERATION",
        `Search candidate snapshotGeneration ${request.snapshotGeneration} is older than authoritativeGeneration ${authRecord.authoritativeGeneration}`,
      );
    }

    this.audit.searchAuthorityVerificationsSucceeded += 1;

    const authoritativeStoreKind = STORE_KIND_BY_CATEGORY[candidate.category];
    const targetViewId = TARGET_VIEW_BY_CATEGORY[candidate.category];
    const verificationDigestHex = computeDeterministicDigest64Hex(
      "LLMOS_STAGE_D_SPOTLIGHT_VERIFY_RECEIPT_V1",
      [
        candidate.candidateIdHex,
        candidate.targetIdHex,
        authoritativeStoreKind,
        String(authRecord.authoritativeGeneration),
      ],
    );

    const receipt: SpotlightAuthorityVerificationReceipt = {
      candidateIdHex: candidate.candidateIdHex,
      targetIdHex: candidate.targetIdHex,
      category: candidate.category,
      verified: true,
      authoritativeStoreKind,
      targetViewId,
      authoritativeGeneration: authRecord.authoritativeGeneration,
      verificationDigestHex,
      unlockedAuthoritativeVerbs: [...authRecord.unlockedVerbs],
      messageZh: "已通过权威存储核验，正在打开最新目标视图。",
      messageEn: "Verified against authoritative store; opening latest target view.",
    };

    this.lastVerificationReceipt = receipt;
    return receipt;
  }

  public retractAuthorityRecord(targetIdHex: string): boolean {
    const rec = this.authoritativeRecords.get(targetIdHex.trim().toLowerCase());
    if (rec === undefined) {
      return false;
    }
    rec.retracted = true;
    this.retractedFilteredCount += 1;
    return true;
  }

  public advanceAuthorityGeneration(targetIdHex: string): number | undefined {
    const rec = this.authoritativeRecords.get(targetIdHex.trim().toLowerCase());
    if (rec === undefined) {
      return undefined;
    }
    rec.authoritativeGeneration += 1;
    return rec.authoritativeGeneration;
  }

  public getSecurityAudit(): Readonly<SpotlightSecurityAuditCounters> {
    return { ...this.audit };
  }

  public getState(): SpotlightViewState {
    return {
      open: this.open,
      disclosure: this.disclosure,
      locale: this.locale,
      queryText: this.queryText,
      categoryFilter: this.categoryFilter,
      response: this.querySpotlightCandidates(),
      lastVerificationReceipt: this.lastVerificationReceipt,
      audit: this.getSecurityAudit(),
    };
  }
}

export function renderSpotlightViewHtml(state: SpotlightViewState): string {
  const isEn = state.locale === "en-US";
  const isL3 = state.disclosure === "L3";
  const placeholder = isEn
    ? "Search tasks, local artifacts, apps, settings, notifications..."
    : "搜索任务、本地制品、受管应用、系统设置、通知历史...";
  const jumpBtnLabel = isEn ? "Jump & Verify" : "跳转并核验";
  const allChipLabel = isEn ? "All" : "全部";
  const scopeNotice = isEn
    ? state.response.scopeNoticeEn
    : state.response.scopeNoticeZh;

  const lockBadgeHtml = renderStatutoryLockBadgeHtml({
    kind: "SEARCH_0X1F_READ_ONLY",
    disclosure: state.disclosure,
    locale: state.locale,
  });

  const categories: readonly SpotlightCandidateCategory[] = [
    "TASK",
    "ARTIFACT",
    "APP",
    "SETTING",
    "NOTIFICATION",
  ];

  const chipsHtml = [
    `<button type="button" class="nlos-spotlight__cat-chip${state.categoryFilter === undefined ? " nlos-spotlight__cat-chip--active" : ""}" data-category="ALL">${escapeHtml(allChipLabel)}</button>`,
    ...categories.map((cat) => {
      const label = isEn ? CATEGORY_LABELS_EN[cat] : CATEGORY_LABELS_ZH[cat];
      const activeClass =
        state.categoryFilter === cat ? " nlos-spotlight__cat-chip--active" : "";
      return `<button type="button" class="nlos-spotlight__cat-chip${activeClass}" data-category="${cat}">${escapeHtml(label)}</button>`;
    }),
  ].join("");

  const itemsHtml = state.response.candidates
    .map((cand) => {
      const catLabel = isEn
        ? CATEGORY_LABELS_EN[cand.category]
        : CATEGORY_LABELS_ZH[cand.category];
      const title = isEn ? cand.titleEn : cand.titleZh;
      const snippet = isEn ? cand.snippetEn : cand.snippetZh;
      const hint = isEn ? cand.candidateHintEn : cand.candidateHintZh;

      const l3ItemHtml = isL3
        ? `<div class="nlos-spotlight-item__l3 nlos-l3-only" data-l3-diagnostics="true">` +
          `<span>${escapeHtml(cand.l3Diagnostics.authorityTag)}</span>` +
          `<span>cell=${escapeHtml(cand.l3Diagnostics.sourceCellIdHex.slice(0, 12))}</span>` +
          `<span>ckpt=${escapeHtml(cand.l3Diagnostics.checkpointDigestHex.slice(0, 16))}</span>` +
          `<span>gen=${cand.snapshotGeneration}</span>` +
          `<span>score=${cand.l3Diagnostics.scorePermille}‰</span>` +
          `</div>`
        : "";

      return (
        `<article class="nlos-spotlight-item" data-candidate-id="${escapeHtml(cand.candidateIdHex)}" data-target-id="${escapeHtml(cand.targetIdHex)}" data-allowed-verbs="${escapeHtml(cand.allowedVerbs.join(","))}">` +
        `<div class="nlos-spotlight-item__row">` +
        `<div class="nlos-spotlight-item__title-wrap">` +
        `<span class="nlos-spotlight-item__category">${escapeHtml(catLabel)}</span>` +
        `<h4 class="nlos-spotlight-item__title">${escapeHtml(title)}</h4>` +
        `</div>` +
        `${lockBadgeHtml}` +
        `</div>` +
        `<p class="nlos-spotlight-item__snippet">${escapeHtml(snippet)}</p>` +
        `<div class="nlos-spotlight-item__footer">` +
        `<span class="nlos-spotlight-item__hint">${escapeHtml(hint)}</span>` +
        `<button type="button" class="nlos-spotlight-item__jump-btn" data-action="jump-to-authority" data-candidate-id="${escapeHtml(cand.candidateIdHex)}" data-target-id="${escapeHtml(cand.targetIdHex)}">${renderIconSvg("external-jump", { size: 14 })}<span>${escapeHtml(jumpBtnLabel)}</span></button>` +
        `</div>` +
        `${l3ItemHtml}` +
        `</article>`
      );
    })
    .join("");

  const footerHtml = isL3
    ? `<span class="nlos-l3-only">NON_AUTHORITATIVE · mask=0x1F · consistency=${escapeHtml(state.response.l3ConsistencyLevel)} · ckpt=${escapeHtml(state.response.l3CheckpointDigestHex.slice(0, 16))} · retracted_filtered=${state.response.l3RetractedFilteredCount}</span>`
    : `<span>${isEn ? "Results are preview-only; clicking verifies latest status with source" : "搜索结果仅供只读预览，点击后自动跳转并核验最新状态"}</span>`;

  const degradedClass = state.response.hasDegradedShards
    ? " nlos-spotlight__scope-banner--degraded"
    : "";

  return (
    `<section class="nlos-spotlight-overlay" role="dialog" aria-label="${isEn ? "Global Search" : "全局搜索"}" data-z-layer="Z3" data-z-index="${Z3_SPOTLIGHT_Z_INDEX}" data-disclosure="${state.disclosure}">` +
    `<header class="nlos-spotlight__search-bar">` +
    `${renderIconSvg("search", { size: 16 })}` +
    `<input type="search" class="nlos-spotlight__input" value="${escapeHtml(state.queryText)}" placeholder="${escapeHtml(placeholder)}" aria-label="${escapeHtml(placeholder)}" />` +
    `<span class="nlos-spotlight__shortcut-badge">Ctrl+K</span>` +
    `</header>` +
    `<div class="nlos-spotlight__scope-banner${degradedClass}">` +
    `<span>${escapeHtml(scopeNotice)}</span>` +
    `${lockBadgeHtml}` +
    `</div>` +
    `<nav class="nlos-spotlight__categories" aria-label="${isEn ? "Search categories" : "搜索分类"}">` +
    `${chipsHtml}` +
    `</nav>` +
    `<div class="nlos-spotlight__results" role="listbox">` +
    `${itemsHtml}` +
    `</div>` +
    `<footer class="nlos-spotlight__footer">` +
    `${footerHtml}` +
    `</footer>` +
    `</section>`
  );
}

export function createSpotlightViewElement(
  controller: SpotlightController,
  doc: Document = document,
): HTMLElement {
  const wrapper = doc.createElement("div");
  wrapper.className = "nlos-spotlight-host";
  wrapper.innerHTML = renderSpotlightViewHtml(controller.getState());
  return wrapper;
}
