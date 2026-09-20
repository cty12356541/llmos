use nlos_schema::sabi::v1::{
    AcknowledgeArtifactRecoveryAlertCommand, AcknowledgeSemanticRecoveryAlertCommand,
    ArtifactRecoveryAlertStatus, ArtifactRecoveryMetrics, ArtifactRecoveryOperationsSnapshot,
    BarrierObservationEvidence, BarrierObservationRecord, BarrierObservationSignature,
    BarrierObservationTarget, CallerIdentity, CancelCommand, CancelOperationRequest,
    CapabilityHandle, ControlCommand, ControlCommandLifecycleState, ControlCommandResult,
    ControlCommandSource, ControlScope, Envelope, ExchangeRequest, ExchangeResponse,
    GetSystemControlRequest, KillCommand, NegotiateServiceResponse, OperationLifecycleState,
    OperationReference, OperationStatus, PauseCommand, PrincipalHandshakeAttestation,
    PrincipalHandshakeChallenge, QueryOperationRequest, ReceiptReference, ReclaimCommand,
    RecoveryFailureAuthority, RecoveryFailureSummary, RecoveryWorkerLifecycleState,
    RegisterWaitRequest, ResolveServiceRequest, ResolveServiceResponse, ResumeCommand,
    ResumeSemanticRecoveryCommand, RetryDirective, SabiErrorCode, SabiFailure, SabiRequestContext,
    SabiResponseContext, SchemaIdentity, SemanticRecoveryAlertStatus, SemanticRecoveryMetrics,
    SemanticRecoveryOperationsSnapshot, SubmitBarrierObservationRequest,
    SubmitControlCommandRequest, SystemControlView, TaskExecutionBinding, ThrottleCommand,
    control_command, envelope as envelope_message, local_rpc,
};
use nlos_schema::{
    CommonSemanticsError, CompatibilityError, HANDSHAKE_NONCE_BYTES, HANDSHAKE_SIGNATURE_BYTES,
    MAX_ENVELOPE_BYTES, MAX_HANDSHAKE_CHANNEL_BINDING_BYTES, MAX_PRINCIPAL_HANDSHAKE_PAYLOAD_BYTES,
    MAX_SERVICE_DIRECTORY_PAYLOAD_BYTES, MAX_SYSTEM_CONTROL_ALERTS,
    MAX_TAKEOVER_CONTROL_PAYLOAD_BYTES, MAX_WAIT_CONTROL_PAYLOAD_BYTES, MethodSemantics,
    SABI_ENVELOPE_SCHEMA, SABI_OPERATION_CONTROL_SCHEMA, SABI_PRINCIPAL_HANDSHAKE_SCHEMA,
    SABI_SERVICE_DIRECTORY_SCHEMA, SABI_SYSTEM_CONTROL_SCHEMA, SABI_TAKEOVER_CONTROL_SCHEMA,
    SABI_WAIT_CONTROL_SCHEMA, decode_artifact_recovery_operations_snapshot,
    decode_barrier_observation_record, decode_cancel_operation_request,
    decode_control_command_result, decode_exchange_request, decode_exchange_response,
    decode_get_system_control_request, decode_operation_status,
    decode_principal_handshake_attestation, decode_principal_handshake_challenge,
    decode_query_operation_request, decode_register_wait_request, decode_resolve_service_request,
    decode_sabi_envelope, decode_semantic_recovery_operations_snapshot,
    decode_submit_barrier_observation_request, decode_submit_control_command_request,
    encode_artifact_recovery_operations_snapshot, encode_barrier_observation_record,
    encode_cancel_operation_request, encode_control_command_result, encode_exchange_request,
    encode_exchange_response, encode_get_system_control_request, encode_operation_status,
    encode_principal_handshake_attestation, encode_principal_handshake_challenge,
    encode_query_operation_request, encode_register_wait_request, encode_resolve_service_request,
    encode_resolve_service_response, encode_sabi_envelope,
    encode_semantic_recovery_operations_snapshot, encode_submit_barrier_observation_request,
    encode_submit_control_command_request, operation_control_schema_identity,
    principal_handshake_schema_identity, registry_frozen, schema_registry,
    service_directory_schema_identity, system_control_schema_identity,
    takeover_control_schema_identity, validate_sabi_request_context,
    validate_sabi_response_context, wait_control_schema_identity,
};
use prost::Message as _;

fn envelope() -> Envelope {
    Envelope {
        schema: Some(SchemaIdentity {
            name: SABI_ENVELOPE_SCHEMA.to_owned(),
            major: 1,
            minor: 0,
            critical_extension_ids: Vec::new(),
            non_critical_extension_ids: vec![42],
        }),
        request_id: (0_u8..16).collect(),
        service: "operation".to_owned(),
        method: "get".to_owned(),
        common_context: None,
        payload: b"abc".to_vec(),
    }
}

fn decode_hex(input: &str) -> Vec<u8> {
    let compact = input.trim();
    assert_eq!(compact.len() % 2, 0);
    (0..compact.len())
        .step_by(2)
        .map(|offset| u8::from_str_radix(&compact[offset..offset + 2], 16).unwrap())
        .collect()
}

fn common_request_envelope() -> Envelope {
    Envelope {
        schema: Some(SchemaIdentity {
            name: SABI_ENVELOPE_SCHEMA.to_owned(),
            major: 1,
            minor: 1,
            critical_extension_ids: Vec::new(),
            non_critical_extension_ids: Vec::new(),
        }),
        request_id: (0_u8..16).collect(),
        service: "operation".to_owned(),
        method: "cancel".to_owned(),
        common_context: Some(envelope_message::CommonContext::RequestContext(
            SabiRequestContext {
                caller: Some(CallerIdentity {
                    principal_id: vec![1; 16],
                    application_id: vec![2; 16],
                    process_id: vec![3; 16],
                    process_generation: 7,
                }),
                activity_context: b"trace".to_vec(),
                task_execution_binding: Some(TaskExecutionBinding {
                    task_attempt_id: vec![4; 16],
                    task_authority_term: 9,
                    task_control_epoch: 10,
                    cancel_epoch: 11,
                    permit_epoch: 12,
                    isolation_domain_generation: 13,
                }),
                correlation_id: vec![5; 16],
                idempotency_key: vec![6; 16],
                deadline_monotonic_ns: 123_456,
                capability_handles: vec![CapabilityHandle {
                    slot: 11,
                    generation: 2,
                }],
                reservation_handle: Some(CapabilityHandle {
                    slot: 12,
                    generation: 3,
                }),
                proposal_or_input_digest_sha256: vec![7; 32],
            },
        )),
        payload: b"abc".to_vec(),
    }
}

fn uncertain_response_envelope() -> Envelope {
    let operation = OperationReference {
        operation_id: vec![8; 16],
        generation: 4,
    };
    let receipt = ReceiptReference {
        receipt_id: vec![9; 16],
    };
    Envelope {
        schema: Some(SchemaIdentity {
            name: SABI_ENVELOPE_SCHEMA.to_owned(),
            major: 1,
            minor: 1,
            critical_extension_ids: Vec::new(),
            non_critical_extension_ids: Vec::new(),
        }),
        request_id: (0_u8..16).collect(),
        service: "operation".to_owned(),
        method: "cancel".to_owned(),
        common_context: Some(envelope_message::CommonContext::ResponseContext(
            SabiResponseContext {
                correlation_id: vec![5; 16],
                operation: Some(operation.clone()),
                receipts: vec![receipt.clone()],
                failure: Some(SabiFailure {
                    code: SabiErrorCode::Uncertain.into(),
                    retry: RetryDirective::QueryOperationOrRetrySameIdempotencyKey.into(),
                    safe_message: "outcome requires reconciliation".to_owned(),
                }),
            },
        )),
        payload: Vec::new(),
    }
}

#[test]
fn generated_encoding_matches_canonical_golden_vector() {
    let expected = decode_hex(include_str!(
        "../../../schema/golden/nlos.sabi.Envelope-v1.hex"
    ));
    let encoded = encode_sabi_envelope(&envelope()).unwrap();

    assert_eq!(encoded, expected);
    assert_eq!(
        decode_sabi_envelope(&expected).unwrap().envelope(),
        &envelope()
    );
}

#[test]
fn registry_exposes_the_supported_contract() {
    let registry = schema_registry();
    assert_eq!(registry.len(), 7);
    let envelope = registry
        .iter()
        .find(|entry| entry.name == SABI_ENVELOPE_SCHEMA)
        .unwrap();
    assert_eq!((envelope.major, envelope.minor), (1, 1));
    let directory = registry
        .iter()
        .find(|entry| entry.name == SABI_SERVICE_DIRECTORY_SCHEMA)
        .unwrap();
    assert_eq!((directory.major, directory.minor), (1, 0));
    let operation_control = registry
        .iter()
        .find(|entry| entry.name == SABI_OPERATION_CONTROL_SCHEMA)
        .unwrap();
    assert_eq!((operation_control.major, operation_control.minor), (1, 0));
    let system_control = registry
        .iter()
        .find(|entry| entry.name == SABI_SYSTEM_CONTROL_SCHEMA)
        .unwrap();
    // W27-A bumped the minor for the additive semantic-domain extension,
    // W28-D bumped it again for the additive operation-level command arms,
    // and W29-D bumped it once more for the additive kill/throttle/reclaim
    // arms; ADR-0014 permits additive extension of a frozen entry.
    assert_eq!(system_control.major, 1);
    assert_eq!(system_control.minor, 3);
    let takeover_control = registry
        .iter()
        .find(|entry| entry.name == SABI_TAKEOVER_CONTROL_SCHEMA)
        .unwrap();
    assert_eq!((takeover_control.major, takeover_control.minor), (1, 0));
    let wait_control = registry
        .iter()
        .find(|entry| entry.name == SABI_WAIT_CONTROL_SCHEMA)
        .unwrap();
    assert_eq!((wait_control.major, wait_control.minor), (1, 0));
    let principal_handshake = registry
        .iter()
        .find(|entry| entry.name == SABI_PRINCIPAL_HANDSHAKE_SCHEMA)
        .unwrap();
    assert_eq!(
        (principal_handshake.major, principal_handshake.minor),
        (1, 0)
    );
}

#[test]
fn registry_freeze_markers_match_adr_0014() {
    let frozen_names = [
        SABI_ENVELOPE_SCHEMA,
        SABI_SERVICE_DIRECTORY_SCHEMA,
        SABI_OPERATION_CONTROL_SCHEMA,
        SABI_SYSTEM_CONTROL_SCHEMA,
        SABI_TAKEOVER_CONTROL_SCHEMA,
        SABI_WAIT_CONTROL_SCHEMA,
    ];
    for name in frozen_names {
        assert_eq!(registry_frozen(name), Some(true), "{name} must be frozen");
    }
    assert_eq!(
        registry_frozen(SABI_PRINCIPAL_HANDSHAKE_SCHEMA),
        Some(false),
        "the additive PrincipalHandshake entry stays open under the freeze"
    );
    assert_eq!(registry_frozen("nlos.sabi.Unknown"), None);

    // The descriptor metadata and the query API must agree on every entry.
    for descriptor in schema_registry() {
        assert_eq!(
            registry_frozen(descriptor.name),
            Some(descriptor.frozen),
            "{} marker disagreement between registry and query API",
            descriptor.name
        );
    }
}

#[test]
fn wait_control_payloads_are_bounded_and_registry_validated() {
    let request = RegisterWaitRequest {
        schema: Some(wait_control_schema_identity()),
        binding: vec![0x11; 16],
        channel_id: vec![0x12; 16],
        target_sequence: 5,
        idempotency_key: vec![0x13; 16],
        registered_at_ms: 1_000,
    };
    let wire = encode_register_wait_request(&request).unwrap();
    assert_eq!(decode_register_wait_request(&wire).unwrap(), request);

    // A payload may legally omit its optional schema field on the wire, so
    // the unbound frame is produced with raw prost encoding, past the local
    // validation; the shared decoder must then fail closed on the missing
    // identity...
    let unbound_wire = RegisterWaitRequest {
        schema: None,
        ..request.clone()
    }
    .encode_to_vec();
    assert!(matches!(
        decode_register_wait_request(&unbound_wire),
        Err(CompatibilityError::MissingSchemaIdentity)
    ));

    // ...and on an identity outside the registered wait-control contract.
    let mut foreign_identity = wait_control_schema_identity();
    foreign_identity.name = SABI_OPERATION_CONTROL_SCHEMA.to_owned();
    let foreign_wire = RegisterWaitRequest {
        schema: Some(foreign_identity),
        ..request.clone()
    }
    .encode_to_vec();
    assert!(matches!(
        decode_register_wait_request(&foreign_wire),
        Err(CompatibilityError::UnknownSchema(name)) if name == SABI_OPERATION_CONTROL_SCHEMA
    ));

    // The shared 64 KiB payload bound is enforced on encode.
    let oversized = RegisterWaitRequest {
        binding: vec![0; MAX_WAIT_CONTROL_PAYLOAD_BYTES + 1],
        ..request
    };
    assert!(matches!(
        encode_register_wait_request(&oversized),
        Err(CompatibilityError::FrameTooLarge { .. })
    ));
}

#[test]
fn generated_encoding_matches_principal_handshake_golden_vector() {
    let expected = decode_hex(include_str!(
        "../../../schema/golden/nlos.sabi.PrincipalHandshake-v1.hex"
    ));
    let encoded = encode_principal_handshake_attestation(&attestation()).unwrap();

    assert_eq!(encoded, expected);
    assert_eq!(
        decode_principal_handshake_attestation(&expected).unwrap(),
        attestation()
    );
}

/// Fixed deterministic bytes for one `SemanticRecoveryOperationsSnapshot`,
/// byte-identical to the TypeScript and Python conformance goldens
/// (B-SCHEMA-014-METRICS-GOLDEN pattern extended by W27-A). The schema
/// identity stays pinned at the v1.1 minor of the W27-A freeze point —
/// like the TS/Python fixtures, which hardcode the same literal identity —
/// so the frozen golden bytes do not drift when the registry minor advances.
const SEMANTIC_RECOVERY_SNAPSHOT_GOLDEN_HEX: &str = concat!(
    "0a1d0a176e6c6f732e736162692e53797374656d436f6e74726f6c1001",
    "18011210080d10061802200428033001380940011a330a107171717171",
    "71717171717171717171711008180520e80728f80a30dc0b3a120a1072",
    "7272727272727272727272727272721a1f0a1073737373737373737373",
    "7373737373731008180320d00f28e01230c4132001",
);

/// The literal v1.1 `SystemControl` identity pinned by the W27-A goldens
/// (the registry minor has since advanced to 3 through the W28-D and W29-D
/// additive command arms; frozen goldens stay pinned at their creation
/// minor).
fn w27a_semantic_identity() -> SchemaIdentity {
    SchemaIdentity {
        name: SABI_SYSTEM_CONTROL_SCHEMA.to_owned(),
        major: 1,
        minor: 1,
        critical_extension_ids: Vec::new(),
        non_critical_extension_ids: Vec::new(),
    }
}

fn semantic_recovery_snapshot() -> SemanticRecoveryOperationsSnapshot {
    SemanticRecoveryOperationsSnapshot {
        schema: Some(w27a_semantic_identity()),
        metrics: Some(SemanticRecoveryMetrics {
            total_inspected: 13,
            total_finalized: 6,
            consecutive_failed_cycles: 2,
            durable_retrying: 4,
            durable_escalated: 3,
            durable_unacknowledged_escalated: 1,
            durable_resolved: 9,
            domain_faulted: true,
        }),
        alerts: vec![
            SemanticRecoveryAlertStatus {
                plan_id: vec![0x71; 16],
                total_failures: 8,
                last_failure_authority: RecoveryFailureAuthority::Semantic.into(),
                first_failed_at_ms: 1_000,
                last_failed_at_ms: 1_400,
                escalated_at_ms: 1_500,
                acknowledgement_receipt: Some(ReceiptReference {
                    receipt_id: vec![0x72; 16],
                }),
            },
            SemanticRecoveryAlertStatus {
                plan_id: vec![0x73; 16],
                total_failures: 8,
                last_failure_authority: RecoveryFailureAuthority::Coordinator.into(),
                first_failed_at_ms: 2_000,
                last_failed_at_ms: 2_400,
                escalated_at_ms: 2_500,
                acknowledgement_receipt: None,
            },
        ],
        alerts_truncated: true,
    }
}

#[test]
fn semantic_recovery_snapshot_pins_the_cross_language_golden_bytes() {
    let golden = decode_hex(SEMANTIC_RECOVERY_SNAPSHOT_GOLDEN_HEX);
    let encoded = encode_semantic_recovery_operations_snapshot(&semantic_recovery_snapshot())
        .expect("valid semantic snapshot encodes");
    assert_eq!(encoded, golden);
    assert_eq!(
        decode_semantic_recovery_operations_snapshot(&golden).unwrap(),
        semantic_recovery_snapshot()
    );
    assert_eq!(
        encode_semantic_recovery_operations_snapshot(
            &decode_semantic_recovery_operations_snapshot(&golden).unwrap()
        )
        .unwrap(),
        golden
    );
}

#[test]
fn semantic_recovery_control_payloads_are_typed_bounded_and_fail_closed() {
    let get = GetSystemControlRequest {
        schema: Some(system_control_schema_identity()),
        view: SystemControlView::SemanticCommitRecovery.into(),
        alert_limit: 8,
    };
    let get_wire = encode_get_system_control_request(&get).unwrap();
    assert_eq!(decode_get_system_control_request(&get_wire).unwrap(), get);

    let acknowledge = SubmitControlCommandRequest {
        schema: Some(system_control_schema_identity()),
        command: Some(ControlCommand {
            control_command_id: vec![0x51; 16],
            issuer_principal_id: vec![0x32; 16],
            source: ControlCommandSource::Cli.into(),
            scope: ControlScope::Operation.into(),
            target_id: vec![0x71; 16],
            expected_generation_or_revision: 8,
            command: Some(control_command::Command::AcknowledgeSemanticRecoveryAlert(
                AcknowledgeSemanticRecoveryAlertCommand {},
            )),
            reason: "operator inspected durable semantic recovery state".to_owned(),
        }),
    };
    let acknowledge_wire = encode_submit_control_command_request(&acknowledge).unwrap();
    assert_eq!(
        decode_submit_control_command_request(&acknowledge_wire).unwrap(),
        acknowledge
    );

    let resume = SubmitControlCommandRequest {
        schema: Some(system_control_schema_identity()),
        command: Some(ControlCommand {
            control_command_id: vec![0x52; 16],
            issuer_principal_id: vec![0x32; 16],
            source: ControlCommandSource::Cli.into(),
            scope: ControlScope::Operation.into(),
            target_id: vec![0x71; 16],
            expected_generation_or_revision: 8,
            command: Some(control_command::Command::ResumeSemanticRecovery(
                ResumeSemanticRecoveryCommand {},
            )),
            reason: "operator resumes the escalated semantic plan".to_owned(),
        }),
    };
    let resume_wire = encode_submit_control_command_request(&resume).unwrap();
    assert_eq!(
        decode_submit_control_command_request(&resume_wire).unwrap(),
        resume
    );

    let mut unbounded = semantic_recovery_snapshot();
    unbounded.alerts = vec![unbounded.alerts[0].clone(); MAX_SYSTEM_CONTROL_ALERTS + 1];
    assert_eq!(
        encode_semantic_recovery_operations_snapshot(&unbounded),
        Err(CompatibilityError::TooManySystemControlAlerts)
    );

    let mut malformed_receipt = semantic_recovery_snapshot();
    malformed_receipt.alerts[0].acknowledgement_receipt = Some(ReceiptReference {
        receipt_id: vec![0x72; 15],
    });
    assert_eq!(
        encode_semantic_recovery_operations_snapshot(&malformed_receipt),
        Err(CompatibilityError::InvalidReceiptReference)
    );

    let mut unspecified_authority = semantic_recovery_snapshot();
    unspecified_authority.alerts[1].last_failure_authority =
        RecoveryFailureAuthority::Unspecified.into();
    assert_eq!(
        encode_semantic_recovery_operations_snapshot(&unspecified_authority),
        Err(CompatibilityError::InvalidSystemControlAlert)
    );

    let mut missing_metrics = semantic_recovery_snapshot();
    missing_metrics.metrics = None;
    assert_eq!(
        encode_semantic_recovery_operations_snapshot(&missing_metrics),
        Err(CompatibilityError::MissingSystemControlMetrics)
    );
}

/// One W28-D operation-level `ControlCommand` submit request for the given
/// oneof arm, addressing a 16-byte operational target under CAS expectation 5.
fn operation_level_submit(
    arm: control_command::Command,
    control_command_id: Vec<u8>,
) -> SubmitControlCommandRequest {
    SubmitControlCommandRequest {
        schema: Some(w28d_operation_identity()),
        command: Some(ControlCommand {
            control_command_id,
            issuer_principal_id: vec![0x32; 16],
            source: ControlCommandSource::Cli.into(),
            scope: ControlScope::Operation.into(),
            target_id: vec![0x81; 16],
            expected_generation_or_revision: 5,
            command: Some(arm),
            reason: "operator pauses the escalated operation".to_owned(),
        }),
    }
}

/// The literal v1.2 `SystemControl` identity of the W28-D operation-level
/// freeze point (the registry minor advanced to 3 with the W29-D additive
/// arms; frozen goldens stay pinned at their creation minor, mirroring
/// [`w27a_semantic_identity`]).
fn w28d_operation_identity() -> SchemaIdentity {
    SchemaIdentity {
        name: SABI_SYSTEM_CONTROL_SCHEMA.to_owned(),
        major: 1,
        minor: 2,
        critical_extension_ids: Vec::new(),
        non_critical_extension_ids: Vec::new(),
    }
}

/// W28-D additive command arms compile, round-trip, and pin their
/// deterministic wire bytes (Rust-side golden vectors).
/// Shared wire prefix of the three W28-D arm fixtures: the v1.2 schema
/// identity and the `ControlCommand` addressing fields up to the CAS
/// expectation. The pinned bytes are prost field order — the oneof arm
/// precedes `reason` — which is the encoding this build puts on the wire.
const OPERATION_LEVEL_SUBMIT_PREFIX_HEX: &str = concat!(
    "0a1d0a176e6c6f732e736162692e53797374656d436f6e74726f6c1001180212",
    "670a106161616161616161616161616161616112103232323232323232323232",
    "3232323232180320022a10818181818181818181818181818181813005",
);
const OPERATION_LEVEL_SUBMIT_REASON_HEX: &str = "42276f70657261746f72207061757365732074686520657363616c6174656420\
     6f7065726174696f6e";

#[test]
fn operation_level_control_payloads_round_trip_and_pin_golden_bytes() {
    for (label, arm, arm_field_hex) in [
        (
            "pause",
            control_command::Command::PauseOperation(PauseCommand {}),
            "5a00",
        ),
        (
            "resume",
            control_command::Command::ResumeOperation(ResumeCommand {}),
            "6200",
        ),
        (
            "cancel",
            control_command::Command::CancelOperation(CancelCommand {}),
            "6a00",
        ),
    ] {
        let request = operation_level_submit(arm, vec![0x61; 16]);
        let wire = encode_submit_control_command_request(&request)
            .unwrap_or_else(|error| panic!("{label} arm must encode: {error}"));
        assert_eq!(
            decode_submit_control_command_request(&wire).unwrap(),
            request,
            "{label} arm must round-trip"
        );
        let golden = format!(
            "{OPERATION_LEVEL_SUBMIT_PREFIX_HEX}{arm_field_hex}\
             {OPERATION_LEVEL_SUBMIT_REASON_HEX}"
        );
        assert_eq!(wire, decode_hex(&golden), "{label} arm golden bytes");
    }

    // The shared per-command bounds apply to the new arms unchanged.
    let mut unsafe_reason = operation_level_submit(
        control_command::Command::PauseOperation(PauseCommand {}),
        vec![0x61; 16],
    );
    unsafe_reason.command.as_mut().unwrap().reason.push('\0');
    assert_eq!(
        encode_submit_control_command_request(&unsafe_reason),
        Err(CompatibilityError::UnsafeControlReason)
    );
    let mut short_target = operation_level_submit(
        control_command::Command::CancelOperation(CancelCommand {}),
        vec![0x61; 16],
    );
    short_target.command.as_mut().unwrap().target_id.pop();
    assert_eq!(
        encode_submit_control_command_request(&short_target),
        Err(CompatibilityError::InvalidSystemControlIdentifier)
    );
}

/// W29-D additive command arms compile, round-trip, pin their deterministic
/// wire bytes, and enforce the `throttle_percent` whole-percent bound
/// (Rust-side golden vectors; prost field order, oneof arm before `reason`).
#[test]
fn kill_throttle_reclaim_arms_round_trip_and_pin_their_bounds() {
    // The same addressing shape as the W28-D prefix, with the schema
    // identity advanced to the v1.3 W29-D additive minor. The command
    // length prefix varies per arm (the throttle arm carries 4 payload
    // bytes against the empty arms' 2), so it stays per-entry.
    const OPERATION_LEVEL_SUBMIT_HEAD_V1_3_HEX: &str =
        "0a1d0a176e6c6f732e736162692e53797374656d436f6e74726f6c1001180312";
    const OPERATION_LEVEL_SUBMIT_BODY_V1_3_HEX: &str = concat!(
        "0a106161616161616161616161616161616112103232323232323232323232",
        "3232323232180320022a10818181818181818181818181818181813005",
    );
    fn w29d_submit(arm: control_command::Command) -> SubmitControlCommandRequest {
        SubmitControlCommandRequest {
            schema: Some(system_control_schema_identity()),
            command: Some(ControlCommand {
                control_command_id: vec![0x61; 16],
                issuer_principal_id: vec![0x32; 16],
                source: ControlCommandSource::Cli.into(),
                scope: ControlScope::Operation.into(),
                target_id: vec![0x81; 16],
                expected_generation_or_revision: 5,
                command: Some(arm),
                reason: "operator pauses the escalated operation".to_owned(),
            }),
        }
    }
    for (label, arm, command_len_hex, arm_field_hex) in [
        (
            "kill",
            control_command::Command::KillOperation(KillCommand {}),
            "67",
            "7200",
        ),
        (
            "throttle",
            control_command::Command::ThrottleOperation(ThrottleCommand {
                throttle_percent: 50,
            }),
            "69",
            "7a020832",
        ),
        (
            "reclaim",
            control_command::Command::ReclaimOperation(ReclaimCommand {}),
            "68",
            "820100",
        ),
    ] {
        let request = w29d_submit(arm);
        let wire = encode_submit_control_command_request(&request)
            .unwrap_or_else(|error| panic!("{label} arm must encode: {error}"));
        assert_eq!(
            decode_submit_control_command_request(&wire).unwrap(),
            request,
            "{label} arm must round-trip"
        );
        let golden = format!(
            "{OPERATION_LEVEL_SUBMIT_HEAD_V1_3_HEX}{command_len_hex}\
             {OPERATION_LEVEL_SUBMIT_BODY_V1_3_HEX}{arm_field_hex}\
             {OPERATION_LEVEL_SUBMIT_REASON_HEX}"
        );
        assert_eq!(wire, decode_hex(&golden), "{label} arm golden bytes");
    }

    for out_of_range in [0_u64, 101] {
        let request = w29d_submit(control_command::Command::ThrottleOperation(
            ThrottleCommand {
                throttle_percent: out_of_range,
            },
        ));
        assert_eq!(
            encode_submit_control_command_request(&request),
            Err(CompatibilityError::InvalidSystemControlIdentifier),
            "throttle percent {out_of_range} must fail closed before the wire"
        );
    }
}

fn attestation() -> PrincipalHandshakeAttestation {
    PrincipalHandshakeAttestation {
        schema: Some(principal_handshake_schema_identity()),
        principal_id: (0_u8..16).collect(),
        nonce: vec![0xA5; HANDSHAKE_NONCE_BYTES],
        channel_binding: b"unix:///tmp/nlos-handshake.sock".to_vec(),
        signature: vec![0xCD; HANDSHAKE_SIGNATURE_BYTES],
    }
}

#[test]
fn principal_handshake_payloads_are_bounded_and_registry_validated() {
    let challenge = PrincipalHandshakeChallenge {
        schema: Some(principal_handshake_schema_identity()),
        nonce: vec![0x5A; HANDSHAKE_NONCE_BYTES],
    };
    let challenge_wire = encode_principal_handshake_challenge(&challenge).unwrap();
    assert_eq!(
        decode_principal_handshake_challenge(&challenge_wire).unwrap(),
        challenge
    );

    let attestation_wire = encode_principal_handshake_attestation(&attestation()).unwrap();
    assert!(attestation_wire.len() <= MAX_PRINCIPAL_HANDSHAKE_PAYLOAD_BYTES);
    assert_eq!(
        decode_principal_handshake_attestation(&attestation_wire).unwrap(),
        attestation()
    );

    // Optional schema fields are legal on the raw wire, so the unbound frame
    // is produced with prost directly; the shared decoders must fail closed.
    let unbound_challenge = PrincipalHandshakeChallenge {
        schema: None,
        ..challenge.clone()
    }
    .encode_to_vec();
    assert!(matches!(
        decode_principal_handshake_challenge(&unbound_challenge),
        Err(CompatibilityError::MissingSchemaIdentity)
    ));
    let unbound_attestation = PrincipalHandshakeAttestation {
        schema: None,
        ..attestation()
    }
    .encode_to_vec();
    assert!(matches!(
        decode_principal_handshake_attestation(&unbound_attestation),
        Err(CompatibilityError::MissingSchemaIdentity)
    ));

    // Foreign identities fail closed on both messages.
    let mut foreign = principal_handshake_schema_identity();
    foreign.name = SABI_WAIT_CONTROL_SCHEMA.to_owned();
    assert!(matches!(
        decode_principal_handshake_challenge(
            &PrincipalHandshakeChallenge {
                schema: Some(foreign.clone()),
                nonce: vec![0x5A; HANDSHAKE_NONCE_BYTES],
            }
            .encode_to_vec()
        ),
        Err(CompatibilityError::UnknownSchema(name))
            if name == SABI_WAIT_CONTROL_SCHEMA
    ));
    assert!(matches!(
        decode_principal_handshake_attestation(
            &PrincipalHandshakeAttestation {
                schema: Some(foreign),
                ..attestation()
            }
            .encode_to_vec()
        ),
        Err(CompatibilityError::UnknownSchema(name))
            if name == SABI_WAIT_CONTROL_SCHEMA
    ));

    // Malformed nonces, principals, signatures, and channel bindings fail closed.
    for malformed in [
        PrincipalHandshakeAttestation {
            nonce: vec![0xA5; HANDSHAKE_NONCE_BYTES - 1],
            ..attestation()
        },
        PrincipalHandshakeAttestation {
            principal_id: vec![1; 15],
            ..attestation()
        },
        PrincipalHandshakeAttestation {
            signature: vec![0xCD; HANDSHAKE_SIGNATURE_BYTES + 1],
            ..attestation()
        },
        PrincipalHandshakeAttestation {
            channel_binding: Vec::new(),
            ..attestation()
        },
        PrincipalHandshakeAttestation {
            channel_binding: vec![0; MAX_HANDSHAKE_CHANNEL_BINDING_BYTES + 1],
            ..attestation()
        },
    ] {
        assert!(matches!(
            encode_principal_handshake_attestation(&malformed),
            Err(CompatibilityError::InvalidHandshakeNonceLength { .. }
                | CompatibilityError::InvalidHandshakePrincipalIdLength { .. }
                | CompatibilityError::InvalidHandshakeSignatureLength { .. }
                | CompatibilityError::InvalidHandshakeChannelBindingLength { .. },)
        ));
    }
    assert!(matches!(
        decode_principal_handshake_challenge(
            &PrincipalHandshakeChallenge {
                nonce: vec![0x5A; HANDSHAKE_NONCE_BYTES + 1],
                ..challenge
            }
            .encode_to_vec()
        ),
        Err(CompatibilityError::InvalidHandshakeNonceLength { .. })
    ));
}

#[test]
fn system_control_payloads_are_typed_bounded_and_sanitized() {
    let get = GetSystemControlRequest {
        schema: Some(system_control_schema_identity()),
        view: SystemControlView::ArtifactCommitRecovery.into(),
        alert_limit: 8,
    };
    let get_wire = encode_get_system_control_request(&get).unwrap();
    assert_eq!(decode_get_system_control_request(&get_wire).unwrap(), get);

    let snapshot = ArtifactRecoveryOperationsSnapshot {
        schema: Some(system_control_schema_identity()),
        metrics: Some(ArtifactRecoveryMetrics {
            worker_state: RecoveryWorkerLifecycleState::BackingOff.into(),
            completed_cycles: 9,
            total_inspected: 10,
            total_finalized: 7,
            consecutive_failed_cycles: 0,
            retry_delay_ms: Some(250),
            durable_retrying: 1,
            durable_escalated: 1,
            durable_unacknowledged_escalated: 1,
            durable_resolved: 3,
            domain_faulted: false,
            last_failures: vec![RecoveryFailureSummary {
                plan_id: vec![0x21; 16],
                authority: RecoveryFailureAuthority::Artifact.into(),
            }],
        }),
        alerts: vec![ArtifactRecoveryAlertStatus {
            plan_id: vec![0x21; 16],
            total_failures: 3,
            last_failure_authority: RecoveryFailureAuthority::Artifact.into(),
            first_failed_at_ms: 1_000,
            last_failed_at_ms: 2_000,
            escalated_at_ms: 2_000,
            acknowledgement_receipt: None,
        }],
        alerts_truncated: false,
    };
    let snapshot_wire = encode_artifact_recovery_operations_snapshot(&snapshot).unwrap();
    assert_eq!(
        decode_artifact_recovery_operations_snapshot(&snapshot_wire).unwrap(),
        snapshot
    );

    let submit = SubmitControlCommandRequest {
        schema: Some(system_control_schema_identity()),
        command: Some(ControlCommand {
            control_command_id: vec![0x31; 16],
            issuer_principal_id: vec![0x32; 16],
            source: ControlCommandSource::Cli.into(),
            scope: ControlScope::Operation.into(),
            target_id: vec![0x21; 16],
            expected_generation_or_revision: 3,
            command: Some(control_command::Command::AcknowledgeArtifactRecoveryAlert(
                AcknowledgeArtifactRecoveryAlertCommand {},
            )),
            reason: "operator inspected durable recovery state".to_owned(),
        }),
    };
    let submit_wire = encode_submit_control_command_request(&submit).unwrap();
    assert_eq!(
        decode_submit_control_command_request(&submit_wire).unwrap(),
        submit
    );

    let result = ControlCommandResult {
        schema: Some(system_control_schema_identity()),
        control_command_id: vec![0x31; 16],
        state: ControlCommandLifecycleState::Completed.into(),
        receipt: Some(ReceiptReference {
            receipt_id: vec![0x41; 16],
        }),
    };
    let result_wire = encode_control_command_result(&result).unwrap();
    assert_eq!(decode_control_command_result(&result_wire).unwrap(), result);

    let mut unbounded = snapshot;
    unbounded.alerts = vec![unbounded.alerts[0].clone(); MAX_SYSTEM_CONTROL_ALERTS + 1];
    assert_eq!(
        encode_artifact_recovery_operations_snapshot(&unbounded),
        Err(CompatibilityError::TooManySystemControlAlerts)
    );
    let mut unsafe_submit = submit;
    unsafe_submit.command.as_mut().unwrap().reason.push('\0');
    assert_eq!(
        encode_submit_control_command_request(&unsafe_submit),
        Err(CompatibilityError::UnsafeControlReason)
    );
}

#[test]
fn operation_control_payloads_are_bounded_typed_and_fail_closed() {
    let operation = OperationReference {
        operation_id: vec![0x31; 16],
        generation: 2,
    };
    let query = QueryOperationRequest {
        schema: Some(operation_control_schema_identity()),
        operation: Some(operation.clone()),
    };
    let query_wire = encode_query_operation_request(&query).unwrap();
    assert_eq!(decode_query_operation_request(&query_wire).unwrap(), query);

    let cancel = CancelOperationRequest {
        schema: Some(operation_control_schema_identity()),
        operation: Some(operation.clone()),
        expected_cancel_epoch: 7,
    };
    let cancel_wire = encode_cancel_operation_request(&cancel).unwrap();
    assert_eq!(
        decode_cancel_operation_request(&cancel_wire).unwrap(),
        cancel
    );

    let status = OperationStatus {
        schema: Some(operation_control_schema_identity()),
        operation: Some(operation),
        state: OperationLifecycleState::CancelRequested.into(),
        cancel_epoch: 8,
        receipt: None,
    };
    let status_wire = encode_operation_status(&status).unwrap();
    assert_eq!(decode_operation_status(&status_wire).unwrap(), status);

    let mut missing_operation = query;
    missing_operation.operation = None;
    assert_eq!(
        encode_query_operation_request(&missing_operation),
        Err(CompatibilityError::MissingOperationReference)
    );
}

#[test]
fn takeover_control_payloads_round_trip_typed_and_bounded() {
    let submit = submit_barrier_observation_request();
    let submit_wire = encode_submit_barrier_observation_request(&submit).unwrap();
    assert_eq!(
        decode_submit_barrier_observation_request(&submit_wire).unwrap(),
        submit
    );

    let record = barrier_observation_record();
    let record_wire = encode_barrier_observation_record(&record).unwrap();
    assert_eq!(
        decode_barrier_observation_record(&record_wire).unwrap(),
        record
    );

    assert!(matches!(
        decode_submit_barrier_observation_request(&vec![
            0_u8;
            MAX_TAKEOVER_CONTROL_PAYLOAD_BYTES + 1
        ]),
        Err(CompatibilityError::FrameTooLarge { .. })
    ));
    assert!(matches!(
        decode_barrier_observation_record(&vec![0_u8; MAX_TAKEOVER_CONTROL_PAYLOAD_BYTES + 1]),
        Err(CompatibilityError::FrameTooLarge { .. })
    ));
}

fn submit_barrier_observation_request() -> SubmitBarrierObservationRequest {
    SubmitBarrierObservationRequest {
        schema: Some(takeover_control_schema_identity()),
        target: Some(BarrierObservationTarget {
            takeover_receipt_id: vec![0x51; 16],
            participant_type: 7,
            participant_id: vec![0x52; 16],
            participant_generation: 3,
            admission_receipt_id: vec![0x53; 16],
        }),
        evidence: Some(BarrierObservationEvidence {
            remote_receipt_id: vec![0x54; 16],
            barrier_digest: vec![0x55; 32],
            observed_at_ms: 1_000,
        }),
        signature: Some(BarrierObservationSignature {
            signer_principal_id: vec![0x56; 16],
            signer_control_domain_id: vec![0x57; 16],
            signer_key_id: vec![0x58; 16],
            signature: vec![0x59; 64],
        }),
    }
}

fn barrier_observation_record() -> BarrierObservationRecord {
    BarrierObservationRecord {
        schema: Some(takeover_control_schema_identity()),
        receipt_id: vec![0x61; 16],
        participant_type: 7,
        participant_id: vec![0x52; 16],
        barrier_digest: vec![0x55; 32],
        observed_at_ms: 1_000,
        signed: true,
        signer_principal_id: vec![0x56; 16],
        signer_key_id: vec![0x58; 16],
        signer_key_generation: 2,
    }
}

#[test]
fn takeover_control_validators_fail_closed_on_bad_bindings() {
    let submit = submit_barrier_observation_request();

    let mut missing_target = submit.clone();
    missing_target.target = None;
    assert_eq!(
        encode_submit_barrier_observation_request(&missing_target),
        Err(CompatibilityError::MissingTakeoverControlTarget)
    );
    let mut missing_evidence = submit.clone();
    missing_evidence.evidence = None;
    assert_eq!(
        encode_submit_barrier_observation_request(&missing_evidence),
        Err(CompatibilityError::MissingTakeoverControlEvidence)
    );
    let mut missing_signature = submit.clone();
    missing_signature.signature = None;
    assert_eq!(
        encode_submit_barrier_observation_request(&missing_signature),
        Err(CompatibilityError::MissingTakeoverControlSignature)
    );

    for invalid_type in [0_u32, 9] {
        let mut bad_type = submit.clone();
        bad_type.target.as_mut().unwrap().participant_type = invalid_type;
        assert_eq!(
            encode_submit_barrier_observation_request(&bad_type),
            Err(CompatibilityError::UnspecifiedTakeoverControlParticipantType)
        );
    }
    let mut short_id = submit.clone();
    short_id.target.as_mut().unwrap().participant_id.pop();
    assert_eq!(
        encode_submit_barrier_observation_request(&short_id),
        Err(CompatibilityError::InvalidTakeoverControlIdentifier)
    );
    let mut short_digest = submit.clone();
    short_digest.evidence.as_mut().unwrap().barrier_digest.pop();
    assert_eq!(
        encode_submit_barrier_observation_request(&short_digest),
        Err(CompatibilityError::InvalidTakeoverControlIdentifier)
    );
    let mut short_signature = submit.clone();
    short_signature.signature.as_mut().unwrap().signature.pop();
    assert_eq!(
        encode_submit_barrier_observation_request(&short_signature),
        Err(CompatibilityError::InvalidTakeoverControlIdentifier)
    );
    let mut zero_generation = submit.clone();
    zero_generation
        .target
        .as_mut()
        .unwrap()
        .participant_generation = 0;
    assert_eq!(
        encode_submit_barrier_observation_request(&zero_generation),
        Err(CompatibilityError::InvalidTakeoverControlGeneration)
    );
    let mut negative_timestamp = submit.clone();
    negative_timestamp.evidence.as_mut().unwrap().observed_at_ms = -1;
    assert_eq!(
        encode_submit_barrier_observation_request(&negative_timestamp),
        Err(CompatibilityError::InvalidTakeoverControlTimestamp)
    );

    let mut unsigned_record = barrier_observation_record();
    unsigned_record.signed = false;
    assert_eq!(
        encode_barrier_observation_record(&unsigned_record),
        Err(CompatibilityError::UnsignedTakeoverControlRecord)
    );
    let mut wrong_schema = barrier_observation_record();
    wrong_schema.schema = Some(system_control_schema_identity());
    assert!(matches!(
        encode_barrier_observation_record(&wrong_schema),
        Err(CompatibilityError::UnknownSchema(_))
    ));
}

#[test]
fn common_request_context_matches_golden_and_method_contract() {
    let request = common_request_envelope();
    let expected = decode_hex(include_str!(
        "../../../schema/golden/nlos.sabi.Envelope-common-request-v1.hex"
    ));
    let encoded = encode_sabi_envelope(&request).unwrap();
    assert_eq!(encoded, expected);
    let decoded = decode_sabi_envelope(&encoded).unwrap();
    assert_eq!(decoded.envelope(), &request);
    validate_sabi_request_context(
        decoded.envelope(),
        MethodSemantics::LONG_RUNNING_MUTATION,
        123_455,
    )
    .unwrap();
}

#[test]
fn mutations_deadlines_and_authority_references_fail_closed() {
    let mut request = common_request_envelope();
    let context = match request.common_context.as_mut().unwrap() {
        envelope_message::CommonContext::RequestContext(context) => context,
        envelope_message::CommonContext::ResponseContext(_) => unreachable!(),
    };
    context.idempotency_key.clear();
    assert_eq!(
        validate_sabi_request_context(&request, MethodSemantics::MUTATION, 0),
        Err(CommonSemanticsError::MissingIdempotencyKey)
    );

    let mut request = common_request_envelope();
    let context = match request.common_context.as_mut().unwrap() {
        envelope_message::CommonContext::RequestContext(context) => context,
        envelope_message::CommonContext::ResponseContext(_) => unreachable!(),
    };
    context.deadline_monotonic_ns = 0;
    assert_eq!(
        validate_sabi_request_context(&request, MethodSemantics::LONG_RUNNING_QUERY, 0),
        Err(CommonSemanticsError::MissingDeadline)
    );

    let mut request = common_request_envelope();
    let context = match request.common_context.as_mut().unwrap() {
        envelope_message::CommonContext::RequestContext(context) => context,
        envelope_message::CommonContext::ResponseContext(_) => unreachable!(),
    };
    context
        .capability_handles
        .push(context.capability_handles[0]);
    assert_eq!(
        validate_sabi_request_context(&request, MethodSemantics::QUERY, 0),
        Err(CommonSemanticsError::DuplicateCapabilityHandle)
    );
}

#[test]
fn uncertain_and_partial_failures_require_reconciliation_evidence() {
    let response = uncertain_response_envelope();
    let expected = decode_hex(include_str!(
        "../../../schema/golden/nlos.sabi.Envelope-common-uncertain-v1.hex"
    ));
    assert_eq!(encode_sabi_envelope(&response).unwrap(), expected);
    validate_sabi_response_context(&response, MethodSemantics::MUTATION).unwrap();

    let mut missing_effect_evidence = response.clone();
    let context = match missing_effect_evidence.common_context.as_mut().unwrap() {
        envelope_message::CommonContext::ResponseContext(context) => context,
        envelope_message::CommonContext::RequestContext(_) => unreachable!(),
    };
    context.operation = None;
    context.receipts.clear();
    context.failure = None;
    assert_eq!(
        validate_sabi_response_context(&missing_effect_evidence, MethodSemantics::MUTATION,),
        Err(CommonSemanticsError::MissingEffectEvidence)
    );

    let mut terminal_rejection = response.clone();
    let context = match terminal_rejection.common_context.as_mut().unwrap() {
        envelope_message::CommonContext::ResponseContext(context) => context,
        envelope_message::CommonContext::RequestContext(_) => unreachable!(),
    };
    context.operation = None;
    context.receipts.clear();
    context.failure = Some(SabiFailure {
        code: SabiErrorCode::Rights.into(),
        retry: RetryDirective::DoNotRetry.into(),
        safe_message: "authorization denied".to_owned(),
    });
    assert!(validate_sabi_response_context(&terminal_rejection, MethodSemantics::MUTATION).is_ok());

    let mut unsafe_retry = response.clone();
    let context = match unsafe_retry.common_context.as_mut().unwrap() {
        envelope_message::CommonContext::ResponseContext(context) => context,
        envelope_message::CommonContext::RequestContext(_) => unreachable!(),
    };
    context.failure.as_mut().unwrap().retry = RetryDirective::RetrySameIdempotencyKey.into();
    assert_eq!(
        validate_sabi_response_context(&unsafe_retry, MethodSemantics::MUTATION),
        Err(CommonSemanticsError::InvalidRetryDirective)
    );

    let mut partial = response;
    let context = match partial.common_context.as_mut().unwrap() {
        envelope_message::CommonContext::ResponseContext(context) => context,
        envelope_message::CommonContext::RequestContext(_) => unreachable!(),
    };
    context.receipts.clear();
    let failure = context.failure.as_mut().unwrap();
    failure.code = SabiErrorCode::Partial.into();
    failure.retry = RetryDirective::DoNotRetry.into();
    assert_eq!(
        validate_sabi_response_context(&partial, MethodSemantics::MUTATION),
        Err(CommonSemanticsError::MissingReceiptForPartialOutcome)
    );
}

#[test]
fn service_directory_payload_has_its_own_identity_and_bound() {
    let request = ResolveServiceRequest {
        schema: Some(service_directory_schema_identity()),
        service: "operation".to_owned(),
    };
    let wire = encode_resolve_service_request(&request).unwrap();
    let golden = decode_hex(include_str!(
        "../../../schema/golden/nlos.sabi.ServiceDirectory.ResolveRequest-v1.hex"
    ));
    assert_eq!(wire, golden);
    assert_eq!(decode_resolve_service_request(&wire).unwrap(), request);

    let mut wrong_major = request.clone();
    wrong_major.schema.as_mut().unwrap().major = 2;
    assert!(matches!(
        encode_resolve_service_request(&wrong_major),
        Err(CompatibilityError::UnsupportedMajor { got: 2, .. })
    ));

    let missing_result = ResolveServiceResponse {
        schema: Some(service_directory_schema_identity()),
        result: None,
    };
    assert_eq!(
        encode_resolve_service_response(&missing_result),
        Err(CompatibilityError::MissingServiceDirectoryResult)
    );
    assert!(matches!(
        decode_resolve_service_request(&vec![0_u8; MAX_SERVICE_DIRECTORY_PAYLOAD_BYTES + 1]),
        Err(CompatibilityError::FrameTooLarge { .. })
    ));

    let missing_negotiate_result = NegotiateServiceResponse {
        schema: Some(service_directory_schema_identity()),
        result: None,
    };
    assert_eq!(
        nlos_schema::encode_negotiate_service_response(&missing_negotiate_result),
        Err(CompatibilityError::MissingServiceDirectoryResult)
    );
}

#[test]
fn newer_minor_and_noncritical_extensions_are_accepted() {
    let mut candidate = envelope();
    let schema = candidate.schema.as_mut().unwrap();
    schema.minor = 99;
    schema.non_critical_extension_ids.push(7_001);

    let encoded = encode_sabi_envelope(&candidate).unwrap();
    assert_eq!(
        decode_sabi_envelope(&encoded).unwrap().envelope(),
        &candidate
    );
}

#[test]
fn unknown_major_is_rejected() {
    let mut candidate = envelope();
    candidate.schema.as_mut().unwrap().major = 2;

    assert!(matches!(
        encode_sabi_envelope(&candidate),
        Err(CompatibilityError::UnsupportedMajor {
            got: 2,
            supported: 1,
            ..
        })
    ));
}

#[test]
fn unknown_critical_extension_is_rejected() {
    let mut candidate = envelope();
    candidate
        .schema
        .as_mut()
        .unwrap()
        .critical_extension_ids
        .push(7_001);

    assert!(matches!(
        encode_sabi_envelope(&candidate),
        Err(CompatibilityError::UnsupportedCriticalExtension {
            extension_id: 7_001,
            ..
        })
    ));
}

#[test]
fn forwarding_preserves_unknown_protobuf_fields_byte_for_byte() {
    let mut wire = encode_sabi_envelope(&envelope()).unwrap();
    // Unknown field 100, varint value 7. Prost accepts but does not expose it
    // in the generated type, so forwarding must retain the original frame.
    wire.extend_from_slice(&[0xa0, 0x06, 0x07]);

    let validated = decode_sabi_envelope(&wire).unwrap();
    assert_eq!(validated.wire_bytes(), wire);
    assert_eq!(validated.into_wire_bytes(), wire);
}

#[test]
fn malformed_identity_and_oversized_frames_fail_closed() {
    let mut candidate = envelope();
    candidate.request_id.pop();
    assert!(matches!(
        encode_sabi_envelope(&candidate),
        Err(CompatibilityError::InvalidRequestIdLength { actual: 15 })
    ));

    let oversized = vec![0_u8; MAX_ENVELOPE_BYTES + 1];
    assert!(matches!(
        decode_sabi_envelope(&oversized),
        Err(CompatibilityError::FrameTooLarge { .. })
    ));
}

#[test]
fn generated_local_rpc_surface_has_distinct_request_and_response_types() {
    assert_eq!(local_rpc::FULL_NAME, "nlos.sabi.v1.LocalRpcService");
    assert_eq!(
        local_rpc::EXCHANGE_NAME,
        "nlos.sabi.v1.LocalRpcService/Exchange"
    );

    let request = ExchangeRequest {
        envelope: Some(envelope()),
    };
    let request_wire = encode_exchange_request(&request).unwrap();
    assert_eq!(
        decode_exchange_request(&request_wire).unwrap().request(),
        &request
    );

    let response = ExchangeResponse {
        envelope: Some(envelope()),
    };
    let response_wire = encode_exchange_response(&response).unwrap();
    assert_eq!(
        decode_exchange_response(&response_wire).unwrap().response(),
        &response
    );
}

#[test]
fn exchange_wrappers_preserve_unknown_fields_and_require_an_envelope() {
    let request = ExchangeRequest {
        envelope: Some(envelope()),
    };
    let mut wire = encode_exchange_request(&request).unwrap();
    wire.extend_from_slice(&[0xa0, 0x06, 0x07]);
    let decoded = decode_exchange_request(&wire).unwrap();
    assert_eq!(decoded.wire_bytes(), wire);
    assert_eq!(decoded.into_wire_bytes(), wire);

    assert_eq!(
        encode_exchange_request(&ExchangeRequest { envelope: None }),
        Err(CompatibilityError::MissingExchangeEnvelope)
    );
    assert_eq!(
        encode_exchange_response(&ExchangeResponse { envelope: None }),
        Err(CompatibilityError::MissingExchangeEnvelope)
    );
}
