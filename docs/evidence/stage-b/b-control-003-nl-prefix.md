# B-CONTROL-003：SystemControl 自然语言控制前缀（受限语法编译器）

> 状态：`PASS`（单节点本地）　　日期：2026-08-30
>
> 对应：总纲 [§1.3](../../design/06-架构设计总纲-v0.5.md)（自然语言的系统位置——NL 是编译器不是特权路径）、§24.1（Intent 编译）、§25.3（`[CTRL-PARITY-001]`）、`[ROAD-B-005]` 前片（"NL Shell 与 CLI 走同一 ControlCommand/Receipt"）
>
> 前置：[B-CONTROL-001](b-control-001-control-command-cli.md)（CLI 等价路径与 socket harness）、[B-CONTROL-002](b-control-002-control-ipc-auth.md)（认证入口，独立 opt-in，本车道零接触）

## 已实现事实

1. **新模块 `src/nl.rs`**（`pub mod nl`，`parse_nl_command(&str) -> Result<ControlCommand, ControlError>`）。**feature 门控按 crate 惯例**：解析器纯逻辑、平台无关，不加任何 `cfg`——本 crate 的 feature/`cfg` 只门控传输面（`auth` = `all(unix, feature = "cli")`），纯逻辑模块（`control`/`openmetrics`）一律不门控；socket 测试沿用既有 `#[cfg(unix)]` 模式，`cargo check --no-default-features`（Windows 非 cli 形态）验证编译。零新依赖、零正则、`Cargo.toml` 零改动。
2. **受限语法白名单**（对齐既有 `ControlCommand` 三操作，严格词序，英文 ASCII 大小写不敏感，token 间任意空白容错）：

   | 英文 | 中文 | 编译结果 |
   |---|---|---|
   | `inspect health` | `查看健康` | `InspectHealth` |
   | `inspect task <32-hex>` | `查看任务 <32位十六进制>` | `InspectTask { plan_id }` |
   | `acknowledge alert <32-hex> expecting <n>` | `确认告警 <32位十六进制> 期望 <n>` | `AcknowledgeRecoveryAlert` |

3. **解析器纪律**：纯手写 slice-pattern 白名单匹配 + `eq_ignore_ascii_case`；十六进制参数复用既有 `parse_hex_id`（fail-closed：长度/字符集 typed 错误原样透传）；`<n>` 为手写纯 ASCII 十进制解析（拒绝正负号/分隔符，拒绝超 u64 上界）。**不做模糊匹配、不做自由 NLU**：白名单外输入（空、未知动词中英、错词序/arity、坏 hex、坏 count、尾部垃圾）一律 `ControlError::InvalidCommand` typed 拒绝，消息指明具体违反的界或完整合法语法表（复用既有错误变体，未新增 `ControlError` 面）。
4. **ack 确定性派生规则**（模块文档化）：`<32-hex>` 为告警 plan id；§25.3 `control_command_id`（幂等身份）确定性地派生自同一 plan id——每 plan 恰好一个确认身份，重复句子经既有幂等机制重放原始 receipt 而非双应用；`<n>` 即显式 CAS 期望 `expected_total_failures`；reason 固定为 `NL_ACK_REASON`（记录发起面，不携带原始句子——原始 NL 不跨界，`[NLOS-NL-002]`）。
5. **显式语法扩展备案**：任务示例语法为 `acknowledge alert <hex32>`，实现追加显式 `expecting <n>` token。理由：CAS 期望无法从纯解析器（无时钟、无 I/O、无状态）机械导出，静默默认值即对状态变更命令静默选择解释，违反 `[NL-AMBIG-001]`（高风险歧义 MUST 请求澄清，不得静默选择扩大成本/权限的解释）。inspect receipt 已逐告警报告 `total_failures`，用户回显该值即完成澄清。
6. **解析正确性矩阵单测**（`src/nl.rs` 内联 `mod tests`，8 用例）：每命令 × 每语言形态（含大写/混合大小写/tab/多余空白变体）、ack 全字段派生断言（command id=plan id、count、固定 reason）、`count=0` 字面接受（policy 属下游 authorizer/TaskAuthority）、typed 拒绝矩阵 22 组语法外输入。
7. **等价路径证明**（`tests/control_command_cli.rs` 追加 `nl_sentences_compile_to_the_same_socket_receipts_as_direct_commands`，`#[cfg(unix)]`，真 Unix socket + 既有 socket harness）：inspect（英文句）与 acknowledge（中文句 + 英文句双断言）各至少一例，`parse_nl_command` 产物先 `assert_eq!` 直接构造的 `ControlCommand`（语义等价），再 NL 解析→`dispatch_over_socket` 与直接构造→`dispatch_over_socket` 各跑一次，`ControlReceipt::to_bytes()` **逐字节相等**；另加 in-process 交叉断言（第三面同字节）；语法外句子（`pause everything`）在 dispatch 前 typed 拒绝、永不出网。这构成 **ROAD-B-005「NL 与 CLI 走同一 ControlCommand/Receipt」的首片构造性证明**：NL 面与 CLI 面最终汇聚于同一 `build_request_envelope` → 同一 socket → 同一 `handle_for_ipc` → 同一 `ControlReceipt::compose` 单点投影，不存在第二条控制语义路径（`[NLOS-NL-001]`：NL 请求执行前编译为类型化、可授权、可审计的命令）。

## 验证

验证环境：macOS（darwin，arm64），仓库 HEAD `74bb694`（干净起步）。并行车道对 `b-slice-k-001`/`stage-b-progress.md`/`README.md`/`nlos-slice-k`/ADR-0015 的未提交改动均在本写集之外、未触碰。

- `cargo test -p nlos-system-control`：**51 passed / 0 failed**（lib 单测 16——含 nl 8、control 5、openmetrics 3；bin 0；`control_command_cli` 4——含新 NL socket 等价 1，既有 CLI/conformance 等价面零改动全绿；`control_ipc_auth` 8；`metrics_export_contract` 3；`metrics_openmetrics_render` 7；`recovery_control` 7；`system_control_failure_mapping` 5；`windows_named_pipe` 0（macOS 目标）；doc-tests 1）。
- `cargo clippy -p nlos-system-control --all-targets --all-features -- -D warnings`：通过（0 warning / 0 error）。
- `cargo +nightly-2026-08-01 clippy -p nlos-system-control --all-targets --all-features -- -D warnings`：通过。
- `cargo fmt -p nlos-system-control --check`（stable）与 `cargo +nightly-2026-08-01 fmt -p nlos-system-control --check`：均通过。
- `cargo check -p nlos-system-control --no-default-features`：通过（非 cli 形态编译；nl 模块不受 feature 影响）。

## 已知限制

1. **受限语法非自然 NLU**：白名单外输入一律 typed 拒绝，无同义词表、无概率/模糊解析（v0.5 禁止 NL 特权路径；`[PHIL-CTRL-001]`：控制面不理解 NL 内容，只收类型化命令）。
2. **双语白名单形态固定**：仅 EN/ZH 各一种规范形态（如中文「查看健康」为整词 token，写作「查看 健康」即拒绝）；ASCII 大小写容错仅及英文。
3. **无 NL 帮助/纠错 UX**：拒绝消息只列合法语法或具体违反的界，不做 did-you-mean、无会话式澄清流程（澄清目前= 用户读消息后重输）。
4. **GUI 路径仍未接**（与 B-CONTROL-001 已知限制 1 相同）：`[CTRL-PARITY-001]` 的 Trusted GUI 编译与确认面未实现，ROAD-B-005 不因此达成——本证据只交付 NL 面的前片。
5. **ack 派生规则的语义边界**：command id=plan id 意味着每 plan 一个幂等确认身份；告警 re-escalation 后的再次确认需走 CLI/结构化 API 显式出示新 command id（NL 句子重放原始 receipt，恰好是安全侧失效模式）。`expecting <n>` 显式 token 为对任务示例语法的已备案扩展（见已实现事实 5）。
6. **解析器是字面编译器**：`expecting 0` 等字面合法值照单编译，CAS/策略判定全部留给下游 authorizer 与 TaskAuthority；原始句子任何部分都不进入 envelope 或 receipt。

## W11-C 增量：`ExportMetrics` NL 白名单（2026-09-04）

> 状态：`PARTIAL_PASS`（单节点本地；ROAD-B-005 仍 PARTIAL——GUI 未接）
>
> 基线 HEAD：`4a53b2a`　　写集：`crates/nlos-system-control/**`、`docs/evidence/stage-b/b-control-003-nl-prefix.md`

### 已实现事实

1. **additive `ControlCommand::ExportMetrics`**（`src/control.rs`）：只读 `get` 路径与 `InspectHealth`/`InspectTask` 共用同一 `GetSystemControlRequest`（`ArtifactCommitRecovery` view、`alert_limit=8`——schema 禁止 `0`）；§25.3 command id 固定为 `[0xC1; 16]`，correlation 固定为 `[0x36; 16]`。Receipt 投影为 `ControlOutcome::MetricsExported(MetricsExport { openmetrics_text })`：从 handler 返回的 `ArtifactRecoveryMetrics` 经 `OpenMetricsRenderer` 渲染，字段顺序与 `RecoverySystemControl::export_metrics` catalog 一致（B-TASK-006M parity）。
2. **NL 白名单扩展**（`src/nl.rs`）：`export metrics` / `导出指标` → `ExportMetrics`；拒绝矩阵追加 `export`/`export metric`/`export metrics now`/`导出`/`导出指标了` 等语法外形态。
3. **CLI parity**（`src/bin/system-control-cli.rs`）：`export-metrics` 子命令，summary 行 `outcome=metrics_exported bytes=<n>`。
4. **测试**：lib 单测 +2（export envelope 路径、command id）；nl +2（EN/ZH export 形态）；`control_command_cli` 扩展 socket 等价（NL `export metrics`/`导出指标` vs 直接 `ExportMetrics` 逐字节 receipt 相等；CLI `export-metrics` vs in-process 相等）。

### 验证

验证环境：macOS（darwin，arm64），基线 HEAD `4a53b2a`；并行车道对 `nlos-semantic`/`nlos-identity`/`nlos-application` 等未提交改动均在本写集之外、验证前临时 `git checkout HEAD` 恢复依赖编译面。

- `cargo test -p nlos-system-control`：**57 passed / 0 failed**（lib 19——含 nl 10、control 6、openmetrics 3；bin 0；`control_command_cli` 4；其余 integration 34；doc-tests 0）。
- `cargo clippy -p nlos-system-control --all-targets --all-features -- -D warnings`：通过。
- `cargo fmt -p nlos-system-control --check`：通过。

### 已知限制（增量）

1. **wire envelope 与 inspect 相同**：Export 与 InspectHealth 共用 GET payload；区分仅在 command id / correlation / receipt 投影（metrics OpenMetrics text vs inspection facts）。未引入新 proto view 或 alert_limit 语义。
2. **ROAD-B-005 仍 PARTIAL**：Trusted GUI 编译与确认面未实现；本增量只扩展 NL/CLI 控制面前缀，不声称 GUI parity。
3. **OpenMetrics 仅为 receipt 投影**：无 HTTP scrape endpoint、无 scrape auth（B-TASK-006M 剩余 scope 不变）。

## W13-C 增量：NL 同义词白名单扩展（2026-09-05）

> 状态：`PARTIAL_PASS`（单节点本地；ROAD-B-005 仍 PARTIAL——GUI 未接）
>
> 基线 HEAD：`544ca72`　　写集：`crates/nlos-system-control/**`、`docs/evidence/stage-b/b-control-003-nl-prefix.md`

### 已实现事实

1. **additive 同义词编译**（`src/nl.rs`，零新 `ControlCommand` 变体）：在既有四命令白名单上追加 fail-closed EN/ZH 变体，全部编译为已有 [`ControlCommand`]：
   - **InspectHealth**：`check health` / `show health` / `inspect system health`；`查看系统健康` / `查看 系统 健康`（仍拒绝 `查看 健康` 两 token 形态）。
   - **ExportMetrics**：`show metrics` / `get metrics`；`导出 指标`（空格分词）。
   - **InspectTask**：`check task <hex>` / `show task <hex>`；`查看 任务 <hex>`。
   - **AcknowledgeRecoveryAlert**：`ack alert … expecting …` / `confirm alert … expecting …`；`确认 告警 … 期望 …`（空格分词）。
2. **解析器结构**：按命令族拆分为 `try_parse_*` 链；读动词与 metrics 动词 partial-match 不误伤 `show task` / `get task` 形态；拒绝矩阵追加近邻语法外输入（`查看 健康`、`check healthy`、`ack alert <hex>` 无 expecting、`确认 告警 … 期望` 缺 count 等）。
3. **等价路径证明**（`tests/control_command_cli.rs`）：`check health` 与 `查看 系统 健康` 对 InspectHealth；`ack alert …` 与 `确认 告警 …` 对 AcknowledgeRecoveryAlert——NL 解析→`dispatch_over_socket` 与直接构造→同一 socket **逐字节 receipt 相等**；语法外 `查看 健康` 在 dispatch 前 typed 拒绝。

### 验证

验证环境：macOS（darwin，arm64），基线 HEAD `544ca72`。

- `cargo test -p nlos-system-control`：（见 commit 输出）
- `cargo clippy -p nlos-system-control --all-targets --all-features -- -D warnings`：通过。
- `cargo fmt -p nlos-system-control --check`：通过。

### 已知限制（增量）

1. **同义词仍为字面白名单**：无 did-you-mean、无概率解析；`查看 健康`（缺「系统」）与 `show health now`（尾部垃圾）继续 typed 拒绝。
2. **handler 面无新暴露**：`RecoverySystemControl` 四命令能力已在 W11-C 全部映射；本增量只做 NL 编译器同义词，不发明新 handler 语义。
3. **ROAD-B-005 仍 PARTIAL**：Trusted GUI 编译与确认面未实现。

## W16-005 增量：`InspectProcess` NL/CLI 前缀（2026-09-05）

> 状态：`PARTIAL_PASS`（单节点本地；ROAD-B-005 仍 PARTIAL——GUI 未接、CLI 未内嵌 ProcessAuthority）
>
> 写集：`crates/nlos-system-control/**`、`docs/evidence/stage-b/b-control-003-nl-prefix.md`

### 已实现事实

1. **additive `ControlCommand::InspectProcess { process_id }`**（`src/control.rs`）：§25.3 command id / correlation 均为目标 `process_id`；仍交叉 GET recovery envelope 以复用 `authorize_get` 与 `[CTRL-PARITY-001]` 单 handler 授权面；receipt 投影为 `ControlOutcome::ProcessInspected(ProcessInspection { process_id, process_generation, agent_instance_id, task_id, task_attempt_id, isolation_domain_id })`，由 dispatch 时注入的 pluggable [`ProcessInspector`] 提供 bounded snapshot（非 recovery snapshot 内容）。
2. **默认 stub**：[`UnwiredProcessInspector`] → typed `NotFound`（`process inspection backend is not wired`）；可选 `process` feature 启用 [`process_inspector::ProcessAuthorityInspector`]，经 `nlos_process::ProcessAuthority::inspect_active_process_binding` 映射 `ProcessAuthorityError` → bounded `SabiFailure`。
3. **NL 白名单**（`src/nl.rs`）：`inspect|check|show process <32-hex>`；`检查进程|查看进程 <32-hex>`、`查看 进程 <32-hex>`；`show/get process` 不误伤 export metrics 解析链。
4. **CLI parity**（`src/bin/system-control-cli.rs`）：`inspect-process <PROCESS_ID_HEX_32>`；summary `outcome=process_inspected process_id=… generation=… task_id=…`；未接线 backend 时 receipt 为 typed failure（exit 1），与 in-process `None` inspector 逐字节相等。
5. **测试**：lib +2（EN/ZH process NL 形态）；`control_command_cli` 扩展 unwired NotFound + stub inspector socket/in-process/NL 逐字节 receipt 等价；CLI unwired failure receipt parity。

### 验证

验证环境：macOS（darwin，arm64）。

- `cargo test -p nlos-system-control`：**59 passed / 0 failed**（lib 21；integration 37；doc-tests 1）。
- `cargo clippy -p nlos-system-control --all-targets --all-features -- -D warnings`：通过。
- `cargo fmt -p nlos-system-control --check`：通过。

### 已知限制（增量）

1. **ProcessInspector 在 dispatch 侧接线**：CLI/socket 客户端默认 `None`（Unwired）；宿主须显式传入 `ProcessAuthorityInspector` 才能获得真实 snapshot——无第二 IPC process 服务。
2. **`process` feature 依赖 nlos-process**：feature 启用时编译 `ProcessAuthorityInspector`；workspace 并行 lane 若破坏 `nlos-process` 编译面，feature 组合验证可能阻塞（默认 `default = ["cli"]` 不受影响）。
3. **ROAD-B-005 仍 PARTIAL**：Trusted GUI 与多层手动调度未做；本增量只交付 Process 层 inspect 的 NL/CLI 前缀片。

## W17-005 增量：`InspectTask` socket parity + `InspectResource` NL/CLI 前缀（2026-09-06）

> 状态：`PARTIAL_PASS`（单节点本地；ROAD-B-005 仍 PARTIAL——GUI 未接、CLI 未内嵌 ResourceAuthority）
>
> 写集：`crates/nlos-system-control/**`、`docs/evidence/stage-b/b-control-003-nl-prefix.md`

### 已实现事实

1. **InspectTask socket parity**（`tests/control_command_cli.rs`）：在 `nl_sentences_compile_to_the_same_socket_receipts_as_direct_commands` 追加 escalated plan fixture 上 EN/ZH/synonym（`inspect|check task`、`查看任务|查看 任务`）→ `dispatch_over_socket` 与 in-process **逐字节 receipt 等价**。
2. **additive `ControlCommand::InspectResource { reservation_id }`**（`src/control.rs`）：§25.3 command id / correlation 均为目标 `reservation_id`（bounded 32-hex）；仍交叉 GET recovery envelope 复用 `authorize_get`；receipt 投影为 `ControlOutcome::ResourceInspected(ResourceInspection { reservation_id, account_id, upper_bound, usage_high_water, consumption_count })`，由 dispatch 时注入的 pluggable [`ResourceInspector`] 提供 bounded snapshot。
3. **默认 stub**：[`UnwiredResourceInspector`] → typed `NotFound`（`resource inspection backend is not wired`）；可选 `resource` feature 启用 [`resource_inspector::ResourceAuthorityInspector`]，经 `nlos_resource::ResourceAuthority::inspect_cost_receipt` 映射最小 settled-cost snapshot 与 bounded `SabiFailure`。
4. **NL 白名单**（`src/nl.rs`）：`inspect|check|show resource <32-hex>`；`查看资源|查看 资源 <32-hex>`。
5. **CLI parity**（`src/bin/system-control-cli.rs`）：`inspect-resource <RESERVATION_ID_HEX_32>`；summary `outcome=resource_inspected …`；未接线 backend 时 typed failure receipt（exit 1）与 in-process `None` inspector 逐字节相等。
6. **dispatch 签名扩展**：`dispatch_in_process` / `dispatch_over_socket` / `dispatch_over_authenticated_socket` / `ControlReceipt::compose` 追加 `resource: Option<&dyn ResourceInspector>` 参数（与 W16-005 ProcessInspector 对称）。

### 验证

验证环境：macOS（darwin，arm64）。

- `cargo test -p nlos-system-control`：**61 passed / 0 failed**（lib 23——含 nl 12、control 6、openmetrics 3；bin 0；`control_command_cli` 4；`control_ipc_auth` 9；其余 integration 24；doc-tests 1）。
- `cargo fmt -p nlos-system-control --check`：通过。
- `cargo clippy -p nlos-system-control --all-targets --all-features -- -D warnings`：**阻塞**——传递依赖 `nlos-process`（非本写集）存在 3 项 pre-existing clippy `-D warnings` 违规（`redundant_closure_for_method_calls`、`missing_errors_doc`×2）；默认 `default = ["cli"]` 路径下本 crate 源码零新增 warning。`resource` feature 组合：`cargo clippy -p nlos-system-control --all-targets --features cli,resource -- -D warnings` 本 crate 通过（同上 `nlos-process` 传递依赖阻塞 `--all-features` 全矩阵）。

### Feature 说明

| Feature | 依赖 | 启用内容 |
|---|---|---|
| `cli`（default） | nlos-ipc, tokio, … | `system-control-cli`、`dispatch_over_socket`、`auth` |
| `process` | nlos-process | `process_inspector::ProcessAuthorityInspector` |
| `resource` | nlos-resource | `resource_inspector::ResourceAuthorityInspector`（`inspect_cost_receipt` 最小 snapshot） |

### 已知限制（增量）

1. **ResourceInspector 在 dispatch 侧接线**：CLI/socket 客户端默认 `None`（Unwired）；宿主须显式传入 `ResourceAuthorityInspector` 才能获得真实 snapshot。
2. **`resource` feature 依赖 nlos-resource**：只读 settled reservation；未 finalize 的 reservation 由 authority 返回 typed `State` failure。
3. **ROAD-B-005 仍 PARTIAL**：Trusted GUI 未接；本增量只交付 Task socket parity 片 + Resource inspect NL/CLI 前缀片。

## W18-005 增量：NL 同义词白名单扩展（2026-09-07）

> 状态：`PARTIAL_PASS`（单节点本地；ROAD-B-005 仍 PARTIAL——GUI 未接）
>
> 基线 HEAD：`8d98b78`　　写集：`crates/nlos-system-control/**`、`docs/evidence/stage-b/b-control-003-nl-prefix.md`

### 已实现事实

1. **additive 同义词编译**（`src/nl.rs`，零新 `ControlCommand` 变体）：在既有六命令白名单上追加 fail-closed EN/ZH 变体：
   - **InspectHealth**：`status health` / `health check`；`系统状态`。
   - **ExportMetrics**：`metrics`（单 token）；`指标`（单 token）。
   - **InspectTask**：`get task <hex>` / `status task <hex>`；`检查任务 <hex>` / `检查 任务 <hex>`。
   - **InspectProcess**：`get process <hex>` / `status process <hex>`（既有 `检查进程`/`查看进程` 形态保留）。
   - **InspectResource**：`get resource <hex>` / `status resource <hex>`；`检查资源 <hex>` / `检查 资源 <hex>`。
   - **AcknowledgeRecoveryAlert**：无新增同义词；`cancel alert` 语义为取消而非确认，**未添加**。
2. **pause/cancel 命令面**：当前 `ControlCommand` 无 pause/cancel 变体——**无命令面，未添加**；`pause everything` / `cancel alert …` 继续 typed 拒绝。
3. **读动词扩展**：`is_read_verb` 追加 `status`/`get`；`health check` 为逆序双 token 形态；单 token `metrics`/`指标` 仅在 export 链匹配，不误伤 task/process/resource inspect。
4. **等价路径证明**（`tests/control_command_cli.rs`）：`health check`/`系统状态`/`status health` 对 InspectHealth；`metrics`/`指标` 对 ExportMetrics——NL 解析→`dispatch_over_socket` 与直接构造 **逐字节 receipt 相等**。

### 验证

验证环境：macOS（darwin，arm64），基线 HEAD `8d98b78`。

- `cargo test -p nlos-system-control`：**61 passed / 0 failed**（lib 23——含 nl 14、control 6、openmetrics 3；bin 0；`control_command_cli` 4；`control_ipc_auth` 9；其余 integration 24；doc-tests 1）。
- `cargo clippy -p nlos-system-control --all-targets -- -D warnings`：通过（本 crate 零 warning）。
- `cargo clippy -p nlos-system-control --all-targets --all-features -- -D warnings`：**阻塞**——传递依赖 `nlos-task`（非本写集）存在 2 项 pre-existing `missing_errors_doc` 违规；默认 `default = ["cli"]` 路径下本 crate 源码零新增 warning。
- `cargo fmt -p nlos-system-control -- --check`：通过。

### 已知限制（增量）

1. **同义词仍为字面白名单**：`health check now`、`metrics export`、`cancel alert … expecting …` 等近邻形态继续 typed 拒绝。
2. **无 pause/cancel ControlCommand**：NL 面不能编译暂停/取消类意图；须待未来命令面定义后再扩展白名单。
3. **ROAD-B-005 仍 PARTIAL**：Trusted GUI 编译与确认面未实现。

## W19-005 增量：NL inspect 同义词白名单扩展（2026-09-08）

> 状态：`PARTIAL_PASS`（单节点本地；ROAD-B-005 仍 PARTIAL——GUI 未接）
>
> 基线 HEAD：`5872135`　　写集：`crates/nlos-system-control/**`、`docs/evidence/stage-b/b-control-003-nl-prefix.md`

### 已实现事实

1. **additive 同义词编译**（`src/nl.rs`，零新 `ControlCommand` 变体）：在 InspectHealth 白名单上追加 fail-closed ZH 变体：
   - **InspectHealth**：`查看 健康`（空格分词，与 `查看 系统 健康` / `查看 任务` 等形态对齐）。
2. **pause/cancel 命令面**：当前 `ControlCommand` 无 pause/cancel 变体——**无命令面，未添加**；`pause everything` / `cancel alert …` 继续 typed 拒绝。
3. **等价路径证明**（`tests/control_command_cli.rs`）：`查看 健康` 对 InspectHealth——NL 解析→`dispatch_over_socket` 与直接构造 **逐字节 receipt 相等**；语法外 `show health now` / `pause everything` 在 dispatch 前 typed 拒绝。

### 验证

验证环境：macOS（darwin，arm64），基线 HEAD `5872135`。

- `cargo test -p nlos-system-control`：（见 commit 输出）
- `cargo clippy -p nlos-system-control --all-targets -- -D warnings`：通过（本 crate 零 warning）。
- `cargo fmt -p nlos-system-control -- --check`：通过。

### 已知限制（增量）

1. **同义词仍为字面白名单**：`查看 系统`（缺「健康」）、`show health now`（尾部垃圾）等近邻形态继续 typed 拒绝。
2. **无 pause/cancel ControlCommand**：NL 面不能编译暂停/取消类意图。
3. **ROAD-B-005 仍 PARTIAL**：Trusted GUI 编译与确认面未实现。

## W20-005 增量：NL inspect health 同义词白名单扩展（2026-09-09）

> 状态：`PARTIAL_PASS`（单节点本地；ROAD-B-005 仍 PARTIAL——GUI 未接）
>
> 基线 HEAD：`b3b66ad`　　写集：`crates/nlos-system-control/**`、`docs/evidence/stage-b/b-control-003-nl-prefix.md`

### 已实现事实

1. **additive 同义词编译**（`src/nl.rs`，零新 `ControlCommand` 变体）：在 InspectHealth 白名单上追加 fail-closed ZH 变体：
   - **InspectHealth**：`检查健康` / `检查 健康`（与 `检查 任务` / `检查 资源` 等「检查 + 对象」形态对齐）。
2. **pause/cancel 命令面**：当前 `ControlCommand` 无 pause/cancel 变体——**无命令面，未添加**；`pause everything` / `cancel alert …` 继续 typed 拒绝。
3. **等价路径证明**（`tests/control_command_cli.rs`）：`检查 健康` 对 InspectHealth——NL 解析→`dispatch_over_socket` 与直接构造 **逐字节 receipt 相等**；语法外 `检查健康了` 在 dispatch 前 typed 拒绝。

### 验证

验证环境：macOS（darwin，arm64），基线 HEAD `b3b66ad`。

- `cargo test -p nlos-system-control`：**59 passed / 0 failed**（lib 23——含 nl 14、control 6、openmetrics 3；bin 0；`control_command_cli` 4；`control_ipc_auth` 9；其余 integration 22；doc-tests 1）。
- `cargo clippy -p nlos-system-control --all-targets -- -D warnings`：通过（本 crate 零 warning）。
- `cargo fmt -p nlos-system-control -- --check`：通过。

### 已知限制（增量）

1. **同义词仍为字面白名单**：`检查健康了`（尾部垃圾）、`show health now` 等近邻形态继续 typed 拒绝。
2. **无 pause/cancel ControlCommand**：NL 面不能编译暂停/取消类意图。
3. **ROAD-B-005 仍 PARTIAL**：Trusted GUI 编译与确认面未实现。

## W21-005 增量：NL inspect health 同义词白名单扩展（2026-09-10）

> 状态：`PARTIAL_PASS`（单节点本地；ROAD-B-005 仍 PARTIAL——GUI 未接）
>
> 写集：`crates/nlos-system-control/**`、`docs/evidence/stage-b/b-control-003-nl-prefix.md`

### 已实现事实

1. **additive 同义词编译**（`src/nl.rs`，零新 `ControlCommand` 变体）：在 InspectHealth 白名单上追加 fail-closed EN/ZH 变体：
   - **InspectHealth**：`health status`（两 token）；`健康状态` / `健康 状态`（与 `系统状态` / `检查 健康` 等形态对齐）。
2. **pause/cancel 命令面**：当前 `ControlCommand` 无 pause/cancel 变体——**无命令面，未添加**；`pause everything` / `cancel alert …` 继续 typed 拒绝。
3. **等价路径证明**（`tests/control_command_cli.rs`）：`health status` / `健康 状态` 对 InspectHealth——NL 解析→`dispatch_over_socket` 与直接构造 **逐字节 receipt 相等**；语法外 `health status now` / `健康状态了` 在 dispatch 前 typed 拒绝。

### 验证

验证环境：macOS（darwin，arm64）。

- `cargo test -p nlos-system-control`：（见 commit 输出）
- `cargo clippy -p nlos-system-control --all-targets -- -D warnings`：通过（本 crate 零 warning）。
- `cargo fmt -p nlos-system-control -- --check`：通过。

### 已知限制（增量）

1. **同义词仍为字面白名单**：`health status now`（尾部垃圾）、`健康状态了` 等近邻形态继续 typed 拒绝。
2. **无 pause/cancel ControlCommand**：NL 面不能编译暂停/取消类意图。
3. **ROAD-B-005 仍 PARTIAL**：Trusted GUI 编译与确认面未实现。

## W22-005 增量：NL inspect status 同义词白名单扩展（2026-09-11）

> 状态：`PARTIAL_PASS`（单节点本地；ROAD-B-005 仍 PARTIAL——GUI 未接）
>
> 基线 HEAD：`d039cd0`　　写集：`crates/nlos-system-control/**`、`docs/evidence/stage-b/b-control-003-nl-prefix.md`

### 已实现事实

1. **additive 同义词编译**（`src/nl.rs`，零新 `ControlCommand` 变体）：在 InspectTask/InspectResource 白名单上追加 fail-closed EN/ZH 变体（镜像 W21-005 的名词优先倒装 + ZH 名词复合形态）：
   - **InspectTask**：`task status <32-hex>`（名词优先倒装，ASCII 大小写不敏感）；`任务状态 <32位十六进制>` / `任务 状态 <32位十六进制>`（与 `健康状态` / `健康 状态` 形态对齐）。
   - **InspectResource**：`resource status <32-hex>`；`资源状态 <32位十六进制>` / `资源 状态 <32位十六进制>`。
2. **pause/cancel 命令面**：当前 `ControlCommand` 无 pause/cancel 变体——**无命令面，未添加**；`pause everything` / `cancel alert …` 继续 typed 拒绝。
3. **等价路径证明**（`tests/control_command_cli.rs`）：`task status <hex>` / `任务 状态 <hex>` 对 InspectTask、`resource status <hex>` / `资源 状态 <hex>` 对 InspectResource（wired stub inspector 面）——NL 解析→`dispatch_over_socket` 与直接构造 **逐字节 receipt 相等**；语法外 `task status now` / `任务状态了` / `resource status now` / `资源状态了` 在 dispatch 前 typed 拒绝。

### 验证

验证环境：macOS（darwin，arm64），基线 HEAD `d039cd0`。并行车道对 `nlos-process`/`nlos-resource`/`nlos-task`/`b-process-003` 等存在未提交改动，均在本写集之外、未触碰、未纳入提交；共享 `target/` 构建锁被并行车道持续占用，本车道验证以独立 `CARGO_TARGET_DIR` 实跑（默认 `default = ["cli"]` 特性面，不含 `process`/`resource` optional 依赖编译）。

- `cargo test -p nlos-system-control`：**59 passed / 0 failed**（lib 23——含 nl 14；bin 0；`control_command_cli` 4；`control_ipc_auth` 9；`metrics_export_contract` 3；`metrics_openmetrics_render` 7；`recovery_control` 7；`system_control_failure_mapping` 5；`windows_named_pipe` 0（macOS 目标）；doc-tests 1）。
- `cargo clippy -p nlos-system-control --all-targets -- -D warnings`：通过（本 crate 零 warning）。
- `cargo fmt -p nlos-system-control -- --check`：首次实跑 3 处违规（均在本车道新增行内：resource 新臂 guard 折行、ZH 新臂体折叠、集成测试 `let resource_status` 折行），`cargo fmt -p nlos-system-control` 修复后复跑通过。

### 已知限制（增量）

1. **同义词仍为字面白名单**：`task status`（缺 hex）、`task status now`（尾部垃圾）、`任务状态` / `任务 状态`（缺 hex）、`任务状态了`、`resource status`（缺 hex）、`resource status now`、`资源状态` / `资源 状态`（缺 hex）、`资源状态了` 等近邻形态继续 typed 拒绝。
2. **无 pause/cancel ControlCommand**：NL 面不能编译暂停/取消类意图。
3. **ROAD-B-005 仍 PARTIAL**：Trusted GUI 编译与确认面未实现。

## W28-D 增量：pause/resume/cancel ControlCommand 变体 + typed Receipt（B5-1 前半，2026-09-20）

> 状态：`PARTIAL_PASS`（单节点本地；B5-1 前半——命令面完成，真实执行接线归 W29-D；ROAD-B-005 仍 PARTIAL——GUI 未接）
>
> 基线 HEAD：`6c7a404`　　写集：`schema/nlos/sabi/v1/system_control.proto`、`gen/`、`crates/nlos-schema`、`crates/nlos-system-control`、本证据文件

### 已实现事实

1. **SABI v1.2 additive 命令臂**（ADR-0014 冻结通道 additive 扩列，镜像 W27-A 先例）：`PauseCommand`/`ResumeCommand`/`CancelCommand` 三空消息 + `ControlCommand.command` oneof 新臂 `pause_operation=11`/`resume_operation=12`/`cancel_operation=13`。寻址与 CAS 沿用共享 `target_id` + `expected_generation_or_revision` 字段（与既有 recovery 命令同构，payload 保持最小诚实）；结果复用既有 `ControlCommandResult`，无新结果消息。`frozen: true` 不变，REGISTRY minor 晋 2（W27-A 的 minor 晋升先例适用于"新命令臂"批量），`system_control_schema_identity()` 随升；gen/ TS/Python 生成物经 `buf generate` 同步。
2. **nlos-schema**：`SABI_SYSTEM_CONTROL_V1.minor 1→2`（描述注释记录两次 additive 扩列出处）；`compatibility.rs` 注册表断言随升；W27-A semantic 快照 golden 改为字面钉死 v1.1 identity（`w27a_semantic_identity()`——与 TS/Python conformance fixture 的字面 minor=1 完全同构），**冻结 golden 字节零改动**；新增 W28-D 三臂 `SubmitControlCommandRequest` 确定性 golden hex（prost 字段序：oneof 臂先于 reason——与 Python 字段号序不同的合法编码，已在常量文档注明），三臂共享前缀常量 + round-trip + 既有界（reason NUL、target 长度）fail-closed 断言。
3. **nlos-system-control 命令面**（`src/control.rs`）：`ControlCommand::PauseOperation`/`ResumeOperation`/`CancelOperation { control_command_id, target_id, expected_generation_or_revision, reason }` 三变体——mutation arm 并入既有 SUBMIT 编译（`mutation_address`/`mutation_reason` 提取共享寻址/CAS/reason；空 reason 在 wire 前 typed 拒绝；幂等键=command id 绑定不变）；`ControlOutcome::OperationPaused`/`OperationResumed`/`OperationCancelled { receipt_id }` 三 typed Receipt（`to_bytes` 判别 tag 8/9/10），receipt 投影复用抽取出的 `decoded_result_receipt`（含 foreign command-id echo 与缺失 receipt 的 fail-closed）。
4. **可插拔执行 seam**（`src/lib.rs`）：`OperationCommandExecutor` trait（`pause_operation`/`resume_operation`/`cancel_operation(OperationControlRequest) -> Result<ReceiptId, SabiFailure>`，`Send + Sync` 为契约部分——auth 测试的 async serve 循环即刻证明了该要求）+ `OperationControlRequest { target_id, expected_generation_or_revision, issuer_principal_id, idempotency_key, requested_at_ms }` + 默认 `UnwiredOperationCommandExecutor`（typed `NOT_FOUND`/`DO_NOT_RETRY` fail-closed，镜像 Unwired inspector stub 模式）。`RecoverySystemControl` 增 `operation_executor` 私有字段与 `with_operation_executor` builder——**`new()` 签名不变**，既有全部调用点零改动。`handle_submit` 三新臂在共享授权/issuer/幂等检查之后路由至 seam；执行器拒绝经 `SystemControlError::OperationExecution(SabiFailure)` 原样转发（bounded passthrough，映射表备案），未接线为 `OperationControlExecutionUnwired`。
5. **CLI parity**（`src/bin/system-control-cli.rs`）：`pause-operation`/`resume-operation`/`cancel-operation <COMMAND_ID_HEX_32> <TARGET_ID_HEX_32> <EXPECTED_REVISION> <REASON>` 三子命令 + summary 行 `outcome=operation_paused|operation_resumed|operation_cancelled receipt_id=…`。
6. **NL 双语白名单**（`src/nl.rs`）：`pause|halt|suspend operation <32-hex> expecting <n>`；`resume operation …`；`cancel|abort operation …`；`暂停操作|暂停 操作 … 期望 <n>`；`恢复操作|恢复 操作 …`；`取消操作|取消 操作 …`。派生规则与 ack 逐字镜像：command id 派生自 target id（一 target 一 pause/resume/cancel 幂等身份，重放安全）、CAS 期望显式（`[NL-AMBIG-001]`）、固定 reason `NL_PAUSE_REASON`/`NL_RESUME_REASON`/`NL_CANCEL_REASON`（原始句子不跨界）。`cancel alert …` 仍 typed 拒绝（名词不匹配），`pause everything` 仍拒绝。
7. **等价路径门**（`tests/control_command_cli.rs` 新 `operation_control_commands_are_byte_identical_across_nl_cli_and_direct_paths`）：wired `DeterministicOperationExecutor` 的真 Unix socket 服务（`serve_forever_with_executor` 扩展）上，三命令 × {直接构造, NL EN/ZH/同义词句, CLI 子命令} 三面 receipt **逐字节相等**；receipt_id 等于确定性派生值，outcome tag 钉死 8/9/10；`denied:` reason 的 RIGHTS 拒绝路径 CLI exit 1 且字节相等。handler 面（`tests/recovery_control.rs` +2）：未接线默认 typed `NOT_FOUND` 拒绝（三臂、无 receipt 证据）；接线后请求字段（target/CAS/issuer/幂等键/墙钟）逐项断言 + 执行器 Conflict 拒绝 bounded 原样转发。`system_control_failure_mapping.rs` 补两新错误变体映射（未接线 NOT_FOUND、passthrough 断言）。
8. **测试账**：nlos-schema 24/24（+1 golden 测试）；nlos-system-control 73/73——lib 30（nl 16、control 11、openmetrics 3）、`control_command_cli` 5、`recovery_control` 13、`system_control_failure_mapping` 5、`control_ipc_auth` 9、metrics 3+7、windows 0、doc-tests 1。

### 验证

验证环境：macOS（darwin，arm64），基线 HEAD `6c7a404`，分支 `feat/w28-d`。工作区无其他车道未提交改动（本车道独占写集）。

- `cargo test -p nlos-schema -p nlos-system-control`：**97 passed / 0 failed**。
- `cargo clippy -p nlos-schema -p nlos-system-control --all-targets -- -D warnings`：通过（0 warning / 0 error）。
- `cargo fmt -p nlos-schema -p nlos-system-control --check`：通过。
- `cargo check -p nlos-system-control --no-default-features`：通过（非 cli 形态编译）。
- `buf lint` + `buf format -d --exit-code`：通过。
- `buf generate` + normalize + `check-generated-schema`：生成物与提交基线一致（提交后验证）。

### 已知限制（增量）

1. **执行 seam 默认未接线**：`UnwiredOperationCommandExecutor` typed `NOT_FOUND` 拒绝；真实执行（pause→Process suspend、kill/throttle/reclaim 等）归 W29-D（B5-1 后半 + B5-2），本车道只交付命令面。
2. **operation-level NL 无 `恢复语义恢复` 等领域特化形态**：`resume operation` 为通用操作级恢复；`ResumeSemanticRecovery` 仍仅 CLI/direct 面（既有状态，非本车道回归）。
3. **TS/Python conformance 未加新臂 golden**：写集排除 `tests/conformance/`；Rust 侧 golden 已钉 prost 序字节，TS/Python fixture 维持既有快照。三语言生成物（gen/）已同步。Deferred minor：conformance 侧补钉三臂 hex。
4. **REGISTRY minor 晋 2 的既有 golden 兼容**：W27-A semantic golden 以字面 v1.1 identity 钉死（与 TS/Python 同构），不随 REGISTRY minor 漂移；此为冻结 golden 的钉定纪律，非字节改动。
5. **ROAD-B-005 仍 PARTIAL**：Trusted GUI 未接；多层手动调度的执行半边（W29-D）未做。

## W29-D 增量：kill/throttle/reclaim 变体 + 真实 authority 执行接线（B5-1 后半 + B5-2，2026-09-20）

> 状态：`PARTIAL_PASS`（单节点本地；三变体命令面 + 三条真实 authority 执行路径完成——计划行验收"每变体 typed Receipt 且执行路径真达对应 authority"达成；ROAD-B-005 仍 PARTIAL——GUI 未接、pause/resume/cancel 的宿主执行器仍为 seam 待接）
>
> 基线 HEAD：`55c7f45`（W29-F merge 后）　　写集：`schema/`、`gen/`、`crates/nlos-schema`、`crates/nlos-system-control`、`crates/nlos-resource`（限流权威面 + `inspect_quote`）、本证据文件；`crates/nlos-task` 只读消费、`crates/nlos-process` 零改动（`request_platform_kill`/`SupervisorPidRegistry`/platform 适配器全部复用 W22-P/W29-F 既有公共面）

### 已实现事实

1. **SABI v1.3 additive 命令臂**（ADR-0014 冻结通道 additive 扩列，镜像 W27-A/W28-D 先例）：`KillCommand`/`ReclaimCommand` 空消息 + `ThrottleCommand{throttle_percent=1}` + `ControlCommand.command` oneof 新臂 `kill_operation=14`/`throttle_operation=15`/`reclaim_operation=16`。kill/reclaim 寻址与 CAS 沿用共享 `target_id` + `expected_generation_or_revision`；throttle 单独携带 `1..=100` 整数百分比（`validate_control_command` 编解码双侧 fail-closed，越界复用 `InvalidSystemControlIdentifier`）。结果复用 `ControlCommandResult`，无新结果消息。REGISTRY minor 晋 3（frozen 不变），`system_control_schema_identity()` 随升；gen/ TS/Python 经 `buf generate` 同步。
2. **nlos-schema 兼容金样**：注册表断言随升；W28-D 操作级 golden 改字面钉死 v1.2 identity（`w28d_operation_identity()`，与 W27-A `w27a_semantic_identity` 钉定纪律同构，冻结 golden 字节零改动）；新增三臂 v1.3 确定性 golden hex（throttle/reclaim 臂长各别 `0x69`/`0x68`——oneof 载荷字节数不同导致命令长度前缀不同，已在常量注释注明）+ round-trip + percent `0`/`101` fail-closed。
3. **nlos-resource 限流权威面**（计划行 throttle→ResourceDemand 的 authority 侧）：`ResourceDemand::throttled_to_percent`（逐维 saturating 缩放；`100` 严格恒等——`u64::MAX` 上 saturating-mul-再除会破坏恒等，已特判并在测试钉死；`<100` 只收缩并截断；`0` 归零）+ `throttle_demand`/`DemandThrottle`（before/after/容量 + `exceedance_of` 固定序首次越界报告）+ `ResourceAuthority::inspect_quote`（镜像 `inspect_reservation` 的只读回查，暴露 quote 行声明的 per-dimension `demand_capacity`）。
4. **命令面**（`src/control.rs`）：`ControlCommand::KillOperation`/`ThrottleOperation{+throttle_percent}`/`ReclaimOperation` 三变体并入既有 SUBMIT 编译（共享寻址/CAS/reason 提取；空 reason 与越界 percent 均在 wire 前 typed 拒绝；幂等键=command id 绑定不变）；`ControlOutcome::OperationKilled`/`OperationThrottled`/`OperationReclaimed { receipt_id }` typed Receipt（`to_bytes` tag 11/12/13），投影复用 `decoded_result_receipt` fail-closed 路径。
5. **执行 seam 扩展**（`src/lib.rs`）：`OperationCommandExecutor` 增 `kill_operation`/`throttle_operation(+throttle_percent)`/`reclaim_operation` 三方法（非默认实现——实现方必须显式表态每个臂，未拥有臂 fail-closed 拒绝）；`OperationArm` 增 `Kill`/`Throttle{percent}`/`Reclaim`；`handle_submit` 三新臂在共享授权/issuer/幂等检查后路由至 seam。`with_operation_executor`/`new()` 签名不变。
6. **三条真实执行路径**（每变体 typed Receipt，receipt id = 域分隔 SHA-256 截断 16 字节，**由 authority 自身回执事实派生**——不经 authority 调用不可能产生，重放幂等重derive 同一 id）：
   - **kill→SupervisorPidRegistry/platform kill**（`process_kill_executor.rs`，`process` feature）：`inspect_active_process_binding`（权威 head：generation+fencing token）→ generation CAS 检查（`CONFLICT`）→ `SupervisorPidRegistry::lookup`（缺映射 `NOT_FOUND`；映射 generation 落后于 head `STATE`）→ `request_platform_kill`（**durable 回执先落库**，再调注入的 `PlatformKillAdapter` 信号 OS——at-least-once；宿主以 `registry.pid_map()` 喂 `Posix/WindowsPlatformKillAdapter`，测试用 `StubPlatformKillAdapter` 验证信号事实）。负墙钟 `INVALID_ARGUMENT`，全部拒绝零 durable 副作用。
   - **throttle→ResourceDemand**（`resource_throttle_executor.rs`，`resource` feature）：`inspect_reservation`（target=reservation；当前声明 demand + `usage_high_water_seq` 作为 revision CAS——reserve 时 demand 不可变，无 generation 可比，usage 序为该行唯一单调修订号）→ `inspect_quote`（声明的 per-dimension 容量）→ `throttle_demand`（权威调整 + admission）。**缺口如实记录**：调整在此计算并校验，不落 durable——reservation 模型无 demand 调整 mutation，持久的 re-reservation 归 Resource coordinator 车道。
   - **reclaim→WorkingSetReclaim**（`working_set_reclaim_executor.rs`，无 feature 门——nlos-task 为核心依赖）：`WorkingSetOccupancySource` trait + `FixedWorkingSetOccupancy`（宿主持有观测——**缺口如实记录**：nlos-task 无对外 store-wide 活跃计数入口，`inspect_working_set_pressure` 为纯函数）→ `inspect_working_set_pressure`（CAS = 观测到的活跃计数，移动即 `CONFLICT`；低于软阈值 `STATE` 拒绝——无可回收即不伪造回收）→ `plan_working_set_reclaim_execution` → `execute_working_set_reclaim_execution`（nlos-task 唯一对外 reclaim 执行入口；`evicted_units` 为 nlos-task 文档明示的合成可重建缓存计数，Context Residency Controller 未落）。
7. **CLI**：`kill-operation`/`reclaim-operation <CMD_ID> <TARGET> <REVISION> <REASON>`、`throttle-operation <CMD_ID> <TARGET> <PERCENT_1_TO_100> <REVISION> <REASON>` + summary 行（`outcome=operation_killed|throttled|reclaimed receipt_id=…`）。
8. **NL 双语白名单**：`kill|terminate operation <32-hex> expecting <n>`；`throttle operation <32-hex> to <n> percent expecting <n>`；`reclaim operation <32-hex> expecting <n>`；`终止操作|终止 操作`、`限流操作|限流 操作 … 到 <n> 百分比 期望 <n>`、`回收操作|回收 操作`。派生规则逐字镜像 pause（command id 派生自 target、显式 CAS、固定 per-verb reason）；百分比同样显式（`[NL-AMBIG-001]`——静默猜 throttle 级别与猜 CAS 同罪）。`kill task`/`throttle task`/`terminate alert` 等近邻形态 typed 拒绝。
9. **等价路径门**（`control_command_cli.rs` 新 `kill_throttle_reclaim_commands_are_byte_identical_across_nl_cli_and_direct_paths`）：wired 确定性执行器的真 Unix socket 服务上，三命令 × {直接构造, NL EN/ZH/同义词, CLI} 三面 receipt 逐字节相等，outcome tag 钉死 11/12/13；CLI 越界百分比（`101`）exit 2 且打印 usage。
10. **authority 接线门**（新 `tests/operation_executor_authorities.rs`，kill/throttle 模块 `cfg(process/resource)`）：每变体三测试——真实 authority 驱动（kill：StubAdapter 信号记录 + durable 回执存在 + 重放同 id；throttle：同输入同 id、不同百分比不同 id；reclaim：同观测同 id、不同观测不同 id）+ fail-closed 拒绝矩阵（CAS 错配/缺 supervisor 映射/负墙钟；CAS 错配/缺 reservation/越界百分比；低于软阈值/观测移动）+ 经共享 handler 的端到端（dispatch_in_process 的 `OperationKilled/Throttled/Reclaimed` receipt id 与直接执行器调用逐字节一致）。
11. **既有 feature 门修复（根因）**：`cargo check -p nlos-system-control --features process,resource` 在基线 HEAD 即坏——W29-F 给 `ProcessAuthorityError` 增 `PlatformKillAlreadySignaled`/`PlatformKillAdapter`、W28-C 系给 `ResourceAuthorityError` 增 `DemandExceedsCapacity` 时未同步 feature 门控的 `process_inspector.rs`/`resource_inspector.rs` 错误映射（后者另有 `map_err` 传 owned 给 `&Error` 形参的既有类型错）。本车道顺带修复（kill 映射 `CONFLICT`/`DRIVER`、demand 归 `INVALID_ARGUMENT` 组、闭包传引用），否则新执行器无法编译。CI 未覆盖该 feature 组合为独立缺口（见下）。

### 验证

验证环境：macOS（darwin，arm64），基线 HEAD `55c7f45`，分支 `feat/w29-d`。工作区无其他车道未提交改动（本车道独占写集）。

- `cargo test -p nlos-schema -p nlos-system-control -p nlos-process -p nlos-resource`：**183 passed / 0 failed**（schema 25；system-control 84——lib 36、`control_command_cli` 6、`recovery_control` 14、`operation_executor_authorities` 3（默认仅 reclaim 模块）、`control_ipc_auth` 9、failure_mapping 5、metrics 3+7、doc 1；process 36；resource 38）。
- `cargo test -p nlos-system-control --features process,resource`：**90 passed / 0 failed**（`operation_executor_authorities` 全 9 测试开跑：kill 3 + throttle 3 + reclaim 3）。
- `cargo clippy -p nlos-system-control -p nlos-schema -p nlos-resource --all-targets`（默认与 `--features process,resource` 两态）：0 warning / 0 error。
- `cargo fmt -p nlos-system-control -p nlos-schema -p nlos-resource -p nlos-process -- --check`：通过。
- `cargo check -p nlos-system-control --no-default-features`（及 `+ --features process,resource`）：通过（非 cli 形态编译）。
- `buf lint` + `buf format -d --exit-code`：通过。
- `npm run schema:check-generated`：提交后复验（生成物与提交基线一致）。

### 已知限制（增量）

1. **throttle 无 durable 写**：reservations 的 demand 在 reserve 时不可变声明；调整经 authority 类型计算 + admission 校验（receipt id 由 before/after 维度值派生），持久的 demand_after 落库（re-reservation）归 Resource coordinator 车道。
2. **reclaim 执行入口为纯函数链**：nlos-task 对外只有 advisory→plan→execute 纯函数族（`evicted_units` 为其文档明示的合成计数），无 durable reclaim mutation、无 Context Residency Controller、rehydrate 仍为 B-TASK-SCALE-001 备案缺口；活跃计数由宿主观测供给（nlos-task 无 store-wide 活跃计数公共入口）。本车道只读消费、零改 nlos-task。
3. **pause/resume/cancel 真实执行器仍缺**：W28-D seam 契约不变，三新真实执行器仅覆盖 kill/throttle/reclaim（计划行本就如此切分）；pause→Process suspend 等宿主执行器递延。
4. **`--features process,resource` 组合此前无 CI 门**：基线即坏而无人发现（见"已实现事实"11）；本车道修复后两态均绿，但 CI 矩阵未扩——deferred minor：CI 补该组合。
5. **kill receipt id 为派生值非 authority 原生 ReceiptId**：nlos-process 的 `PlatformKillReceipt` 无原生 ReceiptId 字段；以域分隔 SHA-256 覆盖其全部字段（process_id/generation/fencing_token/idempotency_key/killed_at_ms）派生，等价证明力（缺任一 authority 事实即不可复现），非伪造。
6. **TS/Python conformance 未加新臂 golden**：与 W28-D 同因（写集排除 `tests/conformance/`）；gen/ 三语言生成物已同步。Deferred minor：conformance 侧补钉三臂 hex。
7. **ROAD-B-005 仍 PARTIAL**：Trusted GUI 未接；多层手动调度的剩余半边（宿主 pause/resume/cancel 执行器、GUI 确认面）递延。

## W32-G 增量：各层 inspect 补齐——TaskGroup/TaskNode/ExecutionFiber/Topic/Operation inspect Receipt（B5-3，2026-09-21）

> 状态：`PARTIAL_PASS`（单节点本地；B5-3 计划行"每层 inspect Receipt 齐"达成——四层五个只读视图全部 typed Receipt 化并三路 parity；ROAD-B-005 仍 PARTIAL——Trusted GUI 宿主执行器等归其余 B5 车道）
>
> 基线 HEAD：`b65255e`（W32 波开基线）　　写集：`schema/nlos/sabi/v1/system_control.proto`、`gen/`、`crates/nlos-schema`、`crates/nlos-system-control`（含新 features `plan`/`runtime`/`topic`/`store` 与四个 feature 门适配器）、本证据文件；`crates/nlos-plan`/`nlos-runtime`/`nlos-runtime-tokio`/`nlos-topic`/`nlos-store`/`nlos-operation`/`nlos-channel` 只读消费（零改动，仅作为新增 optional 依赖）

### 已实现事实

1. **SABI v1.5 additive 视图**（ADR-0014 冻结通道 additive 扩列，镜像 W27-A/W28-D/W29-D/W28-C-3b 先例）：`SystemControlView` 新值 `TASK_GROUP=4`/`TASK_NODE=5`/`EXECUTION_FIBER=6`/`TOPIC=7`/`OPERATION=8`（B5-3 四层中 Topic/Operation 一层由两个互补视图覆盖）；`GetSystemControlRequest` additive 寻址字段 `target_id=4`/`plan_id=5`/`target_generation=6`（新视图必填 16 字节目标，TaskNode 视图额外要求 16 字节 plan_id，fiber/operation 视图额外要求非零 handle generation，recovery 视图 1..=3 三字段必须为空——`validate_view_addressing` 编解码双侧 fail-closed）；五个快照消息族（`TaskGroupOperationsSnapshot`/`TaskNodeOperationsSnapshot`/`ExecutionFiberOperationsSnapshot`/`TopicOperationsSnapshot`/`DurableOperationSnapshot`）+ 九个状态枚举（TaskGroup 13 态/成员类型/成员资格、PlanNode 13 态/ContextResidency 5 级/kind、Fiber 10 态/4 相位、DurableOperation 8 态）。REGISTRY minor 晋 5（frozen 不变），`system_control_schema_identity()` 随升；gen/ TS/Python 经 `buf generate` 同步。
2. **nlos-schema 兼容面**：五个快照 encode/decode 对（bounded 64KiB + 逐字段校验：16 字节 id、32 字节 digest、指定枚举非 Unspecified、成员≤256、TaskGroup 成员必须携带 admission receipt、operation 终态必须且仅携带一个 outcome receipt、topic name ≤256 且无 NUL——`MAX_SYSTEM_CONTROL_TOPIC_NAME_BYTES`）；新错误变体 `MissingSystemControlLayerStatus`/`InvalidSystemControlLayerStatus`；W28-C-3b resource golden 改字面钉死 v1.4 identity（`w28c_resource_identity()`，与 w27a/w28d 钉定纪律同构，冻结 golden 字节零改动）；新增五个 v1.5 确定性 golden hex（Rust prost 字段序，钉死快照字节）。
3. **数据源与 seam**（handler 侧、镜像 `OperationCommandExecutor` 模式；`new()` 签名不变，既有调用点零改动）：
   - **TaskGroup**：handler 直读自有 `SqliteTaskAuthority`——`inspect_group`（GroupRecord 13 态映射）+ `list_group_members`（成员按 `alert_limit` 截断 + `members_truncated` 旗标，Admission/Removal receipt 以 `ReceiptReference` 投影）；`GroupNotFound` 经既有 Task 映射为 `NOT_FOUND`。
   - **TaskNode/ExecutionFiber/Topic/Operation**：四个 pluggable seam（`TaskNodeInspectSource`/`ExecutionFiberInspectSource`/`TopicInspectSource`/`OperationInspectSource`，返回 typed inspection 或 bounded `SabiFailure`，`Send+Sync` 契约）+ `Unwired*` 默认 stub（typed `NOT_FOUND` fail-closed）；`with_*_source` builder 接线；来源拒绝经 `SystemControlError::LayerInspection(SabiFailure)` bounded 原样转发，未接线为 `LayerInspectionUnwired`。
4. **四个真实适配器**（feature 门控，目标 crate 零改动）：
   - `plan_inspector.rs`（`plan` feature）：`PlanAuthorityTaskNodeSource` over `SqlitePlanAuthority::inspect_node`——PlanNodeRecord（kind/13 态/declared_revision/32 字节 node_digest/transition_count/residency_tier/residency_transition_count/时间戳）全字段投影；`Ok(None)`/`NodeNotFound`→`NOT_FOUND`。
   - `fiber_inspector.rs`（`runtime` feature）：`TokioExecutionFiberSource` over `TokioRuntimeAdapter` 只读三面（`inspect`/`inspect_lifecycle_phase`/`activation_usage`，Duration→毫秒饱和投影）；runtime 无专用 not-found——`InvalidGeneration`→`NOT_FOUND`（handle 未知名）、`FiberReaped`→`NOT_FOUND`（已回收名）。
   - `topic_inspector.rs`（`topic` feature）：`TopicAuthoritySource` over `TopicAuthority::inspect_topic`——channel 绑定（id+generation）、admitted name（超界/NUL 早拒 `INVALID_ARGUMENT`，不让畸形投影上线）、active_subscriptions、policy digest；`TopicNotFound`→`NOT_FOUND`。
   - `operation_inspector.rs`（`store` feature）：`OperationStoreSource` over `SqliteOperationStore::inspect`——8 态状态机行 + cancel_epoch + owner fiber handle + 终态 outcome receipt；store 行缺失与 stale generation 同面（`OperationError::InvalidGeneration`）→一条诚实命名歧义的 `NOT_FOUND`。
5. **命令面**（`src/control.rs`）：`InspectTaskGroup{group_id}`/`InspectTaskNode{plan_id,node_id}`/`InspectExecutionFiber{fiber_id,generation}`/`InspectTopic{topic_id}`/`InspectOperation{operation_id,generation}` 五只读变体（GET 编译经 `layer_view_get_arm`，命令 id/关联 id 派生自目标 id——scoped-read 同 InspectTask/Process/Resource 先例；fiber/operation 的零 generation 在 wire 前 typed 拒绝）；`ControlOutcome::TaskGroupInspected`/`TaskNodeInspected`/`ExecutionFiberInspected`/`TopicInspected`/`DurableOperationInspected` typed Receipt（`to_bytes` 判别 tag 15..19，wire 枚举经 schema 校验后投影，compose 侧 `fixed16_or_defect` fail-closed）。
6. **CLI**：`inspect-task-group <GROUP_HEX_32>`、`inspect-task-node <PLAN_HEX_32> <NODE_HEX_32>`、`inspect-fiber <FIBER_HEX_32> <GENERATION>`、`inspect-topic <TOPIC_HEX_32>`、`inspect-operation <OPERATION_HEX_32> <GENERATION>` 五子命令 + summary 行（`outcome=task_group_inspected|task_node_inspected|execution_fiber_inspected|topic_inspected|operation_inspected …`）。
7. **NL 双语白名单**：`inspect|check|show|get|status task group <32-hex>`、名词倒装 `task group status <32-hex>`；`inspect task node <PLAN> <NODE>` 及倒装；`inspect fiber <32-hex> generation <n>`（EN）`查看纤程 <32位十六进制> 世代 <n>`（ZH）及倒装；`inspect topic <32-hex>`/`查看主题`；`inspect operation <32-hex> generation <n>`/`查看操作 … 世代 <n>`。派生规则镜像 scoped-read 先例（command id 派生自目标 id、重放安全）；handle generation 显式且必须为正（`[NL-AMBIG-001]`——静默猜 generation 与猜 CAS 同罪，零/负/非十进制 typed 拒绝）；`inspect task group now`/`查看任务节点`（缺参）等近邻形态 typed 拒绝。
8. **等价路径门**（`control_command_cli.rs` 新 `w32g_layer_reads_are_byte_identical_across_nl_cli_and_direct_paths`）：wired 四 stub 源 + 真实 TaskAuthority（含真实 group+成员 fixture）的真 Unix socket 服务上，五命令 × {直接构造, NL EN/ZH/倒装句, CLI 子命令} 三面 receipt **逐字节相等**。handler 面（`recovery_control.rs` +3）：四源未接线默认 typed `NOT_FOUND`（含 `layer inspection backend is not wired` 消息钉死）；wired stub receipt 字段逐项断言；TaskGroup 真权威 roundtrip（Open 态+1 成员+admission receipt）+ 缺失组 typed `NOT_FOUND`。`system_control_failure_mapping.rs` +1（两新变体 bounded 映射 + passthrough 断言）。
9. **权威接线门**（新 `tests/layer_inspector_authorities.rs`，cfg 门控于四 feature 之和）：plan（真实 `apply_plan_revision`→`inspect_node` 回读 Declared/MetadataOnly/digest 长度 + 缺节点/缺 plan typed `NOT_FOUND`）、runtime（真实 spawn fiber→有界轮询至 Running 态读 state/phase/米表 + 未知 handle `NOT_FOUND` + 零 generation `INVALID_ARGUMENT`）、topic（真实 Channel+Topic 权威建 topic→读回 name/订阅数/digest + 缺失 `NOT_FOUND`）、store（真实 register→Registered 行 + owner fiber + 缺失/stale 同 `NOT_FOUND`）；每适配器另各一条经共享 handler 的端到端（`dispatch_in_process` receipt 与直接调用逐项相等）。
10. **测试账**：`cargo test -p nlos-schema -p nlos-system-control`（默认 features）**148 passed / 0 failed**——schema compatibility 34（+W32-G 六 payload 门 + 五 golden 钉死）；system-control lib 54（nl 新五测试）、`control_command_cli` 8（+parity 1）、`recovery_control` 21（+3）、`system_control_failure_mapping` 7（+1）、`layer_inspector_authorities` 0（默认关）、metrics 4+7、`operation_executor_authorities` 3、`control_ipc_auth` 9、doc 1。`--features plan,runtime,topic,store` **121 passed / 0 failed**（`layer_inspector_authorities` 7 测试开跑：plan 2 + runtime 1 + topic 2 + store 2）。

### 验证

验证环境：macOS（darwin，arm64），基线 HEAD `b65255e`，分支 `feat/w32-g`。工作区无其他车道未提交改动（本车道独占写集）。

- `cargo test -p nlos-schema -p nlos-system-control`：**148 passed / 0 failed**（默认 features）。
- `cargo test -p nlos-system-control --features plan,runtime,topic,store`：**121 passed / 0 failed**。
- `cargo clippy -p nlos-schema -p nlos-system-control --all-targets --all-features -- -D warnings`：通过（0 warning / 0 error；`--all-features` 覆盖 cli+process+resource+plan+runtime+topic+store 全组合）。
- `cargo fmt -p nlos-schema -p nlos-system-control -- --check`：通过。
- `cargo check -p nlos-system-control --no-default-features`（及 `--features process,resource`）：通过（非 cli 形态编译，W29-D 教训的 CI 空白组合本地复验）。
- `buf lint` + `buf format -d --exit-code`：通过。
- `npm run schema:generate` 两次运行输出稳定；`npm run schema:check-generated`：提交后复验（生成物与提交基线一致）。

### 已知限制（增量）

1. **fiber 无列表面**：nlos-runtime-tokio 只暴露 `registered_fibers()` 计数与聚合米表，无 fiber id 枚举 API——ExecutionFiber inspect 只能寻址已知 handle（spawn 回执或宿主登记），整池枚举为登记缺口（W33-G debugger 面同类依赖）；本车道零改 runtime crate，缺口在此登记。
2. **`WaitingModel`/`WaitingIo` 仅经 runtime 自有 wait 机制进入**：fiber 视图对普通 parked future 的可观测态为 `Running`；WaitingIo/WaitingModel 相位的 inspect 覆盖依赖 channel/model-wait 接线场景（测试以 Running 态钉死，注释已注明语义）。
3. **operation 行缺失与 stale generation 不可区分**：nlos-store 的 `inspect` 对两者同返 `OperationError::InvalidGeneration`，适配器以一条命名歧义的 `NOT_FOUND` 如实报告；分离需 store 侧新 readback 变体（登记缺口）。
4. **TS/Python conformance 未加五快照 golden**：与 W28-D/W29-D 同因（写集排除 `tests/conformance/`）；gen/ 三语言生成物已同步。Deferred minor：conformance 侧补钉五快照 hex。
5. **GUI（W32-A/B）面未接五新视图**：desktop/ 在其他车道写集；五视图为纯 additive GET，GUI 侧消费归 B5-4 后续。
6. **ROAD-B-005 仍 PARTIAL**：Trusted GUI 宿主执行器（pause/resume/cancel 的 W28-D seam 接线）等归其余 B5 车道；本车道只交付 B5-3 只读 inspect 面。

## W35-P11 增量：Application 层生命周期控制面前片——disable/uninstall 命令臂 + 真实 ApplicationAuthority 执行接线（移交#11 前片，2026-09-21）

> 状态：`PARTIAL_PASS`（单节点本地；移交#11 前片范围——COMMAND 面 + 生命周期 NL 动词 + 执行接线到真实 nlos-application 权威；W34-A §5 裁决的「七层中 Application 层缺位」的控制动词半边收口，Application 层 inspect（GET 面）与 supervisor pid 发现仍归 C-APP-CONTROL 其余半边）
>
> 基线 HEAD：`051a656`（feat/w35-p11 分支基）　　实现 commits：`fe5a3fa`（nlos-schema v1.6 契约）+ `0fa76bb`（system-control 面）　　写集：`schema/`、`gen/`、`crates/nlos-schema`、`crates/nlos-system-control`（含新 feature `application` 与 `application_lifecycle_executor` 适配器）、本证据文件；`crates/nlos-application` **零改动**（W27-D gate 测试零触碰——MUST NOT 边界遵守，仅公共 API 消费）

### 已实现事实

1. **SABI v1.6 additive 命令臂**（ADR-0014 冻结通道 additive 扩列，镜像 W27-A/W28-D/W29-D/W28-C-3b/W32-G 先例）：`DisableApplicationCommand`/`UninstallApplicationCommand` 两空消息 + `ControlCommand.command` oneof 新臂 `disable_application=19`/`uninstall_application=20`。寻址沿用共享 `target_id`（16 字节 package 身份——ApplicationId 由权威自 package 派生）+ `expected_generation_or_revision`（应用当前安装代 CAS；disable/uninstall 迁移不移动安装代，权威 schema DDL 保证）。结果复用既有 `ControlCommandResult`，无新结果消息。REGISTRY minor 晋 5→6（frozen 不变），`system_control_schema_identity()` 随升；gen/ TS/Python 经 `buf generate` 同步。
2. **nlos-schema 兼容面**：注册表断言随升；W32-G 五层快照 golden 改字面钉死 v1.5 identity（`w32g_layer_identity()`，与 w27a/w28c/w28d 钉定纪律同构——冻结 golden 字节零改动）；新增两臂 v1.6 确定性 golden hex + round-trip（prost 字段序，两臂同为三线字节——双字节 field-19/20 tag + 零长度，共享 `0x68` 命令长度前缀）。
3. **命令面**（`src/control.rs`）：`ControlCommand::DisableApplication`/`UninstallApplication { control_command_id, package_id, expected_generation_or_revision, reason }` 两变体并入既有 SUBMIT 编译（`mutation_address`/`mutation_reason` 提取共享寻址/CAS/reason；空 reason wire 前 typed 拒绝；幂等键=command id 绑定不变）；`ControlOutcome::ApplicationDisabled`/`ApplicationUninstalled { receipt_id }` typed Receipt（`to_bytes` 判别 tag 20/21），投影复用 `decoded_result_receipt` fail-closed 路径。
4. **可插拔执行 seam**（`src/lib.rs`，独立于 operation seam——Application 层为独立控制面）：`ApplicationCommandExecutor` trait（`disable_application`/`uninstall_application(ApplicationControlRequest) -> Result<ReceiptId, SabiFailure>`，`Send + Sync` 契约）+ `ApplicationControlRequest { package_id, expected_generation_or_revision, issuer_principal_id, idempotency_key, requested_at_ms }` + 默认 `UnwiredApplicationCommandExecutor`（typed `NOT_FOUND` fail-closed）。`RecoverySystemControl` 增 `application_executor` 字段与 `with_application_executor` builder——**`new()` 签名不变**，既有全部调用点零改动（workspace check 证明零下游破坏：desktop `build_control_command` 匹配自身 action 枚举，additive 不触及）。`handle_submit` 两新臂在共享授权/issuer/幂等检查之后路由至 seam；执行器拒绝经 `SystemControlError::ApplicationExecution(SabiFailure)` bounded 原样转发，未接线为 `ApplicationControlExecutionUnwired`。
5. **真实执行接线**（`src/application_lifecycle_executor.rs`，新 `application` feature；receipt id = 域分隔 SHA-256 截断 16 字节，由权威自身回执事实派生——不经 authority 调用不可能产生，durable 重放重 derive 同一 id）：
   - **disable→`disable_application`**：`inspect_application`（权威 head：状态 + 当前安装代）→ 安装代 CAS 检查（`CONFLICT`）→ 权威 `installed → disabled` 可逆迁移（**无活动门**——disable 可回滚）；receipt id 覆盖 application_id/generation/idempotency_key/disabled_at_ms。
   - **uninstall→W27-D 真实活动门 + `uninstall_application_with_task_activity_gate`**：同 head/CAS 预检 → 权威在门自身 writer 事务内解析 package 的 durable 后台任务登记并查询 `SqliteTaskAuthority` 活跃任务（登记无 task 行是 promise 非活动；终态 task 非活动）→ 有活跃任务 typed `STATE`（"application still has outstanding task activity"）零 durable 副作用；门通过后 `installed|disabled → uninstalled` 终态迁移 + uninstall receipt。负墙钟 `INVALID_ARGUMENT`；缺包 `NOT_FOUND`；权威错误按类映射（终态 `STATE`、时间序 `INVALID_ARGUMENT`、幂等冲突 `CONFLICT`、存储 `DURABILITY`、活动查询失败 `DRIVER` fail-closed）。
6. **CLI**：`disable-application`/`uninstall-application <COMMAND_ID_HEX_32> <PACKAGE_ID_HEX_32> <EXPECTED_GENERATION> <REASON>` 两子命令 + summary 行（`outcome=application_disabled|application_uninstalled receipt_id=…`）。
7. **NL 双语白名单**：`disable application <32-hex> expecting <n>`；`uninstall application <32-hex> expecting <n>`；`禁用应用|禁用 应用 <32位十六进制> 期望 <n>`；`卸载应用|卸载 应用 <32位十六进制> 期望 <n>`。派生规则逐字镜像 operation 家族（command id 派生自 package id——一 package 一 disable/uninstall 幂等身份，重放安全；CAS 期望显式 `[NL-AMBIG-001]`；固定 per-verb reason `NL_DISABLE_REASON`/`NL_UNINSTALL_REASON`——原始句子不跨界）。EN 本片只入 canonical 两动词（登记缺口原文即 disable/uninstall application；同义词扩展沿 W13-C/W18-C/W19-C 同义词白名单节奏归后续）。近邻形态 typed 拒绝：`disable operation`/`uninstall task`/缺参/非十进制/31 位 hex/`inspect application`（GET 面不在本片）。
8. **四路等价门**（`triple_path_receipt_parity.rs` 新 `application_family_receipts_are_byte_identical_across_direct_nl_cli_and_gui_paths`）：fixture 双入口（认证 + plain）增 `DeterministicApplicationExecutor` stub 接线后，disable/uninstall × {直接构造, NL EN/ZH, CLI 子命令, GUI dispatch core} 四路 receipt **逐字节相等**；outcome tag 钉死 20/21；`denied:` reason 的 RIGHTS 拒绝路径三面（direct/CLI/GUI）字节相等（NL 固定 reason 不可表达 denial，与 operation 家族同口径）。
9. **handler 面 + 权威接线门**：`recovery_control.rs` +2（未接线默认 typed `NOT_FOUND`（消息钉死 "application control execution backend is not wired"）；wired stub 请求字段逐项断言（package/CAS/issuer/幂等键/墙钟）+ `STATE` 拒绝 bounded 原样转发无 receipt 证据）；`system_control_failure_mapping.rs` +2（两新错误变体映射 + passthrough 断言）；新 `tests/application_lifecycle_authorities.rs`（`application` feature 门，4 测试）：真实签名包（Ed25519 + 真实 `verify_package`）→ 真实 `install_application`（gen 1）fixture 上——驱动门（disable 权威状态翻转 + 重放同 id；uninstall 权威终态 + disable/uninstall receipt id 不同）、**W27-D 门开合**（注册后台任务 + 活跃 task 行 → typed `STATE` 拒绝零 durable 副作用；task 终态后 fresh key 收敛）、拒绝矩阵（CAS 错配 `CONFLICT` 消息钉死/缺包 `NOT_FOUND`/负墙钟 `INVALID_ARGUMENT`，全部零状态迁移）、共享 handler 端到端（`dispatch_in_process` 的 `ApplicationDisabled`/`ApplicationUninstalled` receipt id 与直接执行器调用逐字节一致——handler 路径经 durable replay 幂等）。
10. **测试账**：`cargo test -p nlos-schema -p nlos-system-control`（默认 features）**164 passed / 0 failed**——schema 35（+1 v1.6 golden）；system-control 129——lib 60（nl +3、control +3）、`control_command_cli` 8、`recovery_control` 23（+2）、`system_control_failure_mapping` 7（+2）、`triple_path_receipt_parity` 7（+1）、`operation_executor_authorities` 3、`control_ipc_auth` 9、metrics 4+7、windows 0、doc 1。`--features application` **133 passed**（`application_lifecycle_authorities` 4 开跑）；`--features process,resource,plan,runtime,topic,store`（W32-G 组合）142 passed 零回归；`--all-features` **146 passed / 0 failed**。

### 验证

验证环境：macOS（darwin，arm64），分支 `feat/w35-p11`（基线 `051a656`）。工作区无其他车道未提交改动（本车道独占写集；node_modules 为本地工具链安装，不入库）。

- `cargo test -p nlos-schema -p nlos-system-control`：**164 passed / 0 failed**（默认 features）。
- `cargo test -p nlos-system-control --features application`：**133 passed / 0 failed**；`--features process,resource,plan,runtime,topic,store`：142 / 0；`--all-features`：**146 / 0**。
- `cargo clippy -p nlos-system-control -p nlos-schema --all-targets --all-features -- -D warnings`（及默认态）：0 warning / 0 error。
- `cargo fmt -p nlos-system-control -p nlos-schema -- --check`：通过。
- `cargo check -p nlos-system-control --no-default-features`（及 `+ --features application`）：通过（非 cli 形态编译）；`cargo check --workspace`：零下游破坏。
- `buf lint` + `buf format -d --exit-code`：通过。
- `npm run schema:generate` 两次运行输出稳定；`npm run schema:check-generated`：通过（生成物与提交基线一致）。
- `python3 scripts/lint_claims.py`：PASS（0 ERROR 0 INFO；evidence-index 144/144——本片无新证据文件，扩展既有 b-control-003 条目）。

### 已知限制（增量）

1. **前片只覆盖控制动词半边**：Application 层 inspect（GET 视图）未做——ROAD-B-005 §5.1 「Application 行」的 inspect 仍缺位，归 C-APP-CONTROL 后片；GUI（desktop）面未接两新臂（`build_control_command` 单一编译点未扩，desktop/ 属其他车道写集，additive 不破坏）。
2. **TS/Python conformance 未加两新臂 golden**：与 W28-D/W29-D/W32-G 同因（写集排除 `tests/conformance/`）；gen/ 三语言生成物已同步。C-CONFORMANCE-GOLDEN #13 收口条款「后续新增 SABI 臂随波屏障钉死」——本臂 golden 补钉登记为 deferred minor。
3. **安装代 CAS 为控制面预检**：权威自身在事务内做状态 CAS（安装代不随 disable/uninstall 移动，故 CAS 语义为「命令所见代 == 权威当前代」的乐观预检，镜像 kill 执行器对 process generation 的预检先例）；权威迁移的终审在自身 `Immediate` 事务。
4. **EN 无 uninstall/remove 同义词变体**：本片按登记缺口原文只入 canonical `disable`/`uninstall` 两动词（ZH 双拼式照旧）；同义词白名单扩展沿 W13-C/W18-C/W19-C 节奏归后续增量。
5. **supervisor 自动 pid 发现/unregister**：C-APP-CONTROL 行原第二半边，不在本片范围（任务书 MUST NOT 边界）。
6. **ROAD-B-005 其余 residuals 不因本片改变**：AgentInstance 未分面、pause/resume/cancel 宿主执行器、GUI 真机战役、semantic NL 语法、desktop 不在根 CI、Windows GUI（W34-A §5 沿引）。
