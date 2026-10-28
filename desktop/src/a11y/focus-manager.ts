// Workstation Keyboard Navigation Loop, Roving Tabindex & Z4 Modal Focus Trap ([DUI-WINDOW-001], [TUI-CONSENT-001]).
// Covers:
//   - Tab / Shift+Tab focus loop across Z1..Z3 or strictly trapped inside Z4 when active
//   - ArrowUp / ArrowDown / ArrowLeft / ArrowRight / Home / End roving tabindex for grids, trees, and DAGs
//   - Escape deterministic dismissal stack with originating focus restoration
//   - Global workstation shortcuts: Ctrl+Space (Omnibar), Ctrl+K (Spotlight), Ctrl+Shift+E (L3 diagnostic toggle)

export type FocusPlaneId = "Z0" | "Z1" | "Z2" | "Z3" | "Z4";

export type GlobalShortcutCommand =
  | "TOGGLE_OMNIBAR_PALETTE"
  | "TOGGLE_SPOTLIGHT_SEARCH"
  | "TOGGLE_L3_DIAGNOSTIC";

export type DismissalLayerKind =
  | "Z4_SECURITY_MODAL"
  | "Z3_COMMAND_PALETTE"
  | "Z3_SEARCH_PALETTE"
  | "Z3_NOTIFICATION_FLYOUT"
  | "Z3_CONTROL_CENTER_FLYOUT"
  | "L2_INSPECTOR_DRAWER";

export interface FocusTargetDescriptor {
  readonly id: string;
  readonly plane: FocusPlaneId;
  readonly label: string;
  readonly disabled?: boolean;
  readonly visible?: boolean;
}

export interface RovingNodeDescriptor {
  readonly id: string;
  readonly rowIndex: number;
  readonly colIndex?: number;
  readonly parentId?: string | null;
  readonly expandable?: boolean;
  readonly expanded?: boolean;
  readonly disabled?: boolean;
}

export interface RovingGroupSnapshot {
  readonly groupId: string;
  readonly kind: "grid" | "treegrid" | "dag";
  readonly activeNodeId: string;
  readonly tabIndexByNodeId: Readonly<Record<string, 0 | -1>>;
  readonly expandedNodeIds: readonly string[];
}

export interface KeyboardEventInput {
  readonly key: string;
  readonly ctrlKey?: boolean;
  readonly metaKey?: boolean;
  readonly shiftKey?: boolean;
  readonly altKey?: boolean;
}

export interface KeyboardDispatchResult {
  readonly handled: boolean;
  readonly action:
    | "TAB_NEXT"
    | "TAB_PREV"
    | "Z4_TRAP_WRAP_FORWARD"
    | "Z4_TRAP_WRAP_BACKWARD"
    | "ROVING_MOVE"
    | "ROVING_EXPAND"
    | "ROVING_COLLAPSE"
    | "ESCAPE_DISMISS"
    | "GLOBAL_SHORTCUT"
    | "BLOCKED_BY_Z4_MODAL"
    | "NOOP";
  readonly activeFocusId: string | null;
  readonly activePlane: FocusPlaneId;
  readonly shortcutTriggered?: GlobalShortcutCommand;
  readonly dismissedLayer?: DismissalLayerKind;
  readonly restoredFocusId?: string | null;
}

export interface FocusTrapVerificationReport {
  readonly z4ModalActive: boolean;
  readonly z4ControlIds: readonly string[];
  readonly forwardCycleOrder: readonly string[];
  readonly backwardCycleOrder: readonly string[];
  readonly leakedToLowerPlane: boolean;
  readonly blockedExternalFocusAttempts: number;
  readonly blockedGlobalShortcutsUnderZ4: boolean;
  readonly focusRestoredOnDismiss: boolean;
  readonly restoredTargetId: string | null;
}

interface DismissalFrame {
  readonly layer: DismissalLayerKind;
  readonly returnFocusId: string | null;
}

export class WorkstationFocusManager {
  private targets: FocusTargetDescriptor[] = [];
  private activeFocusId: string | null = null;
  private z4ModalActive = false;
  private trapEscapeAttemptCount = 0;
  private readonly dismissalStack: DismissalFrame[] = [];
  private readonly rovingGroups = new Map<
    string,
    {
      kind: "grid" | "treegrid" | "dag";
      nodes: RovingNodeDescriptor[];
      activeNodeId: string;
      expandedIds: Set<string>;
    }
  >();

  public registerFocusTargets(targets: readonly FocusTargetDescriptor[]): void {
    this.targets = [...targets];
    if (this.activeFocusId === null) {
      const firstEligible = this.getEligibleTargets()[0];
      if (firstEligible) {
        this.activeFocusId = firstEligible.id;
      }
    }
  }

  public getActiveFocusId(): string | null {
    return this.activeFocusId;
  }

  public getActivePlane(): FocusPlaneId {
    const found = this.targets.find((t) => t.id === this.activeFocusId);
    if (found) {
      return found.plane;
    }
    return this.z4ModalActive ? "Z4" : "Z1";
  }

  public isZ4ModalActive(): boolean {
    return this.z4ModalActive;
  }

  public getTrapEscapeAttemptCount(): number {
    return this.trapEscapeAttemptCount;
  }

  private getEligibleTargets(): FocusTargetDescriptor[] {
    return this.targets.filter((t) => {
      if (t.disabled || t.visible === false || t.plane === "Z0") {
        return false;
      }
      if (this.z4ModalActive) {
        return t.plane === "Z4";
      }
      return t.plane !== "Z4";
    });
  }

  /**
   * Activates the Z4 system security modal focus trap ([TUI-CONSENT-001]),
   * recording the previously focused Z1..Z3 trigger so Escape/completion restores it.
   */
  public activateZ4ModalTrap(initialZ4FocusId?: string): string | null {
    const priorFocusId = this.activeFocusId;
    this.z4ModalActive = true;
    this.pushDismissalLayer("Z4_SECURITY_MODAL", priorFocusId);

    const z4Targets = this.getEligibleTargets();
    if (initialZ4FocusId && z4Targets.some((t) => t.id === initialZ4FocusId)) {
      this.activeFocusId = initialZ4FocusId;
    } else if (z4Targets.length > 0) {
      this.activeFocusId = z4Targets[0]!.id;
    }
    return this.activeFocusId;
  }

  public deactivateZ4ModalTrap(): string | null {
    this.z4ModalActive = false;
    const idx = this.dismissalStack.findIndex((f) => f.layer === "Z4_SECURITY_MODAL");
    let returnFocusId: string | null = null;
    if (idx >= 0) {
      const [removed] = this.dismissalStack.splice(idx, 1);
      returnFocusId = removed?.returnFocusId ?? null;
    }
    if (returnFocusId && this.targets.some((t) => t.id === returnFocusId && !t.disabled)) {
      this.activeFocusId = returnFocusId;
    } else {
      const firstEligible = this.getEligibleTargets()[0];
      this.activeFocusId = firstEligible ? firstEligible.id : null;
    }
    return this.activeFocusId;
  }

  public requestProgrammaticFocus(targetId: string): boolean {
    const target = this.targets.find((t) => t.id === targetId);
    if (!target || target.disabled || target.visible === false || target.plane === "Z0") {
      return false;
    }
    if (this.z4ModalActive && target.plane !== "Z4") {
      this.trapEscapeAttemptCount += 1;
      const z4Eligible = this.getEligibleTargets();
      if (z4Eligible.length > 0 && (!this.activeFocusId || !z4Eligible.some((t) => t.id === this.activeFocusId))) {
        this.activeFocusId = z4Eligible[0]!.id;
      }
      return false;
    }
    this.activeFocusId = target.id;
    return true;
  }

  public pushDismissalLayer(layer: DismissalLayerKind, returnFocusId?: string | null): void {
    const existingIdx = this.dismissalStack.findIndex((f) => f.layer === layer);
    if (existingIdx >= 0) {
      this.dismissalStack.splice(existingIdx, 1);
    }
    this.dismissalStack.push({
      layer,
      returnFocusId: returnFocusId !== undefined ? returnFocusId : this.activeFocusId,
    });
  }

  public registerRovingGroup(
    groupId: string,
    kind: "grid" | "treegrid" | "dag",
    nodes: readonly RovingNodeDescriptor[],
    initialActiveNodeId?: string,
  ): RovingGroupSnapshot {
    const enabledNodes = nodes.filter((n) => !n.disabled);
    const activeId =
      initialActiveNodeId && enabledNodes.some((n) => n.id === initialActiveNodeId)
        ? initialActiveNodeId
        : (enabledNodes[0]?.id ?? "");
    const expandedIds = new Set<string>(
      nodes.filter((n) => n.expanded).map((n) => n.id),
    );
    this.rovingGroups.set(groupId, {
      kind,
      nodes: [...nodes],
      activeNodeId: activeId,
      expandedIds,
    });
    return this.getRovingGroupSnapshot(groupId);
  }

  public getRovingGroupSnapshot(groupId: string): RovingGroupSnapshot {
    const group = this.rovingGroups.get(groupId);
    if (!group) {
      throw new Error(`Unknown roving tabindex group "${groupId}"`);
    }
    const tabIndexByNodeId: Record<string, 0 | -1> = {};
    for (const node of group.nodes) {
      tabIndexByNodeId[node.id] = node.id === group.activeNodeId && !node.disabled ? 0 : -1;
    }
    return {
      groupId,
      kind: group.kind,
      activeNodeId: group.activeNodeId,
      tabIndexByNodeId,
      expandedNodeIds: [...group.expandedIds],
    };
  }

  private getVisibleRovingNodes(group: {
    kind: "grid" | "treegrid" | "dag";
    nodes: RovingNodeDescriptor[];
    expandedIds: Set<string>;
  }): RovingNodeDescriptor[] {
    const byId = new Map<string, RovingNodeDescriptor>(
      group.nodes.map((n) => [n.id, n]),
    );
    const isAncestorCollapsed = (node: RovingNodeDescriptor): boolean => {
      let curParentId = node.parentId ?? null;
      while (curParentId) {
        if (!group.expandedIds.has(curParentId)) {
          return true;
        }
        const parentNode = byId.get(curParentId);
        curParentId = parentNode?.parentId ?? null;
      }
      return false;
    };
    return group.nodes.filter((n) => !n.disabled && !isAncestorCollapsed(n));
  }

  public handleRovingKey(groupId: string, key: string): KeyboardDispatchResult {
    const group = this.rovingGroups.get(groupId);
    if (!group) {
      return {
        handled: false,
        action: "NOOP",
        activeFocusId: this.activeFocusId,
        activePlane: this.getActivePlane(),
      };
    }

    const visibleNodes = this.getVisibleRovingNodes(group);
    if (visibleNodes.length === 0) {
      return {
        handled: false,
        action: "NOOP",
        activeFocusId: this.activeFocusId,
        activePlane: this.getActivePlane(),
      };
    }

    const currentIdx = Math.max(
      0,
      visibleNodes.findIndex((n) => n.id === group.activeNodeId),
    );
    const currentNode = visibleNodes[currentIdx]!;

    if (key === "ArrowDown") {
      const nextNode = visibleNodes[(currentIdx + 1) % visibleNodes.length]!;
      group.activeNodeId = nextNode.id;
      this.activeFocusId = nextNode.id;
      return {
        handled: true,
        action: "ROVING_MOVE",
        activeFocusId: nextNode.id,
        activePlane: this.getActivePlane(),
      };
    }

    if (key === "ArrowUp") {
      const prevNode =
        visibleNodes[(currentIdx - 1 + visibleNodes.length) % visibleNodes.length]!;
      group.activeNodeId = prevNode.id;
      this.activeFocusId = prevNode.id;
      return {
        handled: true,
        action: "ROVING_MOVE",
        activeFocusId: prevNode.id,
        activePlane: this.getActivePlane(),
      };
    }

    if (key === "Home") {
      const firstNode = visibleNodes[0]!;
      group.activeNodeId = firstNode.id;
      this.activeFocusId = firstNode.id;
      return {
        handled: true,
        action: "ROVING_MOVE",
        activeFocusId: firstNode.id,
        activePlane: this.getActivePlane(),
      };
    }

    if (key === "End") {
      const lastNode = visibleNodes[visibleNodes.length - 1]!;
      group.activeNodeId = lastNode.id;
      this.activeFocusId = lastNode.id;
      return {
        handled: true,
        action: "ROVING_MOVE",
        activeFocusId: lastNode.id,
        activePlane: this.getActivePlane(),
      };
    }

    if (key === "ArrowRight") {
      if (group.kind === "treegrid" && currentNode.expandable && !group.expandedIds.has(currentNode.id)) {
        group.expandedIds.add(currentNode.id);
        return {
          handled: true,
          action: "ROVING_EXPAND",
          activeFocusId: currentNode.id,
          activePlane: this.getActivePlane(),
        };
      }
      const refreshed = this.getVisibleRovingNodes(group);
      const refreshedIdx = Math.max(0, refreshed.findIndex((n) => n.id === currentNode.id));
      const nextNode = refreshed[(refreshedIdx + 1) % refreshed.length]!;
      group.activeNodeId = nextNode.id;
      this.activeFocusId = nextNode.id;
      return {
        handled: true,
        action: "ROVING_MOVE",
        activeFocusId: nextNode.id,
        activePlane: this.getActivePlane(),
      };
    }

    if (key === "ArrowLeft") {
      if (group.kind === "treegrid" && currentNode.expandable && group.expandedIds.has(currentNode.id)) {
        group.expandedIds.delete(currentNode.id);
        return {
          handled: true,
          action: "ROVING_COLLAPSE",
          activeFocusId: currentNode.id,
          activePlane: this.getActivePlane(),
        };
      }
      if (currentNode.parentId) {
        const parentTarget = visibleNodes.find((n) => n.id === currentNode.parentId);
        if (parentTarget) {
          group.activeNodeId = parentTarget.id;
          this.activeFocusId = parentTarget.id;
          return {
            handled: true,
            action: "ROVING_MOVE",
            activeFocusId: parentTarget.id,
            activePlane: this.getActivePlane(),
          };
        }
      }
      const prevNode =
        visibleNodes[(currentIdx - 1 + visibleNodes.length) % visibleNodes.length]!;
      group.activeNodeId = prevNode.id;
      this.activeFocusId = prevNode.id;
      return {
        handled: true,
        action: "ROVING_MOVE",
        activeFocusId: prevNode.id,
        activePlane: this.getActivePlane(),
      };
    }

    return {
      handled: false,
      action: "NOOP",
      activeFocusId: this.activeFocusId,
      activePlane: this.getActivePlane(),
    };
  }

  public handleKeyDown(event: KeyboardEventInput, activeRovingGroupId?: string): KeyboardDispatchResult {
    const primaryMod = Boolean(event.ctrlKey || event.metaKey);

    // 1. Global Workstation Shortcuts (Ctrl+Space, Ctrl+K, Ctrl+Shift+E)
    if (primaryMod && !event.altKey) {
      if (event.key === " " || event.key === "Space") {
        if (this.z4ModalActive) {
          this.trapEscapeAttemptCount += 1;
          return {
            handled: true,
            action: "BLOCKED_BY_Z4_MODAL",
            activeFocusId: this.activeFocusId,
            activePlane: "Z4",
          };
        }
        return {
          handled: true,
          action: "GLOBAL_SHORTCUT",
          activeFocusId: this.activeFocusId,
          activePlane: this.getActivePlane(),
          shortcutTriggered: "TOGGLE_OMNIBAR_PALETTE",
        };
      }
      if (!event.shiftKey && (event.key === "k" || event.key === "K")) {
        if (this.z4ModalActive) {
          this.trapEscapeAttemptCount += 1;
          return {
            handled: true,
            action: "BLOCKED_BY_Z4_MODAL",
            activeFocusId: this.activeFocusId,
            activePlane: "Z4",
          };
        }
        return {
          handled: true,
          action: "GLOBAL_SHORTCUT",
          activeFocusId: this.activeFocusId,
          activePlane: this.getActivePlane(),
          shortcutTriggered: "TOGGLE_SPOTLIGHT_SEARCH",
        };
      }
      if (event.shiftKey && (event.key === "e" || event.key === "E")) {
        return {
          handled: true,
          action: "GLOBAL_SHORTCUT",
          activeFocusId: this.activeFocusId,
          activePlane: this.getActivePlane(),
          shortcutTriggered: "TOGGLE_L3_DIAGNOSTIC",
        };
      }
    }

    // 2. Escape deterministic dismissal stack
    if (event.key === "Escape") {
      const topFrame = this.dismissalStack.pop();
      if (!topFrame) {
        return {
          handled: false,
          action: "NOOP",
          activeFocusId: this.activeFocusId,
          activePlane: this.getActivePlane(),
        };
      }
      if (topFrame.layer === "Z4_SECURITY_MODAL") {
        this.z4ModalActive = false;
      }
      if (topFrame.returnFocusId && this.targets.some((t) => t.id === topFrame.returnFocusId && !t.disabled)) {
        this.activeFocusId = topFrame.returnFocusId;
      }
      return {
        handled: true,
        action: "ESCAPE_DISMISS",
        activeFocusId: this.activeFocusId,
        activePlane: this.getActivePlane(),
        dismissedLayer: topFrame.layer,
        restoredFocusId: this.activeFocusId,
      };
    }

    // 3. Tab / Shift+Tab focus loop with Z4 Modal Focus Trap ([TUI-CONSENT-001])
    if (event.key === "Tab") {
      const eligible = this.getEligibleTargets();
      if (eligible.length === 0) {
        return {
          handled: false,
          action: "NOOP",
          activeFocusId: null,
          activePlane: this.getActivePlane(),
        };
      }
      const curIdx = eligible.findIndex((t) => t.id === this.activeFocusId);
      if (!event.shiftKey) {
        const isWrap = curIdx === eligible.length - 1 || curIdx < 0;
        const nextIdx = curIdx < 0 ? 0 : (curIdx + 1) % eligible.length;
        const nextTarget = eligible[nextIdx]!;
        this.activeFocusId = nextTarget.id;
        return {
          handled: true,
          action: this.z4ModalActive && isWrap ? "Z4_TRAP_WRAP_FORWARD" : "TAB_NEXT",
          activeFocusId: nextTarget.id,
          activePlane: nextTarget.plane,
        };
      } else {
        const isWrap = curIdx <= 0;
        const prevIdx = curIdx <= 0 ? eligible.length - 1 : curIdx - 1;
        const prevTarget = eligible[prevIdx]!;
        this.activeFocusId = prevTarget.id;
        return {
          handled: true,
          action: this.z4ModalActive && isWrap ? "Z4_TRAP_WRAP_BACKWARD" : "TAB_PREV",
          activeFocusId: prevTarget.id,
          activePlane: prevTarget.plane,
        };
      }
    }

    // 4. Roving tabindex arrow keys
    if (
      activeRovingGroupId &&
      (event.key === "ArrowDown" ||
        event.key === "ArrowUp" ||
        event.key === "ArrowLeft" ||
        event.key === "ArrowRight" ||
        event.key === "Home" ||
        event.key === "End")
    ) {
      return this.handleRovingKey(activeRovingGroupId, event.key);
    }

    return {
      handled: false,
      action: "NOOP",
      activeFocusId: this.activeFocusId,
      activePlane: this.getActivePlane(),
    };
  }
}

/**
 * Deterministic verifier for the Z4 Modal Focus Trap ([TUI-CONSENT-001]).
 */
export function verifyZ4ModalFocusTrap(
  targets: readonly FocusTargetDescriptor[],
  originatingTriggerId: string,
): FocusTrapVerificationReport {
  const mgr = new WorkstationFocusManager();
  mgr.registerFocusTargets(targets);
  mgr.requestProgrammaticFocus(originatingTriggerId);

  const z4Controls = targets.filter((t) => t.plane === "Z4" && !t.disabled && t.visible !== false);
  const z4ControlIds = z4Controls.map((t) => t.id);

  mgr.activateZ4ModalTrap();
  const forwardCycleOrder: string[] = [];
  for (let i = 0; i <= z4ControlIds.length; i += 1) {
    forwardCycleOrder.push(mgr.getActiveFocusId() ?? "");
    mgr.handleKeyDown({ key: "Tab" });
  }

  const backwardCycleOrder: string[] = [];
  for (let i = 0; i <= z4ControlIds.length; i += 1) {
    mgr.handleKeyDown({ key: "Tab", shiftKey: true });
    backwardCycleOrder.push(mgr.getActiveFocusId() ?? "");
  }

  // Attempt programmatic escape to a Z1/Z2/Z3 control while Z4 is active
  const lowerTarget = targets.find((t) => t.plane !== "Z4" && t.plane !== "Z0" && !t.disabled);
  if (lowerTarget) {
    mgr.requestProgrammaticFocus(lowerTarget.id);
  }
  const omniAttempt = mgr.handleKeyDown({ key: " ", ctrlKey: true });
  const spotlightAttempt = mgr.handleKeyDown({ key: "k", ctrlKey: true });
  const blockedGlobalShortcutsUnderZ4 =
    omniAttempt.action === "BLOCKED_BY_Z4_MODAL" &&
    spotlightAttempt.action === "BLOCKED_BY_Z4_MODAL";

  const allVisitedIds = [...forwardCycleOrder, ...backwardCycleOrder];
  const leakedToLowerPlane = allVisitedIds.some((id) => !z4ControlIds.includes(id));

  const dismissRes = mgr.handleKeyDown({ key: "Escape" });
  const restoredTargetId = dismissRes.restoredFocusId ?? null;

  return {
    z4ModalActive: true,
    z4ControlIds,
    forwardCycleOrder,
    backwardCycleOrder,
    leakedToLowerPlane,
    blockedExternalFocusAttempts: mgr.getTrapEscapeAttemptCount(),
    blockedGlobalShortcutsUnderZ4,
    focusRestoredOnDismiss: restoredTargetId === originatingTriggerId,
    restoredTargetId,
  };
}
