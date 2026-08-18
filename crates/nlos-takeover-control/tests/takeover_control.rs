//! Acceptance tests for the `TakeoverControl` IPC service: in-memory duplex
//! submission with byte-equal idempotent replay, a ServiceDirectory-resolved
//! Unix endpoint gated by the exact observed peer credential, and the typed
//! `RIGHTS`/`CONFLICT`/`NOT_SUPPORTED` failure mappings over real IPC framing.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::{Mutex, MutexGuard};

use ed25519_dalek::{Signer, SigningKey};
use nlos_identity::{BootstrapPrincipalRequest, IdentityAuthority, KeyPurpose};
use nlos_ipc::{
    LocalRpcClient, OutboundResponse, PeerAuthorizer, PeerIdentity, TransportConfig, serve_one,
};
use nlos_schema::sabi::v1::{
    BarrierObservationEvidence, BarrierObservationSignature as BarrierObservationSignatureProto,
    BarrierObservationTarget, CallerIdentity, CapabilityHandle, Envelope, ExchangeRequest,
    ExchangeResponse, ReceiptReference, RetryDirective, SabiErrorCode, SabiFailure,
    SabiRequestContext, SchemaIdentity, SubmitBarrierObservationRequest, envelope,
};
use nlos_schema::{
    MethodSemantics, SABI_ENVELOPE_SCHEMA, ValidatedExchangeResponse,
    decode_barrier_observation_record, encode_submit_barrier_observation_request,
    takeover_control_schema_identity, validate_sabi_response_context,
};
use nlos_store_fault::{FaultCode, FaultMode};
use nlos_takeover_control::{
    SUBMIT_BARRIER_OBSERVATION_METHOD, TAKEOVER_CONTROL_SERVICE, TakeoverControl,
    TakeoverControlAuthorizer, TakeoverControlError, failure_envelope, participant_type_code,
};
use nlos_task::{
    AttemptSpec, AuthorityLeaseDecision, AuthorityLeaseFinalizeRequest,
    AuthorityLeasePermitRequest, AuthorityLeaseRequest, AuthorityLeaseTakeoverFenceRequest,
    AuthorityTakeoverBarrierCoverageState, AuthorityTakeoverBarrierReceiptRequest,
    BarrierObservationSignature, FinalizeRequest, FinalizeRequestV3, ParticipantRecord,
    PermitDecision, PermitRequest, SnapshotBundle, SqliteTaskAuthority, TaskSpec,
    barrier_observation_signature_message, empty_effect_history_root,
};
use nlos_types::{
    CancellationScopeId, Generation, IdempotencyKey, ProcessId, ReceiptId, TaskAttemptId, TaskId,
    TaskSnapshotId,
};
use tokio::io::duplex;

static NEXT: AtomicU64 = AtomicU64::new(1);
static FAULT_LOCK: Mutex<()> = Mutex::const_new(());
const FAULT_VFS_NAME: &str = "nlos-takeover-control-fault";

async fn fault_lock() -> MutexGuard<'static, ()> {
    FAULT_LOCK.lock().await
}

struct FaultReset;

impl Drop for FaultReset {
    fn drop(&mut self) {
        nlos_store_fault::disarm();
    }
}

struct TestDatabase {
    path: PathBuf,
}

impl TestDatabase {
    fn new(label: &str) -> Self {
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        Self {
            path: std::env::temp_dir().join(format!(
                "nlos-takeover-control-{label}-{}-{sequence}.sqlite3",
                std::process::id()
            )),
        }
    }

    fn open(&self) -> SqliteTaskAuthority {
        SqliteTaskAuthority::open(&self.path).expect("open task authority")
    }

    fn open_with_fault_vfs(&self) -> SqliteTaskAuthority {
        nlos_store_fault::register(FAULT_VFS_NAME).expect("register fault VFS");
        SqliteTaskAuthority::open_with_vfs(&self.path, Some(FAULT_VFS_NAME))
            .expect("open task authority with fault VFS")
    }
}

impl Drop for TestDatabase {
    fn drop(&mut self) {
        for path in [
            self.path.clone(),
            suffix_path(&self.path, "-wal"),
            suffix_path(&self.path, "-shm"),
        ] {
            match fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => panic!("remove test database: {error}"),
            }
        }
    }
}

fn suffix_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

struct IdentityRoot(PathBuf);

impl IdentityRoot {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("monotonic clock")
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "nlos-takeover-control-identity-{label}-{}-{nonce}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for IdentityRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct BarrierSigner {
    key: SigningKey,
    binding: nlos_identity::IdentityBinding,
}

fn bootstrap_barrier_signer(
    identity: &IdentityAuthority,
    seed: u8,
    purpose: KeyPurpose,
) -> BarrierSigner {
    let key = SigningKey::from_bytes(&[seed; 32]);
    let binding = identity
        .bootstrap_principal(BootstrapPrincipalRequest {
            principal_profile_digest: [seed.wrapping_add(1); 32],
            control_domain_policy_digest: [seed.wrapping_add(2); 32],
            public_key: key.verifying_key().to_bytes(),
            key_purpose: purpose,
            key_valid_from_ms: 0,
            key_valid_until_ms: 10_000,
            idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(3); 16]),
            created_at_ms: 0,
        })
        .expect("bootstrap signer")
        .binding();
    BarrierSigner { key, binding }
}

struct FrozenFence {
    takeover_receipt_id: ReceiptId,
    participant: ParticipantRecord,
    fence_set_root: [u8; 32],
}

fn lease_request(holder: u8, key: u8, at_ms: i64, ttl_ms: i64) -> AuthorityLeaseRequest {
    AuthorityLeaseRequest {
        holder_id: ProcessId::from_bytes([key.wrapping_add(holder); 16]),
        idempotency_key: IdempotencyKey::from_bytes([key; 16]),
        requested_at_ms: at_ms,
        ttl_ms,
    }
}

fn lease_record(decision: AuthorityLeaseDecision) -> nlos_task::AuthorityLeaseRecord {
    decision.record()
}

fn register_task_attempt(authority: &SqliteTaskAuthority, seed: u8) -> AttemptSpec {
    let task_id = TaskId::from_bytes([seed; 16]);
    authority
        .register_task(TaskSpec {
            task_id,
            task_generation: Generation::INITIAL,
            registered_at_ms: 1,
        })
        .expect("register task");
    let attempt = AttemptSpec {
        task_id,
        attempt_id: TaskAttemptId::from_bytes([seed.wrapping_add(1); 16]),
        attempt_generation: Generation::INITIAL,
        snapshot: SnapshotBundle {
            snapshot_id: TaskSnapshotId::from_bytes([seed.wrapping_add(2); 16]),
            snapshot_digest: [seed.wrapping_add(3); 32],
            expected_head_commit_seq: 0,
            effect_history_root: empty_effect_history_root(),
            retry_fence_epoch: 0,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes([seed.wrapping_add(4); 16]),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(5); 16]),
        registered_at_ms: 2,
    };
    authority
        .register_attempt(attempt)
        .expect("register attempt");
    attempt
}

fn permit_request(attempt: &AttemptSpec, seed: u8, requested_at_ms: i64) -> PermitRequest {
    PermitRequest {
        task_id: attempt.task_id,
        attempt_id: attempt.attempt_id,
        attempt_generation: attempt.attempt_generation,
        write_set_root: [seed; 32],
        planned_effects: Vec::new(),
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(10); 16]),
        valid_until_ms: 10_000,
        requested_at_ms,
    }
}

fn finalize_request(
    attempt: &AttemptSpec,
    permit_id: nlos_types::CommitPermitId,
    finalized_at_ms: i64,
) -> FinalizeRequestV3 {
    FinalizeRequestV3 {
        base: FinalizeRequest {
            task_id: attempt.task_id,
            attempt_id: attempt.attempt_id,
            attempt_generation: attempt.attempt_generation,
            permit_id,
            new_effect_history_root: [0; 32],
            new_retry_fence_epoch: 0,
            finalized_at_ms,
        },
        required_satisfaction: Vec::new(),
        fenced_participant_digest: [0; 32],
    }
}

fn fence_takeover(authority: &SqliteTaskAuthority, seed: u8) -> FrozenFence {
    let lease_one = lease_record(
        authority
            .acquire_authority_lease(lease_request(1, seed, 100, 100))
            .expect("initial lease"),
    );
    let attempt = register_task_attempt(authority, seed);
    let permit = match authority
        .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
            permit: permit_request(&attempt, seed, 150),
            lease: lease_one,
        })
        .expect("lease-bound permit")
    {
        PermitDecision::Issued(permit) => *permit,
        other => panic!("expected issued permit, got {other:?}"),
    };
    let registry_binding = permit
        .participant_registry_binding
        .expect("permit registry binding");
    authority
        .finalize_commit_v3_with_authority_lease(AuthorityLeaseFinalizeRequest {
            finalize: finalize_request(&attempt, permit.permit_id, 160),
            lease: lease_one,
        })
        .expect("close permit before takeover");
    let lease_two = lease_record(
        authority
            .acquire_authority_lease(lease_request(2, seed.wrapping_add(0x11), 201, 100))
            .expect("takeover lease"),
    );
    assert_eq!(lease_two.term, lease_one.term + 1);
    let frozen = authority
        .prepare_authority_takeover_fence(AuthorityLeaseTakeoverFenceRequest {
            task_id: attempt.task_id,
            expected_registry_binding: registry_binding,
            lease: lease_two,
            requested_at_ms: 210,
        })
        .expect("freeze current registry");
    let fence_receipt = authority
        .inspect_authority_takeover_fence_receipt(attempt.task_id, registry_binding)
        .expect("takeover fence receipt");
    let takeover_receipt = authority
        .inspect_authority_takeover_receipt(attempt.task_id, fence_receipt.receipt_id)
        .expect("pending takeover receipt");
    assert_eq!(takeover_receipt.new_assignment_id, None);
    FrozenFence {
        takeover_receipt_id: takeover_receipt.receipt_id,
        participant: frozen
            .participants
            .first()
            .copied()
            .expect("frozen registry participant"),
        fence_set_root: takeover_receipt
            .exact_fence_set_root
            .expect("exact fence set root"),
    }
}

const OBSERVED_AT_MS: i64 = 220;
const REMOTE_RECEIPT_SEED: u8 = 0x91;
const BARRIER_DIGEST_SEED: u8 = 0x92;

fn remote_receipt_id() -> ReceiptId {
    ReceiptId::from_bytes([REMOTE_RECEIPT_SEED; 16])
}

fn barrier_digest() -> [u8; 32] {
    [BARRIER_DIGEST_SEED; 32]
}

fn observation_digest(fence: &FrozenFence) -> [u8; 32] {
    barrier_observation_signature_message(
        fence.takeover_receipt_id,
        &fence.participant,
        remote_receipt_id(),
        barrier_digest(),
        fence.fence_set_root,
    )
}

fn observation(fence: &FrozenFence, signer: &BarrierSigner) -> [u8; 64] {
    signer.key.sign(&observation_digest(fence)).to_bytes()
}

fn barrier_store_request(fence: &FrozenFence) -> AuthorityTakeoverBarrierReceiptRequest {
    AuthorityTakeoverBarrierReceiptRequest {
        takeover_receipt_id: fence.takeover_receipt_id,
        participant: fence.participant,
        remote_receipt_id: remote_receipt_id(),
        barrier_digest: barrier_digest(),
        observed_at_ms: OBSERVED_AT_MS,
    }
}

fn domain_signature(signer: &BarrierSigner, signature: [u8; 64]) -> BarrierObservationSignature {
    BarrierObservationSignature {
        issuer: signer.binding.principal_id,
        control_domain_id: signer.binding.control_domain_id,
        key_id: signer.binding.key_id,
        signature,
    }
}

struct Fixture {
    authority: Arc<SqliteTaskAuthority>,
    identity: Arc<IdentityAuthority>,
    signer: BarrierSigner,
    fence: FrozenFence,
    // Cleanup fields are declared last: struct fields drop in declaration
    // order, so the SQLite connections above must close before the database
    // files and identity directory are removed (Windows fails with os
    // error 32 when removing files with open handles). The database path
    // is read only by the cfg(unix) socket test, so other targets exempt
    // the field from dead_code while keeping its Drop cleanup.
    #[cfg_attr(not(unix), allow(dead_code))]
    database: TestDatabase,
    // Held only for its Drop-time directory cleanup.
    _identity_root: IdentityRoot,
}

impl Fixture {
    fn new(label: &str, seed: u8, purpose: KeyPurpose) -> Self {
        Self::new_with_authority(label, seed, purpose, false)
    }

    fn new_with_fault_vfs(label: &str, seed: u8, purpose: KeyPurpose) -> Self {
        Self::new_with_authority(label, seed, purpose, true)
    }

    fn new_with_authority(label: &str, seed: u8, purpose: KeyPurpose, fault_vfs: bool) -> Self {
        let database = TestDatabase::new(label);
        let identity_root = IdentityRoot::new(label);
        let authority = Arc::new(if fault_vfs {
            database.open_with_fault_vfs()
        } else {
            database.open()
        });
        let identity = Arc::new(IdentityAuthority::open(identity_root.path()).unwrap());
        let signer = bootstrap_barrier_signer(&identity, seed, purpose);
        let fence = fence_takeover(&authority, seed);
        Self {
            authority,
            identity,
            signer,
            fence,
            database,
            _identity_root: identity_root,
        }
    }
}

struct CapabilityPolicy;

impl TakeoverControlAuthorizer for CapabilityPolicy {
    fn authorize_submit_barrier_observation(
        &self,
        context: &SabiRequestContext,
        _: &SubmitBarrierObservationRequest,
    ) -> Result<(), &'static str> {
        if context.capability_handles
            == [CapabilityHandle {
                slot: 5,
                generation: 1,
            }]
        {
            Ok(())
        } else {
            Err("missing takeover control capability")
        }
    }
}

struct AllowPeer;

impl PeerAuthorizer for AllowPeer {
    fn authorize(&self, _: &PeerIdentity) -> Result<(), String> {
        Ok(())
    }
}

fn request_context() -> SabiRequestContext {
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
        idempotency_key: vec![0x45; 16],
        deadline_monotonic_ns: 0,
        capability_handles: vec![CapabilityHandle {
            slot: 5,
            generation: 1,
        }],
        reservation_handle: None,
        proposal_or_input_digest_sha256: Vec::new(),
    }
}

fn envelope(method: &str, payload: Vec<u8>) -> Envelope {
    Envelope {
        schema: Some(SchemaIdentity {
            name: SABI_ENVELOPE_SCHEMA.to_owned(),
            major: 1,
            minor: 1,
            critical_extension_ids: Vec::new(),
            non_critical_extension_ids: Vec::new(),
        }),
        request_id: vec![0x35; 16],
        service: TAKEOVER_CONTROL_SERVICE.to_owned(),
        method: method.to_owned(),
        common_context: Some(envelope::CommonContext::RequestContext(request_context())),
        payload,
    }
}

fn submit_request(fence: &FrozenFence, signer: &BarrierSigner, signature: [u8; 64]) -> Envelope {
    let submit = SubmitBarrierObservationRequest {
        schema: Some(takeover_control_schema_identity()),
        target: Some(BarrierObservationTarget {
            takeover_receipt_id: fence.takeover_receipt_id.into_bytes().to_vec(),
            participant_type: participant_type_code(fence.participant.participant_type),
            participant_id: fence.participant.participant_id.as_bytes().to_vec(),
            participant_generation: fence.participant.participant_generation.get(),
            admission_receipt_id: fence.participant.admission_receipt_id.into_bytes().to_vec(),
        }),
        evidence: Some(BarrierObservationEvidence {
            remote_receipt_id: remote_receipt_id().into_bytes().to_vec(),
            barrier_digest: barrier_digest().to_vec(),
            observed_at_ms: OBSERVED_AT_MS,
        }),
        signature: Some(BarrierObservationSignatureProto {
            signer_principal_id: signer.binding.principal_id.as_bytes().to_vec(),
            signer_control_domain_id: signer.binding.control_domain_id.as_bytes().to_vec(),
            signer_key_id: signer.binding.key_id.as_bytes().to_vec(),
            signature: signature.to_vec(),
        }),
    };
    envelope(
        SUBMIT_BARRIER_OBSERVATION_METHOD,
        encode_submit_barrier_observation_request(&submit).unwrap(),
    )
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

async fn exchange(
    authority: Arc<SqliteTaskAuthority>,
    identity: Arc<IdentityAuthority>,
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
                let response = match TakeoverControl::new(
                    authority.as_ref(),
                    identity.as_ref(),
                    &CapabilityPolicy,
                )
                .handle(validated.envelope(), 10, 6_000)
                {
                    Ok(envelope) => envelope,
                    Err(error) => failure_envelope(validated.envelope(), &error),
                };
                async move {
                    Ok(OutboundResponse::Typed(ExchangeResponse {
                        envelope: Some(response),
                    }))
                }
            },
        )
        .await
    });
    let response = LocalRpcClient::new(client_stream, config)
        .exchange_validated(request)
        .await
        .expect("IPC exchange succeeds");
    server.await.unwrap().unwrap();
    response
}

fn response_failure(response: &Envelope) -> &SabiFailure {
    match response.common_context.as_ref().unwrap() {
        envelope::CommonContext::ResponseContext(context) => context.failure.as_ref().unwrap(),
        envelope::CommonContext::RequestContext(_) => unreachable!("handler always responds"),
    }
}

#[tokio::test]
async fn signed_observation_crosses_duplex_ipc_and_replays_identically() {
    let fixture = Fixture::new("happy-path", 0x31, KeyPurpose::BarrierObservationSigning);
    let signature = observation(&fixture.fence, &fixture.signer);
    let request = ExchangeRequest {
        envelope: Some(submit_request(&fixture.fence, &fixture.signer, signature)),
    };
    let response = exchange(
        Arc::clone(&fixture.authority),
        Arc::clone(&fixture.identity),
        request.clone(),
    )
    .await;
    let response_envelope = response.envelope();
    validate_sabi_response_context(response_envelope, MethodSemantics::MUTATION).unwrap();
    let record = decode_barrier_observation_record(&response_envelope.payload).unwrap();
    assert!(record.signed);
    assert_eq!(
        record.signer_principal_id,
        fixture.signer.binding.principal_id.as_bytes().to_vec()
    );
    assert_eq!(
        record.signer_key_id,
        fixture.signer.binding.key_id.as_bytes().to_vec()
    );
    assert_eq!(
        record.signer_key_generation,
        fixture.signer.binding.key_generation.get()
    );
    assert_eq!(
        response_envelope
            .common_context
            .as_ref()
            .and_then(|context| match context {
                envelope::CommonContext::ResponseContext(context) => context.receipts.first(),
                envelope::CommonContext::RequestContext(_) => None,
            }),
        Some(&ReceiptReference {
            receipt_id: record.receipt_id.clone()
        })
    );

    let direct = fixture
        .authority
        .record_authority_takeover_barrier_receipt_signed(
            fixture.identity.as_ref(),
            barrier_store_request(&fixture.fence),
            domain_signature(&fixture.signer, signature),
        )
        .expect("direct store replay");
    assert_eq!(record.receipt_id, direct.receipt_id.into_bytes().to_vec());
    let rows = fixture
        .authority
        .inspect_authority_takeover_barrier_receipts(fixture.fence.takeover_receipt_id)
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].receipt_id.into_bytes().to_vec(), record.receipt_id);
    let coverage = fixture
        .authority
        .inspect_authority_takeover_barrier_coverage(fixture.fence.takeover_receipt_id)
        .unwrap();
    assert_eq!(
        coverage.state,
        AuthorityTakeoverBarrierCoverageState::LocallyCovered
    );

    let replay = exchange(
        Arc::clone(&fixture.authority),
        Arc::clone(&fixture.identity),
        request,
    )
    .await;
    assert_eq!(replay.envelope().payload, response_envelope.payload);
    assert_eq!(
        decode_barrier_observation_record(&replay.envelope().payload).unwrap(),
        record
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_callers_linearize_to_one_verified_observation() {
    let fixture = Fixture::new(
        "concurrent-callers",
        0x37,
        KeyPurpose::BarrierObservationSigning,
    );
    let signature = observation(&fixture.fence, &fixture.signer);
    let request = ExchangeRequest {
        envelope: Some(submit_request(&fixture.fence, &fixture.signer, signature)),
    };
    let mut second_request = request.clone();
    let second_envelope = second_request.envelope.as_mut().unwrap();
    second_envelope.request_id = vec![0x36; 16];
    if let Some(envelope::CommonContext::RequestContext(context)) =
        second_envelope.common_context.as_mut()
    {
        context.caller.as_mut().unwrap().principal_id = vec![0x37; 16];
        context.correlation_id = vec![0x38; 16];
    }

    let first_exchange = exchange(
        Arc::clone(&fixture.authority),
        Arc::clone(&fixture.identity),
        request,
    );
    let second_exchange = exchange(
        Arc::clone(&fixture.authority),
        Arc::clone(&fixture.identity),
        second_request,
    );
    let (first, second) = tokio::join!(first_exchange, second_exchange);
    let first_envelope = first.envelope();
    let second_envelope = second.envelope();
    validate_sabi_response_context(first_envelope, MethodSemantics::MUTATION).unwrap();
    validate_sabi_response_context(second_envelope, MethodSemantics::MUTATION).unwrap();
    assert_ne!(first_envelope.request_id, second_envelope.request_id);
    let first_record = decode_barrier_observation_record(&first_envelope.payload).unwrap();
    let second_record = decode_barrier_observation_record(&second_envelope.payload).unwrap();
    assert!(first_record.signed);
    assert_eq!(first_record, second_record);

    let rows = fixture
        .authority
        .inspect_authority_takeover_barrier_receipts(fixture.fence.takeover_receipt_id)
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].receipt_id.into_bytes().as_slice(),
        first_record.receipt_id.as_slice()
    );
    let signer = rows[0].signer.expect("durable signer proof");
    assert_eq!(
        signer.principal_id.as_bytes(),
        first_record.signer_principal_id.as_slice()
    );
    assert_eq!(
        signer.key_id.as_bytes(),
        first_record.signer_key_id.as_slice()
    );
    let coverage = fixture
        .authority
        .inspect_authority_takeover_barrier_coverage(fixture.fence.takeover_receipt_id)
        .unwrap();
    assert_eq!(
        coverage.state,
        AuthorityTakeoverBarrierCoverageState::LocallyCovered
    );
}

async fn run_ipc_write_fault(code: FaultCode) {
    let _serialization = fault_lock().await;
    nlos_store_fault::disarm();
    let fixture = Fixture::new_with_fault_vfs(
        "ipc-write-fault",
        match code {
            FaultCode::IoErr => 0x39,
            FaultCode::Full => 0x3A,
        },
        KeyPurpose::BarrierObservationSigning,
    );
    let signature = observation(&fixture.fence, &fixture.signer);
    let request = ExchangeRequest {
        envelope: Some(submit_request(&fixture.fence, &fixture.signer, signature)),
    };
    {
        let _reset = FaultReset;
        nlos_store_fault::arm(FaultMode::FailWritesAfter { remaining: 0, code });
        let response = exchange(
            Arc::clone(&fixture.authority),
            Arc::clone(&fixture.identity),
            request.clone(),
        )
        .await;
        let failure = response_failure(response.envelope());
        assert_eq!(failure.code, i32::from(SabiErrorCode::Durability));
        assert_eq!(
            failure.retry,
            i32::from(RetryDirective::RetrySameIdempotencyKey)
        );
        assert!(nlos_store_fault::writes_observed() > 0);
        assert!(
            fixture
                .authority
                .inspect_authority_takeover_barrier_receipts(fixture.fence.takeover_receipt_id)
                .unwrap()
                .is_empty()
        );
    }

    let response = exchange(
        Arc::clone(&fixture.authority),
        Arc::clone(&fixture.identity),
        request,
    )
    .await;
    validate_sabi_response_context(response.envelope(), MethodSemantics::MUTATION).unwrap();
    let record = decode_barrier_observation_record(&response.envelope().payload).unwrap();
    assert!(record.signed);
    assert_eq!(
        fixture
            .authority
            .inspect_authority_takeover_barrier_receipts(fixture.fence.takeover_receipt_id)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        fixture
            .authority
            .inspect_authority_takeover_barrier_coverage(fixture.fence.takeover_receipt_id)
            .unwrap()
            .state,
        AuthorityTakeoverBarrierCoverageState::LocallyCovered
    );
}

#[tokio::test]
async fn ipc_power_loss_write_loss_is_invisible_and_same_key_recovers() {
    let _serialization = fault_lock().await;
    nlos_store_fault::disarm();
    let fixture = Fixture::new_with_fault_vfs(
        "ipc-power-loss",
        0x3B,
        KeyPurpose::BarrierObservationSigning,
    );
    let signature = observation(&fixture.fence, &fixture.signer);
    let request = ExchangeRequest {
        envelope: Some(submit_request(&fixture.fence, &fixture.signer, signature)),
    };
    let Fixture {
        authority,
        identity,
        signer: _signer,
        fence,
        database,
        _identity_root,
    } = fixture;

    {
        let _reset = FaultReset;
        nlos_store_fault::arm(FaultMode::PowerLossAfter { remaining: 0 });
        let phantom = exchange(
            Arc::clone(&authority),
            Arc::clone(&identity),
            request.clone(),
        )
        .await;
        validate_sabi_response_context(phantom.envelope(), MethodSemantics::MUTATION).unwrap();
        let record = decode_barrier_observation_record(&phantom.envelope().payload).unwrap();
        assert!(record.signed);
        assert!(nlos_store_fault::writes_observed() > 0);
    }

    drop(authority);
    let recovered = Arc::new(
        SqliteTaskAuthority::open_with_vfs(&database.path, Some(FAULT_VFS_NAME))
            .expect("reopen after IPC power loss"),
    );
    assert!(
        recovered
            .inspect_authority_takeover_barrier_receipts(fence.takeover_receipt_id)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        recovered
            .inspect_authority_takeover_barrier_coverage(fence.takeover_receipt_id)
            .unwrap()
            .state,
        AuthorityTakeoverBarrierCoverageState::Partial
    );

    let response = exchange(Arc::clone(&recovered), Arc::clone(&identity), request).await;
    validate_sabi_response_context(response.envelope(), MethodSemantics::MUTATION).unwrap();
    let record = decode_barrier_observation_record(&response.envelope().payload).unwrap();
    assert!(record.signed);
    assert_eq!(
        recovered
            .inspect_authority_takeover_barrier_receipts(fence.takeover_receipt_id)
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        recovered
            .inspect_authority_takeover_barrier_coverage(fence.takeover_receipt_id)
            .unwrap()
            .state,
        AuthorityTakeoverBarrierCoverageState::LocallyCovered
    );
}

#[tokio::test]
async fn ipc_io_error_maps_to_durability_and_same_key_recovers() {
    run_ipc_write_fault(FaultCode::IoErr).await;
}

#[tokio::test]
async fn ipc_enospc_maps_to_durability_and_same_key_recovers() {
    run_ipc_write_fault(FaultCode::Full).await;
}

#[cfg(unix)]
#[tokio::test]
async fn signed_observation_uses_a_directory_resolved_exactly_bound_unix_endpoint() {
    use nlos_ipc::unix::{UnixListenerAdapter, connect};
    use nlos_ipc::{ExactPeerAuthorizer, PeerCredentialBinding};
    use nlos_schema::SABI_TAKEOVER_CONTROL_SCHEMA;
    use nlos_schema::sabi::v1::{
        LocalEndpoint, LocalTransportKind, NegotiateServiceRequest, ServiceCandidate,
        ServiceVersion, negotiate_service_response,
    };
    use nlos_service_directory::{ServiceRegistration, SnapshotDirectory};

    let fixture = Fixture::new("unix-endpoint", 0x32, KeyPurpose::BarrierObservationSigning);
    let socket_path = fixture.database.path.with_extension("sock");
    let listener = UnixListenerAdapter::bind(&socket_path).unwrap();
    let directory = SnapshotDirectory::new([ServiceRegistration {
        candidate: ServiceCandidate {
            binding_id: vec![0x61; 16],
            generation: 1,
            service: TAKEOVER_CONTROL_SERVICE.to_owned(),
            version: Some(ServiceVersion {
                schema_name: SABI_TAKEOVER_CONTROL_SCHEMA.to_owned(),
                major: 1,
                minor: 0,
            }),
            feature_ids: Vec::new(),
            transport_kinds: vec![LocalTransportKind::UnixSocket.into()],
        },
        endpoint: LocalEndpoint {
            kind: LocalTransportKind::UnixSocket.into(),
            address: socket_path.to_string_lossy().into_owned(),
        },
    }])
    .unwrap();
    let negotiation = directory.negotiate(&NegotiateServiceRequest {
        schema: Some(nlos_schema::service_directory_schema_identity()),
        service: TAKEOVER_CONTROL_SERVICE.to_owned(),
        schema_name: SABI_TAKEOVER_CONTROL_SCHEMA.to_owned(),
        major: 1,
        minimum_minor: 0,
        required_feature_ids: Vec::new(),
        supported_transport_kinds: vec![LocalTransportKind::UnixSocket.into()],
    });
    let negotiate_service_response::Result::Binding(binding) = negotiation.result.unwrap() else {
        panic!("expected binding");
    };
    let endpoint = binding.endpoint.unwrap().address;

    let signature = observation(&fixture.fence, &fixture.signer);
    let request = ExchangeRequest {
        envelope: Some(submit_request(&fixture.fence, &fixture.signer, signature)),
    };
    let config = transport_config();
    let authority = Arc::clone(&fixture.authority);
    let identity = Arc::clone(&fixture.identity);
    let server = tokio::spawn(async move {
        let (stream, peer) = listener.accept(config).await?;
        // Real peer gating: the exact credential tuple observed at accept is
        // the only tuple this connection may serve.
        let authorizer = ExactPeerAuthorizer::new(PeerCredentialBinding::from_peer(peer));
        serve_one(stream, config, peer, &authorizer, move |validated| {
            let response = match TakeoverControl::new(
                authority.as_ref(),
                identity.as_ref(),
                &CapabilityPolicy,
            )
            .handle(validated.envelope(), 10, 6_000)
            {
                Ok(envelope) => envelope,
                Err(error) => failure_envelope(validated.envelope(), &error),
            };
            async move {
                Ok(OutboundResponse::Typed(ExchangeResponse {
                    envelope: Some(response),
                }))
            }
        })
        .await
    });
    let (stream, peer) = connect(endpoint, config).await.unwrap();
    assert!(matches!(peer, PeerIdentity::Unix { .. }));
    let response = LocalRpcClient::new(stream, config)
        .exchange_validated(request)
        .await
        .unwrap();
    let record = decode_barrier_observation_record(&response.envelope().payload).unwrap();
    assert!(record.signed);
    server.await.unwrap().unwrap();
    fs::remove_file(socket_path).unwrap();
}

#[tokio::test]
async fn wrong_purpose_signing_key_fails_as_rights_over_ipc_without_durable_row() {
    let fixture = Fixture::new("wrong-purpose", 0x33, KeyPurpose::SemanticSigning);
    let signature = observation(&fixture.fence, &fixture.signer);
    let response = exchange(
        Arc::clone(&fixture.authority),
        Arc::clone(&fixture.identity),
        ExchangeRequest {
            envelope: Some(submit_request(&fixture.fence, &fixture.signer, signature)),
        },
    )
    .await;
    let failure = response_failure(response.envelope());
    assert_eq!(failure.code, i32::from(SabiErrorCode::Rights));
    assert_eq!(failure.retry, i32::from(RetryDirective::DoNotRetry));
    assert!(
        fixture
            .authority
            .inspect_authority_takeover_barrier_receipts(fixture.fence.takeover_receipt_id)
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn tampered_signature_fails_as_rights_over_ipc_without_durable_row() {
    let fixture = Fixture::new(
        "invalid-signature",
        0x34,
        KeyPurpose::BarrierObservationSigning,
    );
    let mut foreign_digest = observation_digest(&fixture.fence);
    foreign_digest[0] ^= 1;
    let signature = fixture.signer.key.sign(&foreign_digest).to_bytes();
    let response = exchange(
        Arc::clone(&fixture.authority),
        Arc::clone(&fixture.identity),
        ExchangeRequest {
            envelope: Some(submit_request(&fixture.fence, &fixture.signer, signature)),
        },
    )
    .await;
    let failure = response_failure(response.envelope());
    assert_eq!(failure.code, i32::from(SabiErrorCode::Rights));
    assert_eq!(failure.retry, i32::from(RetryDirective::DoNotRetry));
    assert!(
        fixture
            .authority
            .inspect_authority_takeover_barrier_receipts(fixture.fence.takeover_receipt_id)
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn signed_replay_of_an_unsigned_observation_fails_as_conflict() {
    let fixture = Fixture::new("mixed-replay", 0x35, KeyPurpose::BarrierObservationSigning);
    let unsigned = fixture
        .authority
        .record_authority_takeover_barrier_receipt(barrier_store_request(&fixture.fence))
        .expect("record unsigned barrier observation");
    assert_eq!(unsigned.signer, None);
    let signature = observation(&fixture.fence, &fixture.signer);
    let response = exchange(
        Arc::clone(&fixture.authority),
        Arc::clone(&fixture.identity),
        ExchangeRequest {
            envelope: Some(submit_request(&fixture.fence, &fixture.signer, signature)),
        },
    )
    .await;
    let failure = response_failure(response.envelope());
    assert_eq!(failure.code, i32::from(SabiErrorCode::Conflict));
    assert_eq!(failure.retry, i32::from(RetryDirective::DoNotRetry));
    assert_eq!(
        fixture
            .authority
            .inspect_authority_takeover_barrier_receipts(fixture.fence.takeover_receipt_id)
            .unwrap(),
        vec![unsigned]
    );
}

#[tokio::test]
async fn unknown_method_is_a_typed_not_supported_failure_over_ipc() {
    let fixture = Fixture::new(
        "unknown-method",
        0x36,
        KeyPurpose::BarrierObservationSigning,
    );
    let request = ExchangeRequest {
        envelope: Some(envelope("get", Vec::new())),
    };
    assert!(matches!(
        TakeoverControl::new(
            fixture.authority.as_ref(),
            fixture.identity.as_ref(),
            &CapabilityPolicy
        )
        .handle(request.envelope.as_ref().unwrap(), 10, 6_000),
        Err(TakeoverControlError::UnknownMethod)
    ));
    let response = exchange(
        Arc::clone(&fixture.authority),
        Arc::clone(&fixture.identity),
        request,
    )
    .await;
    let failure = response_failure(response.envelope());
    assert_eq!(failure.code, i32::from(SabiErrorCode::NotSupported));
    assert_eq!(failure.retry, i32::from(RetryDirective::DoNotRetry));
}
