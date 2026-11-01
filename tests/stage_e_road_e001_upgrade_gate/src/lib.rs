//! Stage E `E7` (`W81-E7A`, `[ROAD-E-001]`) — Host `N-1 -> N` Zero-Recompile
//! Cross-Version Upgrade Full-Chain Gate and `H7` Adversarial Compatibility Suite.
//!
//! This detached standalone Cargo workspace implements the four-step mechanical
//! proof mandated by `ADR-0027` section 2.4 (`[ROAD-E-001]`):
//! - **Step A ([`step_a_frozen_seal`])**: Verifies all 16 frozen `N-1` baseline
//!   files (`8` `.nlos-pkg` packages, `4` `.certify.json` reports, `4`
//!   `.store-seal.txt` governance receipts) across the 2 third-party
//!   multi-component Applications (`knowledge-observatory-app`,
//!   `field-survey-app`) and 2 third-party userland Drivers (`mcp-bridge-driver`,
//!   `openai-compat-driver`), replaying 3-tier PKI publisher trust, 4-gate
//!   certification receipts (`0b1111`), and `LocalOciCasStore` +
//!   `SqliteStoreRegistry` `Candidate -> Stable` promotion without touching or
//!   rebuilding any frozen `.nlos-pkg` byte.
//! - **Step B ([`step_b_n1_host`])**: Boots a durable `N-1` Host (`host_epoch = 1`),
//!   installs all 4 frozen `v1.0.0.nlos-pkg` packages directly from their sealed
//!   bytes, pins immutable `ecosystem_selector` handles (rejecting floating
//!   `"latest"`/`"*"` pins and `0x1F` non-authoritative search hits), grants
//!   `Z4`-consented attenuated `Capability` tokens, executes real multi-node
//!   `TaskPlan` DAG steps, `McpBridgeAdapter` JSON-RPC 2.0 tool calls,
//!   `MeteredChatGateway` 5-dimension `LlmTokenBudget` charges, content-addressed
//!   `ArtifactStore` lineage writes, durable background `Task` registration, and
//!   `UserData` container persistence, then cleanly drops all `N-1` handles.
//! - **Step C ([`step_c_in_place_upgrade`])**: Simulates an in-place Host `N`
//!   upgrade (`host_epoch = 2`) over the **exact same durable SQLite databases
//!   and `UserData` directories** (zero `.nlos-pkg` modification, recompilation,
//!   or re-signing), proves `[KABI-SVC-001]` insulation across an internal KABI
//!   kernel table/handle refactor, configures the `[2, 1, 0]` compatibility
//!   window (`>= MIN_SABI_COMPATIBILITY_WINDOW_EPOCHS = 2`), evolves the SABI v1
//!   `DeprecationManifest` to `current_epoch = 2`, mounts `nlos-compat-shim`
//!   only for `N-1` (`epoch = 1`) endpoints while keeping `N` (`epoch = 2`) on
//!   0-hop `NativePassthrough`, and verifies `[COMPAT-VER-002]` fail-closed
//!   rejection of unknown `major` / unknown `critical_extension_ids` alongside
//!   lossless round-trip preservation of unknown `non_critical_extension_ids`
//!   and unknown Protobuf wire fields.
//! - **Step D ([`step_d_n_recovery_and_lifecycle`])**: Proves 100% automatic
//!   recovery on Host `N` of `InstallationId`s, `(PublisherId, ApplicationId)`
//!   ownership bindings, `Capability` grants, `ArtifactStore` causal lineage,
//!   5-dimension `LlmTokenBudget` counters, durable `Task` / `TaskPlan` state,
//!   and `UserData` SHA-256 tree digests; verifies `ServiceDirectory`
//!   compatibility resolution and bounded `DeprecationWarningReceipt` emission;
//!   routes `N-1` traffic through `OutOfCoreCompatShimRuntime` +
//!   `TrustedEnforcementGate` with `0`-byte `SecretCredential` wire leakage;
//!   exercises userland driver hot-plug rotation and mid-flight crash ->
//!   `UNCERTAIN` / `EffectUnknown` + `Quarantined` -> same-`IdempotencyKey`
//!   reconciliation; runs the dual-layer `nlos-wasm-sandbox` subprocess fences;
//!   performs `v1.0.0 -> v1.1.0` permission-escalation blocking, injected-fault
//!   atomic rollback, and healthy `v1.0.0 -> v1.1.0` upgrade from frozen
//!   `v1.1.0.nlos-pkg` artifacts; enforces active-task uninstall gating,
//!   immediate `Capability` revocation, zero-residue cache cleanup, shared
//!   `Artifact` preservation, and `Z4`-gated `UserData` retention; and finally
//!   re-verifies that all 16 sealed `N-1` baseline files retain identical
//!   post-run SHA-256 digests (`0` bytes modified).

#![forbid(unsafe_code)]

pub mod step_a_frozen_seal;
pub mod step_b_n1_host;
pub mod step_c_in_place_upgrade;
pub mod step_d_n_recovery_and_lifecycle;

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::step_a_frozen_seal::{StepAFrozenSealReceipt, run_step_a_verify_frozen_n1_baseline};
use crate::step_b_n1_host::{StepBN1HostStateSnapshot, run_step_b_install_and_run_on_n1_host};
use crate::step_c_in_place_upgrade::{
    StepCInPlaceUpgradeReceipt, run_step_c_upgrade_host_to_epoch_n,
};
use crate::step_d_n_recovery_and_lifecycle::{
    StepDHostNRecoveryReceipt, run_step_d_recover_and_exercise_on_host_n,
};

/// Host `N-1` SABI major epoch (`1`).
pub const HOST_EPOCH_N_MINUS_1: u32 = 1;

/// Upgraded Host `N` SABI major epoch (`2`).
pub const HOST_EPOCH_N: u32 = 2;

/// Deterministic seal baseline timestamp (`2026-01-01T00:00:00Z`) matching
/// `examples/ecosystem-apps/n1-baseline-sealer`.
pub const SEAL_T0_MS: u64 = 1_767_225_600_000;

/// Offline Platform Root CA deterministic seed (`[0xE6; 32]`).
pub const SEAL_ROOT_CA_SEED: [u8; 32] = [0xE6; 32];

/// Store Intermediate CA deterministic seed (`[0xB0; 32]`).
pub const SEAL_INTERMEDIATE_CA_SEED: [u8; 32] = [0xB0; 32];

/// Store Certifier deterministic seed (`[0xCE; 32]`).
pub const SEAL_CERTIFIER_SEED: [u8; 32] = [0xCE; 32];

/// Store Entitlement Issuer deterministic seed (`[0x65; 32]`).
pub const SEAL_ENTITLEMENT_SEED: [u8; 32] = [0x65; 32];

/// Store Security Recall Signer deterministic seed (`[0x66; 32]`).
pub const SEAL_RECALL_SEED: [u8; 32] = [0x66; 32];

/// Classification of one frozen `N-1` ecosystem sample.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SampleKind {
    /// Multi-component third-party productivity Application (`Executable` +
    /// `BackgroundService` + `Data` + declarative `TaskPlan` templates).
    MultiComponentApplication,
    /// User-mode third-party Driver (`Executable` + `BackgroundService` +
    /// `Data`, `0` kernel crates).
    UserlandDriver,
}

/// Static specification of one frozen `N-1` sample (`W80-E6B` sealed baseline).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FrozenSampleSpec {
    /// Sample ordinal (`0..=3`) used in deterministic leaf certificate serials.
    pub index: u8,
    /// Human-readable sample label.
    pub label: &'static str,
    /// Sample category (`Application` vs `Driver`).
    pub kind: SampleKind,
    /// Symbolic publisher handle registered in `nlos-publisher-trust`.
    pub publisher_label: &'static str,
    /// Deterministic publisher Ed25519 seed (`0x61..=0x64`).
    pub publisher_seed_byte: u8,
    /// Expected 32-hex `PackageId`.
    pub expected_package_id_hex: &'static str,
    /// Workspace-relative directory containing `package/`, `package-v1.1.0/`,
    /// and `artifacts/n1-sealed/`.
    pub sample_rel_dir: &'static str,
    /// Workspace-relative path to frozen `v1.0.0.nlos-pkg`.
    pub pkg_v1_0_0_rel_path: &'static str,
    /// Frozen SHA-256 hex digest of `v1.0.0.nlos-pkg`.
    pub expected_v1_0_0_sha256: &'static str,
    /// Expected byte length of `v1.0.0.nlos-pkg`.
    pub expected_v1_0_0_bytes: usize,
    /// Workspace-relative path to frozen `v1.1.0.nlos-pkg`.
    pub pkg_v1_1_0_rel_path: &'static str,
    /// Frozen SHA-256 hex digest of `v1.1.0.nlos-pkg`.
    pub expected_v1_1_0_sha256: &'static str,
    /// Expected byte length of `v1.1.0.nlos-pkg`.
    pub expected_v1_1_0_bytes: usize,
    /// Workspace-relative path to frozen `.certify.json`.
    pub certify_json_rel_path: &'static str,
    /// Workspace-relative path to frozen `.store-seal.txt`.
    pub store_seal_rel_path: &'static str,
    /// Primary executable entry name inside `package.manifest`.
    pub executable_entry: &'static str,
    /// Optional background-service entry name inside `package.manifest`.
    pub service_entry: Option<&'static str>,
    /// Primary read-only seed data entry name inside `package.manifest`.
    pub seed_entry: &'static str,
}

/// Canonical 4-sample (`16`-file) frozen `N-1` baseline sealed in `W80-E6B`.
pub const FROZEN_N1_SAMPLES: [FrozenSampleSpec; 4] = [
    FrozenSampleSpec {
        index: 0,
        label: "knowledge-observatory-app",
        kind: SampleKind::MultiComponentApplication,
        publisher_label: "nlos.market.observatory-works",
        publisher_seed_byte: 0x61,
        expected_package_id_hex: "6b6e6f776c65646f6273763130303031",
        sample_rel_dir: "examples/ecosystem-apps/knowledge-observatory-app",
        pkg_v1_0_0_rel_path: "examples/ecosystem-apps/knowledge-observatory-app/sealed/knowledge-observatory-app.v1.0.0.nlos-pkg",
        expected_v1_0_0_sha256: "690c4e296bf3b0bedd56b17592064c04e3eac8defcf14d4d1e988e0daf4602d5",
        expected_v1_0_0_bytes: 2538,
        pkg_v1_1_0_rel_path: "examples/ecosystem-apps/knowledge-observatory-app/sealed/knowledge-observatory-app.v1.1.0.nlos-pkg",
        expected_v1_1_0_sha256: "0bc978a23cc62571503bdd939aef6be76f692b5f2d61e26bb227cb0398191985",
        expected_v1_1_0_bytes: 2828,
        certify_json_rel_path: "examples/ecosystem-apps/knowledge-observatory-app/sealed/knowledge-observatory-app.v1.0.0.certify.json",
        store_seal_rel_path: "examples/ecosystem-apps/knowledge-observatory-app/sealed/knowledge-observatory-app.v1.0.0.store-seal.txt",
        executable_entry: "curator",
        service_entry: Some("index-sync"),
        seed_entry: "observatory-profile",
    },
    FrozenSampleSpec {
        index: 1,
        label: "field-survey-app",
        kind: SampleKind::MultiComponentApplication,
        publisher_label: "nlos.market.fieldkit-labs",
        publisher_seed_byte: 0x62,
        expected_package_id_hex: "6669656c647375727665797731303030",
        sample_rel_dir: "examples/ecosystem-apps/field-survey-app",
        pkg_v1_0_0_rel_path: "examples/ecosystem-apps/field-survey-app/sealed/field-survey-app.v1.0.0.nlos-pkg",
        expected_v1_0_0_sha256: "4e529d61239bec8bf289993fd7294991207842fc77620ed7cefa0f19f9d1c4eb",
        expected_v1_0_0_bytes: 2341,
        pkg_v1_1_0_rel_path: "examples/ecosystem-apps/field-survey-app/sealed/field-survey-app.v1.1.0.nlos-pkg",
        expected_v1_1_0_sha256: "59f219d130fd1845c9d307e6e2e799ca11dbf8e1fe3d05b5f663e16d292111c2",
        expected_v1_1_0_bytes: 2618,
        certify_json_rel_path: "examples/ecosystem-apps/field-survey-app/sealed/field-survey-app.v1.0.0.certify.json",
        store_seal_rel_path: "examples/ecosystem-apps/field-survey-app/sealed/field-survey-app.v1.0.0.store-seal.txt",
        executable_entry: "survey-worker",
        service_entry: Some("sync-field-notes"),
        seed_entry: "survey-checklist",
    },
    FrozenSampleSpec {
        index: 2,
        label: "mcp-bridge-driver",
        kind: SampleKind::UserlandDriver,
        publisher_label: "nlos.market.mcp-bridge-works",
        publisher_seed_byte: 0x63,
        expected_package_id_hex: "6d637062726964676531313131313131",
        sample_rel_dir: "examples/ecosystem-drivers/mcp-bridge-driver",
        pkg_v1_0_0_rel_path: "examples/ecosystem-drivers/mcp-bridge-driver/sealed/mcp-bridge-driver.v1.0.0.nlos-pkg",
        expected_v1_0_0_sha256: "6bff8ec8aeaff6d478eabbb6dbca97e062660fb711b719ff13ec9c67ec1fe83d",
        expected_v1_0_0_bytes: 2327,
        pkg_v1_1_0_rel_path: "examples/ecosystem-drivers/mcp-bridge-driver/sealed/mcp-bridge-driver.v1.1.0.nlos-pkg",
        expected_v1_1_0_sha256: "9a6ec0ebb328a5064b8781a7996f87cffb64dd35b1e6acf2fab8c52881fa525b",
        expected_v1_1_0_bytes: 2778,
        certify_json_rel_path: "examples/ecosystem-drivers/mcp-bridge-driver/sealed/mcp-bridge-driver.v1.0.0.certify.json",
        store_seal_rel_path: "examples/ecosystem-drivers/mcp-bridge-driver/sealed/mcp-bridge-driver.v1.0.0.store-seal.txt",
        executable_entry: "mcp-bridge-adapter",
        service_entry: None,
        seed_entry: "mcp-extension-manifest",
    },
    FrozenSampleSpec {
        index: 3,
        label: "openai-compat-driver",
        kind: SampleKind::UserlandDriver,
        publisher_label: "nlos.market.openai-gateway-works",
        publisher_seed_byte: 0x64,
        expected_package_id_hex: "6f70656e6169636f6d70617431303031",
        sample_rel_dir: "examples/ecosystem-drivers/openai-compat-driver",
        pkg_v1_0_0_rel_path: "examples/ecosystem-drivers/openai-compat-driver/sealed/openai-compat-driver.v1.0.0.nlos-pkg",
        expected_v1_0_0_sha256: "db0d93d156dd07f6efc937b891dfc97bf9b363aeb0c28081382d22e1e68be0e1",
        expected_v1_0_0_bytes: 2080,
        pkg_v1_1_0_rel_path: "examples/ecosystem-drivers/openai-compat-driver/sealed/openai-compat-driver.v1.1.0.nlos-pkg",
        expected_v1_1_0_sha256: "0467b105904b8b013524c880eef32d33d1fc7fe3f13a661dd6fadfb41940de09",
        expected_v1_1_0_bytes: 2413,
        certify_json_rel_path: "examples/ecosystem-drivers/openai-compat-driver/sealed/openai-compat-driver.v1.0.0.certify.json",
        store_seal_rel_path: "examples/ecosystem-drivers/openai-compat-driver/sealed/openai-compat-driver.v1.0.0.store-seal.txt",
        executable_entry: "openai-compat-adapter",
        service_entry: None,
        seed_entry: "openai-extension-manifest",
    },
];

/// Returns the repository root (`../..` relative to `tests/stage_e_road_e001_upgrade_gate`).
///
/// # Panics
///
/// Panics only if `CARGO_MANIFEST_DIR` does not have two parent directories.
#[must_use]
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("tests/stage_e_road_e001_upgrade_gate must sit two levels below repo root")
        .to_path_buf()
}

/// Returns all 16 workspace-relative sealed `N-1` baseline file paths in canonical order.
#[must_use]
pub fn all_sealed_n1_relative_paths() -> Vec<&'static str> {
    let mut paths = Vec::with_capacity(FROZEN_N1_SAMPLES.len() * 4);
    for spec in &FROZEN_N1_SAMPLES {
        paths.push(spec.pkg_v1_0_0_rel_path);
        paths.push(spec.pkg_v1_1_0_rel_path);
        paths.push(spec.certify_json_rel_path);
        paths.push(spec.store_seal_rel_path);
    }
    paths
}

/// Immutable SHA-256 and byte-length snapshot over all 16 sealed `N-1` baseline files.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SealedBaselineSnapshot {
    /// Map from workspace-relative file path to `(sha256_hex, byte_len)`.
    pub files: BTreeMap<String, (String, usize)>,
}

impl SealedBaselineSnapshot {
    /// Captures a live SHA-256 and byte-length snapshot of all 16 sealed `N-1`
    /// baseline files under `root`.
    ///
    /// # Errors
    ///
    /// Returns a descriptive error string if any of the 16 files cannot be read.
    pub fn capture(root: &Path) -> Result<Self, String> {
        let mut files = BTreeMap::new();
        for rel in all_sealed_n1_relative_paths() {
            let full = root.join(rel);
            let bytes = fs::read(&full).map_err(|err| {
                format!("failed to read sealed N-1 file {}: {err}", full.display())
            })?;
            let digest = sha256_hex(&bytes);
            files.insert(rel.to_owned(), (digest, bytes.len()));
        }
        Ok(Self { files })
    }

    /// Asserts that `post` is byte-for-byte identical to `self` across all 16
    /// sealed `N-1` baseline files (`[ROAD-E-001]` zero-recompile / zero-re-sign law).
    ///
    /// # Errors
    ///
    /// Returns an error naming the first drifted or missing file if any digest
    /// or byte count changed.
    pub fn verify_zero_drift(&self, post: &Self) -> Result<(), String> {
        if self.files.len() != post.files.len() {
            return Err(format!(
                "sealed file count drifted: pre={}, post={}",
                self.files.len(),
                post.files.len()
            ));
        }
        for (path, (pre_sha, pre_len)) in &self.files {
            let Some((post_sha, post_len)) = post.files.get(path) else {
                return Err(format!("sealed file missing post-run: {path}"));
            };
            if pre_sha != post_sha || pre_len != post_len {
                return Err(format!(
                    "zero-recompile violation on {path}: pre=({pre_sha}, {pre_len}B) != post=({post_sha}, {post_len}B)"
                ));
            }
        }
        Ok(())
    }
}

/// Computes the lowercase 64-hex SHA-256 digest of `bytes`.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest: [u8; 32] = Sha256::digest(bytes).into();
    hex_encode(&digest)
}

/// Encodes `bytes` as lowercase hex.
#[must_use]
pub fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Decodes an exact fixed-size byte array from a hex string.
///
/// # Errors
///
/// Returns an error if `raw` does not have length `N * 2` or contains non-hex characters.
pub fn hex_decode_exact<const N: usize>(raw: &str, label: &str) -> Result<[u8; N], String> {
    let trimmed = raw.trim();
    if trimmed.len() != N * 2 {
        return Err(format!(
            "{label} expected {} hex chars, got {} ({trimmed:?})",
            N * 2,
            trimmed.len()
        ));
    }
    let mut out = [0_u8; N];
    for (idx, slot) in out.iter_mut().enumerate() {
        let part = &trimmed[idx * 2..idx * 2 + 2];
        *slot = u8::from_str_radix(part, 16)
            .map_err(|err| format!("{label} invalid hex at byte {idx}: {err}"))?;
    }
    Ok(out)
}

/// Derives a deterministic 16-byte key from `(domain, label, ordinal)`.
#[must_use]
pub fn derive_key16(domain: &[u8], label: &[u8], ordinal: u32) -> [u8; 16] {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update([0]);
    hasher.update(label);
    hasher.update(ordinal.to_be_bytes());
    let digest = hasher.finalize();
    let mut out = [0_u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

/// Derives a deterministic 32-byte digest from `(domain, label, ordinal)`.
#[must_use]
pub fn derive_digest32(domain: &[u8], label: &[u8], ordinal: u32) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    hasher.update([0]);
    hasher.update(label);
    hasher.update(ordinal.to_be_bytes());
    hasher.finalize().into()
}

static NEXT_TEMP_WORKSPACE_ID: AtomicU64 = AtomicU64::new(1);

/// Ephemeral host directory holding durable SQLite databases and `UserData`
/// containers across the simulated `N-1 -> N` host OS in-place upgrade.
pub struct TempHostWorkspace {
    root: PathBuf,
}

impl TempHostWorkspace {
    /// Creates a fresh isolated host workspace under `std::env::temp_dir()`.
    ///
    /// # Panics
    ///
    /// Panics if the OS temporary directory cannot be created.
    #[must_use]
    pub fn new(tag: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |dur| dur.as_nanos());
        let seq = NEXT_TEMP_WORKSPACE_ID.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "nlos-w81-road-e001-{tag}-{}-{nanos}-{seq}",
            std::process::id()
        ));
        fs::create_dir_all(&root).expect("create TempHostWorkspace directory");
        Self { root }
    }

    /// Root path of the temporary host workspace.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.root
    }

    /// Durable `SliceKRuntime` + authority state root shared across `N-1` and `N`.
    #[must_use]
    pub fn authority_root(&self) -> PathBuf {
        self.root.join("host-authorities")
    }

    /// Durable `UserData` container directory for `sample_label`, shared across
    /// `N-1` and `N`.
    #[must_use]
    pub fn user_data_dir(&self, sample_label: &str) -> PathBuf {
        self.root.join("user-data").join(sample_label)
    }

    /// Private runtime cache directory for `sample_label` (purged on clean uninstall).
    #[must_use]
    pub fn private_cache_dir(&self, sample_label: &str) -> PathBuf {
        self.root.join("private-cache").join(sample_label)
    }
}

impl Drop for TempHostWorkspace {
    fn drop(&mut self) {
        for attempt in 0..6 {
            match fs::remove_dir_all(&self.root) {
                Err(err) if err.kind() != std::io::ErrorKind::NotFound && attempt < 5 => {
                    std::thread::sleep(std::time::Duration::from_millis(30));
                }
                Ok(()) | Err(_) => return,
            }
        }
    }
}

/// Complete receipt returned by [`run_full_road_e001_upgrade_chain`] covering
/// `ADR-0027` section 2.4 Steps A, B, C, and D.
#[derive(Clone, Debug)]
pub struct RoadE001FullChainReceipt {
    /// Pre-run SHA-256 snapshot of all 16 sealed `N-1` baseline files.
    pub pre_snapshot: SealedBaselineSnapshot,
    /// Step A frozen seal, 3-tier PKI, 4-gate certification, and OCI CAS promotion receipt.
    pub step_a: StepAFrozenSealReceipt,
    /// Step B durable Host `N-1` installation, execution, and persistence snapshot.
    pub step_b: StepBN1HostStateSnapshot,
    /// Step C in-place Host `N` upgrade, KABI refactor insulation, and `COMPAT-VER-002` receipt.
    pub step_c: StepCInPlaceUpgradeReceipt,
    /// Step D Host `N` zero-recompile recovery, `nlos-compat-shim` mediation, driver reconcile,
    /// Wasm sandbox isolation, `v1.0.0 -> v1.1.0` rollback/upgrade, and clean uninstall receipt.
    pub step_d: StepDHostNRecoveryReceipt,
    /// Post-run SHA-256 snapshot of all 16 sealed `N-1` baseline files (`pre == post`).
    pub post_snapshot: SealedBaselineSnapshot,
}

/// Executes the complete `ADR-0027` section 2.4 (`[ROAD-E-001]`) four-step
/// `N-1 -> N` zero-recompile cross-version host upgrade verification chain.
///
/// # Errors
///
/// Fails closed with a descriptive error if any Step A..D assertion or post-run
/// zero-drift SHA-256 verification fails.
pub fn run_full_road_e001_upgrade_chain(
    wasm_host_exe: &Path,
) -> Result<RoadE001FullChainReceipt, String> {
    let root = repo_root();
    let pre_snapshot = SealedBaselineSnapshot::capture(&root)?;

    let host_ws = TempHostWorkspace::new("full-chain");
    let step_a = run_step_a_verify_frozen_n1_baseline(&root, &pre_snapshot)?;
    let step_b = run_step_b_install_and_run_on_n1_host(&root, &host_ws, &step_a)?;
    let step_c = run_step_c_upgrade_host_to_epoch_n(&host_ws, &step_b)?;
    let step_d = run_step_d_recover_and_exercise_on_host_n(
        &root,
        &host_ws,
        &step_b,
        &step_c,
        wasm_host_exe,
    )?;

    let post_snapshot = SealedBaselineSnapshot::capture(&root)?;
    pre_snapshot.verify_zero_drift(&post_snapshot)?;

    Ok(RoadE001FullChainReceipt {
        pre_snapshot,
        step_a,
        step_b,
        step_c,
        step_d,
        post_snapshot,
    })
}
