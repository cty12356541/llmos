"""Bootstrap through Rust ServiceDirectory, then call its negotiated service."""

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
from nlos_sdk import (  # noqa: E402
    ServiceDirectoryClient,
    ServiceRequirement,
    TransportConfig,
)


def endpoint(label: str) -> str:
    unique = f"{os.getpid():x}-{time.time_ns() & 0xFFFFFFF:x}-{label[0]}"
    if sys.platform == "win32":
        return rf"\\.\pipe\nlos-directory-{unique}"
    return str(Path(tempfile.gettempdir()) / f"nlos-directory-{unique}.sock")


async def start_server(
    directory_endpoint: str,
    business_endpoint: str,
) -> asyncio.subprocess.Process:
    process = await asyncio.create_subprocess_exec(
        "cargo",
        "run",
        "--quiet",
        "-p",
        "nlos-ipc",
        "--features",
        "conformance-server",
        "--bin",
        "nlos-directory-chain",
        "--",
        directory_endpoint,
        business_endpoint,
        cwd=ROOT,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )
    assert process.stdout is not None
    ready = await asyncio.wait_for(process.stdout.readline(), timeout=30)
    if ready.strip() != b"READY":
        assert process.stderr is not None
        stderr = await process.stderr.read()
        raise AssertionError(f"directory chain failed before ready: {stderr!r}")
    return process


async def main() -> None:
    directory_endpoint = endpoint("bootstrap")
    business_endpoint = endpoint("business")
    server = await start_server(directory_endpoint, business_endpoint)
    try:
        connected = await ServiceDirectoryClient.negotiate_and_connect(
            directory_endpoint,
            ServiceRequirement(
                service="operation",
                schema_name="nlos.sabi.Envelope",
                major=1,
            ),
            TransportConfig(
                connect_timeout=2,
                read_timeout=2,
                write_timeout=2,
            ),
        )
        assert connected.binding.endpoint.address == business_endpoint
        assert connected.binding.candidate.generation == 7

        request = ExchangeRequest()
        request.envelope.schema.name = "nlos.sabi.Envelope"
        request.envelope.schema.major = 1
        request.envelope.request_id = bytes([9]) * 16
        request.envelope.service = "operation"
        request.envelope.method = "get"
        request.envelope.payload = b"\x04\x05\x06"
        response = await connected.client.exchange(request)
        assert response.envelope.request_id == bytes([9]) * 16
        assert response.envelope.payload == b"\x04\x05\x06"
        await connected.client.close()
        assert await server.wait() == 0
    except BaseException:
        server.kill()
        await server.wait()
        raise


if __name__ == "__main__":
    asyncio.run(main())
