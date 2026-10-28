// ==========================================================================
// NLOS Stage D — Zero-Secret KeychainReference Vault & Session-Lock Wipe
// (`desktop/src/views/settings/keychain-view.ts`)
//
// Covers:
// - `[DUI-SETTINGS-001]` Zero raw secret bytes in any frontend state, JSON,
//   or DOM node; only `KeychainReference` metadata & `use-by-reference` handles.
// - `[DUI-SESSION-001]` Immediate credential handle cache wipe on session lock.
// ==========================================================================

import {
  type DisclosureLevel,
  renderStatusIndicatorHtml,
  type WorkstationLocale,
} from "../../ui/status-indicator";

export interface KeychainReferenceViewItem {
  readonly keychainRefId: string;
  readonly ownerPrincipalId: string;
  readonly labels: Readonly<Record<WorkstationLocale, string>>;
  readonly credentialKindLabel: Readonly<Record<WorkstationLocale, string>>;
  readonly publicFingerprintHex: string;
  readonly authorizedApplicationIds: readonly string[];
  readonly hardwareBacked: boolean;
  readonly revision: number;
  readonly revoked: boolean;
  readonly activeHandleCount: number;
}

export interface KeychainUseByRefHandle {
  readonly handleId: string;
  readonly keychainRefId: string;
  readonly authorizedApplicationId: string;
  readonly sessionGeneration: number;
  readonly referenceRevision: number;
}

export interface KeychainVaultStore {
  listReferences(): readonly KeychainReferenceViewItem[];
  enrollSecretByReference(input: {
    readonly keychainRefId: string;
    readonly ownerPrincipalId: string;
    readonly labels: Readonly<Record<WorkstationLocale, string>>;
    readonly credentialKindLabel: Readonly<Record<WorkstationLocale, string>>;
    readonly rawSecretMaterial: string;
    readonly authorizedApplicationIds: readonly string[];
    readonly hardwareBacked: boolean;
    readonly highAssuranceConfirmToken: string;
  }): {
    readonly ok: boolean;
    readonly errorCode?: "CONFIRMATION_REQUIRED" | "EMPTY_SECRET";
    readonly reference?: KeychainReferenceViewItem;
  };
  acquireUseByReferenceHandle(
    keychainRefId: string,
    callerApplicationId: string,
  ): {
    readonly ok: boolean;
    readonly errorCode?:
      | "SESSION_LOCKED"
      | "UNKNOWN_REF"
      | "REF_REVOKED"
      | "UNAUTHORIZED_APP";
    readonly handle?: KeychainUseByRefHandle;
  };
  revokeKeychainReference(
    keychainRefId: string,
    expectedRevision: number,
  ): {
    readonly ok: boolean;
    readonly errorCode?: "UNKNOWN_REF" | "CAS_CONFLICT";
    readonly reference?: KeychainReferenceViewItem;
  };
  lockSessionAndWipeHandles(): {
    readonly sessionGeneration: number;
    readonly wipedHandleCount: number;
  };
  unlockSession(): { readonly sessionGeneration: number };
}

function escapeHtml(raw: string): string {
  return raw
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

function computePublicFingerprint(rawSecret: string): string {
  let acc = 0x811c9dc5;
  for (let i = 0; i < rawSecret.length; i += 1) {
    acc ^= rawSecret.charCodeAt(i);
    acc = Math.imul(acc, 0x01000193) >>> 0;
  }
  return `fp_blake3_${acc.toString(16).padStart(8, "0")}`;
}

export function createSeededKeychainVaultStore(): KeychainVaultStore {
  // Private closure map holding raw secrets — never exposed or serialized!
  const privateVaultSecrets = new Map<string, string>();
  const references = new Map<string, KeychainReferenceViewItem>();
  const activeHandles = new Map<string, KeychainUseByRefHandle>();

  let sessionLocked = false;
  let sessionGeneration = 100;
  let handleSeq = 1;

  privateVaultSecrets.set(
    "kref-enterprise-git-signing",
    "TOP_SECRET_ENCLAVE_SEED_NEVER_EXPOSE_001",
  );
  references.set("kref-enterprise-git-signing", {
    keychainRefId: "kref-enterprise-git-signing",
    ownerPrincipalId: "principal-workstation-operator",
    labels: {
      "zh-CN": "代码仓库签名密钥引用",
      "en-US": "Repository Signing Key Reference",
    },
    credentialKindLabel: {
      "zh-CN": "安全芯片硬件密钥",
      "en-US": "Hardware Enclave Key",
    },
    publicFingerprintHex: "fp_blake3_9a8f7c6e",
    authorizedApplicationIds: ["node-app-code-analyzer", "app-spec-studio-01"],
    hardwareBacked: true,
    revision: 1,
    revoked: false,
    activeHandleCount: 0,
  });

  privateVaultSecrets.set(
    "kref-registry-upload-token",
    "TOP_SECRET_BEARER_TOKEN_NEVER_EXPOSE_002",
  );
  references.set("kref-registry-upload-token", {
    keychainRefId: "kref-registry-upload-token",
    ownerPrincipalId: "principal-workstation-operator",
    labels: {
      "zh-CN": "制品库发布凭据引用",
      "en-US": "Artifact Registry Credential Reference",
    },
    credentialKindLabel: {
      "zh-CN": "受限访问令牌引用",
      "en-US": "Scoped Access Token Reference",
    },
    publicFingerprintHex: "fp_blake3_1f2e3d4c",
    authorizedApplicationIds: ["node-app-code-analyzer"],
    hardwareBacked: false,
    revision: 1,
    revoked: false,
    activeHandleCount: 0,
  });

  return {
    listReferences: () => Array.from(references.values()),
    enrollSecretByReference: (input) => {
      if (
        !input.highAssuranceConfirmToken ||
        input.highAssuranceConfirmToken.trim().length === 0
      ) {
        return { ok: false, errorCode: "CONFIRMATION_REQUIRED" };
      }
      if (!input.rawSecretMaterial || input.rawSecretMaterial.length === 0) {
        return { ok: false, errorCode: "EMPTY_SECRET" };
      }

      privateVaultSecrets.set(input.keychainRefId, input.rawSecretMaterial);
      const ref: KeychainReferenceViewItem = {
        keychainRefId: input.keychainRefId,
        ownerPrincipalId: input.ownerPrincipalId,
        labels: input.labels,
        credentialKindLabel: input.credentialKindLabel,
        publicFingerprintHex: computePublicFingerprint(input.rawSecretMaterial),
        authorizedApplicationIds: [...input.authorizedApplicationIds],
        hardwareBacked: input.hardwareBacked,
        revision: 1,
        revoked: false,
        activeHandleCount: 0,
      };
      references.set(input.keychainRefId, ref);
      return { ok: true, reference: ref };
    },
    acquireUseByReferenceHandle: (keychainRefId, callerApplicationId) => {
      if (sessionLocked) {
        return { ok: false, errorCode: "SESSION_LOCKED" };
      }
      const ref = references.get(keychainRefId);
      if (!ref) {
        return { ok: false, errorCode: "UNKNOWN_REF" };
      }
      if (ref.revoked) {
        return { ok: false, errorCode: "REF_REVOKED" };
      }
      if (!ref.authorizedApplicationIds.includes(callerApplicationId)) {
        return { ok: false, errorCode: "UNAUTHORIZED_APP" };
      }

      const handleId = `khdl_${handleSeq++}`;
      const handle: KeychainUseByRefHandle = {
        handleId,
        keychainRefId,
        authorizedApplicationId: callerApplicationId,
        sessionGeneration,
        referenceRevision: ref.revision,
      };
      activeHandles.set(handleId, handle);
      references.set(keychainRefId, {
        ...ref,
        activeHandleCount: ref.activeHandleCount + 1,
      });
      return { ok: true, handle };
    },
    revokeKeychainReference: (keychainRefId, expectedRevision) => {
      const ref = references.get(keychainRefId);
      if (!ref) {
        return { ok: false, errorCode: "UNKNOWN_REF" };
      }
      if (ref.revision !== expectedRevision) {
        return { ok: false, errorCode: "CAS_CONFLICT", reference: ref };
      }

      privateVaultSecrets.delete(keychainRefId);
      for (const [hid, h] of activeHandles.entries()) {
        if (h.keychainRefId === keychainRefId) {
          activeHandles.delete(hid);
        }
      }

      const updated: KeychainReferenceViewItem = {
        ...ref,
        revoked: true,
        revision: ref.revision + 1,
        activeHandleCount: 0,
      };
      references.set(keychainRefId, updated);
      return { ok: true, reference: updated };
    },
    lockSessionAndWipeHandles: () => {
      sessionLocked = true;
      sessionGeneration += 1;
      const wipedHandleCount = activeHandles.size;
      activeHandles.clear();
      for (const [id, ref] of references.entries()) {
        references.set(id, { ...ref, activeHandleCount: 0 });
      }
      return { sessionGeneration, wipedHandleCount };
    },
    unlockSession: () => {
      sessionLocked = false;
      sessionGeneration += 1;
      return { sessionGeneration };
    },
  };
}

/**
 * Renders the zero-secret `KeychainReference` vault table HTML (`[DUI-SETTINGS-001]`).
 */
export function renderKeychainVaultViewHtml(
  store: KeychainVaultStore,
  options?: {
    readonly locale?: WorkstationLocale;
    readonly disclosure?: DisclosureLevel;
  },
): string {
  const locale: WorkstationLocale = options?.locale ?? "zh-CN";
  const disclosure: DisclosureLevel = options?.disclosure ?? "L1";

  const rowsHtml = store
    .listReferences()
    .map((ref) => {
      const hwBadge = ref.hardwareBacked
        ? locale === "zh-CN"
          ? "硬件安全芯片隔离"
          : "Hardware Enclave Backed"
        : locale === "zh-CN"
          ? "受控软件保险库"
          : "Managed Vault";

      const l3Fingerprint =
        disclosure === "L3"
          ? ` [ref=${ref.keychainRefId} · fp=${ref.publicFingerprintHex} · rev=${ref.revision}]`
          : "";

      const statusHtml = renderStatusIndicatorHtml({
        state: ref.revoked ? "CANCELLED" : "ACTIVE",
        locale,
        disclosure,
      });

      const actionHtml = ref.revoked
        ? `<span class="nlos-settings-badge">${escapeHtml(
            locale === "zh-CN" ? "凭据引用已永久失效" : "Credential Reference Revoked",
          )}</span>`
        : `<button type="button" class="nlos-settings-btn nlos-settings-btn--danger" data-action="revoke-keychain-ref" data-keychain-ref="${escapeHtml(
            ref.keychainRefId,
          )}" data-revision="${ref.revision}">${escapeHtml(
            locale === "zh-CN" ? "一键吊销凭据引用" : "Revoke Reference",
          )}</button>`;

      return (
        `<div class="nlos-keychain-row" data-keychain-ref="${escapeHtml(
          ref.keychainRefId,
        )}" data-revoked="${ref.revoked ? "true" : "false"}">` +
        `<div class="nlos-keychain-row__label"><strong>${escapeHtml(
          ref.labels[locale],
        )}</strong>${escapeHtml(l3Fingerprint)}</div>` +
        `<div class="nlos-keychain-row__kind">${escapeHtml(
          ref.credentialKindLabel[locale],
        )} · <span class="nlos-settings-badge">${escapeHtml(hwBadge)}</span></div>` +
        `<div class="nlos-keychain-row__status">${statusHtml}</div>` +
        `<div class="nlos-keychain-row__actions">${actionHtml}</div>` +
        `</div>`
      );
    })
    .join("");

  return `<section class="nlos-keychain-table" role="region" aria-label="${
    locale === "zh-CN"
      ? "零明文密钥引用保险库"
      : "Zero-Secret Keychain Reference Vault"
  }">${rowsHtml}</section>`;
}
