// ==========================================================================
// NLOS Stage D — Unified Settings CAS, ContentTypeAssociation & Keychain
// Workstation Surface (`desktop/src/views/settings/index.ts`)
// ==========================================================================

import {
  type DisclosureLevel,
  type WorkstationLocale,
} from "../../ui/status-indicator";
import {
  type ContentTypeAssociationStore,
  createSeededAssociationStore,
  renderAssociationsViewHtml,
} from "./associations-view";
import {
  createSeededKeychainVaultStore,
  type KeychainVaultStore,
  renderKeychainVaultViewHtml,
} from "./keychain-view";
import {
  createSeededSettingsCasStore,
  renderSettingsCasViewHtml,
  type SettingsCasStore,
  type SettingsDomainLayer,
} from "./settings-cas-view";

export * from "./settings-cas-view";
export * from "./associations-view";
export * from "./keychain-view";

export interface WorkstationSettingsSuiteController {
  readonly settingsCas: SettingsCasStore;
  readonly associations: ContentTypeAssociationStore;
  readonly keychainVault: KeychainVaultStore;
  renderSuiteHtml(options?: {
    readonly activeLayer?: SettingsDomainLayer;
    readonly locale?: WorkstationLocale;
    readonly disclosure?: DisclosureLevel;
  }): string;
}

export function createWorkstationSettingsSuiteController(): WorkstationSettingsSuiteController {
  const settingsCas = createSeededSettingsCasStore();
  const associations = createSeededAssociationStore();
  const keychainVault = createSeededKeychainVaultStore();

  return {
    settingsCas,
    associations,
    keychainVault,
    renderSuiteHtml: (options) => {
      const casHtml = renderSettingsCasViewHtml(settingsCas, options);
      const assocHtml = renderAssociationsViewHtml(associations, options);
      const vaultHtml = renderKeychainVaultViewHtml(keychainVault, options);

      return (
        `<div class="nlos-settings-workstation">` +
        casHtml +
        assocHtml +
        vaultHtml +
        `</div>`
      );
    },
  };
}
