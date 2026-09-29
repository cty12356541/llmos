//! W33-C conformance kit: `nlos-package conformance <PKGFILE>` against
//! the real binary (`CARGO_BIN_EXE`), mirroring `package_sdk_cli.rs`.
//!
//! Gates (进度单 §6.5.3 W33-C): 包结构/manifest/验签一致性检查器 —
//! valid packages (built by the W33-A CLI where possible) pass clean,
//! and every rule has a fixture whose violation yields exactly that
//! typed rule id.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicU64, Ordering};

use ed25519_dalek::{Signer, SigningKey};
use nlos_artifact::{
    PackageEntryRole, PackageFile, PackageFileEntry, PackageTaskKind, PackageTaskTemplate,
    SignerDescriptor, derive_artifact_id, package_manifest_message,
    package_manifest_with_tasks_message,
};
use nlos_types::PackageId;
use sha2::{Digest, Sha256};

const EXIT_USAGE: i32 = 1;
const EXIT_CONFORMANT: i32 = 0;
const EXIT_NONCONFORMANT: i32 = 6;

const PACKAGE_ID_BYTES: [u8; 16] = [
    0x0f, 0x1e, 0x2d, 0x3c, 0x4b, 0x5a, 0x69, 0x78, 0x87, 0x96, 0xa5, 0xb4, 0xc3, 0xd2, 0xe1, 0xf0,
];
const VERSION: u64 = 7 << 32 | 3 << 16 | 1;
const PAYLOAD_NEEDLE: &[u8] = b"CONF-PAYLOAD-NEEDLE-a1b2c3d4";

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct ScratchDir {
    root: PathBuf,
}

impl ScratchDir {
    fn new(name: &str) -> Self {
        let sequence = NEXT_DIR.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "nlos-package-conformance-{name}-{}-{sequence}",
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

fn stdout_text(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("utf-8 stdout")
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("utf-8 stderr")
}

/// Every `PKG-CONF-###` token in the report, in printed order.
fn rule_ids(output: &Output) -> Vec<String> {
    stdout_text(output)
        .split_whitespace()
        .filter(|token| token.starts_with("PKG-CONF-"))
        .map(str::to_string)
        .collect()
}

fn needle_position(bytes: &[u8], needle: &[u8]) -> usize {
    let positions: Vec<usize> = bytes
        .windows(needle.len())
        .enumerate()
        .filter(|(_, window)| *window == needle)
        .map(|(index, _)| index)
        .collect();
    assert_eq!(positions.len(), 1, "needle must occur exactly once");
    positions[0]
}

// ---------------------------------------------------------------------------
// package crafting (W33-A CLI where possible, hand-crafted violations)
// ---------------------------------------------------------------------------

fn write_cli_tree(dir: &Path, version: &str, with_tasks: bool) {
    fs::create_dir_all(dir.join("assets")).expect("assets dir");
    let mut text = format!(
        "package-id = {}\nversion = {version}\n\
         entry = hello executable hello.bin\n\
         entry = config data assets/config.bin\n",
        hex(&PACKAGE_ID_BYTES),
    );
    if with_tasks {
        text.push_str(
            "task = 0102030405060708090a0b0c0d0e0f10 agent-role \
             tasks/binding.txt tasks/inputs.txt tasks/outputs.txt \
             tasks/policy.txt tasks/ceiling.txt\n",
        );
        fs::create_dir_all(dir.join("tasks")).expect("tasks dir");
        fs::write(dir.join("tasks/binding.txt"), b"binding-body").expect("binding");
        fs::write(dir.join("tasks/inputs.txt"), b"inputs-body").expect("inputs");
        fs::write(dir.join("tasks/outputs.txt"), b"outputs-body").expect("outputs");
        fs::write(dir.join("tasks/policy.txt"), b"policy-body").expect("policy");
        fs::write(dir.join("tasks/ceiling.txt"), b"ceiling-body").expect("ceiling");
    }
    fs::write(dir.join("package.manifest"), text).expect("write manifest");
    let mut payload = PAYLOAD_NEEDLE.to_vec();
    payload.extend_from_slice(&[0xa5; 89]);
    fs::write(dir.join("hello.bin"), payload).expect("write hello.bin");
    fs::write(dir.join("assets/config.bin"), b"config-bytes-42").expect("write config.bin");
}

fn build_with_cli(scratch: &ScratchDir, name: &str, version: &str, with_tasks: bool) -> PathBuf {
    let key = scratch.path("sample.devkey");
    let keygen = run_cli(&[
        "keygen",
        "--seed",
        &"77".repeat(32),
        "--out",
        &key.to_string_lossy(),
    ]);
    assert_eq!(
        keygen.status.code(),
        Some(0),
        "keygen: {}",
        stderr_text(&keygen)
    );
    let tree = scratch.path(&format!("{name}-tree"));
    write_cli_tree(&tree, version, with_tasks);
    let package = scratch.path(&format!("{name}.nlospkg"));
    let built = run_cli(&[
        "build",
        &tree.to_string_lossy(),
        "--key",
        &key.to_string_lossy(),
        "--out",
        &package.to_string_lossy(),
    ]);
    assert_eq!(
        built.status.code(),
        Some(0),
        "build: {}",
        stderr_text(&built)
    );
    package
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(text, "{byte:02x}");
    }
    text
}

fn descriptor_of(key: &SigningKey) -> SignerDescriptor {
    SignerDescriptor {
        public_key: key.verifying_key().to_bytes(),
        profile_digest: [0x51; 32],
        policy_digest: [0x52; 32],
        bootstrap_key: [0x53; 16],
        valid_from_ms: 0,
        valid_until_ms: u64::try_from(i64::MAX).expect("i64::MAX fits u64"),
    }
}

fn good_entry(name: &str, body: &[u8]) -> PackageFileEntry {
    let mut digest = [0_u8; 32];
    let mut hasher = Sha256::new();
    hasher.update(body);
    digest.copy_from_slice(&hasher.finalize());
    PackageFileEntry {
        name: name.to_string(),
        role: PackageEntryRole::Data,
        artifact_id: derive_artifact_id(PackageId::from_bytes(PACKAGE_ID_BYTES), VERSION, name),
        digest,
        payload: body.to_vec(),
    }
}

fn template(node_key: [u8; 16], dependency_keys: Vec<[u8; 16]>) -> PackageTaskTemplate {
    PackageTaskTemplate {
        node_key,
        kind: PackageTaskKind::AgentRole,
        binding_digest: [0x61; 32],
        dependency_keys,
        input_selectors_digest: [0x62; 32],
        output_contract_digest: [0x63; 32],
        policy_digest: [0x64; 32],
        resource_ceiling_digest: [0x65; 32],
    }
}

/// Encodes a properly signed package (signature over the correct
/// domain-separated manifest digest for the declared face), so a
/// violation fixture fires exactly its target rule.
fn signed_bytes(
    key: &SigningKey,
    version: u64,
    entries: Vec<PackageFileEntry>,
    tasks: Vec<PackageTaskTemplate>,
) -> Vec<u8> {
    let package_id = PackageId::from_bytes(PACKAGE_ID_BYTES);
    let manifest = nlos_artifact::PackageManifest {
        package_id,
        version,
        entries: entries
            .iter()
            .map(PackageFileEntry::manifest_entry)
            .collect(),
    };
    let message = if tasks.is_empty() {
        package_manifest_message(&manifest)
    } else {
        package_manifest_with_tasks_message(&manifest, &tasks)
    };
    PackageFile {
        descriptor: descriptor_of(key),
        package_id,
        version,
        entries,
        tasks,
        signature: key.sign(&message).to_bytes(),
    }
    .encode()
}

/// A clean baseline package body: one entry, one self-free template.
fn baseline_parts() -> (Vec<PackageFileEntry>, Vec<PackageTaskTemplate>) {
    (
        vec![
            good_entry("entry-alpha", b"alpha-payload"),
            good_entry("entry-beta", PAYLOAD_NEEDLE),
        ],
        vec![
            template([0x71; 16], Vec::new()),
            template([0x72; 16], vec![[0x71; 16]]),
        ],
    )
}

fn write_fixture(scratch: &ScratchDir, name: &str, bytes: &[u8]) -> PathBuf {
    let path = scratch.path(&format!("{name}.nlospkg"));
    fs::write(&path, bytes).expect("write fixture");
    path
}

fn conformance(scratch: &ScratchDir, package: &Path) -> Output {
    let _ = scratch;
    run_cli(&["conformance", &package.to_string_lossy()])
}

/// Runs conformance on the fixture and asserts the findings are exactly
/// `expected` (order-insensitive, multiplicity preserved).
fn assert_findings(scratch: &ScratchDir, name: &str, bytes: &[u8], expected: &[&str]) {
    let path = write_fixture(scratch, name, bytes);
    let output = conformance(scratch, &path);
    assert_eq!(
        output.status.code(),
        Some(EXIT_NONCONFORMANT),
        "expected NONCONFORMANT for {name}:\nstdout:\n{}\nstderr:\n{}",
        stdout_text(&output),
        stderr_text(&output)
    );
    let mut found = rule_ids(&output);
    let mut want: Vec<String> = expected.iter().map(ToString::to_string).collect();
    found.sort();
    want.sort();
    assert_eq!(found, want, "rule ids for {name}");
    let summary = stdout_text(&output);
    assert!(
        summary.contains(&format!("NONCONFORMANT {}", path.display()))
            || summary.contains("NONCONFORMANT"),
        "summary line missing:\n{summary}"
    );
}

// ---------------------------------------------------------------------------
// positive gates
// ---------------------------------------------------------------------------

#[test]
fn cli_built_packages_pass_conformance_clean() {
    let scratch = ScratchDir::new("clean");
    for (name, version, with_tasks) in [("legacy", "7.3.1", false), ("templated", "1.4.2", true)] {
        let package = build_with_cli(&scratch, name, version, with_tasks);
        let output = conformance(&scratch, &package);
        assert_eq!(
            output.status.code(),
            Some(EXIT_CONFORMANT),
            "{name} face must be conformant:\n{}",
            stderr_text(&output)
        );
        let stdout = stdout_text(&output);
        assert!(stdout.contains("CONFORMANT"), "summary missing:\n{stdout}");
        assert!(
            !stdout.contains("PKG-CONF-"),
            "no findings expected:\n{stdout}"
        );
    }
}

#[test]
fn hand_crafted_clean_package_passes_too() {
    // A conforming package NOT built by our CLI (library-crafted) must
    // also pass — the kit checks the format, not the producer.
    let scratch = ScratchDir::new("clean-hand");
    let key = SigningKey::from_bytes(&[0x31; 32]);
    let (entries, tasks) = baseline_parts();
    let bytes = signed_bytes(&key, VERSION, entries, tasks);
    let path = write_fixture(&scratch, "hand-clean", &bytes);
    let output = conformance(&scratch, &path);
    assert_eq!(
        output.status.code(),
        Some(EXIT_CONFORMANT),
        "{}",
        stdout_text(&output)
    );
}

#[test]
fn usage_error_exits_one() {
    let output = run_cli(&["conformance"]);
    assert_eq!(output.status.code(), Some(EXIT_USAGE));
}

// ---------------------------------------------------------------------------
// structure rules (PKG-CONF-001..004)
// ---------------------------------------------------------------------------

#[test]
fn rule_001_bad_magic() {
    let scratch = ScratchDir::new("r001");
    let key = SigningKey::from_bytes(&[0x31; 32]);
    let (entries, tasks) = baseline_parts();
    let mut bytes = signed_bytes(&key, VERSION, entries, tasks);
    bytes[0] ^= 0xff;
    assert_findings(&scratch, "r001", &bytes, &["PKG-CONF-001"]);
}

#[test]
fn rule_002_framing_truncation_and_trailing_bytes() {
    let scratch = ScratchDir::new("r002");
    let key = SigningKey::from_bytes(&[0x31; 32]);
    let (entries, tasks) = baseline_parts();
    let bytes = signed_bytes(&key, VERSION, entries, tasks);

    let truncated = &bytes[..bytes.len() - 1];
    assert_findings(&scratch, "r002-trunc", truncated, &["PKG-CONF-002"]);

    let mut trailing = bytes.clone();
    trailing.push(0);
    assert_findings(&scratch, "r002-trail", &trailing, &["PKG-CONF-002"]);
}

#[test]
fn rule_003_entry_name_shape() {
    let scratch = ScratchDir::new("r003");
    let key = SigningKey::from_bytes(&[0x31; 32]);

    // Non-UTF-8 name byte: surgery over the (unique) name needle.
    let (entries, tasks) = baseline_parts();
    let mut bytes = signed_bytes(&key, VERSION, entries, tasks);
    let position = needle_position(&bytes, b"entry-alpha");
    bytes[position] = 0xff;
    assert_findings(&scratch, "r003-utf8", &bytes, &["PKG-CONF-003"]);

    // NUL-bearing name and oversized name are encodable via the codec.
    let mut nul_entry = good_entry("entry-alpha", b"alpha-payload");
    nul_entry.name = "entry\0alpha".to_string();
    let bytes = signed_bytes(
        &key,
        VERSION,
        vec![nul_entry, good_entry("entry-beta", PAYLOAD_NEEDLE)],
        Vec::new(),
    );
    assert_findings(&scratch, "r003-nul", &bytes, &["PKG-CONF-003"]);

    let mut oversized = good_entry("entry-alpha", b"alpha-payload");
    oversized.name = "x".repeat(256);
    let bytes = signed_bytes(&key, VERSION, vec![oversized], Vec::new());
    assert_findings(&scratch, "r003-len", &bytes, &["PKG-CONF-003"]);
}

#[test]
fn rule_004_unknown_enum_bytes() {
    let scratch = ScratchDir::new("r004");
    let key = SigningKey::from_bytes(&[0x31; 32]);

    // Unknown entry role byte: the role byte directly follows the name.
    let (entries, tasks) = baseline_parts();
    let mut bytes = signed_bytes(&key, VERSION, entries, tasks);
    let position = needle_position(&bytes, b"entry-alpha") + "entry-alpha".len();
    bytes[position] = 7;
    assert_findings(&scratch, "r004-role", &bytes, &["PKG-CONF-004"]);

    // Unknown task kind byte: the kind byte directly follows the
    // (unique) node key — a single dep-free template whose key nothing
    // else repeats.
    let (entries, _tasks) = baseline_parts();
    let mut bytes = signed_bytes(
        &key,
        VERSION,
        entries,
        vec![template([0xa7; 16], Vec::new())],
    );
    let node_key: &[u8] = &[0xa7; 16];
    let position = needle_position(&bytes, node_key) + node_key.len();
    bytes[position] = 9;
    assert_findings(&scratch, "r004-kind", &bytes, &["PKG-CONF-004"]);
}

// ---------------------------------------------------------------------------
// manifest schema completeness (PKG-CONF-010/011) and tasks shapes
// (PKG-CONF-012..016)
// ---------------------------------------------------------------------------

#[test]
fn rule_010_empty_entry_set() {
    let scratch = ScratchDir::new("r010");
    let key = SigningKey::from_bytes(&[0x31; 32]);
    let bytes = signed_bytes(&key, VERSION, Vec::new(), Vec::new());
    assert_findings(&scratch, "r010", &bytes, &["PKG-CONF-010"]);
}

#[test]
fn rule_011_duplicate_entry_names() {
    let scratch = ScratchDir::new("r011");
    let key = SigningKey::from_bytes(&[0x31; 32]);
    // Same name ⇒ same derived artifact id; only the duplicate-name rule
    // fires (both payload bindings stay self-consistent).
    let bytes = signed_bytes(
        &key,
        VERSION,
        vec![
            good_entry("twin", b"payload-one"),
            good_entry("twin", b"payload-two"),
        ],
        Vec::new(),
    );
    assert_findings(&scratch, "r011", &bytes, &["PKG-CONF-011"]);
}

#[test]
fn rule_012_duplicate_node_keys() {
    let scratch = ScratchDir::new("r012");
    let key = SigningKey::from_bytes(&[0x31; 32]);
    let (entries, _tasks) = baseline_parts();
    let tasks = vec![
        template([0x71; 16], Vec::new()),
        template([0x71; 16], Vec::new()),
    ];
    let bytes = signed_bytes(&key, VERSION, entries, tasks);
    assert_findings(&scratch, "r012", &bytes, &["PKG-CONF-012"]);
}

#[test]
fn rule_013_self_dependency() {
    let scratch = ScratchDir::new("r013");
    let key = SigningKey::from_bytes(&[0x31; 32]);
    let (entries, _tasks) = baseline_parts();
    let tasks = vec![template([0x71; 16], vec![[0x71; 16]])];
    let bytes = signed_bytes(&key, VERSION, entries, tasks);
    assert_findings(&scratch, "r013", &bytes, &["PKG-CONF-013"]);
}

#[test]
fn rule_014_dangling_dependency() {
    let scratch = ScratchDir::new("r014");
    let key = SigningKey::from_bytes(&[0x31; 32]);
    let (entries, _tasks) = baseline_parts();
    let tasks = vec![template([0x71; 16], vec![[0x9f; 16]])];
    let bytes = signed_bytes(&key, VERSION, entries, tasks);
    assert_findings(&scratch, "r014", &bytes, &["PKG-CONF-014"]);
}

#[test]
fn rule_016_dependency_admission_bound() {
    // One template depending on 257 declared siblings: every reference
    // resolves, no duplicates, but the per-template bound is exceeded.
    let scratch = ScratchDir::new("r016");
    let key = SigningKey::from_bytes(&[0x31; 32]);
    let (entries, _tasks) = baseline_parts();
    let mut tasks: Vec<PackageTaskTemplate> = (0..258_u16)
        .map(|index| {
            let mut node_key = [0_u8; 16];
            node_key[..2].copy_from_slice(&index.to_be_bytes());
            template(node_key, Vec::new())
        })
        .collect();
    let mut head = tasks[0].clone();
    head.dependency_keys = tasks[1..].iter().map(|task| task.node_key).collect();
    tasks[0] = head;
    let bytes = signed_bytes(&key, VERSION, entries, tasks);
    assert_findings(&scratch, "r016", &bytes, &["PKG-CONF-016"]);
}

// ---------------------------------------------------------------------------
// signature chain and digest consistency (PKG-CONF-020/030/031)
// ---------------------------------------------------------------------------

#[test]
fn rule_020_signature_mismatch() {
    let scratch = ScratchDir::new("r020");
    let key = SigningKey::from_bytes(&[0x31; 32]);
    let (entries, tasks) = baseline_parts();
    let mut bytes = signed_bytes(&key, VERSION, entries, tasks);
    let last = bytes.len() - 1;
    bytes[last] ^= 0x01;
    assert_findings(&scratch, "r020", &bytes, &["PKG-CONF-020"]);
}

#[test]
fn rule_020_and_031_tampered_version_breaks_signature_and_derivation() {
    // Flipping the signed version field invalidates the signature AND
    // detaches every artifact id from its documented derivation — the
    // multi-finding report shape.
    let scratch = ScratchDir::new("r020-031");
    let key = SigningKey::from_bytes(&[0x31; 32]);
    let (entries, tasks) = baseline_parts();
    let mut bytes = signed_bytes(&key, VERSION, entries, tasks);
    let needle = VERSION.to_be_bytes();
    let position = needle_position(&bytes, &needle);
    bytes[position] ^= 0x01;
    assert_findings(
        &scratch,
        "r020-031",
        &bytes,
        &["PKG-CONF-020", "PKG-CONF-031", "PKG-CONF-031"],
    );
}

#[test]
fn rule_030_payload_digest_mismatch() {
    // Flipping payload bytes leaves the signed manifest digest intact
    // (it covers the declared digest field, not the payload), so only
    // the payload/digest consistency finding fires.
    let scratch = ScratchDir::new("r030");
    let key = SigningKey::from_bytes(&[0x31; 32]);
    let (entries, tasks) = baseline_parts();
    let mut bytes = signed_bytes(&key, VERSION, entries, tasks);
    let position = needle_position(&bytes, PAYLOAD_NEEDLE);
    bytes[position] ^= 0xff;
    assert_findings(&scratch, "r030", &bytes, &["PKG-CONF-030"]);
}

#[test]
fn rule_031_artifact_id_off_derivation() {
    let scratch = ScratchDir::new("r031");
    let key = SigningKey::from_bytes(&[0x31; 32]);
    let mut entry = good_entry("entry-alpha", b"alpha-payload");
    entry.artifact_id = [0xee; 16]; // properly signed, off the derivation
    let bytes = signed_bytes(
        &key,
        VERSION,
        vec![entry, good_entry("entry-beta", PAYLOAD_NEEDLE)],
        Vec::new(),
    );
    assert_findings(&scratch, "r031", &bytes, &["PKG-CONF-031"]);
}

// ---------------------------------------------------------------------------
// compatibility-window / metadata sanity (PKG-CONF-040..042)
// ---------------------------------------------------------------------------

#[test]
fn rule_040_empty_key_validity_window() {
    let scratch = ScratchDir::new("r040");
    let key = SigningKey::from_bytes(&[0x31; 32]);
    let (entries, tasks) = baseline_parts();
    let package_id = PackageId::from_bytes(PACKAGE_ID_BYTES);
    let manifest = nlos_artifact::PackageManifest {
        package_id,
        version: VERSION,
        entries: entries
            .iter()
            .map(PackageFileEntry::manifest_entry)
            .collect(),
    };
    let mut descriptor = descriptor_of(&key);
    descriptor.valid_from_ms = 9;
    descriptor.valid_until_ms = 1;
    let message = package_manifest_with_tasks_message(&manifest, &tasks);
    let bytes = PackageFile {
        descriptor,
        package_id,
        version: VERSION,
        entries,
        tasks,
        signature: key.sign(&message).to_bytes(),
    }
    .encode();
    assert_findings(&scratch, "r040", &bytes, &["PKG-CONF-040"]);
}

#[test]
fn rule_041_window_beyond_durable_ledger_bounds() {
    // The descriptor is unsigned, so patching the valid-until field to
    // u64::MAX breaks only the ledger-bounds rule.
    let scratch = ScratchDir::new("r041");
    let key = SigningKey::from_bytes(&[0x31; 32]);
    let (entries, tasks) = baseline_parts();
    let mut bytes = signed_bytes(&key, VERSION, entries, tasks);
    let until = u64::MAX.to_be_bytes();
    let offset = b"nlos/package-file/v1".len() + 32 + 32 + 32 + 16 + 8;
    bytes[offset..offset + 8].copy_from_slice(&until);
    assert_findings(&scratch, "r041", &bytes, &["PKG-CONF-041"]);
}

#[test]
fn rule_042_zero_version_metadata() {
    let scratch = ScratchDir::new("r042");
    let key = SigningKey::from_bytes(&[0x31; 32]);
    // Derive the artifact id with the same zero version so only the
    // version-metadata finding fires.
    let mut entry = good_entry("entry-alpha", b"alpha-payload");
    entry.artifact_id =
        derive_artifact_id(PackageId::from_bytes(PACKAGE_ID_BYTES), 0, "entry-alpha");
    let bytes = signed_bytes(&key, 0, vec![entry], Vec::new());
    assert_findings(&scratch, "r042", &bytes, &["PKG-CONF-042"]);
}
