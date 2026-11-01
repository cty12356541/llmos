//! Dual-layer escape-adversarial matrix (`W80-E6A`, closing `U-9` /
//! `RISK-E-04`): every test drives the full two-layer path — a real
//! isolated host child process spawned with an empty environment, running
//! an untrusted WAT guest under the in-process Wasmtime hard caps.
//!
//! Negative tests assert concrete identities (WASI errno numbers, trap
//! report tokens, typed report outcomes), never bare `is_err()`.
//!
//! Platform note: the symlink-escape scenario is covered by the unit
//! matrix under `#[cfg(unix)]`; on this Windows host it is delegated to
//! the three-platform CI. See
//! `docs/evidence/stage-e/e-sandbox-001-cross-platform-wasm-process-isolation.md`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use nlos_wasm_sandbox::process::{HostProcessRequest, run_isolated_host, spawn_env_audit};
use nlos_wasm_sandbox::profile::{PreopenGrant, SandboxProfile};
use nlos_wasm_sandbox::{HostOutcome, HostReport};

static COUNTER: AtomicU64 = AtomicU64::new(0);

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "nlos-e6a-matrix-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        Self(dir)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }

    fn write_wat(&self, tag: &str, wat: &str) -> PathBuf {
        let file = self.0.join(format!("{tag}.wat"));
        std::fs::write(&file, wat).expect("write wat");
        file
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _unused = std::fs::remove_dir_all(&self.0);
    }
}

fn host_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_nlos-wasm-sandbox-host"))
}

fn run_wat_via_child(
    scratch: &ScratchDir,
    tag: &str,
    wat: &str,
    profile: SandboxProfile,
) -> nlos_wasm_sandbox::process::HostProcessOutcome {
    let wasm_path = scratch.write_wat(tag, wat);
    let request = HostProcessRequest::new(host_exe(), &wasm_path, profile);
    run_isolated_host(&request, Some(Duration::from_secs(60))).expect("isolated host run")
}

fn expect_completed_code(outcome: &nlos_wasm_sandbox::process::HostProcessOutcome) -> i64 {
    let report: &HostReport = outcome
        .report
        .as_ref()
        .expect("report line must be present");
    assert_eq!(
        report.outcome,
        HostOutcome::Completed,
        "outcome detail: {} (stderr: {})",
        report.detail,
        String::from_utf8_lossy(&outcome.stderr)
    );
    report.code
}

/// **L1 env fence**: the isolated child observes zero environment variables,
/// while a raw (non-isolated) spawn of the same binary observes the ambient
/// environment — proving the test harness can actually detect leakage.
#[test]
fn isolated_child_inherits_zero_host_environment_variables() {
    let isolated = spawn_env_audit(host_exe()).expect("audit run");
    let report = isolated.report.as_ref().expect("audit report");
    assert_eq!(report.outcome, HostOutcome::AuditEnv);
    assert_eq!(report.code, 0, "env_clear child must observe 0 env vars");

    // Control direction: a raw spawn WITHOUT env_clear must observe the
    // ambient environment (>= 1 variable, e.g. PATH), proving the count
    // observable is not a constant zero.
    let raw = std::process::Command::new(host_exe())
        .arg("--self-audit-env")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .expect("raw spawn");
    let raw_report =
        HostReport::parse(&String::from_utf8_lossy(&raw.stderr)).expect("raw audit report");
    assert!(
        raw_report.code >= 1,
        "ambient environment must be observable in the control direction"
    );
}

/// **Path traversal**: `..` escapes and absolute paths (POSIX and Windows
/// drive forms) outside the preopen whitelist are denied with
/// `ENOTCAPABLE` (76).
#[test]
fn path_traversal_and_absolute_escapes_are_denied_with_notcapable() {
    let scratch = ScratchDir::new("traversal");
    let root = scratch.path().join("workspace");
    std::fs::create_dir_all(&root).expect("workspace");
    let hostile = scratch.path().join("hostile-secret.txt");
    std::fs::write(&hostile, b"hostile").expect("hostile file");

    let profile = SandboxProfile::deny_all()
        .with_fuel(5_000_000)
        .with_preopen(PreopenGrant::new("/workspace", &root, false).expect("grant"));

    let probe = |escape_path: &str| -> i64 {
        let len = escape_path.len();
        let wat = format!(
            r#"(module
                (import "wasi_snapshot_preview1" "path_open"
                    (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
                (memory (export "memory") 1)
                (data (i32.const 1024) "{escape_path}")
                (func (export "main") (result i32)
                    (call $path_open (i32.const 3) (i32.const 0)
                        (i32.const 1024) (i32.const {len}) (i32.const 0)
                        (i64.const 2) (i64.const 0) (i32.const 0) (i32.const 2048))))"#
        );
        let outcome = run_wat_via_child(&scratch, "trav", &wat, profile.clone());
        expect_completed_code(&outcome)
    };

    assert_eq!(
        probe("../../../hostile-secret.txt"),
        76,
        "dotdot escape must return ENOTCAPABLE"
    );
    assert_eq!(
        probe("/etc/passwd"),
        76,
        "POSIX-absolute escape must return ENOTCAPABLE"
    );
    assert_eq!(
        probe("C:/Windows/win.ini"),
        76,
        "Windows drive-absolute escape must return ENOTCAPABLE"
    );
}

/// **Whitelist positive control**: a granted read inside the preopen
/// succeeds and returns the exact file bytes — proving the fence filters
/// rather than blanket-denies.
#[test]
fn in_whitelist_read_succeeds_with_exact_bytes() {
    let scratch = ScratchDir::new("positive");
    let root = scratch.path().join("workspace");
    std::fs::create_dir_all(&root).expect("workspace");
    std::fs::write(root.join("allowed.txt"), b"nlos-e6").expect("seed");

    let profile = SandboxProfile::deny_all()
        .with_fuel(5_000_000)
        .with_preopen(PreopenGrant::new("/workspace", &root, false).expect("grant"));

    let wat = r#"(module
        (import "wasi_snapshot_preview1" "path_open"
            (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
        (import "wasi_snapshot_preview1" "fd_read"
            (func $fd_read (param i32 i32 i32 i32) (result i32)))
        (memory (export "memory") 1)
        (data (i32.const 1024) "allowed.txt")
        (func (export "main") (result i32)
            (local $errno i32) (local $fd i32)
            (local.set $errno (call $path_open (i32.const 3) (i32.const 0)
                (i32.const 1024) (i32.const 11) (i32.const 0)
                (i64.const 2) (i64.const 0) (i32.const 0) (i32.const 2048)))
            (if (i32.ne (local.get $errno) (i32.const 0))
                (then (return (i32.add (local.get $errno) (i32.const 1000)))))
            (local.set $fd (i32.load (i32.const 2048)))
            (i32.store (i32.const 3072) (i32.const 4096))
            (i32.store (i32.const 3076) (i32.const 64))
            (local.set $errno (call $fd_read (local.get $fd) (i32.const 3072) (i32.const 1) (i32.const 3078)))
            (if (i32.ne (local.get $errno) (i32.const 0))
                (then (return (i32.add (local.get $errno) (i32.const 2000)))))
            (i32.load (i32.const 3078))))"#;
    let outcome = run_wat_via_child(&scratch, "positive", wat, profile);
    assert_eq!(
        expect_completed_code(&outcome),
        7,
        "must read exactly 7 bytes"
    );
}

/// **Write fence**: a read-only grant denies CREAT opens with
/// `ENOTCAPABLE`; a write-granted root allows the same open and the bytes
/// land on the host side.
#[test]
fn readonly_grant_denies_writes_and_write_grant_lands_bytes() {
    let scratch = ScratchDir::new("writefence");
    let root = scratch.path().join("workspace");
    std::fs::create_dir_all(&root).expect("workspace");

    let deny_wat = r#"(module
        (import "wasi_snapshot_preview1" "path_open"
            (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
        (memory (export "memory") 1)
        (data (i32.const 1024) "new.txt")
        (func (export "main") (result i32)
            (call $path_open (i32.const 3) (i32.const 0)
                (i32.const 1024) (i32.const 7) (i32.const 1)
                (i64.const 64) (i64.const 0) (i32.const 0) (i32.const 2048))))"#;
    let readonly = SandboxProfile::deny_all()
        .with_fuel(5_000_000)
        .with_preopen(PreopenGrant::new("/workspace", &root, false).expect("grant"));
    let outcome = run_wat_via_child(&scratch, "deny", deny_wat, readonly);
    assert_eq!(
        expect_completed_code(&outcome),
        76,
        "CREAT on read-only grant must return ENOTCAPABLE"
    );
    assert!(
        !root.join("new.txt").exists(),
        "no file may appear through a read-only grant"
    );

    let allow_wat = r#"(module
        (import "wasi_snapshot_preview1" "path_open"
            (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
        (import "wasi_snapshot_preview1" "fd_write"
            (func $fd_write (param i32 i32 i32 i32) (result i32)))
        (memory (export "memory") 1)
        (data (i32.const 1024) "made.txt")
        (data (i32.const 2048) "e6w")
        (func (export "main") (result i32)
            (local $errno i32) (local $fd i32)
            (local.set $errno (call $path_open (i32.const 3) (i32.const 0)
                (i32.const 1024) (i32.const 8) (i32.const 1)
                (i64.const 64) (i64.const 0) (i32.const 0) (i32.const 2096)))
            (if (i32.ne (local.get $errno) (i32.const 0)) (then (return (local.get $errno))))
            (local.set $fd (i32.load (i32.const 2096)))
            (i32.store (i32.const 2112) (i32.const 2048))
            (i32.store (i32.const 2116) (i32.const 3))
            (local.set $errno (call $fd_write (local.get $fd) (i32.const 2112) (i32.const 1) (i32.const 2120)))
            (if (i32.ne (local.get $errno) (i32.const 0)) (then (return (i32.add (local.get $errno) (i32.const 1000)))))
            (if (i32.ne (i32.load (i32.const 2120)) (i32.const 3)) (then (return (i32.const 2000))))
            (i32.const 0)))"#;
    let writable = SandboxProfile::deny_all()
        .with_fuel(5_000_000)
        .with_preopen(PreopenGrant::new("/workspace", &root, true).expect("grant"));
    let outcome = run_wat_via_child(&scratch, "allow", allow_wat, writable);
    assert_eq!(
        expect_completed_code(&outcome),
        0,
        "write-granted run must succeed"
    );
    let content = std::fs::read(root.join("made.txt")).expect("created file");
    assert_eq!(content, b"e6w");
}

/// **Network fence**: socket imports do not exist in the allowlist surface,
/// so a guest importing `sock_open` is rejected at instantiation with an
/// unknown-import error naming the import.
#[test]
fn socket_import_is_absent_and_rejects_instantiation() {
    let scratch = ScratchDir::new("network");
    let wat = r#"(module
        (import "wasi_snapshot_preview1" "sock_open"
            (func $sock_open (param i32 i32 i32) (result i32)))
        (memory (export "memory") 1)
        (func (export "main") (result i32)
            (call $sock_open (i32.const 2) (i32.const 1) (i32.const 512))))"#;
    let outcome = run_wat_via_child(&scratch, "sock", wat, SandboxProfile::deny_all());
    let report = outcome.report.as_ref().expect("report");
    assert_eq!(report.outcome, HostOutcome::HostError);
    assert!(
        report.detail.contains("instantiation-rejected"),
        "detail was {}",
        report.detail
    );
    assert!(
        report.detail.contains("sock_open"),
        "unknown import must be named: {}",
        report.detail
    );
}

/// **Memory fence**: `memory.grow` beyond the page cap observes `-1`
/// (guest returns marker 42); a declared memory over the cap is rejected
/// at instantiation.
#[test]
fn memory_over_allocation_is_terminated_by_store_limiter() {
    let scratch = ScratchDir::new("memory");
    let grow_wat = r#"(module
        (memory (export "memory") 1)
        (func (export "main") (result i32)
            (local $prev i32)
            (local.set $prev (memory.grow (i32.const 100)))
            (if (i32.eq (local.get $prev) (i32.const -1)) (then (return (i32.const 42))))
            (local.get $prev)))"#;
    let profile = SandboxProfile::deny_all().with_max_memory_pages(4);
    let outcome = run_wat_via_child(&scratch, "grow", grow_wat, profile);
    assert_eq!(expect_completed_code(&outcome), 42, "grow must observe -1");

    let declare_wat = r#"(module
        (memory (export "memory") 8)
        (func (export "main") (result i32) (i32.const 0)))"#;
    let outcome = run_wat_via_child(
        &scratch,
        "declare",
        declare_wat,
        SandboxProfile::deny_all().with_max_memory_pages(4),
    );
    let report = outcome.report.as_ref().expect("report");
    assert_eq!(report.outcome, HostOutcome::HostError);
    assert!(
        report.detail.contains("instantiation-rejected"),
        "detail was {}",
        report.detail
    );
}

/// **Fuel fence**: a spinning guest is terminated by deterministic fuel
/// exhaustion; the trap is reported as `out-of-fuel` both times across two
/// runs (deterministic denial).
#[test]
fn fuel_exhaustion_terminates_guest_deterministically() {
    let scratch = ScratchDir::new("fuel");
    let wat = r#"(module
        (memory (export "memory") 1)
        (func (export "main") (result i32)
            (local $i i64)
            (loop $l
                (local.set $i (i64.add (local.get $i) (i64.const 1)))
                (br_if $l (i64.lt_u (local.get $i) (i64.const 1000000000))))
            (i32.const 0)))"#;
    let profile = SandboxProfile::deny_all().with_fuel(50_000);
    for round in 1..=2 {
        let outcome = run_wat_via_child(&scratch, "fuel", wat, profile.clone());
        let report = outcome.report.as_ref().expect("report");
        assert_eq!(report.outcome, HostOutcome::Trap, "round {round}");
        assert_eq!(report.detail, "out-of-fuel", "round {round}");
    }
}

/// **Epoch fence**: with fuel effectively unbounded, the wall-clock epoch
/// deadline interrupts the guest first.
#[test]
fn epoch_deadline_interrupts_before_fuel_exhaustion() {
    let scratch = ScratchDir::new("epoch");
    let wat = r#"(module
        (memory (export "memory") 1)
        (func (export "main") (result i32)
            (loop $l (br $l))
            (i32.const 0)))"#;
    let profile = SandboxProfile::deny_all()
        .with_fuel(100_000_000_000)
        .with_epoch_deadline(2, 1);
    let outcome = run_wat_via_child(&scratch, "epoch", wat, profile);
    let report = outcome.report.as_ref().expect("report");
    assert_eq!(report.outcome, HostOutcome::Trap);
    assert_eq!(report.detail, "epoch-interrupt");
}

/// **Env fence at the WASI layer**: the guest observes an empty
/// environment (0 vars, 0 buffer bytes) and `environ_get` stays `SUCCESS`
/// even with wild pointers.
#[test]
fn wasi_layer_environment_is_empty_for_isolated_child() {
    let scratch = ScratchDir::new("wasenv");
    let wat = r#"(module
        (import "wasi_snapshot_preview1" "environ_sizes_get"
            (func $env_sizes (param i32 i32) (result i32)))
        (import "wasi_snapshot_preview1" "environ_get"
            (func $env_get (param i32 i32) (result i32)))
        (memory (export "memory") 1)
        (func (export "main") (result i32)
            (if (i32.ne (call $env_sizes (i32.const 100) (i32.const 104)) (i32.const 0))
                (then (return (i32.const 100))))
            (if (i32.ne (i32.load (i32.const 100)) (i32.const 0))
                (then (return (i32.const 101))))
            (if (i32.ne (i32.load (i32.const 104)) (i32.const 0))
                (then (return (i32.const 102))))
            (if (i32.ne (call $env_get (i32.const -1) (i32.const -1)) (i32.const 0))
                (then (return (i32.const 103))))
            (i32.const 0)))"#;
    let outcome = run_wat_via_child(&scratch, "wasenv", wat, SandboxProfile::deny_all());
    assert_eq!(expect_completed_code(&outcome), 0);
}

/// **Argv fence**: the guest sees exactly the declared argv and nothing
/// else.
#[test]
fn guest_argv_is_exactly_the_declared_explicit_arguments() {
    let scratch = ScratchDir::new("argv");
    let wat = r#"(module
        (import "wasi_snapshot_preview1" "args_sizes_get"
            (func $args_sizes (param i32 i32) (result i32)))
        (import "wasi_snapshot_preview1" "args_get"
            (func $args_get (param i32 i32) (result i32)))
        (memory (export "memory") 1)
        (func (export "main") (result i32)
            (if (i32.ne (call $args_sizes (i32.const 100) (i32.const 104)) (i32.const 0))
                (then (return (i32.const 100))))
            (if (i32.ne (i32.load (i32.const 100)) (i32.const 2))
                (then (return (i32.const 101))))
            (if (i32.ne (i32.load (i32.const 104)) (i32.const 11))
                (then (return (i32.const 102))))
            (if (i32.ne (call $args_get (i32.const 200) (i32.const 4096)) (i32.const 0))
                (then (return (i32.const 103))))
            (if (i32.ne (i32.load8_u (i32.const 4096)) (i32.const 97))
                (then (return (i32.const 104))))
            (if (i32.ne (i32.load8_u (i32.const 4101)) (i32.const 0))
                (then (return (i32.const 105))))
            (if (i32.ne (i32.load8_u (i32.const 4102)) (i32.const 98))
                (then (return (i32.const 106))))
            (i32.const 0)))"#;
    let profile = SandboxProfile::deny_all().with_guest_args(["alpha", "beta"]);
    let outcome = run_wat_via_child(&scratch, "argv", wat, profile);
    assert_eq!(expect_completed_code(&outcome), 0);
}

/// **Capability discovery**: a granted preopen is discoverable at fd 3
/// with its declared name; with the deny-all profile fd 3 is `EBADF`.
#[test]
fn preopen_discovery_reports_grant_and_default_is_badf() {
    let scratch = ScratchDir::new("prestat");
    let root = scratch.path().join("workspace");
    std::fs::create_dir_all(&root).expect("workspace");

    let discover_wat = r#"(module
        (import "wasi_snapshot_preview1" "fd_prestat_get"
            (func $psg (param i32 i32) (result i32)))
        (import "wasi_snapshot_preview1" "fd_prestat_dir_name"
            (func $pdn (param i32 i32 i32) (result i32)))
        (memory (export "memory") 1)
        (func (export "main") (result i32)
            (local $e i32) (local $len i32)
            (local.set $e (call $psg (i32.const 3) (i32.const 512)))
            (if (i32.ne (local.get $e) (i32.const 0)) (then (return (i32.const 100))))
            (if (i32.ne (i32.load (i32.const 512)) (i32.const 0)) (then (return (i32.const 101))))
            (local.set $len (i32.load (i32.const 516)))
            (if (i32.ne (local.get $len) (i32.const 10)) (then (return (i32.const 102))))
            (local.set $e (call $pdn (i32.const 3) (i32.const 640) (local.get $len)))
            (if (i32.ne (local.get $e) (i32.const 0)) (then (return (i32.const 103))))
            (if (i32.ne (i32.load8_u (i32.const 640)) (i32.const 47)) (then (return (i32.const 104))))
            (i32.const 0)))"#;
    let profile = SandboxProfile::deny_all()
        .with_fuel(5_000_000)
        .with_preopen(PreopenGrant::new("/workspace", &root, false).expect("grant"));
    let outcome = run_wat_via_child(&scratch, "discover", discover_wat, profile);
    assert_eq!(expect_completed_code(&outcome), 0);

    let probe_wat = r#"(module
        (import "wasi_snapshot_preview1" "path_open"
            (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
        (memory (export "memory") 1)
        (data (i32.const 1024) "any.txt")
        (func (export "main") (result i32)
            (call $path_open (i32.const 3) (i32.const 0)
                (i32.const 1024) (i32.const 7) (i32.const 0)
                (i64.const 2) (i64.const 0) (i32.const 0) (i32.const 2048))))"#;
    let outcome = run_wat_via_child(&scratch, "probe", probe_wat, SandboxProfile::deny_all());
    assert_eq!(
        expect_completed_code(&outcome),
        8,
        "deny-all profile must return EBADF for fd 3"
    );
}

/// **Guest stdout pipe**: guest bytes flow through the child's stdout and
/// the report carries the exact emitted length.
#[test]
fn guest_stdout_flows_through_isolated_child_pipe() {
    let scratch = ScratchDir::new("stdout");
    let wat = r#"(module
        (import "wasi_snapshot_preview1" "fd_write"
            (func $fd_write (param i32 i32 i32 i32) (result i32)))
        (memory (export "memory") 1)
        (data (i32.const 1024) "e6a-ok\n")
        (func (export "main") (result i32)
            (i32.store (i32.const 2048) (i32.const 1024))
            (i32.store (i32.const 2052) (i32.const 7))
            (if (i32.ne (call $fd_write (i32.const 1) (i32.const 2048) (i32.const 1) (i32.const 2056)) (i32.const 0))
                (then (return (i32.const 100))))
            (i32.const 0)))"#;
    let outcome = run_wat_via_child(&scratch, "stdout", wat, SandboxProfile::deny_all());
    assert_eq!(expect_completed_code(&outcome), 0);
    assert!(
        outcome.stdout.starts_with(b"e6a-ok\n"),
        "stdout was {:?}",
        outcome.stdout
    );
    let report = outcome.report.as_ref().expect("report");
    assert_eq!(report.guest_stdout_len, 7);
}

/// **L1 hard kill**: an unbounded guest (fuel effectively unlimited, epoch
/// disabled) is terminated by the parent's wall-clock deadline kill.
#[test]
fn wall_clock_deadline_hard_kills_unbounded_guest() {
    let scratch = ScratchDir::new("kill");
    let wat = r#"(module
        (memory (export "memory") 1)
        (func (export "main") (result i32)
            (loop $l (br $l))
            (i32.const 0)))"#;
    let wasm_path = scratch.write_wat("kill", wat);
    let profile = SandboxProfile::deny_all().with_fuel(u64::MAX);
    let request = HostProcessRequest::new(host_exe(), &wasm_path, profile);
    let start = Instant::now();
    let outcome =
        run_isolated_host(&request, Some(Duration::from_millis(400))).expect("isolated run");
    let elapsed = start.elapsed();
    assert!(
        outcome.timed_out_and_killed,
        "parent must hard-kill the unbounded guest (stderr: {})",
        String::from_utf8_lossy(&outcome.stderr)
    );
    assert!(
        elapsed < Duration::from_secs(30),
        "kill must be prompt, took {elapsed:?}"
    );
}
