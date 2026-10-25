//! End-to-end integration tests for `#9 C-PROVIDER-REAL` (`ADR-0024` +
//! `[TASK-PROVIDER-001]`): real out-of-process OS child provider worker
//! (`nlos-provider-worker`), cross-platform ADR-0011 authenticated IPC
//! (`UnixStream` on Unix, Windows Named Pipe on Windows), frozen
//! `nlos.sabi.DriverProvider` v1.0 protobuf payloads, `ProviderCache`
//! crash/respawn degradation and replay, stale generation fail-closed fences,
//! cancel-epoch durable reconciliation across child-process restarts, and
//! `[TASK-PROVIDER-001]` isolation posture enforcement.

use std::fs;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer, SigningKey};
use nlos_clock::{AuthorityClock, NowRequest};
use nlos_driver_mock::proto_codec::{
    CANCEL_PROVIDER_METHOD, REGISTER_PROVIDER_METHOD, encode_query_isolation_request_proto,
};
use nlos_driver_mock::{
    CacheHealth, CancelProviderOperation, CompleteProviderOperation, DegradationCause,
    DispatchProviderOperation, IsolationGuarantee, MockProvider, OutOfProcessProviderCache,
    OutOfProcessProviderDriver, OutOfProcessProviderError, PROVIDER_ISOLATION_UNVERIFIED_TAG,
    PROVIDER_ISOLATION_VERIFIED_TAG, ProviderCache, ProviderClientCredentials,
    ProviderIsolationAttestation, ProviderIsolationPosture, ProviderIsolationProfile,
    ProviderWorkerConfig, derive_worker_compute_digest,
};
use nlos_identity::{BootstrapDecision, BootstrapPrincipalRequest, IdentityAuthority, KeyPurpose};
use nlos_ipc::{FramedIo, TransportConfig};
use nlos_operation::{OperationError, OperationHandle, OperationSpec, OperationState};
use nlos_runtime::FiberHandle;
use nlos_schema::sabi::v1::{
    GetSystemControlRequest, RetryDirective, SabiErrorCode, SystemControlView,
};
use nlos_schema::{
    MAX_DRIVER_PROVIDER_PAYLOAD_BYTES, MethodSemantics, encode_get_system_control_request,
    system_control_schema_identity,
};
use nlos_store::{OutboxKind, SqliteOperationStore, StoreError};
use nlos_types::{
    CallbackId, CancelEpoch, CancellationScopeId, ExecutionFiberId, Generation, IdempotencyKey,
    OperationId, PrincipalId, ReceiptId,
};

const KEY_VALID_UNTIL_MS: u64 = 4_102_444_800_000;
static ENDPOINT_SEQ: AtomicU64 = AtomicU64::new(1);

struct TestWorkspace {
    root: PathBuf,
    endpoint: PathBuf,
}

impl TestWorkspace {
    fn new(label: &str) -> Self {
        let seq = ENDPOINT_SEQ.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .subsec_nanos();
        let root = std::env::temp_dir().join(format!("nlos-oop-{label}-{pid}-{seq}-{nanos}"));
        fs::create_dir_all(&root).expect("create test workspace root");
        let endpoint = platform_endpoint(label, pid, seq, nanos);
        Self { root, endpoint }
    }

    fn store_root(&self) -> PathBuf {
        self.root.join("ops")
    }

    fn identity_root(&self) -> PathBuf {
        self.root.join("identity")
    }

    fn clock_root(&self) -> PathBuf {
        self.root.join("clock")
    }

    fn probe_store_root(&self) -> PathBuf {
        self.root.join("cache_probe")
    }

    fn open_store(&self) -> SqliteOperationStore {
        SqliteOperationStore::open(self.store_root()).expect("open operation store")
    }
}

impl Drop for TestWorkspace {
    fn drop(&mut self) {
        #[cfg(unix)]
        let _ = fs::remove_file(&self.endpoint);
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[cfg(windows)]
fn platform_endpoint(label: &str, pid: u32, seq: u64, nanos: u32) -> PathBuf {
    PathBuf::from(format!(r"\\.\pipe\nlos-oop-{label}-{pid}-{seq}-{nanos}"))
}

#[cfg(unix)]
fn platform_endpoint(label: &str, pid: u32, seq: u64, nanos: u32) -> PathBuf {
    // Keep Unix domain socket paths well below SUN_LEN (104/108 bytes).
    let short: String = label.chars().take(8).collect();
    std::env::temp_dir().join(format!("np-{short}-{pid}-{seq}-{nanos}.s"))
}

struct EnrolledPrincipal {
    id: PrincipalId,
    signing: SigningKey,
}

fn setup_authorities(ws: &TestWorkspace, seeds: &[u8]) -> Vec<EnrolledPrincipal> {
    let identity = IdentityAuthority::open(ws.identity_root()).expect("open identity authority");
    let mut enrolled = Vec::with_capacity(seeds.len());
    for &seed in seeds {
        let signing = SigningKey::from_bytes(&[seed; 32]);
        let BootstrapDecision::Created(binding) = identity
            .bootstrap_principal(BootstrapPrincipalRequest {
                principal_profile_digest: [seed.wrapping_add(1); 32],
                control_domain_policy_digest: [seed.wrapping_add(2); 32],
                public_key: signing.verifying_key().to_bytes(),
                key_purpose: KeyPurpose::SemanticSigning,
                key_valid_from_ms: 0,
                key_valid_until_ms: KEY_VALID_UNTIL_MS,
                idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(3); 16]),
                created_at_ms: 0,
            })
            .expect("bootstrap principal")
        else {
            panic!("fresh identity root must create principal");
        };
        enrolled.push(EnrolledPrincipal {
            id: binding.principal_id,
            signing,
        });
    }
    let clock = AuthorityClock::open(ws.clock_root()).expect("open clock authority");
    clock
        .wall_now(NowRequest {
            idempotency_key: IdempotencyKey::from_bytes([0xE1; 16]),
        })
        .expect("advance wall clock");
    let _ = SqliteOperationStore::open(ws.store_root()).expect("initialize operation store");
    enrolled
}

fn worker_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nlos-provider-worker"))
}

fn client_transport_config() -> TransportConfig {
    TransportConfig::new(
        64 * 1024,
        Duration::from_millis(350),
        Duration::from_secs(5),
        Duration::from_secs(5),
    )
    .expect("valid transport config")
}

fn credentials_for(principal_id: PrincipalId, signing: &SigningKey) -> ProviderClientCredentials {
    let key = signing.clone();
    ProviderClientCredentials {
        principal_id,
        application_id: [0xA1; 16],
        process_id: [0xB2; 16],
        process_generation: NonZeroU64::MIN,
        capability_slot: 7,
        capability_generation: NonZeroU64::MIN,
        signer: Arc::new(move |digest: &[u8; 32]| Ok(key.sign(digest).to_bytes())),
    }
}

fn worker_config(
    ws: &TestWorkspace,
    profile: ProviderIsolationProfile,
    attestation_digest_override: Option<[u8; 32]>,
    allowed_principal: Option<PrincipalId>,
) -> ProviderWorkerConfig {
    ProviderWorkerConfig {
        worker_binary: worker_binary(),
        store_root: ws.store_root(),
        identity_root: ws.identity_root(),
        clock_root: ws.clock_root(),
        endpoint: ws.endpoint.clone(),
        isolation_profile: profile,
        attestation_digest_override,
        allowed_principal,
        max_requests: None,
    }
}

fn sample_spec(tag: u8) -> OperationSpec {
    OperationSpec {
        operation_id: OperationId::from_bytes([tag; 16]),
        generation: Generation::INITIAL,
        owner_fiber: FiberHandle {
            fiber_id: ExecutionFiberId::from_bytes([tag.wrapping_add(1); 16]),
            generation: Generation::INITIAL,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes([tag.wrapping_add(2); 16]),
        cancellation_generation: Generation::INITIAL,
    }
}

const fn handle_of(spec: OperationSpec) -> OperationHandle {
    OperationHandle {
        operation_id: spec.operation_id,
        generation: spec.generation,
    }
}

const fn sample_dispatch(spec: OperationSpec, tag: u8) -> DispatchProviderOperation {
    DispatchProviderOperation {
        handle: handle_of(spec),
        callback_id: CallbackId::from_bytes([tag.wrapping_add(0x30); 16]),
    }
}

fn outbox_kinds_for(store: &SqliteOperationStore, operation: OperationHandle) -> Vec<OutboxKind> {
    store
        .pending_outbox(64)
        .expect("pending outbox")
        .into_iter()
        .filter(|entry| entry.operation == operation)
        .map(|entry| entry.kind)
        .collect()
}

async fn send_unauthenticated_raw_frame(endpoint: &Path, config: TransportConfig) {
    #[cfg(windows)]
    let (stream, _) = nlos_ipc::windows::connect(endpoint, config)
        .await
        .expect("raw pipe connect");
    #[cfg(unix)]
    let (stream, _) = nlos_ipc::unix::connect(endpoint, config)
        .await
        .expect("raw unix connect");
    let mut framed = FramedIo::new(stream, config);
    let challenge = framed.receive().await.expect("worker challenge");
    assert_ne!(challenge, Vec::<u8>::new());
    // Send a non-attestation garbage frame directly where an ADR-0011
    // attestation is required; the worker must drop the connection before
    // entering `serve_one`.
    let _ = framed.send(b"unauthenticated-sabi-bypass").await;
    assert!(
        framed.receive().await.is_err(),
        "worker must close connection without serving unauthenticated frame"
    );
}

#[tokio::test]
async fn real_child_worker_end_to_end_lifecycle_over_authenticated_ipc_and_frozen_protobuf() {
    let ws = TestWorkspace::new("e2e");
    let principals = setup_authorities(&ws, &[0x11]);
    let driver = OutOfProcessProviderDriver::spawn(
        worker_config(
            &ws,
            ProviderIsolationProfile::Verified,
            None,
            Some(principals[0].id),
        ),
        credentials_for(principals[0].id, &principals[0].signing),
        client_transport_config(),
    )
    .expect("spawn provider worker");

    let worker_pid = driver.worker_pid().expect("worker running");
    assert_ne!(
        worker_pid,
        std::process::id(),
        "provider worker must run in a distinct OS child process"
    );

    let (attestation, posture) = driver.query_isolation().await.expect("query isolation");
    assert!(matches!(
        posture,
        ProviderIsolationPosture::VerifiedEndToEndIsolated { .. }
    ));
    assert_ne!(attestation.attestation_digest, [0; 32]);

    let spec = sample_spec(0x21);
    let handle = handle_of(spec);
    let reg = driver
        .register(spec, IdempotencyKey::from_bytes([0x22; 16]))
        .await
        .expect("register over IPC");
    assert!(!reg.replayed);
    assert_eq!(reg.handle, handle);
    assert_ne!(reg.admission_receipt_id, ReceiptId::from_bytes([0; 16]));

    let dispatch_req = sample_dispatch(spec, 0x21);
    let dispatched = driver
        .dispatch(dispatch_req, IdempotencyKey::from_bytes([0x23; 16]))
        .await
        .expect("dispatch over IPC");
    assert!(!dispatched.replayed);
    assert_eq!(dispatched.ticket.callback_id, dispatch_req.callback_id);
    assert_ne!(
        dispatched.preparation_receipt_id,
        ReceiptId::from_bytes([0; 16])
    );
    assert_ne!(
        dispatched.activation_receipt_id,
        ReceiptId::from_bytes([0; 16])
    );

    let seed = [0x44; 32];
    let completed = driver
        .complete(
            CompleteProviderOperation {
                handle,
                callback_id: dispatched.ticket.callback_id,
                seed,
            },
            IdempotencyKey::from_bytes([0x24; 16]),
        )
        .await
        .expect("complete over IPC");
    assert!(!completed.replayed);
    assert!(!completed.fenced_reconciliation_only);
    assert_eq!(completed.isolation_marker, PROVIDER_ISOLATION_VERIFIED_TAG);
    assert_eq!(
        completed.compute_digest,
        derive_worker_compute_digest(handle, dispatched.ticket.callback_id, &seed)
    );

    let store = ws.open_store();
    let snapshot = store.inspect(handle).expect("durable snapshot");
    assert_eq!(snapshot.state, completed.state);
    assert!(snapshot.state.is_terminal());
    assert_eq!(
        outbox_kinds_for(&store, handle),
        vec![OutboxKind::WakeFiber]
    );
}

async fn run_single_lifecycle(
    driver: &OutOfProcessProviderDriver,
    tag: u8,
) -> nlos_driver_mock::ProviderExecutionReceipt {
    let spec = sample_spec(tag);
    driver
        .register(spec, IdempotencyKey::from_bytes([tag.wrapping_add(1); 16]))
        .await
        .expect("register");
    let disp = driver
        .dispatch(
            sample_dispatch(spec, tag),
            IdempotencyKey::from_bytes([tag.wrapping_add(2); 16]),
        )
        .await
        .expect("dispatch");
    driver
        .complete(
            CompleteProviderOperation {
                handle: handle_of(spec),
                callback_id: disp.ticket.callback_id,
                seed: [tag.wrapping_add(3); 32],
            },
            IdempotencyKey::from_bytes([tag.wrapping_add(4); 16]),
        )
        .await
        .expect("complete")
}

#[tokio::test]
async fn task_provider_001_isolation_posture_marks_unverified_profiles_and_zero_digests() {
    // Subcase A: shared-cache profile (`cache_partitioned == false`) even with a non-zero digest -> PROVIDER_ISOLATION_UNVERIFIED.
    let ws_shared = TestWorkspace::new("iso-shared");
    let p_shared = setup_authorities(&ws_shared, &[0x21]);
    let shared_driver = OutOfProcessProviderDriver::spawn(
        worker_config(
            &ws_shared,
            ProviderIsolationProfile::SharedCacheUnverified,
            Some([0xAA; 32]),
            None,
        ),
        credentials_for(p_shared[0].id, &p_shared[0].signing),
        client_transport_config(),
    )
    .expect("spawn shared-cache worker");
    let comp_a = run_single_lifecycle(&shared_driver, 0x31).await;
    assert_eq!(comp_a.isolation_marker, PROVIDER_ISOLATION_UNVERIFIED_TAG);
    assert_eq!(
        comp_a.isolation_posture,
        ProviderIsolationPosture::ProviderIsolationUnverified {
            missing_guarantees: vec![IsolationGuarantee::CachePartitioned],
        }
    );

    // Subcase B: verified booleans with all-zero attestation digest -> PROVIDER_ISOLATION_UNVERIFIED.
    let ws_zero = TestWorkspace::new("iso-zero");
    let p_zero = setup_authorities(&ws_zero, &[0x25]);
    let zero_driver = OutOfProcessProviderDriver::spawn(
        worker_config(
            &ws_zero,
            ProviderIsolationProfile::Verified,
            Some([0_u8; 32]),
            None,
        ),
        credentials_for(p_zero[0].id, &p_zero[0].signing),
        client_transport_config(),
    )
    .expect("spawn zero-digest worker");
    let comp_b = run_single_lifecycle(&zero_driver, 0x41).await;
    assert_eq!(comp_b.isolation_marker, PROVIDER_ISOLATION_UNVERIFIED_TAG);
    assert_eq!(
        comp_b.isolation_posture,
        ProviderIsolationPosture::ProviderIsolationUnverified {
            missing_guarantees: vec![IsolationGuarantee::NonZeroAttestationDigest],
        }
    );

    // Subcase C: all guarantees missing enumerates every missing obligation deterministically.
    let unattested = ProviderIsolationAttestation {
        tenant_isolated: false,
        session_ephemeral: false,
        cache_partitioned: false,
        zero_data_retention: false,
        attestation_digest: [0; 32],
        provider_profile: "external-unattested".to_owned(),
    };
    assert_eq!(
        unattested.evaluate_posture(),
        ProviderIsolationPosture::ProviderIsolationUnverified {
            missing_guarantees: vec![
                IsolationGuarantee::TenantIsolated,
                IsolationGuarantee::SessionEphemeral,
                IsolationGuarantee::CachePartitioned,
                IsolationGuarantee::ZeroDataRetention,
                IsolationGuarantee::NonZeroAttestationDigest,
            ],
        }
    );
}

#[tokio::test]
async fn unauthenticated_forged_and_unauthorized_principals_rejected_before_store() {
    let ws = TestWorkspace::new("auth");
    let principals = setup_authorities(&ws, &[0x51, 0x52]);
    let principal_a = &principals[0];
    let principal_b = &principals[1];

    let authorized_driver = OutOfProcessProviderDriver::spawn(
        worker_config(
            &ws,
            ProviderIsolationProfile::Verified,
            None,
            Some(principal_a.id),
        ),
        credentials_for(principal_a.id, &principal_a.signing),
        client_transport_config(),
    )
    .expect("spawn authorized worker");
    let worker_pid = authorized_driver.worker_pid().expect("worker alive");
    let spec = sample_spec(0x61);

    // 1. Raw unauthenticated frame bypassing ADR-0011 attestation.
    send_unauthenticated_raw_frame(authorized_driver.endpoint(), client_transport_config()).await;

    // 2. Unenrolled principal rejected during ADR-0011 handshake before `serve_one`.
    let unknown_id = PrincipalId::from_bytes([0xEE; 16]);
    let unknown_key = SigningKey::from_bytes(&[0xEE; 32]);
    let unknown_driver = OutOfProcessProviderDriver::connect_existing(
        worker_config(
            &ws,
            ProviderIsolationProfile::Verified,
            None,
            Some(principal_a.id),
        ),
        credentials_for(unknown_id, &unknown_key),
        client_transport_config(),
    );
    assert!(
        unknown_driver
            .register(spec, IdempotencyKey::from_bytes([0x62; 16]))
            .await
            .is_err()
    );

    // 3. Forged signature claiming `principal_a.id` with wrong Ed25519 key.
    let forged_driver = OutOfProcessProviderDriver::connect_existing(
        worker_config(
            &ws,
            ProviderIsolationProfile::Verified,
            None,
            Some(principal_a.id),
        ),
        credentials_for(principal_a.id, &unknown_key),
        client_transport_config(),
    );
    assert!(
        forged_driver
            .register(spec, IdempotencyKey::from_bytes([0x63; 16]))
            .await
            .is_err()
    );

    // 4. Enrolled `principal_b` passes ADR-0011 handshake but is denied by worker policy (`Rights`).
    let unauthorized_driver = OutOfProcessProviderDriver::connect_existing(
        worker_config(
            &ws,
            ProviderIsolationProfile::Verified,
            None,
            Some(principal_a.id),
        ),
        credentials_for(principal_b.id, &principal_b.signing),
        client_transport_config(),
    );
    let err = unauthorized_driver
        .register(spec, IdempotencyKey::from_bytes([0x64; 16]))
        .await
        .expect_err("principal_b must be denied");
    assert!(matches!(
        err,
        OutOfProcessProviderError::RemoteRejected {
            code: SabiErrorCode::Rights,
            retry: RetryDirective::DoNotRetry,
            ..
        }
    ));

    // Verify zero durable side effects landed in `SqliteOperationStore`.
    let store = ws.open_store();
    assert!(matches!(
        store.inspect(handle_of(spec)),
        Err(StoreError::Operation(OperationError::InvalidGeneration))
    ));

    // Worker survived all four attacks on the exact same OS PID and admits `principal_a`.
    assert_eq!(authorized_driver.worker_pid(), Some(worker_pid));
    let admitted = authorized_driver
        .register(spec, IdempotencyKey::from_bytes([0x65; 16]))
        .await
        .expect("authorized principal succeeds");
    assert!(!admitted.replayed);
}

#[tokio::test]
async fn worker_crash_triggers_provider_cache_degradation_and_respawn_replays_cleanly() {
    let ws = TestWorkspace::new("crash");
    let principals = setup_authorities(&ws, &[0x71]);
    let probe_store = Arc::new(
        SqliteOperationStore::open(ws.probe_store_root()).expect("open probe operation store"),
    );
    let provider_cache = Arc::new(ProviderCache::new(Arc::new(MockProvider::new(probe_store))));

    let driver = Arc::new(
        OutOfProcessProviderDriver::spawn(
            worker_config(
                &ws,
                ProviderIsolationProfile::Verified,
                None,
                Some(principals[0].id),
            ),
            credentials_for(principals[0].id, &principals[0].signing),
            client_transport_config(),
        )
        .expect("spawn initial worker")
        .with_provider_cache(Arc::clone(&provider_cache)),
    );
    let oop_cache = OutOfProcessProviderCache::new(Arc::clone(&driver));
    let pid_before = driver.worker_pid().expect("initial worker pid");

    let spec = sample_spec(0x72);
    let handle = handle_of(spec);
    let reg_key = IdempotencyKey::from_bytes([0x73; 16]);
    let disp_key = IdempotencyKey::from_bytes([0x74; 16]);
    let comp_key = IdempotencyKey::from_bytes([0x75; 16]);
    let disp_req = sample_dispatch(spec, 0x72);

    let reg_1 = driver.register(spec, reg_key).await.expect("register 1");
    let disp_1 = driver
        .dispatch(disp_req, disp_key)
        .await
        .expect("dispatch 1");
    assert_eq!(provider_cache.health(), CacheHealth::Healthy);
    assert_eq!(oop_cache.health(), CacheHealth::Healthy);

    // Hard-kill the real OS child process while the operation is in flight.
    driver.kill_worker().expect("kill worker process");
    assert!(driver.worker_pid().is_none());

    let complete_req = CompleteProviderOperation {
        handle,
        callback_id: disp_1.ticket.callback_id,
        seed: [0x76; 32],
    };
    let crash_err = driver
        .complete(complete_req, comp_key)
        .await
        .expect_err("dead worker must fail as Unreachable");
    assert!(matches!(
        crash_err,
        OutOfProcessProviderError::Unreachable(_)
    ));
    let expected_degraded = CacheHealth::Degraded {
        cause: DegradationCause::ProviderUnreachable,
    };
    assert_eq!(provider_cache.health(), expected_degraded);
    assert_eq!(oop_cache.health(), expected_degraded);

    // Respawn worker against the same durable authority roots and replay in-flight work.
    let pid_after = driver.respawn_worker().expect("respawn worker");
    assert_eq!(driver.worker_pid(), Some(pid_after));
    assert_ne!(pid_before, 0);

    let reg_replay = driver
        .register(spec, reg_key)
        .await
        .expect("register replay after respawn");
    assert!(reg_replay.replayed);
    assert_eq!(reg_replay.admission_receipt_id, reg_1.admission_receipt_id);
    assert_eq!(provider_cache.health(), CacheHealth::Healthy);
    assert_eq!(oop_cache.health(), CacheHealth::Healthy);

    let disp_replay = driver
        .dispatch(disp_req, disp_key)
        .await
        .expect("dispatch replay after respawn");
    assert!(disp_replay.replayed);
    assert_eq!(disp_replay.ticket, disp_1.ticket);
    assert_eq!(
        disp_replay.preparation_receipt_id,
        disp_1.preparation_receipt_id
    );
    assert_eq!(
        disp_replay.activation_receipt_id,
        disp_1.activation_receipt_id
    );

    let comp_1 = driver
        .complete(complete_req, comp_key)
        .await
        .expect("complete after respawn");
    assert!(!comp_1.replayed);
    let comp_replay = driver
        .complete(complete_req, comp_key)
        .await
        .expect("complete idempotent replay");
    assert!(comp_replay.replayed);
    assert_eq!(comp_replay.receipt_id, comp_1.receipt_id);
    assert_eq!(comp_replay.compute_digest, comp_1.compute_digest);
    assert_eq!(
        outbox_kinds_for(&ws.open_store(), handle),
        vec![OutboxKind::WakeFiber]
    );
}

#[tokio::test]
async fn stale_generation_fails_closed_over_out_of_process_channel_with_zero_durable_mutation() {
    let ws = TestWorkspace::new("stale-gen");
    let principals = setup_authorities(&ws, &[0x81]);
    let driver = OutOfProcessProviderDriver::spawn(
        worker_config(
            &ws,
            ProviderIsolationProfile::Verified,
            None,
            Some(principals[0].id),
        ),
        credentials_for(principals[0].id, &principals[0].signing),
        client_transport_config(),
    )
    .expect("spawn worker");

    let spec = sample_spec(0x82);
    let handle = handle_of(spec);
    driver
        .register(spec, IdempotencyKey::from_bytes([0x83; 16]))
        .await
        .expect("register gen 1");

    let mut stale_dispatch = sample_dispatch(spec, 0x82);
    stale_dispatch.handle.generation = Generation::new(NonZeroU64::new(2).expect("nonzero"));
    let err = driver
        .dispatch(stale_dispatch, IdempotencyKey::from_bytes([0x84; 16]))
        .await
        .expect_err("stale generation must fail closed");
    assert!(matches!(
        err,
        OutOfProcessProviderError::RemoteRejected {
            code: SabiErrorCode::Conflict,
            retry: RetryDirective::DoNotRetry,
            ..
        }
    ));
    assert_eq!(driver.health(), CacheHealth::Healthy);

    let store = ws.open_store();
    let snapshot = store.inspect(handle).expect("inspect gen 1");
    assert_eq!(snapshot.state, OperationState::Registered);
    assert!(matches!(
        store.inspect_activation_proof(handle),
        Err(StoreError::DispatchPreparationNotFound)
    ));
    assert_eq!(outbox_kinds_for(&store, handle), Vec::<OutboxKind>::new());
}

#[tokio::test]
async fn cancel_epoch_fence_wins_over_late_completion_across_worker_restart() {
    let ws = TestWorkspace::new("cancel-fence");
    let principals = setup_authorities(&ws, &[0x91]);
    let driver = OutOfProcessProviderDriver::spawn(
        worker_config(
            &ws,
            ProviderIsolationProfile::Verified,
            None,
            Some(principals[0].id),
        ),
        credentials_for(principals[0].id, &principals[0].signing),
        client_transport_config(),
    )
    .expect("spawn worker");

    let spec = sample_spec(0x92);
    let handle = handle_of(spec);
    driver
        .register(spec, IdempotencyKey::from_bytes([0x93; 16]))
        .await
        .expect("register");
    let dispatched = driver
        .dispatch(
            sample_dispatch(spec, 0x92),
            IdempotencyKey::from_bytes([0x94; 16]),
        )
        .await
        .expect("dispatch");
    assert_eq!(dispatched.ticket.cancel_epoch, CancelEpoch::new(0));

    let cancel_req = CancelProviderOperation {
        handle,
        expected_cancel_epoch: CancelEpoch::new(0),
        no_effect_receipt_id: ReceiptId::from_bytes([0x95; 16]),
    };
    let cancelled = driver
        .cancel(cancel_req, IdempotencyKey::from_bytes([0x96; 16]))
        .await
        .expect("cancel");
    assert!(!cancelled.replayed);
    assert_eq!(cancelled.state, OperationState::CancelRequested);
    assert_eq!(cancelled.cancel_epoch, CancelEpoch::new(1));

    let cancel_replay = driver
        .cancel(cancel_req, IdempotencyKey::from_bytes([0x96; 16]))
        .await
        .expect("cancel replay");
    assert!(cancel_replay.replayed);
    assert_eq!(cancel_replay.cancel_epoch, CancelEpoch::new(1));

    // Crash and respawn the child worker before the late pre-cancel callback arrives.
    driver.respawn_worker().expect("respawn worker");

    let late_complete = CompleteProviderOperation {
        handle,
        callback_id: dispatched.ticket.callback_id,
        seed: [0x97; 32],
    };
    let fenced = driver
        .complete(late_complete, IdempotencyKey::from_bytes([0x98; 16]))
        .await
        .expect("late completion canonicalized for reconciliation");
    assert!(!fenced.replayed);
    assert!(
        fenced.fenced_reconciliation_only,
        "completion from pre-cancel epoch must be fenced to reconciliation-only"
    );
    assert_eq!(
        outbox_kinds_for(&ws.open_store(), handle),
        vec![OutboxKind::ReconcileEffect]
    );

    // Exact replay across restart returns the same fenced canonical outcome; conflicting seed fails closed.
    let fenced_replay = driver
        .complete(late_complete, IdempotencyKey::from_bytes([0x99; 16]))
        .await
        .expect("fenced replay");
    assert!(fenced_replay.replayed);
    assert!(fenced_replay.fenced_reconciliation_only);
    assert_eq!(
        outbox_kinds_for(&ws.open_store(), handle),
        vec![OutboxKind::ReconcileEffect]
    );

    let conflict_err = driver
        .complete(
            CompleteProviderOperation {
                seed: [0xFF; 32],
                ..late_complete
            },
            IdempotencyKey::from_bytes([0x9A; 16]),
        )
        .await
        .expect_err("conflicting seed on terminal callback must fail closed");
    assert!(matches!(
        conflict_err,
        OutOfProcessProviderError::RemoteRejected {
            code: SabiErrorCode::Conflict,
            retry: RetryDirective::DoNotRetry,
            ..
        }
    ));
}

fn assert_invalid_argument(err: &OutOfProcessProviderError) {
    assert!(matches!(
        err,
        OutOfProcessProviderError::RemoteRejected {
            code: SabiErrorCode::InvalidArgument,
            retry: RetryDirective::DoNotRetry,
            ..
        }
    ));
}

#[tokio::test]
async fn malformed_oversized_and_wrong_schema_payloads_fail_closed_without_crashing_worker() {
    let ws = TestWorkspace::new("wire-neg");
    let principals = setup_authorities(&ws, &[0xA5]);
    let driver = OutOfProcessProviderDriver::spawn(
        worker_config(
            &ws,
            ProviderIsolationProfile::Verified,
            None,
            Some(principals[0].id),
        ),
        credentials_for(principals[0].id, &principals[0].signing),
        client_transport_config(),
    )
    .expect("spawn worker");
    let initial_pid = driver.worker_pid().expect("worker pid");

    // 1. Truncated/malformed protobuf wire bytes.
    let malformed = driver
        .exchange_raw_payload(
            REGISTER_PROVIDER_METHOD,
            MethodSemantics::MUTATION,
            vec![0xFF, 0x00, 0xAB, 0xCD, 0x80],
            Some(IdempotencyKey::from_bytes([0xB1; 16])),
        )
        .await
        .expect_err("malformed protobuf must fail closed");
    assert_invalid_argument(&malformed);

    // 2. Oversized DriverProvider payload (> 16 KiB bound, within 64 KiB IPC frame).
    let oversized = driver
        .exchange_raw_payload(
            REGISTER_PROVIDER_METHOD,
            MethodSemantics::MUTATION,
            vec![0x08; MAX_DRIVER_PROVIDER_PAYLOAD_BYTES + 1],
            Some(IdempotencyKey::from_bytes([0xB2; 16])),
        )
        .await
        .expect_err("oversized DriverProvider payload must fail closed");
    assert_invalid_argument(&oversized);

    // 3. Valid SABI protobuf from another frozen channel (`nlos.sabi.SystemControl`).
    let wrong_schema_bytes = encode_get_system_control_request(&GetSystemControlRequest {
        schema: Some(system_control_schema_identity()),
        view: SystemControlView::ArtifactCommitRecovery.into(),
        alert_limit: 8,
        target_id: Vec::new(),
        plan_id: Vec::new(),
        target_generation: 0,
    })
    .expect("encode SystemControl request");
    let wrong_schema = driver
        .exchange_raw_payload(
            REGISTER_PROVIDER_METHOD,
            MethodSemantics::MUTATION,
            wrong_schema_bytes,
            Some(IdempotencyKey::from_bytes([0xB3; 16])),
        )
        .await
        .expect_err("wrong-schema payload must fail closed");
    assert_invalid_argument(&wrong_schema);

    // 4. Valid DriverProvider protobuf arm paired with mismatched SABI method (`query_isolation` on `cancel`).
    let mismatched_arm = driver
        .exchange_raw_payload(
            CANCEL_PROVIDER_METHOD,
            MethodSemantics::MUTATION,
            encode_query_isolation_request_proto().expect("encode query_isolation"),
            Some(IdempotencyKey::from_bytes([0xB4; 16])),
        )
        .await
        .expect_err("method/body mismatch must fail closed");
    assert_invalid_argument(&mismatched_arm);

    // Worker process stayed alive with zero durable mutations and immediately serves valid calls.
    assert_eq!(driver.worker_pid(), Some(initial_pid));
    let spec = sample_spec(0xB5);
    assert!(matches!(
        ws.open_store().inspect(handle_of(spec)),
        Err(StoreError::Operation(OperationError::InvalidGeneration))
    ));
    let ok = driver
        .register(spec, IdempotencyKey::from_bytes([0xB6; 16]))
        .await
        .expect("worker still serves valid requests");
    assert!(!ok.replayed);
}
