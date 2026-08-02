use nlos_schema::sabi::v1::{
    DirectoryErrorCode, LocalEndpoint, LocalTransportKind, NegotiateServiceRequest,
    ResolveServiceRequest, ServiceCandidate, ServiceVersion, negotiate_service_response,
    resolve_service_response,
};
use nlos_schema::service_directory_schema_identity;
use nlos_service_directory::{
    MAX_REGISTRATIONS, RegistrationError, ServiceRegistration, SnapshotDirectory,
};

fn registration(
    binding_seed: u8,
    minor: u32,
    generation: u64,
    feature_ids: &[u32],
    transport: LocalTransportKind,
) -> ServiceRegistration {
    let address = match transport {
        LocalTransportKind::UnixSocket => format!("/tmp/nlos-operation-{binding_seed}.sock"),
        LocalTransportKind::WindowsNamedPipe => {
            format!(r"\\.\pipe\nlos-operation-{binding_seed}")
        }
        LocalTransportKind::Unspecified => String::new(),
    };
    ServiceRegistration {
        candidate: ServiceCandidate {
            binding_id: vec![binding_seed; 16],
            generation,
            service: "operation".to_owned(),
            version: Some(ServiceVersion {
                schema_name: "nlos.sabi.Operation".to_owned(),
                major: 1,
                minor,
            }),
            feature_ids: feature_ids.to_vec(),
            transport_kinds: vec![transport.into()],
        },
        endpoint: LocalEndpoint {
            kind: transport.into(),
            address,
        },
    }
}

fn resolve(service: &str) -> ResolveServiceRequest {
    ResolveServiceRequest {
        schema: Some(service_directory_schema_identity()),
        service: service.to_owned(),
    }
}

fn negotiate() -> NegotiateServiceRequest {
    NegotiateServiceRequest {
        schema: Some(service_directory_schema_identity()),
        service: "operation".to_owned(),
        schema_name: "nlos.sabi.Operation".to_owned(),
        major: 1,
        minimum_minor: 1,
        required_feature_ids: vec![10],
        supported_transport_kinds: vec![
            LocalTransportKind::UnixSocket.into(),
            LocalTransportKind::WindowsNamedPipe.into(),
        ],
    }
}

fn negotiation_error_code(
    response: nlos_schema::sabi::v1::NegotiateServiceResponse,
) -> DirectoryErrorCode {
    let negotiate_service_response::Result::Error(error) = response.result.unwrap() else {
        panic!("expected negotiation error")
    };
    DirectoryErrorCode::try_from(error.code).unwrap()
}

#[test]
fn registrations_fail_closed_before_entering_the_snapshot() {
    let mut malformed = registration(1, 0, 1, &[2, 1], LocalTransportKind::UnixSocket);
    assert_eq!(
        SnapshotDirectory::new([malformed.clone()]).unwrap_err(),
        RegistrationError::InvalidFeatureIds
    );

    malformed.candidate.feature_ids = vec![1, 2];
    malformed.candidate.transport_kinds = vec![LocalTransportKind::WindowsNamedPipe.into()];
    assert_eq!(
        SnapshotDirectory::new([malformed]).unwrap_err(),
        RegistrationError::EndpointTransportMismatch
    );

    let duplicate = registration(2, 0, 1, &[], LocalTransportKind::UnixSocket);
    assert_eq!(
        SnapshotDirectory::new([duplicate.clone(), duplicate]).unwrap_err(),
        RegistrationError::DuplicateBindingId
    );

    let oversized = (0..=MAX_REGISTRATIONS).map(|index| {
        let mut value = registration(1, 0, 1, &[], LocalTransportKind::UnixSocket);
        value.candidate.binding_id = (index as u128).to_be_bytes().to_vec();
        value
    });
    assert_eq!(
        SnapshotDirectory::new(oversized).unwrap_err(),
        RegistrationError::TooManyRegistrations
    );
}

#[test]
fn resolve_returns_transport_neutral_candidates_in_stable_order() {
    let directory = SnapshotDirectory::new([
        registration(3, 1, 1, &[10], LocalTransportKind::WindowsNamedPipe),
        registration(1, 2, 1, &[10], LocalTransportKind::UnixSocket),
        registration(2, 2, 2, &[10], LocalTransportKind::UnixSocket),
    ])
    .unwrap();

    let response = directory.resolve(&resolve("operation"));
    let resolve_service_response::Result::Candidates(candidates) = response.result.unwrap() else {
        panic!("expected candidates")
    };
    let ids: Vec<_> = candidates
        .candidates
        .iter()
        .map(|candidate| candidate.binding_id[0])
        .collect();
    assert_eq!(ids, [2, 1, 3]);
}

#[test]
fn negotiation_selects_highest_minor_then_generation_deterministically() {
    let directory = SnapshotDirectory::new([
        registration(1, 2, 1, &[10], LocalTransportKind::UnixSocket),
        registration(2, 2, 2, &[10, 20], LocalTransportKind::UnixSocket),
        registration(3, 1, 9, &[10], LocalTransportKind::WindowsNamedPipe),
    ])
    .unwrap();

    let response = directory.negotiate(&negotiate());
    let negotiate_service_response::Result::Binding(binding) = response.result.unwrap() else {
        panic!("expected binding")
    };
    let candidate = binding.candidate.unwrap();
    assert_eq!(candidate.binding_id, vec![2; 16]);
    assert_eq!(candidate.version.unwrap().minor, 2);
    assert_eq!(
        binding.endpoint.unwrap().kind,
        i32::from(LocalTransportKind::UnixSocket)
    );
}

#[test]
fn negotiation_reports_the_first_failed_compatibility_dimension() {
    let directory =
        SnapshotDirectory::new([registration(1, 1, 1, &[10], LocalTransportKind::UnixSocket)])
            .unwrap();

    assert_eq!(
        negotiation_error_code(directory.negotiate(&NegotiateServiceRequest {
            service: "missing".to_owned(),
            ..negotiate()
        })),
        DirectoryErrorCode::NotFound
    );
    assert_eq!(
        negotiation_error_code(directory.negotiate(&NegotiateServiceRequest {
            schema_name: "nlos.sabi.Other".to_owned(),
            ..negotiate()
        })),
        DirectoryErrorCode::SchemaUnsupported
    );
    assert_eq!(
        negotiation_error_code(directory.negotiate(&NegotiateServiceRequest {
            major: 2,
            ..negotiate()
        })),
        DirectoryErrorCode::VersionUnsupported
    );
    assert_eq!(
        negotiation_error_code(directory.negotiate(&NegotiateServiceRequest {
            required_feature_ids: vec![99],
            ..negotiate()
        })),
        DirectoryErrorCode::RequiredFeatureUnsupported
    );
    assert_eq!(
        negotiation_error_code(directory.negotiate(&NegotiateServiceRequest {
            supported_transport_kinds: vec![LocalTransportKind::WindowsNamedPipe.into()],
            ..negotiate()
        })),
        DirectoryErrorCode::TransportUnsupported
    );
}

#[test]
fn malformed_requests_return_bounded_invalid_request_without_reflecting_input() {
    let directory = SnapshotDirectory::default();
    let response = directory.negotiate(&NegotiateServiceRequest {
        service: "x".repeat(256),
        supported_transport_kinds: vec![],
        ..negotiate()
    });
    let negotiate_service_response::Result::Error(error) = response.result.unwrap() else {
        panic!("expected invalid request")
    };
    assert_eq!(
        DirectoryErrorCode::try_from(error.code).unwrap(),
        DirectoryErrorCode::InvalidRequest
    );
    assert!(error.service.is_empty());
}
