"""语义存储层 typed 异常。"""

from __future__ import annotations


class AtomStoreError(Exception):
    """语义原子存储错误基类。"""


class ImmutableViolation(AtomStoreError):
    """试图修改/删除已写入的语义原子（议题 21 Q2：不可变 + 墓碑撤回）。"""


class ForbiddenVerifiedWrite(AtomStoreError):
    """非授权方法试图写入 verified（议题 26 修订 4：仅独立验证者/确定性 checker）。"""


class UnknownAtom(AtomStoreError):
    """引用了不存在的原子 id（lineage/link endpoints/verdict 目标）。"""


class ClosedSetViolation(AtomStoreError, ValueError):
    """冻结闭集枚举收到集外值（议题 24 宪法：枚举冻结的运行时体现）。"""
