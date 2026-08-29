//! Typed `WaitControl` adapter for the durable wait registry authority
//! ([`nlos_wait::WaitAuthority`]).
//!
//! The adapter owns no canonical state. It validates the common SABI context,
//! delegates authorization, forwards one request to the matching
//! `WaitAuthority` entry point, and encodes the authoritative result into an
//! immutable response envelope, exactly mirroring the `SystemControl` and
//! `TakeoverControl` handler pattern (`handle` for typed local errors,
//! `handle_for_ipc` for a bounded failure envelope instead).
//!
//! # Method semantics
//!
//! Classification follows the system-control precedent: reads are `QUERY`,
//! writes are `MUTATION`. `register_wait`, `notify_commits` and `cancel_wait`
//! are `MUTATION` (their SABI idempotency key is the same key the durable
//! wait receipts are replayed under); `list_waits` and `inspect_wait` are
//! `QUERY`.
//!
//! # Receipt evidence
//!
//! Mutation successes carry the receipt the authority durably committed: the
//! wait row id for a registration, and the idempotency-keyed notify /
//! cancellation receipt identifier for notifications and cancellations. Query
//! responses carry no receipts. Failed requests never manufacture evidence.
//!
//! # Authorization posture (deliberately unresolved upstream)
//!
//! The real Capability/Principal model for the local control plane is still
//! an open design question; cross-Principal authentication inherits exactly
//! that unresolved state. This crate therefore only provides the local
//! trust-domain transport prefix: it never interprets
//! [`nlos_schema::sabi::v1::CapabilityHandle`] bytes itself and exposes a
//! [`WaitControlAuthorizer`] injection point with the same posture as
//! `SystemControl` — there is no default allow policy, and mere handle
//! presence is not authorization. A host adapter supplies the concrete
//! policy once the Capability/Principal authority lands.
//!
//! # Local payload schema
//!
//! Request/response payload messages are protobuf and validated fail-closed
//! here under the crate-local [`SABI_WAIT_CONTROL_SCHEMA`] identity. The
//! shared schema registry in `nlos-schema` does not yet carry a wait-control
//! descriptor; registering it there (and freezing the descriptor) is the
//! follow-up lane that owns the canonical schema objects.

use std::error::Error;
use std::fmt;
use std::sync::Arc;

use nlos_channel::ChannelAuthorityError;
use nlos_schema::sabi::v1::{
    Envelope, ReceiptReference, RetryDirective, SabiErrorCode, SabiFailure, SabiRequestContext,
    SabiResponseContext, SchemaIdentity, envelope,
};
use nlos_schema::{
    CommonSemanticsError, CompatibilityError, MethodSemantics, REQUEST_ID_BYTES,
    validate_sabi_request_context,
};
use nlos_types::{ChannelId, IdempotencyKey};
use nlos_wait::{
    BindingId, CancelDecision, CancelWaitRequest, NotifyCommitsRequest, RegisterDecision,
    RegisterWaitRequest as AuthorityRegisterWaitRequest, WaitAuthority, WaitAuthorityError, WaitId,
    WaitState, WakeReport as AuthorityWakeReport,
};

pub const WAIT_CONTROL_SERVICE: &str = "wait_control";
pub const REGISTER_WAIT_METHOD: &str = "register_wait";
pub const NOTIFY_COMMITS_METHOD: &str = "notify_commits";
pub const CANCEL_WAIT_METHOD: &str = "cancel_wait";
pub const LIST_WAITS_METHOD: &str = "list_waits";
pub const INSPECT_WAIT_METHOD: &str = "inspect_wait";

/// Crate-local payload schema identity. Canonical registration in the shared
/// `nlos-schema` registry is a follow-up lane; the identity is validated
/// fail-closed here in the meantime.
pub const SABI_WAIT_CONTROL_SCHEMA: &str = "nlos.sabi.WaitControl";

/// Wire bound for one `WaitControl` request or response payload, mirroring
/// the sibling control-plane payload bounds.
pub const MAX_WAIT_CONTROL_PAYLOAD_BYTES: usize = 64 * 1024;

/// Wire code of one frozen wait execution state. `Unspecified` stays 0 so a
/// decoding peer fails closed on an unknown or absent state.
#[must_use]
pub const fn wait_state_code(state: WaitState) -> payload::WaitStateCode {
    match state {
        WaitState::Pending => payload::WaitStateCode::Pending,
        WaitState::Woken => payload::WaitStateCode::Woken,
        WaitState::Cancelled => payload::WaitStateCode::Cancelled,
    }
}

/// Returns the v1 identity required on every `WaitControl` payload.
#[must_use]
pub fn wait_control_schema_identity() -> SchemaIdentity {
    SchemaIdentity {
        name: SABI_WAIT_CONTROL_SCHEMA.to_owned(),
        major: 1,
        minor: 0,
        critical_extension_ids: Vec::new(),
        non_critical_extension_ids: Vec::new(),
    }
}

/// Typed `WaitControl` payload messages.
///
/// The wire format is protobuf (`prost`), and every encode/decode pair
/// validates the [`SABI_WAIT_CONTROL_SCHEMA`] identity and the payload bound
/// before returning, mirroring the shared schema crate's bounded style.
pub mod payload {
    use nlos_schema::sabi::v1::SchemaIdentity;

    /// Wire enum of the durable wait execution state.
    #[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, prost::Enumeration)]
    #[repr(i32)]
    pub enum WaitStateCode {
        Unspecified = 0,
        Pending = 1,
        Woken = 2,
        Cancelled = 3,
    }

    /// One durable wait row as it exists in the `WaitAuthority`.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct WaitRecord {
        #[prost(bytes = "vec", tag = "1")]
        pub wait_id: Vec<u8>,
        #[prost(bytes = "vec", tag = "2")]
        pub binding: Vec<u8>,
        #[prost(bytes = "vec", tag = "3")]
        pub channel_id: Vec<u8>,
        #[prost(uint64, tag = "4")]
        pub channel_generation: u64,
        #[prost(bytes = "vec", tag = "5")]
        pub channel_fencing_token: Vec<u8>,
        #[prost(uint64, tag = "6")]
        pub target_sequence: u64,
        #[prost(enumeration = "WaitStateCode", tag = "7")]
        pub state: i32,
        #[prost(bytes = "vec", tag = "8")]
        pub idempotency_key: Vec<u8>,
        #[prost(uint64, tag = "9")]
        pub registered_at_ms: u64,
        #[prost(uint64, tag = "10")]
        pub woken_at_ms: u64,
        #[prost(uint64, tag = "11")]
        pub woken_up_to_sequence: u64,
        #[prost(uint64, tag = "12")]
        pub cancelled_at_ms: u64,
    }

    /// `WaitControl.register_wait` request.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct RegisterWaitRequest {
        #[prost(message, optional, tag = "1")]
        pub schema: Option<SchemaIdentity>,
        #[prost(bytes = "vec", tag = "2")]
        pub binding: Vec<u8>,
        #[prost(bytes = "vec", tag = "3")]
        pub channel_id: Vec<u8>,
        #[prost(uint64, tag = "4")]
        pub target_sequence: u64,
        #[prost(bytes = "vec", tag = "5")]
        pub idempotency_key: Vec<u8>,
        #[prost(uint64, tag = "6")]
        pub registered_at_ms: u64,
    }

    /// `WaitControl.register_wait` result.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct RegisterWaitResult {
        #[prost(message, optional, tag = "1")]
        pub schema: Option<SchemaIdentity>,
        #[prost(message, optional, tag = "2")]
        pub record: Option<WaitRecord>,
        #[prost(bool, tag = "3")]
        pub replayed: bool,
    }

    /// `WaitControl.notify_commits` request.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct NotifyCommitsRequest {
        #[prost(message, optional, tag = "1")]
        pub schema: Option<SchemaIdentity>,
        #[prost(bytes = "vec", tag = "2")]
        pub channel_id: Vec<u8>,
        #[prost(uint64, tag = "3")]
        pub up_to_sequence: u64,
        #[prost(uint64, tag = "4")]
        pub notified_at_ms: u64,
        #[prost(bytes = "vec", tag = "5")]
        pub idempotency_key: Vec<u8>,
    }

    /// `WaitControl.notify_commits` result: the exact wake report.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct WakeReport {
        #[prost(message, optional, tag = "1")]
        pub schema: Option<SchemaIdentity>,
        #[prost(message, repeated, tag = "2")]
        pub woken: Vec<WaitRecord>,
    }

    /// `WaitControl.cancel_wait` request.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct CancelWaitRequest {
        #[prost(message, optional, tag = "1")]
        pub schema: Option<SchemaIdentity>,
        #[prost(bytes = "vec", tag = "2")]
        pub wait_id: Vec<u8>,
        #[prost(uint64, tag = "3")]
        pub cancelled_at_ms: u64,
        #[prost(bytes = "vec", tag = "4")]
        pub idempotency_key: Vec<u8>,
    }

    /// `WaitControl.cancel_wait` result.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct CancelWaitResult {
        #[prost(message, optional, tag = "1")]
        pub schema: Option<SchemaIdentity>,
        #[prost(message, optional, tag = "2")]
        pub record: Option<WaitRecord>,
        #[prost(bool, tag = "3")]
        pub replayed: bool,
    }

    /// `WaitControl.list_waits` request. An empty `filter_channel_id` lists
    /// every wait; otherwise the list is scoped to one Channel.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct ListWaitsRequest {
        #[prost(message, optional, tag = "1")]
        pub schema: Option<SchemaIdentity>,
        #[prost(bytes = "vec", tag = "2")]
        pub filter_channel_id: Vec<u8>,
    }

    /// `WaitControl.list_waits` result: every state, in the authority's
    /// durable enumeration order.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct ListWaitsResult {
        #[prost(message, optional, tag = "1")]
        pub schema: Option<SchemaIdentity>,
        #[prost(message, repeated, tag = "2")]
        pub waits: Vec<WaitRecord>,
    }

    /// `WaitControl.inspect_wait` request.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct InspectWaitRequest {
        #[prost(message, optional, tag = "1")]
        pub schema: Option<SchemaIdentity>,
        #[prost(bytes = "vec", tag = "2")]
        pub wait_id: Vec<u8>,
    }

    /// `WaitControl.inspect_wait` result: one validated durable row.
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct InspectWaitResult {
        #[prost(message, optional, tag = "1")]
        pub schema: Option<SchemaIdentity>,
        #[prost(message, optional, tag = "2")]
        pub record: Option<WaitRecord>,
    }
}

/// Policy boundary used by every `WaitControl` entry point. Implementations
/// are expected to validate the supplied capability handles against their
/// authority; mere handle presence is not authorization.
///
/// The real Capability/Principal model for the local control plane is still
/// undecided, and cross-Principal authentication inherits that unresolved
/// state; this trait therefore only names the local trust-domain injection
/// point and adds no authentication semantics of its own.
pub trait WaitControlAuthorizer {
    /// Authorizes one durable wait registration.
    ///
    /// # Errors
    ///
    /// Returns a static policy class safe for the local service log.
    fn authorize_register_wait(
        &self,
        context: &SabiRequestContext,
        request: &payload::RegisterWaitRequest,
    ) -> Result<(), &'static str>;

    /// Authorizes one explicit commit notification.
    ///
    /// # Errors
    ///
    /// Returns a static policy class safe for the local service log.
    fn authorize_notify_commits(
        &self,
        context: &SabiRequestContext,
        request: &payload::NotifyCommitsRequest,
    ) -> Result<(), &'static str>;

    /// Authorizes one wait cancellation.
    ///
    /// # Errors
    ///
    /// Returns a static policy class safe for the local service log.
    fn authorize_cancel_wait(
        &self,
        context: &SabiRequestContext,
        request: &payload::CancelWaitRequest,
    ) -> Result<(), &'static str>;

    /// Authorizes one wait enumeration.
    ///
    /// # Errors
    ///
    /// Returns a static policy class safe for the local service log.
    fn authorize_list_waits(
        &self,
        context: &SabiRequestContext,
        request: &payload::ListWaitsRequest,
    ) -> Result<(), &'static str>;

    /// Authorizes one wait inspection.
    ///
    /// # Errors
    ///
    /// Returns a static policy class safe for the local service log.
    fn authorize_inspect_wait(
        &self,
        context: &SabiRequestContext,
        request: &payload::InspectWaitRequest,
    ) -> Result<(), &'static str>;
}

#[derive(Debug)]
pub enum WaitControlError {
    /// The request payload violated the wire contract: malformed protobuf,
    /// an oversized frame, or a missing/unknown payload schema identity.
    Payload(CompatibilityError),
    /// The common SABI request context was missing or invalid.
    Common(CommonSemanticsError),
    UnknownMethod,
    AuthorizationDenied(&'static str),
    /// The mutation payload's idempotency key does not match the SABI request
    /// context key. The authority replays durable receipts under that key, so
    /// a binding drift would silently fork the replay semantics.
    IdempotencyKeyMismatch,
    /// A payload decoded fine but carried a field outside the bounded
    /// contract (an identifier that is not exactly 16 bytes).
    InvalidIdentifier,
    /// The `WaitAuthority` rejected the request.
    Wait(WaitAuthorityError),
}

impl fmt::Display for WaitControlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Payload(error) => write!(formatter, "invalid WaitControl payload: {error}"),
            Self::Common(error) => write!(formatter, "invalid WaitControl context: {error}"),
            Self::UnknownMethod => formatter.write_str("unknown WaitControl service or method"),
            Self::AuthorizationDenied(reason) => {
                write!(formatter, "WaitControl authorization denied: {reason}")
            }
            Self::IdempotencyKeyMismatch => formatter
                .write_str("WaitControl idempotency key does not match the request context"),
            Self::InvalidIdentifier => {
                formatter.write_str("WaitControl payload identifier length is out of contract")
            }
            Self::Wait(error) => write!(formatter, "WaitAuthority rejected WaitControl: {error}"),
        }
    }
}

impl Error for WaitControlError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Payload(error) => Some(error),
            Self::Common(error) => Some(error),
            Self::Wait(error) => Some(error),
            Self::UnknownMethod
            | Self::AuthorizationDenied(_)
            | Self::IdempotencyKeyMismatch
            | Self::InvalidIdentifier => None,
        }
    }
}

impl From<CompatibilityError> for WaitControlError {
    fn from(error: CompatibilityError) -> Self {
        Self::Payload(error)
    }
}

impl From<CommonSemanticsError> for WaitControlError {
    fn from(error: CommonSemanticsError) -> Self {
        Self::Common(error)
    }
}

impl From<WaitAuthorityError> for WaitControlError {
    fn from(error: WaitAuthorityError) -> Self {
        Self::Wait(error)
    }
}

impl WaitControlError {
    /// Maps one local rejection to the bounded common SABI failure class.
    ///
    /// The mapping deliberately never includes the source error's display
    /// text. `SQLite` messages, static policy reasons and durable-record
    /// details are local diagnostics and must not cross the `WaitControl`
    /// boundary. A failed registration/notification/cancellation carries no
    /// receipt evidence, so a storage error may be retried with the original
    /// idempotency key while a contract or state error is terminal for that
    /// request.
    ///
    /// | Source | Code | Retry |
    /// |---|---|---|
    /// | payload contract / identifier binding | `INVALID_ARGUMENT` | `DO_NOT_RETRY` |
    /// | expired deadline | `DEADLINE` | `DO_NOT_RETRY` |
    /// | authorization denial | `RIGHTS` | `DO_NOT_RETRY` |
    /// | unknown method | `NOT_SUPPORTED` | `DO_NOT_RETRY` |
    /// | payload/context idempotency binding | `CONFLICT` | `DO_NOT_RETRY` |
    /// | wait/Channel object absent | `NOT_FOUND` | `DO_NOT_RETRY` |
    /// | idempotency rebinding | `CONFLICT` | `DO_NOT_RETRY` |
    /// | wait state mismatch (not `PENDING`) | `STATE` | `DO_NOT_RETRY` |
    /// | wait contract violation (zero binding/sequence/timestamp) | `INVALID_ARGUMENT` | `DO_NOT_RETRY` |
    /// | SQLite storage failure | `DURABILITY` | `RETRY_SAME_IDEMPOTENCY_KEY` |
    /// | unavailable durability / corrupt local state / poisoned lock | `DURABILITY`/`DRIVER` | `DO_NOT_RETRY` |
    #[must_use]
    pub fn to_sabi_failure(&self) -> SabiFailure {
        let (code, retry, safe_message) = match self {
            Self::Payload(_) => (
                SabiErrorCode::InvalidArgument,
                RetryDirective::DoNotRetry,
                "request violates the WaitControl payload contract",
            ),
            Self::Common(CommonSemanticsError::DeadlineExpired) => (
                SabiErrorCode::Deadline,
                RetryDirective::DoNotRetry,
                "call deadline has expired",
            ),
            Self::Common(_) => (
                SabiErrorCode::InvalidArgument,
                RetryDirective::DoNotRetry,
                "request violates the common SABI contract",
            ),
            Self::AuthorizationDenied(_) => (
                SabiErrorCode::Rights,
                RetryDirective::DoNotRetry,
                "WaitControl authorization denied",
            ),
            Self::IdempotencyKeyMismatch => (
                SabiErrorCode::Conflict,
                RetryDirective::DoNotRetry,
                "request idempotency key conflicts with the payload key",
            ),
            Self::UnknownMethod => (
                SabiErrorCode::NotSupported,
                RetryDirective::DoNotRetry,
                "unknown WaitControl service or method",
            ),
            Self::InvalidIdentifier => (
                SabiErrorCode::InvalidArgument,
                RetryDirective::DoNotRetry,
                "request carries an identifier outside the WaitControl contract",
            ),
            Self::Wait(error) => wait_authority_failure(error),
        };
        SabiFailure {
            code: code.into(),
            retry: retry.into(),
            safe_message: safe_message.to_owned(),
        }
    }
}

fn wait_authority_failure(
    error: &WaitAuthorityError,
) -> (SabiErrorCode, RetryDirective, &'static str) {
    use WaitAuthorityError as Wait;

    match error {
        Wait::Channel(ChannelAuthorityError::ChannelNotFound(_)) => (
            SabiErrorCode::NotFound,
            RetryDirective::DoNotRetry,
            "referenced Channel does not exist",
        ),
        Wait::Channel(_) => (
            SabiErrorCode::Driver,
            RetryDirective::DoNotRetry,
            "Channel authority rejected the wait operation",
        ),
        Wait::Sqlite(_) => (
            SabiErrorCode::Durability,
            RetryDirective::RetrySameIdempotencyKey,
            "wait authority storage failure; retry with the same idempotency key",
        ),
        Wait::Io(_) | Wait::DurabilityUnavailable { .. } => (
            SabiErrorCode::Durability,
            RetryDirective::DoNotRetry,
            "wait authority durability configuration is unavailable",
        ),
        Wait::SchemaVersionUnsupported(_) => (
            SabiErrorCode::Driver,
            RetryDirective::DoNotRetry,
            "wait authority schema version is unsupported",
        ),
        Wait::WaitNotFound(_) => (
            SabiErrorCode::NotFound,
            RetryDirective::DoNotRetry,
            "requested wait does not exist",
        ),
        Wait::WaitNotPending(_) => (
            SabiErrorCode::State,
            RetryDirective::DoNotRetry,
            "wait state rejects this request",
        ),
        Wait::IdempotencyConflict => (
            SabiErrorCode::Conflict,
            RetryDirective::DoNotRetry,
            "idempotency key conflicts with durable wait state",
        ),
        Wait::InvalidBinding | Wait::InvalidSequence(_) | Wait::InvalidTimestamp(_) => (
            SabiErrorCode::InvalidArgument,
            RetryDirective::DoNotRetry,
            "request violates the wait authority contract",
        ),
        Wait::CorruptRecord(_) | Wait::LockPoisoned => (
            SabiErrorCode::Driver,
            RetryDirective::DoNotRetry,
            "local wait authority defect; do not retry",
        ),
    }
}

/// Typed `WaitControl` service bound to one shared durable
/// [`WaitAuthority`].
///
/// The authorizer is injected at construction like the sibling control
/// services; there is no default policy.
pub struct WaitControlService<A> {
    waits: Arc<WaitAuthority>,
    authorizer: A,
}

impl<A> WaitControlService<A>
where
    A: WaitControlAuthorizer,
{
    #[must_use]
    pub const fn new(waits: Arc<WaitAuthority>, authorizer: A) -> Self {
        Self { waits, authorizer }
    }

    /// Handles one validated-envelope-shaped request without introducing a
    /// transport-specific RPC. The returned Envelope retains the request ID.
    /// `_now_wall_ms` is accepted for handler-surface parity with sibling
    /// control services; the wait authority owns every durable timestamp, so
    /// this path performs no wall-clock enforcement.
    ///
    /// # Errors
    ///
    /// Returns typed payload/common-context/policy/authority errors. A failed
    /// request never manufactures a success Receipt.
    pub fn handle(
        &self,
        request: &Envelope,
        now_monotonic_ns: u64,
        _now_wall_ms: i64,
    ) -> Result<Envelope, WaitControlError> {
        if request.service != WAIT_CONTROL_SERVICE {
            return Err(WaitControlError::UnknownMethod);
        }
        match request.method.as_str() {
            REGISTER_WAIT_METHOD => self.handle_register_wait(request, now_monotonic_ns),
            NOTIFY_COMMITS_METHOD => self.handle_notify_commits(request, now_monotonic_ns),
            CANCEL_WAIT_METHOD => self.handle_cancel_wait(request, now_monotonic_ns),
            LIST_WAITS_METHOD => self.handle_list_waits(request, now_monotonic_ns),
            INSPECT_WAIT_METHOD => self.handle_inspect_wait(request, now_monotonic_ns),
            _ => Err(WaitControlError::UnknownMethod),
        }
    }

    /// Handles one request for a local IPC adapter and always returns a typed
    /// response envelope. Handler errors are converted with
    /// [`failure_envelope`] before framing; transport I/O failures remain the
    /// caller's responsibility. Use [`Self::handle`] when the caller needs
    /// to inspect the local error instead.
    #[must_use]
    pub fn handle_for_ipc(
        &self,
        request: &Envelope,
        now_monotonic_ns: u64,
        now_wall_ms: i64,
    ) -> Envelope {
        match self.handle(request, now_monotonic_ns, now_wall_ms) {
            Ok(response) => response,
            Err(error) => failure_envelope(request, &error),
        }
    }

    fn handle_register_wait(
        &self,
        request: &Envelope,
        now_monotonic_ns: u64,
    ) -> Result<Envelope, WaitControlError> {
        // Common SABI validation runs first: MUTATION semantics enforce the
        // idempotency key that the authority replays registrations under.
        let context =
            validate_sabi_request_context(request, MethodSemantics::MUTATION, now_monotonic_ns)?;
        let payload = decode_register_wait_request(&request.payload)?;
        let idempotency_key = bind_idempotency_key(context, &payload.idempotency_key)?;
        self.authorizer
            .authorize_register_wait(context, &payload)
            .map_err(WaitControlError::AuthorizationDenied)?;
        let decision = self.waits.register_wait(AuthorityRegisterWaitRequest {
            binding: BindingId::from_bytes(fixed16(&payload.binding)?),
            channel_id: ChannelId::from_bytes(fixed16(&payload.channel_id)?),
            target_sequence: payload.target_sequence,
            idempotency_key: IdempotencyKey::from_bytes(idempotency_key),
            registered_at_ms: payload.registered_at_ms,
        })?;
        let replayed = matches!(decision, RegisterDecision::Replayed(_));
        let record = decision.record();
        let result = payload::RegisterWaitResult {
            schema: Some(wait_control_schema_identity()),
            record: Some(wait_record_payload(&record)),
            replayed,
        };
        // The durable wait row is the registration receipt; its
        // authority-derived id is the receipt reference.
        let receipt = ReceiptReference {
            receipt_id: record.wait_id.as_bytes().to_vec(),
        };
        Ok(response_envelope(
            request,
            context.correlation_id.clone(),
            encode_register_wait_result(&result)?,
            vec![receipt],
        ))
    }

    fn handle_notify_commits(
        &self,
        request: &Envelope,
        now_monotonic_ns: u64,
    ) -> Result<Envelope, WaitControlError> {
        let context =
            validate_sabi_request_context(request, MethodSemantics::MUTATION, now_monotonic_ns)?;
        let payload = decode_notify_commits_request(&request.payload)?;
        let idempotency_key = bind_idempotency_key(context, &payload.idempotency_key)?;
        self.authorizer
            .authorize_notify_commits(context, &payload)
            .map_err(WaitControlError::AuthorizationDenied)?;
        let report = self.waits.notify_commits(NotifyCommitsRequest {
            channel_id: ChannelId::from_bytes(fixed16(&payload.channel_id)?),
            up_to_sequence: payload.up_to_sequence,
            notified_at_ms: payload.notified_at_ms,
            idempotency_key: IdempotencyKey::from_bytes(idempotency_key),
        })?;
        let result = wake_report_payload(&report);
        // The durable notify receipt is keyed by the request idempotency
        // key; that key is its receipt reference (present even for an empty
        // wake set, which is a durable replayable success).
        let receipt = ReceiptReference {
            receipt_id: fixed16(&payload.idempotency_key)?.to_vec(),
        };
        Ok(response_envelope(
            request,
            context.correlation_id.clone(),
            encode_notify_commits_result(&result)?,
            vec![receipt],
        ))
    }

    fn handle_cancel_wait(
        &self,
        request: &Envelope,
        now_monotonic_ns: u64,
    ) -> Result<Envelope, WaitControlError> {
        let context =
            validate_sabi_request_context(request, MethodSemantics::MUTATION, now_monotonic_ns)?;
        let payload = decode_cancel_wait_request(&request.payload)?;
        let idempotency_key = bind_idempotency_key(context, &payload.idempotency_key)?;
        self.authorizer
            .authorize_cancel_wait(context, &payload)
            .map_err(WaitControlError::AuthorizationDenied)?;
        let decision = self.waits.cancel_wait(CancelWaitRequest {
            wait_id: WaitId::from_bytes(fixed16(&payload.wait_id)?),
            cancelled_at_ms: payload.cancelled_at_ms,
            idempotency_key: IdempotencyKey::from_bytes(idempotency_key),
        })?;
        let replayed = matches!(decision, CancelDecision::Replayed(_));
        let record = decision.record();
        let result = payload::CancelWaitResult {
            schema: Some(wait_control_schema_identity()),
            record: Some(wait_record_payload(&record)),
            replayed,
        };
        // The durable cancellation receipt is keyed by the request
        // idempotency key; that key is its receipt reference.
        let receipt = ReceiptReference {
            receipt_id: fixed16(&payload.idempotency_key)?.to_vec(),
        };
        Ok(response_envelope(
            request,
            context.correlation_id.clone(),
            encode_cancel_wait_result(&result)?,
            vec![receipt],
        ))
    }

    fn handle_list_waits(
        &self,
        request: &Envelope,
        now_monotonic_ns: u64,
    ) -> Result<Envelope, WaitControlError> {
        let context =
            validate_sabi_request_context(request, MethodSemantics::QUERY, now_monotonic_ns)?;
        let payload = decode_list_waits_request(&request.payload)?;
        self.authorizer
            .authorize_list_waits(context, &payload)
            .map_err(WaitControlError::AuthorizationDenied)?;
        let filter = if payload.filter_channel_id.is_empty() {
            None
        } else {
            Some(ChannelId::from_bytes(fixed16(&payload.filter_channel_id)?))
        };
        let records = self.waits.list_waits(filter)?;
        let result = payload::ListWaitsResult {
            schema: Some(wait_control_schema_identity()),
            waits: records.iter().map(wait_record_payload).collect(),
        };
        Ok(response_envelope(
            request,
            context.correlation_id.clone(),
            encode_list_waits_result(&result)?,
            Vec::new(),
        ))
    }

    fn handle_inspect_wait(
        &self,
        request: &Envelope,
        now_monotonic_ns: u64,
    ) -> Result<Envelope, WaitControlError> {
        let context =
            validate_sabi_request_context(request, MethodSemantics::QUERY, now_monotonic_ns)?;
        let payload = decode_inspect_wait_request(&request.payload)?;
        self.authorizer
            .authorize_inspect_wait(context, &payload)
            .map_err(WaitControlError::AuthorizationDenied)?;
        let record = self
            .waits
            .inspect_wait(WaitId::from_bytes(fixed16(&payload.wait_id)?))?;
        let result = payload::InspectWaitResult {
            schema: Some(wait_control_schema_identity()),
            record: Some(wait_record_payload(&record)),
        };
        Ok(response_envelope(
            request,
            context.correlation_id.clone(),
            encode_inspect_wait_result(&result)?,
            Vec::new(),
        ))
    }
}

fn wake_report_payload(report: &AuthorityWakeReport) -> payload::WakeReport {
    payload::WakeReport {
        schema: Some(wait_control_schema_identity()),
        woken: report.woken.iter().map(wait_record_payload).collect(),
    }
}

/// Converts one typed authority row into its wire record. Every field is a
/// fixed-length array or an enum in the authority model, so the conversion
/// is total and needs no validation.
#[must_use]
pub fn wait_record_payload(record: &nlos_wait::WaitRecord) -> payload::WaitRecord {
    payload::WaitRecord {
        wait_id: record.wait_id.as_bytes().to_vec(),
        binding: record.binding.as_bytes().to_vec(),
        channel_id: record.channel_id.as_bytes().to_vec(),
        channel_generation: record.channel_generation.get(),
        channel_fencing_token: record.channel_fencing_token.to_vec(),
        target_sequence: record.target_sequence,
        state: i32::from(wait_state_code(record.state)),
        idempotency_key: record.idempotency_key.as_bytes().to_vec(),
        registered_at_ms: record.registered_at_ms,
        woken_at_ms: record.woken_at_ms,
        woken_up_to_sequence: record.woken_up_to_sequence,
        cancelled_at_ms: record.cancelled_at_ms,
    }
}

fn fixed16(bytes: &[u8]) -> Result<[u8; 16], WaitControlError> {
    bytes
        .try_into()
        .map_err(|_| WaitControlError::InvalidIdentifier)
}

/// Binds the payload idempotency key to the SABI request context key, the
/// same ordering system-control uses: the identity check runs before the
/// authorizer so a rebinding attempt can never spend a policy decision.
fn bind_idempotency_key(
    context: &SabiRequestContext,
    payload_key: &[u8],
) -> Result<[u8; 16], WaitControlError> {
    if payload_key != context.idempotency_key {
        return Err(WaitControlError::IdempotencyKeyMismatch);
    }
    fixed16(payload_key)
}

/// Builds a typed failure envelope for one rejected request.
///
/// The request ID and service/method are retained for transport correlation,
/// while the payload and all Operation/Receipt evidence are cleared. A
/// malformed correlation ID cannot be echoed into a response, so a valid
/// request ID is preferred and an all-zero bounded correlation is used only
/// when both request identifiers are malformed.
#[must_use]
pub fn failure_envelope(request: &Envelope, error: &WaitControlError) -> Envelope {
    let correlation_id = match request.common_context.as_ref() {
        Some(envelope::CommonContext::RequestContext(context))
            if context.correlation_id.len() == REQUEST_ID_BYTES =>
        {
            context.correlation_id.clone()
        }
        _ if request.request_id.len() == REQUEST_ID_BYTES => request.request_id.clone(),
        _ => vec![0; REQUEST_ID_BYTES],
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

fn validate_payload_identity(identity: Option<&SchemaIdentity>) -> Result<(), CompatibilityError> {
    let Some(identity) = identity else {
        return Err(CompatibilityError::MissingSchemaIdentity);
    };
    if identity.name != SABI_WAIT_CONTROL_SCHEMA {
        return Err(CompatibilityError::UnknownSchema(identity.name.clone()));
    }
    if identity.major != 1 {
        return Err(CompatibilityError::UnsupportedMajor {
            schema: identity.name.clone(),
            got: identity.major,
            supported: 1,
        });
    }
    if let Some(extension_id) = identity.critical_extension_ids.first().copied() {
        return Err(CompatibilityError::UnsupportedCriticalExtension {
            schema: identity.name.clone(),
            extension_id,
        });
    }
    Ok(())
}

fn encode_bounded(
    message: &impl prost::Message,
    maximum: usize,
) -> Result<Vec<u8>, CompatibilityError> {
    let wire = message.encode_to_vec();
    if wire.len() > maximum {
        return Err(CompatibilityError::FrameTooLarge {
            actual: wire.len(),
            maximum,
        });
    }
    Ok(wire)
}

fn decode_bounded<M: prost::Message + Default>(
    wire: &[u8],
    maximum: usize,
) -> Result<M, CompatibilityError> {
    if wire.len() > maximum {
        return Err(CompatibilityError::FrameTooLarge {
            actual: wire.len(),
            maximum,
        });
    }
    M::decode(wire).map_err(|error| CompatibilityError::MalformedProtobuf(error.to_string()))
}

macro_rules! bounded_codec {
    ($encode:ident, $decode:ident, $ty:ty) => {
        /// Encodes a bounded, identity-validated `WaitControl` payload.
        ///
        /// # Errors
        ///
        /// Returns a compatibility error for an invalid schema identity or an
        /// oversized frame.
        pub fn $encode(payload: &$ty) -> Result<Vec<u8>, CompatibilityError> {
            validate_payload_identity(payload.schema.as_ref())?;
            encode_bounded(payload, MAX_WAIT_CONTROL_PAYLOAD_BYTES)
        }

        /// Decodes a bounded, identity-validated `WaitControl` payload.
        ///
        /// # Errors
        ///
        /// Returns a compatibility error for malformed, incompatible, or
        /// oversized input.
        pub fn $decode(wire: &[u8]) -> Result<$ty, CompatibilityError> {
            let payload: $ty = decode_bounded(wire, MAX_WAIT_CONTROL_PAYLOAD_BYTES)?;
            validate_payload_identity(payload.schema.as_ref())?;
            Ok(payload)
        }
    };
}

bounded_codec!(
    encode_register_wait_request,
    decode_register_wait_request,
    payload::RegisterWaitRequest
);
bounded_codec!(
    encode_notify_commits_request,
    decode_notify_commits_request,
    payload::NotifyCommitsRequest
);
bounded_codec!(
    encode_cancel_wait_request,
    decode_cancel_wait_request,
    payload::CancelWaitRequest
);
bounded_codec!(
    encode_list_waits_request,
    decode_list_waits_request,
    payload::ListWaitsRequest
);
bounded_codec!(
    encode_inspect_wait_request,
    decode_inspect_wait_request,
    payload::InspectWaitRequest
);
bounded_codec!(
    encode_register_wait_result,
    decode_register_wait_result,
    payload::RegisterWaitResult
);
bounded_codec!(
    encode_notify_commits_result,
    decode_notify_commits_result,
    payload::WakeReport
);
bounded_codec!(
    encode_cancel_wait_result,
    decode_cancel_wait_result,
    payload::CancelWaitResult
);
bounded_codec!(
    encode_list_waits_result,
    decode_list_waits_result,
    payload::ListWaitsResult
);
bounded_codec!(
    encode_inspect_wait_result,
    decode_inspect_wait_result,
    payload::InspectWaitResult
);
