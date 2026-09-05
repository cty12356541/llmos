# B-APPLICATION-004：Application rollback 最小前缀（disabled|uninstalled → installed，代际 -1）

> 状态：`PARTIAL PASS`
>
> 日期：2026-09-05
>
> 对应：v0.5 总纲 §23.1（生命周期分离——rollback 为 `disabled|uninstalled → installed` 单步代际 CAS 回退）、[PKG-UPDATE-001]（仅消费「回退锚点」语义，不实现完整 health-check/兼容/migration 引擎）
>
> 实现：`crates/nlos-application` schema v4（`application_rollback_receipts` + 放宽 generation/status trigger）、`ApplicationAuthority::rollback_application`
>
> 上游消费：B-APPLICATION-001 install/disable、B-APPLICATION-002 update、B-APPLICATION-003 uninstall

## 1. 本切片目标

在 §23.1 生命周期中落地 **rollback** 最小前缀：当 application 已存在且 `status=disabled|uninstalled`、当前代际 > 1 时，单事务 CAS 标记 `installed`、代际 -1、从 durable `installation_receipts` 恢复上一代 manifest digest，并写入 immutable rollback receipt（replay-first 幂等、fail-closed 门、时间戳不得早于 row 最后更新）。不实现完整 `[PKG-UPDATE-001]` 策略引擎。

## 2. 写集清单

- `crates/nlos-application/**`（`src/schema.rs` migrate_v4；`src/lib.rs` 新增 `rollback_application`/`RollbackApplicationRequest`/`RollbackDecision`/`RollbackReceipt` 与 typed 错误；`tests/application_authority.rs` 新增 3 用例；`tests/support/mod.rs` 新增 `rolled_back`/`rollback_replayed` 夹具；trigger 守卫用例对齐 v4 状态机）
- `docs/evidence/stage-b/b-application-004-rollback.md`（本文件）

其余文件未改动。GC/运行中 Task 门、stage-b-progress、议题 35 未被触碰。

## 3. API 与语义摘要

### 3.1 `rollback_application`

- **前置**：application singleton 必须已存在（`ApplicationNotFound`）；`status=disabled|uninstalled`（`RollbackRequiresDisabledOrUninstalled`）；当前代际 > 1（`RollbackAtInitialGeneration`）；durable history 中存在上一代 installation receipt（`PreviousInstallationNotFound`）。
- **verify-then-commit 顺序**（镜像 disable/uninstall）：事务内幂等 replay（durable rollback receipt 为权威）→ 时间戳 `rollback_at_ms >= updated_at_ms`（`RollbackPrecedesLastUpdate`）→ 单事务 status CAS `→ installed` + generation CAS `-1` + manifest digest 恢复 + receipt insert（co-life，DDL state-bounds 守卫：receipt `to_generation` = 当前代际且 status 已为 installed）。
- **Outcome**：`RollbackDecision::RolledBack` / `Replayed`，事实载体均为 [`RollbackReceipt`]（记录 `from_generation`/`to_generation`，一步回退）。

### 3.2 状态机（schema v4 增量）

| 自 \ 至 | installed (1) | disabled (2) | uninstalled (3) |
|---|---|---|---|
| installed | 代际推进（install/update） | disable（代际不动） | uninstall（代际不动） |
| disabled | **rollback（代际 -1）** | ✗（终态异键 disable） | uninstall（代际不动） |
| uninstalled | **rollback（代际 -1）** | ✗ | ✗（terminal 异键 uninstall） |

generation 单调 trigger 仅在 `disabled|uninstalled → installed` 且代际恰减 1 时允许下降。

## 4. 验收测试与验证门

新增 `tests/application_authority.rs` 3 用例（disabled 回退幂等、uninstalled 回退幂等、拒绝全表）。lib 内嵌单测 4 + application_authority 23 + application_fault_injection 7。

本地验证命令与结果（2026-09-05）：

```text
cargo test -p nlos-application                                # PASS：34 passed / 0 failed
  （lib 单测 4 + application_authority 23 + application_fault_injection 7）
cargo clippy -p nlos-application --all-targets -- -D warnings  # PASS：exit 0
cargo fmt -p nlos-application -- --check                       # PASS：exit 0
```

## 5. 证据等级与限制

证据等级：单节点局部 H3，`PARTIAL PASS`。

明确不声明：

- **无完整 `[PKG-UPDATE-001]` 引擎**：无兼容窗口校验、migration runner、health check、binary 原子切换或多步回退编排；仅单步代际锚点。
- **无 GC / 运行中 Task 门**：rollback 不停止 Task/Process、不 revoke Capability、不物理删行或 GC artifact。
- **无 re-disable/re-uninstall 策略**：历史 disable/uninstall receipt 仍 durable（PRIMARY KEY 不变）；rollback 后再次 disable/uninstall 的语义留给后续策略切片。
- **单写者 Mutex**；未运行 workspace 级门、真实断电、Windows/三平台 CI。

## 6. 下一步

- 完整 ROAD-B-001 生命周期：running-task 拒绝、GC、跨进程 rollback 审批、多步回退编排。
- Slice K：rollback 纵切面接线（消费 `rollback_application` 最小前缀）。
