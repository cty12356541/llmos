# exp7-semantic-atoms：语义原子最小 CRUD（对应外部评审 C-07）

> 日期：2026-07-26
> 验证目标：语义层（议题 21-23 定案）与内核的集成路径——原子信封在真实存储上是否自洽？不变式能否被强制？
> 结果：**判定标准通过**（56 测试全绿，ruff 清洁）

## 设计

单文件 SQLite（stdlib `sqlite3`，无服务端、无网络、无真实 LLM）。包 `sematom`：

| 模块 | 职责 |
|---|---|
| `model.py` | 信封类型 + 冻结枚举（议题 24 宪法）+ 内容哈希 id / chain_hash |
| `errors.py` | typed 异常（ImmutableViolation / ForbiddenVerifiedWrite / ClosedSetViolation / UnknownAtom） |
| `store.py` | AtomStore：只增不改的原子表 + verdict 事件流 + 墓碑 + 血缘遍历 + TTL |
| `links.py` | LinkAtom 五关系写入/查询、REFINES 精化链 |
| `spec.py` | IntentSpec 三轨验收、deterministic 执行器、MockJudge、规格即原子存取 |

### 关键形态决策

1. **不可变的物理强制**：`atoms` 表挂 `BEFORE UPDATE/DELETE` 触发器，连裸 SQL 都改不动；Python API 的 `update_atom()` 直接抛 `ImmutableViolation`。"修改"= 新原子 + lineage（议题 21 Q2）。
2. **验证状态不改写原子**：语义状态变更（unverified→verified/disputed）是 `verdicts` 追加事件，读出时 fold 最新事件（议题 24 Q3：语义状态变更必须由用户态判定流程产出带 provenance 的记录）。这消解了"原子不可变"与"验证状态机迁移"的表面冲突。
3. **verified 写入限制（修订 4）**：`record_verdict(VERIFIED)` 仅接受 `method ∈ {deterministic, independent-verifier}`；`self-attested` 和 `human` 抛 `ForbiddenVerifiedWrite`。disputed/unverified 不受限（分歧可见，议题 12）。
4. **link id 含 relation**：同一对端点上的矛盾判断（CONTRADICTS vs EQUIVALENT）是不同原子，共存不裁决（修订 6：link atom 不自动合并）。首版实现漏了这一点导致矛盾判断被内容哈希去重吞掉——测试抓住后修正，这正是"先写判定标准"的价值。
5. **召回跳过 tombstone**：tombstone 的 lineage 指向目标（推导关系入血缘），但召回传播时墓碑是终态元数据，不再标 disputed。
6. **三轨验收的 escrow 语义（修订 3）**：`AcceptanceReport.escrow_releasable` 只绑 hard gate（deterministic 全过）；soft gate（llm-judge/human）结果进 `soft_outcomes` 供信誉/评价，不挡结算。
7. **MockJudge**：可配置 `pass_rate` / `false_positive_rate` / `false_negative_rate`，种子可复现；不读 criterion/artifact 语义。

## 判定标准对照（experiments/README.md exp7 节）

> 议题 21-23 的全部结构不变式有测试锁定（不可变、墓碑、闭集枚举、内容不被存储层解析）

| 不变式 | 测试组 | 测试数 | 结果 |
|---|---|---|---|
| 不可变（API + 裸 SQL 双锁；修改=新原子+lineage） | `test_immutability.py` | 4 | ✅ |
| 墓碑撤回 + tombstone 优先（修订 6，防复活） | `test_tombstone.py` | 5 | ✅ |
| 内容哈希去重（同 content 同 id 无新行；字面边界） | `test_hash_dedup.py` | 5 | ✅ |
| 枚举冻结（关系 5 值/断言 4 值/method 3 值/constraints 闭集键，集外拒绝） | `test_enum_freeze.py` | 6 | ✅ |
| verified 写入限制（修订 4：仅两档可写；事件折叠非改写） | `test_verified_restriction.py` | 6 | ✅ |
| 血缘遍历（父必须存在、BFS、菱形去重、召回标 disputed） | `test_lineage.py` | 5 | ✅ |
| TTL 过期（遗忘挂钩；边界时刻精确） | `test_ttl.py` | 4 | ✅ |
| 三轨验收（deterministic 执行/mock judge 通过率与误判率/human pending/hard-soft gate 分离） | `test_acceptance.py` | 8 | ✅ |
| 规格即原子 + REFINES 精化链（版本链=需求演化链；墓碑断链） | `test_spec_refinement.py` | 4 | ✅ |
| 内容不被存储层解析（静态扫描禁语义操作 + 诱饵内容字节级往返 + 内容不影响状态机） | `test_content_never_parsed.py` | 4 | ✅ |
| link 五关系行为（矛盾共存、provenance 可查、端点存在性） | `test_links.py` | 4 | ✅ |
| **合计** | | **56** | **全绿** |

## 运行

```bash
cd experiments/exp7-semantic-atoms
uv sync
uv run pytest -q     # 56 passed
uvx ruff check sematom tests
```

## 回答 C-07 的三个问题

1. **原子信封在真实存储上是否自洽？** 是。议题 21 定案字段逐字段落 SQLite 无阻抗失配；唯一需要的设计补充是验证状态的事件化（verdict 流 fold），它同时满足不可变与状态机迁移，且天然对齐议题 24 Q3。
2. **不变式能否被强制？** 能，且大部分可在存储层物理强制：不可变靠触发器（连裸 SQL 都挡）、枚举冻结靠边界解析（`parse_enum` 在写入路径拒绝集外值）、verified 限制靠 `record_verdict` 单入口。唯一不能靠数据库强制的是"content 不被解析"——它靠静态扫描测试 + 运行时诱饵探针双重锁定。
3. **与 Blackboard 如何对接？** 见 [INTEGRATION.md](INTEGRATION.md)。

## 已知边界（明确不做）

- 无 embedding/L1 粗筛、无 L2 真实 LLM 判定（议题 22 三层架构只做 L0 + link 存储；L1/L2 归后续实验）
- 无 DID 签名验证（signature 字段仅存取，议题 16 跨组织场景）
- 遗忘只到"过期查询"挂钩，无物理 GC（修订 6 的最终一致档 merge 归分布式实验）
- verdict 流无压缩/快照（规模问题非本实验目标）
