//! Acceptance tests for B-PROCESS-003 platform kill contract-layer minimum
//! prefix: durable kill receipt, stub adapter invocation, terminal fail-closed,
//! idempotent replay, (on Unix) real SIGTERM via [`PosixPlatformKillAdapter`],
//! (on Windows) real `taskkill /F /T` via [`WindowsPlatformKillAdapter`],
//! (on non-Windows) stub rejection for [`WindowsPlatformKillAdapter`], and
//! each adapter's missing-map / cross-platform stub fail-closed paths.

use nlos_process::{
    CreateIsolationDomainRequest, IsolationDomainDecision, MarkProcessTerminatedRequest,
    PlatformKillAdapter, PlatformKillAdapterError, PlatformKillAdapterOutcome,
    PlatformKillDecision, PosixPlatformKillAdapter, ProcessAuthority, ProcessAuthorityError,
    ProcessBindingDecision, ProcessLifecycleState, PropagateCrashRequest,
    RegisterDelegatedProcessRequest, RequestPlatformKillRequest, StubPlatformKillAdapter,
    WindowsPlatformKillAdapter,
};
use nlos_types::{Generation, IdempotencyKey, ProcessId, TaskAttemptId, TaskId};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

struct TestRoot(PathBuf);

impl TestRoot {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!(
            "nlos-process-platform-kill-{label}-{}-{nonce}-{sequence}",
            std::process::id()
        )))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Fixture {
    authority: ProcessAuthority,
    process_id: ProcessId,
    process_generation: Generation,
    process_fencing_token: nlos_process::FencingToken,
}

fn open_fixture(root: &TestRoot, seed: u8) -> Fixture {
    let authority = ProcessAuthority::open(root.path()).expect("open authority");
    let domain = match authority.create_isolation_domain(CreateIsolationDomainRequest {
        policy_digest: [seed; 32],
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(1); 16]),
        created_at_ms: 1_000,
    }) {
        Ok(
            IsolationDomainDecision::Created(record) | IsolationDomainDecision::Replayed(record),
        ) => record,
        Err(error) => panic!("domain: {error}"),
    };
    let binding = match authority.register_delegated_process(RegisterDelegatedProcessRequest {
        task_id: TaskId::from_bytes([seed.wrapping_add(2); 16]),
        task_attempt_id: TaskAttemptId::from_bytes([seed.wrapping_add(3); 16]),
        attempt_generation: Generation::INITIAL,
        isolation_domain_id: domain.isolation_domain_id,
        isolation_domain_generation: domain.generation,
        isolation_domain_fencing_token: domain.fencing_token,
        idempotency_key: IdempotencyKey::from_bytes([seed.wrapping_add(4); 16]),
        created_at_ms: 2_000,
    }) {
        Ok(
            ProcessBindingDecision::Registered(record) | ProcessBindingDecision::Replayed(record),
        ) => record,
        Err(error) => panic!("register: {error}"),
    };
    Fixture {
        authority,
        process_id: binding.process_id,
        process_generation: binding.process_generation,
        process_fencing_token: binding.process_fencing_token,
    }
}

fn kill_request(fixture: &Fixture, key: IdempotencyKey) -> RequestPlatformKillRequest {
    RequestPlatformKillRequest {
        process_id: fixture.process_id,
        expected_process_generation: fixture.process_generation,
        expected_process_fencing_token: fixture.process_fencing_token,
        idempotency_key: key,
        killed_at_ms: 12_000,
    }
}

#[test]
fn request_platform_kill_records_receipt_and_invokes_stub_adapter() {
    let root = TestRoot::new("stub-success");
    let fixture = open_fixture(&root, 60);
    let adapter = StubPlatformKillAdapter::new();
    let key = IdempotencyKey::from_bytes([0xA1; 16]);
    let request = kill_request(&fixture, key);

    let decision = fixture
        .authority
        .request_platform_kill(request, &adapter)
        .expect("platform kill");
    assert!(matches!(decision, PlatformKillDecision::Signaled(_)));
    assert_eq!(decision.receipt().idempotency_key, key);

    assert_eq!(
        adapter.recorded_signals(),
        vec![(fixture.process_id, fixture.process_generation)]
    );
    assert_eq!(
        fixture
            .authority
            .inspect_platform_kill_receipt(fixture.process_id, fixture.process_generation)
            .expect("inspect kill receipt")
            .as_ref(),
        Some(decision.receipt())
    );
    fixture
        .authority
        .inspect_active_process_binding(fixture.process_id)
        .expect("binding stays active until crash propagation");
}

#[test]
fn request_platform_kill_fail_closed_on_terminal_binding() {
    let root = TestRoot::new("terminal-reject");
    let terminated = open_fixture(&root, 61);
    terminated
        .authority
        .mark_process_terminated(MarkProcessTerminatedRequest {
            process_id: terminated.process_id,
            expected_process_generation: terminated.process_generation,
            expected_process_fencing_token: terminated.process_fencing_token,
            idempotency_key: IdempotencyKey::from_bytes([0xB1; 16]),
            marked_at_ms: 13_000,
        })
        .expect("terminate");

    let adapter = StubPlatformKillAdapter::new();
    assert!(matches!(
        terminated.authority.request_platform_kill(
            kill_request(&terminated, IdempotencyKey::from_bytes([0xA2; 16])),
            &adapter
        ),
        Err(ProcessAuthorityError::ProcessBindingTerminal(
            ProcessLifecycleState::Terminated
        ))
    ));
    assert!(adapter.recorded_signals().is_empty());

    let crashed = open_fixture(&root, 63);
    crashed
        .authority
        .propagate_crash(PropagateCrashRequest {
            process_id: crashed.process_id,
            expected_process_generation: crashed.process_generation,
            expected_process_fencing_token: crashed.process_fencing_token,
            idempotency_key: IdempotencyKey::from_bytes([0xC3; 16]),
            marked_at_ms: 13_500,
        })
        .expect("crash");
    assert!(matches!(
        crashed.authority.request_platform_kill(
            kill_request(&crashed, IdempotencyKey::from_bytes([0xA3; 16])),
            &StubPlatformKillAdapter::new()
        ),
        Err(ProcessAuthorityError::ProcessBindingTerminal(
            ProcessLifecycleState::Crashed
        ))
    ));
}

#[test]
fn request_platform_kill_replays_without_reinvoking_adapter() {
    let root = TestRoot::new("replay");
    let kill_key = IdempotencyKey::from_bytes([0xD1; 16]);
    let (process_id, generation, receipt) = {
        let fixture = open_fixture(&root, 62);
        let adapter = StubPlatformKillAdapter::new();
        let request = kill_request(&fixture, kill_key);
        let first = fixture
            .authority
            .request_platform_kill(request, &adapter)
            .expect("first kill");
        assert!(matches!(first, PlatformKillDecision::Signaled(_)));
        assert_eq!(adapter.recorded_signals().len(), 1);

        let replay = fixture
            .authority
            .request_platform_kill(request, &adapter)
            .expect("in-memory replay");
        assert!(matches!(replay, PlatformKillDecision::Replayed(_)));
        assert_eq!(replay.receipt(), first.receipt());
        assert_eq!(adapter.recorded_signals().len(), 1);

        (
            fixture.process_id,
            fixture.process_generation,
            first.receipt().clone(),
        )
    };

    let reopened = ProcessAuthority::open(root.path()).expect("reopen");
    let adapter = StubPlatformKillAdapter::new();
    let replay = reopened
        .request_platform_kill(
            RequestPlatformKillRequest {
                process_id,
                expected_process_generation: generation,
                expected_process_fencing_token: receipt.process_fencing_token,
                idempotency_key: kill_key,
                killed_at_ms: receipt.killed_at_ms,
            },
            &adapter,
        )
        .expect("reopen replay");
    assert!(matches!(replay, PlatformKillDecision::Replayed(_)));
    assert_eq!(replay.receipt(), &receipt);
    assert!(adapter.recorded_signals().is_empty());
}

#[derive(Debug, Default)]
struct AlreadyTerminatedKillAdapter {
    invocations: std::sync::Mutex<u32>,
}

impl AlreadyTerminatedKillAdapter {
    fn invocations(&self) -> u32 {
        *self
            .invocations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl PlatformKillAdapter for AlreadyTerminatedKillAdapter {
    fn signal_platform_kill(
        &self,
        _process_id: ProcessId,
        _process_generation: Generation,
    ) -> Result<PlatformKillAdapterOutcome, PlatformKillAdapterError> {
        *self
            .invocations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) += 1;
        Ok(PlatformKillAdapterOutcome::AlreadyTerminated)
    }
}

#[test]
fn request_platform_kill_already_terminated_commits_receipt_without_signaled() {
    let root = TestRoot::new("already-terminated");
    let fixture = open_fixture(&root, 64);
    let adapter = AlreadyTerminatedKillAdapter::default();
    let key = IdempotencyKey::from_bytes([0xE1; 16]);
    let request = kill_request(&fixture, key);

    let decision = fixture
        .authority
        .request_platform_kill(request, &adapter)
        .expect("already terminated kill");
    assert!(matches!(
        decision,
        PlatformKillDecision::AlreadyTerminated(_)
    ));
    assert_eq!(decision.receipt().idempotency_key, key);
    assert_eq!(adapter.invocations(), 1);

    assert_eq!(
        fixture
            .authority
            .inspect_platform_kill_receipt(fixture.process_id, fixture.process_generation)
            .expect("inspect kill receipt")
            .as_ref(),
        Some(decision.receipt())
    );

    let replay = fixture
        .authority
        .request_platform_kill(request, &adapter)
        .expect("replay");
    assert!(matches!(replay, PlatformKillDecision::Replayed(_)));
    assert_eq!(replay.receipt(), decision.receipt());
    assert_eq!(adapter.invocations(), 1);
}

#[test]
fn inspect_platform_kill_receipt_generation_scoped_and_conflict_fail_closed() {
    let root = TestRoot::new("inspect-generation");
    let fixture = open_fixture(&root, 65);
    let kill_key = IdempotencyKey::from_bytes([0xF1; 16]);
    let receipt = fixture
        .authority
        .request_platform_kill(
            kill_request(&fixture, kill_key),
            &StubPlatformKillAdapter::new(),
        )
        .expect("kill")
        .receipt()
        .clone();

    assert_eq!(
        fixture
            .authority
            .inspect_platform_kill_receipt(fixture.process_id, fixture.process_generation)
            .expect("inspect current generation")
            .as_ref(),
        Some(&receipt)
    );
    assert_eq!(
        fixture
            .authority
            .inspect_platform_kill_receipt(
                fixture.process_id,
                fixture
                    .process_generation
                    .checked_next()
                    .expect("next generation")
            )
            .expect("inspect stale generation"),
        None
    );

    assert!(matches!(
        fixture.authority.request_platform_kill(
            kill_request(&fixture, IdempotencyKey::from_bytes([0xF2; 16])),
            &StubPlatformKillAdapter::new()
        ),
        Err(ProcessAuthorityError::PlatformKillAlreadySignaled)
    ));

    let reopened = ProcessAuthority::open(root.path()).expect("reopen");
    assert_eq!(
        reopened
            .inspect_platform_kill_receipt(fixture.process_id, fixture.process_generation)
            .expect("inspect after reopen")
            .as_ref(),
        Some(&receipt)
    );
    let replay = reopened
        .request_platform_kill(
            RequestPlatformKillRequest {
                process_id: fixture.process_id,
                expected_process_generation: fixture.process_generation,
                expected_process_fencing_token: receipt.process_fencing_token,
                idempotency_key: kill_key,
                killed_at_ms: receipt.killed_at_ms,
            },
            &StubPlatformKillAdapter::new(),
        )
        .expect("reopen idempotent replay");
    assert!(matches!(replay, PlatformKillDecision::Replayed(_)));
    assert_eq!(replay.receipt(), &receipt);
}

#[test]
#[cfg(unix)]
fn posix_platform_kill_adapter_signals_real_child_process() {
    let root = TestRoot::new("posix-real-kill");
    let fixture = open_fixture(&root, 70);
    let mut child = std::process::Command::new("sleep")
        .arg("600")
        .spawn()
        .expect("spawn sleep child");
    let os_pid = child.id();

    let mut pid_map = HashMap::new();
    pid_map.insert(fixture.process_id, os_pid);
    let adapter = PosixPlatformKillAdapter::new(pid_map);

    let decision = fixture
        .authority
        .request_platform_kill(
            kill_request(&fixture, IdempotencyKey::from_bytes([0x71; 16])),
            &adapter,
        )
        .expect("posix platform kill");
    assert!(matches!(decision, PlatformKillDecision::Signaled(_)));

    let status = child.wait().expect("wait for signaled child");
    assert!(!status.success());
}

#[test]
#[cfg(not(windows))]
fn windows_platform_kill_adapter_unavailable_on_non_windows() {
    let root = TestRoot::new("windows-stub-reject");
    let fixture = open_fixture(&root, 71);
    let mut pid_map = HashMap::new();
    pid_map.insert(fixture.process_id, std::process::id());
    let adapter = WindowsPlatformKillAdapter::new(pid_map);

    assert!(matches!(
        adapter.signal_platform_kill(fixture.process_id, fixture.process_generation),
        Err(PlatformKillAdapterError::Platform(
            "windows platform kill adapter unavailable on non-windows"
        ))
    ));
}

#[test]
#[cfg(not(windows))]
fn windows_platform_kill_adapter_stub_rejects_via_authority_path() {
    let root = TestRoot::new("windows-authority-stub");
    let fixture = open_fixture(&root, 72);
    let mut pid_map = HashMap::new();
    pid_map.insert(fixture.process_id, std::process::id());
    let adapter = WindowsPlatformKillAdapter::new(pid_map);

    assert!(matches!(
        fixture.authority.request_platform_kill(
            kill_request(&fixture, IdempotencyKey::from_bytes([0x72; 16])),
            &adapter,
        ),
        Err(ProcessAuthorityError::PlatformKillAdapter(
            PlatformKillAdapterError::Platform(
                "windows platform kill adapter unavailable on non-windows"
            )
        ))
    ));
}

#[test]
#[cfg(windows)]
fn windows_platform_kill_adapter_missing_map_entry_returns_platform_error() {
    let process_id = ProcessId::from_bytes([0x77; 16]);
    let adapter = WindowsPlatformKillAdapter::new(HashMap::new());

    assert!(matches!(
        adapter.signal_platform_kill(process_id, Generation::INITIAL),
        Err(PlatformKillAdapterError::Platform(
            "os pid mapping not found for process id"
        ))
    ));
}

// Windows runner images carry no `sleep` binary. powershell.exe (Windows
// PowerShell 5.1) ships on every windows-2019/2022/2025 image, and spawning
// it directly — no `cmd /c` wrapper — keeps the `Child` pid in 1:1
// correspondence with the OS pid the adapter must terminate. `-NoProfile`
// sidesteps profile / execution-policy flakiness on CI hosts.
#[cfg(windows)]
fn spawn_sleeper() -> std::process::Child {
    use std::process::{Command, Stdio};

    Command::new("powershell")
        .args(["-NoProfile", "-Command", "Start-Sleep -Seconds 600"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn powershell sleeper child")
}

#[cfg(windows)]
fn wait_for_exit(
    child: &mut std::process::Child,
    timeout: std::time::Duration,
) -> Option<std::process::ExitStatus> {
    use std::time::Instant;

    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().expect("poll sleeper child") {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

#[cfg(windows)]
fn platform_kill_real_child(
    fixture: &Fixture,
    adapter: &WindowsPlatformKillAdapter,
    key: IdempotencyKey,
    child: &mut std::process::Child,
) -> nlos_process::PlatformKillReceipt {
    let decision = fixture
        .authority
        .request_platform_kill(kill_request(fixture, key), adapter)
        .expect("windows platform kill");
    assert!(matches!(decision, PlatformKillDecision::Signaled(_)));
    assert_eq!(decision.receipt().idempotency_key, key);
    assert_eq!(
        fixture
            .authority
            .inspect_platform_kill_receipt(fixture.process_id, fixture.process_generation)
            .expect("inspect kill receipt")
            .as_ref(),
        Some(decision.receipt())
    );

    // The mapped OS pid is really dead — forced termination, not a stub.
    let status = wait_for_exit(child, std::time::Duration::from_secs(30))
        .expect("child terminated within 30s of taskkill /F /T");
    assert!(!status.success());
    decision.receipt().clone()
}

#[test]
#[cfg(windows)]
fn windows_platform_kill_adapter_terminates_real_child_process() {
    use nlos_process::{RegisterSupervisorPidRequest, SupervisorPidRegistry};

    let root = TestRoot::new("windows-real-kill");
    let killed = open_fixture(&root, 73);
    let bystander = open_fixture(&root, 74);

    let mut killed_child = spawn_sleeper();
    let mut bystander_child = spawn_sleeper();

    let registry = SupervisorPidRegistry::new();
    for (fixture, os_pid) in [
        (&killed, killed_child.id()),
        (&bystander, bystander_child.id()),
    ] {
        registry
            .register(RegisterSupervisorPidRequest {
                process_id: fixture.process_id,
                process_generation: fixture.process_generation,
                os_pid,
                registered_at_ms: 3_000,
            })
            .expect("register supervisor pid");
    }

    assert!(
        killed_child
            .try_wait()
            .expect("poll killed child")
            .is_none()
    );
    assert!(
        bystander_child
            .try_wait()
            .expect("poll bystander child")
            .is_none()
    );

    let adapter = WindowsPlatformKillAdapter::new(registry.pid_map());
    let key = IdempotencyKey::from_bytes([0x73; 16]);
    let request = kill_request(&killed, key);
    let receipt = platform_kill_real_child(&killed, &adapter, key, &mut killed_child);

    // Referenced isolation: the bystander pid sits in the same pid_map yet is
    // untouched by the kill aimed at the other process identity (OS and
    // durable layers both).
    assert!(
        bystander_child
            .try_wait()
            .expect("poll bystander child")
            .is_none()
    );
    bystander
        .authority
        .inspect_active_process_binding(bystander.process_id)
        .expect("bystander binding stays active");

    // Re-signaling the now-dead pid maps to AlreadyTerminated, not an error.
    assert!(matches!(
        adapter.signal_platform_kill(killed.process_id, killed.process_generation),
        Ok(PlatformKillAdapterOutcome::AlreadyTerminated)
    ));

    // Exact-idempotency replay never re-invokes the adapter: an empty-map
    // adapter would fail closed if the adapter were consulted again.
    let replay = killed
        .authority
        .request_platform_kill(request, &WindowsPlatformKillAdapter::new(HashMap::new()))
        .expect("replay windows platform kill");
    assert!(matches!(replay, PlatformKillDecision::Replayed(_)));
    assert_eq!(replay.receipt(), &receipt);

    // Bystander teardown through its own binding — a second real Signaled —
    // so the sleeper never outlives the test run.
    platform_kill_real_child(
        &bystander,
        &adapter,
        IdempotencyKey::from_bytes([0x74; 16]),
        &mut bystander_child,
    );
}

#[test]
#[cfg(unix)]
fn posix_platform_kill_adapter_missing_map_entry_returns_platform_error() {
    let process_id = ProcessId::from_bytes([0x78; 16]);
    let adapter = PosixPlatformKillAdapter::new(HashMap::new());

    assert!(matches!(
        adapter.signal_platform_kill(process_id, Generation::INITIAL),
        Err(PlatformKillAdapterError::Platform(
            "os pid mapping not found for process id"
        ))
    ));
}

#[test]
#[cfg(windows)]
fn posix_platform_kill_adapter_unavailable_on_windows() {
    let process_id = ProcessId::from_bytes([0x79; 16]);
    let adapter = PosixPlatformKillAdapter::new(HashMap::new());

    assert!(matches!(
        adapter.signal_platform_kill(process_id, Generation::INITIAL),
        Err(PlatformKillAdapterError::Platform(
            "posix platform kill adapter unavailable on windows"
        ))
    ));
}
