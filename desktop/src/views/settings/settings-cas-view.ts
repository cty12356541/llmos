// ==========================================================================
// NLOS Stage D — 4-Layer SettingsDomain CAS + Watch Synchronization
// (`desktop/src/views/settings/settings-cas-view.ts`)
//
// Covers:
// - `[DUI-SETTINGS-001]` 4-layer (`system / user / workspace / application`)
//   CAS (`expected_domain_revision`) + `watch_settings_domain` live sync
// ==========================================================================

import {
  type DisclosureLevel,
  type WorkstationLocale,
} from "../../ui/status-indicator";

export type SettingsDomainLayer =
  | "system"
  | "user"
  | "workspace"
  | "application";

export const ALL_SETTINGS_LAYERS: readonly SettingsDomainLayer[] = [
  "system",
  "user",
  "workspace",
  "application",
] as const;

export type WorkstationThemeMode =
  | "graphite-workstation"
  | "paper-workstation"
  | "system";

export interface SettingsLayerEntry {
  readonly key: string;
  readonly valueJson: string;
  readonly layer: SettingsDomainLayer;
  readonly entryRevision: number;
  readonly lockedBySystemPolicy: boolean;
  readonly labels: Readonly<Record<WorkstationLocale, string>>;
}

export interface EffectiveSettingProjection {
  readonly key: string;
  readonly effectiveValueJson: string;
  readonly winningLayer: SettingsDomainLayer;
  readonly winningDomainRevision: number;
  readonly definedInLayers: readonly SettingsDomainLayer[];
  readonly lockedBySystemPolicy: boolean;
  readonly labels: Readonly<Record<WorkstationLocale, string>>;
}

export interface SettingsWatchEvent {
  readonly watchSeq: number;
  readonly layer: SettingsDomainLayer;
  readonly key: string;
  readonly previousValueJson: string | null;
  readonly newValueJson: string;
  readonly newDomainRevision: number;
  readonly effectiveTheme: WorkstationThemeMode;
}

export interface SettingsCasMutationResult {
  readonly ok: boolean;
  readonly errorCode?: "CAS_CONFLICT" | "SYSTEM_POLICY_LOCKED" | "INVALID_KEY";
  readonly expectedRevision: number;
  readonly actualRevision: number;
  readonly watchEvent?: SettingsWatchEvent;
  readonly message: Readonly<Record<WorkstationLocale, string>>;
}

export type SettingsWatchListener = (event: SettingsWatchEvent) => void;

export interface SettingsCasStore {
  getDomainRevision(layer: SettingsDomainLayer): number;
  getLayerEntries(layer: SettingsDomainLayer): readonly SettingsLayerEntry[];
  getEffectiveSettings(): readonly EffectiveSettingProjection[];
  getActiveTheme(): WorkstationThemeMode;
  getLastConflict(): SettingsCasMutationResult | null;
  clearConflict(): void;
  updateSettingWithCas(input: {
    readonly layer: SettingsDomainLayer;
    readonly key: string;
    readonly valueJson: string;
    readonly expectedDomainRevision: number;
    readonly labels?: Readonly<Record<WorkstationLocale, string>>;
  }): SettingsCasMutationResult;
  watchSettingsDomain(listener: SettingsWatchListener): () => void;
  getWatchEventHistory(): readonly SettingsWatchEvent[];
}

const LAYER_LABELS: Readonly<
  Record<SettingsDomainLayer, Readonly<Record<WorkstationLocale, string>>>
> = {
  system: { "zh-CN": "系统基线层", "en-US": "System Baseline" },
  user: { "zh-CN": "用户偏好层", "en-US": "User Profile" },
  workspace: { "zh-CN": "工作区策略层", "en-US": "Workspace Policy" },
  application: { "zh-CN": "应用专属层", "en-US": "Application Override" },
};

function escapeHtml(raw: string): string {
  return raw
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

export function createSeededSettingsCasStore(): SettingsCasStore {
  const revisions = new Map<SettingsDomainLayer, number>([
    ["system", 10],
    ["user", 20],
    ["workspace", 30],
    ["application", 40],
  ]);

  const entries = new Map<string, SettingsLayerEntry>();
  const makeCompoundKey = (layer: SettingsDomainLayer, key: string): string =>
    `${layer}::${key}`;

  const seed = (
    layer: SettingsDomainLayer,
    key: string,
    valueJson: string,
    locked: boolean,
    zh: string,
    en: string,
  ): void => {
    entries.set(makeCompoundKey(layer, key), {
      key,
      valueJson,
      layer,
      entryRevision: revisions.get(layer) ?? 1,
      lockedBySystemPolicy: locked,
      labels: { "zh-CN": zh, "en-US": en },
    });
  };

  seed(
    "system",
    "appearance.theme",
    "\"graphite-workstation\"",
    false,
    "工作站色彩主题",
    "Workstation Color Theme",
  );
  seed(
    "system",
    "security.mandatory_biometric_confirm",
    "true",
    true,
    "高危操作强制硬件或生物核身",
    "Mandatory Biometric Verification for Destructive Actions",
  );
  seed(
    "system",
    "control.background_concurrency_limit",
    "8",
    false,
    "后台任务最大并发上限",
    "Background Task Concurrency Limit",
  );
  seed(
    "user",
    "appearance.theme",
    "\"graphite-workstation\"",
    false,
    "工作站色彩主题",
    "Workstation Color Theme",
  );
  seed(
    "user",
    "control.background_concurrency_limit",
    "12",
    false,
    "后台任务最大并发上限",
    "Background Task Concurrency Limit",
  );
  seed(
    "workspace",
    "control.background_concurrency_limit",
    "16",
    false,
    "后台任务最大并发上限",
    "Background Task Concurrency Limit",
  );
  seed(
    "application",
    "analyzer.max_ast_worker_threads",
    "4",
    false,
    "分析器最大工作线程数",
    "Analyzer Max Worker Threads",
  );

  const listeners = new Set<SettingsWatchListener>();
  const watchHistory: SettingsWatchEvent[] = [];
  let watchSeq = 0;
  let lastConflict: SettingsCasMutationResult | null = null;

  const getEffectiveSettings = (): readonly EffectiveSettingProjection[] => {
    const allKeys = new Set<string>();
    for (const item of entries.values()) {
      allKeys.add(item.key);
    }

    const out: EffectiveSettingProjection[] = [];
    for (const key of Array.from(allKeys).sort()) {
      const definedInLayers: SettingsDomainLayer[] = [];
      let winning: SettingsLayerEntry | null = null;
      let lockedBySystem = false;

      for (const layer of ALL_SETTINGS_LAYERS) {
        const found = entries.get(makeCompoundKey(layer, key));
        if (found) {
          definedInLayers.push(layer);
          if (layer === "system" && found.lockedBySystemPolicy) {
            lockedBySystem = true;
          }
          if (!lockedBySystem || layer === "system") {
            winning = found;
          }
        }
      }

      if (winning !== null) {
        out.push({
          key,
          effectiveValueJson: winning.valueJson,
          winningLayer: winning.layer,
          winningDomainRevision: revisions.get(winning.layer) ?? 1,
          definedInLayers,
          lockedBySystemPolicy: lockedBySystem,
          labels: winning.labels,
        });
      }
    }
    return out;
  };

  const getActiveTheme = (): WorkstationThemeMode => {
    const eff = getEffectiveSettings().find(
      (s) => s.key === "appearance.theme",
    );
    const raw = eff?.effectiveValueJson.replace(/^"|"$/g, "") ?? "graphite-workstation";
    if (
      raw === "graphite-workstation" ||
      raw === "paper-workstation" ||
      raw === "system"
    ) {
      return raw;
    }
    return "graphite-workstation";
  };

  return {
    getDomainRevision: (layer) => revisions.get(layer) ?? 1,
    getLayerEntries: (layer) =>
      Array.from(entries.values()).filter((e) => e.layer === layer),
    getEffectiveSettings,
    getActiveTheme,
    getLastConflict: () => lastConflict,
    clearConflict: () => {
      lastConflict = null;
    },
    updateSettingWithCas: (input) => {
      const currentRev = revisions.get(input.layer) ?? 1;
      if (!input.key || input.key.trim().length === 0) {
        return {
          ok: false,
          errorCode: "INVALID_KEY",
          expectedRevision: input.expectedDomainRevision,
          actualRevision: currentRev,
          message: {
            "zh-CN": "配置项名称不能为空",
            "en-US": "Setting key must not be empty",
          },
        };
      }

      if (input.expectedDomainRevision !== currentRev) {
        const conflict: SettingsCasMutationResult = {
          ok: false,
          errorCode: "CAS_CONFLICT",
          expectedRevision: input.expectedDomainRevision,
          actualRevision: currentRev,
          message: {
            "zh-CN": `配置版本冲突：当前层版本已更新为 v${currentRev}（提交基准为 v${input.expectedDomainRevision}），请刷新后重新应用`,
            "en-US": `Version conflict: domain is at v${currentRev} (expected v${input.expectedDomainRevision}); reload before applying`,
          },
        };
        lastConflict = conflict;
        return conflict;
      }

      if (input.layer !== "system") {
        const sysEntry = entries.get(makeCompoundKey("system", input.key));
        if (sysEntry?.lockedBySystemPolicy) {
          return {
            ok: false,
            errorCode: "SYSTEM_POLICY_LOCKED",
            expectedRevision: input.expectedDomainRevision,
            actualRevision: currentRev,
            message: {
              "zh-CN": "该配置项受系统安全基线强制锁定，不可在下级层覆盖",
              "en-US":
                "This setting is locked by system security policy and cannot be overridden",
            },
          };
        }
      }

      const nextRev = currentRev + 1;
      revisions.set(input.layer, nextRev);
      lastConflict = null;

      const cKey = makeCompoundKey(input.layer, input.key);
      const prev = entries.get(cKey) ?? null;
      const fallbackLabels =
        prev?.labels ??
        entries.get(makeCompoundKey("system", input.key))?.labels ??
        input.labels ?? {
          "zh-CN": "自定义配置项",
          "en-US": "Custom Setting",
        };

      entries.set(cKey, {
        key: input.key,
        valueJson: input.valueJson,
        layer: input.layer,
        entryRevision: nextRev,
        lockedBySystemPolicy:
          input.layer === "system" ? (prev?.lockedBySystemPolicy ?? false) : false,
        labels: fallbackLabels,
      });

      watchSeq += 1;
      const watchEvent: SettingsWatchEvent = {
        watchSeq,
        layer: input.layer,
        key: input.key,
        previousValueJson: prev?.valueJson ?? null,
        newValueJson: input.valueJson,
        newDomainRevision: nextRev,
        effectiveTheme: getActiveTheme(),
      };
      watchHistory.push(watchEvent);
      for (const listener of listeners) {
        listener(watchEvent);
      }

      return {
        ok: true,
        expectedRevision: input.expectedDomainRevision,
        actualRevision: nextRev,
        watchEvent,
        message: {
          "zh-CN": `配置已同步并升级至版本 v${nextRev}`,
          "en-US": `Setting synchronized at revision v${nextRev}`,
        },
      };
    },
    watchSettingsDomain: (listener) => {
      listeners.add(listener);
      return () => {
        listeners.delete(listener);
      };
    },
    getWatchEventHistory: () => watchHistory,
  };
}

/**
 * Renders the 4-layer `SettingsDomain` CAS + effective inheritance table HTML.
 */
export function renderSettingsCasViewHtml(
  store: SettingsCasStore,
  options?: {
    readonly activeLayer?: SettingsDomainLayer;
    readonly locale?: WorkstationLocale;
    readonly disclosure?: DisclosureLevel;
  },
): string {
  const activeLayer: SettingsDomainLayer = options?.activeLayer ?? "user";
  const locale: WorkstationLocale = options?.locale ?? "zh-CN";
  const disclosure: DisclosureLevel = options?.disclosure ?? "L1";
  const rev = store.getDomainRevision(activeLayer);
  const conflict = store.getLastConflict();

  const tabsHtml = ALL_SETTINGS_LAYERS.map((layer) => {
    const layerRev = store.getDomainRevision(layer);
    const label = LAYER_LABELS[layer][locale];
    const activeClass =
      layer === activeLayer ? " nlos-settings-tab-btn--active" : "";
    const revTag =
      disclosure === "L3"
        ? ` [domain=${layer} · rev=${layerRev}]`
        : ` (v${layerRev})`;
    return `<button type="button" class="nlos-settings-tab-btn${activeClass}" data-settings-layer="${escapeHtml(
      layer,
    )}" data-domain-revision="${layerRev}">${escapeHtml(
      `${label}${revTag}`,
    )}</button>`;
  }).join("");

  const conflictBannerHtml =
    conflict !== null
      ? `<div class="nlos-settings-cas-banner" role="alert" data-error-code="${escapeHtml(
          conflict.errorCode ?? "CAS_CONFLICT",
        )}">${escapeHtml(conflict.message[locale])}</div>`
      : "";

  const rowsHtml = store
    .getEffectiveSettings()
    .map((eff) => {
      const winnerLabel = LAYER_LABELS[eff.winningLayer][locale];
      const provenanceText =
        disclosure === "L3"
          ? `${winnerLabel} [winning_layer=${eff.winningLayer} · expected_domain_revision=${eff.winningDomainRevision} · key=${eff.key}]`
          : locale === "zh-CN"
            ? `生效层：${winnerLabel} (v${eff.winningDomainRevision})`
            : `Effective from: ${winnerLabel} (v${eff.winningDomainRevision})`;

      const policyLockBadge = eff.lockedBySystemPolicy
        ? `<span class="nlos-settings-badge" data-policy-locked="true">${escapeHtml(
            locale === "zh-CN" ? "系统安全基线锁定" : "Locked by System Policy",
          )}</span>`
        : "";

      return (
        `<div class="nlos-settings-row" data-setting-key="${escapeHtml(
          eff.key,
        )}" data-winning-layer="${escapeHtml(eff.winningLayer)}">` +
        `<div class="nlos-settings-row__label"><strong>${escapeHtml(
          eff.labels[locale],
        )}</strong></div>` +
        `<div class="nlos-settings-row__value"><code>${escapeHtml(
          eff.effectiveValueJson,
        )}</code></div>` +
        `<div class="nlos-settings-row__provenance">${escapeHtml(
          provenanceText,
        )}</div>` +
        `<div class="nlos-settings-row__actions">${policyLockBadge}</div>` +
        `</div>`
      );
    })
    .join("");

  return (
    `<section class="nlos-settings-cas-section" data-active-layer="${escapeHtml(
      activeLayer,
    )}" data-domain-revision="${rev}" data-active-theme="${escapeHtml(
      store.getActiveTheme(),
    )}">` +
    `<div class="nlos-settings-layer-tabs" role="tablist">${tabsHtml}</div>` +
    conflictBannerHtml +
    `<div class="nlos-settings-table">${rowsHtml}</div>` +
    `</section>`
  );
}
