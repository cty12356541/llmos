"""provider usage 字段探测：把不可信的原始 usage JSON 解析为类型化 UsageProbe。

边界解析原则（与 exp1 config.py 一致）：原始 usage dict 是不可信输入，
在此模块一次性解析为类型化值，计费折算器只接收 UsageProbe，不再碰原始 dict。

覆盖三种真实 provider 字段情形：
- DeepSeek 风格：prompt_cache_hit_tokens / prompt_cache_miss_tokens（hit + miss = prompt_tokens）
- OpenAI 风格：prompt_tokens_details.cached_tokens（cached 是 prompt_tokens 的子集）
- 无缓存字段：只报 prompt_tokens / completion_tokens / total_tokens → cached_tokens=0 优雅降级
"""

from __future__ import annotations

import enum
from dataclasses import dataclass
from typing import Any


class CacheFieldKind(enum.Enum):
    """usage 中缓存命中字段的风格。"""

    DEEPSEEK = "deepseek"  # prompt_cache_hit_tokens
    OPENAI = "openai"  # prompt_tokens_details.cached_tokens
    NONE = "none"  # 无缓存字段


@dataclass(frozen=True, slots=True)
class UsageProbe:
    """一次调用 usage 的类型化探测结果（计费折算的唯一事实来源）。

    统一语义：cached_tokens 是 prompt_tokens 中被缓存命中的子集，
    uncached = prompt_tokens - cached_tokens。两种 provider 风格都归入此语义。
    """

    prompt_tokens: int
    completion_tokens: int
    cached_tokens: int
    field_kind: CacheFieldKind

    @property
    def uncached_prompt_tokens(self) -> int:
        return self.prompt_tokens - self.cached_tokens


def _as_non_negative_int(value: object) -> int | None:
    """宽松解析非负整数；非法值返回 None（由调用方决定降级）。"""
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    n = int(value)
    return n if n >= 0 else None


def probe_usage(usage: dict[str, Any]) -> UsageProbe:
    """解析原始 usage JSON 为 UsageProbe。

    usage 必须存在且含 prompt_tokens / completion_tokens（协议约定的事实来源）；
    缓存字段缺失时不报错，按 NONE 降级为 cached_tokens=0。
    命中量被钳制在 [0, prompt_tokens]，防御 provider 回报自相矛盾的数据。
    """
    prompt_tokens = _as_non_negative_int(usage.get("prompt_tokens"))
    completion_tokens = _as_non_negative_int(usage.get("completion_tokens"))
    if prompt_tokens is None or completion_tokens is None:
        raise ValueError(f"usage 缺少 prompt_tokens/completion_tokens: {sorted(usage.keys())}")

    cached: int | None = None
    kind = CacheFieldKind.NONE

    hit = _as_non_negative_int(usage.get("prompt_cache_hit_tokens"))
    if hit is not None:
        # DeepSeek 风格优先（hit/miss 语义最直接）
        cached, kind = hit, CacheFieldKind.DEEPSEEK
    else:
        details = usage.get("prompt_tokens_details")
        if isinstance(details, dict):
            openai_cached = _as_non_negative_int(details.get("cached_tokens"))
            if openai_cached is not None:
                cached, kind = openai_cached, CacheFieldKind.OPENAI

    if cached is None:
        cached = 0
    # 钳制：命中量不可能超过 prompt 总量（防御矛盾回报）
    cached = min(cached, prompt_tokens)

    return UsageProbe(
        prompt_tokens=prompt_tokens,
        completion_tokens=completion_tokens,
        cached_tokens=cached,
        field_kind=kind,
    )
