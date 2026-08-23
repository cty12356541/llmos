# B-TASK-006E：单机 cross-authority commit coordinator

> 状态：`PARTIAL PASS`
>
> 日期：2026-08-09
>
> 对应：`[TASK-TXN-001]` recoverable cross-authority commit、`[TASK-WRITE-001]` staged output、`[TASK-COMMIT-002]` nested receipts
>
> 实现：新 crate `nlos-commit-coordinator`，依赖 `nlos-task` 与 `nlos-artifact`

## 1. 本切片目标

把 B-TASK-006A–006D 与 B-ARTIFACT-002 串成可执行的单机恢复协议：coordinator 不保存第三份真相，只逐步驱动两个 authority 已持久化的状态；任一步后进程退出，重启都能从 durable prefix 继续，而不是猜测 Artifact 是否已经发布或 Task 是否已经 finalized。

## 2. 已实现事实

1. **独立薄适配层**：`nlos-commit-coordinator` 单向依赖 Task/Artifact 两个 crate，二者保持互不反向依赖；coordinator 本身无数据库、无独立状态机副本。
2. **一步一个 durable boundary**：`converge_one_step` 按 `PLANNED → authorize`、`PUBLISHING → publish/replay one + record one`、`READY → finalize`、`FINALIZED → replay` 推进，调用方可在任意一步后安全退出。
3. **全量收敛**：`converge` 循环执行 bounded step 直到返回完整 `ArtifactTaskCommitReceipt`；所有 authority 错误以 `CoordinatorError::Task/Artifact` 保持 typed source。
4. **启动扫描**：TaskAuthority 新增稳定排序、bounded 的 `list_incomplete_artifact_commit_plans`；`converge_pending` 可在进程启动后扫描并收敛未完成 plan，`FINALIZED` 不再出现在后续扫描。
5. **publish-before-record 恢复**：若 ArtifactAuthority 已提交 publication 但进程在 Task receipt 消费前退出，重启后的 publish 返回 immutable replay，coordinator 再补录同一 nested receipt，不重复推进 Artifact head。
6. **预先计算 staging identity**：`nlos-artifact::staging_id_for` 公开既有 domain-separated 确定性公式，使 CommitPermit/write-set plan 能在 stage bytes 前绑定未来 staging identity；stage 内部继续使用同一公式。

## 3. 验收测试

真实双 authority 测试使用独立 Task SQLite 与 Artifact root，覆盖两个 Artifact 的完整链路：

1. 创建 Artifact/Task/Attempt/Permit，按预计算 staging identity 形成 canonical plan 并 stage bytes；
2. `PLANNED` 授权后关闭并重开两个 authority；
3. 人工只提交第一个 Artifact publication，模拟 publish 后 / record 前崩溃；
4. 重启 coordinator，publish replay 后补录第一项，plan 保持 `PUBLISHING`；
5. 再次重启，发布第二项进入 `READY`；
6. 再次重启，通过 `converge_pending` finalize，验证两 Artifact head=1、TaskHead=1、nested receipts=2；
7. 对 `FINALIZED` 再调用 converge 返回逐位相同 receipt，后续 pending scan 为空。

## 4. 证据等级与限制

单节点局部 H3，`PARTIAL PASS`。

明确不声明：

- 两个 SQLite 文件拥有分布式原子事务；协议保证可重试收敛与真实 partial 可见性，不保证 Artifact publication 可回滚；
- coordinator 已被 Process supervisor/daemon 自动托管或拥有 lease/leader election；
- ArtifactAuthority 在线验证了 TaskAuthority 签名 token；当前调用顺序由同进程 coordinator 保证，binding 仍由 ArtifactAuthority 逐位校验；
- 已实现冲突的 compensation 或人工处置队列；
- 已覆盖 coordinator 的 kill-9/ENOSPC/I/O/torn-write 完整矩阵与三平台 CI；
- 混合 Artifact + Effect write set 已获支持。

## 5. 下一步

Artifact publication pending restart scan 已补齐两个 owner publication receipt 的重开消费；仍需继续覆盖更广的 coordinator fault matrix、Resource/Operation/Channel participant 与完整 TaskWriteSet，并把 pending scan 接入最小 Process supervisor 启动路径。online authorization token/签名与长期 coordinator authority 归属在进入真实 IPC 前冻结。

## 6. 增量 Evidence

### 6.1 Artifact publication pending restart scan（B-TASK-008C2G-COORD-ARTIFACT-RESTART-SCAN）

- 新增 `crates/nlos-commit-coordinator/tests/artifact_pending_restart_scan.rs`：建立一个包含两个 Artifact publication expectation 的 Task plan，分别 stage 并提交两个 Artifact owner publication receipt；确认 TaskAuthority 仍处于 `PUBLISHING` 且 Task-side publication consumer 为空后，丢弃并从同一 SQLite/root 重开两个 authority。
- 重开后只调用公开 `ArtifactCommitCoordinator::converge_pending(1, 6_000)`；coordinator 消费两个原始 owner receipt，返回一条嵌套 `TaskCommitReceipt`，两个 Artifact head 均为 revision 1，plan 为 `FINALIZED`。随后显式 `converge` 返回逐位相同 receipt，后续 pending scan 为空。
- `cargo test -p nlos-commit-coordinator --test artifact_pending_restart_scan -- --nocapture`：1 项通过；targeted Clippy、fmt、diff check 与 `cargo test --workspace --quiet` 通过。代码基线 `3d274ab` 的 Rust cross-platform/MSRV CI [32627556597](https://github.com/cty12356541/llmos/actions/runs/32627556597) 与 Pages [32627556621](https://github.com/cty12356541/llmos/actions/runs/32627556621) 均成功。
- 证据仍是单机双 authority bounded recovery：不声称分布式原子提交、真实掉电、Artifact publication 回滚、Resource/Operation/Channel 完整接线、外部 provider proof/attestation 或 complete TaskWriteSet。
