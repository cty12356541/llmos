# B-APPLICATION-003：Application uninstall 最小前缀（installed|disabled → uninstalled）

> 状态：`PARTIAL PASS`
>
> 日期：2026-09-04
>
> 对应：v0.5 总纲 §23.1（生命周期分离——`uninstall` 为 terminal `installed|disabled → uninstalled` CAS 标记）、[PKG-SEP-001]（Package/Installation/Application identity 分离）
>
> 实现：`crates/nlos-application` schema v3（`application_uninstall_receipts` + `status=uninstalled`）、`ApplicationAuthority::uninstall_application`
>
> 上游消费：B-APPLICATION-001 `install_application`/`disable_application`、B-APPLICATION-002 `update_application`

## 1. 本切片目标

在 §23.1 生命周期中落地 **uninstall** 最小前缀：当 application 已存在且 `status=installed|disabled` 时，单事务 CAS 标记 `uninstalled`（代际不动）并写入 immutable uninstall receipt（replay-first 幂等、异键 typed 拒绝、时间戳不得早于 row 最后更新）。fail-closed：未知 package、幂等冲突、终态异键、时间倒流、CAS 丢失。

## 2. 写集清单

- `crates/nlos-application/**`（`src/schema.rs` migrate_v3；`src/lib.rs` 新增 `uninstall_application`/`UninstallApplicationRequest`/`UninstallDecision`/`UninstallReceipt` 与 typed 错误；`tests/application_authority.rs` 新增 4 用例；`tests/support/mod.rs` 新增 `uninstalled`/`uninstall_replayed` 夹具；既有 trigger/故障注入用例对齐 v3 状态机）
- `docs/evidence/stage-b/b-application-003-uninstall.md`（本文件）

其余文件未改动。TaskPlan/TaskNode、stage-b-progress 未被触碰。

## 3. API 与语义摘要

### 3.1 `uninstall_application`

- **前置**：application singleton 必须已存在（`ApplicationNotFound`）；`status=installed|disabled`（`ApplicationAlreadyUninstalled` 对终态异键；`ApplicationUninstalled` 供 install/update/disable 拒绝已卸载应用）。
- **verify-then-commit 顺序**（镜像 disable）：事务内幂等 replay（durable uninstall receipt 为权威）→ 时间戳 `uninstalled_at_ms >= updated_at_ms`（`UninstallPrecedesLastUpdate`）→ 单事务 status CAS `→ uninstalled` + receipt insert（co-life，DDL state-bounds 守卫：receipt 代际 = 当前代际且 status 已为 uninstalled）。
- **Outcome**：`UninstallDecision::Uninstalled` / `Replayed`，事实载体均为 [`UninstallReceipt`]（记录卸载时刻代际，代际本身不被 transition 移动）。

### 3.2 状态机（schema v3）

| 自 \ 至 | installed (1) | disabled (2) | uninstalled (3) |
|---|---|---|---|
| installed | 代际推进（install/update） | disable（代际不动） | uninstall（代际不动） |
| disabled | ✗ | ✗（终态异键 disable） | uninstall（代际不动） |
| uninstalled | ✗ | ✗ | ✗（terminal） |

## 4. 验收测试与验证门

新增 `tests/application_authority.rs` 4 用例（installed 卸载幂等、disabled 卸载幂等、update 后代际卸载、拒绝全表）。lib 内嵌单测 4 + application_authority 20 + application_fault_injection 7。

本地验证命令与结果（2026-09-04）：

```text
cargo test -p nlos-application                                # PASS：31 passed / 0 failed
  （lib 单测 4 + application_authority 20 + application_fault_injection 7）
cargo clippy -p nlos-application --all-targets -- -D warnings  # PASS：exit 0
cargo fmt -p nlos-application -- --check                       # PASS：exit 0
```

## 5. 证据等级与限制

证据等级：单节点局部 H3，`PARTIAL PASS`。

明确不声明：

- **无 rollback 策略引擎**：uninstall 只写 terminal CAS 标记；无代际回退、无 `[PKG-UPDATE-001]` 兼容/migration/health-check/原子切换语义。
- **无 Task/Process teardown**：uninstall 不停止、不等待运行中 Task/Process；不 revoke Capability。
- **无 GC / 物理删行**：application/installation/disable/uninstall receipt 行均 durable；无 artifact 垃圾回收。
- **无多方审批 / 跨进程验证**：uninstall 请求不含 principal 绑定（与 disable 对称）。
- **依赖与并发注记**：单写者 Mutex；未运行 workspace 级门、真实断电、Windows/三平台 CI。

## 6. 下一步

- rollback 策略引擎（消费 uninstalled 终态与代际 CAS，兼容 `[PKG-UPDATE-001]`）。
- Slice K：Task 创建/销毁接线（uninstall 后 Task 引导与 teardown 策略）。
- 完整 ROAD-B-001 生命周期：running-task 拒绝、GC、跨进程 uninstall 审批。
