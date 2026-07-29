# llmos Agent 工作规则

本仓库的最高目标是构建 Windows/macOS 级通用现代 NLOS。任何 Agent 在读取、修改或新增项目知识前，必须遵守：

1. 先读取 [项目知识渐进式披露与自动 CRUD 规则](docs/management/project-knowledge-progressive-disclosure.md)。
2. 按任务范围渐进读取：L0 路由 → L1 当前规范/管理基线 → L2 相关议题/ADR → L3 代码与 Evidence；禁止默认把全仓库全部注入上下文。
3. 当前规范以 `docs/design/06-架构设计总纲-v0.5.md` 为准；历史文档只能解释 rationale，不能覆盖当前规范。
4. 修改必须进入正确权威对象，并同步其 L0 摘要/索引；不得把新结论只留在聊天、临时文件或重复文档中。
5. 多 Agent 并行时，每个任务使用独立 Task/Attempt/写集；共享文件按 revision/CAS 思维更新。冲突必须显式合并，禁止 last-writer-wins 静默覆盖。
6. 只有一个 Attempt 可以把同一工作项晋升为 canonical DONE。失败、取消或候选输出保留为 Evidence，不得冒充已采纳结论。
7. 设计、实现、测试和生产事实分级记录；没有 Evidence 不得把 `DESIGN` 改写为“已实现”。
8. 不得修改或清理不属于当前任务的现有工作区变更。
9. 完成的独立工作单元必须按详细规则形成原子提交：只暂存当前 Task/Attempt 的 write-set，提交前检查 HEAD 漂移、staged diff、敏感信息、格式和测试；禁止在共享脏工作区使用无差别 `git add -A`，禁止把其他任务改动顺带提交。
10. 一个 canonical 结果对应一个可解释提交；候选、失败或未通过验收的 Attempt 不得以完成状态提交。禁止擅自 amend、rebase、force-push、reset 或改写他人提交。push、发布和部署必须报告准确结果，不得把本地 commit 冒充远程已发布。

详细 CRUD、并发一致性、提交、推送、归档和验证规则以链接文档为准。
