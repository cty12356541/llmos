# E-MARKET-002：桌面商店透明目录、发布者证书链检视与 Z4 联动权限透明面板证据（`W80-E6C`）

- **Evidence ID**: `e-market-002-desktop-store-transparency-ui`
- **Wave / Lane**: `W80 / W80-E6C-DESKTOP-STORE-UI`（工作包 `E6` 桌面商店面，里程碑 `M-E4` 前置车道）
- **日期**: `2026-11-01`
- **Assurance Level**: `H4`（确定性本机单元 / 集成 / 组件级前端状态机与静态审计测试；不声称 `H7` 生产环境或物理对抗实证）
- **状态**: `VERIFIED`（车道内 Rust 与前端门禁全绿，且 `W80-INT` 已完成 `desktop/src-tauri/src/lib.rs` `invoke_handler`、`desktop/src/main.ts` 路由装配与 `desktop/src/style.css` 中央样式引入三处壳层接线，`desktop/src-tauri` 单元/集成测试、前端测试套件与 `tsc && vite build` 全绿，晋升 canonical `DONE`）
- **权威规范与治理依据**:
  - [06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §16 / §22（`[TUI-CONSENT-001]`、`[DUI-OFFLINE-001]`、`[TCB-EGRESS-001]`）、§23.1–§23.2（`[PKG-OCI-001]`、`[PKG-MULTI-001]`、`[PKG-APPID-001]`）、§6 / §7（`[TCB-DOWNGRADE-001]`、`[CAP-REVOKE-001]`）
  - [ADR-0025 阶段 D 混合桌面合成层与可信 UI 分层](../../management/adrs/0025-stage-d-hybrid-desktop-compositor-and-trusted-ui-layering.md)（修订 1 `Graphite / Paper` 实心工业工作站规范、`L0–L3` 渐进披露、`[UI-PARITY-001]`）
  - [ADR-0027 Stage E SABI 长期向后兼容与商店治理](../../management/adrs/0027-stage-e-sabi-long-term-compatibility-and-store-governance.md) §2.3
  - [stage-e-progress.md](../../management/stage-e-progress.md) §E.2.2 工作包 `E6` 第 3 条、§E.3.2 `W80` 排程
  - 上游数据面:[e-store-001](e-store-001-oci-index-and-entitlement-ledger.md)、[e-trust-001](e-trust-001-publisher-pki-revocation-and-selectors.md)（`W79` 已 `VERIFIED`）

---

## 1. 范围与架构对齐

本车道交付桌面端受控商店透明面,严格遵守 `[UI-PARITY-001]` 与 `ADR-0025` 视觉纪律,**零自造目录状态、零自绘安全确认屏**:

| 交付物 | 路径 | 职责 |
|---|---|---|
| Rust 桥接模块 | [desktop/src-tauri/src/store_catalog.rs](../../../desktop/src-tauri/src/store_catalog.rs) | 4 个 `#[tauri::command]` 命令 + 无 Tauri 句柄上下文核心,从 `nlos-store-registry` / `nlos-publisher-trust` 数据面读取目录、透明报告、提权差异并产出 `Z4` 入口投影 |
| Rust 集成测试 | [desktop/src-tauri/tests/store_catalog_trust_side.rs](../../../desktop/src-tauri/tests/store_catalog_trust_side.rs) | 真实三级 PKI + OCI CAS + 治理目录夹具驱动的 5 项集成测试 |
| 前端视图 | [desktop/src/views/store-catalog/](../../../desktop/src/views/store-catalog/) | `L1` 高密度目录表 / `L2` 权限透明检视器与证书链面板 / `L3` 深度核验面板、双语 i18n、Z4 联动入口 |
| 前端组件测试 | [desktop/src/views/store-catalog/__tests__/store-catalog-transparency.test.ts](../../../desktop/src/views/store-catalog/__tests__/store-catalog-transparency.test.ts) | 8 大套件 1933 项确定性断言(状态机、渲染、双语、反 AI 静态审计、Tauri 接线契约) |

## 2. 关键不变量与实现要点

1. **目录状态 100% 投影自真实数据面（`[PKG-OCI-001]`）**：快照（`store_catalog_snapshot`）仅调用 `SqliteStoreRegistry::list_official_stable_releases` / `list_community_sandbox_candidates` / `current_store_revocation_epoch`；明细（`store_release_transparency`）仅调用 `inspect_permission_transparency` + `inspect_release`；差异（`store_permission_diff`）仅调用 `diff_release_permission_transparency`。桥内无任何第二份目录缓存或旁路上架路径。
2. **发布者证书链透明 = 查看时真实复验（`[CAP-REVOKE-001]`）**：桥以发布记录字段重算域分离提交摘要（`compute_release_submission_digest`），经真实 `SqlitePublisherTrustAuthority::verify_publisher_signature_with_mode` 在线复验「叶 -> 中介 -> 根」链、`Ed25519` 严格签名、`RevocationEpoch` 与 CRL 防冻结窗口，产出三态 `chain_status`：
   - `VERIFIED`：复验通过（`signature_reverified = true`）；
   - `TRUST_DB_UNAVAILABLE`：本地信任库缺失/不可读——目录浏览按 `[DUI-OFFLINE-001]` 明确降级标注，不崩溃、不伪造核验结论；
   - `VERIFICATION_FAILED`：复验不通过（含 CRL 撤回叶证书路径，集成测试以 epoch 2 撤回 CRL 实证拦截）。
3. **敏感动作仅 `Z4` 入口投影（`[TUI-CONSENT-001]`）**：`store_prepare_sensitive_action` 仅接受三类法定动作（`INSTALL` / `IRREVERSIBLE_UPGRADE` / `DOWNGRADE_SANDBOX`），先过 `evaluate_new_install_gate`（撤回包 `ReleaseRecalled` → `Control` fail-closed），再强制链路复验通过（信任库缺失或复验失败一律 `Control` 拒绝入口），最终产出完全填充的既有 `trusted_ui::Z4ConsentChallengeRequest`（含 `+` / `~` 爆炸半径行、双语白话摘要、资源上限与可逆性等级）——确认屏本体 100% 复用既有 `Z4` 组件，本模块零自绘确认 UI、零旁路动词。前端 `toZ4ConsentChallengeSpec` 适配器把 wire DTO 无损转换为既有 `Z4ConsentChallengeSpec`（`plainSummary` 按语言键配对、`+`→`created`、`~`→`reverified`），交给既有 `Z4SecurityDialogController.openChallenge`。
4. **不可逆升级 / 降档沙箱的提权阻断（`[TCB-DOWNGRADE-001]`）**：不可逆升级强制携带 `old_version` 并加载跨版本差异；凡新增能力、新开网络出站、副作用升级（尤其升至 `IRREVERSIBLE`）、用途或沙箱姿态变更，`requiresZ4Reconsent = true` 并以琥珀色横幅白话呈现，阻断静默升级。
5. **`ADR-0025` 视觉与无障碍纪律**：`nlos-store-*` 全部复用既有 `--nlos-*` 令牌——100% 实心不透明表面、零 `backdrop-filter`、`0px/4px` 工业圆角（`var(--nlos-radius-ctrl)`）、`1px` 结构细线、单色几何 SVG 图标（`shield-seal` / `lock` / `check` / `warning` / `search`，零 Emoji）、`tabular-nums` 版本/纪元读数、双极克制警示色（琥珀=需用户介入、深绯红=失败/不可逆）。双语 `STORE_COPY` `zh-CN`/`en-US` 100% 键位对等，`L1/L2` 文案零内部架构行话、零 Emoji（复用 `a11y_i18n::contains_forbidden_l0_to_l2_jargon` / `contains_pictographic_emoji` 在 Rust 与前端双侧静态门禁）。
6. **离线确定性降级（`[DUI-OFFLINE-001]`）**：治理目录不可用时快照返回空白目录 + 双语明确停用通知（不报错、不锁死视图）；单发布物明细与敏感动作入口 fail-closed，绝不伪造明细或放行未核验动作。
7. **错误与脱敏纪律**：全部错误映射为稳定 `DesktopError` 码（`Config` / `NotFound` / `Control` / `Ipc`），消息零绝对路径、零密钥材料；集成测试断言 DTO 序列化不含临时目录路径片段。发布者提供的 `human_purpose` 等第三方文本在前端渲染前 100% HTML 转义（组件测试含 `<script>` 注入对抗断言）。

## 3. 验证门禁结果与复现命令

平台：Windows 11 Pro / x86_64-pc-windows-msvc / Rust stable、Node 24 + tsx 4.23。

### 3.1 Rust 侧（`desktop/src-tauri`）

```text
$ cargo fmt --check
  -> 通过(零漂移)

$ cargo clippy --all-targets -- -D warnings
  -> Finished `dev` profile(零警告零错误)

$ cargo test --lib
  -> test result: ok. 23 passed; 0 failed(含 store_catalog 模块内 3 项:
     bilingual_plain_copy_is_jargon_free_and_emoji_free /
     package_id_hex_parsing_rejects_malformed_input /
     fingerprint_and_entry_projection_shapes)

$ cargo test --test store_catalog_trust_side
  -> test result: ok. 5 passed; 0 failed
     1. snapshot_projects_live_catalog_from_store_registry_data_plane
     2. transparency_report_and_publisher_chain_reverification_with_crl_revocation_guard
     3. permission_escalation_diff_blocks_silent_upgrade_and_stays_plain_language
     4. sensitive_actions_project_z4_entry_and_fail_closed_on_unverified_inputs
     5. offline_store_root_degrades_deterministically_without_jargon_or_path_leak
```

集成测试夹具为**真实三级 PKI**（离线根 CA 自签 → `CertifiedPublisher` 中介 → 叶证书、`CommunitySandboxed` 自签叶）+ 真实 `LocalOciCasStore` 三组件 OCI 布局 + `WAL + FULL` 治理目录：`pkg_a` v1（双只读权限）经四门签名认证晋升 `Stable`、v2（新增出站 + 不可逆 + 用途变更 + 副作用升级）停留 `Candidate`、`pkg_b` v1（社区）入 `CommunityTrack`。对抗路径覆盖：epoch 2 CRL 撤回叶证书后链路复验转 `VERIFICATION_FAILED` 且敏感动作入口 `Control` fail-closed；信任库缺失时敏感动作入口 fail-closed 而浏览降级可读；未知包 `NotFound`；非法动作种类 `Config` 拒绝。

### 3.2 前端侧（`desktop/`，仓库根执行）

```text
$ npx tsx desktop/src/views/store-catalog/__tests__/store-catalog-transparency.test.ts
  -> EXIT CODE: 0(All 8 W80-E6C Desktop Store Transparency suites passed — 1933 assertions)
     1. Offline deterministic degradation, locale switch, and L1 posture
     2. Live catalog rows, L1 zero-jargon table, selection, L2 inspector, L3 raw fields
     3. Publisher certificate chain transparency panel (verified / degraded / failed)
     4. Permission escalation diff blocks silent upgrade with plain-language banner
     5. Sensitive action Z4 handoff: verified chain only, adapter shape, reversibility badges
     6. Bilingual copy parity with zero jargon and zero emoji
     7. Anti-AI static audit over store-catalog production files
     8. Tauri client wiring contract for W80-INT integration

$ npm run build    (tsc && vite build)
  -> tsc 零错误;vite built in 843ms(69 modules;chunk 大小警告为项目既有基线,非本车道引入)
```

前端 1933 项断言中含：离线降级与双语切换、L1/L2/L3 披露分层、证书链三态面板、提权阻断横幅、Z4 联动仅限复验通过链路（`blocked`/`handoff`/`error` 三态通知）、适配器形状全等（`plainSummary` 语言键配对、爆炸半径行类别映射、`SINGLE_ACTION` 授权范围）、`<script>` 注入转义对抗、`STORE_COPY` 双语键位 100% 对等 + 全值零行话零 Emoji、生产文件静态反 AI 审计（零 `backdrop-filter`、零违禁色、圆角仅 `0/2/4/6px` 字面量或令牌引用、零 Emoji、零绝对主机路径）与 Tauri 命令名 / camelCase 参数接线契约。

## 4. 车道独占写集与偏离声明

1. [desktop/src/views/store-catalog/](../../../desktop/src/views/store-catalog/)（`index.ts`、`store-catalog-view.ts`、`store-catalog.css`、`__tests__/store-catalog-transparency.test.ts`）
2. [desktop/src-tauri/src/store_catalog.rs](../../../desktop/src-tauri/src/store_catalog.rs)（覆写 `W80-0-SCAFFOLD` 占位）
3. [desktop/src-tauri/tests/store_catalog_trust_side.rs](../../../desktop/src-tauri/tests/store_catalog_trust_side.rs)
4. `docs/evidence/stage-e/e-market-002-desktop-store-transparency-ui.md`（本文件）

**写集偏离（唯一一处，如实登记）**：任务要求桥接读取 `nlos-store-registry` / `nlos-publisher-trust` 数据面，但 `W80-0-SCAFFOLD` 未向 `desktop/src-tauri/Cargo.toml` 预挂这两个一方 path 依赖，且该文件不在任何 `W80` 车道独占清单内（`W80-INT` 独占为 `main.ts` / `lib.rs` / `package.json` / `package-lock.json` / `docs/management/**`）。本车道据此向 [desktop/src-tauri/Cargo.toml](../../../desktop/src-tauri/Cargo.toml) 追加两行一方 crate path 依赖（`nlos-store-registry`、`nlos-publisher-trust`，均已在根 workspace 内、零新增第三方供应链，`RISK-E-04` 供应链锁定不受影响），并同步更新 `desktop/src-tauri/Cargo.lock`（desktop 独立 workspace 自有锁文件，非根 `Cargo.lock`）。若维护者裁定该文件归 `W80-INT`，此两行可原样由集成器确认保留。

## 5. `W80-INT` 壳层接线收口记录

| 接点 | 文件 | `W80-INT` 集成状态 |
|---|---|---|
| 命令注册 | `desktop/src-tauri/src/lib.rs` `generate_handler![]` | 已由 `W80-INT` 挂接 `store_catalog::store_catalog_snapshot`、`store_catalog::store_release_transparency`、`store_catalog::store_permission_diff`、`store_catalog::store_prepare_sensitive_action` |
| 前端路由 | `desktop/src/main.ts` | 已由 `W80-INT` 按既有 `register("task-space", ...)` 模式注册 `store-catalog` 视图，并用 `createTauriStoreCatalogClient()` + `createStoreCatalogController({ client, openZ4Challenge: (spec) => trustedUi.z4Dialog.openChallenge(...) })` 装配 |
| 样式引入 | `desktop/src/style.css` | 已由 `W80-INT` 追加 `@import "./views/store-catalog/store-catalog.css";`（沿用 omnibar 中央引入惯例） |
| 生产路径配置 | 桌面运行环境 | 设置 `LLMOS_DESKTOP_STORE_ROOT`（`store-registry.db` 所在目录）与 `LLMOS_DESKTOP_PUBLISHER_TRUST_DB`（`publisher-trust.db` 路径），或经命令入参 `storeRootOverride` / `trustDbOverride` 显式传入 |

## 6. 未运行项与诚实边界

- **实机物理窗口点击流未声称**：`W80-INT` 已完成 `desktop/src-tauri/src/lib.rs` `invoke_handler`、`desktop/src/main.ts` 路由与 `desktop/src/style.css` 样式引入三处壳层接线，并通过 `desktop/src-tauri` 单元/集成测试、前端全部组件状态机套件与 `tsc && vite build` 构建门禁，但本证据属确定性 `H4` 测试验证，不声称人工 GUI 物理点击流实证；
- **三平台 CI 未运行**：本车道仅 Windows 11 本机验证；macOS / Linux 侧模块无平台分支代码（桥接全平台纯 Rust + 既有 `spawn_blocking` 形态），三平台回归归 `W80-INT` 推送后 CI；
- **`H7` 供应链对抗实证见同波次伴生证据**：供应链 / 吊销 / 防抢注六类攻击矩阵由 `W80-E6D`（[e-security-001](e-security-001-h7-supply-chain-and-revocation-adversarial.md)）专项覆盖；本证据仅覆盖桌面桥接面的确定性复验与 fail-closed 路径；
- **根工作区与桌面工作区合流复验**：本车道内通过 `cargo test --lib`（23 项，含既有 20 项）与 `store_catalog_trust_side`（5 项），跨车道合流复验已由 `W80-INT` 完成。
