// Deterministic 5-Plane Workstation & Tiling Window Geometry Solver.
// Authority: ADR-0025 Decision 1 & 3 + Stage D Desktop Visual & UX Spec §2.1–§2.3, §3.3.

export const TASKBAR_HEIGHT_PX = 36;
export const WINDOW_TITLEBAR_HEIGHT_PX = 30;
export const TILE_GAP_PX = 4;

export const Z0_Z_INDEX = 0;
export const Z1_Z_INDEX = 100;
export const Z2_Z_INDEX_MIN = 200;
export const Z2_Z_INDEX_MAX = 299;
export const Z3_TASKBAR_Z_INDEX = 400;
export const Z3_FLYOUT_Z_INDEX = 430;
export const Z3_PALETTE_Z_INDEX = 460;
export const Z4_SECURITY_Z_INDEX = 900;

export const MIN_FLOATING_WIDTH_PX = 320;
export const MIN_FLOATING_HEIGHT_PX = 180;
export const DEFAULT_THREE_PANE_TOP_HEIGHT_RATIO = 0.68;
export const DEFAULT_QUADRANT_TOP_HEIGHT_RATIO = 0.5;
export const DEFAULT_SPLIT_WIDTH_RATIO = 0.5;

export type WmLayoutMode =
  | "three-pane-workstation"
  | "two-column-split"
  | "quadrants"
  | "workspace-maximize"
  | "floating";

export const WM_LAYOUT_MODES: readonly WmLayoutMode[] = [
  "workspace-maximize",
  "two-column-split",
  "three-pane-workstation",
  "quadrants",
  "floating",
];

export interface WmRectPx {
  x: number;
  y: number;
  width: number;
  height: number;
}

export interface SplitSashGeometry {
  id: string;
  orientation: "horizontal" | "vertical";
  rect: WmRectPx;
}

export interface SolvedWindowGeometry {
  windowId: string;
  outerFrameRect: WmRectPx;
  innerContentClipRect: WmRectPx;
  borderRadiusPx: 0 | 6;
  zIndex: number;
  layoutSlot: string;
  isFloating: boolean;
  occludedBySystemPlane: boolean;
}

export interface SolvedWorkspaceGeometry {
  viewportRect: WmRectPx;
  stageRect: WmRectPx;
  taskbarRect: WmRectPx;
  z1TaskSpaceRect: WmRectPx | null;
  windows: SolvedWindowGeometry[];
  splitSashes: Array<{
    id: string;
    orientation: "horizontal" | "vertical";
    rect: WmRectPx;
  }>;
}

export interface SolverWindowInput {
  readonly windowId: string;
  readonly minimized?: boolean;
  readonly isFloating?: boolean;
  readonly floatingRect?: WmRectPx;
  readonly preferredSlot?: string;
}

export interface SolveWorkspaceGeometryInput {
  readonly viewportWidth: number;
  readonly viewportHeight: number;
  readonly layoutMode: WmLayoutMode;
  readonly windows: readonly SolverWindowInput[];
  /** MRU stack ordered from most-recently focused (index 0) to least-recently focused. */
  readonly mruWindowIds?: readonly string[];
  readonly focusedWindowId?: string | null;
  readonly topHeightRatio?: number;
  readonly splitWidthRatio?: number;
  readonly showZ1TaskSpace?: boolean;
  readonly z4ModalLock?: boolean;
  readonly activeZ3OverlayRects?: readonly WmRectPx[];
}

function clampRatio(raw: number, min: number, max: number): number {
  if (!Number.isFinite(raw)) {
    return (min + max) * 0.5;
  }
  return Math.min(max, Math.max(min, raw));
}

export function rectsIntersect(a: WmRectPx, b: WmRectPx): boolean {
  if (a.width <= 0 || a.height <= 0 || b.width <= 0 || b.height <= 0) {
    return false;
  }
  return (
    a.x < b.x + b.width &&
    a.x + a.width > b.x &&
    a.y < b.y + b.height &&
    a.y + a.height > b.y
  );
}

/** Clamps a floating window candidate strictly inside stageRect (above the 36px Z3 taskbar). */
export function clampFloatingRectToStage(
  candidate: WmRectPx,
  stageRect: WmRectPx,
): WmRectPx {
  const minW = Math.min(MIN_FLOATING_WIDTH_PX, stageRect.width);
  const minH = Math.min(MIN_FLOATING_HEIGHT_PX, stageRect.height);

  const safeCandidateW = Number.isFinite(candidate.width) ? Math.round(candidate.width) : minW;
  const safeCandidateH = Number.isFinite(candidate.height) ? Math.round(candidate.height) : minH;

  const width = Math.min(stageRect.width, Math.max(minW, safeCandidateW));
  const height = Math.min(stageRect.height, Math.max(minH, safeCandidateH));

  const maxX = Math.max(stageRect.x, stageRect.x + stageRect.width - width);
  const maxY = Math.max(stageRect.y, stageRect.y + stageRect.height - height);

  const safeCandidateX = Number.isFinite(candidate.x) ? Math.round(candidate.x) : stageRect.x;
  const safeCandidateY = Number.isFinite(candidate.y) ? Math.round(candidate.y) : stageRect.y;

  const x = Math.min(maxX, Math.max(stageRect.x, safeCandidateX));
  const y = Math.min(maxY, Math.max(stageRect.y, safeCandidateY));

  return { x, y, width, height };
}

/** Computes inner content clip rect strictly below the 30px trusted titlebar and within stageRect. */
export function computeInnerContentClipRect(
  outerFrameRect: WmRectPx,
  stageRect: WmRectPx,
): WmRectPx {
  const maxRight = stageRect.x + stageRect.width;
  const maxBottom = stageRect.y + stageRect.height;

  const clampedOuterX = Math.max(stageRect.x, Math.min(maxRight, outerFrameRect.x));
  const innerX = outerFrameRect.width >= 2 ? clampedOuterX + 1 : clampedOuterX;
  const rawInnerW = Math.max(0, outerFrameRect.width - 2);
  const innerWidth = Math.max(0, Math.min(rawInnerW, maxRight - innerX));

  const rawInnerY = outerFrameRect.y + WINDOW_TITLEBAR_HEIGHT_PX;
  const innerY = Math.max(stageRect.y, Math.min(maxBottom, rawInnerY));
  const rawInnerH = Math.max(0, outerFrameRect.height - (WINDOW_TITLEBAR_HEIGHT_PX + 1));
  const innerHeight = Math.max(0, Math.min(rawInnerH, maxBottom - innerY));

  return {
    x: innerX,
    y: innerY,
    width: innerWidth,
    height: innerHeight,
  };
}

function computeMruZIndexMap(
  visibleWindowIds: readonly string[],
  mruWindowIds: readonly string[] | undefined,
  focusedWindowId: string | null | undefined,
): Map<string, number> {
  const visibleSet = new Set(visibleWindowIds);
  const topToBottom: string[] = [];
  const seen = new Set<string>();

  if (focusedWindowId && visibleSet.has(focusedWindowId)) {
    topToBottom.push(focusedWindowId);
    seen.add(focusedWindowId);
  }
  for (const id of mruWindowIds ?? []) {
    if (visibleSet.has(id) && !seen.has(id)) {
      topToBottom.push(id);
      seen.add(id);
    }
  }
  for (const id of visibleWindowIds) {
    if (!seen.has(id)) {
      topToBottom.push(id);
      seen.add(id);
    }
  }

  const bottomToTop = [...topToBottom].reverse();
  const count = bottomToTop.length;
  const zMap = new Map<string, number>();

  for (let rank = 0; rank < count; rank += 1) {
    const id = bottomToTop[rank]!;
    let zIndex: number;
    if (count <= Z2_Z_INDEX_MAX - Z2_Z_INDEX_MIN + 1) {
      zIndex = Z2_Z_INDEX_MIN + rank;
    } else {
      const span = Z2_Z_INDEX_MAX - Z2_Z_INDEX_MIN;
      zIndex = Z2_Z_INDEX_MIN + Math.floor((rank / Math.max(1, count - 1)) * span);
    }
    zMap.set(id, Math.min(Z2_Z_INDEX_MAX, Math.max(Z2_Z_INDEX_MIN, zIndex)));
  }

  return zMap;
}

function defaultFloatingRect(stageRect: WmRectPx, index: number): WmRectPx {
  const defaultW = Math.round(stageRect.width * 0.7);
  const defaultH = Math.round(stageRect.height * 0.76);
  const offset = (index % 5) * 24;
  const baseX = Math.round((stageRect.width - defaultW) * 0.5) + offset;
  const baseY = Math.round((stageRect.height - defaultH) * 0.5) + offset;
  return clampFloatingRectToStage(
    { x: baseX, y: baseY, width: defaultW, height: defaultH },
    stageRect,
  );
}

function stackVerticallyInBounds(
  bounds: WmRectPx,
  count: number,
): WmRectPx[] {
  if (count <= 0) {
    return [];
  }
  if (count === 1) {
    return [{ ...bounds }];
  }
  const totalGaps = TILE_GAP_PX * (count - 1);
  const availH = Math.max(0, bounds.height - totalGaps);
  const baseH = Math.floor(availH / count);
  const out: WmRectPx[] = [];
  let cursorY = bounds.y;
  for (let i = 0; i < count; i += 1) {
    const isLast = i === count - 1;
    const h = isLast ? Math.max(0, bounds.y + bounds.height - cursorY) : baseH;
    out.push({
      x: bounds.x,
      y: cursorY,
      width: bounds.width,
      height: h,
    });
    cursorY = Math.min(bounds.y + bounds.height, cursorY + h + TILE_GAP_PX);
  }
  return out;
}

function assignTwoSlots<T extends SolverWindowInput>(
  items: readonly T[],
  leftAliases: ReadonlySet<string>,
  rightAliases: ReadonlySet<string>,
): { left: T | null; right: T[] } {
  if (items.length === 0) {
    return { left: null, right: [] };
  }
  if (items.length === 1) {
    const only = items[0]!;
    if (only.preferredSlot && rightAliases.has(only.preferredSlot)) {
      return { left: null, right: [only] };
    }
    return { left: only, right: [] };
  }

  let leftPickIndex = items.findIndex(
    (w) => w.preferredSlot !== undefined && leftAliases.has(w.preferredSlot),
  );
  if (leftPickIndex < 0) {
    leftPickIndex = items.findIndex(
      (w) => w.preferredSlot === undefined || !rightAliases.has(w.preferredSlot),
    );
  }
  if (leftPickIndex < 0) {
    leftPickIndex = 0;
  }

  const left = items[leftPickIndex]!;
  const right = items.filter((_, idx) => idx !== leftPickIndex);
  return { left, right };
}

export function solveWorkspaceGeometry(
  input: SolveWorkspaceGeometryInput,
): SolvedWorkspaceGeometry {
  const viewportWidth = Math.max(0, Math.floor(input.viewportWidth));
  const viewportHeight = Math.max(0, Math.floor(input.viewportHeight));

  const viewportRect: WmRectPx = {
    x: 0,
    y: 0,
    width: viewportWidth,
    height: viewportHeight,
  };

  const stageHeight = Math.max(0, viewportHeight - TASKBAR_HEIGHT_PX);
  const stageRect: WmRectPx = {
    x: 0,
    y: 0,
    width: viewportWidth,
    height: stageHeight,
  };

  const taskbarHeight = Math.min(TASKBAR_HEIGHT_PX, viewportHeight);
  const taskbarRect: WmRectPx = {
    x: 0,
    y: Math.max(0, viewportHeight - TASKBAR_HEIGHT_PX),
    width: viewportWidth,
    height: taskbarHeight,
  };

  const visibleWindows = input.windows.filter((w) => !w.minimized);
  const zIndexMap = computeMruZIndexMap(
    visibleWindows.map((w) => w.windowId),
    input.mruWindowIds,
    input.focusedWindowId,
  );

  const splitWidthRatio = clampRatio(
    input.splitWidthRatio ?? DEFAULT_SPLIT_WIDTH_RATIO,
    0.2,
    0.8,
  );
  const z4Locked = Boolean(input.z4ModalLock);
  const z3Overlays = input.activeZ3OverlayRects ?? [];

  const splitSashes: SplitSashGeometry[] = [];
  let z1TaskSpaceRect: WmRectPx | null = null;

  const solvedByWindowId = new Map<
    string,
    { rect: WmRectPx; slot: string; floating: boolean }
  >();

  const floatingWindows: SolverWindowInput[] = [];
  const tiledWindows: SolverWindowInput[] = [];
  for (const win of visibleWindows) {
    if (input.layoutMode === "floating" || win.isFloating) {
      floatingWindows.push(win);
    } else {
      tiledWindows.push(win);
    }
  }

  // Solve floating windows first (always 6px radius and strictly clamped to stageRect)
  floatingWindows.forEach((win, idx) => {
    const rect = win.floatingRect
      ? clampFloatingRectToStage(win.floatingRect, stageRect)
      : defaultFloatingRect(stageRect, idx);
    solvedByWindowId.set(win.windowId, {
      rect,
      slot: "floating",
      floating: true,
    });
  });

  // Solve tiled layout modes
  switch (input.layoutMode) {
    case "workspace-maximize": {
      if (input.showZ1TaskSpace && tiledWindows.length === 0) {
        z1TaskSpaceRect = { ...stageRect };
      }
      for (const win of tiledWindows) {
        solvedByWindowId.set(win.windowId, {
          rect: { ...stageRect },
          slot: "maximized",
          floating: false,
        });
      }
      break;
    }

    case "two-column-split": {
      const gapW = stageRect.width >= TILE_GAP_PX ? TILE_GAP_PX : 0;
      const availW = Math.max(0, stageRect.width - gapW);
      const leftW = Math.floor(availW * splitWidthRatio);
      const rightW = Math.max(0, availW - leftW);
      const rightX = Math.min(stageRect.width, leftW + gapW);

      const leftRect: WmRectPx = { x: 0, y: 0, width: leftW, height: stageRect.height };
      const rightRect: WmRectPx = { x: rightX, y: 0, width: rightW, height: stageRect.height };

      if (tiledWindows.length >= 2 || (tiledWindows.length === 1 && input.showZ1TaskSpace)) {
        splitSashes.push({
          id: "sash-v-main",
          orientation: "vertical",
          rect: {
            x: leftW,
            y: 0,
            width: gapW,
            height: stageRect.height,
          },
        });
      }

      const { left, right } = assignTwoSlots(
        tiledWindows,
        new Set(["left", "col-left"]),
        new Set(["right", "col-right"]),
      );

      if (left !== null) {
        solvedByWindowId.set(left.windowId, {
          rect: leftRect,
          slot: "col-left",
          floating: false,
        });
      }
      if (right.length > 0) {
        const rightStacks = stackVerticallyInBounds(rightRect, right.length);
        right.forEach((win, i) => {
          solvedByWindowId.set(win.windowId, {
            rect: rightStacks[i]!,
            slot: right.length === 1 ? "col-right" : `col-right-${i + 1}`,
            floating: false,
          });
        });
      } else if (input.showZ1TaskSpace) {
        z1TaskSpaceRect = left === null ? { ...stageRect } : rightRect;
      }
      break;
    }

    case "three-pane-workstation": {
      const showZ1 = input.showZ1TaskSpace ?? true;
      const topRatio = clampRatio(
        input.topHeightRatio ?? DEFAULT_THREE_PANE_TOP_HEIGHT_RATIO,
        0.25,
        0.85,
      );

      let topRegionH = stageRect.height;
      if (showZ1) {
        const gapH = stageRect.height >= TILE_GAP_PX ? TILE_GAP_PX : 0;
        const availH = Math.max(0, stageRect.height - gapH);
        topRegionH = Math.round(availH * topRatio);
        const bottomH = Math.max(0, availH - topRegionH);
        const bottomY = Math.min(stageRect.height, topRegionH + gapH);

        z1TaskSpaceRect = {
          x: 0,
          y: bottomY,
          width: stageRect.width,
          height: bottomH,
        };
        splitSashes.push({
          id: "sash-h-stage-z1",
          orientation: "horizontal",
          rect: {
            x: 0,
            y: topRegionH,
            width: stageRect.width,
            height: gapH,
          },
        });
      }

      if (tiledWindows.length === 1) {
        const only = tiledWindows[0]!;
        const gapW = stageRect.width >= TILE_GAP_PX ? TILE_GAP_PX : 0;
        const availW = Math.max(0, stageRect.width - gapW);
        const leftW = Math.floor(availW * splitWidthRatio);
        const rightW = Math.max(0, availW - leftW);
        const rightX = Math.min(stageRect.width, leftW + gapW);

        if (only.preferredSlot === "top-left" || only.preferredSlot === "left" || only.preferredSlot === "col-left") {
          solvedByWindowId.set(only.windowId, {
            rect: { x: 0, y: 0, width: leftW, height: topRegionH },
            slot: "top-left",
            floating: false,
          });
        } else if (only.preferredSlot === "top-right" || only.preferredSlot === "right" || only.preferredSlot === "col-right") {
          solvedByWindowId.set(only.windowId, {
            rect: { x: rightX, y: 0, width: rightW, height: topRegionH },
            slot: "top-right",
            floating: false,
          });
        } else {
          solvedByWindowId.set(only.windowId, {
            rect: { x: 0, y: 0, width: stageRect.width, height: topRegionH },
            slot: "top-full",
            floating: false,
          });
        }
      } else if (tiledWindows.length >= 2) {
        const gapW = stageRect.width >= TILE_GAP_PX ? TILE_GAP_PX : 0;
        const availW = Math.max(0, stageRect.width - gapW);
        const leftW = Math.floor(availW * splitWidthRatio);
        const rightW = Math.max(0, availW - leftW);
        const rightX = Math.min(stageRect.width, leftW + gapW);

        splitSashes.push({
          id: "sash-v-top",
          orientation: "vertical",
          rect: {
            x: leftW,
            y: 0,
            width: gapW,
            height: topRegionH,
          },
        });

        const { left, right } = assignTwoSlots(
          tiledWindows,
          new Set(["left", "col-left", "top-left"]),
          new Set(["right", "col-right", "top-right"]),
        );

        if (left !== null) {
          solvedByWindowId.set(left.windowId, {
            rect: { x: 0, y: 0, width: leftW, height: topRegionH },
            slot: "top-left",
            floating: false,
          });
        }
        const rightBounds: WmRectPx = {
          x: rightX,
          y: 0,
          width: rightW,
          height: topRegionH,
        };
        const rightStacks = stackVerticallyInBounds(rightBounds, right.length);
        right.forEach((win, i) => {
          solvedByWindowId.set(win.windowId, {
            rect: rightStacks[i]!,
            slot: right.length === 1 ? "top-right" : `top-right-${i + 1}`,
            floating: false,
          });
        });
      }
      break;
    }

    case "quadrants": {
      const quadTopRatio = clampRatio(
        input.topHeightRatio ?? DEFAULT_QUADRANT_TOP_HEIGHT_RATIO,
        0.2,
        0.8,
      );
      const gapW = stageRect.width >= TILE_GAP_PX ? TILE_GAP_PX : 0;
      const gapH = stageRect.height >= TILE_GAP_PX ? TILE_GAP_PX : 0;
      const availW = Math.max(0, stageRect.width - gapW);
      const availH = Math.max(0, stageRect.height - gapH);

      const leftW = Math.floor(availW * splitWidthRatio);
      const rightW = Math.max(0, availW - leftW);
      const rightX = Math.min(stageRect.width, leftW + gapW);

      const topH = Math.floor(availH * quadTopRatio);
      const bottomH = Math.max(0, availH - topH);
      const bottomY = Math.min(stageRect.height, topH + gapH);

      splitSashes.push(
        {
          id: "sash-v-quad",
          orientation: "vertical",
          rect: { x: leftW, y: 0, width: gapW, height: stageRect.height },
        },
        {
          id: "sash-h-quad",
          orientation: "horizontal",
          rect: { x: 0, y: topH, width: stageRect.width, height: gapH },
        },
      );

      const slotOrder = ["TL", "TR", "BL", "BR"] as const;
      const slotRects: Record<(typeof slotOrder)[number], WmRectPx> = {
        TL: { x: 0, y: 0, width: leftW, height: topH },
        TR: { x: rightX, y: 0, width: rightW, height: topH },
        BL: { x: 0, y: bottomY, width: leftW, height: bottomH },
        BR: { x: rightX, y: bottomY, width: rightW, height: bottomH },
      };

      const occupied = new Set<string>();
      const unassigned: SolverWindowInput[] = [];

      for (const win of tiledWindows) {
        const pref = win.preferredSlot?.toUpperCase();
        if (
          pref &&
          (pref === "TL" || pref === "TR" || pref === "BL" || pref === "BR") &&
          !occupied.has(pref)
        ) {
          occupied.add(pref);
          solvedByWindowId.set(win.windowId, {
            rect: slotRects[pref],
            slot: pref,
            floating: false,
          });
        } else {
          unassigned.push(win);
        }
      }

      let freeIdx = 0;
      for (const win of unassigned) {
        while (freeIdx < slotOrder.length && occupied.has(slotOrder[freeIdx]!)) {
          freeIdx += 1;
        }
        const chosenSlot =
          freeIdx < slotOrder.length ? slotOrder[freeIdx]! : "BR";
        occupied.add(chosenSlot);
        solvedByWindowId.set(win.windowId, {
          rect: slotRects[chosenSlot],
          slot: chosenSlot,
          floating: false,
        });
      }

      if (input.showZ1TaskSpace && !occupied.has("BR")) {
        z1TaskSpaceRect = slotRects.BR;
      }
      break;
    }

    case "floating": {
      if (input.showZ1TaskSpace) {
        z1TaskSpaceRect = { ...stageRect };
      }
      break;
    }
  }

  const solvedWindows: SolvedWindowGeometry[] = visibleWindows.map((win) => {
    const placement = solvedByWindowId.get(win.windowId) ?? {
      rect: { ...stageRect },
      slot: "maximized",
      floating: false,
    };
    const outerFrameRect = placement.rect;
    const innerContentClipRect = computeInnerContentClipRect(outerFrameRect, stageRect);
    const zIndex = zIndexMap.get(win.windowId) ?? Z2_Z_INDEX_MIN;
    const occludedBySystemPlane =
      z4Locked || z3Overlays.some((ov) => rectsIntersect(outerFrameRect, ov));

    return {
      windowId: win.windowId,
      outerFrameRect,
      innerContentClipRect,
      borderRadiusPx: placement.floating ? 6 : 0,
      zIndex,
      layoutSlot: placement.slot,
      isFloating: placement.floating,
      occludedBySystemPlane,
    };
  });

  return {
    viewportRect,
    stageRect,
    taskbarRect,
    z1TaskSpaceRect,
    windows: solvedWindows,
    splitSashes,
  };
}
