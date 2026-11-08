# `nlos-sdk` — NLOS Python SABI SDK

Python client SDK for NLOS SABI local RPC (`LocalRpcClient`), trusted-bootstrap `ServiceDirectoryClient` negotiation, and common request/response metadata validation (`validate_request_context`, `validate_response_context`).

## Installation

Install in editable mode from an NLOS repository checkout so external applications can import `nlos_sdk` (and generated `nlos.sabi.v1.*` protobuf bindings under `<repo_root>/gen/python`) without manual `sys.path.insert` hacks:

```bash
pip install -e sdk/python
```

Or with `uv`:

```bash
uv pip install -e sdk/python
# Or inside an external project's pyproject.toml / uv environment:
uv add --editable /path/to/llmos/sdk/python
```

When imported from an editable repository checkout, `nlos_sdk` automatically resolves `<repo_root>/gen/python` on `sys.path` if `nlos.sabi.v1` is not already installed.

## Wheel Distribution (L-012)

The wheel embeds the generated `nlos.sabi.v1.*` bindings straight from the pinned buf output (`buf.build/protocolbuffers/python:v33.4`) via a multi-root `[tool.setuptools.packages.find]`, so an installed wheel needs no source checkout:

```bash
python3 sdk/python/scripts/verify_wheel.py   # build → zero-drift parity vs gen/ → fresh-venv import chain
```

Notes:

- The `where` roots are resolved relative to `sdk/python/pyproject.toml`; wheels must be built from a repository checkout, and only the wheel format is supported (an sdist cannot carry the out-of-tree `gen/python` root).
- `protobuf` is pinned to `==6.33.4`, matching the generator pinned in `buf.gen.yaml` and `requirements-schema.txt`; the generated `_pb2.py` runtime check rejects older runtimes.

## Windows Event Loop Requirement (`ProactorEventLoop`)

On Windows (`sys.platform == "win32"`), NLOS local IPC uses Windows Named Pipes (`\\.\pipe\...`), which require `asyncio.ProactorEventLoop` (`create_pipe_connection`).

- Python 3.8+ on Windows defaults to `WindowsProactorEventLoopPolicy()`.
- If a host framework or test runner overrides the event loop policy to `WindowsSelectorEventLoopPolicy()` / `SelectorEventLoop`, `LocalRpcClient.connect()` will fail fast with `IpcError("UNSUPPORTED_PLATFORM", ...)` naming the active loop class.
- Restore the proactor policy before starting the loop:

```python
import asyncio
import sys

if sys.platform == "win32":
    asyncio.set_event_loop_policy(asyncio.WindowsProactorEventLoopPolicy())
```
