"""阈值敏感性扫描：maxStepsWithoutProgress × maxRepeatSimilarity 权衡表。

为"阈值声明进 ELF"提供实证依据：每个组合跑全任务集，记录误报率/检出率/检出步数。

用法：uv run python scripts/threshold_scan.py
"""

from __future__ import annotations

import json
from dataclasses import replace
from pathlib import Path

from runner import run_task_set
from watchdog import WatchdogConfig, load_config

ROOT = Path(__file__).resolve().parent.parent

GRID_STEPS = (3, 4, 5, 6)
GRID_SIM = (0.80, 0.85, 0.90, 0.95)


def scan(base: WatchdogConfig) -> list[dict[str, object]]:
    rows: list[dict[str, object]] = []
    for steps in GRID_STEPS:
        for sim in GRID_SIM:
            cfg = replace(
                base, max_steps_without_progress=steps, max_repeat_similarity=sim
            )
            result = run_task_set(cfg)
            det_steps = result.detection_steps
            rows.append(
                {
                    "maxStepsWithoutProgress": steps,
                    "maxRepeatSimilarity": sim,
                    "fp_count": len(result.false_positives),
                    "fp_rate": round(result.fp_rate, 4),
                    "detection_rate": round(result.detection_rate, 4),
                    "max_detection_step": max(det_steps) if det_steps else None,
                    "boundary_failures": len(result.boundary_failures),
                }
            )
    return rows


def render(rows: list[dict[str, object]]) -> str:
    lines = [
        "# 阈值敏感性扫描（误报/检出权衡）",
        "",
        f"maxStepsWithoutProgress ∈ {GRID_STEPS} × maxRepeatSimilarity ∈ {GRID_SIM}",
        "",
        "| maxSteps | maxSim | 误报数/10 | 误报率 | 检出率/5 | 最晚检出步 | 边界误杀/3 |",
        "|---|---|---|---|---|---|---|",
    ]
    for r in rows:
        lines.append(
            f"| {r['maxStepsWithoutProgress']} | {r['maxRepeatSimilarity']:.2f} "
            f"| {r['fp_count']} | {r['fp_rate']:.0%} | {r['detection_rate']:.0%} "
            f"| {r['max_detection_step']} | {r['boundary_failures']} |"
        )
    lines += [
        "",
        "判读：检出率恒 100% 时，误报率与边界误杀最低的格子是 ELF 声明的候选；",
        "默认 (4, 0.85) 位于安全区。maxSim 过低（0.80）开始误杀慢任务；",
        "maxSteps 越小对慢任务越苛刻（progress_interval=3 的边界任务在 maxSteps=3 时达到触发线）。",
        "",
    ]
    return "\n".join(lines)


def main() -> None:
    base = load_config(ROOT / "config" / "watchdog.yaml")
    rows = scan(base)
    (ROOT / "results").mkdir(exist_ok=True)
    (ROOT / "results" / "threshold_scan.json").write_text(
        json.dumps(rows, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    md = render(rows)
    (ROOT / "results" / "threshold_scan.md").write_text(md, encoding="utf-8")
    print(md)


if __name__ == "__main__":
    main()
