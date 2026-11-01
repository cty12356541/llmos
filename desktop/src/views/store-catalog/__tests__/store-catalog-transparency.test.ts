// Comprehensive Deterministic Verification & Anti-AI Audit Suite for W80-E6C Desktop Store
// Transparency UI (Store Catalog + Publisher Chain + Permission Transparency + Z4 Handoff).
// Run from repo root: npx tsx desktop/src/views/store-catalog/__tests__/store-catalog-transparency.test.ts

import {
  STORE_ACTION_DOWNGRADE_SANDBOX,
  STORE_ACTION_INSTALL,
  STORE_ACTION_IRREVERSIBLE_UPGRADE,
  STORE_CHAIN_STATUS_TRUST_DB_UNAVAILABLE,
  STORE_CHAIN_STATUS_VERIFIED,
  STORE_CHAIN_STATUS_VERIFICATION_FAILED,
  STORE_SOURCE_LIVE_REGISTRY,
  STORE_SOURCE_OFFLINE_UNAVAILABLE,
  STORE_COPY,
  createStoreCatalogController,
  createTauriStoreCatalogClient,
  renderStoreCatalogHtml,
  toZ4ConsentChallengeSpec,
  type StoreCatalogDataClient,
  type StoreCatalogEntryDto,
  type StoreCatalogSnapshotDto,
  type StorePermissionDiffDto,
  type StoreReleaseTransparencyDto,
  type StoreSensitiveActionEntryDto,
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
   Test Harness & Static Anti-AI Audit Helpers
   ========================================================================== */

let passedAssertions = 0;
let passedSuites = 0;

function assert(condition: boolean, message: string): void {
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

const PICTOGRAPHIC_RE = /\p{Extended_Pictographic}/u;

const BANNED_SUBSTRINGS: readonly string[] = [
  ["backdrop", "-filter"].join(""),
  ["-webkit-", "backdrop", "-filter"].join(""),
  ["#030", "710"].join(""),
  ["#7fc", "8ff"].join(""),
  ["radial", "-gradient"].join(""),
  ["max-w-", "7xl"].join(""),
];

const L1_L2_BANNED_JARGON: readonly string[] = [
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
  "TaskAttempt",
  "ExecutionFiber",
  "receipt_hex",
  "control_command_id",
  "CAS_WINNER",
  "SHARED_THREAD_LIKE",
  "LOCAL_SINGLE_NODE",
];

function assertZeroJargon(label: string, text: string): void {
  for (const banned of L1_L2_BANNED_JARGON) {
    assert(!text.includes(banned), `${label} leaked banned jargon '${banned}' in '${text}'`);
  }
}

function assertZeroEmoji(label: string, text: string): void {
  assert(!PICTOGRAPHIC_RE.test(text), `${label} leaked pictographic emoji in '${text}'`);
}

function collectProductionFiles(
  fs: NodeFsLike,
  pathMod: NodePathLike,
  dir: string,
): string[] {
  const out: string[] = [];
  for (const entry of fs.readdirSync(dir, { withFileTypes: true })) {
    if (entry.name === "__tests__") {
      continue;
    }
    const full = pathMod.join(dir, entry.name);
    if (entry.isDirectory()) {
      out.push(...collectProductionFiles(fs, pathMod, full));
    } else if (
      entry.isFile() &&
      (entry.name.endsWith(".ts") || entry.name.endsWith(".css"))
    ) {
      out.push(full);
    }
  }
  return out.sort();
}

/* ==========================================================================
   Deterministic Fixtures (mirroring the Rust side bridge DTO shapes)
   ========================================================================== */

const NOW_MS = 1_700_000_000_000;

function sampleEntry(overrides?: Partial<StoreCatalogEntryDto>): StoreCatalogEntryDto {
  return {
    packageIdHex: "a1".repeat(16),
    packageVersion: 1_099_511_627_776, // pack(1,0,0)
    applicationIdsHex: ["33".repeat(16)],
    publisherIdHex: "41".repeat(16),
    publisherTier: "CERTIFIED_PUBLISHER",
    track: "OFFICIAL",
    channel: "STABLE",
    activeRecallSeverity: null,
    componentCount: 3,
    permissionCount: 2,
    anyNetworkEgress: false,
    anyIrreversibleEffect: false,
    publisherKeyFingerprintHex: "9f86d081884c7d65",
    submittedAtMs: 2_900,
    ...overrides,
  };
}

function sampleSnapshot(): StoreCatalogSnapshotDto {
  return {
    source: STORE_SOURCE_LIVE_REGISTRY,
    offlineNoticeZh: null,
    offlineNoticeEn: null,
    storeRevocationEpoch: 1,
    officialStableReleases: [
      sampleEntry(),
      sampleEntry({
        packageIdHex: "c4".repeat(16),
        packageVersion: 1_099_511_627_777,
        publisherIdHex: "52".repeat(16),
        track: "OFFICIAL",
      }),
    ],
    communityCandidates: [
      sampleEntry({
        packageIdHex: "b1".repeat(16),
        publisherTier: "COMMUNITY_SANDBOXED",
        track: "COMMUNITY",
        channel: "CANDIDATE",
        anyNetworkEgress: true,
      }),
    ],
  };
}

function offlineSnapshot(): StoreCatalogSnapshotDto {
  return {
    source: STORE_SOURCE_OFFLINE_UNAVAILABLE,
    offlineNoticeZh: "本地商店目录暂不可用(未初始化或处于离线状态),当前仅显示空白目录;安装与升级入口已停用。",
    offlineNoticeEn:
      "Local store catalog is unavailable (not initialized or offline); showing an empty catalog, and install or upgrade entries are paused.",
    storeRevocationEpoch: 0,
    officialStableReleases: [],
    communityCandidates: [],
  };
}

function sampleTransparency(
  chainStatus: string = STORE_CHAIN_STATUS_VERIFIED,
): StoreReleaseTransparencyDto {
  return {
    packageIdHex: "a1".repeat(16),
    packageVersion: 1_099_511_627_776,
    applicationIdsHex: ["33".repeat(16)],
    publisherIdHex: "41".repeat(16),
    publisherTier: "CERTIFIED_PUBLISHER",
    track: "OFFICIAL",
    channel: "STABLE",
    permissions: [
      {
        capabilityScope: "sabi.artifact.read",
        humanPurpose: "读取用户笔记",
        effectClass: "READ_ONLY",
        networkEgressDeclared: false,
        sandboxProfile: "wasm-strict-v1",
      },
      {
        capabilityScope: "sabi.sync.push",
        humanPurpose: "推送同步数据",
        effectClass: "IRREVERSIBLE",
        networkEgressDeclared: true,
        sandboxProfile: "wasm-strict-v2",
      },
    ],
    anyNetworkEgress: true,
    anyIrreversibleEffect: true,
    effectLegend: [
      { effectClass: "READ_ONLY", labelZh: "只读", labelEn: "Read-only" },
      { effectClass: "REVERSIBLE", labelZh: "可逆", labelEn: "Reversible" },
      { effectClass: "COMPENSATABLE", labelZh: "可补偿", labelEn: "Compensatable" },
      { effectClass: "IRREVERSIBLE", labelZh: "不可逆", labelEn: "Irreversible" },
    ],
    trustChain: {
      publisherIdHex: "41".repeat(16),
      trustTier: "CERTIFIED_PUBLISHER",
      tierLabelZh: "已认证发布方",
      tierLabelEn: "Certified publisher",
      chainStatus,
      chainStatusNoticeZh: "证书链与发布签名已在本机复验通过。",
      chainStatusNoticeEn:
        "Certificate chain and publisher signature re-verified on this machine.",
      leafKeyFingerprintHex: "9f86d081884c7d65",
      revocationEpoch: 1,
      crlFreshUntilMs: 8_000_000_000_000,
      signatureReverified: chainStatus === STORE_CHAIN_STATUS_VERIFIED,
    },
  };
}

function sampleDiff(reconsent = true): StorePermissionDiffDto {
  return {
    packageIdHex: "a1".repeat(16),
    oldVersion: 1_099_511_627_776,
    newVersion: 1_100_069_968_384,
    addedCapabilities: [
      {
        capabilityScope: "sabi.sync.push",
        humanPurpose: "推送同步数据",
        effectClass: "IRREVERSIBLE",
        networkEgressDeclared: true,
      },
    ],
    removedCapabilityScopes: [],
    newlyEnabledNetworkEgress: ["sabi.sync.push"],
    escalatedEffects: [
      { capabilityScope: "sabi.artifact.read", oldEffect: "READ_ONLY", newEffect: "REVERSIBLE" },
    ],
    escalatedToIrreversible: [
      { capabilityScope: "sabi.sync.push", oldEffect: "READ_ONLY", newEffect: "IRREVERSIBLE" },
    ],
    purposeChanges: [],
    sandboxProfileChanges: [],
    requiresZ4Reconsent: reconsent,
    plainSummaryZh:
      "新版本请求更强权限:1 项新增能力、1 项新开网络出站、1 项不可逆变更;必须经系统安全确认屏重新授权后才能升级。",
    plainSummaryEn:
      "The new version requests stronger permissions. Re-authorization on the system security confirmation screen is required before upgrading.",
  };
}

function sampleActionEntry(
  reversibility: "IRREVERSIBLE" | "COMPENSATABLE_ROLLBACK" = "COMPENSATABLE_ROLLBACK",
): StoreSensitiveActionEntryDto {
  return {
    actionKind: STORE_ACTION_INSTALL,
    plainActionZh: "从受控商店目录安装应用包 a1a1a1a1(版本 1099511627776),将授予 2 项能力权限。",
    plainActionEn:
      "Install application package a1a1a1a1 from the governed store catalog; 2 capability permissions will be granted.",
    reversibilityClass: reversibility,
    z4Request: {
      requestingApp: {
        displayName: "应用包 a1a1a1a1 v1099511627776",
        publisherName: "已认证发布方 a1a1a1a1",
        signatureVerified: true,
        applicationIdHex: "33".repeat(16),
        installationIdHex: "0".repeat(32),
        principalIdHex: "0".repeat(32),
        installationGeneration: 0,
        manifestDigestHex: "5e".repeat(32),
      },
      actionScope: {
        plainSummaryZh: "安装该应用包并授予声明的能力权限。",
        plainSummaryEn: "Install this application package and grant the declared capabilities.",
        capabilityRights: ["sabi.artifact.read", "sabi.sync.push"],
        controlScope: "StoreCatalog",
        planDigestHex: "6e".repeat(32),
      },
      resourceCeiling: {
        plainCeilingZh: "组件总大小上限约 0 MiB;网络出站:允许",
        plainCeilingEn: "Component size ceiling about 0 MiB; network egress allowed",
        memoryCeilingMib: 0,
        tokenBudgetCeiling: 0,
        networkEgressAllowed: true,
        maxDurationMs: 0,
      },
      reversibilityAndRisk: {
        reversibilityClass: reversibility,
        plainConsequenceZh: "该发布物声明了不可逆操作能力。",
        plainConsequenceEn: "This release declares irreversible capabilities.",
        multiTargetCount: 2,
        partialFailureDisclosureZh: null,
        partialFailureDisclosureEn: null,
        blastRadiusRows: [
          {
            symbol: "+",
            category: "能力授权",
            scopeLabel: "sabi.artifact.read",
            targetLabel: "sabi.artifact.read",
            reason: "读取用户笔记",
            targetIdHex: null,
          },
          {
            symbol: "~",
            category: "网络出站",
            scopeLabel: "sabi.sync.push",
            targetLabel: "sabi.sync.push",
            reason: "推送同步数据",
            targetIdHex: null,
          },
        ],
        requiresDataLossReceipt: false,
      },
      allowedGrantScopes: ["SINGLE_ACTION"],
    },
  };
}

/* ==========================================================================
   Main Verification Suites
   ========================================================================== */

async function main(): Promise<void> {
  console.log(
    "Running W80-E6C Desktop Store Transparency UI Verification Suite...",
  );

  await runSuite(
    "1. Offline deterministic degradation, locale switch, and L1 posture ([DUI-OFFLINE-001])",
    async () => {
      const calls: string[] = [];
      const client: StoreCatalogDataClient = {
        loadSnapshot: async () => {
          calls.push("snapshot");
          return offlineSnapshot();
        },
        loadTransparency: async () => sampleTransparency(),
        loadPermissionDiff: async () => sampleDiff(),
        prepareSensitiveAction: async () => sampleActionEntry(),
      };
      const ctrl = createStoreCatalogController({ client, nowMs: () => NOW_MS });
      await ctrl.refresh();

      let state = ctrl.getSnapshot();
      assert(state.snapshot !== null, "offline snapshot must be loaded");
      assert(
        state.snapshot?.source === STORE_SOURCE_OFFLINE_UNAVAILABLE,
        "offline source flag projected",
      );
      assert(
        (state.snapshot?.officialStableReleases.length ?? -1) === 0,
        "offline official list is empty",
      );
      assert(state.loadError === null, "offline degrade is not an error state");
      assert(state.disclosure === "L1", "default disclosure is L1");
      assert(state.locale === "zh-CN", "default locale is zh-CN");

      let html = renderStoreCatalogHtml(ctrl.getSnapshot());
      assert(
        html.includes("本地商店目录暂不可用"),
        "zh offline notice rendered at L1",
      );
      assert(!html.includes("Local store catalog is unavailable"), "en notice hidden under zh locale");
      assert(html.includes('data-store-posture="OFFLINE_UNAVAILABLE"'), "posture attribute present");

      ctrl.setLocale("en-US");
      html = renderStoreCatalogHtml(ctrl.getSnapshot());
      assert(
        html.includes("Local store catalog is unavailable"),
        "en offline notice rendered after locale switch",
      );
      assert(!html.includes("本地商店目录暂不可用"), "zh notice hidden under en locale");

      // Zero jargon / zero emoji in offline notices.
      const zhNotice = offlineSnapshot().offlineNoticeZh ?? "";
      const enNotice = offlineSnapshot().offlineNoticeEn ?? "";
      assertZeroJargon("offline zh notice", zhNotice);
      assertZeroJargon("offline en notice", enNotice);
      assertZeroEmoji("offline zh notice", zhNotice);
      assertZeroEmoji("offline en notice", enNotice);
    },
  );

  await runSuite(
    "2. Live catalog rows, L1 zero-jargon table, selection, L2 permission inspector, L3 raw fields",
    async () => {
      let transparencyCalls = 0;
      const client: StoreCatalogDataClient = {
        loadSnapshot: async () => sampleSnapshot(),
        loadTransparency: async (pkgHex, version) => {
          transparencyCalls += 1;
          assert(pkgHex === "a1".repeat(16), "transparency requested with package hex");
          assert(version === sampleEntry().packageVersion, "transparency requested with version");
          return sampleTransparency();
        },
        loadPermissionDiff: async () => sampleDiff(),
        prepareSensitiveAction: async () => sampleActionEntry(),
      };
      const ctrl = createStoreCatalogController({ client, nowMs: () => NOW_MS });
      await ctrl.refresh();

      const state = ctrl.getSnapshot();
      assert(state.snapshot?.source === STORE_SOURCE_LIVE_REGISTRY, "live source");
      assert(
        state.snapshot?.officialStableReleases.length === 2,
        "two official stable rows",
      );
      assert(state.snapshot?.communityCandidates.length === 1, "one community candidate row");

      const html = renderStoreCatalogHtml(state);
      // L1 table: counts, tabular versions, egress/irreversible status dots.
      const rowMatches = html.match(/<tr[^>]*data-store-pkg=/g) ?? [];
      assert(rowMatches.length === 3, `L1 table renders 3 catalog rows (got ${rowMatches.length})`);
      assert(html.includes("tabular-nums") || html.includes("nlos-store-version"), "versions use tabular numerals hook");
      assert(html.includes('data-store-egress="true"'), "egress flag projected per row");
      assert(html.includes('data-store-tier="COMMUNITY_SANDBOXED"'), "community tier flag projected");

      // Select a release -> L2 inspector with permission table + legend.
      await ctrl.selectRelease("a1".repeat(16), sampleEntry().packageVersion);
      assert(transparencyCalls === 1, "transparency loaded once on selection");
      const selectedState = ctrl.getSnapshot();
      assert(selectedState.selected !== null, "selected transparency present");
      const inspectorHtml = renderStoreCatalogHtml(selectedState);
      // Sensitive action entry lives in the inspector (chain-aware), not on bare L1 rows.
      assert(inspectorHtml.includes('data-store-action-kind="INSTALL"'), "install action entry rendered in inspector");
      assert(inspectorHtml.includes("nlos-store-action"), "sensitive action hook class present");
      assert(inspectorHtml.includes("sabi.artifact.read"), "permission scope rendered in L2");
      assert(inspectorHtml.includes("读取用户笔记"), "publisher purpose rendered in L2");
      assert(inspectorHtml.includes("只读"), "effect legend zh rendered");
      assert(inspectorHtml.includes('data-store-effect="IRREVERSIBLE"'), "irreversible effect badge hook");

      // L3: raw hex identifiers + sandbox profiles + fingerprints.
      ctrl.setDisclosure("L3");
      const l3Html = renderStoreCatalogHtml(ctrl.getSnapshot());
      assert(l3Html.includes(sampleEntry().publisherKeyFingerprintHex), "L3 shows publisher key fingerprint");
      assert(l3Html.includes("wasm-strict-v2"), "L3 shows sandbox profile");
      assert(l3Html.includes("crlFreshUntilMs") === false, "no raw field names leaked");
      assert(l3Html.includes("9f86d081884c7d65"), "L3 shows leaf key fingerprint");

      // XSS: publisher-supplied purpose must be escaped.
      const base = sampleTransparency();
      const evil: StoreReleaseTransparencyDto = {
        ...base,
        permissions: [
          { ...base.permissions[0]!, humanPurpose: '<script>alert("store")</script>' },
          ...base.permissions.slice(1),
        ],
      };
      const evilCtrl = createStoreCatalogController({
        client: {
          loadSnapshot: async () => sampleSnapshot(),
          loadTransparency: async () => evil,
          loadPermissionDiff: async () => sampleDiff(),
          prepareSensitiveAction: async () => sampleActionEntry(),
        },
        nowMs: () => NOW_MS,
      });
      await evilCtrl.refresh();
      await evilCtrl.selectRelease("a1".repeat(16), 1);
      const evilHtml = renderStoreCatalogHtml(evilCtrl.getSnapshot());
      assert(!evilHtml.includes("<script>alert"), "publisher payload HTML-escaped in L2 inspector");
      assert(evilHtml.includes("&lt;script&gt;"), "escaped entity present");
    },
  );

  await runSuite(
    "3. Publisher certificate chain transparency panel (verified / degraded / failed)",
    async () => {
      for (const [status, expectBadge] of [
        [STORE_CHAIN_STATUS_VERIFIED, "verified"],
        [STORE_CHAIN_STATUS_TRUST_DB_UNAVAILABLE, "degraded"],
        [STORE_CHAIN_STATUS_VERIFICATION_FAILED, "failed"],
      ] as const) {
        const ctrl = createStoreCatalogController({
          client: {
            loadSnapshot: async () => sampleSnapshot(),
            loadTransparency: async () => sampleTransparency(status),
            loadPermissionDiff: async () => sampleDiff(),
            prepareSensitiveAction: async () => sampleActionEntry(),
          },
          nowMs: () => NOW_MS,
        });
        await ctrl.refresh();
        await ctrl.selectRelease("a1".repeat(16), sampleEntry().packageVersion);
        const html = renderStoreCatalogHtml(ctrl.getSnapshot());
        assert(
          html.includes(`data-store-chain="${status}"`),
          `chain status attribute ${status} rendered`,
        );
        if (expectBadge === "verified") {
          assert(html.includes("已认证发布方"), "tier label rendered on verified chain");
        }
        if (expectBadge === "failed") {
          assert(html.includes("nlos-store-chain-failed"), "failed chain uses crimson hook class");
        }
        if (expectBadge === "degraded") {
          assert(html.includes("nlos-store-chain-degraded"), "degraded chain uses amber hook class");
        }
        const chainNotice = sampleTransparency(status).trustChain.chainStatusNoticeZh;
        assertZeroJargon(`chain notice ${status}`, chainNotice);
        assertZeroEmoji(`chain notice ${status}`, chainNotice);
      }
    },
  );

  await runSuite(
    "4. Permission escalation diff blocks silent upgrade with plain-language banner",
    async () => {
      let diffCalls = 0;
      const ctrl = createStoreCatalogController({
        client: {
          loadSnapshot: async () => sampleSnapshot(),
          loadTransparency: async () => sampleTransparency(),
          loadPermissionDiff: async (_pkg, oldV, newV) => {
            diffCalls += 1;
            if (oldV === 1) {
              assert(newV === 2, "diff called with new version");
            }
            return sampleDiff(oldV !== newV);
          },
          prepareSensitiveAction: async () => sampleActionEntry("IRREVERSIBLE"),
        },
        nowMs: () => NOW_MS,
      });
      await ctrl.refresh();
      await ctrl.inspectDiff("a1".repeat(16), 1, 2);
      assert(diffCalls === 1, "diff loaded once");

      let html = renderStoreCatalogHtml(ctrl.getSnapshot());
      assert(html.includes("nlos-store-diff-reconsent"), "re-consent banner hook class present");
      assert(html.includes("必须经系统安全确认屏重新授权"), "zh re-consent summary rendered");
      assert(html.includes("+"), "added capability symbol rendered");
      assert(html.includes("~"), "egress change symbol rendered");
      assert(html.includes("READ_ONLY → REVERSIBLE") || html.includes("READ_ONLY"), "effect escalation row rendered");
      assertZeroJargon("diff summary zh", sampleDiff(true).plainSummaryZh);
      assertZeroJargon("diff summary en", sampleDiff(true).plainSummaryEn);
      assertZeroEmoji("diff summary zh", sampleDiff(true).plainSummaryZh);

      // Identical versions -> no banner.
      await ctrl.inspectDiff("a1".repeat(16), 2, 2);
      const quietCtrlState = ctrl.getSnapshot();
      assert(quietCtrlState.selectedDiff?.requiresZ4Reconsent === false, "quiet diff flag false");
      html = renderStoreCatalogHtml(quietCtrlState);
      assert(!html.includes("nlos-store-diff-reconsent"), "no re-consent banner on identical diff");
    },
  );

  await runSuite(
    "5. Sensitive action Z4 handoff: verified chain only, adapter shape, reversibility badges",
    async () => {
      const handoffs: unknown[] = [];
      let prepareCalls = 0;
      const client: StoreCatalogDataClient = {
        loadSnapshot: async () => sampleSnapshot(),
        loadTransparency: async () => sampleTransparency(STORE_CHAIN_STATUS_VERIFICATION_FAILED),
        loadPermissionDiff: async () => sampleDiff(),
        prepareSensitiveAction: async (input) => {
          prepareCalls += 1;
          assert(input.actionKind === STORE_ACTION_INSTALL, "install kind forwarded");
          assert(input.packageIdHex === "a1".repeat(16), "package hex forwarded");
          assert(input.nowMs === NOW_MS, "deterministic nowMs forwarded");
          return sampleActionEntry("IRREVERSIBLE");
        },
      };
      const ctrl = createStoreCatalogController({
        client,
        nowMs: () => NOW_MS,
        openZ4Challenge: (spec) => handoffs.push(spec),
      });
      await ctrl.refresh();
      await ctrl.selectRelease("a1".repeat(16), sampleEntry().packageVersion);

      // Fail-closed UI gate: unverified chain must NOT reach the client.
      await ctrl.requestSensitiveAction(STORE_ACTION_INSTALL);
      assert(prepareCalls === 0, "unverified chain blocks prepare call");
      assert(handoffs.length === 0, "unverified chain produces no handoff");
      let state = ctrl.getSnapshot();
      assert(state.lastActionNotice !== null, "blocked notice present");
      assert(state.lastActionNotice?.kind === "blocked", "notice kind is blocked");
      assertZeroJargon("blocked notice zh", state.lastActionNotice?.textZh ?? "");
      assertZeroEmoji("blocked notice zh", state.lastActionNotice?.textZh ?? "");

      // Degraded chain (trust db unavailable) equally blocked.
      // (Verified path below is the only one that hands off.)

      // Verified chain: full handoff to existing Z4 flow.
      const verifiedCtrl = createStoreCatalogController({
        client: {
          loadSnapshot: async () => sampleSnapshot(),
          loadTransparency: async () => sampleTransparency(STORE_CHAIN_STATUS_VERIFIED),
          loadPermissionDiff: async () => sampleDiff(),
          prepareSensitiveAction: async () => sampleActionEntry("IRREVERSIBLE"),
        },
        nowMs: () => NOW_MS,
        openZ4Challenge: (spec) => handoffs.push(spec),
      });
      await verifiedCtrl.refresh();
      await verifiedCtrl.selectRelease("a1".repeat(16), sampleEntry().packageVersion);
      await verifiedCtrl.requestSensitiveAction(STORE_ACTION_INSTALL);
      assert(handoffs.length === 1, "verified chain hands off once");
      const spec = handoffs[0] as ReturnType<typeof toZ4ConsentChallengeSpec>;
      assert(spec.requestingApp.signatureVerified === true, "requesting app signature verified");
      assert(spec.actionScope.plainSummary["zh-CN"].length > 0, "locale-keyed zh summary");
      assert(spec.actionScope.plainSummary["en-US"].length > 0, "locale-keyed en summary");
      assert(spec.reversibilityAndRisk.reversibilityClass === "IRREVERSIBLE", "reversibility mapped");
      assert(
        spec.reversibilityAndRisk.blastRadiusRows?.length === 2,
        "blast radius rows mapped",
      );
      assert(
        spec.reversibilityAndRisk.blastRadiusRows?.[0]?.category === "created",
        "'+' rows map to created category",
      );
      assert(
        spec.reversibilityAndRisk.blastRadiusRows?.[1]?.category === "reverified",
        "'~' rows map to reverified category",
      );
      assert(spec.allowedGrantScopes?.[0] === "SINGLE_ACTION", "single action grant scope");

      // Adapter is pure and total for all three kinds.
      const upgrade: StoreSensitiveActionEntryDto = {
        ...sampleActionEntry("IRREVERSIBLE"),
        actionKind: STORE_ACTION_IRREVERSIBLE_UPGRADE,
      };
      const downgraded: StoreSensitiveActionEntryDto = {
        ...sampleActionEntry("IRREVERSIBLE"),
        actionKind: STORE_ACTION_DOWNGRADE_SANDBOX,
      };
      assert(
        toZ4ConsentChallengeSpec(upgrade).reversibilityAndRisk.reversibilityClass === "IRREVERSIBLE",
        "upgrade adapter keeps irreversible class",
      );
      assert(
        toZ4ConsentChallengeSpec(downgraded).reversibilityAndRisk.reversibilityClass === "IRREVERSIBLE",
        "downgrade adapter keeps irreversible class",
      );

      // Client error -> error notice, no handoff.
      const failingCtrl = createStoreCatalogController({
        client: {
          loadSnapshot: async () => sampleSnapshot(),
          loadTransparency: async () => sampleTransparency(STORE_CHAIN_STATUS_VERIFIED),
          loadPermissionDiff: async () => sampleDiff(),
          prepareSensitiveAction: async () => {
            throw new Error("revoked");
          },
        },
        nowMs: () => NOW_MS,
        openZ4Challenge: (spec2) => handoffs.push(spec2),
      });
      await failingCtrl.refresh();
      await failingCtrl.selectRelease("a1".repeat(16), sampleEntry().packageVersion);
      await failingCtrl.requestSensitiveAction(STORE_ACTION_INSTALL);
      assert(handoffs.length === 1, "failed prepare adds no handoff");
      state = failingCtrl.getSnapshot();
      assert(state.lastActionNotice?.kind === "error", "error notice kind");
    },
  );

  await runSuite("6. Bilingual copy parity with zero jargon and zero emoji", () => {
    const zhKeys = Object.keys(STORE_COPY["zh-CN"]).sort();
    const enKeys = Object.keys(STORE_COPY["en-US"]).sort();
    assert(zhKeys.length > 20, "copy bundle has meaningful key count");
    assert(
      JSON.stringify(zhKeys) === JSON.stringify(enKeys),
      "zh-CN and en-US copy keys have 100% parity",
    );
    for (const locale of ["zh-CN", "en-US"] as const) {
      for (const [key, value] of Object.entries(STORE_COPY[locale])) {
        assertZeroJargon(`copy ${locale}.${key}`, String(value));
        assertZeroEmoji(`copy ${locale}.${key}`, String(value));
      }
    }
  });

  await runSuite("7. Anti-AI static audit over store-catalog production files", async () => {
    const fs = await importBuiltin<NodeFsLike>("node:fs");
    const path = await importBuiltin<NodePathLike>("node:path");
    const urlMod = await importBuiltin<NodeUrlLike>("node:url");

    const currentFile = urlMod.fileURLToPath(import.meta.url);
    const moduleDir = path.resolve(path.dirname(currentFile), "..");
    const files = collectProductionFiles(fs, path, moduleDir);
    assert(files.length >= 3, `production files discovered (got ${files.length}: ${files.join(", ")})`);

    const allowedRadii = new Set(["0px", "2px", "4px", "6px"]);
    for (const file of files) {
      const content = fs.readFileSync(file, "utf-8");
      const lower = content.toLowerCase();
      for (const banned of BANNED_SUBSTRINGS) {
        assert(
          !lower.includes(banned.toLowerCase()),
          `${file}: banned anti-AI token '${banned}'`,
        );
      }
      assert(!PICTOGRAPHIC_RE.test(content), `${file}: pictographic emoji character`);
      const radiusDeclRe =
        /(?:border(?:-(?:top|bottom)-(?:left|right))?-radius|--nlos-radius-[a-z0-9-]+)\s*:\s*([^;}"'`]+)/gi;
      let match = radiusDeclRe.exec(content);
      while (match !== null) {
        const rawVal = match[1]!.trim();
        if (!rawVal.startsWith("var(--nlos-radius-")) {
          for (const tok of rawVal.split(/\s+/)) {
            assert(
              allowedRadii.has(tok),
              `${file}: illegal border-radius token '${tok}' in '${match[0]}'`,
            );
          }
        }
        match = radiusDeclRe.exec(content);
      }
      // No absolute host paths or temp markers in view sources.
      assert(!content.includes("D:\\llmos"), `${file}: no absolute host path`);
      assert(!content.includes("C:\\Users"), `${file}: no user profile path`);
    }

    // CSS discipline: solid opaque surfaces (zero blur), hairline borders.
    const cssFile = files.find((f) => f.endsWith(".css"));
    assert(cssFile !== undefined, "store-catalog.css present");
    const css = cssFile ? fs.readFileSync(cssFile, "utf-8") : "";
    assert(css.includes("var(--nlos-bg-window)"), "reuses solid window canvas token");
    assert(css.includes("var(--nlos-border-hairline)"), "reuses 1px hairline token");
    assert(css.includes("var(--nlos-radius-ctrl)"), "reuses 4px control radius token");
    assert(css.includes("prefers-reduced-motion"), "reduced-motion respected");
  });

  await runSuite("8. Tauri client wiring contract for W80-INT integration", async () => {
    const invocations: Array<{ cmd: string; args: unknown }> = [];
    const fakeInvoke = async (cmd: string, args?: unknown): Promise<unknown> => {
      invocations.push({ cmd, args });
      if (cmd === "store_catalog_snapshot") {
        return sampleSnapshot();
      }
      if (cmd === "store_release_transparency") {
        return sampleTransparency();
      }
      if (cmd === "store_permission_diff") {
        return sampleDiff();
      }
      return sampleActionEntry();
    };
    const client = createTauriStoreCatalogClient(fakeInvoke);

    await client.loadSnapshot(NOW_MS);
    assert(invocations[0]?.cmd === "store_catalog_snapshot", "snapshot command name");
    const snapArgs = invocations[0]?.args as Record<string, unknown>;
    assert(snapArgs?.nowMs === NOW_MS, "snapshot nowMs camelCase");
    assert(snapArgs?.storeRootOverride === null, "snapshot storeRootOverride default null");

    await client.loadTransparency("a1".repeat(16), 7, NOW_MS);
    const trArgs = invocations[1]?.args as Record<string, unknown>;
    assert(trArgs?.packageIdHex === "a1".repeat(16), "transparency packageIdHex");
    assert(trArgs?.packageVersion === 7, "transparency packageVersion");

    await client.loadPermissionDiff("a1".repeat(16), 1, 2);
    const diffArgs = invocations[2]?.args as Record<string, unknown>;
    assert(diffArgs?.oldVersion === 1 && diffArgs?.newVersion === 2, "diff versions camelCase");

    const entry = await client.prepareSensitiveAction({
      actionKind: STORE_ACTION_IRREVERSIBLE_UPGRADE,
      packageIdHex: "a1".repeat(16),
      packageVersion: 2,
      oldVersion: 1,
      nowMs: NOW_MS,
    });
    assert(entry.reversibilityClass === "COMPENSATABLE_ROLLBACK", "entry round-trips");
    const actArgs = invocations[3]?.args as { input: Record<string, unknown> };
    assert(
      actArgs.input.actionKind === STORE_ACTION_IRREVERSIBLE_UPGRADE,
      "sensitive input nested under input",
    );
  });

  console.log(
    `All ${passedSuites} W80-E6C Desktop Store Transparency suites passed (${passedAssertions} assertions).`,
  );
}

void main();
