// Persistent 36px Bottom Compact Workstation Taskbar (Z3 Plane, L0 Ambient Posture).
// Authority: ADR-0025 Decision 1 & 3 + Stage D Desktop Visual & UX Spec §2.2, §3.3, §3.5.
// Strict discipline:
//   - 36px height, 0px outer radius, 100% solid --nlos-bg-chrome surface
//   - 26px command trigger slot (locked when z4ModalLock === true)
//   - Flat window tabs + Task Space tab with 2px solid --nlos-accent-active bottom bar
//   - Zero internal jargon in L0/L1/L2 (LOCAL_SINGLE_NODE strictly gated to disclosure === "L3")
//   - Zero pictographic characters / emojis (100% geometric SVG strokes)

import {
  createIconElement,
  renderIconSvg,
  type DisclosureLevel,
  type IconName,
  type WorkstationLocale,
} from "../ui";
import { WM_LAYOUT_MODES, type WmLayoutMode } from "../wm/layout-solver";

const SVG_NS = "http://www.w3.org/2000/svg";

export type WorkstationThemeId =
  | "graphite-workstation"
  | "paper-workstation"
  | "system";

export type ConnectivityPostureKind =
  | "local-ready"
  | "offline-local-ready"
  | "cluster-online"
  | "cluster-degraded";

export interface TaskbarConnectivityState {
  readonly kind: ConnectivityPostureKind;
  readonly onlineNodes?: number;
  readonly totalNodes?: number;
  /** Optional diagnostic topology badge rendered strictly when disclosure === "L3". */
  readonly l3TopologyTag?: string;
}

export interface DeviceHardwareCaptureState {
  readonly microphoneActive?: boolean;
  readonly cameraActive?: boolean;
  readonly screenActive?: boolean;
}

export interface TaskbarWindowTabItem {
  readonly windowId: string;
  readonly title: string;
  readonly icon?: IconName;
  readonly focused?: boolean;
  readonly minimized?: boolean;
}

export interface BottomTaskbarCallbacks {
  readonly onToggleLauncher?: () => void;
  readonly onOpenCommandPalette?: () => void;
  readonly onActivateWindowTab?: (windowId: string) => void;
  readonly onToggleTaskSpace?: () => void;
  readonly onSelectLayoutMode?: (nextMode: WmLayoutMode) => void;
  readonly onOpenConnectivityFlyout?: () => void;
  readonly onCycleDisclosure?: (nextDisclosure: DisclosureLevel) => void;
  readonly onToggleTheme?: (nextTheme: "graphite-workstation" | "paper-workstation") => void;
  readonly onOpenNotifications?: () => void;
  readonly onOpenControlCenter?: () => void;
}

export interface BottomTaskbarOptions {
  readonly windows?: readonly TaskbarWindowTabItem[];
  readonly activeTaskCount?: number;
  readonly pendingApprovalCount?: number;
  readonly taskSpaceFocused?: boolean;
  readonly launcherOpen?: boolean;
  readonly z4ModalLock?: boolean;
  readonly layoutMode?: WmLayoutMode;
  readonly connectivity?: TaskbarConnectivityState;
  readonly disclosure?: DisclosureLevel;
  readonly theme?: WorkstationThemeId;
  readonly hardwareCapture?: DeviceHardwareCaptureState;
  readonly unreadNotificationCount?: number;
  readonly clockText?: string;
  readonly locale?: WorkstationLocale;
  readonly callbacks?: BottomTaskbarCallbacks;
  readonly doc?: Document;
}

interface TaskbarCopy {
  readonly launcherBtn: string;
  readonly cmdPromptPlaceholder: string;
  readonly cmdLockedLabel: string;
  readonly tasksTabPrefix: string;
  readonly layoutLabels: Readonly<Record<WmLayoutMode, string>>;
  readonly layoutAria: string;
  readonly localReady: string;
  readonly offlineLocalReady: string;
  readonly clusterOnline: (online: number, total: number) => string;
  readonly clusterDegraded: (online: number, total: number) => string;
  readonly disclosurePrefix: string;
  readonly disclosureTooltip: string;
  readonly themeDark: string;
  readonly themeLight: string;
  readonly themeAria: string;
  readonly micCapture: string;
  readonly camCapture: string;
  readonly screenCapture: string;
  readonly notificationsLabel: string;
  readonly clockAria: string;
}

const TASKBAR_COPY: Readonly<Record<WorkstationLocale, TaskbarCopy>> = {
  "zh-CN": {
    launcherBtn: "应用",
    cmdPromptPlaceholder: "输入指令或自然语言目标...",
    cmdLockedLabel: "安全确认进行中 — 应用窗口输入已锁定",
    tasksTabPrefix: "任务",
    layoutLabels: {
      "workspace-maximize": "单窗",
      "two-column-split": "双栏",
      "three-pane-workstation": "三栏",
      quadrants: "四象限",
      floating: "浮动",
    },
    layoutAria: "切换平铺窗口布局",
    localReady: "本地就绪",
    offlineLocalReady: "离线 · 本地可用",
    clusterOnline: (online, total) => `集群 ${online}/${total} 在线`,
    clusterDegraded: (online, total) => `集群 ${online}/${total}（部分离线）`,
    disclosurePrefix: "披露",
    disclosureTooltip: "切换信息披露层级 (Ctrl+Shift+E)",
    themeDark: "深色",
    themeLight: "浅色",
    themeAria: "切换工作站深浅主题",
    micCapture: "麦克风采集中",
    camCapture: "摄像头采集中",
    screenCapture: "屏幕采集中",
    notificationsLabel: "通知",
    clockAria: "打开系统控制中心与时钟面板",
  },
  "en-US": {
    launcherBtn: "Apps",
    cmdPromptPlaceholder: "Enter command or goal...",
    cmdLockedLabel: "Security prompt active — app input locked",
    tasksTabPrefix: "Tasks",
    layoutLabels: {
      "workspace-maximize": "Single",
      "two-column-split": "2-Col",
      "three-pane-workstation": "3-Pane",
      quadrants: "Quad",
      floating: "Float",
    },
    layoutAria: "Switch tiling window layout",
    localReady: "Local Ready",
    offlineLocalReady: "Offline · Local Ready",
    clusterOnline: (online, total) => `Cluster ${online}/${total} Online`,
    clusterDegraded: (online, total) => `Cluster ${online}/${total} (Partial Offline)`,
    disclosurePrefix: "LOD",
    disclosureTooltip: "Switch progressive disclosure level (Ctrl+Shift+E)",
    themeDark: "Dark",
    themeLight: "Light",
    themeAria: "Toggle workstation theme",
    micCapture: "Mic Active",
    camCapture: "Cam Active",
    screenCapture: "Screen Active",
    notificationsLabel: "Alerts",
    clockAria: "Open system control center and clock",
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

export function formatTaskbarClock(source?: Date | string): string {
  if (typeof source === "string" && /^\d{2}:\d{2}$/.test(source.trim())) {
    return source.trim();
  }
  const d = source instanceof Date ? source : new Date();
  const hh = String(d.getHours()).padStart(2, "0");
  const mm = String(d.getMinutes()).padStart(2, "0");
  return `${hh}:${mm}`;
}

export function iconForLayoutMode(mode: WmLayoutMode): IconName {
  switch (mode) {
    case "workspace-maximize":
      return "layout-1-pane";
    case "two-column-split":
      return "layout-2-col";
    case "three-pane-workstation":
      return "layout-3-pane";
    case "quadrants":
      return "layout-quadrant";
    case "floating":
      return "layout-float";
  }
}

export function iconForDisclosureLevel(level: DisclosureLevel): IconName {
  switch (level) {
    case "L0":
      return "disclosure-l0";
    case "L1":
      return "disclosure-l1";
    case "L2":
      return "disclosure-l2";
    case "L3":
      return "disclosure-l3";
  }
}

function nextLayoutMode(current: WmLayoutMode): WmLayoutMode {
  const idx = WM_LAYOUT_MODES.indexOf(current);
  if (idx < 0) {
    return "three-pane-workstation";
  }
  return WM_LAYOUT_MODES[(idx + 1) % WM_LAYOUT_MODES.length]!;
}

function nextDisclosureLevel(current: DisclosureLevel): DisclosureLevel {
  switch (current) {
    case "L0":
    case "L1":
      return "L2";
    case "L2":
      return "L3";
    case "L3":
      return "L1";
  }
}

function resolveConnectivityCopy(
  connectivity: TaskbarConnectivityState,
  copy: TaskbarCopy,
): {
  label: string;
  l3Tag: string;
  glyph: "solid-dot" | "dashed-ring" | "solid-diamond";
  amber: boolean;
} {
  const online = Math.max(0, Math.floor(connectivity.onlineNodes ?? 1));
  const total = Math.max(1, Math.floor(connectivity.totalNodes ?? 1));

  switch (connectivity.kind) {
    case "local-ready":
      return {
        label: copy.localReady,
        l3Tag: connectivity.l3TopologyTag ?? "LOCAL_SINGLE_NODE",
        glyph: "solid-dot",
        amber: false,
      };
    case "offline-local-ready":
      return {
        label: copy.offlineLocalReady,
        l3Tag: connectivity.l3TopologyTag ?? "LOCAL_SINGLE_NODE",
        glyph: "dashed-ring",
        amber: false,
      };
    case "cluster-online":
      return {
        label: copy.clusterOnline(online, total),
        l3Tag: connectivity.l3TopologyTag ?? `CLUSTER_${online}_OF_${total}`,
        glyph: "solid-dot",
        amber: false,
      };
    case "cluster-degraded":
      return {
        label: copy.clusterDegraded(online, total),
        l3Tag: connectivity.l3TopologyTag ?? `CLUSTER_DEGRADED_${online}_OF_${total}`,
        glyph: "solid-diamond",
        amber: true,
      };
  }
}

function renderMiniGlyphHtml(
  glyph: "solid-dot" | "dashed-ring" | "solid-diamond",
  extraClass: string,
): string {
  let inner = "";
  if (glyph === "solid-dot") {
    inner = `<circle cx="3" cy="3" r="2.5" fill="currentColor" />`;
  } else if (glyph === "dashed-ring") {
    inner = `<circle cx="3" cy="3" r="2.2" fill="none" stroke="currentColor" stroke-width="1.2" stroke-dasharray="1.4 1.1" />`;
  } else {
    inner = `<polygon points="3,0.4 5.6,3 3,5.6 0.4,3" fill="currentColor" />`;
  }
  return (
    `<svg xmlns="${SVG_NS}" class="nlos-status-glyph ${extraClass}" data-glyph="${glyph}" ` +
    `width="6" height="6" viewBox="0 0 6 6" aria-hidden="true">${inner}</svg>`
  );
}

function createMiniGlyphElement(
  doc: Document,
  glyph: "solid-dot" | "dashed-ring" | "solid-diamond",
  extraClass: string,
): SVGSVGElement {
  const svg = doc.createElementNS(SVG_NS, "svg");
  svg.setAttribute("class", `nlos-status-glyph ${extraClass}`.trim());
  svg.setAttribute("data-glyph", glyph);
  svg.setAttribute("width", "6");
  svg.setAttribute("height", "6");
  svg.setAttribute("viewBox", "0 0 6 6");
  svg.setAttribute("aria-hidden", "true");

  if (glyph === "solid-dot") {
    const c = doc.createElementNS(SVG_NS, "circle");
    c.setAttribute("cx", "3");
    c.setAttribute("cy", "3");
    c.setAttribute("r", "2.5");
    c.setAttribute("fill", "currentColor");
    svg.appendChild(c);
  } else if (glyph === "dashed-ring") {
    const c = doc.createElementNS(SVG_NS, "circle");
    c.setAttribute("cx", "3");
    c.setAttribute("cy", "3");
    c.setAttribute("r", "2.2");
    c.setAttribute("fill", "none");
    c.setAttribute("stroke", "currentColor");
    c.setAttribute("stroke-width", "1.2");
    c.setAttribute("stroke-dasharray", "1.4 1.1");
    svg.appendChild(c);
  } else {
    const p = doc.createElementNS(SVG_NS, "polygon");
    p.setAttribute("points", "3,0.4 5.6,3 3,5.6 0.4,3");
    p.setAttribute("fill", "currentColor");
    svg.appendChild(p);
  }
  return svg;
}

export function renderBottomTaskbarHtml(options?: BottomTaskbarOptions): string {
  const locale: WorkstationLocale = options?.locale ?? "zh-CN";
  const disclosure: DisclosureLevel = options?.disclosure ?? "L1";
  const copy = TASKBAR_COPY[locale];

  const windows = options?.windows ?? [];
  const activeTaskCount = Math.max(0, Math.floor(options?.activeTaskCount ?? 0));
  const pendingApprovalCount = Math.max(
    0,
    Math.floor(options?.pendingApprovalCount ?? 0),
  );
  const taskSpaceFocused = Boolean(options?.taskSpaceFocused);
  const launcherOpen = Boolean(options?.launcherOpen);
  const z4Locked = Boolean(options?.z4ModalLock);
  const layoutMode: WmLayoutMode =
    options?.layoutMode ?? "three-pane-workstation";
  const connectivity: TaskbarConnectivityState = options?.connectivity ?? {
    kind: "local-ready",
  };
  const theme: WorkstationThemeId = options?.theme ?? "graphite-workstation";
  const capture = options?.hardwareCapture ?? {};
  const unreadCount = Math.max(
    0,
    Math.floor(options?.unreadNotificationCount ?? 0),
  );
  const clockStr = formatTaskbarClock(options?.clockText ?? "00:00");

  // 1. Left zone: [应用/Apps] launcher + 26px command trigger
  const launcherHtml =
    `<button type="button" class="nlos-btn nlos-taskbar-launcher-btn" ` +
    `data-taskbar-action="toggle-launcher" aria-expanded="${launcherOpen ? "true" : "false"}">` +
    `${renderIconSvg("app-launcher", { size: 14, strokeWidth: 1.5 })}` +
    `<span>${escapeHtml(copy.launcherBtn)}</span>` +
    `</button>`;

  const cmdInnerHtml = z4Locked
    ? `<span>${escapeHtml(copy.cmdLockedLabel)}</span>`
    : `<span>${escapeHtml(copy.cmdPromptPlaceholder)}</span> <kbd class="nlos-taskbar-kbd">Ctrl+Space</kbd>`;

  const cmdTriggerHtml =
    `<button type="button" class="nlos-taskbar-cmd-trigger" ` +
    `data-taskbar-action="open-command-palette"${z4Locked ? " disabled" : ""}>` +
    `${renderIconSvg("command-prompt", { size: 14, strokeWidth: 1.5 })}` +
    `${cmdInnerHtml}` +
    `</button>`;

  // 2. Center-left zone: window tabs + Task Space tab
  const windowTabsHtml = windows
    .map((tab) => {
      const tabClasses = ["nlos-taskbar-tab"];
      if (tab.focused) {
        tabClasses.push("nlos-taskbar-tab--focused");
      }
      if (tab.minimized) {
        tabClasses.push("nlos-taskbar-tab--minimized");
      }
      const activeBar = tab.focused
        ? `<span class="nlos-taskbar-tab__active-bar" aria-hidden="true"></span>`
        : "";
      return (
        `<button type="button" class="${tabClasses.join(" ")}" ` +
        `data-taskbar-action="activate-window" ` +
        `data-window-id="${escapeHtml(tab.windowId)}" ` +
        `aria-pressed="${tab.focused ? "true" : "false"}">` +
        `${renderIconSvg(tab.icon ?? "layout-1-pane", { size: 14, strokeWidth: 1.5 })}` +
        `<span class="nlos-taskbar-tab__title">${escapeHtml(tab.title)}</span>` +
        `${activeBar}` +
        `</button>`
      );
    })
    .join("");

  const taskDiamondHtml =
    pendingApprovalCount > 0
      ? renderMiniGlyphHtml("solid-diamond", "nlos-taskbar-approval-diamond")
      : "";
  const taskActiveBar = taskSpaceFocused
    ? `<span class="nlos-taskbar-tab__active-bar" aria-hidden="true"></span>`
    : "";
  const taskTabHtml =
    `<button type="button" class="nlos-taskbar-tab nlos-taskbar-tab--tasks${taskSpaceFocused ? " nlos-taskbar-tab--focused" : ""}" ` +
    `data-taskbar-action="toggle-task-space" aria-pressed="${taskSpaceFocused ? "true" : "false"}">` +
    `${taskDiamondHtml}` +
    `<span class="nlos-tabular">${escapeHtml(`${copy.tasksTabPrefix} ${activeTaskCount}`)}</span>` +
    `${taskActiveBar}` +
    `</button>`;

  // 3. Right zone: L0 zero-jargon system tray
  const layoutBtnHtml =
    `<button type="button" class="nlos-taskbar-tray-btn" ` +
    `data-taskbar-action="cycle-layout" data-layout-mode="${layoutMode}" ` +
    `aria-label="${escapeHtml(copy.layoutAria)}">` +
    `${renderIconSvg(iconForLayoutMode(layoutMode), { size: 14, strokeWidth: 1.5 })}` +
    `<span>${escapeHtml(copy.layoutLabels[layoutMode])}</span>` +
    `</button>`;

  const conn = resolveConnectivityCopy(connectivity, copy);
  const connL3Badge =
    disclosure === "L3"
      ? `<code class="nlos-status-enum nlos-l3-only">${escapeHtml(conn.l3Tag)}</code>`
      : "";
  const connectivityHtml =
    `<button type="button" class="nlos-taskbar-tray-btn" ` +
    `data-taskbar-action="open-connectivity" data-connectivity="${connectivity.kind}">` +
    `${renderMiniGlyphHtml(conn.glyph, conn.amber ? "nlos-taskbar-approval-diamond" : "")}` +
    `<span>${escapeHtml(conn.label)}</span>` +
    `${connL3Badge}` +
    `</button>`;

  const disclosureDisplayLevel: DisclosureLevel =
    disclosure === "L0" ? "L1" : disclosure;
  const disclosureActiveBar =
    disclosure === "L3"
      ? `<span class="nlos-taskbar-tab__active-bar" aria-hidden="true"></span>`
      : "";
  const disclosureHtml =
    `<button type="button" class="nlos-taskbar-tray-btn" ` +
    `data-taskbar-action="cycle-disclosure" data-disclosure-level="${disclosure}" ` +
    `title="${escapeHtml(copy.disclosureTooltip)}" aria-label="${escapeHtml(copy.disclosureTooltip)}">` +
    `${renderIconSvg(iconForDisclosureLevel(disclosureDisplayLevel), { size: 14, strokeWidth: 1.5 })}` +
    `<span>${escapeHtml(`${copy.disclosurePrefix}:${disclosureDisplayLevel}`)}</span>` +
    `${disclosureActiveBar}` +
    `</button>`;

  const themeLabel =
    theme === "paper-workstation" ? copy.themeLight : copy.themeDark;
  const themeHtml =
    `<button type="button" class="nlos-taskbar-tray-btn" ` +
    `data-taskbar-action="toggle-theme" data-theme="${theme}" ` +
    `aria-label="${escapeHtml(copy.themeAria)}">` +
    `${renderIconSvg("theme-switch", { size: 14, strokeWidth: 1.5 })}` +
    `<span>${escapeHtml(themeLabel)}</span>` +
    `</button>`;

  const captureSpecs: Array<{
    active: boolean;
    key: string;
    icon: IconName;
    label: string;
  }> = [
    {
      active: Boolean(capture.microphoneActive),
      key: "microphone",
      icon: "hw-microphone",
      label: copy.micCapture,
    },
    {
      active: Boolean(capture.cameraActive),
      key: "camera",
      icon: "hw-camera",
      label: copy.camCapture,
    },
    {
      active: Boolean(capture.screenActive),
      key: "screen",
      icon: "hw-screen",
      label: copy.screenCapture,
    },
  ];
  const captureHtml = captureSpecs
    .filter((c) => c.active)
    .map(
      (c) =>
        `<span class="nlos-taskbar-capture-pill" data-hw-capture="${c.key}" ` +
        `role="status" aria-label="${escapeHtml(c.label)}">` +
        `${renderIconSvg(c.icon, { size: 14, strokeWidth: 1.5 })}` +
        `${renderMiniGlyphHtml("solid-dot", "nlos-taskbar-capture-dot")}` +
        `</span>`,
    )
    .join("");

  const notifyHtml =
    `<button type="button" class="nlos-taskbar-tray-btn" data-taskbar-action="open-notifications">` +
    `${renderIconSvg("notification-tray", { size: 14, strokeWidth: 1.5 })}` +
    `<span>${escapeHtml(copy.notificationsLabel)}</span>` +
    `<span class="nlos-taskbar-badge nlos-tabular">${unreadCount}</span>` +
    `</button>`;

  const clockHtml =
    `<button type="button" class="nlos-taskbar-tray-btn nlos-taskbar-clock nlos-tabular" ` +
    `data-taskbar-action="open-control-center" aria-label="${escapeHtml(copy.clockAria)}">` +
    `${escapeHtml(clockStr)}` +
    `</button>`;

  return (
    `<footer role="contentinfo" aria-label="Workstation Taskbar" ` +
    `class="nlos-taskbar-shell nlos-plane-z3-taskbar" ` +
    `data-z4-locked="${z4Locked ? "true" : "false"}">` +
    `<div class="nlos-taskbar-left">${launcherHtml}${cmdTriggerHtml}</div>` +
    `<div class="nlos-taskbar-center">${windowTabsHtml}${taskTabHtml}</div>` +
    `<div class="nlos-taskbar-right">` +
    `${layoutBtnHtml}${connectivityHtml}${disclosureHtml}${themeHtml}${captureHtml}${notifyHtml}${clockHtml}` +
    `</div>` +
    `</footer>`
  );
}

export function createBottomTaskbarElement(options?: BottomTaskbarOptions): HTMLElement {
  const doc = options?.doc ?? document;
  const locale: WorkstationLocale = options?.locale ?? "zh-CN";
  const disclosure: DisclosureLevel = options?.disclosure ?? "L1";
  const copy = TASKBAR_COPY[locale];

  const windows = options?.windows ?? [];
  const activeTaskCount = Math.max(0, Math.floor(options?.activeTaskCount ?? 0));
  const pendingApprovalCount = Math.max(
    0,
    Math.floor(options?.pendingApprovalCount ?? 0),
  );
  const taskSpaceFocused = Boolean(options?.taskSpaceFocused);
  const launcherOpen = Boolean(options?.launcherOpen);
  const z4Locked = Boolean(options?.z4ModalLock);
  const layoutMode: WmLayoutMode =
    options?.layoutMode ?? "three-pane-workstation";
  const connectivity: TaskbarConnectivityState = options?.connectivity ?? {
    kind: "local-ready",
  };
  const theme: WorkstationThemeId = options?.theme ?? "graphite-workstation";
  const capture = options?.hardwareCapture ?? {};
  const unreadCount = Math.max(
    0,
    Math.floor(options?.unreadNotificationCount ?? 0),
  );
  const clockStr = formatTaskbarClock(options?.clockText ?? "00:00");

  const footer = doc.createElement("footer");
  footer.className = "nlos-taskbar-shell nlos-plane-z3-taskbar";
  footer.setAttribute("role", "contentinfo");
  footer.setAttribute("aria-label", "Workstation Taskbar");
  footer.setAttribute("data-z4-locked", z4Locked ? "true" : "false");

  // 1. Left zone
  const leftZone = doc.createElement("div");
  leftZone.className = "nlos-taskbar-left";

  const launcherBtn = doc.createElement("button");
  launcherBtn.type = "button";
  launcherBtn.className = "nlos-btn nlos-taskbar-launcher-btn";
  launcherBtn.setAttribute("data-taskbar-action", "toggle-launcher");
  launcherBtn.setAttribute("aria-expanded", launcherOpen ? "true" : "false");
  launcherBtn.appendChild(
    createIconElement("app-launcher", { size: 14, strokeWidth: 1.5, doc }),
  );
  const launcherText = doc.createElement("span");
  launcherText.textContent = copy.launcherBtn;
  launcherBtn.appendChild(launcherText);
  launcherBtn.addEventListener("click", () => {
    options?.callbacks?.onToggleLauncher?.();
  });
  leftZone.appendChild(launcherBtn);

  const cmdBtn = doc.createElement("button");
  cmdBtn.type = "button";
  cmdBtn.className = "nlos-taskbar-cmd-trigger";
  cmdBtn.setAttribute("data-taskbar-action", "open-command-palette");
  cmdBtn.disabled = z4Locked;
  cmdBtn.appendChild(
    createIconElement("command-prompt", { size: 14, strokeWidth: 1.5, doc }),
  );
  if (z4Locked) {
    const lockedSpan = doc.createElement("span");
    lockedSpan.textContent = copy.cmdLockedLabel;
    cmdBtn.appendChild(lockedSpan);
  } else {
    const placeholderSpan = doc.createElement("span");
    placeholderSpan.textContent = `${copy.cmdPromptPlaceholder} `;
    cmdBtn.appendChild(placeholderSpan);
    const kbd = doc.createElement("kbd");
    kbd.className = "nlos-taskbar-kbd";
    kbd.textContent = "Ctrl+Space";
    cmdBtn.appendChild(kbd);
  }
  cmdBtn.addEventListener("click", () => {
    if (!z4Locked) {
      options?.callbacks?.onOpenCommandPalette?.();
    }
  });
  leftZone.appendChild(cmdBtn);
  footer.appendChild(leftZone);

  // 2. Center-left zone
  const centerZone = doc.createElement("div");
  centerZone.className = "nlos-taskbar-center";

  for (const tab of windows) {
    const tabBtn = doc.createElement("button");
    tabBtn.type = "button";
    const tabClasses = ["nlos-taskbar-tab"];
    if (tab.focused) {
      tabClasses.push("nlos-taskbar-tab--focused");
    }
    if (tab.minimized) {
      tabClasses.push("nlos-taskbar-tab--minimized");
    }
    tabBtn.className = tabClasses.join(" ");
    tabBtn.setAttribute("data-taskbar-action", "activate-window");
    tabBtn.setAttribute("data-window-id", tab.windowId);
    tabBtn.setAttribute("aria-pressed", tab.focused ? "true" : "false");

    tabBtn.appendChild(
      createIconElement(tab.icon ?? "layout-1-pane", {
        size: 14,
        strokeWidth: 1.5,
        doc,
      }),
    );

    const titleSpan = doc.createElement("span");
    titleSpan.className = "nlos-taskbar-tab__title";
    titleSpan.textContent = tab.title;
    tabBtn.appendChild(titleSpan);

    if (tab.focused) {
      const bar = doc.createElement("span");
      bar.className = "nlos-taskbar-tab__active-bar";
      bar.setAttribute("aria-hidden", "true");
      tabBtn.appendChild(bar);
    }

    tabBtn.addEventListener("click", () => {
      options?.callbacks?.onActivateWindowTab?.(tab.windowId);
    });
    centerZone.appendChild(tabBtn);
  }

  const tasksBtn = doc.createElement("button");
  tasksBtn.type = "button";
  tasksBtn.className = taskSpaceFocused
    ? "nlos-taskbar-tab nlos-taskbar-tab--tasks nlos-taskbar-tab--focused"
    : "nlos-taskbar-tab nlos-taskbar-tab--tasks";
  tasksBtn.setAttribute("data-taskbar-action", "toggle-task-space");
  tasksBtn.setAttribute("aria-pressed", taskSpaceFocused ? "true" : "false");

  if (pendingApprovalCount > 0) {
    tasksBtn.appendChild(
      createMiniGlyphElement(doc, "solid-diamond", "nlos-taskbar-approval-diamond"),
    );
  }

  const tasksLabel = doc.createElement("span");
  tasksLabel.className = "nlos-tabular";
  tasksLabel.textContent = `${copy.tasksTabPrefix} ${activeTaskCount}`;
  tasksBtn.appendChild(tasksLabel);

  if (taskSpaceFocused) {
    const bar = doc.createElement("span");
    bar.className = "nlos-taskbar-tab__active-bar";
    bar.setAttribute("aria-hidden", "true");
    tasksBtn.appendChild(bar);
  }

  tasksBtn.addEventListener("click", () => {
    options?.callbacks?.onToggleTaskSpace?.();
  });
  centerZone.appendChild(tasksBtn);
  footer.appendChild(centerZone);

  // 3. Right system tray
  const rightZone = doc.createElement("div");
  rightZone.className = "nlos-taskbar-right";

  // 3.1 Layout switcher
  const layoutBtn = doc.createElement("button");
  layoutBtn.type = "button";
  layoutBtn.className = "nlos-taskbar-tray-btn";
  layoutBtn.setAttribute("data-taskbar-action", "cycle-layout");
  layoutBtn.setAttribute("data-layout-mode", layoutMode);
  layoutBtn.setAttribute("aria-label", copy.layoutAria);
  layoutBtn.appendChild(
    createIconElement(iconForLayoutMode(layoutMode), {
      size: 14,
      strokeWidth: 1.5,
      doc,
    }),
  );
  const layoutText = doc.createElement("span");
  layoutText.textContent = copy.layoutLabels[layoutMode];
  layoutBtn.appendChild(layoutText);
  layoutBtn.addEventListener("click", () => {
    options?.callbacks?.onSelectLayoutMode?.(nextLayoutMode(layoutMode));
  });
  rightZone.appendChild(layoutBtn);

  // 3.2 Connectivity posture ([DUI-OFFLINE-001])
  const conn = resolveConnectivityCopy(connectivity, copy);
  const connBtn = doc.createElement("button");
  connBtn.type = "button";
  connBtn.className = "nlos-taskbar-tray-btn";
  connBtn.setAttribute("data-taskbar-action", "open-connectivity");
  connBtn.setAttribute("data-connectivity", connectivity.kind);
  connBtn.appendChild(
    createMiniGlyphElement(
      doc,
      conn.glyph,
      conn.amber ? "nlos-taskbar-approval-diamond" : "",
    ),
  );
  const connText = doc.createElement("span");
  connText.textContent = conn.label;
  connBtn.appendChild(connText);
  if (disclosure === "L3") {
    const l3Badge = doc.createElement("code");
    l3Badge.className = "nlos-status-enum nlos-l3-only";
    l3Badge.textContent = conn.l3Tag;
    connBtn.appendChild(l3Badge);
  }
  connBtn.addEventListener("click", () => {
    options?.callbacks?.onOpenConnectivityFlyout?.();
  });
  rightZone.appendChild(connBtn);

  // 3.3 Disclosure indicator & button (Ctrl+Shift+E)
  const disclosureDisplayLevel: DisclosureLevel =
    disclosure === "L0" ? "L1" : disclosure;
  const discBtn = doc.createElement("button");
  discBtn.type = "button";
  discBtn.className = "nlos-taskbar-tray-btn";
  discBtn.setAttribute("data-taskbar-action", "cycle-disclosure");
  discBtn.setAttribute("data-disclosure-level", disclosure);
  discBtn.setAttribute("title", copy.disclosureTooltip);
  discBtn.setAttribute("aria-label", copy.disclosureTooltip);
  discBtn.appendChild(
    createIconElement(iconForDisclosureLevel(disclosureDisplayLevel), {
      size: 14,
      strokeWidth: 1.5,
      doc,
    }),
  );
  const discText = doc.createElement("span");
  discText.textContent = `${copy.disclosurePrefix}:${disclosureDisplayLevel}`;
  discBtn.appendChild(discText);
  if (disclosure === "L3") {
    const bar = doc.createElement("span");
    bar.className = "nlos-taskbar-tab__active-bar";
    bar.setAttribute("aria-hidden", "true");
    discBtn.appendChild(bar);
  }
  discBtn.addEventListener("click", () => {
    options?.callbacks?.onCycleDisclosure?.(nextDisclosureLevel(disclosure));
  });
  rightZone.appendChild(discBtn);

  // 3.4 Theme switcher
  const themeBtn = doc.createElement("button");
  themeBtn.type = "button";
  themeBtn.className = "nlos-taskbar-tray-btn";
  themeBtn.setAttribute("data-taskbar-action", "toggle-theme");
  themeBtn.setAttribute("data-theme", theme);
  themeBtn.setAttribute("aria-label", copy.themeAria);
  themeBtn.appendChild(
    createIconElement("theme-switch", { size: 14, strokeWidth: 1.5, doc }),
  );
  const themeText = doc.createElement("span");
  themeText.textContent =
    theme === "paper-workstation" ? copy.themeLight : copy.themeDark;
  themeBtn.appendChild(themeText);
  themeBtn.addEventListener("click", () => {
    const next =
      theme === "paper-workstation"
        ? "graphite-workstation"
        : "paper-workstation";
    options?.callbacks?.onToggleTheme?.(next);
  });
  rightZone.appendChild(themeBtn);

  // 3.5 Hardware capture pills (DevicePermissionState)
  const captureSpecs: Array<{
    active: boolean;
    key: string;
    icon: IconName;
    label: string;
  }> = [
    {
      active: Boolean(capture.microphoneActive),
      key: "microphone",
      icon: "hw-microphone",
      label: copy.micCapture,
    },
    {
      active: Boolean(capture.cameraActive),
      key: "camera",
      icon: "hw-camera",
      label: copy.camCapture,
    },
    {
      active: Boolean(capture.screenActive),
      key: "screen",
      icon: "hw-screen",
      label: copy.screenCapture,
    },
  ];
  for (const c of captureSpecs) {
    if (!c.active) {
      continue;
    }
    const pill = doc.createElement("span");
    pill.className = "nlos-taskbar-capture-pill";
    pill.setAttribute("data-hw-capture", c.key);
    pill.setAttribute("role", "status");
    pill.setAttribute("aria-label", c.label);
    pill.appendChild(createIconElement(c.icon, { size: 14, strokeWidth: 1.5, doc }));
    pill.appendChild(
      createMiniGlyphElement(doc, "solid-dot", "nlos-taskbar-capture-dot"),
    );
    rightZone.appendChild(pill);
  }

  // 3.6 Notification badge button (zero bell emoji)
  const notifyBtn = doc.createElement("button");
  notifyBtn.type = "button";
  notifyBtn.className = "nlos-taskbar-tray-btn";
  notifyBtn.setAttribute("data-taskbar-action", "open-notifications");
  notifyBtn.appendChild(
    createIconElement("notification-tray", { size: 14, strokeWidth: 1.5, doc }),
  );
  const notifyLabel = doc.createElement("span");
  notifyLabel.textContent = copy.notificationsLabel;
  notifyBtn.appendChild(notifyLabel);
  const badgeSpan = doc.createElement("span");
  badgeSpan.className = "nlos-taskbar-badge nlos-tabular";
  badgeSpan.textContent = String(unreadCount);
  notifyBtn.appendChild(badgeSpan);
  notifyBtn.addEventListener("click", () => {
    options?.callbacks?.onOpenNotifications?.();
  });
  rightZone.appendChild(notifyBtn);

  // 3.7 12px tabular-nums monospace clock
  const clockBtn = doc.createElement("button");
  clockBtn.type = "button";
  clockBtn.className = "nlos-taskbar-tray-btn nlos-taskbar-clock nlos-tabular";
  clockBtn.setAttribute("data-taskbar-action", "open-control-center");
  clockBtn.setAttribute("aria-label", copy.clockAria);
  clockBtn.textContent = clockStr;
  clockBtn.addEventListener("click", () => {
    options?.callbacks?.onOpenControlCenter?.();
  });
  rightZone.appendChild(clockBtn);

  footer.appendChild(rightZone);
  return footer;
}
