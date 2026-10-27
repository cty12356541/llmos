// SOTA-1 (CNCF Headlamp): Non-Modal 420px Right / 36% Bottom Solid InspectorDrawer
// with ScopeBreadcrumbBar, L2 <-> L3 toggle hook, and Escape close handler.
// Authority: Stage D Desktop Visual & UX Spec §2A.1 (SOTA-1), §3.2.C, ADR-0025 Decision 4.

import { createIconElement, renderIconSvg } from "./icons";
import type { DisclosureLevel, WorkstationLocale } from "./status-indicator";

export type InspectorScopeKind =
  | "System"
  | "Workspace"
  | "Application"
  | "Task"
  | "TaskGroup"
  | "TaskNode"
  | "TaskAttempt"
  | "Fiber";

export const INSPECTOR_SCOPE_ORDER: readonly InspectorScopeKind[] = [
  "System",
  "Workspace",
  "Application",
  "Task",
  "TaskGroup",
  "TaskNode",
  "TaskAttempt",
  "Fiber",
];

export type InspectorDrawerPlacement = "right" | "bottom";
export type DrawerDisclosureLevel = "L2" | "L3";

export interface ScopeBreadcrumbItem {
  readonly kind: InspectorScopeKind;
  readonly id: string;
  readonly label: string;
}

export interface InspectorDrawerOptions {
  readonly open?: boolean;
  readonly placement?: InspectorDrawerPlacement;
  readonly disclosure?: DrawerDisclosureLevel;
  readonly locale?: WorkstationLocale;
  readonly breadcrumbs: readonly ScopeBreadcrumbItem[];
  readonly onSelectScope?: (item: ScopeBreadcrumbItem, index: number) => void;
  readonly onToggleDisclosure?: (next: DrawerDisclosureLevel) => void;
  readonly onClose?: () => void;
  readonly doc?: Document;
}

export interface InspectorDrawerController {
  readonly element: HTMLElement;
  readonly bodyElement: HTMLElement;
  isOpen(): boolean;
  getDisclosure(): DrawerDisclosureLevel;
  getPlacement(): InspectorDrawerPlacement;
  getVisibleBreadcrumbs(): readonly ScopeBreadcrumbItem[];
  setDisclosure(next: DrawerDisclosureLevel): void;
  toggleDisclosure(): DrawerDisclosureLevel;
  setBreadcrumbs(items: readonly ScopeBreadcrumbItem[]): void;
  open(): void;
  close(): void;
  handleKeyDown(event: { readonly key: string; preventDefault?: () => void }): boolean;
}

const SCOPE_LABELS: Readonly<
  Record<InspectorScopeKind, Readonly<Record<WorkstationLocale, string>>>
> = {
  System: { "zh-CN": "系统", "en-US": "System" },
  Workspace: { "zh-CN": "工作区", "en-US": "Workspace" },
  Application: { "zh-CN": "应用", "en-US": "Application" },
  Task: { "zh-CN": "任务", "en-US": "Task" },
  TaskGroup: { "zh-CN": "步骤组", "en-US": "Step Group" },
  TaskNode: { "zh-CN": "步骤", "en-US": "Step" },
  TaskAttempt: { "zh-CN": "TaskAttempt", "en-US": "TaskAttempt" },
  Fiber: { "zh-CN": "Fiber", "en-US": "Fiber" },
};

function escapeHtml(raw: string): string {
  return raw
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

/** Filters out L3-only kernel scopes (TaskAttempt, Fiber) when disclosure is below L3. */
export function filterBreadcrumbsForDisclosure(
  breadcrumbs: readonly ScopeBreadcrumbItem[],
  disclosure: DisclosureLevel,
): readonly ScopeBreadcrumbItem[] {
  if (disclosure === "L0" || disclosure === "L1") {
    return [];
  }
  const sorted = [...breadcrumbs].sort(
    (a, b) => INSPECTOR_SCOPE_ORDER.indexOf(a.kind) - INSPECTOR_SCOPE_ORDER.indexOf(b.kind),
  );
  if (disclosure === "L2") {
    return sorted.filter((b) => b.kind !== "TaskAttempt" && b.kind !== "Fiber");
  }
  return sorted;
}

export function formatBreadcrumbSegmentLabel(
  item: ScopeBreadcrumbItem,
  disclosure: DrawerDisclosureLevel,
  locale: WorkstationLocale,
): string {
  if (disclosure === "L3") {
    return `${item.kind}:${item.label}`;
  }
  const kindPrefix = SCOPE_LABELS[item.kind][locale];
  return `${kindPrefix}: ${item.label}`;
}

export function renderInspectorDrawerHtml(options: InspectorDrawerOptions): string {
  const isOpen = options.open ?? true;
  if (!isOpen) {
    return "";
  }
  const placement: InspectorDrawerPlacement = options.placement ?? "right";
  const disclosure: DrawerDisclosureLevel = options.disclosure ?? "L2";
  const locale: WorkstationLocale = options.locale ?? "zh-CN";
  const visible = filterBreadcrumbsForDisclosure(options.breadcrumbs, disclosure);
  const sepSvg = renderIconSvg("chevron-right", { size: 14, strokeWidth: 1.25 });

  const crumbsHtml = visible
    .map((item, idx) => {
      const isLast = idx === visible.length - 1;
      const ariaCurrent = isLast ? ` aria-current="page"` : "";
      const text = formatBreadcrumbSegmentLabel(item, disclosure, locale);
      const prefix = idx > 0 ? sepSvg : "";
      return (
        `${prefix}<button type="button" class="nlos-breadcrumb__btn" ` +
        `data-scope-kind="${item.kind}" data-scope-id="${escapeHtml(item.id)}"${ariaCurrent}>` +
        `${escapeHtml(text)}</button>`
      );
    })
    .join("");

  const toggleText =
    disclosure === "L2"
      ? locale === "zh-CN"
        ? "L3 深度诊断"
        : "L3 Diagnostics"
      : locale === "zh-CN"
        ? "返回 L2 摘要"
        : "Back to L2";
  const closeTitle = locale === "zh-CN" ? "关闭详情抽屉 (Esc)" : "Close Inspector Drawer (Esc)";
  const closeIcon = renderIconSvg("close", { size: 14, strokeWidth: 1.5 });

  return (
    `<aside class="nlos-inspector-drawer nlos-inspector-drawer--${placement}" ` +
    `role="complementary" aria-label="Inspector Drawer" ` +
    `data-placement="${placement}" data-drawer-disclosure="${disclosure}">` +
    `<header class="nlos-inspector-drawer__header">` +
    `<nav class="nlos-breadcrumb" aria-label="Scope Breadcrumb">${crumbsHtml}</nav>` +
    `<div style="display:inline-flex;align-items:center;gap:4px">` +
    `<button type="button" class="nlos-btn" data-action="toggle-disclosure">${escapeHtml(toggleText)}</button>` +
    `<button type="button" class="nlos-btn nlos-btn--icon-only" data-action="close-drawer" ` +
    `aria-label="${escapeHtml(closeTitle)}">${closeIcon}</button>` +
    `</div>` +
    `</header>` +
    `<div class="nlos-inspector-drawer__body"></div>` +
    `</aside>`
  );
}

export function createInspectorDrawerElement(
  options: InspectorDrawerOptions,
): InspectorDrawerController {
  const doc = options.doc ?? document;
  let openState = options.open ?? true;
  const placement: InspectorDrawerPlacement = options.placement ?? "right";
  let disclosure: DrawerDisclosureLevel = options.disclosure ?? "L2";
  const locale: WorkstationLocale = options.locale ?? "zh-CN";
  let breadcrumbs: readonly ScopeBreadcrumbItem[] = [...options.breadcrumbs];

  const aside = doc.createElement("aside");
  aside.className = `nlos-inspector-drawer nlos-inspector-drawer--${placement}`;
  aside.setAttribute("role", "complementary");
  aside.setAttribute("aria-label", "Inspector Drawer");
  aside.setAttribute("data-placement", placement);
  aside.setAttribute("data-drawer-disclosure", disclosure);
  aside.tabIndex = -1;

  const header = doc.createElement("header");
  header.className = "nlos-inspector-drawer__header";

  const nav = doc.createElement("nav");
  nav.className = "nlos-breadcrumb";
  nav.setAttribute("aria-label", "Scope Breadcrumb");

  const actions = doc.createElement("div");
  actions.style.display = "inline-flex";
  actions.style.alignItems = "center";
  actions.style.gap = "4px";

  const toggleBtn = doc.createElement("button");
  toggleBtn.type = "button";
  toggleBtn.className = "nlos-btn";
  toggleBtn.setAttribute("data-action", "toggle-disclosure");

  const closeBtn = doc.createElement("button");
  closeBtn.type = "button";
  closeBtn.className = "nlos-btn nlos-btn--icon-only";
  closeBtn.setAttribute("data-action", "close-drawer");
  closeBtn.setAttribute(
    "aria-label",
    locale === "zh-CN" ? "关闭详情抽屉 (Esc)" : "Close Inspector Drawer (Esc)",
  );
  closeBtn.appendChild(createIconElement("close", { size: 14, strokeWidth: 1.5, doc }));

  const bodyElement = doc.createElement("div");
  bodyElement.className = "nlos-inspector-drawer__body";

  const syncHeader = (): void => {
    aside.hidden = !openState;
    aside.setAttribute("data-open", openState ? "true" : "false");
    aside.setAttribute("data-drawer-disclosure", disclosure);

    toggleBtn.textContent =
      disclosure === "L2"
        ? locale === "zh-CN"
          ? "L3 深度诊断"
          : "L3 Diagnostics"
        : locale === "zh-CN"
          ? "返回 L2 摘要"
          : "Back to L2";

    nav.replaceChildren();
    const visible = filterBreadcrumbsForDisclosure(breadcrumbs, disclosure);
    visible.forEach((item, idx) => {
      if (idx > 0) {
        nav.appendChild(
          createIconElement("chevron-right", { size: 14, strokeWidth: 1.25, doc }),
        );
      }
      const btn = doc.createElement("button");
      btn.type = "button";
      btn.className = "nlos-breadcrumb__btn";
      btn.setAttribute("data-scope-kind", item.kind);
      btn.setAttribute("data-scope-id", item.id);
      if (idx === visible.length - 1) {
        btn.setAttribute("aria-current", "page");
      }
      btn.textContent = formatBreadcrumbSegmentLabel(item, disclosure, locale);
      btn.addEventListener("click", () => {
        options.onSelectScope?.(item, idx);
      });
      nav.appendChild(btn);
    });
  };

  const setDisclosure = (next: DrawerDisclosureLevel): void => {
    if (disclosure === next) {
      return;
    }
    disclosure = next;
    syncHeader();
    options.onToggleDisclosure?.(disclosure);
  };

  const toggleDisclosure = (): DrawerDisclosureLevel => {
    const next: DrawerDisclosureLevel = disclosure === "L2" ? "L3" : "L2";
    setDisclosure(next);
    return next;
  };

  const close = (): void => {
    if (!openState) {
      return;
    }
    openState = false;
    syncHeader();
    options.onClose?.();
  };

  const open = (): void => {
    if (openState) {
      return;
    }
    openState = true;
    syncHeader();
  };

  const handleKeyDown = (event: {
    readonly key: string;
    preventDefault?: () => void;
  }): boolean => {
    if (openState && event.key === "Escape") {
      event.preventDefault?.();
      close();
      return true;
    }
    return false;
  };

  toggleBtn.addEventListener("click", () => {
    toggleDisclosure();
  });
  closeBtn.addEventListener("click", () => {
    close();
  });
  aside.addEventListener("keydown", (ev: KeyboardEvent) => {
    handleKeyDown(ev);
  });

  actions.appendChild(toggleBtn);
  actions.appendChild(closeBtn);
  header.appendChild(nav);
  header.appendChild(actions);
  aside.appendChild(header);
  aside.appendChild(bodyElement);

  syncHeader();

  return {
    element: aside,
    bodyElement,
    isOpen: () => openState,
    getDisclosure: () => disclosure,
    getPlacement: () => placement,
    getVisibleBreadcrumbs: () => filterBreadcrumbsForDisclosure(breadcrumbs, disclosure),
    setDisclosure,
    toggleDisclosure,
    setBreadcrumbs: (items: readonly ScopeBreadcrumbItem[]) => {
      breadcrumbs = [...items];
      syncHeader();
    },
    open,
    close,
    handleKeyDown,
  };
}
