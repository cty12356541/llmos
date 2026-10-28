// ==========================================================================
// Deterministic Verification Suite for `W72-D-SETTINGS-CTRL`
// (`desktop/src/views/control-center/__tests__/control-center-settings.test.ts`)
// ==========================================================================

import {
  ALL_CONTROL_CENTER_DOMAINS,
  ALL_OWNERSHIP_LAYERS,
  createDestructiveBlastRadiusSession,
  createSeededControlCenterState,
  createSystemControlCenterController,
  renderBlastRadiusModalHtml,
  renderControlCenterWorkstationHtml,
  renderSystemControlCenterFlyoutHtml,
  windowOwnershipTreeRows,
} from "../index";
import {
  ALL_SETTINGS_LAYERS,
  createWorkstationSettingsSuiteController,
  renderKeychainVaultViewHtml,
} from "../../settings/index";
import type {
  DisclosureLevel,
  WorkstationLocale,
} from "../../../ui/status-indicator";

interface NodeTestContext {
  readonly name: string;
}
type TestFn = (
  name: string,
  fn: (t: NodeTestContext) => void | Promise<void>,
) => void;

interface NodeAssert {
  strictEqual<T>(actual: T, expected: T, message?: string): void;
  deepStrictEqual<T>(actual: T, expected: T, message?: string): void;
  ok(value: unknown, message?: string): asserts value;
}

interface NodeFs {
  readFileSync(path: string, encoding: "utf8"): string;
  readdirSync(
    path: string,
    options: { readonly withFileTypes: true },
  ): readonly {
    readonly name: string;
    isDirectory(): boolean;
    isFile(): boolean;
  }[];
}

interface NodePath {
  resolve(...segments: string[]): string;
  join(...segments: string[]): string;
}

interface NodeUrl {
  fileURLToPath(url: string): string;
}

const importBuiltin = new Function(
  "specifier",
  "return import(specifier);",
) as <T>(specifier: string) => Promise<T>;

function stripHtmlTagsToVisibleText(html: string): string {
  return html
    .replace(/<[^>]+>/g, " ")
    .replace(/\s+/g, " ")
    .trim();
}

const FORBIDDEN_VISIBLE_JARGON_L0_TO_L2: readonly string[] = [
  "Z1",
  "Z2",
  "Z3",
  "Z4",
  "TypedIntent",
  "0x1F",
  "0x1f",
  "Quota",
  "quota",
  "ROAD-D-001",
  "ADR-0011",
  "ADR-0025",
  "TaskAttempt",
  "ExecutionFiber",
  "receipt_hex",
  "control_command_id",
  "CAS_WINNER",
  "SHARED_THREAD_LIKE",
  "LOCAL_SINGLE_NODE",
  "ApplicationId",
  "InstallationId",
  "CapabilityRights",
  "ControlScope",
  "plan_digest",
  "LOSSLESS_UNDO",
  "COMPENSATABLE_ROLLBACK",
  "IRREVERSIBLE_NO_UNDO",
  "RCPT-UNDO-001",
  "EFFECT_UNKNOWN",
  "PartitionAutonomyGate",
];

void (async () => {
  const { test } = await importBuiltin<{ readonly test: TestFn }>("node:test");
  const assert: NodeAssert = await importBuiltin<NodeAssert>(
    "node:assert/strict",
  );
  const fs = await importBuiltin<NodeFs>("node:fs");
  const path = await importBuiltin<NodePath>("node:path");
  const url = await importBuiltin<NodeUrl>("node:url");

  const currentFile = url.fileURLToPath(import.meta.url);
  const viewsRoot = path.resolve(currentFile, "../../..");

  test("12-Domain SystemControlCenter & System Informer 10-layer ownership tree with SHARED_THREAD_LIKE badges", () => {
    const ctrl = createSystemControlCenterController();
    const enriched = ctrl.getEnrichedTree({
      locale: "zh-CN",
      disclosure: "L1",
    });

    // 1. All 12 domains are covered.
    assert.strictEqual(ALL_CONTROL_CENTER_DOMAINS.length, 12);
    for (const domain of ALL_CONTROL_CENTER_DOMAINS) {
      assert.ok(
        enriched.some((n) => n.domain === domain),
        `expected at least one node in domain ${domain}`,
      );
    }

    // 2. All 10 System Informer ownership layers (depths 0..9) are covered.
    assert.strictEqual(ALL_OWNERSHIP_LAYERS.length, 10);
    ALL_OWNERSHIP_LAYERS.forEach((layer, idx) => {
      const node = enriched.find((n) => n.layer === layer);
      assert.ok(node, `expected layer ${layer} in ownership tree`);
      assert.strictEqual(node.depth, idx);
    });

    // 3. Verify SHARED_THREAD_LIKE fate-sharing badge propagation across PID 4820 ([TASK-AGENT-001], [TASK-ISO-005]).
    for (const sharedId of [
      "node-proc-worker-4820",
      "node-agent-scanner-a",
      "node-agent-scanner-b",
      "node-fiber-ast-parser-01",
    ]) {
      const node = enriched.find((n) => n.nodeId === sharedId);
      assert.ok(node, `expected shared node ${sharedId}`);
      assert.strictEqual(node.faultDomainPosture, "SHARED_THREAD_LIKE");
      assert.ok(node.sharedFatePeerIds.length >= 2);
    }

    // 4. Verify cursor windowing & collapsed ancestor filtering.
    const collapsed = new Set<string>(["node-proc-worker-4820"]);
    const windowed = windowOwnershipTreeRows(enriched, {
      collapsedNodeIds: collapsed,
      limit: 50,
    });
    assert.ok(
      !windowed.visibleRows.some((n) => n.nodeId === "node-agent-scanner-a"),
      "children of collapsed process node must be hidden",
    );
    assert.ok(
      !windowed.visibleRows.some((n) => n.nodeId === "node-fiber-ast-parser-01"),
      "grandchildren of collapsed process node must be hidden",
    );
  });

  test("Headlamp capability/state action pruning & 5+1 statutory non-tamperable locks ([UI-PARITY-001])", () => {
    const ctrl = createSystemControlCenterController();
    const enriched = ctrl.getEnrichedTree({
      locale: "en-US",
      disclosure: "L3",
    });

    const byId = new Map(enriched.map((n) => [n.nodeId, n]));

    // Lock 1: 0x1F search candidate is strictly read-only ([CONS-CTRL-001]).
    const searchNode = byId.get("node-search-0x1f-candidate");
    assert.ok(searchNode);
    assert.strictEqual(
      searchNode.prunerResult.statutoryLock,
      "SEARCH_0X1F_READ_ONLY",
    );
    assert.deepStrictEqual(searchNode.prunerResult.allowedVerbs, [
      "JumpToAuthoritativeObject",
    ]);

    // Lock 2: IRREVERSIBLE receipt forbids Undo ([RCPT-UNDO-001]).
    const irrevTask = byId.get("node-task-external-publish");
    assert.ok(irrevTask);
    assert.strictEqual(
      irrevTask.prunerResult.statutoryLock,
      "IRREVERSIBLE_NO_UNDO",
    );
    assert.ok(!irrevTask.prunerResult.allowedVerbs.includes("Undo"));
    assert.ok(irrevTask.prunerResult.allowedVerbs.includes("ViewEffectProof"));

    // Lock 3: COMPLETED or ACTIVE TaskNode forbids structural edits ([TASK-GRAPH-002]).
    const activeTaskNode = byId.get("node-tasknode-ast-scan");
    assert.ok(activeTaskNode);
    assert.strictEqual(
      activeTaskNode.prunerResult.statutoryLock,
      "NODE_COMPLETED_OR_ACTIVE_LOCKED",
    );
    assert.deepStrictEqual(activeTaskNode.prunerResult.allowedVerbs, []);

    // Lock 4: TaskAttempt forbids manual commit ([TASK-COMMIT-001]).
    const attemptNode = byId.get("node-attempt-ast-01");
    assert.ok(attemptNode);
    assert.strictEqual(
      attemptNode.prunerResult.statutoryLock,
      "TASK_ATTEMPT_NO_MANUAL_COMMIT",
    );
    assert.ok(!attemptNode.prunerResult.allowedVerbs.includes("Commit"));
    assert.deepStrictEqual(attemptNode.prunerResult.allowedVerbs, [
      "Cancel",
      "Supersede",
      "RequestReverify",
    ]);

    // Lock 5: Operation(EFFECT_UNKNOWN) forbids manual mark success ([TASK-EFFECT-003]).
    const unknownOp = byId.get("node-op-ext-webhook");
    assert.ok(unknownOp);
    assert.strictEqual(
      unknownOp.prunerResult.statutoryLock,
      "OPERATION_EFFECT_UNKNOWN_NO_MANUAL_SUCCESS",
    );
    assert.ok(!unknownOp.prunerResult.allowedVerbs.includes("MarkSuccess"));
    assert.deepStrictEqual(unknownOp.prunerResult.allowedVerbs, [
      "Reconcile",
      "Compensate",
    ]);

    // Lock 6 (+1): Quarantined Cell lease forbids local TTL unlock ([LEASE-TTL-001]).
    const quarantinedCell = byId.get("node-cell-quarantined-edge");
    assert.ok(quarantinedCell);
    assert.strictEqual(
      quarantinedCell.prunerResult.statutoryLock,
      "QUARANTINED_LEASE_FENCE",
    );
    assert.ok(
      !quarantinedCell.prunerResult.allowedVerbs.includes("UnlockByLocalTtl"),
    );
    assert.deepStrictEqual(quarantinedCell.prunerResult.allowedVerbs, [
      "InspectFenceStatus",
    ]);
  });

  test("Stage C Cell 3-family lease inspection & controlled RequestProcessMigration ([SCHED-STEAL-001])", () => {
    const ctrl = createSystemControlCenterController();
    const initialCells = ctrl.getState().cells;
    const primary = initialCells.find((c) => c.cellId === "cell-local-primary");
    assert.ok(primary);
    assert.strictEqual(primary.leases.length, 3);

    // 1. Stale epoch is rejected with CAS_EPOCH_CONFLICT.
    const staleRes = ctrl.requestProcessMigration({
      processNodeId: "node-proc-worker-4820",
      sourceCellId: "cell-local-primary",
      targetCellId: "cell-secondary-compute",
      expectedEpoch: 999,
      readBackingToken: "rbt-001",
    });
    assert.strictEqual(staleRes.ok, false);
    assert.strictEqual(staleRes.errorCode, "CAS_EPOCH_CONFLICT");

    // 2. Migrating into a Quarantined cell is rejected.
    const quarantinedTargetRes = ctrl.requestProcessMigration({
      processNodeId: "node-proc-worker-4820",
      sourceCellId: "cell-local-primary",
      targetCellId: "cell-quarantined-edge",
      expectedEpoch: 42,
      readBackingToken: "rbt-002",
    });
    assert.strictEqual(quarantinedTargetRes.ok, false);
    assert.strictEqual(
      quarantinedTargetRes.errorCode,
      "MIGRATION_TARGET_UNAVAILABLE",
    );

    // 3. Migrating out of a Quarantined cell without closure + fence receipts is rejected.
    const quarantinedSourceRes = ctrl.requestProcessMigration({
      processNodeId: "node-cell-quarantined-edge",
      sourceCellId: "cell-quarantined-edge",
      targetCellId: "cell-secondary-compute",
      expectedEpoch: 41,
      readBackingToken: "rbt-003",
    });
    assert.strictEqual(quarantinedSourceRes.ok, false);
    assert.strictEqual(
      quarantinedSourceRes.errorCode,
      "QUARANTINED_LEASE_FENCE",
    );

    // 4. Valid controlled migration succeeds and transfers bound lease.
    const okRes = ctrl.requestProcessMigration({
      processNodeId: "node-proc-worker-4820",
      sourceCellId: "cell-local-primary",
      targetCellId: "cell-secondary-compute",
      expectedEpoch: 42,
      readBackingToken: "rbt-004",
    });
    assert.strictEqual(okRes.ok, true);
    assert.strictEqual(okRes.newEpoch, 43);
    assert.deepStrictEqual(okRes.migratedLeaseIds, ["lease-quota-exec-001"]);
  });

  test("Terraform +/~/ - blast-radius diff, 5s two-step arming, Topic Purge Data-Loss Receipt & ControlCommandId recovery", async () => {
    let clockMs = 1_000;
    const session = createDestructiveBlastRadiusSession({
      targetNodeId: "node-topic-telemetry",
      targetLabels: {
        "zh-CN": "编译诊断事件通道",
        "en-US": "Build Diagnostic Event Topic",
      },
      verb: "TopicPurge",
      expectedRevision: 10,
      nowMs: () => clockMs,
    });

    assert.strictEqual(session.requiresDataLossAck, true);
    assert.strictEqual(session.rows.length, 3);

    // Step 1: First press arms the 5-second countdown.
    const step1 = session.triggerArmOrConfirm();
    assert.strictEqual(step1.outcome, "armed");
    assert.strictEqual(step1.snapshot.state, "armed");
    assert.strictEqual(step1.snapshot.remainingMs, 5000);

    // Step 2a: Confirm without dataLossAcknowledged is blocked.
    clockMs = 1_500;
    const noAck = session.triggerArmOrConfirm();
    assert.strictEqual(noAck.outcome, "data-loss-ack-required");

    // Step 2b: Acknowledge data loss, but test clickjack guard on a fresh arm.
    session.armingController.reset();
    session.setDataLossAcknowledged(true);
    clockMs = 2_000;
    session.triggerArmOrConfirm(); // armed at 2000ms
    clockMs = 2_100; // only 100ms elapsed (< 350ms guard)
    const guarded = session.triggerArmOrConfirm();
    assert.strictEqual(guarded.outcome, "guard-blocked");

    // Step 2c: Advance past 350ms guard and simulate transport UNKNOWN outcome ([CTRL-RECOVERY-001]).
    clockMs = 2_600;
    const unknownRes = session.triggerArmOrConfirm({
      commandIdHex: "cmd-topic-purge-001",
      simulateTransportUnknown: true,
    });
    assert.strictEqual(unknownRes.outcome, "unknown-needs-reconcile");
    assert.ok(unknownRes.dataLossReceipt);
    assert.strictEqual(unknownRes.dataLossReceipt.dataLossAcknowledged, true);
    assert.strictEqual(unknownRes.dataLossReceipt.purgedMessagesCount, 1280);

    // Step 3: Blind retry while in UNKNOWN_NEEDS_RECONCILE is strictly blocked!
    clockMs = 3_000;
    const blindRetry = session.triggerArmOrConfirm({
      commandIdHex: "cmd-topic-purge-001",
    });
    assert.strictEqual(blindRetry.outcome, "blind-retry-blocked");

    // Step 4: Reconcile status via query_control_command_status before proceeding.
    const reconciled = await session.reconcileUnknownOutcome("succeeded");
    assert.ok(reconciled);
    assert.strictEqual(reconciled.state, "SUCCEEDED");
    assert.strictEqual(reconciled.resolution, "SUCCEEDED");
  });

  test("4-layer SettingsDomain CAS + watch sync, ContentTypeAssociation anti-hijack, and zero-secret KeychainReference vault", () => {
    const suite = createWorkstationSettingsSuiteController();

    // 1. 4-layer Settings CAS + watch sync ([DUI-SETTINGS-001]).
    assert.strictEqual(ALL_SETTINGS_LAYERS.length, 4);
    const watchedEvents: string[] = [];
    const unsub = suite.settingsCas.watchSettingsDomain((ev) => {
      watchedEvents.push(`${ev.layer}:${ev.key}=${ev.effectiveTheme}`);
    });

    const casOk = suite.settingsCas.updateSettingWithCas({
      layer: "user",
      key: "appearance.theme",
      valueJson: "\"paper-workstation\"",
      expectedDomainRevision: 20,
    });
    assert.strictEqual(casOk.ok, true);
    assert.strictEqual(casOk.actualRevision, 21);
    assert.strictEqual(suite.settingsCas.getActiveTheme(), "paper-workstation");
    assert.deepStrictEqual(watchedEvents, [
      "user:appearance.theme=paper-workstation",
    ]);

    // Stale expectedDomainRevision (20) must fail with CAS_CONFLICT.
    const casConflict = suite.settingsCas.updateSettingWithCas({
      layer: "user",
      key: "appearance.theme",
      valueJson: "\"graphite-workstation\"",
      expectedDomainRevision: 20,
    });
    assert.strictEqual(casConflict.ok, false);
    assert.strictEqual(casConflict.errorCode, "CAS_CONFLICT");
    assert.strictEqual(casConflict.actualRevision, 21);
    unsub();

    // 2. ContentTypeAssociation anti-hijack & one-click revoke ([DUI-ASSOC-001]).
    const proposalRes = suite.associations.submitPackageProposal(
      "text/markdown",
      {
        proposedApplicationId: "app-untrusted-md",
        proposedInstallationId: "inst-untrusted-md-1",
        proposedLabels: {
          "zh-CN": "未授权 Markdown 工具",
          "en-US": "Untrusted Markdown Tool",
        },
      },
    );
    assert.strictEqual(proposalRes.ok, true);
    assert.strictEqual(
      proposalRes.item?.handlerApplicationId,
      "app-spec-studio-01",
      "package proposal must NEVER overwrite active handlerApplicationId",
    );

    const revokeAssoc = suite.associations.revokeAssociationToSystemChooser(
      "text/markdown",
      2,
    );
    assert.strictEqual(revokeAssoc.ok, true);
    assert.strictEqual(revokeAssoc.item?.handlerApplicationId, null);

    // 3. Zero-Secret KeychainReference vault & session-lock handle wipe ([DUI-SETTINGS-001], [DUI-SESSION-001]).
    const secretMarker = "ULTRA_SECRET_FRONTEND_DO_NOT_LEAK_777";
    const enrollOk = suite.keychainVault.enrollSecretByReference({
      keychainRefId: "kref-test-zero-leak",
      ownerPrincipalId: "principal-workstation-operator",
      labels: {
        "zh-CN": "测试零泄露密钥引用",
        "en-US": "Test Zero-Leak Key Reference",
      },
      credentialKindLabel: {
        "zh-CN": "硬件安全芯片密钥",
        "en-US": "Hardware Enclave Key",
      },
      rawSecretMaterial: secretMarker,
      authorizedApplicationIds: ["node-app-code-analyzer"],
      hardwareBacked: true,
      highAssuranceConfirmToken: "confirm-token-ok",
    });
    assert.strictEqual(enrollOk.ok, true);

    const jsonDump = JSON.stringify(suite.keychainVault.listReferences());
    const htmlDumpL3 = renderKeychainVaultViewHtml(suite.keychainVault, {
      locale: "en-US",
      disclosure: "L3",
    });
    assert.ok(
      !jsonDump.includes(secretMarker) && !jsonDump.includes("TOP_SECRET_"),
      "JSON projection of KeychainReference must NEVER contain raw secret material",
    );
    assert.ok(
      !htmlDumpL3.includes(secretMarker) && !htmlDumpL3.includes("TOP_SECRET_"),
      "Rendered HTML of KeychainReference must NEVER contain raw secret material",
    );

    const hdlRes = suite.keychainVault.acquireUseByReferenceHandle(
      "kref-test-zero-leak",
      "node-app-code-analyzer",
    );
    assert.strictEqual(hdlRes.ok, true);

    const lockRes = suite.keychainVault.lockSessionAndWipeHandles();
    assert.strictEqual(lockRes.wipedHandleCount, 1);
    assert.strictEqual(lockRes.sessionGeneration, 101);

    const lockedAcquire = suite.keychainVault.acquireUseByReferenceHandle(
      "kref-test-zero-leak",
      "node-app-code-analyzer",
    );
    assert.strictEqual(lockedAcquire.ok, false);
    assert.strictEqual(lockedAcquire.errorCode, "SESSION_LOCKED");
  });

  test("Zero internal architecture jargon in L0/L1/L2 visible text (zh-CN & en-US) and full diagnostic parity in L3", () => {
    const state = createSeededControlCenterState();
    const suite = createWorkstationSettingsSuiteController();
    const session = createDestructiveBlastRadiusSession({
      targetNodeId: "node-topic-telemetry",
      targetLabels: {
        "zh-CN": "编译诊断事件通道",
        "en-US": "Build Diagnostic Event Topic",
      },
      verb: "TopicPurge",
      expectedRevision: 1,
    });
    session.setDataLossAcknowledged(true);
    session.triggerArmOrConfirm();

    const locales: readonly WorkstationLocale[] = ["zh-CN", "en-US"];
    const nonDiagnosticLevels: readonly DisclosureLevel[] = ["L0", "L1", "L2"];

    for (const locale of locales) {
      for (const disclosure of nonDiagnosticLevels) {
        const combinedHtml = [
          renderSystemControlCenterFlyoutHtml(state, { locale, disclosure }),
          renderControlCenterWorkstationHtml(state, { locale, disclosure }),
          renderBlastRadiusModalHtml(session, { locale, disclosure }),
          suite.renderSuiteHtml({ locale, disclosure }),
        ].join("\n");

        const visibleText = stripHtmlTagsToVisibleText(combinedHtml);
        for (const forbidden of FORBIDDEN_VISIBLE_JARGON_L0_TO_L2) {
          assert.ok(
            !visibleText.includes(forbidden),
            `Forbidden internal jargon "${forbidden}" leaked into ${disclosure} (${locale}) visible text`,
          );
        }
      }

      // Verify L3 explicitly exposes raw diagnostic tags!
      const l3WorkstationText = stripHtmlTagsToVisibleText(
        renderControlCenterWorkstationHtml(state, {
          locale,
          disclosure: "L3",
        }),
      );
      assert.ok(l3WorkstationText.includes("SHARED_THREAD_LIKE"));
      assert.ok(l3WorkstationText.includes("PartitionAutonomyGate"));
      assert.ok(l3WorkstationText.includes("TASK-COMMIT-001"));
    }
  });

  test("Static & rendered ANTI-AI-01..07 compliance across desktop/src/views/{control-center,settings}", () => {
    const targetDirs = [
      path.join(viewsRoot, "control-center"),
      path.join(viewsRoot, "settings"),
    ];

    const collectFiles = (dir: string): string[] => {
      const out: string[] = [];
      for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
        const full = path.join(dir, entry.name);
        if (entry.isDirectory()) {
          if (entry.name !== "__tests__") {
            out.push(...collectFiles(full));
          }
        } else if (
          entry.isFile() &&
          (entry.name.endsWith(".ts") || entry.name.endsWith(".css"))
        ) {
          out.push(full);
        }
      }
      return out;
    };

    const allFiles = targetDirs.flatMap(collectFiles);
    assert.ok(allFiles.length >= 10, "expected at least 10 implementation files");

    const allowedRadii = new Set(["0", "0px", "2px", "4px", "6px"]);
    const emojiRegex = /\p{Extended_Pictographic}/u;

    for (const filePath of allFiles) {
      const content = fs.readFileSync(filePath, "utf8");
      assert.ok(
        !content.includes("backdrop-filter"),
        `[ANTI-AI-01] forbidden backdrop-filter in ${filePath}`,
      );
      assert.ok(
        !content.toLowerCase().includes("#030710") &&
          !content.toLowerCase().includes("#7fc8ff") &&
          !content.includes("radial-gradient") &&
          !content.includes("max-w-7xl"),
        `[ANTI-AI-02/03] forbidden AI template token in ${filePath}`,
      );
      assert.ok(
        !emojiRegex.test(content),
        `[ANTI-AI-06] forbidden Extended_Pictographic emoji in ${filePath}`,
      );

      const radiusMatches = content.matchAll(/border-radius\s*:\s*([^;}\n]+)/g);
      for (const m of radiusMatches) {
        const val = m[1].trim();
        assert.ok(
          allowedRadii.has(val),
          `[ANTI-AI-04] border-radius "${val}" in ${filePath} is outside {0px, 2px, 4px, 6px}`,
        );
      }
    }
  });
})();
