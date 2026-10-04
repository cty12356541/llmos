//! Resident daemon assembly tests (`daemon` feature): the real authority
//! assembly (including the worker's semantic half), both endpoint binds, the
//! health face, worker start/stop, one full round-trip per endpoint, and
//! stale-path rebinding. No long-running process is left behind — every
//! test drives the loops directly and stops or aborts them.
//!
//! Platform split: the filesystem-socket and CLI-client round-trip tests are
//! Unix-gated (they assert Unix socket files and the Unix-only plain
//! `dispatch_over_socket` client); the worker-domain convergence test is
//! portable (endpoint paths are derived per OS through `temp_path`); and the
//! Windows lane carries one minimal smoke — spawn the real
//! `system-control-daemon` binary, verify the READY line (both endpoints
//! bound), and cross one plain round trip over the named pipe.

#![cfg(feature = "daemon")]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread::sleep;
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use std::sync::atomic::AtomicBool;

#[cfg(unix)]
use ed25519_dalek::Signer as _;
use nlos_artifact::ArtifactStore;
use nlos_commit_coordinator::RecoveryWorkerState;
use nlos_semantic::SemanticAuthority;
#[cfg(unix)]
use nlos_system_control::auth::dispatch_over_authenticated_socket;
use nlos_system_control::control::ControlCommand;
#[cfg(unix)]
use nlos_system_control::control::dispatch_over_socket;
#[cfg(unix)]
use nlos_system_control::control::{ControlOutcome, RecoveryWorkerLifecycle};
#[cfg(windows)]
use nlos_system_control::control::{ControlReceipt, build_request_envelope};
use nlos_system_control::daemon::{DaemonOptions, assemble};
#[cfg(unix)]
use nlos_system_control::daemon::{serve_authenticated_endpoint, serve_plain_endpoint};
use nlos_task::{
    AttemptSpec, ParticipantRegistryBinding, PermitDecision, PermitRequest,
    PlanSemanticCommitRequest, SemanticCommitPlanState, SnapshotBundle, SnapshotConsistency,
    SqliteTaskAuthority, TaskSnapshotReceiptSpec, TaskSpec, TaskWriteSetRequest,
    TaskWriteSetSemanticAppendRequest, TaskWriteSetSemanticRequiredDurability,
    TaskWriteSetSemanticTarget, empty_effect_history_root,
};
#[cfg(unix)]
use nlos_types::PrincipalId;
use nlos_types::{
    CancellationScopeId, Generation, IdempotencyKey, NamespaceId, ReceiptId, SemanticEventId,
    TaskAttemptId, TaskId, TaskSnapshotId,
};
use rusqlite::Connection;

static NEXT_PATH: AtomicU64 = AtomicU64::new(0);

/// Short-lived temp root (sockets must stay under the macOS `SUN_LEN`
/// bound).
struct TempRoot(PathBuf);

impl TempRoot {
    fn new(label: &str) -> Self {
        let sequence = NEXT_PATH.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!(
            "nlos-daemon-{label}-{}-{sequence}",
            std::process::id(),
        )))
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Endpoint path in the host OS form, per test: a `.sock` file under the
/// temp dir on Unix (short enough for the macOS `SUN_LEN` bound), a
/// machine-local pipe name on Windows.
#[cfg(unix)]
fn temp_path(label: &str, suffix: &str) -> PathBuf {
    let sequence = NEXT_PATH.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "nlos-daemon-{label}-{suffix}-{}-{sequence}.sock",
        std::process::id(),
    ))
}

/// Endpoint path in the host OS form, per test: a `.sock` file under the
/// temp dir on Unix, a machine-local pipe name on Windows.
#[cfg(windows)]
fn temp_path(label: &str, suffix: &str) -> PathBuf {
    let sequence = NEXT_PATH.fetch_add(1, Ordering::Relaxed);
    PathBuf::from(format!(
        r"\\.\pipe\nlos-daemon-{label}-{suffix}-{}-{sequence}",
        std::process::id(),
    ))
}

#[cfg(unix)]
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

#[cfg(unix)]
fn hex16(value: &str) -> [u8; 16] {
    let bytes = value.as_bytes();
    assert_eq!(bytes.len(), 32, "principal hex");
    let mut out = [0u8; 16];
    for (index, byte) in out.iter_mut().enumerate() {
        let hi = (bytes[2 * index] as char).to_digit(16).expect("hex");
        let lo = (bytes[2 * index + 1] as char).to_digit(16).expect("hex");
        *byte = u8::try_from(hi * 16 + lo).expect("byte");
    }
    out
}

#[cfg(unix)]
fn worker_state(lifecycle: RecoveryWorkerLifecycle) -> RecoveryWorkerState {
    match lifecycle {
        RecoveryWorkerLifecycle::Starting => RecoveryWorkerState::Starting,
        RecoveryWorkerLifecycle::Running => RecoveryWorkerState::Running,
        RecoveryWorkerLifecycle::BackingOff => RecoveryWorkerState::BackingOff,
        RecoveryWorkerLifecycle::Faulted => RecoveryWorkerState::Faulted,
        RecoveryWorkerLifecycle::Stopped => RecoveryWorkerState::Stopped,
    }
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn assembly_opens_authorities_binds_sockets_and_starts_the_worker() {
    let root = TempRoot::new("assemble");
    let auth_socket = temp_path("assemble", "auth");
    let plain_socket = temp_path("assemble", "plain");
    let options = DaemonOptions::new(&root.0)
        .with_auth_socket(&auth_socket)
        .with_plain_socket(&plain_socket);
    let (daemon, endpoints) =
        assemble(options, tokio::runtime::Handle::current()).expect("assemble daemon");

    // Both endpoints bound where the options say; the daemon reports the
    // same paths it prints in the READY line.
    assert!(auth_socket.exists(), "authenticated socket bound");
    assert!(plain_socket.exists(), "plain socket bound");
    assert_eq!(daemon.auth_socket_path, auth_socket);
    assert_eq!(daemon.plain_socket_path, plain_socket);
    assert_eq!(daemon.root, root.0);
    assert!(daemon.bootstrapped_principal_hex.is_none());

    // The daemon opened the semantic authority under the state root
    // (the worker's semantic half is wired, not quiescent).
    assert!(
        root.0
            .join("semantic")
            .join("semantic-authority.db")
            .exists(),
        "semantic authority database exists under the root"
    );

    // The health face is queryable and the worker is live.
    assert_ne!(daemon.recovery_health().state, RecoveryWorkerState::Stopped);

    // The in-process dispatch face serves the same handler the sockets
    // serve, with every layer source wired and the executor arms powered
    // (W59-1); the arms with no implementation anywhere stay fail-closed.
    let receipt = daemon
        .dispatch_command(&ControlCommand::InspectHealth, 10, 6_000)
        .expect("dispatch");
    let ControlOutcome::Inspected(inspection) = receipt.outcome.expect("health snapshot") else {
        panic!("expected aggregate health inspection");
    };
    assert_eq!(
        worker_state(inspection.worker_state),
        daemon.recovery_health().state
    );

    let unwired = daemon
        .dispatch_command(
            &ControlCommand::PauseOperation {
                control_command_id: [0x51; 16],
                target_id: [0x52; 16],
                expected_generation_or_revision: 1,
                reason: "pause has no implementation anywhere".to_owned(),
            },
            10,
            6_000,
        )
        .expect("dispatch");
    let failure = unwired.outcome.expect_err("pause arm stays fail-closed");
    assert_eq!(
        failure.safe_message,
        "the pause arm is not wired in the system-control daemon"
    );

    // Worker stop is observable and idempotent.
    daemon.stop_worker();
    assert_eq!(daemon.recovery_health().state, RecoveryWorkerState::Stopped);
    daemon.stop_worker();
    drop(endpoints);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn plain_endpoint_serves_the_shared_handler_path_and_stops_gracefully() {
    let root = TempRoot::new("plain");
    let socket_path = temp_path("plain", "plain");
    let options = DaemonOptions::new(&root.0).with_plain_socket(&socket_path);
    let (daemon, endpoints) =
        assemble(options, tokio::runtime::Handle::current()).expect("assemble daemon");
    let stop = Arc::new(AtomicBool::new(false));
    let server = tokio::spawn(serve_plain_endpoint(
        Arc::clone(&daemon),
        endpoints.listener_plain,
        Arc::clone(&stop),
    ));

    let receipt = dispatch_over_socket(
        &socket_path,
        &ControlCommand::InspectHealth,
        None,
        None,
        None,
    )
    .await
    .expect("plain round-trip");
    assert!(
        receipt.outcome.is_ok(),
        "health read served: {:?}",
        receipt.outcome
    );

    // Graceful stop: the loop observes the flag within one bounded accept
    // window (transport default: 5s) and retires.
    stop.store(true, Ordering::Relaxed);
    server.await.expect("plain loop retires after stop");

    daemon.stop_worker();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn authenticated_endpoint_serves_a_bootstrapped_key_file_client() {
    let root = TempRoot::new("auth");
    let socket_path = temp_path("auth", "auth");

    // The desktop client's key-file format: one 0600 file, 64 hex seed.
    let seed = [0x7A; 32];
    fs::create_dir_all(&root.0).expect("create root");
    let key_file = root.0.join("client.key");
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&key_file)
            .expect("create key file");
        file.write_all(hex(&seed).as_bytes()).expect("write seed");
    }
    let options = DaemonOptions::new(&root.0)
        .with_auth_socket(&socket_path)
        .with_identity_key_file(&key_file);
    let (daemon, endpoints) =
        assemble(options, tokio::runtime::Handle::current()).expect("assemble daemon");
    let principal_hex = daemon
        .bootstrapped_principal_hex
        .clone()
        .expect("bootstrap reported the principal");
    let stop = Arc::new(AtomicBool::new(false));
    let server = tokio::spawn(serve_authenticated_endpoint(
        Arc::clone(&daemon),
        endpoints.listener_authenticated,
        Arc::clone(&stop),
    ));

    let key = ed25519_dalek::SigningKey::from_bytes(&seed);
    let receipt = dispatch_over_authenticated_socket(
        &socket_path,
        PrincipalId::from_bytes(hex16(&principal_hex)),
        |digest| Ok(key.sign(digest).to_bytes()),
        &ControlCommand::InspectHealth,
        None,
        None,
        None,
    )
    .await
    .expect("authenticated round-trip");
    assert!(
        receipt.outcome.is_ok(),
        "health read served: {:?}",
        receipt.outcome
    );

    stop.store(true, Ordering::Relaxed);
    server.await.expect("authenticated loop retires after stop");
    daemon.stop_worker();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stale_socket_paths_are_rebound_by_the_next_assembly() {
    let root = TempRoot::new("rebind");
    let auth_socket = temp_path("rebind", "auth");
    let plain_socket = temp_path("rebind", "plain");
    {
        let options = DaemonOptions::new(&root.0)
            .with_auth_socket(&auth_socket)
            .with_plain_socket(&plain_socket);
        let (_daemon, endpoints) =
            assemble(options, tokio::runtime::Handle::current()).expect("first assembly");
        drop(endpoints);
        // Simulate an unclean exit: the socket files stay behind.
        assert!(auth_socket.exists());
    }
    let options = DaemonOptions::new(&root.0)
        .with_auth_socket(&auth_socket)
        .with_plain_socket(&plain_socket);
    let (_daemon, endpoints) = assemble(options, tokio::runtime::Handle::current())
        .expect("second assembly rebinds over the stale path");
    assert!(auth_socket.exists());
    drop(endpoints);
}

fn wait_until(description: &str, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !predicate() {
        assert!(
            Instant::now() < deadline,
            "condition did not become true: {description}"
        );
        sleep(Duration::from_millis(5));
    }
}

/// Seeds the raw owner prefix one admitted Semantic event needs (the same
/// construction the nlos-commit-coordinator
/// `semantic_pending_restart_scan.rs` harness uses): content object,
/// admitted event, event-log slot, admission receipt, and durability
/// receipt, all inside the daemon-root-shaped `<root>/semantic` directory.
fn seed_semantic_event(root: &Path) -> (SemanticEventId, ReceiptId, ReceiptId) {
    let event_id = SemanticEventId::from_bytes([0x90; 32]);
    let admission_receipt_id = ReceiptId::from_bytes([0xa0; 16]);
    let durability_receipt_id = ReceiptId::from_bytes([0xb0; 16]);
    let target = NamespaceId::from_bytes([0xc0; 16]);
    drop(SemanticAuthority::open(root.join("semantic")).expect("open Semantic authority"));
    let raw = Connection::open(root.join("semantic").join("semantic-authority.db"))
        .expect("open raw Semantic db");
    raw.execute(
        "INSERT INTO content_objects (content_digest, media_type, exact_bytes)
         VALUES (?1, ?2, ?3)",
        rusqlite::params![[0xd0u8; 32].as_slice(), "text/plain", b"semantic"],
    )
    .expect("insert content");
    raw.execute(
        "INSERT INTO semantic_events (
            event_id, canonical_unsigned_event, event_type, scope_kind, scope_id,
            issuer_principal_id, issuer_process_id, issuer_process_generation,
            control_domain_id, issued_at_unix_ns, valid_until_ms, purpose_digest,
            key_id, content_digest
         ) VALUES (?1, ?2, 1, 1, ?3, ?4, ?5, 1, ?6, 1, NULL, NULL, ?7, ?8)",
        rusqlite::params![
            event_id.as_bytes().as_slice(),
            [0xe1u8, 0xe2, 0xe3].as_slice(),
            target.as_bytes().as_slice(),
            [0xe4u8; 16].as_slice(),
            [0xe5u8; 16].as_slice(),
            [0xe6u8; 16].as_slice(),
            [0xe7u8; 16].as_slice(),
            [0xd0u8; 32].as_slice(),
        ],
    )
    .expect("insert event");
    raw.execute(
        "INSERT INTO event_log (event_id) VALUES (?1)",
        [event_id.as_bytes().as_slice()],
    )
    .expect("insert event log");
    raw.execute(
        "INSERT INTO admission_receipts (
            receipt_id, event_id, log_seq, admitted_at_ms, effective_valid_until_ms,
            effective_taint, authz_policy_digest, durability, store_principal_id,
            store_control_domain_id, store_key_id, store_signature
         ) VALUES (?1, ?2, 1, 100, NULL, 0, ?3, 2, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            admission_receipt_id.as_bytes().as_slice(),
            event_id.as_bytes().as_slice(),
            [0xe8u8; 32].as_slice(),
            [0xe9u8; 16].as_slice(),
            [0xeau8; 16].as_slice(),
            [0xebu8; 16].as_slice(),
            [0xecu8; 64].as_slice(),
        ],
    )
    .expect("insert admission");
    raw.execute(
        "INSERT INTO durability_receipts (
            receipt_id, event_id, durable_checkpoint_id, durable_at_ms, store_signature
         ) VALUES (?1, ?2, ?3, 110, ?4)",
        rusqlite::params![
            durability_receipt_id.as_bytes().as_slice(),
            event_id.as_bytes().as_slice(),
            [0xedu8; 32].as_slice(),
            [0xeeu8; 64].as_slice(),
        ],
    )
    .expect("insert durability");
    (event_id, admission_receipt_id, durability_receipt_id)
}

/// Builds one incomplete Semantic commit plan (state `Planned`, no recovery
/// ledger row, hence immediately due) across exactly the stores the daemon
/// will reopen: `<root>/tasks.sqlite3`, `<root>/artifacts`, and
/// `<root>/semantic`. Every handle is dropped so the daemon becomes the
/// sole owner, mirroring a restart between plan and worker scan.
// Keeping the fixture linear mirrors the nlos-commit-coordinator harness it
// was copied from, so the cross-authority steps stay review-adjacent.
#[allow(clippy::too_many_lines)]
#[allow(deprecated)] // ladder constructors deprecated in favor of the struct entries
fn prepare_due_semantic_plan(root: &Path) -> nlos_task::SemanticCommitPlanId {
    let (event_id, admission_receipt_id, durability_receipt_id) = seed_semantic_event(root);
    let tasks = SqliteTaskAuthority::open(root.join("tasks.sqlite3")).expect("open Task authority");
    let artifacts = ArtifactStore::open(root.join("artifacts")).expect("open Artifact store");
    let semantic = SemanticAuthority::open(root.join("semantic")).expect("open Semantic authority");
    let task_id = TaskId::from_bytes([0x10; 16]);
    let attempt_id = TaskAttemptId::from_bytes([0x11; 16]);
    let target = NamespaceId::from_bytes([0xc0; 16]);
    let attempt = AttemptSpec {
        task_id,
        attempt_id,
        attempt_generation: Generation::INITIAL,
        snapshot: SnapshotBundle {
            snapshot_id: TaskSnapshotId::from_bytes([0x12; 16]),
            snapshot_digest: [0x13; 32],
            expected_head_commit_seq: 0,
            effect_history_root: empty_effect_history_root(),
            retry_fence_epoch: 0,
        },
        cancellation_scope_id: CancellationScopeId::from_bytes([0x14; 16]),
        cancellation_generation: Generation::INITIAL,
        idempotency_key: IdempotencyKey::from_bytes([0x15; 16]),
        registered_at_ms: 10,
    };
    tasks
        .register_task(TaskSpec {
            task_id,
            task_generation: Generation::INITIAL,
            registered_at_ms: 1,
            application_id: None,
            plan_revision: None,
        })
        .expect("register task");
    tasks
        .register_snapshot_receipt(TaskSnapshotReceiptSpec {
            task_id,
            snapshot: attempt.snapshot,
            receipt_id: ReceiptId::from_bytes([0x16; 16]),
            builder_id: [0x17; 16],
            builder_version_digest: [0x18; 32],
            per_authority_checkpoint_receipts: vec![ReceiptId::from_bytes([0x19; 16])],
            dependency_closure_root: [0x1a; 32],
            semantic_resolver_digest: [0x1b; 32],
            canonical_iteration_digest: [0x1c; 32],
            achieved_consistency: SnapshotConsistency::Causal,
            built_at_ms: 2,
            authority_id: [0x1d; 16],
            key_id: [0x1e; 16],
            signature: [0x1f; 64],
        })
        .expect("register snapshot receipt");
    tasks
        .register_attempt_with_snapshot_receipt(attempt, ReceiptId::from_bytes([0x16; 16]))
        .expect("register attempt");
    let registry = tasks
        .inspect_participant_registry(task_id)
        .expect("registry");
    tasks
        .register_semantic_admission_participant(
            &semantic,
            task_id,
            ParticipantRegistryBinding {
                generation: registry.generation,
                root: registry.root,
            },
            3,
        )
        .expect("register participant");
    let write_set = tasks
        .seal_task_write_set_with_semantic_authority(
            &artifacts,
            &semantic,
            TaskWriteSetRequest {
                task_id,
                attempt_id,
                attempt_generation: Generation::INITIAL,
                artifact_reads: Vec::new(),
                artifact_writes: Vec::new(),
                process_binding: None,
                semantic_reads: Vec::new(),
                semantic_appends: vec![TaskWriteSetSemanticAppendRequest {
                    event_id,
                    target: TaskWriteSetSemanticTarget::Namespace(target),
                    required_durability: TaskWriteSetSemanticRequiredDurability::Durable,
                    expected_admission_policy_digest: [0xe8; 32],
                    durability_receipt_id: Some(durability_receipt_id),
                }],
                resource_reservations: Vec::new(),
                planned_effects: Vec::new(),
                effect_endpoints: Vec::new(),
                idempotency_key: IdempotencyKey::from_bytes([0x20; 16]),
                sealed_at_ms: 4,
            },
        )
        .expect("seal write set")
        .record()
        .clone();
    assert_eq!(
        write_set.semantic_appends[0].admission_receipt_id,
        admission_receipt_id
    );
    let permit = match tasks
        .request_commit_permit(PermitRequest {
            task_id,
            attempt_id,
            attempt_generation: Generation::INITIAL,
            write_set_root: write_set.write_set_root,
            planned_effects: Vec::new(),
            idempotency_key: IdempotencyKey::from_bytes([0x21; 16]),
            valid_until_ms: i64::MAX,
            requested_at_ms: 5,
        })
        .expect("request permit")
    {
        PermitDecision::Issued(permit) => *permit,
        other => panic!("expected issued permit, got {other:?}"),
    };
    let plan_id = tasks
        .plan_semantic_commit(PlanSemanticCommitRequest {
            task_id,
            attempt_id,
            attempt_generation: Generation::INITIAL,
            permit_id: permit.permit_id,
            idempotency_key: IdempotencyKey::from_bytes([0x22; 16]),
            planned_at_ms: 6,
        })
        .expect("plan semantic commit")
        .record()
        .plan_id;
    assert_eq!(
        tasks
            .inspect_semantic_commit_progress(plan_id)
            .expect("progress")
            .plan
            .state,
        SemanticCommitPlanState::Planned,
        "fixture leaves the plan incomplete and due"
    );
    drop(tasks);
    drop(artifacts);
    drop(semantic);
    plan_id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worker_semantic_domain_converges_a_due_semantic_plan() {
    let root = TempRoot::new("semantic-domain");
    let auth_socket = temp_path("semantic-domain", "auth");
    let plain_socket = temp_path("semantic-domain", "plain");
    let plan_id = prepare_due_semantic_plan(&root.0);
    let (daemon, endpoints) = assemble(
        DaemonOptions::new(&root.0)
            .with_auth_socket(&auth_socket)
            .with_plain_socket(&plain_socket),
        tokio::runtime::Handle::current(),
    )
    .expect("assemble daemon over the pre-seeded root");
    assert!(
        root.0
            .join("semantic")
            .join("semantic-authority.db")
            .exists()
    );

    // The worker's semantic half is no longer quiescent: the first scan
    // picks up the due plan and converges it to the terminal state without
    // any caller-supplied plan data.
    wait_until("semantic plan finalized by the worker", || {
        daemon.recovery_health().semantic_total_finalized >= 1
    });
    let health = daemon.recovery_health();
    assert_eq!(health.state, RecoveryWorkerState::Running);
    assert_eq!(health.semantic_total_inspected, 1);
    assert_eq!(health.semantic_total_finalized, 1);
    assert_eq!(health.semantic_consecutive_failed_cycles, 0);
    assert!(!health.semantic_domain_faulted);
    assert_eq!(health.last_failures, Vec::new());

    // Durable proof across authorities: the plan is terminal in the task
    // authority the daemon reopens, the incomplete scan is empty, and clean
    // convergence opened no semantic recovery ledger row.
    let tasks = SqliteTaskAuthority::open(root.0.join("tasks.sqlite3"))
        .expect("reopen Task authority beside the daemon");
    assert!(matches!(
        tasks.inspect_semantic_commit_progress(plan_id),
        Ok(progress) if progress.plan.state == SemanticCommitPlanState::Finalized
    ));
    assert_eq!(
        tasks
            .list_incomplete_semantic_commit_plans(8)
            .expect("incomplete scan"),
        Vec::new()
    );
    assert!(
        tasks
            .inspect_semantic_recovery(plan_id)
            .expect("semantic ledger read")
            .is_none(),
        "clean convergence never opens a semantic ledger row"
    );

    // The stop path retires the semantic-armed worker like any other.
    daemon.stop_worker();
    assert_eq!(daemon.recovery_health().state, RecoveryWorkerState::Stopped);
    drop(endpoints);
}

/// Minimal Windows smoke: the real `system-control-daemon` binary assembles
/// every authority under a temp root, binds both named-pipe endpoints (the
/// READY line prints only after both binds), and serves one plain round
/// trip over the pipe — the same minimal service face
/// `windows_named_pipe.rs` pins for the handler itself.
#[cfg(windows)]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn windows_daemon_binary_binds_both_endpoints_and_serves_one_plain_round_trip() {
    use std::io::BufRead as _;
    use std::process::{Command, Stdio};

    use nlos_ipc::windows::connect;
    use nlos_ipc::{LocalRpcClient, TransportConfig};
    use nlos_schema::sabi::v1::ExchangeRequest;

    let root = TempRoot::new("smoke");
    fs::create_dir_all(&root.0).expect("create root");
    let auth_pipe = temp_path("smoke", "auth");
    let plain_pipe = temp_path("smoke", "plain");
    let binary = std::env::var("CARGO_BIN_EXE_system-control-daemon")
        .expect("cargo builds the daemon binary beside the tests");
    let mut child = Command::new(binary)
        .arg("--root")
        .arg(&root.0)
        .arg("--auth-socket")
        .arg(&auth_pipe)
        .arg("--plain-socket")
        .arg(&plain_pipe)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn system-control-daemon");

    // The READY line proves the whole assembly path — authorities, worker,
    // and both pipe binds — completed on the other side of the process
    // boundary; the reader thread plus receive timeout keeps a failed
    // daemon from deadlocking the test.
    let stdout = child.stdout.take().expect("piped daemon stdout");
    let ready = {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut line = String::new();
            let mut reader = std::io::BufReader::new(stdout);
            let _ = reader.read_line(&mut line);
            let _ = sender.send(line);
        });
        receiver
            .recv_timeout(Duration::from_secs(30))
            .expect("daemon printed its READY line")
    };
    assert!(ready.starts_with("READY service=system_control"), "{ready}");
    assert!(
        ready.contains(auth_pipe.to_str().unwrap_or_default()),
        "READY names the authenticated pipe: {ready}"
    );
    assert!(
        ready.contains(plain_pipe.to_str().unwrap_or_default()),
        "READY names the plain pipe: {ready}"
    );

    let command = ControlCommand::InspectHealth;
    let request = build_request_envelope(&command).expect("build request envelope");
    let config = TransportConfig::default();
    let (stream, _peer) = connect(&plain_pipe, config)
        .await
        .expect("connect the plain pipe");
    let response = LocalRpcClient::new(stream, config)
        .exchange_validated(ExchangeRequest {
            envelope: Some(request),
        })
        .await
        .expect("one plain exchange over the pipe");
    let receipt = ControlReceipt::compose(&command, response.envelope(), None, None, None)
        .expect("project the response receipt");
    assert!(
        receipt.outcome.is_ok(),
        "health read served: {:?}",
        receipt.outcome
    );

    child.kill().expect("stop the daemon");
    let _ = child.wait();
}
