# exp7 → Blackboard 集成建议（议题 6/15 × 议题 21-23）

> 问题：语义原子存储如何与 Blackboard（内核元数据 + 用户态字节）对接？
> 前提：议题 6 定案 Blackboard 是"字节层共享状态"；议题 21 3.3 已定"Blackboard 的原子存储成为共享状态的语义层实体（字节层之上）"。本实验给出数据面形态后，对接路径如下。

## 1. 分层：Blackboard = 字节层，AtomStore = 语义层索引

```
┌─────────────────────────────────────────────┐
│ 语义层（本实验）：AtomStore                  │
│   原子信封 / link 图 / verdict 流 / spec 表  │
├─────────────────────────────────────────────┤
│ 字节层（议题 6）：Blackboard                 │
│   内核元数据（namespace/ACL/版本）+ 用户态字节│
└─────────────────────────────────────────────┘
```

- **content 的可选外置**：小 content 直接入 atoms 表（本实验形态）；大 content（长报告/代码产物）存 Blackboard 字节对象，atoms.content 存 `bb://<namespace>/<key>` 引用——**引用是字符串不是解析**，防火墙不破（存储层依然永不读 content，测试 `test_content_never_parsed` 的静态规则对外置形态同样成立）。
- **内核只管信封**：Blackboard 的 namespace/ACL 机制直接复用到原子——原子写入请求带 namespace，读取按 namespace 过滤；原子 id（内容哈希）天然适合跨 namespace 去重。

## 2. 合并语义：修订 6 三条规则已有存储层支撑

议题 26 修订 6（最终一致档限 CRDT 式 merge）在本原型上的映射：

| 修订 6 规则 | 本原型对应机制 | 状态 |
|---|---|---|
| tombstone 优先（撤回战胜内容，防复活） | `get()` 先查 tombstone 再返回；撤回后写 verified 仍隐藏 | ✅ 已锁定（test_tombstone） |
| 原子本体不可变，无合并需求 | 触发器物理拒绝 UPDATE/DELETE | ✅ 已锁定（test_immutability） |
| link 不自动合并，矛盾共存 + CONTRADICTS 显式化 | link id 含 relation，矛盾判断是不同原子 | ✅ 已锁定（test_links） |

**对接建议**：分布式 Blackboard（议题 15）的 merge driver 输入 = 两个 AtomStore 的原子集并集（按 id 去重即完成 merge——内容哈希幂等的红利）；冲突检测 = `links_between(a, b)` 返回多个矛盾 relation 时产出 CONTRADICTS link 到共享视图。本原型的 `INSERT OR IGNORE` 语义就是 CRDT join。

## 3. 事件流对接 WAL（议题 8）

原子写入 + verdict 追加是"信念状态变更的可审计流"：

- `atoms` 表 INSERT 与 `verdicts` 表 INSERT 应进内核 WAL（议题 8 已定"原子转换可入 WAL"），获得 replay/审计能力；
- 本原型用单连接 SQLite 事务提交，工程化时把 `_insert` / `record_verdict` 挂到 WAL appender 之后即可，表结构无需变。

## 4. 闸门与路由的消费方（议题 9/12）

存储层已提供的查询原语 → 内核机制的直接对接：

| 内核机制 | 本原型原语 |
|---|---|
| "只许 verified 进规划上下文"闸门 | `verification_of(id).status is VERIFIED`（过滤规则，机械性合规） |
| provenance gating（只许 verified 进共享，exp5 对照组） | 同上 + `write_claim` 的 source/assertion 信封过滤 |
| 接触追踪 / 召回 | `descendants()` BFS + `recall()` 标 disputed |
| 遗忘（TTL 回收） | `expired(now)` 列出候选，GC 归内核周期任务 |
| 看门狗进展信号（修订 2：只计确定性轨） | `run_acceptance` 的 deterministic 轨结果 + 新原子计数；llm-judge 结果进观测流水不进 watchdog |
| escrow 释放（修订 3） | `AcceptanceReport.escrow_releasable`（只绑 hard gate） |
| spawn 三划拨（议题 23：预算片+namespace+子规格） | `write_spec(..., lineage=(父规格,))` + REFINES link |

## 5. 工程化缺口（进未决清单）

1. **规模**：全表扫的 `descendants()` / `links_between()` 在万级原子下需要 lineage 邻接索引表与 endpoints 复合索引（SQLite 都能做，非设计问题）。
2. **verdict 流增长**：fold 最新事件的查询是 O(verdicts)，需要物化最新状态列或周期快照——但物化列的写入仍须走 `record_verdict` 单入口以保持修订 4 强制。
3. **signature 验证**：跨组织原子的 DID 签名验证（议题 16）是写入前的用户态步骤，存储层只原样保存。
4. **外置 content 的完整性**：`bb://` 引用形态下，chain_hash/id 仍哈希引用字符串；字节对象的完整性由 Blackboard 自身的哈希寻址保证，两层哈希链需在工程化时统一。
