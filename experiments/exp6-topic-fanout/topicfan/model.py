"""领域模型：消息标签、订阅谓词（eBPF 类比）、选择性分布。

消息属性模型（两维，便于把任意目标选择性编译成具体谓词）：
- category: 均匀分布在 [0, NUM_CATEGORIES) 的整数标签（类比消息的主题词/标签集合命中）
- intensity: 均匀分布在 [0, 1) 的强度值（类比消息的属性数值）

谓词（订阅侧的过滤表达式，编译期固定 = eBPF 类比）：
- pass(msg) = msg.category ∈ categories AND msg.intensity ≥ threshold
- 期望选择性 = |categories| / NUM_CATEGORIES × (1 - threshold)

compile_predicate(target) 把目标选择性 s ∈ [0,1] 编译成具体谓词参数，
使得对随机消息的期望通过率恰为 s（解析保证，非统计近似）。
"""

from __future__ import annotations

import math
import random
from dataclasses import dataclass
from typing import NewType, Protocol

import numpy as np

SubscriberId = NewType("SubscriberId", int)
MessageId = NewType("MessageId", int)

NUM_CATEGORIES: int = 64

MSG_ID_SEED_OFFSET: int = 1_000_003


@dataclass(frozen=True, slots=True)
class Message:
    """一条发布到 Topic 的消息，带属性标签。"""

    msg_id: MessageId
    category: int
    intensity: float


@dataclass(frozen=True, slots=True)
class Predicate:
    """订阅者的过滤谓词：编译期决定选择性（eBPF 类比）。

    categories 为空 => 恒假谓词（selectivity = 0）。
    threshold ∈ [0, 1]；threshold = 1 且 categories 非空 => 选择性 0（intensity < 1 恒成立）。
    """

    categories: frozenset[int]
    threshold: float

    def evaluate(self, msg: Message) -> bool:
        return msg.category in self.categories and msg.intensity >= self.threshold

    def expected_selectivity(self, num_categories: int = NUM_CATEGORIES) -> float:
        """对均匀随机消息的期望通过概率（解析值）。"""
        if not self.categories:
            return 0.0
        return len(self.categories) / num_categories * (1.0 - self.threshold)


def compile_predicate(target: float, rng: random.Random) -> Predicate:
    """把目标选择性 target ∈ [0,1] 编译成具体谓词。

    构造：取 k = ceil(target × C) 个类别（k=0 时为空集），
    再调强度阈值 θ = 1 − target×C/k，使期望选择性 = k/C × (1−θ) = target 精确成立。
    """
    if not 0.0 <= target <= 1.0:
        msg = f"target selectivity out of [0,1]: {target}"
        raise ValueError(msg)
    if target == 0.0:
        return Predicate(categories=frozenset(), threshold=0.0)
    k = min(NUM_CATEGORIES, math.ceil(target * NUM_CATEGORIES))
    categories = frozenset(rng.sample(range(NUM_CATEGORIES), k))
    threshold = min(1.0, max(0.0, 1.0 - target * NUM_CATEGORIES / k))
    return Predicate(categories=categories, threshold=threshold)


class SelectivityDistribution(Protocol):
    """订阅者群体选择性的抽样分布（核心自变量）。"""

    def sample(self, n: int, rng: np.random.Generator) -> np.ndarray: ...


@dataclass(frozen=True, slots=True)
class UniformSelectivity:
    """均匀分布："大家关心差不多的事"。lo/hi 即选择性上下界。"""

    lo: float
    hi: float

    def sample(self, n: int, rng: np.random.Generator) -> np.ndarray:
        return rng.uniform(self.lo, self.hi, size=n)


@dataclass(frozen=True, slots=True)
class BimodalSelectivity:
    """双峰分布："兴趣分散"——多数紧谓词 + 少数宽谓词。

    loose_frac 比例的订阅者落在宽峰（loose_mean±loose_sd），
    其余落在紧峰（tight_mean±tight_sd）。截断到 (0,1]。
    """

    tight_mean: float
    tight_sd: float
    loose_mean: float
    loose_sd: float
    loose_frac: float

    def sample(self, n: int, rng: np.random.Generator) -> np.ndarray:
        is_loose = rng.random(n) < self.loose_frac
        out = np.where(
            is_loose,
            rng.normal(self.loose_mean, self.loose_sd, size=n),
            rng.normal(self.tight_mean, self.tight_sd, size=n),
        )
        return np.clip(out, 1e-9, 1.0)


@dataclass(frozen=True, slots=True)
class PowerLawSelectivity:
    """截断幂律分布："少数热门"——绝大多数订阅者极挑，极少数极宽。

    pdf ∝ s^(−alpha)，支撑集 [s_min, 1]。alpha=1 退化为对数均匀。
    逆 CDF 采样：s = (u·(1 − s_min^(1−α)) + s_min^(1−α))^(1/(1−α))。
    """

    s_min: float
    alpha: float

    def sample(self, n: int, rng: np.random.Generator) -> np.ndarray:
        u = rng.random(n)
        if self.alpha == 1.0:
            return np.exp(np.log(self.s_min) * (1.0 - u))
        exponent = 1.0 - self.alpha
        lo_term = self.s_min**exponent
        return (u * (1.0 - lo_term) + lo_term) ** (1.0 / exponent)


def sample_messages(n: int, seed: int) -> tuple[np.ndarray, np.ndarray]:
    """生成 n 条随机消息的属性数组（category, intensity），均匀分布。"""
    rng = np.random.default_rng(seed + MSG_ID_SEED_OFFSET)
    categories = rng.integers(0, NUM_CATEGORIES, size=n)
    intensities = rng.random(n)
    return categories, intensities
