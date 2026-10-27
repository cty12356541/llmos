// Stateful Deterministic Tiling Window Manager & Coupled Surface Lifecycle Controller.
// Authority: ADR-0025 Decision 1 + Stage D Desktop Visual & UX Spec §2.3, §2.4, §3.3.
// Coupled State Machines:
//   - Window:  CREATED -> VISIBLE <-> MINIMIZED -> CLOSING -> CLOSED
//   - Surface: REGISTERED -> CREATED -> PRESENTED <-> HIDDEN -> CLOSED

import type { DisclosureLevel, IconName, WorkstationLocale } from "../ui";
import {
  DEFAULT_SPLIT_WIDTH_RATIO,
  DEFAULT_THREE_PANE_TOP_HEIGHT_RATIO,
  solveWorkspaceGeometry,
  type SolvedWindowGeometry,
  type SolvedWorkspaceGeometry,
  type SplitSashGeometry,
  type WmLayoutMode,
  type WmRectPx,
} from "./layout-solver";
import {
  createWindowFrameElement,
  isWindowGenerationStale,
  renderWindowFrameHtml,
} from "./window-frame";

export type WindowLifecycleState =
  | "CREATED"
  | "VISIBLE"
  | "MINIMIZED"
  | "CLOSING"
  | "CLOSED";

export type SurfaceLifecycleState =
  | "REGISTERED"
  | "CREATED"
  | "PRESENTED"
  | "HIDDEN"
  | "CLOSED";

export type LayoutShortcutDigit = 1 | 2 | 3 | 4;
export type SnapDirection = "left" | "right" | "up" | "down";

export type InputRouteDenialReason =
  | "Z4_MODAL_LOCK"
  | "STALE_GENERATION"
  | "WINDOW_NOT_VISIBLE"
  | "OCCLUDED_BY_SYSTEM_PLANE"
  | "NOT_FOUND";

export interface WindowInputRouteDecision {
  readonly allowed: boolean;
  readonly reason?: InputRouteDenialReason;
}

export interface ManagedWindowDescriptor {
  readonly windowId: string;
  readonly surfaceId: string;
  readonly applicationIdHex: string;
  readonly installationIdHex?: string;
  readonly title: string;
  readonly appIcon?: IconName;
  readonly surfaceGeneration: number;
  readonly latestInstallationGeneration?: number;
  readonly isFloating?: boolean;
  readonly floatingRect?: WmRectPx;
  readonly preferredSlot?: string;
  readonly srcdoc?: string;
  readonly iframeSrc?: string;
  readonly presentImmediately?: boolean;
}

export interface ManagedWindowRecord {
  readonly windowId: string;
  readonly surfaceId: string;
  readonly applicationIdHex: string;
  readonly installationIdHex?: string;
  readonly title: string;
  readonly appIcon: IconName;
  readonly windowState: WindowLifecycleState;
  readonly surfaceState: SurfaceLifecycleState;
  readonly surfaceGeneration: number;
  readonly latestInstallationGeneration: number;
  readonly staleGeneration: boolean;
  readonly isFloating: boolean;
  readonly floatingRect?: WmRectPx;
  readonly preferredSlot?: string;
  readonly srcdoc?: string;
  readonly iframeSrc?: string;
}

export interface WindowManagerSnapshot {
  readonly layoutMode: WmLayoutMode;
  readonly previousTiledLayoutMode: WmLayoutMode;
  readonly viewportWidth: number;
  readonly viewportHeight: number;
  readonly topHeightRatio: number;
  readonly splitWidthRatio: number;
  readonly showZ1TaskSpace: boolean;
  readonly z4ModalLock: boolean;
  readonly activeZ3OverlayRects: readonly WmRectPx[];
  readonly focusedWindowId: string | null;
  readonly mruWindowIds: readonly string[];
  readonly windows: readonly ManagedWindowRecord[];
  readonly geometry: SolvedWorkspaceGeometry;
  readonly locale: WorkstationLocale;
  readonly disclosure: DisclosureLevel;
}

export interface TilingWindowManagerConfig {
  readonly viewportWidth?: number;
  readonly viewportHeight?: number;
  readonly initialLayoutMode?: WmLayoutMode;
  readonly topHeightRatio?: number;
  readonly splitWidthRatio?: number;
  readonly showZ1TaskSpace?: boolean;
  readonly locale?: WorkstationLocale;
  readonly disclosure?: DisclosureLevel;
}

export type WindowManagerListener = (snapshot: WindowManagerSnapshot) => void;

export interface TilingWindowManager {
  getSnapshot(): WindowManagerSnapshot;
  subscribe(listener: WindowManagerListener): () => void;
  registerWindow(descriptor: ManagedWindowDescriptor): ManagedWindowRecord;
  presentWindow(windowId: string): boolean;
  minimizeWindow(windowId: string): boolean;
  restoreWindow(windowId: string): boolean;
  focusWindow(windowId: string): boolean;
  beginCloseWindow(windowId: string): boolean;
  closeWindow(windowId: string): boolean;
  toggleWindowFloating(windowId: string): boolean;
  setWindowFloatingRect(windowId: string, rect: WmRectPx): boolean;
  maximizeWindow(windowId: string): boolean;
  snapWindow(windowId: string, direction: SnapDirection): boolean;
  setLayoutMode(mode: WmLayoutMode): void;
  applyLayoutShortcut(digit: LayoutShortcutDigit): WmLayoutMode;
  setSplitRatios(ratios: { topHeightRatio?: number; splitWidthRatio?: number }): void;
  nudgeSplitRatio(orientation: "horizontal" | "vertical", delta: number): void;
  setShowZ1TaskSpace(visible: boolean): void;
  setViewportSize(width: number, height: number): void;
  setSystemOverlayState(state: {
    z4ModalLock?: boolean;
    activeZ3OverlayRects?: readonly WmRectPx[];
  }): void;
  setLocale(locale: WorkstationLocale): void;
  setDisclosure(disclosure: DisclosureLevel): void;
  advanceApplicationGeneration(applicationIdHex: string, newGeneration: number): number;
  markWindowStale(windowId: string, latestGeneration?: number): boolean;
  reloadWindowGeneration(
    windowId: string,
    options?: { newSurfaceId?: string; newGeneration?: number; srcdoc?: string },
  ): boolean;
  canRouteInputToWindow(windowId: string): WindowInputRouteDecision;
  renderZ2PlaneHtml(): string;
  renderZ2PlaneElement(options?: { doc?: Document }): HTMLElement;
}

export function layoutModeForShortcut(digit: LayoutShortcutDigit): WmLayoutMode {
  switch (digit) {
    case 1:
      return "workspace-maximize";
    case 2:
      return "two-column-split";
    case 3:
      return "three-pane-workstation";
    case 4:
      return "quadrants";
  }
}

interface InternalWindowEntry {
  windowId: string;
  surfaceId: string;
  applicationIdHex: string;
  installationIdHex?: string;
  title: string;
  appIcon: IconName;
  windowState: WindowLifecycleState;
  surfaceState: SurfaceLifecycleState;
  surfaceGeneration: number;
  latestInstallationGeneration: number;
  staleGeneration: boolean;
  isFloating: boolean;
  floatingRect?: WmRectPx;
  preferredSlot?: string;
  srcdoc?: string;
  iframeSrc?: string;
}

function clampRatio(val: number, min: number, max: number): number {
  if (!Number.isFinite(val)) {
    return (min + max) * 0.5;
  }
  return Math.min(max, Math.max(min, val));
}

function toPublicRecord(entry: InternalWindowEntry): ManagedWindowRecord {
  return {
    windowId: entry.windowId,
    surfaceId: entry.surfaceId,
    applicationIdHex: entry.applicationIdHex,
    installationIdHex: entry.installationIdHex,
    title: entry.title,
    appIcon: entry.appIcon,
    windowState: entry.windowState,
    surfaceState: entry.surfaceState,
    surfaceGeneration: entry.surfaceGeneration,
    latestInstallationGeneration: entry.latestInstallationGeneration,
    staleGeneration: entry.staleGeneration,
    isFloating: entry.isFloating,
    floatingRect: entry.floatingRect ? { ...entry.floatingRect } : undefined,
    preferredSlot: entry.preferredSlot,
    srcdoc: entry.srcdoc,
    iframeSrc: entry.iframeSrc,
  };
}

export function createTilingWindowManager(
  config?: TilingWindowManagerConfig,
): TilingWindowManager {
  let viewportWidth = Math.max(0, Math.floor(config?.viewportWidth ?? 1440));
  let viewportHeight = Math.max(0, Math.floor(config?.viewportHeight ?? 900));
  let layoutMode: WmLayoutMode = config?.initialLayoutMode ?? "three-pane-workstation";
  let previousTiledLayoutMode: WmLayoutMode =
    layoutMode !== "workspace-maximize" && layoutMode !== "floating"
      ? layoutMode
      : "three-pane-workstation";

  let topHeightRatio = clampRatio(
    config?.topHeightRatio ?? DEFAULT_THREE_PANE_TOP_HEIGHT_RATIO,
    0.25,
    0.85,
  );
  let splitWidthRatio = clampRatio(
    config?.splitWidthRatio ?? DEFAULT_SPLIT_WIDTH_RATIO,
    0.2,
    0.8,
  );
  let showZ1TaskSpace =
    config?.showZ1TaskSpace ?? (layoutMode === "three-pane-workstation");
  let z4ModalLock = false;
  let activeZ3OverlayRects: WmRectPx[] = [];
  let locale: WorkstationLocale = config?.locale ?? "zh-CN";
  let disclosure: DisclosureLevel = config?.disclosure ?? "L1";

  const entries = new Map<string, InternalWindowEntry>();
  const appLatestGeneration = new Map<string, number>();
  let mruWindowIds: string[] = [];
  let focusedWindowId: string | null = null;
  const listeners = new Set<WindowManagerListener>();

  const promoteMru = (windowId: string): void => {
    mruWindowIds = [windowId, ...mruWindowIds.filter((id) => id !== windowId)];
  };

  const recomputeFocusFallback = (): void => {
    if (focusedWindowId !== null) {
      const cur = entries.get(focusedWindowId);
      if (cur && cur.windowState === "VISIBLE") {
        return;
      }
    }
    const nextVisible = mruWindowIds.find((id) => {
      const w = entries.get(id);
      return w !== undefined && w.windowState === "VISIBLE";
    });
    focusedWindowId = nextVisible ?? null;
  };

  const buildSnapshot = (): WindowManagerSnapshot => {
    const activeEntries = [...entries.values()].filter(
      (e) => e.windowState !== "CLOSED" && e.windowState !== "CLOSING",
    );
    const geometry = solveWorkspaceGeometry({
      viewportWidth,
      viewportHeight,
      layoutMode,
      windows: activeEntries.map((e) => ({
        windowId: e.windowId,
        minimized: e.windowState !== "VISIBLE",
        isFloating: e.isFloating,
        floatingRect: e.floatingRect,
        preferredSlot: e.preferredSlot,
      })),
      mruWindowIds,
      focusedWindowId,
      topHeightRatio,
      splitWidthRatio,
      showZ1TaskSpace,
      z4ModalLock,
      activeZ3OverlayRects,
    });

    return {
      layoutMode,
      previousTiledLayoutMode,
      viewportWidth,
      viewportHeight,
      topHeightRatio,
      splitWidthRatio,
      showZ1TaskSpace,
      z4ModalLock,
      activeZ3OverlayRects: activeZ3OverlayRects.map((r) => ({ ...r })),
      focusedWindowId,
      mruWindowIds: [...mruWindowIds],
      windows: [...entries.values()].map(toPublicRecord),
      geometry,
      locale,
      disclosure,
    };
  };

  const emit = (): void => {
    if (listeners.size === 0) {
      return;
    }
    const snap = buildSnapshot();
    for (const listener of listeners) {
      listener(snap);
    }
  };

  const registerWindow = (descriptor: ManagedWindowDescriptor): ManagedWindowRecord => {
    const appKey = descriptor.applicationIdHex.trim().toLowerCase();
    const knownGen = appLatestGeneration.get(appKey) ?? descriptor.surfaceGeneration;
    const latestGen = Math.max(
      knownGen,
      descriptor.latestInstallationGeneration ?? descriptor.surfaceGeneration,
    );
    appLatestGeneration.set(appKey, latestGen);

    const stale = isWindowGenerationStale({
      surfaceGeneration: descriptor.surfaceGeneration,
      latestInstallationGeneration: latestGen,
    });

    // Coupled Surface: REGISTERED -> CREATED; Window: CREATED
    const entry: InternalWindowEntry = {
      windowId: descriptor.windowId,
      surfaceId: descriptor.surfaceId,
      applicationIdHex: descriptor.applicationIdHex,
      installationIdHex: descriptor.installationIdHex,
      title: descriptor.title,
      appIcon: descriptor.appIcon ?? "layout-1-pane",
      windowState: "CREATED",
      surfaceState: "CREATED",
      surfaceGeneration: descriptor.surfaceGeneration,
      latestInstallationGeneration: latestGen,
      staleGeneration: stale,
      isFloating: Boolean(descriptor.isFloating),
      floatingRect: descriptor.floatingRect ? { ...descriptor.floatingRect } : undefined,
      preferredSlot: descriptor.preferredSlot,
      srcdoc: descriptor.srcdoc,
      iframeSrc: descriptor.iframeSrc,
    };
    entries.set(entry.windowId, entry);

    if (descriptor.presentImmediately !== false) {
      entry.windowState = "VISIBLE";
      entry.surfaceState = "PRESENTED";
      promoteMru(entry.windowId);
      if (!z4ModalLock && !entry.staleGeneration) {
        focusedWindowId = entry.windowId;
      } else if (focusedWindowId === null) {
        focusedWindowId = entry.windowId;
      }
    } else {
      mruWindowIds = [...mruWindowIds.filter((id) => id !== entry.windowId), entry.windowId];
    }

    emit();
    return toPublicRecord(entry);
  };

  const presentWindow = (windowId: string): boolean => {
    const entry = entries.get(windowId);
    if (!entry || entry.windowState === "CLOSED" || entry.windowState === "CLOSING") {
      return false;
    }
    entry.windowState = "VISIBLE";
    entry.surfaceState = "PRESENTED";
    promoteMru(windowId);
    if (!z4ModalLock) {
      focusedWindowId = windowId;
    }
    emit();
    return true;
  };

  const minimizeWindow = (windowId: string): boolean => {
    const entry = entries.get(windowId);
    if (!entry || entry.windowState === "CLOSED" || entry.windowState === "CLOSING") {
      return false;
    }
    if (entry.windowState === "CREATED") {
      return false;
    }
    entry.windowState = "MINIMIZED";
    entry.surfaceState = "HIDDEN";
    if (focusedWindowId === windowId) {
      focusedWindowId = null;
      recomputeFocusFallback();
    }
    emit();
    return true;
  };

  const restoreWindow = (windowId: string): boolean => {
    return presentWindow(windowId);
  };

  const focusWindow = (windowId: string): boolean => {
    if (z4ModalLock) {
      return false;
    }
    const entry = entries.get(windowId);
    if (!entry || entry.windowState === "CLOSED" || entry.windowState === "CLOSING") {
      return false;
    }
    if (entry.windowState === "MINIMIZED" || entry.windowState === "CREATED") {
      entry.windowState = "VISIBLE";
      entry.surfaceState = "PRESENTED";
    }
    promoteMru(windowId);
    focusedWindowId = windowId;
    emit();
    return true;
  };

  const beginCloseWindow = (windowId: string): boolean => {
    const entry = entries.get(windowId);
    if (!entry || entry.windowState === "CLOSED") {
      return false;
    }
    entry.windowState = "CLOSING";
    if (entry.surfaceState === "PRESENTED") {
      entry.surfaceState = "HIDDEN";
    }
    if (focusedWindowId === windowId) {
      focusedWindowId = null;
      recomputeFocusFallback();
    }
    emit();
    return true;
  };

  const closeWindow = (windowId: string): boolean => {
    const entry = entries.get(windowId);
    if (!entry || entry.windowState === "CLOSED") {
      return false;
    }
    entry.windowState = "CLOSING";
    entry.windowState = "CLOSED";
    entry.surfaceState = "CLOSED";
    mruWindowIds = mruWindowIds.filter((id) => id !== windowId);
    if (focusedWindowId === windowId) {
      focusedWindowId = null;
      recomputeFocusFallback();
    }
    emit();
    return true;
  };

  const setLayoutMode = (mode: WmLayoutMode): void => {
    if (mode !== "workspace-maximize" && mode !== "floating") {
      previousTiledLayoutMode = mode;
    }
    layoutMode = mode;
    if (mode === "three-pane-workstation") {
      showZ1TaskSpace = true;
    }
    emit();
  };

  const applyLayoutShortcut = (digit: LayoutShortcutDigit): WmLayoutMode => {
    const nextMode = layoutModeForShortcut(digit);
    setLayoutMode(nextMode);
    return nextMode;
  };

  const toggleWindowFloating = (windowId: string): boolean => {
    const entry = entries.get(windowId);
    if (!entry || entry.windowState === "CLOSED" || entry.windowState === "CLOSING") {
      return false;
    }
    entry.isFloating = !entry.isFloating;
    if (!entry.isFloating && layoutMode === "floating") {
      layoutMode = previousTiledLayoutMode;
    }
    promoteMru(windowId);
    if (!z4ModalLock && entry.windowState === "VISIBLE") {
      focusedWindowId = windowId;
    }
    emit();
    return true;
  };

  const setWindowFloatingRect = (windowId: string, rect: WmRectPx): boolean => {
    const entry = entries.get(windowId);
    if (!entry || entry.windowState === "CLOSED" || entry.windowState === "CLOSING") {
      return false;
    }
    entry.isFloating = true;
    entry.floatingRect = { ...rect };
    emit();
    return true;
  };

  const maximizeWindow = (windowId: string): boolean => {
    const entry = entries.get(windowId);
    if (!entry || entry.windowState === "CLOSED" || entry.windowState === "CLOSING") {
      return false;
    }
    entry.isFloating = false;
    if (entry.windowState !== "VISIBLE") {
      entry.windowState = "VISIBLE";
      entry.surfaceState = "PRESENTED";
    }
    promoteMru(windowId);
    if (!z4ModalLock) {
      focusedWindowId = windowId;
    }
    if (layoutMode === "workspace-maximize") {
      layoutMode = previousTiledLayoutMode;
    } else {
      if (layoutMode !== "floating") {
        previousTiledLayoutMode = layoutMode;
      }
      layoutMode = "workspace-maximize";
    }
    emit();
    return true;
  };

  const snapWindow = (windowId: string, direction: SnapDirection): boolean => {
    const entry = entries.get(windowId);
    if (!entry || entry.windowState === "CLOSED" || entry.windowState === "CLOSING") {
      return false;
    }

    switch (direction) {
      case "left": {
        entry.isFloating = false;
        entry.windowState = "VISIBLE";
        entry.surfaceState = "PRESENTED";
        if (layoutMode === "workspace-maximize" || layoutMode === "floating") {
          layoutMode = "two-column-split";
          previousTiledLayoutMode = "two-column-split";
        }
        entry.preferredSlot =
          layoutMode === "quadrants"
            ? "TL"
            : layoutMode === "three-pane-workstation"
              ? "top-left"
              : "col-left";
        promoteMru(windowId);
        if (!z4ModalLock) {
          focusedWindowId = windowId;
        }
        emit();
        return true;
      }
      case "right": {
        entry.isFloating = false;
        entry.windowState = "VISIBLE";
        entry.surfaceState = "PRESENTED";
        if (layoutMode === "workspace-maximize" || layoutMode === "floating") {
          layoutMode = "two-column-split";
          previousTiledLayoutMode = "two-column-split";
        }
        entry.preferredSlot =
          layoutMode === "quadrants"
            ? "TR"
            : layoutMode === "three-pane-workstation"
              ? "top-right"
              : "col-right";
        promoteMru(windowId);
        if (!z4ModalLock) {
          focusedWindowId = windowId;
        }
        emit();
        return true;
      }
      case "up": {
        entry.isFloating = false;
        entry.windowState = "VISIBLE";
        entry.surfaceState = "PRESENTED";
        if (layoutMode !== "workspace-maximize" && layoutMode !== "floating") {
          previousTiledLayoutMode = layoutMode;
        }
        layoutMode = "workspace-maximize";
        promoteMru(windowId);
        if (!z4ModalLock) {
          focusedWindowId = windowId;
        }
        emit();
        return true;
      }
      case "down": {
        if (layoutMode === "workspace-maximize") {
          layoutMode = previousTiledLayoutMode;
          emit();
          return true;
        }
        return minimizeWindow(windowId);
      }
    }
  };

  const setSplitRatios = (ratios: {
    topHeightRatio?: number;
    splitWidthRatio?: number;
  }): void => {
    if (ratios.topHeightRatio !== undefined) {
      topHeightRatio = clampRatio(ratios.topHeightRatio, 0.25, 0.85);
    }
    if (ratios.splitWidthRatio !== undefined) {
      splitWidthRatio = clampRatio(ratios.splitWidthRatio, 0.2, 0.8);
    }
    emit();
  };

  const nudgeSplitRatio = (
    orientation: "horizontal" | "vertical",
    delta: number,
  ): void => {
    if (orientation === "vertical") {
      splitWidthRatio = clampRatio(splitWidthRatio + delta, 0.2, 0.8);
    } else {
      topHeightRatio = clampRatio(topHeightRatio + delta, 0.25, 0.85);
    }
    emit();
  };

  const setShowZ1TaskSpace = (visible: boolean): void => {
    showZ1TaskSpace = visible;
    emit();
  };

  const setViewportSize = (width: number, height: number): void => {
    viewportWidth = Math.max(0, Math.floor(width));
    viewportHeight = Math.max(0, Math.floor(height));
    emit();
  };

  const setSystemOverlayState = (state: {
    z4ModalLock?: boolean;
    activeZ3OverlayRects?: readonly WmRectPx[];
  }): void => {
    if (state.z4ModalLock !== undefined) {
      z4ModalLock = state.z4ModalLock;
    }
    if (state.activeZ3OverlayRects !== undefined) {
      activeZ3OverlayRects = state.activeZ3OverlayRects.map((r) => ({ ...r }));
    }
    emit();
  };

  const setLocale = (nextLocale: WorkstationLocale): void => {
    locale = nextLocale;
    emit();
  };

  const setDisclosure = (nextDisclosure: DisclosureLevel): void => {
    disclosure = nextDisclosure;
    emit();
  };

  const advanceApplicationGeneration = (
    applicationIdHex: string,
    newGeneration: number,
  ): number => {
    const appKey = applicationIdHex.trim().toLowerCase();
    const prev = appLatestGeneration.get(appKey) ?? 0;
    const target = Math.max(prev, newGeneration);
    appLatestGeneration.set(appKey, target);

    let markedCount = 0;
    for (const entry of entries.values()) {
      if (
        entry.windowState !== "CLOSED" &&
        entry.applicationIdHex.trim().toLowerCase() === appKey
      ) {
        entry.latestInstallationGeneration = target;
        if (entry.surfaceGeneration < target && !entry.staleGeneration) {
          entry.staleGeneration = true;
          markedCount += 1;
        }
      }
    }
    emit();
    return markedCount;
  };

  const markWindowStale = (windowId: string, latestGeneration?: number): boolean => {
    const entry = entries.get(windowId);
    if (!entry || entry.windowState === "CLOSED") {
      return false;
    }
    const targetGen =
      latestGeneration !== undefined && latestGeneration > entry.surfaceGeneration
        ? latestGeneration
        : entry.surfaceGeneration + 1;
    entry.latestInstallationGeneration = targetGen;
    entry.staleGeneration = true;
    emit();
    return true;
  };

  const reloadWindowGeneration = (
    windowId: string,
    options?: { newSurfaceId?: string; newGeneration?: number; srcdoc?: string },
  ): boolean => {
    const entry = entries.get(windowId);
    if (!entry || entry.windowState === "CLOSED") {
      return false;
    }
    const nextGen =
      options?.newGeneration ??
      Math.max(entry.latestInstallationGeneration, entry.surfaceGeneration + 1);
    entry.surfaceGeneration = nextGen;
    entry.latestInstallationGeneration = Math.max(entry.latestInstallationGeneration, nextGen);
    entry.staleGeneration = false;
    if (options?.newSurfaceId !== undefined) {
      entry.surfaceId = options.newSurfaceId;
    }
    if (options?.srcdoc !== undefined) {
      entry.srcdoc = options.srcdoc;
    }
    const appKey = entry.applicationIdHex.trim().toLowerCase();
    appLatestGeneration.set(
      appKey,
      Math.max(appLatestGeneration.get(appKey) ?? 0, entry.latestInstallationGeneration),
    );
    emit();
    return true;
  };

  const canRouteInputToWindow = (windowId: string): WindowInputRouteDecision => {
    if (z4ModalLock) {
      return { allowed: false, reason: "Z4_MODAL_LOCK" };
    }
    const entry = entries.get(windowId);
    if (!entry) {
      return { allowed: false, reason: "NOT_FOUND" };
    }
    if (entry.windowState !== "VISIBLE" || entry.surfaceState !== "PRESENTED") {
      return { allowed: false, reason: "WINDOW_NOT_VISIBLE" };
    }
    if (entry.staleGeneration || entry.surfaceGeneration < entry.latestInstallationGeneration) {
      return { allowed: false, reason: "STALE_GENERATION" };
    }
    const snap = buildSnapshot();
    const solved = snap.geometry.windows.find((w) => w.windowId === windowId);
    if (!solved) {
      return { allowed: false, reason: "WINDOW_NOT_VISIBLE" };
    }
    if (solved.occludedBySystemPlane) {
      return { allowed: false, reason: "OCCLUDED_BY_SYSTEM_PLANE" };
    }
    return { allowed: true };
  };

  const renderSashHtml = (sash: SplitSashGeometry): string => {
    const ariaLabel =
      sash.orientation === "vertical"
        ? locale === "en-US"
          ? "Adjust column split"
          : "调节左右分栏比例"
        : locale === "en-US"
          ? "Adjust row split"
          : "调节上下分栏比例";
    return (
      `<div role="separator" tabindex="0" ` +
      `aria-orientation="${sash.orientation}" ` +
      `aria-label="${ariaLabel}" ` +
      `class="nlos-split-sash nlos-split-sash--${sash.orientation}" ` +
      `data-sash-id="${sash.id}" ` +
      `style="left: ${sash.rect.x}px; top: ${sash.rect.y}px; width: ${sash.rect.width}px; height: ${sash.rect.height}px;"></div>`
    );
  };

  const renderZ2PlaneHtml = (): string => {
    const snap = buildSnapshot();
    const geomById = new Map<string, SolvedWindowGeometry>(
      snap.geometry.windows.map((g) => [g.windowId, g]),
    );

    const framesHtml = snap.windows
      .filter((w) => w.windowState === "VISIBLE" && geomById.has(w.windowId))
      .map((w) =>
        renderWindowFrameHtml({
          windowId: w.windowId,
          surfaceId: w.surfaceId,
          applicationIdHex: w.applicationIdHex,
          title: w.title,
          appIcon: w.appIcon,
          surfaceGeneration: w.surfaceGeneration,
          latestInstallationGeneration: w.latestInstallationGeneration,
          staleGeneration: w.staleGeneration,
          focused: snap.focusedWindowId === w.windowId,
          isFloating: w.isFloating,
          isMaximized: snap.layoutMode === "workspace-maximize" && !w.isFloating,
          geometry: geomById.get(w.windowId),
          srcdoc: w.srcdoc,
          iframeSrc: w.iframeSrc,
          locale: snap.locale,
          disclosure: snap.disclosure,
        }),
      )
      .join("");

    const sashesHtml = snap.geometry.splitSashes.map(renderSashHtml).join("");
    return `<div class="nlos-plane-z2" data-layout-mode="${snap.layoutMode}">${framesHtml}${sashesHtml}</div>`;
  };

  const renderZ2PlaneElement = (options?: { doc?: Document }): HTMLElement => {
    const doc = options?.doc ?? document;
    const snap = buildSnapshot();
    const geomById = new Map<string, SolvedWindowGeometry>(
      snap.geometry.windows.map((g) => [g.windowId, g]),
    );

    const container = doc.createElement("div");
    container.className = "nlos-plane-z2";
    container.setAttribute("data-layout-mode", snap.layoutMode);

    for (const w of snap.windows) {
      const geom = geomById.get(w.windowId);
      if (w.windowState !== "VISIBLE" || !geom) {
        continue;
      }
      const frameEl = createWindowFrameElement({
        doc,
        windowId: w.windowId,
        surfaceId: w.surfaceId,
        applicationIdHex: w.applicationIdHex,
        title: w.title,
        appIcon: w.appIcon,
        surfaceGeneration: w.surfaceGeneration,
        latestInstallationGeneration: w.latestInstallationGeneration,
        staleGeneration: w.staleGeneration,
        focused: snap.focusedWindowId === w.windowId,
        isFloating: w.isFloating,
        isMaximized: snap.layoutMode === "workspace-maximize" && !w.isFloating,
        geometry: geom,
        srcdoc: w.srcdoc,
        iframeSrc: w.iframeSrc,
        locale: snap.locale,
        disclosure: snap.disclosure,
        callbacks: {
          onFocus: (id) => {
            focusWindow(id);
          },
          onMinimize: (id) => {
            minimizeWindow(id);
          },
          onToggleFloat: (id) => {
            toggleWindowFloating(id);
          },
          onMaximize: (id) => {
            maximizeWindow(id);
          },
          onClose: (id) => {
            closeWindow(id);
          },
          onReloadLatestGeneration: (id) => {
            reloadWindowGeneration(id);
          },
        },
      });
      container.appendChild(frameEl);
    }

    for (const sash of snap.geometry.splitSashes) {
      const sashEl = doc.createElement("div");
      sashEl.className = `nlos-split-sash nlos-split-sash--${sash.orientation}`;
      sashEl.setAttribute("role", "separator");
      sashEl.tabIndex = 0;
      sashEl.setAttribute("aria-orientation", sash.orientation);
      sashEl.setAttribute(
        "aria-label",
        sash.orientation === "vertical"
          ? snap.locale === "en-US"
            ? "Adjust column split"
            : "调节左右分栏比例"
          : snap.locale === "en-US"
            ? "Adjust row split"
            : "调节上下分栏比例",
      );
      sashEl.setAttribute("data-sash-id", sash.id);
      sashEl.style.left = `${sash.rect.x}px`;
      sashEl.style.top = `${sash.rect.y}px`;
      sashEl.style.width = `${sash.rect.width}px`;
      sashEl.style.height = `${sash.rect.height}px`;

      sashEl.addEventListener("keydown", (ev) => {
        if (sash.orientation === "vertical") {
          if (ev.key === "ArrowLeft") {
            ev.preventDefault();
            nudgeSplitRatio("vertical", -0.04);
          } else if (ev.key === "ArrowRight") {
            ev.preventDefault();
            nudgeSplitRatio("vertical", 0.04);
          }
        } else {
          if (ev.key === "ArrowUp") {
            ev.preventDefault();
            nudgeSplitRatio("horizontal", -0.04);
          } else if (ev.key === "ArrowDown") {
            ev.preventDefault();
            nudgeSplitRatio("horizontal", 0.04);
          }
        }
      });

      container.appendChild(sashEl);
    }

    return container;
  };

  return {
    getSnapshot: buildSnapshot,
    subscribe(listener: WindowManagerListener): () => void {
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
      };
    },
    registerWindow,
    presentWindow,
    minimizeWindow,
    restoreWindow,
    focusWindow,
    beginCloseWindow,
    closeWindow,
    toggleWindowFloating,
    setWindowFloatingRect,
    maximizeWindow,
    snapWindow,
    setLayoutMode,
    applyLayoutShortcut,
    setSplitRatios,
    nudgeSplitRatio,
    setShowZ1TaskSpace,
    setViewportSize,
    setSystemOverlayState,
    setLocale,
    setDisclosure,
    advanceApplicationGeneration,
    markWindowStale,
    reloadWindowGeneration,
    canRouteInputToWindow,
    renderZ2PlaneHtml,
    renderZ2PlaneElement,
  };
}
