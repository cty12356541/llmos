#!/usr/bin/env python3
"""L-012 机械验收:wheel 构建纳入 gen/python 生成模块且与 buf 钉扎输出零漂移,
全新临时 venv(无 llmos 源码树)安装后 nlos_sdk 与 9 个 nlos.sabi.v1 生成模块
全部可 import。

用法: python3 sdk/python/scripts/verify_wheel.py
退出码 0 = 全部通过;stdout 即机械回执(每步命令与判定)。"""

from __future__ import annotations

import hashlib
import subprocess
import sys
import tempfile
import venv
import zipfile
from pathlib import Path

SDK_PY = Path(__file__).resolve().parents[1]
REPO = SDK_PY.parents[1]
GEN_PY = REPO / "gen" / "python"
GEN_MODULES = sorted(p.relative_to(GEN_PY).as_posix() for p in GEN_PY.rglob("*_pb2.py"))

# fresh venv 里必须可 import 的全链路:nlos_sdk 自身各模块 + 全部生成模块 + 命名空间根
SDK_IMPORT_MODULES = [
    "nlos_sdk",
    "nlos_sdk.channel",
    "nlos_sdk.common",
    "nlos_sdk.local_rpc",
    "nlos_sdk.service_directory",
    "nlos",
]

PROBE = (
    "import importlib, os;"
    "mods = os.environ['NLOS_PROBE_MODULES'].split(',');"
    "[importlib.import_module(m) for m in mods];"
    "from nlos_sdk import Ed25519Signer;"
    "assert len(Ed25519Signer(bytes(32)).sign(b'probe')) == 64;"
    "print('IMPORT-OK', len(mods))"
)


def run(command: list[str], **kwargs: object) -> subprocess.CompletedProcess[str]:
    print(f"$ {' '.join(str(part) for part in command)}", flush=True)
    return subprocess.run(command, capture_output=True, text=True, **kwargs)  # type: ignore[arg-type]


def main() -> int:
    if not GEN_MODULES:
        print("FAIL: gen/python 下没有 *_pb2.py", file=sys.stderr)
        return 1
    print(f"[verify_wheel] gen/python 生成模块 {len(GEN_MODULES)} 个: {GEN_MODULES}")

    with tempfile.TemporaryDirectory(prefix="nlos-wheel-") as tmp_name:
        tmp = Path(tmp_name)
        dist = tmp / "dist"
        build = run(
            [sys.executable, "-m", "pip", "wheel", "--no-deps", "-w", str(dist), str(SDK_PY)]
        )
        if build.returncode != 0:
            print(build.stdout + build.stderr, file=sys.stderr)
            return 1
        wheels = sorted(dist.glob("nlos_sdk-*.whl"))
        if len(wheels) != 1:
            print(f"FAIL: 预期恰好 1 个 wheel,实得 {[w.name for w in wheels]}", file=sys.stderr)
            return 1
        wheel = wheels[0]
        print(f"[verify_wheel] built {wheel.name}")

        # 多根 where 的潜在副作用哨兵:构建不得在 sdk/ 下残留 gen/ 副本
        stray = SDK_PY.parent / "gen"
        if stray.exists():
            print(f"FAIL: 构建残留 {stray} 出现,疑似多根发现副作用,需清理排查", file=sys.stderr)
            return 1

        archive = zipfile.ZipFile(wheel)
        names = archive.namelist()
        missing = [module for module in GEN_MODULES if module not in names]
        if missing:
            print(f"FAIL: wheel 缺生成模块 {missing}(L-012 复现)", file=sys.stderr)
            return 1
        for module in GEN_MODULES:
            in_wheel = archive.read(module)
            on_disk = (GEN_PY / module).read_bytes()
            if hashlib.sha256(in_wheel).digest() != hashlib.sha256(on_disk).digest():
                print(f"FAIL: {module} 与 gen/ 钉扎输出漂移", file=sys.stderr)
                return 1
        print(f"[verify_wheel] {len(GEN_MODULES)} 个生成模块与 gen/ 逐字节一致(零漂移)")

        venv_dir = tmp / "venv"
        venv.create(venv_dir, with_pip=True)
        py = venv_dir / "bin" / "python"
        install = run(
            [
                str(py),
                "-m",
                "pip",
                "install",
                "--disable-pip-version-check",
                "--quiet",
                str(wheel),
            ]
        )
        if install.returncode != 0:
            print(install.stdout + install.stderr, file=sys.stderr)
            return 1
        print(f"[verify_wheel] fresh venv 安装 {wheel.name} 完成(依赖 protobuf==6.33.4 自 PyPI)")

        probe_modules = SDK_IMPORT_MODULES + [
            module[:-3].replace("/", ".") for module in GEN_MODULES
        ]
        import os

        result = run(
            [str(py), "-c", PROBE],
            env={**os.environ, "NLOS_PROBE_MODULES": ",".join(probe_modules)},
        )
        if result.returncode != 0 or "IMPORT-OK" not in result.stdout:
            print(result.stdout + result.stderr, file=sys.stderr)
            return 1
        print(
            f"[verify_wheel] fresh venv(无源码树)import 全链路 OK: "
            f"{result.stdout.strip()}"
        )

    print("[verify_wheel] ALL PASS")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
