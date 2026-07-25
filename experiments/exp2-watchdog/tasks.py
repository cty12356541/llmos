"""任务集定义：≥10 正常 + ≥5 空转 + ≥3 边界（慢但有进展）。

- normal：diverse 模式，覆盖计算/查询/多步推理三类，预期看门狗全程不触发
- livelock：stuck/mixed 模式，预期在阈值步数内被挂起标记
- boundary：slow 模式，进展间隔小于 maxStepsWithoutProgress、步间相似度低于阈值，
  预期不被误杀且正常完成
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Literal

from providers.scripted import Flavor, ProviderMode, ScriptedProvider

TaskKind = Literal["normal", "livelock", "boundary"]


@dataclass(frozen=True, slots=True)
class Task:
    task_id: str
    kind: TaskKind
    prompt: str
    mode: ProviderMode
    flavor: Flavor
    seed: int
    planned_steps: int = 4
    stuck_after: int = 2
    progress_interval: int = 3

    def provider(self) -> ScriptedProvider:
        return ScriptedProvider(
            mode=self.mode,
            seed=self.seed,
            flavor=self.flavor,
            planned_steps=self.planned_steps,
            stuck_after=self.stuck_after,
            progress_interval=self.progress_interval,
        )


def _normal(task_id: str, flavor: Flavor, seed: int, planned_steps: int, prompt: str) -> Task:
    return Task(task_id, "normal", prompt, "diverse", flavor, seed, planned_steps=planned_steps)


NORMAL_TASKS: tuple[Task, ...] = (
    _normal("n-calc-01", "calc", 101, 3, "算一下 (12*8)+5，再算它的平方根。"),
    _normal("n-calc-02", "calc", 102, 4, "分别计算 7*13、15*15、100-37，并汇总。"),
    _normal("n-calc-03", "calc", 103, 5, "一个三步算术题：先乘后加再取模。"),
    _normal("n-query-01", "query", 104, 3, "查一下档案里关于 atlas 项目的三条笔记。"),
    _normal("n-query-02", "query", 105, 4, "分段检索 cipher 与 delta 两个主题的记录。"),
    _normal("n-query-03", "query", 106, 5, "依次查询五个数据集的 schema 摘要。"),
    _normal("n-reason-01", "reason", 107, 4, "多步推理：先算比例，再查证据，再给结论。"),
    _normal("n-reason-02", "reason", 108, 5, "假设检验链：提出、验证、反例、修正、结论。"),
    _normal("n-reason-03", "reason", 109, 6, "六步分析任务：模式识别 → 异常 → 趋势 → 归因。"),
    _normal("n-calc-04", "calc", 110, 6, "较长算术流水线：六个不同表达式的求值。"),
)

LIVELOCK_TASKS: tuple[Task, ...] = (
    Task("l-stuck-01", "livelock", "这个问题再想一下，继续。", "stuck", "calc", 201),
    Task("l-stuck-02", "livelock", "不确定，重新考虑同一个方案。", "stuck", "query", 202),
    Task("l-stuck-03", "livelock", "再检查一遍刚才的计算。", "stuck", "reason", 203),
    Task("l-mixed-01", "livelock", "先算两步，然后继续。", "mixed", "calc", 204, stuck_after=2),
    Task("l-mixed-02", "livelock", "查三个主题后给出结论。", "mixed", "query", 205, stuck_after=3),
)

BOUNDARY_TASKS: tuple[Task, ...] = (
    Task("b-slow-01", "boundary", "沿既定方法系统排查，每几步记录一次中间结果。", "slow", "calc", 301,
         planned_steps=8, progress_interval=3),
    Task("b-slow-02", "boundary", "长程检索任务：同一方向逐段推进。", "slow", "query", 302,
         planned_steps=10, progress_interval=3),
    Task("b-slow-03", "boundary", "审慎推理：步伐慢但每轮都有新证据。", "slow", "reason", 303,
         planned_steps=9, progress_interval=2),
)

ALL_TASKS: tuple[Task, ...] = NORMAL_TASKS + LIVELOCK_TASKS + BOUNDARY_TASKS
