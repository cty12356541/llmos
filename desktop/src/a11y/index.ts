// Unified Workstation Accessibility (WCAG 2.2 AA, ARIA Semantic Tree, Keyboard Navigation,
// Z4 Modal Focus Trap, Reduced Motion) & Bilingual Localization (zh-CN / en-US) Facade.
// Authority: W73-D-A11Y-I18N (D-CONTROL-SETTINGS-A11Y Slice 2, [DUI-WINDOW-001], [TUI-EXT-001]).

import {
  LocalizationController,
  createLocalizationController,
  verifyLocaleKeyParity,
  verifyLocaleZeroEmoji,
  verifyLocaleZeroJargon,
  type LocaleEmojiAuditReport,
  type LocaleParityAuditReport,
  type LocaleResourceBundle,
  type LocaleZeroJargonAuditReport,
  type SupportedLocale,
} from "../i18n/index";
import {
  WorkstationLiveRegionAnnouncer,
  auditWorkstationAriaTree,
  buildCanonicalWorkstationAriaTree,
  type AriaSemanticNode,
  type AriaTreeAuditReport,
} from "./aria-tree-auditor";
import {
  ReducedMotionController,
  auditWorkstationContrast,
  type ReducedMotionPostureSnapshot,
  type WcagContrastAuditReport,
} from "./contrast-and-motion";
import {
  WorkstationFocusManager,
  verifyZ4ModalFocusTrap,
  type FocusTargetDescriptor,
  type FocusTrapVerificationReport,
} from "./focus-manager";

export * from "./focus-manager";
export * from "./aria-tree-auditor";
export * from "./contrast-and-motion";
export * from "../i18n/index";

export interface WorkstationA11yI18nSuiteSnapshot {
  readonly locale: SupportedLocale;
  readonly bundleVersion: string;
  readonly schemaRevision: number;
  readonly localeSwitchGeneration: number;
  readonly reducedMotion: ReducedMotionPostureSnapshot;
  readonly z4ModalActive: boolean;
  readonly activeFocusId: string | null;
  readonly contrastAudit: WcagContrastAuditReport;
  readonly ariaAudit: AriaTreeAuditReport;
  readonly parityAudit: LocaleParityAuditReport;
  readonly zeroJargonAudit: LocaleZeroJargonAuditReport;
  readonly zeroEmojiAudit: LocaleEmojiAuditReport;
}

export class WorkstationA11yI18nSuiteController {
  public readonly i18n: LocalizationController;
  public readonly focus: WorkstationFocusManager;
  public readonly announcer: WorkstationLiveRegionAnnouncer;
  public readonly motion: ReducedMotionController;

  constructor(initialLocale: SupportedLocale = "zh-CN", initialReducedMotion = false) {
    this.i18n = createLocalizationController(initialLocale);
    this.focus = new WorkstationFocusManager();
    this.announcer = new WorkstationLiveRegionAnnouncer();
    this.motion = new ReducedMotionController(initialReducedMotion);
  }

  public switchLocale(nextLocale: SupportedLocale): LocaleResourceBundle {
    const bundle = this.i18n.switchLocale(nextLocale);
    return bundle;
  }

  public setReducedMotionOverride(overrideValue: boolean | null): ReducedMotionPostureSnapshot {
    return this.motion.setUserOverride(overrideValue);
  }

  public buildCurrentAriaTree(): readonly AriaSemanticNode[] {
    return buildCanonicalWorkstationAriaTree({
      locale: this.i18n.getLocale(),
      z4ModalActive: this.focus.isZ4ModalActive(),
    });
  }

  public auditAriaLandmarks(): AriaTreeAuditReport {
    const tree = this.buildCurrentAriaTree();
    return auditWorkstationAriaTree(tree, {
      locale: this.i18n.getLocale(),
      z4ModalActive: this.focus.isZ4ModalActive(),
    });
  }

  public auditWcagContrast(): WcagContrastAuditReport {
    return auditWorkstationContrast();
  }

  public verifyKeyboardFocusTrap(
    targets: readonly FocusTargetDescriptor[],
    originatingTriggerId: string,
  ): FocusTrapVerificationReport {
    return verifyZ4ModalFocusTrap(targets, originatingTriggerId);
  }

  public getConformanceSnapshot(): WorkstationA11yI18nSuiteSnapshot {
    const bundle = this.i18n.getBundle();
    return {
      locale: bundle.locale,
      bundleVersion: bundle.bundleVersion,
      schemaRevision: bundle.schemaRevision,
      localeSwitchGeneration: this.i18n.getSwitchGeneration(),
      reducedMotion: this.motion.getSnapshot(),
      z4ModalActive: this.focus.isZ4ModalActive(),
      activeFocusId: this.focus.getActiveFocusId(),
      contrastAudit: this.auditWcagContrast(),
      ariaAudit: this.auditAriaLandmarks(),
      parityAudit: verifyLocaleKeyParity(),
      zeroJargonAudit: verifyLocaleZeroJargon(),
      zeroEmojiAudit: verifyLocaleZeroEmoji(),
    };
  }
}

export function createWorkstationA11yI18nSuiteController(
  initialLocale: SupportedLocale = "zh-CN",
  initialReducedMotion = false,
): WorkstationA11yI18nSuiteController {
  return new WorkstationA11yI18nSuiteController(initialLocale, initialReducedMotion);
}
