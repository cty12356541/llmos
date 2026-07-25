"""定价表测试：cached_prompt_per_1k 扩展与降级语义。"""

from __future__ import annotations

from kv_billing.pricing import ModelPrice, load_pricing


class TestLoadPricing:
    def test_loads_cached_price_when_configured(self) -> None:
        # Given: 仓库内 pricing.yaml
        # When: 加载
        table = load_pricing()
        # Then: mock-model 带折扣价（命中价 = 原价 1/10）
        price = table.price_for("mock-model")
        assert price.prompt_per_1k == 500.0
        assert price.cached_prompt_per_1k == 50.0
        assert price.effective_cached_prompt_per_1k == 50.0

    def test_effective_price_falls_back_to_full_when_cached_missing(self) -> None:
        # Given: 仓库内 pricing.yaml 的 no-cache-model（无 cached 条目）
        # When: 加载
        table = load_pricing()
        price = table.price_for("no-cache-model")
        # Then: cached 为 None，effective 回落至全价（降级规则）
        assert price.cached_prompt_per_1k is None
        assert price.effective_cached_prompt_per_1k == price.prompt_per_1k

    def test_default_entry_used_for_unknown_model(self) -> None:
        # Given: 未列出的模型名
        # When: 查价
        table = load_pricing()
        price = table.price_for("never-heard-of-it")
        # Then: 回落 default 条目
        assert price == table.default


class TestModelPrice:
    def test_effective_cached_price_prefers_configured_discount(self) -> None:
        # Given: 显式配置折扣价
        price = ModelPrice(prompt_per_1k=500.0, completion_per_1k=1000.0, cached_prompt_per_1k=250.0)
        # When/Then: effective 取折扣价
        assert price.effective_cached_prompt_per_1k == 250.0
