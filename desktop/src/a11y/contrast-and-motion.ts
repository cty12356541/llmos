// WCAG 2.2 AA Relative Luminance Contrast Auditor & Global Reduced-Motion Enforcer
// (Stage D UX Spec §1.1, §1.3, §3.7; WCAG 2.2 1.4.3 Contrast Minimum, 1.4.11 Non-text Contrast,
//  2.4.11 Focus Appearance 2px ring, 2.3.3 Animation from Interactions).

export const WCAG_AA_NORMAL_TEXT_MIN_RATIO = 4.5 as const;
export const WCAG_AA_UI_COMPONENT_MIN_RATIO = 3.0 as const;
export const FOCUS_RING_WIDTH_PX = 2 as const;

export type WorkstationThemeId = "graphite-workstation" | "paper-workstation";
export type ContrastCategory = "normal-text" | "ui-component";

export interface ThemeContrastPairSpec {
  readonly pairId: string;
  readonly theme: WorkstationThemeId;
  readonly category: ContrastCategory;
  readonly foregroundToken: string;
  readonly foregroundHex: string;
  readonly backgroundToken: string;
  readonly backgroundHex: string;
}

export interface EvaluatedContrastPair extends ThemeContrastPairSpec {
  readonly contrastRatio: number;
  readonly requiredRatio: number;
  readonly passed: boolean;
}

export interface WcagContrastAuditReport {
  readonly passed: boolean;
  readonly totalPairsAudited: number;
  readonly minNormalTextRatioObserved: number;
  readonly minUiComponentRatioObserved: number;
  readonly results: readonly EvaluatedContrastPair[];
  readonly failures: readonly EvaluatedContrastPair[];
}

export function parseHexColorToRgb(hex: string): readonly [number, number, number] {
  const cleaned = hex.trim().replace(/^#/, "");
  if (!/^[0-9a-fA-F]{6}$/.test(cleaned)) {
    throw new Error(`Invalid 6-digit hex color "${hex}"`);
  }
  const r = parseInt(cleaned.slice(0, 2), 16);
  const g = parseInt(cleaned.slice(2, 4), 16);
  const b = parseInt(cleaned.slice(4, 6), 16);
  return [r, g, b];
}

export function computeRelativeLuminance(hex: string): number {
  const [r8, g8, b8] = parseHexColorToRgb(hex);
  const linearize = (channel8: number): number => {
    const s = channel8 / 255;
    return s <= 0.04045 ? s / 12.92 : Math.pow((s + 0.055) / 1.055, 2.4);
  };
  const r = linearize(r8);
  const g = linearize(g8);
  const b = linearize(b8);
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

export function computeContrastRatio(foregroundHex: string, backgroundHex: string): number {
  const lumA = computeRelativeLuminance(foregroundHex);
  const lumB = computeRelativeLuminance(backgroundHex);
  const lighter = Math.max(lumA, lumB);
  const darker = Math.min(lumA, lumB);
  return Number(((lighter + 0.05) / (darker + 0.05)).toFixed(2));
}

export const CANONICAL_WORKSTATION_CONTRAST_PAIRS: readonly ThemeContrastPairSpec[] = [
  // Graphite Workstation (Dark)
  {
    pairId: "graphite:text-primary-on-window",
    theme: "graphite-workstation",
    category: "normal-text",
    foregroundToken: "--nlos-text-primary",
    foregroundHex: "#ecedee",
    backgroundToken: "--nlos-bg-window",
    backgroundHex: "#18191c",
  },
  {
    pairId: "graphite:text-primary-on-elevated",
    theme: "graphite-workstation",
    category: "normal-text",
    foregroundToken: "--nlos-text-primary",
    foregroundHex: "#ecedee",
    backgroundToken: "--nlos-bg-elevated",
    backgroundHex: "#23252a",
  },
  {
    pairId: "graphite:text-secondary-on-window",
    theme: "graphite-workstation",
    category: "normal-text",
    foregroundToken: "--nlos-text-secondary",
    foregroundHex: "#9da3ae",
    backgroundToken: "--nlos-bg-window",
    backgroundHex: "#18191c",
  },
  {
    pairId: "graphite:text-secondary-on-elevated",
    theme: "graphite-workstation",
    category: "normal-text",
    foregroundToken: "--nlos-text-secondary",
    foregroundHex: "#9da3ae",
    backgroundToken: "--nlos-bg-elevated",
    backgroundHex: "#23252a",
  },
  {
    pairId: "graphite:accent-fg-on-accent-primary",
    theme: "graphite-workstation",
    category: "normal-text",
    foregroundToken: "--nlos-accent-fg",
    foregroundHex: "#ffffff",
    backgroundToken: "--nlos-accent-primary",
    backgroundHex: "#2563eb",
  },
  {
    pairId: "graphite:signal-action-text-on-window",
    theme: "graphite-workstation",
    category: "normal-text",
    foregroundToken: "--nlos-signal-action-text",
    foregroundHex: "#f59e0b",
    backgroundToken: "--nlos-bg-window",
    backgroundHex: "#18191c",
  },
  {
    pairId: "graphite:signal-danger-text-on-window",
    theme: "graphite-workstation",
    category: "normal-text",
    foregroundToken: "--nlos-signal-danger-text",
    foregroundHex: "#f87171",
    backgroundToken: "--nlos-bg-window",
    backgroundHex: "#18191c",
  },
  {
    pairId: "graphite:signal-danger-fg-on-danger",
    theme: "graphite-workstation",
    category: "normal-text",
    foregroundToken: "--nlos-signal-danger-fg",
    foregroundHex: "#ffffff",
    backgroundToken: "--nlos-signal-danger",
    backgroundHex: "#dc2626",
  },
  {
    pairId: "graphite:focus-ring-on-window",
    theme: "graphite-workstation",
    category: "ui-component",
    foregroundToken: "--nlos-accent-active",
    foregroundHex: "#3b82f6",
    backgroundToken: "--nlos-bg-window",
    backgroundHex: "#18191c",
  },
  {
    pairId: "graphite:focus-ring-on-elevated",
    theme: "graphite-workstation",
    category: "ui-component",
    foregroundToken: "--nlos-accent-active",
    foregroundHex: "#3b82f6",
    backgroundToken: "--nlos-bg-elevated",
    backgroundHex: "#23252a",
  },
  // Paper Workstation (Light)
  {
    pairId: "paper:text-primary-on-window",
    theme: "paper-workstation",
    category: "normal-text",
    foregroundToken: "--nlos-text-primary",
    foregroundHex: "#111827",
    backgroundToken: "--nlos-bg-window",
    backgroundHex: "#ffffff",
  },
  {
    pairId: "paper:text-primary-on-chrome",
    theme: "paper-workstation",
    category: "normal-text",
    foregroundToken: "--nlos-text-primary",
    foregroundHex: "#111827",
    backgroundToken: "--nlos-bg-chrome",
    backgroundHex: "#f3f4f6",
  },
  {
    pairId: "paper:text-secondary-on-window",
    theme: "paper-workstation",
    category: "normal-text",
    foregroundToken: "--nlos-text-secondary",
    foregroundHex: "#4b5563",
    backgroundToken: "--nlos-bg-window",
    backgroundHex: "#ffffff",
  },
  {
    pairId: "paper:text-muted-on-window",
    theme: "paper-workstation",
    category: "normal-text",
    foregroundToken: "--nlos-text-muted",
    foregroundHex: "#6b7280",
    backgroundToken: "--nlos-bg-window",
    backgroundHex: "#ffffff",
  },
  {
    pairId: "paper:accent-fg-on-accent-primary",
    theme: "paper-workstation",
    category: "normal-text",
    foregroundToken: "--nlos-accent-fg",
    foregroundHex: "#ffffff",
    backgroundToken: "--nlos-accent-primary",
    backgroundHex: "#1d4ed8",
  },
  {
    pairId: "paper:signal-action-text-on-window",
    theme: "paper-workstation",
    category: "normal-text",
    foregroundToken: "--nlos-signal-action-text",
    foregroundHex: "#b45309",
    backgroundToken: "--nlos-bg-window",
    backgroundHex: "#ffffff",
  },
  {
    pairId: "paper:signal-danger-text-on-window",
    theme: "paper-workstation",
    category: "normal-text",
    foregroundToken: "--nlos-signal-danger-text",
    foregroundHex: "#b91c1c",
    backgroundToken: "--nlos-bg-window",
    backgroundHex: "#ffffff",
  },
  {
    pairId: "paper:signal-danger-fg-on-danger",
    theme: "paper-workstation",
    category: "normal-text",
    foregroundToken: "--nlos-signal-danger-fg",
    foregroundHex: "#ffffff",
    backgroundToken: "--nlos-signal-danger",
    backgroundHex: "#dc2626",
  },
  {
    pairId: "paper:focus-ring-on-window",
    theme: "paper-workstation",
    category: "ui-component",
    foregroundToken: "--nlos-accent-active",
    foregroundHex: "#1d4ed8",
    backgroundToken: "--nlos-bg-window",
    backgroundHex: "#ffffff",
  },
  {
    pairId: "paper:focus-ring-on-chrome",
    theme: "paper-workstation",
    category: "ui-component",
    foregroundToken: "--nlos-accent-active",
    foregroundHex: "#1d4ed8",
    backgroundToken: "--nlos-bg-chrome",
    backgroundHex: "#f3f4f6",
  },
];

export function auditWorkstationContrast(
  pairs: readonly ThemeContrastPairSpec[] = CANONICAL_WORKSTATION_CONTRAST_PAIRS,
): WcagContrastAuditReport {
  const results: EvaluatedContrastPair[] = [];
  const failures: EvaluatedContrastPair[] = [];
  let minNormalTextRatio = Number.POSITIVE_INFINITY;
  let minUiComponentRatio = Number.POSITIVE_INFINITY;

  for (const spec of pairs) {
    const ratio = computeContrastRatio(spec.foregroundHex, spec.backgroundHex);
    const required =
      spec.category === "normal-text"
        ? WCAG_AA_NORMAL_TEXT_MIN_RATIO
        : WCAG_AA_UI_COMPONENT_MIN_RATIO;
    const passed = ratio >= required;

    if (spec.category === "normal-text" && ratio < minNormalTextRatio) {
      minNormalTextRatio = ratio;
    }
    if (spec.category === "ui-component" && ratio < minUiComponentRatio) {
      minUiComponentRatio = ratio;
    }

    const evaluated: EvaluatedContrastPair = {
      ...spec,
      contrastRatio: ratio,
      requiredRatio: required,
      passed,
    };
    results.push(evaluated);
    if (!passed) {
      failures.push(evaluated);
    }
  }

  return {
    passed: failures.length === 0,
    totalPairsAudited: results.length,
    minNormalTextRatioObserved: Number.isFinite(minNormalTextRatio) ? minNormalTextRatio : 0,
    minUiComponentRatioObserved: Number.isFinite(minUiComponentRatio) ? minUiComponentRatio : 0,
    results,
    failures,
  };
}

export interface ReducedMotionPostureSnapshot {
  readonly systemPrefersReducedMotion: boolean;
  readonly userOverrideReducedMotion: boolean | null;
  readonly effectiveReducedMotion: boolean;
  readonly geronoVerificationSealMode: "animated-canvas" | "static-reduced-motion";
  readonly activeRafLoopCount: number;
  readonly fastTransitionDurationMs: 0 | 80;
  readonly baseTransitionDurationMs: 0 | 140;
}

export class ReducedMotionController {
  private systemPrefersReducedMotion: boolean;
  private userOverrideReducedMotion: boolean | null = null;
  private z4SealMounted = false;

  constructor(initialSystemPrefersReduced = false) {
    this.systemPrefersReducedMotion = initialSystemPrefersReduced;
  }

  public setSystemPrefersReducedMotion(prefersReduced: boolean): ReducedMotionPostureSnapshot {
    this.systemPrefersReducedMotion = prefersReduced;
    return this.getSnapshot();
  }

  public setUserOverride(overrideValue: boolean | null): ReducedMotionPostureSnapshot {
    this.userOverrideReducedMotion = overrideValue;
    return this.getSnapshot();
  }

  public setZ4SealMounted(mounted: boolean): ReducedMotionPostureSnapshot {
    this.z4SealMounted = mounted;
    return this.getSnapshot();
  }

  public isEffectiveReducedMotion(): boolean {
    if (this.userOverrideReducedMotion !== null) {
      return this.userOverrideReducedMotion;
    }
    return this.systemPrefersReducedMotion;
  }

  public getSnapshot(): ReducedMotionPostureSnapshot {
    const reduced = this.isEffectiveReducedMotion();
    const geronoMode = reduced ? "static-reduced-motion" : "animated-canvas";
    const activeRafLoopCount = this.z4SealMounted && !reduced ? 1 : 0;
    return {
      systemPrefersReducedMotion: this.systemPrefersReducedMotion,
      userOverrideReducedMotion: this.userOverrideReducedMotion,
      effectiveReducedMotion: reduced,
      geronoVerificationSealMode: geronoMode,
      activeRafLoopCount,
      fastTransitionDurationMs: reduced ? 0 : 80,
      baseTransitionDurationMs: reduced ? 0 : 140,
    };
  }
}
