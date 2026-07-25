"""实验运行器：跑任务集 → 每任务触发情况 → 误报/检出度量。"""

from __future__ import annotations

import json
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any

from harness.react_loop import RunResult, run_episode
from tasks import BOUNDARY_TASKS, LIVELOCK_TASKS, NORMAL_TASKS, Task
from watchdog import WatchdogConfig


@dataclass(frozen=True, slots=True)
class TaskOutcome:
    task_id: str
    kind: str
    outcome: str  # completed | suspended | max_steps
    steps: int
    trigger_step: int | None  # 1-based，未触发为 None
    reason: str | None
    last_similarity: float

    @classmethod
    def from_run(cls, kind: str, run: RunResult) -> TaskOutcome:
        event = run.event
        return cls(
            task_id=run.task_id,
            kind=kind,
            outcome=run.outcome,
            steps=run.steps,
            trigger_step=(event.step + 1) if event else None,
            reason=event.reason if event else None,
            last_similarity=run.traces[-1].similarity if run.traces else 0.0,
        )


@dataclass(slots=True)
class ExperimentResult:
    config: WatchdogConfig
    outcomes: list[TaskOutcome] = field(default_factory=list)

    def by_kind(self, kind: str) -> list[TaskOutcome]:
        return [o for o in self.outcomes if o.kind == kind]

    @property
    def false_positives(self) -> list[TaskOutcome]:
        return [o for o in self.by_kind("normal") if o.outcome == "suspended"]

    @property
    def fp_rate(self) -> float:
        normal = self.by_kind("normal")
        return len(self.false_positives) / len(normal) if normal else 0.0

    @property
    def detections(self) -> list[TaskOutcome]:
        return [o for o in self.by_kind("livelock") if o.outcome == "suspended"]

    @property
    def detection_rate(self) -> float:
        livelock = self.by_kind("livelock")
        return len(self.detections) / len(livelock) if livelock else 0.0

    @property
    def detection_steps(self) -> list[int]:
        return [o.trigger_step for o in self.detections if o.trigger_step is not None]

    @property
    def boundary_failures(self) -> list[TaskOutcome]:
        return [
            o for o in self.by_kind("boundary") if o.outcome != "completed"
        ]

    def to_dict(self) -> dict[str, Any]:
        return {
            "config": {
                "maxStepsWithoutProgress": self.config.max_steps_without_progress,
                "maxRepeatSimilarity": self.config.max_repeat_similarity,
                "repeatWindow": self.config.repeat_window,
                "similarityBackend": self.config.similarity_backend,
            },
            "metrics": {
                "normal_tasks": len(self.by_kind("normal")),
                "false_positives": len(self.false_positives),
                "fp_rate": round(self.fp_rate, 4),
                "livelock_tasks": len(self.by_kind("livelock")),
                "detections": len(self.detections),
                "detection_rate": round(self.detection_rate, 4),
                "detection_steps": self.detection_steps,
                "boundary_tasks": len(self.by_kind("boundary")),
                "boundary_failures": [o.task_id for o in self.boundary_failures],
            },
            "outcomes": [
                {
                    "task_id": o.task_id,
                    "kind": o.kind,
                    "outcome": o.outcome,
                    "steps": o.steps,
                    "trigger_step": o.trigger_step,
                    "reason": o.reason,
                }
                for o in self.outcomes
            ],
        }


def run_task(task: Task, config: WatchdogConfig) -> TaskOutcome:
    run = run_episode(task.task_id, task.prompt, task.provider(), config)
    return TaskOutcome.from_run(task.kind, run)


def run_task_set(
    config: WatchdogConfig,
    tasks: tuple[Task, ...] = NORMAL_TASKS + LIVELOCK_TASKS + BOUNDARY_TASKS,
) -> ExperimentResult:
    result = ExperimentResult(config=config)
    for task in tasks:
        result.outcomes.append(run_task(task, config))
    return result


def write_json(result: ExperimentResult, path: Path) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(
        json.dumps(result.to_dict(), ensure_ascii=False, indent=2) + "\n",
        encoding="utf-8",
    )
