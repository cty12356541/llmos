//! W33-A Package SDK CLI: `nlos-package build/verify/keygen` round-trip
//! against the real binary (`CARGO_BIN_EXE`), mirroring the
//! `control_command_cli.rs` pattern.
//!
//! Gates (进度单 §6.5.3 W33-A): a sample package can be built, and its
//! signature verified — including the W28-B task-template face end-to-end,
//! typed tamper failures, and byte-reproducible builds.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicU64, Ordering};

/// Typed CLI exit codes (contract documented in docs/developers/packaging.md).
const EXIT_USAGE: i32 = 1;
const EXIT_INPUT: i32 = 2;
const EXIT_SIGNATURE: i32 = 3;
const EXIT_BINDING: i32 = 4;

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct ScratchDir {
    root: PathBuf,
}

impl ScratchDir {
    fn new(name: &str) -> Self {
        let sequence = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "nlos-package-sdk-test-{name}-{}-{sequence}",
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

/// A needle that appears exactly once inside the built package file's
/// payload section, so flipping it is a payload tamper (binding failure),
/// not a framing break.
const PAYLOAD_NEEDLE: &[u8] = b"PAYLOAD-NEEDLE-7f3a91";

/// Writes one minimal developer tree: two data entries plus, when
/// `with_tasks`, a two-template `tasks` segment (second depends on first).
fn write_fixture_tree(dir: &Path, version: &str, with_tasks: bool) {
    fs::create_dir_all(dir.join("assets")).expect("assets dir");
    fs::write(
        dir.join("package.manifest"),
        developer_manifest(version, with_tasks),
    )
    .expect("write manifest");
    let mut payload = PAYLOAD_NEEDLE.to_vec();
    payload.extend_from_slice(&[0x5a; 97]);
    fs::write(dir.join("hello.bin"), payload).expect("write hello.bin");
    fs::write(dir.join("assets/config.bin"), b"config-bytes-42").expect("write config.bin");
    if with_tasks {
        fs::create_dir_all(dir.join("tasks")).expect("tasks dir");
        fs::write(dir.join("tasks/binding.txt"), b"binding-body").expect("binding");
        fs::write(dir.join("tasks/inputs.txt"), b"inputs-body").expect("inputs");
        fs::write(dir.join("tasks/outputs.txt"), b"outputs-body").expect("outputs");
        fs::write(dir.join("tasks/policy.txt"), b"policy-body").expect("policy");
        fs::write(dir.join("tasks/ceiling.txt"), b"ceiling-body").expect("ceiling");
    }
}

fn developer_manifest(version: &str, with_tasks: bool) -> String {
    let mut text = format!(
        "# nlos-package developer manifest v1\n\
         package-id = 0f1e2d3c4b5a69788796a5b4c3d2e1f0\n\
         version = {version}\n\
         entry = hello executable hello.bin\n\
         entry = config data assets/config.bin\n"
    );
    if with_tasks {
        text.push_str(
            "task = 0102030405060708090a0b0c0d0e0f10 agent-role \
             tasks/binding.txt tasks/inputs.txt tasks/outputs.txt \
             tasks/policy.txt tasks/ceiling.txt\n\
             task = 1112131415161718191a1b1c1d1e1f20 executable \
             tasks/binding.txt tasks/inputs.txt tasks/outputs.txt \
             tasks/policy.txt tasks/ceiling.txt \
             deps=0102030405060708090a0b0c0d0e0f10\n",
        );
    }
    text
}

fn keygen(scratch: &ScratchDir, seed_hex: &str) -> PathBuf {
    let key = scratch.path("sample.devkey");
    let output = run_cli(&[
        "keygen",
        "--seed",
        seed_hex,
        "--out",
        &key.to_string_lossy(),
    ]);
    assert_eq!(
        output.status.code(),
        Some(0),
        "keygen failed:\n{}",
        stderr_text(&output)
    );
    assert!(!stdout_line(&output, "principal").is_empty());
    key
}

fn build(tree: &Path, key: &Path, out: &Path) -> Output {
    run_cli(&[
        "build",
        &tree.to_string_lossy(),
        "--key",
        &key.to_string_lossy(),
        "--out",
        &out.to_string_lossy(),
    ])
}

fn verify(scratch: &ScratchDir, package: &Path) -> Output {
    run_cli(&[
        "verify",
        &package.to_string_lossy(),
        "--store",
        &scratch.path("store.d").to_string_lossy(),
        "--identity",
        &scratch.path("identity.d").to_string_lossy(),
    ])
}

#[test]
fn build_then_verify_round_trip_and_persistent_replay() {
    let scratch = ScratchDir::new("roundtrip");
    let key = keygen(&scratch, &"11".repeat(32));
    let tree = scratch.path("tree");
    write_fixture_tree(&tree, "7.3.1", false);
    let package = scratch.path("sample.nlospkg");

    let built = build(&tree, &key, &package);
    assert_eq!(
        built.status.code(),
        Some(0),
        "build failed:\n{}",
        stderr_text(&built)
    );
    let digest = stdout_line(&built, "manifest_digest");
    assert_eq!(digest.len(), 64, "digest must be sha-256 hex");
    assert_eq!(
        stdout_line(&built, "package"),
        "0f1e2d3c4b5a69788796a5b4c3d2e1f0 version 30064967681"
    );
    assert_eq!(stdout_line(&built, "entries"), "2 tasks 0");
    let signer = stdout_line(&built, "signer");

    // Fresh verification against a persistent store: VERIFIED.
    let verified = verify(&scratch, &package);
    assert_eq!(
        verified.status.code(),
        Some(0),
        "verify failed:\n{}",
        stderr_text(&verified)
    );
    assert_eq!(stdout_line(&verified, "manifest_digest"), digest);
    let verified_signer = stdout_line(&verified, "signer");
    assert_eq!(
        verified_signer.split_whitespace().next().expect("signer"),
        signer
    );
    let receipt = stdout_line(&verified, "VERIFIED");
    assert_eq!(receipt.len(), 32, "receipt id must be 16-byte hex");

    // Same package, same store: durable replay without re-verification.
    let replayed = verify(&scratch, &package);
    assert_eq!(replayed.status.code(), Some(0));
    assert_eq!(stdout_line(&replayed, "REPLAYED"), receipt);
}

#[test]
fn build_is_byte_reproducible_for_identical_trees() {
    let scratch = ScratchDir::new("reproducible");
    let key = keygen(&scratch, &"22".repeat(32));
    let tree = scratch.path("tree");
    write_fixture_tree(&tree, "2.0.0", false);
    let first = scratch.path("a.nlospkg");
    let second = scratch.path("b.nlospkg");

    let build_one = build(&tree, &key, &first);
    let build_two = build(&tree, &key, &second);
    assert_eq!(build_one.status.code(), Some(0));
    assert_eq!(build_two.status.code(), Some(0));
    assert_eq!(
        fs::read(&first).expect("read first"),
        fs::read(&second).expect("read second"),
        "identical trees and keys must build byte-identical packages"
    );
    assert_eq!(
        stdout_line(&build_one, "manifest_digest"),
        stdout_line(&build_two, "manifest_digest")
    );
}

#[test]
fn tampered_payload_fails_verification_with_binding_exit_code() {
    let scratch = ScratchDir::new("tamper-payload");
    let key = keygen(&scratch, &"33".repeat(32));
    let tree = scratch.path("tree");
    write_fixture_tree(&tree, "1.0.0", false);
    let package = scratch.path("sample.nlospkg");
    assert_eq!(build(&tree, &key, &package).status.code(), Some(0));

    // Flip exactly one payload byte inside the built package file.
    let mut bytes = fs::read(&package).expect("read package");
    let positions: Vec<usize> = bytes
        .windows(PAYLOAD_NEEDLE.len())
        .enumerate()
        .filter(|(_, window)| *window == PAYLOAD_NEEDLE)
        .map(|(index, _)| index)
        .collect();
    assert_eq!(positions.len(), 1, "needle must occur exactly once");
    bytes[positions[0]] ^= 0xff;
    let tampered = scratch.path("tampered.nlospkg");
    fs::write(&tampered, bytes).expect("write tampered");

    let output = verify(&scratch, &tampered);
    assert_eq!(
        output.status.code(),
        Some(EXIT_BINDING),
        "payload tamper must be a typed binding failure:\n{}",
        stderr_text(&output)
    );
}

#[test]
fn tampered_manifest_fails_signature_verification() {
    let scratch = ScratchDir::new("tamper-manifest");
    let key = keygen(&scratch, &"44".repeat(32));
    let tree = scratch.path("tree");
    // A version whose big-endian bytes form a unique needle in the file.
    let version = 0xAABB_CCDD_0011_2233_u64;
    write_fixture_tree(&tree, &version.to_string(), false);
    let package = scratch.path("sample.nlospkg");
    assert_eq!(build(&tree, &key, &package).status.code(), Some(0));

    let mut bytes = fs::read(&package).expect("read package");
    let needle = version.to_be_bytes();
    let positions: Vec<usize> = bytes
        .windows(needle.len())
        .enumerate()
        .filter(|(_, window)| *window == needle)
        .map(|(index, _)| index)
        .collect();
    assert_eq!(positions.len(), 1, "version needle must occur exactly once");
    bytes[positions[0] + 7] ^= 0x01;
    let tampered = scratch.path("tampered.nlospkg");
    fs::write(&tampered, bytes).expect("write tampered");

    let output = verify(&scratch, &tampered);
    assert_eq!(
        output.status.code(),
        Some(EXIT_SIGNATURE),
        "manifest tamper must be a typed signature failure:\n{}",
        stderr_text(&output)
    );
}

#[test]
fn manifest_with_tasks_builds_and_verifies_end_to_end() {
    let scratch = ScratchDir::new("tasks-face");
    let key = keygen(&scratch, &"55".repeat(32));
    let tree = scratch.path("tree");
    write_fixture_tree(&tree, "1.4.2", true);
    let package = scratch.path("sample.nlospkg");

    let built = build(&tree, &key, &package);
    assert_eq!(
        built.status.code(),
        Some(0),
        "templated build failed:\n{}",
        stderr_text(&built)
    );
    assert_eq!(stdout_line(&built, "entries"), "2 tasks 2");

    // The templated digest lives in the W28-B domain, distinct from any
    // legacy digest of the same base manifest.
    let digest = stdout_line(&built, "manifest_digest");
    assert_eq!(digest.len(), 64);

    let verified = verify(&scratch, &package);
    assert_eq!(
        verified.status.code(),
        Some(0),
        "templated verify failed:\n{}",
        stderr_text(&verified)
    );
    assert_eq!(stdout_line(&verified, "manifest_digest"), digest);
    assert_eq!(stdout_line(&verified, "VERIFIED").len(), 32);
}

#[test]
fn malformed_inputs_fail_typed_at_build_and_verify() {
    let scratch = ScratchDir::new("malformed");
    let key = keygen(&scratch, &"66".repeat(32));

    // Missing developer manifest.
    let empty_tree = scratch.path("empty-tree");
    fs::create_dir_all(&empty_tree).expect("tree dir");
    let output = build(&empty_tree, &key, &scratch.path("out.nlospkg"));
    assert_eq!(output.status.code(), Some(EXIT_INPUT));

    // Duplicate entry names.
    let dup_tree = scratch.path("dup-tree");
    write_fixture_tree(&dup_tree, "1.0.0", false);
    let mut dup_manifest = developer_manifest("1.0.0", false);
    dup_manifest.push_str("entry = hello data hello.bin\n");
    fs::write(dup_tree.join("package.manifest"), dup_manifest).unwrap();
    let output = build(&dup_tree, &key, &scratch.path("dup.nlospkg"));
    assert_eq!(output.status.code(), Some(EXIT_INPUT));

    // Task segment referencing an undeclared dependency key.
    let dangling_tree = scratch.path("dangling-tree");
    write_fixture_tree(&dangling_tree, "1.0.0", true);
    let mut dangling = developer_manifest("1.0.0", true);
    dangling.push_str(
        "task = f1f2f3f4f5f6f7f8f9f0f1f2f3f4f5f6 agent-role \
                       tasks/binding.txt tasks/inputs.txt tasks/outputs.txt \
                       tasks/policy.txt tasks/ceiling.txt \
                       deps=99887766554433221100ffeeddccbbaa\n",
    );
    fs::write(dangling_tree.join("package.manifest"), dangling).unwrap();
    let output = build(&dangling_tree, &key, &scratch.path("dangling.nlospkg"));
    assert_eq!(output.status.code(), Some(EXIT_INPUT));

    // Truncated package file fails parse at verify.
    let tree = scratch.path("tree");
    write_fixture_tree(&tree, "1.0.0", false);
    let package = scratch.path("sample.nlospkg");
    assert_eq!(build(&tree, &key, &package).status.code(), Some(0));
    let full = fs::read(&package).expect("read package");
    fs::write(&package, &full[..full.len() - 5]).expect("truncate");
    let output = verify(&scratch, &package);
    assert_eq!(output.status.code(), Some(EXIT_INPUT));
}

#[test]
fn usage_errors_exit_one() {
    let output = run_cli(&["keygen"]);
    assert_eq!(output.status.code(), Some(EXIT_USAGE));
    let output = run_cli(&["build"]);
    assert_eq!(output.status.code(), Some(EXIT_USAGE));
    let output = run_cli(&["verify"]);
    assert_eq!(output.status.code(), Some(EXIT_USAGE));
    let output = run_cli(&["nonsense"]);
    assert_eq!(output.status.code(), Some(EXIT_USAGE));
}
