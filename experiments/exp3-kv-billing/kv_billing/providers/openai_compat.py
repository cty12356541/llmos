"""真实 provider 适配：OpenAI 兼容端点转发。

出处：复制自 experiments/exp1-trap-layer/trap_layer/providers/openai_compat.py（未改动语义）。
真实 key 只存在于本模块，经 .env 注入，永不进日志、永不随响应外泄。
"""

from __future__ import annotations

import json
from collections.abc import AsyncIterator
from typing import Any

import httpx

from .base import ProviderError


class OpenAICompatProvider:
    """把 OpenAI 格式请求原样转发到 LLM_BASE_URL 指向的兼容服务。"""

    def __init__(self, base_url: str, api_key: str, timeout_s: float = 120.0) -> None:
        self._endpoint = f"{base_url.rstrip('/')}/chat/completions"
        self._client = httpx.AsyncClient(
            timeout=httpx.Timeout(timeout_s, connect=10.0),
            headers={
                # 真实 provider key 仅在此注入请求头
                "Authorization": f"Bearer {api_key}",
                "Content-Type": "application/json",
            },
            limits=httpx.Limits(max_connections=64, max_keepalive_connections=16),
        )

    async def aclose(self) -> None:
        await self._client.aclose()

    async def chat_completion(self, payload: dict[str, Any]) -> dict[str, Any]:
        try:
            resp = await self._client.post(self._endpoint, json={**payload, "stream": False})
        except httpx.HTTPError as exc:
            raise ProviderError(f"provider 网络错误: {type(exc).__name__}") from exc
        if resp.status_code != 200:
            raise ProviderError(f"provider 返回 {resp.status_code}", status_code=resp.status_code)
        data: dict[str, Any] = resp.json()
        return data

    async def chat_completion_stream(self, payload: dict[str, Any]) -> AsyncIterator[dict[str, Any]]:
        body = {**payload, "stream": True}
        # 要求 provider 在流末尾回报 usage（结算的事实来源）
        body.setdefault("stream_options", {"include_usage": True})
        try:
            async with self._client.stream("POST", self._endpoint, json=body) as resp:
                if resp.status_code != 200:
                    await resp.aread()
                    raise ProviderError(f"provider 返回 {resp.status_code}", status_code=resp.status_code)
                async for line in resp.aiter_lines():
                    if not line.startswith("data:"):
                        continue
                    data = line[len("data:"):].strip()
                    if data == "[DONE]":
                        return
                    yield json.loads(data)
        except httpx.HTTPError as exc:
            raise ProviderError(f"provider 网络错误: {type(exc).__name__}") from exc
