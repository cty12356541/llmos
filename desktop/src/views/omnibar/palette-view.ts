// On-Demand Solid Z3 Command & Intent Palette Controller, Renderer, & Compositor Adapter
// Authority:
//   - docs/design/06-架构设计总纲-v0.5.md §24.1, §24.2, §25.4, §28.4
//   - docs/management/adrs/0025-stage-d-hybrid-desktop-compositor-and-trusted-ui-layering.md
//   - docs/superpowers/specs/2026-10-26-stage-d-desktop-visual-and-ux-design.md §0.1, §2.4, §2A, §3.3
// Invariants:
//   - Hidden by default (0px permanent top footprint; never a fixed giant top bar).
//   - Summoned via Ctrl+Space (or Cmd+Space) or the 26px taskbar trigger slot at z-index: 460, 4px radius.
//   - Dismissed via Escape or outside click while preserving in-progress draft utterance & attachments
//     unless explicitly cleared.
//   - L1/L2 visible DOM/HTML contains zero internal architecture jargon across zh-CN & en-US.
//   - L3 diagnostic metadata is strictly gated behind disclosure === "L3" and class="nlos-l3-only".

import type { WorkstationCompositor } from "../../compositor/workstation-compositor";
import type { DeviceHardwareCaptureState } from "../../dock";
import {
  VoiceInputController,
  createAttachmentChipsElement,
  createVoiceInputController,
  renderAttachmentChipsHtml,
  type AttachmentChipModel,
  type HardwarePermissionGrant,
  type VoiceCaptureMode,
  type VoiceRouteCheckDecision,
  type VoiceRouteCheckInput,
  type VoiceStreamSnapshot,
  type VoiceTransitionResult,
} from "../../multimodal";
import type { DesktopSessionStateDto } from "../../types";
import {
  createBulletMicroBarElement,
  createIconElement,
  renderBulletMicroBarHtml,
  renderIconSvg,
  type DisclosureLevel,
  type WorkstationLocale,
} from "../../ui";
import {
  ClarificationRequiredError,
  IntentBindingError,
  compileIntentAndProposal,
  finalizeProposalForSubmission,
  type CompiledIntentBundle,
  type ExplicitDependencyPinInput,
  type SubmittedActionProposal,
  type VerifiedApplicationCatalogItem,
} from "./intent-compiler";

export const OMNIBAR_Z_INDEX = 460 as const;
export const OMNIBAR_BORDER_RADIUS_PX = 4 as const;
export const OMNIBAR_TASKBAR_TRIGGER_HEIGHT_PX = 26 as const;
export const OMNIBAR_TOP_PERMANENT_FOOTPRINT_PX = 0 as const;

export type OmnibarOpenSource = "keyboard-shortcut" | "taskbar-trigger" | "programmatic";
export type OmnibarCloseReason = "escape" | "outside-click" | "submitted" | "security-gate" | "programmatic";

export interface OmnibarKeyInput {
  readonly key: string;
  readonly code?: string;
  readonly ctrlKey?: boolean;
  readonly metaKey?: boolean;
  readonly altKey?: boolean;
  readonly shiftKey?: boolean;
  readonly fromUntrustedSurface?: boolean;
  readonly nowMonoMs?: number;
  readonly preventDefault?: () => void;
}

export interface OmnibarPaletteSnapshot {
  readonly open: boolean;
  readonly zIndex: typeof OMNIBAR_Z_INDEX;
  readonly borderRadiusPx: typeof OMNIBAR_BORDER_RADIUS_PX;
  readonly topPermanentFootprintPx: typeof OMNIBAR_TOP_PERMANENT_FOOTPRINT_PX;
  readonly taskbarTriggerHeightPx: typeof OMNIBAR_TASKBAR_TRIGGER_HEIGHT_PX;
  readonly lastOpenSource: OmnibarOpenSource | null;
  readonly lastCloseReason: OmnibarCloseReason | null;
  readonly locale: WorkstationLocale;
  readonly disclosure: DisclosureLevel;
  readonly sessionState: DesktopSessionStateDto;
  readonly sessionGeneration: number;
  readonly z4ModalActive: boolean;
  readonly draftUtterance: string;
  readonly attachments: readonly AttachmentChipModel[];
  readonly selectedCandidateId: string | null;
  readonly compiledBundle: CompiledIntentBundle | null;
  readonly bindingError: {
    readonly code: string;
    readonly userMessage: string;
    readonly diagnosticDetail: string;
  } | null;
  readonly lastClarificationBlockMessage: string | null;
  readonly lastSubmittedProposal: SubmittedActionProposal | null;
  readonly voice: VoiceStreamSnapshot;
}

export interface OmnibarPaletteControllerOptions {
  readonly initialLocale?: WorkstationLocale;
  readonly initialDisclosure?: DisclosureLevel;
  readonly initialSessionState?: DesktopSessionStateDto;
  readonly initialSessionGeneration?: number;
  readonly initialZ4ModalActive?: boolean;
  readonly initialMicPermission?: HardwarePermissionGrant;
  readonly workspaceIdHex?: string;
  readonly workspaceName?: string;
  readonly principalIdHex?: string;
  readonly effectiveBudgetCapUnits?: number;
  readonly currentBudgetUsageUnits?: number;
  readonly catalog?: readonly VerifiedApplicationCatalogItem[];
  readonly routeVoiceInput?: (input: VoiceRouteCheckInput) => VoiceRouteCheckDecision;
  readonly onHardwareCaptureChange?: (indicator: DeviceHardwareCaptureState) => void;
  readonly onSubmitProposal?: (submitted: SubmittedActionProposal) => void;
  readonly clearDraftOnDirectSubmit?: boolean;
  readonly doc?: Document;
}

export type OmnibarPaletteListener = (snapshot: OmnibarPaletteSnapshot) => void;

interface OmnibarCopy {
  readonly dialogAriaLabel: string;
  readonly inputPlaceholder: string;
  readonly voiceBtnLabel: string;
  readonly clearDraftLabel: string;
  readonly submitReadyLabel: string;
  readonly submitSecurityLabel: string;
  readonly submitClarifyBlockedLabel: string;
  readonly targetAppKey: string;
  readonly affectedScopeKey: string;
  readonly budgetCeilingKey: string;
  readonly dependencyKey: string;
  readonly l2CompensationKey: string;
  readonly l2SourceNoticeKey: string;
  readonly l2ResourceBarKey: string;
  readonly ambiguityHeader: string;
  readonly colCandidateApp: string;
  readonly colCandidateScope: string;
  readonly colCandidateEffect: string;
  readonly colCandidateUndo: string;
  readonly colCandidateBudget: string;
  readonly footerShortcutHint: string;
  readonly footerDraftSavedHint: string;
}

const OMNIBAR_COPY: Readonly<Record<WorkstationLocale, OmnibarCopy>> = {
  "zh-CN": {
    dialogAriaLabel: "命令与意图面板",
    inputPlaceholder: "输入命令或自然语言任务说明（Ctrl+Space 唤起 / Esc 收起并保留草稿）",
    voiceBtnLabel: "语音输入",
    clearDraftLabel: "清空草稿",
    submitReadyLabel: "提交执行",
    submitSecurityLabel: "进入系统安全确认",
    submitClarifyBlockedLabel: "请先选择候选方案",
    targetAppKey: "目标应用",
    affectedScopeKey: "作用范围",
    budgetCeilingKey: "资源上限",
    dependencyKey: "已核验依赖",
    l2CompensationKey: "回退保障",
    l2SourceNoticeKey: "数据来源标记",
    l2ResourceBarKey: "水位预算对照",
    ambiguityHeader: "检测到多义或高影响操作，请先点选一项明确解释：",
    colCandidateApp: "目标应用",
    colCandidateScope: "影响范围",
    colCandidateEffect: "变更性质",
    colCandidateUndo: "可逆等级",
    colCandidateBudget: "额度影响",
    footerShortcutHint: "Ctrl+Space 切换面板 · Esc 收起保留草稿 · Enter 提交",
    footerDraftSavedHint: "收起面板不会丢失当前草稿与附件",
  },
  "en-US": {
    dialogAriaLabel: "Command and Intent Palette",
    inputPlaceholder: "Type a command or plain-language task (Ctrl+Space to toggle / Esc keeps draft)",
    voiceBtnLabel: "Voice Input",
    clearDraftLabel: "Clear Draft",
    submitReadyLabel: "Run Proposal",
    submitSecurityLabel: "Continue to Security Check",
    submitClarifyBlockedLabel: "Select a Candidate First",
    targetAppKey: "Target App",
    affectedScopeKey: "Affected Scope",
    budgetCeilingKey: "Resource Ceiling",
    dependencyKey: "Verified Dependency",
    l2CompensationKey: "Rollback Guarantee",
    l2SourceNoticeKey: "Source Handling",
    l2ResourceBarKey: "Allowance Watermark",
    ambiguityHeader: "Multiple interpretations or high-impact changes detected — choose one row:",
    colCandidateApp: "Target App",
    colCandidateScope: "Affected Scope",
    colCandidateEffect: "Change Level",
    colCandidateUndo: "Reversibility",
    colCandidateBudget: "Allowance Impact",
    footerShortcutHint: "Ctrl+Space toggle · Esc dismiss & keep draft · Enter submit",
    footerDraftSavedHint: "Dismissing keeps your draft prompt and attachments intact",
  },
};

function escapeHtml(raw: string): string {
  return raw
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

function collectVisibleNodeText(node: Node, disclosure: DisclosureLevel): string {
  if (node.nodeType === 3) {
    return node.textContent ?? "";
  }
  const el = node as HTMLElement;
  if (el.hidden || el.getAttribute?.("hidden") !== null) {
    return "";
  }
  const cls = el.className ?? "";
  if (
    typeof cls === "string" &&
    cls.includes("nlos-l3-only") &&
    disclosure !== "L3"
  ) {
    return "";
  }
  const children = el.childNodes ?? [];
  if (children.length === 0) {
    return el.textContent ?? "";
  }
  let acc = "";
  for (let i = 0; i < children.length; i += 1) {
    const child = children[i];
    if (child) {
      acc += ` ${collectVisibleNodeText(child, disclosure)}`;
    }
  }
  return acc;
}

export class OmnibarPaletteController {
  private open = false;
  private lastOpenSource: OmnibarOpenSource | null = null;
  private lastCloseReason: OmnibarCloseReason | null = null;
  private locale: WorkstationLocale;
  private disclosure: DisclosureLevel;
  private sessionState: DesktopSessionStateDto;
  private sessionGeneration: number;
  private z4ModalActive: boolean;
  private workspaceIdHex: string;
  private workspaceName: string;
  private principalIdHex: string;
  private effectiveBudgetCapUnits: number;
  private currentBudgetUsageUnits: number;
  private requestedBudgetOverrideUnits: number | undefined = undefined;
  private catalog: VerifiedApplicationCatalogItem[];
  private explicitDependency: ExplicitDependencyPinInput | undefined = undefined;
  private draftUtterance = "";
  private attachments: AttachmentChipModel[] = [];
  private selectedCandidateId: string | null = null;
  private compiledBundle: CompiledIntentBundle | null = null;
  private bindingError: {
    readonly code: string;
    readonly userMessage: string;
    readonly diagnosticDetail: string;
  } | null = null;
  private lastClarificationBlockMessage: string | null = null;
  private lastSubmittedProposal: SubmittedActionProposal | null = null;

  public readonly voiceController: VoiceInputController;
  private readonly onSubmitProposal?: (submitted: SubmittedActionProposal) => void;
  private readonly clearDraftOnDirectSubmit: boolean;
  private readonly defaultDoc?: Document;
  private readonly listeners = new Set<OmnibarPaletteListener>();
  private mountedContainer: HTMLElement | null = null;
  private mountedDoc: Document | null = null;
  private mountedRootEl: HTMLElement | null = null;

  public constructor(options?: OmnibarPaletteControllerOptions) {
    this.locale = options?.initialLocale ?? "zh-CN";
    this.disclosure = options?.initialDisclosure ?? "L1";
    this.sessionState = options?.initialSessionState ?? "ACTIVE";
    this.sessionGeneration = options?.initialSessionGeneration ?? 1;
    this.z4ModalActive = options?.initialZ4ModalActive ?? false;
    this.workspaceIdHex = options?.workspaceIdHex ?? "aa".repeat(16);
    this.workspaceName =
      options?.workspaceName ??
      (this.locale === "en-US" ? "Main Workspace" : "主工作区");
    this.principalIdHex = options?.principalIdHex ?? "bb".repeat(16);
    this.effectiveBudgetCapUnits = options?.effectiveBudgetCapUnits ?? 100;
    this.currentBudgetUsageUnits = options?.currentBudgetUsageUnits ?? 18;
    this.catalog = [...(options?.catalog ?? [])];
    this.onSubmitProposal = options?.onSubmitProposal;
    this.clearDraftOnDirectSubmit = options?.clearDraftOnDirectSubmit ?? false;
    this.defaultDoc = options?.doc;

    this.voiceController = createVoiceInputController({
      initialMicPermission: options?.initialMicPermission ?? "PROMPT_REQUIRED",
      initialSessionState: this.sessionState,
      initialSessionGeneration: this.sessionGeneration,
      initialZ4ModalActive: this.z4ModalActive,
      locale: this.locale,
      routeVoiceInput: options?.routeVoiceInput,
      onHardwareCaptureChange: options?.onHardwareCaptureChange,
      onTranscriptCommitted: (transcript) => {
        const merged =
          this.draftUtterance.trim().length > 0
            ? `${this.draftUtterance.trim()} ${transcript}`
            : transcript;
        this.setDraftUtterance(merged);
      },
    });

    this.voiceController.subscribe(() => {
      this.emitAndRefreshDom();
    });
  }

  public subscribe(listener: OmnibarPaletteListener): () => void {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  }

  public getSnapshot(): OmnibarPaletteSnapshot {
    return {
      open: this.open,
      zIndex: OMNIBAR_Z_INDEX,
      borderRadiusPx: OMNIBAR_BORDER_RADIUS_PX,
      topPermanentFootprintPx: OMNIBAR_TOP_PERMANENT_FOOTPRINT_PX,
      taskbarTriggerHeightPx: OMNIBAR_TASKBAR_TRIGGER_HEIGHT_PX,
      lastOpenSource: this.lastOpenSource,
      lastCloseReason: this.lastCloseReason,
      locale: this.locale,
      disclosure: this.disclosure,
      sessionState: this.sessionState,
      sessionGeneration: this.sessionGeneration,
      z4ModalActive: this.z4ModalActive,
      draftUtterance: this.draftUtterance,
      attachments: [...this.attachments],
      selectedCandidateId: this.selectedCandidateId,
      compiledBundle: this.compiledBundle,
      bindingError: this.bindingError,
      lastClarificationBlockMessage: this.lastClarificationBlockMessage,
      lastSubmittedProposal: this.lastSubmittedProposal,
      voice: this.voiceController.getSnapshot(),
    };
  }

  private recompileCurrentDraft(): void {
    if (this.draftUtterance.trim().length === 0) {
      this.compiledBundle = null;
      this.bindingError = null;
      this.lastClarificationBlockMessage = null;
      return;
    }
    try {
      this.compiledBundle = compileIntentAndProposal({
        utterance: this.draftUtterance,
        attachments: this.attachments,
        catalog: this.catalog,
        explicitDependency: this.explicitDependency,
        workspaceIdHex: this.workspaceIdHex,
        workspaceName: this.workspaceName,
        principalIdHex: this.principalIdHex,
        sessionGeneration: this.sessionGeneration,
        effectiveBudgetCapUnits: this.effectiveBudgetCapUnits,
        currentBudgetUsageUnits: this.currentBudgetUsageUnits,
        requestedBudgetOverrideUnits: this.requestedBudgetOverrideUnits,
        selectedCandidateId: this.selectedCandidateId,
        locale: this.locale,
      });
      this.bindingError = null;
      if (this.compiledBundle.proposal.canSubmit) {
        this.lastClarificationBlockMessage = null;
      }
    } catch (err) {
      this.compiledBundle = null;
      if (err instanceof IntentBindingError) {
        this.bindingError = {
          code: err.code,
          userMessage: err.userMessage,
          diagnosticDetail: err.diagnosticDetail,
        };
      } else {
        this.bindingError = {
          code: "UNVERIFIED_DEPENDENCY_HANDLE",
          userMessage:
            this.locale === "en-US"
              ? "Unable to verify dependency binding for this request."
              : "无法核验该请求的依赖绑定。",
          diagnosticDetail: err instanceof Error ? err.message : String(err),
        };
      }
    }
  }

  private emitAndRefreshDom(): void {
    if (this.mountedContainer && this.mountedDoc) {
      this.mountedRootEl = createOmnibarPaletteElement(
        this.getSnapshot(),
        {
          onInputUtterance: (val) => this.setDraftUtterance(val),
          onToggleVoice: () => {
            this.voiceController.toggleCapture();
          },
          onRemoveAttachment: (id) => this.removeAttachment(id),
          onSelectCandidate: (candidateId) => this.selectCandidate(candidateId),
          onClearDraft: () => this.clearDraft(),
          onSubmit: () => {
            try {
              this.submitProposal();
            } catch {
              // ClarificationRequiredError / IntentBindingError state is already captured in snapshot.
            }
          },
          onOutsideDismiss: () => this.dismissOutsideClick(),
        },
        { doc: this.mountedDoc },
      );
      this.mountedContainer.replaceChildren(this.mountedRootEl);
    }
    const snap = this.getSnapshot();
    for (const listener of this.listeners) {
      listener(snap);
    }
  }

  public setLocale(locale: WorkstationLocale): void {
    if (this.locale === locale) {
      return;
    }
    this.locale = locale;
    this.voiceController.setLocale(locale);
    this.recompileCurrentDraft();
    this.emitAndRefreshDom();
  }

  public setDisclosure(disclosure: DisclosureLevel): void {
    if (this.disclosure === disclosure) {
      return;
    }
    this.disclosure = disclosure;
    this.emitAndRefreshDom();
  }

  public setCatalog(catalog: readonly VerifiedApplicationCatalogItem[]): void {
    this.catalog = [...catalog];
    this.recompileCurrentDraft();
    this.emitAndRefreshDom();
  }

  public setWorkspaceContext(context: {
    readonly workspaceIdHex: string;
    readonly workspaceName?: string;
    readonly principalIdHex?: string;
    readonly effectiveBudgetCapUnits?: number;
    readonly currentBudgetUsageUnits?: number;
  }): void {
    this.workspaceIdHex = context.workspaceIdHex;
    if (context.workspaceName !== undefined) {
      this.workspaceName = context.workspaceName;
    }
    if (context.principalIdHex !== undefined) {
      this.principalIdHex = context.principalIdHex;
    }
    if (context.effectiveBudgetCapUnits !== undefined) {
      this.effectiveBudgetCapUnits = context.effectiveBudgetCapUnits;
    }
    if (context.currentBudgetUsageUnits !== undefined) {
      this.currentBudgetUsageUnits = context.currentBudgetUsageUnits;
    }
    this.recompileCurrentDraft();
    this.emitAndRefreshDom();
  }

  public syncSecurityAndSessionState(state: {
    readonly sessionState: DesktopSessionStateDto;
    readonly sessionGeneration?: number;
    readonly z4ModalActive: boolean;
  }): void {
    this.sessionState = state.sessionState;
    if (state.sessionGeneration !== undefined) {
      this.sessionGeneration = state.sessionGeneration;
    }
    this.z4ModalActive = state.z4ModalActive;

    if (state.z4ModalActive || state.sessionState !== "ACTIVE") {
      if (this.open) {
        this.open = false;
        this.lastCloseReason = "security-gate";
      }
    }

    this.voiceController.syncSecurityAndSessionGate(state);
    this.recompileCurrentDraft();
    this.emitAndRefreshDom();
  }

  public openPalette(source: OmnibarOpenSource = "programmatic"): boolean {
    if (this.z4ModalActive || this.sessionState !== "ACTIVE") {
      return false;
    }
    this.open = true;
    this.lastOpenSource = source;
    this.emitAndRefreshDom();
    return true;
  }

  public openFromTaskbarSlot(): boolean {
    if (this.open) {
      this.closePalette("programmatic");
      return false;
    }
    return this.openPalette("taskbar-trigger");
  }

  public closePalette(reason: OmnibarCloseReason = "programmatic"): void {
    if (!this.open) {
      return;
    }
    this.open = false;
    this.lastCloseReason = reason;
    this.emitAndRefreshDom();
  }

  public dismissEscape(): boolean {
    if (!this.open) {
      return false;
    }
    this.closePalette("escape");
    return true;
  }

  public dismissOutsideClick(): boolean {
    if (!this.open) {
      return false;
    }
    this.closePalette("outside-click");
    return true;
  }

  public handleKeyDown(event: OmnibarKeyInput): {
    readonly handled: boolean;
    readonly action:
      | "opened"
      | "closed-escape"
      | "toggled-closed"
      | "submitted"
      | "blocked"
      | "ignored";
  } {
    if (event.fromUntrustedSurface) {
      return { handled: true, action: "blocked" };
    }

    const isSpaceKey = event.key === " " || event.code === "Space";
    const isCtrlOrCmdSpace =
      (Boolean(event.ctrlKey) || Boolean(event.metaKey)) &&
      !event.altKey &&
      !event.shiftKey &&
      isSpaceKey;

    if (isCtrlOrCmdSpace) {
      event.preventDefault?.();
      if (this.z4ModalActive || this.sessionState !== "ACTIVE") {
        return { handled: true, action: "blocked" };
      }
      if (this.open) {
        this.closePalette("programmatic");
        return { handled: true, action: "toggled-closed" };
      }
      this.openPalette("keyboard-shortcut");
      return { handled: true, action: "opened" };
    }

    if (this.open && event.key === "Escape") {
      event.preventDefault?.();
      this.dismissEscape();
      return { handled: true, action: "closed-escape" };
    }

    if (
      this.open &&
      event.key === "Enter" &&
      !event.shiftKey &&
      !event.altKey
    ) {
      event.preventDefault?.();
      try {
        this.submitProposal();
        return { handled: true, action: "submitted" };
      } catch {
        return { handled: true, action: "blocked" };
      }
    }

    return { handled: false, action: "ignored" };
  }

  public setDraftUtterance(
    utterance: string,
    options?: {
      readonly explicitDependency?: ExplicitDependencyPinInput;
      readonly requestedBudgetOverrideUnits?: number;
    },
  ): OmnibarPaletteSnapshot {
    this.draftUtterance = utterance;
    this.selectedCandidateId = null;
    this.lastClarificationBlockMessage = null;
    if (options && "explicitDependency" in options) {
      this.explicitDependency = options.explicitDependency;
    }
    if (options && "requestedBudgetOverrideUnits" in options) {
      this.requestedBudgetOverrideUnits = options.requestedBudgetOverrideUnits;
    }
    this.recompileCurrentDraft();
    this.emitAndRefreshDom();
    return this.getSnapshot();
  }

  public setExplicitDependencyPin(
    pin: ExplicitDependencyPinInput | undefined,
  ): OmnibarPaletteSnapshot {
    this.explicitDependency = pin;
    this.selectedCandidateId = null;
    this.recompileCurrentDraft();
    this.emitAndRefreshDom();
    return this.getSnapshot();
  }

  public addAttachment(chip: AttachmentChipModel): OmnibarPaletteSnapshot {
    const existingIdx = this.attachments.findIndex(
      (item) => item.attachmentId === chip.attachmentId,
    );
    if (existingIdx >= 0) {
      this.attachments[existingIdx] = chip;
    } else {
      this.attachments.push(chip);
    }
    this.selectedCandidateId = null;
    this.recompileCurrentDraft();
    this.emitAndRefreshDom();
    return this.getSnapshot();
  }

  public removeAttachment(attachmentId: string): OmnibarPaletteSnapshot {
    this.attachments = this.attachments.filter(
      (item) => item.attachmentId !== attachmentId,
    );
    this.selectedCandidateId = null;
    this.recompileCurrentDraft();
    this.emitAndRefreshDom();
    return this.getSnapshot();
  }

  public clearDraft(): OmnibarPaletteSnapshot {
    this.draftUtterance = "";
    this.attachments = [];
    this.explicitDependency = undefined;
    this.requestedBudgetOverrideUnits = undefined;
    this.selectedCandidateId = null;
    this.compiledBundle = null;
    this.bindingError = null;
    this.lastClarificationBlockMessage = null;
    this.emitAndRefreshDom();
    return this.getSnapshot();
  }

  public selectCandidate(candidateId: string): OmnibarPaletteSnapshot {
    this.selectedCandidateId = candidateId;
    this.lastClarificationBlockMessage = null;
    this.recompileCurrentDraft();
    this.emitAndRefreshDom();
    return this.getSnapshot();
  }

  public submitProposal(): SubmittedActionProposal {
    if (this.z4ModalActive || this.sessionState !== "ACTIVE") {
      throw new IntentBindingError(
        "UNVERIFIED_DEPENDENCY_HANDLE",
        this.locale === "en-US"
          ? "Session is locked or awaiting system security confirmation."
          : "当前会话已锁定或正处于系统安全确认状态。",
        "Submission blocked by non-ACTIVE session or active Z4 modal",
      );
    }

    this.recompileCurrentDraft();
    if (this.bindingError !== null) {
      throw new IntentBindingError(
        this.bindingError.code as IntentBindingError["code"],
        this.bindingError.userMessage,
        this.bindingError.diagnosticDetail,
      );
    }
    if (!this.compiledBundle) {
      throw new IntentBindingError(
        "EMPTY_UTTERANCE",
        this.locale === "en-US"
          ? "Enter a command or natural-language request before submitting."
          : "请先输入命令或自然语言需求后再提交。",
        "Empty utterance cannot be submitted",
      );
    }

    try {
      const submitted = finalizeProposalForSubmission(
        this.compiledBundle,
        this.locale,
      );
      this.lastSubmittedProposal = submitted;
      this.lastClarificationBlockMessage = null;
      this.open = false;
      this.lastCloseReason = "submitted";
      if (
        this.clearDraftOnDirectSubmit &&
        submitted.handoffTarget === "DIRECT_BACKGROUND_TASK"
      ) {
        this.draftUtterance = "";
        this.attachments = [];
        this.selectedCandidateId = null;
        this.compiledBundle = null;
      }
      this.onSubmitProposal?.(submitted);
      this.emitAndRefreshDom();
      return submitted;
    } catch (err) {
      if (err instanceof ClarificationRequiredError) {
        this.lastClarificationBlockMessage = err.message;
        this.emitAndRefreshDom();
      }
      throw err;
    }
  }

  public startPushToTalk(options?: {
    readonly mode?: VoiceCaptureMode;
    readonly fromUntrustedSurface?: boolean;
    readonly nowMonoMs?: number;
  }): VoiceTransitionResult {
    return this.voiceController.startCapture(options);
  }

  public mount(container: HTMLElement, options?: { doc?: Document }): HTMLElement {
    const doc = options?.doc ?? this.defaultDoc ?? document;
    this.mountedContainer = container;
    this.mountedDoc = doc;
    this.emitAndRefreshDom();
    return this.mountedRootEl!;
  }

  public unmount(): void {
    if (this.mountedContainer) {
      this.mountedContainer.replaceChildren();
    }
    this.mountedContainer = null;
    this.mountedDoc = null;
    this.mountedRootEl = null;
  }

  public getVisibleTextForAudit(): string {
    const snap = this.getSnapshot();
    if (this.mountedRootEl) {
      return collectVisibleNodeText(this.mountedRootEl, snap.disclosure)
        .replace(/\s+/g, " ")
        .trim();
    }
    const html = renderOmnibarPaletteHtml(snap);
    return html
      .replace(/<([a-z0-9-]+)\b[^>]*\bhidden\b[^>]*>[\s\S]*?<\/\1>/gi, " ")
      .replace(/<[^>]*>/g, " ")
      .replace(/\s+/g, " ")
      .trim();
  }
}

export function createOmnibarPaletteController(
  options?: OmnibarPaletteControllerOptions,
): OmnibarPaletteController {
  return new OmnibarPaletteController(options);
}

export function renderOmnibarPaletteHtml(
  snapshot: OmnibarPaletteSnapshot,
): string {
  const copy = OMNIBAR_COPY[snapshot.locale];
  if (!snapshot.open) {
    return (
      `<div class="nlos-omnibar-scrim" data-nlos-omnibar-open="false" ` +
      `data-top-footprint-px="${OMNIBAR_TOP_PERMANENT_FOOTPRINT_PX}" ` +
      `data-z-index="${OMNIBAR_Z_INDEX}" style="z-index: ${OMNIBAR_Z_INDEX};" hidden></div>`
    );
  }

  const promptIcon = renderIconSvg("command-prompt", {
    size: 16,
    strokeWidth: 1.5,
  });
  const micIcon = renderIconSvg("hw-microphone", {
    size: 14,
    strokeWidth: 1.5,
  });

  const proposal = snapshot.compiledBundle?.proposal ?? null;
  const typedIntent = snapshot.compiledBundle?.typedIntent ?? null;

  const submitBlockedByAmbiguity =
    proposal !== null && !proposal.canSubmit;
  const submitDisabled =
    proposal === null || snapshot.bindingError !== null || submitBlockedByAmbiguity;

  const submitLabel = submitBlockedByAmbiguity
    ? copy.submitClarifyBlockedLabel
    : proposal?.requiresZ4Consent
      ? copy.submitSecurityLabel
      : copy.submitReadyLabel;

  const voiceActive =
    snapshot.voice.phase === "Capturing" ||
    snapshot.voice.phase === "RequestingMicPermission";
  const showVoiceBar =
    snapshot.voice.phase !== "Idle" || snapshot.voice.lastSeveranceReason !== null;

  const voiceBarHtml = showVoiceBar
    ? `<div class="nlos-omnibar-voice-bar" data-voice-phase="${escapeHtml(snapshot.voice.phase)}">` +
      `<span>${escapeHtml(snapshot.voice.statusLabel)}</span>` +
      (snapshot.voice.streamingTranscript.length > 0
        ? `<span>${escapeHtml(snapshot.voice.streamingTranscript)}</span>`
        : "") +
      `</div>`
    : "";

  const chipsHtml = renderAttachmentChipsHtml(snapshot.attachments, {
    locale: snapshot.locale,
    disclosure: snapshot.disclosure,
  });

  const errorBannerHtml = snapshot.bindingError
    ? `<div class="nlos-omnibar-ambiguity-panel" role="alert" data-binding-error="${escapeHtml(snapshot.bindingError.code)}">` +
      `<div class="nlos-omnibar-ambiguity-header">${escapeHtml(snapshot.bindingError.userMessage)}</div>` +
      (snapshot.disclosure === "L3"
        ? `<div class="nlos-l3-only nlos-omnibar-l3-panel">${escapeHtml(snapshot.bindingError.code)}: ${escapeHtml(snapshot.bindingError.diagnosticDetail)}</div>`
        : "") +
      `</div>`
    : "";

  let previewHtml = "";
  if (proposal && typedIntent) {
    const summaryHtml =
      `<div class="nlos-omnibar-summary-bar">` +
      `<div class="nlos-omnibar-summary-grid">` +
      `<div><span class="nlos-omnibar-kv-label">${escapeHtml(copy.targetAppKey)}:</span>` +
      `<span class="nlos-omnibar-kv-value">${escapeHtml(proposal.targetApplicationLabel)}</span></div>` +
      `<div><span class="nlos-omnibar-kv-label">${escapeHtml(copy.affectedScopeKey)}:</span>` +
      `<span class="nlos-omnibar-kv-value">${escapeHtml(proposal.affectedScopeLabel)}</span></div>` +
      `<div><span class="nlos-omnibar-kv-label">${escapeHtml(copy.budgetCeilingKey)}:</span>` +
      `<span class="nlos-omnibar-kv-value">${escapeHtml(proposal.resourceCeilingLabel)}</span></div>` +
      `<div><span class="nlos-omnibar-kv-label">${escapeHtml(copy.dependencyKey)}:</span>` +
      `<span class="nlos-omnibar-kv-value">${escapeHtml(proposal.dependencySummaryLabel)}</span></div>` +
      `</div>` +
      `<span class="nlos-omnibar-undo-badge" data-undo-class="${proposal.undoClass}">` +
      `${escapeHtml(proposal.undoBadgeLabel)}</span>` +
      `</div>`;

    const l2Html =
      snapshot.disclosure === "L2" || snapshot.disclosure === "L3"
        ? `<div class="nlos-omnibar-l2-details" data-omnibar-tier="L2">` +
          `<div><span class="nlos-omnibar-kv-label">${escapeHtml(copy.l2CompensationKey)}:</span>` +
          `<span class="nlos-omnibar-kv-value">${escapeHtml(proposal.compensationNote)}</span></div>` +
          (proposal.plainTaintSummary
            ? `<div><span class="nlos-omnibar-kv-label">${escapeHtml(copy.l2SourceNoticeKey)}:</span>` +
              `<span class="nlos-omnibar-kv-value">${escapeHtml(proposal.plainTaintSummary)}</span></div>`
            : "") +
          `<div><span class="nlos-omnibar-kv-label">${escapeHtml(copy.l2ResourceBarKey)}:</span>` +
          `${renderBulletMicroBarHtml({
            actualUsage: proposal.budgetPreview.actualUsage,
            effectiveGrantedOrLeaseCap: proposal.budgetPreview.effectiveCap,
            requestedCeiling: proposal.budgetPreview.requestedCeiling,
            unit: proposal.budgetPreview.unit,
            locale: snapshot.locale,
            disclosure: snapshot.disclosure,
          })}</div>` +
          `</div>`
        : "";

    const ambiguityHtml =
      proposal.unresolvedAmbiguities.length > 0
        ? `<div class="nlos-omnibar-ambiguity-panel" data-omnibar-ambiguity="true">` +
          `<div class="nlos-omnibar-ambiguity-header">${escapeHtml(copy.ambiguityHeader)}</div>` +
          `<table class="nlos-omnibar-candidate-table">` +
          `<thead><tr>` +
          `<th>${escapeHtml(copy.colCandidateApp)}</th>` +
          `<th>${escapeHtml(copy.colCandidateScope)}</th>` +
          `<th>${escapeHtml(copy.colCandidateEffect)}</th>` +
          `<th>${escapeHtml(copy.colCandidateUndo)}</th>` +
          `<th>${escapeHtml(copy.colCandidateBudget)}</th>` +
          `</tr></thead><tbody>` +
          proposal.candidates
            .map(
              (cand) =>
                `<tr class="nlos-omnibar-candidate-row" data-candidate-id="${escapeHtml(cand.candidateId)}" ` +
                `data-selected="${proposal.selectedCandidateId === cand.candidateId ? "true" : "false"}">` +
                `<td>${escapeHtml(cand.targetAppName)}</td>` +
                `<td>${escapeHtml(cand.affectedScopeSummary)}</td>` +
                `<td>${escapeHtml(cand.sideEffectLabel)}</td>` +
                `<td>${escapeHtml(cand.undoBadgeLabel)}</td>` +
                `<td>${escapeHtml(cand.budgetImpactSummary)}</td>` +
                `</tr>`,
            )
            .join("") +
          `</tbody></table>` +
          (snapshot.lastClarificationBlockMessage
            ? `<div role="alert">${escapeHtml(snapshot.lastClarificationBlockMessage)}</div>`
            : "") +
          `</div>`
        : "";

    const l3Html =
      snapshot.disclosure === "L3"
        ? `<div class="nlos-l3-only nlos-omnibar-l3-panel" data-omnibar-tier="L3">` +
          `<div>TypedIntent: schema=${escapeHtml(typedIntent.schemaId)}@${escapeHtml(typedIntent.schemaVersion)} id=${escapeHtml(typedIntent.intentIdHex)} kind=${escapeHtml(typedIntent.intentKind)}</div>` +
          `<div>ActionProposal: schema=${escapeHtml(proposal.proposalSchemaId)} digest=${escapeHtml(proposal.proposalDigestHex)}</div>` +
          `<div>ControlScope=${escapeHtml(typedIntent.controlScope)} IdempotencyKey=${escapeHtml(typedIntent.idempotencyKeyHex)}</div>` +
          (typedIntent.resolvedApplication
            ? `<div>ApplicationHandle=(app=${escapeHtml(typedIntent.resolvedApplication.applicationIdHex)}, inst=${escapeHtml(typedIntent.resolvedApplication.installationIdHex)}, generation=${typedIntent.resolvedApplication.generation})</div>`
            : "") +
          (proposal.clarificationDigestHex
            ? `<div>ClarificationBinding=(candidate=${escapeHtml(proposal.selectedCandidateId ?? "")}, digest=${escapeHtml(proposal.clarificationDigestHex)})</div>`
            : "") +
          `<div>ControlCommandPreview=${escapeHtml(typedIntent.compiledControlCommandPreview)}</div>` +
          `</div>`
        : "";

    previewHtml =
      `<div class="nlos-omnibar-preview-section">` +
      summaryHtml +
      l2Html +
      ambiguityHtml +
      l3Html +
      `</div>`;
  } else if (errorBannerHtml.length > 0) {
    previewHtml = `<div class="nlos-omnibar-preview-section">${errorBannerHtml}</div>`;
  }

  return (
    `<div class="nlos-omnibar-scrim" data-nlos-omnibar-open="true" ` +
    `data-top-footprint-px="${OMNIBAR_TOP_PERMANENT_FOOTPRINT_PX}" ` +
    `data-z-index="${OMNIBAR_Z_INDEX}" style="z-index: ${OMNIBAR_Z_INDEX};">` +
    `<section role="dialog" aria-modal="false" aria-label="${escapeHtml(copy.dialogAriaLabel)}" ` +
    `class="nlos-omnibar-dialog" style="border-radius: 4px;">` +
    `<div class="nlos-omnibar-input-row">` +
    `<span class="nlos-omnibar-prompt-glyph" aria-hidden="true">${promptIcon}</span>` +
    `<input type="text" class="nlos-omnibar-input" value="${escapeHtml(snapshot.draftUtterance)}" ` +
    `placeholder="${escapeHtml(copy.inputPlaceholder)}" aria-label="${escapeHtml(copy.dialogAriaLabel)}" />` +
    `<button type="button" class="nlos-omnibar-voice-btn" data-voice-active="${voiceActive ? "true" : "false"}">` +
    `${micIcon}<span>${escapeHtml(copy.voiceBtnLabel)}</span></button>` +
    (snapshot.draftUtterance.length > 0 || snapshot.attachments.length > 0
      ? `<button type="button" class="nlos-omnibar-action-btn" data-omnibar-action="clear-draft">${escapeHtml(copy.clearDraftLabel)}</button>`
      : "") +
    `<button type="button" class="nlos-omnibar-submit-btn" data-omnibar-action="submit"${submitDisabled ? " disabled" : ""}>` +
    `${escapeHtml(submitLabel)}</button>` +
    `</div>` +
    voiceBarHtml +
    chipsHtml +
    previewHtml +
    `<footer class="nlos-omnibar-footer">` +
    `<span>${escapeHtml(copy.footerShortcutHint)}</span>` +
    `<span>${escapeHtml(copy.footerDraftSavedHint)}</span>` +
    `</footer>` +
    `</section>` +
    `</div>`
  );
}

export interface OmnibarElementCallbacks {
  readonly onInputUtterance?: (value: string) => void;
  readonly onToggleVoice?: () => void;
  readonly onRemoveAttachment?: (attachmentId: string) => void;
  readonly onSelectCandidate?: (candidateId: string) => void;
  readonly onClearDraft?: () => void;
  readonly onSubmit?: () => void;
  readonly onOutsideDismiss?: () => void;
}

export function createOmnibarPaletteElement(
  snapshot: OmnibarPaletteSnapshot,
  callbacks?: OmnibarElementCallbacks,
  options?: { doc?: Document },
): HTMLElement {
  const doc = options?.doc ?? document;
  const copy = OMNIBAR_COPY[snapshot.locale];

  const scrim = doc.createElement("div");
  scrim.className = "nlos-omnibar-scrim";
  scrim.setAttribute("data-nlos-omnibar-open", snapshot.open ? "true" : "false");
  scrim.setAttribute(
    "data-top-footprint-px",
    String(OMNIBAR_TOP_PERMANENT_FOOTPRINT_PX),
  );
  scrim.setAttribute("data-z-index", String(OMNIBAR_Z_INDEX));
  scrim.style.zIndex = String(OMNIBAR_Z_INDEX);

  if (!snapshot.open) {
    scrim.hidden = true;
    scrim.setAttribute("hidden", "");
    return scrim;
  }

  scrim.addEventListener("click", (ev) => {
    if (ev.target === scrim) {
      callbacks?.onOutsideDismiss?.();
    }
  });

  const dialog = doc.createElement("section");
  dialog.className = "nlos-omnibar-dialog";
  dialog.setAttribute("role", "dialog");
  dialog.setAttribute("aria-modal", "false");
  dialog.setAttribute("aria-label", copy.dialogAriaLabel);
  dialog.style.borderRadius = `${OMNIBAR_BORDER_RADIUS_PX}px`;

  const inputRow = doc.createElement("div");
  inputRow.className = "nlos-omnibar-input-row";

  const glyphWrap = doc.createElement("span");
  glyphWrap.className = "nlos-omnibar-prompt-glyph";
  glyphWrap.setAttribute("aria-hidden", "true");
  glyphWrap.appendChild(
    createIconElement("command-prompt", { size: 16, strokeWidth: 1.5, doc }),
  );
  inputRow.appendChild(glyphWrap);

  const inputEl = doc.createElement("input");
  inputEl.type = "text";
  inputEl.className = "nlos-omnibar-input";
  inputEl.value = snapshot.draftUtterance;
  inputEl.placeholder = copy.inputPlaceholder;
  inputEl.setAttribute("aria-label", copy.dialogAriaLabel);
  inputEl.addEventListener("input", () => {
    callbacks?.onInputUtterance?.(inputEl.value);
  });
  inputRow.appendChild(inputEl);

  const voiceActive =
    snapshot.voice.phase === "Capturing" ||
    snapshot.voice.phase === "RequestingMicPermission";
  const voiceBtn = doc.createElement("button");
  voiceBtn.type = "button";
  voiceBtn.className = "nlos-omnibar-voice-btn";
  voiceBtn.setAttribute("data-voice-active", voiceActive ? "true" : "false");
  voiceBtn.appendChild(
    createIconElement("hw-microphone", { size: 14, strokeWidth: 1.5, doc }),
  );
  const voiceLabelSpan = doc.createElement("span");
  voiceLabelSpan.textContent = copy.voiceBtnLabel;
  voiceBtn.appendChild(voiceLabelSpan);
  voiceBtn.addEventListener("click", () => {
    callbacks?.onToggleVoice?.();
  });
  inputRow.appendChild(voiceBtn);

  if (snapshot.draftUtterance.length > 0 || snapshot.attachments.length > 0) {
    const clearBtn = doc.createElement("button");
    clearBtn.type = "button";
    clearBtn.className = "nlos-omnibar-action-btn";
    clearBtn.setAttribute("data-omnibar-action", "clear-draft");
    clearBtn.textContent = copy.clearDraftLabel;
    clearBtn.addEventListener("click", () => {
      callbacks?.onClearDraft?.();
    });
    inputRow.appendChild(clearBtn);
  }

  const proposal = snapshot.compiledBundle?.proposal ?? null;
  const typedIntent = snapshot.compiledBundle?.typedIntent ?? null;
  const submitBlockedByAmbiguity = proposal !== null && !proposal.canSubmit;
  const submitDisabled =
    proposal === null ||
    snapshot.bindingError !== null ||
    submitBlockedByAmbiguity;

  const submitBtn = doc.createElement("button");
  submitBtn.type = "button";
  submitBtn.className = "nlos-omnibar-submit-btn";
  submitBtn.setAttribute("data-omnibar-action", "submit");
  submitBtn.disabled = submitDisabled;
  if (submitDisabled) {
    submitBtn.setAttribute("disabled", "");
  }
  submitBtn.textContent = submitBlockedByAmbiguity
    ? copy.submitClarifyBlockedLabel
    : proposal?.requiresZ4Consent
      ? copy.submitSecurityLabel
      : copy.submitReadyLabel;
  submitBtn.addEventListener("click", () => {
    callbacks?.onSubmit?.();
  });
  inputRow.appendChild(submitBtn);
  dialog.appendChild(inputRow);

  if (snapshot.voice.phase !== "Idle" || snapshot.voice.lastSeveranceReason !== null) {
    const voiceBar = doc.createElement("div");
    voiceBar.className = "nlos-omnibar-voice-bar";
    voiceBar.setAttribute("data-voice-phase", snapshot.voice.phase);
    const statusSpan = doc.createElement("span");
    statusSpan.textContent = snapshot.voice.statusLabel;
    voiceBar.appendChild(statusSpan);
    if (snapshot.voice.streamingTranscript.length > 0) {
      const liveSpan = doc.createElement("span");
      liveSpan.textContent = snapshot.voice.streamingTranscript;
      voiceBar.appendChild(liveSpan);
    }
    dialog.appendChild(voiceBar);
  }

  if (snapshot.attachments.length > 0) {
    dialog.appendChild(
      createAttachmentChipsElement(snapshot.attachments, {
        locale: snapshot.locale,
        disclosure: snapshot.disclosure,
        onRemoveAttachment: callbacks?.onRemoveAttachment,
        doc,
      }),
    );
  }

  if (snapshot.bindingError) {
    const previewWrap = doc.createElement("div");
    previewWrap.className = "nlos-omnibar-preview-section";
    const alertBox = doc.createElement("div");
    alertBox.className = "nlos-omnibar-ambiguity-panel";
    alertBox.setAttribute("role", "alert");
    alertBox.setAttribute("data-binding-error", snapshot.bindingError.code);
    const msg = doc.createElement("div");
    msg.className = "nlos-omnibar-ambiguity-header";
    msg.textContent = snapshot.bindingError.userMessage;
    alertBox.appendChild(msg);

    const l3Err = doc.createElement("div");
    l3Err.className = "nlos-l3-only nlos-omnibar-l3-panel";
    if (snapshot.disclosure === "L3") {
      l3Err.hidden = false;
      l3Err.textContent = `${snapshot.bindingError.code}: ${snapshot.bindingError.diagnosticDetail}`;
    } else {
      l3Err.hidden = true;
      l3Err.setAttribute("hidden", "");
    }
    alertBox.appendChild(l3Err);
    previewWrap.appendChild(alertBox);
    dialog.appendChild(previewWrap);
  } else if (proposal && typedIntent) {
    const previewWrap = doc.createElement("div");
    previewWrap.className = "nlos-omnibar-preview-section";

    const summaryBar = doc.createElement("div");
    summaryBar.className = "nlos-omnibar-summary-bar";
    const grid = doc.createElement("div");
    grid.className = "nlos-omnibar-summary-grid";

    const pairs: ReadonlyArray<readonly [string, string]> = [
      [copy.targetAppKey, proposal.targetApplicationLabel],
      [copy.affectedScopeKey, proposal.affectedScopeLabel],
      [copy.budgetCeilingKey, proposal.resourceCeilingLabel],
      [copy.dependencyKey, proposal.dependencySummaryLabel],
    ];
    for (const [k, v] of pairs) {
      const cell = doc.createElement("div");
      const kSpan = doc.createElement("span");
      kSpan.className = "nlos-omnibar-kv-label";
      kSpan.textContent = `${k}:`;
      const vSpan = doc.createElement("span");
      vSpan.className = "nlos-omnibar-kv-value";
      vSpan.textContent = ` ${v}`;
      cell.appendChild(kSpan);
      cell.appendChild(vSpan);
      grid.appendChild(cell);
    }
    summaryBar.appendChild(grid);

    const undoBadge = doc.createElement("span");
    undoBadge.className = "nlos-omnibar-undo-badge";
    undoBadge.setAttribute("data-undo-class", proposal.undoClass);
    undoBadge.textContent = proposal.undoBadgeLabel;
    summaryBar.appendChild(undoBadge);
    previewWrap.appendChild(summaryBar);

    if (snapshot.disclosure === "L2" || snapshot.disclosure === "L3") {
      const l2Box = doc.createElement("div");
      l2Box.className = "nlos-omnibar-l2-details";
      l2Box.setAttribute("data-omnibar-tier", "L2");

      const compRow = doc.createElement("div");
      compRow.textContent = `${copy.l2CompensationKey}: ${proposal.compensationNote}`;
      l2Box.appendChild(compRow);

      if (proposal.plainTaintSummary) {
        const taintRow = doc.createElement("div");
        taintRow.textContent = `${copy.l2SourceNoticeKey}: ${proposal.plainTaintSummary}`;
        l2Box.appendChild(taintRow);
      }

      const barRow = doc.createElement("div");
      const barLabel = doc.createElement("span");
      barLabel.className = "nlos-omnibar-kv-label";
      barLabel.textContent = `${copy.l2ResourceBarKey}: `;
      barRow.appendChild(barLabel);
      barRow.appendChild(
        createBulletMicroBarElement({
          actualUsage: proposal.budgetPreview.actualUsage,
          effectiveGrantedOrLeaseCap: proposal.budgetPreview.effectiveCap,
          requestedCeiling: proposal.budgetPreview.requestedCeiling,
          unit: proposal.budgetPreview.unit,
          locale: snapshot.locale,
          disclosure: snapshot.disclosure,
          doc,
        }),
      );
      l2Box.appendChild(barRow);
      previewWrap.appendChild(l2Box);
    }

    if (proposal.unresolvedAmbiguities.length > 0) {
      const ambPanel = doc.createElement("div");
      ambPanel.className = "nlos-omnibar-ambiguity-panel";
      ambPanel.setAttribute("data-omnibar-ambiguity", "true");

      const ambHead = doc.createElement("div");
      ambHead.className = "nlos-omnibar-ambiguity-header";
      ambHead.textContent = copy.ambiguityHeader;
      ambPanel.appendChild(ambHead);

      const table = doc.createElement("table");
      table.className = "nlos-omnibar-candidate-table";
      const thead = doc.createElement("thead");
      const headTr = doc.createElement("tr");
      for (const colTitle of [
        copy.colCandidateApp,
        copy.colCandidateScope,
        copy.colCandidateEffect,
        copy.colCandidateUndo,
        copy.colCandidateBudget,
      ]) {
        const th = doc.createElement("th");
        th.textContent = colTitle;
        headTr.appendChild(th);
      }
      thead.appendChild(headTr);
      table.appendChild(thead);

      const tbody = doc.createElement("tbody");
      for (const cand of proposal.candidates) {
        const tr = doc.createElement("tr");
        tr.className = "nlos-omnibar-candidate-row";
        tr.setAttribute("data-candidate-id", cand.candidateId);
        tr.setAttribute(
          "data-selected",
          proposal.selectedCandidateId === cand.candidateId ? "true" : "false",
        );
        for (const cellVal of [
          cand.targetAppName,
          cand.affectedScopeSummary,
          cand.sideEffectLabel,
          cand.undoBadgeLabel,
          cand.budgetImpactSummary,
        ]) {
          const td = doc.createElement("td");
          td.textContent = cellVal;
          tr.appendChild(td);
        }
        tr.addEventListener("click", () => {
          callbacks?.onSelectCandidate?.(cand.candidateId);
        });
        tbody.appendChild(tr);
      }
      table.appendChild(tbody);
      ambPanel.appendChild(table);

      if (snapshot.lastClarificationBlockMessage) {
        const blockAlert = doc.createElement("div");
        blockAlert.setAttribute("role", "alert");
        blockAlert.textContent = snapshot.lastClarificationBlockMessage;
        ambPanel.appendChild(blockAlert);
      }

      previewWrap.appendChild(ambPanel);
    }

    const l3Panel = doc.createElement("div");
    l3Panel.className = "nlos-l3-only nlos-omnibar-l3-panel";
    l3Panel.setAttribute("data-omnibar-tier", "L3");
    if (snapshot.disclosure === "L3") {
      l3Panel.hidden = false;
      const lines = [
        `TypedIntent: schema=${typedIntent.schemaId}@${typedIntent.schemaVersion} id=${typedIntent.intentIdHex} kind=${typedIntent.intentKind}`,
        `ActionProposal: schema=${proposal.proposalSchemaId} digest=${proposal.proposalDigestHex}`,
        `ControlScope=${typedIntent.controlScope} IdempotencyKey=${typedIntent.idempotencyKeyHex}`,
      ];
      if (typedIntent.resolvedApplication) {
        lines.push(
          `ApplicationHandle=(app=${typedIntent.resolvedApplication.applicationIdHex}, inst=${typedIntent.resolvedApplication.installationIdHex}, generation=${typedIntent.resolvedApplication.generation})`,
        );
      }
      if (proposal.clarificationDigestHex) {
        lines.push(
          `ClarificationBinding=(candidate=${proposal.selectedCandidateId ?? ""}, digest=${proposal.clarificationDigestHex})`,
        );
      }
      lines.push(
        `ControlCommandPreview=${typedIntent.compiledControlCommandPreview}`,
      );
      for (const textLine of lines) {
        const row = doc.createElement("div");
        row.textContent = textLine;
        l3Panel.appendChild(row);
      }
    } else {
      l3Panel.hidden = true;
      l3Panel.setAttribute("hidden", "");
    }
    previewWrap.appendChild(l3Panel);

    dialog.appendChild(previewWrap);
  }

  const footer = doc.createElement("footer");
  footer.className = "nlos-omnibar-footer";
  const hintLeft = doc.createElement("span");
  hintLeft.textContent = copy.footerShortcutHint;
  const hintRight = doc.createElement("span");
  hintRight.textContent = copy.footerDraftSavedHint;
  footer.appendChild(hintLeft);
  footer.appendChild(hintRight);
  dialog.appendChild(footer);

  scrim.appendChild(dialog);
  return scrim;
}

export interface CompositorOmnibarBinding {
  readonly controller: OmnibarPaletteController;
  readonly unbind: () => void;
}

export function bindOmnibarToCompositor(
  compositor: WorkstationCompositor,
  existingController?: OmnibarPaletteController,
  options?: { doc?: Document },
): CompositorOmnibarBinding {
  const initialSnap = compositor.getSnapshot();
  const activeWs = initialSnap.workspaces.find(
    (w) => w.workspaceIdHex === initialSnap.activeWorkspaceIdHex,
  );

  const controller =
    existingController ??
    createOmnibarPaletteController({
      initialLocale: initialSnap.locale,
      initialDisclosure: initialSnap.disclosure,
      initialSessionState: initialSnap.sessionState,
      initialSessionGeneration: initialSnap.sessionGeneration,
      initialZ4ModalActive: initialSnap.z4Active,
      workspaceIdHex: initialSnap.activeWorkspaceIdHex,
      workspaceName: activeWs?.name,
      principalIdHex: initialSnap.activePrincipal.principalIdHex,
      routeVoiceInput: (voiceCheck) => {
        const res = compositor.routeInput({
          source: "voice",
          targetPlane: "Z3",
          fromUntrustedSurface: voiceCheck.fromUntrustedSurface,
          nowMonoMs: voiceCheck.nowMonoMs,
        });
        return {
          accepted: res.accepted,
          rejectionCode: res.rejectionCode,
        };
      },
      doc: options?.doc,
    });

  let syncing = false;

  const pushPaletteElement = (): void => {
    if (!options?.doc && typeof document === "undefined") {
      return;
    }
    const el = createOmnibarPaletteElement(
      controller.getSnapshot(),
      {
        onInputUtterance: (val) => controller.setDraftUtterance(val),
        onToggleVoice: () => {
          controller.voiceController.toggleCapture();
        },
        onRemoveAttachment: (id) => controller.removeAttachment(id),
        onSelectCandidate: (cid) => controller.selectCandidate(cid),
        onClearDraft: () => controller.clearDraft(),
        onSubmit: () => {
          try {
            controller.submitProposal();
          } catch {
            // ClarificationRequiredError / IntentBindingError stays visible in palette.
          }
        },
        onOutsideDismiss: () => controller.dismissOutsideClick(),
      },
      { doc: options?.doc },
    );
    compositor.setCommandPaletteElement(el);
  };

  const unsubCompositor = compositor.subscribe((compSnap) => {
    if (syncing) {
      return;
    }
    syncing = true;
    try {
      controller.setLocale(compSnap.locale);
      controller.setDisclosure(compSnap.disclosure);
      const ws = compSnap.workspaces.find(
        (w) => w.workspaceIdHex === compSnap.activeWorkspaceIdHex,
      );
      controller.setWorkspaceContext({
        workspaceIdHex: compSnap.activeWorkspaceIdHex,
        workspaceName: ws?.name,
        principalIdHex: compSnap.activePrincipal.principalIdHex,
      });
      controller.syncSecurityAndSessionState({
        sessionState: compSnap.sessionState,
        sessionGeneration: compSnap.sessionGeneration,
        z4ModalActive: compSnap.z4Active,
      });
      if (compSnap.commandPaletteOpen && !controller.getSnapshot().open) {
        controller.openPalette("taskbar-trigger");
      } else if (!compSnap.commandPaletteOpen && controller.getSnapshot().open) {
        controller.closePalette("escape");
      }
      pushPaletteElement();
    } finally {
      syncing = false;
    }
  });

  const unsubController = controller.subscribe((omniSnap) => {
    if (syncing) {
      return;
    }
    syncing = true;
    try {
      compositor.setHardwareCapture(omniSnap.voice.taskbarIndicator);
      if (compositor.getSnapshot().commandPaletteOpen !== omniSnap.open) {
        compositor.setCommandPaletteOpen(omniSnap.open);
      }
      pushPaletteElement();
    } finally {
      syncing = false;
    }
  });

  syncing = true;
  try {
    pushPaletteElement();
  } finally {
    syncing = false;
  }

  return {
    controller,
    unbind: () => {
      unsubCompositor();
      unsubController();
    },
  };
}
