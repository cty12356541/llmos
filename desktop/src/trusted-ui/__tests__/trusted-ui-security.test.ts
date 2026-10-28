// Stage D (W71-D-TRUSTED-UI / D-TRUSTED-UI) Frontend Security, Negative-Bypass & Anti-AI Verification Suite.
// Run via: npx tsx desktop/src/trusted-ui/__tests__/trusted-ui-security.test.ts

import {
  TrustedUiRejectionError,
  Z2_SURFACE_Z_CEILING,
  Z4_ARMING_WINDOW_MS,
  Z4_DIALOG_RADIUS_PX,
  Z4_IRREVERSIBLE_CRIMSON_HEX,
  Z4_SCRIM_RGBA,
  Z4_SEAL_HOUSING_HEIGHT_PX,
  Z4_SEAL_HOUSING_WIDTH_PX,
  Z4_SECURITY_Z_INDEX,
  Z4_TOP_SECURITY_BAR_PX,
  createChooserGrantBroker,
  createClipboardAndDragGuard,
  createTrustedUiSuite,
  createZ4SecurityDialog,
  deterministicDomainDigestHex,
  renderChooserGrantModalHtml,
  renderZ4SecurityDialogHtml,
  type TrustedUiRejectionCode,
  type Z4ConsentChallengeSpec,
} from "../index";

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

class MockTextNode {
  public textContent: string;
  public constructor(text: string) {
    this.textContent = text;
  }
}

class MockElement {
  public readonly tagName: string;
  public readonly namespaceURI: string | null;
  public className = "";
  public width = 0;
  public height = 0;
  public readonly style: Record<string, string> = {};
  private _textContent = "";
  private readonly attrs = new Map<string, string>();
  private readonly childrenList: Array<MockElement | MockTextNode> = [];
  public canvasContext: unknown = null;

  public constructor(tagName: string, namespaceURI: string | null = null) {
    this.tagName = tagName;
    this.namespaceURI = namespaceURI;
  }

  public get textContent(): string {
    if (this.childrenList.length === 0) {
      return this._textContent;
    }
    return this.childrenList.map((c) => c.textContent).join("");
  }

  public set textContent(val: string) {
    this._textContent = val;
    this.childrenList.length = 0;
  }

  public setAttribute(name: string, value: string): void {
    this.attrs.set(name, value);
    if (name === "class") {
      this.className = value;
    }
  }

  public getAttribute(name: string): string | null {
    if (name === "class" && !this.attrs.has("class") && this.className.length > 0) {
      return this.className;
    }
    return this.attrs.get(name) ?? null;
  }

  public appendChild<T extends MockElement | MockTextNode>(child: T): T {
    this.childrenList.push(child);
    return child;
  }

  public getContext(contextId: string): unknown {
    return contextId === "2d" ? this.canvasContext : null;
  }

  public findAll(predicate: (el: MockElement) => boolean): MockElement[] {
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

function createMockDomEnvironment(): {
  doc: Document;
  activeRafHandles: Set<number>;
  requestFrame: (cb: (ts: number) => void) => number;
  cancelFrame: (handle: number) => void;
} {
  let nextHandle = 1;
  const activeRafHandles = new Set<number>();
  const noop2d = {
    strokeStyle: "#000000",
    fillStyle: "#000000",
    lineWidth: 1,
    clearRect: () => undefined,
    beginPath: () => undefined,
    moveTo: () => undefined,
    lineTo: () => undefined,
    arc: () => undefined,
    closePath: () => undefined,
    stroke: () => undefined,
    fill: () => undefined,
    fillRect: () => undefined,
  };
  const mockDoc = {
    createElement: (tag: string) => {
      const el = new MockElement(tag.toLowerCase(), null);
      if (tag.toLowerCase() === "canvas") {
        el.canvasContext = noop2d;
      }
      return el;
    },
    createElementNS: (ns: string, tag: string) => new MockElement(tag, ns),
    createTextNode: (text: string) => new MockTextNode(text),
  };
  return {
    doc: mockDoc as unknown as Document,
    activeRafHandles,
    requestFrame: () => {
      const id = nextHandle++;
      activeRafHandles.add(id);
      return id;
    },
    cancelFrame: (id: number) => {
      activeRafHandles.delete(id);
    },
  };
}

let passedAssertions = 0;
let passedSuites = 0;

function assert(condition: boolean, message: string): void {
  if (!condition) {
    throw new Error(`Assertion failed: ${message}`);
  }
  passedAssertions += 1;
}

function expectRejection(
  fn: () => unknown,
  expectedCode: TrustedUiRejectionCode,
  context: string,
): void {
  let caught: unknown = null;
  try {
    fn();
  } catch (err: unknown) {
    caught = err;
  }
  assert(
    caught instanceof TrustedUiRejectionError,
    `${context}: expected TrustedUiRejectionError(${expectedCode}), got ${String(caught)}`,
  );
  if (caught instanceof TrustedUiRejectionError) {
    assert(
      caught.code === expectedCode,
      `${context}: expected code ${expectedCode}, got ${caught.code}`,
    );
  }
}

async function runSuite(name: string, fn: () => void | Promise<void>): Promise<void> {
  await fn();
  passedSuites += 1;
  console.log(`  [PASS] ${name}`);
}

function stripHtmlTags(html: string): string {
  return html.replace(/<[^>]+>/g, " ").replace(/\s+/g, " ").trim();
}

function sampleChallengeSpec(
  reversibilityClass: "LOSSLESS_UNDO" | "COMPENSATABLE_ROLLBACK" | "IRREVERSIBLE",
): Z4ConsentChallengeSpec {
  return {
    requestingApp: {
      displayName: "财务结算核对台",
      publisherName: "已验证企业发行方",
      signatureVerified: true,
      applicationIdHex: "a1".repeat(16),
      installationIdHex: "b2".repeat(16),
      principalIdHex: "01".repeat(16),
      installationGeneration: 4,
      manifestDigestHex: deterministicDomainDigestHex("manifest", ["ledger-v4"]),
    },
    actionScope: {
      plainSummary: {
        "zh-CN": "向外部清算接口发送季度账单批次并清理临时归档日志",
        "en-US": "Send quarterly billing batch to external clearing gateway and purge staging logs",
      },
      capabilityRights: ["EXTERNAL_CLEARING_SEND", "TOPIC_RETENTION_PURGE"],
      controlScope: "TASK_GROUP",
      planDigestHex: deterministicDomainDigestHex("plan", ["q3-clearing"]),
    },
    resourceCeiling: {
      plainCeiling: {
        "zh-CN": "内存上限 256 MiB · 费用额度上限 12,000 单位 · 允许受控外网连接 · 最长 30 秒",
        "en-US": "Memory cap 256 MiB · Cost cap 12,000 units · Controlled network allowed · Max 30s",
      },
      memoryCeilingMib: 256,
      tokenBudgetCeiling: 12000,
      networkEgressAllowed: true,
      maxDurationMs: 30000,
    },
    reversibilityAndRisk: {
      reversibilityClass,
      plainConsequence: {
        "zh-CN":
          reversibilityClass === "IRREVERSIBLE"
            ? "外部清算指令发出且临时记录清除后无法撤回，仅保留凭证记录。"
            : "可通过系统补偿或快照撤回恢复原状。",
        "en-US":
          reversibilityClass === "IRREVERSIBLE"
            ? "Once sent to clearing and purged, the action cannot be undone; only a proof remains."
            : "Can be restored via compensating rollback or local undo.",
      },
      multiTargetCount: 3,
      blastOperationKind: "topic-purge",
      blastRadiusRows: [
        {
          symbol: "+",
          category: "created",
          scopeLabel: "外部凭证",
          targetLabel: "季度清算回执",
          reason: "写入审计账本",
          targetIdHex: "c1".repeat(16),
        },
        {
          symbol: "-",
          category: "purged",
          scopeLabel: "临时日志",
          targetLabel: "核对中间记录",
          reason: "永久清理释放空间",
          targetIdHex: "c2".repeat(16),
        },
      ],
    },
    allowedGrantScopes: ["SINGLE_ACTION", "CURRENT_TASK", "CURRENT_SESSION"],
  };
}

const ALLOWED_RADII = new Set(["0px", "2px", "4px", "6px"]);
const PICTOGRAPHIC_RE = /\p{Extended_Pictographic}/u;

// Construct banned tokens via segment joins so this test file itself stays clean.
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
  "Z4",
  "TypedIntent",
  "0x1F",
  "0x1f",
  "Quota",
  "ROAD-D-001",
  "ADR-0011",
  "ADR-0025",
  "TaskAttempt",
  "ExecutionFiber",
  "Cell",
  "receipt_hex",
  "control_command_id",
  "CAS_WINNER",
  "SHARED_THREAD_LIKE",
  "LOCAL_SINGLE_NODE",
  "ApplicationId",
  "InstallationId",
  "CapabilityRights",
  "ControlScope",
  "plan_digest",
  "LOSSLESS_UNDO",
  "COMPENSATABLE_ROLLBACK",
  "IRREVERSIBLE_NO_UNDO",
  "RCPT-UNDO-001",
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

function auditRawContentForAntiAi(filePath: string, content: string): string[] {
  const issues: string[] = [];
  const lower = content.toLowerCase();
  for (const banned of BANNED_SUBSTRINGS) {
    if (lower.includes(banned.toLowerCase())) {
      issues.push(`${filePath}: contains banned substring '${banned}'`);
    }
  }
  if (PICTOGRAPHIC_RE.test(content)) {
    issues.push(`${filePath}: contains banned Extended_Pictographic emoji`);
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
          issues.push(`${filePath}: illegal border-radius '${tok}' in '${match[0]}'`);
        }
      }
    }
    match = radiusDeclRe.exec(content);
  }
  return issues;
}

async function main(): Promise<void> {
  console.log("Running Stage D D-TRUSTED-UI Frontend Security & Anti-AI Suite...");

  await runSuite(
    "1. Z4 solid geometry (z-index 900), seal confinement, 0% idle RAF when closed, & Z2 spoof/occlusion rejection",
    () => {
      const env = createMockDomEnvironment();
      const privateSeal = "栖霞银杏-9042";
      const ctrl = createZ4SecurityDialog({
        initialSecuritySealHint: privateSeal,
        doc: env.doc,
        sealOptions: {
          requestFrame: env.requestFrame,
          cancelFrame: env.cancelFrame,
          reducedMotion: false,
        },
      });

      // Idle state: zero running RAF loops
      assert(!ctrl.isSealAnimationRunning(), "seal RAF must be stopped when Z4 is closed");
      assert(env.activeRafHandles.size === 0, "0 active RAF handles before Z4 opens");

      // Negative 1: Untrusted Z2 surface forging Z4 prompt -> rejected
      expectRejection(
        () =>
          ctrl.openChallenge(
            {
              kind: "untrusted_z2_surface",
              surfaceIdHex: "91".repeat(16),
              applicationIdHex: "a1".repeat(16),
            },
            sampleChallengeSpec("LOSSLESS_UNDO"),
            1000,
          ),
        "UNTRUSTED_Z4_FORGE_ATTEMPT",
        "Z2 forge attempt",
      );
      assert(ctrl.audit.rejectedUntrustedZ4ForgeAttempts === 1, "forge audit incremented");

      // Negative 2: Untrusted Z2 surface attempting z-index 900 or > 299 occlusion -> rejected
      expectRejection(
        () =>
          ctrl.openChallenge(
            {
              kind: "untrusted_z2_surface",
              surfaceIdHex: "91".repeat(16),
              applicationIdHex: "a1".repeat(16),
              requestedZIndex: 900,
            },
            sampleChallengeSpec("LOSSLESS_UNDO"),
            1010,
          ),
        "Z4_PLANE_OCCLUSION_REJECTED",
        "Z2 z-index 900 occlusion attempt",
      );
      expectRejection(
        () =>
          ctrl.guardSurfaceZIndex(
            {
              kind: "untrusted_z2_surface",
              surfaceIdHex: "91".repeat(16),
              applicationIdHex: "a1".repeat(16),
            },
            Z2_SURFACE_Z_CEILING + 1,
          ),
        "Z4_PLANE_OCCLUSION_REJECTED",
        "Z2 z-index 300 ceiling violation",
      );
      assert(ctrl.audit.rejectedZ4OcclusionAttempts === 2, "occlusion audit incremented");

      // Positive: TCB opens Z4 challenge
      const snap = ctrl.openChallenge(
        { kind: "tcb_trusted_broker" },
        sampleChallengeSpec("IRREVERSIBLE"),
        2000,
      );
      assert(snap.zIndex === Z4_SECURITY_Z_INDEX && snap.zIndex === 900, "Z4 z-index == 900");
      assert(snap.scrimRgba === Z4_SCRIM_RGBA, "solid scrim rgba(10, 11, 13, 0.78)");
      assert(snap.zeroBlurFilter === true, "zero blur filter invariant");
      assert(snap.dialogRadiusPx === Z4_DIALOG_RADIUS_PX && snap.dialogRadiusPx === 6, "6px dialog");
      assert(snap.topSecurityBarPx === Z4_TOP_SECURITY_BAR_PX && snap.topSecurityBarPx === 2, "2px steel-blue bar");
      assert(
        snap.sealWidthPx === Z4_SEAL_HOUSING_WIDTH_PX &&
          snap.sealHeightPx === Z4_SEAL_HOUSING_HEIGHT_PX,
        "32x20 recessed Gerono seal housing",
      );
      assert(snap.securitySealHint === privateSeal, "Z4 renders active user's private seal");
      assert(ctrl.isSealAnimationRunning(), "seal RAF runs while Z4 is mounted");
      assert(env.activeRafHandles.size === 1, "exactly 1 RAF loop while mounted");

      // Negative 3: Untrusted Z2 surface trying to read trusted Z4 projection / seal hint -> rejected
      expectRejection(
        () =>
          ctrl.inspectTrustedChallenge({
            kind: "untrusted_z2_surface",
            surfaceIdHex: "91".repeat(16),
            applicationIdHex: "a1".repeat(16),
          }),
        "UNTRUSTED_SEAL_READ_DENIED",
        "Z2 reading Z4 seal hint",
      );
      assert(ctrl.audit.rejectedUntrustedSealReads === 1, "seal read rejection counted");

      // Public posture for Z0..Z3 never leaks securitySealHint
      const postureJson = JSON.stringify(ctrl.getPublicPosture());
      assert(!postureJson.includes(privateSeal), "public posture zero seal leak");
      assert(!postureJson.includes("securitySealHint"), "public posture omits seal key");

      // Live DOM rendering check
      const domTree = ctrl.renderActiveDom({ nowMonoMs: 2100 }) as unknown as MockElement;
      assert(domTree !== null, "renders live DOM tree");
      const kvRows = domTree.findAll((el) => el.getAttribute("data-z4-kv-row") !== null);
      assert(kvRows.length === 4, "4-row structured KV spec table in DOM");

      // Cancel at 0ms stops RAF immediately -> 0% idle CPU/GPU
      ctrl.decide(snap.challengeIdHex, {
        origin: "trusted_shell",
        source: "KEYBOARD",
        confirm: false,
        key: "Escape",
        nowMonoMs: 2000,
      });
      assert(!ctrl.isSealAnimationRunning(), "seal RAF stopped on close");
      assert(env.activeRafHandles.size === 0, "0 active RAF handles after Z4 closes");
    },
  );

  await runSuite(
    "2. 4-Row KV Spec Table, IRREVERSIBLE statutory lock (zero Undo button + explicit ack), partial-failure & data-loss receipt",
    () => {
      const ctrl = createZ4SecurityDialog({ locale: "zh-CN", disclosure: "L2" });
      const snap = ctrl.openChallenge(
        { kind: "tcb_trusted_broker" },
        sampleChallengeSpec("IRREVERSIBLE"),
        10_000,
      );

      assert(!snap.undoAffordanceAllowed, "IRREVERSIBLE forbids Undo affordance");
      assert(snap.requiresHighContrastAck, "IRREVERSIBLE requires high-contrast ack");
      assert(
        !snap.candidateReceiptVerbs.includes("Undo"),
        "Undo verb pruned by IRREVERSIBLE_NO_UNDO statutory lock",
      );
      assert(
        snap.reversibilityAndRisk.requiresDataLossReceipt,
        "purged blast-radius row triggers requiresDataLossReceipt",
      );
      assert(
        snap.reversibilityAndRisk.partialFailureDisclosure !== null,
        "multi-target action auto-populates partial-failure disclosure",
      );

      const htmlBeforeAck = ctrl.renderActiveHtml(10_400);
      assert(
        htmlBeforeAck.includes(Z4_IRREVERSIBLE_CRIMSON_HEX),
        "renders crimson #DC2626 warning styling",
      );
      assert(
        htmlBeforeAck.includes('data-statutory-lock="IRREVERSIBLE_NO_UNDO"'),
        "renders IRREVERSIBLE_NO_UNDO statutory lock badge",
      );
      assert(
        htmlBeforeAck.includes('data-z4-partial-failure="true"'),
        "renders multi-target partial-failure warning",
      );
      assert(
        htmlBeforeAck.includes('data-z4-blast-preview="true"'),
        "renders + / - cascade BlastRadiusDiff preview",
      );
      assert(
        !htmlBeforeAck.includes('data-z4-action="undo"'),
        "renders ZERO fake Undo button",
      );

      // Negative: >= 350ms elapsed, but explicit high-contrast checkbox not checked -> rejected!
      expectRejection(
        () =>
          ctrl.decide(snap.challengeIdHex, {
            origin: "trusted_shell",
            source: "POINTER",
            confirm: true,
            nowMonoMs: 10_450,
            irreversibleHighContrastAck: false,
          }),
        "IRREVERSIBLE_HIGH_CONTRAST_ACK_REQUIRED",
        "IRREVERSIBLE confirm without ack",
      );

      // Check high-contrast box and approve
      ctrl.setIrreversibleAckChecked(true);
      const receipt = ctrl.decide(snap.challengeIdHex, {
        origin: "trusted_shell",
        source: "POINTER",
        confirm: true,
        grantScope: "CURRENT_TASK",
        nowMonoMs: 10_460,
      });
      assert(receipt.state === "APPROVED", "approved after explicit ack");
      assert(!receipt.undoAffordanceAllowed, "receipt marks undoAffordanceAllowed=false");
      assert(
        !receipt.allowedFollowUpVerbs.includes("Undo"),
        "receipt follow-up verbs exclude Undo",
      );

      // Negative: attempting Undo on IRREVERSIBLE receipt violates statutory lock
      expectRejection(
        () => ctrl.requestReceiptUndo(receipt.receiptIdHex),
        "IRREVERSIBLE_STATUTORY_LOCK_VIOLATION",
        "Undo on IRREVERSIBLE receipt",
      );
    },
  );

  await runSuite(
    "3. 350ms clickjacking arming guard (presentation + focus regain), 0ms Escape cancel, & session lock/switch invalidation",
    () => {
      const ctrl = createZ4SecurityDialog();
      const snap = ctrl.openChallenge(
        { kind: "tcb_trusted_broker" },
        sampleChallengeSpec("COMPENSATABLE_ROLLBACK"),
        5_000,
      );

      // Negative 1: < 350ms confirm clicks at 0ms and 349ms rejected
      for (const earlyMs of [5_000, 5_200, 5_349]) {
        expectRejection(
          () =>
            ctrl.decide(snap.challengeIdHex, {
              origin: "trusted_shell",
              source: "POINTER",
              confirm: true,
              nowMonoMs: earlyMs,
            }),
          "CLICKJACKING_ARMING_ACTIVE",
          `early confirm at ${earlyMs}ms`,
        );
      }
      assert(
        ctrl.audit.rejectedClickjackingArmingAttempts === 3,
        "3 clickjacking attempts counted",
      );

      // Negative 2: Untrusted surface or AUTOMATION confirm spoof rejected even after 350ms
      expectRejection(
        () =>
          ctrl.decide(snap.challengeIdHex, {
            origin: "untrusted_application_surface",
            source: "POINTER",
            confirm: true,
            nowMonoMs: 5_500,
          }),
        "UNTRUSTED_INPUT_ORIGIN",
        "untrusted surface confirm spoof",
      );
      expectRejection(
        () =>
          ctrl.decide(snap.challengeIdHex, {
            origin: "trusted_shell",
            source: "AUTOMATION",
            confirm: true,
            nowMonoMs: 5_500,
          }),
        "UNTRUSTED_INPUT_ORIGIN",
        "automation confirm spoof",
      );

      // Negative 3: Focus regain at 5_600ms resets the 350ms arming guard!
      const rearmedAt = ctrl.notifyFocusRegained({ kind: "tcb_trusted_broker" }, 5_600);
      assert(rearmedAt === 5_600, "armedFromMonoMs reset to 5_600");
      expectRejection(
        () =>
          ctrl.decide(snap.challengeIdHex, {
            origin: "trusted_shell",
            source: "KEYBOARD",
            confirm: true,
            nowMonoMs: 5_600 + Z4_ARMING_WINDOW_MS - 1,
          }),
        "CLICKJACKING_ARMING_ACTIVE",
        "349ms after focus regain rejected",
      );

      // >= 350ms after focus regain succeeds
      const approved = ctrl.decide(snap.challengeIdHex, {
        origin: "trusted_shell",
        source: "KEYBOARD",
        confirm: true,
        nowMonoMs: 5_600 + Z4_ARMING_WINDOW_MS,
      });
      assert(approved.state === "APPROVED", "350ms after focus regain approved");

      // Session lock / switch invalidates pending Z4 challenge immediately
      const pendingBeforeLock = ctrl.openChallenge(
        { kind: "tcb_trusted_broker" },
        sampleChallengeSpec("LOSSLESS_UNDO"),
        9_000,
      );
      ctrl.syncSession("LOCKED", 2);
      assert(!ctrl.getPublicPosture().z4ModalActive, "lock clears active Z4 modal");
      ctrl.syncSession("ACTIVE", 3, "新用户印章-1190");
      expectRejection(
        () =>
          ctrl.decide(pendingBeforeLock.challengeIdHex, {
            origin: "trusted_shell",
            source: "POINTER",
            confirm: true,
            nowMonoMs: 9_800,
          }),
        "SESSION_NOT_ACTIVE_OR_STALE_GENERATION",
        "pre-lock challenge remains invalidated",
      );
    },
  );

  await runSuite(
    "4. System ChooserGrant broker (Open / Save / Share), zero host bare path leak, & revocation fences",
    () => {
      const broker = createChooserGrantBroker();
      const appAlpha = "a1".repeat(16);
      const appBeta = "b2".repeat(16);

      // Mode ceiling violation: SHARE + READ_WRITE_SINGLE rejected
      expectRejection(
        () =>
          broker.openChooser({
            caller: { kind: "tcb_trusted_broker" },
            requestingApplicationIdHex: appAlpha,
            requestingAppDisplayName: "表格分析器",
            mode: "SHARE",
            requestedRights: "READ_WRITE_SINGLE",
            nowMonoMs: 1_000,
          }),
        "CHOOSER_RIGHTS_EXCEED_MODE_CEILING",
        "SHARE mode ceiling",
      );

      // Open READ_ONLY chooser and select only item 11..11
      const chooserSnap = broker.openChooser({
        caller: { kind: "tcb_trusted_broker" },
        requestingApplicationIdHex: appAlpha,
        requestingAppDisplayName: "表格分析器",
        mode: "OPEN",
        requestedRights: "READ_ONLY",
        nowMonoMs: 2_000,
      });
      broker.selectItems({ kind: "tcb_trusted_broker" }, ["11".repeat(16)]);

      // < 350ms clickjacking guard on chooser confirm
      expectRejection(
        () =>
          broker.confirmChooserGrant({
            origin: "trusted_shell",
            source: "POINTER",
            chooserSessionIdHex: chooserSnap.chooserSessionIdHex,
            ttlMs: 10_000,
            nowMonoMs: 2_200,
          }),
        "CLICKJACKING_ARMING_ACTIVE",
        "chooser < 350ms confirm",
      );

      // >= 350ms confirm issues attenuated ChooserGrantForApp
      const grant = broker.confirmChooserGrant({
        origin: "trusted_shell",
        source: "POINTER",
        chooserSessionIdHex: chooserSnap.chooserSessionIdHex,
        ttlMs: 10_000,
        nowMonoMs: 2_400,
      });
      assert(grant.scopedHandles.length === 1, "only 1 selected handle returned");
      assert(grant.attenuatedRights === "READ_ONLY", "attenuated READ_ONLY");
      assert(grant.grantDigestHex.length === 64, "64-hex SHA-256 grant digest");

      // Zero bare path & zero sibling leak assertion
      const jsonForApp = JSON.stringify(grant);
      for (const forbidden of [
        "C:\\",
        "C:/",
        "Users",
        "operator",
        "Documents",
        "Finance",
        "/home/",
        ".ssh",
        "id_ed25519",
        "design-review-brief.md",
        "privateHostBackingPath",
      ]) {
        assert(
          !jsonForApp.includes(forbidden),
          `ChooserGrantForApp leaked forbidden token '${forbidden}': ${jsonForApp}`,
        );
      }

      // TCB resolves backing path internally while App only exercises opaque handle
      const handleId = grant.scopedHandles[0]!.scopedHandleIdHex;
      assert(
        broker.tcbOnlyResolveHostPath(grant.grantIdHex, handleId)?.includes("q3-financial-ledger.csv") ===
          true,
        "TCB internally resolves host path",
      );
      const exercised = broker.exerciseGrant({
        grantIdHex: grant.grantIdHex,
        callerApplicationIdHex: appAlpha,
        scopedHandleIdHex: handleId,
        requiredRight: "READ_ONLY",
        nowMonoMs: 3_000,
      });
      assert(exercised.displayName === "q3-financial-ledger.csv", "exercises scoped handle");

      // Negative boundaries: write escalation, wrong app, out-of-scope handle, expiry
      expectRejection(
        () =>
          broker.exerciseGrant({
            grantIdHex: grant.grantIdHex,
            callerApplicationIdHex: appAlpha,
            scopedHandleIdHex: handleId,
            requiredRight: "READ_WRITE_SINGLE",
            nowMonoMs: 3_010,
          }),
        "CHOOSER_GRANT_INSUFFICIENT_RIGHTS",
        "READ_ONLY write escalation",
      );
      expectRejection(
        () =>
          broker.exerciseGrant({
            grantIdHex: grant.grantIdHex,
            callerApplicationIdHex: appBeta,
            scopedHandleIdHex: handleId,
            requiredRight: "READ_ONLY",
            nowMonoMs: 3_020,
          }),
        "CHOOSER_GRANT_APPLICATION_MISMATCH",
        "cross-app grant theft",
      );
      expectRejection(
        () =>
          broker.exerciseGrant({
            grantIdHex: grant.grantIdHex,
            callerApplicationIdHex: appAlpha,
            scopedHandleIdHex: "33".repeat(16),
            requiredRight: "READ_ONLY",
            nowMonoMs: 3_030,
          }),
        "CHOOSER_GRANT_HANDLE_OUT_OF_SCOPE",
        "unselected sibling handle",
      );
      expectRejection(
        () =>
          broker.exerciseGrant({
            grantIdHex: grant.grantIdHex,
            callerApplicationIdHex: appAlpha,
            scopedHandleIdHex: handleId,
            requiredRight: "READ_ONLY",
            nowMonoMs: 12_400,
          }),
        "CHOOSER_GRANT_EXPIRED",
        "expired grant",
      );

      // SAVE path traversal rejection + explicit revocation + session lock invalidation
      const saveSnap = broker.openChooser({
        caller: { kind: "tcb_trusted_broker" },
        requestingApplicationIdHex: appAlpha,
        requestingAppDisplayName: "表格分析器",
        mode: "SAVE",
        requestedRights: "EXPORT_SAVE",
        nowMonoMs: 20_000,
      });
      expectRejection(
        () =>
          broker.confirmChooserGrant({
            origin: "trusted_shell",
            source: "KEYBOARD",
            chooserSessionIdHex: saveSnap.chooserSessionIdHex,
            exportSaveLeafName: "..\\..\\evil.dll",
            ttlMs: 30_000,
            nowMonoMs: 20_400,
          }),
        "CHOOSER_INVALID_EXPORT_NAME",
        "SAVE traversal leaf name",
      );
      const saveGrant = broker.confirmChooserGrant({
        origin: "trusted_shell",
        source: "KEYBOARD",
        chooserSessionIdHex: saveSnap.chooserSessionIdHex,
        exportSaveLeafName: "clean-report.pdf",
        exportContentType: "application/pdf",
        ttlMs: 30_000,
        nowMonoMs: 20_410,
      });
      broker.revokeGrant(saveGrant.grantIdHex);
      expectRejection(
        () =>
          broker.exerciseGrant({
            grantIdHex: saveGrant.grantIdHex,
            callerApplicationIdHex: appAlpha,
            scopedHandleIdHex: saveGrant.scopedHandles[0]!.scopedHandleIdHex,
            requiredRight: "EXPORT_SAVE",
            nowMonoMs: 20_500,
          }),
        "CHOOSER_GRANT_REVOKED",
        "explicitly revoked grant",
      );
    },
  );

  await runSuite(
    "5. Taint-aware Clipboard & Drag-Drop Guard ([DUI-CLIP-001]): missing gesture, expiry, & Z4 escalation",
    () => {
      const suite = createTrustedUiSuite({ locale: "zh-CN", disclosure: "L2" });
      const guard = createClipboardAndDragGuard(suite.z4Dialog);
      const srcApp = "d1".repeat(16);
      const dstApp = "e2".repeat(16);
      const dstInst = "f3".repeat(16);
      const principal = "01".repeat(16);

      // Negative 1: Automation cannot mint gesture tokens
      expectRejection(
        () =>
          guard.mintUserGestureToken({
            origin: "trusted_shell",
            source: "AUTOMATION",
            actorApplicationIdHex: srcApp,
            channel: "CLIPBOARD",
            nowMonoMs: 1_000,
          }),
        "AUTOMATION_GESTURE_FORBIDDEN",
        "automation gesture mint",
      );

      // Publish PUBLIC clipboard item
      const copyG = guard.mintUserGestureToken({
        origin: "trusted_shell",
        source: "KEYBOARD",
        actorApplicationIdHex: srcApp,
        channel: "CLIPBOARD",
        nowMonoMs: 1_100,
      });
      const pubMeta = guard.publishTransfer({
        channel: "CLIPBOARD",
        contentType: "text/plain",
        payloadText: "Public release notes",
        sourceApplicationIdHex: srcApp,
        sourceAppDisplayName: "帮助中心",
        taintFlags: ["PUBLIC"],
        gestureTokenIdHex: copyG.gestureTokenIdHex,
        ttlMs: 3_000,
        nowMonoMs: 1_120,
      });

      // Negative 2: Background scripted read without gesture -> MISSING_USER_GESTURE
      expectRejection(
        () =>
          guard.requestTransferRead({
            transferIdHex: pubMeta.transferIdHex,
            targetApplicationIdHex: dstApp,
            targetInstallationIdHex: dstInst,
            targetPrincipalIdHex: principal,
            targetAppDisplayName: "编辑器",
            gestureTokenIdHex: null,
            nowMonoMs: 1_200,
          }),
        "MISSING_USER_GESTURE",
        "background clipboard read without gesture",
      );

      // Negative 3: Reusing consumed copy gesture -> INVALID_OR_EXPIRED_USER_GESTURE
      expectRejection(
        () =>
          guard.requestTransferRead({
            transferIdHex: pubMeta.transferIdHex,
            targetApplicationIdHex: dstApp,
            targetInstallationIdHex: dstInst,
            targetPrincipalIdHex: principal,
            targetAppDisplayName: "编辑器",
            gestureTokenIdHex: copyG.gestureTokenIdHex,
            nowMonoMs: 1_210,
          }),
        "INVALID_OR_EXPIRED_USER_GESTURE",
        "replayed gesture token",
      );

      // Valid PUBLIC cross-app paste -> immediately RELEASED
      const pasteG = guard.mintUserGestureToken({
        origin: "trusted_shell",
        source: "KEYBOARD",
        actorApplicationIdHex: dstApp,
        channel: "CLIPBOARD",
        nowMonoMs: 1_250,
      });
      const pubDecision = guard.requestTransferRead({
        transferIdHex: pubMeta.transferIdHex,
        targetApplicationIdHex: dstApp,
        targetInstallationIdHex: dstInst,
        targetPrincipalIdHex: principal,
        targetAppDisplayName: "编辑器",
        gestureTokenIdHex: pasteG.gestureTokenIdHex,
        nowMonoMs: 1_260,
      });
      assert(
        pubDecision.outcome === "RELEASED" &&
          pubDecision.payloadText === "Public release notes",
        "clean PUBLIC paste released",
      );

      // Negative 4: Expired clipboard item -> TRANSFER_EXPIRED
      const expiredG = guard.mintUserGestureToken({
        origin: "trusted_shell",
        source: "KEYBOARD",
        actorApplicationIdHex: dstApp,
        channel: "CLIPBOARD",
        nowMonoMs: 5_000,
      });
      expectRejection(
        () =>
          guard.requestTransferRead({
            transferIdHex: pubMeta.transferIdHex,
            targetApplicationIdHex: dstApp,
            targetInstallationIdHex: dstInst,
            targetPrincipalIdHex: principal,
            targetAppDisplayName: "编辑器",
            gestureTokenIdHex: expiredG.gestureTokenIdHex,
            nowMonoMs: 5_010,
          }),
        "TRANSFER_EXPIRED",
        "expired clipboard read",
      );

      // Tainted CREDENTIALS drag-drop escalates to Z4 and blocks bytes until approved
      const dragG = guard.mintUserGestureToken({
        origin: "trusted_shell",
        source: "DRAG_DROP",
        actorApplicationIdHex: srcApp,
        channel: "DRAG_DROP",
        nowMonoMs: 10_000,
      });
      const secretMeta = guard.publishTransfer({
        channel: "DRAG_DROP",
        contentType: "text/plain",
        payloadText: "SECRET-TOKEN-99182",
        sourceApplicationIdHex: srcApp,
        sourceAppDisplayName: "密钥管理箱",
        taintFlags: ["CREDENTIALS", "CONFIDENTIAL"],
        gestureTokenIdHex: dragG.gestureTokenIdHex,
        ttlMs: 20_000,
        nowMonoMs: 10_020,
      });
      const dropG = guard.mintUserGestureToken({
        origin: "trusted_shell",
        source: "DRAG_DROP",
        actorApplicationIdHex: dstApp,
        channel: "DRAG_DROP",
        nowMonoMs: 10_100,
      });
      const escalated = guard.requestTransferRead({
        transferIdHex: secretMeta.transferIdHex,
        targetApplicationIdHex: dstApp,
        targetInstallationIdHex: dstInst,
        targetPrincipalIdHex: principal,
        targetAppDisplayName: "外部协作窗口",
        gestureTokenIdHex: dropG.gestureTokenIdHex,
        nowMonoMs: 10_120,
      });
      assert(
        escalated.outcome === "ESCALATED_TO_Z4_CONSENT",
        "CREDENTIALS cross-app drop escalates to Z4",
      );
      if (escalated.outcome !== "ESCALATED_TO_Z4_CONSENT") {
        throw new Error("unreachable");
      }
      assert(suite.isZ4PlaneActive(), "suite reports Z4 plane active during escalation");

      // Premature extraction before Z4 approval -> blocked (0 bytes)
      expectRejection(
        () =>
          guard.completeEscalatedTransferAfterZ4({
            transferIdHex: secretMeta.transferIdHex,
            targetApplicationIdHex: dstApp,
            challengeIdHex: escalated.challengeIdHex,
            nowMonoMs: 10_200,
          }),
        "TRANSFER_CONSENT_MISMATCH_OR_DENIED",
        "premature extraction before Z4 approval",
      );

      // Approve Z4 after >= 350ms with explicit irreversible ack, then release once
      suite.z4Dialog.setIrreversibleAckChecked(true);
      suite.z4Dialog.decide(escalated.challengeIdHex, {
        origin: "trusted_shell",
        source: "POINTER",
        confirm: true,
        nowMonoMs: 10_500,
      });
      const releasedAfterZ4 = guard.completeEscalatedTransferAfterZ4({
        transferIdHex: secretMeta.transferIdHex,
        targetApplicationIdHex: dstApp,
        challengeIdHex: escalated.challengeIdHex,
        nowMonoMs: 10_510,
      });
      assert(
        releasedAfterZ4.outcome === "RELEASED" &&
          releasedAfterZ4.payloadText === "SECRET-TOKEN-99182",
        "releases bytes after Z4 approval",
      );
    },
  );

  await runSuite(
    "6. Bilingual (zh-CN / en-US) L1/L2 zero-jargon confinement & L3 raw protocol disclosure (ANTI-AI-07)",
    () => {
      const ctrl = createZ4SecurityDialog();
      const snap = ctrl.openChallenge(
        { kind: "tcb_trusted_broker" },
        sampleChallengeSpec("IRREVERSIBLE"),
        1_000,
      );

      const chooserBroker = createChooserGrantBroker();
      const chooserSnap = chooserBroker.openChooser({
        caller: { kind: "tcb_trusted_broker" },
        requestingApplicationIdHex: "a1".repeat(16),
        requestingAppDisplayName: "数据分析器",
        mode: "OPEN",
        requestedRights: "READ_ONLY",
        nowMonoMs: 1_000,
      });

      for (const locale of ["zh-CN", "en-US"] as const) {
        for (const disclosure of ["L1", "L2"] as const) {
          const z4VisibleText = stripHtmlTags(
            renderZ4SecurityDialogHtml(snap, { locale, disclosure, nowMonoMs: 1_400 }),
          );
          const chooserVisibleText = stripHtmlTags(
            renderChooserGrantModalHtml(chooserSnap, {
              locale,
              disclosure,
              nowMonoMs: 1_400,
            }),
          );
          for (const banned of L1_L2_BANNED_JARGON) {
            assert(
              !z4VisibleText.includes(banned),
              `Z4 ${locale}/${disclosure} leaked internal jargon '${banned}' in visible text: ${z4VisibleText}`,
            );
            assert(
              !chooserVisibleText.includes(banned),
              `Chooser ${locale}/${disclosure} leaked internal jargon '${banned}' in visible text: ${chooserVisibleText}`,
            );
          }
        }

        // L3 MUST disclose raw technical contract fields
        const z4L3Html = renderZ4SecurityDialogHtml(snap, {
          locale,
          disclosure: "L3",
          nowMonoMs: 1_400,
        });
        assert(z4L3Html.includes("ApplicationId="), `L3 (${locale}) reveals ApplicationId`);
        assert(
          z4L3Html.includes("CapabilityRights="),
          `L3 (${locale}) reveals CapabilityRights`,
        );
        assert(z4L3Html.includes("plan_digest="), `L3 (${locale}) reveals plan_digest`);
      }
    },
  );

  await runSuite(
    "7. Static source & rendered HTML ANTI-AI-01..07 compliance across desktop/src/trusted-ui/**",
    async () => {
      const fs = await importBuiltin<NodeFsLike>("node:fs");
      const pathMod = await importBuiltin<NodePathLike>("node:path");
      const urlMod = await importBuiltin<NodeUrlLike>("node:url");

      const currentFile = urlMod.fileURLToPath(import.meta.url);
      const trustedUiDir = pathMod.resolve(pathMod.dirname(currentFile), "..");
      const prodFiles = collectProductionFiles(fs, pathMod, trustedUiDir);
      assert(prodFiles.length >= 4, `expected >= 4 production files, got ${prodFiles.length}`);

      const allIssues: string[] = [];
      for (const file of prodFiles) {
        const content = fs.readFileSync(file, "utf-8");
        allIssues.push(...auditRawContentForAntiAi(file, content));
      }
      assert(
        allIssues.length === 0,
        `Static ANTI-AI audit failed:\n${allIssues.join("\n")}`,
      );
    },
  );

  console.log(
    `\nAll ${passedSuites} D-TRUSTED-UI suites passed (${passedAssertions} assertions).`,
  );
}

main().catch((err: unknown) => {
  console.error(err);
  throw err;
});
