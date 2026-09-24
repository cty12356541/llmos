//! W38-P2 (handover #2 back slice, `C-APP-PAYLOAD` update sub-item): the
//! `update` CLI consumer — install through the real CLI, then advance one
//! installed application to a new verified package generation through
//! `nlos-package update`, which drives the same public
//! `ApplicationAuthority::update_application` path the library fixtures
//! already exercise (B-APPLICATION-002).
//!
//! ```text
//! fixture v1 → keygen/build/install (CLI)
//!          → fixture v1.0.1 (new manifest) → keygen/build
//!          → nlos-package update <pkg> --root <state>
//!          → generation advances; re-update replays
//! ```

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_artifact::ContentDigest;
use nlos_slice_k::{SliceKRuntime, execute_application_payload, payload_execution_seed};
use nlos_types::PackageId;

const EXIT_USAGE: i32 = 1;
const EXIT_AUTHORITY: i32 = 7;

const PACKAGE: [u8; 16] = [
    0x5c, 0x4f, 0x2a, 0x81, 0xd9, 0x06, 0xb3, 0x74, 0xe8, 0x51, 0xfc, 0x27, 0x9a, 0x60, 0x1d, 0xb5,
];
const ENTRY_EXECUTABLE: &str = "hello";
const EXECUTABLE_V1: &[u8] = b"EXEC-PAYLOAD-NEEDLE-update-v1 bytes for the driver face";
const EXECUTABLE_V2: &[u8] = b"EXEC-PAYLOAD-NEEDLE-update-v2 bytes after content advance";

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct ScratchDir {
    root: PathBuf,
}

impl ScratchDir {
    fn new(name: &str) -> Self {
        let sequence = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "nlos-package-update-payload-{name}-{}-{sequence}",
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

fn write_fixture_tree(dir: &Path, version: &str, executable: &[u8]) {
    fs::create_dir_all(dir.join("assets")).expect("assets dir");
    fs::write(
        dir.join("package.manifest"),
        format!(
            "# update-payload e2e fixture\n\
             package-id = {}\n\
             version = {version}\n\
             entry = {ENTRY_EXECUTABLE} executable hello.bin\n\
             entry = config data assets/config.bin\n",
            hex_32(&PACKAGE),
        ),
    )
    .expect("write manifest");
    fs::write(dir.join("hello.bin"), executable).expect("write hello.bin");
    fs::write(dir.join("assets/config.bin"), b"config-bytes-update-e2e").expect("write config");
}

/// keygen + build through the real CLI, returning the package file path.
fn build_fixture_package(scratch: &ScratchDir, tree: &Path, out_name: &str) -> PathBuf {
    let key = scratch.path("e2e.devkey");
    if !key.exists() {
        let keygen = run_cli(&[
            "keygen",
            "--seed",
            &"cd".repeat(32),
            "--out",
            &key.to_string_lossy(),
        ]);
        assert_eq!(
            keygen.status.code(),
            Some(0),
            "keygen failed:\n{}",
            stderr_text(&keygen)
        );
    }
    let package = scratch.path(out_name);
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
    package
}

fn install_v1(scratch: &ScratchDir) -> (PathBuf, PathBuf) {
    let tree = scratch.path("tree-v1");
    write_fixture_tree(&tree, "1.0.0", EXECUTABLE_V1);
    let package = build_fixture_package(scratch, &tree, "fixture-v1.nlospkg");
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
    assert!(
        stdout_line(&install, "application").contains("generation 1"),
        "first install must land generation 1"
    );
    (state, package)
}

fn build_v2(scratch: &ScratchDir) -> PathBuf {
    let tree = scratch.path("tree-v2");
    write_fixture_tree(&tree, "1.0.1", EXECUTABLE_V2);
    build_fixture_package(scratch, &tree, "fixture-v2.nlospkg")
}

#[test]
fn update_cli_advances_generation_and_replays() {
    let scratch = ScratchDir::new("e2e");
    let (state, _v1) = install_v1(&scratch);
    let v2 = build_v2(&scratch);

    let update = run_cli(&[
        "update",
        &v2.to_string_lossy(),
        "--root",
        &state.to_string_lossy(),
    ]);
    assert_eq!(
        update.status.code(),
        Some(0),
        "update failed:\n{}",
        stderr_text(&update)
    );
    assert_eq!(stdout_line(&update, "VERIFIED").len(), 32);
    assert_eq!(stdout_line(&update, "decision"), "updated");
    assert_eq!(stdout_line(&update, "executables"), ENTRY_EXECUTABLE);
    let application_line = stdout_line(&update, "application");
    assert!(
        application_line.contains("generation 2"),
        "update must advance to generation 2: {application_line}"
    );
    let install_id = stdout_line(&update, "UPDATE");

    // Payload execution must see the new generation and new executable bytes.
    let runtime = SliceKRuntime::open(&state).expect("open CLI-written root");
    let execution =
        execute_application_payload(&runtime, PackageId::from_bytes(PACKAGE), ENTRY_EXECUTABLE)
            .expect("execute after update");
    assert_eq!(execution.installation_generation.get(), 2);
    assert_eq!(
        execution.payload_digest,
        ContentDigest::of_bytes(EXECUTABLE_V2)
    );
    let expected = nlos_driver_mock::derive_provider_outcome(
        execution.operation,
        execution.callback_id,
        &payload_execution_seed(EXECUTABLE_V2),
    );
    assert_eq!(execution.outcome, expected);

    // Re-update through the CLI replays the durable installation receipt.
    let again = run_cli(&[
        "update",
        &v2.to_string_lossy(),
        "--root",
        &state.to_string_lossy(),
    ]);
    assert_eq!(
        again.status.code(),
        Some(0),
        "update replay failed:\n{}",
        stderr_text(&again)
    );
    assert_eq!(stdout_line(&again, "decision"), "replayed");
    assert_eq!(stdout_line(&again, "UPDATE"), install_id);
    assert!(
        stdout_line(&again, "application").contains("generation 2"),
        "replay must not advance generation"
    );
}

#[test]
fn update_negative_gates_stay_typed() {
    let scratch = ScratchDir::new("negative");
    let v2 = {
        let tree = scratch.path("tree-v2");
        write_fixture_tree(&tree, "1.0.1", EXECUTABLE_V2);
        build_fixture_package(&scratch, &tree, "fixture-v2.nlospkg")
    };

    // Missing --root is a usage error.
    let no_root = run_cli(&["update", &v2.to_string_lossy()]);
    assert_eq!(no_root.status.code(), Some(EXIT_USAGE));

    // Update against an empty root (no prior install) is a typed authority refusal.
    let empty = scratch.path("empty-state");
    let unknown = run_cli(&[
        "update",
        &v2.to_string_lossy(),
        "--root",
        &empty.to_string_lossy(),
    ]);
    assert_eq!(
        unknown.status.code(),
        Some(EXIT_AUTHORITY),
        "update without install must be a typed authority refusal:\n{}",
        stderr_text(&unknown)
    );
    assert!(
        stderr_text(&unknown).contains("not found")
            || stderr_text(&unknown).contains("no application"),
        "expected ApplicationNotFound-style message:\n{}",
        stderr_text(&unknown)
    );

    // Same-content update after install is a typed UpdateManifestUnchanged refusal.
    let (state, v1) = install_v1(&scratch);
    let unchanged = run_cli(&[
        "update",
        &v1.to_string_lossy(),
        "--root",
        &state.to_string_lossy(),
    ]);
    assert_eq!(
        unchanged.status.code(),
        Some(EXIT_AUTHORITY),
        "same-manifest update must be a typed refusal:\n{}",
        stderr_text(&unchanged)
    );
    assert!(
        stderr_text(&unchanged).contains("unchanged")
            || stderr_text(&unchanged).contains("manifest"),
        "expected UpdateManifestUnchanged-style message:\n{}",
        stderr_text(&unchanged)
    );
}
