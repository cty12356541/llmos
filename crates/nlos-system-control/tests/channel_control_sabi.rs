//! End-to-end SABI integration tests for `nlos.sabi.ChannelControl` (`v1.0`)
//! inside `nlos-system-control` (`L-002`, `W73`).
//!
//! Covers:
//! 1. Full 9-method SABI envelope round-trip (`create_channel` -> `enqueue` ->
//!    `enqueue_registered` -> `receive` -> `register_queue_consumption` ->
//!    `ack` -> `compact` -> `inspect_queue` -> `inspect_channel` ->
//!    `rotate_channel`) and idempotent replay over a real WAL/FULL
//!    [`nlos_channel::ChannelAuthority`];
//! 2. Real local IPC endpoint multiplexing (`serve_validated_with_channel`)
//!    serving both `service = "channel"` and `service = "system_control"` on
//!    one platform listener (Unix domain socket on Unix, Named Pipe on Windows);
//! 3. Fail-closed SABI error mapping for `QueueFull`, `StaleChannel`,
//!    `IdempotencyConflict`, `ChannelNotFound`, invalid producer registration,
//!    and authorization rejection.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use nlos_channel::{
    AckDecision, AckRequest, ChannelAuthority, ChannelAuthorityError, ChannelDecision,
    ChannelRotationDecision, CompactDecision, ConsumptionRegistrationDecision,
    CreateChannelRequest, EnqueueDecision, EnqueueRequest, ProducerRegistration,
    RegisterQueueConsumptionRequest, RotateChannelRequest,
};
use nlos_clock::AuthorityClock;
use nlos_commit_coordinator::{RecoveryWorkerHealth, RecoveryWorkerState};
use nlos_ipc::{
    LocalRpcClient, OutboundResponse, PeerAuthorizer, PeerIdentity, TransportConfig, serve_one,
};
use nlos_schema::sabi::v1::{
    AckChannelRequestWire, CallerIdentity, CapabilityHandle, CompactChannelRequestWire,
    CreateChannelRequestWire, EnqueueChannelRequestWire, Envelope, ExchangeRequest,
    ExchangeResponse, InspectChannelRequestWire, InspectQueueRequestWire,
    ReceiveChannelRequestWire, RegisterQueueConsumptionRequestWire, RetryDirective,
    RotateChannelRequestWire, SabiErrorCode, SabiRequestContext, SabiResponseContext,
    SchemaIdentity, envelope,
};
use nlos_schema::{MethodSemantics, SABI_ENVELOPE_SCHEMA, validate_sabi_response_context};
use nlos_system_control::auth::{EndpointListener, serve_validated_with_channel};
use nlos_system_control::channel_control::{
    ACK_METHOD, AllowAllChannelControlAuthorizer, CHANNEL_CONTROL_SERVICE, COMPACT_METHOD,
    CREATE_CHANNEL_METHOD, ChannelBackendError, ChannelControlAuthorizer, ChannelControlBackend,
    ChannelControlService, ENQUEUE_METHOD, INSPECT_CHANNEL_METHOD, INSPECT_QUEUE_METHOD,
    RECEIVE_METHOD, REGISTER_QUEUE_CONSUMPTION_METHOD, ROTATE_CHANNEL_METHOD,
    channel_control_schema_identity, decode_ack_channel_result, decode_compact_channel_result,
    decode_create_channel_result, decode_enqueue_channel_result, decode_inspect_channel_result,
    decode_inspect_queue_result, decode_receive_channel_result,
    decode_register_queue_consumption_result, decode_rotate_channel_result,
    encode_ack_channel_request, encode_compact_channel_request, encode_create_channel_request,
    encode_enqueue_channel_request, encode_inspect_channel_request, encode_inspect_queue_request,
    encode_receive_channel_request, encode_register_queue_consumption_request,
    encode_rotate_channel_request, payload,
};
use nlos_system_control::control::{
    CONTROL_CAPABILITY_GENERATION, CONTROL_CAPABILITY_SLOT, ControlCommand, build_request_envelope,
};
use nlos_system_control::{RecoveryHealthSource, RecoverySystemControl, SystemControlAuthorizer};
use nlos_task::SqliteTaskAuthority;
use nlos_types::{ChannelId, ExecutionFiberId, Generation, IdempotencyKey};

static NEXT_SEQ: AtomicU64 = AtomicU64::new(1);

struct TempRoot(PathBuf);

impl TempRoot {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock")
            .as_nanos();
        let seq = NEXT_SEQ.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!(
            "nlos-cc-sabi-{label}-{}-{nonce}-{seq}",
            std::process::id(),
        )))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(unix)]
fn temp_endpoint(label: &str) -> PathBuf {
    let seq = NEXT_SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "nlos-cc-sabi-{label}-{}-{seq}.sock",
        std::process::id(),
    ))
}

#[cfg(windows)]
fn temp_endpoint(label: &str) -> PathBuf {
    let seq = NEXT_SEQ.fetch_add(1, Ordering::Relaxed);
    PathBuf::from(format!(
        r"\\.\pipe\nlos-cc-sabi-{label}-{}-{seq}",
        std::process::id(),
    ))
}

/// Adapter implementing [`ChannelControlBackend`] over [`ChannelAuthority`] so
/// the integration suite compiles and runs both with default features (`cli`)
/// and with `--features daemon`.
struct AuthorityBackend<'a>(&'a ChannelAuthority);

impl AuthorityBackend<'_> {
    fn map_err(error: ChannelAuthorityError) -> ChannelBackendError {
        match error {
            ChannelAuthorityError::Sqlite(inner) => ChannelBackendError::Sqlite(inner.to_string()),
            ChannelAuthorityError::Io(inner) => ChannelBackendError::Io(inner.to_string()),
            ChannelAuthorityError::DurabilityUnavailable {
                journal_mode,
                synchronous,
            } => ChannelBackendError::DurabilityUnavailable {
                journal_mode,
                synchronous,
            },
            ChannelAuthorityError::SchemaVersionUnsupported(version) => {
                ChannelBackendError::SchemaVersionUnsupported(version)
            }
            ChannelAuthorityError::ChannelNotFound(id) => {
                ChannelBackendError::ChannelNotFound(*id.as_bytes())
            }
            ChannelAuthorityError::IdempotencyConflict => ChannelBackendError::IdempotencyConflict,
            ChannelAuthorityError::StaleChannel => ChannelBackendError::StaleChannel,
            ChannelAuthorityError::InvalidCapacity => ChannelBackendError::InvalidCapacity,
            ChannelAuthorityError::GenerationExhausted => ChannelBackendError::GenerationExhausted,
            ChannelAuthorityError::QueueFull => ChannelBackendError::QueueFull,
            ChannelAuthorityError::InvalidPayload => ChannelBackendError::InvalidPayload,
            ChannelAuthorityError::InvalidSequence(reason) => {
                ChannelBackendError::InvalidSequence(reason)
            }
            ChannelAuthorityError::InvalidBindingRegistration => {
                ChannelBackendError::InvalidBindingRegistration
            }
            ChannelAuthorityError::CorruptRecord(reason) => {
                ChannelBackendError::CorruptRecord(reason)
            }
            ChannelAuthorityError::LockPoisoned => ChannelBackendError::LockPoisoned,
        }
    }
}

impl ChannelControlBackend for AuthorityBackend<'_> {
    fn create_channel(
        &self,
        capacity_bytes: u64,
        policy_digest: [u8; 32],
        idempotency_key: IdempotencyKey,
        created_at_ms: u64,
    ) -> Result<(payload::ChannelRecordWire, bool), ChannelBackendError> {
        let decision = self
            .0
            .create_channel(CreateChannelRequest {
                capacity_bytes,
                policy_digest,
                idempotency_key,
                created_at_ms,
            })
            .map_err(Self::map_err)?;
        let replayed = matches!(decision, ChannelDecision::Replayed(_));
        let record = decision.record();
        Ok((
            payload::ChannelRecordWire {
                channel_id: record.channel_id.as_bytes().to_vec(),
                generation: record.generation.get(),
                fencing_token: record.fencing_token.to_vec(),
                capacity_bytes: record.capacity_bytes,
                policy_digest: record.policy_digest.to_vec(),
                idempotency_key: record.idempotency_key.as_bytes().to_vec(),
                created_at_ms: record.created_at_ms,
            },
            replayed,
        ))
    }

    fn rotate_channel(
        &self,
        channel_id: [u8; 16],
        expected_generation: Generation,
        expected_fencing_token: [u8; 32],
        idempotency_key: IdempotencyKey,
        rotated_at_ms: u64,
    ) -> Result<(payload::ChannelRecordWire, bool), ChannelBackendError> {
        let decision = self
            .0
            .rotate_channel(RotateChannelRequest {
                channel_id: ChannelId::from_bytes(channel_id),
                expected_generation,
                expected_fencing_token,
                idempotency_key,
                rotated_at_ms,
            })
            .map_err(Self::map_err)?;
        let replayed = matches!(decision, ChannelRotationDecision::Replayed(_));
        let record = decision.record();
        Ok((
            payload::ChannelRecordWire {
                channel_id: record.channel_id.as_bytes().to_vec(),
                generation: record.generation.get(),
                fencing_token: record.fencing_token.to_vec(),
                capacity_bytes: record.capacity_bytes,
                policy_digest: record.policy_digest.to_vec(),
                idempotency_key: record.idempotency_key.as_bytes().to_vec(),
                created_at_ms: record.created_at_ms,
            },
            replayed,
        ))
    }

    fn enqueue(
        &self,
        channel_id: [u8; 16],
        expected_generation: Generation,
        expected_fencing_token: [u8; 32],
        payload: Vec<u8>,
        idempotency_key: IdempotencyKey,
        enqueued_at_ms: u64,
        producer: Option<([u8; 16], Generation)>,
    ) -> Result<(payload::QueueEntryRecordWire, bool), ChannelBackendError> {
        let request = EnqueueRequest {
            channel_id: ChannelId::from_bytes(channel_id),
            expected_generation,
            expected_fencing_token,
            payload,
            idempotency_key,
            enqueued_at_ms,
        };
        let decision = if let Some((binding, fiber_generation)) = producer {
            self.0
                .enqueue_registered(
                    request,
                    ProducerRegistration {
                        binding: ExecutionFiberId::from_bytes(binding),
                        fiber_generation,
                    },
                )
                .map_err(Self::map_err)?
        } else {
            self.0.enqueue(request).map_err(Self::map_err)?
        };
        let replayed = matches!(decision, EnqueueDecision::Replayed(_));
        let record = decision.record();
        Ok((
            payload::QueueEntryRecordWire {
                channel_id: record.channel_id.as_bytes().to_vec(),
                generation: record.generation.get(),
                fencing_token: record.fencing_token.to_vec(),
                sequence: record.sequence,
                payload: record.payload.clone(),
                payload_bytes: record.payload_bytes,
                idempotency_key: record.idempotency_key.as_bytes().to_vec(),
                enqueued_at_ms: record.enqueued_at_ms,
                binding: record
                    .binding
                    .map_or_else(Vec::new, |binding| binding.as_bytes().to_vec()),
                fiber_generation: record.fiber_generation.map_or(0, Generation::get),
            },
            replayed,
        ))
    }

    fn receive(
        &self,
        channel_id: [u8; 16],
        limit: usize,
    ) -> Result<Vec<payload::QueueEntryRecordWire>, ChannelBackendError> {
        let entries = self
            .0
            .receive(ChannelId::from_bytes(channel_id), limit)
            .map_err(Self::map_err)?;
        Ok(entries
            .into_iter()
            .map(|record| payload::QueueEntryRecordWire {
                channel_id: record.channel_id.as_bytes().to_vec(),
                generation: record.generation.get(),
                fencing_token: record.fencing_token.to_vec(),
                sequence: record.sequence,
                payload: record.payload,
                payload_bytes: record.payload_bytes,
                idempotency_key: record.idempotency_key.as_bytes().to_vec(),
                enqueued_at_ms: record.enqueued_at_ms,
                binding: record
                    .binding
                    .map_or_else(Vec::new, |binding| binding.as_bytes().to_vec()),
                fiber_generation: record.fiber_generation.map_or(0, Generation::get),
            })
            .collect())
    }

    fn ack(
        &self,
        channel_id: [u8; 16],
        up_to_sequence: u64,
        acked_at_ms: u64,
    ) -> Result<(u64, u64, bool), ChannelBackendError> {
        let decision = self
            .0
            .ack(AckRequest {
                channel_id: ChannelId::from_bytes(channel_id),
                up_to_sequence,
                acked_at_ms,
            })
            .map_err(Self::map_err)?;
        let replayed = matches!(decision, AckDecision::Replayed(_));
        let receipt = decision.receipt();
        Ok((receipt.consume_high_water, receipt.acked_at_ms, replayed))
    }

    fn compact(
        &self,
        channel_id: [u8; 16],
        trim_to_sequence: u64,
    ) -> Result<(u64, bool), ChannelBackendError> {
        let decision = self
            .0
            .compact(ChannelId::from_bytes(channel_id), trim_to_sequence)
            .map_err(Self::map_err)?;
        let replayed = matches!(decision, CompactDecision::Replayed(_));
        let receipt = decision.receipt();
        Ok((receipt.trim_high_water, replayed))
    }

    fn inspect_channel(
        &self,
        channel_id: [u8; 16],
    ) -> Result<payload::ChannelRecordWire, ChannelBackendError> {
        let record = self
            .0
            .inspect_channel(ChannelId::from_bytes(channel_id))
            .map_err(Self::map_err)?;
        Ok(payload::ChannelRecordWire {
            channel_id: record.channel_id.as_bytes().to_vec(),
            generation: record.generation.get(),
            fencing_token: record.fencing_token.to_vec(),
            capacity_bytes: record.capacity_bytes,
            policy_digest: record.policy_digest.to_vec(),
            idempotency_key: record.idempotency_key.as_bytes().to_vec(),
            created_at_ms: record.created_at_ms,
        })
    }

    fn inspect_queue(
        &self,
        channel_id: [u8; 16],
    ) -> Result<payload::QueueStateWire, ChannelBackendError> {
        let state = self
            .0
            .inspect_queue(ChannelId::from_bytes(channel_id))
            .map_err(Self::map_err)?;
        Ok(payload::QueueStateWire {
            channel_id: state.channel_id.as_bytes().to_vec(),
            capacity_bytes: state.capacity_bytes,
            consume_high_water: state.consume_high_water,
            trim_high_water: state.trim_high_water,
            backlog_bytes: state.backlog_bytes,
            retained_bytes: state.retained_bytes,
            max_sequence: state.max_sequence,
        })
    }

    fn register_queue_consumption(
        &self,
        channel_id: [u8; 16],
        sequence: u64,
        binding: [u8; 16],
        fiber_generation: Generation,
        idempotency_key: IdempotencyKey,
        registered_at_ms: u64,
    ) -> Result<(payload::QueueConsumptionRecordWire, bool), ChannelBackendError> {
        let decision = self
            .0
            .register_queue_consumption(RegisterQueueConsumptionRequest {
                channel_id: ChannelId::from_bytes(channel_id),
                sequence,
                binding: ExecutionFiberId::from_bytes(binding),
                fiber_generation,
                idempotency_key,
                registered_at_ms,
            })
            .map_err(Self::map_err)?;
        let replayed = matches!(decision, ConsumptionRegistrationDecision::Replayed(_));
        let record = decision.record();
        Ok((
            payload::QueueConsumptionRecordWire {
                registration_id: record.registration_id.as_bytes().to_vec(),
                channel_id: record.channel_id.as_bytes().to_vec(),
                sequence: record.sequence,
                binding: record.binding.as_bytes().to_vec(),
                fiber_generation: record.fiber_generation.get(),
                idempotency_key: record.idempotency_key.as_bytes().to_vec(),
                registered_at_ms: record.registered_at_ms,
            },
            replayed,
        ))
    }
}

fn make_envelope(
    method: &str,
    payload: Vec<u8>,
    idempotency_key: Option<[u8; 16]>,
    correlation_seed: u8,
) -> Envelope {
    Envelope {
        schema: Some(SchemaIdentity {
            name: SABI_ENVELOPE_SCHEMA.to_owned(),
            major: 1,
            minor: 1,
            critical_extension_ids: Vec::new(),
            non_critical_extension_ids: Vec::new(),
        }),
        request_id: vec![correlation_seed; 16],
        service: CHANNEL_CONTROL_SERVICE.to_owned(),
        method: method.to_owned(),
        common_context: Some(envelope::CommonContext::RequestContext(
            SabiRequestContext {
                caller: Some(CallerIdentity {
                    principal_id: vec![0x31; 16],
                    application_id: vec![0x32; 16],
                    process_id: vec![0x33; 16],
                    process_generation: 1,
                }),
                activity_context: Vec::new(),
                task_execution_binding: None,
                correlation_id: vec![correlation_seed; 16],
                idempotency_key: idempotency_key.map_or_else(Vec::new, |bytes| bytes.to_vec()),
                deadline_monotonic_ns: 0,
                capability_handles: vec![CapabilityHandle {
                    slot: CONTROL_CAPABILITY_SLOT,
                    generation: CONTROL_CAPABILITY_GENERATION,
                }],
                reservation_handle: None,
                proposal_or_input_digest_sha256: Vec::new(),
            },
        )),
        payload,
    }
}

fn unwrap_success(response: &Envelope, semantics: MethodSemantics) -> &SabiResponseContext {
    let context = validate_sabi_response_context(response, semantics)
        .expect("response context must satisfy SABI semantics");
    assert!(
        context.failure.is_none(),
        "expected success envelope, got failure: {:?}",
        context.failure
    );
    context
}

fn unwrap_failure(
    response: &Envelope,
    semantics: MethodSemantics,
) -> (SabiErrorCode, RetryDirective, String) {
    let context = validate_sabi_response_context(response, semantics)
        .expect("failure response context must satisfy SABI semantics");
    assert!(
        response.payload.is_empty(),
        "failure envelope must clear payload bytes"
    );
    assert!(
        context.receipts.is_empty(),
        "failure envelope must not manufacture receipts"
    );
    let failure = context.failure.as_ref().expect("expected SABI failure");
    (
        SabiErrorCode::try_from(failure.code).expect("valid error code"),
        RetryDirective::try_from(failure.retry).expect("valid retry directive"),
        failure.safe_message.clone(),
    )
}

#[test]
#[allow(clippy::too_many_lines, clippy::similar_names)]
fn full_sabi_channel_lifecycle_and_idempotent_replay_round_trip() {
    let root = TempRoot::new("lifecycle");
    let authority =
        ChannelAuthority::open(root.path().join("channel")).expect("open channel authority");
    let backend = AuthorityBackend(&authority);
    let service = ChannelControlService::new(&backend, AllowAllChannelControlAuthorizer);

    // 1. create_channel + replay
    let create_key = [0x11; 16];
    let policy_digest = [0x22; 32];
    let create_req = make_envelope(
        CREATE_CHANNEL_METHOD,
        encode_create_channel_request(&CreateChannelRequestWire {
            schema: Some(channel_control_schema_identity()),
            capacity_bytes: 256,
            policy_digest: policy_digest.to_vec(),
            idempotency_key: create_key.to_vec(),
            created_at_ms: 1_000,
        })
        .unwrap(),
        Some(create_key),
        0x01,
    );
    let create_resp = service.handle_for_ipc(&create_req, 100, 1_000);
    let create_ctx = unwrap_success(&create_resp, MethodSemantics::MUTATION);
    let created = decode_create_channel_result(&create_resp.payload).unwrap();
    assert!(!created.replayed);
    let channel_record = created.record.expect("channel record");
    assert_eq!(channel_record.generation, 1);
    assert_eq!(channel_record.capacity_bytes, 256);
    assert_eq!(channel_record.policy_digest, policy_digest);
    assert_eq!(create_ctx.receipts[0].receipt_id, channel_record.channel_id);

    let create_replay_resp = service.handle_for_ipc(&create_req, 101, 1_001);
    let create_replayed = decode_create_channel_result(&create_replay_resp.payload).unwrap();
    assert!(create_replayed.replayed);
    assert_eq!(create_replayed.record.as_ref(), Some(&channel_record));

    let channel_id = channel_record.channel_id.clone();
    let fence_gen1 = channel_record.fencing_token.clone();

    // 2. enqueue (unregistered) + replay
    let enq1_key = [0x12; 16];
    let enq1_req = make_envelope(
        ENQUEUE_METHOD,
        encode_enqueue_channel_request(&EnqueueChannelRequestWire {
            schema: Some(channel_control_schema_identity()),
            channel_id: channel_id.clone(),
            expected_generation: 1,
            expected_fencing_token: fence_gen1.clone(),
            payload: b"first-message".to_vec(),
            idempotency_key: enq1_key.to_vec(),
            enqueued_at_ms: 1_100,
            producer_binding: Vec::new(),
            producer_fiber_generation: 0,
        })
        .unwrap(),
        Some(enq1_key),
        0x02,
    );
    let enq1_resp = service.handle_for_ipc(&enq1_req, 200, 1_100);
    unwrap_success(&enq1_resp, MethodSemantics::MUTATION);
    let enq1_res = decode_enqueue_channel_result(&enq1_resp.payload).unwrap();
    assert!(!enq1_res.replayed);
    let entry1 = enq1_res.record.expect("entry 1");
    assert_eq!(entry1.sequence, 1);
    assert_eq!(entry1.payload, b"first-message");
    assert_eq!(entry1.binding, [] as [u8; 0]);
    assert_eq!(entry1.fiber_generation, 0);

    let enq1_replay =
        decode_enqueue_channel_result(&service.handle_for_ipc(&enq1_req, 201, 1_101).payload)
            .unwrap();
    assert!(enq1_replay.replayed);
    assert_eq!(enq1_replay.record.as_ref(), Some(&entry1));

    // 3. enqueue (registered with producer binding) + replay
    let enq2_key = [0x13; 16];
    let producer_binding = [0x77; 16];
    let enq2_req = make_envelope(
        ENQUEUE_METHOD,
        encode_enqueue_channel_request(&EnqueueChannelRequestWire {
            schema: Some(channel_control_schema_identity()),
            channel_id: channel_id.clone(),
            expected_generation: 1,
            expected_fencing_token: fence_gen1.clone(),
            payload: b"second-message-registered".to_vec(),
            idempotency_key: enq2_key.to_vec(),
            enqueued_at_ms: 1_200,
            producer_binding: producer_binding.to_vec(),
            producer_fiber_generation: 3,
        })
        .unwrap(),
        Some(enq2_key),
        0x03,
    );
    let enq2_resp = service.handle_for_ipc(&enq2_req, 300, 1_200);
    unwrap_success(&enq2_resp, MethodSemantics::MUTATION);
    let enq2_res = decode_enqueue_channel_result(&enq2_resp.payload).unwrap();
    assert!(!enq2_res.replayed);
    let entry2 = enq2_res.record.expect("entry 2");
    assert_eq!(entry2.sequence, 2);
    assert_eq!(entry2.binding, producer_binding);
    assert_eq!(entry2.fiber_generation, 3);

    // 4. receive
    let recv_req = make_envelope(
        RECEIVE_METHOD,
        encode_receive_channel_request(&ReceiveChannelRequestWire {
            schema: Some(channel_control_schema_identity()),
            channel_id: channel_id.clone(),
            limit: 16,
        })
        .unwrap(),
        None,
        0x04,
    );
    let recv_resp = service.handle_for_ipc(&recv_req, 400, 1_300);
    unwrap_success(&recv_resp, MethodSemantics::QUERY);
    let recv_res = decode_receive_channel_result(&recv_resp.payload).unwrap();
    assert_eq!(recv_res.entries, vec![entry1.clone(), entry2.clone()]);

    // 5. register_queue_consumption + replay
    let reg_key = [0x14; 16];
    let consumer_binding = [0x88; 16];
    let reg_req = make_envelope(
        REGISTER_QUEUE_CONSUMPTION_METHOD,
        encode_register_queue_consumption_request(&RegisterQueueConsumptionRequestWire {
            schema: Some(channel_control_schema_identity()),
            channel_id: channel_id.clone(),
            sequence: 1,
            binding: consumer_binding.to_vec(),
            fiber_generation: 2,
            idempotency_key: reg_key.to_vec(),
            registered_at_ms: 1_350,
        })
        .unwrap(),
        Some(reg_key),
        0x05,
    );
    let reg_resp = service.handle_for_ipc(&reg_req, 500, 1_350);
    let reg_ctx = unwrap_success(&reg_resp, MethodSemantics::MUTATION);
    let reg_res = decode_register_queue_consumption_result(&reg_resp.payload).unwrap();
    assert!(!reg_res.replayed);
    let reg_record = reg_res.record.expect("consumption record");
    assert_eq!(reg_record.sequence, 1);
    assert_eq!(reg_record.binding, consumer_binding);
    assert_eq!(reg_record.fiber_generation, 2);
    assert_eq!(reg_ctx.receipts[0].receipt_id, reg_record.registration_id);

    let reg_replay = decode_register_queue_consumption_result(
        &service.handle_for_ipc(&reg_req, 501, 1_351).payload,
    )
    .unwrap();
    assert!(reg_replay.replayed);
    assert_eq!(reg_replay.record.as_ref(), Some(&reg_record));

    // 6. ack + replay
    let ack_key = [0x15; 16];
    let ack_req = make_envelope(
        ACK_METHOD,
        encode_ack_channel_request(&AckChannelRequestWire {
            schema: Some(channel_control_schema_identity()),
            channel_id: channel_id.clone(),
            up_to_sequence: 2,
            idempotency_key: ack_key.to_vec(),
            acked_at_ms: 1_400,
        })
        .unwrap(),
        Some(ack_key),
        0x06,
    );
    let ack_resp = service.handle_for_ipc(&ack_req, 600, 1_400);
    unwrap_success(&ack_resp, MethodSemantics::MUTATION);
    let ack_res = decode_ack_channel_result(&ack_resp.payload).unwrap();
    assert!(!ack_res.replayed);
    assert_eq!(ack_res.consume_high_water, 2);
    assert_eq!(ack_res.acked_at_ms, 1_400);

    let ack_replay =
        decode_ack_channel_result(&service.handle_for_ipc(&ack_req, 601, 1_401).payload).unwrap();
    assert!(ack_replay.replayed);
    assert_eq!(ack_replay.consume_high_water, 2);

    // 7. inspect_queue before compact
    let inspect_q_req = make_envelope(
        INSPECT_QUEUE_METHOD,
        encode_inspect_queue_request(&InspectQueueRequestWire {
            schema: Some(channel_control_schema_identity()),
            channel_id: channel_id.clone(),
        })
        .unwrap(),
        None,
        0x07,
    );
    let inspect_q_resp = service.handle_for_ipc(&inspect_q_req, 700, 1_450);
    unwrap_success(&inspect_q_resp, MethodSemantics::QUERY);
    let queue_before = decode_inspect_queue_result(&inspect_q_resp.payload)
        .unwrap()
        .state
        .expect("queue state");
    assert_eq!(queue_before.consume_high_water, 2);
    assert_eq!(queue_before.trim_high_water, 0);
    assert_eq!(queue_before.backlog_bytes, 0);
    assert_eq!(
        queue_before.retained_bytes,
        entry1.payload_bytes + entry2.payload_bytes
    );
    assert_eq!(queue_before.max_sequence, 2);

    // 8. compact + replay + inspect_queue after compact
    let compact_key = [0x16; 16];
    let compact_req = make_envelope(
        COMPACT_METHOD,
        encode_compact_channel_request(&CompactChannelRequestWire {
            schema: Some(channel_control_schema_identity()),
            channel_id: channel_id.clone(),
            trim_to_sequence: 2,
            idempotency_key: compact_key.to_vec(),
        })
        .unwrap(),
        Some(compact_key),
        0x08,
    );
    let compact_resp = service.handle_for_ipc(&compact_req, 800, 1_500);
    unwrap_success(&compact_resp, MethodSemantics::MUTATION);
    let compact_res = decode_compact_channel_result(&compact_resp.payload).unwrap();
    assert!(!compact_res.replayed);
    assert_eq!(compact_res.trim_high_water, 2);

    let compact_replay =
        decode_compact_channel_result(&service.handle_for_ipc(&compact_req, 801, 1_501).payload)
            .unwrap();
    assert!(compact_replay.replayed);
    assert_eq!(compact_replay.trim_high_water, 2);

    let queue_after =
        decode_inspect_queue_result(&service.handle_for_ipc(&inspect_q_req, 802, 1_502).payload)
            .unwrap()
            .state
            .expect("queue state after compact");
    assert_eq!(queue_after.trim_high_water, 2);
    assert_eq!(queue_after.retained_bytes, 0);

    // 9. rotate_channel + replay + inspect_channel
    let rotate_key = [0x17; 16];
    let rotate_req = make_envelope(
        ROTATE_CHANNEL_METHOD,
        encode_rotate_channel_request(&RotateChannelRequestWire {
            schema: Some(channel_control_schema_identity()),
            channel_id: channel_id.clone(),
            expected_generation: 1,
            expected_fencing_token: fence_gen1,
            idempotency_key: rotate_key.to_vec(),
            rotated_at_ms: 1_600,
        })
        .unwrap(),
        Some(rotate_key),
        0x09,
    );
    let rotate_resp = service.handle_for_ipc(&rotate_req, 900, 1_600);
    unwrap_success(&rotate_resp, MethodSemantics::MUTATION);
    let rotate_res = decode_rotate_channel_result(&rotate_resp.payload).unwrap();
    assert!(!rotate_res.replayed);
    let rotated_record = rotate_res.record.expect("rotated record");
    assert_eq!(rotated_record.generation, 2);
    assert_ne!(rotated_record.fencing_token, channel_record.fencing_token);

    let rotate_replay =
        decode_rotate_channel_result(&service.handle_for_ipc(&rotate_req, 901, 1_601).payload)
            .unwrap();
    assert!(rotate_replay.replayed);
    assert_eq!(rotate_replay.record.as_ref(), Some(&rotated_record));

    let inspect_ch_req = make_envelope(
        INSPECT_CHANNEL_METHOD,
        encode_inspect_channel_request(&InspectChannelRequestWire {
            schema: Some(channel_control_schema_identity()),
            channel_id,
        })
        .unwrap(),
        None,
        0x0A,
    );
    let inspect_ch_resp = service.handle_for_ipc(&inspect_ch_req, 1_000, 1_700);
    unwrap_success(&inspect_ch_resp, MethodSemantics::QUERY);
    let inspected_channel = decode_inspect_channel_result(&inspect_ch_resp.payload)
        .unwrap()
        .record
        .expect("inspected channel");
    assert_eq!(inspected_channel, rotated_record);
}

struct DenyChannelAuthorizer;

impl ChannelControlAuthorizer for DenyChannelAuthorizer {
    fn authorize(&self, _method: &str, _context: &SabiRequestContext) -> Result<(), &'static str> {
        Err("channel policy denied")
    }
}

#[test]
#[allow(clippy::too_many_lines)]
fn channel_control_maps_authority_and_validation_errors_fail_closed() {
    let root = TempRoot::new("errors");
    let authority =
        ChannelAuthority::open(root.path().join("channel")).expect("open channel authority");
    let backend = AuthorityBackend(&authority);
    let service = ChannelControlService::new(&backend, AllowAllChannelControlAuthorizer);

    // Create a channel with small capacity (8 bytes) for QueueFull test.
    let create_key = [0x21; 16];
    let create_resp = service.handle_for_ipc(
        &make_envelope(
            CREATE_CHANNEL_METHOD,
            encode_create_channel_request(&CreateChannelRequestWire {
                schema: Some(channel_control_schema_identity()),
                capacity_bytes: 8,
                policy_digest: vec![0x33; 32],
                idempotency_key: create_key.to_vec(),
                created_at_ms: 100,
            })
            .unwrap(),
            Some(create_key),
            0x21,
        ),
        10,
        100,
    );
    let record = decode_create_channel_result(&create_resp.payload)
        .unwrap()
        .record
        .unwrap();

    // 1. IdempotencyConflict: reuse create_key with different capacity_bytes.
    let conflict_resp = service.handle_for_ipc(
        &make_envelope(
            CREATE_CHANNEL_METHOD,
            encode_create_channel_request(&CreateChannelRequestWire {
                schema: Some(channel_control_schema_identity()),
                capacity_bytes: 16,
                policy_digest: vec![0x33; 32],
                idempotency_key: create_key.to_vec(),
                created_at_ms: 100,
            })
            .unwrap(),
            Some(create_key),
            0x22,
        ),
        11,
        101,
    );
    let (code, retry, _) = unwrap_failure(&conflict_resp, MethodSemantics::MUTATION);
    assert_eq!(code, SabiErrorCode::Conflict);
    assert_eq!(retry, RetryDirective::DoNotRetry);

    // 2. QueueFull: enqueue 9 bytes into 8-byte channel.
    let full_key = [0x22; 16];
    let full_resp = service.handle_for_ipc(
        &make_envelope(
            ENQUEUE_METHOD,
            encode_enqueue_channel_request(&EnqueueChannelRequestWire {
                schema: Some(channel_control_schema_identity()),
                channel_id: record.channel_id.clone(),
                expected_generation: 1,
                expected_fencing_token: record.fencing_token.clone(),
                payload: vec![0xAA; 9],
                idempotency_key: full_key.to_vec(),
                enqueued_at_ms: 110,
                producer_binding: Vec::new(),
                producer_fiber_generation: 0,
            })
            .unwrap(),
            Some(full_key),
            0x23,
        ),
        12,
        110,
    );
    let (code, retry, _) = unwrap_failure(&full_resp, MethodSemantics::MUTATION);
    assert_eq!(code, SabiErrorCode::Quota);
    assert_eq!(retry, RetryDirective::DoNotRetry);

    // 3. StaleChannel: wrong expected_generation (2 instead of 1).
    let stale_key = [0x23; 16];
    let stale_resp = service.handle_for_ipc(
        &make_envelope(
            ENQUEUE_METHOD,
            encode_enqueue_channel_request(&EnqueueChannelRequestWire {
                schema: Some(channel_control_schema_identity()),
                channel_id: record.channel_id.clone(),
                expected_generation: 2,
                expected_fencing_token: record.fencing_token.clone(),
                payload: vec![0xBB; 4],
                idempotency_key: stale_key.to_vec(),
                enqueued_at_ms: 120,
                producer_binding: Vec::new(),
                producer_fiber_generation: 0,
            })
            .unwrap(),
            Some(stale_key),
            0x24,
        ),
        13,
        120,
    );
    let (code, retry, _) = unwrap_failure(&stale_resp, MethodSemantics::MUTATION);
    assert_eq!(code, SabiErrorCode::Fenced);
    assert_eq!(retry, RetryDirective::DoNotRetry);

    // 4. ChannelNotFound: inspect unknown channel.
    let missing_resp = service.handle_for_ipc(
        &make_envelope(
            INSPECT_CHANNEL_METHOD,
            encode_inspect_channel_request(&InspectChannelRequestWire {
                schema: Some(channel_control_schema_identity()),
                channel_id: vec![0xFF; 16],
            })
            .unwrap(),
            None,
            0x25,
        ),
        14,
        130,
    );
    let (code, retry, _) = unwrap_failure(&missing_resp, MethodSemantics::QUERY);
    assert_eq!(code, SabiErrorCode::NotFound);
    assert_eq!(retry, RetryDirective::DoNotRetry);

    // 5. Half-populated producer registration -> InvalidArgument.
    let bad_prod_key = [0x24; 16];
    let bad_prod_resp = service.handle_for_ipc(
        &make_envelope(
            ENQUEUE_METHOD,
            encode_enqueue_channel_request(&EnqueueChannelRequestWire {
                schema: Some(channel_control_schema_identity()),
                channel_id: record.channel_id,
                expected_generation: 1,
                expected_fencing_token: record.fencing_token,
                payload: vec![0xCC; 4],
                idempotency_key: bad_prod_key.to_vec(),
                enqueued_at_ms: 140,
                producer_binding: vec![0x55; 16],
                producer_fiber_generation: 0,
            })
            .unwrap(),
            Some(bad_prod_key),
            0x26,
        ),
        15,
        140,
    );
    let (code, retry, _) = unwrap_failure(&bad_prod_resp, MethodSemantics::MUTATION);
    assert_eq!(code, SabiErrorCode::InvalidArgument);
    assert_eq!(retry, RetryDirective::DoNotRetry);

    // 6. AuthorizationDenied -> CapabilityDenied.
    let deny_service = ChannelControlService::new(&backend, DenyChannelAuthorizer);
    let deny_resp = deny_service.handle_for_ipc(
        &make_envelope(
            INSPECT_QUEUE_METHOD,
            encode_inspect_queue_request(&InspectQueueRequestWire {
                schema: Some(channel_control_schema_identity()),
                channel_id: vec![0xFF; 16],
            })
            .unwrap(),
            None,
            0x27,
        ),
        16,
        150,
    );
    let (code, retry, _) = unwrap_failure(&deny_resp, MethodSemantics::QUERY);
    assert_eq!(code, SabiErrorCode::Rights);
    assert_eq!(retry, RetryDirective::DoNotRetry);
}

struct HealthyWorker;

impl RecoveryHealthSource for HealthyWorker {
    fn recovery_health(&self) -> RecoveryWorkerHealth {
        RecoveryWorkerHealth {
            state: RecoveryWorkerState::Running,
            completed_cycles: 1,
            ..RecoveryWorkerHealth::default()
        }
    }
}

struct AllowAllSystemControl;

impl SystemControlAuthorizer for AllowAllSystemControl {
    fn authorize_get(
        &self,
        _context: &SabiRequestContext,
        _request: &nlos_schema::sabi::v1::GetSystemControlRequest,
    ) -> Result<(), &'static str> {
        Ok(())
    }

    fn authorize_submit(
        &self,
        _context: &SabiRequestContext,
        _command: &nlos_schema::sabi::v1::ControlCommand,
    ) -> Result<(), &'static str> {
        Ok(())
    }
}

struct AllowPeer;

impl PeerAuthorizer for AllowPeer {
    fn authorize(&self, _peer: &PeerIdentity) -> Result<(), String> {
        Ok(())
    }
}

#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn serve_validated_with_channel_multiplexes_channel_and_system_control_over_local_ipc() {
    let root = TempRoot::new("ipc-mux");
    let endpoint = temp_endpoint("mux");
    let clock = AuthorityClock::open(root.path().join("clock")).expect("open clock");
    let tasks = SqliteTaskAuthority::open(root.path().join("tasks.sqlite3")).expect("open tasks");
    let channel_authority =
        ChannelAuthority::open(root.path().join("channel")).expect("open channel authority");

    #[cfg(unix)]
    let listener = EndpointListener::bind(&endpoint).expect("bind unix socket");
    #[cfg(windows)]
    let mut listener =
        EndpointListener::bind(&endpoint, 4, TransportConfig::default()).expect("bind named pipe");

    let server_task = tokio::spawn(async move {
        let backend = AuthorityBackend(&channel_authority);
        let control = RecoverySystemControl::new(&tasks, &HealthyWorker, &AllowAllSystemControl);
        let channel_service =
            ChannelControlService::new(&backend, AllowAllChannelControlAuthorizer);
        for _ in 0..3 {
            let (stream, peer) = listener
                .accept(TransportConfig::default())
                .await
                .expect("accept connection");
            serve_one(
                stream,
                TransportConfig::default(),
                peer,
                &AllowPeer,
                |validated| {
                    let response = serve_validated_with_channel(
                        &control,
                        &channel_service,
                        &clock,
                        500,
                        validated.envelope(),
                    );
                    async move {
                        Ok(OutboundResponse::Typed(ExchangeResponse {
                            envelope: Some(response),
                        }))
                    }
                },
            )
            .await
            .expect("serve one exchange");
        }
    });

    let connect_and_exchange = |request: Envelope| {
        let endpoint = endpoint.clone();
        async move {
            let config = TransportConfig::default();
            #[cfg(unix)]
            let (stream, _) = nlos_ipc::unix::connect(&endpoint, config)
                .await
                .expect("connect unix");
            #[cfg(windows)]
            let (stream, _) = nlos_ipc::windows::connect(endpoint.as_os_str(), config)
                .await
                .expect("connect pipe");
            let client = LocalRpcClient::new(stream, config);
            client
                .exchange_validated(ExchangeRequest {
                    envelope: Some(request),
                })
                .await
                .expect("validated exchange")
                .envelope()
                .clone()
        }
    };

    // Round 1: create_channel over IPC
    let create_key = [0x41; 16];
    let create_env = connect_and_exchange(make_envelope(
        CREATE_CHANNEL_METHOD,
        encode_create_channel_request(&CreateChannelRequestWire {
            schema: Some(channel_control_schema_identity()),
            capacity_bytes: 128,
            policy_digest: vec![0x44; 32],
            idempotency_key: create_key.to_vec(),
            created_at_ms: 2_000,
        })
        .unwrap(),
        Some(create_key),
        0x41,
    ))
    .await;
    unwrap_success(&create_env, MethodSemantics::MUTATION);
    let created = decode_create_channel_result(&create_env.payload)
        .unwrap()
        .record
        .expect("created channel");

    // Round 2: enqueue over IPC
    let enq_key = [0x42; 16];
    let enq_env = connect_and_exchange(make_envelope(
        ENQUEUE_METHOD,
        encode_enqueue_channel_request(&EnqueueChannelRequestWire {
            schema: Some(channel_control_schema_identity()),
            channel_id: created.channel_id.clone(),
            expected_generation: created.generation,
            expected_fencing_token: created.fencing_token.clone(),
            payload: b"ipc-round-trip".to_vec(),
            idempotency_key: enq_key.to_vec(),
            enqueued_at_ms: 2_100,
            producer_binding: Vec::new(),
            producer_fiber_generation: 0,
        })
        .unwrap(),
        Some(enq_key),
        0x42,
    ))
    .await;
    unwrap_success(&enq_env, MethodSemantics::MUTATION);
    let entry = decode_enqueue_channel_result(&enq_env.payload)
        .unwrap()
        .record
        .expect("enqueued entry");
    assert_eq!(entry.sequence, 1);
    assert_eq!(entry.payload, b"ipc-round-trip");

    // Round 3: system_control InspectHealth over the same multiplexed IPC server
    let sys_env = connect_and_exchange(
        build_request_envelope(&ControlCommand::InspectHealth).expect("inspect health envelope"),
    )
    .await;
    unwrap_success(&sys_env, MethodSemantics::QUERY);

    server_task.await.expect("server task clean exit");
    #[cfg(unix)]
    let _ = fs::remove_file(&endpoint);
}

#[cfg(feature = "daemon")]
#[tokio::test]
#[allow(clippy::too_many_lines)]
async fn daemon_plain_endpoint_and_in_process_channel_control_round_trip() {
    use nlos_system_control::daemon::{DaemonOptions, assemble, serve_plain_endpoint};
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    let root = TempRoot::new("daemon-channel");
    let auth_ep = temp_endpoint("daemon-auth");
    let plain_ep = temp_endpoint("daemon-plain");
    let (daemon, endpoints) = assemble(
        DaemonOptions::new(root.path())
            .with_auth_socket(&auth_ep)
            .with_plain_socket(&plain_ep),
        tokio::runtime::Handle::current(),
    )
    .expect("assemble daemon");

    // In-process handle_channel_for_ipc creates a channel
    let create_key = [0x51; 16];
    let create_resp = daemon.handle_channel_for_ipc(
        &make_envelope(
            CREATE_CHANNEL_METHOD,
            encode_create_channel_request(&CreateChannelRequestWire {
                schema: Some(channel_control_schema_identity()),
                capacity_bytes: 512,
                policy_digest: vec![0x55; 32],
                idempotency_key: create_key.to_vec(),
                created_at_ms: 3_000,
            })
            .unwrap(),
            Some(create_key),
            0x51,
        ),
        100,
        3_000,
    );
    unwrap_success(&create_resp, MethodSemantics::MUTATION);
    let created = decode_create_channel_result(&create_resp.payload)
        .unwrap()
        .record
        .expect("daemon created channel");

    // Plain endpoint inspects the channel created in-process
    let stop = Arc::new(AtomicBool::new(false));
    let serve_stop = Arc::clone(&stop);
    let serve_daemon = Arc::clone(&daemon);
    let plain_loop = tokio::spawn(async move {
        serve_plain_endpoint(serve_daemon, endpoints.listener_plain, serve_stop).await;
    });

    let config = TransportConfig::default();
    #[cfg(unix)]
    let (stream, _) = nlos_ipc::unix::connect(&plain_ep, config)
        .await
        .expect("connect plain unix");
    #[cfg(windows)]
    let (stream, _) = nlos_ipc::windows::connect(plain_ep.as_os_str(), config)
        .await
        .expect("connect plain pipe");
    let client = LocalRpcClient::new(stream, config);
    let inspect_env = client
        .exchange_validated(ExchangeRequest {
            envelope: Some(make_envelope(
                INSPECT_CHANNEL_METHOD,
                encode_inspect_channel_request(&InspectChannelRequestWire {
                    schema: Some(channel_control_schema_identity()),
                    channel_id: created.channel_id.clone(),
                })
                .unwrap(),
                None,
                0x52,
            )),
        })
        .await
        .expect("plain endpoint channel exchange")
        .envelope()
        .clone();
    unwrap_success(&inspect_env, MethodSemantics::QUERY);
    let inspected = decode_inspect_channel_result(&inspect_env.payload)
        .unwrap()
        .record
        .expect("inspected channel over plain endpoint");
    assert_eq!(inspected, created);

    stop.store(true, Ordering::Relaxed);
    plain_loop.abort();
    let _ = plain_loop.await;
    daemon.stop_worker();
    daemon.stop_materialization();
    drop(daemon);
}
