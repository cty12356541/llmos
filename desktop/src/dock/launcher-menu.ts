// [DUI-LAUNCH-001] & [UI-PARITY-001] Application Launcher Model & Solid Flyout Builder.
// Authority: ADR-0025 Decision 1 & 4 + Stage D Desktop Visual & UX Spec §3.3.
// Invariants:
//   - Activates applications strictly by (applicationIdHex, installationIdHex)
//   - Rejects launch attempts using packageIdHex or transient processIdHex (even if smuggled)
//   - Exposes DisableApplication & UninstallApplication (locked with monochrome reason if active tasks exist)
//   - Zero internal jargon or hex digests in L0/L1/L2 (strictly gated to disclosure === "L3")

import {
  createIconElement,
  renderIconSvg,
  type DisclosureLevel,
  type IconName,
  type WorkstationLocale,
} from "../ui";

export type InstalledApplicationStatus = "ACTIVE" | "DISABLED" | "UNINSTALLED";

export interface InstalledApplicationEntry {
  readonly applicationIdHex: string;
  readonly installationIdHex: string;
  readonly packageIdHex?: string;
  readonly displayName: string;
  readonly publisherName?: string;
  readonly versionLabel?: string;
  readonly icon?: IconName;
  readonly status: InstalledApplicationStatus;
  readonly installationGeneration: number;
  readonly expectedGenerationOrRevision?: number;
  readonly manifestDigestHex?: string;
  readonly hasActiveTasks?: boolean;
  readonly activeTaskCount?: number;
  readonly activeProcessIdsHex?: readonly string[];
  readonly pinnedToTaskbar?: boolean;
}

export interface LaunchApplicationRequest {
  readonly applicationIdHex?: string;
  readonly installationIdHex?: string;
  /** Non-application package identity — supplying this MUST be rejected ([DUI-LAUNCH-001]). */
  readonly packageIdHex?: string;
  /** Transient OS/runtime process identity — supplying this MUST be rejected ([DUI-LAUNCH-001]). */
  readonly processIdHex?: string;
}

export type LaunchRejectionCode =
  | "PACKAGE_ID_NOT_APPLICATION_IDENTITY"
  | "PROCESS_ID_NOT_APPLICATION_IDENTITY"
  | "MISSING_APPLICATION_OR_INSTALLATION_ID"
  | "UNKNOWN_APPLICATION_INSTALLATION"
  | "APPLICATION_NOT_ACTIVE";

export interface PreparedApplicationLaunch {
  readonly ok: true;
  readonly applicationIdHex: string;
  readonly installationIdHex: string;
  readonly installationGeneration: number;
  readonly displayName: string;
}

export interface RejectedApplicationLaunch {
  readonly ok: false;
  readonly code: LaunchRejectionCode;
  readonly message: string;
}

export type ValidateLaunchResult =
  | PreparedApplicationLaunch
  | RejectedApplicationLaunch;

export interface LauncherAllowedAppVerbs {
  readonly canLaunch: boolean;
  readonly canDisable: boolean;
  readonly canUninstall: boolean;
  readonly uninstallLockedByActiveTasks: boolean;
}

export interface ApplicationLauncherModelInit {
  readonly applications?: readonly InstalledApplicationEntry[];
  readonly knownPackageIdsHex?: readonly string[];
  readonly knownProcessIdsHex?: readonly string[];
}

function normalizeId(raw: string | undefined): string {
  return (raw ?? "").trim().toLowerCase();
}

function compositeAppKey(appId: string, instId: string): string {
  return `${normalizeId(appId)}::${normalizeId(instId)}`;
}

export function computeLauncherAppVerbs(
  entry: InstalledApplicationEntry,
): LauncherAllowedAppVerbs {
  const activeTasks =
    Boolean(entry.hasActiveTasks) || (entry.activeTaskCount ?? 0) > 0;
  const isUninstalled = entry.status === "UNINSTALLED";
  const isActive = entry.status === "ACTIVE";

  return {
    canLaunch: isActive,
    canDisable: isActive,
    canUninstall: !isUninstalled && !activeTasks,
    uninstallLockedByActiveTasks: !isUninstalled && activeTasks,
  };
}

export class ApplicationLauncherModel {
  private readonly appsByKey = new Map<string, InstalledApplicationEntry>();
  private orderedApps: InstalledApplicationEntry[] = [];
  private readonly knownPackageIds = new Set<string>();
  private readonly knownProcessIds = new Set<string>();

  public constructor(init?: ApplicationLauncherModelInit) {
    for (const pkgId of init?.knownPackageIdsHex ?? []) {
      const norm = normalizeId(pkgId);
      if (norm.length > 0) {
        this.knownPackageIds.add(norm);
      }
    }
    for (const procId of init?.knownProcessIdsHex ?? []) {
      const norm = normalizeId(procId);
      if (norm.length > 0) {
        this.knownProcessIds.add(norm);
      }
    }
    if (init?.applications) {
      this.setInstalledApplications(init.applications);
    }
  }

  public setInstalledApplications(
    entries: readonly InstalledApplicationEntry[],
  ): void {
    this.appsByKey.clear();
    this.orderedApps = [...entries];

    for (const entry of entries) {
      const key = compositeAppKey(
        entry.applicationIdHex,
        entry.installationIdHex,
      );
      this.appsByKey.set(key, entry);

      const normPkg = normalizeId(entry.packageIdHex);
      if (normPkg.length > 0) {
        this.knownPackageIds.add(normPkg);
      }
      for (const procId of entry.activeProcessIdsHex ?? []) {
        const normProc = normalizeId(procId);
        if (normProc.length > 0) {
          this.knownProcessIds.add(normProc);
        }
      }
    }
  }

  public registerKnownPackageId(packageIdHex: string): void {
    const norm = normalizeId(packageIdHex);
    if (norm.length > 0) {
      this.knownPackageIds.add(norm);
    }
  }

  public registerKnownProcessId(processIdHex: string): void {
    const norm = normalizeId(processIdHex);
    if (norm.length > 0) {
      this.knownProcessIds.add(norm);
    }
  }

  public getInstalledApplications(): readonly InstalledApplicationEntry[] {
    return [...this.orderedApps];
  }

  public getApplication(
    applicationIdHex: string,
    installationIdHex: string,
  ): InstalledApplicationEntry | undefined {
    return this.appsByKey.get(
      compositeAppKey(applicationIdHex, installationIdHex),
    );
  }

  public validateAndPrepareLaunch(
    request: LaunchApplicationRequest,
  ): ValidateLaunchResult {
    const explicitPkg = normalizeId(request.packageIdHex);
    if (explicitPkg.length > 0) {
      return {
        ok: false,
        code: "PACKAGE_ID_NOT_APPLICATION_IDENTITY",
        message:
          "ApplicationLauncher requires (applicationIdHex, installationIdHex); packageIdHex is not an application identity.",
      };
    }

    const explicitProc = normalizeId(request.processIdHex);
    if (explicitProc.length > 0) {
      return {
        ok: false,
        code: "PROCESS_ID_NOT_APPLICATION_IDENTITY",
        message:
          "ApplicationLauncher requires (applicationIdHex, installationIdHex); transient processIdHex is not an application identity.",
      };
    }

    const appId = normalizeId(request.applicationIdHex);
    const instId = normalizeId(request.installationIdHex);

    // Guard against smuggling packageIdHex or processIdHex into applicationIdHex / installationIdHex
    if (
      (appId.length > 0 && this.knownPackageIds.has(appId)) ||
      (instId.length > 0 && this.knownPackageIds.has(instId))
    ) {
      return {
        ok: false,
        code: "PACKAGE_ID_NOT_APPLICATION_IDENTITY",
        message:
          "Smuggled packageIdHex detected inside application/installation identity fields.",
      };
    }

    if (
      (appId.length > 0 && this.knownProcessIds.has(appId)) ||
      (instId.length > 0 && this.knownProcessIds.has(instId))
    ) {
      return {
        ok: false,
        code: "PROCESS_ID_NOT_APPLICATION_IDENTITY",
        message:
          "Smuggled transient processIdHex detected inside application/installation identity fields.",
      };
    }

    if (appId.length === 0 || instId.length === 0) {
      return {
        ok: false,
        code: "MISSING_APPLICATION_OR_INSTALLATION_ID",
        message:
          "Both applicationIdHex and installationIdHex are required to activate an application.",
      };
    }

    const entry = this.appsByKey.get(compositeAppKey(appId, instId));
    if (!entry) {
      return {
        ok: false,
        code: "UNKNOWN_APPLICATION_INSTALLATION",
        message: "No installed application matches (applicationIdHex, installationIdHex).",
      };
    }

    if (entry.status !== "ACTIVE") {
      return {
        ok: false,
        code: "APPLICATION_NOT_ACTIVE",
        message: `Application '${entry.displayName}' is ${entry.status} and cannot be launched.`,
      };
    }

    return {
      ok: true,
      applicationIdHex: entry.applicationIdHex,
      installationIdHex: entry.installationIdHex,
      installationGeneration: entry.installationGeneration,
      displayName: entry.displayName,
    };
  }
}

export function createApplicationLauncherModel(
  init?: ApplicationLauncherModelInit,
): ApplicationLauncherModel {
  return new ApplicationLauncherModel(init);
}

/* ==========================================================================
   Launcher Flyout HTML & DOM Builders
   ========================================================================== */

export interface ApplicationMutationTarget {
  readonly applicationIdHex: string;
  readonly installationIdHex: string;
  readonly expectedGenerationOrRevision?: number;
}

export interface LauncherMenuCallbacks {
  readonly onLaunch?: (prepared: PreparedApplicationLaunch) => void;
  readonly onLaunchRejected?: (rejected: RejectedApplicationLaunch) => void;
  readonly onDisableApplication?: (target: ApplicationMutationTarget) => void;
  readonly onUninstallApplication?: (target: ApplicationMutationTarget) => void;
  readonly onClose?: () => void;
}

export interface LauncherMenuOptions {
  readonly model?: ApplicationLauncherModel;
  readonly applications?: readonly InstalledApplicationEntry[];
  readonly locale?: WorkstationLocale;
  readonly disclosure?: DisclosureLevel;
  readonly callbacks?: LauncherMenuCallbacks;
  readonly doc?: Document;
}

interface LauncherCopy {
  readonly heading: string;
  readonly closeAria: string;
  readonly emptyState: string;
  readonly statusActive: string;
  readonly statusDisabled: string;
  readonly statusUninstalled: string;
  readonly launchBtn: string;
  readonly disableBtn: string;
  readonly uninstallBtn: string;
  readonly activeTasksLockReason: string;
}

const LAUNCHER_COPY: Readonly<Record<WorkstationLocale, LauncherCopy>> = {
  "zh-CN": {
    heading: "已安装应用",
    closeAria: "关闭应用菜单",
    emptyState: "暂无已安装应用",
    statusActive: "就绪",
    statusDisabled: "已禁用",
    statusUninstalled: "已卸载",
    launchBtn: "打开应用",
    disableBtn: "禁用应用",
    uninstallBtn: "卸载应用",
    activeTasksLockReason: "存在运行中任务，暂不可卸载",
  },
  "en-US": {
    heading: "Installed Applications",
    closeAria: "Close application launcher",
    emptyState: "No installed applications",
    statusActive: "Ready",
    statusDisabled: "Disabled",
    statusUninstalled: "Uninstalled",
    launchBtn: "Launch",
    disableBtn: "Disable",
    uninstallBtn: "Uninstall",
    activeTasksLockReason: "Active tasks running; uninstall locked",
  },
};

function escapeHtml(raw: string): string {
  return raw
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

function resolveModel(options: LauncherMenuOptions): ApplicationLauncherModel {
  if (options.model) {
    return options.model;
  }
  return new ApplicationLauncherModel({
    applications: options.applications ?? [],
  });
}

function statusLabelForEntry(
  status: InstalledApplicationStatus,
  copy: LauncherCopy,
): string {
  switch (status) {
    case "ACTIVE":
      return copy.statusActive;
    case "DISABLED":
      return copy.statusDisabled;
    case "UNINSTALLED":
      return copy.statusUninstalled;
  }
}

function formatL3IdentityLine(entry: InstalledApplicationEntry): string {
  const parts = [
    `app:${entry.applicationIdHex}`,
    `inst:${entry.installationIdHex}`,
    `gen:${entry.installationGeneration}`,
  ];
  if (entry.packageIdHex) {
    parts.push(`pkg:${entry.packageIdHex}`);
  }
  if (entry.manifestDigestHex) {
    parts.push(`digest:${entry.manifestDigestHex}`);
  }
  return parts.join(" · ");
}

export function renderLauncherMenuHtml(options: LauncherMenuOptions): string {
  const locale: WorkstationLocale = options.locale ?? "zh-CN";
  const disclosure: DisclosureLevel = options.disclosure ?? "L1";
  const copy = LAUNCHER_COPY[locale];
  const model = resolveModel(options);
  const entries = model.getInstalledApplications();

  const itemsHtml =
    entries.length === 0
      ? `<li class="nlos-launcher-row"><span class="nlos-launcher-row__meta">${escapeHtml(copy.emptyState)}</span></li>`
      : entries
          .map((entry, idx) => {
            const verbs = computeLauncherAppVerbs(entry);
            const iconName: IconName = entry.icon ?? "app-launcher";
            const statusText = statusLabelForEntry(entry.status, copy);
            const versionPart = entry.versionLabel ? ` · ${entry.versionLabel}` : "";
            const publisherPart = entry.publisherName ? ` · ${entry.publisherName}` : "";

            const l3Badge =
              disclosure === "L3"
                ? `<code class="nlos-status-enum nlos-l3-only">${escapeHtml(formatL3IdentityLine(entry))}</code>`
                : "";

            const lockBadge = verbs.uninstallLockedByActiveTasks
              ? `<span class="nlos-statutory-lock" data-uninstall-lock="active-tasks">` +
                `${renderIconSvg("lock", { size: 14, strokeWidth: 1.25 })}` +
                `<span>${escapeHtml(copy.activeTasksLockReason)}</span>` +
                `</span>`
              : "";

            return (
              `<li class="nlos-launcher-row" data-launcher-index="${idx}" data-app-status="${entry.status}">` +
              `<div class="nlos-launcher-row__top">` +
              `<div class="nlos-launcher-row__identity">` +
              `${renderIconSvg(iconName, { size: 14, strokeWidth: 1.5 })}` +
              `<span class="nlos-launcher-row__name">${escapeHtml(entry.displayName)}</span>` +
              `</div>` +
              `<span class="nlos-launcher-row__meta">${escapeHtml(`${statusText}${versionPart}${publisherPart}`)}</span>` +
              `</div>` +
              `${l3Badge}` +
              `<div class="nlos-launcher-row__actions">` +
              `<button type="button" class="nlos-btn nlos-btn--primary nlos-launcher-row__btn" ` +
              `data-launcher-action="launch"${verbs.canLaunch ? "" : " disabled"}>` +
              `<span>${escapeHtml(copy.launchBtn)}</span>` +
              `</button>` +
              `<button type="button" class="nlos-btn nlos-launcher-row__btn" ` +
              `data-launcher-action="disable-application"${verbs.canDisable ? "" : " disabled"}>` +
              `<span>${escapeHtml(copy.disableBtn)}</span>` +
              `</button>` +
              `<button type="button" class="nlos-btn nlos-launcher-row__btn" ` +
              `data-launcher-action="uninstall-application"${verbs.canUninstall ? "" : " disabled"}>` +
              `<span>${escapeHtml(copy.uninstallBtn)}</span>` +
              `</button>` +
              `${lockBadge}` +
              `</div>` +
              `</li>`
            );
          })
          .join("");

  return (
    `<section role="dialog" aria-label="${escapeHtml(copy.heading)}" class="nlos-launcher-flyout">` +
    `<header class="nlos-launcher-flyout__header">` +
    `<span>${escapeHtml(copy.heading)}</span>` +
    `<button type="button" class="nlos-wm-ctrl-btn" data-launcher-action="close" aria-label="${escapeHtml(copy.closeAria)}">` +
    `${renderIconSvg("close", { size: 14, strokeWidth: 1.5 })}` +
    `</button>` +
    `</header>` +
    `<ul class="nlos-launcher-list">${itemsHtml}</ul>` +
    `</section>`
  );
}

export function createLauncherMenuElement(options: LauncherMenuOptions): HTMLElement {
  const doc = options.doc ?? document;
  const locale: WorkstationLocale = options.locale ?? "zh-CN";
  const disclosure: DisclosureLevel = options.disclosure ?? "L1";
  const copy = LAUNCHER_COPY[locale];
  const model = resolveModel(options);
  const entries = model.getInstalledApplications();

  const root = doc.createElement("section");
  root.className = "nlos-launcher-flyout";
  root.setAttribute("role", "dialog");
  root.setAttribute("aria-label", copy.heading);

  const header = doc.createElement("header");
  header.className = "nlos-launcher-flyout__header";

  const titleSpan = doc.createElement("span");
  titleSpan.textContent = copy.heading;
  header.appendChild(titleSpan);

  const closeBtn = doc.createElement("button");
  closeBtn.type = "button";
  closeBtn.className = "nlos-wm-ctrl-btn";
  closeBtn.setAttribute("data-launcher-action", "close");
  closeBtn.setAttribute("aria-label", copy.closeAria);
  closeBtn.appendChild(createIconElement("close", { size: 14, strokeWidth: 1.5, doc }));
  closeBtn.addEventListener("click", () => {
    options.callbacks?.onClose?.();
  });
  header.appendChild(closeBtn);
  root.appendChild(header);

  const list = doc.createElement("ul");
  list.className = "nlos-launcher-list";

  if (entries.length === 0) {
    const emptyLi = doc.createElement("li");
    emptyLi.className = "nlos-launcher-row";
    const emptyMeta = doc.createElement("span");
    emptyMeta.className = "nlos-launcher-row__meta";
    emptyMeta.textContent = copy.emptyState;
    emptyLi.appendChild(emptyMeta);
    list.appendChild(emptyLi);
  } else {
    entries.forEach((entry, idx) => {
      const verbs = computeLauncherAppVerbs(entry);
      const li = doc.createElement("li");
      li.className = "nlos-launcher-row";
      li.setAttribute("data-launcher-index", String(idx));
      li.setAttribute("data-app-status", entry.status);

      const topRow = doc.createElement("div");
      topRow.className = "nlos-launcher-row__top";

      const identity = doc.createElement("div");
      identity.className = "nlos-launcher-row__identity";
      identity.appendChild(
        createIconElement(entry.icon ?? "app-launcher", {
          size: 14,
          strokeWidth: 1.5,
          doc,
        }),
      );

      const nameSpan = doc.createElement("span");
      nameSpan.className = "nlos-launcher-row__name";
      nameSpan.textContent = entry.displayName;
      identity.appendChild(nameSpan);
      topRow.appendChild(identity);

      const metaSpan = doc.createElement("span");
      metaSpan.className = "nlos-launcher-row__meta";
      const statusText = statusLabelForEntry(entry.status, copy);
      const versionPart = entry.versionLabel ? ` · ${entry.versionLabel}` : "";
      const publisherPart = entry.publisherName ? ` · ${entry.publisherName}` : "";
      metaSpan.textContent = `${statusText}${versionPart}${publisherPart}`;
      topRow.appendChild(metaSpan);

      li.appendChild(topRow);

      if (disclosure === "L3") {
        const l3Code = doc.createElement("code");
        l3Code.className = "nlos-status-enum nlos-l3-only";
        l3Code.textContent = formatL3IdentityLine(entry);
        li.appendChild(l3Code);
      }

      const actions = doc.createElement("div");
      actions.className = "nlos-launcher-row__actions";

      const launchBtn = doc.createElement("button");
      launchBtn.type = "button";
      launchBtn.className = "nlos-btn nlos-btn--primary nlos-launcher-row__btn";
      launchBtn.setAttribute("data-launcher-action", "launch");
      launchBtn.disabled = !verbs.canLaunch;
      const launchLabel = doc.createElement("span");
      launchLabel.textContent = copy.launchBtn;
      launchBtn.appendChild(launchLabel);
      launchBtn.addEventListener("click", () => {
        const res = model.validateAndPrepareLaunch({
          applicationIdHex: entry.applicationIdHex,
          installationIdHex: entry.installationIdHex,
        });
        if (res.ok) {
          options.callbacks?.onLaunch?.(res);
        } else {
          options.callbacks?.onLaunchRejected?.(res);
        }
      });
      actions.appendChild(launchBtn);

      const disableBtn = doc.createElement("button");
      disableBtn.type = "button";
      disableBtn.className = "nlos-btn nlos-launcher-row__btn";
      disableBtn.setAttribute("data-launcher-action", "disable-application");
      disableBtn.disabled = !verbs.canDisable;
      const disableLabel = doc.createElement("span");
      disableLabel.textContent = copy.disableBtn;
      disableBtn.appendChild(disableLabel);
      disableBtn.addEventListener("click", () => {
        options.callbacks?.onDisableApplication?.({
          applicationIdHex: entry.applicationIdHex,
          installationIdHex: entry.installationIdHex,
          expectedGenerationOrRevision:
            entry.expectedGenerationOrRevision ?? entry.installationGeneration,
        });
      });
      actions.appendChild(disableBtn);

      const uninstallBtn = doc.createElement("button");
      uninstallBtn.type = "button";
      uninstallBtn.className = "nlos-btn nlos-launcher-row__btn";
      uninstallBtn.setAttribute("data-launcher-action", "uninstall-application");
      uninstallBtn.disabled = !verbs.canUninstall;
      const uninstallLabel = doc.createElement("span");
      uninstallLabel.textContent = copy.uninstallBtn;
      uninstallBtn.appendChild(uninstallLabel);
      uninstallBtn.addEventListener("click", () => {
        if (!verbs.canUninstall) {
          return;
        }
        options.callbacks?.onUninstallApplication?.({
          applicationIdHex: entry.applicationIdHex,
          installationIdHex: entry.installationIdHex,
          expectedGenerationOrRevision:
            entry.expectedGenerationOrRevision ?? entry.installationGeneration,
        });
      });
      actions.appendChild(uninstallBtn);

      if (verbs.uninstallLockedByActiveTasks) {
        const lockSpan = doc.createElement("span");
        lockSpan.className = "nlos-statutory-lock";
        lockSpan.setAttribute("data-uninstall-lock", "active-tasks");
        lockSpan.appendChild(
          createIconElement("lock", { size: 14, strokeWidth: 1.25, doc }),
        );
        const reasonSpan = doc.createElement("span");
        reasonSpan.textContent = copy.activeTasksLockReason;
        lockSpan.appendChild(reasonSpan);
        actions.appendChild(lockSpan);
      }

      li.appendChild(actions);
      list.appendChild(li);
    });
  }

  root.appendChild(list);
  return root;
}
