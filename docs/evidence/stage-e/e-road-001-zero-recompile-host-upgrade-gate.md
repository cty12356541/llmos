# E-ROAD-001：主系统 `N-1 -> N` 零重编译跨版本升级总门禁与复现矩阵证据（`W81-E7A`）

- **Evidence ID**: `e-road-001-zero-recompile-host-upgrade-gate`
- **Wave / Lane**: `W81 / W81-E7A-ROADE001-GATE`（工作包 `E7`：`[ROAD-E-001]` 主系统 `N-1 -> N` 跨版本零重编译升级总门禁与复现脚本矩阵）
- **日期**: `2026-11-02`
- **Assurance Level**: `H7`（长期兼容与安全 conformance 阶梯下的**确定性本地宿主跨版本原地升级全链路门禁 + 对抗性负向兼容/防篡改集成测试**；不声称 `H8` 生产多机机群金丝雀实证或 `H10` `TLA+/Lean` 形式化机械证明）
- **状态**: `VERIFIED`（`W81-E7A` 独立 git worktree 独占写集交付并经 `W81-INT` 合流复验：全部 Rust / Python 门禁实跑全绿，16 份冻结 `N-1` 封存制品 `pre == post` 零字节漂移，已纳入常设跨平台 CI 守护）
- **验证平台**: Windows（独立 git worktree 基线 `a44d4cd4`，Rust stable `1.97+` / edition `2024`，Python `3.x` 标准库）
- **权威规范与治理依据**:
  - [06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §28.5（`[ROAD-E-001]` 阶段 E 完成判据）、§9（`COMPAT-VER-001`、`COMPAT-VER-002`、`COMPAT-DEPRECATE-001`、`KABI-SVC-001`、`KABI-UNCERTAIN-001`、`SABI-DISC-001`、`SABI-CONFORM-001`）、§15.1–§15.3（`DRV-SHIM-001`、`DRV-CRED-001`、`DRV-LIFE-001`）、§23.1–§23.3（`PKG-APPID-001`、`PKG-UPDATE-001`、`PKG-UNINSTALL-001`）、§6/§24.2（`TCB-BYPASS-001`、`CAP-REVOKE-001`）、§31（`EVID-SCOPE-001`、`EVID-REPRO-001`）
  - [stage-e-progress.md](../../management/stage-e-progress.md) §E.2.2 工作包 `E7`、§E.3.2 `W81-E7A` 车道契约、§E.5.5 退出门禁 1–5
  - [ADR-0027 Stage E SABI 长期向后兼容与商店治理](../../management/adrs/0027-stage-e-sabi-long-term-compatibility-and-store-governance.md) §2.4（步骤 A 冻结封存 -> 步骤 B `N-1` 运行 -> 步骤 C 主系统原地升级至 `N` -> 步骤 D 零重编译继续运行与全生命周期演练）
  - [ADR-0024 Driver Provider 线级载荷 Codec 冻结](../../management/adrs/0024-driver-provider-payload-codec-freeze.md)
  - 前序基座证据：[E-SABI-001](e-sabi-001-version-matrix-and-deprecation.md)、[E-SHIM-001](e-shim-001-out-of-core-epoch-adapter.md)、[E-TRUST-001](e-trust-001-publisher-pki-revocation-and-selectors.md)、[E-STORE-001](e-store-001-oci-index-and-entitlement-ledger.md)、[E-CERTIFY-001](e-certify-001-automated-conformance-and-uninstall-gate.md)、[E-MIGRATE-001](e-migrate-001-static-checker-and-rollbacks.md)、[E-SANDBOX-001](e-sandbox-001-cross-platform-wasm-process-isolation.md)、[E-MARKET-001](e-market-001-reference-apps-and-drivers.md)、[E-MARKET-002](e-market-002-desktop-store-transparency-ui.md)、[E-SECURITY-001](e-security-001-h7-supply-chain-and-revocation-adversarial.md)

---

## 1. 交付物与独占写集边界

本车道严格遵循 `stage-e-progress.md` §E.3.2 `W81-E7A` 写集契约，不触碰根 `Cargo.toml`、根 `Cargo.lock`、`crates/**`、`examples/**` 或 `docs/management/**`：

1. **独立脱离式 Cargo 工作区门禁套件**（`[workspace]` `members = ["."]`，`#![forbid(unsafe_code)]`，`clippy::all = "warn"` + `clippy::pedantic = "warn"`，零新增第三方外部依赖）：
   - [tests/stage_e_road_e001_upgrade_gate/Cargo.toml](../../../tests/stage_e_road_e001_upgrade_gate/Cargo.toml) 与锁定依赖图 `Cargo.lock`、局部 `.gitignore`（忽略 `/target/`）
   - [tests/stage_e_road_e001_upgrade_gate/src/bin/road_e001_wasm_sandbox_host.rs](../../../tests/stage_e_road_e001_upgrade_gate/src/bin/road_e001_wasm_sandbox_host.rs)：封装 `nlos_wasm_sandbox::host_bin::host_main` 的薄宿主子进程二进制，供脱离式工作区通过 `CARGO_BIN_EXE_road-e001-wasm-sandbox-host` 启动真实双层隔离沙箱子进程
   - [tests/stage_e_road_e001_upgrade_gate/src/lib.rs](../../../tests/stage_e_road_e001_upgrade_gate/src/lib.rs)：门禁编排入口 `run_full_road_e001_upgrade_chain`、16 文件 `SHA-256` 零漂移快照器 `SealedBaselineSnapshot` 与隔离宿主状态目录 `TempHostWorkspace`
   - [tests/stage_e_road_e001_upgrade_gate/src/step_a_frozen_seal.rs](../../../tests/stage_e_road_e001_upgrade_gate/src/step_a_frozen_seal.rs)：`ADR-0027` §2.4 步骤 A 冻结 `N-1` 产物封存核验与三级 PKI + OCI CAS + `SqliteStoreRegistry` `Candidate -> Stable` 治理重放
   - [tests/stage_e_road_e001_upgrade_gate/src/step_b_n1_host.rs](../../../tests/stage_e_road_e001_upgrade_gate/src/step_b_n1_host.rs)：`ADR-0027` §2.4 步骤 B 在主系统 `N-1`（`host_epoch = 1`）上安装并驱动 4 个冻结 `v1.0.0.nlos-pkg`（2 个多组件 Application + 2 个用户态 Driver）落盘完整持久化权威状态
   - [tests/stage_e_road_e001_upgrade_gate/src/step_c_in_place_upgrade.rs](../../../tests/stage_e_road_e001_upgrade_gate/src/step_c_in_place_upgrade.rs)：`ADR-0027` §2.4 步骤 C 主系统原地升级 `N-1 -> N`（`host_epoch = 1 -> 2`）、内核私有 `KABI` 内部结构重构对外 `SABI` 线级零漂移证明、演进版 `DeprecationManifest` 校验与 `[COMPAT-VER-002]` 正负兼容矩阵
   - [tests/stage_e_road_e001_upgrade_gate/src/step_d_n_recovery_and_lifecycle.rs](../../../tests/stage_e_road_e001_upgrade_gate/src/step_d_n_recovery_and_lifecycle.rs)：`ADR-0027` §2.4 步骤 D（`D.1..D.7`）在主系统 `N` 上零重编译恢复全部 `N-1` 持久化权威状态、核外兼容垫片中介与零凭据泄漏审计、驱动热插拔与 `UNCERTAIN` 同幂等键对账、双层 Wasm 进程沙箱围栏、冻结 `v1.1.0.nlos-pkg` 故障注入原子回滚与健康热升级、以及干净卸载与能力级联吊销
   - [tests/stage_e_road_e001_upgrade_gate/tests/road_e001_zero_recompile_upgrade_gate.rs](../../../tests/stage_e_road_e001_upgrade_gate/tests/road_e001_zero_recompile_upgrade_gate.rs)：6 个端到端与 `H7` 对抗性负向集成测试
2. **纯标准库可复现验证矩阵脚本**：
   - [scripts/run_road_e001_upgrade_matrix.py](../../../scripts/run_road_e001_upgrade_matrix.py)：执行运行前 16 份封存文件 `SHA-256` 快照与 `.store-seal.txt` 交叉对账 -> 审计 `examples/ecosystem-*` 全部 `Cargo.toml` 零内核私有依赖 -> 调用独立工作区 `cargo test` 全链路门禁 -> 执行运行后 `SHA-256` 快照并断言 `pre == post`（`zero_recompile_verified = true`）；内置 `--self-test`（5 项 `unittest` 自检，含临时副本 1 字节篡改/重签拦截验证）与 `--json` 结构化回执输出
3. **本证据文档**：
   - `docs/evidence/stage-e/e-road-001-zero-recompile-host-upgrade-gate.md`

---

## 2. 冻结 `N-1` 基准包零重编译 `SHA-256` 对账表（`pre == post`）

门禁全程直接从 `examples/ecosystem-apps/*/sealed/` 与 `examples/ecosystem-drivers/*/sealed/` 读取由 `W80-E6B` 冻结入库的 8 个 `.nlos-pkg` 二进制包、4 份 `.certify.json` 四门认证收据与 4 份 `.store-seal.txt` 商店封存记录（共 16 个文件），**不调用 `nlos-package build`、不重打包、不重签任何字节**。`scripts/run_road_e001_upgrade_matrix.py --json` 实跑前后快照逐字节全等（`drifted_files = []`，`zero_recompile_verified = true`）：

| 样板包 | 类型 | 版本 | 仓库相对路径 | 字节数 | 运行前 / 运行后 `SHA-256`（`pre == post`） |
|---|---|---|---|---:|---|
| `knowledge-observatory-app` | 多组件 Application | `v1.0.0` | `examples/ecosystem-apps/knowledge-observatory-app/sealed/knowledge-observatory-app.v1.0.0.nlos-pkg` | `2538` | `690c4e296bf3b0bedd56b17592064c04e3eac8defcf14d4d1e988e0daf4602d5` |
| `knowledge-observatory-app` | 多组件 Application | `v1.1.0` | `examples/ecosystem-apps/knowledge-observatory-app/sealed/knowledge-observatory-app.v1.1.0.nlos-pkg` | `2828` | `0bc978a23cc62571503bdd939aef6be76f692b5f2d61e26bb227cb0398191985` |
| `field-survey-app` | 多组件 Application | `v1.0.0` | `examples/ecosystem-apps/field-survey-app/sealed/field-survey-app.v1.0.0.nlos-pkg` | `2341` | `4e529d61239bec8bf289993fd7294991207842fc77620ed7cefa0f19f9d1c4eb` |
| `field-survey-app` | 多组件 Application | `v1.1.0` | `examples/ecosystem-apps/field-survey-app/sealed/field-survey-app.v1.1.0.nlos-pkg` | `2618` | `59f219d130fd1845c9d307e6e2e799ca11dbf8e1fe3d05b5f663e16d292111c2` |
| `mcp-bridge-driver` | 用户态 Driver（MCP JSON-RPC 2.0） | `v1.0.0` | `examples/ecosystem-drivers/mcp-bridge-driver/sealed/mcp-bridge-driver.v1.0.0.nlos-pkg` | `2327` | `6bff8ec8aeaff6d478eabbb6dbca97e062660fb711b719ff13ec9c67ec1fe83d` |
| `mcp-bridge-driver` | 用户态 Driver（MCP JSON-RPC 2.0） | `v1.1.0` | `examples/ecosystem-drivers/mcp-bridge-driver/sealed/mcp-bridge-driver.v1.1.0.nlos-pkg` | `2778` | `9a6ec0ebb328a5064b8781a7996f87cffb64dd35b1e6acf2fab8c52881fa525b` |
| `openai-compat-driver` | 用户态 Driver（OpenAI `/v1/chat/completions` + SSE） | `v1.0.0` | `examples/ecosystem-drivers/openai-compat-driver/sealed/openai-compat-driver.v1.0.0.nlos-pkg` | `2080` | `db0d93d156dd07f6efc937b891dfc97bf9b363aeb0c28081382d22e1e68be0e1` |
| `openai-compat-driver` | 用户态 Driver（OpenAI `/v1/chat/completions` + SSE） | `v1.1.0` | `examples/ecosystem-drivers/openai-compat-driver/sealed/openai-compat-driver.v1.1.0.nlos-pkg` | `2413` | `0467b105904b8b013524c880eef32d33d1fc7fe3f13a661dd6fadfb41940de09` |

其余 8 份封存伴生文件（4 份 `.v1.0.0.certify.json` + 4 份 `.v1.0.0.store-seal.txt`）同样纳入 `SealedBaselineSnapshot` 与 `run_road_e001_upgrade_matrix.py` 前后 `SHA-256` 比对，16/16 文件零改动。

---

## 3. `ADR-0027` §2.4 四步升级链路验证摘要（Steps A -> B -> C -> D）

| 阶段 | 模块与入口 | 机械验证内容 | 关键断言与实测结果 |
|---|---|---|---|
| **步骤 A：冻结 `N-1` 产物封存核验** | `step_a_frozen_seal::run_step_a_verify_frozen_n1_baseline` | 1. 校验 16 份封存文件存在且 8 个 `.nlos-pkg` 的 `SHA-256`、字节长度、`PackageId`、entry 数、task 数与 `.store-seal.txt` 完全一致；2. 对 8 个 `.nlos-pkg` 执行 `check_package_file` 与 `decode_package_file`；3. 解析 4 份 `.certify.json` 并用 `verify_store_certification_receipt` 验签；4. 在隔离临时目录重放三级 PKI（`OfflinePlatformRootCa -> StoreIntermediateCertificate -> CertifiedPublisher`）、`RevocationEpoch(1)` CRL、`LocalOciCasStore` 五槽位多组件布局写入/校验与 `SqliteStoreRegistry` `Candidate -> Stable`（`passed_gates_mask = 0b1111`）晋升（为兼容 Windows `core.autocrlf`，在隔离治理目录内将源树文本统一做 LF 归一化暂存，不触碰仓库任何字节） | `8/8` `.nlos-pkg` conformance 通过；`4/4` `.certify.json` 验签通过；重放所得 `oci_manifest_digest`、`certify_manifest_digest`、`release_submission_digest`、`attestation_signature`、`certification_receipt_id` 与 `.store-seal.txt` 100% 逐位一致；`4/4` 官方 `Stable` 发布入库 |
| **步骤 B：主系统 `N-1`（`host_epoch = 1`）全量建态** | `step_b_n1_host::run_step_b_exercise_n1_host` | 1. 在同一持久化 `TempHostWorkspace` 打开 `SliceKRuntime`、`SqlitePublisherTrustAuthority`、`LocalOciCasStore`、`SqliteStoreRegistry`、`ResourceAuthority`、`MeteredChatGateway` 与 `SqliteEcosystemDependencyGate`；2. 直接消费 4 个冻结 `v1.0.0.nlos-pkg` 完成验签、安装、可执行入口运行与只读 seed 读取；3. 校验 `SearchUseRestrictions::NON_AUTHORITATIVE`（`0x1F`）拦截全部 5 类控制面消费者，拦截浮动版本标签（`latest`、`*`、`^1.0.0`、`>=1.0.0`）与 `UnpinnedSearchResult`，并绑定+物化验证 4 类跨生态固定选择器（`Package`、`Driver`、`Artifact`、`Dataset`）；4. 推进两套 App 的 3 节点惰性物化 `TaskPlan` DAG（验证前置未完成时 `DependenciesNotReady` fail-closed）与 3 版本因果血缘 `ArtifactStore` 修订链，创建跨应用共享语料 `Artifact`；5. 签发 `knowledge-observatory-app` 根能力（`8 -> 7`）与 `field-survey-app` 根能力+衰减委托子能力（`4 -> 3`）；6. 注册两个用户态 Driver（`Generation(1)`），执行 `McpBridgeAdapter`（验证 `DRV-NEGO-001` 版本失配 fail-closed、3 类工具调用与同键幂等重放，并保留 1 笔在途未决调用跨升级交接）与 `MeteredChatGateway`（`SseParser` + `StreamAggregator` 聚合 `"N-1 baseline sealed"`、五维 `LlmTokenBudget` 扣减 `total_tokens = 2400` / `api_calls = 2` 及同摘要零重复扣费）；7. 启动 `field-survey-app` 后台同步 `Task` 保持 `Active`，落盘各应用 `UserData` 与私有运行缓存并记录 `SHA-256` 树摘要 | `4/4` 冻结 `v1.0.0` 包安装为 `Generation(1)`；`6` 个 `TaskPlan` 节点完成（`3 + 3`）；`4/4` 跨生态固定依赖绑定就绪；`2` 个应用因果 `Artifact` 达 `revision = 3`；后台 `Task` 保持 `Active`；全部 SQLite 连接干净关闭等待原地升级 |
| **步骤 C：主系统原地升级 `N-1 -> N`（`host_epoch = 1 -> 2`）** | `step_c_in_place_upgrade::run_step_c_in_place_host_upgrade` | 1. **内核私有 `KABI` 重构隔离证明（`[KABI-SVC-001]`）**：将宿主内部私有 `HostN1PrivateKabiTable`（紧凑 `u32` 槽位句柄 + 平面切片）彻底替换为 `HostN2RefactoredPrivateKabiArena`（分片 `128`-bit 键 + `64`-bit 代数围栏桶），验证冻结 `N-1`（`major = 1`）客户端经 `SabiEpochAdapterCell` 往返收到的 `SABI` 响应字节与重构前**逐字节全等**；2. **演进版 `DeprecationManifest`（`current_epoch = 2`）**：验证 9 个冻结通道、`nlos.sabi.Envelope` 永久墓碑槽位 `901` / 符号 `RetiredExperimentalPreV1Slot`、`LegacyObservatorySyncV1`（`deprecated_in_epoch = 2`、`earliest_sunset_epoch = 4`、`replacement = "ObservatorySyncV2"`），并负向验证提前日落（`earliest_sunset_epoch = 3`）fail-closed；3. **`[COMPAT-VER-002]` 正负线级矩阵**：验证 `major = 0/3/99`、滑出 `[N, N-1, N-2]` 窗口（宿主 `epoch = 4` 下的 `major = 1`）、未知 `critical_extension_ids = [9999]`、以及墓碑符号均映射为 `E_ABI_VERSION` + `DO_NOT_RETRY`；未知 `non_critical_extension_ids = [7701, 7702]` 与未知原始 Protobuf wire 字段（tag `555`）跨 `N-1 <-> N` 往返无损保留；4. 原地重开步骤 B 的同一持久化权威状态树 | `KABI` 重构前后 `SABI` 线级响应 `0` 字节差异；`>= 6` 类负向不兼容输入 100% 返回 `E_ABI_VERSION` + `DO_NOT_RETRY`；非关键扩展 `[7701, 7702]` 与未知 Protobuf tag 字节 100% 保留；原地 SQLite 状态零擦除重开成功 |
| **步骤 D.1：持久化权威状态零重编译恢复与防抢注** | `step_d_n_recovery_and_lifecycle`（`D.1`） | 重开 `SliceKRuntime`、商店治理三库、`MeteredChatGateway` 与 `SqliteEcosystemDependencyGate`：校验 4 个 `InstallationId`（`Generation(1)`）、`(PublisherId, ApplicationId)` 归属绑定（跨发布者抢注精确拦截为 `ApplicationIdHijackAttempt`）、能力剩余调用额度（在宿主 `N` 上继续消费 `7 -> 6`）与委托子能力有效性、3 套 `ArtifactStore` 修订摘要、五维 `LlmTokenBudget` 精确恢复与 `N-1` 请求摘要跨升级重放零重复计费、4 个 `PinnedEcosystemDependencyHandle` 复验通过（注入内容哈希漂移精确返回 `DriftDetected`）、持久化 `Task`/`TaskPlan` 状态与 4 套 `UserData` `SHA-256` 树摘要 | `4/4` 安装记录与 `UserData` 树摘要 100% 匹配 `N-1`；跨发布者抢注 fail-closed；能力额度 `7 -> 6` 连续；五维 Token 账本跨升级幂等重放 `0` 额外扣费 |
| **步骤 D.2：`ServiceDirectory` 兼容路由与有界弃用告警** | `step_d_n_recovery_and_lifecycle`（`D.2`） | 在宿主 `N` 的 `SnapshotDirectory` 上验证 `N-1` 客户端解析与协商 `DEPRECATED` 符号 `LegacyObservatorySyncV1` 时正常路由并发出结构化 `DeprecationWarningReceipt`（携带 `replacement` 与 `earliest_sunset_epoch = 4`）；压力写入 `> 256` 条告警验证 `MAX_DEPRECATION_WARNINGS = 256` 有界环形缓冲上限；请求 `RESERVED` 墓碑符号精确拒绝为 `E_ABI_VERSION` + `DO_NOT_RETRY`；在宿主 `N` 上直接运行 `N-1` 已安装的 `knowledge-observatory-app` 与 `field-survey-app` 可执行 payload | 弃用告警字段完整且环形队列严格有界于 `256`；墓碑请求 fail-closed；`2/2` `N-1` 已安装应用在宿主 `N` 上零重编译直接执行成功 |
| **步骤 D.3：核外兼容垫片中介与零凭据泄漏审计** | `step_d_n_recovery_and_lifecycle`（`D.3`） | 驱动 `TrustedEnforcementGate` + `OutOfCoreCompatShimRuntime`：1. 未适配 `N-1` 帧直连宿主 `N` 精确拒绝为 `BoundaryViolation::LegacyMajorRequiresOutOfCoreShim`；2. 垫片伪造 `PROVIDER_ISOLATION_VERIFIED_TAG` 提权精确拒绝为 `BoundaryViolation::ForgedIsolationAttestationTag`；3. 合法核外中介链（`prepare_mediated_driver_dispatch` -> `adapt_n_to_legacy_driver` -> 冻结 `N-1` `DriverProvider` -> `adapt_legacy_driver_to_n` -> `verify_and_finalize_driver_response`）端到端保持 `128`-bit `request_id` 与 `IdempotencyKey` 不变；4. 扫描全部流经非特权垫片的线级帧，断言密封 `SecretCredential`（`sk-live-road-e001-top-secret-credential-99887766`）泄漏字节数为 `0` | 两条越权路径 100% 拦截；`SecretCredential` 在非特权垫片全部进出帧上的明文/片段出现字节数 = `0` |
| **步骤 D.4：用户态驱动热插拔与崩溃 `UNCERTAIN` 同幂等键对账** | `step_d_n_recovery_and_lifecycle`（`D.4`） | 1. 对 `mcp-bridge-driver` 与 `openai-compat-driver` 执行 `ResourceAuthority::rotate_driver` 热轮换（`Generation(1) -> Generation(2)`），验证旧代数句柄操作被 `GenerationFenceConflict` 围栏拒绝；2. 在 `TrustedEnforcementGate` 注入垫片/驱动中途崩溃 `fence_shim_or_driver_crash`，验证操作进入 `OperationState::EffectUnknown` + 资源预留 `ReservationState::Quarantined`，换新 `IdempotencyKey` 盲目重试被 `IdempotencyConflict` 拒绝，唯有持**原 `IdempotencyKey`** 对账方可安全收敛结算；3. 在用户态 `McpBridgeAdapter` 注入 `tools/call` 传输静默断连与 `simulate_crash()`，验证进入 `BridgeState::Recovering`（`uncertain_count = 1`），对账前同键盲目 `call_tool` 被 `McpBridgeError::Fence` 拒绝且不重发外部请求，持原 `(op_id, idemp_key)` 调用 `reconcile(EffectLanded { receipt: 77 })` 后恢复 `BridgeState::Ready`，后续同键调用返回 `CallOutcome::Replayed` 且服务端实际执行次数恒为 `1` | 两个驱动热轮换至 `Generation(2)`；内核门与用户态 `McpBridgeAdapter` 两条 `UNCERTAIN` 路径均 100% 拒绝盲目重试并在原 `IdempotencyKey` 下零重复副作用收敛 |
| **步骤 D.5：双层 `nlos-wasm-sandbox` 强隔离围栏（`U-9`）** | `step_d_n_recovery_and_lifecycle`（`D.5`） | 启动真实独立子进程 `road-e001-wasm-sandbox-host`：1. `--self-audit-env` 验证子进程继承宿主环境变量数为 `0`；2. 授权 `/workspace` preopen 正向读写 `made.txt` 成功；3. `../outside-host-secret.txt` 父目录穿越返回 `WASI ENOTCAPABLE (76)` 且宿主外部机密文件毫发无损；4. POSIX `/etc/passwd` 与 Windows `C:\Windows\win.ini` 绝对路径逃逸均返回 `ENOTCAPABLE (76)`；5. 声明 `sock_open` 网络导入的恶意模块在实例化阶段即被 `HostOutcome::LinkRejected` 拒绝；6. 死循环模块精确触发 `HostOutcome::FuelExhausted`；7. 超额 `memory.grow` 被线性内存上限拒绝并触发确定性 `HostOutcome::Trap` | 子进程继承环境变量 = `0`；正向受控写成功；路径穿越/绝对路径/网络导入/算力耗尽/内存越界 6 类沙箱逃逸与资源滥用 100% fail-closed |
| **步骤 D.6：冻结 `v1.1.0.nlos-pkg` 权限透明门、故障注入原子回滚与热升级** | `step_d_n_recovery_and_lifecycle`（`D.6`） | 1. `[TUI-CONSENT-001]`：`diff_permission_transparency` 识别新增不可逆副作用与外发网络权限并强制 `requires_tui_reconsent = true`；2. `[PKG-UPDATE-001]`：验证伪装单向不可逆迁移（`DisguisedIrreversibleMigration`）、缺失升级前备份（`MissingPreUpgradeBackup`）与缺失 `Z4` 确认（`MissingZ4TrustedUiConsent`）三条安全门均 fail-closed；3. 在 `field-survey-app` `v1.0.0 -> v1.1.0` 升级中注入健康探针失败与半写 `.tmp` 残留，验证原子回滚后应用保持 `Generation(1)`、`.tmp` 残留文件数为 `0`、`UserData` 容器树 `SHA-256` 与升级前**逐位一致**；4. 直接消费冻结 `v1.1.0.nlos-pkg` 将 `knowledge-observatory-app`、`mcp-bridge-driver`、`openai-compat-driver` 健康升级至 `Generation(2)`，并将 `knowledge-observatory-app` 的 `TaskPlan` 演进至 `revision = 2`（验证 `N-1` 已完成的 3 个上游节点保持 `Completed` 与 `declared_revision = 1` 执行冻结，激活并完成新增第 4 节点 `verify-digest`，提交 `ArtifactStore` `revision = 4`，并运行升级后 `curator` 载荷） | 权限提权静默升级被阻断；`field-survey-app` 故障注入回滚 `UserData` 摘要逐位还原且 `.tmp = 0`；3 个冻结 `v1.1.0` 包零重编译升至 `Generation(2)`，`TaskPlan` `revision = 2`（链长 `2`、节点 `4`）与 `Artifact` `revision = 4` 落盘 |
| **步骤 D.7：干净卸载、能力级联吊销与 `UserData` `Z4` 保留定律** | `step_d_n_recovery_and_lifecycle`（`D.7`） | 1. 当 `field-survey-app` 后台 `Task` 仍处于 `Active` 时调用 `uninstall_application_with_task_activity_gate`，精确拦截为 `ApplicationActiveTasksRunning`；2. 取消后台 `Task` 并以 `Z4` 操作员签名吊销 `survey_root_capability`，验证旧代数根句柄（`GenerationFenceConflict`）、当前代数根句柄（`CapabilityRevoked`）与委托子能力句柄（`AncestorRevokedOrFenced`）立即全部失效；3. 执行干净卸载：状态转为 `ApplicationStatus::Uninstalled`（再次执行 payload 立即拒绝），清空私有运行缓存（残留文件数 = `0`），跨应用共享 `Artifact` 摘要逐位完好；4. 验证 `UserData` 定律：默认卸载 100% 保留 `N-1` `UserData`；伪造 `Z4` 清除签名被 `InvalidSignature` 拦截且不删一字；唯有通过 `IdentityAuthority` 验签的合法 `Z4` 授权收据方可清空 `UserData`（剩余文件数 = `0`） | 活跃后台任务阻断卸载成立；根能力与委托子能力级联吊销即时生效；私有缓存残留 = `0`；共享 `Artifact` 零损坏；`UserData` 默认保留、伪造 `Z4` 拒删、合法 `Z4` 净空三态全验 |

---

## 4. 架构总纲 Requirement ID 覆盖矩阵

| Requirement ID | 权威条款域 | 覆盖位置（代码与测试） | 验证结论 |
|---|---|---|---|
| `ROAD-E-001` | §28.5 主系统 `N-1 -> N` 零重编译跨版本升级完成判据 | `run_full_road_e001_upgrade_chain`、`full_road_e001_n1_to_n_zero_recompile_cross_version_upgrade_chain`、`scripts/run_road_e001_upgrade_matrix.py` | `PASS`（8 个冻结 `.nlos-pkg` 零重编译跨 `N-1 -> N` 全链路运行，`16/16` 封存文件 `pre == post`） |
| `COMPAT-VER-001` | §9 `N / N-1 / N-2` 向后兼容窗口与主/次版本契约 | `step_c_in_place_upgrade::build_host_n_evolved_deprecation_manifest`、`CompatMountPlanner` | `PASS`（`min_compatibility_window_epochs = 2`，`N-1` 包在宿主 `N` 受控兼容挂载） |
| `COMPAT-VER-002` | §9 未知主版本/关键扩展 fail-closed、非关键扩展/未知字段保留 | `step_c_compat_ver_002_rejects_unknown_major_critical_ext_and_tombstones_while_preserving_unknown_fields` | `PASS`（6 类非法版本/扩展/墓碑返回 `E_ABI_VERSION` + `DO_NOT_RETRY`；`[7701, 7702]` 与未知 tag 字节无损保留） |
| `COMPAT-DEPRECATE-001` | §9 机器可读弃用清单、最短日落窗口与结构化弃用告警 | `build_host_n_evolved_deprecation_manifest`、`verify_d2_service_directory_and_payload_execution` | `PASS`（提前日落 `< +2` 纪元 fail-closed；`DeprecationWarningReceipt` 含替代符号且有界 `256`） |
| `KABI-SVC-001` | §9 内核私有 `KABI` 不外泄为生态 ABI、内部重构对 `SABI` 透明 | `step_c_kabi_internal_refactor_insulates_frozen_n1_sabi_wire_responses`、`h7_ecosystem_manifests_remain_free_of_private_kernel_crate_dependencies` | `PASS`（`KABI` 句柄/分片重构后 `SABI` 线级响应逐字节不变；生态样板 `Cargo.toml` 零私有内核依赖） |
| `KABI-UNCERTAIN-001` | §9/§15 崩溃/超时进入 `UNCERTAIN` 围栏并持原 `IdempotencyKey` 对账 | `verify_d3_and_d4_tcb_shim_mediation_and_uncertain_reconcile`、`verify_d4_driver_hotplug_and_mcp_uncertain_reconcile` | `PASS`（`EffectUnknown` + `Quarantined` 与 `BridgeState::Recovering` 均拒绝盲目重试并在同键对账下零重复执行收敛） |
| `SABI-DISC-001` | §9 `ServiceDirectory` 版本协商、弃用路由与墓碑拒绝 | `verify_d2_service_directory_and_payload_execution` | `PASS`（`SnapshotDirectory` 兼容协商成功，墓碑符号返回 `E_ABI_VERSION` + `DO_NOT_RETRY`） |
| `SABI-CONFORM-001` | §9/§23 包格式、清单与签名一致性门禁 | `step_a_frozen_n1_baseline_seals_certifications_and_cas_promotions_verify`、`h7_adversarial_tampered_frozen_package_and_forged_certification_fail_closed` | `PASS`（8 个冻结包 conformance 全绿；1 字节载荷/签名/收据篡改 100% fail-closed） |
| `DRV-SHIM-001` | §15.2 旧版协议兼容垫片运行于非特权核外域并经可信门中介 | `verify_d3_and_d4_tcb_shim_mediation_and_uncertain_reconcile` | `PASS`（未中介直连与伪造隔离标签提权 100% 拦截；中介往返保持 `request_id` 与 `IdempotencyKey`） |
| `DRV-CRED-001` | §15.2 密封 `SecretCredential` 零字节泄漏至非特权兼容垫片 | `verify_d3_and_d4_tcb_shim_mediation_and_uncertain_reconcile` | `PASS`（全部进出非特权垫片的线级帧中凭据明文/切片字节数 = `0`） |
| `DRV-LIFE-001` | §15.1/§15.3 用户态驱动版本协商、热轮换代数围栏与崩溃恢复 | `step_b_n1_host`（MCP 版本失配拒绝）、`verify_d4_driver_hotplug_and_mcp_uncertain_reconcile` | `PASS`（`rotate_driver` 升至 `Generation(2)` 并围栏旧代数；MCP 崩溃恢复零重复执行） |
| `PKG-UPDATE-001` | §23.3 事务化包升级、不可逆迁移安全门与故障注入原子回滚 | `verify_d6_package_upgrade_rollback_and_v1_1_0_rollout` | `PASS`（三条安全门阻断非法迁移；探针失败回滚至 `Generation(1)` 且 `UserData` 树摘要逐位还原；健康升级升至 `Generation(2)`） |
| `PKG-UNINSTALL-001` | §23.3 活跃任务卸载门控、私有缓存净空、共享产物保留与 `UserData` `Z4` 规则 | `verify_d7_clean_uninstall_and_revocation` | `PASS`（活跃后台任务阻断卸载；卸载后私有缓存 `0` 残留、共享 `Artifact` 完好、`UserData` 默认保留且仅合法 `Z4` 可净空） |
| `TCB-BYPASS-001` | §6/§24 双层 Wasm 进程沙箱不可绕过文件系统/网络/环境变量/配额围栏 | `verify_d5_dual_layer_wasm_sandbox` | `PASS`（真实子进程 `0` 继承环境变量；`..` 穿越、POSIX/Windows 绝对路径、`sock_open`、fuel/memory 超限 100% 拦截） |
| `CAP-REVOKE-001` | §6/§24.2 能力吊销对根句柄与衰减委托子句柄即时生效 | `verify_d7_clean_uninstall_and_revocation` | `PASS`（`revoke_signed` 后旧根 `GenerationFenceConflict`、现根 `CapabilityRevoked`、委托子句柄 `AncestorRevokedOrFenced`） |
| `EVID-SCOPE-001` | §31 证据分级诚实声明（`Claim <= Evidence`） | 本文档扉页与 §6 诚实边界 | `PASS`（如实标注 `CANDIDATE` 与 `H7` 确定性本地门禁边界，不冒充 `H8`/`H10`） |
| `EVID-REPRO-001` | §31 单命令确定性可复现验证与防漂移校验 | `scripts/run_road_e001_upgrade_matrix.py`（默认、`--json`、`--self-test`） | `PASS`（固定种子 `SEAL_T0_MS`、零宿主机时钟过期的能力对账时间戳、前后 16 文件 `SHA-256` 自动对账） |

---

## 5. 验证门禁结果与精确复现命令

> **Windows `cargo fmt` 参数说明**：由于 `tests/stage_e_road_e001_upgrade_gate/Cargo.toml` 是通过相对路径引用仓库内公共权威 crate 的独立脱离式工作区，在 Windows 上切勿传入 `--all`（`cargo fmt --all` 会把全部路径依赖 crate 展开到单条 `rustfmt` 命令行，触发 Windows `os error 206` 命令行长度上限且越出本套件目录）；请始终使用 `-p stage-e-road-e001-upgrade-gate` 精确限定本门禁包。

| 验证门禁 | 精确复现命令 | 实测结果 |
|---|---|---|
| Rust 格式检查 | `cargo fmt --manifest-path tests/stage_e_road_e001_upgrade_gate/Cargo.toml -p stage-e-road-e001-upgrade-gate -- --check` | 退出码 `0` |
| Rust Clippy 严格门禁（`all` + `pedantic` + `#![forbid(unsafe_code)]`） | `cargo clippy --manifest-path tests/stage_e_road_e001_upgrade_gate/Cargo.toml --all-targets -- -D warnings` | 退出码 `0`（`0` 错误、`0` 警告） |
| Rust 跨版本升级全链路 + `H7` 对抗集成测试 | `cargo test --manifest-path tests/stage_e_road_e001_upgrade_gate/Cargo.toml` | 退出码 `0`：**`6/6` 集成测试通过、`0` 失败**（耗时 `7.23s`） |
| Python 矩阵运行器自检套件（含临时副本 1 字节包篡改与封存记录篡改负向检测） | `python scripts/run_road_e001_upgrade_matrix.py --self-test` | 退出码 `0`：**`5/5` 单元测试通过**（耗时 `0.017s`） |
| Python 零重编译全矩阵机器可读回执门禁 | `python scripts/run_road_e001_upgrade_matrix.py --json` | 退出码 `0`：`status = "PASS"`，`sealed_baseline_file_count = 16`，`frozen_nlos_pkg_count = 8`，`zero_recompile_verified = true`，`drifted_files = []`，`manifest_purity_violations = []`，`passed_tests = 6` |
| Python 零重编译全矩阵人类可读摘要门禁 | `python scripts/run_road_e001_upgrade_matrix.py` | 退出码 `0`：`8/8` 冻结 `.nlos-pkg` 全部 `[UNCHANGED]` |

6 个 Rust 集成测试明细（`tests/road_e001_zero_recompile_upgrade_gate.rs`）：

1. `step_a_frozen_n1_baseline_seals_certifications_and_cas_promotions_verify`：`ok`
2. `step_c_kabi_internal_refactor_insulates_frozen_n1_sabi_wire_responses`：`ok`
3. `step_c_compat_ver_002_rejects_unknown_major_critical_ext_and_tombstones_while_preserving_unknown_fields`：`ok`
4. `h7_adversarial_tampered_frozen_package_and_forged_certification_fail_closed`：`ok`
5. `h7_ecosystem_manifests_remain_free_of_private_kernel_crate_dependencies`：`ok`
6. `full_road_e001_n1_to_n_zero_recompile_cross_version_upgrade_chain`（串联 Step A -> Step B -> Step C -> Step D `D.1..D.7` + `pre == post` 16 文件零漂移断言）：`ok`

---

## 6. 未覆盖项与诚实边界（`Claim <= Evidence`）

1. **`H7` 确定性本地门禁边界（非 `H8` / 非 `H10`）**：本门禁在同一宿主机的隔离文件树与真实 SQLite / OCI CAS / Ed25519 / Protobuf / Wasm 子进程权威实现上机械证明 `N-1 -> N` 原地升级与对抗性负向拦截，属于 `H7` 级本地 conformance 与对抗性集成证据；不声称 `H8` 真实多节点生产机群灰度发布/金丝雀遥测，也不声称 `H10` 形式化定理证明。
2. **Windows 宿主下的符号链接逃逸子项边界**：在 Windows 非特权用户态下创建文件系统符号链接受操作系统管理员权限/开发者模式限制，因此 Wasm preopen 目录内符号链接外指逃逸的操作系统级实跑由 [E-SANDBOX-001](e-sandbox-001-cross-platform-wasm-process-isolation.md)（`crates/nlos-wasm-sandbox` 中 `#[cfg(unix)]` 用例与 Linux/macOS CI 矩阵）专项守护；本门禁在步骤 `D.5` 实跑覆盖真实子进程 `--self-audit-env`（`0` 环境变量继承）、`..` 父目录穿越、POSIX/Windows 双形态绝对路径逃逸、`sock_open` 网络导入拒绝、CPU fuel 耗尽与线性内存上限陷阱。
3. **跨平台 LF 封存摘要归一化说明**：`W80-E6B` 的 `.store-seal.txt` 封存摘要基于 Git 规范 LF 字节计算；当 Windows 工作树启用 `core.autocrlf = true` 时，已解包源目录 `package/` 中的文本文件会被检出为 `CRLF`（而二进制 `.nlos-pkg` 不受影响）。步骤 A 在重放 OCI CAS 槽位摘要与 `LoadedCertifyPackage` 清单摘要时，将 `package/` 源树复制至临时治理目录做 `CRLF -> LF` 归一化后再哈希对账，绝不改写仓库工作树任何字节，从而保证 Windows、Linux、macOS 三平台重放同一份 `.store-seal.txt` 摘要逐位一致。
4. **当前状态为车道 `CANDIDATE`**：本证据在本机 Windows 独立 worktree（`feat/w81-e7a-roade001-gate`）完成实跑验证；跨平台三矩阵（Windows / Linux / macOS）CI 守护与阶段进展文档 `stage-e-progress.md` 的 canonical 状态晋升由下游合流车道 `W81-INT` 与远端 CI 统一完成。
