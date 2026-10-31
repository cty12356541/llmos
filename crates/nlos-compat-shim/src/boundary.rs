//! Four-layer `ADR-0027` section 2.2.2 TCB enforcement boundary and unprivileged
//! out-of-core compatibility shim isolation model (`[DRV-SHIM-001]`, `[DRV-CRED-001]`,
//! `[DRV-BOUND-001]`, `[DRV-ACTIVE-001]`, `[DRV-LIFE-001]`, `[KABI-UNCERTAIN-001]`,
//! `[TASK-PROVIDER-001]`).
//!
//! Architecture layers enforced by this module:
//! - **Layer 1 (Core Authorities)**: `IdentityAuthority`, `CapabilityAuthority`,
//!   `ResourceAuthority`, and fenced `OperationRegistry`. Speaks only canonical
//!   host-epoch domain contracts; contains zero historical branches or shim imports.
//! - **Layer 2 (`TrustedEnforcementGate`)**: TCB-edge mediation gateway holding
//!   [`SecretCredential`]s exclusively inside its private TCB vault, requiring
//!   canonical `host_epoch` envelopes (`0` historical `client_epoch` branches),
//!   enforcing `ADR-0013` `verify-then-commit` across Identity, Capability,
//!   Resource, Process/Driver incarnation fences, and Operation state machines,
//!   verifying TCB-meter Ed25519 signatures on usage receipts, and fencing
//!   mid-flight shim/driver crashes into `OperationState::EffectUnknown` +
//!   `ReservationState::Quarantined`.
//! - **Layer 3 (`OutOfCoreCompatShimRuntime`)**: Unprivileged out-of-core shim
//!   combining [`SabiEpochAdapterCell`] and [`DriverProtocolShim`] with a complete
//!   [`WireCaptureTranscript`]. Holds zero credentials, zero private signing keys,
//!   and zero authority database handles.
//! - **Layer 4 (App / Driver Adapter)**: Historical `N-1` / `N-2` or Stage B
//!   `minicbor` caller and user-mode driver endpoints.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::num::NonZeroU64;
use std::sync::{Arc, Mutex};

use nlos_capability::{
    AuthorizeSemanticRequest, CapabilityAuthority, CapabilityAuthorityError,
    CapabilityConsumptionReceipt, CapabilityHandle, CapabilityRights, CapabilityTarget,
    ConsumeCapabilityRequest,
};
use nlos_driver_mock::proto_codec::{
    COMPLETE_PROVIDER_METHOD, DRIVER_PROVIDER_SERVICE, DecodedProviderRequest,
    DecodedProviderResponse, PROVIDER_ISOLATION_VERIFIED_TAG, ProviderExecutionReceipt,
    REGISTER_PROVIDER_METHOD, decode_request_proto, decode_response_proto,
};
use nlos_identity::{
    Ed25519Signature, IdentityAuthority, IdentityAuthorityError,
    VerifySemanticAuthoritySignatureRequest, VerifySemanticSignatureRequest,
};
use nlos_operation::{
    CallbackTicket, CompletionDecision, CompletionOutcome, OperationError, OperationHandle,
    OperationRegistry, OperationSnapshot, OperationSpec, OperationState,
};
use nlos_resource::{
    ActivateReservationRequest, ActivationReceipt, ConsumeReservationRequest, ConsumptionReceipt,
    FencingToken, FinalizationReceipt, FinalizeReservationRequest, QuarantineReceipt,
    QuarantineReservationRequest, ResourceAuthority, ResourceAuthorityError,
};
use nlos_schema::sabi::v1::{
    Envelope, RetryDirective, SabiErrorCode, SabiFailure, SabiRequestContext, SabiResponseContext,
    SchemaIdentity, envelope,
};
use nlos_schema::{
    CompatibilityError, REQUEST_ID_BYTES, SABI_ENVELOPE_SCHEMA, VersionCompatibilityPolicy,
};
use nlos_types::{
    CallId, CallbackId, ControlDomainId, DriverId, Generation, IdempotencyKey, KeyId, OperationId,
    PrincipalId, ReceiptId, ReservationId, SemanticEventId, TaskParticipantId,
};
use prost::Message;
use sha2::{Digest, Sha256};

use crate::driver_shim::{
    DriverProtocolShim, DriverShimError, LegacyCompleteBridgeContext, ProviderTrustCeiling,
};
use crate::sabi_adapter::{SabiAdapterError, SabiEpochAdapterCell};

const CREDENTIAL_ID_BYTES: usize = 16;
const DIGEST_BYTES: usize = 32;

/// TCB-scoped opaque credential (`[DRV-CRED-001]`).
///
/// Raw secret bytes are kept private inside the TCB enforcement gate, redacted in
/// [`fmt::Debug`] and [`fmt::Display`], zeroized on [`Drop`], and never serialized
/// onto any SABI [`Envelope`] or [`DriverProtocolShim`] wire frame.
#[derive(Clone, Eq, PartialEq)]
pub struct SecretCredential {
    credential_id: [u8; CREDENTIAL_ID_BYTES],
    secret_bytes: Vec<u8>,
}

impl SecretCredential {
    /// Creates a new TCB-scoped secret credential.
    ///
    /// # Errors
    ///
    /// Fails closed with [`BoundaryViolation::EmptySecretCredential`] when
    /// `secret_bytes` is empty.
    pub fn new(
        credential_id: [u8; CREDENTIAL_ID_BYTES],
        secret_bytes: Vec<u8>,
    ) -> Result<Self, BoundaryViolation> {
        if secret_bytes.is_empty() {
            return Err(BoundaryViolation::EmptySecretCredential);
        }
        Ok(Self {
            credential_id,
            secret_bytes,
        })
    }

    /// Public 128-bit opaque credential handle identifier (safe to carry in
    /// capability/dispatch requests without exposing secret material).
    #[must_use]
    pub const fn credential_id(&self) -> [u8; CREDENTIAL_ID_BYTES] {
        self.credential_id
    }

    /// Byte length of the sealed secret material.
    #[must_use]
    pub fn secret_len(&self) -> usize {
        self.secret_bytes.len()
    }

    /// Computes a domain-separated one-way TCB commitment digest binding this
    /// credential to `(operation_id, driver_fencing_token)` without revealing
    /// `secret_bytes`.
    #[must_use]
    fn tcb_commitment_digest(
        &self,
        operation_id: OperationId,
        driver_fencing_token: FencingToken,
    ) -> [u8; DIGEST_BYTES] {
        let mut secret_hasher = Sha256::new();
        secret_hasher.update(b"nlos/tcb-secret-material/v1");
        secret_hasher.update(&self.secret_bytes);
        let secret_digest: [u8; DIGEST_BYTES] = secret_hasher.finalize().into();

        let mut commitment = Sha256::new();
        commitment.update(b"nlos/tcb-credential-binding/v1");
        commitment.update(self.credential_id);
        commitment.update(operation_id.as_bytes());
        commitment.update(driver_fencing_token);
        commitment.update(secret_digest);
        commitment.finalize().into()
    }

    #[must_use]
    fn appears_in_wire_slice(&self, wire: &[u8]) -> bool {
        wire.windows(self.secret_bytes.len())
            .any(|candidate| candidate == self.secret_bytes.as_slice())
    }
}

impl fmt::Debug for SecretCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SecretCredential")
            .field("credential_id", &self.credential_id)
            .field("secret_bytes", &"<redacted-secret>")
            .finish()
    }
}

impl fmt::Display for SecretCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "SecretCredential(id={:02x?}, material=<redacted-secret>)",
            &self.credential_id[..4]
        )
    }
}

impl Drop for SecretCredential {
    fn drop(&mut self) {
        self.secret_bytes.fill(0);
    }
}

/// Complete six-tuple execution and authorization envelope accompanying a mediated
/// driver dispatch (`[DRV-SHIM-001]`, `[DRV-BOUND-001]`, `[DRV-ACTIVE-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MediatedDriverDispatchAuth {
    pub caller_principal: PrincipalId,
    pub caller_control_domain: ControlDomainId,
    pub caller_key_id: KeyId,
    pub caller_signature: Ed25519Signature,
    pub process_id: TaskParticipantId,
    pub process_generation: Generation,
    pub capability_handle: CapabilityHandle,
    pub capability_target: CapabilityTarget,
    pub required_right: CapabilityRights,
    pub purpose_digest: Option<[u8; DIGEST_BYTES]>,
    pub reservation_id: ReservationId,
    pub call_id: CallId,
    pub activation_token: [u8; DIGEST_BYTES],
    pub driver_id: DriverId,
    pub driver_generation: Generation,
    pub driver_fencing_token: FencingToken,
    pub credential_id: Option<[u8; CREDENTIAL_ID_BYTES]>,
    pub callback_id: CallbackId,
    pub allow_verified_isolation: bool,
    pub admitted_at_ms: u64,
    pub now_monotonic_ns: u64,
}

/// Computes the deterministic [`SemanticEventId`] signed by the caller principal
/// over every semantic and fencing field of a mediated driver dispatch.
///
/// Any mid-transit shim tampering of `CapabilityId`, `Generation`, `ReservationId`,
/// `activation_token`, `OperationSpec`, `CallbackId`, `process_generation`,
/// `driver_generation`, `driver_fencing_token`, `request_id`, `idempotency_key`,
/// `deadline_monotonic_ns`, or `activity_context` changes this digest and causes
/// `IdentityAuthority::verify_semantic_signature` to fail closed.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn compute_dispatch_semantic_event_id(
    request_id: [u8; REQUEST_ID_BYTES],
    idempotency_key: IdempotencyKey,
    deadline_monotonic_ns: u64,
    activity_context: &[u8],
    spec: &OperationSpec,
    auth: &MediatedDriverDispatchAuth,
) -> SemanticEventId {
    let mut hasher = Sha256::new();
    hasher.update(b"nlos/drv-shim-mediated-dispatch/v1");
    hasher.update(request_id);
    hasher.update(idempotency_key.as_bytes());
    hasher.update(deadline_monotonic_ns.to_be_bytes());
    hasher.update(
        u64::try_from(activity_context.len())
            .unwrap_or(u64::MAX)
            .to_be_bytes(),
    );
    hasher.update(activity_context);
    hasher.update(auth.caller_principal.as_bytes());
    hasher.update(auth.caller_control_domain.as_bytes());
    hasher.update(auth.caller_key_id.as_bytes());
    hasher.update(auth.process_id.as_bytes());
    hasher.update(auth.process_generation.get().to_be_bytes());
    hasher.update(auth.capability_handle.capability_id.as_bytes());
    hasher.update(auth.capability_handle.generation.get().to_be_bytes());
    hash_capability_target(&mut hasher, auth.capability_target);
    hasher.update(auth.required_right.bits().to_be_bytes());
    match auth.purpose_digest {
        Some(digest) => {
            hasher.update([1]);
            hasher.update(digest);
        }
        None => hasher.update([0]),
    }
    hasher.update(auth.reservation_id.as_bytes());
    hasher.update(auth.call_id.as_bytes());
    hasher.update(auth.activation_token);
    hasher.update(auth.driver_id.as_bytes());
    hasher.update(auth.driver_generation.get().to_be_bytes());
    hasher.update(auth.driver_fencing_token);
    match auth.credential_id {
        Some(cred_id) => {
            hasher.update([1]);
            hasher.update(cred_id);
        }
        None => hasher.update([0]),
    }
    hasher.update(spec.operation_id.as_bytes());
    hasher.update(spec.generation.get().to_be_bytes());
    hasher.update(spec.owner_fiber.fiber_id.as_bytes());
    hasher.update(spec.owner_fiber.generation.get().to_be_bytes());
    hasher.update(spec.cancellation_scope_id.as_bytes());
    hasher.update(spec.cancellation_generation.get().to_be_bytes());
    hasher.update(auth.callback_id.as_bytes());
    hasher.update([u8::from(auth.allow_verified_isolation)]);

    let digest: [u8; DIGEST_BYTES] = hasher.finalize().into();
    SemanticEventId::from_bytes(digest)
}

fn hash_capability_target(hasher: &mut Sha256, target: CapabilityTarget) {
    match target {
        CapabilityTarget::Namespace(ns_id) => {
            hasher.update([1]);
            hasher.update(ns_id.as_bytes());
        }
        CapabilityTarget::Task(task_id) => {
            hasher.update([2]);
            hasher.update(task_id.as_bytes());
        }
    }
}

/// Cryptographically signed TCB-meter usage proof required before settling any
/// active or quarantined `Reservation` (`[DRV-ACTIVE-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SignedMeteredUsageProof {
    pub meter_principal: PrincipalId,
    pub meter_control_domain: ControlDomainId,
    pub meter_key_id: KeyId,
    pub meter_signature: Ed25519Signature,
    pub reservation_id: ReservationId,
    pub operation_id: OperationId,
    pub activation_receipt_id: ReceiptId,
    pub execution_receipt_id: ReceiptId,
    pub sequence: u64,
    pub cumulative_usage: u64,
    pub effect_closed_proof_digest: [u8; DIGEST_BYTES],
    pub settled_at_ms: u64,
}

/// Computes the domain-separated 32-byte digest signed by the TCB meter authority
/// and verified via `IdentityAuthority::verify_semantic_authority_signature`.
#[must_use]
pub fn compute_metered_usage_proof_digest(
    reservation_id: ReservationId,
    operation_id: OperationId,
    activation_receipt_id: ReceiptId,
    execution_receipt_id: ReceiptId,
    sequence: u64,
    cumulative_usage: u64,
    effect_closed_proof_digest: [u8; DIGEST_BYTES],
) -> [u8; DIGEST_BYTES] {
    let mut hasher = Sha256::new();
    hasher.update(b"nlos/drv-shim-metered-usage-proof/v1");
    hasher.update(reservation_id.as_bytes());
    hasher.update(operation_id.as_bytes());
    hasher.update(activation_receipt_id.as_bytes());
    hasher.update(execution_receipt_id.as_bytes());
    hasher.update(sequence.to_be_bytes());
    hasher.update(cumulative_usage.to_be_bytes());
    hasher.update(effect_closed_proof_digest);
    hasher.finalize().into()
}

/// Authoritative TCB-edge dispatch admission receipt produced by
/// [`TrustedEnforcementGate::admit_mediated_driver_dispatch`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct AuthorizedDriverDispatchReceipt {
    pub replayed: bool,
    pub request_id: [u8; REQUEST_ID_BYTES],
    pub idempotency_key: IdempotencyKey,
    pub capability_receipt: CapabilityConsumptionReceipt,
    pub activation_receipt: ActivationReceipt,
    pub callback_ticket: CallbackTicket,
    pub tcb_credential_commitment: Option<[u8; DIGEST_BYTES]>,
}

/// Authoritative TCB-edge completion and resource settlement receipt produced by
/// [`TrustedEnforcementGate::complete_mediated_driver_call`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorizedDriverCompletionReceipt {
    pub consumption_receipt: ConsumptionReceipt,
    pub finalization_receipt: FinalizationReceipt,
    pub completion_decision: CompletionDecision,
    pub provider_execution_receipt: ProviderExecutionReceipt,
}

/// Authoritative crash-quarantine receipt produced when a mid-flight shim or
/// driver crashes (`[DRV-LIFE-001]`, `[KABI-UNCERTAIN-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CrashQuarantineReceipt {
    pub operation_snapshot: OperationSnapshot,
    pub quarantine_receipt: QuarantineReceipt,
    pub original_idempotency_key: IdempotencyKey,
}

/// Authoritative reconciliation receipt produced when an `EffectUnknown` +
/// `Quarantined` operation is reconciled with its exact original 128-bit
/// [`IdempotencyKey`] (`[KABI-UNCERTAIN-001]`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReconciledUncertaintyReceipt {
    pub replayed: bool,
    pub operation_id: OperationId,
    pub idempotency_key: IdempotencyKey,
    pub finalization_receipt: FinalizationReceipt,
}

/// Fail-closed TCB enforcement errors returned by [`TrustedEnforcementGate`].
#[derive(Debug)]
pub enum BoundaryViolation {
    /// `SecretCredential` was constructed with zero bytes.
    EmptySecretCredential,
    /// Envelope arriving at TCB edge did not carry canonical `host_epoch` (`0` historical branches in TCB).
    NonCanonicalHostEpoch { expected: u32, actual: u32 },
    /// Canonical envelope failed SABI schema validation.
    EnvelopeSchema(CompatibilityError),
    /// Envelope targeted a service other than `"driver_provider"` or wrong method.
    UnexpectedServiceOrMethod { service: String, method: String },
    /// Envelope was missing `SabiRequestContext` or `SabiResponseContext`.
    MissingEnvelopeContext,
    /// Request omitted or provided a non-16-byte `idempotency_key`.
    InvalidIdempotencyKeyLength { actual: usize },
    /// Request omitted (`0`) or exceeded `deadline_monotonic_ns` (`[DRV-BOUND-001]`).
    DeadlineExpired {
        deadline_monotonic_ns: u64,
        now_monotonic_ns: u64,
    },
    /// Request omitted `SabiRequestContext.activity_context` (`[DRV-BOUND-001]`).
    MissingActivityContext,
    /// Caller process incarnation is unregistered or stale (`[DRV-BOUND-001]`).
    StaleProcessIncarnation {
        process_id: TaskParticipantId,
        expected: Option<Generation>,
        actual: Generation,
    },
    /// Requested `credential_id` does not exist in the TCB secret vault (`[DRV-CRED-001]`).
    UnknownCredential([u8; CREDENTIAL_ID_BYTES]),
    /// Frozen `DriverProvider` payload inside canonical `Envelope.payload` was invalid.
    DriverPayload(DriverShimError),
    /// Signed `OperationId` / `ReservationId` / `ActivationReceipt` disagreed with payload or ledger.
    OperationOrReservationBindingMismatch(&'static str),
    /// Cryptographic caller or meter signature verification failed in `IdentityAuthority`.
    Identity(IdentityAuthorityError),
    /// Capability authorization or consumption failed in `CapabilityAuthority` (`[DRV-SHIM-001]`).
    Capability(CapabilityAuthorityError),
    /// Reservation permit, activation, consumption, quarantine, or finalization failed in `ResourceAuthority`.
    Resource(ResourceAuthorityError),
    /// Fenced operation state transition failed in `OperationRegistry`.
    Operation(OperationError),
    /// `SignedMeteredUsageProof` was signed by an untrusted principal rather than the TCB meter authority.
    UntrustedUsageMeterSigner {
        expected: PrincipalId,
        actual: PrincipalId,
    },
    /// Legacy or unverified shim attempted to claim `PROVIDER_ISOLATION_VERIFIED_TAG` or non-zero digest.
    ForbiddenIsolationElevation(&'static str),
    /// Reused an `IdempotencyKey` with materially different dispatch inputs (`[KABI-IDEM-001]`).
    IdempotencyKeyPayloadConflict { idempotency_key: IdempotencyKey },
    /// Attempted blind retry or reconciliation of an `EffectUnknown` operation with a new `IdempotencyKey` (`[KABI-UNCERTAIN-001]`).
    BlindRetryWithNewIdempotencyKeyForbidden {
        operation_id: OperationId,
        expected_idempotency_key: IdempotencyKey,
        attempted_idempotency_key: IdempotencyKey,
    },
    /// Operation is currently `EffectUnknown` + `Quarantined` and requires explicit reconciliation.
    OperationAwaitingReconciliation {
        operation_id: OperationId,
        original_idempotency_key: IdempotencyKey,
    },
    /// Referenced `OperationId` is unknown to the TCB enforcement gate.
    UnknownOperation(OperationId),
}

impl fmt::Display for BoundaryViolation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptySecretCredential => {
                formatter.write_str("TCB secret credential material cannot be empty")
            }
            Self::NonCanonicalHostEpoch { expected, actual } => write!(
                formatter,
                "TCB enforcement gate accepts only canonical host_epoch {expected}, got {actual}"
            ),
            Self::EnvelopeSchema(err) => write!(formatter, "SABI envelope schema error: {err}"),
            Self::UnexpectedServiceOrMethod { service, method } => write!(
                formatter,
                "unexpected mediated driver service={service:?} method={method:?}"
            ),
            Self::MissingEnvelopeContext => {
                formatter.write_str("mediated envelope missing required request/response context")
            }
            Self::InvalidIdempotencyKeyLength { actual } => write!(
                formatter,
                "mediated dispatch requires 16-byte idempotency_key, got {actual} bytes"
            ),
            Self::DeadlineExpired {
                deadline_monotonic_ns,
                now_monotonic_ns,
            } => write!(
                formatter,
                "request deadline {deadline_monotonic_ns} expired at monotonic {now_monotonic_ns}"
            ),
            Self::MissingActivityContext => {
                formatter.write_str("mediated dispatch requires non-empty activity_context")
            }
            Self::StaleProcessIncarnation {
                process_id,
                expected,
                actual,
            } => write!(
                formatter,
                "stale or unknown process incarnation for {process_id:?}: expected {expected:?}, got {actual:?}"
            ),
            Self::UnknownCredential(id) => {
                write!(formatter, "unknown TCB credential handle {:02x?}", &id[..4])
            }
            Self::DriverPayload(err) => write!(formatter, "invalid DriverProvider payload: {err}"),
            Self::OperationOrReservationBindingMismatch(reason) => {
                write!(
                    formatter,
                    "operation/reservation binding mismatch: {reason}"
                )
            }
            Self::Identity(err) => write!(formatter, "identity verification rejected: {err}"),
            Self::Capability(err) => write!(formatter, "capability authorization rejected: {err}"),
            Self::Resource(err) => write!(formatter, "resource authority rejected: {err}"),
            Self::Operation(err) => write!(formatter, "operation fence rejected: {err}"),
            Self::UntrustedUsageMeterSigner { expected, actual } => write!(
                formatter,
                "usage receipt signed by untrusted principal {actual:?} (expected TCB meter {expected:?})"
            ),
            Self::ForbiddenIsolationElevation(reason) => {
                write!(
                    formatter,
                    "forbidden provider isolation elevation: {reason}"
                )
            }
            Self::IdempotencyKeyPayloadConflict { idempotency_key } => write!(
                formatter,
                "idempotency key {idempotency_key:?} reused with conflicting dispatch payload"
            ),
            Self::BlindRetryWithNewIdempotencyKeyForbidden {
                operation_id,
                expected_idempotency_key,
                attempted_idempotency_key,
            } => write!(
                formatter,
                "blind retry of EffectUnknown operation {operation_id:?} with new key {attempted_idempotency_key:?} forbidden (must reconcile with original {expected_idempotency_key:?})"
            ),
            Self::OperationAwaitingReconciliation {
                operation_id,
                original_idempotency_key,
            } => write!(
                formatter,
                "operation {operation_id:?} is EffectUnknown/Quarantined and awaits reconciliation under {original_idempotency_key:?}"
            ),
            Self::UnknownOperation(op_id) => {
                write!(formatter, "operation {op_id:?} is unknown to TCB gate")
            }
        }
    }
}

impl Error for BoundaryViolation {}

impl BoundaryViolation {
    /// Deterministically maps any TCB [`BoundaryViolation`] to a wire-safe
    /// [`SabiFailure`] with explicit retry/reconciliation guidance.
    #[must_use]
    pub fn to_sabi_failure(&self) -> SabiFailure {
        let (code, retry, message) = match self {
            Self::NonCanonicalHostEpoch { .. } => (
                SabiErrorCode::AbiVersion,
                RetryDirective::DoNotRetry,
                "TCB enforcement gate requires canonical host SABI epoch",
            ),
            Self::EnvelopeSchema(err) => return err.to_sabi_failure(),
            Self::DeadlineExpired { .. } => (
                SabiErrorCode::Deadline,
                RetryDirective::DoNotRetry,
                "mediated driver call deadline expired",
            ),
            Self::StaleProcessIncarnation { .. }
            | Self::Resource(ResourceAuthorityError::StaleDriver)
            | Self::Operation(OperationError::InvalidGeneration) => (
                SabiErrorCode::Fenced,
                RetryDirective::DoNotRetry,
                "stale process, driver, or operation generation fenced at TCB gate",
            ),
            Self::Identity(_)
            | Self::Capability(_)
            | Self::UnknownCredential(_)
            | Self::UntrustedUsageMeterSigner { .. }
            | Self::ForbiddenIsolationElevation(_) => (
                SabiErrorCode::Rights,
                RetryDirective::DoNotRetry,
                "identity, capability, meter authority, or isolation check rejected at TCB gate",
            ),
            Self::Resource(
                ResourceAuthorityError::InsufficientCredit { .. }
                | ResourceAuthorityError::UsageExceedsUpperBound { .. },
            ) => (
                SabiErrorCode::Budget,
                RetryDirective::DoNotRetry,
                "resource reservation credit or upper bound exceeded",
            ),
            Self::IdempotencyKeyPayloadConflict { .. }
            | Self::Resource(ResourceAuthorityError::IdempotencyConflict) => (
                SabiErrorCode::Conflict,
                RetryDirective::DoNotRetry,
                "idempotency key rebound to conflicting payload",
            ),
            Self::BlindRetryWithNewIdempotencyKeyForbidden { .. }
            | Self::OperationAwaitingReconciliation { .. } => (
                SabiErrorCode::EffectUnknown,
                RetryDirective::QueryOperationOrRetrySameIdempotencyKey,
                "operation effect is unknown and reservation is quarantined; reconcile with original IdempotencyKey",
            ),
            Self::EmptySecretCredential
            | Self::UnexpectedServiceOrMethod { .. }
            | Self::MissingEnvelopeContext
            | Self::InvalidIdempotencyKeyLength { .. }
            | Self::MissingActivityContext
            | Self::DriverPayload(_)
            | Self::OperationOrReservationBindingMismatch(_)
            | Self::Resource(_)
            | Self::Operation(_)
            | Self::UnknownOperation(_) => (
                SabiErrorCode::InvalidArgument,
                RetryDirective::DoNotRetry,
                "invalid mediated driver request or state transition rejected at TCB gate",
            ),
        };
        SabiFailure {
            code: code.into(),
            retry: retry.into(),
            safe_message: message.to_owned(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GateDispatchLifecycle {
    Active,
    Completed,
    UncertainQuarantined {
        quarantine_receipt: QuarantineReceipt,
        crash_receipt_id: ReceiptId,
    },
    Reconciled {
        finalization_receipt: FinalizationReceipt,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FencedDispatchRecord {
    semantic_event_id: SemanticEventId,
    idempotency_key: IdempotencyKey,
    reservation_id: ReservationId,
    call_id: CallId,
    allow_verified_isolation: bool,
    dispatch_receipt: AuthorizedDriverDispatchReceipt,
    lifecycle: GateDispatchLifecycle,
}

#[derive(Default)]
struct GateInternalState {
    secret_vault: HashMap<[u8; CREDENTIAL_ID_BYTES], SecretCredential>,
    active_processes: HashMap<TaskParticipantId, Generation>,
    dispatches_by_operation: HashMap<OperationId, FencedDispatchRecord>,
    operation_by_idempotency: HashMap<IdempotencyKey, OperationId>,
}

/// Layer 2 TCB-Edge Enforcement Gateway (`ADR-0027` section 2.2.2).
///
/// Sits strictly between any out-of-core [`OutOfCoreCompatShimRuntime`] and the
/// Layer 1 core authorities (`IdentityAuthority`, `CapabilityAuthority`,
/// `ResourceAuthority`, `OperationRegistry`). Accepts ONLY canonical `host_epoch`
/// SABI frames (`0` historical `client_epoch` branches).
pub struct TrustedEnforcementGate {
    host_epoch: u32,
    identity: Arc<IdentityAuthority>,
    capability: Arc<CapabilityAuthority>,
    resource: Arc<ResourceAuthority>,
    operations: Arc<OperationRegistry>,
    trusted_meter_principal: PrincipalId,
    state: Mutex<GateInternalState>,
}

impl TrustedEnforcementGate {
    /// Creates a new TCB-edge enforcement gateway pinned to canonical `host_epoch`.
    ///
    /// # Panics
    ///
    /// Panics if `host_epoch == 0`.
    #[must_use]
    pub fn new(
        host_epoch: u32,
        identity: Arc<IdentityAuthority>,
        capability: Arc<CapabilityAuthority>,
        resource: Arc<ResourceAuthority>,
        operations: Arc<OperationRegistry>,
        trusted_meter_principal: PrincipalId,
    ) -> Self {
        assert!(host_epoch > 0, "host_epoch must be non-zero");
        Self {
            host_epoch,
            identity,
            capability,
            resource,
            operations,
            trusted_meter_principal,
            state: Mutex::new(GateInternalState::default()),
        }
    }

    /// Canonical host major SABI epoch accepted by this TCB gate.
    #[must_use]
    pub const fn host_epoch(&self) -> u32 {
        self.host_epoch
    }

    /// Borrows the underlying [`OperationRegistry`].
    #[must_use]
    pub fn operations(&self) -> &OperationRegistry {
        &self.operations
    }

    /// Seals a [`SecretCredential`] inside the TCB gate's private credential vault
    /// (`[DRV-CRED-001]`).
    pub fn register_secret_credential(&self, credential: SecretCredential) {
        let mut guard = self.lock_state();
        guard
            .secret_vault
            .insert(credential.credential_id(), credential);
    }

    /// Checks whether `wire` contains any registered raw secret sequence (`[DRV-CRED-001]`
    /// audit helper; never reveals the secret bytes).
    #[must_use]
    pub fn wire_contains_any_registered_secret(&self, wire: &[u8]) -> bool {
        let guard = self.lock_state();
        guard
            .secret_vault
            .values()
            .any(|cred| cred.appears_in_wire_slice(wire))
    }

    /// Registers or advances the active incarnation `Generation` of `process_id`
    /// (`[DRV-BOUND-001]`).
    pub fn register_process_incarnation(
        &self,
        process_id: TaskParticipantId,
        generation: Generation,
    ) {
        let mut guard = self.lock_state();
        guard.active_processes.insert(process_id, generation);
    }

    /// Rotates `process_id` to `next_generation`, immediately fencing any older
    /// `process_generation` (`[DRV-BOUND-001]`).
    ///
    /// # Errors
    ///
    /// Fails closed if `process_id` is unknown or `next_generation <= current`.
    pub fn rotate_process_incarnation(
        &self,
        process_id: TaskParticipantId,
        next_generation: Generation,
    ) -> Result<Generation, BoundaryViolation> {
        let mut guard = self.lock_state();
        let current = guard.active_processes.get(&process_id).copied();
        let Some(curr_gen) = current else {
            return Err(BoundaryViolation::StaleProcessIncarnation {
                process_id,
                expected: None,
                actual: next_generation,
            });
        };
        if next_generation.get() <= curr_gen.get() {
            return Err(BoundaryViolation::StaleProcessIncarnation {
                process_id,
                expected: Some(curr_gen),
                actual: next_generation,
            });
        }
        guard.active_processes.insert(process_id, next_generation);
        Ok(next_generation)
    }

    /// Admits and commits a mediated driver dispatch using strict `ADR-0013`
    /// `verify-then-commit` ordering across Identity, Capability, Resource,
    /// Process/Driver incarnation fences, and Operation state machines.
    ///
    /// Accepts ONLY canonical `self.host_epoch` wire frames (`0` historical
    /// `client_epoch` branches).
    ///
    /// # Errors
    ///
    /// Fails closed with [`BoundaryViolation`] on any non-canonical epoch,
    /// expired deadline, missing `activity_context`, stale process/driver
    /// incarnation, invalid/tampered signature, invalid/revoked/attenuated/exhausted
    /// capability, invalid/missing/hijacked/already-active reservation, or blind
    /// retry of an `EffectUnknown` operation with a new `IdempotencyKey`.
    #[allow(clippy::too_many_lines)]
    pub fn admit_mediated_driver_dispatch(
        &self,
        canonical_host_envelope_wire: &[u8],
        auth: &MediatedDriverDispatchAuth,
    ) -> Result<AuthorizedDriverDispatchReceipt, BoundaryViolation> {
        // 1. Canonical host-epoch SABI Envelope check (ZERO historical epoch branches in TCB).
        let envelope = self.decode_canonical_host_envelope(canonical_host_envelope_wire)?;
        if envelope.service != DRIVER_PROVIDER_SERVICE
            || envelope.method != REGISTER_PROVIDER_METHOD
        {
            return Err(BoundaryViolation::UnexpectedServiceOrMethod {
                service: envelope.service,
                method: envelope.method,
            });
        }

        let request_id: [u8; REQUEST_ID_BYTES] =
            envelope.request_id.as_slice().try_into().map_err(|_| {
                BoundaryViolation::EnvelopeSchema(CompatibilityError::InvalidRequestIdLength {
                    actual: envelope.request_id.len(),
                })
            })?;

        let Some(envelope::CommonContext::RequestContext(req_ctx)) = &envelope.common_context
        else {
            return Err(BoundaryViolation::MissingEnvelopeContext);
        };

        let idemp_bytes: [u8; REQUEST_ID_BYTES] =
            req_ctx.idempotency_key.as_slice().try_into().map_err(|_| {
                BoundaryViolation::InvalidIdempotencyKeyLength {
                    actual: req_ctx.idempotency_key.len(),
                }
            })?;
        let idempotency_key = IdempotencyKey::from_bytes(idemp_bytes);

        if req_ctx.deadline_monotonic_ns == 0
            || auth.now_monotonic_ns > req_ctx.deadline_monotonic_ns
        {
            return Err(BoundaryViolation::DeadlineExpired {
                deadline_monotonic_ns: req_ctx.deadline_monotonic_ns,
                now_monotonic_ns: auth.now_monotonic_ns,
            });
        }
        if req_ctx.activity_context.is_empty() {
            return Err(BoundaryViolation::MissingActivityContext);
        }

        // 2. Decode canonical host-epoch DriverProviderRequest payload.
        let normalized_payload = normalize_host_driver_request(&envelope.payload, self.host_epoch)?;
        let decoded_req = decode_request_proto(&normalized_payload)
            .map_err(|err| BoundaryViolation::DriverPayload(DriverShimError::FrozenProto(err)))?;
        let DecodedProviderRequest::Register(spec) = decoded_req else {
            return Err(BoundaryViolation::UnexpectedServiceOrMethod {
                service: envelope.service,
                method: envelope.method,
            });
        };

        let semantic_event_id = compute_dispatch_semantic_event_id(
            request_id,
            idempotency_key,
            req_ctx.deadline_monotonic_ns,
            &req_ctx.activity_context,
            &spec,
            auth,
        );

        let mut guard = self.lock_state();

        // 3. Uncertainty & Idempotency Replay / Conflict Gate ([KABI-IDEM-001], [KABI-UNCERTAIN-001]).
        if let Some(&existing_op_id) = guard.operation_by_idempotency.get(&idempotency_key) {
            let existing = guard
                .dispatches_by_operation
                .get(&existing_op_id)
                .copied()
                .ok_or(BoundaryViolation::UnknownOperation(existing_op_id))?;
            if existing.semantic_event_id != semantic_event_id {
                return Err(BoundaryViolation::IdempotencyKeyPayloadConflict { idempotency_key });
            }
            if matches!(
                existing.lifecycle,
                GateDispatchLifecycle::UncertainQuarantined { .. }
            ) {
                return Err(BoundaryViolation::OperationAwaitingReconciliation {
                    operation_id: existing_op_id,
                    original_idempotency_key: existing.idempotency_key,
                });
            }
            let mut replayed = existing.dispatch_receipt;
            replayed.replayed = true;
            return Ok(replayed);
        }

        if let Some(existing) = guard.dispatches_by_operation.get(&spec.operation_id) {
            if matches!(
                existing.lifecycle,
                GateDispatchLifecycle::UncertainQuarantined { .. }
            ) && existing.idempotency_key != idempotency_key
            {
                return Err(
                    BoundaryViolation::BlindRetryWithNewIdempotencyKeyForbidden {
                        operation_id: spec.operation_id,
                        expected_idempotency_key: existing.idempotency_key,
                        attempted_idempotency_key: idempotency_key,
                    },
                );
            }
            return Err(BoundaryViolation::Operation(
                OperationError::DuplicateOperation,
            ));
        }

        // 4. VERIFY PHASE (zero authority state mutation).
        let active_proc_gen = guard.active_processes.get(&auth.process_id).copied();
        if active_proc_gen != Some(auth.process_generation) {
            return Err(BoundaryViolation::StaleProcessIncarnation {
                process_id: auth.process_id,
                expected: active_proc_gen,
                actual: auth.process_generation,
            });
        }

        let tcb_credential_commitment = match auth.credential_id {
            Some(cred_id) => {
                let cred = guard
                    .secret_vault
                    .get(&cred_id)
                    .ok_or(BoundaryViolation::UnknownCredential(cred_id))?;
                Some(cred.tcb_commitment_digest(spec.operation_id, auth.driver_fencing_token))
            }
            None => None,
        };

        let verified_signer = self
            .identity
            .verify_semantic_signature(VerifySemanticSignatureRequest {
                event_id: semantic_event_id,
                issuer: auth.caller_principal,
                control_domain_id: auth.caller_control_domain,
                key_id: auth.caller_key_id,
                signature: auth.caller_signature,
                admitted_at_ms: auth.admitted_at_ms,
            })
            .map_err(BoundaryViolation::Identity)?;

        self.capability
            .authorize_semantic(AuthorizeSemanticRequest {
                handle: auth.capability_handle,
                signer: verified_signer,
                target: auth.capability_target,
                required_right: auth.required_right,
                purpose_digest: auth.purpose_digest,
                admitted_at_ms: auth.admitted_at_ms,
            })
            .map_err(BoundaryViolation::Capability)?;

        let permit_record = self
            .resource
            .inspect_permit_binding(auth.reservation_id)
            .map_err(BoundaryViolation::Resource)?;
        if permit_record.operation_id != spec.operation_id
            || permit_record.call_id != auth.call_id
            || permit_record.driver_id != auth.driver_id
            || permit_record.driver_generation != auth.driver_generation
            || permit_record.driver_fencing_token != auth.driver_fencing_token
            || permit_record.activation_token != auth.activation_token
        {
            return Err(BoundaryViolation::Resource(
                ResourceAuthorityError::ReservationBindingMismatch,
            ));
        }

        let probe_handle = OperationHandle {
            operation_id: spec.operation_id,
            generation: spec.generation,
        };
        if self.operations.inspect(probe_handle).is_ok() {
            return Err(BoundaryViolation::Operation(
                OperationError::DuplicateOperation,
            ));
        }

        // 5. COMMIT PHASE (executes only after 100% of Verify Phase succeeded).
        let capability_receipt = self
            .capability
            .consume(ConsumeCapabilityRequest {
                handle: auth.capability_handle,
                signer: verified_signer,
                target: auth.capability_target,
                required_right: auth.required_right,
                purpose_digest: auth.purpose_digest,
                idempotency_key,
                consumed_at_ms: auth.admitted_at_ms,
            })
            .map_err(BoundaryViolation::Capability)?
            .receipt();

        let activation_receipt = self
            .resource
            .activate(ActivateReservationRequest {
                reservation_id: auth.reservation_id,
                call_id: auth.call_id,
                operation_id: spec.operation_id,
                driver_id: auth.driver_id,
                driver_generation: auth.driver_generation,
                driver_fencing_token: auth.driver_fencing_token,
                activation_token: auth.activation_token,
                activated_at_ms: auth.admitted_at_ms,
            })
            .map_err(BoundaryViolation::Resource)?
            .receipt();

        let op_handle = self
            .operations
            .register(spec)
            .map_err(BoundaryViolation::Operation)?;
        let callback_ticket = self
            .operations
            .dispatch(op_handle, auth.callback_id)
            .map_err(BoundaryViolation::Operation)?;

        let receipt = AuthorizedDriverDispatchReceipt {
            replayed: false,
            request_id,
            idempotency_key,
            capability_receipt,
            activation_receipt,
            callback_ticket,
            tcb_credential_commitment,
        };

        guard
            .operation_by_idempotency
            .insert(idempotency_key, spec.operation_id);
        guard.dispatches_by_operation.insert(
            spec.operation_id,
            FencedDispatchRecord {
                semantic_event_id,
                idempotency_key,
                reservation_id: auth.reservation_id,
                call_id: auth.call_id,
                allow_verified_isolation: auth.allow_verified_isolation,
                dispatch_receipt: receipt,
                lifecycle: GateDispatchLifecycle::Active,
            },
        );

        Ok(receipt)
    }

    /// Verifies a canonical `host_epoch` driver completion envelope together with
    /// a TCB-meter-signed [`SignedMeteredUsageProof`], enforces `[TASK-PROVIDER-001]`
    /// isolation honesty, records metered consumption + double-entry settlement
    /// in `ResourceAuthority`, and completes the fenced `Operation` in
    /// `OperationRegistry`.
    ///
    /// # Errors
    ///
    /// Fails closed on non-canonical host epoch, forged/unsigned/wrong-signer/tampered
    /// usage proofs, over-budget usage (`cumulative_usage > upper_bound`),
    /// `[TASK-PROVIDER-001]` forged `PROVIDER_ISOLATION_VERIFIED_TAG` elevation,
    /// or fenced/unknown operations.
    #[allow(clippy::too_many_lines)]
    pub fn complete_mediated_driver_call(
        &self,
        canonical_host_response_wire: &[u8],
        usage_proof: &SignedMeteredUsageProof,
    ) -> Result<AuthorizedDriverCompletionReceipt, BoundaryViolation> {
        let envelope = self.decode_canonical_host_envelope(canonical_host_response_wire)?;
        if envelope.service != DRIVER_PROVIDER_SERVICE
            || envelope.method != COMPLETE_PROVIDER_METHOD
        {
            return Err(BoundaryViolation::UnexpectedServiceOrMethod {
                service: envelope.service,
                method: envelope.method,
            });
        }
        if !matches!(
            envelope.common_context,
            Some(envelope::CommonContext::ResponseContext(_))
        ) {
            return Err(BoundaryViolation::MissingEnvelopeContext);
        }

        let normalized_resp = normalize_host_driver_response(&envelope.payload, self.host_epoch)?;
        let decoded_resp = decode_response_proto(&normalized_resp)
            .map_err(|err| BoundaryViolation::DriverPayload(DriverShimError::FrozenProto(err)))?;
        let DecodedProviderResponse::CompleteAck(exec_receipt) = decoded_resp else {
            return Err(BoundaryViolation::UnexpectedServiceOrMethod {
                service: envelope.service,
                method: envelope.method,
            });
        };

        let mut guard = self.lock_state();
        let record = guard
            .dispatches_by_operation
            .get(&usage_proof.operation_id)
            .copied()
            .ok_or(BoundaryViolation::UnknownOperation(
                usage_proof.operation_id,
            ))?;

        if matches!(
            record.lifecycle,
            GateDispatchLifecycle::UncertainQuarantined { .. }
        ) {
            return Err(BoundaryViolation::OperationAwaitingReconciliation {
                operation_id: usage_proof.operation_id,
                original_idempotency_key: record.idempotency_key,
            });
        }

        if record.reservation_id != usage_proof.reservation_id
            || record.dispatch_receipt.activation_receipt.receipt_id
                != usage_proof.activation_receipt_id
            || exec_receipt.receipt_id != usage_proof.execution_receipt_id
        {
            return Err(BoundaryViolation::OperationOrReservationBindingMismatch(
                "completion usage proof does not match active reservation or execution receipt",
            ));
        }

        // [TASK-PROVIDER-001] TCB anti-elevation enforcement:
        if !record.allow_verified_isolation
            && (exec_receipt.isolation_marker == PROVIDER_ISOLATION_VERIFIED_TAG
                || exec_receipt.isolation_posture.is_verified()
                || exec_receipt.attestation.tenant_isolated
                || exec_receipt.attestation.session_ephemeral
                || exec_receipt.attestation.cache_partitioned
                || exec_receipt.attestation.zero_data_retention
                || exec_receipt.attestation.attestation_digest != [0_u8; DIGEST_BYTES])
        {
            return Err(BoundaryViolation::ForbiddenIsolationElevation(
                "unverified or legacy driver shim attempted to claim VERIFIED isolation or non-zero attestation digest",
            ));
        }

        // [DRV-ACTIVE-001] Verify TCB meter authority signature before touching ResourceAuthority.
        self.verify_tcb_meter_usage_proof(usage_proof)?;

        let consumption_receipt = self
            .resource
            .consume(ConsumeReservationRequest {
                reservation_id: usage_proof.reservation_id,
                operation_id: usage_proof.operation_id,
                activation_receipt_id: usage_proof.activation_receipt_id,
                sequence: usage_proof.sequence,
                cumulative_usage: usage_proof.cumulative_usage,
                consumed_at_ms: usage_proof.settled_at_ms,
            })
            .map_err(BoundaryViolation::Resource)?
            .receipt();

        let finalization_receipt = self
            .resource
            .finalize_reservation(FinalizeReservationRequest {
                reservation_id: usage_proof.reservation_id,
                operation_id: usage_proof.operation_id,
                activation_receipt_id: usage_proof.activation_receipt_id,
                effect_closed_proof_digest: usage_proof.effect_closed_proof_digest,
                final_seq: usage_proof.sequence,
                final_usage: usage_proof.cumulative_usage,
                finalized_at_ms: usage_proof.settled_at_ms,
            })
            .map_err(BoundaryViolation::Resource)?
            .receipt();

        let completion_decision = self
            .operations
            .complete(
                record.dispatch_receipt.callback_ticket,
                exec_receipt.outcome,
            )
            .map_err(BoundaryViolation::Operation)?;

        if let Some(slot) = guard
            .dispatches_by_operation
            .get_mut(&usage_proof.operation_id)
        {
            slot.lifecycle = GateDispatchLifecycle::Completed;
        }

        Ok(AuthorizedDriverCompletionReceipt {
            consumption_receipt,
            finalization_receipt,
            completion_decision,
            provider_execution_receipt: exec_receipt,
        })
    }

    /// Fences an in-flight operation upon out-of-core shim or driver crash
    /// (`[DRV-LIFE-001]`, `[KABI-UNCERTAIN-001]`):
    /// 1. freezes the active `Reservation` into `ReservationState::Quarantined`
    ///    inside `ResourceAuthority`;
    /// 2. transitions the in-flight `Operation` into
    ///    `OperationState::EffectUnknown { receipt_id: crash_receipt_id }` inside
    ///    `OperationRegistry`;
    /// 3. bars any blind retry with a new `IdempotencyKey` until explicit
    ///    reconciliation under the original 128-bit `IdempotencyKey`.
    ///
    /// # Errors
    ///
    /// Fails closed if `operation_id` is unknown, already completed/reconciled,
    /// or `ResourceAuthority::quarantine` / `OperationRegistry::complete` rejects
    /// the transition.
    pub fn fence_shim_or_driver_crash(
        &self,
        operation_id: OperationId,
        crash_receipt_id: ReceiptId,
        reason_digest: [u8; DIGEST_BYTES],
        crashed_at_ms: u64,
    ) -> Result<CrashQuarantineReceipt, BoundaryViolation> {
        let mut guard = self.lock_state();
        let record = guard
            .dispatches_by_operation
            .get(&operation_id)
            .copied()
            .ok_or(BoundaryViolation::UnknownOperation(operation_id))?;

        if let GateDispatchLifecycle::UncertainQuarantined {
            quarantine_receipt, ..
        } = record.lifecycle
        {
            let snapshot = self
                .operations
                .inspect(record.dispatch_receipt.callback_ticket.operation)
                .map_err(BoundaryViolation::Operation)?;
            return Ok(CrashQuarantineReceipt {
                operation_snapshot: snapshot,
                quarantine_receipt,
                original_idempotency_key: record.idempotency_key,
            });
        }

        let quarantine_receipt = self
            .resource
            .quarantine(QuarantineReservationRequest {
                reservation_id: record.reservation_id,
                operation_id,
                activation_receipt_id: record.dispatch_receipt.activation_receipt.receipt_id,
                reason_digest,
                quarantined_at_ms: crashed_at_ms,
            })
            .map_err(BoundaryViolation::Resource)?
            .receipt();

        let _decision = self
            .operations
            .complete(
                record.dispatch_receipt.callback_ticket,
                CompletionOutcome::EffectUnknown {
                    receipt_id: crash_receipt_id,
                },
            )
            .map_err(BoundaryViolation::Operation)?;

        let snapshot = self
            .operations
            .inspect(record.dispatch_receipt.callback_ticket.operation)
            .map_err(BoundaryViolation::Operation)?;
        debug_assert_eq!(
            snapshot.state,
            OperationState::EffectUnknown {
                receipt_id: crash_receipt_id
            }
        );

        if let Some(slot) = guard.dispatches_by_operation.get_mut(&operation_id) {
            slot.lifecycle = GateDispatchLifecycle::UncertainQuarantined {
                quarantine_receipt,
                crash_receipt_id,
            };
        }

        Ok(CrashQuarantineReceipt {
            operation_snapshot: snapshot,
            quarantine_receipt,
            original_idempotency_key: record.idempotency_key,
        })
    }

    /// Reconciles an `EffectUnknown` + `Quarantined` operation (`[KABI-UNCERTAIN-001]`).
    ///
    /// Strictly requires the **exact original 128-bit `IdempotencyKey`** that
    /// initiated the operation plus a TCB-meter-signed [`SignedMeteredUsageProof`].
    /// Any attempt to reconcile using a new `IdempotencyKey` fails closed with
    /// [`BoundaryViolation::BlindRetryWithNewIdempotencyKeyForbidden`].
    ///
    /// # Errors
    ///
    /// Fails closed on unknown operation, mismatched `IdempotencyKey`, invalid
    /// meter signature, or `ResourceAuthority::finalize_reservation` rejection.
    pub fn reconcile_uncertain_operation(
        &self,
        operation_id: OperationId,
        presented_idempotency_key: IdempotencyKey,
        reconciliation_usage_proof: &SignedMeteredUsageProof,
    ) -> Result<ReconciledUncertaintyReceipt, BoundaryViolation> {
        let mut guard = self.lock_state();
        let record = guard
            .dispatches_by_operation
            .get(&operation_id)
            .copied()
            .ok_or(BoundaryViolation::UnknownOperation(operation_id))?;

        if presented_idempotency_key != record.idempotency_key {
            return Err(
                BoundaryViolation::BlindRetryWithNewIdempotencyKeyForbidden {
                    operation_id,
                    expected_idempotency_key: record.idempotency_key,
                    attempted_idempotency_key: presented_idempotency_key,
                },
            );
        }

        match record.lifecycle {
            GateDispatchLifecycle::Reconciled {
                finalization_receipt,
            } => {
                return Ok(ReconciledUncertaintyReceipt {
                    replayed: true,
                    operation_id,
                    idempotency_key: record.idempotency_key,
                    finalization_receipt,
                });
            }
            GateDispatchLifecycle::UncertainQuarantined {
                crash_receipt_id, ..
            } => {
                if reconciliation_usage_proof.reservation_id != record.reservation_id
                    || reconciliation_usage_proof.operation_id != operation_id
                    || reconciliation_usage_proof.activation_receipt_id
                        != record.dispatch_receipt.activation_receipt.receipt_id
                    || reconciliation_usage_proof.execution_receipt_id != crash_receipt_id
                {
                    return Err(BoundaryViolation::OperationOrReservationBindingMismatch(
                        "reconciliation usage proof does not match quarantined operation/reservation",
                    ));
                }
            }
            GateDispatchLifecycle::Active | GateDispatchLifecycle::Completed => {
                return Err(BoundaryViolation::Operation(OperationError::InvalidState));
            }
        }

        self.verify_tcb_meter_usage_proof(reconciliation_usage_proof)?;

        let finalization_receipt = self
            .resource
            .finalize_reservation(FinalizeReservationRequest {
                reservation_id: record.reservation_id,
                operation_id,
                activation_receipt_id: record.dispatch_receipt.activation_receipt.receipt_id,
                effect_closed_proof_digest: reconciliation_usage_proof.effect_closed_proof_digest,
                final_seq: reconciliation_usage_proof.sequence,
                final_usage: reconciliation_usage_proof.cumulative_usage,
                finalized_at_ms: reconciliation_usage_proof.settled_at_ms,
            })
            .map_err(BoundaryViolation::Resource)?
            .receipt();

        if let Some(slot) = guard.dispatches_by_operation.get_mut(&operation_id) {
            slot.lifecycle = GateDispatchLifecycle::Reconciled {
                finalization_receipt,
            };
        }

        Ok(ReconciledUncertaintyReceipt {
            replayed: false,
            operation_id,
            idempotency_key: record.idempotency_key,
            finalization_receipt,
        })
    }

    fn verify_tcb_meter_usage_proof(
        &self,
        proof: &SignedMeteredUsageProof,
    ) -> Result<(), BoundaryViolation> {
        if proof.meter_principal != self.trusted_meter_principal {
            return Err(BoundaryViolation::UntrustedUsageMeterSigner {
                expected: self.trusted_meter_principal,
                actual: proof.meter_principal,
            });
        }
        let digest = compute_metered_usage_proof_digest(
            proof.reservation_id,
            proof.operation_id,
            proof.activation_receipt_id,
            proof.execution_receipt_id,
            proof.sequence,
            proof.cumulative_usage,
            proof.effect_closed_proof_digest,
        );
        self.identity
            .verify_semantic_authority_signature(VerifySemanticAuthoritySignatureRequest {
                message_digest: digest,
                issuer: proof.meter_principal,
                control_domain_id: proof.meter_control_domain,
                key_id: proof.meter_key_id,
                signature: proof.meter_signature,
                verified_at_ms: proof.settled_at_ms,
            })
            .map_err(BoundaryViolation::Identity)?;
        Ok(())
    }

    fn decode_canonical_host_envelope(&self, wire: &[u8]) -> Result<Envelope, BoundaryViolation> {
        let envelope = Envelope::decode(wire).map_err(|err| {
            BoundaryViolation::EnvelopeSchema(CompatibilityError::MalformedProtobuf(
                err.to_string(),
            ))
        })?;
        let actual_major = envelope
            .schema
            .as_ref()
            .ok_or(BoundaryViolation::EnvelopeSchema(
                CompatibilityError::MissingSchemaIdentity,
            ))?
            .major;
        if actual_major != self.host_epoch {
            return Err(BoundaryViolation::NonCanonicalHostEpoch {
                expected: self.host_epoch,
                actual: actual_major,
            });
        }
        let strict_host_policy = VersionCompatibilityPolicy::new(self.host_epoch, &[]);
        strict_host_policy
            .adjudicate_envelope(&envelope)
            .map_err(BoundaryViolation::EnvelopeSchema)?;
        Ok(envelope)
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, GateInternalState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Complete raw wire transcript captured by [`OutOfCoreCompatShimRuntime`] across
/// every ingress/egress boundary so `[DRV-CRED-001]` security audits can scan
/// 100% of bytes touching the unprivileged shim.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WireCaptureTranscript {
    /// Raw client-epoch `Envelope` frames entering [`SabiEpochAdapterCell`].
    pub client_ingress_frames: Vec<Vec<u8>>,
    /// Raw legacy `minicbor` or historical `DriverProvider` payload bytes entering [`DriverProtocolShim`].
    pub driver_ingress_frames: Vec<Vec<u8>>,
    /// Raw canonical host-epoch `Envelope` frames exiting the shim toward [`TrustedEnforcementGate`].
    pub host_egress_frames: Vec<Vec<u8>>,
    /// Raw canonical host-epoch `Envelope` response frames entering the shim from [`TrustedEnforcementGate`].
    pub host_ingress_frames: Vec<Vec<u8>>,
    /// Raw legacy `minicbor` or historical `DriverProvider` response bytes exiting [`DriverProtocolShim`].
    pub driver_egress_frames: Vec<Vec<u8>>,
    /// Raw client-epoch `Envelope` response frames exiting [`SabiEpochAdapterCell`] back to the client.
    pub client_egress_frames: Vec<Vec<u8>>,
}

impl WireCaptureTranscript {
    /// Returns every raw byte frame ever observed by the shim across all 6 directions.
    #[must_use]
    pub fn all_frames(&self) -> Vec<&[u8]> {
        self.client_ingress_frames
            .iter()
            .chain(&self.driver_ingress_frames)
            .chain(&self.host_egress_frames)
            .chain(&self.host_ingress_frames)
            .chain(&self.driver_egress_frames)
            .chain(&self.client_egress_frames)
            .map(Vec::as_slice)
            .collect()
    }

    /// Scans 100% of captured raw frames for `needle` (`[DRV-CRED-001]` 0-byte
    /// secret leak assertion).
    #[must_use]
    pub fn contains_byte_subsequence(&self, needle: &[u8]) -> bool {
        if needle.is_empty() {
            return false;
        }
        self.all_frames()
            .into_iter()
            .any(|frame| frame.windows(needle.len()).any(|win| win == needle))
    }
}

/// Errors surfaced inside the unprivileged Layer 3 [`OutOfCoreCompatShimRuntime`].
#[derive(Debug)]
pub enum OutOfCoreShimError {
    /// SABI envelope epoch translation failed.
    SabiAdapter(SabiAdapterError),
    /// Driver wire protocol translation failed.
    DriverShim(DriverShimError),
}

impl fmt::Display for OutOfCoreShimError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SabiAdapter(err) => write!(formatter, "out-of-core SABI adapter error: {err}"),
            Self::DriverShim(err) => write!(formatter, "out-of-core driver shim error: {err}"),
        }
    }
}

impl Error for OutOfCoreShimError {}

/// Layer 3 Unprivileged Out-of-Core Compatibility Shim Runtime (`ADR-0027` section 2.2.2).
///
/// Combines [`SabiEpochAdapterCell`] and [`DriverProtocolShim`] outside the TCB:
/// holds zero credentials, zero signing keys, and zero authority handles, and
/// records every raw wire frame in [`WireCaptureTranscript`].
#[derive(Clone, Debug)]
pub struct OutOfCoreCompatShimRuntime {
    sabi_adapter: SabiEpochAdapterCell,
    driver_shim: DriverProtocolShim,
    transcript: WireCaptureTranscript,
}

impl OutOfCoreCompatShimRuntime {
    /// Constructs an unprivileged out-of-core compatibility shim runtime.
    #[must_use]
    pub fn new(sabi_adapter: SabiEpochAdapterCell, driver_shim: DriverProtocolShim) -> Self {
        Self {
            sabi_adapter,
            driver_shim,
            transcript: WireCaptureTranscript::default(),
        }
    }

    /// Borrows the [`SabiEpochAdapterCell`].
    #[must_use]
    pub const fn sabi_adapter(&self) -> &SabiEpochAdapterCell {
        &self.sabi_adapter
    }

    /// Borrows the [`DriverProtocolShim`].
    #[must_use]
    pub const fn driver_shim(&self) -> &DriverProtocolShim {
        &self.driver_shim
    }

    /// Borrows the complete raw wire transcript captured by this shim.
    #[must_use]
    pub const fn transcript(&self) -> &WireCaptureTranscript {
        &self.transcript
    }

    /// Returns `true` only when the driver shim permits honest verified isolation
    /// attestations (always `false` for Stage B legacy `minicbor` or `ForceUnverified`).
    #[must_use]
    pub const fn allows_verified_isolation(&self) -> bool {
        matches!(
            self.driver_shim.trust_ceiling(),
            ProviderTrustCeiling::PreserveHonestAttestation
        )
    }

    /// Translates a historical-epoch client [`Envelope`] carrying a Stage B legacy
    /// `minicbor` driver request payload into a canonical `host_epoch` [`Envelope`]
    /// carrying a frozen `nlos.sabi.DriverProvider` Protobuf payload ready for
    /// [`TrustedEnforcementGate::admit_mediated_driver_dispatch`].
    ///
    /// # Errors
    ///
    /// Fails closed if either [`SabiEpochAdapterCell`] or [`DriverProtocolShim`]
    /// rejects the incoming wire bytes.
    pub fn translate_legacy_client_dispatch_to_host(
        &mut self,
        client_envelope_wire: &[u8],
    ) -> Result<Vec<u8>, OutOfCoreShimError> {
        self.transcript
            .client_ingress_frames
            .push(client_envelope_wire.to_vec());

        let translated = self
            .sabi_adapter
            .translate_client_to_host_wire(client_envelope_wire)
            .map_err(OutOfCoreShimError::SabiAdapter)?;

        self.transcript
            .driver_ingress_frames
            .push(translated.envelope.payload.clone());

        let frozen_driver_payload = self
            .driver_shim
            .legacy_minicbor_request_to_frozen_proto(
                &translated.envelope.method,
                &translated.envelope.payload,
            )
            .map_err(OutOfCoreShimError::DriverShim)?;

        let mut host_envelope = translated.envelope;
        host_envelope.payload = frozen_driver_payload;
        let host_wire = crate::proto_unknown::encode_envelope_with_unknown_fields(
            &host_envelope,
            &translated.preserved_unknown_fields,
        )
        .map_err(|err| OutOfCoreShimError::SabiAdapter(SabiAdapterError::WireScan(err)))?;

        self.transcript.host_egress_frames.push(host_wire.clone());
        Ok(host_wire)
    }

    /// Translates a Stage B legacy `minicbor` driver completion response into a
    /// canonical `host_epoch` [`Envelope`] carrying a frozen
    /// `nlos.sabi.DriverProvider` `CompleteAck` (`PROVIDER_ISOLATION_UNVERIFIED_TAG`)
    /// ready for [`TrustedEnforcementGate::complete_mediated_driver_call`], and
    /// also produces the downgraded `client_epoch` response [`Envelope`] wire.
    ///
    /// # Errors
    ///
    /// Fails closed if legacy `minicbor` decoding or `SabiEpochAdapterCell`
    /// downgrade fails.
    pub fn translate_legacy_driver_completion_bidirectional(
        &mut self,
        request_id: [u8; REQUEST_ID_BYTES],
        legacy_complete_result_cbor: &[u8],
        bridge_ctx: LegacyCompleteBridgeContext,
    ) -> Result<(Vec<u8>, Vec<u8>), OutOfCoreShimError> {
        self.transcript
            .driver_ingress_frames
            .push(legacy_complete_result_cbor.to_vec());

        let frozen_resp_payload = self
            .driver_shim
            .legacy_minicbor_response_to_frozen_proto(
                COMPLETE_PROVIDER_METHOD,
                legacy_complete_result_cbor,
                Some(bridge_ctx),
            )
            .map_err(OutOfCoreShimError::DriverShim)?;

        let host_resp_envelope = Envelope {
            schema: Some(SchemaIdentity {
                name: SABI_ENVELOPE_SCHEMA.to_owned(),
                major: self.sabi_adapter.host_epoch(),
                minor: self.sabi_adapter.rules().host_envelope_minor,
                critical_extension_ids: Vec::new(),
                non_critical_extension_ids: Vec::new(),
            }),
            request_id: request_id.to_vec(),
            service: DRIVER_PROVIDER_SERVICE.to_owned(),
            method: COMPLETE_PROVIDER_METHOD.to_owned(),
            common_context: Some(envelope::CommonContext::ResponseContext(
                SabiResponseContext {
                    correlation_id: request_id.to_vec(),
                    operation: None,
                    receipts: Vec::new(),
                    failure: None,
                },
            )),
            payload: frozen_resp_payload.clone(),
        };
        let host_resp_wire = host_resp_envelope.encode_to_vec();
        self.transcript
            .host_ingress_frames
            .push(host_resp_wire.clone());

        let (_, roundtrip_cbor) = self
            .driver_shim
            .frozen_proto_response_to_legacy_minicbor(&frozen_resp_payload)
            .map_err(OutOfCoreShimError::DriverShim)?;
        self.transcript
            .driver_egress_frames
            .push(roundtrip_cbor.clone());

        let mut host_for_client_down = host_resp_envelope;
        host_for_client_down.payload = roundtrip_cbor;
        let downgraded_client = self
            .sabi_adapter
            .translate_host_to_client_wire(&host_for_client_down.encode_to_vec())
            .map_err(OutOfCoreShimError::SabiAdapter)?;

        self.transcript
            .client_egress_frames
            .push(downgraded_client.wire.clone());

        Ok((host_resp_wire, downgraded_client.wire))
    }
}

/// Helper building a client-epoch `nlos.sabi.v1.Envelope` request frame carrying
/// `request_id`, `idempotency_key`, `deadline_monotonic_ns`, `activity_context`,
/// and arbitrary `payload` bytes.
#[must_use]
#[allow(clippy::too_many_arguments)]
pub fn build_driver_request_envelope(
    sabi_major: u32,
    sabi_minor: u32,
    method: &str,
    request_id: [u8; REQUEST_ID_BYTES],
    idempotency_key: IdempotencyKey,
    deadline_monotonic_ns: u64,
    activity_context: Vec<u8>,
    payload: Vec<u8>,
) -> Envelope {
    Envelope {
        schema: Some(SchemaIdentity {
            name: SABI_ENVELOPE_SCHEMA.to_owned(),
            major: sabi_major,
            minor: sabi_minor,
            critical_extension_ids: Vec::new(),
            non_critical_extension_ids: Vec::new(),
        }),
        request_id: request_id.to_vec(),
        service: DRIVER_PROVIDER_SERVICE.to_owned(),
        method: method.to_owned(),
        common_context: Some(envelope::CommonContext::RequestContext(
            SabiRequestContext {
                caller: None,
                activity_context,
                task_execution_binding: None,
                correlation_id: request_id.to_vec(),
                idempotency_key: idempotency_key.as_bytes().to_vec(),
                deadline_monotonic_ns,
                capability_handles: Vec::new(),
                reservation_handle: None,
                proposal_or_input_digest_sha256: Vec::new(),
            },
        )),
        payload,
    }
}

fn normalize_host_driver_request(
    wire: &[u8],
    host_epoch: u32,
) -> Result<Vec<u8>, BoundaryViolation> {
    let mut req = nlos_schema::sabi::v1::DriverProviderRequest::decode(wire).map_err(|err| {
        BoundaryViolation::EnvelopeSchema(CompatibilityError::MalformedProtobuf(err.to_string()))
    })?;
    let schema = req
        .schema
        .as_mut()
        .ok_or(BoundaryViolation::EnvelopeSchema(
            CompatibilityError::MissingSchemaIdentity,
        ))?;
    if schema.major != host_epoch {
        return Err(BoundaryViolation::NonCanonicalHostEpoch {
            expected: host_epoch,
            actual: schema.major,
        });
    }
    schema.major = 1;
    Ok(req.encode_to_vec())
}

fn normalize_host_driver_response(
    wire: &[u8],
    host_epoch: u32,
) -> Result<Vec<u8>, BoundaryViolation> {
    let mut resp = nlos_schema::sabi::v1::DriverProviderResponse::decode(wire).map_err(|err| {
        BoundaryViolation::EnvelopeSchema(CompatibilityError::MalformedProtobuf(err.to_string()))
    })?;
    let schema = resp
        .schema
        .as_mut()
        .ok_or(BoundaryViolation::EnvelopeSchema(
            CompatibilityError::MissingSchemaIdentity,
        ))?;
    if schema.major != host_epoch {
        return Err(BoundaryViolation::NonCanonicalHostEpoch {
            expected: host_epoch,
            actual: schema.major,
        });
    }
    schema.major = 1;
    Ok(resp.encode_to_vec())
}

#[allow(dead_code)]
fn _nonzero_gen_helper(val: u64) -> Option<Generation> {
    NonZeroU64::new(val).map(Generation::new)
}
