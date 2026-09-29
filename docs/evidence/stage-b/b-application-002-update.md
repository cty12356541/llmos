# B-APPLICATION-002：Application update 最小前缀（installed 状态内）

> 状态：`PARTIAL PASS`
>
> 日期：2026-09-04
>
> 对应：v0.5 总纲 §23.1（生命周期分离——`update` 在 `installed` 状态内以新 verified package 推进 installation generation）、[PKG-SEP-001]（Package/Installation/Application identity 分离）、[PKG-APPID-001]（ApplicationId/InstallationId 不互换）
>
> 实现：`crates/nlos-application` schema v2（复用 `installation_receipts`，无 additive 表）、`ApplicationAuthority::update_application`
>
> 上游消费：`crates/nlos-artifact` `verify_package`/`inspect_package_verification_receipt`（B-ARTIFACT-003 verified package receipt 回读 API）；前置：`install_application` + `disable_application`（B-APPLICATION-001）

## 1. 本切片目标

在 §23.1 生命周期中落地 **update** 最小前缀：仅当 application 已存在且 `status=installed` 时，消费新 verified package receipt（authority-first：调用方只提供 receipt id + package 身份，不提供验证结论），单事务 CAS 推进 `current_installation_generation` 并写入 immutable installation receipt（与 install 共用 `installation_receipts` 表与 generation-bounds trigger 纪律）。fail-closed：disabled 拒绝、digest 七项绑定、幂等 replay、代际单调、异 key/异形 typed 冲突、manifest 未变拒绝（同内容走 install 重装通道）。

## 2. 写集清单

- `crates/nlos-application/**`（`src/lib.rs` 新增 `update_application`/`UpdateApplicationRequest`/`UpdateDecision` 与 typed 错误；`tests/application_authority.rs` 新增 4 用例；`tests/support/mod.rs` 新增 `updated`/`update_replayed` 夹具）
- `docs/evidence/stage-b/b-application-002-update.md`（本文件）

其余文件未改动。TaskPlan/TaskNode、nlos-task、slice-k、stage-b-progress 未被触碰。

## 3. API 与语义摘要

### 3.1 `update_application`

- **前置**：application singleton 必须已存在（`ApplicationNotFound`）；`status=installed`（`ApplicationDisabled`）；verified receipt 的 `package_id` 与请求命名一致（`PackageIdentityMismatch`）；verified manifest digest 必须与当前 installation 不同（`UpdateManifestUnchanged`——同内容重装仍走 `install_application`）。
- **verify-then-commit 顺序**（镜像 install）：artifact receipt 回读（FINALIZED 门）→ 事务内幂等 replay（durable installation receipt 为权威，不双跳）→ digest 七项绑定（receipt id、package id、manifest digest、package version、entry count、installer principal + `updated_at_ms >= verified_at_ms`）→ 单事务 CAS 代际 +1 与 receipt insert（co-life，DDL generation-bounds 守卫）。
- **Outcome**：`UpdateDecision::Updated` / `Replayed`，事实载体均为 [`InstallationReceipt`]（新代际的 immutable 安装回执）。

### 3.2 与 install 的分工

| 路径 | 首次安装 | 同内容重装（fresh key） | 新 manifest 代际（installed） |
|---|---|---|---|
| `install_application` | ✓ gen 1 | ✓ 推进代际 | ✓ 推进代际 |
| `update_application` | ✗ `ApplicationNotFound` | ✗ `UpdateManifestUnchanged` | ✓ 语义化 update 入口 |

## 4. 验收测试与验证门

新增 `tests/application_authority.rs` 4 用例（update 正常推进、幂等 replay/冲突、重启 replay、拒绝全表）。lib 内嵌单测 4 + application_authority 16 + application_fault_injection 7 不变结构。

本地验证命令与结果（2026-09-04，fmt 后运行）：

```text
cargo test -p nlos-application                                # PASS：27 passed / 0 failed
  （lib 单测 4 + application_authority 16 + application_fault_injection 7）
cargo clippy -p nlos-application --all-targets -- -D warnings  # PASS：exit 0
cargo fmt -p nlos-application -- --check                       # PASS：exit 0
```

## 5. 证据等级与限制

证据等级：单节点局部 H3，`PARTIAL PASS`。

明确不声明：

- **无 uninstall/rollback 策略引擎**：update 只推进 forward generation；无代际回退、无 uninstall、无 `[PKG-UPDATE-001]` 兼容/migration/health-check/原子切换语义。
- **无 Task 创建接线**：update 不创建 Task/Process/activation。
- **无多方审批 / 跨进程验证**：installer principal 仍取自 verified receipt signer。
- **manifest 变化门槛**：仅 digest 不等即视为新代际；无 semver/兼容窗口校验。
- **依赖与并发注记**：单写者 Mutex；未运行 workspace 级门、真实断电、Windows/三平台 CI。

## 6. 下一步

- uninstall / rollback 策略引擎（消费 disabled 终态与代际 CAS，兼容 `[PKG-UPDATE-001]`）。
- Slice K：Task 创建接线（installation/update receipt 作为 Task 引导事实）。
- 更新通道与内容去重：generation 语义从「安装/更新命令」演进为完整内容代际策略。

## 7. W18-001 `[PKG-UPDATE-001]` compatibility window 最小前缀（2026-09-07）

### 已实现事实

1. **`CompatibilityWindow::SameMajor`**：caller 在 `UpdateApplicationRequest::compatibility_window` 显式声明；`validate` 在 pre-mutation 阶段比较当前 installation receipt 与 verified target 的 semver major（`pack_package_version` / `(version >> 32)`）。
2. **fail-closed**：跨 major 返回 `UpdateCompatibilityViolation` 且零 durable 副作用；同 major minor/patch 推进仍走既有 update CAS；idempotent replay 不重复校验。
3. **诚实范围**：非完整 `[PKG-UPDATE-001]` 引擎——无 migration runner、health check、binary 原子切换或多步编排。

### 验证门（W18-001 实跑）

| 门 | 命令 | 结果 |
| --- | --- | --- |
| 兼容窗口三用例 | `cargo test -p nlos-application --test application_authority update_compat` | PASS（accept/reject/replay） |
| lib 单测 | `cargo test -p nlos-application --lib compatibility_window` | PASS |
| clippy | `cargo clippy -p nlos-application --all-targets -- -D warnings` | （波次 18 integrator 未复跑 workspace 全仓门） |

## 8. W19-001 `[PKG-UPDATE-001]` SameMinor compatibility window prefix（2026-09-08）

### 已实现事实

1. **`CompatibilityWindow::SameMinor`**：caller 显式声明；`validate` 在 pre-mutation 阶段比较当前 installation 与 verified target 的 packed semver major+minor（`version >> 16`）；patch 可任意变化。
2. **fail-closed**：跨 minor（含跨 major）返回 `UpdateCompatibilityViolation` 且零 durable 副作用；同 major+minor 的 patch 推进仍走既有 update CAS；idempotent replay 不重复校验（replay 路径在 compatibility gate 之前）。
3. **诚实范围**：非完整 `[PKG-UPDATE-001]` 引擎——无 patch-downgrade 拒绝、migration runner、health check 或多步编排。

### 验证门（W19-001 实跑）

| 门 | 命令 | 结果 |
| --- | --- | --- |
| SameMinor 三用例 | `cargo test -p nlos-application --test application_authority update_compat_same_minor` | PASS（accept/reject/replay） |
| lib 单测 | `cargo test -p nlos-application --lib compatibility_window_same_minor` | PASS |
| 兼容窗口全集 | `cargo test -p nlos-application --test application_authority update_compat` | PASS（6 passed） |

## 9. W29-E（B1-3）PKG migration runner + health check + 原子切换（2026-09-20）

### 9.1 已实现事实

补齐 ROAD-B-001 在本文件 §5/§7/§8 反复声明的缺口："无 migration runner、health check、原子切换"。实现落在 schema v7 + `src/migration.rs`（新模块），API 五个命令 + 四个读面，全部走既有 verify-then-commit / fail-closed / 幂等 replay 纪律：

1. **迁移演练状态机（per-application durable）**：`migrate_application` 以 FINALIZED 门回读 verified receipt 后，单事务落一行 drill（冻结基线 `from_generation/from_manifest_digest/from_package_version`（拷自当前 installation receipt）+ 冻结目标 `target_manifest_digest/target_package_version`（拷自 verified receipt）+ 兼容窗口 + 声明步数），初态 `pending`。状态格 `pending → running → done|failed`（`done|failed` 终态）由 DDL trigger 承载；partial unique index 保证同一 application 至多一条在途 drill，终态 drill 累积为历史。
2. **步骤记录（crash 收敛的载体）**：`record_migration_step` 只接受 `1 + completed` 的严格顺序（DDL trigger 同口径），步骤行 immutable；replay 逐字节幂等，同 index 不同时间戳为 typed `IdempotencyConflict`。任两条命令之间崩溃→重开 authority，replay 已记录前缀后从下一 index 续跑（进程重启收敛模型；kill-9 fault 矩阵为后续车道）。
3. **健康检查（typed pass/fail durable）**：`run_migration_health_check` 前置 = drill `running` 且全部步骤完成；先由 authority 自做诚实再验证（重读 artifact verified receipt + 对照冻结目标绑定四等式，漂移 fail-closed `CorruptRecord`），再咨询 caller 传入的 `MigrationHealthProbe`（trait，事务开着咨询——activity gate 先例）恰一次；verdict `unchecked → passed|failed` 终态落库（DDL 格），replay 不再咨询 probe（测试断言 probe 调用计数）。config-shape 校验等更重检查归 caller probe 实现。
4. **原子切换（单事务 CAS）**：`activate_package_migration` 前置 = `running` + verdict `passed` + 步骤齐 + 应用仍 `installed` 且仍在冻结基线（代际+digest 双比对）。切换是一个 `Immediate` 事务：application 行 CAS（`from_generation → from+1`、digest→target、`status=installed` 谓词）→ 新代际 installation receipt（复用 `installation_receipts` 表、generation-bounds trigger、七等式 digest 绑定、`derive_installation_id(迁移 key, app, gen)`）→ drill 行 `done` + `activated_installation_id`（FK 指向 receipt，DDL 强制 done 必须带 passed verdict + activation 引用）。三件事 co-life：崩溃前应用整体停在旧代际且 drill 可续，replay 返回原 receipt 不双跳。
5. **PKG 级回滚（≠ 生命周期回滚门）**：`rollback_package_migration` 前置 = verdict `failed` + 基线未移动。单事务：drill 行 → `failed` + immutable `application_migration_rollback_receipts` 行（retained 基线代际/digest + abandoned 目标 digest）。application 行**从不被触碰**——失败演练从未激活任何东西，旧 revision 本就保持 active，receipt 只是把这一事实 durable + replayable。与 `rollback_application`（disabled/uninstalled → installed 代际回退一步）不同表、不同门、不同义；测试双向断言（PKG 回滚后生命周期门仍拒 installed 应用；生命周期回执表不收迁移 key）。
6. **与既有面的交互**：与 update 通道同一兼容窗口语义（`SameMajor`/`SameMinor` 均在 begin 前置复验，`UpdateCompatibilityViolation` 零 durable）；同一 manifest-unchanged 门（`UpdateManifestUnchanged`）；在途 drill 被直接 `update_application` 超越后，activation/rollback typed `MigrationBaselineMoved`（绝不静默挪动应用），该 drill 停留 `running`（无 abandon 命令，见 9.4）；迁移成功后直接通道与后续 drill 从新代际稠密续推（gens 1,2,3,4）；W28-B 模板段包（`verify_package_with_tasks` receipt）与无段包走同一迁移面（runner 只消费 receipt 形状，段无关）。

### 9.2 验证门（W29-E 实跑，2026-09-20）

| 门 | 命令 | 结果 |
| --- | --- | --- |
| 全 crate 测试 | `cargo test -p nlos-application` | PASS：74 passed / 0 failed（lib 单测 9 含 3 新增、application_authority 43 原样全绿、fault_injection 7 原样全绿、application_migration 11 新增、manifest_task_templates 4 原样全绿） |
| 同 major 迁移演练 + 回滚路径 + 健康检查（车道门） | `cargo test -p nlos-application --test application_migration` | PASS（11：happy path 原子切换、begin 幂等/冲突/单在途、拒绝全表零 durable、步骤门与 typed 时序、崩溃收敛逐字节等于无崩溃对照+probe 不重询、健康失败→PKG 回滚 durable/replay/终态拒绝、失败后新迁移+生命周期门区分、模板段包、直接 update 交互 `MigrationBaselineMoved`、代际稠密续推、SameMinor 窗口交互） |
| clippy | `cargo clippy -p nlos-application --all-targets -- -D warnings` | PASS：exit 0 |
| fmt | `cargo fmt -p nlos-application -- --check` | PASS：exit 0 |

### 9.3 写集

`crates/nlos-application/src/{migration.rs 新增, schema.rs v7, lib.rs 模块接线+错误变体+文档头}`、`crates/nlos-application/tests/application_migration.rs` 新增、本文件 §9。未触碰 nlos-types/nlos-artifact/其他 crate、stage-b-progress.md；既有测试（W27-D activity gate、W18/W19 兼容窗口、W28-B 模板段）未改一行且全绿。

### 9.4 证据等级与 deferred minors

证据等级：单节点局部 H3，`PARTIAL PASS`（车道门内全绿）。诚实不声明：

- **步骤执行不在本 authority**：drill 步骤是 caller 驱动的 durable 完成标记（本 crate 拥有生命周期事实，不拥有用户数据/schema）；步骤体执行、`BACKWARD_COMPATIBLE_WINDOW`/`SNAPSHOT_ASSISTED_ROLLBACK`/`IRREVERSIBLE` 声明面、Trusted-UI 确认接线均为后续车道。
- **无 abandon 命令**：在途 drill 被直接 update 超越后停留 `running` 并阻塞该应用的新 drill（typed `MigrationAlreadyLive`）；fail-closed 但占位，abandon/cancel 是显式 deferred minor（minor：正常编排不会直接 update 一个在途迁移的应用）。
- **kill-9 fault 矩阵未扩展**：崩溃收敛以重开 authority（进程重启模型）验证；对五个迁移入口的 kill-9/WAL 尾撕裂矩阵沿用既有 harness 另行补齐。
- **kill 窗口内的多进程并发**：单写者 Mutex 内 CAS 已按多进程口径写谓词（基线 CAS 失败 typed `MigrationBaselineMoved`/`CorruptRecord`），但未做跨进程注入实测。
