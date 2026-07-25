"""缓存感知 mock 测试：命中模拟的物理事实 + 测量流程的离线可测性。"""

from __future__ import annotations

import pytest

from kv_billing.measure import build_shared_prefix, run_measurement
from kv_billing.pricing import ModelPrice
from kv_billing.providers.mock import (
    CacheAwareMockProvider,
    _estimate_messages_tokens,
)
from kv_billing.usage_probe import CacheFieldKind, probe_usage

PRICE = ModelPrice(prompt_per_1k=500.0, completion_per_1k=1000.0, cached_prompt_per_1k=50.0)


def _payload(prefix: str, question: str) -> dict[str, object]:
    return {
        "model": "mock-model",
        "messages": [
            {"role": "system", "content": prefix},
            {"role": "user", "content": question},
        ],
        "max_tokens": 8,
    }


class TestCacheHitSimulation:
    @pytest.mark.parametrize("style", ["deepseek", "openai"])
    async def test_second_call_with_same_prefix_reports_hit(self, style: str) -> None:
        # Given: 同前缀的两次调用
        provider = CacheAwareMockProvider(cache_style=style)  # type: ignore[arg-type]
        prefix = build_shared_prefix(4000)
        # When: 第一次调用
        first = await provider.chat_completion(_payload(prefix, "问题一"))
        # Then: 首次未命中（hit=0，但字段存在）
        first_probe = probe_usage(first["usage"])
        assert first_probe.cached_tokens == 0
        # When: 同前缀不同问题的第二次调用
        second = await provider.chat_completion(_payload(prefix, "问题二"))
        # Then: 命中量 = 前缀估算 token 数（>0）
        second_probe = probe_usage(second["usage"])
        expected_prefix_tokens = _estimate_messages_tokens(
            [{"role": "system", "content": prefix}]
        )
        assert second_probe.cached_tokens == expected_prefix_tokens > 0

    async def test_deepseek_style_hit_plus_miss_equals_prompt_tokens(self) -> None:
        # Given: deepseek 风格 mock，同前缀第二次调用
        provider = CacheAwareMockProvider(cache_style="deepseek")
        prefix = build_shared_prefix(4000)
        await provider.chat_completion(_payload(prefix, "问题一"))
        # When: 第二次调用的 usage
        resp = await provider.chat_completion(_payload(prefix, "问题二"))
        usage = resp["usage"]
        # Then: hit + miss = prompt_tokens（DeepSeek 回报不变式）
        assert usage["prompt_cache_hit_tokens"] + usage["prompt_cache_miss_tokens"] == usage["prompt_tokens"]

    async def test_none_style_never_reports_cache_fields(self) -> None:
        # Given: none 风格 mock，同前缀两次调用
        provider = CacheAwareMockProvider(cache_style="none")
        prefix = build_shared_prefix(4000)
        await provider.chat_completion(_payload(prefix, "问题一"))
        # When: 第二次调用
        resp = await provider.chat_completion(_payload(prefix, "问题二"))
        # Then: 无任何缓存字段，探测降级为 NONE
        assert "prompt_cache_hit_tokens" not in resp["usage"]
        assert "prompt_tokens_details" not in resp["usage"]
        assert probe_usage(resp["usage"]).field_kind is CacheFieldKind.NONE

    async def test_different_prefix_does_not_hit(self) -> None:
        # Given: 两次调用前缀不同
        provider = CacheAwareMockProvider(cache_style="deepseek")
        # When: 第二次用全新前缀
        await provider.chat_completion(_payload(build_shared_prefix(4000), "问题一"))
        resp = await provider.chat_completion(_payload("完全不同的短前缀", "问题二"))
        # Then: 不命中
        assert probe_usage(resp["usage"]).cached_tokens == 0


class TestMeasurementFlow:
    async def test_shared_prefix_calls_get_cheaper_from_second_call(self) -> None:
        # Given: deepseek 风格 mock 与同前缀三次测量
        provider = CacheAwareMockProvider(cache_style="deepseek")
        questions = ["问题一", "问题二", "问题三"]
        # When: 跑测量流程
        rows = await run_measurement(provider, "mock-model", PRICE, questions)
        # Then: 第一次全价，第二、三次因命中而更便宜且成本一致
        assert rows[0].probe.cached_tokens == 0
        assert rows[1].probe.cached_tokens > 0
        assert rows[1].charge.total_cost < rows[0].charge.total_cost
        assert rows[2].charge.total_cost == pytest.approx(rows[1].charge.total_cost)

    async def test_built_prefix_meets_minimum_length_requirement(self) -> None:
        # Given/When: 默认构造的共享前缀
        prefix = build_shared_prefix()
        # Then: ≥10000 字符（≈2500 token，满足 ≥2k token 的实验要求）
        assert len(prefix) >= 10_000
