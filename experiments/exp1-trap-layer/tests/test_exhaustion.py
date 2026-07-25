"""耗尽语义判定：80% 预警头 → 100% 挂起（429）→ 充值恢复。

账户种子 10 credits + 短消息（prompt 估算 1 token = 0.5 credit）：
- 第 1 次调用：cap = floor(10 - 0.5) = 9，扣 9.5，余 0.5（>0 未挂起）
- 第 2 次调用：0.5 ≤ 20%×10 → 预警头；cap 兜底 1，扣完归零
- 第 3 次调用：余额 ≤0 → 429 budget_exhausted
- 充值 100 → 调用恢复
"""

from __future__ import annotations

from trap_layer.config import AccountSeed

from conftest import make_env

SEED = AccountSeed(key="sk-test-broke", agent_id="test-broke", credits=10)
PAYLOAD = {"model": "mock-model", "messages": [{"role": "user", "content": "hi"}]}


async def test_预警挂起充值全生命周期(tmp_path) -> None:
    async with make_env(tmp_path, [SEED]) as env:
        # 第 1 次：正常，无预警
        r1 = await env.client.post("/v1/chat/completions", headers=env.auth(SEED.key), json=PAYLOAD)
        assert r1.status_code == 200
        assert "X-Budget-Warning" not in r1.headers

        # 第 2 次：余额 ≤20% → 预警头（判定标准 3 前半）
        r2 = await env.client.post("/v1/chat/completions", headers=env.auth(SEED.key), json=PAYLOAD)
        assert r2.status_code == 200
        assert r2.headers.get("X-Budget-Warning") == "true"

        # 第 3 次：余额 ≤0 → 挂起（判定标准 3 后半）
        r3 = await env.client.post("/v1/chat/completions", headers=env.auth(SEED.key), json=PAYLOAD)
        assert r3.status_code == 429
        assert r3.json()["error"]["code"] == "budget_exhausted"

        # 充值 → 恢复
        recharge = await env.client.post(
            "/admin/recharge", json={"agent_key": SEED.key, "credits": 100}
        )
        assert recharge.status_code == 200
        assert recharge.json()["balance"] > 0
        r4 = await env.client.post("/v1/chat/completions", headers=env.auth(SEED.key), json=PAYLOAD)
        assert r4.status_code == 200


async def test_挂起期间不扣减不转发(tmp_path) -> None:
    async with make_env(tmp_path, [SEED]) as env:
        # 耗尽账户
        for _ in range(3):
            resp = await env.client.post(
                "/v1/chat/completions", headers=env.auth(SEED.key), json=PAYLOAD
            )
            if resp.status_code == 429:
                break
        assert resp.status_code == 429
        await env.wal.flush_now()
        flushed_before = env.wal.stats.flushed

        # 挂起中的请求被拒且不产生任何流水
        rejected = await env.client.post(
            "/v1/chat/completions", headers=env.auth(SEED.key), json=PAYLOAD
        )
        assert rejected.status_code == 429
        await env.wal.flush_now()
        assert env.wal.stats.flushed == flushed_before


async def test_未知账户充值返回400(tmp_path, rich_seed: AccountSeed) -> None:
    async with make_env(tmp_path, [rich_seed]) as env:
        resp = await env.client.post(
            "/admin/recharge", json={"agent_key": "sk-不存在", "credits": 100}
        )
    assert resp.status_code == 400


async def test_非法充值额返回400(tmp_path, rich_seed: AccountSeed) -> None:
    async with make_env(tmp_path, [rich_seed]) as env:
        resp = await env.client.post(
            "/admin/recharge", json={"agent_key": rich_seed.key, "credits": -5}
        )
    assert resp.status_code == 400
