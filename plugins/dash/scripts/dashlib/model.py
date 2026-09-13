"""dash 统一数据模型:五态任务、里程碑、活动、分片。纯 stdlib。"""
from __future__ import annotations

import unicodedata
from dataclasses import dataclass, field
from datetime import datetime
from typing import List, Optional, Dict

STATES = ("done", "active", "pending", "blocked", "stalled")


@dataclass
class Task:
    id: str
    label: str
    state: str                    # STATES 之一
    lane: str = "无车道"
    since: Optional[str] = None   # ISO8601 带时区偏移
    source: str = "git"           # session|git|sdd
    note: str = ""


@dataclass
class Milestone:
    id: str
    title: str = ""
    state: str = "planned"        # done|active|planned
    tasks_done: int = 0
    tasks_total: int = 0
    started: Optional[str] = None
    ended: Optional[str] = None


@dataclass
class Activity:
    kind: str                     # agent|todo|background|stop
    label: str
    since: str
    last_event: str = ""


@dataclass
class Fragment:
    source: str                   # sdd|session|git
    tasks: List[Task] = field(default_factory=list)
    milestones: List[Milestone] = field(default_factory=list)
    activity: List[Activity] = field(default_factory=list)
    velocity: Dict[str, int] = field(default_factory=dict)
    warnings: List[str] = field(default_factory=list)


@dataclass
class Model:
    project: str
    tasks: List[Task] = field(default_factory=list)
    milestones: List[Milestone] = field(default_factory=list)
    activity: List[Activity] = field(default_factory=list)
    stalled: List[str] = field(default_factory=list)
    velocity: Dict[str, int] = field(default_factory=dict)
    warnings: List[str] = field(default_factory=list)
    barriers: List[str] = field(default_factory=list)


def _parse(ts: str) -> datetime:
    return datetime.fromisoformat(ts)


def is_stalled(task: Task, now_iso: str, threshold_h: float = 2.0) -> bool:
    if task.state != "active" or task.since is None:
        return False
    return (_parse(now_iso) - _parse(task.since)).total_seconds() > threshold_h * 3600


def age(since_iso: str, now_iso: str) -> str:
    seconds = (_parse(now_iso) - _parse(since_iso)).total_seconds()
    minutes = int(seconds // 60)
    if minutes < 60:
        return f"{minutes}m"
    hours = minutes // 60
    if hours < 24:
        return f"{hours}h"
    return f"{hours // 24}d"


def display_width(s: str) -> int:
    return sum(2 if unicodedata.east_asian_width(ch) in "FW" else 1 for ch in s)
