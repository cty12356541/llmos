//! Daemon-mounted `ServiceDirectory` adapter (`L-011`, W82-A): the frozen
//! v1.0 `service = "service_directory"` channel behind the system-control
//! daemon's five-way dispatch, answering `negotiate` (the only method the
//! frozen contract carries) over an immutable [`SnapshotDirectory`] snapshot
//! of the daemon's own five-service offering.
//!
//! Negotiation semantics are the sibling crate's [`SnapshotDirectory::negotiate`]
//! verbatim (deterministic version/feature/transport selection); a request
//! that negotiates no binding gets the typed [`DirectoryError`] result riding
//! the normal response envelope, exactly like the conformance reference
//! (`nlos-ipc/src/bin/nlos-directory-chain.rs::serve_directory`). Only
//! request-shape rejections (undecodable payload, malformed context, unknown
//! method) become failure envelopes — and those keep the frozen invariants
//! (payload cleared, no receipts, no operation, correlation echo).

use std::path::Path;

use nlos_schema::sabi::v1::{
    Envelope, RetryDirective, SabiErrorCode, SabiFailure, SabiResponseContext, ServiceCandidate,
    ServiceVersion, envelope,
};
use nlos_schema::{
    MethodSemantics, REQUEST_ID_BYTES, SABI_CHANNEL_CONTROL_SCHEMA, SABI_OPERATION_CONTROL_SCHEMA,
    SABI_SERVICE_DIRECTORY_SCHEMA, SABI_SYSTEM_CONTROL_SCHEMA, SABI_WAIT_CONTROL_SCHEMA,
    decode_negotiate_service_request, encode_negotiate_service_response,
    validate_sabi_request_context,
};
use nlos_service_directory::{RegistrationError, ServiceRegistration, SnapshotDirectory};
use sha2::{Digest, Sha256};

use crate::SYSTEM_CONTROL_SERVICE;
use crate::channel_control::CHANNEL_CONTROL_SERVICE;
use nlos_wait_control::WAIT_CONTROL_SERVICE;

use crate::auth::operation_control::OPERATION_CONTROL_SERVICE;

/// Frozen v1.0 service name (matches the conformance harness constant).
pub const SERVICE_DIRECTORY_SERVICE: &str = "service_directory";
/// Frozen v1.0 method name (the directory contract carries only negotiate).
pub const NEGOTIATE_METHOD: &str = "negotiate";

/// Domain separator for daemon directory binding ids: deterministic per
/// (service, endpoint role), so the snapshot is byte-stable across restarts
/// with the same endpoint paths.
const BINDING_ID_DOMAIN: &[u8] = b"llmos/system-control-daemon/directory-binding/v1";

fn binding_id(service: &str, role: &str) -> Vec<u8> {
    let digest = Sha256::new()
        .chain_update(BINDING_ID_DOMAIN)
        .chain_update(service.as_bytes())
        .chain_update(role.as_bytes())
        .finalize();
    digest[..16].to_vec()
}

#[cfg(unix)]
fn endpoint_kind() -> nlos_schema::sabi::v1::LocalTransportKind {
    nlos_schema::sabi::v1::LocalTransportKind::UnixSocket
}

#[cfg(windows)]
fn endpoint_kind() -> nlos_schema::sabi::v1::LocalTransportKind {
    nlos_schema::sabi::v1::LocalTransportKind::WindowsNamedPipe
}

/// One (name, schema name, major, minor) service offering row.
type ServiceOffering = (&'static str, &'static str, u32, u32);

/// The five frozen v1.0 services the daemon serves on both endpoints.
const SERVICE_OFFERINGS: [ServiceOffering; 5] = [
    (SYSTEM_CONTROL_SERVICE, SABI_SYSTEM_CONTROL_SCHEMA, 1, 0),
    (CHANNEL_CONTROL_SERVICE, SABI_CHANNEL_CONTROL_SCHEMA, 1, 0),
    (WAIT_CONTROL_SERVICE, SABI_WAIT_CONTROL_SCHEMA, 1, 0),
    (
        OPERATION_CONTROL_SERVICE,
        SABI_OPERATION_CONTROL_SCHEMA,
        1,
        0,
    ),
    (
        SERVICE_DIRECTORY_SERVICE,
        SABI_SERVICE_DIRECTORY_SCHEMA,
        1,
        0,
    ),
];

/// Builds the daemon's directory registrations: every one of the five frozen
/// services bound to BOTH local endpoints (authenticated and plain) — ten
/// candidates, deterministic binding ids per (service, role), generation 1,
/// no feature gates (the frozen contracts carry none).
///
/// # Errors
///
/// Returns [`RegistrationError`] when the snapshot validation rejects a
/// registration (duplicate binding ids, invalid names, response bounds).
pub fn daemon_registrations(
    auth_endpoint: &Path,
    plain_endpoint: &Path,
) -> Result<Vec<ServiceRegistration>, RegistrationError> {
    let mut registrations = Vec::new();
    for (service, schema_name, major, minor) in SERVICE_OFFERINGS {
        for (role, endpoint) in [("auth", auth_endpoint), ("plain", plain_endpoint)] {
            let kind = endpoint_kind();
            registrations.push(ServiceRegistration {
                candidate: ServiceCandidate {
                    binding_id: binding_id(service, role),
                    generation: 1,
                    service: service.to_owned(),
                    version: Some(ServiceVersion {
                        schema_name: schema_name.to_owned(),
                        major,
                        minor,
                    }),
                    feature_ids: Vec::new(),
                    transport_kinds: vec![kind.into()],
                },
                endpoint: nlos_schema::sabi::v1::LocalEndpoint {
                    kind: kind.into(),
                    address: endpoint.to_string_lossy().into_owned(),
                },
            });
        }
    }
    Ok(registrations)
}

/// Typed `ServiceDirectory` dispatch failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ServiceDirectoryError {
    UnknownMethod,
    InvalidRequest,
    Encode,
}

impl ServiceDirectoryError {
    fn to_sabi_failure(self) -> SabiFailure {
        let (code, message): (SabiErrorCode, &str) = match self {
            Self::UnknownMethod => (
                SabiErrorCode::NotSupported,
                "unknown ServiceDirectory service or method",
            ),
            Self::InvalidRequest => (
                SabiErrorCode::InvalidArgument,
                "request violates the ServiceDirectory negotiate contract",
            ),
            Self::Encode => (
                SabiErrorCode::Driver,
                "ServiceDirectory result encoding failed",
            ),
        };
        SabiFailure {
            code: code.into(),
            retry: RetryDirective::DoNotRetry.into(),
            safe_message: message.to_owned(),
        }
    }
}

fn failure_envelope(request: &Envelope, error: ServiceDirectoryError) -> Envelope {
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

/// Immutable `ServiceDirectory` surface over one validated snapshot: answers
/// `service_directory`/`negotiate` with the sibling crate's deterministic
/// selection. The daemon assembles the snapshot once per `assemble` and every
/// round serves from it, so negotiations cannot observe a half-updated
/// offering.
#[derive(Clone, Debug, Default)]
pub struct ServiceDirectorySurface {
    directory: SnapshotDirectory,
}

impl ServiceDirectorySurface {
    /// Validates and mounts one snapshot.
    ///
    /// # Errors
    ///
    /// Returns the snapshot validation error (bounds, duplicate bindings).
    pub fn new(
        registrations: impl IntoIterator<Item = ServiceRegistration>,
    ) -> Result<Self, RegistrationError> {
        Ok(Self {
            directory: SnapshotDirectory::new(registrations)?,
        })
    }

    /// Mounts an already-validated snapshot (the daemon's assembly path).
    #[must_use]
    pub fn from_snapshot(directory: SnapshotDirectory) -> Self {
        Self { directory }
    }

    /// Handles one validated-envelope-shaped request and always returns a
    /// typed response envelope. `_now_wall_ms` is accepted for handler-surface
    /// parity with the sibling control services; the directory is a pure
    /// query face.
    #[must_use]
    pub fn handle_for_ipc(
        &self,
        request: &Envelope,
        now_monotonic_ns: u64,
        _now_wall_ms: i64,
    ) -> Envelope {
        match self.handle(request, now_monotonic_ns) {
            Ok(response) => response,
            Err(error) => failure_envelope(request, error),
        }
    }

    fn handle(
        &self,
        request: &Envelope,
        now_monotonic_ns: u64,
    ) -> Result<Envelope, ServiceDirectoryError> {
        if request.service != SERVICE_DIRECTORY_SERVICE {
            return Err(ServiceDirectoryError::UnknownMethod);
        }
        if request.method != NEGOTIATE_METHOD {
            return Err(ServiceDirectoryError::UnknownMethod);
        }
        let context =
            validate_sabi_request_context(request, MethodSemantics::QUERY, now_monotonic_ns)
                .map_err(|_| ServiceDirectoryError::InvalidRequest)?;
        let negotiation = decode_negotiate_service_request(&request.payload)
            .map_err(|_| ServiceDirectoryError::InvalidRequest)?;
        let result = self.directory.negotiate(&negotiation);
        let payload = encode_negotiate_service_response(&result)
            .map_err(|_| ServiceDirectoryError::Encode)?;
        let mut response = request.clone();
        response.payload = payload;
        response.common_context = Some(envelope::CommonContext::ResponseContext(
            SabiResponseContext {
                correlation_id: context.correlation_id.clone(),
                operation: None,
                receipts: Vec::new(),
                failure: None,
            },
        ));
        Ok(response)
    }
}
