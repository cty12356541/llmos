"""HTML 静态快照:单文件、内联 CSS、零外链零 JS、双主题。"""
from __future__ import annotations

import html as _html

from .model import Model

CSS = """
:root{font-family:ui-monospace,Menlo,monospace;background:#0d1117;color:#c9d1d9}
@media (prefers-color-scheme:light){:root{background:#fff;color:#24292f}}
body{max-width:48rem;margin:2rem auto;padding:0 1rem}
section{margin:1.5rem 0}h2{border-bottom:1px solid #30363d;padding-bottom:.3rem}
.done{color:#2ea043}.active{color:#58a6ff}.stalled{color:#d29922}.pending{color:#8b949e}
"""


def render_html(model: Model, now_iso: str) -> str:
    def li(t):
        cls = t.state if t.state in ("done", "active", "stalled") else "pending"
        return f'<li class="{cls}">{_html.escape(t.id)} {_html.escape(t.label)}({_html.escape(t.state)})</li>'
    ms = "".join(
        f'<li>{_html.escape(m.id)} {_html.escape(m.title)} — {m.state} '
        f'({m.tasks_done}/{m.tasks_total})</li>' for m in model.milestones)
    acts = "".join(f"<li>{_html.escape(a.label)}</li>" for a in model.activity) or "<li>无</li>"
    tasks = "".join(li(t) for t in model.tasks) or "<li>无</li>"
    warns = "".join(f"<li>⚠ {_html.escape(w)}</li>" for w in model.warnings)
    return f"""<!DOCTYPE html>
<html lang="zh"><head><meta charset="utf-8"><title>dash · {_html.escape(model.project)}</title>
<style>{CSS}</style></head><body>
<h1>dash · {_html.escape(model.project)} <small>{now_iso}</small></h1>
<section><h2>在跑 / 健康</h2><ul>{acts}</ul><ul>{warns}</ul></section>
<section><h2>轨迹</h2><ul>{ms}</ul></section>
<section><h2>任务</h2><ul>{tasks}</ul></section>
</body></html>"""
