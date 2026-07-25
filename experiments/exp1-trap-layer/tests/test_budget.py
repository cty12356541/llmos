"""扣减正确性判定：按响应 usage × 定价表折算，余额与 WAL 流水一致。"""

from __future__ import annotations

from trap_layer.config import AccountSeed

from conftest import TEST_PRICING, make_env, read_wal


async def test_扣减额等于usage乘定价(tmp_path, rich_seed: AccountSeed) -> None:
    async with make_env(tmp_path, [rich_seed]) as env:
        resp = await env.client.post(
            "/v1/chat/completions",
            headers=env.auth(rich_seed.key),
            json={"model": "mock-model", "messages": [{"role": "user", "content": "给我一个回答"}]},
        )
        assert resp.status_code == 200
        usage = resp.json()["usage"]
        price = TEST_PRICING.default
        expected_cost = (
            usage["prompt_tokens"] * price.prompt_per_1k / 1000.0
            + usage["completion_tokens"] * price.completion_per_1k / 1000.0
        )
        account = env.budget.account_for_key(rich_seed.key)
        assert account is not None
        # Given 一次成功调用 / When 结算完成 / Then 余额恰好扣掉 usage×单价
        assert account.balance == rich_seed.credits - expected_cost

        await env.wal.flush_now()
        records = read_wal(env.wal.path)
    assert len(records) == 1
    record = records[0]
    assert record["charged"] == expected_cost
    assert record["balance_after"] == rich_seed.credits - expected_cost
    assert record["prompt_tokens"] == usage["prompt_tokens"]
    assert record["completion_tokens"] == usage["completion_tokens"]
    assert record["agent_id"] == rich_seed.agent_id
    assert record["stream"] is False
    # key 本体不落盘
    assert rich_seed.key not in str(record)


async def test_多次调用余额单调递减且流水逐笔对应(tmp_path, rich_seed: AccountSeed) -> None:
    async with make_env(tmp_path, [rich_seed]) as env:
        balances: list[float] = []
        for _ in range(3):
            resp = await env.client.post(
                "/v1/chat/completions",
                headers=env.auth(rich_seed.key),
                json={"model": "mock-model", "messages": [{"role": "user", "content": "hi"}]},
            )
            assert resp.status_code == 200
            balances.append(float(resp.headers["X-Budget-Remaining"]))
        assert balances[0] > balances[1] > balances[2]

        await env.wal.flush_now()
        records = read_wal(env.wal.path)
    assert len(records) == 3
    assert [r["seq"] for r in records] == [1, 2, 3]
    for record, header_balance in zip(records, balances, strict=True):
        assert abs(record["balance_after"] - header_balance) < 1e-4


async def test_每笔扣减有唯一请求ID(tmp_path, rich_seed: AccountSeed) -> None:
    async with make_env(tmp_path, [rich_seed]) as env:
        for _ in range(2):
            await env.client.post(
                "/v1/chat/completions",
                headers=env.auth(rich_seed.key),
                json={"model": "mock-model", "messages": [{"role": "user", "content": "hi"}]},
            )
        await env.wal.flush_now()
        records = read_wal(env.wal.path)
    request_ids = [r["request_id"] for r in records]
    assert len(set(request_ids)) == 2
