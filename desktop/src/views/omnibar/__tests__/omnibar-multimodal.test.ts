// Comprehensive Deterministic Verification & Anti-AI Audit Suite for D-OMNIBAR-MULTIMODAL (W71-D-OMNIBAR).
// Run from repo root: npx tsx desktop/src/views/omnibar/__tests__/omnibar-multimodal.test.ts

import { createWorkstationCompositor } from "../../../compositor";
import type { DeviceHardwareCaptureState } from "../../../dock";
import {
  ATTACHMENT_CHIP_RADIUS_PX,
  AttachmentSecurityError,
  createArtifactAttachment,
  createChooserFileAttachment,
  createScreenshotAttachment,
  createVoiceInputController,
  projectDevicePermissionToTaskbar,
  renderAttachmentChipsHtml,
} from "../../../multimodal";
import { SEARCH_NON_AUTHORITATIVE_RESTRICTIONS_ALL } from "../../../types";
import type { DisclosureLevel, WorkstationLocale } from "../../../ui";
import {
  ACTION_PROPOSAL_SCHEMA_ID,
  ClarificationRequiredError,
  IntentBindingError,
  KEYPOOL_ISOLATION_UNVERIFIED_TAG,
  OMNIBAR_BORDER_RADIUS_PX,
  OMNIBAR_TASKBAR_TRIGGER_HEIGHT_PX,
  OMNIBAR_TOP_PERMANENT_FOOTPRINT_PX,
  OMNIBAR_Z_INDEX,
  TYPED_INTENT_SCHEMA_ID,
  TYPED_INTENT_SCHEMA_VERSION,
  bindOmnibarToCompositor,
  compileIntentAndProposal,
  createOmnibarPaletteController,
  finalizeProposalForSubmission,
  renderOmnibarPaletteHtml,
  selectAmbiguityCandidate,
  type KeyPoolIntentAssistDto,
  type KeyPoolStatusDto,
  type VerifiedApplicationCatalogItem,
} from "../index";

/* ==========================================================================
   Node Builtin Loader (Zero @types/node Dependency for Strict DOM tsconfig)
   ========================================================================== */

interface DirentLike {
  readonly name: string;
  isDirectory(): boolean;
  isFile(): boolean;
}

interface NodeFsLike {
  readFileSync(path: string, encoding: "utf-8"): string;
  readdirSync(path: string, options: { withFileTypes: true }): DirentLike[];
}

interface NodePathLike {
  resolve(...segments: string[]): string;
  dirname(p: string): string;
  basename(p: string): string;
  join(...segments: string[]): string;
}

interface NodeUrlLike {
  fileURLToPath(url: string): string;
}

const importBuiltin = new Function(
  "specifier",
  "return import(specifier);",
) as <T>(specifier: string) => Promise<T>;

/* ==========================================================================
   Minimal Headless DOM & Canvas Mock
   ========================================================================== */

class MockTextNode {
  public readonly nodeType = 3;
  public textContent: string;
  constructor(text: string) {
    this.textContent = text;
  }
}

class MockElement {
  public readonly nodeType = 1;
  public readonly tagName: string;
  public readonly namespaceURI: string | null;
  public className = "";
  public title = "";
  public type = "";
  public value = "";
  public placeholder = "";
  public disabled = false;
  public hidden = false;
  public tabIndex = 0;
  public width = 0;
  public height = 0;
  public focusedCount = 0;
  public readonly style: Record<string, string> = {};
  private _textContent = "";
  private readonly attrs = new Map<string, string>();
  private readonly childrenList: Array<MockElement | MockTextNode> = [];
  private readonly listeners = new Map<string, Array<(ev: unknown) => void>>();
  public canvasContext: unknown = null;

  constructor(tagName: string, namespaceURI: string | null = null) {
    this.tagName = tagName;
    this.namespaceURI = namespaceURI;
  }

  get textContent(): string {
    if (this.childrenList.length === 0) {
      return this._textContent;
    }
    return this.childrenList.map((c) => c.textContent).join("");
  }

  set textContent(val: string) {
    this._textContent = val;
    this.childrenList.length = 0;
    if (val.length > 0) {
      this.childrenList.push(new MockTextNode(val));
    }
  }

  get childNodes(): ReadonlyArray<MockElement | MockTextNode> {
    return this.childrenList;
  }

  get children(): readonly MockElement[] {
    return this.childrenList.filter((c): c is MockElement => c instanceof MockElement);
  }

  setAttribute(name: string, value: string): void {
    this.attrs.set(name, value);
    if (name === "class") {
      this.className = value;
    }
    if (name === "hidden") {
      this.hidden = true;
    }
    if (name === "disabled") {
      this.disabled = true;
    }
  }

  removeAttribute(name: string): void {
    this.attrs.delete(name);
    if (name === "hidden") {
      this.hidden = false;
    }
    if (name === "disabled") {
      this.disabled = false;
    }
  }

  getAttribute(name: string): string | null {
    if (name === "class" && !this.attrs.has("class") && this.className.length > 0) {
      return this.className;
    }
    return this.attrs.get(name) ?? null;
  }

  appendChild<T extends MockElement | MockTextNode>(child: T): T {
    this.childrenList.push(child);
    return child;
  }

  replaceChildren(...nodes: Array<MockElement | MockTextNode>): void {
    this.childrenList.length = 0;
    this._textContent = "";
    this.childrenList.push(...nodes);
  }

  addEventListener(eventType: string, cb: (ev: unknown) => void): void {
    const list = this.listeners.get(eventType) ?? [];
    list.push(cb);
    this.listeners.set(eventType, list);
  }

  dispatch(eventType: string, payload: unknown = {}): void {
    const list = this.listeners.get(eventType) ?? [];
    for (const fn of list) {
      fn(payload);
    }
  }

  click(): void {
    this.dispatch("click", {
      type: "click",
      target: this,
      stopPropagation: () => undefined,
    });
  }

  focus(): void {
    this.focusedCount += 1;
  }

  getContext(contextId: string): unknown {
    if (contextId === "2d") {
      return this.canvasContext;
    }
    return null;
  }

  findAll(predicate: (el: MockElement) => boolean): MockElement[] {
    const out: MockElement[] = [];
    if (predicate(this)) {
      out.push(this);
    }
    for (const child of this.childrenList) {
      if (child instanceof MockElement) {
        out.push(...child.findAll(predicate));
      }
    }
    return out;
  }
}

function createMockDocument(): {
  doc: Document;
  createHost: () => MockElement;
} {
  const noop2d = {
    strokeStyle: "#000000",
    fillStyle: "#000000",
    lineWidth: 1,
    clearRect: () => undefined,
    beginPath: () => undefined,
    moveTo: () => undefined,
    lineTo: () => undefined,
    stroke: () => undefined,
    fillRect: () => undefined,
  };
  const mockDoc = {
    createElement: (tag: string) => {
      const node = new MockElement(tag.toLowerCase(), null);
      if (tag.toLowerCase() === "canvas") {
        node.canvasContext = noop2d;
      }
      return node;
    },
    createElementNS: (ns: string, tag: string) => new MockElement(tag, ns),
    createTextNode: (text: string) => new MockTextNode(text),
  };
  return {
    doc: mockDoc as unknown as Document,
    createHost: () => new MockElement("div", null),
  };
}

/* ==========================================================================
   Test Harness & Static Anti-AI Audit Helpers
   ========================================================================== */

let passedAssertions = 0;
let passedSuites = 0;

function assert(condition: boolean, message: string): void {
  if (!condition) {
    throw new Error(`Assertion failed: ${message}`);
  }
  passedAssertions += 1;
}

async function runSuite(name: string, fn: () => void | Promise<void>): Promise<void> {
  await fn();
  passedSuites += 1;
  console.log(`  [PASS] ${name}`);
}

const ALLOWED_RADII = new Set(["0px", "2px", "4px", "6px"]);
const PICTOGRAPHIC_RE = /\p{Extended_Pictographic}/u;

const BANNED_SUBSTRINGS: readonly string[] = [
  ["backdrop", "-filter"].join(""),
  ["-webkit-", "backdrop", "-filter"].join(""),
  ["#030", "710"].join(""),
  ["#7fc", "8ff"].join(""),
  ["radial", "-gradient"].join(""),
  ["max-w-", "7xl"].join(""),
];

const L1_L2_BANNED_JARGON: readonly string[] = [
  "Z1",
  "Z2",
  "Z3",
  "Z4",
  "TypedIntent",
  "0x1F",
  "0x1f",
  "Quota",
  "quota",
  "ROAD-D-001",
  "ADR-0011",
  "TaskAttempt",
  "ExecutionFiber",
  "Cell",
  "receipt_hex",
  "control_command_id",
  "CAS_WINNER",
  "SHARED_THREAD_LIKE",
  "LOCAL_SINGLE_NODE",
];

function collectProductionFiles(
  fs: NodeFsLike,
  pathMod: NodePathLike,
  dir: string,
): string[] {
  const out: string[] = [];
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    if (entry.name === "__tests__") {
      continue;
    }
    const full = pathMod.join(dir, entry.name);
    if (entry.isDirectory()) {
      out.push(...collectProductionFiles(fs, pathMod, full));
    } else if (
      entry.isFile() &&
      (entry.name.endsWith(".ts") || entry.name.endsWith(".css"))
    ) {
      out.push(full);
    }
  }
  return out.sort();
}

function auditFileForAntiAiViolations(
  filePath: string,
  content: string,
): string[] {
  const issues: string[] = [];
  const lower = content.toLowerCase();

  for (const banned of BANNED_SUBSTRINGS) {
    if (lower.includes(banned.toLowerCase())) {
      issues.push(`${filePath}: contains banned anti-AI token '${banned}'`);
    }
  }

  if (PICTOGRAPHIC_RE.test(content)) {
    issues.push(`${filePath}: contains banned Extended_Pictographic emoji character`);
  }

  const radiusDeclRe =
    /(?:border(?:-(?:top|bottom)-(?:left|right))?-radius|--nlos-radius-[a-z0-9-]+)\s*:\s*([^;}"'`]+)/gi;
  let match: RegExpExecArray | null = radiusDeclRe.exec(content);
  while (match !== null) {
    const rawVal = match[1]!.trim();
    if (!rawVal.startsWith("var(--nlos-radius-")) {
      const tokens = rawVal.split(/\s+/).filter((t) => t !== "!important");
      for (const tok of tokens) {
        if (!ALLOWED_RADII.has(tok)) {
          issues.push(`${filePath}: illegal border-radius token '${tok}' in '${match[0]}'`);
        }
      }
    }
    match = radiusDeclRe.exec(content);
  }

  return issues;
}

function sampleCatalog(): readonly VerifiedApplicationCatalogItem[] {
  return [
    {
      applicationIdHex: "41".repeat(16),
      installationIdHex: "51".repeat(16),
      packageIdHex: "61".repeat(16),
      knownProcessIdHexes: ["71".repeat(16)],
      currentGeneration: 3,
      displayName: "文档工作台",
      keywords: ["document", "editor", "纪要", "笔记"],
      defaultScopeSummary: "主工作区 · 文档工作台",
    },
    {
      applicationIdHex: "42".repeat(16),
      installationIdHex: "52".repeat(16),
      packageIdHex: "62".repeat(16),
      knownProcessIdHexes: ["72".repeat(16)],
      currentGeneration: 5,
      displayName: "表格分析台",
      keywords: ["sheet", "table", "报表", "笔记"],
      defaultScopeSummary: "主工作区 · 表格分析台",
    },
  ];
}

/* ==========================================================================
   Main Verification Suites
   ========================================================================== */

async function main(): Promise<void> {
  console.log("Running Stage D Omnibar & Multimodal Verification Suite (W71-D-OMNIBAR)...");

  await runSuite(
    "1. On-demand Z3 palette geometry (z=460, 4px radius, 0px top footprint), Ctrl+Space/Cmd+Space/26px trigger, and Escape/outside draft preservation",
    () => {
      const { doc, createHost } = createMockDocument();
      const host = createHost();
      const ctrl = createOmnibarPaletteController({
        catalog: sampleCatalog(),
        doc,
      });
      ctrl.mount(host as unknown as HTMLElement, { doc });

      const initSnap = ctrl.getSnapshot();
      assert(initSnap.open === false, "Palette must be hidden by default");
      assert(initSnap.zIndex === OMNIBAR_Z_INDEX && initSnap.zIndex === 460, "Z3 palette z-index must be 460");
      assert(
        initSnap.borderRadiusPx === OMNIBAR_BORDER_RADIUS_PX && initSnap.borderRadiusPx === 4,
        "Omnibar border radius must be 4px",
      );
      assert(
        initSnap.topPermanentFootprintPx === OMNIBAR_TOP_PERMANENT_FOOTPRINT_PX &&
          initSnap.topPermanentFootprintPx === 0,
        "Permanent top-of-screen footprint must be strictly 0px",
      );
      assert(
        initSnap.taskbarTriggerHeightPx === OMNIBAR_TASKBAR_TRIGGER_HEIGHT_PX &&
          initSnap.taskbarTriggerHeightPx === 26,
        "Taskbar trigger slot height must be 26px",
      );

      // Untrusted surface attempting to spoof Ctrl+Space must be blocked.
      const spoofRes = ctrl.handleKeyDown({
        key: " ",
        ctrlKey: true,
        fromUntrustedSurface: true,
      });
      assert(spoofRes.action === "blocked", "Untrusted Ctrl+Space spoof must be blocked");
      assert(ctrl.getSnapshot().open === false, "Palette remains closed after spoof");

      // Trusted Ctrl+Space opens palette.
      const openRes = ctrl.handleKeyDown({ key: " ", ctrlKey: true });
      assert(openRes.handled && openRes.action === "opened", "Ctrl+Space opens palette");
      assert(ctrl.getSnapshot().open === true, "Palette is open");
      assert(ctrl.getSnapshot().lastOpenSource === "keyboard-shortcut", "Open source recorded");

      // Enter draft & attachment, then dismiss via Escape -> draft & attachment preserved!
      ctrl.setDraftUtterance("整理季度产品评审纪要");
      ctrl.addAttachment(
        createScreenshotAttachment({
          label: "review-slide.png",
          byteLength: 4096,
          contentDigestHex: "aa".repeat(32),
        }),
      );

      const escRes = ctrl.handleKeyDown({ key: "Escape" });
      assert(escRes.handled && escRes.action === "closed-escape", "Escape closes palette");
      assert(ctrl.getSnapshot().open === false, "Palette closed after Escape");
      assert(
        ctrl.getSnapshot().draftUtterance === "整理季度产品评审纪要",
        "Draft utterance preserved on Escape",
      );
      assert(
        ctrl.getSnapshot().attachments.length === 1,
        "Draft attachments preserved on Escape",
      );

      // Re-open via Cmd+Space (metaKey), then dismiss via outside click -> draft still preserved!
      const cmdOpen = ctrl.handleKeyDown({ key: " ", metaKey: true });
      assert(cmdOpen.action === "opened", "Cmd+Space opens palette");
      const scrimNode = host.children[0]!;
      scrimNode.dispatch("click", { target: scrimNode });
      assert(ctrl.getSnapshot().open === false, "Outside scrim click dismisses palette");
      assert(ctrl.getSnapshot().lastCloseReason === "outside-click", "Outside click close reason");
      assert(
        ctrl.getSnapshot().draftUtterance === "整理季度产品评审纪要",
        "Draft preserved on outside click",
      );

      // Re-open via 26px taskbar trigger slot, then explicitly clear draft.
      assert(ctrl.openFromTaskbarSlot() === true, "26px taskbar trigger opens palette");
      assert(ctrl.getSnapshot().lastOpenSource === "taskbar-trigger", "Taskbar open source");
      ctrl.clearDraft();
      assert(ctrl.getSnapshot().draftUtterance === "", "clearDraft empties utterance");
      assert(ctrl.getSnapshot().attachments.length === 0, "clearDraft empties attachments");
    },
  );

  await runSuite(
    "2. L1 / L2 zero-jargon audit (ANTI-AI-07) across zh-CN & en-US + reversibility badges + L2 Nomad BulletMicroBar",
    () => {
      const { doc, createHost } = createMockDocument();
      const host = createHost();
      const locales: readonly WorkstationLocale[] = ["zh-CN", "en-US"];
      const levels: readonly DisclosureLevel[] = ["L1", "L2"];

      const ctrl = createOmnibarPaletteController({
        catalog: sampleCatalog(),
        doc,
      });
      ctrl.mount(host as unknown as HTMLElement, { doc });
      ctrl.openPalette("keyboard-shortcut");

      for (const locale of locales) {
        ctrl.setLocale(locale);
        ctrl.addAttachment(
          createChooserFileAttachment({
            fileNameOrPath: "C:\\Users\\Alice\\Desktop\\meeting-notes.md",
            byteLength: 2048,
            contentDigestHex: "bc".repeat(32),
            chooserGrantIdHex: "11".repeat(16),
            scopedCapabilityIdHex: "22".repeat(16),
            taintFlags: ["EXTERNAL_ORIGIN"],
          }),
        );

        for (const utterance of [
          locale === "en-US" ? "summarize document notes" : "汇总文档工作台会议纪要",
          locale === "en-US" ? "delete old backup permanently" : "永久移除过期备份文件",
        ]) {
          ctrl.setDraftUtterance(utterance);

          for (const level of levels) {
            ctrl.setDisclosure(level);
            const html = renderOmnibarPaletteHtml(ctrl.getSnapshot());
            const domVisible = ctrl.getVisibleTextForAudit();

            for (const banned of L1_L2_BANNED_JARGON) {
              assert(
                !domVisible.includes(banned),
                `DOM visible text in ${locale}@${level} leaked banned jargon '${banned}': ${domVisible}`,
              );
              const strippedHtml = html.replace(/<[^>]*>/g, " ");
              assert(
                !strippedHtml.includes(banned),
                `Rendered HTML text in ${locale}@${level} leaked banned jargon '${banned}': ${strippedHtml}`,
              );
            }

            if (level === "L2") {
              assert(
                html.includes("nlos-bullet-bar"),
                `L2 view in ${locale} must render Nomad triple-watermark BulletMicroBar`,
              );
            }
          }
        }
      }

      // Check all 3 reversibility badge labels in zh-CN & en-US
      const zhLossless = compileIntentAndProposal({
        utterance: "查看文档工作台",
        catalog: sampleCatalog(),
        locale: "zh-CN",
      });
      assert(zhLossless.proposal.undoBadgeLabel === "可无损撤销", "zh-CN lossless badge");

      const zhComp = compileIntentAndProposal({
        utterance: "转发到项目归档",
        catalog: sampleCatalog(),
        locale: "zh-CN",
      });
      assert(zhComp.proposal.undoBadgeLabel === "可补偿回滚", "zh-CN compensatable badge");

      const zhIrrev = compileIntentAndProposal({
        utterance: "永久移除历史记录",
        catalog: sampleCatalog(),
        locale: "zh-CN",
      });
      assert(zhIrrev.proposal.undoBadgeLabel === "不可逆", "zh-CN irreversible badge");

      const enIrrev = compileIntentAndProposal({
        utterance: "delete archive permanently",
        catalog: sampleCatalog(),
        locale: "en-US",
      });
      assert(enIrrev.proposal.undoBadgeLabel === "Irreversible", "en-US irreversible badge");
    },
  );

  await runSuite(
    "3. L3 diagnostic fidelity (TypedIntent schema/version, ActionProposal digest, ControlScope, IdempotencyKey, handles, ControlCommand preview)",
    () => {
      const { doc, createHost } = createMockDocument();
      const host = createHost();
      const ctrl = createOmnibarPaletteController({
        catalog: sampleCatalog(),
        initialDisclosure: "L1",
        doc,
      });
      ctrl.mount(host as unknown as HTMLElement, { doc });
      ctrl.openPalette("keyboard-shortcut");
      ctrl.setDraftUtterance("打开文档工作台");

      // In L1, L3 diagnostic panel must be hidden and omitted from visible text.
      assert(
        !ctrl.getVisibleTextForAudit().includes(TYPED_INTENT_SCHEMA_ID),
        "L1 must not expose TypedIntent schema ID",
      );

      // Transition to L3 -> full TypedIntent / ActionProposal diagnostic envelope appears.
      ctrl.setDisclosure("L3");
      const snap = ctrl.getSnapshot();
      const bundle = snap.compiledBundle!;
      assert(bundle.typedIntent.schemaId === TYPED_INTENT_SCHEMA_ID, "schemaId = nlos.intent.v1");
      assert(bundle.typedIntent.schemaVersion === TYPED_INTENT_SCHEMA_VERSION, "schemaVersion = 1.0");
      assert(
        bundle.proposal.proposalSchemaId === ACTION_PROPOSAL_SCHEMA_ID,
        "proposalSchemaId = nlos.action-proposal.v1",
      );

      const l3Visible = ctrl.getVisibleTextForAudit();
      assert(l3Visible.includes("TypedIntent: schema=nlos.intent.v1@1.0"), "L3 shows TypedIntent schema");
      assert(l3Visible.includes(bundle.proposal.proposalDigestHex), "L3 shows proposalDigestHex");
      assert(l3Visible.includes(`ControlScope=${bundle.typedIntent.controlScope}`), "L3 shows ControlScope");
      assert(
        l3Visible.includes(`IdempotencyKey=${bundle.typedIntent.idempotencyKeyHex}`),
        "L3 shows IdempotencyKey",
      );
      assert(
        l3Visible.includes("ApplicationHandle=(app=41414141"),
        "L3 shows verified (ApplicationId, InstallationId, generation) tuple",
      );
      assert(
        l3Visible.includes("ControlCommandPreview=launch_application("),
        "L3 shows compiled ControlCommand preview",
      );
    },
  );

  await runSuite(
    "4. [NL-AMBIG-001] Mandatory Ambiguity Clarification Gate: hard-blocks before candidate selection & unlocks with bound digest after selection",
    () => {
      const { doc, createHost } = createMockDocument();
      const host = createHost();
      const ctrl = createOmnibarPaletteController({
        catalog: sampleCatalog(),
        effectiveBudgetCapUnits: 100,
        doc,
      });
      ctrl.mount(host as unknown as HTMLElement, { doc });
      ctrl.openPalette("keyboard-shortcut");

      // Utterance triggers all 4 ambiguity classes:
      //   1. MULTIPLE_MATCHING_TARGETS ("笔记" matches both 文档工作台 & 表格分析台)
      //   2. HIGH_RISK_IRREVERSIBLE_EFFECT ("永久移除")
      //   3. CROSS_APPLICATION_TRANSFER ("转发到")
      //   4. BUDGET_ESCALATION ("扩容预算")
      ctrl.setDraftUtterance("永久移除旧笔记并转发到外部归档且扩容预算");
      const beforeSelect = ctrl.getSnapshot().compiledBundle!;
      const kinds = beforeSelect.proposal.unresolvedAmbiguities.map((a) => a.kind);
      assert(kinds.includes("MULTIPLE_MATCHING_TARGETS"), "Detects multiple matching targets");
      assert(kinds.includes("HIGH_RISK_IRREVERSIBLE_EFFECT"), "Detects high-risk irreversible effect");
      assert(kinds.includes("CROSS_APPLICATION_TRANSFER"), "Detects cross-app transfer");
      assert(kinds.includes("BUDGET_ESCALATION"), "Detects budget escalation");
      assert(beforeSelect.proposal.candidates.length >= 2, "Renders >= 2 candidate interpretation rows");
      assert(beforeSelect.proposal.canSubmit === false, "canSubmit is false before candidate selection");

      // Direct finalizeProposalForSubmission & controller.submitProposal MUST throw ClarificationRequiredError!
      let caughtClarification = false;
      try {
        ctrl.submitProposal();
      } catch (err) {
        if (err instanceof ClarificationRequiredError && err.code === "CLARIFICATION_REQUIRED") {
          caughtClarification = true;
        }
      }
      assert(caughtClarification, "Hard gate must throw ClarificationRequiredError before candidate selection");
      assert(ctrl.getSnapshot().open === true, "Palette stays open showing candidate comparison table");

      // Pressing Enter while ambiguous must also be blocked.
      const enterRes = ctrl.handleKeyDown({ key: "Enter" });
      assert(enterRes.action === "blocked", "Enter key submission blocked while ambiguous");

      // Click the second candidate row in the mounted DOM comparison table!
      const candidateRows = host.findAll(
        (el) => el.getAttribute("data-candidate-id") !== null,
      );
      assert(candidateRows.length >= 2, "Candidate rows rendered in DOM");
      const chosenRowEl = candidateRows[1]!;
      const chosenId = chosenRowEl.getAttribute("data-candidate-id")!;
      chosenRowEl.click();

      const afterSelect = ctrl.getSnapshot().compiledBundle!;
      assert(afterSelect.proposal.canSubmit === true, "canSubmit unlocks after explicit candidate click");
      assert(afterSelect.proposal.selectedCandidateId === chosenId, "selectedCandidateId bound");
      assert(
        typeof afterSelect.proposal.clarificationDigestHex === "string" &&
          afterSelect.proposal.clarificationDigestHex.length === 64,
        "64-hex clarificationDigestHex deterministically bound",
      );

      // Determinism check via pure helper
      const pureSelected = selectAmbiguityCandidate(
        {
          utterance: "永久移除旧笔记并转发到外部归档且扩容预算",
          catalog: sampleCatalog(),
          effectiveBudgetCapUnits: 100,
        },
        chosenId,
      );
      const finalized = finalizeProposalForSubmission(pureSelected);
      assert(finalized.selectedCandidateId === chosenId, "Pure finalize preserves selectedCandidateId");

      const submitted = ctrl.submitProposal();
      assert(submitted.selectedCandidateId === chosenId, "Submitted proposal carries selectedCandidateId");
      assert(
        submitted.clarificationDigestHex === afterSelect.proposal.clarificationDigestHex,
        "Submitted proposal carries bound clarificationDigestHex",
      );
      assert(
        submitted.handoffTarget === "Z4_TRUSTED_CONSENT",
        "High-risk clarified proposal hands off to Z4 trusted consent",
      );
    },
  );

  await runSuite(
    "5. [NL-BIND-001] & [DUI-LAUNCH-001] rejection of mutable 'latest' tag, 0x1F search hit, PackageId/ProcessId confusion, and stale generation",
    () => {
      const catalog = sampleCatalog();

      const expectBindingCode = (fn: () => unknown, expectedCode: string): void => {
        let matched = false;
        try {
          fn();
        } catch (err) {
          if (err instanceof IntentBindingError && err.code === expectedCode) {
            matched = true;
            for (const banned of L1_L2_BANNED_JARGON) {
              assert(
                !err.userMessage.includes(banned),
                `IntentBindingError userMessage must be zero-jargon, found '${banned}'`,
              );
            }
          }
        }
        assert(matched, `Expected IntentBindingError with code '${expectedCode}'`);
      };

      // 1. Symbolic mutable "latest" tag rejected
      expectBindingCode(
        () =>
          compileIntentAndProposal({
            utterance: "打开文档工作台",
            catalog,
            explicitDependency: { versionTag: "latest" },
          }),
        "MUTABLE_LATEST_TAG_FORBIDDEN",
      );

      expectBindingCode(
        () =>
          compileIntentAndProposal({
            utterance: "run editor@latest on draft",
            catalog,
          }),
        "MUTABLE_LATEST_TAG_FORBIDDEN",
      );

      // 2. Non-authoritative 0x1F search hit rejected as direct execution authority (U-7/U-8)
      expectBindingCode(
        () =>
          compileIntentAndProposal({
            utterance: "打开文档工作台",
            catalog,
            explicitDependency: {
              applicationIdHex: "41".repeat(16),
              installationIdHex: "51".repeat(16),
              fromNonAuthoritativeSearchHit: true,
              searchRestrictionBits: SEARCH_NON_AUTHORITATIVE_RESTRICTIONS_ALL,
            },
          }),
        "NON_AUTHORITATIVE_SEARCH_HIT_FORBIDDEN",
      );

      // 3. PackageId passed as ApplicationId rejected ([DUI-LAUNCH-001])
      expectBindingCode(
        () =>
          compileIntentAndProposal({
            utterance: "启动应用",
            catalog,
            explicitDependency: {
              rawIdentifierHex: "61".repeat(16),
            },
          }),
        "PACKAGE_ID_NOT_APPLICATION_IDENTITY",
      );

      // 4. Transient ProcessId passed as ApplicationId rejected ([DUI-LAUNCH-001])
      expectBindingCode(
        () =>
          compileIntentAndProposal({
            utterance: "启动应用",
            catalog,
            explicitDependency: {
              rawIdentifierHex: "71".repeat(16),
            },
          }),
        "PROCESS_ID_NOT_APPLICATION_IDENTITY",
      );

      // 5. Stale target generation rejected
      expectBindingCode(
        () =>
          compileIntentAndProposal({
            utterance: "打开文档工作台",
            catalog,
            explicitDependency: {
              applicationIdHex: "41".repeat(16),
              installationIdHex: "51".repeat(16),
              expectedGeneration: 2,
            },
          }),
        "STALE_TARGET_GENERATION",
      );
    },
  );

  await runSuite(
    "6. InputSource::Voice state machine, DevicePermissionState L0 tray projection, and automatic severance under Z4 / locked / suspended session",
    () => {
      const trayChanges: DeviceHardwareCaptureState[] = [];
      let committedOut = "";

      const voice = createVoiceInputController({
        initialMicPermission: "PROMPT_REQUIRED",
        onHardwareCaptureChange: (ind) => {
          trayChanges.push(ind);
        },
        onTranscriptCommitted: (text) => {
          committedOut = text;
        },
      });

      assert(voice.getSnapshot().phase === "Idle", "Starts Idle");

      // Untrusted surface attempting to start voice capture is rejected
      const spoofAttempt = voice.startCapture({ fromUntrustedSurface: true, nowMonoMs: 10 });
      assert(
        !spoofAttempt.accepted && spoofAttempt.severanceReason === "UNTRUSTED_ORIGIN_SPOOF",
        "Untrusted voice spoof rejected",
      );

      // Idle -> RequestingMicPermission -> Capturing -> Transcribing -> Committed
      const reqRes = voice.startCapture({ mode: "push-to-talk", nowMonoMs: 20 });
      assert(reqRes.accepted && reqRes.phase === "RequestingMicPermission", "Enters RequestingMicPermission");
      assert(
        voice.getSnapshot().taskbarIndicator.microphoneActive === false,
        "Mic indicator remains off until permission granted",
      );

      const grantRes = voice.grantMicPermission(30);
      assert(grantRes.accepted && grantRes.phase === "Capturing", "Transitions to Capturing once granted");
      assert(
        voice.getSnapshot().taskbarIndicator.microphoneActive === true,
        "L0 taskbar mic indicator turns active during Capturing",
      );

      voice.setScreenCapture("GRANTED", true);
      const projected = projectDevicePermissionToTaskbar(voice.getSnapshot().devicePermissions);
      assert(
        Boolean(projected.microphoneActive) &&
          Boolean(projected.screenActive) &&
          !projected.cameraActive,
        "DevicePermissionState projects (mic=true, screen=true, cam=false) to L0 taskbar",
      );

      voice.appendStreamingTranscript("查看今日构建任务", { nowMonoMs: 40 });
      assert(
        voice.getSnapshot().streamingTranscript === "查看今日构建任务",
        "Streaming transcript accumulates",
      );

      const transRes = voice.finishCaptureToTranscribing(50);
      assert(transRes.accepted && transRes.phase === "Transcribing", "Transitions to Transcribing");
      assert(
        voice.getSnapshot().taskbarIndicator.microphoneActive === false,
        "Mic hardware released on Transcribing",
      );

      const commitRes = voice.commitTranscript(undefined, 60);
      assert(commitRes.accepted && commitRes.phase === "Committed", "Transitions to Committed");
      assert(committedOut === "查看今日构建任务", "Committed transcript delivered");

      // Start capturing again, then activate Z4 modal -> immediate severance!
      voice.startCapture({ nowMonoMs: 70 });
      assert(voice.getSnapshot().phase === "Capturing", "Capturing active before Z4");
      const z4Snap = voice.syncSecurityAndSessionGate({
        sessionState: "ACTIVE",
        z4ModalActive: true,
      });
      assert(z4Snap.phase === "Cancelled", "Active voice severed to Cancelled when Z4 modal opens");
      assert(z4Snap.lastSeveranceReason === "Z4_MODAL_ACTIVE", "Z4 severance reason recorded");
      assert(
        !z4Snap.taskbarIndicator.microphoneActive && !z4Snap.taskbarIndicator.screenActive,
        "Hardware capture indicators cleared on Z4 severance",
      );

      // Clear Z4, start capturing again, then lock/suspend session -> immediate severance!
      voice.syncSecurityAndSessionGate({ sessionState: "ACTIVE", z4ModalActive: false });
      voice.startCapture({ nowMonoMs: 100 });
      assert(voice.getSnapshot().phase === "Capturing", "Capturing resumed after Z4 cleared");
      const lockedSnap = voice.syncSecurityAndSessionGate({
        sessionState: "LOCKED",
        sessionGeneration: 2,
        z4ModalActive: false,
      });
      assert(lockedSnap.phase === "Cancelled", "Active voice severed when session becomes LOCKED");
      assert(lockedSnap.lastSeveranceReason === "SESSION_NOT_ACTIVE", "Session severance reason recorded");
      assert(lockedSnap.severedCount === 2, "Both Z4 and LOCKED severances counted");

      // Denied transition
      voice.syncSecurityAndSessionGate({ sessionState: "ACTIVE", sessionGeneration: 2, z4ModalActive: false });
      const denyRes = voice.denyMicPermission();
      assert(denyRes.phase === "Denied", "Denying mic permission transitions to Denied");
      assert(trayChanges.length > 0, "Taskbar hardware indicator callbacks fired");
    },
  );

  await runSuite(
    "7. Multimodal 4px attachment chips: ChooserGrant enforcement, host path non-leakage, Artifact digest binding, and taint propagation",
    () => {
      // Raw host paths without ChooserGrant MUST throw RAW_HOST_PATH_FORBIDDEN
      for (const rawPath of [
        "C:\\Users\\Alice\\Secrets\\keys.pem",
        "\\\\corp-nas\\finance\\payroll.csv",
        "/Users/alice/.ssh/id_ed25519",
        "file:///home/alice/private.db",
      ]) {
        let blocked = false;
        try {
          createChooserFileAttachment({
            fileNameOrPath: rawPath,
            byteLength: 128,
            contentDigestHex: "12".repeat(32),
            chooserGrantIdHex: "",
            scopedCapabilityIdHex: "",
          });
        } catch (err) {
          if (err instanceof AttachmentSecurityError && err.code === "RAW_HOST_PATH_FORBIDDEN") {
            blocked = true;
          }
        }
        assert(blocked, `Raw host path '${rawPath}' without ChooserGrant must be forbidden`);
      }

      // Authorized ChooserGrant strips directory prefix and hides raw path unless explicitly authorized
      const safeChip = createChooserFileAttachment({
        fileNameOrPath: "C:\\Users\\Alice\\Documents\\q3-contract.pdf",
        mimeType: "application/pdf",
        byteLength: 16384,
        contentDigestHex: "ab".repeat(32),
        chooserGrantIdHex: "aa".repeat(16),
        scopedCapabilityIdHex: "bb".repeat(16),
        taintFlags: ["EXTERNAL_ORIGIN", "CONFIDENTIAL_SECRET"],
      });
      assert(safeChip.chipBorderRadiusPx === ATTACHMENT_CHIP_RADIUS_PX && safeChip.chipBorderRadiusPx === 4, "4px chip radius");
      assert(safeChip.displayLabel === "q3-contract.pdf", "Strips host directory to safe basename");
      assert(safeChip.authorizedHostPath === null, "Raw host path never leaked by default");

      const l1ChipHtml = renderAttachmentChipsHtml([safeChip], { disclosure: "L1", locale: "zh-CN" });
      const l3ChipHtml = renderAttachmentChipsHtml([safeChip], { disclosure: "L3", locale: "zh-CN" });
      assert(!l1ChipHtml.includes("Users\\Alice"), "L1 chip HTML never leaks host path");
      assert(!l3ChipHtml.includes("Users\\Alice"), "Even L3 chip HTML never leaks unauthorized host path");

      // Artifact chip binds both 32-hex ArtifactId and 64-hex ContentDigest
      const artChip = createArtifactAttachment({
        displayLabel: "compiled-summary.json",
        byteLength: 512,
        artifactIdHex: "cc".repeat(16),
        contentDigestHex: "dd".repeat(32),
        sourceApplicationName: "表格分析台",
      });
      assert(artChip.artifactIdHex === "cc".repeat(16), "ArtifactId bound");
      assert(artChip.taintFlags.includes("CROSS_APPLICATION"), "Cross-app source taints artifact chip");

      // Taint flags propagate monotonically into compiled ActionProposal
      const compiled = compileIntentAndProposal({
        utterance: "整理合同要点",
        attachments: [safeChip, artChip],
        catalog: sampleCatalog(),
      });
      assert(
        compiled.proposal.effectiveTaintFlags.includes("EXTERNAL_ORIGIN") &&
          compiled.proposal.effectiveTaintFlags.includes("CONFIDENTIAL_SECRET") &&
          compiled.proposal.effectiveTaintFlags.includes("CROSS_APPLICATION"),
        "All attachment taint flags propagate into ActionProposalPreview",
      );
      assert(compiled.proposal.requiresZ4Consent === true, "Tainted proposal requires Z4 consent");
    },
  );

  await runSuite(
    "8. Seamless WorkstationCompositor integration (bindOmnibarToCompositor: Z3 slot mount, Ctrl+Space, hardware tray & Z4 severance)",
    () => {
      const { doc, createHost } = createMockDocument();
      const rootHost = createHost();
      const compositor = createWorkstationCompositor({
        initialSessionState: "ACTIVE",
        doc,
      });
      compositor.mount(rootHost as unknown as HTMLElement, { doc });

      const binding = bindOmnibarToCompositor(compositor, undefined, { doc });
      const { controller } = binding;
      controller.setCatalog(sampleCatalog());

      // Compositor Ctrl+Space opens both compositor.commandPaletteOpen and OmnibarPaletteController!
      compositor.handleKeydown({ key: " ", ctrlKey: true });
      assert(compositor.getSnapshot().commandPaletteOpen === true, "Compositor palette layer open");
      assert(controller.getSnapshot().open === true, "Bound Omnibar controller open");

      // Grant mic & start voice capture -> routes through compositor FocusAndInputRoute and lights L0 tray!
      controller.voiceController.grantMicPermission(10);
      const startVoice = controller.startPushToTalk({ nowMonoMs: 20 });
      assert(startVoice.accepted && startVoice.phase === "Capturing", "Voice routed through compositor");
      assert(compositor.getSnapshot().inputAudit.acceptedEvents >= 1, "Compositor input router audited voice event");

      // Opening Z4 security modal on compositor immediately closes Z3 Omnibar (preserving draft!) and severs voice!
      controller.setDraftUtterance("保留中的草稿内容");
      compositor.openZ4Modal(
        {
          promptId: "z4-test-1",
          title: "系统高影响操作确认",
          summary: "请核对以下变更范围",
        },
        100,
      );
      assert(controller.getSnapshot().open === false, "Z4 modal closes Z3 Omnibar");
      assert(
        controller.getSnapshot().draftUtterance === "保留中的草稿内容",
        "Z4 security gate preserves Omnibar draft",
      );
      assert(
        controller.getSnapshot().voice.phase === "Cancelled" &&
          controller.getSnapshot().voice.lastSeveranceReason === "Z4_MODAL_ACTIVE",
        "Compositor Z4 modal immediately severs bound Omnibar voice stream",
      );

      binding.unbind();
      compositor.unmount();
    },
  );

  await runSuite(
    "9. Static & rendered ANTI-AI-01..07 compliance scan across desktop/src/views/omnibar/** and desktop/src/multimodal/**",
    async () => {
      const fs = await importBuiltin<NodeFsLike>("node:fs");
      const pathMod = await importBuiltin<NodePathLike>("node:path");
      const urlMod = await importBuiltin<NodeUrlLike>("node:url");

      const currentFile = urlMod.fileURLToPath(import.meta.url);
      const omnibarDir = pathMod.resolve(pathMod.dirname(currentFile), "..");
      const multimodalDir = pathMod.resolve(omnibarDir, "../../multimodal");

      const prodFiles = [
        ...collectProductionFiles(fs, pathMod, omnibarDir),
        ...collectProductionFiles(fs, pathMod, multimodalDir),
      ];
      assert(prodFiles.length >= 6, `Expected >= 6 production files, found ${prodFiles.length}`);

      const allIssues: string[] = [];
      for (const file of prodFiles) {
        const content = fs.readFileSync(file, "utf-8");
        allIssues.push(...auditFileForAntiAiViolations(file, content));
      }
      assert(
        allIssues.length === 0,
        `Static ANTI-AI-01..06 scan found violations:\n${allIssues.join("\n")}`,
      );
    },
  );

  await runSuite(
    "10. Local KeyPool model-assist bridge: online plan breakdown, [DUI-OFFLINE-001] deterministic fallback, non-bypass of [NL-AMBIG-001]/[NL-BIND-001], L1/L2 zero-jargon, and L3 isolation/receipt disclosure",
    async () => {
      const { doc, createHost } = createMockDocument();
      const host = createHost();

      const syntheticOnlineStatus: KeyPoolStatusDto = {
        configured: true,
        reachable: true,
        discoverySource: "explicit_config",
        gatewayOrigin: "http://127.0.0.1:18317",
        openaiBasePath: "/v1",
        defaultModel: "glm-5.3",
        fastIntentModel: "glm-5.3",
        allowedModels: ["glm-5.3", "glm-5.1"],
        activeKeys: 3,
        totalKeys: 4,
        strategy: "round-robin",
        budgetMetered: true,
        isolationTag: KEYPOOL_ISOLATION_UNVERIFIED_TAG,
        fallbackMode: "NONE",
        diagnosticSummary: "3/4 redacted slots active",
      };

      let probeCount = 0;
      let assistCallCount = 0;
      const ctrl = createOmnibarPaletteController({
        catalog: sampleCatalog(),
        initialDisclosure: "L1",
        initialLocale: "zh-CN",
        doc,
        probeKeyPoolStatus: async () => {
          probeCount += 1;
          return syntheticOnlineStatus;
        },
        requestKeyPoolAssist: async (utterance, locale) => {
          assistCallCount += 1;
          return {
            source: "KEYPOOL",
            summary:
              locale === "en-US"
                ? `Draft summary via Z3 TypedIntent and Quota check for: ${utterance}`
                : `通过 Z3 TypedIntent 与 Quota 检查拆解请求：${utterance}`,
            targetDomain: "document",
            undoClass: "LOSSLESS",
            needsClarification: false,
            clarificationReason: "",
            steps:
              locale === "en-US"
                ? [
                    "Resolve verified application target on Cell node without 0x1F shortcut",
                    "Record receipt_hex after execution stream finishes",
                  ]
                : [
                    "在本地 Cell 节点定位已核验应用（拒绝 0x1F 非权威捷径）",
                    "完成后登记 receipt_hex 并生成可回退摘要",
                  ],
            safetyEscalated: false,
            model: "glm-5.3",
            servedSlotFingerprint: "slot-a91f003c",
            isolationTag: KEYPOOL_ISOLATION_UNVERIFIED_TAG,
            budgetReceipt: {
              accountIdHex: "aa".repeat(16),
              budgetIdHex: "bb".repeat(16),
              receiptIdHex: "cc".repeat(16),
              sequence: 1,
              promptTokens: 48,
              completionTokens: 24,
              reasoningTokens: 8,
              totalTokens: 80,
              cumulativeTotalTokens: 80,
              cumulativeApiCalls: 1,
            },
            fallbackNotice: null,
          };
        },
      });

      ctrl.mount(host as unknown as HTMLElement, { doc });
      ctrl.openPalette("keyboard-shortcut");
      await Promise.resolve();
      await Promise.resolve();
      assert(probeCount >= 1, "Opening palette probes Local KeyPool status automatically");
      assert(ctrl.getSnapshot().keyPoolStatus?.reachable === true, "KeyPool status online");
      const refreshedStatus = await ctrl.refreshKeyPoolStatus();
      assert(
        refreshedStatus !== null &&
          refreshedStatus.defaultModel === "glm-5.3" &&
          refreshedStatus.fastIntentModel === "glm-5.3",
        "refreshKeyPoolStatus reflects direct glm-5.3 for both defaultModel and fastIntentModel",
      );

      ctrl.setDraftUtterance("整理文档工作台会议纪要");
      const assistButtons = host.findAll(
        (el) => el.getAttribute("data-omnibar-action") === "keypool-assist",
      );
      assert(assistButtons.length === 1, "Mounted palette renders keypool-assist button");
      assistButtons[0]!.click();
      await Promise.resolve();
      await Promise.resolve();
      assert(assistCallCount === 1, "Clicking keypool-assist button invokes requestKeyPoolAssist");
      assert(
        ctrl.getSnapshot().keyPoolAssist?.source === "KEYPOOL" &&
          ctrl.getSnapshot().keyPoolAssist?.model === "glm-5.3",
        "Online KeyPool assist stored with glm-5.3 model",
      );

      // Audit L1 and L2 across zh-CN & en-US: zero internal jargon + L3 isolation/receipt strictly hidden
      for (const locale of ["zh-CN", "en-US"] as const) {
        ctrl.setLocale(locale);
        await ctrl.runKeyPoolAssist();

        for (const tier of ["L1", "L2"] as const) {
          ctrl.setDisclosure(tier);
          const snap = ctrl.getSnapshot();
          const html = renderOmnibarPaletteHtml(snap);
          const strippedHtml = html.replace(/<[^>]*>/g, " ");
          const visibleDom = ctrl.getVisibleTextForAudit();

          const expectedOnlineBadge =
            locale === "en-US" ? "Local Model Pool · Online" : "本地模型池 · 在线";
          assert(
            visibleDom.includes(expectedOnlineBadge) && html.includes(expectedOnlineBadge),
            `Online badge rendered in ${locale}@${tier}`,
          );
          assert(
            !visibleDom.includes(KEYPOOL_ISOLATION_UNVERIFIED_TAG) &&
              !strippedHtml.includes(KEYPOOL_ISOLATION_UNVERIFIED_TAG),
            `L3 isolation tag hidden in ${locale}@${tier}`,
          );
          assert(
            !visibleDom.includes("slot-a91f003c") && !strippedHtml.includes("cc".repeat(16)),
            `L3 slot fingerprint and receipt digest hidden in ${locale}@${tier}`,
          );

          for (const banned of L1_L2_BANNED_JARGON) {
            assert(
              !visibleDom.includes(banned),
              `KeyPool assist DOM text in ${locale}@${tier} leaked banned jargon '${banned}': ${visibleDom}`,
            );
            assert(
              !strippedHtml.includes(banned),
              `KeyPool assist HTML text in ${locale}@${tier} leaked banned jargon '${banned}': ${strippedHtml}`,
            );
          }

          if (tier === "L2") {
            const expectedSlots =
              locale === "en-US"
                ? "Model: glm-5.3 · Active Slots: 3/4"
                : "默认模型: glm-5.3 · 可用通道: 3/4";
            const expectedTokens =
              locale === "en-US" ? "Model Token Usage: 80 tokens" : "本次模型用量: 80 tokens";
            assert(
              visibleDom.includes(expectedSlots) && visibleDom.includes(expectedTokens),
              `L2 displays plain-language model/slots and token accounting in ${locale}`,
            );
          }
        }
      }

      // L3 reveals PROVIDER_ISOLATION_UNVERIFIED, slot fingerprint, gateway origin, and 5D receipt ID
      ctrl.setLocale("zh-CN");
      ctrl.setDisclosure("L3");
      const l3Visible = ctrl.getVisibleTextForAudit();
      const l3Html = renderOmnibarPaletteHtml(ctrl.getSnapshot());
      assert(
        l3Visible.includes(KEYPOOL_ISOLATION_UNVERIFIED_TAG) &&
          l3Html.includes(KEYPOOL_ISOLATION_UNVERIFIED_TAG),
        "L3 discloses PROVIDER_ISOLATION_UNVERIFIED tag",
      );
      assert(
        l3Visible.includes("model=glm-5.3") &&
          l3Visible.includes("slotFingerprint=slot-a91f003c") &&
          l3Visible.includes(`LlmTokenReceipt: id=${"cc".repeat(16)}`) &&
          l3Visible.includes("origin=http://127.0.0.1:18317") &&
          l3Visible.includes("basePath=/v1"),
        "L3 discloses glm-5.3 model, redacted slot fingerprint, 5D LlmTokenReceipt, and gateway endpoint",
      );

      // [DUI-OFFLINE-001] Offline / rejected KeyPool degrades cleanly to deterministic local fallback
      const offlineHost = createHost();
      const offlineCtrl = createOmnibarPaletteController({
        catalog: sampleCatalog(),
        initialDisclosure: "L1",
        initialLocale: "zh-CN",
        doc,
        probeKeyPoolStatus: async () => {
          throw new Error("connect ECONNREFUSED 127.0.0.1:18317");
        },
        requestKeyPoolAssist: async () => {
          throw new Error("upstream socket timeout");
        },
      });
      offlineCtrl.mount(offlineHost as unknown as HTMLElement, { doc });
      offlineCtrl.openPalette("keyboard-shortcut");
      const fallbackStatus = await offlineCtrl.refreshKeyPoolStatus();
      assert(
        fallbackStatus !== null &&
          fallbackStatus.reachable === false &&
          fallbackStatus.fallbackMode === "LOCAL_DETERMINISTIC",
        "Rejected status probe transitions cleanly to LOCAL_DETERMINISTIC",
      );

      offlineCtrl.setDraftUtterance("删除旧归档文件");
      const fallbackAssist = await offlineCtrl.runKeyPoolAssist();
      assert(offlineCtrl.getSnapshot().open === true, "Z3 palette stays open on KeyPool failure");
      assert(
        fallbackAssist !== null &&
          fallbackAssist.source === "LOCAL_FALLBACK" &&
          fallbackAssist.undoClass === "IRREVERSIBLE" &&
          fallbackAssist.needsClarification === true &&
          fallbackAssist.safetyEscalated === true &&
          Boolean(fallbackAssist.fallbackNotice && fallbackAssist.fallbackNotice.length > 0),
        "Offline destructive assist degrades to LOCAL_FALLBACK with IRREVERSIBLE safety escalation",
      );
      const offlineVisibleZh = offlineCtrl.getVisibleTextForAudit();
      assert(
        offlineVisibleZh.includes("本地模型池 · 离线回退"),
        "zh-CN offline fallback badge rendered",
      );
      for (const banned of L1_L2_BANNED_JARGON) {
        assert(
          !offlineVisibleZh.includes(banned),
          `Offline fallback leaked banned jargon '${banned}': ${offlineVisibleZh}`,
        );
      }

      offlineCtrl.setLocale("en-US");
      await offlineCtrl.runKeyPoolAssist();
      assert(
        offlineCtrl.getVisibleTextForAudit().includes("Local Model Pool · Local Fallback"),
        "en-US offline fallback badge rendered",
      );

      // Non-bypass of [NL-AMBIG-001] and [NL-BIND-001]: permissive LLM output never overrides local compiler gates
      const guardCtrl = createOmnibarPaletteController({
        catalog: sampleCatalog(),
        doc,
        requestKeyPoolAssist: async (utterance): Promise<KeyPoolIntentAssistDto> => ({
          source: "KEYPOOL",
          summary: utterance,
          targetDomain: "document",
          undoClass: "LOSSLESS",
          needsClarification: false,
          clarificationReason: "",
          steps: ["Permissive model suggestion"],
          safetyEscalated: false,
          model: "glm-5.3",
          servedSlotFingerprint: "slot-00001111",
          isolationTag: KEYPOOL_ISOLATION_UNVERIFIED_TAG,
          budgetReceipt: null,
          fallbackNotice: null,
        }),
      });
      guardCtrl.openPalette("keyboard-shortcut");

      // 1) Ambiguous + irreversible request still blocked by ClarificationRequiredError ([NL-AMBIG-001])
      guardCtrl.setDraftUtterance("永久移除旧笔记");
      await guardCtrl.runKeyPoolAssist();
      assert(guardCtrl.getSnapshot().keyPoolAssist?.source === "KEYPOOL", "Assist populated");
      let ambigBlocked = false;
      try {
        guardCtrl.submitProposal();
      } catch (err) {
        if (err instanceof ClarificationRequiredError) {
          ambigBlocked = true;
        }
      }
      assert(
        ambigBlocked,
        "Model Assist must NOT bypass [NL-AMBIG-001] ClarificationRequiredError before candidate selection",
      );

      const firstCand = guardCtrl.getSnapshot().compiledBundle!.proposal.candidates[0]!;
      guardCtrl.selectCandidate(firstCand.candidateId);
      const boundSubmitted = guardCtrl.submitProposal();
      assert(
        boundSubmitted.proposal.selectedCandidateId === firstCand.candidateId,
        "Selecting candidate unlocks submission normally alongside KeyPool assist",
      );

      // 2) Mutable @latest tag still blocked by IntentBindingError("MUTABLE_LATEST_TAG_FORBIDDEN") ([NL-BIND-001])
      guardCtrl.openPalette("keyboard-shortcut");
      guardCtrl.setDraftUtterance("run editor@latest on draft");
      await guardCtrl.runKeyPoolAssist();
      let latestBlocked = false;
      try {
        guardCtrl.submitProposal();
      } catch (err) {
        if (err instanceof IntentBindingError && err.code === "MUTABLE_LATEST_TAG_FORBIDDEN") {
          latestBlocked = true;
        }
      }
      assert(
        latestBlocked,
        "Model Assist must NOT bypass [NL-BIND-001] MUTABLE_LATEST_TAG_FORBIDDEN gate",
      );
    },
  );

  console.log(
    `All ${passedSuites} Stage D Omnibar & Multimodal suites passed (${passedAssertions} assertions).`,
  );
}

void main();
