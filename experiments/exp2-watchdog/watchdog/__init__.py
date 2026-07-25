"""内核看门狗（第一级：结构信号 + 阈值）原型。"""

from .config import WatchdogConfig, load_config
from .core import SuspensionEvent, Watchdog
from .signals import ProgressSignals, SignalTracker, StepObservation, ToolCallView
from .similarity import hashvec_cosine, ngram_jaccard, step_similarity

__all__ = [
    "ProgressSignals",
    "SignalTracker",
    "StepObservation",
    "SuspensionEvent",
    "ToolCallView",
    "Watchdog",
    "WatchdogConfig",
    "hashvec_cosine",
    "load_config",
    "ngram_jaccard",
    "step_similarity",
]
