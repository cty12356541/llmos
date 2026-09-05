# B-SEMANTIC-009：Admission kill-window 故障矩阵最小前缀

> 状态：`PARTIAL_PASS`
>
> 日期：2026-09-05
>
> 范围：`LOCAL_SINGLE_NODE` Assertion/Judgment durable admission 写路径 fault VFS 最小前缀（lane W15-S）

## 1. 验收对象

在 `nlos-semantic` 新增 `tests/admission_fault_injection.rs`，经 `nlos-store-fault` 注入覆盖 Assertion/Judgment admission 单事务写路径的四类 kill-window：

| 窗口 | 注入 | Assertion | Judgment |
|---|---|---|---|
| W1 | pre-commit `IOERR` | typed `Sqlite` fail-closed、零幻影行、disarm 后同请求收敛 | 同上（两 Assertion 端点已 durable 前缀） |
| W2 | pre-commit ENOSPC (`SQLITE_FULL`) | 同上 | 同上 |
| W3 | commit 点 `PowerLossAfter` 不可见方向 | 幻影 admission 重开后整体缺席；删库/WAL sidecar 恢复后同 key redo 与幻影 byte-equal | WAL sidecar 丢弃后前缀两 Assertion 仍在；Judgment 幻影缺席、redo 收敛 |
| W4 | replay storm | 同请求 3+ 次 + reopen 后 `Replayed` 逐字节幂等 | 同上 |

仅 Semantic authority 连接经 `SemanticAuthority::open_with_vfs` 路由 fault VFS；Identity/Process/Capability 保持 plain VFS。

## 2. 实现事实

- 零 `src/` 改动；`Cargo.toml` 增加 test-only `nlos-store-fault` dev-dependency。
- 行计数覆盖 admission 事务触及的六表：`content_objects`、`semantic_events`、`event_signatures`、`event_log`、`admission_receipts`、`semantic_outbox`（Judgment 亦写 outbox，经 `seal_admission`）。
- 每场景 `PRAGMA integrity_check = ok`；进程内 `FAULT_LOCK` 串行化 fault 全局状态。

## 3. 验证

```text
cargo test -p nlos-semantic
cargo clippy -p nlos-semantic --all-targets -- -D warnings
cargo fmt -p nlos-semantic --check
```

结果：34 项 `nlos-semantic` 测试全部通过（含新增 4 项 fault matrix）；Clippy/fmt 零警告。

## 4. 证据等级与已知限制

结论：`PARTIAL_PASS / H3 local durable authority（故障模型）`。

- 仅为本地 SQLite/VFS 故障模型；不等同真实硬件掉电、kill-9 可见方向或 torn-WAL 33 点 sweep。
- SpecEvent、Verification、Retraction、declassification issue、outbox ack/publication 写路径未纳入本最小前缀。
- PowerLoss 不可见方向在 Assertion 空库场景需丢弃 fault 会话后的 semantic 存储 artifact 再 reopen（建模进程死亡 + 无 durable prefix）；Judgment 场景仅丢弃 WAL sidecar，保留已提交 Assertion 前缀。
- 未发现新缺陷；矩阵验证既有 admission 原子性/幂等收敛契约。

## 5. 未运行项

- workspace 全仓门、三平台 CI、并发多写者压测。
- Trust View / Gate / batch DAG 故障矩阵（不在 W15-S 写集）。
