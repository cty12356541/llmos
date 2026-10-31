//! Functional and fail-closed integration tests for `CompatMountPlanner`,
//! `SabiEpochAdapterCell`, `proto_unknown`, `SabiShimBridge`, and
//! `DriverProtocolShim` (`[SABI-DISC-001]`, `[COMPAT-VER-002]`,
//! `[SABI-TRANSPORT-001]`, `[KABI-IDEM-001]`, `[DRV-NEGO-001]`,
//! `[DRV-PROTO-001]`, `[TASK-PROVIDER-001]`, `ADR-0027`).
#![forbid(unsafe_code)]

use std::io::Cursor;
use std::num::NonZeroU64;

use nlos_compat_shim::driver_shim::{
    CANCEL_PROVIDER_METHOD, COMPLETE_PROVIDER_METHOD, CancelOperationResultWire,
    CancelOperationWire, CompleteOperationResultWire, CompleteOperationWire,
    DecodedProviderRequest, DecodedProviderResponse, DispatchOperationResultWire,
    DispatchOperationWire, OUTCOME_CODE_CANCELLED_BEFORE_EFFECT, OUTCOME_CODE_COMPLETED,
    OUTCOME_CODE_EFFECT_UNKNOWN, OUTCOME_CODE_FAILED, OUTCOME_CODE_PARTIAL_EFFECT,
    PROVIDER_ISOLATION_UNVERIFIED_TAG, PROVIDER_ISOLATION_VERIFIED_TAG,
    ProviderIsolationAttestation, QUERY_ISOLATION_PROVIDER_METHOD, REGISTER_PROVIDER_METHOD,
    RegisterOperationResultWire, RegisterOperationWire, STATE_CODE_CANCEL_REQUESTED,
    STATE_CODE_CANCELLED_BEFORE_EFFECT, STATE_CODE_COMPLETED, STATE_CODE_DISPATCHED,
    STATE_CODE_EFFECT_UNKNOWN, STATE_CODE_FAILED, STATE_CODE_PARTIAL_EFFECT, decode_request_proto,
    decode_response_proto, encode_complete_ack_proto, encode_complete_request,
    encode_complete_result, encode_dispatch_request, encode_dispatch_result,
    encode_register_request, encode_register_request_proto, encode_register_result,
};
use nlos_compat_shim::{
    BridgePumpOutcome, ByteStreamFrameTransport, CompatMountDecision, CompatMountPlanner,
    DriverProtocolShim, DriverShimMode, DriverWireEpoch, DuplexFrameChannel, EpochTranslationRules,
    ExtractedEnvelopeUnknowns, InMemoryFrameDuplex, LegacyCompleteBridgeContext,
    MAX_SHIM_DEPRECATION_WARNINGS, PackageCompatibilitySpec, PackageServiceDemand,
    ProviderTrustCeiling, RawUnknownField, SabiEpochAdapterCell, SabiShimBridge,
    TranslationRuleError, encode_cancel_request, encode_cancel_result,
    encode_envelope_with_unknown_fields, extract_envelope_unknown_fields,
};
use nlos_driver_mock::provider::CompleteProviderOutcome;
use nlos_operation::{CompletionOutcome, OperationHandle, OperationSpec, OperationState};
use nlos_runtime::FiberHandle;
use nlos_schema::sabi::v1::{
    DriverProviderRequest, DriverProviderResponse, Envelope, LocalEndpoint, LocalTransportKind,
    RetryDirective, SabiErrorCode, SabiFailure, SabiRequestContext, SabiResponseContext,
    SchemaIdentity, ServiceCandidate, ServiceVersion, envelope,
};
use nlos_schema::{
    ContractElementKind, DeprecationEntry, DeprecationManifest, DeprecationState,
    MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS, SABI_DRIVER_PROVIDER_SCHEMA, SABI_ENVELOPE_SCHEMA,
    SABI_OPERATION_CONTROL_SCHEMA, SABI_SYSTEM_CONTROL_SCHEMA, VersionCompatibilityPolicy,
    VersionRoute, canonical_deprecation_manifest,
};
use nlos_service_directory::{ServiceRegistration, SnapshotDirectory};
use nlos_types::{
    CallbackId, CancellationScopeId, ExecutionFiberId, Generation, IdempotencyKey, OperationId,
    ReceiptId,
};
use prost::Message;

fn gen_u64(raw: u64) -> Generation {
    Generation::new(NonZeroU64::new(raw).expect("non-zero generation"))
}

fn multi_epoch_manifest(current_epoch: u32) -> DeprecationManifest {
    let mut manifest = canonical_deprecation_manifest().expect("canonical manifest parses");
    manifest.current_epoch = current_epoch;

    if let Some(env_ch) = manifest
        .channels
        .iter_mut()
        .find(|ch| ch.name == SABI_ENVELOPE_SCHEMA)
    {
        env_ch.reserved_field_numbers.push(44);
        env_ch
            .reserved_symbols
            .push("legacy_reserved_rpc".to_owned());
    }

    manifest.entries.push(DeprecationEntry {
        channel: SABI_OPERATION_CONTROL_SCHEMA.to_owned(),
        symbol: "poll_legacy_status".to_owned(),
        kind: ContractElementKind::Method,
        parent_type: Some("OperationControl".to_owned()),
        field_number: None,
        introduced_in_epoch: 1,
        state: DeprecationState::Deprecated,
        deprecated_in_epoch: Some(2),
        earliest_sunset_epoch: Some(4),
        sunset_in_epoch: None,
        replacement: Some("query_operation".to_owned()),
        migration_rule_id: Some("MIG-OP-001".to_owned()),
        reason: Some("replaced by fenced query_operation".to_owned()),
    });

    manifest.entries.push(DeprecationEntry {
        channel: SABI_DRIVER_PROVIDER_SCHEMA.to_owned(),
        symbol: "LegacyStageBMinicborV1".to_owned(),
        kind: ContractElementKind::Extension,
        parent_type: None,
        field_number: None,
        introduced_in_epoch: 1,
        state: DeprecationState::Deprecated,
        deprecated_in_epoch: Some(2),
        earliest_sunset_epoch: Some(4),
        sunset_in_epoch: None,
        replacement: Some("nlos.sabi.DriverProvider".to_owned()),
        migration_rule_id: Some("MIG-DRV-001".to_owned()),
        reason: Some("Stage B minicbor superseded by frozen 8th channel".to_owned()),
    });

    manifest.entries.push(DeprecationEntry {
        channel: SABI_SYSTEM_CONTROL_SCHEMA.to_owned(),
        symbol: "retired_alpha_reboot".to_owned(),
        kind: ContractElementKind::Method,
        parent_type: Some("SystemControl".to_owned()),
        field_number: None,
        introduced_in_epoch: 1,
        state: DeprecationState::Sunset,
        deprecated_in_epoch: Some(1),
        earliest_sunset_epoch: Some(3),
        sunset_in_epoch: Some(3),
        replacement: Some("controlled_shutdown".to_owned()),
        migration_rule_id: Some("MIG-SYS-001".to_owned()),
        reason: Some("retired at epoch 3".to_owned()),
    });

    manifest.entries.push(DeprecationEntry {
        channel: SABI_ENVELOPE_SCHEMA.to_owned(),
        symbol: "Envelope.retired_trace_blob".to_owned(),
        kind: ContractElementKind::Field,
        parent_type: Some("Envelope".to_owned()),
        field_number: Some(91),
        introduced_in_epoch: 1,
        state: DeprecationState::Sunset,
        deprecated_in_epoch: Some(1),
        earliest_sunset_epoch: Some(3),
        sunset_in_epoch: Some(3),
        replacement: Some("SabiRequestContext.activity_context".to_owned()),
        migration_rule_id: Some("MIG-ENV-091".to_owned()),
        reason: Some("retired trace blob slot".to_owned()),
    });

    manifest
}

fn sample_directory(host_epoch: u32) -> SnapshotDirectory {
    let registration = ServiceRegistration {
        candidate: ServiceCandidate {
            binding_id: [0x31; 16].to_vec(),
            service: "operation_control".to_owned(),
            version: Some(ServiceVersion {
                schema_name: SABI_OPERATION_CONTROL_SCHEMA.to_owned(),
                major: host_epoch,
                minor: 2,
            }),
            generation: 1,
            feature_ids: vec![1, 2],
            transport_kinds: vec![LocalTransportKind::UnixSocket as i32],
        },
        endpoint: LocalEndpoint {
            kind: LocalTransportKind::UnixSocket as i32,
            address: "ns-handle://operation-control-v3".to_owned(),
        },
    };
    SnapshotDirectory::new([registration]).expect("valid directory snapshot")
}

fn sample_request_envelope(
    major: u32,
    minor: u32,
    service: &str,
    method: &str,
    request_id: [u8; 16],
    idempotency_key: [u8; 16],
    non_critical_extensions: Vec<u32>,
) -> Envelope {
    Envelope {
        schema: Some(SchemaIdentity {
            name: SABI_ENVELOPE_SCHEMA.to_owned(),
            major,
            minor,
            critical_extension_ids: Vec::new(),
            non_critical_extension_ids: non_critical_extensions,
        }),
        request_id: request_id.to_vec(),
        service: service.to_owned(),
        method: method.to_owned(),
        common_context: Some(envelope::CommonContext::RequestContext(
            SabiRequestContext {
                caller: None,
                activity_context: Vec::new(),
                task_execution_binding: None,
                correlation_id: Vec::new(),
                idempotency_key: idempotency_key.to_vec(),
                deadline_monotonic_ns: 95_000_000,
                capability_handles: Vec::new(),
                reservation_handle: None,
                proposal_or_input_digest_sha256: Vec::new(),
            },
        )),
        payload: vec![0xDE, 0xAD, 0xBE, 0xEF],
    }
}

#[test]
fn mount_native_epoch_n_uses_zero_hop_passthrough() {
    let host_epoch = 3;
    let planner = CompatMountPlanner::new(multi_epoch_manifest(host_epoch));
    let policy = VersionCompatibilityPolicy::with_backward_window(
        host_epoch,
        MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS,
        &[11],
    );
    let mut directory = sample_directory(host_epoch);

    let mut spec = PackageCompatibilitySpec::new("pkg.modern.native", host_epoch, 2);
    spec.critical_extensions = vec![11];
    spec.noncritical_extensions = vec![501];

    let demands = [PackageServiceDemand {
        service: "operation_control".to_owned(),
        schema_name: SABI_OPERATION_CONTROL_SCHEMA.to_owned(),
        minimum_minor: 1,
        required_feature_ids: vec![1],
        supported_transport_kinds: vec![LocalTransportKind::UnixSocket as i32],
    }];

    let decision = planner
        .plan_mount_with_directory(&spec, &policy, &mut directory, &demands)
        .expect("native epoch N mount succeeds");
    assert_eq!(decision.shim_hop_count(), 0);
    assert_eq!(decision.warnings(), []);
    assert_eq!(decision.negotiated_bindings().len(), 1);
    match decision {
        CompatMountDecision::NativePassthrough {
            sabi_negotiation, ..
        } => {
            assert_eq!(sabi_negotiation.route, VersionRoute::NativeCurrentEpoch);
            assert_eq!(sabi_negotiation.retained_non_critical_extensions, vec![501]);
        }
        CompatMountDecision::MountOutOfCoreShim(_) => {
            panic!("native current-epoch package must never mount a shim")
        }
    }
}

#[test]
fn mount_n_minus_1_and_n_minus_2_dynamically_mounts_sabi_adapter_and_emits_deprecation_warnings() {
    let host_epoch = 3;
    let planner = CompatMountPlanner::new(multi_epoch_manifest(host_epoch));
    let policy = VersionCompatibilityPolicy::with_backward_window(
        host_epoch,
        MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS,
        &[],
    );
    let mut directory = sample_directory(host_epoch);

    for client_epoch in [2_u32, 1_u32] {
        let mut spec = PackageCompatibilitySpec::new(
            format!("pkg.historical.v{client_epoch}"),
            client_epoch,
            0,
        );
        spec.referenced_symbols = vec![(
            SABI_OPERATION_CONTROL_SCHEMA.to_owned(),
            "poll_legacy_status".to_owned(),
        )];
        let demands = [PackageServiceDemand {
            service: "operation_control".to_owned(),
            schema_name: SABI_OPERATION_CONTROL_SCHEMA.to_owned(),
            minimum_minor: 0,
            required_feature_ids: vec![1],
            supported_transport_kinds: vec![LocalTransportKind::UnixSocket as i32],
        }];

        let decision = planner
            .plan_mount_with_directory(&spec, &policy, &mut directory, &demands)
            .expect("N-1 / N-2 historical package mounts out-of-core shim");
        assert_eq!(decision.shim_hop_count(), 1);
        assert_eq!(decision.warnings().len(), 1);
        assert_eq!(decision.warnings()[0].symbol, "poll_legacy_status");
        assert_eq!(decision.warnings()[0].deprecated_in_epoch, 2);
        assert_eq!(decision.warnings()[0].earliest_sunset_epoch, 4);
        assert_eq!(decision.warnings()[0].replacement, "query_operation");

        let CompatMountDecision::MountOutOfCoreShim(stack) = decision else {
            panic!("expected MountOutOfCoreShim for historical epoch {client_epoch}");
        };
        let adapter = stack.sabi_adapter.expect("SabiEpochAdapterCell mounted");
        assert_eq!(adapter.client_epoch(), client_epoch);
        assert_eq!(adapter.host_epoch(), host_epoch);
        assert!(stack.driver_shim.is_none());
        assert_eq!(adapter.deprecation_warning_count(), 1);
    }
}

#[test]
fn mount_native_epoch_n_with_legacy_stage_b_minicbor_driver_mounts_only_driver_shim() {
    let host_epoch = 3;
    let planner = CompatMountPlanner::new(multi_epoch_manifest(host_epoch));
    let policy = VersionCompatibilityPolicy::new(host_epoch, &[]);

    let mut spec = PackageCompatibilitySpec::new("pkg.driver.stage-b-cbor", host_epoch, 0);
    spec.driver_abi_epoch = Some(DriverWireEpoch::LegacyStageBMinicborV1);

    let decision = planner
        .plan_mount(&spec, &policy)
        .expect("legacy Stage B minicbor driver mounts DriverProtocolShim");
    assert_eq!(decision.shim_hop_count(), 1);
    assert_eq!(decision.warnings().len(), 1);
    assert_eq!(decision.warnings()[0].symbol, "LegacyStageBMinicborV1");

    let CompatMountDecision::MountOutOfCoreShim(stack) = decision else {
        panic!("expected MountOutOfCoreShim");
    };
    assert!(stack.sabi_adapter.is_none());
    let drv_shim = stack.driver_shim.expect("DriverProtocolShim mounted");
    assert_eq!(
        drv_shim.mode(),
        DriverShimMode::LegacyStageBMinicbor { host_epoch }
    );
    assert_eq!(
        drv_shim.trust_ceiling(),
        ProviderTrustCeiling::ForceUnverified
    );
}

#[test]
fn mount_n_minus_1_with_historical_frozen_driver_epoch_mounts_two_hop_stack() {
    let host_epoch = 3;
    let planner = CompatMountPlanner::new(multi_epoch_manifest(host_epoch));
    let policy = VersionCompatibilityPolicy::new(host_epoch, &[]);

    let mut spec = PackageCompatibilitySpec::new("pkg.driver.frozen-v2", 2, 1);
    spec.driver_abi_epoch = Some(DriverWireEpoch::FrozenDriverProviderEpoch(2));

    let decision = planner
        .plan_mount(&spec, &policy)
        .expect("N-1 package + N-1 frozen driver mounts 2-hop out-of-core stack");
    assert_eq!(decision.shim_hop_count(), 2);

    let CompatMountDecision::MountOutOfCoreShim(stack) = decision else {
        panic!("expected MountOutOfCoreShim");
    };
    assert!(stack.sabi_adapter.is_some());
    let drv_shim = stack.driver_shim.expect("historical DriverProtocolShim");
    assert_eq!(
        drv_shim.mode(),
        DriverShimMode::HistoricalFrozenEpoch {
            driver_epoch: 2,
            host_epoch: 3,
        }
    );
}

#[test]
fn mount_fails_closed_with_e_abi_version_on_zero_future_and_sunset_epochs() {
    let host_epoch = 4;
    let planner = CompatMountPlanner::new(multi_epoch_manifest(host_epoch));
    let policy = VersionCompatibilityPolicy::with_backward_window(
        host_epoch,
        MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS,
        &[],
    );

    // Epoch 0, Future N+1 (5), and Sunset N-3 (1 on host 4) must all fail closed with E_ABI_VERSION.
    for invalid_epoch in [0_u32, 5_u32, 1_u32] {
        let spec = PackageCompatibilitySpec::new("pkg.invalid.epoch", invalid_epoch, 0);
        let err = planner
            .plan_mount(&spec, &policy)
            .expect_err("out-of-window epoch must fail closed");
        let failure = err.to_sabi_failure();
        assert_eq!(failure.code, SabiErrorCode::AbiVersion as i32);
        assert_eq!(failure.retry, RetryDirective::DoNotRetry as i32);
    }

    // Sunset driver epoch (1 on host 4) even when SABI target is current (4).
    let mut bad_drv_spec = PackageCompatibilitySpec::new("pkg.bad.driver", host_epoch, 0);
    bad_drv_spec.driver_abi_epoch = Some(DriverWireEpoch::FrozenDriverProviderEpoch(1));
    let drv_err = planner
        .plan_mount(&bad_drv_spec, &policy)
        .expect_err("sunset driver epoch must fail closed");
    assert_eq!(
        drv_err.to_sabi_failure().code,
        SabiErrorCode::AbiVersion as i32
    );
}

#[test]
fn mount_fails_closed_with_e_abi_version_on_unknown_critical_extension_and_sunset_or_reserved_symbols_and_fields()
 {
    let host_epoch = 4;
    let planner = CompatMountPlanner::new(multi_epoch_manifest(host_epoch));
    let policy = VersionCompatibilityPolicy::new(host_epoch, &[10]);

    // 1. Unknown critical extension 999.
    let mut crit_spec = PackageCompatibilitySpec::new("pkg.unknown.crit", 3, 0);
    crit_spec.critical_extensions = vec![10, 999];
    let crit_err = planner
        .plan_mount(&crit_spec, &policy)
        .expect_err("unknown critical extension must fail closed");
    assert_eq!(
        crit_err.to_sabi_failure().code,
        SabiErrorCode::AbiVersion as i32
    );

    // 2. SUNSET method symbol.
    let mut sunset_sym_spec = PackageCompatibilitySpec::new("pkg.sunset.sym", 3, 0);
    sunset_sym_spec.referenced_symbols = vec![(
        SABI_SYSTEM_CONTROL_SCHEMA.to_owned(),
        "retired_alpha_reboot".to_owned(),
    )];
    let sunset_err = planner
        .plan_mount(&sunset_sym_spec, &policy)
        .expect_err("SUNSET symbol must fail closed");
    assert_eq!(
        sunset_err.to_sabi_failure().code,
        SabiErrorCode::AbiVersion as i32
    );

    // 3. Permanently RESERVED field slot (field 44 on Envelope).
    let mut reserved_field_spec = PackageCompatibilitySpec::new("pkg.reserved.field", 3, 0);
    reserved_field_spec.referenced_symbols = vec![(
        SABI_ENVELOPE_SCHEMA.to_owned(),
        "Envelope.field:44".to_owned(),
    )];
    let reserved_err = planner
        .plan_mount(&reserved_field_spec, &policy)
        .expect_err("permanently RESERVED field slot must fail closed");
    assert_eq!(
        reserved_err.to_sabi_failure().code,
        SabiErrorCode::AbiVersion as i32
    );

    // 4. Permanently RESERVED symbol tombstone.
    let mut reserved_sym_spec = PackageCompatibilitySpec::new("pkg.reserved.sym", 3, 0);
    reserved_sym_spec.referenced_symbols = vec![(
        SABI_ENVELOPE_SCHEMA.to_owned(),
        "legacy_reserved_rpc".to_owned(),
    )];
    let res_sym_err = planner
        .plan_mount(&reserved_sym_spec, &policy)
        .expect_err("permanently RESERVED symbol must fail closed");
    assert_eq!(
        res_sym_err.to_sabi_failure().code,
        SabiErrorCode::AbiVersion as i32
    );
}

#[test]
fn sabi_adapter_bidirectional_n_minus_1_and_n_minus_2_preserves_128bit_request_id_and_idempotency_key()
 {
    let host_epoch = 3;
    let rules = EpochTranslationRules {
        host_envelope_minor: 2,
        client_envelope_minor: Some(0),
        backfill_empty_correlation_from_request_id: true,
        default_activity_context: b"shim-default-activity-v1".to_vec(),
        error_code_downgrades: vec![(SabiErrorCode::Quota, SabiErrorCode::Budget)],
    };

    for client_epoch in [2_u32, 1_u32] {
        let mut cell = SabiEpochAdapterCell::new(
            client_epoch,
            host_epoch,
            MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS,
            Vec::new(),
            multi_epoch_manifest(host_epoch),
            rules.clone(),
        )
        .expect("valid adapter cell");

        let req_id = [0xAA; 16];
        let idemp_raw = [0x55; 16];
        let client_env = sample_request_envelope(
            client_epoch,
            0,
            "operation_control",
            "query_operation",
            req_id,
            idemp_raw,
            vec![101, 202],
        );

        let up = cell
            .translate_client_to_host_wire(&client_env.encode_to_vec())
            .expect("client -> host upgrade succeeds");
        assert_eq!(up.preserved_request_id, req_id);
        assert_eq!(
            up.preserved_idempotency_key,
            Some(IdempotencyKey::from_bytes(idemp_raw))
        );
        let up_schema = up.envelope.schema.as_ref().expect("schema");
        assert_eq!(up_schema.major, host_epoch);
        assert_eq!(up_schema.minor, 2);
        assert_eq!(up_schema.non_critical_extension_ids, vec![101, 202]);

        let Some(envelope::CommonContext::RequestContext(up_ctx)) = &up.envelope.common_context
        else {
            panic!("expected RequestContext");
        };
        assert_eq!(up_ctx.correlation_id, req_id);
        assert_eq!(up_ctx.activity_context, b"shim-default-activity-v1");
        assert_eq!(up_ctx.idempotency_key, idemp_raw);

        // Now downgrade a host response frame back to client_epoch.
        let host_resp = Envelope {
            schema: Some(SchemaIdentity {
                name: SABI_ENVELOPE_SCHEMA.to_owned(),
                major: host_epoch,
                minor: 2,
                critical_extension_ids: Vec::new(),
                non_critical_extension_ids: vec![101, 202],
            }),
            request_id: req_id.to_vec(),
            service: "operation_control".to_owned(),
            method: "query_operation".to_owned(),
            common_context: Some(envelope::CommonContext::ResponseContext(
                SabiResponseContext {
                    correlation_id: req_id.to_vec(),
                    operation: None,
                    receipts: Vec::new(),
                    failure: Some(SabiFailure {
                        code: SabiErrorCode::Quota as i32,
                        retry: RetryDirective::DoNotRetry as i32,
                        safe_message: "quota limit".to_owned(),
                    }),
                },
            )),
            payload: vec![1, 2, 3],
        };

        let down = cell
            .translate_host_to_client_wire(&host_resp.encode_to_vec())
            .expect("host -> client downgrade succeeds");
        assert_eq!(down.preserved_request_id, req_id);
        let down_schema = down.envelope.schema.as_ref().expect("schema");
        assert_eq!(down_schema.major, client_epoch);
        assert_eq!(down_schema.minor, 0);
        assert_eq!(down_schema.non_critical_extension_ids, vec![101, 202]);

        let Some(envelope::CommonContext::ResponseContext(down_ctx)) =
            &down.envelope.common_context
        else {
            panic!("expected ResponseContext");
        };
        assert_eq!(
            down_ctx.failure.as_ref().expect("failure").code,
            SabiErrorCode::Budget as i32
        );
    }
}

#[test]
fn sabi_adapter_preserves_noncritical_extension_ids_and_raw_unknown_protobuf_wire_tags_round_trip()
{
    let mut cell = SabiEpochAdapterCell::with_canonical_manifest(1, 2).expect("1 <-> 2 adapter");
    let req_id = [0x12; 16];
    let idemp_raw = [0x34; 16];
    let base_env = sample_request_envelope(
        1,
        1,
        "operation_control",
        "query_operation",
        req_id,
        idemp_raw,
        vec![7, 42],
    );

    // Construct raw unknown Protobuf tags across Envelope (field 77 varint, field 78 bytes),
    // SchemaIdentity (field 61 varint), and SabiRequestContext (field 62 fixed32).
    let injected_unknowns = ExtractedEnvelopeUnknowns {
        envelope_fields: vec![
            RawUnknownField {
                field_number: 77,
                wire_type: 0,
                // key = (77 << 3) | 0 = 616 => varint [0xE8, 0x04], value = 42 (0x2A)
                raw_bytes: vec![0xE8, 0x04, 0x2A],
            },
            RawUnknownField {
                field_number: 78,
                wire_type: 2,
                raw_bytes: {
                    // field 78, wire_type 2 => key = (78 << 3) | 2 = 626 => varint [0xF2, 0x04]
                    let mut v = vec![0xF2, 0x04, 0x05];
                    v.extend_from_slice(b"extra");
                    v
                },
            },
        ],
        schema_fields: vec![RawUnknownField {
            field_number: 61,
            wire_type: 0,
            raw_bytes: vec![0xE8, 0x03, 0x09], // key = (61 << 3) = 488 => [0xE8, 0x03], val = 9
        }],
        request_context_fields: vec![RawUnknownField {
            field_number: 62,
            wire_type: 5,
            raw_bytes: vec![0xF5, 0x03, 0xEF, 0xBE, 0xAD, 0xDE], // key = (62 << 3) | 5 = 501 => [0xF5, 0x03]
        }],
        response_context_fields: Vec::new(),
    };

    let client_wire_with_unknowns =
        encode_envelope_with_unknown_fields(&base_env, &injected_unknowns)
            .expect("injects unknown tags cleanly");

    let upgraded = cell
        .translate_client_to_host_wire(&client_wire_with_unknowns)
        .expect("upgrade preserves unknown tags");
    assert_eq!(upgraded.preserved_unknown_fields, injected_unknowns);
    assert_eq!(
        extract_envelope_unknown_fields(&upgraded.wire).expect("extract from upgraded"),
        injected_unknowns
    );

    // Round-trip back from host (2) -> client (1) and confirm 100% unknown tag survival.
    let downgraded = cell
        .translate_host_to_client_wire(&upgraded.wire)
        .expect("downgrade preserves unknown tags");
    assert_eq!(downgraded.preserved_unknown_fields, injected_unknowns);
    assert_eq!(
        extract_envelope_unknown_fields(&downgraded.wire).expect("extract from downgraded"),
        injected_unknowns
    );
    assert_eq!(
        downgraded
            .envelope
            .schema
            .as_ref()
            .expect("schema")
            .non_critical_extension_ids,
        vec![7, 42]
    );
}

#[test]
fn sabi_adapter_fails_closed_with_e_abi_version_on_unknown_critical_extension_and_sunset_wire_field()
 {
    let host_epoch = 4;
    let mut cell = SabiEpochAdapterCell::new(
        3,
        host_epoch,
        MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS,
        vec![5],
        multi_epoch_manifest(host_epoch),
        EpochTranslationRules::default(),
    )
    .expect("valid adapter");

    // 1. Unsupported critical extension 88 -> E_ABI_VERSION.
    let mut crit_env = sample_request_envelope(
        3,
        0,
        "operation_control",
        "query_operation",
        [0x01; 16],
        [0x02; 16],
        Vec::new(),
    );
    crit_env
        .schema
        .as_mut()
        .expect("schema")
        .critical_extension_ids = vec![5, 88];
    let err = cell
        .translate_client_to_host_wire(&crit_env.encode_to_vec())
        .expect_err("unknown critical extension 88 must fail closed");
    assert_eq!(err.to_sabi_failure().code, SabiErrorCode::AbiVersion as i32);

    // 2. Unknown wire tag using SUNSET field_number 91 on Envelope -> E_ABI_VERSION.
    let clean_env = sample_request_envelope(
        3,
        0,
        "operation_control",
        "query_operation",
        [0x03; 16],
        [0x04; 16],
        Vec::new(),
    );
    // field 91, varint => key = 91 << 3 = 728 => varint [0xD8, 0x05]
    let sunset_unknown = ExtractedEnvelopeUnknowns {
        envelope_fields: vec![RawUnknownField {
            field_number: 91,
            wire_type: 0,
            raw_bytes: vec![0xD8, 0x05, 0x01],
        }],
        ..ExtractedEnvelopeUnknowns::default()
    };
    let wire_with_sunset_tag =
        encode_envelope_with_unknown_fields(&clean_env, &sunset_unknown).expect("encoded");
    let sunset_err = cell
        .translate_client_to_host_wire(&wire_with_sunset_tag)
        .expect_err("SUNSET field 91 on wire must fail closed");
    assert_eq!(
        sunset_err.to_sabi_failure().code,
        SabiErrorCode::AbiVersion as i32
    );
}

#[test]
fn sabi_adapter_downgrades_modern_error_codes_safely_and_forbids_collapsing_uncertain_or_partial() {
    for forbidden_from in [
        SabiErrorCode::EffectUnknown,
        SabiErrorCode::Uncertain,
        SabiErrorCode::Partial,
    ] {
        let bad_rules = EpochTranslationRules {
            error_code_downgrades: vec![(forbidden_from, SabiErrorCode::Driver)],
            ..EpochTranslationRules::default()
        };
        assert!(matches!(
            bad_rules.validate(),
            Err(TranslationRuleError::UnsafeUncertaintyDowngrade { .. })
        ));
    }

    // Valid uncertainty-preserving mapping (EffectUnknown -> Uncertain) is allowed.
    let safe_rules = EpochTranslationRules {
        error_code_downgrades: vec![
            (SabiErrorCode::EffectUnknown, SabiErrorCode::Uncertain),
            (SabiErrorCode::NotSupported, SabiErrorCode::InvalidArgument),
        ],
        ..EpochTranslationRules::default()
    };
    safe_rules
        .validate()
        .expect("uncertainty-preserving mapping is valid");
}

#[test]
fn sabi_adapter_caps_deprecation_warning_ring_at_256_fifo() {
    let host_epoch = 3;
    let mut cell = SabiEpochAdapterCell::new(
        2,
        host_epoch,
        MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS,
        Vec::new(),
        multi_epoch_manifest(host_epoch),
        EpochTranslationRules::default(),
    )
    .expect("valid adapter");

    for seq in 0..320_u16 {
        let mut req_id = [0_u8; 16];
        req_id[..2].copy_from_slice(&seq.to_be_bytes());
        let env = sample_request_envelope(
            2,
            u32::from(seq),
            "operation_control",
            "poll_legacy_status",
            req_id,
            [0x77; 16],
            Vec::new(),
        );
        let translated = cell
            .translate_client_to_host_wire(&env.encode_to_vec())
            .expect("deprecated method translates with warning");
        assert_eq!(translated.emitted_warnings.len(), 1);
    }

    assert_eq!(
        cell.deprecation_warning_count(),
        MAX_SHIM_DEPRECATION_WARNINGS
    );
    let ring = cell.drain_deprecation_warnings();
    assert_eq!(ring.len(), 256);
    // Oldest 64 (0..64) were evicted FIFO; first retained entry has caller_minor == 64.
    assert_eq!(ring[0].caller_minor, 64);
    assert_eq!(ring[255].caller_minor, 319);
    assert_eq!(cell.deprecation_warning_count(), 0);
}

#[test]
fn sabi_shim_bridge_operates_over_in_memory_duplex_and_byte_stream_transport_neutrally() {
    // 1. InMemoryFrameDuplex end-to-end pump + fail-closed client rejection.
    let (mut client_end, shim_client_side) = InMemoryFrameDuplex::pair();
    let (shim_host_side, mut host_end) = InMemoryFrameDuplex::pair();
    let adapter = SabiEpochAdapterCell::with_canonical_manifest(1, 2).expect("1 <-> 2 adapter");
    let mut bridge = SabiShimBridge::new(adapter, shim_client_side, shim_host_side);

    let valid_req = sample_request_envelope(
        1,
        0,
        "operation_control",
        "query_operation",
        [0x81; 16],
        [0x82; 16],
        vec![9],
    );
    client_end
        .send_frame(&valid_req.encode_to_vec())
        .expect("client sends v1 frame");
    let outcome = bridge.pump_client_to_host_once().expect("pump succeeds");
    assert!(matches!(outcome, BridgePumpOutcome::Forwarded(_)));

    let host_received_bytes = host_end
        .recv_frame()
        .expect("recv ok")
        .expect("frame arrived at host");
    let decoded_host_envelope =
        Envelope::decode(host_received_bytes.as_slice()).expect("decode host env");
    assert_eq!(decoded_host_envelope.schema.expect("schema").major, 2);

    // Send invalid critical extension from client; verify host receives NOTHING and client gets E_ABI_VERSION.
    let mut bad_req = valid_req.clone();
    bad_req
        .schema
        .as_mut()
        .expect("schema")
        .critical_extension_ids = vec![404];
    client_end
        .send_frame(&bad_req.encode_to_vec())
        .expect("client sends bad frame");
    let rej_outcome = bridge
        .pump_client_to_host_once()
        .expect("pump handles rejection");
    assert!(matches!(
        rej_outcome,
        BridgePumpOutcome::RejectedToClient { .. }
    ));
    assert!(host_end.recv_frame().expect("host poll").is_none());

    let client_rej_bytes = client_end
        .recv_frame()
        .expect("client recv")
        .expect("rejection frame returned to client");
    let client_rej_env = Envelope::decode(client_rej_bytes.as_slice()).expect("decode rejection");
    assert_eq!(client_rej_env.schema.expect("schema").major, 1);
    let Some(envelope::CommonContext::ResponseContext(resp_ctx)) = client_rej_env.common_context
    else {
        panic!("expected ResponseContext on rejection");
    };
    assert_eq!(
        resp_ctx.failure.expect("failure").code,
        SabiErrorCode::AbiVersion as i32
    );

    // 2. ByteStreamFrameTransport over generic Read + Write memory buffers.
    let mut stream_buf = Vec::new();
    {
        let mut writer_tx =
            ByteStreamFrameTransport::new(Cursor::new(Vec::<u8>::new()), &mut stream_buf);
        writer_tx
            .send_frame(&valid_req.encode_to_vec())
            .expect("length-prefixed stream write");
    }
    let mut reader_tx = ByteStreamFrameTransport::new(Cursor::new(stream_buf), Vec::<u8>::new());
    let roundtrip_frame = reader_tx
        .recv_frame()
        .expect("stream read")
        .expect("one frame");
    assert_eq!(roundtrip_frame, valid_req.encode_to_vec());
    assert!(reader_tx.recv_frame().expect("clean EOF").is_none());
}

#[test]
#[allow(clippy::too_many_lines)]
fn driver_shim_round_trips_register_dispatch_complete_and_cancel_between_minicbor_and_frozen_proto_with_1_to_1_fidelity()
 {
    let shim = DriverProtocolShim::new_legacy_minicbor(2).expect("legacy minicbor shim at host v2");

    // 1. Register request & result 1:1 round-trip.
    let reg_req = RegisterOperationWire {
        operation_id: [0x11; 16],
        operation_generation: 3,
        owner_fiber_id: [0x22; 16],
        owner_fiber_generation: 5,
        cancellation_scope_id: [0x33; 16],
        cancellation_generation: 7,
    };
    let reg_cbor = encode_register_request(&reg_req).expect("encode register cbor");
    let reg_proto = shim
        .legacy_minicbor_request_to_frozen_proto(REGISTER_PROVIDER_METHOD, &reg_cbor)
        .expect("cbor -> proto");
    let (method, reg_cbor_back) = shim
        .frozen_proto_request_to_legacy_minicbor(&reg_proto)
        .expect("proto -> cbor");
    assert_eq!(method, REGISTER_PROVIDER_METHOD);
    assert_eq!(reg_cbor_back, reg_cbor);

    let reg_ack = RegisterOperationResultWire {
        replayed: true,
        operation_id: [0x11; 16],
        operation_generation: 3,
        admission_receipt_id: [0x44; 16],
    };
    let reg_ack_cbor = encode_register_result(&reg_ack).expect("encode reg ack");
    let reg_ack_proto = shim
        .legacy_minicbor_response_to_frozen_proto(REGISTER_PROVIDER_METHOD, &reg_ack_cbor, None)
        .expect("reg ack -> proto");
    let (_, reg_ack_back) = shim
        .frozen_proto_response_to_legacy_minicbor(&reg_ack_proto)
        .expect("proto -> reg ack");
    assert_eq!(reg_ack_back, reg_ack_cbor);

    // 2. Dispatch request & result 1:1 round-trip.
    let disp_req = DispatchOperationWire {
        operation_id: [0x11; 16],
        operation_generation: 3,
        callback_id: [0x55; 16],
    };
    let disp_cbor = encode_dispatch_request(&disp_req).expect("encode dispatch");
    let disp_proto = shim
        .legacy_minicbor_request_to_frozen_proto("dispatch", &disp_cbor)
        .expect("dispatch -> proto");
    let (_, disp_back) = shim
        .frozen_proto_request_to_legacy_minicbor(&disp_proto)
        .expect("proto -> dispatch");
    assert_eq!(disp_back, disp_cbor);

    let dispatch_ack = DispatchOperationResultWire {
        replayed: false,
        preparation_receipt_id: [0x61; 16],
        activation_receipt_id: [0x62; 16],
        callback_id: [0x55; 16],
        operation_id: [0x11; 16],
        operation_generation: 3,
        owner_fiber_id: [0x22; 16],
        owner_fiber_generation: 5,
        cancel_epoch: 0,
    };
    let disp_ack_cbor = encode_dispatch_result(&dispatch_ack).expect("encode disp ack");
    let disp_ack_proto = shim
        .legacy_minicbor_response_to_frozen_proto("dispatch", &disp_ack_cbor, None)
        .expect("disp ack -> proto");
    let (_, disp_ack_back) = shim
        .frozen_proto_response_to_legacy_minicbor(&disp_ack_proto)
        .expect("proto -> disp ack");
    assert_eq!(disp_ack_back, disp_ack_cbor);

    // 3. Complete request & all 5 terminal outcome/state variants 1:1 round-trip.
    let comp_req = CompleteOperationWire {
        operation_id: [0x11; 16],
        operation_generation: 3,
        callback_id: [0x55; 16],
        seed: [0x99; 32],
    };
    let comp_cbor = encode_complete_request(&comp_req).expect("encode complete");
    let comp_proto = shim
        .legacy_minicbor_request_to_frozen_proto(COMPLETE_PROVIDER_METHOD, &comp_cbor)
        .expect("complete -> proto");
    let (_, comp_back) = shim
        .frozen_proto_request_to_legacy_minicbor(&comp_proto)
        .expect("proto -> complete");
    assert_eq!(comp_back, comp_cbor);

    let bridge_ctx = LegacyCompleteBridgeContext {
        handle: OperationHandle {
            operation_id: OperationId::from_bytes([0x11; 16]),
            generation: gen_u64(3),
        },
        callback_id: CallbackId::from_bytes([0x55; 16]),
        seed: [0x99; 32],
        fenced_reconciliation_only: false,
    };
    for (outcome_code, state_code) in [
        (OUTCOME_CODE_COMPLETED, STATE_CODE_COMPLETED),
        (OUTCOME_CODE_FAILED, STATE_CODE_FAILED),
        (OUTCOME_CODE_PARTIAL_EFFECT, STATE_CODE_PARTIAL_EFFECT),
        (OUTCOME_CODE_EFFECT_UNKNOWN, STATE_CODE_EFFECT_UNKNOWN),
        (
            OUTCOME_CODE_CANCELLED_BEFORE_EFFECT,
            STATE_CODE_CANCELLED_BEFORE_EFFECT,
        ),
    ] {
        let complete_ack = CompleteOperationResultWire {
            replayed: false,
            outcome_code,
            receipt_id: [0x77; 16],
            state_code,
        };
        let comp_ack_cbor = encode_complete_result(&complete_ack).expect("encode comp ack");
        let comp_ack_proto = shim
            .legacy_minicbor_response_to_frozen_proto(
                COMPLETE_PROVIDER_METHOD,
                &comp_ack_cbor,
                Some(bridge_ctx),
            )
            .expect("comp ack -> proto");
        let (_, comp_ack_back) = shim
            .frozen_proto_response_to_legacy_minicbor(&comp_ack_proto)
            .expect("proto -> comp ack");
        assert_eq!(comp_ack_back, comp_ack_cbor);
    }

    // 4. Cancel request & result (both non-terminal CancelRequested and terminal CancelledBeforeEffect).
    let cancel_req = CancelOperationWire {
        operation_id: [0x11; 16],
        operation_generation: 3,
        expected_cancel_epoch: 0,
        no_effect_receipt_id: [0x88; 16],
    };
    let cancel_cbor = encode_cancel_request(&cancel_req).expect("encode cancel");
    let cancel_proto = shim
        .legacy_minicbor_request_to_frozen_proto(CANCEL_PROVIDER_METHOD, &cancel_cbor)
        .expect("cancel -> proto");
    let (_, cancel_back) = shim
        .frozen_proto_request_to_legacy_minicbor(&cancel_proto)
        .expect("proto -> cancel");
    assert_eq!(cancel_back, cancel_cbor);

    for (state_code, terminal_receipt_id) in [
        (STATE_CODE_CANCEL_REQUESTED, None),
        (STATE_CODE_CANCELLED_BEFORE_EFFECT, Some([0x88; 16])),
        (STATE_CODE_COMPLETED, Some([0x89; 16])),
    ] {
        let cancellation_ack = CancelOperationResultWire {
            replayed: false,
            already_terminal: terminal_receipt_id.is_some(),
            operation_id: [0x11; 16],
            operation_generation: 3,
            state_code,
            cancel_epoch: 1,
            terminal_receipt_id,
        };
        let cancel_ack_cbor = encode_cancel_result(&cancellation_ack).expect("encode cancel ack");
        let cancel_ack_proto = shim
            .legacy_minicbor_response_to_frozen_proto(
                CANCEL_PROVIDER_METHOD,
                &cancel_ack_cbor,
                None,
            )
            .expect("cancel ack -> proto");
        let (_, cancel_ack_back) = shim
            .frozen_proto_response_to_legacy_minicbor(&cancel_ack_proto)
            .expect("proto -> cancel ack");
        assert_eq!(cancel_ack_back, cancel_ack_cbor);
    }

    // Invalid post-cancel state (Dispatched) must fail closed during translation to frozen proto.
    let invalid_cancel_cbor = encode_cancel_result(&CancelOperationResultWire {
        replayed: false,
        already_terminal: false,
        operation_id: [0x11; 16],
        operation_generation: 3,
        state_code: STATE_CODE_DISPATCHED,
        cancel_epoch: 1,
        terminal_receipt_id: None,
    })
    .expect("encode raw legacy cancel cbor");
    assert!(
        shim.legacy_minicbor_response_to_frozen_proto(
            CANCEL_PROVIDER_METHOD,
            &invalid_cancel_cbor,
            None,
        )
        .is_err()
    );
}

#[test]
fn driver_shim_synthesizes_unverified_isolation_for_legacy_minicbor_and_blocks_forged_elevation() {
    let shim = DriverProtocolShim::new_legacy_minicbor(1).expect("v1 legacy minicbor shim");

    // 1. query_isolation cannot be sent down as minicbor, and synthesized report is strictly UNVERIFIED.
    let iso_report_bytes = shim
        .answer_query_isolation_for_legacy_driver()
        .expect("synthesizes unverified report");
    let DecodedProviderResponse::IsolationReport(attestation) =
        decode_response_proto(&iso_report_bytes).expect("decode report")
    else {
        panic!("expected IsolationReport");
    };
    assert_eq!(
        attestation.evaluate_posture().receipt_marker(),
        PROVIDER_ISOLATION_UNVERIFIED_TAG
    );
    assert_eq!(attestation.attestation_digest, [0_u8; 32]);

    // 2. Attempting to claim verified booleans or non-zero digest on legacy minicbor fails closed.
    let comp_ack = CompleteOperationResultWire {
        replayed: false,
        outcome_code: OUTCOME_CODE_COMPLETED,
        receipt_id: [0x71; 16],
        state_code: STATE_CODE_COMPLETED,
    };
    let comp_ack_cbor = encode_complete_result(&comp_ack).expect("encode");
    let forged_attestation = ProviderIsolationAttestation {
        tenant_isolated: true,
        session_ephemeral: true,
        cache_partitioned: true,
        zero_data_retention: true,
        attestation_digest: [0xFE; 32],
        provider_profile: "forged-tee-enclave".to_owned(),
    };
    let err = shim
        .legacy_minicbor_response_with_claimed_attestation(
            COMPLETE_PROVIDER_METHOD,
            &comp_ack_cbor,
            None,
            &forged_attestation,
        )
        .expect_err("forged isolation elevation must fail closed");
    assert!(
        err.to_string()
            .contains("forbidden provider isolation elevation")
    );

    // 3. QueryIsolation down-conversion to legacy minicbor fails closed explicitly.
    let query_proto =
        nlos_compat_shim::driver_shim::encode_query_isolation_request_proto().expect("query proto");
    let down_err = shim
        .frozen_proto_request_to_legacy_minicbor(&query_proto)
        .expect_err("legacy minicbor has no query_isolation request");
    assert!(
        down_err
            .to_string()
            .contains(QUERY_ISOLATION_PROVIDER_METHOD)
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn driver_shim_bridges_historical_frozen_driver_provider_epochs_losslessly() {
    let honest_shim = DriverProtocolShim::new_historical_frozen_epoch(
        2,
        3,
        ProviderTrustCeiling::PreserveHonestAttestation,
    )
    .expect("2 <-> 3 frozen driver shim");

    let spec = OperationSpec {
        operation_id: OperationId::from_bytes([0xA1; 16]),
        generation: gen_u64(2),
        owner_fiber: FiberHandle {
            fiber_id: ExecutionFiberId::from_bytes([0xB2; 16]),
            generation: gen_u64(4),
        },
        cancellation_scope_id: CancellationScopeId::from_bytes([0xC3; 16]),
        cancellation_generation: gen_u64(6),
    };
    let mut v2_req =
        DriverProviderRequest::decode(encode_register_request_proto(&spec).expect("v1").as_slice())
            .expect("decode");
    v2_req.schema.as_mut().expect("schema").major = 2;
    v2_req
        .schema
        .as_mut()
        .expect("schema")
        .non_critical_extension_ids = vec![19, 23];

    let v3_req_bytes = honest_shim
        .translate_historical_proto_request_to_host(&v2_req.encode_to_vec())
        .expect("v2 -> v3 request");
    let mut v3_decoded = DriverProviderRequest::decode(v3_req_bytes.as_slice()).expect("decode v3");
    assert_eq!(v3_decoded.schema.as_ref().expect("schema").major, 3);
    assert_eq!(
        v3_decoded
            .schema
            .as_ref()
            .expect("schema")
            .non_critical_extension_ids,
        vec![19, 23]
    );
    v3_decoded.schema.as_mut().expect("schema").major = 1;
    v3_decoded
        .schema
        .as_mut()
        .expect("schema")
        .non_critical_extension_ids
        .clear();
    assert_eq!(
        decode_request_proto(&v3_decoded.encode_to_vec()).expect("domain spec"),
        DecodedProviderRequest::Register(spec)
    );

    // ForceUnverified ceiling clamps historical v2 CompleteAck claiming VERIFIED down to UNVERIFIED.
    let clamped_shim = DriverProtocolShim::new_historical_frozen_epoch(
        2,
        3,
        ProviderTrustCeiling::ForceUnverified,
    )
    .expect("clamped 2 <-> 3 shim");
    let verified_att = ProviderIsolationAttestation {
        tenant_isolated: true,
        session_ephemeral: true,
        cache_partitioned: true,
        zero_data_retention: true,
        attestation_digest: [0xAB; 32],
        provider_profile: "external-worker".to_owned(),
    };
    assert_eq!(
        verified_att.evaluate_posture().receipt_marker(),
        PROVIDER_ISOLATION_VERIFIED_TAG
    );
    let receipt_id = ReceiptId::from_bytes([0xD4; 16]);
    let v1_comp_ack = encode_complete_ack_proto(
        &CompleteProviderOutcome {
            replayed: false,
            outcome: CompletionOutcome::Completed { receipt_id },
            state: OperationState::Completed { receipt_id },
        },
        [0x55; 32],
        false,
        &verified_att,
    )
    .expect("encode complete ack");
    let mut v2_resp = DriverProviderResponse::decode(v1_comp_ack.as_slice()).expect("decode");
    v2_resp.schema.as_mut().expect("schema").major = 2;
    v2_resp
        .schema
        .as_mut()
        .expect("schema")
        .non_critical_extension_ids = vec![77];

    let (v3_clamped_bytes, posture) = clamped_shim
        .translate_historical_proto_response_to_host(&v2_resp.encode_to_vec())
        .expect("clamped v2 -> v3 response");
    assert_eq!(
        posture.expect("posture").receipt_marker(),
        PROVIDER_ISOLATION_UNVERIFIED_TAG
    );
    let v3_resp = DriverProviderResponse::decode(v3_clamped_bytes.as_slice()).expect("decode v3");
    assert_eq!(v3_resp.schema.as_ref().expect("schema").major, 3);
    assert_eq!(
        v3_resp
            .schema
            .as_ref()
            .expect("schema")
            .non_critical_extension_ids,
        vec![77]
    );
}
