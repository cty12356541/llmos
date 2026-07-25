"""定价表：每 1k token 的 credits 单价，含缓存命中折扣价（exp3 扩展）。

与 exp1 的差异：ModelPrice 增加 cached_prompt_per_1k（可缺省）。
缺省语义 = 降级规则：即使 provider 回报了缓存命中，命中部分也按全价计——
定价表未配置折扣价时绝不"擅自打折"，折扣必须是显式配置的商业决定。
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

import yaml

PROJECT_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_PRICING_FILE = PROJECT_ROOT / "config" / "pricing.yaml"


@dataclass(frozen=True, slots=True)
class ModelPrice:
    """单个模型的计价：每 1k token 的 credits 单价。

    cached_prompt_per_1k 为 None 表示该模型未配置缓存折扣价，
    命中部分按 prompt_per_1k 全价折算（降级规则）。
    """

    prompt_per_1k: float
    completion_per_1k: float
    cached_prompt_per_1k: float | None = None

    @property
    def effective_cached_prompt_per_1k(self) -> float:
        """命中部分的实际折算单价：未配置折扣价时回落至全价。"""
        if self.cached_prompt_per_1k is None:
            return self.prompt_per_1k
        return self.cached_prompt_per_1k


@dataclass(frozen=True, slots=True)
class PricingTable:
    """定价表：模型名 → 单价；default 兜底。"""

    prices: dict[str, ModelPrice]
    default: ModelPrice

    def price_for(self, model: str) -> ModelPrice:
        return self.prices.get(model, self.default)


def load_pricing(path: Path | None = None) -> PricingTable:
    """解析定价 YAML 为 PricingTable。cached_prompt_per_1k 可缺省。"""
    path = path or DEFAULT_PRICING_FILE
    raw = yaml.safe_load(path.read_text(encoding="utf-8"))
    models: dict[str, object] = raw.get("models", {})
    if "default" not in models:
        raise ValueError(f"定价表缺少 default 条目: {path}")
    prices: dict[str, ModelPrice] = {}
    for name, entry in models.items():
        if not isinstance(entry, dict):
            raise ValueError(f"定价表条目非法: {name}")
        cached_raw = entry.get("cached_prompt_per_1k")
        prices[str(name)] = ModelPrice(
            prompt_per_1k=float(entry["prompt_per_1k"]),
            completion_per_1k=float(entry["completion_per_1k"]),
            cached_prompt_per_1k=float(cached_raw) if cached_raw is not None else None,
        )
    return PricingTable(prices=prices, default=prices["default"])
