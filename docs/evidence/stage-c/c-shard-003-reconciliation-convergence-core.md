# C-SHARD-003 / W55-CS0C：reconciliation 收敛核（最小 executor）

- 状态：`PASS`（ADR-0019 R1.2.4「checkpoint 缺口的有界收敛」最小 executor：trail 核对 + durable prefix 缺口收敛 + ADR-0013 四不变量断言面；**非**完整 C-MIGRATE 执行面，**非**故障注入证据）
- 日期：2026-10-08
- Owner：`C-SHARD` 计划 Phase 0（[c-shard-plan.md](../../management/c-shard-plan.md) CS0-C 车道）
- 设计依据：[ADR-0019](../../management/adrs/0019-cross-cell-commit-semantics-extension.md) R1.2.3/R1.2.4；[ADR-0013](../../management/adrs/0013-cross-authority-verify-then-commit-contract.md) 决定 1（四不变量照搬：崩溃窗口收敛性、无幻影行、无双重提交、replay 逐字节幂等——不新增不放松）
- 分支：`feat/cs0c-reconcile-core`；lane commit `6b77a43c8`；base `d4e23db0e`；合入 main `bb0972118`（W55-CS0C）

## 1. 实现事实

- `crates/nlos-slice-k/src/reconciliation.rs`（913 行）：`converge_commit_prefix_gap` 纯函数 executor——
  1. term 前置守卫：active assignment 绑定核验，双主入口 fail-closed，先于一切 trail/prefix 读取；
  2. `durable_commit_prefix` 公共推导：`list_effect_history` 规范编码 + `compute_effect_history_root` 交叉核对 + head 事实；checkpoint 作者侧与核验侧共用同一语法，不可能漂移；
  3. trail 逐条经 C-SHARD-002 `verify_digest` 四态核对（Verified 计入覆盖；Mismatch/Corrupt/trail 文件损坏 fail-closed；FreeText 按 R1.2.3 归未覆盖）；
  4. 最长已验证锚点 → 缺口逐条 adopt（纯记账）→ 四不变量逐项计算，违反即类型化 fail-closed。
- executor 不取时钟、不铸身份（liveness 属调用方时序义务，模块文档声明）；单机 `converge_resource_commit_plan`/quarantine 生命周期原位不触碰（复用非重复）。不新增 epoch 入口。

## 2. 验证（2026-10-08，macOS arm64 darwin 25.6.0）

```text
cargo test -p nlos-slice-k            → 92 passed / 0 failed / 4 ignored（含本车道 child 入口）
cargo fmt --check（全仓）              → 退出码 0
cargo clippy -p nlos-slice-k --all-targets → 退出码 0
integrator 全仓（merge 后）：fmt=0 / clippy --workspace --all-targets=0 / test --workspace 1915/0/31（347 二进制）
```

证据测试 `crates/nlos-slice-k/tests/dual_cell_reconciliation.rs`（1465 行，8 测试 + 1 child 入口，双层证据）五类场景逐条具名：

1. 缺口覆盖：`gap_converges_from_verified_anchor_and_replay_is_byte_identical`（trail 落后 durable 前缀，收敛 adopt 恰为缺口）+ 双进程主测试 Stage 3；
2. 幂等重放：两次收敛结构相等 + replay_digest 逐字节相等 + 双进程 Stage 4；
3. 双 prefix 一致终态：`anchor_advance_adopts_nothing_and_keeps_the_final_state`、`no_anchor_entry_reaches_the_same_final_state` + 双进程 Stage 5；
4. digest 篡改/损坏 fail-closed：`tampered_digest_claim_fails_closed`、`corrupt_digest_body_fails_closed`；
5. 自由文本归 PARTIAL/UNCERTAIN：`free_text_checkpoints_cover_nothing`（两态 + 终态不变）。
- 附：`stale_term_lease_is_refused` + 双进程 Stage 6（存活旧 Cell 以 term-1 收敛被 `NotTermHolder` 拒绝）——**此为入口守卫证据，非分区/双主注入证据**（通信未阻断）。
- `ConvergenceInvariants{crash_window_converged, no_phantom_rows, no_double_commit, replay_digest}` 三布尔为计算事实，`assert_invariants_all_hold` 在每个收敛场景断言。

## 3. 边界与限制（诚实域）

- **单机双进程**证据；非跨机/真分区/跨时钟域（RISK-B-11 纪律，文件头声明在案）。
- endpoint 覆盖仍仅 TaskStore（同 C-SHARD-001）；executor 校验 term 绑定不校验存活。
- 故障形态下的收敛行为（重启 mid-chain 等）由 C-SHARD-004 注入后断言本核。

## 4. 复现

```text
cargo test -p nlos-slice-k --test dual_cell_reconciliation -- --nocapture
```
