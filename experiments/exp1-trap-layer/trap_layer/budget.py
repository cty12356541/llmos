"""预算账户管理：按 agent key 记账、credits 计价、硬顶折算、预警与挂起。

硬顶语义（议题 11 裂缝 7 定案的原型化）：
请求到达时按"剩余 credits ÷ 单价"折算 completion token 上限，
注入请求覆盖客户端更大的 max_tokens —— 余额物理上不可能被一次调用烧穿。
结算时再按真实 usage 计费，并以余额为上限钳制（账单 ≤ 余额的兜底保证）。
"""

from __future__ import annotations

import math
from dataclasses import dataclass

from .config import AccountSeed, ModelPrice, PricingTable

#: 余额占累计额度 ≤ 此比例时触发预警（X-Budget-Warning）
WARNING_RATIO = 0.2
#: 粗估 prompt token 数的字符比率（仅用于硬顶折算时的 prompt 成本预留）
CHARS_PER_TOKEN_ESTIMATE = 4


@dataclass(slots=True)
class AccountState:
    """单个 agent 的预算账户运行时状态。"""

    agent_id: str
    key_fingerprint: str
    balance: float
    total_granted: float

    @property
    def exhausted(self) -> bool:
        """余额 ≤0 即挂起。"""
        return self.balance <= 0

    @property
    def warning(self) -> bool:
        """余额 ≤20% 累计额度即预警。"""
        return self.total_granted > 0 and self.balance <= WARNING_RATIO * self.total_granted


@dataclass(frozen=True, slots=True)
class Settlement:
    """一次调用的结算结果（用于 WAL 与响应头）。"""

    agent_id: str
    model: str
    prompt_tokens: int
    completion_tokens: int
    cost: float
    charged: float
    balance_after: float


def fingerprint(key: str) -> str:
    """key 不落盘不打印，仅存尾部指纹便于人工对账。"""
    return f"...{key[-4:]}" if len(key) >= 4 else "..."


def estimate_prompt_tokens(payload: dict[str, object]) -> int:
    """粗估请求 prompt token 数（字符数 / 4，下限 1）。仅用于硬顶折算预留。"""
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


class BudgetManager:
    """预算账户集合。所有方法无 await，在事件循环内天然原子。"""

    def __init__(self, seeds: list[AccountSeed], pricing: PricingTable) -> None:
        self._pricing = pricing
        self._by_key: dict[str, AccountState] = {
            seed.key: AccountState(
                agent_id=seed.agent_id,
                key_fingerprint=fingerprint(seed.key),
                balance=seed.credits,
                total_granted=seed.credits,
            )
            for seed in seeds
        }

    def account_for_key(self, key: str) -> AccountState | None:
        return self._by_key.get(key)

    def snapshot(self) -> dict[str, dict[str, float | str | bool]]:
        """管理面可观测：账户余额快照（不含 key 本体）。"""
        return {
            acct.agent_id: {
                "key_fingerprint": acct.key_fingerprint,
                "balance": round(acct.balance, 6),
                "total_granted": round(acct.total_granted, 6),
                "exhausted": acct.exhausted,
                "warning": acct.warning,
            }
            for acct in self._by_key.values()
        }

    def max_completion_tokens_affordable(
        self, account: AccountState, model: str, est_prompt_tokens: int
    ) -> int:
        """按剩余余额折算 completion token 硬顶。

        预留估算 prompt 成本后，剩余额度全部折算为 completion token 上限。
        余额 >0 但折不出 1 个 token 时返回 1（由结算钳制兜底账单 ≤ 余额）。
        """
        price = self._pricing.price_for(model)
        reserved = est_prompt_tokens * price.prompt_per_1k / 1000.0
        remaining = account.balance - reserved
        if remaining <= 0:
            return 1
        cap = math.floor(remaining * 1000.0 / price.completion_per_1k)
        return max(1, cap)

    def settle(
        self,
        account: AccountState,
        model: str,
        prompt_tokens: int,
        completion_tokens: int,
    ) -> Settlement:
        """按真实 usage 结算扣减；扣减额以余额为上限（账单 ≤ 调用前余额）。"""
        price: ModelPrice = self._pricing.price_for(model)
        cost = (
            prompt_tokens * price.prompt_per_1k / 1000.0
            + completion_tokens * price.completion_per_1k / 1000.0
        )
        charged = min(cost, account.balance)
        account.balance -= charged
        return Settlement(
            agent_id=account.agent_id,
            model=model,
            prompt_tokens=prompt_tokens,
            completion_tokens=completion_tokens,
            cost=cost,
            charged=charged,
            balance_after=account.balance,
        )

    def recharge(self, key: str, credits: float) -> AccountState | None:
        """充值即恢复：余额与累计额度同时增加，挂起状态自动解除。"""
        account = self._by_key.get(key)
        if account is None or credits <= 0:
            return None
        account.balance += credits
        account.total_granted += credits
        return account
