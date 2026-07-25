"""测试基座：全部走 mock provider，无需任何真实 key。"""

from __future__ import annotations

import json
from collections.abc import AsyncIterator
from contextlib import asynccontextmanager
from dataclasses import dataclass
from pathlib import Path

import httpx
import pytest
from fastapi import FastAPI

from trap_layer.budget import BudgetManager
from trap_layer.config import AccountSeed, ModelPrice, PricingTable, Settings
from trap_layer.providers.mock import MockProvider
from trap_layer.proxy import create_app
from trap_layer.wal import WalWriter

#: 测试定价：prompt 0.5 credit/token，completion 1 credit/token（便于心算验证）
TEST_PRICING = PricingTable(
    prices={},
    default=ModelPrice(prompt_per_1k=500.0, completion_per_1k=1000.0),
)


@dataclass(slots=True)
class Env:
    client: httpx.AsyncClient
    app: FastAPI
    budget: BudgetManager
    wal: WalWriter

    def auth(self, key: str) -> dict[str, str]:
        return {"Authorization": f"Bearer {key}"}


def make_settings(tmp_path: Path) -> Settings:
    return Settings(
        use_mock=True,
        llm_base_url="",
        llm_api_key="",
        llm_model="mock-model",
        mock_tokens_per_second=1_000_000.0,
        mock_latency_ms=0.0,
        wal_path=tmp_path / "test.wal.jsonl",
        wal_batch_size=8,
        wal_flush_interval_ms=5.0,
        proxy_host="127.0.0.1",
        proxy_port=0,
        admin_token=None,
    )


@asynccontextmanager
async def make_env(
    tmp_path: Path,
    seeds: list[AccountSeed],
    pricing: PricingTable = TEST_PRICING,
) -> AsyncIterator[Env]:
    """装配一套完整代理（内存 ASGI transport，不走真实端口）。"""
    settings = make_settings(tmp_path)
    budget = BudgetManager(seeds, pricing)
    wal = WalWriter(settings.wal_path, settings.wal_batch_size, settings.wal_flush_interval_ms)
    provider = MockProvider(tokens_per_second=1_000_000.0)
    app = create_app(settings, budget, wal, provider)
    await wal.start()
    transport = httpx.ASGITransport(app=app)
    async with httpx.AsyncClient(transport=transport, base_url="http://trap.test") as client:
        yield Env(client=client, app=app, budget=budget, wal=wal)
    await wal.close()


def read_wal(path: Path) -> list[dict[str, object]]:
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]


@pytest.fixture
def rich_seed() -> AccountSeed:
    return AccountSeed(key="sk-test-rich", agent_id="test-rich", credits=100_000)
