"""statusline 单行:无 ANSI,零副作用。"""
from __future__ import annotations

from .model import Model


def render_oneline(model: Model) -> str:
    active_ms = next((m for m in model.milestones if m.state == "active"), None)
    active = sum(1 for t in model.tasks if t.state in ("active", "stalled"))
    done = sum(1 for t in model.tasks if t.state == "done")
    rest = len(model.tasks) - active - done
    agents = sum(1 for a in model.activity if a.kind == "agent")
    ms = f"{active_ms.id} " if active_ms else ""   # I7:带上活跃里程碑;无则整体省略
    return (f"[dash] {model.project} {ms}✓{done}▶{active}·{rest} "
            f"⚑{len(model.stalled)} ·{agents}ag")
