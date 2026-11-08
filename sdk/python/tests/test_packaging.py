"""L-012 wheel 打包不变量:gen/python 生成模块必须进 wheel,protobuf 必须钉扎。

对位的机械验收(构建 wheel → 零漂移比对 → fresh venv 全链路 import)在
``sdk/python/scripts/verify_wheel.py``;本文件守 pyproject 配置本身,
使常规测试周期无需构建 wheel 就能抓住配置回退。
"""

from __future__ import annotations

import re
from pathlib import Path

SDK_PY = Path(__file__).resolve().parents[1]
PYPROJECT = SDK_PY / "pyproject.toml"
REPO = SDK_PY.parents[1]
GEN_PY = REPO / "gen" / "python"


def _pyproject_text() -> str:
    return PYPROJECT.read_text(encoding="utf-8")


def test_gen_python_namespace_packages_are_discovered() -> None:
    text = _pyproject_text()
    assert 'where = [".", "../../gen/python"]' in text, (
        "packages.find 必须把 gen/python 纳入多根发现,否则无 __init__.py 的 "
        "nlos 命名空间包被静默漏收(L-012)"
    )
    assert re.search(r"include\s*=\s*\[[^\]]*\"nlos\*\"", text), (
        "include 必须覆盖 nlos* 命名空间包"
    )
    assert "namespaces = true" in text, (
        "PEP 420 命名空间包发现必须显式开启"
    )


def test_protobuf_runtime_is_pinned() -> None:
    assert re.search(r'"protobuf==6\.33\.4"', _pyproject_text()), (
        "protobuf 必须显式钉 6.33.4:gen _pb2.py 由 buf python:v33.4 生成,"
        "运行时校验要求同代运行时(requirements-schema.txt 同钉)"
    )


def test_gen_python_modules_exist() -> None:
    modules = sorted(p.relative_to(GEN_PY).as_posix() for p in GEN_PY.rglob("*_pb2.py"))
    assert len(modules) == 9, f"预期 9 个生成模块,实得 {modules}"
