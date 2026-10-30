//! `[DRV-SHIM-001]` Dual-boundary driver protocol compatibility architecture:
//! - Outer untrusted [`DriverProtocolCompatShim`]: pure, stateless, unprivileged
//!   wire translation between legacy Stage B `minicbor` payloads and frozen
//!   `nlos.sabi.DriverProvider` Protobuf frames, plus cross-epoch `N-1 / N-2 <-> N`
//!   `DriverProvider` translation. Physically holds zero secrets, zero signing
//!   keys, zero `Capability` issuers, and zero `ResourceAuthority` handles.
//! - Controlled TCB edge [`TrustedDriverEnforcementGate`] (re-exported from
//!   `enforcement`): custodies redacted credential references (`DRV-CRED-001`),
//!   verifies `Capability` + `authorization_digest` (`SABI-AUTH-001`), enforces
//!   one-shot `ReservationActivationToken` consumption (`DRV-ACTIVE-001`),
//!   fencing/incarnation/deadline bounds (`DRV-BOUND-001`), `[TASK-PROVIDER-001]`
//!   isolation posture, and crash-to-`UNCERTAIN` convergence (`DRV-LIFE-001`).

use std::error::Error;
use std::fmt;

pub use nlos_driver_mock::codec::{
    CodecError, CompleteOperationResultWire, CompleteOperationWire, DispatchOperationResultWire,
    DispatchOperationWire, OUTCOME_CODE_CANCELLED_BEFORE_EFFECT, OUTCOME_CODE_COMPLETED,
    OUTCOME_CODE_EFFECT_UNKNOWN, OUTCOME_CODE_FAILED, OUTCOME_CODE_PARTIAL_EFFECT,
    RegisterOperationResultWire, RegisterOperationWire, STATE_CODE_CANCEL_REQUESTED,
    STATE_CODE_CANCELLED_BEFORE_EFFECT, STATE_CODE_COMPLETED, STATE_CODE_DISPATCHED,
    STATE_CODE_EFFECT_UNKNOWN, STATE_CODE_FAILED, STATE_CODE_PARTIAL_EFFECT, STATE_CODE_REGISTERED,
    decode_complete_request, decode_complete_result, decode_dispatch_request,
    decode_dispatch_result, decode_register_request, decode_register_result,
    encode_complete_request, encode_complete_result, encode_dispatch_request,
    encode_dispatch_result, encode_register_request, encode_register_result,
};
use nlos_schema::sabi::v1::{
    DriverProviderCancelAck, DriverProviderCancelRequest, DriverProviderCompleteAck,
    DriverProviderCompleteRequest, DriverProviderDispatchAck, DriverProviderDispatchRequest,
    DriverProviderIsolationReport, DriverProviderQueryIsolationRequest, DriverProviderRegisterAck,
    DriverProviderRegisterRequest, DriverProviderRequest, DriverProviderResponse,
    DriverProviderTerminalOutcome, OperationLifecycleState, ProviderIsolationAttestationWire,
    SchemaIdentity, driver_provider_request, driver_provider_response,
};
use nlos_schema::{
    CompatibilityError, MAX_DRIVER_PROVIDER_PAYLOAD_BYTES, MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS,
    SABI_DRIVER_PROVIDER_SCHEMA, decode_driver_provider_request, decode_driver_provider_response,
    driver_provider_schema_identity, encode_driver_provider_request,
    encode_driver_provider_response,
};

pub use crate::driver_cbor::{
    CancelOperationResultWire, CancelOperationWire, IsolationReportWire, QueryIsolationWire,
    decode_cancel_request, decode_cancel_result, decode_isolation_report,
    decode_query_isolation_request, encode_cancel_request, encode_cancel_result,
    encode_isolation_report, encode_query_isolation_request,
};
pub use crate::enforcement::{
    CustodiedCredentialRef, DriverAdmissionRequest, DriverCompletionSubmission,
    EnforcementGateError, IsolationEnforcementPolicy, ReconcileUncertainRequest,
    ReservationActivationToken, SanitizedDriverDispatchTicket, TrustedDriverEnforcementGate,
    VerifiedDriverCompletion, compute_driver_authorization_digest,
};
use crate::wire::rewrite_top_level_schema_major;

const LEGACY_STAGE_B_PROFILE: &str = "legacy-stage-b-minicbor";
const DRIVER_PROVIDER_KNOWN_FIELDS: &[u32] = &[1, 2, 3, 4, 5, 6];

/// Canonical five-method surface of `nlos.sabi.DriverProvider`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DriverMethod {
    /// `register`
    Register,
    /// `dispatch`
    Dispatch,
    /// `complete`
    Complete,
    /// `cancel`
    Cancel,
    /// `query_isolation`
    QueryIsolation,
}

impl DriverMethod {
    /// Wire method token carried in `Envelope.method`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Register => "register",
            Self::Dispatch => "dispatch",
            Self::Complete => "complete",
            Self::Cancel => "cancel",
            Self::QueryIsolation => "query_isolation",
        }
    }
}

/// Legacy wire encoding spoken by an older driver adapter across [`DriverProtocolCompatShim`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LegacyDriverWireFormat {
    /// Stage B deterministic `minicbor` payloads (`nlos.driver-mock.payload` v1).
    StageBMinicbor,
    /// Historical `nlos.sabi.DriverProvider` Protobuf epoch (`N-1` / `N-2`).
    HistoricalDriverProviderProto,
}

/// Fail-closed translation errors produced by [`DriverProtocolCompatShim`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DriverShimError {
    /// Invalid `(driver_epoch, host_driver_epoch)` compatibility window.
    InvalidEpochWindow {
        driver_epoch: u32,
        host_driver_epoch: u32,
        max_backward_epochs: u32,
    },
    /// Legacy Stage B `minicbor` decode/encode failure.
    LegacyCbor(String),
    /// Canonical `nlos.sabi.DriverProvider` schema failure.
    ProtoSchema(CompatibilityError),
    /// Driver frame `SchemaIdentity.major` did not match expected epoch.
    EpochMismatch { expected: u32, actual: u32 },
    /// `DriverProvider` `oneof` arm was missing or mismatched.
    InvalidDriverMethodArm(&'static str),
    /// Terminal outcome and lifecycle state codes disagreed.
    OutcomeStateMismatch,
}

impl fmt::Display for DriverShimError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidEpochWindow {
                driver_epoch,
                host_driver_epoch,
                max_backward_epochs,
            } => write!(
                formatter,
                "invalid driver shim window: driver_epoch={driver_epoch}, host_driver_epoch={host_driver_epoch}, max_backward_epochs={max_backward_epochs}"
            ),
            Self::LegacyCbor(msg) => write!(formatter, "legacy driver CBOR error: {msg}"),
            Self::ProtoSchema(err) => write!(formatter, "DriverProvider proto error: {err}"),
            Self::EpochMismatch { expected, actual } => write!(
                formatter,
                "DriverProvider epoch mismatch: expected {expected}, got {actual}"
            ),
            Self::InvalidDriverMethodArm(detail) => {
                write!(formatter, "invalid DriverProvider method arm: {detail}")
            }
            Self::OutcomeStateMismatch => {
                formatter.write_str("DriverProvider outcome code and state code disagree")
            }
        }
    }
}

impl Error for DriverShimError {}

/// Outer untrusted `DriverProtocolCompatShim` (`[DRV-SHIM-001]`, `ADR-0027` section 2.2).
///
/// Notice the `Copy` representation consisting solely of integer epoch metadata
/// and [`LegacyDriverWireFormat`]: this type physically holds zero secret bytes,
/// zero signing keys, zero `Capability` handles, and zero authority references.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DriverProtocolCompatShim {
    driver_epoch: u32,
    host_driver_epoch: u32,
    legacy_wire_format: LegacyDriverWireFormat,
}

impl DriverProtocolCompatShim {
    /// Creates an unprivileged `DriverProtocolCompatShim` for `(driver_epoch, host_driver_epoch)`.
    ///
    /// # Errors
    ///
    /// Fails closed if `driver_epoch == 0`, `driver_epoch > host_driver_epoch`, or
    /// `host_driver_epoch - driver_epoch > max_backward_epochs` (`>= 2`).
    pub fn new(
        driver_epoch: u32,
        host_driver_epoch: u32,
        max_backward_epochs: u32,
        legacy_wire_format: LegacyDriverWireFormat,
    ) -> Result<Self, DriverShimError> {
        if driver_epoch == 0
            || max_backward_epochs < MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS
            || driver_epoch > host_driver_epoch
            || host_driver_epoch - driver_epoch > max_backward_epochs
        {
            return Err(DriverShimError::InvalidEpochWindow {
                driver_epoch,
                host_driver_epoch,
                max_backward_epochs,
            });
        }
        Ok(Self {
            driver_epoch,
            host_driver_epoch,
            legacy_wire_format,
        })
    }

    /// Convenience constructor for a Stage B `minicbor` legacy driver shim at `(1, host_epoch)`.
    ///
    /// # Errors
    ///
    /// Fails closed if `host_driver_epoch` is outside the `N / N-1 / N-2` window for epoch `1`.
    pub fn stage_b_minicbor(host_driver_epoch: u32) -> Result<Self, DriverShimError> {
        Self::new(
            1,
            host_driver_epoch,
            MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS,
            LegacyDriverWireFormat::StageBMinicbor,
        )
    }

    /// Declared driver ABI epoch.
    #[must_use]
    pub const fn driver_epoch(self) -> u32 {
        self.driver_epoch
    }

    /// Host canonical `DriverProvider` epoch.
    #[must_use]
    pub const fn host_driver_epoch(self) -> u32 {
        self.host_driver_epoch
    }

    /// Configured legacy wire format.
    #[must_use]
    pub const fn legacy_wire_format(self) -> LegacyDriverWireFormat {
        self.legacy_wire_format
    }

    /// Pure function: translates a legacy Stage B `minicbor` request payload into a
    /// canonical `nlos.sabi.DriverProvider` Protobuf frame at `host_driver_epoch`.
    ///
    /// # Errors
    ///
    /// Fails closed on malformed CBOR, invalid widths, zero generations, or proto overflow.
    pub fn cbor_request_to_proto_wire(
        self,
        method: DriverMethod,
        cbor: &[u8],
    ) -> Result<Vec<u8>, DriverShimError> {
        let arm = match method {
            DriverMethod::Register => {
                let req = decode_register_request(cbor)
                    .map_err(|e| DriverShimError::LegacyCbor(e.to_string()))?;
                driver_provider_request::Request::Register(DriverProviderRegisterRequest {
                    operation_id: req.operation_id.to_vec(),
                    operation_generation: req.operation_generation,
                    owner_fiber_id: req.owner_fiber_id.to_vec(),
                    owner_fiber_generation: req.owner_fiber_generation,
                    cancellation_scope_id: req.cancellation_scope_id.to_vec(),
                    cancellation_generation: req.cancellation_generation,
                })
            }
            DriverMethod::Dispatch => {
                let req = decode_dispatch_request(cbor)
                    .map_err(|e| DriverShimError::LegacyCbor(e.to_string()))?;
                driver_provider_request::Request::Dispatch(DriverProviderDispatchRequest {
                    operation_id: req.operation_id.to_vec(),
                    operation_generation: req.operation_generation,
                    callback_id: req.callback_id.to_vec(),
                })
            }
            DriverMethod::Complete => {
                let req = decode_complete_request(cbor)
                    .map_err(|e| DriverShimError::LegacyCbor(e.to_string()))?;
                driver_provider_request::Request::Complete(DriverProviderCompleteRequest {
                    operation_id: req.operation_id.to_vec(),
                    operation_generation: req.operation_generation,
                    callback_id: req.callback_id.to_vec(),
                    seed: req.seed.to_vec(),
                })
            }
            DriverMethod::Cancel => {
                let req = decode_cancel_request(cbor)
                    .map_err(|e| DriverShimError::LegacyCbor(e.to_string()))?;
                driver_provider_request::Request::Cancel(DriverProviderCancelRequest {
                    operation_id: req.operation_id.to_vec(),
                    operation_generation: req.operation_generation,
                    expected_cancel_epoch: req.expected_cancel_epoch,
                    no_effect_receipt_id: req.no_effect_receipt_id.to_vec(),
                })
            }
            DriverMethod::QueryIsolation => {
                let _ = decode_query_isolation_request(cbor)
                    .map_err(|e| DriverShimError::LegacyCbor(e.to_string()))?;
                driver_provider_request::Request::QueryIsolation(
                    DriverProviderQueryIsolationRequest {},
                )
            }
        };
        let v1_msg = DriverProviderRequest {
            schema: Some(driver_provider_schema_identity()),
            request: Some(arm),
        };
        let v1_wire =
            encode_driver_provider_request(&v1_msg).map_err(DriverShimError::ProtoSchema)?;
        stamp_driver_epoch(&v1_wire, self.host_driver_epoch)
    }

    /// Pure function: translates a canonical `nlos.sabi.DriverProvider` Protobuf
    /// request frame (at `host_driver_epoch`) into a legacy Stage B `minicbor` payload.
    ///
    /// # Errors
    ///
    /// Fails closed on epoch mismatch, malformed protobuf, or missing request arm.
    pub fn proto_wire_to_cbor_request(
        self,
        proto_wire: &[u8],
    ) -> Result<(DriverMethod, Vec<u8>), DriverShimError> {
        let v1_wire = normalize_driver_wire(proto_wire, self.host_driver_epoch)?;
        let decoded =
            decode_driver_provider_request(&v1_wire).map_err(DriverShimError::ProtoSchema)?;
        encode_cbor_from_proto_request(decoded)
    }

    /// Pure function: translates a legacy Stage B `minicbor` response payload into a
    /// canonical `nlos.sabi.DriverProvider` Protobuf response frame at `host_driver_epoch`.
    ///
    /// Legacy `CompleteOperationResultWire` lacks `[TASK-PROVIDER-001]` attestation
    /// fields and is therefore stamped with an explicit all-false, zero-digest
    /// unverified attestation (`PROVIDER_ISOLATION_UNVERIFIED`).
    ///
    /// # Errors
    ///
    /// Fails closed on malformed CBOR, invalid outcome/state codes, or proto validation error.
    pub fn cbor_response_to_proto_wire(
        self,
        method: DriverMethod,
        cbor: &[u8],
    ) -> Result<Vec<u8>, DriverShimError> {
        let arm = cbor_response_to_arm(method, cbor)?;
        let v1_msg = DriverProviderResponse {
            schema: Some(driver_provider_schema_identity()),
            response: Some(arm),
        };
        let v1_wire =
            encode_driver_provider_response(&v1_msg).map_err(DriverShimError::ProtoSchema)?;
        stamp_driver_epoch(&v1_wire, self.host_driver_epoch)
    }

    /// Pure function: translates a canonical `nlos.sabi.DriverProvider` Protobuf
    /// response frame (at `host_driver_epoch`) into a legacy Stage B `minicbor` payload.
    ///
    /// # Errors
    ///
    /// Fails closed on epoch mismatch, malformed protobuf, or missing response arm.
    pub fn proto_wire_to_cbor_response(
        self,
        proto_wire: &[u8],
    ) -> Result<(DriverMethod, Vec<u8>), DriverShimError> {
        let v1_wire = normalize_driver_wire(proto_wire, self.host_driver_epoch)?;
        let decoded =
            decode_driver_provider_response(&v1_wire).map_err(DriverShimError::ProtoSchema)?;
        encode_cbor_from_proto_response(decoded)
    }

    /// Pure function: translates an upstream `DriverProviderRequest` Protobuf frame
    /// from `driver_epoch` (`N-1` / `N-2`) to `host_driver_epoch` (`N`), preserving
    /// unknown trailing Protobuf fields lossless.
    ///
    /// # Errors
    ///
    /// Fails closed on epoch mismatch, size overflow, or structural `DriverProvider` error.
    pub fn translate_driver_request_upstream(
        self,
        driver_proto_wire: &[u8],
    ) -> Result<Vec<u8>, DriverShimError> {
        let v1_wire = normalize_driver_wire(driver_proto_wire, self.driver_epoch)?;
        let _ = decode_driver_provider_request(&v1_wire).map_err(DriverShimError::ProtoSchema)?;
        stamp_driver_epoch(driver_proto_wire, self.host_driver_epoch)
    }

    /// Pure function: translates a downstream `DriverProviderResponse` Protobuf frame
    /// from `host_driver_epoch` (`N`) back to `driver_epoch` (`N-1` / `N-2`), strictly
    /// preserving `EFFECT_UNKNOWN` and unknown trailing Protobuf fields.
    ///
    /// # Errors
    ///
    /// Fails closed on epoch mismatch, size overflow, or structural `DriverProvider` error.
    pub fn translate_driver_response_downstream(
        self,
        host_proto_wire: &[u8],
    ) -> Result<Vec<u8>, DriverShimError> {
        let v1_wire = normalize_driver_wire(host_proto_wire, self.host_driver_epoch)?;
        let _ = decode_driver_provider_response(&v1_wire).map_err(DriverShimError::ProtoSchema)?;
        stamp_driver_epoch(host_proto_wire, self.driver_epoch)
    }
}

fn normalize_driver_wire(wire: &[u8], expected_epoch: u32) -> Result<Vec<u8>, DriverShimError> {
    if wire.len() > MAX_DRIVER_PROVIDER_PAYLOAD_BYTES {
        return Err(DriverShimError::ProtoSchema(
            CompatibilityError::FrameTooLarge {
                actual: wire.len(),
                maximum: MAX_DRIVER_PROVIDER_PAYLOAD_BYTES,
            },
        ));
    }
    let rewritten = rewrite_top_level_schema_major(wire, 1, DRIVER_PROVIDER_KNOWN_FIELDS)
        .map_err(DriverShimError::ProtoSchema)?;
    check_driver_identity(&rewritten.original_identity, expected_epoch)?;
    Ok(rewritten.rewritten_wire)
}

fn stamp_driver_epoch(wire: &[u8], target_epoch: u32) -> Result<Vec<u8>, DriverShimError> {
    let rewritten =
        rewrite_top_level_schema_major(wire, target_epoch, DRIVER_PROVIDER_KNOWN_FIELDS)
            .map_err(DriverShimError::ProtoSchema)?;
    if rewritten.rewritten_wire.len() > MAX_DRIVER_PROVIDER_PAYLOAD_BYTES {
        return Err(DriverShimError::ProtoSchema(
            CompatibilityError::FrameTooLarge {
                actual: rewritten.rewritten_wire.len(),
                maximum: MAX_DRIVER_PROVIDER_PAYLOAD_BYTES,
            },
        ));
    }
    Ok(rewritten.rewritten_wire)
}

fn check_driver_identity(
    identity: &SchemaIdentity,
    expected_epoch: u32,
) -> Result<(), DriverShimError> {
    if identity.name != SABI_DRIVER_PROVIDER_SCHEMA {
        return Err(DriverShimError::ProtoSchema(
            CompatibilityError::UnknownSchema(identity.name.clone()),
        ));
    }
    if identity.major != expected_epoch {
        return Err(DriverShimError::EpochMismatch {
            expected: expected_epoch,
            actual: identity.major,
        });
    }
    Ok(())
}

fn encode_cbor_from_proto_request(
    decoded: DriverProviderRequest,
) -> Result<(DriverMethod, Vec<u8>), DriverShimError> {
    match decoded.request {
        Some(driver_provider_request::Request::Register(req)) => {
            let cbor = encode_register_request(&RegisterOperationWire {
                operation_id: fixed_16(&req.operation_id)?,
                operation_generation: req.operation_generation,
                owner_fiber_id: fixed_16(&req.owner_fiber_id)?,
                owner_fiber_generation: req.owner_fiber_generation,
                cancellation_scope_id: fixed_16(&req.cancellation_scope_id)?,
                cancellation_generation: req.cancellation_generation,
            })
            .map_err(|e| DriverShimError::LegacyCbor(e.to_string()))?;
            Ok((DriverMethod::Register, cbor))
        }
        Some(driver_provider_request::Request::Dispatch(req)) => {
            let cbor = encode_dispatch_request(&DispatchOperationWire {
                operation_id: fixed_16(&req.operation_id)?,
                operation_generation: req.operation_generation,
                callback_id: fixed_16(&req.callback_id)?,
            })
            .map_err(|e| DriverShimError::LegacyCbor(e.to_string()))?;
            Ok((DriverMethod::Dispatch, cbor))
        }
        Some(driver_provider_request::Request::Complete(req)) => {
            let cbor = encode_complete_request(&CompleteOperationWire {
                operation_id: fixed_16(&req.operation_id)?,
                operation_generation: req.operation_generation,
                callback_id: fixed_16(&req.callback_id)?,
                seed: fixed_32(&req.seed)?,
            })
            .map_err(|e| DriverShimError::LegacyCbor(e.to_string()))?;
            Ok((DriverMethod::Complete, cbor))
        }
        Some(driver_provider_request::Request::Cancel(req)) => {
            let cbor = encode_cancel_request(&CancelOperationWire {
                operation_id: fixed_16(&req.operation_id)?,
                operation_generation: req.operation_generation,
                expected_cancel_epoch: req.expected_cancel_epoch,
                no_effect_receipt_id: fixed_16(&req.no_effect_receipt_id)?,
            })
            .map_err(|e| DriverShimError::LegacyCbor(e.to_string()))?;
            Ok((DriverMethod::Cancel, cbor))
        }
        Some(driver_provider_request::Request::QueryIsolation(_)) => {
            let cbor = encode_query_isolation_request(&QueryIsolationWire)
                .map_err(|e| DriverShimError::LegacyCbor(e.to_string()))?;
            Ok((DriverMethod::QueryIsolation, cbor))
        }
        None => Err(DriverShimError::InvalidDriverMethodArm("missing request")),
    }
}

fn cbor_response_to_arm(
    method: DriverMethod,
    cbor: &[u8],
) -> Result<driver_provider_response::Response, DriverShimError> {
    match method {
        DriverMethod::Register => {
            let res = decode_register_result(cbor)
                .map_err(|e| DriverShimError::LegacyCbor(e.to_string()))?;
            Ok(driver_provider_response::Response::RegisterAck(
                DriverProviderRegisterAck {
                    replayed: res.replayed,
                    operation_id: res.operation_id.to_vec(),
                    operation_generation: res.operation_generation,
                    admission_receipt_id: res.admission_receipt_id.to_vec(),
                },
            ))
        }
        DriverMethod::Dispatch => {
            let res = decode_dispatch_result(cbor)
                .map_err(|e| DriverShimError::LegacyCbor(e.to_string()))?;
            Ok(driver_provider_response::Response::DispatchAck(
                DriverProviderDispatchAck {
                    replayed: res.replayed,
                    preparation_receipt_id: res.preparation_receipt_id.to_vec(),
                    activation_receipt_id: res.activation_receipt_id.to_vec(),
                    callback_id: res.callback_id.to_vec(),
                    operation_id: res.operation_id.to_vec(),
                    operation_generation: res.operation_generation,
                    owner_fiber_id: res.owner_fiber_id.to_vec(),
                    owner_fiber_generation: res.owner_fiber_generation,
                    cancel_epoch: res.cancel_epoch,
                },
            ))
        }
        DriverMethod::Complete => {
            let res = decode_complete_result(cbor)
                .map_err(|e| DriverShimError::LegacyCbor(e.to_string()))?;
            let (outcome, state) = map_cbor_outcome_and_state(res.outcome_code, res.state_code)?;
            Ok(driver_provider_response::Response::CompleteAck(
                DriverProviderCompleteAck {
                    replayed: res.replayed,
                    outcome: outcome.into(),
                    receipt_id: res.receipt_id.to_vec(),
                    state: state.into(),
                    compute_digest: [0_u8; 32].to_vec(),
                    isolation: Some(ProviderIsolationAttestationWire {
                        tenant_isolated: false,
                        session_ephemeral: false,
                        cache_partitioned: false,
                        zero_data_retention: false,
                        attestation_digest: [0_u8; 32].to_vec(),
                        provider_profile: LEGACY_STAGE_B_PROFILE.to_owned(),
                    }),
                    fenced_reconciliation_only: false,
                },
            ))
        }
        DriverMethod::Cancel => {
            let res = decode_cancel_result(cbor)
                .map_err(|e| DriverShimError::LegacyCbor(e.to_string()))?;
            let state = cbor_state_to_proto(res.state_code)?;
            let terminal_receipt = if res.already_terminal || state_is_terminal(state) {
                res.terminal_receipt_id.to_vec()
            } else {
                Vec::new()
            };
            Ok(driver_provider_response::Response::CancelAck(
                DriverProviderCancelAck {
                    replayed: res.replayed,
                    already_terminal: res.already_terminal,
                    operation_id: res.operation_id.to_vec(),
                    operation_generation: res.operation_generation,
                    state: state.into(),
                    cancel_epoch: res.cancel_epoch,
                    terminal_receipt_id: terminal_receipt,
                },
            ))
        }
        DriverMethod::QueryIsolation => {
            let rep = decode_isolation_report(cbor)
                .map_err(|e| DriverShimError::LegacyCbor(e.to_string()))?;
            Ok(driver_provider_response::Response::IsolationReport(
                DriverProviderIsolationReport {
                    isolation: Some(ProviderIsolationAttestationWire {
                        tenant_isolated: rep.tenant_isolated,
                        session_ephemeral: rep.session_ephemeral,
                        cache_partitioned: rep.cache_partitioned,
                        zero_data_retention: rep.zero_data_retention,
                        attestation_digest: rep.attestation_digest.to_vec(),
                        provider_profile: rep.provider_profile,
                    }),
                },
            ))
        }
    }
}

fn encode_cbor_from_proto_response(
    decoded: DriverProviderResponse,
) -> Result<(DriverMethod, Vec<u8>), DriverShimError> {
    match decoded.response {
        Some(driver_provider_response::Response::RegisterAck(ack)) => {
            let cbor = encode_register_result(&RegisterOperationResultWire {
                replayed: ack.replayed,
                operation_id: fixed_16(&ack.operation_id)?,
                operation_generation: ack.operation_generation,
                admission_receipt_id: fixed_16(&ack.admission_receipt_id)?,
            })
            .map_err(|e| DriverShimError::LegacyCbor(e.to_string()))?;
            Ok((DriverMethod::Register, cbor))
        }
        Some(driver_provider_response::Response::DispatchAck(ack)) => {
            let cbor = encode_dispatch_result(&DispatchOperationResultWire {
                replayed: ack.replayed,
                preparation_receipt_id: fixed_16(&ack.preparation_receipt_id)?,
                activation_receipt_id: fixed_16(&ack.activation_receipt_id)?,
                callback_id: fixed_16(&ack.callback_id)?,
                operation_id: fixed_16(&ack.operation_id)?,
                operation_generation: ack.operation_generation,
                owner_fiber_id: fixed_16(&ack.owner_fiber_id)?,
                owner_fiber_generation: ack.owner_fiber_generation,
                cancel_epoch: ack.cancel_epoch,
            })
            .map_err(|e| DriverShimError::LegacyCbor(e.to_string()))?;
            Ok((DriverMethod::Dispatch, cbor))
        }
        Some(driver_provider_response::Response::CompleteAck(ack)) => {
            let outcome = DriverProviderTerminalOutcome::try_from(ack.outcome)
                .map_err(|_| DriverShimError::OutcomeStateMismatch)?;
            let state = OperationLifecycleState::try_from(ack.state)
                .map_err(|_| DriverShimError::OutcomeStateMismatch)?;
            let cbor = encode_complete_result(&CompleteOperationResultWire {
                replayed: ack.replayed,
                outcome_code: proto_outcome_to_cbor(outcome)?,
                receipt_id: fixed_16(&ack.receipt_id)?,
                state_code: proto_state_to_cbor(state)?,
            })
            .map_err(|e| DriverShimError::LegacyCbor(e.to_string()))?;
            Ok((DriverMethod::Complete, cbor))
        }
        Some(driver_provider_response::Response::CancelAck(ack)) => {
            let state = OperationLifecycleState::try_from(ack.state)
                .map_err(|_| DriverShimError::OutcomeStateMismatch)?;
            let receipt = if ack.terminal_receipt_id.is_empty() {
                [0_u8; 16]
            } else {
                fixed_16(&ack.terminal_receipt_id)?
            };
            let cbor = encode_cancel_result(&CancelOperationResultWire {
                replayed: ack.replayed,
                already_terminal: ack.already_terminal,
                operation_id: fixed_16(&ack.operation_id)?,
                operation_generation: ack.operation_generation,
                state_code: proto_state_to_cbor(state)?,
                cancel_epoch: ack.cancel_epoch,
                terminal_receipt_id: receipt,
            })
            .map_err(|e| DriverShimError::LegacyCbor(e.to_string()))?;
            Ok((DriverMethod::Cancel, cbor))
        }
        Some(driver_provider_response::Response::IsolationReport(rep)) => {
            let iso = rep
                .isolation
                .ok_or(DriverShimError::InvalidDriverMethodArm("missing isolation"))?;
            let cbor = encode_isolation_report(&IsolationReportWire {
                tenant_isolated: iso.tenant_isolated,
                session_ephemeral: iso.session_ephemeral,
                cache_partitioned: iso.cache_partitioned,
                zero_data_retention: iso.zero_data_retention,
                attestation_digest: fixed_32(&iso.attestation_digest)?,
                provider_profile: iso.provider_profile,
            })
            .map_err(|e| DriverShimError::LegacyCbor(e.to_string()))?;
            Ok((DriverMethod::QueryIsolation, cbor))
        }
        None => Err(DriverShimError::InvalidDriverMethodArm("missing response")),
    }
}

fn map_cbor_outcome_and_state(
    outcome_code: u8,
    state_code: u8,
) -> Result<(DriverProviderTerminalOutcome, OperationLifecycleState), DriverShimError> {
    let outcome = match outcome_code {
        OUTCOME_CODE_COMPLETED => DriverProviderTerminalOutcome::Completed,
        OUTCOME_CODE_FAILED => DriverProviderTerminalOutcome::Failed,
        OUTCOME_CODE_PARTIAL_EFFECT => DriverProviderTerminalOutcome::PartialEffect,
        OUTCOME_CODE_EFFECT_UNKNOWN => DriverProviderTerminalOutcome::EffectUnknown,
        OUTCOME_CODE_CANCELLED_BEFORE_EFFECT => {
            DriverProviderTerminalOutcome::CancelledBeforeEffect
        }
        _ => return Err(DriverShimError::OutcomeStateMismatch),
    };
    let state = cbor_state_to_proto(state_code)?;
    let matches = matches!(
        (outcome, state),
        (
            DriverProviderTerminalOutcome::Completed,
            OperationLifecycleState::Completed
        ) | (
            DriverProviderTerminalOutcome::Failed,
            OperationLifecycleState::Failed
        ) | (
            DriverProviderTerminalOutcome::PartialEffect,
            OperationLifecycleState::PartialEffect
        ) | (
            DriverProviderTerminalOutcome::EffectUnknown,
            OperationLifecycleState::EffectUnknown
        ) | (
            DriverProviderTerminalOutcome::CancelledBeforeEffect,
            OperationLifecycleState::CancelledBeforeEffect
        )
    );
    if !matches {
        return Err(DriverShimError::OutcomeStateMismatch);
    }
    Ok((outcome, state))
}

fn cbor_state_to_proto(state_code: u8) -> Result<OperationLifecycleState, DriverShimError> {
    match state_code {
        STATE_CODE_REGISTERED => Ok(OperationLifecycleState::Registered),
        STATE_CODE_DISPATCHED => Ok(OperationLifecycleState::Dispatched),
        STATE_CODE_CANCEL_REQUESTED => Ok(OperationLifecycleState::CancelRequested),
        STATE_CODE_COMPLETED => Ok(OperationLifecycleState::Completed),
        STATE_CODE_FAILED => Ok(OperationLifecycleState::Failed),
        STATE_CODE_CANCELLED_BEFORE_EFFECT => Ok(OperationLifecycleState::CancelledBeforeEffect),
        STATE_CODE_PARTIAL_EFFECT => Ok(OperationLifecycleState::PartialEffect),
        STATE_CODE_EFFECT_UNKNOWN => Ok(OperationLifecycleState::EffectUnknown),
        _ => Err(DriverShimError::OutcomeStateMismatch),
    }
}

fn proto_outcome_to_cbor(outcome: DriverProviderTerminalOutcome) -> Result<u8, DriverShimError> {
    match outcome {
        DriverProviderTerminalOutcome::Completed => Ok(OUTCOME_CODE_COMPLETED),
        DriverProviderTerminalOutcome::Failed => Ok(OUTCOME_CODE_FAILED),
        DriverProviderTerminalOutcome::PartialEffect => Ok(OUTCOME_CODE_PARTIAL_EFFECT),
        DriverProviderTerminalOutcome::EffectUnknown => Ok(OUTCOME_CODE_EFFECT_UNKNOWN),
        DriverProviderTerminalOutcome::CancelledBeforeEffect => {
            Ok(OUTCOME_CODE_CANCELLED_BEFORE_EFFECT)
        }
        DriverProviderTerminalOutcome::Unspecified => Err(DriverShimError::OutcomeStateMismatch),
    }
}

fn proto_state_to_cbor(state: OperationLifecycleState) -> Result<u8, DriverShimError> {
    match state {
        OperationLifecycleState::Registered => Ok(STATE_CODE_REGISTERED),
        OperationLifecycleState::Dispatched => Ok(STATE_CODE_DISPATCHED),
        OperationLifecycleState::CancelRequested => Ok(STATE_CODE_CANCEL_REQUESTED),
        OperationLifecycleState::Completed => Ok(STATE_CODE_COMPLETED),
        OperationLifecycleState::Failed => Ok(STATE_CODE_FAILED),
        OperationLifecycleState::CancelledBeforeEffect => Ok(STATE_CODE_CANCELLED_BEFORE_EFFECT),
        OperationLifecycleState::PartialEffect => Ok(STATE_CODE_PARTIAL_EFFECT),
        OperationLifecycleState::EffectUnknown => Ok(STATE_CODE_EFFECT_UNKNOWN),
        OperationLifecycleState::Unspecified => Err(DriverShimError::OutcomeStateMismatch),
    }
}

const fn state_is_terminal(state: OperationLifecycleState) -> bool {
    matches!(
        state,
        OperationLifecycleState::Completed
            | OperationLifecycleState::Failed
            | OperationLifecycleState::CancelledBeforeEffect
            | OperationLifecycleState::PartialEffect
            | OperationLifecycleState::EffectUnknown
    )
}

fn fixed_16(slice: &[u8]) -> Result<[u8; 16], DriverShimError> {
    slice
        .try_into()
        .map_err(|_| DriverShimError::LegacyCbor("expected 16-byte identifier".to_owned()))
}

fn fixed_32(slice: &[u8]) -> Result<[u8; 32], DriverShimError> {
    slice
        .try_into()
        .map_err(|_| DriverShimError::LegacyCbor("expected 32-byte digest".to_owned()))
}
