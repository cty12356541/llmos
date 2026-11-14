// TDD Verification Suite — Full Industrial Workstation Surface Mounting into Z2 Tiling & Z3 Flyouts.
// Verifies:
//   1. First-party trusted workstation surfaces mount directly into Z2 tiling windows while
//      preserving sandboxed <iframe> isolation for untrusted third-party windows.
//   2. Default 3-pane workstation startup mounts both the 12-Domain Control & Resource Center
//      and the Task DAG / Store Catalog / Settings / Search multi-tab workbench in Z2.
//   3. Bottom 36px taskbar buttons (connectivity, notifications, clock) and Ctrl+K wire real
//      ClusterPostureTray, NotificationCenter, SystemControlCenterFlyout, and SpotlightView into Z3.
//   4. L0/L1 visible DOM text across all mounted industrial surfaces obeys ANTI-AI-05/07
//      (zero emojis, zero internal architecture jargon).
//   5. `desktop/src/main.ts` wires `mountIndustrialWorkstationSurfaces` instead of no-op stubs.

import { createWorkstationCompositor } from "../workstation-compositor";
import { mountIndustrialWorkstationSurfaces } from "../workstation-shell-mount";
import { createSystemControlCenterController } from "../../views/control-center";
import { NotificationCenterController } from "../../views/notifications";
import { ClusterPostureController } from "../../views/offline-cluster";
import { createWorkstationSettingsSuiteController } from "../../views/settings";
import { SpotlightController } from "../../views/spotlight";
import {
  createStoreCatalogController,
  STORE_CHAIN_STATUS_VERIFIED,
  STORE_SOURCE_LIVE_REGISTRY,
  type StoreCatalogEntryDto,
  type StoreCatalogSnapshotDto,
  type StorePermissionDiffDto,
  type StoreReleaseTransparencyDto,
} from "../../views/store-catalog";
import { createTaskSpaceSurfaceController } from "../../views/task-space";
import type { DisclosureLevel, WorkstationLocale } from "../../ui";

/* ==========================================================================
   Minimal Deterministic DOM Mock (matching compositor-wm.test.ts)
   ========================================================================== */

interface ListenerEntry {
  readonly type: string;
  readonly handler: (ev: Record<string, unknown>) => void;
}

class MockElement {
  public readonly tagName: string;
  public className = "";
  public id = "";
  public hidden = false;
  public tabIndex = 0;
  public style: Record<string, string> = {};
  public dataset: Record<string, string> = {};
  public readonly attributes = new Map<string, string>();
  public readonly children: MockElement[] = [];
  public parentElement: MockElement | null = null;
  private rawText = "";
  private rawInnerHtml = "";
  private readonly listeners: ListenerEntry[] = [];

  public constructor(tag: string) {
    this.tagName = tag.toUpperCase();
  }

  public get textContent(): string {
    if (this.children.length === 0) {
      if (this.rawInnerHtml.length > 0) {
        return stripVisibleHtmlTags(this.rawInnerHtml);
      }
      return this.rawText;
    }
    return (
      this.rawText + this.children.map((child) => child.textContent ?? "").join("")
    );
  }

  public set textContent(val: string | null) {
    this.children.length = 0;
    this.rawInnerHtml = "";
    this.rawText = val ?? "";
  }

  public get innerHTML(): string {
    if (this.rawInnerHtml.length > 0 && this.children.length === 0) {
      return this.rawInnerHtml;
    }
    return (
      this.rawText +
      this.children
        .map((c) => c.outerHTML)
        .join("")
    );
  }

  public set innerHTML(val: string) {
    this.children.length = 0;
    this.rawText = "";
    this.rawInnerHtml = val;
  }

  public get outerHTML(): string {
    const attrs = Array.from(this.attributes.entries())
      .map(([k, v]) => `${k}="${v}"`)
      .join(" ");
    const cls = this.className ? ` class="${this.className}"` : "";
    const open = `<${this.tagName.toLowerCase()}${cls}${attrs ? " " + attrs : ""}>`;
    return `${open}${this.innerHTML}</${this.tagName.toLowerCase()}>`;
  }

  public setAttribute(name: string, value: string): void {
    this.attributes.set(name, value);
    if (name === "class") {
      this.className = value;
    } else if (name === "id") {
      this.id = value;
    } else if (name === "hidden") {
      this.hidden = true;
    } else if (name.startsWith("data-")) {
      const camel = name
        .slice(5)
        .replace(/-([a-z])/g, (_, ch: string) => ch.toUpperCase());
      this.dataset[camel] = value;
    }
  }

  public getAttribute(name: string): string | null {
    if (name === "class") {
      return this.className || null;
    }
    return this.attributes.get(name) ?? null;
  }

  public hasAttribute(name: string): boolean {
    if (name === "hidden") {
      return this.hidden || this.attributes.has("hidden");
    }
    return this.attributes.has(name);
  }

  public removeAttribute(name: string): void {
    this.attributes.delete(name);
    if (name === "hidden") {
      this.hidden = false;
    }
  }

  public appendChild<T extends MockElement>(child: T): T {
    child.parentElement = this;
    this.children.push(child);
    return child;
  }

  public append(...nodes: Array<MockElement | string>): void {
    for (const n of nodes) {
      if (typeof n === "string") {
        const span = new MockElement("span");
        span.textContent = n;
        this.appendChild(span);
      } else {
        this.appendChild(n);
      }
    }
  }

  public replaceChildren(...nodes: MockElement[]): void {
    this.children.length = 0;
    this.rawText = "";
    this.rawInnerHtml = "";
    for (const n of nodes) {
      this.appendChild(n);
    }
  }

  public addEventListener(
    type: string,
    handler: (ev: Record<string, unknown>) => void,
  ): void {
    this.listeners.push({ type, handler });
  }

  public click(): void {
    this.dispatchEvent({ type: "click", target: this, stopPropagation: () => undefined });
  }

  public querySelectorAll(): MockElement[] {
    return [];
  }

  public dispatchEvent(ev: Record<string, unknown>): boolean {
    const type = String(ev.type ?? "");
    for (const l of this.listeners) {
      if (l.type === type) {
        l.handler(ev);
      }
    }
    return true;
  }

  public findByAttr(attrName: string, attrVal?: string): MockElement | null {
    const v = this.getAttribute(attrName);
    if (v !== null && (attrVal === undefined || v === attrVal)) {
      return this;
    }
    for (const child of this.children) {
      const found = child.findByAttr(attrName, attrVal);
      if (found) {
        return found;
      }
    }
    return null;
  }

  public findAllByTag(tag: string): MockElement[] {
    const target = tag.toUpperCase();
    const out: MockElement[] = [];
    if (this.tagName === target) {
      out.push(this);
    }
    for (const child of this.children) {
      out.push(...child.findAllByTag(tag));
    }
    return out;
  }
}

function stripVisibleHtmlTags(html: string): string {
  const withoutHidden = html.replace(
    /<([a-z0-9-]+)\b[^>]*\bhidden\b[^>]*>[\s\S]*?<\/\1>/gi,
    " ",
  );
  return withoutHidden
    .replace(/<style[\s\S]*?<\/style>/gi, " ")
    .replace(/<script[\s\S]*?<\/script>/gi, " ")
    .replace(/<[^>]+>/g, " ")
    .replace(/&middot;/g, "·")
    .replace(/&amp;/g, "&")
    .replace(/&lt;/g, "<")
    .replace(/&gt;/g, ">")
    .replace(/&quot;/g, '"')
    .replace(/&#39;/g, "'")
    .replace(/\s+/g, " ")
    .trim();
}

function createMockDocument(): {
  doc: Document;
  createHost: () => HTMLElement;
} {
  const docObj = {
    documentElement: new MockElement("html"),
    createElement(tag: string): MockElement {
      return new MockElement(tag);
    },
    createElementNS(_ns: string, tag: string): MockElement {
      return new MockElement(tag);
    },
  };
  return {
    doc: docObj as unknown as Document,
    createHost: () => new MockElement("div") as unknown as HTMLElement,
  };
}

function sampleStoreEntry(): StoreCatalogEntryDto {
  return {
    packageIdHex: "a1".repeat(16),
    packageVersion: 1_099_511_627_776,
    applicationIdsHex: ["33".repeat(16)],
    publisherIdHex: "41".repeat(16),
    publisherTier: "CERTIFIED_PUBLISHER",
    track: "OFFICIAL",
    channel: "STABLE",
    activeRecallSeverity: null,
    componentCount: 3,
    permissionCount: 2,
    anyNetworkEgress: false,
    anyIrreversibleEffect: false,
    publisherKeyFingerprintHex: "9f86d081884c7d65",
    submittedAtMs: 2_900,
  };
}

function sampleStoreSnapshot(): StoreCatalogSnapshotDto {
  return {
    source: STORE_SOURCE_LIVE_REGISTRY,
    offlineNoticeZh: null,
    offlineNoticeEn: null,
    storeRevocationEpoch: 1,
    officialStableReleases: [sampleStoreEntry()],
    communityCandidates: [],
  };
}

function sampleTransparency(): StoreReleaseTransparencyDto {
  return {
    packageIdHex: "a1".repeat(16),
    packageVersion: 1_099_511_627_776,
    applicationIdsHex: ["33".repeat(16)],
    publisherIdHex: "41".repeat(16),
    publisherTier: "CERTIFIED_PUBLISHER",
    track: "OFFICIAL",
    channel: "STABLE",
    permissions: [
      {
        capabilityScope: "sabi.artifact.read",
        humanPurpose: "读取用户笔记",
        effectClass: "READ_ONLY",
        networkEgressDeclared: false,
        sandboxProfile: "wasm-strict-v1",
      },
    ],
    anyNetworkEgress: false,
    anyIrreversibleEffect: false,
    effectLegend: [
      { effectClass: "READ_ONLY", labelZh: "只读", labelEn: "Read-only" },
    ],
    trustChain: {
      publisherIdHex: "41".repeat(16),
      trustTier: "CERTIFIED_PUBLISHER",
      tierLabelZh: "已认证发布方",
      tierLabelEn: "Certified publisher",
      chainStatus: STORE_CHAIN_STATUS_VERIFIED,
      chainStatusNoticeZh: "证书链与发布签名已在本机复验通过。",
      chainStatusNoticeEn:
        "Certificate chain and publisher signature re-verified on this machine.",
      leafKeyFingerprintHex: "9f86d081884c7d65",
      revocationEpoch: 1,
      crlFreshUntilMs: 8_000_000_000_000,
      signatureReverified: true,
    },
  };
}

function sampleDiff(): StorePermissionDiffDto {
  return {
    packageIdHex: "a1".repeat(16),
    oldVersion: 1_099_511_627_776,
    newVersion: 1_100_069_968_384,
    addedCapabilities: [],
    removedCapabilityScopes: [],
    newlyEnabledNetworkEgress: [],
    escalatedEffects: [],
    escalatedToIrreversible: [],
    purposeChanges: [],
    sandboxProfileChanges: [],
    requiresZ4Reconsent: false,
    plainSummaryZh: "权限未变。",
    plainSummaryEn: "Permissions unchanged.",
  };
}

const L0_L1_BANNED_JARGON: readonly string[] = [
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
  "receipt_hex",
  "control_command_id",
  "CAS_WINNER",
  "SHARED_THREAD_LIKE",
  "LOCAL_SINGLE_NODE",
];

const PICTOGRAPHIC_RE = /\p{Extended_Pictographic}/u;

let passedAssertions = 0;
function assert(condition: boolean, message: string): void {
  if (!condition) {
    throw new Error(`Assertion failed: ${message}`);
  }
  passedAssertions += 1;
}

interface NodeFsPromises {
  readFile(path: string, encoding: "utf8"): Promise<string>;
}
interface NodePath {
  dirname(p: string): string;
  resolve(...parts: string[]): string;
}
interface NodeUrl {
  fileURLToPath(u: string): string;
}

async function importBuiltin<T>(specifier: string): Promise<T> {
  return (await (Function("s", "return import(s)") as (s: string) => Promise<T>)(
    specifier,
  ));
}

async function main(): Promise<void> {
  // 1. Default Z2 3-pane workstation mounts real 12-Domain Control Center & Workbench surfaces,
  //    while third-party apps still use sandboxed <iframe>.
  {
    const { doc, createHost } = createMockDocument();
    const host = createHost();
    const compositor = createWorkstationCompositor({
      doc,
      viewportWidth: 1440,
      viewportHeight: 900,
      initialLocale: "zh-CN",
      initialDisclosure: "L1",
      initialLayoutMode: "three-pane-workstation",
      installedApplications: [
        {
          applicationIdHex: "aa".repeat(16),
          installationIdHex: "bb".repeat(16),
          displayName: "第三方沙箱表格",
          status: "ACTIVE",
          installationGeneration: 1,
        },
      ],
    });

    const controlCenter = createSystemControlCenterController();
    const taskSpace = createTaskSpaceSurfaceController({
      disclosureModel: compositor.disclosureModel,
      locale: "zh-CN",
    });
    const notifications = new NotificationCenterController();
    const spotlight = new SpotlightController();
    const clusterPosture = new ClusterPostureController();
    const settingsSuite = createWorkstationSettingsSuiteController();
    const storeCatalog = createStoreCatalogController({
      client: {
        loadSnapshot: async () => sampleStoreSnapshot(),
        loadTransparency: async () => sampleTransparency(),
        loadPermissionDiff: async () => sampleDiff(),
        prepareSensitiveAction: async () => {
          throw new Error("not invoked in mount smoke");
        },
      },
      initialLocale: "zh-CN",
      initialDisclosure: "L1",
    });
    await storeCatalog.refresh();

    const shellMount = mountIndustrialWorkstationSurfaces({
      doc,
      compositor,
      controlCenterController: controlCenter,
      taskSpaceController: taskSpace,
      storeCatalogController: storeCatalog,
      notificationController: notifications,
      spotlightController: spotlight,
      clusterPostureController: clusterPosture,
      settingsSuiteController: settingsSuite,
    });

    compositor.mount(host, { doc });
    const hostMock = host as unknown as MockElement;
    const snap = compositor.getSnapshot();

    assert(
      snap.wm.windows.length === 2,
      "Startup mounts 2 default industrial workstation tiling windows in Z2",
    );
    assert(
      hostMock.innerHTML.includes("nlos-control-workstation"),
      "Upper-right Z2 pane renders real 12-Domain Control & Resource Center HTML",
    );
    assert(
      hostMock.innerHTML.includes("nlos-workstation-workbench"),
      "Lower-right Z2 pane renders real multi-tab industrial workbench (DAG / Store / Settings / Search)",
    );

    // Switch workbench tabs and verify Task DAG, Store Catalog, Settings, and Spotlight render
    shellMount.selectWorkbenchTab("store");
    assert(
      hostMock.innerHTML.includes("nlos-store"),
      "Workbench 'store' tab renders real StoreCatalogView",
    );

    shellMount.selectWorkbenchTab("settings");
    assert(
      hostMock.innerHTML.includes("nlos-settings-workstation"),
      "Workbench 'settings' tab renders real WorkstationSettingsSuite",
    );

    shellMount.selectWorkbenchTab("dag");
    assert(
      hostMock.innerHTML.includes("nlos-dag") ||
        hostMock.innerHTML.includes("nlos-task-space"),
      "Workbench 'dag' tab renders real Task Space DAG & Attempt Lanes surface",
    );

    // Launching an untrusted third-party application MUST still produce a sandboxed <iframe>!
    const launched = compositor.launchApplication({
      applicationIdHex: "aa".repeat(16),
      installationIdHex: "bb".repeat(16),
    });
    assert(launched.ok, "Third-party app launch succeeds");
    const launchedFrame = hostMock.findByAttr(
      "data-window-id",
      launched.windowRecord!.windowId,
    );
    assert(
      launchedFrame !== null && launchedFrame.findAllByTag("iframe").length === 1,
      "Untrusted third-party window still enforces sandboxed <iframe> isolation",
    );
  }

  // 2. Z3 Taskbar Tray Buttons (Connectivity / Notifications / Clock Control Center) & Ctrl+K Spotlight
  {
    const { doc, createHost } = createMockDocument();
    const host = createHost();
    const compositor = createWorkstationCompositor({
      doc,
      viewportWidth: 1440,
      viewportHeight: 900,
      initialLocale: "zh-CN",
      initialDisclosure: "L1",
    });

    mountIndustrialWorkstationSurfaces({
      doc,
      compositor,
      controlCenterController: createSystemControlCenterController(),
      taskSpaceController: createTaskSpaceSurfaceController({
        disclosureModel: compositor.disclosureModel,
        locale: "zh-CN",
      }),
      storeCatalogController: createStoreCatalogController({
        client: {
          loadSnapshot: async () => sampleStoreSnapshot(),
          loadTransparency: async () => sampleTransparency(),
          loadPermissionDiff: async () => sampleDiff(),
          prepareSensitiveAction: async () => {
            throw new Error("unused");
          },
        },
        initialLocale: "zh-CN",
        initialDisclosure: "L1",
      }),
      notificationController: new NotificationCenterController(),
      spotlightController: new SpotlightController(),
      clusterPostureController: new ClusterPostureController(),
      settingsSuiteController: createWorkstationSettingsSuiteController(),
    });

    compositor.mount(host, { doc });
    const hostMock = host as unknown as MockElement;

    // Click bottom taskbar connectivity button -> mounts ClusterPostureTray in Z3
    const connBtn = hostMock.findByAttr("data-taskbar-action", "open-connectivity");
    assert(connBtn !== null, "Taskbar renders connectivity posture button");
    connBtn!.click();
    assert(
      compositor.getSnapshot().openZ3FlyoutIds.includes("CONNECTIVITY"),
      "Clicking connectivity button opens CONNECTIVITY Z3 flyout",
    );
    const z3SlotAfterConn = hostMock.findByAttr("data-nlos-slot", "z3-system-flyouts");
    assert(
      z3SlotAfterConn !== null &&
        !z3SlotAfterConn.hidden &&
        z3SlotAfterConn.innerHTML.includes("nlos-cluster"),
      "CONNECTIVITY flyout mounts real ClusterPostureTray DOM",
    );

    // Click bottom taskbar notifications button -> switches Z3 flyout to NotificationCenter
    const notifyBtn = hostMock.findByAttr("data-taskbar-action", "open-notifications");
    assert(notifyBtn !== null, "Taskbar renders notifications button");
    notifyBtn!.click();
    assert(
      compositor.getSnapshot().openZ3FlyoutIds.includes("NOTIFICATIONS") &&
        !compositor.getSnapshot().openZ3FlyoutIds.includes("CONNECTIVITY"),
      "Clicking notifications button switches Z3 flyout to NOTIFICATIONS",
    );
    const z3SlotAfterNotify = hostMock.findByAttr("data-nlos-slot", "z3-system-flyouts");
    assert(
      z3SlotAfterNotify !== null &&
        !z3SlotAfterNotify.hidden &&
        (z3SlotAfterNotify.innerHTML.includes("nlos-notification") ||
          z3SlotAfterNotify.innerHTML.includes("nlos-notify")),
      "NOTIFICATIONS flyout mounts real NotificationCenter DOM",
    );

    // Click bottom taskbar clock -> switches Z3 flyout to SystemControlCenterFlyout
    const clockBtn = hostMock.findByAttr("data-taskbar-action", "open-control-center");
    assert(clockBtn !== null, "Taskbar renders clock / control-center button");
    clockBtn!.click();
    assert(
      compositor.getSnapshot().openZ3FlyoutIds.includes("CONTROL_CENTER"),
      "Clicking clock button opens CONTROL_CENTER Z3 flyout",
    );
    const z3SlotAfterClock = hostMock.findByAttr("data-nlos-slot", "z3-system-flyouts");
    assert(
      z3SlotAfterClock !== null &&
        !z3SlotAfterClock.hidden &&
        z3SlotAfterClock.innerHTML.includes("nlos-control-center-flyout"),
      "CONTROL_CENTER flyout mounts real SystemControlCenterFlyout DOM",
    );

    // Ctrl+K opens SPOTLIGHT_SEARCH flyout
    compositor.handleKeydown({
      key: "k",
      ctrlKey: true,
    });
    assert(
      compositor.getSnapshot().openZ3FlyoutIds.includes("SPOTLIGHT_SEARCH"),
      "Ctrl+K opens SPOTLIGHT_SEARCH Z3 flyout",
    );
    const z3SlotAfterSearch = hostMock.findByAttr("data-nlos-slot", "z3-system-flyouts");
    assert(
      z3SlotAfterSearch !== null &&
        !z3SlotAfterSearch.hidden &&
        z3SlotAfterSearch.innerHTML.includes("nlos-spotlight"),
      "SPOTLIGHT_SEARCH flyout mounts real SpotlightView DOM",
    );
  }

  // 3. ANTI-AI-05/07 Zero-Jargon & Zero-Emoji Gate across L0/L1 with all industrial surfaces mounted
  {
    const locales: readonly WorkstationLocale[] = ["zh-CN", "en-US"];
    const levels: readonly DisclosureLevel[] = ["L0", "L1"];

    for (const locale of locales) {
      for (const level of levels) {
        const { doc, createHost } = createMockDocument();
        const host = createHost();
        const comp = createWorkstationCompositor({
          doc,
          initialLocale: locale,
          initialDisclosure: level,
        });
        mountIndustrialWorkstationSurfaces({
          doc,
          compositor: comp,
          controlCenterController: createSystemControlCenterController(),
          taskSpaceController: createTaskSpaceSurfaceController({
            disclosureModel: comp.disclosureModel,
            locale,
          }),
          storeCatalogController: createStoreCatalogController({
            client: {
              loadSnapshot: async () => sampleStoreSnapshot(),
              loadTransparency: async () => sampleTransparency(),
              loadPermissionDiff: async () => sampleDiff(),
              prepareSensitiveAction: async () => {
                throw new Error("unused");
              },
            },
            initialLocale: locale,
            initialDisclosure: level,
          }),
          notificationController: new NotificationCenterController(),
          spotlightController: new SpotlightController(),
          clusterPostureController: new ClusterPostureController(),
          settingsSuiteController: createWorkstationSettingsSuiteController(),
        });

        comp.mount(host, { doc });
        const visibleText = comp.getVisibleTextForAudit();
        for (const banned of L0_L1_BANNED_JARGON) {
          assert(
            !visibleText.includes(banned),
            `ANTI-AI-07 violation (${locale}, ${level}): leaked jargon '${banned}'`,
          );
        }
        assert(
          !PICTOGRAPHIC_RE.test(visibleText),
          `ANTI-AI-05 violation (${locale}, ${level}): leaked emoji in mounted workstation surfaces`,
        );
        comp.unmount();
      }
    }
  }

  // 4. Static audit: `desktop/src/main.ts` invokes `mountIndustrialWorkstationSurfaces`
  {
    const fs = await importBuiltin<NodeFsPromises>("node:fs/promises");
    const path = await importBuiltin<NodePath>("node:path");
    const url = await importBuiltin<NodeUrl>("node:url");
    const here = path.dirname(url.fileURLToPath(import.meta.url));
    const mainTs = await fs.readFile(path.resolve(here, "../../main.ts"), "utf8");
    assert(
      mainTs.includes("mountIndustrialWorkstationSurfaces"),
      "desktop/src/main.ts must call mountIndustrialWorkstationSurfaces",
    );
    assert(
      !mainTs.includes("void settingsSuiteController.settingsCas;"),
      "desktop/src/main.ts must remove dead no-op controller stubs",
    );
  }

  console.log(
    `PASS: Workstation shell industrial surfaces mount suite (${passedAssertions} assertions).`,
  );
}

void main();
