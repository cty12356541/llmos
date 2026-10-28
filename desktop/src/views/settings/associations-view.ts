// ==========================================================================
// NLOS Stage D — User-Revocable ContentTypeAssociation & Anti-Hijack Manager
// (`desktop/src/views/settings/associations-view.ts`)
//
// Covers:
// - `[DUI-ASSOC-001]` Packages can only submit unapproved proposals; they can
//   NEVER silently hijack an existing default association.
// - User explicit binding (`(ApplicationId, InstallationId)`) + one-click revoke.
// ==========================================================================

import {
  type DisclosureLevel,
  type WorkstationLocale,
} from "../../ui/status-indicator";

export interface PackageAssociationProposal {
  readonly proposedApplicationId: string;
  readonly proposedInstallationId: string;
  readonly proposedLabels: Readonly<Record<WorkstationLocale, string>>;
}

export interface ContentTypeAssociationItem {
  readonly contentTypeOrUri: string;
  readonly descriptions: Readonly<Record<WorkstationLocale, string>>;
  readonly handlerApplicationId: string | null;
  readonly handlerInstallationId: string | null;
  readonly handlerLabels: Readonly<Record<WorkstationLocale, string>>;
  readonly userExplicitlyBound: boolean;
  readonly pendingProposal: PackageAssociationProposal | null;
  readonly revision: number;
}

export interface AssociationMutationResult {
  readonly ok: boolean;
  readonly errorCode?: "UNKNOWN_CONTENT_TYPE" | "CAS_CONFLICT";
  readonly item?: ContentTypeAssociationItem;
}

export interface ContentTypeAssociationStore {
  listAssociations(): readonly ContentTypeAssociationItem[];
  submitPackageProposal(
    contentTypeOrUri: string,
    proposal: PackageAssociationProposal,
  ): AssociationMutationResult;
  bindUserDefaultAssociation(input: {
    readonly contentTypeOrUri: string;
    readonly applicationId: string;
    readonly installationId: string;
    readonly handlerLabels: Readonly<Record<WorkstationLocale, string>>;
    readonly expectedRevision: number;
  }): AssociationMutationResult;
  revokeAssociationToSystemChooser(
    contentTypeOrUri: string,
    expectedRevision: number,
  ): AssociationMutationResult;
}

function escapeHtml(raw: string): string {
  return raw
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

export function createSeededAssociationStore(): ContentTypeAssociationStore {
  const items = new Map<string, ContentTypeAssociationItem>([
    [
      "application/pdf",
      {
        contentTypeOrUri: "application/pdf",
        descriptions: {
          "zh-CN": "便携式文档格式 (PDF)",
          "en-US": "Portable Document Format (PDF)",
        },
        handlerApplicationId: "app-doc-reader-01",
        handlerInstallationId: "inst-doc-reader-v2",
        handlerLabels: {
          "zh-CN": "工作站文档审阅器",
          "en-US": "Workstation Document Reviewer",
        },
        userExplicitlyBound: true,
        pendingProposal: {
          proposedApplicationId: "app-thirdparty-pdf-pro",
          proposedInstallationId: "inst-thirdparty-pdf-09",
          proposedLabels: {
            "zh-CN": "第三方 PDF 套件",
            "en-US": "Third-Party PDF Suite",
          },
        },
        revision: 1,
      },
    ],
    [
      "text/markdown",
      {
        contentTypeOrUri: "text/markdown",
        descriptions: {
          "zh-CN": "Markdown 技术文档",
          "en-US": "Markdown Technical Document",
        },
        handlerApplicationId: "app-spec-studio-01",
        handlerInstallationId: "inst-spec-studio-v4",
        handlerLabels: {
          "zh-CN": "架构规范编辑器",
          "en-US": "Architecture Spec Editor",
        },
        userExplicitlyBound: true,
        pendingProposal: null,
        revision: 1,
      },
    ],
    [
      "nlos-workspace://",
      {
        contentTypeOrUri: "nlos-workspace://",
        descriptions: {
          "zh-CN": "工作区深层链接协议",
          "en-US": "Workspace Deep-Link URI Scheme",
        },
        handlerApplicationId: "app-workstation-shell",
        handlerInstallationId: "inst-workstation-shell-v1",
        handlerLabels: {
          "zh-CN": "系统工作区宿主",
          "en-US": "System Workspace Host",
        },
        userExplicitlyBound: true,
        pendingProposal: null,
        revision: 1,
      },
    ],
  ]);

  return {
    listAssociations: () => Array.from(items.values()),
    submitPackageProposal: (contentTypeOrUri, proposal) => {
      const existing = items.get(contentTypeOrUri);
      if (!existing) {
        return { ok: false, errorCode: "UNKNOWN_CONTENT_TYPE" };
      }
      // Anti-hijack invariant ([DUI-ASSOC-001]):
      // handlerApplicationId and handlerInstallationId are NEVER modified by a package proposal!
      const updated: ContentTypeAssociationItem = {
        ...existing,
        pendingProposal: proposal,
        revision: existing.revision + 1,
      };
      items.set(contentTypeOrUri, updated);
      return { ok: true, item: updated };
    },
    bindUserDefaultAssociation: (input) => {
      const existing = items.get(input.contentTypeOrUri);
      if (!existing) {
        return { ok: false, errorCode: "UNKNOWN_CONTENT_TYPE" };
      }
      if (existing.revision !== input.expectedRevision) {
        return { ok: false, errorCode: "CAS_CONFLICT", item: existing };
      }
      const updated: ContentTypeAssociationItem = {
        ...existing,
        handlerApplicationId: input.applicationId,
        handlerInstallationId: input.installationId,
        handlerLabels: input.handlerLabels,
        userExplicitlyBound: true,
        pendingProposal: null,
        revision: existing.revision + 1,
      };
      items.set(input.contentTypeOrUri, updated);
      return { ok: true, item: updated };
    },
    revokeAssociationToSystemChooser: (contentTypeOrUri, expectedRevision) => {
      const existing = items.get(contentTypeOrUri);
      if (!existing) {
        return { ok: false, errorCode: "UNKNOWN_CONTENT_TYPE" };
      }
      if (existing.revision !== expectedRevision) {
        return { ok: false, errorCode: "CAS_CONFLICT", item: existing };
      }
      const updated: ContentTypeAssociationItem = {
        ...existing,
        handlerApplicationId: null,
        handlerInstallationId: null,
        handlerLabels: {
          "zh-CN": "每次询问（系统默认选择器）",
          "en-US": "Always Ask (System Chooser)",
        },
        userExplicitlyBound: false,
        revision: existing.revision + 1,
      };
      items.set(contentTypeOrUri, updated);
      return { ok: true, item: updated };
    },
  };
}

/**
 * Renders the user-revocable `ContentTypeAssociation` table HTML (`[DUI-ASSOC-001]`).
 */
export function renderAssociationsViewHtml(
  store: ContentTypeAssociationStore,
  options?: {
    readonly locale?: WorkstationLocale;
    readonly disclosure?: DisclosureLevel;
  },
): string {
  const locale: WorkstationLocale = options?.locale ?? "zh-CN";
  const disclosure: DisclosureLevel = options?.disclosure ?? "L1";

  const rowsHtml = store
    .listAssociations()
    .map((item) => {
      const handlerName = item.handlerLabels[locale];
      const l3BindingTag =
        disclosure === "L3" && item.handlerApplicationId !== null
          ? ` [ApplicationId=${item.handlerApplicationId} · InstallationId=${item.handlerInstallationId ?? "none"} · rev=${item.revision}]`
          : "";

      const proposalHtml =
        item.pendingProposal !== null
          ? `<div class="nlos-association-proposal-banner" data-pending-proposal="true">` +
            `<span>${escapeHtml(
              locale === "zh-CN"
                ? `待确认新应用提议：${item.pendingProposal.proposedLabels["zh-CN"]}（未获授权前不会修改当前默认关联）`
                : `Pending Proposal: ${item.pendingProposal.proposedLabels["en-US"]} (current default unchanged until approved)`,
            )}</span>` +
            `<button type="button" class="nlos-settings-btn" data-action="approve-proposal" data-content-type="${escapeHtml(
              item.contentTypeOrUri,
            )}">${escapeHtml(
              locale === "zh-CN" ? "设为默认" : "Approve Default",
            )}</button>` +
            `</div>`
          : "";

      const revokeBtnHtml = item.userExplicitlyBound
        ? `<button type="button" class="nlos-settings-btn nlos-settings-btn--danger" data-action="revoke-association" data-content-type="${escapeHtml(
            item.contentTypeOrUri,
          )}" data-revision="${item.revision}">${escapeHtml(
            locale === "zh-CN" ? "一键撤销默认关联" : "Revoke Default",
          )}</button>`
        : `<span class="nlos-settings-badge">${escapeHtml(
            locale === "zh-CN" ? "已重置为每次询问" : "Reset to Always Ask",
          )}</span>`;

      return (
        `<div class="nlos-association-row" data-content-type="${escapeHtml(
          item.contentTypeOrUri,
        )}" data-revision="${item.revision}">` +
        `<div class="nlos-association-row__type"><strong>${escapeHtml(
          item.contentTypeOrUri,
        )}</strong> · <span>${escapeHtml(item.descriptions[locale])}</span></div>` +
        `<div class="nlos-association-row__handler">${escapeHtml(
          `${handlerName}${l3BindingTag}`,
        )}</div>` +
        `<div class="nlos-association-row__proposal">${proposalHtml}</div>` +
        `<div class="nlos-association-row__actions">${revokeBtnHtml}</div>` +
        `</div>`
      );
    })
    .join("");

  return `<section class="nlos-associations-table" role="region" aria-label="${
    locale === "zh-CN"
      ? "文件类型与链接协议默认关联管理"
      : "File Type & URI Scheme Default Associations"
  }">${rowsHtml}</section>`;
}
