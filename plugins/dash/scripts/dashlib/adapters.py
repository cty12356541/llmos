# plugins/dash/scripts/dashlib/adapters.py
"""适配器层:session(事件重放)/ git(快照派生)/ sdd(台账深语义)。

v1 简化注记:todo 快照 summary 只带 in_progress 项,同快照内无法区分
pending/completed 的细粒度——快照间消失即 done,存在即 active。
"""
from __future__ import annotations

import json
from pathlib import Path

from .model import Activity, Fragment, Task


def _iter_events(state_path: Path):
    if not state_path.exists():
        return
    try:
        text = state_path.read_text(encoding="utf-8")
    except OSError:
        return
    for line in text.splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            yield json.loads(line)
        except json.JSONDecodeError:
            continue  # 截断语义:只取完整行


def load_session(state_path: Path, now_iso: str) -> Fragment:
    frag = Fragment(source="session")
    todo_first: dict = {}     # (session, idx) -> first ts
    todo_seen: dict = {}      # session -> {idx: label}
    prev_labels: dict = {}    # session -> [labels]
    agents: dict = {}         # session|summary -> Activity
    for ev in _iter_events(state_path):
        sess, kind = ev.get("session", ""), ev.get("kind", "")
        ts, summary = ev.get("ts", ""), ev.get("summary", "")
        if kind == "todo":
            labels = [s for s in summary.split(";") if s]
            for idx, label in enumerate(labels):
                todo_first.setdefault((sess, idx), ts)
            prev_labels[sess] = labels
        elif kind == "agent":
            if ev.get("event") == "spawned":
                agents[(sess, summary)] = Activity("agent", summary, ts)
            else:
                agents.pop((sess, summary), None)
        elif kind == "stop":
            for key in agents:
                if key[0] == sess:
                    agents[key].last_event = f"turn_end {ts[11:16]}"
    for (sess, idx), ts in sorted(todo_first.items()):
        labels = prev_labels.get(sess, [])
        if idx >= len(labels):        # 后续快照消失 → 完成
            state = "done"
        else:
            state = "active"
        frag.tasks.append(Task(id=f"todo-{sess}-{idx}", label=labels[idx] if idx < len(labels) else f"#{idx}",
                               state=state, lane="会话", since=ts, source="session"))
    frag.activity = list(agents.values())
    return frag
