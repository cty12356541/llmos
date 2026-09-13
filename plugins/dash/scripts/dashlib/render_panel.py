# plugins/dash/scripts/dashlib/render_panel.py
"""终端面板:区块 A 在跑/健康 → B 轨迹 → C 车道(SDD 源)。ANSI,只读。"""
from __future__ import annotations

from typing import Optional

from .model import Model, Task, age, display_width

C = {"done": "\033[32m", "active": "\033[34m", "pending": "\033[90m",
     "blocked": "\033[90m", "stalled": "\033[33m", "end": "\033[0m", "bold": "\033[1m"}
MARK = {"done": "✓", "active": "▶", "pending": "·", "blocked": "·", "stalled": "⚑"}


def _task_line(t: Task, now_iso: str) -> str:
    suffix = f" · {age(t.since, now_iso)}" if t.since and t.state in ("active", "stalled") else ""
    note = f" · {t.note}" if t.note else ""
    return f"{C[t.state]}{MARK[t.state]} {t.id} {t.label}{note}{suffix}{C['end']}"


def render_panel(model: Model, focus: Optional[str], now_iso: str, width: int = 64) -> str:
    active_ms = next((m for m in model.milestones if m.state == "active"), None)
    counts = {s: sum(1 for t in model.tasks if t.state == s)
              for s in ("done", "active", "stalled", "pending", "blocked")}
    act_n = sum(1 for a in model.activity if a.kind == "agent")
    lines = [
        f"{model.project} · {active_ms.id} {active_ms.title}" if active_ms else f"{model.project}",
        (f"{C['done']}✓{counts['done']}{C['end']} {C['active']}▶{counts['active'] + counts['stalled']}{C['end']} "
         f"{C['pending']}·{counts['pending'] + counts['blocked']}{C['end']} "
         f"{C['stalled']}⚑{counts['stalled']}{C['end']} · {act_n} agents · "
         f"{model.velocity.get('commits_7d', 0)}c/7d   {now_iso[5:16]}"),
        "═" * width,
    ]
    if focus:
        t = next((t for t in model.tasks if t.id == focus), None)
        lines.append(f"{C['bold']}▸聚焦 {focus}{C['end']}   c散焦 ⏎送对话")
        if t:
            meta = f" · {t.lane} · 源:{t.source}" + (f" · {age(t.since, now_iso)}" if t.since else "")
            lines.append(_task_line(t, now_iso) + meta)
        rest = f"✓{counts['done']} ▶{counts['active'] + counts['stalled']} ·{counts['pending'] + counts['blocked']}"
        lines.append(f"── 其余: {rest} · {act_n} agents ──")
        return "\n".join(lines)
    # 区块 A:在跑/健康
    lines.append(f"{C['bold']}在跑 / 健康{C['end']}")
    for a in model.activity:
        lines.append(f"{C['active']}▶ {a.label} · {age(a.since, now_iso)}{C['end']}")
    for s in model.stalled:
        lines.append(f"{C['stalled']}⚑ {s}{C['end']}")
    if not model.activity and not model.stalled:
        lines.append(f"{C['done']}✓ 无活跃/卡死{C['end']}")
    for w in model.warnings:
        lines.append(f"\033[33m⚠ {w}{C['end']}")
    lines.append("─" * width)
    # 区块 B:轨迹
    done_ms = sum(1 for m in model.milestones if m.state == "done")
    lines.append(f"{C['bold']}轨迹 · {done_ms} 里程碑{C['end']}")
    for m in model.milestones[-5:]:
        if m.tasks_total:
            fill = "▓" * round(10 * m.tasks_done / m.tasks_total)
            bar = f"{fill}{'░' * (10 - len(fill))} {m.tasks_done}/{m.tasks_total}"
        else:
            bar = ""
        lines.append(f"  {C['done'] if m.state == 'done' else C['active']}{m.id} {m.title} {bar} {m.state}{C['end']}")
    planned = [m for m in model.milestones if m.state == "planned"]
    lines.append(f"  下一步:待启动 {planned[0].id}" if planned and not active_ms else "  进行中")
    lines.append("─" * width)
    # 区块 C:车道/任务(仅 sdd 源)
    sdd_tasks = [t for t in model.tasks if t.source == "sdd"]
    if sdd_tasks:
        lines.append(f"{C['bold']}车道 / 任务(SDD 源){C['end']}")
        lanes = {}
        for t in sdd_tasks:
            lanes.setdefault(t.lane, []).append(t)
        cols = [f"{C['bold']}{name}{C['end']}\n" + "\n".join(_task_line(t, now_iso) for t in ts)
                for name, ts in lanes.items()]
        lines.extend(cols)   # v1 纵向列出;列排版留待性能允许时优化
        # 注:brief 原文 "\n".join(c) 对字符串逐字符插换行(自测即挂),改为直接铺列
        for b in model.barriers:
            lines.append(f"  {b}")
    return "\n".join(lines)
