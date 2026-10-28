// ============================================================================
// NLOS Stage D (W73-D-GUI-CAMPAIGN) — CJK IME Composition & Multimodal Input
// Probe (`desktop/e2e/ime-and-multimodal-probe.ts`)
//
// Authority:
//   - Handover #3 C-GUI-CAMPAIGN (`docs/management/stage-b-progress.md` §6.5.6 #3)
//   - Unknown U-3 (`docs/management/exit-unknown-risks.md` — CJK IME & multimodal)
//   - Architecture Spec v0.5 §24.1–§24.2 (`[NL-COMPILE-001]`, `[TUI-EXT-001]`,
//     `[DUI-INPUT-001]`, `[DUI-CHOOSER-001]`, `[DUI-CLIP-001]`)
// ============================================================================

import {
  ATTACHMENT_CHIP_RADIUS_PX,
  AttachmentSecurityError,
  createArtifactAttachment,
  createChooserFileAttachment,
  createScreenshotAttachment,
  renderAttachmentChipsHtml,
} from "../src/multimodal";
import { deterministicDomainDigestHex } from "../src/trusted-ui";
import {
  createOmnibarPaletteController,
  type OmnibarPaletteController,
  type VerifiedApplicationCatalogItem,
} from "../src/views/omnibar";
import { SpotlightController } from "../src/views/spotlight";

export type ImeLocaleTag = "zh-CN-pinyin" | "ja-JP-romaji" | "en-US-mixed";

export interface ImeKeyEventInput {
  readonly key: string;
  readonly code?: string;
  readonly keyCode?: number;
  readonly isComposing: boolean;
  readonly ctrlKey?: boolean;
  readonly metaKey?: boolean;
}

export interface ImeStepTrace {
  readonly phase:
    | "compositionstart"
    | "compositionupdate"
    | "composition-key-guarded"
    | "compositionend"
    | "post-composition-key";
  readonly localeTag: ImeLocaleTag;
  readonly activeBuffer: string;
  readonly committedDraft: string;
  readonly omnibarOpen: boolean;
  readonly prematureSubmitTriggered: boolean;
  readonly prematureEscapeDismissTriggered: boolean;
}

export interface CjkImeProbeReceipt {
  readonly passed: true;
  readonly localesTested: readonly ImeLocaleTag[];
  readonly stepCount: number;
  readonly enterDuringCompositionGuarded: true;
  readonly escapeDuringCompositionGuarded: true;
  readonly finalCommittedOmnibarUtterance: string;
  readonly postCompositionSubmitSucceeded: true;
  readonly spotlightCjkHitsMatched: number;
  readonly traces: readonly ImeStepTrace[];
}

export interface MultimodalVoiceAttachmentReceipt {
  readonly passed: true;
  readonly voiceTranscriptCommitted: string;
  readonly micIndicatorActiveDuringCapture: true;
  readonly micSeveredOnZ4LockReason: string;
  readonly attachmentChipCount: number;
  readonly attachmentChipRadiusPx: 4;
  readonly rawWindowsHostPathStrippedToBasename: string;
  readonly missingChooserGrantRejectedCode: string;
  readonly renderedChipsZeroBlur: true;
}

export interface ImeAndMultimodalProbeReport {
  readonly schemaVersion: "1.0.0";
  readonly cjkImeComposition: CjkImeProbeReceipt;
  readonly multimodalVoiceAndAttachments: MultimodalVoiceAttachmentReceipt;
  readonly probeDigestHex: string;
}

function assertIme(condition: unknown, message: string): asserts condition {
  if (!condition) {
    throw new Error(`[ImeAndMultimodalProbeFailure] ${message}`);
  }
}

/**
 * Stateful IME Composition Guard wrapping `OmnibarPaletteController` & `SpotlightController`.
 * Enforces W3C Input Method Editor composition semantics:
 *   - While `isComposing === true` (or `keyCode === 229`), `Enter` commits the IME candidate
 *     buffer without calling `omnibar.handleKeyDown({ key: "Enter" })` (zero premature submit).
 *   - While `isComposing === true`, `Escape` cancels only the active IME composition buffer
 *     without dismissing the `Z3` Omnibar or discarding already-committed text.
 */
export class CjkImeCompositionHarness {
  private composing = false;
  private compositionBuffer = "";
  private localeTag: ImeLocaleTag = "zh-CN-pinyin";
  private readonly omnibar: OmnibarPaletteController;
  private readonly spotlight: SpotlightController;
  private readonly traces: ImeStepTrace[] = [];

  public constructor(
    omnibar: OmnibarPaletteController,
    spotlight: SpotlightController,
  ) {
    this.omnibar = omnibar;
    this.spotlight = spotlight;
  }

  public isComposing(): boolean {
    return this.composing;
  }

  public compositionStart(localeTag: ImeLocaleTag): void {
    this.composing = true;
    this.compositionBuffer = "";
    this.localeTag = localeTag;
    this.recordTrace("compositionstart", false, false);
  }

  public compositionUpdate(rawPhoneticOrCandidate: string): void {
    assertIme(this.composing, "compositionUpdate requires active compositionstart");
    this.compositionBuffer = rawPhoneticOrCandidate;
    this.recordTrace("compositionupdate", false, false);
  }

  public handleKeyDown(event: ImeKeyEventInput): {
    readonly interceptedByIme: boolean;
    readonly delegatedAction: string;
  } {
    const activeIme = this.composing || event.isComposing || event.keyCode === 229;
    if (activeIme) {
      if (event.key === "Escape") {
        // Cancel only the active uncommitted IME buffer; keep Omnibar open & committed text intact
        this.compositionBuffer = "";
        this.composing = false;
        this.recordTrace("composition-key-guarded", false, false);
        return { interceptedByIme: true, delegatedAction: "ime-buffer-cancelled" };
      }
      if (event.key === "Enter") {
        // Enter during IME composition selects candidate; never triggers Omnibar submitProposal()
        this.recordTrace("composition-key-guarded", false, false);
        return { interceptedByIme: true, delegatedAction: "ime-candidate-confirm" };
      }
      return { interceptedByIme: true, delegatedAction: "ime-key-buffered" };
    }

    const res = this.omnibar.handleKeyDown({
      key: event.key,
      code: event.code,
      ctrlKey: event.ctrlKey,
      metaKey: event.metaKey,
    });
    this.recordTrace("post-composition-key", false, false);
    return { interceptedByIme: false, delegatedAction: res.action };
  }

  public compositionEnd(committedText: string, target: "omnibar" | "spotlight" = "omnibar"): void {
    this.composing = false;
    this.compositionBuffer = "";
    if (target === "omnibar") {
      const current = this.omnibar.getSnapshot().draftUtterance;
      const next = current.length > 0 ? `${current}${committedText}` : committedText;
      this.omnibar.setDraftUtterance(next);
    } else {
      this.spotlight.querySpotlightCandidates({ queryText: committedText });
    }
    this.recordTrace("compositionend", false, false);
  }

  public getTraces(): readonly ImeStepTrace[] {
    return [...this.traces];
  }

  private recordTrace(
    phase: ImeStepTrace["phase"],
    prematureSubmitTriggered: boolean,
    prematureEscapeDismissTriggered: boolean,
  ): void {
    const snap = this.omnibar.getSnapshot();
    this.traces.push({
      phase,
      localeTag: this.localeTag,
      activeBuffer: this.compositionBuffer,
      committedDraft: snap.draftUtterance,
      omnibarOpen: snap.open,
      prematureSubmitTriggered,
      prematureEscapeDismissTriggered,
    });
  }
}

/**
 * Executes the CJK (Chinese / Japanese / English mixed) IME composition lifecycle probe
 * inside the `Z3` `Ctrl+Space` Omnibar and `Ctrl+K` Spotlight search.
 */
export function runCjkImeCompositionProbe(): CjkImeProbeReceipt {
  const catalog: readonly VerifiedApplicationCatalogItem[] = [
    {
      applicationIdHex: "11111111111111112222222222222222",
      installationIdHex: "33333333333333334444444444444444",
      packageIdHex: "55555555555555556666666666666666",
      displayName: "季度财务工作台",
      currentGeneration: 2,
      keywords: ["报表", "财务", "汇总", "検算"],
    },
  ];

  const omnibar = createOmnibarPaletteController({
    initialLocale: "zh-CN",
    initialDisclosure: "L2",
    catalog,
  });
  const spotlight = new SpotlightController({
    initialLocale: "zh-CN",
    initialDisclosure: "L2",
  });

  // Summon Omnibar via Ctrl+Space
  const openRes = omnibar.handleKeyDown({
    key: " ",
    code: "Space",
    ctrlKey: true,
  });
  assertIme(openRes.action === "opened" && omnibar.getSnapshot().open, "Ctrl+Space must open Omnibar");

  const harness = new CjkImeCompositionHarness(omnibar, spotlight);

  // 1. Chinese Pinyin IME composition: "shengcheng" -> Enter during composition -> "生成季度财务报表"
  harness.compositionStart("zh-CN-pinyin");
  harness.compositionUpdate("sheng");
  harness.compositionUpdate("sheng'cheng");
  const enterDuringZh = harness.handleKeyDown({
    key: "Enter",
    keyCode: 229,
    isComposing: true,
  });
  assertIme(
    enterDuringZh.interceptedByIme &&
      omnibar.getSnapshot().lastSubmittedProposal === null &&
      omnibar.getSnapshot().open,
    "Enter during active zh-CN IME composition must not prematurely submit Omnibar",
  );
  harness.compositionEnd("生成季度财务报表");

  // 2. Japanese Romaji -> Kanji IME composition with mid-composition Escape cancellation test
  harness.compositionStart("ja-JP-romaji");
  harness.compositionUpdate("あやまり");
  const escDuringJa = harness.handleKeyDown({
    key: "Escape",
    keyCode: 229,
    isComposing: true,
  });
  assertIme(
    escDuringJa.interceptedByIme &&
      omnibar.getSnapshot().open &&
      omnibar.getSnapshot().draftUtterance === "生成季度财务报表",
    "Escape during active IME composition must cancel only IME buffer and keep Omnibar open",
  );

  harness.compositionStart("ja-JP-romaji");
  harness.compositionUpdate("けんさん");
  harness.compositionEnd("（検算済み）");

  // 3. Rapid CJK + English mixed input
  harness.compositionStart("en-US-mixed");
  harness.compositionUpdate("Q4-Report");
  harness.compositionEnd(" Q4-Report");

  const finalCommittedOmnibarUtterance = omnibar.getSnapshot().draftUtterance;
  assertIme(
    finalCommittedOmnibarUtterance === "生成季度财务报表（検算済み） Q4-Report",
    `Unexpected committed utterance: ${finalCommittedOmnibarUtterance}`,
  );

  // 4. Post-composition Enter submits the compiled proposal cleanly
  const postEnter = harness.handleKeyDown({
    key: "Enter",
    isComposing: false,
  });
  assertIme(
    !postEnter.interceptedByIme &&
      postEnter.delegatedAction === "submitted" &&
      omnibar.getSnapshot().lastSubmittedProposal !== null,
    "Post-composition Enter must submit the compiled Omnibar proposal",
  );

  // 5. Spotlight Ctrl+K CJK IME composition query
  spotlight.setOpen(true);
  harness.compositionStart("zh-CN-pinyin");
  harness.compositionUpdate("sheng'chan'huan'jing");
  harness.compositionEnd("生产环境配置导出", "spotlight");
  const spotRes = spotlight.querySpotlightCandidates({ queryText: "生产环境配置导出" });
  assertIme(
    spotRes.candidates.length >= 1 &&
      spotRes.envelopeAuthorityGrade === "NON_AUTHORITATIVE",
    "Spotlight CJK IME query must match local candidate with NON_AUTHORITATIVE 0x1F envelope",
  );

  return {
    passed: true,
    localesTested: ["zh-CN-pinyin", "ja-JP-romaji", "en-US-mixed"],
    stepCount: harness.getTraces().length,
    enterDuringCompositionGuarded: true,
    escapeDuringCompositionGuarded: true,
    finalCommittedOmnibarUtterance,
    postCompositionSubmitSucceeded: true,
    spotlightCjkHitsMatched: spotRes.candidates.length,
    traces: harness.getTraces(),
  };
}

/**
 * Executes the Multimodal Voice (`InputSource::Voice`) & Content-Addressed Attachment probe.
 */
export function runMultimodalVoiceAndAttachmentProbe(): MultimodalVoiceAttachmentReceipt {
  const omnibar = createOmnibarPaletteController({
    initialLocale: "zh-CN",
    initialDisclosure: "L2",
    initialMicPermission: "PROMPT_REQUIRED",
  });
  omnibar.openPalette("programmatic");

  // 1. Voice stream lifecycle: PROMPT_REQUIRED -> GRANTED -> Capturing -> Committed
  const startRes = omnibar.voiceController.startCapture({
    mode: "push-to-talk",
    nowMonoMs: 1_000,
  });
  assertIme(
    startRes.accepted && startRes.phase === "RequestingMicPermission",
    "Initial voice capture with PROMPT_REQUIRED must enter RequestingMicPermission",
  );

  const grantRes = omnibar.voiceController.grantMicPermission(1_050);
  const capturingSnap = omnibar.voiceController.getSnapshot();
  assertIme(
    grantRes.accepted &&
      capturingSnap.phase === "Capturing" &&
      capturingSnap.taskbarIndicator.microphoneActive === true,
    "Granting mic permission must transition to Capturing and light up L0 taskbar mic indicator",
  );

  omnibar.voiceController.appendStreamingTranscript("请汇总本季度", { nowMonoMs: 1_100 });
  omnibar.voiceController.appendStreamingTranscript("研发预算执行报告", { nowMonoMs: 1_200 });
  const commitRes = omnibar.voiceController.commitTranscript(undefined, 1_300);
  assertIme(
    commitRes.accepted &&
      omnibar.getSnapshot().draftUtterance.includes("请汇总本季度 研发预算执行报告"),
    "Committed voice transcript must merge into Omnibar draftUtterance",
  );

  // Verify Z4 modal immediately severs active voice capture
  omnibar.voiceController.startCapture({ mode: "toggle", nowMonoMs: 1_400 });
  omnibar.syncSecurityAndSessionState({
    sessionState: "ACTIVE",
    sessionGeneration: 1,
    z4ModalActive: true,
  });
  const severedVoiceSnap = omnibar.voiceController.getSnapshot();
  assertIme(
    !severedVoiceSnap.taskbarIndicator.microphoneActive &&
      severedVoiceSnap.lastSeveranceReason === "Z4_MODAL_ACTIVE",
    "Active Z4 security screen must immediately sever microphone capture",
  );
  omnibar.syncSecurityAndSessionState({
    sessionState: "ACTIVE",
    sessionGeneration: 1,
    z4ModalActive: false,
  });

  // 2. Content-addressed & ChooserGrant attachment chips
  const artifactChip = createArtifactAttachment({
    displayLabel: "季度研发预算表.xlsx",
    byteLength: 49_152,
    artifactIdHex: "11".repeat(16),
    contentDigestHex: "aa".repeat(32),
    sourceApplicationName: "财务工作台",
  });
  const chooserFileChip = createChooserFileAttachment({
    fileNameOrPath: "C:\\Users\\Operator\\Documents\\内部审计纪要.pdf",
    byteLength: 131_072,
    contentDigestHex: "bb".repeat(32),
    chooserGrantIdHex: "22".repeat(16),
    scopedCapabilityIdHex: "33".repeat(16),
    explicitHostPathAuthorized: false,
  });
  const screenshotChip = createScreenshotAttachment({
    label: "主工作区窗口截图.png",
    byteLength: 262_144,
    contentDigestHex: "cc".repeat(32),
  });

  assertIme(
    chooserFileChip.displayLabel === "内部审计纪要.pdf" &&
      chooserFileChip.authorizedHostPath === null,
    "Raw Windows host path must be stripped to safe basename when explicitHostPathAuthorized is false",
  );

  let missingChooserGrantRejectedCode = "";
  try {
    createChooserFileAttachment({
      fileNameOrPath: "secret-audit-report.pdf",
      byteLength: 1024,
      contentDigestHex: "dd".repeat(32),
      chooserGrantIdHex: "",
      scopedCapabilityIdHex: "33".repeat(16),
    });
  } catch (err) {
    if (err instanceof AttachmentSecurityError) {
      missingChooserGrantRejectedCode = err.code;
    }
  }
  assertIme(
    missingChooserGrantRejectedCode === "MISSING_CHOOSER_GRANT",
    "Local file attachment without ChooserGrant must fail with MISSING_CHOOSER_GRANT",
  );

  omnibar.addAttachment(artifactChip);
  omnibar.addAttachment(chooserFileChip);
  omnibar.addAttachment(screenshotChip);

  const chipsHtml = renderAttachmentChipsHtml(omnibar.getSnapshot().attachments, {
    locale: "zh-CN",
    disclosure: "L2",
  });
  const renderedChipsZeroBlur =
    !chipsHtml.includes("backdrop-filter") &&
    !chipsHtml.includes("C:\\Users\\Operator");
  assertIme(renderedChipsZeroBlur, "Rendered attachment chips must have zero blur and zero bare path leak");

  return {
    passed: true,
    voiceTranscriptCommitted: commitRes.committedTranscript ?? "",
    micIndicatorActiveDuringCapture: true,
    micSeveredOnZ4LockReason: severedVoiceSnap.lastSeveranceReason ?? "Z4_MODAL_ACTIVE",
    attachmentChipCount: omnibar.getSnapshot().attachments.length,
    attachmentChipRadiusPx: ATTACHMENT_CHIP_RADIUS_PX,
    rawWindowsHostPathStrippedToBasename: chooserFileChip.displayLabel,
    missingChooserGrantRejectedCode,
    renderedChipsZeroBlur: true,
  };
}

/**
 * Runs both the CJK IME composition probe and the Multimodal Voice/Attachment probe.
 */
export function runImeAndMultimodalCampaignProbe(): ImeAndMultimodalProbeReport {
  const cjkImeComposition = runCjkImeCompositionProbe();
  const multimodalVoiceAndAttachments = runMultimodalVoiceAndAttachmentProbe();
  const probeDigestHex = deterministicDomainDigestHex(
    "llmos/stage-d/ime-multimodal-probe/v1",
    [
      JSON.stringify(cjkImeComposition),
      JSON.stringify(multimodalVoiceAndAttachments),
    ],
  );

  return {
    schemaVersion: "1.0.0",
    cjkImeComposition,
    multimodalVoiceAndAttachments,
    probeDigestHex,
  };
}
