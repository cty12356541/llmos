# B-TASK-008C2G-COMPLETE-WRITESET：complete TaskWriteSet 六域闭环验收（B3-3，车道 W30-A）

状态：`PARTIAL_PASS`（2026-09-21，W30-A）

> 对应：[ADR-0005 authority-first 顺序](../../management/adrs/0005-task-write-set-authority-first.md)、[ADR-0013 跨 authority verify-then-commit 契约](../../management/adrs/0013-cross-authority-verify-then-commit-contract.md)、[ADR-0017](../../management/adrs/0017-resource-operation-cross-authority-prepare-finalize.md)（决定 2 O-B verify 半边）、[进度单 §6.5.3 W30-A 车道行](../../management/stage-b-progress.md)（B3-3）
>
> 依赖（本波次已全部合入 main）：W26 统一 TaskCommitReceipt 读侧聚合（`receipt.rs` 五变体 + digest）与 semantic/artifact 恢复台账；W28-C/W28-C-3 resource prepare/finalize coordinator（schema v43）与 worker 第三域；W29-C Operation activation-receipt verify gate（`FinalizeSpec.operation_authority`）；W29-A TaskSpec 关联字段；struct 面 `Authorities`/`FinalizeSpec` 组合入口（`combined_authority_seal.rs`/`finalize_spec.rs` 先例）
>
> 实现：`nlos-task/tests/six_domain_write_set_closure.rs`（全域 e2e 3 项）+ `nlos-task/tests/six_domain_fault_matrix.rs`（kill-window 矩阵 9 项，含 crash-child helper）。**零生产代码改动**——本车道为对已落地机械的验收测试（写集限定 nlos-task tests；混合终结的驱动路径在 nlos-task 直连 API，nlos-commit-coordinator 只读消费，本轮零改动零回归）

## 1. 本切片目标

证明一张写集可以同时携带全部六个域面并走完 seal → 唯一 permit → 逐域驱动 → 终结 → 统一 `TaskCommitReceipt`：

| 域 | 写集面 | 驱动 | 终结参与 |
|---|---|---|---|
| Effect | `planned_effects`（2 slot） | effect permit → dispatch token → `Outcome::Closed` | base `TaskReceiptRecord.new_effect_history_root`（durable history 计算，非空推进） |
| Artifact | `artifact_reads`（1）+ `artifact_writes`（1，需先注册 ArtifactHead participant） | owner stage → Task plan → authorize → owner publish → record → plan READY | **边界**：见 §3 |
| Semantic | `semantic_appends`（1，admission participant 先注册） | plan → authorize → owner publish → record → READY | 嵌套 `semantic_publications`（SemanticResource 变体） |
| Resource | `resource_reservations`（2，driver/ledger participant 先注册） | owner activate → consume → finalize（FINALIZED） | 嵌套 `resource_cost_receipts`（+守恒断言） |
| Operation | `OperationBinding` endpoint（slot 0，participant 先注册） | owner `prepare_dispatch` → `activate_dispatch` | finalize 前 activation receipt owner 复核（guard-only） |
| Channel | `ChannelTopicBinding` endpoint（slot 1，participant 先注册） | 无（无发布协议） | **边界**：见 §3 |

seal/permit 走 struct 组合入口：`seal_task_write_set_with_authorities_struct`（六 authority 全给）与 `request_commit_permit_with_authorities_struct`（artifact/resource/operation/channel 复核）；终结走 `finalize_commit_v3_with_spec`（`semantic_plan` + `resource_authority` + `operation_authority` = Combined rung + O-B 门）——即阶梯中最大变体 `SemanticResource` 加 Operation 门，任何单一阶梯构造器都带不齐这个组合（`combined_authority_seal.rs` 先例所证缺口由 struct 面弥合）。

## 2. 验收测试（e2e，`six_domain_write_set_closure.rs`，3/3）

| 测试 | 断言要点 |
|---|---|
| `six_domain_write_set_seals_permits_and_finalizes_with_unified_receipt` | 封存记录六面俱全且 owner 派生（Artifact 写对照 owner head、Semantic append 绑 admission receipt、双 Reservation、双 planned effect、双 endpoint 的 participant_id == 各 owner `inspect_endpoint_proof`）；写集幂等重放逐位相等；permit 重放 Replayed；逐域驱动后 Combined 终结一次事务提交：head +1、permit Closed、effect history root 非空推进且等于 Task 行读回、semantic 嵌套 == owner publication 逐字段、resource 嵌套 == owner `inspect_cost_receipt` 聚合逐字段 + upper−usage=refund 守恒、plan FINALIZED 绑 receipt；统一回执 `TaskCommitReceipt::SemanticResource` 从 durable 行重建逐位相等且 digest 确定；Artifact 边界钉死（§3） |
| `six_domain_replay_after_restart_reads_only_task_rows` | 提交后丢弃全部 owner 与 Task 连接，重开 Task，对**全新空** Semantic/Resource/Operation 三 authority 重放同请求 → `Replayed` 逐字节相等、digest 相等、四表恰一套行零重复——重放只信 Task 行 |
| `six_domain_channel_face_rotation_between_seal_and_permit_fails_closed` | seal 后 rotate channel → permit freeze typed 拒绝（`Channel endpoint proof differs before permit freeze`）、封存记录原样存活——Channel 面的唯一门是 seal/permit 边的 generation 复核 |

## 3. 诚实边界（domain 排除与原因，逐条登记）

1. **Artifact 发布面不能搭乘混合终结事务**（结构性，非缺陷修复项）：`finalize_ready_plan`（`commit.rs`）对 effect slot 非零的 permit typed 拒绝（`"Artifact finalize requires an artifact-only permit"`）；统一回执五变体中无 Artifact+Semantic/Resource 组合变体。因此混合写集里 Artifact 面的机械可驱动到 plan READY + 发布回执持久（`task_artifact_publication_receipts`），但 (i) 其自身 rung 在终结前 typed 拒绝混合 permit（e2e 钉死该拒绝），(ii) Combined 终结对 Artifact 面**无任何门**——故障矩阵窗口 i 钉死：artifact 完全未驱动时终结照常提交、声明写从未发布且无拒绝（诚实暴露，非伪造参与）；窗口 ii 钉死：READY plan 在终结后保持 READY、发布行持久、不链接任何 Task receipt（`task_receipt_id=None`，`list_incomplete_artifact_commit_plans` 持续可见）。候选后续（需 spec/设计决定）：Artifact 嵌套与 Semantic/Resource 的组合变体，或混合终结前的 artifact 完整性门。
2. **Channel 面到 endpoint binding 为止**：durable 注册行 + seal/permit 两道 proof 复核（rotation → typed fail-closed）是其全部机械；无发布协议、无 finalize 门、终结参数不含 Channel authority（重放自然不读它）。按车道指令如实登记而非强行造参与。
3. **Operation 是 verify 门不是嵌套回执**：activation receipt owner 复核（W29-C）guard-only——封存端点在终结前被无条件复核（NotPrepared/NotActivated/Cancelled/StaleGeneration 四类 typed 拒绝，矩阵钉 NotActivated 窗口），但统一回执无 Operation 嵌套字段；这是 ADR-0017 决定 2（仅 verify 半边）的既定形态。
4. 六域「闭环」口径：seal/permit 层**六域全闭**；终结回执层 Effect(base)+Semantic+Resource 嵌套 + Operation 门闭，Artifact/Channel 按上述边界登记——本证据状态 `PARTIAL_PASS` 的唯一根因即此。

## 4. 故障矩阵（`six_domain_fault_matrix.rs`，9/9）

故障模型与 W/WE 系列同源：单机本地 SQLite + `nlos-store-fault` VFS（kill-9 子进程 + piped `READY` 同步、`PowerLossAfter` 进程级丢写、`FAULT_LOCK` 串行），全部注入只指向 Task authority 连接；owner 连接走普通 VFS。子进程停在静止边界（此前所有提交均已持久），父进程 kill-9 后重开，**所有 owner 侧步骤幂等**故同构造器重放即完成缺口域。每行结尾 `PRAGMA integrity_check` = ok；每行收敛后对空 owner 重放 == durable commit 逐字节（replay 只信 Task 行）。

| 行 | 域面/窗口 | 预期（沿 W/WE 断言式样） | 实际 |
|---|---|---|---|
| 1 | Effect / crash before slot outcomes | 重开：Combined typed `OutstandingEffectSlots{2}` 拒绝、终结事务三表零行、permit 仍 Issued；补齐双 slot outcome 后同请求 → Committed 唯一终态（head+1、四表恰一套） | 一致（PASS） |
| 2 | Semantic / crash after plan-authorized, before owner publish | typed `SemanticCommitPlanNotReady{Publishing}`、零 publication 行；重启后 owner publish+record → Ready → 收敛唯一终态 | 一致（PASS） |
| 3 | Resource / crash before owner settle | typed `ResourceParticipantAuthority`（错误链含 owner 门）、零终结行、permit Issued；重启后 settle → 收敛唯一终态 | 一致（PASS） |
| 4 | Operation / crash after prepare, before activate | typed `OperationDispatchNotActivated`（命名 operation_id）、零终结行（门前置 guard，零 Task 变更）；重启后 activate → 收敛唯一终态 | 一致（PASS） |
| 5 | Artifact / crash before any artifact drive | **无 artifact 门**：终结照常提交（诚实钉死）；声明写未发布（owner head 仍空、Task 零 publication 行）；重放只信 Task 行 | 一致（PASS） |
| 6 | Artifact / crash after plan READY | 终结提交；artifact plan 保持 READY、发布行持久、`task_receipt_id=None`（未链接）；重放逐字节相等 | 一致（PASS） |
| 7 | Channel / crash after seal+permit | endpoint proof（durable 注册行）即全部 owner evidence；重启驱动其余五域 → 收敛唯一终态；重放只信 Task 行（终结参数不含 Channel） | 一致（PASS） |
| 8 | 终结提交点 Phase A：`PowerLossAfter{0}` | finalize"报告成功"但重开后终结事务整体不可见（permit Issued、receipts/嵌套零行；semantic publication 行为 plan-READY 前置持久状态，恰 1 行）——非部分可见；同请求重做 → Committed 与幻影 receipt 逐字节相等（确定性 receipt id） | 一致（PASS） |
| 9 | 终结提交点 Phase B：kill-9 after commit | 完全可见（permit Closed、head 1、四表恰一套）；空 owner 双重放逐字节稳定、零重复行 | 一致（PASS） |

按 Ruling R3（SDD 台账）本矩阵为对已落地机械的验证性测试（非 TDD 违例），行 5/6 同时是 §3 边界 1 的缺陷面登记。

## 5. Evidence（命令与结果）

工具链：`rustc 1.97.1 (8bab26f4f 2026-07-14)` / `cargo 1.97.1`，macOS/arm64 本地实跑（worktree `llmos-w30-a`，分支 `feat/w30-a`，base `375000f` 工作区 clean 起步，已验证）。

- 新增测试单列：`cargo test -p nlos-task --test six_domain_write_set_closure` → **3/3**；`cargo test -p nlos-task --test six_domain_fault_matrix` → **9/9**（1 helper + 8 矩阵行），复跑 2 次稳定（并行与 `--test-threads=1` 各一次，FAULT_LOCK 串行下零时序抖动）。
- 零回归：`cargo test -p nlos-task` → **48 test targets / 367 passed / 0 failed / 2 ignored**（对基线 `375000f` 355 passed 恰 +12 = 3+9 新测试；`mixed_semantic_resource_commit` 7/7、`resource_bridge_fault_injection` 14/14、`operation_finalize_gate` 8/8、`combined_authority_seal`、`channel_endpoint`、`unified_task_commit_receipt` 4/4 逐一照绿）。
- 依赖面：`cargo test -p nlos-commit-coordinator` → **12 test targets / 58 passed / 0 failed**（写集外只读消费，零回归；`unified_worker_tri_domain` 7/7 照绿）。
- `cargo clippy -p nlos-task -p nlos-commit-coordinator --all-targets --all-features -- -D warnings` → **exit 0 / 0 error**（CI 教训口径：--all-features）；`cargo fmt -p nlos-task -p nlos-commit-coordinator -- --check` 通过；`git diff --check` 通过。
- 新代码 0 `unsafe`、0 生产改动（纯测试文件）；无 `chunks_exact(N).map()` 模式（CI clippy 1.98 lint）；fixture `unwrap/expect` 沿本 crate 测试先例。
- LSP 诊断：daemon 请求超时（与 W26/W27/W28/W29 各增量同状）；以 `cargo check` + `clippy -D warnings` 替代并记录，二者均通过。

## 6. 已知限制与 deferred minors（如实登记）

- **Artifact 混合终结门缺失**（§3.1）与 **Artifact+Semantic/Resource 组合回执变体缺失**：候选后续须先有 spec/ADR 决定（统一回执 digest 公式文档钉死「变体集扩展必须同步公式」，属冻结面变更）。
- 混合写集的 Task 侧恢复仍由 semantic plan/envelope 机制拥有（W28-C §6 裁定）：本矩阵的收敛是 caller-redo + 幂等重放语义（W/WE 同口径），worker 驱动的混合终结（`semantic_cycle` 对 mixed write set 走 semantic-only rung）不在本车道写集。
- 混合写集终结后残留的 READY artifact plan（§4 行 6 形状）会被 worker `artifact_cycle` 扫描并在 `PermitNotIssued` 上按 plan 级失败入台账退避——运维面可 inspect（`nlos_artifact_recovery_*`），但终态化路径不存在；与 §3.1 同根因。
- Effect 驱动走 legacy effect-permit 铸币路径（`[B-OP-FENCE-003]` opt-in 门未启用），Operation slot 的激活复核由 finalize 门兜底（`operation_finalize_gate.rs` 既有语义，本矩阵复用该形状）。
- 两测试文件 fixture 按仓例各自成套（~1.1k 行/文件），与 W 系列矩阵同款双份维护债。
- 单机 strict reference profile；kill-9 = 进程崩溃（页缓存存活），落盘丢失由 `PowerLossAfter` 建模，不外推真实断电与跨 Cell（沿 W1–W6 disclaimer）。

## 7. 未运行项（显式列出）

- **push / PR / 三平台 CI / MSRV / Pages：未执行**——按波次屏障由控制器统一收尾（MUST NOT 边界）。
- **`cargo test --workspace`：未整跑**——车道纪律限定定向门（nlos-task + nlos-commit-coordinator；零生产改动，其余 crate 编译面不受影响）。
- **stage-b-progress.md / 证据索引 / ADR 勾稽：未更新**——不在本车道写集（沿 W28-C §7 先例，由 W30-E 或控制器屏障统一执行）。
- nlos-system-control / nlos-resource / nlos-artifact 等只读消费 crate 未跑定向测试（零触碰）。
