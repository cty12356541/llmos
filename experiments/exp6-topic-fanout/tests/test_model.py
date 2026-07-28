"""谓词求值语义与选择性抽样分布测试（Given/When/Then）。"""

import random

import numpy as np
import pytest

from topicfan.model import (
    NUM_CATEGORIES,
    BimodalSelectivity,
    Message,
    MessageId,
    PowerLawSelectivity,
    Predicate,
    UniformSelectivity,
    compile_predicate,
    sample_messages,
)


def _msg(category: int, intensity: float) -> Message:
    return Message(msg_id=MessageId(0), category=category, intensity=intensity)


class TestPredicateEvaluate:
    def test_passes_when_category_in_set_and_intensity_above_threshold(self):
        # Given 谓词 {3,7}, θ=0.5
        pred = Predicate(categories=frozenset({3, 7}), threshold=0.5)
        # When 消息 category=7, intensity=0.9
        # Then 通过
        assert pred.evaluate(_msg(7, 0.9)) is True

    def test_rejects_when_category_not_in_set(self):
        # Given 谓词 {3,7}, θ=0.5
        pred = Predicate(categories=frozenset({3, 7}), threshold=0.5)
        # When 消息 category=4（不在集合），intensity 再高
        # Then 拒绝
        assert pred.evaluate(_msg(4, 0.99)) is False

    def test_rejects_when_intensity_below_threshold(self):
        # Given 谓词 {3}, θ=0.5
        pred = Predicate(categories=frozenset({3}), threshold=0.5)
        # When intensity=0.49 < θ
        # Then 拒绝
        assert pred.evaluate(_msg(3, 0.49)) is False

    def test_boundary_intensity_equal_threshold_passes(self):
        # Given θ=0.5
        pred = Predicate(categories=frozenset({3}), threshold=0.5)
        # When intensity == θ（闭区间语义）
        # Then 通过
        assert pred.evaluate(_msg(3, 0.5)) is True

    def test_empty_categories_is_always_false(self):
        # Given 空类别集（恒假谓词）
        pred = Predicate(categories=frozenset(), threshold=0.0)
        # Then 任何消息都不通过
        assert pred.evaluate(_msg(0, 0.0)) is False
        assert pred.evaluate(_msg(NUM_CATEGORIES - 1, 0.999)) is False


class TestExpectedSelectivity:
    def test_full_categories_zero_threshold_is_one(self):
        pred = Predicate(categories=frozenset(range(NUM_CATEGORIES)), threshold=0.0)
        assert pred.expected_selectivity() == pytest.approx(1.0)

    def test_empty_is_zero(self):
        pred = Predicate(categories=frozenset(), threshold=0.0)
        assert pred.expected_selectivity() == 0.0

    def test_formula_half_categories_half_threshold(self):
        # k=32/64 × (1−0.5) = 0.25
        pred = Predicate(categories=frozenset(range(32)), threshold=0.5)
        assert pred.expected_selectivity() == pytest.approx(0.25)


class TestCompilePredicate:
    @pytest.mark.parametrize("target", [0.0, 0.01, 0.05, 0.25, 0.5, 0.9, 0.99, 1.0])
    def test_compiled_selectivity_hits_target(self, target: float):
        # Given 目标选择性
        rng = random.Random(42)
        # When 编译成谓词
        pred = compile_predicate(target, rng)
        # Then 期望选择性 == 目标（解析精确，非统计）
        assert pred.expected_selectivity() == pytest.approx(target, abs=1e-12)

    def test_compile_is_deterministic_per_seed(self):
        a = compile_predicate(0.3, random.Random(7))
        b = compile_predicate(0.3, random.Random(7))
        assert a == b

    def test_compile_zero_gives_empty_categories(self):
        pred = compile_predicate(0.0, random.Random(1))
        assert pred.categories == frozenset()

    def test_target_out_of_range_raises(self):
        with pytest.raises(ValueError, match="selectivity"):
            compile_predicate(1.5, random.Random(0))

    def test_empirical_pass_rate_matches_expected(self):
        # Given 编译目标 s=0.05 的谓词
        pred = compile_predicate(0.05, random.Random(11))
        cats, ints = sample_messages(200_000, seed=1)
        # When 对 20 万条均匀随机消息求经验通过率（向量化手算，与 evaluate 同语义）
        cat_hit = np.isin(cats, list(pred.categories))
        passed = cat_hit & (ints >= pred.threshold)
        # Then 经验值 ≈ 解析期望（±3σ 容差内取 0.5pp）
        assert passed.mean() == pytest.approx(pred.expected_selectivity(), abs=5e-3)


class TestSelectivityDistributions:
    def test_uniform_range_and_seed_determinism(self):
        dist = UniformSelectivity(lo=0.1, hi=0.5)
        a = dist.sample(10_000, np.random.default_rng(3))
        b = dist.sample(10_000, np.random.default_rng(3))
        np.testing.assert_array_equal(a, b)
        assert a.min() >= 0.1 and a.max() < 0.5
        assert a.mean() == pytest.approx(0.3, abs=0.01)

    def test_bimodal_cluster_fraction_and_gap(self):
        # Given 80% 紧峰(0.01) + 20% 宽峰(0.4)
        dist = BimodalSelectivity(
            tight_mean=0.01, tight_sd=0.003, loose_mean=0.4, loose_sd=0.05, loose_frac=0.2
        )
        s = dist.sample(50_000, np.random.default_rng(5))
        # Then 约 20% 样本 > 0.1（宽峰），且两峰可分离
        loose_ratio = (s > 0.1).mean()
        assert loose_ratio == pytest.approx(0.2, abs=0.01)
        assert np.all((s > 0) & (s <= 1.0))

    def test_powerlaw_most_tight_few_loose(self):
        # Given α=2, s_min=0.001 的幂律
        dist = PowerLawSelectivity(s_min=0.001, alpha=2.0)
        s = dist.sample(100_000, np.random.default_rng(9))
        # Then ≥90% 样本 < 0.1（绝大多数极挑），但存在 > 0.5 的少数宽谓词
        assert (s < 0.1).mean() > 0.9
        assert (s > 0.5).any()
        assert s.min() >= 0.001 and s.max() <= 1.0

    def test_powerlaw_alpha1_is_log_uniform(self):
        # α=1 时对数均匀：log10 空间近似均匀 → 中位数量级 ≈ sqrt(s_min)
        dist = PowerLawSelectivity(s_min=1e-4, alpha=1.0)
        s = dist.sample(100_000, np.random.default_rng(13))
        assert np.median(s) == pytest.approx(1e-2, rel=0.1)

    def test_powerlaw_mean_below_uniform_mean(self):
        # 幂律（少数热门）的均值选择性应显著低于对称均匀
        pl = PowerLawSelectivity(s_min=0.001, alpha=2.0).sample(
            100_000, np.random.default_rng(17)
        )
        un = UniformSelectivity(lo=0.0, hi=1.0).sample(100_000, np.random.default_rng(17))
        assert pl.mean() < un.mean() / 5
