"""看门狗状态机：进展信号计数 + 连续步相似度 → 阈值触发挂起标记。

内核侧机制原型（第一级）：只做结构判断，触发后输出监督事件 JSON
（模拟"转第二级语义监督"——本实验只到标记，不做 LLM 裁决）。
"""

from __future__ import annotations

import time
from dataclasses import dataclass
from datetime import UTC, datetime
from typing import Any, Literal

from .config import WatchdogConfig
from .signals import ProgressSignals, SignalTracker, StepObservation
from .similarity import step_similarity

TriggerReason = Literal["no_progress", "repeat_similarity"]


@dataclass(frozen=True, slots=True)
class SuspensionEvent:
    """挂起标记事件：转第二级语义监督的机器可读凭据。"""

    task_id: str
    step: int
    reason: TriggerReason
    steps_without_progress: int
    consecutive_similar_steps: int
    last_similarity: float
    thresholds: dict[str, float | int]
    timestamp: str
    action: str = "suspend_and_mark"
    handoff: str = "level2_semantic_supervision"

    def to_dict(self) -> dict[str, Any]:
        return {
            "event": "watchdog_suspension",
            "task_id": self.task_id,
            "step": self.step,
            "reason": self.reason,
            "steps_without_progress": self.steps_without_progress,
            "consecutive_similar_steps": self.consecutive_similar_steps,
            "last_similarity": round(self.last_similarity, 4),
            "thresholds": self.thresholds,
            "action": self.action,
            "handoff": self.handoff,
            "timestamp": self.timestamp,
        }


class Watchdog:
    """每步 observe 一次；触发后锁存（latched），后续 observe 返回同一事件。"""

    def __init__(self, config: WatchdogConfig, task_id: str) -> None:
        self._config = config
        self._task_id = task_id
        self._tracker = SignalTracker(
            artifact_novelty_threshold=config.artifact_novelty_threshold
        )
        self._steps_without_progress = 0
        self._consecutive_similar = 0
        self._prev_content: str | None = None
        self._last_similarity = 0.0
        self._last_signals = ProgressSignals(new_tool_call=False, new_artifact=False, heartbeat=False)
        self._event: SuspensionEvent | None = None

    @property
    def event(self) -> SuspensionEvent | None:
        return self._event

    @property
    def last_signals(self) -> ProgressSignals:
        """最近一步提取到的进展信号（供 harness 记录 trace）。"""
        return self._last_signals

    @property
    def last_similarity(self) -> float:
        return self._last_similarity

    def observe(self, obs: StepObservation) -> SuspensionEvent | None:
        """拦截点：每步请求/响应后调用。返回触发事件或 None。"""
        if self._event is not None:
            return self._event
        signals = self._tracker.observe(obs)
        self._last_signals = signals
        self._update_progress_counter(signals)
        self._update_similarity_counter(obs.content)
        self._event = self._maybe_trigger(obs)
        return self._event

    def _update_progress_counter(self, signals: ProgressSignals) -> None:
        if signals.any_progress:
            self._steps_without_progress = 0
        else:
            self._steps_without_progress += 1

    def _update_similarity_counter(self, content: str) -> None:
        if self._prev_content is None:
            self._last_similarity = 0.0
        else:
            self._last_similarity = step_similarity(
                self._prev_content,
                content,
                backend=self._config.similarity_backend,
                ngram=self._config.ngram,
            )
        if self._last_similarity >= self._config.max_repeat_similarity:
            self._consecutive_similar += 1
        else:
            self._consecutive_similar = 0
        self._prev_content = content

    def _maybe_trigger(self, obs: StepObservation) -> SuspensionEvent | None:
        # 两条件同步触发时 no_progress 优先（进展信号是主判据，相似度是佐证）
        reason: TriggerReason | None = None
        if self._steps_without_progress >= self._config.max_steps_without_progress:
            reason = "no_progress"
        elif self._consecutive_similar >= self._config.repeat_window:
            reason = "repeat_similarity"
        if reason is None:
            return None
        return SuspensionEvent(
            task_id=self._task_id,
            step=obs.step_index,
            reason=reason,
            steps_without_progress=self._steps_without_progress,
            consecutive_similar_steps=self._consecutive_similar,
            last_similarity=self._last_similarity,
            thresholds={
                "maxStepsWithoutProgress": self._config.max_steps_without_progress,
                "maxRepeatSimilarity": self._config.max_repeat_similarity,
                "repeatWindow": self._config.repeat_window,
            },
            timestamp=datetime.fromtimestamp(time.time(), UTC).isoformat(timespec="milliseconds"),
        )
