# W82-INT 预检、吊销回收联动与全 8 通道前置签认清册

- **证据 ID**：`w82-int-precheck-20261121`
- **任务 / 波次 / 车道**：`W82-INT-WAVE2` / Wave `W82-R2`（分支 `feat/w82-wave2-integration`，基线合流自 `feat/w82-c-accept-r1`、`feat/w82-d-py-r1`、`feat/w82-d-ts-r1`、`feat/del-base-audit-r1`）
- **日期**：`2026-11-21`
- **保障等级与状态**：`H4`（单机 macOS 实机与集成测试；`W82-INT` 整体维持 `IN_PROGRESS / PARTIAL`，`H8 = 0`）
- **合规边界**：严格零修改 `crates/nlos-capability`、`crates/nlos-driver-llm-http`、`crates/nlos-wasm-sandbox`、`schema/**`、`gen/**`、`desktop/**`、`Cargo.toml` 与 `Cargo.lock`；三项未签认新服务在 [w82-b-contract-20261116.md §4.2](./w82-b-contract-20261116.md#42-库层可信注入边界接口责任不是已实现类型) 约束下继续保持 fail-closed，不在未获签认前硬改库或绕过安全边界。

---

## 1. Wave 1 合流成果摘要（`W82-R2` 第一波次）

| 车道 ID | 权威交付物 / 证据 | 核验摘要与诚实边界 |
|---|---|---|
| `W82-C-ACCEPT` | [w82-c-acceptance-20261121.md](./w82-c-acceptance-20261121.md) | 落地 `is_valid_root_capability_slot`、`parse_root_capability_slot`（严格 `^[0-9a-f]{32}$`）、`CapabilityAuthority::inspect_active` + `holder == principal` 启动预检、`operator_root_capability=<32-hex>` READY 行输出及父退出同 `--root` 无冲突重启；macOS 本机验收全绿，三平台 CI 与跨权限/breakaway 边界保留 |
| `W82-D-PY-SDK` | [w82-d-py-sdk-20261121.md](./w82-d-py-sdk-20261121.md) | Python 12 通道 `SABI_V1_CHANNELS`、Ed25519 持久认证会话、5 个 SABI 新客户端（`WaitClient`/`OperationClient`/`LlmGatewayClient`/`ToolSandboxClient`/`BlobReferenceClient`）、源码树外 `verify_wheel.py`（`12` 个 `*_pb2.py` 零漂移）及对真实 5 通道守护进程 `6/6` 一致性测试；`SDK-3` 保持 `PARTIAL-PASS`，未晋升 `SDK-4` |
| `W82-D-TS-SDK` | [w82-d-ts-sdk-20261121.md](./w82-d-ts-sdk-20261121.md) | TypeScript 12 通道 `SABI_V1_CHANNELS`、纯 `node:crypto` Ed25519 认证会话、5 个 SABI 新客户端与跨进程守护进程 + 本地实 socket `12/12` 一致性测试；Windows Named Pipe 仅代码对齐、未发布 npm 且未晋升 `SDK-4` |
| `DEL-BASE-AUDIT` | [del-base-coverage-audit.md](../../management/del-base-coverage-audit.md) | 完成 `v0.5 §1.1` / `§28` 目标与交付包映射、`U-1..U-14` 状态复核与勘误清点（Wave 2 已完成共享索引挂接） |

---

## 2. Wave 2 合规边界内已闭环交付

1. **`nlos-compat-shim` 三保留名映射**（[sabi_adapter.rs](../../../crates/nlos-compat-shim/src/sabi_adapter.rs)）：
   - 在 `map_service_to_channel` 增补 `"llm_gateway" | SABI_LLM_GATEWAY_SCHEMA => nlos.sabi.LlmGateway`、`"tool_sandbox" | SABI_TOOL_SANDBOX_SCHEMA => nlos.sabi.ToolSandbox`、`"blob_reference" | SABI_BLOB_REFERENCE_SCHEMA => nlos.sabi.BlobReference`；
   - 新增单测 `map_service_to_channel_preserves_v1_0_and_adds_v1_1_reserved_names` 与 `epoch_adapter_cell_translates_v1_1_service_envelopes_preserving_idempotency`，验证 v1.0 既有映射零破坏与 v1.1 三服务信封跨纪元无损翻译（保留 `request_id` 与 128-bit `idempotency_key`）。
2. **`nlos-system-control` 令牌吊销联动受管进程树回收**（[daemon.rs](../../../crates/nlos-system-control/src/daemon.rs)、[daemon_capability_reclaim.rs](../../../crates/nlos-system-control/tests/daemon_capability_reclaim.rs)）：
   - 在不新增外部或服务库依赖的前提下，为 `SystemControlDaemon` 新增 `CapabilityWorkerBindingRequest`、`register_supervisor_pid_at_scope`（支持 `KillScope::ProcessGroup` 进程树作用域）、`register_capability_worker_binding`、`bind_capability_worker` 与 `reclaim_revoked_capability_workers(now_wall_ms) -> CapabilityRevocationReclaimReport`；
   - `reclaim_revoked_capability_workers` 通过 `CapabilityAuthority::inspect_active(entry.capability, now_wall_ms)` 核验已绑定能力及其完整祖先链：当 `daemon.capabilities` 未装配或能力仍有效时零副作用、不误伤任何进程；检出 `GenerationFenceConflict` / `CapabilityRevoked` / `AncestorRevokedOrFenced` 等失效原因时，经 `ProcessAuthority::request_platform_kill_with_registry` 与 `TreeAwareDaemonKillAdapter`（透传 `PlatformKillAdapter`，对 `KillScope::ProcessGroup` 执行优雅组信号 + 有界升级强杀排空）落盘不可变 `PlatformKillReceipt` 并回收整棵真实子进程树；
   - [daemon_capability_reclaim.rs](../../../crates/nlos-system-control/tests/daemon_capability_reclaim.rs) 实测覆盖：① 未配置 `--operator-key-file` 零副作用；② 真实父进程（`process_group(0)`）+ 孙进程 `sleep 60` + 兄弟进程在吊销前零误伤；③ `revoke_signed` 吊销目标委托能力后仅回收目标真实进程树（直子信号死亡且孙进程退出、落盘 `PlatformKillReceipt`），兄弟未吊销进程保持存活；④ 二次巡检幂等跳过已回收条目；⑤ 吊销祖先根能力级联触发 `AncestorRevokedOrFenced` 回收剩余兄弟进程树。
3. **14 处「13 导入 → 14 导入」WASI 白名单勘误清零**：
   - 统一修正 [e-sandbox-001](./e-sandbox-001-cross-platform-wasm-process-isolation.md)（L32）、[e-ledger-001](./e-ledger-001-v13-conformance-audit.md)（L124）、[w81-stage-e-exit-gates](./reviews/w81-stage-e-exit-gates.md)（L83、L166）、[risks.yaml](../../management/risks.yaml)（L237）、[exit-unknown-risks.md](../../management/exit-unknown-risks.md)（L50、L65）与 [stage-e-progress.md](../../management/stage-e-progress.md)（L120、L134、L190、L319、L338、L366、L493）共 14 处笔误为 `14 导入`，完整保留 `stage-e-progress.md` §E.6.1.8 第 575 行原始勘误登记说明。
4. **索引挂接与台账同步**：
   - 将 [del-base-coverage-audit.md](../../management/del-base-coverage-audit.md) 挂入 [README.md](../../management/README.md) 与 [nlos-goal-delivery-plan.md](../../management/nlos-goal-delivery-plan.md)（满足 `[PD-CREATE-001]` 无孤儿文档）；
   - 将本波 4 份新 Evidence 入账 [evidence-index.yaml](../../management/evidence-index.yaml)（`213 -> 217`，阶段 E `24 -> 28`），同步更新 [stage-e-progress.md](../../management/stage-e-progress.md)、[nlos-goal-delivery-plan.md](../../management/nlos-goal-delivery-plan.md) 与 [00-讨论索引.md](../../discussions/00-讨论索引.md)。

---

## 3. 阻塞全 8 通道实时挂载的 3 项前置契约/实现缺口

经逐行核对三个 W82-B 服务库及线协议与双语言 SDK，下列 3 项缺口无法在不修改受保护目录的前提下由 `nlos-system-control` 单侧绕过或合规满足：

| 编号 | 阻塞面 | 精确源码/契约锚点 | 阻塞实质与前置解锁条件 |
|---|---|---|---|
| **GAP-1** | `BlobReferenceService` 强制进程内 Ed25519 sidecar 签名与非空摘要校验 | `crates/nlos-capability/src/blob_reference.rs:231`（`mint_reference` 缺 proof 返回 `BlobError::MissingSignature`）、`:475`（`release_reference` 同理）、`:570-597` & `:488-497`（要求调用者 `KeyPurpose::SemanticSigning` 私钥签 `mint_command_message`/`release_command_message`，覆盖 `BlobCommandScope`）、`:925`（`operation()` 强制 `proposal_or_input_digest_sha256 == digest` 拒空）；`crates/nlos-capability/src/blob_model.rs:62-86, 323-380`；`schema/nlos/sabi/v1/blob_reference.proto:39-54, 92-95`；`schema/nlos/sabi/v1/envelope.proto:45-55`；`sdk/python/nlos_sdk/blob_reference.py:298-307, 397-405, 467-475`（`_client_base.py:517` 默认 `b""`） | 线协议 `MintReferenceRequest`/`ReleaseReferenceRequest`/`SabiRequestContext` 均无命令签名与 `BlobCommandScope` 字段；Py/TS SDK 无命令私钥签名通道；服务端不可持有任意客户端私钥或违背 `ADR-0010`（`crates/nlos-capability/src/lib.rs:758-760`）为未签名线调用代签；同时 Python `BlobReferenceClient` 未传递且不可配置 `proposal_or_input_digest_sha256`，导致连 `read_reference` 也必然被 `operation()` 拒绝。**解锁条件**：明确线端命令签名/scope 传输约定（或认证会话衍生接口）与 `operation()`/Python SDK 摘要对齐。 |
| **GAP-2** | `Gateway::handle_wire` 外层 `Envelope.schema.name` 错配与 `attenuate_budget` 无绑定通道 | `crates/nlos-driver-llm-http/src/gateway_wire.rs:53`（要求入站 `Envelope.schema.name == "nlos.sabi.LlmGateway"`，否则 `IncompatibleSchema`）、`:254`（回包构造 `llm_gateway_schema_identity()`）；对比 `sdk/python/nlos_sdk/_client_base.py:596, 647`、`sdk/typescript/src/sabi_client.ts:312`、`crates/nlos-wasm-sandbox/src/tool_service.rs:1013`、`crates/nlos-capability/src/blob_reference.rs:879`（统一校验 `"nlos.sabi.Envelope"`）；`crates/nlos-driver-llm-http/src/llm_gateway.rs:895` & `gateway_wire.rs:148`（`attenuate` 强制要求宿主 `ChildSessionBinding`）；`schema/nlos/sabi/v1/llm_gateway.proto:81-92`；`docs/evidence/stage-e/w82-b-contract-20261116.md:121` | `Gateway::handle_wire` 与双语言 SDK 及另外两个服务在 `Envelope.schema.name` 上直接冲突；同时 `AttenuateBudgetRequest`/`Result` 无 child 标识入参及可消费返回句柄，且 `w82-b-contract-20261116.md` §4.3 明确禁止无真实绑定时静默虚构内部 child。**解锁条件**：修正 `gateway_wire.rs` 外层 `Envelope.schema` 校验/封装，并由 INT 与契约方裁定 `ChildSessionBinding` 的真实进程注册或 additive wire 扩展路径。 |
| **GAP-3** | `CapabilityRights` 分服务权位与权威授权策略未获维护者签认 | `crates/nlos-capability/src/model.rs`（仅定义 `SEMANTIC_APPEND/RETRACT/ADJUDICATE/DELEGATE` 四位与 `Namespace/Task` 目标）；`docs/evidence/stage-e/w82-b-contract-20261116.md:117` | §4.2 明确要求不得虚构 `LLM/Tool/Blob` 权位，亦不得让现有语义权位自动批准新服务，须待维护者明确现有 `Capability` + `purpose`/`target` 到三新服务方法的策略映射或批准扩展后方可开放挂载。 |

*(注：经对照源码，`crates/nlos-wasm-sandbox/src/tool_service.rs:223-414, 1001-1062` 仅校验 `SABI_ENVELOPE_SCHEMA` 且 `check_digest` 容忍空 `proposal_or_input_digest_sha256`，无需改动其库源码即可适配挂载，但仍受 GAP-3 统一权限策略签认及运行期 `nlos-wasm-sandbox-host` 路径与固定 `ToolRegistration` 注册表策略约束。)*

---

## 4. 诚实边界与整树验证回执

- **运行环境与跨平台边界**：本文件全部实测在单机 macOS（`Darwin 25.6.0` arm64）工作树内执行；Windows 与 Linux 远端三平台 CI、外部 `qq-agent` 消费闭环（`E-DOGFOOD-001` 仍为前置约束）与全 8 通道实时挂载尚未在本波完成；全仓 `H8 = 0`；阶段 E 保持 `IN_PROGRESS / NOT EXITED` 且 `SABI v1.0` 长期冻结仍待人类维护者显式签字。
- **验证回执（22 项命令，全部代码与台账落地后实跑通过；仅文档格式/台账/空白检查在本节表格定稿后再次复跑）**：

| # | 验证命令 | 退出码 | 实测原始输出摘要与计数 |
|---|---|---|---|
| 1 | `cargo fmt --all -- --check` | `0` | 全工作区 Rust 格式化零差异（定稿后复跑通过） |
| 2 | `cargo clippy -p nlos-package -p nlos-process -p nlos-up -p nlos-system-control -p nlos-compat-shim -p nlos-schema --all-targets -- -D warnings` | `0` | 6 核心 crate（含全测试目标）严格 Clippy 零警告 |
| 3 | `cargo clippy -p nlos-system-control --features daemon --all-targets -- -D warnings` | `0` | `daemon` feature 全目标严格 Clippy 零警告 |
| 4 | `cargo build -p nlos-system-control --features daemon --bin system-control-daemon` | `0` | 守护进程二进制编译通过，供 Python/TS 一致性测试直接起服 |
| 5 | `cargo test -p nlos-package -p nlos-process -p nlos-up -p nlos-compat-shim -p nlos-schema` | `0` | 共 `282 passed; 0 failed; 0 ignored`（`nlos-compat-shim` 42、`nlos-package` 61、`nlos-process` 96、`nlos-schema` 56、`nlos-up` 27） |
| 6 | `cargo test -p nlos-system-control --features daemon` | `0` | 共 `207 passed; 0 failed; 0 ignored`（`lib` 71 + 集成测试 135 + `doc` 1；含 `daemon_capability_reclaim` 2、`daemon_session_mux` 17、`daemon_operator_root` 4；`windows_named_pipe` 在 Darwin 为 0 active tests） |
| 7 | `python3 -m unittest discover -s sdk/python/tests` | `0` | `Ran 9 tests in 1.563s — OK` |
| 8 | `python3 sdk/python/scripts/verify_wheel.py` | `0` | `12` 个 `*_pb2.py` 与 `gen/python/` 逐字节零漂移；fresh venv 导入 `IMPORT-OK 27`，`ALL PASS` |
| 9 | `python3 tests/conformance/schema/envelope.py` | `0` | Schema Envelope 编码/解码与版本校验通过 |
| 10 | `python3 tests/conformance/ipc/authenticated_session.py` | `0` | `ALL 5/5 CASES PASSED`（认证建立、入队/收包/检视、错钥拒绝、明文对照） |
| 11 | `python3 tests/conformance/ipc/sabi_v1_1_python.py` | `0` | `ALL 6/6 SABI v1.1 CONFORMANCE CASES PASSED` |
| 12 | `npm run schema:typecheck` | `0` | `tsc` 类型检查零报错 |
| 13 | `npm run schema:test:typescript` | `0` | `tsx tests/conformance/schema/envelope.ts` 通过 |
| 14 | `npm run ipc:test:typescript` | `0` | `tsx tests/conformance/ipc/client.ts` 通过 |
| 15 | `npm run directory:test:typescript` | `0` | `tsx tests/conformance/ipc/directory_chain.ts` 通过 |
| 16 | `npm run wait-control:test:typescript` | `0` | `tsx tests/conformance/ipc/wait_control.ts` 通过 |
| 17 | `npm run sabi-v1-1:test:typescript` | `0` | `ALL 12 SUITES PASSED` |
| 18 | `python3 tools/check_golden_immutability.py` | `0` | `[GOLDEN-IMMUTABILITY-OK] legacy_root=7 files; epochs=(v1.0: 9 channels / 11 vectors, v1.1: 3 channels / 6 vectors, v1.1-blob-20261116: 1 channels / 1 vectors)`（定稿后复跑通过） |
| 19 | `python3 scripts/lint_claims.py` | `0` | `claims: 169 (DONE 6, IN_PROGRESS 9, NOT_STARTED 1, PARTIAL_PASS 153) · risks: 17 · evidence-index: 217/217`，`0 项 ERROR，0 项 INFO`（定稿后复跑通过） |
| 20 | `python3 tools/verify_stage_e_ledger_coverage.py --mode strict` | `0` | `Stage E reqs=45 (E.1.1=44 + E.2.2=1), work_packages=8, claims=169, risks=17 (open_P0=0), evidence=217/217 (stage-e=28/28), md_scanned=30 (links=1427)`（定稿后复跑通过） |
| 21 | `python3 scripts/run_road_e001_upgrade_matrix.py` | `0` | `Status: PASS`，`Sealed Baseline Files: 16`，`Frozen .nlos-pkg Count: 8`，`Zero-Recompile Verified: True`，`Manifest Violations: 0`，`Cargo Gate: rc=0, passed=6, failed=0, elapsed_ms=52741` |
| 22 | `git diff --check` | `0` | 全改动零尾随空白与冲突标记（定稿后复跑通过） |
