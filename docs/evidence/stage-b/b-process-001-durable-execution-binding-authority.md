# B-PROCESS-001：durable execution binding authority

> 状态：`PARTIAL PASS`　　日期：2026-08-09
>
> 对应：`[PROC-HOST-001]`、`[PROC-SPAWN-001]`、`[PROC-RESTORE-001]`、[ADR-0005](../../management/adrs/0005-task-write-set-authority-first.md)

## 已实现事实

1. 新增 `nlos-process` 单节点 SQLite reference authority；open 时读回验证 WAL/FULL/foreign-keys，并对未知 schema version fail closed。
2. IsolationDomain identity 与 generation fencing token 由 authority 从 typed idempotency input 派生，调用者不提交 DomainId/token；create 与 rotate 都可精确 replay，key rebinding、stale generation/token 被拒绝。
3. delegated Process 注册由 authority 分配 ProcessId、AgentInstanceId、初始 generation 与 Process fencing token，持久绑定 Task/Attempt 与当前 IsolationDomain generation/token；历史 generation 行由 DDL trigger 保持不可变。
4. restore 保持同一 ProcessId/AgentInstanceId，同时以 CAS 推进两者 generation、签发新 Process fence，并可重新绑定当前 IsolationDomain generation；旧 Process reference 或旧 Domain fence 不能通过 active readback。
5. `verify_active_process_binding` 提供给后续 TaskWriteSet builder 的 fail-closed 查询：Process、AgentInstance、IsolationDomain 的 identity/generation/token 必须逐位等于 authority 当前事实。

## 验证

`cargo test -p nlos-process` 的 5 项 integration tests 全部通过，覆盖：

- authority 分配身份、exact replay 与重启回读；
- idempotency rebinding 和 stale Domain 拒绝；
- Domain rotation replay 及旧 Process binding 围栏；
- restore 后 Process/AgentInstance 双 generation 推进、旧引用失效、新引用跨重启验证；
- Domain generation / Process binding durable rows 的 DDL immutability。

同时通过 `cargo check --workspace --all-targets`、`cargo test --workspace`、`cargo clippy --workspace --all-targets -- -D warnings` 与 `cargo fmt --all -- --check`。

## 边界

本证据只覆盖 delegated execution identity/binding 的单节点 H3 reference authority，不是完整 `BirthDecision`：没有 Resource/Ledger、Namespace/Capability、Task contract/SnapshotReceipt 的多 authority prepare/activate 协调，也没有真实 host spawn、suspend/kill、checkpoint、IsolationUnit、resource controller 或三平台 adapter。Task/Attempt association 由 Process authority 持久记录，但尚未在注册时跨 authority 验证；后续 TaskWriteSet builder 必须同时查询 TaskAuthority。当前 fencing token 是 deterministic generation marker，不是认证 secret、签名或 attestation。尚未执行 VFS/ENOSPC/kill-9 fault matrix，不得外推为真实掉电或生产 HA。
