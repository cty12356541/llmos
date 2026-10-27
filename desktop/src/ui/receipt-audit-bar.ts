// L3-Only Monospace Receipt & CLI Byte-Parity Verification Audit Bar.
// Authority: Stage D Desktop Visual & UX Spec §3.2.D.3, §4.1, [CTRL-PARITY-001].
// Hard Gate: Guaranteed hidden / unrendered when disclosure is L0, L1, or L2.

import { createIconElement, renderIconSvg } from "./icons";
import type { DisclosureLevel, WorkstationLocale } from "./status-indicator";

export type CliParityStatus =
  | "IDLE"
  | "VERIFYING"
  | "PARITY_MATCH"
  | "PARITY_MISMATCH";

export interface ReceiptParityComparison {
  readonly validHex: boolean;
  readonly matched: boolean;
  readonly normalizedGuiHex: string;
  readonly normalizedCliHex: string;
}

export interface ReceiptAuditBarOptions {
  readonly disclosure: DisclosureLevel;
  readonly controlCommandIdHex: string;
  readonly receiptHex: string;
  readonly correlationIdHex?: string;
  readonly cellId?: string;
  readonly leaseEpoch?: number;
  readonly nodeBootGeneration?: number;
  readonly parityStatus?: CliParityStatus;
  readonly locale?: WorkstationLocale;
  readonly onCopyCommandId?: (commandIdHex: string) => void;
  readonly onCopyReceiptHex?: (receiptHex: string) => void;
  readonly onVerifyCliParity?: () => Promise<string> | string;
  readonly doc?: Document;
}

export interface ReceiptAuditBarController {
  readonly element: HTMLElement;
  getParityStatus(): CliParityStatus;
  setParityStatus(status: CliParityStatus): void;
  verifyAgainstCliOutput(cliOutputOrHex: string): ReceiptParityComparison;
}

const HEX_RE = /^[0-9a-f]+$/;

function escapeHtml(raw: string): string {
  return raw
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

export function shouldRenderReceiptAuditBar(disclosure: DisclosureLevel): boolean {
  return disclosure === "L3";
}

function normalizeCliReceiptOutput(raw: string): string {
  const firstLine = raw.trim().split(/\r?\n/)[0]?.trim() ?? "";
  const stripped = firstLine.startsWith("RECEIPT ")
    ? firstLine.slice("RECEIPT ".length).trim()
    : firstLine;
  return stripped.toLowerCase();
}

/** Fail-closed byte-for-byte hex comparison between GUI receipt_hex and CLI RECEIPT output. */
export function verifyReceiptHexParity(
  guiReceiptHex: string,
  cliOutputOrHex: string,
): ReceiptParityComparison {
  const normalizedGuiHex = guiReceiptHex.trim().toLowerCase();
  const normalizedCliHex = normalizeCliReceiptOutput(cliOutputOrHex);

  const validGui =
    normalizedGuiHex.length > 0 &&
    normalizedGuiHex.length % 2 === 0 &&
    HEX_RE.test(normalizedGuiHex);
  const validCli =
    normalizedCliHex.length > 0 &&
    normalizedCliHex.length % 2 === 0 &&
    HEX_RE.test(normalizedCliHex);
  const validHex = validGui && validCli;

  return {
    validHex,
    matched: validHex && normalizedGuiHex === normalizedCliHex,
    normalizedGuiHex,
    normalizedCliHex,
  };
}

function formatReceiptPreview(hex: string): string {
  const trimmed = hex.trim();
  if (trimmed.length <= 24) {
    return trimmed;
  }
  return `${trimmed.slice(0, 12)}...${trimmed.slice(-12)} (${trimmed.length / 2}B)`;
}

export function renderReceiptAuditBarHtml(options: ReceiptAuditBarOptions): string {
  if (!shouldRenderReceiptAuditBar(options.disclosure)) {
    return "";
  }
  const locale: WorkstationLocale = options.locale ?? "zh-CN";
  const parityStatus: CliParityStatus = options.parityStatus ?? "IDLE";
  const copySvg = renderIconSvg("copy", { size: 14, strokeWidth: 1.25 });

  const cellSegment =
    options.cellId !== undefined
      ? `<span>Cell=${escapeHtml(options.cellId)}` +
        (options.leaseEpoch !== undefined ? ` lease_epoch=${options.leaseEpoch}` : "") +
        (options.nodeBootGeneration !== undefined
          ? ` boot_gen=${options.nodeBootGeneration}`
          : "") +
        `</span>`
      : "";

  const copyCmdLabel = locale === "zh-CN" ? "复制命令 ID" : "Copy Command ID";
  const copyRcptLabel = locale === "zh-CN" ? "复制 receipt_hex" : "Copy receipt_hex";
  const verifyLabel =
    locale === "zh-CN" ? "核验 CLI 字节一致性" : "Verify CLI Byte Parity";

  return (
    `<footer class="nlos-receipt-audit-bar nlos-l3-only" data-component="receipt-audit-bar" ` +
    `data-parity-status="${parityStatus}" aria-label="Cryptographic Receipt Audit Bar">` +
    `<div style="display:inline-flex;align-items:center;gap:8px;flex-wrap:wrap">` +
    `${cellSegment}` +
    `<span>control_command_id=${escapeHtml(options.controlCommandIdHex)}</span>` +
    `<button type="button" class="nlos-btn" data-action="copy-command-id">${copySvg}${escapeHtml(copyCmdLabel)}</button>` +
    `<span>receipt_hex=${escapeHtml(formatReceiptPreview(options.receiptHex))}</span>` +
    `<button type="button" class="nlos-btn" data-action="copy-receipt-hex">${copySvg}${escapeHtml(copyRcptLabel)}</button>` +
    `</div>` +
    `<div style="display:inline-flex;align-items:center;gap:8px">` +
    `<code class="nlos-status-enum" data-role="parity-badge">${parityStatus}</code>` +
    `<button type="button" class="nlos-btn" data-action="verify-cli-parity">${escapeHtml(verifyLabel)}</button>` +
    `</div>` +
    `</footer>`
  );
}

export function createReceiptAuditBarElement(
  options: ReceiptAuditBarOptions,
): ReceiptAuditBarController | null {
  if (!shouldRenderReceiptAuditBar(options.disclosure)) {
    return null;
  }
  const doc = options.doc ?? document;
  const locale: WorkstationLocale = options.locale ?? "zh-CN";
  let parityStatus: CliParityStatus = options.parityStatus ?? "IDLE";

  const footer = doc.createElement("footer");
  footer.className = "nlos-receipt-audit-bar nlos-l3-only";
  footer.setAttribute("data-component", "receipt-audit-bar");
  footer.setAttribute("data-parity-status", parityStatus);
  footer.setAttribute("aria-label", "Cryptographic Receipt Audit Bar");

  const left = doc.createElement("div");
  left.style.display = "inline-flex";
  left.style.alignItems = "center";
  left.style.gap = "8px";
  left.style.flexWrap = "wrap";

  if (options.cellId !== undefined) {
    const cellSpan = doc.createElement("span");
    const epochPart =
      options.leaseEpoch !== undefined ? ` lease_epoch=${options.leaseEpoch}` : "";
    const bootPart =
      options.nodeBootGeneration !== undefined
        ? ` boot_gen=${options.nodeBootGeneration}`
        : "";
    cellSpan.textContent = `Cell=${options.cellId}${epochPart}${bootPart}`;
    left.appendChild(cellSpan);
  }

  const cmdSpan = doc.createElement("span");
  cmdSpan.textContent = `control_command_id=${options.controlCommandIdHex}`;
  left.appendChild(cmdSpan);

  const copyCmdBtn = doc.createElement("button");
  copyCmdBtn.type = "button";
  copyCmdBtn.className = "nlos-btn";
  copyCmdBtn.setAttribute("data-action", "copy-command-id");
  copyCmdBtn.appendChild(createIconElement("copy", { size: 14, strokeWidth: 1.25, doc }));
  copyCmdBtn.appendChild(
    doc.createTextNode(locale === "zh-CN" ? "复制命令 ID" : "Copy Command ID"),
  );
  copyCmdBtn.addEventListener("click", () => {
    options.onCopyCommandId?.(options.controlCommandIdHex);
  });
  left.appendChild(copyCmdBtn);

  const rcptSpan = doc.createElement("span");
  rcptSpan.title = options.receiptHex;
  rcptSpan.textContent = `receipt_hex=${formatReceiptPreview(options.receiptHex)}`;
  left.appendChild(rcptSpan);

  const copyRcptBtn = doc.createElement("button");
  copyRcptBtn.type = "button";
  copyRcptBtn.className = "nlos-btn";
  copyRcptBtn.setAttribute("data-action", "copy-receipt-hex");
  copyRcptBtn.appendChild(createIconElement("copy", { size: 14, strokeWidth: 1.25, doc }));
  copyRcptBtn.appendChild(
    doc.createTextNode(locale === "zh-CN" ? "复制 receipt_hex" : "Copy receipt_hex"),
  );
  copyRcptBtn.addEventListener("click", () => {
    options.onCopyReceiptHex?.(options.receiptHex);
  });
  left.appendChild(copyRcptBtn);

  const right = doc.createElement("div");
  right.style.display = "inline-flex";
  right.style.alignItems = "center";
  right.style.gap = "8px";

  const parityBadge = doc.createElement("code");
  parityBadge.className = "nlos-status-enum";
  parityBadge.setAttribute("data-role", "parity-badge");
  parityBadge.textContent = parityStatus;

  const setParityStatus = (next: CliParityStatus): void => {
    parityStatus = next;
    footer.setAttribute("data-parity-status", parityStatus);
    parityBadge.textContent = parityStatus;
  };

  const verifyAgainstCliOutput = (cliOutputOrHex: string): ReceiptParityComparison => {
    const result = verifyReceiptHexParity(options.receiptHex, cliOutputOrHex);
    setParityStatus(result.matched ? "PARITY_MATCH" : "PARITY_MISMATCH");
    return result;
  };

  const verifyBtn = doc.createElement("button");
  verifyBtn.type = "button";
  verifyBtn.className = "nlos-btn";
  verifyBtn.setAttribute("data-action", "verify-cli-parity");
  verifyBtn.textContent =
    locale === "zh-CN" ? "核验 CLI 字节一致性" : "Verify CLI Byte Parity";
  verifyBtn.addEventListener("click", () => {
    if (!options.onVerifyCliParity) {
      return;
    }
    setParityStatus("VERIFYING");
    Promise.resolve(options.onVerifyCliParity())
      .then((cliOut) => {
        verifyAgainstCliOutput(cliOut);
      })
      .catch(() => {
        setParityStatus("PARITY_MISMATCH");
      });
  });

  right.appendChild(parityBadge);
  right.appendChild(verifyBtn);
  footer.appendChild(left);
  footer.appendChild(right);

  return {
    element: footer,
    getParityStatus: () => parityStatus,
    setParityStatus,
    verifyAgainstCliOutput,
  };
}
