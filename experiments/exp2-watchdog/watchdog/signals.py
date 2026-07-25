"""进展信号提取（"喂狗"）：全部是结构信号，不做语义判断。

三类信号（llmos 议题 9 定案）：
1. 新工具调用：本次 (工具名, 规范化参数) 组合在此 run 中首次出现
2. 新产物：工具返回与历史产物不相似（含新信息），或产出最终答案
3. 显式心跳：harness 声明的 heartbeat 标志
"""

from __future__ import annotations

import json
from dataclasses import dataclass, field

from .similarity import ngram_jaccard


def canonical_args(raw_arguments: str) -> str:
    """参数规范化：JSON 键排序，使语义相同的参数串得到相同指纹。"""
    try:
        parsed = json.loads(raw_arguments)
    except json.JSONDecodeError:
        return raw_arguments.strip()
    return json.dumps(parsed, sort_keys=True, ensure_ascii=False)


@dataclass(frozen=True, slots=True)
class ToolCallView:
    name: str
    arguments: str  # 已规范化

    @classmethod
    def from_raw(cls, name: str, raw_arguments: str) -> ToolCallView:
        return cls(name=name, arguments=canonical_args(raw_arguments))


@dataclass(frozen=True, slots=True)
class StepObservation:
    """看门狗在每步边界观察到的事实（请求/响应拦截点提取）。"""

    step_index: int
    finish_reason: str
    content: str
    tool_calls: tuple[ToolCallView, ...] = ()
    tool_results: tuple[str, ...] = ()
    heartbeat: bool = False


@dataclass(frozen=True, slots=True)
class ProgressSignals:
    new_tool_call: bool
    new_artifact: bool
    heartbeat: bool

    @property
    def any_progress(self) -> bool:
        return self.new_tool_call or self.new_artifact or self.heartbeat


@dataclass(slots=True)
class SignalTracker:
    """跨步累积的信号状态（每个 run 一个实例）。"""

    artifact_novelty_threshold: float = 0.7
    _seen_tool_calls: set[tuple[str, str]] = field(default_factory=set)
    _prior_results: list[str] = field(default_factory=list)

    def observe(self, obs: StepObservation) -> ProgressSignals:
        fingerprints = {(c.name, c.arguments) for c in obs.tool_calls}
        new_tool_call = bool(fingerprints - self._seen_tool_calls)
        self._seen_tool_calls |= fingerprints

        new_artifact = self._has_novel_artifact(obs)
        self._prior_results.extend(obs.tool_results)

        return ProgressSignals(
            new_tool_call=new_tool_call,
            new_artifact=new_artifact,
            heartbeat=obs.heartbeat,
        )

    def _has_novel_artifact(self, obs: StepObservation) -> bool:
        # 最终答案落地本身就是产物推进（run 通常到此结束）
        if obs.finish_reason == "stop" and obs.content.strip():
            return True
        for result in obs.tool_results:
            if not result.strip():
                continue
            max_sim = max(
                (ngram_jaccard(result, prior) for prior in self._prior_results),
                default=0.0,
            )
            if max_sim < self.artifact_novelty_threshold:
                return True
        return False
