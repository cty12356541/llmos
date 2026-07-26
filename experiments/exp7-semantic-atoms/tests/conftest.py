"""公共 fixture：注入假时钟的内存存储（无墙钟依赖）。"""

from __future__ import annotations

import pytest

from sematom import AtomStore


class FakeClock:
    def __init__(self, start: float = 1_000.0):
        self.now = start

    def __call__(self) -> float:
        return self.now

    def advance(self, seconds: float) -> None:
        self.now += seconds


@pytest.fixture
def clock() -> FakeClock:
    return FakeClock()


@pytest.fixture
def store(clock: FakeClock):
    s = AtomStore(":memory:", clock=clock)
    yield s
    s.close()
