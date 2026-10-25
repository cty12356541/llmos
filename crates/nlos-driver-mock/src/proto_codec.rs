//! ADR-0024 frozen protobuf payload codec (`nlos.sabi.DriverProvider` v1.0)
//! and `[TASK-PROVIDER-001]` provider isolation attestation verification.
//!
//! While [`crate::codec`] remains in place as the Stage B legacy `minicbor`
//! compatibility shim, all out-of-process `nlos-provider-worker` exchanges
//! run exclusively over the frozen `nlos.sabi.DriverProvider` schema channel
//! registered in `nlos-schema`.

use std::error::Error;
use std::fmt;
use std::num::NonZeroU64;

use nlos_operation::{
    CallbackTicket, CompletionOutcome, OperationHandle, OperationSnapshot, OperationSpec,
    OperationState,
};
use nlos_runtime::FiberHandle;
use nlos_schema::sabi::v1::{
    DriverProviderCancelAck, DriverProviderCancelRequest, DriverProviderCompleteAck,
    DriverProviderCompleteRequest, DriverProviderDispatchAck, DriverProviderDispatchRequest,
    DriverProviderIsolationReport, DriverProviderQueryIsolationRequest, DriverProviderRegisterAck,
    DriverProviderRegisterRequest, DriverProviderRequest, DriverProviderResponse,
    DriverProviderTerminalOutcome, OperationLifecycleState, ProviderIsolationAttestationWire,
    driver_provider_request, driver_provider_response,
};
use nlos_schema::{
    CompatibilityError, decode_driver_provider_request, decode_driver_provider_response,
    driver_provider_schema_identity, encode_driver_provider_request,
    encode_driver_provider_response,
};
use nlos_types::{
    CallbackId, CancelEpoch, CancellationScopeId, ExecutionFiberId, Generation, OperationId,
    ReceiptId,
};
use sha2::{Digest, Sha256};

use crate::provider::{
    CompleteProviderOperation, CompleteProviderOutcome, DispatchProviderOperation,
    DispatchProviderOutcome, RegisterProviderOutcome,
};

pub const DRIVER_PROVIDER_SERVICE: &str = "driver_provider";
pub const REGISTER_PROVIDER_METHOD: &str = "register";
pub const DISPATCH_PROVIDER_METHOD: &str = "dispatch";
pub const COMPLETE_PROVIDER_METHOD: &str = "complete";
pub const CANCEL_PROVIDER_METHOD: &str = "cancel";
pub const QUERY_ISOLATION_PROVIDER_METHOD: &str = "query_isolation";

/// Receipt marker emitted when all `[TASK-PROVIDER-001]` isolation guarantees
/// and a non-zero attestation digest are present.
pub const PROVIDER_ISOLATION_VERIFIED_TAG: &str = "PROVIDER_ISOLATION_VERIFIED";
/// Mandatory `[TASK-PROVIDER-001]` receipt marker when any tenant, session,
/// cache, or data-retention guarantee is absent or the attestation digest is
/// all-zeros.
pub const PROVIDER_ISOLATION_UNVERIFIED_TAG: &str = "PROVIDER_ISOLATION_UNVERIFIED";

/// Domain separator for deterministic worker child-process compute digests.
pub const WORKER_COMPUTE_DOMAIN: &[u8] = b"nlos/driver-provider-worker/compute/v1";
/// Domain separator for the default verified local worker attestation digest.
pub const DEFAULT_VERIFIED_ATTESTATION_DOMAIN: &[u8] =
    b"nlos/driver-provider-worker/verified-attestation/v1";

/// Individual `[TASK-PROVIDER-001]` isolation requirements checked on every
/// remote or out-of-process provider attestation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum IsolationGuarantee {
    TenantIsolated,
    SessionEphemeral,
    CachePartitioned,
    ZeroDataRetention,
    NonZeroAttestationDigest,
}

/// Configured isolation profile for `nlos-provider-worker`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderIsolationProfile {
    /// Full tenant/session/cache/zero-retention guarantees with a non-zero
    /// attestation digest unless explicitly overridden in tests.
    Verified,
    /// Shared cross-session or cross-tenant KV cache (`cache_partitioned = false`);
    /// triggers `[TASK-PROVIDER-001]` `PROVIDER_ISOLATION_UNVERIFIED`.
    SharedCacheUnverified,
}

impl ProviderIsolationProfile {
    #[must_use]
    pub const fn as_cli_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::SharedCacheUnverified => "shared-cache-unverified",
        }
    }

    /// Parses the CLI `--isolation-profile` flag value.
    ///
    /// # Errors
    ///
    /// Returns [`ProtoCodecError::InvalidIsolationProfile`] for unknown names.
    pub fn from_cli_str(value: &str) -> Result<Self, ProtoCodecError> {
        match value {
            "verified" => Ok(Self::Verified),
            "shared-cache-unverified" => Ok(Self::SharedCacheUnverified),
            other => Err(ProtoCodecError::InvalidIsolationProfile(other.to_owned())),
        }
    }

    /// Materializes the `[TASK-PROVIDER-001]` attestation announced by a worker
    /// running under this profile.
    #[must_use]
    pub fn to_attestation(
        self,
        attestation_override: Option<[u8; 32]>,
    ) -> ProviderIsolationAttestation {
        match self {
            Self::Verified => ProviderIsolationAttestation {
                tenant_isolated: true,
                session_ephemeral: true,
                cache_partitioned: true,
                zero_data_retention: true,
                attestation_digest: attestation_override
                    .unwrap_or_else(default_verified_attestation_digest),
                provider_profile: self.as_cli_str().to_owned(),
            },
            Self::SharedCacheUnverified => ProviderIsolationAttestation {
                tenant_isolated: true,
                session_ephemeral: true,
                cache_partitioned: false,
                zero_data_retention: true,
                attestation_digest: attestation_override.unwrap_or([0_u8; 32]),
                provider_profile: self.as_cli_str().to_owned(),
            },
        }
    }
}

/// `[TASK-PROVIDER-001]` attestation reported by a provider over the frozen
/// `nlos.sabi.DriverProvider` wire channel.
// Four orthogonal boolean axes are directly specified by [TASK-PROVIDER-001]
// (tenant, session, cache, and data-retention guarantees).
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderIsolationAttestation {
    pub tenant_isolated: bool,
    pub session_ephemeral: bool,
    pub cache_partitioned: bool,
    pub zero_data_retention: bool,
    pub attestation_digest: [u8; 32],
    pub provider_profile: String,
}

impl ProviderIsolationAttestation {
    /// Evaluates `[TASK-PROVIDER-001]` fail-closed: if ANY boolean guarantee
    /// is `false` or `attestation_digest` is all-zeros, returns
    /// [`ProviderIsolationPosture::ProviderIsolationUnverified`].
    #[must_use]
    pub fn evaluate_posture(&self) -> ProviderIsolationPosture {
        let mut missing = Vec::new();
        if !self.tenant_isolated {
            missing.push(IsolationGuarantee::TenantIsolated);
        }
        if !self.session_ephemeral {
            missing.push(IsolationGuarantee::SessionEphemeral);
        }
        if !self.cache_partitioned {
            missing.push(IsolationGuarantee::CachePartitioned);
        }
        if !self.zero_data_retention {
            missing.push(IsolationGuarantee::ZeroDataRetention);
        }
        if self.attestation_digest == [0_u8; 32] {
            missing.push(IsolationGuarantee::NonZeroAttestationDigest);
        }
        if missing.is_empty() {
            ProviderIsolationPosture::VerifiedEndToEndIsolated {
                attestation_digest: self.attestation_digest,
            }
        } else {
            ProviderIsolationPosture::ProviderIsolationUnverified {
                missing_guarantees: missing,
            }
        }
    }

    #[must_use]
    pub fn to_wire(&self) -> ProviderIsolationAttestationWire {
        ProviderIsolationAttestationWire {
            tenant_isolated: self.tenant_isolated,
            session_ephemeral: self.session_ephemeral,
            cache_partitioned: self.cache_partitioned,
            zero_data_retention: self.zero_data_retention,
            attestation_digest: self.attestation_digest.to_vec(),
            provider_profile: self.provider_profile.clone(),
        }
    }

    /// Reconstructs the domain attestation from a validated wire message.
    ///
    /// # Errors
    ///
    /// Fails closed if `attestation_digest` is not 32 bytes or `provider_profile`
    /// is empty or contains NUL.
    pub fn from_wire(wire: &ProviderIsolationAttestationWire) -> Result<Self, ProtoCodecError> {
        let attestation_digest: [u8; 32] = wire
            .attestation_digest
            .as_slice()
            .try_into()
            .map_err(|_| ProtoCodecError::InvalidIdentifierWidth)?;
        if wire.provider_profile.is_empty() || wire.provider_profile.contains('\0') {
            return Err(ProtoCodecError::InvalidIsolationProfile(
                wire.provider_profile.clone(),
            ));
        }
        Ok(Self {
            tenant_isolated: wire.tenant_isolated,
            session_ephemeral: wire.session_ephemeral,
            cache_partitioned: wire.cache_partitioned,
            zero_data_retention: wire.zero_data_retention,
            attestation_digest,
            provider_profile: wire.provider_profile.clone(),
        })
    }
}

/// `[TASK-PROVIDER-001]` verified vs unverified isolation verdict attached to
/// every provider execution receipt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProviderIsolationPosture {
    VerifiedEndToEndIsolated {
        attestation_digest: [u8; 32],
    },
    ProviderIsolationUnverified {
        missing_guarantees: Vec<IsolationGuarantee>,
    },
}

impl ProviderIsolationPosture {
    #[must_use]
    pub const fn is_verified(&self) -> bool {
        matches!(self, Self::VerifiedEndToEndIsolated { .. })
    }

    #[must_use]
    pub const fn receipt_marker(&self) -> &'static str {
        match self {
            Self::VerifiedEndToEndIsolated { .. } => PROVIDER_ISOLATION_VERIFIED_TAG,
            Self::ProviderIsolationUnverified { .. } => PROVIDER_ISOLATION_UNVERIFIED_TAG,
        }
    }
}

/// Domain cancellation request carried over `nlos.sabi.DriverProvider`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CancelProviderOperation {
    pub handle: OperationHandle,
    pub expected_cancel_epoch: CancelEpoch,
    pub no_effect_receipt_id: ReceiptId,
}

/// Domain cancellation acknowledgment carried over `nlos.sabi.DriverProvider`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CancelProviderOutcome {
    pub replayed: bool,
    pub already_terminal: bool,
    pub handle: OperationHandle,
    pub state: OperationState,
    pub cancel_epoch: CancelEpoch,
}

impl CancelProviderOutcome {
    #[must_use]
    pub const fn from_snapshot(
        replayed: bool,
        already_terminal: bool,
        snapshot: OperationSnapshot,
    ) -> Self {
        Self {
            replayed,
            already_terminal,
            handle: snapshot.handle,
            state: snapshot.state,
            cancel_epoch: snapshot.cancel_epoch,
        }
    }
}

/// Execution receipt returned by an out-of-process provider completion,
/// carrying both durable operation outcome facts and `[TASK-PROVIDER-001]`
/// isolation posture.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderExecutionReceipt {
    pub replayed: bool,
    pub outcome: CompletionOutcome,
    pub receipt_id: ReceiptId,
    pub state: OperationState,
    pub compute_digest: [u8; 32],
    pub fenced_reconciliation_only: bool,
    pub attestation: ProviderIsolationAttestation,
    pub isolation_posture: ProviderIsolationPosture,
    pub isolation_marker: &'static str,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DecodedProviderRequest {
    Register(OperationSpec),
    Dispatch(DispatchProviderOperation),
    Complete(CompleteProviderOperation),
    Cancel(CancelProviderOperation),
    QueryIsolation,
}

impl DecodedProviderRequest {
    #[must_use]
    pub const fn expected_method(&self) -> &'static str {
        match self {
            Self::Register(_) => REGISTER_PROVIDER_METHOD,
            Self::Dispatch(_) => DISPATCH_PROVIDER_METHOD,
            Self::Complete(_) => COMPLETE_PROVIDER_METHOD,
            Self::Cancel(_) => CANCEL_PROVIDER_METHOD,
            Self::QueryIsolation => QUERY_ISOLATION_PROVIDER_METHOD,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DecodedProviderResponse {
    RegisterAck(RegisterProviderOutcome),
    DispatchAck(DispatchProviderOutcome),
    CompleteAck(ProviderExecutionReceipt),
    CancelAck(CancelProviderOutcome),
    IsolationReport(ProviderIsolationAttestation),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProtoCodecError {
    Schema(CompatibilityError),
    UnexpectedResponseArm,
    InvalidIdentifierWidth,
    ZeroGeneration,
    StateOutcomeMismatch,
    InvalidIsolationProfile(String),
}

impl fmt::Display for ProtoCodecError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Schema(error) => write!(formatter, "DriverProvider schema error: {error}"),
            Self::UnexpectedResponseArm => {
                formatter.write_str("DriverProvider response arm did not match requested method")
            }
            Self::InvalidIdentifierWidth => {
                formatter.write_str("DriverProvider field violated fixed 16B/32B width")
            }
            Self::ZeroGeneration => formatter.write_str("DriverProvider generation is zero"),
            Self::StateOutcomeMismatch => {
                formatter.write_str("DriverProvider terminal outcome and lifecycle state disagree")
            }
            Self::InvalidIsolationProfile(profile) => {
                write!(
                    formatter,
                    "unsupported provider isolation profile {profile:?}"
                )
            }
        }
    }
}

impl Error for ProtoCodecError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Schema(error) => Some(error),
            Self::UnexpectedResponseArm
            | Self::InvalidIdentifierWidth
            | Self::ZeroGeneration
            | Self::StateOutcomeMismatch
            | Self::InvalidIsolationProfile(_) => None,
        }
    }
}

impl From<CompatibilityError> for ProtoCodecError {
    fn from(error: CompatibilityError) -> Self {
        Self::Schema(error)
    }
}

/// Deterministic SHA-256 compute digest produced inside the `nlos-provider-worker`
/// child OS process for a given `(operation, callback, seed)` tuple.
#[must_use]
pub fn derive_worker_compute_digest(
    handle: OperationHandle,
    callback_id: CallbackId,
    seed: &[u8; 32],
) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(WORKER_COMPUTE_DOMAIN);
    hasher.update(handle.operation_id.as_bytes());
    hasher.update(handle.generation.get().to_be_bytes());
    hasher.update(callback_id.as_bytes());
    hasher.update(seed);
    hasher.finalize().into()
}

/// Default non-zero 32-byte attestation digest for local verified workers.
#[must_use]
pub fn default_verified_attestation_digest() -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(DEFAULT_VERIFIED_ATTESTATION_DOMAIN);
    hasher.finalize().into()
}

/// Encodes one `register` request onto the frozen `nlos.sabi.DriverProvider` wire.
///
/// # Errors
///
/// Returns [`ProtoCodecError`] if schema validation fails.
pub fn encode_register_request_proto(spec: &OperationSpec) -> Result<Vec<u8>, ProtoCodecError> {
    let request = DriverProviderRequest {
        schema: Some(driver_provider_schema_identity()),
        request: Some(driver_provider_request::Request::Register(
            DriverProviderRegisterRequest {
                operation_id: spec.operation_id.as_bytes().to_vec(),
                operation_generation: spec.generation.get(),
                owner_fiber_id: spec.owner_fiber.fiber_id.as_bytes().to_vec(),
                owner_fiber_generation: spec.owner_fiber.generation.get(),
                cancellation_scope_id: spec.cancellation_scope_id.as_bytes().to_vec(),
                cancellation_generation: spec.cancellation_generation.get(),
            },
        )),
    };
    Ok(encode_driver_provider_request(&request)?)
}

/// Encodes one `dispatch` request onto the frozen `nlos.sabi.DriverProvider` wire.
///
/// # Errors
///
/// Returns [`ProtoCodecError`] if schema validation fails.
pub fn encode_dispatch_request_proto(
    dispatch: &DispatchProviderOperation,
) -> Result<Vec<u8>, ProtoCodecError> {
    let request = DriverProviderRequest {
        schema: Some(driver_provider_schema_identity()),
        request: Some(driver_provider_request::Request::Dispatch(
            DriverProviderDispatchRequest {
                operation_id: dispatch.handle.operation_id.as_bytes().to_vec(),
                operation_generation: dispatch.handle.generation.get(),
                callback_id: dispatch.callback_id.as_bytes().to_vec(),
            },
        )),
    };
    Ok(encode_driver_provider_request(&request)?)
}

/// Encodes one `complete` request onto the frozen `nlos.sabi.DriverProvider` wire.
///
/// # Errors
///
/// Returns [`ProtoCodecError`] if schema validation fails.
pub fn encode_complete_request_proto(
    complete: &CompleteProviderOperation,
) -> Result<Vec<u8>, ProtoCodecError> {
    let request = DriverProviderRequest {
        schema: Some(driver_provider_schema_identity()),
        request: Some(driver_provider_request::Request::Complete(
            DriverProviderCompleteRequest {
                operation_id: complete.handle.operation_id.as_bytes().to_vec(),
                operation_generation: complete.handle.generation.get(),
                callback_id: complete.callback_id.as_bytes().to_vec(),
                seed: complete.seed.to_vec(),
            },
        )),
    };
    Ok(encode_driver_provider_request(&request)?)
}

/// Encodes one `cancel` request onto the frozen `nlos.sabi.DriverProvider` wire.
///
/// # Errors
///
/// Returns [`ProtoCodecError`] if schema validation fails.
pub fn encode_cancel_request_proto(
    cancel: &CancelProviderOperation,
) -> Result<Vec<u8>, ProtoCodecError> {
    let request = DriverProviderRequest {
        schema: Some(driver_provider_schema_identity()),
        request: Some(driver_provider_request::Request::Cancel(
            DriverProviderCancelRequest {
                operation_id: cancel.handle.operation_id.as_bytes().to_vec(),
                operation_generation: cancel.handle.generation.get(),
                expected_cancel_epoch: cancel.expected_cancel_epoch.get(),
                no_effect_receipt_id: cancel.no_effect_receipt_id.as_bytes().to_vec(),
            },
        )),
    };
    Ok(encode_driver_provider_request(&request)?)
}

/// Encodes one `query_isolation` request onto the frozen `nlos.sabi.DriverProvider` wire.
///
/// # Errors
///
/// Returns [`ProtoCodecError`] if schema validation fails.
pub fn encode_query_isolation_request_proto() -> Result<Vec<u8>, ProtoCodecError> {
    let request = DriverProviderRequest {
        schema: Some(driver_provider_schema_identity()),
        request: Some(driver_provider_request::Request::QueryIsolation(
            DriverProviderQueryIsolationRequest {},
        )),
    };
    Ok(encode_driver_provider_request(&request)?)
}

/// Decodes and validates a `nlos.sabi.DriverProvider` request payload into its
/// domain representation.
///
/// # Errors
///
/// Fails closed on malformed, oversized, wrong-schema, or out-of-bounds wire bytes.
pub fn decode_request_proto(wire: &[u8]) -> Result<DecodedProviderRequest, ProtoCodecError> {
    use driver_provider_request::Request;

    let decoded = decode_driver_provider_request(wire)?;
    let arm = decoded
        .request
        .ok_or(CompatibilityError::MissingDriverProviderBody)?;
    match arm {
        Request::Register(reg) => Ok(DecodedProviderRequest::Register(OperationSpec {
            operation_id: OperationId::from_bytes(id16(&reg.operation_id)?),
            generation: generation(reg.operation_generation)?,
            owner_fiber: FiberHandle {
                fiber_id: ExecutionFiberId::from_bytes(id16(&reg.owner_fiber_id)?),
                generation: generation(reg.owner_fiber_generation)?,
            },
            cancellation_scope_id: CancellationScopeId::from_bytes(id16(
                &reg.cancellation_scope_id,
            )?),
            cancellation_generation: generation(reg.cancellation_generation)?,
        })),
        Request::Dispatch(disp) => Ok(DecodedProviderRequest::Dispatch(
            DispatchProviderOperation {
                handle: OperationHandle {
                    operation_id: OperationId::from_bytes(id16(&disp.operation_id)?),
                    generation: generation(disp.operation_generation)?,
                },
                callback_id: CallbackId::from_bytes(id16(&disp.callback_id)?),
            },
        )),
        Request::Complete(comp) => Ok(DecodedProviderRequest::Complete(
            CompleteProviderOperation {
                handle: OperationHandle {
                    operation_id: OperationId::from_bytes(id16(&comp.operation_id)?),
                    generation: generation(comp.operation_generation)?,
                },
                callback_id: CallbackId::from_bytes(id16(&comp.callback_id)?),
                seed: id32(&comp.seed)?,
            },
        )),
        Request::Cancel(cancel) => Ok(DecodedProviderRequest::Cancel(CancelProviderOperation {
            handle: OperationHandle {
                operation_id: OperationId::from_bytes(id16(&cancel.operation_id)?),
                generation: generation(cancel.operation_generation)?,
            },
            expected_cancel_epoch: CancelEpoch::new(cancel.expected_cancel_epoch),
            no_effect_receipt_id: ReceiptId::from_bytes(id16(&cancel.no_effect_receipt_id)?),
        })),
        Request::QueryIsolation(_) => Ok(DecodedProviderRequest::QueryIsolation),
    }
}

/// Encodes one `register_ack` response onto the frozen `nlos.sabi.DriverProvider` wire.
///
/// # Errors
///
/// Returns [`ProtoCodecError`] if schema validation fails.
pub fn encode_register_ack_proto(
    outcome: &RegisterProviderOutcome,
) -> Result<Vec<u8>, ProtoCodecError> {
    let response = DriverProviderResponse {
        schema: Some(driver_provider_schema_identity()),
        response: Some(driver_provider_response::Response::RegisterAck(
            DriverProviderRegisterAck {
                replayed: outcome.replayed,
                operation_id: outcome.handle.operation_id.as_bytes().to_vec(),
                operation_generation: outcome.handle.generation.get(),
                admission_receipt_id: outcome.admission_receipt_id.as_bytes().to_vec(),
            },
        )),
    };
    Ok(encode_driver_provider_response(&response)?)
}

/// Encodes one `dispatch_ack` response onto the frozen `nlos.sabi.DriverProvider` wire.
///
/// # Errors
///
/// Returns [`ProtoCodecError`] if schema validation fails.
pub fn encode_dispatch_ack_proto(
    outcome: &DispatchProviderOutcome,
) -> Result<Vec<u8>, ProtoCodecError> {
    let response = DriverProviderResponse {
        schema: Some(driver_provider_schema_identity()),
        response: Some(driver_provider_response::Response::DispatchAck(
            DriverProviderDispatchAck {
                replayed: outcome.replayed,
                preparation_receipt_id: outcome.preparation_receipt_id.as_bytes().to_vec(),
                activation_receipt_id: outcome.activation_receipt_id.as_bytes().to_vec(),
                callback_id: outcome.ticket.callback_id.as_bytes().to_vec(),
                operation_id: outcome.ticket.operation.operation_id.as_bytes().to_vec(),
                operation_generation: outcome.ticket.operation.generation.get(),
                owner_fiber_id: outcome.ticket.owner_fiber.fiber_id.as_bytes().to_vec(),
                owner_fiber_generation: outcome.ticket.owner_fiber.generation.get(),
                cancel_epoch: outcome.ticket.cancel_epoch.get(),
            },
        )),
    };
    Ok(encode_driver_provider_response(&response)?)
}

/// Encodes one `complete_ack` response onto the frozen `nlos.sabi.DriverProvider` wire.
///
/// # Errors
///
/// Returns [`ProtoCodecError`] if schema validation fails.
pub fn encode_complete_ack_proto(
    outcome: &CompleteProviderOutcome,
    compute_digest: [u8; 32],
    fenced_reconciliation_only: bool,
    attestation: &ProviderIsolationAttestation,
) -> Result<Vec<u8>, ProtoCodecError> {
    let (wire_outcome, receipt_id) = completion_to_wire(outcome.outcome);
    let wire_state = state_to_lifecycle(outcome.state);
    let response = DriverProviderResponse {
        schema: Some(driver_provider_schema_identity()),
        response: Some(driver_provider_response::Response::CompleteAck(
            DriverProviderCompleteAck {
                replayed: outcome.replayed,
                outcome: wire_outcome.into(),
                receipt_id: receipt_id.as_bytes().to_vec(),
                state: wire_state.into(),
                compute_digest: compute_digest.to_vec(),
                isolation: Some(attestation.to_wire()),
                fenced_reconciliation_only,
            },
        )),
    };
    Ok(encode_driver_provider_response(&response)?)
}

/// Encodes one `cancel_ack` response onto the frozen `nlos.sabi.DriverProvider` wire.
///
/// # Errors
///
/// Returns [`ProtoCodecError`] if schema validation fails.
pub fn encode_cancel_ack_proto(
    outcome: &CancelProviderOutcome,
) -> Result<Vec<u8>, ProtoCodecError> {
    let terminal_receipt_id =
        terminal_receipt_of_state(outcome.state).map_or_else(Vec::new, |id| id.as_bytes().to_vec());
    let response = DriverProviderResponse {
        schema: Some(driver_provider_schema_identity()),
        response: Some(driver_provider_response::Response::CancelAck(
            DriverProviderCancelAck {
                replayed: outcome.replayed,
                already_terminal: outcome.already_terminal,
                operation_id: outcome.handle.operation_id.as_bytes().to_vec(),
                operation_generation: outcome.handle.generation.get(),
                state: state_to_lifecycle(outcome.state).into(),
                cancel_epoch: outcome.cancel_epoch.get(),
                terminal_receipt_id,
            },
        )),
    };
    Ok(encode_driver_provider_response(&response)?)
}

/// Encodes one `isolation_report` response onto the frozen `nlos.sabi.DriverProvider` wire.
///
/// # Errors
///
/// Returns [`ProtoCodecError`] if schema validation fails.
pub fn encode_isolation_report_proto(
    attestation: &ProviderIsolationAttestation,
) -> Result<Vec<u8>, ProtoCodecError> {
    let response = DriverProviderResponse {
        schema: Some(driver_provider_schema_identity()),
        response: Some(driver_provider_response::Response::IsolationReport(
            DriverProviderIsolationReport {
                isolation: Some(attestation.to_wire()),
            },
        )),
    };
    Ok(encode_driver_provider_response(&response)?)
}

/// Decodes and validates a `nlos.sabi.DriverProvider` response payload,
/// independently re-evaluating `[TASK-PROVIDER-001]` isolation posture on
/// completion receipts.
///
/// # Errors
///
/// Fails closed on malformed, oversized, wrong-schema, or inconsistent wire bytes.
pub fn decode_response_proto(wire: &[u8]) -> Result<DecodedProviderResponse, ProtoCodecError> {
    use driver_provider_response::Response;

    let decoded = decode_driver_provider_response(wire)?;
    let arm = decoded
        .response
        .ok_or(CompatibilityError::MissingDriverProviderBody)?;
    match arm {
        Response::RegisterAck(ack) => Ok(DecodedProviderResponse::RegisterAck(
            RegisterProviderOutcome {
                replayed: ack.replayed,
                handle: OperationHandle {
                    operation_id: OperationId::from_bytes(id16(&ack.operation_id)?),
                    generation: generation(ack.operation_generation)?,
                },
                admission_receipt_id: ReceiptId::from_bytes(id16(&ack.admission_receipt_id)?),
            },
        )),
        Response::DispatchAck(ack) => Ok(DecodedProviderResponse::DispatchAck(
            DispatchProviderOutcome {
                replayed: ack.replayed,
                preparation_receipt_id: ReceiptId::from_bytes(id16(&ack.preparation_receipt_id)?),
                activation_receipt_id: ReceiptId::from_bytes(id16(&ack.activation_receipt_id)?),
                ticket: CallbackTicket {
                    callback_id: CallbackId::from_bytes(id16(&ack.callback_id)?),
                    operation: OperationHandle {
                        operation_id: OperationId::from_bytes(id16(&ack.operation_id)?),
                        generation: generation(ack.operation_generation)?,
                    },
                    owner_fiber: FiberHandle {
                        fiber_id: ExecutionFiberId::from_bytes(id16(&ack.owner_fiber_id)?),
                        generation: generation(ack.owner_fiber_generation)?,
                    },
                    cancel_epoch: CancelEpoch::new(ack.cancel_epoch),
                },
            },
        )),
        Response::CompleteAck(ack) => decode_complete_ack(&ack),
        Response::CancelAck(ack) => decode_cancel_ack(&ack),
        Response::IsolationReport(report) => {
            let wire_iso = report
                .isolation
                .as_ref()
                .ok_or(CompatibilityError::MissingDriverProviderBody)?;
            Ok(DecodedProviderResponse::IsolationReport(
                ProviderIsolationAttestation::from_wire(wire_iso)?,
            ))
        }
    }
}

fn decode_complete_ack(
    ack: &DriverProviderCompleteAck,
) -> Result<DecodedProviderResponse, ProtoCodecError> {
    let receipt_id = ReceiptId::from_bytes(id16(&ack.receipt_id)?);
    let compute_digest = id32(&ack.compute_digest)?;
    let wire_outcome = DriverProviderTerminalOutcome::try_from(ack.outcome)
        .map_err(|_| CompatibilityError::UnspecifiedDriverProviderOutcome)?;
    let outcome = completion_from_wire(wire_outcome, receipt_id)?;
    let wire_state = OperationLifecycleState::try_from(ack.state)
        .map_err(|_| CompatibilityError::UnspecifiedDriverProviderOutcome)?;
    let state = lifecycle_to_state(wire_state, Some(receipt_id))?;
    if terminal_receipt_of_state(state) != Some(receipt_id) {
        return Err(ProtoCodecError::StateOutcomeMismatch);
    }
    let wire_iso = ack
        .isolation
        .as_ref()
        .ok_or(CompatibilityError::MissingDriverProviderBody)?;
    let attestation = ProviderIsolationAttestation::from_wire(wire_iso)?;
    let isolation_posture = attestation.evaluate_posture();
    let isolation_marker = isolation_posture.receipt_marker();
    Ok(DecodedProviderResponse::CompleteAck(
        ProviderExecutionReceipt {
            replayed: ack.replayed,
            outcome,
            receipt_id,
            state,
            compute_digest,
            fenced_reconciliation_only: ack.fenced_reconciliation_only,
            attestation,
            isolation_posture,
            isolation_marker,
        },
    ))
}

fn decode_cancel_ack(
    ack: &DriverProviderCancelAck,
) -> Result<DecodedProviderResponse, ProtoCodecError> {
    let handle = OperationHandle {
        operation_id: OperationId::from_bytes(id16(&ack.operation_id)?),
        generation: generation(ack.operation_generation)?,
    };
    let wire_state = OperationLifecycleState::try_from(ack.state)
        .map_err(|_| CompatibilityError::UnspecifiedDriverProviderOutcome)?;
    let receipt = if ack.terminal_receipt_id.is_empty() {
        None
    } else {
        Some(ReceiptId::from_bytes(id16(&ack.terminal_receipt_id)?))
    };
    let state = lifecycle_to_state(wire_state, receipt)?;
    Ok(DecodedProviderResponse::CancelAck(CancelProviderOutcome {
        replayed: ack.replayed,
        already_terminal: ack.already_terminal,
        handle,
        state,
        cancel_epoch: CancelEpoch::new(ack.cancel_epoch),
    }))
}

#[must_use]
pub const fn terminal_receipt_of_state(state: OperationState) -> Option<ReceiptId> {
    match state {
        OperationState::Registered
        | OperationState::Dispatched
        | OperationState::CancelRequested => None,
        OperationState::Completed { receipt_id }
        | OperationState::Failed { receipt_id }
        | OperationState::CancelledBeforeEffect { receipt_id }
        | OperationState::PartialEffect { receipt_id }
        | OperationState::EffectUnknown { receipt_id } => Some(receipt_id),
    }
}

const fn completion_to_wire(
    outcome: CompletionOutcome,
) -> (DriverProviderTerminalOutcome, ReceiptId) {
    match outcome {
        CompletionOutcome::Completed { receipt_id } => {
            (DriverProviderTerminalOutcome::Completed, receipt_id)
        }
        CompletionOutcome::Failed { receipt_id } => {
            (DriverProviderTerminalOutcome::Failed, receipt_id)
        }
        CompletionOutcome::PartialEffect { receipt_id } => {
            (DriverProviderTerminalOutcome::PartialEffect, receipt_id)
        }
        CompletionOutcome::EffectUnknown { receipt_id } => {
            (DriverProviderTerminalOutcome::EffectUnknown, receipt_id)
        }
        CompletionOutcome::CancelledBeforeEffect { receipt_id } => (
            DriverProviderTerminalOutcome::CancelledBeforeEffect,
            receipt_id,
        ),
    }
}

const fn completion_from_wire(
    outcome: DriverProviderTerminalOutcome,
    receipt_id: ReceiptId,
) -> Result<CompletionOutcome, ProtoCodecError> {
    match outcome {
        DriverProviderTerminalOutcome::Completed => Ok(CompletionOutcome::Completed { receipt_id }),
        DriverProviderTerminalOutcome::Failed => Ok(CompletionOutcome::Failed { receipt_id }),
        DriverProviderTerminalOutcome::PartialEffect => {
            Ok(CompletionOutcome::PartialEffect { receipt_id })
        }
        DriverProviderTerminalOutcome::EffectUnknown => {
            Ok(CompletionOutcome::EffectUnknown { receipt_id })
        }
        DriverProviderTerminalOutcome::CancelledBeforeEffect => {
            Ok(CompletionOutcome::CancelledBeforeEffect { receipt_id })
        }
        DriverProviderTerminalOutcome::Unspecified => Err(ProtoCodecError::StateOutcomeMismatch),
    }
}

const fn state_to_lifecycle(state: OperationState) -> OperationLifecycleState {
    match state {
        OperationState::Registered => OperationLifecycleState::Registered,
        OperationState::Dispatched => OperationLifecycleState::Dispatched,
        OperationState::CancelRequested => OperationLifecycleState::CancelRequested,
        OperationState::Completed { .. } => OperationLifecycleState::Completed,
        OperationState::Failed { .. } => OperationLifecycleState::Failed,
        OperationState::CancelledBeforeEffect { .. } => {
            OperationLifecycleState::CancelledBeforeEffect
        }
        OperationState::PartialEffect { .. } => OperationLifecycleState::PartialEffect,
        OperationState::EffectUnknown { .. } => OperationLifecycleState::EffectUnknown,
    }
}

fn lifecycle_to_state(
    state: OperationLifecycleState,
    receipt_id: Option<ReceiptId>,
) -> Result<OperationState, ProtoCodecError> {
    match (state, receipt_id) {
        (OperationLifecycleState::Registered, None) => Ok(OperationState::Registered),
        (OperationLifecycleState::Dispatched, None) => Ok(OperationState::Dispatched),
        (OperationLifecycleState::CancelRequested, None) => Ok(OperationState::CancelRequested),
        (OperationLifecycleState::Completed, Some(receipt_id)) => {
            Ok(OperationState::Completed { receipt_id })
        }
        (OperationLifecycleState::Failed, Some(receipt_id)) => {
            Ok(OperationState::Failed { receipt_id })
        }
        (OperationLifecycleState::CancelledBeforeEffect, Some(receipt_id)) => {
            Ok(OperationState::CancelledBeforeEffect { receipt_id })
        }
        (OperationLifecycleState::PartialEffect, Some(receipt_id)) => {
            Ok(OperationState::PartialEffect { receipt_id })
        }
        (OperationLifecycleState::EffectUnknown, Some(receipt_id)) => {
            Ok(OperationState::EffectUnknown { receipt_id })
        }
        _ => Err(ProtoCodecError::StateOutcomeMismatch),
    }
}

fn id16(bytes: &[u8]) -> Result<[u8; 16], ProtoCodecError> {
    bytes
        .try_into()
        .map_err(|_| ProtoCodecError::InvalidIdentifierWidth)
}

fn id32(bytes: &[u8]) -> Result<[u8; 32], ProtoCodecError> {
    bytes
        .try_into()
        .map_err(|_| ProtoCodecError::InvalidIdentifierWidth)
}

fn generation(value: u64) -> Result<Generation, ProtoCodecError> {
    NonZeroU64::new(value)
        .map(Generation::new)
        .ok_or(ProtoCodecError::ZeroGeneration)
}
