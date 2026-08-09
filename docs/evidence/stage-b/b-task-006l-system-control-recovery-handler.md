# B-TASK-006L：SystemControl recovery handler

> 状态：`PARTIAL PASS`　　日期：2026-08-09
>
> 对应：`B-TASK-006K`、`B-SCHEMA-014`、`[SABI-AUTH-001]`、`[CTRL-PARITY-001]`、`[CTRL-RECOVERY-001]`

## 已实现事实

1. 新增无 canonical state 的 `nlos-system-control` handler；它只读取 worker health/TaskAuthority，并把 mutation 委托给 TaskAuthority schema v9 acknowledgement transaction。
2. `get` 每次从 TaskAuthority 重新读取 durable retrying/escalated/unacknowledged/resolved gauge；worker 本地 failure message 被丢弃，只映射 bounded plan ID 与 typed authority。
3. `submit` 先验证 common SABI mutation context，再要求 authenticated caller Principal 等于 command issuer、ControlCommandId 等于 envelope IdempotencyKey，之后才进入 pluggable Capability authorizer。
4. acknowledgement 使用 command target + expected revision 驱动 TaskAuthority failure-count CAS；响应 payload 与 common response context 引用同一个 immutable Receipt，exact replay 不重复确认，且不会隐式 resume。
5. ServiceDirectory 可按 `nlos.sabi.SystemControl` v1 协商 binding；本地 Unix socket 测试从协商 endpoint 建连并完成 typed `get`，另一个 framed LocalRpc 测试完成 `submit` 与 Receipt replay。

## 验证与边界

5 项 integration tests 覆盖脱敏 health、Capability 拒绝、caller/issuer 与 command/key 防替换、真实 framed submit/replay、ServiceDirectory negotiate 和 macOS/Unix endpoint round-trip。crate tests 与 Clippy `-D warnings` 通过。

本证据为单节点本地 H3 / `PARTIAL PASS`。测试 policy/peer authorizer 仍是受控 stub，尚未接真实 Capability authority 与双向 peer policy；拒绝路径到 bounded SABI failure 的映射、Windows named-pipe handler round-trip、外部 metrics exporter、GUI/NL/CLI 多入口等价证明、批量控制和三平台 CI 仍未完成。
