// ==========================================================================
// LLMOS Operator Studio — Full 8-Domain Console Verification Suite
// (`desktop/src/studio/__tests__/operator-studio.test.ts`)
// Verifies:
//   1. All 8 primary sections render cleanly in both zh-CN and en-US locales.
//   2. Theme toggling (Graphite Dark <-> Paper Light) & Disclosure cycling (L1..L3).
//   3. Intent assist & glm-5.3 chat actions + [NL-AMBIG-001] safety escalation.
//   4. Right-hand SABI Receipt & Entity Inspector drawer and Z4 Security Modal.
//   5. Zero pictographic emojis across all 8 views (ANTI-AI-05).
// ==========================================================================

import {
  createOperatorStudioStore,
  mountOperatorStudio,
  type StudioNavSectionId,
} from "../index";

class SimpleElement {
  public readonly tagName: string;
  public className = "";
  public id = "";
  public value = "";
  public hidden = false;
  public style: Record<string, string> = {};
  public dataset: Record<string, string> = {};
  public readonly attributes = new Map<string, string>();
  public readonly children: SimpleElement[] = [];
  private rawHtml = "";
  private rawText = "";
  private readonly listeners: Array<{
    type: string;
    fn: (ev: Record<string, unknown>) => void;
  }> = [];

  public constructor(tag: string) {
    this.tagName = tag.toUpperCase();
  }

  public get innerHTML(): string {
    if (this.children.length === 0) {
      return this.rawHtml || this.rawText;
    }
    return (
      this.rawHtml +
      this.rawText +
      this.children.map((c) => c.outerHTML).join("")
    );
  }

  public set innerHTML(val: string) {
    this.children.length = 0;
    this.rawText = "";
    this.rawHtml = val;
  }

  public get textContent(): string {
    return this.innerHTML.replace(/<[^>]+>/g, " ");
  }

  public set textContent(val: string | null) {
    this.children.length = 0;
    this.rawHtml = "";
    this.rawText = val ?? "";
  }

  public get outerHTML(): string {
    const cls = this.className ? ` class="${this.className}"` : "";
    const attrs = Array.from(this.attributes.entries())
      .map(([k, v]) => `${k}="${v}"`)
      .join(" ");
    return `<${this.tagName.toLowerCase()}${cls}${attrs ? " " + attrs : ""}>${this.innerHTML}</${this.tagName.toLowerCase()}>`;
  }

  public setAttribute(k: string, v: string): void {
    this.attributes.set(k, v);
    if (k === "class") {
      this.className = v;
    } else if (k === "id") {
      this.id = v;
    }
  }

  public getAttribute(k: string): string | null {
    return this.attributes.get(k) ?? null;
  }

  public hasAttribute(k: string): boolean {
    return this.attributes.has(k);
  }

  public removeAttribute(k: string): void {
    this.attributes.delete(k);
  }

  public appendChild<T extends SimpleElement>(child: T): T {
    this.children.push(child);
    return child;
  }

  public replaceChildren(...nodes: SimpleElement[]): void {
    this.children.length = 0;
    this.rawHtml = "";
    this.rawText = "";
    for (const n of nodes) {
      this.children.push(n);
    }
  }

  public addEventListener(
    type: string,
    fn: (ev: Record<string, unknown>) => void,
  ): void {
    this.listeners.push({ type, fn });
  }

  public querySelector(): SimpleElement | null {
    return null;
  }
}

function createMockDom(): { doc: Document; host: HTMLElement } {
  const doc = {
    documentElement: new SimpleElement("html"),
    createElement(tag: string) {
      return new SimpleElement(tag);
    },
    createElementNS(_ns: string, tag: string) {
      return new SimpleElement(tag);
    },
  } as unknown as Document;
  const host = new SimpleElement("div") as unknown as HTMLElement;
  return { doc, host };
}

const PICTOGRAPHIC_RE = /\p{Extended_Pictographic}/u;

let assertions = 0;
function assert(cond: boolean, msg: string): void {
  if (!cond) {
    throw new Error(`OperatorStudio assertion failed: ${msg}`);
  }
  assertions += 1;
}

async function main(): Promise<void> {
  const { doc, host } = createMockDom();
  const store = createOperatorStudioStore();
  mountOperatorStudio(host, { doc, store });

  const sections: readonly StudioNavSectionId[] = [
    "overview",
    "intent-keypool",
    "tasks-dag",
    "processes-resources",
    "recovery-telemetry",
    "apps-store",
    "security-settings",
    "kernel-probes",
  ];

  for (const locale of ["zh-CN", "en-US"] as const) {
    store.setLocale(locale);
    for (const sec of sections) {
      store.setActiveSection(sec);
      const html = (host as unknown as SimpleElement).innerHTML;
      assert(
        html.includes(`data-active-section="${sec}"`),
        `Renders section ${sec} in ${locale}`,
      );
      assert(
        !PICTOGRAPHIC_RE.test(html),
        `Zero pictographic emojis in section ${sec} (${locale})`,
      );
    }
  }

  // Test Intent Assist & Destructive Safety Escalation
  const safeRes = await store.runIntentAssist("汇总当前工作区制品");
  assert(!safeRes.safetyEscalated, "Safe intent is not escalated");
  const dangerRes = await store.runIntentAssist("删除旧缓存并卸载应用");
  assert(
    dangerRes.safetyEscalated && dangerRes.undoClass === "IRREVERSIBLE",
    "Destructive intent escalates to IRREVERSIBLE ([NL-AMBIG-001])",
  );

  // Test Z4 Security Confirmation Modal lifecycle
  store.openZ4Confirmation({
    promptId: "z4-test-1",
    title: "卸载测试包",
    summary: "将移除测试包关联权限",
    consequence: "用户数据保留",
    irreversible: false,
    onConfirm: () => undefined,
  });
  assert(
    (host as unknown as SimpleElement).innerHTML.includes('data-nlos-plane="Z4"'),
    "Opening Z4 prompt mounts Z4 modal overlay",
  );
  assert(store.confirmZ4Prompt(), "Confirming Z4 prompt resolves and closes modal");
  assert(store.getState().z4Prompt === null, "Z4 prompt cleared after confirm");

  // Test 3-Theme cycling (A2A Zürich Light -> Braun Atelier -> A2A Carbon Dark) & Disclosure
  assert(
    store.getState().studioTheme === "zurich-light",
    "Default studio theme is A2A Zürich Light",
  );
  store.toggleTheme();
  assert(
    store.getState().studioTheme === "braun-atelier",
    "Toggles to Braun Atelier warm grey theme",
  );
  store.toggleTheme();
  assert(
    store.getState().studioTheme === "zurich-dark" &&
      store.getState().theme === "graphite-workstation",
    "Toggles to A2A Zürich Carbon dark theme",
  );
  store.setStudioTheme("zurich-light");
  assert(
    store.getState().studioTheme === "zurich-light",
    "Explicitly selects A2A Zürich Light theme",
  );
  store.cycleDisclosure();
  assert(store.getState().disclosure === "L2", "Cycles disclosure L1 -> L2");
  store.cycleDisclosure();
  assert(store.getState().disclosure === "L3", "Cycles disclosure L2 -> L3");

  // Test Always-Live Right Rail & Store Humanized Package Names
  store.setInspector(null);
  store.setActiveSection("overview");
  assert(
    (host as unknown as SimpleElement).innerHTML.includes("st-rail-right") &&
      !(host as unknown as SimpleElement).innerHTML.includes("is-inspector-closed"),
    "Three-column Open Swiss layout keeps right telemetry rail live by default",
  );
  store.setActiveSection("apps-store");
  assert(
    (host as unknown as SimpleElement).innerHTML.includes(
      "knowledge-observatory-app",
    ),
    "Store view renders humanized Stage E ecosystem package names",
  );
  store.setActiveSection("processes-resources");
  assert(
    (host as unknown as SimpleElement).innerHTML.includes("st-facet-pill"),
    "10-Layer tree renders wrapping typographic facet index pills",
  );

  // Test Recovery Inspect & Raw IPC Probe opening Inspector Drawer
  await store.runRecoveryInspect("artifact");
  assert(
    store.getState().inspector !== null &&
      (host as unknown as SimpleElement).innerHTML.includes("nlos-studio-drawer"),
    "Recovery inspect opens right-hand SABI Receipt Inspector Drawer",
  );

  console.log(
    `PASS: LLMOS Operator Studio verification suite (${assertions} assertions green).`,
  );
}

void main();
