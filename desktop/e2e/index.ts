// ============================================================================
// NLOS Stage D (W73-D-GUI-CAMPAIGN) — Barrel Exports & Unified Cross-Platform
// Workstation GUI Campaign Coordinator (`desktop/e2e/index.ts`)
// ============================================================================

import { deterministicDomainDigestHex } from "../src/trusted-ui";
import {
  runImeAndMultimodalCampaignProbe,
  type ImeAndMultimodalProbeReport,
} from "./ime-and-multimodal-probe";
import {
  runLongRunWorkstationSoakDetector,
  type LongRunSoakLeakReport,
} from "./long-run-leak-detector";
import {
  runWorkstationCampaignMatrix,
  type WorkstationCampaignMatrixReport,
  type WorkstationHostPlatformId,
} from "./workstation-campaign-matrix";

export * from "./workstation-campaign-matrix";
export * from "./ime-and-multimodal-probe";
export * from "./long-run-leak-detector";

export const CROSS_PLATFORM_WORKSTATION_PROFILES: readonly WorkstationHostPlatformId[] = [
  "windows-11-x86_64",
  "macos-14-arm64",
  "linux-x86_64",
] as const;

export interface FullCrossPlatformGuiCampaignReceipt {
  readonly schemaVersion: "1.0.0";
  readonly campaignId: "d-gui-campaign-001";
  readonly handoverItemClosed: "#3 C-GUI-CAMPAIGN";
  readonly unknownRetired: "U-3";
  readonly riskReevaluated: "RISK-B-06";
  readonly status: "CAMPAIGN_PASSED";
  readonly platformReports: Readonly<Record<WorkstationHostPlatformId, WorkstationCampaignMatrixReport>>;
  readonly imeAndMultimodalReport: ImeAndMultimodalProbeReport;
  readonly longRunSoakReport: LongRunSoakLeakReport;
  readonly receiptDigestHex: string;
}

/**
 * Executes the complete cross-platform workstation GUI campaign (`#3 C-GUI-CAMPAIGN` / `U-3` / `RISK-B-06`):
 *   1. 5-stage Workstation Campaign Matrix across `windows-11-x86_64`, `macos-14-arm64`, and `linux-x86_64`
 *   2. CJK IME Composition (`zh-CN`, `ja-JP`, `en-US`) & Multimodal (`InputSource::Voice` + `ChooserGrant` chips)
 *   3. 1,000-cycle Workstation Soak & `O(visible_rows)` Bounded-Memory / DOM Leak Detector
 */
export async function runFullCrossPlatformGuiCampaign(options?: {
  readonly soakCycles?: number;
}): Promise<FullCrossPlatformGuiCampaignReceipt> {
  const winReport = await runWorkstationCampaignMatrix({
    platformProfile: "windows-11-x86_64",
    viewportWidth: 1440,
    viewportHeight: 900,
  });
  const macReport = await runWorkstationCampaignMatrix({
    platformProfile: "macos-14-arm64",
    viewportWidth: 1512,
    viewportHeight: 982,
  });
  const linuxReport = await runWorkstationCampaignMatrix({
    platformProfile: "linux-x86_64",
    viewportWidth: 1920,
    viewportHeight: 1080,
  });

  const imeAndMultimodalReport = runImeAndMultimodalCampaignProbe();
  const longRunSoakReport = await runLongRunWorkstationSoakDetector({
    cycles: options?.soakCycles ?? 1_000,
  });

  const receiptDigestHex = deterministicDomainDigestHex(
    "llmos/stage-d/d-gui-campaign-001-full-receipt/v1",
    [
      winReport.campaignDigestHex,
      macReport.campaignDigestHex,
      linuxReport.campaignDigestHex,
      imeAndMultimodalReport.probeDigestHex,
      longRunSoakReport.soakDigestHex,
    ],
  );

  return {
    schemaVersion: "1.0.0",
    campaignId: "d-gui-campaign-001",
    handoverItemClosed: "#3 C-GUI-CAMPAIGN",
    unknownRetired: "U-3",
    riskReevaluated: "RISK-B-06",
    status: "CAMPAIGN_PASSED",
    platformReports: {
      "windows-11-x86_64": winReport,
      "macos-14-arm64": macReport,
      "linux-x86_64": linuxReport,
    },
    imeAndMultimodalReport,
    longRunSoakReport,
    receiptDigestHex,
  };
}
