"""Bounded asyncio client for the transport-neutral NLOS local RPC PoC."""

from __future__ import annotations

import asyncio
import math
import sys
from dataclasses import dataclass
from typing import Any, Callable, Coroutine

from google.protobuf.message import DecodeError
from nlos.sabi.v1.envelope_pb2 import ExchangeRequest, ExchangeResponse

MAXIMUM_SCHEMA_FRAME_BYTES = 1024 * 1024
SUPPORTED_SCHEMA = "nlos.sabi.Envelope"
SUPPORTED_MAJOR = 1


class IpcError(Exception):
    """Typed local IPC failure."""

    def __init__(self, code: str, message: str) -> None:
        super().__init__(message)
        self.code = code


@dataclass(frozen=True, slots=True)
class TransportConfig:
    """Bounded local transport settings."""

    maximum_frame_bytes: int = MAXIMUM_SCHEMA_FRAME_BYTES
    connect_timeout: float = 5.0
    read_timeout: float = 5.0
    write_timeout: float = 5.0

    def __post_init__(self) -> None:
        if (
            isinstance(self.maximum_frame_bytes, bool)
            or not isinstance(self.maximum_frame_bytes, int)
            or not 0 < self.maximum_frame_bytes <= MAXIMUM_SCHEMA_FRAME_BYTES
        ):
            raise IpcError(
                "INVALID_CONFIG",
                "maximum_frame_bytes must be within 1..=1048576",
            )
        timeouts = (
            self.connect_timeout,
            self.read_timeout,
            self.write_timeout,
        )
        if any(
            isinstance(value, bool)
            or not isinstance(value, (int, float))
            or not math.isfinite(value)
            or value <= 0
            for value in timeouts
        ):
            raise IpcError("INVALID_CONFIG", "timeouts must be finite and positive")


class LocalRpcClient:
    """Single-connection, single-in-flight local RPC client."""

    def __init__(
        self,
        reader: asyncio.StreamReader,
        writer: asyncio.StreamWriter,
        config: TransportConfig,
    ) -> None:
        self._reader = reader
        self._writer = writer
        self._config = config
        self._in_flight = False
        self._usable = True

    @classmethod
    async def connect(
        cls,
        endpoint: str,
        config: TransportConfig | None = None,
    ) -> LocalRpcClient:
        """Connect to a ServiceDirectory-resolved local endpoint."""

        if not endpoint:
            raise IpcError("INVALID_CONFIG", "IPC endpoint must not be empty")
        resolved = config or TransportConfig()
        try:
            reader, writer = await asyncio.wait_for(
                _open_platform_stream(endpoint),
                timeout=resolved.connect_timeout,
            )
        except TimeoutError as error:
            raise IpcError("TIMEOUT", "IPC connect timed out") from error
        except OSError as error:
            raise IpcError("CONNECT", "IPC connect failed") from error
        return cls(reader, writer, resolved)

    async def exchange(self, request: ExchangeRequest) -> ExchangeResponse:
        """Execute one unary call with immediate concurrent backpressure."""

        request_envelope = _require_compatible_envelope(request)
        wire = request.SerializeToString()
        _ensure_frame_bound(len(wire), self._config.maximum_frame_bytes)
        if self._in_flight:
            raise IpcError("BACKPRESSURE", "IPC client already has an in-flight call")
        if not self._usable:
            raise IpcError("CONNECTION_UNUSABLE", "IPC connection is unusable; reconnect")

        self._in_flight = True
        operation = "WRITE"
        try:
            prefix = len(wire).to_bytes(4, byteorder="big")
            self._writer.write(prefix + wire)
            await asyncio.wait_for(
                self._writer.drain(),
                timeout=self._config.write_timeout,
            )
            operation = "READ"
            response_wire = await asyncio.wait_for(
                _read_frame(self._reader, self._config.maximum_frame_bytes),
                timeout=self._config.read_timeout,
            )
            response = ExchangeResponse()
            response.ParseFromString(response_wire)
            response_envelope = _require_compatible_envelope(response)
            if response_envelope.request_id != request_envelope.request_id:
                raise IpcError(
                    "REQUEST_ID_MISMATCH",
                    "IPC response request_id does not match the request",
                )
            return response
        except asyncio.IncompleteReadError as error:
            await self._poison()
            raise IpcError("READ", "IPC peer disconnected mid-frame") from error
        except TimeoutError as error:
            await self._poison()
            raise IpcError("TIMEOUT", "IPC exchange timed out") from error
        except DecodeError as error:
            await self._poison()
            raise IpcError("COMPATIBILITY", "malformed protobuf response") from error
        except IpcError:
            await self._poison()
            raise
        except Exception as error:
            await self._poison()
            raise IpcError(operation, f"IPC {operation.lower()} failed") from error
        finally:
            self._in_flight = False

    async def close(self) -> None:
        """Close the stream and make the client permanently unusable."""

        await self._poison()

    async def _poison(self) -> None:
        self._usable = False
        self._writer.close()
        try:
            await self._writer.wait_closed()
        except OSError:
            pass


async def _open_platform_stream(
    endpoint: str,
) -> tuple[asyncio.StreamReader, asyncio.StreamWriter]:
    if sys.platform != "win32":
        return await asyncio.open_unix_connection(endpoint)

    loop = asyncio.get_running_loop()
    create_pipe_connection = getattr(loop, "create_pipe_connection", None)
    if create_pipe_connection is None:
        raise IpcError(
            "UNSUPPORTED_PLATFORM",
            "the active Windows event loop cannot connect to named pipes",
        )
    reader = asyncio.StreamReader(limit=MAXIMUM_SCHEMA_FRAME_BYTES + 4)
    protocol = asyncio.StreamReaderProtocol(reader)
    factory: Callable[[], asyncio.StreamReaderProtocol] = lambda: protocol
    connect: Callable[
        [Callable[[], asyncio.StreamReaderProtocol], str],
        Coroutine[Any, Any, tuple[asyncio.BaseTransport, asyncio.BaseProtocol]],
    ] = create_pipe_connection
    transport, _ = await connect(factory, endpoint)
    writer = asyncio.StreamWriter(transport, protocol, reader, loop)
    return reader, writer


def _ensure_frame_bound(actual: int, maximum: int) -> None:
    if actual > maximum:
        raise IpcError(
            "FRAME_TOO_LARGE",
            f"IPC frame has {actual} bytes; maximum is {maximum}",
        )


async def _read_frame(reader: asyncio.StreamReader, maximum: int) -> bytes:
    prefix = await reader.readexactly(4)
    declared = int.from_bytes(prefix, byteorder="big")
    _ensure_frame_bound(declared, maximum)
    return await reader.readexactly(declared)


def _require_compatible_envelope(message: ExchangeRequest | ExchangeResponse) -> Any:
    if not message.HasField("envelope") or not message.envelope.HasField("schema"):
        raise IpcError("COMPATIBILITY", "schema identity is missing")
    envelope = message.envelope
    if envelope.schema.name != SUPPORTED_SCHEMA:
        raise IpcError(
            "COMPATIBILITY",
            f"schema {envelope.schema.name!r} is not registered",
        )
    if envelope.schema.major != SUPPORTED_MAJOR:
        raise IpcError(
            "COMPATIBILITY",
            f"unsupported schema major {envelope.schema.major}",
        )
    if envelope.schema.critical_extension_ids:
        raise IpcError(
            "COMPATIBILITY",
            "unsupported critical extension "
            f"{envelope.schema.critical_extension_ids[0]}",
        )
    if len(envelope.request_id) != 16:
        raise IpcError("COMPATIBILITY", "request_id must contain 16 bytes")
    if not envelope.service or not envelope.method:
        raise IpcError("COMPATIBILITY", "service and method must not be empty")
    return envelope
