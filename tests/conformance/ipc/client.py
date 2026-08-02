"""Cross-language Python client against the Rust local IPC server."""

from __future__ import annotations

import asyncio
import os
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / "sdk" / "python"))
sys.path.insert(0, str(ROOT / "gen" / "python"))

from nlos.sabi.v1.envelope_pb2 import ExchangeRequest  # noqa: E402
from nlos_sdk import IpcError, LocalRpcClient, TransportConfig  # noqa: E402


def endpoint(label: str) -> str:
    unique = f"{os.getpid()}-{time.time_ns()}"
    if sys.platform == "win32":
        return rf"\\.\pipe\nlos-ipc-{label}-{unique}"
    return str(Path(tempfile.gettempdir()) / f"nlos-ipc-{label}-{unique}.sock")


def request(request_id: int) -> ExchangeRequest:
    value = ExchangeRequest()
    value.envelope.schema.name = "nlos.sabi.Envelope"
    value.envelope.schema.major = 1
    value.envelope.schema.minor = 0
    value.envelope.request_id = bytes([request_id]) * 16
    value.envelope.service = "operation"
    value.envelope.method = "get"
    value.envelope.payload = b"\x01\x02\x03"
    return value


async def start_server(address: str, delay_ms: int) -> asyncio.subprocess.Process:
    process = await asyncio.create_subprocess_exec(
        "cargo",
        "run",
        "--quiet",
        "-p",
        "nlos-ipc",
        "--features",
        "conformance-server",
        "--bin",
        "nlos-ipc-echo",
        "--",
        address,
        str(delay_ms),
        cwd=ROOT,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )
    assert process.stdout is not None
    try:
        ready = await asyncio.wait_for(process.stdout.readline(), timeout=30)
    except BaseException:
        process.kill()
        await process.wait()
        raise
    if ready.strip() != b"READY":
        assert process.stderr is not None
        stderr = await process.stderr.read()
        raise AssertionError(f"Rust server failed before ready: {stderr!r}")
    return process


async def main() -> None:
    for invalid_timeout in (0, float("nan"), float("inf")):
        try:
            TransportConfig(read_timeout=invalid_timeout)
        except IpcError as error:
            assert error.code == "INVALID_CONFIG"
        else:
            raise AssertionError("invalid timeout was accepted")

    address = endpoint("py")
    server = await start_server(address, 100)
    try:
        client = await LocalRpcClient.connect(
            address,
            TransportConfig(
                connect_timeout=2,
                read_timeout=2,
                write_timeout=2,
            ),
        )
        incompatible = request(6)
        incompatible.envelope.schema.major = 2
        try:
            await client.exchange(incompatible)
        except IpcError as error:
            assert error.code == "COMPATIBILITY"
        else:
            raise AssertionError("unknown schema major was accepted")
        first = asyncio.create_task(client.exchange(request(7)))
        await asyncio.sleep(0)
        try:
            await client.exchange(request(8))
        except IpcError as error:
            assert error.code == "BACKPRESSURE"
        else:
            raise AssertionError("concurrent call did not receive backpressure")
        response = await first
        assert response.envelope.request_id == bytes([7]) * 16
        assert response.envelope.payload == b"\x01\x02\x03"
        await client.close()
        assert await server.wait() == 0
    except BaseException:
        server.kill()
        await server.wait()
        raise

    try:
        await LocalRpcClient.connect(
            endpoint("missing"),
            TransportConfig(connect_timeout=0.1),
        )
    except IpcError as error:
        assert error.code in {"CONNECT", "TIMEOUT"}
    else:
        raise AssertionError("missing endpoint unexpectedly connected")


if __name__ == "__main__":
    asyncio.run(main())
