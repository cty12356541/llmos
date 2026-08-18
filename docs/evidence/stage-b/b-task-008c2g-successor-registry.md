# B-TASK-008C2G-SUCCESSOR-REGISTRY：successor term registry 重开与新 permit 接线

状态：`PARTIAL_PASS`（2026-08-18）

## 1. 结论

本切片兑现 takeover 完成后的本地 successor-term hand-off：只有已完成的
`TaskAuthorityTakeoverReceipt`、逐位匹配的 successor lease 和 `Active` successor
assignment 才能调用 `reopen_successor_registry`。该 API 在一个
`BEGIN IMMEDIATE` 事务内把冻结 registry 的 participant tuple 原样链入
`generation + 1` 的新 `Open` registry，使用 `prior_root` 保留历史链；同时把
completion 时的 successor assignment CAS 置为 `Fenced`，插入绑定新 registry
root 的 `Active` assignment，并推进 Task `control_epoch`。因此新的
lease-bound `CommitPermit` 可以在新 registry generation 上签发；permit 冻结后
registry 正常进入 `FrozenForPermit`。

重放不创建第二代 registry 或 assignment：以新 registry 的 `prior_root`、旧/新
assignment 状态和 lease binding 组成 durable identity projection，跨进程重开和
后续 permit 变化仍返回逐字节相同的结果。completion replay 也能识别“原
successor assignment 已因 registry rotation 被 fence”的合法后继形态。

## 2. 已实现事实

- **Participant registry**：新增 `reopen_after_takeover`，仅接受
  `FrozenForTakeover` 且 generation/root 与完成 receipt 相等的 registry；旧行
  `Superseded`，新行 `Open`，participant 列和 `prior_root` 由 authority 复制，
  registry creation receipt 保持既有 immutable 证据链。
- **Assignment rotation**：新增 `AuthoritySuccessorRegistryReopenRequest` /
  `AuthoritySuccessorRegistryReopenRecord`；旧 completion assignment 必须是
  Active 且 lease/registry 逐位匹配，随后 CAS 为 Fenced，再插入新 registry-bound
  Active assignment。新 assignment ID 仍由现有
  `derive_assignment_id(task, generation, authority, term, registry_binding)`
  派生，避免同一 term 下出现两个 Active assignment。
- **Permit path**：既有
  `request_commit_permit_with_authority_lease` 在新 registry 上继续工作；
  `ensure_active_assignment` 命中新 assignment 并只刷新其 live lease/control
  binding，不再把冻结的旧 registry 当作可签发基线。
- **Replay / restart**：重开 replay 在 `Open` 或后续 `FrozenForPermit` 状态都只
  读 durable registry/assignment projection；exact lease bytes 仍必须匹配 receipt，
  但已完成的重开不因 wall-clock lease expiry 被误报为新写入。原 completion receipt
  的 replay 对旧 successor assignment 被 rotation 后的合法 `Fenced` 状态保持可读。

## 3. Evidence

- `cargo test -p nlos-task --test takeover_completion --quiet`：12 项通过（原
  completion 9 项 + successor registry 3 项）。覆盖新 generation/root、旧/新
  assignment、lease-bound 新 permit、permit 后 `FrozenForPermit`、same-process
  replay、SQLite reopen replay 和 completion replay。
- `cargo test -p nlos-task --quiet`：179 项通过。
- `cargo test --workspace --quiet`：443 项通过、0 失败（另有 2 项既有
  ignored scale probes）。
- `cargo fmt --all` 清洁；`cargo clippy -p nlos-task --all-targets -- -D warnings`
  与 workspace 构建在提交前复验。

## 4. 明确限制

- 新 registry 只复制 frozen participant tuple；它没有重新向 Artifact、Semantic、
  Process、Resource 或 Operation owner 请求跨 term endpoint proof，也没有定义
  principal→participant 的授权绑定。因此这不是远端 barrier attestation 或物理
  cleanup proof。
- 旧 term 的 quarantined `CommitPermit` 尚不能在 successor term 通过
  cross-term adoption 继续 reconcile/close；`reject_takeover_fence` 的跨 term
  放宽和 adoption receipt 设计仍是下一验收门。
- 本切片没有新增 v38 schema，也未运行 successor-rotation 专属 VFS kill-9/
  ENOSPC 矩阵；现有 takeover/lease/barrier 故障矩阵只覆盖其前置表组。
- TakeoverControl 仍缺 TypeScript/Python conformance、Windows named pipe
  handler round-trip、真实 Capability authorizer 和 principal-level peer
  attestation。
