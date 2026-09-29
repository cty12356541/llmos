# B-TASK-008C2G-OP：Operation endpoint 的 TaskWriteSet / participant registry 接线

- 状态：`PARTIAL_PASS`
- 日期：2026-08-15
- 范围：schema v24 将 owner-derived Operation endpoint 纳入
  `TaskWriteSet` per-effect endpoint 与 Task participant registry，并在 seal
  和 permit freeze 前做精确 `OperationId + Generation` owner readback。

## 结论

`SqliteTaskAuthority` 新增 Operation participant registration、Operation-aware
TaskWriteSet seal 以及 Operation-aware CommitPermit API。TaskAuthority 不接受
caller-supplied participant tuple：它通过 `SqliteOperationStore::inspect_endpoint_proof`
读取 durable registration row，派生并复核 `TaskParticipantId`、generation 和
admission receipt，再确认该 participant 已存在于 OPEN registry。旧 generation
在 proof 生成前被拒绝；permit replay 返回原决定，不重复 owner readback。

schema v24 只扩大既有 immutable `participant_type`（1..8）和
`endpoint_kind`（1..6）约束，迁移复制历史行并保留 immutable triggers；没有给
历史数据补造 Operation 事实。

## 验证

- `verified_operation_endpoint_is_rechecked_during_seal_and_permit`：Operation
  participant registration、stale generation seal 拒绝、owner-aware seal、无
  authority permit 拒绝、正确 authority permit/replay 与 Operation/Task 重启回读。
- `cargo test -p nlos-task --test participant_registry verified_operation_endpoint_is_rechecked_during_seal_and_permit -- --nocapture`
- `cargo test -p nlos-task --test artifact_commit_plan --quiet`（旧版本迁移兼容）

## 明确缺口

该切片不实现 Operation prepare→activate/dispatch、跨进程签名/租约/attestation、
Channel endpoint、Operation effect completion，也不消费 Semantic/Artifact/Resource
publication receipt；因此不等同于完整 TaskWriteSet 或统一 `TaskCommitReceipt`。

## §W29-C：finalize 时 activation receipt owner 复核（ADR-0017 决定 2 / 附录 B，2026-09-21）

- 状态：增量 `PASS`（本节范围）；负向门（无 Task 侧 plan/台账/worker cycle）同节证明
- 车道：W29-C（nlos-task 串行位 3，波内屏障 2）；base `91203ac`
- 关联：[ADR-0017](../../management/adrs/0017-resource-operation-cross-authority-prepare-finalize.md)
  决定 2（O-B：仅 verify 半边）+ 附录 B；[B-OP-FENCE-002](b-op-fence-002-operation-endpoint-proof.md)
  「明确未完成」清单第一项；[B-OP-FENCE-003](b-op-fence-003-dispatch-activation-readback.md)
  §4（EffectPermit 铸币门，本节的消费端另一半）

### 缝合点（gate 设计）

1. **finalize 半边（本节新增）**：`FinalizeSpec` 新增
   `operation_authority: Option<&SqliteOperationStore>` 槽位；
   `finalize_commit_v3_with_spec` 在终端 Task 事务开启前，经
   `verify_owners_for_spec_finalize` 对已封存 write set 的**每个**
   `OperationBinding` endpoint 回读 owner 的
   `inspect_activation_proof`（guard-only，可与 Semantic/Resource 槽位任意组合，
   四个 spec 分支全部接入）。非 Issued permit（replay）与无 sealed write set 的
   legacy permit 直接跳过——closed permit 的 replay 只读 Task 行。
2. **共享映射（mint/finalize 同一门）**：`effect.rs` 新增
   `verify_operation_activation`（单一 owner 回读 + fail-closed 分类）与
   `verify_sealed_operation_activation_endpoints`（封存端点迭代）；
   `check_effect_slot_activation`（`[B-OP-FENCE-003]` §4 铸币门）重构为复用同一
   映射——「纳入 participant/effect binding」即两处消费门共用同一绑定复核语义。
3. **类型化错误（命名 Operation）**：`TaskStoreError` 新增四变体——
   `OperationDispatchNotPrepared { operation_id, generation }`（仅注册）、
   `OperationDispatchNotActivated { .. }`（prepared 未激活，owner 仍 Registered，
   可重试）、`OperationDispatchCancelled { .. }`（激活前取消，owner 终态
   `CancelledBeforeEffect`，不可再激活）、`OperationDispatchStaleGeneration
   { operation_id, sealed_generation }`（代际漂移/owner 未知）。canceled 与
   not-activated 的区分：`OperationNotActivated` 应答后再读一次 owner 状态机
   （`inspect(handle)`）分类终态形状；owner 存储失败仍以
   `OperationParticipantAuthority(StoreError)` 透传（与 RES/ART/PROCESS 家族一致）。
4. **语义边界（记录在案）**：门对封存的 OperationBinding endpoint **无条件**生效
   （镜像 `validate_semantic_finalization`/`verify_owner_cost_receipts` 对各自
   封存段的无条件复核）；在派发前以 NoEffect 关闭的 Operation slot 属诚实
   no-effect 证据，应经**不含** operation authority 的 finalize 入口终结。
   铸币门错误形状由 wrapped `StoreError` 升级为命名变体（行为变更，本节记录）；
   ladder 构造器不新增 operation rung（沿 `88a9775a` 阶梯弃用方向，struct 入口
   为权威面）。

### 负向门证明（ADR-0017 附录 B）

- **schema 断言**（`negative_gate_no_task_side_operation_plan_machinery_in_schema`）：
  全新 Task authority 打开后，`user_version == 44`（本车道零 schema 变更）；
  `sqlite_master` 中 `task_operation%` 表族为空；显式断言
  `task_operation_plans`/`task_operation_recovery`/
  `task_operation_recovery_alert_receipts`/`task_operation_finalize_envelopes`/
  `task_operation_finalize_satisfactions`/`operation_commit_plans`/
  `operation_dispatch_ledger` 均不存在。
- **代码级**：本车道写集仅 `nlos-task` 既有文件 + 新测试文件，无
  `operation_plan.rs`/`operation_recovery.rs` 类新机械文件；`nlos-commit-coordinator`
  零改动（写集排除）；恢复归属维持 EffectSlot/effect history/`EFFECT_UNKNOWN`
  quarantine-reconcile（进度单 §4.23/§4.25/§4.26）不动。

### 验证（2026-09-21，base `91203ac` 工作区）

- `cargo test -p nlos-task --test operation_finalize_gate --quiet` → 8/8：
  happy（激活后门通过并 Committed）、未 prepare ⇒ `NotPrepared` 命名 Operation
  （零终结变更；激活后同请求重试 Committed，证失败调用零持久化）、仅 prepare ⇒
  `NotActivated`（同上重试窗口）、激活前 cancel ⇒ `Cancelled`（与可重试形态区分）、
  漂移 authority ⇒ `StaleGeneration`（sealed_generation 命名）、finalize replay
  只读 Task 行（对全新空 Operation authority 逐字节 `Replayed`）、owner
  prepare/activate 重启 exact replay 跨两道门保持（重启后 `Replayed` 决策 +
  proof 逐字段相等 + 门通过 + Committed）、负向 schema 门。
- `cargo test -p nlos-task --quiet` → 47 个 test target 全绿（343 passed /
  0 failed / 2 ignored，含本节 8 项新增）；既有断言适配：`effect_activation_gate`
  3 处与 `effect_combined_gate` 2 处升级为命名变体（replay 测试逐字未动），
  `finalize_spec` 2 处机械补 `operation_authority: None` 字面量。
- `cargo test -p nlos-store --quiet` → 全绿（48 passed / 1 ignored；owner 侧
  prepare/activate 重启 exact replay 语义零回归——本车道 owner 代码零改动）。
- `cargo clippy -p nlos-task --all-targets --all-features -- -D warnings` 通过；
  `cargo fmt --all -- --check` 与 `git diff --check` 通过；新代码无
  `chunks_exact(N).map(...)` 模式（规避 CI clippy 1.98 `as_chunks` lint）。
- 未提交/未推送（车道纪律）；三平台 CI 回填由 integrator 晋升后补记。

### 遗留（本节后仍成立）

- 无跨 authority 激活原子性：两次 owner 读（proof + 状态分类）各自一致，Task CAS
  仍是线性化点（verify-then-commit，ADR-0013）。
- 无 fault/corruption 注入矩阵（activation 表篡改路径仍只有 B-OP-FENCE-003 §4
  登记的代码路径面）。
- progress sheet §6.5.3 W29-C 行与 ADR 附录勾稽由 integrator/控制器波次簿记同步
  （不在本车道写集）。
