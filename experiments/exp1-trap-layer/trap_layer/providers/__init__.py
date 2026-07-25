"""provider 抽象层：mock（离线确定性）与 OpenAI 兼容转发（真实）。"""

from .base import ChatProvider, ProviderError
from .mock import MockProvider
from .openai_compat import OpenAICompatProvider

__all__ = ["ChatProvider", "MockProvider", "OpenAICompatProvider", "ProviderError"]
