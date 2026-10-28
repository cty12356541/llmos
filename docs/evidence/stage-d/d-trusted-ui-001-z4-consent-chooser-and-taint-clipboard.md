# D-TRUSTED-UI / W71-D-TRUSTED-UI：不可遮挡 `Z4` 实心安全确认屏、`ChooserGrant` 受限能力经纪器（零宿主裸路径泄露）与污点感知跨应用剪贴板/拖放守卫

- 状态：`PARTIAL_PASS`（不可遮挡 `Z4` `z-index: 900` 实心系统安全确认屏、零模糊遮罩 `rgba(10, 11, 13, 0.78)` + `6px` 实心对话框 + `2px` 顶部钢蓝安全条 + `32×20` 凹陷 Gerono 硬件核验印章与私密 `security_seal_hint` 严格隔离、4 行结构化规格表、`IRREVERSIBLE` 深红 `#DC2626` 法定不可篡改锁与高对比度显式确认、`350ms` 呈现与焦点重获防点击劫持武装门 + `0ms` `Escape` 即时取消、系统 `Open / Save / Share` `ChooserGrantBroker` 零宿主裸路径/零未选中兄弟条目泄露、以及 `ClipboardAndDragBroker` 一次性用户手势门禁 + 过期拦截 + 高敏/外部污点跨应用自动升级 `Z4` 全部实现并通过前后端确定性正反例验证；`desktop/src-tauri/src/lib.rs` `generate_handler![]`、`desktop/src/ipc.ts` 与 `desktop/src/main.ts` 统一壳层挂载按波次屏障由下游 `W71-INT` 合流完成）
- Assurance：`H4`（Rust FIPS 180-4 SHA-256 NIST 向量与叶子路径净化单测 + `trusted_ui_scrim_chooser_clip_side.rs` 5 大端到端与负例旁路集成套件 + 前端 `trusted-ui-security.test.ts` 7 大套件 333 项确定性防伪/法定锁/零裸路径/污点剪贴板/`L1–L3` 零黑话与 `ANTI-AI-01..07` 静态及渲染审计断言 + 全仓 `compositor-wm.test.ts` 733 项兼容性回归）
- 日期：2026-10-27
- Owner：`W71-D-TRUSTED-UI`（Task `D-TRUSTED-UI`, Attempt 1；Base Snapshot `e9087fedae09d950a181fd888c9a3e9f416035fa`）
- 设计依据：
  - [06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §24.1–§24.3 `[TUI-CONSENT-001]`、`[TUI-SEAL-001]`、`[TUI-GUARD-001]`、`[TUI-CHOOSER-001]`、`[RCPT-UNDO-001]`、`[RCPT-PARTIAL-001]`；§25.2 `[CTRL-HIER-001]`；§25.4 `[DUI-CHOOSER-001]`、`[DUI-CLIP-001]`、`[DUI-SESSION-001]`
  - [ADR-0025：阶段 D 混合桌面合成器与可信 UI 分层架构](../../management/adrs/0025-stage-d-hybrid-desktop-compositor-and-trusted-ui-layering.md) Decision 1、Decision 3、Decision 4
  - [阶段 D 完整桌面视觉与交互设计规格（`W68-R2`）](../../superpowers/specs/2026-10-26-stage-d-desktop-visual-and-ux-design.md) §0.1 `ANTI-AI-01..07`、§1.3、§2A.1、§2A.3、§3.5
  - [阶段 D 推进基线](../../management/stage-d-progress.md) §D.3 `W71-D-TRUSTED-UI`

---

## 1. 交付物清单（独占写集 `WriteSet(W71-D-TRUSTED-UI)` 共 7 个代码/测试文件 + 1 份证据报告）

| 路径 | 变更类型 | 核心职责 |
|---|---|---|
| [trusted_ui.rs](../../../desktop/src-tauri/src/trusted_ui.rs) | 修改（由预留桩实现完整模块） | 自包含零外部依赖 FIPS 180-4 SHA-256 与域分离摘要；`TrustedUiBroker`（`Z4 = 900` 实心安全屏、4 行结构化规格表、`security_seal_hint` 严格隔离、`IRREVERSIBLE` 法定不可撤销锁、`350ms` 呈现与焦点重获防劫持武装门、`0ms` `Escape` 即时取消、会话代际围栏）；`ChooserGrantBroker`（`Open / Save / Share` 模式上限、TCB 私有 `HostVaultEntry`、零宿主裸路径 `ChooserGrantForAppDto`、过期与吊销围栏）；`ClipboardAndDragBroker`（一次性非自动化 `UserGestureToken`、TTL 过期门禁、`SENSITIVE / CONFIDENTIAL / CREDENTIALS / UNTRUSTED_EXTERNAL` 跨应用自动升级 `Z4`）；`TrustedUiState` 及 12 个 `#[tauri::command]` + `_core` 处理器 |
| [trusted_ui_scrim_chooser_clip_side.rs](../../../desktop/src-tauri/tests/trusted_ui_scrim_chooser_clip_side.rs) | 新增 | 5 大端到端与对抗旁路测试套件：覆盖 `Z2` 伪造/遮挡 `Z4` 拦截、`security_seal_hint` 零下泄、`IRREVERSIBLE` 法定锁与多目标局部失败披露、`350ms` 呈现与焦点重获武装门、`0ms` `Escape`、会话锁屏/切换用户吊销、`ChooserGrant` 零裸路径/零未选中兄弟泄露、污点剪贴板/拖放自动升级 `Z4` 及 `_core` 往返 |
| [z4-security-dialog.ts](../../../desktop/src/trusted-ui/z4-security-dialog.ts) | 新增 | 前端 `Z4SecurityDialogController` 与实心 `Z4` DOM/HTML 渲染器（复用 `createVerificationSeal` 挂载期单 RAF / 关闭期 `0%` 空闲占用、`createBlastRadiusDiffElement`、`createStatutoryLockBadgeElement`），支持 `zh-CN`/`en-US` 与 `L0–L3` 零黑话分层披露 |
| [chooser-grant-modal.ts](../../../desktop/src/trusted-ui/chooser-grant-modal.ts) | 新增 | 前端 `ChooserGrantModalBroker` 与 `Z4` 系统 `Open / Save / Share` 选择器渲染器，严格隔离 TCB 私有宿主路径与面向应用的 `ChooserGrantForApp` |
| [clipboard-drag-guard.ts](../../../desktop/src/trusted-ui/clipboard-drag-guard.ts) | 新增 | 前端 `ClipboardAndDragGuard`，执行一次性人类手势消费、过期拦截，并在跨应用高敏/外部污点粘贴或拖放时联动 `Z4SecurityDialogController` 阻断字节直至获批 |
| [index.ts](../../../desktop/src/trusted-ui/index.ts) | 新增 | 统一导出面与 `createTrustedUiSuite` 协调器，供 `W71-INT` 在 `main.ts` 与 `WorkstationCompositor` 中一键挂载 |
| [trusted-ui-security.test.ts](../../../desktop/src/trusted-ui/__tests__/trusted-ui-security.test.ts) | 新增 | 7 大前端安全、负例旁路、`zh-CN`/`en-US` `L1/L2` 零黑话与 `ANTI-AI-01..07` 静态扫描验证套件（333 项确定性断言） |

---

## 2. 规范不变量与对抗旁路核验矩阵

| 规范 ID | 核心不变量与负例旁路要求 | 实现锚点 | 实测验证锚点 |
|---|---|---|---|
| `[TUI-CONSENT-001]` / `ADR-0025` Decision 1 & 3 | `Z4` 固定 `z-index: 900`，实心遮罩 `rgba(10, 11, 13, 0.78)`（零模糊滤镜）、`6px` 实心对话框、`2px` 顶部钢蓝安全条；4 行结构化 KV 规格表（请求应用 + 动作范围 + 资源费用上限 + 可逆性与风险）；不可信 `Z2` 伪造 `Z4` 或声明 `z-index > 299` 一律类型化拒绝并记入审计计数 | `trusted_ui.rs::TrustedUiBroker`、`z4-security-dialog.ts` | `trusted_ui_scrim_chooser_clip_side.rs` Test 1；`trusted-ui-security.test.ts` Suite 1 |
| `[TUI-SEAL-001]` / `[DUI-SESSION-001]` | `32×20` 凹陷 Gerono 硬件核验印章（关闭时 `0` 个活跃 RAF 循环）配对当前活动用户私有 `security_seal_hint`；`Z0..Z3` 公共姿态 DTO 与合成器快照中 `0` 处印章泄露；不可信 `Z2` 读取印章返回 `UNTRUSTED_SEAL_READ_DENIED`；锁屏/挂起/切换用户立即吊销挂起 `Z4` 挑战 | `trusted_ui.rs::project_trusted_challenge`、`trusted_ui.rs::project_lower_plane_posture`、`z4-security-dialog.ts` | `trusted_ui_scrim_chooser_clip_side.rs` Test 1 & Test 3；`trusted-ui-security.test.ts` Suite 1 & Suite 3 |
| `[TUI-GUARD-001]` | 初始呈现与每次焦点重获（`notify_focus_regained`）后 `350ms` 内拒绝一切确认提交（`CLICKJACKING_ARMING_ACTIVE`）；拒绝不可信表面与 `Automation` 通道伪造确认（`UNTRUSTED_INPUT_ORIGIN`）；`Escape` 取消在 `0ms` 立即畅通生效 | `trusted_ui.rs::decide_consent_challenge`、`trusted_ui.rs::notify_focus_regained`、`z4-security-dialog.ts` | `trusted_ui_scrim_chooser_clip_side.rs` Test 3；`trusted-ui-security.test.ts` Suite 3 |
| `[RCPT-UNDO-001]` / `[RCPT-PARTIAL-001]` / `[CTRL-HIER-001]` | `IRREVERSIBLE` 动作渲染 `#DC2626` 深红警示、`IRREVERSIBLE_NO_UNDO` 法定锁徽标、要求高对比度显式勾选确认，且渲染 **0 个伪撤销按钮**、拒绝回执 `Undo` 调用（`IRREVERSIBLE_STATUTORY_LOCK_VIOLATION`）；多目标动作强制披露局部失败风险；`purged` 爆炸半径行自动激活数据丢失回执提示 | `trusted_ui.rs::ReversibilityClass`、`trusted_ui.rs::request_undo_for_receipt`、`z4-security-dialog.ts` | `trusted_ui_scrim_chooser_clip_side.rs` Test 2；`trusted-ui-security.test.ts` Suite 2 |
| `[TUI-CHOOSER-001]` / `[DUI-CHOOSER-001]` | 系统独占 `Open / Save / Share` 选择器；向 `Application` 返回的 `ChooserGrantForApp` 仅含 `(grant_id, mode, READ_ONLY | READ_WRITE_SINGLE | EXPORT_SAVE, scoped_handles, session_generation, expires_at_mono_ms, grant_digest_hex)`，序列化 JSON 中 **0 处裸宿主路径**（Windows `C:\Users\...` / Unix `/home/...`）与 **0 处未选中兄弟文件**；跨应用盗用、越权写、路径遍历、过期、显式吊销与锁屏代际推进全部确定性拦截 | `trusted_ui.rs::ChooserGrantBroker`、`chooser-grant-modal.ts` | `trusted_ui_scrim_chooser_clip_side.rs` Test 4；`trusted-ui-security.test.ts` Suite 4 |
| `[DUI-CLIP-001]` | 每条 `ClipboardItem` 与 `DragDropTransfer` 强绑定 `(transfer_id, content_type, payload_digest, source_application_id, taint_flags, created_at_mono_ms, expires_at_mono_ms, user_gesture_token)`；无手势后台静默读取（`MISSING_USER_GESTURE`）、自动化伪造手势（`AUTOMATION_GESTURE_FORBIDDEN`）、重放手势与过期读取（`TRANSFER_EXPIRED`）一律拒绝；跨应用 `SENSITIVE / CONFIDENTIAL / CREDENTIALS / UNTRUSTED_EXTERNAL` 自动升级拉起 `Z4`，未获批或 `Escape` 取消时释放 **0 字节** | `trusted_ui.rs::ClipboardAndDragBroker`、`clipboard-drag-guard.ts` | `trusted_ui_scrim_chooser_clip_side.rs` Test 5；`trusted-ui-security.test.ts` Suite 5 |
| `ANTI-AI-01..07` | 100% 实色表面、零模糊滤镜/霓虹发光/径向渐变、圆角严格属 `{0px, 2px, 4px, 6px}`、零 Emoji 象形字符、`zh-CN`/`en-US` 在 `L1/L2` 可见文本中零内部架构黑话，`L3` 精确披露 `ApplicationId`、`CapabilityRights`、`ControlScope`、`plan_digest` | `desktop/src/trusted-ui/*` | `trusted-ui-security.test.ts` Suite 6 & Suite 7；`compositor-wm.test.ts` Suite 8 |

---

## 3. 验证命令与实测结果（Windows x86_64）

```text
$ npx tsx desktop/src/trusted-ui/__tests__/trusted-ui-security.test.ts
  -> EXIT CODE: 0 (All 7 D-TRUSTED-UI suites passed — 333 deterministic assertions verified)

$ npx tsx desktop/src/compositor/__tests__/compositor-wm.test.ts
  -> EXIT CODE: 0 (All 8 D-COMPOSITOR-WM suites passed — 733 deterministic assertions verified)

$ cd desktop && npx tsc --noEmit
  -> EXIT CODE: 0 (0 TypeScript errors)

$ $env:CARGO_TARGET_DIR='d:\llmos\target\w71-trusted-ui'; cargo fmt --manifest-path desktop/src-tauri/Cargo.toml --all -- --check
  -> EXIT CODE: 0 (0 formatting diffs)

$ $env:CARGO_TARGET_DIR='d:\llmos\target\w71-trusted-ui'; cargo clippy --manifest-path desktop/src-tauri/Cargo.toml --features dev-fixture --all-targets -- -D warnings
  -> EXIT CODE: 0 (0 warnings)

$ $env:CARGO_TARGET_DIR='d:\llmos\target\w71-trusted-ui'; cargo test --manifest-path desktop/src-tauri/Cargo.toml --features dev-fixture --lib trusted_ui --no-fail-fast
  -> EXIT CODE: 0 (2 unit tests passed: fips_180_4_sha256_vectors_match_nist, leaf_name_sanitizer_blocks_host_paths_and_traversal)

$ $env:CARGO_TARGET_DIR='d:\llmos\target\w71-trusted-ui'; cargo test --manifest-path desktop/src-tauri/Cargo.toml --features dev-fixture --test trusted_ui_scrim_chooser_clip_side --no-fail-fast
  -> EXIT CODE: 0 (5 integration & negative-bypass suites passed, 0 failed)
```

---

## 4. 诚实范围边界说明（供 `W71-INT` 合流挂载）

1. **共享入口零触碰纪律**：遵照 `W71` 并行写集隔离规则，本车道未修改 `desktop/src-tauri/src/{lib.rs,ipc.rs,dto.rs}` 与 `desktop/src/{main.ts,ipc.ts,types.ts}`。
2. **`W71-INT` 即插即用挂载契约**：
   - Rust 侧：`desktop/src-tauri/src/lib.rs` 已预声明 `pub mod trusted_ui;`；`W71-INT` 只需注入 `.manage(trusted_ui::TrustedUiState::default())` 并在 `tauri::generate_handler![]` 中注册 `trusted_ui::{trusted_ui_open_z4_consent, trusted_ui_notify_z4_focus_regained, trusted_ui_decide_z4_consent, trusted_ui_get_public_posture, trusted_ui_open_chooser, trusted_ui_confirm_chooser_grant, trusted_ui_revoke_chooser_grant, trusted_ui_mint_gesture_token, trusted_ui_publish_transfer, trusted_ui_request_transfer_read, trusted_ui_complete_escalated_transfer, trusted_ui_get_security_audit}`（共 12 个命令处理器）。
   - 前端侧：`desktop/src/trusted-ui/index.ts` 已导出 `createTrustedUiSuite`、`Z4SecurityDialogController`、`ChooserGrantModalBroker` 与 `ClipboardAndDragGuard`，可直接挂载至 `WorkstationCompositor` 的 `Z4` 顶层容器槽位并与 `DesktopSession` 联动。
