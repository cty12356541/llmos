# B-APPLICATION-001：Application/Installation durable 权威最小前缀

> 状态：`PARTIAL PASS`
>
> 日期：2026-08-30
>
> 对应：v0.5 总纲 §23.1（生命周期分离——只取 installed/disabled 状态与当前 installation 代际的最小子集）、[PKG-APPID-001]（ApplicationId/InstallationId 不互换）、[PKG-SEP-001]（Package/Installation/Application identity 分离）
>
> 实现：`crates/nlos-application` schema v1（`src/lib.rs`、`src/schema.rs::migrate_v1`）
>
> 上游消费：`crates/nlos-artifact` `verify_package`/`inspect_package_verification_receipt`（B-ARTIFACT-003 的 verified package receipt 回读 API）

## 1. 本切片目标

把「已验证的 signed Package」落为 durable 安装记录，形成 Slice K 纵切段 `acquire → verify → install` 的 install 权威：`install_application` 消费 nlos-artifact 的 verified receipt 回读 + nlos-identity 验签事实（authority-first：调用方只提供 receipt id，不提供任何验证结论），单事务写 immutable installation receipt 并 CAS 推进 application 当前代际；幂等键冲突 typed；重启 replay；故障矩阵精简版（镜像 nlos-clock）。

## 2. 写集清单

- `crates/nlos-application/**`（新建：`Cargo.toml`、`src/lib.rs`、`src/schema.rs`、`tests/support/mod.rs`、`tests/application_authority.rs`、`tests/application_fault_injection.rs`）
- `Cargo.toml`（仅 members 一行：`"crates/nlos-application"`，字母序首位）
- `Cargo.lock`（新增 crate 条目，工具链生成，未手编）

其余文件未改动。并行车道（ipc/identity/clock）的共享工作区改动未被触碰、未被顺带提交。

## 3. Schema 与 API 摘要

### 3.1 schema v1（两表 + 7 trigger，STRICT）

- **`applications`**（current-state 载体，singleton per package identity）：`application_id`（authority 派生 PK）、`package_id`（UNIQUE）、`package_manifest_digest`（当前安装的 manifest 摘要）、`current_installation_generation`（≥1，稠密、不复用、不回退）、`status`（`1`=installed / `2`=disabled，§23.1 最小子集）、`created_at_ms`/`updated_at_ms`。
- **`installation_receipts`**（fact 载体，immutable）：`installation_id`（PK）、`idempotency_key`（UNIQUE）、`application_id`（FK）、`installation_generation`（UNIQUE(application_id, generation)）、`package_id`、`package_manifest_digest`、`package_version`、`entry_count`、`package_verification_receipt_id`（回指 artifact authority receipt）、`installer_principal`（verified receipt 的 signer）、`installed_at_ms`。
- **trigger 守卫**：application 代际单调（不可减）、身份冻结（application_id/package_id 不可改）、行不可删（uninstall 不在本切片）；receipt 不可变、不可删、AFTER INSERT 界定 receipt 只能落在 application 的**当前**代际（receipt 与代际推进同事务、同生同灭，镜像 clock 水位界定回执）；状态机合法转移（installed→installed 代际推进 / installed→disabled 不动代际 / disabled 终态、未知状态拒绝、disable 不得同时动代际）。

### 3.2 API

- `ApplicationAuthority::open(root)`：`<root>/application-authority.db`，WAL/FULL/foreign_keys，`file:` URI root 跳过 `create_dir_all`（Windows/CI 教训，抄 nlos-clock）。
- **`install_application(&ArtifactStore, request)`**——verify-then-commit 固定顺序，fail-closed：
  1. **artifact receipt 回读（FINALIZED 门）**：经 `inspect_package_verification_receipt` 按 id 回读；immutable receipt 要么完整存在要么不存在，无中间态；缺失为 typed `PackageVerificationReceiptNotFound`，零 durable 状态——未验证包永远无法安装；
  2. **事务内幂等 replay**：durable installation receipt 为权威，返回原 receipt 不动代际（不双跳）；同 key 不同请求形状（receipt id 或时间戳不同）为 typed `IdempotencyConflict`；
  3. **digest 七项绑定**（镜像 b-resource-006 逐位绑定先例）：committed installation row 与 verified receipt 逐位等值——receipt id、package id、manifest digest、package version、entry count、installer principal 六式 + `installed_at_ms >= verified_at_ms` 时间序第七式（不符 typed `InstallationPrecedesVerification`）；六式由构造保证，作为 pre-commit fail-closed 守卫存在；
  4. **单事务提交**：新 package 插入 application singleton（gen 1，派生 application id）；已装 application 读观察→CAS 写推进一代（`WHERE current_installation_generation = <observed>`，丢失为 `CorruptRecord`）；disabled 为 typed `ApplicationDisabled` 拒绝；receipt 插入与代际推进同事务。
- `inspect_application(package_id)` / `inspect_installation(installation_id)` / `list_installations(application_id)`：只读回读。
- **Id 派生**（域分隔 SHA-256，镜像 artifact receipt-id 先例）：`ApplicationId = SHA256("llmos/application/application-id/v1" ‖ package_id)[..16]`；`InstallationId = SHA256("llmos/application/installation-id/v1" ‖ idempotency_key ‖ application_id ‖ generation BE)[..16]`——幻影丢失后同 key 重做确定性收敛到同一 installation id。

## 4. 验收测试与验证门

新增 `tests/application_authority.rs`（9 用例）与 `tests/application_fault_injection.rs`（7 场景 C1–C6 + child helper），lib 内嵌单测 4（Id 派生域分隔/确定性、status 编码、请求形状、七项绑定逐式）。故障矩阵镜像 nlos-clock 样式（kill-9 子进程 + READY 管道同步、FAULT_LOCK 串行、WAL 截断扫描、typed 错误链断言、raw 行计数、`integrity_check`）：C1 IOERR 双阶段、C2 ENOSPC 双阶段、C3 PowerLoss 双向（不可见方向幻影整体消失 + 同 key 确定性重做收敛；kill-9 可见方向提交存活且续推恰为 +1）、C4 torn WAL（≥6 截断点，代际==回执数==最大代际 co-life）、C5 replay storm（不双跳不回退）、C6 崩溃恢复后 trigger 守卫全表面。故障 VFS 经 URI root 注入（`file:///...?vfs=…&tail=` 归一写法 + `file:` 跳过 create_dir_all）；receipt id 经 ASCII 十进制环境变量跨进程传递（Windows 教训）；全部代码平台无关（纯 SQLite+std）。

本地验证命令与结果（2026-08-30，最终 fmt 后运行）：

```text
cargo test -p nlos-application                                # PASS：20 passed / 0 failed
  （lib 单测 4 + application_authority 9 + application_fault_injection 7）
cargo clippy -p nlos-application --all-targets -- -D warnings  # PASS：exit 0
cargo +nightly-2026-08-01 clippy -p nlos-application --all-targets -- -D warnings  # PASS：exit 0
cargo fmt -p nlos-application -- --check                       # PASS：exit 0
cargo +nightly-2026-08-01 fmt -p nlos-application -- --check   # PASS：exit 0
```

过程记录：期间 nlos-identity（并行车道 WIP）一度编译失败阻塞依赖链，等待该车道自愈后重跑；本车道未触碰其文件。所有门以最终绿状态为准。

## 5. 证据等级与限制

证据等级：单节点局部 H3，`PARTIAL PASS`。

明确不声明：

- **无 uninstall/rollback 策略引擎**：`update_application` 见 B-APPLICATION-002；本切片只有 installed/disabled 状态与当前 installation 代际；无 migrate/rollback、无 uninstall；`disabled` 经 `disable_application` 或合法 SQL 转移进入；disabled 后重装/更新 typed 拒绝。
- **无 Task 创建接线**：安装不创建 Task/Process/activation——Slice K 下一纵切段；不发明 Task/Process 语义。
- **单 installer principal、无多方审批**：installer principal 即 verified receipt 的 signer，无 trust-root/签名链/多签/threshold 审批策略。
- **无跨进程验证**：安装 IPC/传输、对端验证、PackageEnvelope 序列化不在本切片。
- **同内容重装推进代际**：generation 计数安装命令（幂等键）而非内容变化；内容级去重/更新通道策略属更新引擎切片。
- **依赖注记**：除任务列出的 nlos-artifact + nlos-types + rusqlite 外，生产依赖加了 `sha2`（Id 派生所必需，镜像 artifact 先例，workspace 同版）；dev-dependencies 加 ed25519-dalek / nlos-identity / nlos-store-fault（测试夹具与故障矩阵所需）。
- 未改变上游限制：B-ARTIFACT-003 的 `LOCAL_SINGLE_NODE`、单 principal 验签、KeyPurpose 复用等限制继续成立；本 authority 同为单写者（进程内 Mutex）。
- 未运行 `cargo test --workspace`、`cargo clippy --workspace`（任务边界禁 `--workspace`）、真实断电、Windows/三平台 CI、生产级并发性能验证。

## 6. 下一步

- Slice K 后半：Task 创建接线（消费 installation receipt 作为 Task 引导事实）。
- identity 侧 package-signing key purpose 落地后，installer principal 与 installer 审批策略分离。
- 更新/卸载策略引擎消费 `disabled` 终态与代际 CAS（兼容窗口/健康检查/原子切换按 [PKG-UPDATE-001]）——**update 最小前缀**见 [B-APPLICATION-002](b-application-002-update.md)（`update_application`，installed 状态内；uninstall/rollback 未做）。
