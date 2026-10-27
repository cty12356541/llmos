// Deterministic unit, contrast, and Anti-AI-Smell audit suite for D-TOKENS-KIT (W69-D-TOKENS).
// Run from repo root: npx tsx desktop/src/ui/__tests__/tokens-and-primitives.test.ts

import {
  ALL_STATUTORY_LOCK_KINDS,
  ALL_WORKSTATION_STATUS_STATES,
  ARMING_WINDOW_MS,
  BULLET_BAR_MAX_WIDTH_PX,
  BULLET_BAR_MIN_WIDTH_PX,
  CANONICAL_TASK_STATES,
  GERONO_AMPLITUDE_X,
  GERONO_AMPLITUDE_Y,
  GERONO_CENTER_X,
  GERONO_CENTER_Y,
  GERONO_PERIOD_MS,
  ICON_NAMES,
  INSPECTOR_SCOPE_ORDER,
  SEAL_HEIGHT_PX,
  SEAL_WIDTH_PX,
  VERIFICATION_SEAL_FALLBACK_GLYPH,
  computeAttemptRaceModel,
  computeBlastRadiusSummary,
  computeBulletMicroBarModel,
  computeFiberWaterfallModel,
  computeSealPhaseMarkers,
  createAttemptRaceElement,
  createBlastRadiusDiffElement,
  createBulletMicroBarElement,
  createFiberWaterfallElement,
  createIconElement,
  createInspectorDrawerElement,
  createReceiptAuditBarElement,
  createStatusIndicatorElement,
  createStatutoryLockBadgeElement,
  createTwoStepArmingController,
  createVerificationSeal,
  filterBreadcrumbsForDisclosure,
  getStatusDescriptor,
  getStatutoryLockDescriptor,
  pruneVerbsAgainstStatutoryLock,
  renderAttemptRaceHtml,
  renderBlastRadiusDiffHtml,
  renderBulletMicroBarHtml,
  renderFiberWaterfallHtml,
  renderIconSvg,
  renderInspectorDrawerHtml,
  renderReceiptAuditBarHtml,
  renderStatusIndicatorHtml,
  renderStatutoryLockBadgeHtml,
  renderVerificationSealFallbackHtml,
  sampleGeronoCurve,
  sampleGeronoPoint,
  shouldRenderReceiptAuditBar,
  verifyReceiptHexParity,
  type AllowedAttemptVerb,
  type DisclosureLevel,
  type ScopeBreadcrumbItem,
  type WorkstationLocale,
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
  join(...segments: string[]): string;
  relative(from: string, to: string): string;
}

interface NodeUrlLike {
  fileURLToPath(url: string): string;
}

const importBuiltin = new Function(
  "specifier",
  "return import(specifier);",
) as <T>(specifier: string) => Promise<T>;

/* ==========================================================================
   Minimal Headless DOM & Canvas Mock for Testing DOM Builders Without Browser
   ========================================================================== */

class MockElement {
  public readonly tagName: string;
  public readonly namespaceURI: string | null;
  public className = "";
  public title = "";
  public type = "";
  public hidden = false;
  public tabIndex = 0;
  public width = 0;
  public height = 0;
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
  }

  get children(): readonly MockElement[] {
    return this.childrenList.filter((c): c is MockElement => c instanceof MockElement);
  }

  setAttribute(name: string, value: string): void {
    this.attrs.set(name, value);
    if (name === "class") {
      this.className = value;
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
    this.dispatch("click", { type: "click" });
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

class MockTextNode {
  public textContent: string;
  constructor(text: string) {
    this.textContent = text;
  }
}

interface RecordedCanvasOp {
  readonly op: string;
  readonly args: readonly number[];
}

function createMockCanvas2d(): {
  ctx: CanvasRenderingContext2D;
  ops: RecordedCanvasOp[];
} {
  const ops: RecordedCanvasOp[] = [];
  const mock = {
    strokeStyle: "#000000",
    fillStyle: "#000000",
    lineWidth: 1,
    clearRect: (x: number, y: number, w: number, h: number) => {
      ops.push({ op: "clearRect", args: [x, y, w, h] });
    },
    beginPath: () => {
      ops.push({ op: "beginPath", args: [] });
    },
    moveTo: (x: number, y: number) => {
      ops.push({ op: "moveTo", args: [x, y] });
    },
    lineTo: (x: number, y: number) => {
      ops.push({ op: "lineTo", args: [x, y] });
    },
    stroke: () => {
      ops.push({ op: "stroke", args: [] });
    },
    fillRect: (x: number, y: number, w: number, h: number) => {
      ops.push({ op: "fillRect", args: [x, y, w, h] });
    },
  };
  return {
    ctx: mock as unknown as CanvasRenderingContext2D,
    ops,
  };
}

function createMockDocument(options?: { withCanvas2d?: boolean }): {
  doc: Document;
  canvasOps: RecordedCanvasOp[];
} {
  const { ctx, ops } = createMockCanvas2d();
  const withCanvas = options?.withCanvas2d ?? true;
  const mockDoc = {
    createElement: (tag: string) => {
      const el = new MockElement(tag.toLowerCase(), null);
      if (tag.toLowerCase() === "canvas" && withCanvas) {
        el.canvasContext = ctx;
      }
      return el;
    },
    createElementNS: (ns: string, tag: string) => new MockElement(tag, ns),
    createTextNode: (text: string) => new MockTextNode(text),
  };
  return {
    doc: mockDoc as unknown as Document,
    canvasOps: ops,
  };
}

/* ==========================================================================
   Test Harness & Assertion Helpers
   ========================================================================== */

let passedAssertions = 0;
let passedCases = 0;

function assert(condition: boolean, message: string): void {
  if (!condition) {
    throw new Error(`Assertion failed: ${message}`);
  }
  passedAssertions += 1;
}

function assertApprox(actual: number, expected: number, epsilon: number, message: string): void {
  const diff = Math.abs(actual - expected);
  assert(
    diff <= epsilon,
    `${message} (expected ${expected} +/- ${epsilon}, got ${actual})`,
  );
}

async function runCase(name: string, fn: () => void | Promise<void>): Promise<void> {
  await fn();
  passedCases += 1;
  console.log(`  [PASS] ${name}`);
}

/* ==========================================================================
   WCAG 2.2 Relative Luminance & Static Anti-AI Audit Helpers
   ========================================================================== */

function parseHexRgb(hex: string): readonly [number, number, number] {
  const clean = hex.trim().replace(/^#/, "");
  if (clean.length !== 6) {
    throw new Error(`Expected 6-digit hex color, got ${hex}`);
  }
  const r = Number.parseInt(clean.slice(0, 2), 16);
  const g = Number.parseInt(clean.slice(2, 4), 16);
  const b = Number.parseInt(clean.slice(4, 6), 16);
  return [r, g, b];
}

function relativeLuminance(hex: string): number {
  const [r, g, b] = parseHexRgb(hex);
  const lin = (channel: number): number => {
    const s = channel / 255;
    return s <= 0.04045 ? s / 12.92 : Math.pow((s + 0.055) / 1.055, 2.4);
  };
  return 0.2126 * lin(r) + 0.7152 * lin(g) + 0.0722 * lin(b);
}

function contrastRatio(fgHex: string, bgHex: string): number {
  const l1 = relativeLuminance(fgHex);
  const l2 = relativeLuminance(bgHex);
  const lighter = Math.max(l1, l2);
  const darker = Math.min(l1, l2);
  return (lighter + 0.05) / (darker + 0.05);
}

function parseThemeTokens(cssContent: string, selectorMarker: string): Map<string, string> {
  const idx = cssContent.indexOf(selectorMarker);
  if (idx < 0) {
    throw new Error(`Missing selector marker in tokens.css: ${selectorMarker}`);
  }
  const openBrace = cssContent.indexOf("{", idx);
  let depth = 0;
  let endBrace = -1;
  for (let i = openBrace; i < cssContent.length; i += 1) {
    if (cssContent[i] === "{") {
      depth += 1;
    } else if (cssContent[i] === "}") {
      depth -= 1;
      if (depth === 0) {
        endBrace = i;
        break;
      }
    }
  }
  const block = cssContent.slice(openBrace + 1, endBrace);
  const map = new Map<string, string>();
  const propRe = /(--nlos-[a-z0-9-]+)\s*:\s*([^;]+);/gi;
  let match: RegExpExecArray | null = propRe.exec(block);
  while (match !== null) {
    map.set(match[1]!, match[2]!.trim());
    match = propRe.exec(block);
  }
  return map;
}

const ALLOWED_RADII = new Set(["0px", "2px", "4px", "6px", "50%"]);
const PICTOGRAPHIC_RE = /\p{Extended_Pictographic}/u;

// Construct banned tokens via segment joins so scanning this file itself hits 0 literal matches.
const BANNED_SUBSTRINGS: readonly string[] = [
  ["backdrop", "-filter"].join(""),
  ["-webkit-", "backdrop", "-filter"].join(""),
  ["#030", "710"].join(""),
  ["#7fc", "8ff"].join(""),
  ["radial", "-gradient"].join(""),
  ["max-w-", "7xl"].join(""),
];

const L0_L1_BANNED_JARGON: readonly string[] = [
  "Z1",
  "TypedIntent",
  "0x1F",
  "0x1f",
  "Quota",
  "ROAD-D-001",
  "ADR-0011",
  "TaskAttempt",
  "ExecutionFiber",
  "receipt_hex",
  "control_command_id",
  "CAS_WINNER",
  "SHARED_THREAD_LIKE",
];

function auditSourceTextForAntiAiViolations(filePath: string, content: string): string[] {
  const issues: string[] = [];
  const lower = content.toLowerCase();

  for (const banned of BANNED_SUBSTRINGS) {
    if (lower.includes(banned.toLowerCase())) {
      issues.push(`${filePath}: contains banned anti-AI token '${banned}'`);
    }
  }

  if (PICTOGRAPHIC_RE.test(content)) {
    issues.push(`${filePath}: contains banned Extended_Pictographic character`);
  }

  // Audit every border-radius CSS declaration and --nlos-radius-* custom property
  const radiusDeclRe =
    /(?:border(?:-(?:top|bottom)-(?:left|right))?-radius|--nlos-radius-[a-z0-9-]+)\s*:\s*([^;}"']+)/gi;
  let m: RegExpExecArray | null = radiusDeclRe.exec(content);
  while (m !== null) {
    const rawVal = m[1]!.trim();
    if (!rawVal.startsWith("var(--nlos-radius-")) {
      const tokens = rawVal.split(/\s+/).filter((t) => t !== "!important");
      for (const tok of tokens) {
        if (!ALLOWED_RADII.has(tok)) {
          issues.push(`${filePath}: illegal border-radius token '${tok}' in '${m[0]}'`);
        }
      }
    }
    m = radiusDeclRe.exec(content);
  }

  return issues;
}

function collectFilesRecursively(fs: NodeFsLike, pathMod: NodePathLike, dir: string): string[] {
  const out: string[] = [];
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    const full = pathMod.join(dir, entry.name);
    if (entry.isDirectory()) {
      out.push(...collectFilesRecursively(fs, pathMod, full));
    } else if (entry.isFile()) {
      out.push(full);
    }
  }
  return out.sort();
}

function stripHtmlTags(html: string): string {
  return html.replace(/<[^>]*>/g, " ").replace(/\s+/g, " ").trim();
}

/* ==========================================================================
   Main Test Execution
   ========================================================================== */

async function main(): Promise<void> {
  const fs = await importBuiltin<NodeFsLike>("node:fs");
  const pathMod = await importBuiltin<NodePathLike>("node:path");
  const urlMod = await importBuiltin<NodeUrlLike>("node:url");

  const thisFile = urlMod.fileURLToPath(import.meta.url);
  const srcRoot = pathMod.resolve(pathMod.dirname(thisFile), "../..");
  const stylesDir = pathMod.join(srcRoot, "styles");
  const uiDir = pathMod.join(srcRoot, "ui");

  console.log("Running D-TOKENS-KIT (W69-D-TOKENS) Verification Suite...");

  await runCase("1. ANTI-AI-01..06 static CSS & source audit across desktop/src/{styles,ui}", () => {
    const allFiles = [
      ...collectFilesRecursively(fs, pathMod, stylesDir),
      ...collectFilesRecursively(fs, pathMod, uiDir),
    ];
    assert(allFiles.length >= 12, `Expected >= 12 files in styles/ + ui/, got ${allFiles.length}`);

    for (const file of allFiles) {
      const text = fs.readFileSync(file, "utf-8");
      const rel = pathMod.relative(srcRoot, file);
      const issues = auditSourceTextForAntiAiViolations(rel, text);
      assert(issues.length === 0, `Violations in ${rel}: ${issues.join("; ")}`);
    }

    // Verify 50% radius is ONLY used once on the 6x6px geometric status dot helper
    const surfacesCss = fs.readFileSync(pathMod.join(stylesDir, "surfaces-and-borders.css"), "utf-8");
    const fiftyMatches = surfacesCss.match(/50%/g) ?? [];
    assert(fiftyMatches.length === 1, `Expected 50% radius exactly once on 6px status dot, got ${fiftyMatches.length}`);
    assert(
      surfacesCss.includes(".nlos-status-dot-round") &&
        surfacesCss.includes("width: 6px;") &&
        surfacesCss.includes("height: 6px;"),
      "50% radius must be scoped to 6x6px .nlos-status-dot-round",
    );

    // Verify reduced-motion zero-duration overrides, tabular-nums, >=12px mono floor, >=26px hit target, 2px focus ring
    const tokensCss = fs.readFileSync(pathMod.join(stylesDir, "tokens.css"), "utf-8");
    assert(tokensCss.includes("@media (prefers-reduced-motion: reduce)"), "tokens.css missing reduced-motion rule");
    assert(tokensCss.includes("--nlos-dur-fast: 0ms;"), "tokens.css missing 0ms fast override");
    assert(tokensCss.includes("--nlos-dur-base: 0ms;"), "tokens.css missing 0ms base override");
    assert(tokensCss.includes("--nlos-fs-mono-xs: 0.75rem;"), "tokens.css must enforce 0.75rem (12px) mono floor");
    assert(tokensCss.includes("--nlos-hit-min: 26px;"), "tokens.css must enforce 26px minimum hit target");
    assert(surfacesCss.includes("font-variant-numeric: tabular-nums;"), "surfaces-and-borders.css missing tabular-nums");
    assert(surfacesCss.includes("outline: 2px solid var(--nlos-accent-active);"), "missing 2px focus-visible ring");

    // Negative self-check: ensure auditor catches every class of banned pattern
    const syntheticViolation = [
      `${["backdrop", "-filter"].join("")}: blur(12px);`,
      `color: ${["#7FC", "8FF"].join("")};`,
      `background: ${["#030", "710"].join("")};`,
      `background-image: ${["radial", "-gradient"].join("")}(circle, #fff, #000);`,
      `${["border", "-radius"].join("")}: 12px;`,
      `${["border", "-radius"].join("")}: 999px;`,
      String.fromCodePoint(0x1f680),
    ].join("\n");
    const detected = auditSourceTextForAntiAiViolations("synthetic.css", syntheticViolation);
    assert(detected.length >= 6, `Auditor self-check must flag all synthetic violations, got ${detected.length}`);
  });

  await runCase("2. WCAG 2.2 AA relative luminance contrast audit on Graphite & Paper themes", () => {
    const tokensCss = fs.readFileSync(pathMod.join(stylesDir, "tokens.css"), "utf-8");
    const graphite = parseThemeTokens(tokensCss, '[data-nlos-theme="graphite-workstation"]');
    const paper = parseThemeTokens(tokensCss, '[data-nlos-theme="paper-workstation"]');

    const getHex = (theme: Map<string, string>, name: string): string => {
      const v = theme.get(name);
      assert(v !== undefined, `Missing token ${name}`);
      return v!;
    };

    for (const [themeName, theme] of [
      ["graphite-workstation", graphite],
      ["paper-workstation", paper],
    ] as const) {
      const bgWindow = getHex(theme, "--nlos-bg-window");
      const bgChrome = getHex(theme, "--nlos-bg-chrome");
      const bgElevated = getHex(theme, "--nlos-bg-elevated");
      const bgHover = getHex(theme, "--nlos-bg-hover");
      const bgActiveRow = getHex(theme, "--nlos-bg-active-row");
      const bgRecessed = getHex(theme, "--nlos-bg-recessed");

      const textPrimary = getHex(theme, "--nlos-text-primary");
      const textSecondary = getHex(theme, "--nlos-text-secondary");
      const accentPrimary = getHex(theme, "--nlos-accent-primary");
      const accentHover = getHex(theme, "--nlos-accent-primary-hover");
      const accentActive = getHex(theme, "--nlos-accent-active");
      const accentFg = getHex(theme, "--nlos-accent-fg");
      const signalAction = getHex(theme, "--nlos-signal-action");
      const signalActionText = getHex(theme, "--nlos-signal-action-text");
      const signalDanger = getHex(theme, "--nlos-signal-danger");
      const signalDangerText = getHex(theme, "--nlos-signal-danger-text");
      const signalDangerFg = getHex(theme, "--nlos-signal-danger-fg");
      const indActive = getHex(theme, "--nlos-indicator-active");
      const indNeutral = getHex(theme, "--nlos-indicator-neutral");

      // Normal text pairs >= 4.5:1 (WCAG 2.2 AA 1.4.3)
      for (const [label, fg, bg] of [
        ["text-primary vs bg-window", textPrimary, bgWindow],
        ["text-primary vs bg-chrome", textPrimary, bgChrome],
        ["text-primary vs bg-elevated", textPrimary, bgElevated],
        ["text-primary vs bg-hover", textPrimary, bgHover],
        ["text-primary vs bg-active-row", textPrimary, bgActiveRow],
        ["text-primary vs bg-recessed", textPrimary, bgRecessed],
        ["text-secondary vs bg-window", textSecondary, bgWindow],
        ["text-secondary vs bg-chrome", textSecondary, bgChrome],
        ["text-secondary vs bg-elevated", textSecondary, bgElevated],
        ["text-secondary vs bg-hover", textSecondary, bgHover],
        ["accent-fg vs accent-primary", accentFg, accentPrimary],
        ["accent-fg vs accent-hover", accentFg, accentHover],
        ["signal-danger-fg vs signal-danger", signalDangerFg, signalDanger],
        ["signal-action-text vs bg-window", signalActionText, bgWindow],
        ["signal-action-text vs bg-elevated", signalActionText, bgElevated],
        ["signal-danger-text vs bg-window", signalDangerText, bgWindow],
        ["signal-danger-text vs bg-elevated", signalDangerText, bgElevated],
      ] as const) {
        const cr = contrastRatio(fg, bg);
        assert(
          cr >= 4.5,
          `[${themeName}] ${label} (${fg} on ${bg}) contrast ${cr.toFixed(2)}:1 < 4.5:1`,
        );
      }

      // Non-text UI & status glyph pairs >= 3.0:1 (WCAG 2.2 AA 1.4.11)
      for (const [label, fg, bg] of [
        ["accent-active focus ring vs bg-window", accentActive, bgWindow],
        ["accent-active focus ring vs bg-chrome", accentActive, bgChrome],
        ["accent-active focus ring vs bg-elevated", accentActive, bgElevated],
        ["indicator-active vs bg-window", indActive, bgWindow],
        ["indicator-neutral vs bg-window", indNeutral, bgWindow],
        ["signal-action glyph vs bg-window", signalAction, bgWindow],
        ["signal-danger glyph vs bg-window", signalDanger, bgWindow],
        ["signal-danger glyph vs bg-elevated", signalDanger, bgElevated],
      ] as const) {
        const cr = contrastRatio(fg, bg);
        assert(
          cr >= 3.0,
          `[${themeName}] ${label} (${fg} on ${bg}) contrast ${cr.toFixed(2)}:1 < 3.0:1`,
        );
      }
    }

    // Paper text-muted vs pure white window canvas is also >= 4.5:1
    const paperMutedCr = contrastRatio(
      getHex(paper, "--nlos-text-muted"),
      getHex(paper, "--nlos-bg-window"),
    );
    assert(paperMutedCr >= 4.5, `Paper text-muted contrast ${paperMutedCr.toFixed(2)}:1 < 4.5:1`);
  });

  await runCase("3. Monochrome geometric SVG icons (14x14 / 16x16, 1.25px-1.5px stroke, zero emoji)", () => {
    assert(ICON_NAMES.length === 32, `Expected 32 workstation icons, got ${ICON_NAMES.length}`);
    const { doc } = createMockDocument();

    for (const name of ICON_NAMES) {
      const svg14 = renderIconSvg(name, { size: 14, strokeWidth: 1.25 });
      assert(svg14.includes('width="14"') && svg14.includes('height="14"'), `${name} 14x14 attrs`);
      assert(svg14.includes('stroke="currentColor"'), `${name} stroke currentColor`);
      assert(svg14.includes('fill="none"'), `${name} fill none`);
      assert(svg14.includes('stroke-width="1.25"'), `${name} 1.25 stroke width`);
      assert(svg14.includes('aria-hidden="true"'), `${name} decorative aria-hidden`);
      assert(!PICTOGRAPHIC_RE.test(svg14), `${name} must not contain pictographic chars`);

      const titled = renderIconSvg(name, { size: 16, strokeWidth: 1.5, title: "Action Icon" });
      assert(titled.includes('role="img"') && titled.includes("<title>Action Icon</title>"), `${name} titled role`);

      const domIcon = createIconElement(name, { size: 16, strokeWidth: 1.5, doc });
      assert(domIcon.getAttribute("data-icon") === name, `${name} DOM data-icon`);
      assert(domIcon.getAttribute("stroke") === "currentColor", `${name} DOM stroke`);
    }
  });

  await runCase("4. Recessed 32x20 Gerono verification seal math, reduced-motion, zero idle RAF & U+221E fallback", () => {
    assert(SEAL_WIDTH_PX === 32 && SEAL_HEIGHT_PX === 20, "Seal housing must be 32x20px");
    assert(GERONO_CENTER_X === 16 && GERONO_CENTER_Y === 10, "Gerono center must be (16, 10)");
    assert(GERONO_AMPLITUDE_X === 11.5 && GERONO_AMPLITUDE_Y === 5.5, "Gerono amplitudes 11.5 x 5.5");

    const p0 = sampleGeronoPoint(0);
    assertApprox(p0.x, 27.5, 1e-9, "x(0) = 27.5");
    assertApprox(p0.y, 10.0, 1e-9, "y(0) = 10.0");

    const pQuarter = sampleGeronoPoint(Math.PI / 4);
    assertApprox(pQuarter.x, 16 + 11.5 * Math.SQRT1_2, 1e-9, "x(pi/4)");
    assertApprox(pQuarter.y, 15.5, 1e-9, "y(pi/4) = 15.5");

    const pHalf = sampleGeronoPoint(Math.PI / 2);
    assertApprox(pHalf.x, 16.0, 1e-9, "x(pi/2) = 16");
    assertApprox(pHalf.y, 10.0, 1e-9, "y(pi/2) = 10");

    const pPi = sampleGeronoPoint(Math.PI);
    assertApprox(pPi.x, 4.5, 1e-9, "x(pi) = 4.5");
    assertApprox(pPi.y, 10.0, 1e-9, "y(pi) = 10.0");

    const curve = sampleGeronoCurve(64);
    assert(curve.length === 65, "64-segment closed Gerono curve has 65 vertices");
    for (const pt of curve) {
      assert(pt.x >= 4.5 - 1e-9 && pt.x <= 27.5 + 1e-9, `Gerono x within [4.5, 27.5]: ${pt.x}`);
      assert(pt.y >= 4.5 - 1e-9 && pt.y <= 15.5 + 1e-9, `Gerono y within [4.5, 15.5]: ${pt.y}`);
    }

    const [m1, m2] = computeSealPhaseMarkers(GERONO_PERIOD_MS / 8);
    assertApprox((m1.x + m2.x) / 2, GERONO_CENTER_X, 1e-9, "Phase markers symmetric in X");
    assertApprox((m1.y + m2.y) / 2, GERONO_CENTER_Y, 1e-9, "Phase markers symmetric in Y");

    // Verify zero RAF while unmounted, start on mount(), cancel on unmount()
    const { doc, canvasOps } = createMockDocument({ withCanvas2d: true });
    let rafScheduled = 0;
    let cafCalled = 0;
    let mqlReduced = false;
    const mqlHolder: { listener: ((ev: { matches: boolean }) => void) | null } = {
      listener: null,
    };

    const seal = createVerificationSeal({
      doc,
      matchReducedMotion: () => ({
        get matches() {
          return mqlReduced;
        },
        addEventListener: (_t, cb) => {
          mqlHolder.listener = cb;
        },
        removeEventListener: () => {
          mqlHolder.listener = null;
        },
      }),
      requestFrame: () => {
        rafScheduled += 1;
        return rafScheduled;
      },
      cancelFrame: () => {
        cafCalled += 1;
      },
    });

    assert(seal.getMode() === "animated-canvas", "Default mode with 2D context is animated-canvas");
    assert(!seal.isRunning() && rafScheduled === 0, "Zero RAF scheduled before mount()");
    assert(canvasOps.length > 0, "Initial static frame drawn on creation");

    seal.mount();
    assert(seal.isRunning() && rafScheduled === 1, "Single RAF scheduled upon mount()");

    // Dynamic prefers-reduced-motion switch stops RAF immediately
    mqlReduced = true;
    mqlHolder.listener?.({ matches: true });
    assert(seal.getMode() === "static-reduced-motion", "Switched to static-reduced-motion");
    assert(!seal.isRunning() && cafCalled === 1, "RAF cancelled immediately on reduced-motion");

    mqlReduced = false;
    mqlHolder.listener?.({ matches: false });
    assert(seal.getMode() === "animated-canvas" && seal.isRunning(), "Resumed on reduced-motion off");

    seal.unmount();
    assert(!seal.isRunning() && cafCalled === 2, "RAF cancelled immediately upon unmount()");

    // Verify no-Canvas pure math U+221E fallback
    const { doc: noCanvasDoc } = createMockDocument({ withCanvas2d: false });
    const fallbackSeal = createVerificationSeal({ doc: noCanvasDoc });
    assert(fallbackSeal.getMode() === "text-fallback", "No-canvas environment enters text-fallback");
    assert(
      fallbackSeal.element.textContent === VERIFICATION_SEAL_FALLBACK_GLYPH &&
        VERIFICATION_SEAL_FALLBACK_GLYPH.codePointAt(0) === 0x221e &&
        !PICTOGRAPHIC_RE.test(VERIFICATION_SEAL_FALLBACK_GLYPH),
      "Fallback uses pure math U+221E infinity symbol",
    );
    const fallbackHtml = renderVerificationSealFallbackHtml();
    assert(fallbackHtml.includes(VERIFICATION_SEAL_FALLBACK_GLYPH), "Fallback HTML contains U+221E");
  });

  await runCase("5. 16-state status mapping, strict bipolar signals, statutory locks & ANTI-AI-07 zero-jargon gate", () => {
    assert(CANONICAL_TASK_STATES.length === 16, "Must cover all 16 canonical Task states");
    assert(ALL_WORKSTATION_STATUS_STATES.length === 24, "Covers 16 canonical + draft/quarantine/node states");
    assert(ALL_STATUTORY_LOCK_KINDS.length === 6, "Covers all 6 statutory non-tamperable locks");

    const actionStates = new Set<string>();
    const dangerStates = new Set<string>();

    for (const state of ALL_WORKSTATION_STATUS_STATES) {
      const desc = getStatusDescriptor(state);
      if (desc.signalToken === "--nlos-signal-action") {
        actionStates.add(state);
      }
      if (desc.signalToken === "--nlos-signal-danger") {
        dangerStates.add(state);
      }
    }

    // Strict bipolar signal discipline (ANTI-AI-06)
    assert(
      actionStates.size === 3 &&
        actionStates.has("WAITING_CONSENT") &&
        actionStates.has("WAITING_INPUT") &&
        actionStates.has("DISPUTED"),
      "Amber --nlos-signal-action strictly reserved for user-intervention states",
    );
    assert(
      dangerStates.size === 2 &&
        dangerStates.has("QUARANTINED") &&
        dangerStates.has("FAILED"),
      "Crimson --nlos-signal-danger strictly reserved for FAILED and QUARANTINED",
    );

    // Headlamp pre-render verb pruning against statutory locks
    assert(
      pruneVerbsAgainstStatutoryLock(["Delete", "Pause", "JumpToAuthoritativeObject"], "SEARCH_0X1F_READ_ONLY")
        .join(",") === "JumpToAuthoritativeObject",
      "0x1F search allows only JumpToAuthoritativeObject",
    );
    assert(
      pruneVerbsAgainstStatutoryLock(["Undo", "ViewEffectProof"], "IRREVERSIBLE_NO_UNDO")
        .join(",") === "ViewEffectProof",
      "IRREVERSIBLE omits Undo",
    );
    assert(
      pruneVerbsAgainstStatutoryLock(["DeleteNode", "RewireEdge"], "NODE_COMPLETED_OR_ACTIVE_LOCKED")
        .length === 0,
      "COMPLETED/ACTIVE node forbids all topology edits",
    );
    assert(
      !pruneVerbsAgainstStatutoryLock(["Cancel", "ManualCommit", "Supersede"], "TASK_ATTEMPT_NO_MANUAL_COMMIT")
        .includes("ManualCommit"),
      "TaskAttempt forbids ManualCommit",
    );
    assert(
      pruneVerbsAgainstStatutoryLock(["MarkSucceeded", "Reconcile", "Compensate"], "OPERATION_EFFECT_UNKNOWN_NO_MANUAL_SUCCESS")
        .join(",") === "Reconcile,Compensate",
      "EFFECT_UNKNOWN allows only Reconcile/Compensate",
    );
    assert(
      !pruneVerbsAgainstStatutoryLock(["UnlockByLocalTtl", "InspectFenceStatus"], "QUARANTINED_LEASE_FENCE")
        .includes("UnlockByLocalTtl"),
      "QUARANTINED forbids local-TTL unlock",
    );

    // ANTI-AI-07: Exhaustive L0 & L1 zero-jargon verification across ALL components & locales
    const locales: readonly WorkstationLocale[] = ["zh-CN", "en-US"];
    const nonExpertLevels: readonly DisclosureLevel[] = ["L0", "L1"];

    for (const locale of locales) {
      for (const disclosure of nonExpertLevels) {
        const renderedChunks: string[] = [];

        for (const st of ALL_WORKSTATION_STATUS_STATES) {
          renderedChunks.push(renderStatusIndicatorHtml({ state: st, locale, disclosure }));
        }
        for (const lk of ALL_STATUTORY_LOCK_KINDS) {
          renderedChunks.push(renderStatutoryLockBadgeHtml({ kind: lk, locale, disclosure }));
        }
        renderedChunks.push(
          renderBulletMicroBarHtml({
            actualUsage: 6,
            effectiveGrantedOrLeaseCap: 12,
            requestedCeiling: 15,
            unit: locale === "zh-CN" ? "积分" : "credits",
            locale,
            disclosure,
          }),
        );
        renderedChunks.push(
          renderAttemptRaceHtml({
            attempts: [{ attemptNumber: 1, startMs: 0, endMs: 100, outcome: "CAS_WINNER" }],
            locale,
            disclosure,
          }),
        );
        renderedChunks.push(
          renderFiberWaterfallHtml({
            activeCpuMs: 40,
            schedulerWaitMs: 10,
            externalWaitMs: 20,
            backpressureWaitMs: 5,
            suspendedMs: 5,
            sharedProcessFaultDomain: true,
            locale,
            disclosure,
          }),
        );
        renderedChunks.push(
          renderBlastRadiusDiffHtml({
            rows: [
              {
                symbol: "-",
                category: "purged",
                scopeLabel: locale === "zh-CN" ? "消息通道" : "Channel",
                targetLabel: "events.main",
                reason: locale === "zh-CN" ? "清空待处理消息" : "Clear pending backlog",
                targetIdHex: "aabbccddeeff00112233445566778899",
              },
            ],
            operationKind: "topic-purge",
            locale,
            disclosure,
          }),
        );
        renderedChunks.push(
          renderReceiptAuditBarHtml({
            disclosure,
            controlCommandIdHex: "00112233445566778899aabbccddeeff",
            receiptHex: "deadbeefcafebabe",
            locale,
          }),
        );

        const combinedVisibleText = stripHtmlTags(renderedChunks.join(" "));
        for (const bannedTerm of L0_L1_BANNED_JARGON) {
          assert(
            !combinedVisibleText.includes(bannedTerm),
            `[ANTI-AI-07] Banned jargon '${bannedTerm}' leaked into ${disclosure} (${locale}): ${combinedVisibleText}`,
          );
        }
      }
    }

    // Conversely, L3 MUST expose raw enum and invariant diagnostic tags
    const l3StatusHtml = renderStatusIndicatorHtml({ state: "WAITING_CONSENT", disclosure: "L3" });
    assert(l3StatusHtml.includes("WAITING_CONSENT"), "L3 status indicator must render raw enum");
    const l3LockHtml = renderStatutoryLockBadgeHtml({ kind: "SEARCH_0X1F_READ_ONLY", disclosure: "L3" });
    assert(l3LockHtml.includes("0x1F"), "L3 lock badge must expose 0x1F diagnostic detail");

    const { doc } = createMockDocument();
    const domStatus = createStatusIndicatorElement({ state: "FAILED", disclosure: "L1", doc });
    assert(domStatus.getAttribute("data-tone") === "danger", "DOM status indicator sets danger tone");
    assert(!domStatus.textContent.includes("FAILED"), "L1 DOM status indicator hides raw enum");

    const lockDesc = getStatutoryLockDescriptor("IRREVERSIBLE_NO_UNDO");
    assert(lockDesc.forbiddenVerbs.includes("Undo"), "IRREVERSIBLE descriptor forbids Undo");
    const domLock = createStatutoryLockBadgeElement({
      kind: "IRREVERSIBLE_NO_UNDO",
      disclosure: "L1",
      doc,
    });
    assert(
      domLock.getAttribute("data-statutory-lock") === "IRREVERSIBLE_NO_UNDO",
      "DOM statutory lock badge sets data attribute",
    );
  });

  await runCase("6. SOTA-4 Nomad triple-watermark bullet micro-bar math & edge cases", () => {
    const throttled = computeBulletMicroBarModel({
      actualUsage: 6,
      effectiveGrantedOrLeaseCap: 12,
      requestedCeiling: 15,
      unit: "积分",
      widthPx: 60, // below min, clamps to 72
    });
    assert(throttled.widthPx === BULLET_BAR_MIN_WIDTH_PX, "Clamps width floor to 72px");
    assert(throttled.state === "throttled-or-shrunk", "6/12/15 is throttled-or-shrunk");
    assertApprox(throttled.actualPct, 40, 0.01, "6/15 = 40%");
    assertApprox(throttled.effectivePct, 80, 0.01, "12/15 = 80%");
    assert(throttled.readoutText === "6/12/15 积分", "Readout text formatted");

    const nominal = computeBulletMicroBarModel({
      actualUsage: 5,
      effectiveGrantedOrLeaseCap: 10,
      requestedCeiling: 10,
      widthPx: 120, // above max, clamps to 96
    });
    assert(nominal.widthPx === BULLET_BAR_MAX_WIDTH_PX, "Clamps width ceiling to 96px");
    assert(nominal.state === "nominal", "5/10/10 is nominal");

    const zero = computeBulletMicroBarModel({
      actualUsage: -4,
      effectiveGrantedOrLeaseCap: 0,
      requestedCeiling: Number.NaN,
    });
    assert(zero.state === "zero" && zero.actualPct === 0 && zero.effectivePct === 0, "Zero/NaN edge case");

    const shrunkOffline = computeBulletMicroBarModel({
      actualUsage: 9,
      effectiveGrantedOrLeaseCap: 6,
      requestedCeiling: 12,
    });
    assert(shrunkOffline.state === "over-effective-cap", "Usage > shrunk offline lease cap");
    assertApprox(shrunkOffline.actualPct, 75, 0.01, "9/12 = 75%");
    assertApprox(shrunkOffline.effectivePct, 50, 0.01, "6/12 = 50%");

    const { doc } = createMockDocument();
    const el = createBulletMicroBarElement({
      actualUsage: 6,
      effectiveGrantedOrLeaseCap: 12,
      requestedCeiling: 15,
      doc,
    });
    assert(el.getAttribute("role") === "meter", "Bullet micro-bar uses ARIA meter role");
  });

  await runCase("7. SOTA-3 Temporal TaskAttempt race bar & ExecutionFiber 5-segment waterfall", () => {
    const race = computeAttemptRaceModel([
      {
        attemptNumber: 1,
        startMs: 100,
        endMs: 250,
        outcome: "SUPERSEDED",
        retryFenceEpoch: 1,
      },
      {
        attemptNumber: 2,
        startMs: 200,
        endMs: 500,
        outcome: "CAS_WINNER",
        retryFenceEpoch: 2,
        providerIsolationUnverified: true,
      },
    ]);
    assert(race.windowStartMs === 100 && race.windowEndMs === 500 && race.windowDurationMs === 400, "Race window 400ms");
    assert(race.statutoryManualCommitForbidden === true, "Statutory manual commit forbidden");
    assertApprox(race.spans[0]!.offsetPct, 0, 0.01, "Attempt #1 offset 0%");
    assertApprox(race.spans[0]!.widthPct, 37.5, 0.01, "Attempt #1 width 37.5%");
    assertApprox(race.spans[1]!.offsetPct, 25, 0.01, "Attempt #2 offset 25%");
    assertApprox(race.spans[1]!.widthPct, 75, 0.01, "Attempt #2 width 75%");
    assert(race.spans[1]!.providerIsolationUnverified === true, "Unverified isolation flag preserved");

    const { doc } = createMockDocument();
    assert(createAttemptRaceElement({ attempts: [], disclosure: "L1", doc }) === null, "Attempt race hidden in L1");

    const actionsFired: Array<[number, AllowedAttemptVerb]> = [];
    const raceEl = createAttemptRaceElement({
      attempts: [{ attemptNumber: 1, startMs: 0, endMs: 50, outcome: "RUNNING" }],
      disclosure: "L3",
      onAction: (n, v) => actionsFired.push([n, v]),
      doc,
    }) as unknown as MockElement;
    assert(raceEl !== null, "Attempt race rendered in L3");
    const buttons = raceEl.findAll((n) => n.tagName === "button");
    assert(
      buttons.length > 0 && buttons.every((b) => b.textContent !== "ManualCommit"),
      "Zero manual commit button in AttemptRace DOM",
    );
    buttons[0]!.click();
    assert(actionsFired.length === 1, "Legal attempt action callback wired");

    const fiber = computeFiberWaterfallModel({
      activeCpuMs: 50,
      schedulerWaitMs: 20,
      externalWaitMs: 15,
      backpressureWaitMs: 10,
      suspendedMs: 5,
      sharedProcessFaultDomain: true,
    });
    assert(fiber.totalMs === 100, "Total fiber duration 100ms");
    const pctSum = fiber.segments.reduce((acc, s) => acc + s.widthPct, 0);
    assertApprox(pctSum, 100, 0.01, "5 fiber waterfall segments sum to 100%");
    assert(
      fiber.sharedFaultBadgeText !== null && fiber.sharedFaultBadgeText.includes("SHARED_THREAD_LIKE"),
      "Shared process fault domain badge populated",
    );
    assert(createFiberWaterfallElement({
      activeCpuMs: 10,
      schedulerWaitMs: 0,
      externalWaitMs: 0,
      backpressureWaitMs: 0,
      suspendedMs: 0,
      disclosure: "L2",
      doc,
    }) === null, "Fiber waterfall hidden below L3");
  });

  await runCase("8. SOTA-1 Headlamp non-modal InspectorDrawer & ScopeBreadcrumbBar", () => {
    const fullChain: readonly ScopeBreadcrumbItem[] = INSPECTOR_SCOPE_ORDER.map((kind, idx) => ({
      kind,
      id: `id-${idx}`,
      label: `${kind}-${idx}`,
    }));

    const l2Crumbs = filterBreadcrumbsForDisclosure(fullChain, "L2");
    assert(
      l2Crumbs.length === 6 &&
        l2Crumbs.every((c) => c.kind !== "TaskAttempt" && c.kind !== "Fiber"),
      "L2 breadcrumbs omit TaskAttempt and Fiber",
    );
    const l3Crumbs = filterBreadcrumbsForDisclosure(fullChain, "L3");
    assert(l3Crumbs.length === 8, "L3 breadcrumbs expose full 8-scope chain");

    const rightHtml = renderInspectorDrawerHtml({
      placement: "right",
      disclosure: "L2",
      breadcrumbs: fullChain,
    });
    assert(
      rightHtml.includes("nlos-inspector-drawer--right") &&
        !rightHtml.includes("TaskAttempt"),
      "HTML right drawer renders L2 without TaskAttempt",
    );
    assert(
      renderInspectorDrawerHtml({ open: false, breadcrumbs: fullChain }) === "",
      "Closed InspectorDrawer HTML is empty",
    );

    const { doc } = createMockDocument();
    const toggledLevels: string[] = [];
    const selectedScopes: string[] = [];
    let closedCount = 0;

    const drawer = createInspectorDrawerElement({
      placement: "bottom",
      disclosure: "L2",
      breadcrumbs: fullChain,
      onSelectScope: (item) => selectedScopes.push(item.kind),
      onToggleDisclosure: (next) => toggledLevels.push(next),
      onClose: () => {
        closedCount += 1;
      },
      doc,
    });

    assert(drawer.isOpen() && drawer.getPlacement() === "bottom", "Drawer opens in bottom 36% mode");
    assert(drawer.getVisibleBreadcrumbs().length === 6, "Initial L2 visible breadcrumbs = 6");

    drawer.toggleDisclosure();
    assert(drawer.getDisclosure() === "L3" && toggledLevels[0] === "L3", "Toggled L2 -> L3");
    assert(drawer.getVisibleBreadcrumbs().length === 8, "L3 visible breadcrumbs = 8");

    const mockRoot = drawer.element as unknown as MockElement;
    const crumbBtns = mockRoot.findAll((el) => el.getAttribute("data-scope-kind") !== null);
    crumbBtns[2]!.click();
    assert(selectedScopes[0] === "Application", "Clicking ancestor breadcrumb selects Application scope");

    const handled = drawer.handleKeyDown({ key: "Escape" });
    assert(handled && !drawer.isOpen() && closedCount === 1, "Escape key closes non-modal InspectorDrawer");
  });

  await runCase("9. SOTA-6 Terraform + / ~ / - / = blast-radius diff, Topic Purge Data-Loss Receipt & 5s arming", () => {
    const summary = computeBlastRadiusSummary({
      operationKind: "topic-purge",
      rows: [
        { symbol: "+", category: "reverified", scopeLabel: "Task", targetLabel: "Audit", reason: "Reverify" },
        { symbol: "~", category: "paused", scopeLabel: "Node", targetLabel: "Step-2", reason: "Suspend" },
        { symbol: "~", category: "throttled", scopeLabel: "Resource", targetLabel: "Pool-A", reason: "Cap 50%" },
        { symbol: "-", category: "purged", scopeLabel: "Channel", targetLabel: "orders.q", reason: "Purge 48 msgs" },
        { symbol: "=", category: "exempted", scopeLabel: "Task", targetLabel: "DetachedBackup", reason: "Exempted" },
      ],
      locale: "en-US",
      disclosure: "L3",
    });

    assert(summary.addedCount === 1, "+ count = 1");
    assert(summary.modifiedCount === 2, "~ count = 2");
    assert(summary.removedCount === 1, "- count = 1");
    assert(summary.exemptedCount === 1, "= count = 1");
    assert(summary.affectedTotal === 4, "affectedTotal excludes exempted (=) rows");
    assert(
      summary.requiresDataLossReceipt &&
        summary.dataLossBannerText !== null &&
        summary.dataLossBannerText.includes("Data-Loss Receipt"),
      "Topic Purge enforces Data-Loss Receipt warning banner",
    );

    const { doc } = createMockDocument();
    const diffEl = createBlastRadiusDiffElement({
      operationKind: "topic-purge",
      rows: [{ symbol: "-", category: "purged", scopeLabel: "Channel", targetLabel: "q", reason: "Purge" }],
      doc,
    }) as unknown as MockElement;
    const alertAlerts = diffEl.findAll((n) => n.getAttribute("data-dataloss-receipt") === "required");
    assert(alertAlerts.length === 1, "DOM renders role=alert Data-Loss Receipt banner");

    // 5-second two-step arming + 350ms clickjacking guard
    assert(ARMING_WINDOW_MS === 5000, "Default two-step arming window is 5000ms");
    let fakeNow = 1000;
    let confirmed = 0;
    const arming = createTwoStepArmingController({
      windowMs: 5000,
      minGuardMs: 350,
      nowMs: () => fakeNow,
      onConfirm: () => {
        confirmed += 1;
      },
    });

    assert(arming.getSnapshot().state === "idle", "Starts idle");
    assert(arming.press() === "armed", "First press arms");
    assert(arming.getSnapshot().remainingSecondsLabel === "5.0s", "5.0s countdown readout");

    fakeNow = 1200; // only 200ms elapsed (< 350ms guard)
    assert(arming.press() === "guard-blocked" && confirmed === 0, "350ms guard blocks accidental double-click");

    fakeNow = 6100; // > 5000ms window expired
    assert(arming.getSnapshot().state === "idle", "Expired window auto-reverts to idle");
    assert(arming.press() === "armed" && confirmed === 0, "Press after expiry re-arms safely instead of firing");

    fakeNow = 6600; // 500ms after re-arm (within [350ms, 5000ms])
    assert(arming.press() === "confirmed" && confirmed === 1, "Second press inside valid window confirms");

    arming.press();
    arming.cancel();
    assert(arming.getSnapshot().state === "idle", "Cancel/Escape immediately disarms");
  });

  await runCase("10. L3-only receipt-audit-bar gating & CLI RECEIPT byte-parity verifier", () => {
    for (const level of ["L0", "L1", "L2"] as const) {
      assert(!shouldRenderReceiptAuditBar(level), `Audit bar disabled in ${level}`);
      assert(
        renderReceiptAuditBarHtml({
          disclosure: level,
          controlCommandIdHex: "aa".repeat(16),
          receiptHex: "bb".repeat(32),
        }) === "",
        `Audit bar HTML strictly empty in ${level}`,
      );
      const { doc } = createMockDocument();
      assert(
        createReceiptAuditBarElement({
          disclosure: level,
          controlCommandIdHex: "aa".repeat(16),
          receiptHex: "bb".repeat(32),
          doc,
        }) === null,
        `Audit bar DOM strictly null in ${level}`,
      );
    }

    const matchRes = verifyReceiptHexParity(
      "C0C00102AABBCCDD",
      "RECEIPT c0c00102aabbccdd\noutcome=inspected worker_state=BackingOff",
    );
    assert(matchRes.validHex && matchRes.matched, "Matches CLI stdout RECEIPT <hex> byte-for-byte");

    const mismatchRes = verifyReceiptHexParity("c0c00102aabbccdd", "RECEIPT c0c00102aabbccde");
    assert(mismatchRes.validHex && !mismatchRes.matched, "Detects single-nibble mismatch");

    const invalidRes = verifyReceiptHexParity("c0c0", "RECEIPT not_hex_zz");
    assert(!invalidRes.validHex && !invalidRes.matched, "Rejects malformed CLI hex fail-closed");

    const oddLengthRes = verifyReceiptHexParity("abc", "abc");
    assert(!oddLengthRes.validHex && !oddLengthRes.matched, "Rejects odd-length hex fail-closed");

    const { doc } = createMockDocument();
    const copied: string[] = [];
    const ctrl = createReceiptAuditBarElement({
      disclosure: "L3",
      controlCommandIdHex: "11".repeat(16),
      receiptHex: "22".repeat(32),
      cellId: "cell-local-0",
      leaseEpoch: 4,
      nodeBootGeneration: 2,
      onCopyCommandId: (id) => copied.push(id),
      onCopyReceiptHex: (hex) => copied.push(hex),
      doc,
    });
    assert(ctrl !== null, "L3 creates ReceiptAuditBar controller");
    const rootMock = ctrl!.element as unknown as MockElement;
    const copyCmd = rootMock.findAll((n) => n.getAttribute("data-action") === "copy-command-id")[0]!;
    const copyRcpt = rootMock.findAll((n) => n.getAttribute("data-action") === "copy-receipt-hex")[0]!;
    copyCmd.click();
    copyRcpt.click();
    assert(copied.length === 2 && copied[0] === "11".repeat(16) && copied[1] === "22".repeat(32), "Copy callbacks fire");

    ctrl!.verifyAgainstCliOutput(`RECEIPT ${"22".repeat(32)}`);
    assert(ctrl!.getParityStatus() === "PARITY_MATCH", "Controller transitions to PARITY_MATCH");
    ctrl!.verifyAgainstCliOutput(`RECEIPT ${"33".repeat(32)}`);
    assert(ctrl!.getParityStatus() === "PARITY_MISMATCH", "Controller transitions to PARITY_MISMATCH");
  });

  console.log(
    `\nALL ${passedCases} SUITES PASSED (${passedAssertions} deterministic assertions verified).`,
  );
}

main().catch((err: unknown) => {
  console.error("D-TOKENS-KIT test suite failed:", err);
  const proc = (globalThis as { process?: { exitCode?: number } }).process;
  if (proc) {
    proc.exitCode = 1;
  }
});
