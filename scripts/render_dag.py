#!/usr/bin/env python3
"""Render a wave's task DAG from the SDD ledger.

Inputs (inside a workspace dir, default: newest under .superpowers/sdd/):
  - dag.json    declarative wave structure: lanes, task labels, barriers
  - progress.md SDD ledger; status resolved from `Task <N>: ...` lines:
      complete    -> done      (green)
      dispatched / in-review / in-progress / fix round -> active (blue)
      otherwise   -> pending   (gray)

Outputs:
  - stdout:      ANSI lane view for the terminal
  - dag.md:      Mermaid flowchart (GitHub renders natively)
  - progress.md: `## DAG` section refreshed between <!-- dag:begin/end -->
"""
from __future__ import annotations

import json
import re
import sys
from datetime import datetime
from pathlib import Path

ANSI = {"done": "\033[32m", "active": "\033[34m", "pending": "\033[90m", "end": "\033[0m"}
MARKS = {"done": "✓", "active": "▶", "pending": "·"}
ACTIVE_WORDS = ("dispatched", "in-review", "in-progress", "fix round")


def resolve_status(ledger: str, n: int) -> str:
    for line in ledger.splitlines():
        m = re.match(rf"Task {n}\s*: complete", line)
        if m:
            return "done"
        m = re.match(rf"Task {n}\s*: (\S[^;(]*)", line)
        if m and any(w in m.group(1) for w in ACTIVE_WORDS):
            return "active"
    return "pending"


def load(workspace: Path) -> tuple[dict, str]:
    dag = json.loads((workspace / "dag.json").read_text(encoding="utf-8"))
    ledger_path = workspace / "progress.md"
    ledger = ledger_path.read_text(encoding="utf-8") if ledger_path.exists() else ""
    return dag, ledger


def terminal_view(dag: dict, status: dict[int, str]) -> str:
    counts = {s: sum(1 for v in status.values() if v == s) for s in ("done", "active", "pending")}
    header = (
        f"{dag['wave']} {dag['title']}  "
        f"\033[32m✓{counts['done']}\033[0m "
        f"\033[34m▶{counts['active']}\033[0m "
        f"\033[90m·{counts['pending']}\033[0m   {datetime.now():%m-%d %H:%M}"
    )
    width = 64
    lines = [header, "─" * width]
    lanes = dag["lanes"]
    rows = []
    for lane in lanes:
        col = [f"\033[1m{lane['name']}\033[0m"]
        for n in lane["tasks"]:
            s = status[n]
            label = dag["tasks"][str(n)]["label"]
            col.append(f"{ANSI[s]}{MARKS[s]} T{n} {label}{ANSI['end']}")
        rows.append(col)
    for i in range(max(len(c) for c in rows)):
        cells = []
        for col, lane in zip(rows, lanes):
            cell = col[i] if i < len(col) else ""
            pad = " " * max(0, 20 - len(re.sub(r"\033\[[0-9;]*m", "", cell)))
            cells.append(cell + pad)
        lines.append("   ".join(cells).rstrip())
    lines.append("─" * width)
    for b in dag.get("barriers", []):
        gate = "+".join(f"T{n}" for n in b["after"])
        unlocks = "→ " + " ".join(f"T{n}" for n in b["unlocks"])
        lines.append(f"屏障 {b['id']}: {gate} {unlocks}")
    return "\n".join(lines)


def mermaid(dag: dict, status: dict[int, str]) -> str:
    style = {"done": "fill:#2ea04326,stroke:#2ea043", "active": "fill:#388bfd26,stroke:#58a6ff",
             "pending": "fill:#afb8c133,stroke:#8b949e"}
    cls = {"done": "done", "active": "active", "pending": "pending"}
    out = ["```mermaid", "flowchart LR"]
    for lane in dag["lanes"]:
        out.append(f"  subgraph {lane['id']}[{lane['name']}]")
        prev = None
        for n in lane["tasks"]:
            label = dag["tasks"][str(n)]["label"].replace('"', "'")
            out.append(f"    T{n}[\"T{n} {label}\"]:::{cls[status[n]]}")
            if prev is not None:
                out.append(f"    T{prev} --> T{n}")
            prev = n
        out.append("  end")
    for b in dag.get("barriers", []):
        after, unlocks = b["after"], b["unlocks"]
        if len(unlocks) == 1:
            for a in after:
                out.append(f"  T{a} -.-> T{unlocks[0]}")
        else:  # barrier node joins a fan-out
            out.append(f"  B{b['id']}{{\"屏障 {b['id']}\"}}")
            for a in after:
                out.append(f"  T{a} -.-> B{b['id']}")
            for u in unlocks:
                out.append(f"  B{b['id']} -.-> T{u}")
    out.append("  classDef done " + style["done"])
    out.append("  classDef active " + style["active"])
    out.append("  classDef pending " + style["pending"])
    out.append("```")
    return "\n".join(out)


def inject(workspace: Path, section: str) -> None:
    ledger_path = workspace / "progress.md"
    text = ledger_path.read_text(encoding="utf-8") if ledger_path.exists() else ""
    block = f"<!-- dag:begin -->\n{section}\n<!-- dag:end -->"
    if "<!-- dag:begin -->" in text:
        text = re.sub(r"<!-- dag:begin -->.*?<!-- dag:end -->", block, text, flags=re.S)
    else:
        text = text.rstrip() + "\n\n## DAG\n\n" + block + "\n"
    ledger_path.write_text(text, encoding="utf-8")


def main() -> None:
    root = Path(__file__).resolve().parent.parent
    if len(sys.argv) > 1:
        workspace = Path(sys.argv[1])
    else:
        candidates = sorted((root / ".superpowers" / "sdd").glob("*/dag.json"))
        if not candidates:
            sys.exit("no workspace with dag.json under .superpowers/sdd/")
        workspace = candidates[-1].parent
    dag, ledger = load(workspace)
    status = {n: resolve_status(ledger, n) for n in (int(k) for k in dag["tasks"])}
    print(terminal_view(dag, status))
    (workspace / "dag.md").write_text(mermaid(dag, status) + "\n", encoding="utf-8")
    inject(workspace, mermaid(dag, status))
    print(f"\nmermaid → {workspace / 'dag.md'}(并已注入 progress.md 的 ## DAG 节)")


if __name__ == "__main__":
    main()
