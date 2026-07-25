"""连续步内容相似度。

基线：词级 n-gram Jaccard（离线可算，零依赖）。
增强项：哈希向量余弦相似度（embedding 相似度的离线替身——用 blake2b 把
token 投影到定维向量，不依赖外部 embedding 服务；真实部署可换成 embedding API）。
"""

from __future__ import annotations

import hashlib
import math
import re
from typing import Literal

_TOKEN_RE = re.compile(r"[a-z0-9]+|[一-鿿]")


def tokenize(text: str) -> list[str]:
    """小写词 token；CJK 按单字切（原型足够）。"""
    return _TOKEN_RE.findall(text.lower())


def _ngram_set(tokens: list[str], n: int) -> set[tuple[str, ...]]:
    if not tokens:
        return set()
    width = min(n, len(tokens))  # 短文本退化为 unigram，保证短步也可比
    return {tuple(tokens[i : i + width]) for i in range(len(tokens) - width + 1)}


def ngram_jaccard(a: str, b: str, n: int = 2) -> float:
    """两个文本 n-gram 集合的 Jaccard 相似度；双空文本视为完全相同。"""
    sa = _ngram_set(tokenize(a), n)
    sb = _ngram_set(tokenize(b), n)
    if not sa and not sb:
        return 1.0
    union = sa | sb
    if not union:
        return 1.0
    return len(sa & sb) / len(union)


def _hash_vector(text: str, dim: int) -> list[float]:
    """把 token 计数投影到 dim 维向量（符号由哈希次位决定，抗轴对齐偏置）。"""
    vec = [0.0] * dim
    for token in tokenize(text):
        digest = hashlib.blake2b(token.encode("utf-8"), digest_size=8).digest()
        value = int.from_bytes(digest, "big")
        vec[value % dim] += 1.0 if (value >> 63) == 0 else -1.0
    return vec


def hashvec_cosine(a: str, b: str, dim: int = 256) -> float:
    """哈希向量余弦，映射到 [0, 1]；双空文本视为完全相同。"""
    va = _hash_vector(a, dim)
    vb = _hash_vector(b, dim)
    na = math.sqrt(sum(x * x for x in va))
    nb = math.sqrt(sum(x * x for x in vb))
    if na == 0.0 and nb == 0.0:
        return 1.0
    if na == 0.0 or nb == 0.0:
        return 0.0
    dot = sum(x * y for x, y in zip(va, vb, strict=True))
    cosine = dot / (na * nb)
    return max(0.0, min(1.0, (cosine + 1.0) / 2.0))


def step_similarity(
    a: str,
    b: str,
    backend: Literal["ngram", "hashvec"] = "ngram",
    ngram: int = 2,
) -> float:
    """连续步内容相似度统一入口，后端由配置选择。"""
    if backend == "ngram":
        return ngram_jaccard(a, b, n=ngram)
    if backend == "hashvec":
        return hashvec_cosine(a, b)
    raise ValueError(f"未知相似度后端: {backend}")
