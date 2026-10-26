# C-UNFREEZE-001 / W66：解除 `nlos-search` 与 `nlos-notify` F1 冻结声明并接入 `nlos-system-control` 与 `nlos-slice-k` 生产消费面

- 状态：`PASS`（解除 `nlos-search` 与 `nlos-notify` 两侧 `2026-10-05 F1` 冻结保留声明；在零 SABI `.proto` 破坏性改动下将 `SearchServiceInspector` 与 `NotificationServiceInspector` 接入 `SystemControlDaemon`、`system-control-cli`、`nl` 自然语言编译、ADR-0011 认证 IPC 与 `desktop` 全分支 DTO/视图；同步完成 `nlos-slice-k` `SliceKRuntime` / `CellHost` / `slice-k-demo` 消费面装配）
- Assurance：`H4`
- 日期：2026-10-26
- Owner：`C-MULTICELL` 轨道 `#15`（`W66` 解冻与生产消费面闭环）
- 设计依据：[06-架构设计总纲-v0.5.md](../../design/06-架构设计总纲-v0.5.md) §24.1 `[CONS-DECL-001]`、`[CONS-CTRL-001]`；§24 `[DUI-NOTIFY-001]`、`[DUI-OFFLINE-001]`；§25 `[TASK-STATE-001]`、`[TASK-NOTIFY-001]`、`[CTRL-PARITY-001]`；§18 `[SEM-CHECKPOINT-001]`；§27 `[PERF-CONTROL-001]`
- 前置证据：[c-search-001-non-authoritative-checkpoint-search.md](./c-search-001-non-authoritative-checkpoint-search.md)、[c-notify-001-task-lifecycle-action-token.md](./c-notify-001-task-lifecycle-action-token.md)
- 基线：`8f16d57bc5bd09b41eee6227c9f58b5eafaf446f`

## 1. 实现事实

### 1.1 解冻声明与并发安全/O(1) 控制面支撑

- `crates/nlos-search/src/lib.rs`：
  - 移除顶部 `2026-10-05 F1` 冻结保留声明，替换为 `2026-10-26 W66 / C-UNFREEZE-001` 生产装配状态说明，明确登记 `nlos-system-control`（`SearchServiceInspector`、`SystemControlDaemon`、`system-control-cli`、`nl`）与 `nlos-slice-k`（`SliceKRuntime`、`CellHost`、`slice-k-demo`）两大生产消费面；
  - 将 `SearchService` 内部只读 `SQLite` 连接包装为 `Mutex<Connection>`（读操作完成后立即释放锁，不在权威 `inspect_trust_view` 调用期间持锁），使 `SearchService` 满足 `Send + Sync`（含单元测试 `search_service_is_send_and_sync` 编译期静态断言），可直接驻留于多线程 `Arc<SystemControlDaemon>` 中，且 100% 保持既有公共签名与 `SearchError` 变体零破坏性改动；
  - 重导出 `pub use nlos_capability::CapabilityTarget;`，供控制面作用域编解码直接引用。
- `crates/nlos-notify/src/lib.rs`：
  - 移除顶部 `2026-10-05 F1` 冻结保留声明，替换为 `2026-10-26 W66 / C-UNFREEZE-001` 生产装配状态说明，明确登记 `nlos-system-control`（`NotificationServiceInspector`、`SystemControlDaemon`、`system-control-cli`、`nl`）与 `nlos-slice-k`（`SliceKRuntime`、`CellHost`、`slice-k-demo`）两大生产消费面；
  - 新增 `NotificationService::inspect_subscription(&self, notification_id: NotificationId) -> Result<NotificationSubscriptionView, NotifyError>`（`[PERF-CONTROL-001]`）：按主键 O(1) 查询 `notify_subscriptions` 薄引用并与 `TopicAuthority::inspect_subscription` 核对实时状态，避免控制面单条探查退化为全表 `list_subscriptions` 扫描。
- 仓库静态审计：`git grep -E "冻结声明|冻结状态|冻结保留" -- crates/nlos-search crates/nlos-notify` 匹配数为 `0`（退出码 `1`）。

### 1.2 `nlos-system-control` 生产消费面装配（零 SABI `.proto` 破坏性改动）

遵循既有 `ApplicationAuthorityInspector` 客户端/守护进程侧收据合成模式（GET 信封复用权威 `SystemControl` 授权门禁，有界领域事实通过类型化 Inspector 注入 `ControlReceipt`）：

- `crates/nlos-system-control/src/control.rs` 与 `src/auth.rs`：
  - 新增 `SCOPE_ID_BYTES = 32`、`ControlCommand::InspectSearch { scope_id: [u8; 32] }`（确定性折叠域掩码 `0xC6` 保证全零 `scope_id` 仍派生非零 `control_command_id` 与 `correlation_id`）及 `ControlCommand::InspectNotification { notification_id: [u8; 16] }`；
  - 新增有界投影类型 `SearchInspection`（`scope_id`、`consistency_class`、`degradation_policy`、`non_authoritative_restrictions_bits`、`admitted_event_count`、`active_assertion_count`、`retracted_event_count`、`truncated`）与 `NotificationInspection`（`notification_id`、`topic_id`、`subscriber_key`、`next_sequence`、`history_entry_count`、`history_truncated`、`prune_watermark_sequence`）；
  - 新增 `SearchInspector` / `UnwiredSearchInspector` 与 `NotificationInspector` / `UnwiredNotificationInspector`（未接线时严格返回 `SabiErrorCode::NotFound` + `RetryDirective::DoNotRetry`，绝不伪造空成功结果）；
  - `ControlOutcome` 新增 `SearchInspected(SearchInspection)`（`ControlReceipt::to_bytes` 标签 `23`）与 `NotificationInspected(NotificationInspection)`（标签 `24`），并新增 `parse_hex_32`（64 位十六进制严格解析）；
  - 新增 `ControlReceipt::compose_with_services`、`dispatch_in_process_with_services`、`dispatch_over_socket_with_services`（`#[cfg(all(unix, feature = "cli"))]`）与 `dispatch_over_authenticated_socket_with_services`；同时 100% 保持既有 `ControlReceipt::compose`、`dispatch_in_process`、`dispatch_over_socket` 与 `dispatch_over_authenticated_socket` 的原参数签名不变（内部委托 `None, None`）。
- `crates/nlos-system-control/src/search_inspector.rs`（`#[cfg(feature = "search")]`）：
  - 实现 `SearchServiceInspector<'a>`（绑定 `&SearchService` 与 `&SemanticAuthority`），提供 32 字节作用域编码契约：全零 `UNSCOPED_SEARCH_SCOPE_ID` 表示无作用域扫描、`[0x01, 0..0, namespace_id]` 映射 `CapabilityTarget::Namespace`、`[0x02, 0..0, task_id]` 映射 `CapabilityTarget::Task`，其余未知标签一律以 `SabiErrorCode::InvalidArgument` fail-closed 拒绝；
  - 以有界窗口 `INSPECT_SEARCH_PAGE_LIMIT = 64` 调用 `SearchService::search_assertions_paginated`，强制校验 `page.enforce_not_control_authoritative(ControlConsumerKind::CandidateDiscovery)`（`[CONS-CTRL-001]`，`restrictions_bits == 0x1F`）与 `SearchRequestedConsistency::EventualOrDerived`（`[CONS-DECL-001]`），并逐条回读 `SemanticAuthority::inspect_admission_receipt` 核验权威接纳收据。
- `crates/nlos-system-control/src/notify_inspector.rs`（`#[cfg(feature = "notify")]`）：
  - 实现 `NotificationServiceInspector<'a>`（绑定 `&NotificationService` 与 `&TopicAuthority`）：通过 `inspect_subscription`（缺失或悬空引用均返回 `NOT_FOUND`）、`TopicAuthority::inspect_topic` 与 `query_notification_history_page(topic_id, 0, 64)` 汇总订阅游标、离线历史计数、截断位与单调裁剪水位线（`[DUI-OFFLINE-001]`、`[PERF-CONTROL-001]`）。
- `crates/nlos-system-control/src/nl.rs`：
  - 新增英/中受控自然语言文法（`[CTRL-PARITY-001]`）：`inspect [the] search [scope] <64-hex>` / `查看|检查 搜索 <64-hex>` 编译为 `ControlCommand::InspectSearch`；`inspect [the] notification [subscription] <32-hex>` / `查看|检查 通知 <32-hex>` 编译为 `ControlCommand::InspectNotification`（同时支持 `inspect-search` / `inspect-notification` CLI 形式直通）。
- `crates/nlos-system-control/src/bin/system-control-cli.rs`：
  - 新增 `inspect-search <SCOPE_ID_HEX_64>` 与 `inspect-notification <NOTIFICATION_ID_HEX_32>` 子命令、确定性单行摘要及 `--root <STATE_ROOT>` 客户端侧只读装配（未传 `--root` 或未启用 `search`/`notify` feature 时在线路前诚实拒绝并返回退出码 `2`）。
- `crates/nlos-system-control/src/daemon.rs`：
  - `assemble` 在同一状态根下打开 `<root>/waits`（`WaitAuthority`）、`<root>/notify`（`NotificationService`）与 `<root>/semantic`（只读 `SearchService`），暴露 `wait_authority(&self)`、`notification_service(&self)` 与 `search_service(&self)` 访问器，并在 `SystemControlDaemon::dispatch_command` 中注入 `SearchServiceInspector` 与 `NotificationServiceInspector`。
- `desktop` 全分支穷尽类型同步（零通配符 `_ =>` 分支）：
  - 同步更新 `desktop/src-tauri/src/dto.rs`（`OutcomeDto::SearchInspected` / `OutcomeDto::NotificationInspected`）、`desktop/src/types.ts`、`desktop/src/main.ts` 与 `desktop/src/views/task-space/receipts.ts`。

### 1.3 工作区反向依赖图（`cargo tree --workspace -i --all-features`）

```text
$ cargo tree --workspace -i nlos-search --all-features
nlos-search v0.1.0 (D:\llmos\crates\nlos-search)
├── nlos-slice-k v0.1.0 (D:\llmos\crates\nlos-slice-k)
│   └── nlos-package v0.1.0 (D:\llmos\crates\nlos-package)
└── nlos-system-control v0.1.0 (D:\llmos\crates\nlos-system-control)
    └── nlos-slice-k v0.1.0 (D:\llmos\crates\nlos-slice-k) (*)

$ cargo tree --workspace -i nlos-notify --all-features
nlos-notify v0.1.0 (D:\llmos\crates\nlos-notify)
├── nlos-slice-k v0.1.0 (D:\llmos\crates\nlos-slice-k)
│   └── nlos-package v0.1.0 (D:\llmos\crates\nlos-package)
└── nlos-system-control v0.1.0 (D:\llmos\crates\nlos-system-control)
    └── nlos-slice-k v0.1.0 (D:\llmos\crates\nlos-slice-k) (*)
```

## 2. 验证（2026-10-26，Windows x86_64）

```text
git grep -E "冻结声明|冻结状态|冻结保留" -- crates/nlos-search crates/nlos-notify
  → 退出码 1（0 处残留冻结声明）
cargo fmt -p nlos-search -p nlos-notify -p nlos-system-control -- --check
  → 退出码 0（本车道全部工作区 crate 零格式差异；注：并行波次执行时刻 `cargo fmt --all -- --check` 仅因兄弟车道正在编辑的 `crates/nlos-slice-k/**` 未格式化返回 1，按写集隔离规则未触碰兄弟车道文件，合流屏障由父编排器复验全仓 `cargo fmt --all -- --check`）
cargo fmt --manifest-path desktop/src-tauri/Cargo.toml -- --check
  → 退出码 0
cargo clippy -p nlos-search -p nlos-notify --all-targets -- -D warnings
  → 退出码 0，零警告
cargo clippy -p nlos-system-control --no-default-features --all-targets -- -D warnings
  → 退出码 0，零警告
cargo clippy -p nlos-system-control --all-targets -- -D warnings
  → 退出码 0，零警告
cargo clippy -p nlos-system-control --all-features --all-targets -- -D warnings
  → 退出码 0，零警告
cargo test -p nlos-search -p nlos-notify
  → 退出码 0，52 passed / 0 failed（nlos-notify: lib 2 + c_notify_task_action_token 13 + notify_service 12 = 27；nlos-search: lib 3 + c_search_non_authoritative_checkpoint 14 + search_service 8 = 25）
cargo test -p nlos-system-control --all-features
  → 退出码 0，154 passed / 0 failed（含新增集成测试套件 search_notify_wiring 4 passed + lib 70 passed + doctest 1 passed）
npx tsc --noEmit -p desktop/tsconfig.json
  → 退出码 0，零类型错误
cargo clippy --manifest-path desktop/src-tauri/Cargo.toml --features dev-fixture --all-targets -- -D warnings
  → 退出码 0，零警告
cargo test --manifest-path desktop/src-tauri/Cargo.toml --features dev-fixture
  → 退出码 0，19 passed / 0 failed
```

新增端到端证据测试 `crates/nlos-system-control/tests/search_notify_wiring.rs`（4 项集成测试）断言面：

1. `daemon_inspect_search_end_to_end_over_admitted_and_retracted_assertions`：组装真实 `SystemControlDaemon`，在 `SemanticAuthority` 中接纳跨 `Namespace` / `Task` 作用域的多条断言并提交撤回事件，通过 `daemon.dispatch_command(&ControlCommand::InspectSearch { .. })` 验证命名空间作用域、任务作用域、全零无作用域扫描的 `admitted_event_count`、`active_assertion_count`、`retracted_event_count`、`consistency_class == 1`、`non_authoritative_restrictions_bits == 0x1F`，以及非法 `scope_id` 标签以 `INVALID_ARGUMENT` fail-closed 拒绝；
2. `daemon_inspect_notification_end_to_end_over_task_transitions_and_offline_history`：在真实 `SystemControlDaemon` 上创建通道/主题、注册 `NotificationService` 订阅与 `WaitAuthority` 投递等待（`target_sequence = 3`），发布 3 条 `[TASK-NOTIFY-001]` 任务迁移通知并核验 `WaitState::Woken` 与 `[DUI-OFFLINE-001]` 历史计数，随后执行 `ack(up_to_sequence = 2)` 与 `prune_history_before(topic_id, 2)`，验证 `InspectNotification` 精确反映 `next_sequence: 1 -> 3`、`history_entry_count: 3 -> 2`、`prune_watermark_sequence: 0 -> 2`，且未知 `notification_id` 以 `NOT_FOUND` fail-closed 拒绝；
3. `nl_command_structured_and_authenticated_ipc_receipt_parity`（`[CTRL-PARITY-001]`）：证明英文/中文 `parse_nl_command`（`inspect search` / `查看搜索`、`inspect notification` / `查看通知`）、结构化 `ControlCommand`、进程内 `daemon.dispatch_command` 与真实 ADR-0011 认证 IPC 端点（Windows 命名管道 / Unix 域套接字 `serve_authenticated_endpoint` + `dispatch_over_authenticated_socket_with_services`）生成的 `ControlReceipt::to_bytes()`（标签 `23` / `24`）与 `receipt_to_hex` 逐字节完全一致；
4. `unwired_inspectors_fail_closed_with_not_found_and_preserve_legacy_signatures`：验证既有 `ControlReceipt::compose`、`dispatch_in_process` 与 `dispatch_in_process_with_services(..., None, None)` 在未注入 `SearchInspector` / `NotificationInspector` 时严格以 `SabiErrorCode::NotFound` + `RetryDirective::DoNotRetry` fail-closed 拒绝且收据字节完全一致。

## 3. 边界与限制（诚实域，`EVID-SCOPE-001`）

- **零 SABI `.proto` 改动与客户端/守护进程侧合成边界**：本次控制面探查复用既有 `GetSystemControlRequest` 授权通道与 `ApplicationAuthorityInspector` 同构合成契约（进程内守护进程与 CLI `--root` / 认证套接字 `*_with_services` 直接读取同一状态根下的只读 `SearchService` 与持久 `NotificationService`），未在跨主机远程 SABI protobuf 载荷中新增独立搜索/通知消息体。
- **`system-control-cli` 平台边界保持诚实**：与既有 `inspect-process` / `inspect-application` / `inspect-resource` 一致，`system-control-cli` 的明文 Unix 域套接字派发仍仅在 `#[cfg(unix)]` 下启用（Windows 下返回既有诚实提示与退出码 `2`）；Windows 与 Unix 跨平台生产路径由 `SystemControlDaemon`、进程内派发、ADR-0011 认证命名管道/套接字（`dispatch_over_authenticated_socket_with_services`）与 `desktop` Tauri DTO/视图承载并已在本机 Windows x86_64 完成真实命名管道认证往返验证。
- **查询面只读非权威边界不变**：`InspectSearch` 仅暴露有界一致性/限制位与计数观测摘要，绝不写回 `SemanticAuthority`，且 `non_authoritative_restrictions_bits == 0x1F` 持续禁止任何搜索结果直接充当五大控制门禁权威输入（`[CONS-CTRL-001]`）。

## 4. 复现

```text
cargo fmt --all -- --check
cargo clippy -p nlos-search -p nlos-notify --all-targets -- -D warnings
cargo clippy -p nlos-system-control --no-default-features --all-targets -- -D warnings
cargo clippy -p nlos-system-control --all-targets -- -D warnings
cargo clippy -p nlos-system-control --all-features --all-targets -- -D warnings
cargo test -p nlos-search -p nlos-notify
cargo test -p nlos-system-control --all-features
npx tsc --noEmit -p desktop/tsconfig.json
cargo clippy --manifest-path desktop/src-tauri/Cargo.toml --features dev-fixture --all-targets -- -D warnings
cargo test --manifest-path desktop/src-tauri/Cargo.toml --features dev-fixture
```
