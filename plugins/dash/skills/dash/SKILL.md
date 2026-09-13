---
name: dash
description: Render the project DAG dashboard (panel/oneline) on demand, and set panel focus when dispatching, reviewing, or entering fix rounds. Use when the user asks for 进度/DAG/仪表盘, or at any task state transition in an orchestrated wave. Zero-config for any repo; deep semantics if .superpowers/sdd/ exists.
---

# /dash — 项目推进仪表盘

数据源是权威文件与会话事件,不要从对话记忆重构状态:
`.dash/state.jsonl`(hook 事件)+ git 快照 + `.superpowers/sdd/*`(存在时)。

## 执行

```bash
python3 "${CLAUDE_PLUGIN_ROOT}/scripts/dash" render panel   # 会话内点播(附图)
python3 "${CLAUDE_PLUGIN_ROOT}/scripts/dash" oneline        # 单行摘要
python3 "${CLAUDE_PLUGIN_ROOT}/scripts/dash" focus T9       # 状态跃迁时聚焦现场
```

## 跃迁约定(编排波次中)

派发/过审/修复环/完成任务时:回复末尾附 `render panel` 输出,并执行
`dash focus <任务id>` 把面板镜头带到现场;波次收尾执行 `dash focus clear`。

## 诚实约束

图是数据源的投影:冲突时以台账/事件为准并修渲染器,不得改数据迁就图。
