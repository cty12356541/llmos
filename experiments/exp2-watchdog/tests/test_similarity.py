"""相似度计算：n-gram Jaccard 基线 + 哈希向量增强项。"""

from __future__ import annotations

import pytest

from watchdog import hashvec_cosine, ngram_jaccard, step_similarity


class TestNgramJaccard:
    def test_identical_texts_give_one(self) -> None:
        assert ngram_jaccard("the quick brown fox", "the quick brown fox") == 1.0

    def test_disjoint_texts_give_zero(self) -> None:
        assert ngram_jaccard("alpha beta gamma", "delta epsilon zeta") == 0.0

    def test_both_empty_give_one(self) -> None:
        assert ngram_jaccard("", "") == 1.0

    def test_one_empty_gives_zero(self) -> None:
        assert ngram_jaccard("some words here", "") == 0.0

    def test_partial_overlap_between_zero_and_one(self) -> None:
        sim = ngram_jaccard("the quick brown fox jumps", "the quick brown dog runs")
        assert 0.0 < sim < 1.0

    def test_more_overlap_means_higher_similarity(self) -> None:
        base = "one two three four five six"
        high = ngram_jaccard(base, "one two three four five seven")
        low = ngram_jaccard(base, "one two eight nine ten eleven")
        assert high > low

    def test_short_text_falls_back_to_unigram(self) -> None:
        # 只有一个 token 时 bigram 集合退化为 unigram，仍可比
        assert ngram_jaccard("hello", "hello", n=2) == 1.0
        assert ngram_jaccard("hello", "world", n=2) == 0.0

    def test_cjk_text_tokenized_by_char(self) -> None:
        assert ngram_jaccard("继续排查问题", "继续排查问题") == 1.0
        assert 0.0 < ngram_jaccard("继续排查问题", "继续排查故障") < 1.0


class TestHashvecCosine:
    def test_identical_texts_near_one(self) -> None:
        assert hashvec_cosine("the quick brown fox", "the quick brown fox") >= 0.99

    def test_disjoint_texts_clearly_lower(self) -> None:
        sim = hashvec_cosine("alpha beta gamma delta", "xylon yttrium zirconium quartz")
        assert sim < 0.8

    def test_both_empty_give_one(self) -> None:
        assert hashvec_cosine("", "") == 1.0


class TestStepSimilarityDispatch:
    def test_ngram_backend_default(self) -> None:
        assert step_similarity("a b", "a b", backend="ngram") == 1.0

    def test_hashvec_backend(self) -> None:
        assert step_similarity("a b", "a b", backend="hashvec") >= 0.99

    def test_unknown_backend_raises(self) -> None:
        with pytest.raises(ValueError, match="未知相似度后端"):
            step_similarity("a", "a", backend="bogus")  # type: ignore[arg-type]
