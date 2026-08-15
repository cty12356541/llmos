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
- application-visible 幂等身份按 `(ApplicationId, service, method, IdempotencyKey)` 隔离；首次 claim 必须与 Operation 注册同事务，terminal result 必须与 Operation/Receipt/Outbox 同事务且完成后不可变；
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

v1 只允许从空数据库事务创建；遇到未知 `user_version` 直接拒绝打开。v2 新增按 `(operation_id, operation_generation, sequence)` 的 Outbox 恢复索引；v3 新增 scoped durable dedup/result authority。迁移均为事务化前向 migration，并已有升级前备份/恢复演练和 v1 golden database。若 PoC 失败，`nlos-operation` 状态机、Outbox 和 dedup/result 契约保留，可替换底层 WAL/KV；不得让 SQLite row identity 进入公共 Operation handle。

## 当前证据

[PoC-0003](../../evidence/stage-b/poc-0003-sqlite-operation-authority.md)已验证重开恢复、幂等、callback identity、cancel/complete 路由、Outbox ACK 以及 durable ACK 后的无析构进程退出恢复。F1–F4 通过 fault/recovery，F5 完成 v1→v2 migration，F6 完成 100K metadata ScaleProfile，F7 在 Ubuntu/Windows/macOS CI 通过核心测试与 Clippy。`B-STORE-FAULT` 验收范围已完成；本 ADR 仍保持 `POC`，因为真实硬件掉电、更多文件系统、100K 逐条生产写入和完整 Task/Artifact 负载不在当前证据内。

[PoC-0004](../../evidence/stage-b/poc-0004-outbox-wake-consumer.md)（2026-08-01）已补齐 Tokio wake consumer 集成缺口；其核心 workspace 回归随 F7 在 Ubuntu/Windows/macOS 通过。durable wait registry/fiber rehydration、真实副作用授权与协调仍归后续工作包。

[B-SCHEMA-010](../../evidence/stage-b/b-schema-010-durable-idempotency-result.md)（2026-08-02）新增 schema v3 与原子 same-key claim/result replay：相同 key/digest 在重开后返回原 Operation 或稳定 service result，不同 digest 冲突，处理中断不重新授予 dispatch。[三平台 run 30738888761](https://github.com/cty12356541/llmos/actions/runs/30738888761) 已成功。

[B-SCHEMA-011](../../evidence/stage-b/b-schema-011-durable-idempotency-ipc.md) 已把 authority 接入 Rust↔TS/Python 两跳 IPC，并验证 commit 后/response 前断线、原 key 重连回放、conflict 和 `E_UNCERTAIN`；[三平台 run 30740180511](https://github.com/cty12356541/llmos/actions/runs/30740180511) 已成功。后续增量又把 commit/recovery 拆为两个独立服务进程，恢复进程重新协商目录、重开 SQLite 并以零次新 dispatch 回放原结果；该组合由[三平台 run 30741046472](https://github.com/cty12356541/llmos/actions/runs/30741046472) 验证。retention/GC 和完整 deadline/cancel 生产链仍待完成。

[B-SCHEMA-012](../../evidence/stage-b/b-schema-012-deadline-cancel-state-machine.md) 新增 dispatch 前 no-effect 的 idempotent 原子完成路径，并把 deadline/cancel 的 pre-dispatch、post-dispatch partial/effect-unknown 转换接入真实 Rust↔TS/Python IPC；[三平台 run 30741733804](https://github.com/cty12356541/llmos/actions/runs/30741733804) 已成功。

[B-SCHEMA-013](../../evidence/stage-b/b-schema-013-operation-control-timer-worker.md) 新增 generation/expected cancel epoch CAS：首次取消只推进一次，精确重试不重复产生 Outbox，completion 先赢时终态不可改写；独立 OperationControl payload 与 Tokio timer worker 已通过[三平台 run 30743421174](https://github.com/cty12356541/llmos/actions/runs/30743421174) 的 Rust↔TS/Python IPC。持久 deadline queue/restart recovery 与 retention/GC 仍待完成。

[B-OP-FENCE-002](../../evidence/stage-b/b-op-fence-002-operation-endpoint-proof.md)（2026-08-15）新增 `SqliteOperationStore::inspect_endpoint_proof`：owner 按 `OperationId + Generation` 回读 durable registration row 后，派生 owner-bound participant/receipt proof，并在 authority 重启后保持逐字段稳定；旧 generation 在 proof 生成前拒绝。该切片仍不等于 Operation prepare→activate、跨进程签名/租约/attestation、Task participant registry 接线或 Channel endpoint。
