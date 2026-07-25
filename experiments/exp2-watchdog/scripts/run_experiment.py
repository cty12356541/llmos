"""跑默认阈值下的完整实验，写 results/report.json + report.md。

用法：uv run python scripts/run_experiment.py
"""

from __future__ import annotations

from pathlib import Path

from report import render_markdown
from runner import run_task_set, write_json
from watchdog import load_config

ROOT = Path(__file__).resolve().parent.parent


def main() -> None:
    config = load_config(ROOT / "config" / "watchdog.yaml")
    result = run_task_set(config)
    write_json(result, ROOT / "results" / "report.json")
    md = render_markdown(result)
    (ROOT / "results").mkdir(exist_ok=True)
    (ROOT / "results" / "report.md").write_text(md, encoding="utf-8")
    print(md)


if __name__ == "__main__":
    main()
