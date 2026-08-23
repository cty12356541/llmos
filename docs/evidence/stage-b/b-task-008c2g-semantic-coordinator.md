# B-TASK-008C2G-COORD：Semantic publication cross-authority coordinator

状态：`PARTIAL_PASS`（2026-08-23）

## 1. 结论

本切片把 ADR-0006 选择 1 的本地两 authority 前缀接成可重启收敛的 coordinator：它只驱动已由 `TaskAuthority` 持久化的 Semantic plan，不拥有新的事实源；每次跨 authority 调用都从上一次 durable prefix 继续。schema v26 又把混合 Effect finalize 所需的 typed proof envelope 持久化，使 coordinator 能在重启后重建 v3 request；同时，`EffectClosedSuccess` 已收紧为绑定 slot contract 与权威闭合 Receipt 的本地 proof。schema v27 再增加单个 `TaskAuthority` 的 durable lease/term/fencing 原语，schema v28 将该 lease 以 opt-in immutable binding 接入 `CommitPermit` 签发、plain v3 finalize、pre-effect close、mixed persisted-envelope finalize/replay 以及 Semantic-only high-level finalize；schema v29 再把同一 live lease 绑定到 adoption/reconcile 的本地安全子集，并增加 `FROZEN_FOR_TAKEOVER` 的 local takeover-fence CAS pre-gate；schema v30 持久化该 pre-gate 的 immutable local fence receipt，并在 durable participant mapping 完整时计算 frozen registry ∪ durable outstanding-operation participant 的 exact local roots；schema v31 为 lease-bound permit 持久化 immutable local `TaskAuthorityAssignment` baseline；schema v32 再把旧 assignment 的 `TakeoverPending` 与本地 pending `TaskAuthorityTakeoverReceipt` 前缀在同一事务持久化；schema v33 新增逐 endpoint 的 immutable barrier receipt observation；schema v34 再持久化 exact fence-set 的 canonical member manifest，使 observation 能覆盖 durable outstanding-operation participant；这些增量都不推进 parent state 或激活 successor assignment。

2026-08-23 增量新增 `B-TASK-008C2G-COORD-SEM-FAULT-1`：为 Semantic owner publication 接入仅供测试使用的 named SQLite VFS，并加入 owner 写失败、静默丢写、WAL 撕裂尾部、Task consumer 写失败及终态 finalize 写失败后的 durable-prefix/restart 矩阵；9 项测试全部通过。该增量仍只证明本地 SQLite 故障前缀，不改变跨 authority 或 Principal attestation 语义。

同日新增 `B-TASK-008C2G-COORD-SEM-RESTART-SCAN`：在 owner publication 已提交而 TaskAuthority 进程退出后，重新打开 TaskAuthority，调用公开 `converge_pending` 仅凭 durable plan/expectation 完成 owner exact replay、Task receipt 消费和 terminal finalize；目标测试二进制共 5 项（4 项既有 baseline + 1 项新增）通过。该增量仍只证明本地 bounded restart convergence。

随后新增 `B-TASK-008C2G-COORD-SEM-MIXED-RESTART`：在同一 sealed TaskWriteSet 中同时存在 Effect 与 Semantic 计划时，持久化 mixed finalize envelope，完成 Semantic owner publication，再关闭 optional Effect slot；丢弃并重开 TaskAuthority 后只调用公开 `converge_pending`，恢复为单条 nested `TaskCommitReceipt` 与 `FINALIZED`。该增量仍是单机 mixed-envelope recovery evidence，不代表完整 TaskWriteSet 或跨 authority 原子提交。

## 2. 已实现事实

- schema v35 为 `task_authority_takeover_barrier_receipts` 增加 nullable `barrier_receipt_digest`：新 observation 持久化并在重启后读回 caller 提供的 remote barrier digest；v33/v34 旧行无法重建该值，迁移保留 `NULL`，不伪造审计事实。
- barrier observation 写入与 coverage 查询现在都会重新计算 manifest 的 canonical `exact_fence_set_root`，并逐行校验 fence receipt、Task generation 与 participant binding；manifest 行漂移即 fail closed，不会把损坏的 coverage 晋升为本地覆盖。
- `inspect_authority_takeover_fence_members` 也对完整 root/manifest 做同一校验；root 已知却缺少 manifest，或 root 未知却出现 manifest，均按损坏记录拒绝读回。

- `SemanticCommitCoordinator` 复用 `TaskAuthority` 的 `PLANNED → PUBLISHING → READY → FINALIZED` 状态机：先授权，再按 sealed `TaskWriteSet` 声明调用 `SemanticAuthority::publish_semantic_publication`，随后消费 owner receipt，最后由 TaskAuthority 原子写入 nested Task receipt。
- `prepare_semantic_finalize` 在 schema v26 写入 immutable mixed-finalize envelope 与 typed required-satisfaction child rows；重复请求逐字节 replay，缺失/重复/非 required slot proof fail closed。
- `TaskAuthority::inspect_semantic_commit_expectations` 只读 sealed Semantic append declarations；`list_incomplete_semantic_commit_plans` 提供稳定的 bounded restart scan，不允许 coordinator 注入新的 event/target/receipt binding。
- owner publication 重试使用 SemanticAuthority 的 exact replay；Task-side receipt consumption 继续逐字段校验 owner readback、write-set root、target、Admission/Durability receipt 和 plan identity。
- `converge_pending` 在重启后扫描未完成计划；检测到 v26 envelope 时调用 persisted v3 finalize，已 `FINALIZED` 的计划不会再次进入 pending scan，但显式 replay 返回原 nested receipt。
- `expected_success_assertion_digest` 以 slot identity、`success_criteria_digest`、EffectReceipt identity/proof 和闭合 kind 计算 domain-separated digest；v3 finalize 逐位校验 Receipt 与 slot 绑定，错误或跨 slot 复制的摘要 fail closed。
- schema v27 的 `task_authority_leases` 与 immutable `task_authority_lease_history` 在同一 `SQLite` 事务中记录 holder、term、lease epoch、fencing token 和过期时间；同 holder 只能续租，过期后新 holder 推进 term，旧记录在当前行校验时 fail closed。
- schema v28 在 `commit_permits` 持久化可选 authority/holder/term/epoch/token/expiry binding；带 binding 的 permit 只能由同一 live lease 走 opt-in v3 finalize、pre-effect close、mixed persisted-envelope finalize/replay 或 Semantic-only high-level finalize，旧 term 在签发与终结两处都 fail closed，legacy permit 继续保持显式 unbound。
- schema v29 在 `task_adoption_receipts` 持久化可选 authority/holder/term/epoch/token/expiry binding；lease-bound quarantined permit 的 adoption 与后续 unknown-slot reconcile 必须带同一 live lease，binding UPDATE 被 immutable trigger 拒绝；已解决 reconcile 的 replay 仍可读。
- 新增 `prepare_authority_takeover_fence`：新 term 的 live lease 以 expected registry generation/root 做 CAS，把当前 registry 持久置为 `FROZEN_FOR_TAKEOVER`，同一事务递增 Task `control_epoch`；重复调用只读回原冻结事实，旧 lease、旧 registry binding 与冻结后的新 permit/adoption 写入均 fail closed，重启后状态保持。
- schema v30 新增 `task_authority_takeover_fence_receipts`：receipt 逐位绑定 Task generation、frozen registry generation/root、new live lease 和 control epoch；UPDATE/DELETE 由 immutable trigger 拒绝，并在 durable participant mapping 完整时以确定性 canonical participant set 计算 `exact_fence_set_root` 与 `outstanding_operation_participant_root`（无 outstanding participant 时为全零 root，否则映射不完整时保持 NULL）。这只是本地 durable union fact，不把它冒充远端 barrier Receipt 或 successor Assignment。
- schema v31 新增 `task_authority_assignments`：lease-bound permit 在同一 TaskAuthority transaction 建立/刷新当前 term 的 immutable assignment identity，并允许 lease renewal 更新复制的 live binding；assignment identity/state 由 trigger 保护。schema v32 在新 term fence 时把旧 assignment 置为 `TakeoverPending`，并以不可变 pending `TaskAuthorityTakeoverReceipt` 链接旧 assignment、local fence receipt、frozen roots 与新 term/control epoch；`new_assignment_id` 固定为空，未声称远端 barrier 完成。
- schema v33 新增 `task_authority_takeover_barrier_receipts`：仅接受 frozen registry 中的 endpoint、pending takeover 的 exact local fence-set root 与 caller 提供的 remote receipt identity/digest；记录状态是 `Observed`，UPDATE/DELETE 由 immutable trigger 拒绝，重复提交逐字节回放。它不验证远端签名/attestation、不把 observation 计入完成条件，也不改变 parent `Pending` 状态。
- schema v34 新增 `task_authority_takeover_fence_members`：在 local fence roots 可完整计算时，按稳定 `(participant_type, participant_id)` 顺序持久化 frozen registry ∪ durable outstanding-operation participants 的 canonical manifest；manifest UPDATE/DELETE 由 immutable trigger 拒绝，升级后的 replay 可补写缺失 manifest。barrier observation 现在按该 manifest 校验 endpoint，而不是只看 registry 子集。
- 新增只读 `inspect_authority_takeover_barrier_coverage`：返回 `ManifestUnavailable`、`Partial` 或 `LocallyCovered` 及缺失 participant；`LocallyCovered` 仍只表示本地 observation 覆盖 manifest，parent takeover receipt 保持 `Pending`，没有 successor activation。
- `SemanticAuthority::open_with_vfs(root, Option<&str>)` 仅为测试故障注入选择 named SQLite VFS；`open(root)` 保持默认 VFS 行为，生产调用路径不改变。

## 3. Evidence

- `cargo test -p nlos-commit-coordinator --test semantic_convergence -- --nocapture`：4 项通过；覆盖授权→发布→READY→重启后 finalize、显式 replay、pending scan、负时间戳在任何 mutation 前拒绝、TaskAuthority 写失败后 owner publication exact replay 与 durable prefix 收敛，以及 mixed v3 envelope 重启恢复。
- `cargo test -p nlos-commit-coordinator --quiet`：coordinator 既有 Artifact recovery 测试与 Semantic coordinator 测试全绿。
- `cargo test -p nlos-task --test effect_history -- --nocapture`、`cargo test -p nlos-task --test effect_reconcile -- --nocapture`、`cargo test -p nlos-task --test reconcile_fault_injection -- --nocapture`：29 项通过；覆盖错误 success assertion、闭合 Receipt/slot 绑定、reconcile/replay 与 v3 VFS 故障前缀。
- `cargo clippy -p nlos-task -p nlos-commit-coordinator --all-targets -- -D warnings`：通过。
- `cargo clippy -p nlos-commit-coordinator --all-targets --all-features -- -D warnings`：通过。
- `cargo test -p nlos-task --test semantic_commit -- --nocapture`：2 项通过；Semantic-only 与 mixed Effect + Semantic publication path 均使用 lease-bound permit、owner revalidation、对应 finalize/replay 的 opt-in lease API；缺少 lease 的首次 terminal mutation fail closed。
- `cargo test -p nlos-task --test authority_lease -- --nocapture`：5 项通过；新增覆盖 lease-bound permit 建立 assignment baseline、新 term live lease 的 takeover-fence CAS、旧 assignment 进入 `TakeoverPending`、pending takeover receipt、exact fence member manifest、逐 endpoint barrier observation 与只读 coverage view 的 immutable trigger、精确 replay 与 restart readback、v34→v35 digest-column migration、未知 endpoint 拒绝、control epoch 单调推进、旧 lease 拒绝和冻结后新 permit 拒绝。
- `cargo test -p nlos-task --test effect_reconcile -- --nocapture`：13 项通过；新增覆盖冻结后 fresh adoption 拒绝、既有 adoption exact replay 保持可读。
- `cargo test -p nlos-task --quiet`：TaskAuthority 全部测试通过；`cargo clippy -p nlos-task --all-targets -- -D warnings`：通过。
- `cargo test -p nlos-commit-coordinator --test semantic_pending_restart_scan --quiet`：5 项通过；覆盖 owner publication durable 后丢弃/重开 TaskAuthority，由公开 `converge_pending` 完成 owner replay、单条 SemanticTaskCommitReceipt 消费和 `FINALIZED` 收敛；targeted Clippy、fmt、diff check 通过。Rust cross-platform/MSRV CI [32614979438](https://github.com/cty12356541/llmos/actions/runs/32614979438)、Pages [32614979403](https://github.com/cty12356541/llmos/actions/runs/32614979403) 与 Schema fuzz smoke [32614979395](https://github.com/cty12356541/llmos/actions/runs/32614979395) 均成功。

## 4. 明确限制

- 只覆盖单机 SemanticAuthority/TaskAuthority、Semantic-only coordinator；schema v27–v35 的租约、permit/adoption binding、local fence receipt、assignment baseline、pending takeover receipt、fence member manifest 与 barrier observation，都是单 authority opt-in primitive，不是 IPC peer authentication、跨 authority adoption 或完整 term takeover 协议。当前 lease binding 已覆盖 mixed Effect + Semantic owner/publication finalize、persisted envelope replay、plain v3 finalize、pre-effect close、Semantic-only high-level finalize，以及 same-term adoption/reconcile；新 fence receipt 在可完整映射的 durable write set 上固定 frozen registry ∪ durable outstanding-operation participant 的 roots，映射不完整时保留 NULL；pending takeover receipt 只链接旧 assignment 与 local fence，`new_assignment_id` 仍为空；barrier observation 按 v34 manifest 固定 endpoint/root，schema v35 起持久化 remote receipt digest（旧行保持 `NULL`），coverage view 只报告本地覆盖度，未验证远端签名/attestation，也没有 parent completion、successor registry/assignment 激活或跨 term adoption。takeover 表组的本地完整 kill-9/ENOSPC/torn-write 组合矩阵已由 [B-TASK-008C2G-FAULT](./b-task-008c2g-takeover-fault-matrix.md) 接入（macOS 本地，7 测试）；三平台 CI 复验、checkpoint/backup/migration 变体、v28/v29 lease-binding 列逐列注入与多 Cell 传播证据仍未接入。新增的故障证据是 TaskAuthority SQLite abort/VFS 写失败，不等于真实硬件掉电/跨机器原子性。
- mixed v3 envelope 必须在 publication 前由 permit holder 准备；当前 proof 只在 TaskAuthority 内绑定本地 slot contract 与已持久化 EffectReceipt，仍不验证外部 provider 的语义成功内容、签名、attestation 或跨进程 authority lease。
- 不把 outbox ACK、local log-prefix digest 或 coordinator observation 晋升为 Trust View/vector checkpoint，也不声称分布式原子提交。

## 5. 增量 Evidence

### 5.1 Semantic publication coordinator 故障前缀与重启矩阵（B-TASK-008C2G-COORD-SEM-FAULT-1）

- 新增 `crates/nlos-commit-coordinator/tests/semantic_convergence_fault_injection.rs`，复用既有 coordinator fixture，串行化进程级 `nlos-store-fault` VFS 注入；覆盖 Semantic owner `IOERR`/`ENOSPC`、`PowerLossAfter` 静默丢写、子进程持有 WAL 后强杀并截断最后 commit frame、owner receipt 已提交但 Task consumer 写失败，以及 READY 后 Task terminal finalize 写失败。
- 每个故障场景均检查 durable prefix：失败不制造 publication/head 幻影；owner receipt 已存在时重试保持单 receipt；READY/finalize 失败重启后只完成一次 `TaskCommitReceipt`；恢复路径的 publication/task receipt 与计划 identity exact replay。
- `cargo test -p nlos-commit-coordinator --test semantic_convergence_fault_injection -- --nocapture`：9 项通过；既有 `semantic_convergence` 4 项通过；`cargo test -p nlos-commit-coordinator --quiet`、`cargo test -p nlos-semantic --quiet`、workspace 通过；`cargo clippy -p nlos-semantic -p nlos-commit-coordinator --all-targets --all-features -- -D warnings` 与 `cargo fmt --all -- --check` 通过。常规 Rust + MSRV CI [32584611547](https://github.com/cty12356541/llmos/actions/runs/32584611547) 与 Pages [32584611605](https://github.com/cty12356541/llmos/actions/runs/32584611605) 均成功。该证据覆盖本地 SQLite 故障前缀与重启恢复，不外推真实硬件掉电、跨机器原子提交、外部 provider proof、Trust View/vector checkpoint 或 Principal peer attestation。

### 5.2 Semantic pending restart scan（B-TASK-008C2G-COORD-SEM-RESTART-SCAN）

- 新增 `crates/nlos-commit-coordinator/tests/semantic_pending_restart_scan.rs`，复用既有 fixture：先让 TaskAuthority durable prefix 进入 `PUBLISHING`，再由 SemanticAuthority 提交 owner publication，丢弃 TaskAuthority 实例并从同一 SQLite 文件重开。
- 公开 `converge_pending(1, 9)` 从持久化 plan/expectation 重建 owner replay，消费单条 Task receipt，推进 `FINALIZED`，并确认后续 incomplete scan 为空；caller 没有提供新的 event/target/receipt binding。
- `cargo test -p nlos-commit-coordinator --test semantic_pending_restart_scan --quiet`：5 项通过；targeted Clippy、fmt、diff check 通过。该证据只覆盖本地 SQLite bounded restart recovery，不外推真实硬件掉电、跨机器原子提交、外部 provider proof、Trust View/vector checkpoint、Principal attestation 或完整 TaskWriteSet。

### 5.3 Mixed Effect + Semantic pending restart convergence（B-TASK-008C2G-COORD-SEM-MIXED-RESTART）

- 新增 `crates/nlos-commit-coordinator/tests/semantic_pending_mixed_restart_scan.rs`：先以 `prepare(&fixture, true)` 建立 Effect + Semantic sealed write set，持久化 mixed finalize envelope 并完成 Semantic owner publication，再经 `request_effect_permit`/`record_no_effect` 关闭 optional Effect slot。
- 丢弃 `TaskAuthority`、从同一 SQLite 文件重开后只调用公开 `converge_pending(1, 11)`；断言 Semantic publication 恰一条、TaskCommitReceipt 恰一条、状态为 `FINALIZED`、envelope 仍可读且 incomplete scan 为空。目标测试二进制 5 项通过，targeted Clippy/fmt/diff check 通过。
- 该证据只覆盖本地 mixed envelope 的 bounded restart convergence；不外推跨机器原子提交、完整所有权证明、外部 provider attestation、Trust View/vector checkpoint 或完整 TaskWriteSet。
