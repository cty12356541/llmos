// Deterministic Progressive Intent Compiler, Dependency Binding Verifier, & Ambiguity Hard Gate
// Authority:
//   - docs/design/06-架构设计总纲-v0.5.md §24.1 ([NL-COMPILE-001], [NL-PROPOSE-001], [NL-AMBIG-001], [NL-BIND-001])
//   - docs/design/06-架构设计总纲-v0.5.md §24.3 ([RCPT-UNDO-001]), §25.4 ([DUI-LAUNCH-001])
//   - docs/superpowers/specs/2026-10-26-stage-d-desktop-visual-and-ux-design.md §0.1, §2.4, §2A, §3.3
// Invariants:
//   - Compiles natural-language utterance + multimodal attachments + session/workspace context
//     into a versioned TypedIntentEnvelope (nlos.intent.v1) + ActionProposalPreview (nlos.action-proposal.v1).
//   - L1/L2 visible fields strictly contain zero internal architecture jargon across zh-CN & en-US.
//   - [NL-BIND-001] / U-7/U-8: Rejects symbolic "latest" tags, non-authoritative search hits,
//     PackageId/ProcessId confusion, and stale generations as direct execution dependencies.
//   - [NL-AMBIG-001]: Physically blocks submission with ClarificationRequiredError until an explicit
//     candidate row is chosen, binding selectedCandidateId + clarificationDigestHex into the proposal.

import {
  formatPlainTaintSummary,
  mergeAttachmentTaintFlags,
  normalizeTaintFlags,
  type AttachmentChipModel,
  type TaintFlag,
} from "../../multimodal";
import type { WorkstationLocale } from "../../ui";

export const TYPED_INTENT_SCHEMA_ID = "nlos.intent.v1" as const;
export const TYPED_INTENT_SCHEMA_VERSION = "1.0" as const;
export const ACTION_PROPOSAL_SCHEMA_ID = "nlos.action-proposal.v1" as const;

export type CompiledIntentKind =
  | "LAUNCH_APPLICATION"
  | "BACKGROUND_TASK"
  | "CONTROL_OPERATION"
  | "INSPECT_READONLY";

export type ProposalUndoClass = "LOSSLESS" | "COMPENSATABLE" | "IRREVERSIBLE";

export type CandidateSideEffectLevel =
  | "READ_ONLY"
  | "REVERSIBLE_WRITE"
  | "COMPENSATABLE_EXTERNAL"
  | "IRREVERSIBLE_DESTRUCTIVE";

export type UnresolvedAmbiguityKind =
  | "HIGH_RISK_IRREVERSIBLE_EFFECT"
  | "CROSS_APPLICATION_TRANSFER"
  | "MULTIPLE_MATCHING_TARGETS"
  | "BUDGET_ESCALATION";

export type IntentBindingErrorCode =
  | "EMPTY_UTTERANCE"
  | "MUTABLE_LATEST_TAG_FORBIDDEN"
  | "NON_AUTHORITATIVE_SEARCH_HIT_FORBIDDEN"
  | "PACKAGE_ID_NOT_APPLICATION_IDENTITY"
  | "PROCESS_ID_NOT_APPLICATION_IDENTITY"
  | "STALE_TARGET_GENERATION"
  | "UNVERIFIED_DEPENDENCY_HANDLE";

export class IntentBindingError extends Error {
  public readonly code: IntentBindingErrorCode;
  public readonly userMessage: string;
  public readonly diagnosticDetail: string;

  public constructor(
    code: IntentBindingErrorCode,
    userMessage: string,
    diagnosticDetail: string,
  ) {
    super(userMessage);
    this.name = "IntentBindingError";
    this.code = code;
    this.userMessage = userMessage;
    this.diagnosticDetail = diagnosticDetail;
  }
}

export class ClarificationRequiredError extends Error {
  public readonly code = "CLARIFICATION_REQUIRED" as const;
  public readonly ambiguityKinds: readonly UnresolvedAmbiguityKind[];
  public readonly candidateIds: readonly string[];

  public constructor(
    message: string,
    ambiguityKinds: readonly UnresolvedAmbiguityKind[],
    candidateIds: readonly string[],
  ) {
    super(message);
    this.name = "ClarificationRequiredError";
    this.ambiguityKinds = ambiguityKinds;
    this.candidateIds = candidateIds;
  }
}

export interface VerifiedApplicationCatalogItem {
  readonly applicationIdHex: string;
  readonly installationIdHex: string;
  readonly packageIdHex?: string;
  readonly knownProcessIdHexes?: readonly string[];
  readonly currentGeneration: number;
  readonly displayName: string;
  readonly keywords?: readonly string[];
  readonly defaultScopeSummary?: string;
}

export interface ExplicitDependencyPinInput {
  readonly rawIdentifierHex?: string;
  readonly applicationIdHex?: string;
  readonly installationIdHex?: string;
  readonly expectedGeneration?: number;
  readonly versionTag?: string;
  readonly fromNonAuthoritativeSearchHit?: boolean;
  readonly searchRestrictionBits?: number;
  readonly artifactIdHex?: string;
  readonly artifactContentDigestHex?: string;
  readonly artifactGeneration?: number;
}

export interface ResolvedApplicationHandle {
  readonly applicationIdHex: string;
  readonly installationIdHex: string;
  readonly generation: number;
  readonly displayName: string;
}

export interface ResolvedArtifactHandle {
  readonly artifactIdHex: string;
  readonly contentDigestHex: string;
  readonly generation: number;
  readonly displayLabel: string;
}

export interface UnresolvedAmbiguity {
  readonly kind: UnresolvedAmbiguityKind;
  readonly plainReason: string;
}

export interface CandidateInterpretationRow {
  readonly candidateId: string;
  readonly targetAppName: string;
  readonly targetApplicationIdHex: string;
  readonly targetInstallationIdHex: string;
  readonly targetGeneration: number;
  readonly affectedScopeSummary: string;
  readonly sideEffectLevel: CandidateSideEffectLevel;
  readonly sideEffectLabel: string;
  readonly undoClass: ProposalUndoClass;
  readonly undoBadgeLabel: string;
  readonly budgetCeilingUnits: number;
  readonly budgetImpactSummary: string;
  readonly candidateDigestHex: string;
}

export interface TypedIntentEnvelope {
  readonly schemaId: typeof TYPED_INTENT_SCHEMA_ID;
  readonly schemaVersion: typeof TYPED_INTENT_SCHEMA_VERSION;
  readonly intentIdHex: string;
  readonly intentKind: CompiledIntentKind;
  readonly normalizedUtterance: string;
  readonly workspaceIdHex: string;
  readonly principalIdHex: string;
  readonly sessionGeneration: number;
  readonly controlScope: "SYSTEM" | "WORKSPACE" | "APPLICATION" | "TASK" | "OPERATION";
  readonly idempotencyKeyHex: string;
  readonly resolvedApplication: ResolvedApplicationHandle | null;
  readonly resolvedArtifacts: readonly ResolvedArtifactHandle[];
  readonly compiledControlCommandPreview: string;
}

export interface ProposalBudgetPreview {
  readonly actualUsage: number;
  readonly effectiveCap: number;
  readonly requestedCeiling: number;
  readonly unit: string;
}

export interface ActionProposalPreview {
  readonly proposalSchemaId: typeof ACTION_PROPOSAL_SCHEMA_ID;
  readonly proposalDigestHex: string;
  readonly targetApplicationLabel: string;
  readonly affectedScopeLabel: string;
  readonly resourceCeilingLabel: string;
  readonly undoClass: ProposalUndoClass;
  readonly undoBadgeLabel: string;
  readonly compensationNote: string;
  readonly dependencySummaryLabel: string;
  readonly effectiveTaintFlags: readonly TaintFlag[];
  readonly plainTaintSummary: string | null;
  readonly budgetPreview: ProposalBudgetPreview;
  readonly requiresZ4Consent: boolean;
  readonly unresolvedAmbiguities: readonly UnresolvedAmbiguity[];
  readonly candidates: readonly CandidateInterpretationRow[];
  readonly selectedCandidateId: string | null;
  readonly clarificationDigestHex: string | null;
  readonly canSubmit: boolean;
}

export interface CompiledIntentBundle {
  readonly typedIntent: TypedIntentEnvelope;
  readonly proposal: ActionProposalPreview;
}

export interface SubmittedActionProposal {
  readonly typedIntent: TypedIntentEnvelope;
  readonly proposal: ActionProposalPreview;
  readonly selectedCandidateId: string | null;
  readonly clarificationDigestHex: string | null;
  readonly handoffTarget: "Z4_TRUSTED_CONSENT" | "DIRECT_BACKGROUND_TASK";
}

export interface IntentCompileInput {
  readonly utterance: string;
  readonly attachments?: readonly AttachmentChipModel[];
  readonly catalog?: readonly VerifiedApplicationCatalogItem[];
  readonly explicitDependency?: ExplicitDependencyPinInput;
  readonly workspaceIdHex?: string;
  readonly workspaceName?: string;
  readonly principalIdHex?: string;
  readonly sessionGeneration?: number;
  readonly effectiveBudgetCapUnits?: number;
  readonly currentBudgetUsageUnits?: number;
  readonly requestedBudgetOverrideUnits?: number;
  readonly selectedCandidateId?: string | null;
  readonly locale?: WorkstationLocale;
}

const HEX_32_RE = /^[0-9a-f]{32}$/i;
const HEX_64_RE = /^[0-9a-f]{64}$/i;

function fnv1a32(input: string, seed: number): number {
  let hash = seed >>> 0;
  for (let i = 0; i < input.length; i += 1) {
    hash ^= input.charCodeAt(i);
    hash = Math.imul(hash, 0x01000193) >>> 0;
  }
  return hash >>> 0;
}

export function deriveDeterministicHex(
  domainTag: string,
  payload: string,
  hexChars: 32 | 64,
): string {
  const words = hexChars === 32 ? 4 : 8;
  const parts: string[] = [];
  for (let idx = 0; idx < words; idx += 1) {
    const seed = (0x811c9dc5 ^ Math.imul(idx + 1, 0x9e3779b9)) >>> 0;
    const h = fnv1a32(`${domainTag}|${idx}|${payload}`, seed);
    parts.push(h.toString(16).padStart(8, "0"));
  }
  return parts.join("").slice(0, hexChars);
}

export function formatUndoBadgeLabel(
  undoClass: ProposalUndoClass,
  locale: WorkstationLocale,
): string {
  if (locale === "en-US") {
    switch (undoClass) {
      case "LOSSLESS":
        return "Losslessly Reversible";
      case "COMPENSATABLE":
        return "Compensatable Rollback";
      case "IRREVERSIBLE":
        return "Irreversible";
    }
  }
  switch (undoClass) {
    case "LOSSLESS":
      return "可无损撤销";
    case "COMPENSATABLE":
      return "可补偿回滚";
    case "IRREVERSIBLE":
      return "不可逆";
  }
}

function formatSideEffectLabel(
  level: CandidateSideEffectLevel,
  locale: WorkstationLocale,
): string {
  if (locale === "en-US") {
    switch (level) {
      case "READ_ONLY":
        return "Read-only inspection";
      case "REVERSIBLE_WRITE":
        return "Local reversible draft";
      case "COMPENSATABLE_EXTERNAL":
        return "Cross-app or external write (rollback available)";
      case "IRREVERSIBLE_DESTRUCTIVE":
        return "Permanent deletion or external send";
    }
  }
  switch (level) {
    case "READ_ONLY":
      return "只读查看";
    case "REVERSIBLE_WRITE":
      return "本地可撤销草稿";
    case "COMPENSATABLE_EXTERNAL":
      return "跨应用或外部写入（支持补偿回退）";
    case "IRREVERSIBLE_DESTRUCTIVE":
      return "永久删除或不可撤回外发";
  }
}

function formatCompensationNote(
  undoClass: ProposalUndoClass,
  locale: WorkstationLocale,
): string {
  if (locale === "en-US") {
    switch (undoClass) {
      case "LOSSLESS":
        return "Can be reverted cleanly without residual workspace changes.";
      case "COMPENSATABLE":
        return "Produces external changes that can be rolled back via a compensating step.";
      case "IRREVERSIBLE":
        return "Cannot be undone once executed; requires explicit system security confirmation.";
    }
  }
  switch (undoClass) {
    case "LOSSLESS":
      return "执行后可完整恢复至原工作区状态，不留残余改动。";
    case "COMPENSATABLE":
      return "涉及外部或跨应用变更，失败或回退时将执行补偿恢复步骤。";
    case "IRREVERSIBLE":
      return "一旦执行无法撤回，提交后必须经过系统安全确认屏二次核准。";
  }
}

const MUTABLE_SYMBOLIC_VERSION_RE =
  /(?:^|[\s:@=/])(?:latest|head|\*)(?:$|[\s,.;/])/i;

function verifyExplicitDependencyBinding(
  utterance: string,
  pin: ExplicitDependencyPinInput | undefined,
  catalog: readonly VerifiedApplicationCatalogItem[],
  locale: WorkstationLocale,
): ResolvedApplicationHandle | null {
  const mutableVersionRequested =
    (pin?.versionTag !== undefined &&
      /^(?:latest|head|\*)$/i.test(pin.versionTag.trim())) ||
    /(?:@latest|:latest|\buse\s+latest\b|使用\s*latest\b)/i.test(utterance);

  if (mutableVersionRequested || MUTABLE_SYMBOLIC_VERSION_RE.test(pin?.versionTag ?? "")) {
    throw new IntentBindingError(
      "MUTABLE_LATEST_TAG_FORBIDDEN",
      locale === "en-US"
        ? "Symbolic 'latest' version references cannot be authorized directly; pin an installed application revision."
        : "不允许直接授权可变的浮动版本标记，请绑定已安装的确定应用修订版本。",
      "NL-BIND-001 rejected symbolic mutable version tag ('latest'/'*'/'head')",
    );
  }

  if (
    pin?.fromNonAuthoritativeSearchHit === true ||
    (pin?.searchRestrictionBits !== undefined && pin.searchRestrictionBits !== 0)
  ) {
    throw new IntentBindingError(
      "NON_AUTHORITATIVE_SEARCH_HIT_FORBIDDEN",
      locale === "en-US"
        ? "Search preview entries cannot act as direct execution permission; verify the target application first."
        : "搜索预览条目不能直接作为执行授权依据，请先解析并核验目标应用身份。",
      "NL-BIND-001 / U-7/U-8 rejected non-authoritative search hit as direct execution handle",
    );
  }

  if (!pin) {
    return null;
  }

  const candidateRaw = (pin.rawIdentifierHex ?? pin.applicationIdHex ?? "").trim().toLowerCase();
  if (candidateRaw.length > 0) {
    for (const entry of catalog) {
      if (entry.packageIdHex && entry.packageIdHex.toLowerCase() === candidateRaw) {
        throw new IntentBindingError(
          "PACKAGE_ID_NOT_APPLICATION_IDENTITY",
          locale === "en-US"
            ? "Package archive identifiers cannot be used to launch or target an application."
            : "软件包归档标识不能直接用于启动或指定目标应用。",
          `DUI-LAUNCH-001 rejected PackageId ${candidateRaw} as ApplicationId`,
        );
      }
      if (
        entry.knownProcessIdHexes?.some(
          (pid) => pid.toLowerCase() === candidateRaw,
        )
      ) {
        throw new IntentBindingError(
          "PROCESS_ID_NOT_APPLICATION_IDENTITY",
          locale === "en-US"
            ? "Transient process identifiers cannot be used to target an application."
            : "瞬态运行进程标识不能直接作为目标应用身份。",
          `DUI-LAUNCH-001 rejected ProcessId ${candidateRaw} as ApplicationId`,
        );
      }
    }
  }

  if (pin.applicationIdHex !== undefined || pin.installationIdHex !== undefined) {
    const appHex = (pin.applicationIdHex ?? "").trim().toLowerCase();
    const instHex = (pin.installationIdHex ?? "").trim().toLowerCase();
    if (!HEX_32_RE.test(appHex) || !HEX_32_RE.test(instHex)) {
      throw new IntentBindingError(
        "UNVERIFIED_DEPENDENCY_HANDLE",
        locale === "en-US"
          ? "Target application handle is incomplete or not yet verified."
          : "目标应用句柄不完整或尚未通过核验。",
        `Invalid hex handle tuple (${appHex}, ${instHex})`,
      );
    }

    const matched = catalog.find(
      (item) =>
        item.applicationIdHex.toLowerCase() === appHex &&
        item.installationIdHex.toLowerCase() === instHex,
    );
    if (!matched) {
      throw new IntentBindingError(
        "UNVERIFIED_DEPENDENCY_HANDLE",
        locale === "en-US"
          ? "Requested application installation is not registered in the current workspace."
          : "所请求的应用安装实例未在当前工作区登记。",
        `Catalog miss for (${appHex}, ${instHex})`,
      );
    }

    if (
      pin.expectedGeneration !== undefined &&
      pin.expectedGeneration < matched.currentGeneration
    ) {
      throw new IntentBindingError(
        "STALE_TARGET_GENERATION",
        locale === "en-US"
          ? "Target application was upgraded in the background; refresh the proposal before submitting."
          : "目标应用已在后台升级至新修订版，请刷新意图预览后再提交。",
        `Stale target generation ${pin.expectedGeneration} < current ${matched.currentGeneration}`,
      );
    }

    return {
      applicationIdHex: matched.applicationIdHex.toLowerCase(),
      installationIdHex: matched.installationIdHex.toLowerCase(),
      generation: matched.currentGeneration,
      displayName: matched.displayName,
    };
  }

  return null;
}

function collectVerifiedArtifacts(
  attachments: readonly AttachmentChipModel[],
  explicitPin: ExplicitDependencyPinInput | undefined,
  locale: WorkstationLocale,
): readonly ResolvedArtifactHandle[] {
  const handles: ResolvedArtifactHandle[] = [];
  for (const att of attachments) {
    if (att.kind === "artifact" && att.artifactIdHex) {
      handles.push({
        artifactIdHex: att.artifactIdHex,
        contentDigestHex: att.contentDigestHex,
        generation: 1,
        displayLabel: att.displayLabel,
      });
    }
  }
  if (explicitPin?.artifactIdHex !== undefined) {
    const idHex = explicitPin.artifactIdHex.trim().toLowerCase();
    const digestHex = (explicitPin.artifactContentDigestHex ?? "").trim().toLowerCase();
    if (!HEX_32_RE.test(idHex) || !HEX_64_RE.test(digestHex) || /^0+$/.test(digestHex)) {
      throw new IntentBindingError(
        "UNVERIFIED_DEPENDENCY_HANDLE",
        locale === "en-US"
          ? "Saved output reference must include both a verified identifier and content checksum."
          : "引用已保存结果时必须同时具备已核验标识与内容校验值。",
        "NL-BIND-001 requires both ArtifactId (32-hex) and non-zero ContentDigest (64-hex)",
      );
    }
    handles.push({
      artifactIdHex: idHex,
      contentDigestHex: digestHex,
      generation: explicitPin.artifactGeneration ?? 1,
      displayLabel: `ref-${idHex.slice(0, 8)}`,
    });
  }
  return handles;
}

const DESTRUCTIVE_RE =
  /(?:\b(?:delete|remove|purge|wipe|overwrite|uninstall|drop|destroy|send\s+external|publish)\b|删除|清空|永久移除|覆盖|卸载|外发|公开导出)/i;
const CROSS_APP_RE =
  /(?:\b(?:forward\s+to|transfer\s+to|export\s+to|share\s+with|send\s+to|sync\s+to)\b|转发到|同步到|发送到|导入到|跨应用)/i;
const BUDGET_ESCALATION_RE =
  /(?:\b(?:unlimited|escalate\s+budget|raise\s+limit|full\s+cluster|high\s+compute)\b|扩容预算|提升上限|高算力|全量批处理)/i;
const READONLY_RE =
  /(?:\b(?:inspect|check|show|list|view|summarize|preview)\b|查看|检查|列出|预览|汇总)/i;
const LAUNCH_RE =
  /(?:\b(?:open|launch|start|focus)\b|打开|启动|切换到)/i;
const CONTROL_OP_RE =
  /(?:\b(?:pause|resume|cancel|throttle|reclaim|acknowledge)\b|暂停|恢复|取消|限流|回收)/i;

function matchCatalogApplications(
  utterance: string,
  catalog: readonly VerifiedApplicationCatalogItem[],
): readonly VerifiedApplicationCatalogItem[] {
  if (catalog.length === 0) {
    return [];
  }
  const lower = utterance.toLowerCase();
  const matched: VerifiedApplicationCatalogItem[] = [];
  for (const entry of catalog) {
    const tokens = [entry.displayName, ...(entry.keywords ?? [])];
    if (
      tokens.some(
        (tok) => tok.trim().length > 0 && lower.includes(tok.trim().toLowerCase()),
      )
    ) {
      matched.push(entry);
    }
  }
  return matched;
}

function classifyIntentKind(utterance: string): CompiledIntentKind {
  if (READONLY_RE.test(utterance) && !DESTRUCTIVE_RE.test(utterance) && !CROSS_APP_RE.test(utterance)) {
    return "INSPECT_READONLY";
  }
  if (CONTROL_OP_RE.test(utterance)) {
    return "CONTROL_OPERATION";
  }
  if (LAUNCH_RE.test(utterance) && !CROSS_APP_RE.test(utterance) && !DESTRUCTIVE_RE.test(utterance)) {
    return "LAUNCH_APPLICATION";
  }
  return "BACKGROUND_TASK";
}

function defaultCatalogFallback(
  locale: WorkstationLocale,
): VerifiedApplicationCatalogItem {
  return {
    applicationIdHex: "11".repeat(16),
    installationIdHex: "22".repeat(16),
    currentGeneration: 1,
    displayName: locale === "en-US" ? "Workspace Assistant" : "工作区台面工具",
    defaultScopeSummary:
      locale === "en-US" ? "Current active workspace" : "当前活动工作区",
  };
}

export function compileIntentAndProposal(
  input: IntentCompileInput,
): CompiledIntentBundle {
  const locale: WorkstationLocale = input.locale ?? "zh-CN";
  const normalizedUtterance = input.utterance.trim().replace(/\s+/g, " ");
  if (normalizedUtterance.length === 0) {
    throw new IntentBindingError(
      "EMPTY_UTTERANCE",
      locale === "en-US"
        ? "Enter a command or natural-language request before previewing."
        : "请先输入命令或自然语言需求后再生成预览。",
      "Empty utterance rejected before intent compilation",
    );
  }

  const attachments = input.attachments ?? [];
  const catalog = input.catalog ?? [];
  const workspaceIdHex =
    input.workspaceIdHex && HEX_32_RE.test(input.workspaceIdHex)
      ? input.workspaceIdHex.toLowerCase()
      : "aa".repeat(16);
  const principalIdHex =
    input.principalIdHex && HEX_32_RE.test(input.principalIdHex)
      ? input.principalIdHex.toLowerCase()
      : "bb".repeat(16);
  const sessionGeneration = input.sessionGeneration ?? 1;

  const explicitApp = verifyExplicitDependencyBinding(
    normalizedUtterance,
    input.explicitDependency,
    catalog,
    locale,
  );
  const resolvedArtifacts = collectVerifiedArtifacts(
    attachments,
    input.explicitDependency,
    locale,
  );

  const matchedApps = explicitApp
    ? catalog.filter(
        (c) =>
          c.applicationIdHex.toLowerCase() === explicitApp.applicationIdHex &&
          c.installationIdHex.toLowerCase() === explicitApp.installationIdHex,
      )
    : matchCatalogApplications(normalizedUtterance, catalog);

  const intentKind = classifyIntentKind(normalizedUtterance);
  const isDestructive = DESTRUCTIVE_RE.test(normalizedUtterance);
  const baseAttachmentTaint = mergeAttachmentTaintFlags(attachments);
  const isCrossApp =
    CROSS_APP_RE.test(normalizedUtterance) ||
    baseAttachmentTaint.includes("CROSS_APPLICATION") ||
    matchedApps.length > 1;

  const effectiveCap = Math.max(10, Math.round(input.effectiveBudgetCapUnits ?? 100));
  const actualUsage = Math.max(0, Math.round(input.currentBudgetUsageUnits ?? 18));
  const requestedCeiling = Math.max(
    actualUsage,
    Math.round(
      input.requestedBudgetOverrideUnits ??
        (BUDGET_ESCALATION_RE.test(normalizedUtterance) ? effectiveCap + 50 : Math.min(effectiveCap, 48)),
    ),
  );
  const isBudgetEscalated = requestedCeiling > effectiveCap;

  const ambiguities: UnresolvedAmbiguity[] = [];
  if (matchedApps.length > 1) {
    ambiguities.push({
      kind: "MULTIPLE_MATCHING_TARGETS",
      plainReason:
        locale === "en-US"
          ? "Multiple installed applications match this request; select the intended target."
          : "有多个已安装应用匹配该指令，请明确选择目标应用。",
    });
  }
  if (isDestructive) {
    ambiguities.push({
      kind: "HIGH_RISK_IRREVERSIBLE_EFFECT",
      plainReason:
        locale === "en-US"
          ? "This action may permanently modify or delete data; choose reversible archive or confirm permanent execution."
          : "该操作可能造成不可撤回的数据变更，请选择可恢复归档或确认永久执行方案。",
    });
  }
  if (isCrossApp) {
    ambiguities.push({
      kind: "CROSS_APPLICATION_TRANSFER",
      plainReason:
        locale === "en-US"
          ? "Data crosses application boundaries; confirm whether to keep a local copy or send across applications."
          : "数据涉及跨应用流转，请明确仅在当前应用内处理还是跨应用传递。",
    });
  }
  if (isBudgetEscalated) {
    ambiguities.push({
      kind: "BUDGET_ESCALATION",
      plainReason:
        locale === "en-US"
          ? "Requested resource ceiling exceeds the current workspace allowance; select bounded mode or elevated allowance."
          : "预计资源上限超出当前工作区常规额度，请选择常规限额执行或申请提额执行。",
    });
  }

  const primaryCatalogItem =
    explicitApp !== null
      ? {
          applicationIdHex: explicitApp.applicationIdHex,
          installationIdHex: explicitApp.installationIdHex,
          currentGeneration: explicitApp.generation,
          displayName: explicitApp.displayName,
        }
      : matchedApps[0] ?? catalog[0] ?? defaultCatalogFallback(locale);

  const candidates = buildAmbiguityCandidates({
    utterance: normalizedUtterance,
    ambiguities,
    matchedApps,
    primaryItem: primaryCatalogItem,
    effectiveCap,
    requestedCeiling,
    workspaceName:
      input.workspaceName ??
      (locale === "en-US" ? "Main Workspace" : "主工作区"),
    attachments,
    locale,
  });

  const validSelectedCandidate =
    input.selectedCandidateId !== undefined && input.selectedCandidateId !== null
      ? candidates.find((c) => c.candidateId === input.selectedCandidateId) ?? null
      : null;

  const chosenApp: ResolvedApplicationHandle = validSelectedCandidate
    ? {
        applicationIdHex: validSelectedCandidate.targetApplicationIdHex,
        installationIdHex: validSelectedCandidate.targetInstallationIdHex,
        generation: validSelectedCandidate.targetGeneration,
        displayName: validSelectedCandidate.targetAppName,
      }
    : {
        applicationIdHex: primaryCatalogItem.applicationIdHex.toLowerCase(),
        installationIdHex: primaryCatalogItem.installationIdHex.toLowerCase(),
        generation: primaryCatalogItem.currentGeneration,
        displayName: primaryCatalogItem.displayName,
      };

  const finalUndoClass: ProposalUndoClass = validSelectedCandidate
    ? validSelectedCandidate.undoClass
    : isDestructive
      ? "IRREVERSIBLE"
      : isCrossApp
        ? "COMPENSATABLE"
        : "LOSSLESS";

  const finalCeilingUnits = validSelectedCandidate
    ? validSelectedCandidate.budgetCeilingUnits
    : requestedCeiling;

  const taintSet = new Set<TaintFlag>(baseAttachmentTaint);
  if (isCrossApp) {
    taintSet.add("CROSS_APPLICATION");
  }
  const effectiveTaintFlags = normalizeTaintFlags([...taintSet]);

  const affectedFilesPart =
    attachments.length > 0
      ? attachments.map((a) => a.displayLabel).join(", ")
      : locale === "en-US"
        ? "Active workspace context"
        : "当前活动工作区上下文";

  const affectedScopeLabel = validSelectedCandidate
    ? validSelectedCandidate.affectedScopeSummary
    : affectedFilesPart;

  const unitLabel = locale === "en-US" ? "units" : "单位";
  const resourceCeilingLabel =
    locale === "en-US"
      ? `Up to ${finalCeilingUnits} ${unitLabel} (Allowance ${effectiveCap} ${unitLabel})`
      : `上限 ${finalCeilingUnits} ${unitLabel}（当前可用上限 ${effectiveCap} ${unitLabel}）`;

  const dependencySummaryLabel =
    locale === "en-US"
      ? `${chosenApp.displayName} (Verified install rev #${chosenApp.generation})`
      : `${chosenApp.displayName}（已核验安装修订 #${chosenApp.generation}）`;

  const intentPayloadSeed = [
    normalizedUtterance,
    workspaceIdHex,
    principalIdHex,
    String(sessionGeneration),
    chosenApp.applicationIdHex,
    chosenApp.installationIdHex,
    String(chosenApp.generation),
    attachments.map((a) => `${a.attachmentId}:${a.contentDigestHex}`).join(","),
  ].join("|");

  const intentIdHex = deriveDeterministicHex(
    "llmos/stage-d/omnibar/intent-id/v1",
    intentPayloadSeed,
    32,
  );
  const idempotencyKeyHex = deriveDeterministicHex(
    "llmos/stage-d/omnibar/idempotency/v1",
    `${intentIdHex}|${validSelectedCandidate?.candidateId ?? "default"}`,
    32,
  );
  const proposalDigestHex = deriveDeterministicHex(
    "llmos/stage-d/omnibar/proposal-digest/v1",
    `${intentPayloadSeed}|${finalUndoClass}|${finalCeilingUnits}|${effectiveTaintFlags.join(",")}`,
    64,
  );

  const clarificationDigestHex =
    validSelectedCandidate !== null
      ? deriveDeterministicHex(
          "llmos/stage-d/omnibar/clarification-digest/v1",
          `${proposalDigestHex}|${validSelectedCandidate.candidateId}|${validSelectedCandidate.candidateDigestHex}|${sessionGeneration}`,
          64,
        )
      : null;

  const controlScope: TypedIntentEnvelope["controlScope"] =
    intentKind === "LAUNCH_APPLICATION"
      ? "APPLICATION"
      : intentKind === "CONTROL_OPERATION"
        ? "OPERATION"
        : intentKind === "INSPECT_READONLY"
          ? "WORKSPACE"
          : "TASK";

  const compiledControlCommandPreview = buildControlCommandPreview(
    intentKind,
    chosenApp,
    workspaceIdHex,
    idempotencyKeyHex,
  );

  const requiresZ4Consent =
    finalUndoClass === "IRREVERSIBLE" ||
    finalCeilingUnits > effectiveCap ||
    effectiveTaintFlags.includes("EXTERNAL_ORIGIN") ||
    effectiveTaintFlags.includes("CONFIDENTIAL_SECRET") ||
    effectiveTaintFlags.includes("CROSS_APPLICATION");

  const canSubmit =
    ambiguities.length === 0 || validSelectedCandidate !== null;

  const typedIntent: TypedIntentEnvelope = {
    schemaId: TYPED_INTENT_SCHEMA_ID,
    schemaVersion: TYPED_INTENT_SCHEMA_VERSION,
    intentIdHex,
    intentKind,
    normalizedUtterance,
    workspaceIdHex,
    principalIdHex,
    sessionGeneration,
    controlScope,
    idempotencyKeyHex,
    resolvedApplication: chosenApp,
    resolvedArtifacts,
    compiledControlCommandPreview,
  };

  const proposal: ActionProposalPreview = {
    proposalSchemaId: ACTION_PROPOSAL_SCHEMA_ID,
    proposalDigestHex,
    targetApplicationLabel: chosenApp.displayName,
    affectedScopeLabel,
    resourceCeilingLabel,
    undoClass: finalUndoClass,
    undoBadgeLabel: formatUndoBadgeLabel(finalUndoClass, locale),
    compensationNote: formatCompensationNote(finalUndoClass, locale),
    dependencySummaryLabel,
    effectiveTaintFlags,
    plainTaintSummary: formatPlainTaintSummary(effectiveTaintFlags, locale),
    budgetPreview: {
      actualUsage,
      effectiveCap,
      requestedCeiling: finalCeilingUnits,
      unit: unitLabel,
    },
    requiresZ4Consent,
    unresolvedAmbiguities: ambiguities,
    candidates,
    selectedCandidateId: validSelectedCandidate?.candidateId ?? null,
    clarificationDigestHex,
    canSubmit,
  };

  return {
    typedIntent,
    proposal,
  };
}

function buildControlCommandPreview(
  intentKind: CompiledIntentKind,
  app: ResolvedApplicationHandle,
  workspaceIdHex: string,
  idempotencyKeyHex: string,
): string {
  switch (intentKind) {
    case "LAUNCH_APPLICATION":
      return `launch_application(app=${app.applicationIdHex}, inst=${app.installationIdHex}, gen=${app.generation}, idem=${idempotencyKeyHex})`;
    case "CONTROL_OPERATION":
      return `submit_scoped_control(scope=OPERATION, ws=${workspaceIdHex}, idem=${idempotencyKeyHex})`;
    case "INSPECT_READONLY":
      return `inspect_workspace_projection(ws=${workspaceIdHex}, app=${app.applicationIdHex})`;
    case "BACKGROUND_TASK":
      return `propose_task_plan(ws=${workspaceIdHex}, app=${app.applicationIdHex}, gen=${app.generation}, idem=${idempotencyKeyHex})`;
  }
}

function buildAmbiguityCandidates(params: {
  readonly utterance: string;
  readonly ambiguities: readonly UnresolvedAmbiguity[];
  readonly matchedApps: readonly VerifiedApplicationCatalogItem[];
  readonly primaryItem: VerifiedApplicationCatalogItem;
  readonly effectiveCap: number;
  readonly requestedCeiling: number;
  readonly workspaceName: string;
  readonly attachments: readonly AttachmentChipModel[];
  readonly locale: WorkstationLocale;
}): readonly CandidateInterpretationRow[] {
  const {
    utterance,
    ambiguities,
    matchedApps,
    primaryItem,
    effectiveCap,
    requestedCeiling,
    workspaceName,
    attachments,
    locale,
  } = params;

  if (ambiguities.length === 0) {
    return [];
  }

  const unitLabel = locale === "en-US" ? "units" : "单位";
  const attachmentNames =
    attachments.length > 0
      ? attachments.map((a) => a.displayLabel).join(", ")
      : workspaceName;

  if (matchedApps.length >= 2) {
    return matchedApps.slice(0, 4).map((app, index) => {
      const candidateId = `cand-target-${index + 1}`;
      const scopeSummary =
        app.defaultScopeSummary ??
        (locale === "en-US"
          ? `${app.displayName} in ${workspaceName}`
          : `${workspaceName} · ${app.displayName}`);
      const undoClass: ProposalUndoClass = DESTRUCTIVE_RE.test(utterance)
        ? "IRREVERSIBLE"
        : "LOSSLESS";
      const sideEffect: CandidateSideEffectLevel =
        undoClass === "IRREVERSIBLE"
          ? "IRREVERSIBLE_DESTRUCTIVE"
          : "REVERSIBLE_WRITE";
      const ceiling = Math.min(requestedCeiling, effectiveCap);
      return {
        candidateId,
        targetAppName: app.displayName,
        targetApplicationIdHex: app.applicationIdHex.toLowerCase(),
        targetInstallationIdHex: app.installationIdHex.toLowerCase(),
        targetGeneration: app.currentGeneration,
        affectedScopeSummary: scopeSummary,
        sideEffectLevel: sideEffect,
        sideEffectLabel: formatSideEffectLabel(sideEffect, locale),
        undoClass,
        undoBadgeLabel: formatUndoBadgeLabel(undoClass, locale),
        budgetCeilingUnits: ceiling,
        budgetImpactSummary:
          locale === "en-US"
            ? `${ceiling} / ${effectiveCap} ${unitLabel}`
            : `${ceiling} / ${effectiveCap} ${unitLabel}`,
        candidateDigestHex: deriveDeterministicHex(
          "llmos/stage-d/omnibar/candidate/v1",
          `${candidateId}|${app.applicationIdHex}|${app.installationIdHex}|${app.currentGeneration}|${utterance}`,
          64,
        ),
      };
    });
  }

  const boundedCeiling = Math.min(effectiveCap, Math.max(24, Math.round(effectiveCap * 0.6)));
  const conservativeId = "cand-conservative-safe";
  const escalatedId = "cand-direct-elevated";

  const conservativeScope =
    locale === "en-US"
      ? `Staged reversible copy in ${workspaceName} (${attachmentNames})`
      : `${workspaceName} 内可恢复暂存副本（${attachmentNames}）`;
  const directScope =
    locale === "en-US"
      ? `Direct execution on ${attachmentNames}`
      : `直接作用于 ${attachmentNames}`;

  const directUndo: ProposalUndoClass = DESTRUCTIVE_RE.test(utterance)
    ? "IRREVERSIBLE"
    : "COMPENSATABLE";
  const directSideEffect: CandidateSideEffectLevel =
    directUndo === "IRREVERSIBLE"
      ? "IRREVERSIBLE_DESTRUCTIVE"
      : "COMPENSATABLE_EXTERNAL";

  return [
    {
      candidateId: conservativeId,
      targetAppName: primaryItem.displayName,
      targetApplicationIdHex: primaryItem.applicationIdHex.toLowerCase(),
      targetInstallationIdHex: primaryItem.installationIdHex.toLowerCase(),
      targetGeneration: primaryItem.currentGeneration,
      affectedScopeSummary: conservativeScope,
      sideEffectLevel: "REVERSIBLE_WRITE",
      sideEffectLabel: formatSideEffectLabel("REVERSIBLE_WRITE", locale),
      undoClass: "LOSSLESS",
      undoBadgeLabel: formatUndoBadgeLabel("LOSSLESS", locale),
      budgetCeilingUnits: boundedCeiling,
      budgetImpactSummary:
        locale === "en-US"
          ? `Within allowance (${boundedCeiling} / ${effectiveCap} ${unitLabel})`
          : `常规限额内（${boundedCeiling} / ${effectiveCap} ${unitLabel}）`,
      candidateDigestHex: deriveDeterministicHex(
        "llmos/stage-d/omnibar/candidate/v1",
        `${conservativeId}|${primaryItem.applicationIdHex}|LOSSLESS|${boundedCeiling}|${utterance}`,
        64,
      ),
    },
    {
      candidateId: escalatedId,
      targetAppName: primaryItem.displayName,
      targetApplicationIdHex: primaryItem.applicationIdHex.toLowerCase(),
      targetInstallationIdHex: primaryItem.installationIdHex.toLowerCase(),
      targetGeneration: primaryItem.currentGeneration,
      affectedScopeSummary: directScope,
      sideEffectLevel: directSideEffect,
      sideEffectLabel: formatSideEffectLabel(directSideEffect, locale),
      undoClass: directUndo,
      undoBadgeLabel: formatUndoBadgeLabel(directUndo, locale),
      budgetCeilingUnits: requestedCeiling,
      budgetImpactSummary:
        requestedCeiling > effectiveCap
          ? locale === "en-US"
            ? `Requires elevated allowance (${requestedCeiling} > ${effectiveCap} ${unitLabel})`
            : `需提额核准（${requestedCeiling} > ${effectiveCap} ${unitLabel}）`
          : locale === "en-US"
            ? `${requestedCeiling} / ${effectiveCap} ${unitLabel}`
            : `${requestedCeiling} / ${effectiveCap} ${unitLabel}`,
      candidateDigestHex: deriveDeterministicHex(
        "llmos/stage-d/omnibar/candidate/v1",
        `${escalatedId}|${primaryItem.applicationIdHex}|${directUndo}|${requestedCeiling}|${utterance}`,
        64,
      ),
    },
  ];
}

export function selectAmbiguityCandidate(
  input: IntentCompileInput,
  candidateId: string,
): CompiledIntentBundle {
  return compileIntentAndProposal({
    ...input,
    selectedCandidateId: candidateId,
  });
}

export function finalizeProposalForSubmission(
  bundle: CompiledIntentBundle,
  locale: WorkstationLocale = "zh-CN",
): SubmittedActionProposal {
  const { typedIntent, proposal } = bundle;
  if (
    proposal.unresolvedAmbiguities.length > 0 &&
    (proposal.selectedCandidateId === null ||
      proposal.clarificationDigestHex === null)
  ) {
    throw new ClarificationRequiredError(
      locale === "en-US"
        ? "Select one candidate interpretation in the comparison table before continuing."
        : "存在待澄清的执行歧义，请先在对比表中选择一项候选解释后再继续。",
      proposal.unresolvedAmbiguities.map((a) => a.kind),
      proposal.candidates.map((c) => c.candidateId),
    );
  }

  return {
    typedIntent,
    proposal,
    selectedCandidateId: proposal.selectedCandidateId,
    clarificationDigestHex: proposal.clarificationDigestHex,
    handoffTarget: proposal.requiresZ4Consent
      ? "Z4_TRUSTED_CONSENT"
      : "DIRECT_BACKGROUND_TASK",
  };
}
