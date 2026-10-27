# D-IPC-ENUM / W69-D-IPC-ENUM：`IPC-D1..D8` 12 域有界游标枚举、可插拔 `Cell` 检视接缝、五大法定不可篡改锁与四入口字节级 `ControlReceipt` 对称性

- 状态：`PARTIAL_PASS`（SABI `system_control.proto` 纯增量扩展 `ControlScope 3..=17` / `SystemControlView 9..=31` / `ControlCommand 21..=30`、`crates/nlos-system-control` 12 域有界游标枚举 + 单调水位摘要 + 级联爆炸半径 dry-run + `ControlCommandId` 对账恢复 + 五大法定不可篡改禁区锁、Tauri 桥 25 个新增 `#[tauri::command]` 与 `desktop/src/{types.ts,ipc.ts}` 契约同步、以及 Windows Named Pipe 下 `GUI / NL / CLI / API` 四入口 `receipt_hex` 逐字节等价性集成测试 `ipc_enum_symmetry_side.rs` 全部落库通过；`desktop/src-tauri/src/lib.rs` `invoke_handler` 注册留归同波汇聚节点 `W69-INT`，真实跨机 `CellHost` 注入与全桌面 UI 挂载留归 `W71–W74`）
- Assurance：`H3`（确定性 SABI 编解码与边界约束单测、跨进程认证 Named Pipe + CLI 子进程 + NL 编译器 + In-Process API 四入口逐字节 `ControlReceipt` 对等集成测试、法定不可篡改禁区协议级拒绝矩阵核验）
- 日期：2026-10-27
- Owner：`W69-D-IPC-ENUM`（Task `D-IPC-ENUM`, Attempt 1）
- 设计依据：
  - [06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §22 `[CONS-CTRL-001]`；§24.1.1 `[PLAN-CHANGE-001]`、`[PLAN-LAZY-001]`；§24.2 `[TUI-CHOOSER-001]`、`[TUI-EXT-001]`；§24.3 `[RCPT-UNDO-001]`；§25 `[TASK-SPACE-001]`、`[TASK-VIS-001]`；§25.3 `[CTRL-PARITY-001]`、`[CTRL-HIER-001]`、`[CTRL-RECOVERY-001]`；§25.4 `[DUI-LAUNCH-001]`、`[DUI-CONTROL-001]`、`[DUI-CHOOSER-001]`、`[DUI-ASSOC-001]`、`[DUI-SETTINGS-001]`
  - [阶段 D 完整桌面视觉与交互设计规格（`W68-R2`）](../../superpowers/specs/2026-10-26-stage-d-desktop-visual-and-ux-design.md) §2A.1 `SOTA-3..6`、§4.2 `IPC-D1..D8`、§4.3 法定不可篡改禁区
  - [ADR-0011：IPC Principal 挑战-响应认证与签名透传](../../management/adrs/0011-ipc-principal-auth-signature-passthrough.md)
  - [ADR-0014：SABI v1-beta Schema 通道冻结与纯增量演进纪律](../../management/adrs/0014-schema-channel-freeze-v1-beta.md)
  - [ADR-0025：阶段 D 混合自然语言桌面合成层、L0–L3 渐进式披露、12 域显控同构与可信 UI 防伪边界](../../management/adrs/0025-stage-d-hybrid-desktop-compositor-and-trusted-ui-layering.md)（决定 3 & 决定 4，`[UI-PARITY-001]`）
  - [阶段 D 权威进度单](../../management/stage-d-progress.md) §D.1.1、§D.2.2、§D.3.1 `D-IPC-ENUM` & §D.5.2 `W69-D-IPC-ENUM`
- 前置证据：[b-gui-001-task-manager-shell.md](../stage-b/b-gui-001-task-manager-shell.md)、[c-unfreeze-001-search-notify-production-wiring.md](../stage-c/c-unfreeze-001-search-notify-production-wiring.md)
- 快照基线：`a8bbfa219d37a2e7032a57e390be82533c6fdd5a`

---

## 1. 实现事实与交付工件

本车道严格限制在声明互斥写集内交付以下 19 个文件（`WriteSet(W69-D-IPC-ENUM) ∩ WriteSet(W69-D-TOKENS) = ∅` 且 `WriteSet(W69-D-IPC-ENUM) ∩ WriteSet(W69-C-TRANSPORT) = ∅`，零修改 `crates/nlos-system-control/Cargo.toml`、`desktop/src-tauri/Cargo.toml`、`desktop/src-tauri/src/lib.rs`、`crates/nlos-cell/**` 或 `crates/nlos-lease/**`）：

### 1.1 纯增量 SABI Schema 与多语言生成物（符合 `ADR-0014`）

1. [`schema/nlos/sabi/v1/system_control.proto`](../../../schema/nlos/sabi/v1/system_control.proto)、[`gen/typescript/nlos/sabi/v1/system_control_pb.ts`](../../../gen/typescript/nlos/sabi/v1/system_control_pb.ts)、[`gen/python/nlos/sabi/v1/system_control_pb2.py`](../../../gen/python/nlos/sabi/v1/system_control_pb2.py)：
   - **`ControlScope` 纯增量扩展（`3..=17`，保留 `UNSPECIFIED = 0`、`SYSTEM = 1`、`OPERATION = 2` 原编号与原语义不变）**：新增 `WORKSPACE (3)`、`APPLICATION (4)`、`TASK (5)`、`TASK_GROUP (6)`、`TASK_NODE (7)`、`TASK_ATTEMPT (8)`、`PROCESS (9)`、`AGENT_INSTANCE (10)`、`EXECUTION_FIBER (11)`、`RESOURCE_GROUP (12)`、`CONTEXT (13)`、`TOPIC (14)`、`CHANNEL (15)`、`DEVICE (16)`、`CELL (17)`；
   - **`SystemControlView` 纯增量扩展（`9..=31`，保留 `1..=8` 原编号不变）**：覆盖 `TASK_LIST (9)`、`TASK_GROUP_LIST (10)`、`TASK_NODE_LIST (11)`、`TASK_ATTEMPT_LIST (12)`、`EXECUTION_FIBER_LIST (13)`、`OPERATION_LIST (14)`、`INSTALLED_APPLICATION_LIST (15)`、`SEARCH_CANDIDATE_QUERY (16)`、`DESKTOP_WATERMARK_POLL (17)`、`SETTINGS_DOMAIN (18)`、`CONTENT_TYPE_ASSOCIATION_LIST (19)`、`KEYCHAIN_REFERENCE_LIST (20)`、`CHOOSER_GRANT_LIST (21)`、`WORKSPACE_LIST (22)`、`WORKSPACE (23)`、`PROCESS_LIST (24)`、`RESOURCE_RESERVATION_LIST (25)`、`TOPIC_LIST (26)`、`NOTIFICATION_LIST (27)`、`CELL_LIST (28)`、`CELL_LEASE_INSPECT (29)`、`CONTROL_BLAST_RADIUS_PREVIEW (30)`、`CONTROL_COMMAND_STATUS (31)`；
   - **`ControlCommand.command` oneof 纯增量分支（`21..=30`，保留 `7..=20` 原编号与字节表示不变）**：`request_process_migration (21)`、`purge_topic (22)`、`update_settings_domain (23)`、`set_content_type_association (24)`、`issue_chooser_grant (25)`，以及协议级法定不可篡改禁区校验臂 `mutate_search_candidate (26)`、`undo_receipt (27)`、`mutate_task_node_topology (28)`、`override_task_attempt_commit (29)`、`override_effect_unknown_operation (30)`。
2. [`crates/nlos-schema/src/lib.rs`](../../../crates/nlos-schema/src/lib.rs) 与 [`crates/nlos-schema/tests/compatibility.rs`](../../../crates/nlos-schema/tests/compatibility.rs)：
   - 新增常量 `MAX_SYSTEM_CONTROL_ENUM_LIMIT = 64`、`MAX_DESKTOP_WATERMARK_EVENT_LIMIT = 128` 与 `SEARCH_NON_AUTHORITATIVE_RESTRICTIONS_ALL = 0x1F`；
   - 为全部 23 个新增快照消息与 10 个新增命令分支补齐有界编解码与 fail-closed 结构校验（包含 `0x1F` 搜索限制位硬校验、`KeychainReference` 零私钥字段结构约束、`DurableOperation` 终态强制携带 `outcome_receipt` 校验等）。

### 1.2 后端 `crates/nlos-system-control` 可插拔 12 域检视接缝、法定禁区守卫与四入口对等编译

1. [`crates/nlos-system-control/src/cell_inspector.rs`](../../../crates/nlos-system-control/src/cell_inspector.rs)（**`IPC-D7` 可插拔 `Cell` 拓扑与三族租约接缝**）：
   - 定义 `CellTopologyInspector`（`list_cells` + `request_process_migration`）与 `CellLeaseInspector`（`inspect_cell_leases`）只读/受控迁移 trait，以及 `LocalSingleNodeCellInspector` 确定性实现；
   - 原生支持 `OnlineLocal / OnlineRemote / Partitioned / Offline` 可达性、`PartitionAutonomyGate`（对 `Partitioned` 或 `FencedReadOnly / Quarantined` 目标节点确定性返回 `SABI_ERROR_CODE_FENCED`，对齐 `[CELL-FENCE-001]`）及 `Quota / Capacity / ExclusiveDevice` 三族租约与 `Quarantined` 锁态投影，零侵入 `nlos-cell` / `nlos-lease`。
2. [`crates/nlos-system-control/src/desktop_enum.rs`](../../../crates/nlos-system-control/src/desktop_enum.rs)（**`IPC-D1..D6` + `IPC-D8` 可插拔工作站枚举、单调水位与对账账本**）：
   - 定义 `DesktopEnumerationSource`、`DesktopDomainMutationExecutor`、`CommandStatusReconciliationSource` trait、`CommandStatusLedger` 幂等命令对账账本与 `InMemoryDesktopControlSurface`；
   - 实现全 12 域 `limit <= 64` 稳定 16 字节字典序游标分页（`PollDesktopWatermarkEvents` `limit <= 128` 附 `llmos/desktop-watermark/v1\0` 域分离 SHA-256 状态摘要）、`[PLAN-LAZY-001]` `MetadataOnly` / `Cold` 未物化节点折叠计数、显式区分 `SABI_ERROR_CODE_NOT_FOUND` 与携带当前权威代际的 `StaleGeneration { expected_generation, current_generation }`、四层 `SettingsDomain`（`system / user / workspace / application`）CAS、`ContentTypeAssociation`、零明文私钥 `KeychainReference`（仅 `32B` 公钥指纹 + 元数据）、`ChooserGrant`、以及级联 `+ / ~ / -` 爆炸半径计算器。
3. [`crates/nlos-system-control/src/stage_d_handler.rs`](../../../crates/nlos-system-control/src/stage_d_handler.rs)、[`crates/nlos-system-control/src/lib.rs`](../../../crates/nlos-system-control/src/lib.rs)、[`crates/nlos-system-control/src/auth.rs`](../../../crates/nlos-system-control/src/auth.rs)、[`crates/nlos-system-control/src/daemon.rs`](../../../crates/nlos-system-control/src/daemon.rs)：
   - 在统一 `RecoverySystemControl` 请求处理管线内闭环 `SystemControlView 9..=31`、显式 `ControlScope 1..=17` 路由、命令对账账本自动登记与五大法定不可篡改禁区协议级硬拦截；
   - `SystemControlDaemon` 默认挂载工作站表面与单节点 `Cell` 检视器，公开 `auth::serve_validated` 供跨平台测试与守护进程共享同一 `AuthorityClock` 墙钟锚定逻辑。
4. [`crates/nlos-system-control/src/control.rs`](../../../crates/nlos-system-control/src/control.rs)、[`crates/nlos-system-control/src/control_stage_d.rs`](../../../crates/nlos-system-control/src/control_stage_d.rs)、[`crates/nlos-system-control/src/nl.rs`](../../../crates/nlos-system-control/src/nl.rs)、[`crates/nlos-system-control/src/nl_stage_d.rs`](../../../crates/nlos-system-control/src/nl_stage_d.rs)、[`crates/nlos-system-control/src/bin/system-control-cli.rs`](../../../crates/nlos-system-control/src/bin/system-control-cli.rs)：
   - 保持 `W32..W67` 既有 `ControlCommand` / `ControlOutcome` / `ControlReceipt::to_bytes` 金标向量 **100% 零改动兼容**；
   - 为全部 Stage D 读写原语补齐确定性 `ControlReceipt` 编码、受限中英文双语 NL 语法编译器（兼容 snake_case / kebab-case / Proto 枚举名作用域词法）以及跨平台（Windows Named Pipe + Unix Socket）`system-control-cli` 子命令。

### 1.3 Tauri 桥接层、前端强类型契约与跨平台对称性测试套件

1. [`desktop/src-tauri/src/dto.rs`](../../../desktop/src-tauri/src/dto.rs) 与 [`desktop/src-tauri/src/ipc.rs`](../../../desktop/src-tauri/src/ipc.rs)：
   - 闭合 `W35-P11` / `W66` 存量断头路：接通 `inspect_search`（在 Tauri 桥边界二次强制校验 `non_authoritative_restrictions_bits == 0x1F`，篡改即 fail-closed 返回 `ErrorCode::Control`）、`inspect_notification`、以及 `ControlAction::DisableApplication` / `ControlAction::UninstallApplication`；
   - 落地全部 Stage D 只读分页/检视/预演/对账命令（派发前强制校验 `1 <= limit <= 64`，水位轮询 `1 <= limit <= 128`）与显式 `ControlScope` / 领域写入动作编译，支持 `AppState::with_desktop_surface` 会话级可插拔表面注入。
2. [`desktop/src/types.ts`](../../../desktop/src/types.ts) 与 [`desktop/src/ipc.ts`](../../../desktop/src/ipc.ts)：
   - 1:1 镜像后端 `OutcomeDto`、12 域投影 DTO、`ControlScopeDto`、`StageDControlActionInput` 与全部 25 个新增 IPC 包装函数（前端调用前同步执行 `limit <= 64` / `<= 128` 与 `0x1F` fail-closed 断言）。
3. [`desktop/src-tauri/tests/ipc_enum_symmetry_side.rs`](../../../desktop/src-tauri/tests/ipc_enum_symmetry_side.rs)：
   - `#![cfg(all(any(unix, windows), feature = "dev-fixture"))]` 跨平台端到端集成测试套件，在同一进程内同时拉起 `ADR-0011` 认证端点与 CLI Plain 端点并真实拉起 `system-control-cli` 子进程，完整验证 12 域分页、`0x1F` fail-closed、级联爆炸半径、命令状态对账、五大法定不可篡改锁及四入口 `receipt_hex` 逐字节相等。

---

## 2. 需求闭合、法定不可篡改禁区与 `W69-INT` 交接清单

### 2.1 `IPC-D1..D8` 与存量缺口闭合矩阵

| 编号 / 缺口 | 规范锚点 | 落地摘要 | 验证测试锚点 |
|---|---|---|---|
| `IPC-D1`（闭合 `TASK_SPACE_GAPS #1`） | `[TASK-SPACE-001]`、`[UI-PARITY-001]` | `ListTasks`、`ListTaskGroups` 16B 游标分页（`1 <= limit <= 64`） | `twelve_domains_cursor_pagination_bounds_and_stale_generation_distinction` |
| `IPC-D2`（闭合 `TASK_SPACE_GAPS #2`） | `[TASK-VIS-001]`、`[RCPT-UNDO-001]` | `TaskSummaryDto` 完整携带 `application_id`、`installation_generation`、`plan_revision`、`task_state`、`undo_class`、`reservation_id`、`cell_id` 与 CAS 令牌 | 同上 |
| `IPC-D3`（闭合 `TASK_SPACE_GAPS #3`） | `[PLAN-LAZY-001]`、`[TASK-VIS-002]` | `ListTaskNodes`（含 `metadata_only_collapsed_count` / `cold_collapsed_count`）、`ListTaskAttempts`（含 `provider_isolation_unverified` / `cas_winner`）、`ListExecutionFibers`（含 `shared_thread_like` 与五段耗时）、`ListOperations`，严格区分 `SABI_ERROR_CODE_NOT_FOUND` 与 `OutcomeDto::StaleGeneration` | 同上 |
| `IPC-D4` | `[DUI-LAUNCH-001]`、`[CONS-CTRL-001]` | `InspectSearch` + `QuerySearchCandidates`（桥接层硬锁 `0x1F` 掩码 fail-closed）与 `ListInstalledApplications`（按 `ApplicationId` / `InstallationId` 索引） | `inspect_search_0x1f_fail_closed_inspect_notification_and_application_lifecycle_wiring` |
| `IPC-D5` | `[DUI-SETTINGS-001]`、`[DUI-ASSOC-001]`、`[DUI-CHOOSER-001]` | `PollDesktopWatermarkEvents`（`limit <= 128`，单调水位 + SHA-256 状态摘要）、四层 `SettingsDomain` 检视与 CAS 更新、`ContentTypeAssociation`、零私钥 `KeychainReference`、`ChooserGrant` | `twelve_domains_cursor_pagination_bounds_and_stale_generation_distinction` |
| `IPC-D6` | `[UI-PARITY-001]` | `ListWorkspaces` + `InspectWorkspace`、`ListProcesses`、`ListResourceReservations`（Nomad `actual / effective / requested` 三水位）、`ListTopics`、`ListNotifications` + `InspectNotification` | 同上 + `inspect_search_0x1f_fail_closed_...` |
| `IPC-D7` | `[CELL-LEASE-001]`、`[CELL-FENCE-001]` | 可插拔 `ListCells`、`InspectCellLeases`（含 `Quarantined` `ExclusiveDevice` 租约）与受控 `RequestProcessMigration`（对分区围栏节点返回 `SABI_ERROR_CODE_FENCED`） | `four_entry_receipt_hex_byte_parity_across_reads_and_scoped_mutations` |
| `IPC-D8` | `[CTRL-PARITY-001]`、`[CTRL-HIER-001]`、`[CTRL-RECOVERY-001]` | 显式 `ControlScope (1..=17)`、`DisableApplication` / `UninstallApplication`、`PreviewControlBlastRadius`（`+ / ~ / -` 级联影响 + 法定锁预演拦截）、`QueryControlCommandStatus` 对账恢复 | `blast_radius_preview_and_idempotent_command_status_reconciliation` |

### 2.2 五大法定不可篡改禁区（`[UI-PARITY-001]`）协议级拦截与四入口字节对等矩阵

| 法定锁序号 | 禁止动作语义 | SABI 命令分支 | 权威拒绝码（`SabiErrorCode`） | 四入口 `receipt_hex` 对等实测 |
|---|---|---|---|---|
| 法定锁 #1 | 对非权威 `0x1F` 搜索候选令牌直接发起写操作 | `MutateSearchCandidate (26)` | `SABI_ERROR_CODE_RIGHTS`（`DoNotRetry`） | `GUI == NL == CLI == API`（通过） |
| 法定锁 #2 | 对 `IRREVERSIBLE` 等级回执发起撤销 | `UndoReceipt (27)` | `SABI_ERROR_CODE_STATE`（`DoNotRetry`） | `GUI == NL == CLI == API`（通过） |
| 法定锁 #3 | 修改处于 `ACTIVE` 或 `COMPLETED` 态 `TaskNode` 的计划拓扑 | `MutateTaskNodeTopology (28)` | `SABI_ERROR_CODE_STATE`（`DoNotRetry`） | `GUI == NL == CLI == API`（双态均通过） |
| 法定锁 #4 | 绕过 `CommitCoordinator` CAS 人工覆写 `TaskAttempt` 提交胜出状态 | `OverrideTaskAttemptCommit (29)` | `SABI_ERROR_CODE_RIGHTS`（`DoNotRetry`） | `GUI == NL == CLI == API`（通过） |
| 法定锁 #5 | 未经对账证据人工将 `Operation(EFFECT_UNKNOWN)` 标记为成功或未发生 | `OverrideEffectUnknownOperation (30)` | `SABI_ERROR_CODE_STATE`（`DoNotRetry`） | `GUI == NL == CLI == API`（`true` / `false` 双分支均通过） |

### 2.3 供 `W69-INT` 注册至 `desktop/src-tauri/src/lib.rs` `tauri::generate_handler![...]` 的 25 个新增命令符号清单

本车道严格遵守写集边界未触碰 `desktop/src-tauri/src/lib.rs`。`W69-INT` 集成车道在 `desktop/src-tauri/src/lib.rs` 的 `tauri::generate_handler![...]` 中追加以下 **25 个 `ipc::*` 命令函数**即可完成全部 Tauri WebView IPC 暴露（所有新增 Stage D 写侧 `ControlAction` 变体已自动经既有 `ipc::submit_control` 暴露）：

1. `ipc::inspect_search`
2. `ipc::inspect_notification`
3. `ipc::list_tasks`
4. `ipc::list_task_groups`
5. `ipc::list_task_nodes`
6. `ipc::list_task_attempts`
7. `ipc::list_execution_fibers`
8. `ipc::list_operations`
9. `ipc::list_installed_applications`
10. `ipc::query_search_candidates`
11. `ipc::poll_desktop_watermark_events`
12. `ipc::inspect_settings_domain`
13. `ipc::list_content_type_associations`
14. `ipc::list_keychain_references`
15. `ipc::list_chooser_grants`
16. `ipc::list_workspaces`
17. `ipc::inspect_workspace`
18. `ipc::list_processes`
19. `ipc::list_resource_reservations`
20. `ipc::list_topics`
21. `ipc::list_notifications`
22. `ipc::list_cells`
23. `ipc::inspect_cell_leases`
24. `ipc::preview_control_blast_radius`
25. `ipc::query_control_command_status`

---

## 3. 验证结果（2026-10-27，Windows x86_64，Named Pipe 真实跨进程传输）

### 3.1 执行命令与实测结果摘要

```text
$ npm run schema:lint
> buf lint && buf format -d --exit-code
  -> EXIT CODE: 0

$ npm run schema:generate
> buf generate && node tools/normalize-generated-schema.mjs
  -> EXIT CODE: 0 (仅更新 gen/typescript/nlos/sabi/v1/system_control_pb.ts 与 gen/python/nlos/sabi/v1/system_control_pb2.py；注：tools/check-generated-schema.mjs 比对 git status -- gen 与已提交 HEAD，待 W69-INT 原子提交后自然归零)

$ npm run schema:typecheck && npm run schema:test:typescript
> tsc && tsx tests/conformance/schema/envelope.ts
  -> EXIT CODE: 0

$ cargo clippy -p nlos-schema -p nlos-plan -p nlos-system-control --all-features --all-targets -- -D warnings
  -> EXIT CODE: 0 (0 warnings)

$ cargo test -p nlos-schema -p nlos-plan -p nlos-system-control --no-fail-fast -j 2
  -> EXIT CODE: 0 (nlos-schema: 40 passed; nlos-plan: 53 passed; nlos-system-control default: 109 passed; 1 doctest passed; 0 failed)

$ cargo test -p nlos-system-control --all-features --no-fail-fast -j 2
  -> EXIT CODE: 0 (155 passed across 16 test binaries + 1 doctest; 0 failed)

$ cargo clippy --manifest-path desktop/src-tauri/Cargo.toml --features dev-fixture --all-targets -- -D warnings
  -> EXIT CODE: 0 (0 warnings)

$ cargo fmt --manifest-path desktop/src-tauri/Cargo.toml --all -- --check
  -> EXIT CODE: 0

$ rustfmt --edition 2024 --check crates/nlos-schema/src/lib.rs crates/nlos-schema/tests/compatibility.rs
  -> EXIT CODE: 0

$ cargo test --manifest-path desktop/src-tauri/Cargo.toml --features dev-fixture --no-fail-fast -j 2
  -> EXIT CODE: 0 (llmos_desktop_lib unit: 14 passed; ipc_enum_symmetry_side: 5 passed; surface_presentation_side: 2 passed; window_lifecycle_side: 3 passed; windows_authenticated_pipe_side: 1 passed; total 25 passed, 0 failed)

$ cd desktop && npx tsc --noEmit
  -> EXIT CODE: 0 (0 diagnostics)
```

### 3.2 环境与边界诚实说明

1. **本地 Python 环境诚实记录**：执行 `$env:PYTHONIOENCODING='utf-8'; python tests/conformance/schema/envelope.py` 时，当前 Windows 宿主 Python 解释器未预装第三方 `protobuf` 包（报 `ModuleNotFoundError: No module named 'google'`）；`gen/python/nlos/sabi/v1/system_control_pb2.py` 已由官方 `buf generate` + `tools/normalize-generated-schema.mjs` 确定性生成，且等价的 TypeScript Conformance（`npm run schema:test:typescript`）与 Rust `nlos-schema` 40 项兼容性测试全部通过。
2. **跨平台传输覆盖说明**：`desktop/src-tauri/tests/ipc_enum_symmetry_side.rs` 与 `system-control-cli` 同时编译支持 `#[cfg(windows)]`（Named Pipe）与 `#[cfg(unix)]`（Unix Domain Socket）；本次在本机 Windows x86_64 上完成了真实 Named Pipe 认证通道 + CLI 子进程实测，Unix Domain Socket 分支留待 Linux/macOS CI 常设流水线复核。
3. **阶段范围边界**：本车道提供可插拔 `CellTopologyInspector` / `CellLeaseInspector` / `DesktopEnumerationSource` 协议与内存工作站基线实现；`desktop/src-tauri/src/lib.rs` `invoke_handler` 挂载由 `W69-INT` 合入，真实跨物理机 `CellHost` 与全桌面视图消费分别在 `W71–W74` 合拢。
