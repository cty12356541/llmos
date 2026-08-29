# llmos 项目管理机制

> 最高目标：构建达到 Windows/macOS 级别的通用、现代、系统级 NLOS。
>
> 本文件管理“如何把目标持续变成可验证系统”，不改变 [v0.5 架构规范](../design/06-架构设计总纲-v0.5.md)。

## 1. 管理原则

1. **目标、规范、实现、证据分离**：愿景不能冒充能力，代码通过 happy path 不能冒充 conformance。
2. **纵切面优先**：每个里程碑必须贯通身份、权限、资源、执行、持久化、恢复和用户控制，避免只堆独立组件。
3. **需求可追踪**：规范 Requirement ID → 工作项 → 测试 → Evidence → Release Claim 必须可机械追踪。
4. **架构决策显式化**：不可逆或跨模块技术选择必须进入 ADR，注明替代方案、退出成本和复审日期。
5. **默认小批量集成**：短分支、原子提交、持续测试；长期分叉和“大爆炸合并”需要明确批准。
6. **风险先验证**：隔离、durability、取消、资源守恒、ABI 和规模假设先做 PoC/故障注入。
7. **用户控制不后补**：自然语言、GUI、CLI、API 必须从同一 ControlCommand/Receipt 路径生长。

## 2. 权威文档及优先级

冲突时按下列顺序处理：

1. 项目最高目标与安全/权限边界；
2. 当前规范性架构总纲；
3. 已接受 ADR；
4. 阶段计划与验收门；
5. 实现和测试；
6. 讨论记录、原型和站点说明。

代码与规范冲突时，默认登记实现缺口；不能仅因代码已经存在就反向修改规范。

## 3. 工作分解模型

```text
North Star
  → Stage
    → Capability
      → Vertical Slice
        → Work Package
          → Issue / Change
            → Test / Evidence
              → Release Claim
```

| 层级 | 必须包含 |
|---|---|
| Stage | 目标、范围、非目标、进入/退出门、风险预算 |
| Capability | 用户价值、系统对象、Requirement ID、owner |
| Vertical Slice | 端到端路径、故障路径、观测点、演示场景 |
| Work Package | 输入、输出、依赖、验收、预计风险；建议 1–5 个工作日 |
| Issue/Change | 单一结果、关联 ID、测试方案 |
| Evidence | commit、环境、命令、原始输出、断言、复现方法 |
| Release Claim | profile、assurance、适用范围、证据引用和已知限制 |

## 4. 状态模型

### 4.1 工作项

```text
PROPOSED → TRIAGED → READY → IN_PROGRESS → REVIEW
                                      ↘ BLOCKED
REVIEW → VERIFIED → DONE
```

- `READY`：边界、依赖、验收条件和责任人完整。
- `VERIFIED`：测试和证据已完成，不等于已经发布。
- `DONE`：文档、代码、证据和发布说明均已同步。
- `BLOCKED`：必须记录阻塞原因、已尝试方案、需要的决定和复查时间。

### 4.2 技术决策

```text
QUESTION → CANDIDATE → POC → ACCEPTED
                         ↘ REJECTED
ACCEPTED → SUPERSEDED
```

“推荐”不等于 `ACCEPTED`；没有 ADR 和规定证据的选择不得冻结为稳定契约。

## 5. Definition of Ready

工作项进入实施前必须具备：

- 对应 Stage/Capability 和 Requirement ID；
- 明确范围、非目标、依赖和兼容影响；
- happy path、failure path、安全路径的验收条件；
- 是否影响 KABI/SABI/schema/durable format；
- 测试层级与 Evidence 产物；
- 对破坏性迁移、数据丢失和回滚的说明。

## 6. Definition of Done

工作项只有同时满足以下条件才能完成：

- 实现与规范一致；
- unit、integration、conformance 或 fault-injection 测试按风险通过；
- 无未解释的 capability/resource/durability 绕过；
- metrics、trace、typed error 和 Receipt 足以诊断；
- 相关设计、ADR、运行说明和已知限制同步；
- Evidence 可从干净环境复现；
- 兼容性或数据格式变化具有迁移/回滚路径。

## 7. 评审与决策机制

| 变化 | 最低要求 |
|---|---|
| 局部实现，不改变契约 | 普通代码评审 + 自动测试 |
| 新依赖或可替换组件 | 技术选型记录 + license/security/maintenance 检查 |
| KABI/SABI/schema/durable format | ADR + compatibility review + golden vector |
| TCB、Capability、secret、sandbox | threat review + bypass test |
| Resource/Budget/durability | property test + crash/fault injection |
| Stage 退出或 production claim | Evidence review + 未知风险清单 + 明确批准 |

ADR 至少包含：上下文、约束、候选、比较、决定、后果、退出策略、证据、复审触发器。

## 8. 需求与证据台账

建议建立机器可读台账：

```text
management/
  README.md
  stage-b-technology-selection.md
  stages/
  adrs/
  claims.yaml
  risks.yaml
  evidence-index.yaml
```

`claims.yaml` 的最小字段：

```yaml
- requirement_id: FIBER-MN-001
  stage: B
  status: planned
  implementation_refs: []
  test_refs: []
  evidence_refs: []
  assurance: DESIGN
  limitations: []
```

在 schema 确定前不批量生成空台账；首先用阶段 B 的第一个纵切面验证字段是否足够。

## 9. 风险管理

风险按 `影响 × 发生概率 × 不可逆性` 排序，至少维护：

- 安全与权限旁路；
- durable 状态丢失或双重提交；
- Resource/Budget 超卖；
- cancellation/effect unknown；
- ABI/数据格式过早冻结；
- runtime 或 UI 技术锁定；
- 规模目标被冷 metadata 数量冒充；
- 单人关键知识和维护依赖；
- 第三方依赖 license、供应链和停止维护。

P0 风险阻止 Stage 退出；P1 必须有 owner、缓解措施和明确复查点。

## 10. 分支、提交与发布

- 默认从 `main` 创建短生命周期 `codex/<topic>` 或功能分支；紧急修复例外需记录。
- 一个 canonical 结果对应一个可解释、可独立验证和回退的原子提交；提交信息说明“做了什么、为什么、证据是什么”。
- 开始任务时记录 base HEAD；提交前检查 HEAD 漂移，漂移后重新审查 read-set/write-set 并复验。
- 只显式暂存当前 Task/Attempt 的 write-set；共享脏工作区禁止无差别 `git add -A`/`git add .`，不得顺带提交其他 Agent 或用户改动。
- 暂存后检查 staged name/diff、`git diff --cached --check`、适用测试和 Evidence；未运行项目必须明确记录。
- 禁止把密钥、真实凭证、用户数据和不可再生大型输出提交到仓库。
- 未达到 DoD 的候选不得冒充完成；确需 checkpoint commit 时必须标记 `WIP/POC/PARTIAL`。
- 多 Agent 的同一 canonical 对象由单一 integrator 提交；禁止擅自 amend/rebase/reset/force-push 或改写他人历史。
- 合并前运行与变更风险相称的测试，保留命令和结果。
- 发布 Claim 只来自已提交 Evidence；设计文档中的未来时态不得出现在能力声明中。
- 破坏性 schema/数据迁移先备份、演练恢复，再允许发布。
- push 前核对 branch/upstream/HEAD，只允许 fast-forward；push、CI、部署分别确认，任何一级成功都不能冒充后一级成功。
- 详细规范见[项目知识渐进式披露与自动 CRUD 规则第 8 节](./project-knowledge-progressive-disclosure.md#8-控制与提交协议)。

## 11. 节奏与看板

建议使用滚动两周节奏，但以验收门而非日期驱动：

- 周期开始：选择一个纵切面和最多两个主要风险；
- 每日：更新工作项状态、阻塞和最新 Evidence；
- 周中：运行集成/故障测试，避免结束前集中发现架构问题；
- 周期结束：演示端到端路径、复盘偏差、更新 ADR/风险/证据；
- 每四个周期：重新检查阶段范围、技术选型和性能基线。

限制在制品：每位实现者最多一个主要 `IN_PROGRESS` Work Package；紧急故障除外。

## 12. 阶段 B 管理入口

阶段 B 当前管理入口：

- 项目知识规则：[project-knowledge-progressive-disclosure.md](./project-knowledge-progressive-disclosure.md)
- Rust 导读：[rust-for-nlos.md](./rust-for-nlos.md)
- 技术选型：[stage-b-technology-selection.md](./stage-b-technology-selection.md)
- 阶段 B 权威进度单：[stage-b-progress.md](./stage-b-progress.md)
- TaskAuthority 提交恢复归属：[ADR-0004](./adrs/0004-task-authority-commit-recovery-owner.md)
- TaskWriteSet authority-first 顺序：[ADR-0005](./adrs/0005-task-write-set-authority-first.md)
- Semantic publication receipt 权威归属：[ADR-0006](./adrs/0006-semantic-publication-receipt-owner.md)
- Topic 服务层单 log fanout 模型：[ADR-0007](./adrs/0007-topic-service-single-log-fanout.md)
- Durable wait registry 权威归属：[ADR-0008](./adrs/0008-durable-wait-registry-authority.md)
- Fiber 事件溯源续跑为主、受控快照兜底：[ADR-0009](./adrs/0009-fiber-event-sourced-resume.md)
- 多语言 SDK 支持评估：[language-sdk-support-plan.md](./language-sdk-support-plan.md)
- 规范路线：[v0.5 第 28.2 节](../design/06-架构设计总纲-v0.5.md#282-阶段-b单机通用应用平台)
- 执行层级决策：[议题 29](../discussions/29-现代系统执行层级与机制迁移.md)

第一个建议纵切面：

```text
signed Package
  → install Application
  → create Task/TaskPlan
  → materialize Process/AgentInstance/Fiber
  → async Driver Operation
  → Artifact + Receipt
  → cancel/crash recovery
  → CLI/NL inspect and control
```

在该纵切面通过前，不并行冻结完整桌面框架、分布式数据库或公共插件 ABI。

阶段 B 的实现状态、当前主线工作包、提交和 Evidence 以[权威进度单](./stage-b-progress.md)为准。任何完成的实现或验证工作都必须在对应 canonical commit 中同步更新该进度单。
