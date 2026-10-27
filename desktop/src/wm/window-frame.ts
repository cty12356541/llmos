// Trusted 30px WindowFrame, Stale Generation Fence & CSP Null-Origin Surface Viewport.
// Authority: ADR-0025 Decision 1 & 3 + Stage D Desktop Visual & UX Spec §3.3 ([DUI-WINDOW-001]).
// Strict discipline:
//   - 0 internal jargon in L0/L1/L2 (gen:N and surface IDs strictly gated to disclosure === "L3")
//   - Null-origin sandboxed iframe: sandbox="allow-scripts" ONLY (zero allow-same-origin/popups/modals/top-nav)
//   - System-plane occlusion guard hides live child frame when Z3/Z4 covers the window

import {
  createIconElement,
  renderIconSvg,
  type DisclosureLevel,
  type IconName,
  type WorkstationLocale,
} from "../ui";
import type { SolvedWindowGeometry } from "./layout-solver";

export const CSP_SANDBOX_POLICY = "allow-scripts";
export const CSP_SURFACE_DIRECTIVE =
  "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; img-src blob: data:;";

export interface WindowFrameCallbacks {
  readonly onFocus?: (windowId: string) => void;
  readonly onMinimize?: (windowId: string) => void;
  readonly onToggleFloat?: (windowId: string) => void;
  readonly onMaximize?: (windowId: string) => void;
  readonly onClose?: (windowId: string) => void;
  readonly onReloadLatestGeneration?: (windowId: string) => void;
}

export interface WindowFrameOptions {
  readonly windowId: string;
  readonly surfaceId: string;
  readonly applicationIdHex?: string;
  readonly title: string;
  readonly appIcon?: IconName;
  readonly surfaceGeneration: number;
  readonly latestInstallationGeneration?: number;
  readonly staleGeneration?: boolean;
  readonly focused?: boolean;
  readonly isFloating?: boolean;
  readonly isMaximized?: boolean;
  readonly occludedBySystemPlane?: boolean;
  readonly geometry?: SolvedWindowGeometry;
  readonly srcdoc?: string;
  readonly iframeSrc?: string;
  readonly locale?: WorkstationLocale;
  readonly disclosure?: DisclosureLevel;
  readonly callbacks?: WindowFrameCallbacks;
  readonly doc?: Document;
}

interface FrameCopy {
  readonly minimizeAria: string;
  readonly floatAria: string;
  readonly tileAria: string;
  readonly maximizeAria: string;
  readonly restoreAria: string;
  readonly closeAria: string;
  readonly staleNotice: string;
  readonly reloadBtn: string;
  readonly closeStaleBtn: string;
  readonly occludedNotice: string;
}

const FRAME_COPY: Readonly<Record<WorkstationLocale, FrameCopy>> = {
  "zh-CN": {
    minimizeAria: "最小化窗口",
    floatAria: "切换为浮动窗口",
    tileAria: "切换为平铺窗口",
    maximizeAria: "工作区内最大化",
    restoreAria: "还原平铺布局",
    closeAria: "关闭窗口",
    staleNotice: "应用已更新至新版本，旧窗口已停止接收输入",
    reloadBtn: "重新加载新版本",
    closeStaleBtn: "关闭窗口",
    occludedNotice: "系统顶层面板激活中 · 应用画面已安全遮蔽",
  },
  "en-US": {
    minimizeAria: "Minimize window",
    floatAria: "Switch to floating window",
    tileAria: "Switch to tiled window",
    maximizeAria: "Maximize within workspace",
    restoreAria: "Restore tiled layout",
    closeAria: "Close window",
    staleNotice: "Application updated to a newer version; this window no longer accepts input",
    reloadBtn: "Reload New Version",
    closeStaleBtn: "Close Window",
    occludedNotice: "System overlay active · Application view safely occluded",
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

function isSafeSurfaceSrc(src: string | undefined): boolean {
  if (!src) {
    return false;
  }
  const trimmed = src.trim();
  return (
    trimmed === "about:blank" ||
    trimmed.startsWith("blob:") ||
    trimmed.startsWith("data:") ||
    trimmed.startsWith("nlos-surface://")
  );
}

export function isWindowGenerationStale(options: {
  readonly surfaceGeneration: number;
  readonly latestInstallationGeneration?: number;
  readonly staleGeneration?: boolean;
}): boolean {
  if (options.staleGeneration) {
    return true;
  }
  if (
    options.latestInstallationGeneration !== undefined &&
    options.latestInstallationGeneration > options.surfaceGeneration
  ) {
    return true;
  }
  return false;
}

export function renderWindowFrameHtml(options: WindowFrameOptions): string {
  const locale: WorkstationLocale = options.locale ?? "zh-CN";
  const disclosure: DisclosureLevel = options.disclosure ?? "L1";
  const copy = FRAME_COPY[locale];

  const floating = Boolean(options.geometry?.isFloating ?? options.isFloating);
  const focused = Boolean(options.focused);
  const maximized = Boolean(options.isMaximized);
  const stale = isWindowGenerationStale(options);
  const occluded = Boolean(
    options.geometry?.occludedBySystemPlane ?? options.occludedBySystemPlane,
  );
  const iconName: IconName = options.appIcon ?? "layout-1-pane";

  const classes = ["nlos-wm-window"];
  if (focused) {
    classes.push("nlos-wm-window--focused");
  }
  if (floating) {
    classes.push("nlos-wm-window--floating");
  }
  if (stale) {
    classes.push("nlos-wm-window--stale");
  }
  if (occluded) {
    classes.push("nlos-wm-window--occluded");
  }

  let inlineStyle = "";
  if (options.geometry) {
    const r = options.geometry.outerFrameRect;
    const radiusRule = floating ? "border-radius: 6px;" : "border-radius: 0px;";
    inlineStyle =
      ` style="left: ${r.x}px; top: ${r.y}px; width: ${r.width}px; height: ${r.height}px; ` +
      `z-index: ${options.geometry.zIndex}; ${radiusRule}"`;
  }

  const l3TitleBadge =
    disclosure === "L3"
      ? `<code class="nlos-status-enum nlos-l3-only">gen:${options.surfaceGeneration} · surface:${escapeHtml(options.surfaceId)}</code>`
      : "";

  const targetGen =
    options.latestInstallationGeneration !== undefined &&
    options.latestInstallationGeneration > options.surfaceGeneration
      ? options.latestInstallationGeneration
      : options.surfaceGeneration + 1;

  const staleBannerHtml = stale
    ? `<div class="nlos-wm-stale-banner" role="status" data-stale-fence="true">` +
      `<div class="nlos-wm-stale-banner__msg">` +
      `${renderIconSvg("warning", { size: 14, strokeWidth: 1.5 })}` +
      `<span>${escapeHtml(copy.staleNotice)}</span>` +
      (disclosure === "L3"
        ? `<code class="nlos-status-enum nlos-l3-only">gen:${options.surfaceGeneration} -&gt; gen:${targetGen}</code>`
        : "") +
      `</div>` +
      `<div class="nlos-wm-stale-banner__actions">` +
      `<button type="button" class="nlos-btn" data-wm-action="reload-generation">` +
      `${renderIconSvg("retry", { size: 14, strokeWidth: 1.5 })}<span>${escapeHtml(copy.reloadBtn)}</span>` +
      `</button>` +
      `<button type="button" class="nlos-btn" data-wm-action="close-stale">` +
      `${renderIconSvg("close", { size: 14, strokeWidth: 1.5 })}<span>${escapeHtml(copy.closeStaleBtn)}</span>` +
      `</button>` +
      `</div>` +
      `</div>`
    : "";

  const viewportClasses = ["nlos-wm-viewport"];
  if (stale) {
    viewportClasses.push("nlos-wm-viewport--stale");
  }
  if (occluded) {
    viewportClasses.push("nlos-wm-viewport--occluded");
  }

  const iframeSrcAttr = isSafeSurfaceSrc(options.iframeSrc)
    ? ` src="${escapeHtml(options.iframeSrc!)}"`
    : ` srcdoc="${escapeHtml(options.srcdoc ?? "")}"`;
  const iframeHiddenAttrs = occluded
    ? ` hidden aria-hidden="true" tabindex="-1"`
    : stale
      ? ` tabindex="-1"`
      : "";

  const occlusionGuardHtml = occluded
    ? `<div class="nlos-wm-occlusion-guard" data-occlusion-guard="true">` +
      `${renderIconSvg("lock", { size: 14, strokeWidth: 1.5 })}` +
      `<span>${escapeHtml(copy.occludedNotice)}</span>` +
      `</div>`
    : "";

  return (
    `<section role="region" aria-label="Window: ${escapeHtml(options.title)}" ` +
    `class="${classes.join(" ")}" ` +
    `data-window-id="${escapeHtml(options.windowId)}" ` +
    `data-focused="${focused ? "true" : "false"}" ` +
    `data-floating="${floating ? "true" : "false"}" ` +
    `data-stale="${stale ? "true" : "false"}" ` +
    `data-occluded="${occluded ? "true" : "false"}"${inlineStyle}>` +
    `<header class="nlos-window-titlebar">` +
    `<div class="nlos-wm-titlebar-left">` +
    `${renderIconSvg(iconName, { size: 14, strokeWidth: 1.5 })}` +
    `<span class="nlos-wm-title">${escapeHtml(options.title)}</span>` +
    `${l3TitleBadge}` +
    `</div>` +
    `<div class="nlos-wm-controls">` +
    `<button type="button" class="nlos-wm-ctrl-btn" data-wm-action="minimize" aria-label="${escapeHtml(copy.minimizeAria)}">` +
    `${renderIconSvg("disclosure-l0", { size: 14, strokeWidth: 1.5 })}` +
    `</button>` +
    `<button type="button" class="nlos-wm-ctrl-btn" data-wm-action="toggle-float" ` +
    `aria-pressed="${floating ? "true" : "false"}" ` +
    `aria-label="${escapeHtml(floating ? copy.tileAria : copy.floatAria)}">` +
    `${renderIconSvg(floating ? "layout-2-col" : "layout-float", { size: 14, strokeWidth: 1.5 })}` +
    `</button>` +
    `<button type="button" class="nlos-wm-ctrl-btn" data-wm-action="maximize" ` +
    `aria-pressed="${maximized ? "true" : "false"}" ` +
    `aria-label="${escapeHtml(maximized ? copy.restoreAria : copy.maximizeAria)}">` +
    `${renderIconSvg("layout-1-pane", { size: 14, strokeWidth: 1.5 })}` +
    `</button>` +
    `<button type="button" class="nlos-wm-ctrl-btn nlos-wm-ctrl-btn--close" data-wm-action="close" aria-label="${escapeHtml(copy.closeAria)}">` +
    `${renderIconSvg("close", { size: 14, strokeWidth: 1.5 })}` +
    `</button>` +
    `</div>` +
    `</header>` +
    `${staleBannerHtml}` +
    `<div class="${viewportClasses.join(" ")}"${stale ? ' aria-disabled="true"' : ""}>` +
    `<iframe class="nlos-wm-iframe" ` +
    `sandbox="${CSP_SANDBOX_POLICY}" ` +
    `csp="${CSP_SURFACE_DIRECTIVE}" ` +
    `referrerpolicy="no-referrer" ` +
    `title="${escapeHtml(options.title)}"${iframeSrcAttr}${iframeHiddenAttrs}></iframe>` +
    `${occlusionGuardHtml}` +
    `</div>` +
    `</section>`
  );
}

export function createWindowFrameElement(options: WindowFrameOptions): HTMLElement {
  const doc = options.doc ?? document;
  const locale: WorkstationLocale = options.locale ?? "zh-CN";
  const disclosure: DisclosureLevel = options.disclosure ?? "L1";
  const copy = FRAME_COPY[locale];

  const floating = Boolean(options.geometry?.isFloating ?? options.isFloating);
  const focused = Boolean(options.focused);
  const maximized = Boolean(options.isMaximized);
  const stale = isWindowGenerationStale(options);
  const occluded = Boolean(
    options.geometry?.occludedBySystemPlane ?? options.occludedBySystemPlane,
  );
  const iconName: IconName = options.appIcon ?? "layout-1-pane";

  const section = doc.createElement("section");
  section.setAttribute("role", "region");
  section.setAttribute("aria-label", `Window: ${options.title}`);

  const classes = ["nlos-wm-window"];
  if (focused) {
    classes.push("nlos-wm-window--focused");
  }
  if (floating) {
    classes.push("nlos-wm-window--floating");
  }
  if (stale) {
    classes.push("nlos-wm-window--stale");
  }
  if (occluded) {
    classes.push("nlos-wm-window--occluded");
  }
  section.className = classes.join(" ");

  section.setAttribute("data-window-id", options.windowId);
  section.setAttribute("data-focused", focused ? "true" : "false");
  section.setAttribute("data-floating", floating ? "true" : "false");
  section.setAttribute("data-stale", stale ? "true" : "false");
  section.setAttribute("data-occluded", occluded ? "true" : "false");

  if (options.geometry) {
    const r = options.geometry.outerFrameRect;
    section.style.left = `${r.x}px`;
    section.style.top = `${r.y}px`;
    section.style.width = `${r.width}px`;
    section.style.height = `${r.height}px`;
    section.style.zIndex = String(options.geometry.zIndex);
    section.style.borderRadius = floating ? "6px" : "0px";
  }

  section.addEventListener("pointerdown", () => {
    options.callbacks?.onFocus?.(options.windowId);
  });

  // 30px Solid Trusted Titlebar
  const header = doc.createElement("header");
  header.className = "nlos-window-titlebar";

  const left = doc.createElement("div");
  left.className = "nlos-wm-titlebar-left";
  left.appendChild(createIconElement(iconName, { size: 14, strokeWidth: 1.5, doc }));

  const titleSpan = doc.createElement("span");
  titleSpan.className = "nlos-wm-title";
  titleSpan.textContent = options.title;
  left.appendChild(titleSpan);

  if (disclosure === "L3") {
    const badge = doc.createElement("code");
    badge.className = "nlos-status-enum nlos-l3-only";
    badge.textContent = `gen:${options.surfaceGeneration} · surface:${options.surfaceId}`;
    left.appendChild(badge);
  }
  header.appendChild(left);

  const controls = doc.createElement("div");
  controls.className = "nlos-wm-controls";

  const makeCtrlBtn = (
    action: string,
    icon: IconName,
    ariaLabel: string,
    extraClass?: string,
    pressed?: boolean,
    handler?: () => void,
  ): HTMLButtonElement => {
    const btn = doc.createElement("button");
    btn.type = "button";
    btn.className = extraClass ? `nlos-wm-ctrl-btn ${extraClass}` : "nlos-wm-ctrl-btn";
    btn.setAttribute("data-wm-action", action);
    btn.setAttribute("aria-label", ariaLabel);
    if (pressed !== undefined) {
      btn.setAttribute("aria-pressed", pressed ? "true" : "false");
    }
    btn.appendChild(createIconElement(icon, { size: 14, strokeWidth: 1.5, doc }));
    btn.addEventListener("click", (ev) => {
      ev.stopPropagation();
      handler?.();
    });
    return btn;
  };

  controls.appendChild(
    makeCtrlBtn("minimize", "disclosure-l0", copy.minimizeAria, undefined, undefined, () => {
      options.callbacks?.onMinimize?.(options.windowId);
    }),
  );
  controls.appendChild(
    makeCtrlBtn(
      "toggle-float",
      floating ? "layout-2-col" : "layout-float",
      floating ? copy.tileAria : copy.floatAria,
      undefined,
      floating,
      () => {
        options.callbacks?.onToggleFloat?.(options.windowId);
      },
    ),
  );
  controls.appendChild(
    makeCtrlBtn(
      "maximize",
      "layout-1-pane",
      maximized ? copy.restoreAria : copy.maximizeAria,
      undefined,
      maximized,
      () => {
        options.callbacks?.onMaximize?.(options.windowId);
      },
    ),
  );
  controls.appendChild(
    makeCtrlBtn(
      "close",
      "close",
      copy.closeAria,
      "nlos-wm-ctrl-btn--close",
      undefined,
      () => {
        options.callbacks?.onClose?.(options.windowId);
      },
    ),
  );

  header.appendChild(controls);
  section.appendChild(header);

  // Stale Installation Generation Fence Banner
  if (stale) {
    const targetGen =
      options.latestInstallationGeneration !== undefined &&
      options.latestInstallationGeneration > options.surfaceGeneration
        ? options.latestInstallationGeneration
        : options.surfaceGeneration + 1;

    const banner = doc.createElement("div");
    banner.className = "nlos-wm-stale-banner";
    banner.setAttribute("role", "status");
    banner.setAttribute("data-stale-fence", "true");

    const msgBox = doc.createElement("div");
    msgBox.className = "nlos-wm-stale-banner__msg";
    msgBox.appendChild(createIconElement("warning", { size: 14, strokeWidth: 1.5, doc }));

    const msgSpan = doc.createElement("span");
    msgSpan.textContent = copy.staleNotice;
    msgBox.appendChild(msgSpan);

    if (disclosure === "L3") {
      const genBadge = doc.createElement("code");
      genBadge.className = "nlos-status-enum nlos-l3-only";
      genBadge.textContent = `gen:${options.surfaceGeneration} -> gen:${targetGen}`;
      msgBox.appendChild(genBadge);
    }
    banner.appendChild(msgBox);

    const actions = doc.createElement("div");
    actions.className = "nlos-wm-stale-banner__actions";

    const reloadBtn = doc.createElement("button");
    reloadBtn.type = "button";
    reloadBtn.className = "nlos-btn";
    reloadBtn.setAttribute("data-wm-action", "reload-generation");
    reloadBtn.appendChild(createIconElement("retry", { size: 14, strokeWidth: 1.5, doc }));
    const reloadLabel = doc.createElement("span");
    reloadLabel.textContent = copy.reloadBtn;
    reloadBtn.appendChild(reloadLabel);
    reloadBtn.addEventListener("click", (ev) => {
      ev.stopPropagation();
      options.callbacks?.onReloadLatestGeneration?.(options.windowId);
    });
    actions.appendChild(reloadBtn);

    const closeStaleBtn = doc.createElement("button");
    closeStaleBtn.type = "button";
    closeStaleBtn.className = "nlos-btn";
    closeStaleBtn.setAttribute("data-wm-action", "close-stale");
    closeStaleBtn.appendChild(createIconElement("close", { size: 14, strokeWidth: 1.5, doc }));
    const closeLabel = doc.createElement("span");
    closeLabel.textContent = copy.closeStaleBtn;
    closeStaleBtn.appendChild(closeLabel);
    closeStaleBtn.addEventListener("click", (ev) => {
      ev.stopPropagation();
      options.callbacks?.onClose?.(options.windowId);
    });
    actions.appendChild(closeStaleBtn);

    banner.appendChild(actions);
    section.appendChild(banner);
  }

  // Sandboxed Child Surface Viewport
  const viewport = doc.createElement("div");
  const vpClasses = ["nlos-wm-viewport"];
  if (stale) {
    vpClasses.push("nlos-wm-viewport--stale");
    viewport.setAttribute("aria-disabled", "true");
  }
  if (occluded) {
    vpClasses.push("nlos-wm-viewport--occluded");
  }
  viewport.className = vpClasses.join(" ");

  const iframe = doc.createElement("iframe");
  iframe.className = "nlos-wm-iframe";
  iframe.setAttribute("sandbox", CSP_SANDBOX_POLICY);
  iframe.setAttribute("csp", CSP_SURFACE_DIRECTIVE);
  iframe.setAttribute("referrerpolicy", "no-referrer");
  iframe.setAttribute("title", options.title);
  if (isSafeSurfaceSrc(options.iframeSrc)) {
    iframe.setAttribute("src", options.iframeSrc!);
  } else {
    iframe.setAttribute("srcdoc", options.srcdoc ?? "");
  }
  if (occluded) {
    iframe.hidden = true;
    iframe.setAttribute("aria-hidden", "true");
    iframe.tabIndex = -1;
  } else if (stale) {
    iframe.tabIndex = -1;
  }
  viewport.appendChild(iframe);

  if (occluded) {
    const guard = doc.createElement("div");
    guard.className = "nlos-wm-occlusion-guard";
    guard.setAttribute("data-occlusion-guard", "true");
    guard.appendChild(createIconElement("lock", { size: 14, strokeWidth: 1.5, doc }));
    const guardText = doc.createElement("span");
    guardText.textContent = copy.occludedNotice;
    guard.appendChild(guardText);
    viewport.appendChild(guard);
  }

  section.appendChild(viewport);
  return section;
}
