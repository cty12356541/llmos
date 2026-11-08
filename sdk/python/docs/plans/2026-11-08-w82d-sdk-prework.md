# W82-D 屏障前先行件 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 两件不依赖 W82-A/B 新产出的先行件——(1) L-012 wheel 打包把 `gen/python/nlos/**` 生成模块纳入 wheel 并钉 protobuf 运行时;(2) 镜像 Rust 冻结契约的 Ed25519 挑战应答持久会话客户端——外加把 `tests/conformance/ipc/channel_control.py` 修到可稳定重复跑通。

**Architecture:** 件一用 setuptools 多根 `packages.find`(`where = [".", "../../gen/python"]` + `namespaces = true`)让 wheel 直接从钉扎 buf 输出收生成模块——单一来源、无 vendor 副本可漂移;配一个机械验收脚本(构建 wheel → 与 gen/ 逐字节比对 → 全新临时 venv 安装 → import 全链路)。件二分三层:`_signing.py`(Ed25519 薄签名层,隔离唯一非 protobuf 依赖,选型可替换)、`handshake.py`(契约纯函数镜像:digest/binding/编解码+边界校验)、`principal_session.py`(认证会话:握手后把同一条流转交 `LocalRpcClient`,继承其背压/毒化语义);`ChannelClient` 增 `connect_authenticated` 工厂,占位身份字节改会话派生。Rust 兼容性由对 `system-control-daemon --auth-socket` 的真实进程 e2e 证明。

**Tech Stack:** Python ≥3.11 asyncio、protobuf 6.33.4(钉扎,与 `requirements-schema.txt`/buf `python:v33.4` 对齐)、PyNaCl(推荐,待 maintainer 签认)、setuptools ≥61、pytest。

**Spec:** 主入口任务书(W82-D 先行件,维护者 2026-11-08 批准「按推荐的来」);Rust 冻结契约 `crates/nlos-ipc/src/handshake.rs:144-155`、`crates/nlos-ipc/src/handshake/transport.rs:85-105`、`crates/nlos-ipc/src/lib.rs:398-477`;ADR-0011。

## Global Constraints

- **写集(AGENTS.md 第 11 条)**:只写 `sdk/python/**`、`tests/conformance/ipc/**`。禁碰 `sdk/typescript/**`、`gen/**`、`crates/**`、`.github/**`、`docs/management/**`、`claims.yaml`。若实现发现必须越写集,停下报告主入口。
- **protobuf 依赖**:显式钉 `protobuf==6.33.4`(gen `_pb2.py` 头部 `Protobuf Python Version: 6.33.4`,`requirements-schema.txt` 同钉)。
- **提交作者**:`cty12356541 <171764500+cty12356541@users.noreply.github.com>`;格式 `<type>(<scope>): <subject>`(中文祈使句),波次号 `(W82-D)` 入 subject 尾部;只 `git add` 精确文件;提交前 `git log origin/main..HEAD` + `git diff --staged`。
- **不做**:TS SDK、LlmGateway/ToolSandbox 客户端、WaitClient/OperationClient 端到端、common.py 的 v1.1 登记。
- **回执纪律**:每件留机械回执(实际命令+输出+未运行项);CI 接线、claims.yaml、docs/evidence 归 INT;Ed25519 选型「待 maintainer 签认」。
- **测试先红后绿**(superpowers:test-driven-development);完成前 verification-before-commpletion。
- Windows 命名管道路径字节的 binding 计算以参数化测试用例覆盖并标注**未实机核验**(本机 macOS)。

## 冻结契约速查(实现必须逐字节镜像)

| 项 | 值 | 出处 |
|---|---|---|
| 握手消息域 | `b"llmos/principal-handshake/v1"` | handshake.rs:62 |
| 通道绑定域 | `b"llmos/ipc-channel-binding/v1"` | transport.rs:72 |
| 摘要 | `SHA256(domain ‖ nonce[32] ‖ principal[16] ‖ channel_binding)` | handshake.rs:144-155 |
| 端点绑定 | `SHA256(binding_domain ‖ endpoint_path_bytes)` | transport.rs:85-90 |
| Unix path 字节 | OS 字符串原始字节(=`os.fsencode`) | transport.rs:94-97 |
| Windows path 字节 | 管道名 UTF-16 码元按本机字节序排字节(`encode_wide().flat_map(to_ne_bytes)`) | transport.rs:99-107 |
| 签名 | 标准 Ed25519(pure)对 32 字节摘要签名,64 字节;Rust 侧 `verify_strict` | identity lib.rs:913+ |
| 帧法 | 4 字节大端长度前缀 + protobuf 字节 | lib.rs:3-4, 316-379 |
| nonce/principal/sig/binding 长度 | 32 / 16 / 64 / `1..=256` | nlos-schema lib.rs:84-89 |
| 握手 schema 名 | `nlos.sabi.PrincipalHandshake`,major 1 minor 0 | nlos-schema lib.rs:43,193 |
| 毒化语义 | 进 await 前 `usable=false`;仅 send 前 FrameTooLarge(零字节上网)恢复;接收/解码/关联失配保持毒化;并发第二调用立即 Backpressure | lib.rs:398-477 |
| 服务端(现状) | 每连接 = 1 次握手 + 1 次 exchange 后关连接(`serve_one`);nonce 一次性 | daemon.rs:1200-1250 |

**金向量**(作者期用 `/usr/bin/shasum -a 256` 独立计算,非 Python hashlib):

- 摘要向量:nonce=`000102…1f`(bytes(range(32))),principal=`11`*16,binding=`a5`*32 →
  `d8137d26b194334870f733a179ed4c93b6ce0a6a7b1039a6b1c7a66a97c91e01`
- Unix 绑定向量:`/tmp/nlos-test.sock` →
  `8f32c84889dc916ffc10d2b2dc9f9a5a4608a447b38dce1947a5fab3496cb8f9`
- Windows 绑定向量:`\\.\pipe\nlos-test` 的 UTF-16-LE 字节
  `5c005c002e005c0070006900700065005c006e006c006f0073002d007400650073007400` →
  `e1736841b7a49dc10506ef807f0fa6ca1409a13b7edd2d22b4a8c5890532e067`(未实机核验)
- Ed25519 KAT:RFC 8032 §7.1 TEST 1(seed `9d61…7f60`,空消息,签名
  `e5564300c360ac72…38e7a100b`)——证明薄签名层产出标准 Ed25519,与库无关。

---

### Task 1: L-012 红测试——打包守卫单测 + wheel 机械验收脚本

**Files:**
- Create: `sdk/python/tests/__init__.py`(空)
- Create: `sdk/python/tests/test_packaging.py`
- Create: `sdk/python/scripts/verify_wheel.py`

**Interfaces:**
- Consumes: `sdk/python/pyproject.toml`(现状)、`gen/python/nlos/**`(9 个 `_pb2.py`,只读)
- Produces: `verify_wheel.py`(退出码 0=过;stderr 明细);测试断言 pyproject 不变量

- [x] **Step 1: 写守卫单测(失败态)**

```python
# sdk/python/tests/test_packaging.py
"""L-012 wheel 打包不变量:gen/python 生成模块必须进 wheel,protobuf 必须钉扎。"""
import re
from pathlib import Path

import pytest

SDK_PY = Path(__file__).resolve().parents[1]
PYPROJECT = SDK_PY / "pyproject.toml"
REPO = SDK_PY.parents[1]
GEN_PY = REPO / "gen" / "python"


def _pyproject_text() -> str:
    return PYPROJECT.read_text(encoding="utf-8")


def test_gen_python_namespace_packages_are_discovered() -> None:
    text = _pyproject_text()
    assert 'where = [".", "../../gen/python"]' in text, (
        "packages.find 必须把 gen/python 纳入发现根(多根),否则 nlos 命名空间包静默漏收"
    )
    assert re.search(r'include\s*=\s*\[[^\]]*"nlos\*"', text), (
        "include 必须覆盖 nlos* 命名空间包"
    )
    assert "namespaces = true" in text


def test_protobuf_runtime_is_pinned() -> None:
    assert re.search(r'"protobuf==6\.33\.4"', _pyproject_text()), (
        "protobuf 必须显式钉 6.33.4(gen _pb2.py 生成器版本)"
    )


def test_gen_python_modules_exist() -> None:
    modules = sorted(p.relative_to(GEN_PY).as_posix() for p in GEN_PY.rglob("*_pb2.py"))
    assert len(modules) == 9, f"预期 9 个生成模块,实得 {modules}"
```

- [x] **Step 2: 写机械验收脚本(构建→逐字节比对→fresh venv 全链路 import)**

```python
#!/usr/bin/env python3
"""L-012 机械验收:wheel 构建纳入 gen/python 生成模块且与 buf 输出零漂移,
全新临时 venv 安装后 nlos_sdk 与 9 个 nlos.sabi.v1 生成模块全部可 import。

用法: python3 sdk/python/scripts/verify_wheel.py
退出码 0 = 全部通过;输出即机械回执。"""
from __future__ import annotations

import hashlib
import shutil
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


def run(command: list[str], **kwargs: object) -> subprocess.CompletedProcess[str]:
    print(f"$ {' '.join(str(c) for c in command)}", flush=True)
    return subprocess.run(command, capture_output=True, text=True, **kwargs)  # type: ignore[arg-type]


def main() -> int:
    if not GEN_MODULES:
        print("FAIL: gen/python 下没有 *_pb2.py", file=sys.stderr)
        return 1
    print(f"[verify_wheel] gen/python 生成模块 {len(GEN_MODULES)} 个: {GEN_MODULES}")

    with tempfile.TemporaryDirectory(prefix="nlos-wheel-") as tmp_name:
        tmp = Path(tmp_name)
        dist = tmp / "dist"
        build = run([sys.executable, "-m", "pip", "wheel", "--no-deps", "-w", str(dist), str(SDK_PY)])
        if build.returncode != 0:
            print(build.stdout + build.stderr, file=sys.stderr)
            return 1
        wheels = sorted(dist.glob("nlos_sdk-*.whl"))
        if len(wheels) != 1:
            print(f"FAIL: 预期恰好 1 个 wheel,实得 {wheels}", file=sys.stderr)
            return 1
        wheel = wheels[0]
        print(f"[verify_wheel] built {wheel.name}")

        names = zipfile.ZipFile(wheel).namelist()
        missing = [m for m in GEN_MODULES if m not in names]
        if missing:
            print(f"FAIL: wheel 缺生成模块 {missing}(L-012 复现)", file=sys.stderr)
            return 1
        for module in GEN_MODULES:
            in_wheel = zipfile.ZipFile(wheel).read(module)
            on_disk = (GEN_PY / module).read_bytes()
            if hashlib.sha256(in_wheel).digest() != hashlib.sha256(on_disk).digest():
                print(f"FAIL: {module} 与 gen/ 输出漂移", file=sys.stderr)
                return 1
        print(f"[verify_wheel] {len(GEN_MODULES)} 个生成模块与 gen/ 逐字节一致(零漂移)")

        venv_dir = tmp / "venv"
        venv.create(venv_dir, with_pip=True)
        py = venv_dir / "bin" / "python"
        pip = [str(py), "-m", "pip", "install", "--disable-pip-version-check", "--quiet"]
        install = run(pip + [str(wheel)])
        if install.returncode != 0:
            print(install.stdout + install.stderr, file=sys.stderr)
            return 1
        probe = (
            "import nlos_sdk, importlib, sys;"
            "mods=['nlos_sdk','nlos_sdk.channel','nlos_sdk.common','nlos_sdk.local_rpc',"
            f"{GEN_MODULES!r}+['nlos']];"
            "[importlib.import_module(m) for m in mods];"
            "print('IMPORT-OK', len(mods))"
        ).replace("'", '"')
        result = run([str(py), "-c", probe])
        if result.returncode != 0 or "IMPORT-OK" not in result.stdout:
            print(result.stdout + result.stderr, file=sys.stderr)
            return 1
        print(f"[verify_wheel] fresh venv(无源码树)import 全链路 OK: {result.stdout.strip()}")

    print("[verify_wheel] ALL PASS")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
```

- [x] **Step 3: 跑红,留失败回执**

Run: `python3 -m pytest sdk/python/tests/test_packaging.py -v`(需 pytest;若环境无,先 `pip install pytest` 并记入回执)
Expected: FAIL(两断言均不过)。
Run: `python3 sdk/python/scripts/verify_wheel.py`
Expected: 退出 1,`FAIL: wheel 缺生成模块 [...]`。
留全量输出作红回执。

### Task 2: L-012 绿——pyproject 修复 + 验收全绿 + 提交

**Files:**
- Modify: `sdk/python/pyproject.toml`

**Interfaces:**
- Produces: wheel 含 `nlos_sdk/**` 与 `nlos/sabi/v1/**`(9 模块);`protobuf==6.33.4` 依赖

- [x] **Step 1: 修 pyproject**

```toml
[build-system]
requires = ["setuptools>=61.0"]
build-backend = "setuptools.build_meta"

[project]
name = "nlos-sdk"
version = "0.1.0"
description = "Python SDK for NLOS SABI local RPC, ServiceDirectory negotiation, and common metadata validation"
readme = "README.md"
requires-python = ">=3.11"
dependencies = [
    "protobuf==6.33.4",
]

[tool.setuptools.packages.find]
where = [".", "../../gen/python"]
include = ["nlos_sdk*", "nlos*"]
namespaces = true
```

注:多根 `where` 把 `../../gen/python` 的 buf 钉扎输出直接纳入 wheel(单一来源,无 vendor 副本可漂移;sdist 不支持——本包只以 wheel 分发,README 注明)。本机制已在 /tmp 沙箱用最小仿形包验证(wheel 内容含 `ns/inner/mod.py`)。

- [x] **Step 2: 跑绿**

Run: `python3 -m pytest sdk/python/tests/test_packaging.py -v` → PASS
Run: `python3 sdk/python/scripts/verify_wheel.py` → `ALL PASS`,留全量输出作绿回执。

- [x] **Step 3: 提交**

```bash
git add sdk/python/pyproject.toml sdk/python/tests/__init__.py sdk/python/tests/test_packaging.py sdk/python/scripts/verify_wheel.py
git commit  # feat(nlos-sdk): wheel 打包纳入 gen/python 生成模块并钉 protobuf 运行时 (W82-D-L012)
```

正文附验证数据(红→绿输出摘要)。

### Task 3: Ed25519 薄签名层(红→绿)

**Files:**
- Create: `sdk/python/nlos_sdk/_signing.py`
- Test: `sdk/python/tests/test_signing.py`

**Interfaces:**
- Produces: `Ed25519Signer(seed: bytes)`;`.sign(message: bytes) -> bytes(64)`;`.public_key -> bytes(32)`;`SigningUnavailableError`。SDK 内唯一 import PyNaCl 的模块。

**选型对比(待 maintainer 签认,推荐 PyNaCl):**

| 维度 | PyNaCl(推荐) | cryptography |
|---|---|---|
| 绑定库 | libsodium(C,专注安全原语,格式化审计) | OpenSSL/Rust 全栈 |
| API 面 | 极小:sign/verify/seal 恰合本需 | 巨大:X.509/TLS/…攻击面远超所需 |
| Ed25519 互操作 | 标准 RFC 8032,Rust `verify_strict` 接受 | 同左 |
| 维护 | pyca 托管,发布节奏慢但 API 冻结 | pyca 托管,高频发布 |
| 二进制体积 | 小 | 大 |

H7 纪律(SDK 首个非 protobuf 依赖)取最小攻击面;薄层隔离使替换只动 `_signing.py` 一个文件。

- [x] **Step 1: 红测试(RFC 8032 KAT)**

```python
# sdk/python/tests/test_signing.py
"""薄签名层必须产出标准 Ed25519(RFC 8032 §7.1 TEST 1 KAT)。"""
import pytest

from nlos_sdk._signing import Ed25519Signer

RFC8032_SEED = bytes.fromhex(
    "9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60"
)
RFC8032_SIG = bytes.fromhex(
    "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555"
    "fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b"
)


def test_rfc8032_test1_empty_message() -> None:
    signer = Ed25519Signer(RFC8032_SEED)
    assert signer.sign(b"") == RFC8032_SIG


def test_sign_is_deterministic_and_stable() -> None:
    signer = Ed25519Signer(RFC8032_SEED)
    a = signer.sign(b"\x00" * 32)
    b = signer.sign(b"\x00" * 32)
    assert a == b and len(a) == 64


def test_seed_shape_is_enforced() -> None:
    with pytest.raises(ValueError):
        Ed25519Signer(b"short")


def test_public_key_matches_reference() -> None:
    # RFC 8032 TEST 1 公钥
    assert Ed25519Signer(RFC8032_SEED).public_key == bytes.fromhex(
        "d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a"
    )
```

- [x] **Step 2: 跑红** — `python3 -m pytest sdk/python/tests/test_signing.py -v` → FAIL(ModuleNotFoundError: nlos_sdk._signing)
- [x] **Step 3: 实现 `_signing.py`**(如上 Interfaces 描述;lazily import PyNaCl,缺失时抛 `SigningUnavailableError` 带安装指引;seed 强制 32 字节)
- [x] **Step 4: 跑绿** → PASS ×4
- [x] **Step 5: 与 Task 4 合并提交(见 Task 4 Step 5)**

### Task 4: handshake 契约镜像模块(红→绿)

**Files:**
- Create: `sdk/python/nlos_sdk/handshake.py`
- Modify: `sdk/python/nlos_sdk/__init__.py`(导出新公共名)
- Test: `sdk/python/tests/test_handshake.py`

**Interfaces:**
- Consumes: `nlos_sdk._signing.Ed25519Signer`(仅测试用);`nlos.sabi.v1.principal_handshake_pb2`
- Produces:
  - `HANDSHAKE_MESSAGE_DOMAIN: bytes`、`CHANNEL_BINDING_DOMAIN: bytes`、`HANDSHAKE_NONCE_BYTES=32`、`HANDSHAKE_SIGNATURE_BYTES=64`、`MAX_HANDSHAKE_CHANNEL_BINDING_BYTES=256`、`HANDSHAKE_SCHEMA_NAME="nlos.sabi.PrincipalHandshake"`、`HANDSHAKE_SCHEMA_MAJOR=1`、`HANDSHAKE_SCHEMA_MINOR=0`
  - `principal_handshake_message(nonce: bytes, principal_id: bytes, channel_binding: bytes) -> bytes(32)`
  - `endpoint_path_bytes(endpoint: str, platform: str | None = None) -> bytes`(platform 缺省取 `sys.platform`)
  - `endpoint_channel_binding(endpoint: str, platform: str | None = None) -> bytes(32)`
  - `decode_challenge(wire: bytes) -> PrincipalHandshakeChallenge`(nonce==32、schema 名/主版本校验,失配抛 `IpcError("COMPATIBILITY", …)`)
  - `build_attestation(principal_id: bytes, nonce: bytes, channel_binding: bytes, signature: bytes) -> bytes`(wire;长度校验 16/32/1..=256/64;schema 填 `nlos.sabi.PrincipalHandshake` v1.0)

- [x] **Step 1: 红测试(金向量 + 边界校验)** — 覆盖:摘要向量、Unix 绑定向量、Windows 绑定向量(参数化,docstring 标注「未实机核验:Windows 管道名 UTF-16LE 字节序由 transport.rs:99-107 镜像,未在实机 Windows 对拍」)、path 字节编码表、`decode_challenge` 对坏 nonce/坏 schema 名拒绝、`build_attestation` 各长度失配拒绝、build→ParseFromString→字段往返一致。
- [x] **Step 2: 跑红** → FAIL(模块不存在)
- [x] **Step 3: 实现 `handshake.py`** — 纯函数,无 I/O;hashlib.sha256;`endpoint_path_bytes`:unix → `os.fsencode(endpoint)`;win32 → `endpoint.encode("utf-16-le" if sys.byteorder=="little" else "utf-16-be")`(镜像 `encode_wide().flat_map(u16::to_ne_bytes)`;Windows 现实全 LE,be 分支为镜像完备性)
- [x] **Step 4: 跑绿** → 全 PASS
- [x] **Step 5: 提交**

```bash
git add sdk/python/nlos_sdk/_signing.py sdk/python/nlos_sdk/handshake.py sdk/python/nlos_sdk/__init__.py sdk/python/tests/test_signing.py sdk/python/tests/test_handshake.py
git commit  # feat(nlos-sdk): Ed25519 挑战应答握手契约镜像与薄签名层 (W82-D)
```

### Task 5: 认证持久会话 + ChannelClient 会话化(红→绿)

**Files:**
- Create: `sdk/python/nlos_sdk/principal_session.py`
- Modify: `sdk/python/nlos_sdk/local_rpc.py`(导出 `_write_frame` 辅助,`_exchange` 不动)
- Modify: `sdk/python/nlos_sdk/channel.py`(`connect_authenticated` 工厂;身份默认改会话派生;`DEFAULT_*` 常量保留导出但不再是线上默认)
- Test: `sdk/python/tests/test_principal_session.py`(含进程内 Python 测试服务器)

**Interfaces:**
- Consumes: `handshake.*`、`_signing.Ed25519Signer`、`local_rpc.LocalRpcClient/_read_frame/_write_frame/_open_platform_stream`
- Produces:
  - `SessionIdentity(principal_id, application_id, process_id, process_generation=1)`;`SessionIdentity.derive(principal_id) -> SessionIdentity`(app/process 各 `secrets.token_bytes(16)`)
  - `connect_authenticated(endpoint: str, signer: Ed25519Signer, principal_id: bytes, config: TransportConfig | None = None) -> LocalRpcClient` — 连接 → 收挑战帧 → 解码 → `endpoint_channel_binding(endpoint)` → 摘要 → 签名 → 构造证明帧 → 发送 → 把同一 reader/writer 交给 `LocalRpcClient`(此后多轮顺序复用、背压、毒化全继承;注意:Rust 侧证明后无 ack,接受与否由首轮 exchange 揭晓——docstring 写明)
  - `ChannelClient.connect_authenticated(cls, endpoint, *, signer, principal_id, config=None, application_id=None, process_id=None, **kwargs) -> ChannelClient` — 握手一次,身份用 `SessionIdentity`,`_target` 为该 `LocalRpcClient`(五通道同连接复用的最终形状留 W82-A;本层是「认证会话+单通道复用」最小正确版,API 留缝:`connect_authenticated` 已返回可复用 rpc,后续 mux 在其上加)
  - `ChannelClient.__init__` 身份参数默认 `None → 每实例 `secrets.token_bytes(16)` 派生(占位常量退场)

- [x] **Step 1: 红测试** — 进程内测试服务器(asyncio Unix socket,镜像服务端):发挑战→收证明→验签(对期望公钥)→同连接循环 N 轮 echo envelope(request_id 回盖)。用例:
  1. 握手后同连接 3 轮顺序 exchange(单连接多轮);
  2. 并发第二轮 → `IpcError("BACKPRESSURE")`;
  3. 服务器中途断连 → 该次 exchange 报错,再调 → `CONNECTION_UNUSABLE`(毒化);
  4. 错误密钥签名 → 服务器拒收关连接 → 后续 exchange 失败(失败闭合);
  5. 垃圾挑战帧(非 protobuf)→ `COMPATIBILITY` 类型化错误;
  6. `ChannelClient.connect_authenticated` 全链路:create→enqueue→receive 同一会话(对多轮测试服务器)。
- [x] **Step 2: 跑红** → FAIL(模块/工厂不存在)
- [x] **Step 3: 实现**(握手期间帧读写带 read/write 超时,失败关连接;成功即构造 `LocalRpcClient(reader, writer, config)`)
- [x] **Step 4: 跑绿** → 全 PASS;`python3 -m pytest sdk/python/tests -v` 全套绿
- [x] **Step 5: 提交**

```bash
git add sdk/python/nlos_sdk/principal_session.py sdk/python/nlos_sdk/local_rpc.py sdk/python/nlos_sdk/channel.py sdk/python/tests/test_principal_session.py
git commit  # feat(nlos-sdk): 认证持久会话客户端单连接顺序多轮 (W82-D)
```

### Task 6: Rust daemon e2e 验收脚本(真实进程,认证握手+≥1 轮认证 exchange)

**Files:**
- Create: `tests/conformance/ipc/authenticated_session.py`

**Interfaces:**
- Consumes: Task 5 `ChannelClient.connect_authenticated`、`_signing.Ed25519Signer`;`system-control-daemon`(cargo build 产物,不 cargo run)
- Produces: 退出码 0 + 机械回执(stdout 逐 case PASS)

- [x] **Step 1: 写脚本** — 流程:① `secrets.token_bytes(32)` 种子写 0600 临时文件(daemon `--identity-key-file` 契约:64 hex 一行,`parse_key_seed`);② 后台 `cargo build -p nlos-system-control --features daemon --bin system-control-daemon`(>2min 首跑,后台);③ 直启二进制 `--root <tmp> --auth-socket <sock> --plain-socket <sock> --identity-key-file <seedfile>`;④ 解析 `READY … principal=<hex>` 得 principal_id;⑤ 三例各自新会话(服务端单发:每连接 1 握手+1 exchange):create_channel、enqueue、receive+inspect_queue 断言;⑥ 附 plain 端点对照一例;⑦ 全程打印命令与断言结果。
- [x] **Step 2: 跑通并留回执**(本条即「对现有 Rust 单发服务端握手成功并完成至少一轮认证 exchange」的正确性证据;Rust `verify_strict` 通过 = digest/binding/签名逐字节兼容)
- [x] **Step 3: 提交**(与 Task 7 合并或独立 `test(conformance): …`)

### Task 7: channel_control.py 稳定化 + 重复跑回执

**Files:**
- Modify: `tests/conformance/ipc/channel_control.py`

- [x] **Step 1: 现状连跑 3 次记录失败/不稳定点**(疑似:cargo run 冷编译吃掉 READY 120s、socket 残留、stderr 管道无人读)
- [x] **Step 2: 最小修复** — 先 `cargo build` 产二进制再直启(消 cargo run 开销)、READY 超时保持但冷编译移出、清理逻辑保持;不做 CI 接线(.github 非写集,回执标注)
- [x] **Step 3: 连跑 3 次全绿留回执;提交**

### Task 8: 对抗式审查(ultracode workflow)

- [ ] 用 Workflow 并行审查 diff 四维:①契约镜像保真(逐行对 Rust 源)②打包正确性(sdist 场景、site-packages 冲突、pip/uv 差异)③会话语义 vs lib.rs 毒化/背压 ④测试质量与供应链(PyNaCl 来源、--no-deps 影响)。确认项修复后复跑门禁。

### Task 9: 门禁 + push + PR

- [ ] `python3 -m pytest sdk/python/tests -v` 全绿;`verify_wheel.py` ALL PASS;`authenticated_session.py`、`channel_control.py` ×3 回执齐
- [ ] 提交五查(`git log origin/main..HEAD`/`git diff --staged`/敏感扫描)
- [ ] `git push -u origin claude/w82d-sdk-prework-d01a37` → 按 §6 开 PR(标题沿用提交风格;正文:问题/根因/修复/验证数据表/未运行项:Windows 实机、CI 接线归 INT、TS SDK 待裁定、Ed25519 选型待签认;不自行合并)

### Task 10: 回执报告主入口 + 共享笔记

- [ ] 共享笔记目录留 hand-over(两件回执摘要 + 未运行项 + 冲突点如有)
- [ ] `SendMessage` 报主入口:commit/PR 链接、验收证据要点、选型待签认、CI 接线归 INT

## Self-Review

- **覆盖度**:件一(Task 1-2+8②)、件二(Task 3-6+8①③)、附带件(Task 7)、流程(Task 9-10)齐;「9 通道全链路」由 verify_wheel 的 fresh-venv import 全 9 模块 + Task 6 真服务端 exchange 双证。
- **占位符**:无 TBD;所有代码步骤含实码或精确改动。
- **类型一致**:`Ed25519Signer.sign->bytes`、`connect_authenticated->LocalRpcClient`、`SessionIdentity.derive` 各任务引用同名同形。
