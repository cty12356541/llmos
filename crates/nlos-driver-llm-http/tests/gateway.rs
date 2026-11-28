use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use nlos_capability::service_call::{
    AuthorizationError, CallBinding, PolicyGrant, ServiceAuthorizationBinding, ServiceAuthorizer,
    ServiceTime,
};
use nlos_driver_llm_http::llm_gateway::{ChildSessionBinding, Gateway, GatewayError};
use nlos_schema::llm_gateway_schema_identity;
use nlos_schema::sabi::v1::{
    AttenuateBudgetRequest, QueryBudgetRequest, RecordExternalUsageRequest,
};
use nlos_types::PrincipalId;

mod gateway_support;
use gateway_support::*;

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn four_methods_recover_and_keep_advisory_out_of_the_hard_budget() {
    let (port, hits, server) = provider().await;
    let path = TestRoot::new();
    let policy = Arc::new(Policy(AtomicBool::new(true)));
    let clock = Arc::new(AtomicU64::new(10_000));
    let gateway =
        Gateway::initialize(&path.0, config(port, Some(policy.clone()), clock.clone())).unwrap();
    let account = gateway.provision_root(root(), budget(1_000)).unwrap();
    let chat = gateway
        .llm_chat(&call(1, 42), &account, &chat_request())
        .await
        .unwrap();
    let usage = chat.result.usage.unwrap();
    assert_eq!(
        (
            usage.api_calls,
            usage.prompt_tokens,
            usage.completion_tokens,
            usage.total_tokens,
            usage.cost_microcredits
        ),
        (1, 5, 1, 9, 5)
    );
    assert_eq!(chat.provider_attempts, 1);
    let query = QueryBudgetRequest {
        schema: Some(llm_gateway_schema_identity()),
    };
    let before = gateway
        .query_budget(&call(2, 42), &account, &query)
        .unwrap();
    let advisory = RecordExternalUsageRequest {
        schema: Some(llm_gateway_schema_identity()),
        provider: "test".into(),
        model: "advisory".into(),
        usage: Some(budget(8)),
        recorded_at_ms: u64::MAX,
    };
    assert!(
        !gateway
            .record_external_usage(&call(3, 42), &account, &advisory)
            .unwrap()
            .result
            .replayed
    );
    assert!(
        gateway
            .record_external_usage(&call(3, 43), &account, &advisory)
            .unwrap()
            .result
            .replayed
    );
    assert!(matches!(
        gateway.record_external_usage(&call(4, 43), &account, &advisory),
        Err(GatewayError::Quota)
    ));
    let after = gateway
        .query_budget(&call(2, 42), &account, &query)
        .unwrap();
    assert_eq!(after.remaining, before.remaining);
    assert_eq!(after.consumed, before.consumed);
    assert_eq!(after.external_advisory, Some(budget(8)));
    let child = ChildSessionBinding {
        recipient: call(1, 9).caller,
        stable_identity: [11; 16],
        delegated: false,
    };
    let attenuation = AttenuateBudgetRequest {
        schema: Some(llm_gateway_schema_identity()),
        child_limit: Some(budget(60)),
        valid_until_ms: 15_000,
        child_process_id: Vec::new(),
        child_session_id: Vec::new(),
        child_principal_id: Vec::new(),
    };
    let child_binding = gateway
        .attenuate_budget(&call(7, 42), &account, Some(&child), &attenuation)
        .unwrap()
        .child;
    assert_ne!(child_binding.account.account_id(), account.account_id());
    assert_eq!(
        gateway
            .query_budget(&call(1, 9), &child_binding.account, &query)
            .unwrap()
            .limit,
        Some(budget(60))
    );
    drop(gateway);
    let gateway = Gateway::open(&path.0, config(port, Some(policy.clone()), clock)).unwrap();
    assert_eq!(
        gateway
            .attenuate_budget(&call(7, 43), &account, Some(&child), &attenuation)
            .unwrap()
            .child,
        child_binding
    );
    let replay = gateway
        .llm_chat(&call(1, 43), &account, &chat_request())
        .await
        .unwrap();
    assert_eq!(replay.result.usage, Some(usage));
    assert_eq!(replay.result.content_utf8, "答");
    assert_eq!(replay.receipt, chat.receipt);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    let mut changed = chat_request();
    changed.turns[0].content = "changed".into();
    assert!(matches!(
        gateway.llm_chat(&call(1, 43), &account, &changed).await,
        Err(GatewayError::Conflict)
    ));
    policy.0.store(false, Ordering::SeqCst);
    assert!(matches!(
        gateway
            .llm_chat(&call(1, 43), &account, &chat_request())
            .await,
        Err(GatewayError::Authorization(AuthorizationError::Denied))
    ));
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn quote_checks_each_dimension_before_any_tcp_attempt() {
    let (port, hits, server) = provider().await;
    let path = TestRoot::new();
    let policy = Arc::new(Policy(AtomicBool::new(true)));
    let clock = Arc::new(AtomicU64::new(10_000));
    let gateway = Gateway::initialize(&path.0, config(port, Some(policy), clock)).unwrap();
    for (index, dimension) in nlos_resource::LlmBudgetDimension::ALL
        .into_iter()
        .enumerate()
    {
        let mut limits = budget(1_000);
        match dimension {
            nlos_resource::LlmBudgetDimension::PromptTokens => limits.prompt_tokens = 9,
            nlos_resource::LlmBudgetDimension::CompletionTokens => limits.completion_tokens = 11,
            nlos_resource::LlmBudgetDimension::TotalTokens => limits.total_tokens = 11,
            nlos_resource::LlmBudgetDimension::ApiCalls => limits.api_calls = 0,
            nlos_resource::LlmBudgetDimension::CostMicrocredits => limits.cost_microcredits = 1,
        }
        let mut specification = root();
        specification.authorization_domain[0] = u8::try_from(index).unwrap();
        let account = gateway.provision_root(specification, limits).unwrap();
        assert!(
            matches!(gateway.llm_chat(&call(1,42),&account,&chat_request()).await,Err(GatewayError::Budget(d)) if d == dimension)
        );
    }
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    server.abort();
}

fn wire_call(name: &str, call: &CallBinding, payload: Vec<u8>) -> nlos_schema::sabi::v1::Envelope {
    use nlos_schema::sabi::v1::{
        CallerIdentity, CapabilityHandle as WireHandle, SabiRequestContext, envelope::CommonContext,
    };
    nlos_schema::sabi::v1::Envelope {
        schema: Some(envelope_schema_identity()),
        request_id: vec![9; 16],
        service: "llm_gateway".into(),
        method: name.into(),
        payload,
        common_context: Some(CommonContext::RequestContext(SabiRequestContext {
            caller: Some(CallerIdentity {
                principal_id: call.caller.principal.as_bytes().to_vec(),
                application_id: call.caller.application.as_bytes().to_vec(),
                process_id: call.caller.process.as_bytes().to_vec(),
                process_generation: call.caller.process_generation.get(),
            }),
            idempotency_key: call
                .idempotency_key
                .map_or_else(Vec::new, |key| key.as_bytes().to_vec()),
            deadline_monotonic_ns: call.deadline_monotonic_ns.unwrap_or(0),
            capability_handles: vec![WireHandle {
                slot: 4,
                generation: 1,
            }],
            correlation_id: vec![10; 16],
            ..Default::default()
        })),
    }
}

fn wire_header(
    envelope: &mut nlos_schema::sabi::v1::Envelope,
) -> &mut nlos_schema::sabi::v1::SabiRequestContext {
    let Some(nlos_schema::sabi::v1::envelope::CommonContext::RequestContext(header)) =
        &mut envelope.common_context
    else {
        panic!("test envelope must be a request")
    };
    header
}

fn check_response(
    reply: &nlos_driver_llm_http::llm_gateway::WireReply,
    semantics: nlos_schema::MethodSemantics,
    failure: Option<nlos_schema::sabi::v1::SabiErrorCode>,
) -> nlos_schema::ValidatedFrame {
    let frame = nlos_schema::decode_sabi_envelope(&reply.frame).unwrap();
    assert_eq!(frame.envelope().schema, Some(envelope_schema_identity()));
    let context = nlos_schema::validate_sabi_response_context(frame.envelope(), semantics).unwrap();
    assert_eq!(
        context.failure.as_ref().map(|failure| failure.code),
        failure.map(|code| code as i32)
    );
    frame
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn all_wire_methods_use_canonical_codec_and_reject_header_substitution() {
    use nlos_schema::sabi::v1::{SabiErrorCode, TaskExecutionBinding};
    use nlos_schema::{MethodSemantics, encode_sabi_envelope};
    let (port, hits, server) = provider().await;
    let path = TestRoot::new();
    let policy = Arc::new(Policy(AtomicBool::new(true)));
    let clock = Arc::new(AtomicU64::new(10_000));
    let gateway = Gateway::initialize(&path.0, config(port, Some(policy), clock)).unwrap();
    let account = gateway.provision_root(root(), budget(300_000)).unwrap();
    let caller = call(73, 14);

    let query = QueryBudgetRequest {
        schema: Some(llm_gateway_schema_identity()),
    };
    let query = wire_call(
        "query_budget",
        &caller,
        nlos_schema::encode_query_budget_request(&query).unwrap(),
    );
    for field in 0..8 {
        let mut forged = query.clone();
        let header = wire_header(&mut forged);
        let claimed = header.caller.as_mut().unwrap();
        match field {
            0 => claimed.principal_id[0] ^= 1,
            1 => claimed.application_id[0] ^= 1,
            2 => claimed.process_id[0] ^= 1,
            3 => claimed.process_generation += 1,
            4 => header.proposal_or_input_digest_sha256 = vec![99; 32],
            5 => header.capability_handles[0].slot += 1,
            6 => header.capability_handles[0].generation += 1,
            _ => header.deadline_monotonic_ns += 1,
        }
        let reply = gateway
            .handle_wire(
                &encode_sabi_envelope(&forged).unwrap(),
                &caller,
                &[4],
                &account,
                None,
            )
            .await
            .unwrap();
        check_response(&reply, MethodSemantics::QUERY, Some(SabiErrorCode::Rights));
    }
    let mut fenced = query.clone();
    wire_header(&mut fenced).task_execution_binding = Some(TaskExecutionBinding {
        task_attempt_id: vec![3; 16],
        task_authority_term: 1,
        isolation_domain_generation: 1,
        ..Default::default()
    });
    let reply = gateway
        .handle_wire(
            &encode_sabi_envelope(&fenced).unwrap(),
            &caller,
            &[4],
            &account,
            None,
        )
        .await
        .unwrap();
    check_response(
        &reply,
        MethodSemantics::QUERY,
        Some(SabiErrorCode::NotSupported),
    );

    let reply = gateway
        .handle_wire(
            &encode_sabi_envelope(&query).unwrap(),
            &caller,
            &[4],
            &account,
            None,
        )
        .await
        .unwrap();
    let response = check_response(&reply, MethodSemantics::QUERY, None);
    let query_result =
        nlos_schema::decode_query_budget_result(&response.envelope().payload).unwrap();
    assert_eq!(query_result.schema, Some(llm_gateway_schema_identity()));
    assert_eq!(query_result.remaining, Some(budget(300_000)));

    let chat = wire_call(
        "llm_chat",
        &caller,
        nlos_schema::encode_llm_chat_request(&chat_request()).unwrap(),
    );
    let reply = gateway
        .handle_wire(
            &encode_sabi_envelope(&chat).unwrap(),
            &caller,
            &[4],
            &account,
            None,
        )
        .await
        .unwrap();
    let response = check_response(&reply, MethodSemantics::LONG_RUNNING_MUTATION, None);
    let chat_result = nlos_schema::decode_llm_chat_result(&response.envelope().payload).unwrap();
    assert_eq!(chat_result.schema, Some(llm_gateway_schema_identity()));
    assert_eq!(chat_result.content_utf8, "答");

    let child = ChildSessionBinding {
        recipient: call(1, 12).caller,
        stable_identity: [55; 16],
        delegated: false,
    };
    let attenuation = AttenuateBudgetRequest {
        schema: Some(llm_gateway_schema_identity()),
        child_limit: Some(budget(60)),
        valid_until_ms: 19_000,
        child_process_id: Vec::new(),
        child_session_id: Vec::new(),
        child_principal_id: Vec::new(),
    };
    let attenuation = wire_call(
        "attenuate_budget",
        &caller,
        nlos_schema::encode_attenuate_budget_request(&attenuation).unwrap(),
    );
    let encoded = encode_sabi_envelope(&attenuation).unwrap();
    let without_child = gateway
        .handle_wire(&encoded, &caller, &[4], &account, None)
        .await
        .unwrap();
    check_response(
        &without_child,
        MethodSemantics::MUTATION,
        Some(SabiErrorCode::Rights),
    );
    let reply = gateway
        .handle_wire(&encoded, &caller, &[4], &account, Some(&child))
        .await
        .unwrap();
    assert!(reply.child.is_some());
    let response = check_response(&reply, MethodSemantics::MUTATION, None);
    let attenuate_result =
        nlos_schema::decode_attenuate_budget_result(&response.envelope().payload).unwrap();
    assert_eq!(attenuate_result.schema, Some(llm_gateway_schema_identity()));
    assert_eq!(attenuate_result.granted_limit, Some(budget(60)));

    let advisory = RecordExternalUsageRequest {
        schema: Some(llm_gateway_schema_identity()),
        provider: "unit".into(),
        model: "unit".into(),
        usage: Some(budget(2)),
        recorded_at_ms: 10,
    };
    let advisory = wire_call(
        "record_external_usage",
        &caller,
        nlos_schema::encode_record_external_usage_request(&advisory).unwrap(),
    );
    let reply = gateway
        .handle_wire(
            &encode_sabi_envelope(&advisory).unwrap(),
            &caller,
            &[4],
            &account,
            None,
        )
        .await
        .unwrap();
    let response = check_response(&reply, MethodSemantics::MUTATION, None);
    let advisory_result =
        nlos_schema::decode_record_external_usage_result(&response.envelope().payload).unwrap();
    assert_eq!(advisory_result.schema, Some(llm_gateway_schema_identity()));
    assert!(!advisory_result.replayed);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn wire_accepts_envelope_schema_minor_variants_and_preserves_inner_result_schema() {
    use nlos_schema::{MethodSemantics, encode_sabi_envelope};
    let (port, hits, server) = provider().await;
    let path = TestRoot::new();
    let policy = Arc::new(Policy(AtomicBool::new(true)));
    let clock = Arc::new(AtomicU64::new(10_000));
    let gateway = Gateway::initialize(&path.0, config(port, Some(policy), clock)).unwrap();
    let account = gateway.provision_root(root(), budget(1_000)).unwrap();
    let caller = call(81, 14);

    let mut minor_zero_query = wire_call(
        "query_budget",
        &caller,
        nlos_schema::encode_query_budget_request(&query_request()).unwrap(),
    );
    minor_zero_query.schema.as_mut().unwrap().minor = 0;
    let reply = gateway
        .handle_wire(
            &encode_sabi_envelope(&minor_zero_query).unwrap(),
            &caller,
            &[4],
            &account,
            None,
        )
        .await
        .unwrap();
    let frame = check_response(&reply, MethodSemantics::QUERY, None);
    let decoded_query = nlos_schema::decode_query_budget_result(&frame.envelope().payload).unwrap();
    assert_eq!(decoded_query.schema, Some(llm_gateway_schema_identity()));
    assert_eq!(decoded_query.remaining, Some(budget(1_000)));

    let chat = wire_call(
        "llm_chat",
        &caller,
        nlos_schema::encode_llm_chat_request(&chat_request()).unwrap(),
    );
    let reply = gateway
        .handle_wire(
            &encode_sabi_envelope(&chat).unwrap(),
            &caller,
            &[4],
            &account,
            None,
        )
        .await
        .unwrap();
    let frame = check_response(&reply, MethodSemantics::LONG_RUNNING_MUTATION, None);
    let decoded_chat = nlos_schema::decode_llm_chat_result(&frame.envelope().payload).unwrap();
    assert_eq!(decoded_chat.schema, Some(llm_gateway_schema_identity()));
    assert_eq!(decoded_chat.content_utf8, "答");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test]
async fn wire_rejects_outer_gateway_schema_and_incompatible_envelope_versions() {
    use nlos_schema::{
        CompatibilityError, SABI_ENVELOPE_SCHEMA, decode_sabi_envelope, encode_sabi_envelope,
    };
    let (port, hits, server) = provider().await;
    let path = TestRoot::new();
    let policy = Arc::new(Policy(AtomicBool::new(true)));
    let clock = Arc::new(AtomicU64::new(10_000));
    let gateway = Gateway::initialize(&path.0, config(port, Some(policy), clock)).unwrap();
    let account = gateway.provision_root(root(), budget(1_000)).unwrap();
    let caller = call(82, 14);
    let payload = nlos_schema::encode_query_budget_request(&query_request()).unwrap();

    for (schema, service) in [
        (llm_gateway_schema_identity(), "llm_gateway"),
        (nlos_schema::tool_sandbox_schema_identity(), "llm_gateway"),
        (envelope_schema_identity(), "tool_sandbox"),
    ] {
        let mut bad = wire_call("query_budget", &caller, payload.clone());
        bad.schema = Some(schema);
        bad.service = service.into();
        let bytes = encode_sabi_envelope(&bad).unwrap();
        assert_eq!(
            gateway
                .handle_wire(&bytes, &caller, &[4], &account, None)
                .await
                .err(),
            Some(GatewayError::IncompatibleSchema)
        );
    }

    let valid = encode_sabi_envelope(&wire_call("query_budget", &caller, payload)).unwrap();
    assert_eq!(valid[0], 0x0a);
    let missing_schema = valid[2 + usize::from(valid[1])..].to_vec();
    let name_pos = valid
        .windows(SABI_ENVELOPE_SCHEMA.len())
        .position(|window| window == SABI_ENVELOPE_SCHEMA.as_bytes())
        .unwrap();
    let suffix = name_pos + SABI_ENVELOPE_SCHEMA.len();
    assert_eq!(&valid[suffix..suffix + 4], &[0x10, 0x01, 0x18, 0x01]);

    let mut wrong_major = valid.clone();
    wrong_major[suffix + 1] = 0x02;
    let mut critical_ext = valid;
    critical_ext[suffix + 2] = 0x20;

    for (raw, expected) in [
        (missing_schema, CompatibilityError::MissingSchemaIdentity),
        (
            wrong_major,
            CompatibilityError::UnsupportedMajor {
                schema: SABI_ENVELOPE_SCHEMA.into(),
                got: 2,
                supported: 1,
            },
        ),
        (
            critical_ext,
            CompatibilityError::UnsupportedCriticalExtension {
                schema: SABI_ENVELOPE_SCHEMA.into(),
                extension_id: 1,
            },
        ),
    ] {
        assert_eq!(decode_sabi_envelope(&raw).unwrap_err(), expected);
        assert_eq!(
            gateway
                .handle_wire(&raw, &caller, &[4], &account, None)
                .await
                .err(),
            Some(GatewayError::IncompatibleSchema)
        );
    }
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    server.abort();
}

#[tokio::test]
async fn wire_failure_replies_wrap_sabi_failure_in_envelope_schema() {
    use nlos_schema::sabi::v1::SabiErrorCode;
    use nlos_schema::{MethodSemantics, encode_sabi_envelope};
    let (port, hits, server) = provider().await;
    let path = TestRoot::new();
    let policy = Arc::new(Policy(AtomicBool::new(false)));
    let clock = Arc::new(AtomicU64::new(10_000));
    let gateway = Gateway::initialize(&path.0, config(port, Some(policy.clone()), clock)).unwrap();
    let account = gateway.provision_root(root(), budget(1_000)).unwrap();
    let caller = call(83, 14);

    let denied_query = wire_call(
        "query_budget",
        &caller,
        nlos_schema::encode_query_budget_request(&query_request()).unwrap(),
    );
    let reply = gateway
        .handle_wire(
            &encode_sabi_envelope(&denied_query).unwrap(),
            &caller,
            &[4],
            &account,
            None,
        )
        .await
        .unwrap();
    let frame = check_response(&reply, MethodSemantics::QUERY, Some(SabiErrorCode::Rights));
    assert_eq!(frame.envelope().payload, [] as [u8; 0]);

    policy.0.store(true, Ordering::SeqCst);
    let missing_inner_schema = wire_call("query_budget", &caller, Vec::new());
    let reply = gateway
        .handle_wire(
            &encode_sabi_envelope(&missing_inner_schema).unwrap(),
            &caller,
            &[4],
            &account,
            None,
        )
        .await
        .unwrap();
    let frame = check_response(
        &reply,
        MethodSemantics::QUERY,
        Some(SabiErrorCode::AbiVersion),
    );
    assert_eq!(frame.envelope().payload, [] as [u8; 0]);
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    server.abort();
}

fn advisory_request(amount: u64) -> RecordExternalUsageRequest {
    RecordExternalUsageRequest {
        schema: Some(llm_gateway_schema_identity()),
        provider: "unit".into(),
        model: "unit".into(),
        usage: Some(budget(amount)),
        recorded_at_ms: 99,
    }
}

fn query_request() -> QueryBudgetRequest {
    QueryBudgetRequest {
        schema: Some(llm_gateway_schema_identity()),
    }
}

fn child_limit(amount: u64, valid_until_ms: u64) -> AttenuateBudgetRequest {
    AttenuateBudgetRequest {
        schema: Some(llm_gateway_schema_identity()),
        child_limit: Some(budget(amount)),
        valid_until_ms,
        child_process_id: Vec::new(),
        child_session_id: Vec::new(),
        child_principal_id: Vec::new(),
    }
}

#[tokio::test]
async fn no_policy_clock_failure_and_wrong_scope_fail_without_network() {
    let (port, hits, server) = provider().await;
    let path = TestRoot::new();
    let clock = Arc::new(AtomicU64::new(10_000));
    let gateway = Gateway::initialize(&path.0, config(port, None, clock.clone())).unwrap();
    let account = gateway.provision_root(root(), budget(1_000)).unwrap();
    let call1 = call(12, 14);
    let child = ChildSessionBinding {
        recipient: call(1, 15).caller,
        stable_identity: [1; 16],
        delegated: false,
    };
    let denied = GatewayError::Authorization(AuthorizationError::Denied);
    assert_eq!(
        gateway
            .llm_chat(&call1, &account, &chat_request())
            .await
            .unwrap_err(),
        denied
    );
    assert_eq!(
        gateway
            .query_budget(&call1, &account, &query_request())
            .unwrap_err(),
        denied
    );
    assert!(
        matches!(gateway.attenuate_budget(&call1, &account, Some(&child), &child_limit(5, 12_000)), Err(error) if error == denied)
    );
    assert!(
        matches!(gateway.record_external_usage(&call1, &account, &advisory_request(1)), Err(error) if error == denied)
    );
    let policy = Arc::new(Policy(AtomicBool::new(true)));
    drop(gateway);
    let gateway = Gateway::open(&path.0, config(port, Some(policy), clock.clone())).unwrap();
    clock.store(0, Ordering::SeqCst);
    assert_eq!(
        gateway
            .llm_chat(&call1, &account, &chat_request())
            .await
            .unwrap_err(),
        GatewayError::Authorization(AuthorizationError::TimeUnavailable)
    );
    assert_eq!(
        gateway
            .query_budget(&call1, &account, &query_request())
            .unwrap_err(),
        GatewayError::Authorization(AuthorizationError::TimeUnavailable)
    );
    clock.store(10_000, Ordering::SeqCst);
    let mut wrong_caller = call1.clone();
    wrong_caller.caller.principal = PrincipalId::from_bytes([40; 16]);
    assert_eq!(
        gateway
            .query_budget(&wrong_caller, &account, &query_request())
            .unwrap_err(),
        denied
    );
    let mut wrong_child = child;
    wrong_child.recipient.principal = wrong_caller.caller.principal;
    assert!(
        matches!(gateway.attenuate_budget(&call1, &account, Some(&wrong_child), &child_limit(5, 12_000)), Err(error) if error == denied)
    );
    wrong_child.delegated = true;
    let cross = gateway
        .attenuate_budget(
            &call1,
            &account,
            Some(&wrong_child),
            &child_limit(5, 12_000),
        )
        .unwrap();
    assert_eq!(
        gateway
            .query_budget(&call1, &cross.child.account, &query_request())
            .unwrap_err(),
        denied
    );
    wrong_caller.caller = wrong_child.recipient;
    assert_eq!(
        gateway
            .query_budget(&wrong_caller, &cross.child.account, &query_request())
            .unwrap()
            .limit,
        Some(budget(5))
    );
    clock.store(12_001, Ordering::SeqCst);
    assert!(matches!(
        gateway.query_budget(&wrong_caller, &cross.child.account, &query_request()),
        Err(GatewayError::Authorization(
            AuthorizationError::GrantExpired
        ))
    ));
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    server.abort();
}

struct RevokingPolicy {
    checks: AtomicUsize,
    deny_after: usize,
    substitution: Option<usize>,
}

impl ServiceAuthorizer for RevokingPolicy {
    fn authorize(
        &self,
        input: &ServiceAuthorizationBinding,
        _: ServiceTime,
    ) -> Result<PolicyGrant, AuthorizationError> {
        use nlos_capability::service_call::{ServiceMethod, ServiceObject};
        use nlos_types::ResourceAccountId;
        if self.checks.fetch_add(1, Ordering::SeqCst) >= self.deny_after {
            return Err(AuthorizationError::Denied);
        }
        let mut grant = Policy(AtomicBool::new(true)).authorize(
            input,
            ServiceTime {
                wall_ms: 1,
                monotonic_ns: 1,
            },
        )?;
        match self.substitution {
            Some(0) => grant.binding.action.method = ServiceMethod::ListTools,
            Some(1) => {
                grant.binding.action.target.object =
                    ServiceObject::GatewayAccount(ResourceAccountId::from_bytes([93; 16]));
            }
            Some(2) => grant.binding.action.input_digest[0] ^= 1,
            Some(3) => grant.valid_until_ms = 5_000,
            _ => (),
        }
        Ok(grant)
    }
}

#[tokio::test]
async fn substitute_policy_grant_or_revoke_after_sync_cannot_emit_tcp() {
    let (port, hits, server) = provider().await;
    for case in 0..5 {
        let path = TestRoot::new();
        let clock = Arc::new(AtomicU64::new(10_000));
        let policy = Arc::new(RevokingPolicy {
            checks: AtomicUsize::new(0),
            deny_after: if case == 4 { 3 } else { usize::MAX },
            substitution: (case < 4).then_some(case),
        });
        let gateway = Gateway::initialize(&path.0, config(port, Some(policy), clock)).unwrap();
        let account = gateway.provision_root(root(), budget(1_000)).unwrap();
        let expected = match case {
            0..=2 => AuthorizationError::BindingMismatch,
            3 => AuthorizationError::GrantExpired,
            _ => AuthorizationError::Denied,
        };
        assert_eq!(
            gateway
                .llm_chat(&call(1, 1), &account, &chat_request())
                .await
                .unwrap_err(),
            GatewayError::Authorization(expected)
        );
    }
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    server.abort();
}

#[tokio::test]
async fn active_child_and_global_binding_limits_do_not_discard_expired_history() {
    let (port, hits, server) = provider().await;
    let path = TestRoot::new();
    let clock = Arc::new(AtomicU64::new(10_000));
    let policy = Arc::new(Policy(AtomicBool::new(true)));
    let mut cfg = config(port, Some(policy), clock.clone());
    cfg.limits.children_per_parent = 1;
    cfg.limits.active_accounts = 3;
    let gateway = Gateway::initialize(&path.0, cfg).unwrap();
    let parent = gateway.provision_root(root(), budget(1_000)).unwrap();
    let mut other_root = root();
    other_root.authorization_domain[0] += 1;
    gateway.provision_root(other_root, budget(1_000)).unwrap();
    let first = ChildSessionBinding {
        recipient: call(1, 15).caller,
        stable_identity: [1; 16],
        delegated: false,
    };
    let next = ChildSessionBinding {
        recipient: call(1, 16).caller,
        stable_identity: [2; 16],
        delegated: false,
    };
    let earlier = gateway
        .attenuate_budget(
            &call(1, 1),
            &parent,
            Some(&first),
            &child_limit(100, 10_100),
        )
        .unwrap();
    assert!(matches!(
        gateway.attenuate_budget(&call(2, 1), &parent, Some(&next), &child_limit(100, 15_000)),
        Err(GatewayError::Quota)
    ));
    other_root.authorization_domain[0] += 1;
    assert!(matches!(
        gateway.provision_root(other_root, budget(1_000)),
        Err(GatewayError::Quota)
    ));
    gateway
        .llm_chat(&call(8, 15), &earlier.child.account, &chat_request())
        .await
        .unwrap();
    clock.store(10_101, Ordering::SeqCst);
    gateway
        .attenuate_budget(&call(2, 1), &parent, Some(&next), &child_limit(100, 15_000))
        .unwrap();
    let parent_usage = gateway
        .query_budget(&call(1, 1), &parent, &query_request())
        .unwrap()
        .consumed
        .unwrap();
    assert_eq!(parent_usage.api_calls, 1);
    assert_eq!(parent_usage.total_tokens, 9);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    server.abort();
}

async fn wait_hit(hits: &mut tokio::sync::mpsc::Receiver<usize>, expected: usize) {
    assert_eq!(
        tokio::time::timeout(std::time::Duration::from_secs(2), hits.recv())
            .await
            .unwrap(),
        Some(expected)
    );
}

fn begin_chat(
    gateway: &Arc<Gateway>,
    caller: CallBinding,
    account: nlos_driver_llm_http::llm_gateway::GatewayAccountBinding,
) -> tokio::task::JoinHandle<Result<nlos_driver_llm_http::llm_gateway::ChatReply, GatewayError>> {
    let gateway = gateway.clone();
    tokio::spawn(async move { gateway.llm_chat(&caller, &account, &chat_request()).await })
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn subtree_gate_serializes_parent_siblings_and_grandchild_with_live_tcp() {
    use nlos_resource::{LlmBudgetDimension, ResourceAuthority};
    let (reply0, send0) = Reply::response(200, VALID_BODY).held();
    let (reply1, send1) = Reply::response(200, VALID_BODY).held();
    let (port, mut hits, server) = endpoint(vec![reply0, reply1]).await;
    let path = TestRoot::new();
    let policy = Arc::new(Policy(AtomicBool::new(true)));
    let clock = Arc::new(AtomicU64::new(10_000));
    let gateway =
        Arc::new(Gateway::initialize(&path.0, config(port, Some(policy), clock)).unwrap());
    let mut ceiling = budget(1_000);
    ceiling.api_calls = 2;
    let parent = gateway.provision_root(root(), ceiling).unwrap();
    let add_child = |parent, caller: &CallBinding, child_caller, stable: u8| {
        let child_input = ChildSessionBinding {
            recipient: child_caller,
            stable_identity: [stable; 16],
            delegated: true,
        };
        let mut grant = child_limit(1_000, 18_000);
        grant.child_limit = Some(ceiling);
        gateway
            .attenuate_budget(caller, &parent, Some(&child_input), &grant)
            .unwrap()
            .child
            .account
    };
    let parent_caller = call(1, 1);
    let left_caller = call(2, 2);
    let mut right_caller = call(3, 3);
    right_caller.caller.principal = PrincipalId::from_bytes([31; 16]);
    let grand_caller = call(4, 4);
    let left = add_child(parent, &parent_caller, left_caller.caller, 20);
    let right = add_child(parent, &call(8, 1), right_caller.caller, 21);
    let grand = add_child(left, &left_caller, grand_caller.caller, 22);
    let first = begin_chat(&gateway, grand_caller.clone(), grand);
    wait_hit(&mut hits, 0).await;
    let second = begin_chat(&gateway, right_caller.clone(), right);
    wait_hit(&mut hits, 1).await;
    let q = query_request();
    for (caller, account) in [
        (&parent_caller, parent),
        (&left_caller, left),
        (&right_caller, right),
        (&grand_caller, grand),
    ] {
        let budget = gateway.query_budget(caller, &account, &q).unwrap();
        assert_eq!(budget.remaining.unwrap().api_calls, 0);
        assert_eq!(budget.consumed.unwrap().api_calls, 0);
    }
    for (caller, account) in [(&parent_caller, parent), (&left_caller, left)] {
        assert_eq!(
            gateway
                .llm_chat(caller, &account, &chat_request())
                .await
                .unwrap_err(),
            GatewayError::Budget(LlmBudgetDimension::ApiCalls)
        );
    }
    send0.add_permits(1);
    first.await.unwrap().unwrap();
    let usage = gateway.query_budget(&parent_caller, &parent, &q).unwrap();
    assert_eq!(usage.consumed.unwrap().api_calls, 1);
    assert_eq!(usage.remaining.unwrap().api_calls, 0);
    send1.add_permits(1);
    second.await.unwrap().unwrap();
    let authority = ResourceAuthority::open(&path.0).unwrap();
    for (caller, account, own, subtree) in [
        (&parent_caller, parent, 0, 2),
        (&left_caller, left, 0, 1),
        (&right_caller, right, 1, 1),
        (&grand_caller, grand, 1, 1),
    ] {
        let hard = authority
            .inspect_llm_token_budget(account.account_id())
            .unwrap();
        assert_eq!(hard.consumed_api_calls, own);
        assert!(hard.consumed_api_calls <= hard.limits.max_api_calls);
        let effective = gateway.query_budget(caller, &account, &q).unwrap();
        assert_eq!(effective.consumed.unwrap().api_calls, subtree);
        assert_eq!(effective.remaining.unwrap().api_calls, 0);
    }
    assert!(hits.try_recv().is_err());
    server.abort();
}

#[tokio::test]
async fn concurrency_limits_apply_across_accounts_and_release_after_cancellation_deadline() {
    let (reply0, send0) = Reply::response(200, VALID_BODY).held();
    let (reply1, send1) = Reply::response(200, VALID_BODY).held();
    let (port, mut hits, server) =
        endpoint(vec![reply0, reply1, Reply::response(200, VALID_BODY)]).await;
    let path = TestRoot::new();
    let policy = Arc::new(Policy(AtomicBool::new(true)));
    let clock = Arc::new(AtomicU64::new(10_000));
    let mut cfg = config(port, Some(policy), clock.clone());
    cfg.limits.active_calls = 2;
    cfg.limits.calls_per_principal = 1;
    let gateway = Arc::new(Gateway::initialize(&path.0, cfg).unwrap());
    let mut root_config = root();
    let account1 = gateway.provision_root(root_config, budget(1_000)).unwrap();
    root_config.authorization_domain[0] += 1;
    let account2 = gateway.provision_root(root_config, budget(1_000)).unwrap();
    let mut different = call(5, 4);
    root_config.principal = PrincipalId::from_bytes([31; 16]);
    different.caller.principal = root_config.principal;
    let account3 = gateway.provision_root(root_config, budget(1_000)).unwrap();
    root_config.principal = PrincipalId::from_bytes([32; 16]);
    let account4 = gateway.provision_root(root_config, budget(1_000)).unwrap();
    let mut third = call(6, 4);
    third.caller.principal = root_config.principal;
    let first = begin_chat(&gateway, call(3, 3), account1);
    wait_hit(&mut hits, 0).await;
    assert_eq!(
        gateway
            .llm_chat(&call(4, 4), &account2, &chat_request())
            .await
            .unwrap_err(),
        GatewayError::Quota
    );
    let second = begin_chat(&gateway, different, account3);
    wait_hit(&mut hits, 1).await;
    assert_eq!(
        gateway
            .llm_chat(&third, &account4, &chat_request())
            .await
            .unwrap_err(),
        GatewayError::Quota
    );
    assert!(hits.try_recv().is_err());
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    clock.store(20_000, Ordering::SeqCst);
    let reply = gateway
        .llm_chat(&third, &account4, &chat_request())
        .await
        .unwrap();
    assert_eq!(reply.provider_attempts, 1);
    wait_hit(&mut hits, 2).await;
    send0.add_permits(1);
    send1.add_permits(1);
    second.await.unwrap().unwrap();
    let unknown = gateway
        .llm_chat(&call(3, 5), &account1, &chat_request())
        .await
        .unwrap_err();
    assert!(matches!(unknown, GatewayError::EffectUnknown(_)));
    assert!(hits.try_recv().is_err());
    server.abort();
}

#[tokio::test]
async fn durable_capacity_rejects_before_effect_and_preserves_terminal_space() {
    let (port, hits, server) = provider().await;
    let clock = Arc::new(AtomicU64::new(10_000));
    let policy = Arc::new(Policy(AtomicBool::new(true)));
    for case in 0..3 {
        let path = TestRoot::new();
        let mut cfg = config(port, Some(policy.clone()), clock.clone());
        match case {
            0 => cfg.limits.journal_admission_records = 3,
            1 => {
                cfg.limits.journal_admission_bytes = 450;
                cfg.limits.journal_max_bytes = 2_000;
            }
            _ => cfg.limits.journal_max_records = 7,
        }
        if case == 2 {
            cfg.limits.journal_admission_records = 7;
        }
        let gateway = Gateway::initialize(&path.0, cfg).unwrap();
        let account = gateway.provision_root(root(), budget(1000)).unwrap();
        let outcome = gateway
            .llm_chat(&call(1, 1), &account, &chat_request())
            .await;
        if case == 0 {
            assert_eq!(outcome.unwrap().result.content_utf8, "答");
            assert_eq!(
                gateway
                    .llm_chat(&call(2, 1), &account, &chat_request())
                    .await
                    .unwrap_err(),
                GatewayError::Quota
            );
            assert!(matches!(
                gateway.record_external_usage(&call(2, 1), &account, &advisory_request(1)),
                Err(GatewayError::Quota)
            ));
        } else {
            assert_eq!(outcome.unwrap_err(), GatewayError::Quota);
        }
    }
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    server.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 3)]
async fn advisory_rate_is_serialized_across_sessions_and_uses_server_window() {
    let path = TestRoot::new();
    let policy = Arc::new(Policy(AtomicBool::new(true)));
    let clock = Arc::new(AtomicU64::new(10_000));
    let gateway =
        Arc::new(Gateway::initialize(&path.0, config(1, Some(policy), clock.clone())).unwrap());
    let account = gateway.provision_root(root(), budget(1_000)).unwrap();
    let ready = Arc::new(tokio::sync::Barrier::new(3));
    let join = |key| {
        let gateway = gateway.clone();
        let ready = ready.clone();
        tokio::spawn(async move {
            ready.wait().await;
            gateway.record_external_usage(&call(key, key), &account, &advisory_request(3))
        })
    };
    let one = join(1);
    let two = join(2);
    ready.wait().await;
    let first = one.await.unwrap();
    let second = two.await.unwrap();
    assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
    let winner = if first.is_ok() { 1 } else { 2 };
    assert!(matches!(
        gateway.record_external_usage(&call(winner, 5), &account, &advisory_request(4)),
        Err(GatewayError::Conflict)
    ));
    assert!(
        gateway
            .record_external_usage(&call(winner, 6), &account, &advisory_request(3))
            .unwrap()
            .result
            .replayed
    );
    clock.store(9_999, Ordering::SeqCst);
    assert!(matches!(
        gateway.record_external_usage(&call(3, 9), &account, &advisory_request(3)),
        Err(GatewayError::Authorization(
            AuthorizationError::TimeUnavailable
        ))
    ));
    clock.store(69_999, Ordering::SeqCst);
    assert!(matches!(
        gateway.record_external_usage(&call(3, 9), &account, &advisory_request(3)),
        Err(GatewayError::Quota)
    ));
    clock.store(70_000, Ordering::SeqCst);
    assert!(
        !gateway
            .record_external_usage(&call(3, 9), &account, &advisory_request(3))
            .unwrap()
            .result
            .replayed
    );
    let observed = gateway
        .query_budget(&call(8, 8), &account, &query_request())
        .unwrap();
    assert_eq!(observed.external_advisory, Some(budget(6)));
    assert_eq!(observed.remaining, Some(budget(1_000)));
    assert_eq!(observed.consumed, Some(budget(0)));
}

#[tokio::test]
async fn provider_failures_are_durable_and_expose_no_raw_secret_or_result() {
    use nlos_driver_llm_http::HttpClient;
    use nlos_driver_llm_http::llm_gateway::ReconciliationStatus;
    use std::time::Duration;
    let (hanging, unblock) = Reply::response(200, VALID_BODY).held();
    let bodies = [
        hanging,
        Reply {
            bytes: Some(b"HTTP/1.1 200 Test\r\nContent-Length: 1000\r\n\r\nshort".to_vec()),
            permit: None,
        },
        Reply::response(200, "invalid upstream JSON synthetic-credential"),
        Reply::response(
            200,
            r#"{"model":"unit-model","choices":[{"message":{"content":"no usage"}}]}"#,
        ),
        Reply::response(200, &VALID_BODY.replace('答', "synthetic-credential")),
    ];
    let (port, mut hits, server) = endpoint(Vec::from(bodies)).await;
    for index in 0..5 {
        let path = TestRoot::new();
        let policy = Arc::new(Policy(AtomicBool::new(true)));
        let clock = Arc::new(AtomicU64::new(10_000));
        let mut cfg = config(port, Some(policy), clock);
        cfg.shim = cfg
            .shim
            .with_http_client(HttpClient::with_timeout(Duration::from_millis(250)));
        let gateway = Gateway::initialize(&path.0, cfg).unwrap();
        let account = gateway.provision_root(root(), budget(1_000)).unwrap();
        let id = u8::try_from(index + 1).unwrap();
        let error = gateway
            .llm_chat(&call(id, 1), &account, &chat_request())
            .await
            .unwrap_err();
        assert!(!format!("{error:?} {error}").contains("synthetic-credential"));
        let GatewayError::EffectUnknown(operation) = error else {
            panic!("expected uncertainty in case {index}, got {error:?}")
        };
        let observed = gateway.reconciliation(operation).unwrap().unwrap();
        assert_eq!(observed.provider_attempts, Some(1));
        assert_eq!(observed.status, ReconciliationStatus::EffectUnknown);
        assert_eq!(observed.observed, None);
        assert_eq!(observed.provider_response_fingerprint.is_some(), index >= 2);
        assert_eq!(
            gateway
                .llm_chat(&call(id, 2), &account, &chat_request())
                .await
                .unwrap_err(),
            error
        );
        wait_hit(&mut hits, index).await;
        assert_eq!(
            gateway
                .query_budget(&call(11, 1), &account, &query_request())
                .unwrap()
                .consumed,
            Some(budget(0))
        );
        drop(gateway);
        let journal = std::fs::read(path.0.join("llm-gateway.wal")).unwrap();
        assert!(
            !journal
                .windows("synthetic-credential".len())
                .any(|window| window == b"synthetic-credential")
        );
        assert!(
            !journal
                .windows("汉字🦀".len())
                .any(|window| window == "汉字🦀".as_bytes())
        );
    }
    unblock.add_permits(1);
    assert!(hits.try_recv().is_err());
    server.abort();
}

#[tokio::test]
async fn failover_tracks_transport_attempts_separately_from_one_logical_call() {
    use nlos_driver_llm_http::{
        DirectKeyPoolConfig, DirectKeyPoolScheduler, DirectKeySlotConfig, KeySelectionStrategy,
        LlmEnforcementShim, SecretCredential,
    };
    let (port, mut hits, server) = endpoint(vec![
        Reply::response(429, "provider throttled first attempt"),
        Reply::response(200, VALID_BODY),
    ])
    .await;
    let path = TestRoot::new();
    let clock = Arc::new(AtomicU64::new(10_000));
    let policy = Arc::new(Policy(AtomicBool::new(true)));
    let mut cfg = config(port, Some(policy), clock);
    let slots = ["synthetic-credential-a", "synthetic-credential-b"]
        .into_iter()
        .map(|key| DirectKeySlotConfig::new(key, "fixture", SecretCredential::new(key)))
        .collect();
    let pool = DirectKeyPoolScheduler::new(DirectKeyPoolConfig::new(
        format!("http://127.0.0.1:{port}/v1"),
        KeySelectionStrategy::RoundRobin,
        slots,
    ))
    .unwrap();
    cfg.shim = LlmEnforcementShim::from_direct_pool(
        Arc::new(pool),
        "unit-model",
        "unit-model",
        Vec::new(),
    );
    let gateway = Gateway::initialize(&path.0, cfg).unwrap();
    let account = gateway.provision_root(root(), budget(1_000)).unwrap();
    let reply = gateway
        .llm_chat(&call(1, 1), &account, &chat_request())
        .await
        .unwrap();
    wait_hit(&mut hits, 0).await;
    wait_hit(&mut hits, 1).await;
    assert_eq!(reply.provider_attempts, 2);
    assert_eq!(reply.result.usage.unwrap().api_calls, 1);
    assert_eq!(
        gateway
            .reconciliation(reply.operation)
            .unwrap()
            .unwrap()
            .durable_attempt_fences,
        2
    );
    assert!(hits.try_recv().is_err());
    server.abort();
}

#[tokio::test]
async fn settlement_rejects_over_quote_and_preserves_authority_envelope_failure() {
    use nlos_driver_llm_http::llm_gateway::ReconciliationStatus;
    use nlos_resource::{ResourceAuthority, UpdateLlmTokenBudgetRequest};
    let over_quote = VALID_BODY
        .replace(r#""prompt_tokens":5"#, r#""prompt_tokens":55"#)
        .replace(r#""total_tokens":9"#, r#""total_tokens":59"#);
    let (held, unblock) = Reply::response(200, VALID_BODY).held();
    let (port, mut hits, server) = endpoint(vec![Reply::response(200, &over_quote), held]).await;
    let path = TestRoot::new();
    let policy = Arc::new(Policy(AtomicBool::new(true)));
    let clock = Arc::new(AtomicU64::new(10_000));
    let gateway =
        Arc::new(Gateway::initialize(&path.0, config(port, Some(policy), clock)).unwrap());
    let account = gateway.provision_root(root(), budget(1_000)).unwrap();
    let err = gateway
        .llm_chat(&call(1, 1), &account, &chat_request())
        .await
        .unwrap_err();
    wait_hit(&mut hits, 0).await;
    let GatewayError::SettlementRejected(over_quote_op) = err else {
        panic!("observation exceeded quote")
    };
    let first = gateway.reconciliation(over_quote_op).unwrap().unwrap();
    assert_eq!(first.status, ReconciliationStatus::OverQuote);
    assert_eq!(first.quote.prompt_tokens, 26);
    assert_eq!(first.quote.cost_microcredits, 16);
    let actual = first.observed.unwrap();
    assert_eq!(
        (
            actual.api_calls,
            actual.prompt_tokens,
            actual.completion_tokens,
            actual.total_tokens,
            actual.cost_microcredits
        ),
        (1, 55, 1, 59, 17)
    );
    assert!(first.provider_response_fingerprint.is_some());
    assert_eq!(first.provider_attempts, Some(1));

    let task = begin_chat(&gateway, call(2, 1), account);
    wait_hit(&mut hits, 1).await;
    let authority = ResourceAuthority::open(&path.0).unwrap();
    let hard = authority
        .inspect_llm_token_budget(account.account_id())
        .unwrap();
    let mut lower = hard.limits;
    lower.max_prompt_tokens = 1;
    authority
        .update_llm_token_budget(UpdateLlmTokenBudgetRequest {
            account_id: account.account_id(),
            expected_generation: hard.generation,
            limits: lower,
            idempotency_key: nlos_types::IdempotencyKey::from_bytes([55; 16]),
            updated_at_ms: 10_000,
        })
        .unwrap();
    unblock.add_permits(1);
    let err = task.await.unwrap().unwrap_err();
    let GatewayError::SettlementRejected(over_envelope_op) = err else {
        panic!("authority rejected usage")
    };
    let rejected = gateway.reconciliation(over_envelope_op).unwrap().unwrap();
    assert_eq!(rejected.status, ReconciliationStatus::OverAuthorityEnvelope);
    assert_eq!(rejected.observed.unwrap().prompt_tokens, 5);
    assert!(rejected.provider_response_fingerprint.is_some());
    assert_eq!(
        authority
            .list_llm_token_consumptions(account.account_id())
            .unwrap(),
        []
    );
    assert!(hits.try_recv().is_err());
    server.abort();
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn actual_encoded_payload_limits_accept_exact_boundary_and_reject_one_more_byte() {
    use nlos_schema::sabi::v1::LlmChatResult;
    use nlos_schema::{
        CompatibilityError, MAX_LLM_GATEWAY_PAYLOAD_BYTES, encode_llm_chat_request,
        encode_llm_chat_result,
    };
    let priced = nlos_schema::sabi::v1::LlmUsageWire {
        api_calls: 1,
        prompt_tokens: 5,
        completion_tokens: 1,
        total_tokens: 9,
        cost_microcredits: 5,
    };
    let mut result = LlmChatResult {
        schema: Some(llm_gateway_schema_identity()),
        model: "unit-model".into(),
        content_utf8: "x".repeat(64_000),
        reasoning_utf8: "算".into(),
        usage: Some(priced),
    };
    let remaining = MAX_LLM_GATEWAY_PAYLOAD_BYTES - encode_llm_chat_result(&result).unwrap().len();
    result.content_utf8.push_str(&"x".repeat(remaining));
    assert_eq!(
        encode_llm_chat_result(&result).unwrap().len(),
        MAX_LLM_GATEWAY_PAYLOAD_BYTES
    );
    let exact_payload = VALID_BODY.replace('答', &result.content_utf8);
    result.content_utf8.push('x');
    assert!(matches!(
        encode_llm_chat_result(&result),
        Err(CompatibilityError::FrameTooLarge { .. })
    ));
    let oversize_payload = VALID_BODY.replace('答', &result.content_utf8);
    let (port, mut hits, server) = endpoint(vec![
        Reply::response(200, &exact_payload),
        Reply::response(200, &oversize_payload),
    ])
    .await;
    let path = TestRoot::new();
    let policy = Arc::new(Policy(AtomicBool::new(true)));
    let clock = Arc::new(AtomicU64::new(10_000));
    let gateway = Gateway::initialize(&path.0, config(port, Some(policy), clock)).unwrap();
    let account = gateway.provision_root(root(), budget(300_000)).unwrap();
    let mut request = chat_request();
    request.turns[0].content = "x".repeat(64_000);
    let remaining =
        MAX_LLM_GATEWAY_PAYLOAD_BYTES - encode_llm_chat_request(&request).unwrap().len();
    request.turns[0].content.push_str(&"x".repeat(remaining));
    assert_eq!(
        encode_llm_chat_request(&request).unwrap().len(),
        MAX_LLM_GATEWAY_PAYLOAD_BYTES
    );
    let named_result = gateway
        .llm_chat(&call(1, 1), &account, &request)
        .await
        .unwrap();
    assert_eq!(
        encode_llm_chat_result(&named_result.result).unwrap().len(),
        MAX_LLM_GATEWAY_PAYLOAD_BYTES
    );
    wait_hit(&mut hits, 0).await;
    request.turns[0].content.push('x');
    assert_eq!(
        gateway
            .llm_chat(&call(2, 1), &account, &request)
            .await
            .unwrap_err(),
        GatewayError::FrameTooLarge
    );
    let oversized = gateway
        .llm_chat(&call(3, 1), &account, &chat_request())
        .await
        .unwrap_err();
    assert!(matches!(oversized, GatewayError::FrameTooLarge));
    wait_hit(&mut hits, 1).await;
    assert_eq!(
        gateway
            .query_budget(&call(4, 1), &account, &query_request())
            .unwrap()
            .consumed
            .unwrap()
            .api_calls,
        1
    );
    assert!(hits.try_recv().is_err());
    server.abort();
}

#[tokio::test]
async fn missing_prices_zero_output_and_overflow_reject_before_tcp() {
    use nlos_driver_llm_http::llm_gateway::{ModelAccounting, ModelPrice, PriceRegistry};
    let (port, hits, server) = provider().await;
    let path = TestRoot::new();
    let policy = Arc::new(Policy(AtomicBool::new(true)));
    let clock = Arc::new(AtomicU64::new(10_000));
    let mut cfg = config(port, Some(policy), clock);
    cfg.prices = PriceRegistry::new([ModelPrice {
        model: "unit-model".into(),
        version: 9,
        max_output_tokens: 64,
        accounting: ModelAccounting::NoReasoning,
        prompt_microcredits_per_kilotoken: u64::MAX,
        output_microcredits_per_kilotoken: u64::MAX,
    }])
    .unwrap();
    let gateway = Gateway::initialize(&path.0, cfg).unwrap();
    let account = gateway.provision_root(root(), budget(300_000)).unwrap();
    let mut request = chat_request();
    request.model = "unknown".into();
    assert_eq!(
        gateway
            .llm_chat(&call(1, 1), &account, &request)
            .await
            .unwrap_err(),
        GatewayError::UnsupportedModel
    );
    request.model = "unit-model".into();
    for max in [0, 65, 16_385] {
        request.max_output_tokens = max;
        assert_eq!(
            gateway
                .llm_chat(&call(1, 1), &account, &request)
                .await
                .unwrap_err(),
            GatewayError::InvalidRequest
        );
    }
    request.max_output_tokens = 64;
    request.turns[0].content = "字".repeat(5_000);
    assert_eq!(
        gateway
            .llm_chat(&call(1, 1), &account, &request)
            .await
            .unwrap_err(),
        GatewayError::Arithmetic
    );
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    server.abort();
}

#[tokio::test]
async fn authority_ancestor_shrink_after_dispatch_rejects_child_settlement() {
    use nlos_driver_llm_http::llm_gateway::ReconciliationStatus;
    use nlos_resource::{ResourceAuthority, UpdateLlmTokenBudgetRequest};
    let (held, unblock) = Reply::response(200, VALID_BODY).held();
    let (port, mut hits, server) = endpoint(vec![held]).await;
    let path = TestRoot::new();
    let policy = Arc::new(Policy(AtomicBool::new(true)));
    let clock = Arc::new(AtomicU64::new(10_000));
    let gateway =
        Arc::new(Gateway::initialize(&path.0, config(port, Some(policy), clock)).unwrap());
    let parent = gateway.provision_root(root(), budget(1_000)).unwrap();
    let child = ChildSessionBinding {
        recipient: call(1, 15).caller,
        stable_identity: [1; 16],
        delegated: false,
    };
    let child = gateway
        .attenuate_budget(
            &call(1, 1),
            &parent,
            Some(&child),
            &child_limit(100, 15_000),
        )
        .unwrap()
        .child
        .account;
    let task = begin_chat(&gateway, call(2, 15), child);
    wait_hit(&mut hits, 0).await;
    let authority = ResourceAuthority::open(&path.0).unwrap();
    let hard = authority
        .inspect_llm_token_budget(parent.account_id())
        .unwrap();
    let mut lower = hard.limits;
    lower.max_prompt_tokens = 1;
    authority
        .update_llm_token_budget(UpdateLlmTokenBudgetRequest {
            account_id: parent.account_id(),
            expected_generation: hard.generation,
            limits: lower,
            idempotency_key: nlos_types::IdempotencyKey::from_bytes([65; 16]),
            updated_at_ms: 10_000,
        })
        .unwrap();
    unblock.add_permits(1);
    let err = task.await.unwrap().unwrap_err();
    let GatewayError::SettlementRejected(operation) = err else {
        panic!("expected authority rejection")
    };
    assert_eq!(
        gateway.reconciliation(operation).unwrap().unwrap().status,
        ReconciliationStatus::OverAuthorityEnvelope
    );
    assert_eq!(
        authority
            .list_llm_token_consumptions(child.account_id())
            .unwrap(),
        []
    );
    server.abort();
}

#[tokio::test]
async fn revocation_after_tcp_denies_result_but_preserves_exact_settlement() {
    let (held, unblock) = Reply::response(200, VALID_BODY).held();
    let (port, mut hits, server) = endpoint(vec![held]).await;
    let path = TestRoot::new();
    let policy = Arc::new(Policy(AtomicBool::new(true)));
    let clock = Arc::new(AtomicU64::new(10_000));
    let gateway =
        Arc::new(Gateway::initialize(&path.0, config(port, Some(policy.clone()), clock)).unwrap());
    let account = gateway.provision_root(root(), budget(1_000)).unwrap();
    let task = begin_chat(&gateway, call(12, 1), account);
    wait_hit(&mut hits, 0).await;
    policy.0.store(false, Ordering::SeqCst);
    unblock.add_permits(1);
    assert_eq!(
        task.await.unwrap().unwrap_err(),
        GatewayError::Authorization(AuthorizationError::Denied)
    );
    let authority = nlos_resource::ResourceAuthority::open(&path.0).unwrap();
    let receipts = authority
        .list_llm_token_consumptions(account.account_id())
        .unwrap();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].total_tokens, 9);
    assert_eq!(
        gateway
            .llm_chat(&call(12, 2), &account, &chat_request())
            .await
            .unwrap_err(),
        GatewayError::Authorization(AuthorizationError::Denied)
    );
    policy.0.store(true, Ordering::SeqCst);
    let replay = gateway
        .llm_chat(&call(12, 3), &account, &chat_request())
        .await
        .unwrap();
    assert_eq!(replay.receipt, receipts[0].receipt_id);
    assert_eq!(replay.result.content_utf8, "答");
    assert!(hits.try_recv().is_err());
    server.abort();
}

#[test]
fn concurrent_increasing_clock_samples_do_not_invalidate_an_authorized_query() {
    use std::sync::{Mutex, mpsc};
    use std::thread;
    use std::time::Duration;

    let root_dir = TestRoot::new();
    let policy = Arc::new(Policy(AtomicBool::new(true)));
    let mut cfg = config(1, Some(policy), Arc::new(AtomicU64::new(10_000)));
    let pause_first_sample = Arc::new(AtomicBool::new(false));
    let next_monotonic_ns = AtomicU64::new(100);
    let (sampled, first_sampled) = mpsc::channel();
    let (release, resume) = mpsc::channel();
    let resume = Mutex::new(resume);
    cfg.time = {
        let pause_first_sample = pause_first_sample.clone();
        Arc::new(move || {
            let monotonic_ns = next_monotonic_ns.fetch_add(1, Ordering::SeqCst);
            if pause_first_sample.swap(false, Ordering::SeqCst) {
                sampled.send(()).unwrap();
                resume
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(10))
                    .unwrap();
            }
            Ok(ServiceTime {
                wall_ms: 10_000,
                monotonic_ns,
            })
        })
    };
    let gateway = Gateway::initialize(&root_dir.0, cfg).unwrap();
    let account = gateway.provision_root(root(), budget(1_000)).unwrap();
    pause_first_sample.store(true, Ordering::SeqCst);

    thread::scope(|scope| {
        let first = scope.spawn(|| gateway.query_budget(&call(1, 1), &account, &query_request()));
        first_sampled.recv_timeout(Duration::from_secs(10)).unwrap();
        let (second_entering, entered) = mpsc::channel();
        let (second_finished, finished) = mpsc::channel();
        let run_second = || gateway.query_budget(&call(2, 2), &account, &query_request());
        let second = scope.spawn(move || {
            second_entering.send(()).unwrap();
            let result = run_second();
            second_finished.send(()).unwrap();
            result
        });
        entered.recv_timeout(Duration::from_secs(10)).unwrap();
        // The original implementation lets the second query publish its later
        // samples while the first sample is still waiting in the callback.
        // A serialized sampler waits for release, so the bounded wait ends first.
        let _ = finished.recv_timeout(Duration::from_secs(1));
        release.send(()).unwrap();
        assert_eq!(
            second.join().unwrap().unwrap().remaining,
            Some(budget(1_000))
        );
        assert_eq!(
            first.join().unwrap().unwrap().remaining,
            Some(budget(1_000))
        );
    });
}

#[tokio::test]
async fn actual_clock_regressions_callback_errors_and_expired_deadlines_emit_no_tcp() {
    let (port, hits, server) = provider().await;
    let root_dir = TestRoot::new();
    let policy = Arc::new(Policy(AtomicBool::new(true)));
    let wall = Arc::new(AtomicU64::new(10_000));
    let monotonic = Arc::new(AtomicU64::new(100));
    let available = Arc::new(AtomicBool::new(true));
    let mut cfg = config(port, Some(policy.clone()), wall.clone());
    cfg.time = {
        let wall = wall.clone();
        let monotonic = monotonic.clone();
        let available = available.clone();
        Arc::new(move || {
            if !available.load(Ordering::SeqCst) {
                return Err(AuthorizationError::TimeUnavailable);
            }
            Ok(ServiceTime {
                wall_ms: wall.load(Ordering::SeqCst),
                monotonic_ns: monotonic.load(Ordering::SeqCst),
            })
        })
    };
    let gateway = Gateway::initialize(&root_dir.0, cfg).unwrap();
    let account = gateway.provision_root(root(), budget(1_000)).unwrap();
    let mut caller = call(1, 1);
    let time_unavailable = GatewayError::Authorization(AuthorizationError::TimeUnavailable);

    wall.store(9_999, Ordering::SeqCst);
    assert_eq!(
        gateway
            .llm_chat(&caller, &account, &chat_request())
            .await
            .unwrap_err(),
        time_unavailable
    );
    wall.store(10_001, Ordering::SeqCst);
    monotonic.store(99, Ordering::SeqCst);
    assert_eq!(
        gateway
            .llm_chat(&caller, &account, &chat_request())
            .await
            .unwrap_err(),
        time_unavailable
    );
    wall.store(10_000, Ordering::SeqCst);
    monotonic.store(100, Ordering::SeqCst);
    assert_eq!(
        gateway
            .query_budget(&caller, &account, &query_request())
            .unwrap()
            .remaining,
        Some(budget(1_000))
    );

    available.store(false, Ordering::SeqCst);
    assert_eq!(
        gateway
            .llm_chat(&caller, &account, &chat_request())
            .await
            .unwrap_err(),
        time_unavailable
    );
    available.store(true, Ordering::SeqCst);
    caller.deadline_monotonic_ns = Some(100);
    assert_eq!(
        gateway
            .llm_chat(&caller, &account, &chat_request())
            .await
            .unwrap_err(),
        GatewayError::Authorization(AuthorizationError::DeadlineExpired)
    );
    caller.deadline_monotonic_ns = Some(101);
    policy.0.store(false, Ordering::SeqCst);
    assert_eq!(
        gateway
            .llm_chat(&caller, &account, &chat_request())
            .await
            .unwrap_err(),
        GatewayError::Authorization(AuthorizationError::Denied)
    );
    assert_eq!(hits.load(Ordering::SeqCst), 0);
    server.abort();
}
