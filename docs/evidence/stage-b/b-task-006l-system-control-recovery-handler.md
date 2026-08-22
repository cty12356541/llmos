# B-TASK-006L：SystemControl recovery handler

> 状态：`PARTIAL PASS`　　日期：2026-08-23（第二十一增量）
>
> 对应：`B-TASK-006K`、`B-SCHEMA-014`、`[SABI-AUTH-001]`、`[CTRL-PARITY-001]`、`[CTRL-RECOVERY-001]`

## 已实现事实

1. 新增无 canonical state 的 `nlos-system-control` handler；它只读取 worker health/TaskAuthority，并把 mutation 委托给 TaskAuthority schema v9 acknowledgement transaction。
2. `get` 每次从 TaskAuthority 重新读取 durable retrying/escalated/unacknowledged/resolved gauge；worker 本地 failure message 被丢弃，只映射 bounded plan ID 与 typed authority。
3. `submit` 先验证 common SABI mutation context，再要求 authenticated caller Principal 等于 command issuer、ControlCommandId 等于 envelope IdempotencyKey，之后才进入 pluggable Capability authorizer。
4. acknowledgement 使用 command target + expected revision 驱动 TaskAuthority failure-count CAS；响应 payload 与 common response context 引用同一个 immutable Receipt，exact replay 不重复确认，且不会隐式 resume。
5. ServiceDirectory 可按 `nlos.sabi.SystemControl` v1 协商 binding；本地 Unix socket 测试从协商 endpoint 建连并完成 typed `get`，另一个 framed LocalRpc 测试完成 `submit` 与 Receipt replay。

6. `SystemControlError::to_sabi_failure` 将本地拒绝映射为有界 common SABI `SabiFailure`：契约/期限/权限/冲突/状态/NotFound/Durability/Driver/Fenced/NotSupported 均有固定 code、retry directive 和短安全消息；不传播 SQLite、authority reason 或 corrupt-record 诊断。
7. `failure_envelope` 保留 request/correlation identity，清空 payload、Operation 与 Receipt evidence；malformed correlation 只回退到合法 request ID 或有界全零 correlation。映射、SQLite 同 key 重试安全和 envelope 脱敏测试已加入 `nlos-system-control`。
8. 新增 Windows-only named-pipe round-trip：真实 `NamedPipeListenerAdapter`/`connect`、`ExactPeerAuthorizer` OS credential pre-gate、typed `submit`、durable Receipt 和第二连接 exact replay；测试 authorizer 仍是固定 stub，不代表真实 Capability authority。

## 2026-08-23 增量验证

- `cargo fmt --all -- --check`：通过。
- `cargo test -p nlos-system-control --quiet`：通过（6 个既有 integration + 5 个 bounded-failure tests；Windows-only test 在 macOS 目标下为 0 tests，待 Windows CI 执行）。
- `cargo clippy -p nlos-system-control --all-targets --all-features -- -D warnings`：通过。
- `cargo test --workspace --quiet`：通过。
- 本地 Windows target 未安装，未把本地非 Windows 构建伪装成 Windows 证据；远端 Windows job 是该测试的编译/执行依据。

## 验证与边界

既有 5 项 integration tests 覆盖脱敏 health、Capability 拒绝、caller/issuer 与 command/key 防替换、真实 framed submit/replay、ServiceDirectory negotiate 和 macOS/Unix endpoint round-trip；新增 5 项 bounded-failure tests 覆盖映射分类、同 key durability 重试安全、相关性保留与脱敏 envelope；Windows-only named-pipe round-trip 在非 Windows 目标下完成编译门，等待远端 Windows 执行。crate tests、workspace、Clippy `-D warnings` 与 fmt 通过。

本证据为单节点本地 H3 / `PARTIAL PASS`。`handle` 仍是 transport-neutral `Result`，IPC caller 必须显式调用 `failure_envelope` 才能返回 typed rejection；当前 Windows 测试验证成功/重放路径而非拒绝响应。测试 policy/peer authorizer 仍是受控 stub，尚未接真实 Capability authority、Principal-level peer attestation 或双向 peer policy；trusted-clock anti-replay、外部 metrics exporter、GUI/NL/CLI 多入口等价证明、批量控制和三平台 CI 仍未完成。
