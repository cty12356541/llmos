# W82-R3 Gateway Wire 外层信封一致性修复（GAP-2 Part A）

- **证据 ID**：`w82-r3-gateway-envelope-fix-20261121`
- **任务 / 波次 / 车道**：`W82-R3-GW-FIX` / Wave `W82-R3`（分支 `feat/w82-r3-gw-fix`，基线 SHA `9414000f9b19179ca0922d95bbf81e21eff137ab`）
- **日期**：`2026-11-21`
- **保障等级与状态**：`H4`（单机 macOS 单 crate 实测通过；仅关闭 `GAP-2` Part A，`H8 = 0`）
- **依据**：[w82-int-precheck-20261121.md §3 (GAP-2)](./w82-int-precheck-20261121.md#3-阻塞全-8-通道实时挂载的-3-项前置契约实现缺口)、[w82-b2-llm-gateway-20261116.md](./w82-b2-llm-gateway-20261116.md)

---

## 1. 修复范围与关键实现事实

1. **对齐 `SABI_ENVELOPE_SCHEMA` 外层信封校验与封装**（[gateway_wire.rs](../../../crates/nlos-driver-llm-http/src/gateway_wire.rs)）
   - 入站校验：`Gateway::handle_wire` 在通过 `decode_sabi_envelope(frame)?` 后，将外层 `Envelope.schema` 校验由 `nlos_schema::SABI_LLM_GATEWAY_SCHEMA` 修正为 `SABI_ENVELOPE_SCHEMA`（`"nlos.sabi.Envelope"`），并显式对齐 [tool_service.rs](../../../crates/nlos-wasm-sandbox/src/tool_service.rs#L1008-L1016) 与 [blob_reference.rs](../../../crates/nlos-capability/src/blob_reference.rs#L876-L885) 的检查：要求 `schema.name == SABI_ENVELOPE_SCHEMA && schema.major == 1 && schema.critical_extension_ids.is_empty()`（允许合法 `minor` 演进如 `1.0` / `1.1`），任一不匹配返回 `GatewayError::IncompatibleSchema`。
   - 出站封装：`wire_reply` 对成功响应与 `SabiFailure` 错误回包均将外层 `Envelope.schema` 由 `Some(llm_gateway_schema_identity())` 修正为 `Some(envelope_schema_identity())`（`name = "nlos.sabi.Envelope"`, `major = 1`, `minor = 1`, `critical_extension_ids = []`, `non_critical_extension_ids = []`），并移除不再使用的 `llm_gateway_schema_identity` 导入。
   - **`nlos-schema` 接口实情**：核查 [crates/nlos-schema/src/lib.rs](../../../crates/nlos-schema/src/lib.rs#L132-L138) 可见 `SABI_ENVELOPE_SCHEMA` 与 `SABI_ENVELOPE_V1`（`1.1`）已注册，`decode_sabi_envelope` 通过 `validate_schema_identity` 对所有 `REGISTRY` 条目强制校验 `major` 与 `critical_extension_ids`，但 `nlos-schema` 当前未导出 `envelope_schema_identity()` 构造辅助函数（同仓 [control.rs](../../../crates/nlos-system-control/src/control.rs#L1536-L1542) 亦以内联 `SchemaIdentity` 构造）；受独占写集限制未改动 `crates/nlos-schema`，因此在 `gateway_wire.rs` 与测试辅助模块内各自提供本地 `envelope_schema_identity()`（与 `SABI_ENVELOPE_V1` `1.1` 及 Python/TS SDK 对齐）。
2. **保留内层业务载荷契约不变**（[llm_gateway.rs](../../../crates/nlos-driver-llm-http/src/llm_gateway.rs)）
   - `llm_gateway.rs` 全文零改动：四类内层业务请求/响应载荷（`LlmChat*`、`QueryBudget*`、`AttenuateBudget*`、`RecordExternalUsage*`）继续严格保持 `.schema = Some(llm_gateway_schema_identity())`；`attenuate_budget` 对宿主侧 `ChildSessionBinding` 的要求保持完整 untouched。
   - 核查 [gateway_fault_tests.rs](../../../crates/nlos-driver-llm-http/src/gateway_fault_tests.rs) 与 [gateway_support/mod.rs](../../../crates/nlos-driver-llm-http/tests/gateway_support/mod.rs)：两文件原有 `llm_gateway_schema_identity()` 引用全部位于内层业务请求（`AttenuateBudgetRequest`、`QueryBudgetRequest`、`LlmChatRequest`），无外层 `Envelope` 构造，故全部保留内层契约不变；仅在 `gateway_support/mod.rs` 新增 `pub fn envelope_schema_identity()` 供集成测试复用。
3. **测试更新与回归覆盖**（[gateway.rs](../../../crates/nlos-driver-llm-http/tests/gateway.rs)）
   - 将 `wire_call` 与 `check_response` 的外层 `Envelope.schema` 构造/断言更新为 `envelope_schema_identity()`，并在 `all_wire_methods_use_canonical_codec_and_reject_header_substitution` 增补四方法解码结果 `.schema == Some(llm_gateway_schema_identity())` 断言。
   - 新增三项明确回归测试：
     1. `wire_accepts_envelope_schema_minor_variants_and_preserves_inner_result_schema`：覆盖入站 `nlos.sabi.Envelope`（含 `minor = 0` 与 `minor = 1`）正向调用，验证出站外层 `Envelope.schema == Some(envelope_schema_identity())` 且解码内层 `*Result.schema == Some(llm_gateway_schema_identity())`；
     2. `wire_rejects_outer_gateway_schema_and_incompatible_envelope_versions`：覆盖入站外层 `Envelope.schema == Some(llm_gateway_schema_identity())`、`tool_sandbox_schema_identity()`、错 `service`、缺失 `schema`、`major != 1` 与非空 `critical_extension_ids` 均被明确拒绝为 `GatewayError::IncompatibleSchema`，且零 TCP 连接发出；
     3. `wire_failure_replies_wrap_sabi_failure_in_envelope_schema`：覆盖权限拒绝（`SabiErrorCode::Rights`）与内层载荷缺 schema（`SabiErrorCode::AbiVersion`）两条 `SabiFailure` 回包路径，验证回包外层 `Envelope.schema == Some(envelope_schema_identity())` 且 `payload` 为空。

---

## 2. 验证环境与本机实测输出

- **实机环境**：`Darwin 25.6.0 arm64` / `macOS 26.6.2 (25G83)`、`rustc 1.99.0 (b940084d7 2026-09-28)`
- **执行工作树**：`/Users/lipunima/.gemini/antigravity/brain/875ab7ba-845f-40de-8ce8-81186117c749/scratch/wt-w82-r3-gw-fix`

| # | 验证命令 | 退出码 | 实测计数与结果 |
|---|---|---:|---|
| 1 | `cargo fmt -p nlos-driver-llm-http -- --check` | `0` | crate `nlos-driver-llm-http` 格式化检查零差异 |
| 2 | `cargo clippy -p nlos-driver-llm-http --all-targets -- -D warnings` | `0` | lib / bin / tests 全目标严格 Clippy（含 workspace `pedantic`）零警告、零错误 |
| 3 | `cargo test -p nlos-driver-llm-http` | `0` | **共 42 passed; 0 failed; 0 ignored**（分集：lib 单元测试 `10 passed`、`bin/nlos-llm-pool-cli` `0 passed`、`tests/gateway` `22 passed` [原 19 + 新增 3 项外层信封回归]、`tests/keypool_and_budget_gate` `9 passed`、doc-test `1 passed` [compile_fail]） |
| 4 | `git diff --check` | `0` | 零尾随空白、零冲突标记 |

---

## 3. 诚实边界与未闭合缺口

1. **仅关闭 `GAP-2` Part A**：本交付仅解决 `Gateway::handle_wire` 与 `wire_reply` 的外层 `Envelope.schema`（`nlos.sabi.Envelope`）错配问题。
2. **以下阻塞与验证范围仍未闭合**：
   - **`GAP-2` Part B**：`attenuate_budget` 对宿主可信 sidecar `ChildSessionBinding` 的强制要求及其线协议/进程注册路径仍未解决（归 `ADR-0028` / 后续集成裁定）；
   - **`GAP-1`**：`BlobReferenceService` 命令级 Ed25519 sidecar 签名与 `proposal_or_input_digest_sha256` 线协议对齐仍未闭合；
   - **`GAP-3`**：`CapabilityRights` 分服务权位与权威授权策略映射仍待维护者签认；
   - **全仓 / 跨平台 / 集成门**：`nlos-system-control` 尚未在本车道挂载 `llm_gateway`，未运行全工作区与 Python/TS SDK 联调，亦未在 Linux/Windows 远端 CI 复验；`H8 = 0`。
