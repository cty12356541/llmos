#!/usr/bin/env python3
"""薄事件记录器:读 hook stdin JSON,追加一行事件到 .dash/state.jsonl。

铁律:自身任何失败静默吞掉,绝不阻塞会话(hooks.json 已带 || true 双保险)。
"""
from __future__ import annotations

import json
import sys
from datetime import datetime
from pathlib import Path

TOOL_KIND = {"TodoWrite": "todo", "Agent": "agent", "Task": "agent", "Stop": "stop"}
MAX_SUMMARY = 80


def _summary(payload: dict) -> str:
    todos = payload.get("tool_input", {}).get("todos")
    if todos:
        active = [t.get("content", "") for t in todos if t.get("status") == "in_progress"]
        return ";".join(active)[:MAX_SUMMARY] or ";".join(t.get("content", "") for t in todos)[:MAX_SUMMARY]
    for key in ("description", "prompt", "subject"):
        v = payload.get("tool_input", {}).get(key)
        if v:
            return str(v)[:MAX_SUMMARY]
    return ""


def main(stdin_text: str, state_path: Path) -> None:
    try:
        payload = json.loads(stdin_text or "{}")
        kind = TOOL_KIND.get(payload.get("tool_name", ""))
        if kind is None and payload.get("hook_event_name") == "Stop":
            kind = "stop"
        if kind is None:
            return
        event = {"ts": datetime.now().astimezone().isoformat(timespec="seconds"),
                 "kind": kind,
                 "event": "turn_end" if kind == "stop" else ("spawned" if kind == "agent" else "updated"),
                 "session": payload.get("session_id", ""),
                 "summary": _summary(payload)}
        state_path.parent.mkdir(parents=True, exist_ok=True)
        with state_path.open("a", encoding="utf-8") as f:
            f.write(json.dumps(event, ensure_ascii=False) + "\n")
    except Exception:
        pass  # 铁律:静默


if __name__ == "__main__":
    repo = Path(".dash")
    main(sys.stdin.read(), repo / "state.jsonl")
