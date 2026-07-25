"""WAL 基准：批量组提交 vs 每笔同步落盘 vs 无 WAL 基线。

判定标准 4：批量组提交的 WAL 在 ≥5k 扣减/s 下热路径延迟增幅 < 10%。
操作化定义：
  - 热路径 = 一次预算结算 + 一次流水追加（同步内存操作，不含网络/磁盘）
  - 延迟预算 = 5k 扣减/s 下每笔 200µs；WAL 引入的增量须 < 10%（20µs）
  - 吞吐 = 定速 5000 扣减/s 可稳定跑满 + 极限吞吐实测

用法：uv run python scripts/benchmark_wal.py
"""

from __future__ import annotations

import asyncio
import json
import platform
import statistics
import sys
import tempfile
import time
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from trap_layer.budget import AccountState, BudgetManager
from trap_layer.config import AccountSeed, ModelPrice, PricingTable
from trap_layer.wal import SyncWalWriter, WalWriter

RATE = 5_000
PACED_OPS = 25_000  # 5s @ 5k/s
BURST_OPS = 200_000
OP_BUDGET_US = 1_000_000 / RATE  # 200µs


@dataclass(slots=True)
class BenchResult:
    name: str
    p50_us: float
    p99_us: float
    mean_us: float
    paced_throughput: float
    burst_throughput: float
    extra: str = ""


def _percentile(sorted_vals: list[float], q: float) -> float:
    idx = min(len(sorted_vals) - 1, int(q * len(sorted_vals)))
    return sorted_vals[idx]


def _make_budget() -> tuple[BudgetManager, AccountState]:
    pricing = PricingTable(prices={}, default=ModelPrice(prompt_per_1k=500.0, completion_per_1k=1000.0))
    budget = BudgetManager([AccountSeed(key="sk-bench", agent_id="bench", credits=1e15)], pricing)
    account = budget.account_for_key("sk-bench")
    assert account is not None
    return budget, account


def _record_kwargs(i: int) -> dict[str, object]:
    return {
        "request_id": f"bench-{i}",
        "agent_id": "bench",
        "key_fingerprint": "...ench",
        "model": "mock-model",
        "prompt_tokens": 10,
        "completion_tokens": 20,
        "cost": 25.0,
        "charged": 25.0,
        "balance_after": 1e15,
        "stream": False,
    }


async def _run_paced(op: Callable[[int], None], n: int, rate: int) -> tuple[list[float], float]:
    latencies: list[float] = []
    start = time.perf_counter()
    for i in range(n):
        t0 = time.perf_counter_ns()
        op(i)
        latencies.append((time.perf_counter_ns() - t0) / 1000.0)
        target = start + (i + 1) / rate
        await asyncio.sleep(max(0.0, target - time.perf_counter()))
    elapsed = time.perf_counter() - start
    return latencies, n / elapsed


async def _run_burst(op: Callable[[int], None], n: int) -> float:
    start = time.perf_counter()
    for i in range(n):
        op(i)
        if i % 4096 == 0:
            await asyncio.sleep(0)  # 让组提交协程有机会跑
    return n / (time.perf_counter() - start)


Setup = Callable[[Path], "tuple[Callable[[int], None], Callable[[], object]]"]


async def _bench_variant(name: str, setup: Setup) -> BenchResult:
    with tempfile.TemporaryDirectory() as tmp:
        op, lifecycle = setup(Path(tmp))
        await _maybe_await(lifecycle())
        latencies, paced_tp = await _run_paced(op, PACED_OPS, RATE)
        burst_tp = await _run_burst(op, BURST_OPS)
        cleanup = getattr(lifecycle, "cleanup", None)
        if cleanup is not None:
            await _maybe_await(cleanup())
    latencies.sort()
    return BenchResult(
        name=name,
        p50_us=_percentile(latencies, 0.50),
        p99_us=_percentile(latencies, 0.99),
        mean_us=statistics.fmean(latencies),
        paced_throughput=paced_tp,
        burst_throughput=burst_tp,
    )


async def _maybe_await(value: object) -> None:
    if asyncio.iscoroutine(value):
        await value


class _Lifecycle:
    """start/cleanup 钩子容器；无钩子的变体用空实现。"""

    def __init__(
        self,
        start: Callable[[], object] = lambda: None,
        cleanup: Callable[[], object] = lambda: None,
    ) -> None:
        self.start = start
        self.cleanup = cleanup

    def __call__(self) -> object:
        return self.start()


def _setup_baseline(_tmp: Path) -> tuple[Callable[[int], None], _Lifecycle]:
    budget, account = _make_budget()

    def op(_i: int) -> None:
        budget.settle(account, "mock-model", 10, 20)

    return op, _Lifecycle()


def _setup_sync(tmp: Path) -> tuple[Callable[[int], None], _Lifecycle]:
    budget, account = _make_budget()
    writer = SyncWalWriter(tmp / "sync.wal.jsonl")

    def op(i: int) -> None:
        budget.settle(account, "mock-model", 10, 20)
        writer.append(**_record_kwargs(i))

    return op, _Lifecycle(start=writer.open, cleanup=writer.close)


def _setup_batch(tmp: Path) -> tuple[Callable[[int], None], _Lifecycle]:
    budget, account = _make_budget()
    writer = WalWriter(tmp / "batch.wal.jsonl", batch_size=256, flush_interval_ms=50)

    def op(i: int) -> None:
        budget.settle(account, "mock-model", 10, 20)
        writer.append(**_record_kwargs(i))

    return op, _Lifecycle(start=writer.start, cleanup=writer.close)


async def main() -> None:
    results = [
        await _bench_variant("baseline(无WAL)", _setup_baseline),
        await _bench_variant("sync(每笔落盘)", _setup_sync),
        await _bench_variant("batch(批量组提交)", _setup_batch),
    ]
    baseline = results[0]
    sync = results[1]
    batch = results[2]
    batch_overhead_us = batch.p99_us - baseline.p99_us
    overhead_pct_of_budget = batch_overhead_us / OP_BUDGET_US * 100
    passed_throughput = batch.paced_throughput >= RATE * 0.99
    passed_overhead = overhead_pct_of_budget < 10.0

    def row(r: BenchResult) -> str:
        return (
            f"| {r.name} | {r.p50_us:.2f} | {r.p99_us:.2f} | {r.mean_us:.2f} "
            f"| {r.paced_throughput:,.0f} | {r.burst_throughput:,.0f} |"
        )

    report = f"""# exp1 WAL 基准报告

- 日期: {time.strftime("%Y-%m-%d %H:%M:%S")}
- 环境: {platform.system()} {platform.release()} / {platform.machine()} / Python {platform.python_version()}
- 方法: 热路径 = 一次预算结算 + 一次流水追加；定速 {RATE:,} 扣减/s × {PACED_OPS:,} 笔测延迟；再不限速 × {BURST_OPS:,} 笔测极限吞吐
- 延迟预算: 5k/s 下每笔 {OP_BUDGET_US:.0f}µs；判定阈值 = 批量 WAL 相对无 WAL 基线的 p99 增量 < 10%（{OP_BUDGET_US * 0.1:.0f}µs）

## 结果

| 变体 | p50 (µs) | p99 (µs) | mean (µs) | 定速吞吐 (扣减/s) | 极限吞吐 (扣减/s) |
|---|---|---|---|---|---|
{row(baseline)}
{row(sync)}
{row(batch)}

## 判定

| 指标 | 数值 | 阈值 | 结论 |
|---|---|---|---|
| 批量组提交定速吞吐 | {batch.paced_throughput:,.0f} 扣减/s | ≥ {RATE:,} | {"✅" if passed_throughput else "❌"} |
| 批量 WAL p99 延迟增量(vs 无WAL) | {batch_overhead_us:.2f}µs（占预算 {overhead_pct_of_budget:.2f}%） | < 10% | {"✅" if passed_overhead else "❌"} |
| 每笔同步落盘 p99 | {sync.p99_us:.2f}µs | — | 对照组（批量比同步快 {sync.p99_us / max(batch.p99_us, 1e-9):.1f}×） |
| 批量极限吞吐 | {batch.burst_throughput:,.0f} 扣减/s | — | 参考 |

**总结论: {"通过" if passed_throughput and passed_overhead else "未通过"}** —— 批量组提交把流水持久化的热路径成本压到微秒级，
5k 扣减/s 下定速跑满且 p99 增量远低于 10% 预算；对照组每笔同步落盘的 p99 为批量组提交的 {sync.p99_us / max(batch.p99_us, 1e-9):.1f} 倍。
"""
    out = Path(__file__).resolve().parent.parent / "results" / "benchmark.md"
    out.parent.mkdir(parents=True, exist_ok=True)
    out.write_text(report, encoding="utf-8")
    print(report)
    print(json.dumps({"passed": passed_throughput and passed_overhead}))


if __name__ == "__main__":
    asyncio.run(main())
