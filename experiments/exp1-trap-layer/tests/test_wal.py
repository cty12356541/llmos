"""WAL 批量组提交判定：落盘正确性、批次语义、关停不丢记录。"""

from __future__ import annotations

import asyncio

from trap_layer.config import AccountSeed
from trap_layer.wal import WalWriter

from conftest import make_env, read_wal


def _append_n(wal: WalWriter, n: int) -> None:
    for i in range(n):
        wal.append(
            request_id=f"req-{i}",
            agent_id="bench-agent",
            key_fingerprint="...test",
            model="mock-model",
            prompt_tokens=1,
            completion_tokens=1,
            cost=1.5,
            charged=1.5,
            balance_after=100.0 - i,
            stream=False,
        )


async def test_批量落盘内容完整且有序(tmp_path) -> None:
    wal = WalWriter(tmp_path / "a.wal.jsonl", batch_size=8, flush_interval_ms=5)
    await wal.start()
    _append_n(wal, 25)
    await wal.flush_now()
    records = read_wal(wal.path)
    await wal.close()
    # Given 25 笔追加 / When 组提交落盘 / Then 记录全量、有序、字段可往返
    assert len(records) == 25
    assert [r["seq"] for r in records] == list(range(1, 26))
    assert records[0]["request_id"] == "req-0"
    assert records[24]["balance_after"] == 100.0 - 24


async def test_定时组提交在间隔内自动落盘(tmp_path) -> None:
    wal = WalWriter(tmp_path / "b.wal.jsonl", batch_size=1000, flush_interval_ms=10)
    await wal.start()
    _append_n(wal, 3)
    # 不手动 flush：等两个间隔，后台协程应已自动提交
    await asyncio.sleep(0.05)
    records = read_wal(wal.path)
    await wal.close()
    assert len(records) == 3


async def test_关停时残余记录不丢(tmp_path) -> None:
    wal = WalWriter(tmp_path / "c.wal.jsonl", batch_size=1000, flush_interval_ms=60_000)
    await wal.start()
    _append_n(wal, 7)
    await wal.close()  # 间隔很远，但 close 必须冲刷残余
    records = read_wal(wal.path)
    assert len(records) == 7


async def test_组提交批次统计(tmp_path) -> None:
    wal = WalWriter(tmp_path / "d.wal.jsonl", batch_size=4, flush_interval_ms=5)
    await wal.start()
    _append_n(wal, 10)
    await wal.flush_now()
    await wal.close()
    assert wal.stats.appended == 10
    assert wal.stats.flushed == 10
    assert wal.stats.batches >= 3  # 10 条 / 批 4 → 至少 3 批


async def test_代理路径流水经组提交落盘(tmp_path, rich_seed: AccountSeed) -> None:
    async with make_env(tmp_path, [rich_seed]) as env:
        for content in ("第一问", "第二问"):
            await env.client.post(
                "/v1/chat/completions",
                headers=env.auth(rich_seed.key),
                json={"model": "mock-model", "messages": [{"role": "user", "content": content}]},
            )
        await env.wal.flush_now()
        records = read_wal(env.wal.path)
    assert len(records) == 2
    assert all(r["charged"] > 0 for r in records)
    assert all(r["agent_id"] == rich_seed.agent_id for r in records)
