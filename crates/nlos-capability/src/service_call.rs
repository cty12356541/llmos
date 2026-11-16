//! Shared service call binding and validation of injected policy decisions.
//!
//! The host supplies authenticated runtime identities and resolves capability
//! handles. The service resolves the target and computes the input digest from
//! the actual operation. Do not construct these facts by deserializing a wire
//! request. The DTOs are neither credentials nor proof of authentication.
//! Task execution bindings are outside this module: adapters must reject them
//! unless the runtime supports and has validated them.
//!
//! An authorizer must check the current identity, capability and ancestors,
//! rights, target, purpose, input, and expiry. This module only checks that
//! the policy reply binds the exact call, selects a presented handle, and
//! falls within the supplied time snapshot. The service must fetch fresh
//! server time and authorize again before each external side effect, each
//! new Blob block, and each replay of a protected result.

use std::{error::Error, fmt};

use nlos_types::{
    ApplicationId, Generation, IdempotencyKey, NamespaceId, OperationId, PrincipalId, ProcessId,
    ResourceAccountId, SessionId, WorkspaceId,
};

use crate::CapabilityHandle;

/// Upper bound on the number of handles presented in one service call.
pub const MAX_SERVICE_CAPABILITY_HANDLES: usize = 64;

/// Caller identity supplied by the host from authenticated runtime state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RuntimeCaller {
    pub principal: PrincipalId,
    pub application: ApplicationId,
    pub process: ProcessId,
    pub process_generation: Generation,
    pub session: SessionId,
}

/// Call metadata prepared by a trusted host.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CallBinding {
    pub caller: RuntimeCaller,
    pub idempotency_key: Option<IdempotencyKey>,
    /// Deadline in the host monotonic clock domain.
    pub deadline_monotonic_ns: Option<u64>,
    /// Handles resolved by the host from the caller's presented capabilities.
    pub capabilities: Vec<CapabilityHandle>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceMethod {
    LlmChat,
    QueryBudget,
    AttenuateBudget,
    RecordExternalUsage,
    ListTools,
    InvokeTool,
    MintReference,
    ReadReference,
    ReleaseReference,
    /// Internal `OperationControl` action, not a `ToolSandbox` wire method.
    QueryToolOperation,
    /// Internal `OperationControl` action, not a `ToolSandbox` wire method.
    CancelToolOperation,
}

impl ServiceMethod {
    #[must_use]
    pub const fn requires_idempotency_key(self) -> bool {
        match self {
            Self::QueryBudget | Self::ListTools | Self::QueryToolOperation => false,
            Self::LlmChat
            | Self::AttenuateBudget
            | Self::RecordExternalUsage
            | Self::InvokeTool
            | Self::MintReference
            | Self::ReadReference
            | Self::ReleaseReference
            | Self::CancelToolOperation => true,
        }
    }

    #[must_use]
    pub const fn requires_deadline(self) -> bool {
        match self {
            Self::LlmChat | Self::ListTools | Self::InvokeTool | Self::MintReference => true,
            Self::QueryBudget
            | Self::AttenuateBudget
            | Self::RecordExternalUsage
            | Self::ReadReference
            | Self::ReleaseReference
            | Self::QueryToolOperation
            | Self::CancelToolOperation => false,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceObject {
    Namespace(NamespaceId),
    GatewayAccount(ResourceAccountId),
    Workspace(WorkspaceId),
    ToolWasm([u8; 32]),
    BlobReference([u8; 16]),
    ToolOperation(OperationId),
}

/// Target resolved by the service, with its current generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServiceTarget {
    pub namespace: NamespaceId,
    pub object: ServiceObject,
    pub generation: Generation,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServiceAction {
    pub method: ServiceMethod,
    pub target: ServiceTarget,
    /// The service must recompute this digest from the actual operation input.
    pub input_digest: [u8; 32],
}

/// Trusted server time snapshot for one authorization check.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServiceTime {
    pub wall_ms: u64,
    /// In the same host monotonic domain as the call deadline.
    pub monotonic_ns: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceAuthorizationBinding {
    pub call: CallBinding,
    pub action: ServiceAction,
}

/// Policy decision for exactly one binding, over an inclusive wall clock window.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PolicyGrant {
    pub binding: ServiceAuthorizationBinding,
    pub capability: CapabilityHandle,
    pub valid_from_ms: u64,
    pub valid_until_ms: u64,
}

pub trait ServiceAuthorizer: Send + Sync {
    /// Check current policy for the exact binding at the supplied server time.
    ///
    /// # Errors
    ///
    /// Returns a denial or another authorization error if policy cannot grant
    /// the call.
    fn authorize(
        &self,
        binding: &ServiceAuthorizationBinding,
        now: ServiceTime,
    ) -> Result<PolicyGrant, AuthorizationError>;
}

/// Fail-closed default when no service policy has been installed.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DenyAllAuthorizer;

impl ServiceAuthorizer for DenyAllAuthorizer {
    fn authorize(
        &self,
        _binding: &ServiceAuthorizationBinding,
        _now: ServiceTime,
    ) -> Result<PolicyGrant, AuthorizationError> {
        Err(AuthorizationError::Denied)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthorizationError {
    Denied,
    MissingIdempotencyKey,
    MissingDeadline,
    DeadlineExpired,
    EmptyCapabilities,
    TooManyCapabilities,
    DuplicateCapability,
    BindingMismatch,
    CapabilityNotPresented,
    InvalidGrantWindow,
    GrantNotYetValid,
    GrantExpired,
    /// The host could not obtain trusted server time.
    TimeUnavailable,
}

impl fmt::Display for AuthorizationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Denied => "service authorization denied",
            Self::MissingIdempotencyKey => "service call requires an idempotency key",
            Self::MissingDeadline => "service call requires a deadline",
            Self::DeadlineExpired => "service call deadline expired",
            Self::EmptyCapabilities => "service call requires a capability",
            Self::TooManyCapabilities => "service call presented too many capabilities",
            Self::DuplicateCapability => "service call presented a duplicate capability",
            Self::BindingMismatch => "policy grant binding does not match the service call",
            Self::CapabilityNotPresented => "policy grant selected an unpresented capability",
            Self::InvalidGrantWindow => "policy grant has an invalid validity window",
            Self::GrantNotYetValid => "policy grant is not yet valid",
            Self::GrantExpired => "policy grant expired",
            Self::TimeUnavailable => "trusted server time is unavailable",
        })
    }
}

impl Error for AuthorizationError {}

/// A policy reply checked at the time of a specific service call.
///
/// Construct it through [`authorize_call`]. Obtain fresh time and reauthorize
/// before each external side effect, new Blob block, or protected replay.
///
/// ```compile_fail,E0451
/// use nlos_capability::service_call::{AuthorizedServiceCall, PolicyGrant};
///
/// fn from_unchecked_grant(grant: PolicyGrant) -> AuthorizedServiceCall {
///     AuthorizedServiceCall { grant }
/// }
/// ```
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthorizedServiceCall {
    grant: PolicyGrant,
}

impl AuthorizedServiceCall {
    #[must_use]
    pub const fn binding(&self) -> &ServiceAuthorizationBinding {
        &self.grant.binding
    }

    #[must_use]
    pub const fn capability(&self) -> CapabilityHandle {
        self.grant.capability
    }

    #[must_use]
    pub const fn valid_from_ms(&self) -> u64 {
        self.grant.valid_from_ms
    }

    #[must_use]
    pub const fn valid_until_ms(&self) -> u64 {
        self.grant.valid_until_ms
    }
}

/// Preflight the call, then check the injected policy reply against the same
/// service binding and server time snapshot.
///
/// # Errors
///
/// Returns an error for missing or expired call metadata, invalid presented
/// handles, a policy error, or a grant with a mismatched binding, unpresented
/// capability, or invalid validity window.
pub fn authorize_call(
    authorizer: &dyn ServiceAuthorizer,
    call: &CallBinding,
    action: &ServiceAction,
    now: ServiceTime,
) -> Result<AuthorizedServiceCall, AuthorizationError> {
    if action.method.requires_idempotency_key() && call.idempotency_key.is_none() {
        return Err(AuthorizationError::MissingIdempotencyKey);
    }
    match call.deadline_monotonic_ns {
        Some(deadline) if deadline <= now.monotonic_ns => {
            return Err(AuthorizationError::DeadlineExpired);
        }
        None if action.method.requires_deadline() => {
            return Err(AuthorizationError::MissingDeadline);
        }
        _ => {}
    }

    let capabilities = &call.capabilities;
    if capabilities.is_empty() {
        return Err(AuthorizationError::EmptyCapabilities);
    }
    if capabilities.len() > MAX_SERVICE_CAPABILITY_HANDLES {
        return Err(AuthorizationError::TooManyCapabilities);
    }
    // The cap bounds this scan to 2,016 comparisons without extra allocation.
    for (index, capability) in capabilities.iter().enumerate() {
        if capabilities[..index].contains(capability) {
            return Err(AuthorizationError::DuplicateCapability);
        }
    }

    let binding = ServiceAuthorizationBinding {
        call: call.clone(),
        action: *action,
    };
    let grant = authorizer.authorize(&binding, now)?;
    if grant.binding != binding {
        return Err(AuthorizationError::BindingMismatch);
    }
    if !capabilities.contains(&grant.capability) {
        return Err(AuthorizationError::CapabilityNotPresented);
    }
    if grant.valid_from_ms > grant.valid_until_ms {
        return Err(AuthorizationError::InvalidGrantWindow);
    }
    if now.wall_ms < grant.valid_from_ms {
        return Err(AuthorizationError::GrantNotYetValid);
    }
    if now.wall_ms > grant.valid_until_ms {
        return Err(AuthorizationError::GrantExpired);
    }
    Ok(AuthorizedServiceCall { grant })
}
