//! W38-P2 (handover #2 uninstall sub-item, `C-APP-PAYLOAD`): the
//! `uninstall` CLI consumer — install through the real CLI, then mark the
//! application uninstalled through `nlos-package uninstall`, which drives
//! the same public
//! `ApplicationAuthority::uninstall_application` path the library fixtures
//! already exercise (B-APPLICATION-003).
//!
//! ```text
//! fixture → keygen/build/install (CLI)
//!        → nlos-package uninstall <package-id> --root <state>
//!        → terminal Uninstalled; re-uninstall replays
//! ```

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_application::ApplicationStatus;
use nlos_slice_k::{SliceKRuntime, execute_application_payload};
use nlos_types::PackageId;

const EXIT_USAGE: i32 = 1;
const EXIT_AUTHORITY: i32 = 7;

const PACKAGE: [u8; 16] = [
    0x5c, 0x4f, 0x2a, 0x81, 0xd9, 0x06, 0xb3, 0x74, 0xe8, 0x51, 0xfc, 0x27, 0x9a, 0x60, 0x1d, 0xb5,
];
const ENTRY_EXECUTABLE: &str = "hello";
const EXECUTABLE_PAYLOAD: &[u8] = b"EXEC-PAYLOAD-NEEDLE-uninstall-v1 bytes for the driver face";

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct ScratchDir {
    root: PathBuf,
}

impl ScratchDir {
    fn new(name: &str) -> Self {
        let sequence = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "nlos-package-uninstall-payload-{name}-{}-{sequence}",
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

fn hex_32(bytes: &[u8; 16]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn write_fixture_tree(dir: &Path) {
    fs::create_dir_all(dir.join("assets")).expect("assets dir");
    fs::write(
        dir.join("package.manifest"),
        format!(
            "# uninstall-payload e2e fixture\n\
             package-id = {}\n\
             version = 1.0.0\n\
             entry = {ENTRY_EXECUTABLE} executable hello.bin\n\
             entry = config data assets/config.bin\n",
            hex_32(&PACKAGE),
        ),
    )
    .expect("write manifest");
    fs::write(dir.join("hello.bin"), EXECUTABLE_PAYLOAD).expect("write hello.bin");
    fs::write(dir.join("assets/config.bin"), b"config-bytes-uninstall-e2e").expect("write config");
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
fn uninstall_cli_marks_terminal_and_replays() {
    let scratch = ScratchDir::new("e2e");
    let state = build_and_install(&scratch);
    let package_hex = hex_32(&PACKAGE);

    let uninstall = run_cli(&[
        "uninstall",
        &package_hex,
        "--root",
        &state.to_string_lossy(),
    ]);
    assert_eq!(
        uninstall.status.code(),
        Some(0),
        "uninstall failed:\n{}",
        stderr_text(&uninstall)
    );
    assert_eq!(stdout_line(&uninstall, "decision"), "uninstalled");
    let uninstall_id = stdout_line(&uninstall, "UNINSTALL");
    assert_eq!(uninstall_id.len(), 32);
    let application_line = stdout_line(&uninstall, "application");
    assert!(
        application_line.contains("generation 1"),
        "uninstall must report the installation generation: {application_line}"
    );
    assert!(
        application_line.contains(&package_hex),
        "uninstall must echo the package identity: {application_line}"
    );

    let runtime = SliceKRuntime::open(&state).expect("open CLI-written root");
    let view = runtime
        .applications
        .inspect_application(PackageId::from_bytes(PACKAGE))
        .expect("inspect")
        .expect("application row must remain after uninstall");
    assert_eq!(view.status, ApplicationStatus::Uninstalled);
    let receipt = runtime
        .applications
        .inspect_uninstall_receipt(PackageId::from_bytes(PACKAGE))
        .expect("inspect uninstall receipt")
        .expect("uninstall receipt must be durable");
    assert_eq!(hex_32(receipt.application_id.as_bytes()), uninstall_id);

    let refusal =
        execute_application_payload(&runtime, PackageId::from_bytes(PACKAGE), ENTRY_EXECUTABLE)
            .expect_err("uninstalled package must refuse payload execution");
    assert!(
        format!("{refusal}").contains("no application is installed")
            || format!("{refusal}").contains("uninstalled")
            || format!("{refusal}").contains("not installed"),
        "{refusal}"
    );

    let again = run_cli(&[
        "uninstall",
        &package_hex,
        "--root",
        &state.to_string_lossy(),
    ]);
    assert_eq!(
        again.status.code(),
        Some(0),
        "uninstall replay failed:\n{}",
        stderr_text(&again)
    );
    assert_eq!(stdout_line(&again, "decision"), "replayed");
    assert_eq!(stdout_line(&again, "UNINSTALL"), uninstall_id);
    assert!(
        stdout_line(&again, "application").contains("generation 1"),
        "replay must not change generation"
    );
}

#[test]
fn uninstall_negative_gates_stay_typed() {
    let scratch = ScratchDir::new("negative");
    let package_hex = hex_32(&PACKAGE);

    let no_root = run_cli(&["uninstall", &package_hex]);
    assert_eq!(no_root.status.code(), Some(EXIT_USAGE));

    // Malformed package id is a typed input refusal (exit 2), not usage.
    let bad_hex = run_cli(&[
        "uninstall",
        "not-a-package-id",
        "--root",
        &scratch.path("unused").to_string_lossy(),
    ]);
    assert_eq!(
        bad_hex.status.code(),
        Some(2),
        "malformed package id must be exit 2:\n{}",
        stderr_text(&bad_hex)
    );

    let empty = scratch.path("empty-state");
    let unknown = run_cli(&[
        "uninstall",
        &package_hex,
        "--root",
        &empty.to_string_lossy(),
    ]);
    assert_eq!(
        unknown.status.code(),
        Some(EXIT_AUTHORITY),
        "uninstall without install must be a typed authority refusal:\n{}",
        stderr_text(&unknown)
    );
    assert!(
        stderr_text(&unknown).contains("not found")
            || stderr_text(&unknown).contains("no application"),
        "expected ApplicationNotFound-style message:\n{}",
        stderr_text(&unknown)
    );
}
