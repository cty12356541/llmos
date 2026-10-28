// Deterministic ARIA Semantic Tree Builder, Landmark Auditor & Dual-Channel Live Announcer
// ([DUI-WINDOW-001], WCAG 2.2 AA 1.3.1, 2.5.7 Dragging Movements, 2.5.8 Target Size >= 26x26px, 4.1.2 Name/Role/Value).

import {
  FORBIDDEN_L0_L1_L2_JARGON_TOKENS,
  getLocaleBundle,
  interpolateMessage,
  type ExtendedWorkstationStateKey,
  type SupportedLocale,
} from "../i18n/index";
import type { FocusPlaneId } from "./focus-manager";

export const WCAG_MIN_HIT_TARGET_PX = 26 as const;

export type AriaRoleKind =
  | "main"
  | "region"
  | "complementary"
  | "contentinfo"
  | "grid"
  | "treegrid"
  | "dialog"
  | "alertdialog"
  | "status"
  | "progressbar"
  | "toolbar"
  | "button"
  | "row"
  | "treeitem";

export type AriaLivePoliteness = "polite" | "assertive";

export interface AriaStateAttributes {
  readonly ariaExpanded?: boolean;
  readonly ariaSelected?: boolean;
  readonly ariaDisabled?: boolean;
  readonly ariaModal?: boolean;
  readonly ariaHidden?: boolean;
  readonly ariaLive?: AriaLivePoliteness;
  readonly ariaValueMin?: number;
  readonly ariaValueMax?: number;
  readonly ariaValueNow?: number;
}

export interface AriaSemanticNode {
  readonly id: string;
  readonly plane: FocusPlaneId;
  readonly role: AriaRoleKind;
  readonly accessibleName: string;
  readonly interactive: boolean;
  readonly hitTargetWidthPx: number;
  readonly hitTargetHeightPx: number;
  readonly requiresDragEquivalent?: boolean;
  readonly keyboardDragEquivalentMenuId?: string | null;
  readonly state: AriaStateAttributes;
  readonly children: readonly AriaSemanticNode[];
}

export interface AriaAuditViolation {
  readonly nodeId: string;
  readonly ruleId:
    | "MISSING_ACCESSIBLE_NAME"
    | "Z0_MUST_BE_ARIA_HIDDEN"
    | "Z4_MUST_BE_ALERTDIALOG_MODAL"
    | "LOWER_PLANE_NOT_HIDDEN_UNDER_Z4"
    | "PROGRESSBAR_MISSING_BOUNDS"
    | "EXPANDABLE_MISSING_ARIA_EXPANDED"
    | "HIT_TARGET_BELOW_26PX"
    | "DRAG_CONTROL_MISSING_KEYBOARD_EQUIVALENT"
    | "JARGON_IN_ACCESSIBLE_NAME";
  readonly detail: string;
}

export interface AriaTreeAuditReport {
  readonly passed: boolean;
  readonly locale: SupportedLocale;
  readonly z4ModalActive: boolean;
  readonly totalNodeCount: number;
  readonly interactiveNodeCount: number;
  readonly landmarkRolesPresent: readonly AriaRoleKind[];
  readonly violations: readonly AriaAuditViolation[];
}

export interface WorkstationAriaTreeOptions {
  readonly locale?: SupportedLocale;
  readonly z4ModalActive?: boolean;
  readonly l2DrawerOpen?: boolean;
  readonly omnibarOpen?: boolean;
  readonly spotlightOpen?: boolean;
}

/**
 * Builds the canonical 5-plane (Z0..Z4) ARIA semantic tree for the industrial workstation.
 */
export function buildCanonicalWorkstationAriaTree(
  options: WorkstationAriaTreeOptions = {},
): readonly AriaSemanticNode[] {
  const locale: SupportedLocale = options.locale ?? "zh-CN";
  const z4ModalActive = Boolean(options.z4ModalActive);
  const l2DrawerOpen = options.l2DrawerOpen ?? true;
  const omnibarOpen = options.omnibarOpen ?? false;
  const spotlightOpen = options.spotlightOpen ?? false;
  const bundle = getLocaleBundle(locale);
  const lm = bundle.a11yAnnouncements.landmarks;

  const z0Node: AriaSemanticNode = {
    id: "plane-z0-backdrop",
    plane: "Z0",
    role: "region",
    accessibleName: bundle.planes.z0Backdrop,
    interactive: false,
    hitTargetWidthPx: 1920,
    hitTargetHeightPx: 1080,
    state: { ariaHidden: true },
    children: [],
  };

  const z1Node: AriaSemanticNode = {
    id: "plane-z1-workspace-main",
    plane: "Z1",
    role: "main",
    accessibleName: lm.workspaceMain,
    interactive: false,
    hitTargetWidthPx: 1920,
    hitTargetHeightPx: 1044,
    state: { ariaHidden: z4ModalActive },
    children: [
      {
        id: "z1-task-space-region",
        plane: "Z1",
        role: "region",
        accessibleName: lm.taskSpaceRegion,
        interactive: false,
        hitTargetWidthPx: 1500,
        hitTargetHeightPx: 1044,
        state: { ariaHidden: z4ModalActive },
        children: [
          {
            id: "z1-task-space-toolbar",
            plane: "Z1",
            role: "toolbar",
            accessibleName: bundle.planes.z1TaskSpace,
            interactive: true,
            hitTargetWidthPx: 1500,
            hitTargetHeightPx: 32,
            state: { ariaDisabled: false },
            children: [],
          },
          {
            id: "z1-task-space-grid",
            plane: "Z1",
            role: "grid",
            accessibleName: lm.taskSpaceRegion,
            interactive: true,
            hitTargetWidthPx: 1500,
            hitTargetHeightPx: 980,
            state: { ariaSelected: true, ariaDisabled: false },
            children: [
              {
                id: "z1-task-row-1-resource-bar",
                plane: "Z1",
                role: "progressbar",
                accessibleName: bundle.l2AttributionDrawer.resourceUsageBarTitle,
                interactive: false,
                hitTargetWidthPx: 84,
                hitTargetHeightPx: 26,
                state: {
                  ariaValueMin: 0,
                  ariaValueMax: 100,
                  ariaValueNow: 42,
                },
                children: [],
              },
            ],
          },
        ],
      },
      {
        id: "z1-l2-inspector-drawer",
        plane: "Z1",
        role: "complementary",
        accessibleName: lm.inspectorComplementary,
        interactive: true,
        hitTargetWidthPx: 420,
        hitTargetHeightPx: 1044,
        requiresDragEquivalent: true,
        keyboardDragEquivalentMenuId: "menu-drawer-resize-preset",
        state: {
          ariaExpanded: l2DrawerOpen,
          ariaHidden: z4ModalActive,
        },
        children: [],
      },
    ],
  };

  const z2Node: AriaSemanticNode = {
    id: "plane-z2-compositor-region",
    plane: "Z2",
    role: "region",
    accessibleName: interpolateMessage(lm.applicationWindowRegionTemplate, {
      title: bundle.controlCenterActions.taskManagerTitle,
    }),
    interactive: true,
    hitTargetWidthPx: 1480,
    hitTargetHeightPx: 960,
    requiresDragEquivalent: true,
    keyboardDragEquivalentMenuId: "menu-tile-splitter-preset",
    state: { ariaHidden: z4ModalActive },
    children: [
      {
        id: "z2-ownership-treegrid",
        plane: "Z2",
        role: "treegrid",
        accessibleName: bundle.controlCenterActions.taskManagerTitle,
        interactive: true,
        hitTargetWidthPx: 1460,
        hitTargetHeightPx: 900,
        state: {
          ariaExpanded: true,
          ariaSelected: true,
          ariaDisabled: false,
        },
        children: [],
      },
    ],
  };

  const z3Children: AriaSemanticNode[] = [
    {
      id: "z3-taskbar-omnibar-trigger",
      plane: "Z3",
      role: "button",
      accessibleName: bundle.planes.z3CommandPalette,
      interactive: true,
      hitTargetWidthPx: 180,
      hitTargetHeightPx: 26,
      state: {
        ariaExpanded: omnibarOpen,
        ariaDisabled: z4ModalActive,
      },
      children: [],
    },
    {
      id: "z3-live-region-polite",
      plane: "Z3",
      role: "status",
      accessibleName: bundle.planes.z3Notifications,
      interactive: false,
      hitTargetWidthPx: 26,
      hitTargetHeightPx: 26,
      state: { ariaLive: "polite" },
      children: [],
    },
  ];

  if (omnibarOpen) {
    z3Children.push({
      id: "z3-omnibar-dialog",
      plane: "Z3",
      role: "dialog",
      accessibleName: lm.commandPaletteDialog,
      interactive: true,
      hitTargetWidthPx: 580,
      hitTargetHeightPx: 320,
      state: {
        ariaModal: false,
        ariaExpanded: true,
      },
      children: [],
    });
  }

  if (spotlightOpen) {
    z3Children.push({
      id: "z3-spotlight-dialog",
      plane: "Z3",
      role: "dialog",
      accessibleName: lm.searchPaletteDialog,
      interactive: true,
      hitTargetWidthPx: 640,
      hitTargetHeightPx: 360,
      state: {
        ariaModal: false,
        ariaExpanded: true,
      },
      children: [],
    });
  }

  const z3Node: AriaSemanticNode = {
    id: "plane-z3-taskbar",
    plane: "Z3",
    role: "contentinfo",
    accessibleName: lm.taskbarContentinfo,
    interactive: false,
    hitTargetWidthPx: 1920,
    hitTargetHeightPx: 36,
    state: { ariaHidden: z4ModalActive },
    children: z3Children,
  };

  const z4Node: AriaSemanticNode = {
    id: "plane-z4-security-alertdialog",
    plane: "Z4",
    role: "alertdialog",
    accessibleName: lm.securityAlertDialog,
    interactive: z4ModalActive,
    hitTargetWidthPx: 520,
    hitTargetHeightPx: 380,
    state: {
      ariaModal: z4ModalActive,
      ariaHidden: !z4ModalActive,
      ariaLive: "assertive",
    },
    children: z4ModalActive
      ? [
          {
            id: "z4-btn-approve",
            plane: "Z4",
            role: "button",
            accessibleName: bundle.z4SecurityConsent.approveAction,
            interactive: true,
            hitTargetWidthPx: 148,
            hitTargetHeightPx: 32,
            state: { ariaDisabled: false },
            children: [],
          },
          {
            id: "z4-btn-deny",
            plane: "Z4",
            role: "button",
            accessibleName: bundle.z4SecurityConsent.denyAction,
            interactive: true,
            hitTargetWidthPx: 148,
            hitTargetHeightPx: 32,
            state: { ariaDisabled: false },
            children: [],
          },
        ]
      : [],
  };

  return [z0Node, z1Node, z2Node, z3Node, z4Node];
}

function hasForbiddenJargon(text: string): string | null {
  for (const token of FORBIDDEN_L0_L1_L2_JARGON_TOKENS) {
    if (/^[A-Za-z0-9_]+$/.test(token)) {
      const regex = new RegExp(`\\b${token}\\b`);
      if (regex.test(text)) {
        return token;
      }
    } else if (text.includes(token)) {
      return token;
    }
  }
  return null;
}

/**
 * Audits an ARIA semantic tree against WCAG 2.2 AA & [DUI-WINDOW-001] invariants.
 */
export function auditWorkstationAriaTree(
  roots: readonly AriaSemanticNode[],
  options: { readonly locale?: SupportedLocale; readonly z4ModalActive?: boolean } = {},
): AriaTreeAuditReport {
  const locale: SupportedLocale = options.locale ?? "zh-CN";
  const z4ModalActive = Boolean(options.z4ModalActive);
  const violations: AriaAuditViolation[] = [];
  const rolesSet = new Set<AriaRoleKind>();
  let totalNodeCount = 0;
  let interactiveNodeCount = 0;

  const visit = (node: AriaSemanticNode): void => {
    totalNodeCount += 1;
    rolesSet.add(node.role);
    if (node.interactive) {
      interactiveNodeCount += 1;
    }

    if (node.accessibleName.trim().length === 0) {
      violations.push({
        nodeId: node.id,
        ruleId: "MISSING_ACCESSIBLE_NAME",
        detail: `Node "${node.id}" (role=${node.role}) has an empty accessibleName`,
      });
    } else {
      const jargon = hasForbiddenJargon(node.accessibleName);
      if (jargon) {
        violations.push({
          nodeId: node.id,
          ruleId: "JARGON_IN_ACCESSIBLE_NAME",
          detail: `Node "${node.id}" accessibleName contains forbidden L0/L1/L2 jargon token "${jargon}"`,
        });
      }
    }

    if (node.plane === "Z0" && node.state.ariaHidden !== true) {
      violations.push({
        nodeId: node.id,
        ruleId: "Z0_MUST_BE_ARIA_HIDDEN",
        detail: `Decorative Z0 backdrop node "${node.id}" must set ariaHidden=true`,
      });
    }

    if (node.plane === "Z4" && z4ModalActive && node.id === "plane-z4-security-alertdialog") {
      if (node.role !== "alertdialog" || node.state.ariaModal !== true) {
        violations.push({
          nodeId: node.id,
          ruleId: "Z4_MUST_BE_ALERTDIALOG_MODAL",
          detail: `Active Z4 security prompt "${node.id}" must have role="alertdialog" and ariaModal=true`,
        });
      }
    }

    if (
      z4ModalActive &&
      (node.plane === "Z1" || node.plane === "Z2" || node.plane === "Z3") &&
      node.id.startsWith("plane-") &&
      node.state.ariaHidden !== true
    ) {
      violations.push({
        nodeId: node.id,
        ruleId: "LOWER_PLANE_NOT_HIDDEN_UNDER_Z4",
        detail: `Lower plane root "${node.id}" (${node.plane}) must set aria-hidden=true while Z4 modal is active`,
      });
    }

    if (node.role === "progressbar") {
      if (
        typeof node.state.ariaValueMin !== "number" ||
        typeof node.state.ariaValueMax !== "number" ||
        typeof node.state.ariaValueNow !== "number"
      ) {
        violations.push({
          nodeId: node.id,
          ruleId: "PROGRESSBAR_MISSING_BOUNDS",
          detail: `Progressbar "${node.id}" must define numeric ariaValueMin, ariaValueMax, and ariaValueNow`,
        });
      }
    }

    if (
      (node.role === "treegrid" || node.role === "complementary") &&
      typeof node.state.ariaExpanded !== "boolean"
    ) {
      violations.push({
        nodeId: node.id,
        ruleId: "EXPANDABLE_MISSING_ARIA_EXPANDED",
        detail: `Expandable landmark/tree "${node.id}" must specify boolean ariaExpanded`,
      });
    }

    if (node.interactive) {
      if (
        node.hitTargetWidthPx < WCAG_MIN_HIT_TARGET_PX ||
        node.hitTargetHeightPx < WCAG_MIN_HIT_TARGET_PX
      ) {
        violations.push({
          nodeId: node.id,
          ruleId: "HIT_TARGET_BELOW_26PX",
          detail: `Interactive control "${node.id}" hit target (${node.hitTargetWidthPx}x${node.hitTargetHeightPx}px) is smaller than 26x26px (WCAG 2.5.8)`,
        });
      }
    }

    if (
      node.requiresDragEquivalent &&
      (!node.keyboardDragEquivalentMenuId || node.keyboardDragEquivalentMenuId.trim().length === 0)
    ) {
      violations.push({
        nodeId: node.id,
        ruleId: "DRAG_CONTROL_MISSING_KEYBOARD_EQUIVALENT",
        detail: `Draggable control "${node.id}" is missing a keyboard menu equivalent (WCAG 2.5.7)`,
      });
    }

    for (const child of node.children) {
      visit(child);
    }
  };

  for (const root of roots) {
    visit(root);
  }

  return {
    passed: violations.length === 0,
    locale,
    z4ModalActive,
    totalNodeCount,
    interactiveNodeCount,
    landmarkRolesPresent: [...rolesSet],
    violations,
  };
}

export interface LiveAnnouncementRecord {
  readonly sequence: number;
  readonly politeness: AriaLivePoliteness;
  readonly locale: SupportedLocale;
  readonly message: string;
}

export class WorkstationLiveRegionAnnouncer {
  private sequence = 0;
  private readonly log: LiveAnnouncementRecord[] = [];

  public announceTaskStateTransition(
    locale: SupportedLocale,
    taskTitle: string,
    nextState: ExtendedWorkstationStateKey,
  ): LiveAnnouncementRecord {
    const bundle = getLocaleBundle(locale);
    const stateLabel = bundle.taskStates[nextState];
    const isIsolated = nextState === "QUARANTINED";
    const politeness: AriaLivePoliteness = isIsolated ? "assertive" : "polite";
    const template = isIsolated
      ? bundle.a11yAnnouncements.isolatedAlertAssertiveTemplate
      : bundle.a11yAnnouncements.taskStateTransitionPoliteTemplate;
    const message = interpolateMessage(template, {
      taskTitle,
      targetName: taskTitle,
      stateLabel,
    });
    this.sequence += 1;
    const rec: LiveAnnouncementRecord = {
      sequence: this.sequence,
      politeness,
      locale,
      message,
    };
    this.log.push(rec);
    return rec;
  }

  public announceZ4SecurityPrompt(
    locale: SupportedLocale,
    actionName: string,
  ): LiveAnnouncementRecord {
    const bundle = getLocaleBundle(locale);
    const message = interpolateMessage(
      bundle.a11yAnnouncements.securityPromptAssertiveTemplate,
      { actionName },
    );
    this.sequence += 1;
    const rec: LiveAnnouncementRecord = {
      sequence: this.sequence,
      politeness: "assertive",
      locale,
      message,
    };
    this.log.push(rec);
    return rec;
  }

  public getHistory(): readonly LiveAnnouncementRecord[] {
    return [...this.log];
  }
}
