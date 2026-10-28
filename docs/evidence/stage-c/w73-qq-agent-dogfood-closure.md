# W73-DOGFOOD-001：`qq-agent` Dogfood 全量 7 项上游问题闭环（`L-001` ~ `L-007`）

- 状态：`PASS`（`L-001` ~ `L-007` 全量 7 项上游问题在 `W73` 多轨编排波次中全部落地并跑通 Rust 单元/集成测试与 Python ⇄ Rust 跨进程 IPC Conformance 测试）
- 日期：2026-10-29
- Owner：`W73` 多轨编排（`w73-a1`, `w73-a2`, `w73-b1`, `w73-c1`, `w73-d1`, `w73-int1`）
- 设计依据：[06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §10 `[IPC-CHAN-001]`、§13 `[RES-BUDGET-001]` & `[RES-ENFORCE-001]`、§21 `[SCHEMA-REG-001]` + [ADR-0014](../../management/adrs/0014-schema-channel-freeze-v1-beta.md) + [ADR-0024](../../management/adrs/0024-driver-provider-payload-codec-freeze.md) + [language-sdk-support-plan.md](../../management/language-sdk-support-plan.md)
- Base HEAD：`75e1c3f8a5b5fc39179d9f6f8368f782b7137bd7`

## 1. 背景与目标

外部应用 `qq-agent` 在 M0 Dogfood 探针与评审阶段登记了 7 项 `llmos` 上游问题（`L-001` ~ `L-007`）。`W73` 波次按写集互斥与屏障依赖拆分为 4 条并行车道 + 1 条串行下游车道 + 1 条收口车道，一次性完成全部 7 项问题的架构补面、实现与跨语言实测闭环。

## 2. 逐项实现事实与权威提交锚点

| 编号 | 问题描述 | 归属车道与提交 | 核心实现与权威文件 |
| --- | --- | --- | --- |
| `L-001` | `directory_chain.py` 在 Windows 粗时钟粒度下因 `time_ns & 0xFFFFFFF` + `label[0]` 截断导致同名 Named Pipe 冲突（`ACCESS_DENIED`） | `w73-d1` (`ee6c42d4`) | `tests/conformance/ipc/directory_chain.py` 引入进程内单调递增计数器 `_PIPE_SEQ`、PID、全量 `label` 与 `secrets.token_bytes(8)` 构造 SHA-256 pipe 派生摘要，彻底消除粗时钟粒度碰撞。 |
| `L-002` | `channel` 仅有进程内 Rust 库 API（`nlos-channel`），`nlos-system-control` 持有 `Arc<ChannelAuthority>` 但无 SABI 跨进程服务面与 Python SDK 导出 | `w73-a1` (`5e409b0f`) + `w73-a2` (`f8b1687f`) | 1. 冻结第 9 个 SABI 通道 `nlos.sabi.ChannelControl` (`v1.0`, `64 KiB` 上界)：新增 `schema/nlos/sabi/v1/channel_control.proto`、`gen/python/nlos/sabi/v1/channel_control_pb2.py`、`gen/typescript/nlos/sabi/v1/channel_control_pb.ts`，并在 `crates/nlos-schema` 暴露 18 个有界编解码函数。<br>2. 在 `crates/nlos-system-control/src/channel_control.rs` 实现 `ChannelControlService` 全量 9 个方法（`create_channel`, `rotate_channel`, `enqueue`, `receive`, `ack`, `compact`, `inspect_channel`, `inspect_queue`, `register_queue_consumption`），经 `auth.rs` (`serve_validated_with_channel`) 与 `daemon.rs` 挂载至 `SystemControlDaemon` 的 plain/auth 端点，并将 `service = "channel"` 注册入内置 `ServiceDirectory`。<br>3. 新增 `sdk/python/nlos_sdk/channel.py`（`ChannelClient` 及不可变数据类/异常）与 `tests/conformance/ipc/channel_control.py` 跨进程一致性测试。 |
| `L-003` | Python SDK 无 `pyproject.toml` 打包入口与版本号，外部应用被迫手写 `sys.path` 注入 `sdk/python` 与 `gen/python` | `w73-d1` (`ee6c42d4`) + `w73-a2` (`f8b1687f`) | 新增 `sdk/python/pyproject.toml`（支持 `pip install -e sdk/python` / `uv pip install -e sdk/python`）与 `sdk/python/README.md`；在 `local_rpc.py`、`service_directory.py`、`channel.py` 内置 `_ensure_gen_python_path()` 自动挂载仓内 `gen/python`；`nlos_sdk.__init__` 导出 `__version__ = "0.1.0"`。 |
| `L-004` | Python Windows Named Pipe 强依赖 `ProactorEventLoop`，在 `SelectorEventLoop` 下缺乏明确诊断信息 | `w73-d1` (`ee6c42d4`) | `sdk/python/nlos_sdk/local_rpc.py` (`_connect_windows_pipe`) 在调用 `create_pipe_connection` 前显式检测 `hasattr(loop, "create_pipe_connection")`，若缺失则立即抛出 `IpcError("UNSUPPORTED_EVENT_LOOP", ...)` 并提示切换 `asyncio.WindowsProactorEventLoopPolicy()`。 |
| `L-005` | `nlos-resource` 六维资源守恒与计量账本不含 LLM Token 与模型 API 调用费维度 | `w73-b1` (`1c796212` + `2c6c9859`) | `crates/nlos-resource` 升级至 `SCHEMA_VERSION = 8`（`migrate_v8`）：新增 3 张 `STRICT` 持久化表（`llm_token_budgets`、`llm_token_consumptions`、`llm_token_budget_revisions`）与 6 个 SQLite 触发器，实现 `LlmTokenBudgetRecord`（`max_prompt_tokens`, `max_completion_tokens`, `max_total_tokens`, `max_api_calls`, `max_cost_micros_usd` 五维上界）、幂等扣减 `consume_llm_tokens`（超限返回 `LlmTokenQuotaExceeded` 并带完整 `LlmTokenBudgetSnapshot`）、在线调额 `update_llm_token_budget` 以及 `to_open_metrics_text()` 4 项 OpenMetrics 家族导出；`crates/nlos-system-control/src/resource_inspector.rs` 对齐错误码映射。 |
| `L-006` | `tests/conformance/ipc/client.py` 断言型静默，无逐用例进度输出 | `w73-d1` (`ee6c42d4`) | `tests/conformance/ipc/client.py` 补齐 `[conformance/ipc] case i/N <name> ... PASS` 逐行进度输出与 `[conformance/ipc] ALL N/N CASES PASSED` 汇总行（`flush=True`）。 |
| `L-007` | `crates/nlos-channel/tests/` 关键集成测试无结构化日志，外部 `--nocapture` 无法提取投递与消费登记轨迹 | `w73-c1` (`98b9c3ac`) | 在 `queue_delivery.rs`、`queue_consumption_registration.rs`、`channel_authority.rs` 补齐 `[nlos-channel/<suite>] ...` 结构化运行时轨迹输出，支持 `-- --nocapture` 直接审计。 |

## 3. 验证命令与原始输出摘要

### 3.1 Rust 单元与集成测试

```text
$ cargo test -p nlos-schema -p nlos-channel -p nlos-resource
- nlos-schema: 41 passed; 0 failed (含 channel_control_round_trip_and_bounds_check)
- nlos-channel: 31 passed; 0 failed (queue_delivery / queue_consumption_registration / channel_authority / channel_fault_injection / crash_recovery)
- nlos-resource: 61 passed; 0 failed (含 tests/llm_token_budget.rs 7 项测试)

$ cargo test -p nlos-system-control --test channel_control_sabi --features daemon
running 4 tests
test channel_control_sabi_full_lifecycle_and_idempotent_replay ... ok
test channel_control_sabi_fail_closed_error_mapping_and_authorization ... ok
test serve_validated_with_channel_multiplexes_channel_and_system_control_over_local_ipc ... ok
test daemon_plain_endpoint_and_in_process_channel_control_round_trip ... ok
test result: ok. 4 passed; 0 failed
```

### 3.2 Python ⇄ Rust 跨进程 IPC Conformance 测试

```text
$ python tests/conformance/ipc/channel_control.py
[conformance/channel_control] case 1/6 create_channel + idempotent replay PASS
[conformance/channel_control] case 2/6 enqueue + enqueue_registered + replay PASS
[conformance/channel_control] case 3/6 receive + inspect_queue PASS
[conformance/channel_control] case 4/6 register_queue_consumption + ack + compact PASS
[conformance/channel_control] case 5/6 inspect_channel + rotate_channel PASS
[conformance/channel_control] case 6/6 fail-closed error handling PASS
[conformance/channel_control] ALL 6/6 CASES PASSED

$ python tests/conformance/ipc/directory_chain.py
[conformance/directory_chain] phase 1/6 ServiceDirectory negotiation (service_control + wait_control) PASS
[conformance/directory_chain] phase 2/6 wait_control register_wait (replayed=False) PASS
[conformance/directory_chain] phase 3/6 wait_control idempotent replay (replayed=True) PASS
[conformance/directory_chain] phase 4/6 wait_control complete_wait (completed=True) PASS
[conformance/directory_chain] phase 5/6 wait_control cancel_wait terminal no-op (cancelled=False) PASS
[conformance/directory_chain] phase 6/6 system_control get health snapshot PASS
[conformance/directory_chain] ALL 6 PHASES PASSED

$ python tests/conformance/ipc/client.py
[conformance/ipc] case 1/4 happy_path ... PASS
[conformance/ipc] case 2/4 oversized_frame_rejected ... PASS
[conformance/ipc] case 3/4 response_timeout ... PASS
[conformance/ipc] case 4/4 windows_non_proactor_loop_rejected ... PASS
[conformance/ipc] ALL 4/4 CASES PASSED
```
