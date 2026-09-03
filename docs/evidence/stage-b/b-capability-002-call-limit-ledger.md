# B-CAPABILITY-002：Call-limit 消耗账本（最小前缀）

> 状态：`PASS`（最小前缀；已知限制见 §5）
>
> 日期：2026-09-01
>
> 基线：HEAD `498161b`；前序证据 `b-capability-001`（§5，commit `2ccc694`/`563aa4c`，ADR-0010）

## 1. 验收目标

关闭 B-CAPABILITY 未决项「call-limit 消耗账本」的最小前缀：把 ADR-0010 签名命令面的
`call_limit` 从纯声明/衰减字段变成**执行的 admission 面**——limited capability
（`call_limit = N`）的每次行使消耗计数，额度耗尽产生 typed 拒绝（零部分状态），
且消耗经幂等键重放不双扣（镜像 issue/revocation receipt 先例）。

不在本前缀范围内：delegate 限额池化分摊、跨进程认证入口、退款/重置策略、
Namespace hierarchy。

## 2. 消耗模型论证：immutable 消耗行 vs 原子计数行

**选定：durable immutable 消耗行（append-only ledger），剩余额度由 COUNT 派生。**

理由：

1. **crate 先例一致**：`capability_versions`、`capability_issue_receipts`、
   `capability_revocation_receipts` 全部是 immutable append-only 表 + DDL trigger 防
   UPDATE/DELETE；唯一可变列是 `capability_heads.current_generation` 的 CAS。可变计数行
   需要引入第二种可变状态面（新的 UPDATE 权限 + 防回退不变量），与既有权威模型相悖。
2. **幂等回执照 receipt 先例免费获得**：`idempotency_key` 作 PRIMARY KEY，同 key 同
   request digest 重放返回既有行（不双扣），同 key 异 digest fail-closed
   （`IdempotencyConflict`）——与 issue/revocation receipts 的回执语义逐字对齐。
3. **重启安全无漂移**：剩余额度从行数派生，不存在独立 mutable counter 的崩溃窗口
   （counter 与 ledger 行的双写一致性问题被整表消除）。
4. **并发线性化**：单写者 `Mutex<Connection>` + `BEGIN IMMEDIATE` 事务内
   COUNT→检查→INSERT 原子完成；进程内并发消耗按 mutex 顺序串行化，实测断言通过。
5. **代价**：每次行使 O(n) COUNT（有 capability_id 索引）。最小前缀可接受；
   计数缓存列为后续优化项（见 §5）。

## 3. 实现事实

`crates/nlos-capability` schema **v1→v2 纯增量迁移**（`migrate_v2`；新开库
0→v1→v2，既有 v1 库打开时自动升 v2，v2 直接通过；不触碰既有表/触发器）：

- 新表 `capability_consumption_rows`：`idempotency_key`（PK）、`request_digest`、
  `receipt_id`（UNIQUE）、`capability_id`+`generation`（FK→`capability_versions`，
  记录消耗被 admit 的代次）、`remaining`（NULL=unlimited）、`consumed_at_ms`；
  STRICT + immutability UPDATE/DELETE trigger + `capability_id` 索引；
- 新 API：
  - `CapabilityAuthority::consume(ConsumeCapabilityRequest)` — 行使入口。门序：
    幂等回放（回放不重跑 admission，回放即 durable authority）→ 复用与
    `authorize_semantic` 完全相同的 admission 门（抽取为共享 `admit_semantic`：
    exact current generation、祖先链 active、holder/target/right/purpose）→
    COUNT 检查：`used >= limit` 时 typed 拒绝 `CallLimitExhausted`（拒绝发生在任何
    durable 写之前，事务整体丢弃，**零部分状态**）→ 插入一行消耗行（含
    `remaining = limit - used - 1`），返回 `CapabilityConsumptionDecision::
    Consumed|Replayed(CapabilityConsumptionReceipt)`；
  - `CapabilityAuthority::call_limit_remaining(handle)` — 剩余额度回读；
    limited 返回 `Some(剩余)`，unlimited 返回 `None`；纯读回，不评估 admission 态；
  - request digest 域分隔 `nlos/capability-consume-request/v1`，覆盖 handle、
    verified signer principal、全部 admission 字段、幂等键与 authority time；
    receipt id 派生域 `nlos/capability-consumption-receipt/id/v1`；
- `authorize_semantic` 保持纯 admission check，**不消耗**额度（行为不变，仅内部
  抽取共享门函数）；消耗只发生在 `consume`；
- 新 typed 错误 `CapabilityAuthorityError::CallLimitExhausted`；
- **delegate 分摊语义**（按已落地语义最小化）：衰减链只证明 child 的声明 limit
  ≤ parent（`call_limit_is_attenuated`），无池化语义落地。故每个 capability
  （根或 delegate 子）持有**独立预算**：消耗行按 `capability_id` 分别计数，子消耗
  不扣父、父消耗不扣子；revoke 换代不重置预算（quota 按 capability 而非代次）。
  池化/联合预算登记为后续议题（§5）。

## 4. 验证证据

新增 `crates/nlos-capability/tests/call_limit_ledger.rs` 7 项 integration tests：

1. 递减/回读一致 + 耗尽 typed 拒绝 + 拒绝后行数不变（零部分状态）；
2. 同 key 同 digest 重放免计费回执相等、异 digest `IdempotencyConflict` fail-closed、
   **跨重启**剩余额度与重放均持久；
3. 8 线程并发消耗 limit=3：恰 3 消费/5 `CallLimitExhausted`，消费回执
   `remaining` 集合恰为 {2,1,0}（单写者串行化下的线性化断言），行数=3；
4. unlimited（`call_limit=None`）：`remaining=None`、多次行使永不耗尽；
5. delegate 独立预算：child limit=5 耗尽后父仍 `Some(10)`、父消耗不影响 child、
   消耗行按 capability 分表隔离；
6. 消耗复用 semantic admission 门：holder/purpose/validity 违反被 typed 拒绝且
   不产生任何消耗行；
7. 消耗行 DDL trigger 防 UPDATE/DELETE（ledger immutability）。

本地验收命令与结果（全部实跑）：

| 命令 | 结果 |
| --- | --- |
| `cargo test -p nlos-capability` | PASS：25 passed / 0 failed（ledger 7 + authority 8 + signed_commands 10；doc-tests 0） |
| `cargo clippy -p nlos-capability --all-targets -- -D warnings` | PASS（0 warning） |
| `cargo +nightly-2026-08-01 clippy -p nlos-capability --all-targets -- -D warnings` | PASS（0 warning） |
| `cargo fmt -p nlos-capability -- --check` | PASS（clean） |
| `cargo +nightly-2026-08-01 fmt -p nlos-capability -- --check` | PASS（clean） |

写集：`crates/nlos-capability/src/{lib,model,schema}.rs`、
`crates/nlos-capability/tests/call_limit_ledger.rs`、本证据文件。未运行
`--workspace` 门；工作区中 nlos-artifact/nlos-topic 脏改动属并行车道，未触碰。

## 5. 已知限制

- **delegate 分摊**：独立预算语义（§3），无跨 capability 池化/联合上限；委托出的
  限额只受衰减门约束，父额度可被子额度总和超出；
- **跨进程**：单进程内单写者 Mutex 串行化；多进程并发消耗依赖 SQLite 文件锁但未测
  （`BEGIN IMMEDIATE` + busy_timeout 提供基础，无实测断言）；
- **无退款/重置策略**：消耗行不可撤销；revoke 后换代不重置预算，也不存在
  refund/reset API；
- **O(n) COUNT**：每次行使全量计数（有索引），大量消耗后可加计数缓存列（需 v3 迁移）；
- 无 kill-9/torn-write/ENOSPC 故障矩阵、无三平台 CI；不得外推为分布式配额或
  硬件掉电保证。

## 6. B-CAPABILITY 未决项状态增量

- 「call-limit 消耗账本」：**最小前缀已关闭**（本证据）；剩余扩展项见 §5；
- Namespace hierarchy narrowing：未变，仍开放；
- 跨进程认证入口：未变，仍开放（blocked-by B-TASK-006L）；
- AuthorityClock：未变，仍开放（消耗时间仍用上层传入 authority time）。
