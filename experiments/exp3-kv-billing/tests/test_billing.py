"""计费折算测试：三种字段情形下的折算正确性、降级规则、折扣计算、定价表组合。"""

from __future__ import annotations

import pytest

from kv_billing.billing import charge_raw_usage, charge_usage
from kv_billing.pricing import ModelPrice, load_pricing
from kv_billing.usage_probe import CacheFieldKind, UsageProbe

PRICE = ModelPrice(prompt_per_1k=500.0, completion_per_1k=1000.0, cached_prompt_per_1k=50.0)


class TestChargeWithCacheHit:
    def test_splits_prompt_into_uncached_full_price_and_cached_discount(self) -> None:
        # Given: 3000 prompt tokens 其中 2500 命中（DeepSeek 风格探测结果）
        probe = UsageProbe(
            prompt_tokens=3000, completion_tokens=32,
            cached_tokens=2500, field_kind=CacheFieldKind.DEEPSEEK,
        )
        # When: 折算
        charge = charge_usage(probe, PRICE)
        # Then: 未命中 500×0.5 + 命中 2500×0.05 + completion 32×1.0
        assert charge.uncached_prompt_cost == pytest.approx(250.0)
        assert charge.cached_prompt_cost == pytest.approx(125.0)
        assert charge.completion_cost == pytest.approx(32.0)
        assert charge.total_cost == pytest.approx(407.0)

    def test_openai_style_hit_uses_same_unified_semantics(self) -> None:
        # Given: OpenAI 风格探测结果（cached 为子集）
        probe = UsageProbe(
            prompt_tokens=3000, completion_tokens=32,
            cached_tokens=2560, field_kind=CacheFieldKind.OPENAI,
        )
        # When: 折算
        charge = charge_usage(probe, PRICE)
        # Then: 与 deepseek 风格同一折算语义
        assert charge.uncached_prompt_tokens == 440
        assert charge.cached_prompt_cost == pytest.approx(2560 * 50.0 / 1000.0)

    def test_hit_call_is_cheaper_than_full_price_baseline(self) -> None:
        # Given: 有命中的 usage
        probe = UsageProbe(
            prompt_tokens=3000, completion_tokens=32,
            cached_tokens=2500, field_kind=CacheFieldKind.DEEPSEEK,
        )
        # When: 折算并对照全价基线
        charge = charge_usage(probe, PRICE)
        baseline = charge.full_price_baseline(PRICE)
        # Then: 实际成本 = 基线 - 命中量×(全价-折扣价)/1k
        assert baseline == pytest.approx(3000 * 0.5 + 32 * 1.0)
        assert charge.total_cost == pytest.approx(baseline - 2500 * (500.0 - 50.0) / 1000.0)


class TestChargeDegradation:
    def test_no_cache_fields_charges_full_price(self) -> None:
        # Given: 无缓存字段（NONE 降级）
        probe = UsageProbe(
            prompt_tokens=3000, completion_tokens=32,
            cached_tokens=0, field_kind=CacheFieldKind.NONE,
        )
        # When: 折算
        charge = charge_usage(probe, PRICE)
        # Then: 全部全价，与 exp1 settle() 的折算口径一致
        assert charge.total_cost == pytest.approx(3000 * 0.5 + 32 * 1.0)

    def test_hit_with_unconfigured_discount_falls_back_to_full_price(self) -> None:
        # Given: provider 回报了命中，但定价表未配置折扣价（降级规则）
        price_no_discount = ModelPrice(prompt_per_1k=500.0, completion_per_1k=1000.0)
        probe = UsageProbe(
            prompt_tokens=3000, completion_tokens=32,
            cached_tokens=2500, field_kind=CacheFieldKind.DEEPSEEK,
        )
        # When: 折算
        charge = charge_usage(probe, price_no_discount)
        # Then: 命中部分按全价，总价 = 无缓存情形（绝不擅自打折）
        assert charge.applied_cached_price_per_1k == 500.0
        assert charge.total_cost == pytest.approx(3000 * 0.5 + 32 * 1.0)


class TestChargeWithPricingTable:
    def test_mock_model_half_price_openai_style_discount(self) -> None:
        # Given: 仓库定价表中 openai-style-model（命中价 = 原价 1/2）
        table = load_pricing()
        price = table.price_for("openai-style-model")
        usage = {
            "prompt_tokens": 2000, "completion_tokens": 10,
            "prompt_tokens_details": {"cached_tokens": 1000},
        }
        # When: 一步折算
        charge = charge_raw_usage(usage, price)
        # Then: 未命中 1000×0.5 + 命中 1000×0.25 + completion 10×1.0
        assert charge.total_cost == pytest.approx(500.0 + 250.0 + 10.0)

    def test_deepseek_style_end_to_end_from_raw_usage(self) -> None:
        # Given: DeepSeek 风格原始 usage + mock-model 定价（1/10 命中价）
        table = load_pricing()
        price = table.price_for("mock-model")
        usage = {
            "prompt_tokens": 3000, "completion_tokens": 32,
            "prompt_cache_hit_tokens": 2500, "prompt_cache_miss_tokens": 500,
        }
        # When: 一步折算
        charge = charge_raw_usage(usage, price)
        # Then: 与分量手算一致
        assert charge.field_kind is CacheFieldKind.DEEPSEEK
        assert charge.total_cost == pytest.approx(250.0 + 125.0 + 32.0)
