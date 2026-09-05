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
- ~~Slice K：Task 创建/销毁接线（uninstall 后 Task 引导与 teardown 策略）。~~ **部分勾销（2026-09-05）**：`nlos-slice-k` 已接线 `SliceKRuntime::uninstall_application`（installed\|disabled → uninstalled + fail-closed 重装）；demo STEP 09c + `lifecycle_uninstall` 2 测试；Task/Process teardown 策略仍开放——见 [B-SLICE-K-001 §9](b-slice-k-001-end-to-end.md#9-application-lifecycle-uninstall-接入纵切面2026-09-05-追加disable--uninstall-最小前缀)。
- 完整 ROAD-B-001 生命周期：running-task 拒绝、GC、跨进程 uninstall 审批。

## 7. W16-001 追加：运行中 Task 拒绝 uninstall/rollback（2026-09-05）

> 状态：`PARTIAL PASS`（activity gate 前缀；无 Task 登记/Teardown）

### 7.1 目标

ROAD-B-001 最小前缀：**运行中 Task 拒绝 uninstall/rollback**。Authority 不依赖 `nlos-task`；caller 经 `ActiveTaskActivityProbe` 提供 outstanding task count。

### 7.2 写集

- `crates/nlos-application/src/lib.rs`：`ActiveTaskActivityProbe`、`ApplicationActiveTasksRunning`、`uninstall_application_with_activity_gate` / `rollback_application_with_activity_gate`（internal 共享 replay-first 路径；gate 在 pre-mutation 校验通过后、CAS 前执行）
- `crates/nlos-application/tests/application_authority.rs`：3 用例（fresh uninstall 拒绝、replay 绕过 gate、fresh rollback 拒绝）+ `MockTaskProbe`

### 7.3 语义

- **Gate 时机**：replay 成功路径不受影响；fresh mutation 在 temporal/generation 等校验通过后、`UPDATE` 前 consult probe。
- **拒绝**：`probe.outstanding_task_count(package_id) > 0` → `ApplicationActiveTasksRunning { package_id, active_task_count }`；事务 drop，零 durable 副作用。
- **Ungated API**：`uninstall_application` / `rollback_application` 保持向后兼容，不 consult probe。

### 7.4 验收

```text
cargo test -p nlos-application                                # PASS：34 passed / 0 failed
cargo clippy -p nlos-application --all-targets -- -D warnings  # PASS
cargo fmt -p nlos-application -- --check                       # PASS
```

### 7.5 仍开放（PARTIAL_PASS 缺口）

- Task 创建/登记与 probe 生产接线（`nlos-task` / Slice K runtime 侧）
- Task/Process teardown、Capability revoke、GC
- 跨进程 uninstall 审批
- Workspace 级门、真实断电、三平台 CI

## 8. W17-001 追加：Slice K uninstall 后显式 orphan GC（2026-09-06）

> 状态：`PARTIAL PASS`（手动 GC 前缀；无自动触发、uninstall 不解除 artifact 引用）

### 8.1 目标

ROAD-B-001 GC 最小前缀：Slice K 纵切面在 uninstall 后调用 B-ARTIFACT-004 `collect_orphan_blobs`，证明 package 专属可证明孤儿 blob 可被保守收集，referenced blob 保留。

### 8.2 写集

- `crates/nlos-slice-k/**`（`SliceKRuntime::collect_orphan_blobs`、`plant_orphan_artifact_blob`；demo STEP 09d；`lifecycle_uninstall` +1 用例）
- 证据：`b-slice-k-001-end-to-end.md` §10、`b-application-003-uninstall.md` §8（本段）

### 8.3 限制（Claim ≤ Evidence）

- **手动 GC**：仅显式调用；无 uninstall 挂钩、无 schedule/open-time sweep。
- **uninstall 不删 artifact 元数据/引用**：GC 引用集不变；payload/head blob 非孤儿，不会被本路径误删。
- **无 PKG-UPDATE-001 / retention-GC / Task teardown**。

### 8.4 验证

见 [B-SLICE-K-001 §10.4](b-slice-k-001-end-to-end.md#104-验证base-head-77efcb6-工作区定向--p-命令)。
