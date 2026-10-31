# E-CERTIFY-001：应用商店四门自动化一致性认证流水线、纯 Rust 依赖许可证供应链审计与签名认证收据签发器证据（`W79-E5B`）

- **Evidence ID**: `e-certify-001-automated-conformance-and-uninstall-gate`
- **Wave / Lane**: `W79 / W79-E5B-CERTIFY-PIPELINE`（里程碑 `M-E3` 右车道，工作包 `E5` 右半）
- **日期**: `2026-10-31`
- **Assurance Level**: `H4`（纯确定性本地宿主集成测试、真实权威 SQLite 存储沙箱演练、真实工作区 `Cargo.lock` + `package-lock.json` 供应链审计与 CLI 端到端闭环验证；不声称 `H7` 生产环境证明）
- **状态**: `VERIFIED`（经 `W79-INT` 三车道合流与跨 crate 端到端集成门禁 [e2e_trust_store_certify_pipeline.rs](../../../crates/nlos-certify/tests/e2e_trust_store_certify_pipeline.rs) 联合复验通过，晋升 canonical `DONE`）
- **权威规范与治理依据**:
  - [06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §2.6（`TCB-EGRESS-001`）、§5（`SABI-CONFORM-001`）、§9（`COMPAT-VER-001`、`COMPAT-VER-002`、`COMPAT-DEPRECATE-001`）、§11.4（`KABI-UNCERTAIN-001`）、§13（`CAP-REVOKE-001`）、§15.1–§15.2（`DRV-LIFE-001`、`DRV-HEALTH-001`、`DRV-NEGO-001`）、§21.2（`DUI-CHOOSER-001`、`DUI-ASSOC-001`）、§23.1–§23.3（`PKG-IMM-001`、`PKG-SEED-001`、`PKG-UPDATE-001`、`PKG-UNINSTALL-001`）
  - [ADR-0027 Stage E SABI 长期向后兼容与商店治理](../../management/adrs/0027-stage-e-sabi-long-term-compatibility-and-store-governance.md) §3–§4（四门自动化认证门禁与零残留卸载审计）
  - [stage-e-progress.md](../../management/stage-e-progress.md)（工作包 `E5`、里程碑 `M-E3`、继承跨阶段阻塞项 `RISK-B-09`）
  - [e-sabi-001-version-matrix-and-deprecation.md](e-sabi-001-version-matrix-and-deprecation.md) 与 [e-migrate-001-static-checker-and-rollbacks.md](e-migrate-001-static-checker-and-rollbacks.md)（`W77`–`W78` 冻结 SABI 目录与迁移沙箱基线）

---

## 1. 四门自动化一致性认证流水线架构（`crates/nlos-certify`）

在 [crates/nlos-certify/src/lib.rs](../../../crates/nlos-certify/src/lib.rs) 中以 100% 安全 Rust（`#![forbid(unsafe_code)]`、`clippy::all` + `clippy::pedantic` 零警告、零新增外部第三方依赖）实现应用商店上架四门自动化认证流水线与 CLI 二进制 [main.rs](../../../crates/nlos-certify/src/main.rs)：

| 认证门 (`GateId`) | 核心模块 | 强制规范条款 | 阻断条件与证据输出 |
| :--- | :--- | :--- | :--- |
| **Gate 1：协议与副作用完备性审计** | [gate1_protocol.rs](../../../crates/nlos-certify/src/gate1_protocol.rs) | `SABI-CONFORM-001`、`COMPAT-VER-001`、`COMPAT-VER-002`、`COMPAT-DEPRECATE-001`、`DRV-NEGO-001`、`DRV-LIFE-001`、`KABI-UNCERTAIN-001` | 校验 `SurfaceProfile`（`APPLICATION_PLATFORM` vs `DESKTOP`）与冻结 SABI v1 服务目录边界；在 `APPLICATION_PLATFORM` 下拒绝 `DESKTOP` 专属服务及 `ui-surface` 组件；强制 `SEMANTIC_SETTLEMENT` 依赖 `RESOURCE_ESCROW` 能力协商；复用 `nlos_migrate` 校验 `[N-2..=N]` 三纪元支持窗口、弃用清单 [deprecation-manifest.yaml](../../../schema/nlos/sabi/v1/deprecation-manifest.yaml) 及未知 `critical` 扩展拒绝；审计全部可执行/驱动/后台服务组件的 `EffectDescriptor`（`READ_ONLY` / `REVERSIBLE` / `COMPENSATABLE` / `IRREVERSIBLE`）、128-bit `IdempotencyKey` 透传声明及 `UNCERTAIN` 同幂等键对账契约。对既有示范包 [package/package.manifest](../../../examples/starter-assistant-app/package/package.manifest)（`v1.0.0`）与 [package-v1.1.0/package.manifest](../../../examples/starter-assistant-app/package-v1.1.0/package.manifest)（`v1.1.0`）保持 100% 无损兼容 |
| **Gate 2：最小权限、包纯度与依赖许可证供应链门禁** | [gate2_supply_chain.rs](../../../crates/nlos-certify/src/gate2_supply_chain.rs)、[supply_chain_audit.rs](../../../crates/nlos-certify/src/supply_chain_audit.rs) | `TCB-EGRESS-001`、`DUI-CHOOSER-001`、`DUI-ASSOC-001`、`PKG-IMM-001`、`PKG-SEED-001`、`RISK-B-09` | 拦截 `platform-core` / TCB 核心组件违规请求 `net.egress` 或硬编码外部 URL/套接字；拦截未声明 `system-chooser` 门控的原始宿主文件系统绝对路径访问；拦截静默篡改默认 URI/MIME/文件类型关联而不保留用户显式授权的要求；复用 `nlos_migrate::scan_package_payloads` 扫描可变 SQLite/WAL 二进制头与硬编码密钥并强制脱敏回显；同步对含 `Cargo.toml` 或 `package.json` 的包/工作区执行纯 Rust 锁文件完整性与 SPDX 许可证合规审计 |
| **Gate 3：事务升级与故障注入原子回滚沙箱** | [gate3_update_rollback.rs](../../../crates/nlos-certify/src/gate3_update_rollback.rs) | `PKG-UPDATE-001`、`DRV-HEALTH-001` | 在真实 `IdentityAuthority`、`ArtifactStore`、`ApplicationAuthority` 与真实 `UserDataContainer` 临时沙箱中执行三步机械演练：（1）`v1 -> v2` 健康探针通过后单事务原子切换至 `Generation = 2`；（2）注入 `HealthCheckFailed` 探针失败：验证基线代数/清单零漂移且 `UserDataContainer` 全目录 SHA-256 树摘要字节级全等；（3）注入 `MidStepFault` 迁移中途崩溃：验证原子回滚后 `UserDataContainer` 全目录 SHA-256 树摘要字节级全等且残留 `.tmp` / `.partial` 文件数为 `0`；对未获豁免的 `IRREVERSIBLE` 自动化商店包默认阻断 |
| **Gate 4：零残留卸载与即时能力吊销审计沙箱** | [gate4_uninstall_audit.rs](../../../crates/nlos-certify/src/gate4_uninstall_audit.rs) | `PKG-UNINSTALL-001`、`CAP-REVOKE-001` | 在真实 `IdentityAuthority`、`CapabilityAuthority`、`ArtifactStore`、`SqliteTaskAuthority` 与 `ApplicationAuthority` 沙箱中验证：（1）活跃后台任务门控：`uninstall_application_with_task_activity_gate` 在任务运行期返回 `ApplicationActiveTasksRunning` fail-closed 拒绝，调用 `cancel_task` 收敛后卸载放行；（2）卸载后五项不变式机械核验：孤儿活跃后台任务数为 `0`、应用获授 `Capability` 句柄即时吊销（复验返回 `CapabilityRevoked`）、私有缓存目录残留文件数为 `0`、共享导出 `Artifact` 100% 完好可读、默认 100% 保留 `UserDataContainer`（仅在附带经 `IdentityAuthority` Ed25519 验签通过的 `Z4` 清除确认收据时才允许销毁用户数据容器） |

---

## 2. 纯 Rust 依赖许可证与锁文件供应链审计器（闭合 `RISK-B-09`）

在 [supply_chain_audit.rs](../../../crates/nlos-certify/src/supply_chain_audit.rs) 中实现零外部工具依赖的纯 Rust 供应链审计入口 `audit_dependency_supply_chain(root, &SupplyChainPolicy) -> Result<SupplyChainAuditReport, CertifyError>`：

1. **锁文件存在性与版本钉死校验（`SUPPLY-LOCK-001`、`SUPPLY-PIN-001`）**：
   - 同时覆盖 Rust 工作区（`Cargo.toml` + `Cargo.lock`）与桌面前端工作区（`package.json` + `package-lock.json`）。
   - 任何缺失对应锁文件、声明通配符浮动版本（`"*"` / `"latest"`）或引用未钉死 commit SHA 的 Git 远程依赖，一律产生 `BLOCKER` 级违规。
2. **加密哈希完整性校验（`SUPPLY-HASH-001`）**：
   - 强制校验 `Cargo.lock` 中每个外部注册表包的 64 位十六进制 SHA-256 `checksum` 字段，以及 `package-lock.json`（v2/v3 `packages` 映射）中每个非本地第三方包的 SRI `integrity`（`sha512-...` / `sha256-...`）摘要。
3. **SPDX 布尔表达式求值与传染性许可证拦截（`SUPPLY-SPDX-001`）**：
   - 内置递归下降 SPDX 表达式求值器，原生支持 `AND`、`OR`、历史 `/` 析取符、括号优先级与 `WITH <exception>`（如 `Apache-2.0 WITH LLVM-exception`）：
     - `A OR B`：任一分支落入白名单（`MIT`、`Apache-2.0`、`BSD-2-Clause`、`BSD-3-Clause`、`ISC`、`Unicode-3.0`、`Unicode-DFS-2016`、`Zlib`、`CC0-1.0`、`0BSD`、`MPL-2.0`、`BSL-1.0`、`Unlicense`）即放行；
     - `A AND B`：全部分支均须合规；任何必选分支命中强传染性黑名单（`GPL-*`、`AGPL-*`、`SSPL-*`、`EUPL-*`）或未知/空许可证声明，一律 fail-closed 拦截。
4. **安全公告阻断清单（`SUPPLY-ADV-001`）**：
   - 支持结构化 `AdvisoryDenyEntry`（生态、包名、精确版本或 `*`、公告编号与说明），命中即输出 `BLOCKER`。
5. **真实仓库工作区实测结果**：
   - 对本仓库根工作区（`.`）执行 `nlos-certify supply-chain --root .`：审计 `166` 个 Cargo 锁文件包与 `119` 个 npm 锁文件包，`0` 项 `BLOCKER`、`0` 项 `WARNING`，退出码 `0`（`PASS`）。
   - 对桌面子工作区（`desktop`）执行 `nlos-certify supply-chain --root desktop`：审计 `78` 个 npm 锁文件包，`0` 项 `BLOCKER`、`0` 项 `WARNING`，退出码 `0`（`PASS`）。

---

## 3. 签名 `StoreCertificationReceipt` 签发器与跨车道契约对齐

在 [receipt.rs](../../../crates/nlos-certify/src/receipt.rs) 与 [model.rs](../../../crates/nlos-certify/src/model.rs) 中实现与左车道 [nlos-publisher-trust](../../../crates/nlos-publisher-trust/src/lib.rs) 及中车道 [nlos-store-registry](../../../crates/nlos-store-registry/src/lib.rs) 契约严丝合缝对齐的收据签发与纯函数验签闭环：

1. **发布者信任与 `ApplicationId` 归属前置门控**：
   - 签发前通过 `&dyn PublisherTrustVerifier` 依次调用 `verify_publisher_signature`（校验包清单签名与当前 `revocation_epoch` 未吊销状态）和 `verify_or_bind_application_owner`（防止跨发布者冒用或劫持既有 `ApplicationId`）。任一校验失败均拒绝签发收据并记录结构化拒绝原因。
2. **`4/4 PASS` 全门通过不变式**：
   - 仅当 `Gate 1`、`Gate 2`、`Gate 3`、`Gate 4` 四门摘要全部为 `GateStatus::Pass`（`blocker_count == 0`）且四门各恰好出现一次时，才允许构造并签发 `StoreCertificationReceipt`。
3. **域分离签名与 `SigningKeyCustodyVault` 密钥托管集成**：
   - 收据签名载荷采用显式域分离前缀 `NLOS-STORE-CERTIFICATION-RECEIPT-V1` 绑定 `receipt_id`、`publisher_id`、`trust_tier`、`application_id`、`package_id`、`package_version`、`manifest_digest`、`surface_profile`、`target_sabi_epoch`、四门 `evidence_digest`、`revocation_epoch`、`issued_at_ms`、`expires_at_ms`、`certifier_key_id` 与 `certifier_public_key` 的 SHA-256 摘要。
   - 支持通过 `nlos_capability::SigningKeyCustodyVault`（`CustodyTier::OperatorFileSealed` 封存 KEK 托管签名）或确定性 Ed25519 认证密钥签名；配套纯函数 `verify_store_certification_receipt` 独立校验域分离 Ed25519 签名、有效期窗口、四门 `PASS` 状态与最小吊销纪元水位，并支持 RFC 8259 JSON 无损往返序列化。

---

## 4. 验证门禁结果与复现命令

### 4.1 本车道独占写集清单

1. [crates/nlos-certify/src/lib.rs](../../../crates/nlos-certify/src/lib.rs)、[main.rs](../../../crates/nlos-certify/src/main.rs)、[model.rs](../../../crates/nlos-certify/src/model.rs)、[json.rs](../../../crates/nlos-certify/src/json.rs)、[gate1_protocol.rs](../../../crates/nlos-certify/src/gate1_protocol.rs)、[gate2_supply_chain.rs](../../../crates/nlos-certify/src/gate2_supply_chain.rs)、[supply_chain_audit.rs](../../../crates/nlos-certify/src/supply_chain_audit.rs)、[gate3_update_rollback.rs](../../../crates/nlos-certify/src/gate3_update_rollback.rs)、[gate4_uninstall_audit.rs](../../../crates/nlos-certify/src/gate4_uninstall_audit.rs)、[receipt.rs](../../../crates/nlos-certify/src/receipt.rs)
2. [crates/nlos-certify/tests/certify_pipeline_gate.rs](../../../crates/nlos-certify/tests/certify_pipeline_gate.rs)
3. `docs/evidence/stage-e/e-certify-001-automated-conformance-and-uninstall-gate.md`（本文件）

> 注：根 `Cargo.toml`、`Cargo.lock`、`crates/nlos-certify/Cargo.toml` 以及其他 crate 在本车道保持零改动。

### 4.2 门禁执行结果

1. **格式检查门禁**：
   `cargo fmt -p nlos-certify -- --check` -> 退出码 `0`。
2. **Clippy 严格门禁**：
   `cargo clippy --offline -p nlos-certify -p nlos-publisher-trust -p nlos-store-registry --all-targets -- -D warnings` -> 退出码 `0`（`#![forbid(unsafe_code)]`、`clippy::all`、`clippy::pedantic` 零警告）。
3. **Rust 集成测试套件门禁**：
   `cargo test --offline -p nlos-certify` -> 退出码 `0`（[certify_pipeline_gate.rs](../../../crates/nlos-certify/tests/certify_pipeline_gate.rs) 共 `10` 项端到端测试全部通过，`0` 失败）：
   - `starter_assistant_app_v1_to_v1_1_passes_all_four_gates_and_issues_signed_receipt`：真实示范包 `v1.0.0 -> v1.1.0` 四门全通 + `SigningKeyCustodyVault` 签发收据 + JSON 往返验签；
   - `gate1_enforces_surface_profile_catalog_and_settlement_escrow_negotiation`：`APPLICATION_PLATFORM` vs `DESKTOP` 服务边界与 `SEMANTIC_SETTLEMENT -> RESOURCE_ESCROW` 依赖门控；
   - `gate1_rejects_epoch_window_unknown_critical_extension_missing_effects_and_bad_reconcile`：超出 `[N-2..=N]` 纪元、未知 `critical` 扩展、缺失 `EffectDescriptor` 与违规 `UNCERTAIN` 对账拦截；
   - `gate2_enforces_egress_chooser_association_sqlite_purity_and_secret_redaction`：`TCB-EGRESS-001`、`DUI-CHOOSER-001`、`DUI-ASSOC-001`、可变 SQLite 混入拦截与硬编码密钥脱敏；
   - `supply_chain_audit_passes_on_real_repository_workspace_cargo_and_npm_lockfiles`：真实仓库根工作区与 `desktop/` 锁文件及 SPDX 白名单零阻塞通过；
   - `supply_chain_audit_blocks_copyleft_missing_checksum_floating_versions_and_advisories`：`GPL-3.0-only`、`MIT AND AGPL-3.0-or-later`、缺失校验和、`"*"` 浮动版本、未钉死 Git 依赖与安全公告拦截，以及 `(MIT OR GPL-3.0-only)`、`Apache-2.0 WITH LLVM-exception` 合法放行；
   - `gate3_rejects_irreversible_mode_and_catches_simulated_rollback_drift`：`IRREVERSIBLE` 阻断与回滚残余漂移检测；
   - `gate4_catches_unrevoked_capabilities_residual_cache_and_unconsented_user_data_purge`：活跃任务卸载门控、能力未吊销检测、私有缓存残留检测、未经 `Z4` 确认清除用户数据检测，以及附带真实 `IdentityAuthority` 签名 `Z4` 收据时的合法清除；
   - `receipt_issuance_enforces_publisher_ownership_all_gates_pass_and_tamper_resistance`：跨发布者 `ApplicationId` 冒用拦截、任一门失败拒签、字段篡改与过期纪元验签拒绝；
   - `cli_binary_certify_and_supply_chain_subcommands_return_deterministic_exit_codes`：`nlos-certify` CLI 二进制确定性退出码（`0` 通过 / `2` 阻断）。
4. **CLI 端到端实测门禁**：
   - `cargo run --offline -p nlos-certify -- certify --package-dir examples/starter-assistant-app/package --upgrade-dir examples/starter-assistant-app/package-v1.1.0 --profile application-platform --format json` -> `verdict: "CERTIFIED"`, 四门全部 `PASS` 并输出签名 `StoreCertificationReceipt`，退出码 `0`；
   - `cargo run --offline -p nlos-certify -- supply-chain --root . --format text` -> `PASS`（`audited_cargo_packages: 166`, `audited_npm_packages: 119`, `blockers: 0, warnings: 0`），退出码 `0`；
   - `cargo run --offline -p nlos-certify -- supply-chain --root desktop --format text` -> `PASS`（`audited_npm_packages: 78`, `blockers: 0, warnings: 0`），退出码 `0`。
5. **`W79-INT` 跨 crate 端到端全链路集成与常设 Python 锁文件供应链门禁**：
   - `cargo test --offline -p nlos-certify --test e2e_trust_store_certify_pipeline` -> 退出码 `0`（[e2e_trust_store_certify_pipeline.rs](../../../crates/nlos-certify/tests/e2e_trust_store_certify_pipeline.rs) `1/1` 通过，连同 `certify_pipeline_gate.rs` 合计 `nlos-certify` `11/11` 通过：串联 `nlos-publisher-trust` 三级 PKI / 密钥轮换 / 单调 `RevocationEpoch` / `ApplicationId` 防劫持、`nlos-certify` 四门认证收据签发、`nlos-store-registry` OCI CAS 5 槽位复合包上架 / `Candidate -> Stable` 晋升 / 跨版本权限提权 `requires_tui_reconsent` / `EntitlementToken` 幂等核销、以及 `nlos-plan::ecosystem_selector` 7 类跨生态选择器句柄解析 + G3 `WaitingAuthorization -> Materializing` 条件物化门与 Level 3 `QuarantineInstallation` 级联能力吊销及应用隔离）；
   - `python -m unittest discover -s tests/conformance/schema -p "test_*.py"` -> 退出码 `0`（新增纯标准库常设门禁 [test_lockfile_supply_chain_gate.py](../../../tests/conformance/schema/test_lockfile_supply_chain_gate.py) `3/3` 通过，全套 schema conformance `20/20` 通过，无需修改 `.github/workflows/**` 即被跨平台 CI 自动发现执行）。

---

## 5. 诚实非声明边界（Honest Non-Claims）

1. **保证等级边界（`H4 != H7`）**：本文档所有证据均来自本地确定性宿主测试、临时目录真实 SQLite 权威沙箱与仓库锁文件离线扫描（`H4`），不声称构成多租户公网商店运营、硬件 HSM 物理防篡改或跨主机断电故障下的 `H7` 生产环境证明。
2. **并行车道职责与 `W79-INT` 合流边界**：
   - 发布者根/中继/叶证书链管理、开发者门户注册流、CRL/纪元吊销状态机及完整 `PublisherTrustVerifier` 生产实现归属并行左车道 `W79-E4-PUBLISHER-TRUST`（[crates/nlos-publisher-trust](../../../crates/nlos-publisher-trust/src/lib.rs)）；
   - 应用商店索引目录存储、包版本上架/下架查询与客户端安装前收据强制核验归属并行中车道 `W79-E5A-STORE-REGISTRY`（[crates/nlos-store-registry](../../../crates/nlos-store-registry/src/lib.rs)）；
   - 本车道（`W79-E5B-CERTIFY-PIPELINE`）通过稳定的 `&dyn PublisherTrustVerifier` trait 与自包含域分离签名的 `StoreCertificationReceipt` 提供上游认证裁决与收据签发能力，并已在 `W79-INT` 合流节点经 [e2e_trust_store_certify_pipeline.rs](../../../crates/nlos-certify/tests/e2e_trust_store_certify_pipeline.rs) 与左、中两车道联合复验通过，共同闭合里程碑 `M-E3`。
3. **离线 SPDX 元数据解析范围**：`supply_chain_audit.rs` 基于仓库内确定性清单（`Cargo.toml`、`Cargo.lock`、`package.json`、`package-lock.json` 与内置生态已核验 SPDX 映射）执行离线零网络审计，不依赖外部联网查询 crates.io 或 npm Registry API；新增第三方依赖若未在锁文件或离线许可证目录中携带已核验 SPDX 表达式，将按 fail-closed 原则输出 `SUPPLY-SPDX-001` `BLOCKER`，要求显式补充许可证审计条目后方可放行。
