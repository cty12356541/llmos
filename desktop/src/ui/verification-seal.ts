// Recessed 32x20px Hardware-Instrument Verification Seal (Stage D UX Spec §1.3).
// Plots the Lemniscate of Gerono:
//   x(t) = 16 + 11.5 * cos(t), y(t) = 10 + 5.5 * sin(2t), t in [0, 2*pi)
// Guarantees:
//   - Zero RAF loop when unmounted (0% idle CPU/GPU when Z4 is closed)
//   - Automatic prefers-reduced-motion: reduce static closed-curve mode
//   - U+221E pure mathematical character fallback when Canvas 2D is unavailable

export const SEAL_WIDTH_PX = 32;
export const SEAL_HEIGHT_PX = 20;
export const GERONO_CENTER_X = 16;
export const GERONO_CENTER_Y = 10;
export const GERONO_AMPLITUDE_X = 11.5;
export const GERONO_AMPLITUDE_Y = 5.5;
export const GERONO_PERIOD_MS = 3600;
export const VERIFICATION_SEAL_FALLBACK_GLYPH = "\u221E";

export interface GeronoPoint {
  readonly x: number;
  readonly y: number;
}

export type VerificationSealMode =
  | "animated-canvas"
  | "static-reduced-motion"
  | "text-fallback";

export interface VerificationSealColors {
  readonly hairlineStroke: string;
  readonly curveStroke: string;
}

export interface ReducedMotionMediaQueryLike {
  readonly matches: boolean;
  addEventListener?(type: "change", listener: (ev: { matches: boolean }) => void): void;
  removeEventListener?(type: "change", listener: (ev: { matches: boolean }) => void): void;
}

export interface VerificationSealOptions {
  readonly doc?: Document;
  readonly ariaLabel?: string;
  readonly reducedMotion?: boolean;
  readonly forceFallback?: boolean;
  readonly autoStart?: boolean;
  readonly colors?: VerificationSealColors;
  readonly matchReducedMotion?: () => ReducedMotionMediaQueryLike | null;
  readonly requestFrame?: (cb: (timestampMs: number) => void) => number;
  readonly cancelFrame?: (handle: number) => void;
}

export interface VerificationSealController {
  readonly element: HTMLElement;
  getMode(): VerificationSealMode;
  isRunning(): boolean;
  mount(): void;
  unmount(): void;
  renderAt(elapsedMs: number): void;
  renderStaticFrame(): void;
}

const DEFAULT_GRAPHITE_COLORS: VerificationSealColors = {
  hairlineStroke: "#2e3138",
  curveStroke: "#ecedee",
};

function escapeHtml(raw: string): string {
  return raw
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

/** Evaluates x(t) = 16 + 11.5*cos(t), y(t) = 10 + 5.5*sin(2t). */
export function sampleGeronoPoint(tRad: number): GeronoPoint {
  return {
    x: GERONO_CENTER_X + GERONO_AMPLITUDE_X * Math.cos(tRad),
    y: GERONO_CENTER_Y + GERONO_AMPLITUDE_Y * Math.sin(2 * tRad),
  };
}

/** Samples closed Gerono curve across [0, 2*pi] with `steps` segments. */
export function sampleGeronoCurve(steps = 64): readonly GeronoPoint[] {
  const safeSteps = Math.max(8, Math.floor(steps));
  const points: GeronoPoint[] = [];
  for (let i = 0; i <= safeSteps; i += 1) {
    const t = (i / safeSteps) * Math.PI * 2;
    points.push(sampleGeronoPoint(t));
  }
  return points;
}

/** Computes center-symmetric phase scan markers (phi and pi - phi) for period T = 3.6s. */
export function computeSealPhaseMarkers(
  elapsedMs: number,
): readonly [GeronoPoint, GeronoPoint] {
  const normalizedMs =
    ((elapsedMs % GERONO_PERIOD_MS) + GERONO_PERIOD_MS) % GERONO_PERIOD_MS;
  const phi = (normalizedMs / GERONO_PERIOD_MS) * Math.PI * 2;
  return [sampleGeronoPoint(phi), sampleGeronoPoint(Math.PI - phi)];
}

/** Pure HTML fallback rendering U+221E inside the recessed 32x20px housing. */
export function renderVerificationSealFallbackHtml(options?: {
  readonly ariaLabel?: string;
}): string {
  const label = options?.ariaLabel ?? "System Security Verification Seal";
  return (
    `<div class="nlos-verification-seal" data-seal-mode="text-fallback" ` +
    `role="img" aria-label="${escapeHtml(label)}">` +
    `<span class="nlos-verification-seal__fallback" aria-hidden="true">${VERIFICATION_SEAL_FALLBACK_GLYPH}</span>` +
    `</div>`
  );
}

function resolveSealColors(
  el: HTMLElement,
  explicit?: VerificationSealColors,
): VerificationSealColors {
  if (explicit) {
    return explicit;
  }
  if (typeof globalThis.getComputedStyle === "function") {
    try {
      const style = globalThis.getComputedStyle(el);
      const hairline = style.getPropertyValue("--nlos-border-hairline").trim();
      const primary = style.getPropertyValue("--nlos-text-primary").trim();
      if (hairline.length > 0 && primary.length > 0) {
        return { hairlineStroke: hairline, curveStroke: primary };
      }
    } catch {
      // Fallback to default monochrome tokens in headless test environments.
    }
  }
  return DEFAULT_GRAPHITE_COLORS;
}

function drawGeronoFrame(
  ctx: CanvasRenderingContext2D,
  colors: VerificationSealColors,
  phaseMarkers: readonly [GeronoPoint, GeronoPoint] | null,
): void {
  ctx.clearRect(0, 0, SEAL_WIDTH_PX, SEAL_HEIGHT_PX);

  // 1px orthogonal reference crosshair
  ctx.strokeStyle = colors.hairlineStroke;
  ctx.lineWidth = 1;
  ctx.beginPath();
  ctx.moveTo(0, GERONO_CENTER_Y);
  ctx.lineTo(SEAL_WIDTH_PX, GERONO_CENTER_Y);
  ctx.moveTo(GERONO_CENTER_X, 0);
  ctx.lineTo(GERONO_CENTER_X, SEAL_HEIGHT_PX);
  ctx.stroke();

  // 1px crisp monochrome Gerono closed trajectory
  const curve = sampleGeronoCurve(64);
  ctx.strokeStyle = colors.curveStroke;
  ctx.lineWidth = 1;
  ctx.beginPath();
  for (let i = 0; i < curve.length; i += 1) {
    const pt = curve[i]!;
    if (i === 0) {
      ctx.moveTo(pt.x, pt.y);
    } else {
      ctx.lineTo(pt.x, pt.y);
    }
  }
  ctx.stroke();

  ctx.fillStyle = colors.curveStroke;
  if (phaseMarkers === null) {
    // Static reduced-motion center reference point
    ctx.fillRect(GERONO_CENTER_X - 1, GERONO_CENTER_Y - 1, 2, 2);
  } else {
    for (const marker of phaseMarkers) {
      ctx.fillRect(marker.x - 1, marker.y - 1, 2, 2);
    }
  }
}

export function createVerificationSeal(
  options?: VerificationSealOptions,
): VerificationSealController {
  const doc = options?.doc ?? document;
  const housing = doc.createElement("div");
  housing.className = "nlos-verification-seal";
  housing.setAttribute("role", "img");
  housing.setAttribute(
    "aria-label",
    options?.ariaLabel ?? "System Security Verification Seal",
  );

  const defaultMatchMedia = (): ReducedMotionMediaQueryLike | null => {
    if (typeof globalThis.matchMedia === "function") {
      return globalThis.matchMedia("(prefers-reduced-motion: reduce)");
    }
    return null;
  };
  const mql = (options?.matchReducedMotion ?? defaultMatchMedia)();

  const isReducedMotion = (): boolean => {
    if (options?.reducedMotion !== undefined) {
      return options.reducedMotion;
    }
    return Boolean(mql?.matches);
  };

  const raf =
    options?.requestFrame ??
    ((cb: (timestampMs: number) => void): number =>
      globalThis.requestAnimationFrame(cb));
  const caf =
    options?.cancelFrame ??
    ((handle: number): void => {
      globalThis.cancelAnimationFrame(handle);
    });

  let ctx: CanvasRenderingContext2D | null = null;
  if (!options?.forceFallback) {
    const canvas = doc.createElement("canvas");
    canvas.className = "nlos-verification-seal__canvas";
    canvas.width = SEAL_WIDTH_PX;
    canvas.height = SEAL_HEIGHT_PX;
    canvas.setAttribute("aria-hidden", "true");
    try {
      ctx = canvas.getContext("2d");
    } catch {
      ctx = null;
    }
    if (ctx !== null) {
      housing.appendChild(canvas);
    }
  }

  let mode: VerificationSealMode = "text-fallback";
  if (ctx === null) {
    const fallback = doc.createElement("span");
    fallback.className = "nlos-verification-seal__fallback";
    fallback.setAttribute("aria-hidden", "true");
    fallback.textContent = VERIFICATION_SEAL_FALLBACK_GLYPH;
    housing.appendChild(fallback);
    mode = "text-fallback";
  } else {
    mode = isReducedMotion() ? "static-reduced-motion" : "animated-canvas";
  }
  housing.setAttribute("data-seal-mode", mode);

  let mounted = false;
  let rafHandle: number | null = null;
  let originTimestampMs: number | null = null;

  const stopLoop = (): void => {
    if (rafHandle !== null) {
      caf(rafHandle);
      rafHandle = null;
    }
    originTimestampMs = null;
  };

  const renderStaticFrame = (): void => {
    if (ctx === null) {
      return;
    }
    drawGeronoFrame(ctx, resolveSealColors(housing, options?.colors), null);
  };

  const renderAt = (elapsedMs: number): void => {
    if (ctx === null) {
      return;
    }
    const markers = computeSealPhaseMarkers(elapsedMs);
    drawGeronoFrame(ctx, resolveSealColors(housing, options?.colors), markers);
  };

  const tick = (timestampMs: number): void => {
    if (!mounted || ctx === null || isReducedMotion()) {
      rafHandle = null;
      return;
    }
    if (originTimestampMs === null) {
      originTimestampMs = timestampMs;
    }
    renderAt(timestampMs - originTimestampMs);
    rafHandle = raf(tick);
  };

  const syncLifecycle = (): void => {
    stopLoop();
    if (ctx === null) {
      mode = "text-fallback";
      housing.setAttribute("data-seal-mode", mode);
      return;
    }
    if (isReducedMotion()) {
      mode = "static-reduced-motion";
      housing.setAttribute("data-seal-mode", mode);
      renderStaticFrame();
      return;
    }
    mode = "animated-canvas";
    housing.setAttribute("data-seal-mode", mode);
    if (mounted) {
      renderAt(0);
      rafHandle = raf(tick);
    } else {
      renderStaticFrame();
    }
  };

  const onReducedMotionChange = (): void => {
    syncLifecycle();
  };

  // Render initial crisp frame without scheduling any RAF while unmounted.
  if (ctx !== null) {
    renderStaticFrame();
  }

  const mount = (): void => {
    if (mounted) {
      return;
    }
    mounted = true;
    mql?.addEventListener?.("change", onReducedMotionChange);
    syncLifecycle();
  };

  const unmount = (): void => {
    mounted = false;
    stopLoop();
    mql?.removeEventListener?.("change", onReducedMotionChange);
  };

  if (options?.autoStart) {
    mount();
  }

  return {
    element: housing,
    getMode: () => mode,
    isRunning: () => rafHandle !== null,
    mount,
    unmount,
    renderAt,
    renderStaticFrame,
  };
}
