# E-MIGRATE-001：声明式契约静态检查器、跨纪元 SABI 差异工具与 `PKG-UPDATE-001` 三态迁移回滚沙箱证据（`W78-E3`）

- **Evidence ID**: `e-migrate-001-static-checker-and-rollbacks`
- **Wave / Lane**: `W78 / W78-E3-MIGRATE-KIT`（里程碑 `M-E2` 右车道，工作包 `E3`）
- **日期**: `2026-10-31`
- **Assurance Level**: `H4`（纯确定性本地宿主单元测试、真实权威 SQLite 存储集成验证与 CLI 端到端沙箱演练；不声称 `H7` 生产环境证明）
- **状态**: `CANDIDATE`（本车道门禁全部通过，待 `W78-INT` 与左车道 `W78-E2-COMPAT-SHIM` 合流复验后晋升 canonical `DONE`）
- **权威规范与治理依据**:
  - [06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §9（`COMPAT-VER-001`、`COMPAT-VER-002`、`COMPAT-DEPRECATE-001`）、§15.1（`DRV-ABI-001`、`DRV-LIFE-001`、`DRV-CRED-001`、`DRV-HEALTH-001`）、§23.1–§23.2（`PKG-MULTI-001`、`PKG-IMM-001`、`PKG-SEED-001`、`PKG-UPDATE-001`）
  - [ADR-0027 Stage E SABI 长期向后兼容与商店治理](../../management/adrs/0027-stage-e-sabi-long-term-compatibility-and-store-governance.md) §2–§4
  - [stage-e-progress.md](../../management/stage-e-progress.md)（工作包 `E3`、里程碑 `M-E2`、风险项 `RISK-E-05`）
  - [e-sabi-001-version-matrix-and-deprecation.md](e-sabi-001-version-matrix-and-deprecation.md)（`W77` 冻结基线与弃用清单契约）

---

## 1. 声明式 SABI / Driver / Package 契约静态检查器与弃用升级处方（`nlos-migrate check`）

在 [crates/nlos-migrate](../../../crates/nlos-migrate/src/lib.rs) 中以 100% 安全 Rust（`#![forbid(unsafe_code)]`、零新增外部第三方依赖）实现完整静态审计管线（[manifest_audit.rs](../../../crates/nlos-migrate/src/manifest_audit.rs)、[scanner.rs](../../../crates/nlos-migrate/src/scanner.rs)、[prescription.rs](../../../crates/nlos-migrate/src/prescription.rs)、[report.rs](../../../crates/nlos-migrate/src/report.rs) 与 [main.rs](../../../crates/nlos-migrate/src/main.rs)）：

1. **多组件包清单无损兼容与阶段 E 声明式扩展（`[PKG-MULTI-001]`）**：
   - 100% 兼容既有行格式 `package.manifest`（`package-id`、`version`、`entry`、`task`），对开箱即用示范包 [package/package.manifest](../../../examples/starter-assistant-app/package/package.manifest)（`v1.0.0`）与 [package-v1.1.0/package.manifest](../../../examples/starter-assistant-app/package-v1.1.0/package.manifest)（`v1.1.0`）实测均为零告警 `PASS`（退出码 `0`）。
   - 支持阶段 E 多角色声明：`component = <name> <executable|driver|ui-surface|background-service|seed-template|data> <path>`、`target-sabi-epoch = <u32>[.<u32>]`、`driver-abi-epoch = <u32>`、`effect = <component_or_op> <READ_ONLY|REVERSIBLE|COMPENSATABLE|IRREVERSIBLE> [reconcile=<same-idempotency-key|none>]`、`sabi-use = <channel> <symbol> [field=<u32>]`、`sabi-ext = <channel> <critical|noncritical> <id>` 及 `migration-mode = <BACKWARD_COMPATIBLE_WINDOW|SNAPSHOT_ASSISTED_ROLLBACK|IRREVERSIBLE>`。
2. **驱动副作用与 `UNCERTAIN` 崩溃对账门禁（`[DRV-LIFE-001]`）**：
   - 任何声明为 `driver` 的组件若缺失 `EffectDescriptor`（`effect = ...`），或对变更型副作用（`REVERSIBLE` / `COMPENSATABLE` / `IRREVERSIBLE`）省略对账策略、声明 `reconcile=none`，或声明禁止的换新键盲目重试（如 `reconcile=fresh-key-blind-retry`），一律输出 `BLOCKER` 级 `DRV-LIFE-001` 违规（退出码 `2`）。
3. **只读种子与不可变包树扫描器（`[PKG-SEED-001]`、`[PKG-IMM-001]`、`[DRV-CRED-001]`）**：
   - 对全部 `seed-template` / `data` 载荷及包树执行越界路径拦截、可变数据库后缀（`.sqlite`、`.db`、`.sqlite-wal`、`.db-wal`、`.db-shm`、`-journal`）以及二进制 `SQLite format 3\0` / WAL 魔数头扫描，拦截把可变运行库混入不可变包的违规打包。
   - 对全包树载荷逐行扫描硬编码明文凭证（`sk-` 词法边界前缀、`Bearer ` 令牌、PEM 私钥头 `-----BEGIN ... PRIVATE KEY-----` 及 `api_key=` / `secret_key=` 字面量赋值），输出 `PKG-IMM-001` `BLOCKER` 诊断并在纯文本与 RFC 8259 JSON 报告中强制脱敏（零原始密钥字节回显）。
4. **`[COMPAT-DEPRECATE-001]` 生命周期裁决与 `UpgradePrescription` 升级处方**：
   - 针对内置权威清单 [deprecation-manifest.yaml](../../../schema/nlos/sabi/v1/deprecation-manifest.yaml) 或 `--manifest-yaml` 外部演化清单及 `--host-epoch`：
     - `ACTIVE` 且处于 `[N-2..=N]` 窗口：输出 `PASS`（`N` 标记 `NATIVE_CURRENT_EPOCH`，`N-1` / `N-2` 标记 `OUT_OF_CORE_COMPAT_SHIM`）。
     - `DEPRECATED`：不阻断构建（`PASS_WITH_WARNINGS`，退出码 `0`），精确计算剩余宽限纪元 `earliest_sunset_epoch - host_epoch`，生成包含 `replacement_symbol`、`migration_rule_id` 与替换补丁摘要的 `UpgradePrescription`，并可无损桥接为 `nlos_service_directory::DeprecationWarningReceipt`。
     - `SUNSET`、`RESERVED` 墓碑槽位、未注册通道、超出 `N / N-1 / N-2`（如 `<= N-3` 或 `> N`）或未知 `critical` 扩展（`[COMPAT-VER-002]`）：输出 `BLOCKER`（`BLOCKED`，退出码 `2`）并附带迁移规则处方。

---

## 2. 跨纪元 SABI 契约差异检查工具（`tools/sabi-compat-diff`）

新增纯 Python 3 标准库工具 [sabi_compat_diff.py](../../../tools/sabi-compat-diff/sabi_compat_diff.py) 与完整测试套件 [test_sabi_compat_diff.py](../../../tools/sabi-compat-diff/test_sabi_compat_diff.py)：

| 变更分类 | 触发条件 | CLI 退出码影响 |
| :--- | :--- | :--- |
| `UNCHANGED` | 冻结通道版本、墓碑集合、条目状态及 `.proto` 消息/枚举线布局完全一致 | 允许（`0`） |
| `ADDITIVE_SAFE` | 新增 `frozen: true` 通道、通道版本前向递增、新增未占用墓碑槽位的 `ACTIVE` 元素或 `.proto` 可选字段 | 允许（`0`） |
| `DEPRECATION_NOTICE` | `ACTIVE -> DEPRECATED` 合法跃迁且满足 `earliest_sunset_epoch >= deprecated_in_epoch + 2` 及非空 `replacement` / `migration_rule_id` | 允许（`0`） |
| `SUNSET_RETIREMENT` | 完成至少 2 纪元缓冲期后的 `DEPRECATED -> SUNSET` 日落，或已 `SUNSET` 条目归档入永久 `reserved_*` 墓碑（含 `.proto` `reserved`） | 允许（`0`） |
| `BREAKING_VIOLATION` | 未经 `SUNSET` 物理删除历史通道/条目、`frozen: true -> false` 回退、主/次版本倒退、缩短缓冲期、提前日落、删除或重用 `reserved_field_numbers` / `reserved_symbols` 墓碑、篡改既有 `.proto` 字段类型或编号 | 阻断（`1`） |

---

## 3. `[PKG-UPDATE-001]` 三态数据容器迁移与原子回滚沙箱（`nlos-migrate drill`）

在 [sandbox.rs](../../../crates/nlos-migrate/src/sandbox.rs) 与 [migration_sandbox.rs](../../../crates/nlos-migrate/tests/migration_sandbox.rs) 中直接驱动真实 `IdentityAuthority`、`ArtifactStore`、`ApplicationAuthority` 与真实 `UserDataContainer` 目录，闭合三种法定迁移模式的端到端契约：

| 迁移模式 (`MigrationMode`) | 正常升级路径 | 注入故障 / 安全门禁拒绝验证 |
| :--- | :--- | :--- |
| `BackwardCompatibleWindow` | 验证 `CompatibilityWindow::SameMajor` 读写兼容窗口，健康探针通过后单事务 CAS 推进至 `from_generation + 1` 并持久化 `InstallationReceipt` | 注入启动后健康检查失败（`--inject-health-fail` / `DRV-HEALTH-001`）：执行 `ApplicationAuthority::rollback_package_migration` 生成 `MigrationRollbackReceipt`，`from_generation` 保持激活，`UserDataContainer` 树摘要与升级前一致（零容器污染） |
| `SnapshotAssistedRollback` | Step 1 前捕获内容寻址快照 `ContainerSnapshotReceipt`（记录全树 SHA-256 摘要与不可变副本）；全部步骤及健康探针通过后原子激活至 `Generation = 2` | 注入步骤中途崩溃（`--inject-step-crash`，写入半截 `.tmp` 与损坏 v2 状态）或步骤后健康检查失败：协同提交 `MigrationRollbackReceipt` 与快照字节级恢复，机械校验恢复后全目录 SHA-256 树摘要与升级前 100% 字节级全等，且残留 `.tmp` / `.partial` 文件数为 `0` |
| `Irreversible` (`RISK-E-05`) | 升级前落盘并校验 `PreUpgradeBackupReceipt` SHA-256 归档摘要 + 通过 `IdentityAuthority` Ed25519 验签绑定 `(package_id, from_manifest_digest, target_manifest_digest, backup_digest, "IRREVERSIBLE")` 的 `Z4TrustedUiConsentReceipt` 后推进至 `Generation = 2` | 伪装为可逆模式的破坏性重写（`DisguisedIrreversibleMigration`）、缺失或篡改磁盘备份、缺失 `Z4` 确认收据、跨包/跨版本重放 `Z4` 签名、伪造签名或 `IdentityAuthority` 已吊销操作员密钥：全部在触碰 `ApplicationAuthority` 与 `UserDataContainer` 之前 fail-closed 拦截（退出码 `3`，`0` 条持久化迁移行） |

---

## 4. 验证门禁结果与复现命令

### 4.1 本车道独占写集清单

1. [crates/nlos-migrate/src/lib.rs](../../../crates/nlos-migrate/src/lib.rs)、[main.rs](../../../crates/nlos-migrate/src/main.rs)、[json.rs](../../../crates/nlos-migrate/src/json.rs)、[manifest_audit.rs](../../../crates/nlos-migrate/src/manifest_audit.rs)、[scanner.rs](../../../crates/nlos-migrate/src/scanner.rs)、[prescription.rs](../../../crates/nlos-migrate/src/prescription.rs)、[report.rs](../../../crates/nlos-migrate/src/report.rs)、[sandbox.rs](../../../crates/nlos-migrate/src/sandbox.rs)
2. [crates/nlos-migrate/tests/static_checker.rs](../../../crates/nlos-migrate/tests/static_checker.rs)、[crates/nlos-migrate/tests/migration_sandbox.rs](../../../crates/nlos-migrate/tests/migration_sandbox.rs)
3. [tools/sabi-compat-diff/sabi_compat_diff.py](../../../tools/sabi-compat-diff/sabi_compat_diff.py)、[tools/sabi-compat-diff/test_sabi_compat_diff.py](../../../tools/sabi-compat-diff/test_sabi_compat_diff.py)、[tools/sabi-compat-diff/README.md](../../../tools/sabi-compat-diff/README.md)
4. `docs/evidence/stage-e/e-migrate-001-static-checker-and-rollbacks.md`（本文件）

> 注：根 `Cargo.toml`、`Cargo.lock` 与 `crates/nlos-migrate/Cargo.toml` 在本车道保持零改动。

### 4.2 门禁执行结果

1. **格式检查门禁**：
   `cargo fmt -p nlos-migrate -- --check` -> 退出码 `0`。
2. **Clippy 严格门禁**：
   `cargo clippy --offline -p nlos-migrate --all-targets -- -D warnings` -> 退出码 `0`（`#![forbid(unsafe_code)]`、`clippy::all`、`clippy::pedantic` 零警告）。
3. **Rust 单元与集成测试门禁**：
   `cargo test --offline -p nlos-migrate` -> 退出码 `0`：
   - 库内单元测试（`json` + `manifest_audit`）：`5` 项通过，`0` 失败；
   - `tests/static_checker.rs`：`7` 项通过，`0` 失败；
   - `tests/migration_sandbox.rs`：`6` 项通过，`0` 失败；
   - 合计 `18` 项 Rust 测试全部通过。
4. **示范包端到端静态检查门禁**：
   - `cargo run --offline -p nlos-migrate -- check examples/starter-assistant-app/package --json` -> `verdict: "PASS"`, `exit_code: 0`；
   - `cargo run --offline -p nlos-migrate -- check examples/starter-assistant-app/package-v1.1.0 --json` -> `verdict: "PASS"`, `exit_code: 0`。
5. **沙箱演练 CLI 门禁**：
   - `cargo run --offline -p nlos-migrate -- drill --mode snapshot --inject-step-crash --json` -> `status: "PASS"`, `outcome: "ROLLED_BACK_ATOMICALLY"`, `byte_identical_on_rollback: true`, `residual_temp_files: 0`, 退出码 `0`；
   - `cargo run --offline -p nlos-migrate -- drill --mode irreversible --omit-z4-consent --json` -> `status: "BLOCKED"`, `error_code: "MISSING_Z4_TRUSTED_UI_CONSENT"`, 退出码 `3`。
6. **跨纪元 SABI 差异检查器门禁**：
   - `python -m unittest tools/sabi-compat-diff/test_sabi_compat_diff.py` -> `8` 项测试全部通过（退出码 `0`）；
   - `python tools/sabi-compat-diff/sabi_compat_diff.py schema/nlos/sabi/v1/deprecation-manifest.yaml schema/nlos/sabi/v1/deprecation-manifest.yaml --baseline-proto-dir schema/nlos/sabi/v1 --candidate-proto-dir schema/nlos/sabi/v1 --json` -> `compatible: true`, `BREAKING_VIOLATION: 0`, 退出码 `0`。

---

## 5. 诚实非声明边界（Honest Non-Claims）

1. **保证等级边界（`H4 != H7`）**：本文档所有证据均来自单机确定性宿主测试与临时目录真实 SQLite 权威沙箱（`H4`），不构成跨物理主机崩溃掉电或大规模在线应用商店分发的 `H7` 生产环境证明。
2. **核外兼容垫片边界（左车道 `W78-E2` 独立交付）**：本车道（`W78-E3`）负责应用与驱动包升级前的静态契约审计、弃用处方生成、跨纪元契约差异拦截与 `[PKG-UPDATE-001]` 数据容器迁移回滚沙箱；运行期执行 `N-1 / N-2` 旧版线帧转换的核外垫片 `crates/nlos-compat-shim` 由并行左车道 `W78-E2-COMPAT-SHIM` 交付，须经 `W78-INT` 合流复验后共同闭合里程碑 `M-E2`。
3. **Trusted UI 图形交互壳层边界**：`Irreversible` 模式在沙箱与库层已强制接入 `IdentityAuthority` Ed25519 真实签名校验绑定 `Z4TrustedUiConsentReceipt`（闭合 `RISK-E-05` 自动化迁移无备份/无确认安全门禁），但面向终端用户的窗口级图形化确认弹窗交互与商店前端编排归属后续工作包 `E4`–`E5`。
