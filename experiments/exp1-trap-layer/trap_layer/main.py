"""入口：装配配置 → provider → 预算 → WAL → FastAPI，uvicorn 启动。

用法：
    uv run python -m trap_layer.main
"""

from __future__ import annotations

import uvicorn

from .budget import BudgetManager
from .config import load_account_seeds, load_pricing, load_settings
from .providers import MockProvider, OpenAICompatProvider
from .providers.base import ChatProvider
from .proxy import create_app
from .wal import WalWriter


def build() -> tuple[object, int, str]:
    settings = load_settings()
    pricing = load_pricing(settings.pricing_file)
    budget = BudgetManager(load_account_seeds(settings.accounts_file), pricing)
    wal = WalWriter(settings.wal_path, settings.wal_batch_size, settings.wal_flush_interval_ms)
    provider: ChatProvider
    if settings.use_mock:
        provider = MockProvider(settings.mock_tokens_per_second, settings.mock_latency_ms)
    else:
        provider = OpenAICompatProvider(settings.llm_base_url, settings.llm_api_key)
    app = create_app(settings, budget, wal, provider)
    return app, settings.proxy_port, settings.proxy_host


def main() -> None:
    app, port, host = build()
    uvicorn.run(app, host=host, port=port, log_level="info")


if __name__ == "__main__":
    main()
