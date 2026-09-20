//! W30-D lane: natural-language control over slice-k driven applications
//! through the REAL control chain — the restricted-grammar NL compiler of
//! `nlos-system-control` (read-only consumption, zero grammar changes) →
//! the same `ControlCommand` → the shared `SystemControl` handler
//! (envelope, authorization seam, §25.3 idempotency binding) → the landed
//! pluggable seams wired to this runtime's authorities:
//! `inspect process` reads the process authority's readback-validated
//! binding, `kill operation <pid> expecting <generation>` drives the
//! durable platform-kill path through the supervisor pid registry and the
//! platform adapter. Repeating the kill sentence replays the identical
//! durable receipt through an EMPTY pid map (short-circuit proof: a
//! broken replay would fail closed on the missing mapping).

#[cfg(unix)]
use std::collections::HashMap;
use std::sync::Arc;

#[cfg(unix)]
use nlos_process::PosixPlatformKillAdapter;
use nlos_runtime::{FiberState, RuntimeAdapter as _};
use nlos_runtime_tokio::{TokioRuntimeAdapter, TokioRuntimeConfig};
use nlos_slice_k::{SliceKError, SliceKRuntime, dispatch_nl_command, run_second_process_pair};
use nlos_system_control::control::{ControlError, ControlOutcome};

struct TempDir {
    root: std::path::PathBuf,
}

impl TempDir {
    fn new(name: &str) -> Self {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "nlos-slice-k-{name}-{}-{sequence}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("create temp root");
        Self { root }
    }

    fn root(&self) -> &std::path::Path {
        &self.root
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        match std::fs::remove_dir_all(&self.root) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("remove slice-k temp root: {error}"),
        }
    }
}

fn slice_adapter() -> TokioRuntimeAdapter {
    TokioRuntimeAdapter::new(
        tokio::runtime::Handle::current(),
        TokioRuntimeConfig::default(),
    )
    .expect("tokio adapter")
}

fn hex16(id: [u8; 16]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(32);
    for byte in id {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

async fn wait_for_fiber_state(
    adapter: &TokioRuntimeAdapter,
    handle: nlos_runtime::FiberHandle,
    expected: FiberState,
) {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if adapter.inspect(handle) == Ok(expected) {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("fiber did not reach expected state");
}

/// Shared body; `real_children` carries the two OS children on Unix. The
/// kill adapter is the real POSIX adapter on Unix and the noop contract
/// adapter elsewhere (the W29-F two-lane discipline).
#[allow(clippy::too_many_lines)]
async fn nl_control_body(
    dir: &TempDir,
    mut real_children: Option<(std::process::Child, std::process::Child)>,
) {
    let seed = 0xD8_u8;
    let runtime = Arc::new(SliceKRuntime::open(dir.root()).expect("open slice-k runtime"));
    let adapter = slice_adapter();
    let (os_pid_first, os_pid_second) = match &mut real_children {
        Some((first, second)) => (first.id(), second.id()),
        None => (std::process::id(), std::process::id()),
    };

    let pair = run_second_process_pair(&runtime, &adapter, seed, os_pid_first, os_pid_second)
        .await
        .expect("second process pair");
    wait_for_fiber_state(&adapter, pair.fiber_first, FiberState::Running).await;
    wait_for_fiber_state(&adapter, pair.fiber_second, FiberState::Running).await;

    #[cfg(unix)]
    let kill_adapter = PosixPlatformKillAdapter::new(pair.registry.pid_map());
    #[cfg(not(unix))]
    let kill_adapter = nlos_process::NoopPlatformKillAdapter;
    #[cfg(unix)]
    let empty_map_adapter = PosixPlatformKillAdapter::new(HashMap::new());
    #[cfg(not(unix))]
    let empty_map_adapter = nlos_process::NoopPlatformKillAdapter;

    // NL inspect (EN canonical form): the receipt carries the process
    // authority's own readback-validated facts.
    let inspect_receipt = dispatch_nl_command(
        &runtime,
        &pair.registry,
        &kill_adapter,
        &format!(
            "inspect process {}",
            hex16(*pair.process_first.process_id.as_bytes())
        ),
    )
    .expect("NL inspect dispatch");
    let inspected = match inspect_receipt.outcome.expect("NL inspect outcome") {
        ControlOutcome::ProcessInspected(inspected) => inspected,
        other => panic!("expected a process inspection, got {other:?}"),
    };
    assert_eq!(
        inspected.process_id,
        *pair.process_first.process_id.as_bytes()
    );
    assert_eq!(
        inspected.process_generation,
        pair.process_first.process_generation.get()
    );
    assert_eq!(inspected.task_id, *pair.process_first.task_id.as_bytes());

    // NL inspect (ZH synonym form) of the same process compiles to the same
    // command and answers from the same authority path.
    let zh_receipt = dispatch_nl_command(
        &runtime,
        &pair.registry,
        &kill_adapter,
        &format!(
            "查看进程 {}",
            hex16(*pair.process_first.process_id.as_bytes())
        ),
    )
    .expect("NL inspect (ZH) dispatch");
    assert!(
        matches!(zh_receipt.outcome, Ok(ControlOutcome::ProcessInspected(_))),
        "ZH synonym must reach the same inspect path: {zh_receipt:?}"
    );

    // An unknown process is a typed receipt failure, not a dispatch error.
    let unknown = dispatch_nl_command(
        &runtime,
        &pair.registry,
        &kill_adapter,
        "inspect process 0102030405060708090a0b0c0d0e0f10",
    )
    .expect("dispatch of an unknown inspect is itself well-formed");
    assert!(unknown.outcome.is_err(), "unknown process must fail typed");

    // NL kill of the SECOND process: the sentence's CAS expectation is the
    // process generation (1). The durable platform-kill receipt commits
    // before the adapter signals the OS (at-least-once).
    let kill_sentence = format!(
        "kill operation {} expecting 1",
        hex16(*pair.process_second.process_id.as_bytes())
    );
    let kill_receipt = dispatch_nl_command(&runtime, &pair.registry, &kill_adapter, &kill_sentence)
        .expect("NL kill dispatch");
    let receipt_id = match kill_receipt.outcome.expect("NL kill outcome") {
        ControlOutcome::OperationKilled { receipt_id } => receipt_id,
        other => panic!("expected an operation-killed receipt, got {other:?}"),
    };
    let durable_kill = runtime
        .process
        .inspect_platform_kill_receipt(
            pair.process_second.process_id,
            pair.process_second.process_generation,
        )
        .expect("kill receipt readback")
        .expect("the NL kill produced the durable platform-kill receipt");
    assert_eq!(
        durable_kill.idempotency_key.as_bytes(),
        pair.process_second.process_id.as_bytes(),
        "the NL kill's idempotency identity is the derived command id (the target id)"
    );
    if let Some((_, second_child)) = &mut real_children {
        let status = second_child.wait().expect("wait for killed child");
        assert!(!status.success(), "the real OS child died by signal");
    }

    // Sentence replay through an EMPTY pid map: the durable receipt
    // short-circuits before any adapter invocation (a broken replay would
    // fail closed on the missing mapping) and re-derives the identical
    // receipt id.
    let kill_replay =
        dispatch_nl_command(&runtime, &pair.registry, &empty_map_adapter, &kill_sentence)
            .expect("NL kill replay dispatch");
    let replayed_id = match kill_replay.outcome.expect("NL kill replay outcome") {
        ControlOutcome::OperationKilled { receipt_id } => receipt_id,
        other => panic!("expected a replayed operation-killed receipt, got {other:?}"),
    };
    assert_eq!(receipt_id, replayed_id);

    // A stale CAS expectation is refused typed without a second signal.
    let stale = dispatch_nl_command(
        &runtime,
        &pair.registry,
        &kill_adapter,
        &format!(
            "kill operation {} expecting 9",
            hex16(*pair.process_first.process_id.as_bytes())
        ),
    )
    .expect("stale-kill dispatch");
    assert!(stale.outcome.is_err(), "a stale CAS must refuse typed");
    assert!(
        runtime
            .process
            .inspect_platform_kill_receipt(
                pair.process_first.process_id,
                pair.process_first.process_generation
            )
            .expect("first kill receipt readback")
            .is_none(),
        "the refused stale kill took no durable side effect"
    );

    // Isolation: the first process was never addressed — OS child alive,
    // live fiber running, binding active and still NL-inspectable.
    if let Some((first_child, _)) = &mut real_children {
        assert!(first_child.try_wait().expect("first child alive").is_none());
    }
    assert_eq!(
        adapter.inspect(pair.fiber_first),
        Ok(FiberState::Running),
        "the untargeted process's live fiber keeps running"
    );
    let isolation_receipt = dispatch_nl_command(
        &runtime,
        &pair.registry,
        &kill_adapter,
        &format!(
            "inspect process {}",
            hex16(*pair.process_first.process_id.as_bytes())
        ),
    )
    .expect("NL inspect after the kill");
    assert!(isolation_receipt.outcome.is_ok());

    if let Some((first_child, _)) = &mut real_children {
        first_child.kill().expect("cleanup first child");
        first_child.wait().expect("reap first child");
    }

    // Out-of-grammar input — including an application-lifecycle verb the
    // whitelist does not carry — is a typed compiler rejection before any
    // authority is touched (no system-control changes were made for this
    // lane; the gap is reported in the evidence, not wired around).
    for sentence in [
        "uninstall application 0102030405060708090a0b0c0d0e0f10",
        "destroy the application please",
        "",
    ] {
        let rejection = dispatch_nl_command(&runtime, &pair.registry, &kill_adapter, sentence)
            .expect_err("out-of-grammar input must reject");
        assert!(
            matches!(
                &rejection,
                SliceKError::Control(ControlError::InvalidCommand(_))
            ),
            "unexpected rejection for {sentence:?}: {rejection:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(unix)]
async fn nl_control_routes_through_the_full_control_chain() {
    let dir = TempDir::new("nl-control");
    let child_first = std::process::Command::new("sleep")
        .arg("600")
        .spawn()
        .expect("spawn first real child");
    let child_second = std::process::Command::new("sleep")
        .arg("600")
        .spawn()
        .expect("spawn second real child");
    nl_control_body(&dir, Some((child_first, child_second))).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[cfg(not(unix))]
async fn nl_control_contract_via_noop_adapter_on_non_unix() {
    let dir = TempDir::new("nl-control-contract");
    nl_control_body(&dir, None).await;
}
