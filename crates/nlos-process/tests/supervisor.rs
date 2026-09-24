//! B6-4 supervisor spawn / suspend / resume / kill chain acceptance tests.
//!
//! Platform matrix under test:
//!
//! - Unix (runs on macOS/Linux legs): real child spawn + registration,
//!   suspend/resume round-trip observable through a ticker child's frozen
//!   then resumed output, real kill (SIGTERM via the POSIX adapter) with
//!   the ESRCH → `AlreadyTerminated` mapping, generation fences on every
//!   signal operation, and fail-closed spawn-refusal child teardown;
//! - Windows (runs on the Windows leg): real child spawn + kill via the
//!   `taskkill /F /T` adapter (mirroring the W35-P4 live-child pattern),
//!   spawn-refusal teardown, and typed `UnsupportedOnPlatform` for
//!   suspend/resume (no safe suspend API under the workspace `unsafe`
//!   ban);
//! - hosts that are neither Unix nor Windows compile the same typed
//!   fail-closed kill arm (cfg-swap compile-verified; not CI-executed).

use std::collections::HashMap;
#[cfg(unix)]
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
#[cfg(unix)]
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(unix)]
use nlos_process::PosixPlatformKillAdapter;
#[cfg(windows)]
use nlos_process::WindowsPlatformKillAdapter;
use nlos_process::{
    PlatformKillAdapter, PlatformKillAdapterOutcome, ProcessSupervisor,
    RegisterSupervisorPidRequest, SpawnSupervisedRequest, SupervisorError, SupervisorPidDecision,
    SupervisorPidRegistryError, SupervisorSignalOutcome,
};
use nlos_types::{Generation, ProcessId};

// Temp-root fixture: only the Unix ticker test needs a filesystem root.
#[cfg(unix)]
static NEXT_ROOT: AtomicU64 = AtomicU64::new(1);

#[cfg(unix)]
struct TestRoot(PathBuf);

#[cfg(unix)]
impl TestRoot {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let sequence = NEXT_ROOT.fetch_add(1, Ordering::Relaxed);
        Self(std::env::temp_dir().join(format!(
            "nlos-process-supervisor-{label}-{}-{nonce}-{sequence}",
            std::process::id()
        )))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

#[cfg(unix)]
impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn spawn_request(process_id: ProcessId) -> SpawnSupervisedRequest {
    SpawnSupervisedRequest {
        process_id,
        process_generation: Generation::INITIAL,
        registered_at_ms: 5_000,
    }
}

#[cfg(unix)]
fn sleeper_command() -> Command {
    let mut command = Command::new("sleep");
    command.arg("600");
    command
}

#[cfg(windows)]
fn sleeper_command() -> Command {
    let mut command = Command::new("powershell");
    command
        .args(["-NoProfile", "-Command", "Start-Sleep -Seconds 600"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

#[cfg(any(unix, windows))]
fn wait_for_exit(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait().expect("poll child") {
            return Some(status);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn display_names_every_failure_class() {
    let process_id = ProcessId::from_bytes([0x31; 16]);
    let cases = [
        SupervisorError::Registry(SupervisorPidRegistryError::ProcessNotRegistered(process_id)),
        SupervisorError::SpawnRefused {
            cause: SupervisorPidRegistryError::OsPidRebind {
                process_id,
                generation: Generation::INITIAL,
                registered: 1,
                presented: 2,
            },
            torn_down_os_pid: 2,
        },
        SupervisorError::Spawn(std::io::Error::other("spawn refused")),
        SupervisorError::PlatformKill(nlos_process::PlatformKillAdapterError::Platform(
            "adapter refused",
        )),
        SupervisorError::UnsupportedOnPlatform {
            operation: "suspend",
        },
        SupervisorError::Signal("unix signal failed for registered os pid"),
    ];
    for case in &cases {
        assert!(!case.to_string().is_empty());
    }
}

/// Probe an OS pid through the host platform kill adapter: a refused spawn's
/// torn-down child must already be dead (`AlreadyTerminated`), not merely
/// absent from the supervisor registry.
#[cfg(any(unix, windows))]
fn assert_os_pid_already_terminated(os_pid: u32) {
    let probe_id = ProcessId::from_bytes([0xFD; 16]);
    let map = HashMap::from([(probe_id, os_pid)]);
    #[cfg(unix)]
    let adapter = PosixPlatformKillAdapter::new(map);
    #[cfg(windows)]
    let adapter = WindowsPlatformKillAdapter::new(map);
    let outcome = adapter
        .signal_platform_kill(probe_id, Generation::INITIAL)
        .expect("probe signal");
    assert!(
        matches!(outcome, PlatformKillAdapterOutcome::AlreadyTerminated),
        "refused child os_pid={os_pid} must already be dead, got {outcome:?}"
    );
}

/// Unix: spawn registers the observed OS pid; kill delivers a real SIGTERM
/// through the POSIX adapter and a re-kill of the reaped pid maps ESRCH to
/// `AlreadyTerminated`.
#[test]
#[cfg(unix)]
fn supervisor_spawn_registers_pid_and_kill_terminates_real_child() {
    let supervisor = ProcessSupervisor::new();
    let process_id = ProcessId::from_bytes([0x32; 16]);

    let mut spawned = supervisor
        .spawn_supervised(spawn_request(process_id), &mut sleeper_command())
        .expect("spawn supervised");
    assert!(matches!(
        spawned.decision(),
        SupervisorPidDecision::Registered(_)
    ));
    assert_eq!(
        supervisor
            .registry()
            .lookup(process_id)
            .expect("registered")
            .os_pid,
        spawned.os_pid()
    );
    assert!(
        spawned.child().try_wait().expect("poll").is_none(),
        "fresh child must be alive"
    );

    let kill_result = supervisor.kill(process_id, Generation::INITIAL);
    assert!(
        matches!(&kill_result, Ok(SupervisorSignalOutcome::Applied)),
        "kill failed: {kill_result:?}"
    );
    let status = wait_for_exit(spawned.child(), Duration::from_secs(30))
        .expect("child terminates within 30s of SIGTERM");
    assert!(!status.success());

    assert!(
        matches!(
            supervisor.kill(process_id, Generation::INITIAL),
            Ok(SupervisorSignalOutcome::AlreadyTerminated)
        ),
        "a re-kill of the reaped pid maps ESRCH to AlreadyTerminated"
    );
}

/// Windows: spawn registers the observed OS pid; kill delivers a real
/// `taskkill /F /T` through the Windows adapter and a re-kill of the dead
/// pid maps the taskkill 128 exit to `AlreadyTerminated` (W35-P4 pattern).
#[test]
#[cfg(windows)]
fn supervisor_spawn_registers_pid_and_kill_terminates_real_child() {
    let supervisor = ProcessSupervisor::new();
    let process_id = ProcessId::from_bytes([0x33; 16]);

    let mut spawned = supervisor
        .spawn_supervised(spawn_request(process_id), &mut sleeper_command())
        .expect("spawn supervised");
    assert!(matches!(
        spawned.decision(),
        SupervisorPidDecision::Registered(_)
    ));
    assert!(
        spawned.child().try_wait().expect("poll").is_none(),
        "fresh child must be alive"
    );

    let kill_result = supervisor.kill(process_id, Generation::INITIAL);
    assert!(
        matches!(&kill_result, Ok(SupervisorSignalOutcome::Applied)),
        "kill failed: {kill_result:?}"
    );
    let status = wait_for_exit(spawned.child(), Duration::from_secs(30))
        .expect("child terminates within 30s of taskkill /F /T");
    assert!(!status.success());

    assert!(matches!(
        supervisor.kill(process_id, Generation::INITIAL),
        Ok(SupervisorSignalOutcome::AlreadyTerminated)
    ));
}

/// Unix: the suspend/resume round-trip is observable through a ticker
/// child — output freezes while `SIGSTOP`ed and resumes after `SIGCONT`.
#[test]
#[cfg(unix)]
fn supervisor_suspend_resume_round_trip_freezes_and_resumes_child_progress() {
    let supervisor = ProcessSupervisor::new();
    let process_id = ProcessId::from_bytes([0x34; 16]);
    let root = TestRoot::new("ticker");
    std::fs::create_dir_all(root.path()).expect("create ticker root");
    let ticks_path = root.path().join("ticks");

    let ticks_file = std::fs::File::create(&ticks_path).expect("create ticks file");
    let mut ticker = Command::new("sh");
    ticker
        .arg("-c")
        .arg("while true; do echo tick; sleep 0.05; done")
        .stdout(Stdio::from(ticks_file))
        .stderr(Stdio::null());

    let mut spawned = supervisor
        .spawn_supervised(spawn_request(process_id), &mut ticker)
        .expect("spawn ticker");

    let count_ticks = || -> usize {
        match std::fs::read_to_string(&ticks_path) {
            Ok(content) => content.lines().filter(|line| !line.is_empty()).count(),
            Err(_) => 0,
        }
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while count_ticks() < 2 {
        assert!(Instant::now() < deadline, "ticker never started");
        std::thread::sleep(Duration::from_millis(50));
    }

    let suspend_result = supervisor.suspend(process_id, Generation::INITIAL);
    assert!(
        matches!(&suspend_result, Ok(SupervisorSignalOutcome::Applied)),
        "suspend failed: {suspend_result:?}"
    );
    // Any tick in flight at the SIGSTOP instant lands before the first
    // sample; the freeze claim is the count being stable BETWEEN the two
    // post-suspend samples.
    std::thread::sleep(Duration::from_millis(150));
    let frozen_a = count_ticks();
    std::thread::sleep(Duration::from_millis(350));
    let frozen_b = count_ticks();
    assert_eq!(
        frozen_a, frozen_b,
        "a SIGSTOPed child must stop producing ticks"
    );

    let resume_result = supervisor.resume(process_id, Generation::INITIAL);
    assert!(
        matches!(&resume_result, Ok(SupervisorSignalOutcome::Applied)),
        "resume failed: {resume_result:?}"
    );
    let resumed_deadline = Instant::now() + Duration::from_secs(5);
    while count_ticks() <= frozen_b {
        assert!(
            Instant::now() < resumed_deadline,
            "ticking never resumed after SIGCONT"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    let kill_result = supervisor.kill(process_id, Generation::INITIAL);
    assert!(
        matches!(&kill_result, Ok(SupervisorSignalOutcome::Applied)),
        "kill failed: {kill_result:?}"
    );
    let status = wait_for_exit(spawned.child(), Duration::from_secs(30))
        .expect("ticker teardown within 30s");
    assert!(!status.success());
}

/// Unix: every signal operation is fenced by the registry's monotonic
/// generation — a stale presentation never touches the current mapping's
/// pid, and the child survives the stale attempts.
#[test]
#[cfg(unix)]
fn supervisor_signals_fail_closed_on_stale_generation() {
    let supervisor = ProcessSupervisor::new();
    let process_id = ProcessId::from_bytes([0x35; 16]);
    let next = Generation::INITIAL.checked_next().expect("next");

    let mut spawned = supervisor
        .spawn_supervised(spawn_request(process_id), &mut sleeper_command())
        .expect("spawn supervised");

    for operation_error in [
        supervisor.suspend(process_id, next),
        supervisor.resume(process_id, next),
        supervisor.kill(process_id, next),
    ] {
        assert!(
            matches!(
                operation_error,
                Err(SupervisorError::Registry(
                    SupervisorPidRegistryError::StaleProcessGeneration { .. }
                ))
            ),
            "stale generation must fail closed, got {operation_error:?}"
        );
    }
    assert!(
        spawned.child().try_wait().expect("poll").is_none(),
        "the child must survive every stale-generation signal attempt"
    );

    let kill_result = supervisor.kill(process_id, Generation::INITIAL);
    assert!(
        matches!(&kill_result, Ok(SupervisorSignalOutcome::Applied)),
        "kill failed: {kill_result:?}"
    );
    let status =
        wait_for_exit(spawned.child(), Duration::from_secs(30)).expect("teardown within 30s");
    assert!(!status.success());
}

/// A refused registration (same-generation OS pid rebind) never leaves the
/// half-owned child behind: the just-spawned child is killed and reaped
/// before the typed rejection surfaces, the torn-down OS pid is already
/// dead under a host kill probe, and the original mapping is intact.
#[test]
#[cfg(any(unix, windows))]
fn spawn_supervised_refusal_reaps_the_child_and_keeps_the_original_mapping() {
    let supervisor = ProcessSupervisor::new();
    let process_id = ProcessId::from_bytes([0x36; 16]);

    supervisor
        .registry()
        .register(nlos_process::RegisterSupervisorPidRequest {
            process_id,
            process_generation: Generation::INITIAL,
            os_pid: std::process::id(),
            registered_at_ms: 4_000,
        })
        .expect("pre-register original mapping");

    let refusal = supervisor
        .spawn_supervised(spawn_request(process_id), &mut sleeper_command())
        .expect_err("same-generation rebind must be refused");
    let SupervisorError::SpawnRefused {
        cause,
        torn_down_os_pid,
    } = refusal
    else {
        panic!("expected SpawnRefused, got {refusal:?}");
    };
    assert!(
        matches!(cause, SupervisorPidRegistryError::OsPidRebind { .. }),
        "expected OsPidRebind cause, got {cause:?}"
    );
    assert_ne!(
        torn_down_os_pid,
        std::process::id(),
        "torn-down pid must be the refused child, not the pre-registered mapping"
    );
    assert_os_pid_already_terminated(torn_down_os_pid);

    let original = supervisor.registry().lookup(process_id).expect("mapping");
    assert_eq!(original.os_pid, std::process::id());
    assert_eq!(original.process_generation, Generation::INITIAL);
}

/// Windows (and any non-Unix host): suspend/resume fail closed with the
/// typed `UnsupportedOnPlatform` error — no invented capability, zero side
/// effect on the registered mapping.
#[test]
#[cfg(not(unix))]
fn supervisor_suspend_resume_are_typed_unsupported_off_unix() {
    let supervisor = ProcessSupervisor::new();
    let process_id = ProcessId::from_bytes([0x37; 16]);
    supervisor
        .registry()
        .register(nlos_process::RegisterSupervisorPidRequest {
            process_id,
            process_generation: Generation::INITIAL,
            os_pid: std::process::id(),
            registered_at_ms: 4_000,
        })
        .expect("register mapping");

    for (refusal, operation) in [
        (
            supervisor.suspend(process_id, Generation::INITIAL),
            "suspend",
        ),
        (supervisor.resume(process_id, Generation::INITIAL), "resume"),
    ] {
        assert!(matches!(
            refusal,
            Err(SupervisorError::UnsupportedOnPlatform {
                operation: expected
            }) if expected == operation
        ));
    }

    let original = supervisor.registry().lookup(process_id).expect("mapping");
    assert_eq!(original.os_pid, std::process::id());
}

/// A G1 kill must signal the `os_pid` resolved under that generation fence,
/// not a later `pid_map()` snapshot. Sequential stale-generation coverage
/// cannot catch this: if G2 supersedes *before* resolve, `kill(G1)` fails
/// closed and never reaches the adapter. The after-resolve hook interleaves
/// `Supersede` between the fence check and the adapter build — the TOCTOU
/// a full-registry `pid_map()` path is exposed to.
#[test]
#[cfg(any(unix, windows))]
fn supervisor_kill_binds_the_fenced_os_pid_across_a_g2_supersede() {
    let supervisor = ProcessSupervisor::new();
    let process_id = ProcessId::from_bytes([0x38; 16]);
    let next = Generation::INITIAL.checked_next().expect("next");

    let mut g1 = supervisor
        .spawn_supervised(spawn_request(process_id), &mut sleeper_command())
        .expect("spawn G1");
    let g1_pid = g1.os_pid();
    assert!(
        g1.child().try_wait().expect("poll G1").is_none(),
        "G1 child must be alive before the kill"
    );

    let mut g2 = sleeper_command().spawn().expect("spawn G2");
    let g2_pid = g2.id();
    assert_ne!(g1_pid, g2_pid, "the two generations must own distinct pids");
    assert!(
        g2.try_wait().expect("poll G2").is_none(),
        "G2 child must be alive before the kill"
    );

    supervisor.install_kill_after_resolve_hook(move |registry| {
        let decision = registry
            .register(RegisterSupervisorPidRequest {
                process_id,
                process_generation: next,
                os_pid: g2_pid,
                registered_at_ms: 6_000,
            })
            .expect("supersede G2 after G1 resolve");
        assert!(
            matches!(decision, SupervisorPidDecision::Superseded { .. }),
            "expected Superseded, got {decision:?}"
        );
    });

    let kill_result = supervisor.kill(process_id, Generation::INITIAL);
    assert!(
        matches!(&kill_result, Ok(SupervisorSignalOutcome::Applied)),
        "G1 kill must still apply to the fenced G1 pid, got {kill_result:?}"
    );

    // Give a signaled G2 time to exit: a pid_map() TOCTOU delivers SIGTERM
    // to G2, and an immediate try_wait can still see it alive. Waiting on
    // G1 first would only burn the 30s timeout on the surviving G1 child.
    assert!(
        wait_for_exit(&mut g2, Duration::from_millis(500)).is_none(),
        "G1 kill must not SIGTERM/taskkill the superseded G2 child"
    );

    let g1_status = wait_for_exit(g1.child(), Duration::from_secs(30))
        .expect("G1 child must die to the fenced kill");
    assert!(!g1_status.success());

    let g2_kill = supervisor.kill(process_id, next);
    assert!(
        matches!(&g2_kill, Ok(SupervisorSignalOutcome::Applied)),
        "G2 teardown kill failed: {g2_kill:?}"
    );
    let g2_status = wait_for_exit(&mut g2, Duration::from_secs(30)).expect("G2 teardown");
    assert!(!g2_status.success());
}
