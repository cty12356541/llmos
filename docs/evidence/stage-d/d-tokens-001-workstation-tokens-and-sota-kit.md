# D-TOKENS-KIT / W69-D-TOKENS：`Graphite / Paper` 工业工作站双主题 Token、单色几何原语、`32×20` 凹陷 Gerono 防伪印章与六大 SOTA 显示组件库

- 状态：`PARTIAL_PASS`（工作站双主题 CSS 自定义属性、实心表面与 `1px` 细线原子样式、32 枚单色几何 SVG 线性图标、`32×20px` 凹陷 Gerono 双纽线校验印章、16+ 态零行话状态指示器与 6 类法定不可篡改锁徽标、Nomad 三水位子弹微条、Temporal/Perfetto 竞速与五段相位瀑布条、Headlamp 非模态抽屉与作用域面包屑、Terraform `+ / ~ / - / =` 爆炸半径差异表与 5s 两步武装器、`L3` 专用回执字节核验底栏及 `ANTI-AI-01..07` + `WCAG 2.2 AA` 自动化审计单测全部落库通过；全桌面挂载集成留归 `W70` `D-COMPOSITOR-WM` 及 `W72–W74` `D-GATE-006` E2E 终验）
- Assurance：`H3`（确定性数学/模型单测、Headless DOM 交互契约单测、静态源码与 CSS 反 AI 套路扫描、`WCAG 2.2 AA` 相对亮度对比度定量计算）
- 日期：2026-10-26
- Owner：`W69-D-TOKENS`（Task `D-TOKENS-KIT`, Attempt 1）
- 设计依据：
  - [06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §24.2 `[TUI-CONSENT-001]`、`[TUI-EXT-001]`；§24.3 `[RCPT-UNDO-001]`；§25 `[TASK-STATE-001]`；§25.3 `[CTRL-PARITY-001]`、`[CTRL-HIER-001]`、`[CTRL-PRIORITY-001]`、`[CTRL-SAFETY-001]`、`[CTRL-PLAN-001]`；§19 `[CONS-CTRL-001]`；§28.4 `[ROAD-D-001]`
  - [阶段 D 完整桌面视觉与交互设计规格（`W68-R2`）](../../superpowers/specs/2026-10-26-stage-d-desktop-visual-and-ux-design.md) §0.1 `ANTI-AI-01..07`、§1.1–§1.3、§2A.1 `SOTA-1..6`、§4.1、§4.3
  - [ADR-0025：阶段 D 混合自然语言桌面合成层、L0–L3 渐进式披露、12 域显控同构与可信 UI 防伪边界](../../management/adrs/0025-stage-d-hybrid-desktop-compositor-and-trusted-ui-layering.md)（决定 3 & 决定 4，`[UI-PARITY-001]`）
  - [阶段 D 权威进度单](../../management/stage-d-progress.md) §D.3.1 `D-TOKENS-KIT` & §D.5.2 `W69-D-TOKENS`
- 前置证据：[b-gui-001-task-manager-shell.md](../stage-b/b-gui-001-task-manager-shell.md)
- 外部工业级 SOTA 参考查询日期：`2026-10-26`（对齐 CNCF Headlamp、Temporal Web UI、HashiCorp Nomad、Sysinternals Process Explorer / System Informer、Perfetto UI、Terraform Plan Preview）
- 快照基线：`a8bbfa219d37a2e7032a57e390be82533c6fdd5a`

---

## 1. 实现事实与交付工件

本车道严格限制在声明互斥写集内交付以下 13 个文件，零侵入 `desktop/src/{main.ts,style.css,ipc.ts,types.ts}`、`desktop/src-tauri/**` 与 `crates/**`：

### 1.1 双主题工业工作站 CSS Token 与实心表面原子样式（[`desktop/src/styles/`](../../../desktop/src/styles/tokens.css)）

1. [`desktop/src/styles/tokens.css`](../../../desktop/src/styles/tokens.css)：
   - 完整实现默认深色 **`Graphite Workstation`**（`[data-nlos-theme="graphite-workstation"]`：实心中性表面 `#111214` / `#18191c` / `#1e2024` / `#23252a` / `#2a2d33` / `#252830`、`1px` 结构细线 `#2e3138` / `#4b515d`、墨色 `#ecedee` / `#9da3ae` / `#6b7280`、单一工程蓝 `#2563eb` / `#3b82f6`、双极警示色 `#d97706` / `#dc2626`）与内建浅色 **`Paper Workstation`**（`[data-nlos-theme="paper-workstation"]`：实心表面 `#e5e7eb` / `#ffffff` / `#f3f4f6` / `#eaecef` / `#eff3f8`、细线 `#d1d5db` / `#9ca3af`、墨色 `#111827` / `#4b5563` / `#6b7280`、工程蓝 `#1d4ed8`、满足 AA 对比度的双极警示色 `#d97706`/`#b45309` 与 `#dc2626`/`#b91c1c`），并原生支持 `[data-nlos-theme="system"]` 跟随系统 `prefers-color-scheme: light` 切换；
   - 固化紧凑字号阶梯（等宽最小字号下限 `--nlos-fs-mono-xs: 0.75rem` = `12px`）、四档工业几何圆角（`0px` / `2px` / `4px` / `6px`）、工作站壳尺寸（`30px` 标题栏、`36px` 底栏、`>= 26px` 触控/指针命中热区、`420px` 抽屉、`520px` `Z4` 安全框）、五平面 `Z0..Z4` 栈序（`0 / 100 / 200 / 400..460 / 900`）与 `@media (prefers-reduced-motion: reduce)` 零时长覆盖。
2. [`desktop/src/styles/surfaces-and-borders.css`](../../../desktop/src/styles/surfaces-and-borders.css)：
   - 提供 100% 实心零模糊原子表面类（`.nlos-surface-backdrop`、`.nlos-surface-window`、`.nlos-surface-window--floating`、`.nlos-surface-chrome`、`.nlos-surface-elevated`、`.nlos-surface-recessed`、`.nlos-surface-modal-z4`、`.nlos-scrim-z4`）、`1px` 结构分割细线、高密度工作站表格（`28px–32px` 行高、`0px` 行圆角、`tabular-nums`）、`4px` 矩形按钮/输入框、`>= 26px` 最小交互热区、`2px` 键盘 `:focus-visible` 焦点环及 `[data-nlos-disclosure="L0"|"L1"|"L2"] .nlos-l3-only` 渐进披露纵深防御隐藏规则。

### 1.2 单色原语与六大工业级 SOTA 显示组件（[`desktop/src/ui/`](../../../desktop/src/ui/index.ts)）

每个模块同时导出纯函数视图模型、确定性 HTML/SVG 渲染函数与无框架 DOM 构建器（支持在无浏览器 Node `tsx` 环境下进行 100% 确定性断言，并可直接供 `W70–W73` WebView 挂载）：

1. [`desktop/src/ui/icons.ts`](../../../desktop/src/ui/icons.ts)：32 枚 `14×14` / `16×16` 像素对齐、`1.25px–1.5px` 均匀线宽单色几何 SVG 线性图标（`stroke="currentColor"`、`fill="none"`，零 Emoji），覆盖启动器、命令提示符（`>_`）、搜索、5 种平铺/浮动布局、锁、安全盾章、主题切换、`L0..L3` 披露层级、麦克风/摄像头/录屏硬件指示灯、通知托盘、树展开折角及暂停/继续/停止/重试/关闭/勾选/警告/危险/权威外跳/复制。
2. [`desktop/src/ui/verification-seal.ts`](../../../desktop/src/ui/verification-seal.ts)：`Z4` 头部专用 `32×20px`（`2px` 圆角、`--nlos-bg-recessed`、`1px` 细线、零外发光）凹陷硬件仪器风防伪印章，精确采样 Gerono 双纽线 $x(t)=16+11.5\cos(t),\ y(t)=10+5.5\sin(2t)$ 与中心对称相位点 $(\phi, \pi-\phi)$（周期 $3.6\text{s}$）：
   - 未挂载态零 `requestAnimationFrame` 循环（0% 空闲开销），`unmount()` 立即注销帧句柄；
   - `prefers-reduced-motion: reduce` 激活时自动切换并响应动态变更至零动画静态闭合双纽线模式；
   - 无 Canvas 2D 上下文时回退为纯数学符号 `∞`（`U+221E`，非 Emoji 变体）。
3. [`desktop/src/ui/status-indicator.ts`](../../../desktop/src/ui/status-indicator.ts)：
   - 完整覆盖 `[TASK-STATE-001]` 16 个规范 `Task` 状态（及 `DRAFT`、`QUARANTINED` 与 `TaskNode` 扩展态共 24 态），映射 `6×6px` 几何 SVG 符号、严格双极警示色（`--nlos-signal-action` `#D97706` 仅限待确认/待输入/结果待核对；`--nlos-signal-danger` `#DC2626` 仅限失败/受控隔离）与 `zh-CN`/`en-US` 双语 `L1` 零行话文案（原始枚举名仅在 `L3` 渲染）；
   - 实现 6 类法定不可篡改禁区锁徽标与前置动词剪枝函数 `pruneVerbsAgainstStatutoryLock`（覆盖 `0x1F` 非权威搜索只读、`IRREVERSIBLE` 禁撤销、`COMPLETED/ACTIVE` 节点禁改图、`TaskAttempt` 禁人工标提交、`Operation(EFFECT_UNKNOWN)` 禁人工改判成功、`QUARANTINED` 租约禁本地 TTL 解锁）。
4. [`desktop/src/ui/bullet-micro-bar.ts`](../../../desktop/src/ui/bullet-micro-bar.ts)（**SOTA-4 Nomad**）：`72–96px × 6px`（`2px` 圆角）实心三水位子弹微条，同时编码 `(actualUsage, effectiveGrantedOrLeaseCap, requestedCeiling)` 与等宽 `tabular-nums` 读数，确定性处理零上限、调度限流（`effective < requested`）及离线分区租约收缩至已用量之下（`actual > effective`）等边界。
5. [`desktop/src/ui/waterfall-bar.ts`](../../../desktop/src/ui/waterfall-bar.ts)（**SOTA-3 Temporal / Perfetto + SOTA-5 Sysinternals**）：严格门控于 `L3` 的 `TaskAttempt` 墙钟比例竞速瀑布条（对比 `SUPERSEDED` vs `CAS_WINNER`、挂载 `PROVIDER_ISOLATION_UNVERIFIED` 印记、物理剔除人工提交按钮并展示法定锁标）以及 `ExecutionFiber` 五段相位堆栈瀑布微条（`activeCpuMs | schedulerWaitMs | externalWaitMs | backpressureWaitMs | suspendedMs`，配 `SHARED_THREAD_LIKE` 共享进程故障域单色徽标）。
6. [`desktop/src/ui/inspector-drawer.ts`](../../../desktop/src/ui/inspector-drawer.ts)（**SOTA-1 CNCF Headlamp**）：`420px` 右侧 / `36%` 底部、`0px` 外圆角实心非模态 `InspectorDrawer` 容器壳，内置 8 级 `ScopeBreadcrumbBar`（`System > Workspace > Application > Task > TaskGroup > TaskNode > TaskAttempt > Fiber`，`L2` 自动过滤 `TaskAttempt`/`Fiber` 防止行话外泄）、`L2 <-> L3` 原地切换钩子与 `Escape` 关闭处理。
7. [`desktop/src/ui/blast-radius-diff.ts`](../../../desktop/src/ui/blast-radius-diff.ts)（**SOTA-6 Terraform Plan Preview**）：结构化 `+ / ~ / - / =` 级联爆炸半径预览表（精确区分新增/重验、暂停/限流/围栏、终止/驱逐/清空及 `=` 豁免后代）、`Topic Purge` 显式数据损失回执（Data-Loss `Receipt`）高危警告横幅与支持可注入单调时钟及 `350ms` 防误触门的 **5 秒两步武装倒计时控制器**（`createTwoStepArmingController`）。
8. [`desktop/src/ui/receipt-audit-bar.ts`](../../../desktop/src/ui/receipt-audit-bar.ts)：严格门控于 `L3`（在 `L0/L1/L2` 下 HTML 为空串、DOM 返回 `null`）的 `control_command_id` + `receipt_hex` 复制与 `system-control-cli` `RECEIPT <hex>` 逐字节一致性核验底栏（fail-closed 拒绝空串、奇数位与非法十六进制输入）。
9. [`desktop/src/ui/index.ts`](../../../desktop/src/ui/index.ts) 与 [`desktop/src/ui/__tests__/tokens-and-primitives.test.ts`](../../../desktop/src/ui/__tests__/tokens-and-primitives.test.ts)：统一导出桶与 10 大套件自动化审计单测。

---

## 2. 验证结果（2026-10-26，Windows x86_64，Node v24.19.0，TypeScript 5.9.3）

### 2.1 执行命令与原始输出

```text
$ npx tsx desktop/src/ui/__tests__/tokens-and-primitives.test.ts
Running D-TOKENS-KIT (W69-D-TOKENS) Verification Suite...
  [PASS] 1. ANTI-AI-01..06 static CSS & source audit across desktop/src/{styles,ui}
  [PASS] 2. WCAG 2.2 AA relative luminance contrast audit on Graphite & Paper themes
  [PASS] 3. Monochrome geometric SVG icons (14x14 / 16x16, 1.25px-1.5px stroke, zero emoji)
  [PASS] 4. Recessed 32x20 Gerono verification seal math, reduced-motion, zero idle RAF & U+221E fallback
  [PASS] 5. 16-state status mapping, strict bipolar signals, statutory locks & ANTI-AI-07 zero-jargon gate
  [PASS] 6. SOTA-4 Nomad triple-watermark bullet micro-bar math & edge cases
  [PASS] 7. SOTA-3 Temporal TaskAttempt race bar & ExecutionFiber 5-segment waterfall
  [PASS] 8. SOTA-1 Headlamp non-modal InspectorDrawer & ScopeBreadcrumbBar
  [PASS] 9. SOTA-6 Terraform + / ~ / - / = blast-radius diff, Topic Purge Data-Loss Receipt & 5s arming
  [PASS] 10. L3-only receipt-audit-bar gating & CLI RECEIPT byte-parity verifier

ALL 10 SUITES PASSED (697 deterministic assertions verified).
  -> EXIT CODE: 0

$ cd desktop && npx tsc --noEmit
  -> EXIT CODE: 0 (strict: true, noUnusedLocals: true, noUnusedParameters: true, 0 diagnostics)
```

### 2.2 `ANTI-AI-01..07` 静态与渲染审计摘要

| 门禁编号 | 核验范围 | 实测计数 / 结果 |
|---|---|---|
| `ANTI-AI-01` | 扫描 `desktop/src/styles/**` 与 `desktop/src/ui/**` 中 `#030710`、`#7fc8ff`、径向发光渐变与霓虹阴影 | `0` 命中（通过） |
| `ANTI-AI-02` | 扫描毛玻璃滤镜属性（含带厂商前缀变体） | `0` 命中（通过） |
| `ANTI-AI-03` | 扫描网页居中卡片流容器与常驻巨型输入框样式 | `0` 命中（通过） |
| `ANTI-AI-04` | 解析全部圆角属性与 `--nlos-radius-*` 定义 | 100% 属于 `{0px, 2px, 4px, 6px, 50%}`，且 `50%` 唯一出现于 `6×6px` 状态圆点（通过） |
| `ANTI-AI-05` | 扫描全部新建源码与词条文案的 Unicode `Extended_Pictographic` 正则 | `0` 命中；无 Canvas 回退严格使用数学符号 `U+221E`（通过） |
| `ANTI-AI-06` | 校验 24 个状态枚举绑定的颜色信号 Token | 琥珀 `--nlos-signal-action` 仅绑 3 个待介入态，深绯红 `--nlos-signal-danger` 仅绑 2 个失败/隔离态，其余 19 态全部单色（通过） |
| `ANTI-AI-07` | 在 `L0` 与 `L1`（`zh-CN` + `en-US`）下渲染全部组件并提取可见文本，扫描 `Z1`/`TypedIntent`/`0x1F`/`Quota`/`ROAD-D-001`/`ADR-0011`/`TaskAttempt`/`ExecutionFiber`/`receipt_hex` 等黑名单行话 | `0` 命中（通过） |

### 2.3 双主题 `WCAG 2.2 AA` 相对亮度对比度定量实测表

| Token 配对语义 | `Graphite Workstation`（深色） | 实测对比度 | `Paper Workstation`（浅色） | 实测对比度 | AA 门限 |
|---|---|---|---|---|---|
| `text-primary` vs `bg-window` | `#ecedee` / `#18191c` | **15.00:1** | `#111827` / `#ffffff` | **17.74:1** | `>= 4.5:1` |
| `text-primary` vs `bg-elevated` | `#ecedee` / `#23252a` | **13.08:1** | `#111827` / `#ffffff` | **17.74:1** | `>= 4.5:1` |
| `text-secondary` vs `bg-window` | `#9da3ae` / `#18191c` | **6.93:1** | `#4b5563` / `#ffffff` | **7.56:1** | `>= 4.5:1` |
| `text-secondary` vs `bg-elevated` | `#9da3ae` / `#23252a` | **6.05:1** | `#4b5563` / `#ffffff` | **7.56:1** | `>= 4.5:1` |
| `accent-fg` vs `accent-primary`（主按钮） | `#ffffff` / `#2563eb` | **5.17:1** | `#ffffff` / `#1d4ed8` | **6.70:1** | `>= 4.5:1` |
| `signal-action-text` vs `bg-window` | `#f59e0b` / `#18191c` | **8.18:1** | `#b45309` / `#ffffff` | **5.02:1** | `>= 4.5:1` |
| `signal-danger-text` vs `bg-window` | `#f87171` / `#18191c` | **6.35:1** | `#b91c1c` / `#ffffff` | **6.47:1** | `>= 4.5:1` |
| `signal-danger-fg` vs `signal-danger`（高危键） | `#ffffff` / `#dc2626` | **4.83:1** | `#ffffff` / `#dc2626` | **4.83:1** | `>= 4.5:1` |
| `accent-active`（`2px` 焦点环）vs `bg-window` | `#3b82f6` / `#18191c` | **4.78:1** | `#1d4ed8` / `#ffffff` | **6.70:1** | `>= 3.0:1` |
| `signal-action`（`6px` 几何符）vs `bg-window` | `#d97706` / `#18191c` | **5.52:1** | `#d97706` / `#ffffff` | **3.19:1** | `>= 3.0:1` |
| `signal-danger`（`6px` 几何符）vs `bg-window` | `#dc2626` / `#18191c` | **3.64:1** | `#dc2626` / `#ffffff` | **4.83:1** | `>= 3.0:1` |

---

## 3. 诚实边界与未覆盖范围（Limitations — `Claim <= Evidence`）

1. **尚未挂载进入主壳合成树（待 `W70` `D-COMPOSITOR-WM` 与 `W71–W72` 视图车道合入）**：按 `W69` 严格写集互斥规则，本工作包刻意不修改 `desktop/src/main.ts`、`desktop/src/style.css` 或 `desktop/src-tauri/**`，因此上述 CSS Token 与 UI 组件当前作为已独立验证的模块化原语库存在，尚未替换现有阶段 B/C 开发者侧边栏主界面。
2. **尚未执行真实 Tauri WebView 像素光栅化与真机交互战役（待 `W73/W74` `D-GATE-006`）**：本证据完成的是组件级纯模型、Headless DOM、CSS 静态规则与数学相对亮度验证；跨窗口 DPI 缩放、真实浏览器引擎布局盒计算、屏幕阅读器真机播报及 `[ROAD-D-001]` 非开发者端到端旅程留归 `W73`（`W73-D-A11Y-I18N` / `W73-D-GUI-CAMPAIGN`）与 `W74`（`W74-D-ROADD001`）实证。
