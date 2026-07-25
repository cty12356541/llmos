"""mock provider：进程内确定性假 LLM。

- 固定速率生成 token（每个 chunk 恰好 1 个 token，计量可验证）
- 回报 usage（prompt_tokens 与代理同口径粗估，completion_tokens = 实际生成数）
- 可配延迟（MOCK_TOKENS_PER_SECOND / MOCK_LATENCY_MS）
- 脚本化 ReAct 行为：带 tools 且尚无 tool 结果 → 发起 calculator 调用；
  已有 tool 结果 → 输出最终答案。harness 离线可跑。
- 严格遵守注入的 max_tokens：超出即 finish_reason="length"（截断验证的物理事实来源）
"""

from __future__ import annotations

import asyncio
import json
import math
import re
import time
import uuid
from collections.abc import AsyncIterator
from typing import Any

from ..budget import estimate_prompt_tokens

_DEFAULT_COMPLETION_TOKENS = 24
#: mock 的"自然回答长度"：客户端传 max_tokens 时按 min(max_tokens, NATURAL) 生成，
#: max_tokens < NATURAL 即视为被截断（finish_reason="length"）。
#: 这让"余额 100 credits 发起预估 150 credits 的调用 → 被硬顶截断"成为可复现的物理事实。
_NATURAL_COMPLETION_TOKENS = 150
_EXPR_PATTERN = re.compile(r"\(?\d[\d\s+\-*/().]*\d\)?")


def _last_message(payload: dict[str, Any]) -> dict[str, Any]:
    messages = payload.get("messages") or []
    return messages[-1] if messages else {}


def _has_tool_result(payload: dict[str, Any]) -> bool:
    return any(m.get("role") == "tool" for m in payload.get("messages") or [])


def _extract_expression(payload: dict[str, Any]) -> str:
    content = str(_last_message(payload).get("content") or "")
    match = _EXPR_PATTERN.search(content)
    if not match:
        return "1+1"
    expr = match.group(0).strip()
    # 括号不平衡的匹配退化兜底，保证 calculator 总能解析
    return expr if expr.count("(") == expr.count(")") else "1+1"


class MockProvider:
    """确定性 mock：无网络、无 key，全部测试与基准默认使用。"""

    def __init__(self, tokens_per_second: float = 100_000.0, latency_ms: float = 0.0) -> None:
        self._interval = 1.0 / tokens_per_second if tokens_per_second > 0 else 0.0
        self._latency = latency_ms / 1000.0

    async def aclose(self) -> None:
        return None

    # ---- 内部：确定性响应计划 ----

    def _plan(self, payload: dict[str, Any]) -> tuple[dict[str, Any] | None, list[str], bool]:
        """返回 (tool_call 或 None, 内容 token 序列, 是否被 max_tokens 截断)。"""
        max_tokens = payload.get("max_tokens")
        wants_tools = bool(payload.get("tools")) and not _has_tool_result(payload)
        if wants_tools:
            expression = _extract_expression(payload)
            tool_call = {
                "id": f"call_mock_{uuid.uuid4().hex[:8]}",
                "type": "function",
                "function": {
                    "name": "calculator",
                    "arguments": json.dumps({"expression": expression}),
                },
            }
            # tool_call 的 completion 成本按其序列化长度粗估
            n_tokens = max(1, math.ceil(len(json.dumps(tool_call)) / 4))
            return tool_call, [f"tk{i:03d}" for i in range(n_tokens)], False
        if isinstance(max_tokens, int):
            n_tokens = max(1, min(max_tokens, _NATURAL_COMPLETION_TOKENS))
            truncated = max_tokens < _NATURAL_COMPLETION_TOKENS
        else:
            n_tokens = _DEFAULT_COMPLETION_TOKENS
            truncated = False
        if _has_tool_result(payload):
            tool_output = str(_last_message(payload).get("content") or "")
            base = ["FINAL:", tool_output, "|", "task", "done."]
        else:
            base = [f"tok{i:03d}" for i in range(5)]
        tokens = [base[i % len(base)] for i in range(n_tokens)]
        return None, tokens, truncated

    def _usage(self, payload: dict[str, Any], completion_tokens: int) -> dict[str, int]:
        prompt_tokens = estimate_prompt_tokens(payload)
        return {
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": prompt_tokens + completion_tokens,
        }

    @staticmethod
    def _envelope(payload: dict[str, Any]) -> dict[str, Any]:
        return {
            "id": f"chatcmpl-mock-{uuid.uuid4().hex[:12]}",
            "created": int(time.time()),
            "model": str(payload.get("model") or "mock-model"),
        }

    async def chat_completion(self, payload: dict[str, Any]) -> dict[str, Any]:
        tool_call, tokens, truncated = self._plan(payload)
        if self._latency > 0:
            await asyncio.sleep(self._latency)
        if self._interval > 0:
            await asyncio.sleep(self._interval * len(tokens))
        message: dict[str, Any] = {"role": "assistant", "content": None}
        if tool_call is not None:
            message["tool_calls"] = [tool_call]
        else:
            message["content"] = " ".join(t for t in tokens if t)
        return {
            **self._envelope(payload),
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": message,
                    "finish_reason": "tool_calls" if tool_call else ("length" if truncated else "stop"),
                }
            ],
            "usage": self._usage(payload, len(tokens)),
        }

    async def chat_completion_stream(self, payload: dict[str, Any]) -> AsyncIterator[dict[str, Any]]:
        """每个 chunk 恰好 1 个 content token；末 chunk 带 finish_reason；再发 usage chunk。"""
        tool_call, tokens, truncated = self._plan(payload)
        envelope = self._envelope(payload)
        if self._latency > 0:
            await asyncio.sleep(self._latency)

        def chunk(delta: dict[str, Any], finish_reason: str | None) -> dict[str, Any]:
            return {
                **envelope,
                "object": "chat.completion.chunk",
                "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}],
            }

        yield chunk({"role": "assistant", "content": ""}, None)
        if tool_call is not None:
            yield chunk({"tool_calls": [{**tool_call, "index": 0}]}, None)
            yield chunk({}, "tool_calls")
        else:
            for token in tokens:
                if self._interval > 0:
                    await asyncio.sleep(self._interval)
                yield chunk({"content": token}, None)
            yield chunk({}, "length" if truncated else "stop")
        # OpenAI stream_options.include_usage 风格：最后单独发 usage chunk
        yield {**envelope, "object": "chat.completion.chunk", "choices": [], "usage": self._usage(payload, len(tokens))}
