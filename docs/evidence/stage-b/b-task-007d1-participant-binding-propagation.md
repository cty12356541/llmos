# B-TASK-007D1：EffectPermit / Task Receipt participant binding

## 1. 验收对象

本切片消费 `CommitPermit.participant_registry_binding`，把冻结 registry 的 `generation/root` 逐位传播到 `EffectPermit` 与 permit-backed `TaskCommitReceipt` / `TaskPermitClosureReceipt`，并在 effect issuance、dispatch 和 terminal decision 前由 TaskAuthority 在线回读重验。

## 2. 实现事实

- Task schema 升至 v12；`effect_permits` 与 `task_receipts` 新增 nullable participant registry generation/root。新 `EffectPermit` 与所有 permit-backed Task Receipt 必须携带完整 binding，pre-permit cancel closure 保持 `None`。
- v11→v12 迁移只增加列与 EffectPermit binding immutability trigger，不为历史记录伪造 proof。pre-v12 permit/effect/receipt 显式解码为 `None`；历史读回/replay 保留，但缺少 CommitPermit binding 的新 effect/terminal mutation typed fail closed。
- EffectPermit issuance 在同一 Task transaction 中重验当前 registry 仍是 CommitPermit 绑定的 generation/root 且为 `FROZEN_FOR_PERMIT`，随后逐位复制 binding；dispatch 同时重验 current registry 和 EffectPermit→CommitPermit copy。
- legacy/v3 finalize、permit closure、Artifact-only finalize 均在 terminal write 前重验 frozen registry；新 Task Receipt 逐位复制 CommitPermit binding。receipt/effect replay 会拒绝 parent/copy 漂移。
- participant lifecycle test 将 registry state 人为漂移为 `OPEN`，证明 dispatch 与 finalize 均在写入前拒绝；恢复正确 frozen state 后可继续，EffectPermit 与 Task Receipt 跨重启保持相同 binding。

## 3. 验证

```text
cargo test -p nlos-task
cargo clippy -p nlos-task --all-targets -- -D warnings
cargo fmt --all -- --check
git diff --check
cargo check --workspace
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

以上命令均以退出码 0 通过。participant registry integration tests 从 6 项增至 7 项；全套 `nlos-task` migration、effect、reconcile、artifact commit、group、fault-injection 测试及整仓验证保持通过。

## 4. 证据等级与限制

结论：`PARTIAL PASS / H3 local participant binding propagation and revalidation`。

- v12 对历史 unbound active permit 采取 fail-closed；真实升级环境仍需独立的受控 adoption/administrative closure 策略，不能把缺失 binding 静默回填为已验证事实。
- 完整 `TaskWriteSet` 尚未实现；planned EffectSlot 还没有逐项携带并验证 Artifact/Semantic、Driver/Reservation、Process/Domain 等 endpoint binding。
- EffectPermit 仍缺 v0.5 要求的 Operation/Driver/Reservation、Process/Domain incarnation、TaskAuthority term/lease 等维度；本切片只收口 participant registry generation/root。
- takeover registry、exact fenced participant root、coverage proof 与跨 term PermitAdoption 仍未实现；当前 revalidation 只覆盖单节点同 authority term 的 frozen registry。
- TaskCommitReceipt 尚未枚举完整 snapshot/read/write/publication/effect/cost/conflict evidence；本切片不能外推为 `[TASK-COMMIT-002]` 全量通过。

下一验收门：`B-TASK-008A` 持久化 authority-verified complete TaskWriteSet，构造 canonical root，并与 snapshot receipt、group binding、effect set、participant registry 和 CommitPermit 逐位绑定。
