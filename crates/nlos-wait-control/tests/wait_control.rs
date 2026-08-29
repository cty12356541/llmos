//! Conformance for the typed `WaitControl` local IPC adapter.
//!
//! The matrix mirrors the system-control suite: one real-transport (tokio
//! duplex) roundtrip per mutating method, the bounded failure-envelope
//! mappings for every rejection class, and a durable-restart enumeration.
//! A further `#[cfg(unix)]` test crosses a real `UnixListenerAdapter`
//! endpoint exactly like the system-control Unix test.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use nlos_channel::{ChannelAuthority, ChannelDecision, ChannelRecord, CreateChannelRequest};
use nlos_ipc::{
    LocalRpcClient, OutboundResponse, PeerAuthorizer, PeerIdentity, TransportConfig, serve_one,
};
use nlos_schema::sabi::v1::{
    CallerIdentity, CapabilityHandle, Envelope, ExchangeRequest, ExchangeResponse, RetryDirective,
    SabiErrorCode, SabiRequestContext, SchemaIdentity, envelope,
};
use nlos_schema::{
    MethodSemantics, SABI_ENVELOPE_SCHEMA, ValidatedExchangeResponse,
    validate_sabi_response_context,
};
use nlos_types::IdempotencyKey;
use nlos_wait::{
    BindingId, CancelWaitRequest as AuthorityCancelWaitRequest,
    NotifyCommitsRequest as AuthorityNotifyCommitsRequest,
    RegisterWaitRequest as AuthorityRegisterWaitRequest, WaitAuthority, WaitState,
};
use nlos_wait_control::{
    CANCEL_WAIT_METHOD, INSPECT_WAIT_METHOD, LIST_WAITS_METHOD, NOTIFY_COMMITS_METHOD,
    REGISTER_WAIT_METHOD, WAIT_CONTROL_SERVICE, WaitControlAuthorizer, WaitControlService,
    decode_cancel_wait_result, decode_inspect_wait_result, decode_list_waits_result,
    decode_notify_commits_result, decode_register_wait_result, encode_cancel_wait_request,
    encode_inspect_wait_request, encode_list_waits_request, encode_notify_commits_request,
    encode_register_wait_request, payload, wait_control_schema_identity,
};
use prost::Message as _;
use tokio::io::duplex;

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

fn key(seed: u8) -> IdempotencyKey {
    IdempotencyKey::from_bytes([seed; 16])
}

fn binding(seed: u8) -> BindingId {
    BindingId::from_bytes([seed; 16])
}

struct Root(PathBuf);

impl Root {
    fn new(label: &str) -> Self {
        let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!(
            "nlos-wait-control-{label}-{}-{sequence}",
            std::process::id()
        )))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Pair {
    channel: Arc<ChannelAuthority>,
    wait: Arc<WaitAuthority>,
}

fn open_pair(root: &Root) -> Pair {
    let channel = Arc::new(ChannelAuthority::open(root.path()).expect("open channel authority"));
    let wait = Arc::new(WaitAuthority::open(root.path(), Arc::clone(&channel)).expect("open wait"));
    Pair { channel, wait }
}

fn create_channel(pair: &Pair, seed: u8) -> ChannelRecord {
    match pair
        .channel
        .create_channel(CreateChannelRequest {
            capacity_bytes: 4_096,
            policy_digest: [0x44; 32],
            idempotency_key: key(seed),
            created_at_ms: 900,
        })
        .expect("create channel")
    {
        ChannelDecision::Created(record) => record,
        ChannelDecision::Replayed(_) => panic!("fresh create cannot replay"),
    }
}

fn authority_register(
    pair: &Pair,
    channel: &ChannelRecord,
    binding_seed: u8,
    target: u64,
    key_seed: u8,
) {
    pair.wait
        .register_wait(AuthorityRegisterWaitRequest {
            binding: binding(binding_seed),
            channel_id: channel.channel_id,
            target_sequence: target,
            idempotency_key: key(key_seed),
            registered_at_ms: 1_000,
        })
        .expect("register wait");
}

struct AllowPeer;

impl PeerAuthorizer for AllowPeer {
    fn authorize(&self, _: &PeerIdentity) -> Result<(), String> {
        Ok(())
    }
}

/// The same capability posture the system-control suite uses: exactly one
/// known capability handle authorizes; anything else is a policy denial.
struct AllowCapability;

impl WaitControlAuthorizer for AllowCapability {
    fn authorize_register_wait(
        &self,
        context: &SabiRequestContext,
        _: &payload::RegisterWaitRequest,
    ) -> Result<(), &'static str> {
        authorize(context)
    }

    fn authorize_notify_commits(
        &self,
        context: &SabiRequestContext,
        _: &payload::NotifyCommitsRequest,
    ) -> Result<(), &'static str> {
        authorize(context)
    }

    fn authorize_cancel_wait(
        &self,
        context: &SabiRequestContext,
        _: &payload::CancelWaitRequest,
    ) -> Result<(), &'static str> {
        authorize(context)
    }

    fn authorize_list_waits(
        &self,
        context: &SabiRequestContext,
        _: &payload::ListWaitsRequest,
    ) -> Result<(), &'static str> {
        authorize(context)
    }

    fn authorize_inspect_wait(
        &self,
        context: &SabiRequestContext,
        _: &payload::InspectWaitRequest,
    ) -> Result<(), &'static str> {
        authorize(context)
    }
}

fn authorize(context: &SabiRequestContext) -> Result<(), &'static str> {
    if context.capability_handles
        == [CapabilityHandle {
            slot: 9,
            generation: 1,
        }]
    {
        Ok(())
    } else {
        Err("missing wait control capability")
    }
}

/// A custom denial authorizer: every method is rejected with a static local
/// policy class that must never cross the IPC boundary.
struct DenyAll;

impl WaitControlAuthorizer for DenyAll {
    fn authorize_register_wait(
        &self,
        _: &SabiRequestContext,
        _: &payload::RegisterWaitRequest,
    ) -> Result<(), &'static str> {
        Err("denied by test policy")
    }

    fn authorize_notify_commits(
        &self,
        _: &SabiRequestContext,
        _: &payload::NotifyCommitsRequest,
    ) -> Result<(), &'static str> {
        Err("denied by test policy")
    }

    fn authorize_cancel_wait(
        &self,
        _: &SabiRequestContext,
        _: &payload::CancelWaitRequest,
    ) -> Result<(), &'static str> {
        Err("denied by test policy")
    }

    fn authorize_list_waits(
        &self,
        _: &SabiRequestContext,
        _: &payload::ListWaitsRequest,
    ) -> Result<(), &'static str> {
        Err("denied by test policy")
    }

    fn authorize_inspect_wait(
        &self,
        _: &SabiRequestContext,
        _: &payload::InspectWaitRequest,
    ) -> Result<(), &'static str> {
        Err("denied by test policy")
    }
}

fn request_context(idempotency_key: Vec<u8>) -> SabiRequestContext {
    SabiRequestContext {
        caller: Some(CallerIdentity {
            principal_id: vec![0x31; 16],
            application_id: vec![0x32; 16],
            process_id: vec![0x33; 16],
            process_generation: 1,
        }),
        activity_context: Vec::new(),
        task_execution_binding: None,
        correlation_id: vec![0x34; 16],
        idempotency_key,
        deadline_monotonic_ns: 0,
        capability_handles: vec![CapabilityHandle {
            slot: 9,
            generation: 1,
        }],
        reservation_handle: None,
        proposal_or_input_digest_sha256: Vec::new(),
    }
}

fn envelope(method: &str, context: SabiRequestContext, payload_bytes: Vec<u8>) -> Envelope {
    Envelope {
        schema: Some(SchemaIdentity {
            name: SABI_ENVELOPE_SCHEMA.to_owned(),
            major: 1,
            minor: 1,
            critical_extension_ids: Vec::new(),
            non_critical_extension_ids: Vec::new(),
        }),
        request_id: vec![0x35; 16],
        service: WAIT_CONTROL_SERVICE.to_owned(),
        method: method.to_owned(),
        common_context: Some(envelope::CommonContext::RequestContext(context)),
        payload: payload_bytes,
    }
}

fn register_payload(
    channel: &ChannelRecord,
    binding_seed: u8,
    target: u64,
    key_seed: u8,
) -> Vec<u8> {
    encode_register_wait_request(&payload::RegisterWaitRequest {
        schema: Some(wait_control_schema_identity()),
        binding: binding(binding_seed).as_bytes().to_vec(),
        channel_id: channel.channel_id.as_bytes().to_vec(),
        target_sequence: target,
        idempotency_key: key(key_seed).as_bytes().to_vec(),
        registered_at_ms: 1_000,
    })
    .unwrap()
}

fn notify_payload(channel: &ChannelRecord, up_to: u64, key_seed: u8) -> Vec<u8> {
    encode_notify_commits_request(&payload::NotifyCommitsRequest {
        schema: Some(wait_control_schema_identity()),
        channel_id: channel.channel_id.as_bytes().to_vec(),
        up_to_sequence: up_to,
        notified_at_ms: 2_000,
        idempotency_key: key(key_seed).as_bytes().to_vec(),
    })
    .unwrap()
}

fn cancel_payload(wait_id: &[u8; 16], key_seed: u8) -> Vec<u8> {
    encode_cancel_wait_request(&payload::CancelWaitRequest {
        schema: Some(wait_control_schema_identity()),
        wait_id: wait_id.to_vec(),
        cancelled_at_ms: 3_000,
        idempotency_key: key(key_seed).as_bytes().to_vec(),
    })
    .unwrap()
}

fn list_payload() -> Vec<u8> {
    encode_list_waits_request(&payload::ListWaitsRequest {
        schema: Some(wait_control_schema_identity()),
        filter_channel_id: Vec::new(),
    })
    .unwrap()
}

fn inspect_payload(wait_id: &[u8; 16]) -> Vec<u8> {
    encode_inspect_wait_request(&payload::InspectWaitRequest {
        schema: Some(wait_control_schema_identity()),
        wait_id: wait_id.to_vec(),
    })
    .unwrap()
}

fn transport_config() -> TransportConfig {
    TransportConfig::new(
        64 * 1024,
        Duration::from_secs(1),
        Duration::from_secs(1),
        Duration::from_secs(1),
    )
    .unwrap()
}

/// Asserts the bounded failure-envelope shape and returns the typed code.
fn failure_of(response: &Envelope) -> (SabiErrorCode, RetryDirective) {
    assert!(
        response.payload.is_empty(),
        "failure must clear the payload"
    );
    let envelope::CommonContext::ResponseContext(context) =
        response.common_context.as_ref().expect("response context")
    else {
        panic!("failure envelope must carry a response context");
    };
    assert!(context.operation.is_none());
    assert!(context.receipts.is_empty());
    let failure = context.failure.as_ref().expect("typed failure");
    (
        SabiErrorCode::try_from(failure.code).unwrap(),
        RetryDirective::try_from(failure.retry).unwrap(),
    )
}

fn receipts_of(response: &Envelope) -> Vec<Vec<u8>> {
    let envelope::CommonContext::ResponseContext(context) =
        response.common_context.as_ref().expect("response context")
    else {
        panic!("expected a response context");
    };
    context
        .receipts
        .iter()
        .map(|receipt| receipt.receipt_id.clone())
        .collect()
}

/// Serves exactly one request over a tokio duplex transport with the given
/// service and returns the client-validated response.
async fn exchange_over_duplex(
    service: WaitControlService<AllowCapability>,
    request: ExchangeRequest,
) -> ValidatedExchangeResponse {
    let config = transport_config();
    let (client_stream, server_stream) = duplex(64 * 1024);
    let server = tokio::spawn(async move {
        serve_one(
            server_stream,
            config,
            PeerIdentity::InMemory,
            &AllowPeer,
            move |validated| {
                let response = service.handle_for_ipc(validated.envelope(), 10, 6_000);
                async move {
                    Ok(OutboundResponse::Typed(ExchangeResponse {
                        envelope: Some(response),
                    }))
                }
            },
        )
        .await
    });
    let response = LocalRpcClient::new(client_stream, transport_config())
        .exchange_validated(request)
        .await
        .unwrap();
    server.await.unwrap().unwrap();
    response
}

#[tokio::test]
async fn register_wait_crosses_real_ipc_and_replays() {
    let root = Root::new("register-replay");
    let pair = open_pair(&root);
    let channel = create_channel(&pair, 200);
    let request = ExchangeRequest {
        envelope: Some(envelope(
            REGISTER_WAIT_METHOD,
            request_context(key(1).as_bytes().to_vec()),
            register_payload(&channel, 1, 5, 1),
        )),
    };
    let service = WaitControlService::new(Arc::clone(&pair.wait), AllowCapability);
    let response = exchange_over_duplex(service, request.clone()).await;
    validate_sabi_response_context(response.envelope(), MethodSemantics::MUTATION).unwrap();
    let result = decode_register_wait_result(&response.envelope().payload).unwrap();
    assert!(!result.replayed);
    let record = result.record.expect("registered record");
    assert_eq!(
        payload::WaitStateCode::try_from(record.state).unwrap(),
        payload::WaitStateCode::Pending
    );
    assert_eq!(record.target_sequence, 5);
    assert_eq!(record.binding, binding(1).as_bytes().to_vec());
    assert_eq!(record.channel_id, channel.channel_id.as_bytes().to_vec());
    assert_eq!(
        receipts_of(response.envelope()),
        vec![record.wait_id.clone()]
    );

    // The exact same request replays the original durable row and its
    // receipt, never a second registration.
    let replay_service = WaitControlService::new(Arc::clone(&pair.wait), AllowCapability);
    let replay = replay_service
        .handle(request.envelope.as_ref().unwrap(), 10, 7_000)
        .unwrap();
    let replay_result = decode_register_wait_result(&replay.payload).unwrap();
    assert!(replay_result.replayed);
    assert_eq!(replay_result.record, Some(record));
}

#[tokio::test]
async fn notify_commits_crosses_real_ipc_and_returns_the_wake_report() {
    let root = Root::new("notify");
    let pair = open_pair(&root);
    let channel = create_channel(&pair, 201);
    authority_register(&pair, &channel, 1, 3, 1);
    authority_register(&pair, &channel, 2, 5, 2);
    let request = ExchangeRequest {
        envelope: Some(envelope(
            NOTIFY_COMMITS_METHOD,
            request_context(key(9).as_bytes().to_vec()),
            notify_payload(&channel, 5, 9),
        )),
    };
    let service = WaitControlService::new(Arc::clone(&pair.wait), AllowCapability);
    let response = exchange_over_duplex(service, request).await;
    validate_sabi_response_context(response.envelope(), MethodSemantics::MUTATION).unwrap();
    let report = decode_notify_commits_result(&response.envelope().payload).unwrap();
    assert_eq!(report.woken.len(), 2);
    let states: Vec<payload::WaitStateCode> = report
        .woken
        .iter()
        .map(|record| payload::WaitStateCode::try_from(record.state).unwrap())
        .collect();
    assert_eq!(
        states,
        vec![payload::WaitStateCode::Woken, payload::WaitStateCode::Woken]
    );
    assert_eq!(report.woken[0].target_sequence, 3);
    assert_eq!(report.woken[1].target_sequence, 5);
    assert_eq!(report.woken[1].woken_up_to_sequence, 5);
    // The receipt references the durable notify receipt (keyed by the
    // request idempotency key), present even though rows were flipped.
    assert_eq!(
        receipts_of(response.envelope()),
        vec![key(9).as_bytes().to_vec()]
    );
}

#[tokio::test]
async fn cancel_wait_crosses_real_ipc_and_replays() {
    let root = Root::new("cancel");
    let pair = open_pair(&root);
    let channel = create_channel(&pair, 202);
    authority_register(&pair, &channel, 1, 4, 1);
    let wait_id = pair.wait.inspect_channel_waits(channel.channel_id).unwrap()[0].wait_id;
    let request = ExchangeRequest {
        envelope: Some(envelope(
            CANCEL_WAIT_METHOD,
            request_context(key(2).as_bytes().to_vec()),
            cancel_payload(wait_id.as_bytes(), 2),
        )),
    };
    let service = WaitControlService::new(Arc::clone(&pair.wait), AllowCapability);
    let response = exchange_over_duplex(service, request.clone()).await;
    validate_sabi_response_context(response.envelope(), MethodSemantics::MUTATION).unwrap();
    let result = decode_cancel_wait_result(&response.envelope().payload).unwrap();
    assert!(!result.replayed);
    let record = result.record.expect("cancelled record");
    assert_eq!(
        payload::WaitStateCode::try_from(record.state).unwrap(),
        payload::WaitStateCode::Cancelled
    );
    assert_eq!(record.cancelled_at_ms, 3_000);
    assert_eq!(
        receipts_of(response.envelope()),
        vec![key(2).as_bytes().to_vec()]
    );

    let replay_service = WaitControlService::new(Arc::clone(&pair.wait), AllowCapability);
    let replay = replay_service
        .handle(request.envelope.as_ref().unwrap(), 10, 7_000)
        .unwrap();
    let replay_result = decode_cancel_wait_result(&replay.payload).unwrap();
    assert!(replay_result.replayed);
    assert_eq!(replay_result.record, Some(record));
}

#[tokio::test]
async fn list_waits_crosses_real_ipc_with_the_full_state_enumeration() {
    let root = Root::new("list");
    let pair = open_pair(&root);
    let channel = create_channel(&pair, 203);
    authority_register(&pair, &channel, 1, 1, 1);
    authority_register(&pair, &channel, 2, 2, 2);
    authority_register(&pair, &channel, 3, 3, 3);
    pair.wait
        .cancel_wait(AuthorityCancelWaitRequest {
            wait_id: pair.wait.inspect_channel_waits(channel.channel_id).unwrap()[0].wait_id,
            cancelled_at_ms: 3_000,
            idempotency_key: key(4),
        })
        .unwrap();
    pair.wait
        .notify_commits(AuthorityNotifyCommitsRequest {
            channel_id: channel.channel_id,
            // Only the target-2 wait is woken; target 3 stays PENDING so the
            // enumeration covers all three states.
            up_to_sequence: 2,
            notified_at_ms: 2_000,
            idempotency_key: key(5),
        })
        .unwrap();
    let request = ExchangeRequest {
        envelope: Some(envelope(
            LIST_WAITS_METHOD,
            request_context(Vec::new()),
            list_payload(),
        )),
    };
    let service = WaitControlService::new(Arc::clone(&pair.wait), AllowCapability);
    let response = exchange_over_duplex(service, request).await;
    validate_sabi_response_context(response.envelope(), MethodSemantics::QUERY).unwrap();
    let result = decode_list_waits_result(&response.envelope().payload).unwrap();
    assert_eq!(result.waits.len(), 3);
    let states: Vec<payload::WaitStateCode> = result
        .waits
        .iter()
        .map(|record| payload::WaitStateCode::try_from(record.state).unwrap())
        .collect();
    assert_eq!(
        states,
        vec![
            payload::WaitStateCode::Cancelled,
            payload::WaitStateCode::Woken,
            payload::WaitStateCode::Pending,
        ]
    );
}

#[tokio::test]
async fn inspect_wait_crosses_real_ipc_and_returns_the_durable_row() {
    let root = Root::new("inspect");
    let pair = open_pair(&root);
    let channel = create_channel(&pair, 204);
    authority_register(&pair, &channel, 1, 6, 1);
    let wait_id = pair.wait.inspect_channel_waits(channel.channel_id).unwrap()[0].wait_id;
    let request = ExchangeRequest {
        envelope: Some(envelope(
            INSPECT_WAIT_METHOD,
            request_context(Vec::new()),
            inspect_payload(wait_id.as_bytes()),
        )),
    };
    let service = WaitControlService::new(Arc::clone(&pair.wait), AllowCapability);
    let response = exchange_over_duplex(service, request).await;
    validate_sabi_response_context(response.envelope(), MethodSemantics::QUERY).unwrap();
    let result = decode_inspect_wait_result(&response.envelope().payload).unwrap();
    let record = result.record.expect("inspected record");
    assert_eq!(record.wait_id, wait_id.as_bytes().to_vec());
    assert_eq!(record.target_sequence, 6);
    assert_eq!(
        payload::WaitStateCode::try_from(record.state).unwrap(),
        payload::WaitStateCode::Pending
    );
    assert_eq!(record.channel_fencing_token.len(), 32);
}

#[test]
fn unknown_method_maps_to_a_bounded_not_supported_failure() {
    let root = Root::new("unknown-method");
    let pair = open_pair(&root);
    let service = WaitControlService::new(Arc::clone(&pair.wait), AllowCapability);
    let response = service.handle_for_ipc(
        &envelope("frobnicate", request_context(Vec::new()), vec![0xaa, 0xbb]),
        10,
        6_000,
    );
    assert_eq!(
        failure_of(&response),
        (SabiErrorCode::NotSupported, RetryDirective::DoNotRetry)
    );
    assert_eq!(response.method, "frobnicate");
    assert_eq!(response.service, WAIT_CONTROL_SERVICE);

    let foreign_service = {
        let mut request = envelope(
            LIST_WAITS_METHOD,
            request_context(Vec::new()),
            list_payload(),
        );
        request.service = "other_service".to_owned();
        request
    };
    let response = service.handle_for_ipc(&foreign_service, 10, 6_000);
    assert_eq!(
        failure_of(&response),
        (SabiErrorCode::NotSupported, RetryDirective::DoNotRetry)
    );
}

#[test]
fn tampered_payload_maps_to_a_bounded_invalid_argument_failure() {
    let root = Root::new("tamper");
    let pair = open_pair(&root);
    let channel = create_channel(&pair, 205);
    let service = WaitControlService::new(Arc::clone(&pair.wait), AllowCapability);

    // Byte-level tampering: garbage that must not decode.
    let response = service.handle_for_ipc(
        &envelope(
            REGISTER_WAIT_METHOD,
            request_context(key(1).as_bytes().to_vec()),
            vec![0xff; 24],
        ),
        10,
        6_000,
    );
    assert_eq!(
        failure_of(&response),
        (SabiErrorCode::InvalidArgument, RetryDirective::DoNotRetry)
    );

    // A well-formed payload that violates the identity contract: a peer may
    // legally omit the optional schema field on the wire, so the tampered
    // frame is produced with raw prost encoding, past the local validation.
    let forged = payload::RegisterWaitRequest {
        schema: None,
        binding: binding(1).as_bytes().to_vec(),
        channel_id: channel.channel_id.as_bytes().to_vec(),
        target_sequence: 1,
        idempotency_key: key(1).as_bytes().to_vec(),
        registered_at_ms: 1_000,
    }
    .encode_to_vec();
    let response = service.handle_for_ipc(
        &envelope(
            REGISTER_WAIT_METHOD,
            request_context(key(1).as_bytes().to_vec()),
            forged,
        ),
        10,
        6_000,
    );
    assert_eq!(
        failure_of(&response),
        (SabiErrorCode::InvalidArgument, RetryDirective::DoNotRetry)
    );
    assert!(pair.wait.list_waits(None).unwrap().is_empty());
}

#[test]
fn authorizer_denial_maps_to_a_rights_failure_without_side_effects() {
    let root = Root::new("denied");
    let pair = open_pair(&root);
    let channel = create_channel(&pair, 206);
    let service = WaitControlService::new(Arc::clone(&pair.wait), DenyAll);
    for (method, context, payload_bytes) in [
        (
            REGISTER_WAIT_METHOD,
            request_context(key(1).as_bytes().to_vec()),
            register_payload(&channel, 1, 5, 1),
        ),
        (
            NOTIFY_COMMITS_METHOD,
            request_context(key(9).as_bytes().to_vec()),
            notify_payload(&channel, 5, 9),
        ),
        (
            LIST_WAITS_METHOD,
            request_context(Vec::new()),
            list_payload(),
        ),
    ] {
        let response = service.handle_for_ipc(&envelope(method, context, payload_bytes), 10, 6_000);
        assert_eq!(
            failure_of(&response),
            (SabiErrorCode::Rights, RetryDirective::DoNotRetry)
        );
    }
    // Denial precedes any durable effect.
    assert!(pair.wait.list_waits(None).unwrap().is_empty());
}

#[test]
fn cancel_failures_map_typed_not_found_and_state_failures() {
    let root = Root::new("cancel-failures");
    let pair = open_pair(&root);
    let channel = create_channel(&pair, 207);
    let service = WaitControlService::new(Arc::clone(&pair.wait), AllowCapability);

    // A well-formed but unknown wait token fails closed as NOT_FOUND.
    let unknown = service.handle_for_ipc(
        &envelope(
            CANCEL_WAIT_METHOD,
            request_context(key(1).as_bytes().to_vec()),
            cancel_payload(&[0x77; 16], 1),
        ),
        10,
        6_000,
    );
    assert_eq!(
        failure_of(&unknown),
        (SabiErrorCode::NotFound, RetryDirective::DoNotRetry)
    );

    // A wait that was already woken can never be cancelled: STATE.
    authority_register(&pair, &channel, 1, 2, 2);
    let wait_id = pair.wait.inspect_channel_waits(channel.channel_id).unwrap()[0].wait_id;
    pair.wait
        .notify_commits(AuthorityNotifyCommitsRequest {
            channel_id: channel.channel_id,
            up_to_sequence: 2,
            notified_at_ms: 2_000,
            idempotency_key: key(3),
        })
        .unwrap();
    let woken = service.handle_for_ipc(
        &envelope(
            CANCEL_WAIT_METHOD,
            request_context(key(4).as_bytes().to_vec()),
            cancel_payload(wait_id.as_bytes(), 4),
        ),
        10,
        6_000,
    );
    assert_eq!(
        failure_of(&woken),
        (SabiErrorCode::State, RetryDirective::DoNotRetry)
    );
    let record = pair.wait.inspect_wait(wait_id).unwrap();
    assert_eq!(record.state, WaitState::Woken);
}

#[test]
fn invalid_sabi_context_fails_closed_without_touching_the_authority() {
    let root = Root::new("context");
    let pair = open_pair(&root);
    let channel = create_channel(&pair, 208);
    let service = WaitControlService::new(Arc::clone(&pair.wait), AllowCapability);

    // A mutation without an idempotency key violates the common contract.
    let response = service.handle_for_ipc(
        &envelope(
            REGISTER_WAIT_METHOD,
            request_context(Vec::new()),
            register_payload(&channel, 1, 5, 1),
        ),
        10,
        6_000,
    );
    assert_eq!(
        failure_of(&response),
        (SabiErrorCode::InvalidArgument, RetryDirective::DoNotRetry)
    );

    // An expired deadline fails closed as DEADLINE (now = 10 > deadline = 1).
    let mut context = request_context(key(1).as_bytes().to_vec());
    context.deadline_monotonic_ns = 1;
    let response = service.handle_for_ipc(
        &envelope(LIST_WAITS_METHOD, context, list_payload()),
        10,
        6_000,
    );
    assert_eq!(
        failure_of(&response),
        (SabiErrorCode::Deadline, RetryDirective::DoNotRetry)
    );

    assert!(pair.wait.list_waits(None).unwrap().is_empty());
}

#[test]
fn idempotency_key_rebinding_is_a_conflict_before_any_effect() {
    let root = Root::new("key-mismatch");
    let pair = open_pair(&root);
    let channel = create_channel(&pair, 211);
    let service = WaitControlService::new(Arc::clone(&pair.wait), AllowCapability);
    // The payload key (seed 1) differs from the SABI context key (seed 2):
    // the mutation fails closed as CONFLICT and registers nothing.
    let response = service.handle_for_ipc(
        &envelope(
            REGISTER_WAIT_METHOD,
            request_context(key(2).as_bytes().to_vec()),
            register_payload(&channel, 1, 5, 1),
        ),
        10,
        6_000,
    );
    assert_eq!(
        failure_of(&response),
        (SabiErrorCode::Conflict, RetryDirective::DoNotRetry)
    );
    assert!(pair.wait.list_waits(None).unwrap().is_empty());
}

#[test]
fn list_waits_survives_an_authority_restart() {
    let root = Root::new("restart");
    let pair = open_pair(&root);
    let channel = create_channel(&pair, 209);
    authority_register(&pair, &channel, 1, 7, 1);
    authority_register(&pair, &channel, 2, 8, 2);
    let expected: Vec<Vec<u8>> = pair
        .wait
        .list_waits(None)
        .unwrap()
        .iter()
        .map(|record| record.wait_id.as_bytes().to_vec())
        .collect();
    assert_eq!(expected.len(), 2);

    // Drop the whole wait authority and reopen it against the same durable
    // root; the enumeration must still return the original rows.
    drop(pair.wait);
    let restarted = Arc::new(
        WaitAuthority::open(root.path(), Arc::clone(&pair.channel)).expect("reopen wait authority"),
    );
    let service = WaitControlService::new(restarted, AllowCapability);
    let response = service
        .handle(
            &envelope(
                LIST_WAITS_METHOD,
                request_context(Vec::new()),
                list_payload(),
            ),
            10,
            6_000,
        )
        .unwrap();
    let result = decode_list_waits_result(&response.payload).unwrap();
    let listed: Vec<Vec<u8>> = result
        .waits
        .iter()
        .map(|record| record.wait_id.clone())
        .collect();
    assert_eq!(listed, expected);
}

#[cfg(unix)]
#[tokio::test]
async fn register_wait_crosses_a_real_unix_socket() {
    use nlos_ipc::unix::{UnixListenerAdapter, connect};

    let root = Root::new("unix");
    let pair = open_pair(&root);
    let channel = create_channel(&pair, 210);
    let socket_path = root.path().join("wait-control.sock");
    let listener = UnixListenerAdapter::bind(&socket_path).unwrap();
    let service = WaitControlService::new(Arc::clone(&pair.wait), AllowCapability);
    let server = tokio::spawn(async move {
        let (stream, peer) = listener.accept(transport_config()).await?;
        assert!(matches!(peer, PeerIdentity::Unix { .. }));
        serve_one(
            stream,
            transport_config(),
            peer,
            &AllowPeer,
            move |validated| {
                let response = service.handle_for_ipc(validated.envelope(), 10, 6_000);
                async move {
                    Ok(OutboundResponse::Typed(ExchangeResponse {
                        envelope: Some(response),
                    }))
                }
            },
        )
        .await
    });
    let (stream, peer) = connect(&socket_path, transport_config()).await.unwrap();
    assert!(matches!(peer, PeerIdentity::Unix { .. }));
    let response = LocalRpcClient::new(stream, transport_config())
        .exchange_validated(ExchangeRequest {
            envelope: Some(envelope(
                REGISTER_WAIT_METHOD,
                request_context(key(1).as_bytes().to_vec()),
                register_payload(&channel, 1, 9, 1),
            )),
        })
        .await
        .unwrap();
    server.await.unwrap().unwrap();
    let result = decode_register_wait_result(&response.envelope().payload).unwrap();
    assert_eq!(
        payload::WaitStateCode::try_from(result.record.expect("record").state).unwrap(),
        payload::WaitStateCode::Pending
    );
    fs::remove_file(socket_path).unwrap();
}
