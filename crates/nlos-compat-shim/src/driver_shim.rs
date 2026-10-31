//! Driver Protocol Shim (`[DRV-PROTO-001]`, `[DRV-NEGO-001]`, `[TASK-PROVIDER-001]`,
//! `ADR-0024`, `ADR-0027` section 2.2).
//!
//! [`DriverProtocolShim`] provides a pure, deterministic, unprivileged bridge
//! between Stage B legacy `minicbor` (`nlos.driver-mock.payload` v1) or
//! historical `DriverProvider` epochs (`N-1` / `N-2`) and the frozen 8th SABI
//! channel `nlos.sabi.DriverProvider` v1.0 Protobuf (`DriverProviderRequest` /
//! `DriverProviderResponse`):
//! - preserves 1:1 fidelity across `OperationId`, `operation_generation`,
//!   `ExecutionFiberId`, `owner_fiber_generation`, `CancellationScopeId`,
//!   `cancellation_generation`, `CallbackId`, `CancelEpoch`, `ReceiptId`,
//!   `seed`, `OperationState`, and `CompletionOutcome`;
//! - enforces `[TASK-PROVIDER-001]` isolation honesty: legacy `minicbor` frames
//!   and any incomplete or zero-digest attestation are strictly classified as
//!   `PROVIDER_ISOLATION_UNVERIFIED_TAG`, and any attempt to elevate an
//!   unverified adapter or forge a non-zero attestation digest fails closed.

use std::error::Error;
use std::fmt;
use std::num::NonZeroU64;

use minicbor::{Decoder, Encoder};
pub use nlos_driver_mock::codec::{
    CodecError, CompleteOperationResultWire, CompleteOperationWire, DispatchOperationResultWire,
    DispatchOperationWire, MAX_DRIVER_MOCK_PAYLOAD_BYTES, OUTCOME_CODE_CANCELLED_BEFORE_EFFECT,
    OUTCOME_CODE_COMPLETED, OUTCOME_CODE_EFFECT_UNKNOWN, OUTCOME_CODE_FAILED,
    OUTCOME_CODE_PARTIAL_EFFECT, PAYLOAD_SCHEMA_MAJOR, PAYLOAD_SCHEMA_NAME,
    RegisterOperationResultWire, RegisterOperationWire, STATE_CODE_CANCEL_REQUESTED,
    STATE_CODE_CANCELLED_BEFORE_EFFECT, STATE_CODE_COMPLETED, STATE_CODE_DISPATCHED,
    STATE_CODE_EFFECT_UNKNOWN, STATE_CODE_FAILED, STATE_CODE_PARTIAL_EFFECT, STATE_CODE_REGISTERED,
    TerminalOutcomeKind, decode_complete_request, decode_complete_result, decode_dispatch_request,
    decode_dispatch_result, decode_register_request, decode_register_result,
    encode_complete_request, encode_complete_result, encode_dispatch_request,
    encode_dispatch_result, encode_register_request, encode_register_result,
    outcome_from_wire_code, outcome_wire_code, state_from_wire_code, state_wire_code,
};
pub use nlos_driver_mock::proto_codec::{
    CANCEL_PROVIDER_METHOD, COMPLETE_PROVIDER_METHOD, CancelProviderOperation,
    CancelProviderOutcome, DISPATCH_PROVIDER_METHOD, DRIVER_PROVIDER_SERVICE,
    DecodedProviderRequest, DecodedProviderResponse, PROVIDER_ISOLATION_UNVERIFIED_TAG,
    PROVIDER_ISOLATION_VERIFIED_TAG, ProtoCodecError, ProviderExecutionReceipt,
    ProviderIsolationAttestation, ProviderIsolationPosture, QUERY_ISOLATION_PROVIDER_METHOD,
    REGISTER_PROVIDER_METHOD, decode_request_proto, decode_response_proto,
    derive_worker_compute_digest, encode_cancel_ack_proto, encode_cancel_request_proto,
    encode_complete_ack_proto, encode_complete_request_proto, encode_dispatch_ack_proto,
    encode_dispatch_request_proto, encode_isolation_report_proto,
    encode_query_isolation_request_proto, encode_register_ack_proto, encode_register_request_proto,
    terminal_receipt_of_state,
};
use nlos_driver_mock::provider::{
    CompleteProviderOperation, CompleteProviderOutcome, DispatchProviderOperation,
    DispatchProviderOutcome, RegisterProviderOutcome,
};
use nlos_operation::{
    CallbackTicket, CompletionOutcome, OperationHandle, OperationSpec, OperationState,
};
use nlos_runtime::FiberHandle;
use nlos_schema::sabi::v1::{DriverProviderRequest, DriverProviderResponse};
use nlos_schema::{
    CompatibilityError, MAX_DRIVER_PROVIDER_PAYLOAD_BYTES, MAX_EXTENSION_IDS,
    SABI_DRIVER_PROVIDER_SCHEMA,
};
use nlos_types::{
    CallbackId, CancelEpoch, CancellationScopeId, ExecutionFiberId, Generation, OperationId,
    ReceiptId,
};
use prost::Message;

const ID_BYTES: usize = 16;
const DIGEST_BYTES: usize = 32;

/// Canonical `provider_profile` marker synthesized for Stage B legacy `minicbor`
/// adapters that lack `[TASK-PROVIDER-001]` isolation attestations.
pub const LEGACY_MINICBOR_UNVERIFIED_PROFILE: &str = "legacy-stage-b-minicbor-unverified";

/// Deterministic `minicbor` cancellation request frame following the
/// `nlos.driver-mock.payload` v1 ascending-key map discipline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CancelOperationWire {
    pub operation_id: [u8; ID_BYTES],
    pub operation_generation: u64,
    pub expected_cancel_epoch: u64,
    pub no_effect_receipt_id: [u8; ID_BYTES],
}

/// Deterministic `minicbor` cancellation acknowledgment frame following the
/// `nlos.driver-mock.payload` v1 ascending-key map discipline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CancelOperationResultWire {
    pub replayed: bool,
    pub already_terminal: bool,
    pub operation_id: [u8; ID_BYTES],
    pub operation_generation: u64,
    pub state_code: u8,
    pub cancel_epoch: u64,
    pub terminal_receipt_id: Option<[u8; ID_BYTES]>,
}

/// Encodes a [`CancelOperationWire`] into deterministic `nlos.driver-mock.payload`
/// `minicbor` bytes.
///
/// # Errors
///
/// Returns [`CodecError`] if `operation_generation == 0` or encoding fails.
pub fn encode_cancel_request(value: &CancelOperationWire) -> Result<Vec<u8>, CodecError> {
    if value.operation_generation == 0 {
        return Err(CodecError::ZeroGeneration("operation"));
    }
    let mut encoder = Encoder::new(Vec::new());
    encoder
        .map(6)
        .and_then(|e| e.u8(0))
        .and_then(|e| e.str(PAYLOAD_SCHEMA_NAME))
        .and_then(|e| e.u8(1))
        .and_then(|e| e.u32(PAYLOAD_SCHEMA_MAJOR))
        .and_then(|e| e.u8(2))
        .and_then(|e| e.bytes(&value.operation_id))
        .and_then(|e| e.u8(3))
        .and_then(|e| e.u64(value.operation_generation))
        .and_then(|e| e.u8(4))
        .and_then(|e| e.u64(value.expected_cancel_epoch))
        .and_then(|e| e.u8(5))
        .and_then(|e| e.bytes(&value.no_effect_receipt_id))
        .map_err(|err| CodecError::Encoding(err.to_string()))?;
    finish_cbor(encoder)
}

/// Decodes a [`CancelOperationWire`] from deterministic `nlos.driver-mock.payload`
/// `minicbor` bytes.
///
/// # Errors
///
/// Fails closed on any field-count, schema, key-order, width, zero-generation,
/// or trailing-byte violation.
pub fn decode_cancel_request(bytes: &[u8]) -> Result<CancelOperationWire, CodecError> {
    if bytes.len() > MAX_DRIVER_MOCK_PAYLOAD_BYTES {
        return Err(CodecError::Malformed(
            "payload exceeds MAX_DRIVER_MOCK_PAYLOAD_BYTES".to_owned(),
        ));
    }
    let mut decoder = Decoder::new(bytes);
    expect_cbor_map_len(&mut decoder, 6)?;
    expect_cbor_schema(&mut decoder)?;
    let operation_id = expect_fixed_id(&mut decoder, 2, "operation_id")?;
    let operation_generation = expect_nonzero_u64(&mut decoder, 3, "operation")?;
    let expected_cancel_epoch = expect_u64(&mut decoder, 4)?;
    let no_effect_receipt_id = expect_fixed_id(&mut decoder, 5, "no_effect_receipt_id")?;
    expect_cbor_end(&decoder, bytes.len())?;
    Ok(CancelOperationWire {
        operation_id,
        operation_generation,
        expected_cancel_epoch,
        no_effect_receipt_id,
    })
}

/// Encodes a [`CancelOperationResultWire`] into deterministic
/// `nlos.driver-mock.payload` `minicbor` bytes.
///
/// # Errors
///
/// Returns [`CodecError`] if `operation_generation == 0`, `state_code` is unknown,
/// or terminal receipt presence disagrees with `state_code`.
pub fn encode_cancel_result(value: &CancelOperationResultWire) -> Result<Vec<u8>, CodecError> {
    if value.operation_generation == 0 {
        return Err(CodecError::ZeroGeneration("operation"));
    }
    let parsed_state = state_from_wire_code(value.state_code)?;
    if parsed_state.is_terminal() != value.terminal_receipt_id.is_some() {
        return Err(CodecError::Malformed(
            "cancel result terminal_receipt_id disagrees with state_code terminality".to_owned(),
        ));
    }
    let receipt_bytes: &[u8] = value
        .terminal_receipt_id
        .as_ref()
        .map_or(&[], |bytes| bytes.as_slice());

    let mut encoder = Encoder::new(Vec::new());
    encoder
        .map(9)
        .and_then(|e| e.u8(0))
        .and_then(|e| e.str(PAYLOAD_SCHEMA_NAME))
        .and_then(|e| e.u8(1))
        .and_then(|e| e.u32(PAYLOAD_SCHEMA_MAJOR))
        .and_then(|e| e.u8(2))
        .and_then(|e| e.bool(value.replayed))
        .and_then(|e| e.u8(3))
        .and_then(|e| e.bool(value.already_terminal))
        .and_then(|e| e.u8(4))
        .and_then(|e| e.bytes(&value.operation_id))
        .and_then(|e| e.u8(5))
        .and_then(|e| e.u64(value.operation_generation))
        .and_then(|e| e.u8(6))
        .and_then(|e| e.u8(value.state_code))
        .and_then(|e| e.u8(7))
        .and_then(|e| e.u64(value.cancel_epoch))
        .and_then(|e| e.u8(8))
        .and_then(|e| e.bytes(receipt_bytes))
        .map_err(|err| CodecError::Encoding(err.to_string()))?;
    finish_cbor(encoder)
}

/// Decodes a [`CancelOperationResultWire`] from deterministic
/// `nlos.driver-mock.payload` `minicbor` bytes.
///
/// # Errors
///
/// Fails closed on any field-count, schema, key-order, width, state-code,
/// zero-generation, or trailing-byte violation.
pub fn decode_cancel_result(bytes: &[u8]) -> Result<CancelOperationResultWire, CodecError> {
    if bytes.len() > MAX_DRIVER_MOCK_PAYLOAD_BYTES {
        return Err(CodecError::Malformed(
            "payload exceeds MAX_DRIVER_MOCK_PAYLOAD_BYTES".to_owned(),
        ));
    }
    let mut decoder = Decoder::new(bytes);
    expect_cbor_map_len(&mut decoder, 9)?;
    expect_cbor_schema(&mut decoder)?;
    let replayed = expect_bool(&mut decoder, 2)?;
    let already_terminal = expect_bool(&mut decoder, 3)?;
    let operation_id = expect_fixed_id(&mut decoder, 4, "operation_id")?;
    let operation_generation = expect_nonzero_u64(&mut decoder, 5, "operation")?;
    let state_code = expect_u8(&mut decoder, 6)?;
    let parsed_state = state_from_wire_code(state_code)?;
    let cancel_epoch = expect_u64(&mut decoder, 7)?;
    let terminal_receipt_id = expect_optional_fixed_id(&mut decoder, 8, "terminal_receipt_id")?;
    if parsed_state.is_terminal() != terminal_receipt_id.is_some() {
        return Err(CodecError::Malformed(
            "cancel result terminal_receipt_id disagrees with state_code terminality".to_owned(),
        ));
    }
    expect_cbor_end(&decoder, bytes.len())?;
    Ok(CancelOperationResultWire {
        replayed,
        already_terminal,
        operation_id,
        operation_generation,
        state_code,
        cancel_epoch,
        terminal_receipt_id,
    })
}

/// Typed union over the four Stage B legacy `minicbor` driver request frames.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LegacyMinicborRequest {
    Register(RegisterOperationWire),
    Dispatch(DispatchOperationWire),
    Complete(CompleteOperationWire),
    Cancel(CancelOperationWire),
}

impl LegacyMinicborRequest {
    /// Canonical `nlos.sabi.DriverProvider` method name corresponding to this request.
    #[must_use]
    pub const fn method(&self) -> &'static str {
        match self {
            Self::Register(_) => REGISTER_PROVIDER_METHOD,
            Self::Dispatch(_) => DISPATCH_PROVIDER_METHOD,
            Self::Complete(_) => COMPLETE_PROVIDER_METHOD,
            Self::Cancel(_) => CANCEL_PROVIDER_METHOD,
        }
    }

    /// Encodes this request into deterministic Stage B `minicbor` bytes.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError`] if deterministic encoding or generation checks fail.
    pub fn encode(&self) -> Result<Vec<u8>, CodecError> {
        match self {
            Self::Register(wire) => encode_register_request(wire),
            Self::Dispatch(wire) => encode_dispatch_request(wire),
            Self::Complete(wire) => encode_complete_request(wire),
            Self::Cancel(wire) => encode_cancel_request(wire),
        }
    }

    /// Decodes a Stage B `minicbor` request frame for `method`.
    ///
    /// # Errors
    ///
    /// Returns [`DriverShimError`] on unknown method or malformed `minicbor` bytes.
    pub fn decode(method: &str, wire: &[u8]) -> Result<Self, DriverShimError> {
        match method {
            REGISTER_PROVIDER_METHOD => Ok(Self::Register(
                decode_register_request(wire).map_err(DriverShimError::LegacyCbor)?,
            )),
            DISPATCH_PROVIDER_METHOD => Ok(Self::Dispatch(
                decode_dispatch_request(wire).map_err(DriverShimError::LegacyCbor)?,
            )),
            COMPLETE_PROVIDER_METHOD => Ok(Self::Complete(
                decode_complete_request(wire).map_err(DriverShimError::LegacyCbor)?,
            )),
            CANCEL_PROVIDER_METHOD => Ok(Self::Cancel(
                decode_cancel_request(wire).map_err(DriverShimError::LegacyCbor)?,
            )),
            other => Err(DriverShimError::UnsupportedLegacyMethod(other.to_owned())),
        }
    }
}

/// Typed union over the four Stage B legacy `minicbor` driver response frames.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LegacyMinicborResponse {
    RegisterResult(RegisterOperationResultWire),
    DispatchResult(DispatchOperationResultWire),
    CompleteResult(CompleteOperationResultWire),
    CancelResult(CancelOperationResultWire),
}

impl LegacyMinicborResponse {
    /// Canonical `nlos.sabi.DriverProvider` method name corresponding to this response.
    #[must_use]
    pub const fn method(&self) -> &'static str {
        match self {
            Self::RegisterResult(_) => REGISTER_PROVIDER_METHOD,
            Self::DispatchResult(_) => DISPATCH_PROVIDER_METHOD,
            Self::CompleteResult(_) => COMPLETE_PROVIDER_METHOD,
            Self::CancelResult(_) => CANCEL_PROVIDER_METHOD,
        }
    }

    /// Encodes this response into deterministic Stage B `minicbor` bytes.
    ///
    /// # Errors
    ///
    /// Returns [`CodecError`] if deterministic encoding fails.
    pub fn encode(&self) -> Result<Vec<u8>, CodecError> {
        match self {
            Self::RegisterResult(wire) => encode_register_result(wire),
            Self::DispatchResult(wire) => encode_dispatch_result(wire),
            Self::CompleteResult(wire) => encode_complete_result(wire),
            Self::CancelResult(wire) => encode_cancel_result(wire),
        }
    }

    /// Decodes a Stage B `minicbor` response frame for `method`.
    ///
    /// # Errors
    ///
    /// Returns [`DriverShimError`] on unknown method or malformed `minicbor` bytes.
    pub fn decode(method: &str, wire: &[u8]) -> Result<Self, DriverShimError> {
        match method {
            REGISTER_PROVIDER_METHOD => Ok(Self::RegisterResult(
                decode_register_result(wire).map_err(DriverShimError::LegacyCbor)?,
            )),
            DISPATCH_PROVIDER_METHOD => Ok(Self::DispatchResult(
                decode_dispatch_result(wire).map_err(DriverShimError::LegacyCbor)?,
            )),
            COMPLETE_PROVIDER_METHOD => Ok(Self::CompleteResult(
                decode_complete_result(wire).map_err(DriverShimError::LegacyCbor)?,
            )),
            CANCEL_PROVIDER_METHOD => Ok(Self::CancelResult(
                decode_cancel_result(wire).map_err(DriverShimError::LegacyCbor)?,
            )),
            other => Err(DriverShimError::UnsupportedLegacyMethod(other.to_owned())),
        }
    }
}

/// Optional context for computing deterministic worker digests and cancellation
/// wake/reconciliation markers when bridging a legacy `CompleteOperationResultWire`
/// into a frozen `DriverProviderCompleteAck`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LegacyCompleteBridgeContext {
    pub handle: OperationHandle,
    pub callback_id: CallbackId,
    pub seed: [u8; DIGEST_BYTES],
    pub fenced_reconciliation_only: bool,
}

/// Operating mode of [`DriverProtocolShim`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DriverShimMode {
    /// Bridges Stage B legacy `nlos.driver-mock.payload` `minicbor` frames <->
    /// frozen `ADR-0024` `nlos.sabi.DriverProvider` Protobuf at `host_epoch`.
    LegacyStageBMinicbor { host_epoch: u32 },
    /// Bridges historical frozen `nlos.sabi.DriverProvider` Protobuf at
    /// `driver_epoch` (`N-1` / `N-2`) <-> `host_epoch` (`N`).
    HistoricalFrozenEpoch { driver_epoch: u32, host_epoch: u32 },
}

/// Trust ceiling enforced on `[TASK-PROVIDER-001]` isolation attestations
/// passing through [`DriverProtocolShim`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProviderTrustCeiling {
    /// Forces `PROVIDER_ISOLATION_UNVERIFIED_TAG` on every bridged completion or
    /// isolation report (mandatory for legacy `minicbor` and untrusted adapters).
    ForceUnverified,
    /// Permits `PROVIDER_ISOLATION_VERIFIED_TAG` only if the wire attestation
    /// already carries all 5 `[TASK-PROVIDER-001]` guarantees and a non-zero
    /// 32-byte digest; never synthesizes or elevates missing guarantees.
    PreserveHonestAttestation,
}

/// Fail-closed errors returned by [`DriverProtocolShim`].
#[derive(Debug)]
pub enum DriverShimError {
    /// Legacy `minicbor` codec violation.
    LegacyCbor(CodecError),
    /// Frozen `nlos.sabi.DriverProvider` Protobuf codec violation.
    FrozenProto(ProtoCodecError),
    /// Method is not supported on Stage B `minicbor` wire (`query_isolation` has no `minicbor` request).
    UnsupportedLegacyMethod(String),
    /// `OperationState` and `CompletionOutcome` codes disagreed on a `CompleteResult`.
    StateOutcomeMismatch { state_code: u8, outcome_code: u8 },
    /// Generation field was `0`.
    ZeroGeneration(&'static str),
    /// Attempted to forge a non-zero attestation digest or elevate an `UNVERIFIED`
    /// / legacy `minicbor` provider to `PROVIDER_ISOLATION_VERIFIED_TAG`.
    ForbiddenIsolationElevation(&'static str),
    /// Invalid `DriverProvider` epoch metadata.
    InvalidDriverEpoch { expected: u32, actual: u32 },
}

impl fmt::Display for DriverShimError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LegacyCbor(err) => write!(formatter, "legacy minicbor driver error: {err}"),
            Self::FrozenProto(err) => write!(formatter, "frozen DriverProvider error: {err}"),
            Self::UnsupportedLegacyMethod(method) => {
                write!(
                    formatter,
                    "unsupported legacy minicbor driver method {method:?}"
                )
            }
            Self::StateOutcomeMismatch {
                state_code,
                outcome_code,
            } => write!(
                formatter,
                "legacy completion state_code {state_code} disagrees with outcome_code {outcome_code}"
            ),
            Self::ZeroGeneration(field) => {
                write!(
                    formatter,
                    "driver field {field} generation must be non-zero"
                )
            }
            Self::ForbiddenIsolationElevation(reason) => write!(
                formatter,
                "forbidden provider isolation elevation attempt: {reason}"
            ),
            Self::InvalidDriverEpoch { expected, actual } => write!(
                formatter,
                "DriverProvider schema major {actual} did not match expected epoch {expected}"
            ),
        }
    }
}

impl Error for DriverShimError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::LegacyCbor(err) => Some(err),
            Self::FrozenProto(err) => Some(err),
            Self::UnsupportedLegacyMethod(_)
            | Self::StateOutcomeMismatch { .. }
            | Self::ZeroGeneration(_)
            | Self::ForbiddenIsolationElevation(_)
            | Self::InvalidDriverEpoch { .. } => None,
        }
    }
}

/// Synthesizes the canonical honest `[TASK-PROVIDER-001]` unverified attestation
/// attached to Stage B legacy `minicbor` responses.
#[must_use]
pub fn legacy_unverified_isolation_attestation() -> ProviderIsolationAttestation {
    ProviderIsolationAttestation {
        tenant_isolated: false,
        session_ephemeral: false,
        cache_partitioned: false,
        zero_data_retention: false,
        attestation_digest: [0_u8; DIGEST_BYTES],
        provider_profile: LEGACY_MINICBOR_UNVERIFIED_PROFILE.to_owned(),
    }
}

/// Out-of-core unprivileged Driver Protocol Shim (`[DRV-PROTO-001]`, `ADR-0027`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DriverProtocolShim {
    mode: DriverShimMode,
    trust_ceiling: ProviderTrustCeiling,
}

impl DriverProtocolShim {
    /// Constructs a shim bridging Stage B legacy `minicbor` frames and frozen
    /// `ADR-0024` `nlos.sabi.DriverProvider` Protobuf frames.
    ///
    /// Legacy `minicbor` adapters always operate under
    /// [`ProviderTrustCeiling::ForceUnverified`].
    ///
    /// # Errors
    ///
    /// Fails closed if `host_epoch == 0`.
    pub const fn new_legacy_minicbor(host_epoch: u32) -> Result<Self, DriverShimError> {
        if host_epoch == 0 {
            return Err(DriverShimError::InvalidDriverEpoch {
                expected: 1,
                actual: 0,
            });
        }
        Ok(Self {
            mode: DriverShimMode::LegacyStageBMinicbor { host_epoch },
            trust_ceiling: ProviderTrustCeiling::ForceUnverified,
        })
    }

    /// Constructs a shim bridging historical frozen `DriverProvider` epoch
    /// `driver_epoch` (`N-1` / `N-2`) <-> `host_epoch` (`N`).
    ///
    /// # Errors
    ///
    /// Fails closed if `driver_epoch == 0`, `host_epoch == 0`, or
    /// `driver_epoch >= host_epoch`.
    pub const fn new_historical_frozen_epoch(
        driver_epoch: u32,
        host_epoch: u32,
        trust_ceiling: ProviderTrustCeiling,
    ) -> Result<Self, DriverShimError> {
        if driver_epoch == 0 || host_epoch == 0 || driver_epoch >= host_epoch {
            return Err(DriverShimError::InvalidDriverEpoch {
                expected: host_epoch,
                actual: driver_epoch,
            });
        }
        Ok(Self {
            mode: DriverShimMode::HistoricalFrozenEpoch {
                driver_epoch,
                host_epoch,
            },
            trust_ceiling,
        })
    }

    /// Active protocol bridge mode.
    #[must_use]
    pub const fn mode(&self) -> DriverShimMode {
        self.mode
    }

    /// Active `[TASK-PROVIDER-001]` isolation trust ceiling.
    #[must_use]
    pub const fn trust_ceiling(&self) -> ProviderTrustCeiling {
        self.trust_ceiling
    }

    ///Canonical SABI service name (`"driver_provider"`).
    #[must_use]
    pub const fn service_name() -> &'static str {
        DRIVER_PROVIDER_SERVICE
    }

    /// Translates a Stage B legacy `minicbor` request frame for `method` into
    /// frozen `nlos.sabi.DriverProvider` v1.0 Protobuf request bytes.
    ///
    /// # Errors
    ///
    /// Fails closed on unknown method, malformed `minicbor` bytes, or zero
    /// generation fields.
    pub fn legacy_minicbor_request_to_frozen_proto(
        &self,
        method: &str,
        cbor_wire: &[u8],
    ) -> Result<Vec<u8>, DriverShimError> {
        let legacy = LegacyMinicborRequest::decode(method, cbor_wire)?;
        let v1_bytes = match legacy {
            LegacyMinicborRequest::Register(reg) => {
                let spec = OperationSpec {
                    operation_id: OperationId::from_bytes(reg.operation_id),
                    generation: nonzero_generation(reg.operation_generation, "operation")?,
                    owner_fiber: FiberHandle {
                        fiber_id: ExecutionFiberId::from_bytes(reg.owner_fiber_id),
                        generation: nonzero_generation(reg.owner_fiber_generation, "owner_fiber")?,
                    },
                    cancellation_scope_id: CancellationScopeId::from_bytes(
                        reg.cancellation_scope_id,
                    ),
                    cancellation_generation: nonzero_generation(
                        reg.cancellation_generation,
                        "cancellation",
                    )?,
                };
                encode_register_request_proto(&spec).map_err(DriverShimError::FrozenProto)?
            }
            LegacyMinicborRequest::Dispatch(disp) => {
                let dispatch = DispatchProviderOperation {
                    handle: OperationHandle {
                        operation_id: OperationId::from_bytes(disp.operation_id),
                        generation: nonzero_generation(disp.operation_generation, "operation")?,
                    },
                    callback_id: CallbackId::from_bytes(disp.callback_id),
                };
                encode_dispatch_request_proto(&dispatch).map_err(DriverShimError::FrozenProto)?
            }
            LegacyMinicborRequest::Complete(comp) => {
                let complete = CompleteProviderOperation {
                    handle: OperationHandle {
                        operation_id: OperationId::from_bytes(comp.operation_id),
                        generation: nonzero_generation(comp.operation_generation, "operation")?,
                    },
                    callback_id: CallbackId::from_bytes(comp.callback_id),
                    seed: comp.seed,
                };
                encode_complete_request_proto(&complete).map_err(DriverShimError::FrozenProto)?
            }
            LegacyMinicborRequest::Cancel(cancel) => {
                let cancel_op = CancelProviderOperation {
                    handle: OperationHandle {
                        operation_id: OperationId::from_bytes(cancel.operation_id),
                        generation: nonzero_generation(cancel.operation_generation, "operation")?,
                    },
                    expected_cancel_epoch: CancelEpoch::new(cancel.expected_cancel_epoch),
                    no_effect_receipt_id: ReceiptId::from_bytes(cancel.no_effect_receipt_id),
                };
                encode_cancel_request_proto(&cancel_op).map_err(DriverShimError::FrozenProto)?
            }
        };
        stamp_driver_request_epoch(&v1_bytes, self.host_epoch())
    }

    /// Translates frozen `nlos.sabi.DriverProvider` Protobuf request bytes into
    /// `(method, legacy_minicbor_bytes)` for a Stage B legacy `minicbor` driver.
    ///
    /// Note: `query_isolation` has no Stage B `minicbor` wire representation;
    /// use [`Self::answer_query_isolation_for_legacy_driver`] to synthesize the
    /// honest `PROVIDER_ISOLATION_UNVERIFIED` report directly.
    ///
    /// # Errors
    ///
    /// Fails closed on malformed/wrong-epoch Protobuf bytes or if `query_isolation`
    /// is passed to `minicbor` request down-conversion.
    pub fn frozen_proto_request_to_legacy_minicbor(
        &self,
        proto_wire: &[u8],
    ) -> Result<(&'static str, Vec<u8>), DriverShimError> {
        let normalized = normalize_driver_request_to_v1(proto_wire, self.host_epoch())?;
        let decoded = decode_request_proto(&normalized).map_err(DriverShimError::FrozenProto)?;
        let legacy = match decoded {
            DecodedProviderRequest::Register(spec) => {
                LegacyMinicborRequest::Register(RegisterOperationWire {
                    operation_id: spec.operation_id.into_bytes(),
                    operation_generation: spec.generation.get(),
                    owner_fiber_id: spec.owner_fiber.fiber_id.into_bytes(),
                    owner_fiber_generation: spec.owner_fiber.generation.get(),
                    cancellation_scope_id: spec.cancellation_scope_id.into_bytes(),
                    cancellation_generation: spec.cancellation_generation.get(),
                })
            }
            DecodedProviderRequest::Dispatch(disp) => {
                LegacyMinicborRequest::Dispatch(DispatchOperationWire {
                    operation_id: disp.handle.operation_id.into_bytes(),
                    operation_generation: disp.handle.generation.get(),
                    callback_id: disp.callback_id.into_bytes(),
                })
            }
            DecodedProviderRequest::Complete(comp) => {
                LegacyMinicborRequest::Complete(CompleteOperationWire {
                    operation_id: comp.handle.operation_id.into_bytes(),
                    operation_generation: comp.handle.generation.get(),
                    callback_id: comp.callback_id.into_bytes(),
                    seed: comp.seed,
                })
            }
            DecodedProviderRequest::Cancel(cancel) => {
                LegacyMinicborRequest::Cancel(CancelOperationWire {
                    operation_id: cancel.handle.operation_id.into_bytes(),
                    operation_generation: cancel.handle.generation.get(),
                    expected_cancel_epoch: cancel.expected_cancel_epoch.get(),
                    no_effect_receipt_id: cancel.no_effect_receipt_id.into_bytes(),
                })
            }
            DecodedProviderRequest::QueryIsolation => {
                return Err(DriverShimError::UnsupportedLegacyMethod(
                    QUERY_ISOLATION_PROVIDER_METHOD.to_owned(),
                ));
            }
        };
        let method = legacy.method();
        let bytes = legacy.encode().map_err(DriverShimError::LegacyCbor)?;
        Ok((method, bytes))
    }

    /// Synthesizes a frozen `nlos.sabi.DriverProvider` `IsolationReport` response
    /// on behalf of a Stage B legacy `minicbor` driver, strictly tagged as
    /// `PROVIDER_ISOLATION_UNVERIFIED_TAG` (`[TASK-PROVIDER-001]`).
    ///
    /// # Errors
    ///
    /// Returns [`DriverShimError`] only if Protobuf encoding fails.
    pub fn answer_query_isolation_for_legacy_driver(&self) -> Result<Vec<u8>, DriverShimError> {
        let attestation = legacy_unverified_isolation_attestation();
        debug_assert_eq!(
            attestation.evaluate_posture().receipt_marker(),
            PROVIDER_ISOLATION_UNVERIFIED_TAG
        );
        let v1_bytes =
            encode_isolation_report_proto(&attestation).map_err(DriverShimError::FrozenProto)?;
        stamp_driver_response_epoch(&v1_bytes, self.host_epoch())
    }

    /// Translates a Stage B legacy `minicbor` response frame for `method` into
    /// frozen `nlos.sabi.DriverProvider` Protobuf response bytes.
    ///
    /// For `complete`, `complete_context` supplies `(handle, callback_id, seed)`
    /// when available so `compute_digest` matches `derive_worker_compute_digest`;
    /// isolation posture is unconditionally `PROVIDER_ISOLATION_UNVERIFIED_TAG`.
    ///
    /// # Errors
    ///
    /// Fails closed on unknown method, malformed `minicbor` bytes, zero
    /// generations, or disagreed `state_code` / `outcome_code`.
    pub fn legacy_minicbor_response_to_frozen_proto(
        &self,
        method: &str,
        cbor_wire: &[u8],
        complete_context: Option<LegacyCompleteBridgeContext>,
    ) -> Result<Vec<u8>, DriverShimError> {
        let legacy = LegacyMinicborResponse::decode(method, cbor_wire)?;
        let v1_bytes = match legacy {
            LegacyMinicborResponse::RegisterResult(ack) => {
                let outcome = RegisterProviderOutcome {
                    replayed: ack.replayed,
                    handle: OperationHandle {
                        operation_id: OperationId::from_bytes(ack.operation_id),
                        generation: nonzero_generation(ack.operation_generation, "operation")?,
                    },
                    admission_receipt_id: ReceiptId::from_bytes(ack.admission_receipt_id),
                };
                encode_register_ack_proto(&outcome).map_err(DriverShimError::FrozenProto)?
            }
            LegacyMinicborResponse::DispatchResult(ack) => {
                let outcome = DispatchProviderOutcome {
                    replayed: ack.replayed,
                    preparation_receipt_id: ReceiptId::from_bytes(ack.preparation_receipt_id),
                    activation_receipt_id: ReceiptId::from_bytes(ack.activation_receipt_id),
                    ticket: CallbackTicket {
                        callback_id: CallbackId::from_bytes(ack.callback_id),
                        operation: OperationHandle {
                            operation_id: OperationId::from_bytes(ack.operation_id),
                            generation: nonzero_generation(ack.operation_generation, "operation")?,
                        },
                        owner_fiber: FiberHandle {
                            fiber_id: ExecutionFiberId::from_bytes(ack.owner_fiber_id),
                            generation: nonzero_generation(
                                ack.owner_fiber_generation,
                                "owner_fiber",
                            )?,
                        },
                        cancel_epoch: CancelEpoch::new(ack.cancel_epoch),
                    },
                };
                encode_dispatch_ack_proto(&outcome).map_err(DriverShimError::FrozenProto)?
            }
            LegacyMinicborResponse::CompleteResult(ack) => {
                let receipt_id = ReceiptId::from_bytes(ack.receipt_id);
                let outcome_kind = outcome_from_wire_code(ack.outcome_code)
                    .map_err(DriverShimError::LegacyCbor)?;
                let outcome = reconstruct_completion_outcome(outcome_kind, receipt_id);
                let state = reconstruct_terminal_state(ack.state_code, receipt_id)?;
                if state_wire_code(state) != expected_state_code_for_outcome(outcome_kind) {
                    return Err(DriverShimError::StateOutcomeMismatch {
                        state_code: ack.state_code,
                        outcome_code: ack.outcome_code,
                    });
                }
                let (compute_digest, fenced_reconciliation_only) =
                    complete_context.map_or(([0_u8; DIGEST_BYTES], false), |ctx| {
                        (
                            derive_worker_compute_digest(ctx.handle, ctx.callback_id, &ctx.seed),
                            ctx.fenced_reconciliation_only,
                        )
                    });
                let attestation = legacy_unverified_isolation_attestation();
                debug_assert!(!attestation.evaluate_posture().is_verified());
                let provider_outcome = CompleteProviderOutcome {
                    replayed: ack.replayed,
                    outcome,
                    state,
                };
                encode_complete_ack_proto(
                    &provider_outcome,
                    compute_digest,
                    fenced_reconciliation_only,
                    &attestation,
                )
                .map_err(DriverShimError::FrozenProto)?
            }
            LegacyMinicborResponse::CancelResult(ack) => {
                let state = reconstruct_any_state(ack.state_code, ack.terminal_receipt_id)?;
                let cancel_outcome = CancelProviderOutcome {
                    replayed: ack.replayed,
                    already_terminal: ack.already_terminal,
                    handle: OperationHandle {
                        operation_id: OperationId::from_bytes(ack.operation_id),
                        generation: nonzero_generation(ack.operation_generation, "operation")?,
                    },
                    state,
                    cancel_epoch: CancelEpoch::new(ack.cancel_epoch),
                };
                encode_cancel_ack_proto(&cancel_outcome).map_err(DriverShimError::FrozenProto)?
            }
        };
        stamp_driver_response_epoch(&v1_bytes, self.host_epoch())
    }

    /// Rejects any attempt to attach a claimed verified attestation or non-zero
    /// attestation digest onto a legacy `minicbor` completion response
    /// (`[TASK-PROVIDER-001]` / `ADR-0024` isolation honesty guard).
    ///
    /// # Errors
    ///
    /// Fails closed with [`DriverShimError::ForbiddenIsolationElevation`] whenever
    /// `claimed_attestation` claims any verified boolean guarantee or a non-zero
    /// `attestation_digest` under `LegacyStageBMinicbor` or `ForceUnverified`.
    pub fn legacy_minicbor_response_with_claimed_attestation(
        &self,
        method: &str,
        cbor_wire: &[u8],
        complete_context: Option<LegacyCompleteBridgeContext>,
        claimed_attestation: &ProviderIsolationAttestation,
    ) -> Result<Vec<u8>, DriverShimError> {
        let force_unverified = matches!(self.mode, DriverShimMode::LegacyStageBMinicbor { .. })
            || self.trust_ceiling == ProviderTrustCeiling::ForceUnverified;
        let claims_verified = claimed_attestation.tenant_isolated
            || claimed_attestation.session_ephemeral
            || claimed_attestation.cache_partitioned
            || claimed_attestation.zero_data_retention
            || claimed_attestation.attestation_digest != [0_u8; DIGEST_BYTES]
            || claimed_attestation.evaluate_posture().receipt_marker()
                == PROVIDER_ISOLATION_VERIFIED_TAG;
        if force_unverified && claims_verified {
            return Err(DriverShimError::ForbiddenIsolationElevation(
                "DriverProtocolShim cannot forge attestation digests or elevate legacy/untrusted providers to VERIFIED",
            ));
        }
        self.legacy_minicbor_response_to_frozen_proto(method, cbor_wire, complete_context)
    }

    /// Translates frozen `nlos.sabi.DriverProvider` Protobuf response bytes into
    /// `(method, legacy_minicbor_bytes)` for a Stage B legacy `minicbor` caller.
    ///
    /// # Errors
    ///
    /// Fails closed on malformed/wrong-epoch Protobuf bytes or if an
    /// `IsolationReport` (which has no `minicbor` wire arm) is supplied.
    pub fn frozen_proto_response_to_legacy_minicbor(
        &self,
        proto_wire: &[u8],
    ) -> Result<(&'static str, Vec<u8>), DriverShimError> {
        let normalized = normalize_driver_response_to_v1(proto_wire, self.host_epoch())?;
        let decoded = decode_response_proto(&normalized).map_err(DriverShimError::FrozenProto)?;
        let legacy = match decoded {
            DecodedProviderResponse::RegisterAck(ack) => {
                LegacyMinicborResponse::RegisterResult(RegisterOperationResultWire {
                    replayed: ack.replayed,
                    operation_id: ack.handle.operation_id.into_bytes(),
                    operation_generation: ack.handle.generation.get(),
                    admission_receipt_id: ack.admission_receipt_id.into_bytes(),
                })
            }
            DecodedProviderResponse::DispatchAck(ack) => {
                LegacyMinicborResponse::DispatchResult(DispatchOperationResultWire {
                    replayed: ack.replayed,
                    preparation_receipt_id: ack.preparation_receipt_id.into_bytes(),
                    activation_receipt_id: ack.activation_receipt_id.into_bytes(),
                    callback_id: ack.ticket.callback_id.into_bytes(),
                    operation_id: ack.ticket.operation.operation_id.into_bytes(),
                    operation_generation: ack.ticket.operation.generation.get(),
                    owner_fiber_id: ack.ticket.owner_fiber.fiber_id.into_bytes(),
                    owner_fiber_generation: ack.ticket.owner_fiber.generation.get(),
                    cancel_epoch: ack.ticket.cancel_epoch.get(),
                })
            }
            DecodedProviderResponse::CompleteAck(receipt) => {
                LegacyMinicborResponse::CompleteResult(CompleteOperationResultWire {
                    replayed: receipt.replayed,
                    outcome_code: outcome_wire_code(receipt.outcome),
                    receipt_id: receipt.receipt_id.into_bytes(),
                    state_code: state_wire_code(receipt.state),
                })
            }
            DecodedProviderResponse::CancelAck(ack) => {
                LegacyMinicborResponse::CancelResult(CancelOperationResultWire {
                    replayed: ack.replayed,
                    already_terminal: ack.already_terminal,
                    operation_id: ack.handle.operation_id.into_bytes(),
                    operation_generation: ack.handle.generation.get(),
                    state_code: state_wire_code(ack.state),
                    cancel_epoch: ack.cancel_epoch.get(),
                    terminal_receipt_id: terminal_receipt_of_state(ack.state)
                        .map(ReceiptId::into_bytes),
                })
            }
            DecodedProviderResponse::IsolationReport(_) => {
                return Err(DriverShimError::UnsupportedLegacyMethod(
                    QUERY_ISOLATION_PROVIDER_METHOD.to_owned(),
                ));
            }
        };
        let method = legacy.method();
        let bytes = legacy.encode().map_err(DriverShimError::LegacyCbor)?;
        Ok((method, bytes))
    }

    /// Translates a historical frozen `DriverProviderRequest` (`driver_epoch`)
    /// into `host_epoch` while preserving all identifiers and non-critical
    /// extension IDs.
    ///
    /// # Errors
    ///
    /// Fails closed on malformed Protobuf, mismatched `driver_epoch`, or
    /// unsupported `critical_extension_ids`.
    pub fn translate_historical_proto_request_to_host(
        &self,
        driver_wire: &[u8],
    ) -> Result<Vec<u8>, DriverShimError> {
        let (driver_epoch, host_epoch) = self.historical_epochs()?;
        let normalized = normalize_driver_request_to_v1(driver_wire, driver_epoch)?;
        let _validated = decode_request_proto(&normalized).map_err(DriverShimError::FrozenProto)?;
        stamp_driver_request_epoch(&normalized, host_epoch)
    }

    /// Translates a host-epoch `DriverProviderRequest` (`host_epoch`) down to
    /// `driver_epoch` for an out-of-core historical `DriverProvider` worker.
    ///
    /// # Errors
    ///
    /// Fails closed on malformed Protobuf or mismatched `host_epoch`.
    pub fn translate_host_proto_request_to_historical(
        &self,
        host_wire: &[u8],
    ) -> Result<Vec<u8>, DriverShimError> {
        let (driver_epoch, host_epoch) = self.historical_epochs()?;
        let normalized = normalize_driver_request_to_v1(host_wire, host_epoch)?;
        let _validated = decode_request_proto(&normalized).map_err(DriverShimError::FrozenProto)?;
        stamp_driver_request_epoch(&normalized, driver_epoch)
    }

    /// Translates a historical frozen `DriverProviderResponse` (`driver_epoch`)
    /// into `host_epoch`, strictly enforcing `[TASK-PROVIDER-001]` isolation
    /// honesty (`ForceUnverified` or incomplete attestations can never become
    /// `PROVIDER_ISOLATION_VERIFIED_TAG`).
    ///
    /// # Errors
    ///
    /// Fails closed on malformed Protobuf, mismatched `driver_epoch`, or invalid
    /// response fields.
    pub fn translate_historical_proto_response_to_host(
        &self,
        driver_wire: &[u8],
    ) -> Result<(Vec<u8>, Option<ProviderIsolationPosture>), DriverShimError> {
        let (driver_epoch, host_epoch) = self.historical_epochs()?;
        let normalized = normalize_driver_response_to_v1(driver_wire, driver_epoch)?;
        let decoded = decode_response_proto(&normalized).map_err(DriverShimError::FrozenProto)?;
        let (sanitized_v1, posture) =
            self.enforce_response_isolation_ceiling(decoded, &normalized)?;
        let out = stamp_driver_response_epoch(&sanitized_v1, host_epoch)?;
        Ok((out, posture))
    }

    /// Translates a host-epoch `DriverProviderResponse` (`host_epoch`) down to
    /// `driver_epoch`.
    ///
    /// # Errors
    ///
    /// Fails closed on malformed Protobuf or mismatched `host_epoch`.
    pub fn translate_host_proto_response_to_historical(
        &self,
        host_wire: &[u8],
    ) -> Result<Vec<u8>, DriverShimError> {
        let (driver_epoch, host_epoch) = self.historical_epochs()?;
        let normalized = normalize_driver_response_to_v1(host_wire, host_epoch)?;
        let _validated =
            decode_response_proto(&normalized).map_err(DriverShimError::FrozenProto)?;
        stamp_driver_response_epoch(&normalized, driver_epoch)
    }

    const fn host_epoch(&self) -> u32 {
        match self.mode {
            DriverShimMode::LegacyStageBMinicbor { host_epoch }
            | DriverShimMode::HistoricalFrozenEpoch { host_epoch, .. } => host_epoch,
        }
    }

    const fn historical_epochs(&self) -> Result<(u32, u32), DriverShimError> {
        match self.mode {
            DriverShimMode::HistoricalFrozenEpoch {
                driver_epoch,
                host_epoch,
            } => Ok((driver_epoch, host_epoch)),
            DriverShimMode::LegacyStageBMinicbor { host_epoch } => {
                Err(DriverShimError::InvalidDriverEpoch {
                    expected: host_epoch,
                    actual: 1,
                })
            }
        }
    }

    fn enforce_response_isolation_ceiling(
        &self,
        decoded: DecodedProviderResponse,
        normalized_v1: &[u8],
    ) -> Result<(Vec<u8>, Option<ProviderIsolationPosture>), DriverShimError> {
        let preserved_non_critical = DriverProviderResponse::decode(normalized_v1)
            .ok()
            .and_then(|resp| resp.schema)
            .map(|schema| schema.non_critical_extension_ids)
            .unwrap_or_default();
        match decoded {
            DecodedProviderResponse::CompleteAck(receipt) => {
                let effective_attestation = self.clamp_attestation(&receipt.attestation);
                let posture = effective_attestation.evaluate_posture();
                let outcome = CompleteProviderOutcome {
                    replayed: receipt.replayed,
                    outcome: receipt.outcome,
                    state: receipt.state,
                };
                let reencoded = encode_complete_ack_proto(
                    &outcome,
                    receipt.compute_digest,
                    receipt.fenced_reconciliation_only,
                    &effective_attestation,
                )
                .map_err(DriverShimError::FrozenProto)?;
                Ok((
                    reattach_response_non_critical(&reencoded, &preserved_non_critical)?,
                    Some(posture),
                ))
            }
            DecodedProviderResponse::IsolationReport(attestation) => {
                let effective_attestation = self.clamp_attestation(&attestation);
                let posture = effective_attestation.evaluate_posture();
                let reencoded = encode_isolation_report_proto(&effective_attestation)
                    .map_err(DriverShimError::FrozenProto)?;
                Ok((
                    reattach_response_non_critical(&reencoded, &preserved_non_critical)?,
                    Some(posture),
                ))
            }
            DecodedProviderResponse::RegisterAck(_)
            | DecodedProviderResponse::DispatchAck(_)
            | DecodedProviderResponse::CancelAck(_) => Ok((normalized_v1.to_vec(), None)),
        }
    }

    fn clamp_attestation(
        &self,
        incoming: &ProviderIsolationAttestation,
    ) -> ProviderIsolationAttestation {
        match self.trust_ceiling {
            ProviderTrustCeiling::ForceUnverified => ProviderIsolationAttestation {
                tenant_isolated: false,
                session_ephemeral: false,
                cache_partitioned: false,
                zero_data_retention: false,
                attestation_digest: [0_u8; DIGEST_BYTES],
                provider_profile: format!("{}-forced-unverified", incoming.provider_profile),
            },
            ProviderTrustCeiling::PreserveHonestAttestation => incoming.clone(),
        }
    }
}

fn nonzero_generation(raw: u64, field: &'static str) -> Result<Generation, DriverShimError> {
    NonZeroU64::new(raw)
        .map(Generation::new)
        .ok_or(DriverShimError::ZeroGeneration(field))
}

const fn reconstruct_completion_outcome(
    kind: TerminalOutcomeKind,
    receipt_id: ReceiptId,
) -> CompletionOutcome {
    match kind {
        TerminalOutcomeKind::Completed => CompletionOutcome::Completed { receipt_id },
        TerminalOutcomeKind::Failed => CompletionOutcome::Failed { receipt_id },
        TerminalOutcomeKind::PartialEffect => CompletionOutcome::PartialEffect { receipt_id },
        TerminalOutcomeKind::EffectUnknown => CompletionOutcome::EffectUnknown { receipt_id },
        TerminalOutcomeKind::CancelledBeforeEffect => {
            CompletionOutcome::CancelledBeforeEffect { receipt_id }
        }
    }
}

const fn expected_state_code_for_outcome(kind: TerminalOutcomeKind) -> u8 {
    match kind {
        TerminalOutcomeKind::Completed => STATE_CODE_COMPLETED,
        TerminalOutcomeKind::Failed => STATE_CODE_FAILED,
        TerminalOutcomeKind::PartialEffect => STATE_CODE_PARTIAL_EFFECT,
        TerminalOutcomeKind::EffectUnknown => STATE_CODE_EFFECT_UNKNOWN,
        TerminalOutcomeKind::CancelledBeforeEffect => STATE_CODE_CANCELLED_BEFORE_EFFECT,
    }
}

fn reconstruct_terminal_state(
    state_code: u8,
    receipt_id: ReceiptId,
) -> Result<OperationState, DriverShimError> {
    match state_code {
        STATE_CODE_COMPLETED => Ok(OperationState::Completed { receipt_id }),
        STATE_CODE_FAILED => Ok(OperationState::Failed { receipt_id }),
        STATE_CODE_CANCELLED_BEFORE_EFFECT => {
            Ok(OperationState::CancelledBeforeEffect { receipt_id })
        }
        STATE_CODE_PARTIAL_EFFECT => Ok(OperationState::PartialEffect { receipt_id }),
        STATE_CODE_EFFECT_UNKNOWN => Ok(OperationState::EffectUnknown { receipt_id }),
        STATE_CODE_REGISTERED | STATE_CODE_DISPATCHED | STATE_CODE_CANCEL_REQUESTED => {
            Err(DriverShimError::StateOutcomeMismatch {
                state_code,
                outcome_code: 0,
            })
        }
        other => Err(DriverShimError::LegacyCbor(CodecError::UnknownStateCode(
            other,
        ))),
    }
}

fn reconstruct_any_state(
    state_code: u8,
    terminal_receipt_id: Option<[u8; ID_BYTES]>,
) -> Result<OperationState, DriverShimError> {
    match (state_code, terminal_receipt_id) {
        (STATE_CODE_REGISTERED, None) => Ok(OperationState::Registered),
        (STATE_CODE_DISPATCHED, None) => Ok(OperationState::Dispatched),
        (STATE_CODE_CANCEL_REQUESTED, None) => Ok(OperationState::CancelRequested),
        (STATE_CODE_COMPLETED, Some(raw)) => Ok(OperationState::Completed {
            receipt_id: ReceiptId::from_bytes(raw),
        }),
        (STATE_CODE_FAILED, Some(raw)) => Ok(OperationState::Failed {
            receipt_id: ReceiptId::from_bytes(raw),
        }),
        (STATE_CODE_CANCELLED_BEFORE_EFFECT, Some(raw)) => {
            Ok(OperationState::CancelledBeforeEffect {
                receipt_id: ReceiptId::from_bytes(raw),
            })
        }
        (STATE_CODE_PARTIAL_EFFECT, Some(raw)) => Ok(OperationState::PartialEffect {
            receipt_id: ReceiptId::from_bytes(raw),
        }),
        (STATE_CODE_EFFECT_UNKNOWN, Some(raw)) => Ok(OperationState::EffectUnknown {
            receipt_id: ReceiptId::from_bytes(raw),
        }),
        (
            STATE_CODE_REGISTERED
            | STATE_CODE_DISPATCHED
            | STATE_CODE_CANCEL_REQUESTED
            | STATE_CODE_COMPLETED
            | STATE_CODE_FAILED
            | STATE_CODE_CANCELLED_BEFORE_EFFECT
            | STATE_CODE_PARTIAL_EFFECT
            | STATE_CODE_EFFECT_UNKNOWN,
            _,
        ) => Err(DriverShimError::LegacyCbor(CodecError::Malformed(
            "cancel state_code disagrees with terminal_receipt_id presence".to_owned(),
        ))),
        (other, _) => Err(DriverShimError::LegacyCbor(CodecError::UnknownStateCode(
            other,
        ))),
    }
}

fn reattach_response_non_critical(
    v1_wire: &[u8],
    non_critical_extension_ids: &[u32],
) -> Result<Vec<u8>, DriverShimError> {
    if non_critical_extension_ids.is_empty() {
        return Ok(v1_wire.to_vec());
    }
    let mut resp = DriverProviderResponse::decode(v1_wire).map_err(|err| {
        DriverShimError::FrozenProto(ProtoCodecError::Schema(
            CompatibilityError::MalformedProtobuf(err.to_string()),
        ))
    })?;
    if let Some(schema) = resp.schema.as_mut() {
        schema.non_critical_extension_ids = non_critical_extension_ids.to_vec();
    }
    Ok(resp.encode_to_vec())
}

fn stamp_driver_request_epoch(
    v1_wire: &[u8],
    target_epoch: u32,
) -> Result<Vec<u8>, DriverShimError> {
    if target_epoch == 1 {
        return Ok(v1_wire.to_vec());
    }
    let mut req = DriverProviderRequest::decode(v1_wire).map_err(|err| {
        DriverShimError::FrozenProto(ProtoCodecError::Schema(
            CompatibilityError::MalformedProtobuf(err.to_string()),
        ))
    })?;
    if let Some(schema) = req.schema.as_mut() {
        schema.major = target_epoch;
    }
    let out = req.encode_to_vec();
    if out.len() > MAX_DRIVER_PROVIDER_PAYLOAD_BYTES {
        return Err(DriverShimError::FrozenProto(ProtoCodecError::Schema(
            CompatibilityError::FrameTooLarge {
                actual: out.len(),
                maximum: MAX_DRIVER_PROVIDER_PAYLOAD_BYTES,
            },
        )));
    }
    Ok(out)
}

fn stamp_driver_response_epoch(
    v1_wire: &[u8],
    target_epoch: u32,
) -> Result<Vec<u8>, DriverShimError> {
    if target_epoch == 1 {
        return Ok(v1_wire.to_vec());
    }
    let mut resp = DriverProviderResponse::decode(v1_wire).map_err(|err| {
        DriverShimError::FrozenProto(ProtoCodecError::Schema(
            CompatibilityError::MalformedProtobuf(err.to_string()),
        ))
    })?;
    if let Some(schema) = resp.schema.as_mut() {
        schema.major = target_epoch;
    }
    let out = resp.encode_to_vec();
    if out.len() > MAX_DRIVER_PROVIDER_PAYLOAD_BYTES {
        return Err(DriverShimError::FrozenProto(ProtoCodecError::Schema(
            CompatibilityError::FrameTooLarge {
                actual: out.len(),
                maximum: MAX_DRIVER_PROVIDER_PAYLOAD_BYTES,
            },
        )));
    }
    Ok(out)
}

fn normalize_driver_request_to_v1(
    wire: &[u8],
    expected_epoch: u32,
) -> Result<Vec<u8>, DriverShimError> {
    if wire.len() > MAX_DRIVER_PROVIDER_PAYLOAD_BYTES {
        return Err(DriverShimError::FrozenProto(ProtoCodecError::Schema(
            CompatibilityError::FrameTooLarge {
                actual: wire.len(),
                maximum: MAX_DRIVER_PROVIDER_PAYLOAD_BYTES,
            },
        )));
    }
    let mut req = DriverProviderRequest::decode(wire).map_err(|err| {
        DriverShimError::FrozenProto(ProtoCodecError::Schema(
            CompatibilityError::MalformedProtobuf(err.to_string()),
        ))
    })?;
    let schema =
        req.schema
            .as_mut()
            .ok_or(DriverShimError::FrozenProto(ProtoCodecError::Schema(
                CompatibilityError::MissingSchemaIdentity,
            )))?;
    if schema.name != SABI_DRIVER_PROVIDER_SCHEMA {
        return Err(DriverShimError::FrozenProto(ProtoCodecError::Schema(
            CompatibilityError::UnknownSchema(schema.name.clone()),
        )));
    }
    if schema.major != expected_epoch {
        return Err(DriverShimError::InvalidDriverEpoch {
            expected: expected_epoch,
            actual: schema.major,
        });
    }
    if schema.critical_extension_ids.len() > MAX_EXTENSION_IDS
        || schema.non_critical_extension_ids.len() > MAX_EXTENSION_IDS
    {
        return Err(DriverShimError::FrozenProto(ProtoCodecError::Schema(
            CompatibilityError::TooManyExtensionIds {
                schema: schema.name.clone(),
                actual: schema
                    .critical_extension_ids
                    .len()
                    .max(schema.non_critical_extension_ids.len()),
                maximum: MAX_EXTENSION_IDS,
            },
        )));
    }
    if let Some(&ext_id) = schema.critical_extension_ids.first() {
        return Err(DriverShimError::FrozenProto(ProtoCodecError::Schema(
            CompatibilityError::UnsupportedCriticalExtension {
                schema: schema.name.clone(),
                extension_id: ext_id,
            },
        )));
    }
    let saved_non_critical = std::mem::take(&mut schema.non_critical_extension_ids);
    schema.major = 1;
    let mut normalized = req.encode_to_vec();
    let _ = decode_request_proto(&normalized).map_err(DriverShimError::FrozenProto)?;
    if !saved_non_critical.is_empty() {
        if let Some(s) = req.schema.as_mut() {
            s.non_critical_extension_ids = saved_non_critical;
        }
        normalized = req.encode_to_vec();
    }
    Ok(normalized)
}

fn normalize_driver_response_to_v1(
    wire: &[u8],
    expected_epoch: u32,
) -> Result<Vec<u8>, DriverShimError> {
    if wire.len() > MAX_DRIVER_PROVIDER_PAYLOAD_BYTES {
        return Err(DriverShimError::FrozenProto(ProtoCodecError::Schema(
            CompatibilityError::FrameTooLarge {
                actual: wire.len(),
                maximum: MAX_DRIVER_PROVIDER_PAYLOAD_BYTES,
            },
        )));
    }
    let mut resp = DriverProviderResponse::decode(wire).map_err(|err| {
        DriverShimError::FrozenProto(ProtoCodecError::Schema(
            CompatibilityError::MalformedProtobuf(err.to_string()),
        ))
    })?;
    let schema =
        resp.schema
            .as_mut()
            .ok_or(DriverShimError::FrozenProto(ProtoCodecError::Schema(
                CompatibilityError::MissingSchemaIdentity,
            )))?;
    if schema.name != SABI_DRIVER_PROVIDER_SCHEMA {
        return Err(DriverShimError::FrozenProto(ProtoCodecError::Schema(
            CompatibilityError::UnknownSchema(schema.name.clone()),
        )));
    }
    if schema.major != expected_epoch {
        return Err(DriverShimError::InvalidDriverEpoch {
            expected: expected_epoch,
            actual: schema.major,
        });
    }
    if schema.critical_extension_ids.len() > MAX_EXTENSION_IDS
        || schema.non_critical_extension_ids.len() > MAX_EXTENSION_IDS
    {
        return Err(DriverShimError::FrozenProto(ProtoCodecError::Schema(
            CompatibilityError::TooManyExtensionIds {
                schema: schema.name.clone(),
                actual: schema
                    .critical_extension_ids
                    .len()
                    .max(schema.non_critical_extension_ids.len()),
                maximum: MAX_EXTENSION_IDS,
            },
        )));
    }
    if let Some(&ext_id) = schema.critical_extension_ids.first() {
        return Err(DriverShimError::FrozenProto(ProtoCodecError::Schema(
            CompatibilityError::UnsupportedCriticalExtension {
                schema: schema.name.clone(),
                extension_id: ext_id,
            },
        )));
    }
    schema.major = 1;
    Ok(resp.encode_to_vec())
}

fn finish_cbor(encoder: Encoder<Vec<u8>>) -> Result<Vec<u8>, CodecError> {
    let bytes = encoder.into_writer();
    if bytes.len() > MAX_DRIVER_MOCK_PAYLOAD_BYTES {
        return Err(CodecError::Encoding(
            "encoded payload exceeds MAX_DRIVER_MOCK_PAYLOAD_BYTES".to_owned(),
        ));
    }
    Ok(bytes)
}

fn expect_cbor_map_len(decoder: &mut Decoder<'_>, expected: u64) -> Result<(), CodecError> {
    let actual = decoder
        .map()
        .map_err(|err| CodecError::Malformed(err.to_string()))?
        .ok_or_else(|| {
            CodecError::Malformed("indefinite-length CBOR map is forbidden".to_owned())
        })?;
    if actual != expected {
        return Err(CodecError::WrongFieldCount { actual, expected });
    }
    Ok(())
}

fn expect_cbor_key(decoder: &mut Decoder<'_>, expected_key: u8) -> Result<(), CodecError> {
    let key = decoder
        .u64()
        .map_err(|err| CodecError::Malformed(err.to_string()))?;
    let expected_u64 = u64::from(expected_key);
    if key < expected_u64 {
        return Err(CodecError::KeyOutOfOrder);
    }
    if key > expected_u64 {
        return Err(CodecError::UnknownFieldKey(key));
    }
    Ok(())
}

fn expect_cbor_schema(decoder: &mut Decoder<'_>) -> Result<(), CodecError> {
    expect_cbor_key(decoder, 0)?;
    let name = decoder
        .str()
        .map_err(|err| CodecError::Malformed(err.to_string()))?;
    if name != PAYLOAD_SCHEMA_NAME {
        return Err(CodecError::WrongSchemaName);
    }
    expect_cbor_key(decoder, 1)?;
    let major = decoder
        .u32()
        .map_err(|err| CodecError::Malformed(err.to_string()))?;
    if major != PAYLOAD_SCHEMA_MAJOR {
        return Err(CodecError::UnsupportedMajor {
            actual: major,
            supported: PAYLOAD_SCHEMA_MAJOR,
        });
    }
    Ok(())
}

fn expect_fixed_id(
    decoder: &mut Decoder<'_>,
    expected_key: u8,
    field: &'static str,
) -> Result<[u8; ID_BYTES], CodecError> {
    expect_cbor_key(decoder, expected_key)?;
    let slice = decoder
        .bytes()
        .map_err(|err| CodecError::Malformed(err.to_string()))?;
    slice.try_into().map_err(|_| CodecError::InvalidLength {
        field,
        actual: slice.len(),
    })
}

fn expect_optional_fixed_id(
    decoder: &mut Decoder<'_>,
    expected_key: u8,
    field: &'static str,
) -> Result<Option<[u8; ID_BYTES]>, CodecError> {
    expect_cbor_key(decoder, expected_key)?;
    let slice = decoder
        .bytes()
        .map_err(|err| CodecError::Malformed(err.to_string()))?;
    if slice.is_empty() {
        return Ok(None);
    }
    let id: [u8; ID_BYTES] = slice.try_into().map_err(|_| CodecError::InvalidLength {
        field,
        actual: slice.len(),
    })?;
    Ok(Some(id))
}

fn expect_nonzero_u64(
    decoder: &mut Decoder<'_>,
    expected_key: u8,
    field: &'static str,
) -> Result<u64, CodecError> {
    let val = expect_u64(decoder, expected_key)?;
    if val == 0 {
        return Err(CodecError::ZeroGeneration(field));
    }
    Ok(val)
}

fn expect_u64(decoder: &mut Decoder<'_>, expected_key: u8) -> Result<u64, CodecError> {
    expect_cbor_key(decoder, expected_key)?;
    decoder
        .u64()
        .map_err(|err| CodecError::Malformed(err.to_string()))
}

fn expect_u8(decoder: &mut Decoder<'_>, expected_key: u8) -> Result<u8, CodecError> {
    expect_cbor_key(decoder, expected_key)?;
    decoder
        .u8()
        .map_err(|err| CodecError::Malformed(err.to_string()))
}

fn expect_bool(decoder: &mut Decoder<'_>, expected_key: u8) -> Result<bool, CodecError> {
    expect_cbor_key(decoder, expected_key)?;
    decoder
        .bool()
        .map_err(|err| CodecError::Malformed(err.to_string()))
}

fn expect_cbor_end(decoder: &Decoder<'_>, total_len: usize) -> Result<(), CodecError> {
    if decoder.position() != total_len {
        return Err(CodecError::TrailingBytes);
    }
    Ok(())
}

#[allow(dead_code)]
const fn _assert_provider_execution_receipt_send_sync() {
    const fn check<T: Send + Sync>() {}
    check::<ProviderExecutionReceipt>();
}
