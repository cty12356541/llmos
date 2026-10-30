//! Controlled TCB edge `TrustedDriverEnforcementGate` (`[DRV-SHIM-001]`, `ADR-0027`
//! section 2.2) enforcing zero-bypass authority invariants across untrusted
//! [`crate::driver_shim::DriverProtocolCompatShim`] and `ProviderAdapter` hops:
//! - `DRV-CRED-001`: Redacted credential vault (`CustodiedCredentialRef` fingerprint only).
//! - `SABI-AUTH-001` / `CAP-UNFORGE-001`: Independent `CapabilityAuthority` and
//!   `IdentityAuthority` cryptographic `authorization_digest` verification.
//! - `DRV-ACTIVE-001`: One-shot `OperationId`-bound `ReservationActivationToken`
//!   consumption backed by `ResourceAuthority::activate`.
//! - `DRV-BOUND-001`: Monotonic `fencing_token`, `process_incarnation`, and
//!   `deadline_monotonic_ns` enforcement.
//! - `[TASK-PROVIDER-001]`: Fail-closed `ProviderIsolationAttestationWire` posture
//!   evaluation (`PROVIDER_ISOLATION_UNVERIFIED` on missing guarantees or all-zero digest).
//! - `DRV-LIFE-001` / `KABI-UNCERTAIN-001`: Crash-to-`UNCERTAIN` convergence and
//!   mandatory same-`IdempotencyKey` fenced reconciliation.

use std::collections::HashMap;
use std::error::Error;
use std::fmt;

use nlos_capability::{
    CapabilityAuthority, CapabilityAuthorityError, CapabilityHandle, CapabilityRights,
    CapabilityTarget,
};
use nlos_driver_mock::proto_codec::{
    DecodedProviderRequest, IsolationGuarantee, ProviderIsolationAttestation,
    ProviderIsolationPosture, decode_request_proto,
};
use nlos_identity::{
    Ed25519Signature, IdentityAuthority, IdentityAuthorityError,
    VerifySemanticAuthoritySignatureRequest,
};
use nlos_operation::{
    CallbackTicket, CompletionOutcome, OperationHandle, OperationMachine, OperationSnapshot,
    OperationState,
};
use nlos_resource::{
    ActivateReservationRequest, FencingToken, QuarantineReservationRequest, ResourceAuthority,
    ResourceAuthorityError, RotateDriverRequest,
};
use nlos_schema::decode_driver_provider_response;
use nlos_schema::sabi::v1::{
    DriverProviderCompleteAck, DriverProviderTerminalOutcome, OperationLifecycleState,
    driver_provider_response,
};
use nlos_types::{
    CallId, CallbackId, ControlDomainId, DriverId, Generation, IdempotencyKey, KeyId, OperationId,
    PrincipalId, ReceiptId, ReservationId,
};
use sha2::{Digest, Sha256};

const CREDENTIAL_FINGERPRINT_DOMAIN: &[u8] = b"nlos/compat-shim/credential-fingerprint/v1";
const DRIVER_AUTH_DIGEST_DOMAIN: &[u8] = b"nlos/compat-shim/driver-auth-digest/v1";
const CRASH_UNCERTAIN_RECEIPT_DOMAIN: &[u8] = b"nlos/compat-shim/crash-uncertain-receipt/v1";

/// Redacted handle to a TCB-custodied secret (`DRV-CRED-001`).
///
/// Carries only the 128-bit `credential_id` and its domain-separated SHA-256
/// fingerprint; raw secret bytes never leave [`TrustedDriverEnforcementGate`].
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct CustodiedCredentialRef {
    credential_id: [u8; 16],
    slot_sha256_fingerprint: [u8; 32],
}

impl CustodiedCredentialRef {
    /// Stable 128-bit credential slot identifier.
    #[must_use]
    pub const fn credential_id(self) -> [u8; 16] {
        self.credential_id
    }

    /// Domain-separated SHA-256 fingerprint of the custodied credential slot.
    #[must_use]
    pub const fn slot_sha256_fingerprint(self) -> [u8; 32] {
        self.slot_sha256_fingerprint
    }

    /// Forges a reference with an arbitrary fingerprint (exposed for adversarial
    /// security testing of `TrustedDriverEnforcementGate` rejection).
    #[must_use]
    pub const fn from_raw_parts(
        credential_id: [u8; 16],
        slot_sha256_fingerprint: [u8; 32],
    ) -> Self {
        Self {
            credential_id,
            slot_sha256_fingerprint,
        }
    }
}

impl fmt::Debug for CustodiedCredentialRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CustodiedCredentialRef")
            .field("credential_id", &hex_slice(&self.credential_id))
            .field(
                "slot_sha256_fingerprint",
                &hex_slice(&self.slot_sha256_fingerprint),
            )
            .field("raw_secret", &"<REDACTED_TCB_ONLY>")
            .finish()
    }
}

impl fmt::Display for CustodiedCredentialRef {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "CustodiedCredentialRef(id={}, fp={}, secret=<REDACTED_TCB_ONLY>)",
            hex_slice(&self.credential_id),
            hex_slice(&self.slot_sha256_fingerprint)
        )
    }
}

struct RedactedSecretMaterial {
    secret: Vec<u8>,
    fingerprint: [u8; 32],
}

impl Drop for RedactedSecretMaterial {
    fn drop(&mut self) {
        self.secret.fill(0);
    }
}

/// Redacted wrapper around a 32-byte one-shot `Reservation` activation token (`DRV-ACTIVE-001`).
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct ReservationActivationToken([u8; 32]);

impl ReservationActivationToken {
    /// Wraps a 32-byte activation token issued by `ResourceAuthority::reserve`.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Returns the underlying 32-byte token for authority verification.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for ReservationActivationToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ReservationActivationToken")
            .field(&"<REDACTED_ONE_SHOT_TOKEN>")
            .finish()
    }
}

/// `[TASK-PROVIDER-001]` enforcement mode applied to provider completion attestations.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IsolationEnforcementPolicy {
    /// Overwrites any spoofed claim with `PROVIDER_ISOLATION_UNVERIFIED` whenever
    /// any boolean guarantee is `false` or `attestation_digest` is `[0u8; 32]`.
    TagReceiptFailClosed,
    /// Hard-rejects completion if `[TASK-PROVIDER-001]` evaluates to unverified.
    RequireVerifiedIsolation,
}

/// Authoritative admission bundle verified by [`TrustedDriverEnforcementGate::admit_dispatch`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DriverAdmissionRequest {
    /// Authenticated caller `PrincipalId`.
    pub caller_principal: PrincipalId,
    /// Caller's `ControlDomainId`.
    pub caller_control_domain: ControlDomainId,
    /// Active `SemanticSigning` `KeyId` of the caller.
    pub caller_key_id: KeyId,
    /// Monotonic caller/driver `process_incarnation` fence (`DRV-BOUND-001`).
    pub process_incarnation: Generation,
    /// Authoritative `CapabilityHandle` authorizing this operation.
    pub capability_handle: CapabilityHandle,
    /// Target `Namespace` or `Task` scope of the operation.
    pub target: CapabilityTarget,
    /// Required capability right bitmask.
    pub required_right: CapabilityRights,
    /// Optional capability `purpose_digest` bound at issuance.
    pub purpose_digest: Option<[u8; 32]>,
    /// 128-bit caller `IdempotencyKey` (`KABI-IDEM-001`).
    pub idempotency_key: IdempotencyKey,
    /// Non-zero monotonic execution deadline (`DRV-BOUND-001`).
    pub deadline_monotonic_ns: u64,
    /// Current monotonic clock reading in nanoseconds.
    pub now_monotonic_ns: u64,
    /// Authoritative wall/logical timestamp in milliseconds for key/capability validity.
    pub admitted_at_ms: u64,
    /// Pre-reserved `ReservationId` in `ResourceAuthority`.
    pub reservation_id: ReservationId,
    /// Call identifier bound to the reservation.
    pub call_id: CallId,
    /// Target `DriverId` in `ResourceAuthority`.
    pub driver_id: DriverId,
    /// Expected active `DriverRecord.generation`.
    pub driver_generation: Generation,
    /// Expected active `DriverRecord.fencing_token`.
    pub driver_fencing_token: FencingToken,
    /// One-shot `ReservationActivationToken` (`DRV-ACTIVE-001`).
    pub activation_token: ReservationActivationToken,
    /// Optional TCB-custodied credential reference (`DRV-CRED-001`).
    pub credential_ref: Option<CustodiedCredentialRef>,
    /// Canonical `nlos.sabi.DriverProvider` `Register` wire frame (v1.0 normalized).
    pub canonical_register_wire: Vec<u8>,
    /// Canonical `nlos.sabi.DriverProvider` `Dispatch` wire frame (v1.0 normalized).
    pub canonical_dispatch_wire: Vec<u8>,
    /// 32-byte `authorization_digest` (`SABI-AUTH-001`) signed by `caller_signature`.
    pub authorization_digest: [u8; 32],
    /// Caller's Ed25519 signature over `authorization_digest`.
    pub caller_signature: Ed25519Signature,
}

/// Sanitized dispatch ticket emitted by [`TrustedDriverEnforcementGate::admit_dispatch`]
/// across the untrusted boundary. Contains zero raw secrets.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SanitizedDriverDispatchTicket {
    /// Admitted `OperationHandle`.
    pub operation: OperationHandle,
    /// Fenced one-shot `CallbackId`.
    pub callback_id: CallbackId,
    /// Durable `ResourceAuthority` activation receipt ID.
    pub activation_receipt_id: ReceiptId,
    /// Redacted credential fingerprint reference (never raw secret bytes).
    pub credential_ref: Option<CustodiedCredentialRef>,
    /// Validated canonical `DriverProvider` `Dispatch` wire bytes.
    pub canonical_dispatch_wire: Vec<u8>,
}

/// Untrusted completion frame submitted back across the TCB enforcement gate.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DriverCompletionSubmission {
    /// Target `DriverId` reporting completion.
    pub driver_id: DriverId,
    /// `DriverRecord.generation` presented by the adapter.
    pub driver_generation: Generation,
    /// `FencingToken` presented by the adapter (`DRV-BOUND-001`).
    pub driver_fencing_token: FencingToken,
    /// `process_incarnation` presented by the adapter (`DRV-BOUND-001`).
    pub process_incarnation: Generation,
    /// Current monotonic clock reading in nanoseconds.
    pub now_monotonic_ns: u64,
    /// Canonical `nlos.sabi.DriverProvider` `Response` (`CompleteAck`) wire frame.
    pub canonical_complete_response_wire: Vec<u8>,
    /// Untrusted claim flag from an outer shim (ignored by the TCB gate).
    pub untrusted_shim_claimed_verified: bool,
}

/// Authoritative completion receipt verified by [`TrustedDriverEnforcementGate`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedDriverCompletion {
    /// Completed `OperationHandle`.
    pub operation: OperationHandle,
    /// Canonical `OperationState` committed inside the TCB gate.
    pub state: OperationState,
    /// Canonical `CompletionOutcome`.
    pub outcome: CompletionOutcome,
    /// Authoritative `[TASK-PROVIDER-001]` isolation posture evaluated by the TCB gate.
    pub isolation_posture: ProviderIsolationPosture,
    /// Receipt tag (`"PROVIDER_ISOLATION_VERIFIED"` or `"PROVIDER_ISOLATION_UNVERIFIED"`).
    pub isolation_marker: &'static str,
}

/// Explicit fenced reconciliation input required to resolve a post-crash `UNCERTAIN`
/// operation (`DRV-LIFE-001`, `KABI-UNCERTAIN-001`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReconcileUncertainRequest {
    /// Crashed `OperationId` currently locked in `EffectUnknown`.
    pub operation_id: OperationId,
    /// Original 128-bit `IdempotencyKey` (blind retry with a new key is forbidden).
    pub idempotency_key: IdempotencyKey,
    /// Current active `DriverId`.
    pub driver_id: DriverId,
    /// Current post-rotation `driver_generation`.
    pub driver_generation: Generation,
    /// Current post-rotation `driver_fencing_token`.
    pub driver_fencing_token: FencingToken,
    /// Current post-restart `process_incarnation`.
    pub process_incarnation: Generation,
    /// Authoritative reconciled terminal outcome (`Completed`, `Failed`, `CancelledBeforeEffect`, or `PartialEffect`).
    pub reconciled_outcome: CompletionOutcome,
}

/// Fail-closed security and lifecycle errors enforced by [`TrustedDriverEnforcementGate`].
#[derive(Debug)]
pub enum EnforcementGateError {
    /// Provisioned secret was empty.
    EmptyCredentialSecret,
    /// Unknown `credential_id` or forged `slot_sha256_fingerprint` (`DRV-CRED-001`).
    InvalidCredentialReference,
    /// Raw secret material was detected on an untrusted wire/echo surface (`DRV-CRED-001`).
    CredentialMaterialLeakBlocked,
    /// Recomputed `authorization_digest` disagreed with signed digest (`SABI-AUTH-001`).
    AuthorizationDigestMismatch,
    /// `IdentityAuthority` rejected caller signature or key state (`CAP-UNFORGE-001`).
    IdentityVerificationFailed(IdentityAuthorityError),
    /// `CapabilityAuthority` rejected handle state, ancestry, or validity (`CAP-UNFORGE-001`).
    CapabilityVerificationFailed(CapabilityAuthorityError),
    /// Capability holder disagreed with authenticated caller principal.
    CapabilityHolderMismatch,
    /// Capability target scope disagreed with requested target.
    CapabilityTargetMismatch,
    /// Capability lacked required right bit.
    CapabilityRightsInsufficient,
    /// Capability `purpose_digest` disagreed with requested purpose.
    CapabilityPurposeMismatch,
    /// Capability call-limit budget was exhausted.
    CapabilityCallLimitExhausted,
    /// Consumed one-shot `ReservationActivationToken` was replayed (`DRV-ACTIVE-001`).
    ActivationTokenReplayed { prior_operation_id: OperationId },
    /// `ReservationActivationToken` belonged to a different `OperationId` (`DRV-ACTIVE-001`).
    ActivationTokenOperationMismatch {
        reservation_operation_id: OperationId,
        requested_operation_id: OperationId,
    },
    /// `ResourceAuthority` rejected reservation activation or quarantine.
    ResourceAuthorityFailed(ResourceAuthorityError),
    /// Missing or expired monotonic `deadline_monotonic_ns` (`DRV-BOUND-001`).
    DeadlineExpired {
        deadline_monotonic_ns: u64,
        now_monotonic_ns: u64,
    },
    /// Adapter presented a stale `fencing_token` or `driver_generation` (`DRV-BOUND-001`).
    StaleFencingToken { driver_id: DriverId },
    /// Adapter presented a stale `process_incarnation` (`DRV-BOUND-001`).
    StaleProcessIncarnation {
        expected_min: Generation,
        got: Generation,
    },
    /// Malformed or mismatched `DriverProvider` wire payload.
    InvalidDriverWire(String),
    /// `[TASK-PROVIDER-001]` rejected unverified provider isolation under strict policy.
    UnverifiedIsolationRejected {
        missing_guarantees: Vec<IsolationGuarantee>,
    },
    /// Unknown `OperationId` at completion/reconciliation.
    UnknownOperation(OperationId),
    /// Crash-induced `UNCERTAIN` operation rejected normal completion spoofing (`DRV-LIFE-001`).
    UncertainOperationRequiresReconciliation {
        operation_id: OperationId,
        uncertain_receipt_id: ReceiptId,
    },
    /// Attempted to re-dispatch an `UNCERTAIN` operation with a new `IdempotencyKey` (`KABI-UNCERTAIN-001`).
    BlindRetryWithNewIdempotencyKeyForbidden {
        operation_id: OperationId,
        expected_idempotency_key: IdempotencyKey,
        attempted_idempotency_key: IdempotencyKey,
    },
}

impl fmt::Display for EnforcementGateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyCredentialSecret => formatter.write_str("credential secret cannot be empty"),
            Self::InvalidCredentialReference => {
                formatter.write_str("custodied credential reference is unknown or tampered")
            }
            Self::CredentialMaterialLeakBlocked => {
                formatter.write_str("blocked raw credential reflection across untrusted boundary")
            }
            Self::AuthorizationDigestMismatch => {
                formatter.write_str("driver authorization_digest did not match canonical request")
            }
            Self::IdentityVerificationFailed(err) => {
                write!(formatter, "caller identity verification failed: {err}")
            }
            Self::CapabilityVerificationFailed(err) => {
                write!(formatter, "capability verification failed: {err}")
            }
            Self::CapabilityHolderMismatch => {
                formatter.write_str("authenticated caller is not the capability holder")
            }
            Self::CapabilityTargetMismatch => {
                formatter.write_str("capability target scope mismatch")
            }
            Self::CapabilityRightsInsufficient => {
                formatter.write_str("capability lacks required execution right")
            }
            Self::CapabilityPurposeMismatch => {
                formatter.write_str("capability purpose_digest mismatch")
            }
            Self::CapabilityCallLimitExhausted => {
                formatter.write_str("capability call_limit budget is exhausted")
            }
            Self::ActivationTokenReplayed { prior_operation_id } => write!(
                formatter,
                "one-shot ReservationActivationToken was already consumed by {prior_operation_id:?}"
            ),
            Self::ActivationTokenOperationMismatch {
                reservation_operation_id,
                requested_operation_id,
            } => write!(
                formatter,
                "ReservationActivationToken bound to {reservation_operation_id:?}, not {requested_operation_id:?}"
            ),
            Self::ResourceAuthorityFailed(err) => {
                write!(formatter, "resource authority rejected operation: {err}")
            }
            Self::DeadlineExpired {
                deadline_monotonic_ns,
                now_monotonic_ns,
            } => write!(
                formatter,
                "driver deadline {deadline_monotonic_ns} expired at {now_monotonic_ns}"
            ),
            Self::StaleFencingToken { driver_id } => {
                write!(formatter, "stale driver fencing_token for {driver_id:?}")
            }
            Self::StaleProcessIncarnation { expected_min, got } => write!(
                formatter,
                "stale process_incarnation {got:?}; active incarnation is {expected_min:?}"
            ),
            Self::InvalidDriverWire(detail) => {
                write!(formatter, "invalid DriverProvider wire frame: {detail}")
            }
            Self::UnverifiedIsolationRejected { missing_guarantees } => write!(
                formatter,
                "provider isolation unverified; missing guarantees: {missing_guarantees:?}"
            ),
            Self::UnknownOperation(id) => write!(formatter, "unknown operation {id:?}"),
            Self::UncertainOperationRequiresReconciliation {
                operation_id,
                uncertain_receipt_id,
            } => write!(
                formatter,
                "operation {operation_id:?} is locked in UNCERTAIN ({uncertain_receipt_id:?}) pending explicit reconciliation"
            ),
            Self::BlindRetryWithNewIdempotencyKeyForbidden {
                operation_id,
                expected_idempotency_key,
                attempted_idempotency_key,
            } => write!(
                formatter,
                "operation {operation_id:?} in UNCERTAIN forbids blind retry with new idempotency key {attempted_idempotency_key:?} (expected {expected_idempotency_key:?})"
            ),
        }
    }
}

impl Error for EnforcementGateError {}

struct TrackedOperationEntry {
    machine: OperationMachine,
    ticket: CallbackTicket,
    idempotency_key: IdempotencyKey,
    reservation_id: ReservationId,
    activation_receipt_id: ReceiptId,
    driver_id: DriverId,
    driver_generation: Generation,
    driver_fencing_token: FencingToken,
    process_incarnation: Generation,
    deadline_monotonic_ns: u64,
    crash_uncertain_receipt: Option<ReceiptId>,
    reconciled_state: Option<OperationState>,
}

/// Computes the canonical 32-byte `authorization_digest` (`SABI-AUTH-001`) bound
/// to an operation handle, idempotency key, deadline, incarnation, fencing token,
/// and canonical `DriverProvider` dispatch payload.
#[must_use]
pub fn compute_driver_authorization_digest(
    operation: OperationHandle,
    idempotency_key: IdempotencyKey,
    deadline_monotonic_ns: u64,
    process_incarnation: Generation,
    driver_fencing_token: &FencingToken,
    canonical_dispatch_wire: &[u8],
) -> [u8; 32] {
    let payload_hash: [u8; 32] = Sha256::digest(canonical_dispatch_wire).into();
    let mut hasher = Sha256::new();
    hasher.update(DRIVER_AUTH_DIGEST_DOMAIN);
    hasher.update(operation.operation_id.as_bytes());
    hasher.update(operation.generation.get().to_be_bytes());
    hasher.update(idempotency_key.as_bytes());
    hasher.update(deadline_monotonic_ns.to_be_bytes());
    hasher.update(process_incarnation.get().to_be_bytes());
    hasher.update(driver_fencing_token);
    hasher.update(payload_hash);
    hasher.finalize().into()
}

/// Controlled TCB edge `TrustedDriverEnforcementGate` (`ADR-0027` section 2.2).
pub struct TrustedDriverEnforcementGate {
    isolation_policy: IsolationEnforcementPolicy,
    active_process_incarnation: Generation,
    credential_vault: HashMap<[u8; 16], RedactedSecretMaterial>,
    driver_fences: HashMap<DriverId, (Generation, FencingToken)>,
    consumed_activation_tokens: HashMap<[u8; 32], OperationId>,
    operations: HashMap<OperationId, TrackedOperationEntry>,
}

impl TrustedDriverEnforcementGate {
    /// Creates a new `TrustedDriverEnforcementGate` at initial `process_incarnation = 1`.
    #[must_use]
    pub fn new(isolation_policy: IsolationEnforcementPolicy) -> Self {
        Self {
            isolation_policy,
            active_process_incarnation: Generation::INITIAL,
            credential_vault: HashMap::new(),
            driver_fences: HashMap::new(),
            consumed_activation_tokens: HashMap::new(),
            operations: HashMap::new(),
        }
    }

    /// Provisions a raw credential inside the TCB vault and returns ONLY a redacted
    /// [`CustodiedCredentialRef`] (`DRV-CRED-001`).
    ///
    /// # Errors
    ///
    /// Returns [`EnforcementGateError::EmptyCredentialSecret`] if `raw_secret` is empty.
    pub fn provision_credential(
        &mut self,
        credential_id: [u8; 16],
        raw_secret: &[u8],
    ) -> Result<CustodiedCredentialRef, EnforcementGateError> {
        if raw_secret.is_empty() {
            return Err(EnforcementGateError::EmptyCredentialSecret);
        }
        let mut hasher = Sha256::new();
        hasher.update(CREDENTIAL_FINGERPRINT_DOMAIN);
        hasher.update(credential_id);
        hasher.update(raw_secret);
        let fingerprint: [u8; 32] = hasher.finalize().into();

        self.credential_vault.insert(
            credential_id,
            RedactedSecretMaterial {
                secret: raw_secret.to_vec(),
                fingerprint,
            },
        );
        Ok(CustodiedCredentialRef {
            credential_id,
            slot_sha256_fingerprint: fingerprint,
        })
    }

    /// Registers or updates the authoritative `(generation, fencing_token)` fence for `driver_id`.
    pub fn bind_driver_fence(
        &mut self,
        driver_id: DriverId,
        generation: Generation,
        fencing_token: FencingToken,
    ) {
        self.driver_fences
            .insert(driver_id, (generation, fencing_token));
    }

    /// Rotates a driver's fence in `ResourceAuthority` and atomically advances the
    /// gate's active `(generation, fencing_token)` (`DRV-BOUND-001`).
    ///
    /// # Errors
    ///
    /// Propagates [`EnforcementGateError::ResourceAuthorityFailed`] on stale CAS fences.
    pub fn rotate_driver_fence(
        &mut self,
        resource: &ResourceAuthority,
        request: RotateDriverRequest,
    ) -> Result<(Generation, FencingToken), EnforcementGateError> {
        let rotated = resource
            .rotate_driver(request)
            .map_err(EnforcementGateError::ResourceAuthorityFailed)?
            .record();
        self.driver_fences.insert(
            rotated.driver_id,
            (rotated.generation, rotated.fencing_token),
        );
        Ok((rotated.generation, rotated.fencing_token))
    }

    /// Advances the active `process_incarnation` fence, invalidating completions from
    /// prior adapter incarnations (`DRV-BOUND-001`).
    pub fn advance_process_incarnation(&mut self, next_incarnation: Generation) {
        if next_incarnation.get() > self.active_process_incarnation.get() {
            self.active_process_incarnation = next_incarnation;
        }
    }

    /// Scans untrusted wire or diagnostic bytes to guarantee zero raw secret material
    /// crosses or reflects back from [`crate::driver_shim::DriverProtocolCompatShim`] (`DRV-CRED-001`).
    ///
    /// # Errors
    ///
    /// Returns [`EnforcementGateError::CredentialMaterialLeakBlocked`] if any provisioned
    /// secret byte sequence is present in `untrusted_bytes`.
    pub fn assert_zero_secret_reflection(
        &self,
        untrusted_bytes: &[u8],
    ) -> Result<(), EnforcementGateError> {
        for entry in self.credential_vault.values() {
            if !entry.secret.is_empty()
                && untrusted_bytes
                    .windows(entry.secret.len())
                    .any(|win| win == entry.secret.as_slice())
            {
                return Err(EnforcementGateError::CredentialMaterialLeakBlocked);
            }
        }
        Ok(())
    }

    /// Inspects the authoritative `OperationSnapshot` tracked inside the TCB gate.
    #[must_use]
    pub fn inspect_operation(&self, operation_id: OperationId) -> Option<OperationSnapshot> {
        let entry = self.operations.get(&operation_id)?;
        let mut snap = entry.machine.snapshot();
        if let Some(reconciled) = entry.reconciled_state {
            snap.state = reconciled;
        } else if let Some(receipt_id) = entry.crash_uncertain_receipt {
            snap.state = OperationState::EffectUnknown { receipt_id };
        }
        Some(snap)
    }

    /// Verifies caller `Capability`, `authorization_digest`, `ReservationActivationToken`,
    /// `deadline`, `process_incarnation`, `fencing_token`, and redacted credential handle
    /// before emitting a [`SanitizedDriverDispatchTicket`].
    ///
    /// # Errors
    ///
    /// Fails closed on any credential, signature, capability, token replay/cross-op,
    /// deadline, incarnation, or fencing violation.
    pub fn admit_dispatch(
        &mut self,
        identity: &IdentityAuthority,
        capability: &CapabilityAuthority,
        resource: &ResourceAuthority,
        request: DriverAdmissionRequest,
    ) -> Result<SanitizedDriverDispatchTicket, EnforcementGateError> {
        self.assert_zero_secret_reflection(&request.canonical_register_wire)?;
        self.assert_zero_secret_reflection(&request.canonical_dispatch_wire)?;
        self.verify_credential_ref(request.credential_ref)?;
        self.verify_bounds_at_dispatch(&request)?;

        let (spec, dispatch) = decode_register_and_dispatch(
            &request.canonical_register_wire,
            &request.canonical_dispatch_wire,
        )?;
        self.reject_blind_retry_if_uncertain(
            dispatch.handle.operation_id,
            request.idempotency_key,
        )?;

        let expected_digest = compute_driver_authorization_digest(
            dispatch.handle,
            request.idempotency_key,
            request.deadline_monotonic_ns,
            request.process_incarnation,
            &request.driver_fencing_token,
            &request.canonical_dispatch_wire,
        );
        if expected_digest != request.authorization_digest {
            return Err(EnforcementGateError::AuthorizationDigestMismatch);
        }

        verify_identity_and_capability(identity, capability, &request)?;
        let activation_receipt_id = self.consume_one_shot_activation_token(
            resource,
            &request,
            dispatch.handle.operation_id,
        )?;

        let mut machine = OperationMachine::new(spec);
        let ticket = machine
            .dispatch(dispatch.handle, dispatch.callback_id)
            .map_err(|err| EnforcementGateError::InvalidDriverWire(err.to_string()))?;

        self.operations.insert(
            dispatch.handle.operation_id,
            TrackedOperationEntry {
                machine,
                ticket,
                idempotency_key: request.idempotency_key,
                reservation_id: request.reservation_id,
                activation_receipt_id,
                driver_id: request.driver_id,
                driver_generation: request.driver_generation,
                driver_fencing_token: request.driver_fencing_token,
                process_incarnation: request.process_incarnation,
                deadline_monotonic_ns: request.deadline_monotonic_ns,
                crash_uncertain_receipt: None,
                reconciled_state: None,
            },
        );

        Ok(SanitizedDriverDispatchTicket {
            operation: dispatch.handle,
            callback_id: dispatch.callback_id,
            activation_receipt_id,
            credential_ref: request.credential_ref,
            canonical_dispatch_wire: request.canonical_dispatch_wire,
        })
    }

    /// Independently validates an incoming completion response against active fences,
    /// `deadline`, crash `UNCERTAIN` locks, secret reflection rules, and `[TASK-PROVIDER-001]`
    /// provider isolation attestation.
    ///
    /// # Errors
    ///
    /// Fails closed on secret reflection, stale `fencing_token` / `process_incarnation`,
    /// expired deadline, crash-locked `UNCERTAIN` state, or unverified isolation under
    /// [`IsolationEnforcementPolicy::RequireVerifiedIsolation`].
    pub fn admit_completion(
        &mut self,
        submission: &DriverCompletionSubmission,
    ) -> Result<VerifiedDriverCompletion, EnforcementGateError> {
        let _ = submission.untrusted_shim_claimed_verified;
        self.assert_zero_secret_reflection(&submission.canonical_complete_response_wire)?;
        self.verify_active_driver_fence(
            submission.driver_id,
            submission.driver_generation,
            &submission.driver_fencing_token,
        )?;
        if submission.process_incarnation.get() < self.active_process_incarnation.get() {
            return Err(EnforcementGateError::StaleProcessIncarnation {
                expected_min: self.active_process_incarnation,
                got: submission.process_incarnation,
            });
        }

        let ack = decode_complete_ack(&submission.canonical_complete_response_wire)?;
        let (operation_id, outcome, attestation) =
            parse_complete_ack_fields(&ack, &self.operations)?;
        let entry = self
            .operations
            .get_mut(&operation_id)
            .ok_or(EnforcementGateError::UnknownOperation(operation_id))?;

        if let Some(uncertain_receipt_id) = entry.crash_uncertain_receipt
            && entry.reconciled_state.is_none()
        {
            return Err(
                EnforcementGateError::UncertainOperationRequiresReconciliation {
                    operation_id,
                    uncertain_receipt_id,
                },
            );
        }
        if entry.driver_id != submission.driver_id
            || entry.driver_generation != submission.driver_generation
            || entry.driver_fencing_token != submission.driver_fencing_token
        {
            return Err(EnforcementGateError::StaleFencingToken {
                driver_id: submission.driver_id,
            });
        }
        if submission.process_incarnation != entry.process_incarnation {
            return Err(EnforcementGateError::StaleProcessIncarnation {
                expected_min: self.active_process_incarnation,
                got: submission.process_incarnation,
            });
        }
        if submission.now_monotonic_ns >= entry.deadline_monotonic_ns {
            return Err(EnforcementGateError::DeadlineExpired {
                deadline_monotonic_ns: entry.deadline_monotonic_ns,
                now_monotonic_ns: submission.now_monotonic_ns,
            });
        }

        let isolation_posture = attestation.evaluate_posture();
        if self.isolation_policy == IsolationEnforcementPolicy::RequireVerifiedIsolation
            && let ProviderIsolationPosture::ProviderIsolationUnverified {
                ref missing_guarantees,
            } = isolation_posture
        {
            return Err(EnforcementGateError::UnverifiedIsolationRejected {
                missing_guarantees: missing_guarantees.clone(),
            });
        }

        let _decision = entry
            .machine
            .complete(entry.ticket, outcome)
            .map_err(|err| EnforcementGateError::InvalidDriverWire(err.to_string()))?;
        let snapshot = entry.machine.snapshot();
        let isolation_marker = isolation_posture.receipt_marker();

        Ok(VerifiedDriverCompletion {
            operation: snapshot.handle,
            state: snapshot.state,
            outcome,
            isolation_posture,
            isolation_marker,
        })
    }

    /// Converges all non-terminal in-flight operations to `UNCERTAIN` (`EffectUnknown`)
    /// and quarantines their active `ResourceAuthority` reservations upon adapter or
    /// compatibility shim crash (`DRV-LIFE-001`).
    ///
    /// # Errors
    ///
    /// Propagates [`EnforcementGateError::ResourceAuthorityFailed`] if reservation quarantine fails.
    pub fn on_adapter_or_shim_crash(
        &mut self,
        resource: &ResourceAuthority,
        reason_digest: [u8; 32],
        quarantined_at_ms: u64,
    ) -> Result<Vec<(OperationId, ReceiptId)>, EnforcementGateError> {
        let next_inc = self
            .active_process_incarnation
            .checked_next()
            .unwrap_or(self.active_process_incarnation);
        self.active_process_incarnation = next_inc;

        let mut transitioned = Vec::new();
        for (&operation_id, entry) in &mut self.operations {
            if entry.machine.snapshot().state.is_terminal()
                || entry.crash_uncertain_receipt.is_some()
            {
                continue;
            }
            let _ = resource
                .quarantine(QuarantineReservationRequest {
                    reservation_id: entry.reservation_id,
                    operation_id,
                    activation_receipt_id: entry.activation_receipt_id,
                    reason_digest,
                    quarantined_at_ms,
                })
                .map_err(EnforcementGateError::ResourceAuthorityFailed)?;
            let receipt_id = derive_crash_uncertain_receipt(operation_id, &reason_digest);
            entry.crash_uncertain_receipt = Some(receipt_id);
            transitioned.push((operation_id, receipt_id));
        }
        transitioned.sort_by_key(|(op_id, _)| *op_id.as_bytes());
        Ok(transitioned)
    }

    /// Explicitly reconciles a crash-locked `UNCERTAIN` operation after verifying the
    /// original `IdempotencyKey`, post-rotation `fencing_token`, and active `process_incarnation`
    /// (`DRV-LIFE-001`, `KABI-UNCERTAIN-001`).
    ///
    /// # Errors
    ///
    /// Fails closed if the operation is unknown, not in `UNCERTAIN`, presented with a
    /// different `IdempotencyKey`, or presented with stale fencing/incarnation tokens.
    pub fn reconcile_uncertain_operation(
        &mut self,
        request: ReconcileUncertainRequest,
    ) -> Result<OperationSnapshot, EnforcementGateError> {
        self.verify_active_driver_fence(
            request.driver_id,
            request.driver_generation,
            &request.driver_fencing_token,
        )?;
        if request.process_incarnation.get() < self.active_process_incarnation.get() {
            return Err(EnforcementGateError::StaleProcessIncarnation {
                expected_min: self.active_process_incarnation,
                got: request.process_incarnation,
            });
        }
        let entry = self
            .operations
            .get_mut(&request.operation_id)
            .ok_or(EnforcementGateError::UnknownOperation(request.operation_id))?;
        if entry.crash_uncertain_receipt.is_none() {
            return Err(EnforcementGateError::InvalidDriverWire(
                "operation is not in crash UNCERTAIN state".to_owned(),
            ));
        }
        if entry.idempotency_key != request.idempotency_key {
            return Err(
                EnforcementGateError::BlindRetryWithNewIdempotencyKeyForbidden {
                    operation_id: request.operation_id,
                    expected_idempotency_key: entry.idempotency_key,
                    attempted_idempotency_key: request.idempotency_key,
                },
            );
        }
        entry.reconciled_state = Some(outcome_to_state(request.reconciled_outcome));
        let mut snap = entry.machine.snapshot();
        snap.state = outcome_to_state(request.reconciled_outcome);
        Ok(snap)
    }

    fn verify_credential_ref(
        &self,
        cred: Option<CustodiedCredentialRef>,
    ) -> Result<(), EnforcementGateError> {
        let Some(reference) = cred else {
            return Ok(());
        };
        let Some(stored) = self.credential_vault.get(&reference.credential_id) else {
            return Err(EnforcementGateError::InvalidCredentialReference);
        };
        if stored.fingerprint != reference.slot_sha256_fingerprint {
            return Err(EnforcementGateError::InvalidCredentialReference);
        }
        Ok(())
    }

    fn verify_bounds_at_dispatch(
        &mut self,
        request: &DriverAdmissionRequest,
    ) -> Result<(), EnforcementGateError> {
        if request.deadline_monotonic_ns == 0
            || request.now_monotonic_ns >= request.deadline_monotonic_ns
        {
            return Err(EnforcementGateError::DeadlineExpired {
                deadline_monotonic_ns: request.deadline_monotonic_ns,
                now_monotonic_ns: request.now_monotonic_ns,
            });
        }
        if request.process_incarnation.get() < self.active_process_incarnation.get() {
            return Err(EnforcementGateError::StaleProcessIncarnation {
                expected_min: self.active_process_incarnation,
                got: request.process_incarnation,
            });
        }
        self.active_process_incarnation = request.process_incarnation;
        if let Some(&(active_gen, active_fence)) = self.driver_fences.get(&request.driver_id) {
            if request.driver_generation != active_gen
                || request.driver_fencing_token != active_fence
            {
                return Err(EnforcementGateError::StaleFencingToken {
                    driver_id: request.driver_id,
                });
            }
        } else {
            self.bind_driver_fence(
                request.driver_id,
                request.driver_generation,
                request.driver_fencing_token,
            );
        }
        Ok(())
    }

    fn verify_active_driver_fence(
        &self,
        driver_id: DriverId,
        generation: Generation,
        fencing_token: &FencingToken,
    ) -> Result<(), EnforcementGateError> {
        let Some(&(active_gen, ref active_fence)) = self.driver_fences.get(&driver_id) else {
            return Err(EnforcementGateError::StaleFencingToken { driver_id });
        };
        if generation != active_gen || fencing_token != active_fence {
            return Err(EnforcementGateError::StaleFencingToken { driver_id });
        }
        Ok(())
    }

    fn reject_blind_retry_if_uncertain(
        &self,
        operation_id: OperationId,
        attempted_key: IdempotencyKey,
    ) -> Result<(), EnforcementGateError> {
        if let Some(existing) = self.operations.get(&operation_id)
            && let Some(uncertain_receipt_id) = existing.crash_uncertain_receipt
            && existing.reconciled_state.is_none()
        {
            if existing.idempotency_key != attempted_key {
                return Err(
                    EnforcementGateError::BlindRetryWithNewIdempotencyKeyForbidden {
                        operation_id,
                        expected_idempotency_key: existing.idempotency_key,
                        attempted_idempotency_key: attempted_key,
                    },
                );
            }
            return Err(
                EnforcementGateError::UncertainOperationRequiresReconciliation {
                    operation_id,
                    uncertain_receipt_id,
                },
            );
        }
        Ok(())
    }

    fn consume_one_shot_activation_token(
        &mut self,
        resource: &ResourceAuthority,
        request: &DriverAdmissionRequest,
        operation_id: OperationId,
    ) -> Result<ReceiptId, EnforcementGateError> {
        let token_bytes = *request.activation_token.as_bytes();
        if let Some(&prior_operation_id) = self.consumed_activation_tokens.get(&token_bytes) {
            return Err(EnforcementGateError::ActivationTokenReplayed { prior_operation_id });
        }
        let reservation = resource
            .inspect_reservation(request.reservation_id)
            .map_err(EnforcementGateError::ResourceAuthorityFailed)?;
        if reservation.operation_id != operation_id {
            return Err(EnforcementGateError::ActivationTokenOperationMismatch {
                reservation_operation_id: reservation.operation_id,
                requested_operation_id: operation_id,
            });
        }
        let decision = resource
            .activate(ActivateReservationRequest {
                reservation_id: request.reservation_id,
                call_id: request.call_id,
                operation_id,
                driver_id: request.driver_id,
                driver_generation: request.driver_generation,
                driver_fencing_token: request.driver_fencing_token,
                activation_token: token_bytes,
                activated_at_ms: request.admitted_at_ms,
            })
            .map_err(EnforcementGateError::ResourceAuthorityFailed)?;
        if matches!(decision, nlos_resource::ActivationDecision::Replayed(_)) {
            return Err(EnforcementGateError::ActivationTokenReplayed {
                prior_operation_id: operation_id,
            });
        }
        self.consumed_activation_tokens
            .insert(token_bytes, operation_id);
        Ok(decision.receipt().receipt_id)
    }
}

fn verify_identity_and_capability(
    identity: &IdentityAuthority,
    capability: &CapabilityAuthority,
    request: &DriverAdmissionRequest,
) -> Result<(), EnforcementGateError> {
    let verified = identity
        .verify_semantic_authority_signature(VerifySemanticAuthoritySignatureRequest {
            message_digest: request.authorization_digest,
            issuer: request.caller_principal,
            control_domain_id: request.caller_control_domain,
            key_id: request.caller_key_id,
            signature: request.caller_signature,
            verified_at_ms: request.admitted_at_ms,
        })
        .map_err(EnforcementGateError::IdentityVerificationFailed)?;

    let record = capability
        .inspect_active(request.capability_handle, request.admitted_at_ms)
        .map_err(EnforcementGateError::CapabilityVerificationFailed)?;
    if record.holder != verified.principal_id()
        || record.holder_control_domain != verified.control_domain_id()
    {
        return Err(EnforcementGateError::CapabilityHolderMismatch);
    }
    if record.target != request.target {
        return Err(EnforcementGateError::CapabilityTargetMismatch);
    }
    if !record.rights.contains(request.required_right) {
        return Err(EnforcementGateError::CapabilityRightsInsufficient);
    }
    if record.purpose_digest != request.purpose_digest {
        return Err(EnforcementGateError::CapabilityPurposeMismatch);
    }
    if capability
        .call_limit_remaining(request.capability_handle)
        .map_err(EnforcementGateError::CapabilityVerificationFailed)?
        == Some(0)
    {
        return Err(EnforcementGateError::CapabilityCallLimitExhausted);
    }
    Ok(())
}

fn decode_register_and_dispatch(
    register_wire: &[u8],
    dispatch_wire: &[u8],
) -> Result<
    (
        nlos_operation::OperationSpec,
        nlos_driver_mock::DispatchProviderOperation,
    ),
    EnforcementGateError,
> {
    let DecodedProviderRequest::Register(spec) = decode_request_proto(register_wire)
        .map_err(|err| EnforcementGateError::InvalidDriverWire(err.to_string()))?
    else {
        return Err(EnforcementGateError::InvalidDriverWire(
            "expected Register arm in canonical_register_wire".to_owned(),
        ));
    };
    let DecodedProviderRequest::Dispatch(dispatch) = decode_request_proto(dispatch_wire)
        .map_err(|err| EnforcementGateError::InvalidDriverWire(err.to_string()))?
    else {
        return Err(EnforcementGateError::InvalidDriverWire(
            "expected Dispatch arm in canonical_dispatch_wire".to_owned(),
        ));
    };
    if spec.operation_id != dispatch.handle.operation_id
        || spec.generation != dispatch.handle.generation
    {
        return Err(EnforcementGateError::InvalidDriverWire(
            "Register and Dispatch operation handles disagreed".to_owned(),
        ));
    }
    Ok((spec, dispatch))
}

fn decode_complete_ack(wire: &[u8]) -> Result<DriverProviderCompleteAck, EnforcementGateError> {
    let resp = decode_driver_provider_response(wire)
        .map_err(|err| EnforcementGateError::InvalidDriverWire(err.to_string()))?;
    match resp.response {
        Some(driver_provider_response::Response::CompleteAck(ack)) => Ok(ack),
        _ => Err(EnforcementGateError::InvalidDriverWire(
            "expected CompleteAck arm in canonical_complete_response_wire".to_owned(),
        )),
    }
}

fn parse_complete_ack_fields(
    ack: &DriverProviderCompleteAck,
    operations: &HashMap<OperationId, TrackedOperationEntry>,
) -> Result<(OperationId, CompletionOutcome, ProviderIsolationAttestation), EnforcementGateError> {
    let receipt_bytes: [u8; 16] =
        ack.receipt_id.as_slice().try_into().map_err(|_| {
            EnforcementGateError::InvalidDriverWire("invalid receipt_id".to_owned())
        })?;
    let receipt_id = ReceiptId::from_bytes(receipt_bytes);
    let outcome = match DriverProviderTerminalOutcome::try_from(ack.outcome) {
        Ok(DriverProviderTerminalOutcome::Completed) => CompletionOutcome::Completed { receipt_id },
        Ok(DriverProviderTerminalOutcome::Failed) => CompletionOutcome::Failed { receipt_id },
        Ok(DriverProviderTerminalOutcome::PartialEffect) => {
            CompletionOutcome::PartialEffect { receipt_id }
        }
        Ok(DriverProviderTerminalOutcome::EffectUnknown) => {
            CompletionOutcome::EffectUnknown { receipt_id }
        }
        Ok(DriverProviderTerminalOutcome::CancelledBeforeEffect) => {
            CompletionOutcome::CancelledBeforeEffect { receipt_id }
        }
        _ => {
            return Err(EnforcementGateError::InvalidDriverWire(
                "unspecified terminal outcome".to_owned(),
            ));
        }
    };
    let state = OperationLifecycleState::try_from(ack.state)
        .map_err(|_| EnforcementGateError::InvalidDriverWire("invalid state".to_owned()))?;
    let _ = state;

    let iso_wire = ack.isolation.as_ref().ok_or_else(|| {
        EnforcementGateError::InvalidDriverWire("missing isolation attestation".to_owned())
    })?;
    let attestation = ProviderIsolationAttestation::from_wire(iso_wire)
        .map_err(|err| EnforcementGateError::InvalidDriverWire(err.to_string()))?;

    let operation_id = resolve_single_target_operation(operations)?;
    Ok((operation_id, outcome, attestation))
}

fn resolve_single_target_operation(
    operations: &HashMap<OperationId, TrackedOperationEntry>,
) -> Result<OperationId, EnforcementGateError> {
    if let Some((&op_id, _)) = operations
        .iter()
        .find(|(_, entry)| !entry.machine.snapshot().state.is_terminal())
    {
        return Ok(op_id);
    }
    operations
        .keys()
        .next()
        .copied()
        .ok_or(EnforcementGateError::UnknownOperation(
            OperationId::from_bytes([0_u8; 16]),
        ))
}

const fn outcome_to_state(outcome: CompletionOutcome) -> OperationState {
    match outcome {
        CompletionOutcome::Completed { receipt_id } => OperationState::Completed { receipt_id },
        CompletionOutcome::Failed { receipt_id } => OperationState::Failed { receipt_id },
        CompletionOutcome::CancelledBeforeEffect { receipt_id } => {
            OperationState::CancelledBeforeEffect { receipt_id }
        }
        CompletionOutcome::PartialEffect { receipt_id } => {
            OperationState::PartialEffect { receipt_id }
        }
        CompletionOutcome::EffectUnknown { receipt_id } => {
            OperationState::EffectUnknown { receipt_id }
        }
    }
}

fn derive_crash_uncertain_receipt(
    operation_id: OperationId,
    reason_digest: &[u8; 32],
) -> ReceiptId {
    let mut hasher = Sha256::new();
    hasher.update(CRASH_UNCERTAIN_RECEIPT_DOMAIN);
    hasher.update(operation_id.as_bytes());
    hasher.update(reason_digest);
    let digest: [u8; 32] = hasher.finalize().into();
    let mut id = [0_u8; 16];
    id.copy_from_slice(&digest[..16]);
    ReceiptId::from_bytes(id)
}

fn hex_slice(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(2 + bytes.len() * 2);
    out.push_str("0x");
    for byte in bytes {
        let _ = write!(&mut out, "{byte:02x}");
    }
    out
}
