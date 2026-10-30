use nlos_schema::sabi::v1::{
    LocalEndpoint, LocalTransportKind, NegotiateServiceRequest, ResolveServiceRequest,
    RetryDirective, SabiErrorCode, SchemaIdentity, ServiceCandidate, ServiceVersion,
    negotiate_service_response, resolve_service_response,
};
use nlos_schema::{
    CompatibilityError, SABI_OPERATION_CONTROL_SCHEMA, SABI_SERVICE_DIRECTORY_SCHEMA,
    VersionCompatibilityPolicy, VersionRoute, canonical_deprecation_manifest,
    encode_negotiate_service_request_frame, encode_resolve_service_request_frame,
    parse_deprecation_manifest, service_directory_schema_identity,
};
use nlos_service_directory::{
    DeprecationWarningReceipt, MAX_DEPRECATION_WARNINGS, PolicyNegotiationError,
    ServiceRegistration, SnapshotDirectory,
};

fn make_registration(
    id_byte: u8,
    service: &str,
    schema_name: &str,
    major: u32,
    minor: u32,
    feature_ids: Vec<u32>,
    address: &str,
) -> ServiceRegistration {
    ServiceRegistration {
        candidate: ServiceCandidate {
            binding_id: vec![id_byte; 16],
            generation: 1,
            service: service.to_owned(),
            version: Some(ServiceVersion {
                schema_name: schema_name.to_owned(),
                major,
                minor,
            }),
            feature_ids,
            transport_kinds: vec![LocalTransportKind::UnixSocket.into()],
        },
        endpoint: LocalEndpoint {
            kind: LocalTransportKind::UnixSocket.into(),
            address: address.to_owned(),
        },
    }
}

fn epoch4_test_manifest() -> nlos_schema::DeprecationManifest {
    let yaml = r#"
manifest_version: 1
current_epoch: 4
min_compatibility_window_epochs: 2
frozen_channels_total: 2

channels:
  - name: "nlos.sabi.ServiceDirectory"
    major: 4
    minor: 0
    frozen: true
    reserved_field_numbers: []
    reserved_symbols: []
  - name: "nlos.sabi.OperationControl"
    major: 4
    minor: 2
    frozen: true
    reserved_field_numbers: [9]
    reserved_symbols: ["operation.tombstoned_v0"]

entries:
  - channel: "nlos.sabi.ServiceDirectory"
    symbol: "ResolveServiceRequest"
    kind: "MESSAGE"
    introduced_in_epoch: 1
    state: "ACTIVE"
  - channel: "nlos.sabi.ServiceDirectory"
    symbol: "NegotiateServiceRequest"
    kind: "MESSAGE"
    introduced_in_epoch: 1
    state: "ACTIVE"
  - channel: "nlos.sabi.OperationControl"
    symbol: "operation.active"
    kind: "METHOD"
    introduced_in_epoch: 1
    state: "ACTIVE"
  - channel: "nlos.sabi.OperationControl"
    symbol: "operation.legacy_query"
    kind: "METHOD"
    introduced_in_epoch: 1
    state: "DEPRECATED"
    deprecated_in_epoch: 3
    earliest_sunset_epoch: 5
    replacement: "operation.active"
    migration_rule_id: "MIG-OP-201"
    reason: "Legacy query method enters two-epoch grace window in epoch 3"
  - channel: "nlos.sabi.OperationControl"
    symbol: "OperationControl.legacy_batch_flag"
    kind: "FIELD"
    parent_type: "QueryOperationRequest"
    field_number: 8
    introduced_in_epoch: 1
    state: "DEPRECATED"
    deprecated_in_epoch: 2
    earliest_sunset_epoch: 4
    replacement: "OperationControl.streaming_batch"
    migration_rule_id: "MIG-OP-202"
    reason: "Legacy batch feature flag is deprecated pending epoch 4/5 migration"
  - channel: "nlos.sabi.OperationControl"
    symbol: "operation.retired_v1"
    kind: "METHOD"
    introduced_in_epoch: 1
    state: "SUNSET"
    deprecated_in_epoch: 1
    earliest_sunset_epoch: 3
    sunset_in_epoch: 4
    replacement: "operation.active"
    migration_rule_id: "MIG-OP-101"
    reason: "Retired in epoch 4 after completing two-epoch grace window"
  - channel: "nlos.sabi.OperationControl"
    symbol: "OperationControl.sunset_feature_slot"
    kind: "FIELD"
    parent_type: "QueryOperationRequest"
    field_number: 5
    introduced_in_epoch: 1
    state: "SUNSET"
    deprecated_in_epoch: 2
    earliest_sunset_epoch: 4
    sunset_in_epoch: 4
    replacement: "OperationControl.active_feature_slot"
    migration_rule_id: "MIG-OP-102"
    reason: "Feature slot 5 completed grace window and is permanently retired"
"#;
    parse_deprecation_manifest(yaml).unwrap()
}

#[test]
fn active_contract_negotiation_and_resolution_emit_zero_deprecation_warnings() {
    let manifest = canonical_deprecation_manifest().unwrap();
    let policy = VersionCompatibilityPolicy::default();
    let mut directory = SnapshotDirectory::new([make_registration(
        0x11,
        "operation.control",
        SABI_OPERATION_CONTROL_SCHEMA,
        1,
        0,
        vec![1, 2],
        "unix://ns/operation-v1",
    )])
    .unwrap();

    let resolve_req = ResolveServiceRequest {
        schema: Some(service_directory_schema_identity()),
        service: "operation.control".to_owned(),
    };
    let resolve_outcome = directory
        .resolve_wire_with_policy(
            &encode_resolve_service_request_frame(&resolve_req).unwrap(),
            &policy,
            &manifest,
        )
        .unwrap();
    assert_eq!(resolve_outcome.emitted_warnings, Vec::new());
    assert!(matches!(
        resolve_outcome.response.result,
        Some(resolve_service_response::Result::Candidates(set)) if set.candidates.len() == 1
    ));

    let negotiate_req = NegotiateServiceRequest {
        schema: Some(service_directory_schema_identity()),
        service: "operation.control".to_owned(),
        schema_name: SABI_OPERATION_CONTROL_SCHEMA.to_owned(),
        major: 1,
        minimum_minor: 0,
        required_feature_ids: vec![1, 2],
        supported_transport_kinds: vec![LocalTransportKind::UnixSocket.into()],
    };
    let negotiate_outcome = directory
        .negotiate_resolve_wire(
            &encode_negotiate_service_request_frame(&negotiate_req).unwrap(),
            &policy,
            &manifest,
        )
        .unwrap();
    assert_eq!(negotiate_outcome.emitted_warnings, Vec::new());
    assert_eq!(directory.deprecation_warnings(), &[]);
    assert_eq!(
        negotiate_outcome.caller_negotiation.route,
        VersionRoute::NativeCurrentEpoch
    );
    assert!(matches!(
        negotiate_outcome.response.result,
        Some(negotiate_service_response::Result::Binding(binding))
            if binding.endpoint.as_ref().unwrap().address == "unix://ns/operation-v1"
    ));
}

#[test]
fn deprecated_service_and_feature_return_endpoint_without_disruption_and_emit_receipts() {
    let manifest = epoch4_test_manifest();
    let policy = VersionCompatibilityPolicy::new(4, &[]);
    let mut directory = SnapshotDirectory::new([make_registration(
        0x21,
        "operation.legacy_query",
        SABI_OPERATION_CONTROL_SCHEMA,
        4,
        2,
        vec![1, 8],
        "unix://ns/op-legacy",
    )])
    .unwrap();

    let negotiate_req = NegotiateServiceRequest {
        schema: Some(SchemaIdentity {
            name: SABI_SERVICE_DIRECTORY_SCHEMA.to_owned(),
            major: 4,
            minor: 1,
            critical_extension_ids: Vec::new(),
            non_critical_extension_ids: Vec::new(),
        }),
        service: "operation.legacy_query".to_owned(),
        schema_name: SABI_OPERATION_CONTROL_SCHEMA.to_owned(),
        major: 4,
        minimum_minor: 2,
        required_feature_ids: vec![1, 8],
        supported_transport_kinds: vec![LocalTransportKind::UnixSocket.into()],
    };

    let outcome = directory
        .negotiate_resolve_wire(
            &encode_negotiate_service_request_frame(&negotiate_req).unwrap(),
            &policy,
            &manifest,
        )
        .unwrap();

    // Service endpoint is returned normally without disrupting traffic.
    assert!(matches!(
        outcome.response.result,
        Some(negotiate_service_response::Result::Binding(ref binding))
            if binding.endpoint.as_ref().unwrap().address == "unix://ns/op-legacy"
    ));

    // Structured deprecation receipts are emitted for both the deprecated method
    // symbol and the deprecated feature/field number 8.
    let expected = vec![
        DeprecationWarningReceipt {
            channel: SABI_OPERATION_CONTROL_SCHEMA.to_owned(),
            symbol: "operation.legacy_query".to_owned(),
            field_number: None,
            deprecated_in_epoch: 3,
            earliest_sunset_epoch: 5,
            replacement: "operation.active".to_owned(),
            caller_major: 4,
            caller_minor: 2,
            service: "operation.legacy_query".to_owned(),
        },
        DeprecationWarningReceipt {
            channel: SABI_OPERATION_CONTROL_SCHEMA.to_owned(),
            symbol: "OperationControl.legacy_batch_flag".to_owned(),
            field_number: Some(8),
            deprecated_in_epoch: 2,
            earliest_sunset_epoch: 4,
            replacement: "OperationControl.streaming_batch".to_owned(),
            caller_major: 4,
            caller_minor: 2,
            service: "operation.legacy_query".to_owned(),
        },
    ];
    assert_eq!(outcome.emitted_warnings, expected);
    assert_eq!(directory.deprecation_warnings(), expected.as_slice());

    // Resolve also returns candidates while recording the deprecated service receipt.
    let resolve_req = ResolveServiceRequest {
        schema: Some(SchemaIdentity {
            name: SABI_SERVICE_DIRECTORY_SCHEMA.to_owned(),
            major: 3,
            minor: 7,
            critical_extension_ids: Vec::new(),
            non_critical_extension_ids: Vec::new(),
        }),
        service: "operation.legacy_query".to_owned(),
    };
    let resolve_outcome = directory
        .resolve_wire_with_policy(
            &encode_resolve_service_request_frame(&resolve_req).unwrap(),
            &policy,
            &manifest,
        )
        .unwrap();
    assert_eq!(resolve_outcome.emitted_warnings.len(), 1);
    assert_eq!(directory.deprecation_warnings().len(), 3);

    let drained = directory.drain_deprecation_warnings();
    assert_eq!(drained.len(), 3);
    assert_eq!(directory.deprecation_warnings(), &[]);
}

#[test]
fn sunset_and_reserved_contracts_fail_closed_with_abi_version_and_do_not_retry() {
    let manifest = epoch4_test_manifest();
    let policy = VersionCompatibilityPolicy::new(4, &[]);
    let mut directory = SnapshotDirectory::new([make_registration(
        0x31,
        "operation.active",
        SABI_OPERATION_CONTROL_SCHEMA,
        4,
        2,
        vec![1, 5, 9],
        "unix://ns/op-active",
    )])
    .unwrap();

    let caller_schema = SchemaIdentity {
        name: SABI_SERVICE_DIRECTORY_SCHEMA.to_owned(),
        major: 4,
        minor: 0,
        critical_extension_ids: Vec::new(),
        non_critical_extension_ids: Vec::new(),
    };

    // 1. Negotiating a SUNSET service symbol fails closed with E_ABI_VERSION + DO_NOT_RETRY.
    let sunset_service_req = NegotiateServiceRequest {
        schema: Some(caller_schema.clone()),
        service: "operation.retired_v1".to_owned(),
        schema_name: SABI_OPERATION_CONTROL_SCHEMA.to_owned(),
        major: 4,
        minimum_minor: 0,
        required_feature_ids: vec![1],
        supported_transport_kinds: vec![LocalTransportKind::UnixSocket.into()],
    };
    let err_sunset_sym = directory
        .negotiate_resolve_wire(
            &encode_negotiate_service_request_frame(&sunset_service_req).unwrap(),
            &policy,
            &manifest,
        )
        .unwrap_err();
    assert!(matches!(
        err_sunset_sym,
        PolicyNegotiationError::SunsetContract { ref symbol, .. } if symbol == "operation.retired_v1"
    ));
    let failure = err_sunset_sym.to_sabi_failure();
    assert_eq!(failure.code, i32::from(SabiErrorCode::AbiVersion));
    assert_eq!(failure.retry, i32::from(RetryDirective::DoNotRetry));

    // 2. Resolving a SUNSET or RESERVED service symbol also fails closed.
    for retired_name in ["operation.retired_v1", "operation.tombstoned_v0"] {
        let resolve_retired = ResolveServiceRequest {
            schema: Some(caller_schema.clone()),
            service: retired_name.to_owned(),
        };
        let err = directory
            .resolve_wire_with_policy(
                &encode_resolve_service_request_frame(&resolve_retired).unwrap(),
                &policy,
                &manifest,
            )
            .unwrap_err();
        assert_eq!(
            err.to_sabi_failure().code,
            i32::from(SabiErrorCode::AbiVersion)
        );
        assert_eq!(
            err.to_sabi_failure().retry,
            i32::from(RetryDirective::DoNotRetry)
        );
    }

    // 3. Negotiating a SUNSET feature/field slot (5) or permanently RESERVED slot (9) fails closed.
    for retired_slot in [5_u32, 9_u32] {
        let sunset_slot_req = NegotiateServiceRequest {
            schema: Some(caller_schema.clone()),
            service: "operation.active".to_owned(),
            schema_name: SABI_OPERATION_CONTROL_SCHEMA.to_owned(),
            major: 4,
            minimum_minor: 0,
            required_feature_ids: vec![1, retired_slot],
            supported_transport_kinds: vec![LocalTransportKind::UnixSocket.into()],
        };
        let err = directory
            .negotiate_resolve_wire(
                &encode_negotiate_service_request_frame(&sunset_slot_req).unwrap(),
                &policy,
                &manifest,
            )
            .unwrap_err();
        assert!(matches!(
            err,
            PolicyNegotiationError::SunsetContract {
                field_number: Some(slot),
                ..
            } if slot == retired_slot
        ));
        let failure = err.to_sabi_failure();
        assert_eq!(failure.code, i32::from(SabiErrorCode::AbiVersion));
        assert_eq!(failure.retry, i32::from(RetryDirective::DoNotRetry));
    }

    // Zero deprecation warnings emitted on fail-closed sunset rejections.
    assert_eq!(directory.deprecation_warnings(), &[]);
}

#[test]
#[allow(clippy::too_many_lines)]
fn n_n_minus_1_n_minus_2_wire_negotiation_and_compat_ver_002_enforcement() {
    let manifest = epoch4_test_manifest();
    let policy = VersionCompatibilityPolicy::new(4, &[11]);
    let mut directory = SnapshotDirectory::new([make_registration(
        0x41,
        "operation.active",
        SABI_OPERATION_CONTROL_SCHEMA,
        4,
        2,
        vec![1],
        "unix://ns/op-epoch4",
    )])
    .unwrap();

    // N (4), N-1 (3), and N-2 (2) succeed, preserve unknown non-critical extensions
    // and raw wire bytes, and tag N-1 / N-2 for OutOfCoreCompatShim mediation.
    for (epoch, expected_route) in [
        (4_u32, VersionRoute::NativeCurrentEpoch),
        (
            3_u32,
            VersionRoute::OutOfCoreCompatShim {
                client_epoch: 3,
                host_epoch: 4,
                epoch_distance: 1,
            },
        ),
        (
            2_u32,
            VersionRoute::OutOfCoreCompatShim {
                client_epoch: 2,
                host_epoch: 4,
                epoch_distance: 2,
            },
        ),
    ] {
        let req = NegotiateServiceRequest {
            schema: Some(SchemaIdentity {
                name: SABI_SERVICE_DIRECTORY_SCHEMA.to_owned(),
                major: epoch,
                minor: 3,
                critical_extension_ids: vec![11],
                non_critical_extension_ids: vec![4001, 4002],
            }),
            service: "operation.active".to_owned(),
            schema_name: SABI_OPERATION_CONTROL_SCHEMA.to_owned(),
            major: epoch,
            minimum_minor: 0,
            required_feature_ids: vec![1],
            supported_transport_kinds: vec![LocalTransportKind::UnixSocket.into()],
        };
        let mut wire = encode_negotiate_service_request_frame(&req).unwrap();
        // Append unknown protobuf field 99 (length-delimited "ext").
        wire.extend_from_slice(&[0x9A, 0x06, 0x03, b'e', b'x', b't']);

        let outcome = directory
            .negotiate_resolve_wire(&wire, &policy, &manifest)
            .unwrap();
        assert_eq!(outcome.caller_negotiation.route, expected_route);
        assert_eq!(
            outcome.target_negotiation.as_ref().unwrap().route,
            expected_route
        );
        assert_eq!(
            outcome.caller_negotiation.retained_non_critical_extensions,
            vec![4001, 4002]
        );
        assert_eq!(outcome.preserved_wire, wire);
        assert!(matches!(
            outcome.response.result,
            Some(negotiate_service_response::Result::Binding(ref b))
                if b.endpoint.as_ref().unwrap().address == "unix://ns/op-epoch4"
        ));
    }

    // Sunset epoch N-3 (1), future epoch N+1 (5), zero epoch (0), and unknown
    // critical extension (99) all fail closed with E_ABI_VERSION + DO_NOT_RETRY.
    for bad_caller in [
        SchemaIdentity {
            name: SABI_SERVICE_DIRECTORY_SCHEMA.to_owned(),
            major: 1,
            minor: 0,
            critical_extension_ids: Vec::new(),
            non_critical_extension_ids: Vec::new(),
        },
        SchemaIdentity {
            name: SABI_SERVICE_DIRECTORY_SCHEMA.to_owned(),
            major: 5,
            minor: 0,
            critical_extension_ids: Vec::new(),
            non_critical_extension_ids: Vec::new(),
        },
        SchemaIdentity {
            name: SABI_SERVICE_DIRECTORY_SCHEMA.to_owned(),
            major: 0,
            minor: 0,
            critical_extension_ids: Vec::new(),
            non_critical_extension_ids: Vec::new(),
        },
        SchemaIdentity {
            name: SABI_SERVICE_DIRECTORY_SCHEMA.to_owned(),
            major: 4,
            minor: 0,
            critical_extension_ids: vec![99],
            non_critical_extension_ids: Vec::new(),
        },
    ] {
        let req = NegotiateServiceRequest {
            schema: Some(bad_caller),
            service: "operation.active".to_owned(),
            schema_name: SABI_OPERATION_CONTROL_SCHEMA.to_owned(),
            major: 4,
            minimum_minor: 0,
            required_feature_ids: vec![1],
            supported_transport_kinds: vec![LocalTransportKind::UnixSocket.into()],
        };
        let err = directory
            .negotiate_resolve_wire(
                &encode_negotiate_service_request_frame(&req).unwrap(),
                &policy,
                &manifest,
            )
            .unwrap_err();
        assert!(matches!(
            err,
            PolicyNegotiationError::Compatibility(
                CompatibilityError::SunsetEpoch { .. }
                    | CompatibilityError::UnsupportedMajor { .. }
                    | CompatibilityError::ZeroMajorVersion { .. }
                    | CompatibilityError::UnsupportedCriticalExtension { .. }
            )
        ));
        let failure = err.to_sabi_failure();
        assert_eq!(failure.code, i32::from(SabiErrorCode::AbiVersion));
        assert_eq!(failure.retry, i32::from(RetryDirective::DoNotRetry));
    }
}

#[test]
fn deprecation_warning_telemetry_ring_is_bounded_at_max_deprecation_warnings() {
    let manifest = epoch4_test_manifest();
    let policy = VersionCompatibilityPolicy::new(4, &[]);
    let mut directory = SnapshotDirectory::new([make_registration(
        0x51,
        "operation.legacy_query",
        SABI_OPERATION_CONTROL_SCHEMA,
        4,
        500,
        vec![1],
        "unix://ns/op-legacy",
    )])
    .unwrap();

    let total_calls = u32::try_from(MAX_DEPRECATION_WARNINGS + 20).unwrap();
    for caller_minor in 0..total_calls {
        let req = NegotiateServiceRequest {
            schema: Some(SchemaIdentity {
                name: SABI_SERVICE_DIRECTORY_SCHEMA.to_owned(),
                major: 4,
                minor: caller_minor,
                critical_extension_ids: Vec::new(),
                non_critical_extension_ids: Vec::new(),
            }),
            service: "operation.legacy_query".to_owned(),
            schema_name: SABI_OPERATION_CONTROL_SCHEMA.to_owned(),
            major: 4,
            minimum_minor: caller_minor,
            required_feature_ids: vec![1],
            supported_transport_kinds: vec![LocalTransportKind::UnixSocket.into()],
        };
        directory
            .negotiate_with_policy(&req, &policy, &manifest)
            .unwrap();
    }

    let warnings = directory.deprecation_warnings();
    assert_eq!(warnings.len(), MAX_DEPRECATION_WARNINGS);
    assert_eq!(warnings.first().unwrap().caller_minor, 20);
    assert_eq!(warnings.last().unwrap().caller_minor, total_calls - 1);
}
