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
