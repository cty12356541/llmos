"""流式计量判定：chunk 增量计数，结束后统一结算，SSE 格式兼容。"""

from __future__ import annotations

import json

from trap_layer.config import AccountSeed

from conftest import TEST_PRICING, make_env, read_wal


async def _collect_stream(env, key: str, payload: dict) -> tuple[list[dict], list[str]]:
    chunks: list[dict] = []
    raw_lines: list[str] = []
    async with env.client.stream(
        "POST",
        "/v1/chat/completions",
        headers=env.auth(key),
        json={**payload, "stream": True},
    ) as resp:
        assert resp.status_code == 200
        assert resp.headers["content-type"].startswith("text/event-stream")
        async for line in resp.aiter_lines():
            if not line.startswith("data:"):
                continue
            data = line[len("data:"):].strip()
            raw_lines.append(data)
            if data != "[DONE]":
                chunks.append(json.loads(data))
    return chunks, raw_lines


async def test_流式sse格式与内容完整(tmp_path, rich_seed: AccountSeed) -> None:
    async with make_env(tmp_path, [rich_seed]) as env:
        chunks, raw_lines = await _collect_stream(
            env,
            rich_seed.key,
            {"model": "mock-model", "messages": [{"role": "user", "content": "hi"}]},
        )
    # SSE 以 [DONE] 收尾
    assert raw_lines[-1] == "[DONE]"
    # 每个 chunk 都是标准 chat.completion.chunk
    content_tokens = 0
    for chunk in chunks:
        assert chunk["object"] == "chat.completion.chunk"
        assert chunk["choices"], "usage chunk 不得转发给 agent"
        delta = chunk["choices"][0]["delta"]
        if delta.get("content"):
            content_tokens += 1
    assert content_tokens > 0
    # 末 chunk 带 finish_reason
    assert chunks[-1]["choices"][0]["finish_reason"] in {"stop", "length"}


async def test_流式结束后统一结算扣减(tmp_path, rich_seed: AccountSeed) -> None:
    async with make_env(tmp_path, [rich_seed]) as env:
        chunks, _ = await _collect_stream(
            env,
            rich_seed.key,
            {"model": "mock-model", "messages": [{"role": "user", "content": "hi"}]},
        )
        content_tokens = sum(
            1 for c in chunks if (c["choices"][0].get("delta") or {}).get("content")
        )
        account = env.budget.account_for_key(rich_seed.key)
        assert account is not None
        price = TEST_PRICING.default
        # 结算在流结束后已发生（生成器 finally 中完成）
        # completion_tokens 以 provider 回报的 usage 为准，与 chunk 计数一致
        await env.wal.flush_now()
        records = read_wal(env.wal.path)
        assert len(records) == 1
        record = records[0]
        assert record["stream"] is True
        assert record["completion_tokens"] == content_tokens
        expected_cost = (
            record["prompt_tokens"] * price.prompt_per_1k / 1000.0
            + content_tokens * price.completion_per_1k / 1000.0
        )
        assert record["charged"] == expected_cost
        assert account.balance == rich_seed.credits - expected_cost


async def test_流式同样受硬顶截断(tmp_path) -> None:
    seed = AccountSeed(key="sk-test-stream-poor", agent_id="stream-poor", credits=50)
    async with make_env(tmp_path, [seed]) as env:
        chunks, _ = await _collect_stream(
            env,
            seed.key,
            {
                "model": "mock-model",
                "messages": [{"role": "user", "content": "详细回答"}],
                "max_tokens": 100000,
            },
        )
    content_tokens = sum(1 for c in chunks if (c["choices"][0].get("delta") or {}).get("content"))
    # 余额 50 物理上付不起 150 token 的自然长度 → 被截断
    assert chunks[-1]["choices"][0]["finish_reason"] == "length"
    assert content_tokens < 150
