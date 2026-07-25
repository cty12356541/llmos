"""格式兼容判定：mock 下请求/响应结构与 OpenAI 一致；认证语义正确。"""

from __future__ import annotations

from trap_layer.config import AccountSeed

from conftest import make_env


async def test_非流式响应结构兼容openai(tmp_path, rich_seed: AccountSeed) -> None:
    async with make_env(tmp_path, [rich_seed]) as env:
        resp = await env.client.post(
            "/v1/chat/completions",
            headers=env.auth(rich_seed.key),
            json={"model": "mock-model", "messages": [{"role": "user", "content": "你好"}]},
        )
    assert resp.status_code == 200
    body = resp.json()
    # Given mock provider / When 非流式调用 / Then 响应为完整 chat.completion 结构
    assert body["object"] == "chat.completion"
    assert body["id"].startswith("chatcmpl-")
    assert isinstance(body["created"], int)
    assert body["model"] == "mock-model"
    choice = body["choices"][0]
    assert choice["index"] == 0
    assert choice["message"]["role"] == "assistant"
    assert isinstance(choice["message"]["content"], str)
    assert choice["finish_reason"] in {"stop", "length", "tool_calls"}
    usage = body["usage"]
    assert usage["prompt_tokens"] > 0
    assert usage["completion_tokens"] > 0
    assert usage["total_tokens"] == usage["prompt_tokens"] + usage["completion_tokens"]


async def test_未知key返回401(tmp_path, rich_seed: AccountSeed) -> None:
    async with make_env(tmp_path, [rich_seed]) as env:
        resp = await env.client.post(
            "/v1/chat/completions",
            headers=env.auth("sk-agent-nonexistent"),
            json={"model": "mock-model", "messages": [{"role": "user", "content": "hi"}]},
        )
    assert resp.status_code == 401


async def test_缺少authorization头返回401(tmp_path, rich_seed: AccountSeed) -> None:
    async with make_env(tmp_path, [rich_seed]) as env:
        resp = await env.client.post(
            "/v1/chat/completions",
            json={"model": "mock-model", "messages": [{"role": "user", "content": "hi"}]},
        )
    assert resp.status_code == 401


async def test_工具调用轮次结构兼容(tmp_path, rich_seed: AccountSeed) -> None:
    async with make_env(tmp_path, [rich_seed]) as env:
        resp = await env.client.post(
            "/v1/chat/completions",
            headers=env.auth(rich_seed.key),
            json={
                "model": "mock-model",
                "messages": [{"role": "user", "content": "算一下 3+4"}],
                "tools": [
                    {
                        "type": "function",
                        "function": {
                            "name": "calculator",
                            "parameters": {"type": "object", "properties": {"expression": {"type": "string"}}},
                        },
                    }
                ],
            },
        )
    assert resp.status_code == 200
    choice = resp.json()["choices"][0]
    assert choice["finish_reason"] == "tool_calls"
    tool_call = choice["message"]["tool_calls"][0]
    assert tool_call["type"] == "function"
    assert tool_call["function"]["name"] == "calculator"
    assert "expression" in tool_call["function"]["arguments"]


async def test_健康检查报告mock模式(tmp_path, rich_seed: AccountSeed) -> None:
    async with make_env(tmp_path, [rich_seed]) as env:
        resp = await env.client.get("/health")
    assert resp.status_code == 200
    assert resp.json() == {"status": "ok", "mode": "mock"}
