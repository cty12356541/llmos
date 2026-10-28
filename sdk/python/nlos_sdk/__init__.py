"""Candidate Python SDK surface for NLOS SABI."""

from .local_rpc import IpcError, LocalRpcClient, TransportConfig
from .common import (
    CommonSemanticsError,
    MethodSemantics,
    validate_request_context,
    validate_response_context,
)
from .service_directory import (
    ConnectedService,
    DirectoryNegotiationError,
    ServiceDirectoryClient,
    ServiceRequirement,
)
from .channel import (
    CHANNEL_CONTROL_SCHEMA,
    CHANNEL_CONTROL_SERVICE,
    AckResult,
    ChannelClient,
    ChannelControlError,
    ChannelRecord,
    CompactResult,
    QueueConsumptionRecord,
    QueueEntryRecord,
    QueueState,
)

__version__ = "0.1.0"

__all__ = [
    "__version__",
    "AckResult",
    "CHANNEL_CONTROL_SCHEMA",
    "CHANNEL_CONTROL_SERVICE",
    "ChannelClient",
    "ChannelControlError",
    "ChannelRecord",
    "CommonSemanticsError",
    "CompactResult",
    "ConnectedService",
    "DirectoryNegotiationError",
    "IpcError",
    "LocalRpcClient",
    "MethodSemantics",
    "QueueConsumptionRecord",
    "QueueEntryRecord",
    "QueueState",
    "ServiceDirectoryClient",
    "ServiceRequirement",
    "TransportConfig",
    "validate_request_context",
    "validate_response_context",
]
