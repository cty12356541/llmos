// ============================================================================
// NLOS Stage D (W73-D-GUI-CAMPAIGN) — Deterministic E2E Verification Suite
// (`desktop/e2e/__tests__/gui-campaign.test.ts`)
//
// Run from `d:\llmos\desktop`:
//   npx tsx e2e/__tests__/gui-campaign.test.ts
// ============================================================================

import {
  CROSS_PLATFORM_WORKSTATION_PROFILES,
  DEFAULT_SOAK_CYCLES,
  LOGICAL_TASK_POOL_TOTAL,
  MAX_RESIDENT_ROWS_PER_DOMAIN,
  runCjkImeCompositionProbe,
  runFocusAndSessionFencingCampaign,
  runFullCrossPlatformGuiCampaign,
  runGracefulDegradationTrilogyCampaign,
  runImeAndMultimodalCampaignProbe,
  runLongRunWorkstationSoakDetector,
  runMultiWindowTilingAndSplitCampaign,
  runMultimodalVoiceAndAttachmentProbe,
  runStatuslineWatermarkAndRecoveryCampaign,
  runWorkstationCampaignMatrix,
  runZ4TrustedUiAndConsentCampaign,
} from "../index";

let totalAssertions = 0;
let passedSuites = 0;

function check(condition: unknown, description: string): asserts condition {
  totalAssertions += 1;
  if (!condition) {
    throw new Error(`Assertion #${totalAssertions} FAILED: ${description}`);
  }
}

async function runSuite(name: string, fn: () => Promise<void> | void): Promise<void> {
  await fn();
  passedSuites += 1;
  console.log(`  [PASS] Suite ${passedSuites}: ${name}`);
}

async function main(): Promise<void> {
  console.log("=== W73-D-GUI-CAMPAIGN: Cross-Platform Workstation GUI Campaign Suite ===");

  // Suite 1: Multi-window tiling & drag/split ([DUI-WINDOW-001])
  await runSuite(
    "Multi-window tiling (4px gap, 4 presets, 6px floating, 30px titlebar, L0 generation routing)",
    () => {
      const receipt = runMultiWindowTilingAndSplitCampaign({
        viewportWidth: 1440,
        viewportHeight: 900,
      });
      check(receipt.passed, "Tiling campaign passed");
      check(receipt.tileGapPx === 4, "4px tiling gap verified");
      check(receipt.floatingRadiusPx === 6, "6px floating radius verified");
      check(receipt.tiledRadiusPx === 0, "0px tiled radius verified");
      check(receipt.titlebarHeightPx === 30, "30px WindowFrame titlebar verified");
      check(receipt.taskbarHeightPx === 36, "36px Z3 taskbar height verified");
      check(receipt.presetsVerified.length === 4, "All 4 tiling presets verified");
      check(receipt.splitDragVerified, "Split sash drag verified");
      check(receipt.l0GenerationRoutingVerified, "L0 surface generation routing verified");
    },
  );

  // Suite 2: Focus & session generation fencing ([DUI-INPUT-001], [DUI-SESSION-001])
  await runSuite(
    "Focus & session generation fencing (lock/switch/suspend/stale drops all 5 input sources)",
    () => {
      const receipt = runFocusAndSessionFencingCampaign();
      check(receipt.passed, "Session fencing campaign passed");
      check(
        receipt.afterLockGeneration > receipt.initialGeneration,
        "Lock session increments generation",
      );
      check(
        receipt.afterUserSwitchGeneration > receipt.afterLockGeneration,
        "User switch increments generation",
      );
      check(
        receipt.afterSuspendGeneration > receipt.afterUserSwitchGeneration,
        "Suspend session increments generation",
      );
      check(
        receipt.droppedSourcesDuringLock.length === 5,
        "All 5 input sources dropped during session lock",
      );
      check(
        receipt.droppedSourcesDuringStaleSurface.length === 5,
        "All 5 input sources dropped on stale surface generation",
      );
      check(
        receipt.untrustedSpoofRejectedCount >= 1,
        "Untrusted surface Z4 spoof rejected and audited",
      );
      check(
        receipt.sensitiveProjectionRowsAfterLock === 0,
        "Sensitive projection rows wiped to 0 on session lock",
      );
    },
  );

  // Suite 3: Z4 Trusted UI consent & clickjacking defense ([TUI-CONSENT-001], [DUI-CHOOSER-001], [DUI-CLIP-001])
  await runSuite(
    "Z4 Trusted UI (350ms arming, 32x20 Gerono seal, 4-row KV table, IRREVERSIBLE zero-undo, ChooserGrant, taint clipboard)",
    () => {
      const receipt = runZ4TrustedUiAndConsentCampaign();
      check(receipt.passed, "Z4 Trusted UI campaign passed");
      check(receipt.z4PlaneZIndex === 900, "Z4 plane z-index === 900");
      check(receipt.topBarHeightPx === 2, "2px steel-blue security top bar");
      check(
        receipt.sealDimensionsPx[0] === 32 && receipt.sealDimensionsPx[1] === 20,
        "32x20 recessed monochrome Gerono seal",
      );
      check(receipt.zeroBlurFilter, "100% solid surface (zero blur filter)");
      check(receipt.armingWindowMs === 350, "350ms arming gate");
      check(
        receipt.earlyConfirmRejectedCode === "CLICKJACKING_ARMING_ACTIVE",
        "Early confirm <350ms rejected with CLICKJACKING_ARMING_ACTIVE",
      );
      check(receipt.fourRowSpecTableVerified, "4-row KV spec table rendered");
      check(receipt.irreversibleZeroUndoLocked, "IRREVERSIBLE zero-undo lock enforced");
      check(
        receipt.chooserZeroHostPathLeakVerified,
        "ChooserGrant zero bare host path leak verified",
      );
      check(
        receipt.highTaintClipboardEscalatedToZ4,
        "High-taint cross-app clipboard read escalated to Z4 consent",
      );
      check(
        receipt.untrustedAutomationGestureRejected,
        "Automation gesture minting rejected",
      );
    },
  );

  // Suite 4: Statusline & real-time watermark hooks ([CTRL-RECOVERY-001])
  await runSuite(
    "Statusline (36px Z3 dock), monotonic PollDesktopWatermarkEvents, <16ms L1->L2->L3, and ControlCommandId reconciliation",
    async () => {
      const receipt = await runStatuslineWatermarkAndRecoveryCampaign();
      check(receipt.passed, "Statusline & watermark campaign passed");
      check(receipt.statuslineHeightPx === 36, "Statusline height === 36px");
      check(receipt.statuslineZIndex === 400, "Statusline z-index === 400");
      check(
        receipt.monotonicWatermarksObserved.length === 3 &&
          receipt.monotonicWatermarksObserved[0]! < receipt.monotonicWatermarksObserved[2]!,
        "Watermarks advance strictly monotonically",
      );
      check(
        receipt.maxDisclosureTransitionLatencyMs < receipt.disclosureBudgetCeilingMs,
        `Disclosure transitions (${receipt.maxDisclosureTransitionLatencyMs}ms) < 16ms budget`,
      );
      check(
        receipt.blindRetryBlockedCode === "BLIND_RETRY_FORBIDDEN_ON_UNKNOWN",
        "Blind retry blocked with BLIND_RETRY_FORBIDDEN_ON_UNKNOWN",
      );
      check(
        receipt.reconciledOutcomeStatus === "SUCCEEDED",
        "ControlCommandId reconciled to SUCCEEDED",
      );
      check(
        receipt.dataLossReceiptIssuedHex.length === 32,
        "32-hex DataLossReceipt issued on TopicPurge",
      );
    },
  );

  // Suite 5: 3-stage graceful degradation (降级三连: ProviderCache -> PartitionAutonomyGate -> Ambiguity/Stale fence)
  await runSuite(
    "3-stage graceful degradation (ProviderCache fallback, [DUI-OFFLINE-001] 6-surface autonomy, [NL-AMBIG-001] & STALE_GENERATION)",
    () => {
      const receipt = runGracefulDegradationTrilogyCampaign();
      check(receipt.passed, "3-stage graceful degradation campaign passed");
      check(
        receipt.stage1ProviderQuotaFallback.providerCacheDeterministicHit &&
          receipt.stage1ProviderQuotaFallback.zeroTaskStateCorruption,
        "Stage 1 local deterministic ProviderCache fallback verified",
      );
      check(
        receipt.stage2NetworkPartitionAutonomy.partitionAutonomyGate === "PARTITIONED" &&
          receipt.stage2NetworkPartitionAutonomy.autonomousSurfacesVerified === 6 &&
          receipt.stage2NetworkPartitionAutonomy.remoteDispatchStatus === "WAITING_NETWORK" &&
          receipt.stage2NetworkPartitionAutonomy.quarantinedCellStatus ===
            "BLOCKED_QUARANTINED_FENCE",
        "Stage 2 [DUI-OFFLINE-001] 6-surface autonomy + WAITING_NETWORK + QUARANTINED fence verified",
      );
      check(
        receipt.stage3AmbiguityAndStaleFence.ambiguityBlockedCode ===
          "CLARIFICATION_REQUIRED" &&
          receipt.stage3AmbiguityAndStaleFence.resolvedAfterSelection &&
          receipt.stage3AmbiguityAndStaleFence.staleSpotlightRejectedCode ===
            "STALE_GENERATION" &&
          receipt.stage3AmbiguityAndStaleFence.staleWindowBlockedReason ===
            "STALE_GENERATION" &&
          receipt.stage3AmbiguityAndStaleFence.recoveredAfterGenerationReload,
        "Stage 3 [NL-AMBIG-001] clarification & STALE_GENERATION refresh fence verified",
      );
    },
  );

  // Suite 6: CJK IME Composition & Multimodal Input Probe
  await runSuite(
    "CJK IME composition (zh-CN/ja-JP/en-US Enter/Escape guard in Z3 Omnibar & Ctrl+K Spotlight) + Voice & ChooserGrant chips",
    () => {
      const imeReceipt = runCjkImeCompositionProbe();
      check(imeReceipt.passed, "CJK IME composition probe passed");
      check(
        imeReceipt.localesTested.length === 3,
        "zh-CN, ja-JP, and en-US mixed IME lifecycles tested",
      );
      check(
        imeReceipt.enterDuringCompositionGuarded,
        "Enter during active IME composition guarded against premature submit",
      );
      check(
        imeReceipt.escapeDuringCompositionGuarded,
        "Escape during active IME composition guarded against premature Omnibar dismissal",
      );
      check(
        imeReceipt.postCompositionSubmitSucceeded,
        "Post-composition Enter submits compiled Omnibar proposal cleanly",
      );
      check(
        imeReceipt.spotlightCjkHitsMatched >= 1,
        "Ctrl+K Spotlight CJK IME query matches local candidate",
      );

      const mmReceipt = runMultimodalVoiceAndAttachmentProbe();
      check(mmReceipt.passed, "Multimodal voice & attachment probe passed");
      check(
        mmReceipt.micIndicatorActiveDuringCapture,
        "L0 taskbar microphone indicator active during voice capture",
      );
      check(
        mmReceipt.micSeveredOnZ4LockReason === "Z4_MODAL_ACTIVE",
        "Z4 security screen immediately severs active voice capture",
      );
      check(
        mmReceipt.attachmentChipCount === 3 && mmReceipt.attachmentChipRadiusPx === 4,
        "3 multimodal attachment chips rendered at 4px radius",
      );
      check(
        mmReceipt.rawWindowsHostPathStrippedToBasename === "内部审计纪要.pdf",
        "Raw Windows host path stripped to safe basename",
      );
      check(
        mmReceipt.missingChooserGrantRejectedCode === "MISSING_CHOOSER_GRANT",
        "Ungranted local file path rejected with MISSING_CHOOSER_GRANT",
      );
      check(mmReceipt.renderedChipsZeroBlur, "Zero blur and zero bare path leak in chip HTML");

      const combined = runImeAndMultimodalCampaignProbe();
      check(combined.probeDigestHex.length === 64, "64-hex IME & multimodal digest minted");
    },
  );

  // Suite 7: 1,000-cycle Workstation Soak & O(visible_rows) Leak Detector
  await runSuite(
    "1,000-cycle workstation soak (100K logical tasks paged at limit<=64, O(visible_rows) resident memory, 0 DOM/listener leaks)",
    async () => {
      const soakReport = await runLongRunWorkstationSoakDetector({
        cycles: DEFAULT_SOAK_CYCLES,
        pageCursorLimit: 64,
      });
      check(
        soakReport.totalCyclesExecuted === 1_000,
        "1,000 soak cycles executed",
      );
      check(
        soakReport.logicalTaskPoolTotal === LOGICAL_TASK_POOL_TOTAL,
        "100,000 logical tasks in pool",
      );
      check(
        soakReport.totalLogicalTasksVisited === 64_000,
        "Visited 64,000 paged task records across 1,000 cycles",
      );
      check(
        soakReport.maxResidentTasksInStore <= MAX_RESIDENT_ROWS_PER_DOMAIN,
        `Max resident tasks (${soakReport.maxResidentTasksInStore}) <= 64`,
      );
      check(
        soakReport.maxResidentTaskNodesInStore <= MAX_RESIDENT_ROWS_PER_DOMAIN,
        `Max resident task nodes (${soakReport.maxResidentTaskNodesInStore}) <= 64`,
      );
      check(
        soakReport.domNodeGrowthAfterWarmup === 0,
        "0 unbounded DOM node growth between cycle 100 and cycle 1,000",
      );
      check(
        soakReport.actionTokensRedeemed === 64 &&
          soakReport.actionTokenReplayRejections === 64,
        "All 64 ActionTokens redeemed once and all 64 replays rejected",
      );
      check(
        soakReport.activeSubscriberLeakCount === 0 &&
          soakReport.activeTimerLeakCount === 0,
        "0 leaked subscribers or timers after 1,000 cycles",
      );
      check(soakReport.soakDigestHex.length === 64, "64-hex soak receipt digest minted");
    },
  );

  // Suite 8: Full Cross-Platform Matrix (Windows 11 / macOS 14 / Linux)
  await runSuite(
    "Full cross-platform workstation campaign matrix (windows-11-x86_64, macos-14-arm64, linux-x86_64)",
    async () => {
      for (const profile of CROSS_PLATFORM_WORKSTATION_PROFILES) {
        const matrixReport = await runWorkstationCampaignMatrix({
          platformProfile: profile,
        });
        check(
          matrixReport.allStagesPassed && matrixReport.campaignDigestHex.length === 64,
          `Platform profile ${profile} passed all 5 campaign stages`,
        );
      }

      const fullReceipt = await runFullCrossPlatformGuiCampaign({ soakCycles: 1_000 });
      check(fullReceipt.status === "CAMPAIGN_PASSED", "Full campaign status === CAMPAIGN_PASSED");
      check(
        fullReceipt.handoverItemClosed === "#3 C-GUI-CAMPAIGN" &&
          fullReceipt.unknownRetired === "U-3" &&
          fullReceipt.riskReevaluated === "RISK-B-06",
        "Handover #3 C-GUI-CAMPAIGN, U-3, and RISK-B-06 metadata bound",
      );
      check(
        fullReceipt.receiptDigestHex.length === 64,
        "64-hex full campaign receipt digest minted",
      );
    },
  );

  console.log(
    `\n=== ALL ${passedSuites} SUITES PASSED (${totalAssertions} deterministic assertions, 0 failures) ===`,
  );
}

main().catch((err: unknown) => {
  console.error("GUI Campaign Verification FAILED:", err);
  throw err;
});
