// Taint-Aware Cross-Application Clipboard & Drag-Drop Guard ([DUI-CLIP-001]).
// Authority: 06-Architecture Spec v0.5 §24.3, §25.4; ADR-0025 Decision 1.

import {
  TrustedUiRejectionError,
  Z4SecurityDialogController,
  deterministicDomainDigestHex,
  type ReversibilityClass,
  type TrustedSessionLifecycle,
  type TrustedUiInputOrigin,
  type TrustedUiInputSource,
} from "./z4-security-dialog";

export type TransferChannelKind = "CLIPBOARD" | "DRAG_DROP";

export type TaintFlag =
  | "PUBLIC"
  | "WORKSPACE_INTERNAL"
  | "SENSITIVE"
  | "CONFIDENTIAL"
  | "CREDENTIALS"
  | "UNTRUSTED_EXTERNAL";

export interface UserGestureToken {
  readonly gestureTokenIdHex: string;
  readonly actorApplicationIdHex: string;
  readonly channel: TransferChannelKind;
  readonly inputSource: TrustedUiInputSource;
  readonly sessionGeneration: number;
  readonly issuedAtMonoMs: number;
  readonly expiresAtMonoMs: number;
}

export interface TransferEnvelopeMetadata {
  readonly transferIdHex: string;
  readonly channel: TransferChannelKind;
  readonly contentType: string;
  readonly payloadDigestHex: string;
  readonly byteLength: number;
  readonly sourceApplicationIdHex: string;
  readonly sourceAppDisplayName: string;
  readonly taintFlags: readonly TaintFlag[];
  readonly sessionGeneration: number;
  readonly createdAtMonoMs: number;
  readonly expiresAtMonoMs: number;
  readonly userGestureTokenIdHex: string;
}

export type TransferReadDecision =
  | {
      readonly outcome: "RELEASED";
      readonly transferIdHex: string;
      readonly channel: TransferChannelKind;
      readonly contentType: string;
      readonly payloadDigestHex: string;
      readonly payloadText: string;
      readonly byteLength: number;
      readonly taintFlags: readonly TaintFlag[];
    }
  | {
      readonly outcome: "ESCALATED_TO_Z4_CONSENT";
      readonly transferIdHex: string;
      readonly challengeIdHex: string;
      readonly channel: TransferChannelKind;
      readonly sourceApplicationIdHex: string;
      readonly targetApplicationIdHex: string;
      readonly taintFlags: readonly TaintFlag[];
      readonly requiresHighContrastAck: boolean;
    };

export interface ClipboardDragAuditCounters {
  transfersPublished: number;
  rejectedMissingGestureReads: number;
  rejectedExpiredTransferReads: number;
  escalatedTaintedTransfers: number;
}

interface InternalGestureRecord {
  readonly token: UserGestureToken;
  consumed: boolean;
}

interface InternalTransferRecord {
  readonly metadata: TransferEnvelopeMetadata;
  readonly payloadText: string;
}

interface InternalPendingEscalation {
  readonly transferIdHex: string;
  readonly targetApplicationIdHex: string;
  readonly challengeIdHex: string;
  readonly sessionGeneration: number;
}

const TAINT_PLAIN_ZH: Readonly<Record<TaintFlag, string>> = {
  PUBLIC: "公开内容",
  WORKSPACE_INTERNAL: "工作区内部内容",
  SENSITIVE: "敏感业务内容",
  CONFIDENTIAL: "机密受控内容",
  CREDENTIALS: "账号密钥与凭据",
  UNTRUSTED_EXTERNAL: "外部未核验来源内容",
};

const TAINT_PLAIN_EN: Readonly<Record<TaintFlag, string>> = {
  PUBLIC: "Public content",
  WORKSPACE_INTERNAL: "Workspace internal content",
  SENSITIVE: "Sensitive business content",
  CONFIDENTIAL: "Confidential controlled content",
  CREDENTIALS: "Account keys and credentials",
  UNTRUSTED_EXTERNAL: "Untrusted external content",
};

export function taintRequiresCrossAppEscalation(flag: TaintFlag): boolean {
  return (
    flag === "SENSITIVE" ||
    flag === "CONFIDENTIAL" ||
    flag === "CREDENTIALS" ||
    flag === "UNTRUSTED_EXTERNAL"
  );
}

export function taintIsIrreversibleDisclosure(flag: TaintFlag): boolean {
  return flag === "CONFIDENTIAL" || flag === "CREDENTIALS";
}

/** Stateful TCB Clipboard & Drag-Drop Guard coordinated with Z4SecurityDialogController. */
export class ClipboardAndDragGuard {
  private sessionState: TrustedSessionLifecycle;
  private sessionGeneration: number;
  private readonly z4Dialog: Z4SecurityDialogController;
  private readonly gestures = new Map<string, InternalGestureRecord>();
  private readonly transfers = new Map<string, InternalTransferRecord>();
  private readonly pendingEscalations = new Map<string, InternalPendingEscalation>();
  private nextNonce = 0;

  public readonly audit: ClipboardDragAuditCounters = {
    transfersPublished: 0,
    rejectedMissingGestureReads: 0,
    rejectedExpiredTransferReads: 0,
    escalatedTaintedTransfers: 0,
  };

  public constructor(
    z4Dialog: Z4SecurityDialogController,
    options?: {
      readonly initialSessionState?: TrustedSessionLifecycle;
      readonly initialSessionGeneration?: number;
    },
  ) {
    this.z4Dialog = z4Dialog;
    this.sessionState = options?.initialSessionState ?? "ACTIVE";
    this.sessionGeneration = options?.initialSessionGeneration ?? 1;
  }

  public syncSession(
    nextState: TrustedSessionLifecycle,
    nextSessionGeneration: number,
  ): void {
    const invalidate =
      nextState !== "ACTIVE" || nextSessionGeneration !== this.sessionGeneration;
    this.sessionState = nextState;
    this.sessionGeneration = nextSessionGeneration;
    if (invalidate) {
      this.pendingEscalations.clear();
      for (const gesture of this.gestures.values()) {
        gesture.consumed = true;
      }
    }
  }

  public mintUserGestureToken(options: {
    readonly origin: TrustedUiInputOrigin;
    readonly source: TrustedUiInputSource;
    readonly actorApplicationIdHex: string;
    readonly channel: TransferChannelKind;
    readonly nowMonoMs: number;
    readonly ttlMs?: number;
  }): UserGestureToken {
    if (options.origin !== "trusted_shell") {
      this.audit.rejectedMissingGestureReads += 1;
      throw new TrustedUiRejectionError(
        "UNTRUSTED_INPUT_ORIGIN",
        "Untrusted surface cannot self-mint user_gesture_token",
      );
    }
    if (options.source === "AUTOMATION") {
      this.audit.rejectedMissingGestureReads += 1;
      throw new TrustedUiRejectionError(
        "AUTOMATION_GESTURE_FORBIDDEN",
        "Automation channel cannot mint human clipboard or drag-drop gesture tokens",
      );
    }
    if (this.sessionState !== "ACTIVE") {
      throw new TrustedUiRejectionError(
        "SESSION_NOT_ACTIVE_OR_STALE_GENERATION",
        `Cannot mint gesture token while session is ${this.sessionState}`,
      );
    }

    this.nextNonce += 1;
    const boundedTtl = Math.max(250, Math.min(options.ttlMs ?? 5_000, 30_000));
    const gestureTokenIdHex = deterministicDomainDigestHex(
      "llmos/stage-d/ts-gesture-token/v1",
      [
        String(this.nextNonce),
        options.actorApplicationIdHex,
        options.channel,
        String(this.sessionGeneration),
        String(options.nowMonoMs),
      ],
    ).slice(0, 32);

    const token: UserGestureToken = {
      gestureTokenIdHex,
      actorApplicationIdHex: options.actorApplicationIdHex,
      channel: options.channel,
      inputSource: options.source,
      sessionGeneration: this.sessionGeneration,
      issuedAtMonoMs: options.nowMonoMs,
      expiresAtMonoMs: options.nowMonoMs + boundedTtl,
    };
    this.gestures.set(gestureTokenIdHex, { token, consumed: false });
    return token;
  }

  public publishTransfer(options: {
    readonly channel: TransferChannelKind;
    readonly contentType: string;
    readonly payloadText: string;
    readonly sourceApplicationIdHex: string;
    readonly sourceAppDisplayName: string;
    readonly taintFlags: readonly TaintFlag[];
    readonly gestureTokenIdHex: string | null;
    readonly ttlMs: number;
    readonly nowMonoMs: number;
  }): TransferEnvelopeMetadata {
    if (this.sessionState !== "ACTIVE") {
      throw new TrustedUiRejectionError(
        "SESSION_NOT_ACTIVE_OR_STALE_GENERATION",
        "Cannot publish clipboard or drag-drop transfer while session is not ACTIVE",
      );
    }
    const consumedId = this.consumeGestureToken(
      options.gestureTokenIdHex,
      options.sourceApplicationIdHex,
      options.channel,
      options.nowMonoMs,
    );

    const uniqueTaints: TaintFlag[] =
      options.taintFlags.length > 0
        ? [...new Set(options.taintFlags)]
        : ["WORKSPACE_INTERNAL"];
    const byteLength = new TextEncoder().encode(options.payloadText).length;
    const payloadDigestHex = deterministicDomainDigestHex(
      "llmos/stage-d/ts-transfer-payload/v1",
      [options.payloadText],
    );

    this.nextNonce += 1;
    const transferIdHex = deterministicDomainDigestHex(
      "llmos/stage-d/ts-transfer-id/v1",
      [
        String(this.nextNonce),
        options.sourceApplicationIdHex,
        payloadDigestHex,
        String(options.nowMonoMs),
      ],
    ).slice(0, 32);

    const boundedTtl = Math.max(500, Math.min(options.ttlMs, 3_600_000));
    const metadata: TransferEnvelopeMetadata = {
      transferIdHex,
      channel: options.channel,
      contentType: options.contentType.trim(),
      payloadDigestHex,
      byteLength,
      sourceApplicationIdHex: options.sourceApplicationIdHex,
      sourceAppDisplayName: options.sourceAppDisplayName.trim(),
      taintFlags: uniqueTaints,
      sessionGeneration: this.sessionGeneration,
      createdAtMonoMs: options.nowMonoMs,
      expiresAtMonoMs: options.nowMonoMs + boundedTtl,
      userGestureTokenIdHex: consumedId,
    };

    this.transfers.set(transferIdHex, {
      metadata,
      payloadText: options.payloadText,
    });
    this.audit.transfersPublished += 1;
    return metadata;
  }

  public requestTransferRead(options: {
    readonly transferIdHex: string;
    readonly targetApplicationIdHex: string;
    readonly targetInstallationIdHex: string;
    readonly targetPrincipalIdHex: string;
    readonly targetAppDisplayName: string;
    readonly gestureTokenIdHex: string | null;
    readonly nowMonoMs: number;
  }): TransferReadDecision {
    if (this.sessionState !== "ACTIVE") {
      throw new TrustedUiRejectionError(
        "SESSION_NOT_ACTIVE_OR_STALE_GENERATION",
        "Cannot read clipboard or drag-drop transfer while session is not ACTIVE",
      );
    }
    const record = this.transfers.get(options.transferIdHex);
    if (!record) {
      throw new TrustedUiRejectionError(
        "TRANSFER_NOT_FOUND",
        `Transfer ${options.transferIdHex} not found`,
      );
    }

    this.consumeGestureToken(
      options.gestureTokenIdHex,
      options.targetApplicationIdHex,
      record.metadata.channel,
      options.nowMonoMs,
    );

    if (
      record.metadata.sessionGeneration !== this.sessionGeneration ||
      options.nowMonoMs >= record.metadata.expiresAtMonoMs
    ) {
      this.audit.rejectedExpiredTransferReads += 1;
      throw new TrustedUiRejectionError(
        "TRANSFER_EXPIRED",
        `Transfer ${options.transferIdHex} expired at ${record.metadata.expiresAtMonoMs}ms`,
      );
    }

    const isCrossApp =
      options.targetApplicationIdHex !== record.metadata.sourceApplicationIdHex;
    const requiresEscalation =
      isCrossApp &&
      record.metadata.taintFlags.some((f) => taintRequiresCrossAppEscalation(f));

    if (!requiresEscalation) {
      return {
        outcome: "RELEASED",
        transferIdHex: record.metadata.transferIdHex,
        channel: record.metadata.channel,
        contentType: record.metadata.contentType,
        payloadDigestHex: record.metadata.payloadDigestHex,
        payloadText: record.payloadText,
        byteLength: record.metadata.byteLength,
        taintFlags: record.metadata.taintFlags,
      };
    }

    const irreversible = record.metadata.taintFlags.some((f) =>
      taintIsIrreversibleDisclosure(f),
    );
    const revClass: ReversibilityClass = irreversible
      ? "IRREVERSIBLE"
      : "COMPENSATABLE_ROLLBACK";

    const taintSummaryZh = record.metadata.taintFlags
      .map((f) => TAINT_PLAIN_ZH[f])
      .join("、");
    const taintSummaryEn = record.metadata.taintFlags
      .map((f) => TAINT_PLAIN_EN[f])
      .join(", ");
    const channelZh =
      record.metadata.channel === "CLIPBOARD" ? "剪贴板粘贴" : "拖放传入";
    const channelEn =
      record.metadata.channel === "CLIPBOARD"
        ? "clipboard paste"
        : "drag-and-drop";

    const manifestDigestHex = deterministicDomainDigestHex(
      "llmos/stage-d/ts-transfer-manifest/v1",
      [options.targetApplicationIdHex, options.targetInstallationIdHex],
    );

    const z4Snap = this.z4Dialog.openChallenge(
      { kind: "tcb_trusted_broker" },
      {
        requestingApp: {
          displayName: options.targetAppDisplayName,
          publisherName: "本地已核验应用",
          signatureVerified: true,
          applicationIdHex: options.targetApplicationIdHex,
          installationIdHex: options.targetInstallationIdHex,
          principalIdHex: options.targetPrincipalIdHex,
          installationGeneration: 1,
          manifestDigestHex,
        },
        actionScope: {
          plainSummary: {
            "zh-CN": `允许「${options.targetAppDisplayName}」通过${channelZh}读取来自「${record.metadata.sourceAppDisplayName}」的内容（${taintSummaryZh}）`,
            "en-US": `Allow "${options.targetAppDisplayName}" to read ${channelEn} from "${record.metadata.sourceAppDisplayName}" (${taintSummaryEn})`,
          },
          capabilityRights: [
            `CROSS_APP_${record.metadata.channel}_READ`,
            `TAINT(${record.metadata.taintFlags.join(",")})`,
          ],
          controlScope: "CROSS_APPLICATION_TRANSFER",
          planDigestHex: record.metadata.payloadDigestHex,
        },
        resourceCeiling: {
          plainCeiling: {
            "zh-CN": `单次传入 ${record.metadata.byteLength} 字节（${record.metadata.contentType}），不授予后台持续读取`,
            "en-US": `One-time transfer of ${record.metadata.byteLength} bytes (${record.metadata.contentType}), zero background access`,
          },
          memoryCeilingMib: 1,
          tokenBudgetCeiling: 0,
          networkEgressAllowed: false,
          maxDurationMs: 5_000,
        },
        reversibilityAndRisk: {
          reversibilityClass: revClass,
          plainConsequence: {
            "zh-CN": irreversible
              ? "机密或账号凭据一旦传递给目标应用即无法收回，也无法撤销已读取的内容。"
              : "受控内容将进入目标应用，系统会记录此次跨应用流转审计记录。",
            "en-US": irreversible
              ? "Once confidential or credential bytes reach the target app, disclosure cannot be undone."
              : "Controlled content will enter the target app and be recorded in the audit log.",
          },
          multiTargetCount: 1,
          blastRadiusRows: [
            {
              symbol: "~",
              category: "reverified",
              scopeLabel: channelZh,
              targetLabel: options.targetAppDisplayName,
              reason: taintSummaryZh,
              targetIdHex: options.targetApplicationIdHex,
            },
          ],
        },
        allowedGrantScopes: ["SINGLE_ACTION"],
      },
      options.nowMonoMs,
    );

    this.pendingEscalations.set(z4Snap.challengeIdHex, {
      transferIdHex: record.metadata.transferIdHex,
      targetApplicationIdHex: options.targetApplicationIdHex,
      challengeIdHex: z4Snap.challengeIdHex,
      sessionGeneration: this.sessionGeneration,
    });
    this.audit.escalatedTaintedTransfers += 1;

    return {
      outcome: "ESCALATED_TO_Z4_CONSENT",
      transferIdHex: record.metadata.transferIdHex,
      challengeIdHex: z4Snap.challengeIdHex,
      channel: record.metadata.channel,
      sourceApplicationIdHex: record.metadata.sourceApplicationIdHex,
      targetApplicationIdHex: options.targetApplicationIdHex,
      taintFlags: record.metadata.taintFlags,
      requiresHighContrastAck: z4Snap.requiresHighContrastAck,
    };
  }

  public completeEscalatedTransferAfterZ4(options: {
    readonly transferIdHex: string;
    readonly targetApplicationIdHex: string;
    readonly challengeIdHex: string;
    readonly nowMonoMs: number;
  }): TransferReadDecision {
    if (this.sessionState !== "ACTIVE") {
      throw new TrustedUiRejectionError(
        "SESSION_NOT_ACTIVE_OR_STALE_GENERATION",
        "Session is not ACTIVE; escalated transfer blocked",
      );
    }
    const esc = this.pendingEscalations.get(options.challengeIdHex);
    if (
      !esc ||
      esc.transferIdHex !== options.transferIdHex ||
      esc.targetApplicationIdHex !== options.targetApplicationIdHex ||
      esc.sessionGeneration !== this.sessionGeneration
    ) {
      throw new TrustedUiRejectionError(
        "TRANSFER_CONSENT_MISMATCH_OR_DENIED",
        "No matching pending transfer escalation bound to security challenge",
      );
    }

    const challengeSnap = this.z4Dialog.inspectTrustedChallenge(
      { kind: "tcb_trusted_broker" },
      options.challengeIdHex,
    );
    if (challengeSnap.state !== "APPROVED") {
      throw new TrustedUiRejectionError(
        "TRANSFER_CONSENT_MISMATCH_OR_DENIED",
        `Security challenge ${options.challengeIdHex} is ${challengeSnap.state}; zero bytes released`,
      );
    }

    const record = this.transfers.get(options.transferIdHex);
    if (!record) {
      throw new TrustedUiRejectionError(
        "TRANSFER_NOT_FOUND",
        `Transfer ${options.transferIdHex} not found`,
      );
    }
    if (options.nowMonoMs >= record.metadata.expiresAtMonoMs) {
      this.pendingEscalations.delete(options.challengeIdHex);
      this.audit.rejectedExpiredTransferReads += 1;
      throw new TrustedUiRejectionError(
        "TRANSFER_EXPIRED",
        `Transfer ${options.transferIdHex} expired before release`,
      );
    }

    this.pendingEscalations.delete(options.challengeIdHex);
    return {
      outcome: "RELEASED",
      transferIdHex: record.metadata.transferIdHex,
      channel: record.metadata.channel,
      contentType: record.metadata.contentType,
      payloadDigestHex: record.metadata.payloadDigestHex,
      payloadText: record.payloadText,
      byteLength: record.metadata.byteLength,
      taintFlags: record.metadata.taintFlags,
    };
  }

  private consumeGestureToken(
    gestureTokenIdHex: string | null,
    expectedActorApplicationIdHex: string,
    expectedChannel: TransferChannelKind,
    nowMonoMs: number,
  ): string {
    if (gestureTokenIdHex === null || gestureTokenIdHex.trim().length === 0) {
      this.audit.rejectedMissingGestureReads += 1;
      throw new TrustedUiRejectionError(
        "MISSING_USER_GESTURE",
        "Clipboard or drag-drop operation rejected: missing fresh user_gesture_token",
      );
    }
    const rec = this.gestures.get(gestureTokenIdHex);
    if (
      !rec ||
      rec.consumed ||
      rec.token.actorApplicationIdHex !== expectedActorApplicationIdHex ||
      rec.token.channel !== expectedChannel ||
      rec.token.sessionGeneration !== this.sessionGeneration ||
      nowMonoMs >= rec.token.expiresAtMonoMs
    ) {
      this.audit.rejectedMissingGestureReads += 1;
      throw new TrustedUiRejectionError(
        "INVALID_OR_EXPIRED_USER_GESTURE",
        `Gesture token ${gestureTokenIdHex} is unknown, already consumed, expired, or bound to another app`,
      );
    }
    rec.consumed = true;
    return gestureTokenIdHex;
  }
}

export function createClipboardAndDragGuard(
  z4Dialog: Z4SecurityDialogController,
  options?: {
    readonly initialSessionState?: TrustedSessionLifecycle;
    readonly initialSessionGeneration?: number;
  },
): ClipboardAndDragGuard {
  return new ClipboardAndDragGuard(z4Dialog, options);
}
