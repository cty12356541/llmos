# E-MIGRATE-001：`nlos-migrate` 声明式契约静态检查、`sabi-compat-diff` 门禁与 `PKG-UPDATE-001` 三态迁移回滚演练证据（`W78-E3`）

- **Evidence ID**: `e-migrate-001`
- **Wave / Lane**: `W78 / W78-E3-MIGRATE-KIT`（工作包 `E3` `E-MIGRATE-KIT`，里程碑 `M-E2` 候选右半片）
- **日期**: `2026-10-31`
- **Assurance Level**: `H4`（单机确定性宿主单元测试、真实 SQLite 状态根与 `nlos-store-fault` VFS I/O 故障注入演练及 CLI 子进程集成验证；不声称 `H7` 跨版本供应链对抗或多节点生产集群证明）
- **状态**: `CANDIDATE`（待 `W78-INT` 与并行左半片 `W78-E2-COMPAT-SHIM` 屏障合流复验通过后晋升 canonical `DONE`）
- **权威规范与治理依据**:
  - [06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §9（`COMPAT-VER-001`、`COMPAT-VER-002`、`COMPAT-DEPRECATE-001`）、§14（`PLAN-DEPENDENCY-001`）、§15.1（`DRV-LIFE-001`、`DRV-HEALTH-001`）、§23.1–§23.2（`PKG-UPDATE-001`、`PKG-SEED-001`、`PKG-MULTI-001`）、§28.5（阶段 E 路线图 `E3` / `M-E2`）
  - [ADR-0027 Stage E SABI 长期向后兼容与商店治理](../../management/adrs/0027-stage-e-sabi-long-term-compatibility-and-store-governance.md) §2.1、§2.2（第 3 款 `crates/nlos-migrate`）、§5.2（`RISK-E-05`）、§6.1
  - [ADR-0014 SABI Schema 通道 v1-beta 冻结规则](../../management/adrs/0014-schema-channel-freeze-v1-beta.md)
  - [ADR-0013 跨权威 verify-then-commit 与有界收敛契约](../../management/adrs/0013-cross-authority-verify-then-commit-contract.md)
  - [stage-e-progress.md](../../management/stage-e-progress.md) §E.2.2（工作包 `E3`）、§E.3.2（`W78-E3-MIGRATE-KIT` 独占写集）、§E.5.1（`M-E2`）、§E.5.2（`RISK-E-05`）
  - 前置基线证据：[E-SABI-001](./e-sabi-001-version-matrix-and-deprecation.md)、[E-SABI-002](./e-sabi-002-five-lang-golden-corpus.md)

---

## 1. 声明式契约静态检查器与 `UpgradePrescription` 确定性升级处方编译器（`crates/nlos-migrate`）

在 [lib.rs](../../../crates/nlos-migrate/src/lib.rs) 下交付纯安全 Rust（`#![forbid(unsafe_code)]`，零新增第三方依赖，`Cargo.toml` 与根 `Cargo.lock` 零改动）的离线静态检查与处方编译流水线，包含清单加载器 [manifest.rs](../../../crates/nlos-migrate/src/manifest.rs)、契约审计器 [scanner.rs](../../../crates/nlos-migrate/src/scanner.rs)、升级处方编译器 [prescription.rs](../../../crates/nlos-migrate/src/prescription.rs)、确定性 JSON 编码器 [json.rs](../../../crates/nlos-migrate/src/json.rs) 及命令行入口 [main.rs](../../../crates/nlos-migrate/src/main.rs)：

### 1.1 双形态包目标检查（源码目录与已构建 `.nlospkg` 归档）

`inspect_package_target` 支持对两类生态交付物执行无副作用离线检查：

1. **第三方包源码目录（`TargetSourceKind::SourceDirectory`）**：解析 `package.manifest`（标准 `llmos.package` 键值段：基础身份、`target_sabi_epoch` / `target_sabi_minor` / `driver_abi_epoch`、多组件声明 `executable_component` / `driver_component` / `ui_surface_component` / `background_service_component` / 只读 `seed_template`）及可选的 `migration.manifest`（显式声明 SABI 通道与字段/符号引用、`critical_extensions` / `non_critical_extensions`、`EffectDescriptor` 四分类、依赖选择器及 `migration.mode` / `migration.step` SQL 序列）。
2. **已签名二进制归档包（`TargetSourceKind::PackageArchive`，`.nlospkg` / `.nlos-pkg`）**：复用 `nlos_artifact::{decode_package_file, check_package_file}` 执行 `PKG-CONF-001..005` 格式、载荷 SHA-256 摘要、禁止可变记忆/私钥烘焙与 Ed25519 签名一致性核验，并提取归档内嵌 `migration.manifest`（若携带）进入统一契约审计管线；任何篡改字节或损坏归档均在入口处 fail-closed 返回退出码 `2`（`E_CORRUPT_PACKAGE_OR_MANIFEST`）。

### 1.2 SABI 纪元矩阵、弃用窗口、扩展协商与依赖锁定静态裁决

`scan_path` 与 `scan_inspected_target` 联合 `nlos_schema::VersionCompatibilityPolicy` 与 `DeprecationManifest`（默认嵌入权威 [deprecation-manifest.yaml](../../../schema/nlos/sabi/v1/deprecation-manifest.yaml)，亦支持 `--manifest-yaml` 注入未来演进纪元清单做前瞻预检）执行六维静态检查：

| 审计维度 | 规范条款 | 确定性诊断码与阻断策略 |
| :--- | :--- | :--- |
| SABI 与 Driver 纪元窗口 | `COMPAT-VER-001`、`DRV-NEGO-001` | `target_sabi_epoch == host_epoch` 判为 `NativeCurrentEpoch`；落在 `[host_epoch - 2, host_epoch - 1]` 判为 `OutOfCoreCompatShim` 并发出非阻断 `W_EPOCH_REQUIRES_COMPAT_SHIM`；`> host_epoch` 或 `< host_epoch - 2` 分别以 `E_UNSUPPORTED_SABI_EPOCH` / `E_SUNSET_EPOCH`（以及驱动侧 `E_DRIVER_ABI_EPOCH_OUT_OF_WINDOW`）阻断准入 |
| 扩展槽协商 | `COMPAT-VER-002` | 未知 `non_critical_extensions` 产生 `I_UNKNOWN_NON_CRITICAL_EXTENSION`（保留原字节语义放行）；`0` 号或宿主未知的 `critical_extensions` 以 `E_UNKNOWN_CRITICAL_EXTENSION` fail-closed 阻断；列表长度 `> 32` 报 `E_TOO_MANY_EXTENSIONS` |
| 弃用与墓碑符号扫描 | `COMPAT-DEPRECATE-001` | 命中 `DEPRECATED` 缓冲期内字段/符号产生结构化 `DeprecationDiagnostic` 与 `W_DEPRECATED_SYMBOL`（精确携带 `deprecated_in_epoch`、`earliest_sunset_epoch`、`remaining_epochs_until_sunset`、`replacement`）；命中 `SUNSET` 或通道级 `reserved_field_numbers` / `reserved_symbols` 以 `E_SYMBOL_SUNSET_OR_RESERVED` 硬阻断 |
| 驱动效应四分类完整性 | `DRV-LIFE-001` | 核验 `READ_ONLY`、`REVERSIBLE`、`COMPENSATABLE`、`IRREVERSIBLE` 声明；凡声明 `COMPENSATABLE` 但缺失非空补偿处理器符号者以 `E_COMPENSATABLE_MISSING_HANDLER` 阻断 |
| 依赖选择器不可变锁定 | `PLAN-DEPENDENCY-001`、`RISK-E-05` | 扫描 `dependency` 选择器引用，拦截 `latest`、`head`、`main`、`canary`、`*`、`^`、`~` 等浮动标签或缺失 `sha256:` / `gen:` 不可变摘要锁定的依赖项（`E_FLOATING_DEPENDENCY_SELECTOR`） |
| 静态迁移模式与 SQL 危害匹配 | `PKG-UPDATE-001`、`RISK-E-05` | `classify_migration_step_hazard` 静态识别破坏性 DDL/DML（`DROP TABLE/COLUMN`、`TRUNCATE`、`ALTER TABLE ... RENAME`、无条件 `DELETE` 等）：若声明为 `BACKWARD_COMPATIBLE_WINDOW` 或 `SNAPSHOT_ASSISTED_ROLLBACK` 却夹带破坏性语句，立即以 `E_DISGUISED_IRREVERSIBLE_MIGRATION` 阻断；`BACKWARD_COMPATIBLE_WINDOW` 夹带非双读兼容变更报 `E_WINDOW_SCHEMA_INCOMPATIBLE` |

### 1.3 `UpgradePrescription` 确定性升级处方编译

[prescription.rs](../../../crates/nlos-migrate/src/prescription.rs) 将 `ScanReport` 编译为零墙钟时间戳、键序严格固定的 `UpgradePrescription`（支持人类可读文本与 `--json` 规范化输出），将目标包归类为三态 `ExecutionPosture` 之一并生成按 `(priority, code, target)` 确定性排序的 `RemediationAction` 列表：

1. `NativeCurrentReady`：零阻断项且目标纪元等于宿主当前纪元，可直接原生直通运行。
2. `TransientCompatShimEligible`：零阻断项，处于 `N-1` / `N-2` 滚动兼容窗口或引用 `DEPRECATED` 缓冲期符号，当前可经核外 `nlos-compat-shim` 零重编译兼容运行，同时附带按日落剩余周期排序的替换指引。
3. `SourceMigrationRequired`：存在至少一项 `ERROR` 阻断诊断（如引用 `SUNSET`/`RESERVED` 符号、未知关键扩展、浮动依赖标签或伪装不可逆迁移），必须在重发包前按处方逐条修复。

---

## 2. SABI `.proto` 与弃用清单演进静态门禁（`tools/sabi-compat-diff`）

在 [sabi_compat_diff.py](../../../tools/sabi-compat-diff/sabi_compat_diff.py) 与配套说明 [README.md](../../../tools/sabi-compat-diff/README.md) 中实现零第三方 Python 依赖（纯 Python 3 标准库）的跨版本 `.proto` AST 与 `deprecation-manifest.yaml` 双向一致性校验器：

1. **受控演进正向放行规则**：
   - 允许纯增量新增可选字段、新增 `message`、新增 `enum` 枚举值、新增 `service` / `rpc`；
   - 允许契约元素由 `ACTIVE -> DEPRECATED` 跃迁（强制校验 `deprecated_in_epoch`、`earliest_sunset_epoch >= deprecated_in_epoch + min_compatibility_window_epochs`、`min_compatibility_window_epochs >= 2`、非空 `replacement` 与 `reason`）；
   - 允许已完整跨越双纪元弃用缓冲窗口的 `DEPRECATED` 条目在 `current_epoch >= earliest_sunset_epoch` 时跃迁至 `SUNSET`，前提是在对应 `.proto` 父消息中同时写入数值 `reserved <N>;` 与名称 `reserved "<field_name>";` 永久墓碑。
2. **破坏性演进 Fail-Closed 拦截规则**：
   - 直接物理删除现有 `message`、`enum`、`service`、`rpc` 或未同步登记 `SUNSET` + 双重 `reserved` 墓碑即删除字段/枚举值（`E_PROTO_FIELD_REMOVED_WITHOUT_SUNSET` / `E_PROTO_FIELD_MISSING_NUMERIC_RESERVED` / `E_PROTO_FIELD_MISSING_NAME_RESERVED`）；
   - 原号篡改既有字段类型、基数或字段名（`E_PROTO_FIELD_TYPE_CHANGED` / `E_PROTO_FIELD_NAME_CHANGED`），或修改 `rpc` 请求/响应类型与流模式；
   - 删除既有 `.proto` `reserved` 范围/名称，或将已入墓碑的字段号/符号重新分配给新 `ACTIVE` / `DEPRECATED` 定义（`E_PROTO_RESERVED_RANGE_REMOVED` / `E_MANIFEST_RESERVED_REUSE`）；
   - 非法跳过弃用期直接 `ACTIVE -> SUNSET`（`E_ILLEGAL_STATE_SKIP_ACTIVE_TO_SUNSET`）、弃用窗口不足 2 个纪元（`E_INSUFFICIENT_DEPRECATION_WINDOW`）或未到期提前日落（`E_PREMATURE_SUNSET`）。
3. **当前仓库自检闭环**：`python3 tools/sabi-compat-diff/sabi_compat_diff.py --self-check` 对 `schema/nlos/sabi/v1/` 全部 `9` 个 `.proto` 文件（共解析 `167` 个 `message`、`9` 个冻结通道，`current_epoch = 1`）与 `deprecation-manifest.yaml` 完成基线自检，零错误通过（退出码 `0`）。

---

## 3. `[PKG-UPDATE-001]` 三态数据容器迁移与故障回滚沙箱演练器

在 [drill.rs](../../../crates/nlos-migrate/src/drill.rs) 与确定性测试包夹具 [drill_fixtures.rs](../../../crates/nlos-migrate/src/drill_fixtures.rs) 中，直接驱动真实 SQLite 状态根上的 `IdentityAuthority`、`ArtifactStore`、`ApplicationAuthority`（复用 `begin_package_update`、`record_migration_step`、`complete_package_update`、`abort_package_update_and_rollback`）及应用私有 `UserData` SQLite 容器（`userdata/app.sqlite` + `userdata/preferences.kv`），完整覆盖 `[PKG-UPDATE-001]` 三种数据迁移模式与 `RISK-E-05` 硬护栏：

| 迁移模式 (`DataMigrationMode`) | 正向收敛演练验证 | 故障注入、原子回滚与安全阻断验证 |
| :--- | :--- | :--- |
| 1. `BackwardCompatibleWindow`（双读双写兼容窗口） | `v1`（`Generation(1)`）写入基线用户记录后，纯增量 DDL（如带默认值的 `ALTER TABLE ... ADD COLUMN` 或 `CREATE INDEX`）平滑完成升级，新旧读取器双端可读并晋升至 `Generation(2)`（`ActivatedNewGeneration`） | 注入破坏双读窗口的非兼容或破坏性 SQL 时，在触碰 `UserData` 与权威更新事务前即被静态/准入栅栏拦截 |
| 2. `SnapshotAssistedRollback`（内容寻址快照辅助回滚） | 升级启动前由 `create_userdata_snapshot` 对 `UserData` 目录生成逐文件 SHA-256 清单与完整字节快照；支持中途进程崩溃重启（`--inject-crash-after-step K`），重启后先前已持久化步骤幂等返回 `RecordMigrationStepDecision::Replayed`，剩余步骤续跑收敛至 `Generation(2)` | (a) 注入升级后 `DRV-HEALTH-001` 健康检查失败（`--inject-health-failure`）：自动调用 `abort_package_update_and_rollback` + `restore_userdata_snapshot`，断言活跃代际回退为 `Generation(1)`、回滚计数递增且 `UserData` 全树 SHA-256 与升级前快照 **100% 字节级一致**（`RolledBackToPreviousGeneration`）；<br>(b) 挂载 `nlos-store-fault` 自定义 SQLite VFS，在多步迁移中途精确触发底层 `SQLITE_IOERR` 写入故障：捕获存储层 I/O 异常后自动回滚控制面事务并从内容寻址快照无损恢复 `UserData` SHA-256，消除半迁移脏态 |
| 3. `Irreversible`（不可逆迁移硬门禁，`RISK-E-05`） | 仅在同时具备经 SHA-256 摘要校验通过的 `PreUpgradeBackupReceipt`（升级前独立预备份已完成且匹配当前 `UserData` 根哈希）与密码学绑定的 `Z4IrreversibleConsentReceipt`（覆盖 `application_id`、`from_package_digest`、`to_package_digest`、`backup_sha256` 与 `migration_plan_sha256`）时，放行不可逆表重构并晋升至 `Generation(2)` | (a) **物理禁止伪装**：将 `DROP COLUMN` / `DROP TABLE` / `TRUNCATE` / `RENAME` 等破坏性 SQL 伪报为 `BackwardCompatibleWindow` 或 `SnapshotAssistedRollback` 时，一律返回 `DrillError::DisguisedIrreversibleMigration`（CLI 退出码 `4`）；<br>(b) **缺省拒绝**：缺失 `PreUpgradeBackupReceipt`、预备份哈希失配、缺失 `Z4IrreversibleConsentReceipt` 或伪造 `Z4` 确认令牌时，一律在执行任何 SQL 之前以退出码 `4` fail-closed 拦截，`UserData` 保持零改动 |

### 3.1 `nlos-migrate` CLI 子命令与确定性退出码契约

在 [main.rs](../../../crates/nlos-migrate/src/main.rs) 中提供 `scan`、`prescribe`、`drill` 三个子命令（全部支持 `--json` 无墙钟确定性输出），退出码严格钉死为 `0..=5`：

| 退出码 | 常量标识 | 触发条件 |
| :--- | :--- | :--- |
| `0` | `EXIT_PASS` | `scan` / `prescribe` 零阻断错误通过（含纯 `ACTIVE` 或 `N-1`/`N-2` 兼容窗口告警态）、正向迁移演练成功、崩溃重放收敛成功，或显式带 `--expect-rollback` 的回滚演练完成 `Generation(1)` 与 `UserData` SHA-256 完整恢复 |
| `1` | `EXIT_USAGE` | 未知子命令、缺失必填标志或非法命令行参数 |
| `2` | `EXIT_CORRUPT_INPUT` | `package.manifest`、`migration.manifest`、`deprecation-manifest.yaml` 语法/契约损坏，或 `.nlospkg` 归档签名/摘要校验失败（`PKG-CONF-*`） |
| `3` | `EXIT_CONTRACT_BLOCK` | 目标纪元越界或已日落（`E_SUNSET_EPOCH` / `E_UNSUPPORTED_SABI_EPOCH`）、引用 `SUNSET`/`RESERVED` 符号、携带未知 `critical_extensions`、缺失 `COMPENSATABLE` 补偿器、存在浮动依赖标签（`latest` 等）或双读窗口 Schema 不兼容 |
| `4` | `EXIT_IRREVERSIBLE_BLOCK` | 将破坏性迁移伪报为可自动回滚模式（`E_DISGUISED_IRREVERSIBLE_MIGRATION`），或 `IRREVERSIBLE` 迁移缺失/伪造预备份回执 `PreUpgradeBackupReceipt` 与 `Z4IrreversibleConsentReceipt`（`RISK-E-05`） |
| `5` | `EXIT_DRILL_FAILURE` | 未声明 `--expect-rollback` 时遭遇 `DRV-HEALTH-001` 升级后健康检查失败、窗口双读校验失败或回滚快照恢复异常 |

---

## 4. 验证门禁结果与诚实非声明边界（Honest Non-Claims）

### 4.1 本车道严格独占写集清单

1. `crates/nlos-migrate/src/{lib.rs,json.rs,manifest.rs,scanner.rs,prescription.rs,drill_fixtures.rs,drill.rs,main.rs}` 及集成测试 `crates/nlos-migrate/tests/{static_scanner_and_prescription.rs,three_mode_drills_and_rollbacks.rs,cli_subprocess.rs}`（注：`crates/nlos-migrate/Cargo.toml` 与根 `Cargo.toml` / `Cargo.lock` 保持零修改）
2. `tools/sabi-compat-diff/{sabi_compat_diff.py,test_sabi_compat_diff.py,README.md}`
3. `docs/evidence/stage-e/e-migrate-001-static-checker-and-rollbacks.md`（本文档）

### 4.2 五道强制验证门禁执行记录

1. **全仓 Rust 格式门禁**：
   `cargo fmt --all -- --check` -> 退出码 `0`（零差异）。
2. **`nlos-migrate` 严格 Clippy 门禁**：
   `cargo clippy --locked -p nlos-migrate --all-targets --all-features -- -D warnings` -> 退出码 `0`（在 `#![forbid(unsafe_code)]`、`clippy::all`、`clippy::pedantic` 下零警告通过）。
3. **`nlos-migrate` 单元、VFS 故障注入与 CLI 子进程测试门禁**：
   `cargo test --locked -p nlos-migrate` -> 退出码 `0`，共 **`18/18` 项测试全部通过**（`0` 失败）：
   - 库内单测（`drill_fixtures::tests::canonical_fixture_ed25519_signatures_match_seed`）：`1` 项通过，钉死种子 `[0x57; 32]` 派生的确定性 Ed25519 公钥与 `v1`/`v2` 包签名；
   - [static_scanner_and_prescription.rs](../../../crates/nlos-migrate/tests/static_scanner_and_prescription.rs)：`6` 项通过（覆盖 `examples/sample-app` 原生就绪扫描、`N-1` 兼容垫片与 `DEPRECATED` 缓冲期升级处方生成、`SUNSET`/`RESERVED`/浮动依赖阻断、未知 `critical_extensions` 拦截与宿主显式支持放行、伪装不可逆 SQL 静态检出、以及 `.nlospkg` 二进制归档正负向验签扫描）；
   - [three_mode_drills_and_rollbacks.rs](../../../crates/nlos-migrate/tests/three_mode_drills_and_rollbacks.rs)：`6` 项通过（覆盖模式 1 双读窗口升级与破坏性 DDL 阻断、模式 2 中途崩溃幂等续跑收敛、模式 2 `DRV-HEALTH-001` 失败回滚与 `UserData` SHA-256 字节级一致校验、模式 2 `nlos-store-fault` SQLite VFS `SQLITE_IOERR` 写入故障注入回滚恢复、模式 3 伪装不可逆 SQL 物理拦截、以及模式 3 `PreUpgradeBackupReceipt` + `Z4IrreversibleConsentReceipt` 双门禁放行/拒绝矩阵）；
   - [cli_subprocess.rs](../../../crates/nlos-migrate/tests/cli_subprocess.rs)：`5` 项通过（真实子进程拉起编译后 `nlos-migrate` 二进制，逐一核验退出码 `0`、`1`、`2`、`3`、`4`、`5` 与 `--json` 双次运行字节完全一致）。
4. **`sabi-compat-diff` 权威 Schema 基线自检门禁**：
   `python3 tools/sabi-compat-diff/sabi_compat_diff.py --self-check` -> 退出码 `0`（`proto_files=9->9 messages=167->167 channels=9->9 epoch=1->1 errors=0 infos=0`）。
5. **`sabi-compat-diff` 正负向单元测试门禁**：
   `python3 -m unittest discover -s tools/sabi-compat-diff -p "test_*.py" -v` -> 退出码 `0`（[test_sabi_compat_diff.py](../../../tools/sabi-compat-diff/test_sabi_compat_diff.py) 共 **`14/14` 项测试全部通过**，覆盖增量演进、`ACTIVE -> DEPRECATED -> SUNSET` 合法流转、缺失数值/名称 `reserved` 拦截、字段改型/改名拦截、跳过弃用期与窗口不足拦截、墓碑重用拦截及 CLI 子进程退出码/JSON 确定性）。

### 4.3 诚实非声明边界（Honest Non-Claims，`Claim <= Evidence`）

1. **保障等级上限声明（`H4 != H7`）**：本文档记录的全部静态检查与迁移回滚演练均在单机确定性宿主环境与 `nlos-store-fault` VFS 故障注入层（`H4`）完成，不冒充真实物理断电台架（`U-4` / `Layer-3+`）或多节点跨版本生产集群浸泡证据，也不提前声称阶段 E `H7` 退出终验已达成。
2. **并行车道与里程碑 `M-E2` 合流边界**：运行期执行 `N-1` / `N-2` 历史 SABI 与 `DriverProvider` 线帧转换的核外无特权垫片 `crates/nlos-compat-shim` 归属同波并行左半片车道 `W78-E2-COMPAT-SHIM`；本文档在单车道内保持 `CANDIDATE` 状态，里程碑 `M-E2` 闭合、`ADR-0027` 晋升 `ACCEPTED` 以及共享治理索引（`stage-e-progress.md`、`evidence-index.yaml`）同步统一由波末集成器 `W78-INT` 在双车道屏障合流复验后串行执行。
3. **`Z4IrreversibleConsentReceipt` 验证范围边界**：`nlos-migrate` 沙箱演练器在控制面与数据迁移栅栏层验证了 `Z4IrreversibleConsentReceipt` 对 `(application_id, from_package_digest, to_package_digest, backup_sha256, migration_plan_sha256)` 的绑定摘要校验与缺省拒绝语义（`H4`），未在本 CLI 工具包中声称驱动真实桌面合成器 GUI 窗口渲染；桌面商店与升级确认视图联调归属后续波次 `W80`（`W80-E6C-DESKTOP-STORE-UI`）。
4. **后续商店认证与升级总门禁边界**：将 `nlos-migrate` 三态迁移回滚门接入四门自动化上架认证流水线并签发 `StoreCertificationReceipt` 归属 `W79`（`E5` `crates/nlos-certify`）；跨主系统 `N-1 -> N` 零重编译全链路升级门禁 `[ROAD-E-001]` 与 `RISK-E-05` 台账正式退役归属 `W81`（`E7`）。
