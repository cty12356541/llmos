// Multimodal 4px Solid Attachment Chips (Screenshot / Scoped ChooserGrant File / Content-Addressed Artifact)
// Authority:
//   - docs/design/06-架构设计总纲-v0.5.md §24.1 ([NL-COMPILE-001], [NL-BIND-001]),
//     §24.2 ([TUI-CHOOSER-001], [TUI-EXT-001]), §25.4 ([DUI-CHOOSER-001], [DUI-CLIP-001])
//   - docs/superpowers/specs/2026-10-26-stage-d-desktop-visual-and-ux-design.md §0.1, §2A, §3.3
// Invariants:
//   - 4px solid neutral chips (var(--nlos-radius-ctrl), zero blur, zero emojis).
//   - Local files MUST carry a valid 32-hex ChooserGrant + ScopedCapability handle;
//     raw host filesystem paths are stripped to basename and NEVER exposed unless
//     explicitHostPathAuthorized === true alongside a verified ChooserGrant.
//   - Content-addressed Artifact chips bind both 32-hex ArtifactId and non-zero 64-hex ContentDigest.
//   - Attachment taint flags propagate monotonically into the compiled ActionProposal.
//   - L1/L2 visible chip labels contain zero internal architecture jargon.

import {
  createIconElement,
  renderIconSvg,
  type DisclosureLevel,
  type IconName,
  type WorkstationLocale,
} from "../ui";

export const ATTACHMENT_CHIP_RADIUS_PX = 4 as const;

export type AttachmentKind = "screenshot" | "chooser-file" | "artifact";

export type TaintFlag =
  | "EXTERNAL_ORIGIN"
  | "CONFIDENTIAL_SECRET"
  | "CROSS_APPLICATION"
  | "UNVERIFIED_INPUT";

export type AttachmentSecurityErrorCode =
  | "MISSING_CHOOSER_GRANT"
  | "RAW_HOST_PATH_FORBIDDEN"
  | "INVALID_HEX_IDENTITY"
  | "INVALID_CONTENT_DIGEST"
  | "EMPTY_LABEL";

export class AttachmentSecurityError extends Error {
  public readonly code: AttachmentSecurityErrorCode;

  public constructor(code: AttachmentSecurityErrorCode, message: string) {
    super(message);
    this.name = "AttachmentSecurityError";
    this.code = code;
  }
}

export interface AttachmentChipModel {
  readonly attachmentId: string;
  readonly kind: AttachmentKind;
  readonly displayLabel: string;
  readonly mimeType: string;
  readonly byteLength: number;
  readonly chipBorderRadiusPx: typeof ATTACHMENT_CHIP_RADIUS_PX;
  readonly contentDigestHex: string;
  readonly chooserGrantIdHex: string | null;
  readonly scopedCapabilityIdHex: string | null;
  readonly artifactIdHex: string | null;
  readonly sourceApplicationName: string | null;
  readonly taintFlags: readonly TaintFlag[];
  readonly explicitHostPathAuthorized: boolean;
  readonly authorizedHostPath: string | null;
}

export interface ScreenshotAttachmentInput {
  readonly attachmentId?: string;
  readonly label?: string;
  readonly mimeType?: string;
  readonly byteLength: number;
  readonly contentDigestHex: string;
  readonly captureRegionSummary?: string;
  readonly sourceApplicationName?: string;
  readonly taintFlags?: readonly TaintFlag[];
}

export interface ChooserFileAttachmentInput {
  readonly attachmentId?: string;
  readonly fileNameOrPath: string;
  readonly mimeType?: string;
  readonly byteLength: number;
  readonly contentDigestHex: string;
  readonly chooserGrantIdHex: string;
  readonly scopedCapabilityIdHex: string;
  readonly explicitHostPathAuthorized?: boolean;
  readonly taintFlags?: readonly TaintFlag[];
}

export interface ArtifactAttachmentInput {
  readonly attachmentId?: string;
  readonly displayLabel: string;
  readonly mimeType?: string;
  readonly byteLength: number;
  readonly artifactIdHex: string;
  readonly contentDigestHex: string;
  readonly sourceApplicationName?: string;
  readonly taintFlags?: readonly TaintFlag[];
}

const HEX_32_RE = /^[0-9a-f]{32}$/i;
const HEX_64_RE = /^[0-9a-f]{64}$/i;
const ALL_ZERO_64 = "0".repeat(64);

const WINDOWS_DRIVE_RE = /^[a-z]:[\\/]/i;
const UNC_PATH_RE = /^\\\\[^\\]+\\[^\\]+/;
const FILE_URI_RE = /^file:\/\//i;
const POSIX_HOST_ROOT_RE = /^\/(?:Users|home|etc|var|opt|private|root|tmp|mnt|Volumes)\//;

export function looksLikeRawHostFilesystemPath(candidate: string): boolean {
  const trimmed = candidate.trim();
  return (
    WINDOWS_DRIVE_RE.test(trimmed) ||
    UNC_PATH_RE.test(trimmed) ||
    FILE_URI_RE.test(trimmed) ||
    POSIX_HOST_ROOT_RE.test(trimmed)
  );
}

export function extractSafeAttachmentBasename(rawNameOrPath: string): string {
  let cleaned = rawNameOrPath.trim();
  if (FILE_URI_RE.test(cleaned)) {
    cleaned = cleaned.replace(/^file:\/\/+/i, "");
  }
  const normalized = cleaned.replace(/\\/g, "/").replace(/\/+$/, "");
  const lastSlash = normalized.lastIndexOf("/");
  const base = lastSlash >= 0 ? normalized.slice(lastSlash + 1) : normalized;
  return base.trim();
}

function normalizeHex32(raw: string, fieldName: string): string {
  const trimmed = raw.trim().toLowerCase();
  if (!HEX_32_RE.test(trimmed) || /^0+$/.test(trimmed)) {
    throw new AttachmentSecurityError(
      "INVALID_HEX_IDENTITY",
      `${fieldName} must be a non-zero 32-character hex handle`,
    );
  }
  return trimmed;
}

function normalizeDigest64(raw: string): string {
  const trimmed = raw.trim().toLowerCase();
  if (!HEX_64_RE.test(trimmed) || trimmed === ALL_ZERO_64) {
    throw new AttachmentSecurityError(
      "INVALID_CONTENT_DIGEST",
      "contentDigestHex must be a non-zero 64-character hex digest",
    );
  }
  return trimmed;
}

const TAINT_ORDER: readonly TaintFlag[] = [
  "EXTERNAL_ORIGIN",
  "CROSS_APPLICATION",
  "CONFIDENTIAL_SECRET",
  "UNVERIFIED_INPUT",
];

export function normalizeTaintFlags(
  flags?: readonly TaintFlag[],
): readonly TaintFlag[] {
  if (!flags || flags.length === 0) {
    return [];
  }
  const set = new Set<TaintFlag>(flags);
  return TAINT_ORDER.filter((flag) => set.has(flag));
}

export function mergeAttachmentTaintFlags(
  attachments: readonly AttachmentChipModel[],
): readonly TaintFlag[] {
  const merged = new Set<TaintFlag>();
  for (const item of attachments) {
    for (const flag of item.taintFlags) {
      merged.add(flag);
    }
  }
  return TAINT_ORDER.filter((flag) => merged.has(flag));
}

export function formatAttachmentBytes(
  byteLength: number,
  locale: WorkstationLocale = "zh-CN",
): string {
  const safeBytes = Number.isFinite(byteLength) && byteLength > 0 ? Math.round(byteLength) : 0;
  if (safeBytes < 1024) {
    return locale === "en-US" ? `${safeBytes} B` : `${safeBytes} B`;
  }
  if (safeBytes < 1024 * 1024) {
    const kb = (safeBytes / 1024).toFixed(1).replace(/\.0$/, "");
    return `${kb} KB`;
  }
  const mb = (safeBytes / (1024 * 1024)).toFixed(1).replace(/\.0$/, "");
  return `${mb} MB`;
}

export function createScreenshotAttachment(
  input: ScreenshotAttachmentInput,
): AttachmentChipModel {
  const digest = normalizeDigest64(input.contentDigestHex);
  const baseLabel = (input.label ?? input.captureRegionSummary ?? "Screen Capture").trim();
  const safeLabel = extractSafeAttachmentBasename(baseLabel) || "Screen Capture";
  const taintBase = new Set<TaintFlag>(input.taintFlags ?? []);
  if (input.sourceApplicationName && input.sourceApplicationName.trim().length > 0) {
    taintBase.add("CROSS_APPLICATION");
  }
  return {
    attachmentId: input.attachmentId?.trim() || `att-shot-${digest.slice(0, 12)}`,
    kind: "screenshot",
    displayLabel: safeLabel,
    mimeType: input.mimeType?.trim() || "image/png",
    byteLength: Math.max(0, Math.round(input.byteLength)),
    chipBorderRadiusPx: ATTACHMENT_CHIP_RADIUS_PX,
    contentDigestHex: digest,
    chooserGrantIdHex: null,
    scopedCapabilityIdHex: null,
    artifactIdHex: null,
    sourceApplicationName: input.sourceApplicationName?.trim() || null,
    taintFlags: normalizeTaintFlags([...taintBase]),
    explicitHostPathAuthorized: false,
    authorizedHostPath: null,
  };
}

export function createChooserFileAttachment(
  input: ChooserFileAttachmentInput,
): AttachmentChipModel {
  const rawGrant = input.chooserGrantIdHex?.trim() ?? "";
  const rawCap = input.scopedCapabilityIdHex?.trim() ?? "";
  if (rawGrant.length === 0 || rawCap.length === 0) {
    if (looksLikeRawHostFilesystemPath(input.fileNameOrPath)) {
      throw new AttachmentSecurityError(
        "RAW_HOST_PATH_FORBIDDEN",
        "Raw host filesystem path cannot be attached without a Trusted UI ChooserGrant",
      );
    }
    throw new AttachmentSecurityError(
      "MISSING_CHOOSER_GRANT",
      "Local file attachment requires both chooserGrantIdHex and scopedCapabilityIdHex",
    );
  }

  const grantHex = normalizeHex32(rawGrant, "chooserGrantIdHex");
  const capHex = normalizeHex32(rawCap, "scopedCapabilityIdHex");
  const digest = normalizeDigest64(input.contentDigestHex);

  const safeBasename = extractSafeAttachmentBasename(input.fileNameOrPath);
  if (safeBasename.length === 0) {
    throw new AttachmentSecurityError(
      "EMPTY_LABEL",
      "Chooser file attachment must resolve to a non-empty file name",
    );
  }

  const explicitAuthorized = input.explicitHostPathAuthorized === true;
  return {
    attachmentId: input.attachmentId?.trim() || `att-file-${grantHex.slice(0, 12)}`,
    kind: "chooser-file",
    displayLabel: safeBasename,
    mimeType: input.mimeType?.trim() || "application/octet-stream",
    byteLength: Math.max(0, Math.round(input.byteLength)),
    chipBorderRadiusPx: ATTACHMENT_CHIP_RADIUS_PX,
    contentDigestHex: digest,
    chooserGrantIdHex: grantHex,
    scopedCapabilityIdHex: capHex,
    artifactIdHex: null,
    sourceApplicationName: null,
    taintFlags: normalizeTaintFlags(input.taintFlags),
    explicitHostPathAuthorized: explicitAuthorized,
    authorizedHostPath: explicitAuthorized ? input.fileNameOrPath.trim() : null,
  };
}

export function createArtifactAttachment(
  input: ArtifactAttachmentInput,
): AttachmentChipModel {
  const artifactIdHex = normalizeHex32(input.artifactIdHex, "artifactIdHex");
  const digest = normalizeDigest64(input.contentDigestHex);
  const safeLabel = extractSafeAttachmentBasename(input.displayLabel);
  if (safeLabel.length === 0) {
    throw new AttachmentSecurityError(
      "EMPTY_LABEL",
      "Content-addressed output attachment must carry a non-empty label",
    );
  }
  const taintBase = new Set<TaintFlag>(input.taintFlags ?? []);
  if (input.sourceApplicationName && input.sourceApplicationName.trim().length > 0) {
    taintBase.add("CROSS_APPLICATION");
  }
  return {
    attachmentId: input.attachmentId?.trim() || `att-obj-${artifactIdHex.slice(0, 12)}`,
    kind: "artifact",
    displayLabel: safeLabel,
    mimeType: input.mimeType?.trim() || "application/octet-stream",
    byteLength: Math.max(0, Math.round(input.byteLength)),
    chipBorderRadiusPx: ATTACHMENT_CHIP_RADIUS_PX,
    contentDigestHex: digest,
    chooserGrantIdHex: null,
    scopedCapabilityIdHex: null,
    artifactIdHex,
    sourceApplicationName: input.sourceApplicationName?.trim() || null,
    taintFlags: normalizeTaintFlags([...taintBase]),
    explicitHostPathAuthorized: false,
    authorizedHostPath: null,
  };
}

function escapeHtml(raw: string): string {
  return raw
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

function iconForAttachmentKind(kind: AttachmentKind): IconName {
  switch (kind) {
    case "screenshot":
      return "hw-screen";
    case "chooser-file":
      return "shield-seal";
    case "artifact":
      return "copy";
  }
}

function plainKindLabel(kind: AttachmentKind, locale: WorkstationLocale): string {
  if (locale === "en-US") {
    switch (kind) {
      case "screenshot":
        return "Screen Capture";
      case "chooser-file":
        return "Authorized File";
      case "artifact":
        return "Saved Output";
    }
  }
  switch (kind) {
    case "screenshot":
      return "屏幕截图";
    case "chooser-file":
      return "已授权文件";
    case "artifact":
      return "已保存结果";
  }
}

export function formatPlainTaintSummary(
  flags: readonly TaintFlag[],
  locale: WorkstationLocale,
): string | null {
  if (flags.length === 0) {
    return null;
  }
  const labels: string[] = [];
  for (const flag of flags) {
    if (locale === "en-US") {
      switch (flag) {
        case "EXTERNAL_ORIGIN":
          labels.push("External source");
          break;
        case "CROSS_APPLICATION":
          labels.push("Cross-app data");
          break;
        case "CONFIDENTIAL_SECRET":
          labels.push("Sensitive content");
          break;
        case "UNVERIFIED_INPUT":
          labels.push("Unverified input");
          break;
      }
    } else {
      switch (flag) {
        case "EXTERNAL_ORIGIN":
          labels.push("外部来源");
          break;
        case "CROSS_APPLICATION":
          labels.push("跨应用数据");
          break;
        case "CONFIDENTIAL_SECRET":
          labels.push("敏感内容");
          break;
        case "UNVERIFIED_INPUT":
          labels.push("待核验输入");
          break;
      }
    }
  }
  return labels.join(" / ");
}

export interface AttachmentChipRenderOptions {
  readonly locale?: WorkstationLocale;
  readonly disclosure?: DisclosureLevel;
  readonly onRemoveAttachment?: (attachmentId: string) => void;
  readonly doc?: Document;
}

export function renderAttachmentChipsHtml(
  attachments: readonly AttachmentChipModel[],
  options?: AttachmentChipRenderOptions,
): string {
  if (attachments.length === 0) {
    return "";
  }
  const locale = options?.locale ?? "zh-CN";
  const disclosure = options?.disclosure ?? "L1";
  const removeTitle = locale === "en-US" ? "Remove attachment" : "移除附件";

  const itemsHtml = attachments
    .map((chip) => {
      const iconSvg = renderIconSvg(iconForAttachmentKind(chip.kind), {
        size: 14,
        strokeWidth: 1.5,
      });
      const kindText = plainKindLabel(chip.kind, locale);
      const sizeText = formatAttachmentBytes(chip.byteLength, locale);
      const plainTaint =
        disclosure === "L2" || disclosure === "L3"
          ? formatPlainTaintSummary(chip.taintFlags, locale)
          : chip.taintFlags.length > 0
            ? locale === "en-US"
              ? "Guarded"
              : "受控来源"
            : null;

      const l3DiagHtml =
        disclosure === "L3"
          ? `<span class="nlos-l3-only nlos-omnibar-chip-l3" data-nlos-l3-meta="attachment">` +
            `mime=${escapeHtml(chip.mimeType)} ` +
            `sha256=${escapeHtml(chip.contentDigestHex.slice(0, 16))}.. ` +
            (chip.chooserGrantIdHex
              ? `grant=${escapeHtml(chip.chooserGrantIdHex)} cap=${escapeHtml(chip.scopedCapabilityIdHex ?? "")} `
              : "") +
            (chip.artifactIdHex ? `artifact=${escapeHtml(chip.artifactIdHex)} ` : "") +
            (chip.explicitHostPathAuthorized && chip.authorizedHostPath
              ? `hostPath=${escapeHtml(chip.authorizedHostPath)} `
              : "") +
            (chip.taintFlags.length > 0
              ? `taint=[${escapeHtml(chip.taintFlags.join(","))}]`
              : "taint=[]") +
            `</span>`
          : "";

      return (
        `<div class="nlos-omnibar-chip" role="listitem" ` +
        `data-attachment-id="${escapeHtml(chip.attachmentId)}" ` +
        `data-attachment-kind="${escapeHtml(chip.kind)}" ` +
        `style="border-radius: 4px;">` +
        `<span class="nlos-omnibar-chip-icon" aria-hidden="true">${iconSvg}</span>` +
        `<span class="nlos-omnibar-chip-kind">${escapeHtml(kindText)}</span>` +
        `<span class="nlos-omnibar-chip-label">${escapeHtml(chip.displayLabel)}</span>` +
        `<span class="nlos-omnibar-chip-size">${escapeHtml(sizeText)}</span>` +
        (plainTaint
          ? `<span class="nlos-omnibar-chip-taint">${escapeHtml(plainTaint)}</span>`
          : "") +
        l3DiagHtml +
        `<button type="button" class="nlos-omnibar-chip-remove" ` +
        `data-remove-attachment="${escapeHtml(chip.attachmentId)}" ` +
        `aria-label="${escapeHtml(`${removeTitle}: ${chip.displayLabel}`)}">` +
        `${renderIconSvg("close", { size: 14, strokeWidth: 1.5 })}` +
        `</button>` +
        `</div>`
      );
    })
    .join("");

  return `<div class="nlos-omnibar-chip-strip" role="list">${itemsHtml}</div>`;
}

export function createAttachmentChipsElement(
  attachments: readonly AttachmentChipModel[],
  options?: AttachmentChipRenderOptions,
): HTMLElement {
  const doc = options?.doc ?? document;
  const locale = options?.locale ?? "zh-CN";
  const disclosure = options?.disclosure ?? "L1";
  const removeTitle = locale === "en-US" ? "Remove attachment" : "移除附件";

  const strip = doc.createElement("div");
  strip.className = "nlos-omnibar-chip-strip";
  strip.setAttribute("role", "list");

  for (const chip of attachments) {
    const item = doc.createElement("div");
    item.className = "nlos-omnibar-chip";
    item.setAttribute("role", "listitem");
    item.setAttribute("data-attachment-id", chip.attachmentId);
    item.setAttribute("data-attachment-kind", chip.kind);
    item.style.borderRadius = "4px";

    const iconWrap = doc.createElement("span");
    iconWrap.className = "nlos-omnibar-chip-icon";
    iconWrap.setAttribute("aria-hidden", "true");
    iconWrap.appendChild(
      createIconElement(iconForAttachmentKind(chip.kind), {
        size: 14,
        strokeWidth: 1.5,
        doc,
      }),
    );
    item.appendChild(iconWrap);

    const kindSpan = doc.createElement("span");
    kindSpan.className = "nlos-omnibar-chip-kind";
    kindSpan.textContent = plainKindLabel(chip.kind, locale);
    item.appendChild(kindSpan);

    const labelSpan = doc.createElement("span");
    labelSpan.className = "nlos-omnibar-chip-label";
    labelSpan.textContent = chip.displayLabel;
    item.appendChild(labelSpan);

    const sizeSpan = doc.createElement("span");
    sizeSpan.className = "nlos-omnibar-chip-size";
    sizeSpan.textContent = formatAttachmentBytes(chip.byteLength, locale);
    item.appendChild(sizeSpan);

    const plainTaint =
      disclosure === "L2" || disclosure === "L3"
        ? formatPlainTaintSummary(chip.taintFlags, locale)
        : chip.taintFlags.length > 0
          ? locale === "en-US"
            ? "Guarded"
            : "受控来源"
          : null;
    if (plainTaint) {
      const taintSpan = doc.createElement("span");
      taintSpan.className = "nlos-omnibar-chip-taint";
      taintSpan.textContent = plainTaint;
      item.appendChild(taintSpan);
    }

    const l3Meta = doc.createElement("span");
    l3Meta.className = "nlos-l3-only nlos-omnibar-chip-l3";
    l3Meta.setAttribute("data-nlos-l3-meta", "attachment");
    if (disclosure === "L3") {
      l3Meta.hidden = false;
      const parts: string[] = [
        `mime=${chip.mimeType}`,
        `sha256=${chip.contentDigestHex.slice(0, 16)}..`,
      ];
      if (chip.chooserGrantIdHex) {
        parts.push(`grant=${chip.chooserGrantIdHex}`);
        parts.push(`cap=${chip.scopedCapabilityIdHex ?? ""}`);
      }
      if (chip.artifactIdHex) {
        parts.push(`artifact=${chip.artifactIdHex}`);
      }
      if (chip.explicitHostPathAuthorized && chip.authorizedHostPath) {
        parts.push(`hostPath=${chip.authorizedHostPath}`);
      }
      parts.push(`taint=[${chip.taintFlags.join(",")}]`);
      l3Meta.textContent = parts.join(" ");
    } else {
      l3Meta.hidden = true;
      l3Meta.setAttribute("hidden", "");
    }
    item.appendChild(l3Meta);

    const removeBtn = doc.createElement("button");
    removeBtn.type = "button";
    removeBtn.className = "nlos-omnibar-chip-remove";
    removeBtn.setAttribute("data-remove-attachment", chip.attachmentId);
    removeBtn.setAttribute("aria-label", `${removeTitle}: ${chip.displayLabel}`);
    removeBtn.appendChild(
      createIconElement("close", { size: 14, strokeWidth: 1.5, doc }),
    );
    removeBtn.addEventListener("click", () => {
      options?.onRemoveAttachment?.(chip.attachmentId);
    });
    item.appendChild(removeBtn);

    strip.appendChild(item);
  }

  return strip;
}
