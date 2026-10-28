// Push-to-talk & Toggle Voice Stream State Machine + Device Hardware Permission Projection
// Authority:
//   - docs/design/06-架构设计总纲-v0.5.md §24.2 ([TUI-EXT-001]), §25.4 ([DUI-INPUT-001], [DUI-SESSION-001])
//   - docs/management/adrs/0025-stage-d-hybrid-desktop-compositor-and-trusted-ui-layering.md
//   - docs/superpowers/specs/2026-10-26-stage-d-desktop-visual-and-ux-design.md §3.3
// Invariants:
//   - Voice capture routes exclusively as InputSource "voice" through FocusAndInputRoute.
//   - Active or pending voice capture is immediately severed if sessionState !== "ACTIVE"
//     ("LOCKED" | "SUSPENDED" | "CLOSED"), z4ModalActive === true, or mic permission is revoked.
//   - DevicePermissionState deterministically projects { microphoneActive, cameraActive, screenActive }
//     for the 36px L0 bottom taskbar hardware tray indicator.
//   - Zero internal jargon in L0/L1/L2 labels; monochrome geometric SVG stroke icons only.

import type { DeviceHardwareCaptureState } from "../dock";
import type { DesktopSessionStateDto, InputSourceKindDto } from "../types";
import type { WorkstationLocale } from "../ui";

export type VoiceCaptureMode = "push-to-talk" | "toggle";

export type VoiceStreamPhase =
  | "Idle"
  | "RequestingMicPermission"
  | "Capturing"
  | "Transcribing"
  | "Committed"
  | "Cancelled"
  | "Denied";

export type HardwarePermissionGrant = "PROMPT_REQUIRED" | "GRANTED" | "DENIED";

export type VoiceSeveranceReason =
  | "SESSION_NOT_ACTIVE"
  | "Z4_MODAL_ACTIVE"
  | "MIC_PERMISSION_REVOKED"
  | "UNTRUSTED_ORIGIN_SPOOF"
  | "ROUTE_REJECTED";

export interface DevicePermissionState {
  readonly micPermission: HardwarePermissionGrant;
  readonly cameraPermission: HardwarePermissionGrant;
  readonly screenCapturePermission: HardwarePermissionGrant;
  readonly micActive: boolean;
  readonly cameraActive: boolean;
  readonly screenCaptureActive: boolean;
}

export interface VoiceRouteCheckInput {
  readonly source: InputSourceKindDto;
  readonly fromUntrustedSurface: boolean;
  readonly sessionState: DesktopSessionStateDto;
  readonly sessionGeneration: number;
  readonly z4ModalActive: boolean;
  readonly nowMonoMs: number;
}

export interface VoiceRouteCheckDecision {
  readonly accepted: boolean;
  readonly rejectionCode?: string | null;
}

export interface VoiceStreamSnapshot {
  readonly phase: VoiceStreamPhase;
  readonly mode: VoiceCaptureMode;
  readonly streamingTranscript: string;
  readonly committedTranscript: string;
  readonly devicePermissions: DevicePermissionState;
  readonly taskbarIndicator: DeviceHardwareCaptureState;
  readonly sessionState: DesktopSessionStateDto;
  readonly sessionGeneration: number;
  readonly z4ModalActive: boolean;
  readonly lastSeveranceReason: VoiceSeveranceReason | null;
  readonly severedCount: number;
  readonly rejectedAttemptCount: number;
  readonly statusLabel: string;
}

export interface VoiceTransitionResult {
  readonly accepted: boolean;
  readonly phase: VoiceStreamPhase;
  readonly severanceReason: VoiceSeveranceReason | null;
  readonly committedTranscript?: string;
}

export interface VoiceInputControllerOptions {
  readonly initialMicPermission?: HardwarePermissionGrant;
  readonly initialCameraPermission?: HardwarePermissionGrant;
  readonly initialScreenCapturePermission?: HardwarePermissionGrant;
  readonly initialSessionState?: DesktopSessionStateDto;
  readonly initialSessionGeneration?: number;
  readonly initialZ4ModalActive?: boolean;
  readonly locale?: WorkstationLocale;
  readonly routeVoiceInput?: (input: VoiceRouteCheckInput) => VoiceRouteCheckDecision;
  readonly onHardwareCaptureChange?: (indicator: DeviceHardwareCaptureState) => void;
  readonly onTranscriptCommitted?: (transcript: string) => void;
}

export type VoiceStreamListener = (snapshot: VoiceStreamSnapshot) => void;

const VOICE_COPY: Readonly<
  Record<
    WorkstationLocale,
    Readonly<Record<VoiceStreamPhase, string>> & {
      readonly severedZ4: string;
      readonly severedSession: string;
      readonly severedRevoked: string;
    }
  >
> = {
  "zh-CN": {
    Idle: "语音待命（按住说话或点击切换）",
    RequestingMicPermission: "正在请求麦克风硬件授权",
    Capturing: "正在采集语音输入",
    Transcribing: "正在整理语音草稿",
    Committed: "语音已转入意图输入框",
    Cancelled: "语音采集已停止",
    Denied: "麦克风权限未开启",
    severedZ4: "系统安全确认期间已自动断开麦克风",
    severedSession: "会话已锁定或挂起，麦克风已自动断开",
    severedRevoked: "麦克风权限已收回，采集已停止",
  },
  "en-US": {
    Idle: "Voice ready (hold to talk or click to toggle)",
    RequestingMicPermission: "Requesting microphone hardware permission",
    Capturing: "Capturing voice input",
    Transcribing: "Transcribing spoken draft",
    Committed: "Voice transcript added to prompt",
    Cancelled: "Voice capture stopped",
    Denied: "Microphone permission denied",
    severedZ4: "Microphone severed while security prompt is active",
    severedSession: "Microphone severed because session is not active",
    severedRevoked: "Microphone permission revoked; capture stopped",
  },
};

export function projectDevicePermissionToTaskbar(
  state: DevicePermissionState,
): DeviceHardwareCaptureState {
  return {
    microphoneActive: state.micPermission === "GRANTED" && state.micActive,
    cameraActive: state.cameraPermission === "GRANTED" && state.cameraActive,
    screenActive:
      state.screenCapturePermission === "GRANTED" && state.screenCaptureActive,
  };
}

function isLiveVoicePhase(phase: VoiceStreamPhase): boolean {
  return (
    phase === "RequestingMicPermission" ||
    phase === "Capturing" ||
    phase === "Transcribing"
  );
}

export class VoiceInputController {
  private phase: VoiceStreamPhase = "Idle";
  private mode: VoiceCaptureMode = "push-to-talk";
  private streamingTranscript = "";
  private committedTranscript = "";
  private micPermission: HardwarePermissionGrant;
  private cameraPermission: HardwarePermissionGrant;
  private screenCapturePermission: HardwarePermissionGrant;
  private micActive = false;
  private cameraActive = false;
  private screenCaptureActive = false;
  private sessionState: DesktopSessionStateDto;
  private sessionGeneration: number;
  private z4ModalActive: boolean;
  private locale: WorkstationLocale;
  private lastSeveranceReason: VoiceSeveranceReason | null = null;
  private severedCount = 0;
  private rejectedAttemptCount = 0;
  private readonly routeVoiceInput?: (
    input: VoiceRouteCheckInput,
  ) => VoiceRouteCheckDecision;
  private readonly onHardwareCaptureChange?: (
    indicator: DeviceHardwareCaptureState,
  ) => void;
  private readonly onTranscriptCommitted?: (transcript: string) => void;
  private readonly listeners = new Set<VoiceStreamListener>();

  public constructor(options?: VoiceInputControllerOptions) {
    this.micPermission = options?.initialMicPermission ?? "PROMPT_REQUIRED";
    this.cameraPermission = options?.initialCameraPermission ?? "PROMPT_REQUIRED";
    this.screenCapturePermission =
      options?.initialScreenCapturePermission ?? "PROMPT_REQUIRED";
    this.sessionState = options?.initialSessionState ?? "ACTIVE";
    this.sessionGeneration = options?.initialSessionGeneration ?? 1;
    this.z4ModalActive = options?.initialZ4ModalActive ?? false;
    this.locale = options?.locale ?? "zh-CN";
    this.routeVoiceInput = options?.routeVoiceInput;
    this.onHardwareCaptureChange = options?.onHardwareCaptureChange;
    this.onTranscriptCommitted = options?.onTranscriptCommitted;
  }

  public setLocale(locale: WorkstationLocale): void {
    if (this.locale === locale) {
      return;
    }
    this.locale = locale;
    this.emit();
  }

  public subscribe(listener: VoiceStreamListener): () => void {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  }

  private buildDevicePermissionState(): DevicePermissionState {
    return {
      micPermission: this.micPermission,
      cameraPermission: this.cameraPermission,
      screenCapturePermission: this.screenCapturePermission,
      micActive: this.micActive,
      cameraActive: this.cameraActive,
      screenCaptureActive: this.screenCaptureActive,
    };
  }

  private computeStatusLabel(): string {
    const copy = VOICE_COPY[this.locale];
    if (this.phase === "Cancelled" && this.lastSeveranceReason !== null) {
      if (this.lastSeveranceReason === "Z4_MODAL_ACTIVE") {
        return copy.severedZ4;
      }
      if (this.lastSeveranceReason === "SESSION_NOT_ACTIVE") {
        return copy.severedSession;
      }
      if (this.lastSeveranceReason === "MIC_PERMISSION_REVOKED") {
        return copy.severedRevoked;
      }
    }
    return copy[this.phase];
  }

  public getSnapshot(): VoiceStreamSnapshot {
    const devicePermissions = this.buildDevicePermissionState();
    return {
      phase: this.phase,
      mode: this.mode,
      streamingTranscript: this.streamingTranscript,
      committedTranscript: this.committedTranscript,
      devicePermissions,
      taskbarIndicator: projectDevicePermissionToTaskbar(devicePermissions),
      sessionState: this.sessionState,
      sessionGeneration: this.sessionGeneration,
      z4ModalActive: this.z4ModalActive,
      lastSeveranceReason: this.lastSeveranceReason,
      severedCount: this.severedCount,
      rejectedAttemptCount: this.rejectedAttemptCount,
      statusLabel: this.computeStatusLabel(),
    };
  }

  private emit(): void {
    const snap = this.getSnapshot();
    this.onHardwareCaptureChange?.(snap.taskbarIndicator);
    for (const listener of this.listeners) {
      listener(snap);
    }
  }

  private verifyVoiceRouteGate(
    fromUntrustedSurface: boolean,
    nowMonoMs: number,
  ): VoiceSeveranceReason | null {
    if (fromUntrustedSurface) {
      return "UNTRUSTED_ORIGIN_SPOOF";
    }
    if (this.z4ModalActive) {
      return "Z4_MODAL_ACTIVE";
    }
    if (this.sessionState !== "ACTIVE") {
      return "SESSION_NOT_ACTIVE";
    }
    if (this.routeVoiceInput) {
      const decision = this.routeVoiceInput({
        source: "voice",
        fromUntrustedSurface,
        sessionState: this.sessionState,
        sessionGeneration: this.sessionGeneration,
        z4ModalActive: this.z4ModalActive,
        nowMonoMs,
      });
      if (!decision.accepted) {
        if (decision.rejectionCode === "UNTRUSTED_ORIGIN_SPOOF") {
          return "UNTRUSTED_ORIGIN_SPOOF";
        }
        if (decision.rejectionCode === "Z4_MODAL_LOCK") {
          return "Z4_MODAL_ACTIVE";
        }
        if (decision.rejectionCode === "SESSION_NOT_ACTIVE") {
          return "SESSION_NOT_ACTIVE";
        }
        return "ROUTE_REJECTED";
      }
    }
    return null;
  }

  private severActiveCapture(reason: VoiceSeveranceReason): void {
    const hadActiveHardware =
      isLiveVoicePhase(this.phase) ||
      this.micActive ||
      this.cameraActive ||
      this.screenCaptureActive;
    if (!hadActiveHardware) {
      return;
    }
    this.micActive = false;
    if (reason === "SESSION_NOT_ACTIVE" || reason === "Z4_MODAL_ACTIVE") {
      this.cameraActive = false;
      this.screenCaptureActive = false;
    }
    this.streamingTranscript = "";
    if (isLiveVoicePhase(this.phase)) {
      this.phase = "Cancelled";
    }
    this.lastSeveranceReason = reason;
    this.severedCount += 1;
  }

  public syncSecurityAndSessionGate(gate: {
    readonly sessionState: DesktopSessionStateDto;
    readonly sessionGeneration?: number;
    readonly z4ModalActive: boolean;
  }): VoiceStreamSnapshot {
    const prevGeneration = this.sessionGeneration;
    this.sessionState = gate.sessionState;
    if (gate.sessionGeneration !== undefined) {
      this.sessionGeneration = gate.sessionGeneration;
    }
    this.z4ModalActive = gate.z4ModalActive;

    if (gate.z4ModalActive) {
      this.severActiveCapture("Z4_MODAL_ACTIVE");
    } else if (
      gate.sessionState !== "ACTIVE" ||
      (gate.sessionGeneration !== undefined && gate.sessionGeneration !== prevGeneration)
    ) {
      this.severActiveCapture("SESSION_NOT_ACTIVE");
    }
    this.emit();
    return this.getSnapshot();
  }

  public startCapture(options?: {
    readonly mode?: VoiceCaptureMode;
    readonly fromUntrustedSurface?: boolean;
    readonly nowMonoMs?: number;
  }): VoiceTransitionResult {
    const nowMonoMs = options?.nowMonoMs ?? 0;
    const fromUntrusted = options?.fromUntrustedSurface ?? false;
    const blockReason = this.verifyVoiceRouteGate(fromUntrusted, nowMonoMs);
    if (blockReason !== null) {
      this.rejectedAttemptCount += 1;
      this.lastSeveranceReason = blockReason;
      this.micActive = false;
      this.emit();
      return {
        accepted: false,
        phase: this.phase,
        severanceReason: blockReason,
      };
    }

    this.mode = options?.mode ?? "push-to-talk";
    this.lastSeveranceReason = null;
    this.streamingTranscript = "";

    if (this.micPermission === "DENIED") {
      this.phase = "Denied";
      this.micActive = false;
      this.rejectedAttemptCount += 1;
      this.emit();
      return {
        accepted: false,
        phase: this.phase,
        severanceReason: "MIC_PERMISSION_REVOKED",
      };
    }

    if (this.micPermission === "PROMPT_REQUIRED") {
      this.phase = "RequestingMicPermission";
      this.micActive = false;
      this.emit();
      return {
        accepted: true,
        phase: this.phase,
        severanceReason: null,
      };
    }

    this.phase = "Capturing";
    this.micActive = true;
    this.emit();
    return {
      accepted: true,
      phase: this.phase,
      severanceReason: null,
    };
  }

  public toggleCapture(options?: {
    readonly fromUntrustedSurface?: boolean;
    readonly nowMonoMs?: number;
  }): VoiceTransitionResult {
    if (this.phase === "Capturing" || this.phase === "RequestingMicPermission") {
      if (this.streamingTranscript.trim().length > 0) {
        return this.commitTranscript();
      }
      return this.cancelCapture();
    }
    return this.startCapture({
      mode: "toggle",
      fromUntrustedSurface: options?.fromUntrustedSurface,
      nowMonoMs: options?.nowMonoMs,
    });
  }

  public grantMicPermission(nowMonoMs = 0): VoiceTransitionResult {
    this.micPermission = "GRANTED";
    const blockReason = this.verifyVoiceRouteGate(false, nowMonoMs);
    if (blockReason !== null) {
      this.rejectedAttemptCount += 1;
      this.severActiveCapture(blockReason);
      this.emit();
      return {
        accepted: false,
        phase: this.phase,
        severanceReason: blockReason,
      };
    }
    if (this.phase === "RequestingMicPermission") {
      this.phase = "Capturing";
      this.micActive = true;
      this.lastSeveranceReason = null;
    }
    this.emit();
    return {
      accepted: true,
      phase: this.phase,
      severanceReason: null,
    };
  }

  public denyMicPermission(): VoiceTransitionResult {
    const wasActive = isLiveVoicePhase(this.phase) || this.micActive;
    this.micPermission = "DENIED";
    this.micActive = false;
    this.streamingTranscript = "";
    this.phase = "Denied";
    if (wasActive) {
      this.lastSeveranceReason = "MIC_PERMISSION_REVOKED";
      this.severedCount += 1;
    }
    this.emit();
    return {
      accepted: false,
      phase: this.phase,
      severanceReason: "MIC_PERMISSION_REVOKED",
    };
  }

  public appendStreamingTranscript(
    chunk: string,
    options?: {
      readonly fromUntrustedSurface?: boolean;
      readonly nowMonoMs?: number;
    },
  ): VoiceTransitionResult {
    const blockReason = this.verifyVoiceRouteGate(
      options?.fromUntrustedSurface ?? false,
      options?.nowMonoMs ?? 0,
    );
    if (blockReason !== null) {
      this.rejectedAttemptCount += 1;
      if (blockReason !== "UNTRUSTED_ORIGIN_SPOOF") {
        this.severActiveCapture(blockReason);
      }
      this.emit();
      return {
        accepted: false,
        phase: this.phase,
        severanceReason: blockReason,
      };
    }

    if (this.phase !== "Capturing" && this.phase !== "Transcribing") {
      this.rejectedAttemptCount += 1;
      return {
        accepted: false,
        phase: this.phase,
        severanceReason: "ROUTE_REJECTED",
      };
    }

    const cleaned = chunk.trim();
    if (cleaned.length > 0) {
      this.streamingTranscript =
        this.streamingTranscript.length > 0
          ? `${this.streamingTranscript} ${cleaned}`
          : cleaned;
    }
    this.emit();
    return {
      accepted: true,
      phase: this.phase,
      severanceReason: null,
    };
  }

  public finishCaptureToTranscribing(nowMonoMs = 0): VoiceTransitionResult {
    const blockReason = this.verifyVoiceRouteGate(false, nowMonoMs);
    if (blockReason !== null) {
      this.rejectedAttemptCount += 1;
      this.severActiveCapture(blockReason);
      this.emit();
      return {
        accepted: false,
        phase: this.phase,
        severanceReason: blockReason,
      };
    }
    if (this.phase !== "Capturing") {
      return {
        accepted: false,
        phase: this.phase,
        severanceReason: "ROUTE_REJECTED",
      };
    }
    this.micActive = false;
    this.phase = "Transcribing";
    this.emit();
    return {
      accepted: true,
      phase: this.phase,
      severanceReason: null,
    };
  }

  public commitTranscript(
    finalOverride?: string,
    nowMonoMs = 0,
  ): VoiceTransitionResult {
    const blockReason = this.verifyVoiceRouteGate(false, nowMonoMs);
    if (blockReason !== null) {
      this.rejectedAttemptCount += 1;
      this.severActiveCapture(blockReason);
      this.emit();
      return {
        accepted: false,
        phase: this.phase,
        severanceReason: blockReason,
      };
    }

    if (this.phase !== "Capturing" && this.phase !== "Transcribing") {
      return {
        accepted: false,
        phase: this.phase,
        severanceReason: "ROUTE_REJECTED",
      };
    }

    const finalText = (finalOverride ?? this.streamingTranscript).trim();
    this.micActive = false;
    this.streamingTranscript = "";
    if (finalText.length === 0) {
      this.phase = "Cancelled";
      this.emit();
      return {
        accepted: true,
        phase: this.phase,
        severanceReason: null,
      };
    }

    this.committedTranscript = finalText;
    this.phase = "Committed";
    this.onTranscriptCommitted?.(finalText);
    this.emit();
    return {
      accepted: true,
      phase: this.phase,
      severanceReason: null,
      committedTranscript: finalText,
    };
  }

  public cancelCapture(): VoiceTransitionResult {
    this.micActive = false;
    this.streamingTranscript = "";
    this.lastSeveranceReason = null;
    this.phase = "Cancelled";
    this.emit();
    return {
      accepted: true,
      phase: this.phase,
      severanceReason: null,
    };
  }

  public resetToIdle(): void {
    this.micActive = false;
    this.streamingTranscript = "";
    this.lastSeveranceReason = null;
    this.phase = "Idle";
    this.emit();
  }

  public setCameraCapture(
    permission: HardwarePermissionGrant,
    active: boolean,
  ): DevicePermissionState {
    this.cameraPermission = permission;
    this.cameraActive =
      permission === "GRANTED" &&
      active &&
      this.sessionState === "ACTIVE" &&
      !this.z4ModalActive;
    this.emit();
    return this.buildDevicePermissionState();
  }

  public setScreenCapture(
    permission: HardwarePermissionGrant,
    active: boolean,
  ): DevicePermissionState {
    this.screenCapturePermission = permission;
    this.screenCaptureActive =
      permission === "GRANTED" &&
      active &&
      this.sessionState === "ACTIVE" &&
      !this.z4ModalActive;
    this.emit();
    return this.buildDevicePermissionState();
  }
}

export function createVoiceInputController(
  options?: VoiceInputControllerOptions,
): VoiceInputController {
  return new VoiceInputController(options);
}
