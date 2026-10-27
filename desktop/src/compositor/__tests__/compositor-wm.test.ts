// Comprehensive Deterministic Verification & Anti-AI Audit Suite for D-COMPOSITOR-WM (W70).
// Run from repo root: npx tsx desktop/src/compositor/__tests__/compositor-wm.test.ts

import {
  ALL_INPUT_SOURCES,
  LANDMARK_RING_ORDER,
  WORKSTATION_PLANE_CONTRACT,
  Z4_ARMING_WINDOW_MS,
  createWorkstationCompositor,
} from "../index";
import {
  computeLauncherAppVerbs,
  createApplicationLauncherModel,
  renderLauncherMenuHtml,
  type InstalledApplicationEntry,
} from "../../dock";
import {
  ALL_ACTION_PRUNER_DOMAINS,
  BLIND_RETRY_FORBIDDEN_ON_UNKNOWN,
  BlindRetryForbiddenError,
  SAFE_TO_ISSUE_FRESH_COMMAND,
  computeAllowedVerbs,
  computeVisibleCursorLimit,
  createCommandReconciler,
  createDesktopProjectionStore,
  createDisclosureModel,
  isQueryAllowedAtDisclosure,
  summarizeBatchTargetOutcomes,
} from "../../store";
import {
  MAX_SYSTEM_CONTROL_ENUM_LIMIT,
  SEARCH_NON_AUTHORITATIVE_RESTRICTIONS_ALL,
  type AnyControlActionInput,
  type OutcomeDto,
  type TypedReceiptDto,
} from "../../types";
import {
  SEAL_HEIGHT_PX,
  SEAL_WIDTH_PX,
  type DisclosureLevel,
  type WorkstationLocale,
} from "../../ui";
import {
  CSP_SANDBOX_POLICY,
  CSP_SURFACE_DIRECTIVE,
  MIN_FLOATING_HEIGHT_PX,
  MIN_FLOATING_WIDTH_PX,
  TASKBAR_HEIGHT_PX,
  TILE_GAP_PX,
  WINDOW_TITLEBAR_HEIGHT_PX,
  WM_LAYOUT_MODES,
  Z0_Z_INDEX,
  Z1_Z_INDEX,
  Z2_Z_INDEX_MAX,
  Z2_Z_INDEX_MIN,
  Z3_FLYOUT_Z_INDEX,
  Z3_PALETTE_Z_INDEX,
  Z3_TASKBAR_Z_INDEX,
  Z4_SECURITY_Z_INDEX,
  clampFloatingRectToStage,
  computeInnerContentClipRect,
  createTilingWindowManager,
  solveWorkspaceGeometry,
} from "../../wm";

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
   Minimal Headless DOM & Canvas Mock for Live Compositor Mounting
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
  }

  removeAttribute(name: string): void {
    this.attrs.delete(name);
    if (name === "hidden") {
      this.hidden = false;
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
    this.dispatch("click", { type: "click", stopPropagation: () => undefined });
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

class MockTextNode {
  public textContent: string;
  constructor(text: string) {
    this.textContent = text;
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

const ALLOWED_RADII = new Set(["0px", "2px", "4px", "6px", "50%"]);
const PICTOGRAPHIC_RE = /\p{Extended_Pictographic}/u;

// Construct banned tokens via segment joins so scanning never hits literal strings.
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
  baseName: string,
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
        if (tok === "50%" && baseName !== "surfaces-and-borders.css") {
          issues.push(
            `${filePath}: 50% radius is allowed exclusively in surfaces-and-borders.css`,
          );
        }
      }
    }
    match = radiusDeclRe.exec(content);
  }

  return issues;
}

function hex32(byteHex: string): string {
  return byteHex.repeat(16).slice(0, 32).toLowerCase();
}

/* ==========================================================================
   Main Verification Execution (Suites 1..8)
   ========================================================================== */

async function main(): Promise<void> {
  const fs = await importBuiltin<NodeFsLike>("node:fs");
  const pathMod = await importBuiltin<NodePathLike>("node:path");
  const urlMod = await importBuiltin<NodeUrlLike>("node:url");

  const thisFile = urlMod.fileURLToPath(import.meta.url);
  const srcRoot = pathMod.resolve(pathMod.dirname(thisFile), "../..");

  console.log("Running D-COMPOSITOR-WM (W70) Comprehensive Verification Suite...");

  /* ------------------------------------------------------------------------
     Suite 1: 5-Plane Z-Index Contract & StageRect Clamping (1024x768..3840x2160)
     ------------------------------------------------------------------------ */
  await runSuite(
    "Suite 1: 5-Plane Z-Index Contract & StageRect Clamping (1024x768..3840x2160)",
    () => {
      assert(Z0_Z_INDEX === 0, "Z0 backdrop z-index must be 0");
      assert(Z1_Z_INDEX === 100, "Z1 task space z-index must be 100");
      assert(Z2_Z_INDEX_MIN === 200 && Z2_Z_INDEX_MAX === 299, "Z2 window range must be 200..299");
      assert(Z3_TASKBAR_Z_INDEX === 400, "Z3 taskbar z-index must be 400");
      assert(Z3_FLYOUT_Z_INDEX === 430, "Z3 flyout z-index must be 430");
      assert(Z3_PALETTE_Z_INDEX === 460, "Z3 palette z-index must be 460");
      assert(Z4_SECURITY_Z_INDEX === 900, "Z4 security z-index must be 900");
      assert(TASKBAR_HEIGHT_PX === 36, "Bottom taskbar height must be 36px");
      assert(WINDOW_TITLEBAR_HEIGHT_PX === 30, "Window titlebar height must be 30px");
      assert(TILE_GAP_PX === 4, "Tile gap must be 4px");
      assert(
        WORKSTATION_PLANE_CONTRACT.z4Security > WORKSTATION_PLANE_CONTRACT.z3Palette &&
          WORKSTATION_PLANE_CONTRACT.z3Palette > WORKSTATION_PLANE_CONTRACT.z3Flyout &&
          WORKSTATION_PLANE_CONTRACT.z3Flyout > WORKSTATION_PLANE_CONTRACT.z3Taskbar &&
          WORKSTATION_PLANE_CONTRACT.z3Taskbar > WORKSTATION_PLANE_CONTRACT.z2WindowsMax &&
          WORKSTATION_PLANE_CONTRACT.z2WindowsMin > WORKSTATION_PLANE_CONTRACT.z1TaskSpace &&
          WORKSTATION_PLANE_CONTRACT.z1TaskSpace > WORKSTATION_PLANE_CONTRACT.z0Backdrop,
        "5-plane Z-index hierarchy must be strictly monotonic",
      );

      const viewports: ReadonlyArray<readonly [number, number]> = [
        [1024, 768],
        [1366, 768],
        [1920, 1080],
        [2560, 1440],
        [3840, 2160],
      ];

      for (const [vw, vh] of viewports) {
        for (const mode of WM_LAYOUT_MODES) {
          const solved = solveWorkspaceGeometry({
            viewportWidth: vw,
            viewportHeight: vh,
            layoutMode: mode,
            showZ1TaskSpace: true,
            windows: [
              { windowId: "w1", isFloating: mode === "floating" },
              { windowId: "w2", isFloating: mode === "floating" },
              { windowId: "w3", isFloating: mode === "floating" },
              { windowId: "w4", isFloating: mode === "floating" },
            ],
            mruWindowIds: ["w4", "w3", "w2", "w1"],
          });

          assert(
            solved.stageRect.height === vh - TASKBAR_HEIGHT_PX,
            `StageRect height must equal ${vh} - 36 at ${vw}x${vh} (${mode})`,
          );
          assert(
            solved.taskbarRect.y === vh - TASKBAR_HEIGHT_PX &&
              solved.taskbarRect.height === TASKBAR_HEIGHT_PX,
            `TaskbarRect must occupy bottom 36px at ${vw}x${vh}`,
          );

          if (solved.z1TaskSpaceRect !== null) {
            assert(
              solved.z1TaskSpaceRect.y >= 0 &&
                solved.z1TaskSpaceRect.y + solved.z1TaskSpaceRect.height <=
                  vh - TASKBAR_HEIGHT_PX,
              `Z1 pane must never overlap 36px Z3 taskbar at ${vw}x${vh} (${mode})`,
            );
          }

          for (const win of solved.windows) {
            assert(
              win.outerFrameRect.x >= 0 &&
                win.outerFrameRect.y >= 0 &&
                win.outerFrameRect.x + win.outerFrameRect.width <= vw &&
                win.outerFrameRect.y + win.outerFrameRect.height <= vh - TASKBAR_HEIGHT_PX,
              `Window ${win.windowId} outer rect must stay strictly inside StageRect at ${vw}x${vh} (${mode})`,
            );
            assert(
              win.zIndex >= Z2_Z_INDEX_MIN && win.zIndex <= Z2_Z_INDEX_MAX,
              `Window ${win.windowId} zIndex ${win.zIndex} must stay within [200..299]`,
            );

            if (win.isFloating) {
              assert(win.borderRadiusPx === 6, "Floating window must have 6px radius");
            } else {
              assert(win.borderRadiusPx === 0, "Tiled window must have 0px radius");
            }

            const expectedClip = computeInnerContentClipRect(
              win.outerFrameRect,
              solved.stageRect,
            );
            assert(
              win.innerContentClipRect.y === win.outerFrameRect.y + WINDOW_TITLEBAR_HEIGHT_PX &&
                win.innerContentClipRect.height === expectedClip.height,
              `Inner content clip rect must subtract 30px titlebar (${mode})`,
            );
          }

          if (mode === "two-column-split" && solved.windows.length >= 2) {
            const leftWin = solved.windows[0]!;
            const rightWin = solved.windows[1]!;
            const gap = rightWin.outerFrameRect.x - (leftWin.outerFrameRect.x + leftWin.outerFrameRect.width);
            assert(gap === TILE_GAP_PX, `Two-column split gap must equal 4px (got ${gap})`);
          }
        }
      }

      const stage1080 = { x: 0, y: 0, width: 1920, height: 1080 - TASKBAR_HEIGHT_PX };
      const clampedFloat = clampFloatingRectToStage(
        { x: -400, y: 5000, width: 80, height: 60 },
        stage1080,
      );
      assert(
        clampedFloat.width >= MIN_FLOATING_WIDTH_PX &&
          clampedFloat.height >= MIN_FLOATING_HEIGHT_PX,
        "Floating window must clamp to >= 320x180 minimum size",
      );
      assert(
        clampedFloat.x >= 0 &&
          clampedFloat.y >= 0 &&
          clampedFloat.x + clampedFloat.width <= stage1080.width &&
          clampedFloat.y + clampedFloat.height <= stage1080.height,
        "Clamped floating window must stay 100% inside StageRect above 36px taskbar",
      );
    },
  );

  /* ------------------------------------------------------------------------
     Suite 2: Coupled WindowLifecycle <-> SurfaceLifecycle & Z3/Z4 Occlusion
     ------------------------------------------------------------------------ */
  await runSuite(
    "Suite 2: Coupled WindowLifecycle <-> SurfaceLifecycle & Z3/Z4 Occlusion",
    () => {
      const wm = createTilingWindowManager({
        viewportWidth: 1920,
        viewportHeight: 1080,
        initialLayoutMode: "two-column-split",
        showZ1TaskSpace: false,
      });

      const created = wm.registerWindow({
        windowId: "win-alpha",
        surfaceId: "surf-alpha",
        applicationIdHex: hex32("a1"),
        installationIdHex: hex32("b1"),
        title: "Alpha Document Editor",
        surfaceGeneration: 1,
        presentImmediately: false,
      });
      assert(
        created.windowState === "CREATED" && created.surfaceState === "CREATED",
        "Deferred window registration must start at CREATED / CREATED",
      );
      assert(
        wm.canRouteInputToWindow("win-alpha").allowed === false &&
          wm.canRouteInputToWindow("win-alpha").reason === "WINDOW_NOT_VISIBLE",
        "CREATED window must reject input before presentation",
      );

      assert(wm.presentWindow("win-alpha"), "presentWindow must succeed");
      let snap = wm.getSnapshot();
      let alpha = snap.windows.find((w) => w.windowId === "win-alpha")!;
      assert(
        alpha.windowState === "VISIBLE" && alpha.surfaceState === "PRESENTED",
        "presentWindow must couple VISIBLE <-> PRESENTED",
      );
      assert(wm.canRouteInputToWindow("win-alpha").allowed === true, "VISIBLE window accepts input");

      wm.registerWindow({
        windowId: "win-beta",
        surfaceId: "surf-beta",
        applicationIdHex: hex32("a2"),
        installationIdHex: hex32("b2"),
        title: "Beta Data Sheet",
        surfaceGeneration: 1,
        presentImmediately: true,
      });
      snap = wm.getSnapshot();
      assert(snap.focusedWindowId === "win-beta", "Newly presented window becomes MRU focused");
      const alphaGeom1 = snap.geometry.windows.find((g) => g.windowId === "win-alpha")!;
      const betaGeom1 = snap.geometry.windows.find((g) => g.windowId === "win-beta")!;
      assert(
        betaGeom1.zIndex > alphaGeom1.zIndex &&
          betaGeom1.zIndex <= Z2_Z_INDEX_MAX &&
          alphaGeom1.zIndex >= Z2_Z_INDEX_MIN,
        "Focused MRU window must hold strictly higher Z-index inside [200..299]",
      );

      wm.focusWindow("win-alpha");
      snap = wm.getSnapshot();
      const alphaGeom2 = snap.geometry.windows.find((g) => g.windowId === "win-alpha")!;
      const betaGeom2 = snap.geometry.windows.find((g) => g.windowId === "win-beta")!;
      assert(
        alphaGeom2.zIndex > betaGeom2.zIndex,
        "Focusing win-alpha must promote its Z-index above win-beta",
      );

      // Verify strict sandbox & CSP attributes in Z2 HTML
      const z2Html = wm.renderZ2PlaneHtml();
      assert(
        CSP_SANDBOX_POLICY === "allow-scripts" &&
          z2Html.includes(`sandbox="allow-scripts"`) &&
          !z2Html.includes("allow-same-origin") &&
          !z2Html.includes("allow-top-navigation") &&
          !z2Html.includes("allow-popups"),
        "Sandboxed surface iframe must strictly enforce sandbox='allow-scripts' only",
      );
      assert(
        CSP_SURFACE_DIRECTIVE.includes("default-src 'none'") &&
          !CSP_SURFACE_DIRECTIVE.includes("connect-src *") &&
          !CSP_SURFACE_DIRECTIVE.includes("frame-src"),
        "CSP surface directive must lock network & framing via default-src 'none'",
      );

      // Z3 flyout occlusion over left pane (win-alpha) while right pane (win-beta) stays unoccluded
      wm.setSystemOverlayState({
        activeZ3OverlayRects: [{ x: 8, y: 600, width: 320, height: 360 }],
      });
      snap = wm.getSnapshot();
      const alphaOcc = snap.geometry.windows.find((g) => g.windowId === "win-alpha")!;
      const betaOcc = snap.geometry.windows.find((g) => g.windowId === "win-beta")!;
      assert(
        alphaOcc.occludedBySystemPlane === true && betaOcc.occludedBySystemPlane === false,
        "Z3 flyout must engage occlusion fencing strictly on intersecting Z2 windows",
      );
      assert(
        wm.canRouteInputToWindow("win-alpha").allowed === false &&
          wm.canRouteInputToWindow("win-alpha").reason === "OCCLUDED_BY_SYSTEM_PLANE",
        "Occluded Z2 window must reject input routing",
      );
      assert(
        wm.canRouteInputToWindow("win-beta").allowed === true,
        "Non-intersecting Z2 window remains interactive during Z3 flyout",
      );

      // Z4 modal lock occludes ALL Z2 windows unconditionally
      wm.setSystemOverlayState({ z4ModalLock: true, activeZ3OverlayRects: [] });
      snap = wm.getSnapshot();
      assert(
        snap.geometry.windows.every((g) => g.occludedBySystemPlane === true),
        "Z4 modal lock must engage occlusion fencing across all Z2 windows",
      );
      wm.setSystemOverlayState({ z4ModalLock: false, activeZ3OverlayRects: [] });

      // Minimize -> Restore -> BeginClose -> Close coupled lifecycle
      assert(wm.minimizeWindow("win-alpha"), "minimizeWindow must succeed");
      alpha = wm.getSnapshot().windows.find((w) => w.windowId === "win-alpha")!;
      assert(
        alpha.windowState === "MINIMIZED" && alpha.surfaceState === "HIDDEN",
        "minimizeWindow must couple MINIMIZED <-> HIDDEN",
      );
      assert(wm.restoreWindow("win-alpha"), "restoreWindow must succeed");
      alpha = wm.getSnapshot().windows.find((w) => w.windowId === "win-alpha")!;
      assert(
        alpha.windowState === "VISIBLE" && alpha.surfaceState === "PRESENTED",
        "restoreWindow must couple VISIBLE <-> PRESENTED",
      );
      assert(wm.beginCloseWindow("win-alpha"), "beginCloseWindow must succeed");
      alpha = wm.getSnapshot().windows.find((w) => w.windowId === "win-alpha")!;
      assert(
        alpha.windowState === "CLOSING" && alpha.surfaceState === "HIDDEN",
        "beginCloseWindow must couple CLOSING <-> HIDDEN",
      );
      assert(wm.closeWindow("win-alpha"), "closeWindow must succeed");
      alpha = wm.getSnapshot().windows.find((w) => w.windowId === "win-alpha")!;
      assert(
        alpha.windowState === "CLOSED" && alpha.surfaceState === "CLOSED",
        "closeWindow must couple CLOSED <-> CLOSED",
      );
    },
  );

  /* ------------------------------------------------------------------------
     Suite 3: [DUI-LAUNCH-001] & Stale Generation Fence ([DUI-WINDOW-001])
     ------------------------------------------------------------------------ */
  await runSuite(
    "Suite 3: [DUI-LAUNCH-001] & Stale Generation Fence ([DUI-WINDOW-001])",
    () => {
      const appId = hex32("c1");
      const instId = hex32("d1");
      const pkgId = hex32("e1");
      const procId = hex32("f1");

      const busyAppEntry: InstalledApplicationEntry = {
        applicationIdHex: appId,
        installationIdHex: instId,
        packageIdHex: pkgId,
        displayName: "Engineering CAD Suite",
        status: "ACTIVE",
        installationGeneration: 1,
        hasActiveTasks: true,
        activeTaskCount: 2,
        activeProcessIdsHex: [procId],
      };

      const launcher = createApplicationLauncherModel({
        applications: [busyAppEntry],
        knownPackageIdsHex: [pkgId],
        knownProcessIdsHex: [procId],
      });

      const compositor = createWorkstationCompositor({
        viewportWidth: 1600,
        viewportHeight: 900,
        launcherModel: launcher,
      });

      // Reject packageIdHex directly and when smuggled into applicationIdHex
      const rejPkgDirect = compositor.launchApplication({
        applicationIdHex: appId,
        installationIdHex: instId,
        packageIdHex: pkgId,
      });
      assert(
        !rejPkgDirect.ok && rejPkgDirect.code === "PACKAGE_ID_NOT_APPLICATION_IDENTITY",
        "Supplying packageIdHex must be rejected with PACKAGE_ID_NOT_APPLICATION_IDENTITY",
      );

      const rejPkgSmuggled = compositor.launchApplication({
        applicationIdHex: pkgId,
        installationIdHex: instId,
      });
      assert(
        !rejPkgSmuggled.ok && rejPkgSmuggled.code === "PACKAGE_ID_NOT_APPLICATION_IDENTITY",
        "Smuggling packageIdHex inside applicationIdHex must be rejected",
      );

      // Reject processIdHex directly and when smuggled into installationIdHex
      const rejProcDirect = compositor.launchApplication({
        applicationIdHex: appId,
        installationIdHex: instId,
        processIdHex: procId,
      });
      assert(
        !rejProcDirect.ok && rejProcDirect.code === "PROCESS_ID_NOT_APPLICATION_IDENTITY",
        "Supplying processIdHex must be rejected with PROCESS_ID_NOT_APPLICATION_IDENTITY",
      );

      const rejProcSmuggled = compositor.launchApplication({
        applicationIdHex: appId,
        installationIdHex: procId,
      });
      assert(
        !rejProcSmuggled.ok && rejProcSmuggled.code === "PROCESS_ID_NOT_APPLICATION_IDENTITY",
        "Smuggling processIdHex inside installationIdHex must be rejected",
      );

      // Verify Uninstall lock when active tasks exist
      const appVerbs = computeLauncherAppVerbs(busyAppEntry);
      assert(
        appVerbs.canLaunch === true &&
          appVerbs.canDisable === true &&
          appVerbs.canUninstall === false &&
          appVerbs.uninstallLockedByActiveTasks === true,
        "Active tasks must lock UninstallApplication while keeping Launch and DisableApplication",
      );
      const launcherHtml = renderLauncherMenuHtml({
        model: launcher,
        locale: "en-US",
        disclosure: "L1",
      });
      assert(
        launcherHtml.includes("Active tasks running; uninstall locked"),
        "Launcher menu must render monochrome active-task lock reason",
      );

      // Valid launch by (applicationIdHex, installationIdHex)
      const okLaunch = compositor.launchApplication(
        { applicationIdHex: appId, installationIdHex: instId },
        { windowId: "win-cad-1", surfaceId: "surf-cad-1" },
      );
      assert(
        okLaunch.ok && okLaunch.windowRecord?.windowState === "VISIBLE",
        "Valid (applicationIdHex, installationIdHex) launch must open a VISIBLE Z2 window",
      );
      assert(
        compositor.routeInput({
          source: "pointer",
          targetPlane: "Z2",
          targetWindowId: "win-cad-1",
        }).accepted === true,
        "Fresh generation=1 window must accept Z2 pointer input",
      );

      // Advance installation generation 1 -> 2: marks gen=1 window stale & locks input
      const markedCount = compositor.windowManager.advanceApplicationGeneration(appId, 2);
      assert(markedCount === 1, "advanceApplicationGeneration(1 -> 2) must mark 1 open window stale");

      const staleZ2Html = compositor.windowManager.renderZ2PlaneHtml();
      assert(
        staleZ2Html.includes("nlos-wm-stale-banner") &&
          staleZ2Html.includes("nlos-wm-viewport--stale"),
        "Stale window must render solid stale banner and grayscale .nlos-wm-viewport--stale",
      );

      const staleInput = compositor.routeInput({
        source: "keyboard",
        targetPlane: "Z2",
        targetWindowId: "win-cad-1",
      });
      assert(
        staleInput.accepted === false &&
          staleInput.rejectionCode === "STALE_GENERATION_SURFACE" &&
          compositor.getSnapshot().inputAudit.rejectedStaleSurfaceEvents === 1,
        "Stale generation window must reject input with STALE_GENERATION_SURFACE and increment audit counter",
      );

      // Reloading to latest generation restores input routing
      assert(
        compositor.windowManager.reloadWindowGeneration("win-cad-1", { newGeneration: 2 }),
        "reloadWindowGeneration must upgrade window to generation 2",
      );
      assert(
        compositor.routeInput({
          source: "keyboard",
          targetPlane: "Z2",
          targetWindowId: "win-cad-1",
        }).accepted === true,
        "Reloaded generation=2 window must accept input again",
      );
    },
  );

  /* ------------------------------------------------------------------------
     Suite 4: L0-L3 Progressive Disclosure (< 16ms) & O(visible_rows) Fence
     ------------------------------------------------------------------------ */
  await runSuite(
    "Suite 4: L0-L3 Progressive Disclosure (< 16ms) & O(visible_rows) Lazy-Fetch Fence",
    async () => {
      let fakeClockMs = 100;
      const disclosureModel = createDisclosureModel({
        initialLevel: "L0",
        nowMs: () => {
          fakeClockMs += 1.25;
          return fakeClockMs;
        },
      });
      disclosureModel.setVisibleViewport(480, 30); // ceil(480/30) = 16 <= 64
      assert(
        computeVisibleCursorLimit(480, 30) === 16 &&
          computeVisibleCursorLimit(99999, 28) === MAX_SYSTEM_CONTROL_ENUM_LIMIT &&
          computeVisibleCursorLimit(0, 30) === 1,
        "computeVisibleCursorLimit must clamp strictly to 1..=64",
      );

      let l1ListTasksCalls = 0;
      let l2TaskNodesCalls = 0;
      let l3TaskAttemptsCalls = 0;
      let lastRequestedLimit = 0;

      const selectedTaskHex = hex32("11");
      const otherTaskHex = hex32("22");
      const dummyCmd = hex32("00");

      const store = createDesktopProjectionStore({
        disclosureModel,
        dataSource: {
          pollDesktopWatermarkEvents: async () => ({
            receiptHex: "00",
            controlCommandIdHex: dummyCmd,
            correlationIdHex: dummyCmd,
            outcome: {
              kind: "desktop_watermark_polled",
              currentWatermark: 7,
              events: [],
              truncated: false,
            },
          }),
          listTasks: async (_cursor, limit) => {
            l1ListTasksCalls += 1;
            lastRequestedLimit = limit ?? 0;
            return {
              receiptHex: "00",
              controlCommandIdHex: dummyCmd,
              correlationIdHex: dummyCmd,
              outcome: {
                kind: "tasks_listed",
                tasks: [
                  {
                    taskIdHex: selectedTaskHex,
                    planIdHex: hex32("31"),
                    applicationIdHex: null,
                    installationGeneration: 1,
                    planRevision: 2,
                    taskState: "RUNNING",
                    undoClass: "reversible",
                    reservationIdHex: hex32("41"),
                    cellIdHex: null,
                    updatedAtMs: 1000,
                    expectedGenerationOrRevision: 2,
                  },
                  {
                    taskIdHex: otherTaskHex,
                    planIdHex: hex32("32"),
                    applicationIdHex: null,
                    installationGeneration: 1,
                    planRevision: 1,
                    taskState: "RUNNING",
                    undoClass: "reversible",
                    reservationIdHex: hex32("42"),
                    cellIdHex: null,
                    updatedAtMs: 1000,
                    expectedGenerationOrRevision: 1,
                  },
                ],
                nextCursorHex: null,
                truncated: false,
              },
            };
          },
          listInstalledApplications: async () => ({
            receiptHex: "00",
            controlCommandIdHex: dummyCmd,
            correlationIdHex: dummyCmd,
            outcome: {
              kind: "installed_applications_listed",
              applications: [],
              nextCursorHex: null,
              truncated: false,
            },
          }),
          listWorkspaces: async () => ({
            receiptHex: "00",
            controlCommandIdHex: dummyCmd,
            correlationIdHex: dummyCmd,
            outcome: {
              kind: "workspaces_listed",
              workspaces: [],
              nextCursorHex: null,
              truncated: false,
            },
          }),
          listNotifications: async () => ({
            receiptHex: "00",
            controlCommandIdHex: dummyCmd,
            correlationIdHex: dummyCmd,
            outcome: {
              kind: "notifications_listed",
              notifications: [],
              nextCursorHex: null,
              truncated: false,
            },
          }),
          listCells: async () => ({
            receiptHex: "00",
            controlCommandIdHex: dummyCmd,
            correlationIdHex: dummyCmd,
            outcome: {
              kind: "cells_listed",
              cells: [],
              nextCursorHex: null,
              truncated: false,
            },
          }),
          listTaskNodes: async (planIdHex) => {
            l2TaskNodesCalls += 1;
            return {
              receiptHex: "00",
              controlCommandIdHex: dummyCmd,
              correlationIdHex: dummyCmd,
              outcome: {
                kind: "task_nodes_listed",
                planIdHex,
                currentPlanRevision: 2,
                staleGeneration: false,
                nodes: [],
                metadataOnlyCollapsedCount: 0,
                coldCollapsedCount: 0,
                nextCursorHex: null,
                truncated: false,
              },
            };
          },
          inspectResourceCost: async (reservationIdHex) => ({
            receiptHex: "00",
            controlCommandIdHex: dummyCmd,
            correlationIdHex: dummyCmd,
            outcome: {
              kind: "resource_inspected",
              reservationIdHex,
              accountIdHex: dummyCmd,
              upperBound: 100,
              usageHighWater: 40,
              consumptionCount: 2,
            },
          }),
          listTaskAttempts: async () => {
            l3TaskAttemptsCalls += 1;
            return {
              receiptHex: "00",
              controlCommandIdHex: dummyCmd,
              correlationIdHex: dummyCmd,
              outcome: {
                kind: "task_attempts_listed",
                currentGeneration: 1,
                staleGeneration: false,
                attempts: [],
                nextCursorHex: null,
                truncated: false,
              },
            };
          },
          listTaskGroups: async () => ({
            receiptHex: "00",
            controlCommandIdHex: dummyCmd,
            correlationIdHex: dummyCmd,
            outcome: {
              kind: "task_groups_listed",
              groups: [],
              nextCursorHex: null,
              truncated: false,
            },
          }),
        },
      });

      // L0: strictly L0 watermark posture only; zero L1/L2/L3 queries
      let snap = await store.refreshForCurrentDisclosure();
      assert(
        snap.queryTelemetry.L0 === 1 &&
          snap.queryTelemetry.L1 === 0 &&
          snap.queryTelemetry.L2 === 0 &&
          snap.queryTelemetry.L3 === 0 &&
          l1ListTasksCalls === 0,
        "L0 must issue 0 L1/L2/L3 queries",
      );
      const blockedAtL0 = await store.requestTieredFetch("list_tasks", async () => "never");
      assert(
        blockedAtL0.blockedByDisclosureFence === true &&
          store.getSnapshot().queryTelemetry.blockedByDisclosureFence === 1,
        "L1 query attempted at L0 must be blocked by disclosure fence",
      );

      // L1: bounded page lists (limit <= 16 <= 64), zero L2/L3 queries
      const l1Transition = disclosureModel.setLevel("L1");
      assert(l1Transition.lastTransitionDurationMs < 16, "L0 -> L1 transition must be < 16ms");
      snap = await store.refreshForCurrentDisclosure();
      assert(
        l1ListTasksCalls === 1 &&
          lastRequestedLimit === 16 &&
          l2TaskNodesCalls === 0 &&
          l3TaskAttemptsCalls === 0 &&
          snap.queryTelemetry.L2 === 0 &&
          snap.queryTelemetry.L3 === 0,
        "L1 must issue bounded page queries and 0 L2/L3 queries",
      );
      const blockedL3AtL1 = await store.requestTieredFetch(
        "list_task_attempts",
        async () => "never",
      );
      assert(
        blockedL3AtL1.blockedByDisclosureFence === true,
        "L3 query attempted at L1 must be blocked by disclosure fence",
      );

      // L2: single-selected-object detail only (zero N+1 across unselected tasks, zero L3)
      const l2Transition = disclosureModel.toggleDrawerL2(selectedTaskHex);
      assert(
        l2Transition.level === "L2" &&
          l2Transition.selectedEntityId === selectedTaskHex &&
          l2Transition.lastTransitionDurationMs < 16,
        "L1 -> L2 transition must preserve selectedEntityId in < 16ms",
      );
      assert(
        isQueryAllowedAtDisclosure("L2", "list_task_nodes", {
          targetEntityId: selectedTaskHex,
          selectedEntityId: selectedTaskHex,
        }) === true &&
          isQueryAllowedAtDisclosure("L2", "list_task_nodes", {
            targetEntityId: otherTaskHex,
            selectedEntityId: selectedTaskHex,
          }) === false,
        "L2 fence must allow L2 detail strictly for selectedEntityId and block unselected N+1 targets",
      );
      snap = await store.refreshForCurrentDisclosure();
      assert(
        l2TaskNodesCalls === 1 && l3TaskAttemptsCalls === 0 && snap.queryTelemetry.L3 === 0,
        "L2 refresh must fetch single selected entity nodes (1 call) and 0 L3 queries",
      );

      // L3: Ctrl+Shift+E toggle (< 16ms) + scoped expanded parent fetch
      disclosureModel.expandL3Parent(selectedTaskHex);
      const nextL3 = disclosureModel.toggleDiagnosticL3();
      assert(
        nextL3 === "L3" &&
          disclosureModel.getSnapshot().selectedEntityId === selectedTaskHex &&
          disclosureModel.getSnapshot().lastTransitionDurationMs < 16,
        "L2 -> L3 toggle must complete in < 16ms without losing selectedEntityId",
      );
      snap = await store.refreshForCurrentDisclosure();
      assert(
        l3TaskAttemptsCalls === 1 && snap.queryTelemetry.L3 > 0,
        "L3 refresh must execute L3 queries strictly for expanded parent IDs",
      );

      // Toggle back L3 -> L2 preserves preDiagnosticLevel & selection
      const backToL2 = disclosureModel.toggleDiagnosticL3();
      assert(
        backToL2 === "L2" && disclosureModel.getSnapshot().selectedEntityId === selectedTaskHex,
        "Toggling out of L3 must return to remembered L2 state without context loss",
      );
    },
  );

  /* ------------------------------------------------------------------------
     Suite 5: [UI-PARITY-001] Three Laws & 5+1 Statutory Non-Tamperable Locks
     ------------------------------------------------------------------------ */
  await runSuite(
    "Suite 5: [UI-PARITY-001] Three Laws & 5+1 Statutory Non-Tamperable Locks",
    () => {
      const validReadBacking = {
        targetIdHex: hex32("ab"),
        expectedGenerationOrRevision: 3,
      };
      const fullCaps = [
        "control:read",
        "control:write",
        "control:destructive",
        "application:manage",
        "plan:edit",
        "cell:migrate",
      ];

      // Domain coverage + Law 1 (No Blind Control) + Law 2 (No Orphan Display)
      assert(ALL_ACTION_PRUNER_DOMAINS.length >= 12, "Must cover all 12+ NLOS resource domains");
      for (const domain of ALL_ACTION_PRUNER_DOMAINS) {
        const withoutReadBacking = computeAllowedVerbs({
          domain,
          state: "ACTIVE",
          callerCapabilities: fullCaps,
          readBacking: null,
          locale: "en-US",
          disclosure: "L1",
        });
        assert(
          withoutReadBacking.readOnlyReason !== null &&
            withoutReadBacking.readOnlyReason.length > 0,
          `Law 1 & Law 2: domain '${domain}' without readBacking must yield explicit readOnlyReason`,
        );

        // Self-granting SYSTEM_CRITICAL alone must be stripped -> zero write authority
        const selfCriticalOnly = computeAllowedVerbs({
          domain,
          state: "ACTIVE",
          callerCapabilities: ["SYSTEM_CRITICAL"],
          readBacking: validReadBacking,
          locale: "zh-CN",
          disclosure: "L1",
        });
        assert(
          selfCriticalOnly.allowedVerbs.length === 0 &&
            selfCriticalOnly.readOnlyReason !== null,
          `Domain '${domain}' must strip self-granted SYSTEM_CRITICAL capability`,
        );
      }

      // Lock 1: Search 0x1F non-authoritative read-only + fail-closed when != 0x1F
      const searchValid = computeAllowedVerbs({
        domain: "search",
        state: "READY",
        callerCapabilities: fullCaps,
        readBacking: validReadBacking,
        searchRestrictionsBits: SEARCH_NON_AUTHORITATIVE_RESTRICTIONS_ALL,
      });
      assert(
        searchValid.statutoryLock === "SEARCH_0X1F_READ_ONLY" &&
          searchValid.allowedVerbs.length === 1 &&
          searchValid.allowedVerbs[0] === "JumpToAuthoritativeObject",
        "0x1F search candidate must expose strictly JumpToAuthoritativeObject",
      );

      const searchTampered = computeAllowedVerbs({
        domain: "search",
        state: "READY",
        callerCapabilities: fullCaps,
        readBacking: validReadBacking,
        searchRestrictionsBits: 0x07, // != 0x1F -> fail closed!
      });
      assert(
        searchTampered.allowedVerbs.length === 0 &&
          searchTampered.statutoryLock === "SEARCH_0X1F_READ_ONLY" &&
          searchTampered.readOnlyReason !== null,
        "Search candidate with restrictionsBits != 0x1F must fail closed with 0 verbs",
      );

      // Lock 2: IRREVERSIBLE receipt omits Undo while preserving ViewEffectProof
      const irreversibleTask = computeAllowedVerbs({
        domain: "task",
        state: "COMPLETED",
        callerCapabilities: fullCaps,
        readBacking: validReadBacking,
        undoClass: "irreversible",
      });
      assert(
        !irreversibleTask.allowedVerbs.includes("Undo") &&
          irreversibleTask.allowedVerbs.includes("ViewEffectProof") &&
          irreversibleTask.statutoryLock === "IRREVERSIBLE_NO_UNDO",
        "IRREVERSIBLE task receipt must physically omit Undo while keeping ViewEffectProof",
      );

      // Lock 3: Completed / Active TaskNode omits topology edit verbs
      for (const lockedNodeState of ["COMPLETED", "ACTIVE"]) {
        const nodeRes = computeAllowedVerbs({
          domain: "task-node",
          state: lockedNodeState,
          callerCapabilities: fullCaps,
          readBacking: validReadBacking,
        });
        assert(
          nodeRes.statutoryLock === "NODE_COMPLETED_OR_ACTIVE_LOCKED" &&
            !nodeRes.allowedVerbs.includes("DeleteNode") &&
            !nodeRes.allowedVerbs.includes("ReplaceNode") &&
            !nodeRes.allowedVerbs.includes("RewireEdge") &&
            !nodeRes.allowedVerbs.includes("MergeNode"),
          `TaskNode in ${lockedNodeState} must lock out topology edit verbs`,
        );
      }

      // Lock 4: TaskAttempt forbids manual Commit / PromoteWinner
      const attemptRes = computeAllowedVerbs({
        domain: "task-attempt",
        state: "RUNNING",
        callerCapabilities: fullCaps,
        readBacking: validReadBacking,
      });
      assert(
        attemptRes.statutoryLock === "TASK_ATTEMPT_NO_MANUAL_COMMIT" &&
          !attemptRes.allowedVerbs.includes("Commit") &&
          !attemptRes.allowedVerbs.includes("PromoteWinner"),
        "TaskAttempt must forbid manual Commit / PromoteWinner",
      );

      // Lock 5: EFFECT_UNKNOWN DurableOperation forbids manual success override
      const unknownOpRes = computeAllowedVerbs({
        domain: "operation",
        state: "EFFECT_UNKNOWN",
        callerCapabilities: fullCaps,
        readBacking: validReadBacking,
      });
      assert(
        unknownOpRes.statutoryLock === "OPERATION_EFFECT_UNKNOWN_NO_MANUAL_SUCCESS" &&
          !unknownOpRes.allowedVerbs.includes("MarkSucceeded") &&
          !unknownOpRes.allowedVerbs.includes("ForceComplete"),
        "EFFECT_UNKNOWN operation must forbid manual success override",
      );

      // Lock 6 (+1): Quarantined Cell / ExclusiveDevice lease fence
      const quarantinedCellRes = computeAllowedVerbs({
        domain: "cell",
        state: "QUARANTINED",
        callerCapabilities: fullCaps,
        readBacking: validReadBacking,
      });
      assert(
        quarantinedCellRes.statutoryLock === "QUARANTINED_LEASE_FENCE" &&
          quarantinedCellRes.allowedVerbs.length === 1 &&
          quarantinedCellRes.allowedVerbs[0] === "InspectFenceStatus",
        "QUARANTINED cell must omit lease/migration writes and allow only InspectFenceStatus",
      );
    },
  );

  /* ------------------------------------------------------------------------
     Suite 6: [CTRL-RECOVERY-001] ControlCommandReconciler & [RCPT-PARTIAL-001]
     ------------------------------------------------------------------------ */
  await runSuite(
    "Suite 6: [CTRL-RECOVERY-001] ControlCommandReconciler & [RCPT-PARTIAL-001]",
    async () => {
      let dispatchCallCount = 0;
      let shouldThrowDisconnect = true;
      let nextStatusState = "pending";

      const reconciler = createCommandReconciler({
        dispatchCommand: async (_action, meta) => {
          dispatchCallCount += 1;
          if (shouldThrowDisconnect) {
            throw new Error("IPC socket closed mid-flight before receipt");
          }
          const okReceipt: TypedReceiptDto<OutcomeDto> = {
            controlCommandIdHex: meta.controlCommandIdHex,
            correlationIdHex: hex32("cc"),
            receiptHex: "deadbeef",
            outcome: {
              kind: "operation_paused",
              receiptIdHex: hex32("99"),
            },
          };
          return okReceipt;
        },
        getControlCommandStatus: async (cmdId) => ({
          state: nextStatusState,
          controlCommandIdHex: cmdId,
          receiptIdHex: nextStatusState === "succeeded" ? hex32("88") : null,
          failureCode: nextStatusState === "failed" ? "CONFLICT" : null,
          failureSafeMessage: nextStatusState === "failed" ? "Revision conflict" : null,
        }),
      });

      const pauseAction: AnyControlActionInput = {
        action: "pause-operation",
        targetIdHex: hex32("55"),
        expectedRevision: 1,
        scope: "task",
        reason: "Pause report task",
      };

      // 1. Mid-flight disconnect transitions target into UNKNOWN_NEEDS_RECONCILE
      const firstAttempt = await reconciler.dispatch({
        action: pauseAction,
        controlCommandIdHex: hex32("01"),
      });
      assert(
        !firstAttempt.ok &&
          firstAttempt.state === "UNKNOWN_NEEDS_RECONCILE" &&
          dispatchCallCount === 1,
        "Disconnected command must enter UNKNOWN_NEEDS_RECONCILE with 1 dispatch call",
      );

      // 2. Blind retry MUST be blocked with 0 additional dispatchCommand invocations!
      const blindProbe = reconciler.retryBlindly(firstAttempt.targetKey);
      assert(
        !blindProbe.ok && blindProbe.code === BLIND_RETRY_FORBIDDEN_ON_UNKNOWN,
        "retryBlindly must reject with BLIND_RETRY_FORBIDDEN_ON_UNKNOWN",
      );

      let threwForbidden = false;
      try {
        reconciler.assertCanIssueForTarget(firstAttempt.targetKey);
      } catch (err) {
        threwForbidden = err instanceof BlindRetryForbiddenError;
      }
      assert(threwForbidden, "assertCanIssueForTarget must throw BlindRetryForbiddenError");

      const duplicateDispatchAttempt = await reconciler.dispatch({
        action: pauseAction,
        controlCommandIdHex: hex32("02"),
      });
      assert(
        !duplicateDispatchAttempt.ok &&
          duplicateDispatchAttempt.code === BLIND_RETRY_FORBIDDEN_ON_UNKNOWN &&
          dispatchCallCount === 1,
        "dispatch() on fenced targetKey must refuse without calling dispatchCommand again",
      );

      // 3. Reconcile "succeeded" -> clears lock and marks SUCCEEDED
      nextStatusState = "succeeded";
      const recSucceeded = await reconciler.reconcile(firstAttempt.controlCommandIdHex);
      assert(
        recSucceeded.state === "SUCCEEDED" &&
          recSucceeded.resolution === "SUCCEEDED" &&
          recSucceeded.safeToIssueFreshCommand === true &&
          !reconciler.isTargetBlockedByUnknown(firstAttempt.targetKey),
        "Reconciling 'succeeded' must resolve to SUCCEEDED and lift target lock",
      );

      // 4. Reconcile "failed" -> clears lock and marks REJECTED
      const failedAction: AnyControlActionInput = {
        action: "cancel-operation",
        targetIdHex: hex32("56"),
        expectedRevision: 1,
        scope: "task",
        reason: "Cancel task",
      };
      const secondUnknown = await reconciler.dispatch({
        action: failedAction,
        controlCommandIdHex: hex32("03"),
      });
      nextStatusState = "failed";
      const recFailed = await reconciler.reconcile(secondUnknown.controlCommandIdHex);
      assert(
        recFailed.state === "REJECTED" &&
          recFailed.resolution === "REJECTED" &&
          recFailed.failureCode === "CONFLICT" &&
          !reconciler.isTargetBlockedByUnknown(secondUnknown.targetKey),
        "Reconciling 'failed' must resolve to REJECTED and lift target lock",
      );

      // 5. Reconcile "not_found" -> SAFE_TO_ISSUE_FRESH_COMMAND -> fresh command succeeds!
      const notFoundAction: AnyControlActionInput = {
        action: "resume-operation",
        targetIdHex: hex32("57"),
        expectedRevision: 2,
        scope: "task",
        reason: "Resume task",
      };
      const thirdUnknown = await reconciler.dispatch({
        action: notFoundAction,
        controlCommandIdHex: hex32("04"),
      });
      const callsBeforeReconcile = dispatchCallCount;
      nextStatusState = "not_found";
      const recNotFound = await reconciler.reconcile(thirdUnknown.controlCommandIdHex);
      assert(
        recNotFound.resolution === SAFE_TO_ISSUE_FRESH_COMMAND &&
          recNotFound.safeToIssueFreshCommand === true &&
          !reconciler.isTargetBlockedByUnknown(thirdUnknown.targetKey),
        "Reconciling 'not_found' must mark SAFE_TO_ISSUE_FRESH_COMMAND and lift lock",
      );

      shouldThrowDisconnect = false;
      const freshAfterNotFound = await reconciler.dispatch({
        action: notFoundAction,
        controlCommandIdHex: hex32("05"),
      });
      assert(
        freshAfterNotFound.ok &&
          freshAfterNotFound.state === "SUCCEEDED" &&
          dispatchCallCount === callsBeforeReconcile + 1,
        "Fresh command with new ID after not_found reconciliation must succeed cleanly",
      );

      // 6. [RCPT-PARTIAL-001] Batch per-target outcome summary
      const batchSummary = summarizeBatchTargetOutcomes([
        { targetIdHex: hex32("71"), state: "SUCCEEDED", receiptIdHex: hex32("81") },
        { targetIdHex: hex32("72"), state: "REJECTED", reason: "Capability denied" },
        { targetIdHex: hex32("73"), state: "UNKNOWN_NEEDS_RECONCILE", controlCommandIdHex: hex32("91") },
      ]);
      assert(
        batchSummary.total === 3 &&
          batchSummary.succeeded === 1 &&
          batchSummary.rejected === 1 &&
          batchSummary.unknown === 1 &&
          batchSummary.isPartial === true &&
          batchSummary.hasUnknownRequiringReconcile === true,
        "[RCPT-PARTIAL-001] summarizeBatchTargetOutcomes must preserve per-target partial & unknown states",
      );
    },
  );

  /* ------------------------------------------------------------------------
     Suite 7: [DUI-SESSION-001] & [DUI-INPUT-001] WorkstationCompositor, Z4 350ms Guard & Shortcuts
     ------------------------------------------------------------------------ */
  await runSuite(
    "Suite 7: [DUI-SESSION-001] & [DUI-INPUT-001] WorkstationCompositor, Z4 350ms Guard & Shortcuts",
    () => {
      const { doc, createHost } = createMockDocument();
      const host = createHost();
      let monoClock = 1000;

      const appId = hex32("a9");
      const instId = hex32("b9");
      const secretSealAlice = "ALICE-FJORD-91";
      const secretSealBob = "BOB-SUMMIT-44";

      const compositor = createWorkstationCompositor({
        doc,
        viewportWidth: 1920,
        viewportHeight: 1080,
        initialDisclosure: "L1",
        initialTheme: "graphite-workstation",
        initialLocale: "zh-CN",
        initialSecuritySealPhrase: secretSealAlice,
        nowMonoMs: () => monoClock,
        installedApplications: [
          {
            applicationIdHex: appId,
            installationIdHex: instId,
            displayName: "Notes Studio",
            status: "ACTIVE",
            installationGeneration: 1,
          },
        ],
        initialTasks: [
          {
            taskId: hex32("61"),
            title: "整理月度财务核对表",
            status: "RUNNING",
            actualUsage: 42,
            effectiveGrantedOrLeaseCap: 80,
            requestedCeiling: 100,
            resourceUnit: "MB",
            summaryNote: "正在比对三份月度明细账单",
            planIdHex: hex32("62"),
            canPause: true,
            canCancel: true,
          },
        ],
      });

      compositor.mount(host as unknown as HTMLElement, { doc });
      compositor.launchApplication(
        { applicationIdHex: appId, installationIdHex: instId },
        { windowId: "win-notes-1", surfaceId: "surf-notes-1" },
      );

      // Verify untrusted Z2 surface context never leaks seal phrase or principal identity
      const surfaceCtx = compositor.getUntrustedSurfaceVisibleContext("win-notes-1")!;
      assert(
        surfaceCtx !== null &&
          !("sealPhrase" in surfaceCtx) &&
          !("principalIdHex" in surfaceCtx) &&
          !JSON.stringify(surfaceCtx).includes(secretSealAlice),
        "Sandboxed Z2 surface context must never expose personal seal phrase or principal ID",
      );

      // Verify when Z4 is inactive, 0 verification seals exist anywhere in Z0..Z3
      const sealsWhileActive = host.findAll(
        (el) => el.getAttribute("data-nlos-seal") === "personal-verification-seal",
      );
      assert(sealsWhileActive.length === 0, "Inactive Z4 must render 0 verification seals");

      // Ctrl+Alt+L locks DesktopSession: increments sessionGeneration, clears cache, mounts 32x20 seal ONLY in Z4
      compositor.disclosureModel.expandL3Parent("temp-parent");
      const genBeforeLock = compositor.getSnapshot().sessionGeneration;
      const lockRes = compositor.handleKeydown({
        key: "l",
        ctrlKey: true,
        altKey: true,
      });
      const lockedSnap = compositor.getSnapshot();
      assert(
        lockRes.handled &&
          lockRes.action === "lock-session" &&
          lockedSnap.sessionState === "LOCKED" &&
          lockedSnap.sessionGeneration === genBeforeLock + 1 &&
          compositor.disclosureModel.getExpandedL3ParentIds().size === 0,
        "Ctrl+Alt+L must lock session, bump sessionGeneration, and purge sensitive projection caches",
      );

      const z4PlaneNodes = host.findAll((el) => el.getAttribute("data-nlos-plane") === "Z4");
      assert(z4PlaneNodes.length === 1 && !z4PlaneNodes[0]!.hidden, "Z4 plane must be visible when LOCKED");
      const sealsInZ4 = z4PlaneNodes[0]!.findAll(
        (el) => el.getAttribute("data-nlos-seal") === "personal-verification-seal",
      );
      const allSealsInHost = host.findAll(
        (el) => el.getAttribute("data-nlos-seal") === "personal-verification-seal",
      );
      assert(
        sealsInZ4.length === 1 && allSealsInHost.length === 1,
        "32x20 VerificationSeal must exist strictly inside Z4 and NEVER in Z0..Z3",
      );
      const canvasNode = sealsInZ4[0]!.findAll((el) => el.tagName === "canvas")[0]!;
      assert(
        canvasNode.width === SEAL_WIDTH_PX && canvasNode.height === SEAL_HEIGHT_PX,
        "VerificationSeal canvas inside Z4 must be strictly 32x20px",
      );

      // Unlock session -> ACTIVE
      assert(compositor.unlockSession() === true, "unlockSession must restore ACTIVE session");

      // Open Z4 security modal at t = 2000ms and test 350ms arming guard + 5-source input severance
      monoClock = 2000;
      let confirmedCount = 0;
      compositor.openZ4Modal(
        {
          promptId: "prompt-high-impact",
          title: "授权跨设备目录同步",
          blastRadiusItems: [
            {
              symbol: "~",
              category: "reverified",
              scopeLabel: "工作区",
              targetLabel: "财务归档目录",
              reason: "更新 12 份报表",
            },
          ],
          onConfirm: () => {
            confirmedCount += 1;
          },
        },
        2000,
      );

      // Confirm at 0ms and 349ms MUST be rejected by Z4_ARMING_GUARD
      const early0 = compositor.confirmZ4Modal(2000);
      const early349 = compositor.confirmZ4Modal(2349);
      assert(
        !early0.accepted &&
          early0.rejectionCode === "Z4_ARMING_GUARD" &&
          !early349.accepted &&
          early349.rejectionCode === "Z4_ARMING_GUARD" &&
          confirmedCount === 0 &&
          compositor.isZ4ModalOpen() === true &&
          compositor.getSnapshot().inputAudit.rejectedZ4ArmingGuardEvents === 2,
        `Z4 arming guard (${Z4_ARMING_WINDOW_MS}ms) must reject confirm at 0ms and 349ms`,
      );

      // All 5 input sources targeting Z2 while Z4 is active MUST be severed and audited
      for (const src of ALL_INPUT_SOURCES) {
        const res = compositor.routeInput({
          source: src,
          targetPlane: "Z2",
          targetWindowId: "win-notes-1",
          nowMonoMs: 2100,
        });
        assert(
          !res.accepted && res.rejectionCode === "Z4_MODAL_LOCK",
          `Input source '${src}' targeting Z2 must be severed during active Z4 modal`,
        );
        assert(
          compositor.getSnapshot().rejectedBySource[src] >= 1,
          `Severed input source '${src}' must be recorded in rejectedBySource audit map`,
        );
      }

      // Untrusted surface attempting to spoof Z4 confirm or Z3 shortcut MUST be rejected
      const spoofRes = compositor.routeInput({
        source: "automation",
        targetPlane: "Z4",
        fromUntrustedSurface: true,
        action: "confirm-z4",
        nowMonoMs: 2500,
      });
      assert(
        !spoofRes.accepted &&
          spoofRes.rejectionCode === "UNTRUSTED_ORIGIN_SPOOF" &&
          compositor.getSnapshot().inputAudit.rejectedUntrustedSpoofEvents === 1,
        "Untrusted Z2 surface spoofing Z4 must be rejected with UNTRUSTED_ORIGIN_SPOOF",
      );

      // Escape at 0ms on a freshly opened Z4 modal MUST cancel immediately!
      const escapeAtZero = compositor.handleKeydown({ key: "Escape", nowMonoMs: 2000 });
      assert(
        escapeAtZero.handled &&
          escapeAtZero.action === "cancel-z4" &&
          !compositor.isZ4ModalOpen(),
        "Escape must cancel Z4 modal immediately even at 0ms",
      );

      // Re-open Z4 modal at 3000ms and confirm at >= 3350ms (350ms elapsed): MUST succeed!
      compositor.openZ4Modal(
        {
          promptId: "prompt-high-impact-2",
          title: "授权导出审计归档",
          onConfirm: () => {
            confirmedCount += 1;
          },
        },
        3000,
      );
      const confirmAt350 = compositor.handleKeydown({ key: "Enter", nowMonoMs: 3350 });
      assert(
        confirmAt350.handled &&
          confirmAt350.action === "confirm-z4" &&
          confirmedCount === 1 &&
          !compositor.isZ4ModalOpen(),
        "Confirm at >= 350ms must succeed and close Z4 modal",
      );

      // Keyboard shortcuts: 1..4 layout switching (and ignored when typing in text input)
      const ignoredInInput = compositor.handleKeydown({
        key: "1",
        targetIsTextInput: true,
      });
      assert(!ignoredInInput.handled, "Bare digit '1' inside text input must not switch layout");

      for (const [keyDigit, expectedMode] of [
        ["1", "workspace-maximize"],
        ["2", "two-column-split"],
        ["4", "quadrants"],
        ["3", "three-pane-workstation"],
      ] as const) {
        const r = compositor.handleKeydown({ key: keyDigit });
        assert(
          r.handled &&
            r.layoutMode === expectedMode &&
            compositor.getSnapshot().wm.layoutMode === expectedMode,
          `Shortcut '${keyDigit}' must switch layout to '${expectedMode}'`,
        );
      }

      // Space/Enter on Z1 task row toggles L1 <-> L2 InspectorDrawer; F6 cycles landmarks
      assert(
        LANDMARK_RING_ORDER.length === 4,
        "LANDMARK_RING_ORDER must define all 4 canonical workstation landmarks",
      );
      const openL2 = compositor.handleKeydown({
        key: "Enter",
        targetTaskId: hex32("61"),
      });
      assert(
        openL2.handled &&
          openL2.disclosure === "L2" &&
          compositor.getSnapshot().inspectorOpen === true &&
          compositor.getAvailableLandmarks().includes("z2-inspector-drawer"),
        "Enter on Z1 task row must open non-modal L2 InspectorDrawer and add it to F6 ring",
      );

      const seenLandmarks = new Set<string>();
      for (let i = 0; i < 4; i += 1) {
        const step = compositor.handleKeydown({ key: "F6" });
        if (step.landmark) {
          seenLandmarks.add(step.landmark);
        }
      }
      assert(
        seenLandmarks.size === 4,
        "Pressing F6 four times with window + Z1 + L2 drawer + taskbar open must visit all 4 landmarks",
      );
      const backStep = compositor.handleKeydown({ key: "F6", shiftKey: true });
      assert(backStep.handled && backStep.action === "cycle-landmark", "Shift+F6 must cycle reverse");

      // Ctrl+Shift+E toggles L2 -> L3 -> L2
      const toL3 = compositor.handleKeydown({
        key: "E",
        ctrlKey: true,
        shiftKey: true,
      });
      assert(toL3.handled && toL3.disclosure === "L3", "Ctrl+Shift+E must enter L3");
      const backFromL3 = compositor.handleKeydown({
        key: "E",
        ctrlKey: true,
        shiftKey: true,
      });
      assert(backFromL3.handled && backFromL3.disclosure === "L2", "Ctrl+Shift+E must return to L2");

      // Multi-user fast switch isolates windows and seal phrases and bumps sessionGeneration
      const genBeforeSwitch = compositor.getSnapshot().sessionGeneration;
      compositor.switchUser(
        {
          principalIdHex: hex32("02"),
          displayName: "Secondary Operator",
          locale: "en-US",
          theme: "paper-workstation",
          dataContainerScopeIdHex: hex32("20"),
        },
        { securitySealPhrase: secretSealBob },
      );
      const bobSnap = compositor.getSnapshot();
      assert(
        bobSnap.sessionGeneration === genBeforeSwitch + 1 &&
          bobSnap.wm.windows.length === 0 &&
          bobSnap.theme === "paper-workstation",
        "Switching user must increment sessionGeneration and isolate previous user's Z2 windows",
      );
      compositor.lockSession();
      const bobLockHtml = compositor.renderShellHtml();
      assert(
        bobLockHtml.includes(secretSealBob) && !bobLockHtml.includes(secretSealAlice),
        "Switched user's Z4 lock screen must render strictly their own personal seal phrase",
      );
      compositor.unmount();
    },
  );

  /* ------------------------------------------------------------------------
     Suite 8: Full Static & Rendered ANTI-AI-01..07 Audit
     ------------------------------------------------------------------------ */
  await runSuite(
    "Suite 8: Full Static & Rendered ANTI-AI-01..07 Audit (zh-CN & en-US)",
    () => {
      const scanDirs = [
        pathMod.join(srcRoot, "styles"),
        pathMod.join(srcRoot, "ui"),
        pathMod.join(srcRoot, "store"),
        pathMod.join(srcRoot, "wm"),
        pathMod.join(srcRoot, "dock"),
        pathMod.join(srcRoot, "compositor"),
      ];
      const filesToAudit: string[] = [];
      for (const dir of scanDirs) {
        filesToAudit.push(...collectProductionFiles(fs, pathMod, dir));
      }
      filesToAudit.push(pathMod.join(srcRoot, "style.css"));
      filesToAudit.push(pathMod.join(srcRoot, "main.ts"));

      assert(filesToAudit.length >= 20, "Static audit must scan all Stage D CSS & TS modules");

      const allStaticViolations: string[] = [];
      for (const filePath of filesToAudit) {
        const content = fs.readFileSync(filePath, "utf-8");
        const baseName = pathMod.basename(filePath);
        allStaticViolations.push(
          ...auditFileForAntiAiViolations(filePath, baseName, content),
        );
      }
      assert(
        allStaticViolations.length === 0,
        `Static ANTI-AI-01..06 violations found:\n${allStaticViolations.join("\n")}`,
      );

      // Rendered ANTI-AI-07 audit across L0 and L1 in both zh-CN and en-US
      const locales: readonly WorkstationLocale[] = ["zh-CN", "en-US"];
      const levels: readonly DisclosureLevel[] = ["L0", "L1"];

      for (const locale of locales) {
        for (const level of levels) {
          const { doc, createHost } = createMockDocument();
          const host = createHost();
          const l3DiagnosticDummy = doc.createElement("div");
          l3DiagnosticDummy.textContent =
            "L3-ONLY-DIAGNOSTIC: control_command_id receipt_hex TaskAttempt ExecutionFiber 0x1F LOCAL_SINGLE_NODE";

          const comp = createWorkstationCompositor({
            doc,
            initialLocale: locale,
            initialDisclosure: level,
            installedApplications: [
              {
                applicationIdHex: hex32("a1"),
                installationIdHex: hex32("b1"),
                displayName: locale === "zh-CN" ? "表格工作台" : "Spreadsheet Workbench",
                status: "ACTIVE",
                installationGeneration: 1,
                hasActiveTasks: true,
              },
            ],
            initialTasks: [
              {
                taskId: hex32("c1"),
                title: locale === "zh-CN" ? "汇总项目周报" : "Compile weekly summary",
                status: "RUNNING",
                actualUsage: 35,
                effectiveGrantedOrLeaseCap: 75,
                requestedCeiling: 100,
                resourceUnit: "MB",
                planIdHex: hex32("d1"),
                canPause: true,
                canCancel: true,
              },
            ],
          });
          comp.setL3DiagnosticElement(l3DiagnosticDummy);

          // Check pure HTML visible text
          const htmlVisibleText = comp.getVisibleTextForAudit();
          for (const banned of L0_L1_BANNED_JARGON) {
            assert(
              !htmlVisibleText.includes(banned),
              `ANTI-AI-07 HTML violation (${locale}, ${level}): leaked jargon '${banned}' in '${htmlVisibleText}'`,
            );
          }

          // Check live mounted DOM visible text (including opened launcher flyout)
          comp.mount(host as unknown as HTMLElement, { doc });
          comp.setLauncherOpen(true);
          const domVisibleText = comp.getVisibleTextForAudit();
          for (const banned of L0_L1_BANNED_JARGON) {
            assert(
              !domVisibleText.includes(banned),
              `ANTI-AI-07 DOM violation (${locale}, ${level}): leaked jargon '${banned}' in '${domVisibleText}'`,
            );
          }
          assert(
            !PICTOGRAPHIC_RE.test(domVisibleText),
            `Rendered text (${locale}, ${level}) must contain 0 emojis`,
          );
          comp.unmount();
        }
      }
    },
  );

  console.log(
    `\nAll ${passedSuites} D-COMPOSITOR-WM suites passed (${passedAssertions} assertions).`,
  );
}

void main().catch((err: unknown) => {
  console.error("D-COMPOSITOR-WM verification failed:", err);
  const proc = (globalThis as { process?: { exitCode?: number } }).process;
  if (proc) {
    proc.exitCode = 1;
  }
  throw err;
});
