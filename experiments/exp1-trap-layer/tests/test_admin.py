"""管理面：账户快照可观测，且不含 key 本体。"""

from __future__ import annotations

from trap_layer.config import AccountSeed

from conftest import make_env


async def test_账户快照结构与脱敏(tmp_path, rich_seed: AccountSeed) -> None:
    async with make_env(tmp_path, [rich_seed]) as env:
        await env.client.post(
            "/v1/chat/completions",
            headers=env.auth(rich_seed.key),
            json={"model": "mock-model", "messages": [{"role": "user", "content": "hi"}]},
        )
        resp = await env.client.get("/admin/accounts")
    assert resp.status_code == 200
    body = resp.json()
    snap = body["accounts"][rich_seed.agent_id]
    assert snap["balance"] < rich_seed.credits
    assert snap["key_fingerprint"].startswith("...")
    assert rich_seed.key not in resp.text
    assert body["wal"]["appended"] == 1


async def test_admin_token配置后强制校验(tmp_path, rich_seed: AccountSeed) -> None:
    from conftest import make_settings
    from trap_layer.budget import BudgetManager
    from trap_layer.providers.mock import MockProvider
    from trap_layer.proxy import create_app
    from trap_layer.wal import WalWriter

    import httpx

    from conftest import TEST_PRICING

    settings = make_settings(tmp_path)
    object.__setattr__(settings, "admin_token", "test-admin-token")
    budget = BudgetManager([rich_seed], TEST_PRICING)
    wal = WalWriter(settings.wal_path)
    app = create_app(settings, budget, wal, MockProvider())
    await wal.start()
    transport = httpx.ASGITransport(app=app)
    async with httpx.AsyncClient(transport=transport, base_url="http://trap.test") as client:
        denied = await client.get("/admin/accounts")
        allowed = await client.get("/admin/accounts", headers={"X-Admin-Token": "test-admin-token"})
    await wal.close()
    assert denied.status_code == 403
    assert allowed.status_code == 200
