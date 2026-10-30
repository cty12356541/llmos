# E-SABI-001：9/9 SABI v1 通道全冻结、`N / N-1 / N-2` 版本兼容矩阵与弃用清单遥测证据（`W77-E1A`）

- **Evidence ID**: `e-sabi-001`
- **Wave / Lane**: `W77 / W77-E1A-SABI-REGISTRY`（里程碑 `M-E1` 候选左半片）
- **日期**: `2026-10-30`
- **Assurance Level**: `H4`（纯确定性本地宿主单测与跨模块契约集成验证；不声称 `H7` 生产环境证明）
- **状态**: `VERIFIED`（经 `W77-INT` 与右半片五语言黄金矩阵合流复验通过，晋升 canonical `DONE`）
- **权威规范与治理依据**:
  - [06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §9（`COMPAT-VER-001`、`COMPAT-VER-002`、`COMPAT-DEPRECATE-001`）、§28（阶段 E 路线图 `M-E1`、`E1`、`E2`）
  - [ADR-0027 Stage E SABI 长期向后兼容与商店治理](../../management/adrs/0027-stage-e-sabi-long-term-compatibility-and-store-governance.md) §2.1、§2.2、§3.1、§3.2、§3.3
  - [ADR-0014 SABI Schema 通道 v1-beta 冻结规则](../../management/adrs/0014-schema-channel-freeze-v1-beta.md)
  - [stage-e-progress.md](../../management/stage-e-progress.md) §E.2.1（决策点 `E1`）、§E.2.2（工作包 `E1` / 里程碑 `M-E1`）

---

## 1. `PrincipalHandshake` 对端证明槽位审计与 `9/9` SABI 通道全冻结

### 1.1 `PrincipalHandshake`（第 7 通道）对端证明可扩展性审计

依据 `ADR-0027` §3.1 对 `schema/nlos/sabi/v1/principal_handshake.proto` 与 `crates/nlos-schema/src/lib.rs` 完成线级契约审计，无需对现有 `.proto` 或黄金向量做任何破坏性改动即可将第 7 通道晋升为冻结态：

1. **消息字段槽位开放性**：
   - `PrincipalHandshakeChallenge` 当前仅锁定 `schema = 1`、`nonce = 2`（32 字节 Ed25519 随机挑战值）；字段编号 `>= 3` 全部空闲，可供后续以纯增量（additive）方式挂载对端 Cell 证明挑战参数。
   - `PrincipalHandshakeAttestation` 当前锁定 `schema = 1`、`principal_id = 2`（16 字节）、`nonce = 3`（32 字节）、`channel_binding = 4`（`1..=256` 字节连接上下文绑定）、`signature = 5`（64 字节 Ed25519 签名）；字段编号 `>= 6` 全部空闲，可供后续纯增量挂载硬件证明报告或跨域身份凭据。
2. **扩展协商槽位与容量边界**：
   - `SchemaIdentity` 原生携带 `critical_extension_ids = 4` 与 `non_critical_extension_ids = 5`，在 `nlos-schema` 中新增硬边界常量 `MAX_EXTENSION_IDS = 32` 并在解码/编码两侧强制校验，超限即 fail-closed 拒绝（`CompatibilityError::TooManyExtensionIds`）。
3. **帧体积余量审计**：
   - 权威黄金向量 `schema/golden/nlos.sabi.PrincipalHandshake-v1.hex` 实测线长度为 `185` 字节（370 个十六进制字符），距离通道载荷上限 `MAX_PRINCIPAL_HANDSHAKE_PAYLOAD_BYTES = 4096` 保留 `3,911` 字节纯增量扩展余量。

### 1.2 `schema_registry()` 全部 `9/9` 通道冻结基线表

在 [lib.rs](../../../crates/nlos-schema/src/lib.rs) 中将 `SABI_PRINCIPAL_HANDSHAKE_V1.frozen` 由 `false` 翻转为 `true`，达成 `9/9` SABI v1 通道全冻结：

| 序号 | 通道标识 (`SchemaDescriptor.name`) | 当前版本 (`major.minor`) | 载荷字节上限 | `frozen` 状态 |
| :--- | :--- | :--- | :--- | :--- |
| 1 | `nlos.sabi.Envelope` | `1.1` | `1,048,576 B` (`1 MiB`) | `true` |
| 2 | `nlos.sabi.ServiceDirectory` | `1.0` | `65,536 B` (`64 KiB`) | `true` |
| 3 | `nlos.sabi.OperationControl` | `1.0` | `65,536 B` (`64 KiB`) | `true` |
| 4 | `nlos.sabi.SystemControl` | `1.7` | `65,536 B` (`64 KiB`) | `true` |
| 5 | `nlos.sabi.TakeoverControl` | `1.0` | `65,536 B` (`64 KiB`) | `true` |
| 6 | `nlos.sabi.WaitControl` | `1.0` | `65,536 B` (`64 KiB`) | `true` |
| 7 | `nlos.sabi.PrincipalHandshake` | `1.0` | `4,096 B` (`4 KiB`) | `true`（`W77` 新增冻结） |
| 8 | `nlos.sabi.DriverProvider` | `1.0` | `16,384 B` (`16 KiB`) | `true` |
| 9 | `nlos.sabi.ChannelControl` | `1.0` | `65,536 B` (`64 KiB`) | `true` |

---

## 2. `N / N-1 / N-2` 版本兼容矩阵与 `[COMPAT-VER-002]` 扩展协商裁决

在 [version_matrix.rs](../../../crates/nlos-schema/src/version_matrix.rs) 中实现法定滚动兼容窗口常量 `MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS: u32 = 2`、`VersionCompatibilityPolicy`、`VersionRoute`、`VersionNegotiationOutcome` 及 `compatibility_error_to_sabi_failure`：

| 输入条件 | 裁决结果 / 错误变体 | 路由或线级失败映射 |
| :--- | :--- | :--- |
| `client_major == host_epoch` (`N`) 且任意 `minor` | 通过 (`Ok(VersionNegotiationOutcome)`) | `VersionRoute::NativeCurrentEpoch`（零垫片开销直通核心权威） |
| `host_epoch - client_major` 属于 `1..=max_backward_epochs` (`N-1` / `N-2`) | 通过 (`Ok(VersionNegotiationOutcome)`) | `VersionRoute::OutOfCoreCompatShim { client_epoch, host_epoch, epoch_distance }` |
| `client_major == 0` 或 `host_epoch == 0` | `Err(CompatibilityError::ZeroMajorVersion)` | `SabiErrorCode::AbiVersion` + `RetryDirective::DoNotRetry` |
| `client_major > host_epoch` (`> N` 未来大版本) | `Err(CompatibilityError::UnsupportedMajor)` | `SabiErrorCode::AbiVersion` + `RetryDirective::DoNotRetry` |
| `host_epoch - client_major > max_backward_epochs` (`<= N-3` 已日落版本) | `Err(CompatibilityError::SunsetEpoch)` | `SabiErrorCode::AbiVersion` + `RetryDirective::DoNotRetry` |
| `max_backward_epochs < 2` | `Err(CompatibilityError::InvalidCompatibilityWindow)` | `SabiErrorCode::AbiVersion` + `RetryDirective::DoNotRetry` |
| 未注册通道名称 | `Err(CompatibilityError::UnknownSchema)` | `SabiErrorCode::AbiVersion` + `RetryDirective::DoNotRetry` |
| `critical_extension_ids` 含 `0` 或接收方未知 ID (`[COMPAT-VER-002]`) | `Err(CompatibilityError::UnsupportedCriticalExtension)` | `SabiErrorCode::AbiVersion` + `RetryDirective::DoNotRetry` |
| 关键或非关键扩展列表长度 `> MAX_EXTENSION_IDS` (`32`) | `Err(CompatibilityError::TooManyExtensionIds)` | `SabiErrorCode::AbiVersion` + `RetryDirective::DoNotRetry` |
| 携带未知 `non_critical_extension_ids` 及未知 Protobuf 字段 tag | 通过 (`adjudicate_envelope_wire` / `encode_adjudicated_envelope`) | 原样保留 `retained_non_critical_extensions` 与原始 `wire_bytes()` 无损转发 |

---

## 3. 机器可读弃用清单契约与纯安全 Rust 状态机校验器

新增权威机器可读清单 [deprecation-manifest.yaml](../../../schema/nlos/sabi/v1/deprecation-manifest.yaml) 与零外部第三方依赖、100% `#![forbid(unsafe_code)]` 的解析校验模块 [deprecation.rs](../../../crates/nlos-schema/src/deprecation.rs)（`parse_deprecation_manifest`、`canonical_deprecation_manifest`）：

1. **三态单向生命周期状态机（`ACTIVE -> DEPRECATED -> SUNSET`）**：
   - `ACTIVE`：活跃契约元素，禁止提前填写弃用或日落纪元元数据（`InvalidStateMetadata`）。
   - `DEPRECATED`：必须完整声明 `deprecated_in_epoch`、`earliest_sunset_epoch`、非空 `replacement` 与非空 `reason`（缺失报 `MissingDeprecationMetadata`），且强制 `earliest_sunset_epoch >= deprecated_in_epoch + min_compatibility_window_epochs`（`min_compatibility_window_epochs >= 2`，不足报 `InsufficientDeprecationWindow`）。
   - `SUNSET`：除满足完整弃用缓冲期外，必须声明 `sunset_in_epoch` 且同时满足 `sunset_in_epoch >= earliest_sunset_epoch` 与 `current_epoch >= sunset_in_epoch`（违背报 `PrematureSunset`）。
2. **永久墓碑禁重用（Permanent `RESERVED`）**：
   - 双遍顺序无关扫描确保任何 `ACTIVE` 或 `DEPRECATED` 条目一旦重用通道级 `reserved_field_numbers`、`reserved_symbols` 或任何 `SUNSET` 条目的 `(channel, parent_type, field_number)` / `(channel, symbol)`，立即以 `DeprecationManifestError::ReservedSlotReuse` 拒绝。
3. **注册表 `9/9` 双向一致性锁**：
   - `canonical_deprecation_manifest()` 在编译期嵌入 `schema/nlos/sabi/v1/deprecation-manifest.yaml`，并与 `schema_registry()` 逐通道核对通道总数（`9`）、名称、`major`、`minor` 与 `frozen == true`；同时确认新增 YAML 文件不干扰 `schema/` 目录下的 `buf lint` 规则。

---

## 4. `ServiceDirectory` 协商侧弃用审计与 `DeprecationWarningReceipt` 有界遥测

在保持 [lib.rs](../../../crates/nlos-service-directory/src/lib.rs) 既有 `SnapshotDirectory::{new, resolve, negotiate}` 公开签名与语义 100% 向后兼容的前提下，新增策略治理协商与有界遥测能力：

1. **有界遥测缓冲**：定义 `MAX_DEPRECATION_WARNINGS: usize = 256`、`DeprecationWarningReceipt`、`SnapshotDirectory::deprecation_warnings(&self)` 与 `SnapshotDirectory::drain_deprecation_warnings(&mut self)`；超出 `256` 条上限时按确定性 FIFO 淘汰最早告警，杜绝内存无界增长。
2. **线级与结构化策略入口**：新增 `negotiate_resolve_wire`、`negotiate_with_policy`、`resolve_wire_with_policy` 与 `resolve_with_policy`，联合执行 `VersionCompatibilityPolicy` 与 `DeprecationManifest` 审计：
   - **`ACTIVE` 契约**：正常返回 `ServiceBinding` / `ServiceCandidateSet`，产生 `0` 条告警；`N-1` 与 `N-2` 调用方自动标记 `VersionRoute::OutOfCoreCompatShim` 并保留完整原始输入线字节与 `non_critical_extension_ids`。
   - **`DEPRECATED` 契约**：不中断正常业务流量，照常返回匹配的服务端点 `LocalEndpoint`，同时向目录遥测缓冲与本次 `PolicyDirectoryOutcome.emitted_warnings` 写入结构化 `DeprecationWarningReceipt`（精确记录 `channel`、`symbol`、`field_number`、`deprecated_in_epoch`、`earliest_sunset_epoch`、`replacement`、`caller_major`、`caller_minor`、`service`）。
   - **`SUNSET` / `RESERVED` 契约与越界版本**：立即 fail-closed 返回 `Err(PolicyNegotiationError)`，其 `.to_sabi_failure()` 确定性映射为 `SabiErrorCode::AbiVersion` + `RetryDirective::DoNotRetry`，且不写入误导性弃用告警。

---

## 5. 验证门禁结果与诚实非声明边界（Honest Non-Claims）

### 5.1 本车道独占写集与门禁执行记录

独占写集文件清单：

1. `schema/nlos/sabi/v1/deprecation-manifest.yaml`（新增）
2. `crates/nlos-schema/src/lib.rs`、`crates/nlos-schema/src/version_matrix.rs`、`crates/nlos-schema/src/deprecation.rs`、`crates/nlos-schema/tests/compatibility.rs`、`crates/nlos-schema/tests/version_matrix_and_deprecation.rs`
3. `crates/nlos-service-directory/src/lib.rs`、`crates/nlos-service-directory/tests/deprecation_negotiation.rs`
4. `docs/evidence/stage-e/e-sabi-001-version-matrix-and-deprecation.md`（新增）

验证门禁结果：

1. **代码格式门禁**：`cargo fmt --all -- --check` 退出码 `0`（零格式差异）。
2. **静态检查门禁**：`cargo clippy -p nlos-schema -p nlos-service-directory --all-targets --all-features -- -D warnings` 退出码 `0`（在 `#![forbid(unsafe_code)]`、`clippy::all`、`clippy::pedantic` 下零警告通过）。
3. **单元与集成测试门禁**：`cargo test -p nlos-schema -p nlos-service-directory` 退出码 `0`：
   - `nlos-schema`：`tests/compatibility.rs` `42` 项通过 + `tests/version_matrix_and_deprecation.rs` `6` 项通过 = 共 `48` 项通过，`0` 失败。
   - `nlos-service-directory`：`tests/directory.rs` `5` 项通过 + `tests/deprecation_negotiation.rs` `5` 项通过 = 共 `10` 项通过，`0` 失败。
4. **Protobuf 契约门禁**：`buf lint` 退出码 `0`（零告警输出）。

### 5.2 诚实非声明边界（Honest Non-Claims）

1. **保证等级上限声明（`H4 != H7`）**：本证据包所有结论均建立在单机确定性宿主测试（`H4`）之上，不构成多节点生产集群或真实跨纪元在线热升级的 `H7` 生产级证明。
2. **核外兼容垫片边界（`W78` / `E2` 未在本车道实现）**：本车道（`W77-E1A`）仅在 `nlos-schema` 与 `nlos-service-directory` 中完成 `N / N-1 / N-2` 版本窗口裁决、`VersionRoute::OutOfCoreCompatShim` 路由分流标记与弃用遥测收据；实际执行旧版线帧协议转换的核外垫片单元 `crates/nlos-compat-shim` 及其历史黄金重放测试套件归属下一波次 `W78`（工作包 `E2`），本证据绝不声称 `nlos-compat-shim` 已落地。
3. **跨语言三端黄金矩阵合流边界**：TypeScript / Python SDK 侧对 `9/9` 通道冻结及 `PrincipalHandshake` v1 跨语言字节一致性的扩展归属并行右半片车道 `W77-E1B`，须经 `W77-INT` 屏障合流验证后方可共同闭合里程碑 `M-E1`。
4. **风险台账与阶段退出边界**：`RISK-B-05`（SABI 与应用长期演化兼容风险）在本车道完成后仍保持 `partial` 缓解态，须待 `W78` 核外兼容垫片与 `W81` 决策点 `E2` 验收通过后方可更新闭合；商业级安装器签名公证仍归属 `E-deferred` / `RISK-B-06`，本文档不声称阶段 E 已整体退出。
