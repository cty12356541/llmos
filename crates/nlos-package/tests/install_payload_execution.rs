//! W35-P2 (handover #2 first slice, `C-APP-PAYLOAD`): the end-to-end
//! consumer chain, headless —
//!
//! ```text
//! fixture developer tree
//!   → nlos-package keygen / build (real CLI binary)
//!   → nlos-package install <pkg> --root <state>   (W33-H §2 boundary 3)
//!   → kernel payload-execution lane over the driver face (boundary 1)
//!   → durable receipts inspectable
//! ```
//!
//! The install step is the real CLI process driving the public
//! application-authority path (verify → materialize → receipt
//! digest-binding → install-scoped orphan GC → `install_application`);
//! the execution step is the slice-k lane over `nlos-driver-mock`
//! (`register → dispatch → complete` with the payload bytes as the
//! completion seed). Every assertion reads durable state or CLI exit
//! codes/lines — nothing is asserted from in-process side channels of
//! the CLI run.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicU64, Ordering};

use nlos_artifact::ContentDigest;
use nlos_slice_k::{SliceKRuntime, execute_application_payload, payload_execution_seed};
use nlos_types::PackageId;

const EXIT_USAGE: i32 = 1;
const EXIT_BINDING: i32 = 4;

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
            "nlos-package-install-payload-{name}-{}-{sequence}",
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
            "# install-payload e2e fixture\n\
             package-id = {}\n\
             version = 1.0.0\n\
             entry = {ENTRY_EXECUTABLE} executable hello.bin\n\
             entry = config data assets/config.bin\n",
            hex_32(&PACKAGE),
        ),
    )
    .expect("write manifest");
    fs::write(dir.join("hello.bin"), EXECUTABLE_PAYLOAD).expect("write hello.bin");
    fs::write(dir.join("assets/config.bin"), b"config-bytes-install-e2e").expect("write config");
}

fn hex_32(bytes: &[u8; 16]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// keygen + build through the real CLI, returning the package file path.
fn build_fixture_package(scratch: &ScratchDir, tree: &Path) -> PathBuf {
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
    assert_eq!(
        stdout_line(&build, "entries"),
        "2 tasks 0",
        "fixture must carry one executable and one data entry"
    );
    package
}

#[test]
fn install_cli_then_payload_executes_through_driver_face_with_inspectable_receipts() {
    let scratch = ScratchDir::new("e2e");
    let tree = scratch.path("tree");
    write_fixture_tree(&tree);
    let package = build_fixture_package(&scratch, &tree);
    let state = scratch.path("state");

    // ---- install through the real CLI (W33-H §2 boundary 3) ----
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
    assert_eq!(stdout_line(&install, "VERIFIED").len(), 32);
    assert_eq!(stdout_line(&install, "decision"), "installed");
    assert_eq!(
        stdout_line(&install, "executables"),
        ENTRY_EXECUTABLE,
        "the manifest-declared executable entry must be reported"
    );
    let application_line = stdout_line(&install, "application");
    let application_hex = application_line
        .split_whitespace()
        .next()
        .expect("application id");
    assert_eq!(application_hex.len(), 32);
    assert!(
        application_line.contains("generation 1"),
        "first install must land generation 1: {application_line}"
    );

    // Reinstall through the CLI replays the durable installation receipt.
    let replayed = run_cli(&[
        "install",
        &package.to_string_lossy(),
        "--root",
        &state.to_string_lossy(),
    ]);
    assert_eq!(replayed.status.code(), Some(0));
    assert_eq!(stdout_line(&replayed, "decision"), "replayed");
    assert_eq!(
        stdout_line(&replayed, "INSTALL"),
        stdout_line(&install, "INSTALL")
    );

    // ---- execute the installed executable payload through the driver
    // face (W33-H §2 boundary 1, kernel-side lane) ----
    let runtime = SliceKRuntime::open(&state).expect("open the CLI-written state root");
    let execution =
        execute_application_payload(&runtime, PackageId::from_bytes(PACKAGE), ENTRY_EXECUTABLE)
            .expect("execute the installed executable entry");

    assert_eq!(execution.entry_name, ENTRY_EXECUTABLE);
    assert_eq!(execution.installation_generation.get(), 1);
    assert_eq!(
        execution.payload_digest,
        ContentDigest::of_bytes(EXECUTABLE_PAYLOAD)
    );
    assert_eq!(
        execution.payload_size_bytes,
        u64::try_from(EXECUTABLE_PAYLOAD.len()).expect("size fits")
    );
    assert!(!execution.register_replayed);
    assert!(!execution.dispatch_replayed);
    assert!(!execution.complete_replayed);

    // The payload bytes were consumed by the driver operation: the
    // terminal outcome is the deterministic provider outcome under the
    // payload-derived seed, recomputed here through the public face.
    let expected = nlos_driver_mock::derive_provider_outcome(
        execution.operation,
        execution.callback_id,
        &payload_execution_seed(EXECUTABLE_PAYLOAD),
    );
    assert_eq!(execution.outcome, expected);

    // The terminal state is durable on the same operation authority the
    // runtime's fiber lane uses, and stays inspectable afterwards.
    let snapshot = runtime
        .operations
        .inspect(execution.operation)
        .expect("inspect the executed operation");
    assert_eq!(snapshot.state, execution.terminal_state);
    assert!(snapshot.state.is_terminal());

    // Re-execution replays every receipt exactly.
    let again =
        execute_application_payload(&runtime, PackageId::from_bytes(PACKAGE), ENTRY_EXECUTABLE)
            .expect("re-execute");
    assert!(again.register_replayed);
    assert!(again.dispatch_replayed);
    assert!(again.complete_replayed);
    assert_eq!(again.outcome, execution.outcome);
    assert_eq!(again.activation_receipt_id, execution.activation_receipt_id);

    let report = execution.report_lines().join("\n");
    assert!(report.contains(ENTRY_EXECUTABLE), "report:\n{report}");
    assert!(report.contains("receipts admission="), "report:\n{report}");
}

#[test]
fn install_negative_gates_stay_typed() {
    let scratch = ScratchDir::new("negative");
    let tree = scratch.path("tree");
    write_fixture_tree(&tree);
    let package = build_fixture_package(&scratch, &tree);
    let state = scratch.path("state");

    // Missing --root is a usage error.
    let no_root = run_cli(&["install", &package.to_string_lossy()]);
    assert_eq!(no_root.status.code(), Some(EXIT_USAGE));

    // A tampered payload fails the content-binding gate inside install.
    let mut bytes = fs::read(&package).expect("read package");
    let needle = b"EXEC-PAYLOAD-NEEDLE-3e7b91";
    let positions: Vec<usize> = bytes
        .windows(needle.len())
        .enumerate()
        .filter(|(_, window)| *window == needle)
        .map(|(index, _)| index)
        .collect();
    assert_eq!(positions.len(), 1, "needle must occur exactly once");
    bytes[positions[0]] ^= 0xff;
    let tampered = scratch.path("tampered.nlospkg");
    fs::write(&tampered, bytes).expect("write tampered");
    let refused = run_cli(&[
        "install",
        &tampered.to_string_lossy(),
        "--root",
        &state.to_string_lossy(),
    ]);
    assert_eq!(
        refused.status.code(),
        Some(EXIT_BINDING),
        "tampered payload must be a typed binding failure:\n{}",
        stderr_text(&refused)
    );

    // Nothing was installed by the refused attempt: the kernel lane
    // refuses the package identity with no durable application row.
    let runtime = SliceKRuntime::open(&state).expect("open state root");
    let refusal =
        execute_application_payload(&runtime, PackageId::from_bytes(PACKAGE), ENTRY_EXECUTABLE)
            .expect_err("uninstalled package must refuse execution");
    assert!(
        format!("{refusal}").contains("no application is installed"),
        "{refusal}"
    );
}
