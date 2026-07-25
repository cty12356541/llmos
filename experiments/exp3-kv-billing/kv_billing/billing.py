"""计费映射：usage → credits 折算器（议题 11"按驱动回报实际成本记账"的原型化）。

折算规则（usage 各字段 → credits）：
- 未命中 prompt token（prompt_tokens - cached_tokens）→ 按 prompt_per_1k 全价
- 缓存命中 prompt token（cached_tokens）→ 按 cached_prompt_per_1k 折扣价；
  定价表未配置折扣价时按 prompt_per_1k 全价（降级规则，绝不擅自打折）
- completion token → 按 completion_per_1k 全价（completion 无缓存概念）
- provider 无任何缓存字段（CacheFieldKind.NONE）→ cached_tokens=0，全部全价

与 exp1 budget.py settle() 的关系：exp1 的折算等价于本模块 cached_tokens=0
的特例。本模块是其超集——若驱动能回报命中量，预算扣减即可改为按实际成本。
"""

from __future__ import annotations

from dataclasses import dataclass

from .pricing import ModelPrice
from .usage_probe import CacheFieldKind, UsageProbe


@dataclass(frozen=True, slots=True)
class ChargeBreakdown:
    """一次调用的折算明细：各分量成本与合计，供对账与测量对比表使用。"""

    prompt_tokens: int
    completion_tokens: int
    cached_tokens: int
    uncached_prompt_tokens: int
    field_kind: CacheFieldKind
    uncached_prompt_cost: float
    cached_prompt_cost: float
    completion_cost: float
    total_cost: float
    #: 命中部分实际使用的单价（每 1k token credits）；等于全价即发生了降级
    applied_cached_price_per_1k: float

    def full_price_baseline(self, price: ModelPrice) -> float:
        """对照基线：同一份 usage 完全不考虑缓存时的全价成本。"""
        return (
            self.prompt_tokens * price.prompt_per_1k / 1000.0
            + self.completion_tokens * price.completion_per_1k / 1000.0
        )


def charge_usage(probe: UsageProbe, price: ModelPrice) -> ChargeBreakdown:
    """按 UsageProbe + ModelPrice 折算 credits。纯函数，无副作用。"""
    uncached_cost = probe.uncached_prompt_tokens * price.prompt_per_1k / 1000.0
    cached_price = price.effective_cached_prompt_per_1k
    cached_cost = probe.cached_tokens * cached_price / 1000.0
    completion_cost = probe.completion_tokens * price.completion_per_1k / 1000.0
    return ChargeBreakdown(
        prompt_tokens=probe.prompt_tokens,
        completion_tokens=probe.completion_tokens,
        cached_tokens=probe.cached_tokens,
        uncached_prompt_tokens=probe.uncached_prompt_tokens,
        field_kind=probe.field_kind,
        uncached_prompt_cost=uncached_cost,
        cached_prompt_cost=cached_cost,
        completion_cost=completion_cost,
        total_cost=uncached_cost + cached_cost + completion_cost,
        applied_cached_price_per_1k=cached_price,
    )


def charge_raw_usage(usage: dict[str, object], price: ModelPrice) -> ChargeBreakdown:
    """便捷入口：原始 usage dict → 探测 → 折算，一步完成。"""
    from .usage_probe import probe_usage

    return charge_usage(probe_usage(usage), price)
