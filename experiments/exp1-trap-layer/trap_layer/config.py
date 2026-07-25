"""集中配置：环境变量（.env）+ YAML（账户、定价表）。

边界解析原则：所有不可信输入（env、yaml）在此模块解析为类型化值，
内部模块只接收类型化对象，不再重复校验。
"""

from __future__ import annotations

import os
from dataclasses import dataclass, field
from pathlib import Path

import yaml
from dotenv import load_dotenv

PROJECT_ROOT = Path(__file__).resolve().parent.parent
DEFAULT_ACCOUNTS_FILE = PROJECT_ROOT / "config" / "accounts.yaml"
DEFAULT_PRICING_FILE = PROJECT_ROOT / "config" / "pricing.yaml"


@dataclass(frozen=True, slots=True)
class ModelPrice:
    """单个模型的计价：每 1k token 的 credits 单价。"""

    prompt_per_1k: float
    completion_per_1k: float


@dataclass(frozen=True, slots=True)
class PricingTable:
    """定价表：模型名 → 单价；default 兜底。"""

    prices: dict[str, ModelPrice]
    default: ModelPrice

    def price_for(self, model: str) -> ModelPrice:
        return self.prices.get(model, self.default)


@dataclass(frozen=True, slots=True)
class AccountSeed:
    """YAML 中的初始账户：代理签发的 agent key → 预算账户。"""

    key: str
    agent_id: str
    credits: float


@dataclass(frozen=True, slots=True)
class Settings:
    """运行配置。use_mock=True 时 provider 为进程内 mock（离线可跑）。"""

    use_mock: bool
    llm_base_url: str
    llm_api_key: str
    llm_model: str
    mock_tokens_per_second: float
    mock_latency_ms: float
    wal_path: Path
    wal_batch_size: int
    wal_flush_interval_ms: float
    proxy_host: str
    proxy_port: int
    admin_token: str | None
    accounts_file: Path = DEFAULT_ACCOUNTS_FILE
    pricing_file: Path = DEFAULT_PRICING_FILE


def _env_bool(name: str, default: bool) -> bool:
    raw = os.environ.get(name)
    if raw is None:
        return default
    return raw.strip().lower() in {"1", "true", "yes", "on"}


def load_settings(env_file: Path | None = None) -> Settings:
    """从 .env + 环境变量加载配置。无真实 key 或未关 mock 时一律 mock。"""
    load_dotenv(env_file or PROJECT_ROOT / ".env")

    base_url = os.environ.get("LLM_BASE_URL", "").strip()
    api_key = os.environ.get("LLM_API_KEY", "").strip()
    model = os.environ.get("LLM_MODEL", "mock-model").strip() or "mock-model"
    mock_flag = _env_bool("MOCK_LLM", default=True)
    # 凭证托管语义：真实 provider 信息不全时绝不"半真实"运行，回落 mock
    use_mock = mock_flag or not (base_url and api_key)

    return Settings(
        use_mock=use_mock,
        llm_base_url=base_url,
        llm_api_key=api_key,
        llm_model=model,
        mock_tokens_per_second=float(os.environ.get("MOCK_TOKENS_PER_SECOND", "100000")),
        mock_latency_ms=float(os.environ.get("MOCK_LATENCY_MS", "0")),
        wal_path=Path(os.environ.get("WAL_PATH", str(PROJECT_ROOT / "results" / "wal" / "trap-layer.wal.jsonl"))),
        wal_batch_size=int(os.environ.get("WAL_BATCH_SIZE", "256")),
        wal_flush_interval_ms=float(os.environ.get("WAL_FLUSH_INTERVAL_MS", "50")),
        proxy_host=os.environ.get("PROXY_HOST", "127.0.0.1"),
        proxy_port=int(os.environ.get("PROXY_PORT", "8400")),
        admin_token=os.environ.get("ADMIN_TOKEN") or None,
    )


def load_pricing(path: Path | None = None) -> PricingTable:
    """解析定价 YAML 为 PricingTable。"""
    path = path or DEFAULT_PRICING_FILE
    raw = yaml.safe_load(path.read_text(encoding="utf-8"))
    models: dict[str, object] = raw.get("models", {})
    if "default" not in models:
        raise ValueError(f"定价表缺少 default 条目: {path}")
    prices: dict[str, ModelPrice] = {}
    for name, entry in models.items():
        if not isinstance(entry, dict):
            raise ValueError(f"定价表条目非法: {name}")
        prices[str(name)] = ModelPrice(
            prompt_per_1k=float(entry["prompt_per_1k"]),
            completion_per_1k=float(entry["completion_per_1k"]),
        )
    return PricingTable(prices=prices, default=prices["default"])


def load_account_seeds(path: Path | None = None) -> list[AccountSeed]:
    """解析账户 YAML 为初始账户列表。"""
    path = path or DEFAULT_ACCOUNTS_FILE
    raw = yaml.safe_load(path.read_text(encoding="utf-8"))
    seeds: list[AccountSeed] = []
    for entry in raw.get("accounts", []):
        seeds.append(
            AccountSeed(
                key=str(entry["key"]),
                agent_id=str(entry["agent_id"]),
                credits=float(entry["credits"]),
            )
        )
    if not seeds:
        raise ValueError(f"账户配置为空: {path}")
    return seeds
