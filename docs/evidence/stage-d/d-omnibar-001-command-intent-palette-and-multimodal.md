# D-OMNIBAR-MULTIMODAL / W71-D-OMNIBAR：按需 `Z3` 实心命令与意图面板、渐进式自然语言意图编译器、`[NL-AMBIG-001]` 强制澄清硬门与多模态硬件权限状态机

- 状态：`PARTIAL_PASS`（按需唤起的 `Z3` 实心命令与意图面板 `z-index: 460` / `4px` 圆角 / `0px` 屏幕顶部常驻占用、`Ctrl+Space` 与 `26px` 底栏触发槽唤起及 `Escape`/外部点击保留草稿收起、`nlos.intent.v1` + `nlos.action-proposal.v1` 渐进式自然语言编译与中英双语 `L1/L2` 零行话预览 + `L3` 完整诊断投影、`[NL-BIND-001]` 拒绝可变 `latest` 标签/`0x1F` 非权威搜索命中/`PackageId` 与 `ProcessId` 混充及过期代际、`[NL-AMBIG-001]` 候选解释对比表与选定前物理硬门拦截、`InputSource::Voice` 经 `FocusAndInputRoute` 路由与锁屏/`Z4` 即时物理切断、`DevicePermissionState` `L0` 底栏硬件指示投影以及 `ChooserGrant`/`ArtifactId + ContentDigest` `4px` 附件芯片与污染标记单调传播全部落库验证通过；宿主入口 `desktop/src/main.ts`、`desktop/src/ipc.ts` 与 `desktop/src-tauri/src/lib.rs` `tauri::generate_handler!` 的统一共享文件挂载按 `W71` 并行屏障协议由收口车道 `W71-INT` 完成，底层操作系统原生语音 ASR/麦克风硬件驱动音频采样流接入不在本桌面壳层工作包宣称范围内）
- Assurance：`H4`（`omnibar_multimodal_side.rs` 7 项确定性 Rust 集成测试 + `omnibar-multimodal.test.ts` 9 大验证套件 525 项确定性前端状态机、双语零行话、`WorkstationCompositor` 联动及 `ANTI-AI-01..07` 静态与渲染审计断言 + `compositor-wm.test.ts` 733 项全仓回归断言通过）
- 日期：2026-10-27
- Owner：`W71-D-OMNIBAR`（Work Package `D-OMNIBAR-MULTIMODAL`, Attempt 1；Base Snapshot `e9087fedae09d950a181fd888c9a3e9f416035fa`）
- 设计依据：
  - [06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §24.1 `[NL-COMPILE-001]`、`[NL-PROPOSE-001]`、`[NL-AMBIG-001]`、`[NL-BIND-001]`；§24.2 `[TUI-CHOOSER-001]`、`[TUI-EXT-001]`；§24.3 `[RCPT-UNDO-001]`；§25.3 `[CTRL-PARITY-001]`；§25.4 `[DUI-LAUNCH-001]`、`[DUI-INPUT-001]`、`[DUI-CHOOSER-001]`、`[DUI-CLIP-001]`、`[DUI-SESSION-001]`；§28.4 `[ROAD-D-001]`
  - [ADR-0025：阶段 D 混合自然语言桌面合成层、L0–L3 渐进式披露、12 域显控同构与可信 UI 防伪边界](../../management/adrs/0025-stage-d-hybrid-desktop-compositor-and-trusted-ui-layering.md)
  - [阶段 D 完整桌面视觉与交互设计规格（`W68-R2`）](../../superpowers/specs/2026-10-26-stage-d-desktop-visual-and-ux-design.md) §0.1 `ANTI-AI-01..07`、§2.4 `Z3` 命令与意图面板、§2A `L0–L3` 渐进式披露、§3.3 命令与意图面板及多模态 I/O
  - [阶段 D 权威进度单](../../management/stage-d-progress.md) §D.3.1 #4 `D-OMNIBAR-MULTIMODAL`、§D.5.2 `W71-D-OMNIBAR`
  - 前置基线证据：[d-compositor-001-five-plane-tiling-wm-and-session.md](./d-compositor-001-five-plane-tiling-wm-and-session.md)

---

## 1. 交付物清单（`WriteSet(W71-D-OMNIBAR)` 共 11 个文件）

### 1.1 Rust 后端意图编译、强制澄清门、多模态附件校验与语音路由模块（`desktop/src-tauri`）

| 路径 | 变更类型 | 核心职责 |
|---|---|---|
| [desktop/src-tauri/src/omnibar.rs](../../../desktop/src-tauri/src/omnibar.rs) | 替换预留桩 | 实现 `OmnibarEngine`、`TypedIntentDto`（`nlos.intent.v1@1.0`）、`ActionProposalDto`（`nlos.action-proposal.v1`）、`[NL-BIND-001]` 依赖句柄核验器（拒绝 `latest`/`*`/`head`、`0x1F` 非权威搜索命中、`PackageId`/`ProcessId` 混淆与过期代际）、`[NL-AMBIG-001]` `CandidateInterpretationDto` 对比生成与 `OmnibarError::ClarificationRequired` 硬门、`validate_attachment_chip`（`ChooserGrant` 路径剥离与 `ArtifactId + ContentDigest` 绑定）、经 `WorkstationHostShell::dispatch_input` 裁决的 `InputSource::Voice` 流状态机与锁屏/`Z4` 即时切断、`DevicePermissionStateDto` 底栏托盘投影，以及 11 个 `#[tauri::command]` / `*_core` 命令入口供 `W71-INT` 直接注册 |
| [desktop/src-tauri/tests/omnibar_multimodal_side.rs](../../../desktop/src-tauri/tests/omnibar_multimodal_side.rs) | 新增 | 7 项端到端确定性集成测试，覆盖 `0px` 顶部常驻占用、`Ctrl+Space` 与 `26px` 任务栏槽唤起、`Escape`/外部点击草稿保留、双语 `L1/L2` 零行话与 `L3` `parse_nl_command` 同构预览、`[NL-BIND-001]` 四类非法依赖拦截、`[NL-AMBIG-001]` 选定前硬阻断与选定后 `clarification_digest_hex` 解锁、`InputSource::Voice` 在 `Z4`/锁屏下的即时切断、`ChooserGrant` 宿主路径防泄露与污染传播，以及 `AppState` `omnibar_*_core` 全链路往返 |

### 1.2 前端多模态流与按需 `Z3` 命令/意图面板模块（`desktop/src`）

| 路径 | 变更类型 | 核心职责 |
|---|---|---|
| [desktop/src/multimodal/voice-stream.ts](../../../desktop/src/multimodal/voice-stream.ts) | 新增 | 按住说话与切换双模 `VoiceInputController`（`Idle -> RequestingMicPermission -> Capturing -> Transcribing -> Committed \| Cancelled \| Denied`），经可选 `routeVoiceInput` 对接 `FocusAndInputRoute`，在 `Z4` 激活或会话非 `ACTIVE`（`LOCKED`/`SUSPENDED`/`CLOSED`）时立即物理切断麦克风/摄像头/录屏，并通过 `projectDevicePermissionToTaskbar` 驱动 `36px` `L0` 底栏硬件指示灯 |
| [desktop/src/multimodal/attachments.ts](../../../desktop/src/multimodal/attachments.ts) | 新增 | `4px` 圆角实心附件芯片（`Screenshot` / `ChooserGrant` 本地文件 / `ArtifactId + ContentDigest` 引用），拦截无 `ChooserGrant` 的宿主原始路径（`RAW_HOST_PATH_FORBIDDEN`），默认剥离目录仅保留安全文件名且不暴露 `authorizedHostPath`，单调合并 `TaintFlag` |
| [desktop/src/multimodal/index.ts](../../../desktop/src/multimodal/index.ts) | 新增 | 多模态输入与硬件权限托盘投影统一导出桶 |
| [desktop/src/views/omnibar/omnibar.css](../../../desktop/src/views/omnibar/omnibar.css) | 新增 | 100% 实心不透明 `Graphite / Paper` 中性工作站令牌样式（`z-index: var(--nlos-z3-palette)` = `460`，`border-radius: var(--nlos-radius-ctrl)` = `4px`，零模糊滤镜，零霓虹发光，支持 `prefers-reduced-motion: reduce` `0ms` 退化） |
| [desktop/src/views/omnibar/intent-compiler.ts](../../../desktop/src/views/omnibar/intent-compiler.ts) | 新增 | 确定性前端渐进式意图编译器 `compileIntentAndProposal`、`[NL-BIND-001]` 依赖核验器 `IntentBindingError`、四类歧义检测器与 `CandidateInterpretationRow` 生成器、`selectAmbiguityCandidate` 及 `finalizeProposalForSubmission`（`ClarificationRequiredError` 硬门） |
| [desktop/src/views/omnibar/palette-view.ts](../../../desktop/src/views/omnibar/palette-view.ts) | 新增 | `OmnibarPaletteController`、`renderOmnibarPaletteHtml`、`createOmnibarPaletteElement` 及一键合成器接线器 `bindOmnibarToCompositor`（自动挂载至 `WorkstationCompositor` 的 `z3-command-palette` 槽位、同步底栏硬件采集灯并联动 `Z4`/会话锁屏切断） |
| [desktop/src/views/omnibar/index.ts](../../../desktop/src/views/omnibar/index.ts) | 新增 | `views/omnibar` 公共 API 导出桶 |
| [desktop/src/views/omnibar/__tests__/omnibar-multimodal.test.ts](../../../desktop/src/views/omnibar/__tests__/omnibar-multimodal.test.ts) | 新增 | 9 大确定性验证套件共 525 项断言，覆盖几何与草稿保留、中英双语 `L1/L2` 零行话与 `BulletMicroBar`、`L3` 诊断保真、`[NL-AMBIG-001]` 澄清硬门、`[NL-BIND-001]` 依赖拒绝、语音切断、附件防泄密、合成器联动及全目录 `ANTI-AI-01..07` 静态扫描 |

---

## 2. 规范不变量与验收准则核验矩阵

| 规范 ID | 核心不变量 | 实现锚点 | 实测验证锚点 |
|---|---|---|---|
| `ADR-0025` / Spec §2.4 & §3.3 | 默认隐藏，`0px` 顶部常驻占用；`Ctrl+Space`（或 `Cmd+Space`）与底栏 `26px` 槽按需在 `Z3`（`z-index: 460`、`4px` 实心圆角）唤起；`Escape` 与外部点击收起保留草稿与附件，仅显式清空时移除 | `omnibar.rs::OmnibarEngine`、`palette-view.ts`、`omnibar.css` | `omnibar_multimodal_side.rs::omnibar_default_zero_footprint_ctrl_space_taskbar_and_draft_preservation`；`omnibar-multimodal.test.ts` Suite 1 & Suite 8 |
| `[NL-COMPILE-001]` / `[NL-PROPOSE-001]` / `ANTI-AI-07` | 编译为版本化 `nlos.intent.v1@1.0` + `nlos.action-proposal.v1`；`L1/L2` 在 `zh-CN` 与 `en-US` 下严格零架构行话，展示平实目标应用、作用范围、额度上限与可逆徽章（`可无损撤销 / 可补偿回滚 / 不可逆`）；`L3` 展开完整 Schema、摘要、`ControlScope`、`IdempotencyKey`、句柄三元组与 `ControlCommand` 预览 | `omnibar.rs::compile_preview`、`intent-compiler.ts`、`palette-view.ts` | `omnibar_multimodal_side.rs::progressive_compilation_l1_l2_zero_jargon_and_l3_typed_intent_fidelity`；`omnibar-multimodal.test.ts` Suite 2 & Suite 3 |
| `[NL-BIND-001]` / `U-7/U-8` / `[DUI-LAUNCH-001]` | 严禁可变符号标签 `latest`/`*`/`head` 或 `0x1F` 非权威搜索命中直接充当执行授权依赖；拒绝 `PackageId` 与瞬态 `ProcessId` 混充 `ApplicationId`；拒绝过期 `generation` | `omnibar.rs::resolve_explicit_dependency`、`intent-compiler.ts` | `omnibar_multimodal_side.rs::nl_bind_001_rejects_latest_tag_0x1f_search_hit_identity_confusion_and_stale_gen`；`omnibar-multimodal.test.ts` Suite 5 |
| `[NL-AMBIG-001]` | 对高危不可逆副作用、跨应用数据流转、多匹配目标应用与预算提额生成实心 `CandidateInterpretationRow` 对比表；未显式点选前以 `ClarificationRequired` 物理阻断提交与 `Z4` 移交；点选后将 `selected_candidate_id + clarification_digest_hex` 绑定进提案 | `omnibar.rs::{select_candidate,submit_proposal}`、`intent-compiler.ts::{selectAmbiguityCandidate,finalizeProposalForSubmission}` | `omnibar_multimodal_side.rs::nl_ambig_001_hard_blocks_submission_before_selection_and_binds_clarification_after`；`omnibar-multimodal.test.ts` Suite 4 |
| `[DUI-INPUT-001]` / `[DUI-SESSION-001]` / `[TUI-EXT-001]` | 语音采集以 `InputSource::Voice` 经 `FocusAndInputRoute` 裁决，拒绝不可信表面伪造；会话进入 `LOCKED`/`SUSPENDED`/`CLOSED` 或 `Z4` 安全屏激活时立即物理切断活跃麦克风/录屏并归零 `36px` `L0` 底栏硬件指示灯 | `omnibar.rs::{start_voice_capture,sync_with_host_shell}`、`multimodal/voice-stream.ts` | `omnibar_multimodal_side.rs::voice_stream_routes_via_focus_router_projects_tray_and_severs_on_z4_or_lock`；`omnibar-multimodal.test.ts` Suite 6 & Suite 8 |
| `[DUI-CHOOSER-001]` / `[TUI-CHOOSER-001]` | 本地文件芯片强制要求合法 `ChooserGrant` + `ScopedCapability` 句柄，拒绝无授权原始宿主路径（`RAW_HOST_PATH_FORBIDDEN`）且默认剥离目录前缀不泄露宿主路径；`Artifact` 芯片绑定 `ArtifactId + ContentDigest`；附件 `TaintFlag` 单调传播至 `ActionProposal` | `omnibar.rs::validate_attachment_chip`、`multimodal/attachments.ts` | `omnibar_multimodal_side.rs::attachment_chips_enforce_chooser_grant_strip_host_paths_and_propagate_taint`；`omnibar-multimodal.test.ts` Suite 7 |
| `ANTI-AI-01..06` | 100% 实心表面、零 `backdrop-filter`、零 `#030710`/`#7FC8FF` 霓虹色、零径向发光与 `max-w-7xl`、零 Emoji 象形字符、全部圆角严格落在 `{0px, 2px, 4px, 6px}` | `views/omnibar/omnibar.css`、`views/omnibar/*.ts`、`multimodal/*.ts` | `omnibar-multimodal.test.ts` Suite 9；`compositor-wm.test.ts` Suite 8 全仓扫描 |

---

## 3. 验证命令与实测结果（Windows x86_64）

```text
$ npx tsx desktop/src/views/omnibar/__tests__/omnibar-multimodal.test.ts
  -> EXIT CODE: 0 (All 9 Stage D Omnibar & Multimodal suites passed — 525 assertions)

$ npx tsx desktop/src/compositor/__tests__/compositor-wm.test.ts
  -> EXIT CODE: 0 (All 8 D-COMPOSITOR-WM suites passed — 733 assertions)

$ cd desktop && npx tsc --noEmit
  -> EXIT CODE: 0 (0 TypeScript errors)

$ $env:CARGO_TARGET_DIR='d:\llmos\target\w71-omnibar'; cargo fmt --manifest-path desktop/src-tauri/Cargo.toml --all -- --check
  -> EXIT CODE: 0

$ $env:CARGO_TARGET_DIR='d:\llmos\target\w71-omnibar'; cargo clippy --manifest-path desktop/src-tauri/Cargo.toml --features dev-fixture --all-targets -- -D warnings
  -> EXIT CODE: 0 (0 warnings)

$ $env:CARGO_TARGET_DIR='d:\llmos\target\w71-omnibar'; cargo test --manifest-path desktop/src-tauri/Cargo.toml --features dev-fixture --test omnibar_multimodal_side --no-fail-fast
  -> EXIT CODE: 0 (7 passed; 0 failed; 0 ignored)
```

---

## 4. 诚实范围边界说明（供 `W71-INT` 收口对齐）

1. **并行波次共享文件收口边界（`W71-INT`）**：遵照 `W71` 并行写集隔离规则，本车道不触碰共享入口 `desktop/src-tauri/src/lib.rs`、`desktop/src/main.ts`、`desktop/src/ipc.ts`、`desktop/src/style.css`；`omnibar.rs` 已暴露 11 个基于 `tauri::State<'_, AppState>` 的 `#[tauri::command]` 函数（无需额外 `.manage(...)`），前端 `views/omnibar/index.ts` 已暴露 `bindOmnibarToCompositor(compositor, controller)`，供 `W71-INT` 在波次屏障后一键挂载。
2. **多模态采集边界**：本工作包实现了确定性的 `InputSource::Voice` 授权路由状态机、锁屏/`Z4` 即时物理切断围栏、`DevicePermissionState` 底栏硬件指示投影以及 `ChooserGrant`/`Artifact` 芯片核验契约；底层原生音频 PCM 硬件采样与本地离线 ASR 声学模型推理不属于本混合桌面壳层模块职责。
