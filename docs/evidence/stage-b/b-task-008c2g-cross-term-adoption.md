# B-TASK-008C2G-CROSS-TERM-ADOPTION：旧 quarantined permit 的 successor-term adoption

状态：`PARTIAL_PASS`（2026-08-19）

## 1. 结论

schema v38 新增独立的 immutable `task_cross_term_adoption_receipts` 证据面，
把旧 term 的 `QUARANTINED` permit 继续执行限定为一条可重算的本地证明链：

```text
old quarantined permit
  → old fenced assignment + original registry binding
  → completed takeover receipt + exact fence root
  → reopened successor registry/assignment
  → live successor lease
  → cross-term adoption receipt
  → reconcile / close only
```

`adopt_permit_across_takeover` 在一个 `BEGIN IMMEDIATE` 事务内重新读取并校验
整条链，随后推进 adoption epoch 与 Task `control_epoch`。receipt 同时保留
original/current lease、original/current registry、takeover receipt、current
assignment、current cancel/control epoch 和 exact fenced participant root；表级
触发器禁止 UPDATE/DELETE。相同 idempotency key 逐字节回放，不同 proof 或不同
permit 绑定 fail closed。

这不是新的授权边界：cross-term adoption 只能用于 `reconcile`、`close` 或再次
`quarantine`，不会签发新的 `EffectPermit`、消费 dispatch token、注册 participant
或修改 proposal。终结 receipt 使用 successor registry binding，避免把旧 registry
冒充为当前事实。

## 2. 已实现事实

- **schema v38**：`migrate_v38` 幂等创建独立 adoption 表；外键分别指向 Task、
  takeover receipt 和 successor assignment；唯一键为 `(task_id, idempotency_key)`；
  immutable/no-delete trigger 与 permit 索引均已建立。旧 v37/same-term adoption
  表保持原形，读取层合并两张证据表但不把旧行改写成 cross-term 行。
- **完整 proof chain**：要求 takeover receipt 属于同一 task/generation 且为
  `Complete`；old assignment 必须是 `Fenced` 且逐位绑定 permit 的旧 lease、旧
  registry 与 control epoch；takeover exact fence root 必须存在；current registry
  必须正好是 `old generation + 1`、`prior_root == old root` 且已重开；current
  assignment 必须是 successor lease + current registry 的 `Active` assignment；
  successor lease 仍须通过 live lease 校验。
- **terminal 接线**：旧 permit adoption 后，effect reconcile 和 v3 finalize/close
  使用 current successor registry binding；同一 receipt 的 replay 仍可读，不要求
  重新提交 lease。cross-term 分支不调用旧 term 的 `reject_takeover_fence` 放行逻辑，
  而是再次校验 takeover、assignment、registry 和 exact root。
- **迁移/回读**：`SCHEMA_VERSION` 已推进到 38；same-term adoption 行仍按旧表
  解码为 `None` cross-proof 字段，cross-term 行重启后逐位回读。

## 3. Evidence

- `cargo test -p nlos-task --test cross_term_adoption -- --nocapture`：1 项通过。
  测试覆盖真实 Semantic participant、sealed write set、EffectUnknown→Quarantined、
  term-1/term-2 lease、signed barrier observations、takeover completion、successor
  registry reopen、cross-term adoption、同 key replay、successor lease 下 reconcile、
  用 successor registry finalize，以及 SQLite restart readback。
- `cargo clippy -p nlos-task --all-targets -- -D warnings`：通过。
- `cargo test -p nlos-task --quiet`：180 项全过（包含旧 schema/lease/takeover 回归）。
- `cargo test --workspace --quiet`：444 项通过、2 项既有 100K scale probe 保持
  ignored。
- `cargo fmt --all -- --check`：通过。

## 4. 明确限制

- 当前证明是同一 TaskAuthority 内的本地 takeover/adoption 链；没有把远端 barrier
  的签名者提升为 participant 授权者，也没有重新向 Artifact、Semantic、Process、
  Resource 或 Operation owner 请求跨 term endpoint proof。
- `commit.rs` 的 Artifact-aware finalize、`semantic_commit.rs` 的 Semantic-only
  high-level finalize 和跨 authority coordinator 仍保留各自既有边界；本切片只把
  核心 Effect reconcile/close/v3 terminal path 接入 successor registry binding。
- TakeoverControl 的 TypeScript/Python conformance、Windows named pipe handler
  round-trip、真实 IPC 崩溃/并发矩阵、时间窗防重放和 principal-level peer
  attestation 仍未完成。因此工作包仍是 `PARTIAL_PASS`，不能外推为完整分布式接管。
