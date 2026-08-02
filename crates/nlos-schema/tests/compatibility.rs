use nlos_schema::sabi::v1::{
    CallerIdentity, CapabilityHandle, Envelope, ExchangeRequest, ExchangeResponse,
    NegotiateServiceResponse, OperationReference, ReceiptReference, ResolveServiceRequest,
    ResolveServiceResponse, RetryDirective, SabiErrorCode, SabiFailure, SabiRequestContext,
    SabiResponseContext, SchemaIdentity, TaskExecutionBinding, envelope as envelope_message,
    local_rpc,
};
use nlos_schema::{
    CommonSemanticsError, CompatibilityError, MAX_ENVELOPE_BYTES,
    MAX_SERVICE_DIRECTORY_PAYLOAD_BYTES, MethodSemantics, SABI_ENVELOPE_SCHEMA,
    SABI_SERVICE_DIRECTORY_SCHEMA, decode_exchange_request, decode_exchange_response,
    decode_resolve_service_request, decode_sabi_envelope, encode_exchange_request,
    encode_exchange_response, encode_resolve_service_request, encode_resolve_service_response,
    encode_sabi_envelope, schema_registry, service_directory_schema_identity,
    validate_sabi_request_context, validate_sabi_response_context,
};

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
    assert_eq!(registry.len(), 2);
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
