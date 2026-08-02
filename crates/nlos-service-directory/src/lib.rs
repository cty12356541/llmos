//! Deterministic in-memory `ServiceDirectory` snapshot for the Stage B SABI `PoC`.
//!
//! The bootstrap endpoint or handle is supplied by a trusted Namespace. This
//! crate resolves names and negotiates a compatible binding; it never invents
//! a fixed socket path or treats an implementation language as service identity.

use std::cmp::Ordering;
use std::collections::HashSet;
use std::error::Error;
use std::fmt;

use nlos_schema::sabi::v1::{
    DirectoryError, DirectoryErrorCode, LocalEndpoint, LocalTransportKind, NegotiateServiceRequest,
    NegotiateServiceResponse, ResolveServiceRequest, ResolveServiceResponse, ServiceBinding,
    ServiceCandidate, ServiceCandidateSet, negotiate_service_response, resolve_service_response,
};
use nlos_schema::{
    encode_negotiate_service_request, encode_resolve_service_request,
    encode_resolve_service_response, service_directory_schema_identity,
};

pub const BINDING_ID_BYTES: usize = 16;
pub const MAX_NAME_BYTES: usize = 255;
pub const MAX_ENDPOINT_BYTES: usize = 4096;
pub const MAX_FEATURE_IDS: usize = 128;
pub const MAX_TRANSPORT_KINDS: usize = 8;
pub const MAX_REGISTRATIONS: usize = 256;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceRegistration {
    pub candidate: ServiceCandidate,
    pub endpoint: LocalEndpoint,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RegistrationError {
    InvalidBindingIdLength { actual: usize },
    DuplicateBindingId,
    TooManyRegistrations,
    ResolveResponseTooLarge,
    ZeroGeneration,
    InvalidServiceName,
    MissingVersion,
    InvalidSchemaName,
    InvalidFeatureIds,
    InvalidTransportKinds,
    InvalidEndpoint,
    EndpointTransportMismatch,
}

impl fmt::Display for RegistrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidBindingIdLength { actual } => write!(
                formatter,
                "binding_id has {actual} bytes; exactly {BINDING_ID_BYTES} are required"
            ),
            Self::DuplicateBindingId => formatter.write_str("binding_id is duplicated"),
            Self::TooManyRegistrations => formatter.write_str("directory snapshot is too large"),
            Self::ResolveResponseTooLarge => {
                formatter.write_str("a service resolve response exceeds its schema bound")
            }
            Self::ZeroGeneration => formatter.write_str("binding generation must be non-zero"),
            Self::InvalidServiceName => formatter.write_str("service name is invalid"),
            Self::MissingVersion => formatter.write_str("service version is missing"),
            Self::InvalidSchemaName => formatter.write_str("service schema name is invalid"),
            Self::InvalidFeatureIds => {
                formatter.write_str("feature IDs must be non-zero, bounded, sorted, and unique")
            }
            Self::InvalidTransportKinds => formatter
                .write_str("transport kinds must be known, bounded, sorted, unique, and non-empty"),
            Self::InvalidEndpoint => formatter.write_str("local endpoint is invalid"),
            Self::EndpointTransportMismatch => formatter
                .write_str("candidate transport kinds must contain only the endpoint transport"),
        }
    }
}

impl Error for RegistrationError {}

#[derive(Clone, Debug, Default)]
pub struct SnapshotDirectory {
    registrations: Vec<ServiceRegistration>,
}

impl SnapshotDirectory {
    /// Creates an immutable directory snapshot after validating every binding.
    ///
    /// # Errors
    ///
    /// Rejects malformed registrations and duplicate 128-bit binding IDs.
    pub fn new(
        registrations: impl IntoIterator<Item = ServiceRegistration>,
    ) -> Result<Self, RegistrationError> {
        let mut validated = Vec::new();
        let mut binding_ids = HashSet::new();
        for registration in registrations {
            if validated.len() == MAX_REGISTRATIONS {
                return Err(RegistrationError::TooManyRegistrations);
            }
            validate_registration(&registration)?;
            if !binding_ids.insert(registration.candidate.binding_id.clone()) {
                return Err(RegistrationError::DuplicateBindingId);
            }
            validated.push(registration);
        }
        validated.sort_by(|left, right| compare_candidates(&left.candidate, &right.candidate));
        validate_resolve_response_bounds(&validated)?;
        Ok(Self {
            registrations: validated,
        })
    }

    /// Returns transport-neutral candidates in deterministic order.
    #[must_use]
    pub fn resolve(&self, request: &ResolveServiceRequest) -> ResolveServiceResponse {
        if encode_resolve_service_request(request).is_err() || !valid_name(&request.service) {
            return resolve_error(DirectoryErrorCode::InvalidRequest, request);
        }

        let candidates: Vec<_> = self
            .registrations
            .iter()
            .filter(|registration| registration.candidate.service == request.service)
            .map(|registration| registration.candidate.clone())
            .collect();
        if candidates.is_empty() {
            return resolve_error(DirectoryErrorCode::NotFound, request);
        }
        ResolveServiceResponse {
            schema: Some(service_directory_schema_identity()),
            result: Some(resolve_service_response::Result::Candidates(
                ServiceCandidateSet { candidates },
            )),
        }
    }

    /// Selects one compatible endpoint after deterministic version/feature negotiation.
    #[must_use]
    pub fn negotiate(&self, request: &NegotiateServiceRequest) -> NegotiateServiceResponse {
        if !valid_negotiate_request(request) {
            return negotiate_error(DirectoryErrorCode::InvalidRequest, request);
        }

        let service: Vec<_> = self
            .registrations
            .iter()
            .filter(|registration| registration.candidate.service == request.service)
            .collect();
        if service.is_empty() {
            return negotiate_error(DirectoryErrorCode::NotFound, request);
        }

        let schema: Vec<_> = service
            .into_iter()
            .filter(|registration| {
                registration
                    .candidate
                    .version
                    .as_ref()
                    .is_some_and(|version| version.schema_name == request.schema_name)
            })
            .collect();
        if schema.is_empty() {
            return negotiate_error(DirectoryErrorCode::SchemaUnsupported, request);
        }

        let version: Vec<_> = schema
            .into_iter()
            .filter(|registration| {
                registration
                    .candidate
                    .version
                    .as_ref()
                    .is_some_and(|version| {
                        version.major == request.major && version.minor >= request.minimum_minor
                    })
            })
            .collect();
        if version.is_empty() {
            return negotiate_error(DirectoryErrorCode::VersionUnsupported, request);
        }

        let features: Vec<_> = version
            .into_iter()
            .filter(|registration| {
                request.required_feature_ids.iter().all(|required| {
                    registration
                        .candidate
                        .feature_ids
                        .binary_search(required)
                        .is_ok()
                })
            })
            .collect();
        if features.is_empty() {
            return negotiate_error(DirectoryErrorCode::RequiredFeatureUnsupported, request);
        }

        let mut transports: Vec<_> = features
            .into_iter()
            .filter(|registration| {
                request
                    .supported_transport_kinds
                    .binary_search(&registration.endpoint.kind)
                    .is_ok()
            })
            .collect();
        if transports.is_empty() {
            return negotiate_error(DirectoryErrorCode::TransportUnsupported, request);
        }
        transports.sort_by(|left, right| {
            compare_negotiated_candidates(&left.candidate, &right.candidate)
        });
        let selected = transports[0];
        NegotiateServiceResponse {
            schema: Some(service_directory_schema_identity()),
            result: Some(negotiate_service_response::Result::Binding(
                ServiceBinding {
                    candidate: Some(selected.candidate.clone()),
                    endpoint: Some(selected.endpoint.clone()),
                },
            )),
        }
    }
}

fn validate_resolve_response_bounds(
    registrations: &[ServiceRegistration],
) -> Result<(), RegistrationError> {
    let mut offset = 0;
    while offset < registrations.len() {
        let service = &registrations[offset].candidate.service;
        let end = registrations[offset..]
            .iter()
            .position(|registration| registration.candidate.service != *service)
            .map_or(registrations.len(), |relative| offset + relative);
        let response = ResolveServiceResponse {
            schema: Some(service_directory_schema_identity()),
            result: Some(resolve_service_response::Result::Candidates(
                ServiceCandidateSet {
                    candidates: registrations[offset..end]
                        .iter()
                        .map(|registration| registration.candidate.clone())
                        .collect(),
                },
            )),
        };
        encode_resolve_service_response(&response)
            .map_err(|_| RegistrationError::ResolveResponseTooLarge)?;
        offset = end;
    }
    Ok(())
}

fn valid_negotiate_request(request: &NegotiateServiceRequest) -> bool {
    encode_negotiate_service_request(request).is_ok()
        && valid_name(&request.service)
        && valid_name(&request.schema_name)
        && request.major != 0
        && valid_feature_ids(&request.required_feature_ids)
        && valid_transport_kinds(&request.supported_transport_kinds)
}

fn validate_registration(registration: &ServiceRegistration) -> Result<(), RegistrationError> {
    let candidate = &registration.candidate;
    if candidate.binding_id.len() != BINDING_ID_BYTES {
        return Err(RegistrationError::InvalidBindingIdLength {
            actual: candidate.binding_id.len(),
        });
    }
    if candidate.generation == 0 {
        return Err(RegistrationError::ZeroGeneration);
    }
    if !valid_name(&candidate.service) {
        return Err(RegistrationError::InvalidServiceName);
    }
    let version = candidate
        .version
        .as_ref()
        .ok_or(RegistrationError::MissingVersion)?;
    if !valid_name(&version.schema_name) || version.major == 0 {
        return Err(RegistrationError::InvalidSchemaName);
    }
    if !valid_feature_ids(&candidate.feature_ids) {
        return Err(RegistrationError::InvalidFeatureIds);
    }
    if !valid_transport_kinds(&candidate.transport_kinds) {
        return Err(RegistrationError::InvalidTransportKinds);
    }
    if !valid_endpoint(&registration.endpoint) {
        return Err(RegistrationError::InvalidEndpoint);
    }
    if candidate.transport_kinds.as_slice() != [registration.endpoint.kind] {
        return Err(RegistrationError::EndpointTransportMismatch);
    }
    Ok(())
}

fn valid_name(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_NAME_BYTES && !value.contains('\0')
}

fn valid_feature_ids(values: &[u32]) -> bool {
    values.len() <= MAX_FEATURE_IDS
        && values.iter().all(|value| *value != 0)
        && values.windows(2).all(|pair| pair[0] < pair[1])
}

fn valid_transport_kinds(values: &[i32]) -> bool {
    !values.is_empty()
        && values.len() <= MAX_TRANSPORT_KINDS
        && values.windows(2).all(|pair| pair[0] < pair[1])
        && values.iter().all(|value| {
            LocalTransportKind::try_from(*value)
                .is_ok_and(|kind| kind != LocalTransportKind::Unspecified)
        })
}

fn valid_endpoint(endpoint: &LocalEndpoint) -> bool {
    LocalTransportKind::try_from(endpoint.kind)
        .is_ok_and(|kind| kind != LocalTransportKind::Unspecified)
        && !endpoint.address.is_empty()
        && endpoint.address.len() <= MAX_ENDPOINT_BYTES
        && !endpoint.address.contains('\0')
}

fn compare_candidates(left: &ServiceCandidate, right: &ServiceCandidate) -> Ordering {
    let left_version = left
        .version
        .as_ref()
        .expect("validated candidate has a version");
    let right_version = right
        .version
        .as_ref()
        .expect("validated candidate has a version");
    left.service
        .cmp(&right.service)
        .then_with(|| left_version.schema_name.cmp(&right_version.schema_name))
        .then_with(|| right_version.major.cmp(&left_version.major))
        .then_with(|| right_version.minor.cmp(&left_version.minor))
        .then_with(|| right.generation.cmp(&left.generation))
        .then_with(|| left.binding_id.cmp(&right.binding_id))
}

fn compare_negotiated_candidates(left: &ServiceCandidate, right: &ServiceCandidate) -> Ordering {
    let left_version = left
        .version
        .as_ref()
        .expect("validated candidate has a version");
    let right_version = right
        .version
        .as_ref()
        .expect("validated candidate has a version");
    right_version
        .minor
        .cmp(&left_version.minor)
        .then_with(|| right.generation.cmp(&left.generation))
        .then_with(|| left.binding_id.cmp(&right.binding_id))
}

fn safe_error_service(service: &str) -> String {
    if valid_name(service) {
        service.to_owned()
    } else {
        String::new()
    }
}

fn resolve_error(
    code: DirectoryErrorCode,
    request: &ResolveServiceRequest,
) -> ResolveServiceResponse {
    ResolveServiceResponse {
        schema: Some(service_directory_schema_identity()),
        result: Some(resolve_service_response::Result::Error(DirectoryError {
            code: code.into(),
            service: safe_error_service(&request.service),
        })),
    }
}

fn negotiate_error(
    code: DirectoryErrorCode,
    request: &NegotiateServiceRequest,
) -> NegotiateServiceResponse {
    NegotiateServiceResponse {
        schema: Some(service_directory_schema_identity()),
        result: Some(negotiate_service_response::Result::Error(DirectoryError {
            code: code.into(),
            service: safe_error_service(&request.service),
        })),
    }
}
