"""Bounded asyncio client for the transport-neutral NLOS local RPC PoC."""

from __future__ import annotations

import asyncio
import math
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Coroutine

from google.protobuf.message import DecodeError


def _ensure_gen_python_path() -> None:
    """Ensure `<repo_root>/gen/python` is importable in editable repo checkouts."""

    try:
        import nlos.sabi.v1.envelope_pb2  # noqa: F401
    except ModuleNotFoundError:
        resolved = Path(__file__).resolve()
        if len(resolved.parents) > 3:
            candidate = resolved.parents[3] / "gen" / "python"
            if candidate.is_dir():
                candidate_str = str(candidate)
                if candidate_str not in sys.path:
                    sys.path.insert(0, candidate_str)


_ensure_gen_python_path()

from nlos.sabi.v1.envelope_pb2 import ExchangeRequest, ExchangeResponse  # noqa: E402

MAXIMUM_SCHEMA_FRAME_BYTES = 1024 * 1024
SUPPORTED_SCHEMA = "nlos.sabi.Envelope"
SUPPORTED_MAJOR = 1


class IpcError(Exception):
    """Typed local IPC failure (never embeds sensitive wire/key bytes)."""

    def __init__(
        self,
        code: str,
        message: str,
        *,
        retryable: bool = False,
        service: str = "",
        method: str = "",
        idempotency_key: bytes = b"",
        request_digest_sha256: bytes = b"",
        side_effecting: bool = False,
    ) -> None:
        super().__init__(message)
        self.code = code
        self.retryable = retryable
        self.service = service
        self.method = method
        self.idempotency_key = bytes(idempotency_key)
        self.request_digest_sha256 = bytes(request_digest_sha256)
        self.side_effecting = side_effecting

    def with_call_context(
        self,
        *,
        service: str,
        method: str,
        idempotency_key: bytes,
        request_digest_sha256: bytes,
        side_effecting: bool,
    ) -> "IpcError":
        """Attach sanitized call metadata for retry validation."""

        return IpcError(
            self.code,
            str(self),
            retryable=self.retryable or self.code in {"TIMEOUT", "READ", "WRITE", "CONNECTION_UNUSABLE"},
            service=service,
            method=method,
            idempotency_key=idempotency_key,
            request_digest_sha256=request_digest_sha256,
            side_effecting=side_effecting,
        )


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

    @property
    def is_usable(self) -> bool:
        """Whether this connection can accept another sequential RPC exchange."""

        return self._usable

    @property
    def is_in_flight(self) -> bool:
        """Whether an RPC exchange is currently executing on this connection."""

        return self._in_flight

    async def __aenter__(self) -> LocalRpcClient:
        return self

    async def __aexit__(self, *_exc: object) -> None:
        await self.close()

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
        except TimeoutError:
            raise IpcError("TIMEOUT", "IPC connect timed out", retryable=True) from None
        except OSError as error:
            raise IpcError("CONNECT", "IPC connect failed") from error
        return cls(reader, writer, resolved)

    async def exchange(self, request: ExchangeRequest) -> ExchangeResponse:
        """Execute one unary call with immediate concurrent backpressure."""

        if self._in_flight:
            raise IpcError("BACKPRESSURE", "IPC client already has an in-flight call")
        if not self._usable:
            raise IpcError("CONNECTION_UNUSABLE", "IPC connection is unusable; reconnect")
        request_envelope = _require_compatible_envelope(request)
        wire = request.SerializeToString()
        _ensure_frame_bound(len(wire), self._config.maximum_frame_bytes)

        self._in_flight = True
        # 镜像 lib.rs:445-448:进首个 await 前预毒化——外部取消/丢弃本协程
        # 后,半写或滞留缓冲的连接不可被后续调用复用;仅全链成功恢复。
        self._usable = False
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
            self._usable = True
            return response
        except asyncio.IncompleteReadError:
            await self._poison()
            raise IpcError(
                "READ",
                "IPC peer disconnected mid-frame",
                retryable=True,
            ) from None
        except TimeoutError:
            await self._poison()
            raise IpcError(
                "TIMEOUT",
                "IPC exchange timed out",
                retryable=True,
            ) from None
        except DecodeError:
            await self._poison()
            raise IpcError("COMPATIBILITY", "malformed protobuf response") from None
        except IpcError:
            await self._poison()
            raise
        except Exception as error:
            await self._poison()
            raise IpcError(
                operation,
                f"IPC {operation.lower()} failed",
                retryable=True,
            ) from error
        except BaseException:
            # 外部取消(CancelledError 是 BaseException):保持预毒化,同步
            # 关写端,不 await(避免在已取消上下文里再挂起)。
            self._writer.close()
            raise
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
        except (OSError, EOFError, RuntimeError):
            pass


def _require_windows_pipe_connection_factory(
    loop: object,
) -> Callable[
    [Callable[[], asyncio.StreamReaderProtocol], str],
    Coroutine[Any, Any, tuple[asyncio.BaseTransport, asyncio.BaseProtocol]],
]:
    create_pipe_connection = getattr(loop, "create_pipe_connection", None)
    if create_pipe_connection is None:
        loop_type = type(loop).__name__
        raise IpcError(
            "UNSUPPORTED_PLATFORM",
            "Windows named pipe IPC requires ProactorEventLoop, but the active "
            f"event loop is {loop_type} (missing create_pipe_connection); "
            "configure asyncio.set_event_loop_policy("
            "asyncio.WindowsProactorEventLoopPolicy()) instead of SelectorEventLoop",
        )
    return create_pipe_connection


async def _open_platform_stream(
    endpoint: str,
) -> tuple[asyncio.StreamReader, asyncio.StreamWriter]:
    if sys.platform != "win32":
        return await asyncio.open_unix_connection(endpoint)

    loop = asyncio.get_running_loop()
    connect = _require_windows_pipe_connection_factory(loop)
    reader = asyncio.StreamReader(limit=MAXIMUM_SCHEMA_FRAME_BYTES + 4)
    protocol = asyncio.StreamReaderProtocol(reader)
    factory: Callable[[], asyncio.StreamReaderProtocol] = lambda: protocol
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


async def _write_frame(
    writer: asyncio.StreamWriter,
    wire: bytes,
    config: TransportConfig,
) -> None:
    """写一帧 4 字节大端长度前缀 + 载荷(与 FramedIo::send 同构)。"""

    _ensure_frame_bound(len(wire), config.maximum_frame_bytes)
    writer.write(len(wire).to_bytes(4, byteorder="big") + wire)
    await asyncio.wait_for(writer.drain(), timeout=config.write_timeout)


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
    if len(envelope.schema.non_critical_extension_ids) > 32:
        raise IpcError(
            "COMPATIBILITY",
            "non_critical_extension_ids exceeds maximum 32",
        )
    if len(envelope.request_id) != 16:
        raise IpcError("COMPATIBILITY", "request_id must contain 16 bytes")
    if not envelope.service or not envelope.method:
        raise IpcError("COMPATIBILITY", "service and method must not be empty")
    return envelope
