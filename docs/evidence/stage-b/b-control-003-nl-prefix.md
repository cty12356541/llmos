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
