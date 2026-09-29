//! W38-P2 (handover #2 back slice, `C-APP-PAYLOAD`): the `run` CLI
//! consumer — install through the real CLI, then execute one installed
//! executable entry through `nlos-package run`, which drives the same
//! kernel payload-execution lane the W35-P2 front slice closed.
//!
//! ```text
//! fixture → keygen/build/install (CLI)
//!        → nlos-package run <package-id> <entry> --root <state>
//!        → durable outcome + receipts on stdout; re-run replays
//! ```

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_slice_k::{SliceKRuntime, execute_application_payload, payload_execution_seed};
use nlos_types::PackageId;

const EXIT_USAGE: i32 = 1;
const EXIT_RUN: i32 = 7;

const PACKAGE: [u8; 16] = [
    0x5c, 0x4f, 0x2a, 0x81, 0xd9, 0x06, 0xb3, 0x74, 0xe8, 0x51, 0xfc, 0x27, 0x9a, 0x60, 0x1d, 0xb5,
];
const ENTRY_EXECUTABLE: &str = "hello";
const EXECUTABLE_PAYLOAD: &[u8] = b"EXEC-PAYLOAD-NEEDLE-3e7b91 payload bytes for the driver face";

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct ScratchDir {
    root: PathBuf,
}

impl ScratchDir {
    fn new(name: &str) -> Self {
        let sequence = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "nlos-package-run-payload-{name}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("scratch dir");
        Self { root }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn run_cli(arguments: &[&str]) -> Output {
    let binary = std::env::var("CARGO_BIN_EXE_nlos-package")
        .expect("nlos-package binary missing; run tests with default features");
    std::process::Command::new(binary)
        .args(arguments)
        .output()
        .expect("spawn nlos-package")
}

fn stdout_line(output: &Output, prefix: &str) -> String {
    let stdout = String::from_utf8(output.stdout.clone()).expect("utf-8 stdout");
    stdout
        .lines()
        .find_map(|line| line.strip_prefix(prefix))
        .unwrap_or_else(|| panic!("no {prefix:?} line in stdout:\n{stdout}"))
        .trim()
        .to_string()
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("utf-8 stderr")
}

fn write_fixture_tree(dir: &Path) {
    fs::create_dir_all(dir.join("assets")).expect("assets dir");
    fs::write(
        dir.join("package.manifest"),
        format!(
            "# run-payload e2e fixture\n\
             package-id = {}\n\
             version = 1.0.0\n\
             entry = {ENTRY_EXECUTABLE} executable hello.bin\n\
             entry = config data assets/config.bin\n",
            hex_of(&PACKAGE),
        ),
    )
    .expect("write manifest");
    fs::write(dir.join("hello.bin"), EXECUTABLE_PAYLOAD).expect("write hello.bin");
    fs::write(dir.join("assets/config.bin"), b"config-bytes-run-e2e").expect("write config");
}

fn hex_of(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn build_and_install(scratch: &ScratchDir) -> PathBuf {
    let tree = scratch.path("tree");
    write_fixture_tree(&tree);
    let key = scratch.path("e2e.devkey");
    let keygen = run_cli(&[
        "keygen",
        "--seed",
        &"ab".repeat(32),
        "--out",
        &key.to_string_lossy(),
    ]);
    assert_eq!(
        keygen.status.code(),
        Some(0),
        "keygen failed:\n{}",
        stderr_text(&keygen)
    );
    let package = scratch.path("fixture.nlospkg");
    let build = run_cli(&[
        "build",
        &tree.to_string_lossy(),
        "--key",
        &key.to_string_lossy(),
        "--out",
        &package.to_string_lossy(),
    ]);
    assert_eq!(
        build.status.code(),
        Some(0),
        "build failed:\n{}",
        stderr_text(&build)
    );
    let state = scratch.path("state");
    let install = run_cli(&[
        "install",
        &package.to_string_lossy(),
        "--root",
        &state.to_string_lossy(),
    ]);
    assert_eq!(
        install.status.code(),
        Some(0),
        "install failed:\n{}",
        stderr_text(&install)
    );
    assert_eq!(stdout_line(&install, "decision"), "installed");
    state
}

#[test]
fn run_cli_executes_installed_payload_and_replays() {
    let scratch = ScratchDir::new("e2e");
    let state = build_and_install(&scratch);
    let package_hex = hex_of(&PACKAGE);

    let first = run_cli(&[
        "run",
        &package_hex,
        ENTRY_EXECUTABLE,
        "--root",
        &state.to_string_lossy(),
    ]);
    assert_eq!(
        first.status.code(),
        Some(0),
        "run failed:\n{}",
        stderr_text(&first)
    );
    assert_eq!(stdout_line(&first, "decision"), "executed");
    assert_eq!(stdout_line(&first, "entry"), ENTRY_EXECUTABLE);
    let application_hex = stdout_line(&first, "RUN");
    assert_eq!(application_hex.len(), 32);
    assert_eq!(stdout_line(&first, "package"), package_hex);

    let outcome_line = stdout_line(&first, "outcome");
    assert!(
        outcome_line.starts_with("completed:") || outcome_line.starts_with("failed:"),
        "outcome must be a typed provider terminal: {outcome_line}"
    );
    let operation_hex = stdout_line(&first, "operation");
    assert_eq!(operation_hex.len(), 32);

    // The same root's lane agrees with the CLI: re-execution through the
    // library face is a full replay whose outcome matches CLI stdout, and
    // the seed-derived provider outcome recomputes bit-identical.
    let runtime = SliceKRuntime::open(&state).expect("open CLI-written root");
    let library =
        execute_application_payload(&runtime, PackageId::from_bytes(PACKAGE), ENTRY_EXECUTABLE)
            .expect("library re-execution after CLI run");
    assert!(library.register_replayed);
    assert!(library.dispatch_replayed);
    assert!(library.complete_replayed);
    assert_eq!(hex_of(library.application_id.as_bytes()), application_hex);
    assert_eq!(
        hex_of(library.operation.operation_id.as_bytes()),
        operation_hex
    );
    let expected = nlos_driver_mock::derive_provider_outcome(
        library.operation,
        library.callback_id,
        &payload_execution_seed(EXECUTABLE_PAYLOAD),
    );
    assert_eq!(library.outcome, expected);

    // Re-run through the CLI replays every boundary.
    let again = run_cli(&[
        "run",
        &package_hex,
        ENTRY_EXECUTABLE,
        "--root",
        &state.to_string_lossy(),
    ]);
    assert_eq!(again.status.code(), Some(0), "{}", stderr_text(&again));
    assert_eq!(stdout_line(&again, "decision"), "replayed");
    assert_eq!(stdout_line(&again, "RUN"), application_hex);
    assert_eq!(stdout_line(&again, "outcome"), outcome_line);
    assert_eq!(stdout_line(&again, "operation"), operation_hex);
}

#[test]
fn run_negative_gates_stay_typed() {
    let scratch = ScratchDir::new("negative");
    let state = build_and_install(&scratch);
    let package_hex = hex_of(&PACKAGE);

    let no_root = run_cli(&["run", &package_hex, ENTRY_EXECUTABLE]);
    assert_eq!(no_root.status.code(), Some(EXIT_USAGE));

    let no_entry = run_cli(&["run", &package_hex, "--root", &state.to_string_lossy()]);
    assert_eq!(no_entry.status.code(), Some(EXIT_USAGE));

    let unknown = run_cli(&[
        "run",
        &"00".repeat(16),
        ENTRY_EXECUTABLE,
        "--root",
        &state.to_string_lossy(),
    ]);
    assert_eq!(
        unknown.status.code(),
        Some(EXIT_RUN),
        "unknown package must be a typed run refusal:\n{}",
        stderr_text(&unknown)
    );
    assert!(
        stderr_text(&unknown).contains("no application is installed")
            || stderr_text(&unknown).contains("payload execution"),
        "{}",
        stderr_text(&unknown)
    );
}
