"""测量流程：同一长前缀连续调用，逐次采集完整 usage JSON 并折算对比。

被 scripts/measure_mock.py 与 scripts/measure_real.py 共用，
也被测试直接驱动（离线可测的保证）。
"""

from __future__ import annotations

import json
from dataclasses import dataclass
from typing import Any

from .billing import ChargeBreakdown, charge_usage
from .pricing import ModelPrice
from .providers.base import ChatProvider
from .usage_probe import UsageProbe, probe_usage

#: 共享系统前缀的最低字符数（≈2500 token，按 4 字符/token 粗估，满足 ≥2k token 要求）
DEFAULT_PREFIX_CHARS = 10_000

_PREFIX_PARAGRAPH = (
    "你是 llmos 实验 harness 的测试助手。本段是用于构造共享 KV 前缀的填充文本，"
    "模拟真实 agent 的长系统提示（工具说明、策略约束、记忆摘要）。"
    "prefix cache 命中时，这段重复出现的 prompt token 应按折扣价计费。"
    "The quick brown fox jumps over the lazy dog. "
    "Pack my box with five dozen liquor jugs. "
)


def build_shared_prefix(target_chars: int = DEFAULT_PREFIX_CHARS) -> str:
    """构造确定性的长系统前缀（重复填充段落至目标字符数）。"""
    repeats = (target_chars // len(_PREFIX_PARAGRAPH)) + 1
    prefix = _PREFIX_PARAGRAPH * repeats
    return prefix[:target_chars]


def build_payload(model: str, prefix: str, question: str, max_tokens: int = 32) -> dict[str, Any]:
    """构造 chat.completion 请求：同前缀（system）+ 不同问题（user）。"""
    return {
        "model": model,
        "messages": [
            {"role": "system", "content": prefix},
            {"role": "user", "content": question},
        ],
        "max_tokens": max_tokens,
    }


@dataclass(frozen=True, slots=True)
class MeasurementRow:
    """一次调用的测量记录：原始 usage + 探测结果 + 折算明细。"""

    call_index: int
    question: str
    raw_usage: dict[str, Any]
    probe: UsageProbe
    charge: ChargeBreakdown

    def to_dict(self) -> dict[str, Any]:
        return {
            "call_index": self.call_index,
            "question": self.question,
            "raw_usage": self.raw_usage,
            "field_kind": self.probe.field_kind.value,
            "prompt_tokens": self.probe.prompt_tokens,
            "cached_tokens": self.probe.cached_tokens,
            "completion_tokens": self.probe.completion_tokens,
            "total_cost_credits": round(self.charge.total_cost, 6),
            "cached_cost_credits": round(self.charge.cached_prompt_cost, 6),
        }


async def run_measurement(
    provider: ChatProvider,
    model: str,
    price: ModelPrice,
    questions: list[str],
    prefix: str | None = None,
) -> list[MeasurementRow]:
    """对同一前缀连续发起 len(questions) 次调用，逐次记录 usage 与折算结果。"""
    shared = prefix if prefix is not None else build_shared_prefix()
    rows: list[MeasurementRow] = []
    for index, question in enumerate(questions):
        resp = await provider.chat_completion(build_payload(model, shared, question))
        raw_usage = resp.get("usage")
        if not isinstance(raw_usage, dict):
            raise ValueError(f"第 {index + 1} 次调用响应缺少 usage 字段")
        probe = probe_usage(raw_usage)
        rows.append(
            MeasurementRow(
                call_index=index + 1,
                question=question,
                raw_usage=raw_usage,
                probe=probe,
                charge=charge_usage(probe, price),
            )
        )
    return rows


def format_comparison_table(rows: list[MeasurementRow], price: ModelPrice) -> str:
    """输出对比表：各次调用的 prompt/cached 命中量/折算 credits 与全价基线差异。"""
    header = (
        f"{'#':>2} | {'field_kind':>9} | {'prompt':>7} | {'cached':>7} | {'compl':>6} "
        f"| {'credits':>10} | {'全价基线':>10} | {'节省':>8}"
    )
    lines = [header, "-" * len(header)]
    for row in rows:
        baseline = row.charge.full_price_baseline(price)
        saved = baseline - row.charge.total_cost
        lines.append(
            f"{row.call_index:>2} | {row.probe.field_kind.value:>9} "
            f"| {row.probe.prompt_tokens:>7} | {row.probe.cached_tokens:>7} "
            f"| {row.probe.completion_tokens:>6} | {row.charge.total_cost:>10.3f} "
            f"| {baseline:>10.3f} | {saved:>8.3f}"
        )
    return "\n".join(lines)


def rows_to_json(rows: list[MeasurementRow]) -> str:
    """测量记录序列化为 JSON（含每次调用的原始 usage）。"""
    return json.dumps([row.to_dict() for row in rows], ensure_ascii=False, indent=2)
