use nlos_schema::sabi::v1::{Envelope, RetryDirective, SabiErrorCode, SchemaIdentity};
use nlos_schema::{
    CompatibilityError, ContractElementKind, DeprecationManifestError, DeprecationState,
    MAX_EXTENSION_IDS, MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS, SABI_CHANNEL_CONTROL_SCHEMA,
    SABI_DRIVER_PROVIDER_SCHEMA, SABI_ENVELOPE_SCHEMA, SABI_OPERATION_CONTROL_SCHEMA,
    SABI_PRINCIPAL_HANDSHAKE_SCHEMA, SABI_SERVICE_DIRECTORY_SCHEMA, SABI_SYSTEM_CONTROL_SCHEMA,
    SABI_TAKEOVER_CONTROL_SCHEMA, SABI_WAIT_CONTROL_SCHEMA, SchemaDescriptor, SymbolAuditVerdict,
    VersionCompatibilityPolicy, VersionRoute, canonical_deprecation_manifest,
    compatibility_error_to_sabi_failure, parse_deprecation_manifest, schema_registry,
};
use prost::Message as _;

fn sample_envelope(major: u32, minor: u32, critical: Vec<u32>, non_critical: Vec<u32>) -> Envelope {
    Envelope {
        schema: Some(SchemaIdentity {
            name: SABI_ENVELOPE_SCHEMA.to_owned(),
            major,
            minor,
            critical_extension_ids: critical,
            non_critical_extension_ids: non_critical,
        }),
        request_id: (1_u8..=16).collect(),
        service: "nlos.authority.Operation".to_owned(),
        method: "QueryOperation".to_owned(),
        common_context: None,
        payload: b"stage-e-compat-payload".to_vec(),
    }
}

#[test]
fn version_matrix_routes_current_epoch_natively_and_n_minus_1_n_minus_2_to_shim() {
    assert_eq!(MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS, 2);
    let policy = VersionCompatibilityPolicy::new(4, &[101, 202]);

    // Epoch N (major == 4), including higher additive minor versions -> NativeCurrentEpoch.
    for minor in [0_u32, 1, 7, 42] {
        let identity = SchemaIdentity {
            name: SABI_SYSTEM_CONTROL_SCHEMA.to_owned(),
            major: 4,
            minor,
            critical_extension_ids: vec![101],
            non_critical_extension_ids: vec![901, 902],
        };
        let outcome = policy.adjudicate_schema_identity(&identity).unwrap();
        assert_eq!(outcome.channel, SABI_SYSTEM_CONTROL_SCHEMA);
        assert_eq!(outcome.client_major, 4);
        assert_eq!(outcome.client_minor, minor);
        assert_eq!(outcome.host_epoch, 4);
        assert_eq!(outcome.route, VersionRoute::NativeCurrentEpoch);
        assert_eq!(outcome.accepted_critical_extensions, vec![101]);
        assert_eq!(outcome.retained_non_critical_extensions, vec![901, 902]);
    }

    // Epoch N-1 (major == 3) -> OutOfCoreCompatShim with epoch_distance == 1.
    let n_minus_1 = SchemaIdentity {
        name: SABI_OPERATION_CONTROL_SCHEMA.to_owned(),
        major: 3,
        minor: 5,
        critical_extension_ids: vec![202],
        non_critical_extension_ids: vec![5001],
    };
    let outcome_n1 = policy.adjudicate_schema_identity(&n_minus_1).unwrap();
    assert_eq!(
        outcome_n1.route,
        VersionRoute::OutOfCoreCompatShim {
            client_epoch: 3,
            host_epoch: 4,
            epoch_distance: 1,
        }
    );

    // Epoch N-2 (major == 2) -> OutOfCoreCompatShim with epoch_distance == 2.
    let n_minus_2 = SchemaIdentity {
        name: SABI_DRIVER_PROVIDER_SCHEMA.to_owned(),
        major: 2,
        minor: 0,
        critical_extension_ids: Vec::new(),
        non_critical_extension_ids: vec![7001, 7002],
    };
    let outcome_n2 = policy.adjudicate_schema_identity(&n_minus_2).unwrap();
    assert_eq!(
        outcome_n2.route,
        VersionRoute::OutOfCoreCompatShim {
            client_epoch: 2,
            host_epoch: 4,
            epoch_distance: 2,
        }
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn version_matrix_fails_closed_with_abi_version_on_zero_future_sunset_and_unknown_critical() {
    let policy = VersionCompatibilityPolicy::new(4, &[101]);

    // 1. major == 0 -> ZeroMajorVersion -> E_ABI_VERSION + DO_NOT_RETRY.
    let zero_major = SchemaIdentity {
        name: SABI_ENVELOPE_SCHEMA.to_owned(),
        major: 0,
        minor: 0,
        critical_extension_ids: Vec::new(),
        non_critical_extension_ids: Vec::new(),
    };
    let err_zero = policy.adjudicate_schema_identity(&zero_major).unwrap_err();
    assert_eq!(
        err_zero,
        CompatibilityError::ZeroMajorVersion {
            schema: SABI_ENVELOPE_SCHEMA.to_owned(),
        }
    );
    assert!(err_zero.is_abi_version_error());
    let failure_zero = err_zero.to_sabi_failure();
    assert_eq!(failure_zero.code, i32::from(SabiErrorCode::AbiVersion));
    assert_eq!(failure_zero.retry, i32::from(RetryDirective::DoNotRetry));
    assert_ne!(failure_zero.safe_message, "");

    // 2. major > N (future major == 5 at host_epoch == 4) -> UnsupportedMajor -> E_ABI_VERSION.
    let future_major = SchemaIdentity {
        name: SABI_SERVICE_DIRECTORY_SCHEMA.to_owned(),
        major: 5,
        minor: 0,
        critical_extension_ids: Vec::new(),
        non_critical_extension_ids: Vec::new(),
    };
    let err_future = policy
        .adjudicate_schema_identity(&future_major)
        .unwrap_err();
    assert_eq!(
        err_future,
        CompatibilityError::UnsupportedMajor {
            schema: SABI_SERVICE_DIRECTORY_SCHEMA.to_owned(),
            got: 5,
            supported: 4,
        }
    );
    let failure_future = compatibility_error_to_sabi_failure(&err_future);
    assert_eq!(failure_future.code, i32::from(SabiErrorCode::AbiVersion));
    assert_eq!(failure_future.retry, i32::from(RetryDirective::DoNotRetry));

    // 3. major <= N-3 (sunset major == 1 at host_epoch == 4) -> SunsetEpoch -> E_ABI_VERSION.
    let sunset_major = SchemaIdentity {
        name: SABI_WAIT_CONTROL_SCHEMA.to_owned(),
        major: 1,
        minor: 0,
        critical_extension_ids: Vec::new(),
        non_critical_extension_ids: Vec::new(),
    };
    let err_sunset = policy
        .adjudicate_schema_identity(&sunset_major)
        .unwrap_err();
    assert_eq!(
        err_sunset,
        CompatibilityError::SunsetEpoch {
            schema: SABI_WAIT_CONTROL_SCHEMA.to_owned(),
            client_epoch: 1,
            host_epoch: 4,
            max_backward_epochs: 2,
        }
    );
    let failure_sunset = err_sunset.to_sabi_failure();
    assert_eq!(failure_sunset.code, i32::from(SabiErrorCode::AbiVersion));
    assert_eq!(failure_sunset.retry, i32::from(RetryDirective::DoNotRetry));

    // 4. COMPAT-VER-002: unknown critical extension -> UnsupportedCriticalExtension -> E_ABI_VERSION.
    let unknown_critical = SchemaIdentity {
        name: SABI_CHANNEL_CONTROL_SCHEMA.to_owned(),
        major: 4,
        minor: 0,
        critical_extension_ids: vec![101, 999],
        non_critical_extension_ids: vec![42],
    };
    let err_crit = policy
        .adjudicate_schema_identity(&unknown_critical)
        .unwrap_err();
    assert_eq!(
        err_crit,
        CompatibilityError::UnsupportedCriticalExtension {
            schema: SABI_CHANNEL_CONTROL_SCHEMA.to_owned(),
            extension_id: 999,
        }
    );
    let failure_crit = err_crit.to_sabi_failure();
    assert_eq!(failure_crit.code, i32::from(SabiErrorCode::AbiVersion));
    assert_eq!(failure_crit.retry, i32::from(RetryDirective::DoNotRetry));

    // 5. Critical extension ID 0 is always rejected fail-closed.
    let zero_critical = SchemaIdentity {
        name: SABI_TAKEOVER_CONTROL_SCHEMA.to_owned(),
        major: 4,
        minor: 0,
        critical_extension_ids: vec![0],
        non_critical_extension_ids: Vec::new(),
    };
    assert_eq!(
        policy.adjudicate_schema_identity(&zero_critical),
        Err(CompatibilityError::UnsupportedCriticalExtension {
            schema: SABI_TAKEOVER_CONTROL_SCHEMA.to_owned(),
            extension_id: 0,
        })
    );

    // 6. Unknown schema name -> UnknownSchema -> E_ABI_VERSION.
    let unknown_schema = SchemaIdentity {
        name: "nlos.sabi.UnregisteredChannel".to_owned(),
        major: 4,
        minor: 0,
        critical_extension_ids: Vec::new(),
        non_critical_extension_ids: Vec::new(),
    };
    let err_unknown = policy
        .adjudicate_schema_identity(&unknown_schema)
        .unwrap_err();
    assert_eq!(
        err_unknown,
        CompatibilityError::UnknownSchema("nlos.sabi.UnregisteredChannel".to_owned())
    );
    assert_eq!(
        err_unknown.to_sabi_failure().code,
        i32::from(SabiErrorCode::AbiVersion)
    );

    // 7. Missing SchemaIdentity -> MissingSchemaIdentity -> E_ABI_VERSION.
    let err_missing = policy
        .adjudicate_optional_schema_identity(None)
        .unwrap_err();
    assert_eq!(err_missing, CompatibilityError::MissingSchemaIdentity);
    assert_eq!(
        err_missing.to_sabi_failure().code,
        i32::from(SabiErrorCode::AbiVersion)
    );

    // 8. Policy with backward window < 2 -> InvalidCompatibilityWindow -> E_ABI_VERSION.
    let narrow_policy = VersionCompatibilityPolicy::with_backward_window(4, 1, &[]);
    let valid_id = SchemaIdentity {
        name: SABI_PRINCIPAL_HANDSHAKE_SCHEMA.to_owned(),
        major: 4,
        minor: 0,
        critical_extension_ids: Vec::new(),
        non_critical_extension_ids: Vec::new(),
    };
    let err_window = narrow_policy
        .adjudicate_schema_identity(&valid_id)
        .unwrap_err();
    assert_eq!(
        err_window,
        CompatibilityError::InvalidCompatibilityWindow { got: 1, minimum: 2 }
    );
    assert_eq!(
        err_window.to_sabi_failure().code,
        i32::from(SabiErrorCode::AbiVersion)
    );

    // 9. Extension IDs exceeding MAX_EXTENSION_IDS fail closed -> E_ABI_VERSION.
    let over_ext = SchemaIdentity {
        name: SABI_ENVELOPE_SCHEMA.to_owned(),
        major: 4,
        minor: 1,
        critical_extension_ids: Vec::new(),
        non_critical_extension_ids: (1..=u32::try_from(MAX_EXTENSION_IDS + 1).unwrap()).collect(),
    };
    let err_over = policy.adjudicate_schema_identity(&over_ext).unwrap_err();
    assert_eq!(
        err_over,
        CompatibilityError::TooManyExtensionIds {
            schema: SABI_ENVELOPE_SCHEMA.to_owned(),
            actual: MAX_EXTENSION_IDS + 1,
            maximum: MAX_EXTENSION_IDS,
        }
    );
    assert_eq!(
        err_over.to_sabi_failure().code,
        i32::from(SabiErrorCode::AbiVersion)
    );
}

#[test]
fn compat_ver_002_preserves_unknown_non_critical_extensions_and_unknown_wire_fields() {
    let policy = VersionCompatibilityPolicy::new(3, &[17]);
    let unknown_non_critical = vec![8001_u32, 8002, 65_535];

    for (client_major, expected_route) in [
        (3_u32, VersionRoute::NativeCurrentEpoch),
        (
            2_u32,
            VersionRoute::OutOfCoreCompatShim {
                client_epoch: 2,
                host_epoch: 3,
                epoch_distance: 1,
            },
        ),
        (
            1_u32,
            VersionRoute::OutOfCoreCompatShim {
                client_epoch: 1,
                host_epoch: 3,
                epoch_distance: 2,
            },
        ),
    ] {
        let env = sample_envelope(client_major, 9, vec![17], unknown_non_critical.clone());
        let mut wire = env.encode_to_vec();
        // Append an unknown protobuf length-delimited field (field number 99, wire type 2:
        // tag = (99 << 3) | 2 = 794 -> varint [0x9A, 0x06]).
        wire.extend_from_slice(&[0x9A, 0x06, 0x0C]);
        wire.extend_from_slice(b"future-addon");

        let (validated_frame, outcome) = policy.adjudicate_envelope_wire(&wire).unwrap();
        assert_eq!(outcome.route, expected_route);
        assert_eq!(outcome.accepted_critical_extensions, vec![17]);
        assert_eq!(
            outcome.retained_non_critical_extensions,
            unknown_non_critical
        );
        // Forwarding path retains exact wire bytes including unknown protobuf tags.
        assert_eq!(validated_frame.wire_bytes(), wire.as_slice());

        // Re-encoding the typed message also preserves unknown non_critical_extension_ids.
        let (reencoded, re_outcome) = policy
            .encode_adjudicated_envelope(validated_frame.envelope())
            .unwrap();
        assert_eq!(re_outcome.route, expected_route);
        let round_tripped = Envelope::decode(reencoded.as_slice()).unwrap();
        assert_eq!(
            round_tripped
                .schema
                .as_ref()
                .unwrap()
                .non_critical_extension_ids,
            unknown_non_critical
        );
    }
}

#[test]
fn canonical_deprecation_manifest_matches_all_nine_frozen_registry_channels() {
    let manifest = canonical_deprecation_manifest().unwrap();
    assert_eq!(manifest.manifest_version, 1);
    assert_eq!(manifest.current_epoch, 1);
    assert_eq!(
        manifest.min_compatibility_window_epochs,
        MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS
    );
    assert_eq!(manifest.frozen_channels_total, 9);
    assert_eq!(manifest.channels.len(), 9);
    assert!(manifest.entries.len() >= 9);

    for descriptor in schema_registry() {
        let ch = manifest
            .channel(descriptor.name)
            .unwrap_or_else(|| panic!("missing channel {}", descriptor.name));
        assert!(ch.frozen);
        assert_eq!(ch.major, descriptor.major);
        assert_eq!(ch.minor, descriptor.minor);
    }

    for entry in &manifest.entries {
        assert_eq!(entry.state, DeprecationState::Active);
        assert!(matches!(
            manifest.audit_symbol(&entry.channel, &entry.symbol),
            SymbolAuditVerdict::Active(Some(_))
        ));
    }
}

#[test]
fn deprecation_manifest_accepts_valid_lifecycle_and_audits_active_deprecated_sunset() {
    let yaml = r#"
manifest_version: 1
current_epoch: 4
min_compatibility_window_epochs: 2
frozen_channels_total: 2

channels:
  - name: "nlos.sabi.ServiceDirectory"
    major: 4
    minor: 2
    frozen: true
    reserved_field_numbers: [4]
    reserved_symbols: ["NegotiateServiceRequest.legacy_transport_hint"]
  - name: "nlos.sabi.OperationControl"
    major: 4
    minor: 1
    frozen: true
    reserved_field_numbers: []
    reserved_symbols: []

entries:
  - channel: "nlos.sabi.ServiceDirectory"
    symbol: "ResolveServiceRequest"
    kind: "MESSAGE"
    introduced_in_epoch: 1
    state: "ACTIVE"
  - channel: "nlos.sabi.ServiceDirectory"
    symbol: "NegotiateServiceRequest.legacy_feature_mask"
    kind: "FIELD"
    parent_type: "NegotiateServiceRequest"
    field_number: 8
    introduced_in_epoch: 1
    state: "DEPRECATED"
    deprecated_in_epoch: 3
    earliest_sunset_epoch: 5
    replacement: "NegotiateServiceRequest.required_feature_ids"
    migration_rule_id: "MIG-SDIR-001"
    reason: "Bitmask superseded by bounded sorted u32 feature list"
  - channel: "nlos.sabi.OperationControl"
    symbol: "QueryOperationRequest.legacy_timeout_ms"
    kind: "FIELD"
    parent_type: "QueryOperationRequest"
    field_number: 3
    introduced_in_epoch: 1
    state: "SUNSET"
    deprecated_in_epoch: 1
    earliest_sunset_epoch: 3
    sunset_in_epoch: 4
    replacement: "SabiRequestContext.deadline_monotonic_ns"
    migration_rule_id: "MIG-OP-001"
    reason: "Wall-clock timeout retired after two-epoch grace window"
"#;

    let manifest = parse_deprecation_manifest(yaml).unwrap();
    assert_eq!(manifest.current_epoch, 4);
    assert_eq!(manifest.entries.len(), 3);
    assert_eq!(manifest.entries[1].kind, ContractElementKind::Field);

    assert!(matches!(
        manifest.audit_symbol("nlos.sabi.ServiceDirectory", "ResolveServiceRequest"),
        SymbolAuditVerdict::Active(Some(_))
    ));
    assert!(matches!(
        manifest.audit_field_number(
            "nlos.sabi.ServiceDirectory",
            Some("NegotiateServiceRequest"),
            8
        ),
        SymbolAuditVerdict::Deprecated(entry)
            if entry.deprecated_in_epoch == Some(3) && entry.earliest_sunset_epoch == Some(5)
    ));
    assert!(matches!(
        manifest.audit_field_number("nlos.sabi.ServiceDirectory", None, 4),
        SymbolAuditVerdict::SunsetOrReserved { entry: None, .. }
    ));
    assert!(matches!(
        manifest.audit_symbol(
            "nlos.sabi.ServiceDirectory",
            "NegotiateServiceRequest.legacy_transport_hint"
        ),
        SymbolAuditVerdict::SunsetOrReserved { entry: None, .. }
    ));
    assert!(matches!(
        manifest.audit_field_number(
            "nlos.sabi.OperationControl",
            Some("QueryOperationRequest"),
            3
        ),
        SymbolAuditVerdict::SunsetOrReserved { entry: Some(_), .. }
    ));
}

#[test]
#[allow(clippy::too_many_lines)]
fn deprecation_manifest_rejects_short_windows_premature_sunset_and_reserved_reuse() {
    // 1. min_compatibility_window_epochs < 2 -> InvalidCompatibilityWindow.
    let short_global_window = r#"
manifest_version: 1
current_epoch: 3
min_compatibility_window_epochs: 1
frozen_channels_total: 1
channels:
  - name: "nlos.sabi.Envelope"
    major: 3
    minor: 0
    frozen: true
    reserved_field_numbers: []
    reserved_symbols: []
entries: []
"#;
    assert_eq!(
        parse_deprecation_manifest(short_global_window),
        Err(DeprecationManifestError::InvalidCompatibilityWindow { got: 1, minimum: 2 })
    );

    // 2. earliest_sunset_epoch < deprecated_in_epoch + 2 -> InsufficientDeprecationWindow.
    let short_entry_window = r#"
manifest_version: 1
current_epoch: 3
min_compatibility_window_epochs: 2
frozen_channels_total: 1
channels:
  - name: "nlos.sabi.Envelope"
    major: 3
    minor: 0
    frozen: true
    reserved_field_numbers: []
    reserved_symbols: []
entries:
  - channel: "nlos.sabi.Envelope"
    symbol: "Envelope.legacy_tag"
    kind: "FIELD"
    parent_type: "Envelope"
    field_number: 7
    introduced_in_epoch: 1
    state: "DEPRECATED"
    deprecated_in_epoch: 2
    earliest_sunset_epoch: 3
    replacement: "Envelope.common_context"
    reason: "Insufficient 1-epoch grace window must fail closed"
"#;
    assert_eq!(
        parse_deprecation_manifest(short_entry_window),
        Err(DeprecationManifestError::InsufficientDeprecationWindow {
            channel: SABI_ENVELOPE_SCHEMA.to_owned(),
            symbol: "Envelope.legacy_tag".to_owned(),
            deprecated_in_epoch: 2,
            earliest_sunset_epoch: 3,
            required_window_epochs: 2,
        })
    );

    // 3. Premature sunset (current_epoch < sunset_in_epoch) -> PrematureSunset.
    let premature_sunset = r#"
manifest_version: 1
current_epoch: 3
min_compatibility_window_epochs: 2
frozen_channels_total: 1
channels:
  - name: "nlos.sabi.Envelope"
    major: 3
    minor: 0
    frozen: true
    reserved_field_numbers: []
    reserved_symbols: []
entries:
  - channel: "nlos.sabi.Envelope"
    symbol: "Envelope.legacy_tag"
    kind: "FIELD"
    parent_type: "Envelope"
    field_number: 7
    introduced_in_epoch: 1
    state: "SUNSET"
    deprecated_in_epoch: 2
    earliest_sunset_epoch: 4
    sunset_in_epoch: 4
    replacement: "Envelope.common_context"
    reason: "Cannot mark SUNSET at epoch 3 when earliest_sunset_epoch is 4"
"#;
    assert_eq!(
        parse_deprecation_manifest(premature_sunset),
        Err(DeprecationManifestError::PrematureSunset {
            channel: SABI_ENVELOPE_SCHEMA.to_owned(),
            symbol: "Envelope.legacy_tag".to_owned(),
            current_epoch: 3,
            earliest_sunset_epoch: 4,
            sunset_in_epoch: 4,
        })
    );

    // 4. Reusing a field number from a SUNSET entry (even when ACTIVE appears first) -> ReservedSlotReuse.
    let sunset_slot_reuse = r#"
manifest_version: 1
current_epoch: 4
min_compatibility_window_epochs: 2
frozen_channels_total: 1
channels:
  - name: "nlos.sabi.Envelope"
    major: 4
    minor: 0
    frozen: true
    reserved_field_numbers: []
    reserved_symbols: []
entries:
  - channel: "nlos.sabi.Envelope"
    symbol: "Envelope.reused_field"
    kind: "FIELD"
    parent_type: "Envelope"
    field_number: 9
    introduced_in_epoch: 4
    state: "ACTIVE"
  - channel: "nlos.sabi.Envelope"
    symbol: "Envelope.retired_field"
    kind: "FIELD"
    parent_type: "Envelope"
    field_number: 9
    introduced_in_epoch: 1
    state: "SUNSET"
    deprecated_in_epoch: 1
    earliest_sunset_epoch: 3
    sunset_in_epoch: 3
    replacement: "Envelope.common_context"
    reason: "Retired in epoch 3; field 9 is permanently reserved"
"#;
    assert!(matches!(
        parse_deprecation_manifest(sunset_slot_reuse),
        Err(DeprecationManifestError::ReservedSlotReuse {
            channel,
            symbol,
            ..
        }) if channel == SABI_ENVELOPE_SCHEMA && symbol == "Envelope.reused_field"
    ));

    // 5. Reusing a channel-level reserved_field_numbers slot -> ReservedSlotReuse.
    let channel_tombstone_reuse = r#"
manifest_version: 1
current_epoch: 4
min_compatibility_window_epochs: 2
frozen_channels_total: 1
channels:
  - name: "nlos.sabi.Envelope"
    major: 4
    minor: 0
    frozen: true
    reserved_field_numbers: [12]
    reserved_symbols: []
entries:
  - channel: "nlos.sabi.Envelope"
    symbol: "Envelope.bad_new_field"
    kind: "FIELD"
    parent_type: "Envelope"
    field_number: 12
    introduced_in_epoch: 4
    state: "ACTIVE"
"#;
    assert!(matches!(
        parse_deprecation_manifest(channel_tombstone_reuse),
        Err(DeprecationManifestError::ReservedSlotReuse {
            symbol,
            ..
        }) if symbol == "Envelope.bad_new_field"
    ));

    // 6. Invalid state string and unfrozen channel and registry mismatch fail closed.
    let invalid_state = r#"
manifest_version: 1
current_epoch: 1
min_compatibility_window_epochs: 2
frozen_channels_total: 1
channels:
  - name: "nlos.sabi.Envelope"
    major: 1
    minor: 1
    frozen: true
    reserved_field_numbers: []
    reserved_symbols: []
entries:
  - channel: "nlos.sabi.Envelope"
    symbol: "Envelope"
    kind: "MESSAGE"
    introduced_in_epoch: 1
    state: "DELETED"
"#;
    assert_eq!(
        parse_deprecation_manifest(invalid_state),
        Err(DeprecationManifestError::InvalidState {
            symbol: "Envelope".to_owned(),
            state: "DELETED".to_owned(),
        })
    );

    let canonical = canonical_deprecation_manifest().unwrap();
    let mismatched_registry = [SchemaDescriptor {
        name: SABI_ENVELOPE_SCHEMA,
        major: 1,
        minor: 99,
        supported_critical_extensions: &[],
        frozen: true,
    }];
    assert!(matches!(
        canonical.validate_against_registry(&mismatched_registry),
        Err(DeprecationManifestError::RegistryMismatch { .. })
    ));
}
