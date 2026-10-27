# D-COMPOSITOR-WM / W70-D-COMPOSITOR：五平面 `DesktopSession` 工作站合成器、`4px` 平铺优先窗口管理器、`36px` 底部贴边任务栏（`L0`）与代际输入路由

- 状态：`PARTIAL_PASS`（五平面 `Z0–Z4` 工作站宿主合成器、`4px` 间距五模式平铺优先窗口管理器与耦合 `WindowLifecycle <-> SurfaceLifecycle` 状态机、`36px` 底部紧凑任务栏（`L0`）与 `[DUI-LAUNCH-001]` 应用启动器、`L0–L3` 渐进式披露引擎与 `O(visible_rows)` 懒加载归一化 `DesktopProjectionStore`、`[UI-PARITY-001]` 三大铁律动作裁剪器与 `[CTRL-RECOVERY-001]` 幂等命令对账器、`[DUI-INPUT-001]` 代际与五输入源防伪路由及 `Z4` `350ms` 武装门、`[DUI-SESSION-001]` 多用户会话与 `Z4` 专属个人校验印章全部落库通过；原生 OS Wayland/DWM 协议合成器替代按 `ADR-0025` 列为阶段非目标，完整 `Z4` 能力审批/Chooser/剪贴板确认屏、`Ctrl+Space` 全能命令面板、交互式 `Task Space` 拓扑工作台、通知中心与 12 域控制中心由后续 `W71–W73` 挂载至本合成器预留槽位）
- Assurance：`H4`（确定性 Rust 几何/状态机/防伪输入路由单测 + 真实签名 `ApplicationAuthority`/`ArtifactStore`/`IdentityAuthority` 支撑的 `compositor_wm_side.rs` 集成测试 + Windows Named Pipe 四入口字节级回执回归 + `compositor-wm.test.ts` 733 项与 `tokens-and-primitives.test.ts` 698 项共计 1,431 项确定性前端几何、状态机、法定锁、对账器与 `ANTI-AI-01..07` 静态及渲染审计断言）
- 日期：2026-10-27
- Owner：`W70-D-COMPOSITOR`（Task `D-COMPOSITOR-WM`, Attempt 1；Base Snapshot `c09c54b3b62b41c81ef55a793abe6afd0bc2ea41`）
- 子代理波次编排（`[PD-ORCH-001..006]`）：
  - Wave 1（不相交写集并行）：`W70-A-RUST-CORE`（Rust 会话/窗口/合成器/输入路由/IPC 桥与集成测试） `||` `W70-B-TS-STORE`（`L0–L3` 披露模型、动作裁剪器、对账器、归一化投影 Store 与 IPC 类型接线） `||` `W70-C-TS-WM-DOCK-CSS`（五平面样式表、`4px` 几何求解器、`30px` 可信窗口外框、平铺窗口管理器与 `36px` 底部任务栏/启动器）
  - Wave 2（Wave 1 屏障验收后执行）：`W70-D-SHELL-VERIFY`（五平面 `WorkstationCompositor` 壳层控制器、`main.ts` 挂载与 `L3` 深度诊断收纳、`compositor-wm.test.ts` 8 大验证套件）
- 设计依据：
  - [06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §22 `[CONS-CTRL-001]`；§24.2 `[TUI-SEAL-001]`、`[TUI-GUARD-001]`；§24.3 `[RCPT-PARTIAL-001]`；§25.3 `[UI-PARITY-001]`、`[CTRL-PARITY-001]`、`[CTRL-RECOVERY-001]`；§25.4 `[DUI-LAUNCH-001]`、`[DUI-WINDOW-001]`、`[DUI-INPUT-001]`、`[DUI-SESSION-001]`；§30.4 `[ROAD-D-001]`
  - [ADR-0025：阶段 D 混合桌面合成器与可信 UI 分层架构](../../management/adrs/0025-stage-d-hybrid-desktop-compositor-and-trusted-ui-layering.md) Decision 1–Decision 4
  - [阶段 D 完整桌面视觉与交互设计规格（`W68-R2`）](../../superpowers/specs/2026-10-26-stage-d-desktop-visual-and-ux-design.md) §0.1 `ANTI-AI-01..07`、§1.1、§2.1–§2.4、§2A.2–§2A.4、§3.0–§3.5

---

## 1. 交付物清单（`WriteSet(W70-D-COMPOSITOR)` 共 29 个文件）

### 1.1 Rust 宿主合成器、多用户会话与代际防伪输入路由（`desktop/src-tauri`）

| 路径 | 变更类型 | 核心职责 |
|---|---|---|
| `desktop/src-tauri/src/session.rs` | 新增 | `[DUI-SESSION-001]` `DesktopSessionState`（`LOCKED -> ACTIVE <-> SUSPENDED -> CLOSED`）、`WorkspaceState`（`CREATED -> ACTIVE <-> ARCHIVED -> CLOSED`）、`UserProfile`（私有 `security_seal_hint` 仅向 `Z4` 投影）、`SessionScopedCapabilityGrant` 与单调 `session_generation` 推进/跨用户工作区隔离 |
| `desktop/src-tauri/src/window_lifecycle.rs` | 修改 | 保持 `SurfaceLifecycle` 100% 向后兼容；新增 `WindowLifecycle`（`CREATED -> VISIBLE <-> MINIMIZED -> CLOSING -> CLOSED`）与耦合状态机 `CoupledWindowSurface` |
| `desktop/src-tauri/src/surfaces.rs` | 修改 | 保持 `present_surfaces_core` 100% 向后兼容；新增 `resolve_launch_from_authority` 与 `inspect_current_generation_by_package`，严格按 `(ApplicationId, InstallationId)` 校验应用权威状态、当前代际与可呈现表面 |
| `desktop/src-tauri/src/compositor.rs` | 新增 | 五平面常量与几何求解（`ThreePaneWorkstation`、`TwoColumnSplit`、`Quadrants`、`WorkspaceMaximize`、`Floating`），`StageRect` 强制 `height = viewport_h - 36`，`SandboxedSurfaceHost` 标题栏下 `30px` 内容裁剪与 `Z3/Z4` 系统平面遮挡围栏，`WindowAccessibilityNode` a11y 元数据，`ApplicationLauncher` 与 `WorkstationHostShell` |
| `desktop/src-tauri/src/input_router.rs` | 新增 | `[DUI-INPUT-001]` `FocusAndInputRoute`、`Z4_ARMING_WINDOW_MS = 350`、五类 `InputSource`（`Keyboard`/`Pointer`/`Voice`/`DragDrop`/`Automation`）、防伪来源校验（拒绝 `Z2` 冒充 `Z3/Z4`）、过期 `surface_generation` 拦截与 `InputAuditCounters` |
| `desktop/src-tauri/src/dto.rs` | 修改 | 新增 `CompositorSnapshotDto`、`CompositorWindowDto`、`FocusAndInputRouteDto`、`InputRouteResultDto`、`UserProfilePublicDto`（零校验印章泄露）与 `Z4TrustedSealContextDto`（仅 `include_z4_trusted == true` 时输出） |
| `desktop/src-tauri/src/ipc.rs` | 修改 | `AppState` 挂载 `WorkstationHostShell`，新增 `compositor_get_snapshot`、`compositor_session_transition`、`compositor_workspace_action`、`compositor_set_layout`、`compositor_launch_application`、`compositor_window_action`、`compositor_set_z3_overlay`、`compositor_set_z4_modal`、`compositor_route_input` 共 9 个命令；全部持久域变更继续 100% 走 `ADR-0011` `ControlCommand` |
| `desktop/src-tauri/src/lib.rs` | 修改 | 导出 `compositor`、`input_router`、`session` 模块并在 `tauri::generate_handler!` 中注册全部 58 个命令处理器 |
| `desktop/src-tauri/tests/compositor_wm_side.rs` | 新增 | 7 项端到端集成测试，覆盖会话与多用户工作区隔离、五平面几何与遮挡、真实签名 `ApplicationAuthority` 下 `[DUI-LAUNCH-001]` 拒绝 `PackageId`/`ProcessId`、应用升级 `generation 1 -> 2` 过期窗口自动围栏、`Z4` `350ms` 武装门及 Tauri IPC 往返 |

### 1.2 前端五平面样式、渐进披露 Store、平铺窗口管理与底部任务栏（`desktop/src`）

| 路径 | 变更类型 | 核心职责 |
|---|---|---|
| `desktop/src/styles/planes-layout.css` | 新增 | 五平面布局样式（`Z0=0` 实色画布、`Z1=100` 驻留任务空间、`Z2=200..299` `4px` 平铺分割条与 `30px` 可信标题栏、`Z3=400/430/460` `36px` 底部任务栏与实色浮层槽、`Z4=900` 不可遮挡安全屏）及 `prefers-reduced-motion: reduce` 零时长退化 |
| `desktop/src/style.css` | 修改 | 引入 `tokens.css`、`surfaces-and-borders.css`、`planes-layout.css`，清除遗留非标准圆角并保留全部 `L3` 诊断视图类（100% 实色 `--nlos-*` 令牌与 `{0px, 2px, 4px, 6px}` 圆角纪律） |
| `desktop/src/types.ts`、`desktop/src/ipc.ts` | 修改 | 纯增量追加合成器快照/路由 DTO、`getControlCommandStatus` 别名与 9 个 `compositor*` IPC 包装函数 |
| `desktop/src/store/{disclosure-model.ts,action-pruner.ts,command-reconciler.ts,projection-store.ts,index.ts}` | 新增 | `< 16ms` 原地 `data-nlos-disclosure="L0..L3"` 状态切换、`O(visible_rows)`（`1..=64`）游标懒加载围栏、12 域 `[UI-PARITY-001]` 三大铁律 + 5+1 法定不可篡改锁裁剪、`[CTRL-RECOVERY-001]` `UNKNOWN` 阻断盲目重试与 `getControlCommandStatus` 对账恢复、锁屏敏感缓存清理 |
| `desktop/src/wm/{layout-solver.ts,window-frame.ts,window-manager.ts,index.ts}` | 新增 | 五模式确定性像素几何求解器、`30px` 可信 `WindowFrame`（`<iframe sandbox="allow-scripts">` + 严格 CSP + 过期横幅/灰度只读态）与有状态 `TilingWindowManager` |
| `desktop/src/dock/{launcher-menu.ts,bottom-taskbar.ts,index.ts}` | 新增 | `[DUI-LAUNCH-001]` `ApplicationLauncherModel` + 实色启动面板、`36px` 底部三区工作站任务栏（`26px` 命令槽、`2px` 实色底边窗口标签、布局切换器、离线/在线诚实连通徽标、披露级别切换、主题切换、硬件占用指示与等宽时钟） |
| `desktop/src/compositor/{workstation-compositor.ts,index.ts}` | 新增 | 五平面 `WorkstationCompositor` 编排器，`Z4` 专属 `32x20` 个人校验印章渲染、`350ms` 武装门、五输入源切断与防伪审计、`F6`/`1..4`/`Ctrl+Shift+E`/`Space`/`Ctrl+Alt+L`/`Escape` 键盘导航环及多用户会话切换隔离 |
| `desktop/src/main.ts` | 修改 | 默认以 `L1` + `graphite-workstation` + `three-pane-workstation` 挂载五平面工作站壳层，并将既有 Stage B/C 诊断标签页完整收纳至 `L3` 深度诊断面板（保留 `.sidebar` 与 `button.tab[data-view-id]` 无损兼容 `views/task-space`） |
| `desktop/src/compositor/__tests__/compositor-wm.test.ts` | 新增 | 8 大套件共 733 项确定性断言，覆盖五平面几何与分辨率钳制、耦合生命周期、`[DUI-LAUNCH-001]`、`L0–L3` 懒加载围栏、`[UI-PARITY-001]` 法定锁、`[CTRL-RECOVERY-001]` 对账器、`Z4` 武装门与全量 `ANTI-AI-01..07` 静态及渲染审计 |

---

## 2. 规范不变量与验收准则核验矩阵

| 规范 ID | 核心不变量 | 实现锚点 | 实测验证锚点 |
|---|---|---|---|
| `ADR-0025` Decision 1 / `[ROAD-D-001]` | 五平面严格 Z 序（`0 < 100 < 200..299 < 400/430/460 < 900`）；`StageRect.height == viewportHeight - 36`，`1024x768` 至 `3840x2160` 任意模式下零窗口覆盖 `36px` 底部任务栏 | `compositor.rs`、`wm/layout-solver.ts`、`styles/planes-layout.css` | `compositor_wm_side.rs::coupled_window_surface_lifecycle_five_plane_z_order_stage_clamping_and_occlusion`；`compositor-wm.test.ts` Suite 1 |
| `[DUI-WINDOW-001]` | 耦合 `WindowLifecycle <-> SurfaceLifecycle`；`30px` 系统绘制标题栏，`<iframe sandbox="allow-scripts">` 严格裁剪于标题栏下方；`Z3/Z4` 交叠时启用系统平面遮挡围栏；代际过期窗口立挂横幅、灰度化并拒绝输入 | `window_lifecycle.rs`、`compositor.rs`、`wm/window-frame.ts`、`wm/window-manager.ts` | `compositor_wm_side.rs::focus_and_input_route_fences_stale_generations_z4_modal_arming_and_spoofing`；`compositor-wm.test.ts` Suite 2 & Suite 3 |
| `[DUI-LAUNCH-001]` | 严格按 `(ApplicationId, InstallationId)` 激活应用；显式拒绝 `PackageId` 与瞬态 `ProcessId` 启动（含偷带混淆 ID）；存在活跃任务时锁定卸载并披露原因 | `surfaces.rs`、`compositor.rs::ApplicationLauncher`、`dock/launcher-menu.ts` | `compositor_wm_side.rs::application_launcher_enforces_dui_launch_001_against_real_application_authority`；`compositor-wm.test.ts` Suite 3 |
| `[DUI-INPUT-001]` | `Z4` 激活或会话非 `ACTIVE` 时切断键盘/指针/语音/拖拽/自动化五类输入向 `Z2` 投递；拒绝 `Z2` 伪造 `Z3/Z4` 系统事件；`Z4` 强制 `350ms` 武装防抢焦窗（`< 350ms` 拒绝确认、`0ms` `Escape` 立即取消、`>= 350ms` 允许确认） | `input_router.rs`、`compositor/workstation-compositor.ts` | `compositor_wm_side.rs::focus_and_input_route_fences_stale_generations_z4_modal_arming_and_spoofing`；`compositor-wm.test.ts` Suite 7 |
| `[DUI-SESSION-001]` | `DesktopSession` 四态生命周期；`lock`/`suspend`/`switch_user`/`close` 单调递增 `session_generation`、吊销会话瞬态授权并清空敏感缓存；跨用户工作区与 `32x20` 个人校验印章严格隔离（印章仅在 `Z4` 渲染，`Z0..Z3` 零暴露） | `session.rs`、`dto.rs`、`store/projection-store.ts`、`compositor/workstation-compositor.ts` | `compositor_wm_side.rs::desktop_session_lifecycle_multi_user_workspace_isolation_and_seal_privacy`；`compositor-wm.test.ts` Suite 7 |
| `ADR-0025` Decision 2 & 4 | `L0 -> L1 -> L2 -> L3` 原地切换 `< 16ms` 零上下文丢失；`L0/L1` 发起 `0` 次 `L2/L3` IPC 查询；分页游标上限 `<= 64` | `store/disclosure-model.ts`、`store/projection-store.ts` | `compositor-wm.test.ts` Suite 4 |
| `[UI-PARITY-001]` | 无读不控（32-hex 目标 + CAS 代际令牌）、无孤立展示（零黑话只读原因）、剥离自授 `SYSTEM_CRITICAL`、5+1 法定不可篡改锁在渲染前硬裁剪 | `store/action-pruner.ts` | `compositor-wm.test.ts` Suite 5 |
| `[CTRL-RECOVERY-001]` / `[RCPT-PARTIAL-001]` | 传输超时 `UNKNOWN` 进入 `UNKNOWN_NEEDS_RECONCILE`，硬阻断盲目重试（`0` 次重复写派发），强制经 `getControlCommandStatus` 对账；批量操作逐目标汇总成功/失败/未知 | `store/command-reconciler.ts` | `compositor-wm.test.ts` Suite 6 |
| `ANTI-AI-01..07` | 100% 实色令牌、零 `backdrop-filter`/霓虹发光/渐变壁纸、圆角严格属 `{0px, 2px, 4px, 6px}`、零 Emoji 象形字符、`L0/L1` 可见文本零内部架构术语（`Z1`、`TypedIntent`、`0x1F`、`Quota`、`ADR-0011`、`TaskAttempt`、`ExecutionFiber`、`Cell`、`receipt_hex` 等） | `styles/planes-layout.css`、`style.css`、`dock/*`、`wm/*`、`compositor/*`、`main.ts` | `tokens-and-primitives.test.ts` Suite 1–10（698 断言）；`compositor-wm.test.ts` Suite 8（中英双语渲染 + 全目录静态扫描） |

---

## 3. 验证命令与实测结果（Windows x86_64）

```text
$ npx tsx desktop/src/ui/__tests__/tokens-and-primitives.test.ts
  -> EXIT CODE: 0 (ALL 10 SUITES PASSED — 698 deterministic assertions verified)

$ npx tsx desktop/src/compositor/__tests__/compositor-wm.test.ts
  -> EXIT CODE: 0 (All 8 D-COMPOSITOR-WM suites passed — 733 deterministic assertions verified)

$ cd desktop && npx tsc --noEmit
  -> EXIT CODE: 0 (0 TypeScript errors)

$ cd desktop && npx vite build
  -> EXIT CODE: 0 (36 modules transformed; dist/index.html + CSS + JS built in 451ms)

$ cargo fmt --manifest-path desktop/src-tauri/Cargo.toml --all -- --check
  -> EXIT CODE: 0

$ cargo clippy --manifest-path desktop/src-tauri/Cargo.toml --features dev-fixture --all-targets -- -D warnings
  -> EXIT CODE: 0 (0 warnings)

$ cargo test --manifest-path desktop/src-tauri/Cargo.toml --features dev-fixture --no-fail-fast
  -> EXIT CODE: 0 (llmos_desktop_lib unit: 14 passed; compositor_wm_side: 7 passed; ipc_enum_symmetry_side: 5 passed; surface_presentation_side: 2 passed; window_lifecycle_side: 3 passed; windows_authenticated_pipe_side: 1 passed; total 32 passed, 0 failed)
```

---

## 4. 诚实范围边界说明

1. **宿主合成器架构边界（`ADR-0025` Decision 1）**：本工作包实现应用内五平面工作站壳层与沙箱化表面宿主适配器（`SurfaceHostAdapter` + `<iframe sandbox="allow-scripts">`），不替代 Linux Wayland/X11 或 Windows DWM/macOS Quartz 底层系统显示合成器；`SurfaceHostAdapter` 已冻结像素裁剪、遮挡围栏与代际契约，供后续嵌入式 WebView/原生子表面无缝接入。
2. **下游工作包挂载槽位边界**：`Z3` 已就绪 `z3-command-palette`（`z-index: 460`）与 `z3-system-flyouts`（`z-index: 430`）槽位，`Z1` 已就绪可停靠/切换的 `Task Space` 概览槽与 `L2` `InspectorDrawer` 联动，`Z4` 已就绪可信校验印章与 `350ms` 武装门；完整 `Ctrl+Space` 双模全能命令面板、交互式 `Task Space` 拓扑工作台及完整 `Z4` 能力审批/Chooser/剪贴板确认视图归 `W71`，`0x1F` Spotlight 搜索与通知中心归 `W72`，12 域系统控制中心归 `W73`。
