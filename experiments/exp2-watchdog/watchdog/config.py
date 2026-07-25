"""看门狗阈值配置：机制在陷阱侧，策略（阈值）配置化 —— 模拟 ELF 声明。"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
from typing import Literal

import yaml

SimilarityBackend = Literal["ngram", "hashvec"]


@dataclass(frozen=True, slots=True)
class WatchdogConfig:
    """阈值策略。默认值来自 llmos 议题 9 定案。"""

    max_steps_without_progress: int = 4
    max_repeat_similarity: float = 0.85
    repeat_window: int = 3
    artifact_novelty_threshold: float = 0.7
    ngram: int = 2
    similarity_backend: SimilarityBackend = "ngram"


_YAML_KEY_MAP = {
    "maxStepsWithoutProgress": "max_steps_without_progress",
    "maxRepeatSimilarity": "max_repeat_similarity",
    "repeatWindow": "repeat_window",
    "artifactNoveltyThreshold": "artifact_novelty_threshold",
    "ngram": "ngram",
    "similarityBackend": "similarity_backend",
}


def load_config(path: Path) -> WatchdogConfig:
    """从 YAML 声明加载阈值；未声明的字段用默认值。"""
    raw = yaml.safe_load(path.read_text(encoding="utf-8")) or {}
    unknown = set(raw) - set(_YAML_KEY_MAP)
    if unknown:
        raise ValueError(f"未知看门狗配置键: {sorted(unknown)}")
    kwargs = {_YAML_KEY_MAP[k]: v for k, v in raw.items()}
    return WatchdogConfig(**kwargs)
