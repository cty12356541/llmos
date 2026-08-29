# B-CONTROL-001：ControlCommand/Receipt 类型面与 CLI 入口

> 状态：`PARTIAL PASS`　　日期：2026-08-29
>
> 对应：`B-TASK-006L`、`B-SCHEMA-014`、总纲 §24（Intent→Action→Receipt）、§25.3（多层手动调度）、`[CTRL-PARITY-001]`

## 已实现事实

1. `nlos-system-control` 新增 crate 内类型面 `control` 模块：有界枚举 `ControlCommand`（`InspectHealth`、`InspectTask { plan_id }`、`AcknowledgeRecoveryAlert { control_command_id, plan_id, expected_total_failures, reason }`）。读取命令复用既有 `get` 快照；mutation 命令是该 handler 已落地的唯一真实控制能力（TaskAuthority CAS 确认 escalation alert），未发明任何未实现的控制语义。该类型面不进入 schema REGISTRY，不触碰 wire 冻结面。
2. 新增 typed 回执 `ControlReceipt`（`control_command_id` + `correlation_id` + `Result<ControlOutcome, SabiFailure>`）：成功侧给出有界 `RecoveryInspection`（worker lifecycle、durable gauges、逐 alert 摘要）或 authoritative `receipt_id`；失败侧原样沿用 handler 产生的脱敏 `SabiFailure`（code/retry/safe_message），回执层不制造也不升级任何成功证据。`to_bytes()` 为确定性有界编码，是"逐字节等价"契约的比较对象。
3. 统一 dispatch、无第二条控制路径：`build_request_envelope` 是唯一命令编译点（in-process 与 CLI 共用）；`dispatch_in_process` 直接调用既有 `RecoverySystemControl::handle_for_ipc`，`dispatch_over_socket`（feature `cli`，Unix socket）经真实 IPC 到达同一 handler；`ControlReceipt::compose` 是唯一回执投影点。CLI/库/in-process 三入口由构造保证等价。
4. 新增 crate 内 `[[bin]]` `system-control-cli`（feature `cli`，`default = ["cli"]`）：手工参数解析（无新增第三方依赖），支持 `inspect-health`、`inspect-task <PLAN_ID_HEX_32>`、`ack-recovery-alert <COMMAND_ID_HEX_32> <PLAN_ID_HEX_32> <EXPECTED_FAILURES> <REASON>`；stdout 首行输出 `RECEIPT <hex>`（`to_bytes` 确定性编码）+ 一行人类可读摘要；退出码 0=成功回执、1=typed 失败回执、2=用法/传输错误。Windows named-pipe CLI 适配不在本前缀内（bin 在非 Unix 平台 fail-closed）。
5. 集成测试 `tests/control_command_cli.rs` 证明等价路径：真实 `UnixListenerAdapter` socket + `serve_one` + `handle_for_ipc` 服务端下，inspect-health 与 acknowledge 各产生 direct in-process、library-over-socket、CLI 子进程三份回执，`to_bytes()` 断言逐字节相等（acknowledge 首调 + 同 idempotency key replay 均等价）；CLI denied reason 场景退出码 1、`outcome=failure code=3(RIGHTS)`、回执字节与 in-process 拒绝一致；inspect-task 对缺失 plan 返回 typed `NOT_FOUND`；durable 后置断言确认 alert 已被确认且 exact replay 不重复确认。
6. 回执层脱敏失败沿用 SabiFailure：policy 拒绝只回 `RIGHTS + DO_NOT_RETRY + "SystemControl authorization denied"`，测试断言拒绝原因文本与本地诊断不跨边界，且 denied 命令无 durable acknowledgement。
7. 身份姿态：本地信任域占位常量（`LOCAL_ISSUER_PRINCIPAL_ID` 等）+ capability handle `{slot:9, generation:1}` 仅为请求侧载体；服务端 `SystemControlAuthorizer` 仍是唯一 policy 边界。这与 ADR-0011 认证实现线并行不冲突。

## 验证

验证环境：macOS（darwin，arm64），Rust stable（cargo 1.97.1），仓库 HEAD `b0badd5`。本节为 2026-08-29 接管代理在共享主工作区（HEAD `b0badd5` 工作树，叠加并行车道未提交改动）的复跑与补验结果；前代理首验在 HEAD `4f90511` 独立 worktree 完成。

- `cargo test -p nlos-system-control`：**23 passed / 0 failed**（lib 单测 5、bin 单测 0、`control_command_cli` 3、`metrics_export_contract` 3、`recovery_control` 7、`system_control_failure_mapping` 5、`windows_named_pipe` 0（macOS 目标）、doc-tests 0）。共享工作区最终文件状态直跑全绿。
- `cargo clippy -p nlos-system-control --all-targets -- -D warnings`：本写集 0 warning / 0 error。带 `-D warnings` 的完整门在共享工作区被 `nlos-channel`（2）与 `nlos-process`（2）的并行车道未提交改动阻断（redundant closure / doc backticks / 函数超长，均在本写集之外，依规不代改）；已在 `b0badd5` 快照 + 仅叠加本写集的隔离副本（无 .git，独立 `CARGO_TARGET_DIR`）中复跑同命令，**通过**。
- `cargo fmt -p nlos-system-control`：已执行；`cargo fmt -p nlos-system-control -- --check` 在共享工作区与隔离副本均通过。

### 接管期修复（2026-08-29）

`tests/control_command_cli.rs` 的 `serve_forever` harness 存在固有时序竞态：`UnixListenerAdapter::accept` 受 `TransportConfig::default()` 的 5s connect timeout 约束，原实现把任何 accept 错误（含空闲超时）都当作退役信号 → listener 被 drop → 较慢启动的 CLI 子进程（macOS 首次执行加固扫描可超过 5s）connect 得到 `ECONNREFUSED`。共享工作区复跑时该用例稳定复现失败。修复：accept 空闲超时改为 `continue`，单次 `serve_one` 失败不再终结端点，仅 listener 硬错误退出循环；同时消除该修复引入的 clippy `redundant continue` warning。修复后共享工作区与隔离副本多轮全绿。

## 验证与边界

本证据为单节点本地 H3 / `PARTIAL PASS`。已知限制：

1. **NL/GUI 路径未接**：本前缀只交付 CLI + 库入口；`[CTRL-PARITY-001]` 的 NL Shell / Trusted GUI 编译与确认面仍未实现（ROAD-B-005 不因此达成）。
2. **控制操作面极小**：仅 2 个 inspect + 1 个 acknowledge；§25.3 表格中的 start/pause/resume/retry、Process/Fiber 级 kill、Topic purge 等能力均未落地。
3. **无鉴权**：CLI 身份为固定占位常量，capability handle 只是请求侧载体；真实认证、Principal attestation 与 peer policy 由 ADR-0011 实现线并行推进。
4. **CLI 传输仅 Unix socket**；Windows named-pipe CLI 适配、ServiceDirectory 动态协商解析、trusted-clock anti-replay、批量控制与 `UNKNOWN` 查询语义均未包含。
5. `ControlCommand` 是 crate 内类型面，不进 schema REGISTRY；`to_bytes()` 是测试/CLI 等价性比较编码，不是冻结 wire 格式。
