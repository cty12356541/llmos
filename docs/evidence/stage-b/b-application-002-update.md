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
