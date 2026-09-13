"""Mermaid 视图 + 台账注入(承接 scripts/render_dag.py 语义;唯一显式写副作用)。"""
from __future__ import annotations

import re
from pathlib import Path

from .model import Model

CLS = {"done": "done", "active": "active", "stalled": "active",
       "pending": "pending", "blocked": "pending"}


def render_mermaid(model: Model) -> str:
    out = ["```mermaid", "flowchart LR"]
    lanes = {}
    for t in model.tasks:
        if t.source != "sdd":
            continue
        if t.lane == "无车道":    # 无车道不进子图,只在下方作顶层节点(承接 render_dag 修复语义)
            continue
        lanes.setdefault(t.lane, []).append(t)
    for name, ts in lanes.items():
        sid = re.sub(r"\W", "", name) or "L"
        out.append(f"  subgraph {sid}[\"{name}\"]")
        for t in ts:
            label = t.label.replace('"', "'")
            out.append(f"    {t.id}[\"{t.id} {label}\"]:::{CLS[t.state]}")
        out.append("  end")
    for t in model.tasks:      # 无车道任务:顶层节点,带状态类(不落任何子图)
        if t.source == "sdd" and t.lane == "无车道":
            label = t.label.replace('"', "'")
            out.append(f"  {t.id}[\"{t.id} {label}\"]:::{CLS[t.state]}")
    for line in model.barriers:
        m = re.match(r"屏障 \S+: (.+) → (.+)", line)
        if m:
            for a in m.group(1).split("+"):
                for u in m.group(2).split():
                    out.append(f"  {a.strip()} -.-> {u.strip()}")
    out += ['  classDef done fill:#2ea04326,stroke:#2ea043',
            '  classDef active fill:#388bfd26,stroke:#58a6ff',
            '  classDef pending fill:#afb8c133,stroke:#8b949e', "```"]
    return "\n".join(out)


def inject_mermaid(workspace: Path, section: str) -> None:
    path = workspace / "progress.md"
    text = path.read_text(encoding="utf-8") if path.exists() else ""
    block = f"<!-- dag:begin -->\n{section}\n<!-- dag:end -->"
    if "<!-- dag:begin -->" in text:
        # lambda 替换:block 按字面写入,不解释 \1 等反斜杠转义(内容含 \ 时 re.sub 字符串替换会抛/错写)
        text = re.sub(r"<!-- dag:begin -->.*?<!-- dag:end -->", lambda _: block, text, flags=re.S)
    else:
        text = text.rstrip() + "\n\n## DAG\n\n" + block + "\n"
    path.write_text(text, encoding="utf-8")
