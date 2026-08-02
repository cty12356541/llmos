"""Trusted-bootstrap ServiceDirectory negotiation for the candidate Python SDK."""

from __future__ import annotations

import secrets
import sys
from dataclasses import dataclass

from google.protobuf.message import DecodeError
from nlos.sabi.v1.envelope_pb2 import ExchangeRequest
from nlos.sabi.v1.service_directory_pb2 import (
    DIRECTORY_ERROR_CODE_UNSPECIFIED,
    DirectoryErrorCode,
    LOCAL_TRANSPORT_KIND_UNIX_SOCKET,
    LOCAL_TRANSPORT_KIND_WINDOWS_NAMED_PIPE,
    NegotiateServiceRequest,
    NegotiateServiceResponse,
    ServiceBinding,
)

from .local_rpc import IpcError, LocalRpcClient, TransportConfig

ENVELOPE_SCHEMA = "nlos.sabi.Envelope"
DIRECTORY_SCHEMA = "nlos.sabi.ServiceDirectory"
DIRECTORY_SERVICE = "service_directory"
MAX_DIRECTORY_PAYLOAD_BYTES = 64 * 1024


@dataclass(frozen=True, slots=True)
class ServiceRequirement:
    """A fail-closed version and feature requirement for one logical service."""

    service: str
    schema_name: str
    major: int
    minimum_minor: int = 0
    required_feature_ids: tuple[int, ...] = ()

    def __post_init__(self) -> None:
        if (
            not _valid_name(self.service)
            or not _valid_name(self.schema_name)
            or isinstance(self.major, bool)
            or not isinstance(self.major, int)
            or self.major <= 0
            or isinstance(self.minimum_minor, bool)
            or not isinstance(self.minimum_minor, int)
            or self.minimum_minor < 0
            or not _valid_feature_ids(self.required_feature_ids)
        ):
            raise IpcError("INVALID_CONFIG", "invalid service requirement")


@dataclass(frozen=True, slots=True)
class ConnectedService:
    """A negotiated binding and the client connected to its local endpoint."""

    binding: ServiceBinding
    client: LocalRpcClient


class DirectoryNegotiationError(Exception):
    """A typed compatibility failure returned by ServiceDirectory."""

    def __init__(self, code: int, service: str) -> None:
        super().__init__(f"ServiceDirectory negotiation failed with code {code}")
        self.code = code
        self.service = service


class ServiceDirectoryClient:
    """Resolver reached through a trusted Namespace/bootstrap endpoint."""

    def __init__(self, rpc: LocalRpcClient) -> None:
        self._rpc = rpc

    @classmethod
    async def connect(
        cls,
        trusted_bootstrap_endpoint: str,
        config: TransportConfig | None = None,
    ) -> ServiceDirectoryClient:
        """Connect only to an endpoint supplied by the trusted bootstrap path."""

        return cls(
            await LocalRpcClient.connect(trusted_bootstrap_endpoint, config)
        )

    @classmethod
    async def negotiate_and_connect(
        cls,
        trusted_bootstrap_endpoint: str,
        requirement: ServiceRequirement,
        config: TransportConfig | None = None,
    ) -> ConnectedService:
        """Negotiate one binding, close the directory, then connect the service."""

        directory = await cls.connect(trusted_bootstrap_endpoint, config)
        try:
            binding = await directory.negotiate(requirement)
        finally:
            await directory.close()
        endpoint = _require_compatible_binding(binding, requirement)
        client = await LocalRpcClient.connect(endpoint, config)
        return ConnectedService(binding=binding, client=client)

    async def negotiate(self, requirement: ServiceRequirement) -> ServiceBinding:
        """Return a compatible binding or a typed directory error."""

        transport = _platform_transport()
        directory_request = NegotiateServiceRequest()
        _set_identity(directory_request.schema, DIRECTORY_SCHEMA)
        directory_request.service = requirement.service
        directory_request.schema_name = requirement.schema_name
        directory_request.major = requirement.major
        directory_request.minimum_minor = requirement.minimum_minor
        directory_request.required_feature_ids.extend(
            requirement.required_feature_ids
        )
        directory_request.supported_transport_kinds.append(transport)
        payload = directory_request.SerializeToString(deterministic=True)
        if len(payload) > MAX_DIRECTORY_PAYLOAD_BYTES:
            raise IpcError(
                "FRAME_TOO_LARGE",
                "ServiceDirectory request exceeds 64 KiB",
            )

        exchange = ExchangeRequest()
        _set_identity(exchange.envelope.schema, ENVELOPE_SCHEMA)
        exchange.envelope.request_id = secrets.token_bytes(16)
        exchange.envelope.service = DIRECTORY_SERVICE
        exchange.envelope.method = "negotiate"
        exchange.envelope.payload = payload
        response = await self._rpc.exchange(exchange)
        response_payload = response.envelope.payload
        if not response_payload or len(response_payload) > MAX_DIRECTORY_PAYLOAD_BYTES:
            raise IpcError(
                "COMPATIBILITY",
                "ServiceDirectory response payload is missing or oversized",
            )
        try:
            negotiation = NegotiateServiceResponse.FromString(response_payload)
        except DecodeError as error:
            raise IpcError(
                "COMPATIBILITY",
                "malformed ServiceDirectory response",
            ) from error
        _require_directory_identity(negotiation)
        result = negotiation.WhichOneof("result")
        if result == "error":
            try:
                DirectoryErrorCode.Name(negotiation.error.code)
            except ValueError as error:
                raise IpcError(
                    "COMPATIBILITY",
                    "ServiceDirectory returned an unknown error code",
                ) from error
            if negotiation.error.code == DIRECTORY_ERROR_CODE_UNSPECIFIED:
                raise IpcError(
                    "COMPATIBILITY",
                    "ServiceDirectory returned an unspecified error code",
                )
            raise DirectoryNegotiationError(
                negotiation.error.code,
                negotiation.error.service,
            )
        if result != "binding":
            raise IpcError(
                "COMPATIBILITY",
                "ServiceDirectory response is missing a result",
            )
        _require_compatible_binding(negotiation.binding, requirement)
        binding = ServiceBinding()
        binding.CopyFrom(negotiation.binding)
        return binding

    async def close(self) -> None:
        """Close the bootstrap connection."""

        await self._rpc.close()


def _set_identity(identity: object, name: str) -> None:
    identity.name = name
    identity.major = 1
    identity.minor = 0


def _require_directory_identity(response: NegotiateServiceResponse) -> None:
    if (
        not response.HasField("schema")
        or response.schema.name != DIRECTORY_SCHEMA
        or response.schema.major != 1
        or response.schema.critical_extension_ids
    ):
        raise IpcError(
            "COMPATIBILITY",
            "incompatible ServiceDirectory response identity",
        )


def _require_compatible_binding(
    binding: ServiceBinding,
    requirement: ServiceRequirement,
) -> str:
    if (
        not binding.HasField("candidate")
        or not binding.HasField("endpoint")
        or len(binding.candidate.binding_id) != 16
        or binding.candidate.generation <= 0
        or binding.candidate.service != requirement.service
        or not binding.candidate.HasField("version")
        or binding.candidate.version.schema_name != requirement.schema_name
        or binding.candidate.version.major != requirement.major
        or binding.candidate.version.minor < requirement.minimum_minor
        or not _valid_feature_sequence(binding.candidate.feature_ids)
        or not all(
            feature in binding.candidate.feature_ids
            for feature in requirement.required_feature_ids
        )
        or len(binding.candidate.transport_kinds) != 1
        or binding.candidate.transport_kinds[0] != binding.endpoint.kind
        or binding.endpoint.kind != _platform_transport()
        or not binding.endpoint.address
        or len(binding.endpoint.address) > 4096
        or "\0" in binding.endpoint.address
    ):
        raise IpcError(
            "COMPATIBILITY",
            "ServiceDirectory returned an incompatible binding",
        )
    return binding.endpoint.address


def _platform_transport() -> int:
    if sys.platform == "win32":
        return LOCAL_TRANSPORT_KIND_WINDOWS_NAMED_PIPE
    return LOCAL_TRANSPORT_KIND_UNIX_SOCKET


def _valid_name(value: object) -> bool:
    return (
        isinstance(value, str)
        and 0 < len(value) <= 255
        and "\0" not in value
    )


def _valid_feature_ids(values: object) -> bool:
    if not isinstance(values, tuple) or len(values) > 128:
        return False
    return _valid_feature_sequence(values)


def _valid_feature_sequence(values: object) -> bool:
    return all(
        isinstance(value, int)
        and not isinstance(value, bool)
        and value > 0
        and (index == 0 or values[index - 1] < value)
        for index, value in enumerate(values)
    )
