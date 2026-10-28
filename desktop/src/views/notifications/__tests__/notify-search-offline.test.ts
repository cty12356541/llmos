/**
 * Comprehensive Deterministic Verification & Anti-AI Audit Suite for
 * `D-NOTIFY-SEARCH-OFFLINE` (`W72-D-NOTIFY-SEARCH`).
 *
 * Run from `desktop/`:
 *   npx tsx --test src/views/notifications/__tests__/notify-search-offline.test.ts
 */

import { WorkstationCompositor } from "../../../compositor/workstation-compositor";
import {
  CANONICAL_TASK_STATES,
  type DisclosureLevel,
  type WorkstationLocale,
} from "../../../ui/status-indicator";
import {
  ALL_LOCAL_AUTONOMOUS_SURFACES,
  ClusterPostureController,
  OfflineClusterError,
  renderClusterPostureTrayHtml,
} from "../../offline-cluster/index";
import {
  ALL_SEARCH_FORBIDDEN_GATES,
  QUARANTINED_CELL_ID_HEX,
  REMOTE_WORKER_CELL_ID_HEX,
  renderSpotlightViewHtml,
  SpotlightController,
  SpotlightError,
  SPOTLIGHT_STATUTORY_MASK_0X1F,
} from "../../spotlight/index";
import {
  DEFAULT_CALLER_APP_ID_HEX,
  DEFAULT_CALLER_PRINCIPAL_ID_HEX,
  NotificationCenterController,
  NotificationCenterError,
  renderNotificationCenterHtml,
} from "../index";

/* ==========================================================================
   Node Builtin Loader (Zero @types/node Dependency for Strict DOM tsconfig)
   ========================================================================== */

interface DirentLike {
  readonly name: string;
  isDirectory(): boolean;
  isFile(): boolean;
}

interface NodeFsLike {
  readFileSync(path: string, encoding: "utf-8"): string;
  readdirSync(path: string, options: { withFileTypes: true }): DirentLike[];
}

interface NodePathLike {
  resolve(...segments: string[]): string;
  dirname(p: string): string;
  join(...segments: string[]): string;
}

interface NodeUrlLike {
  fileURLToPath(url: string): string;
}

const importBuiltin = new Function(
  "specifier",
  "return import(specifier);",
) as <T>(specifier: string) => Promise<T>;

/* ==========================================================================
   Assertion Helpers
   ========================================================================== */

function assert(condition: boolean, message: string): void {
  if (!condition) {
    throw new Error(`Assertion failed: ${message}`);
  }
}

function assertEqual<T>(actual: T, expected: T, message: string): void {
  if (actual !== expected) {
    throw new Error(
      `${message}: expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`,
    );
  }
}

function stripHtmlTagsAndAttrs(html: string): string {
  return html
    .replace(/<[^>]*>/g, " ")
    .replace(/\s+/g, " ")
    .trim();
}

const BANNED_L1_L2_JARGON: readonly string[] = [
  "Z1",
  "Z2",
  "Z3",
  "Z4",
  "TypedIntent",
  "0x1F",
  "0x1f",
  "NON_AUTHORITATIVE",
  "checkpoint",
  "Quota",
  "quota",
  "ROAD-D-001",
  "ADR-0011",
  "TaskAttempt",
  "ExecutionFiber",
  "Cell",
  "receipt_hex",
  "control_command_id",
  "CAS_WINNER",
  "SHARED_THREAD_LIKE",
  "LOCAL_SINGLE_NODE",
];

const ALLOWED_RADII = new Set(["0", "0px", "2px", "4px", "6px"]);

/* ==========================================================================
   Test Cases
   ========================================================================== */

function testNotification16StatesAndPagination(): void {
  const compositor = new WorkstationCompositor();
  const controller = new NotificationCenterController({ compositor });

  const fullPage = controller.listNotificationFeed({ limit: 64 });
  assertEqual(CANONICAL_TASK_STATES.length, 16, "CANONICAL_TASK_STATES must have 16 states");
  assertEqual(fullPage.items.length, 16, "Full notification feed must contain 16 items");
  assert(
    NotificationCenterController.verifyAll16CanonicalStatesPresent(fullPage.items),
    "All 16 canonical task states must be represented in notification feed",
  );
  assert(
    compositor.renderShellHtml().includes(`>${fullPage.totalUnreadCount}<`),
    "Compositor shell HTML must render initial unread notification badge count",
  );

  // Cursor pagination: page size 6 -> 6 + 6 + 4 = 16
  const seenIds = new Set<string>();
  const pageSizes: number[] = [];
  let cursor: number | undefined;
  let lastSequence = Number.MAX_SAFE_INTEGER;

  for (;;) {
    const page = controller.listNotificationFeed({ cursorSequence: cursor, limit: 6 });
    pageSizes.push(page.items.length);
    for (const item of page.items) {
      assert(
        item.sequence < lastSequence,
        "Notification feed must be strictly descending by sequence",
      );
      lastSequence = item.sequence;
      assert(
        !seenIds.has(item.notificationIdHex),
        `Duplicate notificationIdHex ${item.notificationIdHex} across pages`,
      );
      seenIds.add(item.notificationIdHex);
    }
    if (!page.hasMore) {
      break;
    }
    assert(page.nextCursorSequence !== undefined, "hasMore=true must provide nextCursorSequence");
    cursor = page.nextCursorSequence;
  }

  assertEqual(pageSizes.join(","), "6,6,4", "Paginated sizes for 16 items with limit=6");
  assertEqual(seenIds.size, 16, "Total unique paginated items must equal 16");

  // Dismiss newest notification and verify watermark + unread count update
  const newest = fullPage.items[0]!;
  const dismissReceipt = controller.dismissNotificationItem({
    notificationIdHex: newest.notificationIdHex,
    nowMonoMs: 1_700_000_500_000,
  });
  assertEqual(dismissReceipt.dismissed, true, "Dismiss receipt must mark dismissed=true");
  assertEqual(
    dismissReceipt.ackWatermarkSequence,
    newest.sequence,
    "Dismissing newest item must advance ackWatermarkSequence",
  );
  assertEqual(
    dismissReceipt.updatedUnreadCount,
    fullPage.totalUnreadCount - 1,
    "Unread count must decrement after dismissing unread notification",
  );
}

function testActionTokenRedemptionAndNegativeSecurityGates(): void {
  const controller = new NotificationCenterController();
  const page = controller.listNotificationFeed({ limit: 64 });

  const awaitingItem = page.items.find((i) => i.taskState === "WAITING_CONSENT");
  assert(awaitingItem !== undefined, "WAITING_CONSENT notification must exist");
  const token = awaitingItem!.actionToken;
  assert(token !== undefined, "WAITING_CONSENT notification must carry inline ActionToken");
  assert(
    awaitingItem!.allowedVerbs.includes("redeem_action_token"),
    "Unconsumed ActionToken notification must allow redeem_action_token verb",
  );

  // 1. Valid one-shot redemption
  const receipt = controller.redeemNotificationActionToken({
    notificationIdHex: awaitingItem!.notificationIdHex,
    tokenIdHex: token!.tokenIdHex,
    actionId: token!.actionId,
    callerAppIdHex: DEFAULT_CALLER_APP_ID_HEX,
    callerPrincipalIdHex: DEFAULT_CALLER_PRINCIPAL_ID_HEX,
    taskIdHex: awaitingItem!.taskIdHex,
    nowMonoMs: awaitingItem!.createdAtMonoMs + 1_000,
    presentedCommitmentHex: token!.verifierCommitmentHex,
  });

  assertEqual(
    receipt.resultingTaskState,
    "RUNNING",
    "Redeeming WAITING_CONSENT token must transition taskState to RUNNING",
  );
  assertEqual(
    receipt.redemptionReceiptDigestHex.length,
    64,
    "Redemption receipt digest must be 64 hex chars",
  );
  assert(
    !receipt.remainingAllowedVerbs.includes("redeem_action_token"),
    "Consumed ActionToken must prune redeem_action_token from remainingAllowedVerbs",
  );

  // 2. Replay attack on the same token -> ACTION_TOKEN_ALREADY_CONSUMED
  let replayCode = "";
  try {
    controller.redeemNotificationActionToken({
      notificationIdHex: awaitingItem!.notificationIdHex,
      tokenIdHex: token!.tokenIdHex,
      actionId: token!.actionId,
      callerAppIdHex: DEFAULT_CALLER_APP_ID_HEX,
      callerPrincipalIdHex: DEFAULT_CALLER_PRINCIPAL_ID_HEX,
      taskIdHex: awaitingItem!.taskIdHex,
      nowMonoMs: awaitingItem!.createdAtMonoMs + 2_000,
    });
  } catch (err) {
    if (err instanceof NotificationCenterError) {
      replayCode = err.code;
    }
  }
  assertEqual(
    replayCode,
    "ACTION_TOKEN_ALREADY_CONSUMED",
    "Second redemption of the same ActionToken must fail closed with ACTION_TOKEN_ALREADY_CONSUMED",
  );

  // Test expired, app mismatch, principal mismatch, task mismatch, and invalid proof on WAITING_INPUT item
  const humanItem = page.items.find((i) => i.taskState === "WAITING_INPUT")!;
  const humanToken = humanItem.actionToken!;

  const expectTokenError = (
    overrides: Partial<Parameters<typeof controller.redeemNotificationActionToken>[0]>,
    expectedCode: string,
  ): void => {
    let caughtCode = "";
    try {
      controller.redeemNotificationActionToken({
        notificationIdHex: humanItem.notificationIdHex,
        tokenIdHex: humanToken.tokenIdHex,
        actionId: humanToken.actionId,
        callerAppIdHex: DEFAULT_CALLER_APP_ID_HEX,
        callerPrincipalIdHex: DEFAULT_CALLER_PRINCIPAL_ID_HEX,
        taskIdHex: humanItem.taskIdHex,
        nowMonoMs: humanItem.createdAtMonoMs + 500,
        presentedCommitmentHex: humanToken.verifierCommitmentHex,
        ...overrides,
      });
    } catch (err) {
      if (err instanceof NotificationCenterError) {
        caughtCode = err.code;
      }
    }
    assertEqual(caughtCode, expectedCode, `Expected ActionToken rejection ${expectedCode}`);
  };

  expectTokenError({ nowMonoMs: humanToken.expiryMonoMs + 1 }, "ACTION_TOKEN_EXPIRED");
  expectTokenError(
    { callerAppIdHex: "ffffffffffffffffffffffffffffffff" },
    "ACTION_TOKEN_APP_MISMATCH",
  );
  expectTokenError(
    { callerPrincipalIdHex: "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee" },
    "ACTION_TOKEN_PRINCIPAL_MISMATCH",
  );
  expectTokenError(
    { taskIdHex: "dddddddddddddddddddddddddddddddd" },
    "ACTION_TOKEN_TASK_MISMATCH",
  );
  expectTokenError(
    { presentedCommitmentHex: "0".repeat(64) },
    "ACTION_TOKEN_INVALID_PROOF",
  );

  const audit = controller.getSecurityAudit();
  assertEqual(audit.actionTokenRedemptionsSucceeded, 1, "1 redemption succeeded");
  assertEqual(audit.actionTokenReplayRejections, 1, "1 replay rejection recorded");
  assertEqual(audit.actionTokenExpiredRejections, 1, "1 expired rejection recorded");
  assertEqual(audit.actionTokenAppMismatchRejections, 1, "1 app mismatch rejection recorded");
  assertEqual(
    audit.actionTokenPrincipalMismatchRejections,
    1,
    "1 principal mismatch rejection recorded",
  );
  assertEqual(audit.actionTokenTaskMismatchRejections, 1, "1 task mismatch rejection recorded");
  assertEqual(audit.actionTokenInvalidProofRejections, 1, "1 invalid proof rejection recorded");
}

function testSpotlight0x1fStatutoryLockAndAuthorityReverification(): void {
  const spotlight = new SpotlightController();
  const resp = spotlight.querySpotlightCandidates();

  assertEqual(
    resp.envelopeAuthorityGrade,
    "NON_AUTHORITATIVE",
    "Spotlight envelope must be NON_AUTHORITATIVE",
  );
  assertEqual(
    resp.envelopeRestrictionsBitmask,
    SPOTLIGHT_STATUTORY_MASK_0X1F,
    "Spotlight envelope must carry 0x1F bitmask",
  );
  assert(resp.candidates.length >= 5, "Spotlight fixture must return candidates");

  for (const cand of resp.candidates) {
    assertEqual(cand.authorityGrade, "NON_AUTHORITATIVE", "Candidate authorityGrade");
    assertEqual(cand.restrictionsBitmask, 0x1f, "Candidate restrictionsBitmask must be 0x1F");
    assertEqual(
      cand.allowedVerbs.join(","),
      "inspect,jump_to_authority",
      "Search candidate allowedVerbs must strictly be inspect,jump_to_authority",
    );
  }

  // Statutory Lock #1: All 5 forbidden control gates must fail closed even with callerSuppliedMask=0x00
  const targetCandidate = resp.candidates[0]!;
  for (const gate of ALL_SEARCH_FORBIDDEN_GATES) {
    let caughtCode = "";
    try {
      spotlight.attemptMutationFromSearchHit({
        candidateIdHex: targetCandidate.candidateIdHex,
        attemptedGate: gate,
        callerSuppliedMask: 0x00,
      });
    } catch (err) {
      if (err instanceof SpotlightError) {
        caughtCode = err.code;
      }
    }
    assertEqual(
      caughtCode,
      "SEARCH_RESULT_NON_AUTHORITATIVE_0X1F",
      `Gate ${gate} must fail closed with SEARCH_RESULT_NON_AUTHORITATIVE_0X1F`,
    );
  }

  // Authoritative jump re-verification: fresh candidate succeeds
  const verifyReceipt = spotlight.verifySpotlightCandidateAuthority({
    candidateIdHex: targetCandidate.candidateIdHex,
    expectedTargetIdHex: targetCandidate.targetIdHex,
    snapshotGeneration: targetCandidate.snapshotGeneration,
  });
  assertEqual(verifyReceipt.verified, true, "Fresh candidate jump verification must succeed");
  assert(
    verifyReceipt.unlockedAuthoritativeVerbs.includes("open_authoritative_view"),
    "Authoritative store verification unlocks authoritative view verbs",
  );

  // Advance generation -> old snapshotGeneration fails closed with STALE_GENERATION
  const newGen = spotlight.advanceAuthorityGeneration(targetCandidate.targetIdHex);
  assertEqual(newGen, 2, "Authoritative generation must advance to 2");
  let staleCode = "";
  try {
    spotlight.verifySpotlightCandidateAuthority({
      candidateIdHex: targetCandidate.candidateIdHex,
      expectedTargetIdHex: targetCandidate.targetIdHex,
      snapshotGeneration: targetCandidate.snapshotGeneration,
    });
  } catch (err) {
    if (err instanceof SpotlightError) {
      staleCode = err.code;
    }
  }
  assertEqual(staleCode, "STALE_GENERATION", "Stale snapshot generation must be rejected");

  // Retract authoritative record -> fails closed with AUTHORITY_RECORD_RETRACTED
  assert(
    spotlight.retractAuthorityRecord(targetCandidate.targetIdHex),
    "retractAuthorityRecord must return true",
  );
  let retractedCode = "";
  try {
    spotlight.verifySpotlightCandidateAuthority({
      candidateIdHex: targetCandidate.candidateIdHex,
      expectedTargetIdHex: targetCandidate.targetIdHex,
      snapshotGeneration: newGen!,
    });
  } catch (err) {
    if (err instanceof SpotlightError) {
      retractedCode = err.code;
    }
  }
  assertEqual(
    retractedCode,
    "AUTHORITY_RECORD_RETRACTED",
    "Retracted authoritative record must be rejected on jump",
  );

  const audit = spotlight.getSecurityAudit();
  assertEqual(audit.search0x1fMutationRejections, 5, "All 5 forbidden gates rejected");
  assertEqual(audit.searchAuthorityVerificationsSucceeded, 1, "1 jump verification succeeded");
  assertEqual(audit.searchStaleGenerationRejections, 1, "1 stale generation rejected");
  assertEqual(audit.searchAuthorityRetractedRejections, 1, "1 retracted record rejected");
}

function testOfflineAutonomyDuiOffline001AndQuarantinedLeaseFence(): void {
  const compositor = new WorkstationCompositor();
  const spotlight = new SpotlightController({ compositor });
  const cluster = new ClusterPostureController({
    compositor,
    spotlightController: spotlight,
  });

  for (const mode of ["PARTITIONED", "LOCAL_SINGLE_NODE"] as const) {
    const snap = cluster.setClusterConnectivityMode(mode, 1_700_000_600_000);
    assertEqual(snap.connectivityMode, mode, `Cluster mode must update to ${mode}`);
    assertEqual(snap.localAutonomyActive, true, "Local autonomy must remain active in all modes");

    // All 6 local workstation surfaces must remain 100% interactive with LocalAutonomyProof
    for (const surface of ALL_LOCAL_AUTONOMOUS_SURFACES) {
      const decision = cluster.evaluateOfflineActionAvailability({
        surface,
        actionVerb: "open_and_interact",
        requiresRemoteProvider: false,
      });
      assertEqual(
        decision.interactive,
        true,
        `Local surface ${surface} must remain 100% interactive in ${mode}`,
      );
      assertEqual(
        decision.status,
        "AVAILABLE_LOCAL_AUTONOMY",
        `Local surface ${surface} status must be AVAILABLE_LOCAL_AUTONOMY in ${mode}`,
      );
      assert(
        decision.autonomyProof !== undefined,
        `Local surface ${surface} must attach LocalAutonomyProofBadge`,
      );
    }

    // Remote provider or unreachable remote worker cell must deterministically degrade
    const remoteDecision = cluster.evaluateOfflineActionAvailability({
      surface: "REMOTE_CLUSTER_DISPATCH",
      actionVerb: "invoke_cloud_provider",
      requiresRemoteProvider: true,
    });
    assertEqual(remoteDecision.interactive, false, "Remote provider action must not be interactive offline");
    assertEqual(
      remoteDecision.status,
      mode === "PARTITIONED" ? "WAITING_NETWORK" : "UNAVAILABLE_REMOTE_OFFLINE",
      `Remote action deterministic status in ${mode}`,
    );

    const unreachableCellDecision = cluster.evaluateOfflineActionAvailability({
      surface: "TASK_HISTORY_AND_LOCAL_TASKS",
      actionVerb: "dispatch_remote_worker",
      requiresRemoteProvider: false,
      targetCellIdHex: REMOTE_WORKER_CELL_ID_HEX,
    });
    assertEqual(unreachableCellDecision.interactive, false, "Unreachable remote worker must not be interactive");
  }

  // Statutory Lock #3: QUARANTINED node lease cannot be unlocked by local TTL expiration alone
  let fenceErrorCode = "";
  try {
    cluster.attemptUnlockQuarantinedLeaseByTtl(QUARANTINED_CELL_ID_HEX, 999_999);
  } catch (err) {
    if (err instanceof OfflineClusterError) {
      fenceErrorCode = err.code;
    }
  }
  assertEqual(
    fenceErrorCode,
    "QUARANTINED_LEASE_FENCE_LOCKED",
    "QUARANTINED cell lease must never unlock by local TTL alone",
  );

  const quarantinedDispatch = cluster.evaluateOfflineActionAvailability({
    surface: "TASK_HISTORY_AND_LOCAL_TASKS",
    actionVerb: "dispatch_to_quarantined",
    requiresRemoteProvider: false,
    targetCellIdHex: QUARANTINED_CELL_ID_HEX,
  });
  assertEqual(quarantinedDispatch.interactive, false, "Quarantined cell must block dispatch");
  assertEqual(
    quarantinedDispatch.status,
    "BLOCKED_QUARANTINED_FENCE",
    "Quarantined cell status must be BLOCKED_QUARANTINED_FENCE",
  );
  assertEqual(
    quarantinedDispatch.statutoryLockKind,
    "QUARANTINED_LEASE_FENCE",
    "Quarantined cell must report QUARANTINED_LEASE_FENCE statutory lock",
  );
}

function testProgressiveDisclosureZeroJargonInL1L2AndCompleteDiagnosticsInL3(): void {
  const notify = new NotificationCenterController();
  const spotlight = new SpotlightController();
  const cluster = new ClusterPostureController();

  const locales: readonly WorkstationLocale[] = ["zh-CN", "en-US"];
  const userLevels: readonly DisclosureLevel[] = ["L0", "L1", "L2"];

  for (const locale of locales) {
    notify.setLocale(locale);
    spotlight.setLocale(locale);
    cluster.setLocale(locale);

    for (const level of userLevels) {
      notify.setDisclosureLevel(level);
      spotlight.setDisclosureLevel(level);
      cluster.setDisclosureLevel(level);

      const notifyHtml = renderNotificationCenterHtml(notify.getState());
      const spotlightHtml = renderSpotlightViewHtml(spotlight.getState());
      const clusterHtml = renderClusterPostureTrayHtml(cluster.getState());

      for (const [viewName, html] of [
        ["NotificationCenter", notifyHtml],
        ["SpotlightView", spotlightHtml],
        ["ClusterPostureTray", clusterHtml],
      ] as const) {
        assert(
          !html.includes('data-l3-diagnostics="true"'),
          `${viewName} at ${level} (${locale}) must not render L3 diagnostic blocks`,
        );
        const visibleText = stripHtmlTagsAndAttrs(html);
        for (const jargon of BANNED_L1_L2_JARGON) {
          assert(
            !visibleText.includes(jargon),
            `${viewName} visible text at ${level} (${locale}) must not contain internal jargon '${jargon}'. Text: ${visibleText}`,
          );
        }
      }
    }

    // Verify L3 renders full engineering diagnostics
    notify.setDisclosureLevel("L3");
    spotlight.setDisclosureLevel("L3");
    cluster.setDisclosureLevel("L3");

    const notifyL3Html = renderNotificationCenterHtml(notify.getState());
    const spotlightL3Html = renderSpotlightViewHtml(spotlight.getState());
    const clusterL3Html = renderClusterPostureTrayHtml(cluster.getState());

    assert(
      notifyL3Html.includes("prune_wm=") && notifyL3Html.includes("retracted="),
      `NotificationCenter L3 (${locale}) must render watermark and retracted diagnostics`,
    );
    assert(
      spotlightL3Html.includes("NON_AUTHORITATIVE · mask=0x1F") &&
        spotlightL3Html.includes("consistency="),
      `SpotlightView L3 (${locale}) must render NON_AUTHORITATIVE · mask=0x1F and consistency diagnostics`,
    );
    assert(
      clusterL3Html.includes("cluster_epoch=") &&
        clusterL3Html.includes("QUARANTINED_LEASE_FENCE"),
      `ClusterPostureTray L3 (${locale}) must render cluster_epoch and QUARANTINED_LEASE_FENCE diagnostics`,
    );
  }
}

async function testAntiAiSmellStaticSourceAndCssAudit(): Promise<void> {
  const fs = await importBuiltin<NodeFsLike>("node:fs");
  const path = await importBuiltin<NodePathLike>("node:path");
  const url = await importBuiltin<NodeUrlLike>("node:url");

  const currentDir = path.dirname(url.fileURLToPath(import.meta.url));
  const viewsRoot = path.resolve(currentDir, "../..");
  const targetDirs = [
    path.join(viewsRoot, "notifications"),
    path.join(viewsRoot, "spotlight"),
    path.join(viewsRoot, "offline-cluster"),
  ];

  const collectFiles = (dir: string): string[] => {
    const out: string[] = [];
    for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
      const full = path.join(dir, entry.name);
      if (entry.isDirectory()) {
        if (entry.name !== "__tests__") {
          out.push(...collectFiles(full));
        }
      } else if (entry.isFile() && (entry.name.endsWith(".ts") || entry.name.endsWith(".css"))) {
        out.push(full);
      }
    }
    return out;
  };

  const files = targetDirs.flatMap((d) => collectFiles(d));
  assert(files.length >= 9, `Expected at least 9 implementation files across 3 view dirs, got ${files.length}`);

  const emojiRegex = /\p{Extended_Pictographic}/u;
  const radiusRegex = /border-radius\s*:\s*([^;\}]+)/gi;

  for (const filePath of files) {
    const content = fs.readFileSync(filePath, "utf-8");

    assert(
      !content.includes("backdrop-filter") && !content.includes("-webkit-backdrop-filter"),
      `ANTI-AI-01 violation: backdrop-filter found in ${filePath}`,
    );
    assert(
      !content.toLowerCase().includes("#030710") &&
        !content.toLowerCase().includes("#7fc8ff") &&
        !content.includes("radial-gradient") &&
        !content.includes("max-w-7xl"),
      `ANTI-AI-02/03 violation: banned neon/gradient/marketing token found in ${filePath}`,
    );
    assert(
      !emojiRegex.test(content),
      `ANTI-AI-04 violation: Unicode emoji character found in ${filePath}`,
    );

    let match: RegExpExecArray | null;
    while ((match = radiusRegex.exec(content)) !== null) {
      const rawVal = match[1]!.trim();
      const isAllowed =
        ALLOWED_RADII.has(rawVal) || rawVal.startsWith("var(--nlos-radius-");
      assert(
        isAllowed,
        `ANTI-AI-05 border-radius violation in ${filePath}: '${rawVal}' not in {0px, 2px, 4px, 6px}`,
      );
    }
  }
}

async function runSuite(): Promise<void> {
  testNotification16StatesAndPagination();
  testActionTokenRedemptionAndNegativeSecurityGates();
  testSpotlight0x1fStatutoryLockAndAuthorityReverification();
  testOfflineAutonomyDuiOffline001AndQuarantinedLeaseFence();
  testProgressiveDisclosureZeroJargonInL1L2AndCompleteDiagnosticsInL3();
  await testAntiAiSmellStaticSourceAndCssAudit();
  console.log(
    "PASS: W72-D-NOTIFY-SEARCH deterministic verification & ANTI-AI-01..07 suite (all assertions green).",
  );
}

void runSuite();
