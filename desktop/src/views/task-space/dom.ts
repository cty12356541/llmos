// Task Space(W33-F,X-2 最小只读任务空间)的 DOM 小件。
// 形状与 desktop/src/main.ts 的同名助手保持一致——本目录是独立写集,
// 不从 main.ts 反向导入(那边无导出);样式类复用既有 style.css。

export function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  options?: { className?: string; text?: string },
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  if (options?.className !== undefined) {
    node.className = options.className;
  }
  if (options?.text !== undefined) {
    node.textContent = options.text;
  }
  return node;
}

export function fieldRow(label: string, value: string): HTMLElement {
  const row = el("div", { className: "field-row" });
  row.append(el("span", { className: "field-label", text: label }));
  row.append(el("span", { className: "field-value", text: value }));
  return row;
}

export function labeledInput(
  label: string,
  placeholder: string,
  value: string,
): { row: HTMLElement; input: HTMLInputElement } {
  const row = el("div", { className: "field-row" });
  row.append(el("span", { className: "field-label", text: label }));
  const input = el("input");
  input.type = "text";
  input.placeholder = placeholder;
  input.value = value;
  input.spellcheck = false;
  row.append(input);
  return { row, input };
}

/** 类型化命令失败(DesktopError {code,message})卡片——与主壳同形,不倾倒原始错误。 */
export function showError(container: HTMLElement, error: unknown): void {
  container.replaceChildren();
  const card = el("section", { className: "card error" });
  card.append(el("h3", { text: "命令失败" }));
  let code = "UNKNOWN";
  let message = String(error);
  if (typeof error === "object" && error !== null && "code" in error && "message" in error) {
    const typed = error as { code?: unknown; message?: unknown };
    if (typeof typed.code === "string") {
      code = typed.code;
    }
    if (typeof typed.message === "string") {
      message = typed.message;
    }
  }
  card.append(fieldRow("code", code), fieldRow("message", message));
  container.append(card);
}
