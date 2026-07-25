"""WAL 批量组提交流水（议题 11 裂缝 1 定案的原型化）。

热路径只做内存追加（O(1)，零系统调用）；
后台协程按批次/间隔组提交落盘（JSONL），把写放大从"每笔一次 write"
降到"每批一次 write"。基准脚本对比"每笔同步落盘 vs 批量组提交"。
"""

from __future__ import annotations

import asyncio
import json
import time
from collections import deque
from dataclasses import asdict, dataclass, field
from pathlib import Path
from typing import TextIO


@dataclass(frozen=True, slots=True)
class WalRecord:
    """一笔扣减流水。agent key 不落盘，只记 agent_id 与指纹。"""

    seq: int
    ts: float
    request_id: str
    agent_id: str
    key_fingerprint: str
    model: str
    prompt_tokens: int
    completion_tokens: int
    cost: float
    charged: float
    balance_after: float
    stream: bool


@dataclass(slots=True)
class WalStats:
    """运行统计：基准与可观测用。"""

    appended: int = 0
    flushed: int = 0
    batches: int = 0
    last_flush_ts: float = 0.0


class WalWriter:
    """内存队列 + 批量组提交。append 为热路径，永不阻塞调用方。"""

    def __init__(self, path: Path, batch_size: int = 256, flush_interval_ms: float = 50.0) -> None:
        if batch_size < 1:
            raise ValueError("batch_size 必须 ≥ 1")
        self._path = path
        self._batch_size = batch_size
        self._flush_interval = flush_interval_ms / 1000.0
        self._pending: deque[WalRecord] = deque()
        self._seq = 0
        self._task: asyncio.Task[None] | None = None
        self._closed = False
        self._file: TextIO | None = None
        self.stats = WalStats()

    @property
    def running(self) -> bool:
        return self._task is not None and not self._task.done()

    @property
    def path(self) -> Path:
        return self._path

    async def flush_now(self) -> None:
        """立即按批次冲刷全部待提交记录（测试断言与关停前用）。"""
        while self._pending:
            self._flush_batch(min(len(self._pending), self._batch_size))

    def append(
        self,
        *,
        request_id: str,
        agent_id: str,
        key_fingerprint: str,
        model: str,
        prompt_tokens: int,
        completion_tokens: int,
        cost: float,
        charged: float,
        balance_after: float,
        stream: bool,
    ) -> WalRecord:
        """热路径：纯内存追加。这是扣减路径上唯一的 WAL 成本。"""
        if self._closed:
            raise RuntimeError("WalWriter 已关闭")
        self._seq += 1
        record = WalRecord(
            seq=self._seq,
            ts=time.time(),
            request_id=request_id,
            agent_id=agent_id,
            key_fingerprint=key_fingerprint,
            model=model,
            prompt_tokens=prompt_tokens,
            completion_tokens=completion_tokens,
            cost=cost,
            charged=charged,
            balance_after=balance_after,
            stream=stream,
        )
        self._pending.append(record)
        self.stats.appended += 1
        return record

    async def start(self) -> None:
        """启动后台组提交协程（幂等）。"""
        if self.running:
            return
        self._path.parent.mkdir(parents=True, exist_ok=True)
        self._file = self._path.open("a", encoding="utf-8")
        self._task = asyncio.create_task(self._commit_loop(), name="wal-commit")

    async def close(self) -> None:
        """关停并冲刷残余记录，保证落盘完整。"""
        if self._closed:
            return
        self._closed = True
        if self._task is not None:
            self._task.cancel()
            try:
                await self._task
            except asyncio.CancelledError:
                pass
        self._flush_batch(len(self._pending))
        if self._file is not None:
            self._file.close()
            self._file = None

    async def _commit_loop(self) -> None:
        """组提交循环：凑满批次立即提交，否则按间隔提交。"""
        while True:
            if len(self._pending) >= self._batch_size:
                self._flush_batch(self._batch_size)
                continue  # 积压时连续冲刷，不睡眠
            await asyncio.sleep(self._flush_interval)
            if self._pending:
                self._flush_batch(min(len(self._pending), self._batch_size))

    def _flush_batch(self, n: int) -> None:
        """把至多 n 条记录一次性写入并 flush（一次组提交 = 一次写系统调用）。"""
        if n <= 0 or self._file is None:
            return
        lines: list[str] = []
        for _ in range(min(n, len(self._pending))):
            record = self._pending.popleft()
            lines.append(json.dumps(asdict(record), ensure_ascii=False, separators=(",", ":")))
        if not lines:
            return
        self._file.write("\n".join(lines) + "\n")
        self._file.flush()
        self.stats.flushed += len(lines)
        self.stats.batches += 1
        self.stats.last_flush_ts = time.time()


class SyncWalWriter:
    """对照组：每笔同步落盘。仅用于基准对比，验证批量组提交的收益。"""

    def __init__(self, path: Path) -> None:
        self._path = path
        self._seq = 0
        self._file: TextIO | None = None

    def open(self) -> None:
        self._path.parent.mkdir(parents=True, exist_ok=True)
        self._file = self._path.open("a", encoding="utf-8")

    def append(self, **kwargs: object) -> None:
        """热路径上直接 write + flush（每笔一次系统调用）。"""
        if self._file is None:
            raise RuntimeError("SyncWalWriter 未 open")
        self._seq += 1
        record = {"seq": self._seq, "ts": time.time(), **kwargs}
        self._file.write(json.dumps(record, ensure_ascii=False, separators=(",", ":")) + "\n")
        self._file.flush()

    def close(self) -> None:
        if self._file is not None:
            self._file.close()
            self._file = None
