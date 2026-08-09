# B-TASK-006N：dual-authority VFS / process crash fault matrix

> 状态：`PARTIAL PASS`　　日期：2026-08-09
>
> 对应：`B-TASK-006E`、`B-TASK-006F`、`B-TASK-006H`

## 已验证矩阵

| 故障点 | 注入方式 | 故障时权威前缀 | 修复/重启后结果 |
|---|---|---|---|
| Artifact publication metadata write | ArtifactAuthority 单独通过真实 `nlos-store-fault` SQLite VFS 注入 `SQLITE_IOERR` | 返回 typed `Artifact` failure；Artifact head、Task head 和 nested Receipt 均不推进 | disarm、重开后同一 plan 收敛并只产生一个 terminal Task receipt |
| Task nested Receipt write | TaskAuthority 单独通过 VFS 注入 `SQLITE_FULL`；ArtifactAuthority 使用正常 VFS | Artifact head 已发布；返回 typed `Task` failure；Task plan 保持 `PUBLISHING`、Task head 不推进 | 重放 Artifact immutable receipt，随后 plan `FINALIZED` |
| Task nested Receipt silent loss | TaskAuthority VFS `PowerLossAfter { 0 }` | 当前连接得到表面 `READY`，但连接死亡/重开后幻影 Receipt 不存在，durable plan 仍为 `PUBLISHING`；Artifact head 是唯一已提交前缀 | coordinator 从 Artifact receipt 重做 Task 记账并 finalize |
| Artifact 已发布、Task 尚未记账时进程死亡 | 独立 test process 发布 Artifact 后用 pipe `READY` 同步，父进程强制终止并 reap | 重开可见 Artifact revision 1；Task plan `PUBLISHING`、nested Receipt 空、Task head 0 | 新 TaskAuthority-owned worker 的 startup scan 收敛到 `FINALIZED`、Task head 1 |

以上测试没有用 sleep 猜测崩溃窗口；子进程通过 pipe marker 确认 durable prefix 后才被强制终止。Task/Artifact 两侧只把目标 authority 连接到 fault VFS，另一侧保持默认 VFS，因此错误来源和已提交前缀可区分。

## 验证与边界

`nlos-commit-coordinator` 的 `restart_convergence` integration test 从 6 项增至 11 项；定向测试和 `clippy -D warnings` 通过。

本证据是单节点本地 H3 / `PARTIAL PASS`：它证明 SQLite hard I/O、ENOSPC、Task 侧静默丢写和真实进程死亡后的前缀恢复，不等同真实硬件掉电、跨机器原子事务或三平台验证。ArtifactAuthority 若在介质静默丢写后仍返回成功，coordinator 无法独立证明该 authority 的物理 durability；协议仍依赖各 authority 的 durability contract，不把 nested Receipt 外推为对撒谎/失效存储介质的证明。
