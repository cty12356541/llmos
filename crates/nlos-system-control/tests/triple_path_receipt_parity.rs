#![cfg(all(unix, feature = "cli"))]
#![allow(deprecated)] // Ladder constructors deprecated in favor of the *_with_authorities_struct entries.
#![allow(clippy::too_many_lines)]
//! W32-C (B5-6) triple-path Receipt parity gate: for every
//! `ControlCommand` family in SABI v1.4, the same seeded authority state answers the SAME
//! command with byte-identical [`ControlReceipt`]s on four dispatch
//! mechanisms:
//!
//! (a) **direct** — in-process construction dispatched through
//!     [`dispatch_in_process`] (the reference projection);
//! (b) **NL** — restricted-grammar utterances compiled by
//!     [`parse_nl_command`] into the *same* command, dispatched over the
//!     plain local-IPC entry;
//! (c) **CLI** — the real `system-control-cli` binary as a subprocess over
//!     the plain entry (first stdout line `RECEIPT <hex>`);
//! (d) **GUI** — the dispatch core of the desktop backend command layer
//!     (W32-B `desktop/src-tauri/src/ipc.rs::dispatch_control`), invoked
//!     headless here as [`dispatch_over_authenticated_socket`] over the
//!     ADR-0011 challenge-response authenticated entry — the GUI's only
//!     wiring. The desktop wrapper is a thin typed shell (principal/key
//!     parsing + Ed25519 signing closure + DTO projection whose
//!     `receipt_hex` is exactly `receipt_to_hex`); its byte-transparency is
//!     additionally pinned live by the desktop-side W32-A/B integration
//!     tests, which drive the actual `dispatch_control`/`submit_control`
//!     functions against the authenticated and plain entries. No desktop
//!     backend change was needed for this suite: the headless hook already
//!     exists as the public `dispatch_control` core.
//!
//! One dual-entry fixture (authenticated + plain listeners over one seeded
//! `SqliteTaskAuthority`, one stub health source, one capability policy,
//! one deterministic operation executor) serves all paths of a test, so a
//! byte difference can only come from the dispatch path itself. The matrix
//! covers the success reads, the typed `NotFound` shape (unwired
//! process/resource inspectors, missing plan), the typed `Rights` denial
//! shape, and the mutating recovery/operation families (idempotent
//! acknowledgements replay the same bytes; consuming resumes re-arm the
//! durable ledger row between dispatches, mirroring
//! `control_command_cli.rs`).
//!
//! Known SABI v1.4 boundary, pinned here instead of papered over: the NL
//! grammar has no semantic-domain forms, so semantic commands run the
//! direct/CLI/GUI legs while representative semantic utterances are
//! asserted to stay typed `InvalidCommand` rejections before any dispatch.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ed25519_dalek::{Signer, SigningKey};
use nlos_clock::{AuthorityClock, WallSource};
use nlos_identity::{
    BootstrapDecision, BootstrapPrincipalRequest, IdentityAuthority, IdentityBinding, KeyPurpose,
};
use nlos_ipc::handshake::transport::ServerHandshakeContext;
use nlos_ipc::unix::UnixListenerAdapter;
use nlos_ipc::{OutboundResponse, PeerAuthorizer, PeerIdentity, TransportConfig, serve_one};
use nlos_schema::sabi::v1::{
    ControlCommand as WireControlCommand, ExchangeResponse, GetSystemControlRequest, SabiErrorCode,
    SabiFailure, SabiRequestContext,
};
use nlos_system_control::auth::{
    authenticated_serve_one_control, dispatch_over_authenticated_socket,
};
use nlos_system_control::control::{ControlCommand, ControlOutcome, dispatch_in_process};
use nlos_system_control::nl::{
    NL_ACK_REASON, NL_CANCEL_REASON, NL_KILL_REASON, NL_PAUSE_REASON, NL_RECLAIM_REASON,
    NL_RESOURCE_ACK_REASON, NL_RESOURCE_RESUME_REASON, NL_RESUME_REASON, NL_THROTTLE_REASON,
    parse_nl_command,
};
use nlos_system_control::{
    OperationCommandExecutor, OperationControlRequest, RecoveryHealthSource, RecoverySystemControl,
    SystemControlAuthorizer,
};
use nlos_task::{
    ArtifactCommitPlanId, ArtifactPublicationExpectation, ArtifactRecoveryFailureRequest,
    ArtifactRecoveryFailureSource, AttemptSpec, PermitDecision, PermitRequest,
    PlanArtifactCommitRequest, SnapshotBundle, SqliteTaskAuthority, artifact_publication_plan_root,
    empty_effect_history_root,
};
use nlos_types::{
    ArtifactId, CancellationScopeId, Generation, IdempotencyKey, PrincipalId, ReceiptId,
    TaskAttemptId, TaskId, TaskSnapshotId,
};

/// Fixed service-side clocks: the authenticated entry reads its durable
/// wall from the fixture `AuthorityClock` (`FixedWall`), and the plain and
/// in-process legs pass the same constants, so every receipt timestamp is
/// byte-comparable across paths.
const MONOTONIC_NOW_NS: u64 = 10;
const CLOCK_WALL_MS: u64 = 42_000;

const SEMANTIC_PLAN_ID: [u8; 16] = [0x71; 16];
const SEMANTIC_ACK_COMMAND_ID: [u8; 16] = [0x53; 16];
const SEMANTIC_RESUME_COMMAND_ID: [u8; 16] = [0x54; 16];
const SEMANTIC_TOTAL_FAILURES: u64 = 8;
const SEMANTIC_REASON: &str = "inspected semantic recovery evidence";

const RESOURCE_PLAN_ID: [u8; 16] = [0x81; 16];
const RESOURCE_TOTAL_FAILURES: u64 = 8;

const PROCESS_ID: [u8; 16] = [0x77; 16];
const RESERVATION_ID: [u8; 16] = [0x88; 16];
const MISSING_PLAN_ID: [u8; 16] = [0xEE; 16];

const OPERATION_TARGET_ID: [u8; 16] = [0x91; 16];
const OPERATION_CAS: u64 = 4;
const THROTTLE_PERCENT: u64 = 50;

const DENIED_COMMAND_ID: [u8; 16] = [0x7E; 16];
const DENIED_REASON: &str = "denied: four-path typed Rights parity probe";

const WALL_NOW_MS: i64 = CLOCK_WALL_MS.cast_signed();

fn hex(bytes: &[u8]) -> String {
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(output, "{byte:02x}");
    }
    output
}

// ---------------------------------------------------------------------------
// Fixture: one seeded authority serving two entries (authenticated + plain).
// ---------------------------------------------------------------------------

struct TempRoot(PathBuf);

static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

impl TempRoot {
    fn new(label: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Self(std::env::temp_dir().join(format!(
            "nlos-sc-parity-{label}-{}-{}-{nanos}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed),
        )))
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Short socket path: macOS `SUN_LEN` caps socket paths at 104 bytes.
struct SocketPath(PathBuf);

static NEXT_SOCKET: AtomicU64 = AtomicU64::new(0);

impl SocketPath {
    fn new(label: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        // Compact prefix: with a long per-user TMPDIR (`/var/folders/…/T/`)
        // the path must still fit macOS `SUN_LEN` (104 bytes) for every
        // label used below.
        Self(std::env::temp_dir().join(format!(
            "nlos-sc-3p-{label}-{}-{}-{nanos}",
            std::process::id(),
            NEXT_SOCKET.fetch_add(1, Ordering::Relaxed),
        )))
    }
}

impl std::ops::Deref for SocketPath {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AsRef<Path> for SocketPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Drop for SocketPath {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Deterministic durable wall source: the clock facility is real; only the
/// wall reading is pinned so every entry judges and stamps the same
/// instant.
struct FixedWall(u64);

impl WallSource for FixedWall {
    fn now_ms(&self) -> Result<u64, nlos_clock::AuthorityClockError> {
        Ok(self.0)
    }
}

/// Test policy (same shape as `control_ipc_auth.rs`): the control
/// capability handle authorizes; submit reasons prefixed `denied` exercise
/// the typed `Rights` rejection.
struct CapabilityPolicy;

impl SystemControlAuthorizer for CapabilityPolicy {
    fn authorize_get(
        &self,
        context: &SabiRequestContext,
        _: &GetSystemControlRequest,
    ) -> Result<(), &'static str> {
        authorize(context)
    }

    fn authorize_submit(
        &self,
        context: &SabiRequestContext,
        command: &WireControlCommand,
    ) -> Result<(), &'static str> {
        authorize(context)?;
        if command.reason.starts_with("denied") {
            Err("policy denied this command")
        } else {
            Ok(())
        }
    }
}

fn authorize(context: &SabiRequestContext) -> Result<(), &'static str> {
    let expected = nlos_schema::sabi::v1::CapabilityHandle {
        slot: nlos_system_control::control::CONTROL_CAPABILITY_SLOT,
        generation: nlos_system_control::control::CONTROL_CAPABILITY_GENERATION,
    };
    if context.capability_handles.as_slice() == [expected] {
        Ok(())
    } else {
        Err("missing recovery operations capability")
    }
}

struct AllowPeer;

impl PeerAuthorizer for AllowPeer {
    fn authorize(&self, _: &PeerIdentity) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone)]
struct StubHealth(nlos_commit_coordinator::RecoveryWorkerHealth);

impl RecoveryHealthSource for StubHealth {
    fn recovery_health(&self) -> nlos_commit_coordinator::RecoveryWorkerHealth {
        self.0.clone()
    }
}

fn stub_health(plan_id: &ArtifactCommitPlanId) -> StubHealth {
    StubHealth(nlos_commit_coordinator::RecoveryWorkerHealth {
        state: nlos_commit_coordinator::RecoveryWorkerState::BackingOff,
        completed_cycles: 4,
        total_inspected: 3,
        total_finalized: 2,
        consecutive_failed_cycles: 0,
        retry_delay: Some(Duration::from_millis(250)),
        last_failures: vec![nlos_commit_coordinator::RecoveryWorkerFailure {
            plan_id: Some(*plan_id),
            authority: nlos_commit_coordinator::RecoveryFailureAuthority::Artifact,
            message: "secret local database path must not cross IPC".to_owned(),
        }],
        durable_retrying: 0,
        durable_escalated: 1,
        durable_unacknowledged_escalated: 1,
        durable_resolved: 0,
        semantic_durable_retrying: 0,
        semantic_durable_escalated: 0,
        semantic_durable_unacknowledged_escalated: 0,
        semantic_durable_resolved: 0,
        semantic_consecutive_failed_cycles: 0,
        semantic_total_inspected: 0,
        semantic_total_finalized: 0,
        semantic_domain_faulted: false,
        artifact_domain_faulted: false,
        resource_durable_retrying: 0,
        resource_durable_escalated: 0,
        resource_durable_unacknowledged_escalated: 0,
        resource_durable_resolved: 0,
        resource_consecutive_failed_cycles: 0,
        resource_total_inspected: 0,
        resource_total_finalized: 0,
        resource_domain_faulted: false,
    })
}

/// Deterministic stub executor (same shape as `control_command_cli.rs`):
/// each arm's receipt id names the executed arm in its first byte, so the
/// operation-family receipts are success-shaped and byte-comparable.
struct DeterministicOperationExecutor;

fn operation_receipt(arm_tag: u8) -> [u8; 16] {
    let mut id = OPERATION_TARGET_ID;
    id[0] = arm_tag;
    id
}

impl OperationCommandExecutor for DeterministicOperationExecutor {
    fn pause_operation(&self, _: OperationControlRequest) -> Result<ReceiptId, SabiFailure> {
        Ok(ReceiptId::from_bytes(operation_receipt(1)))
    }

    fn resume_operation(&self, _: OperationControlRequest) -> Result<ReceiptId, SabiFailure> {
        Ok(ReceiptId::from_bytes(operation_receipt(2)))
    }

    fn cancel_operation(&self, _: OperationControlRequest) -> Result<ReceiptId, SabiFailure> {
        Ok(ReceiptId::from_bytes(operation_receipt(3)))
    }

    fn kill_operation(&self, _: OperationControlRequest) -> Result<ReceiptId, SabiFailure> {
        Ok(ReceiptId::from_bytes(operation_receipt(4)))
    }

    fn throttle_operation(
        &self,
        _: OperationControlRequest,
        _: u64,
    ) -> Result<ReceiptId, SabiFailure> {
        Ok(ReceiptId::from_bytes(operation_receipt(5)))
    }

    fn reclaim_operation(&self, _: OperationControlRequest) -> Result<ReceiptId, SabiFailure> {
        Ok(ReceiptId::from_bytes(operation_receipt(6)))
    }
}

/// Seeds one escalated semantic ledger row directly (the `Escalated`
/// transition is W26-tested inside `nlos-task`; the per-connection foreign
/// key is left unchecked by the raw seeding connection — same fixture note
/// as `control_command_cli.rs`).
fn seed_escalated_semantic_recovery(path: &Path) {
    let raw = rusqlite::Connection::open(path).unwrap();
    raw.pragma_update(None, "foreign_keys", "OFF").unwrap();
    raw.execute(
        "INSERT INTO task_semantic_recovery (
            plan_id, recovery_state, consecutive_failures, total_failures,
            last_failure_source, first_failed_at_ms, last_failed_at_ms,
            next_retry_at_ms, escalated_at_ms, resolved_at_ms, updated_at_ms
        ) VALUES (?1, 1, ?2, ?3, 1, 1000, 1400, NULL, 1500, NULL, 1500)",
        rusqlite::params![
            SEMANTIC_PLAN_ID.as_slice(),
            SEMANTIC_TOTAL_FAILURES.to_be_bytes().as_slice(),
            SEMANTIC_TOTAL_FAILURES.to_be_bytes().as_slice(),
        ],
    )
    .unwrap();
}

/// Returns the semantic ledger row to its seeded `Escalated` shape between
/// dispatches of the same resume command (one resume consumes the state).
fn reset_escalated_semantic_recovery(path: &Path) {
    let raw = rusqlite::Connection::open(path).unwrap();
    raw.execute(
        "UPDATE task_semantic_recovery
         SET recovery_state = 1, consecutive_failures = ?2, next_retry_at_ms = NULL,
             escalated_at_ms = 1500, resolved_at_ms = NULL, updated_at_ms = 1500
         WHERE plan_id = ?1",
        rusqlite::params![
            SEMANTIC_PLAN_ID.as_slice(),
            SEMANTIC_TOTAL_FAILURES.to_be_bytes().as_slice(),
        ],
    )
    .unwrap();
}

/// Seeds one escalated `task_resource_recovery` ledger row directly (same
/// fixture note as the semantic mirror; W28-C tests the transition inside
/// `nlos-task`).
fn seed_escalated_resource_recovery(path: &Path) {
    let raw = rusqlite::Connection::open(path).unwrap();
    raw.pragma_update(None, "foreign_keys", "OFF").unwrap();
    raw.execute(
        "INSERT INTO task_resource_recovery (
            plan_id, recovery_state, consecutive_failures, total_failures,
            last_failure_source, first_failed_at_ms, last_failed_at_ms,
            next_retry_at_ms, escalated_at_ms, resolved_at_ms, updated_at_ms
        ) VALUES (?1, 1, ?2, ?3, 1, 1000, 1400, NULL, 1500, NULL, 1500)",
        rusqlite::params![
            RESOURCE_PLAN_ID.as_slice(),
            RESOURCE_TOTAL_FAILURES.to_be_bytes().as_slice(),
            RESOURCE_TOTAL_FAILURES.to_be_bytes().as_slice(),
        ],
    )
    .unwrap();
}

/// Resource mirror of [`reset_escalated_semantic_recovery`].
fn reset_escalated_resource_recovery(path: &Path) {
    let raw = rusqlite::Connection::open(path).unwrap();
    raw.execute(
        "UPDATE task_resource_recovery
         SET recovery_state = 1, consecutive_failures = ?2, next_retry_at_ms = NULL,
             escalated_at_ms = 1500, resolved_at_ms = NULL, updated_at_ms = 1500
         WHERE plan_id = ?1",
        rusqlite::params![
            RESOURCE_PLAN_ID.as_slice(),
            RESOURCE_TOTAL_FAILURES.to_be_bytes().as_slice(),
        ],
    )
    .unwrap();
}

/// Bootstraps one real principal with a genuine Ed25519 keypair (same shape
/// as `control_ipc_auth.rs`); the validity window covers the fixed clock
/// wall reading the server judges at.
fn bootstrap(root: &Path, seed: u8) -> (IdentityAuthority, SigningKey, IdentityBinding) {
    let identity = IdentityAuthority::open(root).unwrap();
    let key = SigningKey::from_bytes(&[seed; 32]);
    let BootstrapDecision::Created(binding) = identity
        .bootstrap_principal(BootstrapPrincipalRequest {
            principal_profile_digest: [seed.wrapping_add(1); 32],
            control_domain_policy_digest: [seed.wrapping_add(2); 32],
            public_key: key.verifying_key().to_bytes(),
            key_purpose: KeyPurpose::SemanticSigning,
            key_valid_from_ms: 0,
            key_valid_until_ms: 1_000_000,
            idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(3); 16]),
            created_at_ms: 0,
        })
        .unwrap()
    else {
        unreachable!("fresh authority bootstraps a new principal");
    };
    (identity, key, binding)
}

/// Assembles one escalated artifact recovery plan through the real
/// authority call sequence (same shape as `control_ipc_auth.rs`).
fn create_escalated_plan(authority: &SqliteTaskAuthority) -> ArtifactCommitPlanId {
    let task_id = TaskId::from_bytes([0x11; 16]);
    authority
        .register_task(nlos_task::TaskSpec {
            task_id,
            task_generation: Generation::INITIAL,
            registered_at_ms: 1_000,
            application_id: None,
            plan_revision: None,
        })
        .unwrap();
    let attempt = AttemptSpec {
        task_id,
        attempt_id: TaskAttemptId::from_bytes([0x12; 16]),
        attempt_generation: Generation::INITIAL,
        snapshot: SnapshotBundle {
            snapshot_id: TaskSnapshotId::from_bytes([0x13; 16]),
            snapshot_digest: [0x14; 32],
            expected_head_commit_seq: 0,
            effect_history_root: empty_effect_history_root(),
            retry_fence_epoch: 0,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes([0x15; 16]),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes([0x16; 16]),
        registered_at_ms: 2_000,
    };
    authority.register_attempt(attempt).unwrap();
    let expectation = ArtifactPublicationExpectation {
        staging_id: [0x21; 16],
        artifact_id: ArtifactId::from_bytes([0x22; 16]),
        target_revision: 1,
        digest: [0x23; 32],
        size_bytes: 10,
    };
    let PermitDecision::Issued(permit) = authority
        .request_commit_permit(PermitRequest {
            task_id,
            attempt_id: attempt.attempt_id,
            attempt_generation: attempt.attempt_generation,
            write_set_root: artifact_publication_plan_root(std::slice::from_ref(&expectation))
                .unwrap(),
            planned_effects: Vec::new(),
            idempotency_key: IdempotencyKey::from_bytes([0x17; 16]),
            valid_until_ms: 20_000,
            requested_at_ms: 3_000,
        })
        .unwrap()
    else {
        panic!("expected permit");
    };
    let plan = authority
        .plan_artifact_commit(PlanArtifactCommitRequest {
            task_id,
            attempt_id: attempt.attempt_id,
            attempt_generation: attempt.attempt_generation,
            permit_id: permit.permit_id,
            idempotency_key: IdempotencyKey::from_bytes([0x18; 16]),
            expectations: vec![expectation],
            planned_at_ms: 4_000,
        })
        .unwrap()
        .record()
        .clone();
    authority
        .record_artifact_recovery_failure(ArtifactRecoveryFailureRequest {
            plan_id: plan.plan_id,
            expected_total_failures: 0,
            source: ArtifactRecoveryFailureSource::ArtifactAuthority,
            observed_at_ms: 5_000,
            base_delay_ms: 100,
            max_delay_ms: 1_000,
            escalation_threshold: 1,
        })
        .unwrap();
    plan.plan_id
}

/// One dual-entry parity fixture: a single seeded `SqliteTaskAuthority`
/// (+ stub health, capability policy, deterministic operation executor)
/// served through an ADR-0011 authenticated listener and a plain listener.
struct ParityFixture {
    #[allow(dead_code)] // keepalive: TempRoot only borrows its Drop.
    root: TempRoot,
    database_path: PathBuf,
    socket_authenticated: SocketPath,
    socket_plain: SocketPath,
    tasks: Arc<SqliteTaskAuthority>,
    identity: Arc<IdentityAuthority>,
    clock: Arc<AuthorityClock>,
    handshake: Arc<ServerHandshakeContext>,
    health: StubHealth,
    plan_id: ArtifactCommitPlanId,
}

impl ParityFixture {
    fn spawn(label: &str, seed: u8) -> (Self, SigningKey, PrincipalId) {
        let root = TempRoot::new(label);
        let socket_authenticated = SocketPath::new(&format!("{label}a"));
        let socket_plain = SocketPath::new(&format!("{label}p"));
        let (identity, key, binding) = bootstrap(&root.0.join("identity"), seed);
        let database_path = root.0.join("tasks.sqlite3");
        let tasks = Arc::new(SqliteTaskAuthority::open(&database_path).unwrap());
        let plan_id = create_escalated_plan(tasks.as_ref());
        seed_escalated_semantic_recovery(&database_path);
        seed_escalated_resource_recovery(&database_path);
        let clock = Arc::new(
            AuthorityClock::open_with_wall_source(root.0.join("clock"), FixedWall(CLOCK_WALL_MS))
                .unwrap(),
        );
        let handshake = Arc::new(ServerHandshakeContext::new(&socket_authenticated, 64).unwrap());
        (
            Self {
                root,
                database_path,
                socket_authenticated,
                socket_plain,
                tasks,
                identity: Arc::new(identity),
                clock,
                handshake,
                health: stub_health(&plan_id),
                plan_id,
            },
            key,
            binding.principal_id,
        )
    }

    /// Serves both entries until the test runtime retires them. Idle
    /// accept windows and failed handshakes are normal rounds (the
    /// transport bounds each accept by the connect timeout).
    fn serve(&self) {
        // Both loops are detached (JoinHandles dropped): they serve until
        // the runtime shuts down, and the socket paths clean up via Drop.
        let _authenticated = {
            let listener = UnixListenerAdapter::bind(&self.socket_authenticated).unwrap();
            let tasks = Arc::clone(&self.tasks);
            let identity = Arc::clone(&self.identity);
            let clock = Arc::clone(&self.clock);
            let handshake = Arc::clone(&self.handshake);
            let health = self.health.clone();
            tokio::spawn(async move {
                let nonce_counter = Arc::new(AtomicU64::new(0));
                loop {
                    let control =
                        RecoverySystemControl::new(tasks.as_ref(), &health, &CapabilityPolicy)
                            .with_operation_executor(&DeterministicOperationExecutor);
                    let nonce_value = nonce_counter.fetch_add(1, Ordering::Relaxed);
                    let mut nonce = [0u8; 32];
                    nonce[..8].copy_from_slice(&nonce_value.to_be_bytes());
                    if authenticated_serve_one_control(
                        &listener,
                        TransportConfig::default(),
                        &control,
                        identity.as_ref(),
                        clock.as_ref(),
                        handshake.as_ref(),
                        &AllowPeer,
                        MONOTONIC_NOW_NS,
                        move || nonce,
                    )
                    .await
                    .is_err()
                    {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                }
            })
        };
        let _plain = {
            let listener = UnixListenerAdapter::bind(&self.socket_plain).unwrap();
            let tasks = Arc::clone(&self.tasks);
            let health = self.health.clone();
            tokio::spawn(async move {
                loop {
                    let (stream, peer) = match listener.accept(TransportConfig::default()).await {
                        Ok(connection) => connection,
                        Err(nlos_ipc::IpcError::Timeout(nlos_ipc::IoOperation::Accept)) => {
                            continue;
                        }
                        Err(_) => break,
                    };
                    let tasks = Arc::clone(&tasks);
                    let health = health.clone();
                    // One misbehaving exchange never takes the endpoint down.
                    let _ = serve_one(
                        stream,
                        TransportConfig::default(),
                        peer,
                        &AllowPeer,
                        move |validated| {
                            let response = RecoverySystemControl::new(
                                tasks.as_ref(),
                                &health,
                                &CapabilityPolicy,
                            )
                            .with_operation_executor(&DeterministicOperationExecutor)
                            .handle_for_ipc(
                                validated.envelope(),
                                MONOTONIC_NOW_NS,
                                WALL_NOW_MS,
                            );
                            async move {
                                Ok(OutboundResponse::Typed(ExchangeResponse {
                                    envelope: Some(response),
                                }))
                            }
                        },
                    )
                    .await;
                }
            })
        };
    }

    /// In-process reference control over the same authority/health/policy
    /// and the same deterministic operation executor as both entries.
    fn reference_control(&self) -> RecoverySystemControl<'_, StubHealth, CapabilityPolicy> {
        RecoverySystemControl::new(self.tasks.as_ref(), &self.health, &CapabilityPolicy)
            .with_operation_executor(&DeterministicOperationExecutor)
    }

    /// One four-path reference dispatch (used for shape assertions).
    fn dispatch_reference(
        &self,
        command: &ControlCommand,
    ) -> nlos_system_control::control::ControlReceipt {
        let control = self.reference_control();
        dispatch_in_process(&control, command, MONOTONIC_NOW_NS, WALL_NOW_MS, None, None).unwrap()
    }

    fn rearm_semantic(&self) {
        reset_escalated_semantic_recovery(&self.database_path);
    }

    fn rearm_resource(&self) {
        reset_escalated_resource_recovery(&self.database_path);
    }
}

// ---------------------------------------------------------------------------
// Four-path helpers.
// ---------------------------------------------------------------------------

fn run_cli(socket: &Path, arguments: &[&str]) -> std::process::Output {
    // Cargo names this env var after the exact bin name (dashes kept).
    let binary = std::env::var("CARGO_BIN_EXE_system-control-cli")
        .expect("system-control-cli binary missing; run tests with default features");
    std::process::Command::new(binary)
        .arg(socket.to_str().unwrap())
        .args(arguments)
        .output()
        .unwrap()
}

fn cli_receipt_bytes(output: &std::process::Output) -> Vec<u8> {
    let stdout = String::from_utf8(output.stdout.clone()).unwrap();
    let line = stdout.lines().next().unwrap();
    let hex = line.strip_prefix("RECEIPT ").unwrap();
    (0..hex.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).unwrap())
        .collect()
}

fn assert_nl_sentences_reject(sentences: &[String]) {
    use nlos_system_control::control::ControlError;

    for sentence in sentences {
        assert!(
            matches!(
                parse_nl_command(sentence),
                Err(ControlError::InvalidCommand(_))
            ),
            "expected typed NL rejection for {sentence:?}"
        );
    }
}

/// Drives one command through the four dispatch mechanisms and asserts the
/// receipts are byte-identical: direct in-process reference, every NL
/// sentence (compiled to the same command) over the plain entry, the real
/// CLI binary over the plain entry, and the desktop backend dispatch core
/// over the authenticated entry. `rearm` (when present) re-arms the durable
/// input before each dispatch of a state-consuming mutation.
#[allow(clippy::too_many_arguments)]
async fn assert_four_path_parity(
    fixture: &ParityFixture,
    key: &SigningKey,
    principal: PrincipalId,
    label: &str,
    command: &ControlCommand,
    nl_sentences: &[String],
    cli_args: &[String],
    rearm: Option<&dyn Fn()>,
) {
    use nlos_system_control::control::dispatch_over_socket;

    if let Some(rearm) = rearm {
        rearm();
    }
    let direct = fixture.dispatch_reference(command);
    let direct_bytes = direct.to_bytes();

    for sentence in nl_sentences {
        let compiled = parse_nl_command(sentence)
            .unwrap_or_else(|error| panic!("{label}: NL sentence {sentence:?}: {error:?}"));
        assert_eq!(&compiled, command, "{label}: NL sentence {sentence:?}");
        if let Some(rearm) = rearm {
            rearm();
        }
        let nl = dispatch_over_socket(&fixture.socket_plain, &compiled, None, None)
            .await
            .unwrap_or_else(|error| panic!("{label}: NL dispatch {sentence:?}: {error:?}"));
        assert_eq!(
            nl.to_bytes(),
            direct_bytes,
            "{label}: NL path bytes diverge for {sentence:?}"
        );
    }

    if let Some(rearm) = rearm {
        rearm();
    }
    let cli_slice: Vec<&str> = cli_args.iter().map(String::as_str).collect();
    let cli = run_cli(&fixture.socket_plain, &cli_slice);
    let expected_exit = i32::from(direct.outcome.as_ref().is_err());
    assert_eq!(
        cli.status.code(),
        Some(expected_exit),
        "{label}: cli {cli_slice:?} stdout={} stderr={}",
        String::from_utf8_lossy(&cli.stdout),
        String::from_utf8_lossy(&cli.stderr),
    );
    assert_eq!(
        cli_receipt_bytes(&cli),
        direct_bytes,
        "{label}: CLI path bytes diverge"
    );

    if let Some(rearm) = rearm {
        rearm();
    }
    let gui = dispatch_over_authenticated_socket(
        &fixture.socket_authenticated,
        principal,
        |digest: &[u8; 32]| Ok(key.sign(digest).to_bytes()),
        command,
        None,
        None,
    )
    .await
    .unwrap_or_else(|error| panic!("{label}: GUI path dispatch: {error:?}"));
    assert_eq!(
        gui.to_bytes(),
        direct_bytes,
        "{label}: GUI path bytes diverge"
    );
}

/// Convenience wrapper for the common string-argument shape.
#[allow(clippy::too_many_arguments)]
async fn parity(
    fixture: &ParityFixture,
    key: &SigningKey,
    principal: PrincipalId,
    label: &str,
    command: &ControlCommand,
    nl_sentences: &[&str],
    cli_args: &[&str],
) {
    let nl: Vec<String> = nl_sentences.iter().map(ToString::to_string).collect();
    let cli: Vec<String> = cli_args.iter().map(ToString::to_string).collect();
    assert_four_path_parity(fixture, key, principal, label, command, &nl, &cli, None).await;
}

// ---------------------------------------------------------------------------
// Family tests.
// ---------------------------------------------------------------------------

/// Aggregate inspect/export reads across the three domains (plus the
/// scoped task read and its typed `NotFound` shape). Semantic-domain forms
/// have no NL grammar in SABI v1.4: those run direct/CLI/GUI and the
/// representative utterances are pinned as typed rejections.
#[tokio::test(flavor = "multi_thread")]
async fn inspect_export_family_receipts_are_byte_identical_across_direct_nl_cli_and_gui_paths() {
    let (fixture, key, principal) = ParityFixture::spawn("read", 0x71);
    fixture.serve();
    let plan_hex = hex(fixture.plan_id.as_bytes());
    let missing_hex = hex(&MISSING_PLAN_ID);

    parity(
        &fixture,
        &key,
        principal,
        "inspect-health",
        &ControlCommand::InspectHealth,
        &["inspect health", "查看 健康"],
        &["inspect-health"],
    )
    .await;
    let health_reference = fixture.dispatch_reference(&ControlCommand::InspectHealth);
    let ControlOutcome::Inspected(inspection) = health_reference.outcome.as_ref().unwrap() else {
        panic!("expected inspection receipt");
    };
    assert_eq!(inspection.durable_escalated, 1);
    assert_eq!(inspection.alerts.len(), 1);

    parity(
        &fixture,
        &key,
        principal,
        "inspect-semantic-health",
        &ControlCommand::InspectSemanticHealth,
        &[],
        &["inspect-semantic-health"],
    )
    .await;
    parity(
        &fixture,
        &key,
        principal,
        "inspect-resource-health",
        &ControlCommand::InspectResourceHealth,
        &["inspect resource recovery", "查看资源恢复"],
        &["inspect-resource-health"],
    )
    .await;
    parity(
        &fixture,
        &key,
        principal,
        "export-metrics",
        &ControlCommand::ExportMetrics,
        &["export metrics", "指标"],
        &["export-metrics"],
    )
    .await;
    parity(
        &fixture,
        &key,
        principal,
        "export-semantic-metrics",
        &ControlCommand::ExportSemanticMetrics,
        &[],
        &["export-semantic-metrics"],
    )
    .await;
    parity(
        &fixture,
        &key,
        principal,
        "export-resource-metrics",
        &ControlCommand::ExportResourceMetrics,
        &["export resource metrics", "导出资源指标"],
        &["export-resource-metrics"],
    )
    .await;

    parity(
        &fixture,
        &key,
        principal,
        "inspect-task",
        &ControlCommand::InspectTask {
            plan_id: *fixture.plan_id.as_bytes(),
        },
        &[
            &format!("inspect task {plan_hex}"),
            &format!("查看任务 {plan_hex}"),
        ],
        &["inspect-task", &plan_hex],
    )
    .await;

    // Typed NotFound shape on all four paths: a missing plan id.
    let missing = ControlCommand::InspectTask {
        plan_id: MISSING_PLAN_ID,
    };
    parity(
        &fixture,
        &key,
        principal,
        "inspect-task-missing",
        &missing,
        &[&format!("inspect task {missing_hex}")],
        &["inspect-task", &missing_hex],
    )
    .await;
    let missing_reference = fixture.dispatch_reference(&missing);
    let Err(failure) = missing_reference.outcome.as_ref() else {
        panic!("expected typed NotFound failure");
    };
    assert_eq!(failure.code, i32::from(SabiErrorCode::NotFound));

    // SABI v1.4 NL boundary: semantic-domain utterances stay typed
    // rejections; they never reach any dispatch path.
    assert_nl_sentences_reject(&[
        "inspect semantic health".to_owned(),
        "export semantic metrics".to_owned(),
        format!("acknowledge semantic alert {plan_hex} expecting 1"),
        format!("resume semantic recovery {plan_hex} expecting 1"),
    ]);
}

/// Scoped process/resource reads with unwired client-side inspectors: the
/// typed `not wired` `NotFound` failure shape is byte-identical across all
/// four paths (the CLI binary and the GUI backend both dispatch with no
/// inspector, so this is the production shape).
#[tokio::test(flavor = "multi_thread")]
async fn scoped_inspect_family_receipts_are_byte_identical_across_direct_nl_cli_and_gui_paths() {
    let (fixture, key, principal) = ParityFixture::spawn("scope", 0x72);
    fixture.serve();

    let process_hex = hex(&PROCESS_ID);
    parity(
        &fixture,
        &key,
        principal,
        "inspect-process",
        &ControlCommand::InspectProcess {
            process_id: PROCESS_ID,
        },
        &[
            &format!("inspect process {process_hex}"),
            &format!("检查进程 {process_hex}"),
        ],
        &["inspect-process", &process_hex],
    )
    .await;

    let reservation_hex = hex(&RESERVATION_ID);
    parity(
        &fixture,
        &key,
        principal,
        "inspect-resource",
        &ControlCommand::InspectResource {
            reservation_id: RESERVATION_ID,
        },
        &[
            &format!("inspect resource {reservation_hex}"),
            &format!("查看资源 {reservation_hex}"),
        ],
        &["inspect-resource", &reservation_hex],
    )
    .await;

    let process_reference = fixture.dispatch_reference(&ControlCommand::InspectProcess {
        process_id: PROCESS_ID,
    });
    let Err(failure) = process_reference.outcome.as_ref() else {
        panic!("expected typed not-wired failure");
    };
    assert_eq!(failure.code, i32::from(SabiErrorCode::NotFound));
}

/// Artifact-domain recovery acknowledgement: the NL form derives the §25.3
/// command identity from the plan id, so all four paths carry the same
/// command and the idempotent replays answer byte-identically. The
/// `denied`-prefixed variant pins the typed Rights shape across direct,
/// CLI, and GUI (the NL grammar fixes its audit reasons and cannot express
/// a denial).
#[tokio::test(flavor = "multi_thread")]
async fn artifact_recovery_family_receipts_are_byte_identical_across_direct_nl_cli_and_gui_paths() {
    use nlos_task::ArtifactRecoveryState;

    let (fixture, key, principal) = ParityFixture::spawn("art", 0x73);
    fixture.serve();
    let plan_bytes: [u8; 16] = *fixture.plan_id.as_bytes();
    let plan_hex = hex(&plan_bytes);

    let acknowledge = ControlCommand::AcknowledgeRecoveryAlert {
        control_command_id: plan_bytes,
        plan_id: plan_bytes,
        expected_total_failures: 1,
        reason: NL_ACK_REASON.to_owned(),
    };
    parity(
        &fixture,
        &key,
        principal,
        "ack-recovery-alert",
        &acknowledge,
        &[
            &format!("acknowledge alert {plan_hex} expecting 1"),
            &format!("确认 告警 {plan_hex} 期望 1"),
        ],
        &[
            "ack-recovery-alert",
            &plan_hex,
            &plan_hex,
            "1",
            NL_ACK_REASON,
        ],
    )
    .await;
    // The mutation was real: the escalated alert is acknowledged.
    assert!(
        fixture
            .tasks
            .list_artifact_recovery_alerts(8)
            .unwrap()
            .first()
            .unwrap()
            .acknowledgement
            .is_some()
    );

    let denied = ControlCommand::AcknowledgeRecoveryAlert {
        control_command_id: DENIED_COMMAND_ID,
        plan_id: plan_bytes,
        expected_total_failures: 1,
        reason: DENIED_REASON.to_owned(),
    };
    parity(
        &fixture,
        &key,
        principal,
        "ack-recovery-alert-denied",
        &denied,
        &[],
        &[
            "ack-recovery-alert",
            &hex(&DENIED_COMMAND_ID),
            &plan_hex,
            "1",
            DENIED_REASON,
        ],
    )
    .await;
    let denied_reference = fixture.dispatch_reference(&denied);
    let Err(denial) = denied_reference.outcome.as_ref() else {
        panic!("expected typed Rights failure");
    };
    assert_eq!(denial.code, i32::from(SabiErrorCode::Rights));
    assert_eq!(denial.safe_message, "SystemControl authorization denied");

    // The denied acknowledgement mutated nothing.
    assert_eq!(
        fixture
            .tasks
            .inspect_artifact_recovery(fixture.plan_id)
            .unwrap()
            .unwrap()
            .state,
        ArtifactRecoveryState::Escalated
    );
}

/// Semantic-domain recovery ack/resume: no NL grammar in SABI v1.4 (pinned
/// in the read-family test), so the byte-equality matrix runs direct, CLI,
/// and GUI; one resume consumes the `Escalated` row, so the ledger is
/// re-armed before every dispatch.
#[tokio::test(flavor = "multi_thread")]
async fn semantic_recovery_family_receipts_are_byte_identical_across_direct_nl_cli_and_gui_paths() {
    use nlos_task::SemanticRecoveryState;

    let (fixture, key, principal) = ParityFixture::spawn("sem", 0x74);
    fixture.serve();
    let semantic_plan = nlos_task::SemanticCommitPlanId::from_bytes(SEMANTIC_PLAN_ID);
    let rearm = || fixture.rearm_semantic();

    let acknowledge = ControlCommand::AcknowledgeSemanticRecoveryAlert {
        control_command_id: SEMANTIC_ACK_COMMAND_ID,
        plan_id: SEMANTIC_PLAN_ID,
        expected_total_failures: SEMANTIC_TOTAL_FAILURES,
        reason: SEMANTIC_REASON.to_owned(),
    };
    parity(
        &fixture,
        &key,
        principal,
        "ack-semantic-recovery-alert",
        &acknowledge,
        &[],
        &[
            "ack-semantic-recovery-alert",
            &hex(&SEMANTIC_ACK_COMMAND_ID),
            &hex(&SEMANTIC_PLAN_ID),
            &SEMANTIC_TOTAL_FAILURES.to_string(),
            SEMANTIC_REASON,
        ],
    )
    .await;
    let ack_reference = fixture.dispatch_reference(&acknowledge);
    let ControlOutcome::Acknowledged { receipt_id } = ack_reference.outcome.as_ref().unwrap()
    else {
        panic!("expected acknowledged receipt");
    };
    assert_eq!(receipt_id.len(), 16);

    let resume = ControlCommand::ResumeSemanticRecovery {
        control_command_id: SEMANTIC_RESUME_COMMAND_ID,
        plan_id: SEMANTIC_PLAN_ID,
        expected_total_failures: SEMANTIC_TOTAL_FAILURES,
        reason: SEMANTIC_REASON.to_owned(),
    };
    assert_four_path_parity(
        &fixture,
        &key,
        principal,
        "resume-semantic-recovery",
        &resume,
        &[],
        &[
            "resume-semantic-recovery".to_owned(),
            hex(&SEMANTIC_RESUME_COMMAND_ID),
            hex(&SEMANTIC_PLAN_ID),
            SEMANTIC_TOTAL_FAILURES.to_string(),
            SEMANTIC_REASON.to_owned(),
        ],
        Some(&rearm),
    )
    .await;
    // The last dispatch (GUI leg) really resumed the ledger row.
    assert_eq!(
        fixture
            .tasks
            .inspect_semantic_recovery(semantic_plan)
            .unwrap()
            .unwrap()
            .state,
        SemanticRecoveryState::Retrying
    );
}

/// Resource-domain recovery ack/resume: the NL forms derive the command
/// identity from the plan id, so all four paths carry the same command;
/// resumes re-arm the ledger row between dispatches.
#[tokio::test(flavor = "multi_thread")]
async fn resource_recovery_family_receipts_are_byte_identical_across_direct_nl_cli_and_gui_paths() {
    use nlos_task::ResourceRecoveryState;

    let (fixture, key, principal) = ParityFixture::spawn("res", 0x75);
    fixture.serve();
    let resource_plan = nlos_task::ResourceCommitPlanId::from_bytes(RESOURCE_PLAN_ID);
    let plan_hex = hex(&RESOURCE_PLAN_ID);
    let rearm = || fixture.rearm_resource();

    let acknowledge = ControlCommand::AcknowledgeResourceRecoveryAlert {
        control_command_id: RESOURCE_PLAN_ID,
        plan_id: RESOURCE_PLAN_ID,
        expected_total_failures: RESOURCE_TOTAL_FAILURES,
        reason: NL_RESOURCE_ACK_REASON.to_owned(),
    };
    parity(
        &fixture,
        &key,
        principal,
        "ack-resource-recovery-alert",
        &acknowledge,
        &[
            &format!("acknowledge resource alert {plan_hex} expecting {RESOURCE_TOTAL_FAILURES}"),
            &format!("确认资源告警 {plan_hex} 期望 {RESOURCE_TOTAL_FAILURES}"),
        ],
        &[
            "ack-resource-recovery-alert",
            &plan_hex,
            &plan_hex,
            &RESOURCE_TOTAL_FAILURES.to_string(),
            NL_RESOURCE_ACK_REASON,
        ],
    )
    .await;
    let ack_reference = fixture.dispatch_reference(&acknowledge);
    let ControlOutcome::Acknowledged { receipt_id } = ack_reference.outcome.as_ref().unwrap()
    else {
        panic!("expected acknowledged receipt");
    };
    assert_eq!(receipt_id.len(), 16);

    assert_four_path_parity(
        &fixture,
        &key,
        principal,
        "resume-resource-recovery",
        &ControlCommand::ResumeResourceRecovery {
            control_command_id: RESOURCE_PLAN_ID,
            plan_id: RESOURCE_PLAN_ID,
            expected_total_failures: RESOURCE_TOTAL_FAILURES,
            reason: NL_RESOURCE_RESUME_REASON.to_owned(),
        },
        &[
            format!("resume resource recovery {plan_hex} expecting {RESOURCE_TOTAL_FAILURES}"),
            format!("恢复 资源恢复 {plan_hex} 期望 {RESOURCE_TOTAL_FAILURES}"),
        ],
        &[
            "resume-resource-recovery".to_owned(),
            plan_hex.clone(),
            plan_hex.clone(),
            RESOURCE_TOTAL_FAILURES.to_string(),
            NL_RESOURCE_RESUME_REASON.to_owned(),
        ],
        Some(&rearm),
    )
    .await;
    // The last dispatch (GUI leg) really resumed the ledger row.
    assert_eq!(
        fixture
            .tasks
            .inspect_resource_recovery(resource_plan)
            .unwrap()
            .unwrap()
            .state,
        ResourceRecoveryState::Retrying
    );
}

/// Operation control family (pause/resume/cancel/kill/throttle/reclaim):
/// the NL forms derive the command identity from the target id, the
/// deterministic executor seam answers every arm, and all four paths agree
/// byte-for-byte. The `denied`-prefixed pause pins the typed Rights shape
/// across direct, CLI, and GUI.
#[tokio::test(flavor = "multi_thread")]
async fn operation_family_receipts_are_byte_identical_across_direct_nl_cli_and_gui_paths() {
    struct Family {
        label: &'static str,
        reason: &'static str,
        command: ControlCommand,
        sentences: [&'static str; 2],
        throttle: bool,
    }

    let (fixture, key, principal) = ParityFixture::spawn("op", 0x76);
    fixture.serve();
    let target_hex = hex(&OPERATION_TARGET_ID);
    let revision = OPERATION_CAS.to_string();

    let families = [
        Family {
            label: "pause-operation",
            reason: NL_PAUSE_REASON,
            command: ControlCommand::PauseOperation {
                control_command_id: OPERATION_TARGET_ID,
                target_id: OPERATION_TARGET_ID,
                expected_generation_or_revision: OPERATION_CAS,
                reason: NL_PAUSE_REASON.to_owned(),
            },
            sentences: [
                "pause operation {target} expecting {revision}",
                "暂停操作 {target} 期望 {revision}",
            ],
            throttle: false,
        },
        Family {
            label: "resume-operation",
            reason: NL_RESUME_REASON,
            command: ControlCommand::ResumeOperation {
                control_command_id: OPERATION_TARGET_ID,
                target_id: OPERATION_TARGET_ID,
                expected_generation_or_revision: OPERATION_CAS,
                reason: NL_RESUME_REASON.to_owned(),
            },
            sentences: [
                "resume operation {target} expecting {revision}",
                "恢复操作 {target} 期望 {revision}",
            ],
            throttle: false,
        },
        Family {
            label: "cancel-operation",
            reason: NL_CANCEL_REASON,
            command: ControlCommand::CancelOperation {
                control_command_id: OPERATION_TARGET_ID,
                target_id: OPERATION_TARGET_ID,
                expected_generation_or_revision: OPERATION_CAS,
                reason: NL_CANCEL_REASON.to_owned(),
            },
            sentences: [
                "cancel operation {target} expecting {revision}",
                "取消操作 {target} 期望 {revision}",
            ],
            throttle: false,
        },
        Family {
            label: "kill-operation",
            reason: NL_KILL_REASON,
            command: ControlCommand::KillOperation {
                control_command_id: OPERATION_TARGET_ID,
                target_id: OPERATION_TARGET_ID,
                expected_generation_or_revision: OPERATION_CAS,
                reason: NL_KILL_REASON.to_owned(),
            },
            sentences: [
                "kill operation {target} expecting {revision}",
                "终止操作 {target} 期望 {revision}",
            ],
            throttle: false,
        },
        Family {
            label: "throttle-operation",
            reason: NL_THROTTLE_REASON,
            command: ControlCommand::ThrottleOperation {
                control_command_id: OPERATION_TARGET_ID,
                target_id: OPERATION_TARGET_ID,
                expected_generation_or_revision: OPERATION_CAS,
                throttle_percent: THROTTLE_PERCENT,
                reason: NL_THROTTLE_REASON.to_owned(),
            },
            sentences: [
                "throttle operation {target} to {percent} percent expecting {revision}",
                "限流操作 {target} 到 {percent} 百分比 期望 {revision}",
            ],
            throttle: true,
        },
        Family {
            label: "reclaim-operation",
            reason: NL_RECLAIM_REASON,
            command: ControlCommand::ReclaimOperation {
                control_command_id: OPERATION_TARGET_ID,
                target_id: OPERATION_TARGET_ID,
                expected_generation_or_revision: OPERATION_CAS,
                reason: NL_RECLAIM_REASON.to_owned(),
            },
            sentences: [
                "reclaim operation {target} expecting {revision}",
                "回收操作 {target} 期望 {revision}",
            ],
            throttle: false,
        },
    ];

    for family in &families {
        let nl_sentences: Vec<String> = family
            .sentences
            .iter()
            .map(|template| {
                template
                    .replace("{target}", &target_hex)
                    .replace("{revision}", &revision)
                    .replace("{percent}", &THROTTLE_PERCENT.to_string())
            })
            .collect();
        let mut cli_args = vec![
            family.label.to_owned(),
            target_hex.clone(),
            target_hex.clone(),
        ];
        if family.throttle {
            cli_args.push(THROTTLE_PERCENT.to_string());
        }
        cli_args.push(revision.clone());
        cli_args.push(family.reason.to_owned());
        assert_four_path_parity(
            &fixture,
            &key,
            principal,
            family.label,
            &family.command,
            &nl_sentences,
            &cli_args,
            None,
        )
        .await;
        let reference = fixture.dispatch_reference(&family.command);
        assert!(
            reference.outcome.as_ref().is_ok(),
            "{}: expected the wired executor to answer with a success outcome",
            family.label
        );
    }

    // Typed Rights shape on direct, CLI, and GUI paths: a denied pause.
    let denied = ControlCommand::PauseOperation {
        control_command_id: DENIED_COMMAND_ID,
        target_id: OPERATION_TARGET_ID,
        expected_generation_or_revision: OPERATION_CAS,
        reason: DENIED_REASON.to_owned(),
    };
    parity(
        &fixture,
        &key,
        principal,
        "pause-operation-denied",
        &denied,
        &[],
        &[
            "pause-operation",
            &hex(&DENIED_COMMAND_ID),
            &target_hex,
            &OPERATION_CAS.to_string(),
            DENIED_REASON,
        ],
    )
    .await;
    let denied_reference = fixture.dispatch_reference(&denied);
    let Err(denial) = denied_reference.outcome.as_ref() else {
        panic!("expected typed Rights failure");
    };
    assert_eq!(denial.code, i32::from(SabiErrorCode::Rights));
}
