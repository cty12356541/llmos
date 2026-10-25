//! W65-CAPP-PAYLOAD (Stage C handover `#2 C-APP-PAYLOAD` end-to-end closure):
//! cross-platform CLI integration suite driving `env!("CARGO_BIN_EXE_nlos-package")`
//! over the real `examples/sample-app/package` (`v1.0.0`) and
//! `examples/sample-app/package-v1.1.0` (`v1.1.0`) third-party developer trees.
//!
//! Covered invariants:
//! 1. Full third-party CLI lifecycle (`keygen -> build v1.0.0 -> conformance ->
//!    verify -> install -> run sample-driver [gen 1, app.bin] -> build v1.1.0 ->
//!    conformance -> update --compat same-major -> run sample-driver [gen 2,
//!    updated app.bin] -> uninstall -> post-uninstall run refusal exit 7`);
//! 2. Non-executable / raw-path asset & unmaterialized entry refusals
//!    (`nlos-package run <pkg_id> assets/config.bin` and `app.bin` fail closed
//!    with binding exit code `4`, and unmaterialized `HeadState::Empty` entry
//!    artifacts fail closed with run exit code `7`, leaving zero operation rows);
//! 3. Tampered package payload rejection (`exit code 4`, zero tampered payload
//!    bytes written to `--root`) and incompatible version window rejections
//!    (`1.0.0 -> 1.1.0` under `--compat same-minor` and `1.1.0 -> 2.0.0` under
//!    `--compat same-major` both fail closed with `exit code 7`);
//! 4. Deterministic byte-reproducible builds and idempotent CLI replay across
//!    `verify`, `install`, `run`, `update`, and `uninstall`.

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use nlos_application::{ApplicationStatus, pack_package_version};
use nlos_artifact::{ContentDigest, CreateArtifactSpec, derive_artifact_id};
use nlos_slice_k::{
    SliceKRuntime, execute_application_payload, payload_execution_seed, seeded_key,
};
use nlos_types::{ArtifactId, PackageId};

const EXIT_OK: i32 = 0;
const EXIT_BINDING: i32 = 4;
const EXIT_AUTHORITY_OR_RUN: i32 = 7;

/// Package identity declared in `examples/sample-app/package/package.manifest`.
const SAMPLE_APP_PACKAGE_HEX: &str = "9a1c7d3e5f0b2a4c6d8e0f1a2b3c4d5e";
const SAMPLE_APP_PACKAGE_BYTES: [u8; 16] = [
    0x9a, 0x1c, 0x7d, 0x3e, 0x5f, 0x0b, 0x2a, 0x4c, 0x6d, 0x8e, 0x0f, 0x1a, 0x2b, 0x3c, 0x4d, 0x5e,
];
/// Manifest-declared executable entry (`entry = sample-driver executable app.bin`).
const SAMPLE_DRIVER_ENTRY: &str = "sample-driver";
/// Sample developer key seed (mirrors `examples/sample-app/lifecycle.sh`).
const SAMPLE_KEY_SEED: &str = "5a11e0b7c3d9f1426a8b0c5d7e93f618a2b4c6d8e0f2a4b6c8d0e2f4a6b8c0d1";

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(1);

struct TempRoot {
    root: PathBuf,
}

impl TempRoot {
    fn new(tag: &str) -> Self {
        let seq = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "nlos-package-sample-app-cli-{tag}-{}-{seq}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create per-test TempRoot");
        Self { root }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        for attempt in 0..5 {
            match fs::remove_dir_all(&self.root) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound && attempt < 4 => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                Ok(()) | Err(_) => return,
            }
        }
    }
}

fn sample_app_source_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/sample-app")
        .canonicalize()
        .expect("canonicalize examples/sample-app")
}

fn copy_dir_recursive(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).expect("create destination dir");
    for entry in fs::read_dir(source).expect("read source dir") {
        let entry = entry.expect("dir entry");
        let file_type = entry.file_type().expect("file type");
        let target = destination.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_recursive(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("copy tree file");
        }
    }
}

fn run_cli(arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_nlos-package"))
        .args(arguments)
        .output()
        .expect("spawn nlos-package binary")
}

fn stdout_text(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("utf-8 stdout")
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("utf-8 stderr")
}

fn stdout_line(output: &Output, prefix: &str) -> String {
    let stdout = stdout_text(output);
    stdout
        .lines()
        .find_map(|line| line.strip_prefix(prefix))
        .unwrap_or_else(|| panic!("missing {prefix:?} line in stdout:\n{stdout}"))
        .trim()
        .to_string()
}

fn hex_of(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

fn collect_files(directory: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

fn stage_sample_trees_and_key(temp: &TempRoot) -> (PathBuf, PathBuf, PathBuf) {
    let sample_root = sample_app_source_root();
    let tree_v1 = temp.path("sample-app-v1.0.0");
    let tree_v1_1 = temp.path("sample-app-v1.1.0");
    copy_dir_recursive(&sample_root.join("package"), &tree_v1);
    copy_dir_recursive(&sample_root.join("package-v1.1.0"), &tree_v1_1);

    let key_path = temp.path("sample-app.devkey");
    let keygen = run_cli(&[
        "keygen",
        "--seed",
        SAMPLE_KEY_SEED,
        "--out",
        &key_path.to_string_lossy(),
    ]);
    assert_eq!(
        keygen.status.code(),
        Some(EXIT_OK),
        "keygen failed:\n{}",
        stderr_text(&keygen)
    );
    assert_eq!(stdout_line(&keygen, "principal").len(), 32);
    assert_eq!(stdout_line(&keygen, "public_key").len(), 64);
    (tree_v1, tree_v1_1, key_path)
}

#[test]
#[allow(clippy::too_many_lines)]
fn full_sample_app_cli_lifecycle_executes_v1_and_v1_1_payloads_and_refuses_after_uninstall() {
    let temp = TempRoot::new("full-lifecycle");
    let (tree_v1, tree_v1_1, key_path) = stage_sample_trees_and_key(&temp);
    let v1_app_bytes = fs::read(tree_v1.join("app.bin")).expect("read v1.0.0 app.bin");
    let v1_1_app_bytes = fs::read(tree_v1_1.join("app.bin")).expect("read v1.1.0 app.bin");
    assert_ne!(v1_app_bytes, v1_1_app_bytes);

    // 1. Build + conformance + verify v1.0.0.
    let pkg_v1 = temp.path("sample-app-v1.0.0.nlospkg");
    let build_v1 = run_cli(&[
        "build",
        &tree_v1.to_string_lossy(),
        "--key",
        &key_path.to_string_lossy(),
        "--out",
        &pkg_v1.to_string_lossy(),
    ]);
    assert_eq!(
        build_v1.status.code(),
        Some(EXIT_OK),
        "build v1.0.0 failed:\n{}",
        stderr_text(&build_v1)
    );
    assert_eq!(
        stdout_line(&build_v1, "package"),
        format!(
            "{SAMPLE_APP_PACKAGE_HEX} version {}",
            pack_package_version(1, 0, 0)
        )
    );
    assert_eq!(stdout_line(&build_v1, "entries"), "2 tasks 2");
    let v1_manifest_digest = stdout_line(&build_v1, "manifest_digest");

    let conf_v1 = run_cli(&["conformance", &pkg_v1.to_string_lossy()]);
    assert_eq!(
        conf_v1.status.code(),
        Some(EXIT_OK),
        "conformance v1.0.0 failed:\n{}{}",
        stdout_text(&conf_v1),
        stderr_text(&conf_v1)
    );
    assert!(stdout_text(&conf_v1).contains("CONFORMANT"));

    let verify_store = temp.path("verify-store");
    let verify_identity = temp.path("verify-identity");
    let verify_v1 = run_cli(&[
        "verify",
        &pkg_v1.to_string_lossy(),
        "--store",
        &verify_store.to_string_lossy(),
        "--identity",
        &verify_identity.to_string_lossy(),
    ]);
    assert_eq!(
        verify_v1.status.code(),
        Some(EXIT_OK),
        "verify v1.0.0 failed:\n{}",
        stderr_text(&verify_v1)
    );
    let receipt_v1 = stdout_line(&verify_v1, "VERIFIED");
    assert_eq!(receipt_v1.len(), 32);
    assert_eq!(
        stdout_line(&verify_v1, "manifest_digest"),
        v1_manifest_digest
    );

    // 2. Install v1.0.0 into persistent state root and execute generation-1 payload.
    let state = temp.path("state");
    let install_v1 = run_cli(&[
        "install",
        &pkg_v1.to_string_lossy(),
        "--root",
        &state.to_string_lossy(),
    ]);
    assert_eq!(
        install_v1.status.code(),
        Some(EXIT_OK),
        "install v1.0.0 failed:\n{}",
        stderr_text(&install_v1)
    );
    assert_eq!(stdout_line(&install_v1, "VERIFIED"), receipt_v1);
    assert_eq!(stdout_line(&install_v1, "decision"), "installed");
    assert_eq!(stdout_line(&install_v1, "executables"), SAMPLE_DRIVER_ENTRY);
    let install_app_line = stdout_line(&install_v1, "application");
    assert!(install_app_line.contains("generation 1"));
    let application_hex = install_app_line
        .split_whitespace()
        .next()
        .expect("application hex")
        .to_string();

    let run_gen1 = run_cli(&[
        "run",
        SAMPLE_APP_PACKAGE_HEX,
        SAMPLE_DRIVER_ENTRY,
        "--root",
        &state.to_string_lossy(),
    ]);
    assert_eq!(
        run_gen1.status.code(),
        Some(EXIT_OK),
        "run generation 1 failed:\n{}",
        stderr_text(&run_gen1)
    );
    assert_eq!(stdout_line(&run_gen1, "RUN"), application_hex);
    assert_eq!(stdout_line(&run_gen1, "decision"), "executed");
    assert_eq!(stdout_line(&run_gen1, "entry"), SAMPLE_DRIVER_ENTRY);
    assert_eq!(stdout_line(&run_gen1, "package"), SAMPLE_APP_PACKAGE_HEX);
    assert_eq!(
        stdout_line(&run_gen1, "application"),
        format!(
            "{application_hex} generation 1 version {}",
            pack_package_version(1, 0, 0)
        )
    );
    let expected_v1_digest = hex_of(ContentDigest::of_bytes(&v1_app_bytes).as_bytes());
    let artifact_gen1 = stdout_line(&run_gen1, "artifact");
    assert!(
        artifact_gen1.contains(&format!(
            "revision 1 digest {expected_v1_digest} bytes {}",
            v1_app_bytes.len()
        )),
        "unexpected generation-1 artifact line: {artifact_gen1}"
    );
    let op_gen1 = stdout_line(&run_gen1, "operation");
    assert_eq!(op_gen1.len(), 32);

    {
        let runtime = SliceKRuntime::open(&state).expect("open state after gen 1 run");
        let replay = execute_application_payload(
            &runtime,
            PackageId::from_bytes(SAMPLE_APP_PACKAGE_BYTES),
            SAMPLE_DRIVER_ENTRY,
        )
        .expect("library replay of gen 1 execution");
        assert!(replay.register_replayed && replay.dispatch_replayed && replay.complete_replayed);
        assert!(replay.semantic_replayed);
        assert_eq!(replay.installation_generation.get(), 1);
        assert_eq!(hex_of(replay.operation.operation_id.as_bytes()), op_gen1);
        let expected_outcome = nlos_driver_mock::derive_provider_outcome(
            replay.operation,
            replay.callback_id,
            &payload_execution_seed(&v1_app_bytes),
        );
        assert_eq!(replay.outcome, expected_outcome);
    }

    // 3. Build + conformance v1.1.0, update (`--compat same-major`), and execute generation-2 payload.
    let pkg_v1_1 = temp.path("sample-app-v1.1.0.nlospkg");
    let build_v1_1 = run_cli(&[
        "build",
        &tree_v1_1.to_string_lossy(),
        "--key",
        &key_path.to_string_lossy(),
        "--out",
        &pkg_v1_1.to_string_lossy(),
    ]);
    assert_eq!(
        build_v1_1.status.code(),
        Some(EXIT_OK),
        "build v1.1.0 failed:\n{}",
        stderr_text(&build_v1_1)
    );
    assert_eq!(stdout_line(&build_v1_1, "entries"), "2 tasks 2");
    assert_ne!(
        stdout_line(&build_v1_1, "manifest_digest"),
        v1_manifest_digest
    );

    let conf_v1_1 = run_cli(&["conformance", &pkg_v1_1.to_string_lossy()]);
    assert_eq!(conf_v1_1.status.code(), Some(EXIT_OK));

    let update_v1_1 = run_cli(&[
        "update",
        &pkg_v1_1.to_string_lossy(),
        "--root",
        &state.to_string_lossy(),
        "--compat",
        "same-major",
    ]);
    assert_eq!(
        update_v1_1.status.code(),
        Some(EXIT_OK),
        "update v1.1.0 failed:\n{}",
        stderr_text(&update_v1_1)
    );
    assert_eq!(stdout_line(&update_v1_1, "decision"), "updated");
    assert_eq!(stdout_line(&update_v1_1, "compat"), "same-major");
    assert_eq!(
        stdout_line(&update_v1_1, "executables"),
        SAMPLE_DRIVER_ENTRY
    );
    assert!(stdout_line(&update_v1_1, "application").contains("generation 2"));

    let run_gen2 = run_cli(&[
        "run",
        SAMPLE_APP_PACKAGE_HEX,
        SAMPLE_DRIVER_ENTRY,
        "--root",
        &state.to_string_lossy(),
    ]);
    assert_eq!(
        run_gen2.status.code(),
        Some(EXIT_OK),
        "run generation 2 failed:\n{}",
        stderr_text(&run_gen2)
    );
    assert_eq!(stdout_line(&run_gen2, "decision"), "executed");
    assert_eq!(
        stdout_line(&run_gen2, "application"),
        format!(
            "{application_hex} generation 2 version {}",
            pack_package_version(1, 1, 0)
        )
    );
    let expected_v1_1_digest = hex_of(ContentDigest::of_bytes(&v1_1_app_bytes).as_bytes());
    let artifact_gen2 = stdout_line(&run_gen2, "artifact");
    assert!(
        artifact_gen2.contains(&format!(
            "revision 1 digest {expected_v1_1_digest} bytes {}",
            v1_1_app_bytes.len()
        )),
        "unexpected generation-2 artifact line: {artifact_gen2}"
    );
    let op_gen2 = stdout_line(&run_gen2, "operation");
    assert_ne!(
        op_gen2, op_gen1,
        "generation 2 must derive a fresh operation identity"
    );

    {
        let runtime = SliceKRuntime::open(&state).expect("open state after gen 2 run");
        let replay = execute_application_payload(
            &runtime,
            PackageId::from_bytes(SAMPLE_APP_PACKAGE_BYTES),
            SAMPLE_DRIVER_ENTRY,
        )
        .expect("library replay of gen 2 execution");
        assert_eq!(replay.installation_generation.get(), 2);
        let expected_outcome = nlos_driver_mock::derive_provider_outcome(
            replay.operation,
            replay.callback_id,
            &payload_execution_seed(&v1_1_app_bytes),
        );
        assert_eq!(replay.outcome, expected_outcome);
    }

    // 4. Uninstall and verify post-uninstall `run` fails closed with exit code 7.
    let uninstall = run_cli(&[
        "uninstall",
        SAMPLE_APP_PACKAGE_HEX,
        "--root",
        &state.to_string_lossy(),
    ]);
    assert_eq!(
        uninstall.status.code(),
        Some(EXIT_OK),
        "uninstall failed:\n{}",
        stderr_text(&uninstall)
    );
    assert_eq!(stdout_line(&uninstall, "UNINSTALL"), application_hex);
    assert_eq!(stdout_line(&uninstall, "decision"), "uninstalled");
    assert!(stdout_line(&uninstall, "application").contains("generation 2"));

    {
        let runtime = SliceKRuntime::open(&state).expect("open state after uninstall");
        let view = runtime
            .applications
            .inspect_application(PackageId::from_bytes(SAMPLE_APP_PACKAGE_BYTES))
            .expect("inspect application")
            .expect("application row remains durable");
        assert_eq!(view.status, ApplicationStatus::Uninstalled);
    }

    let post_uninstall_run = run_cli(&[
        "run",
        SAMPLE_APP_PACKAGE_HEX,
        SAMPLE_DRIVER_ENTRY,
        "--root",
        &state.to_string_lossy(),
    ]);
    assert_eq!(
        post_uninstall_run.status.code(),
        Some(EXIT_AUTHORITY_OR_RUN),
        "post-uninstall run must fail closed with exit 7:\n{}",
        stderr_text(&post_uninstall_run)
    );
    assert!(
        stderr_text(&post_uninstall_run).contains("not installed"),
        "unexpected stderr:\n{}",
        stderr_text(&post_uninstall_run)
    );
}

#[test]
fn non_executable_asset_paths_and_unmaterialized_entries_fail_closed() {
    let temp = TempRoot::new("non-exec-refusal");
    let (tree_v1, _tree_v1_1, key_path) = stage_sample_trees_and_key(&temp);
    let pkg_v1 = temp.path("sample-app-v1.0.0.nlospkg");
    let build = run_cli(&[
        "build",
        &tree_v1.to_string_lossy(),
        "--key",
        &key_path.to_string_lossy(),
        "--out",
        &pkg_v1.to_string_lossy(),
    ]);
    assert_eq!(build.status.code(), Some(EXIT_OK));

    let state = temp.path("state");
    let install = run_cli(&[
        "install",
        &pkg_v1.to_string_lossy(),
        "--root",
        &state.to_string_lossy(),
    ]);
    assert_eq!(install.status.code(), Some(EXIT_OK));
    assert_eq!(stdout_line(&install, "executables"), SAMPLE_DRIVER_ENTRY);

    // Raw asset paths (`assets/config.bin`, `app.bin`) and undeclared logical names
    // resolve via `derive_artifact_id(package_id, version, entry)` to an unknown
    // artifact id and fail closed with exit code 4 (`ToolError::Binding`).
    for invalid_entry in ["assets/config.bin", "app.bin", "missing-entry"] {
        let refused = run_cli(&[
            "run",
            SAMPLE_APP_PACKAGE_HEX,
            invalid_entry,
            "--root",
            &state.to_string_lossy(),
        ]);
        assert_eq!(
            refused.status.code(),
            Some(EXIT_BINDING),
            "entry {invalid_entry:?} must fail closed with exit 4:\n{}",
            stderr_text(&refused)
        );
        assert!(
            stderr_text(&refused).contains("entry artifact unknown:"),
            "unexpected refusal message for {invalid_entry:?}:\n{}",
            stderr_text(&refused)
        );
    }

    // An existing artifact record with `HeadState::Empty` (no materialized revision)
    // fails closed with exit code 7 (`ToolError::Run` / `SliceKError::PayloadState`),
    // and zero operations exist in the durable operation store.
    {
        let runtime = SliceKRuntime::open(&state).expect("open state root");
        let empty_entry = "unmaterialized-entry";
        let empty_artifact = ArtifactId::from_bytes(derive_artifact_id(
            PackageId::from_bytes(SAMPLE_APP_PACKAGE_BYTES),
            pack_package_version(1, 0, 0),
            empty_entry,
        ));
        let created_at_ms = runtime
            .wall_now_ms(seeded_key(0x77, 1))
            .expect("clock reading");
        runtime
            .artifacts
            .create_artifact(CreateArtifactSpec {
                artifact_id: empty_artifact,
                idempotency_key: seeded_key(0x77, 2),
                content_type: "application/octet-stream".to_string(),
                application_id: None,
                owner: None,
                created_at_ms,
            })
            .expect("create empty artifact");
    }
    let empty_refused = run_cli(&[
        "run",
        SAMPLE_APP_PACKAGE_HEX,
        "unmaterialized-entry",
        "--root",
        &state.to_string_lossy(),
    ]);
    assert_eq!(
        empty_refused.status.code(),
        Some(EXIT_AUTHORITY_OR_RUN),
        "empty artifact revision must fail closed with exit 7:\n{}",
        stderr_text(&empty_refused)
    );
    assert!(
        stderr_text(&empty_refused).contains("payload execution state refusal"),
        "unexpected stderr:\n{}",
        stderr_text(&empty_refused)
    );
}

#[test]
#[allow(clippy::too_many_lines)]
fn tampered_payload_exits_four_and_incompatible_version_windows_exit_seven() {
    let temp = TempRoot::new("tamper-and-compat");
    let (tree_v1, tree_v1_1, key_path) = stage_sample_trees_and_key(&temp);

    let pkg_v1 = temp.path("sample-app-v1.0.0.nlospkg");
    assert_eq!(
        run_cli(&[
            "build",
            &tree_v1.to_string_lossy(),
            "--key",
            &key_path.to_string_lossy(),
            "--out",
            &pkg_v1.to_string_lossy(),
        ])
        .status
        .code(),
        Some(EXIT_OK)
    );
    let pkg_v1_1 = temp.path("sample-app-v1.1.0.nlospkg");
    assert_eq!(
        run_cli(&[
            "build",
            &tree_v1_1.to_string_lossy(),
            "--key",
            &key_path.to_string_lossy(),
            "--out",
            &pkg_v1_1.to_string_lossy(),
        ])
        .status
        .code(),
        Some(EXIT_OK)
    );

    // 1) Tamper `app.bin` payload bytes inside the signed `v1.0.0` package file -> exit code 4,
    //    leaving zero tampered bytes in `tampered-state`.
    let v1_app_bytes = fs::read(tree_v1.join("app.bin")).expect("read v1 app.bin");
    let mut package_bytes = fs::read(&pkg_v1).expect("read v1 package");
    let matches: Vec<usize> = package_bytes
        .windows(v1_app_bytes.len())
        .enumerate()
        .filter(|(_, window)| *window == v1_app_bytes.as_slice())
        .map(|(idx, _)| idx)
        .collect();
    assert_eq!(
        matches.len(),
        1,
        "v1 app.bin bytes must appear once in package"
    );
    package_bytes[matches[0]] ^= 0xff;
    let tampered_pkg = temp.path("tampered-v1.nlospkg");
    fs::write(&tampered_pkg, &package_bytes).expect("write tampered package");

    let tampered_state = temp.path("tampered-state");
    let tampered_install = run_cli(&[
        "install",
        &tampered_pkg.to_string_lossy(),
        "--root",
        &tampered_state.to_string_lossy(),
    ]);
    assert_eq!(
        tampered_install.status.code(),
        Some(EXIT_BINDING),
        "tampered payload install must fail with exit 4:\n{}",
        stderr_text(&tampered_install)
    );

    let mut mutated_app = v1_app_bytes;
    mutated_app[0] ^= 0xff;
    let mut state_files = Vec::new();
    collect_files(&tampered_state, &mut state_files);
    for file in state_files {
        if let Ok(bytes) = fs::read(&file) {
            assert!(
                !bytes
                    .windows(mutated_app.len())
                    .any(|window| window == mutated_app.as_slice()),
                "tampered payload bytes leaked into {}",
                file.display()
            );
        }
    }

    // 2) Incompatible `same-minor` window (`1.0.0 -> 1.1.0`) -> exit code 7.
    let compat_state = temp.path("compat-state");
    assert_eq!(
        run_cli(&[
            "install",
            &pkg_v1.to_string_lossy(),
            "--root",
            &compat_state.to_string_lossy(),
        ])
        .status
        .code(),
        Some(EXIT_OK)
    );

    let minor_refused = run_cli(&[
        "update",
        &pkg_v1_1.to_string_lossy(),
        "--root",
        &compat_state.to_string_lossy(),
        "--compat",
        "same-minor",
    ]);
    assert_eq!(
        minor_refused.status.code(),
        Some(EXIT_AUTHORITY_OR_RUN),
        "1.0.0 -> 1.1.0 under same-minor must fail with exit 7:\n{}",
        stderr_text(&minor_refused)
    );

    // 3) Incompatible `same-major` window (`1.0.0 -> 2.0.0`) -> exit code 7.
    let tree_v2 = temp.path("sample-app-v2.0.0");
    copy_dir_recursive(&tree_v1_1, &tree_v2);
    let manifest_v2 = fs::read_to_string(tree_v2.join("package.manifest"))
        .expect("read v1.1.0 manifest")
        .replace("version = 1.1.0", "version = 2.0.0");
    fs::write(tree_v2.join("package.manifest"), manifest_v2).expect("write v2.0.0 manifest");
    fs::write(
        tree_v2.join("app.bin"),
        b"sample-app driver entry v2.0.0 breaking major\n",
    )
    .expect("write v2.0.0 app.bin");

    let pkg_v2 = temp.path("sample-app-v2.0.0.nlospkg");
    assert_eq!(
        run_cli(&[
            "build",
            &tree_v2.to_string_lossy(),
            "--key",
            &key_path.to_string_lossy(),
            "--out",
            &pkg_v2.to_string_lossy(),
        ])
        .status
        .code(),
        Some(EXIT_OK)
    );

    let major_refused = run_cli(&[
        "update",
        &pkg_v2.to_string_lossy(),
        "--root",
        &compat_state.to_string_lossy(),
        "--compat",
        "same-major",
    ]);
    assert_eq!(
        major_refused.status.code(),
        Some(EXIT_AUTHORITY_OR_RUN),
        "1.0.0 -> 2.0.0 under same-major must fail with exit 7:\n{}",
        stderr_text(&major_refused)
    );

    // Both refused updates preserved generation 1 intact.
    let runtime = SliceKRuntime::open(&compat_state).expect("open compat_state");
    let app = runtime
        .applications
        .inspect_application(PackageId::from_bytes(SAMPLE_APP_PACKAGE_BYTES))
        .expect("inspect application")
        .expect("installed application");
    assert_eq!(app.current_installation_generation.get(), 1);
}

#[test]
#[allow(clippy::too_many_lines)]
fn deterministic_builds_and_idempotent_cli_replays_across_all_lifecycle_verbs() {
    let temp = TempRoot::new("determinism-replay");
    let (tree_v1, tree_v1_1, key_path) = stage_sample_trees_and_key(&temp);

    // Byte-reproducible `build` for identical developer trees and signing key.
    let pkg_a = temp.path("build-a.nlospkg");
    let pkg_b = temp.path("build-b.nlospkg");
    let build_a = run_cli(&[
        "build",
        &tree_v1.to_string_lossy(),
        "--key",
        &key_path.to_string_lossy(),
        "--out",
        &pkg_a.to_string_lossy(),
    ]);
    let build_b = run_cli(&[
        "build",
        &tree_v1.to_string_lossy(),
        "--key",
        &key_path.to_string_lossy(),
        "--out",
        &pkg_b.to_string_lossy(),
    ]);
    assert_eq!(build_a.status.code(), Some(EXIT_OK));
    assert_eq!(build_b.status.code(), Some(EXIT_OK));
    assert_eq!(
        fs::read(&pkg_a).expect("read pkg_a"),
        fs::read(&pkg_b).expect("read pkg_b"),
        "repeated builds of sample-app must be byte-identical"
    );
    assert_eq!(
        stdout_line(&build_a, "manifest_digest"),
        stdout_line(&build_b, "manifest_digest")
    );

    // Persistent `verify` replays the identical receipt id.
    let v_store = temp.path("v-store");
    let v_id = temp.path("v-id");
    let verify_1 = run_cli(&[
        "verify",
        &pkg_a.to_string_lossy(),
        "--store",
        &v_store.to_string_lossy(),
        "--identity",
        &v_id.to_string_lossy(),
    ]);
    let verify_2 = run_cli(&[
        "verify",
        &pkg_b.to_string_lossy(),
        "--store",
        &v_store.to_string_lossy(),
        "--identity",
        &v_id.to_string_lossy(),
    ]);
    assert_eq!(verify_1.status.code(), Some(EXIT_OK));
    assert_eq!(verify_2.status.code(), Some(EXIT_OK));
    assert_eq!(
        stdout_line(&verify_1, "VERIFIED"),
        stdout_line(&verify_2, "REPLAYED")
    );

    // `install`, `run`, `update`, and `uninstall` each replay deterministically.
    let state = temp.path("state");
    let install_1 = run_cli(&[
        "install",
        &pkg_a.to_string_lossy(),
        "--root",
        &state.to_string_lossy(),
    ]);
    let install_2 = run_cli(&[
        "install",
        &pkg_b.to_string_lossy(),
        "--root",
        &state.to_string_lossy(),
    ]);
    assert_eq!(stdout_line(&install_1, "decision"), "installed");
    assert_eq!(stdout_line(&install_2, "decision"), "replayed");
    assert_eq!(
        stdout_line(&install_1, "INSTALL"),
        stdout_line(&install_2, "INSTALL")
    );

    let run_1 = run_cli(&[
        "run",
        SAMPLE_APP_PACKAGE_HEX,
        SAMPLE_DRIVER_ENTRY,
        "--root",
        &state.to_string_lossy(),
    ]);
    let run_2 = run_cli(&[
        "run",
        SAMPLE_APP_PACKAGE_HEX,
        SAMPLE_DRIVER_ENTRY,
        "--root",
        &state.to_string_lossy(),
    ]);
    assert_eq!(stdout_line(&run_1, "decision"), "executed");
    assert_eq!(stdout_line(&run_2, "decision"), "replayed");
    assert_eq!(
        stdout_line(&run_1, "operation"),
        stdout_line(&run_2, "operation")
    );
    assert_eq!(
        stdout_line(&run_1, "receipts"),
        stdout_line(&run_2, "receipts")
    );
    assert_eq!(
        stdout_line(&run_1, "outcome"),
        stdout_line(&run_2, "outcome")
    );

    let pkg_v1_1 = temp.path("sample-app-v1.1.0.nlospkg");
    assert_eq!(
        run_cli(&[
            "build",
            &tree_v1_1.to_string_lossy(),
            "--key",
            &key_path.to_string_lossy(),
            "--out",
            &pkg_v1_1.to_string_lossy(),
        ])
        .status
        .code(),
        Some(EXIT_OK)
    );
    let update_1 = run_cli(&[
        "update",
        &pkg_v1_1.to_string_lossy(),
        "--root",
        &state.to_string_lossy(),
        "--compat",
        "same-major",
    ]);
    let update_2 = run_cli(&[
        "update",
        &pkg_v1_1.to_string_lossy(),
        "--root",
        &state.to_string_lossy(),
        "--compat",
        "same-major",
    ]);
    assert_eq!(stdout_line(&update_1, "decision"), "updated");
    assert_eq!(stdout_line(&update_2, "decision"), "replayed");
    assert_eq!(
        stdout_line(&update_1, "UPDATE"),
        stdout_line(&update_2, "UPDATE")
    );

    let uninstall_1 = run_cli(&[
        "uninstall",
        SAMPLE_APP_PACKAGE_HEX,
        "--root",
        &state.to_string_lossy(),
    ]);
    let uninstall_2 = run_cli(&[
        "uninstall",
        SAMPLE_APP_PACKAGE_HEX,
        "--root",
        &state.to_string_lossy(),
    ]);
    assert_eq!(stdout_line(&uninstall_1, "decision"), "uninstalled");
    assert_eq!(stdout_line(&uninstall_2, "decision"), "replayed");
    assert_eq!(
        stdout_line(&uninstall_1, "UNINSTALL"),
        stdout_line(&uninstall_2, "UNINSTALL")
    );
}
