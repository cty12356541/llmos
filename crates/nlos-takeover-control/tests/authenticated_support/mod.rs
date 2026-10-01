//! Platform-neutral fixture machinery shared by the authenticated-IPC
//! acceptance tests: durable stores, the wall-anchored clock, the
//! two-signature-layer fixture, and the typed request builders. Test
//! infrastructure only; each platform test file supplies its own listener
//! and spawn helpers.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer, SigningKey};
use nlos_clock::{AuthorityClock, AuthorityClockError, NowRequest, WallSource};
use nlos_identity::{BootstrapPrincipalRequest, IdentityAuthority, IdentityBinding, KeyPurpose};
use nlos_ipc::TransportConfig;
use nlos_schema::sabi::v1::{
    BarrierObservationEvidence, BarrierObservationSignature as BarrierObservationSignatureProto,
    BarrierObservationTarget, CallerIdentity, CapabilityHandle, Envelope, SabiRequestContext,
    SubmitBarrierObservationRequest, envelope,
};
use nlos_schema::{
    SABI_ENVELOPE_SCHEMA, encode_submit_barrier_observation_request,
    takeover_control_schema_identity,
};
use nlos_task::{
    AttemptSpec, AuthorityLeaseFinalizeRequest, AuthorityLeasePermitRequest, AuthorityLeaseRequest,
    AuthorityLeaseTakeoverFenceRequest, FinalizeRequest, FinalizeRequestV3, ParticipantRecord,
    PermitDecision, PermitRequest, SnapshotBundle, SqliteTaskAuthority, TaskSpec,
    barrier_observation_signature_message, empty_effect_history_root,
};
use nlos_types::{
    CancellationScopeId, Generation, IdempotencyKey, ProcessId, ReceiptId, TaskAttemptId, TaskId,
    TaskSnapshotId,
};

use nlos_takeover_control::{
    SUBMIT_BARRIER_OBSERVATION_METHOD, TAKEOVER_CONTROL_SERVICE, TakeoverControlAuthorizer,
    participant_type_code,
};

pub(crate) const NONCE: [u8; 32] = [0x5D; 32];
pub(crate) const WALL_ANCHOR_MS: u64 = 5_000;

/// First nonce a spawned server issues: the sequence byte replaces
/// `NONCE[0]`, so the first connection sees `[1, ...]`.
pub(crate) fn first_issued_nonce() -> [u8; 32] {
    let mut issued = NONCE;
    issued[0] = 1;
    issued
}

static NEXT: AtomicU64 = AtomicU64::new(1);

pub(crate) fn next_sequence() -> u64 {
    NEXT.fetch_add(1, Ordering::Relaxed)
}

pub(crate) struct TestDatabase {
    path: PathBuf,
}

impl TestDatabase {
    pub(crate) fn new(label: &str) -> Self {
        let sequence = next_sequence();
        Self {
            path: std::env::temp_dir().join(format!(
                "nta-{label}-{}-{sequence}.sqlite3",
                std::process::id()
            )),
        }
    }

    pub(crate) fn open(&self) -> SqliteTaskAuthority {
        SqliteTaskAuthority::open(&self.path).expect("open task authority")
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

pub(crate) struct TempRoot(PathBuf);

impl TempRoot {
    pub(crate) fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("monotonic clock")
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "nta-{label}-{}-{nonce}-{}",
            std::process::id(),
            next_sequence()
        )))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Deterministic wall source: the transport-layer verification instant is
/// anchored to this committed durable reading, never to the system clock.
pub(crate) struct ManualWallSource(Arc<AtomicU64>);

impl ManualWallSource {
    pub(crate) fn at(ms: u64) -> Self {
        Self(Arc::new(AtomicU64::new(ms)))
    }
}

impl WallSource for ManualWallSource {
    fn now_ms(&self) -> Result<u64, AuthorityClockError> {
        Ok(self.0.load(Ordering::Relaxed))
    }
}

pub(crate) struct TestSigner {
    pub(crate) key: SigningKey,
    pub(crate) binding: IdentityBinding,
}

pub(crate) fn bootstrap_test_signer(
    identity: &IdentityAuthority,
    seed: u8,
    purpose: KeyPurpose,
    valid_from_ms: u64,
    valid_until_ms: u64,
) -> TestSigner {
    let key = SigningKey::from_bytes(&[seed; 32]);
    let binding = identity
        .bootstrap_principal(BootstrapPrincipalRequest {
            principal_profile_digest: [seed.wrapping_add(1); 32],
            control_domain_policy_digest: [seed.wrapping_add(2); 32],
            public_key: key.verifying_key().to_bytes(),
            key_purpose: purpose,
            key_valid_from_ms: valid_from_ms,
            key_valid_until_ms: valid_until_ms,
            idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(3); 16]),
            created_at_ms: 0,
        })
        .expect("bootstrap signer")
        .binding();
    TestSigner { key, binding }
}

pub(crate) struct FrozenFence {
    pub(crate) takeover_receipt_id: ReceiptId,
    pub(crate) participant: ParticipantRecord,
    pub(crate) fence_set_root: [u8; 32],
}

fn lease_request(holder: u8, key: u8, at_ms: i64, ttl_ms: i64) -> AuthorityLeaseRequest {
    AuthorityLeaseRequest {
        holder_id: ProcessId::from_bytes([key.wrapping_add(holder); 16]),
        idempotency_key: IdempotencyKey::from_bytes([key; 16]),
        requested_at_ms: at_ms,
        ttl_ms,
    }
}

fn register_task_attempt(authority: &SqliteTaskAuthority, seed: u8) -> AttemptSpec {
    let task_id = TaskId::from_bytes([seed; 16]);
    authority
        .register_task(TaskSpec {
            task_id,
            task_generation: Generation::INITIAL,
            registered_at_ms: 1,
            application_id: None,
            plan_revision: None,
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

fn fence_takeover(
    authority: &SqliteTaskAuthority,
    clock: &AuthorityClock,
    seed: u8,
) -> FrozenFence {
    // Clock-authority anchor: the fixture's durable wall high-water
    // (`WALL_ANCHOR_MS`), read without durable side effect — not a raw
    // system-clock reading.
    let wall_ms = clock.inspect_wall().expect("durable wall reading").as_u64();
    let lease_one = authority
        .acquire_authority_lease_anchored(lease_request(1, seed, 100, 100), wall_ms)
        .expect("initial lease")
        .record();
    let attempt = register_task_attempt(authority, seed);
    let permit = match authority
        .request_commit_permit_with_authority_lease(AuthorityLeasePermitRequest {
            permit: PermitRequest {
                task_id: attempt.task_id,
                attempt_id: attempt.attempt_id,
                attempt_generation: attempt.attempt_generation,
                write_set_root: [seed; 32],
                planned_effects: Vec::new(),
                idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(10); 16]),
                valid_until_ms: 10_000,
                requested_at_ms: 150,
            },
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
            finalize: FinalizeRequestV3 {
                base: FinalizeRequest {
                    task_id: attempt.task_id,
                    attempt_id: attempt.attempt_id,
                    attempt_generation: attempt.attempt_generation,
                    permit_id: permit.permit_id,
                    new_effect_history_root: [0; 32],
                    new_retry_fence_epoch: 0,
                    finalized_at_ms: 160,
                },
                required_satisfaction: Vec::new(),
                fenced_participant_digest: [0; 32],
            },
            lease: lease_one,
        })
        .expect("close permit before takeover");
    let lease_two = authority
        .acquire_authority_lease_anchored(
            lease_request(2, seed.wrapping_add(0x11), 201, 1_000),
            wall_ms,
        )
        .expect("takeover lease")
        .record();
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

pub(crate) struct Fixture {
    pub(crate) authority: Arc<SqliteTaskAuthority>,
    pub(crate) identity: Arc<IdentityAuthority>,
    pub(crate) clock: Arc<AuthorityClock>,
    pub(crate) handshake: TestSigner,
    pub(crate) barrier: TestSigner,
    pub(crate) fence: FrozenFence,
    // Held only for its Drop-time SQLite connection close and file cleanup;
    // the path is never read after Fixture::new opens the authority.
    #[expect(dead_code)]
    database: TestDatabase,
    _identity_root: TempRoot,
    _clock_root: TempRoot,
}

impl Fixture {
    /// Builds the full authenticated-serving fixture. The clock's wall
    /// domain is committed once at `WALL_ANCHOR_MS`, and the handshake
    /// principal's key window is `[1_000, 5_500]`: a fresh-store reading of
    /// 0 fails `KeyNotYetValid` and a real system-clock reading fails
    /// `KeyExpired`, so only the committed durable wall reading verifies.
    pub(crate) fn new(label: &str) -> Self {
        let database = TestDatabase::new(label);
        let identity_root = TempRoot::new(label);
        let clock_root = TempRoot::new(label);
        let authority = Arc::new(database.open());
        let identity = Arc::new(IdentityAuthority::open(identity_root.path()).unwrap());
        let clock = Arc::new(
            AuthorityClock::open_with_wall_source(
                clock_root.path(),
                ManualWallSource::at(WALL_ANCHOR_MS),
            )
            .unwrap(),
        );
        let advanced = clock
            .wall_now(NowRequest {
                idempotency_key: IdempotencyKey::from_bytes([0x5A; 16]),
            })
            .expect("commit the wall anchor reading");
        assert_eq!(advanced.reading().as_u64(), WALL_ANCHOR_MS);
        // Transport layer: SemanticSigning key, valid only at the anchored wall reading.
        let handshake =
            bootstrap_test_signer(&identity, 0x43, KeyPurpose::SemanticSigning, 1_000, 5_500);
        // Payload layer: BarrierObservationSigning key under the unchanged
        // store-side time semantics.
        let barrier = bootstrap_test_signer(
            &identity,
            0x44,
            KeyPurpose::BarrierObservationSigning,
            0,
            10_000,
        );
        let fence = fence_takeover(&authority, clock.as_ref(), 0x45);
        Self {
            authority,
            identity,
            clock,
            handshake,
            barrier,
            fence,
            database,
            _identity_root: identity_root,
            _clock_root: clock_root,
        }
    }
}

pub(crate) struct CapabilityPolicy;

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

pub(crate) struct AllowPeer;

impl nlos_ipc::PeerAuthorizer for AllowPeer {
    fn authorize(&self, _: &nlos_ipc::PeerIdentity) -> Result<(), String> {
        Ok(())
    }
}

pub(crate) fn remote_receipt_id() -> ReceiptId {
    ReceiptId::from_bytes([0x91; 16])
}

pub(crate) fn barrier_digest() -> [u8; 32] {
    [0x92; 32]
}

pub(crate) const OBSERVED_AT_MS: i64 = 220;

pub(crate) fn observation(fence: &FrozenFence, signer: &TestSigner) -> [u8; 64] {
    let digest = barrier_observation_signature_message(
        fence.takeover_receipt_id,
        &fence.participant,
        remote_receipt_id(),
        barrier_digest(),
        fence.fence_set_root,
    );
    signer.key.sign(&digest).to_bytes()
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

pub(crate) fn submit_request(
    fence: &FrozenFence,
    signer: &TestSigner,
    signature: [u8; 64],
) -> Envelope {
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
    Envelope {
        schema: Some(nlos_schema::sabi::v1::SchemaIdentity {
            name: SABI_ENVELOPE_SCHEMA.to_owned(),
            major: 1,
            minor: 1,
            critical_extension_ids: Vec::new(),
            non_critical_extension_ids: Vec::new(),
        }),
        request_id: vec![0x35; 16],
        service: TAKEOVER_CONTROL_SERVICE.to_owned(),
        method: SUBMIT_BARRIER_OBSERVATION_METHOD.to_owned(),
        common_context: Some(envelope::CommonContext::RequestContext(request_context())),
        payload: encode_submit_barrier_observation_request(&submit).unwrap(),
    }
}

pub(crate) fn transport_config() -> TransportConfig {
    TransportConfig::new(
        64 * 1024,
        Duration::from_secs(5),
        Duration::from_secs(5),
        Duration::from_secs(5),
    )
    .unwrap()
}

pub(crate) fn durable_rows(fixture: &Fixture) -> usize {
    fixture
        .authority
        .inspect_authority_takeover_barrier_receipts(fixture.fence.takeover_receipt_id)
        .expect("inspect durable barrier rows")
        .len()
}
