# plugins/dash/scripts/dashlib/adapters.py
"""适配器层:session(事件重放)/ git(快照派生)/ sdd(台账深语义)。

v1 简化注记:todo 快照 summary 只带 in_progress 项(R10 起,空串=无在途),
以标签身份跨快照配对——最新快照存在即 active,缺席即 done。
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
    first_seen: dict = {}     # (session, label) -> first ts
    first_order: dict = {}    # session -> [labels 首现顺序]
    latest: dict = {}         # session -> 最新快照 labels(在途集合)
    agents: dict = {}         # session|summary -> Activity(插入序=生成序)
    for ev in _iter_events(state_path):
        sess, kind = ev.get("session", ""), ev.get("kind", "")
        ts, summary = ev.get("ts", ""), ev.get("summary", "")
        if kind == "todo":
            labels = [s for s in summary.split(";") if s]
            for label in labels:
                first_seen.setdefault((sess, label), ts)
                if label not in first_order.setdefault(sess, []):
                    first_order[sess].append(label)
            latest[sess] = labels        # 最新快照=在途集合(R10)
        elif kind == "agent":
            if ev.get("event") == "spawned":
                agents[(sess, summary)] = Activity("agent", summary, ts)
            elif summary:                # 带摘要:按 (session, summary) 精确配对
                agents.pop((sess, summary), None)
            else:                        # 空摘要(SubagentStop):FIFO 弹出最早仍在途的(R9)
                for key in agents:
                    if key[0] == sess:
                        agents.pop(key)
                        break
        elif kind == "stop":
            for key in agents:
                if key[0] == sess:
                    agents[key].last_event = f"turn_end {ts[11:16]}"
    for sess, labels in first_order.items():
        active = latest.get(sess, [])
        for n, label in enumerate(labels):
            state = "active" if label in active else "done"   # 最新快照缺席 → done
            frag.tasks.append(Task(id=f"todo-{sess}-{n}", label=label,
                                   state=state, lane="会话",
                                   since=first_seen[(sess, label)], source="session"))
    frag.activity = list(agents.values())
    return frag
