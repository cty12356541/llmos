"""Experiment matrix runner: prepaid on/off x backpressure on/off x budget.

Run:  uv run python -m msgstorm.experiment
Outputs land in results/: summary.csv, summary_mean.csv, timeseries_<label>.csv,
volume.png, snr.png, budget_scaling.png, plus an ASCII verdict table on stdout.
"""

import csv
import math
from dataclasses import dataclass
from pathlib import Path
from statistics import mean, stdev
from typing import Final

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt

from msgstorm.model import SimConfig
from msgstorm.scenario import RunResult, ScenarioParams, run_scenario

RESULTS_DIR: Final = Path(__file__).resolve().parent.parent / "results"
SEEDS: Final = (11, 23, 37, 42, 59)
DURATION_S: Final = 200.0
BUDGETS: Final = (2_000, 10_000, 50_000)
MAIN_BUDGET: Final = 10_000
FREE_BUDGET: Final = 10**12  # effectively unbounded; billing is off anyway


@dataclass(frozen=True, slots=True)
class Config:
    """One cell of the experiment matrix."""

    label: str
    prepaid: bool
    backpressure: bool
    storm_budget: int


def build_matrix() -> list[Config]:
    """Prepaid cells vary the storm budget; free cells are budget-independent."""
    cells = [
        Config("B1 free+bp", prepaid=False, backpressure=True, storm_budget=FREE_BUDGET),
        Config("B2 free-nobp", prepaid=False, backpressure=False, storm_budget=FREE_BUDGET),
    ]
    for budget in BUDGETS:
        cells.append(
            Config(
                f"A1 prepaid+bp b{budget // 1000}k",
                prepaid=True,
                backpressure=True,
                storm_budget=budget,
            )
        )
        cells.append(
            Config(
                f"A2 prepaid-nobp b{budget // 1000}k",
                prepaid=True,
                backpressure=False,
                storm_budget=budget,
            )
        )
    return cells


def run_cell(cell: Config) -> list[RunResult]:
    sim = SimConfig(prepaid=cell.prepaid, backpressure=cell.backpressure, duration_s=DURATION_S)
    return [
        run_scenario(ScenarioParams(config=sim, storm_budget=cell.storm_budget), seed=seed)
        for seed in SEEDS
    ]


def windowed_snr(runs: list[RunResult], window_s: float = 10.0) -> tuple[list[float], list[float]]:
    """Mean windowed SNR across seeds: useful share of messages normal agents
    actually consumed inside each time window (derived from cumulative diffs)."""
    step = int(window_s)
    ts: list[float] = []
    snrs: list[float] = []
    n = min(len(r.samples) for r in runs)
    for i in range(step, n, step):
        prev = [r.samples[i - step] for r in runs]
        cur = [r.samples[i] for r in runs]
        pu = mean(c.processed_useful - p.processed_useful for c, p in zip(cur, prev, strict=True))
        ps = mean(c.processed_storm - p.processed_storm for c, p in zip(cur, prev, strict=True))
        total = pu + ps
        ts.append(runs[0].samples[i].t)
        snrs.append(pu / total if total > 0 else math.nan)
    return ts, snrs


def mean_curve(runs: list[RunResult], field: str) -> tuple[list[float], list[float]]:
    """Mean cumulative curve of a Sample field across seeds (ticks are aligned)."""
    n = min(len(r.samples) for r in runs)
    ts = [runs[0].samples[i].t for i in range(n)]
    ys = [mean(getattr(r.samples[i], field) for r in runs) for i in range(n)]
    return ts, ys


SUMMARY_FIELDS: Final = [
    "delivered_useful",
    "delivered_storm",
    "evicted_useful",
    "evicted_storm",
    "processed_useful",
    "processed_storm",
    "snr",
    "storm_capped_at_s",
    "mean_blocked_s_per_normal",
    "credits_charged",
]


def write_summary_csv(cells: dict[str, list[RunResult]]) -> None:
    with (RESULTS_DIR / "summary.csv").open("w", newline="") as f:
        writer = csv.writer(f)
        writer.writerow(["label", "seed", *SUMMARY_FIELDS])
        for label, runs in cells.items():
            for r in runs:
                writer.writerow([label, r.seed, *[getattr(r, k) for k in SUMMARY_FIELDS]])
    with (RESULTS_DIR / "summary_mean.csv").open("w", newline="") as f:
        writer = csv.writer(f)
        writer.writerow(["label", *[f"{k}_mean" for k in SUMMARY_FIELDS], "snr_std"])
        for label, runs in cells.items():
            row: list[str | float] = [label]
            for k in SUMMARY_FIELDS:
                values = [getattr(r, k) for r in runs if getattr(r, k) is not None]
                row.append(round(mean(values), 3) if values else "")
            row.append(round(stdev(r.snr for r in runs), 4))
            writer.writerow(row)


TS_FIELDS: Final = [
    "delivered_useful",
    "delivered_storm",
    "processed_useful",
    "processed_storm",
    "inbox_fill_mean",
]


def write_timeseries_csv(label: str, runs: list[RunResult]) -> None:
    safe = label.replace(" ", "_").replace("+", "")
    with (RESULTS_DIR / f"timeseries_{safe}.csv").open("w", newline="") as f:
        writer = csv.writer(f)
        writer.writerow(["t", *TS_FIELDS])
        n = min(len(r.samples) for r in runs)
        for i in range(n):
            row = [runs[0].samples[i].t]
            row += [round(mean(getattr(r.samples[i], fld) for r in runs), 3) for fld in TS_FIELDS]
            writer.writerow(row)


def plot_charts(cells: dict[str, list[RunResult]]) -> None:
    main = {k: v for k, v in cells.items() if f"b{MAIN_BUDGET // 1000}k" in k or k.startswith("B")}

    fig, ax = plt.subplots(figsize=(8, 5))
    for label, runs in main.items():
        ts, ys = mean_curve(runs, "delivered_storm")
        style = "--" if label.startswith("A1") else "-"
        ax.plot(ts, ys, style, label=label)
    ax.set(xlabel="sim time (s)", ylabel="cumulative storm messages delivered",
           title="Storm volume: prepaid caps at budget, free grows unbounded")
    ax.legend()
    fig.tight_layout()
    fig.savefig(RESULTS_DIR / "volume.png", dpi=120)
    plt.close(fig)

    fig, ax = plt.subplots(figsize=(8, 5))
    for label, runs in main.items():
        ts, snrs = windowed_snr(runs)
        ax.plot(ts, snrs, label=label)
    ax.set(xlabel="sim time (s)", ylabel="windowed SNR (10s)",
           title="Useful share of what normal agents consumed")
    ax.set_ylim(-0.02, 1.02)
    ax.legend()
    fig.tight_layout()
    fig.savefig(RESULTS_DIR / "snr.png", dpi=120)
    plt.close(fig)

    fig, ax = plt.subplots(figsize=(8, 5))
    for prefix, marker, offset in (("A1 prepaid+bp", "o", 0.98), ("A2 prepaid-nobp", "s", 1.02)):
        xs = [b * offset for b in BUDGETS]
        ys = [mean(r.delivered_storm for r in cells[f"{prefix} b{b // 1000}k"]) for b in BUDGETS]
        ax.plot(xs, ys, marker=marker, label=prefix)
    for label, color in (("B1 free+bp", "tab:red"), ("B2 free-nobp", "tab:purple")):
        free_mean = mean(r.delivered_storm for r in cells[label])
        ax.axhline(free_mean, linestyle="--", color=color, label=f"{label} (mean)")
    ax.set(xlabel="storm budget (credits)", ylabel="storm messages delivered",
           title="Delivered storm volume vs budget: linear cap under prepaid")
    ax.legend()
    fig.tight_layout()
    fig.savefig(RESULTS_DIR / "budget_scaling.png", dpi=120)
    plt.close(fig)


def print_ascii_verdict(cells: dict[str, list[RunResult]]) -> None:
    print("\n=== exp4 verdict table (mean over seeds) ===")
    header = (
        f"{'config':<26}{'storm_msgs':>12}{'useful_msgs':>13}"
        f"{'SNR':>8}{'capped@':>9}{'blocked_s':>11}"
    )
    print(header)
    print("-" * len(header))
    for label, runs in cells.items():
        storm = mean(r.delivered_storm for r in runs)
        useful = mean(r.delivered_useful for r in runs)
        snr = mean(r.snr for r in runs)
        caps = [r.storm_capped_at_s for r in runs if r.storm_capped_at_s is not None]
        capped = f"{mean(caps):.1f}s" if caps else "never"
        blocked = mean(r.mean_blocked_s_per_normal for r in runs)
        print(f"{label:<26}{storm:>12.0f}{useful:>13.0f}{snr:>8.3f}{capped:>9}{blocked:>11.1f}")

    print("\n=== storm volume trend (sparkline, mean cumulative) ===")
    for label, runs in cells.items():
        if f"b{MAIN_BUDGET // 1000}k" not in label and not label.startswith("B"):
            continue
        _, ys = mean_curve(runs, "delivered_storm")
        step = max(1, len(ys) // 60)
        print(f"{label:<26}{_sparkline(ys[::step])}  {ys[-1]:.0f}")


def _sparkline(values: list[float]) -> str:
    blocks = "▁▂▃▄▅▆▇█"
    if not values or max(values) == 0:
        return blocks[0] * len(values)
    return "".join(blocks[min(7, int(v / max(values) * 7.999))] for v in values)


def main() -> None:
    RESULTS_DIR.mkdir(exist_ok=True)
    cells: dict[str, list[RunResult]] = {}
    for cell in build_matrix():
        runs = run_cell(cell)
        cells[cell.label] = runs
        write_timeseries_csv(cell.label, runs)
        print(f"done: {cell.label}")
    write_summary_csv(cells)
    plot_charts(cells)
    print_ascii_verdict(cells)
    print(f"\nresults written to {RESULTS_DIR}")


if __name__ == "__main__":
    main()
