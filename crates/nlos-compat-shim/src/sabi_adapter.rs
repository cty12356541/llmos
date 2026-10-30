//! Out-of-core unprivileged `SabiEpochAdapterCell` for `N-1 / N-2 <-> N`
//! bidirectional wire translation across all nine frozen SABI channels.
//!
//! Authority:
//! - `06-架构设计总纲-v0.5.md` section 9 (`COMPAT-VER-001`, `COMPAT-VER-002`, `COMPAT-DEPRECATE-001`)
//! - `06-架构设计总纲-v0.5.md` section 6 / section 9 (`KABI-IDEM-001`, `KABI-UNCERTAIN-001`, `SABI-AUTH-001`, `SABI-RECEIPT-001`)
//! - `ADR-0027` section 2.1 / section 2.2 (out-of-core protocol translation cell)

use std::collections::VecDeque;
use std::error::Error;
use std::fmt;

use nlos_schema::{
    CommonSemanticsError, CompatibilityError, DeprecationEntry, DeprecationManifest,
    DeprecationManifestError, MAX_CHANNEL_CONTROL_PAYLOAD_BYTES, MAX_DRIVER_PROVIDER_PAYLOAD_BYTES,
    MAX_ENVELOPE_BYTES, MAX_OPERATION_CONTROL_PAYLOAD_BYTES, MAX_PRINCIPAL_HANDSHAKE_PAYLOAD_BYTES,
    MAX_SERVICE_DIRECTORY_PAYLOAD_BYTES, MAX_SYSTEM_CONTROL_PAYLOAD_BYTES,
    MAX_TAKEOVER_CONTROL_PAYLOAD_BYTES, MAX_WAIT_CONTROL_PAYLOAD_BYTES,
    MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS, MethodSemantics, SABI_CHANNEL_CONTROL_SCHEMA,
    SABI_DRIVER_PROVIDER_SCHEMA, SABI_ENVELOPE_SCHEMA, SABI_OPERATION_CONTROL_SCHEMA,
    SABI_PRINCIPAL_HANDSHAKE_SCHEMA, SABI_SERVICE_DIRECTORY_SCHEMA, SABI_SYSTEM_CONTROL_SCHEMA,
    SABI_TAKEOVER_CONTROL_SCHEMA, SABI_WAIT_CONTROL_SCHEMA, SymbolAuditVerdict,
    VersionCompatibilityPolicy, VersionRoute, canonical_deprecation_manifest,
    compatibility_error_to_sabi_failure, sabi, validate_sabi_request_context,
    validate_sabi_response_context,
};
use prost::Message;

use crate::wire::{
    RewrittenFrame, replace_envelope_payload_bytes, rewrite_exchange_wrapper_schema_major,
    rewrite_top_level_schema_major,
};

/// Bounded FIFO capacity for `DeprecationWarningReceipt` telemetry inside one adapter cell.
pub const MAX_DEPRECATION_WARNINGS: usize = 256;

/// Canonical identifier for each of the nine frozen SABI channels (`9/9` registry).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum SabiChannelId {
    /// `nlos.sabi.Envelope`
    Envelope,
    /// `nlos.sabi.ServiceDirectory`
    ServiceDirectory,
    /// `nlos.sabi.OperationControl`
    OperationControl,
    /// `nlos.sabi.SystemControl`
    SystemControl,
    /// `nlos.sabi.TakeoverControl`
    TakeoverControl,
    /// `nlos.sabi.WaitControl`
    WaitControl,
    /// `nlos.sabi.PrincipalHandshake`
    PrincipalHandshake,
    /// `nlos.sabi.DriverProvider`
    DriverProvider,
    /// `nlos.sabi.ChannelControl`
    ChannelControl,
}

impl SabiChannelId {
    /// Complete ordered list of all nine frozen SABI channels.
    pub const ALL: [Self; 9] = [
        Self::Envelope,
        Self::ServiceDirectory,
        Self::OperationControl,
        Self::SystemControl,
        Self::TakeoverControl,
        Self::WaitControl,
        Self::PrincipalHandshake,
        Self::DriverProvider,
        Self::ChannelControl,
    ];

    /// Canonical schema registry name for this channel.
    #[must_use]
    pub const fn schema_name(self) -> &'static str {
        match self {
            Self::Envelope => SABI_ENVELOPE_SCHEMA,
            Self::ServiceDirectory => SABI_SERVICE_DIRECTORY_SCHEMA,
            Self::OperationControl => SABI_OPERATION_CONTROL_SCHEMA,
            Self::SystemControl => SABI_SYSTEM_CONTROL_SCHEMA,
            Self::TakeoverControl => SABI_TAKEOVER_CONTROL_SCHEMA,
            Self::WaitControl => SABI_WAIT_CONTROL_SCHEMA,
            Self::PrincipalHandshake => SABI_PRINCIPAL_HANDSHAKE_SCHEMA,
            Self::DriverProvider => SABI_DRIVER_PROVIDER_SCHEMA,
            Self::ChannelControl => SABI_CHANNEL_CONTROL_SCHEMA,
        }
    }

    /// Maximum bounded wire size in bytes permitted for frames on this channel.
    #[must_use]
    pub const fn max_payload_bytes(self) -> usize {
        match self {
            Self::Envelope => MAX_ENVELOPE_BYTES,
            Self::ServiceDirectory => MAX_SERVICE_DIRECTORY_PAYLOAD_BYTES,
            Self::OperationControl => MAX_OPERATION_CONTROL_PAYLOAD_BYTES,
            Self::SystemControl => MAX_SYSTEM_CONTROL_PAYLOAD_BYTES,
            Self::TakeoverControl => MAX_TAKEOVER_CONTROL_PAYLOAD_BYTES,
            Self::WaitControl => MAX_WAIT_CONTROL_PAYLOAD_BYTES,
            Self::PrincipalHandshake => MAX_PRINCIPAL_HANDSHAKE_PAYLOAD_BYTES,
            Self::DriverProvider => MAX_DRIVER_PROVIDER_PAYLOAD_BYTES,
            Self::ChannelControl => MAX_CHANNEL_CONTROL_PAYLOAD_BYTES,
        }
    }
}

/// Top-level message kinds supported by [`SabiEpochAdapterCell`] across all nine frozen channels.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SabiFrameKind {
    // 1. Envelope
    Envelope,
    ExchangeRequest,
    ExchangeResponse,
    // 2. ServiceDirectory
    ResolveServiceRequest,
    ResolveServiceResponse,
    NegotiateServiceRequest,
    NegotiateServiceResponse,
    // 3. OperationControl
    QueryOperationRequest,
    CancelOperationRequest,
    OperationStatus,
    // 4. SystemControl
    GetSystemControlRequest,
    SubmitControlCommandRequest,
    ControlCommandResult,
    DurableOperationSnapshot,
    // 5. TakeoverControl
    SubmitBarrierObservationRequest,
    BarrierObservationRecord,
    // 6. WaitControl
    RegisterWaitRequest,
    RegisterWaitResult,
    NotifyCommitsRequest,
    WakeReport,
    CancelWaitRequest,
    CancelWaitResult,
    ListWaitsRequest,
    ListWaitsResult,
    InspectWaitRequest,
    InspectWaitResult,
    // 7. PrincipalHandshake
    PrincipalHandshakeChallenge,
    PrincipalHandshakeAttestation,
    // 8. DriverProvider
    DriverProviderRequest,
    DriverProviderResponse,
    // 9. ChannelControl
    CreateChannelRequestWire,
    CreateChannelResultWire,
    RotateChannelRequestWire,
    RotateChannelResultWire,
    EnqueueChannelRequestWire,
    EnqueueChannelResultWire,
    ReceiveChannelRequestWire,
    ReceiveChannelResultWire,
    AckChannelRequestWire,
    AckChannelResultWire,
    CompactChannelRequestWire,
    CompactChannelResultWire,
    InspectChannelRequestWire,
    InspectChannelResultWire,
    InspectQueueRequestWire,
    InspectQueueResultWire,
    RegisterQueueConsumptionRequestWire,
    RegisterQueueConsumptionResultWire,
}

impl SabiFrameKind {
    /// Owning frozen SABI channel of this message kind.
    #[must_use]
    pub const fn channel(self) -> SabiChannelId {
        match self {
            Self::Envelope | Self::ExchangeRequest | Self::ExchangeResponse => {
                SabiChannelId::Envelope
            }
            Self::ResolveServiceRequest
            | Self::ResolveServiceResponse
            | Self::NegotiateServiceRequest
            | Self::NegotiateServiceResponse => SabiChannelId::ServiceDirectory,
            Self::QueryOperationRequest | Self::CancelOperationRequest | Self::OperationStatus => {
                SabiChannelId::OperationControl
            }
            Self::GetSystemControlRequest
            | Self::SubmitControlCommandRequest
            | Self::ControlCommandResult
            | Self::DurableOperationSnapshot => SabiChannelId::SystemControl,
            Self::SubmitBarrierObservationRequest | Self::BarrierObservationRecord => {
                SabiChannelId::TakeoverControl
            }
            Self::RegisterWaitRequest
            | Self::RegisterWaitResult
            | Self::NotifyCommitsRequest
            | Self::WakeReport
            | Self::CancelWaitRequest
            | Self::CancelWaitResult
            | Self::ListWaitsRequest
            | Self::ListWaitsResult
            | Self::InspectWaitRequest
            | Self::InspectWaitResult => SabiChannelId::WaitControl,
            Self::PrincipalHandshakeChallenge | Self::PrincipalHandshakeAttestation => {
                SabiChannelId::PrincipalHandshake
            }
            Self::DriverProviderRequest | Self::DriverProviderResponse => {
                SabiChannelId::DriverProvider
            }
            Self::CreateChannelRequestWire
            | Self::CreateChannelResultWire
            | Self::RotateChannelRequestWire
            | Self::RotateChannelResultWire
            | Self::EnqueueChannelRequestWire
            | Self::EnqueueChannelResultWire
            | Self::ReceiveChannelRequestWire
            | Self::ReceiveChannelResultWire
            | Self::AckChannelRequestWire
            | Self::AckChannelResultWire
            | Self::CompactChannelRequestWire
            | Self::CompactChannelResultWire
            | Self::InspectChannelRequestWire
            | Self::InspectChannelResultWire
            | Self::InspectQueueRequestWire
            | Self::InspectQueueResultWire
            | Self::RegisterQueueConsumptionRequestWire
            | Self::RegisterQueueConsumptionResultWire => SabiChannelId::ChannelControl,
        }
    }

    /// Resolves a fully qualified Protobuf message name (as recorded in `manifest.json`)
    /// into its [`SabiFrameKind`].
    #[must_use]
    pub fn from_proto_message_name(name: &str) -> Option<Self> {
        let short = name.strip_prefix("nlos.sabi.v1.").unwrap_or(name);
        match short {
            "Envelope" => Some(Self::Envelope),
            "ExchangeRequest" => Some(Self::ExchangeRequest),
            "ExchangeResponse" => Some(Self::ExchangeResponse),
            "ResolveServiceRequest" => Some(Self::ResolveServiceRequest),
            "ResolveServiceResponse" => Some(Self::ResolveServiceResponse),
            "NegotiateServiceRequest" => Some(Self::NegotiateServiceRequest),
            "NegotiateServiceResponse" => Some(Self::NegotiateServiceResponse),
            "QueryOperationRequest" => Some(Self::QueryOperationRequest),
            "CancelOperationRequest" => Some(Self::CancelOperationRequest),
            "OperationStatus" => Some(Self::OperationStatus),
            "GetSystemControlRequest" => Some(Self::GetSystemControlRequest),
            "SubmitControlCommandRequest" => Some(Self::SubmitControlCommandRequest),
            "ControlCommandResult" => Some(Self::ControlCommandResult),
            "DurableOperationSnapshot" => Some(Self::DurableOperationSnapshot),
            "SubmitBarrierObservationRequest" => Some(Self::SubmitBarrierObservationRequest),
            "BarrierObservationRecord" => Some(Self::BarrierObservationRecord),
            "RegisterWaitRequest" => Some(Self::RegisterWaitRequest),
            "RegisterWaitResult" => Some(Self::RegisterWaitResult),
            "NotifyCommitsRequest" => Some(Self::NotifyCommitsRequest),
            "WakeReport" => Some(Self::WakeReport),
            "CancelWaitRequest" => Some(Self::CancelWaitRequest),
            "CancelWaitResult" => Some(Self::CancelWaitResult),
            "ListWaitsRequest" => Some(Self::ListWaitsRequest),
            "ListWaitsResult" => Some(Self::ListWaitsResult),
            "InspectWaitRequest" => Some(Self::InspectWaitRequest),
            "InspectWaitResult" => Some(Self::InspectWaitResult),
            "PrincipalHandshakeChallenge" => Some(Self::PrincipalHandshakeChallenge),
            "PrincipalHandshakeAttestation" => Some(Self::PrincipalHandshakeAttestation),
            "DriverProviderRequest" => Some(Self::DriverProviderRequest),
            "DriverProviderResponse" => Some(Self::DriverProviderResponse),
            "CreateChannelRequestWire" => Some(Self::CreateChannelRequestWire),
            "CreateChannelResultWire" => Some(Self::CreateChannelResultWire),
            "RotateChannelRequestWire" => Some(Self::RotateChannelRequestWire),
            "RotateChannelResultWire" => Some(Self::RotateChannelResultWire),
            "EnqueueChannelRequestWire" => Some(Self::EnqueueChannelRequestWire),
            "EnqueueChannelResultWire" => Some(Self::EnqueueChannelResultWire),
            "ReceiveChannelRequestWire" => Some(Self::ReceiveChannelRequestWire),
            "ReceiveChannelResultWire" => Some(Self::ReceiveChannelResultWire),
            "AckChannelRequestWire" => Some(Self::AckChannelRequestWire),
            "AckChannelResultWire" => Some(Self::AckChannelResultWire),
            "CompactChannelRequestWire" => Some(Self::CompactChannelRequestWire),
            "CompactChannelResultWire" => Some(Self::CompactChannelResultWire),
            "InspectChannelRequestWire" => Some(Self::InspectChannelRequestWire),
            "InspectChannelResultWire" => Some(Self::InspectChannelResultWire),
            "InspectQueueRequestWire" => Some(Self::InspectQueueRequestWire),
            "InspectQueueResultWire" => Some(Self::InspectQueueResultWire),
            "RegisterQueueConsumptionRequestWire" => {
                Some(Self::RegisterQueueConsumptionRequestWire)
            }
            "RegisterQueueConsumptionResultWire" => Some(Self::RegisterQueueConsumptionResultWire),
            _ => None,
        }
    }

    /// Short message symbol name used in `deprecation-manifest.yaml` `parent_type` / `symbol` audits.
    #[must_use]
    pub const fn message_symbol(self) -> &'static str {
        match self {
            Self::Envelope => "Envelope",
            Self::ExchangeRequest => "ExchangeRequest",
            Self::ExchangeResponse => "ExchangeResponse",
            Self::ResolveServiceRequest => "ResolveServiceRequest",
            Self::ResolveServiceResponse => "ResolveServiceResponse",
            Self::NegotiateServiceRequest => "NegotiateServiceRequest",
            Self::NegotiateServiceResponse => "NegotiateServiceResponse",
            Self::QueryOperationRequest => "QueryOperationRequest",
            Self::CancelOperationRequest => "CancelOperationRequest",
            Self::OperationStatus => "OperationStatus",
            Self::GetSystemControlRequest => "GetSystemControlRequest",
            Self::SubmitControlCommandRequest => "SubmitControlCommandRequest",
            Self::ControlCommandResult => "ControlCommandResult",
            Self::DurableOperationSnapshot => "DurableOperationSnapshot",
            Self::SubmitBarrierObservationRequest => "SubmitBarrierObservationRequest",
            Self::BarrierObservationRecord => "BarrierObservationRecord",
            Self::RegisterWaitRequest => "RegisterWaitRequest",
            Self::RegisterWaitResult => "RegisterWaitResult",
            Self::NotifyCommitsRequest => "NotifyCommitsRequest",
            Self::WakeReport => "WakeReport",
            Self::CancelWaitRequest => "CancelWaitRequest",
            Self::CancelWaitResult => "CancelWaitResult",
            Self::ListWaitsRequest => "ListWaitsRequest",
            Self::ListWaitsResult => "ListWaitsResult",
            Self::InspectWaitRequest => "InspectWaitRequest",
            Self::InspectWaitResult => "InspectWaitResult",
            Self::PrincipalHandshakeChallenge => "PrincipalHandshakeChallenge",
            Self::PrincipalHandshakeAttestation => "PrincipalHandshakeAttestation",
            Self::DriverProviderRequest => "DriverProviderRequest",
            Self::DriverProviderResponse => "DriverProviderResponse",
            Self::CreateChannelRequestWire => "CreateChannelRequestWire",
            Self::CreateChannelResultWire => "CreateChannelResultWire",
            Self::RotateChannelRequestWire => "RotateChannelRequestWire",
            Self::RotateChannelResultWire => "RotateChannelResultWire",
            Self::EnqueueChannelRequestWire => "EnqueueChannelRequestWire",
            Self::EnqueueChannelResultWire => "EnqueueChannelResultWire",
            Self::ReceiveChannelRequestWire => "ReceiveChannelRequestWire",
            Self::ReceiveChannelResultWire => "ReceiveChannelResultWire",
            Self::AckChannelRequestWire => "AckChannelRequestWire",
            Self::AckChannelResultWire => "AckChannelResultWire",
            Self::CompactChannelRequestWire => "CompactChannelRequestWire",
            Self::CompactChannelResultWire => "CompactChannelResultWire",
            Self::InspectChannelRequestWire => "InspectChannelRequestWire",
            Self::InspectChannelResultWire => "InspectChannelResultWire",
            Self::InspectQueueRequestWire => "InspectQueueRequestWire",
            Self::InspectQueueResultWire => "InspectQueueResultWire",
            Self::RegisterQueueConsumptionRequestWire => "RegisterQueueConsumptionRequestWire",
            Self::RegisterQueueConsumptionResultWire => "RegisterQueueConsumptionResultWire",
        }
    }

    fn known_field_numbers(self) -> &'static [u32] {
        match self {
            Self::Envelope | Self::ExchangeRequest | Self::ExchangeResponse => {
                &[1, 2, 3, 4, 5, 6, 15]
            }
            Self::ResolveServiceRequest
            | Self::QueryOperationRequest
            | Self::WakeReport
            | Self::ListWaitsRequest
            | Self::ListWaitsResult
            | Self::InspectWaitRequest
            | Self::InspectWaitResult
            | Self::PrincipalHandshakeChallenge
            | Self::ReceiveChannelResultWire
            | Self::InspectChannelRequestWire
            | Self::InspectChannelResultWire
            | Self::InspectQueueRequestWire
            | Self::InspectQueueResultWire => &[1, 2],
            Self::ResolveServiceResponse
            | Self::NegotiateServiceResponse
            | Self::CancelOperationRequest
            | Self::RegisterWaitResult
            | Self::CancelWaitResult
            | Self::CreateChannelResultWire
            | Self::RotateChannelResultWire
            | Self::EnqueueChannelResultWire
            | Self::ReceiveChannelRequestWire
            | Self::RegisterQueueConsumptionResultWire => &[1, 2, 3],
            Self::SubmitBarrierObservationRequest
            | Self::CancelWaitRequest
            | Self::CompactChannelRequestWire
            | Self::CompactChannelResultWire => &[1, 2, 3, 4],
            Self::OperationStatus
            | Self::NotifyCommitsRequest
            | Self::PrincipalHandshakeAttestation
            | Self::CreateChannelRequestWire
            | Self::AckChannelRequestWire
            | Self::AckChannelResultWire => &[1, 2, 3, 4, 5],
            Self::GetSystemControlRequest
            | Self::RegisterWaitRequest
            | Self::DriverProviderRequest
            | Self::DriverProviderResponse
            | Self::RotateChannelRequestWire => &[1, 2, 3, 4, 5, 6],
            Self::NegotiateServiceRequest | Self::RegisterQueueConsumptionRequestWire => {
                &[1, 2, 3, 4, 5, 6, 7]
            }
            Self::EnqueueChannelRequestWire => &[1, 2, 3, 4, 5, 6, 7, 8, 9],
            Self::BarrierObservationRecord => &[1, 2, 3, 4, 5, 6, 7, 8, 9, 10],
            Self::SubmitControlCommandRequest
            | Self::ControlCommandResult
            | Self::DurableOperationSnapshot => &[
                1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
                24, 25, 26, 27, 28, 29, 30,
            ],
        }
    }

    fn validate_normalized_v1_wire(self, wire: &[u8]) -> Result<(), CompatibilityError> {
        match self {
            Self::Envelope => nlos_schema::decode_sabi_envelope(wire).map(|_| ()),
            Self::ExchangeRequest => nlos_schema::decode_exchange_request(wire).map(|_| ()),
            Self::ExchangeResponse => nlos_schema::decode_exchange_response(wire).map(|_| ()),
            Self::ResolveServiceRequest => {
                nlos_schema::decode_resolve_service_request(wire).map(|_| ())
            }
            Self::ResolveServiceResponse => {
                nlos_schema::decode_resolve_service_response(wire).map(|_| ())
            }
            Self::NegotiateServiceRequest => {
                nlos_schema::decode_negotiate_service_request(wire).map(|_| ())
            }
            Self::NegotiateServiceResponse => {
                nlos_schema::decode_negotiate_service_response(wire).map(|_| ())
            }
            Self::QueryOperationRequest => {
                nlos_schema::decode_query_operation_request(wire).map(|_| ())
            }
            Self::CancelOperationRequest => {
                nlos_schema::decode_cancel_operation_request(wire).map(|_| ())
            }
            Self::OperationStatus => nlos_schema::decode_operation_status(wire).map(|_| ()),
            Self::GetSystemControlRequest => {
                nlos_schema::decode_get_system_control_request(wire).map(|_| ())
            }
            Self::SubmitControlCommandRequest => {
                nlos_schema::decode_submit_control_command_request(wire).map(|_| ())
            }
            Self::ControlCommandResult => {
                nlos_schema::decode_control_command_result(wire).map(|_| ())
            }
            Self::DurableOperationSnapshot => {
                nlos_schema::decode_durable_operation_snapshot(wire).map(|_| ())
            }
            Self::SubmitBarrierObservationRequest => {
                nlos_schema::decode_submit_barrier_observation_request(wire).map(|_| ())
            }
            Self::BarrierObservationRecord => {
                nlos_schema::decode_barrier_observation_record(wire).map(|_| ())
            }
            Self::RegisterWaitRequest => {
                nlos_schema::decode_register_wait_request(wire).map(|_| ())
            }
            Self::RegisterWaitResult => nlos_schema::decode_register_wait_result(wire).map(|_| ()),
            Self::NotifyCommitsRequest => {
                nlos_schema::decode_notify_commits_request(wire).map(|_| ())
            }
            Self::WakeReport => nlos_schema::decode_notify_commits_result(wire).map(|_| ()),
            Self::CancelWaitRequest => nlos_schema::decode_cancel_wait_request(wire).map(|_| ()),
            Self::CancelWaitResult => nlos_schema::decode_cancel_wait_result(wire).map(|_| ()),
            Self::ListWaitsRequest => nlos_schema::decode_list_waits_request(wire).map(|_| ()),
            Self::ListWaitsResult => nlos_schema::decode_list_waits_result(wire).map(|_| ()),
            Self::InspectWaitRequest => nlos_schema::decode_inspect_wait_request(wire).map(|_| ()),
            Self::InspectWaitResult => nlos_schema::decode_inspect_wait_result(wire).map(|_| ()),
            Self::PrincipalHandshakeChallenge => {
                nlos_schema::decode_principal_handshake_challenge(wire).map(|_| ())
            }
            Self::PrincipalHandshakeAttestation => {
                nlos_schema::decode_principal_handshake_attestation(wire).map(|_| ())
            }
            Self::DriverProviderRequest => {
                nlos_schema::decode_driver_provider_request(wire).map(|_| ())
            }
            Self::DriverProviderResponse => {
                nlos_schema::decode_driver_provider_response(wire).map(|_| ())
            }
            other => validate_channel_control_v1_wire(other, wire),
        }
    }
}

fn validate_channel_control_v1_wire(
    kind: SabiFrameKind,
    wire: &[u8],
) -> Result<(), CompatibilityError> {
    match kind {
        SabiFrameKind::CreateChannelRequestWire => {
            nlos_schema::decode_create_channel_request(wire).map(|_| ())
        }
        SabiFrameKind::CreateChannelResultWire => {
            nlos_schema::decode_create_channel_result(wire).map(|_| ())
        }
        SabiFrameKind::RotateChannelRequestWire => {
            nlos_schema::decode_rotate_channel_request(wire).map(|_| ())
        }
        SabiFrameKind::RotateChannelResultWire => {
            nlos_schema::decode_rotate_channel_result(wire).map(|_| ())
        }
        SabiFrameKind::EnqueueChannelRequestWire => {
            nlos_schema::decode_enqueue_channel_request(wire).map(|_| ())
        }
        SabiFrameKind::EnqueueChannelResultWire => {
            nlos_schema::decode_enqueue_channel_result(wire).map(|_| ())
        }
        SabiFrameKind::ReceiveChannelRequestWire => {
            nlos_schema::decode_receive_channel_request(wire).map(|_| ())
        }
        SabiFrameKind::ReceiveChannelResultWire => {
            nlos_schema::decode_receive_channel_result(wire).map(|_| ())
        }
        SabiFrameKind::AckChannelRequestWire => {
            nlos_schema::decode_ack_channel_request(wire).map(|_| ())
        }
        SabiFrameKind::AckChannelResultWire => {
            nlos_schema::decode_ack_channel_result(wire).map(|_| ())
        }
        SabiFrameKind::CompactChannelRequestWire => {
            nlos_schema::decode_compact_channel_request(wire).map(|_| ())
        }
        SabiFrameKind::CompactChannelResultWire => {
            nlos_schema::decode_compact_channel_result(wire).map(|_| ())
        }
        SabiFrameKind::InspectChannelRequestWire => {
            nlos_schema::decode_inspect_channel_request(wire).map(|_| ())
        }
        SabiFrameKind::InspectChannelResultWire => {
            nlos_schema::decode_inspect_channel_result(wire).map(|_| ())
        }
        SabiFrameKind::InspectQueueRequestWire => {
            nlos_schema::decode_inspect_queue_request(wire).map(|_| ())
        }
        SabiFrameKind::InspectQueueResultWire => {
            nlos_schema::decode_inspect_queue_result(wire).map(|_| ())
        }
        SabiFrameKind::RegisterQueueConsumptionRequestWire => {
            nlos_schema::decode_register_queue_consumption_request(wire).map(|_| ())
        }
        SabiFrameKind::RegisterQueueConsumptionResultWire => {
            nlos_schema::decode_register_queue_consumption_result(wire).map(|_| ())
        }
        _ => unreachable!("non-ChannelControl frame routed to ChannelControl validator"),
    }
}

/// Direction of an out-of-core compatibility translation hop.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompatTranslationDirection {
    /// Historical client (`N-1` / `N-2`) -> Host canonical (`N`).
    UpstreamClientToHost,
    /// Host canonical (`N`) -> Historical client (`N-1` / `N-2`).
    DownstreamHostToClient,
}

/// Bounded telemetry receipt emitted when an `N-1` or `N-2` client exercises a
/// `DEPRECATED` SABI element (`[COMPAT-DEPRECATE-001]`, `ADR-0027` section 3.3).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeprecationWarningReceipt {
    /// Registered canonical SABI channel name.
    pub channel: String,
    /// Deprecated symbol name (`service.method`, message, field, or feature).
    pub symbol: String,
    /// Optional Protobuf field or extension number.
    pub field_number: Option<u32>,
    /// Epoch in which deprecation started.
    pub deprecated_in_epoch: u32,
    /// Earliest host epoch at which the element may transition to `SUNSET`.
    pub earliest_sunset_epoch: u32,
    /// Recommended canonical replacement symbol.
    pub replacement_symbol: String,
    /// Optional `nlos-migrate` automated rewrite rule identifier.
    pub migration_rule_id: Option<String>,
    /// Caller's declared SABI `major` version.
    pub client_major: u32,
    /// Caller's declared SABI `minor` version.
    pub client_minor: u32,
}

/// Structured audit trace proving semantic invariant preservation across one
/// `SabiEpochAdapterCell` translation hop.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompatTranslationTrace {
    /// Translation direction (`UpstreamClientToHost` vs `DownstreamHostToClient`).
    pub direction: CompatTranslationDirection,
    /// Registered canonical SABI channel name.
    pub channel: &'static str,
    /// Validated Protobuf message symbol.
    pub message_symbol: &'static str,
    /// Input `SchemaIdentity.major` before translation.
    pub source_major: u32,
    /// Input `SchemaIdentity.minor` (preserved verbatim).
    pub source_minor: u32,
    /// Output `SchemaIdentity.major` after translation.
    pub target_major: u32,
    /// Preserved 128-bit `Envelope.request_id` when applicable.
    pub preserved_request_id: Option<[u8; 16]>,
    /// Preserved 128-bit `idempotency_key` (`KABI-IDEM-001`) when present.
    pub preserved_idempotency_key: Option<[u8; 16]>,
    /// Preserved monotonic `deadline_monotonic_ns` when non-zero.
    pub preserved_deadline_monotonic_ns: Option<u64>,
    /// Preserved `cancel_epoch` fence when present.
    pub preserved_cancel_epoch: Option<u64>,
    /// Preserved 32-byte `proposal_or_input_digest_sha256` (`SABI-AUTH-001`) when present.
    pub preserved_authorization_digest: Option<[u8; 32]>,
    /// Non-critical extension IDs retained verbatim (`[COMPAT-VER-002]`).
    pub retained_non_critical_extensions: Vec<u32>,
    /// Unknown Protobuf field numbers preserved byte-for-byte on the wire.
    pub preserved_unknown_field_numbers: Vec<u32>,
    /// Exact byte count of unknown Protobuf TLV records preserved on the wire.
    pub preserved_unknown_field_bytes: usize,
    /// `true` when the frame carried an `UNCERTAIN` / `EFFECT_UNKNOWN` outcome preserved verbatim.
    pub preserved_uncertain_outcome: bool,
    /// Deprecation warning receipts emitted during this hop without blocking traffic.
    pub emitted_deprecation_warnings: Vec<DeprecationWarningReceipt>,
}

/// Translated SABI wire frame together with its `CompatTranslationTrace`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TranslatedSabiFrame {
    /// Rewritten wire bytes ready for the destination epoch.
    pub wire_bytes: Vec<u8>,
    /// Structured invariant-preservation trace for observability and testing.
    pub trace: CompatTranslationTrace,
}

/// Fail-closed errors returned by [`SabiEpochAdapterCell`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SabiAdapterError {
    /// Adapter configuration violated `1 <= host_epoch - client_epoch <= max_backward_epochs`.
    InvalidAdapterWindow {
        client_epoch: u32,
        host_epoch: u32,
        max_backward_epochs: u32,
    },
    /// Frame `SchemaIdentity.name` disagreed with the expected [`SabiChannelId`].
    ChannelMismatch {
        expected: &'static str,
        actual: String,
    },
    /// Upstream frame `SchemaIdentity.major` did not match the mounted package's `client_epoch`.
    PinnedClientEpochMismatch {
        expected_client_epoch: u32,
        actual_major: u32,
    },
    /// Downstream host frame `SchemaIdentity.major` did not match `host_epoch`.
    HostEpochMismatch {
        expected_host_epoch: u32,
        actual_major: u32,
    },
    /// Wire schema or version-matrix adjudication failed closed.
    Schema(CompatibilityError),
    /// Common SABI request/response context validation failed closed.
    CommonSemantics(CommonSemanticsError),
    /// Frame referenced a `SUNSET` or permanently `RESERVED` contract element.
    SunsetOrReservedContract {
        channel: String,
        target: String,
        reason: &'static str,
    },
    /// Semantic invariant (`idempotency_key`, `deadline`, `UNCERTAIN`, or unknown fields) drifted.
    SemanticInvariantViolation(&'static str),
    /// Embedded or supplied `DeprecationManifest` failed validation.
    InvalidDeprecationManifest(DeprecationManifestError),
}

impl SabiAdapterError {
    /// Deterministic mapping to wire-safe [`sabi::v1::SabiFailure`].
    #[must_use]
    pub fn to_sabi_failure(&self) -> sabi::v1::SabiFailure {
        match self {
            Self::InvalidAdapterWindow { .. }
            | Self::ChannelMismatch { .. }
            | Self::PinnedClientEpochMismatch { .. }
            | Self::HostEpochMismatch { .. }
            | Self::SunsetOrReservedContract { .. }
            | Self::InvalidDeprecationManifest(_) => sabi::v1::SabiFailure {
                code: sabi::v1::SabiErrorCode::AbiVersion.into(),
                retry: sabi::v1::RetryDirective::DoNotRetry.into(),
                safe_message: "SABI compatibility shim rejected incompatible or sunset frame"
                    .to_owned(),
            },
            Self::Schema(err) => compatibility_error_to_sabi_failure(err),
            Self::CommonSemantics(CommonSemanticsError::DeadlineExpired) => sabi::v1::SabiFailure {
                code: sabi::v1::SabiErrorCode::Deadline.into(),
                retry: sabi::v1::RetryDirective::DoNotRetry.into(),
                safe_message: "SABI request deadline expired at compatibility boundary".to_owned(),
            },
            Self::CommonSemantics(_) | Self::SemanticInvariantViolation(_) => {
                sabi::v1::SabiFailure {
                    code: sabi::v1::SabiErrorCode::InvalidArgument.into(),
                    retry: sabi::v1::RetryDirective::DoNotRetry.into(),
                    safe_message:
                        "SABI frame violated common request/response or uncertain invariants"
                            .to_owned(),
                }
            }
        }
    }
}

impl fmt::Display for SabiAdapterError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidAdapterWindow {
                client_epoch,
                host_epoch,
                max_backward_epochs,
            } => write!(
                formatter,
                "invalid SABI adapter window: client_epoch={client_epoch}, host_epoch={host_epoch}, max_backward_epochs={max_backward_epochs}"
            ),
            Self::ChannelMismatch { expected, actual } => write!(
                formatter,
                "SABI channel mismatch: expected {expected:?}, got {actual:?}"
            ),
            Self::PinnedClientEpochMismatch {
                expected_client_epoch,
                actual_major,
            } => write!(
                formatter,
                "client frame major {actual_major} did not match pinned client_epoch {expected_client_epoch}"
            ),
            Self::HostEpochMismatch {
                expected_host_epoch,
                actual_major,
            } => write!(
                formatter,
                "host frame major {actual_major} did not match host_epoch {expected_host_epoch}"
            ),
            Self::Schema(err) => write!(formatter, "SABI schema compatibility error: {err}"),
            Self::CommonSemantics(err) => write!(formatter, "SABI common semantics error: {err}"),
            Self::SunsetOrReservedContract {
                channel,
                target,
                reason,
            } => write!(
                formatter,
                "SABI element {target:?} in {channel:?} is blocked: {reason}"
            ),
            Self::SemanticInvariantViolation(detail) => {
                write!(formatter, "SABI adapter invariant violation: {detail}")
            }
            Self::InvalidDeprecationManifest(err) => {
                write!(formatter, "deprecation manifest error: {err}")
            }
        }
    }
}

impl Error for SabiAdapterError {}

/// Out-of-core unprivileged protocol translation cell mounted per historical package.
///
/// Holds zero cryptographic keys, zero raw credentials, and zero authority write
/// handles (`ADR-0027` section 2.2).
#[derive(Clone, Debug)]
pub struct SabiEpochAdapterCell {
    client_epoch: u32,
    host_epoch: u32,
    max_backward_epochs: u32,
    known_critical_extensions: Vec<u32>,
    deprecation_manifest: DeprecationManifest,
    warning_ring: VecDeque<DeprecationWarningReceipt>,
}

impl SabiEpochAdapterCell {
    /// Creates an adapter cell for `(client_epoch, host_epoch)` using the embedded
    /// [`canonical_deprecation_manifest`].
    ///
    /// # Errors
    ///
    /// Fails closed if `client_epoch == 0`, `host_epoch <= client_epoch`, or
    /// `host_epoch - client_epoch > MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS`.
    pub fn new(
        client_epoch: u32,
        host_epoch: u32,
        known_critical_extensions: Vec<u32>,
    ) -> Result<Self, SabiAdapterError> {
        let manifest = canonical_deprecation_manifest()
            .map_err(SabiAdapterError::InvalidDeprecationManifest)?;
        Self::with_manifest(
            client_epoch,
            host_epoch,
            MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS,
            known_critical_extensions,
            manifest,
        )
    }

    /// Creates an adapter cell with explicit window and [`DeprecationManifest`].
    ///
    /// # Errors
    ///
    /// Fails closed if `max_backward_epochs < MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS`,
    /// `client_epoch == 0`, `client_epoch >= host_epoch`, or
    /// `host_epoch - client_epoch > max_backward_epochs`.
    pub fn with_manifest(
        client_epoch: u32,
        host_epoch: u32,
        max_backward_epochs: u32,
        known_critical_extensions: Vec<u32>,
        deprecation_manifest: DeprecationManifest,
    ) -> Result<Self, SabiAdapterError> {
        if client_epoch == 0
            || max_backward_epochs < MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS
            || host_epoch <= client_epoch
            || host_epoch - client_epoch > max_backward_epochs
        {
            return Err(SabiAdapterError::InvalidAdapterWindow {
                client_epoch,
                host_epoch,
                max_backward_epochs,
            });
        }
        Ok(Self {
            client_epoch,
            host_epoch,
            max_backward_epochs,
            known_critical_extensions,
            deprecation_manifest,
            warning_ring: VecDeque::new(),
        })
    }

    /// Pinned historical client major epoch (`N-1` or `N-2`).
    #[must_use]
    pub const fn client_epoch(&self) -> u32 {
        self.client_epoch
    }

    /// Canonical host major epoch (`N`).
    #[must_use]
    pub const fn host_epoch(&self) -> u32 {
        self.host_epoch
    }

    /// Epoch distance (`host_epoch - client_epoch`, guaranteed in `1..=max_backward_epochs`).
    #[must_use]
    pub const fn epoch_distance(&self) -> u32 {
        self.host_epoch - self.client_epoch
    }

    /// Read-only view of bounded `DeprecationWarningReceipt` telemetry accumulated so far.
    #[must_use]
    pub fn deprecation_warnings(&self) -> &VecDeque<DeprecationWarningReceipt> {
        &self.warning_ring
    }

    /// Drains all buffered `DeprecationWarningReceipt` entries in FIFO order.
    pub fn drain_deprecation_warnings(&mut self) -> Vec<DeprecationWarningReceipt> {
        self.warning_ring.drain(..).collect()
    }

    /// Translates an upstream `Envelope` wire frame (`client_epoch -> host_epoch`).
    ///
    /// # Errors
    ///
    /// Fails closed on oversized frames, invalid schema/version, sunset/reserved
    /// symbols or fields, or malformed common context metadata.
    pub fn translate_upstream_envelope(
        &mut self,
        wire: &[u8],
    ) -> Result<TranslatedSabiFrame, SabiAdapterError> {
        self.translate_upstream_frame(SabiFrameKind::Envelope, wire)
    }

    /// Translates a downstream `Envelope` wire frame (`host_epoch -> client_epoch`),
    /// strictly preserving `E_UNCERTAIN` / `EFFECT_UNKNOWN` and reconciliation references.
    ///
    /// # Errors
    ///
    /// Fails closed on oversized frames, host epoch mismatch, malformed protobuf, or
    /// missing operation/receipt evidence on uncertain/partial outcomes.
    pub fn translate_downstream_envelope(
        &mut self,
        wire: &[u8],
    ) -> Result<TranslatedSabiFrame, SabiAdapterError> {
        self.translate_downstream_frame(SabiFrameKind::Envelope, wire)
    }

    /// Translates an upstream `Envelope` AND its nested typed SABI channel payload
    /// (`payload_kind`) atomically (`client_epoch -> host_epoch`), preserving unknown
    /// trailing fields on both the outer `Envelope` and inner channel payload.
    ///
    /// # Errors
    ///
    /// Fails closed if either the outer `Envelope` or inner channel payload fails validation.
    pub fn translate_upstream_envelope_with_payload(
        &mut self,
        envelope_wire: &[u8],
        payload_kind: SabiFrameKind,
    ) -> Result<TranslatedSabiFrame, SabiAdapterError> {
        let mut outer = self.translate_upstream_envelope(envelope_wire)?;
        let decoded_env = sabi::v1::Envelope::decode(outer.wire_bytes.as_slice()).map_err(|e| {
            SabiAdapterError::Schema(CompatibilityError::MalformedProtobuf(e.to_string()))
        })?;
        let inner = self.translate_upstream_frame(payload_kind, &decoded_env.payload)?;
        outer.wire_bytes = replace_envelope_payload_bytes(&outer.wire_bytes, &inner.wire_bytes)
            .map_err(SabiAdapterError::Schema)?;
        if outer.wire_bytes.len() > MAX_ENVELOPE_BYTES {
            return Err(SabiAdapterError::Schema(
                CompatibilityError::FrameTooLarge {
                    actual: outer.wire_bytes.len(),
                    maximum: MAX_ENVELOPE_BYTES,
                },
            ));
        }
        outer
            .trace
            .emitted_deprecation_warnings
            .extend(inner.trace.emitted_deprecation_warnings);
        Ok(outer)
    }

    /// Translates a downstream `Envelope` AND its nested typed SABI channel payload
    /// (`payload_kind`) atomically (`host_epoch -> client_epoch`).
    ///
    /// # Errors
    ///
    /// Fails closed if either the outer `Envelope` or inner channel payload fails validation.
    pub fn translate_downstream_envelope_with_payload(
        &mut self,
        envelope_wire: &[u8],
        payload_kind: SabiFrameKind,
    ) -> Result<TranslatedSabiFrame, SabiAdapterError> {
        let mut outer = self.translate_downstream_envelope(envelope_wire)?;
        let decoded_env = sabi::v1::Envelope::decode(outer.wire_bytes.as_slice()).map_err(|e| {
            SabiAdapterError::Schema(CompatibilityError::MalformedProtobuf(e.to_string()))
        })?;
        let inner = self.translate_downstream_frame(payload_kind, &decoded_env.payload)?;
        outer.wire_bytes = replace_envelope_payload_bytes(&outer.wire_bytes, &inner.wire_bytes)
            .map_err(SabiAdapterError::Schema)?;
        outer.trace.preserved_uncertain_outcome |= inner.trace.preserved_uncertain_outcome;
        Ok(outer)
    }

    /// Translates any top-level SABI channel message upstream (`client_epoch -> host_epoch`)
    /// across all nine frozen SABI channels.
    ///
    /// # Errors
    ///
    /// Fails closed on size overflow, malformed protobuf, channel/epoch mismatch,
    /// unsupported `critical_extension_ids`, or `SUNSET`/`RESERVED` symbols/fields.
    pub fn translate_upstream_frame(
        &mut self,
        kind: SabiFrameKind,
        wire: &[u8],
    ) -> Result<TranslatedSabiFrame, SabiAdapterError> {
        enforce_channel_bound(kind.channel(), wire.len())?;
        let v1_normalized = rewrite_for_kind(kind, wire, 1)?;
        let expected_channel = kind.channel().schema_name();
        if v1_normalized.original_identity.name != expected_channel {
            return Err(SabiAdapterError::ChannelMismatch {
                expected: expected_channel,
                actual: v1_normalized.original_identity.name,
            });
        }

        let policy = VersionCompatibilityPolicy::with_backward_window(
            self.host_epoch,
            self.max_backward_epochs,
            &self.known_critical_extensions,
        );
        let negotiation = policy
            .adjudicate_schema_identity(&v1_normalized.original_identity)
            .map_err(SabiAdapterError::Schema)?;
        if v1_normalized.original_identity.major != self.client_epoch
            || !matches!(negotiation.route, VersionRoute::OutOfCoreCompatShim { .. })
        {
            return Err(SabiAdapterError::PinnedClientEpochMismatch {
                expected_client_epoch: self.client_epoch,
                actual_major: v1_normalized.original_identity.major,
            });
        }

        let warnings =
            self.audit_upstream_deprecations(kind, &v1_normalized, &v1_normalized.rewritten_wire)?;
        kind.validate_normalized_v1_wire(&v1_normalized.rewritten_wire)
            .map_err(SabiAdapterError::Schema)?;

        let host_rewritten = rewrite_for_kind(kind, wire, self.host_epoch)?;
        enforce_channel_bound(kind.channel(), host_rewritten.rewritten_wire.len())?;

        let trace = build_and_verify_trace(
            CompatTranslationDirection::UpstreamClientToHost,
            kind,
            &v1_normalized,
            &host_rewritten.rewritten_wire,
            self.host_epoch,
            warnings,
        )?;
        self.record_warnings(&trace.emitted_deprecation_warnings);

        Ok(TranslatedSabiFrame {
            wire_bytes: host_rewritten.rewritten_wire,
            trace,
        })
    }

    /// Translates any top-level SABI channel message downstream (`host_epoch -> client_epoch`),
    /// preserving `E_UNCERTAIN` / `EFFECT_UNKNOWN`, operation/receipt reconciliation handles,
    /// `non_critical_extension_ids`, and unknown Protobuf fields.
    ///
    /// # Errors
    ///
    /// Fails closed on size overflow, malformed protobuf, channel/host-epoch mismatch,
    /// or stripped uncertain/partial reconciliation metadata.
    pub fn translate_downstream_frame(
        &mut self,
        kind: SabiFrameKind,
        wire: &[u8],
    ) -> Result<TranslatedSabiFrame, SabiAdapterError> {
        enforce_channel_bound(kind.channel(), wire.len())?;
        let v1_normalized = rewrite_for_kind(kind, wire, 1)?;
        let expected_channel = kind.channel().schema_name();
        if v1_normalized.original_identity.name != expected_channel {
            return Err(SabiAdapterError::ChannelMismatch {
                expected: expected_channel,
                actual: v1_normalized.original_identity.name,
            });
        }
        if v1_normalized.original_identity.major != self.host_epoch {
            return Err(SabiAdapterError::HostEpochMismatch {
                expected_host_epoch: self.host_epoch,
                actual_major: v1_normalized.original_identity.major,
            });
        }

        let policy = VersionCompatibilityPolicy::with_backward_window(
            self.host_epoch,
            self.max_backward_epochs,
            &self.known_critical_extensions,
        );
        policy
            .adjudicate_schema_identity(&v1_normalized.original_identity)
            .map_err(SabiAdapterError::Schema)?;

        kind.validate_normalized_v1_wire(&v1_normalized.rewritten_wire)
            .map_err(SabiAdapterError::Schema)?;

        let client_rewritten = rewrite_for_kind(kind, wire, self.client_epoch)?;
        enforce_channel_bound(kind.channel(), client_rewritten.rewritten_wire.len())?;

        let trace = build_and_verify_trace(
            CompatTranslationDirection::DownstreamHostToClient,
            kind,
            &v1_normalized,
            &client_rewritten.rewritten_wire,
            self.client_epoch,
            Vec::new(),
        )?;

        Ok(TranslatedSabiFrame {
            wire_bytes: client_rewritten.rewritten_wire,
            trace,
        })
    }

    fn audit_upstream_deprecations(
        &self,
        kind: SabiFrameKind,
        frame: &RewrittenFrame,
        normalized_wire: &[u8],
    ) -> Result<Vec<DeprecationWarningReceipt>, SabiAdapterError> {
        let channel = kind.channel().schema_name();
        let client_major = frame.original_identity.major;
        let client_minor = frame.original_identity.minor;
        let mut warnings = Vec::new();

        check_verdict(
            channel,
            kind.message_symbol(),
            self.deprecation_manifest
                .audit_symbol(channel, kind.message_symbol()),
            client_major,
            client_minor,
            &mut warnings,
        )?;

        for &field_no in &frame.top_level_field_numbers {
            let target = format!("{}.{field_no}", kind.message_symbol());
            check_verdict(
                channel,
                &target,
                self.deprecation_manifest.audit_field_number(
                    channel,
                    Some(kind.message_symbol()),
                    field_no,
                ),
                client_major,
                client_minor,
                &mut warnings,
            )?;
        }

        for &ext_id in &frame.original_identity.non_critical_extension_ids {
            let target = format!("SchemaIdentity.non_critical_extension.{ext_id}");
            check_verdict(
                channel,
                &target,
                self.deprecation_manifest.audit_field_number(
                    channel,
                    Some("SchemaIdentity"),
                    ext_id,
                ),
                client_major,
                client_minor,
                &mut warnings,
            )?;
        }

        self.audit_semantic_symbols(
            kind,
            normalized_wire,
            client_major,
            client_minor,
            &mut warnings,
        )?;
        Ok(warnings)
    }

    fn audit_semantic_symbols(
        &self,
        kind: SabiFrameKind,
        normalized_wire: &[u8],
        client_major: u32,
        client_minor: u32,
        warnings: &mut Vec<DeprecationWarningReceipt>,
    ) -> Result<(), SabiAdapterError> {
        let channel = kind.channel().schema_name();
        match kind {
            SabiFrameKind::Envelope
            | SabiFrameKind::ExchangeRequest
            | SabiFrameKind::ExchangeResponse => {
                let env = decode_envelope_from_kind(kind, normalized_wire)?;
                let qualified = format!("{}.{}", env.service, env.method);
                check_verdict(
                    channel,
                    &qualified,
                    self.deprecation_manifest.audit_symbol(channel, &qualified),
                    client_major,
                    client_minor,
                    warnings,
                )?;
                check_verdict(
                    channel,
                    &env.method,
                    self.deprecation_manifest.audit_symbol(channel, &env.method),
                    client_major,
                    client_minor,
                    warnings,
                )?;
            }
            SabiFrameKind::NegotiateServiceRequest => {
                let req =
                    sabi::v1::NegotiateServiceRequest::decode(normalized_wire).map_err(|err| {
                        SabiAdapterError::Schema(CompatibilityError::MalformedProtobuf(
                            err.to_string(),
                        ))
                    })?;
                for &feature_id in &req.required_feature_ids {
                    let target = format!("feature.{feature_id}");
                    check_verdict(
                        &req.schema_name,
                        &target,
                        self.deprecation_manifest.audit_field_number(
                            &req.schema_name,
                            None,
                            feature_id,
                        ),
                        client_major,
                        client_minor,
                        warnings,
                    )?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn record_warnings(&mut self, receipts: &[DeprecationWarningReceipt]) {
        for receipt in receipts {
            if self.warning_ring.len() == MAX_DEPRECATION_WARNINGS {
                self.warning_ring.pop_front();
            }
            self.warning_ring.push_back(receipt.clone());
        }
    }
}

fn check_verdict(
    channel: &str,
    target: &str,
    verdict: SymbolAuditVerdict<'_>,
    client_major: u32,
    client_minor: u32,
    warnings: &mut Vec<DeprecationWarningReceipt>,
) -> Result<(), SabiAdapterError> {
    match verdict {
        SymbolAuditVerdict::Active(_) => Ok(()),
        SymbolAuditVerdict::Deprecated(entry) => {
            let receipt = entry_to_warning(entry, client_major, client_minor);
            if !warnings.contains(&receipt) {
                warnings.push(receipt);
            }
            Ok(())
        }
        SymbolAuditVerdict::SunsetOrReserved { entry, reason } => {
            let symbol = entry.map_or_else(|| target.to_owned(), |item| item.symbol.clone());
            Err(SabiAdapterError::SunsetOrReservedContract {
                channel: channel.to_owned(),
                target: symbol,
                reason,
            })
        }
    }
}

fn entry_to_warning(
    entry: &DeprecationEntry,
    client_major: u32,
    client_minor: u32,
) -> DeprecationWarningReceipt {
    DeprecationWarningReceipt {
        channel: entry.channel.clone(),
        symbol: entry.symbol.clone(),
        field_number: entry.field_number,
        deprecated_in_epoch: entry.deprecated_in_epoch.unwrap_or(1),
        earliest_sunset_epoch: entry.earliest_sunset_epoch.unwrap_or(3),
        replacement_symbol: entry.replacement.clone().unwrap_or_default(),
        migration_rule_id: entry.migration_rule_id.clone(),
        client_major,
        client_minor,
    }
}

fn enforce_channel_bound(channel: SabiChannelId, actual: usize) -> Result<(), SabiAdapterError> {
    let maximum = channel.max_payload_bytes();
    if actual > maximum {
        return Err(SabiAdapterError::Schema(
            CompatibilityError::FrameTooLarge { actual, maximum },
        ));
    }
    Ok(())
}

fn rewrite_for_kind(
    kind: SabiFrameKind,
    wire: &[u8],
    new_major: u32,
) -> Result<RewrittenFrame, SabiAdapterError> {
    let known = kind.known_field_numbers();
    match kind {
        SabiFrameKind::ExchangeRequest | SabiFrameKind::ExchangeResponse => {
            rewrite_exchange_wrapper_schema_major(wire, new_major, known)
                .map_err(SabiAdapterError::Schema)
        }
        _ => {
            rewrite_top_level_schema_major(wire, new_major, known).map_err(SabiAdapterError::Schema)
        }
    }
}

fn decode_envelope_from_kind(
    kind: SabiFrameKind,
    wire: &[u8],
) -> Result<sabi::v1::Envelope, SabiAdapterError> {
    match kind {
        SabiFrameKind::Envelope => sabi::v1::Envelope::decode(wire),
        SabiFrameKind::ExchangeRequest => {
            sabi::v1::ExchangeRequest::decode(wire).map(|msg| msg.envelope.unwrap_or_default())
        }
        SabiFrameKind::ExchangeResponse => {
            sabi::v1::ExchangeResponse::decode(wire).map(|msg| msg.envelope.unwrap_or_default())
        }
        _ => unreachable!(),
    }
    .map_err(|err| SabiAdapterError::Schema(CompatibilityError::MalformedProtobuf(err.to_string())))
}

#[derive(Default)]
struct ExtractedSemantics {
    request_id: Option<[u8; 16]>,
    idempotency_key: Option<[u8; 16]>,
    deadline_ns: Option<u64>,
    cancel_epoch: Option<u64>,
    auth_digest: Option<[u8; 32]>,
    uncertain: bool,
}

fn build_and_verify_trace(
    direction: CompatTranslationDirection,
    kind: SabiFrameKind,
    before: &RewrittenFrame,
    after_wire: &[u8],
    target_major: u32,
    warnings: Vec<DeprecationWarningReceipt>,
) -> Result<CompatTranslationTrace, SabiAdapterError> {
    let pre = extract_and_validate_semantics(kind, &before.rewritten_wire)?;
    let after_check = rewrite_for_kind(kind, after_wire, target_major)?;
    let post = extract_and_validate_semantics(kind, after_wire)?;

    if after_check.original_identity.major != target_major
        || after_check.original_identity.minor != before.original_identity.minor
        || after_check.original_identity.non_critical_extension_ids
            != before.original_identity.non_critical_extension_ids
        || after_check.unknown_field_numbers != before.unknown_field_numbers
        || after_check.unknown_field_bytes != before.unknown_field_bytes
        || pre.request_id != post.request_id
        || pre.idempotency_key != post.idempotency_key
        || pre.deadline_ns != post.deadline_ns
        || pre.cancel_epoch != post.cancel_epoch
        || pre.auth_digest != post.auth_digest
        || pre.uncertain != post.uncertain
    {
        return Err(SabiAdapterError::SemanticInvariantViolation(
            "translation mutated preserved identity, idempotency, deadline, extensions, unknown fields, or UNCERTAIN state",
        ));
    }

    Ok(CompatTranslationTrace {
        direction,
        channel: kind.channel().schema_name(),
        message_symbol: kind.message_symbol(),
        source_major: before.original_identity.major,
        source_minor: before.original_identity.minor,
        target_major,
        preserved_request_id: post.request_id,
        preserved_idempotency_key: post.idempotency_key,
        preserved_deadline_monotonic_ns: post.deadline_ns,
        preserved_cancel_epoch: post.cancel_epoch,
        preserved_authorization_digest: post.auth_digest,
        retained_non_critical_extensions: before
            .original_identity
            .non_critical_extension_ids
            .clone(),
        preserved_unknown_field_numbers: before.unknown_field_numbers.clone(),
        preserved_unknown_field_bytes: before.unknown_field_bytes,
        preserved_uncertain_outcome: post.uncertain,
        emitted_deprecation_warnings: warnings,
    })
}

fn extract_and_validate_semantics(
    kind: SabiFrameKind,
    wire: &[u8],
) -> Result<ExtractedSemantics, SabiAdapterError> {
    match kind {
        SabiFrameKind::Envelope
        | SabiFrameKind::ExchangeRequest
        | SabiFrameKind::ExchangeResponse => extract_envelope_semantics(kind, wire),
        SabiFrameKind::OperationStatus => extract_operation_status_semantics(wire),
        SabiFrameKind::CancelOperationRequest => {
            let req = sabi::v1::CancelOperationRequest::decode(wire).map_err(|e| {
                SabiAdapterError::Schema(CompatibilityError::MalformedProtobuf(e.to_string()))
            })?;
            Ok(ExtractedSemantics {
                cancel_epoch: Some(req.expected_cancel_epoch),
                ..ExtractedSemantics::default()
            })
        }
        SabiFrameKind::DriverProviderResponse => extract_driver_response_semantics(wire),
        SabiFrameKind::CreateChannelRequestWire => {
            let req = sabi::v1::CreateChannelRequestWire::decode(wire).map_err(|e| {
                SabiAdapterError::Schema(CompatibilityError::MalformedProtobuf(e.to_string()))
            })?;
            Ok(ExtractedSemantics {
                idempotency_key: to_fixed_16(&req.idempotency_key),
                ..ExtractedSemantics::default()
            })
        }
        _ => Ok(ExtractedSemantics::default()),
    }
}

fn extract_envelope_semantics(
    kind: SabiFrameKind,
    wire: &[u8],
) -> Result<ExtractedSemantics, SabiAdapterError> {
    let env = decode_envelope_from_kind(kind, wire)?;
    let mut out = ExtractedSemantics {
        request_id: to_fixed_16(&env.request_id),
        ..ExtractedSemantics::default()
    };
    match env.common_context.as_ref() {
        Some(sabi::v1::envelope::CommonContext::RequestContext(_)) => {
            let ctx = validate_sabi_request_context(&env, MethodSemantics::QUERY, 0)
                .map_err(SabiAdapterError::CommonSemantics)?;
            out.idempotency_key = to_fixed_16(&ctx.idempotency_key);
            out.deadline_ns = (ctx.deadline_monotonic_ns != 0).then_some(ctx.deadline_monotonic_ns);
            out.cancel_epoch = ctx.task_execution_binding.as_ref().map(|b| b.cancel_epoch);
            out.auth_digest = to_fixed_32(&ctx.proposal_or_input_digest_sha256);
        }
        Some(sabi::v1::envelope::CommonContext::ResponseContext(_)) => {
            let ctx = validate_sabi_response_context(&env, MethodSemantics::QUERY)
                .map_err(SabiAdapterError::CommonSemantics)?;
            if let Some(failure) = ctx.failure.as_ref() {
                let code = sabi::v1::SabiErrorCode::try_from(failure.code).ok();
                out.uncertain = matches!(
                    code,
                    Some(
                        sabi::v1::SabiErrorCode::Uncertain | sabi::v1::SabiErrorCode::EffectUnknown
                    )
                );
            }
        }
        None => {}
    }
    Ok(out)
}

fn extract_operation_status_semantics(wire: &[u8]) -> Result<ExtractedSemantics, SabiAdapterError> {
    let status = sabi::v1::OperationStatus::decode(wire).map_err(|e| {
        SabiAdapterError::Schema(CompatibilityError::MalformedProtobuf(e.to_string()))
    })?;
    let state = sabi::v1::OperationLifecycleState::try_from(status.state).ok();
    let uncertain = state == Some(sabi::v1::OperationLifecycleState::EffectUnknown);
    if uncertain && (status.operation.is_none() || status.receipt.is_none()) {
        return Err(SabiAdapterError::SemanticInvariantViolation(
            "OperationStatus EFFECT_UNKNOWN must retain operation and receipt references",
        ));
    }
    Ok(ExtractedSemantics {
        cancel_epoch: Some(status.cancel_epoch),
        uncertain,
        ..ExtractedSemantics::default()
    })
}

fn extract_driver_response_semantics(wire: &[u8]) -> Result<ExtractedSemantics, SabiAdapterError> {
    let resp = sabi::v1::DriverProviderResponse::decode(wire).map_err(|e| {
        SabiAdapterError::Schema(CompatibilityError::MalformedProtobuf(e.to_string()))
    })?;
    let mut out = ExtractedSemantics::default();
    match resp.response {
        Some(sabi::v1::driver_provider_response::Response::DispatchAck(ack)) => {
            out.cancel_epoch = Some(ack.cancel_epoch);
        }
        Some(sabi::v1::driver_provider_response::Response::CompleteAck(ack)) => {
            let outcome = sabi::v1::DriverProviderTerminalOutcome::try_from(ack.outcome).ok();
            let state = sabi::v1::OperationLifecycleState::try_from(ack.state).ok();
            out.uncertain = outcome == Some(sabi::v1::DriverProviderTerminalOutcome::EffectUnknown)
                || state == Some(sabi::v1::OperationLifecycleState::EffectUnknown);
            if out.uncertain && ack.receipt_id.len() != 16 {
                return Err(SabiAdapterError::SemanticInvariantViolation(
                    "DriverProviderCompleteAck EFFECT_UNKNOWN must retain 16-byte receipt_id",
                ));
            }
        }
        Some(sabi::v1::driver_provider_response::Response::CancelAck(ack)) => {
            out.cancel_epoch = Some(ack.cancel_epoch);
            let state = sabi::v1::OperationLifecycleState::try_from(ack.state).ok();
            out.uncertain = state == Some(sabi::v1::OperationLifecycleState::EffectUnknown);
        }
        _ => {}
    }
    Ok(out)
}

fn to_fixed_16(slice: &[u8]) -> Option<[u8; 16]> {
    slice.try_into().ok()
}

fn to_fixed_32(slice: &[u8]) -> Option<[u8; 32]> {
    slice.try_into().ok()
}
