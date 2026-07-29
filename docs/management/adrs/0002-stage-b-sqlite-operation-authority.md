# ADR-0002：阶段 B Operation 本地持久化权威

- 状态：POC
- 日期：2026-07-29
- Owner：待指定
- 关联 Requirement：`MODEL-OP-001`、`IO-ASYNC-001`、`IO-CANCEL-001`、`DUR-ACK-001`、`DUR-RECOVER-001`、`SLICE-K-005`
- 复审触发器：kill/torn-write/disk-full 测试失败；单写者成为目标工作集瓶颈；SQLite 无法提供所需跨平台 durability；Operation 与 TaskControlRecord 需要同事务但被拆到不同权威服务

## 上下文

阶段 B 已有内存内 Operation callback fence，但 Process 退出会同时丢失 Operation 终态、迟到副作用证据和 Fiber 唤醒决定。若先唤醒 Fiber 再持久化，崩溃可能使执行越过尚未成为权威事实的结果；若先持久化再直接唤醒，提交后崩溃又可能永久丢失唤醒。因此状态转换与待投递事件必须位于同一 durable transaction。

## 候选

| 候选 | 优点 | 主要风险 |
|---|---|---|
| SQLite WAL + 单写者 authority | 单机成熟、事务边界小、恢复和备份工具完整 | 写吞吐受单点约束；WAL/checkpoint/文件系统语义必须实测 |
| 独立 append WAL + snapshot | 可完全控制格式、校验和 group commit | torn-write、恢复、迁移和工具链的实现风险显著更高 |
| RocksDB/其他嵌入 KV | 写吞吐与 LSM 能力较强 | 事务模型、跨平台打包和恢复复杂度高于当前切片 |
| PostgreSQL | 服务化事务与运维能力成熟 | 阶段 B 本机安装、升级和恢复负担过大 |

## 当前决定

采用 **SQLite WAL + `synchronous=FULL` + 单写者 Operation authority PoC**，Rust 适配使用 `rusqlite` 的 bundled SQLite。

每次 Operation 转换使用 `BEGIN IMMEDIATE`，并通过同一事务提交：

```text
Operation generation/revision CAS
  + cancel epoch
  + dispatched callback identity
  + terminal Receipt identity
  + WakeFiber | ReconcileEffect outbox
```

调用者只有在事务 commit 成功后才收到成功。Outbox consumer 在幂等应用事件后单独 ACK；因此允许“应用成功、ACK 前崩溃”造成重复投递，但不允许已提交事件永久丢失。

## 约束

- `nlos-operation` 是唯一状态转换语义；SQLite 层只负责恢复、CAS、事务与编码；
- OperationId 重放只有在完整 `OperationSpec` 相同时才幂等成功，不同内容复用同一 ID 必须拒绝；
- dispatch 时生成的 CallbackId 与 CancelEpoch 必须持久绑定，回调不得替换票据；
- 所有整数 epoch/generation 以固定 8-byte big-endian BLOB 保存，避免 SQLite signed integer 缩小 `u64` 域；
- schema 使用 `STRICT` table、长度约束、显式 `user_version` 和未知版本 fail-closed；
- SQLite schema 是内部 durable format，不是 KABI/SABI；冻结前可以通过显式 migration 演进；
- WAL、`-shm`、主数据库和 checkpoint 必须作为同一持久状态管理，不允许复制单个主文件冒充备份；
- 当前进程内 Mutex 是 writer admission gate，不能冒充跨主机共识或分布式权威。

## 依赖审查

截至 2026-07-29：

- 采用 `rusqlite 0.40.1`；其项目声明 `rusqlite` 与 `libsqlite3-sys` 使用 [MIT License](https://github.com/rusqlite/rusqlite)；
- bundled feature 编译 SQLite 源码，避免依赖宿主恰好安装的 ABI/version；代价是二进制体积、构建时间以及 SQLite 安全更新必须通过 NLOS 依赖升级重新发布；
- SQLite 官方声明核心交付代码属于 [Public Domain](https://www.sqlite.org/copyright.html)；
- `rusqlite` 是可替换 adapter，不进入稳定 KABI/SABI；Cargo.lock 固定实际供应链版本，升级必须重新运行 durability、migration 和跨平台测试。

## PoC 验收

1. 重开数据库后恢复 Operation 状态、callback fence 和未 ACK Outbox；
2. exact registration、callback 和 outbox ACK 可幂等重放；
3. cancel-before-dispatch 原子产生终态和 Fiber wake；
4. cancel-after-dispatch 的迟到 callback 只进入 reconciliation；
5. forged/stale callback 不改变状态且不产生 Outbox；
6. durable completion ACK 后进程无析构退出，重开仍可恢复状态与 Outbox；
7. 后续补齐 kill -9、commit 中断、disk-full、checkpoint/长读、100K metadata 与 migration 测试。

## 迁移与退出策略

v1 只允许从空数据库事务创建；遇到未知 `user_version` 直接拒绝打开。任何 v2 变更必须提供前向 migration、备份/恢复演练和 golden database。若 PoC 失败，`nlos-operation` 状态机和 Outbox 契约保留，可替换底层 WAL/KV；不得让 SQLite row identity 进入公共 Operation handle。

## 当前证据

[PoC-0003 初始证据](../../evidence/stage-b/poc-0003-sqlite-operation-authority.md)已验证重开恢复、幂等、callback identity、cancel/complete 路由、Outbox ACK 以及 durable ACK 后的无析构进程退出恢复。该结果仍是 `PARTIAL PASS`：尚未完成 torn-write VFS、disk-full、checkpoint/备份、迁移、100K metadata、跨平台和 Tokio wake consumer 集成，因此本 ADR 保持 `POC`。
