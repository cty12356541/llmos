// Deterministic Conformance Suite for W73-D-A11Y-I18N (D-CONTROL-SETTINGS-A11Y Slice 2:
// WCAG 2.2 AA Contrast, ARIA Semantic Tree, Keyboard Navigation, Z4 Focus Trap,
// Reduced Motion, and Versioned zh-CN / en-US Localization).
// Run from desktop/: npx tsx src/a11y/__tests__/a11y-i18n.test.ts

import {
  ALL_12_CONTROL_CENTER_DOMAINS,
  ALL_24_WORKSTATION_TASK_STATES,
  CANONICAL_16_TASK_STATES,
  CANONICAL_WORKSTATION_CONTRAST_PAIRS,
  EN_US_BUNDLE,
  FOCUS_RING_WIDTH_PX,
  LOCALE_BUNDLE_VERSION,
  LOCALE_SCHEMA_REVISION,
  ReducedMotionController,
  WCAG_AA_NORMAL_TEXT_MIN_RATIO,
  WCAG_AA_UI_COMPONENT_MIN_RATIO,
  WCAG_MIN_HIT_TARGET_PX,
  WorkstationFocusManager,
  WorkstationLiveRegionAnnouncer,
  ZH_CN_BUNDLE,
  auditWorkstationAriaTree,
  auditWorkstationContrast,
  buildCanonicalWorkstationAriaTree,
  computeContrastRatio,
  createLocalizationController,
  createWorkstationA11yI18nSuiteController,
  formatTabularNumber,
  interpolateMessage,
  verifyLocaleKeyParity,
  verifyLocaleZeroEmoji,
  verifyLocaleZeroJargon,
  verifyZ4ModalFocusTrap,
  type AriaSemanticNode,
  type FocusTargetDescriptor,
} from "../index";

interface NodeFsLike {
  readFileSync(path: string, encoding: "utf-8"): string;
}

interface NodePathLike {
  resolve(...segments: string[]): string;
  dirname(p: string): string;
}

interface NodeUrlLike {
  fileURLToPath(url: string): string;
}

const importBuiltin = new Function(
  "specifier",
  "return import(specifier);",
) as <T>(specifier: string) => Promise<T>;

let passedSuites = 0;
let passedAssertions = 0;

function assert(condition: unknown, message: string): asserts condition {
  if (!condition) {
    throw new Error(`Assertion failed: ${message}`);
  }
  passedAssertions += 1;
}

async function runSuite(name: string, fn: () => void | Promise<void>): Promise<void> {
  await fn();
  passedSuites += 1;
  console.log(`  [PASS] ${name}`);
}

const SAMPLE_FOCUS_TARGETS: readonly FocusTargetDescriptor[] = [
  { id: "z0-decorative-canvas", plane: "Z0", label: "Backdrop", visible: true },
  { id: "z1-task-row-alpha", plane: "Z1", label: "Task Row Alpha", visible: true },
  { id: "z1-drawer-close-btn", plane: "Z1", label: "Close Inspector Drawer", visible: true },
  { id: "z2-window-taskmgr", plane: "Z2", label: "Task Manager Window", visible: true },
  { id: "z3-taskbar-omnibar-btn", plane: "Z3", label: "Command Palette Trigger", visible: true },
  { id: "z3-taskbar-tray-btn", plane: "Z3", label: "System Control Center Trigger", visible: true },
  { id: "z4-consent-approve-btn", plane: "Z4", label: "Approve Authorization", visible: true },
  { id: "z4-consent-deny-btn", plane: "Z4", label: "Deny Authorization", visible: true },
];

async function main(): Promise<void> {
  console.log("Running W73-D-A11Y-I18N Conformance Suite...");

  await runSuite(
    "1. Versioned zh-CN / en-US resource bundles, 100% key parity & atomic runtime switching ([TUI-EXT-001])",
    () => {
      const parity = verifyLocaleKeyParity(ZH_CN_BUNDLE, EN_US_BUNDLE);
      assert(parity.passed, "zh-CN and en-US bundles must have 100% key parity");
      assert(
        parity.bundleVersion === LOCALE_BUNDLE_VERSION &&
          parity.schemaRevision === LOCALE_SCHEMA_REVISION,
        "Bundle version and schema revision match 1.0.0-stage-d / rev 1",
      );
      assert(parity.zhKeyCount === parity.enKeyCount && parity.zhKeyCount >= 95, "All leaf keys aligned");
      assert(parity.missingInEnUs.length === 0 && parity.missingInZhCn.length === 0, "Zero missing keys");
      assert(parity.emptyValueKeys.length === 0, "Zero empty translation strings");

      for (const state of CANONICAL_16_TASK_STATES) {
        assert(
          ZH_CN_BUNDLE.taskStates[state].length > 0 && EN_US_BUNDLE.taskStates[state].length > 0,
          `Canonical 16 task state ${state} translated in both locales`,
        );
      }
      for (const state of ALL_24_WORKSTATION_TASK_STATES) {
        assert(
          ZH_CN_BUNDLE.taskStates[state].length > 0 && EN_US_BUNDLE.taskStates[state].length > 0,
          `Extended 24 workstation state ${state} translated in both locales`,
        );
      }
      for (const domain of ALL_12_CONTROL_CENTER_DOMAINS) {
        assert(
          ZH_CN_BUNDLE.controlCenterDomains[domain].length > 0 &&
            EN_US_BUNDLE.controlCenterDomains[domain].length > 0,
          `Control Center domain ${domain} translated in both locales`,
        );
      }

      const i18n = createLocalizationController("zh-CN");
      const observedLocales: string[] = [];
      const unsub = i18n.subscribe((loc) => observedLocales.push(loc));

      assert(i18n.getLocale() === "zh-CN", "Starts in zh-CN");
      assert(
        i18n.t("notifications.unreadCountTemplate", { count: 5 }) === "共 5 条未读通知",
        "zh-CN interpolation works",
      );
      i18n.switchLocale("en-US");
      assert(i18n.getLocale() === "en-US", "Switched to en-US");
      assert(i18n.getSwitchGeneration() === 2, "Switch generation increments atomically");
      assert(
        i18n.t("notifications.unreadCountTemplate", { count: 5 }) === "5 unread notifications",
        "en-US interpolation works",
      );
      assert(formatTabularNumber(12480, "en-US") === "12,480", "Tabular number formatting works");
      unsub();
      assert(observedLocales.length === 1 && observedLocales[0] === "en-US", "Listener notified once");
    },
  );

  await runSuite(
    "2. L0/L1/L2 zero internal kernel jargon guard (ANTI-AI-07) & zero emoji guard (ANTI-AI-05)",
    () => {
      const jargonReport = verifyLocaleZeroJargon([ZH_CN_BUNDLE, EN_US_BUNDLE]);
      assert(
        jargonReport.passed && jargonReport.violations.length === 0,
        `Zero L0/L1/L2 jargon violations across ${jargonReport.auditedL0ToL2EntryCount} entries`,
      );
      assert(jargonReport.auditedL0ToL2EntryCount >= 170, "Audited all L0/L1/L2 keys in both locales");

      // Negative test: inject forbidden jargon into a synthetic L1 bundle copy and verify detection
      const taintedBundle = {
        ...EN_US_BUNDLE,
        spotlight: {
          ...EN_US_BUNDLE.spotlight,
          readOnlyPreviewBanner: "Preview with 0x1F mask and TaskAttempt Quota",
        },
      };
      const taintedReport = verifyLocaleZeroJargon([taintedBundle]);
      assert(!taintedReport.passed && taintedReport.violations.length >= 3, "Detects injected 0x1F/TaskAttempt/Quota jargon");

      const emojiReport = verifyLocaleZeroEmoji([ZH_CN_BUNDLE, EN_US_BUNDLE]);
      assert(emojiReport.passed && emojiReport.offendingKeys.length === 0, "Zero emojis in zh-CN and en-US");
    },
  );

  await runSuite(
    "3. Full keyboard navigation loop, roving tabindex (grid/treegrid/DAG), Escape stack & global shortcuts",
    () => {
      const mgr = new WorkstationFocusManager();
      mgr.registerFocusTargets(SAMPLE_FOCUS_TARGETS);

      // Z0 is decorative and skipped; starts at first Z1 target
      assert(mgr.getActiveFocusId() === "z1-task-row-alpha", "Initial focus lands on Z1 task row");

      // Tab forward across Z1 -> Z2 -> Z3 and wrap back to Z1 (skipping Z0 and inactive Z4)
      const tab1 = mgr.handleKeyDown({ key: "Tab" });
      assert(tab1.activeFocusId === "z1-drawer-close-btn", "Tab moves to next Z1 control");
      mgr.handleKeyDown({ key: "Tab" }); // z2-window-taskmgr
      mgr.handleKeyDown({ key: "Tab" }); // z3-taskbar-omnibar-btn
      mgr.handleKeyDown({ key: "Tab" }); // z3-taskbar-tray-btn
      const wrapRes = mgr.handleKeyDown({ key: "Tab" });
      assert(wrapRes.activeFocusId === "z1-task-row-alpha", "Tab wraps cleanly back to first Z1 control");

      // Shift+Tab backward wrap
      const backRes = mgr.handleKeyDown({ key: "Tab", shiftKey: true });
      assert(backRes.activeFocusId === "z3-taskbar-tray-btn", "Shift+Tab wraps to last Z3 control");

      // Roving tabindex on 10-layer ownership treegrid
      const snap0 = mgr.registerRovingGroup("ownership-tree", "treegrid", [
        { id: "tree-l1-sys", rowIndex: 0, expandable: true, expanded: false },
        { id: "tree-l2-ws", rowIndex: 1, parentId: "tree-l1-sys", expandable: true, expanded: true },
        { id: "tree-l3-app", rowIndex: 2, parentId: "tree-l2-ws", expandable: false },
      ]);
      assert(snap0.tabIndexByNodeId["tree-l1-sys"] === 0, "Active roving node has tabindex=0");
      assert(snap0.tabIndexByNodeId["tree-l2-ws"] === -1, "Inactive roving node has tabindex=-1");

      // While tree-l1-sys is collapsed, ArrowDown stays on tree-l1-sys
      mgr.handleKeyDown({ key: "ArrowDown" }, "ownership-tree");
      assert(mgr.getRovingGroupSnapshot("ownership-tree").activeNodeId === "tree-l1-sys", "Collapsed children skipped");

      // ArrowRight expands tree-l1-sys, then ArrowDown enters tree-l2-ws -> tree-l3-app
      const expRes = mgr.handleKeyDown({ key: "ArrowRight" }, "ownership-tree");
      assert(expRes.action === "ROVING_EXPAND", "ArrowRight expands collapsed treegrid parent");
      mgr.handleKeyDown({ key: "ArrowDown" }, "ownership-tree");
      assert(mgr.getRovingGroupSnapshot("ownership-tree").activeNodeId === "tree-l2-ws", "ArrowDown moves into expanded child");
      mgr.handleKeyDown({ key: "End" }, "ownership-tree");
      assert(mgr.getRovingGroupSnapshot("ownership-tree").activeNodeId === "tree-l3-app", "End jumps to last visible node");
      mgr.handleKeyDown({ key: "ArrowLeft" }, "ownership-tree");
      assert(mgr.getRovingGroupSnapshot("ownership-tree").activeNodeId === "tree-l2-ws", "ArrowLeft on leaf jumps to parent");

      // Global shortcuts: Ctrl+Space, Ctrl+K, Ctrl+Shift+E
      const omni = mgr.handleKeyDown({ key: " ", ctrlKey: true });
      assert(omni.shortcutTriggered === "TOGGLE_OMNIBAR_PALETTE", "Ctrl+Space triggers Omnibar");
      const spot = mgr.handleKeyDown({ key: "k", ctrlKey: true });
      assert(spot.shortcutTriggered === "TOGGLE_SPOTLIGHT_SEARCH", "Ctrl+K triggers Spotlight");
      const l3Toggle = mgr.handleKeyDown({ key: "E", ctrlKey: true, shiftKey: true });
      assert(l3Toggle.shortcutTriggered === "TOGGLE_L3_DIAGNOSTIC", "Ctrl+Shift+E toggles L3 diagnostic tier");

      // Escape dismissal stack
      mgr.requestProgrammaticFocus("z3-taskbar-omnibar-btn");
      mgr.pushDismissalLayer("Z3_COMMAND_PALETTE", "z3-taskbar-omnibar-btn");
      const esc = mgr.handleKeyDown({ key: "Escape" });
      assert(
        esc.dismissedLayer === "Z3_COMMAND_PALETTE" && esc.restoredFocusId === "z3-taskbar-omnibar-btn",
        "Escape dismisses top Z3 palette and restores focus to trigger",
      );
    },
  );

  await runSuite(
    "4. Z4 modal focus trap ([TUI-CONSENT-001]): strict cycle inside Z4 & zero escape to Z0..Z3",
    () => {
      const trapReport = verifyZ4ModalFocusTrap(SAMPLE_FOCUS_TARGETS, "z3-taskbar-omnibar-btn");
      assert(trapReport.z4ModalActive, "Z4 modal trap verified");
      assert(!trapReport.leakedToLowerPlane, "Tab / Shift+Tab never leaked outside Z4");
      assert(
        trapReport.z4ControlIds.length === 2 &&
          trapReport.forwardCycleOrder[0] === "z4-consent-approve-btn" &&
          trapReport.forwardCycleOrder[1] === "z4-consent-deny-btn" &&
          trapReport.forwardCycleOrder[2] === "z4-consent-approve-btn",
        "Forward Tab wraps strictly between Z4 approve and deny buttons",
      );
      assert(trapReport.blockedExternalFocusAttempts >= 3, "External focus & global shortcut attempts blocked");
      assert(trapReport.blockedGlobalShortcutsUnderZ4, "Ctrl+Space and Ctrl+K blocked while Z4 is active");
      assert(
        trapReport.focusRestoredOnDismiss && trapReport.restoredTargetId === "z3-taskbar-omnibar-btn",
        "Escape restores focus to originating trigger",
      );
    },
  );

  await runSuite(
    "5. Deterministic ARIA semantic tree auditor, >=26px target size, drag-equivalents & dual aria-live",
    () => {
      for (const locale of ["zh-CN", "en-US"] as const) {
        for (const z4Active of [false, true]) {
          const tree = buildCanonicalWorkstationAriaTree({
            locale,
            z4ModalActive: z4Active,
            l2DrawerOpen: true,
            omnibarOpen: !z4Active,
            spotlightOpen: !z4Active,
          });
          const audit = auditWorkstationAriaTree(tree, { locale, z4ModalActive: z4Active });
          assert(
            audit.passed && audit.violations.length === 0,
            `Canonical ARIA tree passes audit (locale=${locale}, z4Active=${String(z4Active)})`,
          );
          assert(audit.landmarkRolesPresent.includes("main"), "Has main landmark");
          assert(audit.landmarkRolesPresent.includes("region"), "Has region landmark");
          assert(audit.landmarkRolesPresent.includes("complementary"), "Has complementary L2 drawer");
          assert(audit.landmarkRolesPresent.includes("contentinfo"), "Has contentinfo taskbar");
          assert(audit.landmarkRolesPresent.includes("grid"), "Has grid role");
          assert(audit.landmarkRolesPresent.includes("treegrid"), "Has treegrid role");
          assert(audit.landmarkRolesPresent.includes("alertdialog"), "Has Z4 alertdialog role");
          assert(audit.landmarkRolesPresent.includes("progressbar"), "Has progressbar role");
        }
      }

      // Negative ARIA audit checks
      const brokenNode: AriaSemanticNode = {
        id: "bad-btn",
        plane: "Z1",
        role: "button",
        accessibleName: "Action with 0x1F jargon",
        interactive: true,
        hitTargetWidthPx: 18,
        hitTargetHeightPx: 18,
        requiresDragEquivalent: true,
        keyboardDragEquivalentMenuId: null,
        state: {},
        children: [],
      };
      const negAudit = auditWorkstationAriaTree([brokenNode]);
      assert(!negAudit.passed, "Broken ARIA node rejected");
      assert(
        negAudit.violations.some((v) => v.ruleId === "HIT_TARGET_BELOW_26PX") &&
          negAudit.violations.some((v) => v.ruleId === "DRAG_CONTROL_MISSING_KEYBOARD_EQUIVALENT") &&
          negAudit.violations.some((v) => v.ruleId === "JARGON_IN_ACCESSIBLE_NAME"),
        "Detects <26px hit target, missing drag keyboard equivalent, and L1 jargon in aria-label",
      );

      // Dual-channel aria-live announcer
      const announcer = new WorkstationLiveRegionAnnouncer();
      const politeRec = announcer.announceTaskStateTransition("zh-CN", "季度财报汇总", "COMPLETED");
      assert(politeRec.politeness === "polite" && politeRec.message.includes("已完成"), "L1 transition is polite");
      const quarantineRec = announcer.announceTaskStateTransition("en-US", "Node-B Worker", "QUARANTINED");
      assert(
        quarantineRec.politeness === "assertive" && quarantineRec.message.includes("controlled isolation"),
        "QUARANTINED transition escalates to assertive",
      );
      const z4Rec = announcer.announceZ4SecurityPrompt("zh-CN", "清空消息通道");
      assert(z4Rec.politeness === "assertive", "Z4 security prompt is assertive");
    },
  );

  await runSuite(
    "6. WCAG 2.2 AA relative luminance contrast (>=4.5:1 normal text, >=3.0:1 2px focus ring) & Reduced Motion",
    () => {
      const contrast = auditWorkstationContrast(CANONICAL_WORKSTATION_CONTRAST_PAIRS);
      assert(contrast.passed && contrast.failures.length === 0, "All 20 Graphite & Paper token pairs pass WCAG 2.2 AA");
      assert(contrast.totalPairsAudited === 20, "Audited 20 canonical token pairs across Graphite & Paper");
      assert(
        contrast.minNormalTextRatioObserved >= WCAG_AA_NORMAL_TEXT_MIN_RATIO,
        `Min normal text ratio ${contrast.minNormalTextRatioObserved}:1 >= 4.5:1`,
      );
      assert(
        contrast.minUiComponentRatioObserved >= WCAG_AA_UI_COMPONENT_MIN_RATIO,
        `Min UI focus ring ratio ${contrast.minUiComponentRatioObserved}:1 >= 3.0:1`,
      );
      assert(FOCUS_RING_WIDTH_PX === 2 && WCAG_MIN_HIT_TARGET_PX === 26, "2px focus ring & 26px target size constants");
      assert(computeContrastRatio("#ecedee", "#18191c") >= 14.0, "Graphite primary text contrast >= 14:1");
      assert(computeContrastRatio("#111827", "#ffffff") >= 17.0, "Paper primary text contrast >= 17:1");

      const motion = new ReducedMotionController(false);
      motion.setZ4SealMounted(true);
      assert(
        !motion.getSnapshot().effectiveReducedMotion &&
          motion.getSnapshot().geronoVerificationSealMode === "animated-canvas" &&
          motion.getSnapshot().activeRafLoopCount === 1 &&
          motion.getSnapshot().fastTransitionDurationMs === 80,
        "Standard motion runs 1 RAF loop when Z4 seal is mounted",
      );

      motion.setSystemPrefersReducedMotion(true);
      assert(
        motion.getSnapshot().effectiveReducedMotion &&
          motion.getSnapshot().geronoVerificationSealMode === "static-reduced-motion" &&
          motion.getSnapshot().activeRafLoopCount === 0 &&
          motion.getSnapshot().fastTransitionDurationMs === 0 &&
          motion.getSnapshot().baseTransitionDurationMs === 0,
        "prefers-reduced-motion freezes Gerono seal to static closed curve (0 RAF loops, 0ms transitions)",
      );

      const suite = createWorkstationA11yI18nSuiteController("zh-CN", false);
      suite.setReducedMotionOverride(true);
      suite.switchLocale("en-US");
      const snap = suite.getConformanceSnapshot();
      assert(
        snap.locale === "en-US" &&
          snap.reducedMotion.effectiveReducedMotion &&
          snap.contrastAudit.passed &&
          snap.ariaAudit.passed &&
          snap.parityAudit.passed &&
          snap.zeroJargonAudit.passed,
        "Unified WorkstationA11yI18nSuiteController snapshot passes all gates",
      );
    },
  );

  await runSuite("7. Static CSS & Source ANTI-AI-01..07 Conformance Audit", async () => {
    const fs = await importBuiltin<NodeFsLike>("node:fs");
    const path = await importBuiltin<NodePathLike>("node:path");
    const url = await importBuiltin<NodeUrlLike>("node:url");

    const thisDir = path.dirname(url.fileURLToPath(import.meta.url));
    const cssPath = path.resolve(thisDir, "../a11y.css");
    const cssContent = fs.readFileSync(cssPath, "utf-8");

    assert(!cssContent.includes("backdrop-filter"), "Zero backdrop-filter in a11y.css (ANTI-AI-02)");
    assert(
      !cssContent.toLowerCase().includes("#030710") && !cssContent.toLowerCase().includes("#7fc8ff"),
      "Zero neon glow hex codes in a11y.css (ANTI-AI-01)",
    );

    const radiusMatches = [...cssContent.matchAll(/border-radius:\s*([^;]+);/g)];
    const allowedRadii = new Set(["0px", "2px", "4px", "6px"]);
    assert(radiusMatches.length > 0, "a11y.css defines explicit industrial border-radius rules");
    for (const m of radiusMatches) {
      const val = m[1]!.trim();
      assert(allowedRadii.has(val), `border-radius "${val}" is strictly in {0px, 2px, 4px, 6px} (ANTI-AI-04)`);
    }

    assert(
      interpolateMessage("Row {row}", { row: 1 }) === "Row 1",
      "Parameter interpolation helper verified",
    );
  });

  console.log(
    `\nALL ${passedSuites} SUITES PASSED (${passedAssertions} deterministic assertions verified).`,
  );
}

main().catch((err: unknown) => {
  console.error("W73-D-A11Y-I18N test suite failed:", err);
  const proc = (globalThis as { process?: { exitCode?: number } }).process;
  if (proc) {
    proc.exitCode = 1;
  }
});
