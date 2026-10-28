// Barrel exports and unified TrustedUiSuite coordinator for Stage D D-TRUSTED-UI (W71-D-TRUSTED-UI).
// Ready for W71-INT shell integration in main.ts and WorkstationCompositor.

import type { DisclosureLevel, WorkstationLocale } from "../ui/status-indicator";
import type { VerificationSealOptions } from "../ui/verification-seal";
import {
  ChooserGrantModalBroker,
  createChooserGrantBroker,
  renderChooserGrantModalHtml,
} from "./chooser-grant-modal";
import {
  ClipboardAndDragGuard,
  createClipboardAndDragGuard,
} from "./clipboard-drag-guard";
import {
  Z4SecurityDialogController,
  createZ4SecurityDialog,
  type LowerPlaneTrustedPosture,
  type TrustedSessionLifecycle,
} from "./z4-security-dialog";

export * from "./z4-security-dialog";
export * from "./chooser-grant-modal";
export * from "./clipboard-drag-guard";

export interface TrustedUiSuiteOptions {
  readonly initialSessionState?: TrustedSessionLifecycle;
  readonly initialSessionGeneration?: number;
  readonly initialSecuritySealHint?: string;
  readonly locale?: WorkstationLocale;
  readonly disclosure?: DisclosureLevel;
  readonly sealOptions?: VerificationSealOptions;
  readonly doc?: Document;
}

export interface TrustedUiSuite {
  readonly z4Dialog: Z4SecurityDialogController;
  readonly chooserBroker: ChooserGrantModalBroker;
  readonly clipboardDragGuard: ClipboardAndDragGuard;
  syncSession(
    state: TrustedSessionLifecycle,
    sessionGeneration: number,
    securitySealHint?: string,
  ): void;
  setLocaleAndDisclosure(
    locale: WorkstationLocale,
    disclosure: DisclosureLevel,
  ): void;
  isZ4PlaneActive(): boolean;
  getPublicPosture(): LowerPlaneTrustedPosture & {
    readonly activeChooserSessionIdHex: string | null;
  };
  renderActiveOverlayHtml(nowMonoMs?: number): string;
}

/** Creates a cohesive TCB Trusted UI Suite synchronizing Z4 consent, ChooserGrant, and taint clipboard/drag-drop. */
export function createTrustedUiSuite(
  options?: TrustedUiSuiteOptions,
): TrustedUiSuite {
  let locale: WorkstationLocale = options?.locale ?? "zh-CN";
  let disclosure: DisclosureLevel = options?.disclosure ?? "L2";

  const z4Dialog = createZ4SecurityDialog({
    initialSessionState: options?.initialSessionState,
    initialSessionGeneration: options?.initialSessionGeneration,
    initialSecuritySealHint: options?.initialSecuritySealHint,
    locale,
    disclosure,
    sealOptions: options?.sealOptions,
    doc: options?.doc,
  });

  const chooserBroker = createChooserGrantBroker({
    initialSessionState: options?.initialSessionState,
    initialSessionGeneration: options?.initialSessionGeneration,
    initialSecuritySealHint: options?.initialSecuritySealHint,
  });

  const clipboardDragGuard = createClipboardAndDragGuard(z4Dialog, {
    initialSessionState: options?.initialSessionState,
    initialSessionGeneration: options?.initialSessionGeneration,
  });

  return {
    z4Dialog,
    chooserBroker,
    clipboardDragGuard,
    syncSession(
      state: TrustedSessionLifecycle,
      sessionGeneration: number,
      securitySealHint?: string,
    ): void {
      z4Dialog.syncSession(state, sessionGeneration, securitySealHint);
      chooserBroker.syncSession(state, sessionGeneration, securitySealHint);
      clipboardDragGuard.syncSession(state, sessionGeneration);
    },
    setLocaleAndDisclosure(
      nextLocale: WorkstationLocale,
      nextDisclosure: DisclosureLevel,
    ): void {
      locale = nextLocale;
      disclosure = nextDisclosure;
      z4Dialog.setLocaleAndDisclosure(nextLocale, nextDisclosure);
    },
    isZ4PlaneActive(): boolean {
      const posture = z4Dialog.getPublicPosture();
      const chooser = chooserBroker.getActiveSession({
        kind: "tcb_trusted_broker",
      });
      return posture.z4ModalActive || chooser !== null;
    },
    getPublicPosture(): LowerPlaneTrustedPosture & {
      readonly activeChooserSessionIdHex: string | null;
    } {
      const base = z4Dialog.getPublicPosture();
      const chooser = chooserBroker.getActiveSession({
        kind: "tcb_trusted_broker",
      });
      return {
        ...base,
        z4ModalActive: base.z4ModalActive || chooser !== null,
        activeChooserSessionIdHex: chooser?.chooserSessionIdHex ?? null,
      };
    },
    renderActiveOverlayHtml(nowMonoMs?: number): string {
      const z4Html = z4Dialog.renderActiveHtml(nowMonoMs);
      if (z4Html.length > 0) {
        return z4Html;
      }
      const chooser = chooserBroker.getActiveSession({
        kind: "tcb_trusted_broker",
      });
      if (chooser !== null) {
        return renderChooserGrantModalHtml(chooser, {
          locale,
          disclosure,
          nowMonoMs,
        });
      }
      return "";
    },
  };
}
