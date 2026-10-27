// Deterministic 5-Plane Workstation Compositor, Session Lock & Z4 Security Controller,
// 5-Source Input Router, and Keyboard Landmark Orchestrator (D-COMPOSITOR-WM / W70).
// Authority:
//   - docs/management/adrs/0025-stage-d-hybrid-desktop-compositor-and-trusted-ui-layering.md
//   - docs/superpowers/specs/2026-10-26-stage-d-desktop-visual-and-ux-design.md (§2, §2A, §3)
// Invariants:
//   - 5-Plane Z-order: Z0 (0), Z1 (100), Z2 (200..299), Z3 (400/430/460), Z4 (900)
//   - StageRect height strictly viewportHeight - 36px (never overlaps Z3 taskbar)
//   - 32x20 VerificationSeal & personal seal phrase strictly confined to Z4
//   - Z4 350ms arming guard (confirm < 350ms rejected; Escape at 0ms accepted; confirm >= 350ms accepted)
//   - While Z4 modal or non-ACTIVE session is engaged, all 5 input sources targeting Z2 are severed & audited
//   - Zero internal jargon in L0/L1/L2 visible text; zero emojis; 100% solid opaque surfaces

import {
  ApplicationLauncherModel,
  createApplicationLauncherModel,
  createBottomTaskbarElement,
  createLauncherMenuElement,
  renderBottomTaskbarHtml,
  renderLauncherMenuHtml,
  type ApplicationMutationTarget,
  type ConnectivityPostureKind,
  type DeviceHardwareCaptureState,
  type InstalledApplicationEntry,
  type LaunchApplicationRequest,
  type TaskbarConnectivityState,
  type TaskbarWindowTabItem,
  type ValidateLaunchResult,
  type WorkstationThemeId,
} from "../dock";
import {
  computeAllowedVerbs,
  createCommandReconciler,
  createDesktopProjectionStore,
  createDisclosureModel,
  type CommandReconcilerOptions,
  type ControlCommandReconciler,
  type DesktopProjectionStore,
  type DisclosureModel,
  type DisclosureSnapshot,
  type ProjectionDataSource,
} from "../store";
import type {
  DesktopSessionStateDto,
  InputAuditCountersDto,
  InputSourceKindDto,
  SessionWorkspaceDto,
  UserProfilePublicDto,
} from "../types";
import {
  SEAL_HEIGHT_PX,
  SEAL_WIDTH_PX,
  computeSealPhaseMarkers,
  createBlastRadiusDiffElement,
  createBulletMicroBarElement,
  createInspectorDrawerElement,
  createStatusIndicatorElement,
  createStatutoryLockBadgeElement,
  createVerificationSeal,
  renderBlastRadiusDiffHtml,
  renderBulletMicroBarHtml,
  renderInspectorDrawerHtml,
  renderStatusIndicatorHtml,
  renderStatutoryLockBadgeHtml,
  sampleGeronoCurve,
  type BlastRadiusRow,
  type DisclosureLevel,
  type ScopeBreadcrumbItem,
  type StatutoryLockKind,
  type VerificationSealController,
  type WorkstationLocale,
  type WorkstationStatusState,
} from "../ui";
import {
  TASKBAR_HEIGHT_PX,
  Z0_Z_INDEX,
  Z1_Z_INDEX,
  Z2_Z_INDEX_MAX,
  Z2_Z_INDEX_MIN,
  Z3_FLYOUT_Z_INDEX,
  Z3_PALETTE_Z_INDEX,
  Z3_TASKBAR_Z_INDEX,
  Z4_SECURITY_Z_INDEX,
  createTilingWindowManager,
  layoutModeForShortcut,
  type LayoutShortcutDigit,
  type ManagedWindowDescriptor,
  type ManagedWindowRecord,
  type TilingWindowManager,
  type WindowManagerSnapshot,
  type WmLayoutMode,
  type WmRectPx,
} from "../wm";

export const Z4_ARMING_WINDOW_MS = 350;

export const LANDMARK_RING_ORDER = [
  "z2-active-window",
  "z1-task-space",
  "z2-inspector-drawer",
  "z3-bottom-taskbar",
] as const;

export type WorkstationLandmarkId = (typeof LANDMARK_RING_ORDER)[number];

export const ALL_INPUT_SOURCES: readonly InputSourceKindDto[] = [
  "keyboard",
  "pointer",
  "voice",
  "drag_drop",
  "automation",
] as const;

export interface WorkstationPlaneContract {
  readonly z0Backdrop: typeof Z0_Z_INDEX;
  readonly z1TaskSpace: typeof Z1_Z_INDEX;
  readonly z2WindowsMin: typeof Z2_Z_INDEX_MIN;
  readonly z2WindowsMax: typeof Z2_Z_INDEX_MAX;
  readonly z3Taskbar: typeof Z3_TASKBAR_Z_INDEX;
  readonly z3Flyout: typeof Z3_FLYOUT_Z_INDEX;
  readonly z3Palette: typeof Z3_PALETTE_Z_INDEX;
  readonly z4Security: typeof Z4_SECURITY_Z_INDEX;
}

export const WORKSTATION_PLANE_CONTRACT: WorkstationPlaneContract = {
  z0Backdrop: Z0_Z_INDEX,
  z1TaskSpace: Z1_Z_INDEX,
  z2WindowsMin: Z2_Z_INDEX_MIN,
  z2WindowsMax: Z2_Z_INDEX_MAX,
  z3Taskbar: Z3_TASKBAR_Z_INDEX,
  z3Flyout: Z3_FLYOUT_Z_INDEX,
  z3Palette: Z3_PALETTE_Z_INDEX,
  z4Security: Z4_SECURITY_Z_INDEX,
};

export interface WorkstationTaskRowInput {
  readonly taskId: string;
  readonly title: string;
  readonly workspaceIdHex?: string;
  readonly workspaceName?: string;
  readonly status: WorkstationStatusState;
  readonly actualUsage: number;
  readonly effectiveGrantedOrLeaseCap: number;
  readonly requestedCeiling: number;
  readonly resourceUnit?: string;
  readonly summaryNote?: string;
  readonly planIdHex?: string;
  readonly breadcrumbs?: readonly ScopeBreadcrumbItem[];
  readonly readBackingIdHex?: string;
  readonly expectedRevision?: number;
  readonly canPause?: boolean;
  readonly canResume?: boolean;
  readonly canCancel?: boolean;
  readonly isTerminal?: boolean;
  readonly pendingApproval?: boolean;
}

export interface Z4SecurityModalInput {
  readonly promptId: string;
  readonly title: string;
  readonly summary?: string;
  readonly blastRadiusItems?: readonly BlastRadiusRow[];
  readonly statutoryLock?: StatutoryLockKind;
  readonly confirmLabel?: string;
  readonly cancelLabel?: string;
  readonly onConfirm?: () => void;
  readonly onCancel?: () => void;
}

export interface Z4SecurityModalSnapshot {
  readonly promptId: string;
  readonly title: string;
  readonly summary: string;
  readonly armedAtMonoMs: number;
  readonly armingWindowMs: number;
  readonly blastRadiusItems: readonly BlastRadiusRow[];
  readonly statutoryLock: StatutoryLockKind | null;
}

export type InputRejectionCode =
  | "UNTRUSTED_ORIGIN_SPOOF"
  | "Z4_MODAL_LOCK"
  | "Z4_ARMING_GUARD"
  | "STATUTORY_LOCK_FORBIDDEN"
  | "SESSION_NOT_ACTIVE"
  | "WINDOW_NOT_PRESENTED"
  | "STALE_GENERATION_SURFACE"
  | "Z3_SYSTEM_OCCLUSION";

export interface WorkstationInputEvent {
  readonly source: InputSourceKindDto;
  readonly targetPlane: "Z1" | "Z2" | "Z3" | "Z4";
  readonly targetWindowId?: string;
  readonly fromUntrustedSurface?: boolean;
  readonly action?: "interact" | "confirm-z4" | "cancel-z4" | "system-shortcut";
  readonly key?: string;
  readonly nowMonoMs?: number;
}

export interface WorkstationInputRouteResult {
  readonly accepted: boolean;
  readonly routedPlane: "Z1" | "Z2" | "Z3" | "Z4" | "NONE";
  readonly targetWindowId: string | null;
  readonly rejectionCode: InputRejectionCode | null;
  readonly remainingArmingMs?: number;
}

export interface WorkstationKeyEventInput {
  readonly key: string;
  readonly ctrlKey?: boolean;
  readonly altKey?: boolean;
  readonly shiftKey?: boolean;
  readonly metaKey?: boolean;
  readonly targetIsTextInput?: boolean;
  readonly targetTaskId?: string;
  readonly fromUntrustedSurface?: boolean;
  readonly nowMonoMs?: number;
  readonly preventDefault?: () => void;
}

export type WorkstationShortcutAction =
  | "none"
  | "cancel-z4"
  | "confirm-z4"
  | "z4-arming-rejected"
  | "close-palette"
  | "close-z3-flyout"
  | "close-inspector"
  | "lock-session"
  | "toggle-l3-diagnostics"
  | "cycle-landmark"
  | "switch-layout"
  | "toggle-l2-inspector"
  | "toggle-command-palette"
  | "blocked";

export interface WorkstationShortcutResult {
  readonly handled: boolean;
  readonly action: WorkstationShortcutAction;
  readonly landmark?: WorkstationLandmarkId;
  readonly layoutMode?: WmLayoutMode;
  readonly disclosure?: DisclosureLevel;
  readonly rejectionCode?: InputRejectionCode;
}

export interface UntrustedSurfaceVisibleContext {
  readonly windowId: string;
  readonly surfaceId: string;
  readonly locale: WorkstationLocale;
  readonly theme: WorkstationThemeId;
  readonly innerContentClipRect: WmRectPx | null;
}

export interface WorkstationCompositorSnapshot {
  readonly planeContract: WorkstationPlaneContract;
  readonly sessionState: DesktopSessionStateDto;
  readonly sessionGeneration: number;
  readonly activePrincipal: UserProfilePublicDto;
  readonly workspaces: readonly SessionWorkspaceDto[];
  readonly activeWorkspaceIdHex: string;
  readonly disclosure: DisclosureLevel;
  readonly theme: WorkstationThemeId;
  readonly locale: WorkstationLocale;
  readonly activeLandmark: WorkstationLandmarkId;
  readonly availableLandmarks: readonly WorkstationLandmarkId[];
  readonly launcherOpen: boolean;
  readonly commandPaletteOpen: boolean;
  readonly openZ3FlyoutIds: readonly string[];
  readonly inspectorOpen: boolean;
  readonly selectedTaskId: string | null;
  readonly z4Active: boolean;
  readonly z4Reason: "none" | "session-lock" | "security-modal";
  readonly activeZ4Modal: Z4SecurityModalSnapshot | null;
  readonly inputAudit: InputAuditCountersDto;
  readonly totalRejectedEvents: number;
  readonly rejectedBySource: Readonly<Record<InputSourceKindDto, number>>;
  readonly wm: WindowManagerSnapshot;
  readonly tasks: readonly WorkstationTaskRowInput[];
}

export interface WorkstationCompositorCallbacks {
  readonly onTaskVerb?: (
    taskId: string,
    verb: "pause" | "resume" | "cancel",
  ) => void;
  readonly onDisableApplication?: (target: ApplicationMutationTarget) => void;
  readonly onUninstallApplication?: (target: ApplicationMutationTarget) => void;
  readonly onSessionTransition?: (
    nextState: DesktopSessionStateDto,
    generation: number,
  ) => void;
  readonly onLayoutModeChanged?: (mode: WmLayoutMode) => void;
}

export interface WorkstationCompositorConfig {
  readonly viewportWidth?: number;
  readonly viewportHeight?: number;
  readonly initialDisclosure?: DisclosureLevel;
  readonly initialTheme?: WorkstationThemeId;
  readonly initialLocale?: WorkstationLocale;
  readonly initialLayoutMode?: WmLayoutMode;
  readonly initialSessionState?: DesktopSessionStateDto;
  readonly initialSessionGeneration?: number;
  readonly initialPrincipal?: UserProfilePublicDto;
  /** Private personal verification phrase rendered strictly inside Z4. */
  readonly initialSecuritySealPhrase?: string;
  readonly initialWorkspaces?: readonly SessionWorkspaceDto[];
  readonly initialTasks?: readonly WorkstationTaskRowInput[];
  readonly installedApplications?: readonly InstalledApplicationEntry[];
  readonly knownPackageIdsHex?: readonly string[];
  readonly knownProcessIdsHex?: readonly string[];
  readonly connectivity?: TaskbarConnectivityState;
  readonly hardwareCapture?: DeviceHardwareCaptureState;
  readonly clockText?: string;
  readonly disclosureModel?: DisclosureModel;
  readonly projectionStore?: DesktopProjectionStore;
  readonly projectionDataSource?: Partial<ProjectionDataSource>;
  readonly commandReconcilerOptions?: CommandReconcilerOptions;
  readonly launcherModel?: ApplicationLauncherModel;
  readonly windowManager?: TilingWindowManager;
  readonly callbacks?: WorkstationCompositorCallbacks;
  readonly nowMonoMs?: () => number;
  readonly doc?: Document;
}

interface PrincipalPrivateSessionRecord {
  profile: UserProfilePublicDto;
  sealPhrase: string;
  workspaces: SessionWorkspaceDto[];
  activeWorkspaceIdHex: string;
  layoutMode: WmLayoutMode;
  savedWindows: ManagedWindowDescriptor[];
  tasks: WorkstationTaskRowInput[];
}

interface CompositorCopy {
  readonly taskPaneTitle: string;
  readonly taskPaneAria: string;
  readonly colTask: string;
  readonly colStatus: string;
  readonly colResource: string;
  readonly colWorkspace: string;
  readonly colActions: string;
  readonly emptyTasks: string;
  readonly verbPause: string;
  readonly verbResume: string;
  readonly verbCancel: string;
  readonly verbInspect: string;
  readonly l3ToggleBtn: string;
  readonly inspectorHeading: string;
  readonly inspectorStatusLabel: string;
  readonly inspectorResourceLabel: string;
  readonly inspectorSummaryLabel: string;
  readonly inspectorOpenL3Btn: string;
  readonly l3HostTitle: string;
  readonly lockHeading: string;
  readonly lockSubtitle: string;
  readonly sealCaption: string;
  readonly unlockBtn: string;
  readonly z4DefaultHeading: string;
  readonly z4ArmingHint: string;
  readonly z4ConfirmBtn: string;
  readonly z4CancelBtn: string;
  readonly paletteSlotPlaceholder: string;
}

const COMPOSITOR_COPY: Readonly<Record<WorkstationLocale, CompositorCopy>> = {
  "zh-CN": {
    taskPaneTitle: "任务与工作区",
    taskPaneAria: "任务与工作区概览面板",
    colTask: "任务名称",
    colStatus: "状态",
    colResource: "资源使用",
    colWorkspace: "工作区",
    colActions: "操作",
    emptyTasks: "当前工作区暂无运行中任务",
    verbPause: "暂停",
    verbResume: "继续",
    verbCancel: "取消",
    verbInspect: "详情",
    l3ToggleBtn: "深度诊断",
    inspectorHeading: "任务详情",
    inspectorStatusLabel: "当前状态",
    inspectorResourceLabel: "资源使用与上限",
    inspectorSummaryLabel: "执行摘要",
    inspectorOpenL3Btn: "打开深度诊断面板 (Ctrl+Shift+E)",
    l3HostTitle: "深度诊断检查器 (L3)",
    lockHeading: "工作站已锁定",
    lockSubtitle: "请核对下方专属防伪印记后再解锁工作站",
    sealCaption: "专属防伪印记",
    unlockBtn: "解锁工作站",
    z4DefaultHeading: "系统安全确认",
    z4ArmingHint: "防误触保护预备中，请核对上方印记与变更范围",
    z4ConfirmBtn: "确认授权",
    z4CancelBtn: "取消 (Esc)",
    paletteSlotPlaceholder: "命令面板就绪",
  },
  "en-US": {
    taskPaneTitle: "Tasks & Workspaces",
    taskPaneAria: "Tasks and workspaces overview pane",
    colTask: "Task",
    colStatus: "Status",
    colResource: "Resource Usage",
    colWorkspace: "Workspace",
    colActions: "Actions",
    emptyTasks: "No active tasks in this workspace",
    verbPause: "Pause",
    verbResume: "Resume",
    verbCancel: "Cancel",
    verbInspect: "Inspect",
    l3ToggleBtn: "Diagnostics",
    inspectorHeading: "Task Summary",
    inspectorStatusLabel: "Current Status",
    inspectorResourceLabel: "Resource Usage & Limit",
    inspectorSummaryLabel: "Execution Summary",
    inspectorOpenL3Btn: "Open Deep Diagnostics (Ctrl+Shift+E)",
    l3HostTitle: "Deep Diagnostic Inspector (L3)",
    lockHeading: "Workstation Locked",
    lockSubtitle: "Verify your personal security seal below before unlocking",
    sealCaption: "Personal Verification Seal",
    unlockBtn: "Unlock Workstation",
    z4DefaultHeading: "System Security Confirmation",
    z4ArmingHint: "Arming protection active; review seal and impact scope above",
    z4ConfirmBtn: "Authorize",
    z4CancelBtn: "Cancel (Esc)",
    paletteSlotPlaceholder: "Command Palette Ready",
  },
};

const VALID_32_HEX = /^[0-9a-fA-F]{32}$/;

function escapeHtml(raw: string): string {
  return raw
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

function defaultPrincipalProfile(
  locale: WorkstationLocale,
  theme: WorkstationThemeId,
): UserProfilePublicDto {
  return {
    principalIdHex: "01010101010101010101010101010101",
    displayName: locale === "en-US" ? "Primary Operator" : "默认操作员",
    locale,
    theme,
    dataContainerScopeIdHex: "10101010101010101010101010101010",
  };
}

function defaultWorkspaces(
  principalIdHex: string,
  locale: WorkstationLocale,
): SessionWorkspaceDto[] {
  return [
    {
      workspaceIdHex: "a1000000000000000000000000000001",
      ownerPrincipalIdHex: principalIdHex,
      name: locale === "en-US" ? "Main Workspace" : "主工作区",
      state: "ACTIVE",
      revision: 1,
      layoutMode: "three-pane-workstation",
    },
    {
      workspaceIdHex: "a1000000000000000000000000000002",
      ownerPrincipalIdHex: principalIdHex,
      name: locale === "en-US" ? "Review Workspace" : "复核工作区",
      state: "ACTIVE",
      revision: 1,
      layoutMode: "two-column-split",
    },
  ];
}

function createOfflineNoopDataSource(): Partial<ProjectionDataSource> {
  const emptyReceiptHex = "00";
  const dummyCmdId = "00000000000000000000000000000000";
  return {
    pollDesktopWatermarkEvents: async () => ({
      receiptHex: emptyReceiptHex,
      controlCommandIdHex: dummyCmdId,
      correlationIdHex: dummyCmdId,
      outcome: {
        kind: "desktop_watermark_polled",
        currentWatermark: 0,
        events: [],
        truncated: false,
      },
    }),
    listTasks: async () => ({
      receiptHex: emptyReceiptHex,
      controlCommandIdHex: dummyCmdId,
      correlationIdHex: dummyCmdId,
      outcome: {
        kind: "tasks_listed",
        tasks: [],
        nextCursorHex: null,
        truncated: false,
      },
    }),
    listInstalledApplications: async () => ({
      receiptHex: emptyReceiptHex,
      controlCommandIdHex: dummyCmdId,
      correlationIdHex: dummyCmdId,
      outcome: {
        kind: "installed_applications_listed",
        applications: [],
        nextCursorHex: null,
        truncated: false,
      },
    }),
    listWorkspaces: async () => ({
      receiptHex: emptyReceiptHex,
      controlCommandIdHex: dummyCmdId,
      correlationIdHex: dummyCmdId,
      outcome: {
        kind: "workspaces_listed",
        workspaces: [],
        nextCursorHex: null,
        truncated: false,
      },
    }),
    listNotifications: async () => ({
      receiptHex: emptyReceiptHex,
      controlCommandIdHex: dummyCmdId,
      correlationIdHex: dummyCmdId,
      outcome: {
        kind: "notifications_listed",
        notifications: [],
        nextCursorHex: null,
        truncated: false,
      },
    }),
    listCells: async () => ({
      receiptHex: emptyReceiptHex,
      controlCommandIdHex: dummyCmdId,
      correlationIdHex: dummyCmdId,
      outcome: {
        kind: "cells_listed",
        cells: [],
        nextCursorHex: null,
        truncated: false,
      },
    }),
    listTaskNodes: async (planIdHex) => ({
      receiptHex: emptyReceiptHex,
      controlCommandIdHex: dummyCmdId,
      correlationIdHex: dummyCmdId,
      outcome: {
        kind: "task_nodes_listed",
        planIdHex,
        currentPlanRevision: 0,
        staleGeneration: false,
        nodes: [],
        metadataOnlyCollapsedCount: 0,
        coldCollapsedCount: 0,
        nextCursorHex: null,
        truncated: false,
      },
    }),
    inspectResourceCost: async (reservationIdHex) => ({
      receiptHex: emptyReceiptHex,
      controlCommandIdHex: dummyCmdId,
      correlationIdHex: dummyCmdId,
      outcome: {
        kind: "resource_inspected",
        reservationIdHex,
        accountIdHex: dummyCmdId,
        upperBound: 0,
        usageHighWater: 0,
        consumptionCount: 0,
      },
    }),
    listTaskAttempts: async () => ({
      receiptHex: emptyReceiptHex,
      controlCommandIdHex: dummyCmdId,
      correlationIdHex: dummyCmdId,
      outcome: {
        kind: "task_attempts_listed",
        currentGeneration: 0,
        staleGeneration: false,
        attempts: [],
        nextCursorHex: null,
        truncated: false,
      },
    }),
  };
}

function computeLauncherFlyoutRect(viewportWidth: number, viewportHeight: number): WmRectPx {
  const stageH = Math.max(180, viewportHeight - TASKBAR_HEIGHT_PX);
  const w = Math.min(360, Math.max(220, viewportWidth - 16));
  const h = Math.min(412, Math.max(160, stageH - 16));
  return {
    x: 8,
    y: Math.max(8, stageH - h - 4),
    width: w,
    height: h,
  };
}

function computePaletteFlyoutRect(viewportWidth: number, viewportHeight: number): WmRectPx {
  const stageH = Math.max(180, viewportHeight - TASKBAR_HEIGHT_PX);
  const w = Math.min(560, Math.max(260, viewportWidth - 24));
  const h = Math.min(320, Math.max(140, stageH - 32));
  const x = Math.max(8, Math.round((viewportWidth - w) / 2));
  const y = Math.max(8, Math.round(stageH * 0.14));
  return { x, y, width: w, height: h };
}

function computeSystemFlyoutRect(viewportWidth: number, viewportHeight: number): WmRectPx {
  const stageH = Math.max(180, viewportHeight - TASKBAR_HEIGHT_PX);
  const w = Math.min(360, Math.max(220, viewportWidth - 16));
  const h = Math.min(380, Math.max(160, stageH - 16));
  return {
    x: Math.max(8, viewportWidth - w - 8),
    y: Math.max(8, stageH - h - 4),
    width: w,
    height: h,
  };
}

function renderZ4SealSvgHtml(sealTitle: string): string {
  const points = sampleGeronoCurve(64)
    .map((pt) => `${pt.x.toFixed(2)},${pt.y.toFixed(2)}`)
    .join(" ");
  const [primaryMarker, harmonicMarker] = computeSealPhaseMarkers(0);
  return (
    `<div class="nlos-verification-seal" data-nlos-seal="personal-verification-seal" ` +
    `role="img" aria-label="${escapeHtml(sealTitle)}" title="${escapeHtml(sealTitle)}">` +
    `<svg class="nlos-verification-seal__canvas" width="${SEAL_WIDTH_PX}" height="${SEAL_HEIGHT_PX}" ` +
    `viewBox="0 0 ${SEAL_WIDTH_PX} ${SEAL_HEIGHT_PX}" aria-hidden="true">` +
    `<polyline fill="none" stroke="currentColor" stroke-width="1.25" points="${points}"></polyline>` +
    `<rect x="${(primaryMarker.x - 1).toFixed(2)}" y="${(primaryMarker.y - 1).toFixed(2)}" width="2" height="2" fill="currentColor"></rect>` +
    `<rect x="${(harmonicMarker.x - 0.75).toFixed(2)}" y="${(harmonicMarker.y - 0.75).toFixed(2)}" width="1.5" height="1.5" fill="currentColor"></rect>` +
    `</svg>` +
    `</div>`
  );
}

function collectVisibleDomText(node: unknown, currentDisclosure: DisclosureLevel): string {
  if (node === null || node === undefined || typeof node !== "object") {
    return "";
  }
  const el = node as {
    hidden?: boolean;
    className?: string;
    tagName?: string;
    textContent?: string;
    getAttribute?: (name: string) => string | null;
    children?: readonly unknown[];
  };
  if (el.hidden === true) {
    return "";
  }
  if (typeof el.getAttribute === "function" && el.getAttribute("hidden") !== null) {
    return "";
  }
  const cls = typeof el.className === "string" ? el.className : "";
  const classes = cls.split(/\s+/).filter((t) => t.length > 0);
  if (currentDisclosure !== "L3" && classes.includes("nlos-l3-only")) {
    return "";
  }
  if (
    (currentDisclosure === "L0" || currentDisclosure === "L1") &&
    classes.includes("nlos-l2-up")
  ) {
    return "";
  }
  if (el.tagName && el.tagName.toLowerCase() === "iframe") {
    return "";
  }
  if (Array.isArray(el.children) && el.children.length > 0) {
    return el.children
      .map((child) => collectVisibleDomText(child, currentDisclosure))
      .filter((s) => s.length > 0)
      .join(" ");
  }
  return (el.textContent ?? "").trim();
}

function resolveTaskRowAllowedVerbs(
  task: WorkstationTaskRowInput,
  locale: WorkstationLocale,
  disclosure: DisclosureLevel,
): readonly string[] {
  const backingHex =
    task.readBackingIdHex && VALID_32_HEX.test(task.readBackingIdHex)
      ? task.readBackingIdHex
      : "11111111111111111111111111111111";
  const mappedState = task.isTerminal
    ? "COMPLETED"
    : task.status === "SUSPENDED"
      ? "SUSPENDED"
      : "RUNNING";
  const hasWriteIntent = Boolean(task.canPause || task.canResume || task.canCancel);
  const res = computeAllowedVerbs({
    domain: "task",
    state: mappedState,
    callerCapabilities: hasWriteIntent
      ? ["control:read", "control:write"]
      : ["control:read"],
    readBacking: {
      targetIdHex: backingHex,
      expectedGenerationOrRevision: task.expectedRevision ?? 1,
    },
    locale,
    disclosure,
  });
  return res.allowedVerbs.filter((v) => {
    if (v === "Pause") {
      return Boolean(task.canPause);
    }
    if (v === "Resume") {
      return Boolean(task.canResume);
    }
    if (v === "Cancel") {
      return Boolean(task.canCancel);
    }
    return false;
  });
}

export class WorkstationCompositor {
  public readonly disclosureModel: DisclosureModel;
  public readonly projectionStore: DesktopProjectionStore;
  public readonly commandReconciler: ControlCommandReconciler;
  public readonly launcherModel: ApplicationLauncherModel;
  private currentWindowManager: TilingWindowManager;
  private wmUnsubscribe: (() => void) | null = null;

  public get windowManager(): TilingWindowManager {
    return this.currentWindowManager;
  }

  private readonly nowMono: () => number;
  private readonly callbacks?: WorkstationCompositorCallbacks;
  private readonly defaultDoc?: Document;

  private sessionState: DesktopSessionStateDto;
  private sessionGeneration: number;
  private activePrincipalIdHex: string;
  private readonly principalSessions = new Map<string, PrincipalPrivateSessionRecord>();

  private theme: WorkstationThemeId;
  private locale: WorkstationLocale;
  private activeLandmark: WorkstationLandmarkId = "z1-task-space";

  private launcherOpen = false;
  private commandPaletteOpen = false;
  private readonly customZ3Flyouts = new Map<string, WmRectPx>();
  private inspectorOpen = false;
  private selectedTaskId: string | null = null;
  private z1ScrollTop = 0;

  private activeZ4Modal: (Z4SecurityModalSnapshot & {
    onConfirm?: () => void;
    onCancel?: () => void;
    confirmLabel?: string;
    cancelLabel?: string;
  }) | null = null;

  private connectivity: TaskbarConnectivityState;
  private hardwareCapture: DeviceHardwareCaptureState;
  private clockText: string;
  private unreadNotificationCount = 0;

  private acceptedEvents = 0;
  private totalRejectedEvents = 0;
  private rejectedLockedSessionEvents = 0;
  private rejectedZ4SeveredEvents = 0;
  private rejectedZ4ArmingGuardEvents = 0;
  private rejectedStaleSurfaceEvents = 0;
  private rejectedUntrustedSpoofEvents = 0;
  private readonly rejectedBySource: Record<InputSourceKindDto, number> = {
    keyboard: 0,
    pointer: 0,
    voice: 0,
    drag_drop: 0,
    automation: 0,
  };

  private mountedContainer: HTMLElement | null = null;
  private mountedRootEl: HTMLElement | null = null;
  private mountedDoc: Document | null = null;
  private l3DiagnosticElement: HTMLElement | null = null;
  private customPaletteElement: HTMLElement | null = null;
  private customFlyoutElement: HTMLElement | null = null;
  private activeZ4SealHandle: VerificationSealController | null = null;
  private isInternalWmMutation = false;
  private readonly listeners = new Set<(snapshot: WorkstationCompositorSnapshot) => void>();

  public constructor(config?: WorkstationCompositorConfig) {
    this.nowMono = config?.nowMonoMs ?? (() => performance.now());
    this.callbacks = config?.callbacks;
    this.defaultDoc = config?.doc;
    this.locale = config?.initialLocale ?? "zh-CN";
    this.theme = config?.initialTheme ?? "graphite-workstation";
    this.sessionState = config?.initialSessionState ?? "ACTIVE";
    this.sessionGeneration = Math.max(1, config?.initialSessionGeneration ?? 1);

    const initialDisclosure = config?.initialDisclosure ?? "L1";
    this.disclosureModel =
      config?.disclosureModel ??
      createDisclosureModel({
        initialLevel: initialDisclosure,
        nowMs: this.nowMono,
      });

    this.projectionStore =
      config?.projectionStore ??
      createDesktopProjectionStore({
        disclosureModel: this.disclosureModel,
        dataSource: config?.projectionDataSource ?? createOfflineNoopDataSource(),
      });

    this.commandReconciler = createCommandReconciler(
      config?.commandReconcilerOptions,
    );

    this.launcherModel =
      config?.launcherModel ??
      createApplicationLauncherModel({
        applications: config?.installedApplications ?? [],
        knownPackageIdsHex: config?.knownPackageIdsHex ?? [],
        knownProcessIdsHex: config?.knownProcessIdsHex ?? [],
      });

    const viewportWidth = config?.viewportWidth ?? 1440;
    const viewportHeight = config?.viewportHeight ?? 900;
    const initialLayoutMode = config?.initialLayoutMode ?? "three-pane-workstation";

    this.currentWindowManager =
      config?.windowManager ??
      createTilingWindowManager({
        viewportWidth,
        viewportHeight,
        initialLayoutMode,
        showZ1TaskSpace: true,
        locale: this.locale,
        disclosure: this.disclosureModel.getLevel(),
      });

    const initialProfile =
      config?.initialPrincipal ?? defaultPrincipalProfile(this.locale, this.theme);
    this.activePrincipalIdHex = initialProfile.principalIdHex;
    const initialWorkspaces =
      config?.initialWorkspaces && config.initialWorkspaces.length > 0
        ? [...config.initialWorkspaces]
        : defaultWorkspaces(initialProfile.principalIdHex, this.locale);

    this.principalSessions.set(initialProfile.principalIdHex, {
      profile: initialProfile,
      sealPhrase: config?.initialSecuritySealPhrase ?? "NORTH-HARBOR-47",
      workspaces: initialWorkspaces,
      activeWorkspaceIdHex: initialWorkspaces[0]!.workspaceIdHex,
      layoutMode: initialLayoutMode,
      savedWindows: [],
      tasks: [...(config?.initialTasks ?? [])],
    });

    const firstTask = config?.initialTasks?.[0];
    if (firstTask) {
      this.selectedTaskId = firstTask.taskId;
      this.disclosureModel.selectEntity(firstTask.taskId, "task");
    }

    this.connectivity = config?.connectivity ?? {
      kind: "local-ready",
      l3TopologyTag: "LOCAL_SINGLE_NODE",
    };
    this.hardwareCapture = config?.hardwareCapture ?? {};
    this.clockText = config?.clockText ?? "09:42";

    this.syncSystemOverlaysToWindowManager();
    this.wmUnsubscribe = this.currentWindowManager.subscribe(() => {
      if (!this.isInternalWmMutation) {
        this.emitAndRefreshDom();
      }
    });
  }

  private getActivePrincipalSession(): PrincipalPrivateSessionRecord {
    const rec = this.principalSessions.get(this.activePrincipalIdHex);
    if (!rec) {
      throw new Error("Missing active principal session record");
    }
    return rec;
  }

  public isZ4ModalOpen(): boolean {
    return this.activeZ4Modal !== null;
  }

  public isZ4Active(): boolean {
    return this.sessionState === "LOCKED" || this.activeZ4Modal !== null;
  }

  private isZ4BlockingInput(): boolean {
    return this.sessionState !== "ACTIVE" || this.activeZ4Modal !== null;
  }

  private computeActiveZ3OverlayRects(): WmRectPx[] {
    const wmSnap = this.windowManager.getSnapshot();
    const rects: WmRectPx[] = [];
    if (this.launcherOpen) {
      rects.push(computeLauncherFlyoutRect(wmSnap.viewportWidth, wmSnap.viewportHeight));
    }
    if (this.commandPaletteOpen) {
      rects.push(computePaletteFlyoutRect(wmSnap.viewportWidth, wmSnap.viewportHeight));
    }
    for (const rect of this.customZ3Flyouts.values()) {
      rects.push(rect);
    }
    return rects;
  }

  private syncSystemOverlaysToWindowManager(): void {
    this.isInternalWmMutation = true;
    try {
      this.windowManager.setSystemOverlayState({
        z4ModalLock: this.isZ4BlockingInput(),
        activeZ3OverlayRects: this.computeActiveZ3OverlayRects(),
      });
    } finally {
      this.isInternalWmMutation = false;
    }
  }

  public getAvailableLandmarks(): readonly WorkstationLandmarkId[] {
    const wmSnap = this.windowManager.getSnapshot();
    const hasVisibleWindow = wmSnap.windows.some((w) => w.windowState === "VISIBLE");
    const hasZ1Pane = wmSnap.geometry.z1TaskSpaceRect !== null;
    const hasL2Drawer =
      this.inspectorOpen &&
      (this.disclosureModel.getLevel() === "L2" ||
        this.disclosureModel.getLevel() === "L3");

    const available: WorkstationLandmarkId[] = [];
    if (hasVisibleWindow) {
      available.push("z2-active-window");
    }
    if (hasZ1Pane) {
      available.push("z1-task-space");
    }
    if (hasL2Drawer) {
      available.push("z2-inspector-drawer");
    }
    available.push("z3-bottom-taskbar");
    return available;
  }

  private normalizeActiveLandmark(): void {
    const available = this.getAvailableLandmarks();
    if (!available.includes(this.activeLandmark)) {
      this.activeLandmark = available[0] ?? "z3-bottom-taskbar";
    }
  }

  public subscribe(
    listener: (snapshot: WorkstationCompositorSnapshot) => void,
  ): () => void {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  }

  private emitAndRefreshDom(): void {
    this.normalizeActiveLandmark();
    if (this.mountedContainer && this.mountedDoc) {
      this.rerenderMountedDom();
    }
    const snap = this.getSnapshot();
    for (const listener of this.listeners) {
      listener(snap);
    }
  }

  public getSnapshot(): WorkstationCompositorSnapshot {
    const principalRec = this.getActivePrincipalSession();
    const wmSnap = this.windowManager.getSnapshot();
    const z4Reason: "none" | "session-lock" | "security-modal" =
      this.activeZ4Modal !== null
        ? "security-modal"
        : this.sessionState === "LOCKED"
          ? "session-lock"
          : "none";

    return {
      planeContract: WORKSTATION_PLANE_CONTRACT,
      sessionState: this.sessionState,
      sessionGeneration: this.sessionGeneration,
      activePrincipal: { ...principalRec.profile },
      workspaces: principalRec.workspaces.map((ws) => ({ ...ws })),
      activeWorkspaceIdHex: principalRec.activeWorkspaceIdHex,
      disclosure: this.disclosureModel.getLevel(),
      theme: this.theme,
      locale: this.locale,
      activeLandmark: this.activeLandmark,
      availableLandmarks: this.getAvailableLandmarks(),
      launcherOpen: this.launcherOpen,
      commandPaletteOpen: this.commandPaletteOpen,
      openZ3FlyoutIds: Array.from(this.customZ3Flyouts.keys()),
      inspectorOpen: this.inspectorOpen,
      selectedTaskId: this.selectedTaskId,
      z4Active: this.isZ4Active(),
      z4Reason,
      activeZ4Modal: this.activeZ4Modal
        ? {
            promptId: this.activeZ4Modal.promptId,
            title: this.activeZ4Modal.title,
            summary: this.activeZ4Modal.summary,
            armedAtMonoMs: this.activeZ4Modal.armedAtMonoMs,
            armingWindowMs: this.activeZ4Modal.armingWindowMs,
            blastRadiusItems: [...this.activeZ4Modal.blastRadiusItems],
            statutoryLock: this.activeZ4Modal.statutoryLock,
          }
        : null,
      inputAudit: {
        acceptedEvents: this.acceptedEvents,
        rejectedStaleSurfaceEvents: this.rejectedStaleSurfaceEvents,
        rejectedLockedSessionEvents: this.rejectedLockedSessionEvents,
        rejectedZ4SeveredEvents: this.rejectedZ4SeveredEvents,
        rejectedZ4ArmingGuardEvents: this.rejectedZ4ArmingGuardEvents,
        rejectedUntrustedSpoofEvents: this.rejectedUntrustedSpoofEvents,
      },
      totalRejectedEvents: this.totalRejectedEvents,
      rejectedBySource: { ...this.rejectedBySource },
      wm: wmSnap,
      tasks: [...principalRec.tasks],
    };
  }

  /**
   * Returns the exact context observable by a sandboxed Z2 window surface.
   * Never exposes securitySealPhrase, principalIdHex, sessionGeneration, or sibling windows.
   */
  public getUntrustedSurfaceVisibleContext(
    windowId: string,
  ): UntrustedSurfaceVisibleContext | null {
    const wmSnap = this.windowManager.getSnapshot();
    const record = wmSnap.windows.find((w) => w.windowId === windowId);
    if (!record) {
      return null;
    }
    const solved = wmSnap.geometry.windows.find((g) => g.windowId === windowId);
    return {
      windowId: record.windowId,
      surfaceId: record.surfaceId,
      locale: this.locale,
      theme: this.theme,
      innerContentClipRect: solved ? { ...solved.innerContentClipRect } : null,
    };
  }

  /* ==========================================================================
     Viewport, Theme, Locale, Tasks & Connectivity Controls
     ========================================================================== */

  public setViewportSize(width: number, height: number): void {
    this.isInternalWmMutation = true;
    try {
      this.windowManager.setViewportSize(width, height);
    } finally {
      this.isInternalWmMutation = false;
    }
    this.syncSystemOverlaysToWindowManager();
    this.emitAndRefreshDom();
  }

  public setTheme(nextTheme: WorkstationThemeId): void {
    this.theme = nextTheme;
    const rec = this.getActivePrincipalSession();
    rec.profile = { ...rec.profile, theme: nextTheme };
    if (typeof document !== "undefined" && document.documentElement) {
      document.documentElement.setAttribute("data-nlos-theme", nextTheme);
    }
    this.emitAndRefreshDom();
  }

  public toggleTheme(): "graphite-workstation" | "paper-workstation" {
    const next =
      this.theme === "paper-workstation"
        ? "graphite-workstation"
        : "paper-workstation";
    this.setTheme(next);
    return next;
  }

  public setLocale(nextLocale: WorkstationLocale): void {
    this.locale = nextLocale;
    const rec = this.getActivePrincipalSession();
    rec.profile = { ...rec.profile, locale: nextLocale };
    this.isInternalWmMutation = true;
    try {
      this.windowManager.setLocale(nextLocale);
    } finally {
      this.isInternalWmMutation = false;
    }
    this.emitAndRefreshDom();
  }

  public setConnectivity(
    kind: ConnectivityPostureKind,
    options?: { onlineNodes?: number; totalNodes?: number; l3TopologyTag?: string },
  ): void {
    this.connectivity = {
      kind,
      onlineNodes: options?.onlineNodes,
      totalNodes: options?.totalNodes,
      l3TopologyTag: options?.l3TopologyTag ?? "LOCAL_SINGLE_NODE",
    };
    this.emitAndRefreshDom();
  }

  public setHardwareCapture(capture: DeviceHardwareCaptureState): void {
    this.hardwareCapture = { ...capture };
    this.emitAndRefreshDom();
  }

  public setClockText(clockText: string): void {
    this.clockText = clockText;
    this.emitAndRefreshDom();
  }

  public setUnreadNotificationCount(count: number): void {
    this.unreadNotificationCount = Math.max(0, Math.floor(count));
    this.emitAndRefreshDom();
  }

  public setTasks(tasks: readonly WorkstationTaskRowInput[]): void {
    const rec = this.getActivePrincipalSession();
    rec.tasks = [...tasks];
    if (this.selectedTaskId && !rec.tasks.some((t) => t.taskId === this.selectedTaskId)) {
      this.selectedTaskId = rec.tasks[0]?.taskId ?? null;
    } else if (!this.selectedTaskId && rec.tasks.length > 0) {
      this.selectedTaskId = rec.tasks[0]!.taskId;
    }
    this.disclosureModel.selectEntity(this.selectedTaskId, "task");
    this.emitAndRefreshDom();
  }

  public setInstalledApplications(apps: readonly InstalledApplicationEntry[]): void {
    this.launcherModel.setInstalledApplications(apps);
    this.emitAndRefreshDom();
  }

  public setL3DiagnosticElement(element: HTMLElement | null): void {
    this.l3DiagnosticElement = element;
    this.emitAndRefreshDom();
  }

  public setCommandPaletteElement(element: HTMLElement | null): void {
    this.customPaletteElement = element;
    this.emitAndRefreshDom();
  }

  public setSystemFlyoutElement(element: HTMLElement | null): void {
    this.customFlyoutElement = element;
    this.emitAndRefreshDom();
  }

  /* ==========================================================================
     Progressive Disclosure (L0..L3) & Non-Modal L2 InspectorDrawer
     ========================================================================== */

  public setDisclosure(nextLevel: DisclosureLevel): DisclosureSnapshot {
    const snap = this.disclosureModel.setLevel(nextLevel);
    this.isInternalWmMutation = true;
    try {
      this.windowManager.setDisclosure(snap.level);
    } finally {
      this.isInternalWmMutation = false;
    }
    if (snap.level === "L0" || snap.level === "L1") {
      this.inspectorOpen = false;
    } else if (snap.level === "L2") {
      this.inspectorOpen = true;
    }
    if (typeof document !== "undefined" && document.documentElement) {
      document.documentElement.setAttribute("data-nlos-disclosure", snap.level);
    }
    this.emitAndRefreshDom();
    void this.projectionStore.refreshForCurrentDisclosure();
    return snap;
  }

  public toggleDeepDiagnostics(): DisclosureLevel {
    const nextLevel = this.disclosureModel.toggleDiagnosticL3();
    this.isInternalWmMutation = true;
    try {
      this.windowManager.setDisclosure(nextLevel);
    } finally {
      this.isInternalWmMutation = false;
    }
    if (nextLevel === "L0" || nextLevel === "L1") {
      this.inspectorOpen = false;
    }
    if (typeof document !== "undefined" && document.documentElement) {
      document.documentElement.setAttribute("data-nlos-disclosure", nextLevel);
    }
    this.emitAndRefreshDom();
    void this.projectionStore.refreshForCurrentDisclosure();
    return nextLevel;
  }

  public selectTaskAndToggleInspector(taskId?: string): boolean {
    const rec = this.getActivePrincipalSession();
    const resolvedId = taskId ?? this.selectedTaskId ?? rec.tasks[0]?.taskId ?? null;
    if (!resolvedId) {
      return false;
    }
    const sameTask = this.selectedTaskId === resolvedId;
    this.selectedTaskId = resolvedId;

    const currentLevel = this.disclosureModel.getLevel();
    if (currentLevel === "L2" && this.inspectorOpen && sameTask) {
      this.inspectorOpen = false;
      this.disclosureModel.setLevel("L1");
      this.isInternalWmMutation = true;
      try {
        this.windowManager.setDisclosure("L1");
      } finally {
        this.isInternalWmMutation = false;
      }
      this.emitAndRefreshDom();
      void this.projectionStore.refreshForCurrentDisclosure();
      return false;
    }

    this.inspectorOpen = true;
    this.disclosureModel.selectEntity(resolvedId, "task");
    if (currentLevel === "L0" || currentLevel === "L1") {
      this.disclosureModel.setLevel("L2");
      this.isInternalWmMutation = true;
      try {
        this.windowManager.setDisclosure("L2");
      } finally {
        this.isInternalWmMutation = false;
      }
    }
    this.emitAndRefreshDom();
    void this.projectionStore.refreshForCurrentDisclosure();
    return true;
  }

  public closeInspector(): void {
    if (!this.inspectorOpen) {
      return;
    }
    this.inspectorOpen = false;
    if (this.disclosureModel.getLevel() === "L2") {
      this.disclosureModel.closeDrawerOnEscape();
      this.isInternalWmMutation = true;
      try {
        this.windowManager.setDisclosure(this.disclosureModel.getLevel());
      } finally {
        this.isInternalWmMutation = false;
      }
    }
    this.emitAndRefreshDom();
  }

  /* ==========================================================================
     DesktopSession Lifecycle, Multi-User Switching & Z4 350ms Arming Guard
     ========================================================================== */

  public lockSession(): number {
    if (this.sessionState !== "LOCKED") {
      this.sessionState = "LOCKED";
      this.sessionGeneration += 1;
    }
    this.launcherOpen = false;
    this.commandPaletteOpen = false;
    this.customZ3Flyouts.clear();
    this.inspectorOpen = false;
    this.projectionStore.clearSensitiveCacheOnSessionLock();
    this.syncSystemOverlaysToWindowManager();
    this.callbacks?.onSessionTransition?.(this.sessionState, this.sessionGeneration);
    this.emitAndRefreshDom();
    return this.sessionGeneration;
  }

  public unlockSession(): boolean {
    if (this.sessionState === "CLOSED") {
      return false;
    }
    this.sessionState = "ACTIVE";
    if (this.disclosureModel.getLevel() === "L0") {
      this.disclosureModel.setLevel("L1");
      this.isInternalWmMutation = true;
      try {
        this.windowManager.setDisclosure("L1");
      } finally {
        this.isInternalWmMutation = false;
      }
    }
    this.syncSystemOverlaysToWindowManager();
    this.callbacks?.onSessionTransition?.(this.sessionState, this.sessionGeneration);
    this.emitAndRefreshDom();
    return true;
  }

  public suspendSession(): number {
    if (this.sessionState !== "SUSPENDED" && this.sessionState !== "CLOSED") {
      this.sessionState = "SUSPENDED";
      this.sessionGeneration += 1;
    }
    this.launcherOpen = false;
    this.commandPaletteOpen = false;
    this.customZ3Flyouts.clear();
    this.projectionStore.clearSensitiveCacheOnSessionLock();
    this.syncSystemOverlaysToWindowManager();
    this.callbacks?.onSessionTransition?.(this.sessionState, this.sessionGeneration);
    this.emitAndRefreshDom();
    return this.sessionGeneration;
  }

  public resumeSession(): boolean {
    if (this.sessionState !== "SUSPENDED") {
      return false;
    }
    this.sessionState = "ACTIVE";
    if (this.disclosureModel.getLevel() === "L0") {
      this.disclosureModel.setLevel("L1");
    }
    this.syncSystemOverlaysToWindowManager();
    this.callbacks?.onSessionTransition?.(this.sessionState, this.sessionGeneration);
    this.emitAndRefreshDom();
    return true;
  }

  public closeSession(): number {
    if (this.sessionState !== "CLOSED") {
      this.sessionState = "CLOSED";
      this.sessionGeneration += 1;
    }
    this.launcherOpen = false;
    this.commandPaletteOpen = false;
    this.customZ3Flyouts.clear();
    this.inspectorOpen = false;
    this.projectionStore.clearSensitiveCacheOnSessionLock();
    this.isInternalWmMutation = true;
    try {
      for (const w of this.windowManager.getSnapshot().windows) {
        this.windowManager.closeWindow(w.windowId);
      }
    } finally {
      this.isInternalWmMutation = false;
    }
    this.syncSystemOverlaysToWindowManager();
    this.callbacks?.onSessionTransition?.(this.sessionState, this.sessionGeneration);
    this.emitAndRefreshDom();
    return this.sessionGeneration;
  }

  public switchUser(
    nextProfile: UserProfilePublicDto,
    options?: {
      securitySealPhrase?: string;
      workspaces?: readonly SessionWorkspaceDto[];
      tasks?: readonly WorkstationTaskRowInput[];
    },
  ): number {
    const currentRec = this.getActivePrincipalSession();
    const currentSnap = this.windowManager.getSnapshot();
    currentRec.layoutMode = currentSnap.layoutMode;
    currentRec.savedWindows = currentSnap.windows
      .filter((w) => w.windowState !== "CLOSED")
      .map((w) => ({
        windowId: w.windowId,
        surfaceId: w.surfaceId,
        applicationIdHex: w.applicationIdHex,
        installationIdHex: w.installationIdHex,
        title: w.title,
        appIcon: w.appIcon,
        surfaceGeneration: w.surfaceGeneration,
        latestInstallationGeneration: w.latestInstallationGeneration,
        isFloating: w.isFloating,
        floatingRect: w.floatingRect,
        preferredSlot: w.preferredSlot,
        srcdoc: w.srcdoc,
        iframeSrc: w.iframeSrc,
        presentImmediately: w.windowState === "VISIBLE",
      }));

    this.isInternalWmMutation = true;
    try {
      for (const w of currentSnap.windows) {
        this.windowManager.closeWindow(w.windowId);
      }
    } finally {
      this.isInternalWmMutation = false;
    }

    this.sessionGeneration += 1;
    this.launcherOpen = false;
    this.commandPaletteOpen = false;
    this.customZ3Flyouts.clear();
    this.inspectorOpen = false;
    this.activeZ4Modal = null;
    this.projectionStore.clearSensitiveCacheOnSessionLock();

    let targetRec = this.principalSessions.get(nextProfile.principalIdHex);
    if (!targetRec) {
      const ws =
        options?.workspaces && options.workspaces.length > 0
          ? [...options.workspaces]
          : defaultWorkspaces(nextProfile.principalIdHex, nextProfile.locale);
      targetRec = {
        profile: { ...nextProfile },
        sealPhrase: options?.securitySealPhrase ?? "SOUTH-BEACON-82",
        workspaces: ws,
        activeWorkspaceIdHex: ws[0]!.workspaceIdHex,
        layoutMode: "three-pane-workstation",
        savedWindows: [],
        tasks: [...(options?.tasks ?? [])],
      };
      this.principalSessions.set(nextProfile.principalIdHex, targetRec);
    } else {
      targetRec.profile = { ...nextProfile };
      if (options?.securitySealPhrase !== undefined) {
        targetRec.sealPhrase = options.securitySealPhrase;
      }
      if (options?.tasks !== undefined) {
        targetRec.tasks = [...options.tasks];
      }
    }

    this.activePrincipalIdHex = nextProfile.principalIdHex;
    this.locale = nextProfile.locale;
    this.theme = nextProfile.theme;
    this.disclosureModel.setLevel("L1");

    if (this.wmUnsubscribe) {
      this.wmUnsubscribe();
      this.wmUnsubscribe = null;
    }
    this.currentWindowManager = createTilingWindowManager({
      viewportWidth: currentSnap.viewportWidth,
      viewportHeight: currentSnap.viewportHeight,
      initialLayoutMode: targetRec.layoutMode,
      showZ1TaskSpace: true,
      locale: this.locale,
      disclosure: "L1",
    });

    this.isInternalWmMutation = true;
    try {
      for (const saved of targetRec.savedWindows) {
        this.currentWindowManager.registerWindow(saved);
      }
    } finally {
      this.isInternalWmMutation = false;
    }

    this.wmUnsubscribe = this.currentWindowManager.subscribe(() => {
      if (!this.isInternalWmMutation) {
        this.emitAndRefreshDom();
      }
    });

    this.selectedTaskId = targetRec.tasks[0]?.taskId ?? null;
    this.disclosureModel.selectEntity(this.selectedTaskId, "task");
    this.sessionState = "ACTIVE";
    this.syncSystemOverlaysToWindowManager();
    this.callbacks?.onSessionTransition?.(this.sessionState, this.sessionGeneration);
    this.emitAndRefreshDom();
    return this.sessionGeneration;
  }

  public switchWorkspace(workspaceIdHex: string): boolean {
    const rec = this.getActivePrincipalSession();
    const target = rec.workspaces.find((ws) => ws.workspaceIdHex === workspaceIdHex);
    if (!target) {
      return false;
    }
    rec.activeWorkspaceIdHex = workspaceIdHex;
    this.isInternalWmMutation = true;
    try {
      this.windowManager.setLayoutMode(target.layoutMode as WmLayoutMode);
    } finally {
      this.isInternalWmMutation = false;
    }
    this.emitAndRefreshDom();
    return true;
  }

  public openZ4Modal(input: Z4SecurityModalInput, nowMonoMs?: number): Z4SecurityModalSnapshot {
    const armedAt = nowMonoMs ?? this.nowMono();
    this.launcherOpen = false;
    this.commandPaletteOpen = false;
    this.activeZ4Modal = {
      promptId: input.promptId,
      title: input.title,
      summary: input.summary ?? "",
      armedAtMonoMs: armedAt,
      armingWindowMs: Z4_ARMING_WINDOW_MS,
      blastRadiusItems: [...(input.blastRadiusItems ?? [])],
      statutoryLock: input.statutoryLock ?? null,
      confirmLabel: input.confirmLabel,
      cancelLabel: input.cancelLabel,
      onConfirm: input.onConfirm,
      onCancel: input.onCancel,
    };
    this.syncSystemOverlaysToWindowManager();
    this.emitAndRefreshDom();
    return {
      promptId: this.activeZ4Modal.promptId,
      title: this.activeZ4Modal.title,
      summary: this.activeZ4Modal.summary,
      armedAtMonoMs: this.activeZ4Modal.armedAtMonoMs,
      armingWindowMs: this.activeZ4Modal.armingWindowMs,
      blastRadiusItems: [...this.activeZ4Modal.blastRadiusItems],
      statutoryLock: this.activeZ4Modal.statutoryLock,
    };
  }

  public confirmZ4Modal(nowMonoMs?: number): WorkstationInputRouteResult {
    if (!this.activeZ4Modal) {
      return {
        accepted: false,
        routedPlane: "NONE",
        targetWindowId: null,
        rejectionCode: null,
      };
    }
    if (this.activeZ4Modal.statutoryLock !== null) {
      this.totalRejectedEvents += 1;
      this.rejectedZ4SeveredEvents += 1;
      return {
        accepted: false,
        routedPlane: "Z4",
        targetWindowId: null,
        rejectionCode: "STATUTORY_LOCK_FORBIDDEN",
      };
    }

    const now = nowMonoMs ?? this.nowMono();
    const elapsedMs = now - this.activeZ4Modal.armedAtMonoMs;
    if (elapsedMs < Z4_ARMING_WINDOW_MS) {
      this.totalRejectedEvents += 1;
      this.rejectedZ4ArmingGuardEvents += 1;
      const remaining = Math.max(1, Math.ceil(Z4_ARMING_WINDOW_MS - elapsedMs));
      return {
        accepted: false,
        routedPlane: "Z4",
        targetWindowId: null,
        rejectionCode: "Z4_ARMING_GUARD",
        remainingArmingMs: remaining,
      };
    }

    const cb = this.activeZ4Modal.onConfirm;
    this.activeZ4Modal = null;
    this.acceptedEvents += 1;
    this.syncSystemOverlaysToWindowManager();
    cb?.();
    this.emitAndRefreshDom();
    return {
      accepted: true,
      routedPlane: "Z4",
      targetWindowId: null,
      rejectionCode: null,
    };
  }

  public cancelZ4Modal(): WorkstationInputRouteResult {
    if (!this.activeZ4Modal) {
      return {
        accepted: false,
        routedPlane: "NONE",
        targetWindowId: null,
        rejectionCode: null,
      };
    }
    const cb = this.activeZ4Modal.onCancel;
    this.activeZ4Modal = null;
    this.acceptedEvents += 1;
    this.syncSystemOverlaysToWindowManager();
    cb?.();
    this.emitAndRefreshDom();
    return {
      accepted: true,
      routedPlane: "Z4",
      targetWindowId: null,
      rejectionCode: null,
    };
  }

  /* ==========================================================================
     Z3 Flyouts, Launcher ([DUI-LAUNCH-001]), Command Palette & Window Actions
     ========================================================================== */

  public setLauncherOpen(open: boolean): void {
    if (this.isZ4BlockingInput() && open) {
      return;
    }
    this.launcherOpen = open;
    this.syncSystemOverlaysToWindowManager();
    this.emitAndRefreshDom();
  }

  public toggleLauncher(): boolean {
    this.setLauncherOpen(!this.launcherOpen);
    return this.launcherOpen;
  }

  public setCommandPaletteOpen(open: boolean): void {
    if (this.isZ4BlockingInput() && open) {
      return;
    }
    this.commandPaletteOpen = open;
    this.syncSystemOverlaysToWindowManager();
    this.emitAndRefreshDom();
  }

  public setZ3FlyoutOpen(flyoutId: string, open: boolean, customRect?: WmRectPx): void {
    if (this.isZ4BlockingInput() && open) {
      return;
    }
    if (open) {
      const wmSnap = this.windowManager.getSnapshot();
      this.customZ3Flyouts.set(
        flyoutId,
        customRect ?? computeSystemFlyoutRect(wmSnap.viewportWidth, wmSnap.viewportHeight),
      );
    } else {
      this.customZ3Flyouts.delete(flyoutId);
    }
    this.syncSystemOverlaysToWindowManager();
    this.emitAndRefreshDom();
  }

  public launchApplication(
    request: LaunchApplicationRequest,
    options?: {
      windowId?: string;
      surfaceId?: string;
      title?: string;
      srcdoc?: string;
      isFloating?: boolean;
    },
  ): ValidateLaunchResult & { readonly windowRecord?: ManagedWindowRecord } {
    const validation = this.launcherModel.validateAndPrepareLaunch(request);
    if (!validation.ok) {
      return validation;
    }

    const existingCount = this.windowManager.getSnapshot().windows.length;
    const windowId =
      options?.windowId ??
      `win-${validation.applicationIdHex.slice(0, 8)}-${existingCount + 1}`;
    const surfaceId =
      options?.surfaceId ??
      `surf-${validation.installationIdHex.slice(0, 8)}-${existingCount + 1}`;

    let record: ManagedWindowRecord;
    this.isInternalWmMutation = true;
    try {
      record = this.windowManager.registerWindow({
        windowId,
        surfaceId,
        applicationIdHex: validation.applicationIdHex,
        installationIdHex: validation.installationIdHex,
        title: options?.title ?? validation.displayName,
        surfaceGeneration: validation.installationGeneration,
        latestInstallationGeneration: validation.installationGeneration,
        isFloating: options?.isFloating ?? false,
        srcdoc: options?.srcdoc,
        presentImmediately: true,
      });
    } finally {
      this.isInternalWmMutation = false;
    }

    this.launcherOpen = false;
    this.activeLandmark = "z2-active-window";
    this.syncSystemOverlaysToWindowManager();
    this.emitAndRefreshDom();
    return {
      ...validation,
      windowRecord: record,
    };
  }

  public setLayoutMode(mode: WmLayoutMode): void {
    const rec = this.getActivePrincipalSession();
    rec.layoutMode = mode;
    const activeWs = rec.workspaces.find(
      (w) => w.workspaceIdHex === rec.activeWorkspaceIdHex,
    );
    if (activeWs) {
      const idx = rec.workspaces.indexOf(activeWs);
      rec.workspaces[idx] = {
        ...activeWs,
        layoutMode: mode,
      };
    }
    this.isInternalWmMutation = true;
    try {
      this.windowManager.setLayoutMode(mode);
    } finally {
      this.isInternalWmMutation = false;
    }
    this.callbacks?.onLayoutModeChanged?.(mode);
    this.emitAndRefreshDom();
  }

  /* ==========================================================================
     5-Source Input Router ([DUI-INPUT-001]) & Keyboard Shortcut Ring
     ========================================================================== */

  private recordRejection(
    source: InputSourceKindDto,
    code: InputRejectionCode,
  ): WorkstationInputRouteResult {
    this.totalRejectedEvents += 1;
    this.rejectedBySource[source] += 1;
    switch (code) {
      case "UNTRUSTED_ORIGIN_SPOOF":
        this.rejectedUntrustedSpoofEvents += 1;
        break;
      case "Z4_MODAL_LOCK":
      case "STATUTORY_LOCK_FORBIDDEN":
      case "Z3_SYSTEM_OCCLUSION":
      case "WINDOW_NOT_PRESENTED":
        this.rejectedZ4SeveredEvents += 1;
        break;
      case "Z4_ARMING_GUARD":
        this.rejectedZ4ArmingGuardEvents += 1;
        break;
      case "SESSION_NOT_ACTIVE":
        this.rejectedLockedSessionEvents += 1;
        break;
      case "STALE_GENERATION_SURFACE":
        this.rejectedStaleSurfaceEvents += 1;
        break;
    }
    return {
      accepted: false,
      routedPlane: "NONE",
      targetWindowId: null,
      rejectionCode: code,
    };
  }

  public routeInput(event: WorkstationInputEvent): WorkstationInputRouteResult {
    if (
      event.fromUntrustedSurface &&
      (event.targetPlane === "Z4" ||
        event.targetPlane === "Z3" ||
        event.action === "system-shortcut")
    ) {
      return this.recordRejection(event.source, "UNTRUSTED_ORIGIN_SPOOF");
    }

    if (this.activeZ4Modal !== null) {
      if (event.targetPlane !== "Z4") {
        return this.recordRejection(event.source, "Z4_MODAL_LOCK");
      }
      if (event.action === "cancel-z4" || event.key === "Escape") {
        return this.cancelZ4Modal();
      }
      if (event.action === "confirm-z4" || event.key === "Enter") {
        const res = this.confirmZ4Modal(event.nowMonoMs);
        if (!res.accepted) {
          this.rejectedBySource[event.source] += 1;
        }
        return res;
      }
      this.acceptedEvents += 1;
      return {
        accepted: true,
        routedPlane: "Z4",
        targetWindowId: null,
        rejectionCode: null,
      };
    }

    if (this.sessionState !== "ACTIVE") {
      if (this.sessionState === "LOCKED" && event.targetPlane === "Z4") {
        this.acceptedEvents += 1;
        return {
          accepted: true,
          routedPlane: "Z4",
          targetWindowId: null,
          rejectionCode: null,
        };
      }
      return this.recordRejection(event.source, "SESSION_NOT_ACTIVE");
    }

    if (event.targetPlane === "Z2") {
      const wmSnap = this.windowManager.getSnapshot();
      const targetWindowId = event.targetWindowId ?? wmSnap.focusedWindowId;
      if (!targetWindowId) {
        return this.recordRejection(event.source, "WINDOW_NOT_PRESENTED");
      }
      const decision = this.windowManager.canRouteInputToWindow(targetWindowId);
      if (!decision.allowed) {
        switch (decision.reason) {
          case "Z4_MODAL_LOCK":
            return this.recordRejection(event.source, "Z4_MODAL_LOCK");
          case "STALE_GENERATION":
            return this.recordRejection(event.source, "STALE_GENERATION_SURFACE");
          case "OCCLUDED_BY_SYSTEM_PLANE":
            return this.recordRejection(event.source, "Z3_SYSTEM_OCCLUSION");
          default:
            return this.recordRejection(event.source, "WINDOW_NOT_PRESENTED");
        }
      }
      this.acceptedEvents += 1;
      return {
        accepted: true,
        routedPlane: "Z2",
        targetWindowId,
        rejectionCode: null,
      };
    }

    this.acceptedEvents += 1;
    return {
      accepted: true,
      routedPlane: event.targetPlane,
      targetWindowId: null,
      rejectionCode: null,
    };
  }

  public cycleLandmark(reverse = false): WorkstationLandmarkId {
    const available = this.getAvailableLandmarks();
    if (available.length === 0) {
      return this.activeLandmark;
    }
    const currentIdx = available.indexOf(this.activeLandmark);
    const nextIdx =
      currentIdx < 0
        ? 0
        : (currentIdx + (reverse ? -1 : 1) + available.length) % available.length;
    this.activeLandmark = available[nextIdx]!;
    this.emitAndRefreshDom();
    this.focusActiveLandmarkDom();
    return this.activeLandmark;
  }

  private focusActiveLandmarkDom(): void {
    if (!this.mountedRootEl) {
      return;
    }
    const target = findFirstByAttr(
      this.mountedRootEl,
      "data-nlos-landmark",
      this.activeLandmark,
    );
    if (target && typeof (target as { focus?: () => void }).focus === "function") {
      (target as { focus: () => void }).focus();
    }
  }

  public handleKeydown(event: WorkstationKeyEventInput): WorkstationShortcutResult {
    if (event.fromUntrustedSurface) {
      this.recordRejection("keyboard", "UNTRUSTED_ORIGIN_SPOOF");
      return {
        handled: true,
        action: "blocked",
        rejectionCode: "UNTRUSTED_ORIGIN_SPOOF",
      };
    }

    if (event.key === "Escape") {
      if (this.activeZ4Modal !== null) {
        event.preventDefault?.();
        this.cancelZ4Modal();
        return { handled: true, action: "cancel-z4" };
      }
      if (this.commandPaletteOpen) {
        event.preventDefault?.();
        this.setCommandPaletteOpen(false);
        return { handled: true, action: "close-palette" };
      }
      if (this.launcherOpen || this.customZ3Flyouts.size > 0) {
        event.preventDefault?.();
        this.launcherOpen = false;
        this.customZ3Flyouts.clear();
        this.syncSystemOverlaysToWindowManager();
        this.emitAndRefreshDom();
        return { handled: true, action: "close-z3-flyout" };
      }
      if (this.inspectorOpen) {
        event.preventDefault?.();
        this.closeInspector();
        return {
          handled: true,
          action: "close-inspector",
          disclosure: this.disclosureModel.getLevel(),
        };
      }
    }

    if (this.activeZ4Modal !== null) {
      if (event.key === "Enter") {
        event.preventDefault?.();
        const res = this.confirmZ4Modal(event.nowMonoMs);
        if (res.accepted) {
          return { handled: true, action: "confirm-z4" };
        }
        return {
          handled: true,
          action: "z4-arming-rejected",
          rejectionCode: res.rejectionCode ?? "Z4_ARMING_GUARD",
        };
      }
      this.recordRejection("keyboard", "Z4_MODAL_LOCK");
      return {
        handled: true,
        action: "blocked",
        rejectionCode: "Z4_MODAL_LOCK",
      };
    }

    const lowerKey = event.key.toLowerCase();

    if (
      (event.ctrlKey && event.altKey && lowerKey === "l") ||
      (event.metaKey && event.ctrlKey && lowerKey === "q")
    ) {
      event.preventDefault?.();
      this.lockSession();
      return { handled: true, action: "lock-session" };
    }

    if (this.sessionState !== "ACTIVE") {
      this.recordRejection("keyboard", "SESSION_NOT_ACTIVE");
      return {
        handled: true,
        action: "blocked",
        rejectionCode: "SESSION_NOT_ACTIVE",
      };
    }

    if ((event.ctrlKey || event.metaKey) && event.shiftKey && lowerKey === "e") {
      event.preventDefault?.();
      const nextLevel = this.toggleDeepDiagnostics();
      return {
        handled: true,
        action: "toggle-l3-diagnostics",
        disclosure: nextLevel,
      };
    }

    if (event.ctrlKey && !event.altKey && !event.shiftKey && event.key === " ") {
      event.preventDefault?.();
      this.setCommandPaletteOpen(!this.commandPaletteOpen);
      return { handled: true, action: "toggle-command-palette" };
    }

    if (event.key === "F6") {
      event.preventDefault?.();
      const next = this.cycleLandmark(Boolean(event.shiftKey));
      return {
        handled: true,
        action: "cycle-landmark",
        landmark: next,
      };
    }

    const digitMatch =
      event.key === "1" || event.key === "2" || event.key === "3" || event.key === "4"
        ? (Number(event.key) as LayoutShortcutDigit)
        : null;

    if (digitMatch !== null) {
      const hasSystemMod = Boolean(event.metaKey || (event.ctrlKey && event.altKey));
      const bareQuickKey =
        !event.targetIsTextInput &&
        !event.ctrlKey &&
        !event.altKey &&
        !event.metaKey &&
        !event.shiftKey;
      if (hasSystemMod || bareQuickKey) {
        event.preventDefault?.();
        const mode = layoutModeForShortcut(digitMatch);
        this.setLayoutMode(mode);
        return {
          handled: true,
          action: "switch-layout",
          layoutMode: mode,
        };
      }
    }

    if (
      !event.targetIsTextInput &&
      !event.ctrlKey &&
      !event.altKey &&
      !event.metaKey &&
      (event.key === " " || event.key === "Spacebar" || event.key === "Enter") &&
      (this.activeLandmark === "z1-task-space" || event.targetTaskId !== undefined)
    ) {
      event.preventDefault?.();
      this.selectTaskAndToggleInspector(event.targetTaskId);
      return {
        handled: true,
        action: "toggle-l2-inspector",
        disclosure: this.disclosureModel.getLevel(),
      };
    }

    return { handled: false, action: "none" };
  }

  /* ==========================================================================
     Pure Deterministic HTML Shell Rendering & Visible-Text Anti-AI Audit Helper
     ========================================================================== */

  private buildTaskbarItems(): {
    windowTabs: TaskbarWindowTabItem[];
    activeTaskCount: number;
    pendingApprovalCount: number;
  } {
    const wmSnap = this.windowManager.getSnapshot();
    const rec = this.getActivePrincipalSession();
    const windowTabs: TaskbarWindowTabItem[] = wmSnap.windows
      .filter((w) => w.windowState !== "CLOSED")
      .map((w) => ({
        windowId: w.windowId,
        title: w.title,
        icon: w.appIcon,
        focused: wmSnap.focusedWindowId === w.windowId && w.windowState === "VISIBLE",
        minimized: w.windowState === "MINIMIZED",
      }));
    const activeTaskCount = rec.tasks.filter((t) => !t.isTerminal).length;
    const pendingApprovalCount = rec.tasks.filter((t) => Boolean(t.pendingApproval)).length;
    return { windowTabs, activeTaskCount, pendingApprovalCount };
  }

  private resolveSelectedTask(): WorkstationTaskRowInput | null {
    const rec = this.getActivePrincipalSession();
    if (!this.selectedTaskId) {
      return rec.tasks[0] ?? null;
    }
    return rec.tasks.find((t) => t.taskId === this.selectedTaskId) ?? rec.tasks[0] ?? null;
  }

  private renderZ1TaskPaneHtml(): string {
    const wmSnap = this.windowManager.getSnapshot();
    const z1Rect = wmSnap.geometry.z1TaskSpaceRect;
    const copy = COMPOSITOR_COPY[this.locale];
    const disclosure = this.disclosureModel.getLevel();
    const rec = this.getActivePrincipalSession();

    if (!z1Rect) {
      return (
        `<aside class="nlos-plane-z1" data-nlos-plane="Z1" ` +
        `data-nlos-landmark="z1-task-space" data-z-index="${Z1_Z_INDEX}" ` +
        `tabindex="-1" hidden aria-label="${escapeHtml(copy.taskPaneAria)}"></aside>`
      );
    }

    const wsButtonsHtml = rec.workspaces
      .map((ws) => {
        const isCurrent = ws.workspaceIdHex === rec.activeWorkspaceIdHex;
        return (
          `<button type="button" class="nlos-btn${isCurrent ? " nlos-btn--primary" : ""}" ` +
          `data-workspace-id="${escapeHtml(ws.workspaceIdHex)}" ` +
          `aria-pressed="${isCurrent ? "true" : "false"}">` +
          `<span>${escapeHtml(ws.name)}</span>` +
          `</button>`
        );
      })
      .join("");

    const rowsHtml =
      rec.tasks.length === 0
        ? `<tr class="nlos-table-row"><td colspan="5">${escapeHtml(copy.emptyTasks)}</td></tr>`
        : rec.tasks
            .map((task) => {
              const selected = task.taskId === this.selectedTaskId;
              const statusHtml = renderStatusIndicatorHtml({
                state: task.status,
                locale: this.locale,
                disclosure,
              });
              const bulletHtml = renderBulletMicroBarHtml({
                actualUsage: task.actualUsage,
                effectiveGrantedOrLeaseCap: task.effectiveGrantedOrLeaseCap,
                requestedCeiling: task.requestedCeiling,
                unit: task.resourceUnit,
                locale: this.locale,
                disclosure,
              });
              const verbs = resolveTaskRowAllowedVerbs(task, this.locale, disclosure);
              const verbBtns: string[] = [];
              if (verbs.includes("Pause")) {
                verbBtns.push(
                  `<button type="button" class="nlos-btn" data-task-verb="pause" data-task-id="${escapeHtml(task.taskId)}">${escapeHtml(copy.verbPause)}</button>`,
                );
              }
              if (verbs.includes("Resume")) {
                verbBtns.push(
                  `<button type="button" class="nlos-btn" data-task-verb="resume" data-task-id="${escapeHtml(task.taskId)}">${escapeHtml(copy.verbResume)}</button>`,
                );
              }
              if (verbs.includes("Cancel")) {
                verbBtns.push(
                  `<button type="button" class="nlos-btn" data-task-verb="cancel" data-task-id="${escapeHtml(task.taskId)}">${escapeHtml(copy.verbCancel)}</button>`,
                );
              }
              verbBtns.push(
                `<button type="button" class="nlos-btn" data-task-verb="inspect" data-task-id="${escapeHtml(task.taskId)}">${escapeHtml(copy.verbInspect)}</button>`,
              );

              const l3PlanBadge =
                disclosure === "L3" && task.planIdHex
                  ? `<span class="nlos-mono nlos-l3-only"> [${escapeHtml(task.planIdHex)}]</span>`
                  : "";

              return (
                `<tr class="nlos-table-row" tabindex="0" data-task-row-id="${escapeHtml(task.taskId)}" ` +
                `aria-selected="${selected ? "true" : "false"}">` +
                `<td><span>${escapeHtml(task.title)}</span>${l3PlanBadge}</td>` +
                `<td>${statusHtml}</td>` +
                `<td>${bulletHtml}</td>` +
                `<td>${escapeHtml(task.workspaceName ?? rec.workspaces[0]?.name ?? "")}</td>` +
                `<td>${verbBtns.join("")}</td>` +
                `</tr>`
              );
            })
            .join("");

    return (
      `<aside class="nlos-plane-z1" data-nlos-plane="Z1" ` +
      `data-nlos-landmark="z1-task-space" data-z-index="${Z1_Z_INDEX}" tabindex="-1" ` +
      `aria-label="${escapeHtml(copy.taskPaneAria)}" ` +
      `style="left: ${z1Rect.x}px; top: ${z1Rect.y}px; width: ${z1Rect.width}px; height: ${z1Rect.height}px; z-index: ${Z1_Z_INDEX};">` +
      `<header class="nlos-window-titlebar">` +
      `<div class="nlos-wm-titlebar-left">` +
      `<strong class="nlos-wm-title">${escapeHtml(copy.taskPaneTitle)}</strong>` +
      `<div class="nlos-wm-controls">${wsButtonsHtml}</div>` +
      `</div>` +
      `<div class="nlos-wm-controls">` +
      `<button type="button" class="nlos-btn" data-compositor-action="toggle-l3">${escapeHtml(copy.l3ToggleBtn)}</button>` +
      `</div>` +
      `</header>` +
      `<div class="nlos-z1-scroll" data-scroll-top="${this.z1ScrollTop}" style="flex: 1 1 auto; overflow: auto;">` +
      `<table class="nlos-table">` +
      `<thead><tr>` +
      `<th>${escapeHtml(copy.colTask)}</th>` +
      `<th>${escapeHtml(copy.colStatus)}</th>` +
      `<th>${escapeHtml(copy.colResource)}</th>` +
      `<th>${escapeHtml(copy.colWorkspace)}</th>` +
      `<th>${escapeHtml(copy.colActions)}</th>` +
      `</tr></thead>` +
      `<tbody>${rowsHtml}</tbody>` +
      `</table>` +
      `</div>` +
      `</aside>`
    );
  }

  private renderZ2WithLandmarkHtml(): string {
    const wmSnap = this.windowManager.getSnapshot();
    let rawZ2 = this.windowManager.renderZ2PlaneHtml();
    rawZ2 = rawZ2.replace(
      `<div class="nlos-plane-z2"`,
      `<div class="nlos-plane-z2" data-nlos-plane="Z2" data-z-index-range="${Z2_Z_INDEX_MIN}-${Z2_Z_INDEX_MAX}"`,
    );
    if (wmSnap.focusedWindowId) {
      const targetMarker = `data-window-id="${escapeHtml(wmSnap.focusedWindowId)}"`;
      rawZ2 = rawZ2.replace(
        targetMarker,
        `${targetMarker} data-nlos-landmark="z2-active-window" tabindex="-1"`,
      );
    }
    return rawZ2;
  }

  private renderL2InspectorHtml(): string {
    const disclosure = this.disclosureModel.getLevel();
    if (!this.inspectorOpen || (disclosure !== "L2" && disclosure !== "L3")) {
      return "";
    }
    const copy = COMPOSITOR_COPY[this.locale];
    const selected = this.resolveSelectedTask();
    const rec = this.getActivePrincipalSession();
    const breadcrumbs: ScopeBreadcrumbItem[] =
      selected?.breadcrumbs && selected.breadcrumbs.length > 0
        ? [...selected.breadcrumbs]
        : [
            {
              kind: "Workspace",
              id: rec.activeWorkspaceIdHex,
              label:
                rec.workspaces.find(
                  (w) => w.workspaceIdHex === rec.activeWorkspaceIdHex,
                )?.name ?? "Workspace",
            },
            ...(selected
              ? [{ kind: "Task" as const, id: selected.taskId, label: selected.title }]
              : []),
          ];

    const drawerShell = renderInspectorDrawerHtml({
      open: true,
      placement: "right",
      disclosure: disclosure === "L3" ? "L3" : "L2",
      locale: this.locale,
      breadcrumbs,
    });

    const detailBody = selected
      ? `<div class="nlos-inspector-summary">` +
        `<h4>${escapeHtml(copy.inspectorHeading)}: ${escapeHtml(selected.title)}</h4>` +
        `<div><span>${escapeHtml(copy.inspectorStatusLabel)}: </span>` +
        `${renderStatusIndicatorHtml({ state: selected.status, locale: this.locale, disclosure })}</div>` +
        `<div><span>${escapeHtml(copy.inspectorResourceLabel)}: </span>` +
        `${renderBulletMicroBarHtml({
          actualUsage: selected.actualUsage,
          effectiveGrantedOrLeaseCap: selected.effectiveGrantedOrLeaseCap,
          requestedCeiling: selected.requestedCeiling,
          unit: selected.resourceUnit,
          locale: this.locale,
          disclosure,
        })}</div>` +
        (selected.summaryNote
          ? `<p>${escapeHtml(copy.inspectorSummaryLabel)}: ${escapeHtml(selected.summaryNote)}</p>`
          : "") +
        `</div>`
      : "";

    const bodyHook = `<div class="nlos-inspector-drawer__body"></div>`;
    const mergedDrawer = drawerShell.includes(bodyHook)
      ? drawerShell.replace(
          bodyHook,
          `<div class="nlos-inspector-drawer__body">${detailBody}</div>`,
        )
      : `${drawerShell}${detailBody}`;

    return (
      `<div class="nlos-l2-up" data-nlos-landmark="z2-inspector-drawer" tabindex="-1" ` +
      `style="position: absolute; top: 0; right: 0; bottom: 0; z-index: ${Z2_Z_INDEX_MAX};">` +
      `${mergedDrawer}` +
      `</div>`
    );
  }

  private renderZ4PlaneHtml(): string {
    if (!this.isZ4Active()) {
      return (
        `<div class="nlos-plane-z4-layer" data-nlos-plane="Z4" ` +
        `data-z-index="${Z4_SECURITY_Z_INDEX}" style="z-index: ${Z4_SECURITY_Z_INDEX};" hidden></div>`
      );
    }

    const copy = COMPOSITOR_COPY[this.locale];
    const rec = this.getActivePrincipalSession();
    const sealTitle = `${copy.sealCaption}: ${rec.sealPhrase}`;
    const sealHtml = renderZ4SealSvgHtml(sealTitle);

    if (this.activeZ4Modal) {
      const modal = this.activeZ4Modal;
      const blastHtml =
        modal.blastRadiusItems.length > 0
          ? renderBlastRadiusDiffHtml({
              rows: modal.blastRadiusItems,
              locale: this.locale,
              disclosure: this.disclosureModel.getLevel(),
            })
          : "";
      const lockHtml = modal.statutoryLock
        ? renderStatutoryLockBadgeHtml({
            kind: modal.statutoryLock,
            locale: this.locale,
            disclosure: this.disclosureModel.getLevel(),
          })
        : "";
      const confirmLabel = modal.confirmLabel ?? copy.z4ConfirmBtn;
      const cancelLabel = modal.cancelLabel ?? copy.z4CancelBtn;

      return (
        `<div class="nlos-plane-z4-layer" data-nlos-plane="Z4" ` +
        `data-z-index="${Z4_SECURITY_Z_INDEX}" style="z-index: ${Z4_SECURITY_Z_INDEX};">` +
        `<section role="alertdialog" aria-modal="true" class="nlos-surface-modal-z4" ` +
        `data-z4-prompt-id="${escapeHtml(modal.promptId)}" style="padding: 16px;">` +
        `<header style="display: flex; align-items: center; justify-content: space-between; gap: 12px; margin-bottom: 12px;">` +
        `<div>` +
        `<strong>${escapeHtml(modal.title || copy.z4DefaultHeading)}</strong>` +
        `<div><span>${escapeHtml(copy.sealCaption)}: </span><span>${escapeHtml(rec.sealPhrase)}</span></div>` +
        `</div>` +
        `${sealHtml}` +
        `</header>` +
        (modal.summary ? `<p>${escapeHtml(modal.summary)}</p>` : "") +
        `${blastHtml}` +
        `${lockHtml}` +
        `<p data-z4-arming-ms="${modal.armingWindowMs}">${escapeHtml(copy.z4ArmingHint)}</p>` +
        `<footer style="display: flex; justify-content: flex-end; gap: 8px; margin-top: 12px;">` +
        `<button type="button" class="nlos-btn" data-z4-action="cancel">${escapeHtml(cancelLabel)}</button>` +
        `<button type="button" class="nlos-btn nlos-btn--primary" data-z4-action="confirm"${modal.statutoryLock ? " disabled" : ""}>${escapeHtml(confirmLabel)}</button>` +
        `</footer>` +
        `</section>` +
        `</div>`
      );
    }

    return (
      `<div class="nlos-plane-z4-layer" data-nlos-plane="Z4" ` +
      `data-z-index="${Z4_SECURITY_Z_INDEX}" style="z-index: ${Z4_SECURITY_Z_INDEX};">` +
      `<section role="dialog" aria-modal="true" class="nlos-surface-modal-z4" ` +
      `data-z4-mode="session-lock" style="padding: 16px;">` +
      `<header style="display: flex; align-items: center; justify-content: space-between; gap: 12px; margin-bottom: 12px;">` +
      `<div>` +
      `<strong>${escapeHtml(copy.lockHeading)}</strong>` +
      `<div>${escapeHtml(rec.profile.displayName)}</div>` +
      `</div>` +
      `${sealHtml}` +
      `</header>` +
      `<p>${escapeHtml(copy.lockSubtitle)}</p>` +
      `<p><span>${escapeHtml(copy.sealCaption)}: </span><strong>${escapeHtml(rec.sealPhrase)}</strong></p>` +
      `<footer style="display: flex; justify-content: flex-end; gap: 8px; margin-top: 12px;">` +
      `<button type="button" class="nlos-btn nlos-btn--primary" data-z4-action="unlock">${escapeHtml(copy.unlockBtn)}</button>` +
      `</footer>` +
      `</section>` +
      `</div>`
    );
  }

  public renderShellHtml(): string {
    const wmSnap = this.windowManager.getSnapshot();
    const stageHeight = Math.max(0, wmSnap.viewportHeight - TASKBAR_HEIGHT_PX);
    const disclosure = this.disclosureModel.getLevel();
    const copy = COMPOSITOR_COPY[this.locale];
    const { windowTabs, activeTaskCount, pendingApprovalCount } =
      this.buildTaskbarItems();

    const z0Html =
      `<div class="nlos-plane-z0" data-nlos-plane="Z0" ` +
      `data-z-index="${Z0_Z_INDEX}" style="z-index: ${Z0_Z_INDEX};" aria-hidden="true"></div>`;
    const z1Html = this.renderZ1TaskPaneHtml();
    const z2Html = this.renderZ2WithLandmarkHtml();
    const l2Html = this.renderL2InspectorHtml();
    const l3Html =
      disclosure === "L3"
        ? `<section class="nlos-l3-only nlos-l3-diagnostic-host" data-nlos-slot="l3-diagnostic-inspector" ` +
          `aria-label="${escapeHtml(copy.l3HostTitle)}" ` +
          `style="position: absolute; inset: 0; z-index: ${Z2_Z_INDEX_MAX}; background-color: var(--nlos-bg-window); overflow: auto;"></section>`
        : `<section class="nlos-l3-only nlos-l3-diagnostic-host" data-nlos-slot="l3-diagnostic-inspector" hidden></section>`;

    const rawTaskbarHtml = renderBottomTaskbarHtml({
      windows: windowTabs,
      activeTaskCount,
      pendingApprovalCount,
      taskSpaceFocused: this.activeLandmark === "z1-task-space",
      launcherOpen: this.launcherOpen,
      z4ModalLock: this.isZ4BlockingInput(),
      layoutMode: wmSnap.layoutMode,
      connectivity: this.connectivity,
      disclosure,
      theme: this.theme,
      hardwareCapture: this.hardwareCapture,
      unreadNotificationCount: this.unreadNotificationCount,
      clockText: this.clockText,
      locale: this.locale,
    }).replace(
      `class="nlos-plane-z3-taskbar"`,
      `class="nlos-plane-z3-taskbar" data-nlos-plane="Z3" data-nlos-landmark="z3-bottom-taskbar" data-z-index="${Z3_TASKBAR_Z_INDEX}" tabindex="-1"`,
    );

    const flyoutVisible = this.launcherOpen || this.customZ3Flyouts.size > 0;
    const launcherHtml = this.launcherOpen
      ? renderLauncherMenuHtml({
          model: this.launcherModel,
          locale: this.locale,
          disclosure,
        })
      : "";
    const z3FlyoutLayerHtml =
      `<div class="nlos-plane-z3-flyout-layer" data-nlos-plane="Z3" ` +
      `data-nlos-slot="z3-system-flyouts" data-z-index="${Z3_FLYOUT_Z_INDEX}" ` +
      `style="z-index: ${Z3_FLYOUT_Z_INDEX};"${flyoutVisible ? "" : " hidden"}>` +
      `${launcherHtml}` +
      `</div>`;

    const z3PaletteLayerHtml =
      `<div class="nlos-plane-z3-palette-layer" data-nlos-plane="Z3" ` +
      `data-nlos-slot="z3-command-palette" data-z-index="${Z3_PALETTE_Z_INDEX}" ` +
      `style="z-index: ${Z3_PALETTE_Z_INDEX};"${this.commandPaletteOpen ? "" : " hidden"}>` +
      (this.commandPaletteOpen
        ? `<div class="nlos-surface-elevated" style="padding: 12px;">${escapeHtml(copy.paletteSlotPlaceholder)}</div>`
        : "") +
      `</div>`;

    const z4Html = this.renderZ4PlaneHtml();

    return (
      `<div class="nlos-workstation-shell" ` +
      `data-nlos-theme="${this.theme}" ` +
      `data-nlos-disclosure="${disclosure}" ` +
      `data-nlos-session-state="${this.sessionState}" ` +
      `data-nlos-session-generation="${this.sessionGeneration}" ` +
      `data-nlos-active-landmark="${this.activeLandmark}">` +
      `<div class="nlos-stage-container" data-stage-height="${stageHeight}" style="height: ${stageHeight}px;">` +
      `${z0Html}${z1Html}${z2Html}${l2Html}${l3Html}` +
      `</div>` +
      `${rawTaskbarHtml}` +
      `${z3FlyoutLayerHtml}` +
      `${z3PaletteLayerHtml}` +
      `${z4Html}` +
      `</div>`
    );
  }

  /**
   * Extracts currently visible user-facing text for deterministic ANTI-AI-07 L0/L1 audits.
   */
  public getVisibleTextForAudit(): string {
    const disclosure = this.disclosureModel.getLevel();
    if (this.mountedRootEl) {
      return collectVisibleDomText(this.mountedRootEl, disclosure);
    }
    const html = this.renderShellHtml();
    const withoutHiddenBlocks = html
      .replace(/<([a-z0-9-]+)\b[^>]*\bhidden\b[^>]*>[\s\S]*?<\/\1>/gi, " ")
      .replace(/<[^>]*>/g, " ")
      .replace(/\s+/g, " ")
      .trim();
    return withoutHiddenBlocks;
  }

  /* ==========================================================================
     Live DOM Mount & Incremental Rerender (`mount` / `unmount`)
     ========================================================================== */

  public mount(container: HTMLElement, options?: { doc?: Document }): HTMLElement {
    const doc = options?.doc ?? this.defaultDoc ?? document;
    this.mountedContainer = container;
    this.mountedDoc = doc;

    if (typeof document !== "undefined" && doc === document && document.documentElement) {
      document.documentElement.setAttribute("data-nlos-theme", this.theme);
      document.documentElement.setAttribute(
        "data-nlos-disclosure",
        this.disclosureModel.getLevel(),
      );
    }

    this.rerenderMountedDom();
    return this.mountedRootEl!;
  }

  public unmount(): void {
    if (this.activeZ4SealHandle) {
      this.activeZ4SealHandle.unmount();
      this.activeZ4SealHandle = null;
    }
    if (this.mountedContainer) {
      this.mountedContainer.replaceChildren();
    }
    this.mountedContainer = null;
    this.mountedRootEl = null;
    this.mountedDoc = null;
  }

  private rerenderMountedDom(): void {
    const doc = this.mountedDoc;
    const container = this.mountedContainer;
    if (!doc || !container) {
      return;
    }

    if (this.activeZ4SealHandle) {
      this.activeZ4SealHandle.unmount();
      this.activeZ4SealHandle = null;
    }

    const wmSnap = this.windowManager.getSnapshot();
    const stageHeight = Math.max(0, wmSnap.viewportHeight - TASKBAR_HEIGHT_PX);
    const disclosure = this.disclosureModel.getLevel();
    const copy = COMPOSITOR_COPY[this.locale];
    const rec = this.getActivePrincipalSession();

    const shell = doc.createElement("div");
    shell.className = "nlos-workstation-shell";
    shell.setAttribute("data-nlos-theme", this.theme);
    shell.setAttribute("data-nlos-disclosure", disclosure);
    shell.setAttribute("data-nlos-session-state", this.sessionState);
    shell.setAttribute("data-nlos-session-generation", String(this.sessionGeneration));
    shell.setAttribute("data-nlos-active-landmark", this.activeLandmark);

    const stage = doc.createElement("div");
    stage.className = "nlos-stage-container";
    stage.setAttribute("data-stage-height", String(stageHeight));
    stage.style.height = `${stageHeight}px`;

    // Z0 Backdrop
    const z0 = doc.createElement("div");
    z0.className = "nlos-plane-z0";
    z0.setAttribute("data-nlos-plane", "Z0");
    z0.setAttribute("data-z-index", String(Z0_Z_INDEX));
    z0.setAttribute("aria-hidden", "true");
    z0.style.zIndex = String(Z0_Z_INDEX);
    stage.appendChild(z0);

    // Z1 Docked Task Space Pane
    const z1 = this.createZ1Element(doc, wmSnap.geometry.z1TaskSpaceRect, disclosure);
    stage.appendChild(z1);

    // Z2 Tiling & Floating Window Layer
    const z2 = this.windowManager.renderZ2PlaneElement({ doc });
    z2.setAttribute("data-nlos-plane", "Z2");
    z2.setAttribute("data-z-index-range", `${Z2_Z_INDEX_MIN}-${Z2_Z_INDEX_MAX}`);
    if (wmSnap.focusedWindowId) {
      const focusedFrame = findFirstByAttr(z2, "data-window-id", wmSnap.focusedWindowId);
      if (focusedFrame) {
        focusedFrame.setAttribute("data-nlos-landmark", "z2-active-window");
        focusedFrame.tabIndex = -1;
      }
    }
    stage.appendChild(z2);

    // L2 Non-Modal Inspector Drawer
    if (this.inspectorOpen && (disclosure === "L2" || disclosure === "L3")) {
      const l2Wrap = this.createL2DrawerElement(doc, disclosure);
      stage.appendChild(l2Wrap);
    }

    // L3 Deep Diagnostic Inspector Host (hosts Stage B/C diagnostic views strictly in L3)
    const l3Host = doc.createElement("section");
    l3Host.className = "nlos-l3-only nlos-l3-diagnostic-host";
    l3Host.setAttribute("data-nlos-slot", "l3-diagnostic-inspector");
    l3Host.setAttribute("aria-label", copy.l3HostTitle);
    if (disclosure === "L3") {
      l3Host.hidden = false;
      l3Host.style.position = "absolute";
      l3Host.style.inset = "0";
      l3Host.style.zIndex = String(Z2_Z_INDEX_MAX);
      l3Host.style.backgroundColor = "var(--nlos-bg-window)";
      l3Host.style.overflow = "auto";
      const l3Bar = doc.createElement("header");
      l3Bar.className = "nlos-window-titlebar";
      const l3Title = doc.createElement("strong");
      l3Title.className = "nlos-wm-title";
      l3Title.textContent = copy.l3HostTitle;
      const closeL3Btn = doc.createElement("button");
      closeL3Btn.type = "button";
      closeL3Btn.className = "nlos-btn";
      closeL3Btn.textContent =
        this.locale === "en-US"
          ? "Exit Diagnostics (Ctrl+Shift+E)"
          : "退出深度诊断 (Ctrl+Shift+E)";
      closeL3Btn.addEventListener("click", () => {
        this.toggleDeepDiagnostics();
      });
      l3Bar.appendChild(l3Title);
      l3Bar.appendChild(closeL3Btn);
      l3Host.appendChild(l3Bar);
      if (this.l3DiagnosticElement) {
        l3Host.appendChild(this.l3DiagnosticElement);
      }
    } else {
      l3Host.hidden = true;
      l3Host.setAttribute("hidden", "");
    }
    stage.appendChild(l3Host);
    shell.appendChild(stage);

    // Z3 Persistent 36px Bottom Taskbar
    const { windowTabs, activeTaskCount, pendingApprovalCount } =
      this.buildTaskbarItems();
    const taskbarEl = createBottomTaskbarElement({
      doc,
      windows: windowTabs,
      activeTaskCount,
      pendingApprovalCount,
      taskSpaceFocused: this.activeLandmark === "z1-task-space",
      launcherOpen: this.launcherOpen,
      z4ModalLock: this.isZ4BlockingInput(),
      layoutMode: wmSnap.layoutMode,
      connectivity: this.connectivity,
      disclosure,
      theme: this.theme,
      hardwareCapture: this.hardwareCapture,
      unreadNotificationCount: this.unreadNotificationCount,
      clockText: this.clockText,
      locale: this.locale,
      callbacks: {
        onToggleLauncher: () => {
          this.toggleLauncher();
        },
        onOpenCommandPalette: () => {
          this.setCommandPaletteOpen(!this.commandPaletteOpen);
        },
        onActivateWindowTab: (windowId) => {
          const w = wmSnap.windows.find((item) => item.windowId === windowId);
          if (!w) {
            return;
          }
          this.isInternalWmMutation = true;
          try {
            if (w.windowState === "MINIMIZED") {
              this.windowManager.restoreWindow(windowId);
            } else {
              this.windowManager.focusWindow(windowId);
            }
          } finally {
            this.isInternalWmMutation = false;
          }
          this.activeLandmark = "z2-active-window";
          this.emitAndRefreshDom();
        },
        onToggleTaskSpace: () => {
          const nextVisible = !wmSnap.showZ1TaskSpace;
          this.isInternalWmMutation = true;
          try {
            this.windowManager.setShowZ1TaskSpace(nextVisible);
          } finally {
            this.isInternalWmMutation = false;
          }
          if (nextVisible) {
            this.activeLandmark = "z1-task-space";
          }
          this.emitAndRefreshDom();
        },
        onSelectLayoutMode: (nextMode) => {
          this.setLayoutMode(nextMode);
        },
        onCycleDisclosure: (nextDisclosure) => {
          this.setDisclosure(nextDisclosure);
        },
        onToggleTheme: (nextTheme) => {
          this.setTheme(nextTheme);
        },
        onOpenNotifications: () => {
          const isOpen = this.customZ3Flyouts.has("notifications");
          this.setZ3FlyoutOpen("notifications", !isOpen);
        },
      },
    });
    taskbarEl.setAttribute("data-nlos-plane", "Z3");
    taskbarEl.setAttribute("data-nlos-landmark", "z3-bottom-taskbar");
    taskbarEl.setAttribute("data-z-index", String(Z3_TASKBAR_Z_INDEX));
    taskbarEl.tabIndex = -1;
    shell.appendChild(taskbarEl);

    // Z3 Flyouts Layer (430)
    const z3FlyoutLayer = doc.createElement("div");
    z3FlyoutLayer.className = "nlos-plane-z3-flyout-layer";
    z3FlyoutLayer.setAttribute("data-nlos-plane", "Z3");
    z3FlyoutLayer.setAttribute("data-nlos-slot", "z3-system-flyouts");
    z3FlyoutLayer.setAttribute("data-z-index", String(Z3_FLYOUT_Z_INDEX));
    z3FlyoutLayer.style.zIndex = String(Z3_FLYOUT_Z_INDEX);
    const flyoutOpen = this.launcherOpen || this.customZ3Flyouts.size > 0;
    if (flyoutOpen) {
      z3FlyoutLayer.hidden = false;
      if (this.launcherOpen) {
        const flyoutRect = computeLauncherFlyoutRect(
          wmSnap.viewportWidth,
          wmSnap.viewportHeight,
        );
        const launcherEl = createLauncherMenuElement({
          doc,
          model: this.launcherModel,
          locale: this.locale,
          disclosure,
          callbacks: {
            onLaunch: (prepared) => {
              this.launchApplication({
                applicationIdHex: prepared.applicationIdHex,
                installationIdHex: prepared.installationIdHex,
              });
            },
            onDisableApplication: (target) => {
              this.callbacks?.onDisableApplication?.(target);
            },
            onUninstallApplication: (target) => {
              this.callbacks?.onUninstallApplication?.(target);
            },
            onClose: () => {
              this.setLauncherOpen(false);
            },
          },
        });
        launcherEl.style.position = "absolute";
        launcherEl.style.left = `${flyoutRect.x}px`;
        launcherEl.style.top = `${flyoutRect.y}px`;
        z3FlyoutLayer.appendChild(launcherEl);
      }
      if (this.customZ3Flyouts.size > 0 && this.customFlyoutElement) {
        z3FlyoutLayer.appendChild(this.customFlyoutElement);
      }
    } else {
      z3FlyoutLayer.hidden = true;
      z3FlyoutLayer.setAttribute("hidden", "");
    }
    shell.appendChild(z3FlyoutLayer);

    // Z3 Command Palette Layer (460)
    const z3PaletteLayer = doc.createElement("div");
    z3PaletteLayer.className = "nlos-plane-z3-palette-layer";
    z3PaletteLayer.setAttribute("data-nlos-plane", "Z3");
    z3PaletteLayer.setAttribute("data-nlos-slot", "z3-command-palette");
    z3PaletteLayer.setAttribute("data-z-index", String(Z3_PALETTE_Z_INDEX));
    z3PaletteLayer.style.zIndex = String(Z3_PALETTE_Z_INDEX);
    if (this.commandPaletteOpen) {
      z3PaletteLayer.hidden = false;
      if (this.customPaletteElement) {
        z3PaletteLayer.appendChild(this.customPaletteElement);
      } else {
        const placeholder = doc.createElement("div");
        placeholder.className = "nlos-surface-elevated";
        placeholder.style.padding = "12px";
        placeholder.textContent = copy.paletteSlotPlaceholder;
        z3PaletteLayer.appendChild(placeholder);
      }
    } else {
      z3PaletteLayer.hidden = true;
      z3PaletteLayer.setAttribute("hidden", "");
    }
    shell.appendChild(z3PaletteLayer);

    // Z4 Non-Occludable Trusted Security & Session Lock Plane (900)
    const z4Layer = this.createZ4Element(doc, rec, disclosure);
    shell.appendChild(z4Layer);

    this.mountedRootEl = shell;
    container.replaceChildren(shell);
  }

  private createZ1Element(
    doc: Document,
    z1Rect: WmRectPx | null,
    disclosure: DisclosureLevel,
  ): HTMLElement {
    const copy = COMPOSITOR_COPY[this.locale];
    const rec = this.getActivePrincipalSession();
    const aside = doc.createElement("aside");
    aside.className = "nlos-plane-z1";
    aside.setAttribute("data-nlos-plane", "Z1");
    aside.setAttribute("data-nlos-landmark", "z1-task-space");
    aside.setAttribute("data-z-index", String(Z1_Z_INDEX));
    aside.setAttribute("aria-label", copy.taskPaneAria);
    aside.tabIndex = -1;

    if (!z1Rect) {
      aside.hidden = true;
      aside.setAttribute("hidden", "");
      return aside;
    }

    aside.style.left = `${z1Rect.x}px`;
    aside.style.top = `${z1Rect.y}px`;
    aside.style.width = `${z1Rect.width}px`;
    aside.style.height = `${z1Rect.height}px`;
    aside.style.zIndex = String(Z1_Z_INDEX);

    const header = doc.createElement("header");
    header.className = "nlos-window-titlebar";

    const left = doc.createElement("div");
    left.className = "nlos-wm-titlebar-left";
    const title = doc.createElement("strong");
    title.className = "nlos-wm-title";
    title.textContent = copy.taskPaneTitle;
    left.appendChild(title);

    const wsBar = doc.createElement("div");
    wsBar.className = "nlos-wm-controls";
    for (const ws of rec.workspaces) {
      const btn = doc.createElement("button");
      btn.type = "button";
      const isCurrent = ws.workspaceIdHex === rec.activeWorkspaceIdHex;
      btn.className = isCurrent ? "nlos-btn nlos-btn--primary" : "nlos-btn";
      btn.setAttribute("data-workspace-id", ws.workspaceIdHex);
      btn.setAttribute("aria-pressed", isCurrent ? "true" : "false");
      btn.textContent = ws.name;
      btn.addEventListener("click", () => {
        this.switchWorkspace(ws.workspaceIdHex);
      });
      wsBar.appendChild(btn);
    }
    left.appendChild(wsBar);

    const right = doc.createElement("div");
    right.className = "nlos-wm-controls";
    const l3Btn = doc.createElement("button");
    l3Btn.type = "button";
    l3Btn.className = "nlos-btn";
    l3Btn.setAttribute("data-compositor-action", "toggle-l3");
    l3Btn.textContent = copy.l3ToggleBtn;
    l3Btn.addEventListener("click", () => {
      this.toggleDeepDiagnostics();
    });
    right.appendChild(l3Btn);

    header.appendChild(left);
    header.appendChild(right);
    aside.appendChild(header);

    const scrollBox = doc.createElement("div");
    scrollBox.className = "nlos-z1-scroll";
    scrollBox.style.flex = "1 1 auto";
    scrollBox.style.overflow = "auto";

    const table = doc.createElement("table");
    table.className = "nlos-table";
    const thead = doc.createElement("thead");
    const headRow = doc.createElement("tr");
    for (const colLabel of [
      copy.colTask,
      copy.colStatus,
      copy.colResource,
      copy.colWorkspace,
      copy.colActions,
    ]) {
      const th = doc.createElement("th");
      th.textContent = colLabel;
      headRow.appendChild(th);
    }
    thead.appendChild(headRow);
    table.appendChild(thead);

    const tbody = doc.createElement("tbody");
    if (rec.tasks.length === 0) {
      const tr = doc.createElement("tr");
      tr.className = "nlos-table-row";
      const td = doc.createElement("td");
      td.setAttribute("colspan", "5");
      td.textContent = copy.emptyTasks;
      tr.appendChild(td);
      tbody.appendChild(tr);
    } else {
      for (const task of rec.tasks) {
        const tr = doc.createElement("tr");
        tr.className = "nlos-table-row";
        tr.tabIndex = 0;
        tr.setAttribute("data-task-row-id", task.taskId);
        const selected = task.taskId === this.selectedTaskId;
        tr.setAttribute("aria-selected", selected ? "true" : "false");
        tr.addEventListener("click", () => {
          this.activeLandmark = "z1-task-space";
          this.selectTaskAndToggleInspector(task.taskId);
        });

        const tdTitle = doc.createElement("td");
        const titleSpan = doc.createElement("span");
        titleSpan.textContent = task.title;
        tdTitle.appendChild(titleSpan);
        if (disclosure === "L3" && task.planIdHex) {
          const planBadge = doc.createElement("span");
          planBadge.className = "nlos-mono nlos-l3-only";
          planBadge.textContent = ` [${task.planIdHex}]`;
          tdTitle.appendChild(planBadge);
        }

        const tdStatus = doc.createElement("td");
        tdStatus.appendChild(
          createStatusIndicatorElement({
            state: task.status,
            doc,
            locale: this.locale,
            disclosure,
          }),
        );

        const tdResource = doc.createElement("td");
        tdResource.appendChild(
          createBulletMicroBarElement({
            doc,
            actualUsage: task.actualUsage,
            effectiveGrantedOrLeaseCap: task.effectiveGrantedOrLeaseCap,
            requestedCeiling: task.requestedCeiling,
            unit: task.resourceUnit,
            locale: this.locale,
            disclosure,
          }),
        );

        const tdWs = doc.createElement("td");
        tdWs.textContent = task.workspaceName ?? rec.workspaces[0]?.name ?? "";

        const tdActions = doc.createElement("td");
        const verbs = resolveTaskRowAllowedVerbs(task, this.locale, disclosure);

        const addVerbBtn = (
          verb: "pause" | "resume" | "cancel",
          label: string,
        ): void => {
          const vBtn = doc.createElement("button");
          vBtn.type = "button";
          vBtn.className = "nlos-btn";
          vBtn.setAttribute("data-task-verb", verb);
          vBtn.setAttribute("data-task-id", task.taskId);
          vBtn.textContent = label;
          vBtn.addEventListener("click", (ev) => {
            if (typeof (ev as { stopPropagation?: () => void }).stopPropagation === "function") {
              (ev as { stopPropagation: () => void }).stopPropagation();
            }
            this.callbacks?.onTaskVerb?.(task.taskId, verb);
          });
          tdActions.appendChild(vBtn);
        };

        if (verbs.includes("Pause")) {
          addVerbBtn("pause", copy.verbPause);
        }
        if (verbs.includes("Resume")) {
          addVerbBtn("resume", copy.verbResume);
        }
        if (verbs.includes("Cancel")) {
          addVerbBtn("cancel", copy.verbCancel);
        }

        const inspectBtn = doc.createElement("button");
        inspectBtn.type = "button";
        inspectBtn.className = "nlos-btn";
        inspectBtn.setAttribute("data-task-verb", "inspect");
        inspectBtn.setAttribute("data-task-id", task.taskId);
        inspectBtn.textContent = copy.verbInspect;
        inspectBtn.addEventListener("click", (ev) => {
          if (typeof (ev as { stopPropagation?: () => void }).stopPropagation === "function") {
            (ev as { stopPropagation: () => void }).stopPropagation();
          }
          this.selectTaskAndToggleInspector(task.taskId);
        });
        tdActions.appendChild(inspectBtn);

        tr.appendChild(tdTitle);
        tr.appendChild(tdStatus);
        tr.appendChild(tdResource);
        tr.appendChild(tdWs);
        tr.appendChild(tdActions);
        tbody.appendChild(tr);
      }
    }

    table.appendChild(tbody);
    scrollBox.appendChild(table);
    aside.appendChild(scrollBox);
    return aside;
  }

  private createL2DrawerElement(
    doc: Document,
    disclosure: "L2" | "L3",
  ): HTMLElement {
    const copy = COMPOSITOR_COPY[this.locale];
    const selected = this.resolveSelectedTask();
    const rec = this.getActivePrincipalSession();

    const breadcrumbs: ScopeBreadcrumbItem[] =
      selected?.breadcrumbs && selected.breadcrumbs.length > 0
        ? [...selected.breadcrumbs]
        : [
            {
              kind: "Workspace",
              id: rec.activeWorkspaceIdHex,
              label:
                rec.workspaces.find((w) => w.workspaceIdHex === rec.activeWorkspaceIdHex)
                  ?.name ?? "Workspace",
            },
            ...(selected
              ? [{ kind: "Task" as const, id: selected.taskId, label: selected.title }]
              : []),
          ];

    const wrap = doc.createElement("div");
    wrap.className = "nlos-l2-up";
    wrap.setAttribute("data-nlos-landmark", "z2-inspector-drawer");
    wrap.tabIndex = -1;
    wrap.style.position = "absolute";
    wrap.style.top = "0";
    wrap.style.right = "0";
    wrap.style.bottom = "0";
    wrap.style.zIndex = String(Z2_Z_INDEX_MAX);

    const drawerCtrl = createInspectorDrawerElement({
      doc,
      open: true,
      placement: "right",
      disclosure,
      locale: this.locale,
      breadcrumbs,
      onToggleDisclosure: (next) => {
        this.setDisclosure(next);
      },
      onClose: () => {
        this.closeInspector();
      },
    });

    if (selected) {
      const summaryBox = doc.createElement("div");
      summaryBox.className = "nlos-inspector-summary";

      const h4 = doc.createElement("h4");
      h4.textContent = `${copy.inspectorHeading}: ${selected.title}`;
      summaryBox.appendChild(h4);

      const statusRow = doc.createElement("div");
      const statusLabel = doc.createElement("span");
      statusLabel.textContent = `${copy.inspectorStatusLabel}: `;
      statusRow.appendChild(statusLabel);
      statusRow.appendChild(
        createStatusIndicatorElement({
          state: selected.status,
          doc,
          locale: this.locale,
          disclosure,
        }),
      );
      summaryBox.appendChild(statusRow);

      const resRow = doc.createElement("div");
      const resLabel = doc.createElement("span");
      resLabel.textContent = `${copy.inspectorResourceLabel}: `;
      resRow.appendChild(resLabel);
      resRow.appendChild(
        createBulletMicroBarElement({
          doc,
          actualUsage: selected.actualUsage,
          effectiveGrantedOrLeaseCap: selected.effectiveGrantedOrLeaseCap,
          requestedCeiling: selected.requestedCeiling,
          unit: selected.resourceUnit,
          locale: this.locale,
          disclosure,
        }),
      );
      summaryBox.appendChild(resRow);

      if (selected.summaryNote) {
        const noteP = doc.createElement("p");
        noteP.textContent = `${copy.inspectorSummaryLabel}: ${selected.summaryNote}`;
        summaryBox.appendChild(noteP);
      }

      const openL3Btn = doc.createElement("button");
      openL3Btn.type = "button";
      openL3Btn.className = "nlos-btn";
      openL3Btn.textContent = copy.inspectorOpenL3Btn;
      openL3Btn.addEventListener("click", () => {
        this.setDisclosure("L3");
      });
      summaryBox.appendChild(openL3Btn);

      drawerCtrl.bodyElement.appendChild(summaryBox);
    }

    wrap.appendChild(drawerCtrl.element);
    return wrap;
  }

  private createZ4Element(
    doc: Document,
    rec: PrincipalPrivateSessionRecord,
    disclosure: DisclosureLevel,
  ): HTMLElement {
    const z4Layer = doc.createElement("div");
    z4Layer.className = "nlos-plane-z4-layer";
    z4Layer.setAttribute("data-nlos-plane", "Z4");
    z4Layer.setAttribute("data-z-index", String(Z4_SECURITY_Z_INDEX));
    z4Layer.style.zIndex = String(Z4_SECURITY_Z_INDEX);

    if (!this.isZ4Active()) {
      z4Layer.hidden = true;
      z4Layer.setAttribute("hidden", "");
      return z4Layer;
    }

    const copy = COMPOSITOR_COPY[this.locale];
    const sealTitle = `${copy.sealCaption}: ${rec.sealPhrase}`;
    const sealHandle = createVerificationSeal({
      doc,
      reducedMotion: true,
      ariaLabel: sealTitle,
    });
    sealHandle.element.setAttribute("title", sealTitle);
    sealHandle.element.setAttribute("data-nlos-seal", "personal-verification-seal");
    this.activeZ4SealHandle = sealHandle;

    if (this.activeZ4Modal) {
      const modal = this.activeZ4Modal;
      const dialog = doc.createElement("section");
      dialog.className = "nlos-surface-modal-z4";
      dialog.setAttribute("role", "alertdialog");
      dialog.setAttribute("aria-modal", "true");
      dialog.setAttribute("data-z4-prompt-id", modal.promptId);
      dialog.style.padding = "16px";

      const header = doc.createElement("header");
      header.style.display = "flex";
      header.style.alignItems = "center";
      header.style.justifyContent = "space-between";
      header.style.gap = "12px";
      header.style.marginBottom = "12px";

      const titleCol = doc.createElement("div");
      const heading = doc.createElement("strong");
      heading.textContent = modal.title || copy.z4DefaultHeading;
      titleCol.appendChild(heading);

      const phraseRow = doc.createElement("div");
      phraseRow.textContent = `${copy.sealCaption}: ${rec.sealPhrase}`;
      titleCol.appendChild(phraseRow);

      header.appendChild(titleCol);
      header.appendChild(sealHandle.element);
      dialog.appendChild(header);

      if (modal.summary) {
        const summaryP = doc.createElement("p");
        summaryP.textContent = modal.summary;
        dialog.appendChild(summaryP);
      }

      if (modal.blastRadiusItems.length > 0) {
        dialog.appendChild(
          createBlastRadiusDiffElement({
            rows: modal.blastRadiusItems,
            doc,
            locale: this.locale,
            disclosure,
          }),
        );
      }

      if (modal.statutoryLock) {
        dialog.appendChild(
          createStatutoryLockBadgeElement({
            kind: modal.statutoryLock,
            doc,
            locale: this.locale,
            disclosure,
          }),
        );
      }

      const armingNote = doc.createElement("p");
      armingNote.setAttribute("data-z4-arming-ms", String(modal.armingWindowMs));
      armingNote.textContent = copy.z4ArmingHint;
      dialog.appendChild(armingNote);

      const footer = doc.createElement("footer");
      footer.style.display = "flex";
      footer.style.justifyContent = "flex-end";
      footer.style.gap = "8px";
      footer.style.marginTop = "12px";

      const cancelBtn = doc.createElement("button");
      cancelBtn.type = "button";
      cancelBtn.className = "nlos-btn";
      cancelBtn.setAttribute("data-z4-action", "cancel");
      cancelBtn.textContent = modal.cancelLabel ?? copy.z4CancelBtn;
      cancelBtn.addEventListener("click", () => {
        this.cancelZ4Modal();
      });

      const confirmBtn = doc.createElement("button");
      confirmBtn.type = "button";
      confirmBtn.className = "nlos-btn nlos-btn--primary";
      confirmBtn.setAttribute("data-z4-action", "confirm");
      confirmBtn.textContent = modal.confirmLabel ?? copy.z4ConfirmBtn;
      if (modal.statutoryLock) {
        confirmBtn.setAttribute("disabled", "");
      } else {
        confirmBtn.addEventListener("click", () => {
          this.confirmZ4Modal();
        });
      }

      footer.appendChild(cancelBtn);
      footer.appendChild(confirmBtn);
      dialog.appendChild(footer);
      z4Layer.appendChild(dialog);
      return z4Layer;
    }

    const lockDialog = doc.createElement("section");
    lockDialog.className = "nlos-surface-modal-z4";
    lockDialog.setAttribute("role", "dialog");
    lockDialog.setAttribute("aria-modal", "true");
    lockDialog.setAttribute("data-z4-mode", "session-lock");
    lockDialog.style.padding = "16px";

    const header = doc.createElement("header");
    header.style.display = "flex";
    header.style.alignItems = "center";
    header.style.justifyContent = "space-between";
    header.style.gap = "12px";
    header.style.marginBottom = "12px";

    const titleCol = doc.createElement("div");
    const heading = doc.createElement("strong");
    heading.textContent = copy.lockHeading;
    const userLine = doc.createElement("div");
    userLine.textContent = rec.profile.displayName;
    titleCol.appendChild(heading);
    titleCol.appendChild(userLine);

    header.appendChild(titleCol);
    header.appendChild(sealHandle.element);
    lockDialog.appendChild(header);

    const subP = doc.createElement("p");
    subP.textContent = copy.lockSubtitle;
    lockDialog.appendChild(subP);

    const sealP = doc.createElement("p");
    sealP.textContent = `${copy.sealCaption}: ${rec.sealPhrase}`;
    lockDialog.appendChild(sealP);

    const footer = doc.createElement("footer");
    footer.style.display = "flex";
    footer.style.justifyContent = "flex-end";
    footer.style.gap = "8px";
    footer.style.marginTop = "12px";

    const unlockBtn = doc.createElement("button");
    unlockBtn.type = "button";
    unlockBtn.className = "nlos-btn nlos-btn--primary";
    unlockBtn.setAttribute("data-z4-action", "unlock");
    unlockBtn.textContent = copy.unlockBtn;
    unlockBtn.addEventListener("click", () => {
      this.unlockSession();
    });
    footer.appendChild(unlockBtn);
    lockDialog.appendChild(footer);
    z4Layer.appendChild(lockDialog);
    return z4Layer;
  }
}

function findFirstByAttr(
  root: unknown,
  attrName: string,
  attrVal: string,
): HTMLElement | null {
  if (!root || typeof root !== "object") {
    return null;
  }
  const el = root as {
    getAttribute?: (name: string) => string | null;
    children?: readonly unknown[];
  };
  if (typeof el.getAttribute === "function" && el.getAttribute(attrName) === attrVal) {
    return root as HTMLElement;
  }
  if (Array.isArray(el.children)) {
    for (const child of el.children) {
      const found = findFirstByAttr(child, attrName, attrVal);
      if (found) {
        return found;
      }
    }
  }
  return null;
}

export function createWorkstationCompositor(
  config?: WorkstationCompositorConfig,
): WorkstationCompositor {
  return new WorkstationCompositor(config);
}
