// Monochrome geometric SVG stroke icon primitives for NLOS Industrial Workstation.
// Strict compliance: 14x14 / 16x16 pixel-aligned viewBox, 1.25px-1.5px stroke,
// stroke="currentColor", fill="none", zero pictographic characters.

const SVG_NS = "http://www.w3.org/2000/svg";

export type IconSize = 14 | 16;
export type IconStrokeWidth = 1.25 | 1.5;

export type IconName =
  | "app-launcher"
  | "command-prompt"
  | "search"
  | "layout-1-pane"
  | "layout-2-col"
  | "layout-3-pane"
  | "layout-quadrant"
  | "layout-float"
  | "lock"
  | "shield-seal"
  | "theme-switch"
  | "disclosure-l0"
  | "disclosure-l1"
  | "disclosure-l2"
  | "disclosure-l3"
  | "hw-microphone"
  | "hw-camera"
  | "hw-screen"
  | "notification-tray"
  | "chevron-right"
  | "chevron-down"
  | "tree-expand"
  | "pause"
  | "play"
  | "stop"
  | "retry"
  | "close"
  | "check"
  | "warning"
  | "danger"
  | "external-jump"
  | "copy";

export interface IconSpec {
  readonly name: IconName;
  readonly paths: readonly string[];
}

export interface IconRenderOptions {
  readonly size?: IconSize;
  readonly strokeWidth?: IconStrokeWidth;
  readonly className?: string;
  readonly title?: string;
  readonly doc?: Document;
}

const ICON_SPECS: Readonly<Record<IconName, readonly string[]>> = {
  "app-launcher": [
    "M2.5 2.5h4.5v4.5H2.5z",
    "M9 2.5h4.5v4.5H9z",
    "M2.5 9h4.5v4.5H2.5z",
    "M9 9h4.5v4.5H9z",
  ],
  "command-prompt": ["M3 4.5L6.5 8 3 11.5", "M8 11.5h5"],
  search: ["M7 11.5a4.5 4.5 0 1 0 0-9 4.5 4.5 0 0 0 0 9z", "M10.5 10.5L13.5 13.5"],
  "layout-1-pane": ["M2 2.5h12v11H2z"],
  "layout-2-col": ["M2 2.5h12v11H2z", "M8 2.5v11"],
  "layout-3-pane": ["M2 2.5h12v11H2z", "M8 2.5v7", "M2 9.5h12"],
  "layout-quadrant": ["M2 2.5h12v11H2z", "M8 2.5v11", "M2 8h12"],
  "layout-float": ["M2 3.5h8.5v7.5H2z", "M5.5 6h8.5v7.5H5.5z"],
  lock: ["M4 7.5h8v6H4z", "M5.5 7.5V5a2.5 2.5 0 0 1 5 0v2.5"],
  "shield-seal": ["M8 2L3 4v4.2c0 3 2.1 5 5 5.8 2.9-.8 5-2.8 5-5.8V4L8 2z", "M6 8l1.5 1.5L10.5 6.5"],
  "theme-switch": ["M8 13.5a5.5 5.5 0 1 0 0-11 5.5 5.5 0 0 0 0 11z", "M8 2.5v11"],
  "disclosure-l0": ["M3 12.5h10"],
  "disclosure-l1": ["M3 6h10", "M3 10.5h10"],
  "disclosure-l2": ["M2.5 3h11v10h-11z", "M9.5 3v10", "M4.5 6.5h3", "M4.5 9.5h3"],
  "disclosure-l3": ["M2.5 4h11", "M2.5 7h8", "M5.5 10h8", "M2.5 13h11"],
  "hw-microphone": [
    "M6.25 4.25a1.75 1.75 0 0 1 3.5 0v3.25a1.75 1.75 0 0 1-3.5 0z",
    "M4 7.5a4 4 0 0 0 8 0",
    "M8 11.5v2.5",
    "M5.5 14h5",
  ],
  "hw-camera": ["M2 4.5h8.5v7H2z", "M10.5 6.75L14 4.75v6.5l-3.5-2"],
  "hw-screen": ["M2 3h12v8H2z", "M5.5 13.5h5", "M8 11v2.5"],
  "notification-tray": ["M2.5 3.5h11v9h-11z", "M2.5 9.5h3.2l1.1 1.5h2.4l1.1-1.5h3.2"],
  "chevron-right": ["M6 3.5L10.5 8 6 12.5"],
  "chevron-down": ["M3.5 6L8 10.5 12.5 6"],
  "tree-expand": ["M3 3v10h10", "M6.5 6.5h5", "M6.5 9.5h3.5"],
  pause: ["M5.5 3.5v9", "M10.5 3.5v9"],
  play: ["M5 3.5L12 8l-7 4.5z"],
  stop: ["M4 4h8v8H4z"],
  retry: ["M12.5 5.5A5 5 0 1 0 13 9", "M12.5 2.5v3h-3"],
  close: ["M4 4l8 8", "M12 4l-8 8"],
  check: ["M3 8.5l3.2 3.2L13 4.8"],
  warning: ["M8 2.5L14 13H2L8 2.5z", "M8 6.5v3", "M8 11.2v.4"],
  danger: ["M5.2 2.5h5.6L13.5 5.2v5.6l-2.7 2.7H5.2L2.5 10.8V5.2z", "M5.8 5.8l4.4 4.4", "M10.2 5.8l-4.4 4.4"],
  "external-jump": ["M9.5 3h3.5v3.5", "M13 3L7.5 8.5", "M11.5 9.5v3.5H3V4.5h3.5"],
  copy: ["M5.5 5.5h7.5v8H5.5z", "M3 10.5V3h7.5"],
};

export const ICON_NAMES: readonly IconName[] = Object.keys(ICON_SPECS) as IconName[];

function escapeXml(raw: string): string {
  return raw
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

export function getIconSpec(name: IconName): IconSpec {
  return {
    name,
    paths: ICON_SPECS[name],
  };
}

export function renderIconSvg(name: IconName, options?: IconRenderOptions): string {
  const size: IconSize = options?.size ?? 16;
  const strokeWidth: IconStrokeWidth = options?.strokeWidth ?? 1.5;
  const className = options?.className ? `nlos-icon ${options.className}` : "nlos-icon";
  const paths = ICON_SPECS[name]
    .map((d) => `<path d="${escapeXml(d)}" stroke-linecap="square" stroke-linejoin="miter" />`)
    .join("");

  if (options?.title && options.title.trim().length > 0) {
    const safeTitle = escapeXml(options.title);
    return (
      `<svg xmlns="${SVG_NS}" class="${escapeXml(className)}" data-icon="${name}" ` +
      `width="${size}" height="${size}" viewBox="0 0 16 16" fill="none" ` +
      `stroke="currentColor" stroke-width="${strokeWidth}" role="img" aria-label="${safeTitle}">` +
      `<title>${safeTitle}</title>${paths}</svg>`
    );
  }

  return (
    `<svg xmlns="${SVG_NS}" class="${escapeXml(className)}" data-icon="${name}" ` +
    `width="${size}" height="${size}" viewBox="0 0 16 16" fill="none" ` +
    `stroke="currentColor" stroke-width="${strokeWidth}" aria-hidden="true">${paths}</svg>`
  );
}

export function createIconElement(name: IconName, options?: IconRenderOptions): SVGSVGElement {
  const doc = options?.doc ?? document;
  const size: IconSize = options?.size ?? 16;
  const strokeWidth: IconStrokeWidth = options?.strokeWidth ?? 1.5;
  const svg = doc.createElementNS(SVG_NS, "svg");
  svg.setAttribute("class", options?.className ? `nlos-icon ${options.className}` : "nlos-icon");
  svg.setAttribute("data-icon", name);
  svg.setAttribute("width", String(size));
  svg.setAttribute("height", String(size));
  svg.setAttribute("viewBox", "0 0 16 16");
  svg.setAttribute("fill", "none");
  svg.setAttribute("stroke", "currentColor");
  svg.setAttribute("stroke-width", String(strokeWidth));

  if (options?.title && options.title.trim().length > 0) {
    svg.setAttribute("role", "img");
    svg.setAttribute("aria-label", options.title);
    const titleNode = doc.createElementNS(SVG_NS, "title");
    titleNode.textContent = options.title;
    svg.appendChild(titleNode);
  } else {
    svg.setAttribute("aria-hidden", "true");
  }

  for (const d of ICON_SPECS[name]) {
    const path = doc.createElementNS(SVG_NS, "path");
    path.setAttribute("d", d);
    path.setAttribute("stroke-linecap", "square");
    path.setAttribute("stroke-linejoin", "miter");
    svg.appendChild(path);
  }

  return svg;
}
