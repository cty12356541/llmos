//! Typed `TakeoverControl` adapter for cross-process submission of signed
//! takeover barrier observations over local IPC.
//!
//! # Caller and signer are different principals
//!
//! Unlike `SystemControl.submit`, which rejects a `ControlCommand` whose
//! issuer differs from the authenticated caller, this service deliberately
//! does NOT require `caller.principal_id == signer_principal_id`: the
//! Ed25519 signature belongs to the remote participant principal that
//! observed the barrier, while the IPC caller is authorized separately
//! through capability handles ([`TakeoverControlAuthorizer`]). Signer
//! fields in the returned [`BarrierObservationRecord`] come exclusively
//! from the identity-authority-verified proof stored by `TaskAuthority`;
//! caller-asserted signer bytes never reach the response.
//!
//! # Replay and clock policy
//!
//! Anti-replay is the SABI mutation idempotency key plus the store's
//! exact-replay semantics; no additional time-window check is invented
//! here. The handler performs no wall-clock enforcement of
//! `observed_at_ms`: a trusted clock is a separate future authority, and
//! identity key validity already bounds the verification time accepted by
//! the store.
//!
//! # Error mapping over IPC
//!
//! [`TakeoverControlError::to_sabi_failure`] is the authoritative mapping
//! used when a rejected request is answered with a typed failure envelope
//! ([`failure_envelope`]) instead of a transport error. Failed mutations
//! intentionally carry no Operation/Receipt evidence because no effect
//! happened; success responses always reference the durable barrier
//! receipt.
//!
//! The crate's default build keeps the handler transport-neutral. A
//! feature-gated `takeover-control-conformance` binary exercises the same
//! handler over Unix sockets and Windows named pipes, with TypeScript and
//! Python clients constructing the generated payload and verifying durable
//! replay; it is test infrastructure, not a production daemon.

use std::error::Error;
use std::fmt;
use std::num::NonZeroU64;

use nlos_identity::IdentityAuthority;
use nlos_schema::sabi::v1::{
    BarrierObservationRecord, Envelope, ReceiptReference, RetryDirective, SabiErrorCode,
    SabiFailure, SabiRequestContext, SabiResponseContext, SubmitBarrierObservationRequest,
    envelope,
};
use nlos_schema::{
    CommonSemanticsError, CompatibilityError, MethodSemantics, REQUEST_ID_BYTES,
    decode_submit_barrier_observation_request, encode_barrier_observation_record,
    takeover_control_schema_identity, validate_sabi_request_context,
};
use nlos_task::{
    AuthorityTakeoverBarrierReceiptRequest, BarrierObservationSignature, ParticipantRecord,
    ParticipantType, SqliteTaskAuthority, TaskStoreError,
};
use nlos_types::{ControlDomainId, Generation, KeyId, PrincipalId, ReceiptId, TaskParticipantId};

pub const TAKEOVER_CONTROL_SERVICE: &str = "takeover_control";
pub const SUBMIT_BARRIER_OBSERVATION_METHOD: &str = "submit_barrier_observation";

/// Wire code of one frozen takeover fence participant kind. Mirrors the
/// `TaskAuthority` participant codes 1..=8.
#[must_use]
pub const fn participant_type_code(participant_type: ParticipantType) -> u32 {
    match participant_type {
        ParticipantType::TaskStore => 1,
        ParticipantType::ArtifactHead => 2,
        ParticipantType::SemanticAdmission => 3,
        ParticipantType::ChannelTopic => 4,
        ParticipantType::DriverGateway => 5,
        ParticipantType::ResourceLedger => 6,
        ParticipantType::ProcessBinding => 7,
        ParticipantType::OperationBinding => 8,
    }
}

/// Policy boundary for every `TakeoverControl` entry point. Implementations
/// validate the supplied capability handles against their authority; the
/// remote signature proof is verified separately by the identity authority
/// and is not an authorization input.
pub trait TakeoverControlAuthorizer {
    /// Authorizes one signed barrier-observation submission.
    ///
    /// # Errors
    ///
    /// Returns a static policy class safe for the local service log.
    fn authorize_submit_barrier_observation(
        &self,
        context: &SabiRequestContext,
        request: &SubmitBarrierObservationRequest,
    ) -> Result<(), &'static str>;
}

#[derive(Debug)]
pub enum TakeoverControlError {
    Schema(CompatibilityError),
    Common(CommonSemanticsError),
    Task(TaskStoreError),
    UnknownMethod,
    AuthorizationDenied(&'static str),
    InvalidParticipantType(u32),
    InvalidParticipantGeneration,
}

impl fmt::Display for TakeoverControlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Schema(error) => write!(formatter, "invalid TakeoverControl schema: {error}"),
            Self::Common(error) => write!(formatter, "invalid TakeoverControl context: {error}"),
            Self::Task(error) => {
                write!(formatter, "TaskAuthority rejected TakeoverControl: {error}")
            }
            Self::UnknownMethod => formatter.write_str("unknown TakeoverControl service or method"),
            Self::AuthorizationDenied(reason) => {
                write!(formatter, "TakeoverControl authorization denied: {reason}")
            }
            Self::InvalidParticipantType(code) => write!(
                formatter,
                "participant type {code} is outside the frozen fence codes 1..=8"
            ),
            Self::InvalidParticipantGeneration => {
                formatter.write_str("participant generation must be non-zero")
            }
        }
    }
}

impl Error for TakeoverControlError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Schema(error) => Some(error),
            Self::Common(error) => Some(error),
            Self::Task(error) => Some(error),
            Self::UnknownMethod
            | Self::AuthorizationDenied(_)
            | Self::InvalidParticipantType(_)
            | Self::InvalidParticipantGeneration => None,
        }
    }
}

impl From<CompatibilityError> for TakeoverControlError {
    fn from(error: CompatibilityError) -> Self {
        Self::Schema(error)
    }
}

impl From<CommonSemanticsError> for TakeoverControlError {
    fn from(error: CommonSemanticsError) -> Self {
        Self::Common(error)
    }
}

impl From<TaskStoreError> for TakeoverControlError {
    fn from(error: TaskStoreError) -> Self {
        Self::Task(error)
    }
}

impl TakeoverControlError {
    /// Maps one typed failure to the common SABI failure class returned to
    /// IPC callers. Source classes map as follows:
    ///
    /// | Source | Code | Retry |
    /// |---|---|---|
    /// | identity proof failure (purpose/binding/revocation/validity/signature/key lookup) | `RIGHTS` | `DO_NOT_RETRY` |
    /// | authorization denial | `RIGHTS` | `DO_NOT_RETRY` |
    /// | replay conflict incl. unsigned/signed mix | `CONFLICT` | `DO_NOT_RETRY` |
    /// | takeover-state errors (receipt not pending, fence root incomplete, task generation drift) | `STATE` | `DO_NOT_RETRY` |
    /// | participant registry binding mismatch | `STATE` | `DO_NOT_RETRY` |
    /// | takeover receipt not found | `NOT_FOUND` | `DO_NOT_RETRY` |
    /// | SQLite/identity storage failure | `DURABILITY` | `RETRY_SAME_IDEMPOTENCY_KEY` (store replay is idempotent) |
    /// | schema/context validation (bad lengths, enums, missing idempotency key) | `INVALID_ARGUMENT` | `DO_NOT_RETRY` |
    /// | expired deadline | `DEADLINE` | `DO_NOT_RETRY` |
    /// | unknown service or method | `NOT_SUPPORTED` | `DO_NOT_RETRY` |
    /// | poisoned lock, unsupported schema, other local authority defects | `DRIVER` | `DO_NOT_RETRY` |
    #[must_use]
    pub fn to_sabi_failure(&self) -> SabiFailure {
        let (code, retry, safe_message) = match self {
            Self::Schema(_) => (
                SabiErrorCode::InvalidArgument,
                RetryDirective::DoNotRetry,
                "request violates the TakeoverControl payload contract",
            ),
            Self::Common(CommonSemanticsError::DeadlineExpired) => (
                SabiErrorCode::Deadline,
                RetryDirective::DoNotRetry,
                "call deadline has expired",
            ),
            Self::Common(_) => (
                SabiErrorCode::InvalidArgument,
                RetryDirective::DoNotRetry,
                "request violates the common SABI mutation contract",
            ),
            Self::AuthorizationDenied(_) => (
                SabiErrorCode::Rights,
                RetryDirective::DoNotRetry,
                "TakeoverControl authorization denied",
            ),
            Self::InvalidParticipantType(_) | Self::InvalidParticipantGeneration => (
                SabiErrorCode::InvalidArgument,
                RetryDirective::DoNotRetry,
                "participant binding is outside the frozen fence contract",
            ),
            Self::UnknownMethod => (
                SabiErrorCode::NotSupported,
                RetryDirective::DoNotRetry,
                "unknown TakeoverControl service or method",
            ),
            Self::Task(TaskStoreError::ReceiptNotFound) => (
                SabiErrorCode::NotFound,
                RetryDirective::DoNotRetry,
                "takeover receipt does not exist",
            ),
            Self::Task(TaskStoreError::ParticipantRegistryBindingMismatch) => (
                SabiErrorCode::State,
                RetryDirective::DoNotRetry,
                "participant is not bound to the frozen takeover registry",
            ),
            Self::Task(TaskStoreError::Sqlite(_)) => (
                SabiErrorCode::Durability,
                RetryDirective::RetrySameIdempotencyKey,
                "task authority storage failure; retry with the same idempotency key",
            ),
            Self::Task(TaskStoreError::DurabilityUnavailable { .. }) => (
                SabiErrorCode::Durability,
                RetryDirective::DoNotRetry,
                "task authority durability configuration is unavailable",
            ),
            Self::Task(TaskStoreError::CorruptRecord(reason)) => corrupt_record_failure(reason),
            Self::Task(TaskStoreError::BarrierSignerIdentityAuthority(identity)) => {
                identity_failure(identity)
            }
            Self::Task(_) => (
                SabiErrorCode::Driver,
                RetryDirective::DoNotRetry,
                "local task authority defect; do not retry",
            ),
        };
        SabiFailure {
            code: code.into(),
            retry: retry.into(),
            safe_message: safe_message.to_owned(),
        }
    }
}

fn corrupt_record_failure(reason: &'static str) -> (SabiErrorCode, RetryDirective, &'static str) {
    match reason {
        "takeover barrier receipt changed during replay" => (
            SabiErrorCode::Conflict,
            RetryDirective::DoNotRetry,
            "observation conflicts with the durable barrier record",
        ),
        "takeover receipt is not pending" => (
            SabiErrorCode::State,
            RetryDirective::DoNotRetry,
            "takeover receipt is not pending",
        ),
        "takeover fence set root is incomplete" => (
            SabiErrorCode::State,
            RetryDirective::DoNotRetry,
            "takeover fence manifest is unavailable",
        ),
        "takeover barrier task generation" => (
            SabiErrorCode::State,
            RetryDirective::DoNotRetry,
            "takeover barrier task generation drifted",
        ),
        "takeover barrier timestamp" => (
            SabiErrorCode::InvalidArgument,
            RetryDirective::DoNotRetry,
            "observation timestamp is negative",
        ),
        _ => (
            SabiErrorCode::Driver,
            RetryDirective::DoNotRetry,
            "local task authority defect; do not retry",
        ),
    }
}

fn identity_failure(
    identity: &nlos_identity::IdentityAuthorityError,
) -> (SabiErrorCode, RetryDirective, &'static str) {
    use nlos_identity::IdentityAuthorityError as Identity;
    match identity {
        Identity::Sqlite(_)
        | Identity::Io(_)
        | Identity::DurabilityUnavailable { .. }
        | Identity::LockPoisoned => (
            SabiErrorCode::Durability,
            RetryDirective::RetrySameIdempotencyKey,
            "identity authority storage failure; retry with the same idempotency key",
        ),
        Identity::SchemaVersionUnsupported(_)
        | Identity::IdempotencyConflict
        | Identity::GenerationExhausted
        | Identity::CorruptRecord(_) => (
            SabiErrorCode::Driver,
            RetryDirective::DoNotRetry,
            "local identity authority defect; do not retry",
        ),
        _ => (
            SabiErrorCode::Rights,
            RetryDirective::DoNotRetry,
            "barrier observation signature proof was rejected",
        ),
    }
}

pub struct TakeoverControl<'a, A> {
    tasks: &'a SqliteTaskAuthority,
    identity: &'a IdentityAuthority,
    authorizer: &'a A,
}

impl<'a, A> TakeoverControl<'a, A>
where
    A: TakeoverControlAuthorizer,
{
    #[must_use]
    pub const fn new(
        tasks: &'a SqliteTaskAuthority,
        identity: &'a IdentityAuthority,
        authorizer: &'a A,
    ) -> Self {
        Self {
            tasks,
            identity,
            authorizer,
        }
    }

    /// Handles one validated-envelope-shaped request without introducing a
    /// transport-specific RPC. The returned Envelope retains the request ID.
    /// `_now_wall_ms` is accepted for handler-surface parity with sibling
    /// control services; this path performs no wall-clock enforcement
    /// because identity key validity already bounds `observed_at_ms`.
    ///
    /// # Errors
    ///
    /// Returns typed schema/common-context/policy/authority errors. A failed
    /// request never manufactures a success Receipt.
    pub fn handle(
        &self,
        request: &Envelope,
        now_monotonic_ns: u64,
        _now_wall_ms: i64,
    ) -> Result<Envelope, TakeoverControlError> {
        if request.service != TAKEOVER_CONTROL_SERVICE {
            return Err(TakeoverControlError::UnknownMethod);
        }
        match request.method.as_str() {
            SUBMIT_BARRIER_OBSERVATION_METHOD => self.handle_submit(request, now_monotonic_ns),
            _ => Err(TakeoverControlError::UnknownMethod),
        }
    }

    fn handle_submit(
        &self,
        request: &Envelope,
        now_monotonic_ns: u64,
    ) -> Result<Envelope, TakeoverControlError> {
        // Common SABI validation runs first: MUTATION semantics enforce the
        // idempotency key that underpins request-level dedup and anti-replay.
        let context =
            validate_sabi_request_context(request, MethodSemantics::MUTATION, now_monotonic_ns)?;
        let payload = decode_submit_barrier_observation_request(&request.payload)?;
        let target = payload
            .target
            .as_ref()
            .ok_or(CompatibilityError::MissingTakeoverControlTarget)?;
        let evidence = payload
            .evidence
            .as_ref()
            .ok_or(CompatibilityError::MissingTakeoverControlEvidence)?;
        let signature = payload
            .signature
            .as_ref()
            .ok_or(CompatibilityError::MissingTakeoverControlSignature)?;
        self.authorizer
            .authorize_submit_barrier_observation(context, &payload)
            .map_err(TakeoverControlError::AuthorizationDenied)?;
        let store_request = AuthorityTakeoverBarrierReceiptRequest {
            takeover_receipt_id: ReceiptId::from_bytes(fixed16(&target.takeover_receipt_id)?),
            participant: ParticipantRecord {
                participant_type: participant_type(target.participant_type)?,
                participant_id: TaskParticipantId::from_bytes(fixed16(&target.participant_id)?),
                participant_generation: generation(target.participant_generation)?,
                admission_receipt_id: ReceiptId::from_bytes(fixed16(&target.admission_receipt_id)?),
            },
            remote_receipt_id: ReceiptId::from_bytes(fixed16(&evidence.remote_receipt_id)?),
            barrier_digest: fixed32(&evidence.barrier_digest)?,
            observed_at_ms: evidence.observed_at_ms,
        };
        let store_signature = BarrierObservationSignature {
            issuer: PrincipalId::from_bytes(fixed16(&signature.signer_principal_id)?),
            control_domain_id: ControlDomainId::from_bytes(fixed16(
                &signature.signer_control_domain_id,
            )?),
            key_id: KeyId::from_bytes(fixed16(&signature.signer_key_id)?),
            signature: fixed64(&signature.signature)?,
        };
        let record = self
            .tasks
            .record_authority_takeover_barrier_receipt_signed(
                self.identity,
                store_request,
                store_signature,
            )?;
        // Signer fields come exclusively from the identity-authority-verified
        // proof in the returned record, never from caller assertions.
        let signer = record
            .signer
            .ok_or(CompatibilityError::MissingTakeoverControlSigner)?;
        let result = BarrierObservationRecord {
            schema: Some(takeover_control_schema_identity()),
            receipt_id: record.receipt_id.into_bytes().to_vec(),
            participant_type: participant_type_code(record.participant.participant_type),
            participant_id: record.participant.participant_id.as_bytes().to_vec(),
            barrier_digest: record
                .barrier_digest
                .ok_or(CompatibilityError::InvalidTakeoverControlIdentifier)?
                .to_vec(),
            observed_at_ms: record.observed_at_ms,
            signed: true,
            signer_principal_id: signer.principal_id.as_bytes().to_vec(),
            signer_key_id: signer.key_id.as_bytes().to_vec(),
            signer_key_generation: signer.key_generation.get(),
        };
        let receipt = ReceiptReference {
            receipt_id: record.receipt_id.into_bytes().to_vec(),
        };
        Ok(response_envelope(
            request,
            context.correlation_id.clone(),
            encode_barrier_observation_record(&result)?,
            vec![receipt],
        ))
    }
}

/// Builds the typed failure envelope for one rejected request. The response
/// keeps the request ID so the IPC client can correlate it; failed
/// mutations carry no Operation/Receipt evidence because no effect happened.
#[must_use]
pub fn failure_envelope(request: &Envelope, error: &TakeoverControlError) -> Envelope {
    let correlation_id = match request.common_context.as_ref() {
        Some(envelope::CommonContext::RequestContext(context))
            if context.correlation_id.len() == REQUEST_ID_BYTES =>
        {
            context.correlation_id.clone()
        }
        _ => request.request_id.clone(),
    };
    let mut response = request.clone();
    response.payload.clear();
    response.common_context = Some(envelope::CommonContext::ResponseContext(
        SabiResponseContext {
            correlation_id,
            operation: None,
            receipts: Vec::new(),
            failure: Some(error.to_sabi_failure()),
        },
    ));
    response
}

fn participant_type(code: u32) -> Result<ParticipantType, TakeoverControlError> {
    match code {
        1 => Ok(ParticipantType::TaskStore),
        2 => Ok(ParticipantType::ArtifactHead),
        3 => Ok(ParticipantType::SemanticAdmission),
        4 => Ok(ParticipantType::ChannelTopic),
        5 => Ok(ParticipantType::DriverGateway),
        6 => Ok(ParticipantType::ResourceLedger),
        7 => Ok(ParticipantType::ProcessBinding),
        8 => Ok(ParticipantType::OperationBinding),
        other => Err(TakeoverControlError::InvalidParticipantType(other)),
    }
}

fn generation(value: u64) -> Result<Generation, TakeoverControlError> {
    NonZeroU64::new(value)
        .map(Generation::new)
        .ok_or(TakeoverControlError::InvalidParticipantGeneration)
}

fn fixed16(bytes: &[u8]) -> Result<[u8; 16], CompatibilityError> {
    bytes
        .try_into()
        .map_err(|_| CompatibilityError::InvalidTakeoverControlIdentifier)
}

fn fixed32(bytes: &[u8]) -> Result<[u8; 32], CompatibilityError> {
    bytes
        .try_into()
        .map_err(|_| CompatibilityError::InvalidTakeoverControlIdentifier)
}

fn fixed64(bytes: &[u8]) -> Result<[u8; 64], CompatibilityError> {
    bytes
        .try_into()
        .map_err(|_| CompatibilityError::InvalidTakeoverControlIdentifier)
}

fn response_envelope(
    request: &Envelope,
    correlation_id: Vec<u8>,
    payload: Vec<u8>,
    receipts: Vec<ReceiptReference>,
) -> Envelope {
    let mut response = request.clone();
    response.payload = payload;
    response.common_context = Some(envelope::CommonContext::ResponseContext(
        SabiResponseContext {
            correlation_id,
            operation: None,
            receipts,
            failure: None,
        },
    ));
    response
}
