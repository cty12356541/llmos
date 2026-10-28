// Workstation Bilingual Localization Engine ([TUI-EXT-001], ANTI-AI-05, ANTI-AI-07).
// Provides atomic runtime locale switching (zh-CN <-> en-US), parameter interpolation,
// fallback-free key parity verification, zero-emoji verification, and L0/L1/L2 zero-jargon auditing.

import {
  LOCALE_BUNDLE_VERSION,
  LOCALE_SCHEMA_REVISION,
  ZH_CN_BUNDLE,
  type CanonicalTaskStateKey,
  type ControlCenterDomainKey,
  type ExtendedWorkstationStateKey,
  type LocaleResourceBundle,
  type SupportedLocale,
} from "./locales/zh-CN";
import { EN_US_BUNDLE } from "./locales/en-US";

export {
  EN_US_BUNDLE,
  LOCALE_BUNDLE_VERSION,
  LOCALE_SCHEMA_REVISION,
  ZH_CN_BUNDLE,
  type CanonicalTaskStateKey,
  type ControlCenterDomainKey,
  type ExtendedWorkstationStateKey,
  type LocaleResourceBundle,
  type SupportedLocale,
};

export const SUPPORTED_LOCALES: readonly SupportedLocale[] = ["zh-CN", "en-US"];

export const CANONICAL_16_TASK_STATES: readonly CanonicalTaskStateKey[] = [
  "PROPOSED",
  "WAITING_CONSENT",
  "AUTHORIZED",
  "RUNNING",
  "WAITING_INPUT",
  "WAITING_RESOURCE",
  "WAITING_NETWORK",
  "SUSPENDED",
  "RECOVERING",
  "CANCELLING",
  "COMPLETING",
  "COMPLETED",
  "PARTIAL",
  "DISPUTED",
  "FAILED",
  "CANCELLED",
];

export const ALL_24_WORKSTATION_TASK_STATES: readonly ExtendedWorkstationStateKey[] = [
  ...CANONICAL_16_TASK_STATES,
  "DRAFT",
  "QUARANTINED",
  "DECLARED",
  "BLOCKED_DEPENDENCY",
  "ELIGIBLE",
  "ACTIVE",
  "SKIPPED",
  "SUPERSEDED",
];

export const ALL_12_CONTROL_CENTER_DOMAINS: readonly ControlCenterDomainKey[] = [
  "system-device",
  "workspace",
  "application",
  "task",
  "task-group",
  "task-node",
  "task-attempt",
  "execution-unit",
  "resource-group",
  "topic-channel",
  "operation-device",
  "cell-distributed-lease",
];

/**
 * Forbidden internal kernel jargon tokens in L0/L1/L2 user-facing strings (ANTI-AI-07, [ROAD-D-001]).
 */
export const FORBIDDEN_L0_L1_L2_JARGON_TOKENS: readonly string[] = [
  "Z0",
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
  "Fiber",
  "Cell",
  "receipt_hex",
  "control_command_id",
];

const BUNDLES_BY_LOCALE: Readonly<Record<SupportedLocale, LocaleResourceBundle>> = {
  "zh-CN": ZH_CN_BUNDLE,
  "en-US": EN_US_BUNDLE,
};

export function getLocaleBundle(locale: SupportedLocale): LocaleResourceBundle {
  return BUNDLES_BY_LOCALE[locale];
}

export interface FlattenedLocaleEntry {
  readonly keyPath: string;
  readonly value: string;
  readonly isL3DiagnosticOnly: boolean;
}

function collectLeafEntries(
  obj: unknown,
  prefix = "",
  inL3Section = false,
  out: FlattenedLocaleEntry[] = [],
): readonly FlattenedLocaleEntry[] {
  if (typeof obj === "string") {
    out.push({
      keyPath: prefix,
      value: obj,
      isL3DiagnosticOnly: inL3Section,
    });
    return out;
  }
  if (typeof obj !== "object" || obj === null) {
    return out;
  }
  for (const [key, val] of Object.entries(obj as Record<string, unknown>)) {
    if (key === "locale" || key === "bundleVersion" || key === "schemaRevision") {
      continue;
    }
    const nextPrefix = prefix.length > 0 ? `${prefix}.${key}` : key;
    const nextInL3 = inL3Section || key === "l3ForensicDag";
    if (typeof val === "string") {
      out.push({
        keyPath: nextPrefix,
        value: val,
        isL3DiagnosticOnly: nextInL3,
      });
    } else if (typeof val === "object" && val !== null) {
      collectLeafEntries(val, nextPrefix, nextInL3, out);
    }
  }
  return out;
}

export function flattenLocaleBundle(
  bundle: LocaleResourceBundle,
): readonly FlattenedLocaleEntry[] {
  return collectLeafEntries(bundle);
}

export interface LocaleParityAuditReport {
  readonly passed: boolean;
  readonly bundleVersion: string;
  readonly schemaRevision: number;
  readonly zhKeyCount: number;
  readonly enKeyCount: number;
  readonly missingInEnUs: readonly string[];
  readonly missingInZhCn: readonly string[];
  readonly emptyValueKeys: readonly string[];
}

/**
 * Fallback-free key parity auditor across zh-CN and en-US ([TUI-EXT-001]).
 */
export function verifyLocaleKeyParity(
  zhBundle: LocaleResourceBundle = ZH_CN_BUNDLE,
  enBundle: LocaleResourceBundle = EN_US_BUNDLE,
): LocaleParityAuditReport {
  const zhEntries = flattenLocaleBundle(zhBundle);
  const enEntries = flattenLocaleBundle(enBundle);

  const zhMap = new Map<string, string>();
  const enMap = new Map<string, string>();
  const emptyValueKeys: string[] = [];

  for (const item of zhEntries) {
    zhMap.set(item.keyPath, item.value);
    if (item.value.trim().length === 0) {
      emptyValueKeys.push(`zh-CN:${item.keyPath}`);
    }
  }
  for (const item of enEntries) {
    enMap.set(item.keyPath, item.value);
    if (item.value.trim().length === 0) {
      emptyValueKeys.push(`en-US:${item.keyPath}`);
    }
  }

  const missingInEnUs: string[] = [];
  const missingInZhCn: string[] = [];

  for (const key of zhMap.keys()) {
    if (!enMap.has(key)) {
      missingInEnUs.push(key);
    }
  }
  for (const key of enMap.keys()) {
    if (!zhMap.has(key)) {
      missingInZhCn.push(key);
    }
  }

  const versionMatch =
    zhBundle.bundleVersion === LOCALE_BUNDLE_VERSION &&
    enBundle.bundleVersion === LOCALE_BUNDLE_VERSION &&
    zhBundle.schemaRevision === LOCALE_SCHEMA_REVISION &&
    enBundle.schemaRevision === LOCALE_SCHEMA_REVISION;

  return {
    passed:
      versionMatch &&
      missingInEnUs.length === 0 &&
      missingInZhCn.length === 0 &&
      emptyValueKeys.length === 0,
    bundleVersion: zhBundle.bundleVersion,
    schemaRevision: zhBundle.schemaRevision,
    zhKeyCount: zhEntries.length,
    enKeyCount: enEntries.length,
    missingInEnUs,
    missingInZhCn,
    emptyValueKeys,
  };
}

export interface JargonViolationItem {
  readonly locale: SupportedLocale;
  readonly keyPath: string;
  readonly forbiddenToken: string;
  readonly offendingValue: string;
}

export interface LocaleZeroJargonAuditReport {
  readonly passed: boolean;
  readonly auditedL0ToL2EntryCount: number;
  readonly violations: readonly JargonViolationItem[];
}

function containsForbiddenJargonToken(text: string, token: string): boolean {
  if (/^[A-Za-z0-9_]+$/.test(token)) {
    const escaped = token.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
    const regex = new RegExp(`\\b${escaped}\\b`);
    return regex.test(text);
  }
  return text.includes(token);
}

/**
 * Audits all L0/L1/L2 user-facing translation strings across zh-CN and en-US
 * to enforce zero internal kernel jargon (ANTI-AI-07, [ROAD-D-001]).
 */
export function verifyLocaleZeroJargon(
  bundles: readonly LocaleResourceBundle[] = [ZH_CN_BUNDLE, EN_US_BUNDLE],
): LocaleZeroJargonAuditReport {
  const violations: JargonViolationItem[] = [];
  let auditedCount = 0;

  for (const bundle of bundles) {
    const entries = flattenLocaleBundle(bundle);
    for (const entry of entries) {
      if (entry.isL3DiagnosticOnly) {
        continue;
      }
      auditedCount += 1;
      for (const token of FORBIDDEN_L0_L1_L2_JARGON_TOKENS) {
        if (containsForbiddenJargonToken(entry.value, token)) {
          violations.push({
            locale: bundle.locale,
            keyPath: entry.keyPath,
            forbiddenToken: token,
            offendingValue: entry.value,
          });
        }
      }
    }
  }

  return {
    passed: violations.length === 0,
    auditedL0ToL2EntryCount: auditedCount,
    violations,
  };
}

const PICTOGRAPHIC_EMOJI_REGEX = /\p{Extended_Pictographic}/u;

export interface LocaleEmojiAuditReport {
  readonly passed: boolean;
  readonly auditedEntryCount: number;
  readonly offendingKeys: readonly string[];
}

/**
 * Verifies zero pictographic emoji characters across all locale bundles (ANTI-AI-05).
 */
export function verifyLocaleZeroEmoji(
  bundles: readonly LocaleResourceBundle[] = [ZH_CN_BUNDLE, EN_US_BUNDLE],
): LocaleEmojiAuditReport {
  const offendingKeys: string[] = [];
  let auditedEntryCount = 0;
  for (const bundle of bundles) {
    for (const entry of flattenLocaleBundle(bundle)) {
      auditedEntryCount += 1;
      if (PICTOGRAPHIC_EMOJI_REGEX.test(entry.value)) {
        offendingKeys.push(`${bundle.locale}:${entry.keyPath}`);
      }
    }
  }
  return {
    passed: offendingKeys.length === 0,
    auditedEntryCount,
    offendingKeys,
  };
}

export type InterpolationParams = Readonly<Record<string, string | number>>;

export function interpolateMessage(
  template: string,
  params?: InterpolationParams,
): string {
  if (!params) {
    return template;
  }
  return template.replace(/\{([a-zA-Z0-9_]+)\}/g, (fullMatch, key: string) => {
    if (Object.prototype.hasOwnProperty.call(params, key)) {
      return String(params[key]);
    }
    return fullMatch;
  });
}

export function formatTabularNumber(
  value: number,
  locale: SupportedLocale = "zh-CN",
  maximumFractionDigits = 0,
): string {
  return new Intl.NumberFormat(locale, {
    maximumFractionDigits,
  }).format(value);
}

export type LocaleChangeListener = (
  locale: SupportedLocale,
  bundle: LocaleResourceBundle,
) => void;

export class LocalizationController {
  private currentLocale: SupportedLocale;
  private switchGeneration = 1;
  private readonly listeners = new Set<LocaleChangeListener>();
  private readonly keyLookupByLocale: Readonly<
    Record<SupportedLocale, ReadonlyMap<string, FlattenedLocaleEntry>>
  >;

  constructor(initialLocale: SupportedLocale = "zh-CN") {
    this.currentLocale = initialLocale;
    const buildMap = (bundle: LocaleResourceBundle): ReadonlyMap<string, FlattenedLocaleEntry> => {
      const m = new Map<string, FlattenedLocaleEntry>();
      for (const entry of flattenLocaleBundle(bundle)) {
        m.set(entry.keyPath, entry);
      }
      return m;
    };
    this.keyLookupByLocale = {
      "zh-CN": buildMap(ZH_CN_BUNDLE),
      "en-US": buildMap(EN_US_BUNDLE),
    };
  }

  public getLocale(): SupportedLocale {
    return this.currentLocale;
  }

  public getSwitchGeneration(): number {
    return this.switchGeneration;
  }

  public getBundle(): LocaleResourceBundle {
    return BUNDLES_BY_LOCALE[this.currentLocale];
  }

  public switchLocale(nextLocale: SupportedLocale): LocaleResourceBundle {
    if (nextLocale !== "zh-CN" && nextLocale !== "en-US") {
      throw new Error(`Unsupported workstation locale: ${String(nextLocale)}`);
    }
    if (this.currentLocale !== nextLocale) {
      this.currentLocale = nextLocale;
      this.switchGeneration += 1;
      const bundle = this.getBundle();
      for (const fn of this.listeners) {
        fn(this.currentLocale, bundle);
      }
    }
    return this.getBundle();
  }

  public subscribe(listener: LocaleChangeListener): () => void {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  }

  public t(keyPath: string, params?: InterpolationParams): string {
    const map = this.keyLookupByLocale[this.currentLocale];
    const entry = map.get(keyPath);
    if (!entry) {
      throw new Error(
        `Missing translation key "${keyPath}" in locale "${this.currentLocale}" (fallback-free mode)`,
      );
    }
    return interpolateMessage(entry.value, params);
  }

  public formatTaskState(state: ExtendedWorkstationStateKey): string {
    return this.getBundle().taskStates[state];
  }

  public formatControlCenterDomain(domain: ControlCenterDomainKey): string {
    return this.getBundle().controlCenterDomains[domain];
  }
}

export function createLocalizationController(
  initialLocale: SupportedLocale = "zh-CN",
): LocalizationController {
  return new LocalizationController(initialLocale);
}
