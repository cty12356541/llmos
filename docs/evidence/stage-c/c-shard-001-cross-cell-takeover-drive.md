# C-SHARD-001 / W54-CS0A：跨 Cell 接管驱动器 + 双进程 PoC（成功路径）

- 状态：`PASS`（ADR-0019 修订 1 R1.2.2「接管执行唯一通道」在 ADR-0018 单机双进程拓扑上的成功路径证据；**非**故障注入证据（C-SHARD-004），**非**跨机）
- 日期：2026-10-08
- Owner：`C-SHARD` 计划 Phase 0（[c-shard-plan.md](../../management/c-shard-plan.md) CS0-A 车道；登记名「ADR-0019 VERIFIED 证据面」，非 C-SHARD 本体切片）
- 设计依据：[ADR-0019](../../management/adrs/0019-cross-cell-commit-semantics-extension.md) R1.2.2（DIST-TASK-002 CAS 链 + DIST-TASK-004 baseline）+ 修订 2 R2.1（Phase 0 证据车道豁免）
- 分支：`feat/cs0a-takeover-drive`；lane commit `a8966b248`；base `396ab30e9`；合入 main `84d511d39`（W54 波）

## 1. 实现事实

- `crates/nlos-slice-k/src/takeover_drive.rs`（539 行）：传输中立编排器，只编排既有 nlos-task 本地单事务门（`prepare_authority_takeover_fence` store.rs:2101 → 逐 manifest 成员 `record_authority_takeover_barrier_receipt_signed` :2409 → `complete_authority_takeover` :2548 → successor registry reopen），每腿 fail-closed readback；不新增任何 store 语义。
- **传输形状（Phase 0 选择，模块文档在案）**：successor 进程直接打开旧权威 task store（ADR-0018 共享 FS，SQLite WAL 多进程访问）；barrier observation 经 `BarrierObservationSource` 缝由旧 Cell endpoint 自铸 receipt id/digest 并 Ed25519 签名，successor 经 store 签名门提交（与 nlos-takeover-control SABI handler 同一验证代码路径）。生产传输裁定留 CS1-D。
- **不碰 cell epoch**：走 TaskAuthority lease/term/fencing 机器（v27 族），quota 族 `advance_epoch_and_quarantine` 仍是唯一推进入口（`cell_host.rs:38-44` 测试 pin 未动）。
- R4 核验结论：三门与全部类型本就是 nlos-task crate-root `pub`——nlos-task 零改动、零新增依赖边。

## 2. 验证（2026-10-08，macOS arm64 darwin 25.6.0）

```text
cargo test -p nlos-slice-k --test dual_cell_takeover   → 1 passed / 1 ignored（child 入口）+ 3 次复跑 + --include-ignored 批跑全绿
cargo test -p nlos-slice-k                              → 84 passed / 0 failed（25 测试二进制）
cargo fmt --check                                       → 退出码 0
cargo clippy -p nlos-slice-k --all-targets              → 退出码 0，零警告
integrator 全仓（merge 后）：fmt=0 / clippy --workspace --all-targets=0 / test --workspace 1907/0/30（346 二进制）
```

证据测试 `crates/nlos-slice-k/tests/dual_cell_takeover.rs`（1096 行，双进程：child=旧权威 Cell A，parent=successor Cell B）断言面：

1. v27–v38 表族全链落痕：lease/term 历史（term1 Acquired→term2 TakenOver）；`FROZEN_FOR_TAKEOVER` fence receipt + exact/outstanding 双 root；pending receipt 绑旧 assignment；逐 endpoint 签名 observation（v35 digest 列 + v36 signer 列，endpoint 自铸 id/digest 逐字节比对，signer 三元组与旧 Cell bootstrap 绑定一致）；exact-fence manifest；receipt `Pending→Complete`；assignment 状态族三行（旧/completion-successor/active = Fenced/Fenced/Active）；successor registry generation+1、prior_root 链回冻结 root、冻结参与者集合完整存活（DIST-TASK-004）。
2. 旧 Cell fence 后**仍存活**以 stale term-1 lease 做 prepare 与 commit-permit 提交，双双 `TaskStoreError::AuthorityLeaseFenced`（child 侧断言 + 回执行父侧复核）。

## 3. 边界与限制（诚实域）

- **单机双进程**证据（共享 FS/内核/时钟域，ADR-0018 拓扑）；非跨机、非真网络分区、非跨时钟域——引证为跨机证据即违 RISK-B-11。测试文件头边界声明在案。
- endpoint 覆盖逐个声明：**仅 TaskStore** participant endpoint；六类（TASK_STORE/ARTIFACT_HEAD/SEMANTIC_ADMISSION/CHANNEL_TOPIC/DRIVER_GATEWAY/RESOURCE_LEDGER）映射属 Phase 1 CS1-C。
- 成功路径 only：故障注入（分区/重启/迁移故障/重复/stale/双主）属 C-SHARD-004。
- 驱动器为 one-shot（每 receipt 一次执行）；重放语义由 store 各门 CAS 保证（nlos-task 既有测试），本证据未重复断言。

## 4. 复现

```text
cargo test -p nlos-slice-k --test dual_cell_takeover -- --nocapture
```
