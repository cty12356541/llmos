# B-OP-FENCE-003：Operation dispatch activation proof/readback

- 状态：`PARTIAL_PASS`
- 日期：2026-08-24（Attempt `OP-ACTIVATION-EVIDENCE-01` 增量验收）
- Owner：`SqliteOperationStore`
- 代码基线：base HEAD `6b7285e`（`6b7285ed3a52d2eb2703ac11407dcc4a242ff228`）之上的未提交工作区候选（`crates/nlos-store/src/lib.rs` +80 行、`crates/nlos-store/tests/operation_activation_proof.rs` 新增），本 Attempt 只验证并记录，不改代码、不提交
- 关联 Requirement：`MODEL-OP-001`、`TASK-EFFECT-001`、`TCB-ENDPOINT-001`
- 关联 ADR：[ADR-0002](../../management/adrs/0002-stage-b-sqlite-operation-authority.md)
- 前置切片：[B-OP-FENCE-002](b-op-fence-002-operation-endpoint-proof.md)（owner-derived endpoint proof + durable `prepare → activate` boundary）

## 证明范围

本切片为 B-OP-FENCE-002 落地的 durable `prepare_dispatch → activate_dispatch`
boundary 补上 owner 侧 activation readback：新增
`SqliteOperationStore::inspect_activation_proof`，只接受 exact
`OperationId + Generation` 回读，并把 durable preparation、immutable activation
receipt、owner 状态机三方的 facts 交叉验证后，返回 authority-derived
`OperationActivationProof`。proof 携带 `operation`（handle）、
`preparation_receipt_id`、`activation_receipt_id`、`callback_id` 与
`cancel_epoch`；它是独立于 `OperationEndpointProof` 的第二把钥匙——
registration/participant admission 不蕴含 one-shot dispatch boundary 已打开，
只有 activation receipt 存在且与 preparation/状态机逐项一致时才发放。

新增 `StoreError::OperationNotActivated` 变体（"operation has no durable
dispatch activation"）：registered/未 prepare 的 Operation 在 proof 生成前以
`DispatchPreparationNotFound` fail closed，prepared 但未 activate 的 Operation
以 `OperationNotActivated` fail closed。

`inspect_activation_proof` 的逐项检查（顺序即实现顺序）：

1. **Generation fence**：按 `OperationId` 读取 owner 状态机，snapshot handle
   与请求 handle 不一致（旧/超前世代）→ `StoreError::Operation(OperationError::InvalidGeneration)`，
   在任何 proof 生成前 fail closed。
2. **Preparation 存在性**：durable `operation_dispatch_preparations` 行必须存在，
   否则 `StoreError::DispatchPreparationNotFound`。
3. **Activation 存在性**：durable `operation_dispatch_activation_receipts` 行
   必须存在，否则 `StoreError::OperationNotActivated`。
4. **Activation/preparation 一致性**：activation 行必须在 operation handle、
   `preparation_receipt_id`、`callback_id`、`cancel_epoch` 上与 preparation 行
   逐项一致，否则 `CorruptRecord("dispatch activation disagrees with preparation")`。
5. **Receipt 身份重推导**：以 domain-separated SHA-256（domain
   `nlos/operation-dispatch/activation/v1`，输入 operation_id、generation
   big-endian、callback_id，截断 16 字节）重算 `activation_receipt_id`，与
   `activate_dispatch` 写入时同一公式；不一致 →
   `CorruptRecord("dispatch activation receipt identity mismatch")`。
6. **状态机交叉复核**：owner 状态机的 `issued_callback` 必须存在，且其
   `callback_id`/`cancel_epoch` 与 activation 行一致、状态不得仍为
   `Registered`，否则 `CorruptRecord("dispatch activation disagrees with Operation state")`。

全部通过后才返回 proof；三处 `CorruptRecord` 分支构成本切片的 corruption
自检面，但未做系统性 corruption/fault 注入矩阵（见“明确未完成”）。

## 验证

`crates/nlos-store/tests/operation_activation_proof.rs` 的 1 项测试
`activation_proof_requires_owner_activation_and_replays_after_restart` 覆盖：

- **激活前 fail closed**：fresh store 注册后直接回读 →
  `DispatchPreparationNotFound`；`prepare_dispatch` 后回读 →
  `OperationNotActivated`，且状态仍为 `Registered`。
- **激活后逐字段回读**：`activate_dispatch` 返回 `Activated`（非 `Replayed`）
  后，proof 的 `operation`、`preparation_receipt_id`（等于 preparation）、
  `activation_receipt_id`（等于 activation）、`callback_id`、`cancel_epoch`
  逐项相等，状态推进到 `Dispatched`。
- **重启 replay**：drop store 关闭连接后重开同一数据库文件，
  `inspect_activation_proof` 回读与重启前完全相等的 proof（`PartialEq` 全等）。
- **Cancel 后 durability**：重启后 `request_cancel` 成功，activation proof
  依旧回读同一值——terminal 化（取消）不抹除已打开 dispatch boundary 的证据。
- **Stale generation**：对 `checked_next` 得到的下一代 generation 回读 →
  `StoreError::Operation(OperationError::InvalidGeneration)`。

本 Attempt 实际执行的验证命令与结果（均在 base HEAD `6b7285e` 的工作区候选上）：

- `cargo test -p nlos-store --quiet` — 通过：11 个 test target 全部
  `test result: ok`，合计 48 passed / 0 failed / 1 ignored；其中
  `operation_activation_proof.rs` 1/1 passed（0.00s），`operation_prepare_activate.rs`
  2/2，`operation_store.rs` 17/17；唯一 ignored 为 `store_scale.rs` 的
  `one_hundred_thousand_operation_metadata_recovery_pending_and_ack`
  （explicit Stage B 100K Operation metadata scale probe）。
- `cargo clippy -p nlos-store --all-targets --all-features -- -D warnings` —
  通过，无任何告警输出（`Finished dev profile`）。
- `cargo fmt --all -- --check` — 通过（exit 0，无 diff）。
- `git diff --check` — 通过（exit 0）。
- `git rev-parse HEAD` = `6b7285ed3a52d2eb2703ac11407dcc4a242ff228`；
  `git status --short` 确认本 Attempt 写集仅为本文档（其余 modified/untracked
  条目均属其他并行车道，未触碰）。

## 明确未完成

这只是单机 SQLite Operation owner 的 activation proof readback，不是完整
dispatch/commit 闭环：

- **无 TaskWriteSet 消费**：activation proof 尚未接入 TaskWriteSet/
  participant registry 或任何 `TaskCommitReceipt` 路径，`B-TASK-008C2G-OP`
  的接线未消费本 proof。
- **无跨 authority activation**：没有跨 authority/TaskWriteSet prepare→
  activate、跨进程签名/attestation、lease/takeover fence；仍是进程内
  mutex + `BEGIN IMMEDIATE` 的单 owner。
- **无 Channel linkage**：没有 per-effect Operation/Channel endpoint 绑定，
  progress/stream callback 未实现。
- **无 corruption/fault 注入矩阵**：`inspect_activation_proof` 的三个
  `CorruptRecord` 分支与 schema v4 immutability trigger 只有代码路径，
  本次未对 `fault_crash`/`fault_io`/`fault_vfs` 风格的系统性篡改/故障矩阵
  覆盖 activation receipt 表。
- **无真实掉电验证**：重启 replay 仅是进程内 close/reopen，不等于真实
  power-loss/FS 级 durability 证明。
- **无本 Attempt 的 CI 结果**：候选代码与本文档均未提交、未推送，本 Attempt
  不产生任何 commit/CI run；后续由 integrator 统一晋升并记录提交号。
- **Operation completion** 未实现：activation 之后的效果回报/completion fence
  仍是后续切片。

本 Attempt 未修改任何 Rust 代码、progress sheet、ADR、Cargo 文件、
Resource/Task/Channel 文件或 git index/commit/remote/stash。

## 4. TaskWriteSet EffectPermit 消费接线（2026-08-24 增量）

本增量为 `TASK-EFFECT-ACTIVATION-GATE-01` Attempt：把上节已提交的
`inspect_activation_proof` 激活证明接入 Task 侧 `EffectPermit` 签发，作为
validation-only 门禁（B-OP-FENCE-003 未决项第一项「TaskWriteSet 消费接线」）。
base HEAD `7ffbfe8ca7db21201e252d2944fc2e0d9f749e5a`（起点工作区 clean；
本 Attempt 无任何 git index/commit/push 操作）。候选写集仅
`crates/nlos-task/src/effect.rs`（+100/−6）与新测试文件
`crates/nlos-task/tests/effect_activation_gate.rs`；`store.rs`/`lib.rs`/
`migrations.rs`/`model.rs` 零改动（`load_write_set_by_root` 原已是
`pub(crate)`，无需暴露新 loader）。

### 缝合点选择（seam rationale）

- **门禁放在 EffectPermit 签发，而不是 seal 或 commit-permit**：v24 先例
  （`request_commit_permit_with_operation_authority`，store.rs:1303）已在
  permit freeze 时做 endpoint 注册证明复读；本切片把消费端最后一道门放在
  one-shot token 铸造之前——owner 先激活（ADR-0005 authority-first），Task
  才消费证明。seal/commit-permit 路径与 v24 流程完全未动。
- **阶梯命名与线程化镜像 commit-permit 先例**：
  `request_effect_permit`（legacy，`inner(None)`）→
  `request_effect_permit_with_operation_authority(&SqliteOperationStore, req)`
  → `request_effect_permit_inner(Option<&SqliteOperationStore>, req)`，
  authority 在 replay 检查之后、mint 之前线程化注入。
- **Handle 重构与 `validate_operation_endpoint_bindings`（store.rs:3350）
  逐字段一致**：`OperationHandle { operation_id: endpoint.object_id,
  generation: endpoint.participant_generation }`；owner 错误一律经既有
  `TaskStoreError::OperationParticipantAuthority(nlos_store::StoreError)`
  透传（lib.rs:529），无 panic、无新增错误变体。

### Validation-only 决策

不新增 schema（无 v40）、不新增列、不改 root 变体、不持久化
`activation_receipt_id`/`preparation_receipt_id`：gate 只在签发事务内读回
sealed `TaskWriteSet` endpoint（按 `permit.write_set_root`）并调用
`inspect_activation_proof`，要求 `proof.operation == handle`（存在性 + 精确
generation；不 pin callback_id/cancel_epoch）。endpoint kind 非
OperationBinding 或 legacy 无 sealed write-set 的 permit 直接通过既有校验。

### Red → Green 记录

- **Red**：`cargo test -p nlos-task --test effect_activation_gate` →
  编译失败，9 个错误全部为 `error[E0599]`（缺少
  `request_effect_permit_with_operation_authority`），终止于
  `error: could not compile nlos-task (test "effect_activation_gate") due to
  9 previous errors`——失败原因正确（缺方法本身，非导入错误）。
- **Green（窄）**：同命令 → `test result: ok. 7 passed; 0 failed; 0
  ignored; 0 measured`。
- **全量**：`cargo test -p nlos-task --quiet` → 211 passed / 0 failed /
  0 ignored（204 基线 + 本切片 7，无既有测试回归）。
- `cargo check -p nlos-task` 通过；`cargo clippy -p nlos-task
  --all-targets --all-features -- -D warnings` 通过（无告警）；
  `cargo fmt --all -- --check` 与 `git diff --check` 通过。

### 7 项 Given/When/Then 覆盖（tests/effect_activation_gate.rs）

1. **happy**：seal（OperationBinding，Registered）→ v24 commit permit →
   owner `prepare_dispatch`+`activate_dispatch` → 门禁签发 one-shot token，
   slot → `Permitted`。
2. **仅注册**：未 prepare → `OperationParticipantAuthority(
   DispatchPreparationNotFound)`，slot 仍 `Planned`、无 permit；激活后同键
   重试 → `Issued`（非 `Replayed`，证明失败调用零持久化）。
3. **仅 prepare**：未 activate → `OperationNotActivated` 包装，slot 仍
   `Planned`。
4. **stale generation**：sealed 代际 ≠ owner 当前代际（构造同一
   operation_id 在另一 authority 注册于 `checked_next` 代际）→
   `Operation(OperationError::InvalidGeneration)` 包装，slot 仍 `Planned`。
5. **replay 信任 Task 行**：铸币后重开 Task authority 并传入全新空
   Operation authority，同请求两次 → 均返回同一 durable token
   （`Replayed`），`inspect_effect_permit` 与原记录全等、slot 仍
   `Permitted`——空 authority 上任何 owner 读回都会失败，故该测试直接证明
   replay 不再读 owner（Task 行是 replay 权威，镜像 store.rs:1352-1358）。
6. **legacy 边界**：同一仅注册 slot 上 authority-free
   `request_effect_permit` 照常铸币——强化只存在于新变体（legacy 缺口
   by design，镜像 commit-permit 阶梯）。
7. **非 Operation endpoint 直通**：ArtifactHead endpoint + 空
   Operation authority → 门禁方法照常铸币，证明 gate 仅对
   OperationBinding 生效。

### 明确缺口（本增量后仍成立）

- legacy authority-free `request_effect_permit` 不做激活校验（设计如此，
  测试 6 固化边界）。
- Task 侧未持久化任何 activation 事实（activation_receipt_id 不入 Task
  行；消费证据仅为签发时刻的读回）。
- 无跨 authority 激活原子性：gate 只读本 owner；测试 4 的第二 authority
  只是构造 stale 手段，不构成跨 authority 保证。
- 本 gate 无 fault/corruption 注入矩阵（activation 表篡改路径未覆盖）。
- 候选代码与本文档未提交、未推送：无 CI 结果；PARTIAL_PASS 维持，由
  integrator 晋升后补记提交号。
