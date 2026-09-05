# B-APPLICATION-006：Application 多 Process binding 登记最小前缀（ROAD-B-002）

> 状态：`PARTIAL PASS`
>
> 日期：2026-09-06
>
> 对应：v0.5 总纲 `[ROAD-B-002]`（Application 能拥有多 Process binding）、§23.1 Application 生命周期（仅消费 installed 门）
>
> 实现：`crates/nlos-application` schema v6（`application_process_bindings` + generation fence DDL）、`ApplicationAuthority::register_process_binding` / `inspect_process_bindings`
>
> 上游消费：B-APPLICATION-001 install、B-APPLICATION-002 update（generation fence 随代际推进）；B-APPLICATION-005 background task 登记模式

## 1. 本切片目标

在 Application authority 中落地 **多 Process binding 登记** 最小前缀：当 application 已存在且 `status=installed` 时，单事务写入 immutable binding receipt，将 `ApplicationId` 与 `ProcessId` 在 **当前 installation generation** 上 durable 绑定（registrant principal + idempotency + generation fence），并提供只读 inspect API。fail-closed 拒绝 disabled/uninstalled application。不 spawn/kill Process、不依赖 nlos-process runtime、不实现 UI Surface 或 TaskPlan/TaskNode。

## 2. 写集清单

- `crates/nlos-application/**`（`src/schema.rs` migrate_v6；`src/lib.rs` 新增 binding API/类型/错误；`tests/application_authority.rs` 新增 3 用例；`tests/support/mod.rs` 新增夹具）
- `docs/evidence/stage-b/b-application-006-process-binding.md`（本文件）

其余文件未改动。stage-b-progress、nlos-task/nlos-process/nlos-slice-k 未被触碰。

## 3. API 与语义摘要

### 3.1 `register_process_binding`

- **前置**：application singleton 必须已存在（`ApplicationNotFound`）；`status=installed`（`ApplicationDisabled` / `ApplicationUninstalled`）；同代际同 process 异键（`ProcessAlreadyRegistered`）。
- **verify-then-commit 顺序**：事务内幂等 replay（durable binding receipt 为权威）→ 时间戳 `registered_at_ms >= updated_at_ms`（`RegistrationPrecedesLastUpdate`）→ receipt insert（DDL state-bounds 守卫：receipt `application_generation` = 当前代际且 status 已为 installed）。
- **Outcome**：`RegisterProcessBindingDecision::Registered` / `Replayed`，事实载体均为 [`ProcessBindingReceipt`]。

### 3.2 `inspect_process_bindings`

- 按 `package_id` 列出该 application 的全部 immutable binding receipts（oldest first）。未知 package 返回空列表。

### 3.3 Schema v6 增量

- 表 `application_process_bindings`：`idempotency_key` PK、`UNIQUE(application_id, process_id, application_generation)`、immutable/durable triggers、AFTER INSERT generation fence。

## 4. 验收测试与验证门

新增 `tests/application_authority.rs` 3 用例（正常登记幂等、disabled/uninstalled 拒绝、重复/conflict/时间戳拒绝）。

本地验证命令（W17-002）：

```text
cargo test -p nlos-application
cargo clippy -p nlos-application --all-targets -- -D warnings
cargo fmt -p nlos-application -- --check
```

## 5. 证据等级与限制

证据等级：单节点局部 H3，`PARTIAL PASS`。

明确不声明：

- **无 Process 运行时**：不 spawn/kill Process、不依赖 nlos-process；binding 仅为 Application authority 侧 durable 登记面。
- **无 UI Surface / TaskPlan/TaskNode**：ROAD-B-002 其余子能力未实现。
- **generation fence 不自动失效旧代际 binding**：历史代际 receipt 仍 durable；调用方自行解释 staleness。
- **单写者 Mutex**；未运行 workspace 级门、真实断电、Windows/三平台 CI。

## 6. 下一步

- ROAD-B-002 续片：UI Surface 登记面或与 process authority 接线。
- Slice K 纵切面：binding receipt 与 process authority 接线。
- uninstall/disable 时已绑定 Process 的 teardown/GC 策略。
