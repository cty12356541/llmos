// ==========================================================================
// LLMOS Studio · Open Swiss Editorial Governance & Probe Views
// (`desktop/src/studio/views-governance.ts`)
// Implements Views 05..08 with zero pill soup & open hairline cross-grid:
//   05. 3-Domain Durable Recovery & OpenMetrics (`buildRecoveryTelemetryViewElement`)
//   06. Applications & Store PKI Catalog (`buildAppsStoreViewElement`)
//   07. 4-Layer Settings CAS & Keychain Vault (`buildSecuritySettingsViewElement`)
//   08. Raw SABI / SystemControl IPC Probes (`buildKernelProbesViewElement`)
// ==========================================================================

import type { SettingsDomainLayer } from "../views/settings";
import { renderStoreCatalogHtml } from "../views/store-catalog";
import type { OperatorStudioStore, StudioStateSnapshot } from "./studio-store";

function esc(raw: string): string {
  return raw
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

/* ==========================================================================
   05. 3-Domain Durable Recovery & OpenMetrics Telemetry View
   ========================================================================== */

export function buildRecoveryTelemetryViewElement(
  doc: Document,
  store: OperatorStudioStore,
  snap: StudioStateSnapshot,
): HTMLElement {
  const isEn = snap.locale === "en-US";
  const wrap = doc.createElement("div");

  const domains: Array<{
    num: string;
    key: "artifact" | "semantic" | "resource";
    titleZh: string;
    titleEn: string;
    descZh: string;
    descEn: string;
  }> = [
    {
      num: "01",
      key: "artifact",
      titleZh: "Artifact 制品提交恢复域",
      titleEn: "Artifact Commit Recovery",
      descZh: "文件/Blob 落盘与 TaskCommitReceipt 幂等收敛",
      descEn: "Durable blob fsync & TaskCommitReceipt convergence",
    },
    {
      num: "02",
      key: "semantic",
      titleZh: "Semantic 语义发布恢复域",
      titleEn: "Semantic Publication Recovery",
      descZh: "v42 台账跨权威语义索引对账与故障隔离",
      descEn: "v42 ledger cross-authority semantic convergence",
    },
    {
      num: "03",
      key: "resource",
      titleZh: "Resource 五维预算恢复域",
      titleEn: "Resource Settlement Recovery",
      descZh: "五维 Token 与计算预留结算对账",
      descEn: "5D LlmTokenBudget & compute reservation settlement",
    },
  ];

  const domainColumnsHtml = domains
    .map((d, idx) => {
      const h = snap.recoveryHealth[d.key];
      const hasAlert = h.escalated > 0 || h.unacknowledged > 0;
      const borderRight =
        idx < 2 ? "border-right:1px solid var(--st-rule-mid);" : "";
      return (
        `<section style="${borderRight}display:flex;flex-direction:column;">` +
        `<div class="st-sec-head">` +
        `<span class="st-sec-num">${d.num}</span>` +
        `<h2 class="st-sec-title">${esc(isEn ? d.titleEn : d.titleZh)}</h2>` +
        `<span class="st-seal" style="margin-left:auto;" data-tone="${hasAlert ? "vermilion" : "forest"}">${esc(h.workerState.toUpperCase())}</span>` +
        `</div>` +
        `<div style="padding:12px 16px;display:flex;flex-direction:column;gap:10px;">` +
        `<div style="font-size:12px;color:var(--st-ink-600);">${esc(isEn ? d.descEn : d.descZh)}</div>` +
        `<div style="display:grid;grid-template-columns:repeat(4,1fr);border:1px solid var(--st-rule-soft);">` +
        `<div style="padding:8px;border-right:1px solid var(--st-rule-soft);"><div style="font-size:10.5px;color:var(--st-ink-500);">${isEn ? "CYCLES" : "巡检轮次"}</div><strong class="st-mono" style="font-size:16px;color:var(--st-ink-950);">${h.completedCycles}</strong></div>` +
        `<div style="padding:8px;border-right:1px solid var(--st-rule-soft);"><div style="font-size:10.5px;color:var(--st-ink-500);">${isEn ? "RETRY" : "退避重试"}</div><strong class="st-mono" style="font-size:16px;color:var(--st-ink-950);">${h.retrying}</strong></div>` +
        `<div style="padding:8px;border-right:1px solid var(--st-rule-soft);"><div style="font-size:10.5px;color:var(--st-ink-500);">${isEn ? "ALERTS" : "升级告警"}</div><strong class="st-mono" style="font-size:16px;color:var(--st-ink-950);">${h.escalated}</strong></div>` +
        `<div style="padding:8px;"><div style="font-size:10.5px;color:var(--st-ink-500);">${isEn ? "RESOLVED" : "已收敛"}</div><strong class="st-mono" style="font-size:16px;color:var(--st-ink-950);">${h.resolved}</strong></div>` +
        `</div>` +
        `<div style="display:flex;justify-content:space-between;align-items:center;gap:8px;padding-top:4px;">` +
        `<span class="st-mono" style="font-size:11px;">receipt: #${esc((h.lastReceiptHex ?? "—").slice(0, 14))}…</span>` +
        `<button type="button" class="st-btn st-btn--primary" data-inspect-recovery="${d.key}">${isEn ? "Inspect Domain" : "执行实时巡检"}</button>` +
        `</div>` +
        `</div>` +
        `</section>`
      );
    })
    .join("");

  const metricsRowsHtml = snap.metricFamilies
    .map((fam) => {
      const sampleSummary = fam.samples
        .slice(0, 3)
        .map((s) => {
          const lbl = Object.entries(s.labels)
            .map(([k, v]) => `${k}="${v}"`)
            .join(",");
          return lbl ? `{${lbl}} = ${s.value}` : s.value;
        })
        .join(" | ");
      return (
        `<tr>` +
        `<td><span class="st-mono" style="color:var(--st-ink-950);font-weight:600;">${esc(fam.name)}</span></td>` +
        `<td><span class="st-seal">${esc(fam.kind.toUpperCase())}</span></td>` +
        `<td><span class="st-mono">${fam.samples.length}</span></td>` +
        `<td><span class="st-mono">${esc(sampleSummary || "0")}</span></td>` +
        `</tr>`
      );
    })
    .join("");

  wrap.innerHTML =
    `<header class="st-masthead-band">` +
    `<div>` +
    `<div class="st-page-kicker">SECTION 05 · DURABLE RECOVERY LEDGERS & OPENMETRICS</div>` +
    `<h1 class="st-page-title">${isEn ? "3-Domain Durable Recovery & OpenMetrics Telemetry" : "三域持久化恢复台账与 OpenMetrics 遥测"}</h1>` +
    `<p class="st-page-desc">${
      isEn
        ? "Dual-domain worker isolation (Artifact / Semantic / Resource) with exponential backoff CAS ledgers, alert acknowledgement, and deterministic OpenMetrics export."
        : "Artifact / Semantic / Resource 独立故障域隔离巡检，支持指数退避升级告警确认、人工恢复重试与三域 OpenMetrics 指标实时抓取。"
    }</p>` +
    `</div>` +
    `<button type="button" class="st-btn st-btn--primary" data-action="export-openmetrics">${isEn ? "Fetch Live OpenMetrics" : "抓取三域 OpenMetrics 指标"}</button>` +
    `</header>` +
    `<div style="display:grid;grid-template-columns:repeat(3,minmax(0,1fr));border-bottom:1px solid var(--st-rule-mid);">${domainColumnsHtml}</div>` +
    `<section class="st-block">` +
    `<div class="st-sec-head">` +
    `<span class="st-sec-num">04</span>` +
    `<h2 class="st-sec-title">${isEn ? "Parsed OpenMetrics Families (Artifact + Semantic + Resource)" : "OpenMetrics 结构化指标族台账"}</h2>` +
    `<span class="st-sec-kicker">${snap.metricFamilies.length} FAMILIES</span>` +
    `</div>` +
    `<div class="st-table-wrap">` +
    `<table class="st-table">` +
    `<thead><tr><th>${isEn ? "METRIC FAMILY" : "指标族名称"}</th><th>${isEn ? "KIND" : "类型"}</th><th>${isEn ? "SERIES" : "样本数"}</th><th>${isEn ? "VALUES" : "当前采样值"}</th></tr></thead>` +
    `<tbody>${metricsRowsHtml}</tbody>` +
    `</table></div>` +
    `</section>`;

  wrap.addEventListener("click", (ev: MouseEvent) => {
    const target =
      ev.target && typeof (ev.target as Element).closest === "function"
        ? (ev.target as Element)
        : null;
    if (!target) {
      return;
    }

    const recBtn = target.closest("[data-inspect-recovery]");
    if (recBtn) {
      const dom = recBtn.getAttribute("data-inspect-recovery") as
        | "artifact"
        | "semantic"
        | "resource"
        | null;
      if (dom) {
        void store.runRecoveryInspect(dom);
      }
      return;
    }

    if (target.closest('[data-action="export-openmetrics"]')) {
      void store.refreshOpenMetrics();
    }
  });

  return wrap;
}

function humanizeStoreCatalogHtml(rawHtml: string, isEn: boolean): string {
  return rawHtml
    .replace(/a1a1a1a1/g, "knowledge-observatory-app")
    .replace(/b2b2b2b2/g, "field-survey-app")
    .replace(/c3c3c3c3/g, "mcp-bridge-driver")
    .replace(/d4d4d4d4/g, "openai-compat-driver")
    .replace(/1099511627777/g, "v1.0.1 (Epoch 1)")
    .replace(/1099511627776/g, "v1.0.0 (Epoch 1)")
    .replace(/✓/g, isEn ? "[OK]" : "[已验签]");
}

export function buildAppsStoreViewElement(
  doc: Document,
  store: OperatorStudioStore,
  snap: StudioStateSnapshot,
): HTMLElement {
  const isEn = snap.locale === "en-US";
  const activeStoreCtrl =
    snap.storeMode === "showcase"
      ? store.controllers.showcaseStoreCatalog
      : store.controllers.liveStoreCatalog;

  const wrap = doc.createElement("div");

  const installedApps = [
    {
      nameZh: "starter-assistant-app (开箱生产力样板套件)",
      nameEn: "starter-assistant-app (Starter Productivity Suite)",
      pkgHex: "10".repeat(16),
      version: "1.1.0",
      sandbox: "Process + Wasmtime Dual-Layer",
      status: "ACTIVE",
    },
    {
      nameZh: "nlos-driver-llm-http (用户态大模型 HTTP 驱动)",
      nameEn: "nlos-driver-llm-http (User-Space LLM HTTP Driver)",
      pkgHex: "20".repeat(16),
      version: "1.0.0",
      sandbox: "LlmEnforcementShim Mediated",
      status: "ACTIVE",
    },
  ];

  wrap.innerHTML =
    `<header class="st-masthead-band">` +
    `<div>` +
    `<div class="st-page-kicker">SECTION 06 · OCI STORE REGISTRY & 3-TIER PUBLISHER PKI</div>` +
    `<h1 class="st-page-title">${isEn ? "Installed Applications & Governed Ecosystem Store" : "已安装应用生命周期与受治理生态商店"}</h1>` +
    `<p class="st-page-desc">${
      isEn
        ? "OCI content-addressed packages, 3-tier Ed25519 publisher PKI verification, monotonic RevocationEpoch CRLs, permission transparency diffs, and Z4 installation consent."
        : "OCI 内容寻址不可变签名包、三级发布者 PKI 证书链实时复验、权限与目的透明度清单，以及敏感安装/升级动作的 Z4 实心确认联动。"
    }</p>` +
    `</div>` +
    `<div class="st-seg-group">` +
    `<button type="button" class="st-seg-btn ${snap.storeMode === "showcase" ? "is-active" : ""}" data-store-switch="showcase">${isEn ? "Stage E Reference Packages (4)" : "内置参考生态样板包 (4)"}</button>` +
    `<button type="button" class="st-seg-btn ${snap.storeMode === "live" ? "is-active" : ""}" data-store-switch="live">${isEn ? "Live Local Registry" : "实时本地商店数据库"}</button>` +
    `</div>` +
    `</header>` +
    `<section class="st-block">` +
    `<div class="st-sec-head">` +
    `<span class="st-sec-num">01</span>` +
    `<h2 class="st-sec-title">${isEn ? "Active Local Installations" : "本机已激活应用与用户态驱动"}</h2>` +
    `<span class="st-sec-kicker">INSTALLED</span>` +
    `</div>` +
    `<div class="st-table-wrap">` +
    `<table class="st-table">` +
    `<thead><tr><th>${isEn ? "PACKAGE / APPLICATION" : "应用 / 驱动名称"}</th><th>${isEn ? "VERSION" : "版本"}</th><th>${isEn ? "ISOLATION PROFILE" : "沙箱与隔离级别"}</th><th>${isEn ? "STATUS" : "状态"}</th><th>${isEn ? "LIFECYCLE" : "生命周期控制"}</th></tr></thead>` +
    `<tbody>` +
    installedApps
      .map(
        (app) =>
          `<tr>` +
          `<td><strong style="color:var(--st-ink-950);">${esc(isEn ? app.nameEn : app.nameZh)}</strong> <span class="st-mono">#${app.pkgHex.slice(0, 12)}…</span></td>` +
          `<td><span class="st-mono">v${app.version}</span></td>` +
          `<td>${esc(app.sandbox)}</td>` +
          `<td><span class="st-seal" data-tone="forest">${app.status}</span></td>` +
          `<td><div style="display:flex;gap:8px;">` +
          `<button type="button" class="st-btn" data-app-probe="surfaces" data-pkg="${app.pkgHex}">${isEn ? "Surfaces" : "检视表面"}</button>` +
          `<button type="button" class="st-btn st-btn--vermilion" data-app-probe="disable" data-pkg="${app.pkgHex}">${isEn ? "Disable" : "停用"}</button>` +
          `</div></td>` +
          `</tr>`,
      )
      .join("") +
    `</tbody></table></div>` +
    `</section>` +
    `<section class="st-block">` +
    `<div class="st-sec-head">` +
    `<span class="st-sec-num">02</span>` +
    `<h2 class="st-sec-title">${isEn ? "Store Catalog & Certificate Chain Transparency" : "商店目录与三级证书链透明度检视（点击任意包行切换详情并可触发 Z4 安装确认）"}</h2>` +
    `<span class="st-sec-kicker">PKI & PERMISSIONS</span>` +
    `</div>` +
    `<div class="st-block-body store-catalog-host">` +
    humanizeStoreCatalogHtml(
      renderStoreCatalogHtml(activeStoreCtrl.getSnapshot()),
      isEn,
    ) +
    `</div>` +
    `</section>`;

  wrap.addEventListener("click", (ev: MouseEvent) => {
    const target =
      ev.target && typeof (ev.target as Element).closest === "function"
        ? (ev.target as Element)
        : null;
    if (!target) {
      return;
    }

    const modeBtn = target.closest("[data-store-switch]");
    if (modeBtn) {
      const m = modeBtn.getAttribute("data-store-switch") as
        | "showcase"
        | "live"
        | null;
      if (m) {
        store.setStoreMode(m);
        const ctrl =
          m === "showcase"
            ? store.controllers.showcaseStoreCatalog
            : store.controllers.liveStoreCatalog;
        void ctrl.refresh().then(() => {
          store.setStoreMode(m);
        });
      }
      return;
    }

    const probeBtn = target.closest("[data-app-probe][data-pkg]");
    if (probeBtn) {
      const kind = probeBtn.getAttribute("data-app-probe");
      const pkg = probeBtn.getAttribute("data-pkg") ?? "";
      if (kind === "surfaces") {
        void store.executeRawIpcProbe("present_surfaces", pkg);
      } else if (kind === "disable") {
        store.openZ4Confirmation({
          promptId: `z4-disable-${pkg.slice(0, 8)}`,
          title: `停用应用包 ${pkg.slice(0, 8)}…`,
          summary: "暂停该应用全部后台工作集并冻结关联 Capability 令牌",
          consequence: "用户数据完整保留，随时可重新启用",
          irreversible: false,
          onConfirm: () => {
            void store.dispatchControlAction({
              action: "disable-application",
              targetIdHex: pkg,
              expectedRevision: 1,
              scope: "application",
              reason: "Disabled from Operator Studio",
            });
          },
        });
      }
      return;
    }

    const actionBtn = target.closest("[data-store-action-kind]");
    if (actionBtn && !actionBtn.hasAttribute("disabled")) {
      const kind = actionBtn.getAttribute("data-store-action-kind");
      if (kind) {
        void activeStoreCtrl.requestSensitiveAction(kind);
      }
      return;
    }

    const storeRow = target.closest(
      ".nlos-store-row[data-store-pkg][data-store-version]",
    );
    if (storeRow) {
      const pkg = storeRow.getAttribute("data-store-pkg");
      const ver = Number(storeRow.getAttribute("data-store-version"));
      if (pkg && Number.isFinite(ver)) {
        void activeStoreCtrl.selectRelease(pkg, ver).then(() => {
          const host = wrap.querySelector(".store-catalog-host");
          if (host) {
            host.innerHTML = humanizeStoreCatalogHtml(
              renderStoreCatalogHtml(activeStoreCtrl.getSnapshot()),
              isEn,
            );
          }
        });
      }
    }
  });

  return wrap;
}

/* ==========================================================================
   07. Security, 4-Layer Settings CAS & Zero-Secret Keychain Vault View
   ========================================================================== */

export function buildSecuritySettingsViewElement(
  doc: Document,
  store: OperatorStudioStore,
  snap: StudioStateSnapshot,
): HTMLElement {
  const isEn = snap.locale === "en-US";
  const suite = store.controllers.settingsSuite;

  const wrap = doc.createElement("div");

  wrap.innerHTML =
    `<header class="st-masthead-band">` +
    `<div>` +
    `<div class="st-page-kicker">SECTION 07 · 4-LAYER SETTINGS CAS & ZERO-SECRET VAULT</div>` +
    `<h1 class="st-page-title">${isEn ? "Security Policy, 4-Layer Settings CAS & Keychain Vault" : "安全基线、四层 CAS 设置继承与零明文密钥保险库"}</h1>` +
    `<p class="st-page-desc">${
      isEn
        ? "Hierarchical SettingsDomain CAS (System v10 / User v20 / Workspace v30 / Application v40), user-revocable MIME/URI associations, and hardware-backed SigningKeyCustodyVault references."
        : "管理四层配置版本继承（系统基线锁定防篡改）、用户可随时撤销的文件/协议默认关联，以及零原始私钥暴露的硬件托管密钥引用。"
    }</p>` +
    `</div>` +
    `</header>` +
    `<section class="st-block">` +
    `<div class="st-sec-head">` +
    `<span class="st-sec-num">01</span>` +
    `<h2 class="st-sec-title">${isEn ? "Settings CAS, Associations & Keychain Reference Matrix" : "四层配置继承、默认关联与密钥引用矩阵"}</h2>` +
    `<span class="st-sec-kicker">CAS & VAULT</span>` +
    `</div>` +
    `<div>` +
    suite.renderSuiteHtml({
      activeLayer: snap.activeSettingsLayer,
      locale: snap.locale,
      disclosure: snap.disclosure,
    }) +
    `</div>` +
    `</section>`;

  wrap.addEventListener("click", (ev: MouseEvent) => {
    const target =
      ev.target && typeof (ev.target as Element).closest === "function"
        ? (ev.target as Element)
        : null;
    if (!target) {
      return;
    }

    const layerBtn = target.closest("[data-settings-layer]");
    if (layerBtn) {
      const l = layerBtn.getAttribute(
        "data-settings-layer",
      ) as SettingsDomainLayer | null;
      if (l) {
        store.setActiveSettingsLayer(l);
      }
      return;
    }

    const approveBtn = target.closest(
      '[data-action="approve-proposal"][data-content-type]',
    );
    if (approveBtn) {
      const ct = approveBtn.getAttribute("data-content-type");
      if (ct) {
        const cur = suite.associations
          .listAssociations()
          .find((i) => i.contentTypeOrUri === ct);
        if (cur && cur.pendingProposal) {
          suite.associations.bindUserDefaultAssociation({
            contentTypeOrUri: ct,
            applicationId: cur.pendingProposal.proposedApplicationId,
            installationId: cur.pendingProposal.proposedInstallationId,
            handlerLabels: cur.pendingProposal.proposedLabels,
            expectedRevision: cur.revision,
          });
          store.setBannerNotice(
            isEn
              ? `Updated default handler for ${ct}`
              : `已批准并更新 ${ct} 的默认关联应用`,
            "info",
          );
        }
      }
      return;
    }

    const revokeAssoc = target.closest(
      '[data-action="revoke-association"][data-content-type]',
    );
    if (revokeAssoc) {
      const ct = revokeAssoc.getAttribute("data-content-type");
      if (ct) {
        const rev = Number(revokeAssoc.getAttribute("data-revision") ?? "1");
        suite.associations.revokeAssociationToSystemChooser(ct, rev);
        store.setBannerNotice(
          isEn
            ? `Reset ${ct} association to Always Ask`
            : `已撤销 ${ct} 默认关联并重置为系统选择器每次询问`,
          "info",
        );
      }
      return;
    }

    const revokeKey = target.closest(
      '[data-action="revoke-keychain-ref"][data-keychain-ref]',
    );
    if (revokeKey) {
      const refId = revokeKey.getAttribute("data-keychain-ref");
      if (refId) {
        const rev = Number(revokeKey.getAttribute("data-revision") ?? "1");
        store.openZ4Confirmation({
          promptId: `z4-revoke-key-${refId}`,
          title: isEn
            ? `Revoke Credential Reference ${refId}`
            : `永久吊销密钥引用 ${refId}`,
          summary: isEn
            ? "Immediately invalidates all active handles bound to this keychain reference."
            : "立即使绑定该密钥引用的所有活跃会话句柄失效。",
          consequence: isEn
            ? "Irreversible revocation; raw secret material remains sealed."
            : "吊销不可逆；底层私钥字节全程零暴露。",
          irreversible: true,
          onConfirm: () => {
            suite.keychainVault.revokeKeychainReference(refId, rev);
          },
        });
      }
    }
  });

  return wrap;
}

/* ==========================================================================
   08. Raw Kernel IPC Probe & Connection Studio View
   ========================================================================== */

export function buildKernelProbesViewElement(
  doc: Document,
  store: OperatorStudioStore,
  snap: StudioStateSnapshot,
): HTMLElement {
  const isEn = snap.locale === "en-US";
  const cfg = snap.config;

  const wrap = doc.createElement("div");

  const probeCommands = [
    { id: "inspect_health", label: "inspect_health (Artifact 恢复巡检)" },
    { id: "inspect_task", label: "inspect_task (按 32-hex plan_id 查询计划)" },
    {
      id: "inspect_process",
      label: "inspect_process (按 32-hex process_id 查询进程绑定)",
    },
    {
      id: "inspect_resource",
      label: "inspect_resource (按 32-hex reservation_id 查询预留)",
    },
    {
      id: "inspect_resource_cost",
      label: "inspect_resource_cost (查询五维成本账本)",
    },
    {
      id: "cost_fact_check",
      label: "cost_fact_check (双次独立派发成本一致性核验)",
    },
    {
      id: "inspect_application",
      label: "inspect_application (按 32-hex package_id 查询应用头)",
    },
    {
      id: "present_surfaces",
      label: "present_surfaces (读取包声明表面清单)",
    },
    {
      id: "inspect_cell_leases",
      label: "inspect_cell_leases (查询计算节点三族租约)",
    },
    {
      id: "parity_check",
      label: "parity_check (GUI 鉴权通道 vs CLI 通道字节等价校验)",
    },
    {
      id: "parity_check_write",
      label: "parity_check_write (写路径暂停命令等价探针)",
    },
  ];

  wrap.innerHTML =
    `<header class="st-masthead-band">` +
    `<div>` +
    `<div class="st-page-kicker">SECTION 08 · RAW SABI RPC DISPATCHER & TRANSPORT BINDING</div>` +
    `<h1 class="st-page-title">${isEn ? "Raw SABI / SystemControl IPC Probe & Transport Config" : "内核 SABI / SystemControl 原始 RPC 探针与传输配置"}</h1>` +
    `<p class="st-page-desc">${
      isEn
        ? "Direct developer access to authenticated Named Pipe / Unix Socket SystemControl endpoints, GUI <-> CLI receipt byte-parity verification, and session socket configuration."
        : "面向内核排障与联调：直接向鉴权 Named Pipe / Unix Socket 派发原始 RPC 探针、验证 GUI <-> CLI 回执字节级等价，并管理会话连接参数。"
    }</p>` +
    `</div>` +
    `</header>` +
    `<div class="st-split-2">` +
    `<section class="st-block" style="border-bottom:none;">` +
    `<div class="st-sec-head">` +
    `<span class="st-sec-num">01</span>` +
    `<h2 class="st-sec-title">${isEn ? "Interactive RPC Command Dispatcher" : "交互式 SystemControl RPC 派发器"}</h2>` +
    `<span class="st-sec-kicker">SABI_INVOKE</span>` +
    `</div>` +
    `<div class="st-block-body" style="display:flex;flex-direction:column;gap:14px;">` +
    `<label style="display:flex;flex-direction:column;gap:4px;">` +
    `<span class="st-mono" style="font-size:11px;">${isEn ? "SELECT RPC COMMAND" : "选择后端 Tauri / SystemControl 命令"}</span>` +
    `<select id="studio-probe-cmd" class="st-input" style="width:100%;">` +
    probeCommands
      .map((c) => `<option value="${esc(c.id)}">${esc(c.label)}</option>`)
      .join("") +
    `</select>` +
    `</label>` +
    `<label style="display:flex;flex-direction:column;gap:4px;">` +
    `<span class="st-mono" style="font-size:11px;">${isEn ? "TARGET 32-HEX IDENTIFIER" : "目标 32 位 Hex 标识符（全局命令可留空）"}</span>` +
    `<input type="text" id="studio-probe-hex" class="st-input" value="82eb946cb400ece8dfafa7944b0dc51b" />` +
    `</label>` +
    `<div style="padding-top:4px;">` +
    `<button type="button" class="st-btn st-btn--primary" data-action="fire-ipc-probe">${isEn ? "Dispatch RPC & Inspect Receipt →" : "派发鉴权 RPC 并在右轨检视回执 →"}</button>` +
    `</div>` +
    `</div>` +
    `</section>` +
    `<section class="st-block" style="border-bottom:none;">` +
    `<div class="st-sec-head">` +
    `<span class="st-sec-num">02</span>` +
    `<h2 class="st-sec-title">${isEn ? "Control-Plane IPC Transport Binding" : "当前控制面传输通道与密钥绑定"}</h2>` +
    `<span class="st-seal" style="margin-left:auto;" data-tone="accent">${esc(cfg.source.toUpperCase())}</span>` +
    `</div>` +
    `<div class="st-block-body" style="display:flex;flex-direction:column;gap:10px;">` +
    `<label style="display:flex;flex-direction:column;gap:3px;"><span class="st-mono" style="font-size:10.5px;">AUTHENTICATED SOCKET / NAMED PIPE</span><input type="text" id="cfg-auth-socket" class="st-input" value="${esc(cfg.socketPath ?? "")}" /></label>` +
    `<label style="display:flex;flex-direction:column;gap:3px;"><span class="st-mono" style="font-size:10.5px;">OPERATOR PRINCIPAL (32-HEX)</span><input type="text" id="cfg-principal" class="st-input" value="${esc(cfg.principalHex ?? "")}" /></label>` +
    `<label style="display:flex;flex-direction:column;gap:3px;"><span class="st-mono" style="font-size:10.5px;">ED25519 KEY FILE PATH</span><input type="text" id="cfg-keyfile" class="st-input" value="${esc(cfg.keyFile ?? "")}" /></label>` +
    `<label style="display:flex;flex-direction:column;gap:3px;"><span class="st-mono" style="font-size:10.5px;">PLAIN DIAGNOSTIC CLI SOCKET</span><input type="text" id="cfg-cli-socket" class="st-input" value="${esc(cfg.cliSocket ?? "")}" /></label>` +
    `<div style="padding-top:4px;"><button type="button" class="st-btn st-btn--primary" data-action="save-ipc-config">${isEn ? "Apply Session Transport Config" : "保存并应用会话连接配置"}</button></div>` +
    `</div>` +
    `</section>` +
    `</div>`;

  wrap.addEventListener("click", (ev: MouseEvent) => {
    const target =
      ev.target && typeof (ev.target as Element).closest === "function"
        ? (ev.target as Element)
        : null;
    if (!target) {
      return;
    }

    if (target.closest('[data-action="fire-ipc-probe"]')) {
      const cmdSel = wrap.querySelector(
        "#studio-probe-cmd",
      ) as HTMLSelectElement | null;
      const hexInp = wrap.querySelector(
        "#studio-probe-hex",
      ) as HTMLInputElement | null;
      const cmd = cmdSel?.value ?? "inspect_health";
      const hex = hexInp?.value?.trim() ?? "";
      void store.executeRawIpcProbe(cmd, hex);
      return;
    }

    if (target.closest('[data-action="save-ipc-config"]')) {
      const socketPath =
        (
          wrap.querySelector("#cfg-auth-socket") as HTMLInputElement | null
        )?.value?.trim() || null;
      const principalHex =
        (
          wrap.querySelector("#cfg-principal") as HTMLInputElement | null
        )?.value?.trim() || null;
      const keyFile =
        (
          wrap.querySelector("#cfg-keyfile") as HTMLInputElement | null
        )?.value?.trim() || null;
      const cliSocket =
        (
          wrap.querySelector("#cfg-cli-socket") as HTMLInputElement | null
        )?.value?.trim() || null;
      void store.saveConnectionConfig({
        socketPath,
        principalHex,
        keyFile,
        cliSocket,
      });
    }
  });

  return wrap;
}
