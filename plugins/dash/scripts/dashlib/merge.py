"""Fragment 合并:声明源(sdd) > 事件源(session) > 派生源(git)。"""
from __future__ import annotations

from typing import List

from .model import Fragment, Model, Task, age, is_stalled

PRIORITY = {"sdd": 3, "session": 2, "git": 1}


def merge(project: str, fragments: List[Fragment], barriers: List[str],
          now_iso: str, threshold_h: float = 2.0) -> Model:
    model = Model(project=project)
    by_id: dict = {}
    for frag in fragments:
        for t in frag.tasks:
            cur = by_id.get(t.id)
            if cur is None or PRIORITY[t.source] > PRIORITY[cur.source]:
                by_id[t.id] = t
        model.milestones.extend(frag.milestones)
        model.activity.extend(frag.activity)
        model.velocity.update(frag.velocity)
        model.warnings.extend(frag.warnings)
    model.tasks = list(by_id.values())
    model.barriers = barriers
    for t in model.tasks:
        if is_stalled(t, now_iso, threshold_h):
            t.state = "stalled"
            model.stalled.append(f"{t.id}({age(t.since, now_iso)})")
    return model
