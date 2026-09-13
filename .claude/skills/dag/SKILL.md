---
name: dag
description: 渲染当前波次的任务 DAG 完成情况(终端 ANSI 车道图 + Mermaid 注入台账)。Use when the user asks for 波次进度/DAG/进度图, or when closing a task in an SDD wave. 前提:工作区有 .superpowers/sdd/<plan>/dag.json。
---

# /dag — 波次任务 DAG 渲染

渲染数据源是两个权威文件,不要从对话记忆里重构状态:

1. `dag.json`(工作区根):车道/任务/屏障的声明结构
2. `progress.md`(同目录):SDD 台账,`Task <N>: complete` / `dispatched` / `in-review` / `fix round` 行即状态

## 执行

```bash
python3 scripts/render_dag.py            # 自动选最新含 dag.json 的工作区
python3 scripts/render_dag.py <工作区目录>  # 指定
```

## 语义约定

- 状态解析顺序:complete 行 > dispatched/in-review/in-progress/fix round 行 > 待派发
- 三色:绿=完成、蓝=进行中(含审查/修复环)、灰=待派发;屏障用虚线
- Mermaid 自动写入 `dag.md` 并刷新 `progress.md` 的 `<!-- dag:begin/end -->` 标记块(重复运行幂等)
- 新波次开始时:在工作区手写 `dag.json`(lanes/tasks/barriers 三键,任务 label 从 plan 抄),派发任务时在台账追加 `Task <N>: dispatched (lane-x, <commit 基线>)` 行,过审查后改 complete 行

## 诚实约束

图只是台账的投影:如果图与台账文本冲突,以台账为准并修图/修脚本,不得反过来改台账迁就图。
