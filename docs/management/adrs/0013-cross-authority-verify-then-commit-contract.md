# ADR-0013：跨 authority 提交边界采用 verify-then-commit 加有界收敛为正式契约

- 状态：ACCEPTED
- 日期：2026-08-29
- Owner：TaskAuthority / commit-coordinator 家族
- 关联 Requirement：无新 Requirement ID（本 ADR 为既有语义的契约升格）；规范锚点为总纲 v0.5 §26.1 蜂窝式权威
- 关联工作包：`B-TASK-008C2G-COORD`、`B-TASK-008C2G-RES-COMMIT`、`B-TASK-008C2G-SEM`、`B-TASK-008C2G-CROSS-TERM-ADOPTION`
- 决策来源：用户于 2026-08-29 决策会话在三候选（批准现契/单机 ATTACH 共享事务/2PC 协议族）中选择「批准现契」
- 复审触发器：发现收敛不有界的反例（登记 conflict、降级相关 Claim 并重开本议题）；Stage C 跨机提交语义定案；出现必须跨权威强原子的新需求（届时新 ADR 评估 2PC/reconciliation 混合）

## 上下文

跨 authority 提交的现行模式自 [ADR-0004](./0004-task-authority-commit-recovery-owner.md)（恢复归属）与 [ADR-0005](./0005-task-write-set-authority-first.md)（authority-first 顺序）定型以来保持一致：Task 侧在打开单个 Task 终结事务之前，先经各 owner 权威回读验证（`inspect_*_proof`/`inspect_*_receipt` 家族），验证通过后才在同一 Task 事务内写入嵌套权威回执；owner 读发生在 Task 事务之外，两个 authority 仍是两个事务域。崩溃窗口不依赖原子回滚，而由公开 `converge_pending` 从两个 authority 的 durable prefix 收敛。

由此，coordinator/finalize 家族（B-TASK-008C2G 系列）十余个工作包长期挂着同一句诚实标注：「仍是 verify-then-commit，不声称跨 authority 原子性」。该标注反复出现在 [B-TASK-008C2G-SEM](../../evidence/stage-b/b-task-008c2g-semantic-publication-consumer.md)、[B-TASK-008C2G-COORD](../../evidence/stage-b/b-task-008c2g-semantic-coordinator.md)、[B-TASK-008C2G-RES-COMMIT](../../evidence/stage-b/b-task-008c2g-resource-cost-commit.md) 与 [B-TASK-008C2G-CROSS-TERM-ADOPTION](../../evidence/stage-b/b-task-008c2g-cross-term-adoption.md) 等切片 Evidence 的边界声明中，并作为未决项挂在各工作包上：verify-then-commit 边界究竟是需要弥补的缺口，还是可以定案的契约，一直没有裁决。

支持定案的证据已经齐备。全部 kill-window 故障矩阵（task/coordinator/resource/semantic IPC 家族）已证明该模式无幻影行、无双重提交、逐字节幂等 replay；阶段 B 退出门 ROAD-B-003 要求的是双提交、竞态与崩溃收敛被守住，未要求跨权威原子性；[总纲 §26.1](../../design/06-架构设计总纲-v0.5.md) 的蜂窝式权威哲学本就偏向调和而非全局事务。

## 候选

| 候选 | 优点 | 主要代价 |
|---|---|---|
| **A. 批准 verify-then-commit + 有界收敛为正式契约**（采纳） | 零新实现义务；契约不变量全部由既有故障矩阵背书；与蜂窝式权威哲学一致 | 明确放弃单机跨权威原子性声明；崩溃窗口依赖收敛语义而非原子回滚 |
| B. 单机 SQLite ATTACH 跨库共享事务（否决） | Task 与各 owner store 进入同一物理事务，消除验证与提交之间的窗口 | 摧毁「一 authority 一 store 一写者」的整个 crate 分解与隔离模型；跨库 WAL 故障语义相互纠缠；全部已有故障矩阵需要重做。负价值 |
| C. 协议级 2PC 协调者（否决） | 提供跨权威原子提交的教科书语义 | 巨大新协议面（协调者日志、in-doubt 窗口、恢复协议）与已被证明的更简单模型重复建设，等于把 Stage C 难题前移进 Stage B |

## 决定

1. **契约升格**：verify-then-commit 加有界收敛升格为单机跨 authority 提交的正式契约。语义即现行模式：Task 侧先经 owner 回读验证（`inspect_*_proof`/`inspect_*_receipt`），再在单 Task 事务写入嵌套回执；崩溃窗口由公开 `converge_pending` 从 durable prefix replay 收敛。契约不变量为崩溃窗口收敛性、无幻影行、无双重提交、replay 逐字节幂等，全部由既有故障矩阵背书。
2. **关闭未决项**：coordinator 家族各工作包未决项中的「跨 authority 原子性（verify-then-commit 边界）」以本 ADR 关闭。实现状态仍按各切片 Evidence 如实标注，不因契约定案而升级任何实现声明；各 Evidence 的诚实边界描述继续有效，只是性质由「未决缺口」变为「契约语义」。
3. **登记 Stage C 扩展点**：跨机（跨 Cell）提交语义不在本契约范围内；扩展点登记为 reconciliation 协调（蜂窝式权威 §26.1 偏向调和而非全局共识），届时经新 ADR 定案。

## 后果与退出策略

- 十余个工作包的长期未决项就此关闭，coordinator 家族可按既有语义收尾；后续切片与 Evidence 不再把 verify-then-commit 边界挂为未决项，改为引用本 ADR 的契约语义。
- 诚实标注从「未决」转为「契约语义即收敛」：verify-then-commit 边界描述保留，实现与验证声明本身不变。
- 本 ADR 为纯决策落档，不引入任何新实现、测试、schema 或迁移义务。
- 退出策略：复审触发器成立即重开。若发现收敛不有界的反例（存在无法从 durable prefix 收敛到唯一终态的崩溃窗口），必须登记 conflict、降级相关 Claim 并重开本议题，重开期间各切片边界描述回退为未决，不得引用本 ADR 作为契约依据；Stage C 跨机提交语义定案或出现必须跨权威强原子的新需求时，经新 ADR 评估 2PC/reconciliation 混合，本 ADR 在单机范围内继续有效或显式标记 SUPERSEDED。
