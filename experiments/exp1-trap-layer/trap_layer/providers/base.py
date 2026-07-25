"""provider 协议：代理只依赖此接口，不关心下游是 mock 还是真实服务。"""

from __future__ import annotations

from collections.abc import AsyncIterator
from typing import Any, Protocol


class ProviderError(Exception):
    """下游 provider 调用失败（HTTP 错误、网络错误）。"""

    def __init__(self, message: str, status_code: int | None = None) -> None:
        super().__init__(message)
        self.status_code = status_code


class ChatProvider(Protocol):
    """OpenAI /v1/chat/completions 语义的最小协议。

    输入输出都是 OpenAI 格式的 dict（代理透传语义），
    但 usage 字段必须存在（非流式在响应体，流式在末尾 usage chunk），
    这是预算结算的唯一事实来源。
    """

    async def chat_completion(self, payload: dict[str, Any]) -> dict[str, Any]:
        """非流式：返回完整 chat.completion JSON。"""
        ...

    def chat_completion_stream(self, payload: dict[str, Any]) -> AsyncIterator[dict[str, Any]]:
        """流式：逐 chunk 产出 chat.completion.chunk JSON（不含 SSE 包装）。"""
        ...

    async def aclose(self) -> None:
        """释放底层连接资源。"""
        ...
