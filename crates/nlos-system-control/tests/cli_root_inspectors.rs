//! CLI `--root` read-surface end-to-end tests (`process` + `resource` +
//! `application` features, all enabled by `daemon`): a small fixture root
//! with one real process binding drives the real `system-control-cli`
//! binary through a plain socket, proving the client-side inspector
//! composition returns a real authority projection instead of the
//! structural "unwired" exit-1 posture.

#![cfg(all(
    unix,
    feature = "process",
    feature = "resource",
    feature = "application"
))]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use nlos_ipc::unix::UnixListenerAdapter;
use nlos_ipc::{OutboundResponse, TransportConfig, serve_one};
use nlos_process::{
    CreateIsolationDomainRequest, ProcessAuthority, RegisterDelegatedProcessRequest,
};
use nlos_schema::sabi::v1::{ExchangeResponse, GetSystemControlRequest, SabiRequestContext};
use nlos_system_control::control::{CONTROL_CAPABILITY_GENERATION, CONTROL_CAPABILITY_SLOT};
use nlos_system_control::{RecoveryHealthSource, RecoverySystemControl, SystemControlAuthorizer};
use nlos_task::SqliteTaskAuthority;
use nlos_types::{Generation, IdempotencyKey, TaskAttemptId, TaskId};

static NEXT_PATH: AtomicU64 = AtomicU64::new(0);

struct TempPath(PathBuf);

impl TempPath {
    fn new(label: &str, suffix: &str) -> Self {
        let sequence = NEXT_PATH.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!(
            "nlos-cli-root-{label}-{suffix}-{}-{sequence}",
            std::process::id(),
        )))
    }
}

impl Drop for TempPath {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
        let _ = fs::remove_file(&self.0);
    }
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

/// Same capability posture as the daemon and every test harness.
struct CapabilityPolicy;

fn authorize(context: &SabiRequestContext) -> Result<(), &'static str> {
    let expected = nlos_schema::sabi::v1::CapabilityHandle {
        slot: CONTROL_CAPABILITY_SLOT,
        generation: CONTROL_CAPABILITY_GENERATION,
    };
    if context.capability_handles.as_slice() == [expected] {
        Ok(())
    } else {
        Err("missing recovery operations capability")
    }
}

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
        _: &nlos_schema::sabi::v1::ControlCommand,
    ) -> Result<(), &'static str> {
        authorize(context)
    }
}

#[derive(Clone)]
struct StubHealth(nlos_commit_coordinator::RecoveryWorkerHealth);

impl RecoveryHealthSource for StubHealth {
    fn recovery_health(&self) -> nlos_commit_coordinator::RecoveryWorkerHealth {
        self.0.clone()
    }
}

fn stub_health() -> StubHealth {
    StubHealth(nlos_commit_coordinator::RecoveryWorkerHealth {
        state: nlos_commit_coordinator::RecoveryWorkerState::Running,
        ..Default::default()
    })
}

/// Local trust-domain peer gate (the plain endpoint's posture; the
/// capability policy stays the boundary).
struct AllowPeer;

impl nlos_ipc::PeerAuthorizer for AllowPeer {
    fn authorize(&self, _: &nlos_ipc::PeerIdentity) -> Result<(), String> {
        Ok(())
    }
}

/// One durable delegated process binding, the same authority sequence as
/// the kill-executor fixture (a domain plus one delegated process).
fn delegated_process(root: &Path, seed: u8) -> nlos_process::ProcessBindingRecord {
    let authority = ProcessAuthority::open(root.join("process")).expect("open process authority");
    let domain = authority
        .create_isolation_domain(CreateIsolationDomainRequest {
            policy_digest: [seed; 32],
            idempotency_key: IdempotencyKey::from_bytes([seed; 16]),
            created_at_ms: 1_000,
        })
        .expect("create isolation domain")
        .record()
        .clone();
    authority
        .register_delegated_process(RegisterDelegatedProcessRequest {
            task_id: TaskId::from_bytes([seed; 16]),
            task_attempt_id: TaskAttemptId::from_bytes([seed.wrapping_add(1); 16]),
            attempt_generation: Generation::INITIAL,
            isolation_domain_id: domain.isolation_domain_id,
            isolation_domain_generation: domain.generation,
            isolation_domain_fencing_token: domain.fencing_token,
            idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(2); 16]),
            created_at_ms: 1_100,
        })
        .expect("register delegated process")
        .record()
        .clone()
}

/// The daemon's plain endpoint shape, minimized to what the CLI needs.
async fn serve_plain(socket: PathBuf, tasks: Arc<SqliteTaskAuthority>, stop: Arc<AtomicBool>) {
    let listener = UnixListenerAdapter::bind(socket).expect("bind plain socket");
    let health = stub_health();
    while !stop.load(Ordering::Relaxed) {
        let (stream, peer) = match listener.accept(TransportConfig::default()).await {
            Ok(connection) => connection,
            Err(nlos_ipc::IpcError::Timeout(nlos_ipc::IoOperation::Accept)) => continue,
            Err(_) => break,
        };
        let tasks = Arc::clone(&tasks);
        let health = health.clone();
        let _ = serve_one(
            stream,
            TransportConfig::default(),
            peer,
            &AllowPeer,
            move |validated| {
                let tasks = tasks;
                let health = health;
                async move {
                    let control =
                        RecoverySystemControl::new(tasks.as_ref(), &health, &CapabilityPolicy);
                    let response = control.handle_for_ipc(validated.envelope(), 10, 6_000);
                    Ok(OutboundResponse::Typed(ExchangeResponse {
                        envelope: Some(response),
                    }))
                }
            },
        )
        .await;
    }
}

fn run_cli(arguments: &[&str]) -> std::process::Output {
    let binary = std::env::var("CARGO_BIN_EXE_system-control-cli")
        .expect("system-control-cli binary missing; run tests with the cli feature");
    std::process::Command::new(binary)
        .args(arguments)
        .output()
        .expect("run system-control-cli")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn root_flag_composes_the_process_inspector_over_the_real_authority() {
    let root = TempPath::new("e2e", "root");
    fs::create_dir_all(&root.0).expect("create root");
    let binding = delegated_process(&root.0, 0x41);
    let process_hex = hex(binding.process_id.as_bytes());

    let database = TempPath::new("e2e", "tasks");
    let tasks = Arc::new(SqliteTaskAuthority::open(&database.0).expect("open task authority"));
    let socket = TempPath::new("e2e", "sock");
    let stop = Arc::new(AtomicBool::new(false));
    let server = tokio::spawn(serve_plain(
        socket.0.clone(),
        Arc::clone(&tasks),
        Arc::clone(&stop),
    ));

    // With --root the CLI composes the client-side inspector and returns
    // the real authority projection.
    let wired = run_cli(&[
        "--root",
        root.0.to_str().unwrap(),
        socket.0.to_str().unwrap(),
        "inspect-process",
        &process_hex,
    ]);
    let wired_stdout = String::from_utf8_lossy(&wired.stdout);
    assert!(
        wired.status.success(),
        "wired inspect-process failed: {}{}",
        wired_stdout,
        String::from_utf8_lossy(&wired.stderr),
    );
    assert!(
        wired_stdout.contains("outcome=process_inspected"),
        "real projection expected: {wired_stdout}"
    );
    assert!(
        wired_stdout.contains(&format!("process_id={process_hex}")),
        "projection names the binding: {wired_stdout}"
    );
    assert!(
        !wired_stdout.contains("not wired"),
        "no fake unwired failure: {wired_stdout}"
    );

    // An absent binding is an honest per-request typed failure (exit 1)
    // naming the authority, still not the structural unwired posture.
    let absent = run_cli(&[
        "--root",
        root.0.to_str().unwrap(),
        socket.0.to_str().unwrap(),
        "inspect-process",
        &hex(&[0xEE; 16]),
    ]);
    assert_eq!(absent.status.code(), Some(1));
    let absent_stdout = String::from_utf8_lossy(&absent.stdout);
    assert!(
        absent_stdout.contains("was not found"),
        "typed authority failure expected: {absent_stdout}"
    );
    assert!(
        !absent_stdout.contains("not wired"),
        "no fake unwired failure: {absent_stdout}"
    );

    // Without --root the read refuses before the wire with the honest
    // exit-2 hint.
    let refused = run_cli(&[socket.0.to_str().unwrap(), "inspect-process", &process_hex]);
    assert_eq!(refused.status.code(), Some(2));
    assert!(
        !String::from_utf8_lossy(&refused.stdout).contains("RECEIPT"),
        "a refused read must not print a receipt"
    );
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("--root"),
        "the refusal must explain the --root requirement"
    );

    stop.store(true, Ordering::Relaxed);
    server.abort();
}

#[test]
fn composed_reads_keep_refusing_without_root_even_without_a_server() {
    // The refusal happens before any socket or authority is touched, so no
    // server is needed (the socket path is never connected); the exit
    // contract is the honest exit-2 hint.
    let refused_resource = run_cli(&["/nonexistent.sock", "inspect-resource", &hex(&[0x61; 16])]);
    assert_eq!(refused_resource.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&refused_resource.stderr).contains("--root"),
        "resource refusal must explain --root"
    );

    let refused_application = run_cli(&[
        "/nonexistent.sock",
        "inspect-application",
        &hex(&[0x62; 16]),
    ]);
    assert_eq!(refused_application.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&refused_application.stderr).contains("--root"),
        "application refusal must explain --root"
    );

    // --root without a value is a usage error, never a silent default.
    let missing_value = run_cli(&["--root"]);
    assert_eq!(missing_value.status.code(), Some(2));
}
