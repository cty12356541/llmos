"""max_tokens 硬顶判定（核心验证点）：余额物理上不可能被一次调用烧穿。

判定标准 2：余额 ~120 credits 的 agent 发起预估 150 的调用
→ 生成被 max_tokens 截断，账单 ≤ 余额。
"""

from __future__ import annotations

from trap_layer.budget import BudgetManager
from trap_layer.config import AccountSeed

from conftest import TEST_PRICING, make_env


def test_硬顶折算单元语义() -> None:
    # Given 余额 120、prompt 估算 10 tokens（预留 5 credits）
    # When 折算 completion 硬顶 / Then cap = floor((120-5)/1) = 115
    budget = BudgetManager(
        [AccountSeed(key="k-unit", agent_id="unit", credits=120)], TEST_PRICING
    )
    account = budget.account_for_key("k-unit")
    assert account is not None
    cap = budget.max_completion_tokens_affordable(account, "any-model", est_prompt_tokens=10)
    assert cap == 115


def test_余额不足一个token时硬顶兜底为1() -> None:
    budget = BudgetManager(
        [AccountSeed(key="k-tiny", agent_id="tiny", credits=0.3)], TEST_PRICING
    )
    account = budget.account_for_key("k-tiny")
    assert account is not None
    cap = budget.max_completion_tokens_affordable(account, "any-model", est_prompt_tokens=10)
    assert cap == 1


async def test_客户端更大max_tokens被覆盖且生成被物理截断(tmp_path) -> None:
    # 判定标准 2 的端到端复现：余额 120，客户端要 100000 token（远超 150 自然长度）
    seed = AccountSeed(key="sk-test-poor", agent_id="test-poor", credits=120)
    async with make_env(tmp_path, [seed]) as env:
        resp = await env.client.post(
            "/v1/chat/completions",
            headers=env.auth(seed.key),
            json={
                "model": "mock-model",
                "messages": [{"role": "user", "content": "详细回答"}],
                "max_tokens": 100000,
            },
        )
        assert resp.status_code == 200
        body = resp.json()
        usage = body["usage"]
        account = env.budget.account_for_key(seed.key)
        assert account is not None
        # Then 生成被截断（mock 自然长度 150，cap 更小）
        assert body["choices"][0]["finish_reason"] == "length"
        assert usage["completion_tokens"] < 150
        # 账单 ≤ 调用前余额：charged 被钳制在 120 以内，余额不归负
        assert account.balance >= 0
        # 硬顶值通过响应头暴露（观测性）
        cap = int(resp.headers["X-Budget-Max-Tokens-Cap"])
        assert cap == usage["completion_tokens"]
        assert cap < 100000


async def test_客户端更小max_tokens不被放大(tmp_path, rich_seed: AccountSeed) -> None:
    async with make_env(tmp_path, [rich_seed]) as env:
        resp = await env.client.post(
            "/v1/chat/completions",
            headers=env.auth(rich_seed.key),
            json={
                "model": "mock-model",
                "messages": [{"role": "user", "content": "hi"}],
                "max_tokens": 5,
            },
        )
    assert resp.status_code == 200
    # 客户端的 5 < cap，代理不得放大
    assert resp.json()["usage"]["completion_tokens"] == 5
    assert int(resp.headers["X-Budget-Max-Tokens-Cap"]) == 5
