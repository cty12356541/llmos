"""缓存感知 mock provider：进程内确定性假 LLM，模拟 prefix cache 命中的 usage 回报。

出处：复制并扩展自 experiments/exp1-trap-layer/trap_layer/providers/mock.py。
exp1 原版只回报 prompt/completion/total 三个基础字段；本版扩展：

- 以"除最后一条消息外的消息序列"作为共享前缀，按内容哈希识别重复前缀
- 第二次起同前缀调用视为缓存命中，命中量 = 前缀估算 token 数
- cache_style 可配三种回报风格，覆盖真实 provider 的三种字段情形：
    "deepseek" → prompt_cache_hit_tokens / prompt_cache_miss_tokens（DeepSeek 风格）
    "openai"   → prompt_tokens_details.cached_tokens（OpenAI 风格，命中量为子集）
    "none"     → 无任何缓存字段（无缓存感知的 provider，优雅降级情形）
- 固定速率生成 token、可配延迟，严格遵守注入的 max_tokens（与 exp1 同语义）
"""

from __future__ import annotations

import asyncio
import hashlib
import json
import math
import time
import uuid
from collections.abc import AsyncIterator
from typing import Any, Literal

CacheStyle = Literal["deepseek", "openai", "none"]

_DEFAULT_COMPLETION_TOKENS = 24
_NATURAL_COMPLETION_TOKENS = 150
#: 粗估 token 数的字符比率（与 exp1 budget.py 同口径，仅用于 mock 计量）
CHARS_PER_TOKEN_ESTIMATE = 4


def estimate_prompt_tokens(payload: dict[str, Any]) -> int:
    """粗估请求 prompt token 数（字符数 / 4，下限 1）。

    出处：与 exp1-trap-layer/trap_layer/budget.py 的 estimate_prompt_tokens 同口径。
    """
    chars = 0
    messages = payload.get("messages")
    if isinstance(messages, list):
        for msg in messages:
            if isinstance(msg, dict):
                content = msg.get("content")
                if isinstance(content, str):
                    chars += len(content)
                elif isinstance(content, list):
                    chars += sum(
                        len(part.get("text", "")) for part in content if isinstance(part, dict)
                    )
    return max(1, math.ceil(chars / CHARS_PER_TOKEN_ESTIMATE))


def _estimate_messages_tokens(messages: list[dict[str, Any]]) -> int:
    """粗估一组消息的 token 数（字符数 / 4）。前缀为空时返回 0。"""
    chars = 0
    for msg in messages:
        content = msg.get("content")
        if isinstance(content, str):
            chars += len(content)
        elif isinstance(content, list):
            chars += sum(len(part.get("text", "")) for part in content if isinstance(part, dict))
    if chars == 0:
        return 0
    return max(1, math.ceil(chars / CHARS_PER_TOKEN_ESTIMATE))


class CacheAwareMockProvider:
    """模拟"第二次起同前缀命中缓存"的确定性 mock：无网络、无 key，全部测试离线可跑。"""

    def __init__(
        self,
        cache_style: CacheStyle = "deepseek",
        tokens_per_second: float = 100_000.0,
        latency_ms: float = 0.0,
    ) -> None:
        self._cache_style: CacheStyle = cache_style
        self._interval = 1.0 / tokens_per_second if tokens_per_second > 0 else 0.0
        self._latency = latency_ms / 1000.0
        #: 已见前缀签名集合：签名在集合中且前缀非空 → 本次调用命中缓存
        self._seen_prefixes: set[str] = set()

    async def aclose(self) -> None:
        return None

    # ---- 内部：前缀识别与缓存命中判定 ----

    @staticmethod
    def _prefix_messages(payload: dict[str, Any]) -> list[dict[str, Any]]:
        """共享前缀 = 除最后一条消息外的消息序列（典型：系统提示 + 历史）。"""
        messages = payload.get("messages") or []
        return [m for m in messages[:-1] if isinstance(m, dict)]

    def _cache_hit_tokens(self, payload: dict[str, Any]) -> int:
        """判定本次调用的缓存命中量：同前缀第二次起命中，命中量 = 前缀估算 token 数。"""
        prefix = self._prefix_messages(payload)
        if not prefix:
            return 0
        signature = hashlib.sha1(
            json.dumps(prefix, sort_keys=True, ensure_ascii=False).encode("utf-8")
        ).hexdigest()
        prefix_tokens = _estimate_messages_tokens(prefix)
        if signature in self._seen_prefixes:
            return prefix_tokens
        self._seen_prefixes.add(signature)
        return 0

    # ---- 内部：确定性响应计划（与 exp1 mock 同语义） ----

    @staticmethod
    def _plan(payload: dict[str, Any]) -> tuple[list[str], bool]:
        """返回 (内容 token 序列, 是否被 max_tokens 截断)。"""
        max_tokens = payload.get("max_tokens")
        if isinstance(max_tokens, int):
            n_tokens = max(1, min(max_tokens, _NATURAL_COMPLETION_TOKENS))
            truncated = max_tokens < _NATURAL_COMPLETION_TOKENS
        else:
            n_tokens = _DEFAULT_COMPLETION_TOKENS
            truncated = False
        base = [f"tok{i:03d}" for i in range(5)]
        tokens = [base[i % len(base)] for i in range(n_tokens)]
        return tokens, truncated

    def _usage(self, payload: dict[str, Any], completion_tokens: int) -> dict[str, Any]:
        """按 cache_style 回报 usage：三种字段情形的物理事实来源。"""
        prompt_tokens = estimate_prompt_tokens(payload)
        hit = self._cache_hit_tokens(payload)
        usage: dict[str, Any] = {
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": prompt_tokens + completion_tokens,
        }
        if self._cache_style == "deepseek":
            # DeepSeek 风格：prompt_tokens = hit + miss
            usage["prompt_cache_hit_tokens"] = hit
            usage["prompt_cache_miss_tokens"] = prompt_tokens - hit
        elif self._cache_style == "openai":
            # OpenAI 风格：cached_tokens 是 prompt_tokens 的子集
            usage["prompt_tokens_details"] = {"cached_tokens": hit}
        # "none"：不附加任何缓存字段（无缓存感知 provider 的降级情形）
        return usage

    @staticmethod
    def _envelope(payload: dict[str, Any]) -> dict[str, Any]:
        return {
            "id": f"chatcmpl-mock-{uuid.uuid4().hex[:12]}",
            "created": int(time.time()),
            "model": str(payload.get("model") or "mock-model"),
        }

    async def chat_completion(self, payload: dict[str, Any]) -> dict[str, Any]:
        tokens, truncated = self._plan(payload)
        if self._latency > 0:
            await asyncio.sleep(self._latency)
        if self._interval > 0:
            await asyncio.sleep(self._interval * len(tokens))
        return {
            **self._envelope(payload),
            "object": "chat.completion",
            "choices": [
                {
                    "index": 0,
                    "message": {"role": "assistant", "content": " ".join(tokens)},
                    "finish_reason": "length" if truncated else "stop",
                }
            ],
            "usage": self._usage(payload, len(tokens)),
        }

    async def chat_completion_stream(self, payload: dict[str, Any]) -> AsyncIterator[dict[str, Any]]:
        """每个 chunk 恰好 1 个 content token；末 chunk 带 finish_reason；再发 usage chunk。"""
        tokens, truncated = self._plan(payload)
        envelope = self._envelope(payload)
        usage = self._usage(payload, len(tokens))
        if self._latency > 0:
            await asyncio.sleep(self._latency)

        def chunk(delta: dict[str, Any], finish_reason: str | None) -> dict[str, Any]:
            return {
                **envelope,
                "object": "chat.completion.chunk",
                "choices": [{"index": 0, "delta": delta, "finish_reason": finish_reason}],
            }

        yield chunk({"role": "assistant", "content": ""}, None)
        for token in tokens:
            if self._interval > 0:
                await asyncio.sleep(self._interval)
            yield chunk({"content": token}, None)
        yield chunk({}, "length" if truncated else "stop")
        # OpenAI stream_options.include_usage 风格：最后单独发 usage chunk
        yield {**envelope, "object": "chat.completion.chunk", "choices": [], "usage": usage}
