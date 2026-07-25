"""离线测量：对缓存感知 mock 跑三种字段风格的同前缀连续调用，输出对比表。

运行：uv run python scripts/measure_mock.py
产物：results/measurement_mock.json（含每次调用的原始 usage 与折算明细）
"""

from __future__ import annotations

import asyncio
import json
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from kv_billing.measure import format_comparison_table, run_measurement
from kv_billing.pricing import load_pricing
from kv_billing.providers.mock import CacheAwareMockProvider, CacheStyle

RESULTS_FILE = Path(__file__).resolve().parent.parent / "results" / "measurement_mock.json"

QUESTIONS = [
    "这段系统提示的主旨是什么？一句话回答。",
    "把系统提示里提到的计费规则总结成两点。",
    "系统提示中出现的那句英文绕口令是什么？",
]

STYLES: list[CacheStyle] = ["deepseek", "openai", "none"]


async def main() -> None:
    pricing = load_pricing()
    price = pricing.price_for("mock-model")
    report: dict[str, object] = {}
    for style in STYLES:
        provider = CacheAwareMockProvider(cache_style=style)
        rows = await run_measurement(provider, "mock-model", price, QUESTIONS)
        print(f"\n=== cache_style={style} ===")
        print(format_comparison_table(rows, price))
        report[style] = [row.to_dict() for row in rows]
        await provider.aclose()
    RESULTS_FILE.parent.mkdir(parents=True, exist_ok=True)
    RESULTS_FILE.write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
    print(f"\n原始 usage 与折算明细已写入 {RESULTS_FILE}")


if __name__ == "__main__":
    asyncio.run(main())
