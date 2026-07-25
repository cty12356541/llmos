"""真实 API 实测：读 .env 凭证，对真实 provider 跑同前缀测量流程，原始 usage 落盘。

⚠️ 待用户提供转发服务凭证后运行：
  1. cp .env.example .env 并填写 LLM_BASE_URL / LLM_API_KEY / LLM_MODEL
  2. uv run python scripts/measure_real.py

产物：results/real/ 下每次调用的完整响应 usage JSON + 汇总 measurement_real.json
（results/real/ 已入 .gitignore，原始回报绝不提交）
"""

from __future__ import annotations

import asyncio
import json
import os
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

from dotenv import load_dotenv

from kv_billing.measure import format_comparison_table, run_measurement
from kv_billing.pricing import load_pricing
from kv_billing.providers.openai_compat import OpenAICompatProvider

PROJECT_ROOT = Path(__file__).resolve().parent.parent
RESULTS_DIR = PROJECT_ROOT / "results" / "real"

QUESTIONS = [
    "这段系统提示的主旨是什么？一句话回答。",
    "把系统提示里提到的计费规则总结成两点。",
    "系统提示中出现的那句英文绕口令是什么？",
]


async def main() -> None:
    load_dotenv(PROJECT_ROOT / ".env")
    base_url = os.environ.get("LLM_BASE_URL", "").strip()
    api_key = os.environ.get("LLM_API_KEY", "").strip()
    model = os.environ.get("LLM_MODEL", "").strip()
    if not (base_url and api_key and model):
        print("缺少凭证：请复制 .env.example 为 .env 并填写 LLM_BASE_URL/LLM_API_KEY/LLM_MODEL")
        sys.exit(2)

    provider = OpenAICompatProvider(base_url, api_key)
    pricing = load_pricing()
    price = pricing.price_for(model)
    try:
        rows = await run_measurement(provider, model, price, QUESTIONS)
    finally:
        await provider.aclose()

    RESULTS_DIR.mkdir(parents=True, exist_ok=True)
    for row in rows:
        (RESULTS_DIR / f"usage_call_{row.call_index}.json").write_text(
            json.dumps(row.raw_usage, ensure_ascii=False, indent=2), encoding="utf-8"
        )
    summary = {
        "model": model,
        "base_url": base_url,
        "rows": [row.to_dict() for row in rows],
    }
    (RESULTS_DIR / "measurement_real.json").write_text(
        json.dumps(summary, ensure_ascii=False, indent=2), encoding="utf-8"
    )

    print(f"=== 真实 API 实测：model={model} ===")
    print(format_comparison_table(rows, price))
    kinds = {row.probe.field_kind.value for row in rows}
    print(f"\n字段探测结论：本 provider 回报的缓存字段风格 = {sorted(kinds)}")
    print(f"原始 usage JSON 已写入 {RESULTS_DIR}/usage_call_*.json")


if __name__ == "__main__":
    asyncio.run(main())
